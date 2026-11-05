//! The one Messages API path every background job takes (item 28): the plan refresher,
//! the voice namer, the day timeline and any job added later, on whatever Background Tasks ▸ Model names. The main
//! agent never comes here (Pi calls the provider itself).
//!
//! - Prompt caching, one-hour TTL. A job's request is built from a `Call`: its system prompt, answer schema and
//!   effort (the stable prefix: tools → system → messages is the render order, and a job's prefix stays
//!   byte-identical between runs while its prompt is unchanged), then the user message as `Prompt` blocks, stable
//!   first. The system block always carries a breakpoint; a job marks the end of each stable stretch of its user
//!   message (`Prompt::stable`), and the volatile rest ("Now:", live state, the instruction) comes last, unmarked.
//!   At most three user breakpoints are kept (the API takes four with the system's), the newest ones. The prompt's
//!   hash (model, effort, schema, system) is logged with every run, and a change of it once: that run rewrites the
//!   cache. Below the model's minimum (512 tokens on the 5.x models) the API silently caches nothing.
//!   Other providers have no explicit markers; a job that reaches one later sends the same order (`Prompt::text`)
//!   so their automatic prefix caching can reuse the stable part.
//! - No output cap of our own: `max_tokens` is the model's maximum (`MODELS`), lowered only where the run's $0.50
//!   limit needs it.
//! - $0.50 a run (`LIMIT_USD`, the user clicked "Max per run"). Before a call the worst case is priced: every input token
//!   in a cached stretch as a one-hour cache write (a cold cache), the rest at the input price, the price tier by
//!   the whole prompt (input estimated at 2.5 bytes a token: more tokens than English, JSON, ids or Polish give),
//!   plus `max_tokens` of output. `max_tokens` is the model's maximum when that fits under $0.50, else as many as
//!   fit (logged: such a run can stop at the limit, and its stop is logged as one); when not even `MIN_OUTPUT`
//!   tokens fit, the run is skipped (`OverLimit`, logged). A run whose real cost still passed $0.50 (an input the
//!   estimate undercounted) is logged as over. On Claude Haiku 5.5 (the default) the limit lowers nothing for the
//!   jobs' prompts: 400 kB with 128K of output prices at about $0.48.
//! - A paid run that gave no usable answer (refused, cut off, stopped at the limit, empty, timed out) ends in
//!   `Stopped` with its usage: the jobs keep its input as seen and call again only on something new, so one bad
//!   input is paid for once.
//! - Each response's usage (input, output, cache read, cache write) is logged with its dollar cost and counted in
//!   Heimdall's spend ledger (`api_providers::Hub::record_background`: the Anthropic credit line, and one line per
//!   run in `agent/background-runs.jsonl`), refused and cut-off answers included: they are billed too.
//! - Non-streaming with TCP keep-alive and a ten-minute deadline (`TIMEOUT`, the SDKs' non-streaming ceiling): a
//!   long Haiku answer fits. A run that times out returns no usage; it is counted at its worst case. A connect that
//!   times out (10 s) sent nothing: it is free and retried like any failed request.
//!
//! - Tool loops on any provider (deep search runs on Background Tasks ▸ Model "with ANY
//!   provider", no provider-specific code in deep search): `ask_tools_blocking` sends each round in its endpoint's
//!   format (`Endpoint::wire`: Anthropic Messages, OpenAI chat completions, OpenAI Responses; `background_wire` holds
//!   every difference), with the same `Tool`s, `submit`, stops and ledger lines. Anthropic's prices and the $0.50
//!   limit apply on Anthropic; any other provider's runs (a subscription, a key metered by its provider, a local
//!   engine) are counted in rounds at $0 with `max_rounds` as their limit. `ask` (the jobs' structured answers)
//!   stays Messages-only, and every job's endpoint (`background_model::endpoint`) stays Anthropic's.
//! - A harness T3 runs (`background_t3`, provider "t3:<instance>": the Claude subscription through T3, no key): `ask`
//!   is one short-lived T3 thread per call, unpriced, one ledger line at $0. Tool loops are not offered there
//!   (`background_t3::NoToolsOnT3`).
//! - Background Tasks ▸ Model's thinking level for the chosen model (`background_model::effort_for`, the user's pick from
//!   that model's own list) replaces each job's default effort on every format that takes one (`with_effort`): Messages'
//!   `output_config.effort`, Responses' and chat completions' reasoning effort (through `wire`'s mapping to what the
//!   model lists), and T3's `options.effort`. The prompt hash includes the effort, so each level keeps its own cache.
//!
//! - A rolling session (the Goals shadow only, docs/background-call.md): `Call::history` carries the earlier turns,
//!   oldest first and append-only, before the new prompt, so the provider's prefix cache serves all but the newest
//!   turn ("It's like a rolling context, just like yours, but a way cheaper model, so we can afford to
//!   shadow you completely because we're hitting cache anyway"; it reverses the 30 Oct 01:08 stateless rule for that
//!   job alone). Messages: breakpoints on the system, the end of the last earlier answer and the new message's last
//!   stable stretch; the history is priced as cached prefix (a cold one-hour write at worst). Responses: the turns as
//!   input items, `Call::session` as the cache key. T3: one thread per session (`background_t3`). Every other job
//!   leaves both empty and sends the same bytes as before.
//!
//! Prices and limits: Anthropic's pricing page (platform.claude.com/docs/en/about-claude/pricing) and models
//! overview, read 24 Oct 2026. A model not in the table is priced as the dearest one (Claude Fable 5.1) with a 128K
//! maximum, so the limit still holds.

use crate::plan_refresher::Endpoint;
#[path = "background_wire.rs"]
pub mod wire;
pub use wire::Wire;
#[path = "background_t3.rs"]
pub mod t3;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The most one background-job run may cost, in US dollars.
pub const LIMIT_USD: f64 = 0.50;
/// A run that cannot afford this much output is skipped rather than started.
pub const MIN_OUTPUT: u64 = 4_096;
/// Whole request deadline.
pub const TIMEOUT: Duration = Duration::from_secs(600);
/// The one TTL every breakpoint uses (one-hour entries must come before five-minute ones; there are none).
const HOUR: &str = "1h";
const USER_BREAKPOINTS: usize = 3;

/// Per million tokens, US dollars.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Price {
    pub input: f64,
    pub write_5m: f64,
    pub write_1h: f64,
    pub read: f64,
    pub output: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Model {
    pub id: &'static str,
    /// Maximum output tokens (synchronous Messages API).
    pub max_output: u64,
    pub price: Price,
    /// Claude Haiku 5.5: prompts over this many tokens (input + cache read + cache write) pay these prices.
    pub above: Option<(u64, Price)>,
}

const fn p(input: f64, write_5m: f64, write_1h: f64, read: f64, output: f64) -> Price { Price { input, write_5m, write_1h, read, output } }

/// Anthropic's models (pricing page and models overview, 24 Oct 2026; every one 128K output but Haiku 4.5, 64K).
pub const MODELS: [Model; 12] = [
    Model { id: "claude-fable-5-1", max_output: 128_000, price: p(10.0, 12.5, 20.0, 0.25, 50.0), above: None },
    Model { id: "claude-fable-5", max_output: 128_000, price: p(10.0, 12.5, 20.0, 1.0, 50.0), above: None },
    Model { id: "claude-opus-5-5", max_output: 128_000, price: p(4.0, 5.0, 8.0, 0.20, 20.0), above: None },
    Model { id: "claude-opus-5", max_output: 128_000, price: p(5.0, 6.25, 10.0, 0.50, 25.0), above: None },
    Model { id: "claude-opus-4-8", max_output: 128_000, price: p(5.0, 6.25, 10.0, 0.50, 25.0), above: None },
    Model { id: "claude-opus-4-7", max_output: 128_000, price: p(5.0, 6.25, 10.0, 0.50, 25.0), above: None },
    Model { id: "claude-opus-4-6", max_output: 128_000, price: p(5.0, 6.25, 10.0, 0.50, 25.0), above: None },
    Model { id: "claude-sonnet-5-5", max_output: 128_000, price: p(2.0, 2.5, 4.0, 0.10, 10.0), above: None },
    Model { id: "claude-sonnet-5", max_output: 128_000, price: p(2.0, 2.5, 4.0, 0.20, 10.0), above: None },
    Model { id: "claude-sonnet-4-6", max_output: 128_000, price: p(3.0, 3.75, 6.0, 0.30, 15.0), above: None },
    Model { id: "claude-haiku-5-5", max_output: 128_000, price: p(0.10, 0.125, 0.20, 0.01, 0.50), above: Some((100_000, p(0.50, 0.625, 1.0, 0.05, 2.50))) },
    Model { id: "claude-haiku-4-5", max_output: 64_000, price: p(1.0, 1.25, 2.0, 0.10, 5.0), above: None },
];

/// The model's limits and prices; an unknown id is priced as the dearest model (`MODELS[0]`), logged once.
pub fn model(id: &str) -> Model {
    // A dated snapshot id ("claude-haiku-4-5-20251001") is its model's.
    let base = id.rsplit_once('-').filter(|(_, d)| d.len() == 8 && d.bytes().all(|b| b.is_ascii_digit())).map_or(id, |(b, _)| b);
    if let Some(m) = MODELS.iter().find(|m| m.id == base) { return *m }
    static LOGGED: Mutex<Option<String>> = Mutex::new(None);
    let mut l = LOGGED.lock().unwrap();
    if l.as_deref() != Some(id) {
        eprintln!("Background tasks: no price on record for {id}; its runs are priced as Claude Fable 5.1 for the $0.50 limit.");
        *l = Some(id.to_owned());
    }
    MODELS[0]
}

impl Model {
    /// The prices a prompt of `prompt_tokens` pays.
    pub fn price_for(&self, prompt_tokens: u64) -> Price {
        match self.above { Some((above, price)) if prompt_tokens > above => price, _ => self.price }
    }
}

// ---------------------------------------------------------------------------------------------------------
// The request.

/// One block of the user message; `cache` puts a one-hour breakpoint after it.
#[derive(Clone, Debug, PartialEq)]
pub struct Block {
    pub text: String,
    pub cache: bool,
}

/// The user message, stable blocks first.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Prompt {
    pub blocks: Vec<Block>,
}

impl Prompt {
    /// A stretch that stays the same between runs; the cache keeps everything up to its end.
    pub fn stable(&mut self, text: impl Into<String>) -> &mut Self { self.add(text.into(), true) }
    /// The volatile rest (live state, the clock, the instruction).
    pub fn push(&mut self, text: impl Into<String>) -> &mut Self { self.add(text.into(), false) }
    fn add(&mut self, text: String, cache: bool) -> &mut Self {
        if !text.is_empty() { self.blocks.push(Block { text, cache }) }
        self
    }
    /// The whole message as one text (logs, tests, a provider without content blocks).
    pub fn text(&self) -> String { self.blocks.iter().map(|b| b.text.as_str()).collect() }
    pub fn len(&self) -> usize { self.blocks.iter().map(|b| b.text.len()).sum() }
    /// Bytes up to the end of the last kept breakpoint.
    fn cached_len(&self) -> usize {
        let last = self.marks().last().copied();
        last.map_or(0, |i| self.blocks[..=i].iter().map(|b| b.text.len()).sum())
    }
    /// The blocks whose breakpoint is kept: the newest `USER_BREAKPOINTS`.
    fn marks(&self) -> Vec<usize> { self.newest_marks(USER_BREAKPOINTS) }
    fn newest_marks(&self, keep: usize) -> Vec<usize> {
        let all: Vec<usize> = self.blocks.iter().enumerate().filter(|(_, b)| b.cache).map(|(i, _)| i).collect();
        all[all.len().saturating_sub(keep)..].to_vec()
    }
}

impl From<&str> for Prompt {
    fn from(t: &str) -> Prompt { let mut p = Prompt::default(); p.push(t); p }
}
impl From<String> for Prompt {
    fn from(t: String) -> Prompt { let mut p = Prompt::default(); p.push(t); p }
}

/// One earlier exchange of a rolling session: the user message as it was sent, and the answer's text as it came back.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Turn {
    pub user: Prompt,
    pub assistant: String,
}

/// One background-job run.
#[derive(Clone, Debug)]
pub struct Call<'a> {
    /// The log and ledger label ("plan refresher").
    pub job: &'static str,
    pub model: String,
    pub system: &'a str,
    pub schema: Value,
    pub effort: &'static str,
    pub prompt: Prompt,
    /// A rolling session's earlier turns, oldest first, sent before `prompt` (the Goals shadow; empty for every other
    /// job: one stateless message). Append-only between calls, so the prefix stays byte-identical and is read from cache.
    pub history: Vec<Turn>,
    /// The rolling context's current segment ("goals-<segment id>"): Responses' cache key and session headers, T3's one
    /// thread for the segment. `None` for every other job.
    pub session: Option<String>,
}

impl Call<'_> {
    /// The earlier turns' bytes (their prompts and answers).
    fn history_len(&self) -> usize { self.history.iter().map(|t| t.user.len() + t.assistant.len()).sum() }
}

fn cache_control() -> Value { json!({"type": "ephemeral", "ttl": HOUR}) }

/// The stable prefix's hash (model, effort, schema, system): FNV-1a 64, the same in every build.
pub fn prompt_hash(c: &Call) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for part in [c.model.as_str(), "\0", c.effort, "\0", &c.schema.to_string(), "\0", c.system] {
        for b in part.bytes() { h = (h ^ b as u64).wrapping_mul(0x0000_0100_0000_01b3) }
    }
    h
}

/// Input tokens, over-estimated: 2.5 bytes a token.
pub fn estimate_tokens(bytes: usize) -> u64 { (bytes as u64 * 2).div_ceil(5) }

/// What a run may spend, decided before the call.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Budget {
    pub max_tokens: u64,
    /// The worst case at that `max_tokens`, US dollars.
    pub worst_usd: f64,
    pub prompt_tokens: u64,
    /// `max_tokens` is below the model's maximum because of the limit.
    pub limited: bool,
}

/// The run would cost more than `LIMIT_USD` before any useful output: skipped, never sent.
#[derive(Debug)]
pub struct OverLimit {
    pub job: &'static str,
    pub input_usd: f64,
}
impl std::fmt::Display for OverLimit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} skipped: its prompt alone could cost ${:.2}, over the ${LIMIT_USD:.2} limit a run", self.job, self.input_usd)
    }
}
impl std::error::Error for OverLimit {}

/// A paid run without a usable answer: refused, cut off, stopped at the limit, empty, or timed out. Its usage was
/// counted; the job should not resend the same input.
#[derive(Debug)]
pub struct Stopped {
    pub usage: Usage,
    pub usd: f64,
    pub reason: String,
}
impl std::fmt::Display for Stopped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(&self.reason) }
}
impl std::error::Error for Stopped {}

/// Whether `e` means the same input should wait for something new: over the limit, or paid for without an answer.
pub fn settled(e: &anyhow::Error) -> bool { e.downcast_ref::<OverLimit>().is_some() || e.downcast_ref::<Stopped>().is_some() }

/// The worst case before the call (module doc).
pub fn budget(c: &Call) -> std::result::Result<Budget, OverLimit> {
    let m = model(&c.model);
    // The schema reaches the model too (compiled into the grammar); counted as input.
    // A rolling session's earlier turns are the cached prefix too (a cold cache writes them once).
    let fixed = c.system.len() + c.schema.to_string().len() + c.history_len();
    let cached = estimate_tokens(fixed + c.prompt.cached_len());
    let rest = estimate_tokens(c.prompt.len() - c.prompt.cached_len());
    let prompt_tokens = cached + rest;
    let price = m.price_for(prompt_tokens);
    let input_usd = (cached as f64 * price.write_1h + rest as f64 * price.input) / 1e6;
    let affordable = ((LIMIT_USD - input_usd) / price.output * 1e6).floor().max(0.0) as u64;
    if affordable < MIN_OUTPUT { return Err(OverLimit { job: c.job, input_usd }) }
    let max_tokens = m.max_output.min(affordable);
    Ok(Budget { max_tokens, worst_usd: input_usd + max_tokens as f64 * price.output / 1e6, prompt_tokens, limited: max_tokens < m.max_output })
}

/// The Messages API body: adaptive thinking at the job's effort, structured output, the system block and the
/// user's stable blocks marked for the one-hour cache, no sampling parameters, no prefill.
pub fn body(c: &Call, max_tokens: u64) -> Value {
    json!({
        "model": c.model, // Background Tasks ▸ Model
        "max_tokens": max_tokens,
        "thinking": {"type": "adaptive"},
        "output_config": {"effort": c.effort, "format": {"type": "json_schema", "schema": c.schema}},
        "system": [{"type": "text", "text": c.system, "cache_control": cache_control()}],
        "messages": messages(c)
    })
}

/// The Messages conversation. Without history: the one user message, its newest stable stretches marked
/// (`USER_BREAKPOINTS`). A rolling session: each earlier turn (its user blocks unmarked, then the answer's text), a
/// breakpoint at the end of the last earlier answer (the previous run's prefix, read from cache), then the new user
/// message with one breakpoint at its last stable stretch (the next run reads it): with the system's, at most three.
pub(crate) fn messages(c: &Call) -> Vec<Value> {
    let Some(last) = c.history.len().checked_sub(1) else { return vec![json!({"role": "user", "content": content_blocks(&c.prompt)})] };
    let mut out = Vec::with_capacity(2 * c.history.len() + 1);
    for (i, t) in c.history.iter().enumerate() {
        out.push(json!({"role": "user", "content": marked_blocks(&t.user, 0)}));
        let mut a = json!({"type": "text", "text": t.assistant});
        if i == last { a["cache_control"] = cache_control() }
        out.push(json!({"role": "assistant", "content": [a]}));
    }
    out.push(json!({"role": "user", "content": marked_blocks(&c.prompt, 1)}));
    out
}

/// A run ready to send.
#[derive(Clone, Debug)]
pub struct Prepared {
    pub body: Value,
    pub budget: Budget,
    pub hash: u64,
}

/// The budget, the body and the hash; logs a lowered `max_tokens` and a changed prompt.
pub fn prepare(c: &Call) -> Result<Prepared> {
    let budget = match budget(c) {
        Ok(b) => b,
        Err(e) => { eprintln!("Background tasks · {e}"); return Err(e.into()) }
    };
    if budget.limited {
        eprintln!("Background tasks · {}: max_tokens {} (not {}) keeps the run under ${LIMIT_USD:.2} on {}", c.job, budget.max_tokens, model(&c.model).max_output, c.model);
    }
    let hash = prompt_hash(c);
    static LAST: Mutex<Vec<(&'static str, u64)>> = Mutex::new(Vec::new());
    let mut last = LAST.lock().unwrap();
    match last.iter_mut().find(|(j, _)| *j == c.job) {
        Some((_, h)) if *h != hash => {
            eprintln!("Background tasks · {}: prompt changed ({:016x} → {hash:016x}); this run rewrites the cache", c.job, *h);
            *h = hash;
        }
        Some(_) => {}
        None => last.push((c.job, hash)),
    }
    Ok(Prepared { body: body(c, budget.max_tokens), budget, hash })
}

// ---------------------------------------------------------------------------------------------------------
// The response.

/// A response's token counts. `input` is the uncached remainder (after the last breakpoint).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write_5m: u64,
    pub cache_write_1h: u64,
}

impl Usage {
    /// The Messages API `usage` object. Without the per-TTL split every write counts as a one-hour write (the only
    /// TTL these requests ask for).
    pub fn from_api(u: &Value) -> Usage {
        let n = |v: &Value| v.as_u64().unwrap_or(0);
        let write = n(&u["cache_creation_input_tokens"]);
        let split = &u["cache_creation"];
        let (w5, w1) = if split.is_object() { (n(&split["ephemeral_5m_input_tokens"]), n(&split["ephemeral_1h_input_tokens"])) } else { (0, write) };
        Usage { input: n(&u["input_tokens"]), output: n(&u["output_tokens"]), cache_read: n(&u["cache_read_input_tokens"]), cache_write_5m: w5, cache_write_1h: w1 }
    }
    pub fn cache_write(&self) -> u64 { self.cache_write_5m + self.cache_write_1h }
    /// The whole prompt: what the price tier follows.
    pub fn prompt(&self) -> u64 { self.input + self.cache_read + self.cache_write() }
    /// US dollars at `m`'s list price.
    pub fn dollars(&self, m: &Model) -> f64 {
        let p = m.price_for(self.prompt());
        (self.input as f64 * p.input + self.output as f64 * p.output + self.cache_read as f64 * p.read
            + self.cache_write_5m as f64 * p.write_5m + self.cache_write_1h as f64 * p.write_1h) / 1e6
    }
}

/// One run as the ledger keeps it (no request content).
#[derive(Clone, Debug, PartialEq)]
pub struct Run {
    pub job: &'static str,
    pub model: String,
    pub hash: u64,
    pub usage: Usage,
    pub usd: f64,
    /// "answered", "cut off", "limit", "refused", "failed".
    pub outcome: &'static str,
    pub at_ms: u64,
}

impl Run {
    pub fn line(&self) -> Value {
        json!({"at_ms": self.at_ms, "job": self.job, "model": self.model, "prompt": format!("{:016x}", self.hash),
               "input": self.usage.input, "output": self.usage.output, "cache_read": self.usage.cache_read,
               "cache_write": self.usage.cache_write(), "usd": self.usd, "outcome": self.outcome})
    }
}

/// Where runs are counted: the app registers its spend ledger at launch (`set_ledger`); lab runs and tests log only.
pub type Ledger = Arc<dyn Fn(&Run) + Send + Sync>;
#[cfg(not(test))]
static LEDGER: std::sync::RwLock<Option<Ledger>> = std::sync::RwLock::new(None);
#[cfg(not(test))]
pub fn set_ledger(l: Ledger) { *LEDGER.write().unwrap() = Some(l) }
#[cfg(not(test))]
fn ledger() -> Option<Ledger> { LEDGER.read().unwrap().clone() }
// Tests: per thread, so one test's ledger never sees another's runs.
#[cfg(test)]
thread_local! { static LEDGER: std::cell::RefCell<Option<Ledger>> = const { std::cell::RefCell::new(None) }; }
#[cfg(test)]
pub fn set_ledger(l: Ledger) { LEDGER.with(|c| *c.borrow_mut() = Some(l)) }
#[cfg(test)]
fn ledger() -> Option<Ledger> { LEDGER.with(|c| c.borrow().clone()) }

pub(crate) fn record(run: &Run) {
    let u = &run.usage;
    eprintln!("Background tasks · {}: {} {}, prompt {:016x}: {} in ({} cache read, {} cache write, {} uncached), {} out, ${:.4}",
        run.job, run.model, run.outcome, run.hash, u.prompt(), u.cache_read, u.cache_write(), u.input, u.output, run.usd);
    if let Some(l) = ledger() { l(run) }
}

/// A job's answer.
#[derive(Clone, Debug, PartialEq)]
pub struct Answer {
    /// The answer's JSON text.
    pub text: String,
    pub usage: Usage,
    pub usd: f64,
}

/// A Messages API response to `prepared`: its usage counted (whatever the outcome), then the answer's text.
pub fn finish(c: &Call, prepared: &Prepared, v: &Value) -> Result<Answer> {
    let m = model(&c.model);
    let usage = Usage::from_api(&v["usage"]);
    let usd = usage.dollars(&m);
    let text: String = v["content"].as_array().into_iter().flatten().filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str()).collect();
    let outcome = match v["stop_reason"].as_str() {
        Some("refusal") => "refused",
        Some("max_tokens") if prepared.budget.limited => "limit",
        Some("max_tokens") => "cut off",
        _ if text.trim().is_empty() => "failed",
        _ => "answered",
    };
    if usage != Usage::default() {
        record(&Run { job: c.job, model: c.model.clone(), hash: prepared.hash, usage, usd, outcome, at_ms: agent::now_ms() });
    }
    if usd > LIMIT_USD { eprintln!("Background tasks · {}: the run cost ${usd:.2}, over the ${LIMIT_USD:.2} limit (its input was undercounted)", c.job); }
    let reason = match outcome {
        "refused" => "the model declined".to_owned(),
        "limit" => format!("stopped at the ${LIMIT_USD:.2} limit ({} output tokens on {})", prepared.budget.max_tokens, c.model),
        "cut off" => format!("the answer was cut off at max_tokens ({})", prepared.budget.max_tokens),
        "failed" => "the answer has no text".to_owned(),
        _ => return Ok(Answer { text, usage, usd }),
    };
    Err(Stopped { usage, usd, reason }.into())
}

/// How long a connect may take (the test build's is short, `a_connect_that_times_out_is_free_and_retried`).
const CONNECT_TIMEOUT: Duration = if cfg!(test) { Duration::from_secs(1) } else { Duration::from_secs(10) };

/// The provider's name for errors and logs.
fn provider_name(e: &Endpoint) -> &'static str {
    if e.provider == ui::LOCAL_ENGINE { return "the local engine" }
    if ui::t3_instance(e.provider).is_some() { return "T3" }
    agent::provider::provider(e.provider).map_or("the model server", |p| p.name)
}

/// One POST of `body` in `e.wire`'s format: the run's usage is counted by the caller (`finish`, `run_tools_on`). A
/// timeout after the request was sent records the run at its worst case and is not resent. Errors never carry the key
/// (a server that echoes it back has it cut out).
async fn post(e: &Endpoint, key: &str, c: &Call<'_>, hash: u64, body: &Value, worst_usd: f64) -> Result<Value> {
    // Agent off: nothing goes out, also between the rounds of a tool loop (`background_gate`). Nor while the provider's
    // plan limit or credit is spent (the quota episode, audit 27 Oct F8).
    // Setup's agent check asks while Agent is still off (Start turns it on); it is the user's own choice, made in Setup.
    if c.job != crate::pipeline::setup_ready::AGENT_CHECK_JOB { crate::background_gate::check()?; }
    crate::background_gate::quota_check(e.provider, agent::now_ms())?;
    let client = reqwest::Client::builder()
        .user_agent(concat!("heimdall/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(CONNECT_TIMEOUT)
        .tcp_keepalive(Duration::from_secs(30))
        .timeout(e.timeout)
        .build()?;
    let base = e.base.trim_end_matches('/');
    let mut req = match e.wire {
        Wire::Anthropic => {
            let r = client.post(format!("{base}/v1/messages")).header("x-api-key", key).header("anthropic-version", agent::provider::ANTHROPIC_VERSION);
            match &e.workspace { Some((h, id)) => r.header(h.as_str(), id.as_str()), None => r }
        }
        Wire::ChatCompletions => client.post(format!("{base}/chat/completions")).bearer_auth(key),
        // The cache key also as the session affinity headers Pi 1.4.0 sends on the same ChatGPT route
        // (`openai-responses.js`, `createClient`: `session_id` and `x-client-request-id`), where the agent reads ~98%
        // of its prompt from cache. The jobs sent the key in the body alone and read 0 or 8,448 tokens (27 Oct, the user:
        // "we have to cache Luna prompts"; their prompts' prefixes were byte-identical pass to pass).
        Wire::Responses => {
            let r = client.post(format!("{base}/responses")).bearer_auth(key).header("accept", "text/event-stream");
            match body["prompt_cache_key"].as_str() { Some(k) => r.header("session_id", k).header("x-client-request-id", k), None => r }
        }
    };
    req = req.json(body);
    let resp = match req.send().await {
        Ok(r) => r,
        // No usage comes back, but the run was probably billed: counted at its worst case, and not resent. A connect
        // that timed out never sent the request (reqwest's `is_timeout` covers it too): free, and retried below.
        Err(err) if err.is_timeout() && !err.is_connect() => {
            record(&Run { job: c.job, model: c.model.clone(), hash, usage: Usage::default(), usd: worst_usd, outcome: "timed out", at_ms: agent::now_ms() });
            return Err(Stopped { usage: Usage::default(), usd: worst_usd, reason: format!("request failed: timed out after {} s", e.timeout.as_secs()) }.into());
        }
        // Nothing listens (the local engine off, a server down): this backend is skipped by the next calls until a
        // probe is due (`background_gate::backend_down`, `background_fallback`).
        Err(err) => {
            if err.is_connect() { crate::background_gate::backend_down(e.provider, "no connection", agent::now_ms()); }
            bail!("request failed: {}", err.without_url())
        }
    };
    let status = resp.status();
    let retry_after = resp.headers().get("retry-after").and_then(|v| v.to_str().ok()).and_then(|v| v.trim().parse::<u64>().ok());
    let text = resp.text().await.map_err(|err| anyhow::anyhow!("response unreadable: {}", err.without_url()))?;
    let clean = |m: String| if key.len() >= 8 { m.replace(key, "[key]") } else { m };
    if !status.is_success() {
        // The backend's own fault (a spent plan or credit, a 429, a 5xx, the sign-in lost) moves the next calls on (the user,
        // 30 Oct 07:37); a job's own failure (a 400, a prompt too long) marks nothing (`background_gate::fault`).
        let now = agent::now_ms();
        let fault = crate::background_gate::fault(Some(status.as_u16()), &text);
        crate::background_gate::mark_fault(e.provider, fault, retry_after.map(|s| now + s * 1000), &format!("HTTP {}", status.as_u16()), now);
        let msg = serde_json::from_str::<Value>(&text).map(|v| wire::message(&v)).unwrap_or_default();
        let msg = if e.provider == agent::provider::CHATGPT { agent::chatgpt::explain(&text).unwrap_or(msg) } else { msg };
        bail!("{} answered {status}: {}", provider_name(e), clean(msg));
    }
    let read = match e.wire {
        Wire::Responses => wire::responses_final(&text).map_err(|err| anyhow::anyhow!("{}", clean(err.to_string()))),
        _ => serde_json::from_str(&text).context("response is not JSON"),
    };
    // The ChatGPT route states a spent limit inside a 200 stream.
    match &read {
        Err(err) if crate::background_gate::says_spent(&err.to_string()) => crate::background_gate::quota_spent(e.provider, None, agent::now_ms()),
        Ok(_) => crate::background_gate::quota_back(e.provider),
        Err(_) => {}
    }
    read
}

/// One run on `e` (`background_model::endpoint`: the background model and, only while it is on Anthropic, the
/// stored key). Errors never carry the key.
pub async fn ask(e: &Endpoint, c: &Call<'_>) -> Result<Answer> {
    let key = e.key.clone().with_context(|| format!("{} is not connected, or the background model is not on it", provider_name(e)))?;
    let system = agent::identity::for_agent(c.system, &agent::identity::current());
    let c = &with_effort(e, &edition_worded(c, &system));
    if ui::t3_instance(e.provider).is_some() { return t3::ask(e, c).await }
    let l = Loop::for_endpoint(e);
    if l.limit.is_none() { return ask_unpriced(e, &l, &key, c).await }
    let prepared = prepare(c)?;
    let v = post(e, &key, c, prepared.hash, &prepared.body, prepared.budget.worst_usd).await?;
    finish(c, &prepared, &v)
}

/// `ask` on a provider without Anthropic's prices (the jobs run on whatever the background model
/// is, the ChatGPT sign-in first; never a fallback to a paid provider): one request in `e.wire`'s format with the job's
/// schema as its JSON output, usage logged at $0, the same `Stopped` outcomes.
async fn ask_unpriced(e: &Endpoint, l: &Loop, key: &str, c: &Call<'_>) -> Result<Answer> {
    let hash = prompt_hash(c);
    let max_tokens = unpriced_max_tokens(l, &c.model);
    let v = post(e, key, c, hash, &wire::answer_body(l, c, max_tokens), 0.0).await?;
    let (text, stop, usage) = wire::read_answer(l.wire, &v);
    let outcome = match stop {
        wire::Stop::Refusal => "refused",
        wire::Stop::MaxTokens => "cut off",
        _ if text.is_empty() => "failed",
        _ => "answered",
    };
    if usage != Usage::default() {
        record(&Run { job: c.job, model: c.model.clone(), hash, usage, usd: 0.0, outcome, at_ms: agent::now_ms() });
    }
    let reason = match outcome {
        "refused" => "the model declined".to_owned(),
        "cut off" => "the answer was cut off at the output limit".to_owned(),
        "failed" => "the answer has no text".to_owned(),
        _ => return Ok(Answer { text, usage, usd: 0.0 }),
    };
    Err(Stopped { usage, usd: 0.0, reason }.into())
}

/// `ask` from a thread outside the async runtime (the voice namer's and the day timeline's passes).
pub fn ask_blocking(e: &Endpoint, c: &Call<'_>) -> Result<Answer> {
    tokio::runtime::Builder::new_current_thread().enable_all().build()?.block_on(ask(e, c))
}

// ---------------------------------------------------------------------------------------------------------
// Tool use: a job whose model reads local data before it answers (the suggester: file search, the timeline).

/// One tool the model may call: its Messages API entry. `schema` is the input's JSON schema.
#[derive(Clone, Debug)]
pub struct Tool {
    pub name: &'static str,
    pub description: &'static str,
    pub schema: Value,
}

/// The user message's content blocks, stable stretches marked (the same blocks `body` marks).
fn content_blocks(p: &Prompt) -> Vec<Value> { marked_blocks(p, USER_BREAKPOINTS) }

/// `p`'s content blocks with the newest `keep` stable stretches marked.
fn marked_blocks(p: &Prompt, keep: usize) -> Vec<Value> {
    let marks = p.newest_marks(keep);
    p.blocks.iter().enumerate().map(|(i, b)| {
        let mut v = json!({"type": "text", "text": b.text});
        if marks.contains(&i) { v["cache_control"] = cache_control() }
        v
    }).collect()
}

/// One tool round's body: the system prompt (marked, so the tools and the system are one cached prefix), the tools,
/// and the conversation so far. No structured-output format: the answer is the `submit` tool's input.
pub fn tool_body(c: &Call, tools: &[Tool], messages: &[Value], max_tokens: u64) -> Value {
    json!({
        "model": c.model,
        "max_tokens": max_tokens,
        "thinking": {"type": "adaptive"},
        "output_config": {"effort": c.effort},
        "tools": wire::tools_json(Wire::Anthropic, tools),
        "system": [{"type": "text", "text": c.system, "cache_control": cache_control()}],
        "messages": messages,
    })
}

/// The worst case of one round, priced as the dearest input (a one-hour cache write on the whole input) and the
/// output that fits in what is left of `limit` after `spent`. Skipped (`OverLimit`) when not even `MIN_OUTPUT` fits.
fn round_budget(job: &'static str, m: &Model, input_bytes: usize, spent: f64, limit: f64) -> std::result::Result<Budget, OverLimit> {
    let tokens = estimate_tokens(input_bytes);
    let price = m.price_for(tokens);
    let input_usd = tokens as f64 * price.write_1h / 1e6;
    let affordable = ((limit - spent - input_usd) / price.output * 1e6).floor().max(0.0) as u64;
    if affordable < MIN_OUTPUT { return Err(OverLimit { job, input_usd: input_usd + spent }) }
    let max_tokens = m.max_output.min(affordable);
    Ok(Budget { max_tokens, worst_usd: input_usd + max_tokens as f64 * price.output / 1e6, prompt_tokens: tokens, limited: max_tokens < m.max_output })
}

/// The tool loop without the network, so tests can script it. `send` posts one body (with its worst case in USD) and
/// returns the response; `run` executes one local tool and returns its text. The loop ends when the model calls
/// `submit` (the answer is that call's input). It stops with `Stopped` on a refusal, a cut-off, a turn that ends
/// without `submit`, or no answer within `max_rounds`; `OverLimit` when the next round no longer fits in `limit`.
/// Every round is counted in the ledger, and the run's spend stays under `limit`. Anthropic's Messages format at
/// Anthropic's prices: `run_tools_on` is the same loop on any format (the network path goes there directly).
#[cfg_attr(not(test), allow(dead_code))]
pub fn run_tools(
    c: &Call,
    tools: &[Tool],
    submit: &str,
    max_rounds: usize,
    limit: f64,
    run: &mut dyn FnMut(&str, &Value) -> String,
    send: &mut dyn FnMut(&Value, f64) -> Result<Value>,
) -> Result<Answer> {
    run_tools_on(&Loop { wire: Wire::Anthropic, limit: Some(limit), max_output: None, reasoning: true, efforts: 0 }, c, tools, submit, max_rounds, run, send)
}

/// How a tool loop is sent and paid for (`Loop::for_endpoint`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Loop {
    pub wire: Wire,
    /// The run's dollar limit, where Anthropic's prices apply; `None` (subscription, metered elsewhere, local): the run
    /// is counted in rounds only, at $0, and `max_rounds` is its limit.
    pub limit: Option<f64>,
    /// The Messages format's `max_tokens` off Anthropic's price table (unknown: 32K).
    pub max_output: Option<u64>,
    /// Chat completions carry the job's effort (`Endpoint::reasoning`).
    pub reasoning: bool,
    /// The efforts the model lists (`wire::effort_mask` of `Endpoint::efforts`); 0 when unknown.
    pub efforts: u16,
}

impl Loop {
    pub fn for_endpoint(e: &Endpoint) -> Loop {
        let priced = e.wire == Wire::Anthropic && e.provider == agent::provider::ANTHROPIC;
        Loop { wire: e.wire, limit: priced.then_some(LIMIT_USD), max_output: e.max_output, reasoning: e.reasoning, efforts: wire::effort_mask(&e.efforts) }
    }
}

/// An unpriced Messages request's `max_tokens`: the catalog's, else the price table's for a Claude id, else 32K.
fn unpriced_max_tokens(l: &Loop, id: &str) -> u64 {
    l.max_output.filter(|n| *n > 0).or_else(|| MODELS.iter().find(|m| m.id == id).map(|m| m.max_output)).unwrap_or(32_000)
}

/// `run_tools` on any request format (`background_wire`): the same `Tool`s, the same `submit`, the same stops, the
/// same ledger lines; dollars and the limit only where `l.limit` is set.
pub fn run_tools_on(
    l: &Loop,
    c: &Call,
    tools: &[Tool],
    submit: &str,
    max_rounds: usize,
    run: &mut dyn FnMut(&str, &Value) -> String,
    send: &mut dyn FnMut(&Value, f64) -> Result<Value>,
) -> Result<Answer> {
    let priced = l.limit.map(|limit| (limit, model(&c.model)));
    let hash = prompt_hash(c);
    let mut history = wire::first_turn(l.wire, c);
    let (mut usage, mut spent) = (Usage::default(), 0.0);
    let tools_bytes = wire::tools_json(l.wire, tools).to_string().len();
    for round_no in 0..max_rounds {
        let budget = match priced {
            Some((limit, m)) => {
                let input_bytes = c.system.len() + tools_bytes + history.iter().map(|m| m.to_string().len()).sum::<usize>();
                round_budget(c.job, &m, input_bytes, spent, limit)?
            }
            None => Budget { max_tokens: unpriced_max_tokens(l, &c.model), worst_usd: 0.0, prompt_tokens: 0, limited: false },
        };
        let v = send(&wire::body(l, c, tools, &history, budget.max_tokens), budget.worst_usd)?;
        let r = wire::read(l.wire, &v, round_no);
        let usd = priced.map_or(0.0, |(_, m)| r.usage.dollars(&m));
        usage += r.usage;
        spent += usd;
        let submitted = r.calls.iter().find(|t| t.name == submit && t.bad.is_none());
        let outcome = match r.stop {
            wire::Stop::ToolUse if submitted.is_some() => "answered",
            wire::Stop::ToolUse => "tool round",
            wire::Stop::Refusal => "refused",
            wire::Stop::MaxTokens if budget.limited => "limit",
            wire::Stop::MaxTokens => "cut off",
            wire::Stop::End => "failed",
        };
        if r.usage != Usage::default() {
            record(&Run { job: c.job, model: c.model.clone(), hash, usage: r.usage, usd, outcome, at_ms: agent::now_ms() });
        }
        if let Some((limit, _)) = priced {
            if usd > limit { eprintln!("Background tasks · {}: a round cost ${usd:.2}, over the ${limit:.2} limit (its input was undercounted)", c.job); }
        }
        if let Some(s) = submitted {
            return Ok(Answer { text: s.input.to_string(), usage, usd: spent });
        }
        if outcome != "tool round" {
            let reason = match outcome {
                "refused" => "the model declined".to_owned(),
                "limit" => format!("stopped at the ${:.2} limit ({} output tokens on {})", l.limit.unwrap_or(LIMIT_USD), budget.max_tokens, c.model),
                "cut off" if priced.is_some() || l.wire == Wire::Anthropic => format!("the answer was cut off at max_tokens ({})", budget.max_tokens),
                "cut off" => "the answer was cut off at the server's output limit".to_owned(),
                _ => "the model ended without submitting an answer".to_owned(),
            };
            return Err(Stopped { usage, usd: spent, reason }.into());
        }
        // A call whose arguments were not JSON (a submit included) is answered with that, and the model tries again.
        let results: Vec<String> = r.calls.iter().map(|t| match &t.bad {
            Some(why) => format!("Error: {why}. Call {} again with valid JSON arguments.", t.name),
            None => run(&t.name, &t.input),
        }).collect();
        wire::answer_turn(l.wire, &mut history, &v, &r, results);
    }
    Err(Stopped { usage, usd: spent, reason: format!("no answer within {max_rounds} rounds") }.into())
}

/// `run_tools` on the network, on whatever `e` names (`background_model::tool_endpoint`: any provider; a job's
/// `endpoint`: Anthropic): each round is one POST in `e.wire`'s format with `e`'s credential. No credential (not
/// connected, or a job's endpoint off Anthropic): no call. Blocking: call it from a thread outside the async runtime.
pub fn ask_tools_blocking(e: &Endpoint, c: &Call<'_>, tools: &[Tool], submit: &str, max_rounds: usize, run: &mut dyn FnMut(&str, &Value) -> String) -> Result<Answer> {
    // The tools run here; a T3 thread cannot call them (module doc).
    if ui::t3_instance(e.provider).is_some() { return Err(t3::NoToolsOnT3 { job: c.job }.into()) }
    let key = e.key.clone().with_context(|| format!("{} is not connected, or the background model is not on it", provider_name(e)))?;
    let system = agent::identity::for_agent(c.system, &agent::identity::current());
    let c = &with_effort(e, &edition_worded(c, &system));
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    let hash = prompt_hash(c);
    let mut send = |body: &Value, worst: f64| rt.block_on(post(e, &key, c, hash, body, worst));
    let names = agent::identity::current();
    let mut worded = |name: &str, input: &Value| agent::identity::for_agent(&run(name, input), &names);
    run_tools_on(&Loop::for_endpoint(e), c, tools, submit, max_rounds, &mut worded, &mut send)
}

/// `c` in the edition's wording, its system text given as `system` (`agent::identity::for_agent`: the public edition
/// never sends the owner's name; VM run 5 Nov 05:43). The personal edition's call is unchanged, byte for byte.
pub(crate) fn edition_worded<'a>(c: &Call<'_>, system: &'a str) -> Call<'a> {
    let names = agent::identity::current();
    let word = |p: &Prompt| Prompt { blocks: p.blocks.iter().map(|b| Block { text: agent::identity::for_agent(&b.text, &names), cache: b.cache }).collect() };
    Call { job: c.job, model: c.model.clone(), system, schema: c.schema.clone(), effort: c.effort, prompt: word(&c.prompt),
        history: c.history.iter().map(|t| Turn { user: word(&t.user), assistant: t.assistant.clone() }).collect(), session: c.session.clone() }
}

/// Effort names a level may carry (`Call::effort` is `&'static str`): OpenAI's, Anthropic's, T3's Claude extras.
const EFFORT_NAMES: [&str; 11] = ["none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra", "ultrathink", "ultracode", "auto"];

/// The effort a level asks for on `e`'s format: the level itself when it is a known name; the menu's "off" as the
/// least each format takes (Messages' and T3's "low", OpenAI's "minimal"); an unknown name: `None` (the job's own).
pub fn effort_name(level: &str, e: &Endpoint) -> Option<&'static str> {
    if level == "off" {
        return Some(if e.wire == Wire::Anthropic || ui::t3_instance(e.provider).is_some() { "low" } else { "minimal" });
    }
    EFFORT_NAMES.iter().find(|n| **n == level).copied()
}

/// Jobs the user waits on while they talk: Background Tasks ▸ Model's level applies to them only up to the job's own effort
/// (bug 2026-11-05T02-32-17: the suggester ran at their "max" on gpt-6-luna, 7-31 s a pass, its cards 11-21 s after the
/// line that started them).
const LIVE_JOBS: [&str; 1] = [crate::suggester::JOB];

/// The effort `job` runs at when the menu asks for `level`: the level, except that a live job never goes above its own.
pub fn capped_effort(job: &str, level: &'static str, own: &'static str) -> &'static str {
    let rank = |n: &str| EFFORT_NAMES.iter().position(|e| *e == n).unwrap_or(EFFORT_NAMES.len());
    if LIVE_JOBS.contains(&job) && rank(level) > rank(own) { own } else { level }
}

/// `c` at Background Tasks ▸ Model's level for `e`'s model, when the user picked one (else the job's default effort); a
/// live job at most at its own effort (`capped_effort`).
pub fn with_effort<'a>(e: &Endpoint, c: &Call<'a>) -> Call<'a> {
    let level = crate::background_model::effort_for(e.provider, &c.model);
    match level.as_deref().and_then(|l| effort_name(l, e)) {
        Some(effort) => Call { effort: capped_effort(c.job, effort, c.effort), ..c.clone() },
        None => c.clone(),
    }
}

impl std::ops::AddAssign for Usage {
    fn add_assign(&mut self, o: Usage) {
        self.input += o.input;
        self.output += o.output;
        self.cache_read += o.cache_read;
        self.cache_write_5m += o.cache_write_5m;
        self.cache_write_1h += o.cache_write_1h;
    }
}

#[cfg(test)]
#[path = "background_call_tests.rs"]
mod tests;
