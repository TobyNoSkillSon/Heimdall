//! The Debug Agent window's content, platform-neutral: the conversation's telemetry (tokens and context fill,
//! compactions, tool calls and failed edits, errors, latency) and the timeline's turns (what woke the agent and how
//! the turn ended). `agent_view` owns the rows; `mac/agent.rs` draws this.

use std::collections::HashMap;

use crate::agent_notice::NoticeKind;
use crate::agent_view::{failure_copy, typed, Row, Tone};
use crate::model::*;

/// A final text that means the agent chose not to answer (`NO_REPLY` runs).
pub fn is_silent(text: &str) -> bool {
    matches!(text.trim(), "NO_REPLY" | "No request." | "")
}

/// A provider rate-limit wait travels as a Compaction row (`agent::view`); it is not one.
pub fn is_rate_wait(e: &AgentLiveEntry) -> bool {
    e.id == "provider-rate-wait"
}

/// Tools whose failure is a failed patch (mirrors `agent::view::is_edit_tool`).
pub fn is_edit_tool(name: &str) -> bool {
    let name = name.rsplit("__").next().unwrap_or(name).to_ascii_lowercase();
    matches!(name.as_str(), "edit" | "write" | "multiedit" | "multi_edit" | "apply_patch" | "patch" | "str_replace" | "notebookedit")
}

/// A failure row that is an error (the user's Stop is not).
fn is_error(e: &AgentLiveEntry) -> bool {
    e.kind == AgentLiveKind::Failure && failure_copy(&e.text).0 != "Stopped"
}

/// What started a turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trigger {
    /// A voice wake (a Heimdall delivery).
    Wake,
    /// The user typed it in this window.
    Typed,
    /// Context added without waking the agent (or a dry-run prompt).
    Context,
    /// Rows before the first prompt the window still has.
    Earlier,
}

/// How a turn ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Running,
    /// It spoke (the speak tool), whatever else it wrote.
    Spoke,
    Replied,
    /// It ran and chose not to answer (NO_REPLY).
    NoReply,
    Failed,
    Stopped,
    /// Context only: nothing was asked.
    Context,
}

/// One request to the agent and everything up to the next.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Turn {
    /// Row range in `AgentView::rows`: `start` is the prompt (unless `Earlier`).
    pub start: usize,
    pub end: usize,
    pub trigger: Trigger,
    pub outcome: Outcome,
    /// Prompt to its last row; `None` while running or with nothing after the prompt.
    pub duration_ms: Option<u64>,
    /// The first request's time to first token, when it was seen live.
    pub first_token_ms: Option<u64>,
    pub tools: u32,
    pub tool_failures: u32,
    pub failed_edits: u32,
    pub output_tokens: u64,
}

/// The turns over all rows; `running`: the agent is mid-turn now (the last turn is still going).
pub fn turns(rows: &[Row], ttft: &HashMap<u64, u64>, running: bool) -> Vec<Turn> {
    let mut starts: Vec<usize> = rows.iter().enumerate().filter(|(_, r)| is_prompt(r)).map(|(i, _)| i).collect();
    if rows.first().is_some_and(|r| !is_prompt(r)) {
        starts.insert(0, 0);
    }
    let n = starts.len();
    starts
        .iter()
        .enumerate()
        .map(|(k, &start)| {
            let end = starts.get(k + 1).copied().unwrap_or(rows.len());
            turn(rows, start, end, ttft, running && k + 1 == n)
        })
        .collect()
}

/// The turn containing row `i`: its prompt's index (or 0 before the first prompt).
pub fn turn_start(rows: &[Row], i: usize) -> usize {
    rows[..=i.min(rows.len().saturating_sub(1))].iter().rposition(is_prompt).unwrap_or(0)
}

/// The turn starting at `start` (a prompt row, or 0).
pub fn turn_at(rows: &[Row], start: usize, ttft: &HashMap<u64, u64>, running: bool) -> Turn {
    let end = rows.iter().enumerate().skip(start + 1).find(|(_, r)| is_prompt(r)).map_or(rows.len(), |(i, _)| i);
    turn(rows, start, end, ttft, running && end == rows.len())
}

fn is_prompt(r: &Row) -> bool {
    r.live().is_some_and(|e| e.kind == AgentLiveKind::Prompt)
}

fn turn(rows: &[Row], start: usize, end: usize, ttft: &HashMap<u64, u64>, running: bool) -> Turn {
    let trigger = match rows.get(start).and_then(Row::live) {
        Some(e) if e.kind == AgentLiveKind::Prompt => {
            if typed(e).is_some() {
                Trigger::Typed
            } else if e.text.starts_with("[Heimdall delivery: ") && e.sent {
                Trigger::Wake
            } else {
                Trigger::Context
            }
        }
        _ => Trigger::Earlier,
    };
    let body: Vec<&AgentLiveEntry> = rows[(start + 1).min(end)..end].iter().filter_map(Row::live).collect();
    let mut t = Turn { start, end, trigger, outcome: Outcome::Context, duration_ms: None, first_token_ms: None, tools: 0, tool_failures: 0, failed_edits: 0, output_tokens: 0 };
    let (mut spoke, mut replied, mut failed, mut stopped, mut acted) = (false, false, false, false, false);
    for e in &body {
        match e.kind {
            AgentLiveKind::ToolCall => {
                t.tools += 1;
                acted = true;
                if e.tool_name.as_deref().is_some_and(|n| n.ends_with("speak")) {
                    spoke = true;
                }
            }
            AgentLiveKind::ToolResult if e.error && e.finished => {
                t.tool_failures += 1;
                if e.tool_name.as_deref().is_some_and(is_edit_tool) {
                    t.failed_edits += 1;
                }
            }
            AgentLiveKind::Reply => {
                acted = true;
                if !is_silent(&e.text) {
                    replied = true;
                }
            }
            AgentLiveKind::Thinking => acted = true,
            AgentLiveKind::Failure if is_error(e) => failed = true,
            AgentLiveKind::Failure => stopped = true,
            _ => {}
        }
        if let Some(u) = &e.usage {
            t.output_tokens += u.output;
            if t.first_token_ms.is_none() {
                t.first_token_ms = first_token(u, ttft);
            }
        }
    }
    t.outcome = if failed {
        Outcome::Failed
    } else if stopped {
        Outcome::Stopped
    } else if running && trigger != Trigger::Context {
        Outcome::Running
    } else if spoke {
        Outcome::Spoke
    } else if replied {
        Outcome::Replied
    } else if acted || matches!(trigger, Trigger::Wake | Trigger::Typed) {
        Outcome::NoReply
    } else {
        Outcome::Context
    };
    let first = rows.get(start).map(Row::time_ms).unwrap_or(0);
    let last = body.iter().map(|e| e.timestamp_ms).max();
    if t.outcome != Outcome::Running && trigger != Trigger::Earlier {
        t.duration_ms = last.filter(|&l| first > 0 && l >= first).map(|l| l - first);
    }
    t
}

/// A request's time to first token: seen live, or remembered from the live copy that history replaced.
pub fn first_token(u: &AgentUsage, ttft: &HashMap<u64, u64>) -> Option<u64> {
    match u.first_token_ms {
        Some(f) if u.started_ms > 0 && f >= u.started_ms => Some(f - u.started_ms),
        _ => ttft.get(&u.started_ms).copied(),
    }
}

/// The telemetry strip's numbers.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Telemetry {
    pub since_ms: u64,
    pub requests: u64,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    /// The last request's prompt size.
    pub context_used: Option<u64>,
    pub window: Option<u64>,
    pub compactions: u64,
    pub last_compaction_ms: Option<u64>,
    pub tool_calls: u64,
    pub tool_failures: u64,
    pub failed_edits: u64,
    pub errors: u64,
    pub last_error_ms: Option<u64>,
    /// Median over the requests and turns the window holds.
    pub first_token_median_ms: Option<u64>,
    pub turn_median_ms: Option<u64>,
    pub turn_last_ms: Option<u64>,
}

/// The session's totals plus every row newer than them; latency from the rows the window holds.
pub fn telemetry(rows: &[Row], base: Option<&AgentSessionStats>, ttft: &HashMap<u64, u64>, running: bool) -> Telemetry {
    let mut t = Telemetry::default();
    let through = base.map_or(0, |b| b.through_ms);
    if let Some(b) = base {
        t = Telemetry {
            since_ms: b.since_ms,
            requests: b.requests,
            input: b.input,
            output: b.output,
            cache_read: b.cache_read,
            cache_write: b.cache_write,
            context_used: (b.requests > 0).then_some(b.last_prompt),
            window: b.window_tokens,
            compactions: b.compactions,
            last_compaction_ms: b.last_compaction_ms,
            tool_calls: b.tool_calls,
            tool_failures: b.tool_failures,
            failed_edits: b.failed_edits,
            errors: b.errors,
            last_error_ms: b.last_error_ms,
            ..Default::default()
        };
    }
    let mut first_tokens = vec![];
    for row in rows {
        if let Some(u) = row.live().and_then(|e| e.usage.as_ref()) {
            first_tokens.extend(first_token(u, ttft));
        }
        if row.time_ms() <= through {
            continue;
        }
        if t.since_ms == 0 {
            t.since_ms = row.time_ms();
        }
        let e = match row {
            Row::Notice { kind: NoticeKind::Error, time_ms, .. } => {
                t.errors += 1;
                t.last_error_ms = Some(*time_ms);
                continue;
            }
            Row::Notice { .. } => continue,
            Row::Live(e) => e,
        };
        match e.kind {
            AgentLiveKind::ToolCall => t.tool_calls += 1,
            AgentLiveKind::ToolResult if e.error && e.finished => {
                t.tool_failures += 1;
                if e.tool_name.as_deref().is_some_and(is_edit_tool) {
                    t.failed_edits += 1;
                }
            }
            AgentLiveKind::Compaction if e.finished && !is_rate_wait(e) => {
                t.compactions += 1;
                t.last_compaction_ms = Some(e.timestamp_ms);
            }
            AgentLiveKind::Failure if is_error(e) => {
                t.errors += 1;
                t.last_error_ms = Some(e.timestamp_ms);
            }
            _ => {}
        }
        if let Some(u) = &e.usage {
            t.requests += 1;
            t.input += u.input;
            t.output += u.output;
            t.cache_read += u.cache_read;
            t.cache_write += u.cache_write;
            t.context_used = Some(u.input + u.cache_read + u.cache_write);
        }
    }
    let durations: Vec<u64> = turns(rows, ttft, running)
        .iter()
        .filter(|t| matches!(t.trigger, Trigger::Wake | Trigger::Typed))
        .filter_map(|t| t.duration_ms)
        .collect();
    t.turn_last_ms = durations.last().copied();
    t.turn_median_ms = median(durations);
    t.first_token_median_ms = median(first_tokens);
    t
}

fn median(mut v: Vec<u64>) -> Option<u64> {
    if v.is_empty() {
        return None;
    }
    v.sort_unstable();
    Some(v[v.len() / 2])
}

/// One cell of the telemetry strip: a value over its caption; `fill` draws a meter under the value.
#[derive(Clone, Debug, PartialEq)]
pub struct Tile {
    pub value: String,
    pub caption: String,
    /// The value's colour; the caption is secondary unless `caption_tone` warns.
    pub tone: Tone,
    pub caption_tone: Tone,
    pub fill: Option<f64>,
}

/// The strip: context, tokens, tools, latency, compactions, errors. `clock` formats a time ("14:05").
pub fn tiles(t: &Telemetry, clock: &dyn Fn(u64) -> String) -> Vec<Tile> {
    let tile = |value: String, caption: String, tone| Tile { value, caption, tone, caption_tone: Tone::Secondary, fill: None };
    let mut out = vec![];
    // Context fill.
    let mut context = match (t.context_used, t.window) {
        (Some(used), Some(window)) if window > 0 => {
            let fill = used as f64 / window as f64;
            let tone = if fill >= 0.85 { Tone::Orange } else { Tone::Primary };
            Tile { value: tokens(used), caption: format!("context · {}% of {}", (fill * 100.0).round() as u64, tokens(window)), tone, caption_tone: Tone::Secondary, fill: Some(fill.min(1.0)) }
        }
        (Some(used), _) => tile(tokens(used), "context".into(), Tone::Primary),
        (None, _) => tile("—".into(), "context".into(), Tone::Tertiary),
    };
    if t.context_used.is_some() && t.window.is_none() {
        context.caption = "context · last request".into();
    }
    out.push(context);
    out.push(if t.requests == 0 {
        tile("—".into(), "tokens in · cached · out".into(), Tone::Tertiary)
    } else {
        tile(
            format!("{} · {} · {}", tokens(t.input + t.cache_write), tokens(t.cache_read), tokens(t.output)),
            "tokens in · cached · out".into(),
            Tone::Primary,
        )
    });
    let failed = match (t.tool_failures, t.failed_edits) {
        (0, _) => "tool calls · none failed".to_string(),
        (f, 0) => format!("tool calls · {f} failed"),
        (f, e) => format!("tool calls · {f} failed · {e} {}", if e == 1 { "edit" } else { "edits" }),
    };
    // A failed edit warns in the caption; the count itself stays calm.
    out.push(Tile {
        value: count(t.tool_calls),
        caption: failed,
        tone: if t.tool_calls == 0 { Tone::Tertiary } else { Tone::Primary },
        caption_tone: if t.failed_edits > 0 { Tone::Orange } else { Tone::Secondary },
        fill: None,
    });
    out.push(match (t.first_token_median_ms, t.turn_median_ms) {
        (None, None) => tile("—".into(), "first token · turn".into(), Tone::Tertiary),
        (f, d) => tile(
            format!("{} · {}", f.map_or("—".into(), duration), d.map_or("—".into(), duration)),
            "first token · turn, median".into(),
            Tone::Primary,
        ),
    });
    out.push(tile(
        count(t.compactions),
        match t.last_compaction_ms {
            Some(ms) if t.compactions > 0 => format!("compactions · last {}", clock(ms)),
            _ => "compactions".into(),
        },
        if t.compactions == 0 { Tone::Tertiary } else { Tone::Primary },
    ));
    out.push(tile(
        count(t.errors),
        match t.last_error_ms {
            Some(ms) if t.errors > 0 => format!("errors · last {}", clock(ms)),
            _ => "errors".into(),
        },
        if t.errors == 0 { Tone::Tertiary } else { Tone::Orange },
    ));
    out
}

/// A turn's heading after its time: what started it, how it ended (coloured) and the facts ("6.2 s · 3 tools").
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Heading {
    pub what: &'static str,
    pub outcome: &'static str,
    pub outcome_tone: Tone,
    pub facts: String,
}

pub fn turn_heading(t: &Turn) -> Heading {
    let what = match t.trigger {
        Trigger::Wake => "Wake",
        Trigger::Typed => "User",
        Trigger::Context => "Context",
        Trigger::Earlier => "Earlier",
    };
    let (outcome, tone) = match t.outcome {
        Outcome::Running => ("Running", Tone::Blue),
        Outcome::Spoke => ("Spoke", Tone::Green),
        Outcome::Replied => ("Replied", Tone::Green),
        Outcome::NoReply => ("No reply", Tone::Secondary),
        Outcome::Failed => ("Failed", Tone::Red),
        Outcome::Stopped => ("Stopped", Tone::Orange),
        Outcome::Context => ("", Tone::Secondary),
    };
    let mut facts: Vec<String> = vec![];
    if let Some(d) = t.duration_ms {
        facts.push(duration(d));
    }
    if let Some(f) = t.first_token_ms {
        facts.push(format!("first token {}", duration(f)));
    }
    if t.tools > 0 {
        facts.push(format!("{} {}", t.tools, if t.tools == 1 { "tool" } else { "tools" }));
    }
    if t.tool_failures > 0 {
        facts.push(format!("{} failed", t.tool_failures));
    }
    if t.output_tokens > 0 {
        facts.push(format!("{} out", tokens(t.output_tokens)));
    }
    let tone = if t.tool_failures > 0 && tone == Tone::Green { Tone::Orange } else { tone };
    Heading { what, outcome, outcome_tone: tone, facts: facts.join(" · ") }
}

/// Long bodies cut to `max` lines, saying how many more (display only; the archive keeps everything).
pub fn clamp_lines(text: &str, max: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= max {
        return text.to_string();
    }
    format!("{}\n… {} more lines", lines[..max].join("\n"), lines.len() - max)
}

/// The most lines a row's body shows in the timeline, by kind.
pub fn max_lines(row: &Row) -> usize {
    match row.live().map(|e| &e.kind) {
        Some(AgentLiveKind::Prompt) => 8,
        Some(AgentLiveKind::Thinking) => 6,
        Some(AgentLiveKind::ToolCall) => 8,
        Some(AgentLiveKind::ToolResult) if row.live().is_some_and(|e| e.error) => 24,
        Some(AgentLiveKind::ToolResult) => 10,
        Some(AgentLiveKind::Compaction) => 4,
        _ => 200,
    }
}

/// "950", "12.3K", "1.2M", "38M".
pub fn tokens(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=99_999 => trim(format!("{:.1}K", n as f64 / 1e3)),
        100_000..=999_999 => format!("{}K", (n as f64 / 1e3).round() as u64),
        1_000_000..=99_999_999 => trim(format!("{:.1}M", n as f64 / 1e6)),
        _ => format!("{}M", (n as f64 / 1e6).round() as u64),
    }
}

fn trim(s: String) -> String {
    s.replace(".0K", "K").replace(".0M", "M")
}

/// "4,812".
pub fn count(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// "640 ms", "6.2 s", "2 m 05 s".
pub fn duration(ms: u64) -> String {
    match ms {
        0..=999 => format!("{ms} ms"),
        1_000..=59_999 => format!("{:.1} s", ms as f64 / 1000.0),
        _ => format!("{} m {:02} s", ms / 60_000, (ms % 60_000) / 1000),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(id: &str, s: u64, kind: AgentLiveKind, text: &str) -> AgentLiveEntry {
        AgentLiveEntry {
            id: id.into(),
            session_entry_id: None,
            timestamp_ms: 1_000_000 + s * 1000,
            kind,
            text: text.into(),
            tool_name: None,
            attachments: vec![],
            finished: true,
            editable: false,
            error: false,
            sent: true,
            queued: false,
            owner_text: None,
            usage: None,
        }
    }
    fn tool(id: &str, s: u64, kind: AgentLiveKind, name: &str, error: bool) -> AgentLiveEntry {
        let mut x = e(id, s, kind, "{}");
        x.tool_name = Some(name.into());
        x.error = error;
        x
    }
    fn used(mut x: AgentLiveEntry, input: u64, cached: u64, output: u64, started_s: u64, first_token_s: Option<u64>) -> AgentLiveEntry {
        x.usage = Some(AgentUsage {
            input,
            output,
            cache_read: cached,
            cache_write: 0,
            started_ms: 1_000_000 + started_s * 1000,
            first_token_ms: first_token_s.map(|f| 1_000_000 + f * 1000),
        });
        x
    }
    fn wake(id: &str, s: u64) -> AgentLiveEntry {
        e(id, s, AgentLiveKind::Prompt, &format!("[Heimdall delivery: {id}]\nTranscript:\n[Mic · User] Heimdall, check the build"))
    }

    /// A busy stretch shaped like the live session (22 Oct): a voice wake that edits a file (the edit fails, the
    /// retry works) and speaks; a NO_REPLY wake; context; a typed request that fails; a compaction; and the rate
    /// wait that travels as a Compaction row.
    fn fixture() -> Vec<Row> {
        let mut typed = e("m1", 60, AgentLiveKind::Prompt, "[Heimdall delivery: m1]\n[Heimdall typed message]\nrun the tests");
        typed.owner_text = Some("run the tests".into());
        let mut rate = e("provider-rate-wait", 62, AgentLiveKind::Compaction, "Anthropic rate limit · retrying in 5 seconds");
        rate.finished = true;
        [
            wake("d1", 0),
            used(e("t1", 3, AgentLiveKind::Thinking, "look"), 2, 100_000, 50, 1, Some(2)),
            used(tool("c1-call", 4, AgentLiveKind::ToolCall, "edit", false), 3, 101_000, 80, 3, Some(4)),
            tool("c1-result", 4, AgentLiveKind::ToolResult, "edit", true),
            used(tool("c2-call", 6, AgentLiveKind::ToolCall, "edit", false), 3, 102_000, 90, 5, None),
            tool("c2-result", 6, AgentLiveKind::ToolResult, "edit", false),
            used(tool("c3-call", 8, AgentLiveKind::ToolCall, "speak", false), 3, 103_000, 30, 7, None),
            tool("c3-result", 8, AgentLiveKind::ToolResult, "speak", false),
            wake("d2", 20),
            used(e("r2", 23, AgentLiveKind::Reply, "NO_REPLY"), 4, 104_000, 5, 21, Some(22)),
            e("x1", 30, AgentLiveKind::Prompt, "Captured observation (data only):\n[Discord · Dave] hi"),
            typed,
            rate,
            used(e("f1", 64, AgentLiveKind::Failure, "overloaded_error: 529"), 1, 0, 0, 61, None),
            e("k1", 70, AgentLiveKind::Compaction, "Summary of the session so far"),
        ]
        .into_iter()
        .map(Row::Live)
        .collect()
    }

    #[test]
    fn turns_group_each_request_with_what_happened() {
        let rows = fixture();
        let t = turns(&rows, &HashMap::new(), false);
        let shape: Vec<(usize, usize, Trigger, Outcome)> = t.iter().map(|t| (t.start, t.end, t.trigger, t.outcome)).collect();
        assert_eq!(
            shape,
            vec![
                (0, 8, Trigger::Wake, Outcome::Spoke),
                (8, 10, Trigger::Wake, Outcome::NoReply),
                (10, 11, Trigger::Context, Outcome::Context),
                (11, 15, Trigger::Typed, Outcome::Failed),
            ]
        );
        assert_eq!((t[0].tools, t[0].tool_failures, t[0].failed_edits, t[0].output_tokens), (3, 1, 1, 250));
        assert_eq!(t[0].duration_ms, Some(8_000));
        assert_eq!(t[0].first_token_ms, Some(1_000));
        assert_eq!(turn_start(&rows, 5), 0);
        assert_eq!(turn_start(&rows, 9), 8);
        assert_eq!(turn_at(&rows, 8, &HashMap::new(), false), t[1]);
        let h = turn_heading(&t[0]);
        assert_eq!((h.what, h.outcome), ("Wake", "Spoke"));
        assert_eq!(h.facts, "8.0 s · first token 1.0 s · 3 tools · 1 failed · 250 out");
        assert_eq!(h.outcome_tone, Tone::Orange, "a turn with a failed tool is not plain green");
        assert_eq!(turn_heading(&t[3]).outcome_tone, Tone::Red);
        // While the agent works, the last request is running and has no duration yet.
        let running = turns(&rows[..10], &HashMap::new(), true);
        assert_eq!((running[1].outcome, running[1].duration_ms), (Outcome::Running, None));
        // Rows before the first prompt the window still has form an "Earlier" group.
        let tail = turns(&rows[2..], &HashMap::new(), false);
        assert_eq!((tail[0].start, tail[0].end, tail[0].trigger), (0, 6, Trigger::Earlier));
    }

    #[test]
    fn telemetry_adds_rows_newer_than_the_session_totals() {
        let rows = fixture();
        let alone = telemetry(&rows, None, &HashMap::new(), false);
        assert_eq!((alone.requests, alone.input, alone.cache_read, alone.output), (6, 16, 510_000, 255));
        assert_eq!(alone.context_used, Some(1), "the last request's prompt");
        assert_eq!((alone.tool_calls, alone.tool_failures, alone.failed_edits), (3, 1, 1));
        assert_eq!((alone.compactions, alone.last_compaction_ms), (1, Some(1_070_000)), "the rate wait is not a compaction");
        assert_eq!((alone.errors, alone.last_error_ms), (1, Some(1_064_000)));
        // First token: 1.0 s, 1.0 s (live) and 1.0 s (d2): median 1 s. Turns: 8 s, 3 s, 10 s (typed, to its compaction).
        assert_eq!(alone.first_token_median_ms, Some(1_000));
        assert_eq!((alone.turn_median_ms, alone.turn_last_ms), (Some(8_000), Some(10_000)));
        // With the session's totals through the second wake, only later rows add.
        let base = AgentSessionStats {
            since_ms: 5, through_ms: 1_023_000, requests: 100, input: 1_000, output: 2_000, cache_read: 9_000_000,
            cache_write: 10, last_prompt: 104_004, compactions: 4, last_compaction_ms: Some(3), tool_calls: 50,
            tool_failures: 2, failed_edits: 1, errors: 0, last_error_ms: None, window_tokens: Some(200_000),
        };
        let t = telemetry(&rows, Some(&base), &HashMap::new(), false);
        assert_eq!((t.since_ms, t.requests, t.input, t.output), (5, 101, 1_001, 2_000));
        assert_eq!((t.tool_calls, t.compactions, t.errors, t.failed_edits), (50, 5, 1, 1));
        assert_eq!((t.context_used, t.window), (Some(1), Some(200_000)));
        // No row after the totals: the context is the session's last request.
        let t = telemetry(&rows[..10], Some(&base), &HashMap::new(), false);
        assert_eq!(t.context_used, Some(104_004));
    }

    /// History replaces a live row (and its first-token time) once the turn settles; the time stays remembered by
    /// the request's start.
    #[test]
    fn first_token_survives_history_settling() {
        let settled = used(e("r", 5, AgentLiveKind::Reply, "Done"), 1, 2, 3, 1, None);
        let rows = vec![Row::Live(wake("d", 0)), Row::Live(settled)];
        assert_eq!(telemetry(&rows, None, &HashMap::new(), false).first_token_median_ms, None);
        let ttft = HashMap::from([(1_001_000, 640)]);
        assert_eq!(telemetry(&rows, None, &ttft, false).first_token_median_ms, Some(640));
        assert_eq!(turns(&rows, &ttft, false)[0].first_token_ms, Some(640));
    }

    #[test]
    fn tiles_read_in_words_and_units() {
        let t = Telemetry {
            requests: 4_812, input: 3_400, cache_write: 900_000, cache_read: 412_000_000, output: 1_234_567,
            context_used: Some(178_992), window: Some(200_000), compactions: 3, last_compaction_ms: Some(7),
            tool_calls: 1_312, tool_failures: 9, failed_edits: 2, errors: 1, last_error_ms: Some(8),
            first_token_median_ms: Some(1_840), turn_median_ms: Some(6_210), ..Default::default()
        };
        let clock = |ms: u64| format!("at{ms}");
        let tiles = tiles(&t, &clock);
        let text: Vec<(String, String)> = tiles.iter().map(|t| (t.value.clone(), t.caption.clone())).collect();
        assert_eq!(
            text,
            vec![
                ("179K".into(), "context · 89% of 200K".into()),
                ("903K · 412M · 1.2M".into(), "tokens in · cached · out".into()),
                ("1,312".into(), "tool calls · 9 failed · 2 edits".into()),
                ("1.8 s · 6.2 s".into(), "first token · turn, median".into()),
                ("3".into(), "compactions · last at7".into()),
                ("1".into(), "errors · last at8".into()),
            ]
        );
        assert_eq!(tiles[0].tone, Tone::Orange, "near-full context warns");
        assert!((tiles[0].fill.unwrap() - 0.89496).abs() < 1e-3);
        assert_eq!((tiles[2].tone, tiles[2].caption_tone), (Tone::Primary, Tone::Orange), "a failed edit warns in words");
        // An empty session: dashes and quiet captions, no meter.
        let empty = super::tiles(&Telemetry::default(), &clock);
        assert!(empty.iter().all(|t| t.tone == Tone::Tertiary && t.fill.is_none()));
        assert_eq!(empty[0].value, "—");
    }

    /// A resolved rate wait read "History compacted" in the window and would have counted as a compaction.
    #[test]
    fn a_rate_wait_is_not_a_compaction() {
        let wait = e("provider-rate-wait", 1, AgentLiveKind::Compaction, "Anthropic request resumed");
        let shown = crate::agent_view::display(&Row::Live(wait.clone())).unwrap();
        assert_eq!((shown.heading.as_str(), shown.body.as_str()), ("Anthropic request resumed", ""));
        assert_eq!(telemetry(&[Row::Live(wait)], None, &HashMap::new(), false).compactions, 0);
    }

    #[test]
    fn units_and_clamps() {
        assert_eq!([tokens(950), tokens(12_300), tokens(12_000), tokens(178_992), tokens(1_234_567), tokens(38_000_000), tokens(412_000_000)],
            ["950", "12.3K", "12K", "179K", "1.2M", "38M", "412M"]);
        assert_eq!([duration(640), duration(6_210), duration(125_000)], ["640 ms", "6.2 s", "2 m 05 s"]);
        assert_eq!(count(1_234_567), "1,234,567");
        assert_eq!(clamp_lines("a\nb\nc\nd", 2), "a\nb\n… 2 more lines");
        assert_eq!(clamp_lines("a\nb", 2), "a\nb");
        assert!(is_silent(" NO_REPLY\n") && !is_silent("Done."));
    }
}
