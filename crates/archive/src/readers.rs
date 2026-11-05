//! Who holds a read snapshot of the index. A reader's snapshot pins the WAL: a checkpoint cannot copy frames
//! committed after it began, and the WAL cannot restart until it ends. From 22:45 to 23:37 on 3 Nov the index WAL
//! grew from 64 MB to 8.8 GB that way and nothing named the reader (bug note 2026-11-02T15-58-15).
//!
//! Every index connection opened through `store::open` or `store::read_index` is watched: SQLite's statement trace
//! tells, on the connection's own thread, when its first statement starts and when its last one ends (or its
//! explicit transaction commits). The checkpointer names any read transaction older than `LONG_READ`, once, with the
//! thread, the code that opened the connection and the statement that began it; the WAL line names the holders when
//! a reader cut a checkpoint short. Nothing here calls into a connection from another thread.
use crate::live::Site;
use anyhow::{bail, Result};
use rusqlite::{ffi, Connection};
use std::{
    ffi::{c_int, c_uint, c_void, CStr},
    io::Write,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex, OnceLock, Weak,
    },
    time::{Duration, Instant},
};

/// A read transaction held this long is logged with its holder.
pub const LONG_READ: Duration = Duration::from_secs(60);
/// The longest statement text kept per reader.
const SQL_CHARS: usize = 160;
const CLIENT_DATA: &CStr = c"heimdall-reader";

struct Reader {
    site: Site,
    /// Milliseconds since `epoch()` (plus one) when its read transaction began; 0 while it holds none.
    since: AtomicU64,
    /// Its current transaction was logged as long (its end is logged too).
    reported: AtomicBool,
    /// The thread and the statement that began the current transaction.
    began: Mutex<(String, String)>,
}
impl Reader {
    fn name(&self) -> String {
        let (thread, sql) = self.began.lock().map(|b| b.clone()).unwrap_or_default();
        format!("{thread} on a connection opened at {}:{} ({sql})", self.site.file(), self.site.line())
    }
}

fn epoch() -> Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}
fn now_ms() -> u64 {
    epoch().elapsed().as_millis() as u64 + 1
}
fn registry() -> &'static Mutex<Vec<Weak<Reader>>> {
    static READERS: OnceLock<Mutex<Vec<Weak<Reader>>>> = OnceLock::new();
    READERS.get_or_init(Default::default)
}

/// Watches `db`'s read transactions under the name of the code that opened it.
pub(crate) fn watch(db: &Connection, site: Site) -> Result<()> {
    epoch();
    let reader = Arc::new(Reader { site, since: AtomicU64::new(0), reported: AtomicBool::new(false), began: Mutex::new(Default::default()) });
    {
        let mut readers = registry().lock().unwrap();
        readers.retain(|r| r.strong_count() > 0);
        readers.push(Arc::downgrade(&reader));
    }
    let context = Arc::into_raw(reader) as *mut c_void;
    // SAFETY: the connection owns `context` from here: SQLite calls `release` once when the connection is freed (or at
    // once when it cannot store it), after its last trace callback. The callbacks run on the connection's own thread.
    unsafe {
        let db = db.handle();
        if ffi::sqlite3_set_clientdata(db, CLIENT_DATA.as_ptr(), context, Some(release)) != ffi::SQLITE_OK {
            bail!("archive reader watch: out of memory");
        }
        ffi::sqlite3_trace_v2(db, ffi::SQLITE_TRACE_STMT | ffi::SQLITE_TRACE_PROFILE, Some(trace), context);
    }
    Ok(())
}
unsafe extern "C" fn release(context: *mut c_void) {
    // SAFETY: `context` came from `Arc::into_raw` in `watch` and SQLite releases it exactly once.
    drop(unsafe { Arc::from_raw(context as *const Reader) });
}
/// STMT: a statement starts (`x` is its text; a trigger's program starts with "--"). PROFILE: a statement ended or is
/// being reset (`p` is the statement). The connection's transaction ends with its last running statement unless an
/// explicit `BEGIN` keeps it open.
unsafe extern "C" fn trace(kind: c_uint, context: *mut c_void, p: *mut c_void, x: *mut c_void) -> c_int {
    // SAFETY: `context` stays valid until `release`, which SQLite calls after the connection's last callback.
    let reader = unsafe { &*(context as *const Reader) };
    if kind == ffi::SQLITE_TRACE_STMT {
        if reader.since.load(Ordering::Acquire) != 0 || x.is_null() {
            return 0;
        }
        // SAFETY: for STMT, `x` is the statement's NUL-terminated text.
        let text = unsafe { CStr::from_ptr(x as *const std::ffi::c_char) }.to_string_lossy();
        if text.starts_with("--") {
            return 0;
        }
        let sql: String = text.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(SQL_CHARS).collect();
        let thread = std::thread::current().name().unwrap_or("an unnamed thread").to_owned();
        if let Ok(mut began) = reader.began.lock() {
            *began = (thread, sql);
        }
        reader.since.store(now_ms(), Ordering::Release);
    } else if kind == ffi::SQLITE_TRACE_PROFILE {
        let stmt = p as *mut ffi::sqlite3_stmt;
        // SAFETY: `stmt` is a live statement of this connection; these calls only read its state, on its own thread.
        let open = unsafe {
            let db = ffi::sqlite3_db_handle(stmt);
            let mut other = ffi::sqlite3_next_stmt(db, std::ptr::null_mut());
            let mut running = false;
            while !other.is_null() && !running {
                running = other != stmt && ffi::sqlite3_stmt_busy(other) != 0;
                other = ffi::sqlite3_next_stmt(db, other);
            }
            running || ffi::sqlite3_get_autocommit(db) == 0
        };
        if !open {
            let since = reader.since.swap(0, Ordering::AcqRel);
            if since != 0 && reader.reported.swap(false, Ordering::AcqRel) {
                let held = now_ms().saturating_sub(since) / 1000;
                let _ = writeln!(std::io::stderr(), "Archive read snapshot released after {held} s by {}", reader.name());
            }
        }
    }
    0
}

/// A read transaction open now: who holds it and for how long.
#[derive(Debug, Clone)]
pub struct Held {
    pub name: String,
    pub held: Duration,
}
/// Read transactions on the index held at least `min` in this process, oldest first.
pub fn holding(min: Duration) -> Vec<Held> {
    let now = now_ms();
    let mut held: Vec<Held> = live()
        .iter()
        .filter_map(|r| {
            let since = r.since.load(Ordering::Acquire);
            let held = Duration::from_millis(now.saturating_sub(since));
            (since != 0 && held >= min).then(|| Held { name: r.name(), held })
        })
        .collect();
    held.sort_by(|a, b| b.held.cmp(&a.held));
    held
}
fn live() -> Vec<Arc<Reader>> {
    registry().lock().unwrap().iter().filter_map(Weak::upgrade).collect()
}
/// Lines for read transactions that reached `min` since the last call, each once (its release is logged by the
/// reader's own thread).
pub fn newly_long(min: Duration) -> Vec<String> {
    let now = now_ms();
    live()
        .iter()
        .filter_map(|r| {
            let since = r.since.load(Ordering::Acquire);
            let held = Duration::from_millis(now.saturating_sub(since));
            (since != 0 && held >= min && !r.reported.swap(true, Ordering::AcqRel)).then(|| {
                format!("Archive read snapshot held {} s so far by {}; the WAL cannot restart until it ends", held.as_secs(), r.name())
            })
        })
        .collect()
}
/// Logs read transactions past `LONG_READ` (the checkpointer asks every few seconds).
pub(crate) fn report_long() {
    for line in newly_long(LONG_READ) {
        eprintln!("{line}");
    }
}
/// Who may hold the snapshot that cut a checkpoint short: this process's open read transactions (up to three, oldest
/// first), else the other processes that have `index` open.
pub(crate) fn suspects(index: &str) -> String {
    // The writer's own groups and the checkpointer's statement are younger than a second and never pin the WAL.
    let held = holding(Duration::from_secs(1));
    if !held.is_empty() {
        let names: Vec<String> = held.iter().take(3).map(|h| format!("{} s by {}", h.held.as_secs(), h.name)).collect();
        return format!("read transactions open here: {}", names.join("; "));
    }
    let others = other_processes(index);
    if others.is_empty() {
        "no read transaction open in this process".into()
    } else {
        format!("no read transaction open in this process; also open in {}", others.join(", "))
    }
}
/// Other processes with `path` open (`lsof`, never blocking on a stuck file system).
fn other_processes(path: &str) -> Vec<String> {
    let Ok(out) = std::process::Command::new("/usr/sbin/lsof").args(["-n", "-P", "-w", "-b", "-F", "pc", "--", path]).output() else { return vec![] };
    let me = std::process::id().to_string();
    let mut found = vec![];
    let mut pid = String::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        if let Some(p) = line.strip_prefix('p') {
            pid = p.to_owned();
        } else if let Some(c) = line.strip_prefix('c') {
            if pid != me {
                found.push(format!("{c} (pid {pid})"));
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store;

    fn mine(min: Duration, sql: &str) -> Option<Held> {
        holding(min).into_iter().find(|h| h.name.contains(sql))
    }

    /// 3 Nov 22:45-23:37: a reader kept its snapshot about 50 minutes and nothing named it. A reader held open is
    /// named with its thread, the code that opened its connection and the statement that began it; once it ends it is
    /// gone, and the next checkpoint copies every frame and the next write restarts the WAL.
    #[test]
    fn a_held_reader_is_named_and_the_wal_restarts_after_it_ends() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("archive");
        let writer = store::open(&root).unwrap();
        writer.execute_batch("PRAGMA wal_autocheckpoint=0; CREATE TABLE held_test(id INTEGER PRIMARY KEY, data BLOB)").unwrap();
        for i in 0..64 {
            writer.execute("INSERT INTO held_test VALUES(?1, zeroblob(8192))", [i]).unwrap();
        }
        let (hold, held) = (std::sync::mpsc::channel::<()>(), std::sync::mpsc::channel::<()>());
        let reader_root = root.clone();
        let reader = std::thread::Builder::new()
            .name("held-reader-test".into())
            .spawn(move || {
                let db = store::read_index(&reader_root).unwrap();
                let mut stmt = db.prepare("SELECT id FROM held_test /* held reader */ ORDER BY id").unwrap();
                let mut rows = stmt.query([]).unwrap();
                rows.next().unwrap();
                held.0.send(()).unwrap();
                hold.1.recv().unwrap();
            })
            .unwrap();
        held.1.recv().unwrap();
        std::thread::sleep(Duration::from_millis(30));
        let h = mine(Duration::from_millis(20), "held reader").expect("the open reader is listed");
        assert!(h.name.starts_with("held-reader-test on a connection opened at crates/archive/src/readers.rs:"), "{}", h.name);
        let lines = newly_long(Duration::from_millis(20));
        assert!(lines.iter().any(|l| l.contains("held reader") && l.contains("so far by held-reader-test")), "{lines:?}");
        assert!(!newly_long(Duration::from_millis(20)).iter().any(|l| l.contains("held reader")), "said once");
        std::thread::sleep(Duration::from_secs(1));
        assert!(suspects("unused").contains("held reader"));

        // Frames committed after its snapshot cannot be copied while it holds it.
        for i in 64..128 {
            writer.execute("INSERT INTO held_test VALUES(?1, zeroblob(8192))", [i]).unwrap();
        }
        let ckpt = |db: &Connection| db.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |r| Ok((r.get::<_, i64>(1)?, r.get::<_, i64>(2)?))).unwrap();
        let (log, copied) = ckpt(&writer);
        assert!(copied < log, "the held snapshot stops the checkpoint ({copied} of {log})");

        hold.0.send(()).unwrap();
        reader.join().unwrap();
        assert!(mine(Duration::ZERO, "held reader").is_none(), "gone once it ends");
        let (log, copied) = ckpt(&writer);
        assert_eq!(copied, log, "every frame copied after it ends");
        writer.execute("INSERT INTO held_test VALUES(1000, zeroblob(8192))", []).unwrap();
        let (log, _) = ckpt(&writer);
        assert!(log < 10, "the next write restarted the WAL ({log} frames)");
    }

    /// An explicit transaction holds its snapshot between statements, until it commits.
    #[test]
    fn an_explicit_read_transaction_is_held_until_it_commits() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("archive");
        store::open(&root).unwrap();
        let db = store::read_index(&root).unwrap();
        db.execute_batch("BEGIN; SELECT count(*) FROM sessions /* explicit reader */;").unwrap();
        assert!(mine(Duration::ZERO, "BEGIN").is_some_and(|h| h.name.contains("readers.rs")), "{:?}", holding(Duration::ZERO));
        db.query_row("SELECT count(*) FROM segments", [], |r| r.get::<_, i64>(0)).unwrap();
        assert!(mine(Duration::ZERO, "BEGIN").is_some(), "still held between statements");
        db.execute_batch("COMMIT").unwrap();
        assert!(mine(Duration::ZERO, "BEGIN").is_none());
        // A statement run to its end holds nothing afterwards.
        db.query_row("SELECT count(*) FROM sessions /* one-shot */", [], |r| r.get::<_, i64>(0)).unwrap();
        assert!(mine(Duration::ZERO, "one-shot").is_none());
    }
}
