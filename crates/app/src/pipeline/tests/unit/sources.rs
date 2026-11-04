    use super::*;
    /// Review 23 Oct, finding 11: one chunk's or one source's transcription error is noted, never the menu's
    /// "Transcription stopped. Turn Listening off and on to restart it."; a worker that could not start is, and the
    /// line clears when transcription runs again.
    #[test]
    fn one_transcription_error_is_not_transcription_stopped_and_a_real_stop_clears_on_recovery() {
        let (_t, mut a) = app();
        let mic = crate::fake_source("mic", 0, 1);
        let id = mic.id.0.clone();
        a.source(mic);
        a.worker_event(listen::WorkerEvent::Error { source: Some(id), message: "decode failed".into() }).unwrap();
        a.worker_event(listen::WorkerEvent::Error { source: None, message: "chunk dropped".into() }).unwrap();
        assert_eq!(a.failure.as_deref(), Some("Listen failed: chunk dropped"), "still noted");
        assert!(!a.menu_problems.contains_key("listen"), "{:?}", a.menu_problems);
        a.fail("Listen: model unavailable");
        assert_eq!(a.menu_problems.get("listen").map(String::as_str), Some("Transcription stopped. Turn Listening off and on to restart it."));
        a.worker_event(listen::WorkerEvent::Ready).unwrap();
        assert!(!a.menu_problems.contains_key("listen"), "transcription runs again");
        a.fail(crate::pipeline::sources::TRANSCRIPTION_GAVE_UP);
        assert!(a.menu_problems.contains_key("listen"), "restarts used up");
    }
    #[test]
    fn locked_archive_diagnostics_do_not_stall_live_event_processing() {
        let (_temp, mut a) = app();
        let db = heimdall_archive::store::open(a.index.evidence.root()).unwrap();
        db.execute_batch("BEGIN IMMEDIATE").unwrap();
        let (release, unlock) = std::sync::mpsc::channel();
        let locked = std::thread::spawn(move || {
            // Release even on regression: a synchronous diagnostic fails the timing
            // assertion instead of holding up the test for SQLite's 60-second timeout.
            let _ = unlock.recv_timeout(std::time::Duration::from_secs(2));
            db.execute_batch("ROLLBACK").unwrap();
        });
        let start = Instant::now();
        a.screen_event(screen::ScreenEvent::Started { displays: 0 });
        a.settings_result("fixture", Some("injected error".into()));
        for i in 0..6000 {
            a.capture_event(capture::CaptureEvent::Diagnostic { host_ns: i, detail: "locked archive".into() }).unwrap();
        }
        let elapsed = start.elapsed();
        let held = release.send(()).is_ok();
        locked.join().unwrap();
        assert!(held && elapsed < std::time::Duration::from_millis(500), "diagnostics stalled event processing: {elapsed:?}");
        assert!(health_cues(&a).iter().any(|cue| matches!(cue, ui::HealthCue::Drop { key, .. } if key == "screen")));
        assert_eq!(a.component_status.get("screen").map(String::as_str), Some("Screen unavailable · no active display"));
        a.evidence.flush().unwrap();
        let records = a.index.evidence.records().unwrap();
        let accepted = records.iter().filter(|r| r.kind == "health" || r.kind == "capture_diagnostic").count() as u64;
        let dropped: u64 = records.iter().filter(|r| r.kind == "diagnostic_drops").map(|r| r.payload["count"].as_u64().unwrap()).sum();
        // The writer keeps up to `WRITER_BACKLOG` records behind a held lock (31 Oct): 6,000 are all kept, none dropped.
        // Saturation itself is tested on a small bound in heimdall-archive (diagnostic_saturation_is_nonblocking…).
        assert_eq!(dropped, 0, "a few thousand diagnostics behind a held lock are kept, not dropped");
        assert_eq!(accepted + dropped, 6004, "every accepted or dropped diagnostic is accounted for");
        eprintln!("live diagnostics while locked: {elapsed:?}; accepted={accepted}, durably counted drops={dropped}");
    }

    #[test]
    fn discovery_diagnostic_never_invalidates_healthy_streams() {
        let (_temp,mut a)=app();
        for kind in ["mic","app"] {
            let info=crate::fake_source(kind,0,1);let source=info.id.0.clone();a.source(info);
            a.audio.insert(source.clone(),AudioRing {start:0,samples:vec![0.1;16000].into()});
            a.ensure_utterance(&source,0);
        }
        a.capture_event(capture::CaptureEvent::Diagnostic { host_ns:500_000_000,detail:"tap recovery attempt 1/4".into() }).unwrap();
        assert_eq!(a.gaps,0);assert!(a.source_cutoffs.is_empty());assert_eq!(a.active.len(),2);
        assert!(a.audio.values().all(|r| r.samples.len()==16000));
        a.evidence.flush().unwrap();
        assert_eq!(a.index.evidence.records().unwrap().iter().filter(|r|r.kind=="capture_diagnostic").count(),1);
    }
    #[test]
    fn native_gap_notification_does_not_reject_a_completed_queued_final() {
        let (_temp,mut a)=app();let info=crate::fake_source("app",0,1);let source=info.id.0.clone();a.source(info);
        a.audio.insert(source.clone(),AudioRing {start:0,samples:vec![0.1;32000].into()});
        a.capture_event(capture::CaptureEvent::Gap { source_id:Some(capture::SourceId(source.clone())),host_ns:2_000_000_000,detail:"native overflow: 5120 frames".into() }).unwrap();
        assert!(a.source_cutoffs.is_empty());
        say(&mut a,&source,"completed before loss",1_000_000_000,false);
        a.worker_event(listen::WorkerEvent::Listen(ListenEvent::Discontinuity { source:source.clone(),host_time_ns:2_000_000_000 })).unwrap();
        assert_eq!(a.finals,1);assert_eq!(a.gaps,1);
        assert_eq!(a.index.utterances_between(0,2_000_000_000).unwrap()[0]["text"],"completed before loss");
        assert!(!a.index.evidence.records().unwrap().iter().any(|r|r.kind=="obsolete_worker_event"));
    }
    // Reviewer B's gap probe: retain the second half of the waveform after an older loss.
    #[test]
    fn older_worker_gap_retains_later_pcm_and_indexes_final() {
        let (_temp,mut a)=app();let info=crate::fake_source("app",0,1);let source=info.id.0.clone();a.source(info);
        a.audio.insert(source.clone(),AudioRing {start:0,samples:vec![0.1;16000].into()});
        a.worker_event(listen::WorkerEvent::Gap {source:source.clone(),start_ns:0,end_ns:500_000_000,reason:listen::GapReason::InputOverflow}).unwrap();
        assert_eq!(a.audio[&source].start,500_000_000);assert_eq!(a.audio[&source].samples.len(),8000);
        a.worker_event(listen::WorkerEvent::Listen(ListenEvent::SpeechStart {source:source.clone(),host_time_ns:500_000_000})).unwrap();
        a.worker_event(listen::WorkerEvent::Listen(ListenEvent::FinalUtterance {source:source.clone(),text:"retained words".into(),words:vec![Word {text:"retained words".into(),start_ns:500_000_000,end_ns:1_000_000_000,confidence:1.}],start_ns:500_000_000,end_ns:1_000_000_000,forced:false,end_of_turn_probability:1.})).unwrap();
        let rows=a.index.utterances_between(0,1_000_000_000).unwrap();assert_eq!(rows.len(),1);assert_eq!(rows[0]["decision"]["status"],"archive_only");
        assert_eq!(a.finals,1);assert_eq!(a.errors,0);
    }
    #[test]
    fn unavailable_waveform_final_is_indexed_with_its_gap_and_later_final_still_completes() {
        let (_temp,mut a)=app();let info=crate::fake_source("app",0,1);let source=info.id.0.clone();a.source(info);
        say(&mut a,&source,"audio is unavailable",2_000_000_000,false);
        a.audio.insert(source.clone(),AudioRing {start:0,samples:vec![0.1;16000*10].into()});
        say(&mut a,&source,"later retained final",4_000_000_000,false);
        let rows=a.index.utterances_between(0,10_000_000_000).unwrap();assert_eq!(rows.len(),2);
        // Night review L1: a lost waveform no longer ends the final; this app source is archive-only, with the gap noted.
        assert_eq!(rows[0]["decision"]["status"],"archive_only");assert!(rows[0]["decision"]["audio_error"].is_string());
        assert_eq!(rows[1]["decision"]["status"],"archive_only");assert!(rows[1]["decision"]["audio_error"].is_null());assert_eq!(a.finals,2);assert!(a.agent.is_none());
        assert!(rows.iter().all(|r|r["source"]=="App replay"));
        a.finish().unwrap();assert!(heimdall_archive::store::is_complete(&a.session).unwrap());
    }
    #[test]
    fn obsolete_gap_and_final_are_explicitly_archived() {
        let (_temp,mut a)=app();let info=crate::fake_source("app",0,1);let source=info.id.0.clone();a.source(info);
        a.gap(&source,2_000_000_000,"fixture gap");
        say(&mut a,&source,"obsolete words",1_000_000_000,false);
        a.worker_event(listen::WorkerEvent::Gap {source:source.clone(),start_ns:0,end_ns:1_000_000_000,reason:listen::GapReason::InputOverflow}).unwrap();
        assert_eq!(a.finals,0);assert_eq!(a.gaps,1);
        let records=a.index.evidence.records().unwrap();assert!(records.iter().filter(|r|r.kind=="obsolete_worker_event").count()>=2);
        a.close_source(&source);
        say(&mut a,&source,"closed-source words",4_000_000_000,false);assert_eq!(a.finals,0);
    }
    #[test]
    fn a_worker_that_ends_by_itself_is_shown_and_restarted_three_times_then_reported() {
        // The models are missing, so every worker ends at once: its reason, then its end marker, reach the loop.
        let (_t, mut a) = app();
        let source = crate::fake_source("mic", 0, 1);
        a.source(source);
        let (tx, rx) = crossbeam_channel::bounded(512);
        a.listening_starting();
        a.ensure_live_worker(tx.clone()).unwrap();
        let mut ends = 0;
        while ends < 4 {
            match rx.recv_timeout(std::time::Duration::from_secs(10)).expect("worker output") {
                Some(event) => a.worker_event(event).unwrap(),
                None => { ends += 1; a.worker_ended(Some(&tx)).unwrap(); }
            }
            // A stale marker (the current worker still running or gone) changes nothing.
            if ends < 4 { assert!(a.live_worker.is_some()); }
        }
        assert!(a.live_worker.is_none(), "gave up after three restarts");
        a.worker_ended(Some(&tx)).unwrap();
        let ended: Vec<_> = a.index.evidence.records().unwrap().into_iter().filter(|r| r.kind == "listen_worker_ended")
            .map(|r| r.payload["restarting"].as_bool().unwrap()).collect();
        assert_eq!(ended, [true, true, true, false]);
        // The island: one orange "Restarting transcription…" for the whole run of restarts (never "OK": no worker
        // got ready), then the next rung, a relaunch, which main.rs carries out or shows red past its bound.
        let health = health_cues(&a);
        assert_eq!(health, [ui::HealthCue::Restarting { key: "listen".into(), part: ui::Part::Transcription, label: "Restarting transcription…".into() }]);
        assert_eq!(a.take_recovery(), Some(crate::health::Recovery::Relaunch { key: "listen".into(), part: ui::Part::Transcription, problem: "Transcription keeps stopping".into() }));
        assert_eq!(a.component_status.get("listen").map(String::as_str), Some("Transcription unavailable"));
        assert!(!cues(&a).iter().any(|c| matches!(c, ui::CommandCue::Done { .. })), "never claimed Listening");
        // Live audio without a worker is not transcribed on the loop (no inline model load).
        assert!(a.engine.is_none());
    }
    #[test]
    fn listening_off_keeps_the_worker_and_on_is_ready_at_once() {
        let (_t, mut a) = app();
        let (tx, rx) = crossbeam_channel::bounded(512);
        let worker = Arc::new(listen::ListenWorker::spawn(Default::default(), || ListenEngine::new(Silent, Silent, Silent, Default::default())).unwrap());
        let output = worker.clone();
        let forward = tx.clone();
        std::thread::spawn(move || { while let Some(e) = output.recv_event() { if forward.send(Some(e)).is_err() { return; } } let _ = forward.send(None); });
        a.live_worker = Some(worker.clone());
        worker.wait_ready().unwrap();
        let info = crate::fake_source("mic", 0, 1);
        let source = info.id.0.clone();
        for round in 0..2u64 {
            a.source(info.clone());
            for tick in 0..8u64 {
                worker.push(listen::AudioChunk { source: &source, generation: 1, host_time_ns: (round * 100 + tick) * 32_000_000, samples: &[0.; 512] }).unwrap();
            }
            let started = Instant::now();
            a.close_live_sources(&rx).unwrap();
            assert!(started.elapsed() < std::time::Duration::from_secs(5));
            assert!(!a.sources.contains_key(&source), "SourceClosed reached the pipeline");
            assert!(a.live_worker_running());
            // Listening on again: the models are already loaded, so "Listening" follows the glyph at once.
            a.listening_starting();
            a.listening_ready_check();
            assert!(matches!(cues(&a).last(), Some(ui::CommandCue::Done { kind: ui::CommandKind::Listening, .. })));
        }
        assert_eq!(a.index.evidence.records().unwrap().iter().filter(|r| r.kind == "listening_ready").count(), 2);
        a.stop_live_worker(&rx).unwrap();
    }
    #[test]
    fn stop_returns_when_the_worker_already_ended_and_its_end_marker_was_consumed() {
        // 13 Oct 11:16: Listening off waited forever for a second end marker; every later click and Quit froze.
        let (_t, mut a) = app();
        let (tx, rx) = crossbeam_channel::bounded(512);
        let worker = Arc::new(listen::ListenWorker::spawn(Default::default(), || -> Result<ListenEngine<Silent, Silent, Silent>> {
            anyhow::bail!("engine failed")
        }).unwrap());
        while !worker.is_finished() { std::thread::sleep(std::time::Duration::from_millis(5)); }
        a.live_worker = Some(worker);
        let _keep_sender_alive = tx;
        let started = std::time::Instant::now();
        a.stop_live_worker(&rx).unwrap();
        assert!(started.elapsed() < std::time::Duration::from_secs(2), "stop waited {:?}", started.elapsed());
        assert!(a.live_worker.is_none());
    }
    #[test]
    fn failed_single_final_does_not_abort_worker_teardown() {
        let (_temp,mut a)=app();let info=crate::fake_source("app",0,1);let source=info.id.0.clone();a.source(info);
        a.tools.archive_floor.store(u64::MAX,Ordering::Release);
        let (tx,rx)=crossbeam_channel::unbounded();
        tx.send(Some(listen::WorkerEvent::Listen(ListenEvent::FinalUtterance {source:source.clone(),text:"preserve if possible".into(),words:vec![word("preserve")],start_ns:1_000_000_000,end_ns:1_200_000_000,forced:false,end_of_turn_probability:1.}))).unwrap();
        tx.send(Some(listen::WorkerEvent::SourceClosed {source:source.clone()})).unwrap();tx.send(None).unwrap();
        a.drain_worker_events(&rx, None);
        assert_eq!(a.errors,1);assert!(!a.sources.contains_key(&source));assert!(!a.active.contains_key(&source));
        a.tools.archive_floor.store(1,Ordering::Release);a.finish().unwrap();assert!(heimdall_archive::store::is_complete(&a.session).unwrap());
    }
    #[test]
    fn quit_drains_the_already_closed_speaker_with_owned_engine_and_listening_on() {
        let (_t, mut a) = app();
        let (_engine_lab, manager, engine_pid) = crate::inference_engine::tests::owned_quit_fixture();
        a.attach_test_engine(manager);
        let info = crate::fake_source("app", 0, 1);
        let source = info.id.0.clone();
        let mut raw = a.raw_archive().unwrap();
        raw.add_source(&info,0).unwrap();
        let captured = vec![0.1;16000];
        raw.append(&info.id,0,capture::NativeFormat {sample_rate:16000,channels:1},&captured,false).unwrap();
        raw.remove_source(&info.id,1_000_000_000).unwrap();
        raw.flush().unwrap();
        drop(raw);
        a.source(info);
        a.audio.insert(source.clone(), AudioRing { start: 0, samples: vec![0.1;16000].into() });
        let (actor, speaker) = crate::speakers_runtime::Actor::finishing_fixture(source.clone(),0,a.session.clone(),a.speaker_tx.clone(),a.speaker_cpu.clone());
        a.discord.insert(source.clone(), Discord { origin:0,session:0,actor_generation:0,actor,speaking:true,fed_until:1_000_000_000,active:true,loading:false,closed:false,segments:vec![],unlabelled:vec![] });
        let worker = Arc::new(listen::ListenWorker::spawn(Default::default(), || ListenEngine::new(Silent, Silent, Silent, Default::default())).unwrap());
        worker.wait_ready().unwrap();
        worker.push(listen::AudioChunk {source: &source, generation: 0, host_time_ns: 0, samples: &[0.1;320]}).unwrap();
        let (tx, rx) = crossbeam_channel::unbounded();
        let output = worker.clone();
        let forwarder = std::thread::spawn(move || {
            loop {
                if let Some(e) = output.pop_event() { tx.send(Some(e)).unwrap(); }
                else if output.is_finished() { tx.send(None).unwrap(); break; }
                else { std::thread::sleep(std::time::Duration::from_millis(5)); }
            }
        });
        a.live_worker = Some(worker);
        // Exactly the menu Quit sequence: stop Splash, drain Listen's SourceClosed,
        // then finish the pipeline. Speaker Closed is queued, not yet handled.
        a.action(ui::UiAction::Quit).unwrap();
        a.request_engine_stop();
        a.stop_live_worker(&rx).unwrap();
        forwarder.join().unwrap();
        assert!(a.discord[&source].closed);
        // Its final snapshot is queued and its receiver has exited, deterministically
        // reproducing the second Finish send that used to fail at Quit.
        speaker.join().unwrap();
        a.finish().unwrap();
        assert!(heimdall_archive::store::is_complete(&a.session).unwrap());
        assert!(!archive::run_dir(&a.data, &a.session).exists());
        assert_eq!(unsafe { libc::kill(engine_pid, 0) }, -1, "owned fake Splash was not stopped");
        let view = heimdall_archive::View::session(&a.session).unwrap();
        view.capture_span(&source,0,1_000_000_000).unwrap();
        let block: i64 = view.db.query_row("SELECT id FROM temp.blocks LIMIT 1",[],|r|r.get(0)).unwrap();
        assert_eq!(heimdall_archive::audio::read_block(&view.db,block).unwrap().samples,captured);
        assert!(!a.sources.contains_key(&source));
        assert!(a.index.evidence.records().unwrap().iter().any(|r| r.kind == "speaker_segments_delta" && r.payload["segments"][0]["segment"]["end_time"] == 1.));
        assert_eq!(a.errors, 0);
    }
    #[test]
    fn physical_source_removal_surfaces_clean_disconnection_name() {
        let (_temp,mut a)=app();let mic=crate::fake_source("mic",0,1);let source=mic.id.clone();a.source(mic);
        a.capture_event(capture::CaptureEvent::SourceRemoved {source_id:source.clone(),host_ns:1,reason:"input device disconnected".into()}).unwrap();
        assert!(a.gap_notices.contains(&("Replay mic 1".into(),"disconnected")));
        assert!(a.index.evidence.records().unwrap().iter().any(|r|r.kind=="source_removed"&&r.payload["reason"]=="input device disconnected"));
    }
    /// Bug note 2026-10-27T00-47: a route change removed the Shure and capture re-added it 62 ms later under a new
    /// generation; the log printed only the removal, so a live mic looked dead.
    #[test]
    fn a_readded_mic_logs_one_source_added_line() {
        let (_temp,mut a)=app();
        let mut mic=crate::fake_source("mic",0,1);mic.id=capture::SourceId("mic:Shure-MV7i:17".into());mic.generation=17;
        a.source(mic.clone());
        a.capture_event(capture::CaptureEvent::SourceRemoved {source_id:mic.id.clone(),host_ns:capture::host_now_ns(),reason:"source stopped / route or format changed".into()}).unwrap();
        let mut back=mic.clone();back.id=capture::SourceId("mic:Shure-MV7i:18".into());back.generation=18;
        a.capture_event(capture::CaptureEvent::SourceAdded(back)).unwrap();
        let records=a.index.evidence.records().unwrap();
        let readded:Vec<_>=records.iter().filter(|r|r.kind=="source_readded").collect();
        assert_eq!(readded.len(),1);
        assert_eq!(readded[0].source,"mic:Shure-MV7i:18");
        assert_eq!(readded[0].payload["generation"],18);
        let (line,_)=crate::diagnostics::evidence_line(readded[0]).expect("a log line");
        assert!(line.starts_with("capture: source_readded (mic:Shure-MV7i:18): event: back ") && line.contains("generation 18"),"{line}");
        // A device that never left is no return.
        let other=crate::fake_source("mic",0,2);a.capture_event(capture::CaptureEvent::SourceAdded(other)).unwrap();
        assert_eq!(a.index.evidence.records().unwrap().iter().filter(|r|r.kind=="source_readded").count(),1);
    }
    /// Review 27 Oct: only the generation is stripped; two sources that differ only in their trailing number stay two.
    #[test]
    fn a_device_key_strips_only_the_generation() {
        use crate::pipeline::sources::device_key;
        assert_eq!(device_key("mic:Shure-MV7i:17"),"mic:Shure-MV7i");
        assert_eq!(device_key("process:412:1760000000:3"),"process:412:1760000000");
        assert_eq!(device_key("screen:1"),"screen:1");
        assert_ne!(device_key("screen:1"),device_key("screen:2"));
    }
    #[test]
    fn older_gap_does_not_erase_a_newer_active_transcript_prefix() {
        let (_temp,mut a)=app();let info=crate::fake_source("app",0,1);let source=info.id.0.clone();a.source(info);
        a.audio.insert(source.clone(),AudioRing {start:0,samples:vec![0.1;16000*10].into()});
        a.ensure_utterance(&source,2_000_000_000);let original=a.active[&source].id;
        a.active.get_mut(&source).unwrap().prefix=timed("retained segment",2_000_000_000);
        a.worker_event(listen::WorkerEvent::Gap {source:source.clone(),start_ns:0,end_ns:1_000_000_000,reason:listen::GapReason::InputOverflow}).unwrap();
        assert_eq!(a.active[&source].id,original);assert_eq!(a.active[&source].prefix.len(),2);
        assert_eq!(a.audio[&source].start,1_000_000_000);
    }
    #[test]
    fn natural_ring_eviction_keeps_available_final_tail_without_substitution() {
        let (_temp,mut a)=app();let info=crate::fake_source("app",0,1);let source=info.id.0.clone();a.source(info);
        a.ensure_utterance(&source,0);
        a.audio.insert(source.clone(),AudioRing {start:3_000_000_000,samples:vec![0.1;16000*30].into()});
        a.listen(ListenEvent::FinalUtterance {source:source.clone(),text:"long retained utterance".into(),words:timed("long retained utterance",29_000_000_000),start_ns:0,end_ns:30_000_000_000,forced:false,end_of_turn_probability:1.}).unwrap();
        let rows=a.index.utterances_between(0,33_000_000_000).unwrap();assert_eq!(rows.len(),1);assert_eq!(rows[0]["decision"]["status"],"archive_only");
    }
    #[test]
    fn partial_and_segment_events_are_durable_without_replacing_history() {
        let t = tempfile::tempdir().unwrap();
        let mut app = Pipeline::new(t.path().into(),PathBuf::from("unused"),Settings::default(),None,true).unwrap();
        let mut source=crate::fake_source("mic",0,1);source.id.0="mic".into();app.source(source);
        let word = listen::Word { text:"partial".into(),start_ns:10,end_ns:20,confidence:0.75 };
        app.listen(ListenEvent::PartialTranscript { source:"mic".into(),text:"partial".into(),words:vec![word.clone()],stable_words:0 }).unwrap();
        app.listen(ListenEvent::TranscriptSegment { source:"mic".into(),text:"partial".into(),words:vec![word],start_ns:10,end_ns:20 }).unwrap();
        app.evidence.flush().unwrap();
        let journal=heimdall_archive::Journal::open_read_only(&app.session).unwrap();
        let events: Vec<_> = journal.records().unwrap().into_iter().filter(|r| r.kind=="listen_event").collect();
        assert_eq!(events.len(),2);
        assert_eq!(events[0].payload["event"],"partial_transcript");
        assert_eq!(events[0].payload["words"][0]["confidence"],0.75);
        assert_eq!(events[1].payload["event"],"transcript_segment");
    }
    #[test]
    fn failures_survive_archive_recovery_and_tools_use_configured_floor() {
        let t = tempfile::tempdir().unwrap();
        let settings = Settings {
            free_space_floor_bytes: 2 * 1073741824,
            ..Settings::default()
        };
        let mut a = Pipeline::new(
            t.path().join("data"),
            t.path().join("models"),
            settings,
            None,
            true,
        )
        .unwrap();
        a.worker_event(listen::WorkerEvent::Error {
            source: None,
            message: "model unavailable".into(),
        })
        .unwrap();
        assert_eq!(
            a.failure.as_deref(),
            Some("Listen failed: model unavailable")
        );
        a.disk_status.lock().unwrap().take();
        a.listening_status();
        assert!(a.failure.is_some());
        a.capture_event(capture::CaptureEvent::Gap {
            source_id: None,
            host_ns: 1,
            detail: "archive pipeline stopped: write failed".into(),
        })
        .unwrap();
        assert!(a.failure.as_deref().unwrap().contains("write failed"));
        assert!(a.disk_status.lock().unwrap().is_some());
        assert_eq!(
            a.tools.archive_floor.load(Ordering::Acquire),
            a.settings.free_space_floor_bytes
        );
    }
    #[test]
    fn independent_component_statuses_do_not_clear_each_other() {
        let (_t, mut app) = app();
        app.component_status("listen-gpu", Some("Listening using fallback".into()));
        app.component_status("speakers:a", Some("Speakers using CPU fallback".into()));
        app.component_status("speakers:b", Some("Speakers using CPU fallback".into()));
        app.component_status("listen-gpu", None);
        app.component_status("speakers:a", None);
        assert_eq!(app.component_status.len(), 1);
        assert!(app.component_status.contains_key("speakers:b"));
        app.fail("Decision failed; transcript retained: unavailable");
        assert!(app.component_status.contains_key("decision"));
        assert!(app.component_status.contains_key("speakers:b"));
    }
    #[test]
    fn waveform_ring_preserves_native_clock_drift() {
        let mut r = AudioRing {
            start: 0,
            samples: VecDeque::new(),
        };
        for i in 0..100 {
            r.push(i * 20_000_000 + i * 1000, &[0.1; 320]);
        }
        assert_eq!(r.samples.len(), 32000);
        assert_eq!(r.clip(1_000_000_000, 1_500_000_000).unwrap().len(), 8000);
    }

    #[test]
    fn segment_gap_and_screen_occurrence_are_preserved() {
        let (_t, mut a) = app();
        a.command_test_answers.push_back((decide::CommandIntent::Yes, decide::CommandSelection::LookAtThis));
        let i = crate::fake_source("mic", 0, 1);
        let id = i.id.0.clone();
        a.source(i);
        a.listen(ListenEvent::SpeechStart {
            source: id.clone(),
            host_time_ns: 1_000_000_000,
        })
        .unwrap();
        let utterance = a.active[&id].id;
        a.audio.insert(id.clone(), AudioRing {start:1_000_000_000,samples:vec![0.01;16000].into()});
        a.listen(ListenEvent::TranscriptSegment {
            source: id.clone(),
            text: "spójrz".into(),
            words: vec![word("spójrz")],
            start_ns: 1_000_000_000,
            end_ns: 1_200_000_000,
        })
        .unwrap();
        assert_eq!(a.notes, 0);
        a.listen(ListenEvent::SpeechStart {
            source: id.clone(),
            host_time_ns: 1_300_000_000,
        })
        .unwrap();
        assert_eq!(a.active[&id].id, utterance);
        a.commands(&id, &timed("look at this", 1_000_000_000), true).unwrap();
        assert_eq!(a.notes, 1);
        a.commands(&id, &timed("look at this", 1_000_000_000), true).unwrap();
        assert_eq!(a.notes, 1);
        a.gap(&id, 1_400_000_000, "test overflow");
        assert!(!a.active.contains_key(&id));
        assert_eq!(a.segments, 1);
        assert_eq!(a.gaps, 1);
        let records = a.index.evidence.records().unwrap();
        assert_eq!(
            records
                .iter()
                .filter(|r| r.kind == "transcript_segment")
                .count(),
            1
        );
        assert_eq!(records.iter().filter(|r| r.kind == "gap").count(), 1);
        assert_eq!(
            records
                .iter()
                .filter(|r| r.kind == "screen_command")
                .count(),
            1
        );
    }
    #[test]
    fn heimdall_off_is_an_explicit_archived_gap_not_a_fault() {
        let (_t, mut a) = app();
        let i = crate::fake_source("mic", 0, 1);
        let id = i.id.0.clone();
        a.source(i);
        a.pause_sources();
        a.set_running(false, "menu").unwrap();
        assert!(a.sources.is_empty());
        assert!(a.component_status.keys().all(|k| !k.starts_with("gap:")), "{:?}", a.component_status);
        a.set_running(true, "menu").unwrap();
        let records = a.index.evidence.records().unwrap();
        let kinds: Vec<_> = records.iter().map(|r| r.kind.as_str()).filter(|k| ["gap", "heimdall_off", "heimdall_on"].contains(k)).collect();
        assert_eq!(kinds, ["gap", "heimdall_off", "heimdall_on"]);
        assert!(records.iter().any(|r| r.kind == "gap" && r.source == id));
        assert!(a.send_worker.as_ref().is_none_or(|o|o.outstanding.load(Ordering::Acquire)==0));
    }
    #[test]
    fn transcript_shows_names_and_only_actionable_gaps_once() {
        assert_eq!(gap_notice("capture discontinuity"), None);
        assert_eq!(gap_notice("active output has delivered only digital silence for 10 seconds; genuine silence vs permission/route failure is unverified"), None);
        assert_eq!(gap_notice("native overflow: 12 frames"), None);
        assert_eq!(gap_notice("heimdall off"), None);
        assert_eq!(gap_notice("input device disconnected"), Some("disconnected"));
        let mic = crate::fake_source("mic", 0, 1);
        assert_eq!(display_name(&mic), "Replay mic 1");
        let other = crate::fake_source("app", 1, 1);
        assert_eq!(display_name(&other), "App replay");
        let (_t, mut a) = app();
        let id = mic.id.0.clone();
        a.source(mic);
        a.gap(&id, 1, "input device disconnected");
        let i = crate::fake_source("mic", 0, 1);
        a.source(i);
        a.gap(&id, 2, "input device disconnected");
        assert_eq!(a.gap_notices.len(), 1);
    }
    #[test]
    fn own_audio_is_archive_only_without_loading_models_or_waking() {
        let (_t, mut a) = app();
        let i = capture::SourceInfo::new(
            capture::SourceKind::Process {
                name: "Heimdall".into(),
                bundle_id: Some("dev.heimdall.app".into()),
                pid: std::process::id(),
                birth_us: 1,
                owner: None,
                output_devices: vec![],
            },
            capture::NativeFormat {
                sample_rate: 16000,
                channels: 1,
            },
            1,
        );
        let id = i.id.0.clone();
        a.source(i);
        a.audio.insert(
            id.clone(),
            AudioRing {
                start: 1_000_000_000,
                samples: vec![0.1; 16000].into(),
            },
        );
        a.listen(ListenEvent::FinalUtterance {
            source: id,
            text: "reply".into(),
            words: vec![word("reply")],
            start_ns: 1_000_000_000,
            end_ns: 2_000_000_000,
            forced: false,
            end_of_turn_probability: 1.,
        })
        .unwrap();
        assert!(a.send_worker.as_ref().is_none_or(|o|o.outstanding.load(Ordering::Acquire)==0));
        assert!(a.agent.is_none());
        assert!(a.context.is_empty());
        assert!(a.tools.send_context.lock().unwrap().snapshot().turns.is_empty());
        assert_eq!(a.errors, 0);
    }
