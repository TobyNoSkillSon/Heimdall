//! Crewmates: permanent workers under Heimdall (17:09–17:13: "a persistent worker inside of Heimdall …
//! whenever the T3 Code thread is getting killed … he just wakes up and does stuff. Nothing is ever lost because it's
//! actually with you"; "it's essentially a permanent sub-agent that is always under you").
//!
//! A crewmate is a record Heimdall owns in `<data>/agent/crew.sqlite`: name, role, project folder, model and effort,
//! standing-brief file, schedules, an inbox and a history of every message in and every result out. Its body is a T3
//! thread, one of Heimdall's tasks (`workers`), that never closes itself. The body can be lost (archived, deleted, a
//! crash, a usage limit) and is reopened, resumed or replaced from the record; the record is never lost.
//!
//! Rules this module holds:
//! - Everything reaches a crewmate through here (the user, 17:13: "I want all of the tasks … to go through you and then to
//!   them"): the agent's `crew_tell`, a project tool's `heimdall --crew tell`, the crewmate's own schedules, and
//!   Heimdall's resume and wake messages. `Workers::crew_guard` refuses every other send, stop or archive of a crew
//!   thread. Each delivery is in the inbox before it is tried, so a failed one is tried again on the next pass.
//! - Every result is recorded (`on_result`). A result of a message the agent sent reaches the agent as any task's
//!   does; a routine one (a schedule, a project tool) reaches it only in the digest (`crew_status`), unless it failed
//!   or asks something of the user.
//! - A turn that failed or was interrupted is resumed once by itself, with one notice; a second failure is reported.
//! - A crewmate that waited on its own sub-agents or background commands is woken when they have all ended and no
//!   turn of it started meanwhile (`WAKE_GRACE_MS`).
use crate::workers::Workers;
use agent::delivery::DeliveryPermit;
use anyhow::{bail, ensure, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};

/// Inbox sources whose results reach the agent only in the digest.
pub const QUIET_SOURCES: [&str; 2] = ["local", "schedule"];
/// One history entry keeps at most this much text.
pub const HISTORY_TEXT: usize = 32 * 1024;
/// A message into a crewmate is at most this long.
pub const MESSAGE_BYTES: usize = 64 * 1024;
/// What a new thread is given of the history: the last result and the latest entries, within this many bytes.
pub const SUMMARY_BYTES: usize = 8 * 1024;
/// How often each crewmate's thread is read to see it still exists and which account it runs on.
pub const BODY_CHECK_MS: u64 = 10 * 60 * 1000;
/// After the work a crewmate waited on ended, how long T3's own wake turn is given before Heimdall wakes it.
pub const WAKE_GRACE_MS: u64 = 2 * 60 * 1000;
/// A message that failed to reach its crewmate this many times is reported once; it stays in the inbox and is tried
/// again, backing off up to `RETRY_MAX_MS` between tries. Nothing is ever given up.
pub const MAX_ATTEMPTS: i64 = 20;
pub const RETRY_MAX_MS: u64 = 10 * 60 * 1000;
/// The shortest repeat of an `every` schedule.
pub const MIN_EVERY_MIN: u64 = 15;
pub const TITLE_PREFIX: &str = "Crew · ";

/// One crewmate's record.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Mate {
    pub name: String,
    pub role: String,
    pub folder: Option<String>,
    pub model: String,
    pub effort: String,
    pub provider: Option<String>,
    pub brief: Option<String>,
    pub thread: Option<String>,
    /// The thread is one of Heimdall's tasks (`Workers::adopt` or a launch); false: the next pass adopts it.
    pub adopted: bool,
    pub paused: bool,
    pub retired_ms: Option<i64>,
    pub last_state: Option<String>,
    pub last_result: Option<String>,
    pub last_ms: Option<i64>,
    /// A thread that was lost, named when its replacement starts.
    pub lost: Option<String>,
}

/// The crew's store. The local command (`cli`) opens only this, never the T3 client.
pub struct CrewStore {
    db: Mutex<Connection>,
}
const MATE_COLUMNS: &str = "name,role,folder,model,effort,provider,brief,thread,adopted,paused,retired_ms,last_state,last_result,last_ms,lost";
fn mate(r: &rusqlite::Row) -> rusqlite::Result<Mate> {
    Ok(Mate {
        name: r.get(0)?,
        role: r.get(1)?,
        folder: r.get(2)?,
        model: r.get(3)?,
        effort: r.get(4)?,
        provider: r.get(5)?,
        brief: r.get(6)?,
        thread: r.get(7)?,
        adopted: r.get::<_, i64>(8)? != 0,
        paused: r.get::<_, i64>(9)? != 0,
        retired_ms: r.get(10)?,
        last_state: r.get(11)?,
        last_result: r.get(12)?,
        last_ms: r.get(13)?,
        lost: r.get(14)?,
    })
}
fn cut(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}
fn one_line(text: &str, max: usize) -> String {
    cut(&text.split_whitespace().collect::<Vec<_>>().join(" "), max)
}
fn now() -> i64 {
    crate::now_ms() as i64
}
impl CrewStore {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p)?;
        }
        let db = Connection::open(path)?;
        db.busy_timeout(std::time::Duration::from_secs(5))?;
        db.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
             CREATE TABLE IF NOT EXISTS crew(name TEXT PRIMARY KEY COLLATE NOCASE,role TEXT NOT NULL,folder TEXT,model TEXT NOT NULL,effort TEXT NOT NULL,
               provider TEXT,brief TEXT,thread TEXT,adopted INTEGER NOT NULL DEFAULT 0,paused INTEGER NOT NULL DEFAULT 0,retired_ms INTEGER,
               last_state TEXT,last_result TEXT,last_ms INTEGER,lost TEXT,resume_run TEXT,resume_state TEXT,waiting TEXT,waiting_run TEXT,
               waiting_clear_ms INTEGER,checked_ms INTEGER,created_ms INTEGER NOT NULL,imported INTEGER NOT NULL DEFAULT 0);
             CREATE TABLE IF NOT EXISTS inbox(id INTEGER PRIMARY KEY AUTOINCREMENT,name TEXT NOT NULL COLLATE NOCASE,op TEXT UNIQUE NOT NULL,source TEXT NOT NULL,
               text TEXT NOT NULL,ms INTEGER NOT NULL,state TEXT NOT NULL DEFAULT 'pending',how TEXT,delivered_ms INTEGER,attempts INTEGER NOT NULL DEFAULT 0,
               error TEXT,retry INTEGER NOT NULL DEFAULT 0,composed TEXT,composed_ms INTEGER,next_try_ms INTEGER NOT NULL DEFAULT 0,warned INTEGER NOT NULL DEFAULT 0);
             CREATE TABLE IF NOT EXISTS history(id INTEGER PRIMARY KEY AUTOINCREMENT,name TEXT NOT NULL COLLATE NOCASE,ms INTEGER NOT NULL,kind TEXT NOT NULL,
               source TEXT,run TEXT,text TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS runs(thread TEXT NOT NULL,run TEXT NOT NULL,source TEXT NOT NULL,retry INTEGER NOT NULL DEFAULT 0,PRIMARY KEY(thread,run));
             CREATE TABLE IF NOT EXISTS schedules(id INTEGER PRIMARY KEY AUTOINCREMENT,name TEXT NOT NULL COLLATE NOCASE,every TEXT NOT NULL,message TEXT NOT NULL,
               next_ms INTEGER NOT NULL,last_ms INTEGER,created_ms INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY,value TEXT NOT NULL);
             CREATE INDEX IF NOT EXISTS history_name ON history(name,ms);
             CREATE INDEX IF NOT EXISTS inbox_pending ON inbox(state,id);",
        )?;
        Ok(Self { db: Mutex::new(db) })
    }
    pub fn get(&self, name: &str) -> Result<Option<Mate>> {
        Ok(self.db.lock().unwrap().query_row(&format!("SELECT {MATE_COLUMNS} FROM crew WHERE name=?"), [name.trim()], mate).optional()?)
    }
    fn need(&self, name: &str) -> Result<Mate> {
        self.get(name)?.with_context(|| {
            let names = self.all(false).map(|m| m.iter().map(|m| m.name.clone()).collect::<Vec<_>>().join(", ")).unwrap_or_default();
            format!("NO_CREWMATE: no crewmate is called '{name}'. The crew: {}", if names.is_empty() { "none yet".into() } else { names })
        })
    }
    pub fn all(&self, retired: bool) -> Result<Vec<Mate>> {
        let sql = format!("SELECT {MATE_COLUMNS} FROM crew {} ORDER BY created_ms", if retired { "" } else { "WHERE retired_ms IS NULL" });
        Ok(self.db.lock().unwrap().prepare(&sql)?.query_map([], mate)?.collect::<rusqlite::Result<_>>()?)
    }
    /// The crewmate whose body `thread` is (retired ones included: their old thread stays theirs).
    pub fn crewmate_of(&self, thread: &str) -> Result<Option<String>> {
        Ok(self.db.lock().unwrap().query_row("SELECT name FROM crew WHERE thread=? ORDER BY retired_ms IS NOT NULL LIMIT 1", [thread], |r| r.get(0)).optional()?)
    }
    /// A new crewmate, or a retired one re-added with its history.
    pub fn add(&self, m: &Mate) -> Result<bool> {
        validate_name(&m.name)?;
        ensure!(!m.role.trim().is_empty(), "role: one line saying what the crewmate does");
        let db = self.db.lock().unwrap();
        let existing: Option<Option<i64>> = db.query_row("SELECT retired_ms FROM crew WHERE name=?", [&m.name], |r| r.get(0)).optional()?;
        if let Some(thread) = &m.thread {
            let other: Option<String> = db.query_row("SELECT name FROM crew WHERE thread=? AND name!=? AND retired_ms IS NULL", params![thread, m.name], |r| r.get(0)).optional()?;
            ensure!(other.is_none(), "CREW_THREAD: {thread} is already {}'s thread", other.unwrap_or_default());
        }
        match existing {
            Some(None) => bail!("CREW_EXISTS: {} is already on the crew; change it with crew_change", m.name),
            Some(Some(_)) => {
                db.execute(
                    "UPDATE crew SET role=?2,folder=?3,model=?4,effort=?5,brief=?6,thread=COALESCE(?7,thread),adopted=CASE WHEN ?7 IS NULL THEN adopted ELSE 0 END,paused=0,retired_ms=NULL WHERE name=?1",
                    params![m.name, m.role, m.folder, m.model, m.effort, m.brief, m.thread],
                )?;
                Ok(false)
            }
            None => {
                db.execute(
                    "INSERT INTO crew(name,role,folder,model,effort,provider,brief,thread,adopted,created_ms) VALUES(?,?,?,?,?,?,?,?,?,?)",
                    params![m.name.trim(), m.role, m.folder, m.model, m.effort, m.provider, m.brief, m.thread, m.adopted as i64, now()],
                )?;
                Ok(true)
            }
        }
    }
    /// Puts a message in a crewmate's inbox under `op` (a replay of the same `op` is the same message). Returns its id.
    /// Only for an existing crewmate that is not retired.
    pub fn queue(&self, name: &str, text: &str, source: &str, op: &str, retry: bool) -> Result<i64> {
        let m = self.need(name)?;
        ensure!(m.retired_ms.is_none(), "CREW_RETIRED: {} is retired; add it again with crew_add to give it work", m.name);
        let text = text.trim();
        ensure!(!text.is_empty(), "message: empty");
        ensure!(text.len() <= MESSAGE_BYTES, "message: {} KB, at most {} KB", text.len() / 1024, MESSAGE_BYTES / 1024);
        let db = self.db.lock().unwrap();
        db.execute("INSERT OR IGNORE INTO inbox(name,op,source,text,ms,retry) VALUES(?,?,?,?,?,?)", params![m.name, op, source, text, now(), retry as i64])?;
        Ok(db.query_row("SELECT id FROM inbox WHERE op=?", [op], |r| r.get(0))?)
    }
    pub fn record(&self, name: &str, kind: &str, source: Option<&str>, run: Option<&str>, text: &str) -> Result<()> {
        self.db.lock().unwrap().execute(
            "INSERT INTO history(name,ms,kind,source,run,text) VALUES(?,?,?,?,?,?)",
            params![name, now(), kind, source, run, cut(text, HISTORY_TEXT)],
        )?;
        Ok(())
    }
    fn set(&self, name: &str, column: &str, value: rusqlite::types::Value) -> Result<()> {
        self.db.lock().unwrap().execute(&format!("UPDATE crew SET {column}=? WHERE name=?"), params![value, name])?;
        Ok(())
    }
    fn text_col(&self, name: &str, column: &str) -> Result<Option<String>> {
        Ok(self.db.lock().unwrap().query_row(&format!("SELECT {column} FROM crew WHERE name=?"), [name], |r| r.get(0)).optional()?.flatten())
    }
    fn int_col(&self, name: &str, column: &str) -> Result<Option<i64>> {
        Ok(self.db.lock().unwrap().query_row(&format!("SELECT {column} FROM crew WHERE name=?"), [name], |r| r.get(0)).optional()?.flatten())
    }
    /// The source a thread's run answers to: the message that started it, else the thread's latest known one.
    fn run_source(&self, thread: &str, run: &str) -> Result<(String, bool)> {
        let db = self.db.lock().unwrap();
        let exact: Option<(String, i64)> = db.query_row("SELECT source,retry FROM runs WHERE thread=? AND run=?", params![thread, run], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
        if let Some((s, retry)) = exact {
            return Ok((s, retry != 0));
        }
        let latest: Option<String> = db.query_row("SELECT source FROM runs WHERE thread=? ORDER BY rowid DESC LIMIT 1", [thread], |r| r.get(0)).optional()?;
        Ok((latest.unwrap_or_else(|| "agent".into()), false))
    }
    fn set_run_source(&self, thread: &str, run: &str, source: &str, retry: bool, fresh: bool) -> Result<()> {
        let db = self.db.lock().unwrap();
        if fresh {
            db.execute("INSERT OR REPLACE INTO runs VALUES(?,?,?,?)", params![thread, run, source, retry as i64])?;
        } else if retry {
            // A resume that joined a turn: that turn is the retry, so its failure is reported, not resumed again.
            db.execute("INSERT INTO runs VALUES(?1,?2,?3,1) ON CONFLICT(thread,run) DO UPDATE SET retry=1", params![thread, run, source])?;
        }
        if !fresh && source == "agent" {
            // The agent's message joined a routine turn: its result is the agent's to hear.
            db.execute("INSERT INTO runs VALUES(?1,?2,'agent',0) ON CONFLICT(thread,run) DO UPDATE SET source='agent'", params![thread, run])?;
        }
        Ok(())
    }
    pub fn pending(&self) -> Result<Vec<(i64, String, String)>> {
        Ok(self.db.lock().unwrap().prepare("SELECT id,name,source FROM inbox WHERE state='pending' AND next_try_ms<=? ORDER BY id")?.query_map([now()], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<_>>()?)
    }
    pub fn history_lines(&self, name: &str, since_ms: i64, limit: usize) -> Result<Vec<String>> {
        let rows: Vec<(i64, String, Option<String>, String)> = self
            .db
            .lock()
            .unwrap()
            .prepare("SELECT ms,kind,source,text FROM history WHERE name=? AND ms>=? ORDER BY id DESC LIMIT ?")?
            .query_map(params![name, since_ms, limit as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows
            .into_iter()
            .rev()
            .map(|(ms, kind, source, text)| {
                let what = match (kind.as_str(), source.as_deref()) {
                    ("in", Some("agent")) => "from Heimdall".to_owned(),
                    ("in", Some("local")) => "from a project tool".to_owned(),
                    ("in", Some(s)) => format!("from {s}"),
                    ("out", _) => "result".to_owned(),
                    (k, _) => k.to_owned(),
                };
                format!("{} {what}: {}", clock(ms), one_line(&text, 300))
            })
            .collect())
    }
    /// What a new thread is told of its crewmate's past: the last result whole (within limits) and the latest
    /// entries, oldest first.
    fn summary(&self, name: &str) -> Result<String> {
        let m = self.need(name)?;
        let mut out = String::new();
        if let Some(result) = &m.last_result {
            out.push_str(&format!("Your last result ({}):\n{}\n\n", m.last_ms.map(clock).unwrap_or_default(), cut(result, SUMMARY_BYTES / 2)));
        }
        let lines = self.history_lines(name, 0, 12)?;
        if !lines.is_empty() {
            out.push_str("Your latest history, oldest first:\n");
            let mut kept: Vec<String> = vec![];
            let mut size = out.len();
            for l in lines.iter().rev() {
                if size + l.len() > SUMMARY_BYTES {
                    break;
                }
                size += l.len() + 1;
                kept.push(l.clone());
            }
            kept.reverse();
            out.push_str(&kept.join("\n"));
        }
        Ok(out)
    }
    fn mark(&self, id: i64, state: &str, how: &str) -> Result<()> {
        self.db.lock().unwrap().execute("UPDATE inbox SET state=?,how=?,delivered_ms=? WHERE id=?", params![state, how, now(), id])?;
        Ok(())
    }
    fn inbox(&self, id: i64) -> Result<(String, String, String, String, Option<String>, i64, bool)> {
        Ok(self.db.lock().unwrap().query_row("SELECT name,source,text,state,how,attempts,retry FROM inbox WHERE id=?", [id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get::<_, i64>(6)? != 0))
        })?)
    }
    fn schedules(&self, name: &str) -> Result<Vec<(i64, String, String, i64)>> {
        Ok(self.db.lock().unwrap().prepare("SELECT id,every,message,next_ms FROM schedules WHERE name=? ORDER BY next_ms")?.query_map([name], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<rusqlite::Result<_>>()?)
    }
    /// Every crew schedule: (crewmate, id, every, message, next run). For the Crew island and the Scheduled list.
    pub fn all_schedules(&self) -> Result<Vec<(String, i64, String, String, i64)>> {
        Ok(self
            .db
            .lock()
            .unwrap()
            .prepare("SELECT s.name,s.id,s.every,s.message,s.next_ms FROM schedules s JOIN crew c ON c.name=s.name WHERE c.retired_ms IS NULL ORDER BY s.next_ms")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?
            .collect::<rusqlite::Result<_>>()?)
    }
    fn meta(&self, key: &str) -> Result<Option<String>> {
        Ok(self.db.lock().unwrap().query_row("SELECT value FROM meta WHERE key=?", [key], |r| r.get(0)).optional()?)
    }
    fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.db.lock().unwrap().execute("INSERT OR REPLACE INTO meta VALUES(?,?)", params![key, value])?;
        Ok(())
    }
}

fn a_is_list(a: &Value) -> bool {
    !matches!(a["action"].as_str(), Some("add" | "remove"))
}
fn validate_name(name: &str) -> Result<()> {
    let n = name.trim();
    ensure!(!n.is_empty() && n.chars().count() <= 40, "name: 1 to 40 characters");
    ensure!(n.chars().all(|c| c.is_alphanumeric() || " -_.,()'".contains(c)), "name: letters, digits, spaces and - _ . , ( ) ' only");
    Ok(())
}
fn clock(ms: i64) -> String {
    use chrono::TimeZone;
    chrono::Local.timestamp_millis_opt(ms).single().map(|t| t.format("%a %d %b %H:%M").to_string()).unwrap_or_default()
}
/// "not found" and the like: the thread no longer exists in T3.
pub fn gone(error: &str) -> bool {
    let e = error.to_lowercase();
    ["thread not found", "unknown thread", "no such thread", "thread_not_found"].iter().any(|p| e.contains(p))
        || (e.contains("thread") && (e.contains("does not exist") || e.contains("was deleted")))
}

// ---- schedules ----
/// When a schedule next fires after `after_ms`. Forms: `daily HH:MM`, `weekdays HH:MM`, `weekly <mon…sun> HH:MM`,
/// `every N min` / `every N h` (at least `MIN_EVERY_MIN`), `once YYYY-MM-DD HH:MM`. None: a `once` already past.
pub fn next_fire(every: &str, after_ms: i64) -> Result<Option<i64>> {
    use chrono::{Datelike, Local, NaiveTime, TimeZone, Timelike};
    let words: Vec<String> = every.split_whitespace().map(str::to_lowercase).collect();
    let after = Local.timestamp_millis_opt(after_ms).single().context("time")?;
    let hm = |s: &str| -> Result<NaiveTime> { NaiveTime::parse_from_str(s, "%H:%M").with_context(|| format!("'{s}': a time is HH:MM")) };
    let at_day = |day: chrono::NaiveDate, t: NaiveTime| Local.from_local_datetime(&day.and_time(t)).earliest().map(|d| d.timestamp_millis());
    let next_matching = |t: NaiveTime, ok: &dyn Fn(chrono::Weekday) -> bool| -> Option<i64> {
        (0..9).map(|d| after.date_naive() + chrono::Days::new(d)).filter(|d| ok(d.weekday())).filter_map(|d| at_day(d, t)).find(|ms| *ms > after_ms)
    };
    let words: Vec<&str> = words.iter().map(String::as_str).collect();
    let next = match words.as_slice() {
        ["daily", t] => next_matching(hm(t)?, &|_| true),
        ["weekdays", t] => next_matching(hm(t)?, &|d| d.number_from_monday() <= 5),
        ["weekly", day, t] => {
            let wd: chrono::Weekday = day.parse().map_err(|_| anyhow::anyhow!("'{day}': a day is mon, tue, … sun"))?;
            next_matching(hm(t)?, &|d| d == wd)
        }
        ["every", n, unit] => {
            let n: u64 = n.parse().context("every: a whole number")?;
            let min = match *unit {
                "min" | "mins" | "minute" | "minutes" | "m" => n,
                "h" | "hour" | "hours" => n * 60,
                _ => bail!("every N min or every N h"),
            };
            ensure!(min >= MIN_EVERY_MIN, "every: at least {MIN_EVERY_MIN} minutes");
            let _ = after.minute();
            Some(after_ms + (min * 60_000) as i64)
        }
        ["once", date, t] => {
            let day = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").context("once: a date is YYYY-MM-DD")?;
            at_day(day, hm(t)?).filter(|ms| *ms > after_ms)
        }
        _ => bail!("when: daily HH:MM, weekdays HH:MM, weekly <mon…sun> HH:MM, every N min|h, or once YYYY-MM-DD HH:MM"),
    };
    Ok(next)
}

/// The crew layer: the store plus Heimdall's T3 tasks.
pub struct Crew {
    pub store: CrewStore,
    workers: Weak<Workers>,
    /// One delivery at a time: the agent's tools and the pass never send the same message twice.
    deliver: tokio::sync::Mutex<()>,
    /// `WAKE_GRACE_MS`, `BODY_CHECK_MS` and the first retry's wait (30 s); tests shorten them (`set_timing`).
    timing: Mutex<(u64, u64, u64)>,
}
impl Crew {
    /// Opens the crew's store and attaches it to `workers` (`Workers::set_crew`).
    pub fn open(path: &Path, workers: &Arc<Workers>) -> Result<Arc<Self>> {
        let crew = Arc::new(Self { store: CrewStore::open(path)?, workers: Arc::downgrade(workers), deliver: tokio::sync::Mutex::new(()), timing: Mutex::new((WAKE_GRACE_MS, BODY_CHECK_MS, 30_000)) });
        workers.set_crew(crew.clone());
        Ok(crew)
    }
    /// Tests: every schedule comes due `by_ms` earlier.
    #[doc(hidden)]
    pub fn shift_schedules_for_tests(&self, by_ms: i64) {
        self.store.db.lock().unwrap().execute("UPDATE schedules SET next_ms=next_ms-?", [by_ms]).unwrap();
    }
    pub fn set_timing(&self, wake_grace_ms: u64, body_check_ms: u64) {
        let mut t = self.timing.lock().unwrap();
        (t.0, t.1, t.2) = (wake_grace_ms, body_check_ms, 0);
    }
    fn w(&self) -> Result<Arc<Workers>> {
        self.workers.upgrade().context("T3_UNAVAILABLE: the T3 workers are gone")
    }
    /// The agent's crew tools.
    pub async fn tool(&self, name: &str, a: Value, op: &str, permit: &DeliveryPermit) -> Result<String> {
        permit.check()?;
        // A replay of the same call answers as the first time (crew_tell is keyed by its inbox row instead; reads
        // are read again).
        let key = format!("op:{op}");
        let kept = !matches!(name, "crew_tell" | "crew_status") && !(name == "crew_schedule" && a_is_list(&a));
        if kept {
            if let Some(done) = self.store.meta(&key)? {
                return Ok(done);
            }
        }
        let out = self.tool_once(name, a, op, permit).await?;
        if kept {
            self.store.set_meta(&key, &out)?;
        }
        Ok(out)
    }
    async fn tool_once(&self, name: &str, a: Value, op: &str, permit: &DeliveryPermit) -> Result<String> {
        let s = |k: &str| a[k].as_str().map(str::trim).filter(|v| !v.is_empty()).map(str::to_owned);
        match name {
            "crew_add" => self.add(&a, op, permit).await,
            "crew_tell" => {
                let who = s("name").context("name missing")?;
                let message = s("message").context("message missing")?;
                let id = self.store.queue(&who, &message, "agent", &format!("tell:{op}"), false)?;
                self.deliver(id, permit).await
            }
            "crew_status" => self.status(s("name").as_deref(), a["days"].as_u64()),
            "crew_change" => self.change(&a, op, permit).await,
            "crew_schedule" => self.schedule(&a),
            "crew_retire" => self.retire(&s("name").context("name missing")?, s("reason").as_deref(), op, permit).await,
            other => bail!("unknown crew tool {other}"),
        }
    }
    async fn add(&self, a: &Value, op: &str, permit: &DeliveryPermit) -> Result<String> {
        let s = |k: &str| a[k].as_str().map(str::trim).filter(|v| !v.is_empty()).map(str::to_owned);
        let name = s("name").context("name missing")?;
        let folder = match s("folder") {
            Some(f) => {
                let p = PathBuf::from(&f);
                ensure!(p.is_absolute() && p.is_dir(), "folder: {f} is not an existing absolute folder");
                Some(f)
            }
            None => None,
        };
        let m = Mate {
            name: name.clone(),
            role: one_line(&s("role").context("role missing")?, 300),
            folder,
            model: s("model").unwrap_or_else(default_model),
            effort: s("effort").unwrap_or_else(default_effort),
            brief: s("brief"),
            thread: s("thread"),
            ..Default::default()
        };
        // A replay after a crash between the record and the answer: the record stands; its first message is keyed by
        // this op too, so it is queued once.
        let fresh = match self.store.meta(&format!("adding:{op}"))? {
            Some(f) => f == "1",
            None => {
                let f = self.store.add(&m)?;
                self.store.set_meta(&format!("adding:{op}"), if f { "1" } else { "0" })?;
                self.store.record(&name, "event", None, None, &format!("{} as: {}", if f { "added" } else { "re-added" }, m.role))?;
                f
            }
        };
        let mut out = format!(
            "Crewmate {name} {}: {} · {} {}{}.",
            if fresh { "added" } else { "re-added with its history" },
            m.role,
            m.model,
            m.effort,
            m.folder.as_deref().map(|f| format!(" · {f}")).unwrap_or_default()
        );
        if let Some(thread) = &m.thread {
            match self.adopt(&name, thread).await {
                Ok(()) => out.push_str(&format!(" Its body is thread {thread}.")),
                Err(e) => out.push_str(&format!(" Thread {thread} is not adopted yet ({e:#}); the next pass tries again.")),
            }
        }
        match s("message") {
            Some(message) => {
                let id = self.store.queue(&name, &message, "agent", &format!("tell:{op}"), false)?;
                out.push(' ');
                out.push_str(&self.deliver(id, permit).await?);
            }
            None if m.thread.is_none() => out.push_str(" Its thread starts with its first message (crew_tell)."),
            None => {}
        }
        Ok(out)
    }
    /// Makes `thread` the body of `name`: one of Heimdall's tasks from now on.
    async fn adopt(&self, name: &str, thread: &str) -> Result<()> {
        let w = self.w()?;
        let r = w.client.call("t3_thread_read", json!({"threadId":thread,"limit":1,"maxCharsPerItem":1,"runLimit":1})).await?;
        let m = self.store.need(name)?;
        let title = r["thread"]["title"].as_str().map(str::to_owned).unwrap_or_else(|| format!("{TITLE_PREFIX}{name}"));
        w.adopt_thread(&r["thread"], m.folder.as_deref().unwrap_or(""), &title)?;
        let db = self.store.db.lock().unwrap();
        db.execute("UPDATE crew SET thread=?,adopted=1,provider=?,checked_ms=? WHERE name=?", params![thread, r["thread"]["providerInstanceId"].as_str(), now(), m.name])?;
        drop(db);
        self.store.record(&m.name, "event", None, None, &format!("thread {thread} is its body"))?;
        Ok(())
    }
    /// The standing words above a new thread's first message.
    fn intro(&self, m: &Mate) -> String {
        let names = agent::identity::current();
        let (me, user) = (names.assistant.clone(), agent::identity::user());
        let brief = match &m.brief {
            Some(b) if Path::new(b).exists() => format!("Your standing brief is {b}. Read it first."),
            Some(b) => format!("Your standing brief is {b}; it does not exist yet. Write it before other work: what you own, how you run it, where its state lives, your routines."),
            None => "You have no standing brief file yet. Write one in your project folder (WORKER.md) before other work: what you own, how you run it, where its state lives, your routines, and tell me its path.".into(),
        };
        format!(
            "You are {name}, one of {me}'s crewmates: a permanent worker under {me}, who is {user}'s assistant and your only contact.\n\
             Your role: {role}\n\
             {folder}{brief}\n\n\
             How you work:\n\
             - Every message reaches you through {me}: from {me} itself, from one of {user}'s project tools on the Mac, from your own schedule, or {me}'s resume and wake notes. Each says which.\n\
             - Act on your own inside your project under the standing rules: nothing published, sent, bought or deleted of {user}'s without {user}'s yes.\n\
             - Never address {user} directly: no speech, no pages opened for {user}, no questions in T3. Whatever needs {user} goes in your final reply as one plain question with your default; {me} decides what {user} hears and when.\n\
             - Your thread stays open between messages; you are not closed after a reply, and {me} keeps your history.\n\
             - End every turn with a short report: what you did, what is still running, what needs {user}. If you start a background job or a sub-task you will report on, say so; {me} wakes you when it ends.",
            name = m.name,
            role = m.role,
            folder = m.folder.as_deref().map(|f| format!("Your project folder: {f}\n")).unwrap_or_default(),
        )
    }
    /// How a message is put to the crewmate, by where it came from.
    fn framed(source: &str, text: &str) -> String {
        match source {
            "local" => format!("From one of the project's tools on the Mac (passed on by Heimdall):\n{text}"),
            "schedule" => format!("Your schedule (passed on by Heimdall):\n{text}"),
            _ => text.to_owned(),
        }
    }
    /// Delivers inbox message `id` into its crewmate's thread: into its running turn, as a new turn, into its
    /// reopened archived thread, or into a new thread started from its brief and history when the old one is gone. A
    /// message that cannot go now stays pending for the next pass. Returns the line that says which happened.
    pub async fn deliver(&self, id: i64, permit: &DeliveryPermit) -> Result<String> {
        let _one = self.deliver.lock().await;
        let (name, source, text, state, how, attempts, retry) = self.store.inbox(id)?;
        if state != "pending" {
            return Ok(how.unwrap_or_else(|| format!("Message {id} to {name}: {state}.")));
        }
        let m = self.store.need(&name)?;
        if m.retired_ms.is_some() {
            self.store.mark(id, "refused", "retired")?;
            return Ok(format!("Not delivered: {name} is retired."));
        }
        if m.paused && source != "agent" {
            return Ok(format!("Held for {name}, which is paused; it goes when crew_change unpauses it."));
        }
        let message = Self::framed(&source, &text);
        let w = self.w()?;
        let result: Result<(String, String, Option<String>, bool)> = async {
            let Some(thread) = m.thread.clone() else {
                let (thread, run) = self.start(&m, id, &message, permit).await?;
                return Ok((format!("started its thread {thread}"), thread, run, true));
            };
            let read = w.client.call("t3_thread_read", json!({"threadId":thread,"limit":1,"maxCharsPerItem":1,"runLimit":1})).await;
            let row = match read {
                Err(e) if gone(&format!("{e:#}")) => {
                    self.lose(&m.name, &thread, "gone from T3")?;
                    let m = self.store.need(&name)?;
                    let (new, run) = self.start(&m, id, &message, permit).await?;
                    return Ok((format!("its thread {thread} was gone, so it started a new one ({new}) from its brief and history"), new, run, true));
                }
                Err(e) => return Err(e),
                Ok(r) => r["thread"].clone(),
            };
            if w.task_row(&thread)?.is_none() {
                w.adopt_thread(&row, m.folder.as_deref().unwrap_or(""), row["title"].as_str().unwrap_or(&format!("{TITLE_PREFIX}{name}")))?;
            }
            let how = if row["archived"] == true {
                "reopened its archived thread and started a turn"
            } else if row["activeRunId"].is_string() {
                "went into its running turn"
            } else {
                "started a turn in its thread"
            };
            let before = w.task_row(&thread)?.and_then(|r| r.0);
            let sent = w.execute_inner("t3_thread_send", json!({"threadId":thread,"message":message}), &format!("crew-send:{id}"), permit).await;
            if let Err(e) = &sent {
                if gone(&format!("{e:#}")) {
                    self.lose(&m.name, &thread, "gone from T3")?;
                    let m = self.store.need(&name)?;
                    let (new, run) = self.start(&m, id, &message, permit).await?;
                    return Ok((format!("its thread {thread} was gone, so it started a new one ({new}) from its brief and history"), new, run, true));
                }
            }
            sent?;
            let after = w.task_row(&thread)?.and_then(|r| r.0);
            let fresh = after.is_some() && after != before;
            Ok((how.to_owned(), thread, after, fresh))
        }
        .await;
        match result {
            Ok((how, thread, run, fresh)) => {
                if let Some(run) = &run {
                    self.store.set_run_source(&thread, run, &source, retry, fresh)?;
                }
                let line = format!("Told {name}: {how}. Its result {}.", if QUIET_SOURCES.contains(&source.as_str()) { "goes into the crew's history" } else { "arrives as a notice" });
                self.store.mark(id, "delivered", &line)?;
                self.store.record(&name, "in", Some(&source), run.as_deref(), &text)?;
                Ok(line)
            }
            Err(e) => {
                let attempts = attempts + 1;
                let error = one_line(&format!("{e:#}"), 300);
                let base = self.timing.lock().unwrap().2;
                let wait = base.saturating_mul(1 << attempts.min(8)).min(RETRY_MAX_MS) as i64;
                self.store.db.lock().unwrap().execute("UPDATE inbox SET attempts=?,error=?,next_try_ms=? WHERE id=?", params![attempts, error, now() + wait, id])?;
                let warned: bool = self.store.db.lock().unwrap().query_row("SELECT warned FROM inbox WHERE id=?", [id], |r| r.get::<_, i64>(0))? != 0;
                if attempts >= MAX_ATTEMPTS && !warned {
                    self.store.db.lock().unwrap().execute("UPDATE inbox SET warned=1 WHERE id=?", [id])?;
                    self.store.record(&name, "event", Some(&source), None, &format!("a message has not gone after {attempts} tries: {error}"))?;
                    let text = format!("Crew: a message to {name} has not gone after {attempts} tries ({error}); it stays in its inbox and is tried every few minutes. It is: {}", one_line(&text, 400));
                    w.actions.enqueue(&format!("crew:undelivered:{id}"), &text, permit)?;
                    let _ = w.actions.drain(permit);
                }
                Ok(format!("Queued for {name}: it could not go now ({error}). It stays in {name}'s inbox and goes on Heimdall's next pass."))
            }
        }
    }
    /// A new thread for `m`: its standing intro, what its history holds, then the message.
    async fn start(&self, m: &Mate, id: i64, message: &str, permit: &DeliveryPermit) -> Result<(String, Option<String>)> {
        let w = self.w()?;
        let base = format!("crew-start:{id}:{}", m.lost.as_deref().unwrap_or("first"));
        // The brief is composed once per start and kept with the message: a retry sends the very same launch, so its
        // operation record (`prepare`) recognises it instead of refusing changed arguments.
        let kept: Option<(String, i64)> = self
            .store
            .db
            .lock()
            .unwrap()
            .query_row("SELECT composed,composed_ms FROM inbox WHERE id=? AND composed LIKE ?", params![id, format!("{base}%")], |r| Ok((r.get(0)?, r.get::<_, Option<i64>>(1)?.unwrap_or(0))))
            .optional()?;
        let (op, task, since) = match kept.as_ref().and_then(|(k, ms)| k.split_once('\n').map(|(o, t)| (o.to_owned(), t.to_owned(), *ms))) {
            Some(k) => k,
            None => {
                let t = self.compose(m, message)?;
                self.store.db.lock().unwrap().execute("UPDATE inbox SET composed=?,composed_ms=? WHERE id=?", params![format!("{base}\n{t}"), now(), id])?;
                (base.clone(), t, now())
            }
        };
        match self.launch(m, &op, task.clone(), permit, &w).await {
            // A launch whose answer was lost: `delegate_task` looks for its thread first (`reconcile_launches`). Still
            // none after a settling time means it never launched; it goes again under a new operation.
            Err(e) if format!("{e:#}").contains("LAUNCH_UNCERTAIN") && now() - since >= (self.timing.lock().unwrap().2 * 4) as i64 => {
                let n = op.rsplit_once(":r").and_then(|(_, n)| n.parse::<u32>().ok()).unwrap_or(0) + 1;
                let again = format!("{base}:r{n}");
                self.store.db.lock().unwrap().execute("UPDATE inbox SET composed=?,composed_ms=? WHERE id=?", params![format!("{again}\n{task}"), now(), id])?;
                self.launch(m, &again, task, permit, &w).await
            }
            other => other,
        }
    }
    fn compose(&self, m: &Mate, message: &str) -> Result<String> {
        let summary = self.store.summary(&m.name)?;
        let mut task = self.intro(m);
        if let Some(lost) = &m.lost {
            task.push_str(&format!("\n\nThis is a new thread: your previous one ({lost}) was lost. Pick up from your record below and your project's own files."));
        }
        if !summary.is_empty() {
            task.push_str(&format!("\n\nWhat {} keeps of your history:\n{summary}", agent::identity::current().assistant));
        }
        task.push_str(&format!("\n\nThe message:\n{message}"));
        Ok(task)
    }
    async fn launch(&self, m: &Mate, op: &str, task: String, permit: &DeliveryPermit, w: &Arc<Workers>) -> Result<(String, Option<String>)> {
        let op = op.to_owned();
        // Public edition: no model named means the user's own T3 providers decide (`workers::catalog_default`).
        let mut target = if m.model.is_empty() { json!({}) } else { json!({"model":m.model}) };
        if !m.effort.is_empty() {
            target["options"] = json!({"effort":m.effort});
        }
        let mut a = json!({"task":task,"title":format!("{TITLE_PREFIX}{}", m.name),"target":target});
        if let Some(f) = &m.folder {
            a["path"] = json!(f);
        }
        w.execute_inner("delegate_task", a, &op, permit).await?;
        let thread = w.op_result(&op)?.and_then(|r| r["task_id"].as_str().map(str::to_owned)).context("the new thread's id was not recorded")?;
        let row = w.task_row(&thread)?;
        let db = self.store.db.lock().unwrap();
        db.execute(
            "UPDATE crew SET thread=?,adopted=1,lost=NULL,provider=?,checked_ms=?,resume_run=NULL,waiting=NULL WHERE name=?",
            params![thread, row.as_ref().map(|r| r.2.clone()), now(), m.name],
        )?;
        drop(db);
        self.store.record(&m.name, "event", None, None, &match &m.lost {
            Some(l) => format!("new thread {thread}, replacing {l}"),
            None => format!("thread {thread} started"),
        })?;
        Ok((thread, row.and_then(|r| r.0)))
    }
    /// `thread` is no longer `name`'s body; the next delivery starts a new one.
    fn lose(&self, name: &str, thread: &str, why: &str) -> Result<()> {
        self.w()?.forget(thread)?;
        self.store.db.lock().unwrap().execute("UPDATE crew SET thread=NULL,adopted=0,lost=? WHERE name=? AND thread=?", params![thread, name, thread])?;
        self.store.record(name, "event", None, None, &format!("lost thread {thread}: {why}"))
    }
    /// Called by `Workers` with each finished run of a crew thread, before its notice: recorded, and true when the
    /// agent is not told now (a routine result, or a first failure the next pass resumes).
    pub fn on_result(&self, thread: &str, run: &str, state: &str, reply: Option<&str>, still: Option<&str>, carried: &[String]) -> bool {
        let r = (|| -> Result<bool> {
            let Some(name) = self.store.crewmate_of(thread)? else { return Ok(false) };
            let (source, retry) = self.store.run_source(thread, run)?;
            let body = format!("{state}{}{}", still.map(|s| format!(" ({s})")).unwrap_or_default(), reply.map(|r| format!(": {r}")).unwrap_or_default());
            self.store.record(&name, "out", Some(&source), Some(run), &body)?;
            {
                let db = self.store.db.lock().unwrap();
                db.execute("UPDATE crew SET last_state=?,last_ms=?,last_result=COALESCE(?,last_result) WHERE name=?", params![state, now(), reply.map(|r| cut(r, HISTORY_TEXT)), name])?;
            }
            let m = self.store.need(&name)?;
            // Earlier turns this notice carries: if any answers the agent, the notice goes to the agent whole.
            for c in carried.iter().filter(|c| c.as_str() != run) {
                if !QUIET_SOURCES.contains(&self.store.run_source(thread, c)?.0.as_str()) {
                    return Ok(false);
                }
            }
            Ok(match state {
                // A usage limit or a sign-in failure has its own handling (the account move, the sign-in retry).
                "failed" | "interrupted" if reply.and_then(|r| r.lines().next()).is_some_and(|l| crate::failover::usage_limit(l).is_some() || crate::text::auth_failure(l).is_some()) => false,
                "failed" | "interrupted" if !retry && m.retired_ms.is_none() => {
                    let db = self.store.db.lock().unwrap();
                    db.execute("UPDATE crew SET resume_run=?,resume_state=? WHERE name=?", params![run, state, name])?;
                    true
                }
                "completed" => QUIET_SOURCES.contains(&source.as_str()) && !reply.is_some_and(crate::text::waits_on_user),
                _ => false,
            })
        })();
        r.unwrap_or_else(|e| {
            eprintln!("t3: crew result of {thread} not recorded: {e:#}");
            false
        })
    }
    /// One crew pass, after each control-plane pass: adoptions, resumes, schedules, the inbox, sub-job wakes, and a
    /// look at each thread every `BODY_CHECK_MS`.
    pub async fn pass(&self, permit: &DeliveryPermit) -> Result<()> {
        let w = self.w()?;
        let (grace, body_check, _) = *self.timing.lock().unwrap();
        let mates = self.store.all(false)?;
        // Threads named but not adopted yet (an import, a failed adoption).
        for m in mates.iter().filter(|m| m.thread.is_some() && !m.adopted) {
            let thread = m.thread.clone().unwrap();
            // An imported line may name only a thread Heimdall itself started: the local import cannot hand a crewmate
            // one of the user's own threads (crew_add, the agent's own call, can).
            if self.store.int_col(&m.name, "imported")? == Some(1) && w.task_row(&thread)?.is_none() {
                self.lose(&m.name, &thread, "not one of Heimdall's own threads, so the import does not adopt it")?;
                continue;
            }
            if let Err(e) = self.adopt(&m.name, &thread).await {
                if gone(&format!("{e:#}")) {
                    self.lose(&m.name, &thread, "gone from T3 before it was adopted")?;
                } else {
                    eprintln!("t3: crewmate {} not adopted yet: {e:#}", m.name);
                }
            }
        }
        // A turn that failed or was interrupted: resumed once, with one notice.
        for m in &mates {
            let resume: Option<(String, String)> = self.store.db.lock().unwrap().query_row("SELECT resume_run,resume_state FROM crew WHERE name=? AND resume_run IS NOT NULL", [&m.name], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
            let (Some((run, state)), Some(thread)) = (resume, m.thread.clone()) else { continue };
            let (source, _) = self.store.run_source(&thread, &run)?;
            let note = format!("Heimdall: your last turn ended {state} before it finished (a restart, a crash or a provider error). Carry on from where you left off. First check whether background jobs or sub-tasks you had started still run, and restart any that died.");
            let id = self.store.queue(&m.name, &note, &source, &format!("resume:{thread}:{run}"), true)?;
            self.store.set(&m.name, "resume_run", rusqlite::types::Value::Null)?;
            let how = self.deliver(id, permit).await?;
            let text = format!("Crew: {}'s turn ended {state}; Heimdall resumed it by itself ({how}) Nothing to do unless it fails again.", m.name);
            w.actions.enqueue(&format!("crew:resumed:{thread}:{run}"), &text, permit)?;
                    let _ = w.actions.drain(permit);
        }
        // Schedules that came due: into the inbox under an id per firing, so a firing is never doubled or lost.
        let due: Vec<(i64, String, String, String, i64)> = self
            .store
            .db
            .lock()
            .unwrap()
            .prepare("SELECT s.id,s.name,s.every,s.message,s.next_ms FROM schedules s JOIN crew c ON c.name=s.name WHERE s.next_ms<=? AND c.retired_ms IS NULL AND c.paused=0")?
            .query_map([now()], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?
            .collect::<rusqlite::Result<_>>()?;
        for (sid, name, every, message, next) in due {
            self.store.queue(&name, &message, "schedule", &format!("schedule:{sid}:{next}"), false)?;
            self.store.record(&name, "event", Some("schedule"), None, &format!("schedule {sid} ({every}) fired"))?;
            match next_fire(&every, now().max(next))? {
                Some(n) => self.store.db.lock().unwrap().execute("UPDATE schedules SET next_ms=?,last_ms=? WHERE id=?", params![n, now(), sid])?,
                None => self.store.db.lock().unwrap().execute("DELETE FROM schedules WHERE id=?", [sid])?,
            };
        }
        // The inbox: whatever has not gone yet.
        for (id, name, _) in self.store.pending()? {
            permit.check()?;
            if let Err(e) = self.deliver(id, permit).await {
                eprintln!("t3: crew message {id} to {name} not delivered: {e:#}");
            }
        }
        // Sub-job wakes: the crewmate waited on its sub-agents or background commands; they ended and nothing woke it.
        for m in self.store.all(false)? {
            let Some(thread) = m.thread.clone().filter(|_| m.adopted) else { continue };
            let open = w.open_work(&thread)?;
            let row = w.task_row(&thread)?;
            let (run, status) = row.as_ref().map(|r| (r.0.clone(), r.1.clone())).unwrap_or_default();
            let waiting: Option<(String, Option<String>, Option<i64>)> = self.store.db.lock().unwrap().query_row("SELECT waiting,waiting_run,waiting_clear_ms FROM crew WHERE name=? AND waiting IS NOT NULL", [&m.name], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()?;
            if !open.is_empty() {
                // Work a running turn waits on inside itself is the turn's own business; only a turn that ended with
                // work still open is waiting.
                if crate::threads::ACTIVE.contains(&status.as_str()) {
                    continue;
                }
                self.store.db.lock().unwrap().execute("UPDATE crew SET waiting=?,waiting_run=?,waiting_clear_ms=NULL WHERE name=?", params![open.join("; "), run, m.name])?;
                continue;
            }
            let Some((labels, waited_run, cleared)) = waiting else { continue };
            if crate::threads::ACTIVE.contains(&status.as_str()) || run != waited_run {
                // A turn started since (T3's own wake, or a message): it is awake.
                self.store.set(&m.name, "waiting", rusqlite::types::Value::Null)?;
                continue;
            }
            match cleared {
                None => self.store.set(&m.name, "waiting_clear_ms", now().into())?,
                Some(c) if now() - c >= grace as i64 => {
                    let note = format!("Heimdall: the work you were waiting on has ended ({labels}). Check its result, carry on, and end with your report.");
                    let (source, _) = self.store.run_source(&thread, run.as_deref().unwrap_or(""))?;
                    let id = self.store.queue(&m.name, &note, &source, &format!("wake:{thread}:{}", run.as_deref().unwrap_or("")), false)?;
                    self.store.set(&m.name, "waiting", rusqlite::types::Value::Null)?;
                    self.deliver(id, permit).await?;
                }
                Some(_) => {}
            }
        }
        // Each thread now and then: still there, and on which account (a usage-limit move changes it).
        for m in self.store.all(false)? {
            let Some(thread) = m.thread.clone().filter(|_| m.adopted) else { continue };
            if self.store.int_col(&m.name, "checked_ms")?.is_some_and(|c| now() - c < body_check as i64) {
                continue;
            }
            match w.client.call("t3_thread_read", json!({"threadId":thread,"limit":1,"maxCharsPerItem":1,"runLimit":1})).await {
                Err(e) if gone(&format!("{e:#}")) => {
                    let working = w.task_row(&thread)?.is_some_and(|r| crate::threads::ACTIVE.contains(&r.1.as_str()));
                    self.lose(&m.name, &thread, "gone from T3")?;
                    if working {
                        let note = "Heimdall: your thread was lost while you were working. Carry on with what you were doing, from your record above and your project's files.";
                        let id = self.store.queue(&m.name, note, "agent", &format!("lost:{thread}"), true)?;
                        let how = self.deliver(id, permit).await?;
                        w.actions.enqueue(&format!("crew:lost:{thread}"), &format!("Crew: {}'s thread {thread} disappeared mid-work; {how}", m.name), permit)?;
                    let _ = w.actions.drain(permit);
                    }
                }
                Err(e) => eprintln!("t3: crewmate {}'s thread not checked: {e:#}", m.name),
                Ok(r) => {
                    let provider = r["thread"]["providerInstanceId"].as_str().map(str::to_owned);
                    if provider.is_some() && provider != m.provider {
                        self.store.record(&m.name, "event", None, None, &format!("runs on {} now", provider.clone().unwrap_or_default()))?;
                    }
                    self.store.db.lock().unwrap().execute("UPDATE crew SET checked_ms=?,provider=COALESCE(?,provider) WHERE name=?", params![now(), provider, m.name])?;
                }
            }
        }
        Ok(())
    }
    /// A crewmate's state in a word: retired, paused, working, waiting (on its sub-jobs), failed, idle, unborn.
    pub fn state(&self, m: &Mate) -> String {
        if m.retired_ms.is_some() {
            return "retired".into();
        }
        if m.paused {
            return "paused".into();
        }
        let Some(thread) = m.thread.as_deref() else { return if m.lost.is_some() { "lost its thread".into() } else { "not started".into() } };
        let w = self.w().ok();
        let row = w.as_ref().and_then(|w| w.task_row(thread).ok().flatten());
        let waiting = self.store.text_col(&m.name, "waiting").ok().flatten();
        match row {
            Some((_, status, ..)) if crate::threads::ACTIVE.contains(&status.as_str()) => "working".into(),
            _ if waiting.is_some() => "waiting on its sub-jobs".into(),
            Some((_, status, ..)) if status == "failed" => "failed".into(),
            _ => "idle".into(),
        }
    }
    /// `crew_status`: the crew in one line each with what happened since the agent last looked, or one crewmate.
    fn status(&self, name: Option<&str>, days: Option<u64>) -> Result<String> {
        if let Some(name) = name {
            let m = self.store.need(name)?;
            let mut out = format!("{} · {} · {}", m.name, self.state(&m), m.role);
            out.push_str(&format!("\nModel: {} {}{}", m.model, m.effort, m.provider.as_deref().map(|p| format!(" on {p}")).unwrap_or_default()));
            if let Some(f) = &m.folder {
                out.push_str(&format!("\nFolder: {f}"));
            }
            if let Some(b) = &m.brief {
                out.push_str(&format!("\nStanding brief: {b}"));
            }
            out.push_str(&format!("\nThread: {}", m.thread.as_deref().unwrap_or("none yet")));
            for (id, every, message, next) in self.store.schedules(&m.name)? {
                out.push_str(&format!("\nSchedule {id}: {every}, next {} · {}", clock(next), one_line(&message, 120)));
            }
            let pending = self.store.db.lock().unwrap().query_row("SELECT COUNT(*) FROM inbox WHERE name=? AND state='pending'", [&m.name], |r| r.get::<_, i64>(0))?;
            if pending > 0 {
                out.push_str(&format!("\nInbox: {pending} not delivered yet"));
            }
            if let Some(r) = &m.last_result {
                out.push_str(&format!("\nLast result ({}, {}):\n{}", m.last_state.as_deref().unwrap_or("?"), m.last_ms.map(clock).unwrap_or_default(), cut(r, 4000)));
            }
            let since = days.map(|d| now() - (d.min(60) as i64) * 86_400_000).unwrap_or(0);
            let lines = self.store.history_lines(&m.name, since, if days.is_some() { 200 } else { 10 })?;
            if !lines.is_empty() {
                out.push_str(&format!("\nHistory{}:\n{}", days.map(|d| format!(" (last {d} days)")).unwrap_or_default(), lines.join("\n")));
            }
            return Ok(out);
        }
        let mates = self.store.all(false)?;
        if mates.is_empty() {
            return Ok("No crewmates yet; add one with crew_add.".into());
        }
        let seen: i64 = self.store.meta("seen_ms")?.and_then(|v| v.parse().ok()).unwrap_or(0);
        let mut out = vec!["name · state · role · model · since you last looked:".to_owned()];
        for m in &mates {
            let (ins, routine, outs): (i64, i64, i64) = self.store.db.lock().unwrap().query_row(
                "SELECT SUM(kind='in'),SUM(kind='in' AND source IN ('local','schedule')),SUM(kind='out') FROM history WHERE name=? AND ms>?",
                params![m.name, seen],
                |r| Ok((r.get::<_, Option<i64>>(0)?.unwrap_or(0), r.get::<_, Option<i64>>(1)?.unwrap_or(0), r.get::<_, Option<i64>>(2)?.unwrap_or(0))),
            )?;
            let since = if ins + outs == 0 {
                "nothing".to_owned()
            } else {
                let last = m.last_result.as_deref().filter(|_| m.last_ms.is_some_and(|l| l > seen)).map(|r| format!("; last: {}", one_line(r, 160))).unwrap_or_default();
                format!("{ins} in ({routine} routine), {outs} results{last}")
            };
            out.push(format!("{} · {} · {} · {} {} · {since}", m.name, self.state(m), one_line(&m.role, 80), m.model, m.effort));
        }
        let schedules = self.store.all_schedules()?;
        if let Some((name, _, every, _, next)) = schedules.first() {
            out.push(format!("{} schedules; next: {name} ({every}) at {}", schedules.len(), clock(*next)));
        }
        self.store.set_meta("seen_ms", &now().to_string())?;
        Ok(out.join("\n"))
    }
    /// `crew_change`: re-brief, move to a model, point at another thread, pause or resume.
    async fn change(&self, a: &Value, op: &str, permit: &DeliveryPermit) -> Result<String> {
        let s = |k: &str| a[k].as_str().map(str::trim).filter(|v| !v.is_empty()).map(str::to_owned);
        let m = self.store.need(&s("name").context("name missing")?)?;
        ensure!(m.retired_ms.is_none(), "CREW_RETIRED: {} is retired; crew_add brings it back", m.name);
        let mut done = vec![];
        let mut rebrief = vec![];
        if let Some(role) = s("role") {
            self.store.set(&m.name, "role", one_line(&role, 300).into())?;
            rebrief.push(format!("your role is now: {role}"));
            done.push("role");
        }
        if let Some(brief) = s("brief") {
            self.store.set(&m.name, "brief", brief.clone().into())?;
            rebrief.push(format!("your standing brief is now {brief}; read it"));
            done.push("brief");
        }
        if let Some(folder) = s("folder") {
            ensure!(Path::new(&folder).is_dir(), "folder: {folder} is not an existing folder");
            self.store.set(&m.name, "folder", folder.clone().into())?;
            rebrief.push(format!("your project folder is now {folder}"));
            done.push("folder");
        }
        if let Some(p) = a["paused"].as_bool() {
            self.store.set(&m.name, "paused", (p as i64).into())?;
            done.push(if p { "paused" } else { "unpaused" });
        }
        let (model, effort) = (s("model"), s("effort"));
        if model.is_some() || effort.is_some() {
            let model = model.unwrap_or(m.model.clone());
            let effort = effort.unwrap_or(m.effort.clone());
            if let (Some(thread), Some(provider)) = (m.thread.as_deref(), m.provider.as_deref()) {
                let selection = json!({"instanceId":provider,"model":model,"options":{"effort":effort}});
                self.w()?.execute_inner("t3_call", json!({"tool":"t3_thread_configure","args":{"threadId":thread,"modelSelection":selection}}), &format!("crew-model:{op}"), permit).await?;
            }
            self.store.set(&m.name, "model", model.into())?;
            self.store.set(&m.name, "effort", effort.into())?;
            done.push("model");
        }
        if let Some(thread) = s("thread") {
            self.adopt(&m.name, &thread).await?;
            done.push("thread");
        }
        ensure!(!done.is_empty(), "nothing to change: give role, brief, folder, model, effort, thread or paused");
        self.store.record(&m.name, "event", None, None, &format!("changed: {}", done.join(", ")))?;
        let mut out = format!("{}: changed {}.", m.name, done.join(", "));
        if !rebrief.is_empty() {
            let note = format!("Heimdall changed your standing orders: {}.", rebrief.join("; "));
            let id = self.store.queue(&m.name, &note, "agent", &format!("rebrief:{op}"), false)?;
            out.push(' ');
            out.push_str(&self.deliver(id, permit).await?);
        }
        Ok(out)
    }
    /// `crew_schedule`: add, remove or list a crewmate's schedules. A firing goes straight into its thread through
    /// the inbox, without a turn of the agent; its result is in the digest.
    fn schedule(&self, a: &Value) -> Result<String> {
        let s = |k: &str| a[k].as_str().map(str::trim).filter(|v| !v.is_empty()).map(str::to_owned);
        match a["action"].as_str().unwrap_or("list") {
            "add" => {
                let m = self.store.need(&s("name").context("name missing")?)?;
                ensure!(m.retired_ms.is_none(), "CREW_RETIRED: {} is retired", m.name);
                let when = s("when").context("when missing")?;
                let message = s("message").context("message missing: what the crewmate does each time")?;
                let next = next_fire(&when, now())?.context("when: that time has passed")?;
                let id = {
                    let db = self.store.db.lock().unwrap();
                    db.execute("INSERT INTO schedules(name,every,message,next_ms,created_ms) VALUES(?,?,?,?,?)", params![m.name, when, message, next, now()])?;
                    db.last_insert_rowid()
                };
                self.store.record(&m.name, "event", None, None, &format!("schedule {id} added: {when}: {}", one_line(&message, 200)))?;
                Ok(format!("Schedule {id} for {}: {when}, first at {}.", m.name, clock(next)))
            }
            "remove" => {
                let id = a["id"].as_i64().context("id missing")?;
                let name: Option<String> = self.store.db.lock().unwrap().query_row("SELECT name FROM schedules WHERE id=?", [id], |r| r.get(0)).optional()?;
                let name = name.with_context(|| format!("no schedule {id}"))?;
                self.store.db.lock().unwrap().execute("DELETE FROM schedules WHERE id=?", [id])?;
                self.store.record(&name, "event", None, None, &format!("schedule {id} removed"))?;
                Ok(format!("Schedule {id} of {name} removed."))
            }
            _ => {
                let rows = self.store.all_schedules()?;
                let rows: Vec<_> = rows.into_iter().filter(|r| s("name").is_none_or(|n| r.0.eq_ignore_ascii_case(&n))).collect();
                if rows.is_empty() {
                    return Ok("No crew schedules.".into());
                }
                Ok(rows.iter().map(|(name, id, every, message, next)| format!("{id} · {name} · {every} · next {} · {}", clock(*next), one_line(message, 100))).collect::<Vec<_>>().join("\n"))
            }
        }
    }
    /// `crew_retire`: its running turn stopped, its thread archived, its schedules and inbox ended; its record and
    /// history stay, and `crew_add` with its name brings it back.
    async fn retire(&self, name: &str, reason: Option<&str>, op: &str, permit: &DeliveryPermit) -> Result<String> {
        let m = self.store.need(name)?;
        ensure!(m.retired_ms.is_none(), "{} is already retired", m.name);
        let mut out = format!("{} retired; its history stays and crew_add brings it back.", m.name);
        if let Some(thread) = m.thread.as_deref().filter(|_| m.adopted) {
            let mut a = json!({"taskId":thread});
            if let Some(r) = reason {
                a["reason"] = json!(r);
            }
            match self.w()?.execute_inner("task_cancel", a, &format!("crew-retire:{op}"), permit).await {
                Ok(line) => out.push_str(&format!(" {line}")),
                Err(e) => out.push_str(&format!(" Its thread was not archived: {e:#}")),
            }
        }
        let db = self.store.db.lock().unwrap();
        db.execute("UPDATE crew SET retired_ms=?,resume_run=NULL,waiting=NULL WHERE name=?", params![now(), m.name])?;
        db.execute("UPDATE inbox SET state='refused',how='retired' WHERE name=? AND state='pending'", [&m.name])?;
        db.execute("DELETE FROM schedules WHERE name=?", [&m.name])?;
        drop(db);
        self.store.record(&m.name, "event", None, None, &format!("retired{}", reason.map(|r| format!(": {r}")).unwrap_or_default()))?;
        Ok(out)
    }
}

// ---- the local command ----
/// `heimdall --crew <command>`: the way in for local processes (a project's server passing on the user's notes).
/// - `tell <name> <file|-> [--id KEY]`: queues the file's text (or stdin) for an existing crewmate. Heimdall delivers it
///   on its next pass, records it, and its result goes into the crew's history. `--id` makes a retry the same message.
/// - `list`: the crew, one line each (read-only).
/// - `import <registry.md>`: the one-time migration of an existing workers registry (a Markdown file) (night pass only, not for project tools).
///
/// It may not create, change or retire a crewmate (outside `import`), start or read threads, or call any other tool:
/// it writes one inbox row in `crew.sqlite` and nothing else.
pub fn cli(args: &[String], data: impl FnOnce() -> Result<PathBuf>, stdin: &mut dyn std::io::Read) -> Result<String> {
    let usage = "usage: heimdall --crew tell <name> <file|-> [--id KEY] | list | import <registry.md>";
    let store = || -> Result<CrewStore> { CrewStore::open(&data()?.join("agent/crew.sqlite")) };
    match args.first().map(String::as_str) {
        Some("tell") => {
            let (name, file) = (args.get(1).context(usage)?, args.get(2).context(usage)?);
            let id = args.iter().position(|a| a == "--id").and_then(|i| args.get(i + 1));
            let mut text = String::new();
            if file == "-" {
                stdin.read_to_string(&mut text)?;
            } else {
                let meta = std::fs::metadata(file).with_context(|| format!("{file}: not readable"))?;
                ensure!(meta.len() as usize <= MESSAGE_BYTES, "{file}: at most {} KB", MESSAGE_BYTES / 1024);
                text = std::fs::read_to_string(file).with_context(|| format!("{file}: not UTF-8 text"))?;
            }
            let store = store()?;
            let name = &store.need(name)?.name;
            let op = match id {
                Some(k) => format!("local:{}:{k}", name.to_lowercase()),
                None => format!("local:{}", uuid::Uuid::new_v4()),
            };
            let n = store.queue(name, &text, "local", &op, false)?;
            Ok(format!("Queued for {name} as message {n}; Heimdall delivers it on its next pass (within a minute while it runs) and keeps it in {name}'s history."))
        }
        Some("list") => {
            let mates = store()?.all(false)?;
            if mates.is_empty() {
                return Ok("No crewmates.".into());
            }
            Ok(mates.iter().map(|m| format!("{} · {}{}", m.name, m.role, if m.paused { " · paused" } else { "" })).collect::<Vec<_>>().join("\n"))
        }
        Some("import") => {
            let file = args.get(1).context(usage)?;
            let text = std::fs::read_to_string(file)?;
            let vault = Path::new(file).canonicalize()?.parent().and_then(Path::parent).map(Path::to_path_buf).context("registry: not inside the Vault")?;
            let store = store()?;
            let mut added = vec![];
            for m in parse_registry(&text, &vault) {
                if store.get(&m.name)?.is_some() {
                    continue;
                }
                store.add(&m)?;
                store.set(&m.name, "imported", 1.into())?;
                store.record(&m.name, "event", None, None, "imported from the workers registry")?;
                added.push(m.name);
            }
            Ok(format!("Imported {} crewmates{}; Heimdall adopts their threads on its next pass.", added.len(), if added.is_empty() { String::new() } else { format!(": {}", added.join(", ")) }))
        }
        _ => bail!(usage),
    }
}

/// The active workers of a workers registry file (one line each: project · what it owns · T3 thread · model · standing
/// brief · started) as crewmates. Paths are made absolute from the Vault root; a brief "(to write)" is kept as the path
/// it will have.
pub fn parse_registry(text: &str, vault: &Path) -> Vec<Mate> {
    let mut out = vec![];
    let mut active = false;
    for line in text.lines() {
        if line.starts_with("## ") {
            active = line.trim() == "## Active";
            continue;
        }
        let Some(line) = line.strip_prefix("- ").filter(|_| active) else { continue };
        let parts: Vec<&str> = line.split(" · ").map(str::trim).collect();
        if parts.len() < 5 {
            continue;
        }
        let thread = parts[2].trim_start_matches("mcp:").to_owned();
        let thread = if parts[2].starts_with("mcp:") { format!("mcp:{thread}") } else { parts[2].to_owned() };
        let (model, effort, provider) = parse_model(parts[3]);
        let brief_raw = parts[4].replace("(to write)", "");
        let first = brief_raw.split_whitespace().next().unwrap_or("").to_owned();
        let brief = (!first.is_empty()).then(|| vault.join(&first));
        let folder = brief.as_ref().and_then(|b| b.parent()).filter(|p| p.is_dir()).map(|p| p.to_string_lossy().into_owned());
        let name: String = parts[0].chars().filter(|c| c.is_alphanumeric() || " -_.,()'".contains(*c)).collect::<String>().trim().chars().take(40).collect();
        if name.is_empty() {
            continue;
        }
        out.push(Mate {
            name,
            role: parts[1].to_owned(),
            folder,
            model,
            effort,
            provider,
            brief: brief.map(|b| b.to_string_lossy().into_owned()),
            thread: Some(thread),
            adopted: false,
            ..Default::default()
        });
    }
    out
}
fn parse_model(text: &str) -> (String, String, Option<String>) {
    let provider = text.split('(').nth(1).map(|p| p.trim_end_matches(')').trim().to_owned());
    let low = text.split('(').next().unwrap_or("").to_lowercase();
    let family = ["opus", "sonnet", "haiku", "fable"].into_iter().find(|f| low.contains(f)).unwrap_or("opus");
    let version = low.split_whitespace().find(|w| w.chars().next().is_some_and(|c| c.is_ascii_digit())).unwrap_or("5.5").replace('.', "-");
    let effort = ["low", "medium", "high", "xhigh", "max"].into_iter().find(|e| low.split_whitespace().any(|w| w == *e)).unwrap_or("medium");
    (format!("claude-{family}-{version}"), effort.to_owned(), provider)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn registry_lines_become_crewmates() {
        let text = "# x\n## Active\n\n- Newsletter, builder · the approval hub · mcp:024c · Opus 5.5 medium (claudeAgent_2) · Projects/Newsletter/WORKER.md · 14 Nov\n- Decider v7 · plan · mcp:aeb6 · Opus 5.5 high (claudeAgent) · Projects/Heimdall/lab/decider-v7/WORKER.md (to write) · 14 Nov\n## Retired\n- Old · x · mcp:1 · Opus 5.5 · a · b\n";
        let got = parse_registry(text, Path::new("/V"));
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].name, "Newsletter, builder");
        assert_eq!(got[0].thread.as_deref(), Some("mcp:024c"));
        assert_eq!((got[0].model.as_str(), got[0].effort.as_str()), ("claude-opus-5-5", "medium"));
        assert_eq!(got[0].provider.as_deref(), Some("claudeAgent_2"));
        assert_eq!(got[1].brief.as_deref(), Some("/V/Projects/Heimdall/lab/decider-v7/WORKER.md"));
        assert_eq!(got[1].effort, "high");
    }
    #[test]
    fn schedules_parse_and_advance() {
        use chrono::{Local, TimeZone};
        let base = Local.with_ymd_and_hms(2026, 11, 14, 18, 0, 0).unwrap().timestamp_millis();
        let at = |ms: i64| Local.timestamp_millis_opt(ms).unwrap().format("%a %H:%M").to_string();
        assert_eq!(at(next_fire("daily 07:00", base).unwrap().unwrap()), "Sun 07:00");
        assert_eq!(at(next_fire("daily 19:30", base).unwrap().unwrap()), "Sat 19:30");
        assert_eq!(at(next_fire("weekdays 07:00", base).unwrap().unwrap()), "Mon 07:00");
        assert_eq!(at(next_fire("weekly wed 09:00", base).unwrap().unwrap()), "Wed 09:00");
        assert_eq!(next_fire("every 30 min", base).unwrap().unwrap() - base, 30 * 60_000);
        assert!(next_fire("every 5 min", base).is_err());
        assert_eq!(next_fire("once 2026-11-13 07:00", base).unwrap(), None);
        assert!(next_fire("sometimes", base).is_err());
    }
    #[test]
    fn missing_threads_are_recognised() {
        assert!(gone("T3_TOOL_FAILED: Thread not found"));
        assert!(!gone("T3 Code closed: connection refused"));
    }
}

/// A crewmate's model when none is named: the personal edition's task default; the public edition names no vendor.
#[cfg(not(feature = "public"))]
fn default_model() -> String { crate::workers::DEFAULT_TASK_MODEL.into() }
#[cfg(feature = "public")]
fn default_model() -> String { String::new() }
#[cfg(not(feature = "public"))]
fn default_effort() -> String { crate::workers::DEFAULT_TASK_EFFORT.into() }
#[cfg(feature = "public")]
fn default_effort() -> String { String::new() }
