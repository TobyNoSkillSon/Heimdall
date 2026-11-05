//! The single archive store: one index database plus content packs.
//!
//! `heimdall.sqlite` holds every row and catalogue; `packs/hot.sqlite` holds hot content and
//! `packs/YYYY-MM.sqlite` cold content of one UTC month. Content is written and committed before
//! the index transaction that references it; content is deleted only through `gc_blobs`, under the
//! exclusive lifetime lock and the index write lock, when no catalogue row references it.
//!
//! Every index write in a process goes through the store's writer (`live.rs`). Helpers here that write (`ensure_session`,
//! `create_segment`, `mark_complete`, `archive_log`) hand themselves to it, unless their connection is already inside
//! a transaction (a writer job, a test fixture). Reads use any connection: a WAL snapshot never waits for a writer.
use crate::{hash, private_dir, wall_ns};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::Duration,
};

pub const INDEX: &str = "heimdall.sqlite";
pub const PACKS: &str = "packs";
pub const HOT: &str = "hot";
/// Orphaned blobs younger than this are never swept: a writer may not have published them yet.
pub const ORPHAN_GRACE_NS: u64 = 3_600_000_000_000;
/// How long a connection waits for the index or a pack before reporting it locked. Inside a process only the writer
/// writes the index, so this wait meets another process or a pack writer's short transaction.
pub const BUSY: Duration = Duration::from_secs(5);

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS sessions(id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE, started_wall_ns INTEGER NOT NULL, complete_wall_ns INTEGER, migrated_wall_ns INTEGER);
CREATE TABLE IF NOT EXISTS segments(id INTEGER PRIMARY KEY, session INTEGER NOT NULL, name TEXT NOT NULL, version INTEGER NOT NULL, wall_unix_ns INTEGER NOT NULL, host_origin_ns INTEGER NOT NULL, codec TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS segment_session ON segments(session);
CREATE TABLE IF NOT EXISTS sources(segment INTEGER NOT NULL, id TEXT NOT NULL, metadata_json TEXT NOT NULL, added_ns INTEGER NOT NULL, removed_ns INTEGER, PRIMARY KEY(segment, id)) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS gaps(id INTEGER PRIMARY KEY, segment INTEGER NOT NULL, source_id TEXT, host_ns INTEGER NOT NULL, detail TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS gap_segment ON gaps(segment, id);
CREATE TABLE IF NOT EXISTS blocks(id INTEGER PRIMARY KEY AUTOINCREMENT, segment INTEGER NOT NULL, source_id TEXT NOT NULL, host_ns INTEGER NOT NULL, rate INTEGER NOT NULL, channels INTEGER NOT NULL, frames INTEGER NOT NULL, codec TEXT NOT NULL, discontinuity INTEGER NOT NULL, object_id TEXT, offset_frames INTEGER NOT NULL DEFAULT 0, gain REAL NOT NULL DEFAULT 1, wall_ns INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS block_timeline ON blocks(segment, source_id, host_ns);
CREATE INDEX IF NOT EXISTS block_age ON blocks(codec, wall_ns);
CREATE INDEX IF NOT EXISTS block_object ON blocks(object_id);
CREATE INDEX IF NOT EXISTS block_cold ON blocks(segment, id) WHERE codec IN ('cold-ref','zero');
CREATE TABLE IF NOT EXISTS audio_objects(id TEXT PRIMARY KEY, rate INTEGER NOT NULL, channels INTEGER NOT NULL, frames INTEGER NOT NULL, codec TEXT NOT NULL, pack TEXT NOT NULL, stored_sha256 TEXT NOT NULL) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS audio_spans(source TEXT NOT NULL, start_ns INTEGER NOT NULL, end_ns INTEGER NOT NULL, object_id TEXT NOT NULL, quality INTEGER NOT NULL, fingerprint TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS span_fingerprint ON audio_spans(fingerprint, start_ns);
CREATE INDEX IF NOT EXISTS span_object ON audio_spans(object_id);
CREATE TABLE IF NOT EXISTS packed_audio_refs(object_id TEXT PRIMARY KEY) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS evidence(id INTEGER PRIMARY KEY AUTOINCREMENT, session INTEGER NOT NULL, kind TEXT NOT NULL, source TEXT NOT NULL, utterance_ids TEXT NOT NULL, host_ns INTEGER NOT NULL, wall_ns INTEGER NOT NULL, payload_id TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS evidence_session ON evidence(session, id);
CREATE INDEX IF NOT EXISTS evidence_time ON evidence(wall_ns);
CREATE INDEX IF NOT EXISTS evidence_kind_latest ON evidence(kind, id DESC);
CREATE INDEX IF NOT EXISTS evidence_payload ON evidence(payload_id, wall_ns);
CREATE TABLE IF NOT EXISTS payloads(id TEXT PRIMARY KEY, raw_bytes INTEGER NOT NULL, codec TEXT NOT NULL, data BLOB NOT NULL, chunk INTEGER) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS packed_payloads(id TEXT PRIMARY KEY, raw_bytes INTEGER NOT NULL, chunk INTEGER NOT NULL) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS vectors(id TEXT PRIMARY KEY, dimensions INTEGER NOT NULL, data BLOB NOT NULL);
CREATE TABLE IF NOT EXISTS images(id TEXT PRIMARY KEY, mime TEXT NOT NULL, pack TEXT NOT NULL, stored_sha256 TEXT NOT NULL, wall_ns INTEGER NOT NULL, cold_checked INTEGER NOT NULL DEFAULT 0);
CREATE INDEX IF NOT EXISTS image_age ON images(cold_checked, wall_ns);
CREATE TABLE IF NOT EXISTS audio_activity(session INTEGER NOT NULL, source TEXT NOT NULL, start_ns INTEGER NOT NULL, end_ns INTEGER NOT NULL, speech INTEGER NOT NULL, PRIMARY KEY(session, source, start_ns)) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS utterances(session INTEGER NOT NULL, id TEXT NOT NULL, wall_ms INTEGER, source_id TEXT, source TEXT, speaker TEXT, start_ns INTEGER, end_ns INTEGER, text TEXT, words TEXT, notes TEXT, decision TEXT, PRIMARY KEY(session, id));
CREATE INDEX IF NOT EXISTS utterance_time ON utterances(wall_ms);
CREATE INDEX IF NOT EXISTS utterance_span ON utterances(session, start_ns);
CREATE TABLE IF NOT EXISTS transcript_events(id INTEGER PRIMARY KEY, session INTEGER NOT NULL, kind TEXT, source_id TEXT, host_ns INTEGER, payload TEXT);
CREATE TABLE IF NOT EXISTS cold_chunks(id INTEGER PRIMARY KEY, kind TEXT NOT NULL, scope INTEGER NOT NULL, first_id INTEGER NOT NULL, last_id INTEGER NOT NULL, pack TEXT NOT NULL, raw_bytes INTEGER NOT NULL, sha256 TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS chunk_scope ON cold_chunks(kind, scope, first_id);
CREATE INDEX IF NOT EXISTS chunk_ids ON cold_chunks(kind, first_id, last_id);
CREATE INDEX IF NOT EXISTS chunk_hash ON cold_chunks(sha256);
CREATE TABLE IF NOT EXISTS cold_sources(chunk_id INTEGER NOT NULL, source TEXT NOT NULL, start_ns INTEGER NOT NULL, end_ns INTEGER NOT NULL, PRIMARY KEY(source, start_ns, chunk_id)) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS cold_source_end ON cold_sources(source, end_ns, start_ns);
CREATE TABLE IF NOT EXISTS logs(id INTEGER PRIMARY KEY, session INTEGER NOT NULL, name TEXT NOT NULL, raw_bytes INTEGER NOT NULL, sha256 TEXT NOT NULL, pack TEXT NOT NULL, wall_ns INTEGER NOT NULL, UNIQUE(session, name));
CREATE INDEX IF NOT EXISTS log_hash ON logs(sha256);
CREATE TABLE IF NOT EXISTS clips(id INTEGER PRIMARY KEY, object_id TEXT NOT NULL, origin TEXT NOT NULL, line_id TEXT, text TEXT NOT NULL, language TEXT NOT NULL, voice TEXT NOT NULL, generator TEXT NOT NULL, settings TEXT NOT NULL, sample_rate INTEGER NOT NULL, samples INTEGER NOT NULL, created_wall_ns INTEGER NOT NULL);
CREATE UNIQUE INDEX IF NOT EXISTS clip_identity ON clips(object_id, origin, ifnull(line_id,''), voice, generator);
CREATE INDEX IF NOT EXISTS clip_bank ON clips(voice, generator, line_id) WHERE origin='bank';
CREATE INDEX IF NOT EXISTS clip_object ON clips(object_id);
CREATE TABLE IF NOT EXISTS gc_blobs(pack TEXT NOT NULL, key TEXT NOT NULL, PRIMARY KEY(pack, key)) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS migrations(name TEXT PRIMARY KEY, session INTEGER, wall_ns INTEGER NOT NULL, receipt TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS store_meta(key TEXT PRIMARY KEY, value TEXT NOT NULL) WITHOUT ROWID;
";

/// `<archive>/<session-name>`: the name of a session inside the store, never a directory.
pub fn locate(session: &Path) -> Result<(PathBuf, String)> {
    let name = session
        .file_name()
        .and_then(|n| n.to_str())
        .context("archive session name")?;
    ensure!(
        name.starts_with("session")
            && name.len() <= 200
            && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "not an archive session address: {}",
        session.display()
    );
    let root = session.parent().context("archive root")?.to_path_buf();
    Ok((root, name.to_owned()))
}
pub fn session_path(root: &Path, name: &str) -> PathBuf {
    root.join(name)
}
/// True when `root` holds a store index.
pub fn exists(root: &Path) -> bool {
    root.join(INDEX).is_file()
}

fn initialized() -> &'static Mutex<HashSet<PathBuf>> {
    static INIT: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    INIT.get_or_init(|| Mutex::new(HashSet::new()))
}

thread_local! {
    /// Set on a compactor's thread (`interrupt_when`): every index connection it opens stops when the flag is set.
    static INTERRUPT: RefCell<Option<std::sync::Arc<std::sync::atomic::AtomicBool>>> = const { RefCell::new(None) };
}
/// From now on, index connections opened on this thread interrupt their running statement (it fails and its
/// transaction rolls back) as soon as `flag` is set. Compaction steps hold the shared locks for up to a minute;
/// Quit must not wait for one (24 Oct 02:31).
pub(crate) fn interrupt_when(flag: std::sync::Arc<std::sync::atomic::AtomicBool>) {
    INTERRUPT.with(|i| *i.borrow_mut() = Some(flag));
}
fn watch_interrupt(db: &Connection) -> Result<()> {
    if let Some(flag) = INTERRUPT.with(|i| i.borrow().clone()) {
        db.progress_handler(1000, Some(move || flag.load(std::sync::atomic::Ordering::Acquire)))?;
    }
    Ok(())
}
/// Per-connection settings for the index and packs. Commits flush fully (`synchronous=FULL` with `F_FULLFSYNC`), but
/// opening writes nothing: `PRAGMA auto_vacuum=…` is a write on an existing database, so it ran into the shared write
/// lock and a full flush on every open, and froze the live loop 2.0 s on 26 Oct 23:01 (tests/open_no_write.rs). It is
/// set only while the setting differs (a new file); `journal_mode=WAL` likewise only while the file isn't in WAL yet.
fn tune(db: &Connection) -> Result<()> {
    if db.query_row("PRAGMA auto_vacuum", [], |r| r.get::<_, i64>(0))? != 2 {
        db.execute_batch("PRAGMA auto_vacuum=INCREMENTAL;")?;
    }
    if !db.query_row("PRAGMA journal_mode", [], |r| r.get::<_, String>(0))?.eq_ignore_ascii_case("wal") {
        db.execute_batch("PRAGMA journal_mode=WAL;")?;
    }
    // A checkpoint leaves the WAL at its largest size unless capped (the index WAL sat at 102 MB, 27 Oct).
    db.execute_batch("PRAGMA synchronous=FULL; PRAGMA fullfsync=ON; PRAGMA checkpoint_fullfsync=ON; PRAGMA journal_size_limit=67108864;")?;
    Ok(())
}
/// Index commits made outside the store's writer in this process (zero once every writer goes through it).
static UNGATED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Index commits made outside the store's writer since the process started (tests assert none during live work).
pub fn ungated_writes() -> u64 {
    UNGATED.load(std::sync::atomic::Ordering::Relaxed)
}
/// Counts `db`'s commits made off a writer thread and logs the first one of each thread. The commit hook sees every
/// write, WITHOUT ROWID tables and schema changes included.
fn watch_commits(db: &Connection) -> Result<()> {
    db.commit_hook(Some(|| {
        if !crate::live::on_writer() {
            if UNGATED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 8 {
                let by = std::thread::current().name().unwrap_or("an unnamed thread").to_owned();
                eprintln!("Archive index written outside its writer by {by}");
            }
        }
        false
    }))?;
    Ok(())
}
/// The index, created on first use. Every writer shares it: keep transactions short. Its read transactions are
/// watched under the caller's name (`readers`).
#[track_caller]
pub fn open(root: &Path) -> Result<Connection> {
    let site = std::panic::Location::caller();
    private_dir(root)?;
    let path = root.join(INDEX);
    let db = Connection::open(&path)?;
    crate::readers::watch(&db, site)?;
    watch_interrupt(&db)?;
    watch_commits(&db)?;
    db.busy_timeout(BUSY)?;
    tune(&db)?;
    db.execute_batch("PRAGMA temp_store=FILE;")?;
    let key = path.canonicalize().unwrap_or(path.clone());
    if !initialized().lock().unwrap().contains(&key) {
        db.execute_batch(SCHEMA)?;
        if db.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))? != 1 {
            db.execute_batch("PRAGMA user_version=1")?;
        }
        db.execute_batch(crate::timeline::SCHEMA)?;
        db.execute_batch(crate::video::schema())?;
        crate::timeline::recording::schema(&db)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        }
        initialized().lock().unwrap().insert(key);
    }
    Ok(db)
}
/// Index-only reader. Caller holds a lifetime lock while resolving pack blobs.
/// No WAL mode/full-sync/schema writes on a screenshot lookup.
#[track_caller]
pub fn read_index(root:&Path)->Result<Connection> {
    let site = std::panic::Location::caller();
    let db=Connection::open_with_flags(root.join(INDEX),rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    crate::readers::watch(&db, site)?;
    watch_interrupt(&db)?;
    db.busy_timeout(BUSY)?;db.execute_batch("PRAGMA query_only=ON; PRAGMA temp_store=FILE;")?;Ok(db)
}
/// The archive root an index connection belongs to.
pub fn root_of(db: &Connection) -> Result<PathBuf> {
    let path = db.path().filter(|p| !p.is_empty()).context("archive index path")?;
    Ok(Path::new(path).parent().context("archive root")?.to_path_buf())
}
/// Runs `f` on `db` when it is already inside a transaction (a writer job, a fixture), else as a job on the store's
/// writer, and returns its result.
pub fn written<T: Send + 'static>(db: &Connection, f: impl FnMut(&Connection) -> Result<T> + Send + 'static) -> Result<T> {
    let mut f = f;
    if !db.is_autocommit() || crate::live::on_writer() {
        return f(db);
    }
    crate::live::Queue::shared(&root_of(db)?)?.call(f)
}
/// Session row ID, created on first use (by the writer).
pub fn ensure_session(db: &Connection, name: &str) -> Result<i64> {
    // Journals are opened often (one per agent transport event): read first, write only once.
    if let Some(id) = find_session(db, name)? {
        return Ok(id);
    }
    let name = name.to_owned();
    written(db, move |db| {
        db.prepare_cached("INSERT OR IGNORE INTO sessions(name,started_wall_ns) VALUES(?1,?2)")?
            .execute(params![name, wall_ns() as i64])?;
        Ok(db.prepare_cached("SELECT id FROM sessions WHERE name=?1")?.query_row([&name], |r| r.get(0))?)
    })
}
/// A new capture segment (one capture timeline) under a session (by the writer).
pub fn create_segment(db: &Connection, session: i64, name: &str, wall_unix_ns: u64, host_origin_ns: u64, codec: &str) -> Result<i64> {
    let (name, codec) = (name.to_owned(), codec.to_owned());
    written(db, move |db| {
        db.prepare_cached("INSERT INTO segments(session,name,version,wall_unix_ns,host_origin_ns,codec) VALUES(?1,?2,3,?3,?4,?5)")?
            .execute(params![session, name, wall_unix_ns as i64, host_origin_ns as i64, codec])?;
        Ok(db.last_insert_rowid())
    })
}
/// Store bytes on disk (index and packs, WAL checkpointed first).
pub fn bytes_on_disk(root: &Path) -> Result<u64> {
    let mut n = 0;
    // PASSIVE: never waits for a reader or stops a writer (TRUNCATE blocked every writer of the running app while a
    // reader held an old snapshot).
    if exists(root) {
        open(root)?.execute_batch("PRAGMA wal_checkpoint(PASSIVE)")?;
    }
    for pack in packs(root)? {
        with_pack(root, &pack, |p| Ok(p.execute_batch("PRAGMA wal_checkpoint(PASSIVE)")?))?;
    }
    let mut files = vec![root.join(INDEX)];
    for pack in packs(root)? {
        files.push(pack_path(root, &pack)?);
    }
    for f in files {
        for suffix in ["", "-wal", "-shm"] {
            if let Ok(m) = std::fs::metadata(format!("{}{suffix}", f.display())) {
                n += m.len();
            }
        }
    }
    Ok(n)
}
pub fn find_session(db: &Connection, name: &str) -> Result<Option<i64>> {
    Ok(db
        .prepare_cached("SELECT id FROM sessions WHERE name=?1")?
        .query_row([name], |r| r.get(0))
        .optional()?)
}
/// Clean application drain completed (formerly the `COMPLETE` file).
pub fn mark_complete(session: &Path) -> Result<()> {
    let (root, name) = locate(session)?;
    let db = open(&root)?;
    let id = ensure_session(&db, &name)?;
    written(&db, move |db| {
        db.execute("UPDATE sessions SET complete_wall_ns=?2 WHERE id=?1", params![id, wall_ns() as i64])?;
        Ok(())
    })
}
pub fn is_complete(session: &Path) -> Result<bool> {
    let (root, name) = locate(session)?;
    if !exists(&root) {
        return Ok(false);
    }
    let db = open(&root)?;
    Ok(db
        .query_row(
            "SELECT complete_wall_ns IS NOT NULL FROM sessions WHERE name=?1",
            [name],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(false))
}
/// Session names, oldest first.
pub fn sessions(root: &Path) -> Result<Vec<String>> {
    if !exists(root) {
        return Ok(vec![]);
    }
    let db = open(root)?;
    let names = db
        .prepare("SELECT name FROM sessions ORDER BY id")?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(names)
}

/// UTC `YYYY-MM` of a Unix wall time: the month pack that holds content captured then.
pub fn month(wall_ns: u64) -> String {
    // Howard Hinnant's days-to-civil.
    let z = (wall_ns / 86_400_000_000_000) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}")
}
pub fn pack_path(root: &Path, pack: &str) -> Result<PathBuf> {
    ensure!(
        pack == HOT
            || (pack.len() == 7
                && pack.as_bytes()[4] == b'-'
                && pack.bytes().enumerate().all(|(i, b)| i == 4 || b.is_ascii_digit())),
        "invalid pack name {pack}"
    );
    Ok(root.join(PACKS).join(format!("{pack}.sqlite")))
}
/// Pack names present on disk.
pub fn packs(root: &Path) -> Result<Vec<String>> {
    let dir = root.join(PACKS);
    if !dir.is_dir() {
        return Ok(vec![]);
    }
    let mut out = vec![];
    for e in std::fs::read_dir(dir)? {
        let name = e?.file_name().to_string_lossy().to_string();
        if let Some(stem) = name.strip_suffix(".sqlite") {
            if pack_path(root, stem).is_ok() {
                out.push(stem.to_owned());
            }
        }
    }
    out.sort();
    Ok(out)
}

/// Pack connections shared by every thread. A pack never has more than `PACK_CONNECTIONS` (lent and idle; a caller
/// past that waits for one), and only the `PACK_POOLS` packs used last keep idle ones. Per-thread caches kept one
/// connection per thread that ever touched a pack, and SQLite keeps a closed connection's file open while any other
/// connection to that file lives: 26 Oct 01:15, 25 handles to `hot.sqlite` helped fill the 256-descriptor table and
/// Jarvis failed with "Too many open files" (tests/fd_bound.rs).
#[derive(Default)]
struct PackPool {
    packs: HashMap<PathBuf, Pooled>,
    tick: u64,
}
#[derive(Default)]
struct Pooled {
    idle: Vec<Connection>,
    /// Lent and idle connections.
    open: usize,
    used: u64,
}
pub const PACK_CONNECTIONS: usize = 4;
pub const PACK_POOLS: usize = 4;
fn pack_pool() -> &'static (Mutex<PackPool>, std::sync::Condvar) {
    static POOL: OnceLock<(Mutex<PackPool>, std::sync::Condvar)> = OnceLock::new();
    POOL.get_or_init(Default::default)
}
fn open_pack(path: &Path) -> Result<Connection> {
    private_dir(path.parent().unwrap())?;
    let db = Connection::open(path)?;
    db.busy_timeout(BUSY)?;
    tune(&db)?;
    db.execute_batch("CREATE TABLE IF NOT EXISTS blobs(key TEXT PRIMARY KEY, added_ns INTEGER NOT NULL, data BLOB NOT NULL);")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(db)
}
/// An idle connection to `path`, or a new one while the pack is under its cap. Connections are closed outside the lock.
fn lend(path: &Path) -> Result<Connection> {
    let (lock, freed) = pack_pool();
    let mut stale = vec![];
    let mut pool = lock.lock().unwrap();
    loop {
        pool.tick += 1;
        let tick = pool.tick;
        let pack = pool.packs.entry(path.to_path_buf()).or_default();
        pack.used = tick;
        if !pack.idle.is_empty() && !path.is_file() {
            // The pack file was removed: its idle connections point at the old file.
            pack.open -= pack.idle.len();
            stale.append(&mut pack.idle);
        }
        if let Some(db) = pack.idle.pop() {
            drop(pool);
            drop(stale);
            return Ok(db);
        }
        if pack.open < PACK_CONNECTIONS {
            pack.open += 1;
            drop(pool);
            drop(stale);
            return open_pack(path).inspect_err(|_| give_back(path, None));
        }
        pool = freed.wait(pool).unwrap();
    }
}
/// Returns a lent connection (`None`: it was dropped), keeping it idle when it is clean, then drops the idle connections
/// of the packs used longest ago beyond `PACK_POOLS`.
fn give_back(path: &Path, db: Option<Connection>) {
    let (lock, freed) = pack_pool();
    let mut closed = vec![];
    {
        let mut pool = lock.lock().unwrap();
        match (pool.packs.get_mut(path), db) {
            (Some(pack), Some(db)) if db.is_autocommit() => pack.idle.push(db),
            (Some(pack), db) => {
                pack.open -= 1;
                closed.extend(db);
            }
            (None, db) => closed.extend(db),
        }
        loop {
            let pooled = pool.packs.values().filter(|p| !p.idle.is_empty()).count();
            if pooled <= PACK_POOLS {
                break;
            }
            let Some(oldest) = pool.packs.iter().filter(|(_, p)| !p.idle.is_empty()).min_by_key(|(_, p)| p.used).map(|(k, _)| k.clone()) else { break };
            let pack = pool.packs.get_mut(&oldest).unwrap();
            pack.open -= pack.idle.len();
            closed.append(&mut pack.idle);
        }
        pool.packs.retain(|_, p| p.open > 0);
    }
    freed.notify_all();
    drop(closed);
}
/// Runs `f` on a pooled connection to one pack, creating the pack on first use.
pub fn with_pack<T>(root: &Path, pack: &str, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
    struct Lent<'a>(&'a Path, Option<Connection>);
    impl Drop for Lent<'_> {
        // Also on a panic in `f`: the pack's cap counts this connection until it is back.
        fn drop(&mut self) {
            give_back(self.0, self.1.take());
        }
    }
    let path = pack_path(root, pack)?;
    let lent = Lent(&path, Some(lend(&path)?));
    f(lent.1.as_ref().unwrap())
}
/// Closes every idle pack connection (tests, migration teardown); lent ones go back to the pool as usual.
pub fn close_packs() {
    let (lock, _) = pack_pool();
    let mut closed = vec![];
    {
        let mut pool = lock.lock().unwrap();
        for pack in pool.packs.values_mut() {
            pack.open -= pack.idle.len();
            closed.append(&mut pack.idle);
        }
        pool.packs.retain(|_, p| p.open > 0);
    }
    drop(closed);
}
/// Commits blobs (one durable transaction). Existing keys are kept as they are: keys name content.
pub fn put(root: &Path, pack: &str, items: &[(String, &[u8])]) -> Result<()> {
    if items.is_empty() {
        return Ok(());
    }
    with_pack(root, pack, |db| {
        let tx = db.unchecked_transaction()?;
        {
            let mut insert = tx.prepare_cached("INSERT OR IGNORE INTO blobs(key,added_ns,data) VALUES(?1,?2,?3)")?;
            let now = wall_ns() as i64;
            for (key, data) in items {
                insert.execute(params![key, now, data])?;
            }
        }
        tx.commit()?;
        Ok(())
    })
}
pub fn get(root: &Path, pack: &str, key: &str) -> Result<Option<Vec<u8>>> {
    if !pack_path(root, pack)?.is_file() {
        return Ok(None);
    }
    with_pack(root, pack, |db| {
        Ok(db
            .prepare_cached("SELECT data FROM blobs WHERE key=?1")?
            .query_row([key], |r| r.get(0))
            .optional()?)
    })
}
pub fn has(root: &Path, pack: &str, key: &str) -> Result<bool> {
    if !pack_path(root, pack)?.is_file() {
        return Ok(false);
    }
    with_pack(root, pack, |db| {
        Ok(db
            .prepare_cached("SELECT EXISTS(SELECT 1 FROM blobs WHERE key=?1)")?
            .query_row([key], |r| r.get(0))?)
    })
}
/// Writes the blob unless present. Call inside the index write transaction that publishes the
/// reference: the collector cannot remove it until that transaction has committed.
pub fn ensure_blob(root: &Path, pack: &str, key: &str, data: &[u8]) -> Result<()> {
    if !has(root, pack, key)? {
        put(root, pack, &[(key.to_owned(), data)])?;
    }
    Ok(())
}
/// Reads a referenced blob and checks its stored SHA-256.
pub fn read_checked(root: &Path, pack: &str, key: &str, sha256: &str) -> Result<Vec<u8>> {
    let bytes = get(root, pack, key)?
        .with_context(|| format!("archive blob {pack}/{key} missing"))?;
    ensure!(hash(&bytes) == sha256, "archive blob {pack}/{key} checksum mismatch");
    Ok(bytes)
}

pub fn audio_key(id: &str) -> String {
    format!("a/{id}")
}
pub fn image_ext(mime: &str) -> Result<&'static str> {
    Ok(match mime {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        _ => anyhow::bail!("unsupported archived image {mime}"),
    })
}
pub fn image_key(id: &str, mime: &str) -> Result<String> {
    Ok(format!("i/{id}.{}", image_ext(mime)?))
}
pub fn chunk_key(sha256: &str) -> String {
    format!("c/{sha256}")
}
pub fn log_key(sha256: &str) -> String {
    format!("l/{sha256}")
}
/// Queues a blob for collection. Use in the transaction that removes its last reference.
pub fn queue_gc(db: &Connection, pack: &str, key: &str) -> Result<()> {
    db.prepare_cached("INSERT OR IGNORE INTO gc_blobs VALUES(?1,?2)")?
        .execute(params![pack, key])?;
    Ok(())
}
/// Whether any catalogue row references `(pack, key)`. Unknown key kinds count as referenced.
pub fn referenced(db: &Connection, pack: &str, key: &str) -> Result<bool> {
    let exists = |sql: &str, a: &str| -> Result<bool> {
        Ok(db.prepare_cached(sql)?.query_row(params![a, pack], |r| r.get(0))?)
    };
    Ok(if let Some(id) = key.strip_prefix("a/") {
        exists("SELECT EXISTS(SELECT 1 FROM audio_objects WHERE id=?1 AND pack=?2)", id)?
    } else if let Some(id) = key.strip_prefix("v/") {
        exists("SELECT EXISTS(SELECT 1 FROM video_samples WHERE sha256=?1 AND pack=?2)", id)?
    } else if let Some(rest) = key.strip_prefix("i/") {
        let (id, ext) = rest.rsplit_once('.').unwrap_or((rest, ""));
        let mime: Option<String> = db
            .prepare_cached("SELECT mime FROM images WHERE id=?1 AND pack=?2")?
            .query_row(params![id, pack], |r| r.get(0))
            .optional()?;
        mime.is_some_and(|m| image_ext(&m).is_ok_and(|e| e == ext))
    } else if let Some(sha) = key.strip_prefix("c/") {
        exists("SELECT EXISTS(SELECT 1 FROM cold_chunks WHERE sha256=?1 AND pack=?2)", sha)?
    } else if let Some(sha) = key.strip_prefix("l/") {
        exists("SELECT EXISTS(SELECT 1 FROM logs WHERE sha256=?1 AND pack=?2)", sha)?
    } else {
        true
    })
}
fn delete_unreferenced(db: &Connection, root: &Path, pack: &str, keys: &[String]) -> Result<usize> {
    let mut doomed = vec![];
    for key in keys {
        if !referenced(db, pack, key)? {
            doomed.push(key.clone());
        }
    }
    if !doomed.is_empty() && pack_path(root, pack)?.is_file() {
        // In the same index transaction: a publisher that read the count before writing its blobs sees it change.
        db.execute(
            "INSERT INTO store_meta VALUES('collections','1') ON CONFLICT(key) DO UPDATE SET value=CAST(value AS INTEGER)+1",
            [],
        )?;
        with_pack(root, pack, |p| {
            let tx = p.unchecked_transaction()?;
            for key in &doomed {
                tx.prepare_cached("DELETE FROM blobs WHERE key=?1")?.execute([key])?;
            }
            tx.commit()?;
            Ok(())
        })?;
    }
    Ok(doomed.len())
}
/// Collection steps that removed blobs from a pack, store-wide (`store_meta`), counted in the transaction that removed
/// them. A publisher that reads it before committing its blobs and finds it unchanged under the write lock knows those
/// blobs are still there (`audio::publish_put`).
pub fn collections(db: &Connection) -> Result<i64> {
    Ok(db
        .query_row("SELECT CAST(value AS INTEGER) FROM store_meta WHERE key='collections'", [], |r| r.get(0))
        .optional()?
        .unwrap_or(0))
}
/// Queued blobs one collection step removes at most.
pub const COLLECT_KEYS: usize = 256;
/// Removes up to `limit` queued blobs, inside the caller's write transaction (a background job), under the exclusive
/// lifetime lock taken without waiting: while a reader holds the shared one, nothing is removed and 0 comes back.
pub fn collect(db: &Connection, root: &Path, limit: usize) -> Result<usize> {
    let Some(_lock) = crate::compact::exclusive_lock(root)? else { return Ok(0) };
    let queued: Vec<(String, String)> = db
        .prepare("SELECT pack,key FROM gc_blobs ORDER BY pack,key LIMIT ?1")?
        .query_map([limit as i64], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    if queued.is_empty() {
        return Ok(0);
    }
    let mut by_pack: HashMap<String, Vec<String>> = HashMap::new();
    for (pack, key) in &queued {
        by_pack.entry(pack.clone()).or_default().push(key.clone());
    }
    let mut removed = 0;
    // The index write lock is held across the pack deletion: a publisher checking its blob
    // either commits before (and the blob stays referenced) or after (and rewrites it).
    ensure!(!db.is_autocommit(), "collection runs inside a write transaction");
    for (pack, keys) in &by_pack {
        removed += delete_unreferenced(db, root, pack, keys)?;
        for key in keys {
            db.prepare_cached("DELETE FROM gc_blobs WHERE pack=?1 AND key=?2")?.execute(params![pack, key])?;
        }
    }
    Ok(removed)
}
/// Removes unreferenced blobs older than the grace period, `budget` keys per pack per call, resuming from a cursor,
/// inside the caller's write transaction (a background job), under the exclusive lifetime lock taken without waiting.
pub fn sweep(db: &Connection, root: &Path, now: u64, budget: usize) -> Result<usize> {
    let Some(_lock) = crate::compact::exclusive_lock(root)? else { return Ok(0) };
    ensure!(!db.is_autocommit(), "the sweep runs inside a write transaction");
    let mut removed = 0;
    for pack in packs(root)? {
        let cursor_key = format!("sweep:{pack}");
        let cursor: String = db
            .query_row("SELECT value FROM store_meta WHERE key=?1", [&cursor_key], |r| r.get(0))
            .optional()?
            .unwrap_or_default();
        let keys: Vec<(String, i64)> = with_pack(root, &pack, |p| {
            Ok(p.prepare("SELECT key,added_ns FROM blobs WHERE key>?1 ORDER BY key LIMIT ?2")?
                .query_map(params![cursor, budget as i64], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?)
        })?;
        let next = if keys.len() < budget {
            String::new()
        } else {
            keys.last().map(|k| k.0.clone()).unwrap_or_default()
        };
        let old: Vec<String> = keys
            .into_iter()
            .filter(|(_, added)| (*added as u64).saturating_add(ORPHAN_GRACE_NS) <= now)
            .map(|(k, _)| k)
            .collect();
        removed += delete_unreferenced(db, root, &pack, &old)?;
        db.execute(
            "INSERT INTO store_meta VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![cursor_key, next],
        )?;
    }
    Ok(removed)
}
/// Pages one reclaim step frees at most.
pub const RECLAIM_PAGES: u32 = 512;
/// Frees up to `pages` SQLite pages, `RECLAIM_PAGES` per statement. Inside a write transaction (a background job) pass
/// at most `RECLAIM_PAGES`, so one job stays short.
pub fn reclaim(db: &Connection, pages: u32) -> Result<()> {
    let mut left = pages;
    while left > 0 {
        let step = left.min(RECLAIM_PAGES);
        let freelist: i64 = db.query_row("PRAGMA freelist_count", [], |r| r.get(0))?;
        if freelist == 0 {
            break;
        }
        // This pragma returns one row per freed page. execute_batch only steps once.
        let mut stmt = db.prepare(&format!("PRAGMA incremental_vacuum({step})"))?;
        let mut rows = stmt.query([])?;
        while rows.next()?.is_some() {}
        left -= step;
    }
    Ok(())
}
pub fn reclaim_pack(root: &Path, pack: &str, pages: u32) -> Result<()> {
    if !pack_path(root, pack)?.is_file() {
        return Ok(());
    }
    with_pack(root, pack, |p| reclaim(p, pages))
}

/// Archives a log or other per-launch file into the session's `logs`, zstd-compressed and
/// verified. Re-archiving identical bytes under the same name is a no-op. Returns once the row is committed (its
/// caller deletes or rewrites the source next).
pub fn archive_log(session: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    let (root, session_name) = locate(session)?;
    let packed = crate::compress_log(bytes)?;
    let sha = hash(bytes);
    let pack = month(wall_ns());
    put(&root, &pack, &[(log_key(&sha), &packed)])?;
    let (name, raw) = (name.to_owned(), bytes.len() as i64);
    // The agent's loop retires payloads through here: it waits at most `live::COMMIT_WAIT`, then hears "the archive
    // is busy" and keeps its source; the row still lands later (archiving the same bytes again is a no-op).
    let queue = crate::live::Queue::shared(&root)?;
    queue.call_within(crate::live::COMMIT_WAIT, move |db| {
        ensure_blob(&root, &pack, &log_key(&sha), &packed)?;
        let id = ensure_session(db, &session_name)?;
        let existing: Option<String> = db
            .query_row("SELECT sha256 FROM logs WHERE session=?1 AND name=?2", params![id, name], |r| r.get(0))
            .optional()?;
        match existing {
            Some(s) if s == sha => {}
            Some(_) => anyhow::bail!("archive log {name} already holds different bytes"),
            None => {
                db.execute(
                    "INSERT INTO logs(session,name,raw_bytes,sha256,pack,wall_ns) VALUES(?1,?2,?3,?4,?5,?6)",
                    params![id, name, raw, sha, pack, wall_ns() as i64],
                )?;
            }
        }
        Ok(())
    })
}
/// A SQLite file's consistent bytes, committed WAL included (online backup to a private temp file).
pub fn sqlite_bytes(path: &Path) -> Result<Vec<u8>> {
    let src = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).or_else(|_| Connection::open(path))?;
    src.busy_timeout(Duration::from_secs(10))?;
    let temp = tempfile::tempdir()?;
    let out = temp.path().join("copy.sqlite");
    src.backup(rusqlite::MAIN_DB, &out, None)?;
    Ok(std::fs::read(out)?)
}
/// One archived log's original bytes.
pub fn read_log(session: &Path, name: &str) -> Result<Vec<u8>> {
    let (root, session_name) = locate(session)?;
    let db = open(&root)?;
    let (sha, pack): (String, String) = db.query_row(
        "SELECT l.sha256,l.pack FROM logs l JOIN sessions s ON s.id=l.session WHERE s.name=?1 AND l.name=?2",
        params![session_name, name],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let packed = get(&root, &pack, &log_key(&sha))?.context("archived log blob missing")?;
    let bytes = crate::decompress_log(&packed)?;
    ensure!(hash(&bytes) == sha, "archived log checksum mismatch");
    Ok(bytes)
}
/// Archived log names of one session.
pub fn logs(session: &Path) -> Result<Vec<String>> {
    let (root, session_name) = locate(session)?;
    let db = open(&root)?;
    let names = db
        .prepare("SELECT l.name FROM logs l JOIN sessions s ON s.id=l.session WHERE s.name=?1 ORDER BY l.id")?
        .query_map([session_name], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;
    /// 24 Oct 02:31: Quit waited behind a compaction step holding the write lock. A preempted compactor's running
    /// statement stops at once, its transaction rolls back, and the lock is free for the next writer.
    #[test]
    fn a_preempted_compaction_statement_stops_and_frees_the_write_lock() {
        use std::sync::{atomic::{AtomicBool, Ordering}, Arc};
        let t = tempfile::tempdir().unwrap();
        open(t.path()).unwrap().execute_batch("CREATE TABLE held(x)").unwrap();
        let flag = Arc::new(AtomicBool::new(false));
        let (root, preempt) = (t.path().to_path_buf(), flag.clone());
        let (locked_tx, locked) = std::sync::mpsc::channel();
        let step = std::thread::spawn(move || {
            interrupt_when(preempt);
            let db = open(&root).unwrap();
            db.execute_batch("BEGIN IMMEDIATE; INSERT INTO held VALUES(1);").unwrap();
            locked_tx.send(()).unwrap();
            // A step that would hold the lock for minutes.
            let r = db.query_row("WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n) SELECT count(*) FROM n", [], |r| r.get::<_, i64>(0));
            assert!(r.is_err(), "the statement ran to completion");
        });
        locked.recv().unwrap();
        // `open` itself waits for the lock (its pragmas): a plain connection plays the live writer.
        let live = Connection::open(t.path().join(INDEX)).unwrap();
        live.busy_timeout(Duration::ZERO).unwrap();
        assert!(live.execute("INSERT INTO held VALUES(2)", []).is_err(), "the step holds the write lock");
        let started = std::time::Instant::now();
        flag.store(true, Ordering::Release);
        step.join().unwrap();
        assert!(started.elapsed() < Duration::from_secs(2), "stopped after {:?}", started.elapsed());
        live.busy_timeout(Duration::from_secs(5)).unwrap();
        live.execute("INSERT INTO held VALUES(2)", []).unwrap();
        let rows: i64 = live.query_row("SELECT count(*) FROM held", [], |r| r.get(0)).unwrap();
        assert_eq!(rows, 1, "the interrupted step's write rolled back");
    }
    #[test]
    fn months_and_names() {
        assert_eq!(month(0), "1970-01");
        // 2026-10-13T00:00:00Z
        assert_eq!(month(1_791_849_600_000_000_000), "2026-10");
        assert_eq!(month(1_791_849_600_000_000_000 - 13 * 86_400_000_000_000), "2026-09");
        assert!(pack_path(Path::new("/a"), "2026-10").is_ok());
        assert!(pack_path(Path::new("/a"), "../x").is_err());
        assert!(locate(Path::new("/a/session-1-x")).is_ok());
        assert!(locate(Path::new("/a/.tmpXYZ")).is_err());
        assert!(locate(Path::new("/a/session-1/../b")).is_err());
    }
    #[test]
    fn collector_keeps_referenced_blobs_and_sweep_respects_grace() -> Result<()> {
        let t = tempfile::tempdir()?;
        let db = open(t.path())?;
        put(t.path(), HOT, &[(audio_key("x"), b"one"), (audio_key("y"), b"two")])?;
        db.execute("INSERT INTO audio_objects VALUES('x',1,1,1,'raw','hot',?1)", [hash(b"one")])?;
        queue_gc(&db, HOT, &audio_key("x"))?;
        queue_gc(&db, HOT, &audio_key("y"))?;
        let collect_in = |db: &Connection| -> Result<usize> { db.execute_batch("BEGIN IMMEDIATE")?; let n = collect(db, t.path(), 10)?; db.execute_batch("COMMIT")?; Ok(n) };
        let sweep_in = |db: &Connection, now: u64| -> Result<usize> { db.execute_batch("BEGIN IMMEDIATE")?; let n = sweep(db, t.path(), now, 100)?; db.execute_batch("COMMIT")?; Ok(n) };
        assert_eq!(collect_in(&db)?, 1);
        assert!(has(t.path(), HOT, &audio_key("x"))?);
        assert!(!has(t.path(), HOT, &audio_key("y"))?);
        put(t.path(), HOT, &[(audio_key("z"), b"orphan")])?;
        assert_eq!(sweep_in(&db, wall_ns())?, 0, "young orphans wait");
        assert_eq!(sweep_in(&db, wall_ns() + 2 * ORPHAN_GRACE_NS)?, 1);
        assert!(has(t.path(), HOT, &audio_key("x"))?);
        Ok(())
    }
}
