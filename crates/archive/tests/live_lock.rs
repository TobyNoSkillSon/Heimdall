//! Live writes under a held archive lock (review 20 Oct, item 10): `BEGIN IMMEDIATE` on the live loop could wait
//! out `store::BUSY` (60 s, the watchdog's freeze limit). A live write now waits at most its short busy timeout,
//! then queues in order behind the lock; nothing is dropped and later writes see earlier ones.
use anyhow::Result;
use heimdall_archive::{live, store, timeline, Journal};
use rusqlite::params;
use serde_json::json;
use std::{
    path::Path,
    sync::mpsc,
    time::{Duration, Instant},
};


/// Holds the index write lock from another connection, like a long compaction or backfill commit, until the
/// returned sender is used (or dropped).
struct Held {
    release: mpsc::Sender<()>,
    holder: std::thread::JoinHandle<Result<()>>,
}
impl Held {
    fn release(self) -> Result<()> {
        let _ = self.release.send(());
        self.holder.join().unwrap()
    }
}
fn hold_lock(root: &Path) -> Held {
    let root = root.to_owned();
    let (locked, wait_locked) = mpsc::channel();
    let (release, released) = mpsc::channel::<()>();
    let holder = std::thread::spawn(move || -> Result<()> {
        let other = store::open(&root)?;
        other.execute_batch("BEGIN IMMEDIATE")?;
        other.execute("INSERT INTO sessions(name,started_wall_ns) VALUES('session-holder',0)", [])?;
        locked.send(()).unwrap();
        let _ = released.recv_timeout(Duration::from_secs(30));
        other.execute_batch("COMMIT")?;
        Ok(())
    });
    wait_locked.recv().unwrap();
    Held { release, holder }
}

fn upsert(db: &rusqlite::Connection, sid: i64, id: &str, text: &str) -> Result<()> {
    db.execute(
        "INSERT OR REPLACE INTO utterances(session,id,wall_ms,source_id,speaker,start_ns,end_ns,text,words) VALUES(?1,?2,0,'mic','User',0,1,?3,'[]')",
        params![sid, id, text],
    )?;
    timeline::utterance(db, sid, id)
}

fn text(db: &rusqlite::Connection, id: &str) -> Option<String> {
    db.query_row("SELECT text FROM utterances WHERE id=?1", [id], |r| r.get(0)).ok()
}

#[test]
fn live_writes_queue_behind_a_held_lock_in_order_and_never_wait_long() -> Result<()> {
    let t = tempfile::tempdir()?;
    let sid = Journal::open(&t.path().join("session-live"))?.session_id();
    let queue = live::Queue::shared(t.path())?;
    let db = store::open(t.path())?;
    let holder = hold_lock(t.path());
    let started = Instant::now();
    let mut slowest = Duration::ZERO;
    let mut results = vec![];
    // A final, then a write that depends on it (read, then write), then twenty more: the live loop's shape.
    let mut timed = |job: Box<dyn FnMut(&rusqlite::Connection) -> Result<()> + Send>| -> Result<()> {
        let at = Instant::now();
        results.push(queue.write(job)?);
        slowest = slowest.max(at.elapsed());
        Ok(())
    };
    timed(Box::new(move |db| timeline::transaction(db, || upsert(db, sid, "a", "first"))))?;
    timed(Box::new(move |db| timeline::transaction(db, || {
        let before: String = db.query_row("SELECT text FROM utterances WHERE id='a'", [], |r| r.get(0))?;
        upsert(db, sid, "a", &format!("{before} revised"))
    })))?;
    for i in 0..20 {
        timed(Box::new(move |db| timeline::transaction(db, || upsert(db, sid, &format!("n{i}"), "later"))))?;
    }
    let queued_in = started.elapsed();
    // Only the first write waited, and only `COMMIT_WAIT`; the rest queued at once.
    assert!(slowest <= live::COMMIT_WAIT + Duration::from_millis(200), "one live write waited {slowest:?}");
    assert!(queued_in < live::COMMIT_WAIT + Duration::from_millis(500), "22 live writes took {queued_in:?} while the lock was held");
    assert!(results.iter().all(|w| *w == live::Written::Queued), "{results:?}");
    assert_eq!(queue.pending(), 22);
    assert!(text(&db, "a").is_none(), "nothing lands while the lock is held");
    holder.release()?;
    assert!(queue.settle(Duration::from_secs(10)), "the queue drains once the lock is free");
    assert_eq!(text(&db, "a").as_deref(), Some("first revised"), "the dependent write saw the earlier one");
    let n: i64 = db.query_row("SELECT count(*) FROM utterances WHERE id LIKE 'n%'", [], |r| r.get(0))?;
    assert_eq!(n, 20, "no write dropped");
    // Lock free and nothing queued: the next write is committed before it returns.
    assert_eq!(queue.write(move |db| timeline::transaction(db, || upsert(db, sid, "b", "now")))?, live::Written::Now);
    assert_eq!(text(&db, "b").as_deref(), Some("now"));
    Ok(())
}

/// The writer outwaits a lock held longer than its own busy timeout: it retries without end.
#[test]
fn queued_write_survives_a_lock_held_past_one_attempt() -> Result<()> {
    let t = tempfile::tempdir()?;
    let journal = Journal::open(&t.path().join("session-live"))?;
    let holder = hold_lock(t.path());
    let started = Instant::now();
    for i in 0..50 {
        journal.event("listen_event", "mic", &[], i, json!({"event":"final_utterance","n":i}))?;
    }
    assert!(started.elapsed() < live::COMMIT_WAIT + Duration::from_millis(500), "50 evidence events took {:?}", started.elapsed());
    assert!(journal_pending(t.path()) > 0);
    // Longer than the writer's busy wait per attempt: it tries again.
    std::thread::sleep(live::BUSY + Duration::from_millis(500));
    holder.release()?;
    assert!(live::settle(t.path(), Duration::from_secs(10)), "settle by store root sees the queue");
    let n: Vec<u64> = journal.records()?.iter().filter(|r| r.kind == "listen_event").map(|r| r.payload["n"].as_u64().unwrap()).collect();
    assert_eq!(n, (0..50).collect::<Vec<_>>(), "every event, in order");
    Ok(())
}
fn journal_pending(root: &Path) -> usize {
    live::Queue::shared(root).unwrap().pending()
}

/// Images and vectors from a journal take the same writer.
#[test]
fn journal_images_queue_too() -> Result<()> {
    let t = tempfile::tempdir()?;
    let j = Journal::open(&t.path().join("session-live"))?;
    let holder = hold_lock(t.path());
    let png = b"\x89PNG not really".to_vec();
    let id = j.image(&png, "image/png")?;
    let vector = j.vector(&[0.5, 0.25])?;
    j.event("screenshot", "screen", &[], 1, json!({"image_id": id}))?;
    assert_eq!(journal_pending(t.path()), 3);
    holder.release()?;
    assert!(live::settle(t.path(), Duration::from_secs(10)));
    assert_eq!(j.image_bytes(&id)?.1, png);
    assert_eq!(j.vector_values(&vector)?, vec![0.5, 0.25]);
    Ok(())
}

/// A write that fails for its own reason (not the environment) is dropped alone: a waiting caller hears it, the
/// writes around it commit, and later callers never hear it.
#[test]
fn a_job_error_drops_that_job_alone() -> Result<()> {
    let t = tempfile::tempdir()?;
    let sid = Journal::open(&t.path().join("session-live"))?.session_id();
    let queue = live::Queue::shared(t.path())?;
    let db = store::open(t.path())?;
    let failed = queue.write(|db| timeline::transaction(db, || { db.execute("INSERT INTO no_such_table VALUES(1)", [])?; Ok(()) }));
    assert!(format!("{:#}", failed.unwrap_err()).contains("no_such_table"), "the waiting caller hears its own error");
    let holder = hold_lock(t.path());
    queue.write(move |db| timeline::transaction(db, || upsert(db, sid, "before", "kept")))?;
    queue.enqueue(|db| timeline::transaction(db, || { db.execute("INSERT INTO no_such_table VALUES(2)", [])?; Ok(()) }))?;
    queue.write(move |db| timeline::transaction(db, || upsert(db, sid, "after", "kept")))?;
    holder.release()?;
    assert!(queue.settle(Duration::from_secs(10)));
    assert_eq!(text(&db, "before").as_deref(), Some("kept"));
    assert_eq!(text(&db, "after").as_deref(), Some("kept"), "the writes around a failed one commit");
    assert_eq!(queue.write(move |db| timeline::transaction(db, || upsert(db, sid, "next", "ok")))?, live::Written::Now, "nobody hears it later");
    Ok(())
}

/// Production numbers: a live caller waits at most a second, far from the watchdog's limit.
#[test]
fn live_wait_is_far_below_the_watchdog_limit() {
    assert!(live::COMMIT_WAIT <= Duration::from_secs(1));
    assert!(live::BUSY <= store::BUSY);
}

/// Dropping the last handle never blocks (the lock is still held), and the writer still commits what was queued
/// once the lock frees.
#[test]
fn dropping_the_queue_keeps_its_writes() -> Result<()> {
    let t = tempfile::tempdir()?;
    let sid = Journal::open(&t.path().join("session-live"))?.session_id();
    let db = store::open(t.path())?;
    let queue = live::Queue::shared(t.path())?;
    let holder = hold_lock(t.path());
    assert_eq!(queue.write(move |db| timeline::transaction(db, || upsert(db, sid, "late", "kept")))?, live::Written::Queued);
    let at = Instant::now();
    drop(queue);
    assert!(at.elapsed() < Duration::from_millis(100), "drop waited {:?}", at.elapsed());
    holder.release()?;
    let until = Instant::now() + Duration::from_secs(10);
    while text(&db, "late").is_none() && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(text(&db, "late").as_deref(), Some("kept"));
    Ok(())
}

/// Every writer of one store gets the same writer, so the projection, its journal and the send-context journal keep
/// one order; another store has its own.
#[test]
fn writers_of_one_store_share_one_writer() -> Result<()> {
    let t = tempfile::tempdir()?;
    let other = tempfile::tempdir()?;
    let a = live::Queue::shared(t.path())?;
    let holder = hold_lock(t.path());
    a.enqueue(|db| timeline::transaction(db, || Ok(())))?;
    assert_eq!(live::Queue::shared(t.path())?.pending(), 1, "the second handle sees the first one's queue");
    assert_eq!(live::Queue::shared(other.path())?.pending(), 0);
    holder.release()?;
    assert!(live::settle(t.path(), Duration::from_secs(10)));
    Ok(())
}

/// Review, 1 Nov: `settle` waited for an empty queue, which a steady stream of listen notes (124 a second) never
/// leaves. It waits for the writes handed over before it was called.
#[test]
fn settle_waits_for_earlier_writes_not_for_an_empty_queue() -> Result<()> {
    let t = tempfile::tempdir()?;
    let journal = Journal::open(&t.path().join("session-live"))?;
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let notes = {
        let (stop, session) = (stop.clone(), t.path().join("session-live"));
        std::thread::spawn(move || {
            let j = Journal::open(&session).unwrap();
            let mut n = 0u64;
            while !stop.load(std::sync::atomic::Ordering::Acquire) {
                j.note("listen_event", "mic", &[], n, json!({"n": n})).unwrap();
                n += 1;
                // Four sources at 31 frames a second.
                std::thread::sleep(Duration::from_millis(8));
            }
        })
    };
    std::thread::sleep(Duration::from_millis(200));
    journal.event("mine", "test", &[], 1, json!({}))?;
    let started = Instant::now();
    assert!(live::settle(t.path(), Duration::from_secs(10)), "settled behind a steady stream of notes");
    assert!(started.elapsed() < Duration::from_secs(2), "settle took {:?}", started.elapsed());
    stop.store(true, std::sync::atomic::Ordering::Release);
    notes.join().unwrap();
    Ok(())
}

/// Review, 1 Nov: a background job that panicked mid-transaction left it open, and the next live write was dropped
/// with "cannot start a transaction within a transaction". The writer rolls it back and goes on.
#[test]
fn a_panicking_background_job_costs_no_live_write() -> Result<()> {
    let t = tempfile::tempdir()?;
    let sid = Journal::open(&t.path().join("session-live"))?.session_id();
    let queue = live::Queue::shared(t.path())?;
    let failed = queue.background(|db| -> Result<()> {
        db.execute("INSERT INTO sessions(name,started_wall_ns) VALUES('half-done',0)", [])?;
        panic!("a background job's bug");
    });
    assert!(format!("{:#}", failed.unwrap_err()).contains("panicked"));
    assert_eq!(queue.write(move |db| timeline::transaction(db, || upsert(db, sid, "after", "kept")))?, live::Written::Now);
    let db = store::open(t.path())?;
    assert_eq!(text(&db, "after").as_deref(), Some("kept"));
    assert!(store::find_session(&db, "half-done")?.is_none(), "the panicked job's write rolled back");
    Ok(())
}

/// Bug note 2026-11-02T15-58-15: a `call_within` that runs out of time is a `Busy`, not a failure, and its caller can
/// see when the writer reached the job it stopped waiting for (the screen recorder starts its next epoch then).
#[test]
fn a_call_past_its_wait_is_busy_and_says_when_the_writer_reached_it() -> Result<()> {
    let t = tempfile::tempdir()?;
    Journal::open(&t.path().join("session-busy"))?;
    let queue = live::Queue::shared(t.path())?;
    let holder = hold_lock(t.path());
    let error = queue.call_within(Duration::from_millis(200), |db| Ok(db.execute("INSERT INTO sessions(name,started_wall_ns) VALUES('session-late',0)", [])?)).unwrap_err();
    let busy = live::busy(&error).expect("a held lock is busy, not a failure").clone();
    assert_eq!(error.to_string(), "the archive is busy", "the words callers and tests already match");
    assert!(!busy.settled(), "the writer has not reached it yet");
    // The writer is named while it waits: the step it is in now.
    let (what, _) = queue.holder().expect("the writer is in a step");
    assert!(what.contains("write lock") || what.contains("live write"), "{what}");
    holder.release()?;
    assert!(queue.settle(Duration::from_secs(30)));
    assert!(busy.settled(), "the job it gave up on ran once the lock was free");
    let db = store::open(t.path())?;
    assert_eq!(db.query_row("SELECT count(*) FROM sessions WHERE name='session-late'", [], |r| r.get::<_, i64>(0))?, 1, "and it committed, once");
    Ok(())
}

/// 2 Nov 15:51 the writer stood still 110 s and the log never said what held it: a slow step is named by the code
/// that handed it over, while it runs and when it ends.
#[test]
fn a_slow_writer_step_is_named_by_the_code_that_handed_it_over() -> Result<()> {
    let t = tempfile::tempdir()?;
    Journal::open(&t.path().join("session-slow"))?;
    let queue = live::Queue::shared(t.path())?;
    let (entered, inside) = mpsc::channel();
    let (release, released) = mpsc::channel::<()>();
    let (entered, released) = (std::sync::Mutex::new(entered), std::sync::Mutex::new(released));
    let worker = {
        let queue = queue.clone();
        std::thread::spawn(move || queue.background(move |_| {
            entered.lock().unwrap().send(()).unwrap();
            let _ = released.lock().unwrap().recv_timeout(Duration::from_secs(30));
            Ok(())
        }))
    };
    inside.recv_timeout(Duration::from_secs(30))?;
    let (what, _) = queue.holder().expect("the writer is in the background job");
    assert!(what.starts_with("a background job from ") && what.contains("tests/live_lock.rs:"), "{what}");
    release.send(()).unwrap();
    worker.join().unwrap()?;
    assert!(queue.settle(Duration::from_secs(5)));
    let deadline = Instant::now() + Duration::from_secs(5);
    while queue.holder().is_some() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(queue.holder().is_none(), "nothing holds the writer once the job is done");
    Ok(())
}
