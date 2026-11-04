//! Background Tasks ("one section for background tasks, and islands would be one of the background
//! tasks. Then we choose what model we want to use for background tasks"): the one model every cheap background job
//! runs on, and which jobs are on. The menu writes `Settings::background_model` and `Settings::background_off`; the
//! app publishes them here on every settings change, and each job reads them at each pass:
//! - Islands, the plan refresher (`plan_refresher::Config::endpoint`, `call`);
//! - Voice Names, the voice namer (`voice_namer::ask_model`, `call`).
//! - Day Timeline, the left island of where the day went (`day_timeline::spawn_live`).
//! A new job (proactive offers) adds a `ui::BackgroundTask` and reads `endpoint`, `anthropic_model` and `enabled`.
//! Every job's model call goes through `background_call` (the one-hour prompt cache, `max_tokens` at the model's
//! maximum, the $0.50 limit a run, usage and cost into the spend ledger).
//!
//! The jobs run the chosen model on its own provider (`endpoint`): Anthropic, Sign in with
//! ChatGPT, OpenCode Go, OpenRouter, a custom OpenAI-compatible URL, Own Engine or the built-in engine, each in its own
//! request format (`background_call::Wire`). A provider that is not connected makes no call: a local choice never
//! sends the plan or a transcript to the cloud, and no choice ever falls back to Anthropic or another paid provider.
//! A harness T3 runs is a provider too ("t3:<instance>", `background_call::t3`): the Claude subscription through T3
//! Code, one short-lived thread a call, no key.
//!
//! Each model may carry its own thinking level (`Settings::background_efforts`, Background Tasks ▸ Model's level
//! picker): published here beside the model (`publish_efforts`), read at each call (`effort_for`) in place of the job's
//! default effort. A model without one runs each job's own.
//!
//! A task may run its own model instead (`Settings::task_models`, Background Tasks ▸ a task's Model):
//! 03:37, the island agent on Claude Haiku 5.5 through T3 on the second Claude Code account (`ui::ISLANDS_DEFAULT`, the
//! personal edition's default) while Background Tasks ▸ Model keeps driving the other jobs. Such a task reads
//! `task_endpoint(task)`; the same rule holds there: no credential, a paused instance or a spent plan means no call,
//! never a fallback to the shared model or any other provider.

use serde::{Deserialize, Serialize};
#[cfg(not(test))]
use std::sync::RwLock;
pub use ui::BackgroundTask as Task;

/// (provider, model id); provider `ui::LOCAL_ENGINE` is the built-in engine's current model.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackgroundModel {
    pub provider: String,
    pub id: String,
}

impl Default for BackgroundModel {
    /// The personal edition's jobs run Claude Haiku 5.5 on Anthropic; the public edition starts with none (`is_none`),
    /// and no job makes a call until one is chosen.
    fn default() -> Self {
        if crate::edition::PUBLIC { Self { provider: String::new(), id: String::new() } }
        else { Self { provider: ui::BACKGROUND_DEFAULT.0.into(), id: ui::BACKGROUND_DEFAULT.1.into() } }
    }
}

impl BackgroundModel {
    /// No background model chosen (the public edition's blank start).
    pub fn is_none(&self) -> bool {
        self.provider.is_empty()
    }
    pub fn ui(&self) -> (String, String) {
        (self.provider.clone(), self.id.clone())
    }
    /// A menu choice, checked: a known provider (or the built-in engine, or a T3 instance this edition offers) and a
    /// plausible id. "t3:" alone names `ui::T3_DEFAULT_INSTANCE`.
    pub fn from_ui(provider: &str, id: &str) -> Option<BackgroundModel> {
        if let Some(instance) = ui::t3_instance(provider) {
            let ok = crate::background_call::t3::choice_ok(instance) && id.len() <= 256 && !id.trim().is_empty();
            return ok.then(|| BackgroundModel { provider: format!("{}{instance}", ui::T3_PROVIDER_PREFIX), id: id.into() });
        }
        let known = provider == ui::LOCAL_ENGINE || agent::provider::provider(provider).is_some();
        (known && id.len() <= 256 && (provider == ui::LOCAL_ENGINE || !id.trim().is_empty()))
            .then(|| BackgroundModel { provider: provider.into(), id: id.into() })
    }
}

/// A model's thinking level for background jobs, as the settings file keeps it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackgroundEffort {
    pub provider: String,
    pub id: String,
    pub level: String,
}

impl BackgroundEffort {
    pub fn ui(&self) -> (String, String, String) { (self.provider.clone(), self.id.clone(), self.level.clone()) }
    /// A menu pick, checked: a model `BackgroundModel::from_ui` takes and a level of plain letters (at most 32), or an
    /// empty level (the job's own effort again).
    pub fn from_ui(provider: &str, id: &str, level: &str) -> Option<BackgroundEffort> {
        let m = BackgroundModel::from_ui(provider, id)?;
        let plain = level.len() <= 32 && level.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        plain.then(|| BackgroundEffort { provider: m.provider, id: m.id, level: level.to_ascii_lowercase() })
    }
}

/// `efforts` with `pick` in place of its model's entry (an empty level removes it).
pub fn with_effort(efforts: &[BackgroundEffort], pick: &BackgroundEffort) -> Vec<BackgroundEffort> {
    let mut out: Vec<BackgroundEffort> = efforts.iter().filter(|e| !same_model(&e.provider, &e.id, &pick.provider, &pick.id)).cloned().collect();
    if !pick.level.is_empty() { out.push(pick.clone()); }
    out
}

/// One model under two spellings: "t3:" is the default instance; the built-in engine's empty id is its current model.
fn same_model(p1: &str, id1: &str, p2: &str, id2: &str) -> bool {
    let provider = |p: &str| ui::t3_instance(p).map_or_else(|| p.to_owned(), |i| format!("{}{i}", ui::T3_PROVIDER_PREFIX));
    if provider(p1) != provider(p2) { return false }
    let id = |p: &str, id: &str| if p == ui::LOCAL_ENGINE && id.is_empty() { agent::runtime::MODEL } else { id }.to_owned();
    id(p1, id1) == id(p2, id2)
}

/// A task's own model, as the settings file keeps it (`task`: `ui::BackgroundTask::key`; an unknown key is ignored).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskModel {
    pub task: String,
    pub provider: String,
    pub id: String,
}

impl TaskModel {
    pub fn ui(&self) -> Option<(Task, String, String)> { Some((Task::from_key(&self.task)?, self.provider.clone(), self.id.clone())) }
    fn model(&self) -> BackgroundModel { BackgroundModel { provider: self.provider.clone(), id: self.id.clone() } }
}

/// Whether `task`'s job reads its own model (`task_endpoint`): every task ("each with its own model
/// pick"); a new task decides here (no wildcard), and the menu action and the settings tool refuse a model for one that
/// does not. Deep search is not a task: it runs on Background Tasks ▸ Model.
pub fn reads_own_model(task: Task) -> bool {
    match task { Task::Islands | Task::Plan | Task::Goals | Task::VoiceNames | Task::DayTimeline | Task::Suggestions => true }
}

/// Whether `task` runs a tool loop (the suggester's searches): never on T3 (`background_call::t3::NoToolsOnT3`).
pub fn tool_task(task: Task) -> bool {
    match task { Task::Suggestions => true, Task::Islands | Task::Plan | Task::Goals | Task::VoiceNames | Task::DayTimeline => false }
}

/// Whether `task` may run `m`: a tool loop never on a T3 instance.
pub fn task_accepts(task: Task, m: &BackgroundModel) -> bool {
    !(tool_task(task) && ui::t3_instance(&m.provider).is_some())
}

/// The edition's task models before the user picks any: the personal edition's island agent and Goals on
/// `ui::ISLANDS_DEFAULT` (Claude Haiku 5.5 through T3 on the subscription: Goals' rolling session is cached there, the
/// path its climb measured, 31 Oct; the shared model may be one that caches this workload poorly); the public edition
/// none (every task on Background Tasks ▸ Model).
pub fn default_task_models() -> Vec<TaskModel> {
    if crate::edition::PUBLIC { return vec![] }
    [Task::Islands, Task::Goals].into_iter().map(goals_default_for).collect()
}

/// The personal edition's default model entry for `task` (`ui::ISLANDS_DEFAULT`).
pub fn goals_default_for(task: Task) -> TaskModel {
    TaskModel { task: task.key().into(), provider: ui::ISLANDS_DEFAULT.0.into(), id: ui::ISLANDS_DEFAULT.1.into() }
}

/// `models` with `task` on its own model (`Some`), or back on the shared one (`None`).
pub fn with_task_model(models: &[TaskModel], task: Task, model: Option<BackgroundModel>) -> Vec<TaskModel> {
    let mut out: Vec<TaskModel> = models.iter().filter(|m| m.task != task.key()).cloned().collect();
    if let Some(m) = model { out.push(TaskModel { task: task.key().into(), provider: m.provider, id: m.id }); }
    out
}

/// The tasks turned off, as the settings file keeps them (`ui::BackgroundTask::key`); unknown keys are ignored.
pub fn off_tasks(keys: &[String]) -> Vec<Task> {
    Task::ALL.into_iter().filter(|t| keys.iter().any(|k| k == t.key())).collect()
}

#[derive(Clone, Default)]
struct Published {
    model: BackgroundModel,
    off: Vec<Task>,
    efforts: Vec<BackgroundEffort>,
    /// `None` before the first settings load: the edition's defaults.
    tasks: Option<Vec<TaskModel>>,
    /// The built-in engine is on.
    engine: bool,
    /// The fallback may use the paid step.
    paid: bool,
}

#[cfg(not(test))]
static CURRENT: RwLock<Option<Published>> = RwLock::new(None);
#[cfg(not(test))]
fn read() -> Option<Published> { CURRENT.read().unwrap().clone() }
#[cfg(not(test))]
fn write(p: Published) { *CURRENT.write().unwrap() = Some(p); }
// Tests: each test thread has its own published state, so tests that build a pipeline (which publishes the
// defaults) cannot change another test's choice mid-assertion.
#[cfg(test)]
thread_local! { static CURRENT: std::cell::RefCell<Option<Published>> = const { std::cell::RefCell::new(None) }; }
#[cfg(test)]
fn read() -> Option<Published> { CURRENT.with(|c| c.borrow().clone()) }
#[cfg(test)]
fn write(p: Published) { CURRENT.with(|c| *c.borrow_mut() = Some(p)); }

/// The app's settings changed: every job's next pass reads this.
pub fn publish(model: &BackgroundModel, off_keys: &[String]) {
    let p = read().unwrap_or_default();
    write(Published { model: model.clone(), off: off_tasks(off_keys), ..p });
}

/// The tasks' own models changed (every settings change publishes them, after `publish`).
pub fn publish_tasks(models: &[TaskModel]) {
    let mut p = read().unwrap_or_default();
    p.tasks = Some(models.to_vec());
    write(p);
}

/// The model `task` runs: its own when the user gave it one (the edition's default before the first settings load), else
/// Background Tasks ▸ Model.
pub fn task_model(task: Task) -> BackgroundModel {
    let p = read().unwrap_or_default();
    let tasks = p.tasks.clone().unwrap_or_else(default_task_models);
    tasks.iter().find(|m| m.task == task.key()).map(TaskModel::model).unwrap_or(p.model)
}

/// `task`'s endpoint for its next call: its own model (else Background Tasks ▸ Model) on that model's provider, or, while
/// that one is spent or unavailable, the backend `background_fallback::route` moves it to (no switch mid-call: a job
/// resolves its endpoint right before each call). The model's thinking level applies as for any model
/// (`background_call::with_effort` reads the endpoint's model). Nothing usable: no credential, no call.
pub fn task_endpoint(task: Task) -> crate::plan_refresher::Endpoint {
    crate::background_fallback::route(Some(task), task_model(task), tool_task(task))
}

/// The user allowed the fallback's paid step, the Anthropic API key (`Settings::background_fallback_paid`, off by default).
pub fn fallback_paid() -> bool { read().is_some_and(|p| p.paid) }
/// The paid-fallback switch changed (every settings change publishes it).
pub fn publish_fallback_paid(on: bool) {
    let mut p = read().unwrap_or_default();
    p.paid = on;
    write(p);
}

/// The built-in engine is on (a fallback step; `publish_engine`).
pub fn engine_on() -> bool { read().is_some_and(|p| p.engine) }
/// The engine switch changed (every settings change publishes it).
pub fn publish_engine(on: bool) {
    let mut p = read().unwrap_or_default();
    p.engine = on;
    write(p);
}

/// The model OpenCode Go would run as a fallback: the shared model when OpenCode Go lists it, else its default; `None`
/// without its catalog (not connected, or no hub).
pub fn opencode_model() -> Option<String> {
    let hub = hub()?;
    let shared = current();
    let listed = [shared.id.as_str(), agent::provider::HAIKU].into_iter().find(|id| hub.api_model(agent::provider::OPENCODE_GO, id).is_some()).map(str::to_owned);
    listed.or_else(|| hub.default_model(agent::provider::OPENCODE_GO))
}

/// The models' thinking levels changed (every settings change publishes them, after `publish`).
pub fn publish_efforts(efforts: &[BackgroundEffort]) {
    let mut p = read().unwrap_or_default();
    p.efforts = efforts.to_vec();
    write(p);
}

/// The level the user picked for `model` on `provider` (an endpoint's provider id and model), if any.
pub fn effort_for(provider: &str, model: &str) -> Option<String> {
    read()?.efforts.into_iter().find(|e| same_model(&e.provider, &e.id, provider, model)).map(|e| e.level).filter(|l| !l.is_empty())
}

/// The background model as last published (the default before the first settings load).
pub fn current() -> BackgroundModel {
    read().unwrap_or_default().model
}

/// Whether the user left `task` on in Background Tasks (all are, before the first settings load).
pub fn enabled(task: Task) -> bool {
    !read().is_some_and(|p| p.off.contains(&task))
}

/// Background Tasks ▸ Model's endpoint for a caller that is not a task (tests, lab runs): the shared model on its own
/// provider, the same resolution as `tool_endpoint`. Not a task, so no fallback and no sign (review 30 Oct): while that
/// provider is spent its calls stop at the gate. No credential: no call. The tasks read `task_endpoint`.
#[cfg_attr(not(test), allow(dead_code))]
pub fn endpoint() -> crate::plan_refresher::Endpoint { endpoint_for(current()) }

/// Background Tasks ▸ Model's endpoint for deep search and the background probe (not tasks: no fallback, no sign; the
/// tasks read `task_endpoint`). Deep search runs an in-app
/// tool loop on Background Tasks ▸ Model, "with ANY provider" he picks there; 22:10: the existing jobs too): the model
/// on its own provider, in the format that provider serves it in (`background_call::Wire`).
/// - Anthropic: the stored key and workspace; Anthropic prices and the $0.50 limit a run.
/// - Sign in with ChatGPT: the Responses API at `agent::chatgpt::API_BASE` with the running app's token keeper's access
///   token (`api_providers::Hub::chatgpt_access_token`; a lab binary's `HEIMDALL_CHATGPT_ACCESS_TOKEN`).
/// - OpenCode Go: the format its catalog entry names (Claude models on Messages at its Anthropic base, the rest on its
///   OpenAI base), its stored key.
/// - OpenRouter: chat completions at its OpenAI base, its stored key.
/// - Custom ("openai-compatible") and Own Engine: chat completions (Responses when the model's entry says so) at the
///   saved base URL, the stored key (a keyless engine's `NO_KEY`).
/// - The built-in engine (`ui::LOCAL_ENGINE`): Splash's OpenAI-compatible server (`agent::runtime::BASE_URL`), no key;
///   an empty id is the engine's model. When the engine is off, the connect is refused: free, nothing sent.
/// No credential (a provider not connected, a custom one without its URL): `key` is `None` and no call is made. The
/// model's catalog entry and the ChatGPT token come from the app's provider hub (`set_hub`); without it (tests, lab)
/// the provider's defaults and the environment's key apply. Blocking (the ChatGPT token may be refreshed): call it
/// from a thread outside the async runtime.
pub fn tool_endpoint() -> crate::plan_refresher::Endpoint {
    endpoint_for(current())
}

/// `tool_endpoint` for a model that is not the published choice (the compaction fallback's other subscriptions and the
/// built-in engine, bug note 2026-10-28T19-41-00): the same resolution, the same rule that no credential means no call.
pub fn endpoint_for(m: BackgroundModel) -> crate::plan_refresher::Endpoint {
    // Nothing chosen: no endpoint and no key, so no call.
    if m.is_none() { return crate::plan_refresher::Endpoint { timeout: crate::background_call::TIMEOUT, ..Default::default() }; }
    // A T3 instance: the app's T3 client, no key (no client: no call).
    if ui::t3_instance(&m.provider).is_some() { return crate::background_call::t3::endpoint(&m.provider, &m.id); }
    if m.provider == agent::provider::ANTHROPIC {
        return crate::plan_refresher::Endpoint { model: Some(m.id), ..crate::plan_refresher::Endpoint::anthropic() };
    }
    let hub = hub();
    let entry = hub.as_ref().and_then(|h| h.api_model(&m.provider, &m.id));
    let p = agent::provider::provider(&m.provider);
    let key = match p {
        Some(p) if p.id == agent::provider::CHATGPT => hub.as_ref().and_then(|h| h.chatgpt_access_token()).or_else(|| agent::provider::api_key(p).ok()),
        Some(p) => agent::provider::api_key(p).ok().or_else(|| (!p.key_required).then(|| agent::provider::NO_KEY.into())),
        None => None,
    };
    resolve(&m, p, entry.as_ref(), key, p.and_then(agent::provider::base_url))
}

/// `tool_endpoint` without the Keychain or the hub: the choice, its provider (`None`: the built-in engine), its catalog
/// entry, its credential and a custom provider's saved base URL, to an endpoint. Each provider is its own billing route
/// (the Codex subscription, the OpenAI API, the Anthropic API, the OpenCode Go subscription, Custom
/// and Own Engine "must never mix or fall back"): the endpoint is that provider's or has no key, never another's.
/// Everything here is read from the provider's catalog entry (`agent::provider::PROVIDERS`), so a new provider such as
/// a metered "OpenAI API" (Responses at OpenAI's own API base, a pasted key) needs only its entry: a provider whose
/// OpenAI base is OpenAI's own API speaks Responses unless its model's entry names another format.
fn resolve(m: &BackgroundModel, p: Option<&'static agent::provider::ProviderInfo>, entry: Option<&agent::provider::ApiModel>, key: Option<String>, saved_base: Option<String>) -> crate::plan_refresher::Endpoint {
    use crate::background_call::Wire;
    use agent::provider::Api;
    let mut e = crate::plan_refresher::Endpoint { timeout: crate::background_call::TIMEOUT, model: Some(m.id.clone()), ..Default::default() };
    let Some(p) = p else {
        // The built-in engine, the one choice that is not a provider.
        e.provider = ui::LOCAL_ENGINE;
        e.wire = Wire::ChatCompletions;
        e.base = format!("{}/v1", agent::runtime::BASE_URL);
        e.key = Some(agent::provider::NO_KEY.into());
        e.reasoning = true; // Pi's Splash entry: `supportsReasoningEffort`
        if m.id.trim().is_empty() { e.model = Some(agent::runtime::MODEL.into()) }
        return e;
    };
    e.provider = p.id;
    e.wire = match entry.map(|a| a.api) {
        Some(Api::AnthropicMessages) => Wire::Anthropic,
        Some(Api::OpenaiResponses | Api::OpenaiCodexResponses) => Wire::Responses,
        Some(_) => Wire::ChatCompletions,
        None if p.openai_base == Some(agent::chatgpt::API_BASE) => Wire::Responses,
        None if p.openai_base.is_none() && (p.anthropic_base.is_some() || p.key_header == agent::provider::KeyHeader::XApiKey) => Wire::Anthropic,
        None => Wire::ChatCompletions,
    };
    let fixed = match e.wire { Wire::Anthropic => p.anthropic_base, _ => p.openai_base };
    e.base = entry.and_then(|a| a.base_url.clone()).or(saved_base).or_else(|| fixed.map(str::to_owned)).unwrap_or_default();
    e.key = key.filter(|_| !e.base.is_empty());
    e.max_output = entry.map(|a| a.max_output).filter(|n| *n > 0);
    e.efforts = entry.map(|a| a.efforts.clone()).unwrap_or_default();
    // The Codex subscription's models all reason (Sol, Terra, Luna); others say so in their catalog entry.
    e.reasoning = entry.is_some_and(|a| a.reasoning) || p.id == agent::provider::CHATGPT;
    e
}

/// `tool_endpoint` with a credential the caller already holds (`background_probe`: the stored ChatGPT session's token,
/// whose length no key check bounds), without the hub.
pub fn endpoint_with_key(key: String) -> crate::plan_refresher::Endpoint {
    let m = current();
    if m.is_none() { return crate::plan_refresher::Endpoint { timeout: crate::background_call::TIMEOUT, ..Default::default() }; }
    if ui::t3_instance(&m.provider).is_some() { return crate::background_call::t3::endpoint(&m.provider, &m.id); }
    let p = agent::provider::provider(&m.provider);
    resolve(&m, p, None, Some(key), p.and_then(agent::provider::base_url))
}

/// The app's provider hub, for `tool_endpoint` (the pipeline registers it at launch, beside the spend ledger).
static HUB: std::sync::RwLock<Option<std::sync::Weak<crate::api_providers::Hub>>> = std::sync::RwLock::new(None);
pub fn set_hub(h: &std::sync::Arc<crate::api_providers::Hub>) { *HUB.write().unwrap() = Some(std::sync::Arc::downgrade(h)) }
fn hub() -> Option<std::sync::Arc<crate::api_providers::Hub>> { HUB.read().unwrap().as_ref()?.upgrade() }

/// The model id a test endpoint without `model` names (every real endpoint names the choice itself): the choice when
/// it is an Anthropic model, else Claude Haiku 5.5.
pub fn anthropic_model() -> String {
    anthropic_model_for(&current())
}

fn anthropic_model_for(m: &BackgroundModel) -> String {
    if m.provider == agent::provider::ANTHROPIC { m.id.clone() } else { agent::provider::HAIKU.into() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jobs_run_the_chosen_anthropic_model_and_haiku_otherwise() {
        if !crate::edition::PUBLIC { assert_eq!(BackgroundModel::default(), BackgroundModel { provider: "anthropic".into(), id: "claude-haiku-5-5".into() }); }
        assert_eq!(anthropic_model_for(&BackgroundModel { provider: "anthropic".into(), id: "claude-sonnet-5-5".into() }), "claude-sonnet-5-5");
        assert_eq!(anthropic_model_for(&BackgroundModel { provider: "openai-compatible".into(), id: "moonshotai/kimi-k3".into() }), "claude-haiku-5-5");
        assert_eq!(anthropic_model_for(&BackgroundModel { provider: ui::LOCAL_ENGINE.into(), id: String::new() }), "claude-haiku-5-5");
    }
    #[test]
    fn a_choice_off_anthropic_never_sends_anything_to_anthropic() {
        publish(&BackgroundModel { provider: ui::LOCAL_ENGINE.into(), id: "qwen3.8-27b".into() }, &[]);
        let e = endpoint();
        assert!(e.base.starts_with("http://127.0.0.1:") && e.provider == ui::LOCAL_ENGINE, "the local engine, never the cloud: {}", e.base);
        publish(&BackgroundModel { provider: "openai-compatible".into(), id: "moonshotai/kimi-k3".into() }, &[]);
        let e = endpoint();
        assert!(!e.base.contains("anthropic.com") && e.model.as_deref() == Some("moonshotai/kimi-k3"), "no fallback to Anthropic");
        publish(&BackgroundModel { provider: "anthropic".into(), id: "claude-opus-5-5".into() }, &[]);
        assert_eq!(endpoint().model.as_deref(), Some("claude-opus-5-5"), "the model is read with the key");
        publish(&BackgroundModel::default(), &[]);
    }
    #[test]
    fn every_job_reads_the_one_model_and_its_own_toggle() {
        publish(&BackgroundModel { provider: "anthropic".into(), id: "claude-sonnet-5-5".into() }, &["voice_names".into(), "someday".into()]);
        assert!(enabled(Task::Islands));
        assert!(!enabled(Task::VoiceNames));
        assert_eq!(crate::plan_refresher::request_body("p")["model"], "claude-sonnet-5-5");
        assert_eq!(crate::voice_namer::request_body("p")["model"], "claude-sonnet-5-5");
        publish(&BackgroundModel::default(), &[]);
        assert!(Task::ALL.into_iter().all(enabled));
        assert_eq!(crate::voice_namer::request_body("p")["model"], "claude-haiku-5-5");
    }
    /// Deep search: every provider on the menu resolves to a base URL, a credential and a format;
    /// no credential, no call.
    #[test]
    fn deep_search_reaches_the_background_model_on_any_provider() {
        use crate::background_call::Wire;
        use agent::provider::{Api, ApiModel};
        let bm = |p: &str, id: &str| BackgroundModel { provider: p.into(), id: id.into() };
        let entry = |p: &str, id: &str, api: Api, base: Option<&str>| ApiModel { provider: p.into(), id: id.into(), name: id.into(), api, context: 200_000,
            max_output: 64_000, images: false, reasoning: true, cost: None, context_budget: None, thinking: None, base_url: base.map(str::to_owned), efforts: vec![] };
        let k = || Some("sk-test".to_owned());
        let at = |e: crate::plan_refresher::Endpoint| (e.wire, e.base, e.key, e.model, e.provider);
        // Sign in with ChatGPT: Responses on OpenAI's API, the keeper's token.
        assert_eq!(at(resolve(&bm("openai", "gpt-6.1-sol"), agent::provider::provider("openai"), None, Some("access-1".into()), None)),
            (Wire::Responses, "https://api.openai.com/v1".into(), Some("access-1".into()), Some("gpt-6.1-sol".into()), "openai"));
        // F11: the account's reasoning levels reach the jobs' requests.
        let sol = ApiModel { efforts: vec!["low".into(), "max".into()], ..entry("openai", "gpt-6.1-sol", Api::OpenaiResponses, None) };
        assert_eq!(resolve(&bm("openai", "gpt-6.1-sol"), agent::provider::provider("openai"), Some(&sol), Some("access-1".into()), None).efforts, ["low", "max"]);
        // OpenCode Go: its Claude models on Messages, the others on its OpenAI base.
        let go = entry("opencode-go", "claude-haiku-5-5", Api::AnthropicMessages, None);
        let e = resolve(&bm("opencode-go", "claude-haiku-5-5"), agent::provider::provider("opencode-go"), Some(&go), k(), None);
        assert_eq!((e.wire, e.base.as_str(), e.max_output), (Wire::Anthropic, "https://opencode.ai/inference/go/anthropic", Some(64_000)));
        assert_eq!(crate::background_call::Loop::for_endpoint(&e).limit, None, "a subscription: rounds, not dollars");
        let kimi = entry("opencode-go", "kimi-k3", Api::OpenaiCompletions, None);
        assert_eq!(at(resolve(&bm("opencode-go", "kimi-k3"), agent::provider::provider("opencode-go"), Some(&kimi), k(), None)).0, Wire::ChatCompletions);
        assert_eq!(resolve(&bm("opencode-go", "kimi-k3"), agent::provider::provider("opencode-go"), None, k(), None).base, "https://opencode.ai/inference/go/openai/v1");
        // Custom providers: the saved URL (or the model's own), the stored key; not connected: no call.
        assert_eq!(at(resolve(&bm("openai-compatible", "m"), agent::provider::provider("openai-compatible"), None, k(), Some("http://10.0.0.2:8000/v1".into()))),
            (Wire::ChatCompletions, "http://10.0.0.2:8000/v1".into(), k(), Some("m".into()), "openai-compatible"));
        let own = entry("openai-compatible", "qwen", Api::OpenaiCompletions, Some("http://127.0.0.1:1234/v1"));
        assert_eq!(resolve(&bm("openai-compatible", "qwen"), agent::provider::provider("openai-compatible"), Some(&own), Some("local".into()), None).base, "http://127.0.0.1:1234/v1");
        // Anthropic-compatible: the Messages API at its root, also before its catalog entry is known.
        let claude = entry("anthropic-compatible", "claude-lab", Api::AnthropicMessages, Some("http://127.0.0.1:8080"));
        assert_eq!(at(resolve(&bm("anthropic-compatible", "claude-lab"), agent::provider::provider("anthropic-compatible"), Some(&claude), k(), None)).0, Wire::Anthropic);
        assert_eq!(at(resolve(&bm("anthropic-compatible", "x"), agent::provider::provider("anthropic-compatible"), None, k(), Some("http://127.0.0.1:8080".into()))).0, Wire::Anthropic);
        assert!(resolve(&bm("openai-compatible", "m"), agent::provider::provider("openai-compatible"), None, k(), None).key.is_none(), "no URL saved: no call");
        assert!(resolve(&bm("openai-compatible", "m"), agent::provider::provider("openai-compatible"), None, None, Some("http://10.0.0.2:8000/v1".into())).key.is_none(), "not connected: no call");
        // The built-in engine: Splash's local server, no key, its model when none is named.
        assert_eq!(at(resolve(&bm(ui::LOCAL_ENGINE, ""), agent::provider::provider(ui::LOCAL_ENGINE), None, None, None)),
            (Wire::ChatCompletions, "http://127.0.0.1:8100/v1".into(), Some("local".into()), Some("qwen3.8-27b".into()), ui::LOCAL_ENGINE));
        // A metered OpenAI API provider is one catalog entry away: Responses at OpenAI's API with its own key, never the
        // Codex subscription's token. Leaked once, so the test can hand it in as `'static`.
        let openai_api: &'static agent::provider::ProviderInfo = Box::leak(Box::new(agent::provider::ProviderInfo {
            id: "openai-api", name: "OpenAI API", openai_base: Some(agent::chatgpt::API_BASE), key_required: true, subscription: false, sign_in: false,
            ..*agent::provider::provider("openai-compatible").unwrap() }));
        assert_eq!(at(resolve(&bm("openai-api", "gpt-6.1"), Some(openai_api), None, k(), None)),
            (Wire::Responses, "https://api.openai.com/v1".into(), k(), Some("gpt-6.1".into()), "openai-api"));
        assert_eq!(crate::background_call::Loop::for_endpoint(&resolve(&bm("openai-api", "gpt-6.1"), Some(openai_api), None, k(), None)).limit, None);
        // Anthropic: priced, with the $0.50 limit.
        publish(&bm("anthropic", "claude-sonnet-5-5"), &[]);
        let e = tool_endpoint();
        assert_eq!((e.wire, e.model.as_deref(), crate::background_call::Loop::for_endpoint(&e).limit), (Wire::Anthropic, Some("claude-sonnet-5-5"), Some(0.5)));
        publish(&bm("opencode-go", "kimi-k3"), &[]);
        assert_eq!((tool_endpoint().wire, endpoint().wire), (Wire::ChatCompletions, Wire::ChatCompletions), "jobs and deep search: one resolution");
        publish(&BackgroundModel::default(), &[]);
    }
    /// The public edition starts with no background model: no endpoint, no key, so no job makes a call.
    #[test]
    fn the_public_edition_background_model_starts_empty_and_calls_nothing() {
        let none = BackgroundModel { provider: String::new(), id: String::new() };
        assert!(none.is_none());
        assert_eq!(BackgroundModel::default().is_none(), crate::edition::PUBLIC);
        publish(&none, &[]);
        let (e, d) = (tool_endpoint(), endpoint());
        assert!(e.key.is_none() && e.base.is_empty() && d.key.is_none(), "nothing chosen: no call");
        assert!(endpoint_with_key("sk-x".into()).key.is_none());
        publish(&BackgroundModel::default(), &[]);
    }
    #[test]
    fn menu_choices_are_checked() {
        assert!(BackgroundModel::from_ui("anthropic", "claude-haiku-5-5").is_some());
        assert!(BackgroundModel::from_ui(ui::LOCAL_ENGINE, "qwen3.8-27b").is_some());
        assert!(BackgroundModel::from_ui("nowhere", "x").is_none());
        assert!(BackgroundModel::from_ui("anthropic", " ").is_none());
    }
    /// the island agent runs its own model (Claude Haiku 5.5 through T3 on Claude code 2 by
    /// default) while his shared Background Tasks ▸ Model (Luna) drives every other job; its thinking level applies;
    /// without a T3 connection, or with the instance's plan spent, it makes no call and never falls back to Luna.
    #[test]
    fn a_task_runs_its_own_model_and_the_others_the_shared_one() {
        use crate::background_call::t3;
        let luna = BackgroundModel { provider: "openai".into(), id: "gpt-6-luna".into() };
        publish(&luna, &[]);
        // Before the first settings load: the edition's default.
        let islands = task_endpoint(Task::Islands);
        if crate::edition::PUBLIC { assert_eq!(islands.model.as_deref(), Some("gpt-6-luna")); }
        else { assert_eq!((islands.provider, islands.model.as_deref()), ("t3:claudeAgent_claude_code_2", Some("claude-haiku-5-5"))); }
        publish_tasks(&default_task_models());
        assert_eq!(task_endpoint(Task::Plan).model.as_deref(), Some("gpt-6-luna"), "the other jobs keep the shared model");
        assert_eq!(task_endpoint(Task::DayTimeline).provider, endpoint().provider);
        if !crate::edition::PUBLIC {
            // No T3 client registered on this thread: no key, no call, and no fallback to Luna.
            let e = task_endpoint(Task::Islands);
            assert!(e.key.is_none() && e.provider.starts_with("t3:") && e.model.as_deref() == Some("claude-haiku-5-5"));
            // Connected: a key; the model's own level applies to the task's model.
            struct Never;
            #[async_trait::async_trait]
            impl t3::Calls for Never {
                async fn call(&self, name: &str, _: serde_json::Value, _: std::time::Duration) -> anyhow::Result<serde_json::Value> { anyhow::bail!("{name}: no call expected") }
            }
            t3::set_calls(std::sync::Arc::new(Never), std::env::temp_dir().join("heimdall-task-endpoint-test"), t3::Limits::default());
            let e = task_endpoint(Task::Islands);
            assert!(e.key.is_some());
            publish_efforts(&[BackgroundEffort { provider: ui::ISLANDS_DEFAULT.0.into(), id: ui::ISLANDS_DEFAULT.1.into(), level: "low".into() }]);
            let c = crate::background_call::Call { job: "island agent", model: "claude-haiku-5-5".into(), system: "s", schema: serde_json::json!({"type": "object"}), effort: "medium", prompt: "x".into(), history: Vec::new(), session: None };
            assert_eq!(crate::background_call::with_effort(&e, &c).effort, "low");
            // The instance's plan spent: the call stops at the gate, nothing reaches T3 or another provider.
            crate::background_gate::quota_spent(e.provider, Some(agent::now_ms() + 3_600_000), agent::now_ms());
            let err = crate::background_call::ask_blocking(&e, &c).unwrap_err();
            assert!(err.downcast_ref::<crate::background_gate::QuotaOut>().is_some(), "{err:#}");
            crate::background_gate::quota_back(e.provider);
            publish_efforts(&[]);
        }
        // A task's own model, then back on the shared one.
        let sonnet = BackgroundModel { provider: "anthropic".into(), id: "claude-sonnet-5-5".into() };
        publish_tasks(&with_task_model(&[], Task::VoiceNames, Some(sonnet)));
        assert_eq!(task_endpoint(Task::VoiceNames).model.as_deref(), Some("claude-sonnet-5-5"));
        assert_eq!(task_endpoint(Task::Islands).model.as_deref(), Some("gpt-6-luna"), "Islands put back on the shared model");
        publish_tasks(&with_task_model(&default_task_models(), Task::Islands, None));
        assert_eq!(task_endpoint(Task::Islands).model.as_deref(), Some("gpt-6-luna"));
        publish(&BackgroundModel::default(), &[]);
        publish_tasks(&default_task_models());
    }
    /// Review 30 Oct: per-task models were saved but only Islands read one. Every task's job reads `task_endpoint` with
    /// its own task, never the shared `endpoint()`.
    #[test]
    fn every_task_job_reads_its_own_endpoint() {
        let src = |f: &str| std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src").join(f)).unwrap();
        // Up to the file's test module (`mod tests`), not its first `#[cfg(test)]` item.
        let shipping = |f: &str| { let s = src(f); s.find("#[cfg(test)]\nmod tests").or_else(|| s.find("#[cfg(test)]\n#[path")).map_or(s.clone(), |at| s[..at].to_owned()) };
        for (file, task) in [("day_timeline.rs", "Task::DayTimeline"), ("pipeline/voice_names.rs", "Task::VoiceNames"), ("island_agent.rs", "Task::Islands"), ("pipeline/suggest.rs", "Task::Suggestions"), ("goals/mod.rs", "Task::Goals")] {
            let s = shipping(file);
            assert!(s.contains(&format!("task_endpoint(crate::background_model::{task})")), "{file} reads {task}'s own model");
            assert!(!s.contains("background_model::endpoint()"), "{file} reads the shared model");
        }
        assert!(Task::ALL.into_iter().all(reads_own_model));
    }
    #[test]
    fn a_settings_file_without_the_field_loads_haiku_with_every_task_on() {
        let s: crate::settings::Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(s.background_model, BackgroundModel::default());
        assert!(s.background_off.is_empty());
        assert!(s.engine_context.is_empty());
    }
}
