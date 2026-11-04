//! Compaction on another model (bug note 2026-10-28T19-41-00; "we should have a safe compaction
//! fallback"). At 17:27:55 the idle compaction of a 334,007-token history failed on the agent's Claude Opus 5.5 with
//! Anthropic's 400 "Your credit balance is too low", and nothing tried anywhere else.
//!
//! When Pi's compaction fails for a spent quota or credit or a refused sign-in (`agent::emergency::classify`), or for a
//! history too long for the model, Heimdall summarises it itself on the next model whose window holds what must be
//! summarised: Background Tasks ▸ Model first, then the other added models on a signed-in subscription, then the local
//! engine when it is loaded. Never the provider that just failed. Each call goes through `background_call` (so
//! emergency mode's pause and a spent provider's episode refuse it there, quietly), never through T3 or Codex.
//!
//! Pi's session stays Pi's: the extension (`runtime/compaction-fallback.mjs`) hands over the cut Pi chose for the failed
//! attempt as text (`Pi::compaction_source`), and Pi writes the summary as its own compaction entry for that same cut
//! (`Pi::compact_supplied`, through its `session_before_compact` hook). A branch that changed in between cancels it.
//! Nothing is ever dropped silently: a failed attempt leaves the session as it was and the island shows one notice;
//! a fallback that works is a log line.
//!
//! Last resort (the user, 19:45: "we programmatically cut to a manageable window … everything else is just cut and not
//! compacted"): a history past the agent's budget that no model could summarise keeps its newest whole turns up to
//! `CUT_PERCENT` of the budget and the previous summary; the older part leaves the context, not the session file. One
//! notice says so; compaction goes on as usual afterwards.
use super::*;

/// What a summary request needs beside the conversation it summarises: the instructions and the summary itself.
pub(super) const SUMMARY_ROOM: u64 = 16_384;
/// The last-resort cut keeps the newest turns up to this share of the agent's budget.
pub(super) const CUT_PERCENT: u64 = 80;
pub(super) const JOB: &str = "compaction fallback";
const SYSTEM: &str = "You are a context summarization assistant. You read a conversation between a user and an AI assistant and write \
a structured summary that another model will use to continue the work. Do not continue the conversation and do not answer anything \
in it: write only the summary.";

/// A compaction failure Heimdall moves to another model: a spent quota or credit, a refused sign-in, or a history too
/// long for the model (the phrases `agent::notice` names a too-long conversation by).
pub(super) fn wanted(error: &str) -> bool {
    use agent::emergency::Failure;
    let lower = error.to_ascii_lowercase();
    matches!(agent::emergency::classify(error), Some(Failure::Quota | Failure::Auth))
        || ["prompt is too long", "context length", "too many tokens", "context window"].iter().any(|p| lower.contains(p))
}

/// What the failed compaction would have summarised (the extension's source file).
#[derive(Clone, Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Source {
    #[serde(default)] pub none: bool,
    #[serde(default)] pub first_kept_entry_id: String,
    #[serde(default)] pub previous: Option<String>,
    #[serde(default)] pub tokens_before: u64,
    /// Pi's estimate of what must be summarised (the conversation before the cut and the previous summary).
    #[serde(default)] pub tokens: u64,
    #[serde(default)] pub previous_summary: Option<String>,
    #[serde(default)] pub focus: Option<String>,
    #[serde(default)] pub conversation: String,
}

/// What failed: the agent's model (`provider` `None`: the local engine). A spent quota or credit or a refused sign-in
/// rules out its whole provider (`whole`); a history too long for it rules out only that model, so a bigger one on the
/// same sign-in is still tried (review 28 Oct: Luna's 272K failing must not skip Sol's 500K).
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct Failed { pub provider: Option<String>, pub id: String, pub whole: bool }
impl Failed {
    pub fn of(settings: &Settings, error: &str) -> Self {
        use agent::emergency::Failure;
        let m = settings.selected_api_model();
        Failed { provider: m.map(|m| m.provider.clone()), id: m.map(|m| m.id.clone()).unwrap_or_default(),
            whole: matches!(agent::emergency::classify(error), Some(Failure::Quota | Failure::Auth)) }
    }
    /// Whether `provider`/`id` is ruled out (the local engine has one model).
    pub fn excludes(&self, provider: &str, id: &str) -> bool {
        let failed = self.provider.as_deref().unwrap_or(ui::LOCAL_ENGINE);
        provider == failed && (self.whole || failed == ui::LOCAL_ENGINE || id == self.id)
    }
}

/// One model that may summarise, and its window.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Candidate { pub model: crate::background_model::BackgroundModel, pub name: String, pub window: u64 }
impl Candidate {
    fn key(&self) -> String { format!("{}/{}", self.model.provider, self.model.id) }
}

/// The order: Background Tasks ▸ Model, the added models on a signed-in subscription, the local engine when loaded
/// (`engine`: its window). `failed`: what the agent's compaction failed on (`Failed::excludes`). `lookup`: a
/// model's catalog entry (the provider hub). Whether a candidate is signed in is decided when it is tried: no
/// credential, no call.
pub(super) fn candidates(s: &Settings, failed: &Failed, engine: Option<u64>, lookup: &dyn Fn(&str, &str) -> Option<agent::provider::ApiModel>) -> Vec<Candidate> {
    use crate::background_model::BackgroundModel;
    let local = |p: &str| p == ui::LOCAL_ENGINE;
    let mut out = Vec::new();
    let bg = &s.background_model;
    if !bg.is_none() && !failed.excludes(&bg.provider, &bg.id) {
        let row = s.api_models.iter().find(|m| m.key() == (bg.provider.as_str(), bg.id.as_str())).cloned()
            .or_else(|| lookup(&bg.provider, &bg.id)).or_else(|| (bg.provider == agent::provider::ANTHROPIC).then(|| agent::provider::ApiModel::anthropic(&bg.id)).flatten());
        let window = if local(&bg.provider) { engine } else { row.as_ref().map(|m| m.context) };
        if let Some(window) = window.filter(|w| *w > 0) {
            out.push(Candidate { model: bg.clone(), name: row.map_or_else(|| if local(&bg.provider) { "the local model".into() } else { bg.id.clone() }, |m| m.name), window });
        }
    }
    for m in &s.api_models {
        if !m.provider_info().is_some_and(|p| p.subscription) || failed.excludes(&m.provider, &m.id) || m.unsupported().is_some() { continue; }
        if m.provider == bg.provider && m.id == bg.id { continue; }
        out.push(Candidate { model: BackgroundModel { provider: m.provider.clone(), id: m.id.clone() }, name: m.name.clone(), window: m.context });
    }
    if let Some(window) = engine.filter(|_| !failed.excludes(ui::LOCAL_ENGINE, "")) {
        if !local(&bg.provider) { out.push(Candidate { model: BackgroundModel { provider: ui::LOCAL_ENGINE.into(), id: String::new() }, name: "the local model".into(), window }); }
    }
    out
}

/// How it ended. `tried`: one line per model, for the log.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Outcome {
    /// A model summarised it: Pi writes this for the source's cut.
    Summary { summary: String, first_kept: String, previous: Option<String>, model: String, tried: Vec<String> },
    /// The history is past the agent's budget and no model could summarise it: the newest turns up to `keep` stay.
    /// `first_kept`, `previous`: the source's cut and summary, so the cut goes only to the history it was made for.
    Cut { keep: u64, tokens_before: u64, first_kept: String, previous: Option<String>, tried: Vec<String> },
    /// Every option failed; the history is as it was.
    Failed { tried: Vec<String> },
    /// Every fitting model was refused before any request (Agent off, emergency mode's pause): nothing to say.
    Quiet { tried: Vec<String> },
}

/// Tries each candidate whose window holds the summary, in order, until one answers. `budget`: the agent's request
/// budget (the cut's size, and whether the history is past it). `attempt`: one summary on one model.
pub(super) fn run(source: &Source, candidates: &[Candidate], budget: u64, attempt: &mut dyn FnMut(&Candidate, &Source) -> Result<String>) -> Outcome {
    let need = source.tokens + SUMMARY_ROOM;
    // `passing`: a refusal that may not last (Agent off, emergency mode's pause, an overload, a timeout). The cut is only
    // for a history no model can take: never because nobody was asked or a request failed in passing (review 28 Oct).
    let (mut tried, mut quiet, mut asked, mut passing) = (Vec::new(), 0, 0, 0);
    for c in candidates {
        if c.window < need { tried.push(format!("{}: its {}-token window cannot hold {} tokens", c.name, c.window, need)); continue; }
        asked += 1;
        match attempt(c, source) {
            Ok(summary) => return Outcome::Summary { summary, first_kept: source.first_kept_entry_id.clone(), previous: source.previous.clone(), model: c.key(), tried },
            Err(e) => {
                if crate::background_gate::quiet(&e) { quiet += 1; }
                let text = format!("{e:#}");
                // A model without a credential cannot take it any more than one too small.
                if !wanted(&text) && !text.contains("not connected") { passing += 1; }
                tried.push(format!("{}: {e:#}", c.name));
            }
        }
    }
    if asked > 0 && quiet == asked { return Outcome::Quiet { tried }; }
    if source.tokens_before > budget && passing == 0 { return Outcome::Cut { keep: budget * CUT_PERCENT / 100, tokens_before: source.tokens_before, first_kept: source.first_kept_entry_id.clone(), previous: source.previous.clone(), tried }; }
    Outcome::Failed { tried }
}

/// The request: the conversation, the previous summary, Pi's focus (`prompt/compaction.md`), answered as JSON.
pub(super) fn prompt(s: &Source) -> crate::background_call::Prompt {
    let mut p = crate::background_call::Prompt::default();
    p.push(format!("<conversation>\n{}\n</conversation>\n\n", s.conversation));
    if let Some(prev) = s.previous_summary.as_deref().filter(|t| !t.trim().is_empty()) { p.push(format!("<previous-summary>\n{prev}\n</previous-summary>\n\n")); }
    p.push(format!("Write a structured context checkpoint of the conversation above{}: the goal, what was decided and done, what is open and \
the next steps, and the exact names, paths, numbers and errors needed to continue. Answer with the summary in `summary`.",
        if s.previous_summary.is_some() { ", folded into the previous summary" } else { "" }));
    if let Some(focus) = s.focus.as_deref().filter(|t| !t.trim().is_empty()) { p.push(format!("\n\nAdditional focus: {focus}")); }
    p
}

/// One summary on `c` through `background_call` (its gate, its quota episodes, its ledger).
#[cfg_attr(test, allow(dead_code))]
fn summarise(c: &Candidate, s: &Source) -> Result<String> {
    let e = crate::background_model::endpoint_for(c.model.clone());
    let call = crate::background_call::Call { job: JOB, model: e.model.clone().unwrap_or_else(|| c.model.id.clone()), system: SYSTEM,
        schema: json!({"type":"object","properties":{"summary":{"type":"string"}},"required":["summary"],"additionalProperties":false}), effort: "low", prompt: prompt(s), history: Vec::new(), session: None };
    let answer = crate::background_call::ask_blocking(&e, &call)?;
    let summary = serde_json::from_str::<Value>(&answer.text).ok().and_then(|v| v["summary"].as_str().map(str::to_owned)).unwrap_or(answer.text);
    anyhow::ensure!(!summary.trim().is_empty(), "the answer has no summary");
    Ok(summary)
}

/// Tests script each model's answer in `test-compaction-answers.json` beside the data (`{"provider/id": {"summary"} |
/// {"error"}}`); a model it does not name is not connected. No model is ever called.
#[cfg(test)]
fn attempt(data: &Path, c: &Candidate, _: &Source) -> Result<String> {
    let answers: Value = std::fs::read(data.join("test-compaction-answers.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    let a = &answers[c.key()];
    if let Some(s) = a["summary"].as_str() { return Ok(s.into()); }
    anyhow::bail!("{}", a["error"].as_str().unwrap_or("not connected"))
}
#[cfg(not(test))]
fn attempt(_: &Path, c: &Candidate, s: &Source) -> Result<String> { summarise(c, s) }

/// The fallback's state in the supervisor: a failure waiting for Pi to be idle, the summary running, its result waiting
/// for Pi to be idle, the compaction Pi runs from it, and whether this episode's notice was shown.
#[derive(Default)]
pub(super) struct CompactionFallback {
    /// A failure waits for `step`, with what failed.
    pub wanted: Option<Failed>,
    pub task: Option<tokio::task::JoinHandle<Outcome>>,
    pub ready: Option<Outcome>,
    pub supplied: Option<Outcome>,
    pub noticed: bool,
}
impl CompactionFallback {
    pub fn busy(&self) -> bool { self.wanted.is_some() || self.task.is_some() || self.ready.is_some() || self.supplied.is_some() }
    /// Pi answered a compaction as failed (`error`): one Heimdall supplied did not go in (the island says so, once), or
    /// one of Pi's own that another model may summarise waits for `step`.
    pub fn failed(&mut self, b: &mut agent::Bridge<RecordedAdapter>, tools: &Tools, settings: &Settings, error: &str) -> Result<()> {
        if let Some(done) = self.supplied.take() {
            eprintln!("Agent: the compaction from another model did not go in ({error}); the history is as it was");
            b.harness.journal.event("agent_compaction", "agent:pi", &[], capture::host_now_ns(), json!({"state":"fallback_failed","error":error,"outcome":kind(&done)}))?;
            self.say(tools, &Outcome::Failed { tried: vec![] });
        } else if !self.busy() && wanted(error) {
            // A new episode: its own one notice.
            self.wanted = Some(Failed::of(settings, error));
            self.noticed = false;
        }
        Ok(())
    }
    /// A new Pi process (review 28 Oct): the old one's extension kept the source and any armed result. A compaction it
    /// was running from Heimdall's result never answers; a failure waiting for its source has none now. Both end
    /// quietly (logged); a summary still waiting is for the same session and its cut is checked when Pi writes it.
    pub fn new_process(&mut self) {
        if let Some(done) = self.supplied.take() { eprintln!("Agent: the compaction from another model ended with its Pi process ({}); the history is as it was", kind(&done)); }
        self.wanted = None;
    }
    /// Pi completed a compaction: one of Heimdall's is logged (a cut also says so on the island); a new episode may say
    /// its own notice.
    pub fn compacted(&mut self, b: &mut agent::Bridge<RecordedAdapter>, tools: &Tools) -> Result<()> {
        let Some(done) = self.supplied.take() else {
            // Pi compacted on its own meanwhile (review 28 Oct): a summary or cut made for the old history is stale.
            if let Some(task) = self.task.take() { task.abort(); }
            if self.ready.take().is_some() || self.wanted.take().is_some() { eprintln!("Agent: Pi compacted on its own; the compaction fallback stands down"); }
            return Ok(());
        };
        match &done {
            Outcome::Summary { model, tried, .. } => eprintln!("Agent: compacted on {model} after the agent's provider refused{}", tail(tried)),
            Outcome::Cut { keep, tokens_before, tried, .. } => eprintln!("Agent: no model could summarise the {tokens_before}-token history; its newest turns up to {keep} tokens stay, the older part was cut (still in the session file){}", tail(tried)),
            _ => {}
        }
        b.harness.journal.event("agent_compaction", "agent:pi", &[], capture::host_now_ns(), json!({"state":kind(&done),"model":match &done { Outcome::Summary { model, .. } => json!(model), _ => Value::Null }}))?;
        if matches!(done, Outcome::Cut { .. }) { self.say(tools, &done); }
        Ok(())
    }
    /// The next step while Pi is idle: start the summary for a failure, or hand a finished one to Pi, or say that
    /// every option failed. `engine`: the local engine's probe (its window when loaded).
    pub async fn step(&mut self, b: &mut agent::Bridge<RecordedAdapter>, settings: &Settings, data: &Path, tools: &Tools, engine: &agent::supervision::EngineProbe) -> Result<()> {
        let Adapter::Pi(pi) = &mut b.harness.inner else { return Ok(()) };
        if pi.active() || pi.input_blocked().is_some() { return Ok(()) }
        if let Some(failed) = self.wanted.take() {
            let window = if fixture_executable().is_some() { std::fs::read_to_string(data.join("test-engine-window")).ok().and_then(|t| t.trim().parse().ok()) }
                else if !failed.excludes(ui::LOCAL_ENGINE, "") && engine_available(&tools.engine_signal, engine).await { engine.context_limit().await.ok() } else { None };
            let api = tools.api.clone();
            let found = candidates(settings, &failed, window, &|p, i| api.api_model(p, i));
            let budget = match settings.selected_api_model() { Some(m) => m.budget(), None => pi.context_usage().await.map(|u| u.1).unwrap_or(0) };
            if let Err(e) = self.begin(pi, data, found, budget).await {
                eprintln!("Agent: the compaction could not move to another model: {e:#}");
                // Nothing kept to summarise (Pi had nothing to compact): no option was tried, nothing to say.
                if e.downcast_ref::<NoSource>().is_some() { return Ok(()); }
                b.harness.journal.event("agent_compaction", "agent:pi", &[], capture::host_now_ns(), json!({"state":"fallback_failed","error":format!("{e:#}")}))?;
                self.say(tools, &Outcome::Failed { tried: vec![] });
            }
            return Ok(());
        }
        let Some(out) = self.ready.take() else { return Ok(()) };
        match &out {
            Outcome::Summary { .. } | Outcome::Cut { .. } => if let Err(e) = self.supply(pi, data, out.clone()).await {
                eprintln!("Agent: the compaction from another model could not be handed to Pi: {e:#}");
                b.harness.journal.event("agent_compaction", "agent:pi", &[], capture::host_now_ns(), json!({"state":"fallback_failed","error":format!("{e:#}")}))?;
                self.say(tools, &Outcome::Failed { tried: vec![] });
            },
            Outcome::Failed { tried } | Outcome::Quiet { tried } => {
                eprintln!("Agent: no model could summarise the history; it is as it was{}", tail(tried));
                b.harness.journal.event("agent_compaction", "agent:pi", &[], capture::host_now_ns(), json!({"state":kind(&out),"tried":tried}))?;
                self.say(tools, &out);
            }
        }
        Ok(())
    }
    /// This episode's one notice (`notice`), never a second.
    fn say(&mut self, tools: &Tools, outcome: &Outcome) {
        if let Some(label) = notice(outcome) { if !std::mem::replace(&mut self.noticed, true) { tools.agent_notice(label); } }
    }
    /// Starts the fallback for a failed compaction: reads Pi's source and summarises off the async runtime.
    pub async fn begin(&mut self, pi: &mut agent::harness::Pi, data: &Path, candidates: Vec<Candidate>, budget: u64) -> Result<()> {
        let path = data.join("agent/compaction/source.json");
        agent::runtime::private_dir(path.parent().context("compaction folder")?)?;
        let _ = std::fs::remove_file(&path);
        pi.compaction_source(&path).await?;
        let read = std::fs::read(&path);
        let _ = std::fs::remove_file(&path);
        let source: Source = serde_json::from_slice(&read.context("Pi wrote no compaction source")?)?;
        if source.none || source.first_kept_entry_id.is_empty() { return Err(NoSource.into()); }
        eprintln!("Agent: compaction moves to another model: {} tokens to summarise, {} in the history; tries {}", source.tokens, source.tokens_before,
            if candidates.is_empty() { "nothing".into() } else { candidates.iter().map(|c| c.name.as_str()).collect::<Vec<_>>().join(", ") });
        let data = data.to_owned();
        self.task = Some(tokio::task::spawn_blocking(move || run(&source, &candidates, budget, &mut |c, s| attempt(&data, c, s))));
        Ok(())
    }
    /// A summary or cut waiting: Pi writes it as its compaction now (`Pi` idle). The file is private and gone once read.
    pub async fn supply(&mut self, pi: &mut agent::harness::Pi, data: &Path, outcome: Outcome) -> Result<()> {
        let body = match &outcome {
            Outcome::Summary { summary, first_kept, previous, model, .. } => json!({"summary":summary,"firstKeptEntryId":first_kept,"previous":previous,"model":model}),
            Outcome::Cut { keep, tokens_before, first_kept, previous, .. } => json!({"mode":"cut","keepTokens":keep,"tokensBefore":tokens_before,"sourceKept":first_kept,"previous":previous,"at":chrono::Local::now().format("%-d %b %H:%M").to_string()}),
            _ => anyhow::bail!("nothing to supply"),
        };
        let path = data.join("agent/compaction/supply.json");
        agent::runtime::atomic_write(&path, &serde_json::to_vec(&body)?)?;
        self.supplied = Some(outcome);
        pi.compact_supplied(&path).await.inspect_err(|_| { self.supplied = None; let _ = std::fs::remove_file(&path); })
    }
}

/// Pi kept no compaction to summarise.
#[derive(Debug)]
struct NoSource;
impl std::fmt::Display for NoSource { fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str("Pi kept no compaction to summarise") } }
impl std::error::Error for NoSource {}

fn kind(o: &Outcome) -> &'static str {
    match o { Outcome::Summary { .. } => "fallback", Outcome::Cut { .. } => "cut", Outcome::Failed { .. } => "fallback_failed", Outcome::Quiet { .. } => "fallback_paused" }
}
fn tail(tried: &[String]) -> String { if tried.is_empty() { String::new() } else { format!(" ({})", tried.join("; ")) } }

/// The island's one line when every option failed, or when the history was cut.
pub(super) fn notice(outcome: &Outcome) -> Option<String> {
    match outcome {
        Outcome::Failed { .. } => Some("Agent history not compacted: no model could summarise it".into()),
        Outcome::Cut { .. } => Some("Agent history too long for every model: its oldest part was cut".into()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::background_model::BackgroundModel;
    fn chatgpt(id: &str, name: &str, context: u64) -> agent::provider::ApiModel {
        agent::provider::ApiModel { provider: agent::provider::CHATGPT.into(), id: id.into(), name: name.into(), api: agent::provider::Api::OpenaiResponses, context, max_output: 128_000,
            images: true, reasoning: true, cost: None, context_budget: None, thinking: None, base_url: None, efforts: vec![] }
    }
    /// 28 Oct 17:27: Opus on the Anthropic key, Luna the background model, Sol and Terra on the same ChatGPT sign-in.
    fn owner() -> Settings {
        let mut s = Settings::default();
        s.background_model = BackgroundModel { provider: agent::provider::CHATGPT.into(), id: "gpt-6-luna".into() };
        s.api_models = vec![agent::provider::ApiModel::anthropic(agent::provider::OPUS).unwrap(), chatgpt("gpt-6.1-sol", "GPT-6.1 Sol", 500_000), chatgpt("gpt-6-luna", "GPT-6 Luna", 272_000)];
        s
    }
    #[test]
    fn the_order_is_the_background_model_then_other_subscriptions_then_the_local_engine_never_the_failed_provider() {
        let none = |_: &str, _: &str| None;
        let credit = |p: &str| Failed { provider: Some(p.into()), id: "x".into(), whole: true };
        let c = candidates(&owner(), &credit(agent::provider::ANTHROPIC), Some(262_144), &none);
        assert_eq!(c.iter().map(|c| (c.name.as_str(), c.window)).collect::<Vec<_>>(), [("GPT-6 Luna", 272_000), ("GPT-6.1 Sol", 500_000), ("the local model", 262_144)]);
        // The agent on ChatGPT: its own sign-in is never asked again; no engine loaded, none offered.
        let c = candidates(&owner(), &credit(agent::provider::CHATGPT), None, &none);
        assert!(c.is_empty(), "{c:?}");
        // The agent on the local engine: the engine is the failed one.
        let mut s = owner(); s.background_model = BackgroundModel { provider: ui::LOCAL_ENGINE.into(), id: String::new() };
        assert_eq!(candidates(&s, &Failed { provider: None, id: String::new(), whole: false }, Some(262_144), &none).iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["GPT-6.1 Sol", "GPT-6 Luna"]);
    }
    /// Review 28 Oct: the agent on Luna (272K) failing with "prompt is too long" still tries Sol (500K) on the same
    /// ChatGPT sign-in; a spent ChatGPT limit rules out every model on it.
    #[test]
    fn a_length_failure_rules_out_only_its_model_a_credit_failure_its_whole_sign_in() {
        let mut s = owner();
        s.agent_model = agent::provider::AgentModel::Api { provider: agent::provider::CHATGPT.into(), id: "gpt-6-luna".into() };
        s.background_model = BackgroundModel { provider: ui::LOCAL_ENGINE.into(), id: String::new() };
        let names = |f: &Failed| candidates(&s, f, Some(262_144), &|_, _| None).iter().map(|c| c.name.clone()).collect::<Vec<_>>();
        let long = Failed::of(&s, "Summarization failed: 400 prompt is too long: 334007 tokens > 272000 maximum");
        assert_eq!((long.provider.as_deref(), long.id.as_str(), long.whole), (Some(agent::provider::CHATGPT), "gpt-6-luna", false));
        assert_eq!(names(&long), ["the local model", "GPT-6.1 Sol"]);
        let spent = Failed::of(&s, "Summarization failed: The ChatGPT user has reached their Subscription Sharing usage limit.");
        assert!(spent.whole);
        assert_eq!(names(&spent), ["the local model"]);
    }
    #[test]
    fn only_a_model_whose_window_holds_the_summary_is_asked_and_the_first_answer_wins() {
        let c = candidates(&owner(), &Failed { provider: Some(agent::provider::ANTHROPIC.into()), id: agent::provider::OPUS.into(), whole: true }, None, &|_, _| None);
        // 17:27: 334,007 tokens in the history, about 314,000 before Pi's kept tail: past Luna's window, inside Sol's.
        let source = Source { first_kept_entry_id: "k".into(), tokens_before: 334_007, tokens: 314_000, ..Default::default() };
        let mut asked = vec![];
        let out = run(&source, &c, 252_571, &mut |c, _| { asked.push(c.name.clone()); Ok("Sol summary".into()) });
        assert_eq!(asked, ["GPT-6.1 Sol"]);
        assert!(matches!(out, Outcome::Summary { ref summary, ref model, ref first_kept, .. } if summary == "Sol summary" && model == "openai/gpt-6.1-sol" && first_kept == "k"), "{out:?}");
        // Inside the budget and every model failing: the history stays, one notice.
        let small = Source { tokens_before: 200_000, tokens: 180_000, ..source.clone() };
        let out = run(&small, &c, 252_571, &mut |_, _| anyhow::bail!("usage limit reached"));
        assert!(matches!(out, Outcome::Failed { ref tried } if tried.len() == 2), "{out:?}");
        assert!(notice(&out).is_some());
        // Past the budget and nothing fits: the last-resort cut at 80% of the budget.
        let huge = Source { tokens: 600_000, ..source };
        assert_eq!(run(&huge, &c, 252_571, &mut |_, _| unreachable!("nothing fits")), Outcome::Cut { keep: 202_056, tokens_before: 334_007, first_kept: "k".into(), previous: None, tried: vec![
            "GPT-6 Luna: its 272000-token window cannot hold 616384 tokens".into(), "GPT-6.1 Sol: its 500000-token window cannot hold 616384 tokens".into()] });
        // Past the budget, a model fits, and it was not asked (Agent off, emergency mode's pause) or failed in passing:
        // nothing is cut (review 28 Oct); the cut only follows lasting refusals.
        let past = Source { tokens: 200_000, ..huge };
        assert!(matches!(run(&past, &c, 252_571, &mut |_, _| Err(crate::background_gate::AgentOff.into())), Outcome::Quiet { .. }));
        assert!(matches!(run(&past, &c, 252_571, &mut |_, _| anyhow::bail!("529 overloaded_error")), Outcome::Failed { .. }));
        assert!(matches!(run(&past, &c, 252_571, &mut |_, _| anyhow::bail!("Your credit balance is too low")), Outcome::Cut { .. }));
    }
    #[test]
    fn a_credit_quota_sign_in_or_length_failure_moves_the_summary_a_passing_one_does_not() {
        assert!(wanted("Summarization failed: 400 {\"type\":\"error\",\"error\":{\"type\":\"invalid_request_error\",\"message\":\"Your credit balance is too low to access the Anthropic API. Please go to Plans & Billing to upgrade or purchase credits.\"}}"));
        assert!(wanted("Summarization failed: 401 invalid x-api-key"));
        assert!(wanted("Summarization failed: The ChatGPT user has reached their Subscription Sharing usage limit."));
        assert!(wanted("400 prompt is too long: 334007 tokens > 272000 maximum"));
        assert!(!wanted("Summarization failed: 529 overloaded_error"));
        assert!(!wanted("Compaction cancelled"));
    }
}
