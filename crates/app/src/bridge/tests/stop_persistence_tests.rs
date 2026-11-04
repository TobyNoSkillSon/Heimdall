    use super::*;
    use agent::delivery::StopReason;

    #[test]
    fn notice_epoch_survives_relaunch_and_off_on_cancels_the_old_epoch() -> Result<()> {
        let dir=tempfile::tempdir()?;
        let tools=Tools::new(dir.path(),dir.path());
        let mut settings=Settings {dry_run:false,..Default::default()};
        tools.initialize_delivery(&settings);
        tools.menu_intent(StopReason::Agent,false);
        tools.menu_intent(StopReason::Agent,true);
        tools.flush_intent()?;
        let epoch=tools.agent_enabled.permit()?.generation();
        assert_eq!(epoch,2);
        settings.delivery_generation=epoch;
        settings.save(&dir.path().join("settings.json"))?;
        tools.intent_saved_in_settings(tools.intent_generation())?;
        drop(tools);
        let settings=Settings::load(&dir.path().join("settings.json"))?;
        let tools=Tools::new(dir.path(),dir.path());tools.initialize_delivery(&settings);
        assert_eq!(tools.agent_enabled.permit()?.generation(),epoch);
        tools.menu_intent(StopReason::Agent,false);tools.menu_intent(StopReason::Agent,true);
        tools.flush_intent()?;drop(tools);
        let tools=Tools::new(dir.path(),dir.path());tools.initialize_delivery(&settings);
        assert_eq!(tools.agent_enabled.permit()?.generation(),epoch+2);
        Ok(())
    }
    #[test]
    fn off_after_final_flush_is_durable_before_exit_and_recovers_stop_only() -> Result<()> {
        for reason in [StopReason::Agent, StopReason::Heimdall] {
            let dir = tempfile::tempdir()?;
            let tools = Tools::new(dir.path(), dir.path());
            let settings = Settings { dry_run: false, paused: false, ..Default::default() };
            tools.initialize_delivery(&settings);
            tools.with_final_intent(|| {
                assert!(tools.intent_acceptance.try_lock().unwrap().closed);
                // Simulate a native Off callback during helper creation, AFTER
                // the final flush. None of the subsequent On sources may undo it.
                tools.menu_intent(reason, false);
                let generation = tools.intent_generation();
                tools.menu_intent(reason, true);
                assert!(!tools.file_intent(reason, true));
                assert!(!tools.resume_agent_intent(generation.agent));
                assert_eq!(generation, tools.intent_generation());
                Ok(())
            }, || {
                let intent: StopIntent = serde_json::from_slice(&std::fs::read(&tools.intent_path)?)?;
                assert!(if matches!(reason, StopReason::Agent) { intent.agent_off } else { intent.heimdall_off });
                // Startup receives older On settings; the acknowledged stop must win.
                let relaunched = Tools::new(dir.path(), dir.path());
                relaunched.initialize_delivery(&settings);
                assert!(relaunched.stopped_for(reason));
                // Listening's stop never closes the agent's delivery.
                assert_eq!(relaunched.agent_enabled.permit().is_err(), matches!(reason, StopReason::Agent));
                Ok(())
            })?;
            let failed: Result<()> = tools.with_final_intent(|| anyhow::bail!("helper unavailable"), || panic!("must not exit"));
            assert!(failed.is_err());
            tools.menu_intent(reason, true);
            tools.flush_intent()?;
            assert!(!tools.stopped_for(reason), "failed relaunch did not reopen acceptance");
        }
        Ok(())
    }

    #[test]
    fn menu_callbacks_and_quit_dispatch_without_waiting_for_stalled_final_flush() -> Result<()> {
        for (reason, off, on) in [
            (StopReason::Agent, ui::UiAction::SetDryRun(true), ui::UiAction::SetDryRun(false)),
            (StopReason::Heimdall, ui::UiAction::SetListening(false), ui::UiAction::SetListening(true)),
        ] {
            let dir = tempfile::tempdir()?;
            let mut tools = Tools::new(dir.path(), dir.path());
            let (tx, writer_rx) = std::sync::mpsc::channel();
            tools.intent_writer = Ok(IntentWriter { tx: Some(tx), thread: None });
            let tools = Arc::new(tools);
            let shutdown_tools = tools.clone();
            let shutdown = std::thread::spawn(move || shutdown_tools.with_final_intent(|| Ok(()), || Ok(())));
            let IntentCommand::Flush(flush_reply) = writer_rx.recv_timeout(std::time::Duration::from_secs(2))? else { panic!("expected final flush"); };
            // The writer is now deterministically stalled. Run the REAL callback
            // path on one simulated UI thread: Off, On, then a subsequent Quit.
            let (action_tx, action_rx) = crossbeam_channel::unbounded();
            let (done_tx, done_rx) = std::sync::mpsc::channel();
            let callback = crate::ui_action_callback(tools.clone(), action_tx, ui::Ui::handle());
            let ui_thread = std::thread::spawn(move || {
                let began = std::time::Instant::now();
                callback(off);
                callback(on);
                callback(ui::UiAction::Quit);
                done_tx.send(began.elapsed()).unwrap();
            });
            let latency = done_rx.recv_timeout(std::time::Duration::from_millis(100));
            // Release even on regression so the test can join both threads.
            flush_reply.send(Ok(()))?;
            ui_thread.join().unwrap();
            let latency = latency.context("UI callbacks waited on the stalled flush")?;
            assert!(latency < std::time::Duration::from_millis(5), "callbacks took {latency:?}");
            assert!(matches!(action_rx.try_recv(), Ok(ui::UiAction::SetDryRun(true) | ui::UiAction::SetListening(false))));
            assert!(action_rx.try_recv().is_ok());
            assert!(matches!(action_rx.try_recv(), Ok(ui::UiAction::Quit)), "subsequent Quit was not dispatched");
            assert!(tools.stopped_for(reason));
            assert!(tools.agent_enabled.stopped_for(StopReason::Quit));
            let IntentCommand::Persist(Some(stop_reply)) = writer_rx.recv_timeout(std::time::Duration::from_secs(2))? else { panic!("expected acknowledged late Off"); };
            // Model the writer's durable commit, not just a successful enqueue.
            let (generation, agent_off, heimdall_off) = tools.stop_snapshot();
            heimdall_archive::atomic_write(&tools.intent_path, &serde_json::to_vec(&StopIntent { generation: generation.agent, agent_off, heimdall_off })?)?;
            stop_reply.send(Ok(()))?;
            shutdown.join().unwrap()?;
            let relaunched = Tools::new(dir.path(), dir.path());
            relaunched.initialize_delivery(&Settings { dry_run: false, paused: false, ..Default::default() });
            assert!(relaunched.stopped_for(reason));
        }
        Ok(())
    }

    #[test]
    fn failed_final_flush_reopens_acceptance() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut tools = Tools::new(dir.path(), dir.path());
        let (tx, rx) = std::sync::mpsc::channel();
        tools.intent_writer = Ok(IntentWriter { tx: Some(tx), thread: None });
        let tools = Arc::new(tools);
        let shutdown_tools = tools.clone();
        let shutdown = std::thread::spawn(move || shutdown_tools.with_final_intent(|| panic!("must not create helper"), || Ok(())));
        let IntentCommand::Flush(reply) = rx.recv_timeout(std::time::Duration::from_secs(2))? else { panic!("expected flush"); };
        reply.send(Err("stalled disk".into()))?;
        assert!(shutdown.join().unwrap().is_err());
        tools.menu_intent(StopReason::Agent, true);
        assert!(matches!(rx.try_recv(), Ok(IntentCommand::Persist(None))));
        assert!(!tools.intent_acceptance.lock().unwrap().closed);
        Ok(())
    }

    #[test]
    fn unacknowledged_late_off_cancels_exit_with_a_short_bound() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut tools = Tools::new(dir.path(), dir.path());
        let (tx, rx) = std::sync::mpsc::channel();
        tools.intent_writer = Ok(IntentWriter { tx: Some(tx), thread: None });
        let tools = Arc::new(tools);
        let shutdown_tools = tools.clone();
        let shutdown = std::thread::spawn(move || shutdown_tools.with_final_intent::<()>(|| {
            shutdown_tools.menu_intent(StopReason::Heimdall, false);
            Ok(())
        }, || panic!("cannot exit before late Off is durable")));
        let IntentCommand::Flush(reply) = rx.recv_timeout(std::time::Duration::from_secs(2))? else { panic!("expected flush"); };
        reply.send(Ok(()))?;
        let IntentCommand::Persist(Some(_held_reply)) = rx.recv_timeout(std::time::Duration::from_secs(2))? else { panic!("expected late Off"); };
        let began = std::time::Instant::now();
        let error = shutdown.join().unwrap().unwrap_err();
        assert!(began.elapsed() < std::time::Duration::from_secs(1), "exit waited on the 10 s shared flush timeout");
        assert!(format!("{error:#}").contains("late stop intent did not finish"));
        tools.menu_intent(StopReason::Heimdall, true);
        assert!(!tools.stopped_for(StopReason::Heimdall));
        assert!(matches!(rx.try_recv(), Ok(IntentCommand::Persist(None))));
        Ok(())
    }

    #[test]
    fn settings_save_wait_does_not_hold_off_acceptance() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut tools = Tools::new(dir.path(), dir.path());
        let (tx, rx) = std::sync::mpsc::channel();
        tools.intent_writer = Ok(IntentWriter { tx: Some(tx), thread: None });
        let tools = Arc::new(tools);
        let saved_tools = tools.clone();
        let saved = std::thread::spawn(move || saved_tools.intent_saved_in_settings(Default::default()));
        let IntentCommand::Saved(_, reply) = rx.recv_timeout(std::time::Duration::from_secs(2))? else { panic!("expected save"); };
        let off_tools = tools.clone();
        let off = std::thread::spawn(move || off_tools.menu_intent(StopReason::Agent, false));
        // The callback must enqueue Persist before the settings writer replies.
        let persisted = rx.recv_timeout(std::time::Duration::from_secs(2));
        reply.send(Ok(()))?;
        saved.join().unwrap()?;
        off.join().unwrap();
        assert!(matches!(persisted, Ok(IntentCommand::Persist(None))), "settings I/O blocked Off acceptance");
        assert!(tools.agent_enabled.stopped_for(StopReason::Agent));
        Ok(())
    }

    #[test]
    fn stale_persisted_on_never_overrides_settings_off_at_startup() -> Result<()> {
        for reason in [StopReason::Agent, StopReason::Heimdall] {
            let dir = tempfile::tempdir()?;
            std::fs::write(dir.path().join("stop-intent.json"), br#"{"agent_off":false,"heimdall_off":false}"#)?;
            let settings = Settings { dry_run: matches!(reason, StopReason::Agent), paused: matches!(reason, StopReason::Heimdall), ..Default::default() };
            let tools = Tools::new(dir.path(), dir.path());
            tools.initialize_delivery(&settings);
            assert!(tools.stopped_for(reason), "stale durable On overrode settings Off");
            assert_eq!(tools.agent_enabled.permit().is_err(), matches!(reason, StopReason::Agent));
        }
        Ok(())
    }
    /// Listening keeps its own latch: its Off and On persist across a relaunch but never move the
    /// agent's delivery epoch, so in-flight agent work, speech and permits survive a Listening toggle.
    #[test]
    fn listening_intents_persist_on_their_own_latch_and_never_touch_delivery() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let tools = Tools::new(dir.path(), dir.path());
        tools.initialize_delivery(&Settings { dry_run: false, ..Default::default() });
        let permit = tools.agent_enabled.permit()?;
        let epoch = tools.agent_enabled.generation();
        tools.menu_intent(StopReason::Heimdall, false);
        assert!(tools.stopped_for(StopReason::Heimdall) && !tools.agent_enabled.stopped_for(StopReason::Heimdall));
        assert!(permit.check().is_ok() && tools.agent_enabled.permit().is_ok(), "Listening off closed delivery");
        assert_eq!(tools.agent_enabled.generation(), epoch, "Listening off moved the delivery epoch");
        tools.flush_intent()?;
        let intent: StopIntent = serde_json::from_slice(&std::fs::read(&tools.intent_path)?)?;
        assert!(intent.heimdall_off && !intent.agent_off && intent.generation == epoch);
        drop(tools);
        let relaunched = Tools::new(dir.path(), dir.path());
        relaunched.initialize_delivery(&Settings { dry_run: false, ..Default::default() });
        assert!(relaunched.stopped_for(StopReason::Heimdall), "Listening off did not survive the relaunch");
        assert!(relaunched.agent_enabled.permit().is_ok(), "the relaunch stopped the agent for Listening");
        assert_eq!(relaunched.agent_enabled.generation(), epoch);
        let permit = relaunched.agent_enabled.permit()?;
        relaunched.menu_intent(StopReason::Heimdall, true);
        assert!(!relaunched.stopped_for(StopReason::Heimdall) && permit.check().is_ok(), "Listening on revoked the agent's epoch");
        Ok(())
    }
