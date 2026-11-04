//! Capture's commit (`archive::Archive::flush`, about once a second on the committer) while the archive compacts with
//! throttled background I/O (the model in `crates/archive/tests/support/throttle.rs`), on an isolated store. 1 Nov
//! 00:12: capture's commit waited behind compaction steps holding the write lock for minutes. A flush now waits at
//! most `COMMIT`, and every callback commits with its audio.
#[path = "../../archive/tests/support/throttle.rs"]
mod throttle;
use capture::{NativeFormat, SourceInfo, SourceKind};
use heimdall_archive::{compact, hash, records::DICTIONARY, store, wall_ns, DAY_NS};
use rusqlite::params;
use std::time::{Duration, Instant};

const COMMIT: Duration = Duration::from_millis(600);

/// Old records and old hot audio (compaction work), written directly.
fn world(root: &std::path::Path, old: usize) {
    let db = store::open(root).unwrap();
    let sid = store::ensure_session(&db, "session-old").unwrap();
    let seg = store::create_segment(&db, sid, "capture-old", 1_000_000_000, 0, "native").unwrap();
    let now = wall_ns();
    let mut enc = zstd::bulk::Compressor::with_dictionary(1, DICTIONARY).unwrap();
    db.execute_batch("BEGIN").unwrap();
    for i in 0..old {
        let body = serde_json::to_vec(&serde_json::json!({"model":"lab","n":i,"pad":"x".repeat(400)})).unwrap();
        let id = hash(&body);
        db.execute("INSERT OR IGNORE INTO payloads VALUES(?1,?2,'json-zstd-dict-v1',?3,NULL)", params![id, body.len() as i64, enc.compress(&body).unwrap()]).unwrap();
        db.execute("INSERT INTO evidence(session,kind,source,utterance_ids,host_ns,wall_ns,payload_id) VALUES(?1,'model_output','mic','[]',?2,?3,?4)", params![sid, i as i64, (now - 3 * DAY_NS + i as u64) as i64, id]).unwrap();
    }
    db.execute_batch("COMMIT").unwrap();
    for i in 0..48usize {
        let pcm: Vec<f32> = (0..16000).map(|k| (((k + i * 7919) as f32 * 0.071).sin() * 6000.).round() / 32768.).collect();
        let id = heimdall_archive::audio::put_hot(&db, 16000, 1, &pcm).unwrap();
        let ns = i as u64 * 1_000_000_000;
        db.execute(
            "INSERT INTO blocks(segment,source_id,host_ns,rate,channels,frames,codec,discontinuity,object_id,offset_frames,wall_ns) VALUES(?1,'mic',?2,16000,1,16000,'f32-zstd',0,?3,0,?4)",
            params![seg, ns as i64, id, (now - 3 * DAY_NS + ns) as i64],
        )
        .unwrap();
    }
}

#[test]
fn capture_commits_while_compaction_runs() {
    throttle::install();
    throttle::set_io(Duration::from_micros(300));
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("archive");
    world(&root, 1024);
    let mut archive = capture::Archive::create(&store::session_path(&root, "session-live")).unwrap();
    let format = NativeFormat { sample_rate: 16000, channels: 1 };
    let source = SourceInfo::new(SourceKind::InputDevice { name: "Synthetic".into(), uid: "hardening".into() }, format, 1);
    archive.add_source(&source, 0).unwrap();
    let compactor = compact::Compactor::start_gated(root.clone(), || true).unwrap();
    let (mut worst, mut callbacks) = (Duration::ZERO, 0u64);
    let started = Instant::now();
    let mut host = 0u64;
    // Fifteen seconds of 10 ms callbacks, flushed once a second like the committer.
    while started.elapsed() < Duration::from_secs(15) {
        for _ in 0..100 {
            let pcm: Vec<f32> = (0..160).map(|k| ((k as u64 + host / 1000) as f32 * 0.013).sin() * 0.2).collect();
            archive.append(&source.id, host, format, &pcm, false).unwrap();
            host += 10_000_000;
            callbacks += 1;
        }
        let t = Instant::now();
        archive.flush().unwrap();
        worst = worst.max(t.elapsed());
        std::thread::sleep(Duration::from_secs(1).saturating_sub(t.elapsed()));
    }
    drop(compactor);
    let blocks = archive.read_blocks(&source.id).unwrap();
    eprintln!("capture: {} flushes, longest {worst:?}, {} of {callbacks} callbacks committed", started.elapsed().as_secs(), blocks.len());
    assert_eq!(blocks.len() as u64, callbacks, "every callback committed");
    let short: Vec<_> = blocks.iter().enumerate().filter(|(_, b)| b.samples.len() != 160).map(|(i, b)| (i, b.samples.len())).take(5).collect();
    assert!(short.is_empty(), "every callback with its audio: {short:?}");
    assert!(worst <= COMMIT, "a capture commit waited {worst:?} (limit {COMMIT:?})");
}
