use agent::{
    protocol::{message, Attachment, Event, Framer, Observation},
    Bridge, Status,
};
use serde_json::json;
fn observation(n: u64) -> Observation {
    Observation {
        id: format!("event-{n}"),
        timestamp_ms: n,
        source: "mic:headset".into(),
        speaker: "User".into(),
        language: "pl".into(),
        text: format!("Spójrz na to. [screenshot selected] sentence-{n}."),
    }
}
#[test]
fn rpc_framing_every_byte() {
    let raw = include_bytes!("fixtures/pi-rpc.jsonl");
    let mut f = Framer::default();
    let mut frames = vec![];
    for b in raw {
        frames.extend(f.push(&[*b]).unwrap());
    }
    f.finish().unwrap();
    assert_eq!(frames[0]["id"], "one");
    assert_eq!(frames[0]["success"], true);
    assert_eq!(frames.last().unwrap()["type"], "agent_settled");
    assert!(frames.iter().any(|v| v["type"] == "compaction_end"));
}
#[test]
fn utf8_split_and_bad_eof() {
    let mut f = Framer::default();
    let raw = "{\"text\":\"żółć\"}\r\n\n".as_bytes();
    let mut all = Vec::new();
    for b in raw {
        all.extend(f.push(&[*b]).unwrap());
    }
    assert_eq!(all[0]["text"], "żółć");
    f.finish().unwrap();
    f.push(b"{\"partial\":").unwrap();
    assert!(f.finish().is_err());
}
#[test]
fn envelope_images_steering() {
    let v = message(
        "id",
        "look",
        &[Attachment {
            media_type: "image/png".into(),
            bytes: vec![1, 2, 3],
        }],
        true,
        true,
    )
    .unwrap();
    assert_eq!(v["priority"], "now");
    assert_eq!(v["message"]["content"][1]["source"]["data"], "AQID");
    assert_eq!(v["shouldQuery"], true);
}
#[tokio::test]
async fn review_survives_crash_without_pi_history_and_keeps_evidence() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("spool.sqlite");
    let id;
    let expected;
    {
        let mut bridge = Bridge::dry_run(&path).unwrap();
        bridge.observe(observation(1)).await.unwrap();
        id = bridge.wake("Inspect the screenshot", &[Attachment {
            media_type: "image/png".into(), bytes: vec![1, 2, 3],
        }], false).await.unwrap().unwrap();
        expected = bridge.delivery_reviews().unwrap().remove(0);
        assert_eq!(expected.id, id);
        assert!(expected.text.contains("Inspect the screenshot"));
        assert!(expected.text.contains("observations already in context"));
        assert!(expected.timestamp_ms.is_some());
        assert_eq!(expected.attachments[0].0, "image/png");
        assert_eq!(expected.attachments[0].1.as_deref(), Some("039058c6f2c0cb492c533b0a4d14ef77cc0f78abccced5287d84a1a2011cfb81"));
    }
    let mut reopened = Bridge::dry_run(&path).unwrap();
    assert_eq!(reopened.status, Status::NeedsReconciliation(id.clone()));
    assert_eq!(reopened.delivery_reviews().unwrap(), vec![expected]);
    reopened.reconcile(&id, false).unwrap();
    assert!(reopened.delivery_reviews().unwrap().is_empty());
}

#[test]
fn review_is_bounded_utf8_safe_and_supports_legacy_wires() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("spool.sqlite");
    drop(Bridge::dry_run(&path).unwrap());
    let db = rusqlite::Connection::open(&path).unwrap();
    let mut wire = message("legacy", &"界".repeat(agent::REVIEW_TEXT_BYTES),
        &vec![Attachment { media_type: "image/png".into(), bytes: vec![1, 2, 3] }; agent::REVIEW_ATTACHMENTS + 1], true, false).unwrap();
    wire.as_object_mut().unwrap().remove("timestamp_ms");
    for image in wire["message"]["content"].as_array_mut().unwrap().iter_mut().skip(1) {
        image["source"].as_object_mut().unwrap().remove("archive_image_id");
    }
    for n in 0..agent::REVIEW_ROWS + 1 {
        db.execute("INSERT INTO deliveries VALUES(?1,?2,1,0,'needs_reconciliation')",
            rusqlite::params![format!("wake-{n}"), wire.to_string()]).unwrap();
    }
    let mut bridge = Bridge::dry_run(&path).unwrap();
    let reviews = bridge.delivery_reviews().unwrap();
    assert_eq!(reviews.len(), agent::REVIEW_ROWS);
    assert_eq!(bridge.inflight().unwrap().len(), agent::REVIEW_ROWS + 1);
    for review in reviews {
        assert_eq!(review.text.len(), agent::REVIEW_TEXT_BYTES / 3 * 3);
        assert!(review.text_truncated && review.attachments_truncated);
        assert_eq!(review.timestamp_ms, None);
        assert_eq!(review.attachments.len(), agent::REVIEW_ATTACHMENTS);
        assert_eq!(review.attachments[0].1.as_deref(), Some("039058c6f2c0cb492c533b0a4d14ef77cc0f78abccced5287d84a1a2011cfb81"));
    }
    bridge.reconcile("wake-0", false).unwrap();
    assert_eq!(bridge.delivery_reviews().unwrap().last().unwrap().id, "wake-32");
}
#[tokio::test]
async fn dry_run_never_wakes_for_observation() {
    let mut b = Bridge::dry_run(":memory:").unwrap();
    b.observe(observation(1)).await.unwrap();
    assert_eq!(b.harness.log[0]["would_send"]["shouldQuery"], false);
    let id = b.wake("look", &[], false).await.unwrap().unwrap();
    assert_eq!(b.harness.log.len(), 2);
    assert!(
        b.harness.log[1]["would_send"]["message"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("observations already in context")
    );
    b.handle(&Event::Complete {
        id: Some("append-result".into()),
        query: false,
        error: false,
        text: String::new(),
    })
    .unwrap();
    assert_eq!(b.status, Status::Working);
    assert_eq!(b.pending_observations().unwrap(), 1);
    b.handle(&Event::Complete {
        id: Some(id),
        query: true,
        error: false,
        text: "ok".into(),
    })
    .unwrap();
    assert_eq!(b.pending_observations().unwrap(), 0);
    assert_eq!(b.status, Status::Idle);
}
#[tokio::test]
async fn limit_collects_and_reset_flushes() {
    let mut b = Bridge::dry_run(":memory:").unwrap();
    b.handle(&Event::Limit {
        rejected: true,
        reset: Some(agent::now_ms() / 1000 + 1),
        status: "rejected".into(),
    })
    .unwrap();
    b.observe(observation(1)).await.unwrap();
    assert!(b.wake("urgent", &[], true).await.unwrap().is_none());
    assert_eq!(b.harness.log.len(), 0);
    assert_eq!(b.pending_observations().unwrap(), 1);
    b.wait_for_reset().await.unwrap();
    assert_eq!(b.harness.log.len(), 1);
    assert_eq!(b.harness.log[0]["would_send"]["priority"], "now");
}
#[tokio::test]
async fn crash_keeps_session_and_requires_reconciliation() {
    let p = std::env::temp_dir().join(format!(
        "heimdall-agent-test-{}.sqlite",
        uuid::Uuid::new_v4()
    ));
    let mut b = Bridge::dry_run(&p).unwrap();
    let sid = b.session_id.clone();
    b.observe(observation(1)).await.unwrap();
    let id = b.wake("look", &[], false).await.unwrap().unwrap();
    drop(b);
    let mut b = Bridge::dry_run(&p).unwrap();
    assert_eq!(b.session_id, sid);
    assert!(matches!(b.status, Status::NeedsReconciliation(_)));
    // A wake waits behind the uncertain delivery instead of failing.
    assert_eq!(b.wake("repeat", &[], false).await.unwrap(), None);
    b.reconcile(&id, true).unwrap();
    assert_eq!(b.pending_observations().unwrap(), 0);
    drop(b);
    for ext in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{ext}", p.display()));
    }
}
#[test]
fn warning_does_not_mean_rejection() {
    let e = Event::Limit {
        rejected: false,
        reset: Some(123),
        status: "allowed_warning".into(),
    };
    let mut b = Bridge::dry_run(":memory:").unwrap();
    b.handle(&e).unwrap();
    assert_eq!(b.status, Status::Idle);
}
#[tokio::test]
async fn rejected_result_retries_with_newly_collected_transcript() {
    let mut b = Bridge::dry_run(":memory:").unwrap();
    b.observe(observation(1)).await.unwrap();
    let id = b.wake("answer", &[], false).await.unwrap().unwrap();
    b.handle(&Event::Limit {
        rejected: true,
        reset: Some(agent::now_ms() / 1000),
        status: "rejected".into(),
    })
    .unwrap();
    b.handle(&Event::Complete {
        id: Some(id),
        query: true,
        error: true,
        text: "limited".into(),
    })
    .unwrap();
    b.observe(observation(2)).await.unwrap();
    b.flush_queued().await.unwrap();
    let wire = &b.harness.log.last().unwrap()["would_send"];
    let text = wire["message"]["content"][0]["text"].as_str().unwrap();
    assert_eq!(text.matches("sentence-1.").count(), 1);
    assert_eq!(text.matches("sentence-2.").count(), 1);
    assert!(text.contains("(resent after a delivery interruption; ignore repeated speech)"));
    assert!(!text.contains("observations already in context"));
}
#[tokio::test]
async fn rate_limit_after_tool_call_is_never_blindly_replayed() {
    let mut b = Bridge::dry_run(":memory:").unwrap();
    let id = b.wake("do it", &[], false).await.unwrap().unwrap();
    b.handle(&Event::Other(json!({"type":"tool_execution_start","delivery_id":id,"message":{"content":[{"type":"tool_use","name":"Bash"}]}}))).unwrap();
    b.handle(&Event::Limit {
        rejected: true,
        reset: Some(agent::now_ms() / 1000),
        status: "rejected".into(),
    })
    .unwrap();
    b.handle(&Event::Complete {
        id: Some(id),
        query: true,
        error: true,
        text: "limited".into(),
    })
    .unwrap();
    // Settled as done (a tool ran), never queued for a replay and never left for review.
    assert!(!matches!(b.status, Status::NeedsReconciliation(_)));
    assert_eq!(b.take_resolutions()[0].outcome, agent::Resolved::SettledAfterError);
    let count = b.harness.log.len();
    b.flush_queued().await.unwrap();
    assert_eq!(b.harness.log.len(), count);
    b.observe(observation(1)).await.unwrap();
    assert_eq!(b.pending_observations().unwrap(), 1);
}
#[tokio::test]
async fn noquery_fallback_buffers_until_wake() {
    struct Buffered(agent::harness::DryRun);
    #[async_trait::async_trait]
    impl agent::harness::Harness for Buffered {
        async fn send(&mut self, v: serde_json::Value) -> anyhow::Result<()> {
            agent::harness::Harness::send(&mut self.0, v).await
        }
        async fn next(&mut self) -> anyhow::Result<Option<Event>> {
            Ok(None)
        }
        async fn shutdown(&mut self) -> anyhow::Result<()> {
            Ok(())
        }
    }
    let mut b = Bridge::open(Buffered(Default::default()), ":memory:").unwrap();
    b.observe(observation(1)).await.unwrap();
    assert!(b.harness.0.log.is_empty());
    b.wake("answer", &[], false).await.unwrap();
    assert_eq!(b.harness.0.log.len(), 1);
}

#[tokio::test]
async fn reset_timer_and_restart_preserve_effect_reconciliation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite");
    let mut b = Bridge::dry_run(&path).unwrap();
    let first = b.wake("act", &[], false).await.unwrap().unwrap();
    b.handle(&Event::Other(json!({"type":"tool_execution_start","delivery_id":first,"message":{"content":[{"type":"tool_use","name":"speak"}]}}))).unwrap();
    b.handle(&Event::Limit {
        rejected: true,
        reset: Some(agent::now_ms() / 1000),
        status: "rejected".into(),
    })
    .unwrap();
    // A second wake arrives while inference is limited.
    // Use an unknown reset to ensure it remains queued.
    b.handle(&Event::Limit {
        rejected: true,
        reset: None,
        status: "rejected".into(),
    })
    .unwrap();
    assert!(b.wake("next", &[], false).await.unwrap().is_none());
    b.handle(&Event::Limit {
        rejected: true,
        reset: Some(agent::now_ms() / 1000),
        status: "rejected".into(),
    })
    .unwrap();
    b.handle(&Event::Complete {
        id: Some(first.clone()),
        query: true,
        error: true,
        text: "limited after effect".into(),
    })
    .unwrap();
    // Repeated limit notifications must not release the same guard either.
    b.handle(&Event::Limit {
        rejected: true,
        reset: Some(agent::now_ms() / 1000),
        status: "rejected".into(),
    })
    .unwrap();
    let sent = b.harness.log.len();
    b.wait_for_reset().await.unwrap();
    // The effect-bearing wake settled without replay; only the wake queued during the limit goes out.
    assert_eq!(b.harness.log.len(), sent + 1);
    assert_ne!(b.harness.log.last().unwrap()["would_send"]["uuid"], first.as_str());
    let state: String = rusqlite::Connection::open(&path).unwrap().query_row("SELECT state FROM deliveries WHERE id=?1", [&first], |r| r.get(0)).unwrap();
    assert_eq!(state, "completed");
    drop(b);
    let b = Bridge::dry_run(&path).unwrap();
    assert!(!matches!(b.status, Status::NeedsReconciliation(ref id) if id == &first));
}

#[tokio::test]
async fn steering_during_unlabelled_tool_call_never_retries_either_wake() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite");
    let mut b = Bridge::dry_run(&path).unwrap();
    let first = b.wake("speak first", &[], false).await.unwrap().unwrap();
    b.handle(&Event::Echo { id: first.clone() }).unwrap();
    let second = b
        .wake("steer while a tool cycle is running", &[], true)
        .await
        .unwrap()
        .unwrap();
    b.handle(&Event::Echo { id: second.clone() }).unwrap();
    b.handle(&Event::Other(
        json!({"type":"tool_execution_start","toolCallId":"effect","toolName":"speak"}),
    ))
    .unwrap();
    b.handle(&Event::Limit {
        rejected: true,
        reset: Some(0),
        status: "fixture".into(),
    })
    .unwrap();
    for _ in 0..2 {
        b.handle(&Event::Complete {
            id: None,
            query: true,
            error: true,
            text: "interrupted after tool use".into(),
        })
        .unwrap();
    }
    assert!(!matches!(b.status, Status::NeedsReconciliation(_)));
    let db = rusqlite::Connection::open(&path).unwrap();
    for id in [&first, &second] {
        let state: String = db
            .query_row("SELECT state FROM deliveries WHERE id=?1", [id], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(state, "completed");
    }
    let sent = b.harness.log.len();
    b.flush_queued().await.unwrap();
    assert_eq!(b.harness.log.len(), sent);
    assert_eq!(b.status, Status::Idle);
}

#[tokio::test]
async fn overlapping_results_are_fifo_and_do_not_clear_working_early() {
    let mut b = Bridge::dry_run(":memory:").unwrap();
    b.wake("first", &[], false).await.unwrap();
    b.wake("second", &[], true).await.unwrap();
    let done = Event::Complete {
        id: None,
        query: true,
        error: false,
        text: "ok".into(),
    };
    b.handle(&done).unwrap();
    assert_eq!(b.status, Status::Working);
    b.handle(&done).unwrap();
    assert_eq!(b.status, Status::Idle);
}

#[tokio::test]
async fn ambiguous_tool_success_does_not_erase_effect_guard() {
    let mut b = Bridge::dry_run(":memory:").unwrap();
    b.wake("first", &[], false).await.unwrap();
    b.wake("steer", &[], true).await.unwrap();
    b.handle(&Event::Other(json!({"type":"tool_execution_start","message":{"content":[{"type":"tool_use","id":"effect","name":"speak"}]}}))).unwrap();
    b.handle(&Event::Complete {
        id: None,
        query: true,
        error: false,
        text: "ok".into(),
    })
    .unwrap();
    assert_eq!(b.status, Status::Working);
    b.handle(&Event::Complete {
        id: None,
        query: true,
        error: false,
        text: "ok".into(),
    })
    .unwrap();
    // Both possible owners of the effect are settled; neither is replayed, nothing waits for review.
    assert_eq!(b.status, Status::Idle);
    assert!(b.take_resolutions().is_empty(), "a successful reply is not reported as a failure");
    let sent = b.harness.log.len();
    assert!(b.wake("retry", &[], false).await.unwrap().is_some());
    assert_eq!(b.harness.log.len(), sent + 1);
    assert!(!b.harness.log.last().unwrap()["would_send"]["message"]["content"][0]["text"].as_str().unwrap().contains("resent"));
}

#[tokio::test]
async fn ambiguous_effect_survives_restart_after_other_wake_completes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite");
    let mut b = Bridge::dry_run(&path).unwrap();
    let first = b.wake("first", &[], false).await.unwrap().unwrap();
    let second = b.wake("steer", &[], true).await.unwrap().unwrap();
    b.handle(&Event::Other(json!({"type":"tool_execution_start","message":{"content":[{"type":"tool_use","id":"effect","name":"speak"}]}}))).unwrap();
    b.handle(&Event::Complete {
        id: Some(first),
        query: true,
        error: false,
        text: "ok".into(),
    })
    .unwrap();
    drop(b);
    let mut b = Bridge::dry_run(&path).unwrap();
    b.handle(&Event::Complete {
        id: None,
        query: true,
        error: false,
        text: "unlabelled completion".into(),
    })
    .unwrap();
    assert!(!matches!(&b.status, Status::NeedsReconciliation(_)));
    let db = rusqlite::Connection::open(&path).unwrap();
    let state: String = db
        .query_row("SELECT state FROM deliveries WHERE id=?1", [&second], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(state, "completed", "settled without replay");
}

#[tokio::test]
async fn durable_receipts_resolve_interrupted_wakes_without_replaying_effects() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("ledger");
    let mut b = Bridge::dry_run(&path).unwrap();
    let id = b.wake("do it", &[], false).await.unwrap().unwrap();
    drop(b);
    let mut b = Bridge::dry_run(&path).unwrap();
    let session = d.path().join("session");
    for completed in [false, true] {
        std::fs::write(&session,format!("{}\n",json!({"type":"custom","customType":"heimdall_receipt","data":{"id":id,"completed":completed,"effects":true}}))).unwrap();
        b.recover_receipts(&session).unwrap();
        assert!(!matches!(b.status, Status::NeedsReconciliation(_)));
        assert!(b.inflight().unwrap().is_empty());
        let sent = b.harness.log.len();
        b.flush_queued().await.unwrap();
        assert_eq!(b.harness.log.len(), sent, "a wake with effects is never replayed (completed={completed})");
    }
}
#[tokio::test]
async fn spool_notice_is_deduplicated_and_cannot_cross_off_on() {
    use agent::{
        delivery::{DeliveryGate, DeliveryPermit},
        harness::Harness,
    };
    use async_trait::async_trait;
    use std::sync::{atomic::Ordering, Arc};
    struct Sink {
        gate: Arc<DeliveryGate>,
        sent: usize,
    }
    #[async_trait]
    impl Harness for Sink {
        fn delivery_epoch(&self) -> Option<u64> {
            Some(self.gate.generation())
        }
        async fn send(&mut self, _: serde_json::Value) -> anyhow::Result<()> {
            self.sent += 1;
            Ok(())
        }
        async fn send_with_permit(
            &mut self,
            v: serde_json::Value,
            p: DeliveryPermit,
        ) -> anyhow::Result<()> {
            p.check()?;
            self.send(v).await
        }
        async fn next(&mut self) -> anyhow::Result<Option<Event>> {
            Ok(None)
        }
        async fn shutdown(&mut self) -> anyhow::Result<()> {
            Ok(())
        }
    }
    let gate = Arc::new(DeliveryGate::new(true));
    let mut b = Bridge::open(
        Sink {
            gate: gate.clone(),
            sent: 0,
        },
        ":memory:",
    )
    .unwrap();
    b.status = Status::WaitingEngine;
    let p = gate.permit().unwrap();
    assert!(b
        .wake_keyed(Some("notice-1"), "one notice", &[], false, Some(p.clone()))
        .await
        .unwrap()
        .is_none());
    assert!(b
        .wake_keyed(Some("notice-1"), "same retry", &[], false, Some(p))
        .await
        .unwrap()
        .is_none());
    gate.store(false, Ordering::Release);
    gate.store(true, Ordering::Release);
    b.engine_ready();
    b.flush_queued().await.unwrap();
    assert_eq!(b.harness.sent, 0);
    b.wake_keyed(
        Some("notice-2"),
        "new epoch notice",
        &[],
        false,
        Some(gate.permit().unwrap()),
    )
    .await
    .unwrap();
    assert_eq!(b.harness.sent, 1);
}
#[test]
fn oversized_frames_fail_without_unbounded_growth() {
    let mut f = Framer::default();
    assert!(f.push(&vec![b'x'; 64 * 1024 * 1024 + 1]).is_err());
}

#[tokio::test]
async fn typed_text_does_not_attach_or_acknowledge_pending_observations() {
    let mut b = Bridge::dry_run(":memory:").unwrap();
    b.observe(observation(1)).await.unwrap();
    let id = b
        .send_text(None, "[Heimdall typed message]\nhello", false, None)
        .await
        .unwrap()
        .unwrap();
    let wire = &b.harness.log.last().unwrap()["would_send"];
    assert_eq!(
        wire["message"]["content"][0]["text"],
        "[Heimdall typed message]\nhello"
    );
    b.handle(&Event::Complete {
        id: Some(id),
        query: true,
        error: false,
        text: "reply".into(),
    })
    .unwrap();
    assert_eq!(b.pending_observations().unwrap(), 1);
}

#[tokio::test]
async fn settled_payload_retirement_is_archive_first_and_preserves_unresolved_images() {
    use std::sync::{Arc,Mutex};
    let d=tempfile::tempdir().unwrap();let path=d.path().join("db");let mut b=Bridge::dry_run(&path).unwrap();
    let first=b.wake("completed body",&[Attachment{media_type:"image/png".into(),bytes:vec![1,2,3]}],false).await.unwrap().unwrap();
    let second=b.wake("unresolved body",&[Attachment{media_type:"image/png".into(),bytes:vec![4,5,6]}],false).await.unwrap().unwrap();
    b.handle(&Event::Complete{id:Some(first.clone()),query:true,error:false,text:"ok".into()}).unwrap();
    let fail:agent::PayloadArchive=Arc::new(|_|anyhow::bail!("archive unavailable"));assert!(b.retire(&fail).is_err());
    let saved=Arc::new(Mutex::new(vec![]));let archive=saved.clone();let archive:agent::PayloadArchive=Arc::new(move|v|{archive.lock().unwrap().push(v.clone());Ok("saved".into())});
    b.retire(&archive).unwrap();let db=rusqlite::Connection::open(path).unwrap();
    let wire=|id:&str| db.query_row("SELECT wire FROM deliveries WHERE id=?",[id],|r|r.get::<_,String>(0)).unwrap();
    assert!(wire(&first).contains("archive_ref"));assert!(!wire(&first).contains("AQID"));assert!(wire(&second).contains("BAUG"));
    assert!(saved.lock().unwrap()[0]["wire"].to_string().contains("AQID"));assert_eq!(b.inflight().unwrap(),vec![second]);
}

struct EpochSink { gate:std::sync::Arc<agent::delivery::DeliveryGate>, sent:Vec<serde_json::Value>, serial:bool }
#[async_trait::async_trait]
impl agent::harness::Harness for EpochSink {
    fn delivery_epoch(&self)->Option<u64>{Some(self.gate.generation())}
    fn serial_wakes(&self)->bool{self.serial}
    async fn send(&mut self,v:serde_json::Value)->anyhow::Result<()> {self.sent.push(v);Ok(())}
    async fn send_with_permit(&mut self,v:serde_json::Value,p:agent::delivery::DeliveryPermit)->anyhow::Result<()> {p.check()?;self.send(v).await}
    async fn next(&mut self)->anyhow::Result<Option<Event>> {Ok(None)}
    async fn shutdown(&mut self)->anyhow::Result<()> {Ok(())}
}
#[tokio::test]
async fn voice_wakes_queued_behind_busy_turn_or_engine_down_cannot_cross_off_on() {
    use std::sync::{Arc,atomic::Ordering};
    for busy in [false,true] {
        let gate=Arc::new(agent::delivery::DeliveryGate::new(true));
        let mut b=Bridge::open(EpochSink{gate:gate.clone(),sent:vec![],serial:busy},":memory:").unwrap();
        let active=if busy { b.wake("active",&[],false).await.unwrap() }else {b.status=Status::WaitingEngine;None};
        b.wake("old queued voice",&[],false).await.unwrap();
        assert_eq!(b.queued_count().unwrap(),1);
        gate.store(false,Ordering::Release);gate.store(true,Ordering::Release);
        if let Some(id)=active {b.reconcile(&id,false).unwrap();}else{b.engine_ready();}
        b.flush_queued().await.unwrap();
        assert_eq!(b.queued_count().unwrap(),0);
        assert_eq!(b.harness.sent.len(),usize::from(busy));
    }
}
#[tokio::test]
async fn upgraded_legacy_live_spool_cancels_unscoped_queued_wake_on_open() {
    let d=tempfile::tempdir().unwrap();let spool=d.path().join("old.sqlite");
    let db=rusqlite::Connection::open(&spool).unwrap();
    db.execute_batch("CREATE TABLE deliveries(id TEXT PRIMARY KEY,wire TEXT NOT NULL,query INTEGER NOT NULL,last_seq INTEGER NOT NULL,state TEXT NOT NULL)").unwrap();
    let wire=agent::protocol::message("legacy","old queued voice",&[],true,false).unwrap();
    db.execute("INSERT INTO deliveries VALUES('legacy',?1,1,0,'queued')",[wire.to_string()]).unwrap();drop(db);
    let gate=std::sync::Arc::new(agent::delivery::DeliveryGate::new(true));
    let mut b=Bridge::open(EpochSink{gate,sent:vec![],serial:false},&spool).unwrap();
    let db=rusqlite::Connection::open(&spool).unwrap();
    assert_eq!(db.query_row("SELECT state FROM deliveries WHERE id='legacy'",[],|r|r.get::<_,String>(0)).unwrap(),"cancelled");
    b.flush_queued().await.unwrap();assert!(b.harness.sent.is_empty());
}

#[tokio::test]
async fn ten_wakes_coalesce_ordered_without_repeating_completed_observations() {
    let gate = std::sync::Arc::new(agent::delivery::DeliveryGate::new(true));
    let mut b = Bridge::open(EpochSink { gate, sent: vec![], serial: true }, ":memory:").unwrap();
    b.observe(observation(1)).await.unwrap();
    let first = b.wake("first", &[], false).await.unwrap().unwrap();
    for n in 2..12 {
        b.observe(observation(n)).await.unwrap();
        // Ambient urgent wakes also stay in the single follow-up; typed steer is separate.
        b.wake_keyed(Some(&format!("wake-{n}")), &format!("instruction {n}"), &[Attachment {
            media_type: "image/png".into(), bytes: vec![42],
        }], n == 3, None).await.unwrap();
        assert_eq!(b.queued_count().unwrap(), 1);
        assert_eq!(b.inflight().unwrap(), vec![first.clone()]);
    }
    // Replayed stable keys do not repeat instructions or screenshots.
    assert!(b.wake_keyed(Some("wake-3"), "DUPLICATE", &[], false, None).await.unwrap().is_none());
    let typed = b.send_text(Some("typed"), "typed next", false, None).await.unwrap();
    assert!(typed.is_none()); assert_eq!(b.queued_count().unwrap(), 2);
    b.handle(&Event::Complete { id: Some(first), query: true, error: false, text: "done".into() }).unwrap();
    b.flush_queued().await.unwrap();
    assert_eq!(b.harness.sent.len(), 2);
    assert_eq!(b.queued_count().unwrap(), 1);
    let wire = b.harness.sent[1].clone();
    let text = wire["message"]["content"][0]["text"].as_str().unwrap();
    assert!(!text.contains("sentence-1."));
    assert!(!text.contains("DUPLICATE"));
    let mut previous = 0;
    for n in 2..12 {
        let event = format!("sentence-{n}.");
        assert_eq!(text.matches(&event).count(), 1);
        let pos = text.find(&event).unwrap(); assert!(pos >= previous); previous = pos;
        assert_eq!(text.matches(&format!("instruction {n}")).count(), 1);
    }
    assert_eq!(wire["message"]["content"].as_array().unwrap().len(), 2, "one content ID attached once");
    b.handle(&Event::Complete { id: wire["uuid"].as_str().map(str::to_owned), query: true, error: false, text: "done".into() }).unwrap();
    b.flush_queued().await.unwrap();
    assert_eq!(b.harness.sent[2]["uuid"], "typed");
    assert_eq!(b.queued_count().unwrap(), 0);
    b.handle(&Event::Complete { id: Some("typed".into()), query: true, error: false, text: "done".into() }).unwrap();
    assert!(b.inflight().unwrap().is_empty());
    assert_eq!(b.pending_observations().unwrap(), 0);
    assert_eq!(b.status, Status::Idle);
}

#[test]
fn bounded_wake_images_deduplicate_and_keep_overflow_content_ids() {
    let images: Vec<_> = (0..12).flat_map(|n|vec![Attachment { media_type:"image/png".into(), bytes:vec![n] };2]).collect();
    let mut wire = message("wake", "look", &images, true, false).unwrap();
    agent::protocol::bound_wake_images(&mut wire);
    assert_eq!(wire["message"]["content"].as_array().unwrap().len(), agent::protocol::WAKE_IMAGES+1);
    assert_eq!(wire["screenshot_overflow"].as_array().unwrap().len(), 12-agent::protocol::WAKE_IMAGES);
    let before = wire.clone(); agent::protocol::bound_wake_images(&mut wire); assert_eq!(wire,before);
    agent::protocol::describe_overflow_images(&mut wire);
    let text = wire["message"]["content"][0]["text"].as_str().unwrap();
    for id in wire["screenshot_overflow"].as_array().unwrap() { assert!(text.contains(id.as_str().unwrap())); }
}

#[tokio::test]
async fn upgrade_and_restart_keep_one_pending_wake_and_stable_aliases() {
    let dir = tempfile::tempdir().unwrap(); let path = dir.path().join("spool");
    let gate = std::sync::Arc::new(agent::delivery::DeliveryGate::new(true));
    let sink = || EpochSink {gate:gate.clone(),sent:vec![],serial:true};
    drop(Bridge::open(sink(), &path).unwrap());
    let db = rusqlite::Connection::open(&path).unwrap();
    for n in 0..10 {
        let id = format!("old-{n}");
        let mut wire = message(&id,"old transcript",&[Attachment{media_type:"image/png".into(),bytes:vec![n]}],true,false).unwrap();
        wire["delivery_epoch"] = json!(gate.generation());
        db.execute("INSERT INTO deliveries VALUES(?1,?2,1,0,'queued')", rusqlite::params![id,wire.to_string()]).unwrap();
        db.execute("INSERT INTO wake_instructions VALUES(?1,?2)", rusqlite::params![id,format!("old instruction {n}")]).unwrap();
    }
    drop(db);
    let mut b = Bridge::open(sink(), &path).unwrap();
    assert_eq!(b.queued_count().unwrap(),1);
    assert!(b.wake_keyed(Some("old-7"),"DO NOT REPEAT",&[],false,None).await.unwrap().is_none());
    b.status = Status::WaitingEngine; // genuine fallback retains the alias/coalescing migration case
    b.wake_keyed(Some("new-key"),"new instruction",&[],false,None).await.unwrap();
    drop(b);
    let mut b = Bridge::open(sink(), &path).unwrap();
    assert_eq!(b.queued_count().unwrap(),1);
    assert!(b.wake_keyed(Some("new-key"),"DO NOT REPEAT",&[],false,None).await.unwrap().is_none());
    b.flush_queued().await.unwrap();
    assert_eq!(b.harness.sent.len(),1);
    let wire = &b.harness.sent[0];
    let text = wire["message"]["content"][0]["text"].as_str().unwrap();
    assert!(!text.contains("DO NOT REPEAT"));
    for n in 0..10 { assert!(text.contains(&format!("old instruction {n}"))); }
    assert!(text.contains("new instruction"));
    assert_eq!(wire["screenshot_overflow"].as_array().unwrap().len(),2);
}


#[tokio::test]
async fn typed_entries_are_barriers_between_contiguous_ambient_wakes() {
    let gate = std::sync::Arc::new(agent::delivery::DeliveryGate::new(true));
    let mut b = Bridge::open(EpochSink { gate, sent: vec![], serial: true }, ":memory:").unwrap();
    b.observe(observation(1)).await.unwrap();
    b.wake_keyed(Some("A"), "active A", &[], false, None).await.unwrap();
    b.observe(observation(2)).await.unwrap();
    b.wake_keyed(Some("B"), "voice B", &[], false, None).await.unwrap();
    b.send_text(Some("T"), "typed T", false, None).await.unwrap();
    b.observe(observation(3)).await.unwrap();
    b.wake_keyed(Some("C"), "voice C", &[], false, None).await.unwrap();
    b.wake_keyed(Some("D"), "voice D", &[], false, None).await.unwrap();
    assert_eq!(b.queued_count().unwrap(), 3, "B, T, and contiguous C+D");
    for (active, next) in [("A", "B"), ("B", "T"), ("T", "C")] {
        b.handle(&Event::Complete { id: Some(active.into()), query: true, error: false, text: "done".into() }).unwrap();
        b.flush_queued().await.unwrap();
        let wire = b.harness.sent.last().unwrap();
        assert_eq!(wire["uuid"], next);
        let text = wire["message"]["content"][0]["text"].as_str().unwrap();
        if next == "B" {
            assert!(!text.contains("voice C") && !text.contains("voice D"));
            assert!(text.contains("sentence-2."));
            assert!(!text.contains("sentence-3."), "post-typed speech crossed the ordering barrier: {text}");
        }
        if next == "C" {
            assert!(text.contains("voice C") && text.contains("voice D"));
            assert!(text.contains("sentence-3."));
            assert!(!text.contains("sentence-2."));
        }
    }
}

#[tokio::test]
async fn backlog_migration_coalesces_each_contiguous_run_without_crossing_typed_entries() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("spool");
    let gate = std::sync::Arc::new(agent::delivery::DeliveryGate::new(true));
    let sink = || EpochSink { gate: gate.clone(), sent: vec![], serial: true };
    drop(Bridge::open(sink(), &path).unwrap());
    let db = rusqlite::Connection::open(&path).unwrap();
    for (id, ambient) in [("B", true), ("T", false), ("C", true), ("D", true), ("U", false), ("E", true), ("F", true)] {
        let mut wire = message(id, &format!("input {id}"), &[], true, false).unwrap();
        wire["delivery_epoch"] = json!(gate.generation());
        db.execute("INSERT INTO deliveries VALUES(?1,?2,1,0,'queued')", rusqlite::params![id, wire.to_string()]).unwrap();
        if ambient { db.execute("INSERT INTO wake_instructions VALUES(?1,?2)", rusqlite::params![id, format!("voice {id}")]).unwrap(); }
    }
    drop(db);
    let mut b = Bridge::open(sink(), &path).unwrap();
    assert_eq!(b.queued_count().unwrap(), 5, "B, T, C+D, U, E+F");
    for id in ["B", "T", "C", "U", "E"] {
        b.flush_queued().await.unwrap();
        assert_eq!(b.harness.sent.last().unwrap()["uuid"], id);
        b.handle(&Event::Complete { id: Some(id.into()), query: true, error: false, text: "done".into() }).unwrap();
    }
    assert_eq!(b.queued_count().unwrap(), 0);
    // Stable aliases must survive the upgrade and restart as well.
    drop(b);
    let mut b = Bridge::open(sink(), &path).unwrap();
    for id in ["D", "F"] { assert!(b.wake_keyed(Some(id), "duplicate", &[], false, None).await.unwrap().is_none()); }
}

#[tokio::test]
async fn asynchronous_wake_keeps_its_observation_ceiling_through_queue_merge_and_restart() {
    let temp=tempfile::tempdir().unwrap();let spool=temp.path().join("spool.sqlite");
    let gate=std::sync::Arc::new(agent::delivery::DeliveryGate::new(true));
    let adapter=||EpochSink {gate:gate.clone(),sent:vec![],serial:true};
    let mut b=Bridge::open(adapter(),&spool).unwrap();
    b.status=Status::WaitingEngine;
    for n in 1..=3 {b.observe(observation(n)).await.unwrap();}
    b.wake_keyed_through(Some("first"),"respond",&[],false,None,Some("event-1"),agent::Speaker::Owner).await.unwrap();
    let queued=b.last_queued().unwrap().unwrap();
    let text=queued["message"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("sentence-1.") && !text.contains("sentence-2."));
    assert_eq!(queued["observation_ids"], json!(["event-1"]));
    b.wake_keyed_through(Some("second"),"respond",&[],false,None,Some("event-2"),agent::Speaker::Owner).await.unwrap();
    assert_eq!(b.queued_count().unwrap(),1);drop(b);
    let mut b=Bridge::open(adapter(),&spool).unwrap();b.engine_ready();b.flush_queued().await.unwrap();
    let text=b.harness.sent[0]["message"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("sentence-1.") && text.contains("sentence-2.") && !text.contains("sentence-3."));
    assert_eq!(b.harness.sent[0]["observation_ids"], json!(["event-1", "event-2"]));
}

#[test]
fn compact_rendering_preserves_words_and_screenshot_markers() {
    let mut o = observation(1);
    o.text = "Dobra, zobacz to [screenshot 14:02:40 attached]".into();
    assert_eq!(o.labelled(), "[Mic · User] Dobra, zobacz to [screenshot 14:02:40 attached]");
    o.source = "Discord".into(); o.speaker = "Dave".into();
    assert_eq!(o.labelled(), "[Discord · Dave] Dobra, zobacz to [screenshot 14:02:40 attached]");
    o.speaker="Call".into();o.text="Dave: first sentence\nAda: second sentence".into();
    assert_eq!(o.labelled(), "[Discord · Dave] first sentence\n[Discord · Ada] second sentence");
}

#[tokio::test]
async fn durable_metadata_owns_exact_batch_and_legacy_queue_upgrade() {
    let dir = tempfile::tempdir().unwrap(); let path = dir.path().join("spool.sqlite");
    {
        let mut b = Bridge::dry_run(&path).unwrap();
        b.observe(observation(1)).await.unwrap();
        let append = &b.harness.log[0]["would_send"];
        assert_eq!(append["observation_ids"], json!(["event-1"]));
        let text = append["message"]["content"][0]["text"].as_str().unwrap();
        assert!(text.starts_with("Observation:\n[") && text.contains(" Mic · User] "), "{text}");
    }
    // A previous build stored the verbose labelled observation. The upgrade
    // reads it without changing the archived source record or dedupe key.
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute("UPDATE observations SET supplied=0,text=?1", ["event=event-1 timestamp_ms=1 source=\"mic:headset\" speaker=\"User\" lang=pl: old words [screenshot 14:02:40 attached]"]).unwrap();
    drop(db);
    let mut b = Bridge::dry_run(&path).unwrap();
    b.observe(observation(1)).await.unwrap(); // same id stays deduplicated
    b.observe(observation(2)).await.unwrap();
    let id = b.wake_keyed_through(Some("wake"), "answer", &[], false, None, Some("event-1"), agent::Speaker::Owner).await.unwrap().unwrap();
    let wire = &b.harness.log.last().unwrap()["would_send"];
    assert_eq!(wire["observation_ids"], json!(["event-1"]));
    let text = wire["message"]["content"][0]["text"].as_str().unwrap();
    // Each line carries the clock time it was heard (root cause 4): `[HH:MM:SS Mic · User] …`.
    assert!(text.starts_with("Transcript:\n[") && text.contains(" Mic · User] old words [screenshot 14:02:40 attached]"), "{text}");
    assert!(!text.contains("event=") && !text.contains("timestamp_ms=") && !text.contains("lang=") && !text.contains("sentence-2."));
    // Durable receipts still settle the delivery id, never an observation id.
    let session = dir.path().join("session.jsonl");
    std::fs::write(&session, format!("{{\"type\":\"custom\",\"customType\":\"heimdall_receipt\",\"data\":{{\"id\":\"{id}\",\"completed\":true}}}}\n")).unwrap();
    b.recover_receipts(&session).unwrap();
    assert!(b.inflight().unwrap().is_empty());
    b.wake("next", &[], false).await.unwrap();
    assert_eq!(b.harness.log.last().unwrap()["would_send"]["observation_ids"], json!(["event-2"]));
}

struct SteerSink { sent:Vec<serde_json::Value>, blocked:bool, steering:bool, fallbacks:Vec<String> }
#[async_trait::async_trait]
impl agent::harness::Harness for SteerSink {
    fn serial_wakes(&self)->bool { true }
    fn supports_steering(&self)->bool { self.steering }
    fn input_blocked(&self)->Option<&'static str> { self.blocked.then_some("Compaction in progress") }
    fn record_queue_fallback(&mut self, _:&serde_json::Value, reason:&str)->anyhow::Result<()> { self.fallbacks.push(reason.into()); Ok(()) }
    async fn send(&mut self, wire:serde_json::Value)->anyhow::Result<()> { self.sent.push(wire); Ok(()) }
    async fn next(&mut self)->anyhow::Result<Option<Event>> { Ok(None) }
    async fn shutdown(&mut self)->anyhow::Result<()> { Ok(()) }
}
#[tokio::test]
async fn out_of_order_settlement_keeps_rejected_speech_through_retirement_and_restart() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("spool.sqlite");
    let sink = || SteerSink { sent: vec![], blocked: false, steering: true, fallbacks: vec![] };
    let mut b = Bridge::open(sink(), &path).unwrap();
    b.observe(observation(1)).await.unwrap();
    let first = b.wake("first", &[], false).await.unwrap().unwrap();
    b.observe(observation(2)).await.unwrap();
    let second = b.wake("second", &[], false).await.unwrap().unwrap();
    b.handle(&Event::Deferred { id: first.clone(), reason: "Pi is compacting the session".into() }).unwrap();
    b.handle(&Event::Complete { id: Some(second), query: true, error: false, text: "done".into() }).unwrap();
    assert_eq!(b.pending_observations().unwrap(), 1);
    let archive: agent::PayloadArchive = std::sync::Arc::new(|_| Ok("saved".into()));
    b.retire(&archive).unwrap();
    drop(b);
    let mut b = Bridge::open(sink(), &path).unwrap();
    b.flush_queued().await.unwrap();
    let wire = &b.harness.sent[0];
    assert_eq!(wire["uuid"], first);
    assert_eq!(wire["observation_ids"], json!(["event-1"]));
    let text = wire["message"]["content"][0]["text"].as_str().unwrap();
    assert_eq!(text.matches("sentence-1.").count(), 1);
    assert!(!text.contains("sentence-2.") && !text.contains("archive:"));
    b.reconcile(&first, true).unwrap();
    assert_eq!(b.pending_observations().unwrap(), 0);
}

#[tokio::test]
async fn consumed_speech_is_resent_after_restart_and_coalescing() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("spool.sqlite");
    let sink = || SteerSink { sent: vec![], blocked: false, steering: true, fallbacks: vec![] };
    let mut b = Bridge::open(sink(), &path).unwrap();
    b.observe(observation(1)).await.unwrap();
    let first = b.wake("first", &[], false).await.unwrap().unwrap();
    b.handle(&Event::Consumed { id: first.clone() }).unwrap();
    b.handle(&Event::Deferred { id: first.clone(), reason: "Agent waiting for the engine".into() }).unwrap();
    assert_eq!(b.pending_observations().unwrap(), 1);
    b.observe(observation(2)).await.unwrap();
    b.wake("follow-up", &[], false).await.unwrap();
    drop(b);
    let mut b = Bridge::open(sink(), &path).unwrap();
    b.flush_queued().await.unwrap();
    let wire = &b.harness.sent[0];
    assert_eq!(wire["uuid"], first);
    assert_eq!(wire["observation_ids"], json!(["event-1", "event-2"]));
    let text = wire["message"]["content"][0]["text"].as_str().unwrap();
    assert_eq!(text.matches("sentence-1.").count(), 1);
    assert!(!text.contains("observations already in context"));
    assert_eq!(text.matches("sentence-2.").count(), 1);
    assert_eq!(b.pending_observations().unwrap(), 2);
    b.reconcile(&first, true).unwrap();
    assert_eq!(b.pending_observations().unwrap(), 0);
}

#[tokio::test]
async fn legacy_cursor_migration_restores_unresolved_observation_below_completed_wake() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("spool.sqlite");
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch("CREATE TABLE observations(seq INTEGER PRIMARY KEY AUTOINCREMENT,id TEXT UNIQUE NOT NULL,text TEXT NOT NULL);
        CREATE TABLE deliveries(id TEXT PRIMARY KEY,wire TEXT NOT NULL,query INTEGER NOT NULL,last_seq INTEGER NOT NULL,state TEXT NOT NULL);
        CREATE TABLE meta(key TEXT PRIMARY KEY,value TEXT NOT NULL);
        CREATE TABLE wake_instructions(id TEXT PRIMARY KEY,instruction TEXT NOT NULL);
        INSERT INTO meta VALUES('cursor','2'); INSERT INTO wake_instructions VALUES('first','respond')").unwrap();
    for (n, state) in [(1, "queued"), (2, "completed")] {
        let o = observation(n);
        db.execute("INSERT INTO observations(id,text) VALUES(?1,?2)", rusqlite::params![o.id,o.labelled()]).unwrap();
        let mut wire = message(if n == 1 { "first" } else { "second" }, "respond", &[], true, false).unwrap();
        wire["observation_ids"] = json!([o.id]);
        db.execute("INSERT INTO deliveries VALUES(?1,?2,1,?3,?4)", rusqlite::params![wire["uuid"].as_str().unwrap(),wire.to_string(),n as i64,state]).unwrap();
    }
    drop(db);
    let mut b = Bridge::open(SteerSink { sent: vec![], blocked: false, steering: true, fallbacks: vec![] }, &path).unwrap();
    assert_eq!(b.pending_observations().unwrap(), 1);
    b.flush_queued().await.unwrap();
    assert_eq!(b.harness.sent[0]["observation_ids"], json!(["event-1"]));
    assert!(b.harness.sent[0]["message"]["content"][0]["text"].as_str().unwrap().contains("sentence-1."));
}
#[tokio::test]
async fn active_wakes_and_typed_messages_steer_by_default_without_repeating_speech() {
    let mut b=Bridge::open(SteerSink {sent:vec![],blocked:false,steering:true,fallbacks:vec![]},":memory:").unwrap();
    b.observe(observation(1)).await.unwrap();
    let first=b.wake("first",&[],false).await.unwrap().unwrap();
    b.observe(observation(2)).await.unwrap();
    let second=b.wake("decider follow-up",&[],false).await.unwrap().unwrap();
    let typed=b.send_text(None,"ordinary typed message",false,None).await.unwrap().unwrap();
    assert_eq!(b.queued_count().unwrap(),0);
    assert_eq!(b.harness.sent.len(),3);
    assert!(b.harness.sent.iter().all(|w|w["priority"]=="now"));
    assert_eq!(b.harness.sent[0]["observation_ids"],json!(["event-1"]));
    assert_eq!(b.harness.sent[1]["observation_ids"],json!(["event-2"]));
    assert_eq!(b.harness.sent[2]["observation_ids"],json!([]));
    assert!(!b.harness.sent[1]["message"]["content"][0]["text"].as_str().unwrap().contains("sentence-1."));
    for id in [first,second,typed] {b.handle(&Event::Complete {id:Some(id),query:true,error:false,text:"one answer".into()}).unwrap();}
    assert_eq!(b.status,Status::Idle); assert!(b.inflight().unwrap().is_empty()); assert!(b.harness.fallbacks.is_empty());
}
#[tokio::test]
async fn compaction_fallback_coalesces_ambient_but_preserves_typed_order_then_steers_active_turn() {
    let mut b=Bridge::open(SteerSink {sent:vec![],blocked:false,steering:true,fallbacks:vec![]},":memory:").unwrap();
    b.observe(observation(1)).await.unwrap(); b.wake_keyed(Some("A"),"A",&[],false,None).await.unwrap();
    b.harness.blocked=true;
    for (id,n) in [("B",2),("C",3)] {b.observe(observation(n)).await.unwrap(); assert!(b.wake_keyed(Some(id),id,&[],false,None).await.unwrap().is_none());}
    b.send_text(Some("T"),"typed",false,None).await.unwrap();
    b.observe(observation(4)).await.unwrap(); b.wake_keyed(Some("D"),"D",&[],false,None).await.unwrap();
    assert_eq!(b.queued_count().unwrap(),3); assert_eq!(b.harness.sent.len(),1);
    assert_eq!(b.last_queued().unwrap().unwrap()["queue_reason"],"Compaction in progress");
    let records=b.harness.fallbacks.len(); b.flush_queued().await.unwrap(); assert_eq!(b.harness.fallbacks.len(),records);
    b.harness.blocked=false; b.flush_queued().await.unwrap();
    assert_eq!(b.queued_count().unwrap(),0);
    assert_eq!(b.harness.sent.iter().map(|w|w["uuid"].as_str().unwrap()).collect::<Vec<_>>(),vec!["A","B","T","D"]);
    assert!(b.harness.sent.iter().all(|w|w["priority"]=="now"));
    assert_eq!(b.harness.sent[1]["observation_ids"],json!(["event-2","event-3"]));
    assert_eq!(b.harness.sent[2]["observation_ids"],json!([]));
    assert_eq!(b.harness.sent[3]["observation_ids"],json!(["event-4"]));
    assert!(b.wake_keyed(Some("C"),"duplicate",&[],false,None).await.unwrap().is_none());
    assert!(b.harness.fallbacks.iter().all(|reason|reason=="Compaction in progress"));
}
#[tokio::test]
async fn unsupported_active_harness_queues_even_urgent_input_with_durable_reason() {
    let dir=tempfile::tempdir().unwrap(); let path=dir.path().join("spool.sqlite");
    let mut b=Bridge::open(SteerSink {sent:vec![],blocked:false,steering:false,fallbacks:vec![]},&path).unwrap();
    b.wake("active",&[],false).await.unwrap();
    assert!(b.send_text(Some("typed"),"urgent",true,None).await.unwrap().is_none());
    assert_eq!(b.queued_count().unwrap(),1);
    assert_eq!(b.harness.fallbacks,vec!["Harness does not support steering an active turn"]);
    let db=rusqlite::Connection::open(path).unwrap();
    let reason:String=db.query_row("SELECT json_extract(wire,'$.queue_reason') FROM deliveries WHERE id='typed'",[],|r|r.get(0)).unwrap();
    assert_eq!(reason,b.harness.fallbacks[0]);
}

#[tokio::test]
async fn appended_observations_are_referenced_once_across_wakes_restart_and_identical_real_turns() {
    let dir=tempfile::tempdir().unwrap();let path=dir.path().join("spool.sqlite");
    let mut b=Bridge::dry_run(&path).unwrap();
    b.observe(observation(1)).await.unwrap();b.observe(observation(1)).await.unwrap();
    // A real second turn with the same words, a minute later (within 3 s it would be one utterance heard on two
    // microphones: night pass 3).
    let mut repeated=observation(2);repeated.text=observation(1).text;repeated.timestamp_ms=agent::now_ms()+60_000;b.observe(repeated).await.unwrap();
    let mut context=b.harness.log.clone();drop(b);
    let mut b=Bridge::dry_run(&path).unwrap();
    let id=b.wake("respond",&[Attachment{media_type:"image/png".into(),bytes:vec![1,2,3]}],false).await.unwrap().unwrap();
    let wire=&b.harness.log[0]["would_send"];
    assert_eq!(wire["observation_ids"],json!(["event-1","event-2"]));
    assert!(wire["message"]["content"][0]["text"].as_str().unwrap().contains("observations already in context"));
    assert_eq!(wire["message"]["content"].as_array().unwrap().len(),2);
    context.extend(b.harness.log.clone());
    let supplied=context.iter().map(|v|v["would_send"]["message"]["content"][0]["text"].as_str().unwrap()).collect::<Vec<_>>().join("\n");
    assert_eq!(supplied.matches(&observation(1).text).count(),2,"two distinct turns, each once; duplicate id is ignored");
    let mut view=agent::view::LiveView::default();
    let shown=context.iter().flat_map(|v|view.sent(&v["would_send"],1)).map(|e|e.text).collect::<Vec<_>>().join("\n");
    assert_eq!(shown.matches(&observation(1).text).count(),2);
    b.handle(&Event::Complete{id:Some(id),query:true,error:false,text:"done".into()}).unwrap();
    assert_eq!(b.pending_observations().unwrap(),0);
    b.wake("again",&[],false).await.unwrap();
    assert_eq!(b.harness.log.last().unwrap()["would_send"]["observation_ids"],json!([]));
}
#[tokio::test]
async fn uncertain_append_blocks_replay_until_reconciled_and_unsupported_harness_keeps_speech() {
    let dir=tempfile::tempdir().unwrap();let path=dir.path().join("spool.sqlite");
    let mut b=Bridge::dry_run(&path).unwrap();b.observe_local(&observation(1)).unwrap();drop(b);
    let db=rusqlite::Connection::open(&path).unwrap();
    let mut wire=message("append",&format!("Observation:\n{}",observation(1).labelled()),&[],false,false).unwrap();wire["observation_ids"]=json!(["event-1"]);
    db.execute("INSERT INTO deliveries VALUES('append',?1,0,0,'submitted')",[wire.to_string()]).unwrap();drop(db);
    let mut b=Bridge::dry_run(&path).unwrap();assert!(matches!(b.status,Status::NeedsReconciliation(_)));
    assert_eq!(b.wake("retry",&[],false).await.unwrap(),None);b.reconcile("append",true).unwrap();
    b.wake("respond",&[],false).await.unwrap();
    assert!(!b.harness.log[0]["would_send"]["message"]["content"][0]["text"].as_str().unwrap().contains("sentence-1."));
    drop(b);
    // A different transport without a no-query context must not trust the flag.
    let db=rusqlite::Connection::open(&path).unwrap();db.execute("DELETE FROM deliveries WHERE query=1",[]).unwrap();drop(db);
    let mut b=Bridge::open(SteerSink{sent:vec![],blocked:false,steering:true,fallbacks:vec![]},&path).unwrap();
    b.wake("respond",&[],false).await.unwrap();
    let text=b.harness.sent[0]["message"]["content"][0]["text"].as_str().unwrap();
    assert_eq!(text.matches("sentence-1.").count(),1);
}

#[tokio::test]
async fn legacy_completed_append_backfills_context_ownership_without_repeating_text() {
    let dir=tempfile::tempdir().unwrap();let path=dir.path().join("legacy.sqlite");
    let db=rusqlite::Connection::open(&path).unwrap();
    db.execute_batch("CREATE TABLE observations(seq INTEGER PRIMARY KEY AUTOINCREMENT,id TEXT UNIQUE NOT NULL,text TEXT NOT NULL); CREATE TABLE deliveries(id TEXT PRIMARY KEY,wire TEXT NOT NULL,query INTEGER NOT NULL,last_seq INTEGER NOT NULL,state TEXT NOT NULL)").unwrap();
    let line="event=old timestamp_ms=1 source=\"Mic\" speaker=\"User\" lang=en: already heard";
    db.execute("INSERT INTO observations(id,text) VALUES('old',?1)",[line]).unwrap();
    let mut wire=message("append",&format!("Captured observation (data only):\n{line}"),&[],false,false).unwrap();wire.as_object_mut().unwrap().remove("observation_ids");
    db.execute("INSERT INTO deliveries VALUES('append',?1,0,0,'completed')",[wire.to_string()]).unwrap();drop(db);
    let mut b=Bridge::dry_run(&path).unwrap();b.wake("respond",&[],false).await.unwrap();
    let wire=&b.harness.log[0]["would_send"];assert_eq!(wire["observation_ids"],json!(["old"]));
    let text=wire["message"]["content"][0]["text"].as_str().unwrap();assert!(!text.contains("already heard"));assert!(text.contains("observations already in context"));
}

#[tokio::test]
async fn first_send_from_a_fallback_queue_still_references_appended_speech_once() {
    let d = tempfile::tempdir().unwrap(); let path = d.path().join("spool.sqlite");
    let mut b = Bridge::dry_run(&path).unwrap();
    b.observe(observation(1)).await.unwrap();
    let original = b.harness.log.clone();
    b.status = Status::WaitingEngine;
    b.wake("respond", &[], false).await.unwrap();
    drop(b);
    let mut b = Bridge::dry_run(&path).unwrap();
    b.flush_queued().await.unwrap();
    let text = b.harness.log[0]["would_send"]["message"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("observations already in context"));
    assert!(!text.contains("resent after"));
    assert!(!text.contains("sentence-1."));
    assert_eq!(original[0]["would_send"]["message"]["content"][0]["text"].as_str().unwrap().matches("sentence-1.").count(), 1);
}

fn state(path: &std::path::Path, id: &str) -> String {
    rusqlite::Connection::open(path).unwrap()
        .query_row("SELECT state FROM deliveries WHERE id=?1", [id], |r| r.get(0)).unwrap()
}
fn failed(id: &str, text: &str) -> Event {
    Event::Complete { id: Some(id.into()), query: true, error: true, text: text.into() }
}

/// 18 Oct 13:51: a provider 400 (thinking signature after a mid-turn compaction) before any tool ran put the wake
/// into review and blocked every later wake until the user clicked. A failure with no effects is resent once by itself
/// in the same process; a second failure ends it, and its speech rides the next wake. Nothing waits for a click.
/// (Across a start it is never resent: `recovery_resolves_every_interrupted_wake_from_the_session_file`.)
const SIGNATURE_400: &str = "400 invalid_request_error: messages.3.content.0: Invalid `signature` in `thinking` block";
#[tokio::test]
async fn failed_turn_without_effects_retries_once_then_hands_speech_to_the_next_wake() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("spool.sqlite");
    let mut b = Bridge::dry_run(&path).unwrap();
    b.observe(observation(1)).await.unwrap();
    let id = b.wake("answer", &[], false).await.unwrap().unwrap();
    b.handle(&Event::Echo { id: id.clone() }).unwrap();
    b.handle(&failed(&id, SIGNATURE_400)).unwrap();
    assert!(!matches!(b.status, Status::NeedsReconciliation(_)), "{:?}", b.status);
    assert_eq!(b.take_resolutions(), vec![agent::Resolution { id: id.clone(), outcome: agent::Resolved::Retrying, reason: SIGNATURE_400.into() }]);
    let sent = b.harness.log.len();
    b.flush_queued().await.unwrap();
    assert_eq!(b.harness.log.len(), sent + 1, "one automatic resend");
    let text = b.harness.log.last().unwrap()["would_send"]["message"]["content"][0]["text"].as_str().unwrap().to_owned();
    assert!(text.contains("sentence-1.") && text.contains("resent after a delivery interruption"));
    assert_eq!(b.status, Status::Working);
    b.handle(&failed(&id, SIGNATURE_400)).unwrap();
    assert_eq!(b.status, Status::Idle);
    assert_eq!(state(&path, &id), "failed");
    assert_eq!(b.take_resolutions()[0].outcome, agent::Resolved::Failed);
    b.flush_queued().await.unwrap();
    assert_eq!(b.harness.log.len(), sent + 1, "never a second resend");
    // The unanswered speech is not lost: the next wake carries it.
    b.observe(observation(2)).await.unwrap();
    b.wake("answer", &[], false).await.unwrap().unwrap();
    let wire = &b.harness.log.last().unwrap()["would_send"];
    assert_eq!(wire["observation_ids"], json!(["event-1", "event-2"]), "{wire}");
}

/// E10.2 / S58: a provider 400 a resend cannot fix ("image exceeds 5 MB") fails the turn once, is never resent,
/// tells the user in words, and its speech rides the next wake once, without the rejected image.
#[tokio::test]
async fn provider_400_is_not_resent_and_its_words_ride_the_next_wake_without_the_image() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("spool.sqlite");
    let mut b = Bridge::dry_run(&path).unwrap();
    b.observe(observation(1)).await.unwrap();
    let image = Attachment { media_type: "image/png".into(), bytes: vec![9; 64] };
    let id = b.wake("look at this", std::slice::from_ref(&image), false).await.unwrap().unwrap();
    b.handle(&Event::Echo { id: id.clone() }).unwrap();
    let rejected = r#"400 {"type":"error","error":{"type":"invalid_request_error","message":"messages.3.content.1.image.source.base64: image exceeds 5 MB maximum"}}"#;
    b.handle(&failed(&id, rejected)).unwrap();
    assert_eq!(b.status, Status::Idle);
    assert_eq!(state(&path, &id), "failed");
    let resolutions = b.take_resolutions();
    assert_eq!(resolutions.len(), 1);
    assert_eq!(resolutions[0].outcome, agent::Resolved::Rejected);
    // No repair ran in this run (night review 21 Oct, minor): the island makes no claim that one did.
    assert_eq!(resolutions[0].reason, "Agent couldn't answer: the model refuses this conversation (the screenshot was too large for the model)");
    let sent = b.harness.log.len();
    b.flush_queued().await.unwrap();
    assert_eq!(b.harness.log.len(), sent, "never resent");
    b.observe(observation(2)).await.unwrap();
    b.wake("answer", &[], false).await.unwrap().unwrap();
    let wire = &b.harness.log.last().unwrap()["would_send"];
    assert_eq!(wire["observation_ids"], json!(["event-1", "event-2"]), "{wire}");
    assert!(!wire["message"]["content"].as_array().unwrap().iter().any(|b| b["type"] == "image"), "the rejected image does not ride along");
}

/// 18 Oct 13:50: the turn opened Finder, looked and drew, then failed. Its tools must never run twice; it settles
/// by itself as done (its tool calls and results are in the conversation) and the agent takes the next wake.
#[tokio::test]
async fn failed_turn_after_tools_settles_without_replay_or_review() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("spool.sqlite");
    let mut b = Bridge::dry_run(&path).unwrap();
    b.observe(observation(1)).await.unwrap();
    let id = b.wake("open Finder and circle it", &[], false).await.unwrap().unwrap();
    b.handle(&Event::Other(json!({"type":"tool_execution_start","toolCallId":"bash-1","toolName":"bash"}))).unwrap();
    b.handle(&failed(&id, "400 thinking signature")).unwrap();
    assert_eq!(b.status, Status::Idle);
    assert_eq!(state(&path, &id), "completed");
    assert_eq!(b.pending_observations().unwrap(), 0, "its speech is settled, never re-sent");
    assert_eq!(b.take_resolutions()[0].outcome, agent::Resolved::SettledAfterError);
    let sent = b.harness.log.len();
    b.flush_queued().await.unwrap();
    assert_eq!(b.harness.log.len(), sent, "no replay");
    b.observe(observation(2)).await.unwrap();
    assert!(b.wake("next", &[], false).await.unwrap().is_some(), "the next wake is delivered, not refused");
}

/// Two steered wakes shared one tool cycle and the reply failed: neither may be replayed, neither needs a click.
#[tokio::test]
async fn ambiguous_effects_settle_both_wakes_without_replay() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("spool.sqlite");
    let mut b = Bridge::dry_run(&path).unwrap();
    let first = b.wake("speak first", &[], false).await.unwrap().unwrap();
    let second = b.wake("steer", &[], true).await.unwrap().unwrap();
    b.handle(&Event::Other(json!({"type":"tool_execution_start","toolCallId":"effect","toolName":"speak"}))).unwrap();
    for _ in 0..2 { b.handle(&Event::Complete { id: None, query: true, error: true, text: "interrupted".into() }).unwrap(); }
    assert_eq!(b.status, Status::Idle);
    for id in [&first, &second] { assert_eq!(state(&path, id), "completed"); }
    let sent = b.harness.log.len();
    b.flush_queued().await.unwrap();
    assert_eq!(b.harness.log.len(), sent);
}

/// A wake while a delivery is still uncertain (between a crash and the session check) waits in the queue with a
/// reason instead of failing, so the supervisor never restarts Pi over it (13:50:15 and 13:50:21).
#[tokio::test]
async fn wake_during_uncertainty_queues_and_flushes_after_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("spool.sqlite");
    let session = dir.path().join("session.jsonl");
    let mut b = Bridge::dry_run(&path).unwrap();
    b.observe(observation(1)).await.unwrap();
    let id = b.wake("look", &[], false).await.unwrap().unwrap();
    drop(b);
    let mut b = Bridge::dry_run(&path).unwrap();
    assert!(matches!(b.status, Status::NeedsReconciliation(_)));
    b.observe(observation(2)).await.unwrap();
    assert_eq!(b.wake("again", &[], false).await.unwrap(), None, "queued, not an error");
    assert_eq!(b.send_text(Some("notice-1"), "[Heimdall notice: x]", false, None).await.unwrap(), None);
    let sent = b.harness.log.len();
    // The interrupted turn ran a tool before the crash: settle it, then the queue drains in order.
    std::fs::write(&session, format!("{}\n", json!({"type":"custom","customType":"heimdall_effects","data":{"ids":[id],"toolCallId":"t1"}}))).unwrap();
    b.recover_receipts(&session).unwrap();
    assert_eq!(state(&path, &id), "completed");
    b.flush_queued().await.unwrap();
    assert_eq!(b.harness.log.len(), sent + 2);
    let text = b.harness.log[sent]["would_send"]["message"]["content"][0]["text"].as_str().unwrap().to_owned();
    assert!(text.contains("sentence-2.") && !text.contains("sentence-1."), "{text}");
}

/// After a crash or app restart the session file decides: a successful receipt completes; a tool that started
/// settles without replay. 13:35: a wake left from 17 Oct (tools ran, no receipt) blocked the agent until the user
/// clicked. With no sign of any tool the wake is still not resent across a start (E1.14, E4.4, S17 rewrite): it
/// ends, and its words become the start's history.
#[tokio::test]
async fn recovery_resolves_every_interrupted_wake_from_the_session_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("spool.sqlite");
    let session = dir.path().join("session.jsonl");
    let mut b = Bridge::dry_run(&path).unwrap();
    let mut ids = vec![];
    for n in 1..=4 {
        b.observe(observation(n)).await.unwrap();
        ids.push(b.wake(&format!("wake {n}"), &[], true).await.unwrap().unwrap());
    }
    drop(b);
    let lines = [
        json!({"type":"custom","customType":"heimdall_receipt","data":{"id":ids[0],"completed":true,"effects":false}}),
        json!({"type":"custom","customType":"heimdall_effects","data":{"ids":[ids[1]],"toolCallId":"t1"}}),
        json!({"type":"custom","customType":"heimdall_receipt","data":{"id":ids[2],"completed":false,"effects":true}}),
    ];
    std::fs::write(&session, lines.iter().map(|l| l.to_string() + "\n").collect::<String>()).unwrap();
    let mut b = Bridge::dry_run(&path).unwrap();
    b.recover_receipts(&session).unwrap();
    assert!(!matches!(b.status, Status::NeedsReconciliation(_)), "{:?}", b.status);
    assert_eq!(state(&path, &ids[0]), "completed");
    assert_eq!(state(&path, &ids[1]), "completed");
    assert_eq!(state(&path, &ids[2]), "completed");
    assert_eq!(state(&path, &ids[3]), "failed");
    let resolved = b.take_resolutions();
    assert_eq!(resolved.iter().filter(|r| r.outcome == agent::Resolved::Failed).map(|r| r.id.clone()).collect::<Vec<_>>(), vec![ids[3].clone()]);
    let sent = b.harness.log.len();
    b.flush_queued().await.unwrap();
    assert_eq!(b.harness.log.len(), sent, "nothing is resent across a start");
    b.begin_start(None).unwrap();
    let history: Vec<_> = b.start_leftovers().iter().map(|l| l.text.clone()).collect();
    assert_eq!(history.len(), 1, "{history:?}");
    assert!(history[0].contains("sentence-4."), "its words are history now: {history:?}");
    // Settled only once the start context is in the conversation (E4.4): until then the speech is still pending.
    let mut context = agent::notice::StartContext::new(0, None);
    assert!(b.deliver_start_context(&mut context, |_| u64::MAX).await.unwrap());
    assert_eq!(b.pending_observations().unwrap(), 0, "and never ride a wake");
}
