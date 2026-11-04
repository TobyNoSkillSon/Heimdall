//! Lab only (ignored): how long one compaction step holds the index write lock on main's code.
mod support { pub mod throttle; }
use heimdall_archive::{compact, hash, records::DICTIONARY, store, wall_ns, DAY_NS};
use rusqlite::params;
use std::{sync::{atomic::{AtomicBool, Ordering}, Arc}, time::{Duration, Instant}};

fn fill(root: &std::path::Path, old: usize, hot: usize) {
    let db = store::open(root).unwrap();
    let sid = store::ensure_session(&db, "session-lab").unwrap();
    let now = wall_ns();
    let mut enc = zstd::bulk::Compressor::with_dictionary(1, DICTIONARY).unwrap();
    db.execute_batch("BEGIN").unwrap();
    for i in 0..(old + hot) {
        let wall = if i < old { now - 2 * DAY_NS + i as u64 } else { now - 1000 + i as u64 };
        let body = serde_json::to_vec(&serde_json::json!({"model":"lab","input":{"source":"mic:lab","start_ns":i,"end_ns":i+32},"probability":(i % 97) as f64 / 97.0,"pad":"x".repeat(600)})).unwrap();
        let id = hash(&body);
        db.execute("INSERT OR IGNORE INTO payloads VALUES(?1,?2,'json-zstd-dict-v1',?3,NULL)", params![id, body.len() as i64, enc.compress(&body).unwrap()]).unwrap();
        db.execute("INSERT INTO evidence(session,kind,source,utterance_ids,host_ns,wall_ns,payload_id) VALUES(?1,'model_output','mic:lab','[]',?2,?3,?4)", params![sid, i as i64, wall as i64, id]).unwrap();
    }
    db.execute_batch("COMMIT; PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
}

/// Longest interval in which a zero-wait writer could not take the lock.
fn probe(root: std::path::PathBuf, stop: Arc<AtomicBool>) -> std::thread::JoinHandle<Duration> {
    std::thread::spawn(move || {
        let c = rusqlite::Connection::open(root.join(store::INDEX)).unwrap();
        c.busy_timeout(Duration::ZERO).unwrap();
        let (mut worst, mut since) = (Duration::ZERO, None::<Instant>);
        while !stop.load(Ordering::Acquire) {
            match c.execute_batch("BEGIN IMMEDIATE; ROLLBACK") {
                Ok(()) => { if let Some(s) = since.take() { worst = worst.max(s.elapsed()); } }
                Err(_) => { let _ = c.execute_batch("ROLLBACK"); since.get_or_insert_with(Instant::now); }
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        worst
    })
}

#[test]
#[ignore]
fn lab_pack_evidence_hold() {
    if std::env::var("LAB_THROTTLE").is_ok() { support::throttle::install(); }
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("archive");
    let old: usize = std::env::var("LAB_OLD").ok().and_then(|v| v.parse().ok()).unwrap_or(8192);
    let hot: usize = std::env::var("LAB_HOT").ok().and_then(|v| v.parse().ok()).unwrap_or(200_000);
    let t0 = Instant::now();
    fill(&root, old, hot);
    eprintln!("filled {old}+{hot} in {:.1} s, index {} MB", t0.elapsed().as_secs_f64(), std::fs::metadata(root.join(store::INDEX)).unwrap().len() >> 20);
    let stop = Arc::new(AtomicBool::new(false));
    let p = probe(root.clone(), stop.clone());
    std::thread::sleep(Duration::from_millis(50));
    let r = root.clone();
    let (n, step) = std::thread::spawn(move || {
        if std::env::var("LAB_BG").is_ok() { unsafe { libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_BACKGROUND, 0); } }
        let t1 = Instant::now();
        let n = compact::compact_metadata(&r, wall_ns(), false).unwrap();
        (n, t1.elapsed())
    }).join().unwrap();
    stop.store(true, Ordering::Release);
    eprintln!("throttled calls {}", support::throttle::THROTTLED.load(Ordering::Relaxed));
    eprintln!("packed {n} rows in {:.3} s; longest lock hold seen by a probe: {:.3} s", step.as_secs_f64(), p.join().unwrap().as_secs_f64());
}

/// Lab: where a throttled compaction pass spends its time on the hardening world (branch code).
#[test]
#[ignore]
fn lab_pass_timing() {
    support::throttle::install();
    support::throttle::set_io(Duration::from_micros(300));
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("archive");
    fill(&root, 1024, 20_000);
    {
        let db = store::open(&root).unwrap();
        let sid = store::ensure_session(&db, "session-lab").unwrap();
        let seg = store::create_segment(&db, sid, "capture-lab", 1_000_000_000, 0, "native").unwrap();
        let now = wall_ns();
        for i in 0..64usize {
            let pcm: Vec<f32> = (0..16000).map(|k| (((k + i * 7919) as f32 * 0.071).sin() * 6000.).round() / 32768.).collect();
            let id = heimdall_archive::audio::put_hot(&db, 16000, 1, &pcm).unwrap();
            let ns = i as u64 * 1_000_000_000;
            db.execute("INSERT INTO blocks(segment,source_id,host_ns,rate,channels,frames,codec,discontinuity,object_id,offset_frames,wall_ns) VALUES(?1,'mic',?2,16000,1,16000,'f32-zstd',0,?3,0,?4)", params![seg, ns as i64, id, (now - 3 * DAY_NS + ns) as i64]).unwrap();
        }
    }
    let r = root.clone();
    std::thread::spawn(move || {
        unsafe { libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_BACKGROUND, 0); }
        for pass in 0..20 {
            let t0 = Instant::now();
            let receipt = compact::compact_pass(&r, wall_ns(), &mut || true).unwrap();
            eprintln!("pass {pass}: {:.2} s {receipt:?}", t0.elapsed().as_secs_f64());
            if !(receipt.payloads > 0 || receipt.chunks > 0 || receipt.audio_groups > 0 || receipt.images > 0) { break; }
        }
    }).join().unwrap();
}

/// Lab: what one screen frame costs through the writer when nothing else keeps it alive (screen's buffer opens a
/// writer per frame).
#[test]
#[ignore]
fn lab_video_frame_cost() {
    use heimdall_archive::video::{Format, Geometry, Sample, Writer};
    let t = tempfile::tempdir().unwrap();
    let session = t.path().join("session-video");
    let geometry = Geometry { display: 1, x: 0., y: 0., width: 64, height: 32 };
    let format = Format { width: 64, height: 32, hvcc: vec![], parameters: vec![] };
    for round in 0..3 {
        let t0 = Instant::now();
        let w = Writer::open(&session).unwrap();
        let open = t0.elapsed();
        let t1 = Instant::now();
        let seg = w.begin(&geometry, &format, 1_000_000_000 + round * 10_000_000_000, 1, 5).unwrap();
        let begin = t1.elapsed();
        let t2 = Instant::now();
        w.append(seg, &Sample { host_ns: 1_000_000_000 + round * 10_000_000_000, end_ns: 1_200_000_000 + round * 10_000_000_000, key: true, data: vec![round as u8; 64] }).unwrap();
        let append = t2.elapsed();
        drop(w);
        eprintln!("round {round}: open {open:?} begin {begin:?} append {append:?}");
    }
}
