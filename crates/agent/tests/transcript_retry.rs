use agent::{
    harness::{Pi, PiConfig},
    protocol::{Event, Observation},
    Bridge, Status,
};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
fn config(dir: &Path) -> PiConfig {
    PiConfig::fixture(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mock-adapter.py"),
        dir.join("session.jsonl"),
        dir.to_owned(),
    )
}
fn speech() -> Observation {
    Observation {
        id: "speech-1".into(),
        timestamp_ms: 1,
        source: "Mic".into(),
        speaker: "User".into(),
        language: "en".into(),
        text: "remember the blue folder".into(),
    }
}
async fn next(b: &mut Bridge<Pi>) -> Event {
    tokio::time::timeout(Duration::from_secs(3), b.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap()
}
async fn consumed_503(dir: &Path) -> (Bridge<Pi>, String) {
    let mut b = Bridge::open(
        Pi::launch_config(config(dir)).unwrap(),
        dir.join("spool.sqlite"),
    )
    .unwrap();
    b.observe(speech()).await.unwrap();
    let id = b.wake("[fixture:503]", &[], false).await.unwrap().unwrap();
    let mut consumed = false;
    loop {
        match next(&mut b).await {
            Event::Consumed { id: cid } => {
                assert_eq!(cid, id);
                consumed = true
            }
            Event::Deferred { .. } => break,
            _ => {}
        }
    }
    assert!(consumed);
    assert_eq!(b.status, Status::WaitingEngine);
    (b, id)
}
async fn retry_text(b: &mut Bridge<Pi>) -> String {
    b.engine_ready();
    b.flush_queued().await.unwrap();
    let mut text = None;
    loop {
        match next(b).await {
            Event::Other(v) if v["type"] == "mock_pi" => {
                text = Some(v["request"]["message"].as_str().unwrap().to_owned())
            }
            Event::Consumed { .. } => return text.unwrap(),
            _ => {}
        }
    }
}
#[tokio::test]
async fn consumed_speech_missing_from_reset_session_must_be_retransmitted() {
    let d = tempfile::tempdir().unwrap();
    let (mut b, id) = consumed_503(d.path()).await;
    b.shutdown().await.unwrap();
    drop(b);
    // Model a replaced/reset Pi context while retaining Heimdall's durable spool.
    std::fs::remove_file(d.path().join("session.jsonl")).unwrap();
    let mut b = Bridge::open(
        Pi::launch_config(config(d.path())).unwrap(),
        d.path().join("spool.sqlite"),
    )
    .unwrap();
    let text = retry_text(&mut b).await;
    b.shutdown().await.unwrap();
    eprintln!("reset session retry for {id}: {text}");
    assert_resend(&text);
    assert!(
        text.contains("remember the blue folder"),
        "only a context reference was sent to an empty Pi session"
    );
}
#[tokio::test]
async fn migration_of_legacy_queue_resends_retained_speech_once_with_a_marker() {
    let d = tempfile::tempdir().unwrap();
    let (mut b, _) = consumed_503(d.path()).await;
    b.shutdown().await.unwrap();
    drop(b);
    // Recreate the 521dc2e schema and wire: no settlement or attempt evidence.
    let db = rusqlite::Connection::open(d.path().join("spool.sqlite")).unwrap();
    let has_consumed = db.prepare("PRAGMA table_info(observations)").unwrap().query_map([], |r| r.get::<_, String>(1)).unwrap().any(|name| name.unwrap() == "consumed");
    if has_consumed { db.execute_batch("ALTER TABLE observations DROP COLUMN consumed").unwrap(); }
    db.execute_batch("DROP INDEX observations_unsettled; ALTER TABLE observations DROP COLUMN settled;").unwrap();
    db.execute("UPDATE deliveries SET wire=json_remove(wire,'$.transcript_attempted')", []).unwrap();
    drop(db);
    let old = std::fs::read_to_string(d.path().join("session.jsonl")).unwrap();
    assert_eq!(old.matches("remember the blue folder").count(), 1);
    let mut b = Bridge::open(
        Pi::launch_config(config(d.path())).unwrap(),
        d.path().join("spool.sqlite"),
    )
    .unwrap();
    let text = retry_text(&mut b).await;
    b.shutdown().await.unwrap();
    assert_resend(&text);
    assert!(!d.path().join("context-queried").exists());
    let session = std::fs::read_to_string(d.path().join("session.jsonl")).unwrap();
    assert_eq!(
        session.matches("remember the blue folder").count(),
        2,
        "legacy recovery must preserve one original and one resend"
    );
}

fn assert_resend(text: &str) {
    assert_eq!(text.matches("remember the blue folder").count(), 1);
    assert_eq!(text.matches("(resent after a delivery interruption; ignore repeated speech)").count(), 1);
    assert!(!text.contains("observations already in context"));
}

#[tokio::test]
async fn restart_and_coalescing_resend_old_and_new_speech_once() {
    let d = tempfile::tempdir().unwrap();
    let (mut b, _) = consumed_503(d.path()).await;
    b.shutdown().await.unwrap(); drop(b);
    // Round-1/2 spools keep an unused consumed column; it must not suppress retry speech.
    let db = rusqlite::Connection::open(d.path().join("spool.sqlite")).unwrap();
    db.execute_batch("ALTER TABLE observations ADD COLUMN consumed INTEGER NOT NULL DEFAULT 1").unwrap();
    db.execute("UPDATE deliveries SET wire=json_set(wire,'$.transcript_context',json('{}'))", []).unwrap();
    drop(db);
    let mut b = Bridge::open(Pi::launch_config(config(d.path())).unwrap(), d.path().join("spool.sqlite")).unwrap();
    b.status = Status::WaitingEngine;
    let mut later = speech(); later.id = "speech-2".into(); later.text = "and the red folder".into();
    b.observe(later).await.unwrap(); b.wake("follow-up", &[], false).await.unwrap();
    let text = retry_text(&mut b).await;
    b.shutdown().await.unwrap();
    assert_resend(&text);
    assert_eq!(text.matches("and the red folder").count(), 1);
    assert!(!d.path().join("context-queried").exists());
}

#[tokio::test]
async fn compaction_between_retention_controls_cannot_omit_retry_speech() {
    let d = tempfile::tempdir().unwrap();
    let (mut b, _) = consumed_503(d.path()).await;
    std::fs::write(d.path().join("compact-between-controls"), "").unwrap();
    let text = retry_text(&mut b).await;
    b.shutdown().await.unwrap();
    assert_eq!(text.matches("remember the blue folder").count(), 1);
    assert!(!d.path().join("context-queried").exists());
    assert_resend(&text);
}

#[tokio::test]
async fn prompt_preflight_compaction_cannot_lose_retry_speech() {
    let d = tempfile::tempdir().unwrap();
    let (mut b, id) = consumed_503(d.path()).await;
    std::fs::write(d.path().join("compact-on-prompt"), "").unwrap();
    b.wake("recovered", &[], false).await.unwrap();
    let text = retry_text(&mut b).await;
    loop { if matches!(next(&mut b).await, Event::Complete { id: Some(ref completed), error: false, .. } if completed == &id) { break; } }
    assert_eq!(b.pending_observations().unwrap(), 0);
    b.flush_queued().await.unwrap();
    assert_eq!(b.queued_count().unwrap(), 0);
    b.shutdown().await.unwrap();
    let current = std::fs::read_to_string(d.path().join("active-messages.json")).unwrap();
    assert_eq!(current.matches("remember the blue folder").count(), 1);
    assert_resend(&text);
}

#[tokio::test]
async fn new_steer_never_queries_context_of_another_active_delivery() {
    let d = tempfile::tempdir().unwrap();
    let mut b = Bridge::open(Pi::launch_config(config(d.path())).unwrap(), d.path().join("spool.sqlite")).unwrap();
    b.observe(speech()).await.unwrap();
    let first = b.wake("[fixture:steering-thinking]", &[], false).await.unwrap().unwrap();
    loop { if matches!(next(&mut b).await, Event::Consumed { .. }) { break; } }
    let mut later = speech(); later.id = "speech-2".into(); later.text = "new steer".into();
    b.observe(later).await.unwrap();
    std::fs::write(d.path().join("stall-context"), "").unwrap();
    let result = tokio::time::timeout(Duration::from_secs(1), b.wake("follow-up", &[], false)).await;
    // Always clean up the isolated process, including the failing-first timeout.
    if result.is_err() { b.shutdown().await.unwrap(); panic!("new steering waited on a context query"); }
    let steer = result.unwrap().unwrap().unwrap();
    let finish = b.send_text(None, "[fixture:steering-finish]", false, None).await.unwrap().unwrap();
    let mut completed = vec![];
    let mut requests = vec![];
    while completed.len() < 3 {
        match next(&mut b).await {
            Event::Other(v) if v["type"] == "mock_pi" => requests.push(v["request"].clone()),
            Event::Complete { id: Some(id), error: false, .. } => completed.push(id),
            _ => {}
        }
    }
    b.shutdown().await.unwrap();
    assert_eq!(completed, [first, steer.clone(), finish]);
    let request = requests.iter().find(|v| v["id"] == steer).unwrap();
    assert_eq!(request["observation_ids"], serde_json::json!(["speech-2"]));
    let text = request["message"].as_str().unwrap();
    assert_eq!(text.matches("new steer").count(), 1);
    assert!(!text.contains("remember the blue folder") && !text.contains("resent after"));
    assert!(!d.path().join("context-queried").exists());
}
