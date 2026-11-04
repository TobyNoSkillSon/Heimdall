    //! Agent off lets crucial work finish, then stops ("When the user turns the Agent toggle off,
    //! Heimdall must first let crucial in-flight work finish, then stop. Crucial work means a running compaction, plus
    //! any tool call that is writing state … Show 'Stopping after compaction…' in the menu while it waits. Anything
    //! else stops at once."). The mock Pi holds its compaction while `hold-turn-compaction` exists; no model, no sound.
    use super::*;
    use agent::delivery::StopReason;
    fn rows(session:&Path)->Result<Vec<heimdall_archive::Record>> { heimdall_archive::Journal::open_read_only(session)?.records() }
    fn wait_for(session:&Path,what:&str,test:impl Fn(&[heimdall_archive::Record])->bool)->Result<()> {
        let began=std::time::Instant::now();
        loop{if test(&rows(session)?){return Ok(());}anyhow::ensure!(began.elapsed()<std::time::Duration::from_secs(30),"{what} did not arrive");std::thread::sleep(std::time::Duration::from_millis(20));}
    }
    fn stop(r:&heimdall_archive::Record,state:&str)->bool { r.kind=="agent_stop"&&r.payload["state"]==state }
    fn setup()->Result<(tempfile::TempDir,PathBuf,PathBuf,Arc<Tools>,AgentRuntime)> {
        let d=tempfile::tempdir()?;let data=d.path().join("data");let session=data.join("archive/session-stop");std::fs::create_dir_all(&data)?;
        test_harness(Some(Path::new(env!("CARGO_MANIFEST_DIR")).join("../agent/tests/fixtures/mock-adapter.py")));
        let tools=Arc::new(Tools::new(d.path(),&data));
        let result=AgentRuntime::start(Settings{dry_run:false,..Default::default()},data.clone(),session.clone(),tools.clone(),Some(ui::Ui::handle()));
        test_harness(None);let runtime=result?;runtime.barrier()?;
        Ok((d,data,session,tools,runtime))
    }
    /// Pi compacting inside a turn (its threshold compaction), held running by the mock Pi.
    fn compacting(data:&Path,session:&Path,runtime:&AgentRuntime)->Result<()> {
        std::fs::write(data.join("hold-turn-compaction"),b"hold")?;
        runtime.send(Command::Send{text:"a long conversation".into(),steer:false})?;
        wait_for(session,"the compaction",|r|r.iter().any(|e|e.kind=="agent_transport_reply"&&e.payload["wire"]["type"]=="compaction_start"))?;
        runtime.barrier()
    }
    fn compaction_ended(r:&heimdall_archive::Record)->bool {
        r.kind=="agent_transport_reply"&&r.payload["wire"]["type"]=="compaction_end"
    }
    #[test]
    fn agent_off_during_a_compaction_waits_for_it_then_stops() -> Result<()> {
        let (_d,data,session,tools,runtime)=setup()?;
        compacting(&data,&session,&runtime)?;
        tools.menu_intent(StopReason::Agent,false);
        wait_for(&session,"the wait",|r|r.iter().any(|e|stop(e,"waiting")))?;
        let waiting=rows(&session)?.into_iter().find(|e|stop(e,"waiting")).unwrap();
        assert_eq!(waiting.payload["for"],"compaction");
        assert_eq!(waiting.payload["line"],"Stopping after compaction…","the menu's Agent caption");
        // Off itself is immediate: nothing new is admitted while the compaction runs.
        assert!(tools.agent_enabled.permit().is_err() && runtime.send(Command::Send{text:"during the wait".into(),steer:false}).is_err());
        std::thread::sleep(std::time::Duration::from_millis(400));
        let held=rows(&session)?;
        assert!(!held.iter().any(|e|stop(e,"stopped")||stop(e,"interrupted")),"Off cut the compaction: {:?}",held.iter().filter(|e|e.kind=="agent_stop"||e.kind=="agent_reply").map(|e|e.payload.to_string().chars().take(160).collect::<String>()).collect::<Vec<_>>());
        assert!(runtime.inspect()?.is_some(),"Pi still runs the compaction");
        std::fs::remove_file(data.join("hold-turn-compaction"))?;
        wait_for(&session,"the stop",|r|r.iter().any(|e|stop(e,"stopped")))?;
        let after=rows(&session)?;
        let answered=after.iter().position(compaction_ended).expect("Pi finished the compaction");
        let stopped=after.iter().position(|e|stop(e,"stopped")).unwrap();
        assert!(answered<stopped,"stopped only after the compaction ended");
        assert!(!after.iter().any(|e|e.kind=="agent_transport_send"&&e.payload.to_string().contains("during the wait")),"a new message reached Pi");
        runtime.barrier()?;
        assert!(runtime.inspect()?.is_none(),"Pi stopped after the compaction");
        drop(runtime);Ok(())
    }
    #[test]
    fn agent_off_during_a_state_writing_tool_call_lets_it_finish() -> Result<()> {
        let (_d,_data,session,tools,runtime)=setup()?;
        // The tool call's admission, as the MCP server takes it for `schedule_wake` (agent::mcp).
        let gate=tools.agent_enabled.clone();
        let write=gate.crucial_write(&gate.permit()?)?;
        tools.menu_intent(StopReason::Agent,false);
        wait_for(&session,"the wait",|r|r.iter().any(|e|stop(e,"waiting")))?;
        let waiting=rows(&session)?.into_iter().find(|e|stop(e,"waiting")).unwrap();
        assert_eq!(waiting.payload["for"],"state_write");
        assert_eq!(waiting.payload["line"],"Stopping after saving…");
        // Its write runs after Off and completes; the harness has not stopped under it.
        let saved=tokio::runtime::Runtime::new()?.block_on(tools.mcp_actions.action(&tools,"schedule_wake",json!({"message":"check the build","in_minutes":30}),"op-stop-wait",write.permit()))?;
        assert!(saved.contains("check the build")||saved.to_lowercase().contains("wake"),"{saved}");
        let listed=tools.mcp_actions.wakes.as_ref().map_err(|e|anyhow::anyhow!(e.clone()))?.list_line()?;
        assert!(listed.contains("check the build"),"the wake was saved: {listed}");
        assert!(!rows(&session)?.iter().any(|e|stop(e,"stopped")),"the harness stopped during the write");
        drop(write);
        wait_for(&session,"the stop",|r|r.iter().any(|e|stop(e,"stopped")))?;
        runtime.barrier()?;
        assert!(runtime.inspect()?.is_none());
        drop(runtime);Ok(())
    }
    #[test]
    fn agent_off_with_nothing_crucial_stops_at_once() -> Result<()> {
        let (_d,_data,session,tools,runtime)=setup()?;
        assert!(runtime.inspect()?.is_some());
        tools.menu_intent(StopReason::Agent,false);
        let began=std::time::Instant::now();
        while runtime.inspect()?.is_some() {anyhow::ensure!(began.elapsed()<std::time::Duration::from_secs(5),"Pi still runs");std::thread::sleep(std::time::Duration::from_millis(10));}
        assert!(!rows(&session)?.iter().any(|e|e.kind=="agent_stop"),"no wait without crucial work");
        drop(runtime);Ok(())
    }
    /// Agent back on during the wait cancels the stop: the compaction still finishes, then Pi restarts in the new
    /// epoch (an old epoch is never revived), and what the user sent meanwhile reaches the restarted Pi, not the old one.
    #[test]
    fn agent_back_on_during_the_wait_cancels_the_stop() -> Result<()> {
        let (_d,data,session,tools,runtime)=setup()?;
        compacting(&data,&session,&runtime)?;
        let (old,_)=runtime.inspect()?.unwrap();
        tools.menu_intent(StopReason::Agent,false);
        wait_for(&session,"the wait",|r|r.iter().any(|e|stop(e,"waiting")))?;
        tools.menu_intent(StopReason::Agent,true);
        assert!(tools.agent_enabled.permit().is_ok());
        runtime.send(Command::Send{text:"said while it compacted".into(),steer:false})?;
        std::thread::sleep(std::time::Duration::from_millis(300));
        let held=rows(&session)?;
        assert!(!held.iter().any(|e|stop(e,"stopped")),"Agent on cut the compaction");
        assert!(!held.iter().any(|e|e.kind=="agent_transport_send"&&e.payload.to_string().contains("said while it compacted")),"input reached the stopping Pi");
        std::fs::remove_file(data.join("hold-turn-compaction"))?;
        wait_for(&session,"the restarted Pi's answer",|r|r.iter().any(|e|e.kind=="agent_transport_sent"&&e.payload.to_string().contains("said while it compacted")))?;
        let after=rows(&session)?;
        let answered=after.iter().position(compaction_ended).unwrap_or_else(||panic!("Pi finished the compaction: {:?}",after.iter().filter(|e|e.kind.starts_with("agent_transport")||e.kind=="agent_stop").map(|e|format!("{} {}",e.kind,e.payload.to_string().chars().take(140).collect::<String>())).collect::<Vec<_>>()));
        let stopped=after.iter().position(|e|stop(e,"stopped")).unwrap();
        let sent=after.iter().position(|e|e.kind=="agent_transport_sent"&&e.payload.to_string().contains("said while it compacted")).unwrap();
        assert!(answered<stopped&&stopped<sent);
        runtime.barrier()?;
        let (new,_)=runtime.inspect()?.expect("Agent is on again");
        assert_ne!(new,old,"the old epoch's Pi is not revived");
        assert!(!tools.agent_enabled.stopped_for(StopReason::Agent));
        drop(runtime);Ok(())
    }
    /// The menu's Agent caption and the window line while the wait lasts (the supervisor publishes this state).
    #[test]
    fn the_waiting_state_says_stopping_after_compaction_and_takes_nothing() -> Result<()> {
        let dir=tempfile::tempdir()?;let settings=Settings{dry_run:false,..Default::default()};
        let bridge=unavailable(&settings,dir.path(),&dir.path().join("archive/session-stop-state"),"fixture".into())?;
        for (compaction,line) in [(true,"Stopping after compaction…"),(false,"Stopping after saving…")] {
            let state=stopping_window_state(&bridge,&settings,0,None,Some(compaction));
            assert_eq!(state.state,ui::AgentRunState::Stopping{compaction});
            assert!(!state.can_send&&!state.can_stop&&!state.can_reload);
            assert_eq!(ui::agent_view::state_line(Some(&state)).0,line);
        }
        assert_ne!(agent_window_state(&bridge,&settings,0,None).state,ui::AgentRunState::Stopping{compaction:true});
        Ok(())
    }
