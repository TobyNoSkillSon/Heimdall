//! Background jobs keep working when their backend cannot: a simpler form of the main agent's
//! emergency mode. When a task's pick is spent (a usage limit, a 429, Claude's "usage limit reached", Codex's plan
//! limit, credit) or unavailable (T3 not running or not connected, a refused connection, a 5xx), the task's next call
//! goes to the next backend in `chain`, and it comes back to its pick at the first call after the pick is usable again.
//!
//! - Decided when a job resolves its endpoint (`background_model::task_endpoint`, `endpoint`, `tool_endpoint`: `route`),
//!   which every job does right before its call: a call that started on a backend finishes or fails there, and only the
//!   next one moves. Every call stays one-off (one request, or one short-lived T3 thread).
//! - What is spent or down is `background_gate`'s episodes, marked where a call fails (`background_call::post`,
//!   `background_call::t3`): skipped until the stated reset, else until a probe is due (10 minutes, doubling to an
//!   hour). The Claude and Codex accounts' own quota (`agent::accounts`, read by `Tools/subscription_quota.py` at most
//!   every 5 minutes, only while some task needs a fallback) also skips a spent account and orders the Claude accounts.
//! - The order (`chain`): (a) the same Claude model on another Claude Code account (T3 Claude instances; with more than
//!   one left, the one whose quota renews soonest first, `accounts::earliest_reset_first`); (b) Codex through the ChatGPT
//!   sign-in (Responses), on the shared Background Tasks ▸ Model when that is a Codex model, else gpt-6-luna; never a
//!   Codex T3 instance; (c) OpenCode Go, then the built-in engine (when it is on), then the Anthropic API key last, and
//!   only when the user allowed a paid fallback (`Settings::background_fallback_paid`, off by default: money is his call). A backend that is not configured (no credential, T3 not connected) is skipped; nothing usable: no call.
//!   A pick that was never connected (no key, signed out) makes no call, as before; only T3 not connected counts as
//!   unavailable and falls back.
//! - A tool loop (Suggestions, deep search) never runs on T3 (`background_call::t3::NoToolsOnT3`): its chain has no T3
//!   step. Only a pick on a cloud subscription or API (a T3 instance, the ChatGPT sign-in, OpenCode Go, the Anthropic
//!   API) falls back: the built-in engine, Custom (OpenAI- or Anthropic-compatible) and Own Engine may be on this Mac or
//!   the private network, and a local choice never sends the plan or a transcript to the cloud (`falls_back`).
//! - Deep search and the shared `background_model::endpoint`/`tool_endpoint` are not tasks: they stay on Background
//!   Tasks ▸ Model, with no fallback and no sign.
//! - Each move is logged; a task off its pick is listed in `UiUpdate::BackgroundRoutes` (Background Tasks' small sign),
//!   and the list is sent again when it returns. There is no card and no failure notice.
//! - Emergency mode's pause (`background_gate::emergency_pause`) stays: while the main agent is covered no background
//!   job runs. Outside it a spent T3 instance no longer stops its task: the task falls back.

use crate::background_model::{BackgroundModel, Task};
use crate::plan_refresher::Endpoint;
use agent::accounts::{self, AccountQuota};
use std::collections::BTreeMap;
use std::sync::Arc;

/// Codex's model when the shared one is not a Codex model.
pub const CODEX_DEFAULT: &str = "gpt-6-luna";
/// T3's Claude instances before T3 listed its own (`background_call::t3::publish_catalog`).
pub const CLAUDE_ACCOUNTS: [&str; 2] = ["claudeAgent", "claudeAgent_claude_code_2"];

/// One backend a task may run on, and why it is in the chain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Step {
    pub model: BackgroundModel,
    pub why: &'static str,
}

/// What the chain is built from (the app's state in `route`, a table in tests).
#[derive(Clone, Debug)]
pub struct World {
    /// T3's Claude instances, in T3's order.
    pub claude: Vec<String>,
    /// The accounts' quota, when read.
    pub quota: Option<Vec<AccountQuota>>,
    /// Background Tasks ▸ Model.
    pub shared: BackgroundModel,
    /// The model OpenCode Go would run (`None`: not connected or no catalog).
    pub opencode: Option<String>,
    /// The built-in engine is on.
    pub engine: bool,
    /// The user allowed the paid step (the Anthropic API key).
    pub paid: bool,
    pub now_ms: u64,
}

fn t3(m: &BackgroundModel) -> Option<&str> { ui::t3_instance(&m.provider) }
fn bm(provider: impl Into<String>, id: impl Into<String>) -> BackgroundModel { BackgroundModel { provider: provider.into(), id: id.into() } }
/// "t3:" names the default instance: one spelling per backend.
fn norm(m: &BackgroundModel) -> BackgroundModel {
    match t3(m) { Some(i) => bm(format!("{}{i}", ui::T3_PROVIDER_PREFIX), m.id.clone()), None => m.clone() }
}

/// The Claude accounts to try, in order: by `accounts::earliest_reset_first` when their quota is known (spent ones
/// left out), the rest (no quota read for them) after, in T3's order.
fn claude_order(w: &World) -> Vec<String> {
    let Some(q) = &w.quota else { return w.claude.clone() };
    let ranked = accounts::earliest_reset_first(q, w.now_ms);
    let mut out: Vec<String> = ranked.into_iter().filter(|i| w.claude.contains(i)).collect();
    out.extend(w.claude.iter().filter(|i| !q.iter().any(|a| &a.instance == *i)).cloned());
    out
}

/// Whether the accounts' quota allows `m` (a T3 Claude instance or the Codex sign-in whose account is spent: no).
fn quota_ok(m: &BackgroundModel, w: &World) -> bool {
    let Some(q) = &w.quota else { return true };
    let account = match t3(m) { Some(i) => i, None if m.provider == agent::provider::CHATGPT => accounts::CODEX, None => return true };
    q.iter().find(|a| a.instance == account).is_none_or(|a| accounts::usable(a, w.now_ms))
}

/// Whether a pick may fall back at all: only one on a cloud subscription or API. The built-in engine, Custom and Own
/// Engine (any other provider) may be local or on the private network: never moved to the cloud (review 30 Oct).
pub fn falls_back(pick: &BackgroundModel) -> bool {
    t3(pick).is_some() || [agent::provider::CHATGPT, agent::provider::OPENCODE_GO, agent::provider::ANTHROPIC].contains(&pick.provider.as_str())
}

/// Every backend `pick` may run on, its pick first (module doc). `tools`: a tool loop (no T3 step).
pub fn chain(pick: &BackgroundModel, tools: bool, w: &World) -> Vec<Step> {
    let pick = norm(pick);
    let mut out: Vec<Step> = Vec::new();
    if !(tools && t3(&pick).is_some()) { out.push(Step { model: pick.clone(), why: "its pick" }); }
    if !falls_back(&pick) { return out }
    let claude = |id: &str| id.starts_with("claude-");
    let claude_id = if claude(&pick.id) { pick.id.clone() } else if claude(&w.shared.id) { w.shared.id.clone() } else { agent::provider::HAIKU.to_owned() };
    if !tools {
        for i in claude_order(w) { out.push(Step { model: bm(format!("{}{i}", ui::T3_PROVIDER_PREFIX), claude_id.clone()), why: "another Claude Code account" }); }
    }
    let codex = if w.shared.provider == agent::provider::CHATGPT { w.shared.id.clone() } else { CODEX_DEFAULT.to_owned() };
    out.push(Step { model: bm(agent::provider::CHATGPT, codex), why: "Codex" });
    if let Some(id) = &w.opencode { out.push(Step { model: bm(agent::provider::OPENCODE_GO, id.clone()), why: "OpenCode Go" }); }
    if w.engine { out.push(Step { model: bm(ui::LOCAL_ENGINE, ""), why: "the local engine" }); }
    if w.paid { out.push(Step { model: bm(agent::provider::ANTHROPIC, claude_id), why: "the Anthropic API key (paid)" }); }
    // One step per backend: the pick's own place wins.
    let mut seen: Vec<BackgroundModel> = Vec::new();
    out.retain(|s| { let new = !seen.contains(&s.model); seen.push(s.model.clone()); new });
    out.retain(|s| s.model == pick || quota_ok(&s.model, w));
    out
}

/// The first step that is configured (`endpoint` gives a credential) and usable (`usable`: no episode, quota left).
pub fn choose(steps: &[Step], w: &World, endpoint: &dyn Fn(&BackgroundModel) -> Endpoint, usable: &dyn Fn(&str) -> bool) -> Option<(Step, Endpoint)> {
    steps.iter().find_map(|s| {
        if !quota_ok(&s.model, w) { return None }
        let e = endpoint(&s.model);
        (e.key.is_some() && usable(e.provider)).then(|| (s.clone(), e))
    })
}

// ---------------------------------------------------------------------------------------------------------
// The app's state.

/// The tasks off their pick: (the pick it left, what it runs on now).
type Routes = BTreeMap<Task, (BackgroundModel, BackgroundModel)>;
/// Where a change of routes goes (the pipeline sends `UiUpdate::BackgroundRoutes`).
pub type Sink = Arc<dyn Fn(Vec<(Task, String, String)>) + Send + Sync>;

#[cfg(not(test))]
mod state {
    use super::*;
    static ROUTES: std::sync::Mutex<Routes> = std::sync::Mutex::new(BTreeMap::new());
    static SINK: std::sync::RwLock<Option<Sink>> = std::sync::RwLock::new(None);
    static BOARD: std::sync::RwLock<Option<Arc<accounts::Board>>> = std::sync::RwLock::new(None);
    pub fn routes<T>(f: impl FnOnce(&mut Routes) -> T) -> T { f(&mut ROUTES.lock().unwrap()) }
    pub fn sink() -> Option<Sink> { SINK.read().unwrap().clone() }
    pub fn set_sink(s: Sink) { *SINK.write().unwrap() = Some(s) }
    pub fn board() -> Option<Arc<accounts::Board>> { BOARD.read().unwrap().clone() }
    pub fn set_board(b: Arc<accounts::Board>) { *BOARD.write().unwrap() = Some(b) }
}
// Tests: per thread, like the published model.
#[cfg(test)]
mod state {
    use super::*;
    thread_local! {
        static ROUTES: std::cell::RefCell<Routes> = const { std::cell::RefCell::new(BTreeMap::new()) };
        static SINK: std::cell::RefCell<Option<Sink>> = const { std::cell::RefCell::new(None) };
        static BOARD: std::cell::RefCell<Option<Arc<accounts::Board>>> = const { std::cell::RefCell::new(None) };
    }
    pub fn routes<T>(f: impl FnOnce(&mut Routes) -> T) -> T { ROUTES.with(|r| f(&mut r.borrow_mut())) }
    pub fn sink() -> Option<Sink> { SINK.with(|s| s.borrow().clone()) }
    pub fn set_sink(s: Sink) { SINK.with(|c| *c.borrow_mut() = Some(s)) }
    pub fn board() -> Option<Arc<accounts::Board>> { BOARD.with(|b| b.borrow().clone()) }
    pub fn set_board(b: Arc<accounts::Board>) { BOARD.with(|c| *c.borrow_mut() = Some(b)) }
}

/// The pipeline's `UiUpdate::BackgroundRoutes`.
pub fn set_routes_sink(s: Sink) { state::set_sink(s) }
/// The accounts' quota reader (the personal edition's `Tools/subscription_quota.py`).
pub fn set_quota_board(b: Arc<accounts::Board>) { state::set_board(b) }
/// The same reader for the relay's account choice (`emergency::T3Cover::quotas`): one script run serves both.
pub fn quota_board() -> Option<Arc<accounts::Board>> { state::board() }
/// The tasks off their pick and what each runs on now.
pub fn routes() -> Vec<(Task, String, String)> { state::routes(|r| listed(r)) }
fn listed(r: &Routes) -> Vec<(Task, String, String)> { r.iter().map(|(t, (_, m))| (*t, m.provider.clone(), m.id.clone())).collect() }

/// Changes the routes and, when they changed, sends the new list from inside the same lock: lists go out in the order
/// the changes happened, so an older one never arrives last (review 30 Oct).
fn change(f: impl FnOnce(&mut Routes) -> bool) {
    state::routes(|r| { if f(r) { if let Some(s) = state::sink() { s(listed(r)) } } });
}

fn set_route(task: Option<Task>, now: Option<(&BackgroundModel, &BackgroundModel)>) {
    let Some(task) = task else { return };
    change(|r| match now {
        Some((pick, m)) if r.get(&task).map(|(_, x)| x) != Some(m) => { r.insert(task, (pick.clone(), m.clone())); true }
        None => r.remove(&task).is_some(),
        _ => false,
    });
}

/// Drops the routes that no longer hold, without waiting for the task's next call (review 30 Oct: a stale sign): the
/// task turned off, its pick changed (a new choice), or its pick usable again (a probe succeeded,
/// `background_gate::quota_back`). Called on every settings change and when a backend answers again.
pub fn refresh_routes() {
    let now = agent::now_ms();
    change(|r| {
        let before = r.len();
        r.retain(|task, (pick, _)| {
            let keep = crate::background_model::enabled(*task) && norm(&crate::background_model::task_model(*task)) == *pick
                && !crate::background_gate::usable(&pick.provider, now);
            if !keep { eprintln!("Background tasks · {}: back on its pick, {}/{}", task.title(), pick.provider, pick.id); }
            keep
        });
        r.len() != before
    });
}

/// The app's state, without the accounts' quota (`with_quota` adds it only when the choice depends on it).
fn world(now_ms: u64) -> World {
    let mut claude: Vec<String> = crate::background_call::t3::instances().into_iter().filter(|i| i.driver.to_ascii_lowercase().contains("claude")).map(|i| i.instance).collect();
    if claude.is_empty() { claude = CLAUDE_ACCOUNTS.iter().map(|s| s.to_string()).collect(); }
    World { claude, quota: None, shared: crate::background_model::current(), opencode: crate::background_model::opencode_model(),
        engine: crate::background_model::engine_on(), paid: crate::background_model::fallback_paid(), now_ms }
}

/// Whether the quota reading could change this choice (review 30 Oct: the script starts two `claude` and a `codex`
/// process): the step chosen without it is a Claude account with another Claude account beside it, or Codex.
pub fn quota_matters(steps: &[Step], chosen: &Step) -> bool {
    let accounts = steps.iter().filter(|s| s.why == "another Claude Code account").count();
    (chosen.why == "another Claude Code account" && accounts >= 2) || chosen.model.provider == agent::provider::CHATGPT
}

fn clock(ms: u64) -> String {
    if ms == u64::MAX { return "it is back".into() }
    chrono::DateTime::from_timestamp_millis(ms as i64).map(|t| t.with_timezone(&chrono::Local).format("%H:%M").to_string()).unwrap_or_default()
}
fn label(task: Option<Task>) -> &'static str { task.map_or("Background Tasks ▸ Model", |t| t.title()) }

/// The endpoint `task`'s next call goes to: its pick when it is configured and usable (the fast path, nothing else
/// read), else the first usable step of its `chain`; nothing usable: an endpoint without a credential (no call). Blocking
/// (a ChatGPT token may be refreshed): call it where jobs resolve their endpoint, off the async runtime.
pub fn route(task: Option<Task>, pick: BackgroundModel, tools: bool) -> Endpoint {
    let resolve = |m: &BackgroundModel| crate::background_model::endpoint_for(m.clone());
    if pick.is_none() { return resolve(&pick) }
    let now = agent::now_ms();
    let pick = norm(&pick);
    let on_t3 = t3(&pick).is_some();
    let own = resolve(&pick);
    let episode = crate::background_gate::episode(own.provider, now);
    let blocked = episode.map(|(until, _)| until);
    if own.key.is_some() && blocked.is_none() && !(tools && on_t3) {
        if state::routes(|r| task.is_some_and(|t| r.contains_key(&t))) {
            eprintln!("Background tasks · {}: back on its pick, {}/{}", label(task), pick.provider, pick.id);
        }
        set_route(task, None);
        return own;
    }
    // A pick that was never connected (no key, signed out) is the user's to fix: no call, as before. T3 not connected is T3
    // being unavailable, and falls back.
    if own.key.is_none() && !on_t3 {
        set_route(task, None);
        return own;
    }
    let mut w = world(now);
    // The last quota reading as it is (free); a new one only when the choice depends on it (read at most every 20
    // minutes, on its own thread).
    w.quota = state::board().and_then(|b| b.cached());
    let mut steps = chain(&pick, tools, &w);
    let usable = |p: &str| crate::background_gate::usable(p, now);
    let mut chosen = choose(&steps, &w, &resolve, &usable);
    if chosen.as_ref().is_some_and(|(s, _)| quota_matters(&steps, s)) {
        if let Some(q) = state::board().and_then(|b| b.snapshot(now)) {
            w.quota = Some(q);
            steps = chain(&pick, tools, &w);
            chosen = choose(&steps, &w, &resolve, &usable);
        }
    }
    match chosen {
        Some((step, e)) if step.model == pick => { set_route(task, None); e }
        Some((step, e)) => {
            let already = state::routes(|r| task.and_then(|t| r.get(&t).map(|(_, m)| m.clone()))) == Some(step.model.clone());
            if !already {
                let why = match blocked {
                    Some(at) if episode.is_some_and(|(_, k)| k == "spent") => format!("has spent its quota until {}", clock(at)),
                    Some(at) => format!("is {} (next try {})", episode.map_or("down", |(_, k)| k), clock(at)),
                    None if tools && on_t3 => "is on T3, where a tool loop cannot run".into(),
                    None if own.key.is_none() => "is not connected".into(),
                    None => "has no quota left".into(),
                };
                eprintln!("Background tasks · {}: {}/{} {why}; its calls run on {}/{} ({}) until it is back",
                    label(task), pick.provider, pick.id, step.model.provider, step.model.id, step.why);
            }
            set_route(task, Some((&pick, &step.model)));
            e
        }
        None => {
            let had = state::routes(|r| task.is_some_and(|t| r.contains_key(&t)));
            set_route(task, None);
            // Nowhere else to go: a pick that is only down (not spent) is tried again, as before the fallback; being
            // down matters only when there is somewhere else to go.
            if own.key.is_some() && crate::background_gate::spent_until(own.provider, now).is_none() && !(tools && on_t3) {
                return own;
            }
            if had { eprintln!("Background tasks · {}: {}/{} cannot run and no other backend is usable; no call", label(task), pick.provider, pick.id); }
            Endpoint { key: None, ..own }
        }
    }
}

#[cfg(test)]
#[path = "background_fallback_tests.rs"]
mod tests;
