    use super::*;
    use crate::pipeline::plumbing_tests::{rig, turn, Detect};
    use std::time::Duration;
    const S: u64 = 1_000_000_000;
    pub(super) fn drain(app: &mut Pipeline) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while app
            .send_worker
            .as_ref()
            .is_some_and(|o| o.outstanding.load(Ordering::Acquire) > 0)
        {
            let event = app
                .internal
                .1
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap();
            app.internal(event).unwrap();
        }
    }
    /// The screen-command check (Decider V5's foreground question) for `text`, as the conductor asks it.
    pub(super) fn command_input(id: &str, text: &str) -> decide::DecisionInput {
        decide::DecisionInput {
            source: decide::DecisionSource::Microphone,
            is_final: false,
            current: decide::Turn { id: id.into(), role: decide::Role::Owner, transcript: text.into(), audio: decide::AudioClip::default() },
            context: vec![],
            pending_transcript: String::new(),
            agent_state: "collecting".into(),
            media: decide::DecisionMedia::Text,
            questions: vec![decide::Question::CommandCheck(decide::CommandSelection::LookAtThis)],
        }
    }
    pub(super) fn ready(app: &mut Pipeline) {
        app.inference()
            .unwrap()
            .task(2, |s| {
                s.decider().unwrap();
                Box::new(|_| Ok(()))
            })
            .unwrap();
        drain(app);
    }
    /// Hold the exact urgency signal observed by native send inference for one
    /// queued send, then finish foreground work without another spoken final.
    fn preempt_one_send(app: &mut Pipeline, mic: &str) {
        app.models = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../models");
        ready(app);
        let owner = app.inference().unwrap();
        let hold = owner.hold();
        owner
            .task(1, |s| {
                s.urgent.fetch_add(1, Ordering::AcqRel);
                Box::new(|_| Ok(()))
            })
            .unwrap();
        turn(
            app,
            mic,
            10 * S,
            "Please inspect the current build",
            Detect::Final,
            0,
            None,
            0,
            |_| {},
        );
        app.inference()
            .unwrap()
            .task(1, |s| {
                s.urgent.fetch_sub(1, Ordering::AcqRel);
                Box::new(|_| Ok(()))
            })
            .unwrap();
        drop(hold);
    }
    #[test]
    fn foreground_preemption_resumes_pending_without_another_final() {
        let (_temp, mut app, mic) = rig(true);
        preempt_one_send(&mut app, &mic);
        drain(&mut app);
        let records = app.index.evidence.records().unwrap();
        let decisions: Vec<_> = records
            .iter()
            .filter(|r| r.kind == "send_decision")
            .collect();
        assert_eq!(decisions.len(), 1);
        assert!(
            decisions[0].payload["probabilities"].is_object(),
            "preempted inference never resumed: {:?}",
            decisions[0].payload
        );
        assert!(records
            .iter()
            .any(|r| r.kind == "send_decision_preempted" && r.payload["outcome"] == "retry"));
        assert_eq!(
            app.errors, 0,
            "foreground preemption is not a model failure"
        );
    }
    #[test]
    fn preempted_send_does_not_retry_after_typed_or_explicit_barrier() {
        for typed in [true, false] {
            let (_temp, mut app, mic) = rig(true);
            preempt_one_send(&mut app, &mic);
            if typed {
                app.tools
                    .send_context
                    .lock()
                    .unwrap()
                    .typed("Use this typed request instead");
            } else {
                let through = app.tools.send_context.lock().unwrap().snapshot().through;
                app.tools.send_context.lock().unwrap().sent(through);
                app.dispatched_through = through;
            }
            drain(&mut app);
            app.agent.as_ref().unwrap().barrier().unwrap();
            let records = app.index.evidence.records().unwrap();
            assert!(!records
                .iter()
                .any(|r| r.kind == "send_decision" && r.payload["probabilities"].is_object()));
            assert!(!records.iter().any(|r| r.kind == "agent_wake"));
            assert!(records.iter().any(|r| r.kind == "send_decision_preempted"
                && r.payload["outcome"] == "superseded_by_barrier"));
            assert_eq!(
                app.errors, 0,
                "superseded preemption is not a model failure"
            );
        }
    }
    #[test]
    fn real_native_lock_does_not_block_explicit_command_send() {
        let (_temp, mut app, mic) = rig(true);
        app.models = std::env::var_os("HEIMDALL_FIX2_MODELS")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../models"));
        app.settings.screen_off = true;
        app.prepare_decisions().unwrap();
        let lib = app.models.join("decide/runtime/libheimdall_decide.dylib");
        let (entered, held) = std::sync::mpsc::channel::<()>();
        let holder = std::thread::spawn(move || unsafe {
            let library = libloading::Library::new(lib).unwrap();
            let hold = library
                .get::<unsafe extern "C" fn(
                    i32,
                    unsafe extern "C" fn(*mut std::ffi::c_void),
                    *mut std::ffi::c_void,
                )>(b"heimdall_d1_test_hold_lock\0")
                .unwrap();
            unsafe extern "C" fn notify(ctx: *mut std::ffi::c_void) {
                let tx = &*(ctx as *const std::sync::mpsc::Sender<()>);
                let _ = tx.send(());
            }
            hold(250, notify, &entered as *const _ as *mut _);
        });
        held.recv_timeout(Duration::from_secs(2)).unwrap();
        let began = Instant::now();
        turn(
            &mut app,
            &mic,
            10 * S,
            "Heimdall take a screenshot",
            Detect::Final,
            0,
            None,
            0,
            |_| {},
        );
        app.agent.as_ref().unwrap().barrier().unwrap();
        let ms = began.elapsed().as_secs_f64() * 1000.;
        let wakes = app
            .index
            .evidence
            .records()
            .unwrap()
            .iter()
            .filter(|r| r.kind == "agent_wake")
            .count();
        holder.join().unwrap();
        assert_eq!(wakes, 1, "explicit wake must precede command inference");
        assert!(
            ms < 200.,
            "explicit command send blocked on the actual 250ms native lock: {ms:.1}ms"
        );
        drain(&mut app);
        let records = app.index.evidence.records().unwrap();
        let record = records.iter().find(|r| r.kind == "send_decision").unwrap();
        assert_eq!(record.payload["trigger"], "name");
        assert!(record.payload["input_tokens"].as_u64().unwrap() <= 2000);
        assert!(!app.active.contains_key(&mic));
        eprintln!("real 250ms native lock: explicit wake {ms:.1}ms");
    }
    #[test]
    fn ready_send_is_published_before_background_question_work_and_results_remain_bounded() {
        let (_temp, mut app, mic) = rig(true);
        app.models = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../models");
        ready(&mut app);
        let (entered, started) = crossbeam_channel::bounded(1);
        let (release, wait) = crossbeam_channel::bounded(1);
        let hold = app.inference().unwrap().hold();
        turn(
            &mut app,
            &mic,
            10 * S,
            "Please inspect the current build",
            Detect::Final,
            0,
            None,
            0,
            |_| {},
        );
        app.inference()
            .unwrap()
            .task(0, move |_| {
                entered.send(()).unwrap();
                wait.recv_timeout(Duration::from_secs(10)).unwrap();
                Box::new(|_| Ok(()))
            })
            .unwrap();
        drop(hold);
        started.recv_timeout(Duration::from_secs(2)).unwrap();
        let event = app.internal.1.recv_timeout(Duration::from_secs(2)).unwrap();
        app.internal(event).unwrap();
        let records = app.index.evidence.records().unwrap();
        let send = records
            .iter()
            .find(|r| r.kind == "send_decision")
            .expect("native send answer was withheld by unrelated background-question work");
        assert!(send.payload["probabilities"].is_object());
        assert!(app.internal.1.is_empty(), "background question is still blocked");
        // Bound includes in-flight and undrained completions. Saturation of one
        // background lane reserves independent admission for a spoken command's check.
        let owner = app.inference().unwrap();
        for _ in 0..CAPACITY {
            owner.task(1, |_| Box::new(|_| Ok(()))).unwrap();
        }
        assert!(owner.task(1, |_| Box::new(|_| Ok(()))).is_err());
        owner.task(3, |_| Box::new(|_| Ok(()))).unwrap();
        release.send(()).unwrap();
        drain(&mut app);
        assert_eq!(
            app.send_worker
                .as_ref()
                .unwrap()
                .outstanding
                .load(Ordering::Acquire),
            0
        );
    }
    #[test]
    fn real_command_check_precedes_queued_sends_and_meets_main_latency_bound() {
        let (_temp, mut app, _) = rig(true);
        app.models = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../models");
        ready(&mut app);
        let mut context = decide::send::Conversation::default();
        context.append(
            "User",
            &"Please inspect the current build and report the failure. ".repeat(100),
            true,
        );
        let snapshot = context.snapshot();
        let (started, entered) = crossbeam_channel::bounded(1);
        let owner = app.inference().unwrap();
        owner
            .task(1, move |s| {
                let urgent = s.urgent.clone();
                started.send(()).unwrap();
                let _ = s
                    .decider()
                    .unwrap()
                    .send_decision_interruptible(&snapshot, &urgent);
                Box::new(|_| Ok(()))
            })
            .unwrap();
        entered.recv_timeout(Duration::from_secs(2)).unwrap();
        let (done, answer) = crossbeam_channel::bounded(1);
        let began = Instant::now();
        let hold = owner.hold();
        let (queued, order) = crossbeam_channel::unbounded();
        for _ in 0..16 {
            let queued = queued.clone();
            owner
                .task(1, move |_| {
                    queued.send("send").unwrap();
                    Box::new(|_| Ok(()))
                })
                .unwrap();
        }
        owner
            .task(2, move |s| {
                let input = command_input("command-priority", "look at this, the menu is cut off");
                let result = s.decider().unwrap().command_answer(&input).0;
                done.send((began.elapsed().as_secs_f64() * 1000., result))
                    .unwrap();
                queued.send("command").unwrap();
                Box::new(|_| Ok(()))
            })
            .unwrap();
        drop(hold);
        let (ms, result) = answer.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(
            order.recv().unwrap(),
            "command",
            "the command check queued behind send decisions"
        );
        assert!(
            ms < 120.,
            "command check {ms:.1}ms exceeds main's 120ms busy-Mac native bound"
        );
        drain(&mut app);
        eprintln!("foreground command check with 16 queued sends: {ms:.1}ms (main native bound 120ms)");
    }
