//! Quiet speaker naming in the conductor (01:35/01:37; voice card v3, 3 Nov 19:22): the voice namer's minute check
//! during calls, its one island card at a time (a new voice: "New voice: “…”", its guessed names, a name field, Not
//! important; a voice that keeps coming back: "Is this John?", Yes, the field, Not important), and the renames that
//! card, the transcript's double-click, the Speakers window and the agent's `name_voice` tool share. A rename relabels
//! every archived line.
use super::*;
use crate::voice_namer as namer;
use crate::voice_suggest as sg;
use anyhow::bail;
use serde_json::json;
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::atomic::AtomicBool;

/// A card on the island: the new-voice card (`voice_suggest::CARD_PREFIX`) or the returning-voice proposal
/// (`namer::CARD_PREFIX`). Both end in the name field, then Not important.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Card {
    pub code: String,
    pub kind: CardKind,
    pub quote: String,
    /// Unix ms.
    pub expires: u64,
    /// Asks only while the voice is in an active call.
    pub in_call: bool,
}
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum CardKind {
    /// "New voice: “…”": its guessed names, one click each (none without a real guess).
    New(Vec<String>),
    /// "Is this John?": `Yes` names it John.
    Proposal(String),
}
impl Card {
    pub(crate) fn id(&self) -> String {
        match self.kind {
            CardKind::New(_) => format!("{}{}", sg::CARD_PREFIX, self.code),
            CardKind::Proposal(_) => format!("{}{}", namer::CARD_PREFIX, self.code),
        }
    }
}

#[derive(Default)]
pub(super) struct VoiceNamer {
    running: Arc<AtomicBool>,
    last: Option<Instant>,
    /// The card on the island now (one at a time).
    card: Option<Card>,
    /// The last failure logged (once per distinct message).
    failure: Option<String>,
}

/// The namer's state file is read and written under this lock (the pass's thread and the card's answers).
static STATE: Mutex<()> = Mutex::new(());

impl Pipeline {
    fn namer_state_path(&self) -> PathBuf {
        self.data.join("voice-namer.json")
    }
    /// Changes the namer's state file under its lock.
    fn namer_state(&self, f: impl FnOnce(&mut namer::State)) -> Result<()> {
        let _guard = STATE.lock().unwrap_or_else(|e| e.into_inner());
        let path = self.namer_state_path();
        let mut state = namer::State::load(&path);
        f(&mut state);
        state.save(&path)
    }
    /// The coded voices on a track of an open, active call now.
    fn voices_in_call(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .speaker_names
            .iter()
            .filter(|(t, _)| self.discord.get(&t.source).is_some_and(|d| d.active && d.session == t.session))
            .map(|(_, n)| n.clone())
            // Codes only ("Speaker Fri 23 Oct 21h #3"); the pass keeps those in the store.
            .filter(|n| n.starts_with("Speaker ") && n.as_str() != "Speaker ?" && super::transcript_naming::unidentified_speaker(n).is_none())
            .collect();
        v.sort();
        v.dedup();
        v
    }
    /// The live loop's tick: while a call is open, at most once a `CHECK_EVERY`, one pass in the background.
    pub(super) fn voice_namer_tick(&mut self) {
        let now = agent::now_ms();
        // An expired card is gone from the island (expiry sends nothing back). A new-voice card left unanswered
        // snoozes its voice; the next card may show.
        if self.voice_namer.card.as_ref().is_some_and(|c| now >= c.expires) {
            let c = self.voice_namer.card.take().unwrap();
            self.ui(ui::UiUpdate::DismissAgentNotice(c.id()));
        }
        // A card that asks only during the call goes when its voice's call went quiet or closed.
        if let Some(c) = &self.voice_namer.card {
            if c.in_call && !self.voice_in_call(&c.code) {
                let id = c.id();
                self.voice_namer.card = None;
                self.ui(ui::UiUpdate::DismissAgentNotice(id));
            }
        }
        // Background Tasks ▸ Voice Names off: no pass (a card already up stays until answered or expired).
        if !crate::background_model::enabled(crate::background_model::Task::VoiceNames) { return; }
        // Emergency mode's pause: no pass, and no voice is marked checked (the next tick after resume runs it).
        if crate::background_gate::emergency_paused().is_some() { return; }
        // No pass while a card waits for the user: its answer may change what the next one should ask.
        if self.replay || self.voice_namer.card.is_some() || self.voice_namer.running.load(Ordering::Acquire) {
            return;
        }
        // Both cards ask only during a call.
        if self.discord.is_empty() || self.voice_namer.last.is_some_and(|t| t.elapsed() < namer::CHECK_EVERY) {
            return;
        }
        self.voice_namer.last = Some(Instant::now());
        let db = crate::archive::store_root(&self.session).join(heimdall_archive::store::INDEX);
        let store = self.data.join("speakers.json");
        let state = self.namer_state_path();
        let in_call = self.voices_in_call();
        let running = self.voice_namer.running.clone();
        let tx = self.internal.0.clone();
        running.store(true, Ordering::Release);
        std::thread::spawn(move || {
            // One read of the background model for the whole pass. Without a usable model (no key, or a choice off
            // Anthropic) new-voice cards still show, guessing only the voice it sounds like.
            // Its own model when the user gave it one (Background Tasks ▸ Voice Names), else the shared one.
            let endpoint = crate::background_model::task_endpoint(crate::background_model::Task::VoiceNames);
            let has_model = endpoint.key.is_some();
            let mut old = |p: &str| namer::ask_model(&endpoint, p);
            let mut look = |p: &str| sg::ask_model(&endpoint, p);
            let result = background_pass(&db, &store, &state, &in_call, agent::now_ms(), has_model.then_some(&mut old as &mut dyn FnMut(&str) -> Result<String>), has_model.then_some(&mut look as &mut dyn FnMut(&str) -> Result<String>));
            running.store(false, Ordering::Release);
            let _ = tx.send(Internal::Inference(Box::new(move |p: &mut Pipeline| p.voice_namer_done(result))));
        });
    }
    pub(super) fn voice_namer_done(&mut self, result: Result<namer::Outcome>) -> Result<()> {
        match result {
            Ok(namer::Outcome::Idle) => {}
            Ok(namer::Outcome::Waiting(w)) => {
                let _ = self.index.evidence.event("voice_namer", "heimdall", &[], capture::host_now_ns(), json!({"waiting": w}));
            }
            Ok(namer::Outcome::Unsure(code)) => {
                self.index.evidence.event("voice_namer", "heimdall", &[], capture::host_now_ns(), json!({"code": code, "named": null}))?;
            }
            Ok(namer::Outcome::Propose(p)) => {
                self.index.evidence.event("voice_namer", "heimdall", &[], capture::host_now_ns(), json!({"code": p.code, "named": p.name, "hints": p.hints, "quote": p.quote}))?;
                // The answer can come after the call ended or the voice left it: then no card, and it may ask later.
                if self.voice_in_call(&p.code) && self.voice_namer.card.is_none() {
                    let now = agent::now_ms();
                    self.namer_state(|s| {
                        let v = s.voices.entry(p.code.clone()).or_default();
                        if v.suggested_ms == 0 {
                            v.suggested_ms = now;
                        }
                        v.snoozed_until = v.snoozed_until.max(now + sg::SNOOZE_MS);
                    })?;
                    self.show_voice_card(p);
                } else {
                    self.namer_state(|s| {
                        if let Some(v) = s.voices.get_mut(&p.code) {
                            v.asked = None;
                        }
                    })?;
                }
            }
            Ok(namer::Outcome::Suggest(s)) => {
                self.index.evidence.event("voice_suggested", "heimdall", &[], capture::host_now_ns(), json!({
                    "code": s.code, "quote": s.quote, "error": s.error,
                    "candidates": s.candidates.iter().map(|c| json!({"name": c.name, "sure": c.sure})).collect::<Vec<_>>(),
                }))?;
                // The guess can come after the voice left the call: then no card, and it may come later.
                if self.voice_in_call(&s.code) && self.voice_namer.card.is_none() {
                    self.show_new_voice(s)?;
                }
            }
            Err(e) if crate::background_gate::quiet(&e) => {}
            Err(e) => {
                let message = format!("{e:#}");
                if self.voice_namer.failure.as_deref() != Some(message.as_str()) {
                    eprintln!("Voice namer: {message}");
                    let _ = self.index.evidence.event("voice_namer", "heimdall", &[], capture::host_now_ns(), json!({"error": message}));
                    self.voice_namer.failure = Some(message);
                }
            }
        }
        Ok(())
    }
    /// The new-voice card: "New voice: “…”", its guessed names one click each, the name field,
    /// Not important. It counts against the rate limit, and the voice rests for a snooze unless he answers.
    pub(crate) fn show_new_voice(&mut self, s: sg::Suggestion) -> Result<()> {
        if self.voice_namer.card.is_some() {
            return Ok(());
        }
        let now = agent::now_ms();
        self.namer_state(|st| {
            st.shown.retain(|t| now.saturating_sub(*t) < 24 * 3600_000);
            st.shown.push(now);
            let v = st.voices.entry(s.code.clone()).or_default();
            v.suggested_ms = now;
            v.snoozed_until = now + sg::SNOOZE_MS;
        })?;
        let options = sg::card_options(&s);
        let names = options[..options.len() - 1].to_vec();
        let card = Card { code: s.code.clone(), kind: CardKind::New(names), quote: s.quote.clone(), expires: now + sg::CARD_MS, in_call: true };
        self.show_card(card, sg::card_text(&s), options);
        Ok(())
    }
    /// The card was put away unanswered (Esc in its name field): the next card may come; its voice rests as if the
    /// card had expired.
    pub(super) fn voice_card_dismissed(&mut self, request_id: &str) {
        if let Some(c) = self.voice_namer.card.take_if(|c| c.id() == request_id) {
            let _ = self.index.evidence.event("voice_card_answer", "heimdall", &[], capture::host_now_ns(), json!({"code": c.code, "card": format!("{:?}", c.kind), "dismissed": true}));
        }
    }
    /// A track of an open, active call carries this voice's label now.
    fn voice_in_call(&self, code: &str) -> bool {
        self.speaker_names.iter().any(|(t, n)| n == code && self.discord.get(&t.source).is_some_and(|d| d.active && d.session == t.session))
    }
    /// The returning-voice proposal: "Is this John? “the user: hey John…”" with Yes, the name field, Not important.
    pub(super) fn show_voice_card(&mut self, p: namer::Proposal) {
        let card = Card { code: p.code.clone(), kind: CardKind::Proposal(p.name.clone()), quote: p.quote.clone(), expires: agent::now_ms() + namer::CARD_MS, in_call: true };
        self.show_card(card, namer::card_text(&p), [namer::YES, sg::NOT_IMPORTANT].map(String::from).to_vec());
    }
    /// A card on the island: `options`, the name field before the last of them (Not important). The field never takes
    /// the keyboard by itself: every voice card comes on its own, while he may be typing in another app; one click on
    /// the field gives it the keyboard.
    pub(super) fn show_card(&mut self, card: Card, text: String, options: Vec<String>) {
        if self.voice_namer.card.is_some() {
            return;
        }
        self.speaker_prompts += 1;
        let field = ui::agent_notice::OtherField { placeholder: sg::NAME_FIELD.into(), focus_until_ms: 0, after: 1 };
        self.ui(ui::UiUpdate::AgentNotice(voice_card(&card.id(), &text, &options, field, card.expires)));
        self.voice_namer.card = Some(card);
    }
    /// The card's answer (its request id is a card prefix + code): an option's label, or the name typed in the field
    /// (`typed`, saved exactly as typed).
    pub(super) fn voice_card_answer(&mut self, request_id: &str, choice: &str, typed: Option<&str>) -> Result<()> {
        let code = request_id.strip_prefix(namer::CARD_PREFIX).or_else(|| request_id.strip_prefix(sg::CARD_PREFIX)).context("not a voice card")?.to_string();
        self.ui(ui::UiUpdate::DismissAgentNotice(request_id.into()));
        if !self.voice_namer.card.as_ref().is_some_and(|c| c.code == code && c.id() == request_id) {
            bail!("that voice card is no longer open");
        }
        let card = self.voice_namer.card.take().unwrap();
        self.index.evidence.event("voice_card_answer", "heimdall", &[], capture::host_now_ns(), json!({"code": code, "card": format!("{:?}", card.kind), "choice": choice, "typed": typed}))?;
        if let Some(name) = typed.map(str::trim).filter(|t| !t.is_empty() && choice.is_empty()) {
            self.rename_voice(&code, name)?;
            return Ok(());
        }
        match (&card.kind, choice) {
            (_, sg::NOT_IMPORTANT | namer::FORGET) => self.forget_voice(&code)?,
            (CardKind::Proposal(name), namer::YES) => {
                self.rename_voice(&code, name)?;
            }
            (CardKind::New(names), _) if names.iter().any(|n| n == choice) => {
                self.rename_voice(&code, choice)?;
            }
            (_, other) => bail!("{other} is not one of the card's answers"),
        }
        Ok(())
    }
    /// The agent's `name_voice` with `voice` "latest": the coded voice that spoke last in the user's last conversation
    /// (an open call's voices first), its newest sentence, and the other unnamed voices of that conversation.
    pub(crate) fn latest_unnamed_voice(&self) -> Result<(String, String, Vec<String>)> {
        let store = self.load_store()?;
        let db = crate::archive::store_root(&self.session).join(heimdall_archive::store::INDEX);
        let conn = namer::open(&db)?;
        latest_unnamed(&conn, &store, &self.voices_in_call(), agent::now_ms() as i64)
    }
    /// A voice gets a name: a coded voice ("Speaker Fri 23 Oct 21h #3") or a named one renamed. The store first,
    /// then the open calls' labels, then every archived line in every session. Returns one line for the agent.
    pub(crate) fn rename_voice(&mut self, from: &str, to: &str) -> Result<String> {
        let (from, to) = (from.trim().to_string(), to.trim().to_string());
        ensure!(!to.is_empty() && to.chars().count() <= 128, "give a name of 1 to 128 characters");
        ensure!(from != to, "{from} already has that name");
        // The user is his microphone and Heimdall its own voice: no other voice takes those names (listen_to.rs).
        ensure!(!speakers::identity::NOT_GUESTS.iter().any(|n| n.eq_ignore_ascii_case(&to)), "{to} is not a name for another voice");
        // An anonymous track name stands for different people in different calls: never renamed everywhere.
        ensure!(
            super::transcript_naming::unidentified_speaker(&from).is_none() && !["Speaker ?", "Unknown", "User", "Heimdall", "audio"].contains(&from.as_str()),
            "{from} is not one voice"
        );
        let shared = self.enrollment_lock.clone();
        let guard = shared.lock().unwrap();
        let mut store = self.load_store()?;
        let in_store = if store.voices.contains_key(&from) {
            store.name_voice(&from, &to)?;
            true
        } else if let Some(e) = store.names.remove(&from) {
            for x in e.embeddings {
                store.enroll(&to, x)?;
            }
            // A renamed guest keeps being listened to (pipeline/listen_to.rs); never as the user or Heimdall.
            if e.listening {
                let _ = store.set_listening(&to, true);
            }
            true
        } else {
            false
        };
        // The archive first: a rename that stops halfway can be repeated (the store still has the old label), and
        // a label only the archive still carries (an earlier rename cut short) is renamed there.
        let lines = self.index.relabel_speaker(&from, &to)?;
        ensure!(in_store || lines > 0, "no voice is called {from}");
        if in_store {
            store.save(&self.data.join("speakers.json"))?;
        }
        drop(guard);
        self.index.evidence.event("voice_named", "heimdall", &[], capture::host_now_ns(), json!({"from": from, "to": to, "lines": lines}))?;
        self.rename_recovered(&from, &to)?;
        for d in self.discord.values() {
            d.actor.send(crate::speakers_runtime::Work::Store(store.clone()))?;
            d.actor.send(crate::speakers_runtime::Work::Rename(from.clone(), to.clone()))?;
        }
        let tracks: BTreeSet<TrackId> = self
            .labels
            .values()
            .filter_map(|(l, t)| l.label.as_ref().filter(|l| l.speaker == from).and(t.clone()))
            .chain(self.speaker_names.iter().filter(|(_, n)| **n == from).map(|(t, _)| t.clone()))
            .collect();
        self.speaker_events(tracks.into_iter().map(|track| SpeakerEvent::LabelChanged { track, name: to.clone() }).collect())?;
        if let Some(c) = self.voice_namer.card.take_if(|c| c.code == from) {
            self.ui(ui::UiUpdate::DismissAgentNotice(c.id()));
        }
        self.send_speakers();
        Ok(format!("Named: {from} is now {to}; {lines} earlier line(s) relabelled."))
    }
    /// Forget: the voice is irrelevant for good. It keeps its code label and samples; it is never proposed again.
    pub(crate) fn forget_voice(&mut self, code: &str) -> Result<()> {
        let shared = self.enrollment_lock.clone();
        let guard = shared.lock().unwrap();
        let mut store = self.load_store()?;
        ensure!(store.set_irrelevant(code, true), "no voice is called {code}");
        store.save(&self.data.join("speakers.json"))?;
        drop(guard);
        self.index.evidence.event("voice_forgotten", "heimdall", &[], capture::host_now_ns(), json!({"code": code}))?;
        if let Some(c) = self.voice_namer.card.take_if(|c| c.code == code) {
            self.ui(ui::UiUpdate::DismissAgentNotice(c.id()));
        }
        for d in self.discord.values() {
            d.actor.send(crate::speakers_runtime::Work::Store(store.clone()))?;
        }
        self.send_speakers();
        Ok(())
    }
}

/// A voice card on the island: one line, its answers as one-click options, and the name field before the last
/// `field.after` of them. Silent like every question card.
pub(crate) fn voice_card(id: &str, text: &str, options: &[String], field: ui::agent_notice::OtherField, expires_ms: u64) -> ui::agent_notice::AgentNotice {
    use ui::agent_notice::{AgentNotice, NoticeAction, NoticeKind};
    AgentNotice {
        id: id.into(),
        text: text.into(),
        kind: NoticeKind::Info,
        action: Some(NoticeAction::Ask {
            request_id: id.into(),
            // One question, its answers as options; its Other row is the name field.
            questions: vec![ui::agent_notice::AskQuestion {
                id: "q1".into(),
                header: None,
                text: text.into(),
                multi_select: false,
                allow_other: true,
                field: Some(field),
                options: options.iter().map(|l| ui::agent_notice::AskOption { label: l.clone(), description: None, grant: vec![] }).collect(),
            }],
            expires_ms,
            countdown: false,
        }),
        outcome: None,
        expires_ms: None,
        detail: None,
    }
}

type Ask<'a> = Option<&'a mut dyn FnMut(&str) -> Result<String>>;

/// One pass off the live loop: the archive read-only, the store read, the state merged back under its lock (only
/// the fields a pass owns, so an answer given meanwhile is kept). A due new-voice card first, with its guessed names
/// (`voice_suggest::next`, then `guess` on `look`), else the returning-voice proposal (only with a model: `old`).
pub(crate) fn background_pass(db: &Path, store: &Path, state_path: &Path, in_call: &[String], now_ms: u64, old: Ask, look: Ask) -> Result<namer::Outcome> {
    if !store.exists() || !db.exists() {
        return Ok(namer::Outcome::Idle);
    }
    let store = EnrollmentStore::load(store)?;
    if store.voices.is_empty() {
        return Ok(namer::Outcome::Idle);
    }
    let conn = namer::open(db)?;
    let before = {
        let _g = STATE.lock().unwrap_or_else(|e| e.into_inner());
        namer::State::load(state_path)
    };
    let mut after = before.clone();
    let out = (|| {
        if let Some(mut s) = sg::next(&conn, &store, &after, in_call, now_ms)? {
            sg::guess(&conn, &store, &after, &mut s, look)?;
            return Ok(namer::Outcome::Suggest(s));
        }
        match old {
            Some(ask) => namer::pass(&conn, &store, &mut after, now_ms, ask),
            None => Ok(namer::Outcome::Idle),
        }
    })();
    let _g = STATE.lock().unwrap_or_else(|e| e.into_inner());
    let mut fresh = namer::State::load(state_path);
    let mut changed = false;
    for (code, v) in after.voices {
        if before.voices.get(&code) != Some(&v) {
            let entry = fresh.voices.entry(code).or_default();
            entry.checked_ms = v.checked_ms;
            entry.lines_at_check = v.lines_at_check;
            if v.asked.is_some() {
                entry.asked = v.asked;
            }
            changed = true;
        }
    }
    if changed {
        fresh.save(state_path)?;
    }
    out
}

/// The coded voice that spoke last in the user's last conversation: among `in_call` when a call is open, else in the
/// archive's last `LATEST_WITHIN_MS`. Returns it, its newest sentence and the other unnamed voices heard within
/// `CONVERSATION_MS` of it.
pub(crate) const LATEST_WITHIN_MS: i64 = 12 * 3600_000;
pub(crate) const CONVERSATION_MS: i64 = 30 * 60_000;
pub(crate) fn latest_unnamed(db: &rusqlite::Connection, store: &EnrollmentStore, in_call: &[String], now_ms: i64) -> Result<(String, String, Vec<String>)> {
    let mut q = db.prepare("SELECT speaker, max(wall_ms) FROM utterances WHERE wall_ms>=?1 AND speaker LIKE 'Speaker %' AND coalesce(text,'')<>'' GROUP BY speaker ORDER BY 2 DESC")?;
    let heard: Vec<(String, i64)> = q
        .query_map([now_ms - LATEST_WITHIN_MS], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        // A voice marked Not important keeps its code, but is never the one the user means.
        .filter(|(c, _)| store.voices.get(c).is_some_and(|v| !v.irrelevant))
        .collect();
    let open: Vec<&(String, i64)> = heard.iter().filter(|(c, _)| in_call.contains(c)).collect();
    let pool: Vec<&(String, i64)> = if open.is_empty() { heard.iter().collect() } else { open };
    let Some((code, at)) = pool.first().map(|(c, t)| (c.clone(), *t)) else {
        bail!("no unnamed voice spoke in the last 12 hours (a voice needs about 12 seconds of clear speech before it is recognised); find the label with timeline_search");
    };
    let others = pool.iter().filter(|(c, t)| *c != code && at - *t <= CONVERSATION_MS).map(|(c, _)| c.clone()).collect();
    let quote = sg::heard(db, &code, at - CONVERSATION_MS)?.quote;
    Ok((code, quote, others))
}
