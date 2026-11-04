    use super::*;
    #[test]
    fn voice_wake_actor_queue_keeps_the_pipeline_decision_permit() -> Result<()> {
        let gate=Arc::new(agent::delivery::DeliveryGate::new(true));let (tx,mut rx)=mpsc::unbounded_channel();
        let actor=AgentRuntime{gate:gate.clone(),tx,thread:None};
        let p=actor.wake_permit(false)?;
        actor.wake_with_permit(vec![],false,p)?;
        actor.send(Command::Wake{images:vec![],urgent:false})?;
        gate.store(false,Ordering::Release);gate.store(true,Ordering::Release);
        for _ in 0..2 { let Command::Scoped{command,permit}=rx.try_recv()? else{panic!("wake lost its original permit")};
            assert!(matches!(*command,Command::Wake{..}));assert!(permit.check().is_err()); }
        Ok(())
    }
    #[test]
    fn dry_run_wake_permit_cannot_authorize_live_transport_with_the_same_epoch() -> Result<()> {
        let d=tempfile::tempdir()?;let gate=Arc::new(agent::delivery::DeliveryGate::new(true));
        let (tx,_rx)=mpsc::unbounded_channel();let actor=AgentRuntime{gate:gate.clone(),tx,thread:None};
        let dry=actor.wake_permit(true)?;assert_eq!(dry.generation(),gate.generation());assert!(!dry.belongs_to(&gate));
        let mut live=RecordedAdapter{inner:Adapter::Unavailable(Unavailable),journal:heimdall_archive::Journal::open(&d.path().join("session"))?,gate:Some(gate),ids:Default::default(),queries:HashMap::new()};
        let rt=tokio::runtime::Runtime::new()?;
        let error=rt.block_on(live.send_with_permit(agent::protocol::message("dry","simulation",&[],true,false)?,dry)).unwrap_err();
        assert_eq!(error.downcast_ref::<agent::delivery::DeliveryBlocked>().unwrap().written,0);
        assert!(heimdall_archive::Journal::open_read_only(&d.path().join("session"))?.records()?.is_empty());Ok(())
    }
    #[test]
    fn off_at_each_startup_await_prevents_pi_execution_including_off_on() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        for stage in ["prepare","readiness","metadata"] {for back_on in [false,true] {
            let d=tempfile::tempdir()?;let exe=d.path().join("fake-pi");
            std::fs::write(&exe,"#!/usr/bin/python3\nfrom pathlib import Path\nPath('executed').touch()\n")?;
            std::fs::set_permissions(&exe,std::fs::Permissions::from_mode(0o700))?;
            let gate=Arc::new(agent::delivery::DeliveryGate::new(true));let revoke=gate.clone();
            TEST_STARTUP_AFTER_AWAIT.with(|hook|*hook.borrow_mut()=Some(Box::new(move|current| { if current==stage {
                revoke.store(false,Ordering::Release);if back_on{revoke.store(true,Ordering::Release);}
            }})));
            test_harness(Some(exe));
            let rt=tokio::runtime::Runtime::new()?;
            let result=rt.block_on(open(&Settings{dry_run:false,..Default::default()},d.path(),&d.path().join("archive/session"),None,None,&crate::api_providers::Hub::new(None,d.path().into(),Default::default()),gate,Arc::new(AtomicU8::new(0)),None,None));
            TEST_STARTUP_AFTER_AWAIT.with(|hook|*hook.borrow_mut()=None);test_harness(None);
            assert!(result.is_err(),"{stage} startup escaped revoked permit");
            assert!(!d.path().join("executed").exists());assert!(!d.path().join("tool-groups").exists());
        }}
        Ok(())
    }
    #[test]
    fn off_during_post_launch_startup_awaits_suppresses_ready_results() -> Result<()> {
        let rt=tokio::runtime::Runtime::new()?;
        for stage in ["state","bridge","entries"] {
            let gate=Arc::new(agent::delivery::DeliveryGate::new(true));let p=gate.permit()?;
            let result=rt.block_on(startup_step(stage,&p,async{gate.store(false,Ordering::Release);gate.store(true,Ordering::Release);Ok(())}));
            assert!(result.is_err(),"{stage} exposed stale startup result");
        }
        Ok(())
    }
