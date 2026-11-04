    use super::*;
    #[test]
    fn quit_capture_pump_preserves_pcm_tail_and_removal_with_a_full_queue() {
        let (input, rx) = unbounded();
        let (tx, output) = bounded(1);
        let info = fake_source("app",0,1);
        let source = info.id.clone();
        input.send(capture::CaptureEvent::SourceAdded(info)).unwrap();
        for n in 0..4 {
            input.send(capture::CaptureEvent::Audio(capture::AudioChunk {source_id:source.clone(),label:"tail".into(),host_ns:n*20_000_000,samples:vec![n as f32;320],discontinuity:false})).unwrap();
        }
        input.send(capture::CaptureEvent::SourceRemoved {source_id:source,host_ns:80_000_000,reason:"quit".into()}).unwrap();
        drop(input);
        let pump = std::thread::spawn(move || pump_capture_events(
            || rx.recv_timeout(Duration::from_secs(1)),tx,Arc::new(AtomicBool::new(false)),Arc::new(AtomicBool::new(true))));
        let first = output.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(matches!(first,capture::CaptureEvent::SourceAdded(_)));
        for n in 0..4 {
            let event = output.recv_timeout(Duration::from_secs(5)).unwrap();
            let capture::CaptureEvent::Audio(chunk) = event else {panic!("lost the final PCM");};
            assert_eq!(chunk.samples,vec![n as f32;320]);
            assert_eq!(chunk.host_ns,n*20_000_000);
        }
        assert!(matches!(output.recv_timeout(Duration::from_secs(5)).unwrap(),capture::CaptureEvent::SourceRemoved {..}));
        pump.join().unwrap();
    }
    /// Bug note 2026-10-27T16-57-38: a capture restart joined the capture thread on the live loop for 2.1 s. A retired
    /// run stops on its own thread: the caller returns at once, a forwarding run's stop flag is set only after its
    /// capture is gone (its last events still reach the loop), and Quit's settle waits for it.
    #[test]
    fn a_retired_capture_run_stops_off_the_live_loop_and_quit_waits_for_it() {
        for forward in [true, false] {
            let stop = Arc::new(AtomicBool::new(false));
            let (release, held) = bounded::<()>(1);
            // A pump that is slow to end, as one forwarding a stopped run's tail behind a held archive is; it ends
            // when its run's receiver disconnects (here: the release).
            let pump = std::thread::spawn(move || {
                let _ = held.recv_timeout(Duration::from_secs(10));
            });
            let run = CaptureRun { capture: None, pump: Some(pump), stop: stop.clone(), draining: Arc::new(AtomicBool::new(false)) };
            let started = Instant::now();
            run.retire(forward);
            assert!(started.elapsed() < Duration::from_millis(200), "retire waited {:?}", started.elapsed());
            if !forward {
                assert!(stop.load(Ordering::Acquire), "Listening off abandons the stopped run's events at once");
            }
            assert!(!crate::live_loop::settle_retired(Duration::from_millis(30)), "the run is still stopping");
            release.send(()).unwrap();
            assert!(crate::live_loop::settle_retired(Duration::from_secs(10)));
            assert!(stop.load(Ordering::Acquire));
        }
    }
    /// Review 27 Oct: a restart's retired run forwards its last events even when the live loop's channel is full
    /// at the moment its capture stops (CaptureRun::start can hold the loop): the old source's removal still arrives.
    #[test]
    fn a_retired_run_forwards_its_removal_through_a_full_channel() {
        let (input, rx) = unbounded();
        let (tx, output) = bounded(1);
        let info = fake_source("mic", 0, 7);
        let source = info.id.clone();
        tx.send(capture::CaptureEvent::SourceAdded(fake_source("mic", 0, 8))).unwrap();
        input.send(capture::CaptureEvent::SourceRemoved { source_id: source.clone(), host_ns: 80_000_000, reason: "capture stopped".into() }).unwrap();
        drop(input);
        let stop = Arc::new(AtomicBool::new(false));
        let pump = std::thread::spawn({
            let stop = stop.clone();
            move || pump_capture_events(|| rx.recv_timeout(Duration::from_millis(50)), tx, stop, Arc::new(AtomicBool::new(false)))
        });
        let run = CaptureRun { capture: None, pump: Some(pump), stop, draining: Arc::new(AtomicBool::new(false)) };
        run.retire(true);
        // The live loop is busy for a second, as when a new run starts, then reads.
        std::thread::sleep(Duration::from_secs(1));
        assert!(matches!(output.recv_timeout(Duration::from_secs(1)).unwrap(), capture::CaptureEvent::SourceAdded(_)));
        let removed = output.recv_timeout(Duration::from_secs(5)).expect("the old source's removal was dropped");
        assert!(matches!(removed, capture::CaptureEvent::SourceRemoved { source_id, .. } if source_id == source));
        assert!(crate::live_loop::settle_retired(Duration::from_secs(10)));
    }
