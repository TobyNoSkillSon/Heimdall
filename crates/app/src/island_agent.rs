//! The island agent: "background jobs already run one call per island. Keep that shape. Add the
//! code-driven updates, and use them to wake the island agent at the right moments, because its point is to make the
//! island rich: little graphics, a loss curve going down for training, a screenshot, animation, not just text."
//!
//! Code owns a bound island's state, stage, clock, progress and the band (helper rows and the stage ribbon), written
//! by the binding within one T3 pass (`islands::stages`). This job draws what code cannot choose: the now line, the
//! moment's graphic, the icon and title, the helpers' short names and the open target. Its instructions are the final
//! `Assistant/Plans/island-design/agent-prompt.md` (the user 03:37: "use them as they are"), its contract the island-design
//! README's: one user message `<signals>{…}</signals>` (task, brief, now, elapsed_min, state_hint, stage, history,
//! recent, helpers, counters with the latest picture and its age, question, owner, metrics, previous_island), one JSON
//! island back (title, icon, state, stage, summary, parts, helpers, open, progress), checked (title and stage words,
//! the 44-character now line, known parts, real picture and open paths, numbers that are in the signals, no zero or
//! bookkeeping counters); a failed check gets ONE repair turn: a second full call carrying the input, the answer and
//! the failures, never more. What still fails after it is not written.
//!
//! ONE stateless call per island (never a long-lived thread or a shared session, the user 01:08), through the shared call
//! path (`background_call::ask`) on the Islands task's own model (`background_model::task_endpoint(Task::Islands)`:
//! Claude Haiku 5.5 through T3 by default, selectable apart from the shared model; the user 03:37).
//!
//! When: the binding sends every bound island's signals after each pass (`Snapshot`); a call is due on a stage change
//! (of the compact history's stage: Reading to Building is not one), a run end, a new picture, a finished helper, a new
//! eval, or training loss with `LOSS_POINTS` new points (at most one such call per `LOSS_EVERY`), and a heartbeat every
//! `HEARTBEAT` when anything changed. Per island at least `MIN_GAP` between calls and `COALESCE` after the first
//! reason, so a burst of changes is one call; one call in flight across islands. Nothing while emergency mode pauses
//! background work, while Background Tasks ▸ Islands is off, or without a credential.

use crate::islands::{binding, digest, files::Root, stages};

/// The islands app's gate for model HTML, the same file (`crates/islands/src/model/html.rs`): an answer is checked
/// here exactly as the app will clean it before drawing.
#[path = "../../islands/src/model/html_charts.rs"]
mod html_charts;
#[path = "../../islands/src/model/html.rs"]
pub mod island_html;
use anyhow::{Context, Result};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

/// The log and ledger label.
pub const JOB: &str = "island agent";
/// The held-out climb ran Opus 5.5 at medium; Haiku without a level. A per-model level in Background Tasks wins.
pub const EFFORT: &str = "medium";
pub const MIN_GAP: f64 = 120.0;
pub const COALESCE: f64 = 90.0;
pub const HEARTBEAT: f64 = 30.0 * 60.0;
pub const LOSS_EVERY: f64 = 10.0 * 60.0;
pub const LOSS_POINTS: usize = 5;
/// Without a credential (or while Islands is off) a due island is looked at again after this long.
pub const DEFER: f64 = 10.0 * 60.0;
/// The contract's limits (agent-prompt.md, tools/run_agent.py).
pub const TITLE_WORDS: usize = 3;
pub const TITLE_CHARS: usize = 24;
pub const STAGE_WORDS: usize = 2;
pub const SUMMARY_CHARS: usize = 44;
/// What a job was sent to do: at most two lines on the 376-point island at 14 pt (a 91-character line rendered as two
/// lines, 2 Nov), tighter than `islands_put`'s 110.
pub const SENT_CHARS: usize = 90;
pub const PARTS_CAP: usize = 2;
pub const HELPERS_CAP: usize = 4;
pub const LIST_CAP: usize = 6;
pub const BARS_CAP: usize = 6;
pub const LINE_POINTS: usize = 40;
/// A picture older than this is stale while the work moves on (the prompt's rule; the code gives its age).
pub const PICTURE_FRESH_MIN: f64 = 15.0;
/// The most of a task's status file the signals carry (`stages::STATUS_FILE`); a bigger one is left out.
pub const STATUS_BYTES: u64 = 16 * 1024;
/// Counters that are bookkeeping, never progress (agent-prompt.md: "never show bookkeeping: files changed, pages
/// written, lines written, commits, sources read, jobs left"; README: a sources-read counter survived iteration 4).
pub const BANNED: [&str; 11] = ["files changed", "file changed", "pages written", "page written", "lines written", "line written", "commits", "sources read", "source read", "jobs left", "job left"];
/// Marks the parts this job wrote (replaced on its next call; every other writer's stay).
pub const OWN_PART: &str = "islandAgent";
/// The plan refresher's mark: on a bound island its old parts give way to this job's.
const REFRESHER_PART: &str = "refresher";

/// The final instructions (Assistant/Plans/island-design/agent-prompt.md, copied as they are). The public edition has
/// its own copy without the user's quotes and paths (docs/editions.md).
/// The HTML body (3 Nov) is its own addendum after it, so the frozen copy stays as it is.
#[cfg(not(feature = "public"))]
pub const SYSTEM: &str = concat!(include_str!("../prompt/island-agent.md"), "\n", include_str!("../prompt/island-agent-html.md"));
#[cfg(feature = "public")]
pub const SYSTEM: &str = concat!(include_str!("../prompt/public/island-agent.md"), "\n", include_str!("../prompt/public/island-agent-html.md"));

// ---------------------------------------------------------------------------------------------------------
// What the binding sends.

/// One bound island after a pass: its id, its thread, its state on disk after the sync, the code signals, the thread
/// as last read (its title), the latest picture as stored in the island's own folder, and T3's open request.
#[derive(Clone, Debug)]
pub struct Snapshot {
    pub island: String,
    pub thread: String,
    pub state: String,
    pub signals: stages::Signals,
    pub raw: digest::ThreadRaw,
    pub image: Option<String>,
    pub question: Option<binding::Question>,
    /// The island already shows the agent's line for this run and moment (after a restart: no call on first sight).
    pub drawn: bool,
}

impl Snapshot {
    /// What a call looks at, for its triggers.
    fn seen(&self) -> Seen {
        use std::hash::{Hash, Hasher};
        let s = &self.signals;
        let mut h = std::collections::hash_map::DefaultHasher::new();
        (&s.stage, &s.say, s.files, s.commits, s.passed, s.subs_done, s.subs_total, s.background_open, &self.state, &self.image).hash(&mut h);
        Seen {
            status: s.status.as_deref().and_then(status_stamp),
            key_stage: key_stage(&s.stage).to_owned(),
            ended: matches!(self.state.as_str(), "done" | "failed"),
            picture: self.image.clone(),
            helpers_done: s.subs_done,
            evals: s.train.as_ref().map_or(0, |t| t.evals.len()),
            loss_n: s.train.as_ref().map_or(0, |t| t.loss_n),
            hash: h.finish(),
        }
    }
}

use stages::key_stage;

#[derive(Clone, Debug, Default, PartialEq)]
struct Seen {
    /// The status file's size and modification time.
    status: Option<(u64, i128)>,
    key_stage: String,
    ended: bool,
    picture: Option<String>,
    helpers_done: u32,
    evals: usize,
    loss_n: usize,
    hash: u64,
}

// ---------------------------------------------------------------------------------------------------------
// When to call (pure; the clock is the caller's, in seconds).

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason { Stage, End, Picture, Helper, Eval, Loss, Status, Heartbeat }

impl Reason {
    pub fn says(self) -> &'static str {
        match self {
            Reason::Stage => "the stage changed",
            Reason::End => "the run ended",
            Reason::Picture => "a new picture",
            Reason::Helper => "a helper finished",
            Reason::Eval => "a new eval",
            Reason::Loss => "new training loss points",
            Reason::Status => "the task's status file changed",
            Reason::Heartbeat => "the heartbeat (something changed)",
        }
    }
}

#[derive(Clone, Debug, Default)]
struct Island {
    latest: Option<Snapshot>,
    /// What the island agent drew last for this job (`previous_island`).
    previous: Option<Value>,
    /// What the last call saw, and when it ran.
    sent: Option<Seen>,
    last_call: Option<f64>,
    loss_call: Option<f64>,
    /// The first reason since the last call, and when it came.
    due: Option<(f64, Reason)>,
}

/// Per island: what the last call saw, what is due, and when.
#[derive(Default)]
pub struct Triggers {
    islands: HashMap<String, Island>,
}

impl Triggers {
    /// A new snapshot of one island at `now`.
    pub fn observe(&mut self, snap: Snapshot, now: f64) {
        let i = self.islands.entry(snap.island.clone()).or_default();
        let seen = snap.seen();
        let snap_drawn = snap.drawn;
        i.latest = Some(snap);
        let reason = match &i.sent {
            // First sight (a new island, or every island after a restart): no call for a settled one or one that already
            // shows this moment's line; else once it is past Starting (a job island at birth has nothing to draw).
            None if seen.ended || snap_drawn => None,
            None if seen.key_stage != "Preparing" || seen.picture.is_some() => Some(Reason::Stage),
            None => None,
            Some(s) if seen.ended && !s.ended => Some(Reason::End),
            Some(s) if seen.key_stage != s.key_stage && !seen.ended => Some(Reason::Stage),
            Some(s) if seen.picture.is_some() && seen.picture != s.picture => Some(Reason::Picture),
            Some(s) if seen.helpers_done > s.helpers_done => Some(Reason::Helper),
            Some(s) if seen.evals > s.evals => Some(Reason::Eval),
            Some(s) if seen.status.is_some() && seen.status != s.status && !seen.ended => Some(Reason::Status),
            Some(s) if seen.loss_n >= s.loss_n + LOSS_POINTS && i.loss_call.is_none_or(|t| now - t >= LOSS_EVERY) => Some(Reason::Loss),
            Some(s) if seen.hash != s.hash && i.last_call.is_some_and(|t| now - t >= HEARTBEAT) => Some(Reason::Heartbeat),
            Some(_) => None,
        };
        // A run end outranks what was waiting; anything else keeps the first reason's clock.
        match (reason, i.due) {
            (Some(Reason::End), Some((at, r))) if r != Reason::End => i.due = Some((at, Reason::End)),
            (Some(r), None) => i.due = Some((now, r)),
            _ => {}
        }
    }

    /// An island gone from the binding.
    pub fn forget(&mut self, island: &str) { self.islands.remove(island); }

    /// The island to call now, if any: due for `COALESCE`, `MIN_GAP` after its last call; the longest waiting first.
    pub fn next(&self, now: f64) -> Option<(String, Reason)> {
        self.islands.iter()
            .filter_map(|(id, i)| {
                let (at, r) = i.due?;
                let ready = (at + COALESCE).max(i.last_call.map_or(0.0, |t| t + MIN_GAP));
                (ready <= now && i.latest.is_some()).then(|| (ready, id.clone(), r))
            })
            .min_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal).then(a.1.cmp(&b.1)))
            .map(|(_, id, r)| (id, r))
    }

    /// When `next` could next answer (the loop sleeps until then).
    pub fn wake_at(&self) -> Option<f64> {
        self.islands.values().filter_map(|i| {
            let (at, _) = i.due?;
            Some((at + COALESCE).max(i.last_call.map_or(0.0, |t| t + MIN_GAP)))
        }).min_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
    }

    /// The call for `island` starts at `now`: its latest snapshot, and what it saw is remembered.
    pub fn begin(&mut self, island: &str, now: f64) -> Option<Snapshot> {
        let i = self.islands.get_mut(island)?;
        let snap = i.latest.clone()?;
        let seen = snap.seen();
        if i.due.is_some_and(|(_, r)| r == Reason::Loss) || i.sent.as_ref().is_none_or(|s| seen.loss_n >= s.loss_n + LOSS_POINTS) { i.loss_call = Some(now) }
        i.sent = Some(seen);
        i.last_call = Some(now);
        i.due = None;
        Some(snap)
    }

    /// The island the last call drew, for the next call's `previous_island`.
    pub fn drew(&mut self, island: &str, v: Value) { if let Some(i) = self.islands.get_mut(island) { i.previous = Some(v) } }
    pub fn previous(&self, island: &str) -> Option<&Value> { self.islands.get(island)?.previous.as_ref() }

    /// Not called now (no key, Islands off): looked at again after `DEFER`.
    pub fn defer(&mut self, island: &str, now: f64) {
        if let Some(i) = self.islands.get_mut(island) {
            if let Some((_, r)) = i.due { i.due = Some((now + DEFER - COALESCE, r)) }
        }
    }
}

// ---------------------------------------------------------------------------------------------------------
// The call.

/// The answer's JSON schema: the prompt's output contract (only title, icon, state, stage and summary required).
pub fn schema() -> Value {
    let item = json!({"type": "object", "properties": {
        "id": {"type": "string"}, "title": {"type": "string"}, "label": {"type": "string"},
        "value": {"type": ["number", "string"]}, "highlight": {"type": "boolean"}}, "additionalProperties": false});
    json!({
        "type": "object",
        "properties": {
            "title": {"type": "string"},
            "icon": {"type": "string"},
            "state": {"type": "string", "enum": ["running", "waiting", "done", "failed"]},
            "stage": {"type": "string"},
            "summary": {"type": "string"},
            "parts": {"type": "array", "items": {"type": "object", "properties": {
                "type": {"type": "string", "enum": ["line", "bars", "stat", "image", "list", "text", "button"]},
                "points": {"type": "array", "items": {"type": "number"}},
                "unit": {"type": "string"}, "max": {"type": "number"}, "value": {"type": ["number", "string"]},
                "baseline": {"type": "object", "properties": {"label": {"type": "string"}, "value": {"type": ["number", "string"]}, "unit": {"type": "string"}}, "additionalProperties": false},
                "items": {"type": "array", "items": item},
                "path": {"type": "string"}, "open": {"type": "string"}, "label": {"type": "string"}, "action": {"type": "string"}},
                "required": ["type"], "additionalProperties": false}},
            "helpers": {"type": "array", "items": {"type": "object", "properties": {"title": {"type": "string"}, "state": {"type": "string", "enum": ["now", "done"]}}, "required": ["title", "state"], "additionalProperties": false}},
            "open": {"type": "string"},
            "progress": {"type": "number"},
            "html": {"type": "string"},
            "sent": {"type": "string"}
        },
        "required": ["title", "icon", "state", "stage", "summary"],
        "additionalProperties": false
    })
}

pub fn call<'a>(model: &str, system: &'a str, prompt: crate::background_call::Prompt) -> crate::background_call::Call<'a> {
    crate::background_call::Call { job: JOB, model: model.to_owned(), system, schema: schema(), effort: EFFORT, prompt, history: Vec::new(), session: None }
}

fn hhmm(t: f64) -> String {
    chrono::DateTime::from_timestamp(t as i64, 0).map(|d| d.with_timezone(&chrono::Local).format("%H:%M").to_string()).unwrap_or_default()
}

/// The moment's signals, as the README's contract names them (built from the snapshot alone: no T3 call).
pub fn signals(snap: &Snapshot, island: &Map<String, Value>, previous: Option<&Value>) -> Value {
    let s = &snap.signals;
    let shown = match snap.state.as_str() { "done" | "failed" => stages::Shown::Ended, "waiting" => stages::Shown::Asking, _ => stages::Shown::Running };
    let task = snap.raw.read["thread"]["title"].as_str().map(digest::without_tag).map(str::to_owned)
        .or_else(|| island.get("title").and_then(Value::as_str).map(str::to_owned)).unwrap_or_default();
    let open: Vec<&String> = s.subs_open.iter().collect();
    let finished: Vec<&String> = s.subs_names.iter().filter(|t| !s.subs_open.contains(t)).collect();
    let picture_min_ago = snap.image.as_ref().and(s.picture_at).map(|t| ((s.now - t) / 60.0).round().max(0.0));
    let mut v = json!({
        "task": task,
        "brief": s.brief,
        "now": hhmm(s.now),
        "elapsed_min": ((s.now - s.started) / 60.0).round(),
        "state_hint": snap.state,
        // The stage word is code's, as the island shows it (the prompt: copy it exactly): Background, Needs approval,
        // Downloading, Labelling and Drafting included.
        "stage": island.get("stage").and_then(Value::as_str).filter(|st| !st.is_empty() && *st != "Starting")
            .unwrap_or(if shown == stages::Shown::Asking { "Your answer" } else if s.stage == "Starting" { "Reading" } else { s.stage.as_str() }),
        "history": s.history_minutes(shown, &snap.state),
        "recent": s.recent.iter().map(|(t, l)| format!("{} {l}", hhmm(*t))).collect::<Vec<_>>(),
        "helpers": {"working": open, "finished": finished},
        "counters": {"files": s.files, "commits": s.commits, "tests_passed": s.passed, "sources": s.sources,
                     "latest_picture": snap.image, "picture_min_ago": picture_min_ago},
    });
    if let Some(q) = &snap.question {
        let first = q.items.first();
        v["question"] = json!({"kind": "question", "prompt": first.map(|i| i.text.clone()).unwrap_or_default(),
            "reason": first.and_then(|i| i.header.clone()), "options": first.map(|i| i.options.iter().map(|o| o.label.clone()).collect::<Vec<_>>()).unwrap_or_default()});
    } else if let Some(a) = s.approval.as_ref().filter(|_| snap.state == "waiting") {
        let mut l = a.lines();
        v["question"] = json!({"kind": "command", "prompt": l.next().unwrap_or(""), "reason": l.next().unwrap_or(""), "options": ["Decline", "Approve"]});
    }
    if let Some(t) = &s.owner { v["owner"] = json!(t) }
    if let Some(t) = &s.train {
        let evals: Map<String, Value> = t.evals.last().map(|(_, e)| e.iter().map(|(k, x)| (k.clone(), if (0.0..=1.0).contains(x) { json!((x * 1000.0).round() / 10.0) } else { json!(x) })).collect()).unwrap_or_default();
        v["metrics"] = json!({"run": t.run, "status": t.status, "step": t.step, "total_steps": t.total, "loss": t.loss,
            "evals_percent": evals, "eval_step": t.evals.last().map(|e| e.0), "tok_s": t.tok_s, "mac_memory_gb": t.mem_gb});
    }
    if let Some(st) = s.status.as_deref().and_then(read_status) { v["status"] = st }
    if let Some(p) = previous { v["previous_island"] = p.clone() }
    v["sent"] = json!(island.get("sent").and_then(Value::as_str).map(digest::one_line).unwrap_or_default());
    v
}

fn status_path(p: &str) -> std::path::PathBuf {
    match p.strip_prefix("~/") {
        Some(rest) => std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(rest),
        None => std::path::PathBuf::from(p),
    }
}

/// A status file's size and modification time (ns), for the trigger.
fn status_stamp(p: &str) -> Option<(u64, i128)> {
    let m = std::fs::metadata(status_path(p)).ok()?;
    let t = m.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_nanos() as i128;
    Some((m.len(), t))
}

/// The task's status file as the signals carry it: one JSON object of at most `STATUS_BYTES`.
pub fn read_status(p: &str) -> Option<Value> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(status_path(p)).ok()?.take(STATUS_BYTES + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() as u64 > STATUS_BYTES { return None }
    let v: Value = serde_json::from_slice(&bytes).ok()?;
    v.is_object().then_some(v)
}

// What it was sent to do (bug note 2026-11-01T05-50-57;, looking at an island: "I just don't know"
// what it's for). The binding writes `sent` from the task title when the island is made; an island still without one
// (a title that gave nothing, an island older than the line) gets it from this job, once: the signals carry `sent` ("" while
// there is none), the message asks for it only then, `check` holds it to one plain line and `apply` never replaces one.
// The system prompt is the frozen copy of the final agent prompt (the user 03:37: "use them as they are"), so the ask
// rides in the message.

/// The message's ask for the sent line, while the island has none.
pub const SENT_ASK: &str = "This island has no `sent` line yet. Add `sent` to your answer, once: what the agent was sent to do, taken from `task` and `brief`, \
one plain line starting with a verb in the imperative, at most 90 characters (\"Promote the allowed models into the 1.0 release\"). \
It stays under the title for the whole job, so it is the errand, never its status, stage, progress or result; no ids, paths, \
branch names or project prefixes.";

fn wants_sent(signals: &Value) -> bool { signals["sent"].as_str().is_some_and(|s| s.trim().is_empty()) }

/// The one user message: the signals, then the ask (tools/run_agent.py).
pub fn message(signals: &Value) -> String {
    let sent = if wants_sent(signals) { format!(" {SENT_ASK}") } else { String::new() };
    format!("<signals>\n{}\n</signals>\n\nDraw the island for this moment.{sent}", serde_json::to_string_pretty(signals).unwrap_or_default())
}

/// The repair turn: the whole input again, the answer, and the checks it failed (once; never more).
pub fn repair_message(msg: &str, answer: &str, failures: &[String]) -> String {
    format!("{msg}\n\nYour answer was:\n{answer}\n\nIt fails these checks: {}. Answer again with the corrected island, changing only what the checks require.", failures.join("; "))
}

/// The answer's JSON object (a stray fence or a sentence around it is cut away).
pub fn parse(answer: &str) -> Result<Map<String, Value>> {
    let t = answer.trim();
    let (a, b) = (t.find('{').context("not one JSON object")?, t.rfind('}').context("not one JSON object")?);
    let v: Value = serde_json::from_str(&t[a..=b]).context("not one JSON object")?;
    v.as_object().cloned().context("not one JSON object")
}

/// Every number written in `text` (`3,100` and `0.42` and `83%` alike; a comma counts only as a thousands
/// separator, so a JSON array `[0.32,0.33]` reads as two numbers).
fn numbers(text: &str) -> Vec<f64> {
    let cs: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut cur = String::new();
    for (i, c) in cs.iter().copied().chain(std::iter::once(' ')).enumerate() {
        let thousands = c == ',' && !cur.is_empty() && !cur.contains('.')
            && cs.get(i + 1..i + 4).is_some_and(|d| d.iter().all(char::is_ascii_digit)) && cs.get(i + 4).is_none_or(|d| !d.is_ascii_digit());
        if c.is_ascii_digit() || (c == '.' && !cur.is_empty() && !cur.contains('.')) || thousands { if c != ',' { cur.push(c) } continue }
        if !cur.is_empty() {
            if let Ok(v) = cur.trim_end_matches('.').parse::<f64>() { out.push(v) }
            cur.clear();
        }
    }
    out
}

/// A number the signals carry (to its shown precision, or as a percentage of a share).
fn grounded(v: f64, known: &[f64]) -> bool {
    known.iter().any(|n| (n - v).abs() <= (0.01 * n.abs()).max(0.005) || (n * 100.0 - v).abs() <= 0.5)
}

fn num(v: &Value) -> Option<f64> {
    v.as_f64().or_else(|| v.as_str().and_then(|s| s.replace(',', "").trim().parse().ok()))
}

/// A zero or bookkeeping counter (`BANNED`), in a stat, a bar or a list row.
fn banned(p: &Value) -> Option<String> {
    let words = |v: &Value| v.as_str().unwrap_or("").to_lowercase();
    let mut texts = vec![words(&p["unit"]), words(&p["label"])];
    for it in p["items"].as_array().into_iter().flatten() { texts.push(words(&it["label"])); texts.push(words(&it["title"])); }
    let joined = texts.join(" ");
    if let Some(b) = BANNED.iter().find(|b| {
        joined.match_indices(**b).any(|(i, _)| joined[..i].chars().next_back().is_none_or(|c| !c.is_alphanumeric()) && joined[i + b.len()..].chars().next().is_none_or(|c| !c.is_alphanumeric()))
    }) {
        return Some(format!("a bookkeeping counter ({b}) is not progress"));
    }
    if p["type"] == "stat" && num(&p["value"]) == Some(0.0) { return Some("a counter of 0".into()) }
    None
}

/// The answer, checked: what may be written, and every failed check (for the one repair turn).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Checked {
    pub title: Option<String>,
    pub icon: Option<String>,
    /// Only `failed` or `waiting` over a run code reads as done (the agent read a stop or an ask in the words).
    pub state: Option<String>,
    pub summary: Option<String>,
    pub parts: Vec<Value>,
    pub helpers: Option<Vec<Value>>,
    pub open: Option<String>,
    /// What the job was sent to do, only for an island without one (written once, never replaced).
    pub sent: Option<String>,
    /// The model's HTML for the island body (checked by the app's own gate); its `parts` become the fallback.
    pub html: Option<String>,
    /// What the island agent drew (its answer as it stands after the checks), for `previous_island`.
    pub drew: Value,
    pub failures: Vec<String>,
}

/// Checks one answer against the contract and the moment's signals. Text and buttons stay the binding's (the question
/// text is on the island, the answer on the notch card, 24 Oct), so they are not written; a picture must be the
/// island's stored latest picture; a line's points and every number must be in the signals.
pub fn check(answer: &str, signals: &Value, snap: &Snapshot) -> Checked {
    let mut c = Checked::default();
    let o = match parse(answer) { Ok(o) => o, Err(e) => { c.failures.push(format!("{e}")); return c } };
    c.drew = Value::Object(o.clone());
    let known = numbers(&signals.to_string());
    let s = |k: &str| o.get(k).and_then(Value::as_str).map(str::trim).unwrap_or("").to_owned();
    let title = s("title");
    if title.split_whitespace().count() > TITLE_WORDS || title.chars().count() > TITLE_CHARS { c.failures.push(format!("title \"{title}\" over {TITLE_WORDS} words or {TITLE_CHARS} characters")) }
    else if !title.is_empty() && crate::islands::tools::style_problem(&snap.island, json!({"title": title}).as_object().unwrap(), false).is_none() { c.title = Some(title) }
    let stage = s("stage");
    if stage.split_whitespace().count() > STAGE_WORDS { c.failures.push(format!("stage \"{stage}\" over {STAGE_WORDS} words")) }
    let state = s("state");
    if !["running", "waiting", "done", "failed"].contains(&state.as_str()) { c.failures.push(format!("state \"{state}\"")) }
    else if snap.state == "done" && (state == "failed" || state == "waiting") { c.state = Some(state.clone()) }
    let summary = digest::one_line(&s("summary"));
    let n = summary.chars().count();
    let invented: Vec<f64> = numbers(&summary).into_iter().filter(|x| !grounded(*x, &known)).collect();
    if n > SUMMARY_CHARS { c.failures.push(format!("summary {n} characters (at most {SUMMARY_CHARS})")) }
    else if !invented.is_empty() { c.failures.push(format!("the summary's {} is not in the signals", number(invented[0]))) }
    else if !summary.is_empty() { c.summary = Some(summary) }
    c.icon = crate::islands::icon(&s("icon"));
    let mut graphics = 0;
    for p in o.get("parts").and_then(Value::as_array).into_iter().flatten() {
        let ty = p["type"].as_str().unwrap_or("");
        if !["line", "bars", "stat", "image", "list", "text", "button"].contains(&ty) { c.failures.push(format!("part {ty}")); continue }
        // Text and buttons are the binding's (never written), so they are not checked either.
        if ty == "text" || ty == "button" { continue }
        if let Some(why) = banned(p) { c.failures.push(why); continue }
        let part = match ty {
            "line" => {
                let pts: Vec<f64> = p["points"].as_array().into_iter().flatten().filter_map(Value::as_f64).filter(|x| x.is_finite()).collect();
                if pts.len() < 2 { continue }
                if pts.iter().any(|x| !grounded(*x, &known)) { c.failures.push("line points that are not in the signals".into()); continue }
                json!({"type": "line", "points": pts[pts.len().saturating_sub(LINE_POINTS)..], "unit": digest::clip(p["unit"].as_str().unwrap_or(""), 12)})
            }
            "stat" => {
                let Some(x) = num(&p["value"]).filter(|x| x.is_finite()) else { continue };
                if !grounded(x, &known) { c.failures.push(format!("stat {} is not in the signals", p["value"])); continue }
                let mut st = json!({"type": "stat", "value": p["value"].as_str().map(str::to_owned).unwrap_or_else(|| number(x)), "unit": digest::clip(p["unit"].as_str().unwrap_or(""), 16)});
                if let Some(b) = p["baseline"].as_object().filter(|b| b.get("value").and_then(num).is_some_and(|v| grounded(v, &known))) { st["baseline"] = Value::Object(b.clone()) }
                st
            }
            "bars" => {
                let items: Vec<Value> = p["items"].as_array().into_iter().flatten().filter_map(|b| {
                    let l = digest::clip(&digest::one_line(b["label"].as_str()?), 14);
                    let x = num(&b["value"]).filter(|x| x.is_finite() && *x >= 0.0)?;
                    if !grounded(x, &known) { return None }
                    let mut it = json!({"label": l, "value": x});
                    if b["highlight"] == true { it["highlight"] = json!(true) }
                    Some(it)
                }).take(BARS_CAP).collect();
                if items.len() < 2 { c.failures.push("bars need two values from the signals".into()); continue }
                let mut b = json!({"type": "bars", "items": items, "unit": digest::clip(p["unit"].as_str().unwrap_or(""), 8)});
                if let Some(m) = p["max"].as_f64() { b["max"] = json!(m) }
                b
            }
            "list" => {
                let items: Vec<Value> = p["items"].as_array().into_iter().flatten().enumerate().filter_map(|(n, it)| {
                    let t = digest::clip(&digest::one_line(it["title"].as_str().or(it["label"].as_str())?), 32);
                    (!t.is_empty()).then(|| json!({"id": format!("a{n}"), "title": t, "value": digest::clip(it["value"].as_str().unwrap_or(""), 16)}))
                }).take(LIST_CAP).collect();
                if items.is_empty() { continue }
                json!({"type": "list", "items": items})
            }
            _ => {
                let path = p["path"].as_str().unwrap_or("").trim().to_owned();
                if snap.image.as_deref() != Some(path.as_str()) || !crate::islands::picture(&path) { c.failures.push(format!("image path {path} is not the latest picture")); continue }
                // A picture older than 15 minutes is stale while the work moves on (only a done job may show it).
                let age = snap.signals.picture_at.map(|t| (snap.signals.now - t) / 60.0);
                if snap.state != "done" && age.is_some_and(|m| m > PICTURE_FRESH_MIN) { c.failures.push(format!("the picture is {} minutes old: stale while the work moves on", age.unwrap_or(0.0).round())); continue }
                json!({"type": "image", "path": path, "open": path})
            }
        };
        if graphics >= PARTS_CAP { c.failures.push(format!("more than {PARTS_CAP} graphic parts")); break }
        graphics += 1;
        let mut part = part;
        part[OWN_PART] = json!(true);
        c.parts.push(part);
    }
    if let Some(h) = o.get("helpers").and_then(Value::as_array) {
        let rows: Vec<Value> = h.iter().filter_map(|x| {
            let t = digest::one_line(x["title"].as_str()?);
            let ok = !t.is_empty() && t.split_whitespace().count() <= 4;
            ok.then(|| json!({"title": t, "state": if x["state"] == "now" { "now" } else { "done" }}))
        }).take(HELPERS_CAP).collect();
        c.helpers = Some(rows);
    }
    let sent = digest::one_line(&s("sent"));
    if wants_sent(signals) && !sent.is_empty() {
        let n = sent.chars().count();
        let invented: Vec<f64> = numbers(&sent).into_iter().filter(|x| !grounded(*x, &known)).collect();
        let first = sent.split_whitespace().next().unwrap_or("").to_lowercase();
        if n > SENT_CHARS { c.failures.push(format!("sent {n} characters (at most {SENT_CHARS})")) }
        else if sent.split_whitespace().count() < 2 { c.failures.push("sent is one word: say what the agent was sent to do".into()) }
        else if sent.contains("://") || sent.contains("[heimdall:") || sent.split_whitespace().any(|w| w.starts_with('/') || w.starts_with("~/")) {
            c.failures.push("sent carries a path, link or tag".into())
        }
        else if first.len() > 5 && first.ends_with("ing") {
            c.failures.push(format!("sent starts with \"{first}\": the errand in the imperative, not its status"))
        }
        else if !invented.is_empty() { c.failures.push(format!("the sent line's {} is not in the signals", number(invented[0]))) }
        else { c.sent = Some(sent) }
    }
    let html = o.get("html").and_then(Value::as_str).unwrap_or("").trim().to_owned();
    if !html.is_empty() {
        match check_html(&html, signals, &known, snap) {
            Ok(()) => c.html = Some(html),
            Err(why) => c.failures.push(why),
        }
    }
    let open = s("open");
    if !open.is_empty() {
        // A page the work served, or a file the island viewer shows or a folder (revealed in Finder): absolute or `~/`
        // only, and nothing that could run (review, 30 Oct: a prompt in a thread must not make one click launch an app).
        let real = if open.starts_with("http://") || open.starts_with("https://") { signals.to_string().contains(&open) } else { safe_open_path(&open) };
        if real { c.open = Some(open) } else { c.failures.push(format!("open target {open} does not exist")) }
    }
    c
}

/// The model's HTML against the gate and the signals: it must pass the app's sanitizer untouched (nothing stripped);
/// every number the user reads in it (its text, the chart tags' data as the gate parsed it, percentages in its styles such
/// as a bar's width) must be in the signals (a share of two numbers there counts, as a percentage or a ring); no
/// bookkeeping counter (`BANNED`); and every picture must be the island's latest (fresh unless the job is done) or
/// the status file's own.
pub fn check_html(html: &str, signals: &Value, known: &[f64], snap: &Snapshot) -> std::result::Result<(), String> {
    let status_picture = signals["status"]["picture"].as_str().map(str::to_owned);
    let latest = snap.image.clone();
    let clean = island_html::sanitize(html, &|p| {
        let p = p.to_string_lossy();
        p.as_ref() == latest.as_deref().unwrap_or("\0") || Some(p.as_ref()) == status_picture.as_deref()
    }).map_err(|e| format!("html refused: {e}"))?;
    if !clean.stripped.is_empty() {
        return Err(format!("html uses what islands do not draw: {}", clean.stripped.join(", ")));
    }
    let age = snap.signals.picture_at.map(|t| (snap.signals.now - t) / 60.0);
    if clean.images.iter().any(|i| latest.as_deref() == Some(i.as_str())) && snap.state != "done" && age.is_some_and(|m| m > PICTURE_FRESH_MIN) {
        return Err(format!("the picture is {} minutes old: stale while the work moves on", age.unwrap_or(0.0).round()));
    }
    let mut known = known.to_vec();
    known.extend(shares(signals));
    let text = island_html::visible_text(&clean.html);
    let mut shown = text.clone();
    for d in &clean.chart_data { shown.push(' '); shown.push_str(&d.replace('|', " ").replace(':', " ")) }
    // A width in percent (a bar) is a number the user reads as a length; other percentages (a radius) are style.
    for st in &clean.styles {
        let l = st.to_lowercase();
        for (k, _) in l.match_indices("width:") {
            let v: String = l[k + 6..].trim_start().chars().take_while(|c| c.is_ascii_digit() || *c == '.' || *c == '%').collect();
            if let Some(d) = v.strip_suffix('%') { shown.push(' '); shown.push_str(d); }
        }
    }
    if let Some(why) = banned(&json!({"label": text})) { return Err(why) }
    let invented: Vec<f64> = numbers(&shown).into_iter().filter(|x| !grounded(*x, &known) && *x != 100.0 && *x != 0.0).collect();
    if let Some(x) = invented.first() { return Err(format!("the html's {} is not in the signals", number(*x))) }
    Ok(())
}

/// Shares the signals imply: a status file's `progress` (done of total) and the code's own progress, as fractions.
fn shares(signals: &Value) -> Vec<f64> {
    let mut out = vec![];
    for p in [&signals["status"]["progress"], &signals["metrics"]] {
        let d = num(&p["done"]).or_else(|| num(&p["step"]));
        let t = num(&p["total"]).or_else(|| num(&p["total_steps"]));
        if let (Some(d), Some(t)) = (d, t.filter(|t| *t > 0.0)) { out.push(d / t) }
    }
    out
}

/// File kinds an island's open target may name: what Heimdall's viewer shows (pages, notes, pictures, PDFs, audio,
/// video, office documents). Anything else (apps, installers, scripts that run on a click, unknown types) is refused.
pub const OPENABLE: [&str; 38] = ["md", "markdown", "txt", "log", "csv", "tsv", "json", "jsonl", "html", "htm", "pdf", "png", "jpg", "jpeg",
    "gif", "webp", "heic", "tif", "tiff", "svg", "mp3", "wav", "m4a", "aif", "aiff", "flac", "ogg", "mp4", "mov", "m4v", "webm", "docx",
    "xlsx", "pptx", "key", "pages", "numbers", "rtf"];

/// An open target path that is safe to click: absolute or `~/`, there, and a folder or an `OPENABLE` file without an
/// executable bit.
pub fn safe_open_path(open: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let path = match open.strip_prefix("~/") {
        Some(rest) => match std::env::var("HOME") { Ok(h) => format!("{h}/{rest}"), Err(_) => return false },
        None if open.starts_with('/') => open.to_owned(),
        None => return false,
    };
    let p = std::path::Path::new(&path);
    let Ok(m) = std::fs::metadata(p) else { return false };
    if m.is_dir() {
        let ext = p.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
        return !["app", "pkg", "mpkg", "bundle", "framework", "workflow", "action", "prefpane", "plugin", "kext", "scptd", "xpc", "appex"].contains(&ext.as_str());
    }
    let ext = p.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    OPENABLE.contains(&ext.as_str()) && m.permissions().mode() & 0o111 == 0
}

fn number(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e12 { return format!("{}", v as i64) }
    let t = format!("{v:.2}");
    t.trim_end_matches('0').trim_end_matches('.').to_owned()
}

/// Writes the checked answer into the island under the folder lock: this job's parts replace its last ones (and the
/// plan refresher's old ones), before every other writer's (the question text, the band last); an answer without parts
/// clears its graphic (each moment's graphic is drawn anew). The now line marks the run as the agent's
/// (`summaryRun`, `summaryStage`: code's first-sentence fallback steps back while the moment lasts). Title, icon, helpers' names and the open target are
/// written as checked; state only `failed` or `waiting` over a done run. Returns whether anything changed.
pub fn apply(root: &Root, island: &str, run: &str, moment: &str, c: &Checked) -> Result<bool> {
    root.locked(|| {
        let Some(mut o) = root.read(island) else { return Ok(false) };
        let mut patch = Map::new();
        let others: Vec<Value> = o.get("parts").and_then(Value::as_array).into_iter().flatten()
            .filter(|p| p.get(OWN_PART).is_none() && p.get(REFRESHER_PART).is_none()).cloned().collect();
        // The model's HTML leads; its parts are the fallback the app draws if the HTML cannot be (`resolve_html`).
        let own: Vec<Value> = match &c.html {
            Some(h) => vec![json!({"type": "html", "html": h, "fallback": c.parts, OWN_PART: true})],
            None => c.parts.clone(),
        };
        patch.insert("parts".into(), Value::Array(own.into_iter().chain(others).collect()));
        if let Some(s) = &c.summary {
            patch.insert("summary".into(), json!(s));
            patch.insert("summaryRun".into(), json!(run));
            patch.insert("summaryStage".into(), json!(moment));
        }
        if let Some(t) = &c.title {
            patch.insert("title".into(), json!(t));
            if let Some(Value::Object(th)) = o.get("thread") { let mut th = th.clone(); th.insert("title".into(), json!(t)); patch.insert("thread".into(), Value::Object(th)); }
        }
        if let Some(i) = &c.icon { patch.insert("icon".into(), json!(i)); }
        if let Some(h) = &c.helpers { patch.insert("helpers".into(), if h.is_empty() { Value::Null } else { Value::Array(h.clone()) }); }
        if let Some(t) = &c.open { patch.insert("open".into(), json!(t)); }
        // Written once: an island that has its errand keeps it, whatever a later answer says.
        if let Some(t) = c.sent.as_ref().filter(|_| o.get("sent").and_then(Value::as_str).is_none_or(|x| x.trim().is_empty())) { patch.insert("sent".into(), json!(t)); }
        if let Some(st) = &c.state { if o.get("state").and_then(Value::as_str) == Some("done") { patch.insert("state".into(), json!(st)); } }
        patch.retain(|k, v| o.get(k).unwrap_or(&Value::Null) != v);
        if patch.is_empty() { return Ok(false) }
        crate::islands::files::merge(&mut o, &patch);
        crate::islands::tools::stamp_now(&mut o, &patch);
        crate::islands::files::write_atomic(&root.file(island), &Value::Object(o))?;
        Ok(true)
    })
}

// ---------------------------------------------------------------------------------------------------------
// The loop.

/// The model behind the call (`Live`: the shared call path; tests: a fake).
#[async_trait::async_trait]
pub trait Model: Send + Sync {
    async fn ask(&self, e: &crate::plan_refresher::Endpoint, message: String) -> Result<String>;
}

pub struct Live;

#[async_trait::async_trait]
impl Model for Live {
    async fn ask(&self, e: &crate::plan_refresher::Endpoint, message: String) -> Result<String> {
        let model = e.model.clone().unwrap_or_else(crate::background_model::anthropic_model);
        Ok(crate::background_call::ask(e, &call(&model, SYSTEM, message.into())).await?.text)
    }
}

#[derive(Clone)]
pub struct Config {
    pub root: Root,
    pub endpoint: Arc<dyn Fn() -> crate::plan_refresher::Endpoint + Send + Sync>,
    /// Background Tasks ▸ Islands on and the agent allowed.
    pub enabled: Arc<dyn Fn() -> bool + Send + Sync>,
    pub model: Arc<dyn Model>,
}

fn now_s() -> f64 { std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs_f64() }

/// One due island's call, start to finish. `Ok(None)`: skipped (off, paused, no key).
pub async fn run_one(cfg: &Config, triggers: &mut Triggers, island: &str, reason: Reason, now: f64) -> Result<Option<bool>> {
    if !(cfg.enabled)() || crate::background_gate::emergency_paused().is_some() { triggers.defer(island, now); return Ok(None) }
    let resolve = cfg.endpoint.clone();
    let endpoint = tokio::task::spawn_blocking(move || resolve()).await?;
    if endpoint.key.is_none() { triggers.defer(island, now); return Ok(None) }
    let Some(snap) = triggers.begin(island, now) else { return Ok(None) };
    let Some(on_disk) = cfg.root.read(island) else { triggers.forget(island); return Ok(None) };
    let sig = signals(&snap, &on_disk, triggers.previous(island));
    let msg = message(&sig);
    let answer = cfg.model.ask(&endpoint, msg.clone()).await?;
    let mut c = check(&answer, &sig, &snap);
    let mut repaired = false;
    if !c.failures.is_empty() {
        // One repair turn, never more: the whole input, the answer, the failures.
        eprintln!("Island agent: '{island}' answer fails {}; one repair turn", c.failures.join("; "));
        let again = cfg.model.ask(&endpoint, repair_message(&msg, &answer, &c.failures)).await?;
        c = check(&again, &sig, &snap);
        repaired = true;
    }
    if c.drew.is_object() { triggers.drew(island, c.drew.clone()) }
    let shown = match snap.state.as_str() { "done" | "failed" => stages::Shown::Ended, "waiting" => stages::Shown::Asking, _ => stages::Shown::Running };
    let changed = apply(&cfg.root, island, &snap.signals.run, binding::summary_key(shown, &snap.signals.stage), &c)?;
    eprintln!("Island agent: '{island}' (thread {}) for {}: {} part(s){}{}{}{}", snap.thread, reason.says(), c.parts.len(),
        if c.summary.is_some() { ", a now line" } else { "" }, if repaired { ", after a repair turn" } else { "" },
        if c.failures.is_empty() { String::new() } else { format!(", left out: {}", c.failures.join("; ")) }, if changed { "" } else { ", unchanged" });
    Ok(Some(changed))
}

/// The loop: snapshots in, one call at a time out, until `stop`.
pub fn spawn(cfg: Config, mut rx: tokio::sync::mpsc::UnboundedReceiver<Snapshot>, mut stop: tokio::sync::watch::Receiver<bool>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut triggers = Triggers::default();
        loop {
            if *stop.borrow() { break }
            let wait = triggers.wake_at().map_or(Duration::from_secs(3600), |t| Duration::from_secs_f64((t - now_s()).clamp(0.5, 3600.0)));
            tokio::select! {
                snap = rx.recv() => match snap { Some(s) => triggers.observe(s, now_s()), None => break },
                _ = tokio::time::sleep(wait) => {}
                _ = crate::background_gate::resumed() => {}
                r = stop.changed() => { if r.is_err() || *stop.borrow() { break } }
            }
            // Everything that arrived meanwhile first.
            while let Ok(s) = rx.try_recv() { triggers.observe(s, now_s()) }
            let now = now_s();
            let Some((island, reason)) = triggers.next(now) else { continue };
            match run_one(&cfg, &mut triggers, &island, reason, now).await {
                Ok(_) => {}
                Err(e) if crate::background_gate::quiet(&e) => {}
                Err(e) => eprintln!("Island agent: '{island}' call failed: {e:#}"),
            }
        }
    })
}

/// The installed loop beside the binding: its snapshots come through the islands host. No host: nothing runs.
pub fn spawn_live(gate: Arc<agent::delivery::DeliveryGate>, stop: tokio::sync::watch::Receiver<bool>) -> Option<tokio::task::JoinHandle<()>> {
    if cfg!(test) { return None }
    let host = crate::islands::host()?;
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    host.set_island_agent(Some(tx));
    let cfg = Config {
        root: host.root.clone(),
        // The Islands task's own model (Claude Haiku 5.5 through T3 by default), else the shared one.
        endpoint: Arc::new(|| crate::background_model::task_endpoint(crate::background_model::Task::Islands)),
        enabled: Arc::new(move || crate::background_model::enabled(crate::background_model::Task::Islands) && gate.check().is_ok()),
        model: Arc::new(Live),
    };
    Some(spawn(cfg, rx, stop))
}

#[cfg(test)]
#[path = "island_agent_tests.rs"]
mod tests;
