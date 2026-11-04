//! Emergency mode (docs/emergency.md; bug note 2026-10-26T23-59-23, ~00:05): when Heimdall's own agent
//! cannot answer (a spent plan quota, a failed sign-in, a provider failing again and again, or no answer at all), a T3
//! thread covers for him. Everything that would have woken the agent still goes into Pi's session, as context without
//! a turn, and is relayed to that cover thread, which answers the user through the `heimdall-emergency` CLI.
//!
//! This module is the part without I/O: what counts as the agent failing, when to probe it again, which T3 harness
//! covers, the cover agent's first message, and its scoped token's file. The app owns the controller
//! (`crates/app/src/emergency.rs`); the Bridge owns the hold (`Bridge::hold_turns`).
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::{collections::VecDeque, path::Path};

/// Why the agent is down. `line` is what the user and the cover agent are told.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Cause {
    /// The plan's usage limit or the account's credit is spent. `reset_ms`: when it comes back, when the provider
    /// said; `line`: the quota module's own sentence (runtime/quota.mjs), when it sent one.
    Quota { reset_ms: Option<u64>, line: Option<String> },
    /// The provider rejected the sign-in or key, twice in a row.
    Auth(String),
    /// Requests failed `PROVIDER_FAILURES` times in a row within `PROVIDER_WINDOW_MS`.
    Provider(String),
    /// The user's words or a running turn got no answer and no progress for `NO_ANSWER_MS`.
    NoAnswer,
}
impl Cause {
    pub fn kind(&self) -> &'static str {
        match self { Self::Quota { .. } => "quota", Self::Auth(_) => "auth", Self::Provider(_) => "provider", Self::NoAnswer => "no_answer" }
    }
    /// The island's one line: the agent is down, why, a backup agent is on it, nothing for the user to do.
    pub fn island(&self) -> String {
        let why = match self {
            Self::Quota { reset_ms: Some(at), .. } => format!("Agent out of quota till {}", clock(*at)),
            Self::Quota { .. } => "Agent out of quota".into(),
            Self::Auth(_) => "Agent sign-in failed".into(),
            Self::Provider(_) => "Agent's provider failing".into(),
            Self::NoAnswer => "Agent not answering".into(),
        };
        format!("{why} · backup agent on it, nothing to do")
    }
    /// One plain sentence for the cover agent and the log.
    pub fn sentence(&self) -> String {
        match self {
            Self::Quota { line: Some(line), .. } => line.clone(),
            Self::Quota { reset_ms: Some(at), .. } => format!("The agent's plan usage limit is used up; it resets at {}.", clock(*at)),
            Self::Quota { .. } => "The agent's plan usage limit or credit is used up; the provider gave no reset time.".into(),
            Self::Auth(e) => format!("The agent's provider rejected its sign-in: {}", bounded(e, 300)),
            Self::Provider(e) => format!("The agent's provider failed {PROVIDER_FAILURES} requests in a row; the last error: {}", bounded(e, 300)),
            Self::NoAnswer => format!("The agent gave no answer and no progress for {} minutes.", NO_ANSWER_MS / 60_000),
        }
    }
    pub fn to_json(&self) -> Value {
        match self {
            Self::Quota { reset_ms, line } => json!({"kind":"quota","reset_ms":reset_ms,"line":line}),
            Self::Auth(e) | Self::Provider(e) => json!({"kind":self.kind(),"error":e}),
            Self::NoAnswer => json!({"kind":"no_answer"}),
        }
    }
    pub fn from_json(v: &Value) -> Option<Self> {
        let error = || v["error"].as_str().unwrap_or_default().to_owned();
        Some(match v["kind"].as_str()? {
            "quota" => Self::Quota { reset_ms: v["reset_ms"].as_u64(), line: v["line"].as_str().map(str::to_owned) },
            "auth" => Self::Auth(error()),
            "provider" => Self::Provider(error()),
            "no_answer" => Self::NoAnswer,
            _ => return None,
        })
    }
}
/// Local `HH:MM` of a Unix time in milliseconds.
pub fn clock(ms: u64) -> String {
    use chrono::TimeZone;
    chrono::Local.timestamp_millis_opt(ms as i64).single().map(|t| t.format("%H:%M").to_string()).unwrap_or_else(|| "?".into())
}
fn bounded(s: &str, chars: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= chars { s.into() } else { format!("{}…", s.chars().take(chars).collect::<String>()) }
}

/// What one failed request says about the agent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    /// A spent plan limit or credit: it lasts until the window resets, so one is enough.
    Quota,
    /// A rejected sign-in or key.
    Auth,
    /// Anything else the provider or transport returned (a 5xx, an overload, a request that never started).
    Other,
}
/// The kind of a failed turn's error, `None` when it is not the agent failing (the user's Stop, an abort, a reload).
pub fn classify(error: &str) -> Option<Failure> {
    let lower = error.to_ascii_lowercase();
    // Not the provider failing: the user's Stop, aborts, the loop guard, a reply cut by its own length (review 27 Oct).
    if lower.trim().is_empty() || ["stopped by user", "aborted", "cancelled", "canceled", "interrupted", "repeating loop", "settled without a successful reply"].iter().any(|p| lower.contains(p)) {
        return None;
    }
    // The quota task's own test (notice::quota_exhausted), plus a spent prepaid balance (26 Oct 22:02: "credit balance is
    // too low" from Anthropic): neither comes back by resending.
    // A spent prepaid balance (2 Nov 13:05: "402 Payment Required: Grok Build usage balance exhausted").
    if crate::notice::quota_exhausted(error) || lower.contains("credit balance") || crate::limits::parse(error, 0).is_some_and(|l| l.billing) {
        return Some(Failure::Quota);
    }
    // A status code as a number of its own: "401" inside a request id or a port is not one.
    let code = |c: &str| lower.match_indices(c).any(|(i, _)| {
        let digit = |b: Option<&u8>| b.is_some_and(u8::is_ascii_digit);
        !digit(i.checked_sub(1).and_then(|j| lower.as_bytes().get(j))) && !digit(lower.as_bytes().get(i + c.len()))
    });
    if code("401") || (code("403") && lower.contains("forbidden")) || ["unauthorized", "invalid api key", "invalid x-api-key", "authentication_error", "authentication failed", "sign in again", "signed out", "invalid_api_key", "--provider-token"]
        .iter().any(|p| lower.contains(p)) {
        return Some(Failure::Auth);
    }
    Some(Failure::Other)
}

/// A rejected sign-in this many times in a row is a failure (one can be a token refreshed a moment late).
pub const AUTH_FAILURES: u32 = 2;
/// Other failed requests: this many in a row, within `PROVIDER_WINDOW_MS`. Pi's own retry already resends passing
/// errors (provider-retry.mjs), so each one here is a request that stayed failed.
pub const PROVIDER_FAILURES: usize = 3;
pub const PROVIDER_WINDOW_MS: u64 = 10 * 60_000;
/// Work waiting for an answer (a running turn, the user's queued words) with no event from Pi for this long. A tool call
/// that is running or a compaction never counts: those are progress the agent cannot report.
pub const NO_ANSWER_MS: u64 = 5 * 60_000;

/// Decides when the agent has failed. Fed by the supervisor: every failed and successful turn, Pi's events, the
/// quota module's frames and a tick. A success clears everything: a blip never adds up across good turns.
#[derive(Debug, Default)]
pub struct Detector {
    auth: u32,
    other: VecDeque<u64>,
    last_error: String,
    quota: Option<(Option<u64>, Option<String>)>,
    /// Since when work has waited for an answer with no progress.
    waiting_since: Option<u64>,
    tools_running: u32,
    /// Since when a compaction Pi reported runs. It counts as progress only up to the harness's own compaction limit:
    /// a Pi killed mid-compaction never reports its end (27 Oct 03:25-03:53: no emergency in 28 dead minutes).
    compacting_since: Option<u64>,
    last_tick: Option<u64>,
    /// The reset time of the last quota warning (`event: "warning"`): a limit frame without one keeps it.
    warned_reset: Option<u64>,
}
/// Ticks further apart than this mean the Mac slept: the wait starts again (review 27 Oct).
const TICK_GAP_MS: u64 = 60_000;
/// A failed turn while the agent is on trial (the hand-back): no counting, the cover goes back on duty.
pub fn cause_of(error: &str) -> Option<Cause> {
    Some(match classify(error)? {
        Failure::Quota => Cause::Quota { reset_ms: None, line: None },
        Failure::Auth => Cause::Auth(error.into()),
        Failure::Other => Cause::Provider(error.into()),
    })
}
/// A relay run's failure as a cause: a spent limit or balance T3 reported (`limits::parse`, with its window and reset,
/// named for `who`, the account), else as `cause_of` reads it (bug note 2026-11-02T14-47-37: "run failed" was all).
pub fn relay_cause(error: &str, who: &str, now_ms: u64) -> Option<Cause> {
    match crate::limits::parse(error, now_ms) {
        Some(limit) => Some(limit.cause(who, now_ms)),
        None => cause_of(error),
    }
}
impl Detector {
    /// The quota module's `heimdall_quota` frame with `event: "limit"`: the reset time and the sentence for the
    /// failure that follows (or came just before it).
    /// A warned reset that has passed (`now_ms`) names an old window: it is dropped, never given as this limit's.
    pub fn quota_frame(&mut self, frame: &Value, now_ms: u64) -> Option<Cause> {
        if self.warned_reset.is_some_and(|at| at <= now_ms) { self.warned_reset = None; }
        if frame["event"] == "warning" {
            if let Some(at) = frame["resets_at_ms"].as_u64().filter(|&at| at > now_ms) { self.warned_reset = Some(at); }
            return None;
        }
        if frame["event"] != "limit" { return None; }
        let reset = frame["resets_at_ms"].as_u64().or(self.warned_reset);
        let line = frame["line"].as_str().map(str::to_owned);
        self.quota = Some((reset, line.clone()));
        Some(Cause::Quota { reset_ms: reset, line })
    }
    /// A turn ended with `error`. `Some` when that makes the agent failed.
    pub fn failed(&mut self, error: &str, now_ms: u64) -> Option<Cause> {
        self.waiting_since = None;
        match classify(error)? {
            Failure::Quota => {
                let (reset_ms, line) = self.quota.clone().unwrap_or_default();
                Some(Cause::Quota { reset_ms, line })
            }
            Failure::Auth => {
                self.auth += 1;
                (self.auth >= AUTH_FAILURES).then(|| Cause::Auth(error.into()))
            }
            Failure::Other => {
                self.other.push_back(now_ms);
                while self.other.front().is_some_and(|t| now_ms.saturating_sub(*t) > PROVIDER_WINDOW_MS) { self.other.pop_front(); }
                self.last_error = error.into();
                (self.other.len() >= PROVIDER_FAILURES).then(|| Cause::Provider(self.last_error.clone()))
            }
        }
    }
    /// A turn answered: the agent works.
    pub fn answered(&mut self) {
        *self = Self { tools_running: self.tools_running, compacting_since: self.compacting_since, last_tick: self.last_tick, warned_reset: self.warned_reset, ..Self::default() };
    }
    /// A new Pi process: the old one's running tools and compaction died with it.
    pub fn new_process(&mut self) {
        self.tools_running = 0;
        self.compacting_since = None;
    }
    /// Any event from Pi is progress; tool and compaction events also say whether silence is expected.
    pub fn event(&mut self, value: &Value, now_ms: u64) {
        match value["type"].as_str() {
            Some("tool_execution_start") => self.tools_running += 1,
            Some("tool_execution_end") => self.tools_running = self.tools_running.saturating_sub(1),
            Some("compaction_start") => { self.compacting_since.get_or_insert(now_ms); }
            Some("compaction_end") => self.compacting_since = None,
            Some("agent_settled") => { self.tools_running = 0; self.compacting_since = None; }
            _ => {}
        }
        if self.waiting_since.is_some() { self.waiting_since = Some(now_ms); }
    }
    /// The supervisor's tick: `waiting` is true while a turn runs or the user's words wait for one.
    pub fn tick(&mut self, waiting: bool, now_ms: u64) -> Option<Cause> {
        if self.last_tick.replace(now_ms).is_some_and(|t| now_ms.saturating_sub(t) > TICK_GAP_MS) { self.waiting_since = None; }
        let compacting = self.compacting_since.is_some_and(|t| now_ms.saturating_sub(t) < crate::delivery::COMPACTION_WAIT.as_millis() as u64);
        if !waiting || self.tools_running > 0 || compacting { self.waiting_since = None; return None; }
        let since = *self.waiting_since.get_or_insert(now_ms);
        (now_ms.saturating_sub(since) >= NO_ANSWER_MS).then(|| { self.waiting_since = None; Cause::NoAnswer })
    }
}

/// When to look at the agent again. A known reset waits for it (plus a minute: windows open a little late); otherwise
/// 2 minutes, doubling to 30. A probe is one tiny request (or a sign-in check), never a turn.
pub fn next_probe_ms(cause: &Cause, failed_probes: u32, now_ms: u64) -> u64 {
    if let Cause::Quota { reset_ms: Some(at), .. } = cause {
        if *at + 60_000 > now_ms { return at + 60_000; }
    }
    now_ms + (2 * 60_000u64 << failed_probes.min(4)).min(30 * 60_000)
}

/// One T3 harness that could cover.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Harness {
    pub instance: String,
    pub driver: String,
    pub name: String,
    pub model: String,
}
/// Whether a T3 driver runs on the same subscription as Heimdall's provider, so it fails with it: ChatGPT sign-in
/// (`openai`) is what Codex runs on (26 Oct 23:33: Codex threads hit the same quota), OpenCode Go is OpenCode's.
pub fn shares_subscription(provider: &str, driver: &str) -> bool {
    let driver = driver.to_ascii_lowercase();
    match provider {
        crate::provider::CHATGPT => driver.contains("codex"),
        crate::provider::OPENCODE_GO => driver.contains("opencode"),
        _ => false,
    }
}
/// The harnesses to try, in `order` (docs/claude-relay.md, "Which T3 harness"): each entry a driver-kind substring,
/// `*` every other harness in T3's order, `-x` never. Each with the first model T3 lists for it (its live catalog, never a
/// remembered id). Left out: disabled instances, ones without models or without full access, and those on the failed
/// provider's subscription. T3 tells an outside client nothing about plans, quota or sign-in health, so the order is all
/// there is to rank on; the first run's reply is the health check.
pub fn cover_harnesses(catalog: &Value, failed_provider: Option<&str>, order: &[String]) -> Vec<Harness> {
    let never: Vec<String> = order.iter().filter_map(|o| o.strip_prefix('-')).map(str::to_ascii_lowercase).collect();
    let wanted: Vec<String> = order.iter().filter(|o| !o.starts_with('-')).map(|o| o.to_ascii_lowercase()).collect();
    let rest = wanted.iter().position(|o| o == "*");
    let mut out: Vec<(usize, Harness)> = Vec::new();
    for p in catalog["providers"].as_array().into_iter().flatten() {
        let disabled = p["constraints"].as_array().into_iter().flatten().any(|c| c.as_str().is_some_and(|c| c.to_ascii_lowercase().contains("disabled")));
        let full = p["runtimeModes"].as_array().is_none_or(|m| m.iter().any(|x| x == "full-access"));
        let (Some(instance), Some(model)) = (p["providerInstanceId"].as_str(), p["models"].as_array().and_then(|m| m.first()).and_then(|m| m["id"].as_str())) else { continue };
        let driver = p["driverKind"].as_str().unwrap_or(instance).to_owned();
        if disabled || !full || failed_provider.is_some_and(|f| shares_subscription(f, &driver)) { continue; }
        let lower = driver.to_ascii_lowercase();
        if never.iter().any(|n| lower.contains(n.as_str())) { continue; }
        let Some(rank) = wanted.iter().position(|o| o != "*" && lower.contains(o.as_str())).or(rest) else { continue };
        out.push((rank, Harness { instance: instance.into(), name: p["displayName"].as_str().unwrap_or(instance).into(), driver, model: model.into() }));
    }
    out.sort_by_key(|(rank, _)| *rank);
    out.into_iter().map(|(_, h)| h).collect()
}
/// Relay mode's chosen model: each harness whose T3 instance lists `model` runs it instead of its first model.
pub fn prefer_model(harnesses: &mut [Harness], catalog: &Value, model: &str) {
    for h in harnesses {
        let lists = catalog["providers"].as_array().into_iter().flatten()
            .filter(|p| p["providerInstanceId"].as_str() == Some(h.instance.as_str()))
            .flat_map(|p| p["models"].as_array().into_iter().flatten())
            .any(|m| m["id"].as_str() == Some(model));
        if lists { h.model = model.into(); }
    }
}
/// The Claude models relay mode can run: the models of T3's first Claude instance, as (id, name).
pub fn relay_models(catalog: &Value) -> Vec<(String, String)> {
    catalog["providers"].as_array().into_iter().flatten()
        .find(|p| p["driverKind"].as_str().or(p["providerInstanceId"].as_str()).is_some_and(|d| d.to_ascii_lowercase().contains("claude")))
        .and_then(|p| p["models"].as_array())
        .into_iter().flatten()
        .filter_map(|m| { let id = m["id"].as_str()?; Some((id.to_owned(), m["label"].as_str().or(m["name"].as_str()).unwrap_or(id).to_owned())) })
        .collect()
}
/// The Claude accounts relay mode can run on: T3's Claude provider instances that can run (not disabled, full access
/// offered), as (instance id, display name), in T3's order (Agent ▸ Model ▸ Claude Code subscription's account rows).
pub fn relay_accounts(catalog: &Value) -> Vec<(String, String)> {
    catalog["providers"].as_array().into_iter().flatten()
        .filter(|p| p["driverKind"].as_str().or(p["providerInstanceId"].as_str()).is_some_and(|d| d.to_ascii_lowercase().contains("claude")))
        .filter(|p| !p["constraints"].as_array().into_iter().flatten().any(|c| c.as_str().is_some_and(|c| c.to_ascii_lowercase().contains("disabled"))))
        .filter(|p| p["runtimeModes"].as_array().is_none_or(|m| m.iter().any(|x| x == "full-access")))
        .filter_map(|p| { let id = p["providerInstanceId"].as_str()?; Some((id.to_owned(), p["displayName"].as_str().unwrap_or(id).to_owned())) })
        .collect()
}
/// The default order, both editions: Claude, then Codex, then the rest. The relay is Heimdall's main assistant, not a
/// delegate:, allows Claude → Codex (and back) for it when no account of the failed provider works
/// (Assistant/Plans/relay-account-routing-2026-10-30.md). Delegates stay Claude only (27 Oct 03:25: "do not spawn
/// Codex subagents like ever"; `delegate_task` is untouched by this order).
pub fn default_order(_public: bool) -> Vec<String> {
    let order: &[&str] = &["claude", "codex", "*"];
    order.iter().map(|s| s.to_string()).collect()
}

/// A harness that could not run ends its turn with the provider's error as the whole reply ("API Error: 429 …",
/// "Claude AI usage limit reached|…", "Credit balance is too low"). Only such a reply counts: a cover agent that
/// answered will talk about the quota too (lab 27 Oct 00:50: "Emergency mode since 00:50: The agent's plan usage
/// limit…" was taken for the harness failing, and three working harnesses were archived).
pub fn harness_error_reply(reply: &str) -> bool {
    let lower = reply.trim().to_ascii_lowercase();
    lower.len() < 400 && ["api error", "claude ai usage limit", "credit balance is too low", "invalid api key", "please run /login",
        "you've hit your", "you have hit your", "usage limit reached", "error:", "unexpected status", "stream error"]
        .iter().any(|p| lower.starts_with(p))
}
/// The CLI's grant: 256 random bits as hex (two v4 UUIDs).
pub fn new_token() -> String {
    format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple())
}
/// Where the cover agent's connection lives inside Heimdall's data folder.
pub const DIR: &str = "emergency";
pub const CONNECTION: &str = "connection.json";
/// The CLI's connection: the scoped MCP server, its token and the cover thread it was issued for. The folder is 0700
/// and the file 0600, written atomically; it exists only while emergency mode is on.
pub fn write_connection(data: &Path, url: &str, token: &str, thread: &str) -> Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let dir = data.join(DIR);
    std::fs::create_dir_all(&dir)?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    let body = json!({"url":url,"token":token,"thread":thread,"issued_ms":crate::now_ms()}).to_string();
    let tmp = dir.join(format!(".{CONNECTION}.{}", uuid::Uuid::new_v4().simple()));
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
        f.write_all(body.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, dir.join(CONNECTION)).context("emergency connection not written")?;
    Ok(())
}
/// The token dies with emergency mode: the file goes (the server stops accepting it on its own).
pub fn remove_connection(data: &Path) -> Result<()> {
    match std::fs::remove_file(data.join(DIR).join(CONNECTION)) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
        _ => Ok(()),
    }
}

/// Why the relay acts for Heimdall: the user chose it (relay mode), or the agent failed (emergency mode).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason { #[default] Emergency, Chosen }

/// The relay thread's first message (~00:05 and 19:50: "this is the emergency protocol, continue
/// Heimdall's work"). The relay's system prompt comes first, whole (`relay_prompt`); the conversation itself goes in a
/// private context file: T3 caps a first message at 120,000 characters, and the takeover carries far more
/// (`CONTEXT_BYTES`).
pub struct Brief<'a> {
    pub reason: Reason,
    /// Emergency mode's cause.
    pub cause: Option<&'a Cause>,
    pub since_ms: u64,
    /// The CLI as the relay runs it, with `--data-dir` when Heimdall is not on its default folder.
    pub cli: &'a str,
    /// docs/emergency.md as this build ships it (docs/claude-relay.md beside it).
    pub docs: &'a str,
    /// The relay's whole system prompt, filled (`relay_prompt::text`): it goes first, between `PROMPT_START` and
    /// `PROMPT_END`, since T3 threads take no system prompt.
    pub prompt: &'a str,
    /// The context file (`write_context`).
    pub context: &'a str,
    /// The recent conversation, oldest first, already bounded (`TAIL_BYTES`).
    pub tail: &'a str,
    /// The plan lists, already bounded.
    pub plan: &'a str,
}
/// The first message's opening, word for word: emergency mode, then relay mode.
pub const OPENING: &str = "This is Heimdall's emergency protocol. Heimdall's agent failed, and you now act as Heimdall: continue his work.";
pub const OPENING_RELAY: &str = "You are Heimdall's agent now. User chose to run Heimdall through you, a T3 Code thread, instead of his built-in agent.";
/// The delimiters around the system prompt at the top of the first message.
pub const PROMPT_START: &str = "<system_prompt>";
pub const PROMPT_END: &str = "</system_prompt>";
/// T3's cap on a thread's first message, in characters.
pub const FIRST_MESSAGE_CHARS: usize = 120_000;
/// The conversation's last part in the first message itself, and the plan.
pub const TAIL_BYTES: usize = 12 * 1024;
pub const PLAN_BYTES: usize = 16 * 1024;
/// The context file's conversation: the last 100,000 tokens of it at about 4 bytes a token (the user, 19:50: "the last
/// ~100k tokens of what Heimdall had").
pub const CONTEXT_BYTES: usize = 400 * 1024;
pub const CONTEXT: &str = "context.md";
impl Brief<'_> {
    pub fn text(&self) -> String {
        let cli = self.cli;
        let emergency = self.reason == Reason::Emergency;
        let cause = match (emergency, self.cause) {
            (true, Some(c)) => format!("\n\nWhat failed ({}): {}{}", c.kind(), c.sentence(),
                if matches!(c, Cause::Quota { line: Some(_), .. }) { "\nHeimdall already said that sentence aloud: don't repeat it." } else { "" }),
            _ => String::new(),
        };
        let end = if emergency { " Hand back early with emergency_end; Heimdall checks his agent first and tells you if it still fails." } else { "" };
        let now = if emergency {
            "Now: find in the context what User and Heimdall were doing and left unfinished, pick it up, and tell User in one \
short message (your reply to this message): what broke, that you have it, and what you are continuing. After that, speak \
only when he talks to you or must act. Heimdall checks his agent by himself and tells you here when to wrap up."
        } else {
            "Now: read the context. Reply NO_REPLY to this message unless something in the context needs User now. \
You act for Heimdall until User switches back; you will be told here."
        };
        let text = format!("{opening}{cause}
You are the only agent acting for Heimdall now.

Your system prompt is above, Heimdall's own with the relay's notes: you are that agent; you run here instead of inside \
Heimdall, so where it names a tool, use his MCP tools. Then read your context, in pages: {context} (Heimdall's recent \
conversation, oldest first, the last 100,000 tokens of it; the open plan; the T3 tasks he follows).

How to act: Heimdall's own tools, every one his built-in agent has, as mcp__heimdall__<tool> (speak, island_notice, \
plan_edit, delegate_task ...); emergency_status says who you are, since when, and the next check.{end} Without \
mcp__heimdall__ tools in your list, run the same tools from bash: `{cli} tools` lists them, `{cli} call <tool> '<json>'` \
runs one by its name (images come back as file paths).

Everything that would wake Heimdall arrives here as a message headed [For Heimdall, …]: User said, typed or clicked, a
task finished, a T3 thread asks, a scheduled wake, User pressed Stop. Answer it as Heimdall would. Your final reply to
each message is spoken to User in Heimdall's voice and shown on the island, exactly like his own replies: short spoken
words, no markdown, never what you already said with `speak`. Reply NO_REPLY when nothing should be said. Everything you
do with his tools is written into Heimdall's conversation, so he knows it when he is back. Protocol: {docs}

{now}

Heimdall's recent conversation, the last part (the whole context is in the file):
{tail}

Plan:
{plan}",
            opening = if emergency { OPENING } else { OPENING_RELAY },
            context = self.context, docs = self.docs,
            tail = if self.tail.trim().is_empty() { "(nothing recent)" } else { self.tail },
            plan = if self.plan.trim().is_empty() { "(empty)" } else { self.plan });
        // Personalise (26 Oct): the configured names, as in the agent's own prompt.
        // The prompt has its names already (`harness::fill_prompt`).
        let text = crate::identity::names_only(&text, &crate::identity::current());
        let text = if emergency { text + &format!("\n\nDown since {}.", clock(self.since_ms)) } else { text };
        format!("{PROMPT_START}\n{}\n{PROMPT_END}\n\n{text}", self.prompt.trim_end())
    }
}
/// The takeover's context file (0600 in the 0700 folder): the cause, the conversation, the plan and the tasks.
pub fn write_context(data: &Path, body: &str) -> Result<std::path::PathBuf> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let dir = data.join(DIR);
    std::fs::create_dir_all(&dir)?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    let path = dir.join(CONTEXT);
    let tmp = dir.join(format!(".{CONTEXT}.{}", uuid::Uuid::new_v4().simple()));
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
        f.write_all(body.as_bytes())?;
    }
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}
/// The last `max` bytes of `text` on a line boundary.
pub fn tail_bytes(text: &str, max: usize) -> String {
    if text.len() <= max { return text.into(); }
    let mut start = text.len() - max;
    while !text.is_char_boundary(start) { start += 1; }
    let cut = &text[start..];
    cut.split_once('\n').map_or(cut, |(_, rest)| rest).into()
}

/// One input the hold relayed to the cover thread instead of waking the agent (`Bridge::hold_turns`).
#[derive(Clone, Debug)]
pub struct Relayed {
    pub id: String,
    /// `wake` (the user's voice), `typed`, or a notice kind (`answer`, `question`, `event`, `result`, `scheduled`).
    pub kind: String,
    pub text: String,
    pub images: Vec<crate::protocol::Attachment>,
}
pub type Relay = std::sync::Arc<dyn Fn(Relayed) + Send + Sync>;
/// The line in front of each held input in Pi's session: it reached the cover agent, not a turn of his.
pub const HELD: &str = "[Relayed: this reached the relay agent (a T3 thread acting as you), so it started no turn of yours.]";

#[cfg(test)]
mod tests {
    use super::*;

    const QUOTA: &str = "The ChatGPT user has reached their Subscription Sharing usage limit. Ask the user to try again after their usage limit resets or use an API key instead.";

    #[test]
    fn a_spent_quota_fails_the_agent_at_once_with_the_reset_the_quota_module_sent() {
        let mut d = Detector::default();
        let frame = json!({"type":"heimdall_quota","event":"limit","provider":"openai","line":"ChatGPT's five-hour limit is used up; it resets at 03:04.","resets_at_ms":1_793_066_649_000u64});
        d.quota_frame(&frame, 1_000);
        let cause = d.failed(QUOTA, 1_000).unwrap();
        assert_eq!(cause, Cause::Quota { reset_ms: Some(1_793_066_649_000), line: Some("ChatGPT's five-hour limit is used up; it resets at 03:04.".into()) });
        assert_eq!(cause.sentence(), "ChatGPT's five-hour limit is used up; it resets at 03:04.");
        assert!(cause.island().starts_with("Agent out of quota till ") && cause.island().ends_with("· backup agent on it, nothing to do"));
        // Without the frame: still a quota, with no time invented.
        assert_eq!(Detector::default().failed("429 usage_limit_reached", 1), Some(Cause::Quota { reset_ms: None, line: None }));
        assert_eq!(Detector::default().failed("400 Your credit balance is too low to access the Anthropic API", 1).unwrap().kind(), "quota");
    }

    #[test]
    fn one_passing_error_never_flips_and_a_success_clears_the_count() {
        let mut d = Detector::default();
        assert_eq!(d.failed("503 Service Unavailable", 0), None);
        assert_eq!(d.failed("529 overloaded_error", 1_000), None);
        d.answered();
        assert_eq!(d.failed("503 Service Unavailable", 2_000), None);
        assert_eq!(d.failed("503 Service Unavailable", 3_000), None);
        assert!(matches!(d.failed("Anthropic did not answer within 120 seconds", 4_000), Some(Cause::Provider(e)) if e.contains("did not answer")));
        // Spread over more than the window: not "again and again".
        let mut d = Detector::default();
        assert_eq!(d.failed("503", 0), None);
        assert_eq!(d.failed("503", PROVIDER_WINDOW_MS + 1), None);
        assert_eq!(d.failed("503", 2 * PROVIDER_WINDOW_MS + 2), None);
        // A rejected sign-in needs two in a row; the user's Stop and aborts are not failures.
        let mut d = Detector::default();
        assert_eq!(d.failed("401 Unauthorized", 0), None);
        assert!(matches!(d.failed("ChatGPT: sign in again", 1), Some(Cause::Auth(_))));
        let mut d = Detector::default();
        for _ in 0..5 { assert_eq!(d.failed("Stopped by User", 0), None); assert_eq!(d.failed("Request was aborted", 0), None); }
    }

    /// Ticks every 10 s, as the supervisor's.
    fn ticks(d: &mut Detector, waiting: bool, from: u64, to: u64) -> Option<Cause> {
        (from..=to).step_by(10_000).find_map(|t| d.tick(waiting, t))
    }
    #[test]
    fn no_answer_needs_five_quiet_minutes_and_a_running_tool_or_compaction_is_not_silence() {
        let mut d = Detector::default();
        assert_eq!(ticks(&mut d, true, 0, NO_ANSWER_MS - 10_000), None);
        assert_eq!(d.tick(true, NO_ANSWER_MS), Some(Cause::NoAnswer));
        // Progress restarts the clock; nothing waiting stops it.
        let mut d = Detector::default();
        assert_eq!(ticks(&mut d, true, 0, NO_ANSWER_MS - 20_000), None);
        d.event(&json!({"type":"message_update"}), NO_ANSWER_MS - 10_000);
        assert_eq!(ticks(&mut d, true, NO_ANSWER_MS - 10_000, 2 * NO_ANSWER_MS - 20_000), None);
        assert_eq!(ticks(&mut d, false, 2 * NO_ANSWER_MS, 3 * NO_ANSWER_MS), None);
        // A ten-minute bash call is work, not silence.
        let mut d = Detector::default();
        d.event(&json!({"type":"tool_execution_start"}), 0);
        assert_eq!(ticks(&mut d, true, 0, 2 * NO_ANSWER_MS), None);
        d.event(&json!({"type":"tool_execution_end"}), 2 * NO_ANSWER_MS);
        assert_eq!(ticks(&mut d, true, 2 * NO_ANSWER_MS, 3 * NO_ANSWER_MS - 10_000), None);
        assert_eq!(d.tick(true, 3 * NO_ANSWER_MS), Some(Cause::NoAnswer));
        let mut d = Detector::default();
        d.event(&json!({"type":"compaction_start"}), 0);
        assert_eq!(ticks(&mut d, true, 0, 3 * NO_ANSWER_MS), None);
        // A Mac asleep with a turn running: the first tick after waking starts the wait, it never ends it.
        let mut d = Detector::default();
        assert_eq!(d.tick(true, 0), None);
        assert_eq!(d.tick(true, 2 * 60 * 60_000), None);
        assert_eq!(ticks(&mut d, true, 2 * 60 * 60_000, 2 * 60 * 60_000 + NO_ANSWER_MS - 10_000), None);
    }

    /// Night audit H7 (27 Oct 03:25-03:53): Pi died mid-compaction, its `compaction_end` never came, and the
    /// detector took 28 dead minutes for a compaction. A new process clears it; without one, a compaction counts as
    /// progress only up to the harness's own compaction limit.
    #[test]
    fn a_compaction_that_died_with_its_pi_does_not_hide_a_dead_agent() {
        let limit = crate::delivery::COMPACTION_WAIT.as_millis() as u64;
        let mut d = Detector::default();
        d.event(&json!({"type":"compaction_start"}), 0);
        d.new_process();
        assert_eq!(ticks(&mut d, true, 10_000, NO_ANSWER_MS + 10_000), Some(Cause::NoAnswer), "the new Pi is not compacting");
        let mut d = Detector::default();
        d.event(&json!({"type":"tool_execution_start"}), 0);
        d.new_process();
        assert_eq!(ticks(&mut d, true, 10_000, NO_ANSWER_MS + 10_000), Some(Cause::NoAnswer), "nor running the old one's tools");
        // No end ever arrives: the compaction stops counting at the harness's limit.
        let mut d = Detector::default();
        d.event(&json!({"type":"compaction_start"}), 0);
        assert_eq!(ticks(&mut d, true, 0, limit - 10_000), None);
        assert_eq!(ticks(&mut d, true, limit, limit + NO_ANSWER_MS + 10_000), Some(Cause::NoAnswer));
        // A repeated start frame does not move the start.
        let mut d = Detector::default();
        d.event(&json!({"type":"compaction_start"}), 0);
        d.event(&json!({"type":"compaction_start"}), limit - 10_000);
        assert_eq!(ticks(&mut d, true, limit, limit + NO_ANSWER_MS + 10_000), Some(Cause::NoAnswer));
    }
    /// Bug note 2026-10-27T02-40: the warning frame named the reset, the limit frame did not, and `status` gave the
    /// next probe as if it were the reset.
    #[test]
    fn a_limit_frame_without_a_reset_keeps_the_one_its_warning_named() {
        let reset = 1_793_066_649_000u64;
        let warning = json!({"type":"heimdall_quota","event":"warning","resets_at_ms":reset,"line":"80% used"});
        let limit = json!({"type":"heimdall_quota","event":"limit","line":"ChatGPT's limit is used up."});
        let mut d = Detector::default();
        assert_eq!(d.quota_frame(&warning, reset - 3_600_000), None);
        d.answered();
        let cause = d.quota_frame(&limit, reset - 60_000).unwrap();
        assert_eq!(cause, Cause::Quota { reset_ms: Some(reset), line: Some("ChatGPT's limit is used up.".into()) });
        assert_eq!(d.failed(QUOTA, 1_000), Some(cause));
        // Review 27 Oct: a warning whose reset has passed is an old window's; the next limit's reset is unknown.
        let mut d = Detector::default();
        d.quota_frame(&warning, reset - 3_600_000);
        let cause = d.quota_frame(&limit, reset + 60_000).unwrap();
        assert_eq!(cause, Cause::Quota { reset_ms: None, line: Some("ChatGPT's limit is used up.".into()) });
        // A warning that arrives already past its reset is not kept either.
        let mut d = Detector::default();
        d.quota_frame(&warning, reset + 1);
        assert_eq!(d.quota_frame(&limit, reset + 2), Some(Cause::Quota { reset_ms: None, line: Some("ChatGPT's limit is used up.".into()) }));
    }

    #[test]
    fn status_codes_count_only_as_numbers_of_their_own_and_a_trial_failure_needs_no_count() {
        assert_eq!(classify("401 Unauthorized"), Some(Failure::Auth));
        assert_eq!(classify("HTTP 403 Forbidden"), Some(Failure::Auth));
        assert_eq!(classify("503 upstream error (request req_4012345)"), Some(Failure::Other));
        assert_eq!(classify("connect to 127.0.0.1:14010 refused"), Some(Failure::Other));
        assert_eq!(classify("Pi settled without a successful reply"), None);
        assert_eq!(cause_of("503 Service Unavailable"), Some(Cause::Provider("503 Service Unavailable".into())));
        assert_eq!(cause_of("Stopped by User"), None);
    }

    #[test]
    fn probes_wait_for_a_known_reset_and_otherwise_back_off_to_half_an_hour() {
        let quota = Cause::Quota { reset_ms: Some(10_000_000), line: None };
        assert_eq!(next_probe_ms(&quota, 0, 1_000), 10_060_000);
        // A reset already past: back off like any other cause.
        assert_eq!(next_probe_ms(&quota, 0, 20_000_000), 20_000_000 + 120_000);
        let steps: Vec<u64> = (0..7).map(|n| next_probe_ms(&Cause::NoAnswer, n, 0) / 60_000).collect();
        assert_eq!(steps, [2, 4, 8, 16, 30, 30, 30]);
    }

    fn catalog() -> Value {
        // The shape of T3's live orchestrator_capabilities (27 Oct), ids made up.
        json!({"providers":[
            {"providerInstanceId":"codex","driverKind":"codex","displayName":"Codex","models":[{"id":"codex-model-a"},{"id":"codex-model-b"}],"constraints":[]},
            {"providerInstanceId":"cursor","driverKind":"cursor","displayName":"Cursor","models":[],"constraints":["Provider instance is disabled."]},
            {"providerInstanceId":"grok","driverKind":"grok","displayName":"Grok","models":[{"id":"grok-model"}],"runtimeModes":["approval-required","auto","full-access"],"constraints":[]},
            {"providerInstanceId":"muse","driverKind":"muse","displayName":"Muse","models":[{"id":"muse-model"}],"runtimeModes":["approval-required"],"constraints":[]},
            {"providerInstanceId":"claudeAgent","driverKind":"claudeAgent","displayName":"Claude","models":[{"id":"claude-model-a"},{"id":"claude-model-b"}],"constraints":[]},
            {"providerInstanceId":"pi","driverKind":"pi","displayName":"Pi","models":[{"id":"default"}],"constraints":["Provider instance is disabled."]}
        ]})
    }

    #[test]
    fn claude_covers_first_then_codex_then_the_rest() {
        // The main assistant may fall back to Codex; never on the failed ChatGPT subscription.
        let order = |p: Option<&str>, public: bool| cover_harnesses(&catalog(), p, &default_order(public)).into_iter().map(|h| format!("{}/{}", h.instance, h.model)).collect::<Vec<_>>();
        assert_eq!(order(Some("anthropic"), false), ["claudeAgent/claude-model-a", "codex/codex-model-a", "grok/grok-model"]);
        assert_eq!(order(Some(crate::provider::CHATGPT), false), ["claudeAgent/claude-model-a", "grok/grok-model"]);
        assert_eq!(order(None, false), ["claudeAgent/claude-model-a", "codex/codex-model-a", "grok/grok-model"]);
        // The public edition (audit 27 Oct): Claude, Codex, the rest; the shared-subscription skip still holds.
        assert_eq!(order(None, true), ["claudeAgent/claude-model-a", "codex/codex-model-a", "grok/grok-model"]);
        assert_eq!(order(Some(crate::provider::CHATGPT), true), ["claudeAgent/claude-model-a", "grok/grok-model"]);
        // Another user's set: no Claude at all, an unknown harness ranks with the rest.
        let other = json!({"providers":[{"providerInstanceId":"zed","driverKind":"zed","displayName":"Zed","models":[{"id":"z1"}],"constraints":[]}]});
        assert_eq!(cover_harnesses(&other, None, &default_order(false)).len(), 1);
        assert!(cover_harnesses(&other, None, &["claude".into()]).is_empty(), "no `*`: only the named ones");
        assert!(shares_subscription("opencode-go", "opencode") && !shares_subscription("anthropic", "claudeAgent"));
    }

    #[test]
    fn relay_accounts_are_the_claude_instances_that_can_run() {
        let mut c = catalog();
        let providers = c["providers"].as_array_mut().unwrap();
        providers.push(json!({"providerInstanceId":"claudeAgent_claude_code_2","driverKind":"claudeAgent","displayName":"Claude (work)","models":[{"id":"claude-model-a"}],"constraints":[]}));
        providers.push(json!({"providerInstanceId":"claudeAgent_old","driverKind":"claudeAgent","displayName":"Claude (old)","models":[],"constraints":["Provider instance is disabled."]}));
        assert_eq!(relay_accounts(&c), [("claudeAgent".to_string(), "Claude".to_string()), ("claudeAgent_claude_code_2".to_string(), "Claude (work)".to_string())]);
        assert!(relay_accounts(&json!({})).is_empty());
    }

    #[test]
    fn only_an_error_as_the_whole_reply_means_the_harness_could_not_run() {
        for failed in ["API Error: 429 {\"type\":\"error\",\"error\":{\"type\":\"rate_limit_error\"}}", "Claude AI usage limit reached|1793066649", "Credit balance is too low", "Error: stream disconnected before completion: usage limit reached"] {
            assert!(harness_error_reply(failed), "{failed}");
        }
        // The lab's real answer: it talks about the quota because that is what failed.
        let answer = "I ran both lab commands.\n\nThe speak command printed: \"Speech queued: 1.\" The status command printed: \"Emergency mode since 00:50: The agent's plan usage limit or credit is used up; the provider gave no reset time.\"";
        assert!(!harness_error_reply(answer));
        assert!(!harness_error_reply(&format!("API Error explained: {}", "x".repeat(500))));
    }
    #[test]
    fn the_connection_file_is_private_and_goes_with_emergency_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let token = new_token();
        assert_eq!(token.len(), 64);
        assert_ne!(token, new_token());
        write_connection(dir.path(), "http://127.0.0.1:9/mcp", &token, "thread-1").unwrap();
        let file = dir.path().join(DIR).join(CONNECTION);
        assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::metadata(dir.path().join(DIR)).unwrap().permissions().mode() & 0o777, 0o700);
        let v: Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
        assert_eq!((v["token"].as_str(), v["thread"].as_str()), (Some(token.as_str()), Some("thread-1")));
        assert_eq!(std::fs::read_dir(dir.path().join(DIR)).unwrap().count(), 1, "no temporary file left");
        remove_connection(dir.path()).unwrap();
        assert!(!file.exists());
        remove_connection(dir.path()).unwrap();
    }

    #[test]
    fn the_first_message_carries_the_whole_prompt_then_the_protocol_the_context_and_every_tool() {
        let cause = Cause::Quota { reset_ms: None, line: Some("ChatGPT's five-hour limit is used up; it resets at 03:04.".into()) };
        let brief = |reason, prompt: &str, tail: &str, plan: &str| Brief { reason, cause: Some(&cause), since_ms: 0, cli: "/A/heimdall-emergency", docs: "/V/docs/emergency.md", prompt, context: "/D/emergency/context.md", tail, plan }.text();
        let after = |text: &str| text.split_once(PROMPT_END).unwrap().1.trim_start().to_string();
        let text = brief(Reason::Emergency, "# Heimdall's relay\nPROMPT BODY", "User: are you there?", "To do: ship it");
        assert!(text.starts_with(&format!("{PROMPT_START}\n# Heimdall's relay\nPROMPT BODY\n{PROMPT_END}\n\n{OPENING}")), "{text}");
        assert!(after(&text).starts_with(OPENING));
        // The user, 19:50: the takeover continues the work in flight and tells the user once, in one short message.
        assert!(text.contains("left unfinished, pick it up, and tell User in one\nshort message") || text.contains("left unfinished, pick it up, and tell User in one short message"), "{text}");
        for part in ["ChatGPT's five-hour limit is used up", "mcp__heimdall__<tool>", "emergency_status", "emergency_end", "/A/heimdall-emergency tools", "/A/heimdall-emergency call", "/V/docs/emergency.md", "/D/emergency/context.md", "NO_REPLY", "User: are you there?", "To do: ship it"] {
            assert!(text.contains(part), "{part}");
        }
        let relay = after(&brief(Reason::Chosen, "P", "", ""));
        assert!(relay.starts_with(OPENING_RELAY) && !relay.contains("What failed") && !relay.contains("emergency_end") && !relay.contains("Down since"), "{relay}");
        // The real prompt with the longest tail and plan still fits T3's first message.
        let d = tempfile::tempdir().unwrap();
        let prompt = crate::relay_prompt::text(std::path::Path::new("/nowhere/heimdall"), d.path(), "/Applications/Heimdall.app/Contents/MacOS/heimdall-emergency --data-dir /Users/someone/Library/Application Support/Heimdall");
        let tail = "[23:59] To Heimdall: a long line of words that User said\n".repeat(TAIL_BYTES / 56 + 1);
        let tail = tail_bytes(&tail, TAIL_BYTES);
        let plan = "To do: a plan item with a few words in it\n".repeat(PLAN_BYTES / 42 + 1);
        let plan = tail_bytes(&plan, PLAN_BYTES);
        let full = brief(Reason::Emergency, &prompt, &tail, &plan);
        assert!(full.contains(prompt.trim_end()));
        eprintln!("relay prompt {} chars; first message with max tail and plan {} chars", prompt.chars().count(), full.chars().count());
        assert!(full.chars().count() < FIRST_MESSAGE_CHARS, "{} chars: over T3's first-message cap", full.chars().count());
        let dir = tempfile::tempdir().unwrap();
        let path = write_context(dir.path(), "hello").unwrap();
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        let long = "line\n".repeat(10_000);
        let cut = tail_bytes(&long, TAIL_BYTES);
        assert!(cut.len() <= TAIL_BYTES && cut.starts_with("line\n"));
        assert_eq!(tail_bytes("short", 10), "short");
        assert_eq!(Cause::from_json(&cause.to_json()), Some(cause));
        assert_eq!(Cause::from_json(&Cause::Auth("401".into()).to_json()), Some(Cause::Auth("401".into())));
    }
}
