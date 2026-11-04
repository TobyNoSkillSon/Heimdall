//! An in-process Pi on the virtual clock: it accepts prompts, steers at tool boundaries, runs scripted tool calls
//! with recorded side effects, appends context without a turn, compacts, fails with scripted provider errors and
//! settles — the events real Pi reports for each (EXPECTED.md terms: turn, wake, append, settled).
use crate::{
    clock::{Clock, SimClock},
    harness::{Harness, InputUnavailable},
    protocol::Event,
};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InputKind {
    /// Started a turn.
    Prompt,
    /// Entered a running turn at its next tool boundary.
    Steer,
    /// Context without a turn (`heimdall_context`).
    Append,
}
/// Everything Pi received, with the virtual time and the process generation it reached.
#[derive(Clone, Debug)]
pub struct Input {
    pub at: u64,
    pub kind: InputKind,
    pub id: Option<String>,
    pub text: String,
    pub images: usize,
    pub observation_ids: Vec<String>,
    pub generation: u32,
}
/// One tool call's side effect (speech played, island notice, shell command…), keyed by delivery and call id.
#[derive(Clone, Debug)]
pub struct Effect {
    pub at: u64,
    pub delivery: String,
    pub call: String,
    pub tool: String,
    pub generation: u32,
}
/// A scripted reply step, in virtual time.
#[derive(Clone, Debug)]
pub enum Step {
    /// The model thinks for this long (nothing written yet: the user's new words may restart the turn).
    Think(u64),
    /// The model writes its reply or a tool call's arguments for this long: from the first token the turn has acted
    /// (night review 4), so new words steer it.
    Stream(u64),
    /// A tool call: a tool boundary (steers enter first), then the effect, then this long running.
    Tool(&'static str, u64),
    /// The request fails with this provider error text after the steps before it (no retry inside Pi).
    Fail(String),
    /// A `speak` call with these words (a tool boundary like `Tool`), then this long running.
    Speak(&'static str, u64),
    /// The turn's final text so far (the last one wins): what settles as `heimdall_final_answer`. Without one the
    /// turn ends with "done".
    Final(&'static str),
    /// The running agent's history repair acts (`runtime/history-repair.mjs`): Pi announces its entry with this
    /// state ("repaired", "restarted_from_summary", "gave_up"), and the run goes on.
    Repaired(&'static str),
}
pub fn reply_speak() -> Vec<Step> {
    vec![Step::Think(2_000), Step::Tool("speak", 1_000)]
}
pub fn reply_text_only() -> Vec<Step> {
    vec![Step::Think(1_500)]
}

#[derive(Debug)]
struct Turn {
    deliveries: Vec<String>,
    owner: String,
    steps: VecDeque<Step>,
    next_at: u64,
    steers: Vec<String>,
    tools: usize,
    /// A reply or tool call started streaming (`Step::Stream`).
    streamed: bool,
    /// The final assistant text written so far (`Step::Final`).
    text: Option<String>,
}

#[derive(Debug)]
pub struct World {
    pub clock: SimClock,
    pub inputs: Vec<Input>,
    pub effects: Vec<Effect>,
    pub generation: u32,
    pub alive: bool,
    /// Replies for the next turns, in order; `default_reply` after they run out.
    pub replies: VecDeque<Vec<Step>>,
    pub default_reply: Vec<Step>,
    /// What a steer that arrives after the last tool adds before settling (one more model call).
    pub steer_reply: Vec<Step>,
    pub context_tokens: Option<u64>,
    pub window: u64,
    pub compaction_ms: u64,
    pub compactions: Vec<(u64, u64)>,
    compacting_until: Option<u64>,
    turn: Option<Turn>,
    events: VecDeque<Event>,
    next_call: u64,
    /// Turns started and how each ended (delivery owner, error).
    pub turns: Vec<(u64, String, bool)>,
    /// Pi's session file entries: the extension's effects records (written before each tool runs) and completion
    /// receipts, which decide interrupted deliveries after a crash (`Bridge::recover_receipts`). Kept in memory; the
    /// runner writes them to a file at each restart.
    pub session: Vec<String>,
    /// The next context append fails this many times (a start that cannot add its context).
    pub failing_appends: u32,
    /// Prompts refused as `InputUnavailable` (Pi could not take input at that moment; nothing was written).
    pub unavailable_prompts: u32,
    /// When the last request that carried the history started (`maintenance::LastRequest`); `None` after a
    /// compaction. `same_model`: it ran on the model that runs now.
    pub last_request_at: Option<u64>,
    pub same_model: bool,
    /// The running compaction is Heimdall's idle one (`compact_history`).
    idle: bool,
    /// The user's Stop came while the turn compacted: the turn stops as the compaction ends.
    stop_after_compaction: bool,
    /// Compactions started; each one ends in `compactions` (nothing cuts one short).
    pub compactions_started: usize,
    /// Turns ended by `rejoin` (their deliveries).
    pub rejoined: Vec<Vec<String>>,
    /// Restarts since a turn last ended any other way, and the most seen in a row (`crate::MAX_RESTARTS`).
    restarts_in_row: usize,
    pub most_restarts_in_row: usize,
    /// When the conversation's record ends: the last input it took or message it wrote (`Harness::history_end_ms`).
    pub history_end: Option<u64>,
}
impl World {
    pub fn new(clock: SimClock) -> Arc<Mutex<Self>> {
        Arc::new(Mutex::new(Self {
            clock,
            inputs: vec![],
            effects: vec![],
            generation: 1,
            alive: true,
            replies: VecDeque::new(),
            default_reply: reply_speak(),
            steer_reply: reply_speak(),
            context_tokens: Some(10_000),
            window: 100_000,
            compaction_ms: 18_000,
            compactions: vec![],
            compacting_until: None,
            turn: None,
            events: VecDeque::new(),
            next_call: 1,
            turns: vec![],
            session: Vec::new(),
            failing_appends: 0,
            unavailable_prompts: 0,
            last_request_at: None,
            same_model: true,
            idle: false,
            stop_after_compaction: false,
            compactions_started: 0,
            rejoined: vec![],
            restarts_in_row: 0,
            most_restarts_in_row: 0,
            history_end: None,
        }))
    }
    fn persist(&mut self, entry: Value) {
        self.session.push(entry.to_string());
    }
    /// What Pi's session file says about Heimdall's deliveries (user messages, receipts and effects records), read
    /// by the app's own reader.
    pub fn evidence(&self) -> crate::session::Evidence {
        let mut out = crate::session::Evidence::default();
        for line in &self.session {
            out.read(line);
        }
        out
    }
    /// Pi reads a delivery: its user message goes into the session file, then the harness hears of it. (Fields, not
    /// `self`: the running turn is borrowed while steers enter it.)
    fn took_in(session: &mut Vec<String>, events: &mut VecDeque<Event>, id: &str) {
        session.push(json!({"type":"message","id":format!("m-{id}"),"message":{"role":"user","content":[{"type":"text","text":format!("[Heimdall delivery: {id}]\n…")}]}}).to_string());
        events.push_back(Event::Consumed { id: id.to_owned() });
    }
    fn now(&self) -> u64 {
        self.clock.now_ms()
    }
    pub fn turn_running(&self) -> bool {
        self.turn.is_some()
    }
    pub fn compacting(&self) -> bool {
        self.compacting_until.is_some()
    }
    /// The next virtual time something happens inside Pi.
    pub fn next_due(&self) -> Option<u64> {
        [self.turn.as_ref().map(|t| t.next_at), self.compacting_until].into_iter().flatten().min()
    }
    pub fn has_events(&self) -> bool {
        !self.events.is_empty()
    }
    pub fn pop_event(&mut self) -> Option<Event> {
        self.events.pop_front()
    }
    /// A new process: the old one's turn and queues are gone (crash, reload, model switch).
    pub fn restart(&mut self) {
        self.generation += 1;
        self.alive = true;
        self.turn = None;
        self.compacting_until = None;
        self.events.clear();
    }
    pub fn crash(&mut self) {
        self.alive = false;
        self.turn = None;
    }
    /// The user's Stop: Pi clears its queues and aborts; every delivery of the turn ends as aborted. Like real Pi, the
    /// aborted reply settles with whatever text it had and `stop_reason` "aborted".
    /// A compaction is never cut: a turn compacting inside its run stops as the compaction ends.
    pub fn abort(&mut self) {
        if self.compacting() {
            self.stop_after_compaction = self.turn.is_some();
            return;
        }
        self.restarts_in_row = 0;
        if let Some(turn) = self.turn.take() {
            self.events.push_back(Event::Other(json!({"type":"heimdall_final_answer","text":turn.text.clone().unwrap_or_default(),"stop_reason":"aborted"})));
            for id in turn.deliveries.iter().chain(turn.steers.iter()) {
                self.events.push_back(Event::Other(json!({"type":"heimdall_aborted","id":id,"reason":"Stopped by User"})));
                self.events.push_back(Event::Complete { id: Some(id.clone()), query: true, error: true, text: "Stopped by User".into() });
            }
            self.turns.last_mut().map(|t| t.2 = true);
        }
    }
    /// Pi's own compaction (its threshold, inside a turn): nothing may cancel it.
    pub fn start_compaction(&mut self) {
        let now = self.now();
        self.compacting_until = Some(now + self.compaction_ms);
        self.compactions_started += 1;
        self.idle = false;
        self.events.push_back(Event::Other(json!({"type":"compaction_start"})));
    }
    /// Heimdall's idle compaction (`compact_history`): nothing cancels it either.
    pub fn start_idle_compaction(&mut self) {
        self.start_compaction();
        self.idle = true;
    }
    pub fn idle_compacting(&self) -> bool {
        self.compacting() && self.idle
    }
    /// The running turn has neither acted (no tool, nothing written) nor taken a steer: the user's next words may
    /// restart it.
    pub fn can_rejoin(&self) -> bool {
        self.turn.as_ref().is_some_and(|t| t.tools == 0 && !t.streamed && t.steers.is_empty())
    }
    /// Pi's `abort` for `Harness::rejoin`: the turn ends without an answer, its deliveries complete (their words stay
    /// in the conversation) and the next prompt starts a fresh turn. Like the real harness, it checks again first
    /// and leaves a turn that has acted alone. Returns whether it ended the turn.
    pub fn rejoin(&mut self) -> bool {
        if !self.can_rejoin() { return false; }
        let Some(turn) = self.turn.take() else { return false };
        self.events.push_back(Event::Other(json!({"type":"heimdall_final_answer","text":"","stop_reason":"aborted"})));
        for id in &turn.deliveries {
            self.events.push_back(Event::Complete { id: Some(id.clone()), query: true, error: false, text: String::new() });
        }
        self.events.push_back(Event::Other(json!({"type":"agent_settled","aborted":true})));
        self.rejoined.push(turn.deliveries);
        self.restarts_in_row += 1;
        self.most_restarts_in_row = self.most_restarts_in_row.max(self.restarts_in_row);
        true
    }
    fn record(&mut self, kind: InputKind, wire: Option<&Value>, text: &str) {
        let input = Input {
            at: self.now(),
            kind,
            id: wire.and_then(|w| w["uuid"].as_str()).map(str::to_owned),
            text: text.to_owned(),
            images: wire.map_or(0, |w| w["message"]["content"].as_array().map_or(0, |c| c.iter().filter(|b| b["type"] == "image").count())),
            observation_ids: wire.map(crate::protocol::observation_ids).unwrap_or_default(),
            generation: self.generation,
        };
        self.history_end = Some(input.at);
        self.inputs.push(input);
    }
    fn prompt(&mut self, wire: Value) -> Result<()> {
        anyhow::ensure!(self.alive, "Pi exited");
        anyhow::ensure!(wire["shouldQuery"] == true, "Pi observations stay in the durable local spool");
        if self.unavailable_prompts > 0 {
            self.unavailable_prompts -= 1;
            return Err(InputUnavailable("Pi is compacting the session").into());
        }
        let id = wire["uuid"].as_str().unwrap_or_default().to_owned();
        let text = wire["message"]["content"].as_array().into_iter().flatten().filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n");
        let now = self.now();
        self.events.push_back(Event::Echo { id: id.clone() });
        match &mut self.turn {
            Some(turn) => {
                turn.steers.push(id);
                self.record(InputKind::Steer, Some(&wire), &text);
            }
            None => {
                let steps: VecDeque<Step> = self.replies.pop_front().unwrap_or_else(|| self.default_reply.clone()).into();
                Self::took_in(&mut self.session, &mut self.events, &id);
                self.turn = Some(Turn { deliveries: vec![id.clone()], owner: id.clone(), steps, next_at: now, steers: vec![], tools: 0, streamed: false, text: None });
                self.turns.push((now, id, false));
                self.last_request_at = Some(now);
                self.same_model = true;
                self.record(InputKind::Prompt, Some(&wire), &text);
            }
        }
        Ok(())
    }
    /// Runs everything due at the current virtual time.
    pub fn run_due(&mut self) {
        let now = self.now();
        if self.compacting_until.is_some_and(|t| t <= now) {
            self.compacting_until = None;
            self.compactions.push((now - self.compaction_ms, now));
            self.context_tokens = None;
            self.last_request_at = None;
            self.idle = false;
            self.events.push_back(Event::Other(json!({"type":"compaction_end","result":{}})));
            if std::mem::take(&mut self.stop_after_compaction) { self.abort(); }
        }
        while let Some(turn) = self.turn.as_mut().filter(|t| t.next_at <= now) {
            let step = turn.steps.pop_front();
            // A spoken line is a tool call whose arguments carry the words.
            let (step, words) = match step {
                Some(Step::Speak(words, ms)) => (Some(Step::Tool("speak", ms)), Some(words)),
                other => (other, None),
            };
            match step {
                Some(Step::Think(ms)) => {
                    // Each model call writes the provider's cache for the history it carried.
                    self.last_request_at = Some(now);
                    turn.next_at += ms
                }
                Some(Step::Stream(ms)) => {
                    turn.streamed = true;
                    turn.next_at += ms
                }
                Some(Step::Final(text)) => turn.text = Some(text.into()),
                Some(Step::Repaired(state)) => self.events.push_back(Event::Other(json!({"type":"heimdall_history_repair","state":state,"line":format!("lab repair: {state}")}))),
                Some(Step::Speak(..)) => unreachable!("mapped to a tool call above"),
                Some(Step::Tool(name, ms)) => {
                    // A tool boundary: steered input enters here, in order, once (E2.1).
                    for id in std::mem::take(&mut turn.steers) {
                        turn.owner = id.clone();
                        turn.deliveries.push(id.clone());
                        Self::took_in(&mut self.session, &mut self.events, &id);
                    }
                    let call = format!("call-{}", self.next_call);
                    self.next_call += 1;
                    self.history_end = Some(now);
                    turn.tools += 1;
                    let owner = turn.owner.clone();
                    turn.next_at += ms;
                    self.persist(json!({"type":"custom","customType":"heimdall_effects","data":{"ids":[owner],"toolCallId":call}}));
                    let args = words.map_or(json!({}), |w| json!({"text":w}));
                    self.events.push_back(Event::Other(json!({"type":"tool_execution_start","toolCallId":call,"toolName":name,"args":args,"delivery_id":owner})));
                    self.effects.push(Effect { at: now, delivery: owner, call, tool: name.into(), generation: self.generation });
                }
                Some(Step::Fail(text)) => {
                    self.restarts_in_row = 0;
                    self.history_end = Some(now);
                    let turn = self.turn.take().unwrap();
                    self.events.push_back(Event::Other(json!({"type":"heimdall_final_answer","text":turn.text.clone().unwrap_or_default(),"stop_reason":"error"})));
                    // Like the extension: a receipt for every delivery the run read, before it settles.
                    for id in &turn.deliveries {
                        self.persist(json!({"type":"custom","customType":"heimdall_receipt","data":{"id":id,"completed":false,"effects":turn.tools>0}}));
                    }
                    for id in turn.deliveries.iter().chain(turn.steers.iter()) {
                        self.events.push_back(Event::Complete { id: Some(id.clone()), query: true, error: true, text: text.clone() });
                    }
                    self.events.push_back(Event::Other(json!({"type":"agent_settled","error":text})));
                    self.turns.last_mut().map(|t| t.2 = true);
                }
                None if !turn.steers.is_empty() => {
                    // A steer after the last tool gets one more model call before the turn settles. Like real Pi, the
                    // reply written so far ended its own model turn (`turn_end`, stop) and is reported then: the
                    // harness says each finished reply once, so a steer never swallows it.
                    if let Some(text) = turn.text.take() {
                        self.events.push_back(Event::Other(json!({"type":"heimdall_final_answer","text":text,"stop_reason":"stop"})));
                    }
                    let extra = self.steer_reply.clone();
                    turn.steps.extend(extra);
                    for id in std::mem::take(&mut turn.steers) {
                        turn.owner = id.clone();
                        turn.deliveries.push(id.clone());
                        Self::took_in(&mut self.session, &mut self.events, &id);
                    }
                }
                None => {
                    self.restarts_in_row = 0;
                    self.history_end = Some(now);
                    let turn = self.turn.take().unwrap();
                    let text = turn.text.clone().unwrap_or_else(|| "done".into());
                    self.events.push_back(Event::Other(json!({"type":"heimdall_final_answer","text":text,"stop_reason":"stop"})));
                    for id in &turn.deliveries {
                        self.persist(json!({"type":"custom","customType":"heimdall_receipt","data":{"id":id,"completed":true,"effects":turn.tools>0}}));
                        self.events.push_back(Event::Complete { id: Some(id.clone()), query: true, error: false, text: "done".into() });
                    }
                    self.events.push_back(Event::Other(json!({"type":"agent_settled"})));
                    self.context_tokens = self.context_tokens.map(|t| t + 2_000);
                }
            }
        }
    }
}

/// The `Harness` the bridge talks to; every call lands in the shared `World`.
pub struct FakePi {
    pub world: Arc<Mutex<World>>,
}
#[async_trait]
impl Harness for FakePi {
    fn serial_wakes(&self) -> bool {
        true
    }
    fn supports_steering(&self) -> bool {
        true
    }
    fn input_blocked(&self) -> Option<&'static str> {
        self.world.lock().unwrap().compacting().then_some("Pi is compacting the session")
    }
    fn turn_active(&self) -> bool {
        let w = self.world.lock().unwrap();
        w.turn_running() || w.compacting()
    }
    async fn send(&mut self, wire: Value) -> Result<()> {
        self.world.lock().unwrap().prompt(wire)
    }
    async fn send_with_permit(&mut self, wire: Value, permit: crate::delivery::DeliveryPermit) -> Result<()> {
        permit.check()?;
        self.world.lock().unwrap().prompt(wire)
    }
    async fn next(&mut self) -> Result<Option<Event>> {
        Ok(self.world.lock().unwrap().pop_event())
    }
    async fn shutdown(&mut self) -> Result<()> {
        self.world.lock().unwrap().alive = false;
        Ok(())
    }
    async fn append_context(&mut self, text: &str) -> Result<()> {
        let mut w = self.world.lock().unwrap();
        anyhow::ensure!(w.alive, "Pi exited");
        if w.turn_running() || w.compacting() {
            return Err(InputUnavailable("Context waits for the turn to settle").into());
        }
        if w.failing_appends > 0 {
            w.failing_appends -= 1;
            anyhow::bail!("Pi control request stalled");
        }
        w.record(InputKind::Append, None, text);
        Ok(())
    }
    async fn context_tokens(&mut self) -> Result<Option<(Option<u64>, u64)>> {
        let w = self.world.lock().unwrap();
        Ok(Some((w.context_tokens, w.window)))
    }
    async fn compact_history(&mut self) -> Result<()> {
        self.world.lock().unwrap().start_idle_compaction();
        Ok(())
    }
    fn can_rejoin(&self) -> bool {
        self.world.lock().unwrap().can_rejoin()
    }
    async fn rejoin(&mut self) -> Result<bool> {
        Ok(self.world.lock().unwrap().rejoin())
    }
    fn history_end_ms(&self) -> Option<u64> {
        self.world.lock().unwrap().history_end
    }
    fn last_request(&self) -> Option<crate::maintenance::LastRequest> {
        let w = self.world.lock().unwrap();
        w.last_request_at.map(|at_ms| crate::maintenance::LastRequest { at_ms, same_model: w.same_model })
    }
}
