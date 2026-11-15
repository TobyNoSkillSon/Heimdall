//! The in-conversation suggester in the conductor: the window of finished talk, Decider V5's "suggest now" that starts
//! a pass, the one card on the island, the pick's notice to the main agent, and the typed "bad suggestion" that
//! attaches to the last card. The rules and the job are in `crate::suggester`.
//!
//! Decider V5 (29 Oct) learned suggest yes/no on every line, the user's and the people he talks with (`decide::suggest`).
//! After each line's send decision V5 is asked with the same conversation; at `SUGGEST_MIN` or more a pass starts, if
//! the pacing allows one (`sg::Pacer`: a model call a minute, a card every three minutes, no repeated topic, a pause
//! after two dismissals). It is not asked while the user is talking to Heimdall: a line that woke it or that it waits on,
//! the `ADDRESSING_MS` after, and while Heimdall speaks; a card held back by that waits for the next pass.
use super::*;
use crate::suggester as sg;
use anyhow::bail;
use std::sync::atomic::AtomicBool;
use serde_json::{json, Value};
use speakers::identity::EnrollmentStore;

/// V5's P(yes) at or above which a line starts a pass. On V5's frozen test split (5,484 real lines, labels by Haiku
/// 5.5): 0.7 fires on 2.8% of lines with precision 0.72 and recall 0.35; 0.5 fires on 3.4% at 0.68 (derived from
/// `training-material/d1/runs/v5/test_v5.json`, 29 Oct). The higher bar keeps cards rare; the pacing caps them anyway.
pub(super) const SUGGEST_MIN: f32 = 0.7;
/// The bar in a call (`suggest_call_open`), where Heimdall does not wake without its name and a card is the only way an
/// open question reaches it. Night 1112 lane G: in the user's calls on 11 Nov (13:35-14:35, 17:25-18:30) V5 was asked on 720
/// lines and reached 0.7 on 7; "Yeah, I didn't know open code has image generation." (18:12:01) scored 0.55 and started
/// nothing. At 0.5 three more lines would have started a pass in those two hours; the pacing still caps cards.
pub(super) const SUGGEST_MIN_CALL: f32 = 0.5;
/// After the user's line woke Heimdall or left it waiting for more, he is talking to Heimdall: no suggestion for this long.
pub(super) const ADDRESSING_MS: u64 = 20_000;

#[derive(Default)]
pub(super) struct Suggest {
    window: sg::Window,
    /// Until when (Unix ms) the user is talking to Heimdall (`ADDRESSING_MS`).
    addressing_until: u64,
    pacer: sg::Pacer,
    running: Arc<AtomicBool>,
    /// The card on the island, if any.
    card: Option<Shown>,
    /// The last card's id and when it was shown (Unix ms): typed feedback attaches to it.
    last: Option<(String, u64)>,
    /// The last failure logged (once per distinct message).
    failure: Option<String>,
}

/// A card on the island and what it was made from.
struct Shown {
    id: String,
    s: sg::Suggestion,
    shown_ms: u64,
    expires_ms: u64,
    excerpt: String,
    during_call: bool,
}

/// Clears the pass's running flag however the pass ends, a panic included.
struct Running(Arc<AtomicBool>);

impl Drop for Running {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// What a pass saw when it started, for its log lines.
pub(super) struct Snapshot {
    now: u64,
    excerpt: String,
    during_call: bool,
    /// The user pressed ⌃⌘' (hotkeys.rs): past the pacing and the addressing hold.
    forced: bool,
    /// The line of his the call gate held, for a `Pass::HeldInCall`: its own card if the pass stays silent or fails.
    held: Option<String>,
}

/// What the pass is told when the user asked for a card themselves (their name from Personalise, never a literal).
fn forced_note() -> String {
    format!("\n\n{} pressed the suggestion key: he wants one card now from this conversation. Show the most useful card you can; stay silent only if the transcript is empty.", agent::identity::user_start())
}

/// What starts a pass.
pub(super) enum Pass {
    /// Decider V5's suggest yes, within the pacing.
    Paced,
    /// ⌃⌘' (hotkeys.rs): past the pacing and the addressing hold.
    Hotkey,
    /// The user's line in a call that d1 sent and the call gate held (`held_in_call`, `decide::send::D1Gate::InCall`): past
    /// V5 and the minute's call gap; its card shows past the pacing.
    HeldInCall(String),
}

/// What the pass is told about a line of the user's that the call gate held (bug 2026-11-05T02-32-17).
fn held_note(line: &str) -> String {
    let line: String = line.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(600).collect();
    format!("\n\nIn this call {user} just said this, and the decider read it as meant for Heimdall, but in calls Heimdall does not wake without its name: \"{line}\". If it asks Heimdall for something, show the card that offers to do it, with his request in the handoff. Stay silent only if it was plainly said to the people on the call.", user = agent::identity::user())
}

impl Pipeline {
    fn suggest_paths(&self) -> sg::Paths {
        sg::Paths {
            home: std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default(),
            data: self.data.clone(),
            archive: Some(crate::archive::store_root(&self.session).join(heimdall_archive::store::INDEX)),
        }
    }
    fn log_suggest(&self, event: Value) {
        if let Err(e) = sg::append(&sg::log_path(&self.data), &event) {
            eprintln!("Suggester: log failed: {e:#}");
        }
    }
    /// The user is in a call or a conversation: a call line (any app in `context_policy::CALL_BUNDLES`) ended within the
    /// conversation window (`speech_policy`), or a Discord track is active.
    fn suggest_call_open(&self) -> bool {
        self.tools.speech_policy.in_conversation(capture::host_now_ns()) || self.discord.values().any(|d| d.active)
    }
    /// Every finished line of talk joins the suggester's window; whether it starts a pass is V5's (`suggest_ask`).
    pub(super) fn suggest_heard(&mut self, speaker: &str, text: &str, user: bool) {
        if text.trim().is_empty() {
            return;
        }
        let now = agent::now_ms();
        self.suggest.window.push(sg::Line { at_ms: now, speaker: speaker.into(), text: text.into(), user });
    }
    /// A pick can only reach a running agent: no pass while it is off or restarting. `is_running` stays true with
    /// Agent off (the supervisor lives on), so the switch itself is asked too (`background_gate`).
    fn suggest_allowed(&self) -> bool {
        #[cfg(test)]
        if self.hooks.suggest_allowed {
            return true;
        }
        // Background Tasks ▸ Suggestions off: no pass.
        if !crate::background_model::enabled(crate::background_model::Task::Suggestions) { return false; }
        !self.replay && self.agent.as_ref().is_some_and(|a| a.is_running()) && crate::background_gate::allowed()
    }
    /// The user is talking to Heimdall: a line of his that woke it or that it waits on, the `ADDRESSING_MS` after it, or
    /// Heimdall speaking.
    fn suggest_addressing(&self, now: u64) -> bool {
        now < self.suggest.addressing_until || self.tools.heimdall_speaking()
    }
    /// After a line's send decision (`record`, the `send_decision` evidence): ask Decider V5 whether this is a good
    /// moment for a card, the user's line or a call guest's, unless he is talking to Heimdall or no card could follow.
    pub(super) fn suggest_ask(&mut self, source: &str, snapshot: &decide::send::Snapshot, record: &Value) {
        let now = agent::now_ms();
        let Some(line) = snapshot.pending.iter().find(|t| t.seq == snapshot.through) else { return };
        let user = self.user_source(source) && line.speaker == agent::identity::DEFAULT_USER;
        let outcome = record["outcome"].as_str().unwrap_or("");
        // Bug 2026-11-05T02-32-17: in a call, a line of his that d1 sent but the call gate held (23:30-23:34, six lines
        // at send 0.76-0.96, until he pressed ⌃⌘\ at 23:35:01) reaches nobody. It no longer counts as talking to
        // Heimdall (which held every card for 20 s); it starts a pass at once, so a card can offer what he asked.
        if user && outcome == "held_in_call" {
            self.suggest_held(&line.text, line.observation.as_ref().and_then(|o| o["id"].as_str()).unwrap_or_default());
            return;
        }
        if user && (outcome == "send" || record["choice"] == "wait" || record["choice"] == "send") {
            self.suggest.addressing_until = now + ADDRESSING_MS;
        }
        if outcome == "send" || self.own_audio(source) || line.text.trim().is_empty() || self.suggest_addressing(now)
            || self.suggest.card.is_some() || self.suggest.running.load(Ordering::Acquire) || !self.suggest.pacer.may_call(now)
            || !self.suggest_allowed()
        {
            return;
        }
        let ids = [line.observation.as_ref().and_then(|o| o["id"].as_str()).unwrap_or_default().to_owned()];
        let speaker = line.speaker.clone();
        let done = move |p: &mut Pipeline, result: Result<(decide::send::Fitted, decide::suggest::Suggestion)>| -> Result<()> {
            let now = agent::now_ms();
            let (yes, mut evidence) = match result {
                Ok((fitted, s)) => (Some(s.yes), json!({"schema":1,"yes":s.yes,"choice":s.choice,"input_tokens":s.input_tokens,
                    "context_tokens":fitted.context_tokens,"dropped_turns":fitted.dropped_turns,"model_output":s.output})),
                Err(e) => (None, json!({"schema":1,"error":format!("{e:#}")})),
            };
            let in_call = p.suggest_call_open();
            let bar = if in_call { SUGGEST_MIN_CALL } else { SUGGEST_MIN };
            let fires = yes.is_some_and(|y| y >= bar);
            evidence["speaker"] = json!(speaker);
            evidence["threshold"] = json!(bar);
            evidence["in_call"] = json!(in_call);
            evidence["outcome"] = json!(if !fires { "no" } else if p.suggest_addressing(now) { "held_addressing" } else { "pass" });
            p.index.evidence.event("suggest_decision", "heimdall", &ids, capture::host_now_ns(), evidence)?;
            if fires && !p.suggest_addressing(now) {
                eprintln!("Suggester: Decider V5 {:.2} on {speaker}'s line{}: a pass if the pacing allows", yes.unwrap_or_default(), if in_call { " in a call" } else { "" });
                p.suggest_pass(now);
            }
            Ok(())
        };
        #[cfg(test)]
        if let Some(yes) = self.hooks.suggest.pop_front() {
            let s = decide::suggest::Suggestion { yes, choice: yes >= 0.5, input_tokens: 0, output: json!({"test":true}) };
            let fitted = decide::send::Fitted { request: json!({}), context_tokens: 0, dropped_turns: 0, pending_truncated: false };
            if let Err(e) = done(self, Ok((fitted, s))) {
                eprintln!("Suggest decision failed: {e:#}");
            }
            return;
        }
        if decide::send_v3::Generation::shipped() != decide::send_v3::Generation::V5 {
            return;
        }
        // A sentence waiting for its send decision goes first: suggest never delays a wake (review 29 Oct).
        if self.send_worker.as_ref().is_some_and(|o| o.send_busy()) {
            return;
        }
        let snapshot = snapshot.clone();
        let queued = self.inference().and_then(|owner| owner.request(
            0,
            snapshot.token_bound(),
            move |state| {
                // Its outcome never counts toward the decider's health: a failing suggest head must not unload the wake
                // model. Failures are `suggest_decision` evidence.
                let urgent = state.urgent.clone();
                state.decider().and_then(|d| d.suggest_decision_interruptible(&snapshot, &urgent))
            },
            done,
        ));
        if let Err(e) = queued {
            eprintln!("Suggest question not queued: {e:#}");
        }
    }
    /// One pass on the background model, if the pacing allows it and the model is Anthropic's (`background_model`).
    fn suggest_pass(&mut self, now: u64) {
        if self.suggest.card.is_some() || self.suggest.running.load(Ordering::Acquire) || !self.suggest.pacer.may_call(now) {
            return;
        }
        self.suggest_start(now, Pass::Paced);
    }
    /// A line of the user's held by the call gate: a pass now, without V5's question or the minute's call gap, unless a pass
    /// is already running or a card is up (a run of held lines makes one card, not one each). Journalled as a
    /// `suggest_decision` with outcome `held_in_call_pass` (or `held_in_call_busy`).
    fn suggest_held(&mut self, text: &str, id: &str) {
        if text.trim().is_empty() || !self.suggest_allowed() {
            return;
        }
        let busy = self.suggest.card.is_some() || self.suggest.running.load(Ordering::Acquire);
        let evidence = json!({"schema":1,"speaker":agent::identity::DEFAULT_USER,"outcome":if busy {"held_in_call_busy"} else {"held_in_call_pass"}});
        if let Err(e) = self.index.evidence.event("suggest_decision", "heimdall", &[id.to_owned()], capture::host_now_ns(), evidence) {
            eprintln!("Suggest decision failed: {e:#}");
        }
        if !busy {
            self.suggest_start(agent::now_ms(), Pass::HeldInCall(text.to_owned()));
        }
    }
    /// ⌃⌘': a pass now, whatever the pacing; an open card makes way for the new one.
    pub(super) fn suggest_now(&mut self) {
        if self.suggest.running.load(Ordering::Acquire) {
            eprintln!("hotkey: suggest pressed while a pass is already running; its card comes when it ends");
            return;
        }
        if !self.suggest_allowed() {
            eprintln!("hotkey: suggest pressed while the agent is off or stopped; no card");
            return;
        }
        if let Some(c) = self.suggest.card.take() {
            self.suggest.pacer.closed();
            self.ui(ui::UiUpdate::DismissAgentNotice(c.id.clone()));
            self.log_suggest(json!({"id": c.id, "event": "replaced", "at": agent::now_ms()}));
        }
        self.suggest_start(agent::now_ms(), Pass::Hotkey);
    }
    fn suggest_start(&mut self, now: u64, pass: Pass) {
        let forced = !matches!(pass, Pass::Paced);
        let hotkey = matches!(pass, Pass::Hotkey);
        // The endpoint is resolved on the pass's own thread: a ChatGPT sign-in may wait on the Keychain or a token
        // refresh, and this is the pipeline's thread (review 27 Oct).
        self.suggest.pacer.called(now);
        let known: Vec<String> = EnrollmentStore::load(&self.data.join("speakers.json")).map(|s| s.names.keys().cloned().collect()).unwrap_or_default();
        let mut prompt = sg::prompt(&self.suggest.window, now, &known);
        match &pass {
            Pass::Paced => {}
            Pass::Hotkey => { prompt.push(forced_note()); }
            Pass::HeldInCall(line) => { prompt.push(held_note(line)); }
        }
        let held = match &pass { Pass::HeldInCall(line) => Some(line.clone()), _ => None };
        match &pass {
            Pass::Paced => {}
            Pass::Hotkey => eprintln!("Suggester: a pass on his key"),
            Pass::HeldInCall(_) => eprintln!("Suggester: a pass for his line the call gate held"),
        }
        let snap = Snapshot { now, excerpt: self.suggest.window.transcript(), during_call: self.suggest_call_open(), forced, held };
        let paths = self.suggest_paths();
        let running = self.suggest.running.clone();
        let tx = self.internal.0.clone();
        // Tests never reach a background model (no inference in the default suite): the pass ends here.
        #[cfg(test)]
        {
            let _ = (snap, paths, tx, hotkey, running);
            return;
        }
        #[allow(unreachable_code)]
        running.store(true, Ordering::Release);
        let guard = Running(running);
        std::thread::spawn(move || {
            let _guard = guard;
            // Its own model (Background Tasks ▸ Suggestions), else the shared one; a fallback while that one is spent.
            let endpoint = crate::background_model::task_endpoint(crate::background_model::Task::Suggestions);
            if endpoint.key.is_none() {
                if hotkey { eprintln!("hotkey: suggest pressed, but no background model is signed in; no card"); }
                return;
            }
            let result = sg::ask(&endpoint, prompt, &paths);
            let _ = tx.send(Internal::Inference(Box::new(move |p: &mut Pipeline| p.suggest_done(result, snap))));
        });
    }
    /// The pass's answer: silence is logged; a card shows if the pacing still allows it (the pass took a while).
    pub(super) fn suggest_done(&mut self, result: Result<sg::Verdict>, snap: Snapshot) -> Result<()> {
        // A run that worked clears the failure, so the next failure is logged again.
        if result.is_ok() {
            self.suggest.failure = None;
        }
        // Bug note 2026-11-05T02-32-17 item 2 (night 1111): a line of his the call gate held, that d1 read as for
        // Heimdall, always gets a card: when the pass stays silent or fails, his own words are offered (`sg::held_card`).
        // Night 1112 lane G: every outcome gets a line in the app log. On 11 Nov the passes in the user's calls left only
        // "suggester: gpt-6-luna answered" there, and "nothing ever triggered" took the suggestions log to explain.
        let kind = if snap.held.is_some() { "held line" } else if snap.forced { "his key" } else { "Decider V5" };
        let place = if snap.during_call { ", in a call" } else { "" };
        let fallback = snap.held.as_deref().and_then(sg::held_card);
        let no_fallback = if snap.held.is_some() && fallback.is_none() { "; the held line asks for nothing or would act outward" } else { "" };
        let result = match (result, fallback) {
            (Ok(sg::Verdict::Silent(reason)), Some(card)) => {
                self.log_suggest(json!({"event": "silent", "at": snap.now, "reason": reason, "transcript": snap.excerpt, "held_fallback": true}));
                Ok(sg::Verdict::Card(card))
            }
            (Err(e), Some(card)) => {
                eprintln!("Suggester: {e:#}; the line held in the call gets its own card");
                self.log_suggest(json!({"event": "error", "at": snap.now, "error": format!("{e:#}"), "held_fallback": true}));
                Ok(sg::Verdict::Card(card))
            }
            (result, _) => result,
        };
        match result {
            Ok(sg::Verdict::Silent(reason)) => {
                eprintln!("Suggester: no card ({kind}{place}): {reason}{no_fallback}");
                self.log_suggest(json!({"event": "silent", "at": snap.now, "reason": reason, "transcript": snap.excerpt}));
            }
            Ok(sg::Verdict::Card(s)) => {
                let now = agent::now_ms();
                if !snap.forced && self.suggest_addressing(now) {
                    // He started talking to Heimdall while the pass ran: no card over that exchange.
                    eprintln!("Suggester: no card ({kind}{place}): he is talking to Heimdall; it was on {}", s.topic);
                    self.log_suggest(json!({"event": "silent", "at": now, "reason": "talking to Heimdall", "topic": s.topic}));
                    return Ok(());
                }
                if self.suggest.card.is_some() || (!snap.forced && !self.suggest.pacer.may_show(now, &s.topic)) {
                    eprintln!("Suggester: no card ({kind}{place}): held by the pacing; it was on {}", s.topic);
                    self.log_suggest(json!({"event": "silent", "at": now, "reason": "pacing: a card is up, the gap, a pause, or a repeat topic", "topic": s.topic}));
                    return Ok(());
                }
                // Expiry is `suggest_tick`'s; the card's request id is `CARD_PREFIX` and a fresh uuid.
                let id = format!("{}{}", sg::CARD_PREFIX, uuid::Uuid::new_v4());
                let expires_ms = now + sg::CARD_MS;
                self.ui(ui::UiUpdate::AgentNotice(sg::card(&id, &s, expires_ms)));
                self.suggest.pacer.shown(now, &s.topic);
                eprintln!("Suggester: card shown ({kind}{place}): {}", s.topic);
                self.log_suggest(json!({
                    "id": id, "event": "shown", "at": now, "topic": s.topic, "card": sg::card_line(&s),
                    "questions": s.questions, "found": s.found, "handoff": s.handoff,
                    "transcript": snap.excerpt, "during_call": snap.during_call,
                }));
                self.suggest.last = Some((id.clone(), now));
                self.suggest.card = Some(Shown { id, s, shown_ms: now, expires_ms, excerpt: snap.excerpt, during_call: snap.during_call });
            }
            Err(e) if crate::background_gate::quiet(&e) => {}
            Err(e) => {
                let message = format!("{e:#}");
                if self.suggest.failure.as_deref() != Some(message.as_str()) {
                    eprintln!("Suggester: {message}");
                    self.log_suggest(json!({"event": "error", "at": snap.now, "error": message}));
                    self.suggest.failure = Some(message);
                }
            }
        }
        Ok(())
    }
    /// The live loop's tick: a card nobody answered in time leaves the island (logged as ignored).
    pub(super) fn suggest_tick(&mut self) {
        let now = agent::now_ms();
        if self.suggest.card.as_ref().is_some_and(|c| now >= c.expires_ms) {
            let c = self.suggest.card.take().unwrap();
            self.suggest.pacer.closed();
            self.ui(ui::UiUpdate::DismissAgentNotice(c.id.clone()));
            self.log_suggest(json!({"id": c.id, "event": "ignored", "at": now}));
        }
    }
    /// The card's answer. A dismissal ("Not now") is logged and paces; a pick wakes the main agent with the handoff.
    pub(super) fn suggest_answer(&mut self, request_id: &str, answers: &[ui::agent_notice::QuestionAnswer]) -> Result<()> {
        self.ui(ui::UiUpdate::DismissAgentNotice(request_id.into()));
        if self.suggest.card.as_ref().is_none_or(|c| c.id != request_id) {
            bail!("that suggestion is no longer open");
        }
        let card = self.suggest.card.take().unwrap();
        self.suggest.pacer.closed();
        let now = agent::now_ms();
        let outcome = if sg::dismissed(answers) { sg::Outcome::Dismissed } else { sg::Outcome::Picked };
        self.suggest.pacer.resolved(now, outcome);
        self.suggest.last = Some((card.id.clone(), card.shown_ms));
        self.log_suggest(json!({"id": card.id, "event": outcome.name(), "at": now, "choice": sg::choice(answers)}));
        if outcome == sg::Outcome::Picked {
            let text = sg::handoff(&card.s, answers, card.shown_ms, card.during_call, &card.excerpt);
            let id = format!("{}{}", sg::PICK_PREFIX, card.id.trim_start_matches(sg::CARD_PREFIX));
            if let Err(e) = self.tools.notice_pick(id, text) {
                self.log_suggest(json!({"id": card.id, "event": "error", "at": now, "error": format!("{e:#}")}));
                return Err(e);
            }
        }
        Ok(())
    }
    /// Typed "bad suggestion" or "irrelevant" within `FEEDBACK_MS` of the last card attaches to it and is not sent to
    /// the agent. An open card closes with it, and it paces as a dismissal. Returns whether it was consumed.
    pub(super) fn suggest_feedback(&mut self, text: &str) -> bool {
        let Some(kind) = sg::feedback(text) else { return false };
        let now = agent::now_ms();
        let Some((id, shown)) = self.suggest.last.clone() else { return false };
        if now.saturating_sub(shown) > sg::FEEDBACK_MS {
            return false;
        }
        self.log_suggest(json!({"id": id, "event": "feedback", "at": now, "kind": kind, "text": text}));
        if self.suggest.card.as_ref().is_some_and(|c| c.id == id) {
            self.suggest.card = None;
            self.suggest.pacer.closed();
            self.ui(ui::UiUpdate::DismissAgentNotice(id));
            self.suggest.pacer.resolved(now, sg::Outcome::Dismissed);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::Pipeline;

    fn app() -> (tempfile::TempDir, Pipeline) {
        let t = tempfile::tempdir().unwrap();
        let a = Pipeline::new(t.path().join("data"), t.path().join("missing-models"), crate::settings::Settings::default(), None, true).unwrap();
        (t, a)
    }
    fn cards(a: &Pipeline) -> Vec<ui::agent_notice::AgentNotice> {
        a.hooks.ui.lock().unwrap().iter().filter_map(|u| match u { ui::UiUpdate::AgentNotice(n) => Some(n.clone()), _ => None }).collect()
    }
    fn dismissals(a: &Pipeline) -> Vec<String> {
        a.hooks.ui.lock().unwrap().iter().filter_map(|u| match u { ui::UiUpdate::DismissAgentNotice(id) => Some(id.clone()), _ => None }).collect()
    }
    fn log(a: &Pipeline) -> Vec<Value> {
        std::fs::read_to_string(sg::log_path(&a.data)).unwrap_or_default().lines().map(|l| serde_json::from_str(l).unwrap()).collect()
    }
    fn card_verdict(topic: &str) -> sg::Verdict {
        let answer = json!({
            "suggest": true, "topic": topic,
            "questions": [{"text": "Should I find it?", "options": ["Find the file", "Not now"], "multi_select": false}],
            "found": [], "handoff": "Find the drawing from the laptop."
        });
        sg::verify(&answer.to_string(), &[]).unwrap()
    }
    fn snap(now: u64) -> Snapshot {
        Snapshot { now, excerpt: "[10:00 Sam] the CAD file from the laptop".into(), during_call: false, forced: false, held: None }
    }
    fn answer(labels: &[&str], other: Option<&str>) -> Vec<ui::agent_notice::QuestionAnswer> {
        vec![ui::agent_notice::QuestionAnswer { question_id: "q1".into(), labels: labels.iter().map(|l| l.to_string()).collect(), other: other.map(str::to_owned) }]
    }

    /// A conversation whose current line is `speaker`'s, as the send decision saw it.
    fn line(speaker: &str, text: &str) -> decide::send::Snapshot {
        let mut c = decide::send::Conversation::default();
        c.observe_at("User", "did you see the new MacBooks", 1, json!({"id":"s:1"}));
        c.observe_at(speaker, text, 2, json!({"id":"s:2"}));
        c.snapshot()
    }
    fn decisions(a: &Pipeline) -> Vec<Value> {
        a.index.evidence.records().unwrap().into_iter().filter(|r| r.kind == "suggest_decision").map(|r| r.payload).collect()
    }
    fn not_for() -> Value {
        json!({"choice":"not_for_heimdall","outcome":"not_for_heimdall"})
    }

    /// A friend's line on a call (a call guest, not the user): Decider V5 says yes above the bar and a suggester pass starts;
    /// the pacing then allows no second model call within the minute, so the next yes asks nothing.
    #[test]
    fn a_friends_line_above_the_bar_starts_one_pass() {
        let (_t, mut a) = app();
        a.hooks.suggest_allowed = true;
        a.hooks.suggest.push_back(0.83);
        a.suggest_ask("discord", &line("Morgan", "I wonder which codec keeps the voice tracks separate"), &not_for());
        let d = decisions(&a);
        assert_eq!((d[0]["outcome"].as_str(), d[0]["speaker"].as_str()), (Some("pass"), Some("Morgan")), "{d:?}");
        assert!(!a.suggest.pacer.may_call(agent::now_ms()), "the pass took the minute's model call");
        a.hooks.suggest.push_back(0.9);
        a.suggest_ask("discord", &line("Morgan", "and which one is smallest"), &not_for());
        assert_eq!(decisions(&a).len(), 1, "rate-limited: V5 not even asked");
        assert_eq!(a.hooks.suggest.len(), 1);
    }

    /// ⌃⌘' (hotkeys.rs): the user's own request shows its card past the pacing (a card a minute ago on the same topic).
    #[test]
    fn a_forced_pass_shows_its_card_past_the_pacing() {
        let (_t, mut a) = app();
        let now = agent::now_ms();
        a.suggest.pacer.called(now);
        a.suggest.pacer.shown(now, "cad file");
        a.suggest_done(Ok(card_verdict("cad file")), snap(now)).unwrap();
        assert!(cards(&a).is_empty(), "paced: no card");
        a.suggest_done(Ok(card_verdict("cad file")), Snapshot { forced: true, ..snap(now) }).unwrap();
        assert_eq!(cards(&a).len(), 1, "his key: the card shows");
    }

    /// Bug 2026-11-05T02-32-17: in a call, the user's line that d1 sent and the call gate held (23:34:33, "look at some
    /// other well-made profiles on LinkedIn…", send 0.93, `held_in_call`) used to open the 20-s "talking to Heimdall"
    /// hold and reach nobody. It now starts a pass at once, without V5's question and past the minute's call gap, and
    /// opens no hold; a second held line while that pass runs starts nothing more.
    #[test]
    fn a_line_held_in_a_call_starts_a_pass_instead_of_holding_cards() {
        let (_t, mut a) = app();
        a.hooks.suggest_allowed = true;
        let mic = crate::fake_source("mic", 0, 1);
        let id = mic.id.0.clone();
        a.source(mic);
        let now = agent::now_ms();
        a.suggest.pacer.called(now); // a pass a moment ago: the minute's gap would hold a V5 pass
        let held = json!({"choice":"send","outcome":"held_in_call","d1_gate":{"gate":"in_call"}});
        a.suggest_ask(&id, &line("User", "Look at some other well-made profiles on LinkedIn and update it"), &held);
        assert_eq!(a.suggest.addressing_until, 0, "a held line is not talking to Heimdall");
        let d = decisions(&a);
        assert_eq!(d.len(), 1, "{d:?}");
        assert_eq!(d[0]["outcome"], "held_in_call_pass");
        assert!(a.hooks.suggest.is_empty(), "V5 was not asked");
        assert!(!a.suggest.pacer.may_call(agent::now_ms()), "the held pass took the call");
        // The card a held pass returns shows past the pacing (a card on the same topic a moment ago), like his key's.
        a.suggest.pacer.shown(now, "LinkedIn profile");
        a.suggest_done(Ok(card_verdict("LinkedIn profile")), Snapshot { forced: true, ..snap(now) }).unwrap();
        assert_eq!(cards(&a).len(), 1);
        // With that card up, the next held line starts nothing: one card for a run of held lines.
        a.suggest_ask(&id, &line("User", "and make it nice"), &held);
        assert_eq!(decisions(&a)[1]["outcome"], "held_in_call_busy");
        assert_eq!(cards(&a).len(), 1);
        assert!(held_note("Look at it").contains("\"Look at it\""));
    }

    /// Bug note 2026-11-05T02-32-17 item 2 (night 1111): on 8 Nov 01:45:02 the user's "Okay, bro, sorry, run it in eight in
    /// in four-bit. Just run it once…" (d1 send 0.99) was held in the call, and its pass answered "quiet: Transcription
    /// report": no card, nothing reached Heimdall. A held line's pass that stays silent or fails now shows his own words
    /// as the card (the pick hands them over); one whose words propose contacting someone stays silent.
    #[test]
    fn a_held_lines_silent_or_failed_pass_still_shows_his_words() {
        let line = "Okay, bro, sorry, run it in four-bit. Just run it once and give me the transcript.";
        let held = |now| Snapshot { forced: true, during_call: true, held: Some(line.into()), ..snap(now) };
        let (_t, mut a) = app();
        a.suggest_done(Ok(sg::Verdict::Silent("quiet: Transcription report".into())), held(agent::now_ms())).unwrap();
        let shown = cards(&a);
        assert_eq!(shown.len(), 1, "his words get a card");
        let events = log(&a);
        assert_eq!((events[0]["event"].as_str(), events[0]["held_fallback"].as_bool()), (Some("silent"), Some(true)));
        assert_eq!(events[1]["event"], "shown");
        assert!(events[1]["handoff"].as_str().unwrap().contains("run it in four-bit"), "{}", events[1]);
        assert_eq!(events[1]["questions"][0]["options"], json!(["Do it", "Not now"]));
        // The model call failed (8 Nov 10:54:36 "request failed: error sending request"): his words still show.
        let (_t, mut a) = app();
        a.suggest_done(Err(anyhow::anyhow!("request failed: error sending request")), held(agent::now_ms())).unwrap();
        assert_eq!(cards(&a).len(), 1);
        // A paced pass's silence stays silence; a held line that would contact someone gets no card of its own.
        let (_t, mut a) = app();
        a.suggest_done(Ok(sg::Verdict::Silent("quiet: small talk".into())), snap(agent::now_ms())).unwrap();
        let contact = Snapshot { held: Some("Message Angel the transcript".into()), ..held(agent::now_ms()) };
        a.suggest_done(Ok(sg::Verdict::Silent("quiet".into())), contact).unwrap();
        assert!(cards(&a).is_empty());
    }

    /// Night 1112 lane G, the user's call of 11 Nov ("I don't think it triggered at any moment in the conversation"): the real
    /// lines of 18:11:43-18:13:32. V5 gave "Yeah, I didn't know open code has image generation." 0.55, under the 0.7 bar,
    /// and nothing started; the pass V5 started at 18:14:18 over the same talk answered "quiet: OpenCode image
    /// generation". In a call the bar is now 0.5, so that line starts a pass, the pass sees the question, the look-up card
    /// it should return shows on the call, and the same talk's outward offer (a friend's "can you send the GitHub…") is
    /// still refused. Outside a call the bar stays 0.7. V5 and the model are fakes: no inference.
    #[test]
    fn an_open_question_on_a_real_call_starts_a_pass_and_its_look_up_card_shows() {
        const CALL: &[(&str, &str)] = &[
            ("User", "How though?"),
            ("Speaker Wed 11 Nov 16h #2", "What do you mean how? It just can."),
            ("User", "Like how did you connect the image generation? From where?"),
            ("Speaker Wed 11 Nov 16h #2", "In open code?"),
            ("Unknown", "I'm confused."),
            ("User", "Yeah, I didn't know open code has image generation."),
            ("Speaker Wed 11 Nov 16h #2", "Does it really generate though? I just don't understand."),
            ("User", "Yeah, I didn't know that they have image generation on OpenCode. Can you double check that? Because if that's the case"),
            ("Speaker Mon 26 Oct 18h #1", "Wait, you have a User, can you send the GitHub of your all-in-one Swiss knife?"),
        ];
        let question = "Yeah, I didn't know open code has image generation.";
        let run = |in_call: bool| {
            let (t, mut a) = app();
            a.hooks.suggest_allowed = true;
            let mic = crate::fake_source("mic", 0, 1);
            let id = mic.id.0.clone();
            a.source(mic);
            if in_call {
                a.tools.speech_policy.other_line(capture::host_now_ns());
            }
            for (speaker, text) in CALL {
                a.suggest_heard(speaker, text, *speaker == "User");
            }
            a.hooks.suggest.push_back(0.55);
            a.suggest_ask(&id, &line("User", question), &not_for());
            (t, a)
        };
        let (_t, quiet) = run(false);
        assert_eq!((decisions(&quiet)[0]["outcome"].as_str(), decisions(&quiet)[0]["threshold"].as_f64()), (Some("no"), Some(0.7f32 as f64)));
        let (_t, mut a) = run(true);
        let d = decisions(&a);
        assert_eq!((d[0]["outcome"].as_str(), d[0]["in_call"].as_bool()), (Some("pass"), Some(true)), "{d:?}");
        assert!(!a.suggest.pacer.may_call(agent::now_ms()), "the pass took the minute's model call");
        // The pass sees the question in the transcript, under a system prompt that names this case.
        let now = agent::now_ms();
        let prompt = format!("{:?}", sg::prompt(&a.suggest.window, now, &[]));
        assert!(prompt.contains("Can you double check that?"), "{prompt}");
        assert!(sg::SYSTEM.contains("Can you double check that?"));
        // The card a right pass returns shows on the call.
        let look_up = json!({
            "suggest": true, "topic": "OpenCode image generation",
            "questions": [{"text": "Check whether OpenCode can generate images?", "options": ["Look it up", "Not now"], "multi_select": false}],
            "found": [], "handoff": "On a call User wondered whether OpenCode has image generation and asked to double check it."
        });
        let verdict = sg::verify(&look_up.to_string(), &[]).unwrap();
        assert!(matches!(verdict, sg::Verdict::Card(_)), "{verdict:?}");
        a.suggest_done(Ok(verdict), Snapshot { during_call: true, ..snap(now) }).unwrap();
        assert_eq!(cards(&a).len(), 1);
        let shown = log(&a);
        assert_eq!((shown[0]["event"].as_str(), shown[0]["during_call"].as_bool()), (Some("shown"), Some(true)));
        // The same talk's outward offers are still refused: sending the friend the link, or sharing it.
        for (text, option) in [("Send him the GitHub link?", "Send the link"), ("Share the Swiss knife repo?", "Share it")] {
            let outward = json!({"suggest": true, "topic": "Swiss knife GitHub", "found": [], "handoff": "Share the repository.",
                "questions": [{"text": text, "options": [option, "Not now"], "multi_select": false}]});
            assert!(matches!(sg::verify(&outward.to_string(), &[]).unwrap(), sg::Verdict::Silent(_)), "{text}");
        }
    }

    /// Night 1112 lane G: the held lines of 11 Nov that asked for nothing get no card when their pass is quiet.
    #[test]
    fn a_held_line_that_asks_for_nothing_stays_silent_with_its_pass() {
        let (_t, mut a) = app();
        for line in ["Okay, hi Doll, I'm back.", "But the scrolling is very smooth. I like the scrolling."] {
            let held = Snapshot { forced: true, during_call: true, held: Some(line.into()), ..snap(agent::now_ms()) };
            a.suggest_done(Ok(sg::Verdict::Silent("quiet: Call greeting".into())), held).unwrap();
        }
        assert!(cards(&a).is_empty());
        assert!(log(&a).iter().all(|e| e["event"] == "silent" && e.get("held_fallback").is_none()), "{:?}", log(&a));
    }

    /// Below the bar (0.7) V5's answer is journalled and nothing starts.
    #[test]
    fn below_the_bar_nothing_starts() {
        let (_t, mut a) = app();
        a.hooks.suggest_allowed = true;
        a.hooks.suggest.push_back(SUGGEST_MIN - 0.05);
        a.suggest_ask("discord", &line("Morgan", "yeah that was funny"), &not_for());
        assert_eq!(decisions(&a)[0]["outcome"], "no");
        assert!(a.suggest.pacer.may_call(agent::now_ms()));
    }

    /// While the user talks to Heimdall no suggestion is asked: a line that woke Heimdall, one it waits on, and the 20 s
    /// after, guests' lines included. Without a running agent (nothing could take a pick) it is not asked either.
    #[test]
    fn talking_to_heimdall_holds_suggestions() {
        let (_t, mut a) = app();
        a.hooks.suggest_allowed = true;
        a.hooks.suggest.extend([0.95, 0.95]);
        a.suggest_ask("discord", &line("Morgan", "check that file"), &json!({"choice":"send","outcome":"send"}));
        assert!(decisions(&a).is_empty(), "a line that woke Heimdall is for Heimdall");
        // His own wait (mid-sentence to Heimdall) opens the hold; the mic source makes it his line.
        let mic = crate::fake_source("mic", 0, 1);
        let id = mic.id.0.clone();
        a.source(mic);
        a.suggest_ask(&id, &line("User", "Heimdall, can you, um"), &json!({"choice":"wait","outcome":"wait"}));
        a.suggest_ask("discord", &line("Morgan", "what codec was that"), &not_for());
        assert!(decisions(&a).is_empty() && a.hooks.suggest.len() == 2, "held for {ADDRESSING_MS} ms");
        a.suggest.addressing_until = 0;
        a.hooks.suggest_allowed = false;
        a.suggest_ask("discord", &line("Morgan", "what codec was that"), &not_for());
        assert!(decisions(&a).is_empty(), "no running agent: nothing could take a pick");
    }

    #[test]
    fn a_card_shows_once_and_the_gap_holds_the_next_one() {
        let (_t, mut a) = app();
        a.suggest_done(Ok(card_verdict("CAD file")), snap(agent::now_ms())).unwrap();
        let shown = cards(&a);
        assert_eq!(shown.len(), 1);
        assert!(shown[0].id.starts_with(sg::CARD_PREFIX), "{}", shown[0].id);
        // A second card at once, on another topic, is held by the gap: logged as silence, not shown.
        a.suggest_done(Ok(card_verdict("Sam's photos")), snap(agent::now_ms())).unwrap();
        assert_eq!(cards(&a).len(), 1);
        let events: Vec<String> = log(&a).iter().map(|e| e["event"].as_str().unwrap().to_owned()).collect();
        assert_eq!(events, vec!["shown", "silent"], "{events:?}");
    }

    #[test]
    fn a_silent_answer_is_logged_and_shows_nothing() {
        let (_t, mut a) = app();
        a.suggest_done(Ok(sg::Verdict::Silent("quiet: small talk".into())), snap(agent::now_ms())).unwrap();
        assert!(cards(&a).is_empty());
        assert_eq!(log(&a)[0]["event"], "silent");
        assert_eq!(log(&a)[0]["reason"], "quiet: small talk");
    }

    #[test]
    fn not_now_dismisses_the_card_and_wakes_nothing() {
        let (_t, mut a) = app();
        a.suggest_done(Ok(card_verdict("CAD file")), snap(agent::now_ms())).unwrap();
        let id = cards(&a)[0].id.clone();
        a.suggest_answer(&id, &answer(&["Not now"], None)).unwrap();
        assert!(dismissals(&a).contains(&id), "the card leaves the island");
        let events: Vec<Value> = log(&a);
        assert_eq!(events.last().unwrap()["event"], "dismissed");
        // The agent was never asked: no notice was made (the agent is not running in this harness).
        assert!(!events.iter().any(|e| e["event"] == "error"));
    }

    /// Bug note 2026-11-05T02-53-27: the island's answer goes through main.rs's `ui_action_callback`, which sent every
    /// card to the agent's question path (ASK_NOT_FOUND), so no card was ever picked. Through the real callback, the
    /// answer now reaches the live loop's channel and the card's handler here.
    #[test]
    fn an_island_answer_reaches_the_card_through_the_apps_callback() {
        let (_t, mut a) = app();
        a.suggest_done(Ok(card_verdict("CAD file")), snap(agent::now_ms())).unwrap();
        let id = cards(&a)[0].id.clone();
        let (tx, rx) = crossbeam_channel::unbounded();
        let callback = crate::ui_action_callback(a.tools.clone(), tx, ui::Ui::handle());
        callback(ui::UiAction::AgentNotice(ui::agent_notice::AgentNoticeAction::Answer { request_id: id.clone(), answers: answer(&["Not now"], None), by_hand: vec![] }));
        let forwarded = rx.try_recv().expect("the app's own card goes to the live loop, not the agent");
        a.action(forwarded).unwrap();
        assert!(dismissals(&a).contains(&id), "the card leaves the island");
        assert_eq!(log(&a).last().unwrap()["event"], "dismissed");
    }

    #[test]
    fn a_pick_with_no_agent_running_is_refused_and_logged() {
        let (_t, mut a) = app();
        a.suggest_done(Ok(card_verdict("CAD file")), snap(agent::now_ms())).unwrap();
        let id = cards(&a)[0].id.clone();
        assert!(a.suggest_answer(&id, &answer(&["Find the file"], None)).is_err(), "the notice cannot reach the agent");
        let events = log(&a);
        assert_eq!(events[events.len() - 2]["event"], "picked");
        assert_eq!(events[events.len() - 2]["choice"], "Find the file");
        assert_eq!(events.last().unwrap()["event"], "error");
    }

    #[test]
    fn a_partial_pick_on_a_card_is_left_to_the_card() {
        let (_t, mut a) = app();
        a.suggest_done(Ok(card_verdict("CAD file")), snap(agent::now_ms())).unwrap();
        let id = cards(&a)[0].id.clone();
        let pick = ui::agent_notice::QuestionAnswer { question_id: "q1".into(), labels: vec!["Find the file".into()], other: None };
        // Not sent to the agent (none is running here) and not logged: the island keeps it until the card is complete.
        assert!(a.action(ui::UiAction::AgentNotice(ui::agent_notice::AgentNoticeAction::Pick { request_id: id.clone(), answer: pick })).is_ok());
        assert!(log(&a).iter().all(|e| e["event"] != "picked"));
        assert_eq!(cards(&a).len(), 1, "the card is still up");
    }

    #[test]
    fn an_answer_to_a_card_that_is_not_open_is_refused() {
        let (_t, mut a) = app();
        assert!(a.suggest_answer("suggest:nope", &answer(&["Find the file"], None)).is_err());
    }

    #[test]
    fn typed_bad_suggestion_after_a_card_is_feedback_and_closes_it() {
        let (_t, mut a) = app();
        a.suggest_done(Ok(card_verdict("CAD file")), snap(agent::now_ms())).unwrap();
        let id = cards(&a)[0].id.clone();
        assert!(a.suggest_feedback("that was a bad suggestion"));
        assert!(dismissals(&a).contains(&id), "the open card leaves the island");
        let events = log(&a);
        assert_eq!(events.last().unwrap()["event"], "feedback");
        assert_eq!(events.last().unwrap()["kind"], "bad suggestion");
        assert_eq!(events.last().unwrap()["id"], id.as_str());
    }

    #[test]
    fn typed_feedback_without_a_recent_card_is_not_consumed() {
        let (_t, mut a) = app();
        assert!(!a.suggest_feedback("irrelevant"), "no card yet");
        // Through the action path: a non-feedback line still reaches the agent (which is not running here: an error).
        assert!(a.action(ui::UiAction::AgentWindow(ui::AgentWindowAction::Send { text: "find the drawing".into(), steer: false })).is_err());
    }

    #[test]
    fn typed_feedback_on_a_card_reaches_no_agent_at_all() {
        let (_t, mut a) = app();
        a.suggest_done(Ok(card_verdict("CAD file")), snap(agent::now_ms())).unwrap();
        // Consumed in `action`: Ok, though the agent is not running.
        assert!(a.action(ui::UiAction::AgentWindow(ui::AgentWindowAction::Send { text: "irrelevant".into(), steer: false })).is_ok());
    }
}
