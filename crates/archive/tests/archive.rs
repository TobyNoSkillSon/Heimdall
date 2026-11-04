use heimdall_archive::{DAY_NS, Journal, Record, View, audio, compact, export, migrate, store};
use rusqlite::{Connection, params};
use serde_json::json;
use std::path::{Path, PathBuf};

const SESSION: &str = "session-test";
/// A store at `root` with one session and one capture segment holding a mic and a process source.
fn segment(root: &Path) -> (Connection, i64) {
    let db = store::open(root).unwrap();
    let session = store::ensure_session(&db, SESSION).unwrap();
    let segment = store::create_segment(&db, session, "capture-test", 1_000_000_000, 0, "native").unwrap();
    db.execute("INSERT INTO sources VALUES(?1,'mic','{\"kind\":{\"InputDevice\":{}},\"id\":\"mic\"}',0,NULL)", [segment]).unwrap();
    db.execute("INSERT INTO sources VALUES(?1,'process','{\"kind\":{\"Process\":{}},\"id\":\"process\"}',0,NULL)", [segment]).unwrap();
    (db, segment)
}
fn session(root: &Path) -> PathBuf {
    store::session_path(root, SESSION)
}
fn speech() -> Vec<f32> {
    (0..32000).map(|i| ((i as f32 * 0.079).sin() * 8192.).round() / 32768.).collect()
}
/// One hot block; its wall clock follows the segment's 1-s anchor like old schema-1 rows.
fn block_at(db: &Connection, segment: i64, source: &str, ns: u64, rate: u32, pcm: &[f32]) -> i64 {
    let id = audio::put_hot(db, rate, 1, pcm).unwrap();
    db.execute(
        "INSERT INTO blocks(segment,source_id,host_ns,rate,channels,frames,codec,discontinuity,object_id,offset_frames,wall_ns) VALUES(?1,?2,?3,?4,1,?5,'f32-zstd',0,?6,0,?7)",
        params![segment, source, ns as i64, rate, pcm.len() as i64, id, 1_000_000_000 + ns as i64],
    )
    .unwrap();
    db.last_insert_rowid()
}
fn block(db: &Connection, segment: i64, source: &str, ns: u64, pcm: &[f32]) -> i64 {
    block_at(db, segment, source, ns, 16000, pcm)
}
fn count(db: &Connection, sql: &str) -> i64 {
    db.query_row(sql, [], |r| r.get(0)).unwrap()
}
fn pack_blobs(root: &Path, pack: &str, prefix: &str) -> i64 {
    store::with_pack(root, pack, |p| Ok(p.query_row("SELECT count(*) FROM blobs WHERE key LIKE ?1||'%'", [prefix], |r| r.get(0))?)).unwrap()
}
fn all_blobs(root: &Path, prefix: &str) -> i64 {
    store::packs(root).unwrap().iter().map(|p| pack_blobs(root, p, prefix)).sum()
}
/// Nothing but the store: no per-launch folders, no loose content files.
fn assert_only_store(root: &Path) {
    for e in std::fs::read_dir(root).unwrap() {
        let name = e.unwrap().file_name().to_string_lossy().to_string();
        assert!(
            name.starts_with("heimdall.sqlite") || name == "packs" || name == ".compact.lock" || name == ".compact.gate" || name == ".compact.owner",
            "unexpected archive entry {name}"
        );
    }
    for e in std::fs::read_dir(root.join("packs")).unwrap() {
        let name = e.unwrap().file_name().to_string_lossy().to_string();
        assert!(name.split('.').next().is_some_and(|p| p == "hot" || p.len() == 7), "unexpected pack {name}");
    }
}

#[test]
fn hot_bits_exact_silence_and_identical_paths() {
    let t = tempfile::tempdir().unwrap();
    let (db, seg) = segment(t.path());
    let pcm = [0., -0., f32::from_bits(1), 1e-12, 0.1, -0.4];
    let a = block(&db, seg, "mic", 0, &pcm);
    block(&db, seg, "process", 0, &pcm);
    assert_eq!(count(&db, "SELECT count(*) FROM audio_objects"), 1);
    db.execute("INSERT INTO blocks(segment,source_id,host_ns,rate,channels,frames,codec,discontinuity,wall_ns) VALUES(?1,'mic',100000,16000,1,16000,'zero',0,1000100000)", [seg]).unwrap();
    let zero = db.last_insert_rowid();
    drop(db);
    let db = store::open(t.path()).unwrap();
    assert_eq!(audio::pcm_bytes(&audio::read_block(&db, a).unwrap().samples), audio::pcm_bytes(&pcm));
    assert_eq!(audio::read_block(&db, zero).unwrap().samples, vec![0.; 16000]);
    assert_eq!(pack_blobs(t.path(), store::HOT, "a/"), 1);
    assert_only_store(t.path());
}
#[test]
fn tier_boundary_and_cold_decode_preserve_timing() {
    let t = tempfile::tempdir().unwrap();
    let (db, seg) = segment(t.path());
    let pcm = speech();
    let id = block(&db, seg, "mic", 0, &pcm);
    assert_eq!(compact::compact_audio(t.path(), DAY_NS + 2_999_999_999, 32000, 1).unwrap().audio_groups, 0);
    assert_eq!(compact::compact_audio(t.path(), DAY_NS + 3_000_000_000, 32000, 1).unwrap().audio_groups, 1);
    assert_eq!(audio::read_block(&db, id).unwrap().samples, pcm);
    assert_eq!(db.query_row("SELECT codec FROM blocks", [], |r| r.get::<_, String>(0)).unwrap(), "cold-ref");
    assert_eq!(count(&db, "SELECT count(*) FROM audio_objects WHERE pack='hot'"), 0);
    assert_eq!(pack_blobs(t.path(), store::HOT, "a/"), 0, "the hot blob is collected in the same pass");
    assert_eq!(count(&db, "SELECT count(*) FROM gc_blobs"), 0);
    assert_eq!(all_blobs(t.path(), "a/"), 1);
}
#[test]
fn cold_cross_source_correlation_prefers_digital_and_keeps_gain() {
    let t = tempfile::tempdir().unwrap();
    let (db, seg) = segment(t.path());
    let pcm = speech();
    let low: Vec<_> = pcm.iter().map(|x| x / 2.).collect();
    let a = block(&db, seg, "mic", 0, &low);
    let b = block(&db, seg, "process", 0, &pcm);
    compact::compact_audio(t.path(), DAY_NS + 10_000_000_000, 32000, 10).unwrap();
    assert_eq!(count(&db, "SELECT count(*) FROM audio_objects"), 1);
    let ra = audio::read_block(&db, a).unwrap().samples;
    let rb = audio::read_block(&db, b).unwrap().samples;
    assert!(ra.iter().zip(&low).all(|(a, b)| (a - b).abs() <= 1. / 32768.));
    assert_eq!(rb, pcm);
    assert_eq!(all_blobs(t.path(), "a/"), 1, "one stored signal");
}
#[test]
fn unrelated_audio_and_time_offsets_are_not_merged() {
    let t = tempfile::tempdir().unwrap();
    let (db, seg) = segment(t.path());
    let pcm = speech();
    let other: Vec<_> = (0..32000).map(|i| ((i as f32 * 0.031).sin() * 8000.).round() / 32768.).collect();
    block(&db, seg, "mic", 0, &pcm);
    block(&db, seg, "process", 0, &other);
    compact::compact_audio(t.path(), DAY_NS + 10_000_000_000, 32000, 10).unwrap();
    assert_eq!(count(&db, "SELECT count(*) FROM audio_objects"), 2);
}
#[test]
fn real_sigkill_before_and_after_reference_commit_is_recoverable() {
    for stage in ["verified", "committed"] {
        let t = tempfile::tempdir().unwrap();
        let (db, seg) = segment(t.path());
        let pcm = speech();
        let id = block(&db, seg, "mic", 0, &pcm);
        drop(db);
        let status = std::process::Command::new(env!("CARGO_BIN_EXE_heimdall-archive"))
            .args(["compact-file", t.path().to_str().unwrap(), &(DAY_NS + 10_000_000_000).to_string(), "--crash-after", stage])
            .status()
            .unwrap();
        assert!(!status.success());
        let db = store::open(t.path()).unwrap();
        assert_eq!(db.query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0)).unwrap(), "ok");
        assert_eq!(audio::read_block(&db, id).unwrap().samples, pcm);
        compact::compact_audio(t.path(), DAY_NS + 10_000_000_000, 32000, 10).unwrap();
        assert_eq!(audio::read_block(&db, id).unwrap().samples, pcm);
        assert_eq!(db.query_row("SELECT codec FROM blocks", [], |r| r.get::<_, String>(0)).unwrap(), "cold-ref");
        // A crash before the reference commit can leave only an unreferenced blob; the sweep
        // removes it after the grace period, never a referenced one.
        compact::compact_once(t.path(), store::ORPHAN_GRACE_NS * 3 + heimdall_archive::wall_ns()).unwrap();
        assert_eq!(all_blobs(t.path(), "a/"), 1);
        assert_eq!(audio::read_block(&db, id).unwrap().samples, pcm);
    }
}
#[test]
fn opus_decode_alignment_and_corruption_are_checked() {
    let pcm = speech();
    for rate in [16000, 24000, 32000] {
        let bytes = audio::encode_opus(&pcm, rate).unwrap();
        let read = audio::decode_opus(&bytes).unwrap();
        assert_eq!(read.len(), pcm.len());
        assert!(read.iter().all(|x| x.is_finite()));
        assert!(audio::decode_opus(&bytes[..bytes.len() - 3]).is_err());
    }
}
#[allow(clippy::too_many_arguments)]
fn utterance(db: &Connection, session: i64, id: &str, source: &str, speaker: &str, start: i64, end: i64, text: &str, words: &str, decision: &str) {
    db.execute(
        "INSERT INTO utterances(session,id,wall_ms,source_id,source,speaker,start_ns,end_ns,text,words,notes,decision) VALUES(?1,?2,1,?3,?3,?4,?5,?6,?7,?8,'[]',?9)",
        params![session, id, source, speaker, start, end, text, words, decision],
    )
    .unwrap();
}
fn png(w: u32, h: u32, f: impl Fn(u32, u32) -> [u8; 4]) -> (image::RgbaImage, Vec<u8>) {
    let img = image::RgbaImage::from_fn(w, h, |x, y| image::Rgba(f(x, y)));
    let mut out = std::io::Cursor::new(vec![]);
    image::DynamicImage::ImageRgba8(img.clone()).write_to(&mut out, image::ImageFormat::Png).unwrap();
    (img, out.into_inner())
}
#[test]
fn dictionary_records_images_and_export() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("archive");
    let (db, seg) = segment(&root);
    block(&db, seg, "mic", 0, &speech());
    let journal = Journal::open(&session(&root)).unwrap();
    utterance(&db, journal.session_id(), "u1", "mic", "User", 0, 2_000_000_000, "hello", "[{\"text\":\"hello\",\"start_ns\":0,\"end_ns\":1000000000}]", "{\"send\":true}");
    let record = Record {
        kind: "decision".into(),
        source: "mic".into(),
        utterance_ids: vec!["u1".into()],
        host_ns: 0,
        wall_ns: 1,
        payload: json!({"probability":0.91,"audio":{"utterance_id":"u1"},"word":"hello"}),
    };
    journal.append(&record).unwrap();
    journal.append(&record).unwrap();
    assert_eq!(count(&db, "SELECT count(*) FROM payloads"), 1);
    let (img, png) = png(128, 128, |x, y| [(x / 8) as u8, (y / 8) as u8, 20, 255]);
    let id = journal.image(&png, "image/png").unwrap();
    assert_eq!(journal.image(&png, "image/png").unwrap(), id);
    db.execute("UPDATE images SET wall_ns=1", []).unwrap();
    compact::compact_records(&root, DAY_NS + 10_000_000_000).unwrap();
    assert_eq!(journal.records().unwrap()[0].payload, record.payload);
    let (_, bytes) = journal.image_bytes(&id).unwrap();
    assert_eq!(image::load_from_memory(&bytes).unwrap().to_rgba8(), img);
    let other = Journal::open(&store::session_path(&root, "session-second")).unwrap();
    assert_eq!(other.image(&png, "image/png").unwrap(), id);
    drop(other);
    let output = t.path().join("dataset");
    assert_eq!(export::export(&root, &output).unwrap(), 1);
    let manifest: serde_json::Value = serde_json::from_str(std::fs::read_to_string(output.join("manifest.jsonl")).unwrap().trim()).unwrap();
    assert_eq!(manifest["utterance_id"], "u1");
    assert_eq!(manifest["speaker"], "User");
    assert!(!manifest["audio_missing"].as_bool().unwrap());
    assert_eq!(manifest["audio_segments"].as_array().unwrap().len(), 1);
    let wav = output.join(manifest["audio_segments"][0]["audio"].as_str().unwrap());
    assert_eq!(hound::WavReader::open(wav).unwrap().duration(), 32000);
    assert_eq!(std::fs::read_dir(output.join("images")).unwrap().count(), 1);
    assert_eq!(std::fs::read_to_string(output.join("images.jsonl")).unwrap().lines().count(), 1);
    assert!(export::export(&root, &output).is_err());
    assert_only_store(&root);
}
#[test]
fn asynchronous_writer_flushes_and_preserves_all_outputs() {
    let t = tempfile::tempdir().unwrap();
    let writer = heimdall_archive::records::Writer::open(&session(t.path())).unwrap();
    for i in 0..128 {
        writer.event("vad", "mic", &[], i, json!({"probability":i as f64/128.})).unwrap();
    }
    writer.flush().unwrap();
    assert_eq!(Journal::open(&session(t.path())).unwrap().records().unwrap().len(), 128);
}
#[test]
fn diagnostic_saturation_is_nonblocking_and_drops_are_durable_after_unlock() {
    let t = tempfile::tempdir().unwrap();
    let path = session(t.path());
    // A small bound (the app's writers keep `WRITER_BACKLOG`), so 6,000 records saturate it.
    let writer = heimdall_archive::records::Writer::open_bounded(&path, 4096).unwrap();
    let journal = Journal::open(&path).unwrap();
    let db = store::open(journal.root()).unwrap();
    db.execute_batch("BEGIN IMMEDIATE").unwrap();
    let start = std::time::Instant::now();
    for i in 0..6000 {
        writer.diagnostic("health", "fixture", i, json!({"state": "locked", "sequence": i}));
    }
    let elapsed = start.elapsed();
    db.execute_batch("ROLLBACK").unwrap();
    assert!(elapsed < std::time::Duration::from_millis(500), "diagnostic enqueue blocked: {elapsed:?}");
    writer.flush().unwrap();
    let records = journal.records().unwrap();
    let accepted = records.iter().filter(|r| r.kind == "health").count() as u64;
    let dropped: u64 = records.iter().filter(|r| r.kind == "diagnostic_drops").map(|r| r.payload["count"].as_u64().unwrap()).sum();
    assert!(dropped > 0, "must exercise saturation of the bounded queue");
    assert_eq!(accepted + dropped, 6000);
    eprintln!("diagnostic saturation while locked: {elapsed:?}; accepted={accepted}, durably counted drops={dropped}");
    writer.diagnostic("health", "fixture", 6001, json!({"state": "recovered"}));
    writer.flush().unwrap();
    let after = journal.records().unwrap();
    assert!(after.iter().any(|r| r.payload["state"] == "recovered"));
    assert_eq!(after.iter().filter(|r| r.kind == "diagnostic_drops").map(|r| r.payload["count"].as_u64().unwrap()).sum::<u64>(), dropped, "drop counts are not published twice");
}

#[test]
fn cold_export_keeps_utterance_offsets_into_shared_objects() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("archive");
    let (db, seg) = segment(&root);
    block(&db, seg, "mic", 0, &speech());
    let sid = store::ensure_session(&db, SESSION).unwrap();
    utterance(&db, sid, "u1", "mic", "User", 500_000_000, 1_500_000_000, "hello", "[]", "{}");
    store::mark_complete(&session(&root)).unwrap();
    compact::compact_once(&root, DAY_NS + 10_000_000_000).unwrap();
    assert_eq!(count(&db, "SELECT count(*) FROM cold_chunks WHERE kind='blocks'"), 1, "the cold row is packed");
    let out = t.path().join("training");
    assert_eq!(export::export(&root, &out).unwrap(), 1);
    let row: serde_json::Value = serde_json::from_str(std::fs::read_to_string(out.join("manifest.jsonl")).unwrap().trim()).unwrap();
    assert_eq!(row["audio_segments"][0]["offset_samples"], 8000);
    assert_eq!(row["audio_segments"][0]["samples"], 16000);
    assert_eq!(row["audio_segments"][0]["tier"], "cold");
}
#[test]
fn cold_evidence_vectors_are_exact_and_new_image_reuse_restores_hot_bytes() {
    let t = tempfile::tempdir().unwrap();
    let journal = Journal::open(&session(t.path())).unwrap();
    let values: Vec<_> = (0..512).map(|i| (i as f32 / 17.).sin()).collect();
    let id = journal.vector(&values).unwrap();
    assert_eq!(journal.vector(&values).unwrap(), id);
    let mut payload = json!({"current_embedding":values,"pristine_embedding":values,"history_embeddings":values,"probability":0.93});
    journal.reference_vectors(&mut payload).unwrap();
    assert!(payload.get("current_embedding").is_none());
    assert_eq!(payload["current_embedding_id"], payload["pristine_embedding_id"]);
    let image = image::RgbaImage::from_pixel(512, 512, image::Rgba([30, 60, 90, 255]));
    let mut png = std::io::Cursor::new(vec![]);
    image::DynamicImage::ImageRgba8(image.clone()).write_to(&mut png, image::ImageFormat::Png).unwrap();
    let png = png.into_inner();
    let image_id = journal.image(&png, "image/png").unwrap();
    let db = store::open(t.path()).unwrap();
    db.execute("UPDATE images SET wall_ns=1", []).unwrap();
    compact::compact_records(t.path(), DAY_NS + 10_000_000_000).unwrap();
    assert_eq!(journal.image_bytes(&image_id).unwrap().0, "image/webp");
    assert_eq!(pack_blobs(t.path(), store::HOT, "i/"), 0, "the hot original is collected");
    assert_eq!(journal.image(&png, "image/png").unwrap(), image_id);
    assert_eq!(journal.image_bytes(&image_id).unwrap().1, png);
    db.execute("UPDATE images SET wall_ns=1", []).unwrap();
    journal
        .append(&Record { kind: "tcl".into(), source: "mic".into(), utterance_ids: vec!["u1".into()], host_ns: 1, wall_ns: 1, payload: payload.clone() })
        .unwrap();
    drop(journal);
    compact::compact_once(t.path(), DAY_NS + 10_000_000_000).unwrap();
    assert_eq!(count(&db, "SELECT count(*) FROM evidence"), 0, "the record is packed");
    let read = Journal::open_read_only(&session(t.path())).unwrap();
    assert_eq!(read.records().unwrap()[0].payload, payload);
    assert_eq!(audio::pcm_bytes(&read.vector_values(&id).unwrap()), audio::pcm_bytes(&values));
    assert_eq!(image::load_from_memory(&read.image_bytes(&image_id).unwrap().1).unwrap().to_rgba8(), image);
}
#[test]
fn native_48k_fractional_callback_boundaries_export_every_cold_sample() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("archive");
    let (db, seg) = segment(&root);
    let pcm: Vec<_> = (0..48000).map(|i| (i as f32 * 0.053).sin() * 0.2).collect();
    let mut ids = vec![];
    for (n, samples) in pcm.chunks(512).enumerate() {
        ids.push(block_at(&db, seg, "mic", n as u64 * 512 * 1_000_000_000 / 48000, 48000, samples));
    }
    compact::compact_audio(&root, DAY_NS + 10_000_000_000, 32000, 10).unwrap();
    let mut decoded = vec![];
    for id in &ids {
        decoded.extend(audio::read_block(&db, *id).unwrap().samples);
    }
    assert_eq!(decoded.len(), 16000);
    let expected = audio::mono_16k(&pcm, 48000, 1).unwrap();
    assert!(decoded.iter().zip(&expected).all(|(a, b)| (a - b).abs() <= 1. / 32768.));
    drop(db);
    let out = t.path().join("training");
    export::export(&root, &out).unwrap();
    assert_eq!(std::fs::read_dir(out.join("audio")).unwrap().count(), 1);
    let timeline: Vec<serde_json::Value> = std::fs::read_to_string(out.join("audio.jsonl")).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(timeline.iter().map(|r| r["data"]["samples"].as_u64().unwrap()).sum::<u64>(), 16000);
}
#[test]
fn exceptional_native_floats_remain_bit_exact_and_do_not_block_other_sources() {
    let t = tempfile::tempdir().unwrap();
    let (db, seg) = segment(t.path());
    let pcm = [f32::from_bits(0x7fc01234), f32::INFINITY, -0.0, 0.01];
    let a = block(&db, seg, "mic", 0, &pcm);
    let b = block(&db, seg, "process", 0, &speech());
    assert_eq!(compact::compact_audio(t.path(), DAY_NS + 10_000_000_000, 32000, 10).unwrap().audio_groups, 2);
    assert_eq!(audio::pcm_bytes(&audio::read_block(&db, a).unwrap().samples), audio::pcm_bytes(&pcm));
    assert_eq!(audio::read_block(&db, b).unwrap().samples, speech());
}
#[test]
fn training_export_marks_partial_missing_audio_without_filling_the_gap() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("archive");
    let (db, seg) = segment(&root);
    block(&db, seg, "mic", 0, &speech()[..16000]);
    block(&db, seg, "mic", 2_000_000_000, &speech()[..16000]);
    let sid = store::ensure_session(&db, SESSION).unwrap();
    utterance(&db, sid, "u1", "mic", "unknown", 0, 3_000_000_000, "hello there", "[]", "{}");
    drop(db);
    let out = t.path().join("dataset");
    export::export(&root, &out).unwrap();
    let row: serde_json::Value = serde_json::from_str(std::fs::read_to_string(out.join("manifest.jsonl")).unwrap().trim()).unwrap();
    assert_eq!(row["audio_gaps"], json!([{"start_ns":1_000_000_000u64,"end_ns":2_000_000_000u64}]));
    assert_eq!(row["audio_complete"], false);
    assert_eq!(row["speaker"], "unknown");
    assert_eq!(row["audio_segments"].as_array().unwrap().len(), 2);
}
#[test]
fn a_bad_segment_does_not_stall_unrelated_eligible_audio() {
    let t = tempfile::tempdir().unwrap();
    let (db, bad) = segment(t.path());
    let sid = store::ensure_session(&db, SESSION).unwrap();
    let good = store::create_segment(&db, sid, "capture-good", 1_000_000_000, 0, "native").unwrap();
    let broken = block(&db, bad, "mic", 0, &speech());
    let fine = block(&db, good, "mic", 0, &speech().iter().map(|x| -x).collect::<Vec<_>>());
    // Corrupt the bad segment's hot blob: its checksum no longer matches.
    let object: String = db.query_row("SELECT object_id FROM blocks WHERE id=?1", [broken], |r| r.get(0)).unwrap();
    store::with_pack(t.path(), store::HOT, |p| Ok(p.execute("UPDATE blobs SET data=X'00' WHERE key=?1", [store::audio_key(&object)])?)).unwrap();
    let result = compact::compact_once(t.path(), DAY_NS + 10_000_000_000).unwrap();
    assert_eq!(result.errors.len(), 1, "{:?}", result.errors);
    assert_eq!(result.audio_groups, 1);
    assert_eq!(audio::read_block(&db, fine).unwrap().samples, speech().iter().map(|x| -x).collect::<Vec<_>>());
    assert_eq!(db.query_row("SELECT codec FROM blocks WHERE id=?1", [broken], |r| r.get::<_, String>(0)).unwrap(), "f32-zstd", "the original stays");
}
#[test]
fn live_cold_chunks_restore_all_events_reuse_payload_and_keep_ids_monotonic() {
    let t = tempfile::tempdir().unwrap();
    let journal = Journal::open(&session(t.path())).unwrap();
    let make = |i: u64| Record {
        kind: "model_output".into(),
        source: "mic".into(),
        utterance_ids: vec![],
        host_ns: i * 32_000_000,
        wall_ns: 1,
        payload: json!({"model":"silero","input":{"source":"mic","start_ns":i*32_000_000,"end_ns":(i+1)*32_000_000},"probability":0.01}),
    };
    for i in 0..128 {
        journal.append(&make(i)).unwrap();
    }
    let original = journal.records().unwrap();
    assert_eq!(compact::compact_metadata(t.path(), DAY_NS + 100, false).unwrap(), 1);
    assert_eq!(journal.records().unwrap().len(), 128);
    let db = store::open(t.path()).unwrap();
    assert_eq!(count(&db, "SELECT count(*) FROM payloads"), 0);
    journal.append(&make(0)).unwrap(); // identical old input: no second payload body
    journal.append(&make(129)).unwrap();
    let restored = journal.records().unwrap();
    assert_eq!(restored.len(), 130);
    for (a, b) in restored.iter().zip(original) {
        assert_eq!(serde_json::to_value(a).unwrap(), serde_json::to_value(b).unwrap());
    }
    assert_eq!(restored[128].payload, restored[0].payload);
    assert_eq!(count(&db, "SELECT count(*) FROM payloads WHERE length(data)>0"), 1);
    assert!(count(&db, "SELECT min(id) FROM evidence") > 128);
    compact::compact_metadata(t.path(), DAY_NS + 100, false).unwrap();
    assert_eq!(journal.records().unwrap().len(), 130);
}
#[test]
fn a_payload_packed_by_one_session_reads_back_in_another() {
    let t = tempfile::tempdir().unwrap();
    let a = Journal::open(&store::session_path(t.path(), "session-a")).unwrap();
    let b = Journal::open(&store::session_path(t.path(), "session-b")).unwrap();
    let shared = json!({"source_info":{"id":"mic:built-in","rate":48000}});
    a.append(&Record { kind: "source".into(), source: "mic".into(), utterance_ids: vec![], host_ns: 1, wall_ns: 1, payload: shared.clone() }).unwrap();
    compact::compact_metadata(t.path(), DAY_NS + 100, false).unwrap();
    // The body now lives only in session A's chunk; B repeats it.
    b.event("source", "mic", &[], 2, shared.clone()).unwrap();
    b.event("only_b", "mic", &[], 3, json!({"x":1})).unwrap();
    let rb = b.records().unwrap();
    assert_eq!(rb.len(), 2, "views are per session");
    assert_eq!(rb[0].payload, shared);
    assert_eq!(a.records().unwrap().len(), 1);
    let db = store::open(t.path()).unwrap();
    assert_eq!(count(&db, "SELECT count(*) FROM payloads WHERE length(data)>0"), 1, "only B's own new body is stored");
}
#[test]
fn a_vad_body_held_live_by_one_session_survives_being_packed_after_another_session_packed_a_reference() {
    let t = tempfile::tempdir().unwrap();
    let a = Journal::open(&store::session_path(t.path(), "session-a")).unwrap();
    let b = Journal::open(&store::session_path(t.path(), "session-b")).unwrap();
    let vad = json!({"model":"silero","input":{"source":"mic","start_ns":0,"end_ns":32_000_000},"probability":0.01});
    let record = |wall| Record { kind: "model_output".into(), source: "mic".into(), utterance_ids: vec![], host_ns: 0, wall_ns: wall, payload: vad.clone() };
    a.append(&record(DAY_NS * 10)).unwrap(); // hot in A: the body stays live
    b.append(&record(1)).unwrap(); // old in B: packed, referencing A's live body
    compact::compact_metadata(t.path(), DAY_NS + 100, false).unwrap();
    let db = store::open(t.path()).unwrap();
    db.execute("UPDATE evidence SET wall_ns=1", []).unwrap(); // A's row ages too
    compact::compact_metadata(t.path(), DAY_NS + 100, false).unwrap();
    assert_eq!(count(&db, "SELECT count(*) FROM evidence"), 0);
    assert_eq!(b.records().unwrap()[0].payload, vad);
    assert_eq!(a.records().unwrap()[0].payload, vad);
}
#[test]
fn non_speech_opus_keeps_missed_audio_and_missing_vad_defaults_to_flac() {
    let t = tempfile::tempdir().unwrap();
    let (db, seg) = segment(t.path());
    let signal = speech();
    let a = block(&db, seg, "mic", 1_000_000_000, &signal);
    let b = block(&db, seg, "process", 1_000_000_000, &signal);
    let journal = Journal::open(&session(t.path())).unwrap();
    for i in 0..128u64 {
        journal.append(&Record{kind:"model_output".into(),source:"mic".into(),utterance_ids:vec![],host_ns:i*32_000_000,wall_ns:1,payload:json!({"model":"silero","input":{"source":"mic","start_ns":i*32_000_000,"end_ns":(i+1)*32_000_000},"probability":0.01})}).unwrap();
    }
    compact::compact_audio(t.path(), DAY_NS + 10_000_000_000, 32000, 10).unwrap();
    let read = audio::read_block(&db, a).unwrap();
    assert_eq!(read.samples.len(), signal.len());
    assert!(read.samples.iter().any(|x| x.abs() > 0.01));
    let codec = |id: i64| db.query_row("SELECT o.codec FROM blocks b JOIN audio_objects o ON o.id=b.object_id WHERE b.id=?1", [id], |r| r.get::<_, String>(0)).unwrap();
    assert_eq!(codec(a), "opus-har1");
    assert_eq!(codec(b), "flac16");
}
#[test]
fn real_sigkill_metadata_publication_preserves_audio_and_outputs() {
    use std::os::unix::process::ExitStatusExt;
    for stage in ["chunk_verified", "chunk_committed"] {
        for is_audio in [true, false] {
            let t = tempfile::tempdir().unwrap();
            let (db, seg) = segment(t.path());
            let id = block(&db, seg, "mic", 0, &speech());
            drop(db);
            compact::compact_audio(t.path(), DAY_NS + 10_000_000_000, 32000, 10).unwrap();
            let journal = Journal::open(&session(t.path())).unwrap();
            journal.append(&Record { kind: "test".into(), source: "mic".into(), utterance_ids: vec![], host_ns: 0, wall_ns: 1, payload: json!({"text":"retained"}) }).unwrap();
            drop(journal);
            let status = std::process::Command::new(env!("CARGO_BIN_EXE_heimdall-archive"))
                .args(["pack-file", t.path().to_str().unwrap(), &(DAY_NS + 10_000_000_000).to_string(), if is_audio { "audio" } else { "records" }, "--crash-after", stage])
                .status()
                .unwrap();
            assert_eq!(status.signal(), Some(libc::SIGKILL));
            compact::compact_metadata(t.path(), DAY_NS + 10_000_000_000, is_audio).unwrap();
            let view = View::session(&session(t.path())).unwrap();
            view.capture().unwrap();
            assert_eq!(audio::read_block(&view.db, id).unwrap().samples, speech());
            drop(view);
            assert_eq!(Journal::open_read_only(&session(t.path())).unwrap().records().unwrap()[0].payload, json!({"text":"retained"}));
            // An interrupted publication leaves at most one unreferenced chunk blob; the sweep takes it.
            compact::compact_once(t.path(), store::ORPHAN_GRACE_NS * 3 + heimdall_archive::wall_ns()).unwrap();
            let db = store::open(t.path()).unwrap();
            assert_eq!(all_blobs(t.path(), "c/"), count(&db, "SELECT count(*) FROM cold_chunks"));
        }
    }
}
#[test]
fn orphan_and_queued_blobs_are_collected_only_when_unreferenced() {
    let t = tempfile::tempdir().unwrap();
    let (db, seg) = segment(t.path());
    let signal = speech();
    let id = block(&db, seg, "mic", 0, &signal);
    compact::compact_audio(t.path(), DAY_NS + 10_000_000_000, 32000, 1).unwrap();
    let (object, pack): (String, String) = db.query_row("SELECT o.id,o.pack FROM blocks b JOIN audio_objects o ON o.id=b.object_id WHERE b.id=?1", [id], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
    // A queued but still referenced blob stays.
    store::queue_gc(&db, &pack, &store::audio_key(&object)).unwrap();
    // An owned orphan (crash leftover) and an unowned key.
    let orphan = store::audio_key(&"f".repeat(64));
    store::put(t.path(), &pack, &[(orphan.clone(), b"orphan".as_slice()), ("x/notes".into(), b"unowned".as_slice())]).unwrap();
    compact::compact_once(t.path(), heimdall_archive::wall_ns()).unwrap();
    assert!(store::has(t.path(), &pack, &orphan).unwrap(), "young orphans wait for the grace period");
    db.execute("DELETE FROM store_meta", []).unwrap();
    compact::compact_once(t.path(), heimdall_archive::wall_ns() + 3 * store::ORPHAN_GRACE_NS).unwrap();
    assert!(!store::has(t.path(), &pack, &orphan).unwrap());
    assert!(store::has(t.path(), &pack, "x/notes").unwrap());
    assert!(store::has(t.path(), &pack, &store::audio_key(&object)).unwrap());
    assert_eq!(audio::read_block(&db, id).unwrap().samples.len(), signal.len());
}
#[test]
fn planar_hot_round_trip_keeps_every_float_bit_in_multichannel_geometry() {
    let t = tempfile::tempdir().unwrap();
    let db = store::open(t.path()).unwrap();
    let mut bits = 1u32;
    let mut samples = vec![0., -0., f32::INFINITY, f32::from_bits(0x7fc01234)];
    for _ in 0..4092 {
        bits = bits.wrapping_mul(1664525).wrapping_add(1013904223);
        samples.push(f32::from_bits(bits));
    }
    let id = audio::put_hot(&db, 48000, 4, &samples).unwrap();
    let restored = audio::read_object(&db, &id).unwrap();
    assert_eq!(restored.channels, 4);
    assert_eq!(audio::pcm_bytes(&restored.samples), audio::pcm_bytes(&samples));
}
#[test]
fn index_and_packs_reclaim_pages_incrementally() {
    let t = tempfile::tempdir().unwrap();
    let (db, seg) = segment(t.path());
    for i in 0..8u64 {
        block(&db, seg, "mic", i * 40_000_000_000, &speech());
    }
    assert_eq!(db.query_row("PRAGMA auto_vacuum", [], |r| r.get::<_, i64>(0)).unwrap(), 2);
    let hot = store::pack_path(t.path(), store::HOT).unwrap();
    store::with_pack(t.path(), store::HOT, |p| Ok(p.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?)).unwrap();
    let before = std::fs::metadata(&hot).unwrap().len();
    compact::compact_once(t.path(), DAY_NS * 2).unwrap();
    store::with_pack(t.path(), store::HOT, |p| {
        assert_eq!(p.query_row("PRAGMA auto_vacuum", [], |r| r.get::<_, i64>(0))?, 2);
        p.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
        Ok(())
    })
    .unwrap();
    assert!(std::fs::metadata(&hot).unwrap().len() < before, "hot pack pages were returned");
}
#[test]
fn mixed_speech_non_speech_chunks_cover_every_original_frame() {
    let t = tempfile::tempdir().unwrap();
    let (db, seg) = segment(t.path());
    let pcm = speech()[..16000].to_vec();
    let mut ids = vec![];
    for i in 0..12u64 {
        ids.push(block(&db, seg, "mic", i * 1_000_000_000, &pcm));
    }
    let journal = Journal::open(&session(t.path())).unwrap();
    for i in 0..375u64 {
        journal.append(&Record{kind:"model_output".into(),source:"mic".into(),utterance_ids:vec![],host_ns:i*32_000_000,wall_ns:1,payload:json!({"model":"silero","input":{"source":"mic","start_ns":i*32_000_000,"end_ns":(i+1)*32_000_000},"probability":if i<187 {0.01}else{0.99}})}).unwrap();
    }
    compact::compact_audio(t.path(), DAY_NS + 30_000_000_000, 32000, 32).unwrap();
    assert_eq!(count(&db, "SELECT count(DISTINCT o.codec) FROM blocks b JOIN audio_objects o ON o.id=b.object_id"), 2);
    let mut n = 0;
    for id in &ids {
        let block = audio::read_block(&db, *id).unwrap();
        n += block.samples.len();
        assert!(block.samples.iter().any(|x| x.abs() > 0.01));
    }
    assert_eq!(n, 192000);
    compact::compact_metadata(t.path(), DAY_NS + 30_000_000_000, true).unwrap();
    let view = View::session(&session(t.path())).unwrap();
    view.capture().unwrap();
    assert_eq!(view.db.query_row("SELECT sum(frames) FROM blocks", [], |r| r.get::<_, i64>(0)).unwrap(), 192000);
    assert_eq!(count(&db, "SELECT count(*) FROM blocks"), 0, "every row lives in its chunk");
}
#[test]
fn model_tensor_references_roundtrip_across_chunk_boundaries_and_deduplicate() {
    let t = tempfile::tempdir().unwrap();
    let journal = Journal::open(&session(t.path())).unwrap();
    let samples: Vec<f32> = (0..70000).map(|i| i as f32 / 70000.).collect();
    let mut payload = json!({"output":{"shape":[70000],"tensor_values":samples}});
    journal.reference_tensors(&mut payload).unwrap();
    assert!(payload["output"].get("tensor_values").is_none());
    let ids = payload["output"]["vector_ids"].as_array().unwrap();
    assert_eq!(ids.len(), 3);
    let restored: Vec<_> = ids.iter().flat_map(|id| journal.vector_values(id.as_str().unwrap()).unwrap()).collect();
    assert_eq!(restored, samples);
    let mut repeated = json!({"tensor_values":samples});
    journal.reference_tensors(&mut repeated).unwrap();
    assert_eq!(repeated["vector_ids"], payload["output"]["vector_ids"]);
}
#[test]
fn wall_window_reads_only_records_inside_it_in_append_order() {
    let t = tempfile::tempdir().unwrap();
    let journal = Journal::open(&session(t.path())).unwrap();
    let other = Journal::open(&store::session_path(t.path(), "session-other")).unwrap();
    for (i, wall) in [100u64, 200, 300, 400].into_iter().enumerate() {
        let r = Record {
            kind: if i == 2 { heimdall_archive::BUG_NOTE_KIND.into() } else { "d1_tcl".into() },
            source: "mic".into(),
            utterance_ids: vec![format!("s:{i}")],
            host_ns: i as u64,
            wall_ns: wall,
            payload: json!({"i": i}),
        };
        journal.append(&r).unwrap();
        other.append(&Record { payload: json!({"other": i}), ..r }).unwrap();
    }
    let inside = journal.records_between_wall(150, 300).unwrap();
    assert_eq!(inside.iter().map(|r| r.payload["i"].as_u64().unwrap()).collect::<Vec<_>>(), [1, 2]);
    assert_eq!(inside[1].kind, "bug_note");
    assert!(journal.records_between_wall(401, 500).unwrap().is_empty());
    assert_eq!(journal.records().unwrap().len(), 4);
}
fn spans(root: &Path) -> i64 {
    count(&store::open(root).unwrap(), "SELECT count(*) FROM audio_spans")
}
#[test]
fn a_shared_hot_object_is_decoded_once_and_dropped_in_the_publish_transaction() {
    let t = tempfile::tempdir().unwrap();
    let (db, seg) = segment(t.path());
    // One four-second hot object referenced by two callback blocks, as capture batches them.
    let mut pcm = speech();
    pcm.extend(speech().iter().map(|x| -x));
    let id = audio::put_hot(&db, 16000, 1, &pcm).unwrap();
    let mut rows = vec![];
    for (n, offset) in [(0i64, 0i64), (2_000_000_000, 32000)] {
        db.execute("INSERT INTO blocks(segment,source_id,host_ns,rate,channels,frames,codec,discontinuity,object_id,offset_frames,wall_ns) VALUES(?1,'mic',?2,16000,1,32000,'f32-zstd',0,?3,?4,?5)", params![seg, n, id, offset, 1_000_000_000 + n]).unwrap();
        rows.push(db.last_insert_rowid());
    }
    let r = compact::compact_audio(t.path(), DAY_NS + 10_000_000_000, 32000, 10).unwrap();
    assert_eq!((r.audio_groups, r.reclaimed_objects), (1, 2), "one catalogue row and its hot blob");
    assert_eq!(count(&db, "SELECT count(*) FROM audio_objects WHERE pack='hot'"), 0);
    assert_eq!(count(&db, "SELECT count(*) FROM gc_blobs"), 0);
    assert_eq!(count(&db, "SELECT count(*) FROM blocks WHERE codec='cold-ref'"), 2);
    assert_eq!(audio::read_block(&db, rows[0]).unwrap().samples, speech());
    assert_eq!(audio::read_block(&db, rows[1]).unwrap().samples, speech().iter().map(|x| -x).collect::<Vec<_>>());
}
#[test]
fn compactor_waits_while_gated_and_stops_between_groups_when_the_gate_closes() {
    use std::sync::{Arc, atomic::{AtomicBool, AtomicUsize, Ordering}};
    use std::time::{Duration, Instant};
    let t = tempfile::tempdir().unwrap();
    let (db, seg) = segment(t.path());
    let mut ids = vec![];
    for i in 0..4u64 {
        ids.push(block(&db, seg, "mic", i * 40_000_000_000, &speech()));
    }
    drop(db);
    let calls = Arc::new(AtomicUsize::new(0));
    let open = Arc::new(AtomicBool::new(false));
    let (c, o) = (calls.clone(), open.clone());
    // Closed from the start (battery): the compactor keeps asking but touches nothing.
    let compactor = compact::Compactor::start_with(t.path().into(), move || { c.fetch_add(1, Ordering::SeqCst); o.load(Ordering::SeqCst) }, Duration::ZERO, Duration::from_millis(10)).unwrap();
    let begin = Instant::now();
    while calls.load(Ordering::SeqCst) < 5 {
        assert!(begin.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(spans(t.path()), 0);
    drop(compactor);
    // Open for the pass check and one group, then closed (unplugged): exactly that group is
    // published and nothing follows while it stays closed.
    let calls = Arc::new(AtomicUsize::new(0));
    let c = calls.clone();
    let o = open.clone();
    let compactor = compact::Compactor::start_with(t.path().into(), move || c.fetch_add(1, Ordering::SeqCst) < 3 || o.load(Ordering::SeqCst), Duration::ZERO, Duration::from_millis(10)).unwrap();
    let begin = Instant::now();
    while calls.load(Ordering::SeqCst) < 10 {
        assert!(begin.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(spans(t.path()), 1);
    // AC again: the rest catches up.
    open.store(true, Ordering::SeqCst);
    let begin = Instant::now();
    while spans(t.path()) < 4 {
        assert!(begin.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(10));
    }
    drop(compactor);
    // The compactor may also have packed the cold rows; the view reads packed and live alike.
    let view = View::session(&session(t.path())).unwrap();
    view.capture().unwrap();
    for id in ids {
        assert_eq!(audio::read_block(&view.db, id).unwrap().samples, speech());
    }
}
#[test]
fn a_reader_never_waits_for_a_compaction_pass() {
    let t = tempfile::tempdir().unwrap();
    let (db, seg) = segment(t.path());
    for g in 0..4u64 {
        block(&db, seg, "mic", g * 40_000_000_000, &speech());
    }
    drop(db);
    let now = DAY_NS + 400_000_000_000;
    // A reader arriving mid-pass takes the shared lock at once; the pass goes on and finishes every group.
    let mut waited = None;
    let first = compact::compact_audio_while(t.path(), now, &mut || {
        if waited.is_none() {
            let at = std::time::Instant::now();
            let _shared = compact::reader_lock(t.path()).unwrap();
            waited = Some(at.elapsed());
        }
        true
    })
    .unwrap();
    let waited = waited.unwrap();
    assert!(waited < std::time::Duration::from_millis(50), "a reader waited {waited:?} for compaction");
    assert_eq!(first.audio_groups, 4, "{:?}", first.errors);
}

// ---- Migration from the per-launch folder layout ----

/// The legacy fixture was produced by the pre-store code: a closed session with a sealed
/// (`.sqlite.zst`) capture database, a hot capture database, cold FLAC/Opus files, chunked
/// evidence, cold and kept screenshots, vectors, VAD activity, transcript (with a legacy events
/// row), rotated and hot dry-run logs, a dry-run spool and a decide log; an open session with a
/// schema-1 inline-PCM capture database; and a voice-bank store with one clip and lines.json.
fn legacy_copy(t: &tempfile::TempDir) -> PathBuf {
    let root = t.path().join("archive");
    copy_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/legacy-archive"), &root);
    root
}
fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        if e.file_type().unwrap().is_dir() {
            copy_dir(&e.path(), &to.join(e.file_name()));
        } else {
            let target = to.join(e.file_name());
            std::fs::copy(e.path(), &target).unwrap();
            // As the legacy app left it: last written well before the migration.
            let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
            std::fs::File::options().write(true).open(&target).unwrap().set_modified(old).unwrap();
        }
    }
}
const A: &str = "session-1000-aaaaaaaa";
const B: &str = "session-2000-bbbbbbbb";
#[test]
fn conflicting_voice_bank_lines_preserve_both_lists_and_retry() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("archive");
    let bank = root.join("voice-bank");
    std::fs::create_dir_all(&bank).unwrap();
    let source = br#"[{"id":"legacy","text":"Unique legacy line"}]"#;
    let destination = br#"[{"id":"current","text":"Unique current line"}]"#;
    std::fs::write(bank.join("lines.json"), source).unwrap();
    std::fs::write(bank.join("COMPLETE"), b"").unwrap();
    let target = t.path().join("voice-bank-lines.json");
    std::fs::write(&target, destination).unwrap();
    let reports = migrate::migrate_all(&root, &migrate::Options::default()).unwrap();
    assert!(!reports[0].verified && !reports[0].removed, "{reports:?}");
    assert!(reports[0].error.as_deref().unwrap().contains("lines"));
    assert_eq!(std::fs::read(bank.join("lines.json")).unwrap(), source);
    assert_eq!(std::fs::read(&target).unwrap(), destination);
    let db = store::open(&root).unwrap();
    assert_eq!(count(&db, "SELECT count(*) FROM migrations"), 0);
    // Once the conflict is resolved, verification and removal can proceed.
    std::fs::write(&target, source).unwrap();
    let reports = migrate::migrate_all(&root, &migrate::Options::default()).unwrap();
    assert!(reports[0].verified && reports[0].removed, "{reports:?}");
}
#[test]
#[ignore = "needs the private legacy-archive fixture that is not in the public repository"]
fn legacy_folders_migrate_verified_and_every_reader_sees_them() {
    let t = tempfile::tempdir().unwrap();
    let root = legacy_copy(&t);
    let reports = migrate::migrate_all(&root, &migrate::Options::default()).unwrap();
    assert_eq!(reports.len(), 3);
    assert!(reports.iter().all(|r| r.verified && r.removed), "{reports:?}");
    let a = &reports[0];
    assert_eq!(a.rows["evidence"], 133);
    assert_eq!(a.rows["blocks"], 19);
    assert_eq!(a.rows["segments"], 2);
    assert_eq!(a.rows["images"], 2);
    assert_eq!(a.rows["logs"], 5, "rotated logs, hot log, spool, decide log");
    assert_eq!(reports[1].rows["blocks"], 2, "schema-1 inline PCM");
    assert_only_store(&root);
    assert!(root.parent().unwrap().join("voice-bank-lines.json").is_file());
    // Readers: journal, capture view, transcript search rows, logs, voice-bank clip, export.
    let records = Journal::open_read_only(&store::session_path(&root, A)).unwrap().records().unwrap();
    assert_eq!(records.len(), 133);
    assert!(records.iter().any(|r| r.kind == "listen_event" && r.payload["text"] == "hello there"));
    let view = View::session(&store::session_path(&root, A)).unwrap();
    assert!(view.is_complete().unwrap());
    view.capture().unwrap();
    let ids: Vec<i64> = view.db.prepare("SELECT id FROM blocks ORDER BY id").unwrap().query_map([], |r| r.get(0)).unwrap().collect::<rusqlite::Result<_>>().unwrap();
    for id in ids {
        audio::read_block(&view.db, id).unwrap();
    }
    drop(view);
    let open = View::session(&store::session_path(&root, B)).unwrap();
    assert!(!open.is_complete().unwrap());
    open.capture().unwrap();
    let inline: Vec<f32> = audio::read_block(&open.db, open.db.query_row("SELECT min(id) FROM blocks", [], |r| r.get(0)).unwrap()).unwrap().samples;
    assert_eq!(inline.len(), 3200);
    assert!((inline[2] - (0.01f32).cos() * 0.3).abs() < 1e-6 && inline[3] == -inline[2]);
    drop(open);
    assert_eq!(store::read_log(&store::session_path(&root, A), "dry-run.jsonl").unwrap(), b"{\"line\":\"hot dry log after rotation\"}\n");
    assert_eq!(store::read_log(&store::session_path(&root, A), "decisions/decisions.jsonl").unwrap(), b"{\"decision\":\"probe\"}\n");
    let db = store::open(&root).unwrap();
    let clip: String = db.query_row("SELECT object_id FROM clips WHERE line_id='yes'", [], |r| r.get(0)).unwrap();
    assert_eq!(audio::read_object(&db, &clip).unwrap().samples.len(), 4410);
    assert_eq!(count(&db, "SELECT count(*) FROM migrations"), 3);
    let out = t.path().join("dataset");
    assert_eq!(export::export(&root, &out).unwrap(), 3);
    // Compaction carries on over migrated data.
    let r = compact::compact_once(&root, heimdall_archive::wall_ns() + 2 * DAY_NS).unwrap();
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    assert_eq!(Journal::open_read_only(&store::session_path(&root, A)).unwrap().records().unwrap().len(), 133);
}
#[test]
#[ignore = "needs the private legacy-archive fixture that is not in the public repository"]
fn a_failed_legacy_session_stays_untouched_and_retries_cleanly() {
    let t = tempfile::tempdir().unwrap();
    let root = legacy_copy(&t);
    // A damaged cold object file: verification must fail, the folder must stay.
    let objects = root.join(A).join("objects");
    let flac = std::fs::read_dir(&objects).unwrap().map(|e| e.unwrap().path()).find(|p| p.extension().is_some_and(|e| e == "flac")).unwrap();
    let original = std::fs::read(&flac).unwrap();
    std::fs::write(&flac, &original[..original.len() / 2]).unwrap();
    let reports = migrate::migrate_all(&root, &migrate::Options::default()).unwrap();
    let a = reports.iter().find(|r| r.name == A).unwrap();
    assert!(!a.verified && !a.removed);
    assert!(a.error.as_deref().unwrap().contains("checksum"), "{:?}", a.error);
    assert!(root.join(A).is_dir());
    assert!(!root.join(B).exists() && !root.join("voice-bank").exists(), "other folders still migrate");
    let db = store::open(&root).unwrap();
    assert_eq!(store::find_session(&db, A).unwrap(), None, "partial rows are removed");
    std::fs::write(&flac, &original).unwrap();
    let again = migrate::migrate_all(&root, &migrate::Options::default()).unwrap();
    assert!(again.len() == 1 && again[0].verified && again[0].removed, "{again:?}");
    assert_eq!(Journal::open_read_only(&store::session_path(&root, A)).unwrap().records().unwrap().len(), 133);
}
#[test]
#[ignore = "needs the private legacy-archive fixture that is not in the public repository"]
fn real_sigkill_during_migration_recovers_without_loss() {
    use std::os::unix::process::ExitStatusExt;
    for stage in ["migrate_copied", "migrate_verified"] {
        let t = tempfile::tempdir().unwrap();
        let root = legacy_copy(&t);
        let status = std::process::Command::new(env!("CARGO_BIN_EXE_heimdall-archive"))
            .args(["migrate", root.to_str().unwrap(), "--crash-after", stage])
            .status()
            .unwrap();
        assert_eq!(status.signal(), Some(libc::SIGKILL));
        assert!(root.join(A).is_dir(), "{stage}: the first folder is still there");
        let reports = migrate::migrate_all(&root, &migrate::Options::default()).unwrap();
        assert!(reports.iter().all(|r| r.verified && r.removed), "{stage}: {reports:?}");
        assert_only_store(&root);
        assert_eq!(Journal::open_read_only(&store::session_path(&root, A)).unwrap().records().unwrap().len(), 133);
        let db = store::open(&root).unwrap();
        assert_eq!(count(&db, "SELECT count(*) FROM sessions WHERE name LIKE 'session-%'"), 2);
    }
}
#[test]
#[ignore = "needs the private legacy-archive fixture that is not in the public repository"]
fn the_compactor_migrates_one_legacy_folder_per_pass() {
    let t = tempfile::tempdir().unwrap();
    let root = legacy_copy(&t);
    let first = compact::compact_once(&root, heimdall_archive::wall_ns()).unwrap();
    assert_eq!(first.migrated_sessions, 1);
    assert!(first.errors.is_empty(), "{:?}", first.errors);
    assert!(!root.join(A).exists() && root.join(B).is_dir());
    for _ in 0..2 {
        compact::compact_once(&root, heimdall_archive::wall_ns()).unwrap();
    }
    assert!(!migrate::pending(&root).unwrap());
    assert_only_store(&root);
}
