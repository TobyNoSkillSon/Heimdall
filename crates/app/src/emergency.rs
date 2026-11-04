//! The relay's controller (docs/claude-relay.md, docs/emergency.md). Heimdall's agent can be a T3 thread: the user chooses
//! it (relay mode, `Reason::Chosen`), or the supervisor switches it on when Pi's agent fails (emergency mode,
//! `Reason::Emergency`, detected by `agent::emergency::Detector`). Either way the Bridge holds Pi's turns
//! (`Bridge::hold_turns`) and this starts the thread through T3, relays every held input to it, follows each run (the
//! island's working state, the final reply spoken like Pi's), serves every main tool to it through a scoped MCP server
//! and token, and ends it. Emergency mode also probes the main agent and hands back, and offers the local engine when
//! no relay answers.
//!
//! Phases: `Off` → `Starting` (finding a harness that answers) → `Covering`; `NoCover` when none answered (launch
//! retried every 10 min); `Trial` when a probe passed: the token is revoked, the hold released and the agent's next
//! turn carries the "while you were down" summary. That turn answering ends it (the user hears "I'm back"); failing puts
//! the same thread back on duty with a new token. `Local`: the agent runs on the local engine the offer switched to,
//! and the main model is probed until it can take over again. The state survives a restart
//! (`<data>/emergency/state.json`).
use agent::emergency::{self as em, Cause, Harness, Reason, Relayed};
use agent::routing::{self, Quotas};
use crate::emergency_recovery::{self as rec, Original, Pick, Route};
use agent::mcp::{Allow, McpServer, Scope, ScopedTools, ToolHost, TranscriptSearch};
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::{
    collections::{HashSet, VecDeque},
    path::{Path, PathBuf},
    sync::{atomic::{AtomicU64, AtomicUsize, Ordering}, Arc, Mutex},
    time::Duration,
};
use tokio::sync::{mpsc, oneshot};

/// T3 as the controller uses it: `t3::Client` in the app (`T3Cover`), a fake in tests.
#[async_trait]
pub trait CoverT3: Send + Sync {
    async fn catalog(&self) -> Result<Value>;
    /// A full-access thread at the Vault's root with `message` as its first message: thread and run ids.
    async fn launch(&self, title: &str, message: &str, harness: &Harness) -> Result<(String, Option<String>)>;
    /// A message into the thread: the run it started or joined.
    async fn send(&self, thread: &str, message: &str) -> Result<Option<String>>;
    /// The run after at most `wait`.
    async fn outcome(&self, thread: &str, run: Option<&str>, wait: Duration) -> Result<Run>;
    /// The thread's latest assistant message.
    async fn latest_reply(&self, thread: &str) -> Result<Option<String>>;
    async fn archive(&self, thread: &str) -> Result<()>;
    /// The user's Stop: the running run ends now.
    async fn interrupt(&self, thread: &str) -> Result<()>;
    /// The thread's timeline items after `after`, oldest first, each with its whole text (`t3_thread_read`'s activity
    /// view, paged and continued), at most `MIRROR_PAGES` pages a call.
    async fn activity(&self, thread: &str, after: u64) -> Result<Vec<Item>>;
    /// Each account's live quota by provider instance (`agent::accounts`); empty when nothing could be read.
    async fn quotas(&self) -> Quotas { Quotas::new() }
}
/// One item of a relay thread's timeline, as the mirror reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item { pub position: u64, pub kind: String, pub text: String, pub status: String }
/// Pages the mirror reads per pass (100 items each); a longer backlog continues on the next pass.
const MIRROR_PAGES: usize = 20;
/// One mirrored message is at most this long per context entry; a longer one goes in as consecutive parts, whole.
const MIRROR_PART: usize = 64 * 1024;
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Run {
    /// Still running when the wait ended: it answers.
    Working,
    /// Ended with this reply.
    Completed(Option<String>),
    /// Failed, interrupted, or ended with nothing but a provider error.
    Failed(String),
}
/// Is the agent answering again? One tiny request or a sign-in check, never a turn (`ProviderProbe`).
#[async_trait]
pub trait MainProbe: Send + Sync {
    async fn probe(&self, model: Option<&agent::provider::ApiModel>) -> std::result::Result<(), String>;
}
/// The app around the controller: the island, Heimdall's voice, his conversation and the supervisor.
pub trait Side: Send + Sync {
    fn cue(&self, cue: ui::HealthCue);
    /// The notch indicator and the menu's Background Tasks pause.
    fn indicator(&self, state: ui::RelayIndicator);
    /// One line in Heimdall's voice.
    fn say(&self, text: &str);
    /// Context in the agent's conversation, no turn (an `emergency-cover:` outbox notice).
    fn record(&self, id: &str, text: &str);
    /// End the hold; with `back`, the agent's next turn starts with it (a hand-back), without, nothing starts (Agent off).
    fn release(&self, back: Option<String>);
    /// Input that reached the relay as the relay ended: nobody covers it, so it wakes the agent.
    fn late(&self, r: &Relayed);
    /// Input the relay had not finished when the user switched back to the agent: it wakes the agent, which finishes it.
    fn unfinished(&self, r: &Relayed) { self.late(r) }
    /// The conversation (oldest first, bounded to `bytes`), the plan, and the T3 work Heimdall follows.
    fn context(&self, bytes: usize) -> (String, String, String);
    /// The user's words went to the relay: the island's send arrow.
    fn sent(&self);
    /// A relay run works (true) or every one has ended (false): the island's working state.
    fn working(&self, on: bool);
    /// A relay run's final reply: spoken and shown like Pi's (`Tools::agent_final_answer`).
    fn reply(&self, text: &str);
    /// A Heimdall tool call the relay made, with its result: the pull-down's rows, as the same call of Pi's makes them
    /// (T3 reports no tool events; bug note 30 Oct 03:00).
    fn tool_call(&self, _name: &str, _args: &Value, _result: &std::result::Result<String, String>) {}
    /// Emergency mode: background model work pauses quietly (`Some`, the quota's reset when known) or resumes (`None`).
    fn pause_background(&self, reset: Option<Option<u64>>);
    /// No relay can drive Heimdall: the island's triangle and the local engine's offer.
    fn no_relay(&self, reset: Option<u64>);
    /// The main model works again after the local fallback: the user is asked whether the agent goes back to it.
    fn restore_model(&self, model: &agent::provider::ApiModel);
    /// The Claude models relay mode can run, from T3's live catalog (Agent ▸ Model ▸ Claude Code subscription).
    fn relay_models(&self, _models: Vec<ui::RelayModel>) {}
    /// The harness instances background jobs can run on, from the same catalog (Background Tasks ▸ Model's T3
    /// providers, `background_call::t3::publish_catalog`).
    fn t3_instances(&self, _instances: Vec<ui::T3Instance>) {}
    /// A card or line in the pull-down (the recovery choice, the return question, the waiting line).
    fn notice(&self, _card: ui::agent_notice::AgentNotice) {}
    fn dismiss(&self, _id: &str) {}
    /// The local engine when it is on this Mac: its model ready, or still to download (`rec::Local`).
    fn local_engine(&self) -> Option<rec::Local> { None }
    /// A T3 account's limit or balance is spent until `reset_ms` (when known): background helpers on it move off too
    /// (bug note 2026-11-02T14-47-41, `background_gate::quota_spent` on "t3:<instance>").
    fn account_spent(&self, _instance: &str, _reset_ms: Option<u64>) {}
    /// The user stays on the replacement: relay mode on that account and model becomes his setting (the menu's actions).
    fn keep_relay(&self, _route: &Route) {}
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase { Off, Starting, Covering, NoCover, Trial, Local, Choosing }
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct State {
    phase: Phase,
    #[serde(default)]
    reason: Reason,
    cause: Option<Value>,
    since_ms: u64,
    thread: Option<String>,
    harness: Option<String>,
    failed_probes: u32,
    next_probe_ms: u64,
    /// The last relay thread and when it stood down: a new relay within `REUSE_MS` goes back to it.
    last_thread: Option<(String, String, u64)>,
    /// What the relay did through the CLI, one line each (for the agent's "while you were down").
    did: Vec<String>,
    model: Option<agent::provider::ApiModel>,
    /// The thread and harness being tried while it shows it answers (`status` names them before `covering`).
    #[serde(default)]
    trying: Option<(String, String)>,
    /// The main model while the local engine stands in (`Emergency::local_fallback`).
    #[serde(default)]
    fallback: Option<agent::provider::ApiModel>,
    /// The relay thread and the last of its timeline positions mirrored into Pi's session (`mirror`).
    #[serde(default)]
    mirrored: Option<(String, u64)>,
    /// The T3 model relay mode asked for when it last started (`Options::relay_model`): a new choice moves the relay;
    /// a model T3 does not list, or a harness that stood in for Claude, does not restart it again.
    #[serde(default)]
    asked: Option<String>,
    /// The account relay mode asked for when it last started (`Options::relay_account`).
    #[serde(default)]
    asked_account: Option<String>,
    /// The account and model the relay runs on now.
    #[serde(default)]
    route: Option<Route>,
    /// What the user had before a failure: offered back when it works again, never switched to by itself.
    #[serde(default)]
    original: Option<Original>,
    /// What failed, for the search order.
    #[serde(default)]
    failed: Option<routing::Failed>,
    /// The open recovery card (`Phase::Choosing`).
    #[serde(default)]
    choice: Option<rec::Open>,
    /// The user chose to wait for the original (or for the local model to start): no relay is searched for.
    #[serde(default)]
    waiting: bool,
    /// The open return question; asked once (silence keeps the replacement).
    #[serde(default)]
    return_asked: Option<String>,
    /// The original works and the user is asked as soon as no relay run works (`Msg::Idle`).
    #[serde(default)]
    return_ready: bool,
    /// The user chose to go back while a run worked: it happens when the runs end.
    #[serde(default)]
    going_back: bool,
    /// The account and model of `last_thread` (a reused thread keeps its route; review 2 Nov: without it the relay
    /// ran with no route, and its failure took the stale built-in agent for the original).
    #[serde(default)]
    last_route: Option<Route>,
    /// Accounts whose limit or balance a failed run reported spent, with the reset T3 gave (bug note
    /// 2026-11-02T14-47-37): left out of the search and the cards until then, and the original's next check.
    #[serde(default)]
    spent: Vec<Spent>,
    /// Each Heimdall tool's definition digest (`tool_digests`) as the relay thread last learned them. A thread outlives
    /// Heimdall's restarts, and Claude keeps a definition it loaded in its conversation: after an update the thread
    /// is told which tools changed (bug note 2026-11-02T23-28-44: plan_edit without `quote` four hours after it had one).
    #[serde(default)]
    tools: std::collections::BTreeMap<String, String>,
    /// The thread `tools` belongs to: a thread reused within `REUSE_MS` keeps what it learned (fresh review, 3 Nov: an
    /// update between its stand-down and its reuse was never told).
    #[serde(default)]
    tools_thread: Option<String>,
}
/// One account T3 reported spent (`agent::limits`).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Spent { pub instance: String, pub reset_ms: Option<u64>, pub line: String, pub at_ms: u64, #[serde(default)] pub billing: bool }
/// A spent prepaid balance (no reset of its own) counts this long; the next search may try it again.
pub const SPENT_BALANCE_MS: u64 = 6 * 60 * 60_000;
/// A limit without a reset time counts this long (it may have been a passing rate limit).
pub const SPENT_NO_RESET_MS: u64 = 30 * 60_000;
impl State {
    /// `thread` takes over: a new thread loads Heimdall's tools as they are now; a reused one keeps what it learned.
    fn learn_tools(&mut self, thread: &str) {
        if self.tools_thread.as_deref() != Some(thread) { self.tools = tool_digests(); self.tools_thread = Some(thread.into()); }
    }
    /// The spent reports that still hold at `now`.
    fn spent_now(&self, now: u64) -> impl Iterator<Item = &Spent> {
        self.spent.iter().filter(move |x| x.reset_ms.map_or(now.saturating_sub(x.at_ms) < if x.billing { SPENT_BALANCE_MS } else { SPENT_NO_RESET_MS }, |r| r > now))
    }
    /// When `instance` is back by its spent report.
    fn spent_until(&self, instance: &str, now: u64) -> Option<u64> {
        self.spent_now(now).filter(|x| x.instance == instance).filter_map(|x| x.reset_ms).max()
    }
    /// Whether the last relay thread's account is reported spent: that thread is not reused (a send there would start
    /// a run on it; fresh review, 2 Nov).
    fn last_route_spent(&self, now: u64) -> bool {
        self.last_route.as_ref().is_some_and(|r| self.spent_now(now).any(|x| x.instance == r.instance))
    }
    /// The live quotas with every account reported spent marked so (`reached`): the search and the cards leave it out.
    fn with_spent(&self, mut quotas: Quotas, now: u64) -> Quotas {
        for x in self.spent_now(now) { quotas.entry(x.instance.clone()).or_default().reached = true; }
        quotas
    }
    /// What the user had when a failure began, when nothing is remembered: relay mode's account (never the built-in agent
    /// there, bug note 2026-11-02T14-47-34), else the built-in agent.
    fn host(&self) -> Original {
        match self.reason {
            Reason::Chosen => match self.route.clone().or_else(|| label_route(self.harness.as_deref(), None)) {
                Some(r) => Original::Relay(r),
                None => Original::Pi { model: self.model.clone() },
            },
            Reason::Emergency => Original::Pi { model: self.model.clone() },
        }
    }
}
/// A route from a harness line "Claude (claude-opus-5-5)" alone, when T3's catalog cannot name its account: the
/// account is `instance` (relay mode's choice) when known. Its quota is then unknown, so it is never offered back by
/// itself; the card still names it rightly.
fn label_route(harness: Option<&str>, instance: Option<&str>) -> Option<Route> {
    let (name, model) = harness?.strip_suffix(')')?.rsplit_once(" (")?;
    Some(Route { instance: instance.unwrap_or("").into(), driver: String::new(), name: name.into(), model: model.into() })
}
impl State {
}
impl Default for State {
    fn default() -> Self { Self { phase: Phase::Off, reason: Reason::Emergency, cause: None, since_ms: 0, thread: None, harness: None, failed_probes: 0, next_probe_ms: 0, last_thread: None, did: vec![], model: None, trying: None, fallback: None, mirrored: None, asked: None, asked_account: None, route: None, original: None, failed: None, choice: None, waiting: false, return_asked: None, return_ready: false, going_back: false, last_route: None, spent: vec![], tools: Default::default(), tools_thread: None } }
}
/// The longest a launched harness has to show it answers. It counts as answering at its first sign of life (a CLI
/// call, a real assistant message); a quota-dead one fails in seconds (27 Oct 19:40: the full 90 s was waited).
pub const ANSWER_WAIT: Duration = Duration::from_secs(90);
/// How often a starting thread is looked at.
const ANSWER_POLL: Duration = if cfg!(test) { Duration::from_millis(50) } else { Duration::from_secs(2) };
/// No harness answered: launch again after this.
pub const RELAUNCH_MS: u64 = if cfg!(test) { 300 } else { 10 * 60_000 };
pub const REUSE_MS: u64 = 2 * 60 * 60_000;
/// After a stand-down the thread's token still takes `end` (and `status`) this long, so its last summary reaches the
/// agent's record instead of being refused (bug note 2026-10-27T02-16-00).
pub const END_GRACE_MS: u64 = if cfg!(test) { 1_500 } else { 2 * 60_000 };
/// A relay run followed at most this long (its final reply is spoken when it ends).
const FOLLOW_MS: u64 = 60 * 60_000;
/// Relayed inputs kept while no thread takes them.
const PENDING: usize = 200;
/// Its thread titles: the T3 observer never follows them (`t3::observer::RELAY_TITLES`).
const TITLE: &str = t3::observer::RELAY_TITLES[0];
const TITLE_RELAY: &str = t3::observer::RELAY_TITLES[1];
/// A relay run is being followed: the agent's state reads Working (`bridge::delivery::stopping_window_state`).
pub static RELAY_WORKING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

enum Msg {
    Enter(Cause, Option<agent::provider::ApiModel>),
    Choose(bool),
    Relay(Relayed),
    Answered,
    End(String),
    HandBack(String, oneshot::Sender<String>),
    Model(Option<agent::provider::ApiModel>),
    Resume,
    RelayModels,
    /// The relay's own account failed (its quota, its sign-in); `true`: a prepaid balance (no reset by itself).
    RelayFailed(Cause, bool),
    /// The user's answer to a recovery or return card (`""`: dismissed).
    Answer(String, String),
    /// Every relay run has ended.
    Idle,
}
/// How the relay picks its harness (`Settings::relay_order`, `Settings::relay_skip`).
#[derive(Clone, Debug)]
pub struct Options {
    pub order: Vec<String>,
    pub skip_relay: bool,
    /// Relay mode's T3 model (Agent ▸ Model ▸ Claude Code subscription ▸ a model); `None`: the first T3 lists.
    pub relay_model: Option<String>,
    /// Relay mode's T3 Claude instance (Agent ▸ Model ▸ Claude Code subscription ▸ an account, `Settings::agent_relay_account`);
    /// `None`: automatic.
    pub relay_account: Option<String>,
}
impl Options {
    /// The model relay mode wants, when `harness` ("Claude (claude-opus-5-5)") runs another one.
    fn wants_other(&self, harness: &str) -> Option<&str> {
        self.relay_model.as_deref().filter(|m| !harness.ends_with(&format!("({m})")))
    }
    /// The user chose another relay model than the running relay was started with.
    fn moved(&self, asked: Option<&str>) -> bool {
        self.relay_model.is_some() && self.relay_model.as_deref() != asked
    }
    /// The user chose another relay account than the running relay was started with.
    fn moved_account(&self, asked: Option<&str>) -> bool {
        self.relay_account.is_some() && self.relay_account.as_deref() != asked
    }
}
pub struct Emergency {
    data: PathBuf,
    state: Mutex<State>,
    /// The controller's inbox. Swapped for a new channel when a dead controller is replaced (`respawn`).
    tx: Mutex<mpsc::UnboundedSender<Msg>>,
    rx: Mutex<Option<mpsc::UnboundedReceiver<Msg>>>,
    /// How the relay agent runs the CLI and where the protocol is.
    cli: String,
    docs: String,
    options: Mutex<Options>,
    /// When the relay's token last ran a call: a starting thread that calls has answered.
    last_call_ms: AtomicU64,
    /// Relay runs being followed, and the runs whose reply was already passed on.
    runs: AtomicUsize,
    replied: Mutex<HashSet<String>>,
    /// Input the relay thread got whose run has not ended: if the user switches back to Pi meanwhile, nobody has answered
    /// it (the relay's reply is no longer spoken), so it wakes Pi (a switch must break nothing).
    open: Mutex<Vec<Relayed>>,
    /// Bumped whenever a relay ends (hand-back, stand-down, relay off): its run followers stop at once and leave the
    /// island's state and the speech policy to Pi (review 27 Oct).
    generation: AtomicU64,
    /// The saved state held a relay when Heimdall started: the first controller picks it up (`resume`). A relay begun
    /// since then is the controller's own message to handle, never a resume (a startup race otherwise).
    restored: std::sync::atomic::AtomicBool,
    /// One mirror pass at a time.
    mirroring: tokio::sync::Mutex<()>,
}
impl Emergency {
    pub fn new(data: &Path, cli: String, docs: String) -> Arc<Self> {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut state: State = std::fs::read(data.join(em::DIR).join("state.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        // A hand-back turn a restart cut off proved nothing: the thread is back on duty until a probe passes.
        if state.phase == Phase::Trial { state.phase = Phase::Covering; }
        // Bug note 2026-11-02T14-47-34: relay mode (the user's choice) never ran on the built-in agent, so a built-in
        // original there is stale (2 Nov: "Wait for GPT-6.1 Sol" on every card, "back to GPT-6.1 Sol?" three times).
        if state.reason == Reason::Chosen && matches!(state.original, Some(Original::Pi { .. })) {
            eprintln!("emergency: a stale built-in original in relay mode is dropped, with its return question");
            state.original = None; state.return_asked = None; state.return_ready = false; state.going_back = false;
        }
        let restored = std::sync::atomic::AtomicBool::new(!matches!(state.phase, Phase::Off | Phase::Local));
        Arc::new(Self { restored, mirroring: tokio::sync::Mutex::new(()), data: data.into(), state: Mutex::new(state), tx: Mutex::new(tx), rx: Mutex::new(Some(rx)), cli, docs,
            options: Mutex::new(Options { order: em::default_order(crate::edition::PUBLIC), skip_relay: false, relay_model: None, relay_account: None }), last_call_ms: AtomicU64::new(0),
            runs: AtomicUsize::new(0), generation: AtomicU64::new(0), replied: Mutex::new(HashSet::new()), open: Mutex::new(Vec::new()) })
    }
    /// The CLI line for the relay agent: the bundled binary beside Heimdall's, with `--data-dir` off the default folder.
    pub fn cli_for(data: &Path) -> String {
        let bin = std::env::current_exe().ok().and_then(|e| e.parent().map(|p| p.join("heimdall-emergency"))).filter(|p| p.exists())
            .map_or_else(|| "heimdall-emergency".to_string(), |p| p.display().to_string());
        let default = crate::settings::profile_data();
        if default.as_deref() == Some(data) { bin } else { format!("{bin} --data-dir {}", data.display()) }
    }
    pub fn phase(&self) -> Phase { self.state.lock().unwrap().phase }
    pub fn reason(&self) -> Reason { self.state.lock().unwrap().reason }
    /// Input is held (the relay takes it) in these phases.
    pub fn holding(&self) -> bool { matches!(self.phase(), Phase::Starting | Phase::Covering | Phase::NoCover | Phase::Choosing) }
    /// The user's answer to a recovery or return card (`pipeline/actions.rs`). The local model is switched to by the
    /// pipeline (the menu's own path): its model id comes back here for that.
    pub fn answer(&self, request_id: &str, label: &str) -> Option<String> {
        let local = self.state.lock().unwrap().choice.as_ref().filter(|c| c.request_id == request_id)
            .and_then(|c| c.choices.iter().find(|x| x.label == label)).and_then(|x| match &x.pick { Pick::Local { model } => Some(model.clone()), _ => None });
        self.send(Msg::Answer(request_id.into(), label.into()));
        local
    }
    fn send(&self, msg: Msg) {
        // Bug note 2026-10-27T19-45-00: an emergency sent to a controller nobody ran went nowhere, silently.
        if self.tx.lock().unwrap().send(msg).is_err() { eprintln!("emergency: controller not running; the supervisor starts it again"); }
    }
    pub fn relay(self: &Arc<Self>) -> em::Relay {
        let me = self.clone();
        Arc::new(move |r| me.send(Msg::Relay(r)))
    }
    /// The supervisor saw the agent fail (or fail again during a trial). It holds turns itself, after this: the phase is
    /// `Starting` before the first held input reaches the controller (27 Oct 19:40:05: the unanswered wake the hold
    /// relayed arrived while the phase still read Off, was taken for late input, and started a Pi turn at 19:40:30).
    pub fn enter(&self, cause: Cause, model: Option<agent::provider::ApiModel>) {
        self.begin(Reason::Emergency);
        self.send(Msg::Enter(cause, model));
    }
    /// Relay mode on or off (`Settings::agent_relay`). On holds at once, like `enter`.
    pub fn choose(&self, on: bool) {
        // Every start says it; the controller decides in order (review 27 Oct: a stale early return here left relay
        // mode on in the settings with nothing held after a quick Agent off and on).
        let s = self.state.lock().unwrap().clone();
        if !on && s.reason != Reason::Chosen { return; }
        if on { self.begin(Reason::Chosen); }
        self.send(Msg::Choose(on));
    }
    fn begin(&self, reason: Reason) {
        let mut s = self.state.lock().unwrap();
        if matches!(s.phase, Phase::Off | Phase::Local) {
            s.phase = Phase::Starting; s.reason = reason; s.since_ms = agent::now_ms(); s.cause = None; s.thread = None;
            self.save(&s);
        }
    }
    /// A turn answered.
    pub fn answered(&self) { if self.phase() == Phase::Trial { self.send(Msg::Answered); } }
    /// Agent off, or the app stopping for good: the relay thread stands down.
    pub fn end(&self, why: &str) { if !matches!(self.phase(), Phase::Off | Phase::Local) { self.send(Msg::End(why.into())); } }
    /// The agent (re)started: a relay that a restart interrupted acts again.
    pub fn resume(&self) { if self.holding() && self.restored.swap(false, Ordering::AcqRel) { self.send(Msg::Resume); } }
    /// The menu opened: T3's Claude models are read again for Agent ▸ Model ▸ Claude Code subscription.
    pub fn refresh_relay_models(&self) { self.send(Msg::RelayModels); }
    /// The user picked another model: probe it soon.
    pub fn model(&self, model: Option<agent::provider::ApiModel>) { if self.phase() != Phase::Off { self.send(Msg::Model(model)); } }
    /// The local-engine offer was taken (`emergency_local`): `from` is the main model to go back to.
    pub fn local_fallback(&self, from: Option<agent::provider::ApiModel>) {
        let mut s = self.state.lock().unwrap();
        s.fallback = from.or_else(|| s.model.clone());
        self.save(&s);
    }
    /// The recovery card's local pick while the relay itself was the agent: no API model to go back to (review 30 Oct:
    /// the idle `agent_model` was offered as "Back to …?" and Yes landed him on a model he was not using).
    pub fn no_fallback(&self) {
        let mut s = self.state.lock().unwrap();
        s.fallback = None;
        self.save(&s);
    }
    /// The user's No to "Back to …?": the agent stays on the local engine and the stand-in is over (nothing asks again).
    pub fn keep_local(&self) {
        let mut s = self.state.lock().unwrap();
        if s.phase != Phase::Local { return; }
        s.phase = Phase::Off;
        s.fallback = None;
        self.save(&s);
    }
    pub fn configure(&self, options: Options) { *self.options.lock().unwrap() = options; }
    /// The controller's inbox for a new `run` after the old one died: queued messages went with it (logged).
    pub fn respawn(&self) {
        let mut rx = self.rx.lock().unwrap();
        if rx.is_some() { return; }
        let (tx, new) = mpsc::unbounded_channel();
        *self.tx.lock().unwrap() = tx;
        *rx = Some(new);
        // What the dead one missed is in the saved state: the new one picks it up.
        self.restored.store(true, Ordering::Release);
    }
    fn save(&self, s: &State) {
        let dir = self.data.join(em::DIR);
        let write = || -> Result<()> {
            std::fs::create_dir_all(&dir)?;
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
            agent::runtime::atomic_write(&dir.join("state.json"), &serde_json::to_vec(s)?)
        };
        if let Err(e) = write() { eprintln!("emergency: state not saved: {e:#}"); }
    }
    fn set(&self, f: impl FnOnce(&mut State)) -> State {
        let mut s = self.state.lock().unwrap();
        f(&mut s);
        self.save(&s);
        s.clone()
    }
    fn indicator(&self) -> ui::RelayIndicator {
        let s = self.state.lock().unwrap();
        // Emergency is the switch only: choosing and searching; a relay that answers runs normally.
        match (s.phase, s.reason) {
            (Phase::Off | Phase::Local | Phase::Trial, _) => ui::RelayIndicator::Off,
            (Phase::Choosing, _) => ui::RelayIndicator::Emergency { reset_ms: reset_of(&s) },
            (Phase::Starting | Phase::NoCover, _) if s.failed.is_some() || s.waiting => ui::RelayIndicator::Emergency { reset_ms: reset_of(&s) },
            (Phase::Covering, _) | (_, Reason::Chosen) => ui::RelayIndicator::Relay,
            _ => ui::RelayIndicator::Emergency { reset_ms: reset_of(&s) },
        }
    }

    /// The controller, until `stop`. One task: every phase change happens here, in order.
    #[allow(clippy::too_many_arguments)]
    pub async fn run(self: Arc<Self>, t3: Arc<dyn CoverT3>, probe: Arc<dyn MainProbe>, side: Arc<dyn Side>, host: Arc<dyn ToolHost>,
                     archive: Arc<dyn TranscriptSearch>, gate: Arc<agent::delivery::DeliveryGate>, mut stop: tokio::sync::watch::Receiver<bool>) {
        let Some(mut rx) = self.rx.lock().unwrap().take() else { return };
        let mut run = Running { em: self.clone(), t3, probe, side, host, archive, gate, server: None, token: None, grace_until: None, pending: VecDeque::new(), relaunch_ms: None, retry_ms: None };
        // A restart in the middle of a relay picks it up now, or when Agent comes on (`resume`).
        if run.gate.load(Ordering::Acquire) && self.restored.swap(false, Ordering::AcqRel) { run.resume().await; }
        run.side.indicator(self.indicator());
        let result = loop {
            let now = agent::now_ms();
            let due = {
                let s = self.state.lock().unwrap();
                let probe = (matches!(s.phase, Phase::Covering | Phase::NoCover) && (s.reason == Reason::Emergency || s.original.is_some()) && s.return_asked.is_none() && !s.return_ready && !s.going_back || s.phase == Phase::Local).then_some(s.next_probe_ms);
                let choosing = s.choice.as_ref().filter(|_| s.phase == Phase::Choosing).map(|c| c.until_ms);
                [probe, choosing, run.relaunch_ms, run.grace_until, run.retry_ms.filter(|_| !run.pending.is_empty() && s.phase == Phase::Covering)].into_iter().flatten().min()
            };
            let sleep = Duration::from_millis(due.map_or(3_600_000, |d| d.saturating_sub(now)));
            tokio::select! {
                _ = stop.changed() => break true,
                msg = rx.recv() => match msg { Some(m) => run.handle(m).await, None => break false },
                _ = tokio::time::sleep(sleep) => run.timer().await,
            }
        };
        // Heimdall stops; the relay (if any) continues at the next start. The token never outlives the process. The
        // inbox goes back for the next run (a respawned controller or the next supervisor).
        run.revoke().await;
        if result { *self.rx.lock().unwrap() = Some(rx); }
    }
}
fn reset_of(s: &State) -> Option<u64> {
    match s.cause.as_ref().and_then(Cause::from_json) { Some(Cause::Quota { reset_ms, .. }) => reset_ms, _ => None }
}

struct Running {
    em: Arc<Emergency>,
    t3: Arc<dyn CoverT3>,
    probe: Arc<dyn MainProbe>,
    side: Arc<dyn Side>,
    host: Arc<dyn ToolHost>,
    archive: Arc<dyn TranscriptSearch>,
    gate: Arc<agent::delivery::DeliveryGate>,
    server: Option<McpServer>,
    /// The live server's token (`covering` keeps both and only names the real thread).
    token: Option<String>,
    /// An end-only token after a stand-down, until then (`END_GRACE_MS`).
    grace_until: Option<u64>,
    pending: VecDeque<Relayed>,
    relaunch_ms: Option<u64>,
    /// When held input that failed to reach the thread is tried again: fixed at the first failure, so input that keeps
    /// arriving never pushes it back (review 27 Oct).
    retry_ms: Option<u64>,
}
impl Running {
    fn cause(&self) -> Option<Cause> {
        self.em.state.lock().unwrap().cause.as_ref().and_then(Cause::from_json)
    }
    async fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Enter(cause, model) => self.enter(cause, model).await,
            Msg::Choose(on) => self.choose(on).await,
            Msg::Resume => self.resume().await,
            Msg::RelayModels => match self.t3.catalog().await {
                Ok(catalog) => {
                    self.side.relay_models(em::relay_models(&catalog).into_iter().map(|(id, name)| ui::RelayModel { id, name }).collect());
                    self.side.t3_instances(crate::background_call::t3::publish_catalog(&catalog));
                }
                Err(e) => eprintln!("emergency: T3's model list unavailable: {e:#}"),
            },
            Msg::Relay(r) => {
                // Held as the hand-back or Agent off happened: nobody covers it now (review 27 Oct).
                if matches!(self.em.phase(), Phase::Off | Phase::Trial | Phase::Local) { self.side.late(&r); return; }
                if self.em.phase() == Phase::Covering && self.pending.is_empty() {
                    if let Err(r) = self.deliver(r).await { self.pending.push_back(r); self.retry_ms.get_or_insert(agent::now_ms() + 30_000); }
                } else {
                    if self.pending.len() >= PENDING { self.pending.pop_front(); }
                    self.pending.push_back(r);
                }
            }
            Msg::Answered => if self.em.phase() == Phase::Trial { self.finish().await },
            Msg::RelayFailed(cause, billing) => self.relay_failed(cause, billing).await,
            Msg::Answer(request_id, label) => self.answer(&request_id, &label).await,
            Msg::Idle => {
                let s = self.em.state.lock().unwrap().clone();
                if s.phase != Phase::Covering { return; }
                if s.going_back { self.go_back().await } else if s.return_ready { self.ask_return().await }
            }
            Msg::End(why) => self.stand_down(&why).await,
            Msg::HandBack(summary, reply) => {
                if self.em.phase() == Phase::Off && self.grace_until.is_some() {
                    // Stood down a moment ago (Agent off): its summary is the record, nothing is handed back.
                    self.side.record(&format!("{}end:{}", agent::notice::EMERGENCY_COVER, agent::now_ms()),
                        &agent::identity::names_only(&format!("[Relay agent, after standing down] {}", bounded(&summary, 2000)), &agent::identity::current()));
                    let _ = reply.send("Recorded for Heimdall's agent. The relay is over; you are done.".into());
                    return;
                }
                if self.em.reason() == Reason::Chosen {
                    let _ = reply.send("Relay mode is User's choice: it ends when he switches Heimdall back to his built-in agent. Keep acting for him.".into());
                    return;
                }
                if self.em.phase() != Phase::Covering {
                    let _ = reply.send(format!("Nothing to hand back now ({}): Heimdall is not waiting on you.", format!("{:?}", self.em.phase()).to_lowercase()));
                    return;
                }
                let model = self.em.state.lock().unwrap().model.clone();
                match self.probe.probe(model.as_ref()).await {
                    Ok(()) => {
                        // Going back is the user's choice (06:34): he is asked; the relay acts until he answers.
                        self.em.set(|s| s.did.push(format!("asked to hand back: {summary}")));
                        let _ = reply.send("Heimdall's agent answers again. User will be asked whether to go back to it once your run ends; keep acting for him until you are told.".into());
                        self.ask_return().await;
                    }
                    Err(e) => {
                        let next = self.em.set(|s| { s.failed_probes += 1; s.next_probe_ms = em::next_probe_ms(&Cause::from_json(s.cause.as_ref().unwrap_or(&Value::Null)).unwrap_or(Cause::NoAnswer), s.failed_probes, agent::now_ms()); s.did.push(format!("asked to hand back: {summary}")); }).next_probe_ms;
                        let _ = reply.send(format!("Heimdall's agent still fails ({}). Keep covering; Heimdall checks again at {}.", bounded(&e, 200), em::clock(next)));
                    }
                }
            }
            Msg::Model(model) => {
                let s = self.em.state.lock().unwrap().clone();
                if s.phase == Phase::Local {
                    // The user (or the hand-back) picked an API model again: the local stand-in is over.
                    if model.is_some() { self.em.set(|s| { s.phase = Phase::Off; s.fallback = None; }); eprintln!("emergency: the local engine stands down; the agent runs on its model again"); }
                    return;
                }
                if s.reason == Reason::Emergency { self.em.set(|s| { s.model = model; s.next_probe_ms = agent::now_ms() + 5_000; }); }
            }
        }
    }
    /// A restart in the middle of a relay: the same thread acts again with a new token, and in emergency mode the agent
    /// is checked within 30 s (it may have come back while Heimdall was down). Nothing when it already acts.
    async fn resume(&mut self) {
        if self.server.is_some() || !matches!(self.em.phase(), Phase::Starting | Phase::Covering | Phase::NoCover | Phase::Trial | Phase::Choosing) { return; }
        // A restart while the recovery card was open: its minute is over; the search starts now.
        if self.em.phase() == Phase::Choosing {
            let order = self.em.state.lock().unwrap().choice.as_ref().map(|c| c.order.iter().map(Route::harness).collect::<Vec<_>>());
            if let Some(order) = order { return self.search(order).await; }
            self.em.set(|s| s.phase = Phase::Starting);
        }
        let (thread, reason) = { let s = self.em.state.lock().unwrap(); (s.thread.clone(), s.reason) };
        match thread {
            Some(thread) => { self.issue(&thread).await; self.em.set(|s| { s.phase = Phase::Covering; s.next_probe_ms = agent::now_ms() + 30_000; }); }
            None if reason == Reason::Chosen || self.em.phase() == Phase::Starting => { self.start(None).await; }
            None => { let s = self.em.set(|s| { s.phase = Phase::NoCover; s.next_probe_ms = agent::now_ms() + 30_000; }); if !s.waiting { self.relaunch_ms = Some(agent::now_ms()); } }
        }
        if self.em.reason() == Reason::Emergency && self.em.holding() { self.side.pause_background(Some(reset_of(&self.em.state.lock().unwrap()))); }
        self.side.indicator(self.em.indicator());
    }
    async fn enter(&mut self, cause: Cause, model: Option<agent::provider::ApiModel>) {
        let s = self.em.state.lock().unwrap().clone();
        let failed = model.as_ref().map(|m| m.provider.clone());
        if s.reason == Reason::Chosen && s.phase != Phase::Off {
            // The relay is the agent already: Pi's provider failing changes nothing the user uses.
            eprintln!("emergency: the agent's provider fails ({}), and the relay User chose answers anyway", cause.sentence());
            return;
        }
        match s.phase {
            Phase::Off | Phase::Starting | Phase::Local if s.thread.is_none() && s.cause.is_none() => {
                let now = agent::now_ms();
                eprintln!("emergency: on: {}", cause.sentence());
                let original = Original::Pi { model: model.clone() };
                self.em.set(|s| { *s = State { phase: Phase::Starting, reason: Reason::Emergency, cause: Some(cause.to_json()), since_ms: now, next_probe_ms: em::next_probe_ms(&cause, 0, now), last_thread: s.last_thread.take(), last_route: s.last_route.take(), spent: std::mem::take(&mut s.spent), model, fallback: s.fallback.take(), original: Some(original), ..State::default() }; });
                self.side.cue(ui::HealthCue::Notice { part: ui::Part::Agent, label: cause.island() });
                self.side.indicator(self.em.indicator());
                self.side.pause_background(Some(reset_of(&self.em.state.lock().unwrap())));
                self.offer(routing::Failed { pi_provider: failed, ..routing::Failed::default() }, &cause.sentence()).await;
            }
            Phase::Trial => {
                // The hand-back turn failed: the same thread covers again.
                eprintln!("emergency: the agent failed again after a probe passed: {}", cause.sentence());
                let thread = s.thread.clone();
                let now = agent::now_ms();
                let s = self.em.set(|s| { s.phase = Phase::Covering; s.failed_probes += 1; s.cause = Some(cause.to_json()); s.next_probe_ms = em::next_probe_ms(&cause, s.failed_probes, now); });
                self.side.indicator(self.em.indicator());
                self.side.pause_background(Some(reset_of(&s)));
                match thread {
                    Some(thread) => {
                        self.issue(&thread).await;
                        let _ = self.t3.send(&thread, &format!("Heimdall's agent failed again right after it seemed back: {} You are covering again; Heimdall's tools work again. Heimdall checks again at {}.", cause.sentence(), em::clock(s.next_probe_ms))).await;
                    }
                    None => self.start(None).await,
                }
            }
            // Already covering: a better cause (the quota module's reset time) replaces the old one.
            _ => {
                let s = self.em.set(|s| { if matches!(cause, Cause::Quota { reset_ms: Some(_), .. }) || s.cause.is_none() { s.cause = Some(cause.to_json()); s.next_probe_ms = em::next_probe_ms(&cause, s.failed_probes, agent::now_ms()); } if s.model.is_none() { s.model = model; } });
                self.side.indicator(self.em.indicator());
                self.side.pause_background(Some(reset_of(&s)));
            }
        }
    }
    /// Relay mode on: the thread starts (or the emergency thread stays on as the user's choice). Off: Pi takes over again,
    /// quietly; what the relay did is in his conversation.
    async fn choose(&mut self, on: bool) {
        let s = self.em.state.lock().unwrap().clone();
        if on {
            match s.phase {
                Phase::Covering | Phase::NoCover if s.reason == Reason::Emergency => {
                    // The user chose the relay that already covers: it stays, without probes or a hand-back, and moves to
                    // the model he chose if it runs another. Waiting (no relay), it starts now.
                    self.close_cards();
                    self.em.set(|s| { s.reason = Reason::Chosen; s.cause = None; s.original = None; s.failed = None; s.waiting = false; s.return_ready = false; s.going_back = false; });
                    self.side.pause_background(None);
                    if s.phase == Phase::Covering && self.moved(&s) { self.move_relay(&s).await; }
                    if s.phase == Phase::NoCover { self.em.set(|s| s.phase = Phase::Starting); self.start(None).await; }
                }
                // The recovery card is open: an agent restart (the supervisor says `choose` on every start) leaves it;
                // the user's own pick in the menu answers it.
                Phase::Choosing if s.reason == Reason::Chosen && !self.moved(&s) => {}
                // The user picked another T3 model for the relay: the thread on the old one stands down, one on the new
                // model takes over (the record and the held input carry on).
                Phase::Covering if self.moved(&s) => self.move_relay(&s).await,
                Phase::Covering => {}
                // Off, Starting, a hand-back trial or the local stand-in: the relay starts, held from now (the
                // supervisor's tick holds a Bridge that is not).
                _ => {
                    self.close_cards();
                    self.em.set(|s| { s.phase = Phase::Starting; s.reason = Reason::Chosen; s.cause = None; s.fallback = None; s.waiting = false; s.original = None; s.failed = None; s.return_ready = false; s.going_back = false; });
                    self.start(None).await;
                }
            }
            self.side.indicator(self.em.indicator());
            return;
        }
        if s.reason != Reason::Chosen || s.phase == Phase::Off { return; }
        if let Some(thread) = &s.thread {
            let _ = self.t3.send(thread, "User switched Heimdall back to his built-in agent. Stop acting for him now; his tools stop working for you.").await;
        }
        let _ = std::fs::remove_dir_all(self.em.data.join(em::DIR).join("images"));
        // Pi's conversation mirrors the thread all along: the last of it goes in now, and nothing else is handed over.
        mirror(&self.em, &*self.t3, &*self.side).await;
        self.side.record(&format!("{}end:{}", agent::notice::EMERGENCY_COVER, agent::now_ms()), "[Relay mode over] User switched Heimdall back to you; the relay's conversation above is yours. Carry on from there.");
        // What the relay had not answered yet (a run still working, or input not yet delivered) is Pi's now: it wakes
        // Pi once the hold is off, in order.
        // The user's Stop cancels what came before it, as it would have for the relay.
        let mut unanswered: Vec<Relayed> = std::mem::take(&mut *self.em.open.lock().unwrap()).into_iter().chain(self.pending.drain(..)).collect();
        if let Some(stop) = unanswered.iter().rposition(|r| r.kind == "stop") { unanswered.drain(..=stop); }
        self.end_runs();
        self.revoke().await;
        self.forget_context();
        self.pending.clear();
        self.relaunch_ms = None;
        self.close_cards();
        self.em.set(|s| { s.phase = Phase::Off; s.reason = Reason::Emergency; s.mirrored = None; if let (Some(t), Some(h)) = (s.thread.take(), s.harness.take()) { s.last_thread = Some((t, h, agent::now_ms())); s.last_route = s.route.clone(); } s.did.clear(); s.cause = None; s.route = None; s.original = None; s.failed = None; s.waiting = false; });
        self.side.release(None);
        for r in &unanswered { self.side.unfinished(r); }
        self.side.indicator(ui::RelayIndicator::Off);
        eprintln!("emergency: relay mode off; the built-in agent answers again{}", if unanswered.is_empty() { String::new() } else { format!(" and takes {} unanswered input(s)", unanswered.len()) });
    }
    /// Relay mode's model changed: the running thread stands down and a thread on the new model takes over.
    async fn move_relay(&mut self, s: &State) {
        if let Some(thread) = &s.thread {
            let _ = self.t3.send(thread, "User switched the relay to another model. Stop acting for him now; his tools stop working for you.").await;
        }
        mirror(&self.em, &*self.t3, &*self.side).await;
        // What the old thread had not finished goes to the new one first.
        for r in std::mem::take(&mut *self.em.open.lock().unwrap()).into_iter().rev() { self.pending.push_front(r); }
        self.end_runs();
        self.revoke().await;
        self.em.set(|s| { s.phase = Phase::Starting; s.thread = None; s.harness = None; s.last_thread = None; s.mirrored = None; });
        self.start(None).await;
    }
    /// The context file and the first message for this takeover.
    fn brief(&self) -> String {
        let s = self.em.state.lock().unwrap().clone();
        let cause = s.cause.as_ref().and_then(Cause::from_json);
        let (conversation, plan, tasks) = self.side.context(em::CONTEXT_BYTES);
        let why = match (&s.reason, &cause) {
            (Reason::Emergency, Some(c)) => format!("Heimdall's agent failed at {}: {}", em::clock(s.since_ms), c.sentence()),
            _ => "User chose to run Heimdall's agent as this T3 thread (relay mode).".into(),
        };
        let body = format!("# Heimdall's context, {}\n\n{why}\n\n## Conversation, oldest first (the last 100,000 tokens at most)\n\n{}\n\n## Plan\n\n{}\n\n## T3 work Heimdall follows\n\n{}\n",
            em::clock(agent::now_ms()), if conversation.trim().is_empty() { "(nothing recent)" } else { &conversation },
            if plan.trim().is_empty() { "(empty)" } else { &plan }, if tasks.trim().is_empty() { "(none)" } else { &tasks });
        let file = match em::write_context(&self.em.data, &agent::identity::names_only(&body, &agent::identity::current())) {
            Ok(p) => p.display().to_string(),
            Err(e) => { eprintln!("emergency: context file not written: {e:#}"); "(not written; use timeline_search)".into() }
        };
        let tail = em::tail_bytes(&conversation, em::TAIL_BYTES);
        let plan = if plan.len() > em::PLAN_BYTES { em::tail_bytes(&plan, em::PLAN_BYTES) } else { plan };
        // The relay's whole system prompt, filled as Pi's is, goes first (docs/claude-relay.md, "Takeover").
        let prompt = relay_prompt(&self.em.data, &self.em.cli);
        em::Brief { reason: s.reason, cause: cause.as_ref(), since_ms: s.since_ms, cli: &self.em.cli, docs: &self.em.docs, prompt: &prompt, context: &file, tail: &tail, plan: &plan }.text()
    }
    /// Whether a launched (or re-briefed) thread answers: its first CLI call, a new real assistant message, a run that
    /// completed, or a run still working after `ANSWER_WAIT`. A harness error fails it at once.
    async fn answers(&self, thread: &str, run: Option<&str>, before: Option<String>, since_ms: u64) -> std::result::Result<(), String> {
        let began = std::time::Instant::now();
        loop {
            if self.em.last_call_ms.load(Ordering::Acquire) >= since_ms { return Ok(()); }
            let polled = std::time::Instant::now();
            match self.t3.outcome(thread, run, ANSWER_POLL).await {
                Ok(Run::Completed(_)) => return Ok(()),
                Ok(Run::Failed(e)) => return Err(e),
                Ok(Run::Working) => {
                    if let Ok(Some(reply)) = self.t3.latest_reply(thread).await {
                        if Some(&reply) != before.as_ref() && !em::harness_error_reply(&reply) { return Ok(()); }
                    }
                }
                Err(e) if began.elapsed() >= ANSWER_WAIT => return Err(format!("{e:#}")),
                Err(_) => {}
            }
            if began.elapsed() >= ANSWER_WAIT { return Ok(()); }
            if let Some(rest) = ANSWER_POLL.checked_sub(polled.elapsed()) { tokio::time::sleep(rest).await; }
        }
    }
    /// Find a harness that answers: `list` when given (the user's pick, or the search order after a failure), else the last
    /// relay thread when it is recent, else the accounts in order (`agent::routing`: relay mode's account first, else
    /// the one whose quota resets soonest; after a failure the same model on another account first). None (or relays
    /// skipped): the local engine is offered.
    async fn start(&mut self, list: Option<Vec<Harness>>) {
        let options = self.em.options.lock().unwrap().clone();
        let reason = self.em.reason();
        // What this start asked for: a later `choose` moves the relay only for a new choice (review 28 Oct: a model
        // T3 no longer lists, or Grok standing in for Claude, restarted the relay on every Pi start).
        if reason == Reason::Chosen { self.em.set(|s| { s.asked = options.relay_model.clone(); s.asked_account = options.relay_account.clone(); }); }
        if !(options.skip_relay && reason == Reason::Emergency && list.is_none()) {
            let brief = self.brief();
            // A recent thread on another model or account than relay mode's choice is not reused, nor one after a
            // failure (it may be on the failed account).
            let (last, failed) = { let s = self.em.state.lock().unwrap(); (s.last_thread.clone(), s.failed.is_some() || s.last_route_spent(agent::now_ms())) };
            let recent = last.filter(|(_, _, at)| agent::now_ms().saturating_sub(*at) < REUSE_MS)
                .filter(|(_, h, _)| reason != Reason::Chosen || options.wants_other(h).is_none())
                .filter(|_| list.is_none() && options.relay_account.is_none() && !failed);
            if let Some((thread, harness, _)) = recent {
                self.issue(&thread).await;
                self.em.set(|s| s.trying = Some((thread.clone(), harness.clone())));
                let before = self.t3.latest_reply(&thread).await.ok().flatten();
                // A reused thread's past was mirrored when it last acted: the mirror goes on from its end now.
                let known = self.em.state.lock().unwrap().mirrored.as_ref().filter(|(t, _)| t == &thread).map_or(0, |m| m.1);
                let base = self.t3.activity(&thread, known).await.ok().and_then(|items| items.last().map(|i| i.position)).unwrap_or(known);
                let since = agent::now_ms();
                let answered = match self.t3.send(&thread, &brief).await {
                    Ok(run) => self.answers(&thread, run.as_deref(), before, since).await.map(|()| run),
                    Err(e) => Err(format!("{e:#}")),
                };
                if let Ok(run) = answered { self.em.set(|s| s.route = s.last_route.clone()); return self.covering(thread, harness, run, base).await; }
                self.em.set(|s| s.trying = None);
                self.revoke().await;
            }
            let harnesses = match list {
                Some(list) => list,
                None => match self.t3.catalog().await {
                    Ok(catalog) => {
                        let live = self.t3.quotas().await;
                        let quotas = self.em.state.lock().unwrap().with_spent(live, agent::now_ms());
                        let failed = self.em.state.lock().unwrap().failed.clone();
                        // Relay mode starting on a spent account: the recovery card, as for any failure.
                        if let (Reason::Chosen, None, Some(h)) = (reason, &failed, routing::spent_choice(&catalog, &quotas, &options.order, options.relay_model.as_deref(), options.relay_account.as_deref())) {
                            let back = quotas.get(&h.instance).and_then(|q| q.back_ms());
                            let cause = Cause::Quota { reset_ms: back, line: None };
                            eprintln!("emergency: relay mode's account {} is spent; offering what can take over", h.name);
                            self.em.set(|s| { s.original = Some(Original::Relay(Route::from(&h))); s.cause = Some(cause.to_json()); });
                            let why = format!("{}'s quota is used up.", h.name);
                            return Box::pin(self.offer(routing::Failed { instance: Some(h.instance.clone()), driver: Some(h.driver.clone()), model: Some(h.model.clone()), pi_provider: None }, &why)).await;
                        }
                        match (reason, failed) {
                            (Reason::Chosen, None) => routing::initial(&catalog, &quotas, &options.order, options.relay_model.as_deref(), options.relay_account.as_deref()),
                            (_, Some(f)) => routing::failover(&catalog, &quotas, &f, &options.order, true),
                            (_, None) => {
                                let pi = self.em.state.lock().unwrap().model.as_ref().map(|m| m.provider.clone());
                                routing::failover(&catalog, &quotas, &routing::Failed { pi_provider: pi, ..routing::Failed::default() }, &options.order, true)
                            }
                        }
                    }
                    Err(e) => { eprintln!("emergency: T3 unavailable: {e:#}"); vec![] }
                },
            };
            for h in harnesses {
                // A fresh token for each thread: one that never answered never holds a live grant.
                self.issue("pending").await;
                let since = agent::now_ms();
                let launched = self.t3.launch(if reason == Reason::Chosen { TITLE_RELAY } else { TITLE }, &brief, &h).await;
                let (thread, run) = match launched {
                    Ok(t) => t,
                    Err(e) => { eprintln!("emergency: {} ({}) did not launch: {e:#}", h.name, h.model); continue; }
                };
                self.em.set(|s| s.trying = Some((thread.clone(), format!("{} ({})", h.name, h.model))));
                // The grant names the thread at once: its MCP server (`heimdall-emergency mcp`) serves only that thread,
                // and its first call may be what shows it answers.
                if let (Some(server), Some(token)) = (&self.server, &self.token) {
                    if let Err(e) = em::write_connection(&self.em.data, &server.url, token, &thread) { eprintln!("emergency: CLI connection not written: {e:#}"); }
                }
                match self.answers(&thread, run.as_deref(), None, since).await {
                    Ok(()) => {
                        self.em.set(|s| s.route = Some(Route::from(&h)));
                        return self.covering(thread, format!("{} ({})", h.name, h.model), run, 0).await;
                    }
                    Err(e) => eprintln!("emergency: {} ({}) failed: {}", h.name, h.model, bounded(&e, 300)),
                }
                let _ = self.t3.archive(&thread).await;
            }
        }
        self.em.set(|s| s.trying = None);
        self.revoke().await;
        eprintln!("emergency: no T3 relay answered{}; trying again in 10 minutes", if options.skip_relay { " (relays skipped by the setting)" } else { "" });
        let s = self.em.set(|s| s.phase = Phase::NoCover);
        self.relaunch_ms = (!s.waiting && (!options.skip_relay || reason == Reason::Chosen)).then(|| agent::now_ms() + RELAUNCH_MS);
        self.side.no_relay(reset_of(&s));
    }
    async fn covering(&mut self, thread: String, harness: String, run: Option<String>, base: u64) {
        self.relaunch_ms = None;
        // The mirror starts at the takeover's first message: what the thread already held is Heimdall's own past.
        self.em.set(|s| if s.mirrored.as_ref().is_none_or(|(t, _)| t != &thread) || base > s.mirrored.as_ref().map_or(0, |m| m.1) { s.mirrored = Some((thread.clone(), base)); });
        // The thread may already be calling through the server it was given at launch: re-issuing here retired it
        // mid-call (27 Oct 02:12:11, "Speech failed: MCP server retired"). Only the connection learns the real thread.
        match (&self.server, &self.token) {
            (Some(server), Some(token)) => {
                if let Err(e) = em::write_connection(&self.em.data, &server.url, token, &thread) { eprintln!("emergency: CLI connection not written: {e:#}"); }
            }
            _ => self.issue(&thread).await,
        }
        let switched = self.em.state.lock().unwrap().failed.is_some();
        let s = self.em.set(|s| {
            s.phase = Phase::Covering; s.thread = Some(thread.clone()); s.harness = Some(harness.clone()); s.trying = None;
            // A new thread loads the tools as they are now; a reused one keeps what it learned.
            s.learn_tools(&thread);
            // The search landed on the original account itself (2 Nov 13:06: Claude main had reset at 13:00): the
            // chain is over; there is nothing to go back to and nothing to ask.
            if let (Some(Original::Relay(o)), Some(r)) = (&s.original, &s.route) {
                if o.instance == r.instance {
                    let instance = o.instance.clone();
                    s.original = None; s.return_asked = None; s.return_ready = false; s.going_back = false;
                    s.spent.retain(|x| x.instance != instance);
                }
            }
        });
        eprintln!("emergency: {harness} {} in T3 thread {thread}", if s.reason == Reason::Chosen { "is the agent (relay mode)" } else { "covers" });
        // Bug note 2026-11-02T14-47-46: a switch after a failure is noticed on the island, and in relay mode said (or
        // shown with Speaking off); in emergency mode the relay's own first reply tells the user what broke and that it
        // has the work, so it is not said twice.
        if switched {
            if let Some(r) = &s.route {
                self.side.cue(ui::HealthCue::Notice { part: ui::Part::Agent, label: format!("Now on {}", r.label()) });
                if s.reason == Reason::Chosen { self.side.say(&rec::switched_line(r)); }
            }
        }
        let line = match s.cause.as_ref().and_then(Cause::from_json) {
            Some(cause) => format!("[Emergency mode] Your agent failed at {}: {} A relay agent, {harness}, took over in T3 thread {thread}. What reaches you meanwhile is context; what it does is recorded here.", em::clock(s.since_ms), cause.sentence()),
            None => format!("[Relay mode] User chose to run you as a T3 thread: {harness}, thread {thread}, acts for you. What reaches you meanwhile is context; what it does is recorded here."),
        };
        self.side.record(&format!("{}start:{}", agent::notice::EMERGENCY_COVER, s.since_ms), &line);
        self.side.indicator(self.em.indicator());
        // A switch that worked is over: the replacement runs normally, background work included.
        if s.reason == Reason::Emergency || s.original.is_some() { self.side.pause_background(None); }
        self.em.set(|s| { s.failed = None; s.waiting = false; });
        // Its answer to the first message is a reply like any other (emergency: the one short message to the user).
        self.follow(thread, run, None);
        while let Some(r) = self.pending.pop_front() {
            if let Err(r) = self.deliver(r).await { self.pending.push_front(r); self.retry_ms = Some(agent::now_ms() + 30_000); break; }
        }
    }
    /// One held input as a message in the relay thread; images go to private files it can open. The user's words animate
    /// the send arrow; his Stop interrupts the running run first.
    async fn deliver(&mut self, r: Relayed) -> std::result::Result<(), Relayed> {
        let Some(thread) = self.em.state.lock().unwrap().thread.clone() else { return Err(r) };
        let names = agent::identity::current();
        let mut text = format!("{} {}", agent::identity::names_only(&format!("[For Heimdall, {}]", label(&r.kind)), &names), r.text);
        let now_tools = tool_digests();
        let note = tools_note(&self.em.state.lock().unwrap().tools, &now_tools);
        if let Some(note) = &note { text = format!("{note}\n\n{text}"); }
        if !r.images.is_empty() {
            let dir = self.em.data.join(em::DIR).join("images");
            for (n, image) in r.images.iter().enumerate() {
                let ext = image.media_type.rsplit('/').next().unwrap_or("png");
                let path = dir.join(format!("{}-{n}.{ext}", safe(&r.id)));
                if write_private(&path, &image.bytes).is_ok() { text.push_str(&format!("\nImage: {}", path.display())); }
            }
        }
        let fresh = r.id.strip_prefix("stop-").and_then(|t| t.parse::<u64>().ok()).is_none_or(|at| agent::now_ms().saturating_sub(at) < 30_000);
        if r.kind == "stop" && fresh {
            if let Err(e) = self.t3.interrupt(&thread).await { eprintln!("emergency: Stop did not interrupt the relay run: {e:#}"); }
        }
        match self.t3.send(&thread, &text).await {
            Ok(run) => {
                if note.is_some() { self.em.set(|s| s.tools = now_tools); }
                // Bug note 2026-10-27 19:48: the send arrow never showed for words relayed instead of sent to Pi.
                if matches!(r.kind.as_str(), "wake" | "typed" | "answer") { self.side.sent(); }
                if r.kind != "stop" {
                    self.em.open.lock().unwrap().push(r.clone());
                    self.follow(thread, run, Some(r.id.clone()));
                }
                Ok(())
            }
            Err(e) => { eprintln!("emergency: relay to the thread failed, retrying in 30 s: {e:#}"); Err(r) }
        }
    }
    /// Follows one relay run to its end: the island works meanwhile, and its final reply is spoken and shown like Pi's
    /// (once per run, while the relay still acts).
    /// `input`: the relayed input this run answers, open until the run ends.
    fn follow(&self, thread: String, run: Option<String>, input: Option<String>) {
        let (em, t3, side) = (self.em.clone(), self.t3.clone(), self.side.clone());
        let generation = em.generation.load(Ordering::Acquire);
        if em.runs.fetch_add(1, Ordering::AcqRel) == 0 { side.working(true); }
        tokio::spawn(async move {
            let began = agent::now_ms();
            let wait = if cfg!(test) { Duration::from_millis(50) } else { Duration::from_secs(300) };
            let current = || em.generation.load(Ordering::Acquire) == generation;
            // The account failed under this run: its input stays open for the next account (`relay_failed`).
            let mut account_failed = false;
            // What T3 reported, read as a limit with its window and reset when it is one (bug note 2026-11-02T14-47-37).
            let who = { let s = em.state.lock().unwrap(); s.route.as_ref().map(|r| r.name.clone()).or_else(|| s.harness.as_deref().map(|h| h.rsplit_once(" (").map_or(h, |(n, _)| n).to_owned())).unwrap_or_else(|| "The relay's account".into()) };
            let cause_of = |text: &str| em::relay_cause(text, &who, agent::now_ms()).map(|c| (c, agent::limits::parse(text, 0).is_some_and(|l| l.billing)));
            loop {
                if !current() { return; }
                let polled = std::time::Instant::now();
                match t3.outcome(&thread, run.as_deref(), wait).await {
                    Ok(Run::Working) if agent::now_ms().saturating_sub(began) < FOLLOW_MS => {
                        // A long run is mirrored as it goes.
                        mirror(&em, &*t3, &*side).await;
                        // A fake (or a T3 that answers at once) never spins.
                        if polled.elapsed() < wait / 2 { tokio::time::sleep(wait / 2).await; }
                    }
                    Ok(Run::Completed(reply)) if current() => {
                        let key = run.clone().unwrap_or_else(|| reply.clone().unwrap_or_default());
                        let acting = matches!(em.phase(), Phase::Covering) && em.state.lock().unwrap().thread.as_deref() == Some(thread.as_str());
                        // Whoever takes the input out of `open` owns it: this reply, or Pi after a switch back (never both).
                        // A run that could not run at all (its account's limit): the relay's account failed.
                        if let Some((cause, billing)) = reply.as_deref().filter(|t| em::harness_error_reply(t)).and_then(cause_of).filter(|_| acting) {
                            account_failed = true;
                            let _ = em.tx.lock().unwrap().send(Msg::RelayFailed(cause, billing));
                        }
                        let mine = !account_failed && input.as_ref().is_none_or(|id| { let mut open = em.open.lock().unwrap(); let n = open.len(); open.retain(|r| &r.id != id); open.len() < n });
                        if let Some(text) = reply.filter(|t| !em::harness_error_reply(t)) {
                            if acting && mine && em.replied.lock().unwrap().insert(key) { side.reply(&text); }
                        }
                        mirror(&em, &*t3, &*side).await;
                        break;
                    }
                    Ok(Run::Failed(e)) => {
                        eprintln!("emergency: a relay run failed: {}", bounded(&e, 300));
                        if let Some((cause, billing)) = cause_of(&e).filter(|_| current()) { account_failed = true; let _ = em.tx.lock().unwrap().send(Msg::RelayFailed(cause, billing)); }
                        break;
                    }
                    Ok(Run::Working | Run::Completed(_)) => break,
                    Err(e) if agent::now_ms().saturating_sub(began) < FOLLOW_MS => { eprintln!("emergency: a relay run is unreadable, looking again: {e:#}"); tokio::time::sleep(wait / 10).await; }
                    Err(_) => break,
                }
            }
            if current() {
                if let Some(id) = input.as_ref().filter(|_| !account_failed) { em.open.lock().unwrap().retain(|r| &r.id != id); }
                if em.runs.fetch_sub(1, Ordering::AcqRel) == 1 { side.working(false); let _ = em.tx.lock().unwrap().send(Msg::Idle); }
            }
        });
    }
    /// Whether relay mode's model or account changed since the running relay started.
    fn moved(&self, s: &State) -> bool {
        let o = self.em.options.lock().unwrap();
        o.moved(s.asked.as_deref()) || o.moved_account(s.asked_account.as_deref())
    }
    /// Closes the recovery card and the return question, if open.
    fn close_cards(&self) {
        let s = self.em.state.lock().unwrap().clone();
        if let Some(c) = &s.choice { self.side.dismiss(&c.request_id); }
        if let Some(id) = &s.return_asked { self.side.dismiss(id); }
        self.em.set(|s| { s.choice = None; s.return_asked = None; s.return_ready = false; s.going_back = false; });
    }
    /// The agent's account failed: a quick scan (T3's catalog, every account's live quota, the local model), then the
    /// recovery card with what can take over and a one-minute countdown; nothing usable: straight to the search's end
    /// (the no-relay offer). Input is held meanwhile.
    async fn offer(&mut self, failed: routing::Failed, why: &str) {
        let order_setting = self.em.options.lock().unwrap().order.clone();
        let catalog = match self.t3.catalog().await { Ok(c) => c, Err(e) => { eprintln!("emergency: T3 unavailable for the recovery scan: {e:#}"); json!({}) } };
        let now = agent::now_ms();
        let s = self.em.state.lock().unwrap().clone();
        // The live quotas, and every account a failed run reported spent (bug note 2026-11-02T14-47-37).
        let quotas = s.with_spent(self.t3.quotas().await, now);
        // Relays skipped by the user's setting (`Settings::relay_skip`): only the local model and the wait.
        let skip = self.em.options.lock().unwrap().skip_relay && s.reason == Reason::Emergency;
        let order = if skip { vec![] } else { routing::failover(&catalog, &quotas, &failed, &order_setting, true) };
        let original = s.original.clone().unwrap_or_else(|| s.host());
        let back = match &original { Original::Relay(r) => s.spent_until(&r.instance, now).or(quotas.get(&r.instance).and_then(|q| q.back_ms())), Original::Pi { .. } => reset_of(&s) };
        let local = self.side.local_engine();
        let next = back.map_or(s.next_probe_ms.max(now + 2 * 60_000), |b| b.max(now) + 60_000);
        if order.is_empty() && local.is_none() {
            eprintln!("emergency: nothing can take over ({why}); waiting");
            self.em.set(|s| { s.failed = Some(failed); s.next_probe_ms = next; });
            return self.start(Some(vec![])).await;
        }
        let clock = |t: u64| agent::limits::when(t, now);
        let choices = rec::choices(&order, failed.driver.as_deref(), &quotas, local, &original, back, &clock);
        let open = rec::Open { request_id: format!("{}{now}", rec::PREFIX), until_ms: now + rec::COUNTDOWN_MS, choices, order: order.iter().map(Route::from).collect() };
        eprintln!("emergency: {why} Recovery card: {}", open.choices.iter().map(|c| c.label.as_str()).collect::<Vec<_>>().join(" | "));
        // One short line (a question that wraps reads badly in the card): what failed, then the choice.
        // Out of quota: the newest failure says so, or the original account is reported spent (a chain: Claude main
        // spent, then Claude code 2 failing otherwise; review 2 Nov).
        let quota = matches!(s.cause.as_ref().and_then(Cause::from_json), Some(Cause::Quota { .. }))
            || matches!(&original, Original::Relay(r) if s.spent_until(&r.instance, now).is_some());
        let until = back.filter(|_| quota).map(clock);
        let headline = match &until { Some(t) => format!("{} is out until {t}.", original.label()), None => format!("{} {}.", original.label(), if quota { "is out of quota" } else { "can't answer" }) };
        let card = rec::card(&open.request_id, &headline, &open.choices, open.until_ms);
        // Bug note 2026-11-02T14-47-46: the card and its countdown are said (shown on the island with Speaking off)
        // and noticed on the island.
        let any_route = open.choices.iter().any(|c| matches!(c.pick, Pick::Route(_)));
        let line = rec::card_line(&original, quota, until.as_deref(), &open.choices);
        let notice = rec::card_notice(&original, quota, until.as_deref(), any_route);
        self.em.set(|s| { s.phase = Phase::Choosing; s.failed = Some(failed); s.choice = Some(open); s.next_probe_ms = next; s.original = Some(original); });
        self.side.indicator(self.em.indicator());
        self.side.notice(card);
        self.side.cue(ui::HealthCue::Notice { part: ui::Part::Agent, label: notice });
        self.side.say(&line);
    }
    /// The automatic search (no choice in time, or the card closed): the scan's order, then whatever answers.
    async fn search(&mut self, order: Vec<Harness>) {
        self.close_cards();
        self.em.set(|s| s.phase = Phase::Starting);
        self.side.indicator(self.em.indicator());
        self.start(Some(order)).await;
    }
    /// The user answered a card.
    async fn answer(&mut self, request_id: &str, label: &str) {
        let s = self.em.state.lock().unwrap().clone();
        if let Some(open) = s.choice.clone().filter(|c| c.request_id == request_id) {
            if s.phase != Phase::Choosing { return; }
            let picked = open.choices.iter().find(|c| c.label == label).map(|c| c.pick.clone());
            eprintln!("emergency: recovery card answered: {}", if label.is_empty() { "closed" } else { label });
            match picked {
                Some(Pick::Route(r)) => {
                    let mut order = vec![r.harness()];
                    order.extend(open.order.iter().filter(|x| **x != r).map(Route::harness));
                    self.search(order).await;
                }
                // The pipeline switches the agent to the local model (the menu's path); its start ends this.
                Some(Pick::Local { .. }) => {
                    self.close_cards();
                    self.em.set(|s| { s.phase = Phase::NoCover; s.waiting = true; });
                    self.side.indicator(self.em.indicator());
                }
                Some(Pick::Wait) => {
                    self.close_cards();
                    let back = match &s.original { Some(Original::Relay(_)) => Some(s.next_probe_ms.saturating_sub(60_000)), _ => reset_of(&s) };
                    self.em.set(|s| { s.phase = Phase::NoCover; s.waiting = true; });
                    self.relaunch_ms = None;
                    self.side.notice(crate::emergency_local::waiting(back));
                    self.side.indicator(self.em.indicator());
                }
                // Closed without a choice: the search starts now.
                None => self.search(open.order.iter().map(Route::harness).collect()).await,
            }
            return;
        }
        if s.return_asked.as_deref() == Some(request_id) {
            let back = s.original.as_ref().is_some_and(|o| label == rec::back_label(o));
            self.side.dismiss(request_id);
            if back { self.go_back().await } else { self.stay().await }
        }
    }
    /// The relay's own account failed (its quota, a rejected sign-in): it stops, what it had not finished waits for
    /// the next one, and the recovery card offers what can take over. The original is what the user had when this chain
    /// of failures began: the first account in it, relay mode's own when it was relay mode (bug note
    /// 2026-11-02T14-47-34), never a built-in agent relay mode was not running on.
    async fn relay_failed(&mut self, cause: Cause, billing: bool) {
        let s = self.em.state.lock().unwrap().clone();
        if s.phase != Phase::Covering { return; }
        eprintln!("emergency: the relay's account fails: {}", cause.sentence());
        // A relay reused or restored without its route: found again by its harness ("Claude (claude-opus-5-5)").
        let route = match s.route.clone() {
            Some(r) => Some(r),
            None => match self.route_of(s.harness.as_deref()).await {
                Some(r) => Some(r),
                None => label_route(s.harness.as_deref(), self.em.options.lock().unwrap().relay_account.as_deref()),
            },
        };
        let original = match (s.original.clone(), s.reason) {
            (Some(Original::Pi { .. }), Reason::Chosen) => None,
            (o, _) => o,
        }.or_else(|| match s.reason {
            Reason::Chosen => route.clone().map(Original::Relay),
            Reason::Emergency => Some(Original::Pi { model: s.model.clone() }),
        });
        // Bug note 2026-11-02T14-47-44: no "stop acting" message into the spent thread: it started a run on the spent
        // account each time (runs 926, 22 and 3 on 2 Nov). Its token dies below; the thread is left as it is.
        eprintln!("emergency: the spent thread {} gets no message (it would start a run on the spent account)", s.thread.as_deref().unwrap_or("?"));
        mirror(&self.em, &*self.t3, &*self.side).await;
        for r in std::mem::take(&mut *self.em.open.lock().unwrap()).into_iter().rev() { self.pending.push_front(r); }
        self.end_runs();
        self.revoke().await;
        // A return question still open is answered by this failure: the card that follows asks again.
        if let Some(id) = &s.return_asked { self.side.dismiss(id); }
        let now = agent::now_ms();
        let spent = match (&cause, &route) {
            (Cause::Quota { reset_ms, line }, Some(r)) if !r.instance.is_empty() => Some(Spent { instance: r.instance.clone(), reset_ms: *reset_ms, line: line.clone().unwrap_or_else(|| cause.sentence()), at_ms: now, billing }),
            _ => None,
        };
        if let Some(x) = &spent { self.side.account_spent(&x.instance, x.reset_ms); }
        self.em.set(|s| {
            s.thread = None; s.harness = None; s.last_thread = None; s.last_route = None; s.mirrored = None; s.route = None; s.original = original;
            s.cause = Some(cause.to_json()); s.failed_probes = 0; s.return_asked = None; s.return_ready = false; s.going_back = false;
            if let Some(x) = spent { s.spent.retain(|y| y.instance != x.instance); s.spent.push(x); }
        });
        self.side.pause_background(Some(reset_of(&self.em.state.lock().unwrap())));
        // When Pi failed first, its provider's subscription stays out too (ChatGPT ↔ Codex).
        let pi_provider = match self.em.state.lock().unwrap().original.clone() { Some(Original::Pi { model }) => model.or(s.model.clone()).map(|m| m.provider), _ => None };
        let failed = routing::Failed { instance: route.as_ref().map(|r| r.instance.clone()), driver: route.as_ref().map(|r| r.driver.clone()), model: route.as_ref().map(|r| r.model.clone()), pi_provider };
        self.offer(failed, &cause.sentence()).await;
    }
    /// The account a relay without a recorded route runs on, from its harness line "Name (model)" and T3's catalog.
    async fn route_of(&self, harness: Option<&str>) -> Option<Route> {
        let (name, model) = harness?.strip_suffix(')')?.rsplit_once(" (")?;
        let catalog = self.t3.catalog().await.ok()?;
        catalog["providers"].as_array()?.iter().find(|p| p["displayName"].as_str() == Some(name)).and_then(|p| {
            let instance = p["providerInstanceId"].as_str()?;
            Some(Route { instance: instance.into(), driver: p["driverKind"].as_str().unwrap_or(instance).into(), name: name.into(), model: model.into() })
        })
    }
    /// The original's check: Pi by its probe, a relay account by its live quota (a reset passing is not enough). It
    /// works: the user is asked, once, when no relay run is working (never interrupting live work).
    async fn check_original(&mut self) {
        let s = self.em.state.lock().unwrap().clone();
        let original = s.original.clone().unwrap_or_else(|| s.host());
        let now = agent::now_ms();
        let (works, back) = match &original {
            Original::Pi { model } => (self.probe.probe(model.as_ref().or(s.model.as_ref())).await.is_ok(), None),
            // Its live quota decides (the reset passing is no proof), and never before the reset T3 reported: the quota
            // reading covers only the 5-hour and weekly windows, so a limit it misses still holds until then.
            Original::Relay(r) => {
                let quotas = self.t3.quotas().await;
                let reported = s.spent_until(&r.instance, now);
                (routing::recovered(quotas.get(&r.instance)) && reported.is_none(), reported.or(quotas.get(&r.instance).and_then(|q| q.back_ms())))
            }
        };
        if works { if let Original::Relay(r) = &original { self.em.set(|s| s.spent.retain(|x| x.instance != r.instance)); } }
        if !works {
            let s = self.em.set(|s| {
                s.failed_probes += 1;
                let c = Cause::from_json(s.cause.as_ref().unwrap_or(&Value::Null)).unwrap_or(Cause::NoAnswer);
                s.next_probe_ms = back.filter(|b| *b > now).map_or_else(|| em::next_probe_ms(&c, s.failed_probes, now), |b| b + 60_000);
            });
            eprintln!("emergency: {} does not work yet; next check at {}", original.label(), em::clock(s.next_probe_ms));
            return;
        }
        if s.phase == Phase::NoCover {
            // The user chose to wait for this account: it takes over now.
            if let Original::Relay(r) = &original {
                eprintln!("emergency: {} works again; the relay starts on it", r.label());
                self.em.set(|s| { s.waiting = false; s.original = None; s.failed = None; s.phase = Phase::Starting; });
                self.side.dismiss(&format!("{}waiting", crate::emergency_local::CARD_PREFIX));
                return self.start(Some(vec![r.harness()])).await;
            }
            return;
        }
        self.ask_return().await;
    }
    /// Asks the user once, by voice and with a card, whether to go back to the original; later while a relay run works.
    async fn ask_return(&mut self) {
        let s = self.em.state.lock().unwrap().clone();
        if s.return_asked.is_some() || s.phase != Phase::Covering { return; }
        // Never over live work: asked when the runs end (`Msg::Idle`), without checking again meanwhile.
        if self.em.runs.load(Ordering::Acquire) > 0 {
            self.em.set(|s| s.return_ready = true);
            return;
        }
        let original = s.original.clone().unwrap_or_else(|| s.host());
        let Some(current) = s.route.clone() else { return };
        let now = agent::now_ms();
        let id = format!("{}{now}", rec::RETURN_PREFIX);
        eprintln!("emergency: {} works again; asking User whether to go back", original.label());
        self.em.set(|s| { s.return_asked = Some(id.clone()); s.return_ready = false; s.original = Some(original.clone()); });
        self.side.say(&rec::return_line(&original));
        self.side.notice(rec::return_card(&id, &original, &current, now));
    }
    /// Back to the original: the built-in agent by the hand-back, a relay account by moving the relay to it (what
    /// the relay had not finished goes along; nothing finished is replayed).
    async fn go_back(&mut self) {
        // A run works: going back would cut it and send its input again. It happens when the runs end.
        if self.em.runs.load(Ordering::Acquire) > 0 {
            self.em.set(|s| { s.going_back = true; s.return_asked = None; });
            return;
        }
        self.em.set(|s| s.going_back = false);
        let s = self.em.state.lock().unwrap().clone();
        eprintln!("emergency: User goes back to {}", s.original.as_ref().map_or_else(|| "the built-in agent".into(), Original::label));
        match s.original.clone() {
            Some(Original::Relay(r)) => {
                if let Some(thread) = &s.thread { let _ = self.t3.send(thread, "User went back to his original account. Stop acting for him now; his tools stop working for you.").await; }
                mirror(&self.em, &*self.t3, &*self.side).await;
                for x in std::mem::take(&mut *self.em.open.lock().unwrap()).into_iter().rev() { self.pending.push_front(x); }
                self.end_runs();
                self.revoke().await;
                let mut order = vec![r.harness()];
                order.extend(s.route.as_ref().map(Route::harness));
                self.em.set(|s| { s.phase = Phase::Starting; s.thread = None; s.harness = None; s.last_thread = None; s.mirrored = None; s.original = None; s.return_asked = None; s.route = None; s.cause = None; });
                self.start(Some(order)).await;
            }
            _ => {
                self.em.set(|s| { s.original = None; s.return_asked = None; });
                self.recover(None).await;
            }
        }
    }
    /// The user stays on the replacement: it becomes his relay setting, and he is not asked again.
    async fn stay(&mut self) {
        let s = self.em.set(|s| { s.original = None; s.cause = None; s.reason = Reason::Chosen; s.asked = s.route.as_ref().map(|r| r.model.clone()); s.asked_account = s.route.as_ref().map(|r| r.instance.clone()); });
        eprintln!("emergency: User stays on {}", s.route.as_ref().map_or_else(String::new, Route::label));
        if let Some(r) = &s.route { self.side.keep_relay(r); }
        self.side.indicator(self.em.indicator());
    }
    async fn timer(&mut self) {
        let now = agent::now_ms();
        if self.grace_until.is_some_and(|t| t <= now) { self.revoke().await; }
        if self.em.phase() == Phase::Covering && !self.pending.is_empty() && self.retry_ms.is_none_or(|t| t <= now) {
            self.retry_ms = None;
            while let Some(r) = self.pending.pop_front() {
                if let Err(r) = self.deliver(r).await { self.pending.push_front(r); self.retry_ms = Some(agent::now_ms() + 30_000); break; }
            }
        }
        if self.relaunch_ms.is_some_and(|t| t <= now) && self.em.phase() == Phase::NoCover {
            self.relaunch_ms = None;
            if !self.em.state.lock().unwrap().waiting { self.start(None).await; }
        }
        // The recovery card's minute is over: Heimdall searches by itself, in the order the card was built from.
        let open = { let s = self.em.state.lock().unwrap(); s.choice.clone().filter(|c| c.until_ms <= now && s.phase == Phase::Choosing) };
        if let Some(open) = open {
            eprintln!("emergency: no choice on the recovery card; searching");
            return self.search(open.order.iter().map(Route::harness).collect()).await;
        }
        let s = self.em.state.lock().unwrap().clone();
        if s.phase == Phase::Local && s.next_probe_ms <= now {
            // The local engine stands in: is the main model back?
            match self.probe.probe(s.fallback.as_ref()).await {
                Ok(()) => {
                    // Asked, never switched (30 Oct 01:2x): the local stand-in lasts until the user answers. His Yes picks the
                    // main model (`Msg::Model` ends this phase), his No ends it on the local model (`keep_local`); an
                    // unanswered card is asked again at the next check, half an hour on.
                    eprintln!("emergency: the main model answers again; asking User whether the agent goes back to it");
                    if let Some(m) = &s.fallback { self.side.restore_model(m); }
                    self.em.set(|s| { s.next_probe_ms = agent::now_ms() + 30 * 60_000; });
                }
                Err(_) => { self.em.set(|s| { s.failed_probes += 1; let c = Cause::from_json(s.cause.as_ref().unwrap_or(&Value::Null)).unwrap_or(Cause::NoAnswer); s.next_probe_ms = em::next_probe_ms(&c, s.failed_probes, agent::now_ms()); }); }
            }
            return;
        }
        // A relay covers after a switch: the original is checked, and the user asked when it works (never switched to).
        // Waiting for a relay account he chose to wait for: it takes over again once it works.
        let waiting_relay = s.phase == Phase::NoCover && s.waiting && matches!(s.original, Some(Original::Relay(_)));
        if (s.phase == Phase::Covering || waiting_relay) && s.return_asked.is_none() && !s.return_ready && !s.going_back && s.next_probe_ms <= now && (s.original.is_some() || s.reason == Reason::Emergency) {
            return self.check_original().await;
        }
        let due = s.phase == Phase::NoCover && s.reason == Reason::Emergency && s.next_probe_ms <= now;
        if due {
            match self.probe.probe(s.model.as_ref()).await {
                Ok(()) => self.recover(None).await,
                Err(e) => {
                    let s = self.em.set(|s| { s.failed_probes += 1; let c = Cause::from_json(s.cause.as_ref().unwrap_or(&Value::Null)).unwrap_or(Cause::NoAnswer); s.next_probe_ms = em::next_probe_ms(&c, s.failed_probes, agent::now_ms()); });
                    eprintln!("emergency: the agent still fails ({}); next check at {}", bounded(&e, 200), em::clock(s.next_probe_ms));
                }
            }
        }
    }
    /// The probe passed: the thread wraps up, its token dies, and the agent's next turn is the proof. Everything the
    /// thread saw and did is in Pi's conversation already (`mirror`); the turn only says he is back, no hand-off.
    async fn recover(&mut self, summary: Option<String>) {
        let s = self.em.state.lock().unwrap().clone();
        let thread = s.thread.clone();
        if let Some(thread) = &thread {
            if summary.is_none() {
                let _ = self.t3.send(thread, "Heimdall's agent answers again. Wrap up: stop acting for him and don't speak to User again. His tools stop working for you now.").await;
            }
        }
        mirror(&self.em, &*self.t3, &*self.side).await;
        self.end_runs();
        self.revoke().await;
        self.forget_context();
        let cause = Cause::from_json(s.cause.as_ref().unwrap_or(&Value::Null)).unwrap_or(Cause::NoAnswer);
        let mut back = format!("You are back: your agent failed at {} ({}) and answers again at {}.", em::clock(s.since_ms), cause.sentence(), em::clock(agent::now_ms()));
        match (&s.harness, &thread) {
            (Some(h), Some(t)) => back.push_str(&format!(" Meanwhile {h} acted as you in T3 thread {t}: everything it was told, said and did is in your conversation above, in order. Carry on from there.")),
            _ => back.push_str(" No relay agent answered; what reached you meanwhile is in your conversation above, marked as relayed."),
        }
        back.push_str("\nOwner hears that you are back when this turn answers; answer what still needs you, briefly.");
        self.close_cards();
        self.em.set(|s| { s.phase = Phase::Trial; s.mirrored = None; s.original = None; s.last_route = s.route.take(); s.failed = None; s.waiting = false; });
        self.side.indicator(ui::RelayIndicator::Off);
        eprintln!("emergency: the agent answers a probe; handing back");
        self.side.release(Some(agent::identity::names_only(&back, &agent::identity::current())));
    }
    /// The hand-back turn answered: emergency mode is over (or the local engine stands in until the main model is back).
    async fn finish(&mut self) {
        let _ = std::fs::remove_dir_all(self.em.data.join(em::DIR).join("images"));
        let local = self.em.state.lock().unwrap().fallback.is_some();
        eprintln!("emergency: off; the agent is back{}", if local { " on the local engine" } else { "" });
        self.side.record(&format!("{}end:{}", agent::notice::EMERGENCY_COVER, agent::now_ms()), "[Emergency mode over] Your agent answers again; the relay agent stood down.");
        self.side.cue(ui::HealthCue::Recovered { key: "emergency".into(), part: ui::Part::Agent, label: "Agent back".into() });
        self.side.say("I'm back.");
        self.side.pause_background(None);
        self.em.set(|s| {
            s.phase = if local { Phase::Local } else { Phase::Off };
            // The route went to `last_route` at the hand-back (`recover`).
            if let (Some(t), Some(h)) = (s.thread.take(), s.harness.take()) { s.last_thread = Some((t, h, agent::now_ms())); }
            s.did.clear();
            if local { s.failed_probes = 0; s.next_probe_ms = s.cause.as_ref().and_then(Cause::from_json).map_or(agent::now_ms() + 2 * 60_000, |c| em::next_probe_ms(&c, 0, agent::now_ms())); } else { s.cause = None; }
        });
        self.side.indicator(ui::RelayIndicator::Off);
    }
    /// Agent off (or no way back): the thread is told to stop and the token dies; nothing is said.
    async fn stand_down(&mut self, why: &str) {
        let thread = self.em.state.lock().unwrap().thread.clone();
        if let Some(thread) = &thread { let _ = self.t3.send(thread, &format!("Stop acting now: {why}. For two more minutes Heimdall's tools take only `emergency_end` with a short summary of what you did; then they stop working.")).await; }
        mirror(&self.em, &*self.t3, &*self.side).await;
        self.end_runs();
        self.revoke().await;
        // The thread's last `end` is recorded, not refused: an end-only token for `END_GRACE_MS`. Agent off closed the
        // agent's delivery gate, so this server has its own, always open: it reaches no host tool, only `status` and
        // `end`, which record and answer and deliver nothing to the user.
        if let Some(thread) = &thread {
            self.issue_scoped(thread, Allow::Only(&[]), Arc::new(agent::delivery::DeliveryGate::new(true))).await;
            self.grace_until = Some(agent::now_ms() + END_GRACE_MS);
        }
        self.pending.clear();
        self.relaunch_ms = None;
        self.close_cards();
        self.em.set(|s| { s.original = None; s.failed = None; s.waiting = false; });
        // Relay mode is the user's setting: Agent back on starts it again (the supervisor's `choose`).
        self.em.set(|s| { s.phase = Phase::Off; s.reason = Reason::Emergency; s.mirrored = None; if let (Some(t), Some(h)) = (s.thread.take(), s.harness.take()) { s.last_thread = Some((t, h, agent::now_ms())); s.last_route = s.route.clone(); } s.route = None; s.did.clear(); s.cause = None; });
        let _ = std::fs::remove_dir_all(self.em.data.join(em::DIR).join("images"));
        self.forget_context();
        // The Bridge may have been held again meanwhile (Agent back on before this ran): it must not stay deaf.
        self.side.release(None);
        self.side.pause_background(None);
        self.side.indicator(ui::RelayIndicator::Off);
        eprintln!("emergency: off ({why})");
    }
    /// A new token and scoped server for `thread` with every main tool; the old ones die first.
    async fn issue(&mut self, thread: &str) { self.issue_scoped(thread, Allow::All, self.gate.clone()).await }
    /// `issue` with these host tools (none: only `status` and `end`), gated by `gate`.
    async fn issue_scoped(&mut self, thread: &str, allow: Allow, gate: Arc<agent::delivery::DeliveryGate>) {
        self.revoke().await;
        let token = em::new_token();
        let own = Arc::new(Own { em: self.em.clone(), side: self.side.clone() });
        let scope = Scope { token: token.clone(), allow, own };
        match McpServer::start_scoped(self.host.clone(), self.archive.clone(), gate, scope).await {
            Ok(server) => {
                if let Err(e) = em::write_connection(&self.em.data, &server.url, &token, thread) { eprintln!("emergency: CLI connection not written: {e:#}"); }
                self.server = Some(server);
                self.token = Some(token);
            }
            Err(e) => eprintln!("emergency: relay server did not start: {e:#}"),
        }
    }
    /// The relay ended: its followers stop, and the island is no longer working for it.
    fn end_runs(&self) {
        self.em.generation.fetch_add(1, Ordering::AcqRel);
        self.em.open.lock().unwrap().clear();
        if self.em.runs.swap(0, Ordering::AcqRel) > 0 { self.side.working(false); }
    }
    /// The takeover's context file goes with the relay (it quotes the user).
    fn forget_context(&self) { let _ = std::fs::remove_file(self.em.data.join(em::DIR).join(em::CONTEXT)); }
    async fn revoke(&mut self) {
        self.grace_until = None;
        self.token = None;
        if let Err(e) = em::remove_connection(&self.em.data) { eprintln!("emergency: CLI connection not removed: {e:#}"); }
        if let Some(server) = self.server.take() { server.shutdown().await; }
    }
}

/// The head of the tools note: a relayed input it opens is Heimdall's own message, never mirrored as the relay's.
const TOOLS_NOTE: &str = "[Heimdall was updated]";
/// Each tool Heimdall serves the relay (`mcp_contract::tools()`), by name: a digest of its whole definition.
fn tool_digests() -> std::collections::BTreeMap<String, String> {
    use sha2::Digest;
    agent::mcp::tools().as_array().into_iter().flatten().filter_map(|t| {
        let name = t["name"].as_str()?.to_string();
        let digest = sha2::Sha256::digest(t.to_string().as_bytes()).iter().take(8).map(|b| format!("{b:02x}")).collect();
        Some((name, digest))
    }).collect()
}
/// The line the relay thread reads before its next input when Heimdall's tools changed since it learned them: the
/// tools by name, and how to load them again. None when nothing changed. A thread that learned none (state written
/// before this field) is told every definition may be old.
pub(crate) fn tools_note(learned: &std::collections::BTreeMap<String, String>, now: &std::collections::BTreeMap<String, String>) -> Option<String> {
    let reload = "Any definition of them earlier in this conversation is out of date: load them again before you use them (in Claude Code, ToolSearch with select:";
    let other = " Without ToolSearch, list Heimdall's tools again (the CLI's tools) and follow a tool's error when it names a field.";
    if learned.is_empty() {
        return Some(format!("{TOOLS_NOTE} Its tools may have changed since you loaded them. {reload}mcp__heimdall__<name>, or a keyword search).{other}"));
    }
    let changed: Vec<&str> = now.iter().filter(|(n, d)| learned.get(*n) != Some(*d)).map(|(n, _)| n.as_str()).collect();
    if changed.is_empty() { return None; }
    let select = changed.iter().map(|n| format!("mcp__heimdall__{n}")).collect::<Vec<_>>().join(",");
    Some(format!("{TOOLS_NOTE} These tools changed: {}. {reload}{select}).{other}", changed.join(", ")))
}

/// The relay agent's own tools and the record of everything it does.
struct Own { em: Arc<Emergency>, side: Arc<dyn Side> }
#[async_trait]
impl ScopedTools for Own {
    fn definitions(&self) -> Vec<Value> {
        let none = json!({"type":"object","properties":{},"additionalProperties":false});
        vec![
            json!({"name":"emergency_status","description":"Who you are acting as and why (relay mode, or what failed and since when), and when Heimdall checks his agent again.","inputSchema":none}),
            json!({"name":"emergency_end","description":"Emergency mode: hand back to Heimdall with a summary of what you did. He checks his agent first and says if it still fails.",
                "inputSchema":{"type":"object","properties":{"summary":{"type":"string","minLength":1,"maxLength":4000}},"required":["summary"],"additionalProperties":false}}),
        ]
    }
    async fn call(&self, name: &str, args: &Value, _: &agent::delivery::DeliveryPermit) -> Option<Result<String>> {
        self.em.last_call_ms.store(agent::now_ms(), Ordering::Release);
        match name {
            "emergency_status" => {
                let s = self.em.state.lock().unwrap().clone();
                if s.phase == Phase::Off { return Some(Ok("The relay is over; nothing to cover. Call `emergency_end` with your summary if you have not.".into())); }
                let (thread, harness) = match (&s.thread, &s.harness, &s.trying) {
                    (Some(t), Some(h), _) => (t.clone(), h.clone()),
                    (_, _, Some((t, h))) => (t.clone(), h.clone()),
                    _ => ("?".into(), "?".into()),
                };
                if s.reason == Reason::Chosen {
                    return Some(Ok(format!("Relay mode since {}: User chose to run Heimdall's agent as you, {harness} in T3 thread {thread}. It lasts until he switches back.", em::clock(s.since_ms))));
                }
                let parsed = Cause::from_json(s.cause.as_ref().unwrap_or(&Value::Null));
                let cause = parsed.as_ref().map_or_else(|| "unknown".into(), |c| c.sentence());
                // The next check is not the quota's reset: say which one is known.
                let reset = match parsed {
                    Some(Cause::Quota { reset_ms: Some(at), .. }) => format!(" The quota resets at {}.", em::clock(at)),
                    Some(Cause::Quota { reset_ms: None, .. }) => " The quota's reset time is unknown; the next check is not the reset.".into(),
                    _ => String::new(),
                };
                Some(Ok(format!("Emergency mode since {}: {cause} You cover as {harness} in T3 thread {thread}.{reset} Heimdall checks his agent again at {}.",
                    em::clock(s.since_ms), em::clock(s.next_probe_ms))))
            }
            "emergency_end" => {
                let summary = args["summary"].as_str().filter(|s| !s.trim().is_empty() && s.len() <= 4000);
                let Some(summary) = summary else { return Some(Err(anyhow::anyhow!("INVALID_ARGUMENT: summary: say what you did (1-4000 characters)"))) };
                let (tx, rx) = oneshot::channel();
                if self.em.tx.lock().unwrap().send(Msg::HandBack(summary.into(), tx)).is_err() { return Some(Err(anyhow::anyhow!("Heimdall is stopping"))); }
                Some(rx.await.context("Heimdall did not answer the hand-back"))
            }
            _ => None,
        }
    }
    fn record(&self, name: &str, args: &Value, result: &std::result::Result<String, String>) {
        self.em.last_call_ms.store(agent::now_ms(), Ordering::Release);
        if matches!(name, "emergency_status" | "emergency_end") { return; }
        self.side.tool_call(name, args, result);
        let what = describe(name, args);
        let outcome = match result { Ok(t) => bounded(t, 200), Err(e) => format!("failed: {}", bounded(e, 200)) };
        self.em.set(|s| { if s.did.len() < 50 { s.did.push(format!("{what} → {outcome}")); } });
        self.side.record(&format!("{}{}", agent::notice::EMERGENCY_COVER, uuid::Uuid::new_v4()), &agent::identity::names_only(&format!("[Relay agent] {what} → {outcome}"), &agent::identity::current()));
    }
}
/// (total context continuity, no hand-off summaries): Pi's session mirrors the relay thread.
/// Every message in it goes in whole; each action the relay took becomes one plain line with its result, never a
/// replayed tool call (the tool sets differ). Heimdall's own tools reach Pi live as `[Relay agent] …` lines (`Own`), so
/// their CLI commands are left out here; the relayed inputs are in Pi's session already (held), and the takeover's
/// first message is Pi's own context. Idempotent: each entry's id is its thread and position, and the position kept
/// in the state survives a restart.
async fn mirror(em: &Arc<Emergency>, t3: &dyn CoverT3, side: &dyn Side) {
    let _one = em.mirroring.lock().await;
    let Some((thread, after)) = em.state.lock().unwrap().mirrored.clone() else { return };
    let items = match t3.activity(&thread, after).await {
        Ok(items) => items,
        Err(e) => { eprintln!("emergency: the relay thread was not mirrored this time: {e:#}"); return }
    };
    let names = agent::identity::current();
    for item in items {
        if let Some(text) = mirror_text(&item, &em.cli) {
            let text = agent::identity::names_only(&text, &names);
            let parts = split_whole(&text, MIRROR_PART);
            let n = parts.len();
            for (i, part) in parts.into_iter().enumerate() {
                let part = if n > 1 { format!("{part}\n[part {} of {n}]", i + 1) } else { part };
                side.record(&format!("{}mirror:{}:{}:{i}", agent::notice::EMERGENCY_COVER, safe(&thread), item.position), &part);
            }
        }
        em.set(|s| if s.mirrored.as_ref().is_some_and(|(t, _)| t == &thread) { s.mirrored = Some((thread.clone(), item.position)); });
    }
}
/// What one thread item becomes in Pi's session, `None` when it is already there or no message or action.
fn mirror_text(item: &Item, cli: &str) -> Option<String> {
    let text = item.text.trim();
    let bin = cli.split_whitespace().next().unwrap_or("heimdall-emergency").rsplit('/').next().unwrap_or("heimdall-emergency");
    let line = |s: &str| { let first = s.lines().next().unwrap_or("").trim_start_matches("$ "); bounded(first, 160) };
    match item.kind.as_str() {
        "user_message" if text.starts_with("[For Heimdall,") || text.starts_with(TOOLS_NOTE) || text.starts_with(em::PROMPT_START) || text.starts_with(em::OPENING) || text.starts_with(em::OPENING_RELAY) => None,
        "user_message" => Some(format!("[Heimdall to the relay agent] {text}")),
        "assistant_message" if text.is_empty() => None,
        "assistant_message" => Some(format!("[Relay agent] {text}")),
        // T3's own bookkeeping, not the agent's actions (live smoke 27 Oct: "Preparing workspace", "checkpoint []").
        "reasoning" | "checkpoint" => None,
        "command_execution" if text.starts_with("$ Preparing workspace") => None,
        // A tool of its harness's own (ToolSearch, an MCP tool): its name, never its JSON.
        "dynamic_tool" => {
            let v: Value = serde_json::from_str(text).unwrap_or(Value::Null);
            let name = v["toolName"].as_str().unwrap_or("a tool");
            // Heimdall's own tools, through his MCP server or the CLI: recorded live as they ran.
            if name.contains(bin) || name.starts_with("mcp__heimdall__") { return None; }
            Some(format!("[Relay agent used {name}] → {}", item.status))
        }
        "command_execution" if text.contains(bin) => None,
        "command_execution" => Some(format!("[Relay agent ran] {} → {}", line(text), item.status)),
        kind if text.is_empty() => Some(format!("[Relay agent: {}] → {}", kind.replace('_', " "), item.status)),
        kind => Some(format!("[Relay agent: {}] {} → {}", kind.replace('_', " "), line(text), item.status)),
    }
}
/// `text` in parts of at most `max` bytes, on character boundaries, nothing left out.
fn split_whole(text: &str, max: usize) -> Vec<String> {
    let mut parts = Vec::new();
    let mut rest = text;
    while rest.len() > max {
        let mut cut = max;
        while !rest.is_char_boundary(cut) { cut -= 1; }
        parts.push(rest[..cut].to_owned());
        rest = &rest[cut..];
    }
    parts.push(rest.to_owned());
    parts
}
/// One line for a cover agent's call: the tool and its words.
fn describe(name: &str, a: &Value) -> String {
    let s = |k: &str| a[k].as_str().map(|v| bounded(v, 300)).unwrap_or_default();
    match name {
        "speak" => format!("said to User: \"{}\"", s("text")),
        "island_notice" => format!("showed '{}': {}", s("text"), a["detail"]["text"].as_str().map(|t| bounded(t, 300)).unwrap_or_default()),
        "bug_note" => format!("filed a bug note: {}", s("text")),
        "plan_edit" => format!("plan {} '{}'", s("action"), s("item")),
        "delegate_task" => format!("started task '{}' on {}", s("title"), a["target"]["model"].as_str().unwrap_or("?")),
        "t3_thread_send" => format!("sent to thread {}: {}", s("threadId"), s("message")),
        _ => format!("{name} {}", bounded(&a.to_string(), 300)),
    }
}
fn label(kind: &str) -> &str {
    match kind { "stop" => "User pressed Stop", "wake" => "User said", "typed" => "User typed", "answer" => "User clicked", "question" => "a T3 thread asks", "result" => "a task finished", "scheduled" => "a scheduled wake", _ => "a notice" }
}
fn bounded(s: &str, chars: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= chars { s.into() } else { format!("{}…", s.chars().take(chars).collect::<String>()) }
}
fn safe(id: &str) -> String { id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).take(80).collect() }
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(dir) = path.parent() { std::fs::create_dir_all(dir)?; }
    use std::io::Write;
    std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)?.write_all(bytes)?;
    Ok(())
}

/// Whether Pi's provider problems reach the menu: not while the relay answers (relay or emergency mode), when Pi is
/// only the record and its own calls are not the agent's.
pub fn reports_provider_problems(emergency: &Emergency) -> bool { !emergency.holding() }

/// What one Pi event tells the detector; `Some` when the agent has failed. A successful turn also ends a trial.
pub fn signal(d: &mut em::Detector, event: &agent::protocol::Event, emergency: &Emergency) -> Option<Cause> {
    use agent::protocol::Event;
    let now = agent::now_ms();
    match event {
        Event::Other(v) => {
            d.event(v, now);
            if v["type"] == "heimdall_quota" { d.quota_frame(v, now); }
            None
        }
        // The hand-back turn failed: the cover goes back on duty at once, without counting (review 27 Oct).
        Event::Complete { query: true, error: true, text, .. } if emergency.phase() == Phase::Trial => em::cause_of(text),
        Event::Complete { query: true, error: true, text, .. } => d.failed(text, now),
        Event::Complete { query: true, error: false, .. } => { d.answered(); emergency.answered(); None }
        _ => { d.event(&Value::Null, now); None }
    }
}
/// How long a relay choice waits for a stale quota reading.
const QUOTA_WAIT: Duration = Duration::from_secs(20);
/// The oldest reading a relay choice takes (the relay's own reader reused a scan for a minute).
const QUOTA_AGE: Duration = Duration::from_secs(60);
/// The production T3: the app's own outside-client connection (crates/t3).
pub struct T3Cover(pub Arc<t3::Client>);
#[async_trait]
impl CoverT3 for T3Cover {
    async fn catalog(&self) -> Result<Value> { self.0.call("orchestrator_capabilities", json!({})).await }
    /// The background jobs' reader (`agent::accounts`, `background_fallback::quota_board`): a reading at most a minute
    /// old, refreshed and waited for up to `QUOTA_WAIT`; none (a failed read, the public edition): unknown.
    async fn quotas(&self) -> Quotas {
        let Some(board) = crate::background_fallback::quota_board() else { return Quotas::new() };
        tokio::task::spawn_blocking(move || board.fresh(QUOTA_AGE, QUOTA_WAIT)).await.ok().flatten()
            .map(|a| routing::quotas_from(&a)).unwrap_or_default()
    }
    async fn launch(&self, title: &str, message: &str, h: &Harness) -> Result<(String, Option<String>)> {
        let project = self.vault_project().await?;
        let args = json!({"projectId":project,"title":title,"message":message,"modelSelection":{"instanceId":h.instance,"model":h.model},"runtimeMode":"full-access","workspaceStrategy":{"type":"root"}});
        let r = self.0.call_deadline("t3_thread_launch", args, Duration::from_secs(60)).await?;
        Ok((r["threadId"].as_str().context("T3 launch lacks a thread id")?.into(), r["runId"].as_str().map(str::to_owned)))
    }
    async fn send(&self, thread: &str, message: &str) -> Result<Option<String>> {
        let send = |mode: &str| self.0.call("t3_thread_send", json!({"threadId":thread,"message":message,"mode":mode,"clientRequestId":uuid::Uuid::new_v4().to_string()}));
        let r = match send("auto").await {
            // Live smoke 27 Oct: right after the user's Stop interrupted it, a run "is stopping and cannot be steered"; the
            // message goes in as the next turn instead of waiting 30 s for a retry.
            Err(e) if format!("{e:#}").contains("cannot be steered") => send("queue").await?,
            r => r?,
        };
        Ok(r["runId"].as_str().map(str::to_owned))
    }
    async fn outcome(&self, thread: &str, run: Option<&str>, wait: Duration) -> Result<Run> {
        let run = match run {
            Some(r) => r.to_owned(),
            None => self.0.call("t3_thread_read", json!({"threadId":thread,"limit":1})).await?["thread"]["latestRunId"].as_str().context("no run yet")?.to_owned(),
        };
        let r = self.0.call_deadline("t3_thread_wait", json!({"threadId":thread,"runId":run,"timeoutMs":wait.as_millis() as u64}), wait + Duration::from_secs(15)).await?;
        if r["timedOut"] == true { return Ok(Run::Working); }
        let status = r["status"].as_str().unwrap_or("");
        let reply = self.latest_reply(thread).await.ok().flatten();
        Ok(match status {
            "completed" => match &reply {
                // Claude Code ends a turn it could not run with the provider's error as its reply.
                Some(text) if em::harness_error_reply(text) => Run::Failed(self.failure(thread, &run).await.unwrap_or_else(|| text.clone())),
                _ => Run::Completed(reply),
            },
            "running" | "preparing" | "queued" => Run::Working,
            "failed" => ended("failed", self.failure(thread, &run).await),
            other => ended(other, None),
        })
    }
    async fn latest_reply(&self, thread: &str) -> Result<Option<String>> {
        let head = self.0.call("t3_thread_read", json!({"threadId":thread,"limit":1})).await?;
        let count = head["thread"]["itemCount"].as_u64().unwrap_or(0);
        let page = self.0.call("t3_thread_read", json!({"threadId":thread,"view":"messages","afterPosition":count.saturating_sub(8),"limit":20,"maxCharsPerItem":4000})).await?;
        Ok(page["items"].as_array().into_iter().flatten().rev().find(|i| i["type"] == "assistant_message").and_then(|i| i["text"].as_str()).map(str::to_owned))
    }
    async fn archive(&self, thread: &str) -> Result<()> { self.0.call("t3_thread_organize", json!({"threadId":thread,"action":"archive"})).await.map(|_| ()) }
    async fn interrupt(&self, thread: &str) -> Result<()> { self.0.call("t3_thread_interrupt", json!({"threadId":thread})).await.map(|_| ()) }
    async fn activity(&self, thread: &str, after: u64) -> Result<Vec<Item>> {
        let mut out = Vec::new();
        let mut position = after;
        for _ in 0..MIRROR_PAGES {
            let page = self.0.call("t3_thread_read", json!({"threadId":thread,"view":"activity","afterPosition":position,"limit":100,"maxCharsPerItem":50_000,"runLimit":1})).await?;
            for raw in page["items"].as_array().into_iter().flatten() {
                let Some(at) = raw["position"].as_u64() else { continue };
                // A running item is read again once it ends (its text is not whole yet).
                if raw["status"] == "running" || raw["status"] == "in_progress" { return Ok(out); }
                let mut text = raw["text"].as_str().unwrap_or("").to_owned();
                let mut item = raw.clone();
                while item["textTruncated"] == true {
                    let Some(offset) = item["nextTextOffset"].as_u64() else { break };
                    let r = self.0.call("t3_thread_read", json!({"threadId":thread,"view":"activity","itemId":item["itemId"],"textOffset":offset,"limit":1,"maxCharsPerItem":50_000,"runLimit":1})).await?;
                    let Some(next) = r["items"].as_array().and_then(|a| a.first()).cloned() else { break };
                    if next["nextTextOffset"].as_u64().is_some_and(|n| n <= offset) { break; }
                    text.push_str(next["text"].as_str().unwrap_or(""));
                    item = next;
                }
                out.push(Item { position: at, kind: raw["type"].as_str().unwrap_or("").into(), text, status: raw["status"].as_str().unwrap_or("").into() });
                position = at;
            }
            if page["hasMore"] != true { break; }
            if let Some(next) = page["nextPosition"].as_u64() { position = position.max(next); }
        }
        Ok(out)
    }
}
/// The last timeline items a failure is looked for in.
const FAILURE_ITEMS: u64 = 16;
impl T3Cover {
    /// Why `run` failed, as T3 0.1.6 keeps it in the thread's activity (read live, 2 Nov): its `system_notice` (a
    /// usage limit, its id ending `usage-limit:…:<window>:<reset seconds>`), its `error` items ("Usage limit
    /// reached: …", "Provider error: API error (status 402 Payment Required): …") and Claude Code's reply ("You've
    /// hit your weekly limit · resets 1pm"), one per line for `agent::limits::parse`. `None`: nothing there.
    async fn failure(&self, thread: &str, run: &str) -> Option<String> {
        let head = self.0.call("t3_thread_read", json!({"threadId":thread,"limit":1})).await.ok()?;
        let count = head["thread"]["itemCount"].as_u64().unwrap_or(0);
        let page = self.0.call("t3_thread_read", json!({"threadId":thread,"view":"activity","afterPosition":count.saturating_sub(FAILURE_ITEMS + 1),"limit":FAILURE_ITEMS,"maxCharsPerItem":2000,"runLimit":1})).await.ok()?;
        failure_text(page["items"].as_array()?, run)
    }
    /// The home folder's registered project (the deepest one containing Heimdall's home, docs/home.md).
    async fn vault_project(&self) -> Result<String> {
        let vault = std::fs::canonicalize(heimdall_home::home_dir())?;
        let page = self.0.call("t3_project_list", json!({"limit":100})).await?;
        page["projects"].as_array().into_iter().flatten()
            .filter_map(|p| Some((p["id"].as_str()?, std::fs::canonicalize(p["workspaceRoot"].as_str()?).ok()?)))
            .filter(|(_, root)| vault.starts_with(root))
            .max_by_key(|(_, root)| root.as_os_str().len())
            .map(|(id, _)| id.to_owned()).context("PROJECT_NOT_FOUND: no T3 project contains Heimdall's home folder")
    }
}

/// A run that ended without completing, with what T3 kept about why (bug note 2026-11-02T14-47-37: "run failed" was
/// all Heimdall read). Only a failed run carries it: an interrupted one (the user's Stop) stays "run interrupted", which is
/// no failure (review 2 Nov: the run's prose in its place read as the account failing).
fn ended(status: &str, why: Option<String>) -> Run {
    match why.filter(|_| status == "failed") {
        Some(why) => Run::Failed(format!("run failed: {why}")),
        None => Run::Failed(format!("run {status}")),
    }
}
/// A failed run's own items as one text, oldest first: notices and errors with their title and id, and its replies.
fn failure_text(items: &[Value], run: &str) -> Option<String> {
    let lines: Vec<String> = items.iter().filter(|i| i["runId"].as_str().is_none_or(|r| r == run)).filter_map(|i| {
        let text = i["text"].as_str().unwrap_or("").trim();
        let title = i["title"].as_str().unwrap_or("").trim();
        match i["type"].as_str()? {
            "system_notice" | "error" | "notice" => Some(format!("{}{text} [{}]", if title.is_empty() || title == text { String::new() } else { format!("{title}: ") }, i["itemId"].as_str().unwrap_or(""))),
            // Only a reply that is the provider's error: a working run's prose is no failure (review 2 Nov).
            "assistant_message" if em::harness_error_reply(text) => Some(text.to_owned()),
            _ => None,
        }
    }).collect();
    (!lines.is_empty()).then(|| lines.join("\n"))
}

/// The production probe: ChatGPT gets one tiny request on the agent's own model (the only check that sees the plan's
/// limit); other API providers a sign-in check; the local engine nothing (the hand-back turn is the check).
pub struct ProviderProbe { pub data: PathBuf }
#[async_trait]
impl MainProbe for ProviderProbe {
    async fn probe(&self, model: Option<&agent::provider::ApiModel>) -> std::result::Result<(), String> {
        let Some(model) = model else { return Ok(()) };
        let p = agent::provider::provider(&model.provider).ok_or("unknown provider")?;
        if p.id == agent::provider::CHATGPT {
            let session = agent::console::access_token(p, &self.data.join("agent"), agent::console::Budget::APP).await.map_err(|e| format!("{e:#}"))?;
            return agent::chatgpt::probe(agent::chatgpt::API_BASE, &session.access, &model.id).await.map_err(|e| format!("{e:#}"));
        }
        let credential = agent::provider::credential(p).map_err(|e| format!("{e:#}"))?;
        crate::api_providers::check_credential(p, &credential, &self.data, None).await.map(|_| ())
    }
}

/// The app side: the island, speech through `speak`, context through the outbox, the supervisor's command channel.
pub struct AppSide { pub tools: std::sync::Weak<crate::bridge::Tools>, pub data: PathBuf, pub session: Mutex<Option<PathBuf>> }
impl Side for AppSide {
    fn cue(&self, cue: ui::HealthCue) {
        if let Some(t) = self.tools.upgrade() { t.emergency_cue(cue); }
    }
    fn indicator(&self, state: ui::RelayIndicator) {
        if let Some(t) = self.tools.upgrade() { t.emergency_ui(ui::UiUpdate::AgentRelay(state)); }
    }
    fn relay_models(&self, models: Vec<ui::RelayModel>) {
        if let Some(t) = self.tools.upgrade() { t.emergency_ui(ui::UiUpdate::RelayModels(models)); }
    }
    fn t3_instances(&self, instances: Vec<ui::T3Instance>) {
        if let Some(t) = self.tools.upgrade() { t.emergency_ui(ui::UiUpdate::T3Instances(instances)); }
    }
    fn say(&self, text: &str) {
        if let Some(t) = self.tools.upgrade() { if let Err(e) = t.emergency_say(text) { eprintln!("emergency: '{text}' not said: {e:#}"); } }
    }
    fn record(&self, id: &str, text: &str) {
        if let Some(t) = self.tools.upgrade() { if let Err(e) = t.emergency_record(id, text) { eprintln!("emergency: record not kept: {e:#}"); } }
    }
    fn release(&self, back: Option<String>) {
        if let Some(t) = self.tools.upgrade() { t.emergency_release(back); }
    }
    fn late(&self, r: &Relayed) {
        let text = agent::identity::names_only(&format!("[Arrived while the relay was ending; nobody answered it] {}: {}", label(&r.kind), r.text), &agent::identity::current());
        if let Some(t) = self.tools.upgrade() { if let Err(e) = t.emergency_record(&format!("{}{}", agent::notice::EMERGENCY_LATE, r.id), &text) { eprintln!("emergency: late input not passed on: {e:#}"); } }
    }
    fn unfinished(&self, r: &Relayed) {
        let images = if r.images.is_empty() { String::new() } else { format!(" ({} image(s) came with it; ask User to send them again if you need them)", r.images.len()) };
        let text = agent::identity::names_only(&format!("[The relay had not finished this when User switched back to you; what it did is in your conversation above, and its reply was not given. Finish what is left] {}: {}{images}", label(&r.kind), r.text), &agent::identity::current());
        if let Some(t) = self.tools.upgrade() { if let Err(e) = t.emergency_record(&format!("{}{}", agent::notice::EMERGENCY_LATE, r.id), &text) { eprintln!("emergency: unfinished input not passed on: {e:#}"); } }
    }
    fn context(&self, bytes: usize) -> (String, String, String) {
        let conversation = self.session.lock().unwrap().as_ref().and_then(|p| conversation_tail(p, bytes).ok()).unwrap_or_default();
        let tasks = self.tools.upgrade().and_then(|t| t.emergency_tasks()).unwrap_or_default();
        (conversation, plan_text(&self.data), tasks)
    }
    fn sent(&self) {
        if let Some(t) = self.tools.upgrade() { t.emergency_ui(ui::UiUpdate::AgentSent); }
    }
    fn working(&self, on: bool) {
        if let Some(t) = self.tools.upgrade() { t.emergency_working(on); }
    }
    fn reply(&self, text: &str) {
        if let Some(t) = self.tools.upgrade() { if let Err(e) = t.agent_final_answer(text, "stop") { eprintln!("emergency: the relay's reply not passed on: {e:#}"); } }
    }
    fn tool_call(&self, name: &str, args: &Value, result: &std::result::Result<String, String>) {
        crate::chat_history::relay_call(name, args, result, agent::now_ms());
    }
    fn pause_background(&self, reset: Option<Option<u64>>) { crate::background_gate::emergency_pause(reset); }
    fn no_relay(&self, reset: Option<u64>) {
        if let Some(t) = self.tools.upgrade() { crate::emergency_local::offer(&t, reset); }
    }
    fn restore_model(&self, model: &agent::provider::ApiModel) {
        // Asked, never switched (30 Oct 01:2x): the island's "Back to …?" Yes picks it (`pipeline/actions.rs`).
        if let Some(t) = self.tools.upgrade() { crate::emergency_local::offer_back(&t, model); }
    }
    fn notice(&self, card: ui::agent_notice::AgentNotice) {
        if let Some(t) = self.tools.upgrade() { t.emergency_ui(ui::UiUpdate::AgentNotice(card)); }
    }
    fn dismiss(&self, id: &str) {
        if let Some(t) = self.tools.upgrade() { t.emergency_ui(ui::UiUpdate::DismissAgentNotice(id.into())); }
    }
    /// The engine on this Mac, its model ready or still to download (no network in a quick scan: the catalog's size).
    fn local_engine(&self) -> Option<rec::Local> {
        let model = crate::settings::Settings::load(&self.data.join("settings.json")).map(|s| s.engine_model).unwrap_or_default();
        match crate::emergency_local::engine(&self.data, &model, &crate::emergency_local::hub(), &|_, _| None) {
            crate::emergency_local::Engine::Ready { model, label } => Some(rec::Local { model, label, ready: true, download_bytes: None }),
            crate::emergency_local::Engine::Download { model, label, download_bytes, .. } => Some(rec::Local { model, label, ready: false, download_bytes }),
            crate::emergency_local::Engine::Missing => None,
        }
    }
    /// Background helpers on that account move off it until its reset (`background_fallback`).
    fn account_spent(&self, instance: &str, reset_ms: Option<u64>) {
        crate::background_gate::quota_spent(&format!("{}{instance}", ui::T3_PROVIDER_PREFIX), reset_ms, agent::now_ms());
    }
    /// One settings change (one restart): relay mode on this account, with its model when it is a Claude account (the
    /// menu's relay model is a Claude model).
    fn keep_relay(&self, route: &Route) {
        let model = route.driver.to_ascii_lowercase().contains("claude").then(|| route.model.clone());
        if let Some(t) = self.tools.upgrade() { t.emergency_select(ui::UiAction::SelectRelayRoute { model, account: route.instance.clone() }); }
    }
}
/// The relay's system prompt for this build and profile (`agent::relay_prompt`): Heimdall's own with Pi's fills, the
/// profile's skills index, and the CLI line. The fills take this process's executable, as Pi's launch does.
pub fn relay_prompt(data: &Path, cli: &str) -> String {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("heimdall"));
    agent::relay_prompt::text(&exe, data, cli)
}
/// The protocol the cover agent reads: the bundle's copy (Contents/Resources/emergency.md, this build's), else the
/// repository's.
pub fn docs_path() -> String {
    let bundled = std::env::current_exe().ok().and_then(|e| e.parent()?.parent().map(|c| c.join("Resources/emergency.md"))).filter(|p| p.exists());
    bundled.unwrap_or_else(|| agent::runtime::checkout_root().join("docs/emergency.md")).display().to_string()
}
/// The conversation's last turns as `To Heimdall:` / `Heimdall:` lines, the last `bytes` of them, with everything
/// that reached it as context: held input relayed to a relay (`[Relayed …]`), the relay's mirrored messages and
/// actions (`[Relay agent] …`), notices. Bug note 2026-11-02T14-47-43: only Pi's own turns were read, and since relay
/// mode began on 30 Oct the session held nothing else, so every hand-over on 2 Nov ended at 30 Oct 06:36.
pub fn conversation_tail(session: &Path, bytes: usize) -> Result<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(session)?;
    let start = f.metadata()?.len().saturating_sub((bytes as u64 * 4).max(512 * 1024));
    f.seek(SeekFrom::Start(start))?;
    let mut raw = vec![];
    f.read_to_end(&mut raw)?;
    let text = String::from_utf8_lossy(&raw);
    let text = if start > 0 { text.split_once('\n').map_or("", |(_, t)| t) } else { &text };
    let mut view = agent::view::SessionView::default();
    let mut lines: Vec<String> = Vec::new();
    for line in text.split('\n') {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        if v["type"] == "custom_message" && v["customType"] == "heimdall_context" {
            let time = v["timestamp"].as_str().and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok()).map_or(0, |t| t.timestamp_millis() as u64);
            if let Some(entry) = context_line(v["content"].as_str().unwrap_or("")) { lines.push(format!("[{}] {entry}", em::clock(time))); }
            continue;
        }
        for e in view.history_line(&v) {
            // Messages whole; each of Heimdall's tool actions one line with its result (the user, 20:45: never a replayed call).
            let text = e.text.trim();
            match &e.kind {
                agent::view::Kind::Prompt { .. } | agent::view::Kind::Reply if text.is_empty() => continue,
                agent::view::Kind::Prompt { .. } => lines.push(format!("[{}] To Heimdall: {text}", em::clock(e.time_ms))),
                agent::view::Kind::Reply => lines.push(format!("[{}] Heimdall: {text}", em::clock(e.time_ms))),
                agent::view::Kind::ToolCall { name } => lines.push(format!("[{}] Heimdall used {name}: {}", em::clock(e.time_ms), bounded(&text.replace('\n', " "), 160))),
                agent::view::Kind::ToolResult { error, .. } => {
                    let result = format!(" → {}{}", if *error { "failed: " } else { "" }, bounded(&text.replace('\n', " "), 120));
                    match lines.last_mut() { Some(last) if last.contains(" Heimdall used ") && !last.contains(" → ") => last.push_str(&result), _ => {} }
                }
                agent::view::Kind::Failure => {}
            }
        }
    }
    Ok(em::tail_bytes(&lines.join("\n"), bytes))
}
/// One context entry of Pi's session as a conversation line: input held for the relay as what reached Heimdall, the
/// relay's mirrored lines as they are, other notices by their text; the delivery header goes.
fn context_line(content: &str) -> Option<String> {
    let (relayed, body) = match content.strip_prefix(em::HELD) { Some(rest) => (true, rest.trim_start()), None => (false, content) };
    // "[Heimdall notice: <id>]" heads most entries: the id is bookkeeping.
    let body = match body.strip_prefix("[Heimdall notice: ").and_then(|r| r.split_once("]\n")) { Some((_, rest)) => rest, None => body };
    let body = agent::view::clean(body);
    // The user's typed words carry the delivery's "[Heimdall typed message]" line: the words alone.
    let body = agent::view::typed_text(&body).unwrap_or(&body).trim();
    if body.is_empty() { return None; }
    Some(if relayed { format!("To Heimdall (relayed to the relay agent): {body}") } else { body.to_owned() })
}
/// The plan lists' open items, bounded (`em::PLAN_BYTES`).
pub fn plan_text(data: &Path) -> String {
    let (root, _) = crate::islands::live::root_for(data, crate::settings::profile_data().as_deref());
    let mut out = Vec::new();
    for (id, title, _) in crate::islands::plan::LISTS {
        if id == "settled" { continue; }
        let Ok(bytes) = std::fs::read(root.file(id)) else { continue };
        let Ok(Value::Object(o)) = serde_json::from_slice::<Value>(&bytes) else { continue };
        let items: Vec<String> = crate::islands::plan::items(&o).iter().filter(|i| i.get("done") != Some(&json!(true)))
            .filter_map(|i| i.get("title").and_then(Value::as_str).map(|t| format!("- {t}"))).collect();
        if !items.is_empty() { out.push(format!("{title}:\n{}", items.join("\n"))); }
    }
    let text = out.join("\n");
    if text.len() > em::PLAN_BYTES { em::tail_bytes(&text, em::PLAN_BYTES) } else { text }
}

#[cfg(test)]
#[path = "emergency_tests.rs"]
mod tests;
