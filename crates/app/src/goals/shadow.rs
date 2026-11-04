//! The shadow's engine, the same in the app and in `goals-lab replay`: read new session entries into the stream, fire
//! on the triggers, build the turn (system, the segment's turns, the new user turn: stable lines first), take the
//! model's answer, check and apply its ops, keep the turn, compact. No model call and no clock of its own: the caller
//! passes every time (epoch ms) and makes the call, so a replay runs a day in simulated time with the real code.

use crate::goals::ops::{self, Outcome};
use crate::goals::segment::{self, Segment, Turn, COMPACT_TOKENS, TAIL_LINES};
use crate::goals::stream::{Line, Stream};
use crate::goals::trigger::{Triggers, Why};
use crate::islands::files::Root;
use anyhow::Result;
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

/// The system prompt (`prompt/goals.md`). The public edition has its own copy: no the user, no seed goals of his.
#[cfg(not(feature = "public"))]
pub const SYSTEM: &str = include_str!("../../prompt/goals.md");
#[cfg(feature = "public")]
pub const SYSTEM: &str = include_str!("../../prompt/public/goals.md");

/// A failed call's lines go again this long after (a network or server failure costs nothing), doubling with each
/// failure in a row up to `RETRY_MAX_MS`.
pub const RETRY_MS: i64 = 120_000;
pub const RETRY_MAX_MS: i64 = 30 * 60_000;
/// One turn carries at most this many of the newest lines and bytes (a night of notices while the model was away);
/// older ones are set aside, logged (review 31 Oct: a turn had no cap).
pub const TURN_LINES: usize = 400;
pub const TURN_BYTES: usize = 60_000;

/// The system prompt and the schema: what every call carries before its turns.
pub fn fixed_bytes() -> usize { SYSTEM.len() + ops::schema().to_string().len() }

/// One call to make.
#[derive(Clone, Debug)]
pub struct Request {
    pub why: Why,
    pub at: i64,
    pub seg: String,
    /// The rolling session's key (`Call::session`).
    pub key: String,
    pub turn: usize,
    pub history: Vec<Turn>,
    /// The new turn: its stable part (the header on a first turn, the new lines), then the Goals now and the clock.
    pub stable: String,
    pub volatile: String,
    pub lines: Vec<Line>,
    /// What the answer must account for (`ledger`): the completed task notices, the open asks (every turn).
    pub ledger: crate::goals::ledger::Ledger,
}

impl Request {
    pub fn user(&self) -> String { format!("{}{}", self.stable, self.volatile) }
    pub fn lines_text(&self) -> String { self.lines.iter().map(Line::text).collect::<Vec<_>>().join("\n") }
    /// What the ops are checked against: every new line, and the user's own (his words, notices of his clicks).
    pub fn evidence(&self) -> ops::Evidence {
        let owner = self.lines.iter().filter(|l| l.kind == crate::goals::stream::Kind::Owner || crate::goals::stream::owners_click(l))
            .map(Line::text).collect::<Vec<_>>().join("\n");
        let times = self.lines.iter().map(|l| (l.text(), l.at / 1000)).collect();
        ops::Evidence { all: self.lines_text(), owner, times, removed: Vec::new(), recorded: Vec::new() }
    }
}

/// How a call ended, for `Shadow::settle`.
pub enum Answer {
    Text(String),
    /// Failed without an answer and without cost (network, server, backend down): the lines go again later.
    Retry(String),
    /// Refused at the gate (Agent off, emergency mode's pause): the lines wait, and go when it opens (no backoff).
    Paused,
    /// Paid for without a usable answer (refused, cut off, over the limit): the lines wait for the next event.
    Settled(String),
}

/// What one turn did.
#[derive(Clone, Debug)]
pub struct Done {
    pub outcomes: Vec<Outcome>,
    pub goals: Map<String, Value>,
    /// The segment's estimated tokens after this turn.
    pub tokens: u64,
    /// The old segment's key when this turn compacted (end its session).
    pub ended: Option<String>,
    /// The answer was not `{"ops": [...]}` (logged; nothing applied).
    pub malformed: Option<String>,
}

pub struct Shadow {
    pub dir: PathBuf,
    pub root: Root,
    pub stream: Stream,
    /// Stream lines no turn carried yet.
    pub pending: Vec<Line>,
    /// The last lines turns carried (a new segment's header).
    pub recent: Vec<Line>,
    pub triggers: Triggers,
    pub segment: Segment,
    pub compact_tokens: u64,
    /// No stream and no segment yet (the first start, a lab replay): the first lines read are the past, not news;
    /// they become the header's earlier lines.
    pub fresh: bool,
    /// Calls that failed in a row (the retry's backoff).
    pub failures: u32,
}

impl Shadow {
    /// `<data>/goals` and the islands folder. Seeds and migrates the Goals file on the way (`ops::ensure`).
    pub fn open(dir: &Path, root: &Root, now: i64, seed: bool) -> Result<Shadow> {
        for note in ops::ensure(root, now / 1000, seed)? { eprintln!("Goals: {note}") }
        let (mut stream, all) = Stream::open(&dir.join("stream.jsonl"))?;
        let saved = Segment::load(&dir.join("segment.json"))?;
        let fresh = all.is_empty() && saved.is_none();
        let (segment, at) = match saved {
            Some(s) => {
                let at = s.sent.as_ref().and_then(|id| all.iter().position(|l| &l.id == id)).map(|i| i + 1);
                (s, at)
            }
            None => {
                let goals = root.read(ops::ID).unwrap_or_else(ops::fresh);
                let tail = &all[all.len().saturating_sub(TAIL_LINES)..];
                (Segment::new(now, segment::header(&goals, tail), all.last().map(|l| l.id.clone())), Some(all.len()))
            }
        };
        // A segment whose last line is gone from the stream: only the newest lines are news.
        let at = at.unwrap_or(all.len().saturating_sub(40));
        let recent = all[at.saturating_sub(TAIL_LINES)..at].to_vec();
        let pending = all[at..].to_vec();
        if Stream::big(&stream.path, all.len()) {
            let cut = at.saturating_sub(TAIL_LINES);
            stream.rotate(&all[cut..], &all[..cut])?;
        }
        let mut triggers = Triggers::default();
        for l in &pending { triggers.note(l.kind, l.at) }
        Ok(Shadow { dir: dir.into(), root: root.clone(), stream, pending, recent, triggers, segment, compact_tokens: COMPACT_TOKENS, fresh, failures: 0 })
    }

    /// New session entries (those stamped by `upto` when given) into the stream; returns how many lines came. On a
    /// fresh start they are the past: the last of them open the first segment's header, none is news.
    pub fn ingest(&mut self, entries: &[Value], now: i64, upto: Option<i64>) -> Result<usize> {
        let new = self.stream.ingest(entries, now, upto)?;
        if self.fresh && !new.is_empty() {
            self.fresh = false;
            self.recent = new[new.len().saturating_sub(TAIL_LINES)..].to_vec();
            let goals = self.root.read(ops::ID).unwrap_or_else(ops::fresh);
            self.segment = Segment::new(self.segment.started, segment::header(&goals, &self.recent), self.recent.last().map(|l| l.id.clone()));
            self.save()?;
            return Ok(0);
        }
        for l in &new { self.triggers.note(l.kind, l.at) }
        let n = new.len();
        self.pending.extend(new);
        Ok(n)
    }

    /// Lines that came while the job was off are not news when it comes back on.
    pub fn forget_pending(&mut self) {
        self.recent.append(&mut self.pending);
        let over = self.recent.len().saturating_sub(TAIL_LINES);
        self.recent.drain(..over);
        self.triggers = Triggers::default();
        let sent = self.recent.last().map(|l| l.id.clone());
        if sent.is_some() && sent != self.segment.sent {
            self.segment.sent = sent;
            if let Err(e) = self.save() { eprintln!("Goals: segment not saved: {e:#}") }
        }
    }

    /// The request's lines leave `pending` for `recent`; the segment's cursor moves past them.
    fn carried(&mut self, req: &Request) {
        let n = req.lines.len().min(self.pending.len());
        let sent: Vec<Line> = self.pending.drain(..n).collect();
        if let Some(l) = sent.last() { self.segment.sent = Some(l.id.clone()) }
        self.recent.extend(sent);
        let over = self.recent.len().saturating_sub(TAIL_LINES);
        self.recent.drain(..over);
    }

    /// A new segment from the Goals as they are and the last lines; returns the old one's key (end its session).
    fn restart(&mut self, now: i64) -> String {
        let goals = self.root.read(ops::ID).unwrap_or_else(ops::fresh);
        let old = self.segment.key();
        self.segment = Segment::new(now, segment::header(&goals, &self.recent), self.segment.sent.clone());
        old
    }

    fn save(&self) -> Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        self.segment.save(&self.dir.join("segment.json"))
    }

    pub fn due(&self) -> Option<i64> { if self.pending.is_empty() { None } else { self.triggers.due() } }

    /// The call due at `now`, if one is. Only the newest `TURN_LINES` lines and `TURN_BYTES` bytes go; older ones
    /// are set aside.
    pub fn request(&mut self, now: i64) -> Option<Request> {
        if self.pending.is_empty() { return None }
        let why = self.triggers.fire(now)?;
        let mut keep = 0;
        let mut bytes = 0;
        for l in self.pending.iter().rev() {
            bytes += l.lines.iter().map(|t| t.len() + 7).sum::<usize>();
            if keep >= TURN_LINES || bytes > TURN_BYTES && keep > 0 { break }
            keep += 1;
        }
        let over = self.pending.len() - keep;
        if over > 0 {
            eprintln!("Goals: {over} older line(s) set aside; this turn carries the newest {keep}");
            let aside: Vec<Line> = self.pending.drain(..over).collect();
            if let Some(l) = aside.last() { self.segment.sent = Some(l.id.clone()) }
            self.recent.extend(aside);
            let o = self.recent.len().saturating_sub(TAIL_LINES);
            self.recent.drain(..o);
        }
        let goals = self.root.read(ops::ID).unwrap_or_else(ops::fresh);
        let first = self.segment.turns.is_empty();
        let ledger = crate::goals::ledger::build(&self.pending, &goals);
        Some(Request {
            why,
            at: now,
            seg: self.segment.id.clone(),
            key: self.segment.key(),
            turn: self.segment.turns.len(),
            history: self.segment.turns.clone(),
            stable: segment::stable(first.then_some(self.segment.header.as_str()), &self.pending),
            volatile: format!("{}{}", segment::volatile(&goals, now), crate::goals::ledger::text(&ledger)),
            lines: self.pending.clone(),
            ledger,
        })
    }

    /// The call's end. An answer is checked and applied, kept as the segment's next turn, and the segment compacted
    /// when it grew past `compact_tokens`; a failure keeps the lines for later. `Ok(None)`: nothing applied.
    pub fn settle(&mut self, req: &Request, answer: Answer, now: i64) -> Result<Option<Done>> {
        let text = match answer {
            Answer::Text(t) => t,
            Answer::Retry(e) => {
                let wait = (RETRY_MS << self.failures.min(10)).min(RETRY_MAX_MS);
                self.failures += 1;
                eprintln!("Goals: call failed ({e}); its lines go again in {} s", wait / 1000);
                self.triggers.retry_in(now, wait);
                return Ok(None);
            }
            Answer::Paused => {
                self.triggers.retry_in(now, 0);
                return Ok(None);
            }
            // Paid for, or refused before the call (over the $0.50 limit): its lines are not sent again, and a segment that
            // has turns starts over, so a context grown past what a run may cost never stalls the job (review 31 Oct).
            Answer::Settled(e) => {
                self.failures = 0;
                eprintln!("Goals: no answer ({e}); its lines are set aside and the context starts over");
                self.carried(req);
                let ended = (!self.segment.turns.is_empty()).then(|| self.restart(now));
                self.save()?;
                let goals = self.root.read(ops::ID).unwrap_or_else(ops::fresh);
                return Ok(Some(Done { outcomes: Vec::new(), goals, tokens: self.segment.tokens(fixed_bytes()), ended, malformed: Some(e) }));
            }
        };
        self.failures = 0;
        let (ops, malformed, kept) = match ops::parse(&text) {
            Ok(o) => {
                // The ledger's entries first (a done task per finished notice, the asks the user settled), then the model's ops.
                let v = ops::parse_value(&text).unwrap_or_default();
                let (mut all, missing) = crate::goals::ledger::ops(&v, &req.ledger);
                if !missing.is_empty() { eprintln!("Goals: the answer left out {}", missing.join(", ")) }
                let own = crate::goals::ledger::drop_duplicates(o, &all);
                all.extend(own);
                (all, None, text.trim().to_owned())
            }
            // The turn still goes into the context, as an empty answer: the next turn follows a well-formed history.
            Err(e) => (Vec::new(), Some(format!("{e:#}")), json!({"ops": []}).to_string()),
        };
        let (outcomes, goals) = ops::apply_file(&self.root, &ops, &req.evidence(), now / 1000)?;
        self.segment.turns.push(Turn { user: req.stable.clone(), volatile: req.volatile.clone(), assistant: kept, at: now });
        self.carried(req);
        let tokens = self.segment.tokens(fixed_bytes());
        let ended = (tokens > self.compact_tokens).then(|| self.restart(now));
        // A long run: the stream rotates as it would at a start.
        if std::fs::metadata(&self.stream.path).is_ok_and(|m| m.len() > crate::goals::stream::ROTATE_BYTES) {
            let (_, all) = Stream::open(&self.stream.path)?;
            let at = self.segment.sent.as_ref().and_then(|id| all.iter().position(|l| &l.id == id)).map_or(all.len(), |i| i + 1);
            let cut = at.saturating_sub(TAIL_LINES);
            self.stream.rotate(&all[cut..], &all[..cut])?;
        }
        self.save()?;
        Ok(Some(Done { outcomes, goals, tokens, ended, malformed }))
    }
}

/// The runner's request (`goals-lab`): the call as JSON.
pub fn runner_input(req: &Request, effort: &str) -> Value {
    json!({
        "session": req.key, "system": SYSTEM, "schema": ops::schema(),
        "history": req.history.iter().map(|t| json!({"user": format!("{}{}", t.user, t.volatile), "assistant": t.assistant})).collect::<Vec<_>>(),
        "user": req.user(), "effort": effort,
    })
}
