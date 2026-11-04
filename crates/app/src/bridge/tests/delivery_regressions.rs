    use super::*;

    #[test]
    fn screenshot_action_prefers_full_resolution_per_display_archives_every_capture() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let journal = heimdall_archive::Journal::open(&dir.path().join("archive/session-delivery"))?;
        // Real one-pixel PNGs: the agent's context image decodes what the capture wrote.
        let tiny=|v:u8|{let mut b=vec![];image::RgbImage::from_pixel(1,1,image::Rgb([v,v,v])).write_to(&mut std::io::Cursor::new(&mut b),image::ImageFormat::Png).unwrap();b};
        let captures = [
            (1, screen::ShotKind::Ring, tiny(1)),
            (1, screen::ShotKind::FullResolution, tiny(2)),
            (2, screen::ShotKind::Ring, tiny(3)), // Full-res failed only on this display.
            (3, screen::ShotKind::FullResolution, tiny(2)), // Identical content on another display.
        ];
        let mut shots = vec![];
        for (n, (display_id, kind, bytes)) in captures.into_iter().enumerate() {
            let path = dir.path().join(format!("{n}.png")); std::fs::write(&path, bytes)?;
            shots.push(screen::Shot { display_id, frame_ns: n as u64, kind, width: 1, height: 1, grab_latency_ns: None, path });
        }
        let action = screen::Action { source:"mic".into(),trigger_ns:0,note:"screenshot".into(),image_path:shots[0].path.clone(),shots,
            full_resolution_errors:vec!["display 2 unavailable".into()],full_resolution_elapsed_ns:None };
        let (images, captured) = archive_screenshot_action(&journal, &action, "mic", &[], 0)?;
        assert_eq!(images.iter().map(|i|i.bytes.clone()).collect::<Vec<_>>(), vec![tiny(2),tiny(3)]);
        assert_eq!(captured.len(),4);
        assert_eq!(journal.records()?.iter().filter(|r|r.kind=="screenshot").count(),4);
        assert!(action.shots.iter().all(|s|!s.path.exists()));
        Ok(())
    }

    #[tokio::test]
    async fn recorded_pi_retry_archives_marked_resend_with_or_without_retained_context() -> Result<()> {
        for retained in [true, false] {
            let dir = tempfile::tempdir()?;
            let journal = heimdall_archive::Journal::open(&dir.path().join("archive/session-retry"))?;
            let pi = Pi::launch_config(PiConfig::fixture(
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../agent/tests/fixtures/mock-adapter.py"),
                dir.path().join("session.jsonl"), dir.path().to_owned()))?;
            let mut b = agent::Bridge::open(RecordedAdapter {
                inner:Adapter::Pi(pi), journal, gate:None,ids:Default::default(),queries:HashMap::new(),
            },dir.path().join("spool"))?;
            b.observe(agent::protocol::Observation { id:"speech".into(),timestamp_ms:1,source:"Mic".into(),speaker:"User".into(),language:"en".into(),text:"remember the blue folder".into() }).await?;
            b.wake("[fixture:503]", &[], false).await?;
            loop {
                if matches!(tokio::time::timeout(std::time::Duration::from_secs(3),b.next()).await??, Some(Event::Deferred {..})) { break; }
            }
            if !retained { std::fs::write(dir.path().join("active-messages.json"),"[]")?; }
            b.engine_ready(); b.flush_queued().await?;
            let mut retried = None;
            loop {
                match tokio::time::timeout(std::time::Duration::from_secs(3),b.next()).await?? {
                    Some(Event::Other(v)) if v["type"]=="mock_pi" => retried=Some(v["request"]["message"].as_str().unwrap().to_owned()),
                    Some(Event::Consumed {..}) => break,
                    _=>{}
                }
            }
            b.shutdown().await?;
            let text = retried.unwrap();
            assert_eq!(text.matches("remember the blue folder").count(), 1);
            assert!(text.contains("(resent after a delivery interruption; ignore repeated speech)"));
            assert!(!dir.path().join("context-queried").exists());
            let records = b.harness.journal.records()?;
            let sent = records.iter().rev().find(|r|r.kind=="agent_send").unwrap();
            let payload = &sent.payload["payload"];
            assert!(payload.get("transcript_context").is_none());
            assert_eq!(agent::protocol::observation_ids(payload), ["speech"]);
            assert!(payload["message"]["content"][0]["text"].as_str().unwrap().contains("remember the blue folder"));
        }
        Ok(())
    }

    #[tokio::test]
    async fn window_counts_active_steers_without_a_local_followup_queue_then_clears() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let pi = Pi::launch_config(PiConfig::fixture(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../agent/tests/fixtures/mock-adapter.py"),
            dir.path().join("session.jsonl"), dir.path().to_owned()))?;
        let mut b = agent::Bridge::open(RecordedAdapter {
            inner:Adapter::Pi(pi), journal:heimdall_archive::Journal::open(&dir.path().join("archive/session-counts"))?,
            gate:None,ids:Default::default(),queries:HashMap::new(),
        },dir.path().join("spool"))?;
        let settings = Settings { dry_run:false,..Default::default() };
        b.wake("screenshot", &[Attachment {media_type:"image/png".into(),bytes:vec![1,2,3]}], false).await?;
        for n in 0..10 { b.wake(&format!("later {n}"), &[], false).await?; }
        let state = agent_window_state(&b,&settings,0,None);
        assert_eq!(state.state,ui::AgentRunState::Working);
        assert_eq!(state.unresolved.len(),11);
        assert_eq!(state.queued,0);
        assert!(state.can_stop);
        for remaining in (0..11).rev() {
            loop { if matches!(b.next().await?,Some(Event::Complete{error:false,..})) {break;} }
            let state = agent_window_state(&b,&settings,0,None);
            assert_eq!(state.state,if remaining==0 {ui::AgentRunState::Ready}else{ui::AgentRunState::Working});
            assert_eq!(state.unresolved.len(),remaining); assert_eq!(state.queued,0);
            assert_eq!(state.can_stop,remaining>0);
            b.flush_queued().await?;
        }
        assert!(b.inflight()?.is_empty());
        b.shutdown().await?;
        Ok(())
    }
