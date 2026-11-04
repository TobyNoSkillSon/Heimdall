//! The taste ledger: "a parallel library of the user's taste: what the user likes, doesn't
//! like, complains about", kept from now on so that later a model, or a reviewer that checks work before the user
//! sees it, can learn from it. No training and no reviewer yet; this is only the data, shaped for them
//! (`docs/taste-ledger.md`).
//!
//! One append-only JSON-lines file, `<data>/taste/ledger.jsonl`, one judgement per line (`Entry`). Writers:
//! - the agent's `taste_note`, the moment it hears a verdict (`by: agent`);
//! - `Heimdall --taste add`, which a review task runs after reading `Heimdall --taste review` (the user, 03:31: no
//!   background pass; "from time to time, weekly, I can tell you to go through the last week and check if you missed
//!   something"), `by: review`; the one-off offline seed wrote `by: seed`.
//! Readers: the agent's `taste_search`, `Heimdall --taste search`, and any worker that reads the file.
//!
//! Every append takes an exclusive lock on the file, re-reads it and skips a duplicate (`duplicate_of`), so the app
//! and the command line can write at once, a replayed call or a review over a week already covered adds nothing. A
//! torn last line (a crash mid-write) is ended before the next entry, and unreadable lines are skipped on read.
use anyhow::{bail, ensure, Context, Result};
use chrono::{DateTime, Local, NaiveDate, NaiveDateTime, NaiveTime, TimeZone};
use fs2::FileExt;
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

pub const SCHEMA: u32 = 1;
/// What the user did: praised it, rejected or complained about it, corrected Heimdall, or stated a standing taste.
pub const VERDICTS: [&str; 4] = ["like", "dislike", "correction", "preference"];
/// What was judged.
pub const KINDS: [&str; 11] = ["report", "page", "app", "icon", "image", "voice", "audio", "writing", "code", "behaviour", "other"];
/// Who wrote an entry.
pub const BY: [&str; 3] = ["agent", "review", "seed"];
pub const SUBJECT_CHARS: usize = 200;
pub const WORDS_CHARS: usize = 2000;
pub const WHY_CHARS: usize = 400;
pub const REF_CHARS: usize = 4096;
pub const MAX_LINES: usize = 20;
pub const MAX_FILES: usize = 10;
/// `taste_search` answers at most this many entries.
pub const SEARCH_LIMIT: usize = 20;
/// A review page's size in characters: what one file read of a worker takes comfortably.
pub const PAGE_CHARS: usize = 24_000;
/// Two entries with the same words (or one's inside the other's) are the same judgement only this close in time, or
/// said in the same line: "much better" about the icon on Monday and about a report on Friday are two.
const NEAR_MS: i64 = 12 * 3600_000;
/// `--taste review` lists the newest known entries on page 1 within this many characters.
const KNOWN_CHARS: usize = 16_000;
/// Words shorter than this (normalized) only match exactly: "no" inside "no, that's great" is no duplicate.
const CONTAINED_CHARS: usize = 16;

/// One judgement. `at` is when the user said it (local time with its offset); `recorded` when it was written.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub schema: u32,
    pub id: String,
    pub at: String,
    pub recorded: String,
    /// What was judged, in a few words: "benchmark report", "Heimdall's app icon", "spoken replies".
    pub subject: String,
    pub kind: String,
    pub verdict: String,
    /// The user's exact words, in the language he said them.
    pub words: String,
    /// The underlying quality he cares about, in one line: the part a model should learn. Heimdall's reading of his
    /// words, not his words.
    pub why: String,
    /// Archive transcript line ids (`session-…:159`, as `Heimdall --taste review` and the archive name them): first
    /// the line he said it in, then any context lines (what Heimdall said or showed before).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lines: Vec<String>,
    /// When the screen showed what he judged (RFC3339): the archived frame `screen_frames` returns for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screen_at: Option<String>,
    /// The judged artefact's files or URLs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
    /// The earlier entry this one reacts to: the work changed after that verdict and he judged the change. A pair
    /// shows how far a fix may go: "too many headings", then of the stripped-down redo "now it's too plain".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follows: Option<String>,
    pub by: String,
}

impl Entry {
    /// An entry from `taste_note`'s arguments (or one JSON object of `--taste add`), checked here as well as by the
    /// tool schema, since the command line has none. `at` defaults to the first line's archived time, then now.
    pub fn from_args(args: &Value, by: &str, now_ms: i64, line_time: impl Fn(&str) -> Option<i64>) -> Result<Entry> {
        ensure!(args.is_object(), "an entry is one JSON object");
        let text = |key: &str, max: usize| -> Result<String> {
            let v = args[key].as_str().with_context(|| format!("{key}: required text"))?.trim().to_owned();
            ensure!(!v.is_empty() && v.chars().count() <= max, "{key}: 1 to {max} characters");
            Ok(v)
        };
        let one_of = |key: &str, values: &[&str]| -> Result<String> {
            let v = args[key].as_str().with_context(|| format!("{key}: required, one of {}", values.join(", ")))?;
            ensure!(values.contains(&v), "{key}: one of {}", values.join(", "));
            Ok(v.to_owned())
        };
        let list = |key: &str, max: usize| -> Result<Vec<String>> {
            let Some(v) = args.get(key) else { return Ok(vec![]) };
            // One id written as text instead of a list (a worker's usual slip) is that one id.
            let one = v.is_string().then(|| vec![v.clone()]);
            let items = one.as_ref().or(v.as_array()).with_context(|| format!("{key}: a list of text"))?;
            ensure!(items.len() <= max, "{key}: at most {max}");
            items
                .iter()
                .map(|i| {
                    let s = i.as_str().map(str::trim).filter(|s| !s.is_empty() && s.len() <= REF_CHARS);
                    s.map(str::to_owned).with_context(|| format!("{key}: each item is non-empty text"))
                })
                .collect()
        };
        for key in args.as_object().unwrap().keys() {
            ensure!(
                matches!(key.as_str(), "subject" | "kind" | "verdict" | "words" | "why" | "at" | "lines" | "screen_at" | "files" | "follows"),
                "{key}: unknown field"
            );
        }
        let lines: Vec<String> = list("lines", MAX_LINES)?.iter().map(|l| line_id(l).to_owned()).collect();
        let at_ms = match args.get("at") {
            Some(v) => parse_time(v.as_str().context("at: RFC3339 text")?)?,
            None => lines.first().and_then(|l| line_time(l)).unwrap_or(now_ms),
        };
        let screen_at = match args.get("screen_at") {
            Some(v) => Some(rfc3339(parse_time(v.as_str().context("screen_at: RFC3339 text")?)?)),
            None => None,
        };
        ensure!(BY.contains(&by), "by: one of {}", BY.join(", "));
        Ok(Entry {
            schema: SCHEMA,
            id: format!("taste-{now_ms}-{}", &uuid::Uuid::new_v4().simple().to_string()[..6]),
            at: rfc3339(at_ms),
            recorded: rfc3339(now_ms),
            subject: text("subject", SUBJECT_CHARS)?,
            kind: one_of("kind", &KINDS)?,
            verdict: one_of("verdict", &VERDICTS)?,
            words: text("words", WORDS_CHARS)?,
            why: text("why", WHY_CHARS)?,
            lines,
            screen_at,
            files: list("files", MAX_FILES)?,
            follows: match args.get("follows") {
                Some(v) => Some(v.as_str().filter(|f| f.starts_with("taste-") && f.len() <= 64).context("follows: an entry id, taste-…")?.to_owned()),
                None => None,
            },
            by: by.into(),
        })
    }

    pub fn at_ms(&self) -> i64 {
        DateTime::parse_from_rfc3339(&self.at).map_or(0, |t| t.timestamp_millis())
    }

    /// One line for the agent and for review pages: `Fri 23 Oct 21:04 · dislike · icon · Heimdall's app icon: "…" (why) [id]`.
    pub fn line(&self) -> String {
        let when = Local.timestamp_millis_opt(self.at_ms()).single().map_or_else(|| self.at.clone(), |t| t.format("%a %-d %b %H:%M").to_string());
        let follows = self.follows.as_ref().map(|f| format!(" after {f}")).unwrap_or_default();
        format!("{when} · {} · {} · {}: \"{}\" ({}) [{}]{follows}", self.verdict, self.kind, self.subject, clip(&self.words, 300), self.why, self.id)
    }
}

/// A transcript line's archive id: `timeline_search` names a line `49:text:utterance:session-…:159` (or `…:turn:…`
/// for a whole turn); the ledger keeps the line's own id, `session-…:159`, the one reviews cite.
pub fn line_id(r: &str) -> &str {
    let mut parts = r.splitn(4, ':');
    match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some(n), Some("text"), Some("utterance" | "turn"), Some(id)) if n.chars().all(|c| c.is_ascii_digit()) => id,
        _ => r,
    }
}

/// `<data>/taste/ledger.jsonl`.
pub fn path(data: &Path) -> PathBuf {
    data.join("taste").join("ledger.jsonl")
}

/// Every readable entry, oldest first; unreadable lines (a torn write) are skipped.
pub fn load(ledger: &Path) -> Result<Vec<Entry>> {
    match std::fs::read_to_string(ledger) {
        Ok(text) => Ok(parse(&text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(vec![]),
        Err(e) => Err(e).with_context(|| format!("reading {}", ledger.display())),
    }
}

fn parse(text: &str) -> Vec<Entry> {
    text.lines().filter(|l| !l.trim().is_empty()).filter_map(|l| serde_json::from_str(l).ok()).collect()
}

/// Lowercase letters and digits, words single-spaced: the form duplicates are compared in.
fn normalized(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_alphanumeric() { c.to_lowercase().next().unwrap_or(c) } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// The entry `e` repeats, if any: one with the same verdict and the same words (case and punctuation aside), or words
/// containing or contained in `e`'s (`CONTAINED_CHARS` or more), said in the same line (when both cite the line they
/// were said in) or else within `NEAR_MS`, and, when `e` reacts to an earlier entry, reacting to the same one. Two
/// verdicts in one line ("the layout is great but the colours are too dark": a like quoting part of it, a dislike
/// quoting all of it) are two; the same words in two different lines, or about a redo with `follows`, or a stock
/// phrase on another day: none of them is a repeat. Kind and subject are not compared: they are the writer's reading,
/// and a review pass may file the agent's "report" entry as a "page" or name its subject differently, which must
/// still be a repeat.
pub fn duplicate_of<'a>(existing: &'a [Entry], e: &Entry) -> Option<&'a Entry> {
    let words = normalized(&e.words);
    existing.iter().find(|x| {
        let other = normalized(&x.words);
        let (short, long) = if other.len() < words.len() { (&other, &words) } else { (&words, &other) };
        // Containment counts whole words only: "number 1" is not inside "number 10".
        let contained = short.len() >= CONTAINED_CHARS && format!(" {long} ").contains(&format!(" {short} "));
        let same_words = !words.is_empty() && (other == words || contained);
        // Each cites the line it was said in: the same line or not, whatever the time. Else, close in time.
        let close = match (x.lines.first(), e.lines.first()) {
            (Some(a), Some(b)) => a == b,
            _ => (x.at_ms() - e.at_ms()).abs() <= NEAR_MS,
        };
        // A repeat without `follows` repeats whichever entry it matches; one with it, only an entry with the same.
        x.verdict == e.verdict && same_words && close && (e.follows.is_none() || x.follows == e.follows)
    })
}

/// What an append did.
#[derive(Debug, PartialEq)]
pub enum Added {
    New(String),
    Duplicate(String),
}

/// Appends `e` unless it repeats an entry, under an exclusive lock on the file (the app and the command line may
/// write at once). The file and its folder are private to the user.
pub fn append(ledger: &Path, e: &Entry) -> Result<Added> {
    let dir = ledger.parent().context("ledger path has no folder")?;
    std::fs::create_dir_all(dir)?;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    let mut f = std::fs::OpenOptions::new().read(true).append(true).create(true).mode(0o600).open(ledger)?;
    f.lock_exclusive()?;
    let result = (|| {
        let mut text = String::new();
        f.seek(SeekFrom::Start(0))?;
        f.read_to_string(&mut text)?;
        let existing = parse(&text);
        if let Some(d) = duplicate_of(&existing, e) {
            return Ok(Added::Duplicate(d.id.clone()));
        }
        if let Some(f) = &e.follows {
            ensure!(existing.iter().any(|x| &x.id == f), "follows: no entry {f} in the ledger; find it with taste_search");
        }
        let mut line = String::new();
        if !text.is_empty() && !text.ends_with('\n') {
            line.push('\n'); // end a torn last line, so this entry stays readable
        }
        line.push_str(&serde_json::to_string(e)?);
        line.push('\n');
        f.write_all(line.as_bytes())?;
        f.sync_data()?;
        Ok(Added::New(e.id.clone()))
    })();
    let _ = FileExt::unlock(&f);
    result
}

/// Words a topic query ignores.
const STOP: [&str; 12] = ["the", "and", "for", "with", "that", "this", "was", "are", "his", "her", "about", "from"];

/// Entries matching `query` by topic, best first: an entry matches when any query word (3+ letters and not a
/// `STOP` word, or any word of a one-word query) is in its subject, kind, words or why; more matched words rank
/// higher, then newer.
pub fn search<'a>(entries: &'a [Entry], query: Option<&str>, verdict: Option<&str>, since_ms: Option<i64>, limit: usize) -> Vec<&'a Entry> {
    let terms: Vec<String> = query.map(normalized).unwrap_or_default().split(' ').filter(|w| !w.is_empty()).map(str::to_owned).collect();
    let terms: Vec<&String> = if terms.len() == 1 { terms.iter().collect() } else { terms.iter().filter(|w| w.chars().count() >= 3 && !STOP.contains(&w.as_str())).collect() };
    let mut hits: Vec<(usize, &Entry)> = entries
        .iter()
        .filter(|e| verdict.is_none_or(|v| e.verdict == v))
        .filter(|e| since_ms.is_none_or(|s| e.at_ms() >= s))
        .filter_map(|e| {
            if terms.is_empty() {
                return Some((0, e));
            }
            let hay = normalized(&format!("{} {} {} {}", e.subject, e.kind, e.words, e.why));
            let hay: Vec<&str> = hay.split(' ').collect();
            // A term matches a word that starts with it, plural or not ("report" and "reports" find "reports").
            let n = terms.iter().filter(|t| {
                let stem = if t.chars().count() > 3 { t.strip_suffix('s').unwrap_or(t) } else { t.as_str() };
                hay.iter().any(|w| w.starts_with(stem))
            }).count();
            (n > 0).then_some((n, e))
        })
        .collect();
    hits.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.at_ms().cmp(&a.1.at_ms())));
    hits.into_iter().take(limit).map(|(_, e)| e).collect()
}

/// `taste_search` and `--taste search`: one line per entry, under the tool answer's size.
pub fn search_answer(ledger: &Path, query: Option<&str>, verdict: Option<&str>, since: Option<&str>, limit: usize) -> Result<String> {
    let since_ms = since.map(parse_time).transpose()?;
    let entries = load(ledger)?;
    let hits = search(&entries, query, verdict, since_ms, limit.clamp(1, SEARCH_LIMIT));
    if hits.is_empty() {
        return Ok(format!("No judgements match ({} in the ledger).", entries.len()));
    }
    let mut out = format!("{} of {} judgements, best match first:\n", hits.len(), entries.len());
    for e in hits {
        let line = e.line();
        if out.len() + line.len() + 1 > crate::mcp_contract::TEXT_BYTES - 200 {
            out.push_str("… narrow the query for more.\n");
            break;
        }
        out.push_str(&line);
        out.push('\n');
    }
    Ok(out.trim_end().into())
}

/// `taste_note`: records one judgement; a repeat answers with the entry it repeats.
pub fn note(ledger: &Path, args: &Value, archive: Option<&Path>) -> Result<String> {
    let times = archive.map(LineTimes::new);
    let e = Entry::from_args(args, "agent", crate::now_ms() as i64, |id| times.as_ref().and_then(|t| t.get(id)))?;
    Ok(match append(ledger, &e)? {
        Added::New(id) => format!("Noted: {id}."),
        Added::Duplicate(id) => format!("Already in the ledger as {id}; nothing added."),
    })
}

// ---------------------------------------------------------------------------------------------------------
// Times.

fn rfc3339(ms: i64) -> String {
    Local.timestamp_millis_opt(ms).single().unwrap_or_else(Local::now).to_rfc3339_opts(chrono::SecondsFormat::Secs, false)
}

/// RFC3339 (`2026-10-24T01:16:00+02:00`), or a local `2026-10-24`, `2026-10-24 01:16` or `2026-10-24T01:16`.
pub fn parse_time(text: &str) -> Result<i64> {
    let text = text.trim();
    if let Ok(t) = DateTime::parse_from_rfc3339(text) {
        return Ok(t.timestamp_millis());
    }
    let local = if let Ok(d) = NaiveDate::parse_from_str(text, "%Y-%m-%d") {
        d.and_time(NaiveTime::MIN)
    } else {
        NaiveDateTime::parse_from_str(&text.replacen('T', " ", 1), "%Y-%m-%d %H:%M")
            .map_err(|_| anyhow::anyhow!("time: use RFC3339, or a local 2026-10-24 or 2026-10-24 01:16"))?
    };
    // A time in the spring-forward gap does not exist: the hour after it is meant.
    let r = Local.from_local_datetime(&local).earliest().or_else(|| Local.from_local_datetime(&(local + chrono::Duration::hours(1))).earliest());
    r.map(|t| t.timestamp_millis()).context("time: not a local time")
}

/// The end of a `--to`: a bare date means the whole day.
fn parse_end(text: &str) -> Result<i64> {
    let start = parse_time(text)?;
    Ok(if NaiveDate::parse_from_str(text.trim(), "%Y-%m-%d").is_ok() { parse_time(&next_day(text.trim())?)? } else { start })
}

fn next_day(date: &str) -> Result<String> {
    let d = NaiveDate::parse_from_str(date, "%Y-%m-%d")?;
    Ok(d.succ_opt().context("date out of range")?.format("%Y-%m-%d").to_string())
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.into() } else { format!("{}…", s.chars().take(n).collect::<String>()) }
}

// ---------------------------------------------------------------------------------------------------------
// The archive's transcript, read-only, for reviews.

/// `<data>/archive/heimdall.sqlite`.
pub fn archive_path(data: &Path) -> PathBuf {
    data.join("archive").join("heimdall.sqlite")
}

fn open_archive(db: &Path) -> Result<Connection> {
    ensure!(db.is_file(), "no archive at {}", db.display());
    let c = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
    c.busy_timeout(std::time::Duration::from_secs(5))?;
    Ok(c)
}

/// When transcript lines were heard, by archive id: one read-only connection, opened at the first lookup, and the
/// `(session, id)` key (a line id starts with its session's name).
pub struct LineTimes {
    db: PathBuf,
    conn: std::cell::OnceCell<Option<Connection>>,
}

impl LineTimes {
    pub fn new(db: &Path) -> Self {
        LineTimes { db: db.into(), conn: std::cell::OnceCell::new() }
    }
    pub fn get(&self, id: &str) -> Option<i64> {
        let c = self.conn.get_or_init(|| open_archive(&self.db).ok()).as_ref()?;
        let session = id.split_once(':')?.0;
        c.query_row(
            "SELECT u.wall_ms FROM utterances u WHERE u.session=(SELECT s.id FROM sessions s WHERE s.name=?1) AND u.id=?2",
            params![session, id],
            |r| r.get::<_, Option<i64>>(0),
        )
        .optional()
        .ok()
        .flatten()
        .flatten()
    }
}

/// The user's own lines (his microphones) and Heimdall's, in a time range: what a review reads. Other people's
/// lines are left out; a verdict is the user's.
pub struct Line {
    pub id: String,
    pub wall_ms: i64,
    pub speaker: String,
    pub text: String,
}

pub fn transcript(db: &Path, from_ms: i64, to_ms: i64) -> Result<Vec<Line>> {
    let c = open_archive(db)?;
    let mut q = c.prepare(
        "SELECT id, wall_ms, coalesce(speaker,''), text FROM utterances INDEXED BY utterance_time
         WHERE wall_ms >= ?1 AND wall_ms < ?2 AND coalesce(text,'') <> ''
           AND (source_id LIKE 'mic:%' OR speaker = 'Heimdall') ORDER BY wall_ms, id",
    )?;
    let rows = q.query_map(params![from_ms, to_ms], |r| Ok(Line { id: r.get(0)?, wall_ms: r.get(1)?, speaker: r.get(2)?, text: r.get(3)? }))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// What the review task is told on page 1: what to look for and how to add it.
pub const REVIEW_GUIDE: &str = "\
You are reviewing the user's transcript for judgements Heimdall did not record in the taste ledger.
A judgement is the user's own verdict on something he saw, heard or used: praise (\"this is really well made\"),
rejection or complaint (\"that logo looks cheap\"), a correction of Heimdall (\"stop apologising every time\") or a
standing preference (\"I want fewer headings\"). Not judgements: plans, questions, instructions with no verdict in
them, banter and game talk with friends, and Heimdall's own lines (they are context: what he reacted to).
For each judgement not already listed under \"Already in the ledger\", write one JSON object, one per line, with:
  subject  what was judged, a few words (\"benchmark report\", \"Heimdall's app icon\", \"spoken replies\")
  kind     one of report, page, app, icon, image, voice, audio, writing, code, behaviour, other
  verdict  one of like, dislike, correction, preference
  words    his exact words from the line, unchanged
  why      the quality he cares about, one short line (\"reports must read at a glance\")
  lines    line ids, first the line he said it in: the session name of the \"## \" header above the line, a colon
           and the text after the line's colon exactly as shown (header \"## session-1792…-ab12 (Fri 23 Oct 2026)\",
           line \"21:04:05 Sam :159 …\" give \"session-1792…-ab12:159\"; the whole session name, never shortened)
  files    optional: paths or URLs of what he judged, when the lines name them
  follows  optional: the id of an earlier entry this reacts to, when he judges a change made after that verdict
After each page, add that page's entries at once, before reading on, with EOF alone at the start of its line:
HEIMDALL add --by review <<'EOF'
{\"subject\":…}
EOF
It skips duplicates, checks the line ids and fills each entry's time from its first line. Write no files. Be
selective: a missed real verdict is cheap, a wrong one teaches the wrong taste. At the end, report how many you
added, with each subject and verdict.";

/// `--taste review`: page `page` (1-based) of the guide, the entries already in the range and the transcript.
pub fn review(data: &Path, exe: &str, from_ms: i64, to_ms: i64, page: usize) -> Result<String> {
    // Page 1 is the guide and the entries already in the range; the transcript starts on page 2. Its pages are cut
    // from the transcript alone, so entries the worker adds between pages never move a page boundary.
    let lines = transcript(&archive_path(data), from_ms, to_ms)?;
    let pages = transcript_pages(&lines);
    let total = pages.len() + 1;
    ensure!(page >= 1 && page <= total, "page {page}: this range has {total} page(s)");
    let mut out = String::new();
    if page == 1 {
        out.push_str(&REVIEW_GUIDE.replace("HEIMDALL", exe));
        out.push_str("\n\nAlready in the ledger:\n");
        let entries = load(&path(data))?;
        let known: Vec<&Entry> = entries.iter().filter(|e| (from_ms..to_ms).contains(&e.at_ms())).collect();
        if known.is_empty() {
            out.push_str("(none in this range)\n");
        }
        // Newest kept, within `KNOWN_CHARS`, each with the line it cites.
        let mut shown: Vec<String> = vec![];
        let mut used = 0;
        for e in known.iter().rev() {
            let line = format!("{}{}\n", clip(&e.line(), 400), e.lines.first().map(|l| format!(" line {l}")).unwrap_or_default());
            if used + line.len() > KNOWN_CHARS {
                break;
            }
            used += line.len();
            shown.push(line);
        }
        shown.iter().rev().for_each(|l| out.push_str(l));
        if shown.len() < known.len() {
            out.push_str(&format!("(and {} older ones; add skips any repeat)\n", known.len() - shown.len()));
        }
        out.push_str(&format!("\nTranscript: {} lines of the user and Heimdall, on pages 2 to {total}.\n", lines.len()));
        if pages.is_empty() {
            out.push_str("(no transcript lines in this range)\n");
        }
    } else {
        out.push_str(&pages[page - 2]);
    }
    if page < total {
        out.push_str(&format!("\n[page {page} of {total}; next: the same command with --page {}]\n", page + 1));
    } else {
        out.push_str(&format!("\n[page {page} of {total}; end of the transcript]\n"));
    }
    Ok(out)
}

/// The transcript in pages of at most about `PAGE_CHARS`, cut between lines: a header per session, repeated
/// ("continued") at the top of a page that starts inside one, then `HH:MM:SS speaker :n text`. A line id is
/// `<session name>:<rest>`; session names hold no colon.
fn transcript_pages(lines: &[Line]) -> Vec<String> {
    let mut pages = vec![];
    let mut cur = String::new();
    let mut rows = 0;
    let mut session = "";
    let mut header = String::new();
    for l in lines {
        let (s, n) = l.id.split_once(':').unwrap_or((l.id.as_str(), ""));
        let at = Local.timestamp_millis_opt(l.wall_ms).single();
        let row = format!("{} {} :{n} {}\n", at.map(|t| t.format("%H:%M:%S").to_string()).unwrap_or_default(), l.speaker, l.text.split_whitespace().collect::<Vec<_>>().join(" "));
        let new_session = s != session;
        if new_session {
            header = format!("## {s} ({})\n", at.map(|t| t.format("%a %-d %b %Y").to_string()).unwrap_or_default());
            session = s;
        }
        let need = row.len() + if new_session { header.len() + 1 } else { 0 };
        // Full: start a new page, unless this one holds no line yet (a single overlong line still gets its page).
        if rows > 0 && cur.len() + need > PAGE_CHARS {
            pages.push(std::mem::take(&mut cur));
            rows = 0;
            if !new_session {
                cur = header.replacen(')', ", continued)", 1);
            }
        }
        if new_session {
            if !cur.is_empty() {
                cur.push('\n');
            }
            cur.push_str(&header);
        }
        cur.push_str(&row);
        rows += 1;
    }
    if rows > 0 {
        pages.push(cur);
    }
    pages
}

// ---------------------------------------------------------------------------------------------------------
// `Heimdall --taste …`

pub const USAGE: &str = "usage: Heimdall --taste [--data-dir DIR] COMMAND
  review --from DATE [--to DATE] [--page N]   the transcript of a range, for finding missed judgements
  add [--by review|seed]                      JSON entries from stdin, one per line; duplicates skipped
  search [WORDS] [--verdict V] [--since DATE] [--limit N]
  path                                        the ledger file
DATE: 2026-10-24 (a --to date includes that day), 2026-10-24 01:16, or RFC3339.";

pub fn cli(args: &[String], default_data: impl FnOnce() -> Result<PathBuf>, stdin: &mut dyn Read) -> Result<String> {
    let mut data = None;
    let mut rest = vec![];
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--data-dir" {
            data = Some(PathBuf::from(it.next().with_context(|| format!("--data-dir needs a folder\n{USAGE}"))?));
        } else {
            rest.push(a.clone());
        }
    }
    let data = match data {
        Some(d) => d,
        None => default_data()?,
    };
    let Some((command, rest)) = rest.split_first() else { bail!("{USAGE}") };
    let mut flags = std::collections::HashMap::new();
    let mut words = vec![];
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        if let Some(name) = a.strip_prefix("--") {
            flags.insert(name.to_owned(), it.next().with_context(|| format!("{a} needs a value\n{USAGE}"))?.clone());
        } else {
            words.push(a.clone());
        }
    }
    let known = |allowed: &[&str]| -> Result<()> {
        for k in flags.keys() {
            ensure!(allowed.contains(&k.as_str()), "--{k}: not an option of {command}\n{USAGE}");
        }
        Ok(())
    };
    let ledger = path(&data);
    match command.as_str() {
        "path" => Ok(ledger.display().to_string()),
        "search" => {
            known(&["verdict", "since", "limit"])?;
            let limit = flags.get("limit").map(|l| l.parse::<usize>()).transpose().map_err(|_| anyhow::anyhow!("--limit takes a number"))?;
            let query = (!words.is_empty()).then(|| words.join(" "));
            search_answer(&ledger, query.as_deref(), flags.get("verdict").map(String::as_str), flags.get("since").map(String::as_str), limit.unwrap_or(SEARCH_LIMIT))
        }
        "review" => {
            known(&["from", "to", "page"])?;
            let from = parse_time(flags.get("from").with_context(|| format!("review needs --from\n{USAGE}"))?)?;
            let to = flags.get("to").map(|t| parse_end(t)).transpose()?.unwrap_or(crate::now_ms() as i64 + 1);
            ensure!(from < to, "--to is before --from");
            let page = flags.get("page").map(|p| p.parse::<usize>()).transpose().map_err(|_| anyhow::anyhow!("--page takes a number"))?.unwrap_or(1);
            let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("Heimdall"));
            review(&data, &command_prefix(&exe, &data), from, to, page)
        }
        "add" => {
            known(&["by"])?;
            let by = flags.get("by").map_or("review", String::as_str);
            ensure!(by == "review" || by == "seed", "--by: review or seed (the agent writes with taste_note)");
            let mut input = String::new();
            stdin.read_to_string(&mut input)?;
            let archive = archive_path(&data);
            let times = LineTimes::new(&archive);
            let line_time = |id: &str| times.get(id);
            let (mut added, mut repeated, mut failed) = (vec![], 0, vec![]);
            for (i, raw) in input.lines().enumerate().filter(|(_, l)| !l.trim().is_empty()) {
                let result = serde_json::from_str::<Value>(raw)
                    .map_err(anyhow::Error::from)
                    .and_then(|v| Entry::from_args(&v, by, crate::now_ms() as i64, line_time))
                    .and_then(|e| {
                        let unknown: Vec<&String> = e.lines.iter().filter(|l| archive.is_file() && line_time(l).is_none()).collect();
                        ensure!(unknown.is_empty(), "lines not in the archive: {unknown:?}; use the session header's name, a colon and the line number");
                        append(&ledger, &e)
                    });
                match result {
                    Ok(Added::New(id)) => added.push(id),
                    Ok(Added::Duplicate(_)) => repeated += 1,
                    Err(e) => failed.push(format!("line {}: {e:#}", i + 1)),
                }
            }
            let mut out = format!("Added {}, skipped {repeated} already in the ledger.", added.len());
            for f in &failed {
                out.push_str(&format!("\nNot added, {f}"));
            }
            ensure!(failed.is_empty() || !added.is_empty() || repeated > 0, "{out}");
            Ok(out)
        }
        other => bail!("unknown command {other}\n{USAGE}"),
    }
}

fn data_is_default(data: &Path) -> bool {
    data == heimdall_home::data_dir()
}

/// `'…/Heimdall' --taste`, with `--data-dir '…'` for a profile other than the default: how a task calls the ledger
/// commands (`--taste` must come first).
pub fn command_prefix(exe: &Path, data: &Path) -> String {
    let quote = |p: &Path| format!("'{}'", p.display().to_string().replace('\'', "'\\''"));
    let exe = format!("{} --taste", quote(exe));
    if data_is_default(data) { exe } else { format!("{exe} --data-dir {}", quote(data)) }
}

/// The tokens in `prompt/heimdall.md` that become the ledger's path and the search and review commands at launch.
pub const PROMPT_LEDGER: &str = "HEIMDALL_TASTE_LEDGER";
pub const PROMPT_SEARCH: &str = "HEIMDALL_TASTE_SEARCH";
pub const PROMPT_REVIEW: &str = "HEIMDALL_TASTE_REVIEW";

/// The prompt with this profile's ledger path and this binary's review command in place of the tokens.
pub fn fill_prompt(prompt: &str, data: &Path, exe: &Path) -> String {
    prompt
        .replace(PROMPT_REVIEW, &format!("{} review", command_prefix(exe, data)))
        .replace(PROMPT_SEARCH, &format!("{} search", command_prefix(exe, data)))
        .replace(PROMPT_LEDGER, &path(data).display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn args(words: &str) -> Value {
        json!({"subject":"Heimdall's app icon","kind":"icon","verdict":"dislike","words":words,"why":"marks must look crafted, not generated"})
    }
    fn entry(words: &str, at_ms: i64) -> Entry {
        let mut a = args(words);
        a["at"] = json!(rfc3339(at_ms));
        Entry::from_args(&a, "agent", at_ms, |_| None).unwrap()
    }

    #[test]
    fn an_entry_keeps_his_words_its_references_and_who_wrote_it() {
        let mut a = args("  really terrible ");
        a["lines"] = json!(["session-1-a:12"]);
        a["files"] = json!(["/tmp/icon.png"]);
        a["screen_at"] = json!("2026-10-23T21:04:00+02:00");
        let e = Entry::from_args(&a, "agent", 1_000, |id| (id == "session-1-a:12").then_some(1_792_700_000_000)).unwrap();
        assert_eq!(e.words, "really terrible");
        assert_eq!(e.at_ms(), 1_792_700_000_000, "the time comes from the line he said it in");
        assert_eq!((e.by.as_str(), e.schema), ("agent", SCHEMA));
        assert!(e.id.starts_with("taste-1000-"));
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["files"], json!(["/tmp/icon.png"]));
        assert!(v.get("follows").is_none());
        let mut later = args("now it is too plain");
        later["follows"] = json!(e.id);
        assert_eq!(Entry::from_args(&later, "agent", 2_000, |_| None).unwrap().follows.as_deref(), Some(e.id.as_str()));
        later["follows"] = json!("not-an-id");
        assert!(Entry::from_args(&later, "agent", 2_000, |_| None).is_err());
        assert_eq!(serde_json::from_value::<Entry>(v).unwrap(), e);
        // No optional field: none is written.
        let bare = serde_json::to_value(entry("bad", 5)).unwrap();
        assert!(bare.get("lines").is_none() && bare.get("files").is_none() && bare.get("screen_at").is_none());
    }

    #[test]
    fn timeline_ids_become_line_ids() {
        assert_eq!(line_id("49:text:utterance:session-17-ca0c:165"), "session-17-ca0c:165");
        assert_eq!(line_id("49:text:turn:session-17-ca0c:165"), "session-17-ca0c:165");
        assert_eq!(line_id("session-17-ca0c:165"), "session-17-ca0c:165");
        assert_eq!(line_id("screen:1:2:3"), "screen:1:2:3");
        let mut a = args("w");
        a["lines"] = json!(["49:text:utterance:session-17-ca0c:165"]);
        assert_eq!(Entry::from_args(&a, "agent", 1, |_| None).unwrap().lines, ["session-17-ca0c:165"]);
        a["lines"] = json!("session-17-ca0c:166");
        assert_eq!(Entry::from_args(&a, "review", 1, |_| None).unwrap().lines, ["session-17-ca0c:166"], "one id as text");
    }

    #[test]
    fn bad_entries_are_refused() {
        let now = 1;
        for (key, value) in [("verdict", json!("meh")), ("kind", json!("vibe")), ("words", json!("  ")), ("why", json!("x".repeat(WHY_CHARS + 1))), ("lines", json!([""])), ("at", json!("yesterday")), ("extra", json!(1))] {
            let mut a = args("bad");
            a[key] = value;
            assert!(Entry::from_args(&a, "agent", now, |_| None).is_err(), "{key}");
        }
        assert!(Entry::from_args(&args("bad"), "someone", now, |_| None).is_err());
        assert!(Entry::from_args(&json!("text"), "agent", now, |_| None).is_err());
    }

    #[test]
    fn appends_skip_repeats_and_survive_a_torn_line() {
        let d = tempfile::tempdir().unwrap();
        let ledger = path(d.path());
        let h = 3600_000;
        let first = entry("The icon is really terrible, honestly", 10 * h);
        assert!(matches!(append(&ledger, &first).unwrap(), Added::New(_)));
        // Same words, other punctuation and case, close in time: a repeat. The same stock words a day later: new.
        assert_eq!(append(&ledger, &entry("the icon is REALLY terrible honestly!", 12 * h)).unwrap(), Added::Duplicate(first.id.clone()));
        assert!(matches!(append(&ledger, &entry("the icon is REALLY terrible honestly!", 40 * h)).unwrap(), Added::New(_)));
        // Words inside the other's, close in time: a repeat.
        assert!(matches!(append(&ledger, &entry("icon is really terrible", 11 * h)).unwrap(), Added::Duplicate(_)));
        // Containment is by whole words: "honest" inside "honestly" makes no repeat.
        assert!(matches!(append(&ledger, &entry("icon is really terrible, honest", 10 * h)).unwrap(), Added::New(_)));
        // A short phrase inside another is no repeat.
        assert!(matches!(append(&ledger, &entry("terrible", 10 * h)).unwrap(), Added::New(_)));
        // Two verdicts in one line: different words, both kept; the same words from that line days later (a review
        // over a range the agent covered): a repeat.
        let mut a = args("the icon is great");
        a["lines"] = json!(["s:1", "s:0"]);
        let cited = Entry::from_args(&a, "agent", 50 * h, |_| None).unwrap();
        assert!(matches!(append(&ledger, &cited).unwrap(), Added::New(_)));
        a["words"] = json!("but the font is ugly");
        assert!(matches!(append(&ledger, &Entry::from_args(&a, "agent", 50 * h, |_| None).unwrap()).unwrap(), Added::New(_)));
        a["lines"] = json!(["s:1"]);
        assert!(matches!(append(&ledger, &Entry::from_args(&a, "review", 200 * h, |_| None).unwrap()).unwrap(), Added::Duplicate(_)));
        // The same stock words in two different lines, close in time: two judgements.
        let mut b = args("this is really well made indeed");
        b["lines"] = json!(["s:10"]);
        assert!(matches!(append(&ledger, &Entry::from_args(&b, "agent", 70 * h, |_| None).unwrap()).unwrap(), Added::New(_)));
        b["lines"] = json!(["s:11"]);
        assert!(matches!(append(&ledger, &Entry::from_args(&b, "agent", 70 * h, |_| None).unwrap()).unwrap(), Added::New(_)));
        // Words that are only punctuation never match each other.
        assert!(matches!(append(&ledger, &entry("!!!", 80 * h)).unwrap(), Added::New(_)));
        assert!(matches!(append(&ledger, &entry("???", 80 * h)).unwrap(), Added::New(_)));
        // A context line two entries share makes no repeat.
        a["lines"] = json!(["s:2", "s:0"]);
        a["words"] = json!("the colours are lovely");
        assert!(matches!(append(&ledger, &Entry::from_args(&a, "agent", 51 * h, |_| None).unwrap()).unwrap(), Added::New(_)));
        // The same words about a redo (`follows`): a new judgement; `follows` must name an entry.
        let mut redo = args("The icon is really terrible, honestly");
        redo["follows"] = json!(first.id);
        assert!(matches!(append(&ledger, &Entry::from_args(&redo, "agent", 11 * h, |_| None).unwrap()).unwrap(), Added::New(_)));
        // The same verdict added again without `follows` (a review that missed the link): a repeat.
        redo.as_object_mut().unwrap().remove("follows");
        assert!(matches!(append(&ledger, &Entry::from_args(&redo, "review", 11 * h, |_| None).unwrap()).unwrap(), Added::Duplicate(_)));
        redo["follows"] = json!("taste-0-nothere");
        redo["words"] = json!("something else entirely");
        assert!(append(&ledger, &Entry::from_args(&redo, "agent", 11 * h, |_| None).unwrap()).unwrap_err().to_string().contains("no entry taste-0-nothere"));
        // A crash mid-write leaves a torn line: the next entry starts on its own line and both reads stay whole.
        std::fs::OpenOptions::new().append(true).open(&ledger).unwrap().write_all(b"{\"schema\":1,\"id\":\"torn").unwrap();
        assert!(matches!(append(&ledger, &entry("too many headings, hard to scan", 60 * h)).unwrap(), Added::New(_)));
        let all = load(&ledger).unwrap();
        assert_eq!(all.len(), 13);
        assert_eq!(all.last().unwrap().words, "too many headings, hard to scan");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&ledger).unwrap().permissions().mode() & 0o777, 0o600);
    }

    /// Night review 24 Oct, finding 3: "the layout is great but the colours are too dark" holds two verdicts. The like
    /// quotes part of the line, the dislike the whole line (as the review guide asks): the words contain each other
    /// and both cite that line, yet they are two judgements. A repeat of either is still a repeat, whatever kind or
    /// subject the second writer gave it.
    #[test]
    fn two_verdicts_in_one_line_are_never_merged() {
        let d = tempfile::tempdir().unwrap();
        let ledger = path(d.path());
        let line = |verdict: &str, words: &str, kind: &str, subject: &str| {
            let a = json!({"subject":subject,"kind":kind,"verdict":verdict,"words":words,"why":"w","lines":["s:7"]});
            Entry::from_args(&a, "agent", 3600_000, |_| None).unwrap()
        };
        let like = line("like", "the layout is great", "page", "the report's layout");
        assert!(matches!(append(&ledger, &like).unwrap(), Added::New(_)));
        let whole = "the layout is great but the colours are too dark";
        assert!(matches!(append(&ledger, &line("dislike", whole, "page", "the report's colours")).unwrap(), Added::New(_)), "the dislike is kept");
        assert!(matches!(append(&ledger, &line("like", whole, "page", "the report")).unwrap(), Added::Duplicate(_)), "the like again, quoting the whole line");
        assert!(matches!(append(&ledger, &line("dislike", whole, "report", "colours")).unwrap(), Added::Duplicate(_)), "a review's other kind and subject");
        assert_eq!(load(&ledger).unwrap().len(), 2);
    }

    #[test]
    fn concurrent_writers_never_lose_or_double_an_entry() {
        let d = tempfile::tempdir().unwrap();
        let ledger = path(d.path());
        let threads: Vec<_> = (0..8)
            .map(|i| {
                let ledger = ledger.clone();
                std::thread::spawn(move || {
                    for j in 0..10 {
                        // Thread i and i+4 write the same ten judgements.
                        append(&ledger, &entry(&format!("judgement number {} of {j} words", i % 4), 0)).unwrap();
                    }
                })
            })
            .collect();
        threads.into_iter().for_each(|t| t.join().unwrap());
        assert_eq!(load(&ledger).unwrap().len(), 40);
    }

    #[test]
    fn search_finds_by_topic_and_verdict_best_first() {
        let h = 3600_000;
        let mut report = args("too many headings, hard to scan");
        report["subject"] = json!("research reports");
        report["kind"] = json!("report");
        report["why"] = json!("reports must read at a glance");
        let mut praise = report.clone();
        praise["subject"] = json!("benchmark report");
        praise["verdict"] = json!("like");
        praise["words"] = json!("really well made");
        praise["why"] = json!("clean layout with graphics over text");
        let all = vec![
            Entry::from_args(&report, "seed", 2 * h, |_| None).unwrap(),
            Entry::from_args(&praise, "seed", 3 * h, |_| None).unwrap(),
            entry("really terrible", h),
        ];
        let hits = search(&all, Some("report headings"), None, None, 10);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].words, "too many headings, hard to scan", "two words matched beat one");
        assert_eq!(search(&all, Some("reports"), Some("like"), None, 10).len(), 1);
        assert_eq!(search(&all, None, None, Some(2 * h), 10).len(), 2);
        assert_eq!(search(&all, Some("icon"), None, None, 10)[0].kind, "icon");
        assert!(search(&all, Some("voices"), None, None, 10).is_empty());
        let d = tempfile::tempdir().unwrap();
        assert_eq!(search_answer(&path(d.path()), Some("icon"), None, None, 5).unwrap(), "No judgements match (0 in the ledger).");
    }

    fn archive_with_lines(data: &Path) {
        let db = archive_path(data);
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        let c = Connection::open(&db).unwrap();
        c.execute_batch("CREATE TABLE sessions(id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE);
            INSERT INTO sessions VALUES(1,'session-7-ab');
            CREATE TABLE utterances(session INTEGER NOT NULL, id TEXT NOT NULL, wall_ms INTEGER, source_id TEXT, source TEXT, speaker TEXT, start_ns INTEGER, end_ns INTEGER, text TEXT, words TEXT, notes TEXT, decision TEXT, PRIMARY KEY(session, id));
            CREATE INDEX utterance_time ON utterances(wall_ms);").unwrap();
        let t0 = parse_time("2026-10-23 21:00").unwrap();
        for (i, (src, who, text)) in [
            ("process:1:2:3", "Heimdall", "Here is the new icon."),
            ("mic:Shure:1", "User", "That icon is really terrible."),
            ("discord:9", "Morgan", "lol it is"),
            ("mic:Shure:1", "User", "Make it again, simpler."),
        ]
        .into_iter()
        .enumerate()
        {
            c.execute("INSERT INTO utterances(session,id,wall_ms,source_id,speaker,text) VALUES(1,?1,?2,?3,?4,?5)", params![format!("session-7-ab:{}", i + 1), t0 + i as i64 * 1000, src, who, text]).unwrap();
        }
    }

    #[test]
    fn a_review_lists_the_users_and_heimdalls_lines_and_what_is_known() {
        let d = tempfile::tempdir().unwrap();
        archive_with_lines(d.path());
        let mut stdin: &[u8] = b"{\"subject\":\"Heimdall's app icon\",\"kind\":\"icon\",\"verdict\":\"dislike\",\"words\":\"That icon is really terrible.\",\"why\":\"marks must look crafted\",\"lines\":[\"session-7-ab:2\"]}\n\n{\"subject\":\"x\",\"kind\":\"icon\",\"verdict\":\"dislike\",\"words\":\"w\",\"why\":\"y\",\"lines\":[\"session-7-ab:99\"]}\nnot json\n";
        let data = d.path().to_path_buf();
        let out = cli(&["--data-dir".into(), data.display().to_string(), "add".into()], || unreachable!(), &mut stdin).unwrap();
        assert!(out.starts_with("Added 1, skipped 0"), "{out}");
        assert!(out.contains("line 3: lines not in the archive") && out.contains("line 4:"), "{out}");
        let e = &load(&path(&data)).unwrap()[0];
        assert_eq!((e.by.as_str(), e.at_ms()), ("review", parse_time("2026-10-23 21:00").unwrap() + 1000));
        // Adding it again: skipped.
        let mut again: &[u8] = b"{\"subject\":\"icon\",\"kind\":\"icon\",\"verdict\":\"dislike\",\"words\":\"that icon is REALLY terrible\",\"why\":\"y\",\"lines\":[\"session-7-ab:2\"]}";
        assert_eq!(cli(&["--data-dir".into(), data.display().to_string(), "add".into()], || unreachable!(), &mut again).unwrap(), "Added 0, skipped 1 already in the ledger.");

        let page = cli(&["review".into(), "--from".into(), "2026-10-23".into(), "--to".into(), "2026-10-23".into()], || Ok(data.clone()), &mut &b""[..]).unwrap();
        assert!(page.contains("A judgement is the user's own verdict"));
        assert!(page.contains(&format!("--taste --data-dir '{}' add --by review", data.display())), "the add command names this data folder: {page}");
        assert!(page.contains("Already in the ledger:\n") && page.contains("dislike · icon · Heimdall's app icon"));
        assert!(page.contains("] line session-7-ab:2\n"), "known entries show the line they cite");
        assert!(page.contains("\nadd --by review <<'EOF'\n") || page.contains(" add --by review <<'EOF'\n{"), "the worker adds each page through a heredoc");
        assert!(page.contains("\nEOF\n"), "the heredoc's end stands alone at the start of its line");
        assert!(page.contains("Transcript: 3 lines of the user and Heimdall, on pages 2 to 2.") && !page.contains("## session-7-ab"));
        assert!(page.contains("[page 1 of 2; next: the same command with --page 2]"));
        let page = review(&data, "H", parse_time("2026-10-23").unwrap(), parse_time("2026-10-24").unwrap(), 2).unwrap();
        assert!(page.starts_with("## session-7-ab (Fri 23 Oct 2026)\n"), "{page}");
        assert!(page.contains("21:00:00 Heimdall :1 Here is the new icon.") && page.contains("21:00:01 User :2 That icon is really terrible."));
        assert!(!page.contains("Morgan"), "other people's lines are left out");
        assert!(page.contains("[page 2 of 2; end of the transcript]"));
        // The day before: nothing known, no lines.
        let empty = review(&data, "H", parse_time("2026-10-22").unwrap(), parse_time("2026-10-23").unwrap(), 1).unwrap();
        assert!(empty.contains("(none in this range)") && empty.contains("(no transcript lines in this range)"));
        assert!(empty.contains("[page 1 of 1; end of the transcript]"));
        assert!(review(&data, "H", 0, 1, 2).is_err() && review(&data, "H", 0, 1, 0).is_err());
    }

    #[test]
    fn the_prompt_names_this_profiles_ledger_and_review_command() {
        let text = fill_prompt("read `HEIMDALL_TASTE_LEDGER`; run `HEIMDALL_TASTE_REVIEW --from 2026-10-17`; `HEIMDALL_TASTE_SEARCH icon`", Path::new("/tmp/lab data"), Path::new("/Apps/Heimdall.app/Contents/MacOS/Heimdall"));
        assert_eq!(text, "read `/tmp/lab data/taste/ledger.jsonl`; run `'/Apps/Heimdall.app/Contents/MacOS/Heimdall' --taste --data-dir '/tmp/lab data' review --from 2026-10-17`; `'/Apps/Heimdall.app/Contents/MacOS/Heimdall' --taste --data-dir '/tmp/lab data' search icon`");
        assert_eq!(command_prefix(Path::new("/A/User's/Heimdall"), Path::new("/d")), "'/A/User'\\''s/Heimdall' --taste --data-dir '/d'");
        assert_eq!(command_prefix(Path::new("/A/Heimdall"), &heimdall_home::data_dir()), "'/A/Heimdall' --taste");
    }

    #[test]
    fn every_page_names_its_session_and_page_one_fits_with_the_guide() {
        let d = tempfile::tempdir().unwrap();
        archive_with_lines(d.path());
        let c = Connection::open(archive_path(d.path())).unwrap();
        c.execute("INSERT INTO sessions VALUES(2,'session-8-cd')", []).unwrap();
        let t0 = parse_time("2026-10-23 22:00").unwrap();
        c.execute_batch("BEGIN").unwrap();
        for i in 0..2000 {
            // A revised line keeps its parent's id before a suffix (`:159:speech:1`).
            c.execute("INSERT INTO utterances(session,id,wall_ms,source_id,speaker,text) VALUES(2,?1,?2,'mic:x','User',?3)", params![format!("session-8-cd:{i}:speech:1"), t0 + i * 1000, "word ".repeat(20)]).unwrap();
        }
        c.execute_batch("COMMIT").unwrap();
        let from = parse_time("2026-10-23").unwrap();
        let to = parse_time("2026-10-24").unwrap();
        let first = review(d.path(), "H", from, to, 1).unwrap();
        let total: usize = first.split("[page 1 of ").nth(1).unwrap().split(';').next().unwrap().parse().unwrap();
        assert!(total > 4);
        let third = review(d.path(), "H", from, to, 3).unwrap();
        let mut rows = 0;
        for page in 2..=total {
            let text = review(d.path(), "H", from, to, page).unwrap();
            assert!(text.starts_with("## session-"), "page {page} starts with its session: {}", &text[..80]);
            assert!(text.len() <= PAGE_CHARS + 200, "page {page}: {}", text.len());
            assert!(!text.contains(")\n\n## ") && !text.contains(")\n\n["), "page {page}: no header without lines under it");
            rows += text.lines().filter(|l| l.contains(" User :")).count();
            if page > 2 {
                assert!(text.starts_with("## session-8-cd (Fri 23 Oct 2026, continued)\n"));
                assert!(text.contains(":speech:1 word"), "the line number keeps its suffix");
            }
        }
        assert_eq!(rows, 2002, "every line of the user once, across the pages");
        assert!(review(d.path(), "H", from, to, total + 1).is_err());
        // Entries the worker adds between pages never move a page boundary.
        for i in 0..150 {
            append(&path(d.path()), &entry(&format!("a judgement worth keeping, number {i}"), from + i * 1000)).unwrap();
        }
        assert_eq!(review(d.path(), "H", from, to, 3).unwrap(), third);
        let first = review(d.path(), "H", from, to, 1).unwrap();
        assert!(first.len() <= KNOWN_CHARS + REVIEW_GUIDE.len() + 1000, "page 1 stays one page: {}", first.len());
        assert!(first.contains("number 149\"") && !first.contains("number 0\"") && first.contains("older ones; add skips any repeat"));
    }

    #[test]
    fn times_take_rfc3339_and_local_dates() {
        assert_eq!(parse_time("2026-10-24T01:16:00+02:00").unwrap(), parse_time("2026-10-23T23:16:00Z").unwrap());
        assert_eq!(parse_time("2026-10-23T23:16:00Z").unwrap() % 86_400_000, (23 * 60 + 16) * 60_000);
        assert_eq!(parse_end("2026-10-24").unwrap() - parse_time("2026-10-24").unwrap(), 24 * 3600_000);
        assert_eq!(parse_time("2026-10-24 01:16").unwrap(), parse_time("2026-10-24T01:16").unwrap());
        assert!(parse_time("Saturday").is_err());
        // Search ignores filler words in a topic.
        let all = vec![entry("the font is ugly", 0)];
        assert!(search(&all, Some("about the voice"), None, None, 5).is_empty());
    }
}
