//! The archive's writer: one thread per store per process (`heimdall-archive-live`) that makes every write to the
//! index, in the order the writes were handed over. SQLite hands its write lock out by busy-polling (sleeps of up to
//! 100 ms, no queue), so with twenty writers on their own connections a steady one starved the rest and a long one
//! decided everyone's wait (31 Oct 22:20, 1 Nov 00:12-00:22). With one writer there is nothing to poll for inside the
//! process; its connection's busy wait only ever meets another process (the night pass, a CLI).
//!
//! Two lanes. Live jobs (journals, the transcript, listen evidence, capture, screen, images, logs) commit in groups:
//! one transaction of up to `BATCH_JOBS` jobs (`BATCH_BEHIND` while a backlog drains) or `BATCH_TIME`, each job a
//! savepoint of it; notes nobody waits on gather for `NOTE_GROUP` first. Background jobs
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
/// Live jobs one transaction applies at most while more than `BATCH_JOBS` wait (still at most `BATCH_TIME` of work).
/// A backlog drained 256 jobs per full-sync commit plus a `YIELD`: 4 Nov 22:04-22:10 and 5 Nov 00:09-00:14 about 165
/// jobs a second, barely above what arrives, so 40,000-50,000 queued writes took 250-320 s to catch up.
pub const BATCH_BEHIND: usize = 2048;
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
/// The WAL size past which the writer makes room for a restart (`restart_wal`): half of `WAL_CAP`, so the WAL stays
/// under the cap. Restarting only past the cap left it rising past 64 MB and back every 8 s (4-5 Nov), two log lines
/// each time.
pub const RESTART_AT: u64 = WAL_CAP / 2;
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
/// The most WAL frames the writer's restart checkpoint (`restart_wal`) copies: more, and it leaves them to the
/// checkpointer's own thread and asks again after that one's next full copy. 9 Nov 16:39-16:45 the WAL stood at 169-553
/// MB and the writer's restart checkpoints held every live write 2.0-7.3 s copying what had piled up (log sweep F2).
pub const RESTART_COPY: u64 = 2048;
/// Restarts skipped in a row for a larger tail before the writer copies it anyway: under a long burst each full copy
/// can leave more than `RESTART_COPY` frames behind it, and the WAL must not grow without bound (3-4 Nov: 4.2 GB).
pub const RESTART_SKIPS: u32 = 3;
/// How often a gathering writer looks for a waiting background job or `settle` caller.
const GATHER_STEP: Duration = Duration::from_millis(25);
/// Notes alone (no caller waits on any queued job) gather this long after the previous commit before they commit. 9 Nov:
/// listening's VAD notes (about 62 a second) went out one or two per transaction, about 62 commits a second, each
/// rewriting about 12 index pages into the WAL and again into the database at its checkpoint: 6.3 MB a second of
/// logical writes, 1,058 GB in 30 hours (macOS's disk-write reports, log sweep F1). Replaying that WAL's commits
/// grouped by 16 (about 250 ms) wrote 2.0 times fewer pages. A waiting caller (`write`, `call`) still commits at once.
pub const NOTE_GROUP: Duration = Duration::from_millis(250);

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
    /// Live transactions committed so far.
    groups: AtomicU64,
    /// Set by the checkpointer: the WAL is past its cap and its checkpoint copied every frame it saw, so the writer
    /// copies the few frames committed meanwhile, between two groups, and its next write restarts the WAL (`restart_wal`).
    restart: AtomicBool,
    /// WAL frames past which the checkpointer asks for a restart (`RESTART_AT` in pages), and the least time between two
    /// restarts in milliseconds (`RESTART_EVERY`); tests lower both.
    restart_frames: AtomicU64,
    restart_every_ms: AtomicU64,
    /// WAL frames the checkpointer's last full copy covered, and the WAL's frames after the writer's last commit.
    copied_frames: AtomicU64,
    wal_frames: AtomicU64,
    /// Callers waiting in `settle`: notes stop gathering for them.
    settling: AtomicUsize,
    /// `NOTE_GROUP` in milliseconds (tests set 0 to measure the writer as before).
    note_group_ms: AtomicU64,
    /// The writer's current step, and the longest since it fell behind.
    doing: Mutex<Option<Doing>>,
    longest: Mutex<Option<Longest>>,
    /// WAL pages each writer site committed since the checkpointer's last WAL line, and how many of its jobs: the line
    /// names them (13 Nov: the WAL crossed its cap 41 times in 10 min and nothing said which writes filled it).
    written: Mutex<std::collections::HashMap<Site, (u64, u64)>>,
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
    /// Set while this thread holds the store's writer aside (`Queue::turn`).
    static TURN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Set while this thread starts a writer under the registry lock (`Queue::existing`).
    static STARTING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
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
/// True while this thread holds the store's writer aside (inside `Queue::turn`).
pub fn in_turn() -> bool {
    TURN.with(|t| t.get())
}
/// Ends a turn: the parked writer goes on when `_release` drops, even if the turn's body panicked.
struct TurnEnd {
    _release: crossbeam_channel::Sender<()>,
}
impl Drop for TurnEnd {
    fn drop(&mut self) {
        TURN.with(|t| t.set(false));
    }
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
        STARTING.with(|s| s.set(true));
        let db = crate::store::open(root);
        STARTING.with(|s| s.set(false));
        let db = db?;
        // Opened on the caller's thread: a compactor thread's Quit interrupt must not reach the writer's statements.
        db.progress_handler(0, None::<fn() -> bool>)?;
        db.busy_timeout(BUSY)?;
        db.execute_batch("PRAGMA wal_autocheckpoint=0")?;
        // Every live job is a savepoint of its group, so SQLite keeps each page a job first changes in a sub-journal,
        // a temp file at `temp_store=FILE`: as many bytes again as the WAL took, written to disk and thrown away at
        // commit (log sweep F1, 9 Nov). In memory it is bounded by one group's changed pages.
        db.execute_batch("PRAGMA temp_store=MEMORY")?;
        db.wal_hook(Some(note_wal));
        STARTING.with(|s| s.set(true));
        let checkpoints = crate::store::open(root);
        STARTING.with(|s| s.set(false));
        let checkpoints = checkpoints?;
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
            groups: AtomicU64::new(0),
            restart: AtomicBool::new(false),
            restart_frames: AtomicU64::new(RESTART_AT / page_size(&db)),
            restart_every_ms: AtomicU64::new(RESTART_EVERY.as_millis() as u64),
            note_group_ms: AtomicU64::new(NOTE_GROUP.as_millis() as u64),
            copied_frames: AtomicU64::new(0),
            wal_frames: AtomicU64::new(0),
            settling: AtomicUsize::new(0),
            doing: Mutex::new(None),
            longest: Mutex::new(None),
            written: Mutex::new(Default::default()),
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
        anyhow::ensure!(!in_turn(), "an archive write handed over while its thread holds the writer aside");
        let (done, answer) = crossbeam_channel::bounded(1);
        let job = Background { site, job: Box::new(move |db: &Connection| {
            let _ = done.send(guarded(|| background_once(db, f)));
        }) };
        self.inner.background.as_ref().context("archive writer stopped")?.send(job).map_err(|_| anyhow!("archive writer stopped"))?;
        answer.recv().map_err(|_| anyhow!("archive writer stopped"))?
    }
    /// The store's writer of `root` if one runs in this process (`shared` would start one). `None` on a thread that
    /// is starting a writer: `shared` opens the index under the registry lock, and that open's schema upgrade runs a
    /// projection transaction that asks here.
    pub fn existing(root: &Path) -> Option<Self> {
        if STARTING.with(|s| s.get()) {
            return None;
        }
        let key = canonical(root);
        let queues = registry().lock().unwrap();
        let found = queues.iter().filter_map(Weak::upgrade).find(|q| q.shared.root == key);
        found.map(|inner| Self { inner })
    }
    /// Runs `f` on the caller's thread and connection while the store's writer stands aside: `f` starts in a
    /// background slot (once no live job waits, or once per `BACKGROUND_EVERY` between live groups) and the writer
    /// waits until it returns. For maintenance that writes on its own connection (the timeline backfill): SQLite
    /// queues nobody for its write lock, so such a writer polling `store::BUSY` against a writer that takes the lock
    /// again microseconds after each commit lost every poll and failed with "database is locked" (night 10-11 Nov,
    /// `tests/contention.rs`). Inside a turn only another process can hold the lock. `f` must not hand a write to
    /// this writer (it would wait for itself): that is refused. Keep turns to one bounded step; end one early once
    /// `backlog` says live jobs have waited.
    #[track_caller]
    pub fn turn<T>(&self, f: impl FnOnce() -> Result<T>) -> Result<T> {
        if on_writer() || in_turn() {
            return f();
        }
        let site = std::panic::Location::caller();
        let (started, start) = crossbeam_channel::bounded::<()>(1);
        let (release, released) = crossbeam_channel::bounded::<()>(1);
        let job = Background { site, job: Box::new(move |_db: &Connection| {
            if started.send(()).is_ok() {
                // Disconnected when the turn ends.
                let _ = released.recv();
            }
        }) };
        self.inner.background.as_ref().context("archive writer stopped")?.send(job).map_err(|_| anyhow!("archive writer stopped"))?;
        start.recv().map_err(|_| anyhow!("archive writer stopped"))?;
        TURN.with(|t| t.set(true));
        let _end = TurnEnd { _release: release };
        f()
    }
    /// False when the job was dropped (too many pending).
    fn submit(&self, q: Queued) -> Result<bool> {
        anyhow::ensure!(!in_turn(), "an archive write handed over while its thread holds the writer aside");
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
    /// Live transactions (groups, and jobs run alone) committed so far.
    pub fn groups(&self) -> u64 {
        self.inner.shared.groups.load(Ordering::Relaxed)
    }
    /// Sets how long notes gather (`NOTE_GROUP`); labs set zero to measure the writer as before.
    #[doc(hidden)]
    pub fn set_note_group(&self, group: Duration) {
        self.inner.shared.note_group_ms.store(group.as_millis() as u64, Ordering::Relaxed);
    }
    /// Notes queued and not yet committed.
    pub fn notes(&self) -> usize {
        self.inner.shared.notes.load(Ordering::Relaxed)
    }
    /// Waits until every queued write has been committed, or `timeout` (at once on the writer: it would wait for
    /// itself). True when nothing is left.
    /// The writers that filled the WAL since the last WAL line, most first (lab measurements; the checkpointer says them).
    #[doc(hidden)]
    pub fn wal_writers(&self) -> Option<String> {
        self.inner.shared.writers(4096)
    }
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
    /// The writers that filled the WAL since the last call, most first: "chunks.rs:491 520 MB in 30 jobs, ...".
    fn writers(&self, page: u64) -> Option<String> {
        let mut by_site: Vec<(Site, (u64, u64))> = self.written.lock().unwrap().drain().collect();
        by_site.sort_by(|a, b| b.1.0.cmp(&a.1.0));
        let named: Vec<String> = by_site
            .iter()
            .take(3)
            .filter(|(_, (pages, _))| *pages > 0)
            .map(|(site, (pages, jobs))| format!("{}:{} {} MB in {jobs} jobs", site.file(), site.line(), (pages * page) >> 20))
            .collect();
        (!named.is_empty()).then(|| named.join(", "))
    }
    /// Wakes the checkpointer now (the writer left a restart's tail to it).
    fn nudge_checkpointer(&self) {
        let mut commits = self.commits.0.lock().unwrap();
        *commits = commits.wrapping_add(1).min(u64::MAX - 1);
        self.commits.1.notify_all();
    }
    /// After a commit by `sites`' jobs: counts the pages it wrote to them and wakes the checkpointer once the WAL holds
    /// `CHECKPOINT_PAGES`.
    fn committed(&self, db: &Connection, sites: &[Site]) {
        let written = pages_written(db);
        if written > 0 && !sites.is_empty() {
            let mut by_site = self.written.lock().unwrap();
            // A group's pages are shared out evenly among its jobs.
            let share = written / sites.len() as u64;
            for site in sites {
                let entry = by_site.entry(*site).or_default();
                entry.0 += share;
                entry.1 += 1;
            }
        }
        let pages = WAL_PAGES.with(|p| p.replace(0));
        if pages > 0 {
            self.wal_frames.store(pages as u64, Ordering::Release);
        }
        if pages >= CHECKPOINT_PAGES {
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
    let mut catch_ups = CatchUps::default();
    let mut last_background = Instant::now();
    let mut last_restart: Option<Instant> = None;
    let mut last_group = Instant::now();
    let mut skipped = 0u32;
    loop {
        let every = Duration::from_millis(state.restart_every_ms.load(Ordering::Relaxed));
        if db.is_autocommit() && last_restart.is_none_or(|t| t.elapsed() >= every) && state.restart.swap(false, Ordering::AcqRel) {
            if restart_now(state.wal_frames.load(Ordering::Acquire), state.copied_frames.load(Ordering::Acquire), skipped) {
                skipped = 0;
                if !state.step("a WAL restart checkpoint", None, || restart_wal(db)) {
                    state.restart.store(true, Ordering::Release);
                }
                last_restart = Some(Instant::now());
            } else {
                // The checkpointer copies the tail on its own thread now; its next full copy asks again.
                skipped += 1;
                state.nudge_checkpointer();
            }
        }
        while jobs.len() < BATCH_BEHIND {
            match live.try_recv() {
                Ok(job) => jobs.push_back(job),
                Err(_) => break,
            }
        }
        if !jobs.is_empty() && last_background.elapsed() >= BACKGROUND_EVERY {
            if let Ok(job) = background.try_recv() {
                let site = job.site;
                state.step("a background job", Some(site), || (job.job)(db));
                end_open(db);
                state.committed(db, &[site]);
                last_background = Instant::now();
                continue;
            }
        }
        if jobs.is_empty() {
            // Nothing live: one background job, else wait for either lane.
            if let Ok(job) = background.try_recv() {
                let site = job.site;
                state.step("a background job", Some(site), || (job.job)(db));
                end_open(db);
                state.committed(db, &[site]);
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
                        let site = job.site;
                        state.step("a background job", Some(site), || (job.job)(db));
                        end_open(db);
                        state.committed(db, &[site]);
                        last_background = Instant::now();
                    }
                    continue;
                },
            }
            continue;
        }
        if !isolate {
            gather(live, background, &state.settling, &mut jobs, last_group + Duration::from_millis(state.note_group_ms.load(Ordering::Relaxed)));
        }
        // Behind, a group takes up to `BATCH_BEHIND` jobs; past the first `BATCH_JOBS` only those prepared within
        // `BATCH_TIME` (a group stops at the first job still unprepared).
        let cap = if jobs.len() > BATCH_JOBS { BATCH_BEHIND } else { BATCH_JOBS };
        let preparing = Instant::now();
        for (i, q) in jobs.iter_mut().take(cap).enumerate() {
            if i >= BATCH_JOBS && preparing.elapsed() >= BATCH_TIME {
                break;
            }
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
        let ran = if isolate { alone(db, &mut jobs, state) } else { group(db, &mut jobs, cap, state) };
        let full = ran.finished.len() >= BATCH_JOBS || group_started.elapsed() >= BATCH_TIME;
        isolate = false;
        if !ran.finished.is_empty() {
            last_group = Instant::now();
            state.groups.fetch_add(1, Ordering::Relaxed);
            let sites: Vec<Site> = ran.finished.iter().map(|(q, _)| q.site).collect();
            state.committed(db, &sites);
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
                let waited = waiting.take().is_some();
                if waited || at.elapsed() >= Duration::from_secs(1) {
                    // The longest step names what held the writer (its own work; another process's lock shows as the
                    // "wait" line and as a long BEGIN).
                    let held = longest.filter(|l| l.took >= SLOW_STEP).map(|l| format!("; longest step {:.1} s: {}", l.took.as_secs_f64(), named(l.what, l.site))).unwrap_or_default();
                    if let Some(also) = catch_ups.said(at.elapsed(), waited, Instant::now()) {
                        eprintln!("Archive live writes caught up: {n} queued, {:.1} s behind the held lock{held}{also}", at.elapsed().as_secs_f64());
                    }
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

/// While every queued job is a note (nobody waits on it), takes the notes that arrive until `until` (`NOTE_GROUP` after
/// the previous commit), `BATCH_JOBS` of them, a job a caller waits on, a background job or a `settle` caller: one
/// transaction then carries them all.
fn gather(live: &crossbeam_channel::Receiver<Queued>, background: &crossbeam_channel::Receiver<Background>, settling: &AtomicUsize, jobs: &mut VecDeque<Queued>, until: Instant) {
    while jobs.len() < BATCH_JOBS && jobs.iter().all(|q| q.done.is_none()) && background.is_empty() && settling.load(Ordering::Acquire) == 0 {
        let Some(left) = until.checked_duration_since(Instant::now()).filter(|d| !d.is_zero()) else { return };
        match live.recv_timeout(left.min(GATHER_STEP)) {
            Ok(job) => jobs.push_back(job),
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return,
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

/// The writer's restart checkpoint copies only the frames committed since the checkpointer's full copy: at most
/// `RESTART_COPY` (a WAL that restarted since has fewer frames than were copied: nothing left to copy), unless it
/// skipped `RESTART_SKIPS` times in a row.
fn restart_now(wal_frames: u64, copied_frames: u64, skipped: u32) -> bool {
    wal_frames.saturating_sub(copied_frames) <= RESTART_COPY || skipped >= RESTART_SKIPS
}

/// Pages this connection wrote since the last call (SQLite's `SQLITE_DBSTATUS_CACHE_WRITE`, reset on read): in WAL
/// mode, the frames its commits appended.
fn pages_written(db: &Connection) -> u64 {
    let (mut current, mut high) = (0, 0);
    // SAFETY: the handle is this open connection's, used on its own thread; the call only reads and resets a counter.
    let ok = unsafe { rusqlite::ffi::sqlite3_db_status(db.handle(), rusqlite::ffi::SQLITE_DBSTATUS_CACHE_WRITE, &mut current, &mut high, 1) };
    if ok == rusqlite::ffi::SQLITE_OK { current.max(0) as u64 } else { 0 }
}
/// A background job that panicked mid-transaction leaves it open: rolled back before the next group.
fn end_open(db: &Connection) {
    if !db.is_autocommit() {
        let _ = db.execute_batch("ROLLBACK");
    }
}

/// Checkpoints the WAL (PASSIVE: never blocks a reader or the writer) once the writer's commits filled
/// `CHECKPOINT_PAGES`, at most once a second.
/// Every few seconds it also names a writer step that has run `HELD_REPORT` (`Shared::report_held`) and a read
/// transaction open `readers::LONG_READ` (`readers::report_long`).
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
        crate::readers::report_long();
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
                        shared.copied_frames.store(log as u64, Ordering::Release);
                        shared.restart.store(true, Ordering::Release);
                    }
                }
            }
            if let (Ok((log, copied)), Some(path)) = (pages, db.path().filter(|p| !p.is_empty())) {
                let bytes = std::fs::metadata(format!("{path}-wal")).map_or(0, |m| m.len());
                if let Some(mut line) = wal.look(bytes, log, copied, Instant::now()) {
                    if let Some(writers) = state.upgrade().and_then(|s| s.writers(page_size(db))) {
                        line = format!("{line}; written since the last line by {writers}");
                    }
                    if (0..log).contains(&copied) {
                        // A reader cut it short: name who may hold the old snapshot.
                        eprintln!("{line} ({})", crate::readers::suspects(path));
                    } else {
                        eprintln!("{line}");
                    }
                }
            }
            seen = now;
            std::thread::sleep(Duration::from_secs(1));
        }
    }
}

/// A catch-up shorter than this, after no wait for another process, is counted rather than logged on its own.
pub(crate) const SHORT_CATCH_UP: Duration = Duration::from_secs(5);
/// The least time between two lines about short catch-ups alone.
pub(crate) const CATCH_UP_REPEAT: Duration = Duration::from_secs(10 * 60);
/// Which "caught up" lines are said. 3-5 Nov: 489 in two days, half of them under 1.8 s. A catch-up of
/// `SHORT_CATCH_UP` or more, or after a wait for another process, is always said; a short one is said when none was
/// for `CATCH_UP_REPEAT`, and otherwise counted into the next line said.
#[derive(Default)]
pub(crate) struct CatchUps {
    last_line: Option<Instant>,
    folded: u32,
    folded_time: Duration,
}
impl CatchUps {
    /// `Some(suffix)` when this catch-up is said: the short ones folded since the last line, if any.
    pub(crate) fn said(&mut self, took: Duration, waited: bool, now: Instant) -> Option<String> {
        let quiet = self.last_line.is_none_or(|t| now.duration_since(t) >= CATCH_UP_REPEAT);
        if took < SHORT_CATCH_UP && !waited && !quiet {
            self.folded += 1;
            self.folded_time += took;
            return None;
        }
        self.last_line = Some(now);
        let n = std::mem::take(&mut self.folded);
        let time = std::mem::take(&mut self.folded_time);
        Some(if n > 0 { format!(" (+{n} shorter catch-ups since the last line, {:.0} s in all)", time.as_secs_f64()) } else { String::new() })
    }
}

/// The WAL past `WAL_CAP` (2 Nov 15:55: 991 MB against the 64 MB cap, nothing in the log said why). SQLite truncates
/// it to the cap only when a checkpoint copied every page and the next write restarts it; a reader holding an old
/// snapshot stops both, and one large transaction fills it at once. Said when it first passes the cap and once per
/// doubling. It is back only after `WAL_SETTLE` under the cap: 3-5 Nov it rose to 65-68 MB and was cut back to 64 MB
/// every 12 s, two lines each time (7,208 lines in two days, 40% of the log). Meanwhile each rise past the cap is
/// counted and the count said at most once per `WAL_REPEAT`.
#[derive(Default)]
pub(crate) struct WalWatch {
    /// The size last said past the cap; 0 while the WAL is (settled) under it.
    said: u64,
    /// The last look found it past the cap.
    over: bool,
    /// Under the cap since then, while `said` is set.
    under_since: Option<Instant>,
    /// When the last line was said.
    last_line: Option<Instant>,
    /// Rises past the cap since the last line.
    crossings: u64,
}
/// How long the WAL stays under its cap before it is said to be back.
pub(crate) const WAL_SETTLE: Duration = Duration::from_secs(10 * 60);
/// The least time between two lines about a WAL that keeps crossing its cap.
pub(crate) const WAL_REPEAT: Duration = Duration::from_secs(10 * 60);
impl WalWatch {
    /// `bytes`: the WAL file's size; `log`, `copied`: the pages in it and the pages the checkpoint copied.
    pub(crate) fn look(&mut self, bytes: u64, log: i64, copied: i64, now: Instant) -> Option<String> {
        if bytes <= WAL_CAP {
            self.over = false;
            if self.said == 0 {
                return None;
            }
            let since = *self.under_since.get_or_insert(now);
            if now.duration_since(since) < WAL_SETTLE {
                return None;
            }
            let more = std::mem::take(&mut self.crossings);
            *self = Self::default();
            let more = if more > 0 { format!(" (it crossed the cap {more} more times after the last line)") } else { String::new() };
            return Some(format!("Archive WAL back under its cap: {} MB{more}", bytes >> 20));
        }
        self.under_since = None;
        let rose = !std::mem::replace(&mut self.over, true);
        let why = if log < 0 || copied < 0 {
            "the checkpoint could not run".to_string()
        } else if copied < log {
            format!("the checkpoint copied {copied} of {log} pages: a reader holds an old snapshot, so the WAL cannot restart")
        } else {
            format!("every page copied ({log}); it is truncated at the next write that finds no reader on it")
        };
        if self.said == 0 || bytes >= self.said * 2 {
            self.said = bytes;
            self.last_line = Some(now);
            self.crossings = 0;
            return Some(format!("Archive WAL at {} MB, past its {} MB cap: {why}", bytes >> 20, WAL_CAP >> 20));
        }
        self.crossings += u64::from(rose);
        let quiet = self.last_line.is_none_or(|t| now.duration_since(t) >= WAL_REPEAT);
        if self.crossings == 0 || !quiet {
            return None;
        }
        let minutes = self.last_line.map_or(0, |t| now.duration_since(t).as_secs() / 60);
        let n = std::mem::take(&mut self.crossings);
        self.last_line = Some(now);
        Some(format!("Archive WAL crossed its {} MB cap {n} times in {minutes} min, now {} MB: {why}", WAL_CAP >> 20, bytes >> 20))
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
fn group(db: &Connection, jobs: &mut VecDeque<Queued>, cap: usize, state: &Shared) -> Ran {
    let durable = jobs.iter().take(cap).any(|q| q.durable);
    if !durable && db.execute_batch("PRAGMA synchronous=NORMAL").is_err() {
        return alone(db, jobs, state);
    }
    let ran = group_at(db, jobs, cap, state);
    if !durable {
        let _ = db.execute_batch("PRAGMA synchronous=FULL");
    }
    ran
}

fn group_at(db: &Connection, jobs: &mut VecDeque<Queued>, cap: usize, state: &Shared) -> Ran {
    if let Err(e) = state.step("taking the write lock", None, || db.execute_batch("BEGIN IMMEDIATE")) {
        return Ran { finished: vec![], retry: Some(e.into()) };
    }
    let started = Instant::now();
    let (mut ran, mut failed, mut solo) = (0usize, vec![], false);
    while ran < jobs.len() && ran < cap && (ran == 0 || (started.elapsed() < BATCH_TIME && jobs[ran].prepare.is_none())) {
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
    // On the writer, or holding it aside in a turn, nothing queued can finish while this waits.
    if on_writer() || in_turn() {
        return reached();
    }
    // Notes gathering for `NOTE_GROUP` commit at once for a caller waiting here.
    shared.settling.fetch_add(1, Ordering::AcqRel);
    let (_pending, _) = shared.settled.wait_timeout_while(pending, timeout, |_| !reached()).unwrap();
    shared.settling.fetch_sub(1, Ordering::AcqRel);
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
    /// 13 Nov: the WAL crossed its cap 41 times in 10 min and the log did not say which writes filled it. The pages
    /// each writer site commits are counted (a group's shared among its jobs) and the WAL line names the largest.
    #[test]
    fn the_wal_line_names_the_writers_that_filled_it() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("archive");
        crate::Journal::open(&crate::store::session_path(&root, "session-writers")).unwrap();
        let queue = Queue::shared(&root).unwrap();
        queue.call(|db| Ok(db.execute_batch("CREATE TABLE IF NOT EXISTS writers_test(id INTEGER PRIMARY KEY, data BLOB)")?)).unwrap();
        queue.inner.shared.written.lock().unwrap().clear();
        queue
            .background(|db| {
                for i in 0..128i64 {
                    db.execute("INSERT INTO writers_test VALUES(?1,?2)", rusqlite::params![i, vec![i as u8; 65_536]])?;
                }
                Ok(())
            })
            .unwrap();
        for i in 0..4i64 {
            queue.call(move |db| Ok(db.execute("INSERT INTO writers_test VALUES(?1,X'00')", [1000 + i]).map(|_| ())?)).unwrap();
        }
        let line = queue.inner.shared.writers(4096).unwrap();
        let first = line.split(", ").next().unwrap();
        assert!(first.starts_with("crates/archive/src/live.rs:") && first.ends_with(" in 1 jobs"), "the background job wrote the most: {line}");
        let mb: u64 = first.split(' ').nth(1).unwrap().parse().unwrap();
        assert!((8..=9).contains(&mb), "8 MB of rows and their pages: {line}");
        assert!(line.contains(" in 4 jobs"), "the four small writes are counted too: {line}");
        assert!(queue.inner.shared.written.lock().unwrap().is_empty(), "said once, then counted afresh");
    }
    /// The WAL restarts under continuous writes (3-4 Nov: it grew to 4.2 GB because every checkpoint raced a commit).
    /// The threshold is lowered to 1,000 frames (4 MB) here; 48 MB is written in 64 KB rows, committed back to back.
    #[test]
    fn the_wal_restarts_under_continuous_writes() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("archive");
        crate::Journal::open(&crate::store::session_path(&root, "session-wal")).unwrap();
        let queue = Queue::shared(&root).unwrap();
        // `HEIMDALL_WAL_TEST_UNFIXED=1`: the writer never asked to restart, as before the fix (measured 59 MB vs 10 MB).
        assert_eq!(queue.inner.shared.restart_frames.load(Ordering::Relaxed), RESTART_AT / 4096, "a restart is asked for at half the cap");
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
    /// Log sweep F2 (6-9 Nov): the embedding index's pending selection scanned every chunk in one statement, 1-13 s,
    /// again and again; its snapshot kept the WAL from restarting (620 MB at worst). A reader that scans the timeline
    /// in slices (`timeline::by_slices`) holds a snapshot one slice long, so under continuous writes the WAL still
    /// restarts. `HEIMDALL_WAL_READER_UNFIXED=1`: the same scan in one read transaction, as before.
    #[test]
    fn a_reader_scanning_in_slices_lets_the_wal_restart() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("archive");
        let queue = Queue::shared(&root).unwrap();
        queue.inner.shared.restart_frames.store(1000, Ordering::Relaxed);
        queue.inner.shared.restart_every_ms.store(100, Ordering::Relaxed);
        let chunks = 8 * crate::timeline::SLICE;
        queue.call(move |db| {
            db.execute_batch("CREATE TABLE IF NOT EXISTS wal_test(id INTEGER PRIMARY KEY, data BLOB)")?;
            for i in 0..chunks {
                db.execute("INSERT INTO main.timeline_chunks VALUES(?1,1,'text','utterance','mic',0,0,?2,?2,'','{}','h')", rusqlite::params![format!("c{i}"), i * 1000])?;
            }
            Ok(())
        }).unwrap();
        let unfixed = std::env::var_os("HEIMDALL_WAL_READER_UNFIXED").is_some();
        let stop = Arc::new(AtomicBool::new(false));
        let (reader_root, reader_stop) = (root.clone(), stop.clone());
        // Each slice takes 40 ms of "work", so one pass over the timeline takes about a third of a second.
        let reader = std::thread::spawn(move || {
            let db = crate::store::read_index(&reader_root).unwrap();
            let mut passes = 0;
            while !reader_stop.load(Ordering::Acquire) {
                if unfixed {
                    db.execute_batch("BEGIN").unwrap();
                }
                crate::timeline::by_slices(&db, i64::MIN, |lo, hi| {
                    db.query_row("SELECT count(*) FROM main.timeline_chunks WHERE start_wall_ns>=?1 AND start_wall_ns<?2", [lo, hi], |r| r.get::<_, i64>(0))?;
                    std::thread::sleep(Duration::from_millis(40));
                    Ok(true)
                }).unwrap();
                if unfixed {
                    db.execute_batch("COMMIT").unwrap();
                }
                passes += 1;
            }
            passes
        });
        let wal = root.join(format!("{}-wal", crate::store::INDEX));
        let mut largest = 0u64;
        for i in 0..640usize {
            let data: Vec<u8> = (0..65_536u32).map(|k| (k.wrapping_mul(2_654_435_761) >> 13) as u8 ^ i as u8).collect();
            queue.enqueue(move |db| Ok(db.execute("INSERT OR REPLACE INTO wal_test VALUES(?1,?2)", rusqlite::params![i as i64, data]).map(|_| ())?)).unwrap();
            std::thread::sleep(Duration::from_millis(5));
            if i % 16 == 0 {
                largest = largest.max(std::fs::metadata(&wal).map_or(0, |m| m.len()));
            }
        }
        assert!(queue.settle(Duration::from_secs(60)));
        stop.store(true, Ordering::Release);
        let passes = reader.join().unwrap();
        largest = largest.max(std::fs::metadata(&wal).map_or(0, |m| m.len()));
        eprintln!("40 MB written beside {passes} reader passes; largest WAL {} MB", largest >> 20);
        assert!(passes >= 3, "the reader kept scanning ({passes} passes)");
        assert!(largest < 24 << 20, "the WAL restarted between the reader's slices: largest {} MB", largest >> 20);
    }
    /// The writer's restart checkpoint copies at most `RESTART_COPY` frames; a larger backlog stays with the
    /// checkpointer (9 Nov 16:39: the writer held 7.3 s by one).
    #[test]
    fn the_writers_restart_checkpoint_copies_only_a_small_tail() {
        assert!(restart_now(1000, 1000, 0));
        assert!(restart_now(1000 + RESTART_COPY, 1000, 0));
        assert!(!restart_now(1001 + RESTART_COPY, 1000, 0));
        assert!(!restart_now(140_660, 43_045, 0), "16:39-16:40: 97,615 frames piled up after the full copy");
        assert!(restart_now(12, 43_045, 0), "the WAL restarted since: nothing left to copy");
        // Review: under a long burst every full copy may leave a large tail; the WAL still restarts.
        assert!(!restart_now(140_660, 43_045, RESTART_SKIPS - 1));
        assert!(restart_now(140_660, 43_045, RESTART_SKIPS), "after {RESTART_SKIPS} skips in a row it copies the tail anyway");
    }
    /// Log sweep F1 (9 Nov): VAD notes arrived about 62 a second and nearly each committed alone, so the index WAL took
    /// about 760 pages a second for 0.27 MB a second of content. Notes gather for `NOTE_GROUP`; a caller that waits
    /// still commits at once. `HEIMDALL_NOTE_GROUP_UNFIXED=1` measures the writer as before.
    #[test]
    fn notes_gather_into_a_few_commits_a_second_and_a_waiting_call_is_not_held() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("archive");
        let queue = Queue::shared(&root).unwrap();
        if std::env::var_os("HEIMDALL_NOTE_GROUP_UNFIXED").is_some() {
            queue.inner.shared.note_group_ms.store(0, Ordering::Relaxed);
        }
        queue.call(|db| Ok(db.execute_batch("CREATE TABLE IF NOT EXISTS notes_test(id INTEGER PRIMARY KEY AUTOINCREMENT, body BLOB)")?)).unwrap();
        let groups = queue.groups();
        let started = Instant::now();
        let mut waited = Duration::ZERO;
        // Two seconds of notes at 124 a second; every half second a caller waits for its own write.
        for i in 0..248u32 {
            queue.enqueue(move |db| Ok(db.execute("INSERT INTO notes_test(body) VALUES(randomblob(200))", []).map(|_| ())?)).unwrap();
            if i % 62 == 31 {
                let asked = Instant::now();
                queue.call(|db| Ok(db.execute("INSERT INTO notes_test(body) VALUES(randomblob(200))", []).map(|_| ())?)).unwrap();
                waited = waited.max(asked.elapsed());
            }
            std::thread::sleep(Duration::from_millis(8));
        }
        assert!(queue.settle(Duration::from_secs(10)));
        let commits = queue.groups() - groups;
        let seconds = started.elapsed().as_secs_f64();
        eprintln!("252 writes in {seconds:.1} s: {commits} commits; longest waiting call {waited:?}");
        let n: i64 = queue.call(|db| Ok(db.query_row("SELECT count(*) FROM notes_test", [], |r| r.get(0))?)).unwrap();
        assert_eq!(n, 252, "every note kept");
        assert!(commits as f64 <= seconds / NOTE_GROUP.as_secs_f64() + 8.0, "{commits} commits in {seconds:.1} s: notes gather for {NOTE_GROUP:?}");
        assert!(waited < NOTE_GROUP * 2, "a waiting caller commits at once ({waited:?})");
    }
    /// Review of the gathering: a background job (compaction publishing, collection) and a `settle` caller are not
    /// held for the rest of a gather while notes keep arriving.
    #[test]
    fn a_background_job_and_a_settle_do_not_wait_for_gathering_notes() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("archive");
        let queue = Queue::shared(&root).unwrap();
        queue.set_note_group(Duration::from_secs(5));
        queue.call(|db| Ok(db.execute_batch("CREATE TABLE IF NOT EXISTS gather_test(id INTEGER PRIMARY KEY AUTOINCREMENT, body BLOB)")?)).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let (notes, notes_stop) = (queue.clone(), stop.clone());
        let feeder = std::thread::spawn(move || {
            while !notes_stop.load(Ordering::Acquire) {
                notes.enqueue(|db| Ok(db.execute("INSERT INTO gather_test(body) VALUES(randomblob(200))", []).map(|_| ())?)).unwrap();
                std::thread::sleep(Duration::from_millis(8));
            }
        });
        std::thread::sleep(Duration::from_millis(300));
        let asked = Instant::now();
        let n: i64 = queue.background(|db| Ok(db.query_row("SELECT count(*) FROM gather_test", [], |r| r.get(0))?)).unwrap();
        let background = asked.elapsed();
        let asked = Instant::now();
        let settled = queue.settle(Duration::from_secs(4));
        let settle = asked.elapsed();
        stop.store(true, Ordering::Release);
        feeder.join().unwrap();
        eprintln!("background job after {background:?} ({n} rows), settle after {settle:?}");
        assert!(background < Duration::from_secs(1), "a background job waited {background:?} for a 5 s gather");
        assert!(settled && settle < Duration::from_secs(1), "settle waited {settle:?} for a 5 s gather");
    }
    /// 2 Nov 15:55: the WAL at 991 MB against the 64 MB cap, and nothing said why. Past the cap the checkpointer says
    /// whether a reader stopped the checkpoint, once per doubling, and once when it is back.
    #[test]
    fn a_wal_past_its_cap_is_explained_once_per_doubling() {
        let t = Instant::now();
        let mut w = WalWatch::default();
        assert_eq!(w.look(10 << 20, 100, 100, t), None, "under the cap: nothing");
        let line = w.look(991 << 20, 250_000, 1_000, t).unwrap();
        assert!(line.starts_with("Archive WAL at 991 MB, past its 64 MB cap") && line.contains("a reader holds an old snapshot"), "{line}");
        assert_eq!(w.look(1500 << 20, 300_000, 1_000, t), None, "not again until it doubles");
        assert!(w.look(2000 << 20, 500_000, 500_000, t).unwrap().contains("every page copied"));
        assert_eq!(w.look(64 << 20, 0, 0, t), None, "not back until it settles");
        assert_eq!(w.look(64 << 20, 0, 0, t + WAL_SETTLE).as_deref(), Some("Archive WAL back under its cap: 64 MB"));
        assert_eq!(w.look(1 << 20, 0, 0, t + WAL_SETTLE * 2), None);
    }
    /// Bug 2026-11-05T02-53-27-3 (log sweep): short "caught up" lines are counted, long ones always said.
    #[test]
    fn short_catch_ups_are_counted_into_the_next_line() {
        let t = Instant::now();
        let mut c = CatchUps::default();
        let s = Duration::from_secs;
        assert_eq!(c.said(s(2), false, t).as_deref(), Some(""), "the first is said");
        for i in 1..=30 {
            assert_eq!(c.said(s(2), false, t + s(10 * i)), None, "short ones within the repeat are counted");
        }
        assert_eq!(c.said(s(1), true, t + s(301)).as_deref(), Some(" (+30 shorter catch-ups since the last line, 60 s in all)"), "a wait for another process is said");
        assert_eq!(c.said(s(2), false, t + s(302)), None);
        assert_eq!(c.said(s(41), false, t + s(303)).as_deref(), Some(" (+1 shorter catch-ups since the last line, 2 s in all)"), "a long one is said");
        assert_eq!(c.said(s(3), false, t + s(303) + CATCH_UP_REPEAT).as_deref(), Some(""), "a short one after a quiet spell is said");
    }
    /// Bug 2026-11-05T02-53-27-3: 3-5 Nov the WAL rose to 65-68 MB and was cut back to 64 MB every 12 s, and each
    /// crossing logged two lines. A WAL oscillating around the cap is said once, then its crossings counted at most
    /// once per `WAL_REPEAT`, and it is back only after `WAL_SETTLE` under the cap.
    #[test]
    fn a_wal_oscillating_around_its_cap_is_said_once_then_counted() {
        let t = Instant::now();
        let mut w = WalWatch::default();
        let mut lines = vec![];
        // 9 minutes of a 12 s cycle: 1 s past the cap, 11 s back at it.
        for i in 0..45u64 {
            let at = t + Duration::from_secs(12 * i);
            lines.extend(w.look(66 << 20, 16_900, 16_900, at));
            lines.extend(w.look(64 << 20, 16_900, 16_900, at + Duration::from_secs(1)));
        }
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].starts_with("Archive WAL at 66 MB, past its 64 MB cap"), "{lines:?}");
        // Past `WAL_REPEAT` the crossings since are said once, with their count.
        let mut later = vec![];
        for i in 45..60u64 {
            let at = t + Duration::from_secs(12 * i);
            later.extend(w.look(67 << 20, 16_900, 16_900, at));
            later.extend(w.look(64 << 20, 16_900, 16_900, at + Duration::from_secs(1)));
        }
        assert_eq!(later.len(), 1, "{later:?}");
        assert!(later[0].starts_with("Archive WAL crossed its 64 MB cap 50 times in 10 min, now 67 MB"), "{later:?}");
        // Settled under the cap: back, with the crossings not yet said.
        let end = t + Duration::from_secs(12 * 60);
        assert_eq!(w.look(64 << 20, 0, 0, end), None);
        assert_eq!(w.look(64 << 20, 0, 0, end + WAL_SETTLE).as_deref(), Some("Archive WAL back under its cap: 64 MB (it crossed the cap 9 more times after the last line)"));
    }
}
