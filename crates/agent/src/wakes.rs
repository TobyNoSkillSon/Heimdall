//! Scheduled wakes and the delivery heartbeat (night 21 Oct). The user, 01:38: "You should be able to put a schedule on
//! yourself, when to wake yourself up … we should always have a timer in the background that can wake you … When I
//! tell you 'at seven, do this', you receive that notification later."
//!
//! A wake is one row in `agent/wakes.sqlite` (WAL, synchronous FULL): the agent's `schedule_wake` and the
//! `Heimdall --schedule-wake` command both insert into it, so it survives restarts and two writers are safe. The
//! heartbeat (`Wakes::heartbeat`, one task beside the T3 watcher) wakes at the next due time or every `TICK_MS`,
//! whichever comes first, and on each pass:
//! 1. claims every due pending wake (`pending` → `sent`; a wake more than `LATE_MS` past due is marked late, so one
//!    that came due while Heimdall or the agent was off arrives once at the next start, with its original time);
//! 2. puts each sent wake into the action outbox under `selfwake:<id>` (the existing notice path: `Actions::drain` →
//!    the agent's durable, id-deduplicated notice queue), re-arming a row an Agent Off/On dropped before delivery;
//! 3. runs registered checks (`add_check`), then drains the outbox, which retries every undelivered notice, also
//!    ones the T3 watcher could not drain because its own pass failed;
//! 4. marks wakes whose outbox row was delivered, and logs any outbox notice still undelivered after `STUCK_MS`.
//!
//! Exactly once: the outbox id and the agent's delivery key derive from the wake id, so a retry, a crash between
//! delivery and bookkeeping, or a restart can never deliver a wake twice.
use crate::{
    actions::Actions,
    clock::ClockHandle,
    delivery::{DeliveryGate, DeliveryPermit},
};
use anyhow::{bail, ensure, Context, Result};
use chrono::{NaiveDate, NaiveDateTime, NaiveTime};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, Mutex},
};

/// The note's bounds (the tool schema's `message`).
pub const MESSAGE_CHARS: usize = 2000;
/// Furthest ahead a wake may be set.
pub const MAX_AHEAD_MS: u64 = 30 * 24 * 60 * 60_000;
/// The heartbeat's longest sleep: due wakes, CLI-written wakes and failed deliveries are picked up within this.
pub const TICK_MS: u64 = 30_000;
/// A wake fired more than this after its due time says it is late (Heimdall or the agent was off when it came due).
pub const LATE_MS: u64 = 2 * 60_000;
/// An outbox notice undelivered for this long is logged as stuck; it keeps being retried every tick.
pub const STUCK_MS: u64 = 2 * 60_000;
/// Pending wakes allowed at once.
pub const MAX_PENDING: usize = 100;
/// Outbox ids of wakes; `agent::notice::NoticeKind::classify` makes them a `Scheduled` notice.
pub const OUTBOX_PREFIX: &str = "selfwake:";

/// When a wake is due: a local wall-clock time (the next occurrence, or on a given date) or a delay.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum When {
    At { date: Option<NaiveDate>, time: NaiveTime },
    In { ms: u64 },
}
impl When {
    /// `07:00`, `7:05`, `2026-10-23 07:00` or `2026-10-23T07:00`, 24-hour local time.
    pub fn parse_at(text: &str) -> Result<Self> {
        let text = text.trim();
        let bad = || anyhow::anyhow!("at: use 24-hour local time HH:MM, like 07:00, or a date and time like 2026-10-23 07:00");
        let (date, clock) = match text.split_once([' ', 'T']) {
            Some((d, t)) => (Some(NaiveDate::parse_from_str(d.trim(), "%Y-%m-%d").map_err(|_| bad())?), t.trim()),
            None => (None, text),
        };
        let (h, m) = clock.split_once(':').ok_or_else(bad)?;
        ensure!((1..=2).contains(&h.len()) && m.len() == 2, bad());
        let time = NaiveTime::from_hms_opt(h.parse().map_err(|_| bad())?, m.parse().map_err(|_| bad())?, 0).ok_or_else(bad)?;
        Ok(Self::At { date, time })
    }
    fn request(&self) -> Value {
        match self {
            Self::At { date, time } => json!({"at": match date { Some(d) => format!("{d} {}", time.format("%H:%M")), None => time.format("%H:%M").to_string() }}),
            Self::In { ms } => json!({"in_ms": ms}),
        }
    }
}

/// Local wall-clock time. Production is the Mac's own zone (`SystemZone`); tests give a zone with a DST change.
pub trait Zone: Send + Sync {
    /// The local wall-clock time at `ms`.
    fn local(&self, ms: u64) -> NaiveDateTime;
    /// The instants (ms) a local wall-clock time names: one, two when clocks go back, none when they skip it.
    fn instants(&self, local: NaiveDateTime) -> Vec<u64>;
}
pub struct SystemZone;
impl Zone for SystemZone {
    fn local(&self, ms: u64) -> NaiveDateTime {
        use chrono::TimeZone;
        chrono::Local.timestamp_millis_opt(ms as i64).single().map(|t| t.naive_local()).unwrap_or_default()
    }
    fn instants(&self, local: NaiveDateTime) -> Vec<u64> {
        use chrono::{LocalResult, TimeZone};
        let ms = |t: chrono::DateTime<chrono::Local>| t.timestamp_millis().max(0) as u64;
        match chrono::Local.from_local_datetime(&local) {
            LocalResult::Single(t) => vec![ms(t)],
            LocalResult::Ambiguous(a, b) => vec![ms(a), ms(b)],
            LocalResult::None => vec![],
        }
    }
}
/// The instant of a local wall-clock time, DST-agnostic: the earlier one when the time occurs twice; the first
/// minute after the gap when clocks skip it (02:30 on a spring-forward night is 03:00).
fn instant(zone: &dyn Zone, local: NaiveDateTime) -> Option<u64> {
    (0..=180).find_map(|m| zone.instants(local + chrono::Duration::minutes(m)).into_iter().min())
}
/// The due time of `when`, asked at `now`. Past, too far ahead and empty delays are refused with one line.
pub fn due_ms(when: &When, now: u64, zone: &dyn Zone) -> Result<u64> {
    let due = match when {
        When::In { ms } => now.checked_add(*ms).context("in: delay too long")?,
        When::At { date: Some(date), time } => {
            let due = instant(zone, date.and_time(*time)).context("at: that local time does not exist")?;
            ensure!(due > now, "at: {} {} has passed; give a future time", date, time.format("%H:%M"));
            due
        }
        When::At { date: None, time } => {
            // The next occurrence: today if still ahead, else tomorrow (23:50 asking for 00:10 is tomorrow's).
            let today = zone.local(now).date();
            match instant(zone, today.and_time(*time)).filter(|t| *t > now) {
                Some(t) => t,
                None => instant(zone, today.succ_opt().context("date out of range")?.and_time(*time)).context("at: that local time does not exist")?,
            }
        }
    };
    ensure!(due - now <= MAX_AHEAD_MS, "at: more than 30 days ahead; schedule something nearer");
    Ok(due)
}
/// `07:00`, `07:00 tomorrow` or `07:00 Fri 23 Oct`, as seen at `now`.
pub fn label(due: u64, now: u64, zone: &dyn Zone) -> String {
    let (d, n) = (zone.local(due), zone.local(now));
    let hm = d.format("%H:%M").to_string();
    match (d.date() - n.date()).num_days() {
        0 => hm,
        1 => format!("{hm} tomorrow"),
        -1 => format!("{hm} yesterday"),
        _ => format!("{hm} {}", d.format("%a %-d %b")),
    }
}
/// `in 5 h 8 min`, `in 3 min`, `in 40 s`.
fn span(ms: u64) -> String {
    let s = ms.div_ceil(1000);
    if s < 60 {
        return format!("{s} s");
    }
    let m = s.div_ceil(60);
    match (m / 60 / 24, m / 60 % 24, m % 60) {
        (0, 0, m) => format!("{m} min"),
        (0, h, 0) => format!("{h} h"),
        (0, h, m) => format!("{h} h {m} min"),
        (d, h, _) => format!("{d} d {h} h"),
    }
}

/// Who set a wake: the agent's own tool, or a local agent through the command line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Agent,
    Cli,
}
impl Source {
    fn as_str(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Cli => "cli",
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Wake {
    pub id: String,
    pub message: String,
    pub source: String,
    pub created_ms: u64,
    pub due_ms: u64,
    pub state: String,
}
/// A heartbeat pass's outcome, for logs and tests.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Pass {
    /// Wakes claimed in this pass (due now), with whether they were late.
    pub fired: Vec<(String, bool)>,
    /// Wakes whose notice the agent durably took in this pass.
    pub delivered: Vec<String>,
    /// Outbox notices first found stuck in this pass (undelivered for `STUCK_MS`).
    pub stuck: Vec<String>,
    /// The outbox drain's failure, if any (retried next pass).
    pub error: Option<String>,
}
/// A periodic check another part registers on the heartbeat (`add_check`); it may enqueue outbox notices, which
/// the same pass then delivers.
pub type Check = Arc<dyn Fn(&DeliveryPermit) -> Result<()> + Send + Sync>;

pub struct Wakes {
    db: Mutex<Connection>,
    clock: ClockHandle,
    zone: Arc<dyn Zone>,
    changed: tokio::sync::Notify,
    checks: Mutex<Vec<(String, Check)>>,
    /// Undelivered outbox ids: when first seen and whether logged as stuck.
    seen: Mutex<HashMap<String, (u64, bool)>>,
    /// The last logged pass failure; the same failure is not logged every tick.
    last_error: Mutex<Option<String>>,
    /// The longest sleep (`TICK_MS`; integration tests shorten it).
    tick_ms: std::sync::atomic::AtomicU64,
}
fn outbox_id(id: &str) -> String {
    format!("{OUTBOX_PREFIX}{id}")
}
impl Wakes {
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_with(path, crate::clock::system(), Arc::new(SystemZone))
    }
    pub fn open_with(path: &Path, clock: ClockHandle, zone: Arc<dyn Zone>) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let db = Connection::open(path)?;
        db.busy_timeout(std::time::Duration::from_secs(5))?;
        db.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
             CREATE TABLE IF NOT EXISTS wakes(id TEXT PRIMARY KEY, operation TEXT UNIQUE NOT NULL, request TEXT NOT NULL,
               message TEXT NOT NULL, source TEXT NOT NULL, created_ms INTEGER NOT NULL, due_ms INTEGER NOT NULL,
               state TEXT NOT NULL, sent_ms INTEGER, late INTEGER NOT NULL DEFAULT 0, done_ms INTEGER);
             CREATE INDEX IF NOT EXISTS wakes_due ON wakes(state, due_ms);",
        )?;
        Ok(Self {
            db: Mutex::new(db),
            clock,
            zone,
            changed: tokio::sync::Notify::new(),
            checks: Mutex::new(vec![]),
            seen: Mutex::new(HashMap::new()),
            last_error: Mutex::new(None),
            tick_ms: std::sync::atomic::AtomicU64::new(TICK_MS),
        })
    }
    fn now(&self) -> u64 {
        self.clock.now_ms()
    }
    /// Schedule a wake once per `operation`: a replay returns the same wake; the same operation with another
    /// request is a `REPLAY_CONFLICT`. Returns the wake and the one-line confirmation.
    pub fn schedule(&self, operation: &str, when: &When, message: &str, source: Source) -> Result<(Wake, String)> {
        let message = message.trim();
        ensure!(!message.is_empty(), "message: write the note to yourself");
        ensure!(message.chars().count() <= MESSAGE_CHARS, "message: at most {MESSAGE_CHARS} characters");
        let mut request = when.request();
        request["message"] = json!(message);
        request["source"] = json!(source.as_str());
        let request = request.to_string();
        let now = self.now();
        let existing = self.by_operation(operation)?;
        let wake = match existing {
            Some((wake, old)) => {
                ensure!(old == request, "REPLAY_CONFLICT: operation identity was reused for a different wake");
                wake
            }
            None => {
                let due = due_ms(when, now, self.zone.as_ref())?;
                let db = self.db.lock().unwrap();
                let pending: i64 = db.query_row("SELECT count(*) FROM wakes WHERE state='pending'", [], |r| r.get(0))?;
                ensure!((pending as usize) < MAX_PENDING, "too many pending wakes ({MAX_PENDING}); cancel some with cancel_wake");
                for _ in 0..4 {
                    let id = format!("wake-{}", &uuid::Uuid::new_v4().simple().to_string()[..6]);
                    // Another writer (the command line) may hold the same operation: one row wins, both read it back.
                    match db.execute(
                        "INSERT INTO wakes(id,operation,request,message,source,created_ms,due_ms,state) VALUES(?1,?2,?3,?4,?5,?6,?7,'pending')
                         ON CONFLICT(operation) DO NOTHING",
                        params![id, operation, request, message, source.as_str(), now as i64, due as i64],
                    ) {
                        Err(rusqlite::Error::SqliteFailure(e, _)) if e.code == rusqlite::ErrorCode::ConstraintViolation => continue,
                        r => {
                            r?;
                            break;
                        }
                    }
                }
                drop(db);
                let (wake, old) = self.by_operation(operation)?.context("wake was not saved")?;
                ensure!(old == request, "REPLAY_CONFLICT: operation identity was reused for a different wake");
                // The app's log (the command line prints its own one line).
                if source == Source::Agent {
                    eprintln!("Scheduled wake {} set for {}.", wake.id, label(wake.due_ms, now, self.zone.as_ref()));
                }
                self.changed.notify_one();
                wake
            }
        };
        let line = match wake.state.as_str() {
            "pending" => format!(
                "Wake {} set for {} (in {}).",
                wake.id,
                label(wake.due_ms, now, self.zone.as_ref()),
                span(wake.due_ms.saturating_sub(now))
            ),
            "cancelled" => format!("Wake {} was cancelled.", wake.id),
            _ => format!("Wake {} already fired at {}.", wake.id, label(wake.due_ms, now, self.zone.as_ref())),
        };
        Ok((wake, line))
    }
    fn by_operation(&self, operation: &str) -> Result<Option<(Wake, String)>> {
        Ok(self
            .db
            .lock()
            .unwrap()
            .query_row(
                "SELECT id,message,source,created_ms,due_ms,state,request FROM wakes WHERE operation=?",
                [operation],
                |r| Ok((Self::row(r)?, r.get::<_, String>(6)?)),
            )
            .optional()?)
    }
    fn row(r: &rusqlite::Row) -> rusqlite::Result<Wake> {
        Ok(Wake {
            id: r.get(0)?,
            message: r.get(1)?,
            source: r.get(2)?,
            created_ms: r.get::<_, i64>(3)? as u64,
            due_ms: r.get::<_, i64>(4)? as u64,
            state: r.get(5)?,
        })
    }
    fn select(&self, filter: &str, values: impl rusqlite::Params) -> Result<Vec<Wake>> {
        let db = self.db.lock().unwrap();
        let mut q = db.prepare(&format!("SELECT id,message,source,created_ms,due_ms,state FROM wakes WHERE {filter} ORDER BY due_ms,rowid"))?;
        let rows = q.query_map(values, Self::row)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
    /// Pending wakes, soonest first.
    pub fn pending(&self) -> Result<Vec<Wake>> {
        self.select("state='pending'", [])
    }
    /// `list_wakes`: one line each, or that there are none.
    pub fn list_line(&self) -> Result<String> {
        let now = self.now();
        let wakes = self.pending()?;
        if wakes.is_empty() {
            return Ok("No wakes scheduled.".into());
        }
        Ok(wakes
            .iter()
            .map(|w| {
                let first = w.message.lines().next().unwrap_or("");
                let mut note: String = first.chars().take(120).collect();
                if note.len() < w.message.len() {
                    note.push('…');
                }
                let from = if w.source == "cli" { " (from a local agent)" } else { "" };
                format!("{} · {}{from} · {note}", w.id, label(w.due_ms, now, self.zone.as_ref()))
            })
            .collect::<Vec<_>>()
            .join("\n"))
    }
    /// `cancel_wake`: a pending wake is cancelled; anything else says what became of it. Repeating is harmless.
    pub fn cancel(&self, id: &str) -> Result<String> {
        let id = id.trim();
        let changed = self.db.lock().unwrap().execute("UPDATE wakes SET state='cancelled',done_ms=?2 WHERE id=?1 AND state='pending'", params![id, self.now() as i64])?;
        let wake = self.select("id=?1", [id])?.into_iter().next().with_context(|| format!("WAKE_NOT_FOUND: no wake {id}; list_wakes shows the pending ones"))?;
        if changed == 1 {
            eprintln!("Scheduled wake {id} cancelled.");
            self.changed.notify_one();
        }
        let at = label(wake.due_ms, self.now(), self.zone.as_ref());
        Ok(match wake.state.as_str() {
            "cancelled" => format!("Wake {id} for {at} cancelled."),
            _ => format!("Wake {id} already fired at {at}; nothing to cancel."),
        })
    }
    /// The notice the agent receives for a fired wake.
    fn text(&self, w: &Wake, late: bool, now: u64) -> String {
        let z = self.zone.as_ref();
        let set = label(w.created_ms, w.due_ms, z);
        let due = label(w.due_ms, now, z);
        let head = if w.source == "cli" {
            format!("Scheduled message {} from a local agent (not User), set at {set} for {due}", w.id)
        } else {
            format!("Scheduled wake {}: you set it at {set} for {due}", w.id)
        };
        let late = if late { format!(". Late: it came due while Heimdall or the agent was off; it is now {}", z.local(now).format("%H:%M")) } else { String::new() };
        let tail = if w.source == "cli" { "Its message" } else { "Your note, your own instruction to act on now" };
        format!("{head}{late}.\n{tail}: {}", w.message)
    }
    /// Register a check run on every heartbeat pass, before the outbox drain.
    pub fn add_check(&self, name: &str, check: Check) {
        self.checks.lock().unwrap().push((name.into(), check));
    }
    /// One heartbeat pass (see the module comment). Blocking: the outbox drain waits for the agent's acknowledgement.
    pub fn pass(&self, actions: &Actions, permit: &DeliveryPermit) -> Result<Pass> {
        permit.check()?;
        let now = self.now();
        let mut pass = Pass::default();
        // 1. Claim due wakes. The claim is the state change itself, so a cancel racing it either wins or finds it fired.
        for w in self.select("state='pending' AND due_ms<=?1", [now as i64])? {
            let late = now.saturating_sub(w.due_ms) > LATE_MS;
            let claimed = permit.with(|| Ok(self.db.lock().unwrap().execute("UPDATE wakes SET state='sent',sent_ms=?2,late=?3 WHERE id=?1 AND state='pending'", params![w.id, now as i64, late])?))?;
            if claimed == 1 {
                eprintln!(
                    "Scheduled wake {} fired: due {}{}.",
                    w.id,
                    crate::clock::local_hm(w.due_ms),
                    if late { format!(", late (now {})", crate::clock::local_hm(now)) } else { String::new() }
                );
                pass.fired.push((w.id.clone(), late));
            }
        }
        // 2. Every sent wake has its outbox row in this epoch.
        let sent = self.select("state='sent'", [])?;
        for w in &sent {
            let late: bool = self.db.lock().unwrap().query_row("SELECT late FROM wakes WHERE id=?", [&w.id], |r| r.get(0))?;
            let id = outbox_id(&w.id);
            match actions.outbox_state(&id)? {
                None => actions.enqueue(&id, &self.text(w, late, now), permit)?,
                // An Agent Off/On dropped it (or is about to) before the agent took it: the same id goes again in
                // this epoch, said late if it now is. The agent's key is the same, so it can never arrive twice.
                Some((delivered, epoch)) if delivered == -1 || (delivered == 0 && epoch != permit.generation() as i64) => {
                    let late = late || now.saturating_sub(w.due_ms) > LATE_MS;
                    actions.outbox_rearm(&id, &self.text(w, late, now), permit)?;
                    eprintln!("Scheduled wake {} re-sent after an agent restart dropped it.", w.id);
                }
                _ => {}
            }
        }
        // 3. Registered checks, then the drain: every undelivered outbox notice is retried.
        let checks = self.checks.lock().unwrap().clone();
        let mut errors = vec![];
        for (name, check) in checks {
            if let Err(e) = check(permit) {
                errors.push(format!("{name}: {e:#}"));
            }
        }
        if let Err(e) = actions.drain(permit) {
            errors.push(format!("outbox: {e:#}"));
        }
        // 4. Bookkeeping: delivered wakes, stuck notices.
        for w in &sent {
            if matches!(actions.outbox_state(&outbox_id(&w.id))?, Some((1 | -2, _))) {
                let done = self.db.lock().unwrap().execute("UPDATE wakes SET state='delivered',done_ms=?2 WHERE id=?1 AND state='sent'", params![w.id, now as i64])?;
                if done == 1 {
                    eprintln!("Scheduled wake {} delivered to the agent.", w.id);
                    pass.delivered.push(w.id.clone());
                }
            }
        }
        let undelivered = actions.outbox_undelivered()?;
        let mut seen = self.seen.lock().unwrap();
        seen.retain(|id, (first, stuck)| {
            let still = undelivered.contains(id);
            if !still && *stuck {
                eprintln!("Heartbeat: notice {id} delivered after {}.", span(now.saturating_sub(*first)));
            }
            still
        });
        for id in undelivered {
            let (first, stuck) = seen.entry(id.clone()).or_insert((now, false));
            if !*stuck && now.saturating_sub(*first) >= STUCK_MS {
                *stuck = true;
                eprintln!("Heartbeat: notice {id} undelivered for {}; retrying every {} s.", span(now - *first), TICK_MS / 1000);
                pass.stuck.push(id);
            }
        }
        drop(seen);
        let error = (!errors.is_empty()).then(|| errors.join("; "));
        let mut last = self.last_error.lock().unwrap();
        if error.is_some() && *last != error {
            eprintln!("Heartbeat pass failed (retried every {} s): {}", TICK_MS / 1000, error.as_deref().unwrap_or(""));
        }
        *last = error.clone();
        pass.error = error;
        Ok(pass)
    }
    /// Shorten the heartbeat's longest sleep (integration tests: a command-line wake is picked up within it).
    pub fn set_tick_ms(&self, ms: u64) {
        self.tick_ms.store(ms.max(50), std::sync::atomic::Ordering::Relaxed);
        self.changed.notify_one();
    }
    /// How long the heartbeat sleeps: until the next future wake, at most `TICK_MS`.
    pub fn next_wait_ms(&self) -> u64 {
        let tick = self.tick_ms.load(std::sync::atomic::Ordering::Relaxed);
        let now = self.now();
        let next: Option<i64> = self
            .db
            .lock()
            .unwrap()
            .query_row("SELECT min(due_ms) FROM wakes WHERE state='pending' AND due_ms>?", [now as i64], |r| r.get(0))
            .ok()
            .flatten();
        next.map_or(tick, |due| (due as u64 - now).min(tick))
    }
    /// The heartbeat: a pass now, then at each due time or every `TICK_MS`, and at once after a schedule or cancel.
    /// Passes run only while the agent is on (a permit exists); a wake due while it is off fires, late, once it is on.
    pub async fn heartbeat(self: Arc<Self>, actions: Arc<Actions>, gate: Arc<DeliveryGate>, mut stop: tokio::sync::watch::Receiver<bool>) {
        loop {
            if *stop.borrow() {
                return;
            }
            if let Ok(permit) = gate.permit() {
                let (this, actions) = (self.clone(), actions.clone());
                let pass = tokio::task::spawn_blocking(move || this.pass(&actions, &permit));
                // A stop never waits for a pass blocked on the agent's acknowledgement (bounded by its own timeout).
                let result = tokio::select! {
                    biased;
                    changed = stop.changed() => if changed.is_err() || *stop.borrow() { return } else { continue },
                    result = pass => result,
                };
                if let Ok(Err(e)) = result {
                    let line = format!("{e:#}");
                    let mut last = self.last_error.lock().unwrap();
                    if last.as_deref() != Some(line.as_str()) {
                        eprintln!("Heartbeat pass failed (retried every {} s): {line}", TICK_MS / 1000);
                        *last = Some(line);
                    }
                }
            }
            let wait = std::time::Duration::from_millis(self.next_wait_ms().max(50));
            tokio::select! {
                biased;
                changed = stop.changed() => if changed.is_err() || *stop.borrow() { return; },
                _ = self.changed.notified() => {},
                _ = tokio::time::sleep(wait) => {},
            }
        }
    }
}

/// The agent's three tools. `operation` is the tool call's replay identity.
pub fn tool(wakes: &Wakes, name: &str, args: &Value, operation: &str) -> Result<String> {
    match name {
        "schedule_wake" => {
            let when = match (args["at"].as_str(), args["in_minutes"].as_u64()) {
                (Some(at), None) => When::parse_at(at)?,
                (None, Some(m)) => When::In { ms: m * 60_000 },
                _ => bail!("give at or in_minutes"),
            };
            Ok(wakes.schedule(operation, &when, args["message"].as_str().unwrap_or(""), Source::Agent)?.1)
        }
        "list_wakes" => wakes.list_line(),
        "cancel_wake" => wakes.cancel(args["id"].as_str().unwrap_or("")),
        _ => bail!("unknown wake tool"),
    }
}

/// `Heimdall --schedule-wake [--data-dir DIR] (--in SECONDS | --at HH:MM) --message TEXT [--id OPERATION]`: writes
/// one wake into `DIR/agent/wakes.sqlite` (safe while the app runs) and returns the one-line confirmation. The
/// running app fires it within one heartbeat of its due time; a stopped one at its next start. `--id` makes a retry
/// of the same command idempotent.
pub fn cli(args: &[String], default_data: impl FnOnce() -> Result<std::path::PathBuf>) -> Result<String> {
    let usage = "usage: Heimdall --schedule-wake [--data-dir DIR] (--in SECONDS | --at HH:MM) --message TEXT [--id OPERATION]";
    let (mut data, mut when, mut message, mut op) = (None, None, None, None);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut value = || it.next().cloned().with_context(|| format!("{a} needs a value; {usage}"));
        match a.as_str() {
            "--data-dir" => data = Some(std::path::PathBuf::from(value()?)),
            "--in" => {
                let s: u64 = value()?.parse().map_err(|_| anyhow::anyhow!("--in takes whole seconds"))?;
                ensure!(when.is_none(), "give --in or --at, not both");
                when = Some(When::In { ms: s.saturating_mul(1000) });
            }
            "--at" => {
                ensure!(when.is_none(), "give --in or --at, not both");
                when = Some(When::parse_at(&value()?)?);
            }
            "--message" => message = Some(value()?),
            "--id" => op = Some(value()?),
            other => bail!("unknown argument {other}; {usage}"),
        }
    }
    let when = when.with_context(|| format!("give --in or --at; {usage}"))?;
    let message = message.with_context(|| format!("give --message; {usage}"))?;
    let data = match data {
        Some(d) => d,
        None => default_data()?,
    };
    let wakes = Wakes::open(&data.join("agent/wakes.sqlite"))?;
    let op = format!("cli:{}", op.unwrap_or_else(|| uuid::Uuid::new_v4().to_string()));
    Ok(wakes.schedule(&op, &when, &message, Source::Cli)?.1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        actions::{ActionSink, Notice, Question},
        clock::SimClock,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A zone at UTC+1 that moves to UTC+2 at `switch` (spring forward: 02:00–03:00 local is skipped), or from UTC+2
    /// to UTC+1 (fall back: 02:00–03:00 local happens twice).
    struct DstZone {
        switch: u64,
        before: i64,
        after: i64,
    }
    impl DstZone {
        fn offset(&self, ms: u64) -> i64 {
            if ms < self.switch { self.before } else { self.after }
        }
    }
    impl Zone for DstZone {
        fn local(&self, ms: u64) -> NaiveDateTime {
            chrono::DateTime::from_timestamp_millis(ms as i64 + self.offset(ms) * 3_600_000).unwrap().naive_utc()
        }
        fn instants(&self, local: NaiveDateTime) -> Vec<u64> {
            let naive = local.and_utc().timestamp_millis();
            let mut out: Vec<u64> = [self.before, self.after]
                .iter()
                .map(|o| (naive - o * 3_600_000) as u64)
                .filter(|ms| self.local(*ms) == local)
                .collect();
            out.sort();
            out.dedup();
            out
        }
    }
    fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> u64 {
        NaiveDate::from_ymd_opt(y, mo, d).unwrap().and_hms_opt(h, mi, 0).unwrap().and_utc().timestamp_millis() as u64
    }
    fn plain(offset: i64) -> DstZone {
        DstZone { switch: u64::MAX, before: offset, after: offset }
    }
    fn hm(h: u32, m: u32) -> When {
        When::At { date: None, time: NaiveTime::from_hms_opt(h, m, 0).unwrap() }
    }

    #[test]
    fn at_is_the_next_local_occurrence_with_midnight_rollover() {
        let z = plain(2);
        // 23:50 local (21:50 UTC) asking for 00:10: tomorrow's.
        let now = utc(2026, 10, 21, 21, 50);
        assert_eq!(due_ms(&hm(0, 10), now, &z).unwrap(), utc(2026, 10, 21, 22, 10));
        assert_eq!(label(utc(2026, 10, 21, 22, 10), now, &z), "00:10 tomorrow");
        // 01:52 asking for 07:00: today's.
        let now = utc(2026, 10, 20, 23, 52);
        assert_eq!(due_ms(&hm(7, 0), now, &z).unwrap(), utc(2026, 10, 21, 5, 0));
        assert_eq!(label(utc(2026, 10, 21, 5, 0), now, &z), "07:00");
        // The current minute already passed: tomorrow (never a wake in the past).
        let now = utc(2026, 10, 21, 5, 0) + 20_000;
        assert_eq!(due_ms(&hm(7, 0), now, &z).unwrap(), utc(2026, 10, 22, 5, 0));
        // A date: that day; a past one is refused; more than 30 days ahead is refused.
        let when = When::parse_at("2026-10-23 07:00").unwrap();
        assert_eq!(due_ms(&when, now, &z).unwrap(), utc(2026, 10, 23, 5, 0));
        assert_eq!(label(utc(2026, 10, 23, 5, 0), now, &z), "07:00 Fri 23 Oct");
        assert!(due_ms(&When::parse_at("2026-10-20T07:00").unwrap(), now, &z).unwrap_err().to_string().contains("has passed"));
        assert!(due_ms(&When::parse_at("2026-12-25 07:00").unwrap(), now, &z).unwrap_err().to_string().contains("30 days"));
        assert!(due_ms(&When::In { ms: MAX_AHEAD_MS + 1 }, now, &z).is_err());
        assert_eq!(due_ms(&When::In { ms: 90_000 }, now, &z).unwrap(), now + 90_000);
    }

    #[test]
    fn at_parsing_takes_24_hour_local_times_and_refuses_the_rest() {
        assert_eq!(When::parse_at("7:05").unwrap(), hm(7, 5));
        assert_eq!(When::parse_at(" 07:00 ").unwrap(), hm(7, 0));
        for bad in ["24:00", "7", "7pm", "07:60", "07:5", "tomorrow 07:00", "2026-13-01 07:00", "", "-1:00"] {
            assert!(When::parse_at(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn at_is_dst_agnostic_local_wall_clock_time() {
        // Spring forward at 01:00 UTC on 29 Mar 2026 (02:00 CET → 03:00 CEST).
        let spring = DstZone { switch: utc(2026, 3, 29, 1, 0), before: 1, after: 2 };
        let evening = utc(2026, 3, 28, 21, 0); // 22:00 CET the night before
        // 07:00 the next morning is 07:00 CEST (05:00 UTC), not 06:00 UTC: the offset of the due day counts.
        assert_eq!(due_ms(&hm(7, 0), evening, &spring).unwrap(), utc(2026, 3, 29, 5, 0));
        // 02:30 does not exist that night: the first minute after the gap, 03:00 CEST.
        assert_eq!(due_ms(&hm(2, 30), evening, &spring).unwrap(), utc(2026, 3, 29, 1, 0));
        assert_eq!(spring.local(utc(2026, 3, 29, 1, 0)).format("%H:%M").to_string(), "03:00");
        // Fall back at 01:00 UTC on 25 Oct 2026 (03:00 CEST → 02:00 CET): 02:30 happens twice; the first one.
        let fall = DstZone { switch: utc(2026, 10, 25, 1, 0), before: 2, after: 1 };
        let evening = utc(2026, 10, 24, 20, 0);
        assert_eq!(due_ms(&hm(2, 30), evening, &fall).unwrap(), utc(2026, 10, 25, 0, 30));
        assert_eq!(due_ms(&hm(7, 0), evening, &fall).unwrap(), utc(2026, 10, 25, 6, 0));
        // The real zone resolves an ordinary time to one instant.
        let now = crate::clock::now_ms();
        let due = due_ms(&hm(12, 0), now, &SystemZone).unwrap();
        // A fall-back day is 25 hours long (24 Oct 2026, the day before the switch, noon was 24 h 20 min away).
        assert!(due > now && due - now <= 25 * 3_600_000);
        assert_eq!(SystemZone.local(due).format("%H:%M").to_string(), "12:00");
    }

    /// The agent side of the outbox: like `Tools::notice` → `Bridge::notice`, durable and deduplicated by id; it can
    /// be made to fail the next `fail` deliveries.
    #[derive(Default)]
    struct Agent {
        got: Mutex<Vec<(String, String)>>,
        known: Mutex<std::collections::HashSet<String>>,
        fail: AtomicUsize,
    }
    impl ActionSink for Agent {
        fn notice(&self, _: &Notice, _: &DeliveryPermit) -> Result<()> { Ok(()) }
        fn question(&self, _: &Question, _: &DeliveryPermit) -> Result<()> { Ok(()) }
        fn question_closed(&self, _: &str, _: &str, _: &DeliveryPermit) -> Result<()> { Ok(()) }
        fn deliver(&self, id: &str, text: &str, _: &DeliveryPermit) -> Result<()> {
            if self.fail.load(Ordering::SeqCst) > 0 {
                self.fail.fetch_sub(1, Ordering::SeqCst);
                bail!("Agent notice was not durably acknowledged");
            }
            if self.known.lock().unwrap().insert(id.into()) {
                self.got.lock().unwrap().push((id.into(), text.into()));
            }
            Ok(())
        }
        fn reload(&self, _: &str, _: &DeliveryPermit) -> Result<()> { Ok(()) }
    }
    struct App {
        wakes: Wakes,
        actions: Actions,
    }
    fn app(dir: &Path, clock: &SimClock, agent: &Arc<Agent>) -> App {
        let actions = Actions::open(&dir.join("agent/mcp-actions.sqlite")).unwrap();
        actions.attach(agent.clone());
        App { wakes: Wakes::open_with(&dir.join("agent/wakes.sqlite"), Arc::new(clock.clone()), Arc::new(plain(2))).unwrap(), actions }
    }
    fn gate() -> (Arc<DeliveryGate>, DeliveryPermit) {
        let g = Arc::new(DeliveryGate::new(true));
        let p = g.permit().unwrap();
        (g, p)
    }

    #[test]
    fn a_wake_fires_once_at_its_time_and_says_whose_note_it_is() {
        let d = tempfile::tempdir().unwrap();
        let clock = SimClock::at(utc(2026, 10, 20, 23, 52)); // 01:52 local
        let agent = Arc::new(Agent::default());
        let a = app(d.path(), &clock, &agent);
        let (_g, p) = gate();
        let (w, line) = a.wakes.schedule("op-1", &hm(7, 0), "Check whether the TTS research task finished", Source::Agent).unwrap();
        assert_eq!(line, format!("Wake {} set for 07:00 (in 5 h 8 min).", w.id));
        assert_eq!(a.wakes.next_wait_ms(), TICK_MS);
        // Not due: nothing.
        clock.advance(5 * 3_600_000);
        assert_eq!(a.wakes.pass(&a.actions, &p).unwrap(), Pass::default());
        // 07:00:10: fires on time, once.
        clock.set(utc(2026, 10, 21, 5, 0) + 10_000);
        let pass = a.wakes.pass(&a.actions, &p).unwrap();
        assert_eq!(pass.fired, vec![(w.id.clone(), false)]);
        assert_eq!(pass.delivered, vec![w.id.clone()]);
        let got = agent.got.lock().unwrap().clone();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, format!("selfwake:{}", w.id));
        assert_eq!(got[0].1, format!("Scheduled wake {}: you set it at 01:52 for 07:00.\nYour note, your own instruction to act on now: Check whether the TTS research task finished", w.id));
        assert_eq!(crate::notice::NoticeKind::classify(&got[0].0, &got[0].1), crate::notice::NoticeKind::Scheduled);
        for _ in 0..3 {
            clock.advance(TICK_MS);
            assert_eq!(a.wakes.pass(&a.actions, &p).unwrap(), Pass::default());
        }
        assert_eq!(agent.got.lock().unwrap().len(), 1);
        assert_eq!(a.wakes.list_line().unwrap(), "No wakes scheduled.");
    }

    #[test]
    fn replaying_the_same_operation_returns_the_same_wake_and_a_different_one_conflicts() {
        let d = tempfile::tempdir().unwrap();
        let clock = SimClock::at(utc(2026, 10, 20, 23, 52));
        let agent = Arc::new(Agent::default());
        let a = app(d.path(), &clock, &agent);
        let (w, _) = a.wakes.schedule("op-1", &When::In { ms: 3_600_000 }, "note", Source::Agent).unwrap();
        clock.advance(60_000);
        let (again, line) = a.wakes.schedule("op-1", &When::In { ms: 3_600_000 }, "note", Source::Agent).unwrap();
        assert_eq!(again, w, "same wake, same due time, not a second one");
        assert_eq!(line, format!("Wake {} set for 02:52 (in 59 min).", w.id));
        assert!(a.wakes.schedule("op-1", &When::In { ms: 60_000 }, "note", Source::Agent).unwrap_err().to_string().starts_with("REPLAY_CONFLICT"));
        assert_eq!(a.wakes.pending().unwrap().len(), 1);
        // Through the tool entry point: the same.
        let args = json!({"in_minutes": 60, "message": "note"});
        let first = tool(&a.wakes, "schedule_wake", &args, "op-2").unwrap();
        assert_eq!(tool(&a.wakes, "schedule_wake", &args, "op-2").unwrap(), first);
        assert_eq!(a.wakes.pending().unwrap().len(), 2);
        assert!(tool(&a.wakes, "schedule_wake", &json!({"message":"x"}), "op-3").is_err());
        assert!(tool(&a.wakes, "schedule_wake", &json!({"message":"x","at":"25:00"}), "op-3").is_err());
    }

    #[test]
    fn cancel_stops_a_pending_wake_and_says_what_became_of_others() {
        let d = tempfile::tempdir().unwrap();
        let clock = SimClock::at(utc(2026, 10, 20, 23, 52));
        let agent = Arc::new(Agent::default());
        let a = app(d.path(), &clock, &agent);
        let (_g, p) = gate();
        let (w1, _) = a.wakes.schedule("a", &When::In { ms: 60_000 }, "first note", Source::Agent).unwrap();
        let (w2, _) = a.wakes.schedule("b", &When::In { ms: 120_000 }, "second note\nwith a second line", Source::Cli).unwrap();
        assert_eq!(a.wakes.list_line().unwrap(), format!("{} · 01:53 · first note\n{} · 01:54 (from a local agent) · second note…", w1.id, w2.id));
        assert_eq!(a.wakes.cancel(&w1.id).unwrap(), format!("Wake {} for 01:53 cancelled.", w1.id));
        assert_eq!(a.wakes.cancel(&w1.id).unwrap(), format!("Wake {} for 01:53 cancelled.", w1.id), "repeat is harmless");
        assert!(a.wakes.cancel("wake-nope").unwrap_err().to_string().starts_with("WAKE_NOT_FOUND"));
        clock.advance(10 * 60_000);
        let pass = a.wakes.pass(&a.actions, &p).unwrap();
        assert_eq!(pass.fired.iter().map(|f| f.0.clone()).collect::<Vec<_>>(), vec![w2.id.clone()]);
        assert_eq!(a.wakes.cancel(&w2.id).unwrap(), format!("Wake {} already fired at 01:54; nothing to cancel.", w2.id));
        let got = agent.got.lock().unwrap().clone();
        assert_eq!(got.len(), 1, "the cancelled wake never fires");
        assert!(got[0].1.starts_with(&format!("Scheduled message {} from a local agent (not User), set at 01:52 for 01:54. Late: it came due", w2.id)), "{}", got[0].1);
    }

    #[test]
    fn a_wake_due_while_heimdall_was_down_arrives_once_at_startup_marked_late() {
        let d = tempfile::tempdir().unwrap();
        let clock = SimClock::at(utc(2026, 10, 20, 23, 52));
        let agent = Arc::new(Agent::default());
        let (_g, p) = gate();
        let id = {
            let a = app(d.path(), &clock, &agent);
            a.wakes.schedule("op", &hm(7, 0), "Tell User the build is ready", Source::Agent).unwrap().0.id
        }; // Heimdall quits.
        clock.set(utc(2026, 10, 21, 6, 13)); // starts again at 08:13
        let a = app(d.path(), &clock, &agent);
        let pass = a.wakes.pass(&a.actions, &p).unwrap();
        assert_eq!(pass.fired, vec![(id.clone(), true)]);
        let got = agent.got.lock().unwrap().clone();
        assert_eq!(got.len(), 1);
        assert!(got[0].1.contains("you set it at 01:52 for 07:00. Late: it came due while Heimdall or the agent was off; it is now"), "{}", got[0].1);
        drop(a);
        // Another restart: never again.
        for _ in 0..2 {
            let a = app(d.path(), &clock, &agent);
            assert_eq!(a.wakes.pass(&a.actions, &p).unwrap(), Pass::default());
        }
        assert_eq!(agent.got.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_crash_after_delivery_before_bookkeeping_never_delivers_twice() {
        let d = tempfile::tempdir().unwrap();
        let clock = SimClock::at(utc(2026, 10, 20, 23, 52));
        let agent = Arc::new(Agent::default());
        let (_g, p) = gate();
        let a = app(d.path(), &clock, &agent);
        let (w, _) = a.wakes.schedule("op", &When::In { ms: 1_000 }, "note", Source::Agent).unwrap();
        clock.advance(5_000);
        // The crash: the agent took the notice and the outbox says so, but the wake row still says sent.
        a.wakes.db.lock().unwrap().execute("UPDATE wakes SET state='sent',sent_ms=0 WHERE id=?", [&w.id]).unwrap();
        a.actions.enqueue(&outbox_id(&w.id), "Scheduled wake …", &p).unwrap();
        a.actions.drain(&p).unwrap();
        drop(a);
        let a = app(d.path(), &clock, &agent);
        let pass = a.wakes.pass(&a.actions, &p).unwrap();
        assert_eq!(pass.delivered, vec![w.id.clone()]);
        assert_eq!(agent.got.lock().unwrap().len(), 1);
        // A crash after the claim, before the outbox row: the next pass writes it, once.
        let (w2, _) = a.wakes.schedule("op2", &When::In { ms: 1_000 }, "note 2", Source::Agent).unwrap();
        clock.advance(5_000);
        a.wakes.db.lock().unwrap().execute("UPDATE wakes SET state='sent',sent_ms=0 WHERE id=?", [&w2.id]).unwrap();
        drop(a);
        let a = app(d.path(), &clock, &agent);
        assert_eq!(a.wakes.pass(&a.actions, &p).unwrap().delivered, vec![w2.id.clone()]);
        assert_eq!(a.wakes.pass(&a.actions, &p).unwrap(), Pass::default());
        assert_eq!(agent.got.lock().unwrap().len(), 2);
    }

    #[test]
    fn a_failed_delivery_is_retried_each_pass_and_logged_stuck_after_two_minutes() {
        let d = tempfile::tempdir().unwrap();
        let clock = SimClock::at(utc(2026, 10, 20, 23, 52));
        let agent = Arc::new(Agent::default());
        let (_g, p) = gate();
        let a = app(d.path(), &clock, &agent);
        let (w, _) = a.wakes.schedule("op", &When::In { ms: 1_000 }, "note", Source::Agent).unwrap();
        // Another producer's notice the T3 watcher never drained (its own pass failed): the heartbeat sends it too.
        a.actions.enqueue("terminal:w1", "Task 'TTS research' (w1) completed.", &p).unwrap();
        agent.fail.store(5, Ordering::SeqCst);
        clock.advance(2_000);
        let first = a.wakes.pass(&a.actions, &p).unwrap();
        assert_eq!(first.fired.len(), 1);
        assert!(first.delivered.is_empty() && first.error.as_deref().unwrap().contains("not durably acknowledged"));
        let mut stuck = vec![];
        for _ in 0..4 {
            clock.advance(TICK_MS);
            let pass = a.wakes.pass(&a.actions, &p).unwrap();
            assert!(pass.fired.is_empty(), "claimed once");
            stuck.extend(pass.stuck);
        }
        stuck.sort();
        assert_eq!(stuck, vec![format!("selfwake:{}", w.id), "terminal:w1".to_string()], "both stuck, each logged once");
        clock.advance(TICK_MS);
        let pass = a.wakes.pass(&a.actions, &p).unwrap();
        assert_eq!(pass.delivered, vec![w.id.clone()]);
        assert_eq!(pass.error, None);
        let got: Vec<String> = agent.got.lock().unwrap().iter().map(|g| g.0.clone()).collect();
        assert_eq!(got, vec!["terminal:w1".to_string(), format!("selfwake:{}", w.id)]);
        assert!(a.actions.outbox_undelivered().unwrap().is_empty());
    }

    #[test]
    fn an_agent_off_on_before_delivery_resends_the_same_wake_once() {
        let d = tempfile::tempdir().unwrap();
        let clock = SimClock::at(utc(2026, 10, 20, 23, 52));
        let agent = Arc::new(Agent::default());
        let a = app(d.path(), &clock, &agent);
        let (g, p) = gate();
        let (w, _) = a.wakes.schedule("op", &When::In { ms: 1_000 }, "note", Source::Agent).unwrap();
        agent.fail.store(1, Ordering::SeqCst);
        clock.advance(2_000);
        assert!(a.wakes.pass(&a.actions, &p).unwrap().delivered.is_empty());
        // Agent Off then On: a new epoch. The old row is dropped by the outbox, then re-armed by the heartbeat.
        g.intent(crate::delivery::StopReason::Agent, false);
        g.intent(crate::delivery::StopReason::Agent, true);
        let p2 = g.permit().unwrap();
        assert!(a.wakes.pass(&a.actions, &p).is_err(), "the old epoch's permit is revoked");
        clock.advance(10 * 60_000);
        let pass = a.wakes.pass(&a.actions, &p2).unwrap();
        assert_eq!(pass.delivered, vec![w.id.clone()]);
        let got = agent.got.lock().unwrap().clone();
        assert_eq!(got.len(), 1);
        assert!(got[0].1.contains(". Late: it came due"), "it is late by now: {}", got[0].1);
    }

    #[test]
    fn the_command_line_writes_while_the_app_runs_and_every_wake_fires_once() {
        let d = tempfile::tempdir().unwrap();
        let clock = SimClock::at(crate::clock::now_ms());
        let agent = Arc::new(Agent::default());
        let a = Arc::new(app(d.path(), &clock, &agent));
        let (_g, p) = gate();
        let data = d.path().to_path_buf();
        // Four command-line writers (separate connections, like separate processes) while the app passes.
        let writers: Vec<_> = (0..4)
            .map(|w| {
                let data = data.clone();
                std::thread::spawn(move || {
                    for i in 0..10 {
                        let args: Vec<String> = ["--data-dir", data.to_str().unwrap(), "--in", "0", "--message", &format!("writer {w} message {i}"), "--id", &format!("{w}-{i}")]
                            .iter()
                            .map(|s| s.to_string())
                            .collect();
                        let line = cli(&args, || unreachable!()).unwrap();
                        assert!(line.starts_with("Wake wake-") && line.contains("(in 0 s)"), "{line}");
                        // A retried command with the same --id is the same wake.
                        assert_eq!(cli(&args, || unreachable!()).unwrap().split(' ').nth(1), line.split(' ').nth(1));
                    }
                })
            })
            .collect();
        let app = a.clone();
        let passes = std::thread::spawn(move || {
            for _ in 0..20 {
                app.wakes.pass(&app.actions, &p).unwrap();
            }
            p
        });
        for w in writers {
            w.join().unwrap();
        }
        let p = passes.join().unwrap();
        clock.set(crate::clock::now_ms() + 1_000); // the command line wrote with the real clock: all due now
        a.wakes.pass(&a.actions, &p).unwrap();
        a.wakes.pass(&a.actions, &p).unwrap();
        let got = agent.got.lock().unwrap().clone();
        assert_eq!(got.len(), 40, "every wake once");
        let ids: std::collections::HashSet<_> = got.iter().map(|g| g.0.clone()).collect();
        assert_eq!(ids.len(), 40);
        assert!(got.iter().all(|g| g.1.contains("from a local agent (not User)")));
        // Bad command lines say what is wrong in one line.
        let bad = |args: &[&str]| cli(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>(), || Ok(data.clone())).unwrap_err().to_string();
        assert!(bad(&["--message", "x"]).starts_with("give --in or --at"));
        assert!(bad(&["--in", "5"]).starts_with("give --message"));
        assert!(bad(&["--in", "-5", "--message", "x"]).contains("whole seconds"));
        assert!(bad(&["--in", "5", "--at", "07:00", "--message", "x"]).contains("not both"));
        assert!(bad(&["--at", "7pm", "--message", "x"]).contains("HH:MM"));
        assert!(bad(&["--in", "5", "--message", "  "]).contains("message"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_heartbeat_fires_a_wake_at_its_time_and_stops_on_request() {
        let d = tempfile::tempdir().unwrap();
        let agent = Arc::new(Agent::default());
        let actions = Arc::new(Actions::open(&d.path().join("agent/mcp-actions.sqlite")).unwrap());
        actions.attach(agent.clone());
        let wakes = Arc::new(Wakes::open(&d.path().join("agent/wakes.sqlite")).unwrap());
        let gate = Arc::new(DeliveryGate::new(true));
        let (stop, rx) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(wakes.clone().heartbeat(actions.clone(), gate.clone(), rx));
        // Scheduled after the heartbeat started sleeping its 30 s tick: the schedule wakes it, and it fires at 300 ms.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let started = std::time::Instant::now();
        wakes.schedule("op", &When::In { ms: 300 }, "note", Source::Agent).unwrap();
        while agent.got.lock().unwrap().is_empty() {
            assert!(started.elapsed() < std::time::Duration::from_secs(5), "never fired");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(started.elapsed() >= std::time::Duration::from_millis(300));
        stop.send_replace(true);
        tokio::time::timeout(std::time::Duration::from_secs(2), task).await.unwrap().unwrap();
    }

    /// The agent's side (`Bridge::notice`): after a start, before the user speaks, a T3 update is only context (E4.3)
    /// but a scheduled wake starts a turn; the same wake id twice is one delivery; one that waits behind a turn longer
    /// than the 30-minute context window still starts a turn.
    #[test]
    fn a_scheduled_notice_wakes_the_agent_right_after_a_start_and_only_once() {
        use crate::sim::{fake_pi::Step, Sim, EVENING_MS, MIN};
        let mut sim = Sim::new(EVENING_MS);
        sim.restart(None);
        assert!(sim.bridge.awaiting_owner());
        sim.advance(MIN);
        sim.notice("t3:mcp:w:r:done", "T3 update:\n- 'w' (yours) completed: Worker w run r completed.");
        assert!(sim.notice("selfwake:wake-1a2b3c", "Scheduled wake wake-1a2b3c: you set it at 01:52 for 07:00.\nYour note, your own instruction to act on now: check the build"));
        assert!(!sim.notice("selfwake:wake-1a2b3c", "Scheduled wake wake-1a2b3c: you set it at 01:52 for 07:00.\nYour note, your own instruction to act on now: check the build"));
        sim.settle();
        let carrying = |sim: &Sim, words: &str| sim.wakes().iter().filter(|i| i.text.contains(words)).count();
        assert_eq!(carrying(&sim, "check the build"), 1, "a turn, once");
        assert_eq!(carrying(&sim, "Worker w run r completed"), 0, "the T3 update stays context");
        // Behind a 40-minute turn: still a turn when it settles.
        sim.world.lock().unwrap().replies.push_back(vec![Step::Think(40 * MIN)]);
        sim.say("start the long job");
        sim.advance(MIN);
        sim.notice("selfwake:wake-4d5e6f", "Scheduled wake wake-4d5e6f: you set it at 01:52 for 08:00.\nYour note, your own instruction to act on now: look at the long job");
        sim.advance(45 * MIN);
        sim.settle();
        assert_eq!(carrying(&sim, "look at the long job"), 1);
        assert!(sim.wakes().iter().find(|i| i.text.contains("look at the long job")).unwrap().at > EVENING_MS + 40 * MIN);
        // Queued behind a turn when Heimdall restarts: still a turn after the start, once (not catch-up history).
        sim.world.lock().unwrap().replies.push_back(vec![Step::Think(10 * MIN)]);
        sim.say("another long job");
        sim.advance(MIN);
        sim.notice("selfwake:wake-7a8b9c", "Scheduled wake wake-7a8b9c: you set it at 01:52 for 09:00.\nYour note, your own instruction to act on now: tell User the result");
        sim.restart(None);
        sim.advance(MIN);
        sim.settle();
        assert_eq!(carrying(&sim, "tell User the result"), 1, "{:#?}", sim.wakes());
        sim.restart(None);
        sim.advance(MIN);
        sim.settle();
        assert_eq!(carrying(&sim, "tell User the result"), 1);
        sim.check_global();
    }

    /// Idle cost: one pass over an empty schedule and outbox (what runs every 30 s all day). Measured here and
    /// asserted loosely; the measured figure is printed.
    #[test]
    fn an_idle_pass_costs_well_under_a_millisecond() {
        let d = tempfile::tempdir().unwrap();
        let clock = SimClock::at(utc(2026, 10, 20, 23, 52));
        let agent = Arc::new(Agent::default());
        let a = app(d.path(), &clock, &agent);
        let (_g, p) = gate();
        a.wakes.schedule("later", &When::In { ms: 3_600_000 }, "note", Source::Agent).unwrap();
        let n = 2000;
        let start = std::time::Instant::now();
        for _ in 0..n {
            a.wakes.pass(&a.actions, &p).unwrap();
            a.wakes.next_wait_ms();
        }
        let per = start.elapsed() / n;
        eprintln!("idle heartbeat pass: {per:?} (2880 passes a day = {:?} CPU a day)", per * 2880);
        assert!(per < std::time::Duration::from_millis(2), "{per:?}");
    }
}
