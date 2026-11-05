//! A background job's call on a harness T3 runs (Background Tasks ▸ Model "t3:<instance>", docs/background-call.md):
//! The user's Claude subscription through T3 Code, with no API key ("every background job is a short stateless call
//! through one shared call path whose backend is selectable", 30 Oct 01:08). `background_call::ask` comes here when the
//! endpoint's provider is a T3 instance; the jobs never see which backend ran.
//!
//! One call is one short-lived T3 thread:
//! - launched in the "Heimdall background" project (an empty folder under Heimdall's data folder, so no project's
//!   instructions reach the model and nothing lands in the user's own projects; a client T3 does not let make projects
//!   falls back to the project holding Heimdall's home, as the relay uses), titled `[heimdall-bg] <job>`, on the
//!   instance with `modelSelection {instanceId, model, options {effort}}`;
//! - in `approval-required`, T3's most restrictive mode: nothing runs without a yes. Heimdall's permission hook
//!   declines a prompt in a thread Heimdall launched (the client records the launch), and any tool item or a wait on an
//!   approval or question stops the run here at once (interrupt, `Stopped`), so nothing ever waits for the user;
//! - one message: the job's system prompt, the JSON schema and the whole prompt (stable parts first, so Claude Code's
//!   own prompt cache can reuse the prefix), with the instruction to use no tools and answer with only the JSON;
//! - followed with `t3_thread_wait` in short slices (the activity is read between them), its last assistant message
//!   read in full, the JSON cut out of it and checked against the schema's top-level shape;
//! - archived afterwards, whatever happened (T3 has no thread delete). The T3 observer never follows a
//!   `[heimdall-bg]` thread (`t3::observer::BACKGROUND_TITLE`), so no call wakes Heimdall's agent.
//!
//! Outcomes are the other wires': an answer, or `Stopped` (a tool request, no JSON, a malformed answer, the deadline)
//! so the job does not resend the same input; a harness error reply (`em::harness_error_reply`) or a failed run is a
//! plain error, retried on the next pass. A usage-limit reply starts the instance's quota episode
//! (`background_gate::quota_spent` under the provider id "t3:<instance>", with the reset Claude states), and the next
//! answer ends it. T3's own `system_notice` and `error` items are not tools: a spent account's run shows "Claude usage
//! limit reached …" as a notice, which read as "the model asked for a tool" and kept Goals and the island agent on a
//! spent account for five hours (bug note 2026-11-02T14-47-41); they are read as the run's failure
//! (`agent::limits`, with the reset T3 gives), so the task falls back. Emergency mode pauses these calls like every other background request (`background_gate::check`).
//! T3 reports no token counts: each call is one ledger line at $0 (`agent/background-runs.jsonl`), like the other
//! subscriptions. The whole call, archive included, stays within `background_call::TIMEOUT`.
//!
//! A rolling session (`Call::session`, the Goals shadow only;, docs/background-call.md) keeps one thread per
//! session key instead: the first call launches it as above (title `[heimdall-bg] <job> <session> <tag>`) with the
//! whole history rendered into its first message, and each later call sends only the new turn into it
//! (`t3_thread_send`), followed the same way, so Claude Code's own cache serves the rest. The thread stays open after an
//! answer; anything else (a tool, no JSON, a failed run, the deadline) interrupts and archives it as above. A remembered
//! thread that has not answered exactly `history.len()` turns on the same instance, model, effort and prompt (a
//! restart, a fallback that ran elsewhere, a failed turn) is archived and a fresh one launched with the whole history;
//! a job's new session key (compaction) archives its old thread, and `end_session` does so on the job's word. The
//! session → thread map is in memory only: a restart relaunches once, and each session launch first archives the job's
//! session threads no session holds (`sweep`).
//!
//! Tool loops (deep search, the suggester's file search) are not offered on T3: their tools run in Heimdall's process
//! and a T3 thread cannot call them, so `ask_tools_blocking` answers `NoToolsOnT3` instead of faking a loop in text.
//!
//! The personal edition offers only Claude-driver instances (the user: never start Codex agents for his work; the Codex
//! subscription's background calls go through the ChatGPT sign-in provider). The public edition lists every usable one.

use crate::background_call::{wire, Answer, Call, Run, Stopped, Usage, TIMEOUT};
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// Every background thread's title starts with this; the T3 observer ignores such threads.
pub const TITLE: &str = ::t3::observer::BACKGROUND_TITLE;
/// The T3 project the threads live in.
pub const PROJECT_TITLE: &str = "Heimdall background";
/// Its folder under Heimdall's data folder (empty: nothing in it is the model's business).
pub const PROJECT_DIR: &str = "background-t3";
/// T3's most restrictive runtime mode (`approval-required`, `auto-accept-edits`, `auto`, `full-access`).
pub const RUNTIME_MODE: &str = "approval-required";
/// Timeline items that are not a tool: the messages, thinking, and T3's own bookkeeping.
const BENIGN: [&str; 7] = ["user_message", "assistant_message", "reasoning", "checkpoint", "plan", "context_compaction", "notice"];

/// T3's tools as a background call uses them: the app's `t3::Client`, a scripted fake in tests.
#[async_trait]
pub trait Calls: Send + Sync {
    async fn call(&self, name: &str, args: Value, deadline: Duration) -> Result<Value>;
}

struct Real(Arc<::t3::Client>);
#[async_trait]
impl Calls for Real {
    async fn call(&self, name: &str, args: Value, deadline: Duration) -> Result<Value> { self.0.call_deadline(name, args, deadline).await }
}

/// How long a call may take: its work (project, launch, waits, reads), each wait slice, and each cleanup call's own
/// deadline (interrupt, archive), outside the work's budget so a late run is still stopped and archived (review 30
/// Oct: cleanup deadlines as short as 50 ms). By default the work and both cleanup calls fit in `TIMEOUT`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Limits {
    pub total: Duration,
    pub slice: Duration,
    pub cleanup: Duration,
}
impl Default for Limits {
    fn default() -> Self { Limits { total: TIMEOUT - Duration::from_secs(30), slice: Duration::from_secs(15), cleanup: Duration::from_secs(10) } }
}

#[derive(Clone)]
struct Registered {
    calls: Arc<dyn Calls>,
    /// The project's folder.
    folder: PathBuf,
    limits: Limits,
    /// The app's client: its calls run on this module's own long-lived runtime, never on a job's short-lived one
    /// (`ask_blocking` builds a runtime per call, and a connection pooled on a runtime that is gone fails the next
    /// request on it).
    own_runtime: bool,
}

#[cfg(not(test))]
static REGISTERED: std::sync::RwLock<Option<Registered>> = std::sync::RwLock::new(None);
#[cfg(not(test))]
fn registered() -> Option<Registered> { REGISTERED.read().unwrap().clone() }
#[cfg(not(test))]
fn register(r: Registered) { *REGISTERED.write().unwrap() = Some(r) }
// Tests: per thread, like the ledger and the gate.
#[cfg(test)]
thread_local! { static REGISTERED: std::cell::RefCell<Option<Registered>> = const { std::cell::RefCell::new(None) }; }
#[cfg(test)]
fn registered() -> Option<Registered> { REGISTERED.with(|r| r.borrow().clone()) }
#[cfg(test)]
fn register(r: Registered) { REGISTERED.with(|c| *c.borrow_mut() = Some(r)) }

/// The app's T3 client (registered where the app builds its T3 workers): background calls on a T3 instance use it,
/// with their project folder under `data`.
pub fn set_client(client: Arc<::t3::Client>, data: &Path) {
    register(Registered { calls: Arc::new(Real(client)), folder: data.join(PROJECT_DIR), limits: Limits::default(), own_runtime: true });
}

/// Tests and lab runs: any `Calls`, any folder and limits, on the caller's runtime.
#[cfg_attr(not(test), allow(dead_code))]
pub fn set_calls(calls: Arc<dyn Calls>, folder: PathBuf, limits: Limits) {
    register(Registered { calls, folder, limits, own_runtime: false });
}

/// Whether a T3 client is registered (none: a T3 choice makes no call).
pub fn available() -> bool { registered().is_some() }

fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RT.get_or_init(|| tokio::runtime::Builder::new_multi_thread().worker_threads(1).thread_name("heimdall-bg-t3").enable_all().build().expect("background T3 runtime"))
}

// ---------------------------------------------------------------------------------------------------------
// T3's catalog: the instances Background Tasks ▸ Model offers.

/// One model as the cache keeps it: the UI's row and the option its effort is set through ("effort" on Claude,
/// "reasoningEffort" on Codex).
#[derive(Clone, Debug, PartialEq)]
struct Known {
    instance: String,
    driver: String,
    model: ui::T3Model,
    option: Option<String>,
}

#[cfg(not(test))]
static KNOWN: std::sync::RwLock<Option<Vec<Known>>> = std::sync::RwLock::new(None);
#[cfg(not(test))]
fn known() -> Option<Vec<Known>> { KNOWN.read().unwrap().clone() }
#[cfg(not(test))]
fn set_known(k: Vec<Known>) { *KNOWN.write().unwrap() = Some(k) }
#[cfg(test)]
thread_local! { static KNOWN: std::cell::RefCell<Option<Vec<Known>>> = const { std::cell::RefCell::new(None) }; }
#[cfg(test)]
fn known() -> Option<Vec<Known>> { KNOWN.with(|k| k.borrow().clone()) }
#[cfg(test)]
fn set_known(k: Vec<Known>) { KNOWN.with(|c| *c.borrow_mut() = Some(k)) }

/// Whether an instance may be offered in this edition: the personal one never offers Codex.
pub fn offered(driver: &str, public: bool) -> bool {
    let d = driver.to_ascii_lowercase();
    if public { true } else { d.contains("claude") }
}

fn parse(catalog: &Value, public: bool) -> (Vec<ui::T3Instance>, Vec<Known>) {
    let mut out = Vec::new();
    let mut flat = Vec::new();
    for p in catalog["providers"].as_array().into_iter().flatten() {
        let Some(instance) = p["providerInstanceId"].as_str() else { continue };
        let driver = p["driverKind"].as_str().unwrap_or(instance).to_owned();
        let disabled = p["enabled"] == false || p["constraints"].as_array().into_iter().flatten().any(|c| c.as_str().is_some_and(|c| c.to_ascii_lowercase().contains("disabled")));
        // A thread must be able to run, in the mode these calls use (no list: every mode).
        let runs = p["canRunChildTask"] != false;
        let mode = p["runtimeModes"].as_array().is_none_or(|m| m.iter().any(|x| x == RUNTIME_MODE));
        if disabled || !runs || !mode || !offered(&driver, public) { continue }
        let mut models = Vec::new();
        for m in p["models"].as_array().into_iter().flatten() {
            let Some(id) = m["id"].as_str() else { continue };
            let option = m["options"].as_array().into_iter().flatten().find(|o| o["id"] == "effort" || o["id"] == "reasoningEffort");
            let choices = option.map(::t3::text::option_choices).unwrap_or_default();
            let efforts: Vec<String> = choices.iter().filter_map(|(v, _)| v.as_str().map(str::to_owned)).collect();
            let default_effort = choices.iter().find(|(_, d)| *d).and_then(|(v, _)| v.as_str().map(str::to_owned));
            let model = ui::T3Model { id: id.into(), name: m["label"].as_str().or(m["name"].as_str()).unwrap_or(id).into(), efforts, default_effort };
            flat.push(Known { instance: instance.into(), driver: driver.clone(), model: model.clone(), option: option.and_then(|o| o["id"].as_str()).map(str::to_owned) });
            models.push(model);
        }
        if models.is_empty() { continue }
        out.push(ui::T3Instance { instance: instance.into(), name: p["displayName"].as_str().unwrap_or(instance).into(), driver, models });
    }
    (out, flat)
}

/// T3's live `orchestrator_capabilities` → the instances Background Tasks ▸ Model lists (every enabled one that can
/// run a thread in `RUNTIME_MODE` and lists models; the personal edition's Claude ones only), kept for the calls
/// (each model's effort option) and the menu's choices.
pub fn publish_catalog(catalog: &Value) -> Vec<ui::T3Instance> {
    let (list, flat) = parse(catalog, crate::edition::PUBLIC);
    set_known(flat);
    list
}

/// The instances as last published, for the settings tool.
pub fn instances() -> Vec<ui::T3Instance> {
    let Some(k) = known() else { return vec![] };
    let mut out: Vec<ui::T3Instance> = Vec::new();
    for m in k {
        match out.iter_mut().find(|i| i.instance == m.instance) {
            Some(i) => i.models.push(m.model),
            None => out.push(ui::T3Instance { instance: m.instance.clone(), name: m.instance, driver: m.driver, models: vec![m.model] }),
        }
    }
    out
}

/// A model's effort levels as T3 lists them (empty: unknown, or none to pick).
pub fn efforts(instance: &str, model: &str) -> Vec<String> {
    known().into_iter().flatten().find(|k| k.instance == instance && k.model.id == model).map(|k| k.model.efforts).unwrap_or_default()
}

/// Whether a menu choice is one this edition offers: a plausible instance id, and (once T3's list is known) a listed
/// instance this edition offers. Before the list loads the personal edition still refuses any Codex instance.
pub fn choice_ok(instance: &str) -> bool {
    let plain = !instance.is_empty() && instance.len() <= 128 && instance.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.');
    if !plain { return false }
    match known() {
        Some(k) if !k.is_empty() => k.iter().any(|m| m.instance == instance && offered(&m.driver, crate::edition::PUBLIC)),
        _ => crate::edition::PUBLIC || !instance.to_ascii_lowercase().contains("codex"),
    }
}

/// The effort a call asks T3 for: the level when the model lists it (or nothing is known), else the model's default
/// (`None`: T3 picks).
fn pick_effort(level: &str, listed: &[String], default: Option<&str>) -> Option<String> {
    if listed.is_empty() || listed.iter().any(|l| l == level) { return Some(level.to_owned()) }
    default.map(str::to_owned)
}

/// The `modelSelection` of a call.
fn selection(instance: &str, model: &str, effort: &str) -> Value {
    let k = known().into_iter().flatten().find(|k| k.instance == instance && k.model.id == model);
    let option = k.as_ref().and_then(|k| k.option.clone()).unwrap_or_else(|| if instance.to_ascii_lowercase().contains("codex") { "reasoningEffort".into() } else { "effort".into() });
    let (listed, default) = k.as_ref().map_or((vec![], None), |k| (k.model.efforts.clone(), k.model.default_effort.clone()));
    let mut s = json!({"instanceId": instance, "model": model});
    if let Some(e) = pick_effort(effort, &listed, default.as_deref()) { s["options"] = json!({ option: e }); }
    s
}

// ---------------------------------------------------------------------------------------------------------
// The endpoint.

/// "t3:" alone is the default instance: one provider id per instance.
fn interned(provider: &str) -> &'static str {
    static IDS: std::sync::Mutex<Vec<&'static str>> = std::sync::Mutex::new(Vec::new());
    let id = match ui::t3_instance(provider) { Some(i) => format!("{}{i}", ui::T3_PROVIDER_PREFIX), None => provider.to_owned() };
    let mut ids = IDS.lock().unwrap();
    if let Some(s) = ids.iter().find(|s| **s == id) { return s }
    let s: &'static str = Box::leak(id.into_boxed_str());
    ids.push(s);
    s
}

/// A background choice on a T3 instance as an endpoint (`background_model::endpoint_for`): the provider id
/// "t3:<instance>", the model, no key but the marker that a client is there (no client, or an instance this edition
/// does not offer: no call). Unpriced (`Loop::for_endpoint`: not Anthropic), reasoning, the model's listed efforts.
pub fn endpoint(provider: &str, model: &str) -> crate::plan_refresher::Endpoint {
    let instance = ui::t3_instance(provider).unwrap_or(ui::T3_DEFAULT_INSTANCE);
    let ok = available() && choice_ok(instance) && !model.trim().is_empty();
    crate::plan_refresher::Endpoint {
        base: format!("t3://{instance}"),
        key: ok.then(|| agent::provider::NO_KEY.into()),
        timeout: TIMEOUT,
        model: Some(model.into()),
        provider: interned(provider),
        reasoning: true,
        efforts: efforts(instance, model),
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------------------------------------
// One call.

/// `ask_tools_blocking` on a T3 instance: the tools run in Heimdall and a T3 thread cannot call them.
#[derive(Debug)]
pub struct NoToolsOnT3 { pub job: &'static str }
impl std::fmt::Display for NoToolsOnT3 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} needs tool calls, which a T3 background thread cannot make: not on T3 (pick an API or local model in Background Tasks ▸ Model for it)", self.job)
    }
}
impl std::error::Error for NoToolsOnT3 {}

/// The one message a call's thread gets (a rolling session's: its first, `session_message`).
pub fn message(c: &Call) -> String {
    if c.session.is_some() || !c.history.is_empty() { return session_message(c) }
    format!("{TITLE} A one-time, stateless background call from Heimdall (job: {job}). Use no tools at all: read no files, run no \
commands, search nothing, call nothing; everything you need is below. Answer with only one JSON value that matches the JSON \
schema below: no other text, no Markdown fence.\n\n<instructions>\n{system}\n</instructions>\n\n<json_schema>\n{schema}\n</json_schema>\n\n\
<input>\n{prompt}\n</input>\n\nAnswer now with only the JSON.",
        job = c.job, system = c.system, schema = c.schema, prompt = c.prompt.text())
}

/// A rolling session's first message: the same rules and parts as `message`, the earlier turns (each its input and
/// the answer given), then the new input. Later turns get `next_message` alone.
pub fn session_message(c: &Call) -> String {
    let mut earlier = String::new();
    if !c.history.is_empty() {
        earlier.push_str("<earlier_turns>\n");
        for t in &c.history { earlier.push_str(&format!("<turn>\n<input>\n{}\n</input>\n<answer>\n{}\n</answer>\n</turn>\n", t.user.text(), t.assistant)); }
        earlier.push_str("</earlier_turns>\n\n");
    }
    format!("{TITLE} A rolling background session from Heimdall (job: {job}): each later message in this thread brings the next \
input. Use no tools at all: read no files, run no commands, search nothing, call nothing; everything you need is in this \
conversation. Answer every input with only one JSON value that matches the JSON schema below: no other text, no Markdown \
fence.\n\n<instructions>\n{system}\n</instructions>\n\n<json_schema>\n{schema}\n</json_schema>\n\n{earlier}<input>\n{prompt}\n\
</input>\n\nAnswer now with only the JSON.",
        job = c.job, system = c.system, schema = c.schema, prompt = c.prompt.text())
}

/// A rolling session's later turn: the new input alone.
pub fn next_message(c: &Call) -> String { format!("<input>\n{}\n</input>\n\nAnswer now with only the JSON.", c.prompt.text()) }

/// The JSON in a reply: the whole text (after a fence or `</think>`), else from its first `{`/`[` to its last `}`/`]`.
pub fn extract(text: &str) -> Option<Value> {
    let t = wire::json_text(text);
    if let Ok(v) = serde_json::from_str::<Value>(&t) { return Some(v) }
    let start = t.find(['{', '['])?;
    let end = t.rfind(['}', ']'])?;
    (end > start).then(|| serde_json::from_str(&t[start..=end]).ok()).flatten()
}

fn type_ok(v: &Value, t: &str) -> bool {
    match t {
        "object" => v.is_object(),
        "array" => v.is_array(),
        "string" => v.is_string(),
        "boolean" => v.is_boolean(),
        "number" => v.is_number(),
        "integer" => v.is_i64() || v.is_u64(),
        "null" => v.is_null(),
        _ => true,
    }
}
fn types_ok(v: &Value, t: &Value) -> bool {
    match t {
        Value::String(t) => type_ok(v, t),
        Value::Array(ts) => ts.iter().filter_map(Value::as_str).any(|t| type_ok(v, t)),
        _ => true,
    }
}

/// The schema's top-level shape: the type, the required keys, and each present property's own type.
pub fn check_shape(v: &Value, schema: &Value) -> std::result::Result<(), String> {
    if !types_ok(v, &schema["type"]) { return Err(format!("it is not a JSON {}", schema["type"].as_str().unwrap_or("value of the schema's type"))) }
    let Some(o) = v.as_object() else { return Ok(()) };
    for key in schema["required"].as_array().into_iter().flatten().filter_map(Value::as_str) {
        if !o.contains_key(key) { return Err(format!("it lacks \"{key}\"")) }
    }
    for (key, value) in o {
        let t = &schema["properties"][key]["type"];
        if !types_ok(value, t) { return Err(format!("\"{key}\" is not {}", t)) }
    }
    Ok(())
}

/// The reset a usage-limit reply states ("Claude AI usage limit reached|1793066649": seconds).
pub fn stated_reset(reply: &str) -> Option<u64> {
    let (_, rest) = reply.split_once('|')?;
    let digits: String = rest.trim().chars().take_while(char::is_ascii_digit).collect();
    let n: u64 = digits.parse().ok()?;
    Some(if n < 100_000_000_000 { n * 1000 } else { n })
}

/// One thread's run, as followed.
#[derive(Debug, PartialEq)]
enum Follow {
    /// Completed, with its last assistant message.
    Reply(Option<String>),
    /// A tool item, or a wait on an approval or question.
    Tool(String),
    /// Failed, interrupted, cancelled: the status and any reply.
    Failed(String, Option<String>),
}

/// A call's job, owned (the work runs on another runtime).
struct Job {
    job: &'static str,
    provider: &'static str,
    instance: String,
    model: String,
    effort: String,
    message: String,
    schema: Value,
    hash: u64,
    /// A rolling session's key, the turns before this one, and the new turn alone (`next_message`).
    session: Option<String>,
    turns: usize,
    next: String,
}

/// A rolling session's thread, in memory only.
#[derive(Clone, Debug)]
struct Session {
    key: String,
    job: &'static str,
    thread: String,
    /// The turns it has answered.
    turns: usize,
    /// The last timeline position read: the next turn reads only its own items.
    after: Option<u64>,
    /// The run of the last answered turn: a send that names no run never follows that one again.
    run: Option<String>,
    /// Instance, model, effort, prompt hash and project it runs on: any change relaunches.
    on: (String, String, String, u64, String),
}

#[cfg(not(test))]
static SESSIONS: std::sync::Mutex<Vec<Session>> = std::sync::Mutex::new(Vec::new());
#[cfg(not(test))]
fn sessions<T>(f: impl FnOnce(&mut Vec<Session>) -> T) -> T { f(&mut SESSIONS.lock().unwrap()) }
#[cfg(test)]
thread_local! { static SESSIONS: std::cell::RefCell<Vec<Session>> = const { std::cell::RefCell::new(Vec::new()) }; }
#[cfg(test)]
fn sessions<T>(f: impl FnOnce(&mut Vec<Session>) -> T) -> T { SESSIONS.with(|s| f(&mut s.borrow_mut())) }

/// The sessions `gone` picks, taken out of the map.
fn take_sessions(gone: impl Fn(&Session) -> bool) -> Vec<Session> {
    sessions(|all| {
        let (out, kept): (Vec<Session>, Vec<Session>) = std::mem::take(all).into_iter().partition(|s| gone(s));
        *all = kept;
        out
    })
}

/// The job is done with a rolling session (`Call::session`): its thread, if one is open, is archived. A later call with
/// the same key launches anew.
pub fn end_session(session: &str) {
    let gone = take_sessions(|s| s.key == session);
    if gone.is_empty() { return }
    let Some(r) = registered() else { return };
    let own = r.own_runtime;
    let work = async move { for s in gone { cleanup(&r, s.job, &s.thread, false).await } };
    if own { runtime().spawn(work); return }
    match tokio::runtime::Handle::try_current() {
        Ok(h) => { h.spawn(work); }
        Err(_) => match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(rt) => rt.block_on(work),
            Err(err) => eprintln!("Background tasks: the session {session}'s T3 thread was not archived: {err:#}"),
        },
    }
}

/// One call on `e`'s T3 instance (`background_call::ask`).
pub async fn ask(e: &crate::plan_refresher::Endpoint, c: &Call<'_>) -> Result<Answer> {
    // Setup's agent check asks before Agent is on (setup_ready.rs).
    if c.job != crate::pipeline::setup_ready::AGENT_CHECK_JOB { crate::background_gate::check()?; }
    crate::background_gate::quota_check(e.provider, agent::now_ms())?;
    let r = registered().context("T3 is not connected: no T3 background call")?;
    let instance = ui::t3_instance(e.provider).unwrap_or(ui::T3_DEFAULT_INSTANCE).to_owned();
    anyhow::ensure!(choice_ok(&instance), "the T3 instance {instance} is not offered for background tasks");
    let job = Job { job: c.job, provider: interned(e.provider), instance, model: c.model.clone(), effort: c.effort.to_owned(), message: message(c),
        schema: c.schema.clone(), hash: crate::background_call::prompt_hash(c), session: c.session.clone().filter(|s| !s.is_empty()), turns: c.history.len(),
        next: next_message(c) };
    if r.own_runtime { runtime().spawn(run(r, job)).await.context("the background T3 call stopped")? } else { run(r, job).await }
}

fn line(j: &Job, outcome: &'static str) {
    crate::background_call::record(&Run { job: j.job, model: format!("{}/{}", j.provider, j.model), hash: j.hash, usage: Usage::default(), usd: 0.0, outcome, at_ms: agent::now_ms() });
}

fn stopped(reason: String) -> anyhow::Error { Stopped { usage: Usage::default(), usd: 0.0, reason }.into() }

async fn run(r: Registered, j: Job) -> Result<Answer> {
    let start = tokio::time::Instant::now();
    let work_until = start + r.limits.total;
    let left = |cap: Duration| work_until.saturating_duration_since(tokio::time::Instant::now()).min(cap);
    let project = match tokio::time::timeout_at(work_until, project(&*r.calls, &r.folder)).await {
        Ok(p) => p.inspect_err(|e| { t3_down(e); })?,
        Err(_) => bail!("T3 did not answer in time (the background project)"),
    };
    let on = (j.instance.clone(), j.model.clone(), j.effort.clone(), j.hash, project.clone());
    // A rolling session: the job's other segments are over (compaction); its own thread takes the new turn alone when it
    // has answered every earlier one on the same instance, model, effort and prompt, else it is replaced.
    let mut resumed = None;
    if let Some(key) = &j.session {
        for old in take_sessions(|s| s.job == j.job && s.key != *key) { cleanup(&r, j.job, &old.thread, false).await; }
        for s in take_sessions(|s| s.key == *key) {
            if resumed.is_none() && s.turns == j.turns && s.on == on { resumed = Some(s) } else { cleanup(&r, j.job, &s.thread, false).await; }
        }
    }
    let resumed = match resumed {
        Some(s) => match send(&*r.calls, &s.thread, &j.next, left(Duration::from_secs(60))).await {
            Ok(run_id) => Some((s, run_id)),
            // A thread that no longer takes a turn (archived or gone in T3) is replaced by a fresh launch.
            Err(err) => {
                eprintln!("Background tasks · {}: the session's T3 thread {} took no turn ({err:#}); a new one starts with the whole history", j.job, s.thread);
                cleanup(&r, j.job, &s.thread, true).await;
                if t3_down(&err) { return Err(err.context(format!("T3 did not take the {} turn", j.job))) }
                None
            }
        },
        None => None,
    };
    let (thread, mut run_id, mut after, stale) = match resumed {
        Some((s, run_id)) => (s.thread, run_id, s.after, s.run),
        None => {
            // A session's thread left open by an earlier process (the map is in memory) or an archive that failed.
            if j.session.is_some() { sweep(&r, j.job, &project).await; }
            let (t, run_id) = launch(&r, &j, &project, left(Duration::from_secs(60))).await?;
            (t, run_id, None, None)
        }
    };
    let followed = tokio::time::timeout_at(work_until, follow(&*r.calls, &thread, &mut run_id, stale.as_deref(), r.limits.slice, &mut after)).await;
    // A run that still works is stopped, and the thread is archived, but a rolling session's thread that answered.
    let still_running = !matches!(followed, Ok(Ok(Follow::Reply(_) | Follow::Failed(..))));
    let answer = judge(&r, &j, followed);
    match (&j.session, &answer) {
        (Some(key), Ok(_)) => {
            let s = Session { key: key.clone(), job: j.job, thread, turns: j.turns + 1, after, run: run_id, on };
            // A call of the same session that ran meanwhile left its own thread: the newest answer's stays.
            for old in take_sessions(|o| o.key == *key) { cleanup(&r, j.job, &old.thread, false).await; }
            sessions(|all| all.push(s));
        }
        _ => cleanup(&r, j.job, &thread, still_running).await,
    }
    answer
}

/// The launch of a call's thread in `project`: its id and run.
async fn launch(r: &Registered, j: &Job, project: &str, deadline: Duration) -> Result<(String, Option<String>)> {
    // The title carries the request's own tag, so a launch whose answer was lost can still be found and cleaned up.
    let request = uuid::Uuid::new_v4().simple().to_string();
    let tag = &request[..12];
    let title = match &j.session { Some(key) => format!("{TITLE} {} {key} {tag}", j.job), None => format!("{TITLE} {} {tag}", j.job) };
    let args = json!({"projectId": project, "title": title, "message": j.message,
        "modelSelection": selection(&j.instance, &j.model, &j.effort), "runtimeMode": RUNTIME_MODE, "interactionMode": "default",
        "workspaceStrategy": {"type": "root"}, "clientRequestId": request});
    let launched = r.calls.call("t3_thread_launch", args, deadline).await;
    match launched.as_ref().ok().and_then(|v| v["threadId"].as_str()) {
        Some(t) => Ok((t.to_owned(), launched.as_ref().ok().and_then(|v| v["runId"].as_str()).map(str::to_owned))),
        None => {
            // T3 may have made the thread before the answer was lost (a timeout, a dropped connection, an answer without
            // its id): found by its tag, it is stopped and archived like any other.
            if let Some(t) = find_launched(&*r.calls, project, tag, r.limits.cleanup).await { cleanup(r, j.job, &t, true).await; }
            Err(match launched {
                Err(err) => {
                    // T3 down: every instance. A refused launch (T3_TOOL_FAILED: a project, an argument) is this job's
                    // failure and changes no backend (review 30 Oct).
                    t3_down(&err);
                    if format!("{err:#}").to_ascii_lowercase().contains("project") { forget_project(&r.folder); }
                    err.context(format!("T3 did not start the {} thread", j.job))
                }
                Ok(_) => anyhow::anyhow!("T3 launch lacks a thread id"),
            })
        }
    }
}

/// Archives the job's session threads (`[heimdall-bg] <job> <key> <tag>`) that no session holds: a restart lost the
/// map, or an archive failed while T3 was away. Read before each session launch (once a segment); a stateless call's
/// title (`[heimdall-bg] <job> <tag>`) is never taken for one.
///
/// T3's list has no archived filter, so it may bring back threads long archived: those it marks (`archived`,
/// `archivedAt`) and those this process archived are skipped, at most `SWEEP_MAX` are archived a sweep, and the whole
/// sweep has one cleanup deadline (review 31 Oct: up to 50 re-archives, each with its own deadline, ate the launch's
/// budget). What is left over goes at the next session launch.
async fn sweep(r: &Registered, job: &'static str, project: &str) {
    if tokio::time::timeout(r.limits.cleanup, sweep_within(r, job, project)).await.is_err() {
        eprintln!("Background tasks · {job}: the sweep of old session threads stopped at its {} ms; the rest goes next launch", r.limits.cleanup.as_millis());
    }
}

/// Session threads archived a sweep, at most.
const SWEEP_MAX: usize = 5;

async fn sweep_within(r: &Registered, job: &'static str, project: &str) {
    let prefix = format!("{TITLE} {job} ");
    let Ok(page) = r.calls.call("t3_thread_list", json!({"projectId": project, "titleContains": prefix, "limit": 50}), r.limits.cleanup).await else { return };
    let held: Vec<String> = sessions(|all| all.iter().map(|s| s.thread.clone()).collect());
    let mut done = 0;
    for t in page["threads"].as_array().into_iter().flatten() {
        if done == SWEEP_MAX { break }
        let (Some(id), Some(title)) = (t["threadId"].as_str(), t["title"].as_str()) else { continue };
        let rest: Vec<&str> = title.strip_prefix(&prefix).map(|r| r.split(' ').collect()).unwrap_or_default();
        let session_title = rest.len() == 2 && rest[1].len() == 12 && rest[1].bytes().all(|b| b.is_ascii_hexdigit());
        let gone = t["archived"] == true || t["archivedAt"].as_str().is_some_and(|a| !a.is_empty()) || was_archived(id);
        if !session_title || gone || held.iter().any(|h| h == id) { continue }
        let running = ["preparing", "queued", "starting", "running", "waiting"].contains(&t["status"].as_str().unwrap_or(""));
        cleanup(r, job, id, running).await;
        done += 1;
    }
}

/// Threads this process archived (the newest `ARCHIVED_KEEP`), so a sweep never archives them again.
const ARCHIVED_KEEP: usize = 512;
#[cfg(not(test))]
static ARCHIVED: std::sync::Mutex<std::collections::VecDeque<String>> = std::sync::Mutex::new(std::collections::VecDeque::new());
#[cfg(not(test))]
fn archived<T>(f: impl FnOnce(&mut std::collections::VecDeque<String>) -> T) -> T { f(&mut ARCHIVED.lock().unwrap()) }
#[cfg(test)]
thread_local! { static ARCHIVED: std::cell::RefCell<std::collections::VecDeque<String>> = const { std::cell::RefCell::new(std::collections::VecDeque::new()) }; }
#[cfg(test)]
fn archived<T>(f: impl FnOnce(&mut std::collections::VecDeque<String>) -> T) -> T { ARCHIVED.with(|a| f(&mut a.borrow_mut())) }
fn was_archived(thread: &str) -> bool { archived(|a| a.iter().any(|t| t == thread)) }
fn mark_archived(thread: &str) {
    archived(|a| {
        if a.len() == ARCHIVED_KEEP { a.pop_front(); }
        a.push_back(thread.to_owned());
    })
}

/// A rolling session's next turn into its open thread: the run it started.
async fn send(calls: &dyn Calls, thread: &str, message: &str, deadline: Duration) -> Result<Option<String>> {
    let r = calls.call("t3_thread_send", json!({"threadId": thread, "message": message, "mode": "auto", "clientRequestId": uuid::Uuid::new_v4().to_string()}), deadline).await?;
    Ok(r["runId"].as_str().map(str::to_owned))
}

/// The followed run as the call's outcome, its ledger line written.
fn judge(r: &Registered, j: &Job, followed: std::result::Result<Result<Follow>, tokio::time::error::Elapsed>) -> Result<Answer> {
    let followed = match followed {
        Err(_) => { line(j, "timed out"); return Err(stopped(format!("the T3 thread did not answer within {} s", r.limits.total.as_secs()))) }
        Ok(f) => f.inspect_err(|err| { t3_down(err); line(j, "failed"); })?,
    };
    let reply = match followed {
        Follow::Tool(what) => { line(j, "refused"); return Err(stopped(format!("the model asked for a tool ({what}); a T3 background call uses none, so the run was stopped"))) }
        Follow::Failed(status, reply) => {
            // The provider's error as the reply, or T3's own notes about the run ("title: text [item id]", `follow`).
            if let Some(text) = reply.as_deref().filter(|t| t.lines().any(|l| agent::emergency::harness_error_reply(l) || l.ends_with(']'))) { spent(j, text); }
            line(j, "failed");
            bail!("the T3 run on {} ended {status}{}", j.instance, reply.map(|t| format!(": {}", bounded(&t, 300))).unwrap_or_default());
        }
        Follow::Reply(reply) => reply,
    };
    let Some(text) = reply.filter(|t| !t.trim().is_empty()) else { line(j, "failed"); return Err(stopped("the answer has no text".into())) };
    if agent::emergency::harness_error_reply(&text) {
        spent(j, &text);
        line(j, "failed");
        bail!("{} could not run: {}", j.instance, bounded(&text, 300));
    }
    crate::background_gate::quota_back(j.provider);
    let Some(v) = extract(&text) else { line(j, "failed"); return Err(stopped("the answer is not JSON".into())) };
    if let Err(why) = check_shape(&v, &j.schema) { line(j, "failed"); return Err(stopped(format!("the answer is not the JSON the schema asks for: {why}"))) }
    line(j, "answered");
    Ok(Answer { text: v.to_string(), usage: Usage::default(), usd: 0.0 })
}

/// Interrupt (when it may still run) and archive a call's thread, each with the cleanup deadline of its own.
async fn cleanup(r: &Registered, job: &str, thread: &str, interrupt: bool) {
    if interrupt {
        if let Err(err) = r.calls.call("t3_thread_interrupt", json!({"threadId": thread}), r.limits.cleanup).await {
            eprintln!("Background tasks · {job}: the T3 thread {thread} was not interrupted: {err:#}");
        }
    }
    match r.calls.call("t3_thread_organize", json!({"threadId": thread, "action": "archive"}), r.limits.cleanup).await {
        Ok(_) => mark_archived(thread),
        Err(err) => eprintln!("Background tasks · {job}: the T3 thread {thread} was not archived: {err:#}"),
    }
}

/// The thread a launch made whose answer was lost: the project's thread whose title has the request's tag (two looks,
/// a moment apart, as T3 may still be making it).
async fn find_launched(calls: &dyn Calls, project: &str, tag: &str, deadline: Duration) -> Option<String> {
    for attempt in 0..2 {
        if attempt > 0 { tokio::time::sleep(Duration::from_millis(500)).await; }
        let Ok(page) = calls.call("t3_thread_list", json!({"projectId": project, "titleContains": tag, "limit": 5}), deadline).await else { continue };
        let found = page["threads"].as_array().into_iter().flatten()
            .find(|t| t["title"].as_str().is_some_and(|x| x.contains(tag))).and_then(|t| t["threadId"].as_str()).map(str::to_owned);
        if found.is_some() { return found }
    }
    None
}

/// A harness error reply, classified like the API wires' (`background_gate::fault`): a usage limit, a 429 or spent
/// credit starts the instance's quota episode (its stated reset when there is one); overloaded, a 5xx, no connection or
/// the sign-in lost marks it down; anything else ("prompt is too long", a 400) is this job's failure and changes nothing.
fn spent(j: &Job, text: &str) {
    let now = agent::now_ms();
    // T3's limit report (`agent::limits`): spent, with the reset it names ("resets 1pm", the notice's own reset).
    let limit = agent::limits::parse(text, now);
    let fault = if limit.is_some() { Some(crate::background_gate::Fault::Spent) } else { crate::background_gate::fault(None, text) };
    crate::background_gate::mark_fault(j.provider, fault, stated_reset(text).or(limit.and_then(|l| l.reset_ms)), &bounded(text, 80), now);
}

/// T3 itself did not answer (not running, not connected): every T3 instance falls back until it does. True when so.
fn t3_down(err: &anyhow::Error) -> bool {
    let text = format!("{err:#}");
    let down = ["T3_UNAVAILABLE", "T3_AUTH_REQUIRED", "T3_TRANSPORT_ERROR"].iter().any(|c| text.contains(c));
    if down { crate::background_gate::backend_down(crate::background_gate::T3_ALL, "T3 is not running or not connected", agent::now_ms()); }
    down
}

fn bounded(s: &str, chars: usize) -> String { s.chars().take(chars).collect() }

/// The run until it ends, a tool appears, or it waits on the user.
/// `after`: the last position already read (a rolling session's earlier turns), moved on as items are read. `stale`: the
/// previous turn's run, never taken for this one when the send named none (T3 queued it).
async fn follow(calls: &dyn Calls, thread: &str, run: &mut Option<String>, stale: Option<&str>, slice: Duration, after: &mut Option<u64>) -> Result<Follow> {
    let mut last: Option<String> = None;
    // T3's notices and errors for this run (a usage limit, a provider error): the failure's own words.
    let mut notes: Vec<String> = Vec::new();
    loop {
        let mut args = json!({"threadId": thread, "timeoutMs": slice.as_millis() as u64});
        if let Some(r) = run.as_ref() { args["runId"] = json!(r); }
        let w = calls.call("t3_thread_wait", args, slice + Duration::from_secs(15)).await?;
        let (tool, waiting, latest) = read_activity(calls, thread, after, &mut last, &mut notes).await?;
        if let Some(kind) = tool { return Ok(Follow::Tool(kind)) }
        if waiting { return Ok(Follow::Tool("an approval or question".into())) }
        // A usage-limit notice (its id) while the run still waits ("paused until the 7-day limit resets"): the
        // account is spent now, not after the call's whole deadline (review 2 Nov).
        if w["timedOut"] == true && notes.iter().any(|n| agent::limits::parse(n, 0).is_some_and(|l| l.reset_ms.is_some())) {
            return Ok(Follow::Failed("paused on a usage limit".into(), Some(notes.join("\n"))));
        }
        if w["timedOut"] == true { continue }
        // Without a run of its own, a wait may answer for the previous turn's, already over: the new one is not yet.
        if run.is_none() && stale.is_some() && w["runId"].as_str() == stale {
            tokio::time::sleep(slice).await;
            continue;
        }
        let status = w["status"].as_str().unwrap_or("");
        return Ok(match status {
            "completed" => Follow::Reply(last),
            // Idle: the launched run has not started yet (a wait without its run id answers at once), or it ended and
            // the thread rests. With a reply, it ended; without one, the wait goes on for the run it launched.
            "idle" if last.is_some() => Follow::Reply(last),
            "idle" => {
                if run.is_none() { *run = latest.filter(|l| Some(l.as_str()) != stale); }
                tokio::time::sleep(slice).await;
                continue;
            }
            "running" | "preparing" | "queued" | "starting" | "waiting" => continue,
            // T3's notes, with the reply only when it is the provider's error (a model's prose about limits is no
            // limit, review 2 Nov).
            other => Follow::Failed(other.to_owned(), match notes.is_empty() {
                true => last,
                false => Some(last.filter(|t| agent::emergency::harness_error_reply(t)).into_iter().chain(notes).collect::<Vec<_>>().join("\n")),
            }),
        });
    }
}

/// The thread's new timeline items: the first tool among them, whether it waits on an approval or question, and its
/// latest run id; the latest assistant message's whole text into `last`. An item still running is read again next time.
async fn read_activity(calls: &dyn Calls, thread: &str, after: &mut Option<u64>, last: &mut Option<String>, notes: &mut Vec<String>) -> Result<(Option<String>, bool, Option<String>)> {
    let mut waiting = false;
    let mut latest = None;
    for _ in 0..20 {
        let mut args = json!({"threadId": thread, "view": "activity", "limit": 100, "maxCharsPerItem": 50_000, "runLimit": 1});
        if let Some(p) = after { args["afterPosition"] = json!(p); }
        let page = calls.call("t3_thread_read", args, Duration::from_secs(15)).await?;
        let t = &page["thread"];
        waiting |= t["status"] == "waiting" || t["pendingRequestCount"].as_u64().unwrap_or(0) > 0;
        if let Some(r) = t["latestRunId"].as_str() { latest = Some(r.to_owned()); }
        for item in page["items"].as_array().into_iter().flatten() {
            let Some(at) = item["position"].as_u64() else { continue };
            let kind = item["type"].as_str().unwrap_or("");
            let text = item["text"].as_str().unwrap_or("");
            // T3's own notice or error about the run (a usage limit, a provider error): its failure, never a tool.
            if matches!(kind, "system_notice" | "error") {
                notes.push(format!("{}: {text} [{}]", item["title"].as_str().unwrap_or(kind), item["itemId"].as_str().unwrap_or("")));
                *after = Some(at);
                continue;
            }
            let tool = !BENIGN.contains(&kind) && !(kind == "command_execution" && text.starts_with("$ Preparing workspace"));
            if tool { return Ok((Some(if kind.is_empty() { "an unnamed item".into() } else { kind.replace('_', " ") }), waiting, latest)) }
            if item["status"] == "running" || item["status"] == "in_progress" { return Ok((None, waiting, latest)) }
            if kind == "assistant_message" { *last = Some(whole_text(calls, thread, item).await?); }
            *after = Some(at);
        }
        if page["hasMore"] != true { break }
    }
    Ok((None, waiting, latest))
}

/// An item's text, continued while T3 cut it off.
async fn whole_text(calls: &dyn Calls, thread: &str, item: &Value) -> Result<String> {
    let mut text = item["text"].as_str().unwrap_or("").to_owned();
    let mut cur = item.clone();
    for _ in 0..40 {
        if cur["textTruncated"] != true { break }
        let Some(offset) = cur["nextTextOffset"].as_u64() else { break };
        let r = calls.call("t3_thread_read", json!({"threadId": thread, "view": "activity", "itemId": cur["itemId"], "textOffset": offset, "limit": 1, "maxCharsPerItem": 50_000, "runLimit": 1}), Duration::from_secs(15)).await?;
        let Some(next) = r["items"].as_array().and_then(|a| a.first()).cloned() else { break };
        if next["nextTextOffset"].as_u64().is_some_and(|n| n <= offset) { break }
        text.push_str(next["text"].as_str().unwrap_or(""));
        cur = next;
    }
    Ok(text)
}

// ---------------------------------------------------------------------------------------------------------
// The project.

static PROJECTS: std::sync::Mutex<Vec<(PathBuf, String)>> = std::sync::Mutex::new(Vec::new());
static PROJECT_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn forget_project(folder: &Path) { PROJECTS.lock().unwrap().retain(|(f, _)| f != folder) }

/// The "Heimdall background" project's id: the registered project at `folder`, else a new one (created once, the
/// folder with it).
async fn project(calls: &dyn Calls, folder: &Path) -> Result<String> {
    if let Some((_, id)) = PROJECTS.lock().unwrap().iter().find(|(f, _)| f == folder) { return Ok(id.clone()) }
    let _one = PROJECT_LOCK.lock().await;
    if let Some((_, id)) = PROJECTS.lock().unwrap().iter().find(|(f, _)| f == folder) { return Ok(id.clone()) }
    // The file system off the async runtime (review 30 Oct): the folder made and resolved, each listed root resolved.
    let made = folder.to_owned();
    let root = blocking(move || {
        std::fs::create_dir_all(&made).with_context(|| format!("the background project folder {} could not be made", made.display()))?;
        Ok(std::fs::canonicalize(&made)?)
    }).await?;
    let mut cursor = Value::Null;
    let mut found = None;
    for _ in 0..10 {
        let mut args = json!({"limit": 100});
        if !cursor.is_null() { args["cursor"] = cursor.clone(); }
        let page = calls.call("t3_project_list", args, Duration::from_secs(15)).await?;
        let roots = resolved_roots(&page).await?;
        found = roots.into_iter().find(|(_, r)| *r == root).map(|(id, _)| id);
        cursor = page["nextCursor"].clone();
        if found.is_some() || cursor.is_null() { break }
    }
    let id = match found {
        Some(id) => id,
        None => match create(calls, &root).await {
            Ok(id) => id,
            // Only a full-access client may make a project (T3: "Changing projects … needs … a full-access client"):
            // the threads go to the project holding Heimdall's home instead, as the relay's do.
            Err(err) => {
                eprintln!("Background tasks: T3 did not make the background project ({err:#}); its threads go to Heimdall's home project");
                home_project(calls).await.context("no T3 project for background threads")?
            }
        },
    };
    PROJECTS.lock().unwrap().push((folder.to_owned(), id.clone()));
    Ok(id)
}

async fn create(calls: &dyn Calls, root: &Path) -> Result<String> {
    let r = calls.call("t3_project_create", json!({"title": PROJECT_TITLE, "workspaceRoot": root.display().to_string(), "createWorkspaceRootIfMissing": true}), Duration::from_secs(30)).await?;
    let id = [&r["projectId"], &r["id"], &r["project"]["id"], &r["project"]["projectId"]].into_iter().find_map(|v| v.as_str()).map(str::to_owned);
    id.context("T3 made the background project but named no id")
}

/// The deepest registered project containing Heimdall's home folder (`emergency::T3Cover`'s rule).
async fn home_project(calls: &dyn Calls) -> Result<String> {
    let home = blocking(|| Ok(std::fs::canonicalize(heimdall_home::home_dir())?)).await?;
    let page = calls.call("t3_project_list", json!({"limit": 100}), Duration::from_secs(15)).await?;
    resolved_roots(&page).await?.into_iter()
        .filter(|(_, root)| home.starts_with(root))
        .max_by_key(|(_, root)| root.as_os_str().len())
        .map(|(id, _)| id).context("PROJECT_NOT_FOUND: no T3 project contains Heimdall's home folder")
}

/// A project page's (id, resolved root) pairs; the roots resolved on a blocking thread.
async fn resolved_roots(page: &Value) -> Result<Vec<(String, PathBuf)>> {
    let listed: Vec<(String, String)> = page["projects"].as_array().into_iter().flatten()
        .filter_map(|p| Some((p["id"].as_str().or(p["projectId"].as_str())?.to_owned(), p["workspaceRoot"].as_str()?.to_owned()))).collect();
    blocking(move || Ok(listed.into_iter().filter_map(|(id, r)| Some((id, std::fs::canonicalize(r).ok()?))).collect())).await
}

/// File-system work off the async runtime.
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(f).await.context("a background file-system step stopped")?
}

#[cfg(test)]
#[path = "background_t3_tests.rs"]
mod tests;
