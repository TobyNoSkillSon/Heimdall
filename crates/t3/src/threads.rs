//! The thread tools the agent calls directly, under T3's own names (`t3_thread_list`, `t3_thread_read`), and the
//! compact form of T3 results that pass through `t3_call`. "spawning and managing T3 code threads, it
//! should take you like maximum one tool call … responses from them should be easy". A read defaults to the latest
//! messages in full; runs and thread settings only with `details`.
use super::workers::Workers;
use crate::text;
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};

/// A read's result budget (`orchestrator::RESULT_BYTES`): whole messages up to it, newest first.
const BUDGET: usize = crate::orchestrator::RESULT_BYTES;
/// Characters per T3 read page.
const PAGE_CHARS: u64 = 4000;
/// Characters of an activity item's first line in `view: activity`.
const ACTIVITY_CHARS: usize = 160;
pub(crate) const ACTIVE: [&str; 5] = ["preparing", "queued", "starting", "running", "waiting"];

/// The questions and approvals T3 counts on a thread (a list row or read shell) that can still wait on the user. A question
/// or an approval holds a live run; once the run has ended, `pendingRequestCount` is a leftover: an approval Heimdall's
/// permission hook answered stays "pending" in T3 until the provider session closes, sometimes hours later (bug notes
/// 2026-11-02T14-56-51, 16-05-08, 16-19-09). So a finished thread never reads as waiting.
pub fn live_requests(t: &Value) -> u64 {
    if ACTIVE.contains(&t["status"].as_str().unwrap_or("")) { t["pendingRequestCount"].as_u64().unwrap_or(0) } else { 0 }
}
/// The run a thread is on and its state, from its read shell `t` and its `recentRuns`. T3's thread status is its
/// latest run's, so a run T3 queued and cancelled before it started (its own hand-over turn for a sub-agent that
/// finished while a turn ran) made the thread read "cancelled" while that turn still worked, and after it (bug note
/// 2026-11-04T22-25-15: the setup-wizard task, run 7 cancelled 8 s after it was queued, run 6 running on for two
/// minutes). A run that never started is no state of the thread: the active run decides while one runs, else the
/// newest run that started (when `runs` reaches it). A thread whose only run never started keeps T3's state.
pub fn current_run(t: &Value, runs: &[Value]) -> (Option<String>, String) {
    followed_run(t, runs, None, &|_| false)
}
/// `current_run` for a thread Heimdall follows (review 5 Nov). `recorded`: the run Heimdall followed last and the
/// state it recorded for it; `sent`: whether Heimdall's own message started a run.
/// - A run Heimdall's own message started that T3 cancelled before it started (newer than every run that started)
///   is the thread's state, "cancelled": his follow-up never ran, so the thread does not read "completed" with the
///   earlier run's result.
/// - A never-started run recorded earlier (a result recorded against it before this rule) that is newer than the run
///   that started, now finished, stays the followed run with its recorded state: runs are compared by their order,
///   not their names, so the earlier run never looks newer, nothing is reported twice and the reported task can still
///   be archived.
pub fn followed_run(t: &Value, runs: &[Value], recorded: Option<(&str, &str)>, sent: &dyn Fn(&str) -> bool) -> (Option<String>, String) {
    let status = t["status"].as_str().unwrap_or("idle").to_owned();
    let terminal = |s: &str| matches!(s, "completed" | "failed" | "interrupted" | "cancelled" | "rolled_back");
    let run_status = |id: &str| runs.iter().find(|r| r["runId"] == id).and_then(|r| r["status"].as_str()).map(str::to_owned);
    if let Some(active) = t["activeRunId"].as_str() {
        let state = if terminal(&status) { run_status(active).filter(|s| !terminal(s)).unwrap_or_else(|| "running".into()) } else { status };
        return (Some(active.to_owned()), state);
    }
    let latest = t["latestRunId"].as_str().map(str::to_owned);
    let dropped = |r: &Value| r["status"] == "cancelled" && r["startedAt"].is_null();
    let ordinal = |r: &Value| r["ordinal"].as_i64().unwrap_or(0);
    let mut earlier: Vec<&Value> = runs.iter().filter(|r| !dropped(r) && r["startedAt"].is_string()).collect();
    earlier.sort_by_key(|r| std::cmp::Reverse(ordinal(r)));
    let newest_started = earlier.first().map(|r| ordinal(r));
    let after_started = |r: &&Value| newest_started.is_none_or(|n| ordinal(r) > n);
    // Heimdall's own message, dropped before it ran.
    let mut mine: Vec<&Value> = runs.iter().filter(|r| dropped(r) && r["runId"].as_str().is_some_and(sent)).filter(after_started).collect();
    mine.sort_by_key(|r| std::cmp::Reverse(ordinal(r)));
    if let Some(r) = mine.first() {
        return (r["runId"].as_str().map(str::to_owned), "cancelled".into());
    }
    if latest.as_deref().and_then(|l| runs.iter().find(|r| r["runId"] == l)).is_some_and(dropped) {
        if let Some(r) = earlier.first() {
            let state = r["status"].as_str().unwrap_or("completed").to_owned();
            // The dropped run Heimdall recorded, still newer than the run that started and that run over: kept.
            let kept = recorded.filter(|_| terminal(&state))
                .filter(|(id, _)| runs.iter().find(|x| x["runId"] == *id).is_some_and(|x| dropped(x) && after_started(&x)));
            if let Some((id, recorded_state)) = kept {
                return (Some(id.to_owned()), recorded_state.to_owned());
            }
            return (r["runId"].as_str().map(str::to_owned), state);
        }
    }
    (latest, status)
}
/// Whether a thread reads as waiting on a question or approval: T3 says "waiting", or counts a request while its run is
/// live. The observer's wake, the labels and the islands use this one rule (bug note 2026-11-01T00-15-42).
pub fn asks(t: &Value) -> bool {
    t["status"] == "waiting" || live_requests(t) > 0
}

/// The last items of one view of a thread, oldest first, with the thread's state and recent runs.
pub(crate) struct Latest {
    pub thread: Value,
    pub runs: Value,
    pub items: Vec<Value>,
}

impl Workers {
    /// The last `n` items of a thread's `view`. T3 pages forward from a position, so this reads a window before the
    /// end (from the thread's item count) and widens it until it holds `n` items or reaches the start.
    pub(crate) async fn latest(&self, thread: &str, view: &str, n: usize, chars: u64) -> Result<Latest> {
        let head = self
            .client
            .call("t3_thread_read", json!({"threadId":thread,"view":view,"limit":1,"maxCharsPerItem":1,"runLimit":5}))
            .await?;
        let count = head["thread"]["itemCount"].as_u64().unwrap_or(0);
        let mut window = n as u64 * 15 + 30;
        loop {
            let start = count.saturating_sub(window);
            let mut args = json!({"threadId":thread,"view":view,"limit":100,"maxCharsPerItem":chars,"runLimit":1});
            if start > 0 {
                args["afterPosition"] = json!(start);
            }
            let mut items: Vec<Value> = vec![];
            let mut state = head["thread"].clone();
            for _ in 0..50 {
                let page = self.client.call("t3_thread_read", args.clone()).await?;
                state = page["thread"].clone();
                let got = page["items"].as_array().cloned().unwrap_or_default();
                let empty = got.is_empty();
                items.extend(got);
                let next = page["nextPosition"].as_u64();
                if page["hasMore"] != true || empty || next.is_none() {
                    break;
                }
                ensure!(args["afterPosition"].as_u64().is_none_or(|a| next.unwrap() > a), "T3 read cursor made no progress");
                args["afterPosition"] = json!(next);
            }
            if items.len() >= n || start == 0 {
                let skip = items.len().saturating_sub(n);
                return Ok(Latest { thread: state, runs: head["recentRuns"].clone(), items: items.split_off(skip) });
            }
            window *= 4;
        }
    }

    /// `t3_thread_list`: every project's threads, active ones first, then the most recently updated; one line each.
    pub(super) async fn thread_list(&self, a: &Value) -> Result<String> {
        let limit = a["limit"].as_u64().unwrap_or(10).clamp(1, 50) as usize;
        let query = a["query"].as_str();
        let mut projects = vec![];
        let mut cursor = Value::Null;
        for _ in 0..10 {
            let mut args = json!({"limit":100});
            if !cursor.is_null() {
                args["cursor"] = cursor.clone();
            }
            let page = self.client.call("t3_project_list", args).await?;
            for p in page["projects"].as_array().context("T3 projects missing")? {
                if let Some(id) = p["id"].as_str() {
                    projects.push((id.to_owned(), p["title"].as_str().unwrap_or("").to_owned()));
                }
            }
            cursor = page["nextCursor"].clone();
            if cursor.is_null() {
                break;
            }
        }
        let mut rows: Vec<(Value, String)> = vec![];
        for (project, name) in &projects {
            let mut active = json!({"projectId":project,"statuses":ACTIVE,"limit":50,"includeSubagents":false});
            let mut recent = json!({"projectId":project,"limit":limit,"includeSubagents":false});
            if let Some(q) = query {
                active["titleContains"] = json!(q);
                recent["titleContains"] = json!(q);
            }
            for args in [active, recent] {
                let page = self.client.call("t3_thread_list", args).await?;
                for t in page["threads"].as_array().into_iter().flatten() {
                    let id = t["threadId"].as_str().unwrap_or("");
                    let child = t["relationshipToParent"] == "subagent" || t["archived"] == true;
                    if !id.is_empty() && !child && !rows.iter().any(|(r, _)| r["threadId"] == id) {
                        rows.push((t.clone(), name.clone()));
                    }
                }
            }
        }
        let is_active = |t: &Value| ACTIVE.contains(&t["status"].as_str().unwrap_or(""));
        rows.sort_by(|(x, _), (y, _)| {
            is_active(y)
                .cmp(&is_active(x))
                .then_with(|| y["updatedAt"].as_str().unwrap_or("").cmp(x["updatedAt"].as_str().unwrap_or("")))
        });
        let total = rows.len();
        rows.truncate(limit);
        if rows.is_empty() {
            return Ok(match query {
                Some(q) => format!("No threads with {q:?} in the title."),
                None => "No threads.".into(),
            });
        }
        let several = rows.iter().any(|(_, p)| *p != rows[0].1);
        let now = crate::now_ms();
        let mut lines = vec![];
        for (t, project) in &rows {
            let id = t["threadId"].as_str().unwrap_or("");
            let mut line = list_line(t, now);
            if self.owns_task(id)? {
                line.push_str(" · your task");
            }
            if several && !project.is_empty() {
                line.push_str(&format!(" · {project}"));
            }
            lines.push(format!("{line} [thread {id}]"));
        }
        let mut out = format!("{} thread{}, active first:\n{}", rows.len(), if rows.len() == 1 { "" } else { "s" }, lines.join("\n"));
        if total > rows.len() {
            out.push_str(&format!("\n{} more; raise limit or filter with query.", total - rows.len()));
        }
        Ok(out)
    }

    /// `t3_thread_read`.
    pub(super) async fn thread_read(&self, a: &Value) -> Result<String> {
        let view = a["view"].as_str().unwrap_or("messages");
        let details = a["details"] == true;
        ensure!(a.get("threadId").is_some() != a.get("threadIds").is_some(), "INVALID_ARGUMENT: pass threadId or threadIds");
        if let Some(ids) = a["threadIds"].as_array() {
            let ids: Vec<&str> = ids.iter().filter_map(Value::as_str).collect();
            let share = BUDGET / ids.len().max(1);
            let mut sections = vec![];
            for id in ids {
                let n = a["last"].as_u64().map(|n| n as usize);
                sections.push(match self.section(id, view, n, share, details).await {
                    Ok(s) => s,
                    // One unreadable thread does not hide the others.
                    Err(e) => format!("Thread {id}: {}", crate::client::bounded_text(&e.to_string(), 300)),
                });
            }
            return Ok(sections.join("\n\n"));
        }
        let thread = a["threadId"].as_str().context("threadId missing")?;
        if let Some(item) = a["itemId"].as_str() {
            return self.item(thread, item, a["view"].as_str().unwrap_or("activity"), a["textOffset"].as_u64()).await;
        }
        self.section(thread, view, a["last"].as_u64().map(|n| n as usize), BUDGET, details).await
    }

    async fn section(&self, thread: &str, view: &str, last: Option<usize>, budget: usize, details: bool) -> Result<String> {
        let out = if view == "activity" {
            self.activity(thread, last.unwrap_or(10)).await?
        } else {
            self.messages(thread, last.unwrap_or(3), budget).await?
        };
        if !details {
            return Ok(out);
        }
        let l = self.latest(thread, "messages", 1, 1).await?;
        Ok(format!("{out}\n{}", details_lines(&l.thread, &l.runs)))
    }

    /// The thread's state line and its last `n` messages, whole, oldest first. Whole messages fit `budget` newest
    /// first; the newest is cut only when it alone is larger, with the call that continues it.
    async fn messages(&self, thread: &str, n: usize, budget: usize) -> Result<String> {
        let l = self.latest(thread, "messages", n, PAGE_CHARS).await?;
        let head = self.header(thread, &l.thread)?;
        if l.items.is_empty() {
            return Ok(format!("{head}\nNo messages yet."));
        }
        let mut used = head.len();
        let mut parts = vec![];
        let mut omitted = 0;
        for (i, item) in l.items.iter().rev().enumerate() {
            let label = label(item);
            let room = budget.saturating_sub(used + label.len() + 4);
            let (text, whole) = self.whole_text(thread, item.clone(), room).await?;
            if whole && text.trim().len() <= room {
                used += label.len() + text.trim().len() + 4;
                parts.push(format!("{label}:\n{}", text.trim()));
                continue;
            }
            if i == 0 {
                // The newest message alone is past the budget: what fits, and where it continues (T3's own UTF-16
                // offset into the untrimmed text).
                let mut cut = room.saturating_sub(300).min(text.len());
                while !text.is_char_boundary(cut) {
                    cut -= 1;
                }
                let offset = text[..cut].encode_utf16().count();
                let id = item["itemId"].as_str().unwrap_or("");
                parts.push(format!(
                    "{label}:\n{}\nMore: t3_thread_read {{threadId: \"{thread}\", itemId: \"{id}\", view: \"messages\", textOffset: {offset}}}",
                    &text[..cut]
                ));
                omitted = l.items.len() - 1;
            } else {
                omitted = l.items.len() - i;
            }
            break;
        }
        parts.reverse();
        let mut out = format!("{head}\n{}", parts.join("\n\n"));
        if omitted > 0 {
            out.push_str(&format!(
                "\n({omitted} earlier message{} did not fit; read fewer with last.)",
                if omitted == 1 { "" } else { "s" }
            ));
        }
        Ok(out)
    }

    /// The last `n` timeline items, one line each (type, state, first line), newest last, with their item ids.
    async fn activity(&self, thread: &str, n: usize) -> Result<String> {
        let l = self.latest(thread, "activity", n, (ACTIVITY_CHARS * 2) as u64).await?;
        let head = self.header(thread, &l.thread)?;
        if l.items.is_empty() {
            return Ok(format!("{head}\nNo activity yet."));
        }
        let lines: Vec<String> = l.items.iter().map(activity_line).collect();
        Ok(format!(
            "{head}\nLatest activity, oldest first (whole text of one: t3_thread_read {{threadId, itemId}}):\n{}",
            lines.join("\n")
        ))
    }

    /// One item's whole text, continued from `offset` (UTF-16 units, T3's own).
    async fn item(&self, thread: &str, item: &str, view: &str, offset: Option<u64>) -> Result<String> {
        let mut args = json!({"threadId":thread,"view":view,"itemId":item,"limit":1,"maxCharsPerItem":PAGE_CHARS,"runLimit":1});
        if let Some(o) = offset {
            args["textOffset"] = json!(o);
        }
        let r = self.client.call("t3_thread_read", args).await?;
        let first = r["items"].as_array().and_then(|i| i.first()).cloned().with_context(|| format!("ITEM_NOT_FOUND: no item {item} in thread {thread}"))?;
        let (text, whole) = self.whole_text_in(thread, view, first.clone(), BUDGET - 1024).await?;
        let mut out = format!("{}\n{}:\n{text}", self.header(thread, &r["thread"])?, item_kind(&first));
        if !whole {
            let next = offset.unwrap_or(0) as usize + text.encode_utf16().count();
            out.push_str(&format!(
                "\nMore: t3_thread_read {{threadId: \"{thread}\", itemId: \"{item}\", view: \"{view}\", textOffset: {next}}}"
            ));
        }
        Ok(out)
    }

    /// `'Title' · running · updated 2 min ago · model [thread id]`, plus a waiting question or approval.
    pub(crate) fn header(&self, id: &str, t: &Value) -> Result<String> {
        let mut out = format!("'{}' · {}", title(t), t["status"].as_str().unwrap_or("idle"));
        if let Some(ago) = t["updatedAt"].as_str().and_then(|u| text::ago(u, crate::now_ms())) {
            out.push_str(&format!(" · updated {ago}"));
        }
        if let Some(m) = t["model"].as_str() {
            out.push_str(&format!(" · {m}"));
        }
        if self.owns_task(id)? {
            out.push_str(" · your task");
        }
        match live_requests(t).max(u64::from(asks(t))) {
            0 => {}
            1 => out.push_str(" · waits on a question or approval"),
            n => out.push_str(&format!(" · waits on {n} questions or approvals")),
        }
        if t["archived"] == true {
            out.push_str(" · archived");
        }
        out.push_str(&format!(" [thread {id}]"));
        Ok(out)
    }
}

fn title(t: &Value) -> String {
    let raw = text::title(t["title"].as_str().unwrap_or(""));
    let flat = raw.replace(['\'', '\n'], " ");
    if flat.trim().is_empty() { "Untitled".into() } else { crate::client::bounded_text(&flat, 80) }
}
fn list_line(t: &Value, now: u64) -> String {
    let status = t["status"].as_str().unwrap_or("idle");
    let state = if status == "waiting" { "waiting for an answer or approval" } else { status };
    let mut line = format!("'{}' · {state}", title(t));
    if let Some(ago) = t["updatedAt"].as_str().and_then(|u| text::ago(u, now)) {
        line.push_str(&format!(" · {ago}"));
    }
    line
}
/// Who wrote a message: the user, an agent through MCP (Heimdall or another), the thread's assistant, a plan.
fn label(item: &Value) -> String {
    let who = match item["type"].as_str().unwrap_or("") {
        "user_message" if item["createdBy"] == "user" => "User".to_owned(),
        "user_message" => "Sent by an agent".to_owned(),
        "assistant_message" => "Assistant".to_owned(),
        "proposed_plan" => "Plan".to_owned(),
        other => other.replace('_', " "),
    };
    match item["updatedAt"].as_str().and_then(text::clock) {
        Some(at) => format!("{who}, {at}"),
        None => who,
    }
}
fn item_kind(item: &Value) -> String {
    let mut kind = item["type"].as_str().unwrap_or("item").replace('_', " ");
    if let Some(t) = item["title"].as_str().filter(|t| !t.trim().is_empty()) {
        kind.push_str(&format!(" '{}'", t.trim()));
    }
    if let Some(s) = item["status"].as_str().filter(|s| *s != "completed") {
        kind.push_str(&format!(" ({s})"));
    }
    kind
}
fn activity_line(item: &Value) -> String {
    let first = item["text"].as_str().unwrap_or("").lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    let first = crate::client::bounded_text(first, ACTIVITY_CHARS);
    format!(
        "#{} {}{} [item {}]",
        item["position"],
        item_kind(item),
        if first.is_empty() { String::new() } else { format!(": {first}") },
        item["itemId"].as_str().unwrap_or("")
    )
}
/// `details`: the latest runs and the thread's settings.
fn details_lines(t: &Value, runs: &Value) -> String {
    let mut out = vec!["Runs, newest first:".to_owned()];
    for r in runs.as_array().into_iter().flatten() {
        let mut line = format!("- #{} {}", r["ordinal"], r["status"].as_str().unwrap_or("?"));
        let start = r["startedAt"].as_str().or(r["requestedAt"].as_str()).and_then(text::clock);
        let end = r["completedAt"].as_str().and_then(text::clock);
        match (start, end) {
            (Some(s), Some(e)) => line.push_str(&format!(" {s}–{e}")),
            (Some(s), None) => line.push_str(&format!(" since {s}")),
            _ => {}
        }
        if let Some(m) = r["model"].as_str() {
            line.push_str(&format!(" · {m}"));
        }
        out.push(line);
    }
    let mut settings = vec![];
    for (key, name) in [("projectId", "project"), ("providerInstanceId", "provider"), ("runtimeMode", "mode"), ("branch", "branch"), ("worktreePath", "worktree"), ("parentThreadId", "parent")] {
        if let Some(v) = t[key].as_str() {
            settings.push(format!("{name} {v}"));
        }
    }
    if let Some(n) = t["itemCount"].as_u64() {
        settings.push(format!("{n} items"));
    }
    if let Some(c) = t["createdAt"].as_str() {
        settings.push(format!("created {c}"));
    }
    out.push(format!("Settings: {}", settings.join(" · ")));
    out.join("\n")
}

/// The compact form of a T3 result passed through `t3_call`, for the tools whose raw results are walls of metadata
/// (bug note 18-17-04: a `t3_thread_read` carried ten runs and eighteen thread fields per call). None: the generic
/// plain rendering.
pub fn compact(tool: &str, args: &Value, r: &Value) -> Option<String> {
    Some(match tool {
        "t3_thread_read" => {
            let t = &r["thread"];
            let id = t["threadId"].as_str().or(args["threadId"].as_str()).unwrap_or("?");
            let mut out = format!("'{}' · {} [thread {id}]", title(t), t["status"].as_str().unwrap_or("idle"));
            if asks(t) {
                out.push_str(" · waits on a question or approval");
            }
            let items = r["items"].as_array()?;
            if items.is_empty() {
                out.push_str("\nNo more items.");
            }
            for i in items {
                out.push_str(&format!("\n#{} {}:\n{}", i["position"], item_kind(i), i["text"].as_str().unwrap_or("").trim_end()));
                if i["textTruncated"] == true {
                    if let Some(next) = i["nextTextOffset"].as_u64() {
                        out.push_str(&format!("\n(continues: itemId \"{}\", textOffset {next})", i["itemId"].as_str().unwrap_or("")));
                    }
                }
            }
            if r["hasMore"] == true {
                out.push_str(&format!("\nMore: afterPosition {}", r["nextPosition"]));
            }
            out
        }
        "t3_thread_list" => {
            let rows = r["threads"].as_array()?;
            let now = crate::now_ms();
            let mut out = if rows.is_empty() { "No threads.".to_owned() } else { format!("{} of {} threads:", rows.len(), r["total"].as_u64().unwrap_or(rows.len() as u64)) };
            for t in rows {
                let mut line = list_line(t, now);
                if t["relationshipToParent"] == "subagent" {
                    line.push_str(" · sub-agent");
                }
                out.push_str(&format!("\n{line} [thread {}]", t["threadId"].as_str().unwrap_or("?")));
            }
            if !r["nextCursor"].is_null() {
                out.push_str(&format!("\nMore: cursor {}", r["nextCursor"]));
            }
            out
        }
        "t3_project_list" => {
            let rows = r["projects"].as_array()?;
            let mut lines: Vec<String> = rows
                .iter()
                .map(|p| format!("'{}' · {} [project {}]", p["title"].as_str().unwrap_or("?"), p["workspaceRoot"].as_str().unwrap_or("?"), p["id"].as_str().unwrap_or("?")))
                .collect();
            if !r["nextCursor"].is_null() {
                lines.push(format!("More: cursor {}", r["nextCursor"]));
            }
            if lines.is_empty() { "No projects.".into() } else { lines.join("\n") }
        }
        "orchestrator_capabilities" => text::catalog(r),
        "t3_thread_send" => {
            let mut out = format!("Sent to thread {}", r["threadId"].as_str().or(args["threadId"].as_str()).unwrap_or("?"));
            let detail: Vec<&str> = [r["delivery"].as_str(), r["status"].as_str()].into_iter().flatten().collect();
            if !detail.is_empty() {
                out.push_str(&format!(" ({})", detail.join(", ")));
            }
            // The promise matches the watch (orchestrator.rs t3_call: a run to follow) and T3's delivery: a queued
            // message runs only after the thread's current turn, and that turn's end is not its reply.
            out.push_str(match (r["runId"].as_str(), r["delivery"].as_str()) {
                (None, _) => ". T3 returned no run to follow; read the reply later with t3_thread_read.",
                (Some(_), Some("queued")) => ". It waits behind the thread's current turn; its reply arrives as a notice when that queued turn ends.",
                (Some(_), _) => ". Its reply arrives as a notice when this run ends.",
            });
            // Bug note 2026-10-26T23-42: stacked steers could not be taken back, the reply hid the queued run's id.
            if let (Some(run), Some("queued")) = (r["runId"].as_str(), r["delivery"].as_str()) {
                out.push_str(&format!(" Queued run {run}: t3_call with t3_queue (action edit or cancel) and this queuedRunId changes or takes it back."));
            }
            out
        }
        "t3_thread_launch" => format!(
            "Started thread {} ({}). Its reply arrives as a notice when it is done.",
            r["threadId"].as_str()?,
            r["status"].as_str().unwrap_or("starting")
        ),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    /// Bug note 2026-11-04T22-25-15: the setup-wizard task's run 7, queued by T3 and cancelled before it started.
    #[test]
    fn a_run_cancelled_before_it_started_is_not_the_threads_state() {
        let run6 = json!({"runId":"r6","ordinal":6,"status":"running","startedAt":"2026-11-04T21:22:19Z"});
        let run7 = json!({"runId":"r7","ordinal":7,"status":"cancelled","startedAt":null,"completedAt":"2026-11-04T21:22:37Z"});
        let during = json!({"status":"cancelled","latestRunId":"r7","activeRunId":"r6"});
        assert_eq!(current_run(&during, &[run7.clone(), run6.clone()]), (Some("r6".into()), "running".into()));
        let mut done6 = run6.clone();
        done6["status"] = json!("completed");
        let after = json!({"status":"cancelled","latestRunId":"r7","activeRunId":null});
        assert_eq!(current_run(&after, &[run7.clone(), done6]), (Some("r6".into()), "completed".into()));
        // Only the dropped run in view, or the thread's only run: T3's own state.
        assert_eq!(current_run(&after, &[run7.clone()]), (Some("r7".into()), "cancelled".into()));
        // A run that started and was cancelled is the thread's state.
        let mut started = run7.clone();
        started["startedAt"] = json!("2026-11-04T21:22:30Z");
        assert_eq!(current_run(&after, &[started, run6]), (Some("r7".into()), "cancelled".into()));
        let waiting = json!({"status":"waiting","latestRunId":"r1","activeRunId":"r1"});
        assert_eq!(current_run(&waiting, &[]), (Some("r1".into()), "waiting".into()));
    }
    /// Review 5 Nov: (a) a result recorded against a dropped run (before the rule above) keeps that run as the followed
    /// one while it is newer than the run that started, so the earlier run never looks newer; (b) Heimdall's own
    /// follow-up dropped before it started reads "cancelled", not "completed" with the earlier run's result.
    #[test]
    fn a_dropped_run_is_compared_by_order_and_heimdalls_own_dropped_message_reads_cancelled() {
        let run6 = json!({"runId":"r6","ordinal":6,"status":"completed","startedAt":"2026-11-04T21:22:19Z"});
        let run7 = json!({"runId":"r7","ordinal":7,"status":"cancelled","startedAt":null,"completedAt":"2026-11-04T21:22:37Z"});
        let after = json!({"status":"cancelled","latestRunId":"r7","activeRunId":null});
        let runs = [run7.clone(), run6.clone()];
        let none = |_: &str| false;
        // (a) Recorded against r7: r7 stays, with its recorded state. Recorded at r6, or nothing recorded: r6.
        assert_eq!(followed_run(&after, &runs, Some(("r7", "cancelled")), &none), (Some("r7".into()), "cancelled".into()));
        assert_eq!(followed_run(&after, &runs, Some(("r6", "completed")), &none), (Some("r6".into()), "completed".into()));
        assert_eq!(followed_run(&after, &runs, None, &none), (Some("r6".into()), "completed".into()));
        // While r6 still runs (T3 shows no active run yet), r6 decides.
        let mut running6 = run6.clone();
        running6["status"] = json!("running");
        assert_eq!(followed_run(&after, &[run7.clone(), running6], Some(("r7", "cancelled")), &none), (Some("r6".into()), "running".into()));
        // A dropped run older than the one that started is no stand-in.
        let run5 = json!({"runId":"r5","ordinal":5,"status":"cancelled","startedAt":null});
        assert_eq!(followed_run(&after, &[run7.clone(), run6.clone(), run5], Some(("r5", "cancelled")), &none), (Some("r6".into()), "completed".into()));
        // (b) Heimdall sent r7: his message never ran.
        let mine = |id: &str| id == "r7";
        assert_eq!(followed_run(&after, &runs, Some(("r7", "running")), &mine), (Some("r7".into()), "cancelled".into()));
        // A run of his that started (r6) and finished is not "dropped".
        let six = |id: &str| id == "r6";
        assert_eq!(followed_run(&after, &runs, Some(("r6", "completed")), &six), (Some("r6".into()), "completed".into()));
        // While a run is active it decides.
        let during = json!({"status":"cancelled","latestRunId":"r7","activeRunId":"r8"});
        assert_eq!(followed_run(&during, &runs, Some(("r7", "running")), &mine), (Some("r8".into()), "running".into()));
    }
    #[test]
    fn compact_thread_read_drops_runs_and_metadata() {
        let raw = json!({"thread":{"threadId":"t1","title":"Compare Qwen 27B [heimdall:ab]","status":"completed","pendingRequestCount":0,"itemCount":321,"runtimeMode":"full-access","model":"claude-opus-5-5"},
            "recentRuns":(0..10).map(|i|json!({"runId":format!("run-{i}"),"status":"completed","model":"claude-opus-5-5","ordinal":i})).collect::<Vec<_>>(),
            "items":[{"position":312,"itemId":"i-312","type":"assistant_message","status":"completed","text":"Found a better candidate.","textTruncated":false},
                     {"position":319,"itemId":"i-319","type":"assistant_message","status":"completed","text":"Short version: …","textTruncated":true,"nextTextOffset":1500}],
            "nextPosition":319,"hasMore":false});
        let out = compact("t3_thread_read", &json!({"threadId":"t1"}), &raw).unwrap();
        assert_eq!(out, "'Compare Qwen 27B' · completed [thread t1]\n#312 assistant message:\nFound a better candidate.\n#319 assistant message:\nShort version: …\n(continues: itemId \"i-319\", textOffset 1500)");
        assert!(!out.contains("run-") && !out.contains("full-access"), "{out}");
        let caps = json!({"providers":[{"providerInstanceId":"claudeAgent","models":[{"id":"claude-haiku-5-5","options":[{"id":"effort","type":"select","options":[{"id":"low"},{"id":"medium","isDefault":true},{"id":"high"}]}]}]},
            {"providerInstanceId":"cursor","models":[{"id":"x"}],"constraints":["Provider instance is disabled."]}]});
        assert_eq!(compact("orchestrator_capabilities", &json!({}), &caps).unwrap(), "provider model — options (* default):\nclaudeAgent claude-haiku-5-5 — effort low|medium*|high");
        assert_eq!(
            compact("t3_thread_send", &json!({"threadId":"t1"}), &json!({"threadId":"t1","delivery":"started","status":"starting","runId":"r","messageId":"m"})).unwrap(),
            "Sent to thread t1 (started, starting). Its reply arrives as a notice when this run ends."
        );
        assert_eq!(
            compact("t3_thread_send", &json!({"threadId":"t1"}), &json!({"threadId":"t1","delivery":"queued","status":"queued","runId":"r2","messageId":"m"})).unwrap(),
            "Sent to thread t1 (queued, queued). It waits behind the thread's current turn; its reply arrives as a notice when that queued turn ends. Queued run r2: t3_call with t3_queue (action edit or cancel) and this queuedRunId changes or takes it back."
        );
        assert!(compact("t3_thread_send", &json!({"threadId":"t1"}), &json!({"threadId":"t1","delivery":"started"})).unwrap().ends_with("T3 returned no run to follow; read the reply later with t3_thread_read."));
        assert!(compact("t3_queue", &json!({"action":"list"}), &json!({})).is_none());
    }
    #[test]
    fn labels_and_activity_lines() {
        assert!(label(&json!({"type":"user_message","createdBy":"user"})).starts_with("User"));
        assert!(label(&json!({"type":"user_message","createdBy":"agent","creationSource":"mcp"})).starts_with("Sent by an agent"));
        assert_eq!(label(&json!({"type":"assistant_message"})), "Assistant");
        let failed = json!({"position":19,"itemId":"turn-item:x:toolu_015","type":"command_execution","status":"failed","text":"$ ps -o pid -p 38738; ls ~/.cache\nNo such process"});
        assert_eq!(activity_line(&failed), "#19 command execution (failed): $ ps -o pid -p 38738; ls ~/.cache [item turn-item:x:toolu_015]");
        let sub = json!({"position":43,"itemId":"s","type":"subagent","status":"running","title":"Research oMLX","text":""});
        assert_eq!(activity_line(&sub), "#43 subagent 'Research oMLX' (running) [item s]");
    }
}
