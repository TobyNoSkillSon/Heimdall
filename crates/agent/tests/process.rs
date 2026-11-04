//! Scripted Pi RPC processes only. Never invokes a provider.
use agent::{
    harness::{Harness, Pi, PiConfig},
    protocol::{message, Event},
    Bridge, Status,
};
use serde_json::json;
use std::{path::PathBuf, time::Duration};
fn config(dir: &std::path::Path) -> PiConfig {
    PiConfig::fixture(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mock-adapter.py"),
        dir.join("session.jsonl"),
        dir.to_owned(),
    )
}
async fn complete(h: &mut Pi) -> Event {
    loop {
        let e = tokio::time::timeout(Duration::from_secs(3), h.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        if matches!(e, Event::Complete { .. } | Event::Deferred { .. }) {
            return e;
        }
    }
}
#[tokio::test]
async fn persistent_child_prompts_images_steering_and_same_session_restart() {
    let d = tempfile::tempdir().unwrap();
    let c = config(d.path());
    let mut h = Pi::launch_config(c.clone()).unwrap();
    let pid = h.pid();
    h.send(
        message(
            "one",
            "first",
            &[agent::protocol::Attachment {
                media_type: "image/png".into(),
                bytes: vec![1, 2, 3],
            }],
            true,
            true,
        )
        .unwrap(),
    )
    .await
    .unwrap();
    loop {
        let e = h.next().await.unwrap().unwrap();
        if let Event::Other(v) = e {
            if v["type"] == "mock_pi" {
                assert_eq!(v["request"]["images"][0]["data"], "AQID");
                assert_eq!(v["request"]["streamingBehavior"], "steer");
                assert!(v["request"]["message"]
                    .as_str()
                    .unwrap()
                    .contains("[Heimdall delivery: one]"));
                // Night pass 20→21 Oct (P1): the local date and time at sending, right under the marker.
                let lines: Vec<&str> = v["request"]["message"].as_str().unwrap().splitn(3, '\n').collect();
                assert_eq!(lines[0], "[Heimdall delivery: one]");
                assert!(lines[1].starts_with(agent::clock::NOW_PREFIX) && lines[1].ends_with(']'), "{lines:?}");
                let today = chrono::Local::now().format("%-d %b %Y").to_string();
                assert!(lines[1].contains(&today), "{lines:?} lacks {today}");
                assert!(lines[2].contains("first"), "{lines:?}");
                break;
            }
        }
    }
    assert!(
        matches!(complete(&mut h).await,Event::Complete{id:Some(id),error:false,..}if id=="one")
    );
    assert_eq!(h.pid(), pid);
    h.shutdown().await.unwrap();
    drop(h);
    let mut h = Pi::launch_config(c).unwrap();
    assert_ne!(h.pid(), pid);
    h.send(message("two", "follow", &[], true, false).unwrap())
        .await
        .unwrap();
    assert!(
        matches!(complete(&mut h).await,Event::Complete{id:Some(id),error:false,..}if id=="two")
    );
    h.shutdown().await.unwrap();
}
#[tokio::test]
async fn low_level_agent_end_never_completes_a_delivery() {
    let d = tempfile::tempdir().unwrap();
    let mut h = Pi::launch_config(config(d.path())).unwrap();
    h.send(message("one", "error", &[], true, false).unwrap())
        .await
        .unwrap();
    let mut ended = false;
    loop {
        let e = h.next().await.unwrap().unwrap();
        match e {
            Event::Other(v) if v["type"] == "agent_end" => ended = true,
            Event::Complete { id, error, .. } => {
                assert!(ended && error);
                assert_eq!(id.as_deref(), Some("one"));
                break;
            }
            _ => {}
        }
    }
    h.shutdown().await.unwrap();
}
#[tokio::test]
async fn crash_and_garbage_require_reconciliation_without_replay() {
    for scenario in ["[fixture:crash]", "[fixture:garbage]"] {
        let d = tempfile::tempdir().unwrap();
        let mut b = Bridge::open(
            Pi::launch_config(config(d.path())).unwrap(),
            d.path().join("spool"),
        )
        .unwrap();
        b.wake(scenario, &[], false).await.unwrap();
        loop {
            match tokio::time::timeout(Duration::from_secs(3), b.next())
                .await
                .unwrap()
            {
                Ok(Some(_)) => {}
                _ => break,
            }
        }
        assert!(matches!(b.status, Status::NeedsReconciliation(_)));
        // Queued behind the uncertain delivery (the restart's session check decides it), never replayed now.
        assert_eq!(b.wake("repeat", &[], false).await.unwrap(), None);
        let _ = b.shutdown().await;
    }
}
#[tokio::test]
async fn active_hang_has_a_deadline_idle_has_none() {
    let d = tempfile::tempdir().unwrap();
    let mut c = config(d.path());
    c.inactivity = Duration::from_millis(100);
    let mut h = Pi::launch_config(c).unwrap();
    assert!(tokio::time::timeout(Duration::from_millis(150), h.next())
        .await
        .is_err());
    let began = tokio::time::Instant::now();
    h.send(message("hang", "[fixture:hang]", &[], true, false).unwrap())
        .await
        .unwrap();
    // The child may still be starting when the 100 ms active deadline expires.
    // Its diagnostic frame is not progress and is not required before the timeout.
    let error = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match h.next().await {
                Ok(Some(Event::Other(v))) => assert_eq!(v["type"], "mock_pi"),
                Err(error) => break error,
                event => panic!("hung fixture unexpectedly settled: {event:?}"),
            }
        }
    }).await.expect("active hang had no deadline");
    assert!(error.to_string().contains("no progress"), "{error:#}");
    assert!(began.elapsed() >= Duration::from_millis(100));
    h.shutdown().await.unwrap();
}
#[tokio::test]
async fn slow_output_is_progress() {
    let d = tempfile::tempdir().unwrap();
    let mut c = config(d.path());
    c.inactivity = Duration::from_millis(100);
    let mut h = Pi::launch_config(c).unwrap();
    h.state().await.unwrap();
    h.send(message("slow", "[fixture:slow]", &[], true, false).unwrap())
        .await
        .unwrap();
    assert!(matches!(
        complete(&mut h).await,
        Event::Complete { error: false, .. }
    ));
    h.shutdown().await.unwrap();
}
#[tokio::test]
async fn engine_503_requeues_only_without_tool_effects() {
    for (scenario, effects) in [
        ("[fixture:503]", false),
        ("[fixture:drop]", false),
        ("[fixture:tool-error]", true),
    ] {
        let d = tempfile::tempdir().unwrap();
        let mut b = Bridge::open(
            Pi::launch_config(config(d.path())).unwrap(),
            d.path().join("spool"),
        )
        .unwrap();
        b.wake(scenario, &[], false).await.unwrap();
        until_deferred(&mut b).await.unwrap();
        // Without effects the wake waits for the engine; after a tool it settles without replay. Neither needs review.
        assert!(!matches!(b.status, Status::NeedsReconciliation(_)));
        if effects {
            assert_eq!(b.take_resolutions().iter().map(|r| r.outcome).collect::<Vec<_>>(), vec![agent::Resolved::SettledAfterError]);
            assert_eq!(b.queued_count().unwrap(), 0);
        } else {
            assert_eq!(b.status, Status::WaitingEngine);
            assert_eq!(b.queued_count().unwrap(), 1);
        }
        let _ = b.shutdown().await;
    }
}
#[tokio::test]
async fn compaction_events_do_not_complete_turn_early() {
    let d = tempfile::tempdir().unwrap();
    let mut h = Pi::launch_config(config(d.path())).unwrap();
    h.send(message("c", "[fixture:compaction]", &[], true, false).unwrap())
        .await
        .unwrap();
    let mut compacted = false;
    loop {
        match h.next().await.unwrap().unwrap() {
            Event::Other(v) if v["type"] == "compaction_start" => assert!(h.compacting()),
            Event::Other(v) if v["type"] == "compaction_end" => {assert!(!h.compacting());compacted = true;},
            Event::Complete { error: false, .. } => {
                assert!(compacted);
                break;
            }
            _ => {}
        }
    }
    h.shutdown().await.unwrap();
}
#[tokio::test]
async fn state_entries_fork_and_stop_use_real_rpc_commands() {
    let d = tempfile::tempdir().unwrap();
    let mut h = Pi::launch_config(config(d.path())).unwrap();
    assert_eq!(h.state().await.unwrap()["model"]["id"], "qwen3.8-27b");
    h.send(message("one", "[Heimdall typed message]\nhello", &[], true, false).unwrap())
        .await
        .unwrap();
    complete(&mut h).await;
    let entries = h.entries(None).await.unwrap();
    assert_eq!(entries["entries"][0]["id"], "one");
    assert!(h.fork("absent").await.is_err());
    assert_eq!(h.fork("one").await.unwrap(), d.path().join("forked.jsonl"));
    assert!(h.incremental_entries().await.unwrap()["entries"].as_array().unwrap().is_empty(),"fork rewound incremental history");
    h.abort().await.unwrap();
    h.shutdown().await.unwrap();
}
#[tokio::test]
async fn session_lock_blocks_duplicate_process_and_releases_after_shutdown() {
    let d = tempfile::tempdir().unwrap();
    let c = config(d.path());
    let mut h = Pi::launch_config(c.clone()).unwrap();
    assert!(Pi::launch_config(c.clone()).is_err());
    h.shutdown().await.unwrap();
    let mut next = Pi::launch_config(c).unwrap();
    next.shutdown().await.unwrap();
}
#[tokio::test]
async fn off_on_during_recording_cannot_adopt_a_new_epoch() {
    use std::sync::{atomic::Ordering, Arc};
    let d = tempfile::tempdir().unwrap();
    let mut h = Pi::launch_config(config(d.path())).unwrap();
    let gate = Arc::new(agent::delivery::DeliveryGate::new(true));
    h.set_delivery_gate(gate.clone());
    let revoke = gate.clone();
    h.set_transport_recorder(Arc::new(move |direction, _| {
        if direction == "send" {
            revoke.store(false, Ordering::Release);
            revoke.store(true, Ordering::Release);
        }
        Ok(())
    }));
    let wrong = Arc::new(agent::delivery::DeliveryGate::new(true));
    assert!(h
        .send_with_permit(
            message("wrong", "must not send", &[], true, false).unwrap(),
            wrong.permit().unwrap()
        )
        .await
        .unwrap_err()
        .to_string()
        .contains("another gate"));
    let e = h
        .send(message("original", "private", &[], true, false).unwrap())
        .await
        .unwrap_err();
    assert_eq!(
        e.downcast_ref::<agent::delivery::DeliveryBlocked>()
            .unwrap()
            .written,
        0
    );
    h.shutdown().await.unwrap();
}
#[test]
fn inherited_environment_does_not_enable_cloud_or_global_resources() {
    let d = tempfile::tempdir().unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let mut h = Pi::launch_config(config(d.path())).unwrap();
        h.send(message("one", "hi", &[], true, false).unwrap())
            .await
            .unwrap();
        loop {
            if let Event::Other(v) = h.next().await.unwrap().unwrap() {
                if v["type"] == "mock_pi" {
                    assert!(v["args"]
                        .as_array()
                        .unwrap()
                        .contains(&json!("--no-approve")));
                    assert!(v["args"].as_array().unwrap().contains(&json!("--offline")));
                    assert!(v["agent_dir"]
                        .as_str()
                        .unwrap()
                        .starts_with(d.path().to_str().unwrap()));
                    break;
                }
            }
        }
        h.shutdown().await.unwrap();
    });
}

#[tokio::test]
async fn unknown_frames_and_invalid_tool_events_cannot_extend_hang_deadline() {
    let d=tempfile::tempdir().unwrap();let mut c=config(d.path());c.inactivity=Duration::from_millis(70);
    let mut h=Pi::launch_config(c).unwrap();h.send(message("one","[fixture:json-flood]",&[],true,false).unwrap()).await.unwrap();
    let start=std::time::Instant::now();
    loop {match h.next().await {Err(e)=>{assert!(e.to_string().contains("no progress"));break;},Ok(Some(_))=>{},Ok(None)=>panic!("Pi EOF before hang detection")}}
    assert!(start.elapsed()<Duration::from_secs(1));h.shutdown().await.unwrap();
}
#[tokio::test]
async fn session_entries_are_incremental_and_empty_settlements_do_not_repeat_history() {
    let d=tempfile::tempdir().unwrap();let mut h=Pi::launch_config(config(d.path())).unwrap();
    h.send(message("one","first",&[],true,false).unwrap()).await.unwrap();complete(&mut h).await;
    assert_eq!(h.incremental_entries().await.unwrap()["entries"].as_array().unwrap().len(),1);
    assert!(h.incremental_entries().await.unwrap()["entries"].as_array().unwrap().is_empty());
    h.send(message("two","second",&[],true,false).unwrap()).await.unwrap();complete(&mut h).await;
    assert_eq!(h.incremental_entries().await.unwrap()["entries"][0]["id"],"two");h.shutdown().await.unwrap();
}

#[tokio::test]
async fn identical_compaction_start_frames_cannot_keep_60ms_deadline_alive_300ms() {
    // A running compaction has its own silence limit (`compaction_inactivity`); repeated start frames must not refresh it.
    let d=tempfile::tempdir().unwrap();let mut c=config(d.path());c.inactivity=Duration::from_millis(60);c.compaction_inactivity=Duration::from_millis(60);
    let mut h=Pi::launch_config(c).unwrap();h.state().await.unwrap();std::fs::write(d.path().join("compaction-flood"),b"").unwrap();
    h.compact().await.unwrap();let began=std::time::Instant::now();
    let result=tokio::time::timeout(Duration::from_millis(300),async {
        loop {match h.next().await {Err(e)=>break e,Ok(Some(_))=>{},Ok(None)=>panic!("Pi exited before hang detection")}}
    }).await;
    h.shutdown().await.unwrap();
    assert!(result.unwrap_or_else(|_|panic!("duplicate compaction frames defeated hang deadline")).to_string().contains("no progress"));
    assert!(began.elapsed()<Duration::from_secs(1));
}

#[test]
fn loop_guard_extension_hooks() {
    // The shipped extension module is exercised directly with Pi-shaped events, no provider.
    let mut command = std::process::Command::new("node");
    command.arg("--test").arg(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/loop-guard.test.mjs"));
    agent::runtime::checked(&mut command, Duration::from_secs(20)).unwrap();
}

async fn loop_events(h: &mut Pi, stop_at_block: bool) -> (Vec<serde_json::Value>, Option<Event>) {
    let mut events = vec![];
    loop {
        let e = tokio::time::timeout(Duration::from_secs(5), h.next()).await.unwrap().unwrap().unwrap();
        match e {
            Event::Other(v) => {
                let stop = stop_at_block && v["type"] == "mock_tool_attempt" && v["ran"] == false;
                events.push(v);
                if stop { return (events, None); }
            }
            Event::Complete { .. } => return (events, Some(e)),
            _ => {}
        }
    }
}
#[tokio::test]
async fn loop_guard_changing_poll_results_and_intervening_tools_never_block() {
    for scenario in ["[fixture:loop-poll]", "[fixture:loop-interrupted]"] {
        let d = tempfile::tempdir().unwrap();
        let mut h = Pi::launch_config(config(d.path())).unwrap();
        h.send(message("one", scenario, &[], true, false).unwrap()).await.unwrap();
        let (events, complete) = loop_events(&mut h, false).await;
        assert!(matches!(complete, Some(Event::Complete { error: false, .. })));
        let attempts: Vec<_> = events.iter().filter(|v| v["type"] == "mock_tool_attempt").collect();
        assert!(!attempts.is_empty());
        assert!(attempts.iter().all(|v| v["ran"] == true));
        assert!(!events.iter().any(|v| v["type"] == "mock_control"));
        h.shutdown().await.unwrap();
    }
}
#[tokio::test]
async fn loop_guard_sixth_call_blocks_and_counters_reset_each_user_turn() {
    let d = tempfile::tempdir().unwrap();
    let mut h = Pi::launch_config(config(d.path())).unwrap();
    for id in ["one", "two", "three"] {
        h.send(message(id, "[fixture:loop-block]", &[], true, false).unwrap()).await.unwrap();
        let (events, complete) = loop_events(&mut h, false).await;
        assert!(matches!(complete, Some(Event::Complete { error: false, .. })));
        let attempts: Vec<_> = events.iter().filter(|v| v["type"] == "mock_tool_attempt").collect();
        assert_eq!(attempts.len(), 6);
        assert!(attempts[..5].iter().all(|v| v["ran"] == true));
        assert_eq!(attempts[5]["ran"], false);
        assert!(attempts[5]["decision"]["reason"].as_str().unwrap().contains("same arguments 5 times and got the same result"));
        assert!(!events.iter().any(|v| v["type"] == "mock_control"));
    }
    h.shutdown().await.unwrap();
}
#[tokio::test]
async fn loop_guard_three_blocked_attempts_abort_once_and_preserve_session() {
    let d = tempfile::tempdir().unwrap();
    let mut h = Pi::launch_config(config(d.path())).unwrap();
    let pid = h.pid();
    h.send(message("one", "[fixture:loop-abort]", &[], true, false).unwrap()).await.unwrap();
    let (mut events, complete) = loop_events(&mut h, false).await;
    assert!(matches!(complete, Some(Event::Complete { error: true, ref text, .. }) if text == "Agent stopped a repeating loop"));
    loop {
        if let Event::Other(v) = tokio::time::timeout(Duration::from_secs(3), h.next()).await.unwrap().unwrap().unwrap() {
            let stopped = v["type"] == "heimdall_loop_stopped";
            events.push(v);
            if stopped { break; }
        }
    }
    let attempts: Vec<_> = events.iter().filter(|v| v["type"] == "mock_tool_attempt").collect();
    assert_eq!(attempts.len(), 8);
    assert!(attempts[..5].iter().all(|v| v["ran"] == true));
    assert!(attempts[5..].iter().all(|v| v["ran"] == false));
    let controls: Vec<_> = events.iter().filter(|v| v["type"] == "mock_control").map(|v| v["command"].as_str().unwrap()).collect();
    assert_eq!(controls, ["clear_queue", "abort"]);
    let stopped: Vec<_> = events.iter().filter(|v| v["type"] == "heimdall_loop_stopped").collect();
    assert_eq!(stopped.len(), 1);
    assert_eq!(stopped[0]["tool"], "read");
    assert_eq!(stopped[0]["repeats"], 5);
    assert_eq!(stopped[0]["blocked_attempts"], 3);
    assert_eq!(stopped[0]["arguments_hash"].as_str().unwrap().len(), 64);
    assert_eq!(h.pid(), pid);
    assert!(!h.active());
    let mut live = agent::view::LiveView::default();
    let lines: Vec<_> = events.iter().flat_map(|v| live.received(v, 1)).filter(|e| e.kind == agent::view::StreamKind::Failure).collect();
    assert_eq!(lines.len(), 1);
    let entries = h.entries(None).await.unwrap();
    let history: Vec<_> = entries["entries"].as_array().unwrap().iter().flat_map(|v| live.history(v)).filter(|e| e.kind == agent::view::StreamKind::Failure).collect();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].id, lines[0].id);
    h.send(message("two", "[fixture:loop-block]", &[], true, false).unwrap()).await.unwrap();
    let (_, complete) = loop_events(&mut h, false).await;
    assert!(matches!(complete, Some(Event::Complete { error: false, .. })));
    h.shutdown().await.unwrap();
}
#[tokio::test]
async fn loop_guard_agent_off_retires_loop_and_reload_has_fresh_counters() {
    use std::sync::{atomic::Ordering, Arc};
    let d = tempfile::tempdir().unwrap();
    let c = config(d.path());
    let mut h = Pi::launch_config(c.clone()).unwrap();
    let gate = Arc::new(agent::delivery::DeliveryGate::new(true));
    h.set_delivery_gate(gate.clone());
    h.send(message("one", "[fixture:loop-hold]", &[], true, false).unwrap()).await.unwrap();
    let (events, _) = loop_events(&mut h, true).await;
    assert_eq!(events.iter().filter(|v| v["type"] == "mock_tool_attempt" && v["ran"] == false).count(), 1);
    gate.store(false, Ordering::Release);
    h.shutdown().await.unwrap();
    assert_eq!(h.pid(), None);
    drop(h);
    let mut replacement = Pi::launch_config(c).unwrap();
    replacement.send(message("two", "[fixture:loop-block]", &[], true, false).unwrap()).await.unwrap();
    let (events, complete) = loop_events(&mut replacement, false).await;
    assert!(matches!(complete, Some(Event::Complete { error: false, .. })));
    assert_eq!(events.iter().find(|v| v["entry"]["customType"] == "heimdall_loop_guard").unwrap()["entry"]["data"]["blocked_attempts"], 1);
    replacement.shutdown().await.unwrap();
}

#[tokio::test]
async fn loop_guard_abort_survives_cancellation_of_next_during_control() {
    let d = tempfile::tempdir().unwrap();
    let mut h = Pi::launch_config(config(d.path())).unwrap();
    std::fs::write(d.path().join("hold-clear-queue"), b"").unwrap();
    h.send(message("one", "[fixture:loop-abort]", &[], true, false).unwrap()).await.unwrap();
    // Model the host selecting a UI command while next() is waiting on the clear-queue RPC.
    let consumed = tokio::time::timeout(Duration::from_secs(2), async {
        loop { h.next().await.unwrap().unwrap(); }
    }).await;
    assert!(consumed.is_err());
    assert!(d.path().join("clearing-queue").exists());
    std::fs::remove_file(d.path().join("hold-clear-queue")).unwrap();
    let (mut events, complete) = loop_events(&mut h, false).await;
    assert!(matches!(complete, Some(Event::Complete { error: true, ref text, .. }) if text == "Agent stopped a repeating loop"));
    loop {
        if let Event::Other(v) = tokio::time::timeout(Duration::from_secs(3), h.next()).await.unwrap().unwrap().unwrap() {
            let stopped = v["type"] == "heimdall_loop_stopped";
            events.push(v);
            if stopped { break; }
        }
    }
    assert_eq!(events.iter().filter(|v| v["type"] == "heimdall_loop_stopped").count(), 1);
    assert_eq!(events.iter().filter(|v| v["type"] == "mock_control" && v["command"] == "abort").count(), 1);
    h.send(message("two", "[fixture:loop-block]", &[], true, false).unwrap()).await.unwrap();
    assert!(matches!(loop_events(&mut h, false).await.1, Some(Event::Complete { error: false, .. })));
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn closed_stdout_stderr_and_guardian_socket_finish_without_eof_spin() {
    let d=tempfile::tempdir().unwrap();
    let mut h=Pi::launch_config(PiConfig::fixture("/usr/bin/true".into(),d.path().join("session"),d.path().into())).unwrap();
    for _ in 0..3 {
        assert!(tokio::time::timeout(Duration::from_secs(2),h.next()).await.unwrap().unwrap().is_none());
    }
    h.shutdown().await.unwrap();
    let mut c=std::process::Command::new("/usr/bin/true");
    assert_eq!(agent::runtime::checked(&mut c,Duration::from_secs(2)).unwrap(),"");
}
#[tokio::test]
async fn retired_process_controls_cannot_adopt_new_on_epoch() {
    use std::sync::{Arc,atomic::Ordering};
    let d=tempfile::tempdir().unwrap();let gate=Arc::new(agent::delivery::DeliveryGate::new(true));
    let mut h=Pi::launch_config_permitted(config(d.path()),Some(gate.permit().unwrap())).unwrap();
    h.state().await.unwrap();
    gate.store(false,Ordering::Release);gate.store(true,Ordering::Release);
    assert!(h.state().await.is_err());
    h.shutdown().await.unwrap();
}

async fn until_deferred(b: &mut Bridge<Pi>) -> anyhow::Result<Event> {
    tokio::time::timeout(Duration::from_secs(3),async {
        loop {match b.next().await? {
            Some(e @ Event::Deferred{..})=>return Ok(e),
            Some(_)=>{},
            None=>anyhow::bail!("Pi EOF before engine deferred event"),
        }}
    }).await?
}
#[tokio::test]
async fn deferred_event_waiter_fails_promptly_on_a_closed_pipe() {
    let d=tempfile::tempdir().unwrap();
    let mut b=Bridge::open(Pi::launch_config(PiConfig::fixture("/usr/bin/true".into(),d.path().join("session"),d.path().into())).unwrap(),":memory:").unwrap();
    let result=tokio::time::timeout(Duration::from_secs(2),until_deferred(&mut b)).await.unwrap();
    assert!(result.unwrap_err().to_string().contains("EOF"));
    b.shutdown().await.unwrap();
}

#[tokio::test]
async fn pi_transport_rejects_a_persisted_epoch_from_an_older_process() {
    use std::sync::{Arc,atomic::Ordering};
    let d=tempfile::tempdir().unwrap();let gate=Arc::new(agent::delivery::DeliveryGate::new(true));let old=gate.generation();
    gate.store(false,Ordering::Release);gate.store(true,Ordering::Release);
    let mut h=Pi::launch_config_permitted(config(d.path()),Some(gate.permit().unwrap())).unwrap();
    let mut wire=message("old","stale",&[],true,false).unwrap();wire["delivery_epoch"]=json!(old);
    let e=h.send(wire).await.unwrap_err();assert_eq!(e.downcast_ref::<agent::delivery::DeliveryBlocked>().unwrap().written,0);
    h.state().await.unwrap();h.shutdown().await.unwrap();
}

// ce85c029: Pi appended image coordinate notes, ran screen_frames, then
// agent_end + a successful receipt. The old exact-text echo match never consumed it.
#[tokio::test]
async fn resized_image_tool_turn_and_typed_steer_settle_exactly_once() {
    let d = tempfile::tempdir().unwrap();
    let mut b = Bridge::open(Pi::launch_config(config(d.path())).unwrap(), ":memory:").unwrap();
    let first = b.wake("[fixture:loop-block]", &[agent::protocol::Attachment {
        media_type: "image/png".into(), bytes: vec![1,2,3],
    }], false).await.unwrap().unwrap();
    // Typed steering is deliberately independent of ambient wake coalescing.
    let steer = b.send_text(None, "typed steer", true, None).await.unwrap().unwrap();
    let mut completed = vec![];
    while completed.len() < 2 {
        let event = tokio::time::timeout(Duration::from_secs(5), b.next()).await.unwrap().unwrap().unwrap();
        if let Event::Complete { id: Some(id), error, .. } = event {
            assert!(!error); completed.push(id);
        }
    }
    assert_eq!(completed, vec![first, steer]);
    assert!(b.inflight().unwrap().is_empty());
    assert_eq!(b.status, Status::Idle);
    assert!(!b.harness.active());
    b.shutdown().await.unwrap();
}

#[tokio::test]
async fn missing_settlement_recovers_exact_receipt_within_bound_and_logs_it() {
    for scenario in ["[fixture:receipt-tool]", "[fixture:receipt-durable]"] {
        let d = tempfile::tempdir().unwrap();
        let mut b = Bridge::open(Pi::launch_config(config(d.path())).unwrap(), ":memory:").unwrap();
        let first = b.wake(scenario, &[agent::protocol::Attachment {
            media_type: "image/png".into(), bytes: vec![1,2,3],
        }], false).await.unwrap().unwrap();
        assert_eq!(b.queued_count().unwrap(), 0);
        let mut completions = vec![];
        let mut recovered = false;
        tokio::time::timeout(Duration::from_secs(5), async {
            while !recovered {
                match b.next().await.unwrap().unwrap() {
                    Event::Complete { id: Some(id), error, .. } => { assert!(!error); completions.push(id); }
                    Event::Other(v) if v["type"] == "heimdall_settlement_recovered" => {
                        assert_eq!(v["ids"], json!([first])); recovered = true;
                    }
                    _ => {}
                }
            }
        }).await.unwrap();
        assert_eq!(completions, vec![first]);
        assert!(b.inflight().unwrap().is_empty());
        assert!(!b.harness.active());
        b.flush_queued().await.unwrap();
        assert_eq!(b.queued_count().unwrap(), 0);
        assert_eq!(b.status, Status::Idle);
        b.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn wrong_or_failed_receipt_does_not_claim_success() {
    for scenario in ["[fixture:receipt-wrong]", "[fixture:receipt-failed]"] {
        let d = tempfile::tempdir().unwrap();
        let mut c = config(d.path()); c.inactivity = Duration::from_millis(2300);
        let mut h = Pi::launch_config(c).unwrap();
        h.send(message("one", scenario, &[], true, false).unwrap()).await.unwrap();
        loop {
            match tokio::time::timeout(Duration::from_secs(4), h.next()).await.unwrap() {
                Err(e) => { assert!(e.to_string().contains("no progress")); break; }
                Ok(Some(Event::Complete { .. })) => panic!("receipt did not prove this delivery completed"),
                Ok(Some(_)) => {}
                Ok(None) => panic!("unexpected EOF"),
            }
        }
        assert!(h.active()); h.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn final_text_is_a_single_settlement_event_for_normal_and_recovered_replies() {
    for scenario in ["ordinary", "[fixture:receipt-durable]"] {
        let d=tempfile::tempdir().unwrap();
        let mut h=Pi::launch_config(config(d.path())).unwrap();
        h.send(message("reply",scenario,&[],true,false).unwrap()).await.unwrap();
        let mut finals=vec![];
        loop {
            match tokio::time::timeout(Duration::from_secs(4),h.next()).await.unwrap().unwrap().unwrap() {
                Event::Other(v) if v["type"]=="heimdall_final_answer" => finals.push(v["text"].as_str().unwrap().to_owned()),
                Event::Complete {error,..} => {assert!(!error);break;}
                _ => {}
            }
        }
        assert_eq!(finals,["fixture reply"]);
        h.shutdown().await.unwrap();
    }
}

/// A run that settles without any assistant message still closes the turn's reply state: one empty aborted final.
#[tokio::test]
async fn a_run_without_a_reply_settles_with_an_empty_aborted_final() {
    let d=tempfile::tempdir().unwrap();
    let mut h=Pi::launch_config(config(d.path())).unwrap();
    h.send(message("reply","missing",&[],true,false).unwrap()).await.unwrap();
    let mut finals=vec![];
    loop {
        match tokio::time::timeout(Duration::from_secs(4),h.next()).await.unwrap().unwrap().unwrap() {
            Event::Other(v) if v["type"]=="heimdall_final_answer" => finals.push(v.clone()),
            Event::Complete {..} => break,
            _ => {}
        }
    }
    assert_eq!(finals,[json!({"type":"heimdall_final_answer","text":"","stop_reason":"aborted"})]);
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn pi_wake_keeps_observation_ids_outside_prompt_and_settles_exact_delivery() {
    let d = tempfile::tempdir().unwrap();
    let h = Pi::launch_config(config(d.path())).unwrap();
    let mut b = Bridge::open(h, d.path().join("spool.sqlite")).unwrap();
    b.observe(agent::protocol::Observation {
        id:"observation-one".into(), timestamp_ms:42, source:"Mic".into(), speaker:"User".into(), language:"en".into(),
        text:"Check this [screenshot 14:02:40 attached]".into(),
    }).await.unwrap();
    let id = b.wake("respond", &[agent::protocol::Attachment {media_type:"image/png".into(),bytes:vec![1,2,3]}], false).await.unwrap().unwrap();
    let mut seen = false;
    loop {
        let e = tokio::time::timeout(Duration::from_secs(3), b.next()).await.unwrap().unwrap().unwrap();
        match e {
            Event::Other(v) if v["type"] == "mock_pi" => {
                let request = &v["request"];
                assert_eq!(request["observation_ids"], json!(["observation-one"]));
                let text = request["message"].as_str().unwrap();
                assert!(text.contains("Transcript:\n[") && text.contains(" Mic · User] Check this [screenshot 14:02:40 attached]"), "{text}");
                assert!(!text.contains("observation-one") && !text.contains("timestamp_ms=") && !text.contains("lang="));
                assert_eq!(request["images"].as_array().unwrap().len(), 1);
                seen = true;
            }
            Event::Complete { id:Some(completed), error:false, .. } => { assert_eq!(completed, id); break; }
            _ => {}
        }
    }
    assert!(seen); assert_eq!(b.status, Status::Idle); assert!(b.inflight().unwrap().is_empty());
    b.harness.shutdown().await.unwrap();
}

/// Night pass H2: while the model still thinks (nothing acted or replied), the user's next words restart the turn: Pi
/// is asked to abort, the first delivery completes without an answer of its own, and the new words go out as a fresh
/// run with their image; once the turn has acted, words steer it (below).
#[tokio::test]
async fn pi_restarts_a_thinking_turn_with_owners_new_words() {
    let d=tempfile::tempdir().unwrap(); let mut b=Bridge::open(Pi::launch_config(config(d.path())).unwrap(),":memory:").unwrap();
    let observation=|n|agent::protocol::Observation {id:format!("speech-{n}"),timestamp_ms:n,source:"Mic".into(),speaker:"User".into(),language:"en".into(),text:format!("sentence {n}")};
    b.observe(observation(1)).await.unwrap(); let first=b.wake("[fixture:steering-thinking]",&[],false).await.unwrap().unwrap();
    loop {
        let e=tokio::time::timeout(Duration::from_secs(3),b.next()).await.unwrap().unwrap().unwrap();
        if let Event::Other(v)=e {if v["type"]=="message_update" {break;}}
    }
    assert!(b.harness.can_rejoin());
    b.observe(observation(2)).await.unwrap();
    let voice=b.wake("decider follow-up",&[agent::protocol::Attachment {media_type:"image/png".into(),bytes:vec![1,2,3]}],false).await.unwrap().unwrap();
    let mut completed=vec![]; let mut final_answers=vec![]; let mut aborts=0; let mut requests=vec![];
    tokio::time::timeout(Duration::from_secs(5),async {
        while completed.len()<2 {
            match b.next().await.unwrap().unwrap() {
                Event::Other(v) if v["type"]=="mock_control" && v["command"]=="abort" => aborts+=1,
                Event::Other(v) if v["type"]=="mock_pi" => requests.push(v["request"].clone()),
                Event::Other(v) if v["type"]=="heimdall_final_answer" => final_answers.push((v["text"].as_str().unwrap().to_owned(),v["stop_reason"].as_str().unwrap_or_default().to_owned())),
                Event::Other(v) if v["type"]=="heimdall_aborted" => panic!("a restart is no Stop: {v}"),
                Event::Complete {id:Some(id),error,..}=>{assert!(!error,"{id}");completed.push(id);}
                _=>{}
            }
        }
    }).await.unwrap();
    assert_eq!(aborts,1); assert_eq!(completed,vec![first,voice.clone()]);
    let restarted=requests.iter().find(|r|r["id"]==voice).unwrap();
    assert_eq!(restarted["observation_ids"],json!(["speech-2"]));assert_eq!(restarted["images"].as_array().unwrap().len(),1);
    // The thinking turn ended unanswered (nothing said); the restarted one answered once.
    assert_eq!(final_answers,[("".to_owned(),"aborted".to_owned()),("fixture reply".to_owned(),"stop".to_owned())]);
    assert!(b.inflight().unwrap().is_empty());assert_eq!(b.status,Status::Idle);
    assert!(b.take_resolutions().is_empty());
    b.shutdown().await.unwrap();
}
/// Night review 4: a reply or a tool call that started streaming is never cut by a restart, even when the bridge
/// has not read that event yet. The fixture writes the thinking delta and the first written token together; the
/// bridge reads only the thinking delta (so its own view still says "can restart"), then the user speaks: the harness
/// takes in what Pi already wrote, sees the turn writing, sends no abort, and his words steer the turn.
#[tokio::test]
async fn pi_never_restarts_a_turn_that_started_writing_even_before_the_bridge_read_it() {
    for phase in ["[fixture:steering-reply-streaming]", "[fixture:steering-toolcall-streaming]"] {
        let d=tempfile::tempdir().unwrap(); let mut b=Bridge::open(Pi::launch_config(config(d.path())).unwrap(),":memory:").unwrap();
        let observation=|n|agent::protocol::Observation {id:format!("speech-{n}"),timestamp_ms:n,source:"Mic".into(),speaker:"User".into(),language:"en".into(),text:format!("sentence {n}")};
        b.observe(observation(1)).await.unwrap(); let first=b.wake(phase,&[],false).await.unwrap().unwrap();
        loop {
            let e=tokio::time::timeout(Duration::from_secs(3),b.next()).await.unwrap().unwrap().unwrap();
            if let Event::Other(v)=e {if v["type"]=="message_update" {assert_eq!(v["assistantMessageEvent"]["type"],"thinking_delta");break;}}
        }
        // The written token is in the pipe; the bridge has not taken it in.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(b.harness.can_rejoin(), "{phase}: the bridge's own view is stale (the race this test is about)");
        b.observe(observation(2)).await.unwrap();
        let voice=b.wake("decider follow-up",&[],false).await.unwrap().unwrap();
        assert!(!b.harness.can_rejoin(), "{phase}: the harness saw the turn writing");
        let finish=b.send_text(None,"[fixture:steering-finish]",false,None).await.unwrap().unwrap();
        let mut completed=vec![]; let mut aborts=0; let mut requests=vec![];
        tokio::time::timeout(Duration::from_secs(5),async {
            while completed.len()<3 {
                match b.next().await.unwrap().unwrap() {
                    Event::Other(v) if v["type"]=="mock_control" && v["command"]=="abort" => aborts+=1,
                    Event::Other(v) if v["type"]=="mock_pi" => requests.push(v["request"].clone()),
                    Event::Other(v) if v["type"]=="heimdall_aborted" => panic!("{phase}: {v}"),
                    Event::Complete {id:Some(id),error,..}=>{assert!(!error,"{phase} {id}");completed.push(id);}
                    _=>{}
                }
            }
        }).await.unwrap();
        assert_eq!(aborts,0,"{phase}: a turn that started writing was aborted");
        assert_eq!(completed,vec![first,voice.clone(),finish],"{phase}");
        assert!(requests.iter().all(|r|r["streamingBehavior"]=="steer"));
        assert!(b.inflight().unwrap().is_empty());assert_eq!(b.status,Status::Idle);
        b.shutdown().await.unwrap();
    }
}
#[tokio::test]
async fn pi_steers_default_voice_followups_and_typed_input_during_tools() {
    for phase in ["[fixture:steering-tool]"] {
        let d=tempfile::tempdir().unwrap(); let mut b=Bridge::open(Pi::launch_config(config(d.path())).unwrap(),":memory:").unwrap();
        let observation=|n|agent::protocol::Observation {id:format!("speech-{n}"),timestamp_ms:n,source:"Mic".into(),speaker:"User".into(),language:"en".into(),text:format!("sentence {n}")};
        b.observe(observation(1)).await.unwrap(); let first=b.wake(phase,&[],false).await.unwrap().unwrap();
        loop {
            let e=tokio::time::timeout(Duration::from_secs(3),b.next()).await.unwrap().unwrap().unwrap();
            if let Event::Other(v)=e {if v["type"]=="tool_execution_start" || v["type"]=="message_update" {break;}}
        }
        b.observe(observation(2)).await.unwrap();
        let voice=b.wake("decider follow-up",&[agent::protocol::Attachment {media_type:"image/png".into(),bytes:vec![1,2,3]}],false).await.unwrap().unwrap();
        let typed=b.send_text(None,"ordinary typed correction",false,None).await.unwrap().unwrap();
        let finish=b.send_text(None,"[fixture:steering-finish]",false,None).await.unwrap().unwrap();
        assert_eq!(b.queued_count().unwrap(),0);
        let mut completed=vec![]; let mut final_answers=vec![]; let mut requests=0;
        tokio::time::timeout(Duration::from_secs(5),async {
            while completed.len()<4 {
                match b.next().await.unwrap().unwrap() {
                    Event::Other(v) if v["type"]=="mock_pi" => {
                        requests+=1; assert_eq!(v["request"]["streamingBehavior"],"steer");
                        if v["request"]["id"]==voice {assert_eq!(v["request"]["observation_ids"],json!(["speech-2"]));assert_eq!(v["request"]["images"].as_array().unwrap().len(),1);}
                    }
                    Event::Other(v) if v["type"]=="heimdall_final_answer" => {final_answers.push(v["text"].as_str().unwrap().to_owned());}
                    Event::Complete {id:Some(id),error,..}=>{assert!(!error);completed.push(id);}
                    _=>{}
                }
            }
        }).await.unwrap();
        assert_eq!(requests,3); assert_eq!(completed,vec![first,voice,typed,finish]);
        // Every finished reply of the run is said once, at its own turn end: the reply written before the steers
        // entered is not lost to the last one (review 19 Oct, MUST 1), and message_end + turn_end + settlement of
        // the same message report it once.
        let steered=vec!["one steered answer".to_owned();3];
        assert_eq!(final_answers,steered,"{phase}");
        assert_eq!(agent::session::receipts(b.harness.session_path()).unwrap().len(),4);
        assert!(b.inflight().unwrap().is_empty());assert_eq!(b.status,Status::Idle);b.shutdown().await.unwrap();
    }
}
#[tokio::test]
async fn pi_compaction_preflight_rejection_queues_then_retries_exact_delivery() {
    let d=tempfile::tempdir().unwrap();let mut b=Bridge::open(Pi::launch_config(config(d.path())).unwrap(),":memory:").unwrap();
    let id=b.wake("[fixture:compaction-preflight]",&[],false).await.unwrap().unwrap();
    loop {if matches!(b.next().await.unwrap(),Some(Event::Deferred {..})){break;}}
    assert_eq!(b.queued_count().unwrap(),1);assert!(b.inflight().unwrap().is_empty());
    assert_eq!(b.last_queued().unwrap().unwrap()["queue_reason"],"Pi is compacting the session");
    b.flush_queued().await.unwrap();assert_eq!(b.queued_count().unwrap(),1);
    loop {if matches!(b.next().await.unwrap(),Some(Event::Other(v)) if v["type"]=="compaction_end"){break;}}
    b.flush_queued().await.unwrap();assert_eq!(b.queued_count().unwrap(),0);
    loop {if let Some(Event::Complete {id:Some(completed),error,..})=b.next().await.unwrap(){assert_eq!(completed,id);assert!(!error);break;}}
    assert_eq!(b.status,Status::Idle);b.shutdown().await.unwrap();
}

fn review_speech(n:u64)->agent::protocol::Observation { agent::protocol::Observation{id:format!("speech-{n}"),timestamp_ms:n,source:"Mic".into(),speaker:"User".into(),language:"en".into(),text:format!("important speech {n}")} }
#[tokio::test]
async fn pi_fixture_consumes_steers_at_turn_boundaries_before_settlement() {
    for phase in ["[fixture:steering-thinking]", "[fixture:steering-tool]"] {
        let d = tempfile::tempdir().unwrap();
        let mut h = Pi::launch_config(config(d.path())).unwrap();
        h.send(message("first", phase, &[], true, false).unwrap()).await.unwrap();
        loop {
            if matches!(h.next().await.unwrap().unwrap(), Event::Other(v)
                if v["type"] == "message_update" || v["type"] == "tool_execution_start") { break; }
        }
        h.send(message("voice", "voice correction", &[], true, false).unwrap()).await.unwrap();
        h.send(message("finish", "[fixture:steering-finish]", &[], true, false).unwrap()).await.unwrap();
        let mut consumed = Vec::new();
        let mut boundaries = 0;
        let mut tool_finished = phase != "[fixture:steering-tool]";
        let mut completed = Vec::new();
        while completed.len() < 3 {
            match tokio::time::timeout(Duration::from_secs(3), h.next()).await.unwrap().unwrap().unwrap() {
                Event::Other(v) if v["type"] == "tool_execution_end" => tool_finished = true,
                Event::Other(v) if v["type"] == "turn_end" => boundaries += 1,
                Event::Other(v) if v["type"] == "message_end" && v["message"]["role"] == "user" => {
                    assert!(tool_finished, "steer consumed during the held tool");
                    assert!(boundaries > consumed.len(), "steer consumed before its turn boundary");
                    consumed.push(v["message"]["content"].as_str().unwrap().to_owned());
                }
                Event::Other(v) if v["type"] == "entry_appended" && v["entry"]["customType"] == "heimdall_receipt" => {
                    assert_eq!(consumed.len(), 2, "receipt before queued steering was consumed");
                }
                Event::Complete { id: Some(id), error, .. } => { assert!(!error); completed.push(id); }
                _ => {}
            }
        }
        assert_eq!(consumed.len(), 2);
        assert!(consumed[0].contains("voice correction"));
        assert!(consumed[1].contains("[fixture:steering-finish]"));
        assert_eq!(completed, ["first", "voice", "finish"]);
        h.shutdown().await.unwrap();
    }
}
#[tokio::test]
async fn review_preflight_rejected_earlier_wake_preserves_its_speech_after_later_success() {
    let d = tempfile::tempdir().unwrap();
    let mut b = Bridge::open(Pi::launch_config(config(d.path())).unwrap(), ":memory:").unwrap();
    b.observe(review_speech(1)).await.unwrap();
    let first = b.wake("[fixture:compaction-preflight]", &[], false).await.unwrap().unwrap();
    b.observe(review_speech(2)).await.unwrap();
    let second = b.wake("second response", &[], false).await.unwrap().unwrap();
    let mut original = String::new();
    loop {
        match tokio::time::timeout(Duration::from_secs(3), b.next()).await.unwrap().unwrap().unwrap() {
            Event::Other(v) if v["type"] == "mock_pi" && v["request"]["id"] == first => {
                original = v["request"]["message"].as_str().unwrap().into();
            }
            Event::Complete { id: Some(id), error: false, .. } if id == second => break,
            _ => {}
        }
    }
    assert!(original.contains("important speech 1"));
    assert_eq!(b.queued_count().unwrap(), 1);
    assert_eq!(b.pending_observations().unwrap(), 1);
    b.flush_queued().await.unwrap();
    let retry = loop {
        if let Event::Other(v) = tokio::time::timeout(Duration::from_secs(3), b.next()).await.unwrap().unwrap().unwrap() {
            if v["type"] == "mock_pi" && v["request"]["id"] == first { break v["request"].clone(); }
        }
    };
    assert!(retry["message"].as_str().unwrap().contains("important speech 1"), "preflight-rejected speech was omitted from the retry");
    assert_eq!(retry["observation_ids"], json!(["speech-1"]));
    loop {
        if let Event::Complete { id: Some(id), error, .. } = tokio::time::timeout(Duration::from_secs(3), b.next()).await.unwrap().unwrap().unwrap() {
            assert_eq!(id, first);
            assert!(!error);
            break;
        }
    }
    assert_eq!(b.pending_observations().unwrap(), 0);
    assert!(b.inflight().unwrap().is_empty());
    b.shutdown().await.unwrap();
}

#[tokio::test]
async fn review_consumed_speech_is_resent_once_after_engine_error() {
    let d = tempfile::tempdir().unwrap();
    let mut b = Bridge::open(Pi::launch_config(config(d.path())).unwrap(), ":memory:").unwrap();
    b.observe(review_speech(1)).await.unwrap();
    let id = b.wake("[fixture:503]", &[], false).await.unwrap().unwrap();
    let mut requests = vec![];
    let mut consumed = 0;
    let mut consumed_ids = vec![];
    loop {
        match tokio::time::timeout(Duration::from_secs(3), b.next()).await.unwrap().unwrap().unwrap() {
            Event::Other(v) if v["type"] == "mock_pi" => requests.push(v["request"]["message"].as_str().unwrap().to_owned()),
            Event::Other(v) if v["type"] == "message_end" && v["message"]["role"] == "user" => consumed += 1,
            Event::Consumed { id } => consumed_ids.push(id),
            Event::Deferred { .. } => break,
            _ => {}
        }
    }
    assert_eq!(consumed, 1);
    assert_eq!(consumed_ids, [id.clone()]);
    assert_eq!(b.pending_observations().unwrap(), 1, "consumption is not settlement");
    b.engine_ready();
    b.flush_queued().await.unwrap();
    loop {
        if let Event::Other(v) = tokio::time::timeout(Duration::from_secs(3), b.next()).await.unwrap().unwrap().unwrap() {
            if v["type"] == "mock_pi" {
                assert_eq!(v["request"]["id"], id);
                assert_eq!(v["request"]["observation_ids"], json!(["speech-1"]));
                requests.push(v["request"]["message"].as_str().unwrap().into());
                break;
            }
        }
    }
    b.shutdown().await.unwrap();
    assert_eq!(requests.join("\n").matches("important speech 1").count(), 2, "one original and one recovery copy");
    assert_eq!(requests[1].matches("important speech 1").count(), 1);
    assert!(requests[1].contains("(resent after a delivery interruption; ignore repeated speech)"));
}
/// Heimdall's Pi never loads the Vault's AGENTS.md/CLAUDE.md ("Heimdall should have his own system
/// prompt … Vault is something separate"); T3 workers are separate processes and keep theirs.
#[tokio::test]
async fn pi_launches_without_context_files() {
    let d = tempfile::tempdir().unwrap();
    let mut h = Pi::launch_config(config(d.path())).unwrap();
    h.state().await.unwrap();
    let argv: Vec<String> = serde_json::from_str(&std::fs::read_to_string(d.path().join("mock-argv.json")).unwrap()).unwrap();
    assert!(argv.iter().any(|a| a == "--no-context-files"), "{argv:?}");
    h.shutdown().await.unwrap();
}

/// Review of 18 Oct: the user's Stop reported the wake as an error with no tool started, so the self-resolving delivery
/// resent it. A turn ended by an abort is settled and never resent; its speech is not carried to the next wake.
#[tokio::test]
async fn stop_while_thinking_settles_the_wake_and_never_resends_it() {
    let d = tempfile::tempdir().unwrap();
    let h = Pi::launch_config(config(d.path())).unwrap();
    let mut b = Bridge::open(h, d.path().join("spool.sqlite")).unwrap();
    b.observe(review_speech(1)).await.unwrap();
    let id = b.wake("[fixture:thinking]", &[], false).await.unwrap().unwrap();
    let mut prompts = 0;
    loop {
        let e = tokio::time::timeout(Duration::from_secs(3), b.next()).await.unwrap().unwrap().unwrap();
        if matches!(&e, Event::Other(v) if v["type"] == "mock_pi") { prompts += 1; }
        if matches!(&e, Event::Other(v) if v["type"] == "message_update") { break; }
    }
    b.harness.abort().await.unwrap();
    loop {
        let e = tokio::time::timeout(Duration::from_secs(3), b.next()).await.unwrap().unwrap().unwrap();
        if let Event::Complete { id: Some(done), error, .. } = &e { assert_eq!(done, &id); assert!(error); break; }
    }
    assert_eq!(b.take_resolutions().iter().map(|r| r.outcome).collect::<Vec<_>>(), vec![agent::Resolved::Stopped]);
    assert_eq!(b.status, Status::Idle);
    assert!(b.inflight().unwrap().is_empty());
    assert_eq!(b.pending_observations().unwrap(), 0, "the stopped turn's speech is settled, not carried forward");
    b.flush_queued().await.unwrap();
    // Anything Pi still says (the aborted settlement) arrives; no second prompt is ever sent.
    while let Ok(Ok(Some(e))) = tokio::time::timeout(Duration::from_millis(300), b.next()).await {
        if matches!(&e, Event::Other(v) if v["type"] == "mock_pi") { prompts += 1; }
    }
    assert_eq!(prompts, 1, "the stopped wake was resent");
    b.harness.shutdown().await.unwrap();
}
