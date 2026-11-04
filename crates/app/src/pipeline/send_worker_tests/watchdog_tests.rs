    use super::*;
    use crate::pipeline::plumbing_tests::{rig, turn, Detect};
    use std::time::Duration;
    fn quick_deadline(_: usize) -> Duration {
        Duration::from_secs(1)
    }
    fn pump_until(app: &mut Pipeline, mut done: impl FnMut(&Pipeline) -> bool) {
        let until = Instant::now() + Duration::from_secs(10);
        while !done(app) {
            let event = app
                .internal
                .1
                .recv_timeout(until.saturating_duration_since(Instant::now()))
                .expect("watchdog/completion stalled");
            app.internal(event).unwrap();
        }
    }
    /// The owner's first attempt fails (panics, or hangs in its loader past the deadline); the replacement loads with
    /// `retry_load`. Returns the rig after the in-flight sentence completed, with the number of owner attempts.
    fn fail_first_owner(
        blocked: bool,
        retry_load: Arc<dyn Fn() -> anyhow::Result<decide::LocalDecider> + Send + Sync>,
        deadline: fn(usize) -> Duration,
    ) -> (tempfile::TempDir, Pipeline, usize) {
        let (temp, mut app, mic) = rig(true);
        let attempts = Arc::new(AtomicUsize::new(0));
        let count = attempts.clone();
        let (release, wait) = crossbeam_channel::bounded(1);
        let (finished, retired) = crossbeam_channel::bounded(1);
        app.send_worker = Some(
            InferenceOwner::supervised(
                app.internal.0.clone(),
                Arc::new(move || {
                    if count.fetch_add(1, Ordering::AcqRel) == 0 {
                        if blocked {
                            wait.recv_timeout(Duration::from_secs(60)).unwrap();
                            let _ = finished.send(());
                            anyhow::bail!("released abandoned loader");
                        }
                        panic!("real injected owner panic");
                    }
                    retry_load()
                }),
                deadline,
                owner_pause,
            )
            .unwrap(),
        );
        // No injected model answer: the real sentence admission, native send and completion paths run after the
        // actual owner thread fails.
        turn(&mut app, &mic, 10_000_000_000, "Please investigate this build", Detect::Final, 0, None, 0, |_| {});
        pump_until(&mut app, |p| p.index.evidence.records().unwrap().iter().any(|r| r.kind == "decider_owner_failed"));
        assert!(app.health.holds("decisions"));
        assert!(app
            .index
            .evidence
            .records()
            .unwrap()
            .iter()
            .filter(|r| r.kind == "decider_owner_failed")
            .all(|r| heimdall_archive::diagnostics::problem(&r.kind, &r.payload)));
        pump_until(&mut app, |p| p.send_worker.as_ref().unwrap().outstanding.load(Ordering::Acquire) == 0);
        assert_eq!(failures(&app), 1, "one owner failure, one requeue");
        assert_eq!(sends(&app).len(), 1, "one original sentence, one completion");
        if blocked {
            // A late abandoned answer must not consume a callback, clear a newer health problem, release tickets
            // twice or execute again.
            release.send(()).unwrap();
            retired.recv_timeout(Duration::from_secs(2)).unwrap();
            while let Ok(event) = app.internal.1.recv_timeout(Duration::from_millis(100)) {
                app.internal(event).unwrap();
            }
            assert_eq!(sends(&app).len(), 1);
        }
        assert!(app.take_recovery().is_none());
        let attempts = attempts.load(Ordering::Acquire);
        (temp, app, attempts)
    }
    fn sends(app: &Pipeline) -> Vec<heimdall_archive::Record> {
        app.index.evidence.records().unwrap().into_iter().filter(|r| r.kind == "send_decision").collect()
    }
    #[test]
    fn panicking_and_blocked_owners_requeue_the_inflight_sentence_once() {
        // Bug 2026-10-27T03-40-00: the replacement loaded the real decider under the 1 s test deadline and, under
        // the full check's load, hung too (3 attempts, not 2). The default suite runs no model: the replacement's
        // loader fails at once, as a missing model does, and the sentence still completes exactly once.
        for blocked in [false, true] {
            let (_temp, app, attempts) =
                fail_first_owner(blocked, Arc::new(|| anyhow::bail!("fake decider: no model")), quick_deadline);
            assert_eq!(attempts, 2, "the failed owner and one replacement");
            let sent = sends(&app);
            assert!(sent[0].payload["error"].as_str().is_some_and(|e| e.contains("fake decider")), "{:?}", sent[0].payload);
            assert!(
                sent[0].payload["pending"].to_string().contains("Please investigate this build"),
                "the requeued sentence is the original one: {:?}",
                sent[0].payload
            );
        }
    }
    /// Opt-in (loads the real decider): the first owner's hang is caught at 1 s, the real replacement then gets the
    /// production load and cold allowances, so machine load cannot add attempts.
    static REAL_LOAD_STARTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    fn real_deadline(tokens: usize) -> Duration {
        if !REAL_LOAD_STARTED.load(Ordering::Acquire) {
            return Duration::from_secs(1);
        }
        match tokens {
            LOADING => Duration::from_secs(30),
            _ => Duration::from_secs(10),
        }
    }
    #[test]
    #[ignore = "loads the real decider model; run with --ignored"]
    fn real_decider_replacement_answers_the_requeued_sentence() {
        let models = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../models/decide");
        for blocked in [false, true] {
            REAL_LOAD_STARTED.store(false, Ordering::Release);
            let models = models.clone();
            let scratch = tempfile::tempdir().unwrap();
            let archive = scratch.path().join("fault-decisions");
            let (_temp, app, attempts) = fail_first_owner(
                blocked,
                Arc::new(move || {
                    REAL_LOAD_STARTED.store(true, Ordering::Release);
                    decide::LocalDecider::load(&models, &archive)
                }),
                real_deadline,
            );
            assert_eq!(attempts, 2);
            let sent = sends(&app);
            assert!(sent[0].payload.get("error").is_none(), "{:?}", sent[0].payload);
            assert!(sent[0].payload["prompt"]["state"].as_str().unwrap().contains("Please investigate this build"));
            assert!(!app.health.holds("decisions"));
        }
    }
    fn fast_deadline(tokens: usize) -> Duration {
        match tokens {
            LOADING => Duration::from_secs(5),
            _ => Duration::from_millis(200),
        }
    }
    fn quick_pause(attempt: u32) -> Option<Duration> {
        (attempt >= 3).then(|| Duration::from_millis(400))
    }
    fn cues(app: &Pipeline) -> Vec<ui::HealthCue> {
        app.hooks.ui.lock().unwrap().iter().filter_map(|u| match u { ui::UiUpdate::Health(c) => Some(c.clone()), _ => None }).collect()
    }
    #[test]
    fn a_decider_that_keeps_hanging_pauses_commands_and_recovers_without_relaunching_heimdall() {
        // Bugs 2026-10-26T15-49-44 / T15-52-01: three owner timeouts in 10 minutes relaunched Heimdall mid-answer.
        // A fake owner (no model): its first three runs hang past the deadline, later runs answer.
        let (_temp, mut app, _) = rig(false);
        let loads = Arc::new(AtomicUsize::new(0));
        let loaded = loads.clone();
        app.send_worker = Some(
            InferenceOwner::supervised(
                app.internal.0.clone(),
                Arc::new(move || {
                    loaded.fetch_add(1, Ordering::AcqRel);
                    anyhow::bail!("fake owner: no model")
                }),
                fast_deadline,
                quick_pause,
            )
            .unwrap(),
        );
        let runs = Arc::new(AtomicUsize::new(0));
        let answers = Arc::new(std::sync::Mutex::new(Vec::<&'static str>::new()));
        let ask = |app: &Pipeline| {
            let (runs, answers) = (runs.clone(), answers.clone());
            app.send_worker.as_ref().unwrap().request(
                1,
                20,
                move |state| {
                    if runs.fetch_add(1, Ordering::AcqRel) < 3 {
                        std::thread::sleep(Duration::from_millis(600));
                        return "late";
                    }
                    match state.decider() {
                        Err(e) if e.to_string().contains("next try") => "paused",
                        // Past the pause the loader runs again; the fake then answers as a model would.
                        _ => {
                            state.outcome(true);
                            "answered"
                        }
                    }
                },
                move |p, answer| {
                    answers.lock().unwrap().push(answer);
                    p.decisions += 1;
                    Ok(())
                },
            )
        };
        ask(&app).unwrap();
        pump_until(&mut app, |p| p.decisions == 1);
        // Three hangs: the owner was replaced each time and the same request ran again; the third pauses it.
        assert_eq!(failures(&app), 3);
        assert_eq!(*answers.lock().unwrap(), ["paused"], "the in-flight request is answered, never dropped");
        assert_eq!(loads.load(Ordering::Acquire), 0, "no reload during the pause");
        assert!(app.take_recovery().is_none(), "a decider failure never relaunches Heimdall");
        let records = app.index.evidence.records().unwrap();
        let failed: Vec<_> = records.iter().filter(|r| r.kind == "decider_owner_failed").collect();
        assert!(failed.iter().all(|r| r.payload["restarting"] == true));
        assert!(failed[..2].iter().all(|r| r.payload["pause_s"].is_null()));
        assert!(!failed[2].payload["pause_s"].is_null());
        assert!(app.health.holds("decisions"));
        let shown = cues(&app);
        assert!(
            matches!(shown.last(), Some(ui::HealthCue::Drop { key, label, .. }) if key == "decisions" && label == "Commands paused · decider not answering"),
            "{shown:?}"
        );
        assert!(!shown.iter().any(|c| matches!(c, ui::HealthCue::Problem { .. })), "paused is not a red problem: {shown:?}");

        // After the pause the next request loads again and answers: commands are back.
        std::thread::sleep(Duration::from_millis(500));
        ask(&app).unwrap();
        pump_until(&mut app, |p| p.decisions == 2);
        assert_eq!(*answers.lock().unwrap(), ["paused", "answered"]);
        assert_eq!(loads.load(Ordering::Acquire), 1);
        assert!(!app.health.holds("decisions"));
        assert!(matches!(cues(&app).last(), Some(ui::HealthCue::Recovered { key, label, .. }) if key == "decisions" && label == "Commands OK"));
        assert!(app.take_recovery().is_none());
        let began = Instant::now();
        drop(app.send_worker.take());
        join_retired();
        assert!(began.elapsed() < Duration::from_secs(1), "Quit joined an abandoned owner");
    }
    #[test]
    fn writing_a_decision_row_never_holds_the_owner() {
        // The root cause of the 26 Oct hangs: the owner wrote each training row itself, 6 s behind live archive
        // commits after a 72 ms answer, and was declared hung. Rows now queue to their own thread, in order.
        let (_temp, app, _) = rig(false);
        let (release, held) = crossbeam_channel::bounded::<()>(0);
        let written = Arc::new(std::sync::Mutex::new(Vec::new()));
        let rows = written.clone();
        let archive = DecisionArchive::start_with(app.evidence.clone(), move |(input, ..)| {
            held.recv_timeout(Duration::from_secs(5)).unwrap();
            rows.lock().unwrap().push(input.current.id.clone());
            Ok(())
        })
        .unwrap();
        let began = Instant::now();
        for id in ["a", "b", "c"] {
            archive.queue(&decide::barge::input(id, "stop", "working").unwrap(), None, None, None);
        }
        assert!(began.elapsed() < Duration::from_millis(100), "queueing waited for the archive");
        for _ in 0..3 {
            release.send(()).unwrap();
        }
        let until = Instant::now() + Duration::from_secs(5);
        while written.lock().unwrap().len() < 3 && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(*written.lock().unwrap(), ["a", "b", "c"]);
    }
    #[test]
    fn a_row_that_panics_does_not_stop_the_rows_after_it() {
        let (_temp, app, _) = rig(false);
        let written = Arc::new(std::sync::Mutex::new(Vec::new()));
        let rows = written.clone();
        let archive = DecisionArchive::start_with(app.evidence.clone(), move |(input, ..)| {
            assert_ne!(input.current.id, "bad", "injected row panic");
            rows.lock().unwrap().push(input.current.id.clone());
            Ok(())
        })
        .unwrap();
        for id in ["bad", "good"] {
            archive.queue(&decide::barge::input(id, "stop", "working").unwrap(), None, None, None);
        }
        let until = Instant::now() + Duration::from_secs(5);
        while written.lock().unwrap().is_empty() && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(*written.lock().unwrap(), ["good"]);
    }
    #[test]
    fn owners_pause_from_the_third_failure_doubling_to_five_minutes() {
        assert_eq!(owner_pause(1), None);
        assert_eq!(owner_pause(2), None);
        assert_eq!(owner_pause(3), Some(Duration::from_secs(30)));
        assert_eq!(owner_pause(4), Some(Duration::from_secs(60)));
        assert_eq!(owner_pause(6), Some(Duration::from_secs(240)));
        assert_eq!(owner_pause(7), Some(Duration::from_secs(300)));
        assert_eq!(owner_pause(40), Some(Duration::from_secs(300)));
    }
    #[test]
    fn completed_callbacks_survive_a_later_owner_failure() {
        let (_temp, mut app, _) = rig(false);
        let attempts = Arc::new(AtomicUsize::new(0));
        let count = attempts.clone();
        let (finished, done) = crossbeam_channel::bounded(1);
        app.send_worker = Some(
            InferenceOwner::supervised(
                app.internal.0.clone(),
                Arc::new(|| anyhow::bail!("unused loader")),
                quick_deadline,
                owner_pause,
            )
            .unwrap(),
        );
        let owner = app.send_worker.as_ref().unwrap();
        owner
            .request(
                1,
                10,
                |_| (),
                |p, _| {
                    p.decisions += 1;
                    Ok(())
                },
            )
            .unwrap();
        owner
            .request(
                1,
                10,
                move |_| {
                    if count.fetch_add(1, Ordering::AcqRel) == 0 {
                        panic!("second task fails after first callback was published");
                    }
                    finished.send(()).unwrap();
                },
                |p, _| {
                    p.decisions += 1;
                    Ok(())
                },
            )
            .unwrap();
        // Let replacement finish before processing either callback. Retirement
        // must invalidate late attempts, never already-published completions.
        done.recv_timeout(Duration::from_secs(2)).unwrap();
        pump_until(&mut app, |p| {
            p.send_worker
                .as_ref()
                .unwrap()
                .outstanding
                .load(Ordering::Acquire)
                == 0
        });
        assert_eq!(app.decisions, 2);
        assert_eq!(attempts.load(Ordering::Acquire), 2);
    }
    #[test]
    fn request_deadline_scales_with_bounded_input_size() {
        assert!(request_deadline(100) < request_deadline(2000));
        assert_eq!(request_deadline(2000), request_deadline(100_000));
        assert_eq!(request_deadline(usize::MAX), Duration::from_secs(30));
    }
    fn cold_deadline(tokens: usize) -> Duration {
        match tokens {
            LOADING => Duration::from_secs(30),
            COLD => Duration::from_secs(3),
            _ => Duration::from_millis(300),
        }
    }
    fn failures(app: &Pipeline) -> usize {
        app.index.evidence.records().unwrap().iter().filter(|r| r.kind == "decider_owner_failed").count()
    }
    #[test]
    fn the_first_requests_after_a_load_get_the_cold_allowance_and_a_later_hang_still_recovers() {
        // Bug 2026-10-24T04-30-49: a cold decider's first answer missed the warm deadline and recovery relaunched.
        let (_temp, mut app, _) = rig(false);
        app.send_worker = Some(
            InferenceOwner::supervised(app.internal.0.clone(), Arc::new(|| anyhow::bail!("unused loader")), cold_deadline, owner_pause).unwrap(),
        );
        let owner = app.send_worker.as_ref().unwrap();
        // A load, then a first answer slower than the warm 300 ms (the cold 02:32 case), then two quick ones.
        owner.request(1, 20, |state| { state.mark_loaded(); std::thread::sleep(Duration::from_millis(800)) },
            |p, ()| { p.decisions += 1; Ok(()) }).unwrap();
        for _ in 0..2 {
            owner.request(1, 20, |_| (), |p, ()| { p.decisions += 1; Ok(()) }).unwrap();
        }
        pump_until(&mut app, |p| p.decisions == 3);
        assert_eq!(failures(&app), 0, "a cold first answer is not a hang");
        // The allowance is spent after three requests: a stall now is a hang, and recovery runs as before.
        let slow = Arc::new(AtomicUsize::new(0));
        let once = slow.clone();
        app.send_worker.as_ref().unwrap().request(1, 20, move |_| {
            if once.fetch_add(1, Ordering::AcqRel) == 0 { std::thread::sleep(Duration::from_millis(800)) }
        }, |p, ()| { p.decisions += 1; Ok(()) }).unwrap();
        pump_until(&mut app, |p| p.decisions == 4);
        assert_eq!(failures(&app), 1, "a warm stall still restarts the owner");
        assert_eq!(slow.load(Ordering::Acquire), 2, "the stalled sentence ran again on the replacement");
        assert!(app.take_recovery().is_none());
    }
    #[test]
    fn a_hang_past_the_cold_allowance_still_recovers() {
        let (_temp, mut app, _) = rig(false);
        app.send_worker = Some(
            InferenceOwner::supervised(app.internal.0.clone(), Arc::new(|| anyhow::bail!("unused loader")), cold_deadline, owner_pause).unwrap(),
        );
        let runs = Arc::new(AtomicUsize::new(0));
        let count = runs.clone();
        app.send_worker.as_ref().unwrap().request(1, 20, move |state| {
            if count.fetch_add(1, Ordering::AcqRel) == 0 { state.mark_loaded(); std::thread::sleep(Duration::from_millis(3600)) }
        }, |p, ()| { p.decisions += 1; Ok(()) }).unwrap();
        pump_until(&mut app, |p| p.decisions == 1);
        assert_eq!(failures(&app), 1);
    }
    #[test]
    fn the_cold_allowance_ends_with_its_window_and_rearms_only_on_a_new_load() {
        let t = Instant::now();
        let mut cold = Cold::default();
        cold.watch(0, t);
        assert!(!cold.applies(t), "no load yet: warm");
        cold.watch(1, t);
        assert!(cold.applies(t + Duration::from_secs(59)));
        assert!(!cold.applies(t + COLD_WINDOW), "never past the window, requests left or not");
        cold.left = 0;
        cold.watch(1, t);
        assert!(!cold.applies(t), "the same load does not rearm it");
        cold.watch(2, t);
        assert!(cold.applies(t) && cold.left == COLD_REQUESTS);
        assert_eq!(request_deadline(COLD), Duration::from_secs(10));
        assert!(request_deadline(COLD) > request_deadline(decide::send::WINDOW));
    }
