//! The conversation the pulled-down notch shows ("literally a chat conversation between you and
//! me … not the actual context"), built from what Heimdall already stores, never a store of its own:
//!
//! - the user's words: the archive's `utterances` rows from his microphone (`source_id` `mic:…`, speaker the user) that the
//!   decider sent (`decision.outcome` `send`; a `wait` just before one joins it), and his typed messages
//!   (`[Heimdall typed message]` in the agent's session file). Other speakers, app audio and his talk with other
//!   people never appear.
//! - Heimdall: each finished reply (a session-file assistant message that stopped on its own, not the silent reply)
//!   and each `speak` line; marked when the user talked over it (`unheard-speech:cut:` notes), when it never reached him
//!   (`skipped-reply:` / `unheard-speech:skipped:` notes, a `speak` shown silently, or a live withdrawal).
//! - Actions: `delegate_task` started and its `terminal:` notice; an `ask` card and the option the user clicked (its
//!   `ask-…` notice) or the answer `ask_close` recorded; an `island_notice` with a detail (kept by id for pulling
//!   out).
//!
//! - Relay mode (docs/claude-relay.md, bug note 2026-10-28T19-12-56): the user's typed messages and voice wakes are held
//!   context (`emergency::HELD`) and the relay agent's messages and `speak` lines are mirrored context
//!   (`[Relay agent] …`), never user or assistant messages; both are read as turns. A voice wake the decider's model
//!   judged not for Heimdall, forced only by a trigger, that the relay left without a word or action until the user's
//!   next words a minute or more later, is not his turn.
//! - The relay's tool calls, live (bug note 30 Oct 03:00: the pull-down showed nothing while the relay worked): T3
//!   reports no tool events, but every Heimdall tool call the relay makes passes through `emergency::Own`, which hands
//!   it here (`relay_call`) with its name, arguments and result. The reader takes it as Pi's own call and result, so it
//!   makes the same rows a Pi turn's call makes, as soon as it happens; the `said to` record that reaches the session
//!   file later through the outbox does not add the line again.
//!
//! The session file (`agent/active-session.json`) is one file across restarts, so "today" is read from it from local
//! midnight; it is tailed by size afterwards. `start` runs the reader on its own thread and sends the conversation
//! whole (`UiUpdate::Chat`) whenever it changes.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use serde_json::Value;
use ui::agent_notice::Detail;
use ui::chat::{ActionKind, ChatEntry, ChatKind, Heard, NoteShape};

/// The most rows sent to the island (the oldest of today go first).
pub const MAX_ROWS: usize = 400;
/// How often the reader looks at the session file's size (a stat; nothing is read while it does not grow).
const POLL: Duration = Duration::from_millis(1500);
/// A `wait` utterance this close before the user's sent one is the start of the same request.
const WAIT_JOIN_MS: u64 = 30_000;
/// A held wake the relay answered nothing for this long, until the user's next words, was not taken as his turn.
const UNANSWERED_MS: u64 = 60_000;
/// How long after a relay `speak` shown live its outbox record may reach the session file and still be that line.
const RELAY_RECORD_MS: u64 = 15 * 60_000;

/// A tool call waiting for its result (the result carries the id the call was given).
#[derive(Clone, Debug)]
enum Call {
    Notice { title: String, detail: Detail, at_ms: u64 },
    Ask { question: String, at_ms: u64 },
    Delegate { title: String, at_ms: u64 },
    Speak { text: String, at_ms: u64 },
    AskClose { id: String, answer: String },
}

/// The conversation as read so far.
#[derive(Default)]
pub struct History {
    /// Rows from the session file, in its order.
    pi: Vec<ChatEntry>,
    /// The user's spoken words, by time.
    voice: Vec<ChatEntry>,
    calls: HashMap<String, Call>,
    /// Each `ask` row's place in `pi`, by request id.
    asks: HashMap<String, usize>,
    /// Each started task's place in `pi`, by its title.
    tasks: HashMap<String, usize>,
    /// Notices with a detail, by id: what pulling one out keeps.
    notes: HashMap<String, (String, Detail)>,
    /// Where the agent's current run began in `pi` (its last user-role entry): a reply repeating a `speak` line of
    /// the same run shows once.
    run_start: usize,
    /// The last entry's time, and the held voice wake the relay has not acted on yet (the entry before it, its time).
    last_ms: u64,
    held_wake: Option<(u64, u64)>,
    /// Held voice wakes the relay left unanswered (`UNANSWERED_MS`), as (the entry before, the wake).
    unanswered: Vec<(u64, u64)>,
    /// When the decider sent words its model judged not for Heimdall (a trigger forced the wake).
    forced: Vec<u64>,
    /// The relay's `speak` lines already shown live (`relay_call`), with their time: their `said to` record adds none.
    relay_said: Vec<(String, u64)>,
    /// The relay's `said to` records read before their live call (both can land in one look): that call adds none.
    relay_recorded: Vec<(String, u64)>,
    /// His typed messages shown the moment he sent them (`typed_pending`), until the session file has them.
    typed: Vec<(String, ChatEntry)>,
}

/// Local midnight of the day holding `ms`, in Unix milliseconds.
pub fn day_start(ms: u64) -> u64 {
    use chrono::{Local, TimeZone};
    let Some(at) = Local.timestamp_millis_opt(ms as i64).single() else { return 0 };
    let midnight = at.date_naive().and_hms_opt(0, 0, 0).expect("midnight exists");
    Local.from_local_datetime(&midnight).earliest().map_or(0, |m| m.timestamp_millis().max(0) as u64)
}

fn detail(v: &Value) -> Option<Detail> {
    let strings = |a: &Value| -> Option<Vec<String>> { a.as_array()?.iter().map(|x| x.as_str().map(str::to_owned)).collect() };
    match v["type"].as_str()? {
        "text" => Some(Detail::Text(v["text"].as_str()?.to_owned())),
        "table" => Some(Detail::Table { headers: strings(&v["headers"])?, rows: v["rows"].as_array()?.iter().map(strings).collect::<Option<_>>()? }),
        "svg" => Some(Detail::Svg(v["svg"].as_str()?.to_owned())),
        "image" => Some(Detail::Image { archive_id: v["archive_id"].as_str()?.to_owned() }),
        _ => None,
    }
}

pub fn shape(d: &Detail) -> NoteShape {
    match d {
        Detail::Text(_) => NoteShape::Text,
        Detail::Table { .. } => NoteShape::Table,
        Detail::Svg(_) => NoteShape::Svg,
        Detail::Image { .. } => NoteShape::Image,
    }
}

/// An entry's time: the message's own milliseconds, else its ISO timestamp.
fn entry_ms(e: &Value) -> u64 {
    if let Some(ms) = e["message"]["timestamp"].as_u64() {
        return ms;
    }
    e["timestamp"].as_str().and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok()).map_or(0, |t| t.timestamp_millis().max(0) as u64)
}

fn texts(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks.iter().filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    }
}

/// Whether `record` (a `said to` record's words, bounded with a trailing `…`) stands for the line `text`.
fn bounded_same(text: &str, record: &str) -> bool {
    let norm = |t: &str| t.split_whitespace().collect::<Vec<_>>().join(" ");
    let probe = norm(record.trim_end_matches('…'));
    !probe.is_empty() && norm(text).starts_with(&probe)
}

/// The question an `ask` call asked, for the chat: the legacy `question`, or every `questions` item's text joined.
fn ask_text(args: &Value) -> String {
    match args["questions"].as_array() {
        Some(items) => items.iter().filter_map(|i| i["question"].as_str()).map(str::trim).collect::<Vec<_>>().join(" "),
        None => args["question"].as_str().unwrap_or("").trim().to_string(),
    }
}

/// The first `prefix`-led token of `text` (an id such as `ask-…` or `notice-…`), trailing punctuation dropped.
fn token_after(text: &str, prefix: &str) -> Option<String> {
    let at = text.find(prefix)?;
    let id: String = text[at..].chars().take_while(|c| !c.is_whitespace() && *c != ']' && *c != ',').collect();
    Some(id.trim_end_matches('.').to_string())
}

/// The quoted words after `marker` in a notice (up to the next sentence that starts a new clause of the note).
fn quoted_after<'a>(text: &'a str, marker: &str) -> Option<&'a str> {
    let at = text.find(marker)? + marker.len();
    let rest = text[at..].trim_start();
    let end = [" He did not hear", "\n"].iter().filter_map(|m| rest.find(m)).min().unwrap_or(rest.len());
    let s = rest[..end].trim();
    (!s.is_empty()).then_some(s)
}

impl History {
    /// Reads one session-file entry.
    pub fn entry(&mut self, e: &Value, since_ms: u64) {
        let at_ms = entry_ms(e);
        match e["type"].as_str() {
            Some("message") => self.message(&e["message"], at_ms, since_ms),
            Some("custom_message") if at_ms >= since_ms => {
                let text = texts(&e["content"]);
                self.marks(&text);
                self.relay(&text, at_ms);
            }
            _ => {}
        }
        self.last_ms = at_ms.max(self.last_ms);
    }

    /// Relay mode's held input and mirrored relay agent (bug note 2026-10-28T19-12-56).
    fn relay(&mut self, text: &str, at_ms: u64) {
        if let Some(held) = text.strip_prefix(agent::emergency::HELD).map(str::trim_start) {
            let held = held.strip_prefix("[Heimdall delivery: ").and_then(|r| r.split_once('\n')).map_or(held, |(_, t)| t);
            let typed = held.split_once("[Heimdall typed message]\n").map(|(_, t)| t.trim());
            let wake = typed.is_none() && !held.starts_with("[Heimdall notice: ");
            if typed.is_some() || wake {
                if let Some((before, w)) = self.held_wake.take().filter(|(_, w)| at_ms.saturating_sub(*w) >= UNANSWERED_MS) {
                    self.unanswered.push((before, w));
                }
                self.run_start = self.pi.len();
            }
            match typed {
                Some(t) if !t.is_empty() => self.pi.push(ChatEntry { at_ms, kind: ChatKind::Owner(t.to_string()) }),
                Some(_) => {}
                None if wake => self.held_wake = Some((self.last_ms, at_ms)),
                None => self.notice(held, at_ms),
            }
            return;
        }
        let Some((id, line)) = text.strip_prefix("[Heimdall notice: ").and_then(|r| r.split_once("]\n")) else { return };
        if !id.starts_with(agent::notice::EMERGENCY_COVER) {
            return;
        }
        // Only words to the user take the wake before them: never an action line or a silent reply (NO_REPLY).
        // `said to <name>`: the user's configured name stands in for the user (`identity::names_only`).
        if let Some(said) = line.strip_prefix("[Relay agent] said to ").and_then(|r| r.split_once(": \"")).map(|(_, r)| r) {
            let Some((words, outcome)) = said.rsplit_once("\" → ") else { return };
            // A failed `speak` reached nobody (as Pi's failed call shows no row); one already shown live is that row.
            if outcome.starts_with("failed:") || self.shown_live(words, at_ms) { return; }
            if !words.trim().is_empty() { self.held_wake = None; }
            self.relay_recorded.push((words.to_string(), at_ms));
            let over = self.relay_recorded.len().saturating_sub(64);
            self.relay_recorded.drain(..over);
            self.heimdall(words, at_ms, if outcome.starts_with("Not spoken") { Heard::Never } else { Heard::Whole });
        } else if id.contains(":mirror:") {
            // A message over 64 KB comes in consecutive parts, whole: the later ones continue the first.
            let (body, part) = match line.rsplit_once("\n[part ") { Some((b, p)) if p.ends_with(']') => (b, p.split(' ').next().and_then(|n| n.parse::<u32>().ok())), _ => (line, None) };
            if let Some(reply) = body.strip_prefix("[Relay agent] ") {
                if !agent::reply::is_silent(reply) {
                    self.held_wake = None;
                    self.heimdall(reply, at_ms, Heard::Whole);
                }
            } else if part.is_some_and(|n| n > 1) {
                if let Some(ChatEntry { kind: ChatKind::Heimdall { text, .. }, .. }) = self.pi.last_mut() {
                    text.push_str(body);
                }
            }
        }
    }

    /// A Heimdall tool call the relay made (`emergency::Own`), read as Pi's own call and its result: the same rows.
    pub fn relay_call(&mut self, name: &str, args: &Value, result: &Result<String, String>, at_ms: u64) {
        let id = format!("relay-call:{}", uuid::Uuid::new_v4());
        self.tool_call(&serde_json::json!({"id": id, "name": name, "arguments": args}), at_ms);
        let Some(call) = self.calls.remove(&id) else { return };
        // Only words to the user take the held wake before them (as their record does, `relay`).
        if let (Call::Speak { text, .. }, Ok(_)) = (&call, result) {
            // Its record was read first (the outbox delivered it within the same look): the row is there.
            if let Some(k) = self.relay_recorded.iter().position(|(rec, at)| at_ms.abs_diff(*at) <= RELAY_RECORD_MS && bounded_same(text, rec)) {
                self.relay_recorded.remove(k);
                return;
            }
            if !text.is_empty() { self.held_wake = None; }
            self.relay_said.push((text.clone(), at_ms));
            let over = self.relay_said.len().saturating_sub(64);
            self.relay_said.drain(..over);
        }
        match result {
            Ok(text) => self.tool_result(call, text, false),
            Err(e) => self.tool_result(call, e, true),
        }
        self.last_ms = at_ms.max(self.last_ms);
    }

    /// Whether the relay's `said to` record (its words bounded, ending in `…`) is a line already shown live; that
    /// line is then spent, so the same words said again later still show.
    fn shown_live(&mut self, words: &str, at_ms: u64) -> bool {
        let found = self.relay_said.iter().position(|(text, at)| at_ms.abs_diff(*at) <= RELAY_RECORD_MS && bounded_same(text, words));
        found.map(|k| self.relay_said.remove(k)).is_some()
    }

    fn message(&mut self, m: &Value, at_ms: u64, since_ms: u64) {
        // A turn of Pi's own (the hand-back, relay off): no held wake waits on the relay any more.
        if matches!(m["role"].as_str(), Some("user" | "assistant")) { self.held_wake = None; }
        match m["role"].as_str() {
            Some("user") => {
                let text = texts(&m["content"]);
                if at_ms < since_ms {
                    return;
                }
                self.run_start = self.pi.len();
                if let Some(typed) = text.split_once("[Heimdall typed message]\n").map(|(_, t)| t.trim()) {
                    if !typed.is_empty() {
                        self.pi.push(ChatEntry { at_ms, kind: ChatKind::Owner(typed.to_string()) });
                    }
                    return;
                }
                self.marks(&text);
                self.notice(&text, at_ms);
            }
            Some("assistant") => {
                let blocks = m["content"].as_array().cloned().unwrap_or_default();
                for b in &blocks {
                    if b["type"] == "toolCall" {
                        self.tool_call(b, at_ms);
                    }
                }
                let stop = m["stopReason"].as_str().unwrap_or("");
                if at_ms >= since_ms && agent::reply::finished(stop) && !blocks.is_empty() {
                    let text = texts(&m["content"]);
                    if !agent::reply::is_silent(&text) {
                        self.heimdall(text.trim(), at_ms, Heard::Whole);
                    }
                }
            }
            Some("toolResult") => {
                let Some(id) = m["toolCallId"].as_str() else { return };
                let Some(call) = self.calls.remove(id) else { return };
                if at_ms < since_ms {
                    return;
                }
                let result = texts(&m["content"]);
                let failed = m["isError"].as_bool() == Some(true);
                self.tool_result(call, &result, failed);
            }
            _ => {}
        }
    }

    fn tool_call(&mut self, b: &Value, at_ms: u64) {
        let (Some(id), Some(name)) = (b["id"].as_str(), b["name"].as_str()) else { return };
        let args = &b["arguments"];
        let s = |k: &str| args[k].as_str().unwrap_or("").trim().to_string();
        let call = match name.rsplit("__").next().unwrap_or(name) {
            "island_notice" => match detail(&args["detail"]) {
                Some(detail) => Call::Notice { title: s("text"), detail, at_ms },
                None => return,
            },
            "ask" => Call::Ask { question: ask_text(args), at_ms },
            "delegate_task" => Call::Delegate { title: s("title"), at_ms },
            "speak" => Call::Speak { text: s("text"), at_ms },
            "ask_close" => Call::AskClose { id: s("request_id"), answer: s("answer") },
            _ => return,
        };
        self.calls.insert(id.to_string(), call);
    }

    fn tool_result(&mut self, call: Call, result: &str, failed: bool) {
        match call {
            Call::Notice { title, detail, at_ms } => {
                if failed {
                    return;
                }
                let Some(id) = token_after(result, "notice-").or_else(|| result.strip_prefix("Notice shown: ").map(|r| r.trim().trim_end_matches('.').to_string())) else { return };
                self.note(at_ms, &id, &title, detail);
            }
            Call::Ask { question, at_ms } => {
                if failed || question.is_empty() {
                    return;
                }
                let Some(id) = token_after(result, "ask-") else { return };
                self.asks.insert(id, self.pi.len());
                self.pi.push(ChatEntry { at_ms, kind: ChatKind::Action { kind: ActionKind::Question, text: question, answer: None } });
            }
            Call::Delegate { title, at_ms } => {
                if failed || !result.starts_with("Started") || title.is_empty() {
                    return;
                }
                self.tasks.insert(title.clone(), self.pi.len());
                self.pi.push(ChatEntry { at_ms, kind: ChatKind::Action { kind: ActionKind::Started, text: title, answer: None } });
            }
            Call::Speak { text, at_ms } => {
                if failed || text.is_empty() {
                    return;
                }
                // Shown silently on the island (the user was talking with other people): he never heard it.
                let heard = if result.starts_with("Not spoken") { Heard::Never } else { Heard::Whole };
                self.heimdall(&text, at_ms, heard);
            }
            Call::AskClose { id, answer } => {
                if !failed {
                    self.answer(&id, &answer);
                }
            }
        }
    }

    /// A Heimdall notice delivered as a user message: the user's click on a question, a task's end.
    fn notice(&mut self, text: &str, at_ms: u64) {
        let Some(id) = text.split_once("[Heimdall notice: ").and_then(|(_, r)| r.split_once(']')).map(|(id, _)| id.trim().to_string()) else { return };
        if id.starts_with("ask-") {
            // The user's name is the edition's (`agent::identity::for_agent`: `User`, a name, or "the user").
            if let Some(choice) = text.split_once(" clicked ").and_then(|(_, r)| r.split_once(". Question:").map(|(c, _)| c.trim().to_string())) {
                self.answer(&id, &choice);
            } else if text.lines().any(|l| l.starts_with("Question ") && l.contains(" answered ")) {
                // One line per question: `Question ask-…: User answered q1 "Which?": Warm and Dark`; the chat shows the picks.
                let picks: Vec<&str> = text.lines().filter_map(|l| l.rsplit_once("\": ").map(|(_, pick)| pick.trim())).collect();
                if !picks.is_empty() {
                    self.answer(&id, &picks.join("; "));
                }
            }
        } else if id.starts_with("terminal:") {
            // "Task '<title>' (<id>) completed." — failed, cancelled and stopped tasks end red.
            let Some((title, rest)) = text.split_once("Task '").and_then(|(_, r)| r.split_once("' (")) else { return };
            let state = rest.split_once(") ").map_or("", |(_, s)| s.split(['.', ' ']).next().unwrap_or(""));
            let kind = if state == "completed" { ActionKind::Finished } else { ActionKind::Failed };
            self.pi.push(ChatEntry { at_ms, kind: ChatKind::Action { kind, text: title.to_string(), answer: None } });
        }
    }

    /// Notes telling the agent what the user did not hear: the reply they quote is marked.
    fn marks(&mut self, text: &str) {
        // `bridge::tools::missed_lines` and `skipped_note`, as they are worded.
        for (marker, heard) in [("he heard only the start of:", Heard::Cut), (" talked over you before he heard:", Heard::Cut), ("with his new words:", Heard::Never)] {
            if let Some(words) = quoted_after(text, marker) {
                self.mark(words, heard, 0);
            }
        }
    }

    /// The newest of the last few replies (none older than `since_ms`) holding the start of `words` takes `heard` (a
    /// cut never undoes "never"). Returns whether one did.
    pub fn mark(&mut self, words: &str, heard: Heard, since_ms: u64) -> bool {
        let probe: String = words.split_whitespace().take(8).collect::<Vec<_>>().join(" ");
        if probe.is_empty() {
            return false;
        }
        for e in self.pi.iter_mut().rev().take(24).filter(|e| e.at_ms >= since_ms) {
            if let ChatKind::Heimdall { text, heard: h } = &mut e.kind {
                if ui::chat::holds(text, &probe) || ui::chat::holds(&probe, text) {
                    if *h != Heard::Never {
                        *h = heard;
                    }
                    return true;
                }
            }
        }
        false
    }

    pub fn heimdall(&mut self, text: &str, at_ms: u64, heard: Heard) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        // The same words again in the same run (a `speak` line the final reply repeats) show once.
        let same = self.pi[self.run_start.min(self.pi.len())..].iter().any(|e| matches!(&e.kind, ChatKind::Heimdall { text: t, .. } if ui::chat::same_text(t, text)));
        if !same {
            self.pi.push(ChatEntry { at_ms, kind: ChatKind::Heimdall { text: text.to_string(), heard } });
        }
    }

    pub fn note(&mut self, at_ms: u64, id: &str, title: &str, detail: Detail) {
        let shape = shape(&detail);
        self.notes.insert(id.to_string(), (title.to_string(), detail));
        if let Some(e) = self.pi.iter_mut().rev().find(|e| matches!(&e.kind, ChatKind::Note { id: i, .. } if i == id)) {
            e.kind = ChatKind::Note { id: id.to_string(), title: title.to_string(), shape };
            return;
        }
        self.pi.push(ChatEntry { at_ms, kind: ChatKind::Note { id: id.to_string(), title: title.to_string(), shape } });
    }

    fn answer(&mut self, id: &str, choice: &str) {
        if let Some(&i) = self.asks.get(id) {
            if let Some(ChatKind::Action { answer, .. }) = self.pi.get_mut(i).map(|e| &mut e.kind) {
                if answer.is_none() && !choice.trim().is_empty() {
                    *answer = Some(choice.trim().to_string());
                }
            }
        }
    }

    /// The user's spoken words to Heimdall from `utterances` rows (time, text, decision outcome), oldest first: each sent
    /// row, with the `wait` rows just before it joined in front.
    /// `forced` is a send its model judged not for Heimdall (`utterances`).
    pub fn voice_rows(rows: &[(u64, String, String)]) -> Vec<ChatEntry> {
        let mut out = vec![];
        let mut waiting: Vec<(u64, &str)> = vec![];
        for (at, text, outcome) in rows {
            match outcome.as_str() {
                "wait" => waiting.push((*at, text)),
                "send" | "forced" => {
                    waiting.retain(|(w, _)| at.saturating_sub(*w) <= WAIT_JOIN_MS);
                    let start = waiting.first().map_or(*at, |(w, _)| *w);
                    let words: Vec<&str> = waiting.drain(..).map(|(_, t)| t).chain([text.as_str()]).map(str::trim).filter(|t| !t.is_empty()).collect();
                    if !words.is_empty() {
                        out.push(ChatEntry { at_ms: start, kind: ChatKind::Owner(words.join(" ")) });
                    }
                }
                _ => waiting.clear(),
            }
        }
        out
    }

    /// Takes his typed messages sent since the last look and drops those the session file now holds (the same words,
    /// read at most `TYPED_SLACK_MS` before they were sent) or that waited `TYPED_WAIT_MS`.
    fn typed(&mut self, sent: Vec<(String, ChatEntry)>, failed: &[String], now_ms: u64) {
        self.typed.extend(sent);
        let pi = &self.pi;
        self.typed.retain(|(id, e)| {
            let ChatKind::Owner(words) = &e.kind else { return false };
            let read = pi.iter().rev().take(64).any(|p| p.at_ms + TYPED_SLACK_MS >= e.at_ms && matches!(&p.kind, ChatKind::Owner(t) if t.trim() == words.trim()));
            !read && !failed.contains(id) && now_ms < e.at_ms + TYPED_WAIT_MS
        });
    }

    /// The conversation from `since_ms`, oldest first, at most `MAX_ROWS`.
    pub fn rows(&self, since_ms: u64) -> Vec<ChatEntry> {
        // A forced wake the relay left unanswered: the spoken row holding it is not the user's turn.
        let skip: Vec<u64> = self.forced.iter().filter(|f| self.unanswered.iter().any(|(before, w)| before < *f && *f <= w))
            .filter_map(|f| self.voice.iter().rev().find(|e| e.at_ms <= *f).map(|e| e.at_ms)).collect();
        let voice = self.voice.iter().filter(|e| !skip.contains(&e.at_ms));
        let typed = self.typed.iter().map(|(_, e)| e);
        let mut all: Vec<ChatEntry> = self.pi.iter().chain(voice).chain(typed).filter(|e| e.at_ms >= since_ms).cloned().collect();
        all.sort_by_key(|e| e.at_ms);
        let cut = all.len().saturating_sub(MAX_ROWS);
        all.drain(..cut);
        all
    }

}

// ---------------------------------------------------------------- the live reader

struct Live {
    history: History,
    /// The session file read so far, and where the next read starts.
    session: Option<PathBuf>,
    offset: u64,
    partial: Vec<u8>,
    /// Today's local midnight.
    day: u64,
    sent: Vec<ChatEntry>,
}

/// Notices with a detail kept for pulling out, by id, oldest first: today's from the session file and every one shown
/// since launch. Their own small lock (the reader's file and database work never holds it), never cleared by a new
/// day or a rewritten session file, at most `MAX_NOTES`.
const MAX_NOTES: usize = 300;
fn notes() -> &'static Mutex<Vec<(String, String, Detail)>> {
    static NOTES: OnceLock<Mutex<Vec<(String, String, Detail)>>> = OnceLock::new();
    NOTES.get_or_init(|| Mutex::new(vec![]))
}

fn keep_note(id: &str, title: &str, detail: &Detail) {
    let mut n = notes().lock().unwrap_or_else(|e| e.into_inner());
    n.retain(|(i, _, _)| i != id);
    n.push((id.to_string(), title.to_string(), detail.clone()));
    let over = n.len().saturating_sub(MAX_NOTES);
    n.drain(..over);
}

/// A notice's title and detail by id: what pulling it out keeps.
pub fn note_detail(id: &str) -> Option<(String, Detail)> {
    notes().lock().unwrap_or_else(|e| e.into_inner()).iter().rev().find(|(i, _, _)| i == id).map(|(_, t, d)| (t.clone(), d.clone()))
}

/// A notice with a detail shown on the island now (`mcp_actions::Sink::notice`): kept for pulling out at once, before
/// the session file records it.
pub fn shown_note(id: &str, title: &str, detail: &Detail) {
    keep_note(id, title, detail);
}

/// The relay's tool calls not yet read (name, arguments, result, when), at most `MAX_RELAY_CALLS`.
type RelayCall = (String, Value, Result<String, String>, u64);
const MAX_RELAY_CALLS: usize = 200;
fn relay_calls() -> &'static Mutex<Vec<RelayCall>> {
    static CALLS: OnceLock<Mutex<Vec<RelayCall>>> = OnceLock::new();
    CALLS.get_or_init(|| Mutex::new(vec![]))
}

/// A Heimdall tool call the relay just made (`emergency::AppSide::tool_call`): the reader shows it on its next look,
/// as the rows Pi's own call would make.
pub fn relay_call(name: &str, args: &Value, result: &Result<String, String>, at_ms: u64) {
    let mut calls = relay_calls().lock().unwrap_or_else(|e| e.into_inner());
    calls.push((name.to_string(), args.clone(), result.clone(), at_ms));
    let over = calls.len().saturating_sub(MAX_RELAY_CALLS);
    calls.drain(..over);
}

/// His typed messages sent and not yet read by the reader (id, row), and the IDs of those whose send failed.
type TypedSent = (Vec<(String, ChatEntry)>, Vec<String>);
#[cfg(not(test))]
fn with_typed<R>(f: impl FnOnce(&mut TypedSent) -> R) -> R {
    static T: OnceLock<Mutex<TypedSent>> = OnceLock::new();
    f(&mut T.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner()))
}
/// Tests run side by side: each thread its own (a typed send in one must not show in another's reader).
#[cfg(test)]
fn with_typed<R>(f: impl FnOnce(&mut TypedSent) -> R) -> R {
    thread_local! { static T: std::cell::RefCell<TypedSent> = Default::default(); }
    T.with(|t| f(&mut t.borrow_mut()))
}
/// How long a typed row waits for the session file before it goes (a send that never reached it).
const TYPED_WAIT_MS: u64 = 10 * 60_000;
/// The session file's copy of a typed message may carry a time this much before the moment it was sent.
const TYPED_SLACK_MS: u64 = 5_000;
/// Wakes the reader at once (it otherwise looks every `POLL`).
fn wake() -> &'static (crossbeam_channel::Sender<()>, crossbeam_channel::Receiver<()>) {
    static W: OnceLock<(crossbeam_channel::Sender<()>, crossbeam_channel::Receiver<()>)> = OnceLock::new();
    W.get_or_init(|| crossbeam_channel::bounded(1))
}

/// The user just sent `text` (`Tools::window_action`): the pulled-down notch shows it now, before the agent has it (bug
/// note 2026-11-01T00-20-27); the session file's copy takes its place.
pub fn typed_pending(id: &str, text: &str) {
    let text = text.trim();
    if text.is_empty() {
        return;
    }
    with_typed(|t| t.0.push((id.to_string(), ChatEntry { at_ms: agent::now_ms(), kind: ChatKind::Owner(text.to_string()) })));
    let _ = wake().0.try_send(());
}
/// The typed message `id` was not sent after all: its row goes.
pub fn typed_failed(id: &str) {
    with_typed(|t| t.1.push(id.to_string()));
    let _ = wake().0.try_send(());
}

/// Replies withdrawn live (their words and when), waiting for their record in the session file.
fn withdrawals() -> &'static Mutex<Vec<(String, u64)>> {
    static W: OnceLock<Mutex<Vec<(String, u64)>>> = OnceLock::new();
    W.get_or_init(|| Mutex::new(vec![]))
}
/// How long a withdrawal waits for its reply's record.
const WITHDRAW_WAIT_MS: u64 = 120_000;

/// The reply with `text` never reached the user after all (`Tools::withdraw_final`): marked so once its record is read
/// (it usually is not yet: the reader looks every `POLL`).
pub fn withdrawn(text: &str) {
    withdrawals().lock().unwrap_or_else(|e| e.into_inner()).push((text.to_string(), agent::now_ms()));
}

fn session_path(data: &Path) -> Option<PathBuf> {
    let pointer = data.join("agent/active-session.json");
    let path: PathBuf = serde_json::from_slice(&std::fs::read(pointer).ok()?).ok()?;
    path.starts_with(data).then_some(path)
}

/// The user's addressed utterances from `after_ms` (exclusive), oldest first: (time, text, outcome, `forced` for a send
/// its model judged not for Heimdall).
fn utterances(archive: &Path, after_ms: u64) -> Vec<(u64, String, String)> {
    let path = archive.join("heimdall.sqlite");
    let Ok(db) = rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX) else { return vec![] };
    let _ = db.busy_timeout(Duration::from_millis(500));
    let Ok(mut q) = db.prepare(
        "SELECT wall_ms, text, CASE WHEN json_extract(decision,'$.outcome')='send' AND json_extract(decision,'$.choice')='not_for_heimdall' \
         THEN 'forced' ELSE coalesce(json_extract(decision,'$.outcome'),'') END FROM utterances \
         WHERE wall_ms > ?1 AND speaker = 'User' AND source_id LIKE 'mic:%' ORDER BY wall_ms",
    ) else {
        return vec![];
    };
    q.query_map([after_ms as i64], |r| Ok((r.get::<_, i64>(0)?.max(0) as u64, r.get::<_, Option<String>>(1)?.unwrap_or_default(), r.get::<_, String>(2)?)))
        .map(|rows| rows.flatten().collect())
        .unwrap_or_default()
}

impl Live {
    /// Reads what was added since the last look; returns whether the conversation may have changed.
    fn refresh(&mut self, data: &Path, now_ms: u64) -> bool {
        let day = day_start(now_ms);
        let path = session_path(data);
        let size = path.as_ref().and_then(|p| std::fs::metadata(p).ok()).map_or(0, |m| m.len());
        // A new day, another session file, or one rewritten shorter (rotated past its size limit): read it again.
        if day != self.day || path != self.session || size < self.offset {
            self.history = History::default();
            self.offset = 0;
            self.partial.clear();
            self.session = path.clone();
            self.day = day;
        }
        // The relay's calls are read after the file (its held input, which starts the run, is in the file first).
        let relay = std::mem::take(&mut *relay_calls().lock().unwrap_or_else(|e| e.into_inner()));
        let (typed, failed) = with_typed(std::mem::take);
        let typed_changed = !typed.is_empty() || !failed.is_empty() || !self.history.typed.is_empty();
        if size == self.offset && relay.is_empty() {
            self.history.typed(typed, &failed, now_ms);
            return typed_changed;
        }
        if let Some(p) = path.as_ref().filter(|_| size != self.offset) {
            if let Ok(mut f) = std::fs::File::open(p) {
                if f.seek(SeekFrom::Start(self.offset)).is_ok() {
                    let mut buf = vec![];
                    if let Ok(n) = f.take(size - self.offset).read_to_end(&mut buf) {
                        self.offset += n as u64;
                        self.partial.extend_from_slice(&buf);
                        let complete = self.partial.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
                        let lines: Vec<u8> = self.partial.drain(..complete).collect();
                        for line in lines.split(|b| *b == b'\n').filter(|l| !l.is_empty()) {
                            if let Ok(e) = serde_json::from_slice::<Value>(line) {
                                self.history.entry(&e, self.day);
                            }
                        }
                    }
                }
            }
        }
        for (name, args, result, at_ms) in &relay {
            if *at_ms >= self.day {
                self.history.relay_call(name, args, result, *at_ms);
            }
        }
        self.history.typed(typed, &failed, now_ms);
        // His words: re-read the recent ones whole (a `wait` may be joined by the `send` that follows it).
        let rows = utterances(&data.join("archive"), self.day.saturating_sub(1));
        self.history.voice = History::voice_rows(&rows);
        self.history.forced = rows.iter().filter(|(_, _, o)| o == "forced").map(|(at, _, _)| *at).collect();
        // Notices read, kept for pulling out; withdrawals whose reply is in now (or that waited too long), applied.
        for (id, (title, detail)) in self.history.notes.drain() {
            keep_note(&id, &title, &detail);
        }
        let pending = std::mem::take(&mut *withdrawals().lock().unwrap_or_else(|e| e.into_inner()));
        let left: Vec<(String, u64)> = pending.into_iter().filter(|(text, at)| !self.history.mark(text, Heard::Never, at.saturating_sub(WITHDRAW_WAIT_MS)) && now_ms < at + WITHDRAW_WAIT_MS).collect();
        withdrawals().lock().unwrap_or_else(|e| e.into_inner()).extend(left);
        true
    }
}

/// Starts the reader: today's conversation now, then whenever the session file grows, sent to the island whole. The
/// reader's state is its thread's own.
pub fn start(data: &Path, ui: ui::Ui) {
    let data = data.to_path_buf();
    let mut l = Live { history: History::default(), session: None, offset: 0, partial: vec![], day: 0, sent: vec![] };
    let spawned = std::thread::Builder::new().name("heimdall-chat".into()).spawn(move || loop {
        l.refresh(&data, agent::now_ms());
        let rows = l.history.rows(l.day);
        if rows != l.sent {
            l.sent = rows.clone();
            ui.send(ui::UiUpdate::Chat(rows));
        }
        let _ = wake().1.recv_timeout(POLL);
    });
    if let Err(e) = spawned {
        eprintln!("island: the conversation reader did not start: {e}");
    }
}

/// Shares nothing with tests' threads: a test's own reader over `data`.
#[cfg(test)]
fn read_once(data: &Path, now_ms: u64) -> Vec<ChatEntry> {
    let mut l = Live { history: History::default(), session: None, offset: 0, partial: vec![], day: 0, sent: vec![] };
    l.refresh(data, now_ms);
    l.history.rows(l.day)
}

#[cfg(test)]
#[path = "chat_history_tests.rs"]
mod tests;
