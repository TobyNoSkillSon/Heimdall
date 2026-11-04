//! Reading the agent's Pi session file as Pi builds the model's context (`buildContextEntries`), shared by the plan
//! refresher (the whole active context each pass) and the Goals shadow (`goals::stream`: each new entry once, by id).
//! Pure functions over the file's JSON lines: no app state, so the `goals-lab` binary includes this file as is.

use serde_json::Value;
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};

/// The plan refresher's notice id prefix (`plan_refresher::NOTICE_PREFIX`): its own notices are never input.
pub const REFRESHER_NOTICE: &str = "plan-refresh-";

/// The agent's live session: `agent/active-session.json` names it, else `pi-session.jsonl` (`bridge::startup`).
pub fn session_path(data: &Path) -> PathBuf {
    let pointer = data.join("agent/active-session.json");
    std::fs::read(&pointer)
        .ok()
        .and_then(|b| serde_json::from_slice::<PathBuf>(&b).ok())
        .filter(|p| p.starts_with(data) && !p.components().any(|c| matches!(c, std::path::Component::ParentDir)))
        .unwrap_or_else(|| data.join("pi-session.jsonl"))
}

/// The file's first `len` bytes as entries (a line still being written, or not JSON, is skipped).
pub fn entries(path: &Path, len: u64) -> std::io::Result<Vec<Value>> {
    if len == 0 { return Ok(Vec::new()) }
    let mut bytes = Vec::new();
    std::fs::File::open(path)?.take(len).read_to_end(&mut bytes)?;
    Ok(bytes.split(|b| *b == b'\n').filter_map(|l| serde_json::from_slice(l).ok()).collect())
}

/// The entries written since `offset`, up to the last whole line of the file's first `len` bytes, and the offset after
/// them (a line still being written waits for the next read).
pub fn entries_from(path: &Path, offset: u64, len: u64) -> std::io::Result<(Vec<Value>, u64)> {
    use std::io::{Seek, SeekFrom};
    if len <= offset { return Ok((Vec::new(), offset)) }
    let mut f = std::fs::File::open(path)?;
    f.seek(SeekFrom::Start(offset))?;
    let mut bytes = Vec::new();
    f.take(len - offset).read_to_end(&mut bytes)?;
    let Some(end) = bytes.iter().rposition(|b| *b == b'\n') else { return Ok((Vec::new(), offset)) };
    let entries = bytes[..end].split(|b| *b == b'\n').filter_map(|l| serde_json::from_slice(l).ok()).collect();
    Ok((entries, offset + end as u64 + 1))
}

/// The branch that ends at the last entry with an id (sessions are trees by `parentId`), root first; entries without
/// ids after it (or in a file with none) follow in file order. Abandoned branches are not on it.
pub fn branch(entries: &[Value]) -> Vec<&Value> {
    let by_id: HashMap<&str, &Value> = entries.iter().filter_map(|e| e["id"].as_str().map(|id| (id, e))).collect();
    let Some(last) = entries.iter().rposition(|e| e["id"].is_string()) else { return entries.iter().collect() };
    let mut path = Vec::new();
    let mut seen = HashSet::new();
    let mut cursor = entries[last]["id"].as_str();
    while let Some(id) = cursor {
        if !seen.insert(id) { break }
        let Some(e) = by_id.get(id) else { break };
        path.push(*e);
        cursor = e["parentId"].as_str();
    }
    path.reverse();
    path.extend(&entries[last + 1..]);
    path
}

/// Pi's active context: the branch (`branch`) from its latest compaction's `firstKeptEntryId` on (just after the
/// compaction when that entry is not on the branch), and that compaction's summary.
pub fn active_context(entries: &[Value]) -> (Option<String>, Vec<Cow<'_, Value>>) { active_from(branch(entries)) }

/// Each target's latest `context_edit` on the path is applied as Pi does: `replacement: null` drops the entry, else its
/// `content` stands in for the message's (Heimdall's history repair writes these).
fn active_from(path: Vec<&Value>) -> (Option<String>, Vec<Cow<'_, Value>>) {
    let at = path.iter().rposition(|e| e["type"] == "compaction");
    let summary = at.and_then(|at| path[at]["summary"].as_str().map(str::to_owned));
    let start = at.map_or(0, |at| {
        let kept = path[at]["firstKeptEntryId"].as_str();
        kept.and_then(|k| path[..at].iter().position(|e| e["id"].as_str() == Some(k))).unwrap_or(at + 1)
    });
    let context = &path[start..];
    let mut edits: HashMap<&str, &Value> = HashMap::new();
    for e in context.iter().filter(|e| e["type"] == "context_edit") {
        if let Some(target) = e["targetId"].as_str() { edits.insert(target, &e["replacement"]); }
    }
    let mut out = Vec::new();
    for e in context.iter().filter(|e| e["type"] != "compaction") {
        match e["id"].as_str().and_then(|id| edits.get(id)) {
            None => out.push(Cow::Borrowed(*e)),
            Some(Value::Null) => {}
            Some(replacement) => {
                let mut v = (*e).clone();
                v["message"]["content"] = replacement["content"].clone();
                out.push(Cow::Owned(v));
            }
        }
    }
    (summary, out)
}

/// A message's text blocks joined (a plain string content as is); images and thinking are never text.
pub fn text_of(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(a) => a.iter().filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    }
}

/// One session entry as conversation lines: what was said, which tools ran and what they answered (short). Only
/// `message` entries (the plan refresher's reading); `goals::stream` adds Pi's `custom_message` entries.
pub fn render_entry(v: &Value) -> Vec<String> {
    if v["type"] != "message" { return Vec::new() }
    let m = &v["message"];
    let blocks: Vec<&Value> = match &m["content"] {
        Value::Array(a) => a.iter().collect(),
        _ => Vec::new(),
    };
    let text = |limit: usize| -> String { clip(without_own_notices(&text_of(&m["content"])).trim(), limit) };
    let tool = |n: &str| n.strip_prefix("mcp__heimdall__").unwrap_or(n).to_owned();
    match m["role"].as_str() {
        Some("user") => {
            let t = text(usize::MAX);
            if t.is_empty() { vec![] } else { vec![format!("Incoming: {t}")] }
        }
        Some("assistant") => {
            let mut out = Vec::new();
            let t = text(usize::MAX);
            if !t.is_empty() { out.push(format!("Heimdall: {t}")); }
            for b in blocks.iter().filter(|b| b["type"] == "toolCall") {
                let name = tool(b["name"].as_str().unwrap_or("tool"));
                out.push(format!("Heimdall called {name} {}", clip(&b["arguments"].to_string(), 400)));
            }
            out
        }
        Some("toolResult") => {
            let t = text(300);
            if t.is_empty() { vec![] } else { vec![format!("  {} answered: {t}", tool(m["toolName"].as_str().unwrap_or("tool")))] }
        }
        _ => Vec::new(),
    }
}

/// The text without the refresher's own notices (`[Heimdall notice: plan-refresh-…]` up to the next notice header):
/// they are not news for the next pass, nor input for the model. Other notices coalesced beside them stay.
pub fn without_own_notices(t: &str) -> String {
    const ANY: &str = "[Heimdall notice: ";
    let own = format!("{ANY}{REFRESHER_NOTICE}");
    if !t.contains(&own) { return t.to_owned() }
    let mut out = String::new();
    let mut rest = t;
    while let Some(at) = rest.find(&own) {
        out.push_str(&rest[..at]);
        let after = &rest[at + own.len()..];
        rest = after.find(ANY).map_or("", |n| &after[n..]);
    }
    out.push_str(rest);
    out
}

/// At most `n` characters, with an ellipsis when cut.
pub fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n { return s.to_owned() }
    let mut out: String = s.chars().take(n.saturating_sub(1)).collect();
    out.push('\u{2026}');
    out
}
