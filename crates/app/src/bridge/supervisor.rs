//! The agent supervisor (`AgentRuntime`): owns the harness thread, its command channel and shutdown.
use super::*;

pub struct AgentRuntime {
    pub(super) gate: Arc<agent::delivery::DeliveryGate>,
    pub(super) tx: mpsc::UnboundedSender<Command>,
    pub(super) thread: Option<std::thread::JoinHandle<()>>,
}
impl AgentRuntime {
    pub fn start(
        settings: Settings,
        data: PathBuf,
        session: PathBuf,
        tools: Arc<Tools>,
        ui: Option<ui::Ui>,
    ) -> Result<Self> {
        if let Some(u)=ui{u.send(ui::UiUpdate::AgentWindow(ui::AgentWindowUpdate::State(ui::AgentWindowState{state:if settings.dry_run{ui::AgentRunState::Off}else{ui::AgentRunState::Starting},can_send:false,can_stop:false,can_reload:!settings.dry_run,problem:None,unresolved:vec![],queued:0,reviews:vec![],reload:None})));}
        let runtime_gate=tools.agent_enabled.clone();
        let (tx, mut rx) = mpsc::unbounded_channel();
        *tools.agent_commands.lock().unwrap() = Some(tx.clone());
        // Commands a stop held back (`Stopping`) go back into the queue once it ends.
        let resend = tx.clone();
        *tools.notice_ui.lock().unwrap() = ui;
        tools.fast_reads.attach_ui(ui);
        tools.speech_voices.attach_ui(ui); tools.speech_voices.refresh_elevenlabs(); tools.speech_voices.refresh_own(); // Speaking ▸ the voice (night 1022 menu)
        // Speaking ▸ Voice's samples, rendered ahead at idle so a click plays at once (only the app, with its menu).
        if ui.is_some() { tools.prerender_voice_previews(); }
        if ui.is_some() { attach_speech_level(ui, Arc::downgrade(&tools)); }
        let deliver_tools = Arc::downgrade(&tools);
        let reload_tools = Arc::downgrade(&tools);
        let ask_tools = Arc::downgrade(&tools);
        let workers = tools.mcp_actions.attach(ui.unwrap_or_else(ui::Ui::handle),
            Arc::new(move |id,text,p| deliver_tools.upgrade().context("Agent runtime stopped")?.notice(id,text,p)),
            Arc::new(move |id,p| reload_tools.upgrade().context("Agent runtime stopped")?.reload(id,p)),
            Arc::new(move |q,p| ask_tools.upgrade().context("Agent runtime stopped")?.show_question(q,p)))?;
        let archive_session=session.clone();
        let payload_archive:agent::PayloadArchive=Arc::new(move|value|{
            let mut value=value.clone();let journal=heimdall_archive::Journal::open(&archive_session)?;
            reference_transport_images(&journal,&mut value)?;
            let bytes=serde_json::to_vec(&value)?;let reference=format!("agent-payload-{}.json",heimdall_archive::hash(&bytes));
            heimdall_archive::store::archive_log(&archive_session,&reference,&bytes)?;Ok(reference)
        });
        workers.actions.attach_archive(payload_archive.clone());
        if !settings.dry_run { if let Ok(p)=tools.agent_enabled.permit() { if let Err(e)=crate::fast_read::report_stale(&tools,&p) {eprintln!("Stale fast reads not reported: {e:#}");} } }
        #[cfg(test)]
        let test_harness = fixture_executable();
        let thread=std::thread::Builder::new().name("heimdall-agent".into()).spawn(move||{
            #[cfg(test)]TEST_HARNESS.with(|p|*p.borrow_mut()=test_harness);
            let rt=tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
            rt.block_on(async move{
                let (watch_stop, watch_rx) = tokio::sync::watch::channel(false);
                tools.watcher_running.store(true, Ordering::Release);
                // Scheduled wakes and the delivery heartbeat (agent::wakes, selfwake night 21 Oct), beside the watcher.
                let mut heartbeat = tools.mcp_actions.wakes.as_ref().ok().cloned().map(|w| tokio::spawn(w.heartbeat(workers.actions.clone(), tools.agent_enabled.clone(), watch_rx.clone())));
                // Plan refresher (night 1022): ends with the watcher's stop signal.
                let _plan_refresher = tools.mcp_actions.spawn_plan_refresher(workers.clone(), tools.agent_enabled.clone(), watch_rx.clone());
                // Background Tasks ▸ Goals (31 Oct): the shadow keeps Goals true from the conversation (docs/goals.md).
                let _goals = crate::goals::spawn_live(&data, tools.agent_enabled.clone(), watch_rx.clone());
                // Background Tasks ▸ Day Timeline: the left island of where the day went, every 15 minutes.
                let _day_timeline = crate::day_timeline::spawn_live(&data, tools.agent_enabled.clone(), watch_rx.clone());
                // The island agent (30 Oct): one stateless call per bound island, woken by the binding's signals.
                let _island_agent = crate::island_agent::spawn_live(tools.agent_enabled.clone(), watch_rx.clone());
                // Floating islands (night 1022): thread-bound islands follow their threads after each T3 pass.
                let _islands = crate::islands::live::binding_loop(workers.clone(), watch_rx.clone());
                // Emergency mode (docs/emergency.md): the cover thread's controller, beside the watcher.
                // It runs whatever Agent was at launch and is started again if it ever ends while Heimdall runs (bug note
                // 2026-10-27T19-45-00: launched with Agent off, it never ran, and three quota failures went nowhere).
                let emergency_side=Arc::new(crate::emergency::AppSide{tools:Arc::downgrade(&tools),data:data.clone(),session:Mutex::new(None)});
                let emergency_run={
                    let (tools,side,data,client,watch)=(tools.clone(),emergency_side.clone(),data.clone(),workers.client.clone(),watch_rx.clone());
                    move||{
                        tools.emergency.respawn();
                        tokio::spawn(tools.emergency.clone().run(Arc::new(crate::emergency::T3Cover(client.clone())),Arc::new(crate::emergency::ProviderProbe{data:data.clone()}),side.clone(),tools.clone(),
                            Arc::new(ArchiveSearch{root:data.join("archive"),embeddings:tools.embeddings.clone()}),tools.agent_enabled.clone(),watch.clone()))
                    }
                };
                let mut emergency_task=emergency_run();
                tools.emergency.configure(relay_options(&settings));
                // Claude Code subscription's models and accounts, for the menu and the settings tool before the menu first opens.
                tools.relay_catalog.connect(workers.client.clone(),tokio::runtime::Handle::current());
                tools.refresh_relay_catalog();
                let mut detector=agent::emergency::Detector::default();
                let mut watcher = Some(tokio::spawn(workers.clone().watch(tools.agent_enabled.clone(), watch_rx)));
                let mut connections = tokio::task::JoinSet::new();
                let result=async{
                    let diagnostics = heimdall_archive::records::Writer::open(&session)?;
                    let mut stale_receipts=settle_stale_reload(&data,&session,&tools.agent_enabled);
                    let mut settings=settings;let gate=tools.agent_enabled.clone();
                    let mut server=if settings.dry_run{None}else{Some(agent::mcp::McpServer::start(tools.clone(),Arc::new(ArchiveSearch{root:data.join("archive"),embeddings:tools.embeddings.clone()}),gate.clone()).await?)};
                    let engine=agent::supervision::EngineProbe::new(agent::runtime::BASE_URL)?;
                    let mut budget=agent::supervision::RestartBudget::load(&data)?;
                    let mut failures=0;let mut retry=None;
                    let mut held_late:Vec<LateImage>=Vec::new();
                    let mut b=match open(&settings,&data,&session,server.as_ref(),ui,&tools.api,gate.clone(),tools.engine_signal.clone(),None,None).await{
                        Ok(b)=>b,
                        Err(e)=>{let engine_down=e.downcast_ref::<agent::harness::EngineUnavailable>().is_some();let mut b=unavailable(&settings,&data,&session,format!("{e:#}"))?;
                            if let Some(waiting)=e.downcast_ref::<agent::harness::ProviderUnavailable>(){if !matches!(b.status,agent::Status::NeedsReconciliation(_)){b.status=agent::Status::WaitingProvider(waiting.0.clone());}}
                            else if engine_down{if !matches!(b.status,agent::Status::NeedsReconciliation(_)){b.status=agent::Status::WaitingEngine;}}else{failures=1;retry=budget.take(&data,agent::now_ms())?.map(|d|tokio::time::Instant::now()+d);if retry.is_none(){failures=4;}}b}
                    };
                    // Decider turns pending from before this launch are not re-observed: they are in the archive, so a
                    // start's catch-up carries them as history (E4.4); they never ride a wake (root cause 3).
                    // Every start (launch, Agent on, model switch, reload, recovery) appends the catch-up and the
                    // restart message before any wake (E4.1). A launch with the agent on looks at archived speech from
                    // before this launch; later starts at speech before the start.
                    let mut launch_on=if settings.dry_run{None}else{launch_ms(&session)};
                    let mut carried:Option<agent::notice::StartContext>=None;
                    let mut start:Option<agent::notice::StartContext>=None;
                    // Test fixtures that never answer control requests (blocked-pi.py) cannot take context; the
                    // scripted mock Pi and the real Pi can.
                    let start_contexts=fixture_executable().is_none_or(|p|p.ends_with("mock-adapter.py"));
                    // An idle compaction failed (provider down, summary failed): no new attempt, not at a restart either,
                    // until a reply succeeds again. Never a supervisor failure. A compaction that died with its Pi backs
                    // off and caps (`CompactionHold`, night audit H1).
                    let mut compaction_hold=CompactionHold::default();
                    // A compaction the agent's provider refused moves to another model (bug note 2026-10-28T19-41-00).
                    let mut fallback=CompactionFallback::default();
                    macro_rules! started {() => {{
                        // A new process: a compaction that dies with it is counted again (`compaction_killed!`).
                        compaction_hold.new_process();
                        fallback.new_process();
                        // A turn the previous process never settled has no reply (agent::reply).
                        tools.replies.lock().unwrap().reset();
                        // Its tools and compaction died with it: they no longer excuse silence (night audit H7).
                        detector.new_process();
                        tools.speech_policy.run_settled(); // speech-policy (night 3)
                        // Worker and T3 notices fit this agent's message limit (it follows the context setting).
                        workers.actions.set_message_limit(b.harness.max_text_bytes());
                        // A context the previous process never took travels to this start.
                        if let Some(old)=start.take(){if carried.is_none(){carried=Some(old);}}
                        if b.start_pending(){
                            let on=launch_on.take().unwrap_or_else(agent::now_ms);
                            if start_contexts {start=start_context(&b,&session,on,&mut carried);} else {b.abandon_start_context();}
                        }
                        // Relay mode is the user's setting (docs/claude-relay.md): on, the relay is the agent from this start.
                        // The session first: a relay that starts at once builds its context from it (review 27 Oct).
                        if let Adapter::Pi(pi)=&b.harness.inner{*emergency_side.session.lock().unwrap()=Some(pi.session_path().to_owned());}
                        if !settings.dry_run{tools.emergency.choose(settings.agent_relay);tools.emergency.resume();tools.emergency_flush_records();}
                        // A relay (chosen or emergency) survives this start: the new process's turns are held too.
                        if !settings.dry_run&&tools.emergency.holding()&&!b.holding_turns(){b.hold_turns(Some(tools.emergency.relay())).await?;}
                    }}}
                    // The agent failed (agent::emergency::Detector): turns are held now, the controller finds a cover.
                    macro_rules! emergency {($cause:expr) => {{
                        let cause:agent::emergency::Cause=$cause;
                        if let Adapter::Pi(pi)=&b.harness.inner{*emergency_side.session.lock().unwrap()=Some(pi.session_path().to_owned());}
                        b.harness.journal.event("agent_emergency","agent:pi",&[],capture::host_now_ns(),json!({"state":"on","cause":cause.to_json()}))?;
                        // The controller is in emergency mode before the hold relays its first input (27 Oct 19:40:05: the
                        // unanswered wake reached it first, was taken for late input, and started a Pi turn).
                        tools.emergency.enter(cause,settings.selected_api_model().cloned());
                        if !b.holding_turns(){b.hold_turns(Some(tools.emergency.relay())).await?;}
                    }}}
                    started!();
                    let mut health=AgentHealth::default();let mut output_open=!matches!(b.harness.inner,Adapter::Unavailable(_));
                    let mut generation=gate.generation();let mut reload:Option<(String,Option<agent::delivery::DeliveryPermit>)>=None;
                    let mut retry_cue=RetryCue::None;let mut tick=tokio::time::interval(std::time::Duration::from_secs(10));tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                    let mut provider_try:Option<tokio::time::Instant>=None;
                    // Idle maintenance (`maintain_idle`) may ask Pi for the history size: from each start (a real Pi only;
                    // test fixtures that never answer control requests start without it), else from the first settled
                    // reply. It runs after every event and on the 10 s tick, and waits for quiet itself.
                    let start_check=fixture_executable().is_none() || (cfg!(test) && data.join("test-start-check").exists());
                    let mut context_check=start_check;
                    // Pi dies (or is stopped) while it compacts: that compaction failed, and the next Pi must not summarise
                    // the same history at once (night audit H1). Counted whenever the agent stays on (a model switch
                    // waiting for it included), once per process: a reopen that fails leaves the dead Pi in `b`, still
                    // reporting its compaction, and must not count it again (review 27 Oct).
                    macro_rules! compaction_killed {($error:expr) => {{
                        if gate.load(Ordering::Acquire)&&matches!(&b.harness.inner,Adapter::Pi(pi) if pi.compaction_in_progress()) {
                            if let Some((kills,wait))=compaction_hold.killed_at(std::time::Instant::now()) {
                                let error:String=$error;
                                let payload=json!({"state":"killed","kills":kills,"held_s":wait.map(|w|w.as_secs()),"error":error});
                                eprintln!("Agent: a compaction died with Pi ({kills} so far); compactions held {}",wait.map_or("until one completes".into(),|w|format!("for {} min",w.as_secs()/60)));
                                let _=b.harness.journal.event("agent_compaction","agent:pi",&[],capture::host_now_ns(),payload.clone());
                                crate::diagnostics::record(&diagnostics,"agent_compaction_killed","agent:pi",payload);
                            }
                        }
                    }}}
                    // Agent off waiting for crucial work before the harness stops (`Stopping`).
                    let mut stopping:Option<Stopping>=None;
                    // The wait is over (crucial work ended, its cap passed, or `$error` broke it): Pi stops now. A
                    // Restart that came meanwhile runs next; Agent back on restarts Pi in its new epoch. Held input
                    // follows the restart in the queue, so it reaches the new Pi.
                    macro_rules! finish_stop {($error:expr) => {{
                        if let Some(stop)=stopping.take(){
                            let error:Option<String>=$error;
                            // Pi's own stop watcher may end it as the work ends, a moment before this loop sees it.
                            let compaction_left=matches!(&b.harness.inner,Adapter::Pi(pi) if pi.compaction_in_progress());
                            let unfinished=compaction_left||gate.writes_in_flight()>0;
                            let waited_ms=stop.since.elapsed().as_millis() as u64;
                            let state=if !unfinished{"stopped"}else if error.is_some()||!output_open{"interrupted"}else{"cap"};
                            // The cap cuts the compaction Pi still runs: a kill like any other (`$error` counts its own).
                            if state=="cap"&&compaction_left {compaction_killed!(format!("stopped at its limit after {waited_ms} ms"));}
                            let _=b.harness.journal.event(if stop.for_restart{"agent_restart_wait"}else{"agent_stop"},"agent:pi",&[],capture::host_now_ns(),json!({"state":state,"waited_ms":waited_ms,"error":error}));
                            if state!="stopped"{
                                eprintln!("Agent off: stopped after {waited_ms} ms with crucial work unfinished ({state})");
                                crate::diagnostics::record(&diagnostics,"agent_stop_wait","agent:pi",json!({"state":state,"waited_ms":waited_ms,"error":error}));
                            }
                            // The compaction it waited for may be storage maintenance: rotate now, so the next start
                            // does not compact the same history again.
                            if !unfinished { if let Adapter::Pi(pi)=&mut b.harness.inner { if pi.session_path().metadata().is_ok_and(|m|m.len()>=agent::session::ROTATE_BYTES) {
                                let archive=session.clone();
                                if let Err(e)=pi.stop_and_rotate(move|bytes|heimdall_archive::store::archive_log(&archive,&format!("pi-session-{}.jsonl",heimdall_archive::hash(bytes)),bytes)).await {eprintln!("Agent off: session not rotated after its compaction: {e:#}");}
                            }}}
                            let _=b.shutdown().await;output_open=false;
                            retry=None;
                            match stop.restart.or_else(||gate.load(Ordering::Acquire).then(||settings.clone())) {
                                Some(s)=>{let _=resend.send(Command::Restart(s));},
                                None=>reload=None,
                            }
                            for command in stop.held{let _=resend.send(command);}
                        }
                    }}}
                    publish_agent_state(&b,&diagnostics,&settings,failures,reload.as_ref().map(|(reason,_)|reason.as_str()),None,ui);health.restarting=retry.is_some();if let Some(u)=ui{health.update(&b.status,failures,&settings,&diagnostics,u);}
                    loop{
                        let notice_due=b.notice_deadline();
                        // A wake waiting out its retry delay after a passing provider failure (`agent::notice::RETRY_DELAYS_MS`).
                        let retry_due=b.retry_deadline();
                        let operation=async{
                            if let Some(stop)=&stopping {
                                if !output_open||crucial_work(&b,&gate,stop.since.elapsed()).is_none() {finish_stop!(None);return Ok(false);}
                            }
                            tokio::select!{
                                biased;
                                _=gate.changed(generation),if !settings.dry_run=>{
                                    generation=gate.generation();reload=None;tools.clear_questions();
                                    // Agent off ends emergency mode: the cover thread stands down with the agent. Quit closes the
                                    // gate too, but is no Agent off: the emergency carries on at the next launch (night audit H6).
                                    if gate.stopped_for(agent::delivery::StopReason::Agent){tools.emergency.end("User switched Heimdall's agent off");}
                                    if !gate.load(Ordering::Acquire){detector=Default::default();}
                                    // A newer intent revokes this process's in-flight work even
                                    // when On immediately follows Off. MCP permits are revoked too.
                                    if !gate.load(Ordering::Acquire)||matches!(b.harness.inner,Adapter::Pi(_)){
                                        tools.interrupt();
                                        // Off during a model switch's wait for a compaction: it is Off's wait now.
                                        if let Some(stop)=stopping.as_mut(){if stop.for_restart&&!gate.load(Ordering::Acquire){
                                            stop.for_restart=false;if let Some(server)=server.take(){server.shutdown().await;}
                                        }}
                                        // Except crucial work: a running compaction or state write
                                        // finishes first, then Pi stops (`Stopping`). Agent back on meanwhile keeps
                                        // waiting and then restarts Pi in the new epoch, the compaction kept.
                                        if stopping.is_none()&&output_open { if let Adapter::Pi(pi)=&mut b.harness.inner { let _=pi.take_in_ready(); } }
                                        if stopping.is_none()&&output_open { if let Some(compaction)=crucial_work(&b,&gate,std::time::Duration::ZERO) {
                                            b.harness.journal.event("agent_stop","agent:pi",&[],capture::host_now_ns(),json!({"state":"waiting","for":if compaction {"compaction"} else {"state_write"},"line":ui::stopping_line(compaction)}))?;
                                            stopping=Some(Stopping{since:stop_since(&data),restart:None,held:vec![],for_restart:false});retry=None;
                                            // No tool call is admitted from here on, also if Agent comes back on during the
                                            // wait (this Pi belongs to the old epoch); a write already running finishes.
                                            if let Some(server)=server.take(){server.shutdown().await;}
                                        }}
                                        if stopping.is_none() {
                                            let _=b.shutdown().await;output_open=false;
                                            if gate.load(Ordering::Acquire){retry=Some(tokio::time::Instant::now());}else{retry=None;reload=None;}
                                        }
                                    }
                                }
                                // The wait's own clock: write ends and the caps are seen within 50 ms.
                                _=tokio::time::sleep(std::time::Duration::from_millis(50)),if stopping.is_some()=>{},
                                _=connections.join_next(), if !connections.is_empty()=>{},
                                cmd=rx.recv()=>{
                                    let (cmd,admission) = match cmd {
                                        Some(Command::Scoped{command,permit}) => { if permit.check().is_err() || (!settings.dry_run && !permit.belongs_to(&gate)){return Ok(false);} (Some(*command),Some(permit)) },
                                        cmd => (cmd,None),
                                    };
                                    // While Agent off waits for crucial work nothing new reaches Pi: a restart waits for
                                    // the stop; input waits too when Agent is back on (it reaches the restarted Pi), and is
                                    // refused when off, as Off refuses it anyway.
                                    let cmd=match (cmd,stopping.as_mut()) {
                                        (Some(Command::Restart(s)),Some(stop))=>{stop.restart=Some(s);return Ok(false);},
                                        // The settings apply to the Pi that starts after the wait.
                                        (Some(Command::Thinking(s)),Some(stop))=>{stop.restart=Some(s);return Ok(false);},
                                        // The user's Stop: Off already stopped all but the crucial work, which no Stop cuts.
                                        (Some(Command::Cancel),Some(_))=>{tools.interrupt();tools.replies.lock().unwrap().reset();return Ok(false);},
                                        (Some(c@(Command::Wake{..}|Command::WakePending{..}|Command::LateImages{..}|Command::Send{..}|Command::Typed{..}|Command::Dropped{..}|Command::EditAndResend{..}|Command::Reload{..}|Command::Notice{..}|Command::ProviderKeyChanged(_))),Some(stop))=>{
                                            if gate.load(Ordering::Acquire) {stop.held.push(match admission {Some(permit)=>Command::Scoped{command:Box::new(c),permit},None=>c});return Ok(false);}
                                            match c {
                                                Command::Notice{ack,..}=>{let _=ack.send(Err("Agent is off".into()));return Ok(false);},
                                                Command::Typed{echo,text,..}=>{if let Some(u)=ui{u.send(typed_echo(&echo,&text,Echo::Failed));}crate::chat_history::typed_failed(&echo);return Err(WindowActionRejected("Agent is off".into()).into())},
                                                Command::Send{..}|Command::Dropped{..}|Command::EditAndResend{..}|Command::Reload{..}=>return Err(WindowActionRejected("Agent is off".into()).into()),
                                                _=>return Ok(false),
                                            }
                                        },
                                        (cmd,_)=>cmd,
                                    };
                                    let mut pending_through=None;
                                    let mut pending_observation=None;
                                    let mut pending_speaker=agent::Speaker::Owner;
                                    let cmd=match cmd {
                                        Some(Command::ProviderKeyChanged(provider))=>{
                                            if settings.dry_run || settings.selected_api_model().is_none_or(|m|m.provider!=provider) {return Ok(false);}
                                            Some(Command::Restart(settings.clone()))
                                        },
                                        Some(Command::WakePending {images,urgent,through,observation,speaker})=>{
                                            if !tools.send_context.lock().unwrap().has_pending_through(through) {
                                                b.harness.journal.event("send_decision_delivery","agent",&[],capture::host_now_ns(),json!({"through":through,"outcome":"superseded_by_barrier"}))?;
                                                return Ok(false);
                                            }
                                            pending_through=Some(through);pending_observation=observation;pending_speaker=speaker;Some(Command::Wake {images,urgent})
                                        },
                                        cmd=>cmd,
                                    };
                                    match cmd{
                                    Some(Command::NoticeAction {action,permit}) => {
                                        if permit.check().is_err(){return Ok(false);}
                                        match action {
                                            ui::agent_notice::AgentNoticeAction::Pick {request_id, answer} => {
                                                tools.record_pick(&request_id, &answer);
                                            }
                                            ui::agent_notice::AgentNoticeAction::Dismiss {request_id} => {
                                                let weak = Arc::downgrade(&tools);
                                                connections.spawn(async move { tokio::task::spawn_blocking(move || {
                                                    if let Some(t) = weak.upgrade() { t.dismiss_question(&request_id); }
                                                }).await.ok(); });
                                            }
                                            ui::agent_notice::AgentNoticeAction::Answer {request_id,answers,by_hand} => {
                                                let weak = Arc::downgrade(&tools);
                                                connections.spawn(async move { tokio::task::spawn_blocking(move || {
                                                    if let Some(t) = weak.upgrade() {
                                                        if let Err(e) = permit.check().and_then(|()| t.answer_question(&request_id,&answers,&by_hand)) {
                                                            t.record_diagnostic("health",json!({"component":"agent-question", "state":"unavailable", "request_id":request_id, "reason":format!("{e:#}")}));
                                                            if let Some(ui)=*t.notice_ui.lock().unwrap(){notice_outcome(ui,request_id.clone(),ui::agent_notice::NoticeOutcome::QuestionUnavailable);}
                                                        }
                                                    }
                                                }).await.ok(); });
                                            }
                                            ui::agent_notice::AgentNoticeAction::ConnectT3 | ui::agent_notice::AgentNoticeAction::ConnectT3Notice { .. } => {
                                                let ids = match &action {
                                                    ui::agent_notice::AgentNoticeAction::ConnectT3Notice { id } => vec![id.clone()],
                                                    _ => connection_notice_ids(&workers.client),
                                                };
                                                let client = workers.client.clone();
                                                let connection_ui=ui;
                                                let connection_tools = Arc::downgrade(&tools);
                                                connections.spawn(async move { let result = permit.run(client.authorize(|url| permit.with(|| {
                                                    std::process::Command::new("/usr/bin/open").arg(url).spawn()?; Ok(())
                                                }))).await;
                                                    if let Some(t) = connection_tools.upgrade() {
                                                        t.record_diagnostic("health",json!({"component":"t3-connection", "state":if result.is_ok(){"connected"}else{"failed"}, "reason":result.as_ref().err().map(|e|format!("{e:#}"))}));
                                                    }
                                                    if permit.check().is_ok(){if let Some(ui)=connection_ui{notice_outcomes(ui,ids,if result.is_ok(){ui::agent_notice::NoticeOutcome::T3Connected}else{ui::agent_notice::NoticeOutcome::T3ConnectionFailed});}} });
                                            }
                                        }
                                    }
                                    Some(Command::Observe(o))=>{if !settings.dry_run&&!gate.load(Ordering::Acquire){b.observe_local(&o)?;}else{b.observe(o).await?;}},
                                    Some(Command::LateImages{caption,images})=>{
                                        if settings.dry_run||gate.load(Ordering::Acquire){
                                            if matches!(&b.harness.inner,Adapter::Pi(pi) if pi.active()) {
                                                let image_ids:Vec<_>=images.iter().map(|i|b.harness.journal.image(&i.bytes,&i.media_type)).collect::<Result<_>>()?;
                                                let delivered=b.steer(&caption,&images,admission.clone()).await?;
                                                b.harness.journal.event("agent_steer",b.harness.source(),&[],capture::host_now_ns(),json!({"image_ids":image_ids,"delivered":delivered.is_some()}))?;
                                            } else {
                                                held_late.push(LateImage{captured_ns:capture::host_now_ns(),caption,images});
                                            }
                                        }
                                    }
                                    Some(Command::Wake{images,urgent})=>{
                                        let mut images=images;let mut instruction=ui::agent_view::WAKE_INSTRUCTION.to_owned();
                                        if settings.dry_run||gate.load(Ordering::Acquire){
                                            for late in take_fresh_late(&mut held_late,capture::host_now_ns()){instruction.push('\n');instruction.push_str(&late.caption);images.extend(late.images);}
                                            // The user's speech closes the question card he was answering, now (bridge/questions.rs).
                                            for line in tools.voice_closes(pending_through){instruction.push('\n');instruction.push_str(&line);}
                                            // His words make a line still waiting for him stale: dropped unheard, and the wake says so (bridge/hold.rs).
                                            if pending_through.is_some()&&pending_speaker==agent::Speaker::Owner{if let Some(line)=tools.drop_held_speech(){instruction.push('\n');instruction.push_str(&line);}}
                                            // The instant acknowledgement already said for these words (pipeline/ack.rs): the agent knows, and doesn't repeat it.
                                            if pending_speaker==agent::Speaker::Owner{if let Some(line)=tools.take_ack_line(pending_through){instruction.push('\n');instruction.push_str(&line);}}
                                        }
                                        let image_ids:Vec<_>=images.iter().map(|i|b.harness.journal.image(&i.bytes,&i.media_type)).collect::<Result<_>>()?;
                                        b.harness.journal.event("agent_wake",b.harness.source(),&[],capture::host_now_ns(),json!({"urgent":urgent,"image_ids":image_ids}))?;
                                        if settings.dry_run||gate.load(Ordering::Acquire){
                                            let busy=matches!(&b.harness.inner,Adapter::Pi(pi) if pi.active()); // Latency map (lab/latency).
                                            let delivered=b.wake_keyed_through(None,&instruction,&images,urgent,admission.clone(),pending_observation.as_deref(),pending_speaker).await?;
                                            if let (Some(id),false)=(&pending_observation,settings.dry_run){crate::latency::delivered(id,busy||delivered.is_none());} // queued: its wait shows as Pi start
                                            // The island's send arrow means delivered now, not queued (18 Oct: "Sent" showed for refused wakes).
                                            if delivered.is_some()&&!settings.dry_run{if let Some(u)=ui{u.send(ui::UiUpdate::AgentSent);}}
                                            if let Some(through)=pending_through {
                                                let mut context=tools.send_context.lock().unwrap();
                                                context.sent(through);tools.persist_send_context(&context)?;
                                            }
                                        }
                                    }
                                    Some(Command::WakePending {..})=>unreachable!("pending wakes normalized above"),
                                    Some(Command::ProviderKeyChanged(_))=>unreachable!("key changes normalized above"),
                                    Some(Command::Notice{id,text,permit,ack})=>{
                                        let result=async {
                                            permit.check()?;validate_message(&text)?;anyhow::ensure!(!settings.dry_run,"Agent is off");
                                            let key=format!("notice-{}",heimdall_archive::hash(id.as_bytes()));
                                            // Notices never steer: they wait for the turn to settle, coalesce, and context
                                            // kinds are appended without a turn (E2.2, E1.10, E1.18).
                                            b.notice(&key,&id,&text,agent::notice::NoticeKind::classify(&id,&text),Some(permit.clone())).await?;
                                            permit.check()?;Ok::<_,anyhow::Error>(())
                                        }.await;
                                        // Success means the deduplicated wake is DURABLE, not just
                                        // that a channel accepted it. No new epoch is acquired here.
                                        let _=ack.send(result.as_ref().map(|_|()).map_err(|e|format!("{e:#}")));
                                        // A notice too long for the agent is the outbox's to shorten (Actions::drain), not a window problem.
                                        if let Err(e)=result{if permit.check().is_ok()&&e.downcast_ref::<agent::MessageRejected>().is_none(){return Err(e);}}
                                    }
                                    Some(Command::Typed{text,steer,echo})=>{
                                        // The bubble shows already (sent with the window's action); a send that fails marks it not sent.
                                        let sent=async {
                                            validate_message(&text)?;if settings.dry_run{return Err(WindowActionRejected("Agent is off".into()).into());}gate.check()?;
                                            typed_delivery(&tools,&text,true,send_typed_message(&mut b,&text,steer,admission.clone(),Some(&echo))).await
                                        }.await;
                                        if sent.is_err(){if let Some(u)=ui{u.send(typed_echo(&echo,&text,Echo::Failed));}crate::chat_history::typed_failed(&echo);}
                                        sent?;
                                        // Still queued (a turn runs): the queued row under the bubble's ID; else it reached the agent.
                                        if let Some(u)=ui{match b.last_queued()?.filter(|w|w["uuid"].as_str()==Some(echo.as_str())) {
                                            Some(wire)=>{let mut view=agent::view::LiveView::default();u.send(ui::UiUpdate::AgentWindow(ui::AgentWindowUpdate::Entries(view.sent(&wire,agent::now_ms()).into_iter().map(|e|{let mut e=live_entry(e);e.sent=false;e.queued=true;e}).collect())));}
                                            None=>u.send(typed_echo(&echo,&text,Echo::Sent)),
                                        }}
                                    }
                                    Some(Command::Send{text,steer})=>{
                                        validate_message(&text)?;if settings.dry_run{return Err(WindowActionRejected("Agent is off".into()).into());}gate.check()?;
                                        typed_delivery(&tools,&text,true,send_typed_message(&mut b,&text,steer,admission.clone(),None)).await?;if let Some(u)=ui{if let Some(wire)=b.last_queued()?{let mut view=agent::view::LiveView::default();u.send(ui::UiUpdate::AgentWindow(ui::AgentWindowUpdate::Entries(view.sent(&wire,agent::now_ms()).into_iter().map(|e|{let mut e=live_entry(e);e.sent=false;e.queued=true;e}).collect())));}}
                                    }
                                    Some(Command::Dropped{text,images})=>{
                                        validate_message(&text)?;if settings.dry_run{return Err(WindowActionRejected("Agent is off".into()).into());}gate.check()?;
                                        let image_ids:Vec<_>=images.iter().map(|i|b.harness.journal.image(&i.bytes,&i.media_type)).collect::<Result<_>>()?;
                                        b.harness.journal.event("notch_drop",b.harness.source(),&[],capture::host_now_ns(),json!({"image_ids":image_ids}))?;
                                        typed_delivery(&tools,&text,false,send_dropped(&mut b,&text,&images,admission.clone())).await?;
                                        if let Some(u)=ui{if let Some(wire)=b.last_queued()?{let mut view=agent::view::LiveView::default();u.send(ui::UiUpdate::AgentWindow(ui::AgentWindowUpdate::Entries(view.sent(&wire,agent::now_ms()).into_iter().map(|e|{let mut e=live_entry(e);e.sent=false;e.queued=true;e}).collect())));}}
                                    }
                                    Some(Command::EditAndResend{session_entry_id,text})=>{
                                        if matches!(b.status,agent::Status::NeedsReconciliation(_)){return Err(WindowActionRejected("Review the uncertain delivery before editing".into()).into());}
                                        validate_message(&text)?;gate.check()?;anyhow::ensure!(!settings.dry_run,"Agent is off");
                                        let Adapter::Pi(pi)=&mut b.harness.inner else{return Err(WindowActionRejected("Agent is waiting for the engine".into()).into());};
                                        if pi.active() {return Err(WindowActionRejected("Stop the current turn before editing".into()).into());}
                                        let p=admission.as_ref().context("edit permit missing")?;
                                        let through=tools.send_context.lock().unwrap().snapshot().through;
                                        let typed_at=capture::host_now_ns();
                                        let messages=p.run(pi.query(json!({"type":"get_fork_messages"}))).await?;
                                        if !messages["messages"].as_array().is_some_and(|a|a.iter().any(|m|m["entryId"]==session_entry_id&&m["text"].as_str().is_some_and(|t|agent::view::typed_text(t).is_some()))) {return Err(WindowActionRejected("This typed message is no longer on the active branch".into()).into());}
                                        let path=p.run(pi.fork(&session_entry_id)).await.map_err(|e|{
                                            if e.to_string()=="fork cancelled" || e.to_string()=="message is not on the active branch" {WindowActionRejected(e.to_string()).into()}else{e}
                                        })?;
                                        // Save the fork path before sending. If saving fails the
                                        // actor stops transport; restart resumes the old branch.
                                        agent::runtime::atomic_write(&data.join("agent/active-session.json"),&serde_json::to_vec(&path)?)?;
                                        if let Some(u)=ui{u.send(ui::UiUpdate::AgentWindow(settled_history(&path)?));}
                                        tools.speech_policy.owner_woke(&text,typed_at); // pipeline/listen_to.rs
                                        b.send_text(None,&format!("[Heimdall typed message]\n{text}"),false,admission.clone()).await?;
                                        tools.typed_context(&text,through,typed_at)?;
                                    }
                                    Some(Command::Reload{reason,abort,permit})=>{
                                        if permit.as_ref().is_some_and(|p|p.check().is_err()){return Ok(false);}
                                        anyhow::ensure!(!settings.dry_run,"Agent is off");gate.check()?;validate_message(&reason)?;
                                        if abort{if let Adapter::Pi(pi)=&mut b.harness.inner{pi.abort().await?;}drain_ready(&mut b,&tools).await?;}
                                        // Persist the ORIGINAL accepted epoch, never a generation sampled
                                        // after disk I/O or an intervening Off/On.
                                        let reload_permit=permit.or(admission).unwrap_or(gate.permit()?);reload_permit.check()?;
                                        let reason=if let Some((old,_))=&reload{format!("{old}; also requested {reason}")}else{reason};
                                        let reason=tools.persist_reload(reason,&reload_permit)?;
                                        reload=Some((reason,Some(reload_permit)));b.status=agent::Status::ReloadPending;
                                        // Explicit reload is also the retry after a red pill.
                                        budget=agent::supervision::RestartBudget::reset(&data)?;failures=0;
                                    }
                                    Some(Command::ResolveDelivery{id,completed})=>{if !b.inflight()?.contains(&id){return Err(WindowActionRejected("Delivery is no longer waiting for review".into()).into());}b.reconcile(&id,completed)?;},
                                    Some(Command::Cancel)=>{
                                        // The user's Stop (window, island, or an approved spoken stop phrase; E3): audio and the
                                        // speech queue stop, waiting notices become context, queued wakes are not delivered.
                                        tools.interrupt();tools.replies.lock().unwrap().reset();b.stop()?;if let Adapter::Pi(pi)=&mut b.harness.inner{pi.abort().await?;}drain_ready(&mut b,&tools).await?;
                                        // In emergency mode the cover agent is the one acting: it stops too.
                                        if b.holding_turns(){(tools.emergency.relay())(agent::emergency::Relayed{id:format!("stop-{}",agent::now_ms()),kind:"stop".into(),text:"Stop speaking and stop what you are doing; wait for User.".into(),images:vec![]});}
                                        tools.speech_policy.run_settled(); // speech-policy (night 3)
                                    },
                                    #[cfg(test)]
                                    Some(Command::Inspect(done))=>{let snapshot=match &b.harness.inner{Adapter::Pi(pi)=>pi.pid().map(|pid|(pid,pi.session_path().to_owned())),_=>None};let _=done.send(snapshot);},
                                    #[cfg(test)]
                                    Some(Command::InspectSettings(done))=>{let _=done.send(settings.clone());},
                                    Some(Command::Barrier(done))=>{settle_dry(&mut b)?;let _=done.send(());},
                                    Some(Command::RelayState)=>{},
                                    Some(Command::EmergencyRelease{back})=>{
                                        b.hold_turns(None).await?;
                                        b.harness.journal.event("agent_emergency","agent:pi",&[],capture::host_now_ns(),json!({"state":if back.is_some(){"released"}else{"off"}}))?;
                                        if let Some(back)=back{
                                            let key=format!("{}{}",agent::notice::EMERGENCY_BACK,agent::now_ms());
                                            b.notice(&key,"while you were down",&back,agent::notice::NoticeKind::Scheduled,Some(gate.permit()?)).await?;
                                        }
                                    }
                                    Some(Command::Thinking(s))=>{
                                        let level=agent_thinking(&s,agent::runtime::Runtime::installed().ok().as_ref().map(|r|r.root.as_path()));
                                        settings=s;
                                        if let Adapter::Pi(pi)=&mut b.harness.inner {
                                            let now=pi.set_thinking(level).await?;
                                            b.harness.journal.event("agent_thinking","agent:pi",&[],capture::host_now_ns(),json!({"requested":level,"running":now}))?;
                                        }
                                    }
                                    // A model switch (or a new key) never cuts a running compaction (the user: "compaction must never be
                                    // interrupted"; 27 Oct 03:24 a switch threw away a 24 s summary): the restart waits for it,
                                    // as Agent off does, and input meanwhile follows the restart (night audit H4).
                                    // Agent off: nothing starts now, but the switch is kept, and the next start (Agent on) runs it.
                                    // A switch parked behind a compaction when Agent went off arrives here after the wait; it
                                    // was refused by its permit and lost (review 27 Oct).
                                    Some(Command::Restart(s)) if !s.dry_run && !settings.dry_run && !gate.load(Ordering::Acquire) => {
                                        b.harness.journal.event("agent_restart","agent:pi",&[],capture::host_now_ns(),json!({"state":"saved","for":"next_start"}))?;
                                        settings=s;reload=None;retry=None;
                                    }
                                    Some(Command::Restart(s)) if output_open && crucial_work(&b,&gate,std::time::Duration::ZERO)==Some(true) => {
                                        b.harness.journal.event("agent_restart_wait","agent:pi",&[],capture::host_now_ns(),json!({"state":"waiting","for":"compaction"}))?;
                                        stopping=Some(Stopping{since:stop_since(&data),restart:Some(s),held:vec![],for_restart:true});retry=None;
                                    }
                                    Some(Command::Restart(s))=>{
                                        let startup_permit=if s.dry_run { None } else { Some(gate.permit()?) };
                                        if let Some(server)=server.take(){server.shutdown().await;}let _=b.shutdown().await;
                                        generation=gate.generation();reload=None;retry=None;failures=0;
                                        settings=s;
                                        tools.emergency.configure(relay_options(&settings));
                                        tools.emergency.model(settings.selected_api_model().cloned());detector=Default::default();
                                        if settings.dry_run{start=None;carried=None;}
                                        budget=agent::supervision::RestartBudget::reset(&data)?;
                                        if !settings.dry_run{server=Some(agent::mcp::McpServer::start(tools.clone(),Arc::new(ArchiveSearch{root:data.join("archive"),embeddings:tools.embeddings.clone()}),gate.clone()).await?);}
                                        provider_try=None;
                                        b=match open(&settings,&data,&session,server.as_ref(),ui,&tools.api,gate.clone(),tools.engine_signal.clone(),None,startup_permit).await{Ok(b)=>b,Err(e)=>{let mut b=unavailable(&settings,&data,&session,e.to_string())?;if let Some(waiting)=e.downcast_ref::<agent::harness::ProviderUnavailable>(){if !matches!(b.status,agent::Status::NeedsReconciliation(_)){b.status=agent::Status::WaitingProvider(waiting.0.clone());}}else if e.downcast_ref::<agent::harness::EngineUnavailable>().is_some(){if !matches!(b.status,agent::Status::NeedsReconciliation(_)){b.status=agent::Status::WaitingEngine;}}else{failures=1;retry=budget.take(&data,agent::now_ms())?.map(|d|tokio::time::Instant::now()+d);}b}};
                                        output_open=!matches!(b.harness.inner,Adapter::Unavailable(_));
                                        started!();
                                    }
                                    Some(Command::Scoped{..})=>unreachable!("nested action scope"),
                                    Some(Command::Shutdown)|None=>{watch_stop.send_replace(true);if let Some(watcher)=watcher.take(){let _=watcher.await;}if let Some(h)=heartbeat.take(){let _=h.await;}tools.watcher_running.store(false,Ordering::Release);if let Some(server)=server.take(){server.shutdown().await;}let _=b.shutdown().await;return Ok::<bool,anyhow::Error>(true);},
                                }},
                                _=tokio::time::sleep_until(retry.unwrap_or_else(tokio::time::Instant::now)),if retry.is_some()&&gate.load(Ordering::Acquire)&&stopping.is_none()=>{
                                    retry=None;let _=b.shutdown().await;
                                    // An Agent-off wait for crucial work shut the MCP server down; the next start needs one.
                                    if server.is_none()&&!settings.dry_run{server=Some(agent::mcp::McpServer::start(tools.clone(),Arc::new(ArchiveSearch{root:data.join("archive"),embeddings:tools.embeddings.clone()}),gate.clone()).await?);}
                                    b=open(&settings,&data,&session,server.as_ref(),ui,&tools.api,gate.clone(),tools.engine_signal.clone(),None,None).await?;output_open=true;context_check=start_check&&!compaction_hold.held();
                                    // The restart worked: "could not recover" is the budget's to say, when it refuses a restart,
                                    // never a count of every failure since launch (night audit H3: Failed while restarts ran).
                                    failures=0;
                                    b.engine_ready();started!();
                                }
                                event=b.next(),if output_open=>{
                                    let Some(event)=event? else{output_open=false;anyhow::bail!("Pi exited");};
                                    crate::latency::agent_event(&event); // Latency map (lab/latency).
                                    if !settings.dry_run { if let Some(cause)=crate::emergency::signal(&mut detector,&event,&tools.emergency) { emergency!(cause); } }
                                    if let Event::Other(value)=&event {
                                        if let Some(model)=settings.selected_api_model() {
                                            // The extension names key, size and usage-limit failures; a later answer clears them.
                                            match value["type"].as_str() {
                                                // While the relay answers (relay or emergency mode), Pi is only the record: its own calls (a held
                                                // compaction) say nothing to the user about a provider he is not using (28 Oct 17:27:55: the relay's
                                                // compaction on Opus by API key put "Anthropic credits used up" in the menu while Claude ran in T3).
                                                Some("heimdall_provider_problem") if crate::emergency::reports_provider_problems(&tools.emergency) => tools.api.runtime_problem(&model.provider, value["message"].as_str().map(str::to_owned)),
                                                // Prepaid providers: each reply's and summary's tokens join the spend estimate.
                                                Some("message_end") if value["message"]["role"]=="assistant" && value["message"]["provider"].as_str()==Some(model.provider.as_str()) => {
                                                    if let Some(u)=agent::provider::Usage::from_pi(&value["message"]["usage"]) { tools.api.record_usage(model,&u,agent::now_ms()); }
                                                    if value["message"]["stopReason"]!="error" { tools.api.runtime_problem(&model.provider, None); if value["message"]["stopReason"]!="aborted" { crate::background_gate::quota_back(&model.provider); } }
                                                    else if value["message"]["errorMessage"].as_str().is_some_and(|e| e.contains("--provider-token")) { tools.api.recheck(); }
                                                }
                                                Some("compaction_end") => if let Some(u)=agent::provider::Usage::from_pi(&value["result"]["usage"]) { tools.api.record_usage(model,&u,agent::now_ms()); },
                                                Some("message_end") if value["message"]["role"]=="assistant" && value["message"]["stopReason"]!="error" => { tools.api.runtime_problem(&model.provider, None); if value["message"]["stopReason"]!="aborted" { crate::background_gate::quota_back(&model.provider); } },
                                                // The token command failed: re-check the sign-in, which tells offline from signed out.
                                                Some("message_end") if value["message"]["errorMessage"].as_str().is_some_and(|e| e.contains("--provider-token")) => tools.api.recheck(),
                                                _ => {}
                                            }
                                        }
                                        // Audit 27 Oct, F6: a carried history is summarised; Pi goes back to this budget's threshold.
                                        if let Some(policy)=carry_ended(b.compaction_policy(),value) {
                                            b.set_compaction_policy(Some(policy));
                                            match tools.agent_enabled.permit() {
                                                Ok(permit)=>if let Err(e)=tools.reload(CARRY_RELOAD,&permit) {eprintln!("Agent: reload after the carried history not queued: {e:#}");},
                                                // Agent off: its next start reads the compacted history and carries nothing.
                                                Err(_)=>{}
                                            }
                                        }
                                        // A spent plan limit or the five-hour window past 80% (runtime/quota.mjs): logged, said once.
                                        if value["type"]=="heimdall_quota" {tools.quota_notice(value);}
                                        // A reply that ends while Agent off waits is not shown or said: Off stopped the turn.
                                        if value["type"]=="heimdall_final_answer" && stopping.is_none() {tools.agent_final_answer(value["text"].as_str().context("final answer text missing")?,value["stop_reason"].as_str().unwrap_or_default())?;}
                                    }
                                    if let Event::Complete {query:true,..}=&event {tools.agent_working(false)?;tools.replies.lock().unwrap().settled();}
                                    // speech-policy (night 3): only the last completion of a settled run ends the user's address; a turn his name-wake
                                    // restarted (night pass H2) is still running when the aborted turn's completions arrive.
                                    if matches!(&event,Event::Complete {query:true,..}) && !b.turn_running() {tools.speech_policy.run_settled();}
                                    // A reply succeeded: the provider works again, so the history may be checked again.
                                    if matches!(&event,Event::Complete{query:true,error:false,..}) && compaction_hold.reply_succeeded() {context_check=true;}
                                    // Pi completed a compaction: summaries work again, a kill's backoff is over.
                                    if let Event::Other(v)=&event { if v["type"]=="compaction_end" && v["aborted"]!=true && v["errorMessage"].is_null() && !v["result"].is_null() {compaction_hold.compacted();fallback.compacted(&mut b,&tools)?;} }
                                    if matches!(&event,Event::Complete{query:true,error:false,..}) {health.turn_succeeded();if let Some(u)=ui{u.send(ui::UiUpdate::Health(ui::HealthCue::Clear{key:RETRY_KEY.into()}));}}
                                    // A conversation the model refused, repaired at start or in the run (`agent::history`).
                                    if let Event::Other(v)=&event { if v["type"]=="heimdall_history_repair" {
                                        b.harness.journal.event("agent_history_repair","agent:pi",&[],capture::host_now_ns(),json!({"state":v["state"],"line":v["line"]}))?;
                                        if let Some(u)=ui { history_repair_cue(u,v["state"].as_str().unwrap_or_default()); }
                                    }}
                                    if let Event::Other(v)=&event { if v["type"]=="heimdall_compaction_failed" {
                                        compaction_hold.failed();
                                        b.harness.journal.event("agent_compaction","agent:pi",&[],capture::host_now_ns(),json!({"state":"idle_failed","error":v["error"]}))?;
                                        crate::diagnostics::record(&diagnostics,"agent_idle_compaction_failed","agent:pi",json!({"error":v["error"]}));
                                        if !settings.dry_run {fallback.failed(&mut b,&tools,&settings,v["error"].as_str().unwrap_or_default())?;}
                                    }}
                                    // Queued wakes are released below, after settlement maintenance.
                                    if let Event::Other(v)=&event{
                                        if v["type"]=="heimdall_loop_stopped" && gate.load(Ordering::Acquire)
                                            && v["delivery_epoch"].as_u64()==Some(gate.generation()) {
                                            if let Some(u)=ui { u.send(ui::UiUpdate::Health(ui::HealthCue::Notice {
                                                part:ui::Part::Agent,label:agent::harness::loop_policy().notice.clone(),
                                            })); }
                                        }
                                        if (v["type"]=="agent_settled" || v["type"]=="heimdall_settlement_recovered") && !compaction_hold.held() {context_check=true;}
                                        if v["type"]=="agent_settled" || v["type"]=="heimdall_settlement_recovered"{if let Adapter::Pi(pi)=&mut b.harness.inner{
                                            // Persisted ids differ from streaming ids. Replace the active
                                            // branch at settlement instead of appending a second copy of
                                            // every prompt/reply/tool row. Bound reads as at startup.
                                            if !pi.active() && pi.session_path().metadata().is_ok_and(|m|m.len()<agent::session::ROTATE_BYTES){if let Some(u)=ui{u.send(ui::UiUpdate::AgentWindow(settled_history(pi.session_path())?));}}
                                        }}
                                    }
                                }
                                // A notice waiting out the 30-second gap between notice-only turns (E1.18).
                                _=tokio::time::sleep(std::time::Duration::from_millis(notice_due.unwrap_or(0).saturating_sub(agent::now_ms()))),if notice_due.is_some()=>{},
                                _=tokio::time::sleep(std::time::Duration::from_millis(retry_due.unwrap_or(0).saturating_sub(agent::now_ms()))),if retry_due.is_some()=>{},
                                // The fallback's summary finished: Pi takes it at the next idle moment (below).
                                out=async{fallback.task.as_mut().unwrap().await},if fallback.task.is_some()=>{
                                    fallback.task=None;
                                    fallback.ready=Some(out.unwrap_or_else(|e|compaction_fallback::Outcome::Failed{tried:vec![format!("the summary stopped: {e}")]}));
                                }
                                _=tick.tick(),if !settings.dry_run&&gate.load(Ordering::Acquire)&&stopping.is_none()=>{
                                    if let Some(cause)=detector.tick(b.awaiting_answer()&&!b.holding_turns(),agent::now_ms()) { emergency!(cause); }
                                    // The controller ended (a panic, a closed inbox) while the agent is on: a new one.
                                    if emergency_task.is_finished(){eprintln!("emergency: the controller had stopped; started again");emergency_task=emergency_run();}
                                    // A hold the relay no longer has (it ended while this Bridge was being held) lifts here, and a
                                    // relay acting without one (relay mode chosen during a hand-back, a quick Agent off and on) gets it.
                                    if b.holding_turns()&&tools.emergency.phase()==crate::emergency::Phase::Off{b.hold_turns(None).await?;}
                                    else if !b.holding_turns()&&tools.emergency.holding(){b.hold_turns(Some(tools.emergency.relay())).await?;}
                                    b.set_hold_compacts(tools.emergency.reason()==agent::emergency::Reason::Chosen);
                                    let readiness_permit=gate.permit()?;
                                    if settings.selected_api_model().is_some() && fixture_executable().is_none() {
                                        // An unreachable provider is retried once a minute; a missing or rejected key waits
                                        // for Connect (ProviderKeyChanged), never in a loop.
                                        let unreachable=settings.selected_api_model().and_then(|m|m.provider_info()).map(agent::provider::unreachable_message);
                                        if matches!(b.harness.inner,Adapter::Unavailable(_)) && retry.is_none() && failures<4
                                            && matches!(&b.status,agent::Status::WaitingProvider(m) if Some(m)==unreachable.as_ref())
                                            && provider_try.is_none_or(|t:tokio::time::Instant|t.elapsed()>=std::time::Duration::from_secs(60)) {
                                            provider_try=Some(tokio::time::Instant::now());
                                            match open(&settings,&data,&session,server.as_ref(),ui,&tools.api,gate.clone(),tools.engine_signal.clone(),None,Some(readiness_permit.clone())).await {
                                                Ok(next)=>{b=next;output_open=true;context_check=start_check&&!compaction_hold.held();b.engine_ready();started!();}
                                                Err(e)=>match e.downcast::<agent::harness::ProviderUnavailable>() {
                                                    Ok(waiting)=>{if !matches!(b.status,agent::Status::NeedsReconciliation(_)){b.status=agent::Status::WaitingProvider(waiting.0);}}
                                                    Err(e)=>return Err(e),
                                                },
                                            }
                                        } else if !matches!(b.harness.inner,Adapter::Unavailable(_)) { b.engine_ready(); }
                                    } else if fixture_executable().is_some()||readiness_permit.run(async { Ok(engine_available(&tools.engine_signal,&engine).await) }).await?{
                                        if matches!(b.harness.inner,Adapter::Unavailable(_))&&matches!(b.status,agent::Status::WaitingEngine)&&retry.is_none()&&failures<4{
                                            b=open(&settings,&data,&session,server.as_ref(),ui,&tools.api,gate.clone(),tools.engine_signal.clone(),None,Some(readiness_permit.clone())).await?;output_open=true;context_check=start_check&&!compaction_hold.held();
                                            started!();
                                        }
                                        b.engine_ready();
                                    }else if !matches!(b.status,agent::Status::NeedsReconciliation(_)|agent::Status::AuthBlocked(_)){b.status=agent::Status::WaitingEngine;}
                                }
                            }
                            tools.agent_working(matches!(b.status,agent::Status::Working)||crate::emergency::RELAY_WORKING.load(Ordering::Acquire))?;
                            // Housekeeping: old payloads move into the archive. One the archive cannot take now (another
                            // process holds it, the disk is full) stays for the next pass; never a restart (1 Nov 00:16:44).
                            if let Err(e)=b.retire(&payload_archive).and_then(|_|workers.retire()).and_then(|_|workers.actions.retire()){
                                eprintln!("Agent: old payloads kept for the next pass: {e:#}");
                            }
                            settle_dry(&mut b)?;
                            // Nothing new reaches a Pi that only finishes crucial work before it stops.
                            let live=gate.load(Ordering::Acquire)&&stopping.is_none();
                            if reload.is_none()&&!settings.dry_run&&live{
                                deliver_start(&mut b,&mut start,&mut carried,&settings,compaction_hold.held(),&data,&gate).await?;
                                if !stale_receipts.is_empty()&&matches!(b.harness.inner,Adapter::Pi(_)){stale_receipts=complete_receipts(&data,&gate,std::mem::take(&mut stale_receipts));}
                            }
                            if reload.is_none()&&!settings.dry_run&&live{
                                // Not while compactions are held: rotation compacts first, and a held one would loop (H1).
                                if let Adapter::Pi(pi)=&mut b.harness.inner{if !pi.active()&&!compaction_hold.held()&&pi.session_path().metadata().is_ok_and(|m|m.len()>=agent::session::ROTATE_BYTES){
                                    reload=Some(("Session storage maintenance".into(),None));b.status=agent::Status::ReloadPending;
                                    pi.compact().await?;
                                }}
                            }
                            if reload.is_none()&&!settings.dry_run&&live{
                                if let Err(e)=fallback.step(&mut b,&settings,&data,&tools,&engine).await {eprintln!("Agent: compaction fallback: {e:#}");}
                            }
                            if reload.is_none()&&!settings.dry_run&&live&&context_check&&!compaction_hold.held(){
                                if let Err(e)=maintain_idle(&mut b).await {
                                    // Optional work: logged and held, never a restart.
                                    compaction_hold.failed();
                                    crate::diagnostics::record(&diagnostics,"agent_idle_compaction_failed","agent:pi",json!({"error":format!("{e:#}")}));
                                }
                            }
                            if reload.is_none()&&!settings.dry_run&&live{
                                if let Err(e)=ping_idle(&mut b,&gate).await {
                                    crate::diagnostics::record(&diagnostics,"agent_cache_ping_failed","agent:pi",json!({"error":format!("{e:#}")}));
                                }
                            }
                            if reload.is_none() && live { b.flush_queued().await?; }
                            // Reload waits for true settlement, not agent_end. No new wake
                            // crosses while a restart is pending and the old process exists.
                            let idle=match &b.harness.inner{Adapter::Pi(pi)=>!pi.active(),_=>true};
                            if idle&&live{if let Some((reason,permit))=reload.take(){
                                if permit.as_ref().is_some_and(|p|p.check().is_err()){return Ok(false);}
                                let restart_permit=permit.clone().unwrap_or(gate.permit()?);restart_permit.check()?;
                                let reason=tools.persist_reload(reason,&restart_permit)?;
                                // The reason is the line before the restart message (E4.1).
                                let note=reload_line(&reason);
                                b.harness.journal.event("agent_reload","agent:pi",&[],capture::host_now_ns(),json!({"reason":reason,"note":note}))?;
                                if let Adapter::Pi(pi)=&mut b.harness.inner{
                                    let archive=session.clone();pi.stop_and_rotate(move|bytes|heimdall_archive::store::archive_log(&archive,&format!("pi-session-{}.jsonl",heimdall_archive::hash(bytes)),bytes)).await?;
                                }else{let _=b.shutdown().await;}
                                restart_permit.check()?;
                                let replacement_permit=restart_permit.for_replacement()?;
                                tools.clear_questions();
                                if let Some(old)=server.take(){old.shutdown().await;}
                                server=Some(agent::mcp::McpServer::start(tools.clone(),Arc::new(ArchiveSearch{root:data.join("archive"),embeddings:tools.embeddings.clone()}),gate.clone()).await?);
                                b=open(&settings,&data,&session,server.as_ref(),ui,&tools.api,gate.clone(),tools.engine_signal.clone(),Some(note),Some(replacement_permit)).await?;output_open=true;context_check=start_check&&!compaction_hold.held();
                                b.engine_ready();started!();deliver_start(&mut b,&mut start,&mut carried,&settings,compaction_hold.held(),&data,&gate).await?;b.flush_queued().await?;
                            }}
                            Ok::<bool,anyhow::Error>(false)
                        }.await;
                        match operation{
                            Ok(true)=>break,Ok(false)=>{},
                            Err(e)=>{
                                if e.downcast_ref::<agent::MessageRejected>().is_some() || e.downcast_ref::<WindowActionRejected>().is_some(){
                                    crate::diagnostics::record(&diagnostics, "health", "agent:pi", json!({"component":"agent-command", "state":"rejected", "reason":format!("{e:#}")}));
                                    if let Some(u)=ui{window_problem(u,e.to_string());}
                                    continue;
                                }

                                // Pi died while it compacted (the harness's silence limit, a crash): that compaction failed.
                                compaction_killed!(format!("{e:#}"));
                                detector.new_process();
                                // A failure during the wait ends it, and Pi stops now: the stop itself, never a crash to
                                // recover from (a restart Agent on asked for is already queued).
                                let stopped=stopping.is_some();
                                finish_stop!(Some(format!("{e:#}")));
                                let off=stopped||e.downcast_ref::<agent::delivery::DeliveryBlocked>().is_some()||!gate.load(Ordering::Acquire)||gate.generation()!=generation;
                                let waiting=e.downcast_ref::<agent::harness::EngineUnavailable>().is_some();
                                let provider=e.downcast_ref::<agent::harness::ProviderUnavailable>().map(|w|w.0.clone());
                                eprintln!("Agent supervisor: {e:#}");
                                crate::diagnostics::record(&diagnostics, "health", "agent:pi", json!({"component":"agent-supervisor", "state":if off{"off"}else if waiting{"waiting-engine"}else{"failed"}, "reason":format!("{e:#}")}));
                                if off{let _=b.shutdown().await;output_open=false;retry=None;}
                                else if let Some(reason)=provider{
                                    // A reopen found no usable key: wait for Connect, without the restart budget.
                                    let before=b.status.clone();let _=b.shutdown().await;output_open=false;retry=None;
                                    b.status=if matches!(before,agent::Status::NeedsReconciliation(_)){before}else{agent::Status::WaitingProvider(reason)};
                                }
                                else if waiting{if !matches!(b.status,agent::Status::NeedsReconciliation(_)){b.status=agent::Status::WaitingEngine;}}
                                else{
                                    let before=b.status.clone();let _=b.shutdown().await;b.status=before;output_open=false;
                                    // b.next marks all uncertain deliveries before this recovery.
                                    if !matches!(b.status,agent::Status::NeedsReconciliation(_)){b.status=agent::Status::AuthBlocked("Agent recovery in progress".into());}
                                    failures+=1;retry=budget.take(&data,budget_now(&data))?.map(|d|tokio::time::Instant::now()+d);if retry.is_none(){failures=4;}
                                }
                            }
                        }
                        report_resolutions(&mut b,&diagnostics,ui,&mut retry_cue,gate.load(Ordering::Acquire));
                        health.restarting=retry.is_some();if let Some(u)=ui{health.update(&b.status,failures,&settings,&diagnostics,u);}
                        // "Stopping after compaction…" while Agent is off and waits; Agent back on shows the work itself.
                        let shown=if stopping.is_some()&&!gate.load(Ordering::Acquire){crucial_work(&b,&gate,std::time::Duration::ZERO)}else{None};
                        publish_agent_state(&b,&diagnostics,&settings,failures,reload.as_ref().map(|(reason,_)|reason.as_str()),shown,ui);
                    }Ok::<_,anyhow::Error>(())
                }.await;
                watch_stop.send_replace(true);
                if let Some(watcher)=watcher.take(){let _ = watcher.await;}
                if let Some(h)=heartbeat.take(){let _ = h.await;}
                tools.watcher_running.store(false, Ordering::Release);
                connections.abort_all();
                while connections.join_next().await.is_some() {}
                tools.clear_questions();
                tools.mcp_actions.detach();
                tools.agent_commands.lock().unwrap().take();
                if let Err(e)=result{eprintln!("Agent supervisor stopped: {e:#}");
                    tools.record_diagnostic("health",json!({"component":"agent-supervisor", "state":"stopped", "reason":format!("{e:#}")}));
                    if let Some(u)=ui{
                    u.send(ui::UiUpdate::Health(ui::HealthCue::Problem{key:"harness".into(),part:ui::Part::Agent,label:"The agent could not recover".into(),action:"Check free disk space, then reload it in Open Agent".into(),control:ui::HealthControl::OpenAgent}));
                    u.send(ui::UiUpdate::Agent(ui::AgentState{harness:ui::Harness::Pi,session:ui::SessionState::Failed("The agent could not recover".into())}));
                }}
            });
        })?;
        Ok(Self {
            gate: runtime_gate,
            tx,
            thread: Some(thread),
        })
    }
    pub fn is_running(&self) -> bool {
        !self.tx.is_closed()
    }
    pub fn send(&self, c: Command) -> Result<()> {
        let c = if matches!(&c,Command::Wake{..}|Command::WakePending{..}|Command::LateImages{..}|Command::Send{..}|Command::Typed{..}|Command::Dropped{..}|Command::EditAndResend{..}|Command::Reload{permit:None,..}|Command::ResolveDelivery{..}|Command::Cancel) {
            Command::Scoped{command:Box::new(c),permit:self.gate.permit()?}
        } else { c };
        self.tx.send(c).map_err(|_| anyhow::anyhow!("Agent supervisor stopped"))
    }
    pub fn wake_permit(&self, dry_run: bool) -> Result<agent::delivery::DeliveryPermit> {
        // A local simulation has no authority on the live gate, even when the
        // numeric epochs coincide. Actor and transport reject that gate on Pi.
        if dry_run { Arc::new(agent::delivery::DeliveryGate::new(true)).permit() }
        else { self.gate.permit() }
    }
    pub fn wake_pending_with_permit(&self,images:Vec<Attachment>,urgent:bool,through:u64,permit:agent::delivery::DeliveryPermit)->Result<()> {
        self.wake_pending_through_with_permit(images,urgent,through,None,agent::Speaker::Owner,permit)
    }
    pub fn wake_pending_through_with_permit(&self,images:Vec<Attachment>,urgent:bool,through:u64,observation:Option<String>,speaker:agent::Speaker,permit:agent::delivery::DeliveryPermit)->Result<()> {
        permit.check()?;
        self.send(Command::Scoped {command:Box::new(Command::WakePending {images,urgent,through,observation,speaker}),permit})
    }
    pub fn late_images_with_permit(&self, caption: String, images: Vec<Attachment>, permit: agent::delivery::DeliveryPermit) -> Result<()> {
        permit.check()?;
        self.send(Command::Scoped { command: Box::new(Command::LateImages { caption, images }), permit })
    }
    pub fn wake_with_permit(&self, images: Vec<Attachment>, urgent: bool, permit: agent::delivery::DeliveryPermit) -> Result<()> {
        permit.check()?;
        self.send(Command::Scoped { command: Box::new(Command::Wake { images, urgent }), permit })
    }
    pub fn notice(
        &self,
        id: impl Into<String>,
        text: impl Into<String>,
        permit: &agent::delivery::DeliveryPermit,
    ) -> Result<()> {
        notice_to(&self.tx, id.into(), text.into(), permit)
    }
    pub fn reload(&self, reason: impl Into<String>) -> Result<()> {
        self.send(Command::Reload {
            reason: reason.into(),
            abort: false,
            permit: None,
        })
    }
    pub fn notice_action(&self, action: ui::agent_notice::AgentNoticeAction) -> Result<()> { self.send(Command::NoticeAction {action,permit:self.gate.permit()?}) }
    pub fn window_action(&self, action: ui::AgentWindowAction) -> Result<()> {
        self.send(window_command(action))
    }

    #[cfg(test)]
    pub(super) fn inspect(&self) -> Result<Option<(u32, PathBuf)>> {
        let (t, r) = oneshot::channel();
        self.send(Command::Inspect(t))?;
        Ok(r.blocking_recv()?)
    }
    pub fn barrier(&self) -> Result<()> {
        let (t, r) = oneshot::channel();
        self.send(Command::Barrier(t))?;
        r.blocking_recv()?;
        Ok(())
    }
}
impl Drop for AgentRuntime {
    fn drop(&mut self) {
        let _ = self.send(Command::Shutdown);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
/// Agent off while crucial work runs ("When the user turns the Agent toggle off, Heimdall must first
/// let crucial in-flight work finish, then stop. Crucial work means a running compaction, plus any tool call that is
/// writing state … Anything else stops at once"; 22 Oct 16:02: he cut two compactions and paid for both). The Off
/// itself is immediate and durable as before: speech, questions, wakes, reads, T3 calls and every new delivery stop
/// with the epoch. Only Pi and the MCP server stay up, until the compaction or state write ends (Pi's own stop
/// watcher holds its kill the same way), never longer than `COMPACTION_WAIT` / `WRITE_WAIT`; the menu's Agent
/// caption says "Stopping after compaction…" (or "…saving…") meanwhile. A Restart and any input sent meanwhile wait
/// in `restart` / `held` and follow the stop.
/// A Restart (model switch, new key) waits the same way for a running compaction (`for_restart`): Pi stays up with its
/// MCP server, and the restart follows the compaction's end or its cap.
pub(super) struct Stopping { pub(super) since:tokio::time::Instant, pub(super) restart:Option<Settings>, pub(super) held:Vec<Command>, pub(super) for_restart:bool }
/// What Agent off still waits for after `waited`: `Some(true)` a running compaction, `Some(false)` a state-writing
/// tool call, `None` nothing (or past its cap).
/// A compaction's limit is the harness's own (`PiConfig::compaction_inactivity`, measured from Pi's last output), not
/// the wait's clock: a model switch that arrived mid-compaction cut it 15 minutes after the switch, uncounted, while
/// the harness would still let it run (review 27 Oct). Past that limit the harness ends Pi itself.
pub(super) fn crucial_work(b:&agent::Bridge<RecordedAdapter>,gate:&agent::delivery::DeliveryGate,waited:std::time::Duration)->Option<bool>{
    let compaction=matches!(&b.harness.inner,Adapter::Pi(pi) if pi.compaction_in_progress()&&pi.silent_for()<pi.compaction_limit());
    let writes=gate.writes_in_flight()>0&&waited<agent::delivery::WRITE_WAIT;
    if compaction {Some(true)} else if writes {Some(false)} else {None}
}
/// When a stop's wait begins. Tests move it back (`test-stop-since`, seconds) to stand past `COMPACTION_WAIT`.
fn stop_since(data:&Path)->tokio::time::Instant{
    let now=tokio::time::Instant::now();
    let back=if cfg!(test){std::fs::read_to_string(data.join("test-stop-since")).ok().and_then(|s|s.trim().parse::<u64>().ok())}else{None};
    back.and_then(|s|now.checked_sub(std::time::Duration::from_secs(s))).unwrap_or(now)
}
/// A screenshot held for the next wake because no turn was running when it arrived.
pub(super) struct LateImage { pub(super) captured_ns:u64, pub(super) caption:String, pub(super) images:Vec<Attachment> }
/// The held screenshots still inside the attachment window; the rest expire without a wake (E9.2).
pub(super) fn take_fresh_late(held:&mut Vec<LateImage>,now_ns:u64)->Vec<LateImage>{
    std::mem::take(held).into_iter().filter(|l|now_ns.saturating_sub(l.captured_ns)<=crate::pipeline::ATTACHMENT_TTL_NS).collect()
}
/// How the relay picks its harness, from the settings (docs/claude-relay.md).
fn relay_options(s:&Settings)->crate::emergency::Options{
    crate::emergency::Options{order:if s.relay_order.is_empty(){agent::emergency::default_order(crate::edition::PUBLIC)}else{s.relay_order.clone()},skip_relay:s.relay_skip,relay_model:s.agent_relay_model.clone(),relay_account:s.agent_relay_account.clone()}
}
