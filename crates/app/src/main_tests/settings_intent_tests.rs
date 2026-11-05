    use super::*;
    use agent::delivery::StopReason;
    fn mock_pi() -> PathBuf { Path::new(env!("CARGO_MANIFEST_DIR")).join("../agent/tests/fixtures/mock-adapter.py") }
    /// A processed menu action as the live loop handles it: Agent On's pre-flight answer arrives on the loop's
    /// internal channel and is handled before anything else is checked.
    fn act(app: &mut pipeline::Pipeline, a: ui::UiAction) -> Result<()> {
        app.action(a)?;
        let rx = app.internal_events();
        while app.preflight_pending() {
            let e = rx.recv_timeout(Duration::from_secs(15))?;
            app.internal(e)?;
        }
        Ok(())
    }
    type Launched<C> = (pipeline::Pipeline, settings::SettingsWatch, C, crossbeam_channel::Receiver<ui::UiAction>);
    /// main()'s order: settings file -> tools/latch -> callback -> pipeline. Persistence is real.
    fn launch(data: &Path, models: &Path, initial: Option<Settings>) -> Result<Launched<impl Fn(ui::UiAction)>> {
        if let Some(s) = initial { settings::private_dir(data)?; s.save(&data.join("settings.json"))?; }
        let s = Settings::load(&data.join("settings.json"))?;
        let tools = Arc::new(bridge::Tools::new(models, data));
        tools.initialize_delivery(&s);
        let (tx, rx) = unbounded();
        let callback = ui_action_callback(tools.clone(), tx, ui::Ui::handle());
        let mut app = pipeline::Pipeline::new_with_tools(data.into(), models.into(), s, None, true, tools)?;
        app.test_persist_settings();
        let mut watch = settings::SettingsWatch::default();
        assert!(reload_settings(&mut app, &mut watch).is_none(), "startup revision is not an intent");
        Ok((app, watch, callback, rx))
    }
    /// An external editor (or `heimdall settings`): load, change, save.
    fn edit(data: &Path, change: impl FnOnce(&mut Settings)) -> Result<()> {
        let mut s = Settings::load(&data.join("settings.json"))?; change(&mut s); s.save(&data.join("settings.json"))
    }
    /// Agent menu item == latch == persisted file == applied settings, for both reasons.
    fn agrees(app: &pipeline::Pipeline) -> Result<()> {
        let saved = Settings::load(&app.data.join("settings.json"))?;
        let tools = &app.tools;
        ensure!(app.menu_agent_off() == tools.stopped_for(StopReason::Agent) && saved.dry_run == app.menu_agent_off() && app.settings.dry_run == saved.dry_run,
            "Agent: menu_off={} file={} settings={}", app.menu_agent_off(), saved.dry_run, app.settings.dry_run);
        ensure!(saved.paused == tools.stopped_for(StopReason::Heimdall) && app.settings.paused == saved.paused,
            "Heimdall: latch_off={} file={} settings={}", tools.stopped_for(StopReason::Heimdall), saved.paused, app.settings.paused);
        ensure!(!tools.agent_enabled.stopped_for(StopReason::Heimdall), "Listening reached the agent's delivery gate");
        ensure!(!app.stop_settings_fresh(&saved), "persisted stop value not accepted");
        Ok(())
    }
    /// A fresh relaunch from the persisted file, exactly as main() builds delivery.
    fn relaunch_open(data: &Path) -> Result<bool> {
        let s = Settings::load(&data.join("settings.json"))?;
        let tools = bridge::Tools::new(data, data); tools.initialize_delivery(&s);
        Ok(tools.agent_enabled.load(Ordering::Acquire))
    }
    /// Whether a fresh relaunch starts with `reason` off. Listening off never closes the agent (`relaunch_open`).
    fn relaunch_stopped(data: &Path, reason: StopReason) -> Result<bool> {
        let s = Settings::load(&data.join("settings.json"))?;
        let tools = bridge::Tools::new(data, data); tools.initialize_delivery(&s);
        Ok(tools.stopped_for(reason))
    }
    fn pi_receives(gate: Arc<agent::delivery::DeliveryGate>, dir: &Path, text: &str) -> Result<bool> {
        use agent::harness::{Harness, Pi};
        tokio::runtime::Runtime::new()?.block_on(async {
            let mut pi = Pi::launch(&mock_pi(), &dir.join("pi.jsonl"), dir)?; pi.set_delivery_gate(gate);
            let sent = pi.send(agent::protocol::message("file-intent", text, &[], true, false)?).await;
            let received = sent.is_ok() && matches!(tokio::time::timeout(Duration::from_secs(5), pi.next()).await??,
                Some(agent::protocol::Event::Other(v)) if v["request"]["message"].as_str().is_some_and(|m| m.starts_with(&format!("[Heimdall delivery: file-intent]\n{}", agent::clock::NOW_PREFIX)) && m.ends_with(&format!("]\n{text}"))));
            pi.shutdown().await?; Ok(received)
        })
    }
    #[test]
    fn file_off_while_on_closes_latch_revokes_permits_and_blocks_real_pi() -> Result<()> {
        for reason in [StopReason::Agent, StopReason::Heimdall] {
            let temp = tempfile::tempdir()?; let data = temp.path().join("data");
            let (mut app, mut watch, _callback, _rx) = launch(&data, temp.path(), Some(Settings { dry_run: false, ..Default::default() }))?;
            let agent = matches!(reason, StopReason::Agent);
            let prior = app.tools.agent_enabled.permit()?;
            edit(&data, |s| if agent { s.dry_run = true } else { s.paused = true })?;
            assert!(reload_settings(&mut app, &mut watch).is_some());
            // Agent Off closes delivery; Listening Off closes only Listening.
            assert!(app.tools.stopped_for(reason) && app.tools.agent_enabled.load(Ordering::Acquire) != agent);
            assert_eq!(prior.check().is_err(), agent);
            assert_eq!(app.tools.agent_enabled.permit().is_err(), agent);
            assert_eq!(pi_receives(app.tools.agent_enabled.clone(), temp.path(), "private prompt after file Off")?, !agent);
            agrees(&app)?;
            assert!(reload_settings(&mut app, &mut watch).is_none());
            assert!(relaunch_stopped(&data, reason)?);
            assert_eq!(relaunch_open(&data)?, !agent);
            app.tools.shutdown();
        }
        Ok(())
    }
    #[test]
    fn file_on_while_off_opens_fresh_permits_and_survives_relaunch() -> Result<()> {
        for reason in [StopReason::Agent, StopReason::Heimdall] {
            let temp = tempfile::tempdir()?; let data = temp.path().join("data");
            let agent = matches!(reason, StopReason::Agent);
            let (mut app, mut watch, _callback, _rx) = launch(&data, temp.path(), Some(Settings { dry_run: agent, paused: !agent, ..Default::default() }))?;
            assert_eq!(app.tools.agent_enabled.permit().is_err(), agent, "Listening off at launch closed delivery");
            assert!(app.tools.stopped_for(reason));
            edit(&data, |s| if agent { s.dry_run = false } else { s.paused = false })?;
            bridge::test_harness(Some(mock_pi()));
            let applied = reload_settings(&mut app, &mut watch);
            bridge::test_harness(None);
            assert!(applied.is_some());
            assert!(app.tools.agent_enabled.permit().is_ok() && !app.tools.stopped_for(reason));
            assert!(pi_receives(app.tools.agent_enabled.clone(), temp.path(), "prompt after file On")?);
            agrees(&app)?;
            assert!(reload_settings(&mut app, &mut watch).is_none());
            assert!(relaunch_open(&data)? && !relaunch_stopped(&data, reason)?);
            app.finish()?;
        }
        Ok(())
    }
    #[test]
    fn callback_off_survives_relaunch_without_any_live_loop_processing() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let data = temp.path().join("data");
        let (app, _watch, callback, _rx) = launch(&data, temp.path(), Some(Settings { dry_run: false, ..Default::default() }))?;
        callback(ui::UiAction::SetDryRun(true));
        assert!(app.tools.agent_enabled.permit().is_err());
        // Simulate the watchdog relaunch barrier while the loop never reads its queue or saves settings.
        app.tools.flush_intent()?;
        assert!(!relaunch_open(&data)?, "the frozen loop relaunched Agent On");
        let saved = Settings::load(&data.join("settings.json"))?;
        let restored = Arc::new(bridge::Tools::new(temp.path(), &data));
        restored.initialize_delivery(&saved);
        let mut relaunched = pipeline::Pipeline::new_with_tools(data.clone(), temp.path().into(), saved, None, true, restored)?;
        relaunched.test_persist_settings();
        assert!(relaunched.settings.dry_run && relaunched.menu_agent_off());
        relaunched.save_settings()?;
        assert!(Settings::load(&data.join("settings.json"))?.dry_run);
        relaunched.tools.shutdown();
        callback(ui::UiAction::SetDryRun(false));
        app.tools.flush_intent()?;
        assert!(!app.tools.agent_enabled.stopped_for(StopReason::Agent), "a newer explicit On clears the current latch");
        assert!(!relaunch_open(&data)?, "an uncommitted On cannot override durable settings Off");
        edit(&data, |s| s.dry_run = false)?;
        assert!(relaunch_open(&data)?, "settings On plus a cleared stop overlay opens delivery");
        callback(ui::UiAction::SetDryRun(true));
        app.tools.flush_intent()?;
        assert!(!relaunch_open(&data)?);
        app.tools.shutdown();
        Ok(())
    }
    #[test]
    fn durable_settings_retire_only_the_same_intent_and_allow_later_launch_file_edits() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let data = temp.path().join("data");
        let (mut app, _watch, callback, rx) = launch(&data, temp.path(), Some(Settings { dry_run: false, ..Default::default() }))?;
        let earlier = app.tools.intent_generation();
        callback(ui::UiAction::SetDryRun(true));
        app.tools.intent_saved_in_settings(earlier)?;
        app.tools.flush_intent()?;
        assert!(data.join("stop-intent.json").is_file(), "an older save cannot retire a newer Off");
        act(&mut app, rx.try_recv()?)?;
        app.save_settings()?;
        assert!(!data.join("stop-intent.json").exists(), "settings durably hold the stop intent");
        // An explicit file edit made while the app is closed remains a launch authority.
        edit(&data, |s| s.dry_run = false)?;
        assert!(relaunch_open(&data)?);
        app.tools.shutdown();
        Ok(())
    }
    #[test]
    fn menu_intent_pending_wins_over_an_older_file_write_and_relaunches_off() -> Result<()> {
        for reason in [StopReason::Agent, StopReason::Heimdall] {
            let temp = tempfile::tempdir()?; let data = temp.path().join("data");
            let agent = matches!(reason, StopReason::Agent);
            let (mut app, mut watch, callback, rx) = launch(&data, temp.path(), Some(Settings { dry_run: agent, paused: !agent, ..Default::default() }))?;
            // An older external On lands, then the user's Off; the watcher reads before the action is processed.
            edit(&data, |s| if agent { s.dry_run = false } else { s.paused = false })?;
            callback(if agent { ui::UiAction::SetDryRun(true) } else { ui::UiAction::SetListening(false) });
            reload_settings(&mut app, &mut watch);
            assert!(app.tools.stopped_for(reason), "older file On overrode newer menu Off");
            assert_eq!(app.tools.agent_enabled.permit().is_err(), agent);
            agrees(&app)?; // the yielded value was rewritten to the effective intent
            assert!(relaunch_stopped(&data, reason)?, "crash before processing would relaunch On");
            match rx.try_recv()? { a @ ui::UiAction::SetDryRun(_) => act(&mut app, a)?, ui::UiAction::SetListening(_) => app.heimdall_intent_processed()?, a => panic!("{a:?}") }
            assert!(reload_settings(&mut app, &mut watch).is_none(), "own write echoed");
            assert_eq!(app.tools.agent_enabled.permit().is_err(), agent);
            agrees(&app)?;
            assert!(relaunch_stopped(&data, reason)?);
            assert_eq!(relaunch_open(&data)?, !agent);
            // Once processed, a later file On is fresh again.
            edit(&data, |s| if agent { s.dry_run = false } else { s.paused = false })?;
            bridge::test_harness(Some(mock_pi()));
            let applied = reload_settings(&mut app, &mut watch);
            bridge::test_harness(None);
            assert!(applied.is_some() && app.tools.agent_enabled.permit().is_ok());
            agrees(&app)?;
            app.finish()?;
        }
        Ok(())
    }
    #[test]
    fn app_own_writes_never_echo_back_as_intents() -> Result<()> {
        let temp = tempfile::tempdir()?; let data = temp.path().join("data");
        let (mut app, mut watch, callback, rx) = launch(&data, temp.path(), Some(Settings { dry_run: false, ..Default::default() }))?;
        let held = app.tools.agent_enabled.permit()?;
        for action in [ui::UiAction::SetSpeakReplies(false), ui::UiAction::SetSpeakReplies(true), ui::UiAction::SetLaunchAtLogin(false)] {
            callback(action); act(&mut app, rx.try_recv()?)?;
            assert!(reload_settings(&mut app, &mut watch).is_none(), "own write applied as a change");
            assert!(held.check().is_ok(), "own write bumped the latch generation");
            agrees(&app)?;
        }
        bridge::test_harness(Some(mock_pi()));
        for (action, reason, off) in [(ui::UiAction::SetDryRun(true), StopReason::Agent, true), (ui::UiAction::SetDryRun(false), StopReason::Agent, false), (ui::UiAction::SetListening(false), StopReason::Heimdall, true), (ui::UiAction::SetListening(true), StopReason::Heimdall, false)] {
            // Listening's intents never close the agent's delivery.
            let agent_off = off && matches!(reason, StopReason::Agent);
            callback(action);
            let held = app.tools.agent_enabled.permit().ok();
            match rx.try_recv()? { a @ ui::UiAction::SetDryRun(_) => act(&mut app, a)?, _ => app.heimdall_intent_processed()? }
            assert!(reload_settings(&mut app, &mut watch).is_none(), "own write applied as a change");
            assert_eq!(app.tools.agent_enabled.load(Ordering::Acquire), !agent_off);
            assert_eq!(app.tools.stopped_for(reason), off);
            if let Some(p) = held { assert!(p.check().is_ok(), "own write bumped the latch generation"); }
            agrees(&app)?;
            assert_eq!(relaunch_open(&data)?, !agent_off);
            assert_eq!(relaunch_stopped(&data, reason)?, off);
        }
        bridge::test_harness(None);
        app.finish()?;
        Ok(())
    }
    #[test]
    fn no_save_can_persist_on_while_the_latch_is_off() -> Result<()> {
        let temp = tempfile::tempdir()?; let data = temp.path().join("data");
        let (mut app, _watch, callback, rx) = launch(&data, temp.path(), Some(Settings { dry_run: false, ..Default::default() }))?;
        // Off is clicked behind an older queued action that saves first.
        callback(ui::UiAction::SetSpeakReplies(false)); callback(ui::UiAction::SetDryRun(true));
        act(&mut app, rx.try_recv()?)?;
        assert!(!app.settings.dry_run, "applied settings lag the queued Off");
        assert!(Settings::load(&data.join("settings.json"))?.dry_run && !relaunch_open(&data)?);
        // Internal settings paths cannot reopen either.
        let mut next = app.settings.clone(); next.dry_run = false; app.apply_menu_settings(next)?; app.save_settings()?;
        assert!(!relaunch_open(&data)?);
        act(&mut app, rx.try_recv()?)?;
        agrees(&app)?;
        // Explicit On -> save -> relaunch On.
        callback(ui::UiAction::SetDryRun(false));
        bridge::test_harness(Some(mock_pi()));
        let processed = act(&mut app, rx.try_recv()?);
        bridge::test_harness(None);
        processed?;
        agrees(&app)?;
        assert!(relaunch_open(&data)?);
        app.finish()?;
        Ok(())
    }
    /// Writes settings.json directly (no temp file), as an editor that saves in place.
    fn edit_in_place(data: &Path, change: impl FnOnce(&mut Settings)) -> Result<()> {
        let mut s = Settings::load(&data.join("settings.json"))?; change(&mut s);
        std::fs::write(data.join("settings.json"), serde_json::to_vec_pretty(&s)?)?; Ok(())
    }
    #[test]
    fn failed_stop_save_is_retried_and_never_reopens_on_a_later_edit_or_relaunch() -> Result<()> {
        for reason in [StopReason::Agent, StopReason::Heimdall] {
            let temp = tempfile::tempdir()?; let data = temp.path().join("data");
            let agent = matches!(reason, StopReason::Agent);
            let (mut app, mut watch, callback, rx) = launch(&data, temp.path(), Some(Settings { dry_run: false, ..Default::default() }))?;
            // Saves fail: the atomic write's temp path is occupied.
            let blocker = data.join("settings.json.tmp"); std::fs::create_dir(&blocker)?;
            callback(if agent { ui::UiAction::SetDryRun(true) } else { ui::UiAction::SetListening(false) });
            match rx.try_recv()? { a @ ui::UiAction::SetDryRun(_) => act(&mut app, a)?, _ => if let Err(e) = app.heimdall_intent_processed() { app.note_save_failure(&e) } }
            app.tools.flush_intent()?;
            assert!(app.tools.stopped_for(reason) && relaunch_stopped(&data, reason)?, "durable callback intent must survive the failed settings save");
            // An unrelated in-place edit keeps the stale On; it is not a fresh intent.
            edit_in_place(&data, |s| s.speak_replies = false)?;
            reload_settings(&mut app, &mut watch);
            assert!(app.tools.stopped_for(reason), "stale file On reopened after a failed save");
            assert_eq!(app.tools.agent_enabled.permit().is_err(), agent);
            // The disk recovers: the next tick writes the effective Off without any new event.
            std::fs::remove_dir(&blocker)?;
            reload_settings(&mut app, &mut watch);
            assert!(Settings::load(&data.join("settings.json")).map(|s| if agent { s.dry_run } else { s.paused })?, "failed stop save was never retried");
            assert!(relaunch_stopped(&data, reason)?);
            assert_eq!(relaunch_open(&data)?, !agent);
            assert!(reload_settings(&mut app, &mut watch).is_none(), "retried write echoed");
            assert!(!app.settings.speak_replies);
            agrees(&app)?;
            app.finish()?;
        }
        Ok(())
    }
    #[test]
    fn failed_settings_save_keeps_the_choice_and_lands_once_unblocked() -> Result<()> {
        let temp = tempfile::tempdir()?; let data = temp.path().join("data");
        let (mut app, mut watch, _callback, _rx) = launch(&data, temp.path(), Some(Settings { dry_run: false, ..Default::default() }))?;
        let held = app.tools.agent_enabled.permit()?;
        let seen = capture::MicrophoneState { devices: vec![capture::InputDevice { uid: "usb-mic".into(), name: "USB Mic".into(), built_in: false, default: false }], ..Default::default() };
        let blocker = data.join("settings.json.tmp"); std::fs::create_dir(&blocker)?;
        // The live loop's Microphone ▸ choice, twice while the disk refuses the write.
        for _ in 0..2 { app.settings.choose_microphone("usb-mic", &seen); app.save_or_note(); }
        assert_eq!(app.settings.microphone.as_ref().map(|m| m.uid.as_str()), Some("usb-mic"), "applied choice dropped");
        assert!(Settings::load(&data.join("settings.json"))?.microphone.is_none());
        reload_settings(&mut app, &mut watch);
        assert!(Settings::load(&data.join("settings.json"))?.microphone.is_none());
        std::fs::remove_dir(&blocker)?;
        reload_settings(&mut app, &mut watch);
        assert_eq!(Settings::load(&data.join("settings.json"))?.microphone.map(|m| m.uid), Some("usb-mic".into()), "failed save was never retried");
        assert!(reload_settings(&mut app, &mut watch).is_none(), "retried write echoed");
        assert!(held.check().is_ok(), "retry touched the latch");
        agrees(&app)?;
        app.finish()?;
        Ok(())
    }
    #[test]
    fn external_off_survives_an_app_save_before_the_watcher_reads_it() -> Result<()> {
        for reason in [StopReason::Agent, StopReason::Heimdall] {
            let temp = tempfile::tempdir()?; let data = temp.path().join("data");
            let agent = matches!(reason, StopReason::Agent);
            let (mut app, mut watch, callback, rx) = launch(&data, temp.path(), Some(Settings { dry_run: false, ..Default::default() }))?;
            let prior = app.tools.agent_enabled.permit()?;
            edit(&data, |s| if agent { s.dry_run = true } else { s.paused = true })?;
            // Inside the poll window the app saves an unrelated menu change.
            callback(ui::UiAction::SetSpeakReplies(false)); act(&mut app, rx.try_recv()?)?;
            assert!(app.tools.stopped_for(reason), "external Off lost to the app's save");
            assert_eq!(prior.check().is_err(), agent);
            assert_eq!(app.tools.agent_enabled.permit().is_err(), agent);
            assert!(relaunch_stopped(&data, reason)?, "app save overwrote the external Off");
            assert_eq!(relaunch_open(&data)?, !agent);
            // The watcher then applies the rest of the Off (agent restart, listening) from the app's write.
            bridge::test_harness(Some(mock_pi()));
            let applied = reload_settings(&mut app, &mut watch);
            bridge::test_harness(None);
            let old = applied.expect("Off reached the applied settings");
            assert_eq!((old.dry_run, old.paused), (false, false));
            agrees(&app)?;
            assert!(reload_settings(&mut app, &mut watch).is_none(), "own write echoed");
            // Menu On still works afterwards.
            callback(if agent { ui::UiAction::SetDryRun(false) } else { ui::UiAction::SetListening(true) });
            bridge::test_harness(Some(mock_pi()));
            let processed = match rx.try_recv()? { a @ ui::UiAction::SetDryRun(_) => act(&mut app, a), _ => app.heimdall_intent_processed() };
            bridge::test_harness(None);
            processed?;
            assert!(app.tools.agent_enabled.permit().is_ok() && relaunch_open(&data)? && !relaunch_stopped(&data, reason)?);
            agrees(&app)?;
            app.finish()?;
        }
        Ok(())
    }
    struct NoTools;
    #[async_trait::async_trait]
    impl agent::mcp::ToolHost for NoTools {
        fn speak(&self, _: &str, _: &str, _: &agent::delivery::DeliveryPermit) -> Result<u64> { anyhow::bail!("unused") }
        async fn screenshot_now(&self, _: &agent::delivery::DeliveryPermit) -> Result<agent::mcp::ScreenshotResult> { anyhow::bail!("unused") }
    }
    struct HeldArchive { began: Arc<tokio::sync::Notify>, release: Arc<tokio::sync::Notify> }
    #[async_trait::async_trait]
    impl agent::mcp::TranscriptSearch for HeldArchive {
        async fn timeline(&self, _: serde_json::Value, p: &agent::delivery::DeliveryPermit) -> Result<serde_json::Value> {
            self.began.notify_one(); self.release.notified().await; p.check()?;
            Ok(serde_json::json!({"text":"private-MCP-sentinel"}))
        }
    }
    #[test]
    fn file_off_suppresses_a_held_mcp_response() -> Result<()> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let temp = tempfile::tempdir()?; let data = temp.path().join("data");
        let (mut app, mut watch, _callback, _rx) = launch(&data, temp.path(), Some(Settings { dry_run: false, ..Default::default() }))?;
        let gate = app.tools.agent_enabled.clone();
        let response = tokio::runtime::Runtime::new()?.block_on(async {
            let began = Arc::new(tokio::sync::Notify::new()); let release = Arc::new(tokio::sync::Notify::new());
            let server = agent::mcp::McpServer::start(Arc::new(NoTools), Arc::new(HeldArchive { began: began.clone(), release: release.clone() }), gate).await?;
            let addr = server.url.strip_prefix("http://").unwrap().strip_suffix("/mcp").unwrap().to_owned();
            let token = server.token.clone();
            let request = tokio::spawn(async move {
                let mut stream = tokio::net::TcpStream::connect(&addr).await?;
                let body = serde_json::json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"timeline_search","arguments":{"query":"fixture"}}}).to_string();
                let header = format!("POST /mcp HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {token}\r\nAccept: application/json, text/event-stream\r\nMCP-Protocol-Version: 2025-06-18\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                stream.write_all(header.as_bytes()).await?; stream.write_all(body.as_bytes()).await?;
                let mut bytes = vec![]; stream.read_to_end(&mut bytes).await?; Ok::<_, anyhow::Error>(bytes)
            });
            tokio::time::timeout(Duration::from_secs(5), began.notified()).await?;
            edit(&data, |s| s.dry_run = true)?;
            ensure!(reload_settings(&mut app, &mut watch).is_some());
            release.notify_one();
            let response = tokio::time::timeout(Duration::from_secs(5), request).await???;
            server.shutdown().await;
            Ok::<_, anyhow::Error>(String::from_utf8_lossy(&response).into_owned())
        })?;
        assert!(!response.contains("private-MCP-sentinel"), "held MCP result escaped after file Off");
        agrees(&app)?;
        app.tools.shutdown();
        Ok(())
    }
    #[test]
    fn file_off_cancels_a_busy_actor_send_before_its_restart() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir()?; let data = temp.path().join("data");
        let fixture = temp.path().join("held-pi.py");
        let python = include_str!("../../tests/fixtures/blocked-pi.py").replace("p=Path.cwd()", "p=Path.cwd()\nfirst=os.read(sys.stdin.fileno(),1)\nassert first\n(p/'write-progress.marker').write_bytes(first)");
        std::fs::write(&fixture, python)?; std::fs::set_permissions(&fixture, std::fs::Permissions::from_mode(0o700))?;
        let (mut app, mut watch, _callback, _rx) = launch(&data, temp.path(), Some(Settings { dry_run: false, ..Default::default() }))?;
        bridge::test_harness(Some(fixture)); let started = act(&mut app, ui::UiAction::RestartSession); bridge::test_harness(None); started?;
        app.test_agent().send(bridge::Command::Observe(agent::protocol::Observation { id: "busy-private".into(), timestamp_ms: 1, source: "mic".into(), speaker: "User".into(), language: "en".into(), text: "a".repeat(256 * 1024) }))?;
        app.test_agent().barrier()?;
        app.test_agent().send(bridge::Command::Wake { images: vec![], urgent: false })?;
        let began = Instant::now();
        while !data.join("write-progress.marker").exists() { ensure!(began.elapsed() < Duration::from_secs(5), "fixture did not hold the send"); std::thread::sleep(Duration::from_millis(20)); }
        edit(&data, |s| s.dry_run = true)?;
        let applied = reload_settings(&mut app, &mut watch).is_some();
        let closed = !app.tools.agent_enabled.load(Ordering::Acquire);
        // Release the child before any assertion, so a failure cannot hang in teardown.
        std::fs::write(data.join("off.marker"), b"off")?; std::fs::write(data.join("release.marker"), b"release")?;
        app.test_agent().barrier()?;
        assert!(applied && closed, "file Off did not close the latch");
        let records = heimdall_archive::Journal::open_read_only(&app.session)?.records()?;
        assert!(!records.iter().any(|r| r.kind == "agent_transport_sent"), "busy actor completed the private send after file Off");
        assert!(records.iter().any(|r| r.kind == "agent_transport_cancelled"), "send was not cancelled");
        assert!(!data.join("received.json").exists());
        agrees(&app)?;
        app.finish()?;
        Ok(())
    }
    fn engine_tile(on: bool) -> ui::EngineState {
        if on { ui::EngineState::Ready } else { ui::EngineState::Off }
    }
    fn tile_on(app: &pipeline::Pipeline, tile: ui::Tile) -> bool {
        let listen = if app.settings.paused { ui::ListenState::Paused } else { ui::ListenState::Listening };
        tile.is_on(&listen, &app.runtime_settings_value(), &app.ui_settings_value(), &engine_tile(app.settings.engine_on))
    }
    fn saved_tile_on(data: &Path, tile: ui::Tile) -> Result<bool> {
        let s = Settings::load(&data.join("settings.json"))?;
        let listen = if s.paused { ui::ListenState::Paused } else { ui::ListenState::Listening };
        let runtime = ui::RuntimeSettings { dry_run: s.dry_run, seeing: !s.screen_off, ..Default::default() };
        Ok(tile.is_on(&listen, &runtime, &ui::Settings { speak_replies: s.speak_replies, launch_at_login: s.launch_at_login, voice_speed_percent: s.voice_speed_percent, engine_thinking: s.engine_thinking_level().into(), wake_sound: s.wake_sound, ..Default::default() }, &engine_tile(s.engine_on)))
    }
    /// Each tile's click goes through the callback and the live loop's handling, flips its setting, persists it
    /// (a relaunch shows the same tile state) and keeps the stop latch in agreement; a second click restores it.
    #[test]
    fn every_tile_round_trips_through_settings_and_relaunch() -> Result<()> {
        let temp = tempfile::tempdir()?; let data = temp.path().join("data");
        // The test pipeline never starts the engine manager: Engine's click only flips and persists its setting.
        // The local engine's model is the agent's here: an API selection keeps the Engine tile off.
        let (mut app, _watch, callback, rx) = launch(&data, temp.path(), Some(Settings { dry_run: false, setup_requested: true, engine_on: true, agent_model: agent::provider::AgentModel::Local, ..Default::default() }))?;
        bridge::test_harness(Some(mock_pi()));
        for tile in ui::Tile::ALL {
            assert!(tile_on(&app, tile), "{tile:?} starts on");
            for _ in 0..2 {
                let before = tile_on(&app, tile);
                callback(tile.toggle(before));
                match rx.try_recv()? {
                    ui::UiAction::SetListening(_) => app.heimdall_intent_processed()?,
                    ui::UiAction::SetSeeing(on) => app.set_seeing(on)?,
                    a => act(&mut app, a)?,
                }
                assert_eq!(tile_on(&app, tile), !before, "{tile:?} in the running app");
                assert_eq!(saved_tile_on(&data, tile)?, !before, "{tile:?} after relaunch");
                agrees(&app)?;
            }
        }
        bridge::test_harness(None);
        app.finish()?;
        Ok(())
    }
    /// Setup's Allow All orders On at the click; an Off clicked while the permission dialogs are up is newer and
    /// wins over the dialogs' completion, which carries no intent. Without an Off, Allow All turns Heimdall on.
    #[test]
    fn allow_all_completion_never_overrides_a_later_off() -> Result<()> {
        let temp = tempfile::tempdir()?; let data = temp.path().join("data");
        let (mut app, _watch, callback, rx) = launch(&data, temp.path(), Some(Settings { paused: true, ..Default::default() }))?;
        let process = |app: &mut pipeline::Pipeline| -> Result<()> {
            while let Ok(action) = rx.try_recv() {
                match action {
                    ui::UiAction::SetupAllowed | ui::UiAction::SetListening(_) => app.heimdall_intent_processed()?,
                    ui::UiAction::RequestSystemAudio => app.setup_requested()?,
                    a => panic!("{a:?}"),
                }
            }
            Ok(())
        };
        callback(ui::UiAction::SetupAllowed);
        assert!(!app.tools.stopped_for(StopReason::Heimdall), "Allow All is On at the click");
        callback(ui::UiAction::SetListening(false));
        callback(ui::UiAction::RequestSystemAudio);
        assert!(app.tools.stopped_for(StopReason::Heimdall), "the dialogs' completion overrode Off");
        process(&mut app)?;
        assert!(app.settings.paused && app.settings.setup_requested);
        agrees(&app)?;
        assert!(Settings::load(&data.join("settings.json"))?.paused);
        // The same completion processed before the Off: still Off.
        callback(ui::UiAction::SetupAllowed);
        callback(ui::UiAction::RequestSystemAudio);
        callback(ui::UiAction::SetListening(false));
        process(&mut app)?;
        assert!(app.settings.paused);
        agrees(&app)?;
        // No Off: Allow All leaves Heimdall on.
        callback(ui::UiAction::SetupAllowed);
        callback(ui::UiAction::RequestSystemAudio);
        process(&mut app)?;
        assert!(!app.settings.paused && !app.tools.stopped_for(StopReason::Heimdall));
        agrees(&app)?;
        app.finish()?;
        Ok(())
    }
    /// Seeing off stops the stream owner and frees every buffered frame; the screenshot tool then answers that
    /// screen capture is unavailable, without naming the switch. A settings-file edit turns it back on (the live loop
    /// reconciles the stream).
    #[test]
    fn seeing_applies_even_when_the_settings_save_fails() -> Result<()> {
        let temp = tempfile::tempdir()?; let data = temp.path().join("data");
        let (mut app, _watch, _callback, _rx) = launch(&data, temp.path(), Some(Settings { dry_run: false, setup_requested: true, ..Default::default() }))?;
        std::fs::create_dir_all(data.join("settings.json.tmp"))?;
        // The live loop reconciles the stream only when set_seeing returns Ok.
        app.set_seeing(false)?;
        assert!(app.settings.screen_off && app.tools.screen_off.load(Ordering::Acquire));
        assert!(!Settings::load(&data.join("settings.json"))?.screen_off, "save was blocked");
        std::fs::remove_dir(data.join("settings.json.tmp"))?;
        app.persist_settings()?;
        assert!(Settings::load(&data.join("settings.json"))?.screen_off, "retry did not land");
        app.finish()?;
        Ok(())
    }
    /// 24 Oct 02:31: Seeing on and off ran ScreenCaptureKit and the archive's lock waits on the live loop. A start
    /// still running neither holds the loop nor a second start, Seeing off cancels it, and a failed one is reported.
    #[test]
    fn a_slow_seeing_on_never_holds_the_live_loop() -> Result<()> {
        let temp = tempfile::tempdir()?; let data = temp.path().join("data");
        let (mut app, _watch, _callback, _rx) = launch(&data, temp.path(), Some(Settings { dry_run: false, setup_requested: true, ..Default::default() }))?;
        let (tx, rx) = crossbeam_channel::bounded(1);
        app.screen_starting = Some((rx, String::new(), Default::default()));
        let mut screens = None;
        let started = std::time::Instant::now();
        adopt_screen(&mut app, &mut screens, Duration::ZERO)?;
        start_screen(&mut app, &mut screens)?;
        assert!(started.elapsed() < Duration::from_millis(500), "waited {:?}", started.elapsed());
        assert!(app.screen_starting.is_some() && screens.is_none(), "one start at a time, still running");
        stop_screen(&mut app, &mut screens, "Seeing off");
        assert!(app.screen_starting.is_none());
        // A cancelled start's result is taken off the live loop, by the lane.
        let _ = tx.send(Err(anyhow::anyhow!("late")));
        if let Some(lane) = app.screen_lane.take() { lane.join().unwrap(); }
        assert!(screens.is_none() && app.screen_starting.is_none());
        let (tx, rx) = crossbeam_channel::bounded(1);
        app.screen_starting = Some((rx, String::new(), Default::default()));
        tx.send(Err(anyhow::anyhow!("no display")))?;
        assert!(adopt_screen(&mut app, &mut screens, Duration::ZERO).is_err(), "a failed start is reported");
        assert!(app.screen_starting.is_none() && screens.is_none());
        // A stop stuck behind the archive lock: Seeing on and off again return at once, and the queued start,
        // cancelled before its turn, finishes without the live loop once the stop does.
        let (release, held) = crossbeam_channel::bounded::<()>(0);
        app.screen_lane = Some(std::thread::spawn(move || { let _ = held.recv(); }));
        let started = std::time::Instant::now();
        start_screen(&mut app, &mut screens)?;
        stop_screen(&mut app, &mut screens, "Seeing off");
        // The stop is held for good: returning at all proves nothing waited for it. Seeing on itself waits
        // SCREEN_START_WAIT; the margin covers a loaded test machine's evidence fsyncs.
        assert!(started.elapsed() < SCREEN_START_WAIT + Duration::from_secs(10), "waited {:?} behind a stuck stop", started.elapsed());
        assert!(app.screen_lane.as_ref().is_some_and(|l| !l.is_finished()), "the lane waits its turn");
        drop(release);
        let lane = app.screen_lane.take().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !lane.is_finished() { assert!(std::time::Instant::now() < deadline, "the lane never finished"); std::thread::sleep(Duration::from_millis(10)); }
        lane.join().unwrap();
        assert!(screens.is_none() && app.screen_starting.is_none());
        app.finish()?;
        Ok(())
    }
    /// 26 Oct 23:01: Seeing off built an empty recording store on the live loop (2 s of fsync behind a held archive
    /// lock). The buffer it leaves is the store-less placeholder.
    #[test]
    fn seeing_off_leaves_a_buffer_without_a_store() -> Result<()> {
        let temp = tempfile::tempdir()?; let data = temp.path().join("data");
        let (mut app, _watch, _callback, _rx) = launch(&data, temp.path(), Some(Settings { dry_run: false, setup_requested: true, ..Default::default() }))?;
        let mut screens = None;
        stop_screen(&mut app, &mut screens, "Seeing off");
        assert!(app.screen_buffer.lock().unwrap().is_idle(), "Seeing off opened a store on the live loop");
        if let Some(lane) = app.screen_lane.take() { lane.join().unwrap(); }
        app.finish()?;
        Ok(())
    }
    #[test]
    fn seeing_off_stops_capture_and_frees_frames() -> Result<()> {
        use agent::mcp::ToolHost;
        let temp = tempfile::tempdir()?; let data = temp.path().join("data");
        let (mut app, mut watch, _callback, _rx) = launch(&data, temp.path(), Some(Settings { dry_run: false, setup_requested: true, ..Default::default() }))?;
        let display = screen::DisplayInfo { id: 1, x: 0., y: 0., width: 1, height: 1 };
        app.screen_buffer.lock().unwrap().push(screen::Frame::new(display, 1_000, 1, 1, vec![1, 2, 3])?)?;
        *app.tools.buffer.lock().unwrap() = Some(app.screen_buffer.clone());
        let mut screens = None;
        let permissions = LivePermissions { screen: true, ..Default::default() };
        let buffer = app.screen_buffer.clone();
        app.screen_holds.pin(&buffer, "mic", 1_500, Some(1_000), 2_000);
        assert_eq!(app.screen_holds.len(), 1);
        app.set_seeing(false)?;
        reconcile_screen(&mut app, &mut screens, true, permissions);
        assert!(screens.is_none() && app.tools.buffer.lock().unwrap().is_none());
        assert!(app.screen_holds.is_empty(), "a held command frame survived Seeing off");
        assert_eq!(app.screen_buffer.lock().unwrap().frame_count(), 0, "a frame survived Seeing off");
        assert!(Settings::load(&data.join("settings.json"))?.screen_off);
        let runtime = tokio::runtime::Builder::new_current_thread().build()?;
        let shot = runtime.block_on(app.tools.screenshot_now(&app.tools.agent_enabled.permit()?))?;
        assert!(shot.images.is_empty() && shot.notes == [bridge::SCREEN_OFF]);
        // A later relaunch or file edit to Seeing on reaches the app as a change for the live loop.
        edit(&data, |s| s.screen_off = false)?;
        let old = reload_settings(&mut app, &mut watch).expect("file edit applied");
        assert!(old.screen_off && !app.settings.screen_off && !app.tools.screen_off.load(Ordering::Acquire));
        app.finish()?;
        Ok(())
    }
