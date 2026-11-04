//! 26 Oct 23:01:27: the live loop froze 2.0 s in `fcntl(F_FULLFSYNC)` under `store::open`, called from
//! `video::Writer::open` while capture was stopping and its archive still committing (hang-2026-10-26-230129-live.txt).
//! Every open ran `PRAGMA auto_vacuum=INCREMENTAL`, which on an existing database is a write: it waited for the shared
//! write lock and then committed one page with a full flush. Opening must not write; real commits keep their flush.
use heimdall_archive::store;
use rusqlite::Connection;
use std::{path::Path, sync::mpsc, time::Duration};

fn wal_len(db: &Path) -> u64 {
    std::fs::metadata(format!("{}-wal", db.display())).map_or(0, |m| m.len())
}

/// Runs `f` on its own thread; `None` when it hasn't returned within `limit`.
fn within<T: Send + 'static>(limit: Duration, f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(limit).ok()
}

#[test]
fn index_open_neither_writes_nor_waits_for_a_held_write_lock() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("archive");
    let index = root.join(store::INDEX);
    let keeper = store::open(&root).unwrap();
    store::ensure_session(&keeper, "session-a").unwrap();
    assert_eq!(keeper.query_row("PRAGMA auto_vacuum", [], |r| r.get::<_, i64>(0)).unwrap(), 2, "new index is incremental");

    // Another writer holds the index write lock, as the live writer does mid-commit.
    let writer = Connection::open(&index).unwrap();
    writer.execute_batch("BEGIN IMMEDIATE; INSERT INTO store_meta(key,value) VALUES('held','1');").unwrap();
    let before = wal_len(&index);
    let r = root.clone();
    let opened = within(Duration::from_secs(3), move || {
        let t = std::time::Instant::now();
        let db = store::open(&r).map(|_| ());
        (db.map_err(|e| e.to_string()), t.elapsed())
    });
    let (result, took) = opened.expect("store::open blocked behind another connection's write lock");
    result.unwrap();
    assert!(took < Duration::from_millis(100), "open took {took:?}");
    assert_eq!(wal_len(&index), before, "open wrote to the WAL");
    writer.execute_batch("COMMIT").unwrap();

    // Real commits keep the full flush.
    let db = store::open(&root).unwrap();
    assert_eq!(db.query_row("PRAGMA synchronous", [], |r| r.get::<_, i64>(0)).unwrap(), 2, "synchronous=FULL");
    assert_eq!(db.query_row("PRAGMA fullfsync", [], |r| r.get::<_, i64>(0)).unwrap(), 1);
    assert_eq!(db.query_row("PRAGMA checkpoint_fullfsync", [], |r| r.get::<_, i64>(0)).unwrap(), 1);
    assert_eq!(db.query_row("PRAGMA journal_mode", [], |r| r.get::<_, String>(0)).unwrap(), "wal");
    assert_eq!(db.query_row("PRAGMA journal_size_limit", [], |r| r.get::<_, i64>(0)).unwrap(), 64 << 20);
}

#[test]
fn pack_open_neither_writes_nor_waits_for_a_held_write_lock() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("archive");
    let pack = "2026-09";
    store::put(&root, pack, &[("k".to_owned(), b"v".as_slice())]).unwrap();
    let path = store::pack_path(&root, pack).unwrap();
    let writer = Connection::open(&path).unwrap();
    writer.execute_batch("BEGIN IMMEDIATE; INSERT INTO blobs(key,added_ns,data) VALUES('held',0,x'00');").unwrap();
    let before = wal_len(&path);
    let r = root.clone();
    // The outer call holds the pool's idle connection, so the inner one opens a fresh connection.
    let opened = within(Duration::from_secs(3), move || {
        store::with_pack(&r, pack, |_| {
            store::with_pack(&r, pack, |inner| {
                Ok((
                    inner.query_row("PRAGMA auto_vacuum", [], |row| row.get::<_, i64>(0))?,
                    inner.query_row("PRAGMA synchronous", [], |row| row.get::<_, i64>(0))?,
                ))
            })
        })
        .map_err(|e| e.to_string())
    });
    let (vacuum, sync) = opened.expect("pack open blocked behind another connection's write lock").unwrap();
    assert_eq!((vacuum, sync), (2, 2), "pack is incremental and commits with synchronous=FULL");
    assert_eq!(wal_len(&path), before, "pack open wrote to the WAL");
    writer.execute_batch("COMMIT").unwrap();
}
