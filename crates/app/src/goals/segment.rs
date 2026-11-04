//! The shadow's rolling context (plan, 31 Oct; the one exception to the 30 Oct stateless rule, docs/goals.md): a
//! segment is the system prompt, then turns, each turn's user message the new stream lines since the last turn, the
//! Goals as they are now and the clock, its assistant message the model's ops. The first turn opens with the segment's
//! header (the Goals as they stood when it began, with their finished tasks, and the last lines before it), so the
//! prefix only grows and every call pays for its new lines while the rest reads from cache. Past `COMPACT_TOKENS`
//! (under Claude Haiku 5.5's 100k-token price step) a new segment starts. Kept in `<data>/goals/segment.json`.

use crate::goals::stream::Line;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::path::Path;

/// A segment passing this many estimated tokens is compacted after its turn. The estimate runs 25-50 % under the real
/// count (climb, 31 Oct: 64,590 estimated was 95,501 real through Claude Code), so 40,000 keeps the next call, with a
/// full turn of new lines (`shadow::TURN_BYTES`) on top, under Claude Haiku 5.5's 100k-token price step (5x above it).
pub const COMPACT_TOKENS: u64 = 40_000;
/// The lines before a new segment its header carries.
pub const TAIL_LINES: usize = 30;

/// Tokens from bytes, over-estimated (about 3 bytes a token: English is nearer 4, JSON, ids and Polish nearer 3).
pub fn tokens(bytes: usize) -> u64 { (bytes as u64).div_ceil(3) }

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Turn {
    /// The turn's stable part (header, new lines): resent as one cached stretch.
    pub user: String,
    /// Its volatile part (the Goals now, the clock), kept apart so the resent turn has the same blocks as the call
    /// that sent it first and the provider's prefix cache still matches (review 31 Oct: joined, every call missed one
    /// turn and paid its write twice).
    #[serde(default)]
    pub volatile: String,
    pub assistant: String,
    /// When it was answered, epoch ms.
    #[serde(default)]
    pub at: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Segment {
    pub id: String,
    /// Epoch ms.
    pub started: i64,
    pub header: String,
    pub turns: Vec<Turn>,
    /// The id of the last stream line a turn carried: the next turn starts after it.
    pub sent: Option<String>,
}

impl Segment {
    pub fn new(started: i64, header: String, sent: Option<String>) -> Segment {
        Segment { id: format!("s{started}"), started, header, turns: Vec::new(), sent }
    }
    /// The rolling session's key (`background_call::Call::session`: on T3 one thread per key).
    pub fn key(&self) -> String { format!("goals-{}", self.id) }
    /// What the next call resends before its new turn, estimated: the system prompt and schema, the turns.
    pub fn tokens(&self, fixed: usize) -> u64 {
        tokens(fixed + self.turns.iter().map(|t| t.user.len() + t.volatile.len() + t.assistant.len()).sum::<usize>())
    }
    pub fn load(path: &Path) -> Result<Option<Segment>> {
        match std::fs::read(path) {
            Ok(b) => Ok(serde_json::from_slice(&b).ok()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
    pub fn save(&self, path: &Path) -> Result<()> {
        crate::islands::files::write_atomic(path, &serde_json::to_value(self)?)
    }
}

/// `HH:MM` local.
pub fn hhmm(ms: i64) -> String {
    use chrono::TimeZone;
    chrono::Local.timestamp_millis_opt(ms).single().map(|t| t.format("%H:%M").to_string()).unwrap_or_default()
}

/// The lines as the model reads them: each entry's time before its first line.
pub fn lines_text(lines: &[Line]) -> String {
    let mut out = String::new();
    for l in lines {
        for (i, t) in l.lines.iter().enumerate() {
            if i == 0 { out.push_str(&hhmm(l.at)); out.push(' '); }
            out.push_str(t);
            out.push('\n');
        }
    }
    out
}

/// A new segment's header: the Goals with every task (the finished ones are the memory) and the last lines.
pub fn header(goals: &Map<String, Value>, tail: &[Line]) -> String {
    let mut h = format!("<goals_at_start note=\"the Goals as they stood when this context began\">\n{}</goals_at_start>\n\n", crate::goals::ops::compact(goals, true));
    if !tail.is_empty() {
        h.push_str(&format!("<earlier_lines note=\"the last lines before this context began; already handled\">\n{}</earlier_lines>\n\n", lines_text(tail)));
    }
    h
}

/// A turn's stable part: the header on a segment's first turn, then the new lines.
pub fn stable(header: Option<&str>, lines: &[Line]) -> String {
    format!("{}<new_lines>\n{}</new_lines>\n\n", header.unwrap_or(""), lines_text(lines))
}

/// A turn's volatile part: the Goals now and the local clock.
pub fn volatile(goals: &Map<String, Value>, now_ms: i64) -> String {
    use chrono::TimeZone;
    let clock = chrono::Local.timestamp_millis_opt(now_ms).single().map(|t| t.format("%A %Y-%m-%d %H:%M").to_string()).unwrap_or_default();
    format!("<goals_now>\n{}</goals_now>\n\nNow: {clock}\n", crate::goals::ops::compact(goals, false))
}
