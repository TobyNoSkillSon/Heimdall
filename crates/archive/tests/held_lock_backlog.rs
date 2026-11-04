//! 31 Oct 22:20-22:24: the archive's write lock was held, 6,489 live writes queued behind it, and the background
//! writer then drained them one transaction and one F_FULLFSYNC each, back to back. SQLite's busy handler re-polls at
//! most every 100 ms, so for the whole drain every other writer that waited for the lock (the lighter voice's evidence,
//! the listen evidence writer, capture) missed every gap: speech went 118 s without audio, the evidence writer gave up
//! after its 60 s wait and Listening failed over 1,000 times. Measured on an isolated copy (lab/archive-lock-1031): one
//! commit per job starved a second writer outright; grouped commits with a short yield let it in within 41 ms.
use heimdall_archive::{live, store, timeline};
use std::{
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

const JOBS: usize = 3000;

#[test]
fn a_backlog_behind_a_held_lock_never_starves_another_writer() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("archive");
    store::open(&root).unwrap().execute_batch("CREATE TABLE IF NOT EXISTS backlog_test(k TEXT PRIMARY KEY)").unwrap();

    // Another writer holds the index write lock (a long compaction step, a slow commit).
    let holder = store::open(&root).unwrap();
    holder.execute_batch("BEGIN IMMEDIATE; INSERT INTO store_meta(key,value) VALUES('held','1');").unwrap();

    // The live loop's writes: the first waits its short live wait, then it and every later one queue in order.
    let queue = live::Queue::shared(&root).unwrap();
    for i in 0..JOBS {
        queue
            .write(move |db| {
                timeline::transaction(db, || {
                    db.execute("INSERT OR REPLACE INTO backlog_test(k) VALUES(?1)", [format!("job-{i}")])?;
                    Ok(())
                })
            })
            .unwrap();
    }
    assert!(queue.pending() > 0, "the writes queued behind the held lock");

    // Speech's evidence (or the listen evidence writer) waits for the lock with a bounded busy timeout throughout.
    let draining = Arc::new(AtomicBool::new(true));
    let (failed, ok) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let other = {
        let (root, draining, failed, ok) = (root.clone(), draining.clone(), failed.clone(), ok.clone());
        std::thread::spawn(move || {
            let o = store::open(&root).unwrap();
            o.busy_timeout(Duration::from_secs(1)).unwrap();
            let mut worst = Duration::ZERO;
            let mut n = 0;
            while draining.load(Ordering::Acquire) || n < 3 {
                let t = Instant::now();
                match o.execute_batch("BEGIN IMMEDIATE; INSERT INTO store_meta(key,value) VALUES('other','1') ON CONFLICT(key) DO UPDATE SET value=value+1; COMMIT;") {
                    Ok(()) => {
                        ok.fetch_add(1, Ordering::Relaxed);
                        worst = worst.max(t.elapsed());
                    }
                    Err(_) => {
                        let _ = o.execute_batch("ROLLBACK");
                        failed.fetch_add(1, Ordering::Relaxed);
                    }
                }
                n += 1;
                std::thread::sleep(Duration::from_millis(30));
            }
            worst
        })
    };

    std::thread::sleep(Duration::from_millis(400));
    holder.execute_batch("COMMIT").unwrap();
    let released = Instant::now();
    assert!(queue.settle(Duration::from_secs(60)), "the backlog drains");
    let drain = released.elapsed();
    draining.store(false, Ordering::Release);
    let worst = other.join().unwrap();

    let rows: i64 = store::open(&root).unwrap().query_row("SELECT count(*) FROM backlog_test", [], |r| r.get(0)).unwrap();
    assert_eq!(rows as usize, JOBS, "nothing queued is lost");
    assert_eq!(failed.load(Ordering::Relaxed), 0, "another writer timed out {} times during a {drain:?} drain ({} got in, worst wait {worst:?})", failed.load(Ordering::Relaxed), ok.load(Ordering::Relaxed));
    assert!(worst < Duration::from_millis(900), "another writer waited {worst:?} for the lock during the drain");
    assert!(drain < Duration::from_secs(8), "{JOBS} queued writes took {drain:?} to drain");
}

/// 31 Oct 22:21:13: the listen evidence writer waited its one 60 s attempt for the lock, stopped for good, and every
/// listen event after it failed ("evidence worker failed", over 1,000 lines). It now waits as long as the lock is
/// held, keeps taking records meanwhile, and commits all of them in order once it is free.
#[test]
fn the_evidence_writer_outlasts_a_held_lock_and_keeps_every_record() {
    use heimdall_archive::{records, Journal};
    let temp = tempfile::tempdir().unwrap();
    let session = temp.path().join("archive").join("session-held");
    let writer = records::Writer::open(&session).unwrap();
    let root = store::locate(&session).unwrap().0;
    let holder = store::open(&root).unwrap();
    holder.execute_batch("BEGIN IMMEDIATE; INSERT INTO store_meta(key,value) VALUES('held','1');").unwrap();
    let started = Instant::now();
    let mut n = 0u64;
    // Over ten of the writer's lock waits, and more records than its queue holds.
    while started.elapsed() < Duration::from_millis(800) || n < 5000 {
        writer.event("model_output", "mic:test", &[], n, serde_json::json!({"n": n})).unwrap();
        n += 1;
        if n % 500 == 0 {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    let queue = heimdall_archive::live::Queue::shared(&root).unwrap();
    assert!(queue.pending() > 0, "the records wait behind the held lock");
    holder.execute_batch("ROLLBACK").unwrap();
    writer.flush().unwrap();
    assert_eq!(queue.pending(), 0, "caught up");
    let kept = Journal::open_read_only(&session).unwrap().records().unwrap();
    assert_eq!(kept.len() as u64, n, "every record committed");
    assert!(kept.iter().enumerate().all(|(i, r)| r.payload["n"] == i as u64), "in order");
}

/// The host clock starts again at every boot, so an old chunk of the same source can cover the current time: then
/// every new VAD frame (31 a second per source) read, decompressed and decoded that chunk again inside the evidence
/// writer's transaction. It is decoded once now.
#[test]
fn vad_frames_inside_an_old_boots_chunk_decode_it_once() {
    use heimdall_archive::{chunks, compact, Journal, Record};
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("archive");
    let old = Journal::open(&store::session_path(&root, "session-old-boot")).unwrap();
    let frame = |i: u64, p: f64| Record {
        kind: "model_output".into(),
        source: "mic:BuiltIn:93".into(),
        utterance_ids: vec![],
        host_ns: i * 32_000_000,
        wall_ns: 1,
        payload: serde_json::json!({"model":"silero","input":{"source":"mic:BuiltIn:93","start_ns":i*32_000_000,"end_ns":(i+1)*32_000_000},"probability":p}),
    };
    for i in 0..256 {
        old.append(&frame(i, 0.01)).unwrap();
    }
    assert_eq!(compact::compact_metadata(&root, heimdall_archive::DAY_NS + 100, false).unwrap(), 1, "the old boot's frames are one cold chunk");
    let before = chunks::payload_chunk_decodes();
    let writer = heimdall_archive::records::Writer::open(&store::session_path(&root, "session-this-boot")).unwrap();
    for i in 0..200 {
        // New frames, same source, the same stretch of the (restarted) host clock.
        writer.event("model_output", "mic:BuiltIn:93", &[], i * 32_000_000, frame(i, 0.5).payload).unwrap();
    }
    writer.flush().unwrap();
    assert_eq!(chunks::payload_chunk_decodes() - before, 1, "one decode of the old chunk for 200 frames");
}
