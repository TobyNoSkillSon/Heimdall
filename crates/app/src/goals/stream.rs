//! The shadow's stream (plan, 31 Oct): the agent's Pi session rendered entry by entry, the same conversation the main
//! agent gets (its messages and Pi's `custom_message` notices, which Pi turns into user messages: relayed transcripts,
//! T3 task notices, catch-ups), images dropped, long tool output cut, the plan refresher's own notices and pure
//! bookkeeping (receipts, model changes, compactions, edits) left out. Each entry is rendered once, by its id, and
//! appended to `<data>/goals/stream.jsonl` as `{id, at, lines, kind}`: the cursor is the set of ids already there, so a
//! Pi compaction (a new entry on the same branch) or a restart (a new session file) never sends a line twice.

use crate::pi_session::{branch, clip, text_of, without_own_notices};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};

/// A tool's answer, at most (a file read or a search result is a pointer, not news).
pub const TOOL_CHARS: usize = 300;
/// A tool call's arguments, at most.
pub const ARGS_CHARS: usize = 300;
/// One message, at most (a pasted wall of text).
pub const MESSAGE_CHARS: usize = 4_000;

/// What an entry is, for the triggers: the user's words fire a call when he goes quiet, a finished task soon after;
/// Heimdall's lines, tools and other notices ride along with the next call.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind { Owner, Heimdall, Tool, Notice, Task }

/// One rendered entry.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Line {
    /// The session entry's id (a hash of the entry when it has none).
    pub id: String,
    /// When it was written, epoch ms (the entry's timestamp, else when it was first read).
    pub at: i64,
    pub lines: Vec<String>,
    pub kind: Kind,
}

impl Line {
    pub fn text(&self) -> String { self.lines.join("\n") }
}

/// An entry's time in epoch ms: its ISO `timestamp`, else its message's ms `timestamp`.
pub fn entry_ms(e: &Value) -> Option<i64> {
    if let Some(t) = e["timestamp"].as_str() {
        if let Ok(d) = chrono::DateTime::parse_from_rfc3339(t) { return Some(d.timestamp_millis()) }
    }
    e["timestamp"].as_i64().or_else(|| e["message"]["timestamp"].as_i64())
}

fn entry_id(e: &Value) -> String {
    match e["id"].as_str() {
        Some(id) => id.to_owned(),
        None => {
            // FNV-1a of the entry: stable from read to read.
            let mut h: u64 = 0xcbf2_9ce4_8422_2325;
            for b in e.to_string().bytes() { h = (h ^ b as u64).wrapping_mul(0x0000_0100_0000_01b3) }
            format!("h-{h:016x}")
        }
    }
}

/// The delivery header and the clock line Heimdall puts before a message (`[Heimdall delivery: …]`, `[Now: …]`): the
/// stream carries its own time.
/// Lines the pipeline repeats around every relayed input (the agent's own instructions, not news): about a sixth of a
/// busy stretch's lines in the climb (v2, 31 Oct), each one resent with every later turn.
const BOILERPLATE: [&str; 6] = [
    "Owner's wake instruction:",
    "Act on User's latest request to you in the transcript, if there is one. Everyone else is context.",
    "Tell User only what he needs to know; if nothing here needs him, your whole reply is NO_REPLY.",
    "Transcript:",
    "[Emergency mode: this reached the cover agent; you were down, so it started no turn of yours.]",
    "[Heimdall typed message]",
];

fn without_headers(t: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for l in t.lines() {
        let s = l.trim();
        // In either edition's wording (the public one sends the user's name or "the user": `agent::identity::for_agent`).
        if BOILERPLATE.iter().any(|b| *b == s || agent::identity::public_wording(b, &agent::identity::current()) == s) { continue }
        if s.starts_with("[Heimdall delivery: ") && s.ends_with(']') || s.starts_with("[Now: ") && s.ends_with(']') { continue }
        out.push(l);
    }
    out.join("\n")
}

/// What an incoming text is: a T3 task's result (`terminal:`), another notice, or the user's own words.
fn incoming_kind(t: &str) -> Kind {
    if t.starts_with("Heimdall catch-up:") { return Kind::Notice }
    let notice = t.contains("[Heimdall notice: ") || t.contains("[Heimdall delivery: notice-");
    // The user's words delivered together with a notice are still his: they wait for his quiet minute (review 31 Oct).
    if !notice || said(t) { return Kind::Owner }
    if t.contains("[Heimdall notice: terminal:") { Kind::Task } else { Kind::Notice }
}

/// A notice of the user's own click on an island ("the user chose …", "the user ticked …"): his answer as much as his words. Only
/// the islands' own notices (`[Heimdall notice: islands-answer:…]`, `islands-context:…`) count, never a line of
/// Heimdall's that says so (review 31 Oct).
pub fn owners_click(l: &Line) -> bool {
    let t = l.text();
    // Also a question card's answer (`[Heimdall notice: ask-…] Question ask-…: the user clicked Install both.`; night 1 Nov:
    // two of the three asks left open had been answered there).
    // The marker on the entry's own first line, never inside a catch-up of spooled lines (review 1 Nov: a catch-up is a
    // notice whose text holds other speakers' words).
    let head = l.lines.first().map(String::as_str).unwrap_or("");
    let card = head.contains("[Heimdall notice: ask-") && t.contains(": User clicked ");
    l.kind == Kind::Notice && !t.contains("Heimdall catch-up:")
        && (card || head.contains("[Heimdall notice: islands-answer:") || head.contains("[Heimdall notice: islands-context:"))
        && ["User chose ", "User ticked ", "User clicked ", "User picked ", "User switched ", "User set "].iter().any(|m| t.contains(m))
}

/// Words typed or heard in the text: a typed message, or a transcript line (`[Mic · User] …`, source · speaker).
fn said(t: &str) -> bool {
    t.lines().any(|l| {
        let l = l.trim_start();
        l.starts_with("[Heimdall typed message]") || l.strip_prefix('[').and_then(|r| r.split_once("] ")).is_some_and(|(tag, _)| tag.contains(" · ") && !tag.starts_with("Heimdall"))
    })
}

fn lines_of(prefix: &str, t: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (i, l) in t.lines().map(str::trim_end).filter(|l| !l.trim().is_empty()).enumerate() {
        out.push(if i == 0 { format!("{prefix}{}", l.trim_start()) } else { format!("  {l}") });
    }
    out
}

fn one_line(t: &str) -> String { t.split_whitespace().collect::<Vec<_>>().join(" ") }

/// One entry as stream lines, or `None` when it is bookkeeping or says nothing.
pub fn render(e: &Value, read_at: i64) -> Option<Line> {
    let at = entry_ms(e).unwrap_or(read_at);
    let id = entry_id(e);
    let incoming = |raw: &str| -> Option<(Vec<String>, Kind)> {
        // A relayed input held by the relay agent is still what reached Heimdall.
        let raw = raw.strip_prefix(agent::emergency::HELD).map_or(raw, str::trim_start);
        let kind = incoming_kind(raw);
        let t = clip(without_headers(&without_own_notices(raw)).trim(), MESSAGE_CHARS);
        if t.is_empty() { return None }
        Some((lines_of(if kind == Kind::Owner { "Incoming: " } else { "Notice: " }, &t), kind))
    };
    let (lines, kind) = match e["type"].as_str()? {
        "message" => {
            let m = &e["message"];
            let tool = |n: &str| n.strip_prefix("mcp__heimdall__").unwrap_or(n).to_owned();
            match m["role"].as_str()? {
                "user" => incoming(&text_of(&m["content"]))?,
                "assistant" => {
                    let t = clip(text_of(&m["content"]).trim(), MESSAGE_CHARS);
                    // A turn that chose to say nothing says nothing here either.
                    let t = if t == "NO_REPLY" { String::new() } else { t };
                    let mut out = lines_of("Heimdall: ", &t);
                    let said = !out.is_empty();
                    for b in m["content"].as_array().into_iter().flatten().filter(|b| b["type"] == "toolCall") {
                        let name = tool(b["name"].as_str().unwrap_or("tool"));
                        out.push(format!("Heimdall called {name} {}", clip(&b["arguments"].to_string(), ARGS_CHARS)));
                    }
                    if out.is_empty() { return None }
                    (out, if said { Kind::Heimdall } else { Kind::Tool })
                }
                "toolResult" => {
                    let t = clip(&one_line(&text_of(&m["content"])), TOOL_CHARS);
                    if t.is_empty() { return None }
                    (vec![format!("  {} answered: {t}", tool(m["toolName"].as_str().unwrap_or("tool")))], Kind::Tool)
                }
                _ => return None,
            }
        }
        // Pi turns these into user messages (`createCustomMessage`): relayed transcripts, task notices, catch-ups.
        "custom_message" => {
            let t = text_of(&e["content"]);
            match relay(&t) {
                Some(Some(said)) => said,
                Some(None) => return None,
                None => incoming(&t)?,
            }
        }
        _ => return None,
    };
    Some(Line { id, at, lines, kind })
}

/// The relay thread's own record in Pi's session (`crate::emergency`'s mirror and its tool lines, `[Heimdall notice:
/// emergency-cover:…] [Relay agent] …`): the relay is Heimdall's agent, so what it says is Heimdall's own words and
/// what it runs its tools (Goals night 1 Nov: finished work stated only by the relay was missed as a mere notice).
/// `None`: not a relay record; `Some(None)`: a silent turn, not news.
fn relay(t: &str) -> Option<Option<(Vec<String>, Kind)>> {
    let rest = t.trim_start().strip_prefix(&format!("[Heimdall notice: {}", agent::notice::EMERGENCY_COVER))?;
    let (id, body) = rest.split_once(']')?;
    let body = body.trim();
    // A call of Heimdall's own tools (`record`, an id of its own; the thread's messages are `mirror:` ids): a tool
    // line, its words to the user excepted.
    let call = !id.starts_with("mirror:");
    for agent in ["[Relay agent", "[Cover agent"] {
        let Some(after) = body.strip_prefix(agent) else { continue };
        let (tag, text) = after.split_once(']')?;
        let text = clip(text.trim(), MESSAGE_CHARS);
        if tag.is_empty() {
            if text.is_empty() || text == "NO_REPLY" { return Some(None) }
            if call && !text.starts_with("said to User:") {
                return Some(Some((vec![clip(&format!("Heimdall called {}", one_line(&text)), TOOL_CHARS + 40)], Kind::Tool)));
            }
            return Some(Some((lines_of("Heimdall: ", &text), Kind::Heimdall)));
        }
        // ` ran`, ` used Read`, `: subagent`, `: notification`: an action of its harness, one line.
        let what = tag.trim_start_matches(':').trim();
        return Some(Some((vec![clip(&format!("Heimdall {what}: {}", one_line(&text)), TOOL_CHARS + 40)], Kind::Tool)));
    }
    None
}

/// The stream is rotated past this many lines or bytes (review 31 Oct: it grew without limit and was read whole at
/// every start): the lines a turn has not carried stay, with `TAIL_LINES` before them.
pub const ROTATE_LINES: usize = 5_000;
pub const ROTATE_BYTES: u64 = 4 << 20;
/// Ids of rotated lines kept (`seen.ids`), newest last: enough that a session file read again repeats nothing.
pub const SEEN_KEEP: usize = 20_000;

/// `<data>/goals/stream.jsonl`, append-only between rotations, and `seen.ids` beside it (the ids of rotated lines).
pub struct Stream {
    pub path: PathBuf,
    /// Every entry id already rendered into the stream: the cursor.
    pub seen: HashSet<String>,
}

fn ids_path(stream: &Path) -> PathBuf { stream.with_file_name("seen.ids") }

impl Stream {
    /// The stream file and its lines, oldest first (a malformed line is skipped).
    pub fn open(path: &Path) -> Result<(Stream, Vec<Line>)> {
        let lines: Vec<Line> = match std::fs::read_to_string(path) {
            Ok(t) => t.lines().filter_map(|l| serde_json::from_str(l).ok()).collect(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e.into()),
        };
        let mut seen: HashSet<String> = lines.iter().map(|l| l.id.clone()).collect();
        if let Ok(t) = std::fs::read_to_string(ids_path(path)) { seen.extend(t.lines().filter(|l| !l.is_empty()).map(str::to_owned)) }
        Ok((Stream { path: path.into(), seen }, lines))
    }

    /// Whether the file is past `ROTATE_LINES` lines or `ROTATE_BYTES`.
    pub fn big(path: &Path, lines: usize) -> bool {
        lines > ROTATE_LINES || std::fs::metadata(path).is_ok_and(|m| m.len() > ROTATE_BYTES)
    }

    /// Rewrites the stream as `keep`; the ids of `dropped` go to `seen.ids` (its newest `SEEN_KEEP`), so they never
    /// come back.
    pub fn rotate(&mut self, keep: &[Line], dropped: &[Line]) -> Result<()> {
        let ids = ids_path(&self.path);
        let mut all: Vec<String> = std::fs::read_to_string(&ids).map(|t| t.lines().filter(|l| !l.is_empty()).map(str::to_owned).collect()).unwrap_or_default();
        all.extend(dropped.iter().map(|l| l.id.clone()));
        let from = all.len().saturating_sub(SEEN_KEEP);
        let tmp = ids.with_extension("ids.tmp");
        std::fs::write(&tmp, all[from..].join("\n") + "\n")?;
        std::fs::rename(&tmp, &ids)?;
        let mut buf = Vec::new();
        for l in keep { serde_json::to_writer(&mut buf, l)?; buf.push(b'\n'); }
        let tmp = self.path.with_extension("jsonl.tmp");
        std::fs::write(&tmp, buf)?;
        std::fs::rename(&tmp, &self.path)?;
        eprintln!("Goals: stream rotated: {} line(s) kept, {} set aside", keep.len(), dropped.len());
        Ok(())
    }

    /// The session's branch entries not seen yet, rendered and appended (those `upto` allows: an entry stamped after
    /// it waits; the lab replays a day by moving `upto`). Returns the new lines, oldest first.
    pub fn ingest(&mut self, entries: &[Value], read_at: i64, upto: Option<i64>) -> Result<Vec<Line>> {
        let mut out = Vec::new();
        let mut ids = HashSet::new();
        for e in branch(entries) {
            if upto.is_some_and(|u| entry_ms(e).is_some_and(|at| at > u)) { continue }
            let id = entry_id(e);
            if self.seen.contains(&id) || !ids.insert(id) { continue }
            if let Some(l) = render(e, read_at) { out.push(l) }
        }
        // Seen only once written: a failed append (a full disk) leaves them for the next read (review 31 Oct).
        if !out.is_empty() {
            if let Some(dir) = self.path.parent() { std::fs::create_dir_all(dir)? }
            let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&self.path)?;
            let mut buf = Vec::new();
            for l in &out { serde_json::to_writer(&mut buf, l)?; buf.push(b'\n'); }
            f.write_all(&buf)?;
        }
        self.seen.extend(ids);
        Ok(out)
    }
}
