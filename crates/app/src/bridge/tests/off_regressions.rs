    use super::*;
    #[test]
    fn off_on_during_outer_journal_preparation_cannot_adopt_new_provider_epoch() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let gate = Arc::new(agent::delivery::DeliveryGate::new(true));
        let rt = tokio::runtime::Runtime::new()?;
        rt.block_on(async {
            let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../agent/tests/fixtures/mock-adapter.py");
            let mut pi = Pi::launch(&fixture, &temp.path().join("session.jsonl"), temp.path())?;
            pi.set_delivery_gate(gate.clone());
            let mut adapter = RecordedAdapter { inner: Adapter::Pi(pi), journal: heimdall_archive::Journal::open(&temp.path().join("session-pi"))?, gate: Some(gate.clone()), ids: Default::default(), queries: HashMap::new() };
            let off_on = gate.clone();
            TEST_BEFORE_DELIVERY.with(|hook| *hook.borrow_mut() = Some(Box::new(move || {
                off_on.store(false, Ordering::Release);
                off_on.store(true, Ordering::Release);
            })));
            let error = adapter.send(agent::protocol::message("original", "private journalled prompt", &[], true, false)?).await.unwrap_err();
            assert_eq!(error.downcast_ref::<agent::delivery::DeliveryBlocked>().unwrap().written, 0);
            assert!(gate.load(Ordering::Acquire));
            adapter.shutdown().await?;
            let records = heimdall_archive::Journal::open_read_only(&temp.path().join("session-pi"))?.records()?;
            assert!(records.iter().any(|r| r.kind == "agent_delivery" && r.payload["transport_accepted"] == false));
            println!("outer_journal_pre_off_send_accepted_after_off_on=false written_bytes=0");
            Ok::<_, anyhow::Error>(())
        })
    }
    #[test]
    fn agent_off_cancels_reviewers_blocked_pi_prompt_without_retry() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir()?;
        let data=temp.path().join("data");let session=data.join("archive/session");
        // The harness runs in the data directory, which the app always has.
        std::fs::create_dir_all(&data)?;
        let executable=temp.path().join("fake-pi.py");
        let fixture=include_str!("../../../tests/fixtures/blocked-pi.py").replace("p=Path.cwd()", "p=Path.cwd()\nfirst=os.read(sys.stdin.fileno(),1)\nassert first\n(p/'write-progress.marker').write_bytes(first)");
        std::fs::write(&executable,fixture)?;
        std::fs::set_permissions(&executable,std::fs::Permissions::from_mode(0o700))?;
        TEST_HARNESS.with(|p| *p.borrow_mut()=Some(executable));
        let settings=Settings {dry_run:false,..Default::default()};
        let tools=Arc::new(Tools::new(temp.path(),&data));
        let actor=AgentRuntime::start(settings.clone(),data.clone(),session.clone(),tools.clone(),None)?;
        TEST_HARNESS.with(|p| p.borrow_mut().take());
        actor.send(Command::Observe(Observation {id:"local-1".into(),timestamp_ms:1,source:"mic".into(),speaker:"User".into(),language:"en".into(),text:"a".repeat(256*1024)}))?;
        actor.barrier()?;
        actor.send(Command::Wake{images:vec![],urgent:false})?;
        let began=std::time::Instant::now();
        loop {
            if heimdall_archive::Journal::open_read_only(&session)?.records()?.iter().any(|r|r.kind=="agent_transport_send") {break;}
            anyhow::ensure!(began.elapsed()<std::time::Duration::from_secs(5),"transport did not start");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let began=std::time::Instant::now();
        while !data.join("write-progress.marker").exists() {
            anyhow::ensure!(began.elapsed()<std::time::Duration::from_secs(5),"transport wrote no byte");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        tools.agent_enabled.store(false,Ordering::Release);
        std::fs::write(data.join("off.marker"),b"off")?;
        // This barrier must complete while the child is still withholding all reads.
        actor.barrier()?;
        std::fs::write(data.join("release.marker"),b"read stdin now")?;
        let mut off=settings;off.dry_run=true;
        actor.send(Command::Restart(off))?;actor.barrier()?;drop(actor);
        assert!(!data.join("received.json").exists(),"fake Pi received a full prompt after Off");
        let records=heimdall_archive::Journal::open_read_only(&session)?.records()?;
        let cancellation=records.iter().find(|r|r.kind=="agent_transport_cancelled").context("no transport cancellation")?;
        let written=cancellation.payload["wire"]["written_bytes"].as_u64().unwrap();
        let total=cancellation.payload["wire"]["total_bytes"].as_u64().unwrap();
        assert!(written>0 && written<total,"expected a partial write: {written}/{total}");
        assert_eq!(cancellation.payload["wire"]["uncertain"],true);
        assert!(!records.iter().any(|r|r.kind=="agent_transport_sent"));
        assert_eq!(records.iter().filter(|r|r.kind=="agent_transport_send").count(),1);
        let spool=rusqlite::Connection::open(data.join("pi-spool.sqlite"))?;
        let state:String=spool.query_row("SELECT state FROM deliveries WHERE query=1",[],|r|r.get(0))?;
        assert_eq!(state,"needs_reconciliation");
        let mut reopened=agent::Bridge::dry_run(data.join("pi-spool.sqlite"))?;
        assert!(matches!(reopened.status,agent::Status::NeedsReconciliation(_)));
        let rt=tokio::runtime::Runtime::new()?;rt.block_on(reopened.flush_queued())?;
        assert!(reopened.harness.log.is_empty());
        Ok(())
    }
    #[test]
    fn queued_recovery_cannot_cross_recorded_adapter_while_off() -> Result<()> {
        let temp=tempfile::tempdir()?;let gate=Arc::new(agent::delivery::DeliveryGate::new(false));
        let mut bridge=agent::Bridge::open(RecordedAdapter{inner:Adapter::Unavailable(Unavailable),journal:heimdall_archive::Journal::open(&temp.path().join("session-pi"))?,gate:Some(gate.clone()),ids:Default::default(),queries:HashMap::new()},temp.path().join("spool.sqlite"))?;
        bridge.handle(&Event::Limit{rejected:true,reset:Some(0),status:"fixture".into()})?;
        let rt=tokio::runtime::Runtime::new()?;
        // A recovery flush uses the exact same boundary as observations and new wakes.
        let error=rt.block_on(bridge.wake("queued wake",&[],false)).unwrap_err();
        assert!(error.downcast_ref::<agent::delivery::DeliveryBlocked>().is_some());
        assert!(!matches!(bridge.status,agent::Status::NeedsReconciliation(_)));
        rt.block_on(bridge.flush_queued())?;
        assert_eq!(bridge.queued_count()?,0,"revoked live wakes are cancelled rather than retried");
        assert!(!heimdall_archive::Journal::open_read_only(&temp.path().join("session-pi"))?.records()?.iter().any(|r|r.kind=="agent_send"));
        Ok(())
    }
