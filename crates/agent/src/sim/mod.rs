//! Harness simulator (lab/testing/TOOLING.md §2): the real `Bridge` (durable spool, wake/steer/queue policy, notice
//! pacing, start context, Stop, provider-error resolution) driven on a virtual clock against an in-process Pi
//! (`fake_pi`), a scripted T3 source and the user's scripted speech. Nothing sleeps, spawns or calls a provider; a
//! fifteen-minute session replays in milliseconds. `Sim::pump` is the supervisor's per-event step (handle the event,
//! add the start context when idle, flush what may go out), so the same order runs here and in the app.
//!
//! Every scenario checks the two global properties of lab/harness/SCENARIOS.md: G1 no the user utterance reaches the model
//! twice or is lost without a recorded reason, G2 no tool side effect happens twice; and no turn says more than one
//! final reply (the reply path, `reply_scenarios`).
pub mod fake_pi;
#[cfg(test)]
mod scenarios;
#[cfg(test)]
mod reply_scenarios;
#[cfg(test)]
mod night_scenarios;
#[cfg(test)]
mod recovery_scenarios;
// Night pass 3 (delivery): one wake per utterance; requests queued around a start are answered after it.
#[cfg(test)]
mod delivery_scenarios;
// pi-exit (21 Oct): a task's result wakes the agent after a start and survives a restart once.
#[cfg(test)]
mod result_scenarios;
// Day pass 26 Oct: the cache-refresh ping keeps the hour cache warm while idle, and stays silent.
#[cfg(test)]
mod cache_scenarios;
// Emergency mode (27 Oct): held input is context and a message for the cover agent, never a turn.
#[cfg(test)]
mod emergency_scenarios;

use crate::{
    clock::{Clock, SimClock},
    notice::{NoticeKind, StartContext},
    protocol::{Attachment, Observation},
    Bridge,
};
pub use fake_pi::{reply_speak, reply_text_only, Effect, FakePi, Input, InputKind, Step, World};
use std::sync::{Arc, Mutex};

/// 18 Oct 2026 17:39:00 CEST, the start of the evening replay.
pub const EVENING_MS: u64 = 1_792_337_940_000;
pub const S: u64 = 1_000;
pub const MIN: u64 = 60_000;

/// A T3 event the fake source reports, before the wake policy.
#[derive(Clone, Debug)]
pub enum T3Event {
    /// A run ended. `owned`: a worker Heimdall launched.
    Finished { thread: String, run: String, owned: bool, text: String },
    /// A thread waits for an answer (`requests`: the pending request ids) or an approval (empty `requests`).
    Waiting { thread: String, run: String, owned: bool, requests: Vec<String>, text: String },
}
/// The E7 policy as the oracle the simulator feeds the harness with (crates/t3 owns the production observer):
/// owned threads report every transition; other threads only questions and approvals; ids are deterministic.
pub fn t3_policy(event: &T3Event) -> Option<(String, String)> {
    match event {
        T3Event::Finished { owned: false, .. } => None,
        T3Event::Finished { thread, run, owned: true, text } => Some((format!("terminal:{thread}:{run}:completed"), text.clone())),
        T3Event::Waiting { thread, run, requests, text, .. } if !requests.is_empty() => {
            Some((format!("t3:{thread}:{run}:attention:{}", requests.join(",")), text.clone()))
        }
        T3Event::Waiting { thread, run, text, .. } => Some((format!("t3:{thread}:{run}:approval"), text.clone())),
    }
}

/// What reached the user (heard with Speaking on, read on the island with it off), in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Heard {
    pub at: u64,
    pub text: String,
    /// `true`: the turn's final reply; `false`: a `speak` call during the work.
    pub final_reply: bool,
}

/// One scenario's world: the virtual clock, the fake Pi, the bridge on a real SQLite spool (in memory).
pub struct Sim {
    pub clock: SimClock,
    pub world: Arc<Mutex<World>>,
    pub bridge: Bridge<FakePi>,
    rt: tokio::runtime::Runtime,
    start: Option<StartContext>,
    /// Lines the next start's catch-up takes from the archive (speech the spool never held), `(id, at, labelled)`.
    pub archive: Vec<(String, u64, String)>,
    /// Every the user utterance by id: (at, words, addressed).
    pub said: Vec<(String, u64, String, bool)>,
    pub threshold: u64,
    pub resolutions: Vec<crate::Resolution>,
    next_id: u64,
    /// When the process that is serving now started.
    pub started_at: u64,
    start_failed: Option<StartContext>,
    spool: std::path::PathBuf,
    _keeper: rusqlite::Connection,
    /// The reply path as the app runs it (`crate::reply`): `speak` lines and each settled turn's final text.
    pub replies: crate::reply::Replies,
    pub heard: Vec<Heard>,
    /// What the island's pull-down shows as the last reply.
    pub last_reply: Option<String>,
    /// The Agent toggle as the supervisor passes it to `Bridge::keep_cache_warm`.
    pub agent_on: bool,
}
impl Sim {
    /// The agent turned on at `at_ms` (a start).
    pub fn new(at_ms: u64) -> Self {
        let clock = SimClock::at(at_ms);
        let world = World::new(clock.clone());
        // The spool is real SQLite, held in memory (shared cache) so a scenario never waits on the disk; one keeper
        // connection keeps it alive across restarts, which reopen it exactly like the app reopens its file.
        let spool = std::path::PathBuf::from(format!("file:heimdall-sim-{}?mode=memory&cache=shared", uuid::Uuid::new_v4()));
        let keeper = rusqlite::Connection::open(&spool).unwrap();
        let bridge = Self::open_bridge(&world, &spool, &clock);
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let mut sim = Self { clock, world, bridge, rt, spool, _keeper: keeper, start: None, archive: vec![], said: vec![], threshold: 75_000, resolutions: vec![], next_id: 1, started_at: at_ms, start_failed: None,
            replies: Default::default(), heard: vec![], last_reply: None, agent_on: true };
        sim.begin_start(None);
        sim.pump();
        sim
    }
    pub fn now(&self) -> u64 {
        self.clock.now_ms()
    }
    fn open_bridge(world: &Arc<Mutex<World>>, path: &std::path::Path, clock: &SimClock) -> Bridge<FakePi> {
        Bridge::open_with_clock(FakePi { world: world.clone() }, path, Arc::new(clock.clone())).unwrap()
    }
    fn begin_start(&mut self, reason: Option<String>) {
        let now = self.now();
        self.bridge.begin_start(reason).unwrap();
        let mut ctx = StartContext::new(now, None);
        // As the app's `start_context`: after what the conversation holds, at most 30 minutes back.
        let window = self.bridge.catchup_since(now).unwrap();
        ctx.since_ms = Some(window);
        let archive: Vec<_> = self.archive.iter().filter(|(id, at, _)| *at >= window && *at < now && !self.bridge.has_observation(id).unwrap())
            .map(|(id, at, text)| crate::notice::CatchupLine { id: id.clone(), at_ms: Some(*at), text: text.clone(), observation: true }).collect();
        ctx.add_lines(archive);
        self.start = Some(ctx);
        self.started_at = now;
    }
    /// A fresh Pi process serves the same conversation: crash recovery, reload, model switch, relaunch (E4).
    pub fn restart(&mut self, reason: Option<&str>) {
        self.world.lock().unwrap().restart();
        // A turn the old process never settled has no reply; the new process starts with nothing said.
        self.replies.reset();
        let path = self.spool.clone();
        // The new bridge replaces the old one on the same spool (as `open` does in the app).
        self.bridge = Self::open_bridge(&self.world, &path, &self.clock);
        // As `open` does: Pi's session file decides every interrupted delivery before anything else.
        let evidence = self.world.lock().unwrap().evidence();
        self.bridge.recover_from(evidence).unwrap();
        self.resolutions.extend(self.bridge.take_resolutions());
        // A start context that could not be added is tried once more on the next start (as the app does).
        let carried = self.start_failed.take();
        self.begin_start(reason.map(str::to_owned));
        if let (Some(old), Some(new)) = (carried, self.start.as_mut()) {
            new.add_lines(old.lines);
            new.failed_on = Some(0);
        }
        self.pump();
    }
    fn id(&mut self) -> String {
        self.next_id += 1;
        format!("u{:04}", self.next_id)
    }
    fn observation(&mut self, speaker: &str, text: &str) -> Observation {
        Observation { id: self.id(), timestamp_ms: self.now(), source: "mic:primary".into(), speaker: speaker.into(), language: "en".into(), text: text.into() }
    }
    /// The user addresses Heimdall (decider `send`, name, follow-up window): the utterance and a wake.
    pub fn say(&mut self, text: &str) -> String {
        self.say_with(text, &[])
    }
    pub fn say_with(&mut self, text: &str, images: &[Attachment]) -> String {
        let o = self.observation("User", text);
        let id = o.id.clone();
        self.said.push((id.clone(), self.now(), text.into(), true));
        self.rt.block_on(self.bridge.observe(o)).unwrap();
        let result = self.rt.block_on(self.bridge.wake_keyed_through(None, "Act on User's latest request.", images, true, None, Some(&id), crate::Speaker::Owner));
        result.unwrap();
        self.pump();
        id
    }
    /// The decider sends someone else's line (call or system audio, 19 Oct: 12 such sends in 85 min): a wake, but not
    /// the user addressing Heimdall (`crate::Speaker::Other`).
    pub fn false_wake(&mut self, speaker: &str, text: &str) -> String {
        let mut o = self.observation(speaker, text);
        o.source = "Discord".into();
        let id = o.id.clone();
        self.rt.block_on(self.bridge.observe(o)).unwrap();
        self.rt.block_on(self.bridge.wake_keyed_through(None, "Act on User's latest request.", &[], false, None, Some(&id), crate::Speaker::Other)).unwrap();
        self.pump();
        id
    }
    /// Speech that is not for Heimdall (decider `not_for_heimdall`), or another speaker: context only.
    pub fn aside(&mut self, speaker: &str, text: &str) -> String {
        let o = self.observation(speaker, text);
        let id = o.id.clone();
        if speaker == "User" { self.said.push((id.clone(), self.now(), text.into(), false)); }
        self.rt.block_on(self.bridge.observe(o)).unwrap();
        self.pump();
        id
    }
    /// Speech heard while the agent was off: only the archive has it.
    pub fn heard_while_off(&mut self, at: u64, speaker: &str, text: &str) -> String {
        let id = self.id();
        self.archive.push((id.clone(), at, format!("[Mic · {speaker}] {text}")));
        id
    }
    pub fn typed(&mut self, text: &str) {
        self.rt.block_on(self.bridge.send_text(None, &format!("[Heimdall typed message]\n{text}"), true, None)).unwrap();
        self.pump();
    }
    /// A screenshot that arrived after its words went out (E9.1), as the app supervisor handles
    /// `Command::LateImages`: with a turn running it steers that turn as one prompt carrying the caption and the
    /// images; with none running it is held for the next wake (the supervisor's `held_late`) and starts nothing.
    /// Returns whether it steered.
    pub fn late_screenshot(&mut self, caption: &str, images: &[Attachment]) -> bool {
        let running = self.world.lock().unwrap().turn_running();
        if running {
            self.rt.block_on(self.bridge.steer(caption, images, None)).unwrap();
        }
        self.pump();
        running
    }
    /// An outbox notice with the kind its id and words give it.
    pub fn notice(&mut self, id: &str, text: &str) -> bool {
        let kind = NoticeKind::classify(id, text);
        let fresh = self.rt.block_on(self.bridge.notice(id, id, text, kind, None)).unwrap();
        self.pump();
        fresh
    }
    /// One T3 pass result through the E7 policy.
    pub fn t3(&mut self, event: T3Event) -> bool {
        match t3_policy(&event) {
            Some((id, text)) => self.notice(&id, &text),
            None => false,
        }
    }
    /// The user's Stop (window, island, or an approved spoken stop phrase): the harness stops, the model is not asked.
    pub fn stop(&mut self) {
        self.bridge.stop().unwrap();
        self.world.lock().unwrap().abort();
        self.pump();
        self.replies.reset();
    }
    /// Pi compacts on its own (threshold before a request) or idle compaction starts.
    pub fn compaction(&mut self) {
        self.world.lock().unwrap().start_compaction();
        self.pump();
    }
    /// The supervisor's step after any event: hand Pi's events to the bridge, add the start context once Pi is idle,
    /// flush what may go out now. Repeats until nothing changes at this instant.
    pub fn pump(&mut self) {
        // A crashed Pi is the supervisor's to replace (`restart`); nothing is sent to it meanwhile.
        if !self.world.lock().unwrap().alive { return; }
        loop {
            let mut progressed = false;
            self.world.lock().unwrap().run_due();
            loop {
                let event = self.world.lock().unwrap().pop_event();
                let Some(event) = event else { break };
                progressed = true;
                self.reply_event(&event);
                self.bridge.handle(&event).unwrap();
            }
            if let Some(mut ctx) = self.start.take() {
                let threshold = self.threshold;
                match self.rt.block_on(self.bridge.deliver_start_context(&mut ctx, |_| threshold)) {
                    Ok(true) => progressed = true,
                    Ok(false) => self.start = Some(ctx),
                    Err(_) => {
                        // Not added: retried once on the next start, then dropped; wakes stop waiting for it.
                        self.bridge.abandon_start_context();
                        if ctx.failed_on.is_none() { self.start_failed = Some(ctx); }
                        progressed = true;
                    }
                }
            }
            self.rt.block_on(self.bridge.flush_queued()).unwrap();
            // Heimdall's own compactions, while idle and quiet (`maintenance`), as the supervisor runs them.
            if self.rt.block_on(self.bridge.maintain()).unwrap().is_some() { progressed = true; }
            if self.rt.block_on(self.bridge.keep_cache_warm(self.agent_on)).unwrap().is_some() { progressed = true; }
            self.resolutions.extend(self.bridge.take_resolutions());
            let pending = self.world.lock().unwrap().has_events();
            if !progressed && !pending { break; }
        }
    }
    /// Advances virtual time, stopping at every moment something is due (Pi steps, compaction end, notice gap).
    pub fn advance(&mut self, ms: u64) {
        let target = self.now() + ms;
        loop {
            let due = [self.world.lock().unwrap().next_due(), self.bridge.notice_deadline(), self.bridge.retry_deadline()].into_iter().flatten().filter(|t| *t > self.now()).min();
            let next = due.map_or(target, |t| t.min(target));
            self.clock.set(next.max(self.now()));
            self.pump();
            if next >= target { break; }
        }
    }
    /// Runs until Pi is idle and nothing is waiting on a timer (bounded: one virtual hour).
    pub fn settle(&mut self) {
        for _ in 0..10_000 {
            let busy = { let w = self.world.lock().unwrap(); w.next_due().is_some() } || self.bridge.notice_deadline().is_some() || self.bridge.retry_deadline().is_some();
            if !busy { break; }
            self.advance(5 * S);
        }
    }

    /// The app's reply glue (bridge/supervisor → `Tools::agent_final_answer`, `mcp_actions::Host::speak`): a `speak`
    /// call is said at once; a settled turn's final text goes through `Replies::finish`.
    fn reply_event(&mut self, event: &crate::protocol::Event) {
        // A completed delivery: Pi's run settled, the next reply may repeat this run's words.
        if matches!(event, crate::protocol::Event::Complete { query: true, .. }) {
            self.replies.settled();
        }
        let crate::protocol::Event::Other(v) = event else { return };
        let now = self.now();
        match v["type"].as_str() {
            // Scenarios written before the reply path call `speak` without words: nothing to hear.
            Some("tool_execution_start") if v["toolName"] == "speak" && v["args"]["text"].as_str().is_some_and(|t| !t.trim().is_empty()) => {
                let text = v["args"]["text"].as_str().unwrap_or_default();
                self.replies.said(text);
                self.heard.push(Heard { at: now, text: text.into(), final_reply: false });
            }
            Some("heimdall_final_answer") => {
                let reply = self.replies.finish(v["text"].as_str().unwrap_or_default(), v["stop_reason"].as_str().unwrap_or_default());
                if let Some(say) = reply.say {
                    self.heard.push(Heard { at: now, text: say, final_reply: true });
                }
                if reply.shown.is_some() {
                    self.last_reply = reply.shown;
                }
            }
            _ => {}
        }
    }
    /// The words the user got, in order.
    pub fn heard_texts(&self) -> Vec<String> {
        self.heard.iter().map(|h| h.text.clone()).collect()
    }

    // ---- what Pi received ----
    pub fn inputs(&self) -> Vec<Input> {
        self.world.lock().unwrap().inputs.clone()
    }
    pub fn turns(&self) -> Vec<(u64, String, bool)> {
        self.world.lock().unwrap().turns.clone()
    }
    pub fn effects(&self) -> Vec<Effect> {
        self.world.lock().unwrap().effects.clone()
    }
    pub fn appends(&self) -> Vec<Input> {
        self.inputs().into_iter().filter(|i| i.kind == InputKind::Append).collect()
    }
    /// Inputs that start or join a turn (prompts and steers).
    pub fn wakes(&self) -> Vec<Input> {
        self.inputs().into_iter().filter(|i| i.kind != InputKind::Append).collect()
    }
    pub fn turns_between(&self, from: u64, to: u64) -> usize {
        self.turns().iter().filter(|t| t.0 >= from && t.0 < to).count()
    }
    pub fn dropped(&self, id: &str) -> Option<String> {
        self.bridge.db.query_row("SELECT dropped FROM observations WHERE id=?1", [id], |r| r.get::<_, Option<String>>(0)).ok().flatten()
    }

    /// G1 and G2 (SCENARIOS.md): every the user utterance reached the model at most once (a marked resend of a delivery
    /// that failed before any tool ran is not a second copy) and is never lost without a reason; every tool call ran
    /// once and no tool-bearing delivery was prompted again.
    pub fn check_global(&self) {
        let inputs = self.inputs();
        let failed_untouched: std::collections::HashSet<String> = self.resolutions.iter()
            .filter(|r| matches!(r.outcome, crate::Resolved::Retrying | crate::Resolved::Failed | crate::Resolved::Rejected))
            .map(|r| r.id.clone()).collect();
        for (id, _, words, _) in &self.said {
            // The utterance as a transcript line: `[HH:MM:SS Mic · User] words` at the end of a line.
            let line = format!("User] {words}\n");
            let carries = |i: &&Input| i.text.contains(&line) || i.text.ends_with(&line[..line.len() - 1]);
            let copies = inputs.iter().filter(carries)
                .filter(|i| !i.id.as_ref().is_some_and(|d| failed_untouched.contains(d)) || i.text.contains("(resent after a delivery interruption"))
                .count();
            let resent = inputs.iter().filter(carries).filter(|i| i.text.contains("(resent after a delivery interruption")).count();
            assert!(copies <= 1 + resent, "G1: {words:?} reached the model {copies} times:\n{:#?}", inputs.iter().filter(carries).collect::<Vec<_>>());
            let pending: bool = self.bridge.db.query_row("SELECT settled=0 FROM observations WHERE id=?1", [id], |r| r.get(0)).unwrap_or(false);
            let copies = inputs.iter().filter(carries).count();
            assert!(copies > 0 || pending || self.dropped(id).is_some() || self.stopped_context(id),
                "G1: {words:?} was lost without a reason");
        }
        let effects = self.effects();
        let mut calls = std::collections::HashSet::new();
        for e in &effects {
            assert!(calls.insert(e.call.clone()), "G2: tool call {} ran twice", e.call);
        }
        for e in &effects {
            let again = inputs.iter().filter(|i| i.id.as_deref() == Some(e.delivery.as_str()) && i.kind != InputKind::Append && i.at > e.at).count();
            assert_eq!(again, 0, "G2: delivery {} was delivered again after its tool effect", e.delivery);
        }
        // The reply path (crate::reply): a model turn says at most one final reply. A run has one model turn per
        // input it took in (its prompt, then each steer that arrived after the last tool), so finals never outnumber
        // prompts and steers.
        let finals = self.heard.iter().filter(|h| h.final_reply).count();
        let wakes = self.wakes().len();
        assert!(finals <= wakes, "more final replies than inputs: {finals} finals for {wakes} prompts and steers");
    }
    fn stopped_context(&self, id: &str) -> bool {
        // An utterance whose wake a Stop cancelled stays pending context for the next wake (E3.6).
        self.bridge.db.query_row("SELECT settled=0 FROM observations WHERE id=?1", [id], |r| r.get::<_, bool>(0)).unwrap_or(false)
    }
}
