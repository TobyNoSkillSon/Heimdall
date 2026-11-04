    use super::*;
    fn wait_for(session:&Path,test:impl Fn(&[heimdall_archive::Record])->bool)->Result<()> {
        let began=std::time::Instant::now();loop{let rows=heimdall_archive::Journal::open_read_only(session)?.records()?;if test(&rows){return Ok(());}anyhow::ensure!(began.elapsed()<std::time::Duration::from_secs(5),"supervisor evidence did not arrive");std::thread::sleep(std::time::Duration::from_millis(20));}
    }
    fn setup() -> Result<(tempfile::TempDir, PathBuf, PathBuf, Arc<Tools>, AgentRuntime)> {
        let d=tempfile::tempdir()?; let data=d.path().join("data"); let session=data.join("archive/session-integration"); std::fs::create_dir_all(&data)?;
        test_harness(Some(Path::new(env!("CARGO_MANIFEST_DIR")).join("../agent/tests/fixtures/mock-adapter.py")));
        let tools=Arc::new(Tools::new(d.path(),&data));
        let result=AgentRuntime::start(Settings{dry_run:false,..Default::default()},data.clone(),session.clone(),tools.clone(),Some(ui::Ui::handle()));
        test_harness(None); let runtime=result?; runtime.barrier()?;
        Ok((d,data,session,tools,runtime))
    }
    #[test]
    fn settled_unspoken_final_reaches_the_live_context_and_survives_restore() -> Result<()> {
        let (dir,data,session,tools,runtime)=setup()?;
        *tools.archive_session.lock().unwrap()=Some(session.clone());
        tools.enabled.store(false,Ordering::Release);
        runtime.send(Command::Send{text:"ordinary".into(),steer:false})?;
        wait_for(&session,|rows|rows.iter().any(|row|row.kind=="send_context" && row.payload["turns"].as_array().is_some_and(|turns|turns.iter().any(|turn|turn["speaker"]=="Heimdall" && turn["text"]=="fixture reply"))))?;
        runtime.barrier()?;
        let context=tools.send_context.lock().unwrap();
        assert_eq!(context.snapshot().turns.iter().filter(|t|t.speaker=="Heimdall").count(),1);
        assert!(context.finished_ns.is_none());
        drop(context);
        let restored=Tools::new(dir.path(),&data);restored.restore_send_context()?;
        assert_eq!(restored.send_context.lock().unwrap().snapshot().turns.last().unwrap().text,"fixture reply");
        drop(runtime);Ok(())
    }
    /// Context the agent got without a turn (the start's catch-up and restart message, context-only notices).
    fn contexts(session:&Path)->Result<Vec<String>>{
        Ok(heimdall_archive::Journal::open_read_only(session)?.records()?.iter().filter(|e|e.kind=="agent_context").map(|e|e.payload["text"].as_str().unwrap_or_default().to_owned()).collect())
    }
    fn delivery_count(data: &Path, text: &str) -> Result<i64> {
        let db=rusqlite::Connection::open(data.join("pi-spool.sqlite"))?;
        let wires=db.prepare("SELECT wire FROM deliveries")?.query_map([],|r|r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        let session=data.join("archive/session-integration");
        let mut count=0;
        for wire in wires {let value:Value=serde_json::from_str(&wire)?;let body=if let Some(reference)=value["archive_ref"].as_str(){String::from_utf8(heimdall_archive::store::read_log(&session,reference)?)?}else{wire};if body.contains(text){count+=1;}}
        Ok(count)
    }
    #[test]
    fn joined_loop_guard_aborts_turn_journals_once_and_keeps_pi_session() -> Result<()> {
        let (_d,_data,session,tools,runtime)=setup()?;
        let initial=runtime.inspect()?.unwrap();
        runtime.send(Command::Send{text:"[fixture:loop-abort]".into(),steer:false})?;
        wait_for(&session,|r|r.iter().any(|e|e.kind=="agent_loop_guard"))?;
        runtime.barrier()?;
        assert_eq!(runtime.inspect()?.unwrap(),initial);
        let rows=heimdall_archive::Journal::open_read_only(&session)?.records()?;
        let stops:Vec<_>=rows.iter().filter(|r|r.kind=="agent_loop_guard").collect();
        assert_eq!(stops.len(),1);
        assert_eq!(stops[0].payload["tool"],"read");
        assert_eq!(stops[0].payload["repeats"],5);
        assert_eq!(stops[0].payload["blocked_attempts"],3);
        assert_eq!(stops[0].payload["delivery_epoch"].as_u64(),Some(tools.agent_enabled.generation()));
        assert_eq!(stops[0].payload["arguments_hash"].as_str().unwrap().len(),64);
        assert_eq!(rows.iter().filter(|r|r.payload["wire"]["type"]=="mock_control"&&r.payload["wire"]["command"]=="abort").count(),1);
        assert!(rows.iter().any(|r|r.kind=="agent_reply"&&r.payload["event"]["kind"]=="complete"&&r.payload["event"]["error"]==true&&r.payload["event"]["text"]==agent::harness::loop_policy().notice));
        // Real tool effects are held for review, never replayed or treated as a Pi crash.
        assert!(!rows.iter().any(|r|r.kind=="agent_reload"));
        drop(runtime); Ok(())
    }
    /// 18 Oct: Pi compacted inside replies (13:35, 13:49), 18-20 s of silence each. Near the threshold the history
    /// is compacted right after an answer, while idle, so the next reply does not wait for it (app tests skip the
    /// 30 s quiet wait; the simulator's `n12_…` covers it).
    #[test]
    fn history_near_the_threshold_compacts_after_an_answer_not_inside_the_next() -> Result<()> {
        let (_d,data,session,_tools,runtime)=setup()?;
        // Haiku's default budget (its whole 1M window, 28 Oct): the idle mark is 85% of it.
        let mark=agent::maintenance::CompactionPolicy::for_limits(agent::provider::ApiModel::haiku().limits(),false).mark;
        assert_eq!(mark,agent::provider::ApiModel::haiku().budget()*85/100);
        std::fs::write(data.join("context-tokens"),(mark-1).to_string())?;
        runtime.send(Command::Send{text:"below the mark".into(),steer:false})?;
        wait_for(&session,|r|r.iter().filter(|e|e.kind=="agent_reply"&&e.payload["event"]["kind"]=="complete").count()==1)?;
        runtime.barrier()?;
        let compacts=|rows:&[heimdall_archive::Record]|rows.iter().filter(|r|r.kind=="agent_transport_reply"&&r.payload["wire"]["type"]=="mock_control"&&r.payload["wire"]["command"]=="compact").count();
        assert_eq!(compacts(&heimdall_archive::Journal::open_read_only(&session)?.records()?),0);
        std::fs::write(data.join("context-tokens"),mark.to_string())?;
        runtime.send(Command::Send{text:"past the mark".into(),steer:false})?;
        wait_for(&session,|r|r.iter().any(|e|e.kind=="agent_compaction"&&e.payload["state"]=="idle")&&compacts(r)==1)?;
        runtime.barrier()?;
        let rows=heimdall_archive::Journal::open_read_only(&session)?.records()?;
        assert_eq!(compacts(&rows),1);
        let answered=rows.iter().rposition(|r|r.kind=="agent_reply"&&r.payload["event"]["kind"]=="complete").unwrap();
        let compacted=rows.iter().position(|r|r.kind=="agent_compaction").unwrap();
        assert!(answered<compacted,"the answer comes first");
        drop(runtime);Ok(())
    }
    /// Review of 18 Oct: a failed idle compaction errored out of Pi's event stream, the supervisor restarted Pi,
    /// the start compacted again, and the agent ended Failed. A failure is logged and held: Pi stays up, one paid
    /// attempt, no new attempt at a restart, and a later successful reply allows the next check.
    #[test]
    fn failed_idle_compaction_at_launch_keeps_the_agent_up_with_one_attempt() -> Result<()> {
        let d=tempfile::tempdir()?; let data=d.path().join("data"); let session=data.join("archive/session-integration"); std::fs::create_dir_all(&data)?;
        // 870,000: past Haiku's idle mark (850,000 of its default 1M budget), below Pi's own 900,000.
        for (name,body) in [("test-start-check",""),("compaction-fails",""),("context-tokens","870000")] { std::fs::write(data.join(name),body)?; }
        test_harness(Some(Path::new(env!("CARGO_MANIFEST_DIR")).join("../agent/tests/fixtures/mock-adapter.py")));
        let tools=Arc::new(Tools::new(d.path(),&data));
        let result=AgentRuntime::start(Settings{dry_run:false,..Default::default()},data.clone(),session.clone(),tools.clone(),Some(ui::Ui::handle()));
        test_harness(None); let runtime=result?; runtime.barrier()?;
        let compacts=|rows:&[heimdall_archive::Record]|rows.iter().filter(|r|r.kind=="agent_transport_reply"&&r.payload["wire"]["type"]=="mock_control"&&r.payload["wire"]["command"]=="compact").count();
        wait_for(&session,|r|r.iter().any(|e|e.kind=="agent_compaction"&&e.payload["state"]=="idle_failed"))?;
        runtime.barrier()?;
        let pid=runtime.inspect()?.unwrap().0;
        assert_eq!(compacts(&heimdall_archive::Journal::open_read_only(&session)?.records()?),1);
        // A reload reopens Pi with the same long history: the held failure makes no second attempt.
        runtime.send(Command::Reload{reason:"test reload".into(),abort:false,permit:None})?;
        wait_for(&session,|r|r.iter().any(|e|e.kind=="agent_reload"))?;
        runtime.barrier()?;
        let reloaded=runtime.inspect()?.unwrap().0; assert_ne!(pid,reloaded);
        assert_eq!(compacts(&heimdall_archive::Journal::open_read_only(&session)?.records()?),1,"no attempt at the restart");
        // The agent is up and answers; a successful reply allows one new check (which fails again here, once).
        runtime.send(Command::Send{text:"still there".into(),steer:false})?;
        wait_for(&session,|r|r.iter().any(|e|e.kind=="agent_reply"&&e.payload["event"]["kind"]=="complete"&&e.payload["event"]["error"]==false))?;
        // The failure's own record, not only the request: the barrier does not wait for Pi's events.
        wait_for(&session,|r|compacts(r)==2&&r.iter().filter(|e|e.kind=="agent_compaction"&&e.payload["state"]=="idle_failed").count()==2)?;
        runtime.barrier()?;
        let rows=heimdall_archive::Journal::open_read_only(&session)?.records()?;
        assert_eq!(compacts(&rows),2);
        assert_eq!(rows.iter().filter(|e|e.kind=="agent_compaction"&&e.payload["state"]=="idle_failed").count(),2);
        assert_eq!(runtime.inspect()?.unwrap().0,reloaded,"never restarted over a failed compaction");
        drop(runtime);Ok(())
    }
    /// The fixture Pi with these knob files in place before it starts.
    fn setup_with(knobs:&[(&str,&str)]) -> Result<(tempfile::TempDir, PathBuf, PathBuf, Arc<Tools>, AgentRuntime)> {
        let d=tempfile::tempdir()?; let data=d.path().join("data"); let session=data.join("archive/session-integration"); std::fs::create_dir_all(&data)?;
        for (name,body) in knobs { std::fs::write(data.join(name),body)?; }
        test_harness(Some(Path::new(env!("CARGO_MANIFEST_DIR")).join("../agent/tests/fixtures/mock-adapter.py")));
        let tools=Arc::new(Tools::new(d.path(),&data));
        let result=AgentRuntime::start(Settings{dry_run:false,..Default::default()},data.clone(),session.clone(),tools.clone(),Some(ui::Ui::handle()));
        test_harness(None); let runtime=result?;
        Ok((d,data,session,tools,runtime))
    }
    fn journal_until(session:&Path,what:&str,test:impl Fn(&[heimdall_archive::Record])->bool)->Result<()> {
        let began=std::time::Instant::now();
        loop{let rows=heimdall_archive::Journal::open_read_only(session).and_then(|j|j.records()).unwrap_or_default();if test(&rows){return Ok(());}anyhow::ensure!(began.elapsed()<std::time::Duration::from_secs(20),"{what}");std::thread::sleep(std::time::Duration::from_millis(20));}
    }
    fn journal_records(session:&Path)->Vec<heimdall_archive::Record>{heimdall_archive::Journal::open_read_only(session).and_then(|j|j.records()).unwrap_or_default()}
    fn count(path:&Path)->usize{std::fs::read(path).map_or(0,|b|b.len())}
    /// Night audit H1 (27 Oct 03:25-03:53): each restart compacted the same 230k-token history, the compaction died
    /// with Pi, and the supervisor restarted it, six times. A compaction that dies with its Pi counts as failed: the
    /// restart does not compact again, and the hold backs off.
    #[test]
    fn a_compaction_that_dies_with_pi_is_not_tried_again_at_the_restart() -> Result<()> {
        let (_d,data,session,_tools,runtime)=setup_with(&[("test-start-check",""),("compaction-crashes",""),("context-tokens","870000")])?;
        journal_until(&session,"no killed compaction",|r|r.iter().any(|e|e.kind=="agent_compaction"&&e.payload["state"]=="killed"))?;
        // The restart comes 1 s later; without the hold it compacts at once and dies again (then 2 s, 4 s).
        std::thread::sleep(std::time::Duration::from_millis(3500));
        runtime.barrier()?;
        assert_eq!(count(&data.join("compaction-crashes-count")),1,"one paid attempt, not one per restart");
        assert!(runtime.inspect()?.is_some(),"the restarted Pi is up");
        let rows=heimdall_archive::Journal::open_read_only(&session)?.records()?;
        let killed:Vec<_>=rows.iter().filter(|e|e.kind=="agent_compaction"&&e.payload["state"]=="killed").collect();
        assert_eq!(killed.len(),1);
        assert_eq!(killed[0].payload["held_s"],600);
        drop(runtime);Ok(())
    }
    /// Night audit H3 (27 Oct 03:46): every failure since launch counted toward "The agent could not recover", so the
    /// fourth one showed Failed and blocked typing while the budget (3 in 10 minutes) still restarted Pi. Failed is
    /// the budget's refusal; a restart that opens resets the count.
    #[test]
    fn restarts_minutes_apart_never_say_the_agent_could_not_recover() -> Result<()> {
        let (_d,data,session,_tools,runtime)=setup_with(&[("crash-on-context","")])?;
        let crashes=data.join("context-crashes");
        for k in 1..=4usize {
            let began=std::time::Instant::now();
            while count(&crashes)<k {anyhow::ensure!(began.elapsed()<std::time::Duration::from_secs(20),"crash {k} did not come");std::thread::sleep(std::time::Duration::from_millis(10));}
            // The next failure comes 11 minutes later on the budget's clock: the earlier restarts have aged out.
            std::fs::write(data.join("test-restart-clock"),(k as u64*11*60_000).to_string())?;
        }
        std::fs::remove_file(data.join("crash-on-context"))?;
        std::thread::sleep(std::time::Duration::from_millis(2500));
        runtime.barrier()?;
        assert_eq!(count(&crashes),4);
        assert!(runtime.inspect()?.is_some(),"the fifth Pi is up");
        assert!(journal_records(&session).iter().any(|e|e.kind=="agent_catchup"&&e.payload["state"]=="added"),"and took its start");
        let rows=heimdall_archive::Journal::open_read_only(&session)?.records()?;
        assert!(!rows.iter().any(|e|e.kind=="health"&&e.payload["cue"].as_str().is_some_and(|c|c.contains("could not recover"))),"Failed while the budget still restarted");
        drop(runtime);Ok(())
    }
    /// Review 27 Oct: a reopen that fails leaves the dead Pi in place, still reporting its compaction, and each failed
    /// reopen counted another kill (three reach the cap). One compaction, one kill.
    #[test]
    fn a_killed_compaction_counts_once_when_the_reopen_fails() -> Result<()> {
        let (_d,data,session,_tools,runtime)=setup_with(&[("test-start-check",""),("compaction-crashes",""),("context-tokens","870000")])?;
        journal_until(&session,"no killed compaction",|r|r.iter().any(|e|e.kind=="agent_compaction"&&e.payload["state"]=="killed"))?;
        // The restarts 1 s and 2 s later fail before a new Pi exists (a session pointer it must refuse), so the dead
        // Pi stays in place.
        std::fs::write(data.join("agent/active-session.json"),"\"/elsewhere/pi-session.jsonl\"")?;
        let failed=|r:&[heimdall_archive::Record]|r.iter().filter(|e|e.kind=="health"&&e.payload["component"]=="agent-supervisor").count();
        journal_until(&session,"the reopens did not fail",|r|failed(r)>=3)?;
        runtime.barrier()?;
        let rows=journal_records(&session);
        assert_eq!(rows.iter().filter(|e|e.kind=="agent_compaction"&&e.payload["state"]=="killed").count(),1,"counted once");
        assert_eq!(count(&data.join("compaction-crashes-count")),1);
        drop(runtime);Ok(())
    }
    /// Review 27 Oct: Agent off while a model switch waited for a compaction lost the switch: after the wait the
    /// re-sent restart failed its permit before it saved the settings. They are kept, and the next start runs them.
    #[test]
    fn agent_off_during_a_parked_model_switch_keeps_the_switch_for_the_next_start() -> Result<()> {
        let (_d,data,session,tools,runtime)=setup_with(&[("test-start-check",""),("hold-compaction",""),("context-tokens","870000")])?;
        journal_until(&session,"no compaction",|r|r.iter().any(|e|e.kind=="agent_transport_send"&&e.payload["wire"]["type"]=="compact"))?;
        runtime.barrier()?;
        let pid=runtime.inspect()?.unwrap().0;
        let settings=||->Result<Settings>{let (t,r)=oneshot::channel();runtime.send(Command::InspectSettings(t))?;Ok(r.blocking_recv()?)};
        runtime.send(Command::Restart(Settings{dry_run:false,engine_model:"switched-model".into(),..Default::default()}))?;
        journal_until(&session,"the switch did not wait",|r|r.iter().any(|e|e.kind=="agent_restart_wait"&&e.payload["state"]=="waiting"))?;
        tools.agent_enabled.intent(agent::delivery::StopReason::Agent,false);
        runtime.barrier()?;
        std::fs::remove_file(data.join("hold-compaction"))?;
        journal_until(&session,"the switch was not kept",|r|r.iter().any(|e|e.kind=="agent_restart"&&e.payload["state"]=="saved"))?;
        runtime.barrier()?;
        assert!(journal_records(&session).iter().any(|e|e.kind=="agent_stop"&&e.payload["state"]=="stopped"),"the compaction finished first");
        assert_eq!(settings()?.engine_model,"switched-model");
        assert!(runtime.inspect()?.is_none(),"Agent off: nothing starts");
        tools.agent_enabled.intent(agent::delivery::StopReason::Agent,true);
        let began=std::time::Instant::now();
        while runtime.inspect()?.is_none_or(|(p,_)|p==pid) {anyhow::ensure!(began.elapsed()<std::time::Duration::from_secs(10),"Agent on did not start Pi");std::thread::sleep(std::time::Duration::from_millis(20));}
        assert_eq!(settings()?.engine_model,"switched-model","the next start runs the switch");
        drop(runtime);Ok(())
    }
    /// Night audit H4 (27 Oct 03:24): a model switch restarted Pi in the middle of a compaction and threw the summary
    /// away. The restart waits for the compaction, as Agent off does; then the new Pi starts. Review 27 Oct: the wait's
    /// own clock (here 15 minutes past the switch) never cuts a compaction the harness still lets run.
    #[test]
    fn a_model_switch_waits_for_a_running_compaction() -> Result<()> {
        let (_d,data,session,_tools,runtime)=setup_with(&[("test-start-check",""),("hold-compaction",""),("context-tokens","870000"),("test-stop-since","901")])?;
        journal_until(&session,"no compaction",|r|r.iter().any(|e|e.kind=="agent_transport_send"&&e.payload["wire"]["type"]=="compact"))?;
        runtime.barrier()?;
        let pid=runtime.inspect()?.unwrap().0;
        runtime.send(Command::Restart(Settings{dry_run:false,..Default::default()}))?;
        runtime.barrier()?;
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert_eq!(runtime.inspect()?.unwrap().0,pid,"the compaction's Pi keeps running");
        assert!(heimdall_archive::Journal::open_read_only(&session)?.records()?.iter().any(|e|e.kind=="agent_restart_wait"&&e.payload["state"]=="waiting"));
        std::fs::remove_file(data.join("hold-compaction"))?;
        let began=std::time::Instant::now();
        while runtime.inspect()?.is_none_or(|(p,_)|p==pid) {anyhow::ensure!(began.elapsed()<std::time::Duration::from_secs(10),"the restart never followed");std::thread::sleep(std::time::Duration::from_millis(20));}
        let rows=heimdall_archive::Journal::open_read_only(&session)?.records()?;
        assert!(rows.iter().any(|e|e.kind=="agent_restart_wait"&&e.payload["state"]=="stopped"),"the compaction finished first");
        drop(runtime);Ok(())
    }
    #[test]
    fn joined_reload_me_checkpoints_tool_result_replaces_mcp_and_resumes_receipt() -> Result<()> {
        let (_d,data,session,tools,runtime)=setup()?;
        let first=runtime.inspect()?.unwrap().0;
        runtime.send(Command::Send{text:"[fixture:reload-me]".into(),steer:false})?;
        wait_for(&session,|r|r.iter().any(|e|e.kind=="agent_catchup"&&e.payload["state"]=="added"&&e.payload["reason"].as_str().is_some_and(|s|s.contains("fake Pi integration"))))?;
        runtime.barrier()?; assert_ne!(first,runtime.inspect()?.unwrap().0);
        let rows=heimdall_archive::Journal::open_read_only(&session)?.records()?;
        let tool=rows.iter().position(|e|e.kind=="agent_transport_reply"&&e.payload["wire"]["type"]=="tool_execution_end").unwrap();
        let settled=rows.iter().enumerate().skip(tool+1).find(|(_,e)|e.kind=="agent_transport_reply"&&e.payload["wire"]["type"]=="agent_settled").unwrap().0;
        let reload=rows.iter().position(|e|e.kind=="agent_reload").unwrap(); assert!(tool<settled&&settled<reload);
        // E4.1: the reload's reason is the line before the restart message; nothing continues by itself.
        let note=contexts(&session)?.into_iter().rfind(|c|c.contains("fake Pi integration")).context("no reload context")?;
        assert!(note.contains("Reloaded: ")&&!note.contains("reload-")&&note.contains("you have just been restarted")&&note.ends_with("Don't do anything unless User talks to you."),"{note}");
        let actions=tools.mcp_actions.actions.as_ref().unwrap();
        let db=rusqlite::Connection::open(data.join("agent/mcp-actions.sqlite"))?;
        let (id,state):(String,String)=db.query_row("SELECT id,state FROM receipts WHERE operation='stable-fake-reload-call'",[],|r|Ok((r.get(0)?,r.get(1)?)))?;
        assert_eq!(state,"completed"); assert_eq!(actions.state(&id)?,"completed");
        assert!(std::fs::read_to_string(data.join("pi-session.jsonl"))?.contains("toolResult"));
        assert!(!data.join("agent/pending-reload.json").exists()); drop(runtime); Ok(())
    }
    #[test]
    fn joined_notice_dedup_survives_pi_reload_and_runtime_relaunch() -> Result<()> {
        let (_d,data,session,tools,runtime)=setup()?;
        let p=tools.agent_enabled.permit()?;
        let actions=tools.mcp_actions.actions.as_ref().unwrap().clone();
        actions.enqueue("joined-notice","unique_worker_result",&p)?; actions.drain(&p)?;
        // Before the user has spoken after the start, a worker event is context (E4.3).
        wait_for(&session,|r|r.iter().any(|e|e.kind=="agent_context"&&e.payload["text"].as_str().is_some_and(|s|s.contains("unique_worker_result"))))?;
        runtime.reload("dedup integration")?;
        wait_for(&session,|r|r.iter().any(|e|e.kind=="agent_catchup"&&e.payload["state"]=="added"&&e.payload["reason"].as_str().is_some_and(|s|s.contains("dedup integration"))))?;
        tools.notice("joined-notice","unique_worker_result",&p)?; assert_eq!(delivery_count(&data,"unique_worker_result")?,1);
        drop(runtime);
        test_harness(Some(Path::new(env!("CARGO_MANIFEST_DIR")).join("../agent/tests/fixtures/mock-adapter.py")));
        let runtime=AgentRuntime::start(Settings{dry_run:false,..Default::default()},data.clone(),session,tools.clone(),None)?; test_harness(None); runtime.barrier()?;
        tools.notice("joined-notice","unique_worker_result",&p)?; assert_eq!(delivery_count(&data,"unique_worker_result")?,1); drop(runtime); Ok(())
    }
    /// A read still waiting when Heimdall quit can never finish: the next start tells the agent it was dismissed,
    /// exactly once.
    #[test]
    fn stale_fast_read_is_reported_dismissed_once_after_restart() -> Result<()> {
        let (_d,data,session,tools,runtime)=setup()?;
        let p=tools.agent_enabled.permit()?; let actions=tools.mcp_actions.actions.as_ref().unwrap().clone();
        // As the previous process left it: shown on the island, not waiting in this process.
        let id=actions.receipt("old-read","fastread",&serde_json::to_string(&("Long text.",Some("Build failure")))?,None,&p)?;
        actions.set_state(&id,"shown",&p)?;
        drop(runtime);
        for _ in 0..2 {
            test_harness(Some(Path::new(env!("CARGO_MANIFEST_DIR")).join("../agent/tests/fixtures/mock-adapter.py")));
            let runtime=AgentRuntime::start(Settings{dry_run:false,..Default::default()},data.clone(),session.clone(),tools.clone(),None)?; test_harness(None); runtime.barrier()?;
            wait_for(&session,|r|r.iter().any(|e|e.kind=="agent_context"&&e.payload.to_string().contains("dismissed (Heimdall restarted)")))?;
            drop(runtime);
        }
        assert_eq!(delivery_count(&data,"dismissed (Heimdall restarted)")?,1,"reported exactly once across two restarts");
        assert_eq!(delivery_count(&data,"Build failure")?,1,"the report names the read");
        assert_eq!(actions.state(&id)?,"completed");
        Ok(())
    }
    // Fast reading (rsvp-reader).
    #[test]
    fn joined_fast_read_result_reaches_the_agent_once_and_remembers_the_speed() -> Result<()> {
        let (_d,data,session,tools,runtime)=setup()?;
        tools.fast_reads.attach_ui(Some(ui::Ui::handle()));
        tools.fast_reads.set_embedder(Arc::new(|_: &str| Some(vec![0.5f32; 768])));
        let p=tools.agent_enabled.permit()?; let actions=tools.mcp_actions.actions.as_ref().unwrap();
        let args=json!({"text":"The build failed because tests timed out. Three things matter.","title":"Build failure"});
        agent::mcp_contract::validate_tool("fast_read",&args)?;
        assert!(agent::mcp_contract::validate_tool("fast_read",&json!({"text":"x","title":"a title far longer than forty characters in total"})).is_err());
        let reply=crate::fast_read::start(&tools,actions,&args,"fast-read-call",&p)?;
        assert!(reply.starts_with("Fast read pending: fastread-"),"{reply}");
        let id=reply.split_whitespace().nth(3).unwrap().trim_end_matches('.').to_string();
        assert_eq!(actions.state(&id)?,"shown"); assert!(tools.fast_reads.pending(&id));
        // A replayed call is the same pending read, not a second one.
        assert_eq!(crate::fast_read::start(&tools,actions,&args,"fast-read-call",&p)?,reply);
        let result=ui::reader::ReadResult{id:id.clone(),outcome:ui::reader::ReadOutcome::Read,start_wpm:240,settled_wpm:265,rewinds:1,progress_pct:100};
        crate::fast_read::finish_now(&tools,&result)?;
        // S11 / E1.10: a fast-read result is context for the next turn, never a turn of its own.
        wait_for(&session,|r|r.iter().any(|e|e.kind=="agent_context"&&e.payload.to_string().contains("read at 265 wpm")))?;
        assert_eq!(delivery_count(&data,"read at 265 wpm")?,1);
        runtime.barrier()?;
        let rows=heimdall_archive::Journal::open_read_only(&session)?.records()?;
        assert!(!rows.iter().any(|e|e.kind=="agent_send"&&e.payload.to_string().contains("read at 265 wpm")),"the result started a turn");
        assert_eq!(actions.state(&id)?,"completed"); assert!(!tools.fast_reads.pending(&id));
        // A second click on a finished read is ignored.
        crate::fast_read::finish_now(&tools,&result)?;
        assert_eq!(delivery_count(&data,"read at 265 wpm")?,1);
        let history=tools.fast_reads.speeds.history();
        assert_eq!((history.len(),history[0].settled_wpm,history[0].rewinds),(1,265,1));
        assert_eq!(history[0].embedding.as_ref().map(Vec::len),Some(768));
        assert!(data.join("reading/speeds.jsonl").is_file());
        // Closed on the ready pill: the agent hears "dismissed"; no speed is stored.
        let reply=crate::fast_read::start(&tools,actions,&json!({"text":"Another note."}),"fast-read-2",&p)?;
        let id=reply.split_whitespace().nth(3).unwrap().trim_end_matches('.').to_string();
        crate::fast_read::finish_now(&tools,&ui::reader::ReadResult{id,outcome:ui::reader::ReadOutcome::Dismissed,start_wpm:240,settled_wpm:240,rewinds:0,progress_pct:0})?;
        wait_for(&session,|r|r.iter().any(|e|e.kind=="agent_context"&&e.payload.to_string().contains("Fast read: dismissed.")))?;
        assert_eq!(tools.fast_reads.speeds.history().len(),1);
        drop(runtime); Ok(())
    }
    #[test]
    fn joined_question_click_delivers_once_and_old_clicks_cannot_revive() -> Result<()> {
        let (_d,data,session,tools,runtime)=setup()?; tools.notice_actions_available.store(true,Ordering::Release);
        let p=tools.agent_enabled.permit()?; let actions=tools.mcp_actions.actions.as_ref().unwrap();
        let yes_no=["Yes".to_string(),"No".to_string()];
        let id=actions.ask("question-click",&asked("Publish the prepared change?",&yes_no),&p)?;
        let retained=actions.ask("another-question",&asked("Retain this separate question?",&yes_no),&p)?;
        runtime.notice_action(ui::agent_notice::AgentNoticeAction::Answer{request_id:id.clone(),answers:vec![ui::agent_notice::QuestionAnswer{question_id:"q1".into(),labels:vec!["No".into()],other:None}], by_hand: vec![]})?;
        wait_for(&session,|r|r.iter().any(|e|e.kind=="agent_send"&&e.payload.to_string().contains("User clicked No.")))?;
        assert_eq!(actions.state(&id)?,"answered"); assert_eq!(delivery_count(&data,"User clicked No.")?,1);
        // A second answer to a committed card is no error and no second delivery (R16).
        tools.answer_question(&id,&one_answer("Yes"),&[])?;
        assert_eq!(delivery_count(&data,"User clicked Yes.")?,0);
        assert!(tools.pending_questions.lock().unwrap().contains_key(&retained));
        let stale=actions.ask("stale-click",&asked("Old question",&yes_no),&p)?;
        tools.agent_enabled.store(false,Ordering::Release); tools.agent_enabled.store(true,Ordering::Release);
        assert!(tools.answer_question(&stale,&one_answer("Yes"),&[]).is_err()); runtime.barrier()?;
        assert!(tools.pending_questions.lock().unwrap().is_empty());
        assert_eq!(actions.state(&stale)?,"cancelled"); assert_eq!(delivery_count(&data,"User clicked Yes.")?,0); drop(runtime); Ok(())
    }
    /// Night review 20 Oct (6), through the real supervisor: a reply still waiting out the user's pause when his words
    /// reach the agent is dropped unheard, and the wake says it never reached him (night pass 1022 wording).
    #[test]
    fn joined_owner_s_words_drop_the_reply_waiting_for_him() -> Result<()> {
        let (_d,_data,session,tools,runtime)=setup()?;
        struct Synth;
        impl speak::Synthesizer for Synth {
            fn synthesize(&mut self,_:&str,_:&str,_:&dyn Fn()->bool)->Result<speak::Audio>{Ok(speak::Audio{samples:vec![0.1;160],sample_rate:16_000,first_audio_ms:0.,synthesis_ms:0.})}
        }
        struct Speaker;
        impl speak::AudioOutput for Speaker { fn output(&mut self,_:&speak::Audio,_:&dyn Fn()->bool)->Result<()>{Ok(())} }
        tools.test_guarded_speech_queue(super::hold::HeldSynth{inner:Synth,owner:tools.owner.clone()},super::hold::HeldOutput{inner:Speaker,owner:tools.owner.clone()});
        // His pause, long enough to outlast the wake's delivery (a line that would start while he talks is skipped).
        tools.owner.set_quiet(std::time::Duration::from_secs(3));
        tools.voice("mic:1",true);tools.voice("mic:1",false);
        let reply="The widget thread is started.";
        agent::mcp::ToolHost::speak(&*tools,reply,"en",&tools.agent_enabled.permit()?)?;
        std::thread::sleep(std::time::Duration::from_millis(100));
        let (start,text)=(capture::host_now_ns(),"actually make it a separate project");
        runtime.send(Command::Observe(Observation{id:"voice-1".into(),timestamp_ms:agent::now_ms(),source:"Mic".into(),speaker:"User".into(),language:"en".into(),text:text.into()}))?;
        let through={let mut c=tools.send_context.lock().unwrap();c.observe_at("User",text,start+1,json!({"id":"voice-1","start_ns":start}))};
        runtime.wake_pending_through_with_permit(vec![],false,through,Some("voice-1".into()),agent::Speaker::Owner,tools.agent_enabled.permit()?)?;runtime.barrier()?;
        tools.voice("mic:1",false);
        wait_for(&session,|r|r.iter().any(|e|e.kind=="agent_send"&&e.payload.to_string().contains("separate project")))?;
        let wake=heimdall_archive::Journal::open_read_only(&session)?.records()?.into_iter().rev().find(|e|e.kind=="agent_send"&&e.payload.to_string().contains("separate project")).unwrap().payload.to_string();
        assert!(wake.contains("This never reached User")&&wake.contains("The widget thread is started."),"{wake}");
        std::thread::sleep(std::time::Duration::from_millis(3_300));
        tools.speech_status();
        assert!(!tools.send_context.lock().unwrap().snapshot().turns.iter().any(|t|t.speaker=="Heimdall"&&t.text==reply),"the dropped reply was never said");
        drop(runtime);Ok(())
    }
    /// Instant acknowledgement, through the real supervisor: the wake carrying the user's words says which
    /// ack Heimdall already said and when, once; a wake for someone else's line does not take it.
    #[test]
    fn the_wake_for_owner_s_words_says_the_ack_already_said() -> Result<()> {
        let (_d,_data,session,tools,runtime)=setup()?;
        let (start,text)=(capture::host_now_ns(),"rename the island file");
        runtime.send(Command::Observe(Observation{id:"voice-ack".into(),timestamp_ms:agent::now_ms(),source:"Mic".into(),speaker:"User".into(),language:"en".into(),text:text.into()}))?;
        let through={let mut c=tools.send_context.lock().unwrap();c.observe_at("User",text,start+1,json!({"id":"voice-ack","start_ns":start}))};
        *tools.ack_said.lock().unwrap()=Some((through,"[Heimdall already said: \"On it.\" at 16:04:10]".into()));
        runtime.wake_pending_through_with_permit(vec![],false,through,Some("voice-ack".into()),agent::Speaker::Owner,tools.agent_enabled.permit()?)?;runtime.barrier()?;
        wait_for(&session,|r|r.iter().any(|e|e.kind=="agent_send"&&e.payload.to_string().contains("rename the island")))?;
        let wake=heimdall_archive::Journal::open_read_only(&session)?.records()?.into_iter().rev().find(|e|e.kind=="agent_send"&&e.payload.to_string().contains("rename the island")).unwrap().payload.to_string();
        assert!(wake.contains(r#"[Heimdall already said: \"On it.\" at 16:04:10]"#),"{wake}");
        assert!(tools.ack_said.lock().unwrap().is_none(),"said once");
        drop(runtime);Ok(())
    }
    /// Night review A: a wake for someone else's line (a caller d1 sent) never drops the reply waiting for the user.
    #[test]
    fn joined_someone_elses_line_keeps_the_reply_waiting_for_owner() -> Result<()> {
        let (_d,_data,session,tools,runtime)=setup()?;
        struct Synth;
        impl speak::Synthesizer for Synth {
            fn synthesize(&mut self,_:&str,_:&str,_:&dyn Fn()->bool)->Result<speak::Audio>{Ok(speak::Audio{samples:vec![0.1;160],sample_rate:16_000,first_audio_ms:0.,synthesis_ms:0.})}
        }
        struct Speaker;
        impl speak::AudioOutput for Speaker { fn output(&mut self,_:&speak::Audio,_:&dyn Fn()->bool)->Result<()>{Ok(())} }
        tools.test_guarded_speech_queue(super::hold::HeldSynth{inner:Synth,owner:tools.owner.clone()},super::hold::HeldOutput{inner:Speaker,owner:tools.owner.clone()});
        tools.owner.set_quiet(std::time::Duration::from_secs(3));
        tools.voice("mic:1",true);tools.voice("mic:1",false);
        let reply="The widget thread is started.";
        agent::mcp::ToolHost::speak(&*tools,reply,"en",&tools.agent_enabled.permit()?)?;
        std::thread::sleep(std::time::Duration::from_millis(100));
        let (start,text)=(capture::host_now_ns(),"did you see the new map");
        runtime.send(Command::Observe(Observation{id:"call-1".into(),timestamp_ms:agent::now_ms(),source:"Discord".into(),speaker:"Angel".into(),language:"en".into(),text:text.into()}))?;
        let through={let mut c=tools.send_context.lock().unwrap();c.observe_at("Angel",text,start+1,json!({"id":"call-1","start_ns":start}))};
        runtime.wake_pending_through_with_permit(vec![],false,through,Some("call-1".into()),agent::Speaker::Other,tools.agent_enabled.permit()?)?;runtime.barrier()?;
        wait_for(&session,|r|r.iter().any(|e|e.kind=="agent_send"&&e.payload.to_string().contains("new map")))?;
        let wake=heimdall_archive::Journal::open_read_only(&session)?.records()?.into_iter().rev().find(|e|e.kind=="agent_send"&&e.payload.to_string().contains("new map")).unwrap().payload.to_string();
        assert!(!wake.contains("never reached User"),"{wake}");
        std::thread::sleep(std::time::Duration::from_millis(3_600));
        tools.speech_status();
        assert!(tools.send_context.lock().unwrap().snapshot().turns.iter().any(|t|t.speaker=="Heimdall"&&t.text==reply),"the reply was said once User was quiet");
        drop(runtime);Ok(())
    }
    /// 19 Oct 18:44–18:47 (bug notes 18-46-19, 18-48-21), through the real supervisor: the user's spoken answer closes the
    /// card at its wake, and the wake says so; his late click is dropped; a click made while he was talking is
    /// overridden in the wake; a guarded confirmation stays up; speech from before the card leaves it open.
    #[test]
    fn talk_leaves_the_card_open_and_a_click_still_answers_it() -> Result<()> {
        let (_d,data,session,tools,runtime)=setup()?; tools.notice_actions_available.store(true,Ordering::Release);
        let p=tools.agent_enabled.permit()?; let actions=tools.mcp_actions.actions.as_ref().unwrap();
        let opts=["Yes".to_string(),"Later".to_string(),"No".to_string()];
        let mut n=0;
        // The user's line, delivered in a wake; `for_heimdall`: d1, his name or "send it" sent it (the pipeline's mark).
        let mut say_as=|text:&str,start:u64,end:u64,for_heimdall:bool|->Result<()> {
            n+=1; let id=format!("voice-{n}");
            runtime.send(Command::Observe(Observation{id:id.clone(),timestamp_ms:agent::now_ms(),source:"Mic".into(),speaker:"User".into(),language:"en".into(),text:text.into()}))?;
            let through={let mut c=tools.send_context.lock().unwrap();c.observe_at("User",text,end,json!({"id":id,"start_ns":start}))};
            if for_heimdall {tools.spoke_to_heimdall(through);}
            runtime.wake_pending_through_with_permit(vec![],false,through,Some(id),agent::Speaker::Owner,tools.agent_enabled.permit()?)?;runtime.barrier()
        };
        let sent=|words:&str|->Result<String>{wait_for(&session,|r|r.iter().any(|e|e.kind=="agent_send"&&e.payload.to_string().contains(words)))?;
            Ok(heimdall_archive::Journal::open_read_only(&session)?.records()?.into_iter().rev().find(|e|e.kind=="agent_send"&&e.payload.to_string().contains(words)).unwrap().payload.to_string())};
        // Speech that ended before the card appeared does not close it.
        let before=capture::host_now_ns();
        let widget=actions.ask("q-widget",&asked("Start a thread on one test desktop widget?",&opts),&p)?;
        say_as("before: that would be really cool actually",before-2_000_000_000,before-1_000_000_000,true)?;
        sent("that would be really cool")?;
        assert_eq!(actions.state(&widget)?,"shown");
        // Talk while it is up: the card stays open and the wake says so (25 Oct: talk never closes a card).
        let now=capture::host_now_ns();
        say_as("Yes, but let's keep it as a separate project",now,now+1,true)?;
        let wake=sent("separate project")?;
        assert!(wake.contains("stays open"),"{wake}");
        assert_eq!(actions.state(&widget)?,"shown");
        // A click answers it, as at any time while it is open.
        tools.answer_question(&widget,&one_answer("Later"),&[])?; sent("clicked Later")?;
        assert_eq!(actions.state(&widget)?,"answered");
        assert_eq!(delivery_count(&data,"clicked Later")?,1);
        // A click while he is talking answers the card: his words after it do not override it.
        let stop=actions.ask("q-stop",&asked("Stop the widget thread?",&["Keep going".to_string(),"Stop it".to_string()]),&p)?;
        let talking_from=capture::host_now_ns();
        tools.answer_question(&stop,&one_answer("Stop it"),&[])?;
        sent("clicked Stop it")?;
        let now=capture::host_now_ns();
        say_as("no no keep it going",talking_from,now,true)?;
        let wake=sent("keep it going")?;
        assert!(!wake.contains("override"),"{wake}");
        assert_eq!(actions.state(&stop)?,"answered");
        assert_eq!(delivery_count(&data,"clicked Stop it")?,1);
        // A guarded T3 confirmation: his voice reaches the agent but only a click closes it.
        actions.require_click("Delete project 'Vella'?");
        let guard=actions.ask("q-guard",&asked("Delete project 'Vella'?",&["Yes".to_string(),"No".to_string()]),&p)?;
        let now=capture::host_now_ns();
        say_as("yes delete it",now,now+1,true)?;
        assert!(sent("yes delete it")?.contains("stays open: only his click approves it"));
        assert_eq!(actions.state(&guard)?,"shown");
        assert!(tools.pending_questions.lock().unwrap().contains_key(&guard));
        // The agent's own ask_close lands before the host's close: one close, no second note, and a click after it
        // is dropped.
        tools.answer_question(&guard,&one_answer("No"),&[])?; sent("clicked No")?;
        let race=actions.ask("q-race",&asked("Install the new build?",&["Yes".to_string(),"No".to_string()]),&p)?;
        assert_eq!(actions.close_question(&race,"yes",&p)?,"closed");
        let now=capture::host_now_ns();
        say_as("yes install it",now,now+1,true)?;
        assert!(!sent("yes install it")?.contains("Install the new build?"));
        assert!(!tools.pending_questions.lock().unwrap().contains_key(&race));
        tools.answer_question(&race,&one_answer("No"),&[])?; runtime.barrier()?;
        assert_eq!(delivery_count(&data,"clicked No")?,1,"only the guard's click");
        drop(runtime); Ok(())
    }
    /// Review 20 Oct (items 3, 7, 8), through the real supervisor: only speech that was for Heimdall closes a card or
    /// overrides a click. An aside or TV line riding along in a wake, and a filler, leave the card up and the click
    /// standing; a card the agent closed with `ask_close` leaves the host's list, so it neither takes his next words
    /// (the card on screen does) nor keeps the lower d1 bar alive.
    #[test]
    fn talk_for_heimdall_leaves_cards_open_and_asides_never_touch_them() -> Result<()> {
        let (_d,data,session,tools,runtime)=setup()?; tools.notice_actions_available.store(true,Ordering::Release);
        let p=tools.agent_enabled.permit()?; let actions=tools.mcp_actions.actions.as_ref().unwrap();
        let yes_no=["Yes".to_string(),"No".to_string()];
        let mut n=0;
        let mut say_as=|text:&str,start:u64,end:u64,for_heimdall:bool|->Result<()> {
            n+=1; let id=format!("aside-{n}");
            runtime.send(Command::Observe(Observation{id:id.clone(),timestamp_ms:agent::now_ms(),source:"Mic".into(),speaker:"User".into(),language:"en".into(),text:text.into()}))?;
            let through={let mut c=tools.send_context.lock().unwrap();c.observe_at("User",text,end,json!({"id":id,"start_ns":start}))};
            if for_heimdall {tools.spoke_to_heimdall(through);}
            runtime.wake_pending_through_with_permit(vec![],false,through,Some(id),agent::Speaker::Owner,tools.agent_enabled.permit()?)?;runtime.barrier()
        };
        let sent=|words:&str|->Result<String>{wait_for(&session,|r|r.iter().any(|e|e.kind=="agent_send"&&e.payload.to_string().contains(words)))?;
            Ok(heimdall_archive::Journal::open_read_only(&session)?.records()?.into_iter().rev().find(|e|e.kind=="agent_send"&&e.payload.to_string().contains(words)).unwrap().payload.to_string())};
        // An aside (not for Heimdall) delivered with a wake: the card stays up, no closed line.
        let card=actions.ask("q-aside",&asked("Delete AllForOne's heavy files and archive the rest?",&yes_no),&p)?;
        let now=capture::host_now_ns();
        say_as("Alex, are you coming tonight",now,now+1,false)?;
        let wake=sent("are you coming tonight")?;
        assert!(!wake.contains("stays open"),"{wake}");
        assert_eq!(actions.state(&card)?,"shown");
        // A filler, even one d1 would have sent: the card stays up.
        let now=capture::host_now_ns();
        say_as("Um.",now,now+1,true)?;
        assert!(!sent("Um.")?.contains("stays open"));
        assert_eq!(actions.state(&card)?,"shown");
        // His click while the aside was being said: the click stands, nothing overrides it.
        let talking_from=capture::host_now_ns();
        tools.answer_question(&card,&one_answer("No"),&[])?; sent("clicked No")?;
        let now=capture::host_now_ns();
        say_as("yeah the refund came yesterday",talking_from,now,false)?;
        assert!(!sent("refund came yesterday")?.contains("override"));
        assert_eq!(delivery_count(&data,"his words override that click")?,0);
        assert_eq!(actions.state(&card)?,"answered");
        // Item 7: the agent closes the older card itself; the newer card on screen takes his next words.
        let older=actions.ask("q-older",&asked("Start a thread on one test desktop widget?",&yes_no),&p)?;
        let newer=actions.ask("q-newer",&asked("Install the new build?",&yes_no),&p)?;
        let rt=tokio::runtime::Runtime::new()?;
        let reply=rt.block_on(tools.mcp_actions.action(&tools,"ask_close",json!({"request_id":older,"answer":"yes"}),"close-older",&p))?;
        assert_eq!(reply,"Question closed.");
        assert!(!tools.pending_questions.lock().unwrap().contains_key(&older),"ask_close leaves the host's list");
        let now=capture::host_now_ns();
        say_as("yes install it",now,now+1,true)?;
        let wake=sent("yes install it")?;
        assert!(wake.contains("Install the new build?")&&wake.contains("stays open"),"{wake}");
        assert_eq!(actions.state(&newer)?,"shown");
        assert_eq!(actions.ask_answer(&older)?.unwrap().1,"yes","the agent's own close stands");
        // The newer card is still up: his talk does not close it, so d1 sees a card.
        let now=capture::host_now_ns();
        assert_ne!(tools.card_state(now,now+1),crate::bridge::questions::CardState::None);
        // A card closed straight in the receipts (any path that bypasses the host) is skipped too.
        actions.close_question(&newer,"no",&p)?;
        let gone=actions.ask("q-gone",&asked("Push it too?",&yes_no),&p)?;
        actions.close_question(&gone,"no",&p)?;
        assert!(tools.open_cards().iter().all(|c| c.id != gone), "a closed card is not open");
        assert!(!tools.pending_questions.lock().unwrap().contains_key(&gone));
        // A card the island is drawing right now (receipt not yet marked shown) stays in the list but is not up yet.
        let drawing=actions.receipt("q-drawing","ask",&json!({"question":"Drawing?","options":["Yes","No"]}).to_string(),Some(agent::now_ms()+60_000),&p)?;
        tools.pending_questions.lock().unwrap().insert(drawing.clone(),crate::bridge::tools::OpenQuestion{permit:p.clone(),question:"Drawing?".into(),shown_ns:now,items:vec![agent::actions::AskItem::one("Drawing?",&["Yes","No"])],expires_ms:agent::now_ms()+agent::actions::QUESTION_LIFETIME_MS,picks:vec![]});
        assert_eq!(tools.card_state(now,now+1),crate::bridge::questions::CardState::None);
        assert!(tools.pending_questions.lock().unwrap().contains_key(&drawing),"a card mid-show is not dropped");
        actions.set_state(&drawing,"shown",&p)?;
        assert_eq!(tools.card_state(now,now+1),crate::bridge::questions::CardState::Answering);
        drop(runtime); Ok(())
    }
    #[test]
    fn joined_off_cancels_reload_notice_outbox_and_unrendered_questions() -> Result<()> {
        let (_d,data,_session,tools,runtime)=setup()?; let p=tools.agent_enabled.permit()?;
        let actions=tools.mcp_actions.actions.as_ref().unwrap();
        // The island renders Yes/No now (ui::agent_notice::ACTIONS_RENDERED); this case is a UI without them.
        tools.notice_actions_available.store(false,Ordering::Release);
        assert!(actions.ask("invisible",&asked("May I publish?",&["Yes".into(),"No".into()]),&p).unwrap_err().to_string().contains("UI_UNAVAILABLE"));
        actions.enqueue("old-outbox","old_epoch_must_not_reach_pi",&p)?;
        tools.agent_enabled.store(false,Ordering::Release); tools.agent_enabled.store(true,Ordering::Release);
        assert!(tools.reload("revoked reload",&p).is_err()); assert!(tools.notice("revoked","revoked_notice",&p).is_err());
        actions.drain(&tools.agent_enabled.permit()?)?; runtime.barrier()?;
        assert_eq!(delivery_count(&data,"old_epoch_must_not_reach_pi")?,0);
        assert_eq!(delivery_count(&data,"revoked_notice")?,0); drop(runtime); Ok(())
    }
    #[test]
    fn pending_reload_retains_all_receipts_and_discards_a_revoked_epoch() -> Result<()> {
        let d=tempfile::tempdir()?; let gate=agent::delivery::DeliveryGate::new(true);
        let first=format!("reload-{}",uuid::Uuid::new_v4());let second=format!("reload-{}",uuid::Uuid::new_v4());
        let pending=PendingReload::accepted(format!("{first}: first; also requested {second}: second"),gate.generation());
        assert_eq!(pending.receipts,vec![first,second]);
        agent::runtime::atomic_write(&d.path().join("agent/pending-reload.json"),&serde_json::to_vec(&pending)?)?;
        assert!(pending_reload(d.path(),&gate)?.is_some());gate.store(false,Ordering::Release);gate.store(true,Ordering::Release);
        assert!(pending_reload(d.path(),&gate)?.is_none());assert!(!d.path().join("agent/pending-reload.json").exists());Ok(())
    }
    #[test]
    fn joined_connect_notice_is_durable_deduplicated_and_does_not_authorize() -> Result<()> {
        let (_d,data,session,tools,runtime)=setup()?;
        let p=tools.agent_enabled.permit()?; let actions=tools.mcp_actions.actions.as_ref().unwrap();
        actions.enqueue("connection-notice","Connect T3 · click",&p)?; actions.drain(&p)?; actions.drain(&p)?;
        // E7.6: T3 connection state never wakes the agent; it is context.
        wait_for(&session,|r|r.iter().any(|e|e.kind=="agent_context"&&e.payload.to_string().contains("Connect T3")))?;
        assert_eq!(delivery_count(&data,"Connect T3")?,1);
        assert!(!data.join("agent/fake-t3-runtime.json").exists());
        drop(runtime); Ok(())
    }
    #[test]
    fn joined_watcher_starts_and_joins_without_a_tools_ownership_cycle() -> Result<()> {
        let (_d,_data,_session,tools,runtime)=setup()?;
        assert!(tools.watcher_running.load(Ordering::Acquire));
        assert!(tools.mcp_actions.workers.lock().unwrap().is_some());
        drop(runtime);
        assert!(!tools.watcher_running.load(Ordering::Acquire));
        assert!(tools.mcp_actions.workers.lock().unwrap().is_none());
        assert!(tools.agent_commands.lock().unwrap().is_none()); assert_eq!(Arc::strong_count(&tools),1); Ok(())
    }
    #[test]
    fn reload_waits_for_settlement_and_notice_uses_original_permit()->Result<()> {
        let d=tempfile::tempdir()?;let data=d.path().join("data");let session=data.join("archive/session-test");std::fs::create_dir_all(&data)?;
        test_harness(Some(Path::new(env!("CARGO_MANIFEST_DIR")).join("../agent/tests/fixtures/mock-adapter.py")));
        let tools=Arc::new(Tools::new(d.path(),&data));let runtime=AgentRuntime::start(Settings{dry_run:false,..Default::default()},data.clone(),session.clone(),tools.clone(),None)?;test_harness(None);runtime.barrier()?;
        runtime.send(Command::Send{text:"[fixture:slow]".into(),steer:false})?;
        wait_for(&session,|r|r.iter().any(|e|e.kind=="agent_transport_sent"))?;
        tools.reload("MCP changed",&tools.agent_enabled.permit()?)?;
        wait_for(&session,|r|r.iter().any(|e|e.kind=="agent_reload"))?;
        let rows=heimdall_archive::Journal::open_read_only(&session)?.records()?;let settled=rows.iter().position(|r|r.kind=="agent_transport_reply"&&r.payload["wire"]["type"]=="agent_settled").unwrap();let reloaded=rows.iter().position(|r|r.kind=="agent_reload").unwrap();assert!(settled<reloaded);
        let p=tools.agent_enabled.permit()?;runtime.notice("receipt-1","worker finished",&p)?;runtime.notice("receipt-1","worker finished",&p)?;runtime.barrier()?;
        // After the reload's start and before the user speaks, the worker event is context, once (E4.3, E6.3).
        wait_for(&session,|r|r.iter().any(|e|e.kind=="agent_context"&&e.payload["text"].as_str().is_some_and(|s|s.contains("worker finished"))))?;
        let c=rusqlite::Connection::open(data.join("pi-spool.sqlite"))?;let count:i64=c.query_row("SELECT count(*) FROM deliveries WHERE id=?",[format!("notice-{}",heimdall_archive::hash(b"receipt-1"))],|r|r.get(0))?;assert_eq!(count,1);
        tools.agent_enabled.store(false,Ordering::Release);tools.agent_enabled.store(true,Ordering::Release);assert!(tools.notice("stale","must not deliver",&p).is_err());drop(runtime);Ok(())
    }

    #[test]
    fn ui_clicks_capture_permits_before_the_pipeline_queue()->Result<()> {
        let d=tempfile::tempdir()?;let tools=Arc::new(Tools::new(d.path(),d.path()));let (commands,mut rx)=mpsc::unbounded_channel();*tools.agent_commands.lock().unwrap()=Some(commands);
        let (pipeline,queued)=crossbeam_channel::unbounded();let callback=crate::ui_action_callback(tools.clone(),pipeline,ui::Ui::handle());
        callback(ui::UiAction::AgentWindow(ui::AgentWindowAction::Send{text:"click queued".into(),steer:false}));
        callback(ui::UiAction::AgentNotice(ui::agent_notice::AgentNoticeAction::ConnectT3));
        assert!(queued.is_empty());tools.agent_enabled.store(false,Ordering::Release);tools.agent_enabled.store(true,Ordering::Release);
        for _ in 0..2 {match rx.try_recv()?{Command::Scoped{permit,..}|Command::NoticeAction{permit,..}=>assert!(permit.check().is_err()),_=>panic!("missing click permit")}}
        Ok(())
    }
    #[test]
    fn every_window_action_captures_admission_epoch_and_old_actor_commands_are_revoked()->Result<()> {
        let gate=Arc::new(agent::delivery::DeliveryGate::new(true));let (tx,mut rx)=mpsc::unbounded_channel();let actor=AgentRuntime{gate:gate.clone(),tx,thread:None};
        for action in [ui::AgentWindowAction::Send{text:"queued".into(),steer:false},ui::AgentWindowAction::Stop,ui::AgentWindowAction::EditAndResend{session_entry_id:"old".into(),text:"edited".into()},ui::AgentWindowAction::Reload{reason:"reload".into(),abort:false},ui::AgentWindowAction::ResolveDelivery{id:"old".into(),completed:false}] {actor.window_action(action)?;}
        gate.store(false,Ordering::Release);gate.store(true,Ordering::Release);
        for _ in 0..5 {let Command::Scoped{permit,..}=rx.try_recv()?else{panic!("missing action permit")};assert!(permit.check().is_err());}Ok(())
    }
    #[test]
    fn typed_message_waiting_for_engine_retains_its_epoch_across_spool_reopen()->Result<()> {
        let d=tempfile::tempdir()?;let gate=Arc::new(agent::delivery::DeliveryGate::new(true));let spool=d.path().join("db");
        let adapter=||->Result<RecordedAdapter>{Ok(RecordedAdapter{inner:Adapter::Unavailable(Unavailable),journal:heimdall_archive::Journal::open(&d.path().join("session"))?,gate:Some(gate.clone()),ids:Default::default(),queries:HashMap::new()})};
        let mut b=agent::Bridge::open(adapter()?,&spool)?;b.status=agent::Status::WaitingEngine;let rt=tokio::runtime::Runtime::new()?;
        rt.block_on(b.send_text(Some("typed"),"queued typed request",false,Some(gate.permit()?)))?;assert_eq!(b.queued_count()?,1);drop(b);
        gate.store(false,Ordering::Release);gate.store(true,Ordering::Release);let mut b=agent::Bridge::open(adapter()?,&spool)?;rt.block_on(b.flush_queued())?;assert_eq!(b.queued_count()?,0);
        let db=rusqlite::Connection::open(spool)?;assert_eq!(db.query_row("SELECT state FROM deliveries WHERE id='typed'",[],|r|r.get::<_,String>(0))?,"cancelled");Ok(())
    }
    #[tokio::test]
    async fn early_typed_send_waits_without_failure_rows_and_delivers_once() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // Exercise both a reported Off and the stale Ready signal before the
        // asynchronous manager update. Explicit steering must queue as well.
        for (known, steer) in [(2, false), (1, true)] {
            let d = tempfile::tempdir()?;
            let off = d.path().join("engine-off");
            std::fs::write(&off, b"off")?;
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
            let engine = agent::supervision::EngineProbe::new(&format!("http://{}", listener.local_addr()?))?;
            let off_probe = off.clone();
            let first_probe_delivery_count = Arc::new(Mutex::new(None));
            let probe_count = first_probe_delivery_count.clone();
            let spool = d.path().join("spool");
            let server = tokio::spawn(async move {
                loop {
                    let (mut socket, _) = listener.accept().await.unwrap();
                    let mut request = [0; 1024];
                    socket.read(&mut request).await.unwrap();
                    {
                        let mut first = probe_count.lock().unwrap();
                        if first.is_none() {
                            let db = rusqlite::Connection::open(&spool).unwrap();
                            *first = Some(db.query_row("SELECT count(*) FROM deliveries", [], |r| r.get::<_, i64>(0)).unwrap());
                        }
                    }
                    let status = if off_probe.exists() { "503 Service Unavailable" } else { "200 OK" };
                    socket.write_all(format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
                }
            });
            let fixture = d.path().join("adapter.py");
            std::fs::write(&fixture, include_str!("../../../../agent/tests/fixtures/mock-adapter.py").replace(
                "scenario=v.get('message','').split('\\n')[-1]",
                "scenario=v.get('message','').split('\\n')[-1]\n        if (session.parent/'engine-off').exists(): scenario='[fixture:503]'",
            ))?;
            std::fs::set_permissions(&fixture, std::fs::Permissions::from_mode(0o700))?;
            let gate = Arc::new(agent::delivery::DeliveryGate::new(true));
            let signal = Arc::new(AtomicU8::new(known));
            let mut config = PiConfig::fixture(fixture, d.path().join("session.jsonl"), d.path().to_owned());
            config.engine = Some(engine.clone());
            config.engine_signal = Some(signal.clone());
            let mut pi = Pi::launch_config_permitted(config, Some(gate.permit()?))?;
            let wires = Arc::new(Mutex::new(Vec::new()));
            let recorded = wires.clone();
            pi.set_transport_recorder(Arc::new(move |direction, wire| {
                recorded.lock().unwrap().push((direction.to_owned(), wire.clone()));
                Ok(())
            }));
            let mut b = agent::Bridge::open(RecordedAdapter {
                inner: Adapter::Pi(pi),
                journal: heimdall_archive::Journal::open(&d.path().join("archive/session-engine-wait"))?,
                gate: Some(gate.clone()), ids: Default::default(), queries: HashMap::new(),
            }, d.path().join("spool"))?;
            assert_eq!(b.status, agent::Status::Idle, "readiness tick has not observed Off yet");
            let sent = send_typed_message(&mut b, "early typed request", steer, Some(gate.permit()?), None).await?;
            if sent.is_some() {
                // On the old path Pi emits a failed message_end before the
                // Deferred event returns this same delivery to the spool.
                loop {
                    if matches!(tokio::time::timeout(std::time::Duration::from_secs(3), b.next()).await??, Some(Event::Deferred { .. })) { break; }
                }
            }
            let mut live = agent::view::LiveView::default();
            let entries: Vec<_> = wires.lock().unwrap().iter().filter(|(direction, _)| direction == "reply")
                .flat_map(|(_, wire)| live.received(wire, agent::now_ms())).collect();
            assert!(entries.iter().all(|e| !e.error), "engine Off produced transient failed-turn rows: {entries:?}");
            assert!(sent.is_none());
            if known == 2 {
                assert_eq!(*first_probe_delivery_count.lock().unwrap(), Some(1), "the message must be durable before awaiting readiness");
            }
            let state = agent_window_state(&b, &Settings { dry_run: false, ..Default::default() }, 0, None);
            assert_eq!(state.state, ui::AgentRunState::WaitingEngine);
            assert_eq!(state.queued, 1);
            assert!(state.can_send);
            let queued = b.last_queued()?.unwrap();
            let id = queued["uuid"].as_str().unwrap().to_owned();
            assert_eq!(queued["delivery_epoch"], gate.generation());
            assert!(queued["message"]["content"][0]["text"].as_str().unwrap().ends_with("early typed request"));
            assert!(b.inflight()?.is_empty());
            assert!(!engine_available(&signal, &engine).await, "a stale Ready signal must not release the queue on the next tick");
            b.flush_queued().await?;
            assert!(!wires.lock().unwrap().iter().any(|(direction, wire)| direction == "sent" && wire["type"] == "prompt"));

            std::fs::remove_file(&off)?;
            signal.store(2, Ordering::Release);
            assert!(engine_available(&signal, &engine).await);
            b.engine_ready();
            b.flush_queued().await?;
            loop {
                if let Some(Event::Complete { id: done, error, .. }) = tokio::time::timeout(std::time::Duration::from_secs(3), b.next()).await?? {
                    assert_eq!(done.as_deref(), Some(id.as_str()));
                    assert!(!error);
                    break;
                }
            }
            b.flush_queued().await?;
            assert_eq!(b.status, agent::Status::Idle);
            assert_eq!(b.queued_count()?, 0);
            assert!(b.inflight()?.is_empty());
            let db = rusqlite::Connection::open(d.path().join("spool"))?;
            assert_eq!(db.query_row("SELECT count(*) FROM deliveries", [], |r| r.get::<_, i64>(0))?, 1);
            assert_eq!(db.query_row("SELECT state FROM deliveries WHERE id=?1", [&id], |r| r.get::<_, String>(0))?, "completed");
            assert_eq!(wires.lock().unwrap().iter().filter(|(direction, wire)| direction == "sent" && wire["type"] == "prompt" && wire["id"] == id).count(), 1);
            b.shutdown().await?;
            server.abort();
        }
        Ok(())
    }
    #[test]
    fn invalid_window_actions_leave_pi_and_restart_budget_unchanged()->Result<()> {
        let (_d,data,_session,_tools,runtime)=setup()?;let before=runtime.inspect()?.unwrap().0;
        for action in [ui::AgentWindowAction::Send{text:"".into(),steer:false},ui::AgentWindowAction::EditAndResend{session_entry_id:"absent".into(),text:"edit".into()},ui::AgentWindowAction::ResolveDelivery{id:"stale".into(),completed:true}] {runtime.window_action(action)?;runtime.barrier()?;assert_eq!(runtime.inspect()?.unwrap().0,before);}
        assert!(!data.join("agent/restarts.json").exists());Ok(())
    }
    #[test]
    fn acknowledged_reload_is_executable_before_actor_receives_it_and_recovers_all_receipts()->Result<()> {
        let d=tempfile::tempdir()?;let data=d.path().join("data");let tools=Arc::new(Tools::new(d.path(),&data));let (tx,mut rx)=mpsc::unbounded_channel();*tools.agent_commands.lock().unwrap()=Some(tx);
        let weak=Arc::downgrade(&tools);
        tools.mcp_actions.attach(ui::Ui::handle(),Arc::new(|_,_,_|Ok(())),Arc::new(move|id,p|weak.upgrade().unwrap().reload(id,p)),Arc::new(|_,_|Ok(())))?;
        let actions=tools.mcp_actions.actions.as_ref().unwrap();let p=tools.agent_enabled.permit()?;
        let first=actions.reload_reason("reload-one",Some("change settings"),&p)?;let second=actions.reload_reason("reload-two",Some("change extension"),&p)?;
        assert_eq!(actions.state(&first)?,"scheduled");assert_eq!(actions.state(&second)?,"scheduled");
        let durable=pending_reload(&data,&tools.agent_enabled)?.unwrap();assert!(durable.wait_for_settlement);assert_eq!(durable.receipts,vec![first.clone(),second.clone()]);
        // Receiving the first queued request cannot overwrite the later acknowledgement.
        let Command::Reload{reason,permit:Some(p),..}=rx.try_recv()?else{panic!("reload not queued")};tools.persist_reload(reason,&p)?;
        assert_eq!(pending_reload(&data,&tools.agent_enabled)?.unwrap().receipts,vec![first.clone(),second.clone()]);
        drop(rx);tools.mcp_actions.detach();*tools.agent_commands.lock().unwrap()=None;
        test_harness(Some(Path::new(env!("CARGO_MANIFEST_DIR")).join("../agent/tests/fixtures/mock-adapter.py")));
        let session=data.join("archive/session-recover");let runtime=AgentRuntime::start(Settings{dry_run:false,..Default::default()},data.clone(),session,tools.clone(),None)?;test_harness(None);runtime.barrier()?;
        assert_eq!(actions.state(&first)?,"completed");assert_eq!(actions.state(&second)?,"completed");assert!(!data.join("agent/pending-reload.json").exists());drop(runtime);Ok(())
    }

    #[test]
    fn settlement_rotates_before_queued_wake_and_does_not_fetch_oversized_history()->Result<()> { settlement_rotates(false) }
    /// Night review 21 Oct (item 4): a repair's edit of a summarised entry in the kept range made rotation refuse, and
    /// storage maintenance reloaded and failed again at every settlement. The same storage maintenance now rotates.
    #[test]
    fn settlement_rotates_past_an_edit_of_a_summarised_entry()->Result<()> { settlement_rotates(true) }
    fn settlement_rotates(edit_of_summarised:bool)->Result<()> {
        let (_d,data,session,_tools,runtime)=setup()?;
        runtime.send(Command::Send{text:"[fixture:hold]".into(),steer:false})?;
        wait_for(&session,|rows|rows.iter().any(|r|r.payload["wire"]["type"]=="message_end"&&r.payload["wire"]["message"]["role"]=="user"))?;
        let path=data.join("pi-session.jsonl");let mut file=std::fs::File::create(&path)?;
        let mut rows=vec![json!({"type":"session","version":3,"id":"same-session"}),json!({"type":"message","id":"old","parentId":null,"message":{"role":"user","content":"x".repeat(agent::session::ROTATE_BYTES as usize)}}),json!({"type":"message","id":"recent","parentId":"old","message":{"role":"user","content":"keep"}}),json!({"type":"compaction","id":"checkpoint","parentId":"recent","firstKeptEntryId":"recent","systemMessage":{"role":"system","content":"stable"},"summary":"continuity"})];
        if edit_of_summarised {
            rows.insert(3,json!({"type":"context_edit","id":"edit-old","parentId":"recent","targetId":"old","replacement":{"content":"(Image removed by Heimdall: larger than the model accepts.)"}}));
            rows[4]["parentId"]=json!("edit-old");
        }
        for row in rows {writeln!(file,"{row}")?;}
        file.sync_all()?;drop(file);
        std::fs::write(data.join("hold-compaction"),b"hold")?;
        std::fs::write(data.join("release-turn"),b"go")?;
        wait_for(&session,|rows|rows.iter().any(|r|r.kind=="agent_transport_send"&&r.payload["wire"]["type"]=="compact"))?;
        runtime.send(Command::Send{text:"queued after maintenance".into(),steer:false})?;runtime.barrier()?;
        let db=rusqlite::Connection::open(data.join("pi-spool.sqlite"))?;assert_eq!(db.query_row("SELECT count(*) FROM deliveries WHERE state='queued'",[],|r|r.get::<_,i64>(0))?,1);
        assert!(heimdall_archive::Journal::open_read_only(&session)?.records()?.iter().any(|r|r.kind=="agent_queue_fallback" && r.payload["reason"]=="Agent will reload after this turn"));
        std::fs::remove_file(data.join("hold-compaction"))?;
        wait_for(&session,|rows|rows.iter().any(|r|r.kind=="agent_transport_sent"&&r.payload.to_string().contains("queued after maintenance")))?;
        let rows=heimdall_archive::Journal::open_read_only(&session)?.records()?;
        let compact=rows.iter().position(|r|r.kind=="agent_transport_send"&&r.payload["wire"]["type"]=="compact").unwrap();
        let reload=rows.iter().position(|r|r.kind=="agent_reload").unwrap();
        let send=rows.iter().position(|r|r.kind=="agent_transport_sent"&&r.payload.to_string().contains("queued after maintenance")).unwrap();assert!(compact<reload && reload<send);
        assert!(!rows[..reload].iter().any(|r|r.kind=="agent_transport_send"&&r.payload["wire"]["type"]=="get_entries"));
        assert!(path.metadata()?.len()<agent::session::ROTATE_BYTES);drop(runtime);Ok(())
    }
    #[test]
    fn bounded_history_skips_large_older_payloads_and_keeps_latest_entries()->Result<()> {
        let d=tempfile::tempdir()?;let path=d.path().join("session");let mut file=std::fs::File::create(&path)?;
        writeln!(file,"{}",json!({"id":"huge","message":{"role":"user","content":"x".repeat(3*1024*1024)}}))?;
        for i in 0..600 {writeln!(file,"{}",json!({"id":format!("e-{i}"),"message":{"role":"user","content":"recent"}}))?;}
        let rows=bounded_session_history(&path)?;assert_eq!(rows.len(),500);assert_eq!(rows[0]["id"],"e-100");assert_eq!(rows[499]["id"],"e-599");Ok(())
    }
    fn late_png()->agent::protocol::Attachment{agent::protocol::Attachment{media_type:"image/png".into(),bytes:vec![137,80,78,71,1]}}
    /// A screenshot ready after its words went out joins the running turn, never a wake of its own: once the turn
    /// has acted it steers it; while the model still thinks it restarts the turn with the image (night pass H2), so
    /// the one answer sees words and image together.
    #[test]
    fn late_screenshot_steers_a_running_turn_and_wakes_nothing()->Result<()> {
        for (phase,restarts) in [("[fixture:steering-tool]",false),("[fixture:steering-thinking]",true)] {
            let (_d,_data,session,_tools,runtime)=setup()?;
            runtime.send(Command::Send{text:phase.into(),steer:false})?;
            // The harness has taken in the turn's progress (its reply record), so its state is current.
            wait_for(&session,|rows|rows.iter().any(|r|r.kind=="agent_reply"&&r.payload["event"]["type"]==if restarts {"message_update"} else {"tool_execution_start"}))?;
            runtime.send(Command::LateImages{caption:"Screenshot for: look at this".into(),images:vec![late_png()]})?;
            runtime.barrier()?;
            let rows=heimdall_archive::Journal::open_read_only(&session)?.records()?;
            let steers:Vec<_>=rows.iter().filter(|r|r.kind=="agent_steer").collect();
            assert_eq!(steers.len(),1,"{phase}: one input into the running turn");
            assert_eq!(steers[0].payload["image_ids"].as_array().unwrap().len(),1);
            assert_eq!(steers[0].payload["delivered"],true,"{phase}");
            assert!(rows.iter().all(|r|r.kind!="agent_wake"),"{phase}: not a wake");
            assert_eq!(rows.iter().filter(|r|r.kind=="agent_turn_rejoined").count(),usize::from(restarts),"{phase}");
            drop(runtime);
        }
        Ok(())
    }
    #[test]
    fn late_screenshot_without_a_running_turn_rides_the_next_wake()->Result<()> {
        let (_d,_data,session,_tools,runtime)=setup()?;
        runtime.send(Command::LateImages{caption:"Screenshot for: look at this".into(),images:vec![late_png()]})?;
        runtime.barrier()?;
        let rows=heimdall_archive::Journal::open_read_only(&session)?.records()?;
        assert!(rows.iter().all(|r|r.kind!="agent_wake"&&r.kind!="agent_steer"),"held while no turn runs");
        runtime.send(Command::Wake{images:vec![],urgent:false})?;
        runtime.barrier()?;
        let rows=heimdall_archive::Journal::open_read_only(&session)?.records()?;
        let wakes:Vec<_>=rows.iter().filter(|r|r.kind=="agent_wake").collect();
        assert_eq!(wakes.len(),1);
        assert_eq!(wakes[0].payload["image_ids"].as_array().unwrap().len(),1,"the next wake carries the held screenshot");
        Ok(())
    }
    #[test]
    fn held_late_screenshots_expire_with_the_attachment_window(){
        use super::supervisor::{take_fresh_late,LateImage};
        let held=|at:u64|vec![LateImage{captured_ns:at,caption:"c".into(),images:vec![]}];
        let ttl=crate::pipeline::ATTACHMENT_TTL_NS;
        assert_eq!(take_fresh_late(&mut held(1_000),1_000+ttl).len(),1,"two minutes exactly still waits");
        assert!(take_fresh_late(&mut held(1_000),1_000+ttl+1).is_empty(),"after two minutes it expires");
    }
    #[test]
    fn window_keeps_500_entries_around_a_huge_line()->Result<()> {
        // 5 KB rows over several 1 MB reads, with an 8 MB inline image between them.
        let d=tempfile::tempdir()?;let path=d.path().join("session");let mut file=std::fs::File::create(&path)?;
        let filler="x".repeat(5000);let mut prev=Value::Null;
        for i in 0..600 {
            if i==400 {writeln!(file,"{}",json!({"type":"message","id":"huge","parentId":prev,"message":{"role":"user","content":[{"type":"text","text":"y".repeat(8*1024*1024)}]}}))?;prev=json!("huge");}
            writeln!(file,"{}",json!({"type":"message","id":format!("e-{i}"),"parentId":prev,"message":{"role":"user","content":[{"type":"text","text":filler}]}}))?;prev=json!(format!("e-{i}"));
        }
        let rows=bounded_session_history(&path)?;
        assert_eq!(rows.len(),500);assert_eq!(rows[0]["id"],"e-101");assert_eq!(rows[499]["id"],"e-599");
        assert!(rows.iter().any(|r|r["id"]=="huge"),"the huge entry is inside the window");
        let ui::AgentWindowUpdate::History(entries)=settled_history(&path)? else{panic!("settlement is a history update")};
        assert_eq!(entries.len(),500,"every row of the window shows");
        Ok(())
    }
    #[test]
    fn window_keeps_500_entries_when_the_newest_entry_is_huge()->Result<()> {
        let d=tempfile::tempdir()?;let path=d.path().join("session");let mut file=std::fs::File::create(&path)?;
        let mut prev=Value::Null;
        for i in 0..600 {writeln!(file,"{}",json!({"type":"message","id":format!("e-{i}"),"parentId":prev,"message":{"role":"user","content":[{"type":"text","text":"recent"}]}}))?;prev=json!(format!("e-{i}"));}
        writeln!(file,"{}",json!({"type":"message","id":"huge","parentId":prev,"message":{"role":"user","content":[{"type":"text","text":"y".repeat(8*1024*1024)}]}}))?;
        let rows=bounded_session_history(&path)?;
        assert_eq!(rows.len(),500);assert_eq!(rows[499]["id"],"huge");assert_eq!(rows[0]["id"],"e-101");Ok(())
    }
    #[test]
    fn settlement_replaces_stream_rows_with_editable_active_branch_without_duplicates()->Result<()> {
        let d=tempfile::tempdir()?;let path=d.path().join("session.jsonl");
        let prompt="[Heimdall typed message]\nSOAK-PING";
        let user=json!({"type":"message","id":"user","parentId":null,"timestamp":"2026-10-14T19:02:00.000Z","message":{"role":"user","content":[{"type":"text","text":prompt}]}});
        let reply=json!({"type":"message","id":"reply","parentId":"user","timestamp":"2026-10-14T19:02:01.000Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"Check"},{"type":"text","text":"PONG"}],"stopReason":"stop"}});
        let abandoned=json!({"type":"message","id":"abandoned","parentId":"user","message":{"role":"assistant","content":[{"type":"text","text":"Abandoned reply"}]}});
        let receipt=json!({"type":"custom","id":"receipt","parentId":"reply","customType":"heimdall_receipt","data":{"completed":true}});
        let mut file=std::fs::File::create(&path)?;
        for row in [&user,&reply,&abandoned,&receipt]{writeln!(file,"{row}")?;}
        let mut stream=agent::view::LiveView::default();let mut view=ui::agent_view::AgentView::default();
        view.entries(stream.sent(&json!({"type":"prompt","id":"delivery","message":prompt}),agent::now_ms()).into_iter().map(live_entry).collect());
        stream.received(&json!({"type":"message_start","message":{"role":"assistant"}}),agent::now_ms());
        view.entries(stream.received(&json!({"type":"message_end","message":reply["message"]}),agent::now_ms()).into_iter().map(live_entry).collect());
        view.notice(&ui::agent_notice::AgentNotice::line("question","Keep this notice",ui::agent_notice::NoticeKind::Info),agent::now_ms());
        let ui::AgentWindowUpdate::History(entries)=settled_history(&path)? else{panic!("settlement must replace streamed history")};
        view.history(entries);
        let live:Vec<_>=view.rows.iter().filter_map(|r|r.live()).collect();
        assert_eq!(live.len(),3,"prompt, thinking and reply each appear once");
        assert_eq!(live.iter().filter(|e|e.text=="PONG").count(),1);
        assert!(live.iter().all(|e|e.text!="Abandoned reply"));
        let typed=live.iter().find(|e|e.kind==ui::AgentLiveKind::Prompt).unwrap();
        assert!(typed.editable);assert_eq!(typed.session_entry_id.as_deref(),Some("user"));
        assert_eq!(view.rows.iter().filter(|r|matches!(r,ui::agent_view::Row::Notice{..})).count(),1);
        Ok(())
    }
    fn heard(id:&str,minutes_ago:u64,speaker:&str,text:&str)->Observation{
        Observation{id:id.into(),timestamp_ms:agent::now_ms()-minutes_ago*60_000,source:"mic".into(),speaker:speaker.into(),language:"en".into(),text:text.into()}
    }
    /// Like `wait_for`, for evidence behind supervisor restarts (their backoff is 1, 2, 4 seconds).
    fn wait_long(session:&Path,test:impl Fn(&[heimdall_archive::Record])->bool)->Result<()> {
        let began=std::time::Instant::now();loop{let rows=heimdall_archive::Journal::open_read_only(session)?.records()?;if test(&rows){return Ok(());}anyhow::ensure!(began.elapsed()<std::time::Duration::from_secs(20),"supervisor evidence did not arrive");std::thread::sleep(std::time::Duration::from_millis(50));}
    }
    fn session_rows(path:&Path)->Result<Vec<Value>>{std::fs::read_to_string(path)?.lines().map(|l|Ok(serde_json::from_str(l)?)).collect()}
    fn wait_until(what:&str,test:impl Fn()->Result<bool>)->Result<()>{
        let began=std::time::Instant::now();while !test()?{anyhow::ensure!(began.elapsed()<std::time::Duration::from_secs(15),"{what} did not happen");std::thread::sleep(std::time::Duration::from_millis(50));}Ok(())
    }
    /// An utterance as the pipeline archives it (the timeline the decider and timeline_search read).
    fn archived(session:&Path,id:&str,minutes_ago:u64,source_id:&str,source:&str,speaker:&str,text:&str)->Result<()>{
        archived_as(session,id,minutes_ago,source_id,source,speaker,text,r#"{"given_to_agent":true}"#)
    }
    #[allow(clippy::too_many_arguments)]
    fn archived_as(session:&Path,id:&str,minutes_ago:u64,source_id:&str,source:&str,speaker:&str,text:&str,decision:&str)->Result<()>{
        let db=heimdall_archive::store::open(&crate::archive::store_root(session))?;
        db.execute("INSERT INTO utterances(session,id,wall_ms,source_id,source,speaker,start_ns,end_ns,text,words,notes,decision) VALUES(1,?1,?2,?3,?4,?5,0,0,?6,'[]','[]',?7)",
            rusqlite::params![id,(agent::now_ms()-minutes_ago*60_000) as i64,source_id,source,speaker,text,decision])?;
        Ok(())
    }
    fn catchups(path:&Path)->Result<Vec<String>>{
        Ok(session_rows(path)?.iter().filter(|r|r["customType"]=="heimdall_context").map(|r|r["content"].as_str().unwrap_or_default().to_owned()).collect())
    }
    fn mock_runtime(dir:&Path,data:&Path,session:&Path,dry_run:bool)->Result<AgentRuntime>{
        test_harness(Some(Path::new(env!("CARGO_MANIFEST_DIR")).join("../agent/tests/fixtures/mock-adapter.py")));
        let result=AgentRuntime::start(Settings{dry_run,..Default::default()},data.to_owned(),session.to_owned(),Arc::new(Tools::new(dir,data)),None);
        test_harness(None);let runtime=result?;runtime.barrier()?;Ok(runtime)
    }
    /// "after like five hours of talking I will enable the agent? … I would want to give the agent the
    /// last thirty minutes of context … on top of what he already had. If it's too much, then compact and then add".
    /// Turning the agent on hands over the archived speech of the last 30 minutes that the conversation never
    /// received, every speaker, as one context-only message in the same conversation: no turn, no reply. Speech heard
    /// while on, or already handed over, is not repeated. A full context compacts first.
    #[test]
    fn agent_on_adds_the_last_30_minutes_heard_while_off_as_context_only() -> Result<()> {
        let d=tempfile::tempdir()?;let data=d.path().join("data");let session=data.join("archive/session-catchup");std::fs::create_dir_all(&data)?;
        let runtime=mock_runtime(d.path(),&data,&session,true)?;
        // A first live period: this observation reaches the live spool (the agent got it).
        runtime.send(Command::Restart(Settings{dry_run:false,..Default::default()}))?;runtime.barrier()?;
        runtime.send(Command::Observe(heard("heard",4,"Call","Dave: already heard part")))?;runtime.barrier()?;
        runtime.send(Command::Restart(Settings{dry_run:true,..Default::default()}))?;runtime.barrier()?;
        archived(&session,"old",45,"mic:x","Shure MV7i","User","forty five minutes ago")?;
        archived(&session,"dave",20,"app:discord","Discord","Dave","did you push the audio fix")?;
        archived(&session,"owner",2,"mic:x","Shure MV7i","User","not yet, after lunch")?;
        // Routed away from the agent: an undiarized call stream.
        archived_as(&session,"call",3,"app:discord","Discord","Call","mixed call audio",r#"{"status":"awaiting_diarization","given_to_agent":false}"#)?;
        // Speech splits: of the observation the agent got (not repeated), and of off-period speech (handed over).
        archived(&session,"heard:speech:1",4,"app:discord","Discord","Dave","already heard part")?;
        archived(&session,"dave:speech:1",20,"app:discord","Discord","Erin","and the tests too")?;
        runtime.send(Command::Restart(Settings{dry_run:false,..Default::default()}))?;runtime.barrier()?;
        wait_for(&session,|r|r.iter().any(|e|e.kind=="agent_catchup"&&e.payload["state"]=="added"))?;
        let (_,path)=runtime.inspect()?.context("Pi is not running")?;
        // Every start appends its context; the first start (nothing unheard yet) only the restart message.
        let all=catchups(&path)?;
        assert_eq!(all.len(),2,"one context per start");
        assert!(all[0].starts_with("Heimdall, you have just been restarted"),"{}",all[0]);
        let first=&all[1..];
        assert!(first[0].contains(" Discord · Dave] did you push the audio fix")&&first[0].contains(" Mic · User] not yet, after lunch"),"{}",first[0]);
        assert!(!first[0].contains("forty five minutes ago"),"speech older than 30 minutes stays in the timeline");
        assert!(!first[0].contains("mixed call audio"),"speech routed away from the agent is never handed over");
        // The spool row the agent was observed with never reached the model (no wake): it is history now (E4.4),
        // once; its speech-split child is not repeated.
        assert_eq!(first[0].matches("already heard part").count(),1,"a split of spooled speech is not repeated: {}",first[0]);
        assert!(first[0].contains(" Discord · Erin] and the tests too"),"a split of unheard speech is part of it: {}",first[0]);
        assert!(first[0].contains("nothing here is a request")&&first[0].ends_with("Don't do anything unless User talks to you."));
        let records=heimdall_archive::Journal::open_read_only(&session)?.records()?;
        assert!(!records.iter().any(|r|r.kind=="agent_transport_reply"&&r.payload["wire"]["type"]=="mock_pi"),"the catch-up started a turn");
        // Heard while on: it reaches the live spool (and the next wake), so it is never a catch-up.
        runtime.send(Command::Observe(heard("live",0,"User","said while the agent was on")))?;runtime.barrier()?;
        archived(&session,"live",0,"mic:x","Shure MV7i","User","said while the agent was on")?;
        // Off again, more talk, and a conversation near the compaction threshold: compact, then add only the new speech.
        runtime.send(Command::Restart(Settings{dry_run:true,..Default::default()}))?;runtime.barrier()?;
        archived(&session,"later",1,"mic:x","Shure MV7i","User","the second off period")?;
        // Pi compacts Haiku past its in-turn threshold (its default budget's); the catch-up adds about 40.
        std::fs::write(data.join("context-tokens"),(agent::provider::ApiModel::haiku().limits().threshold()-10).to_string())?;
        runtime.send(Command::Restart(Settings{dry_run:false,..Default::default()}))?;runtime.barrier()?;
        wait_for(&session,|r|r.iter().filter(|e|e.kind=="agent_catchup"&&e.payload["state"]=="added").count()==3)?;
        let rows=heimdall_archive::Journal::open_read_only(&session)?.records()?;
        let states:Vec<_>=rows.iter().filter(|e|e.kind=="agent_catchup").map(|e|e.payload["state"].as_str().unwrap_or_default().to_owned()).collect();
        assert_eq!(states,["added","added","compacting_first","added"]);
        let compact_at=rows.iter().position(|r|r.kind=="agent_transport_reply"&&r.payload["wire"]["type"]=="mock_control"&&r.payload["wire"]["command"]=="compact").context("no compaction")?;
        let added_at=rows.iter().rposition(|r|r.kind=="agent_catchup"&&r.payload["state"]=="added").unwrap();
        assert!(compact_at<added_at,"compaction must finish before the catch-up is added");
        let (_,same)=runtime.inspect()?.context("Pi is not running")?;
        assert_eq!(same,path,"the same conversation continues");
        let all=catchups(&path)?;
        assert_eq!(all.len(),3);
        assert!(all[2].contains(" Mic · User] the second off period"),"{}",all[2]);
        // Speech the earlier catch-up delivered is not repeated; speech observed while on that never rode a wake is
        // history at this start, once (E4.4).
        assert!(!all[2].contains("audio fix"),"{}",all[2]);
        assert_eq!(all[2].matches("said while the agent was on").count(),1,"{}",all[2]);
        drop(runtime);Ok(())
    }
    /// Listening on, the app relaunched while the agent stays off, then the agent turned on: speech from before the
    /// relaunch is in the archive, so it is part of the catch-up.
    #[test]
    fn catchup_includes_speech_from_before_a_relaunch_while_off() -> Result<()> {
        let d=tempfile::tempdir()?;let data=d.path().join("data");let session=data.join("archive/session-before");std::fs::create_dir_all(&data)?;
        let before=mock_runtime(d.path(),&data,&session,true)?;
        archived(&session,"a",10,"mic:x","Shure MV7i","User","before the relaunch")?;
        drop(before);
        // The relaunch is a new archive session in the same archive.
        let session=data.join("archive/session-after");
        let after=mock_runtime(d.path(),&data,&session,true)?;
        archived(&session,"b",1,"app:discord","Discord","Dave","after the relaunch")?;
        after.send(Command::Restart(Settings{dry_run:false,..Default::default()}))?;after.barrier()?;
        wait_for(&session,|r|r.iter().any(|e|e.kind=="agent_catchup"&&e.payload["state"]=="added"))?;
        let (_,path)=after.inspect()?.context("Pi is not running")?;
        let all=catchups(&path)?;
        assert_eq!(all.len(),1);
        assert!(all[0].contains(" Mic · User] before the relaunch")&&all[0].contains(" Discord · Dave] after the relaunch"),"{}",all[0]);
        drop(after);Ok(())
    }
    /// A launch with the agent already on hands over only speech from before this launch that it never got;
    /// speech recorded since the launch belongs to the wakes.
    #[test]
    fn launch_with_the_agent_on_hands_over_only_unheard_speech_from_before_the_launch() -> Result<()> {
        let d=tempfile::tempdir()?;let data=d.path().join("data");std::fs::create_dir_all(&data)?;
        let earlier=data.join("archive/session-1-earlier");
        archived(&earlier,"before",5,"mic:x","Shure MV7i","User","said before the relaunch")?;
        let session=data.join(format!("archive/session-{}-now",agent::now_ms()));
        archived(&session,"after",0,"mic:x","Shure MV7i","User","said after the launch")?;
        std::thread::sleep(std::time::Duration::from_millis(5));
        let runtime=mock_runtime(d.path(),&data,&session,false)?;
        wait_for(&session,|r|r.iter().any(|e|e.kind=="agent_catchup"&&e.payload["state"]=="added"))?;
        let (_,path)=runtime.inspect()?.context("Pi is not running")?;
        let all=catchups(&path)?;
        assert_eq!(all.len(),1);
        assert!(all[0].contains("said before the relaunch")&&!all[0].contains("said after the launch"),"{}",all[0]);
        // As the pipeline does with speech recorded while the agent is on.
        runtime.send(Command::Observe(heard("after",0,"User","said after the launch")))?;runtime.barrier()?;
        drop(runtime);
        // The next launch: the observed line never rode a wake, so it is history now, once (E4.4); the speech handed
        // over before is not repeated.
        let next=data.join(format!("archive/session-{}-next",agent::now_ms()));
        let runtime=mock_runtime(d.path(),&data,&next,false)?;
        wait_for(&next,|r|r.iter().any(|e|e.kind=="agent_catchup"&&e.payload["state"]=="added"))?;
        let all=catchups(&path)?;
        assert_eq!(all.len(),2);
        assert!(!all[1].contains("said before the relaunch"),"handed over twice: {}",all[1]);
        assert_eq!(all[1].matches("said after the launch").count(),1,"{}",all[1]);
        drop(runtime);Ok(())
    }
    /// Night pass 20→21 Oct (the user: "after the compaction, we don't need to send it the last 30 minutes of
    /// transcript … what we need to send is any audio that happened after the compaction started"): the catch-up
    /// begins where the conversation's own record ends, at most 30 minutes back, and names that time.
    #[test]
    fn the_catch_up_begins_where_the_conversation_ends() -> Result<()> {
        let d=tempfile::tempdir()?;let data=d.path().join("data");std::fs::create_dir_all(&data)?;
        let ended=agent::now_ms()-6*60_000;
        let iso=agent::mcp_contract::utc(ended*1_000_000);
        std::fs::write(data.join("pi-session.jsonl"),format!("{}\n{}\n",
            json!({"type":"session","version":3,"id":"catchup-window","timestamp":iso,"cwd":"/"}),
            json!({"type":"message","id":"last","parentId":null,"timestamp":iso,"message":{"role":"user","content":"[Heimdall delivery: d]\nTranscript:\n[Mic · User] the last request","timestamp":1}})))?;
        let session=data.join(format!("archive/session-{}-now",agent::now_ms()));
        archived(&session,"before",20,"mic:x","Shure MV7i","User","said before the conversation's last message")?;
        archived(&session,"after",2,"app:discord","Discord","Dave","said after it")?;
        std::thread::sleep(std::time::Duration::from_millis(5));
        let runtime=mock_runtime(d.path(),&data,&session,false)?;
        wait_for(&session,|r|r.iter().any(|e|e.kind=="agent_catchup"&&e.payload["state"]=="added"))?;
        let (_,path)=runtime.inspect()?.context("Pi is not running")?;
        let all=catchups(&path)?;
        assert_eq!(all.len(),1);
        assert!(all[0].contains(" Discord · Dave] said after it")&&!all[0].contains("before the conversation's last message"),"{}",all[0]);
        assert!(all[0].starts_with(&format!("Heimdall catch-up: what was said since {} ",agent::clock::local_hms(ended))),"{}",all[0]);
        assert!(all[0].ends_with("Don't do anything unless User talks to you."));
        drop(runtime);Ok(())
    }
    /// A catch-up that cannot be added is tried again on the next verified start, then dropped.
    #[test]
    fn failed_catchup_is_retried_once_on_the_next_start() -> Result<()> {
        let d=tempfile::tempdir()?;let data=d.path().join("data");let session=data.join("archive/session-retry");std::fs::create_dir_all(&data)?;
        let runtime=mock_runtime(d.path(),&data,&session,true)?;
        archived(&session,"a",2,"mic:x","Shure MV7i","User","heard while off")?;
        std::fs::write(data.join("no-context-command"),"")?;
        runtime.send(Command::Restart(Settings{dry_run:false,..Default::default()}))?;runtime.barrier()?;
        wait_long(&session,|r|r.iter().any(|e|e.kind=="agent_catchup"&&e.payload["state"]=="retry_next_start"))?;
        std::fs::remove_file(data.join("no-context-command"))?;
        let (pid,path)=runtime.inspect()?.context("Pi is not running")?;
        unsafe{libc::kill(pid as i32,libc::SIGKILL);}
        wait_long(&session,|r|r.iter().any(|e|e.kind=="agent_catchup"&&e.payload["state"]=="added"))?;
        assert_eq!(catchups(&path)?.len(),1);
        // Failing on the second start as well drops it.
        runtime.send(Command::Restart(Settings{dry_run:true,..Default::default()}))?;runtime.barrier()?;
        archived(&session,"b",1,"mic:x","Shure MV7i","User","heard in the second off period")?;
        std::fs::write(data.join("no-context-command"),"")?;
        runtime.send(Command::Restart(Settings{dry_run:false,..Default::default()}))?;runtime.barrier()?;
        wait_long(&session,|r|r.iter().filter(|e|e.kind=="agent_catchup"&&e.payload["state"]=="retry_next_start").count()==2)?;
        let (pid,_)=runtime.inspect()?.context("Pi is not running")?;
        unsafe{libc::kill(pid as i32,libc::SIGKILL);}
        wait_long(&session,|r|r.iter().any(|e|e.kind=="agent_catchup"&&e.payload["state"]=="dropped"))?;
        assert_eq!(catchups(&path)?.len(),1);
        drop(runtime);Ok(())
    }
    /// A stale pending reload found with the agent off is cleared at once; its receipts complete when the agent can
    /// deliver. Nothing about it stops the start, even an unreadable file.
    #[test]
    fn stale_reload_settles_without_blocking_the_start() -> Result<()> {
        let d=tempfile::tempdir()?;let data=d.path().join("data");let session=data.join("archive/session-stale");std::fs::create_dir_all(data.join("agent"))?;
        let gate=Arc::new(agent::delivery::DeliveryGate::new(true));
        let actions=agent::actions::Actions::open(&data.join("agent/mcp-actions.sqlite"))?;
        let id=actions.receipt("op-1","reload","earlier reload",None,&gate.permit()?)?;
        std::fs::write(data.join("agent/pending-reload.json"),serde_json::to_vec(&json!({"reason":"earlier","generation":0,"receipts":[id]}))?)?;
        let off=Arc::new(agent::delivery::DeliveryGate::new(false));
        let waiting=settle_stale_reload(&data,&session,&off);
        assert_eq!(waiting,vec![id.clone()]);assert!(!data.join("agent/pending-reload.json").exists());
        assert_ne!(actions.state(&id)?,"completed");
        assert!(complete_receipts(&data,&gate,waiting).is_empty());
        assert_eq!(actions.state(&id)?,"completed");
        std::fs::write(data.join("agent/pending-reload.json"),b"{not json")?;
        assert!(settle_stale_reload(&data,&session,&gate).is_empty());
        assert!(!data.join("agent/pending-reload.json").exists());
        Ok(())
    }
    /// The Agent window always opens on the one Pi conversation, agent on or off, never on a launch's dry-run log.
    #[test]
    fn agent_window_shows_the_pi_conversation_while_off() -> Result<()> {
        let d=tempfile::tempdir()?;let data=d.path().join("data");std::fs::create_dir_all(data.join("agent"))?;
        assert!(window_history(&data).is_none(),"no conversation yet");
        let path=data.join("conversation.jsonl");
        std::fs::write(&path,[
            json!({"type":"session","version":3,"id":"one"}),
            json!({"type":"message","id":"u","parentId":null,"timestamp":"2026-10-17T18:45:00Z","message":{"role":"user","content":"[Heimdall typed message]\nwhat were we doing"}}),
            json!({"type":"message","id":"a","parentId":"u","timestamp":"2026-10-17T18:45:05Z","message":{"role":"assistant","content":[{"type":"text","text":"The voice bank."}],"stopReason":"stop"}}),
        ].iter().map(|v|format!("{v}\n")).collect::<String>())?;
        agent::runtime::atomic_write(&data.join("agent/active-session.json"),&serde_json::to_vec(&path)?)?;
        // A launch's dry-run log exists beside it; it is not what the window shows.
        let run=crate::archive::run_dir(&data,&data.join("archive/session-x"));std::fs::create_dir_all(&run)?;
        std::fs::write(run.join("dry-run.jsonl"),json!({"dry_run":true,"would_send":{"type":"user","message":{"content":[{"type":"text","text":"Observation:\n[Mic · User] hello"}]}}}).to_string())?;
        let Some(ui::AgentWindowUpdate::History(entries))=window_history(&data) else{panic!("the conversation must show")};
        let texts:Vec<_>=entries.iter().map(|e|e.text.clone()).collect();
        assert!(texts.iter().any(|t|t.contains("what were we doing"))&&texts.iter().any(|t|t.contains("The voice bank.")),"{texts:?}");
        assert!(!texts.iter().any(|t|t.contains("hello")));
        Ok(())
    }
    /// Reload notes describe verified starts only: a replacement that never answers writes nothing, and the automatic
    /// restart that finally resumes writes one note that says so. A pending reload left by an earlier run is cleared
    /// at startup without any note.
    #[test]
    fn reload_note_follows_a_verified_start_only() -> Result<()> {
        let (dir,data,session,_tools,runtime)=setup()?;
        // The reload's reason reaches the conversation only in the context of a start that took it (E4.1, E4.6).
        let notes=|session:&Path|->Result<Vec<String>>{Ok(contexts(session)?.into_iter().filter(|c|c.contains("Reloaded: ")).collect())};
        std::fs::write(data.join("die-on-start"),"")?;
        runtime.send(Command::Reload{reason:"User reloaded the agent from the Agent window".into(),abort:false,permit:None})?;
        wait_for(&session,|r|r.iter().any(|e|e.kind=="agent_reload"))?;
        std::thread::sleep(std::time::Duration::from_millis(500));
        assert!(notes(&session)?.is_empty(),"a failed replacement wrote a note");
        assert!(data.join("agent/pending-reload.json").exists(),"the reload stays pending");
        std::fs::remove_file(data.join("die-on-start"))?;
        wait_until("automatic restart",||Ok(!data.join("agent/pending-reload.json").exists()))?;runtime.barrier()?;
        let written=notes(&session)?;
        assert_eq!(written.len(),1,"{written:?}");
        assert!(written[0].contains("Reloaded: User reloaded the agent")&&written[0].ends_with("Don't do anything unless User talks to you."),"{}",written[0]);
        drop(runtime);
        // A pending reload from an earlier run: cleared at startup, no note.
        std::fs::write(data.join("agent/pending-reload.json"),serde_json::to_vec(&json!({"wait_for_settlement":true,"reason":"User reloaded the agent from the Agent window at 05:16","generation":0,"receipts":[]}))?)?;
        let session=data.join("archive/session-relaunch");
        let relaunched=mock_runtime(dir.path(),&data,&session,false)?;
        assert!(!data.join("agent/pending-reload.json").exists());
        let rows=heimdall_archive::Journal::open_read_only(&session)?.records()?;
        assert!(rows.iter().any(|e|e.kind=="agent_reload_stale"));
        assert!(notes(&session)?.is_empty(),"a stale reload produced a note");
        drop(relaunched);Ok(())
    }
    /// "One very long session." Agent off/on, a model switch, a reload, a crash and a relaunch (the
    /// state an app reinstall keeps) all resume the one conversation: same session file, same header, no new file.
    #[test]
    fn every_restart_path_resumes_the_same_pi_session() -> Result<()> {
        let (dir,data,session,tools,runtime)=setup()?;
        let (pid,path)=runtime.inspect()?.context("Pi is not running")?;
        runtime.send(Command::Send{text:"first".into(),steer:false})?;
        wait_for(&session,|r|r.iter().any(|e|e.kind=="agent_reply"&&e.payload["event"]["kind"]=="complete"))?;runtime.barrier()?;
        let header=std::fs::read_to_string(&path)?.lines().next().map(str::to_owned);
        let sessions=||->Result<Vec<PathBuf>>{Ok(std::fs::read_dir(&data)?.filter_map(|e|e.ok().map(|e|e.path())).filter(|p|p.extension().is_some_and(|x|x=="jsonl")).collect())};
        let before=sessions()?;
        let same=|label:&str,runtime:&AgentRuntime|->Result<u32>{
            let (pid,now)=runtime.inspect()?.with_context(||format!("Pi not running after {label}"))?;
            assert_eq!(now,path,"{label} changed the session");
            assert_eq!(std::fs::read_to_string(&now)?.lines().next().map(str::to_owned),header,"{label} rewrote the session header");
            Ok(pid)
        };
        runtime.send(Command::Restart(Settings{dry_run:true,..Default::default()}))?;runtime.barrier()?;
        runtime.send(Command::Restart(Settings{dry_run:false,..Default::default()}))?;runtime.barrier()?;
        let after_on=same("Agent off and on",&runtime)?;assert_ne!(after_on,pid);
        runtime.send(Command::Restart(Settings{dry_run:false,agent_model:agent::provider::AgentModel::Local,..Default::default()}))?;runtime.barrier()?;
        let after_switch=same("a model switch",&runtime)?;
        runtime.send(Command::Reload{reason:"test reload".into(),abort:false,permit:None})?;
        wait_until("reload",||Ok(runtime.inspect()?.is_some_and(|(p,_)|p!=after_switch)))?;runtime.barrier()?;
        let after_reload=same("a reload",&runtime)?;
        unsafe{libc::kill(after_reload as i32,libc::SIGKILL);}
        wait_until("crash recovery",||Ok(runtime.inspect()?.is_some_and(|(p,_)|p!=after_reload)))?;runtime.barrier()?;
        same("crash recovery",&runtime)?;
        drop(runtime);
        test_harness(Some(Path::new(env!("CARGO_MANIFEST_DIR")).join("../agent/tests/fixtures/mock-adapter.py")));
        let relaunched=AgentRuntime::start(Settings{dry_run:false,..Default::default()},data.clone(),session.clone(),Arc::new(Tools::new(dir.path(),&data)),None);
        test_harness(None);let relaunched=relaunched?;relaunched.barrier()?;
        same("a relaunch",&relaunched)?;
        assert_eq!(sessions()?,before,"a new session file appeared");
        drop(relaunched);drop(tools);Ok(())
    }
    #[test]
    fn reload_prompt_explicitly_excludes_new_heimdall_binary_code() {
        let prompt=include_str!("../../../../agent/prompt/heimdall.md");let section=prompt.split("## Your own setup\n\n").nth(1).unwrap().split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(section.contains("call `reload_me` with the reason"),"{section}");
        assert!(section.contains("`reload_me` does not change Heimdall itself."),"{section}");
    }

    /// Review of night pass 3 (finding 1): the user's name-wake restarts a turn that has not acted (night pass H2). The
    /// aborted turn's completions arrive after his wake set the address flag, while the restarted turn runs; they
    /// must not clear it, or the restarted turn answering him during a call is held silent. The restarted turn's own
    /// settlement clears it.
    #[test]
    fn a_rejoined_turns_aborted_completion_keeps_owners_address() -> Result<()> {
        let (_d,_data,session,tools,runtime)=setup()?;
        tools.speech_policy.other_line(capture::host_now_ns());
        runtime.send(Command::Send{text:"[fixture:steering-thinking]".into(),steer:false})?;
        wait_for(&session,|rows|rows.iter().any(|r|r.kind=="agent_reply"&&r.payload["event"]["type"]=="message_update"))?;
        // The pipeline marks the address when the wake goes out, before the bridge delivers (and rejoins) it.
        tools.speech_policy.addressed_wake();
        runtime.send(Command::Send{text:"Heimdall, what time is it?\n[fixture:steering-thinking]".into(),steer:false})?;
        wait_for(&session,|rows|rows.iter().any(|r|r.kind=="agent_turn_rejoined"&&r.payload["restarted"]==true))?;
        // The aborted turn's completion has been handled once the actor answers the barrier after it.
        wait_for(&session,|rows|rows.iter().any(|r|r.kind=="agent_reply"&&r.payload["event"]["kind"]=="complete"))?;
        runtime.barrier()?;
        assert!(!tools.speech_policy.speech_held(capture::host_now_ns()), "the aborted turn's completion cleared User's address");
        drop(runtime);
        Ok(())
    }

/// One single-question card of `labels`, as the `ask` tool would make it (test helper, 24 Oct).
fn asked(question: &str, labels: &[String]) -> Vec<agent::actions::AskItem> {
    let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
    vec![agent::actions::AskItem::one(question, &refs)]
}

/// The island's answer to a one-question card: one label, no words of his own (test helper, 24 Oct).
fn one_answer(label: &str) -> Vec<ui::agent_notice::QuestionAnswer> {
    vec![ui::agent_notice::QuestionAnswer { question_id: "q1".into(), labels: vec![label.into()], other: None }]
}

/// The user says `text` for Heimdall now (the wake carries his words; the card voice path runs inside it, 24 Oct).
fn say_for_heimdall(tools:&Arc<Tools>,runtime:&AgentRuntime,n:u64,text:&str)->Result<()> {
    let now=capture::host_now_ns();
    let id=format!("choice-{n}");
    runtime.send(Command::Observe(Observation{id:id.clone(),timestamp_ms:agent::now_ms(),source:"Mic".into(),speaker:"User".into(),language:"en".into(),text:text.into()}))?;
    let through={let mut c=tools.send_context.lock().unwrap();c.observe_at("User",text,now+1,json!({"id":id,"start_ns":now}))};
    tools.spoke_to_heimdall(through);
    runtime.wake_pending_through_with_permit(vec![],false,through,Some(id),agent::Speaker::Owner,tools.agent_enabled.permit()?)?;
    runtime.barrier()
}

/// Two questions on one card: a single-select "Which voice?" and a multi-select "Which tones?" with Other.
fn two_question_items()->Vec<agent::actions::AskItem> {
    use agent::actions::{AskItem,AskOption};
    let opt=|label:&str|AskOption{label:label.into(),description:None,grant:None};
    vec![
        AskItem::one("Which voice?",&["Warm","Bright"]),
        AskItem{id:"q2".into(),header:Some("Tones".into()),question:"Which tones?".into(),multi_select:true,allow_other:true,options:vec![opt("Soft"),opt("Loud")]},
    ]
}

/// Answers from the island for both questions go to the agent once, each keyed by its question (24 Oct).
#[test]
fn a_two_question_card_commits_once_with_each_answer_keyed_by_its_question()->Result<()> {
    let (_d,data,session,tools,_runtime)=setup()?; tools.notice_actions_available.store(true,Ordering::Release);
    let p=tools.agent_enabled.permit()?; let actions=tools.mcp_actions.actions.as_ref().unwrap();
    let card=actions.ask("q-two",&two_question_items(),&p)?;
    let answers=vec![
        ui::agent_notice::QuestionAnswer{question_id:"q1".into(),labels:vec!["Bright".into()],other:None},
        ui::agent_notice::QuestionAnswer{question_id:"q2".into(),labels:vec!["Soft".into()],other:Some("low".into())},
    ];
    tools.answer_question(&card,&answers,&[])?;
    assert_eq!(actions.state(&card)?,"answered");
    wait_for(&session,|r|r.iter().any(|e|e.kind=="agent_send"&&e.payload.to_string().contains("User answered")))?;
    assert_eq!(delivery_count(&data,"User answered q1")?,1);
    assert_eq!(delivery_count(&data,"User answered q2")?,1);
    assert_eq!(delivery_count(&data,"Soft; Other:")?,1,"the second answer keeps its words of his own");
    // Committed once: a second Send finds the card answered and changes nothing (R16: no "Question unavailable").
    tools.answer_question(&card,&answers,&[])?;
    assert_eq!(actions.state(&card)?,"answered");
    assert_eq!(delivery_count(&data,"User answered q1")?,1);
    Ok(())
}

/// Approval grants (4 Nov evening): a click on a granting option writes the grant only when the island marks the pick as
/// the user's own pointer (`by_hand`); the same answer from a key, an accessibility press or a posted event writes none.
#[test]
fn a_grant_is_written_only_for_a_pick_by_hand()->Result<()> {
    let (_d,data,_session,tools,_runtime)=setup()?; tools.notice_actions_available.store(true,Ordering::Release);
    let p=tools.agent_enabled.permit()?; let actions=tools.mcp_actions.actions.as_ref().unwrap();
    let card=|| agent::actions::parse_ask(&serde_json::json!({"questions":[{"question":"Delete the old copy?","options":[
        {"label":"Delete it","grant":{"delete":["~/heimdall-grant-test/old-copy.app"]}},"Keep it"]}]})).unwrap();
    let pick=vec![ui::agent_notice::QuestionAnswer{question_id:"q1".into(),labels:vec!["Delete it".into()],other:None}];
    let grants=data.join(heimdall_emergency::grants::DIR);
    let files=|| std::fs::read_dir(&grants).map(|d|d.flatten().filter(|e|e.path().extension().is_some_and(|x|x=="json")).count()).unwrap_or(0);
    let not_by_hand=actions.ask("grant-key",&card(),&p)?;
    tools.answer_question(&not_by_hand,&pick,&[])?;
    assert_eq!(actions.state(&not_by_hand)?,"answered","the answer still goes to the agent");
    assert_eq!(files(),0,"no grant without his own click");
    let by_hand=actions.ask("grant-click",&card(),&p)?;
    tools.answer_question(&by_hand,&pick,&pick)?;
    assert_eq!(files(),1,"his click grants exactly the named path");
    let home=std::env::var("HOME").unwrap();
    let ledger=heimdall_emergency::grants::Ledger::load(&data,"rm",agent::now_ms());
    assert!(ledger.covers(&heimdall_emergency::grants::Scope::Delete(PathBuf::from(format!("{home}/heimdall-grant-test/old-copy.app")))));
    assert!(!ledger.covers(&heimdall_emergency::grants::Scope::Delete(PathBuf::from(format!("{home}/heimdall-grant-test")))));
    Ok(())
}

/// A spoken choice answers the first open question; the card waits for the rest, then completes with the last one
/// (24 Oct, "B", then "both"). Talk that is not a choice closes a card as before.
#[test]
fn spoken_choices_fill_a_card_one_question_at_a_time_and_talk_leaves_it_open()->Result<()> {
    let (_d,data,session,tools,runtime)=setup()?; tools.notice_actions_available.store(true,Ordering::Release);
    let p=tools.agent_enabled.permit()?; let actions=tools.mcp_actions.actions.as_ref().unwrap();
    let card=actions.ask("q-spoken",&two_question_items(),&p)?;
    // "Bright": a partial answer. The card stays up with the pick kept, and the island is told the pick.
    say_for_heimdall(&tools,&runtime,1,"Bright")?;
    assert_eq!(actions.state(&card)?,"shown");
    assert_eq!(tools.pending_questions.lock().unwrap()[&card].picks.len(),1);
    assert_eq!(tools.pending_questions.lock().unwrap()[&card].picks[0].labels,vec!["Bright".to_string()]);
    // "both" on the two-option multi-select: the card completes and is committed with both answers.
    say_for_heimdall(&tools,&runtime,2,"both")?;
    wait_state(actions,&card,"answered")?;
    wait_for(&session,|r|r.iter().any(|e|e.kind=="agent_send"&&e.payload.to_string().contains("User answered")))?;
    assert_eq!(delivery_count(&data,"Soft, Loud")?,1);
    // A single-question card: "Yes" is its option, so it completes with the legacy line, as a click would.
    let yes_no=["Yes".to_string(),"No".to_string()];
    let yes=actions.ask("q-yes",&asked("Push it too?",&yes_no),&p)?;
    say_for_heimdall(&tools,&runtime,3,"Yes")?;
    wait_state(actions,&yes,"answered")?;
    wait_for(&session,|r|r.iter().any(|e|e.kind=="agent_send"&&e.payload.to_string().contains("User clicked Yes. Question: Push it too?")))?;
    // A letter past the last option is talk, not an answer: the card stays open until a click or ask_close.
    let late=actions.ask("q-late",&asked("Push it too?",&yes_no),&p)?;
    say_for_heimdall(&tools,&runtime,4,"D")?;
    assert_eq!(actions.state(&late)?,"shown");
    assert!(actions.ask_answer(&late)?.is_none());
    Ok(())
}

/// Waits for card `id` to reach `want`: a spoken choice that completes a card commits it off the wake's thread.
fn wait_state(actions:&agent::actions::Actions,id:&str,want:&str)->Result<()> {
    let began=std::time::Instant::now();
    while actions.state(id)?!=want {
        anyhow::ensure!(began.elapsed()<std::time::Duration::from_secs(5),"card {id} is {} not {want}",actions.state(id)?);
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    Ok(())
}

/// A pick made by voice and kept on a card the agent did not answer: a non-choice is talk and leaves the card open with
/// the pick kept (R2; 25 Oct).
#[test]
fn a_partial_pick_survives_talk_and_the_card_stays_open()->Result<()> {
    let (_d,_data,session,tools,runtime)=setup()?; tools.notice_actions_available.store(true,Ordering::Release);
    let p=tools.agent_enabled.permit()?; let actions=tools.mcp_actions.actions.as_ref().unwrap();
    let card=actions.ask("q-kept-pick",&two_question_items(),&p)?;
    say_for_heimdall(&tools,&runtime,11,"Bright")?;
    assert_eq!(actions.state(&card)?,"shown", "a partial pick keeps the card open");
    say_for_heimdall(&tools,&runtime,12,"what's the meter one")?;
    assert_eq!(actions.state(&card)?,"shown", "talk leaves the card open");
    assert_eq!(tools.pending_questions.lock().unwrap()[&card].picks[0].labels,vec!["Bright".to_string()]);
    wait_for(&session,|r|r.iter().any(|e|e.kind=="agent_send"&&e.payload.to_string().contains("stays open")))?;
    Ok(())
}

/// A commit the ledger refuses (here: the permit it was asked under is revoked) leaves the card up and answerable,
/// and a commit that goes through retracts it (R8).
#[test]
fn a_refused_voice_commit_leaves_the_card_up_and_a_later_one_answers_it()->Result<()> {
    let (_d,_data,_session,tools,_runtime)=setup()?; tools.notice_actions_available.store(true,Ordering::Release);
    let p=tools.agent_enabled.permit()?; let actions=tools.mcp_actions.actions.as_ref().unwrap();
    let yes_no=["Yes".to_string(),"No".to_string()];
    let card=actions.ask("q-refused-commit",&asked("Push it too?",&yes_no),&p)?;
    let yes=vec![agent::actions::AskAnswer{question_id:"q1".into(),labels:vec!["Yes".into()],other:None}];
    // A permit from a gate of its own, switched off: the ledger refuses the commit without stopping the live agent.
    let gate=Arc::new(agent::delivery::DeliveryGate::new(true));
    let stale=gate.permit()?;
    gate.intent(agent::delivery::StopReason::Agent,false);
    assert!(super::tools::commit_voice_choice(actions,None,&card,&yes,&stale,"Yes").is_err());
    assert_eq!(actions.state(&card)?,"shown", "refused: still open, not answered");
    super::tools::commit_voice_choice(actions,None,&card,&yes,&p,"Yes")?;
    assert_eq!(actions.state(&card)?,"answered");
    Ok(())
}

/// A card past its day takes no words: it is not an open card, so a spoken choice cannot commit it (R8).
#[test]
fn an_expired_card_is_not_open_and_takes_no_words()->Result<()> {
    let (_d,_data,_session,tools,_runtime)=setup()?; tools.notice_actions_available.store(true,Ordering::Release);
    let p=tools.agent_enabled.permit()?; let actions=tools.mcp_actions.actions.as_ref().unwrap();
    let yes_no=["Yes".to_string(),"No".to_string()];
    let card=actions.ask("q-expired-card",&asked("Push it too?",&yes_no),&p)?;
    assert!(tools.open_cards().iter().any(|c|c.id==card), "a live card is open");
    tools.pending_questions.lock().unwrap().get_mut(&card).unwrap().expires_ms=1;
    assert!(!tools.open_cards().iter().any(|c|c.id==card), "an expired card is not open");
    let now=capture::host_now_ns();
    assert_eq!(tools.card_state(now,now+1),crate::bridge::questions::CardState::None);
    Ok(())
}

/// The late ask_close after a spoken choice committed the card says it is already answered, not by click (R12).
#[test]
fn a_late_close_after_a_voice_commit_says_already_answered_without_a_click()->Result<()> {
    let (_d,_data,_session,tools,_runtime)=setup()?; tools.notice_actions_available.store(true,Ordering::Release);
    let p=tools.agent_enabled.permit()?; let actions=tools.mcp_actions.actions.as_ref().unwrap();
    let yes_no=["Yes".to_string(),"No".to_string()];
    let card=actions.ask("q-late-voice",&asked("Push it too?",&yes_no),&p)?;
    let yes=vec![agent::actions::AskAnswer{question_id:"q1".into(),labels:vec!["Yes".into()],other:None}];
    super::tools::commit_voice_choice(actions,None,&card,&yes,&p,"Yes")?;
    let rt=tokio::runtime::Runtime::new()?;
    let reply=rt.block_on(tools.mcp_actions.action(&tools,"ask_close",json!({"request_id":card,"answer":"yes"}),"close-late",&p))?;
    assert_eq!(reply,"Question already answered; nothing to close.");
    Ok(())
}

/// The user says each of `texts` for Heimdall, all in one wake, so the card sees them together (R9, 24 Oct).
fn say_together_for_heimdall(tools:&Arc<Tools>,runtime:&AgentRuntime,n:u64,texts:&[&str])->Result<()> {
    let mut through=0;
    let mut last_id=String::new();
    for (k,&text) in texts.iter().enumerate() {
        let now=capture::host_now_ns();
        let id=format!("together-{n}-{k}");
        runtime.send(Command::Observe(Observation{id:id.clone(),timestamp_ms:agent::now_ms(),source:"Mic".into(),speaker:"User".into(),language:"en".into(),text:text.into()}))?;
        through={let mut c=tools.send_context.lock().unwrap();c.observe_at("User",text,now+1,json!({"id":id,"start_ns":now}))};
        tools.spoke_to_heimdall(through);
        last_id=id;
    }
    runtime.wake_pending_through_with_permit(vec![],false,through,Some(last_id),agent::Speaker::Owner,tools.agent_enabled.permit()?)?;
    runtime.barrier()
}

/// One question with three options, Alpha, Beta and Gamma (letters A, B and C), single or multi-select (R9 tests).
fn three_options(multi:bool)->Vec<agent::actions::AskItem> {
    use agent::actions::{AskItem,AskOption};
    let opt=|label:&str|AskOption{label:label.into(),description:None,grant:None};
    vec![AskItem{id:"q1".into(),header:None,question:"Which?".into(),multi_select:multi,allow_other:true,options:vec![opt("Alpha"),opt("Beta"),opt("Gamma")]}]
}

/// Two choices said in one wake on a single select: the last one is the pick, the earlier one is dropped (R9).
#[test]
fn two_choices_in_one_wake_on_a_single_select_keep_the_last()->Result<()> {
    let (_d,_data,_session,tools,runtime)=setup()?; tools.notice_actions_available.store(true,Ordering::Release);
    let p=tools.agent_enabled.permit()?; let actions=tools.mcp_actions.actions.as_ref().unwrap();
    let card=actions.ask("q-r9-single",&three_options(false),&p)?;
    say_together_for_heimdall(&tools,&runtime,31,&["B","C"])?;
    wait_state(actions,&card,"answered")?;
    assert_eq!(actions.ask_answer(&card)?.unwrap().1,"Gamma");
    Ok(())
}

/// Two choices said in one wake on a multi-select: both picks stand, in option order (R9).
#[test]
fn two_choices_in_one_wake_on_a_multi_select_keep_both()->Result<()> {
    let (_d,_data,_session,tools,runtime)=setup()?; tools.notice_actions_available.store(true,Ordering::Release);
    let p=tools.agent_enabled.permit()?; let actions=tools.mcp_actions.actions.as_ref().unwrap();
    let card=actions.ask("q-r9-multi",&three_options(true),&p)?;
    // "A." rather than "A": a bare capital A is the article for the parser (R5).
    say_together_for_heimdall(&tools,&runtime,32,&["A.","C"])?;
    wait_state(actions,&card,"answered")?;
    assert_eq!(actions.ask_answer(&card)?.unwrap().1,"Alpha, Gamma");
    Ok(())
}

/// The same letter said twice on a multi-select is one pick (R9).
#[test]
fn a_repeated_choice_in_one_wake_on_a_multi_select_is_one_pick()->Result<()> {
    let (_d,_data,_session,tools,runtime)=setup()?; tools.notice_actions_available.store(true,Ordering::Release);
    let p=tools.agent_enabled.permit()?; let actions=tools.mcp_actions.actions.as_ref().unwrap();
    let card=actions.ask("q-r9-repeat",&three_options(true),&p)?;
    say_together_for_heimdall(&tools,&runtime,33,&["A.","A."])?;
    wait_state(actions,&card,"answered")?;
    assert_eq!(actions.ask_answer(&card)?.unwrap().1,"Alpha");
    Ok(())
}

/// A choice said in one wake with talk that is not a choice: the card stays open and the letter is no pick (R9; talk
/// never closes a card, 25 Oct).
#[test]
fn a_choice_and_talk_in_one_wake_leave_the_card_open_with_no_pick()->Result<()> {
    let (_d,_data,_session,tools,runtime)=setup()?; tools.notice_actions_available.store(true,Ordering::Release);
    let p=tools.agent_enabled.permit()?; let actions=tools.mcp_actions.actions.as_ref().unwrap();
    let card=actions.ask("q-r9-talk",&three_options(false),&p)?;
    say_together_for_heimdall(&tools,&runtime,34,&["B","what's the meter one"])?;
    assert_eq!(actions.state(&card)?,"shown");
    assert!(actions.ask_answer(&card)?.is_none());
    assert!(tools.pending_questions.lock().unwrap()[&card].picks.is_empty());
    Ok(())
}

/// Bug note 2026-10-28T19-41-00 (28 Oct 17:27:55): the idle compaction failed on the agent's model with Anthropic's 400
/// "Your credit balance is too low" and nothing tried elsewhere. The fixture Pi fails the same way; Background Tasks ▸
/// Model (Luna on the ChatGPT sign-in, scripted) summarises instead, Pi writes it as its own compaction entry for the
/// cut it chose, and the island says nothing.
const CREDIT: &str = "Summarization failed: 400 {\"type\":\"error\",\"error\":{\"type\":\"invalid_request_error\",\"message\":\"Your credit balance is too low to access the Anthropic API. Please go to Plans & Billing to upgrade or purchase credits.\"}}";
fn chatgpt_row(id:&str,name:&str,context:u64)->agent::provider::ApiModel{
    agent::provider::ApiModel{provider:agent::provider::CHATGPT.into(),id:id.into(),name:name.into(),api:agent::provider::Api::OpenaiResponses,context,max_output:128_000,
        images:true,reasoning:true,cost:None,context_budget:None,thinking:None,base_url:None,efforts:vec![]}
}
fn fallback_setup(knobs:&[(&str,&str)],answers:Value)->Result<(tempfile::TempDir,PathBuf,PathBuf,Arc<Tools>,AgentRuntime)>{
    let d=tempfile::tempdir()?; let data=d.path().join("data"); let session=data.join("archive/session-integration"); std::fs::create_dir_all(&data)?;
    for (name,body) in [("test-start-check",""),("compaction-fails",CREDIT)].iter().chain(knobs) { std::fs::write(data.join(name),body)?; }
    std::fs::write(data.join("test-compaction-answers.json"),answers.to_string())?;
    test_harness(Some(Path::new(env!("CARGO_MANIFEST_DIR")).join("../agent/tests/fixtures/mock-adapter.py")));
    let tools=Arc::new(Tools::new(d.path(),&data));
    let settings=Settings{dry_run:false,background_model:crate::background_model::BackgroundModel{provider:agent::provider::CHATGPT.into(),id:"gpt-6-luna".into()},
        api_models:vec![chatgpt_row("gpt-6-luna","GPT-6 Luna",272_000),chatgpt_row("gpt-6.1-sol","GPT-6.1 Sol",500_000)],..Default::default()};
    let result=AgentRuntime::start(settings,data.clone(),session.clone(),tools.clone(),Some(ui::Ui::handle()));
    test_harness(None); let runtime=result?; runtime.barrier()?;
    Ok((d,data,session,tools,runtime))
}
fn compactions(data:&Path)->Vec<Value>{
    std::fs::read_to_string(data.join("pi-session.jsonl")).unwrap_or_default().lines().filter_map(|l|serde_json::from_str::<Value>(l).ok()).filter(|e|e["type"]=="compaction").collect()
}
fn agent_notices(tools:&Tools)->Vec<String>{
    tools.test_island.lock().unwrap().iter().filter_map(|u|match u{ui::UiUpdate::Health(ui::HealthCue::Notice{part:ui::Part::Agent,label})=>Some(label.clone()),_=>None}).collect()
}
fn compact_requests(rows:&[heimdall_archive::Record])->usize{rows.iter().filter(|r|r.kind=="agent_transport_reply"&&r.payload["wire"]["type"]=="mock_control"&&r.payload["wire"]["command"]=="compact").count()}
#[test]
fn a_compaction_refused_for_credit_is_summarised_on_the_background_model_without_a_notice() -> Result<()> {
    let (_d,data,session,tools,runtime)=fallback_setup(&[("context-tokens","440000"),("compaction-source-tokens","50000")],json!({"openai/gpt-6-luna":{"summary":"Luna summary"}}))?;
    journal_until(&session,"the fallback never compacted",|r|r.iter().any(|e|e.kind=="agent_compaction"&&e.payload["state"]=="fallback"))?;
    runtime.barrier()?;
    let rows=journal_records(&session);
    assert_eq!(rows.iter().filter(|e|e.kind=="agent_compaction"&&e.payload["state"]=="idle_failed").count(),1,"the agent's own attempt failed once");
    assert_eq!(rows.iter().find(|e|e.kind=="agent_compaction"&&e.payload["state"]=="fallback").unwrap().payload["model"],"openai/gpt-6-luna");
    assert_eq!(compact_requests(&rows),2,"the failed one, then Pi writing Luna's");
    let c=compactions(&data);
    assert_eq!(c.len(),1);
    assert_eq!((c[0]["summary"].as_str(),c[0]["fromHook"].as_bool()),(Some("Luna summary"),Some(true)));
    assert!(agent_notices(&tools).is_empty(),"a fallback that works is a log line: {:?}",agent_notices(&tools));
    drop(runtime);Ok(())
}
/// The same failure with every option failing too (Luna's limit spent, Sol's sign-in gone): the history is as it was,
/// and the island says so once.
#[test]
fn a_compaction_every_model_refuses_leaves_the_history_intact_with_one_notice() -> Result<()> {
    // 95,000 tokens: past the fixture budget's idle mark (87,040), inside its 102,400 tokens, so nothing is cut.
    let (_d,data,session,tools,runtime)=fallback_setup(&[("context-tokens","95000"),("compaction-source-tokens","80000")],
        json!({"openai/gpt-6-luna":{"error":"The ChatGPT user has reached their Subscription Sharing usage limit."},"openai/gpt-6.1-sol":{"error":"401 signed out"}}))?;
    journal_until(&session,"the fallback never ended",|r|r.iter().any(|e|e.kind=="agent_compaction"&&e.payload["state"]=="fallback_failed"))?;
    runtime.barrier()?;
    let rows=journal_records(&session);
    let failed=rows.iter().find(|e|e.kind=="agent_compaction"&&e.payload["state"]=="fallback_failed").unwrap();
    assert_eq!(failed.payload["tried"].as_array().map(Vec::len),Some(2),"Luna, then Sol: {}",failed.payload);
    assert_eq!(compact_requests(&rows),1,"no second compaction");
    assert!(compactions(&data).is_empty(),"history intact");
    assert_eq!(agent_notices(&tools),["Agent history not compacted: no model could summarise it"]);
    drop(runtime);Ok(())
}
/// Review 28 Oct: Luna's summary was made, but the history changed before Pi could write it (the extension cancels):
/// nothing is written, and the island says once that it was not compacted.
#[test]
fn a_summary_pi_cancels_leaves_the_history_intact_with_one_notice() -> Result<()> {
    let (_d,data,session,tools,runtime)=fallback_setup(&[("context-tokens","95000"),("compaction-source-tokens","50000"),("supply-cancels","")],json!({"openai/gpt-6-luna":{"summary":"Luna summary"}}))?;
    journal_until(&session,"the cancel was never seen",|r|r.iter().any(|e|e.kind=="agent_compaction"&&e.payload["state"]=="fallback_failed"))?;
    runtime.barrier()?;
    let rows=journal_records(&session);
    assert_eq!(compact_requests(&rows),2,"the failed one, then the cancelled supply");
    assert!(compactions(&data).is_empty(),"history intact");
    assert_eq!(agent_notices(&tools),["Agent history not compacted: no model could summarise it"]);
    drop(runtime);Ok(())
}
/// a history past the agent's budget that no model's window holds is cut to the newest whole turns
/// (80% of the budget) without a summary, with one notice; the entries stay in the session file.
#[test]
fn a_history_no_model_can_summarise_is_cut_to_the_newest_turns_with_one_notice() -> Result<()> {
    let (_d,data,session,tools,runtime)=fallback_setup(&[("context-tokens","440000"),("compaction-source-tokens","600000")],json!({}))?;
    journal_until(&session,"the history was never cut",|r|r.iter().any(|e|e.kind=="agent_compaction"&&e.payload["state"]=="cut"))?;
    runtime.barrier()?;
    let c=compactions(&data);
    assert_eq!(c.len(),1);
    assert!(c[0]["summary"].as_str().unwrap().contains("cut without a summary"));
    assert_eq!(c[0]["details"]["keepTokens"],81_920,"80% of the fixture's 102,400-token budget");
    assert_eq!(agent_notices(&tools),["Agent history too long for every model: its oldest part was cut"]);
    drop(runtime);Ok(())
}
