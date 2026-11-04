    use super::*;
    struct HeldSpeech {
        started: crossbeam_channel::Sender<()>,
        probe: crossbeam_channel::Receiver<()>,
        cancelled: crossbeam_channel::Sender<bool>,
    }
    impl speak::Synthesizer for HeldSpeech {
        fn synthesize(&mut self, _: &str, _: &str, cancel: &dyn Fn() -> bool) -> Result<speak::Audio> {
            self.started.send(())?;
            // Only the test releases this synthesis; pipeline initialization and
            // scheduler latency must not end it before the callback is exercised.
            self.probe.recv()?;
            self.cancelled.send(cancel())?;
            anyhow::bail!("fixture ends without generating audio")
        }
    }
    /// Waits that only prove something happened (not how fast): building a pipeline under a full parallel test run
    /// can take over 5 s on a busy Mac (night 1023 integration).
    const LIVENESS: Duration = Duration::from_secs(30);
    /// Agent off only: Listening off never closes delivery or interrupts the agent's speech (see
    /// `listening_off_callback_keeps_delivery_and_speech`).
    #[test]
    fn actual_ui_callback_closes_delivery_and_interrupts_speech_with_pipeline_busy() -> Result<()> {
        for action in [ui::UiAction::SetDryRun(true)] {
            let temp = tempfile::tempdir()?;
            let tools = Arc::new(bridge::Tools::new(temp.path(), temp.path()));
            let permit = tools.agent_enabled.permit()?;
            let (started_tx, started) = bounded(1);
            let (probe, probe_rx) = bounded(1);
            let (cancelled_tx, cancelled) = bounded(1);
            tools.test_guarded_speech_queue(
                HeldSpeech { started: started_tx, probe: probe_rx, cancelled: cancelled_tx },
                speak::WavOutput::new(temp.path().join("must-not-play.wav")),
            );
            use agent::mcp::ToolHost;
            tools.speak("blocked private speech", "en", &permit)?;
            started.recv_timeout(LIVENESS)?;
            let (tx, rx) = unbounded();
            let pending = rx.clone();
            let (busy_tx, busy) = bounded(1);
            let (release, release_rx) = bounded(1);
            let pipeline_tools = tools.clone();
            let data = temp.path().join("busy-pipeline");
            let models = temp.path().to_path_buf();
            let pipeline = std::thread::spawn(move || -> Result<()> {
                let mut settings = Settings::default(); settings.dry_run=false;
                let app = pipeline::Pipeline::new_with_tools(data, models, settings, None, true, pipeline_tools.clone())?;
                assert!(Arc::ptr_eq(&app.tools, &pipeline_tools));
                busy_tx.send(())?;
                // Hold the actual pipeline before it can process any menu action.
                release_rx.recv()?;
                assert!(!app.tools.agent_enabled.load(Ordering::Acquire));
                assert!(matches!(rx.recv_timeout(LIVENESS)?, ui::UiAction::SetDryRun(true)));
                Ok(())
            });
            busy.recv_timeout(LIVENESS)?;
            // Same callback installed by main, with the pipeline held busy.
            let callback = ui_action_callback(tools.clone(), tx, ui::Ui::handle());
            assert!(cancelled.is_empty(), "speech ended before the callback");
            assert!(!pipeline.is_finished(), "pipeline did not stay held");
            callback(action.clone());
            assert!(!tools.agent_enabled.load(Ordering::Acquire));
            assert!(permit.check().is_err());
            assert_eq!(pending.len(), 1, "pipeline has not processed Off");
            probe.send(())?;
            assert!(cancelled.recv_timeout(LIVENESS)?, "speech was not interrupted in the callback");
            release.send(())?;
            pipeline.join().unwrap()?;
            // Constructor must not revive the epoch while the queued action waits.
            let mut settings = Settings::default(); settings.dry_run=false;
            let app = pipeline::Pipeline::new_with_tools(temp.path().join("data"), temp.path().into(), settings, None, true, tools.clone())?;
            assert!(!app.tools.agent_enabled.load(Ordering::Acquire));
            assert!(Arc::ptr_eq(&app.tools, &tools));
            assert!(!temp.path().join("must-not-play.wav").exists());
            tools.shutdown();
            println!("{action:?}: actual_callback_agent_off=true speech_interrupted=true pipeline_action_still_queued=true");
        }
        Ok(())
    }
    /// Before 22 Oct the Listening callback closed the shared stop latch: the agent's in-flight speech was cut and
    /// its permits revoked. Listening now has its own latch; the agent keeps talking with Speaking on.
    #[test]
    fn listening_off_callback_keeps_delivery_and_speech() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let tools = Arc::new(bridge::Tools::new(temp.path(), temp.path()));
        let permit = tools.agent_enabled.permit()?;
        let (started_tx, started) = bounded(1);
        let (probe, probe_rx) = bounded(1);
        let (cancelled_tx, cancelled) = bounded(1);
        tools.test_guarded_speech_queue(
            HeldSpeech { started: started_tx, probe: probe_rx, cancelled: cancelled_tx },
            speak::WavOutput::new(temp.path().join("never-written.wav")),
        );
        use agent::mcp::ToolHost;
        tools.speak("a reply while Listening goes off", "en", &permit)?;
        started.recv_timeout(Duration::from_secs(5))?;
        let (tx, rx) = unbounded();
        let callback = ui_action_callback(tools.clone(), tx, ui::Ui::handle());
        callback(ui::UiAction::SetListening(false));
        assert!(tools.stopped_for(agent::delivery::StopReason::Heimdall), "the Listening intent was not taken");
        assert!(tools.agent_enabled.load(Ordering::Acquire) && permit.check().is_ok(), "Listening off closed delivery");
        assert!(matches!(rx.try_recv()?, ui::UiAction::SetListening(false)));
        probe.send(())?;
        assert!(!cancelled.recv_timeout(Duration::from_secs(5))?, "Listening off interrupted the agent's speech");
        tools.shutdown();
        Ok(())
    }
