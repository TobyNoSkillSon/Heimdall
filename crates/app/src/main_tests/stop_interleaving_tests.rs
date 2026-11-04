    use super::*;
    use agent::delivery::StopReason;
    use agent::mcp::ToolHost;
    use std::sync::atomic::AtomicUsize;
    use tokio::io::AsyncReadExt;
    use std::future::Future;
    // A real AsyncWrite is polled before scheduling any callback, then held at
    // its writable boundary. This models a pipe waiting for capacity.
    struct HeldWrite {
        ready: Arc<std::sync::atomic::AtomicBool>,
        bytes: Arc<std::sync::Mutex<Vec<u8>>>,
    }
    impl tokio::io::AsyncWrite for HeldWrite {
        fn poll_write(self: std::pin::Pin<&mut Self>, _: &mut std::task::Context<'_>, bytes: &[u8]) -> std::task::Poll<std::io::Result<usize>> {
            if !self.ready.load(Ordering::Acquire) { return std::task::Poll::Pending; }
            self.bytes.lock().unwrap().extend_from_slice(bytes);
            std::task::Poll::Ready(Ok(bytes.len()))
        }
        fn poll_flush(self: std::pin::Pin<&mut Self>, _: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Result<()>> { std::task::Poll::Ready(Ok(())) }
        fn poll_shutdown(self: std::pin::Pin<&mut Self>, _: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Result<()>> { std::task::Poll::Ready(Ok(())) }
    }

    struct HeldSynth {
        started: crossbeam_channel::Sender<()>,
        release: crossbeam_channel::Receiver<()>,
    }
    impl speak::Synthesizer for HeldSynth {
        fn synthesize(&mut self, _: &str, _: &str, _: &dyn Fn() -> bool) -> Result<speak::Audio> {
            self.started.send(())?;
            self.release.recv_timeout(Duration::from_secs(2))?;
            Ok(speak::Audio { samples: vec![0.25; 4], sample_rate: 16000, first_audio_ms: 0., synthesis_ms: 1. })
        }
    }
    struct AudioCounter(Arc<AtomicUsize>);
    impl speak::AudioOutput for AudioCounter {
        fn output(&mut self, _: &speak::Audio, cancel: &dyn Fn() -> bool) -> Result<()> {
            if !cancel() { self.0.fetch_add(1, Ordering::SeqCst); }
            Ok(())
        }
    }
    fn permutations(values: &mut [usize], at: usize, out: &mut Vec<Vec<usize>>) {
        if at == values.len() { out.push(values.to_vec()); return; }
        for i in at..values.len() {
            values.swap(at, i);
            permutations(values, at + 1, out);
            values.swap(at, i);
        }
    }
    #[derive(Default)]
    struct OrderTally { iterations: usize, file_applied: usize, file_yielded: usize, callbacks: usize, max_callback: Duration }
    /// Runs the given (reason, order) cases on one pipeline of its own. Every case resets the
    /// tools, settings and persisted stop values first, so cases are independent of each other
    /// and of which worker runs them.
    fn run_stop_orders(cases: &[(usize, &[usize])]) -> Result<OrderTally> {
        let temp = tempfile::tempdir()?;
        let mut settings = Settings::default(); settings.dry_run = false;
        let mut app = pipeline::Pipeline::new(temp.path().join("data"), temp.path().into(), settings.clone(), None, true)?;
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
        let reasons = [
            (StopReason::Agent, ui::UiAction::SetDryRun(true), ui::UiAction::SetDryRun(false)),
            (StopReason::Heimdall, ui::UiAction::SetListening(false), ui::UiAction::SetListening(true)),
            (StopReason::Quit, ui::UiAction::Quit, ui::UiAction::SetDryRun(false)),
        ];
        let mut file_applied = 0;
        let mut file_yielded = 0;
        let mut callbacks = 0;
        let mut max_callback = Duration::ZERO;
        for &(r, order) in cases {
            let (reason, off, on) = (reasons[r].0, &reasons[r].1, &reasons[r].2);
            {
                let tools = Arc::new(bridge::Tools::new(temp.path(), temp.path()));
                app.tools = tools.clone();
                app.settings = settings.clone();
                app.save_settings()?; // persisted stop values: On
                // An external settings write made before either callback; the watcher reads it at event 6.
                let mut older_file = settings.clone();
                if matches!(reason, StopReason::Heimdall) { older_file.paused = true; } else { older_file.dry_run = true; }
                let permit = tools.agent_enabled.permit()?;
                let (tx, rx) = unbounded();
                let callback = ui_action_callback(tools.clone(), tx, ui::Ui::handle());
                // Issued before either callback; only its processing is shuffled.
                callback(ui::UiAction::SetSpeakReplies(true));
                let (started_tx, started) = bounded(1);
                let (release, held) = bounded(1);
                let audio = Arc::new(AtomicUsize::new(0));
                tools.test_guarded_speech_queue(HeldSynth { started: started_tx, release: held }, AudioCounter(audio.clone()));
                tools.speak("held private speech", "en", &permit)?;
                started.recv_timeout(Duration::from_secs(2))?;
                let send_ready = Arc::new(std::sync::atomic::AtomicBool::new(false));
                let sent_bytes = Arc::new(std::sync::Mutex::new(vec![]));
                let mut writer = HeldWrite { ready: send_ready.clone(), bytes: sent_bytes.clone() };
                let mut send = Box::pin(permit.write(&mut writer, b"in-flight private prompt"));
                let mcp_ready = std::sync::atomic::AtomicBool::new(false);
                let executed = AtomicUsize::new(0);
                let mut mcp = Box::pin(permit.run(async {
                    std::future::poll_fn(|_| if mcp_ready.load(Ordering::Acquire) {
                        std::task::Poll::Ready(())
                    } else { std::task::Poll::Pending }).await;
                    permit.with(|| { executed.fetch_add(1, Ordering::SeqCst); Ok("private MCP result") })
                }));
                let mut context = std::task::Context::from_waker(std::task::Waker::noop());
                assert!(send.as_mut().poll(&mut context).is_pending());
                assert!(mcp.as_mut().poll(&mut context).is_pending());
                // Both operations and synthesis are now genuinely in flight.
                // Expected stop bits; the file names Heimdall in its row, Agent otherwise. Listening (Heimdall) has
                // its own latch: its intents never revoke delivery, speech or MCP work.
                let (mut agent_off, mut heimdall_off, mut quit) = (false, false, false);
                let mut menu_for_file_reason = false;
                let mut speech_interrupted = false;
                let mut invalidated = false;
                for event in order {
                    match event {
                        0 => {
                            app.action(rx.recv()?)?;
                            assert_eq!(tools.agent_enabled.load(Ordering::Acquire), !agent_off && !quit,
                                "settings revived latch: {order:?}");
                        },
                        1 | 2 => {
                            let begin = Instant::now();
                            callback(if *event == 1 { off.clone() } else { on.clone() });
                            let elapsed = begin.elapsed();
                            max_callback = max_callback.max(elapsed);
                            callbacks += 1;
                            assert!(elapsed < Duration::from_millis(50), "callback waited: {elapsed:?}");
                            invalidated |= !matches!(reason, StopReason::Heimdall);
                            match (reason, *event) {
                                (StopReason::Agent, e) => { agent_off = e == 1; menu_for_file_reason = true; },
                                (StopReason::Heimdall, e) => { heimdall_off = e == 1; menu_for_file_reason = true; },
                                (StopReason::Quit, 1) => quit = true,
                                (StopReason::Quit, _) => { agent_off = false; menu_for_file_reason = true; },
                            }
                        },
                        6 => {
                            // The older write is fresh but yields to any newer menu intent for its reason.
                            let applied = (app.settings.dry_run, app.settings.paused);
                            app.apply_settings(older_file.clone())?;
                            // Settings that now follow a closed Agent latch interrupt queued speech; Listening's never do.
                            speech_interrupted |= applied.0 != app.settings.dry_run;
                            if menu_for_file_reason { file_yielded += 1; } else {
                                file_applied += 1;
                                if matches!(reason, StopReason::Heimdall) { heimdall_off = true; } else { agent_off = true; invalidated = true; }
                            }
                            assert_eq!(tools.stopped_for(StopReason::Agent), agent_off, "file Agent: {order:?}");
                            assert_eq!(tools.stopped_for(StopReason::Heimdall), heimdall_off, "file Heimdall: {order:?}");
                            assert!(!tools.agent_enabled.stopped_for(StopReason::Heimdall), "Listening reached the delivery gate: {order:?}");
                            assert_eq!((app.settings.dry_run, app.settings.paused), (agent_off, heimdall_off), "applied settings != latch: {order:?}");
                            assert_eq!(app.menu_agent_off(), agent_off);
                            assert!(!app.stop_settings_fresh(&Settings { dry_run: agent_off, paused: heimdall_off, ..settings.clone() }), "persisted != latch: {order:?}");
                        },
                        3 => {
                            send_ready.store(true, Ordering::Release);
                            let delivered = runtime.block_on(&mut send).is_ok();
                            assert_eq!(delivered, !invalidated, "send: {order:?}");
                            assert_eq!(sent_bytes.lock().unwrap().is_empty(), invalidated, "bytes: {order:?}");
                        },
                        4 => {
                            // MCP is already awaiting its held host future; its
                            // execution/result polls use the production run/with.
                            mcp_ready.store(true, Ordering::Release);
                            let result = runtime.block_on(&mut mcp);
                            assert_eq!(result.is_ok(), !invalidated, "MCP execution: {order:?}");
                            let response = permit.with(|| Ok("ready private MCP body"));
                            assert_eq!(response.is_ok(), !invalidated, "MCP response: {order:?}");
                            assert_eq!(executed.load(Ordering::SeqCst), usize::from(!invalidated));
                        },
                        5 => {
                            release.send(())?;
                            // A file Off also interrupts the queue, which then reports no event.
                            if !speech_interrupted { tools.test_finish_speech()?; }
                            tools.shutdown(); // deterministic join of held synthesis
                            assert_eq!(audio.load(Ordering::SeqCst), usize::from(!invalidated), "audio: {order:?}");
                        },
                        _ => unreachable!(),
                    }
                    let open = !agent_off && !quit;
                    assert_eq!(tools.agent_enabled.load(Ordering::Acquire), open, "latch: {order:?}");
                    assert_eq!(tools.agent_enabled.permit().is_ok(), open, "fresh permit: {order:?}");
                    assert_eq!(tools.stopped_for(StopReason::Heimdall), heimdall_off, "Listening latch: {order:?}");
                    assert_eq!(permit.check().is_err(), invalidated, "in-flight permit: {order:?}");
                }
                // In-flight work remains revoked even if a later On was issued; Listening never revoked it.
                assert_eq!(permit.check().is_err(), !matches!(reason, StopReason::Heimdall));
                if !matches!(reason, StopReason::Quit) {
                    callback(on.clone()); // explicitly newer than the final Off
                    runtime.block_on(async {
                        let (mut writer, mut reader) = tokio::io::duplex(64);
                        tools.agent_enabled.write(&mut writer, b"fresh prompt after On").await?;
                        drop(writer);
                        let mut received = vec![];
                        reader.read_to_end(&mut received).await?;
                        assert_eq!(received, b"fresh prompt after On");
                        Ok::<_, anyhow::Error>(())
                    })?;
                } else { assert!(!tools.agent_enabled.load(Ordering::Acquire)); }
            }
        }
        Ok(OrderTally { iterations: cases.len(), file_applied, file_yielded, callbacks, max_callback })
    }
    #[test]
    fn all_5040_orders_of_settings_file_off_on_send_mcp_and_held_speech() -> Result<()> {
        let mut orders = vec![];
        permutations(&mut [0, 1, 2, 3, 4, 5, 6], 0, &mut orders);
        assert_eq!(orders.len(), 5040);
        let cases: Vec<(usize, &[usize])> = (0..3).flat_map(|r| orders.iter().map(move |o| (r, o.as_slice()))).collect();
        // Every case waits on the journal's F_FULLFSYNC commits, not the CPU; independent
        // pipelines overlap those waits. Each worker takes every `workers`-th case.
        let workers = std::thread::available_parallelism().map_or(4, |n| n.get()).min(12);
        let shards: Vec<Vec<(usize, &[usize])>> = (0..workers).map(|w| cases.iter().skip(w).step_by(workers).copied().collect()).collect();
        let tallies = std::thread::scope(|scope| {
            let handles: Vec<_> = shards.iter().map(|shard| scope.spawn(move || run_stop_orders(shard))).collect();
            handles.into_iter().map(|h| h.join().expect("orderings worker panicked")).collect::<Result<Vec<_>>>()
        })?;
        let total = tallies.iter().fold(OrderTally::default(), |a, t| OrderTally {
            iterations: a.iterations + t.iterations,
            file_applied: a.file_applied + t.file_applied,
            file_yielded: a.file_yielded + t.file_yielded,
            callbacks: a.callbacks + t.callbacks,
            max_callback: a.max_callback.max(t.max_callback),
        });
        let OrderTally { iterations, file_applied, file_yielded, callbacks, max_callback } = total;
        assert_eq!(iterations, 15120);
        assert!(file_applied > 0 && file_yielded > 0);
        println!("orderings={iterations} (5040 x Agent/Heimdall/Quit, {workers} workers); callbacks={callbacks}; file_intents_applied={file_applied}; file_intents_yielded_to_newer_menu={file_yielded}; max_callback_us={}; callback_bound_ms=50; stale_bytes/results/audio=0 (Agent/Quit), delivered (Listening); fresh_prompt_after_On=true", max_callback.as_micros());
        Ok(())
    }
    #[test]
    fn on_for_one_reason_cannot_clear_other_reason_or_quit() {
        let gate = agent::delivery::DeliveryGate::new(true);
        gate.intent(StopReason::Agent, false);
        gate.intent(StopReason::Heimdall, false);
        gate.intent(StopReason::Agent, true);
        assert!(!gate.load(Ordering::Acquire));
        gate.intent(StopReason::Heimdall, true);
        assert!(gate.load(Ordering::Acquire));
        gate.intent(StopReason::Quit, false);
        gate.intent(StopReason::Agent, true);
        gate.intent(StopReason::Heimdall, true);
        gate.intent(StopReason::Quit, true);
        assert!(!gate.load(Ordering::Acquire));
    }
