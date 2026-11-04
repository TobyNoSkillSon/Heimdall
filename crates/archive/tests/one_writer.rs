//! The one rule the store keeps: in a process every index write goes through the store's writer (`live.rs`). The
//! commit hook counts commits made anywhere else (`store::ungated_writes`), the WITHOUT ROWID tables and schema changes
//! included. Live writers, capture's kind of rows, images, logs, sessions and a compaction pass run here together; the
//! count must not move.
use heimdall_archive::{compact, live, records, store, timeline, wall_ns, Journal, DAY_NS};
use serde_json::json;
use std::time::Duration;

#[test]
fn every_index_write_goes_through_the_writer() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("archive");
    let session = store::session_path(&root, "session-one-writer");
    // Old work for the compactor, written through the writer as Heimdall would have.
    let journal = Journal::open(&session).unwrap();
    for i in 0..300u64 {
        journal.append(&heimdall_archive::Record { kind: "model_output".into(), source: "mic".into(), utterance_ids: vec![], host_ns: i, wall_ns: 1 + i, payload: json!({"n": i}) }).unwrap();
    }
    let db = store::open(&root).unwrap();
    let sid = store::ensure_session(&db, "session-one-writer").unwrap();
    let seg = store::create_segment(&db, sid, "capture-one-writer", 1_000_000_000, 0, "native").unwrap();
    let pcm: Vec<f32> = (0..16000).map(|k| ((k as f32 * 0.07).sin() * 6000.).round() / 32768.).collect();
    let object = heimdall_archive::audio::put_hot(&db, 16000, 1, &pcm).unwrap();
    live::Queue::shared(&root)
        .unwrap()
        .call(move |w| {
            w.execute("INSERT INTO blocks(segment,source_id,host_ns,rate,channels,frames,codec,discontinuity,object_id,offset_frames,wall_ns) VALUES(?1,'mic',0,16000,1,16000,'f32-zstd',0,?2,0,1)", rusqlite::params![seg, object])?;
            Ok(())
        })
        .unwrap();
    assert!(live::settle(&root, Duration::from_secs(10)));

    let before = store::ungated_writes();
    let writer = records::Writer::open(&session).unwrap();
    for i in 0..200 {
        writer.event("model_output", "mic:0", &[], i, json!({"model":"silero","n":i})).unwrap();
        journal.note("listen_event", "mic", &[], i, json!({"n": i})).unwrap();
    }
    journal.event("command_decision", "mic", &[], 1, json!({"approved": "x"})).unwrap();
    journal.vector(&[0.5, 0.25]).unwrap();
    journal.image(b"\x89PNG not really", "image/png").unwrap();
    store::archive_log(&session, "run.log", b"a run's log").unwrap();
    Journal::open(&store::session_path(&root, "session-another")).unwrap();
    let queue = live::Queue::shared(&root).unwrap();
    queue.write(move |w| timeline::transaction(w, || Ok(()))).unwrap();
    let now = wall_ns() + 2 * DAY_NS;
    for _ in 0..10 {
        compact::compact_pass(&root, now, &mut || true).unwrap();
    }
    store::mark_complete(&session).unwrap();
    writer.flush().unwrap();
    assert!(live::settle(&root, Duration::from_secs(10)));
    let after = store::ungated_writes();
    let packed: i64 = db.query_row("SELECT count(*) FROM cold_chunks", [], |r| r.get(0)).unwrap();
    assert!(packed > 0, "the compaction pass did its work");
    assert_eq!(after - before, 0, "{} index commits were made outside the writer", after - before);
}
