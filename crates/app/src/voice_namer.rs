//! The voice namer (01:35/01:37): a small agent of its own, on the background model (Background Tasks ▸
//! Model, Claude Haiku 5.5 by default; Background Tasks ▸ Voice Names turns it off) with its own system
//! prompt, that proposes a name for a coded voice ("Speaker Fri 23 Oct 21h #3") from the archived transcript.
//!
//! When it runs: only while a call is open, at most once a minute, no card already up, and only for a coded voice
//! that is talking right now (a line in the last three minutes), that came back (two or more days, in separate
//! visits three hours apart) and has built up about an hour of conversation across calls. Never on the first day, never as a batch, nothing spent on a one-off voice. Per
//! voice, a model call needs 20 minutes since the last one and new lines since then.
//! What it reads: the voice's own lines, the lines around each of them (a name said just before the voice answers
//! is the usual hint: "hey John" belongs to whoever answers), and the names Heimdall already knows.
//! What it decides: a name only with several independent hints (at least three lines, in at least two separate
//! moments), each checked here in code against the transcript: the voice says the name as its own, or someone says
//! it and the voice answers next. Never a name Heimdall already knows (that is another voice). Then the app shows one island card, "Is this John?",
//! with the quote: Yes names the voice, Not important marks it irrelevant for good, the name field takes any other name.
//! New voices met once get their own silent card first (`voice_suggest.rs`).
//! Contract: `docs/speakers.md` ("Naming voices").

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use speakers::identity::EnrollmentStore;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::Duration;

pub const EFFORT: &str = "high";
/// The live loop's check cadence while a call is open.
pub const CHECK_EVERY: Duration = Duration::from_secs(60);
/// "Talking with that person now": the voice said something this recently.
pub const ACTIVE_MS: i64 = 3 * 60_000;
/// A voice that came back: heard on this many local days, in this many separate visits (stretches of its lines
/// with at least `VISIT_GAP_MS` between them), so one call past midnight is not a return.
pub const MIN_DAYS: usize = 2;
pub const MIN_VISITS: usize = 2;
const VISIT_GAP_MS: i64 = 3 * 3600_000;
/// About an hour of conversation across calls: five-minute stretches in which the voice spoke.
pub const MIN_MINUTES: u64 = 60;
const STRETCH_MS: i64 = 5 * 60_000;
/// Per voice: model calls this far apart, and only with this many new lines since the last one.
pub const RECHECK_MS: u64 = 20 * 60_000;
pub const NEW_LINES: usize = 15;
/// How long the card waits on the island.
pub const CARD_MS: u64 = 10 * 60_000;
/// A card shown and left unanswered is not shown again for this long.
pub const ASK_AGAIN_MS: u64 = 24 * 3600_000;
/// Independent hints the answer needs: lines naming the person, in separate moments.
pub const MIN_HINTS: usize = 3;
pub const MIN_MOMENTS: usize = 2;
/// Two hints are separate moments when they are in different sessions or this far apart.
const MOMENT_MS: i64 = 10 * 60_000;
/// Context bounds: the voice's newest lines, and lines around each.
const OWN_LINES: usize = 300;
const BEFORE: usize = 3;
const BEFORE_MS: i64 = 60_000;
const AFTER_MS: i64 = 20_000;
const LINE_CHARS: usize = 300;
/// The log and ledger label.
pub const JOB: &str = "voice namer";
/// Island card ids: `voice-name:<code>`; answers come back with that request id.
pub const CARD_PREFIX: &str = "voice-name:";
pub const YES: &str = "Yes";
pub const FORGET: &str = "Forget";

pub const SYSTEM: &str = "You identify one unnamed voice in User's recorded calls (mostly Discord). Heimdall, his \
assistant, labelled it with a code such as \"Speaker Fri 23 Oct 21h #3\"; in the transcript below its lines are \
marked VOICE. Other lines carry their speaker: User, a known person's name, or another code. You get the voice's \
lines from several calls and, around each, the lines just before and after it.

Decide whether the transcript shows this voice's name. Good evidence, strongest first:
- the voice says its own name (\"it's Dave\", \"Dave here\");
- someone addresses a person by name and the VOICE answers next, as the reply to that address (\"Dave, you there?\" \
then VOICE: \"yeah\");
- others talk to or about the voice by name in a way only it fits.
Not evidence: a name the VOICE itself says to someone else (that is another person); a name followed by a \
different speaker's answer; a name in a story, a game, a stream or a song; one moment repeated. ASR may misspell \
names; use the spelling the transcript uses most. Never answer User or Heimdall.

Answer a name only when several independent moments point at it and nothing points elsewhere. hints lists the \
line numbers that name the person, each a separate piece of evidence; quote_line is the clearest one, to show \
User. When unsure, answer name \"\" with confidence low and no hints: a wrong guess costs User more than none.";

/// Archived lines of one voice: how much of them there is.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Stats {
    pub lines: usize,
    pub days: BTreeSet<String>,
    /// Five-minute stretches with a line of the voice, in minutes.
    pub minutes: u64,
    pub last_ms: i64,
    /// Separate visits: runs of lines with gaps under `VISIT_GAP_MS`.
    pub visits: usize,
}
impl Stats {
    /// Came back, with about an hour of conversation.
    pub fn eligible(&self) -> bool {
        self.days.len() >= MIN_DAYS && self.visits >= MIN_VISITS && self.minutes >= MIN_MINUTES
    }
}

pub fn open(db: &Path) -> Result<Connection> {
    let c = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
    c.busy_timeout(Duration::from_secs(5))?;
    Ok(c)
}

pub fn stats(db: &Connection, code: &str) -> Result<Stats> {
    let mut s = Stats::default();
    let mut stretches = BTreeSet::new();
    let mut q = db.prepare_cached("SELECT coalesce(wall_ms,0) FROM utterances WHERE speaker=?1 AND coalesce(text,'')<>'' ORDER BY wall_ms")?;
    let mut previous: Option<i64> = None;
    for wall in q.query_map(params![code], |r| r.get::<_, i64>(0))? {
        let wall = wall?;
        if wall <= 0 {
            continue;
        }
        if previous.is_none_or(|p| wall - p >= VISIT_GAP_MS) {
            s.visits += 1;
        }
        previous = Some(wall);
        s.lines += 1;
        s.last_ms = s.last_ms.max(wall);
        s.days.insert(speakers::naming::voice_time(wall as u64).day);
        stretches.insert(wall / STRETCH_MS);
    }
    s.minutes = stretches.len() as u64 * (STRETCH_MS / 60_000) as u64;
    Ok(s)
}

/// Coded voices to look at now: in the store, not forgotten, with a line in the last `ACTIVE_MS`.
pub fn active(db: &Connection, store: &EnrollmentStore, now_ms: i64) -> Result<Vec<String>> {
    let mut q = db.prepare_cached("SELECT DISTINCT speaker FROM utterances WHERE wall_ms>=?1 AND speaker IS NOT NULL")?;
    let recent: Vec<String> = q.query_map(params![now_ms - ACTIVE_MS], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
    Ok(recent.into_iter().filter(|c| store.voices.get(c).is_some_and(|v| !v.irrelevant)).collect())
}

/// One transcript line the model sees, numbered.
#[derive(Clone, Debug, PartialEq)]
pub struct Line {
    pub n: usize,
    pub session: i64,
    pub wall_ms: i64,
    pub speaker: String,
    pub text: String,
    pub own: bool,
}

/// The voice's newest lines and the lines around each, in time order, numbered from 1.
pub fn context(db: &Connection, code: &str) -> Result<Vec<Line>> {
    type Row = (i64, String, i64, i64, String, String);
    let own: Vec<Row> = db
        .prepare("SELECT session,source_id,coalesce(start_ns,0),coalesce(wall_ms,0),coalesce(speaker,''),coalesce(text,'') FROM utterances WHERE speaker=?1 AND coalesce(text,'')<>'' ORDER BY wall_ms DESC LIMIT ?2")?
        .query_map(params![code, OWN_LINES as i64], |r| Ok((r.get(0)?, r.get::<_, Option<String>>(1)?.unwrap_or_default(), r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut picked: BTreeMap<(i64, i64, String), Line> = BTreeMap::new();
    let mut add = |session: i64, start: i64, wall: i64, speaker: String, text: String| {
        let text: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
        let text = if text.chars().count() > LINE_CHARS { format!("{}…", text.chars().take(LINE_CHARS).collect::<String>()) } else { text };
        picked.entry((wall, start, speaker.clone())).or_insert(Line { n: 0, session, wall_ms: wall, own: speaker == code, speaker, text });
    };
    let mut before = db.prepare("SELECT coalesce(start_ns,0),coalesce(wall_ms,0),coalesce(speaker,''),coalesce(text,'') FROM utterances WHERE session=?1 AND wall_ms<?2 AND wall_ms>=?3 AND coalesce(text,'')<>'' ORDER BY wall_ms DESC LIMIT ?4")?;
    let mut after = db.prepare("SELECT coalesce(start_ns,0),coalesce(wall_ms,0),coalesce(speaker,''),coalesce(text,'') FROM utterances WHERE session=?1 AND wall_ms>?2 AND wall_ms<=?3 AND coalesce(text,'')<>'' ORDER BY wall_ms LIMIT 1")?;
    for (session, _, start, wall, speaker, text) in own {
        add(session, start, wall, speaker, text);
        for r in before.query_map(params![session, wall, wall - BEFORE_MS, BEFORE as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))? {
            let (s, w, sp, t): (i64, i64, String, String) = r?;
            add(session, s, w, sp, t);
        }
        for r in after.query_map(params![session, wall, wall + AFTER_MS], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))? {
            let (s, w, sp, t): (i64, i64, String, String) = r?;
            add(session, s, w, sp, t);
        }
    }
    let mut lines: Vec<Line> = picked.into_values().collect();
    for (i, l) in lines.iter_mut().enumerate() {
        l.n = i + 1;
    }
    Ok(lines)
}

/// The user message: known names, then the lines grouped by call with local times.
pub fn prompt(code: &str, lines: &[Line], known: &[String], declined: &[String]) -> String {
    let mut p = format!("The voice: {code}.\n");
    if !known.is_empty() {
        p.push_str(&format!("Names Heimdall already knows (other voices): {}.\n", known.join(", ")));
    }
    if !declined.is_empty() {
        p.push_str(&format!("User already said it is not: {}.\n", declined.join(", ")));
    }
    p.push_str("\n<transcript note=\"[n] local time, speaker: words; a blank line between calls\">\n");
    let mut last: Option<(i64, i64)> = None;
    for l in lines {
        let gap = last.is_none_or(|(s, w)| s != l.session || l.wall_ms - w > 15 * 60_000);
        if gap {
            let t = speakers::naming::voice_time(l.wall_ms as u64);
            p.push_str(&format!("\n-- {} --\n", t.day));
        }
        last = Some((l.session, l.wall_ms));
        let hm = clock(l.wall_ms);
        let who = if l.own { "VOICE".to_string() } else if l.speaker.is_empty() { "unknown".into() } else { l.speaker.clone() };
        p.push_str(&format!("[{}] {hm} {who}: {}\n", l.n, l.text));
    }
    p.push_str("</transcript>\n\nWho is VOICE?");
    p
}

#[cfg(test)]
pub fn clock_for_tests(wall_ms: i64) -> String {
    clock(wall_ms)
}
fn clock(wall_ms: i64) -> String {
    let secs = (wall_ms / 1000) as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&secs, &mut tm) };
    format!("{:02}:{:02}", tm.tm_hour, tm.tm_min)
}

pub fn schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "name": {"type": "string"},
            "confidence": {"type": "string", "enum": ["high", "medium", "low"]},
            "hints": {"type": "array", "items": {"type": "integer"}},
            "quote_line": {"type": "integer"},
            "reason": {"type": "string"}
        },
        "required": ["name", "confidence", "hints", "quote_line", "reason"],
        "additionalProperties": false
    })
}

/// The run on the background model (`background_call`: adaptive thinking with `output_config.effort`, structured
/// output, the one-hour cache on the system prompt, the $0.50 limit). The user message is one voice's transcript,
/// different every run, so only the system prompt is marked for the cache.
pub fn call(model: &str, prompt: &str) -> crate::background_call::Call<'static> {
    crate::background_call::Call { job: JOB, model: model.to_owned(), system: SYSTEM, schema: schema(), effort: EFFORT, prompt: prompt.into(), history: Vec::new(), session: None }
}

#[cfg(test)]
pub fn request_body(prompt: &str) -> Value {
    request_body_for(&crate::background_model::anthropic_model(), prompt)
}

/// The Messages API body naming `model` (the endpoint's snapshot).
pub fn request_body_for(model: &str, prompt: &str) -> Value {
    crate::background_call::prepare(&call(model, prompt)).map(|p| p.body).unwrap_or(Value::Null)
}

/// What the card asks.
#[derive(Clone, Debug, PartialEq)]
pub struct Proposal {
    pub code: String,
    pub name: String,
    /// The clearest hint, as said: "the user: hey John, you there?".
    pub quote: String,
    pub hints: Vec<usize>,
}

fn words(s: &str) -> Vec<String> {
    s.split(|c: char| !(c.is_alphanumeric() || c == '\'' || c == '-')).filter(|w| !w.is_empty()).map(|w| w.to_lowercase()).collect()
}
/// Whether `text` says `name`, allowing the speech recogniser's spelling drift on longer names ("Fosta" for
/// "Morgan"): same first letter and at most two edits for names of five letters or more.
pub(crate) fn says(text: &str, name: &str) -> bool {
    let name = words(name);
    let text = words(text);
    !name.is_empty() && text.windows(name.len()).any(|w| w.iter().zip(&name).all(|(a, b)| same_name(a, b)))
}
/// Two names, word by word, with the same spelling tolerance.
pub(crate) fn same_names(a: &str, b: &str) -> bool {
    let (a, b) = (words(a), words(b));
    !a.is_empty() && a.len() == b.len() && a.iter().zip(&b).all(|(x, y)| same_name(x, y))
}
fn same_name(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    a.len().min(b.len()) >= 5 && a[0] == b[0] && edits(&a, &b) <= 2
}
fn edits(a: &[char], b: &[char]) -> usize {
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, x) in a.iter().enumerate() {
        let mut prev = row[0];
        row[0] = i + 1;
        for (j, y) in b.iter().enumerate() {
            let next = (row[j + 1] + 1).min(row[j] + 1).min(prev + usize::from(x != y));
            prev = row[j + 1];
            row[j + 1] = next;
        }
    }
    row[b.len()]
}

/// The model's answer, checked against the transcript: a plausible name, confidence high, at least `MIN_HINTS`
/// distinct lines that contain it in at least `MIN_MOMENTS` separate moments, a VOICE line never addressing it, and
/// not a name the user turned down or one of his own.
pub fn verify(code: &str, answer: &str, lines: &[Line], known: &[String], declined: &[String]) -> Result<Option<Proposal>> {
    let v: Value = serde_json::from_str(answer).context("answer is not JSON")?;
    let name = v["name"].as_str().unwrap_or("").split_whitespace().collect::<Vec<_>>().join(" ");
    if name.is_empty() || v["confidence"] != "high" {
        return Ok(None);
    }
    let lower = name.to_lowercase();
    if name.chars().count() > 40
        || name.split_whitespace().count() > 3
        || !name.chars().all(|c| c.is_alphabetic() || c == ' ' || c == '-' || c == '\'')
        || ["owner", "heimdall", "voice", "speaker", "unknown"].contains(&lower.as_str())
        || declined.iter().any(|d| same_names(d, &name))
        // Someone Heimdall already knows is another voice: Yes would mix this voice into their samples.
        || known.iter().any(|k| same_names(k, &name))
    {
        return Ok(None);
    }
    let by_n: BTreeMap<usize, &Line> = lines.iter().map(|l| (l.n, l)).collect();
    let mut hints: Vec<&Line> = vec![];
    for n in v["hints"].as_array().into_iter().flatten().filter_map(Value::as_u64) {
        let Some(l) = by_n.get(&(n as usize)) else { continue };
        // The voice saying its own name, or someone saying it with the voice answering next ("hey John" belongs to
        // whoever answers): the next line of that call is the voice's own.
        let answered = || lines.iter().find(|x| x.n > l.n && x.session == l.session).is_some_and(|x| x.own && x.wall_ms - l.wall_ms <= AFTER_MS);
        let counts = if l.own { introduces(&l.text, &name) } else { answered() };
        if counts && says(&l.text, &name) && !hints.iter().any(|h| h.n == l.n) {
            hints.push(l);
        }
    }
    // The voice naming someone else is evidence against: it would be addressing that person.
    let addressed_by_voice = lines.iter().filter(|l| l.own && says(&l.text, &name)).any(|l| !introduces(&l.text, &name));
    if hints.len() < MIN_HINTS || addressed_by_voice {
        return Ok(None);
    }
    let mut moments: Vec<(i64, i64)> = vec![];
    for h in &hints {
        if !moments.iter().any(|(s, w)| *s == h.session && (h.wall_ms - w).abs() < MOMENT_MS) {
            moments.push((h.session, h.wall_ms));
        }
    }
    if moments.len() < MIN_MOMENTS {
        return Ok(None);
    }
    let quote_n = v["quote_line"].as_u64().map(|n| n as usize).filter(|n| hints.iter().any(|h| h.n == *n)).unwrap_or(hints[0].n);
    let q = by_n[&quote_n];
    let who = if q.own { "They said" } else if q.speaker.is_empty() { "Someone" } else { q.speaker.as_str() };
    Ok(Some(Proposal { code: code.into(), name, quote: format!("{who}: {}", q.text), hints: hints.iter().map(|h| h.n).collect() }))
}

/// "I'm Dave", "it's Dave", "Dave here": the voice saying its own name.
pub(crate) fn introduces(text: &str, name: &str) -> bool {
    let t = format!(" {} ", words(text).join(" "));
    let n = words(name).join(" ");
    [format!(" i'm {n} "), format!(" i am {n} "), format!(" it's {n} "), format!(" this is {n} "), format!(" {n} here "), format!(" my name is {n} "), format!(" call me {n} ")]
        .iter()
        .any(|p| t.contains(p.as_str()))
}

/// Per voice: when the model last looked, how many lines it had then, the card shown, the names turned down.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct VoiceState {
    #[serde(default)]
    pub checked_ms: u64,
    #[serde(default)]
    pub lines_at_check: usize,
    #[serde(default)]
    pub asked: Option<(String, u64)>,
    #[serde(default)]
    pub declined: Vec<String>,
    /// New-voice suggestions (`voice_suggest.rs`): when its first card showed (0: never) and when it may be suggested
    /// again (a card left unanswered). An older file's `mode` and `figured_ms` (Ask me after, Only if sure) are dropped.
    #[serde(default)]
    pub suggested_ms: u64,
    #[serde(default)]
    pub snoozed_until: u64,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct State {
    #[serde(default)]
    pub voices: BTreeMap<String, VoiceState>,
    /// When each new-voice card was shown (Unix ms, the last day): the rate limit.
    #[serde(default)]
    pub shown: Vec<u64>,
}
impl State {
    pub fn load(path: &Path) -> State {
        std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }
    pub fn save(&self, path: &Path) -> Result<()> {
        agent::runtime::atomic_write(path, &serde_json::to_vec_pretty(self)?)
    }
}

/// What one pass did, for the log and the tests.
#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    /// No coded voice is talking now.
    Idle,
    /// Talking, but not yet a voice to name: (code, why).
    Waiting(Vec<(String, String)>),
    /// The model looked and did not name it (or its answer failed the checks).
    Unsure(String),
    Propose(Proposal),
    /// A new voice to show on the island now, with its guessed names (`voice_suggest::next`, then `guess`).
    Suggest(crate::voice_suggest::Suggestion),
}

/// One pass at `now_ms`. `ask` is the model (system prompt is `SYSTEM`; it gets the user message, returns the JSON
/// answer). At most one model call per pass: the voice talking most recently that is due.
pub fn pass(db: &Connection, store: &EnrollmentStore, state: &mut State, now_ms: u64, ask: &mut dyn FnMut(&str) -> Result<String>) -> Result<Outcome> {
    let codes = active(db, store, now_ms as i64)?;
    if codes.is_empty() {
        return Ok(Outcome::Idle);
    }
    let mut waiting = vec![];
    let mut due: Vec<(String, Stats)> = vec![];
    for code in codes {
        let s = stats(db, &code)?;
        let v = state.voices.get(&code).cloned().unwrap_or_default();
        let why = if !s.eligible() {
            Some(format!("{} day(s), {} visit(s), {} min", s.days.len(), s.visits, s.minutes))
        } else if v.asked.as_ref().is_some_and(|(_, at)| now_ms.saturating_sub(*at) < ASK_AGAIN_MS) {
            Some("asked recently".into())
        } else if v.checked_ms > 0 && (now_ms.saturating_sub(v.checked_ms) < RECHECK_MS || s.lines < v.lines_at_check + NEW_LINES) {
            Some("checked recently".into())
        } else {
            None
        };
        match why {
            Some(w) => waiting.push((code, w)),
            None => due.push((code, s)),
        }
    }
    let Some((code, s)) = due.into_iter().max_by_key(|(_, s)| s.last_ms) else {
        return Ok(Outcome::Waiting(waiting));
    };
    let lines = context(db, &code)?;
    let known: Vec<String> = store.names.keys().cloned().collect();
    let declined = state.voices.get(&code).map(|v| v.declined.clone()).unwrap_or_default();
    let v = state.voices.entry(code.clone()).or_default();
    v.checked_ms = now_ms;
    v.lines_at_check = s.lines;
    let answer = ask(&prompt(&code, &lines, &known, &declined))?;
    match verify(&code, &answer, &lines, &known, &declined)? {
        Some(p) => {
            state.voices.entry(code).or_default().asked = Some((p.name.clone(), now_ms));
            Ok(Outcome::Propose(p))
        }
        None => Ok(Outcome::Unsure(code)),
    }
}

/// The real model: one run on `e` (`background_model::endpoint`: the background model and, only while it is on
/// Anthropic, the provider's stored key). Errors never carry the key.
pub fn ask_model(e: &crate::plan_refresher::Endpoint, prompt: &str) -> Result<String> {
    let model = e.model.clone().unwrap_or_else(crate::background_model::anthropic_model);
    Ok(crate::background_call::ask_blocking(e, &call(&model, prompt))?.text)
}

/// The card's words: "Is this John?" and the quote, cut to the island's one line (`voice_suggest::fitted`).
pub fn card_text(p: &Proposal) -> String {
    let lead = format!("Is this {}?", p.name);
    crate::voice_suggest::fitted(&format!("{lead} "), &p.quote, "").unwrap_or(lead)
}

#[cfg(test)]
#[path = "voice_namer_tests.rs"]
mod tests;
