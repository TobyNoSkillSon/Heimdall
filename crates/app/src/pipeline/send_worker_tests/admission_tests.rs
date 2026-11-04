    use super::*;
    use crate::pipeline::plumbing_tests::{rig, turn, Detect};
    use std::time::Duration;
    const S: u64 = 1_000_000_000;
    #[test]
    fn consecutive_explicit_finals_send_all_pending_while_an_older_command_is_held() {
        let (_t, mut app, mic) = rig(true);
        let (entered, started) = crossbeam_channel::bounded(1);
        let (release, wait) = crossbeam_channel::bounded(1);
        app.inference()
            .unwrap()
            .task(2, move |_| {
                entered.send(()).unwrap();
                wait.recv_timeout(Duration::from_secs(10)).unwrap();
                Box::new(|_| Ok(()))
            })
            .unwrap();
        started.recv_timeout(Duration::from_secs(2)).unwrap();
        turn(
            &mut app,
            &mic,
            10 * S,
            "take a screenshot",
            Detect::Final,
            0,
            None,
            0,
            |_| {},
        );
        assert!(app.command_inflight.get(&mic).is_some_and(|n| *n > 0));
        turn(
            &mut app,
            &mic,
            20 * S,
            "Heimdall inspect that capture",
            Detect::Final,
            0,
            None,
            0,
            |_| {},
        );
        turn(
            &mut app,
            &mic,
            30 * S,
            "send it with the current branch",
            Detect::Final,
            0,
            None,
            0,
            |_| {},
        );
        app.agent.as_ref().unwrap().barrier().unwrap();
        let records = app.index.evidence.records().unwrap();
        assert_eq!(records.iter().filter(|r| r.kind == "agent_wake").count(), 2);
        let pending = app.tools.send_context.lock().unwrap().snapshot();
        assert_eq!(
            pending
                .turns
                .iter()
                // Settled assistant replies may arrive before this assertion.
                // These are the three captured observations being ordered.
                .filter(|t| t.observation.is_some())
                .map(|t| t.text.as_str())
                .collect::<Vec<_>>(),
            [
                "take a screenshot",
                "Heimdall inspect that capture",
                "send it with the current branch"
            ]
        );
        let ids: std::collections::BTreeSet<_> = pending
            .turns
            .iter()
            .filter_map(|t| t.observation.as_ref())
            .map(|observation| observation["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids.len(), 3);
        assert!(pending.pending.is_empty());
        let sends: Vec<_> = records
            .iter()
            .filter(|r| r.kind == "agent_send" && r.payload["query"] == true)
            .collect();
        assert!(!sends.is_empty());
        let text = records.iter().filter(|r|r.kind=="agent_send").filter_map(|r|r.payload["payload"]["message"]["content"][0]["text"].as_str()).collect::<Vec<_>>().join("\n");
        assert!(text.find("take a screenshot").unwrap() < text.find("inspect that capture").unwrap());
        assert_eq!(text.matches("take a screenshot").count(),1);
        assert_eq!(text.matches("inspect that capture").count(),1);

        release.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while app
            .send_worker
            .as_ref()
            .unwrap()
            .outstanding
            .load(Ordering::Acquire)
            > 0
        {
            let event = app
                .internal
                .1
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap();
            app.internal(event).unwrap();
        }
        assert!(app.command_deferred.values().all(|q| q.is_empty()));
        assert!(app.early_sends.is_empty());
    }

    /// E9.2: a screenshot with no words to ride is pending for two minutes, then it expires.
    #[test]
    fn a_screenshot_without_words_expires_after_two_minutes() {
        let (_t, mut app, _mic) = rig(false);
        app.attach_pending(agent::protocol::Attachment { media_type: "image/png".into(), bytes: vec![1, 2, 3] });
        let records = app.index.evidence.records().unwrap();
        assert!(records.iter().all(|r| r.kind != "agent_wake"), "a pending screenshot wakes nothing");
        let captured = app.attachment_stamps[0].1;
        app.expire_attachments(captured + crate::pipeline::ATTACHMENT_TTL_NS);
        assert_eq!(app.attachment_batch.len(), 1, "exactly two minutes still waits");
        app.expire_attachments(captured + crate::pipeline::ATTACHMENT_TTL_NS + 1);
        assert!(app.attachment_batch.is_empty() && app.attachment_stamps.is_empty(), "expired");
    }


    /// E9.1: a screenshot that arrives after its words is delivered once, in a steer or the next wake, and never
    /// wakes the agent by itself.
    #[test]
    fn a_late_screenshot_is_delivered_once_and_never_wakes_alone() {
        let (_t, mut app, mic) = rig(true);
        app.command_test_answers.push_back((decide::CommandIntent::Yes, decide::CommandSelection::LookAtThis));
        turn(&mut app, &mic, 10 * S, "Heimdall look at this the chart is wrong", Detect::Partial, 4, None, S / 2, |_| {});
        app.agent.as_ref().unwrap().barrier().unwrap();
        turn(&mut app, &mic, 30 * S, "Heimdall what do you see", Detect::Final, 0, None, S / 2, |_| {});
        app.agent.as_ref().unwrap().barrier().unwrap();
        let records = app.index.evidence.records().unwrap();
        assert!(records.iter().any(|r| r.kind == "screen_command"), "the screenshot was taken");
        let count = |kind: &str| -> usize {
            records
                .iter()
                .filter(|r| r.kind == kind)
                .map(|r| r.payload["image_ids"].as_array().map_or(0, |a| a.len()))
                .sum()
        };
        assert_eq!(records.iter().filter(|r| r.kind == "agent_wake").count(), 2, "one wake per explicit turn");
        assert_eq!(count("agent_wake") + count("agent_steer"), 2, "one screenshot per display, delivered exactly once");
    }
