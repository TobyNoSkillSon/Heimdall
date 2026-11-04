//! The archive's writer: one thread per store per process (`heimdall-archive-live`) that makes every write to the
//! index, in the order the writes were handed over. SQLite hands its write lock out by busy-polling (sleeps of up to
//! 100 ms, no queue), so with twenty writers on their own connections a steady one starved the rest and a long one
//! decided everyone's wait (31 Oct 22:20, 1 Nov 00:12-00:22). With one writer there is nothing to poll for inside the
//! process; its connection's busy wait only ever meets another process (the night pass, a CLI).
//!
//! Two lanes. Live jobs (journals, the transcript, listen evidence, capture, screen, images, logs) commit in groups:
//! one transaction of up to `BATCH_JOBS` jobs or `BATCH_TIME`, each job a savepoint of it. Background jobs
//! (compaction publish steps, collection, embeddings) are each one bounded transaction and run when no live job waits,
//! or at most once per `BACKGROUND_EVERY` between live groups; the caller prepares everything else on its own thread
//! first. The thread runs at user-initiated
//! QoS, so a held lock is never stretched by throttled background I/O (23:51 hang sample: the compactor at background
//! QoS held the lock about a minute). Checkpoints run on their own thread and never hold a writer.
//!
//! Nothing that hands a write over waits on the archive for long: `write` waits for its own commit at most
//! `COMMIT_WAIT` and only while the writer keeps up; `enqueue` never waits; `call` (an id, or a write its caller must
//! see committed before it deletes the source) waits for the result. The writer keeps every live job through a held
//! lock, a full disk or an I/O error and retries until it commits; it never stops (31 Oct 22:21, 1 Nov 00:17: one
//! 60 s wait ended the evidence writer for good, and the agent restarted on "database is locked"). A job's own error
//! (a constraint, a bug) drops that job alone and goes back to its caller if it waits, else to the log. A job that
//! keeps failing alone with an I/O error is dropped after `STRIKES` tries.
use anyhow::{anyhow, Context, Result};
use rusqlite::Connection;
use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        Arc, Condvar, Mutex, OnceLock, Weak,
    },
    time::{Duration, Instant},
};

/// The longest `write` waits for its own commit (only while the writer keeps up).
pub const COMMIT_WAIT: Duration = Duration::from_secs(1);
/// Live jobs one transaction applies at most.
pub const BATCH_JOBS: usize = 256;
/// The longest one live transaction runs before it commits (at least one job runs).
pub const BATCH_TIME: Duration = Duration::from_millis(200);
/// Live jobs kept behind a held lock or a full disk; past it new jobs are dropped and counted.
pub const MAX_PENDING: usize = 262_144;
/// Tries a job gets alone after I/O errors before it is dropped.
pub const STRIKES: u32 = 8;
/// The writer connection's busy wait (another process only), per attempt.
pub const BUSY: Duration = Duration::from_secs(2);
/// Under continuous live writes (four listening sources never leave the live lane empty for long) a waiting
/// background job still runs once this long after the last one, between two live groups: background work keeps
/// going, and a live write waits at most one bounded background job.
pub const BACKGROUND_EVERY: Duration = Duration::from_millis(250);
/// After a full group (a backlog draining) the lock stays free this long: longer than SQLite's longest busy-handler
/// sleep (100 ms), so another process's writer gets in (31 Oct 22:20: back-to-back commits starved every other writer).
pub const YIELD: Duration = Duration::from_millis(120);
/// WAL pages after which the checkpointer runs (SQLite's own automatic checkpoint threshold).
pub const CHECKPOINT_PAGES: i32 = 1000;
/// A writer step (a job, its preparation, a commit, a background job) that takes this long is logged with the code
/// that handed it over: 2 Nov 15:51 the writer stood still 110 s and nothing in the log named what held it.
pub const SLOW_STEP: Duration = Duration::from_secs(2);
/// A step still running this long is logged while it runs (it may never finish), once.
pub const HELD_REPORT: Duration = Duration::from_secs(10);
/// The WAL size `store::tune` caps it at after checkpoints (`journal_size_limit`); past it the checkpointer says why.
pub const WAL_CAP: u64 = 64 << 20;
/// The least time between two WAL restarts the writer makes room for (`restart_wal`): each costs it one small
/// checkpoint and a full sync.
pub const RESTART_EVERY: Duration = Duration::from_secs(5);

/// Where in the code a write was handed over (`#[track_caller]`), so a slow one is named in the log.
pub type Site = &'static std::panic::Location<'static>;

/// `call_within` stopped waiting: the job stays queued and commits later, in order. Its caller tells it apart from a
/// failure (`busy`) and can see when the writer has reached it (`settled`): the screen recorder keeps its stream
/// through a held archive and starts its next epoch once the writer is back (bug note 2026-11-02T15-58-15).
#[derive(Debug, Clone)]
pub struct Busy {
    ran: Arc<AtomicBool>,
}
impl Busy {
    /// The writer has run the job this caller stopped waiting for: it is moving again.
    pub fn settled(&self) -> bool {
        self.ran.load(Ordering::Acquire)
    }
}
impl std::fmt::Display for Busy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the archive is busy")
    }
}
impl std::error::Error for Busy {}
/// The `Busy` in `e`'s chain: the archive was busy, nothing failed.
pub fn busy(e: &anyhow::Error) -> Option<&Busy> {
    e.chain().find_map(|e| e.downcast_ref::<Busy>())
}

/// One live write. It runs inside the writer's transaction (a savepoint of it) and may run again after a held lock or
/// a full disk rolled its group back, so it must not consume what it captured.
pub type Job = Box<dyn FnMut(&Connection) -> Result<()> + Send>;
/// Work a job needs done before its transaction (a blob into its pack, old chunks read): runs once, on the writer's
/// connection, outside any transaction; the job re-checks what it relies on.
pub type Prepare = Box<dyn FnOnce(&Connection) + Send>;
struct Background {
    job: Box<dyn FnOnce(&Connection) + Send>,
    site: Site,
}

/// What the writer is doing now, since when (named in the log when it takes long).
struct Doing {
    what: &'static str,
    site: Option<Site>,
    since: Instant,
    reported: bool,
}
/// The longest step since the writer fell behind (named in its "caught up" line).
#[derive(Clone, Copy)]
struct Longest {
    took: Duration,
    what: &'static str,
    site: Option<Site>,
}
fn named(what: &str, site: Option<Site>) -> String {
    match site {
        Some(s) => format!("{what} from {}:{}", s.file(), s.line()),
        None => what.to_string(),
    }
}

/// Where a write went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Written {
    /// Committed before `write` returned.
    Now,
    /// Still queued (the writer is behind); it commits in order.
    Queued,
}

struct Shared {
    root: PathBuf,
    pending: Mutex<usize>,
    /// Jobs counted in so far, and jobs done (both only change under `pending`): `settle` waits for the jobs counted
    /// in before it was called, not for an empty queue (listen notes arrive about 124 a second).
    submitted: AtomicU64,
    finished: AtomicU64,
    settled: Condvar,
    /// Queued notes (jobs nothing waits on): `records::Writer` bounds them.
    notes: AtomicUsize,
    /// A write missed `COMMIT_WAIT`: later writes return at once until nothing is pending.
    behind: AtomicBool,
    /// When each queued job was queued, oldest first (the island's "catching up").
    stamps: Mutex<VecDeque<Instant>>,
    dropped: AtomicUsize,
    /// Commits since the checkpointer last ran.
    commits: (Mutex<u64>, Condvar),
    /// Set by the checkpointer: the WAL is past its cap and its checkpoint copied every frame it saw, so the writer
    /// copies the few frames committed meanwhile, between two groups, and its next write restarts the WAL (`restart_wal`).
    restart: AtomicBool,
    /// WAL frames past which the checkpointer asks for a restart (`WAL_CAP` in pages), and the least time between two
    /// restarts in milliseconds (`RESTART_EVERY`); tests lower both.
    restart_frames: AtomicU64,
    restart_every_ms: AtomicU64,
    /// The writer's current step, and the longest since it fell behind.
    doing: Mutex<Option<Doing>>,
    longest: Mutex<Option<Longest>>,
}
struct Queued {
    site: Site,
    job: Job,
    prepare: Option<Prepare>,
    /// Commits with F_FULLFSYNC (notes commit at NORMAL).
    durable: bool,
    done: Option<crossbeam_channel::Sender<Result<(), String>>>,
    strikes: u32,
}
struct Inner {
    live: Option<crossbeam_channel::Sender<Queued>>,
    background: Option<crossbeam_channel::Sender<Background>>,
    shared: Arc<Shared>,
    checkpointer: Option<std::thread::JoinHandle<()>>,
}
impl Drop for Inner {
    /// The last handle: with nothing queued, the writer and checkpointer end now and close their connections before
    /// this returns (SQLite checkpoints when a database's last connection closes: a later close rewrote the file under
    /// its owner, e.g. an import snapshotting it). With writes still queued (a held lock), it never waits: they commit
    /// later, in order.
    fn drop(&mut self) {
        let idle = *self.shared.pending.lock().unwrap() == 0;
        self.live.take();
        self.background.take();
        if idle && !on_writer() {
            let writer = draining().lock().unwrap().remove(&self.shared.root);
            if let Some(writer) = writer {
                let _ = writer.join();
            }
            if let Some(checkpointer) = self.checkpointer.take() {
                let _ = checkpointer.join();
            }
        }
    }
}
/// A handle on a store's writer. Clones share it. When the last handle is dropped the writer still commits what is
/// queued, then ends; wait with `settle` where losing it matters (exit).
#[derive(Clone)]
pub struct Queue {
    inner: Arc<Inner>,
}

fn registry() -> &'static Mutex<Vec<Weak<Inner>>> {
    static QUEUES: OnceLock<Mutex<Vec<Weak<Inner>>>> = OnceLock::new();
    QUEUES.get_or_init(|| Mutex::new(Vec::new()))
}
/// Each store's latest writer thread (joined before another writer for that store starts).
fn draining() -> &'static Mutex<std::collections::HashMap<PathBuf, std::thread::JoinHandle<()>>> {
    static THREADS: OnceLock<Mutex<std::collections::HashMap<PathBuf, std::thread::JoinHandle<()>>>> = OnceLock::new();
    THREADS.get_or_init(Default::default)
}
fn canonical(root: &Path) -> PathBuf {
    root.canonicalize().unwrap_or_else(|_| root.to_path_buf())
}

thread_local! {
    /// Set on the writer threads (a write handed over from inside a job would wait for itself).
    static WRITER: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Pages in the WAL after the writer's last commit (`wal_hook`).
    static WAL_PAGES: std::cell::Cell<i32> = const { std::cell::Cell::new(0) };
}
fn note_wal(_: &rusqlite::hooks::Wal, pages: std::ffi::c_int) -> rusqlite::Result<()> {
    WAL_PAGES.with(|p| p.set(pages));
    Ok(())
}
/// Runs `f`, turning a panic into an error: a job's bug never ends the writer.
fn guarded<T>(f: impl FnOnce() -> Result<T>) -> Result<T> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or_else(|panic| {
        let why = panic.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| panic.downcast_ref::<String>().cloned()).unwrap_or_default();
        Err(anyhow!("archive write panicked: {why}"))
    })
}
/// True on a writer thread (inside a job).
pub fn on_writer() -> bool {
    WRITER.with(|w| w.get())
}

impl Queue {
    /// The store's writer, shared by every writer of `root` in this process, so their writes stay in one order.
    pub fn shared(root: &Path) -> Result<Self> {
        let key = {
            crate::private_dir(root)?;
            canonical(root)
        };
        let mut queues = registry().lock().unwrap();
        loop {
            queues.retain(|q| q.strong_count() > 0);
            if let Some(inner) = queues.iter().filter_map(Weak::upgrade).find(|q| q.shared.root == key) {
                return Ok(Self { inner });
            }
            // A writer whose last handle is gone may still be committing its queue: a new one starts after it, so
            // the two never write out of order.
            let previous = draining().lock().unwrap().remove(&key);
            match previous {
                Some(thread) if !thread.is_finished() => {
                    drop(queues);
                    let _ = thread.join();
                    queues = registry().lock().unwrap();
                }
                _ => break,
            }
        }
        let db = crate::store::open(root)?;
        // Opened on the caller's thread: a compactor thread's Quit interrupt must not reach the writer's statements.
        db.progress_handler(0, None::<fn() -> bool>)?;
        db.busy_timeout(BUSY)?;
        db.execute_batch("PRAGMA wal_autocheckpoint=0")?;
        db.wal_hook(Some(note_wal));
        let checkpoints = crate::store::open(root)?;
        checkpoints.progress_handler(0, None::<fn() -> bool>)?;
        let shared = Arc::new(Shared {
            root: key,
            pending: Mutex::new(0),
            submitted: AtomicU64::new(0),
            finished: AtomicU64::new(0),
            settled: Condvar::new(),
            notes: AtomicUsize::new(0),
            behind: AtomicBool::new(false),
            stamps: Mutex::new(Default::default()),
            dropped: AtomicUsize::new(0),
            commits: (Mutex::new(0), Condvar::new()),
            restart: AtomicBool::new(false),
            restart_frames: AtomicU64::new(WAL_CAP / page_size(&db)),
            restart_every_ms: AtomicU64::new(RESTART_EVERY.as_millis() as u64),
            doing: Mutex::new(None),
            longest: Mutex::new(None),
        });
        let (live, live_rx) = crossbeam_channel::unbounded::<Queued>();
        let (background, background_rx) = crossbeam_channel::unbounded::<Background>();
        let state = shared.clone();
        let thread = std::thread::Builder::new().name("heimdall-archive-live".into()).spawn(move || {
            set_user_initiated();
            WRITER.with(|w| w.set(true));
            drain(&db, &live_rx, &background_rx, &state);
            // The last handle is gone: the checkpointer ends (no checkpoint after its owner is done with the files).
            *state.commits.0.lock().unwrap() = u64::MAX;
            state.commits.1.notify_all();
        })?;
        draining().lock().unwrap().insert(shared.root.clone(), thread);
        let state = Arc::downgrade(&shared);
        let checkpointer = std::thread::Builder::new().name("heimdall-archive-checkpoint".into()).spawn(move || checkpointer(&checkpoints, &state))?;
        let inner = Arc::new(Inner { live: Some(live), background: Some(background), shared, checkpointer: Some(checkpointer) });
        queues.push(Arc::downgrade(&inner));
        Ok(Self { inner })
    }

    /// Hands `job` to the writer (durable: F_FULLFSYNC) and waits for its commit at most `COMMIT_WAIT`, only while the
    /// writer keeps up. Its own error comes back if it failed in that time; a held lock never does.
    #[track_caller]
    pub fn write(&self, job: impl FnMut(&Connection) -> Result<()> + Send + 'static) -> Result<Written> {
        self.write_prepared(None, job)
    }
    /// `write` with work done before the job's transaction (see `Prepare`).
    #[track_caller]
    pub fn write_prepared(&self, prepare: Option<Prepare>, job: impl FnMut(&Connection) -> Result<()> + Send + 'static) -> Result<Written> {
        let site = std::panic::Location::caller();
        anyhow::ensure!(!on_writer(), "an archive write handed over from inside another write");
        let shared = &self.inner.shared;
        let mut wait = !shared.behind.load(Ordering::Acquire);
        if !wait && shared.stamps.lock().unwrap().front().is_none_or(|at| at.elapsed() < COMMIT_WAIT / 2) {
            // The writer caught up (its oldest job is young again): wait for commits again.
            shared.behind.store(false, Ordering::Release);
            wait = true;
        }
        let (done, answer) = crossbeam_channel::bounded(1);
        anyhow::ensure!(
            self.submit(Queued { site, job: Box::new(job), prepare, durable: true, done: wait.then_some(done), strikes: 0 })?,
            "archive write dropped: {MAX_PENDING} writes are already queued"
        );
        if !wait {
            return Ok(Written::Queued);
        }
        match answer.recv_timeout(COMMIT_WAIT) {
            Ok(Ok(())) => Ok(Written::Now),
            Ok(Err(e)) => Err(anyhow!("archive write failed: {e}")),
            Err(_) => {
                shared.behind.store(true, Ordering::Release);
                Ok(Written::Queued)
            }
        }
    }
    /// Hands `job` to the writer without waiting (a note: committed at NORMAL, durable with the next full-sync
    /// commit). Order with every other write is kept.
    #[track_caller]
    pub fn enqueue(&self, job: impl FnMut(&Connection) -> Result<()> + Send + 'static) -> Result<()> {
        self.enqueue_prepared(None, job)
    }
    #[track_caller]
    pub fn enqueue_prepared(&self, prepare: Option<Prepare>, job: impl FnMut(&Connection) -> Result<()> + Send + 'static) -> Result<()> {
        let site = std::panic::Location::caller();
        if on_writer() {
            return Err(anyhow!("an archive write handed over from inside another write"));
        }
        self.inner.shared.notes.fetch_add(1, Ordering::Relaxed);
        match self.submit(Queued { site, job: Box::new(job), prepare, durable: false, done: None, strikes: 0 }) {
            Ok(true) => Ok(()),
            other => {
                self.inner.shared.notes.fetch_sub(1, Ordering::Relaxed);
                other.map(|_| ())
            }
        }
    }
    /// Runs `f` as a live job and waits for its result: an id, or a write its caller must see committed before it
    /// deletes or rewrites the source. Waits as long as the writer needs (another process may hold the lock), so
    /// never call it from a callback the user waits on. Its own error comes back.
    #[track_caller]
    pub fn call<T: Send + 'static>(&self, f: impl FnMut(&Connection) -> Result<T> + Send + 'static) -> Result<T> {
        self.call_until(None, f)
    }
    /// `call` waiting at most `wait` (a screen epoch's first row). Past it the job stays queued and commits later; the
    /// caller hears "the archive is busy" (a `Busy`) and must not depend on it not happening.
    #[track_caller]
    pub fn call_within<T: Send + 'static>(&self, wait: Duration, f: impl FnMut(&Connection) -> Result<T> + Send + 'static) -> Result<T> {
        self.call_until(Some(wait), f)
    }
    #[track_caller]
    fn call_until<T: Send + 'static>(&self, wait: Option<Duration>, mut f: impl FnMut(&Connection) -> Result<T> + Send + 'static) -> Result<T> {
        let site = std::panic::Location::caller();
        anyhow::ensure!(!on_writer(), "an archive write handed over from inside another write");
        let slot = Arc::new(Mutex::new(None));
        let out = slot.clone();
        let (done, answer) = crossbeam_channel::bounded(1);
        let ran = Arc::new(AtomicBool::new(false));
        let reached = ran.clone();
        let job = move |db: &Connection| {
            let value = f(db);
            reached.store(true, Ordering::Release);
            *out.lock().unwrap() = Some(value?);
            Ok(())
        };
        anyhow::ensure!(self.submit(Queued { site, job: Box::new(job), prepare: None, durable: true, done: Some(done), strikes: 0 })?, "the archive writer has too many writes queued");
        let outcome = match wait {
            None => answer.recv().map_err(|_| anyhow!("archive writer stopped")),
            Some(wait) => match answer.recv_timeout(wait) {
                Ok(outcome) => Ok(outcome),
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => return Err(anyhow::Error::new(Busy { ran })),
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => Err(anyhow!("archive writer stopped")),
            },
        };
        match outcome? {
            Ok(()) => slot.lock().unwrap().take().ok_or_else(|| anyhow!("archive call produced no value")),
            Err(e) => Err(anyhow!("archive write failed: {e}")),
        }
    }
    /// Runs `f` as a background job, in its own transaction, once no live job waits, and returns its result. A held
    /// lock (another process) comes back as an error at once: the background caller tries again later.
    #[track_caller]
    pub fn background<T: Send + 'static>(&self, f: impl FnOnce(&Connection) -> Result<T> + Send + 'static) -> Result<T> {
        let site = std::panic::Location::caller();
        anyhow::ensure!(!on_writer(), "an archive write handed over from inside another write");
        let (done, answer) = crossbeam_channel::bounded(1);
        let job = Background { site, job: Box::new(move |db: &Connection| {
            let _ = done.send(guarded(|| background_once(db, f)));
        }) };
        self.inner.background.as_ref().context("archive writer stopped")?.send(job).map_err(|_| anyhow!("archive writer stopped"))?;
        answer.recv().map_err(|_| anyhow!("archive writer stopped"))?
    }
    /// False when the job was dropped (too many pending).
    fn submit(&self, q: Queued) -> Result<bool> {
        let shared = &self.inner.shared;
        if !shared.queued() {
            let n = shared.dropped.fetch_add(1, Ordering::Relaxed) + 1;
            lost_write();
            if n == 1 || n.is_power_of_two() {
                eprintln!("Archive writes dropped: {n} past {MAX_PENDING} queued");
            }
            return Ok(false);
        }
        if self.inner.live.as_ref().is_none_or(|live| live.send(q).is_err()) {
            shared.done(1);
            return Err(anyhow!("archive writer stopped"));
        }
        Ok(true)
    }

    /// What the writer is doing now and for how long ("a background job from src/compact.rs:181"), `None` while it
    /// waits for work.
    pub fn holder(&self) -> Option<(String, Duration)> {
        self.inner.shared.holder()
    }
    /// Writes queued and not yet committed.
    pub fn pending(&self) -> usize {
        *self.inner.shared.pending.lock().unwrap()
    }
    /// Notes queued and not yet committed.
    pub fn notes(&self) -> usize {
        self.inner.shared.notes.load(Ordering::Relaxed)
    }
    /// Waits until every queued write has been committed, or `timeout` (at once on the writer: it would wait for
    /// itself). True when nothing is left.
    pub fn settle(&self, timeout: Duration) -> bool {
        settle_shared(&self.inner.shared, timeout)
    }
}

/// One background job's transaction on the writer's connection.
fn background_once<T>(db: &Connection, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
    db.execute_batch("BEGIN IMMEDIATE")?;
    match f(db) {
        Ok(v) => match db.execute_batch("COMMIT") {
            Ok(()) => Ok(v),
            Err(e) => {
                if !db.is_autocommit() {
                    let _ = db.execute_batch("ROLLBACK");
                }
                Err(e.into())
            }
        },
        Err(e) => {
            if !db.is_autocommit() {
                let _ = db.execute_batch("ROLLBACK");
            }
            Err(e)
        }
    }
}

impl Shared {
    /// Counts a new job; false (not counted) past `MAX_PENDING`.
    fn queued(&self) -> bool {
        let mut pending = self.pending.lock().unwrap();
        if *pending >= MAX_PENDING {
            return false;
        }
        self.stamps.lock().unwrap().push_back(Instant::now());
        *pending += 1;
        self.submitted.fetch_add(1, Ordering::Relaxed);
        true
    }
    /// One queued job is done (committed, or failed and reported). Returns the jobs still queued.
    fn done(&self, n: usize) -> usize {
        let mut pending = self.pending.lock().unwrap();
        *pending -= n;
        self.finished.fetch_add(n as u64, Ordering::Relaxed);
        let mut stamps = self.stamps.lock().unwrap();
        for _ in 0..n {
            stamps.pop_front();
        }
        if *pending == 0 {
            stamps.clear();
            self.behind.store(false, Ordering::Release);
        }
        self.settled.notify_all();
        *pending
    }
    /// Runs one writer step under its name: a step that takes `SLOW_STEP` is logged with the code that handed it over,
    /// and the checkpointer names one still running at `HELD_REPORT` (`report_held`).
    fn step<T>(&self, what: &'static str, site: Option<Site>, f: impl FnOnce() -> T) -> T {
        let since = Instant::now();
        *self.doing.lock().unwrap() = Some(Doing { what, site, since, reported: false });
        let out = f();
        let took = since.elapsed();
        *self.doing.lock().unwrap() = None;
        if took >= SLOW_STEP {
            eprintln!("Archive writer held {:.1} s by {}", took.as_secs_f64(), named(what, site));
        }
        let mut longest = self.longest.lock().unwrap();
        if longest.is_none_or(|l| took > l.took) {
            *longest = Some(Longest { took, what, site });
        }
        out
    }
    /// The writer's step running now and for how long (`None` while it waits for work): what holds the archive.
    fn holder(&self) -> Option<(String, Duration)> {
        self.doing.lock().unwrap().as_ref().map(|d| (named(d.what, d.site), d.since.elapsed()))
    }
    /// Logs, once, a step that has run `HELD_REPORT` so far (the checkpointer's thread asks every few seconds).
    fn report_held(&self) {
        let mut doing = self.doing.lock().unwrap();
        if let Some(d) = doing.as_mut().filter(|d| !d.reported && d.since.elapsed() >= HELD_REPORT) {
            d.reported = true;
            let waiting = *self.pending.lock().unwrap();
            eprintln!("Archive writer busy {:.0} s so far with {}; {waiting} writes waiting", d.since.elapsed().as_secs_f64(), named(d.what, d.site));
        }
    }
    /// After a commit: wakes the checkpointer once the WAL holds `CHECKPOINT_PAGES`.
    fn committed(&self) {
        if WAL_PAGES.with(|p| p.replace(0)) >= CHECKPOINT_PAGES {
            *self.commits.0.lock().unwrap() += 1;
            self.commits.1.notify_all();
        }
    }
}

/// A held lock, a full disk or an I/O error: the environment, not the job. The group rolls back and every job in it
/// runs again later.
pub fn transient(e: &anyhow::Error) -> bool {
    code(e).is_some_and(|c| {
        matches!(c, rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked | rusqlite::ErrorCode::DiskFull | rusqlite::ErrorCode::SystemIoFailure)
    })
}
fn code(e: &anyhow::Error) -> Option<rusqlite::ErrorCode> {
    e.chain().find_map(|e| match e.downcast_ref::<rusqlite::Error>() {
        Some(rusqlite::Error::SqliteFailure(c, _)) => Some(c.code),
        _ => None,
    })
}
/// Busy or full: waited out without limit. An I/O error strikes the job that hit it.
fn waits(e: &anyhow::Error) -> bool {
    code(e).is_some_and(|c| matches!(c, rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked | rusqlite::ErrorCode::DiskFull))
}
fn reason(e: &anyhow::Error) -> &'static str {
    match code(e) {
        Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked) => "another process holds the archive",
        Some(rusqlite::ErrorCode::DiskFull) => "the disk is full",
        _ => "the disk reported an I/O error",
    }
}

/// "cannot start a transaction within a transaction": a job that opens its own `BEGIN` cannot be a savepoint of a
/// group; it runs alone, outside one.
fn nested_begin(e: &anyhow::Error) -> bool {
    format!("{e:#}").contains("within a transaction")
}

/// The writer: live groups first, a background job only while no live job waits.
fn drain(db: &Connection, live: &crossbeam_channel::Receiver<Queued>, background: &crossbeam_channel::Receiver<Background>, state: &Shared) {
    let mut jobs: VecDeque<Queued> = VecDeque::new();
    let mut behind: Option<(Instant, usize)> = None;
    let mut waiting: Option<&'static str> = None;
    let mut backoff = Duration::from_millis(50);
    let mut isolate = false;
    let mut failed = 0usize;
    let mut last_background = Instant::now();
    let mut last_restart: Option<Instant> = None;
    loop {
        let every = Duration::from_millis(state.restart_every_ms.load(Ordering::Relaxed));
        if db.is_autocommit() && last_restart.is_none_or(|t| t.elapsed() >= every) && state.restart.swap(false, Ordering::AcqRel) {
            if !state.step("a WAL restart checkpoint", None, || restart_wal(db)) {
                state.restart.store(true, Ordering::Release);
            }
            last_restart = Some(Instant::now());
        }
        while jobs.len() < BATCH_JOBS {
            match live.try_recv() {
                Ok(job) => jobs.push_back(job),
                Err(_) => break,
            }
        }
        if !jobs.is_empty() && last_background.elapsed() >= BACKGROUND_EVERY {
            if let Ok(job) = background.try_recv() {
                state.step("a background job", Some(job.site), || (job.job)(db));
                end_open(db);
                state.committed();
                last_background = Instant::now();
                continue;
            }
        }
        if jobs.is_empty() {
            // Nothing live: one background job, else wait for either lane.
            if let Ok(job) = background.try_recv() {
                state.step("a background job", Some(job.site), || (job.job)(db));
                end_open(db);
                state.committed();
                last_background = Instant::now();
                continue;
            }
            crossbeam_channel::select! {
                recv(live) -> job => match job {
                    Ok(job) => jobs.push_back(job),
                    Err(_) => {
                        // Every handle is gone; background jobs never outlive their caller.
                        while let Ok(job) = background.try_recv() { (job.job)(db); }
                        return;
                    }
                },
                recv(background) -> job => {
                    if let Ok(job) = job {
                        state.step("a background job", Some(job.site), || (job.job)(db));
                        end_open(db);
                        state.committed();
                        last_background = Instant::now();
                    }
                    continue;
                },
            }
            continue;
        }
        for q in jobs.iter_mut().take(BATCH_JOBS) {
            if let Some(prepare) = q.prepare.take() {
                state.step("a write's preparation", Some(q.site), || {
                    let _ = guarded(|| {
                        prepare(db);
                        Ok(())
                    });
                });
            }
        }
        behind.get_or_insert((Instant::now(), 0));
        let group_started = Instant::now();
        let ran = if isolate { alone(db, &mut jobs, state) } else { group(db, &mut jobs, state) };
        let full = ran.finished.len() >= BATCH_JOBS || group_started.elapsed() >= BATCH_TIME;
        isolate = false;
        if !ran.finished.is_empty() {
            state.committed();
        }
        for (q, outcome) in ran.finished {
            if let (None, Err(e)) = (&q.done, &outcome) {
                failed += 1;
                if failed == 1 || failed.is_power_of_two() {
                    eprintln!("Archive write failed and was dropped ({failed} so far): {e}");
                }
            }
            if !q.durable {
                state.notes.fetch_sub(1, Ordering::Relaxed);
            }
            if let Some(done) = q.done {
                if let Err(crossbeam_channel::SendError(Err(e))) = done.send(outcome) {
                    // Its caller stopped waiting (`COMMIT_WAIT`, `call_within`): nobody else hears it.
                    failed += 1;
                    if failed == 1 || failed.is_power_of_two() {
                        eprintln!("Archive write failed and was dropped ({failed} so far): {e}");
                    }
                }
            }
            if let Some(b) = behind.as_mut() {
                b.1 += 1;
            }
            if state.done(1) == 0 {
                let (at, n) = behind.take().unwrap_or((Instant::now(), 0));
                let longest = state.longest.lock().unwrap().take();
                if waiting.take().is_some() || at.elapsed() >= Duration::from_secs(1) {
                    // The longest step names what held the writer (its own work; another process's lock shows as the
                    // "wait" line and as a long BEGIN).
                    let held = longest.filter(|l| l.took >= SLOW_STEP).map(|l| format!("; longest step {:.1} s: {}", l.took.as_secs_f64(), named(l.what, l.site))).unwrap_or_default();
                    eprintln!("Archive live writes caught up: {n} queued, {:.1} s behind the held lock{held}", at.elapsed().as_secs_f64());
                }
                behind = None;
            }
        }
        match ran.retry {
            None => {
                backoff = Duration::from_millis(50);
                if full && (!jobs.is_empty() || !live.is_empty()) {
                    // A backlog draining: another process's writer takes its turn in the gap.
                    std::thread::sleep(YIELD);
                }
            }
            Some(e) => {
                let why = reason(&e);
                if waiting != Some(why) {
                    eprintln!("Archive writes wait: {why} ({e:#}); {} kept in order", jobs.len() + live.len());
                    waiting = Some(why);
                }
                // An I/O error: the front job alone next, so one bad job cannot hold the rest for ever.
                isolate = !waits(&e);
                std::thread::sleep(backoff);
                let cap = if crate::embedding::database_busy(&e) { Duration::from_secs(2) } else { Duration::from_secs(30) };
                backoff = (backoff * 2).min(cap);
            }
        }
    }
}

/// The database's page size (4096 unless the store says otherwise).
fn page_size(db: &Connection) -> u64 {
    db.query_row("PRAGMA page_size", [], |r| r.get::<_, i64>(0)).ok().filter(|n| *n > 0).map_or(4096, |n| n as u64)
}
/// SQLite restarts the WAL (and `journal_size_limit` cuts the file back to `WAL_CAP`) only at a write whose
/// transaction began after a checkpoint copied every frame. The checkpointer runs on its own thread and a full-sync
/// checkpoint outlasts the writer's 200 ms groups, so a commit nearly always lands during it and the WAL only grows
/// (3-4 Nov: 64 MB to 4.2 GB, "every page copied" 1094 times in a day; a GB-sized WAL slows every read and makes each
/// checkpoint's final sync seconds long). Here, on the writer between two groups, nothing can commit meanwhile: one
/// PASSIVE checkpoint copies what was committed since the checkpointer's (never waiting for a reader), and the next
/// group's first write restarts the WAL unless a reader still holds an old snapshot (then it is asked again later).
/// False when the checkpoint could not run (the checkpointer's was running): asked again.
fn restart_wal(db: &Connection) -> bool {
    db.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |r| r.get::<_, i64>(0)).is_ok_and(|busy| busy == 0)
}

/// A background job that panicked mid-transaction leaves it open: rolled back before the next group.
fn end_open(db: &Connection) {
    if !db.is_autocommit() {
        let _ = db.execute_batch("ROLLBACK");
    }
}

/// Checkpoints the WAL (PASSIVE: never blocks a reader or the writer) once the writer's commits filled
/// `CHECKPOINT_PAGES`, at most once a second.
/// Every few seconds it also names a writer step that has run `HELD_REPORT` (`Shared::report_held`).
fn checkpointer(db: &Connection, state: &Weak<Shared>) {
    let mut seen = 0u64;
    let mut wal = WalWatch::default();
    loop {
        let Some(shared) = state.upgrade() else { return };
        let now = {
            let commits = shared.commits.0.lock().unwrap();
            let (commits, _) = shared.commits.1.wait_timeout_while(commits, Duration::from_secs(5), |c| *c == seen).unwrap();
            *commits
        };
        shared.report_held();
        drop(shared);
        if now == u64::MAX {
            return;
        }
        if now != seen {
            let started = Instant::now();
            let pages = db.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |r| Ok((r.get::<_, i64>(1)?, r.get::<_, i64>(2)?)));
            let took = started.elapsed();
            if took >= SLOW_STEP {
                // 4 Nov 03:36 and 04:19: the writer stood 5 s in a page read while this thread was in the checkpoint's
                // final truncate of the database file. Said so the next stall is named.
                let copied = pages.as_ref().map_or(-1, |p| p.1);
                eprintln!("Archive checkpoint took {:.1} s ({copied} WAL pages in it); the writer's reads wait for its final sync", took.as_secs_f64());
            }
            if let Ok((log, copied)) = pages {
                if let Some(shared) = state.upgrade() {
                    if log >= 0 && copied >= log && log as u64 >= shared.restart_frames.load(Ordering::Relaxed) {
                        shared.restart.store(true, Ordering::Release);
                    }
                }
            }
            if let (Ok((log, copied)), Some(path)) = (pages, db.path().filter(|p| !p.is_empty())) {
                let bytes = std::fs::metadata(format!("{path}-wal")).map_or(0, |m| m.len());
                if let Some(line) = wal.look(bytes, log, copied) {
                    eprintln!("{line}");
                }
            }
            seen = now;
            std::thread::sleep(Duration::from_secs(1));
        }
    }
}

/// The WAL past `WAL_CAP` (2 Nov 15:55: 991 MB against the 64 MB cap, nothing in the log said why). SQLite truncates
/// it to the cap only when a checkpoint copied every page and the next write restarts it; a reader holding an old
/// snapshot stops both, and one large transaction fills it at once. Said once per doubling, and once when it is back.
#[derive(Default)]
pub(crate) struct WalWatch {
    said: u64,
}
impl WalWatch {
    /// `bytes`: the WAL file's size; `log`, `copied`: the pages in it and the pages the checkpoint copied.
    pub(crate) fn look(&mut self, bytes: u64, log: i64, copied: i64) -> Option<String> {
        if bytes <= WAL_CAP {
            let back = self.said > 0;
            self.said = 0;
            return back.then(|| format!("Archive WAL back under its cap: {} MB", bytes >> 20));
        }
        if self.said > 0 && bytes < self.said * 2 {
            return None;
        }
        self.said = bytes;
        let why = if log < 0 || copied < 0 {
            "the checkpoint could not run".to_string()
        } else if copied < log {
            format!("the checkpoint copied {copied} of {log} pages: a reader holds an old snapshot, so the WAL cannot restart")
        } else {
            format!("every page copied ({log}); it is truncated at the next write that finds no reader on it")
        };
        Some(format!("Archive WAL at {} MB, past its {} MB cap: {why}", bytes >> 20, WAL_CAP >> 20))
    }
}

/// What one group did.
struct Ran {
    /// Jobs taken off the queue: committed (`Ok`) or dropped for their own error.
    finished: Vec<(Queued, Result<(), String>)>,
    /// Stopped by a held lock, a full disk or an I/O error: the rest run again after a backoff.
    retry: Option<anyhow::Error>,
}

/// One transaction over the front of `jobs`, at F_FULLFSYNC when any job it may take is durable.
fn group(db: &Connection, jobs: &mut VecDeque<Queued>, state: &Shared) -> Ran {
    let durable = jobs.iter().take(BATCH_JOBS).any(|q| q.durable);
    if !durable && db.execute_batch("PRAGMA synchronous=NORMAL").is_err() {
        return alone(db, jobs, state);
    }
    let ran = group_at(db, jobs, state);
    if !durable {
        let _ = db.execute_batch("PRAGMA synchronous=FULL");
    }
    ran
}

fn group_at(db: &Connection, jobs: &mut VecDeque<Queued>, state: &Shared) -> Ran {
    if let Err(e) = state.step("taking the write lock", None, || db.execute_batch("BEGIN IMMEDIATE")) {
        return Ran { finished: vec![], retry: Some(e.into()) };
    }
    let started = Instant::now();
    let (mut ran, mut failed, mut solo) = (0usize, vec![], false);
    while ran < jobs.len() && ran < BATCH_JOBS && (ran == 0 || started.elapsed() < BATCH_TIME) {
        if let Err(e) = db.execute_batch("SAVEPOINT live_job") {
            if !db.is_autocommit() {
                let _ = db.execute_batch("ROLLBACK");
            }
            return Ran { finished: vec![], retry: Some(e.into()) };
        }
        let site = jobs[ran].site;
        let job = &mut jobs[ran].job;
        match state.step("a live write", Some(site), || guarded(|| job(db))) {
            Ok(()) => {
                let _ = db.execute_batch("RELEASE live_job");
                ran += 1;
            }
            Err(e) => {
                let _ = db.execute_batch("ROLLBACK TO live_job; RELEASE live_job");
                if transient(&e) || db.is_autocommit() {
                    // The environment, or an error that took the whole transaction with it: nothing of this group
                    // commits; every job runs again (alone first, after an I/O error).
                    if !db.is_autocommit() {
                        let _ = db.execute_batch("ROLLBACK");
                    }
                    return Ran { finished: vec![], retry: Some(e) };
                }
                if nested_begin(&e) {
                    solo = true;
                    break;
                }
                failed.push((ran, format!("{e:#}")));
                ran += 1;
            }
        }
    }
    if let Err(e) = state.step("a commit", None, || db.execute_batch("COMMIT")) {
        if !db.is_autocommit() {
            let _ = db.execute_batch("ROLLBACK");
        }
        return Ran { finished: vec![], retry: Some(e.into()) };
    }
    let mut finished: Vec<(Queued, Result<(), String>)> = jobs.drain(..ran).map(|q| (q, Ok(()))).collect();
    for (i, e) in failed {
        finished[i].1 = Err(e);
    }
    if solo {
        let more = alone(db, jobs, state);
        finished.extend(more.finished);
        return Ran { finished, retry: more.retry };
    }
    Ran { finished, retry: None }
}

/// The front job in its own transaction, outside any group. An I/O error strikes it; after `STRIKES` it is dropped.
fn alone(db: &Connection, jobs: &mut VecDeque<Queued>, state: &Shared) -> Ran {
    let Some(mut q) = jobs.pop_front() else { return Ran { finished: vec![], retry: None } };
    let site = q.site;
    let result = state.step("a live write on its own", Some(site), || -> Result<()> {
        db.execute_batch("BEGIN IMMEDIATE")?;
        match guarded(|| (q.job)(db)) {
            Ok(()) => Ok(db.execute_batch("COMMIT")?),
            Err(e) if nested_begin(&e) => {
                // The job opens its own transaction: run it bare.
                let _ = db.execute_batch("ROLLBACK");
                guarded(|| (q.job)(db))
            }
            Err(e) => Err(e),
        }
    });
    match result {
        Ok(()) => Ran { finished: vec![(q, Ok(()))], retry: None },
        Err(e) => {
            if !db.is_autocommit() {
                let _ = db.execute_batch("ROLLBACK");
            }
            if transient(&e) {
                if !waits(&e) {
                    q.strikes += 1;
                    if q.strikes >= STRIKES {
                        let e = format!("{e:#} (gave up after {STRIKES} tries)");
                        return Ran { finished: vec![(q, Err(e))], retry: None };
                    }
                }
                jobs.push_front(q);
                return Ran { finished: vec![], retry: Some(e) };
            }
            let e = format!("{e:#}");
            Ran { finished: vec![(q, Err(e))], retry: None }
        }
    }
}

/// The writes queued in this process, over every store: how many, and when the oldest was queued. `None` while
/// nothing waits.
pub fn backlog() -> Option<(usize, Instant)> {
    let queues: Vec<_> = registry().lock().unwrap().iter().filter_map(Weak::upgrade).collect();
    queues.iter().fold(None, |acc, q| {
        let n = *q.shared.pending.lock().unwrap();
        let oldest = q.shared.stamps.lock().unwrap().front().copied();
        match (n, oldest, acc) {
            (0, _, acc) | (_, None, acc) => acc,
            (n, Some(at), None) => Some((n, at)),
            (n, Some(at), Some((m, since))) => Some((n + m, at.min(since))),
        }
    })
}

/// Writes dropped in this process since it started (only rises): past `MAX_PENDING`, or a `records::Writer` note
/// past its backlog (`records::WRITER_BACKLOG`, about 9 minutes of notes). Notes lost (the island's "catching up").
static LOST: AtomicUsize = AtomicUsize::new(0);
pub(crate) fn lost_write() {
    LOST.fetch_add(1, Ordering::Relaxed);
}
pub fn dropped_writes() -> usize {
    LOST.load(Ordering::Relaxed)
}

/// Waits until every job counted in before the call is done (later ones are not waited for).
fn settle_shared(shared: &Shared, timeout: Duration) -> bool {
    let pending = shared.pending.lock().unwrap();
    let target = shared.submitted.load(Ordering::Relaxed);
    let reached = || shared.finished.load(Ordering::Relaxed) >= target;
    if on_writer() {
        return reached();
    }
    let (_pending, _) = shared.settled.wait_timeout_while(pending, timeout, |_| !reached()).unwrap();
    reached()
}

/// For readers on other threads that need the latest rows (the decider's archive sink reads the utterance it decided
/// on): waits until no write for `root` is queued, or `timeout`. True when settled.
pub fn settle(root: &Path, timeout: Duration) -> bool {
    let root = canonical(root);
    let queues: Vec<_> = registry().lock().unwrap().iter().filter_map(Weak::upgrade).filter(|q| q.shared.root == root).collect();
    let until = Instant::now() + timeout;
    queues.iter().all(|q| settle_shared(&q.shared, until.saturating_duration_since(Instant::now())))
}

/// While alive, a thread at background or utility QoS runs at user-initiated QoS: an idle-only maintenance write on
/// its own connection (the timeline backfill's transactions, legacy migration) never holds the index's write lock at
/// throttled priority.
pub(crate) struct Raised(Option<libc::qos_class_t>);
impl Raised {
    pub(crate) fn new() -> Self {
        #[cfg(target_os = "macos")]
        unsafe {
            extern "C" {
                fn qos_class_self() -> libc::qos_class_t;
            }
            let qos = qos_class_self();
            if matches!(qos, libc::qos_class_t::QOS_CLASS_BACKGROUND | libc::qos_class_t::QOS_CLASS_UTILITY) {
                set_user_initiated();
                return Self(Some(qos));
            }
        }
        Self(None)
    }
}
impl Drop for Raised {
    fn drop(&mut self) {
        #[cfg(target_os = "macos")]
        if let Some(qos) = self.0 {
            unsafe {
                libc::pthread_set_qos_class_self_np(qos, 0);
            }
        }
    }
}
#[cfg(target_os = "macos")]
fn set_user_initiated() {
    unsafe {
        libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INITIATED, 0);
    }
}
#[cfg(not(target_os = "macos"))]
fn set_user_initiated() {}

#[cfg(test)]
mod wal_tests {
    use super::*;
    /// The WAL restarts under continuous writes (3-4 Nov: it grew to 4.2 GB because every checkpoint raced a commit).
    /// The threshold is lowered to 1,000 frames (4 MB) here; 48 MB is written in 64 KB rows, committed back to back.
    #[test]
    fn the_wal_restarts_under_continuous_writes() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("archive");
        crate::Journal::open(&crate::store::session_path(&root, "session-wal")).unwrap();
        let queue = Queue::shared(&root).unwrap();
        // `HEIMDALL_WAL_TEST_UNFIXED=1`: the writer never asked to restart, as before the fix (measured 59 MB vs 10 MB).
        let before = std::env::var_os("HEIMDALL_WAL_TEST_UNFIXED").is_some();
        queue.inner.shared.restart_frames.store(if before { u64::MAX } else { 1000 }, Ordering::Relaxed);
        queue.inner.shared.restart_every_ms.store(100, Ordering::Relaxed);
        queue.call(|db| Ok(db.execute_batch("CREATE TABLE IF NOT EXISTS wal_test(id INTEGER PRIMARY KEY, data BLOB)")?)).unwrap();
        let wal = root.join(format!("{}-wal", crate::store::INDEX));
        let rows = 768usize;
        let started = Instant::now();
        let mut largest = 0u64;
        for i in 0..rows {
            let data: Vec<u8> = (0..65_536u32).map(|k| (k.wrapping_mul(2_654_435_761) >> 13) as u8 ^ i as u8).collect();
            queue.enqueue(move |db| Ok(db.execute("INSERT OR REPLACE INTO wal_test VALUES(?1,?2)", rusqlite::params![i as i64, data]).map(|_| ())?)).unwrap();
            // About 12 MB a second in small groups, like capture and notes under load (4 Nov 04:45: 6 MB a second).
            std::thread::sleep(Duration::from_millis(5));
            if i % 16 == 0 {
                largest = largest.max(std::fs::metadata(&wal).map_or(0, |m| m.len()));
            }
        }
        assert!(queue.settle(Duration::from_secs(60)));
        largest = largest.max(std::fs::metadata(&wal).map_or(0, |m| m.len()));
        let n: i64 = queue.call(|db| Ok(db.query_row("SELECT count(*) FROM wal_test", [], |r| r.get(0))?)).unwrap();
        eprintln!("48 MB in {:?}; largest WAL {} MB", started.elapsed(), largest >> 20);
        assert_eq!(n as usize, rows, "every row kept");
        assert!(largest < 24 << 20, "the WAL restarted instead of growing to the data's size: largest {} MB", largest >> 20);
    }
    /// 2 Nov 15:55: the WAL at 991 MB against the 64 MB cap, and nothing said why. Past the cap the checkpointer says
    /// whether a reader stopped the checkpoint, once per doubling, and once when it is back.
    #[test]
    fn a_wal_past_its_cap_is_explained_once_per_doubling() {
        let mut w = WalWatch::default();
        assert_eq!(w.look(10 << 20, 100, 100), None, "under the cap: nothing");
        let line = w.look(991 << 20, 250_000, 1_000).unwrap();
        assert!(line.starts_with("Archive WAL at 991 MB, past its 64 MB cap") && line.contains("a reader holds an old snapshot"), "{line}");
        assert_eq!(w.look(1500 << 20, 300_000, 1_000), None, "not again until it doubles");
        assert!(w.look(2000 << 20, 500_000, 500_000).unwrap().contains("every page copied"));
        assert_eq!(w.look(64 << 20, 0, 0).as_deref(), Some("Archive WAL back under its cap: 64 MB"));
        assert_eq!(w.look(1 << 20, 0, 0), None);
    }
}
