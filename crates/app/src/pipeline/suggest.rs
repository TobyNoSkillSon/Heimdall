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
}

/// What the pass is told when the user asked for a card himself.
const FORCED: &str = "\n\nOwner pressed the suggestion key: he wants one card now from this conversation. Show the most useful card you can; stay silent only if the transcript is empty.";

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
    pub(super) fn suggest_heard(&mut self, speaker: &str, text: &str, owner: bool) {
        if text.trim().is_empty() {
            return;
        }
        let now = agent::now_ms();
        self.suggest.window.push(sg::Line { at_ms: now, speaker: speaker.into(), text: text.into(), owner });
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
        let owner = self.owner(source) && line.speaker == "User";
        let outcome = record["outcome"].as_str().unwrap_or("");
        if owner && (outcome == "send" || record["choice"] == "wait" || record["choice"] == "send") {
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
            let fires = yes.is_some_and(|y| y >= SUGGEST_MIN);
            evidence["speaker"] = json!(speaker);
            evidence["threshold"] = json!(SUGGEST_MIN);
            evidence["outcome"] = json!(if !fires { "no" } else if p.suggest_addressing(now) { "held_addressing" } else { "pass" });
            p.index.evidence.event("suggest_decision", "heimdall", &ids, capture::host_now_ns(), evidence)?;
            if fires && !p.suggest_addressing(now) {
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
        self.suggest_start(now, false);
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
        self.suggest_start(agent::now_ms(), true);
    }
    fn suggest_start(&mut self, now: u64, forced: bool) {
        // The endpoint is resolved on the pass's own thread: a ChatGPT sign-in may wait on the Keychain or a token
        // refresh, and this is the pipeline's thread (review 27 Oct).
        self.suggest.pacer.called(now);
        let known: Vec<String> = EnrollmentStore::load(&self.data.join("speakers.json")).map(|s| s.names.keys().cloned().collect()).unwrap_or_default();
        let mut prompt = sg::prompt(&self.suggest.window, now, &known);
        if forced { prompt.push(FORCED); }
        let snap = Snapshot { now, excerpt: self.suggest.window.transcript(), during_call: self.suggest_call_open(), forced };
        let paths = self.suggest_paths();
        let running = self.suggest.running.clone();
        let tx = self.internal.0.clone();
        running.store(true, Ordering::Release);
        let guard = Running(running);
        std::thread::spawn(move || {
            let _guard = guard;
            // Its own model (Background Tasks ▸ Suggestions), else the shared one; a fallback while that one is spent.
            let endpoint = crate::background_model::task_endpoint(crate::background_model::Task::Suggestions);
            if endpoint.key.is_none() {
                if forced { eprintln!("hotkey: suggest pressed, but no background model is signed in; no card"); }
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
        match result {
            Ok(sg::Verdict::Silent(reason)) => {
                self.log_suggest(json!({"event": "silent", "at": snap.now, "reason": reason, "transcript": snap.excerpt}));
            }
            Ok(sg::Verdict::Card(s)) => {
                let now = agent::now_ms();
                if !snap.forced && self.suggest_addressing(now) {
                    // He started talking to Heimdall while the pass ran: no card over that exchange.
                    self.log_suggest(json!({"event": "silent", "at": now, "reason": "talking to Heimdall", "topic": s.topic}));
                    return Ok(());
                }
                if self.suggest.card.is_some() || (!snap.forced && !self.suggest.pacer.may_show(now, &s.topic)) {
                    self.log_suggest(json!({"event": "silent", "at": now, "reason": "pacing: a card is up, the gap, a pause, or a repeat topic", "topic": s.topic}));
                    return Ok(());
                }
                // Expiry is `suggest_tick`'s; the card's request id is `CARD_PREFIX` and a fresh uuid.
                let id = format!("{}{}", sg::CARD_PREFIX, uuid::Uuid::new_v4());
                let expires_ms = now + sg::CARD_MS;
                self.ui(ui::UiUpdate::AgentNotice(sg::card(&id, &s, expires_ms)));
                self.suggest.pacer.shown(now, &s.topic);
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
        Snapshot { now, excerpt: "[10:00 Sam] the CAD file from the laptop".into(), during_call: false, forced: false }
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
