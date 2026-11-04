//! Write-lock contention on the shared archive index (19 Oct: "Pipeline error: database is locked"
//! dropped live finals and set "Listening interrupted"; "timeline backfill delayed: database is
//! locked" ×36). Every projection write must wait for a concurrent writer, never fail at once.
use anyhow::Result;
use heimdall_archive::{compact, store, timeline, Journal, Record, DAY_NS};
use rusqlite::params;
use serde_json::json;
use std::{
    sync::{atomic::{AtomicBool, Ordering}, mpsc, Arc},
    time::{Duration, Instant},
};
const S: u64 = 1_000_000_000;
const WALL: u64 = 1_800_000_000_000_000_000;

fn event(j: &Journal, kind: &str, host: u64, payload: serde_json::Value) -> Result<()> {
    j.append(&Record { kind: kind.into(), source: "mic".into(), host_ns: host, wall_ns: WALL + host, utterance_ids: vec![], payload })
}
fn final_row(db: &rusqlite::Connection, sid: i64, id: &str, start: u64, text: &str) -> Result<()> {
    let words = json!([{"text": text, "start_ns": start, "end_ns": start + S / 2}]);
    db.execute(
        "INSERT OR REPLACE INTO utterances(session,id,wall_ms,source_id,speaker,start_ns,end_ns,text,words) VALUES(?1,?2,?3,'mic','User',?4,?5,?6,?7)",
        params![sid, id, ((WALL + start) / 1_000_000) as i64, start as i64, (start + S) as i64, text, words.to_string()],
    )?;
    Ok(())
}

/// The live shape (Index::revise_transcript, embedding writes): read, then write, while the
/// evidence writer holds the lock. Before the fix this failed in microseconds with SQLITE_BUSY.
#[test]
fn read_then_write_projection_waits_for_a_writer_holding_the_lock() -> Result<()> {
    let t = tempfile::tempdir()?;
    let live = store::open(t.path())?;
    let j = Journal::open(&t.path().join("session-live"))?;
    let sid = j.session_id();
    final_row(&live, sid, "a", S, "hello")?;
    let root = t.path().to_owned();
    let (locked, wait_locked) = mpsc::channel();
    let holder = std::thread::spawn(move || -> Result<()> {
        let other = store::open(&root)?;
        other.execute_batch("BEGIN IMMEDIATE")?;
        other.execute("INSERT INTO sessions(name,started_wall_ns) VALUES('session-other',0)", [])?;
        locked.send(()).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        other.execute_batch("COMMIT")?;
        Ok(())
    });
    wait_locked.recv()?;
    let started = Instant::now();
    timeline::transaction(&live, || {
        let n: i64 = live.query_row("SELECT count(*) FROM utterances WHERE session=?1", [sid], |r| r.get(0))?;
        assert_eq!(n, 1);
        live.execute("DELETE FROM utterances WHERE session=?1 AND id='a'", [sid])?;
        final_row(&live, sid, "a", S, "hello again")?;
        timeline::utterance(&live, sid, "a")
    })?;
    assert!(started.elapsed() >= Duration::from_millis(200), "the write waited for the holder");
    holder.join().unwrap()?;
    let text: String = live.query_row("SELECT text FROM utterances WHERE id='a'", [], |r| r.get(0))?;
    assert_eq!(text, "hello again");
    assert!(live.is_autocommit(), "no transaction leaks out of the projection");
    Ok(())
}

/// A writer that commits between the projection's read and its write. With a deferred
/// transaction the write failed with SQLITE_BUSY_SNAPSHOT (517); now the other writer waits.
#[test]
fn commit_between_read_and_write_cannot_invalidate_the_projection() -> Result<()> {
    let t = tempfile::tempdir()?;
    let live = store::open(t.path())?;
    let j = Journal::open(&t.path().join("session-live"))?;
    let sid = j.session_id();
    final_row(&live, sid, "a", S, "hello")?;
    let root = t.path().to_owned();
    let (go, wait_go) = mpsc::channel::<()>();
    let other = std::thread::spawn(move || -> Result<()> {
        let other = Journal::open(&root.join("session-other"))?;
        wait_go.recv().unwrap();
        event(&other, "listen_event", 0, json!({"n": 1}))
    });
    timeline::transaction(&live, || {
        let _: i64 = live.query_row("SELECT count(*) FROM utterances", [], |r| r.get(0))?;
        go.send(()).unwrap();
        std::thread::sleep(Duration::from_millis(150));
        timeline::utterance(&live, sid, "a")
    })?;
    other.join().unwrap()?;
    Ok(())
}

/// Nested use keeps the caller's transaction (a savepoint) and rolls back only its own part.
#[test]
fn nested_projection_is_a_savepoint_of_the_callers_transaction() -> Result<()> {
    let t = tempfile::tempdir()?;
    let db = store::open(t.path())?;
    let j = Journal::open(&t.path().join("session-live"))?;
    let sid = j.session_id();
    db.execute_batch("BEGIN IMMEDIATE")?;
    final_row(&db, sid, "kept", S, "kept")?;
    let failed: Result<()> = timeline::transaction(&db, || {
        final_row(&db, sid, "dropped", 2 * S, "dropped")?;
        anyhow::bail!("injected")
    });
    assert!(failed.is_err());
    assert!(!db.is_autocommit(), "the caller's transaction is still open");
    db.execute_batch("COMMIT")?;
    let ids: Vec<String> = db.prepare("SELECT id FROM utterances ORDER BY id")?.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
    assert_eq!(ids, ["kept"]);
    Ok(())
}

/// The background backfill runs while the live evidence writer commits every few ms (the
/// 19 Oct call). It must finish without "database is locked", and the live writer never fails.
#[test]
fn backfill_completes_while_a_live_writer_commits_continuously() -> Result<()> {
    let t = tempfile::tempdir()?;
    let db = store::open(t.path())?;
    let j = Journal::open(&t.path().join("session-old"))?;
    let sid = j.session_id();
    for i in 0..300 {
        event(&j, "command_decision", i * S, json!({"phase":"chain","transcript":format!("command {i}")}))?;
    }
    for i in 0..200 {
        final_row(&db, sid, &format!("u{i}"), i * S, &format!("word{i}"))?;
    }
    db.execute("UPDATE evidence SET wall_ns=1", [])?;
    compact::compact_metadata(t.path(), DAY_NS + 100, false)?;
    for table in ["timeline_chunks", "chunk_links", "chunk_tags", "timeline_state", "timeline_finals", "timeline_backfill"] {
        db.execute(&format!("DELETE FROM {table}"), [])?;
    }
    let stop = Arc::new(AtomicBool::new(false));
    let written = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let root = t.path().to_owned();
    let flag = stop.clone();
    let progress = written.clone();
    let writer = std::thread::spawn(move || -> Result<usize> {
        let live = Journal::open(&root.join("session-live"))?;
        let db = store::open(&root)?;
        let live_sid = live.session_id();
        let mut n = 0;
        while !flag.load(Ordering::Relaxed) {
            event(&live, "listen_event", n as u64, json!({"n": n}))?;
            // The live final projection: read-then-write, as Index::revise_transcript does.
            timeline::transaction(&db, || {
                let _: i64 = db.query_row("SELECT count(*) FROM utterances WHERE session=?1", [live_sid], |r| r.get(0))?;
                final_row(&db, live_sid, &format!("live{n}"), n as u64 * S, "live")?;
                timeline::utterance(&db, live_sid, &format!("live{n}"))
            })?;
            n += 1;
            progress.store(n, Ordering::Relaxed);
        }
        Ok(n)
    });
    // Start only once the live writer is committing, so the two really overlap.
    while written.load(Ordering::Relaxed) < 3 && !writer.is_finished() {
        std::thread::sleep(Duration::from_millis(1));
    }
    // Backfill until the old session is indexed. The live session keeps getting rows while the writer runs, so a loop
    // that waits for "nothing left anywhere" only ends when backfill outpaces the writer, which a loaded machine
    // decides (seen in the install check, 28 Oct). The app runs backfill in budgeted ticks; what matters is that the
    // old work completes under the live writer's lock traffic.
    let mut passes = 0;
    let old_done = |db: &rusqlite::Connection| -> Result<bool> {
        let commands: i64 = db.query_row("SELECT count(*) FROM timeline_chunks WHERE role='command'", [], |r| r.get(0))?;
        let finals: i64 = db.query_row("SELECT count(*) FROM timeline_chunks WHERE role='utterance' AND kind='text' AND session=?1", [sid], |r| r.get(0))?;
        Ok(commands >= 300 && finals >= 200)
    };
    let result = (|| -> Result<()> {
        while !old_done(&db)? && timeline::backfill(t.path(), &mut || true)? > 0 {
            passes += 1;
            anyhow::ensure!(passes < 10_000, "backfill does not converge");
        }
        anyhow::ensure!(old_done(&db)?, "backfill stopped before the old session was indexed");
        Ok(())
    })();
    stop.store(true, Ordering::Relaxed);
    let live_writes = writer.join().unwrap()?;
    result?;
    assert!(live_writes > 3);
    let commands: i64 = db.query_row("SELECT count(*) FROM timeline_chunks WHERE role='command'", [], |r| r.get(0))?;
    assert_eq!(commands, 300);
    let old_finals: i64 = db.query_row("SELECT count(*) FROM timeline_chunks WHERE role='utterance' AND kind='text' AND session=?1", [sid], |r| r.get(0))?;
    assert_eq!(old_finals, 200);
    Ok(())
}
