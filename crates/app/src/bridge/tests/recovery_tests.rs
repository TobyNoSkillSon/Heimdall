    use super::*;
    #[test]
    fn dry_log_rotation_is_lossless_and_bounded() -> Result<()> {
        let temp=tempfile::tempdir()?;
        let path=temp.path().join("dry-run.jsonl");
        let session=temp.path().join("archive/session-rotation");
        let original=b"{\"dry_run\":true}\n".repeat(80_000);
        std::fs::write(&path,&original)?;
        let mut file=OpenOptions::new().append(true).open(&path)?;
        rotate_dry_log(&mut file,&path,&session)?;
        assert_eq!(file.metadata()?.len(),0);
        let name=format!("dry-run-{}.jsonl",heimdall_archive::hash(&original));
        assert_eq!(heimdall_archive::store::logs(&session)?,vec![name.clone()]);
        assert_eq!(heimdall_archive::store::read_log(&session,&name)?,original);
        Ok(())
    }
    #[test]
    fn runtime_failure_thread_keeps_collecting_and_queueing_wakes() -> Result<()> {
        let temp=tempfile::tempdir()?;
        let session=temp.path().join("archive/session");
        let settings=Settings { dry_run:false, ..Default::default() };
        // Unavailable Pi executable never receives audio or a provider submission.
        TEST_HARNESS.with(|p| *p.borrow_mut()=Some(temp.path().join("missing-pi")));
        let runtime=AgentRuntime::start(settings,temp.path().into(),session.clone(),Arc::new(Tools::new(temp.path(),temp.path())),None)?;
        for id in 0..3 {
            runtime.send(Command::Observe(Observation { id:id.to_string(),timestamp_ms:1,source:"mic".into(),speaker:"User".into(),language:"en".into(),text:"keep this transcript".into() }))?;
            runtime.send(Command::Wake { images:vec![],urgent:false })?;
            runtime.barrier()?;
            assert!(runtime.is_running());
        }
        std::thread::sleep(std::time::Duration::from_secs(3));
        runtime.barrier()?;
        assert!(runtime.is_running());
        drop(runtime);
        assert!(temp.path().join("agent/restarts.json").is_file());
        TEST_HARNESS.with(|p| p.borrow_mut().take());
        let db=rusqlite::Connection::open(temp.path().join("pi-spool.sqlite"))?;
        let observations:i64=db.query_row("SELECT count(*) FROM observations",[],|r|r.get(0))?;
        assert_eq!(observations,3);
        Ok(())
    }
