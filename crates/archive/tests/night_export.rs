use heimdall_archive::{export, store, Journal, Record};
use rusqlite::params;
use serde_json::{json, Value};
use std::{fs, path::Path};
/// One complete session in the store at `root`: a transcript row and eight journal events.
fn session(root: &Path, name: &str) -> std::path::PathBuf {
    let path = store::session_path(root, name);
    let journal = Journal::open(&path).unwrap();
    let db = store::open(root).unwrap();
    db.execute(
        "INSERT INTO utterances(session,id,source_id,source,speaker,start_ns,end_ns,text,words,notes,decision) VALUES(?1,'u1','call','Discord','Speaker 1',100,200,'hello','[]','[]','{}')",
        [journal.session_id()],
    )
    .unwrap();
    for (kind, ids, ns, payload) in [
        ("command_decision", vec!["u1"], 201, json!({"answers":["false_alarm"]})),
        ("vad", vec![], 150, json!({"probability":0.9})),
        ("turn", vec![], 200, json!({"probability":0.8})),
        ("bug_note", vec![], 999, json!({"transcript":[{"id":"u1","text":"hello"}]})),
        ("agent_send", vec!["u1"], 201, json!({"message_id":"q1"})),
        ("agent_reply", vec!["u1"], 999, json!({"text":"reply"})),
        ("speaker_label", vec!["u1"], 200, json!({"name":"Alice"})),
        ("unrelated", vec![], 999, json!({"probability":0.1})),
    ] {
        journal
            .append(&Record {
                kind: kind.into(),
                source: "call".into(),
                utterance_ids: ids.into_iter().map(str::to_owned).collect(),
                host_ns: ns,
                wall_ns: ns,
                payload,
            })
            .unwrap();
    }
    store::mark_complete(&path).unwrap();
    path
}
#[test]
fn joined_export_keeps_every_event_once_with_session_keys() {
    let t = tempfile::tempdir().unwrap();
    let archive = t.path().join("archive");
    let s = session(&archive, "session-original");
    let snapshot = t.path().join("isolated-snapshot");
    export::snapshot_archive(&archive, &snapshot).unwrap();
    assert!(snapshot.join("NIGHT_SNAPSHOT").is_file());
    let output = t.path().join("dataset");
    assert_eq!(export::export(&snapshot, &output).unwrap(), 1);
    let row: Value = serde_json::from_str(fs::read_to_string(output.join("manifest.jsonl")).unwrap().trim()).unwrap();
    assert_eq!(row["schema"], 2);
    assert_eq!(row["session"], "session-original");
    let refs = row["evidence_refs"].as_array().unwrap();
    assert_eq!(refs.len(), 7);
    let events = fs::read_to_string(output.join("evidence.jsonl")).unwrap();
    assert_eq!(events.lines().count(), 8);
    for reference in refs {
        assert!(events.lines().any(|s| serde_json::from_str::<Value>(s).unwrap()["event_id"] == reference["event_id"]));
    }
    // Originals and archived failed/escape decision remain unchanged.
    assert_eq!(Journal::open_read_only(&s).unwrap().records().unwrap().len(), 8);
}
#[test]
fn snapshot_refuses_missing_store_and_existing_destination() {
    let t = tempfile::tempdir().unwrap();
    assert!(export::snapshot_archive(&t.path().join("no-archive"), &t.path().join("no-snapshot")).is_err());
    let archive = t.path().join("archive");
    session(&archive, "session-live");
    fs::create_dir(t.path().join("taken")).unwrap();
    assert!(export::snapshot_archive(&archive, &t.path().join("taken")).is_err());
    assert!(export::snapshot_archive(&archive, &archive.join("inside")).is_err());
    assert!(!archive.join("inside").exists());
}
#[test]
fn night_audio_is_content_addressed_and_original_transcript_is_preserved() {
    let t = tempfile::tempdir().unwrap();
    let archive = t.path().join("archive");
    session(&archive, "session-1");
    let night = t.path().join("night");
    fs::create_dir_all(night.join("audio")).unwrap();
    let bytes = b"night test audio";
    let hash = heimdall_archive::hash(bytes);
    let audio = format!("audio/{hash}.wav");
    fs::write(night.join(&audio), bytes).unwrap();
    let event = json!({"session":"session-1","event_id":"night:job1","record":{"kind":"night_model_output","source":"call","utterance_ids":["u1"],"host_ns":100,"wall_ns":999,"payload":{"tracks":[{"audio":audio,"audio_sha256":hash,"window_transcript":{"text":"recovered"}}]}}});
    fs::write(night.join("evidence.jsonl"), event.to_string()).unwrap();
    fs::create_dir_all(night.join("phases")).unwrap();
    fs::write(night.join("phases/duplicate.json"), event.to_string()).unwrap();
    let partial = json!({"session":"session-1","event_id":"night:partial","record":{"kind":"night_asr_output","source":"call","utterance_ids":["u1"],"host_ns":100,"wall_ns":999,"payload":{"text":"partial result survives kill"}}});
    fs::write(night.join("phases/partial.json"), partial.to_string()).unwrap();
    let output = t.path().join("dataset");
    export::export_with_additional(&archive, &output, Some(&night)).unwrap();
    let row: Value = serde_json::from_str(fs::read_to_string(output.join("manifest.jsonl")).unwrap().trim()).unwrap();
    assert_eq!(row["text"], "hello");
    assert_eq!(row["evidence_refs"].as_array().unwrap().len(), 9);
    assert!(output.join(audio).exists());
    let db = store::open(&archive).unwrap();
    assert_eq!(db.query_row("SELECT text FROM utterances", params![], |r| r.get::<_, String>(0)).unwrap(), "hello");
}
#[test]
fn live_snapshot_includes_committed_wal_and_stays_unchanged_after_new_evidence() {
    let t = tempfile::tempdir().unwrap();
    let archive = t.path().join("archive");
    let source = session(&archive, "session-live");
    let db = store::open(&archive).unwrap();
    db.execute("UPDATE utterances SET text='updated in WAL'", []).unwrap();
    let journal = Journal::open(&source).unwrap();
    journal
        .append(&Record { kind: "before_snapshot".into(), source: "call".into(), utterance_ids: vec!["u1".into()], host_ns: 200, wall_ns: 200, payload: json!({"retained":true}) })
        .unwrap();
    let snapshot = t.path().join("snapshot");
    export::snapshot_archive(&archive, &snapshot).unwrap();
    journal
        .append(&Record { kind: "after_snapshot".into(), source: "call".into(), utterance_ids: vec!["u1".into()], host_ns: 201, wall_ns: 201, payload: json!({"retained":true}) })
        .unwrap();
    let rows = Journal::open_read_only(&store::session_path(&snapshot, "session-live")).unwrap().records().unwrap();
    assert!(rows.iter().any(|r| r.kind == "before_snapshot"));
    assert!(!rows.iter().any(|r| r.kind == "after_snapshot"));
    let copy = store::open(&snapshot).unwrap();
    assert_eq!(copy.query_row("SELECT text FROM utterances", [], |r| r.get::<_, String>(0)).unwrap(), "updated in WAL");
    assert_eq!(Journal::open_read_only(&source).unwrap().records().unwrap().len(), 10);
}
#[test]
fn explicit_utterance_link_prevents_neighboring_command_training_label() {
    let t = tempfile::tempdir().unwrap();
    let archive = t.path().join("archive");
    let source = session(&archive, "session-1");
    let journal = Journal::open(&source).unwrap();
    let db = store::open(&archive).unwrap();
    db.execute(
        "INSERT INTO utterances(session,id,source_id,source,speaker,start_ns,end_ns,text,words,notes,decision) VALUES(?1,'u2','call','Discord','Speaker 2',100,200,'other voice','[]','[]','{}')",
        [journal.session_id()],
    )
    .unwrap();
    journal
        .append(&Record { kind: "command_decision".into(), source: "call".into(), utterance_ids: vec!["u1".into()], host_ns: 150, wall_ns: 150, payload: json!({"answer":"false_alarm"}) })
        .unwrap();
    drop(journal);
    drop(db);
    let output = t.path().join("dataset");
    export::export(&archive, &output).unwrap();
    let rows = fs::read_to_string(output.join("manifest.jsonl")).unwrap().lines().map(|s| serde_json::from_str::<Value>(s).unwrap()).collect::<Vec<_>>();
    let second = rows.iter().find(|r| r["utterance_id"] == "u2").unwrap();
    assert_eq!(second["evidence_refs"].as_array().unwrap().len(), 2);
    assert!(second["evidence_refs"].as_array().unwrap().iter().all(|r| r["kind"] == "vad" || r["kind"] == "turn"));
}
