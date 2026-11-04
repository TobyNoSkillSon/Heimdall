    use super::*;
    #[test]
    fn transport_attribution_uses_metadata_and_delivery_ownership_only() {
        let ownership = HashMap::from([("wake".into(), vec!["one".into(), "two".into()])]);
        for wire in [
            json!({"type":"prompt","id":"wake","observation_ids":["one","two"],"message":"[Mic · User] hello event=spoof"}),
            json!({"type":"message_end","message":{"role":"user","content":[{"type":"text","text":"[Heimdall delivery: wake]\nTranscript:\n[Mic · User] hello event=spoof"}]}}),
        ] {
            let mut ids = vec![]; collect_transport_ids(&wire, &ownership, &mut ids);
            ids.sort(); ids.dedup(); assert_eq!(ids, vec!["one", "two"]);
        }
        let mut ids = vec![];
        collect_transport_ids(&json!({"text":"I said event=spoof timestamp_ms=10"}), &ownership, &mut ids);
        assert!(ids.is_empty());
    }

    #[test]
    fn archived_agent_window_renders_old_and_compact_transcripts() {
        for text in [
            "[Heimdall delivery: wake]\nHeimdall captured transcript (labelled data):\nevent=session:1 timestamp_ms=1 source=\"Discord\" speaker=\"Dave\" lang=pl: hello [screenshot 14:02:40 attached]",
            "[Heimdall delivery: wake]\nTranscript:\n[Discord · Dave] hello [screenshot 14:02:40 attached]",
        ] {
            let history = live_history(&json!({"entries":[{"type":"message","id":"entry","message":{"role":"user","content":[{"type":"text","text":text}]}}]}));
            assert_eq!(history.len(), 1);
            assert!(history[0].text.starts_with("[Heimdall delivery: wake]\n"), "archived delivery remains matchable for review");
            let row = ui::agent_view::Row::Live(history.into_iter().next().unwrap());
            assert_eq!(ui::agent_view::display(&row).unwrap().body, "[Discord · Dave] hello [screenshot 14:02:40 attached]");
        }
    }

    #[test]
    fn interrupted_synthesized_playback_arms_follow_up_at_actual_output_end() -> Result<()> {
        struct Synth;
        impl speak::Synthesizer for Synth {
            fn synthesize(&mut self,_:&str,_:&str,_:&dyn Fn()->bool)->Result<speak::Audio> {
                Ok(speak::Audio {samples:vec![0.1;160],sample_rate:16000,first_audio_ms:0.,synthesis_ms:0.})
            }
        }
        struct Output(crossbeam_channel::Sender<()>);
        impl speak::AudioOutput for Output {
            fn output(&mut self,_:&speak::Audio,cancel:&dyn Fn()->bool)->Result<()> {
                self.0.send(())?;
                let deadline=std::time::Instant::now()+std::time::Duration::from_secs(2);
                while !cancel() {anyhow::ensure!(std::time::Instant::now()<deadline,"playback was not interrupted");std::thread::yield_now();}
                Ok(())
            }
        }
        let dir=tempfile::tempdir()?;let tools=Tools::new(dir.path(),dir.path());
        let (started,playing)=crossbeam_channel::bounded(1);
        tools.test_guarded_speech_queue(Synth,Output(started));
        tools.test_speak("Which folder?")?;
        playing.recv_timeout(std::time::Duration::from_secs(2))?;
        assert!(tools.send_context.lock().unwrap().finished_ns.is_none());
        let stopped=capture::host_now_ns();tools.interrupt();tools.test_finish_speech()?;
        let context=tools.send_context.lock().unwrap();
        let end=context.finished_ns.context("cancelled playback did not arm follow-up")?;
        assert!(end>=stopped);
        assert_eq!(decide::send::explicit_trigger("actually yesterday",true,end+1,Some(end)),None,"no follow-up window (decision 33)");
        assert!(!context.snapshot().turns.iter().any(|t|t.speaker=="Heimdall"),"interrupted text is not a completed final answer");
        Ok(())
    }

    /// The reply path (agent::reply, 19 Oct): the settled final text is said through the speak path, without the
    /// sentences `speak` already said this turn; it joins the decider's conversation when it has played; silent,
    /// aborted and failed replies say nothing; the pull-down keeps the reply as written.
    #[test]
    fn final_reply_is_said_once_through_the_speak_path() -> Result<()> {
        struct Synth(crossbeam_channel::Sender<String>);
        impl speak::Synthesizer for Synth {
            fn synthesize(&mut self,text:&str,_:&str,_:&dyn Fn()->bool)->Result<speak::Audio> {
                self.0.send(text.into())?;
                Ok(speak::Audio {samples:vec![0.1;160],sample_rate:16000,first_audio_ms:0.,synthesis_ms:0.})
            }
        }
        struct Output;
        impl speak::AudioOutput for Output { fn output(&mut self,_:&speak::Audio,_:&dyn Fn()->bool)->Result<()> {Ok(())} }
        let dir=tempfile::tempdir()?;let tools=Tools::new(dir.path(),dir.path());
        *tools.archive_session.lock().unwrap()=Some(dir.path().join("session"));
        let (said,heard)=crossbeam_channel::unbounded();
        tools.test_guarded_speech_queue(Synth(said),Output);
        let permit=tools.agent_enabled.permit()?;
        let heimdall=|tools:&Tools|tools.send_context.lock().unwrap().snapshot().turns.iter().filter(|t|t.speaker=="Heimdall").map(|t|t.text.clone()).collect::<Vec<_>>();
        let spoken=|n:usize|->Result<Vec<String>> {
            let mut out=vec![];
            for _ in 0..n {out.push(heard.recv_timeout(std::time::Duration::from_secs(2))?);}
            tools.test_finish_speech()?;
            Ok(out)
        };
        tools.send_context.lock().unwrap().working=true;
        tools.mcp_actions.speak(&tools,"The file is ready.","en","op-speak",&permit)?;
        assert_eq!(spoken(1)?,["The file is ready."]);
        tools.agent_final_answer("The file is ready. All three checks passed.","stop")?;
        assert_eq!(spoken(1)?,["All three checks passed."],"only what speak did not say");
        assert_eq!(heimdall(&tools),["The file is ready.","All three checks passed."],"each said line once in the decider's conversation");
        assert!(!tools.send_context.lock().unwrap().working);
        // A replayed speak call (same op) is not said or remembered twice.
        tools.mcp_actions.speak(&tools,"The file is ready.","en","op-speak",&permit)?;
        assert!(heard.recv_timeout(std::time::Duration::from_millis(200)).is_err());
        // A list reads as separate sentences.
        tools.agent_final_answer("Two fixes:\n- Latency map\n- Island polish","stop")?;
        assert_eq!(spoken(1)?,["Two fixes. Latency map. Island polish"]);
        // Silent, aborted and failed replies say nothing and leave the conversation and the pull-down alone.
        let before=heimdall(&tools);
        for (text,reason) in [("NO_REPLY","stop"),("","stop"),("No request.","stop"),("Half an answer","aborted"),("Partial","error")] {
            tools.agent_final_answer(text,reason)?;
        }
        assert!(heard.recv_timeout(std::time::Duration::from_millis(200)).is_err(),"a silent turn said something");
        assert_eq!(heimdall(&tools),before);
        // A new run may say the same words again (a completed delivery settles the run: supervisor/delivery).
        tools.replies.lock().unwrap().settled();
        tools.agent_final_answer("The file is ready.","stop")?;
        assert_eq!(spoken(1)?,["The file is ready."]);
        Ok(())
    }

    /// Review 19 Oct: a reply the user talks over (barge-in) still joins the decider's conversation; before the reply path
    /// spoke final replies through the queue, every final reply did. A line cut by Speaking going off is shown and
    /// joins it once.
    #[test]
    fn an_interrupted_reply_still_joins_the_decider_conversation() -> Result<()> {
        let dir=tempfile::tempdir()?;let tools=Tools::new(dir.path(),dir.path());
        tools.mcp_actions.attach(ui::Ui::handle(),Arc::new(|_,_,_|Ok(())),Arc::new(|_,_|Ok(())),Arc::new(|_,_|Ok(())))?;
        let heimdall=|tools:&Tools|tools.send_context.lock().unwrap().snapshot().turns.iter().filter(|t|t.speaker=="Heimdall").map(|t|t.text.clone()).collect::<Vec<_>>();
        tools.speech_text.lock().unwrap().insert(7,"The build is green. All 222 tests passed.".into());
        tools.speech_event(speak::SpeechEvent::Cancelled{job:7,heard:true});
        assert_eq!(heimdall(&tools),["The build is green. All 222 tests passed."]);
        // Cut before any audio (queued behind the line the user talked over): never said, never in the conversation.
        tools.speech_text.lock().unwrap().insert(6,"Queued and never played.".into());
        tools.speech_event(speak::SpeechEvent::Cancelled{job:6,heard:false});
        assert_eq!(heimdall(&tools).len(),1);
        tools.speech_text.lock().unwrap().insert(8,"Speaking went off here.".into());
        tools.unheard_by_mode.lock().unwrap().insert(8);
        tools.speech_event(speak::SpeechEvent::Cancelled{job:8,heard:false});
        assert_eq!(heimdall(&tools),["The build is green. All 222 tests passed.","Speaking went off here."]);
        // An unknown job (already completed or never queued) adds nothing.
        tools.speech_event(speak::SpeechEvent::Cancelled{job:9,heard:true});
        assert_eq!(heimdall(&tools).len(),2);
        Ok(())
    }

    /// Speaking off: the final reply is the island line (no voice), said once, and in the decider's conversation.
    #[test]
    fn final_reply_with_speaking_off_is_one_island_line() -> Result<()> {
        let dir=tempfile::tempdir()?;let tools=Tools::new(dir.path(),dir.path());
        tools.mcp_actions.attach(ui::Ui::handle(),Arc::new(|_,_,_|Ok(())),Arc::new(|_,_|Ok(())),Arc::new(|_,_|Ok(())))?;
        tools.enabled.store(false,Ordering::Release);
        let permit=tools.agent_enabled.permit()?;
        tools.mcp_actions.speak(&tools,"On it.","en","op-1",&permit)?;
        tools.agent_final_answer("On it. The build is green.","stop")?;
        let db=rusqlite::Connection::open(dir.path().join("agent/mcp-actions.sqlite"))?;
        let lines:Vec<String>=db.prepare("SELECT body FROM receipts WHERE kind='notice' ORDER BY rowid")?.query_map([],|r|r.get(0))?.collect::<rusqlite::Result<_>>()?;
        assert_eq!(lines.len(),2,"{lines:?}");
        assert!(lines[0].contains("On it.") && lines[1].contains("The build is green.") && !lines[1].contains("On it."),"{lines:?}");
        let turns=tools.send_context.lock().unwrap().snapshot().turns.iter().map(|t|t.text.clone()).collect::<Vec<_>>();
        assert_eq!(turns,["On it.","The build is green."]);
        Ok(())
    }

    /// Plan usage limits (runtime/quota.mjs, 26 Oct 23:33): a spent limit is said once through the reply path (here
    /// Speaking off, so the island line), a repeat within its window and a replayed frame (a Pi restart) say nothing,
    /// the 80% warning is its own line, and every event is logged under `quota:` with its reset and source.
    #[test]
    fn a_spent_usage_limit_is_said_once_per_window() -> Result<()> {
        let dir=tempfile::tempdir()?;let tools=Tools::new(dir.path(),dir.path());
        tools.mcp_actions.attach(ui::Ui::handle(),Arc::new(|_,_,_|Ok(())),Arc::new(|_,_|Ok(())),Arc::new(|_,_|Ok(())))?;
        tools.enabled.store(false,Ordering::Release);
        let reset=1793066648000i64;
        let limit=json!({"type":"heimdall_quota","event":"limit","provider":"openai","line":"ChatGPT's five-hour limit is used up; it resets at 3:04.",
            "window_minutes":300,"resets_at_ms":reset,"source":"header","code":"subscription_sharing_usage_limit_exceeded","status":200,"key":"limit:openai:29884444","repeat":false});
        tools.quota_notice(&limit);
        let mut repeat=limit.clone();repeat["repeat"]=json!(true);
        tools.quota_notice(&repeat);
        tools.quota_notice(&limit);
        let warning=json!({"type":"heimdall_quota","event":"warning","provider":"openai","line":"ChatGPT's five-hour limit is 81% used; it resets at 8:04.",
            "used_percent":81,"window_minutes":300,"resets_at_ms":reset+5*3600_000,"source":"header","key":"warning:openai:29884744","repeat":false});
        tools.quota_notice(&warning);
        let db=rusqlite::Connection::open(dir.path().join("agent/mcp-actions.sqlite"))?;
        let lines:Vec<String>=db.prepare("SELECT body FROM receipts WHERE kind='notice' ORDER BY rowid")?.query_map([],|r|r.get(0))?.collect::<rusqlite::Result<_>>()?;
        assert_eq!(lines.len(),2,"{lines:?}");
        assert!(lines[0].contains("ChatGPT's five-hour limit is used up; it resets at 3:04.") && lines[1].contains("81% used"),"{lines:?}");
        let log=super::tools::quota_log(&repeat);
        assert!(log.starts_with("quota: openai limit hit: \"ChatGPT's five-hour limit is used up; it resets at 3:04.\" · reset ")
            && log.contains("· source header") && log.contains("· code subscription_sharing_usage_limit_exceeded") && log.ends_with("· already said this window"),"{log}");
        let unknown=super::tools::quota_log(&json!({"event":"limit","provider":"openai","line":"ChatGPT's usage limit is used up.","resets_at_ms":null,"source":"none"}));
        assert!(unknown.contains("· reset not stated · source none"),"{unknown}");
        Ok(())
    }

    /// Bug note 2026-11-01T00-20-27: Enter to bubble waited on the supervisor's delivery (the archive held). The bubble
    /// is published by the window's action itself, before the supervisor has even taken the command, and the command
    /// carries its ID so the delivered row replaces it.
    #[test]
    fn typed_bubble_shows_before_the_supervisor_takes_the_message() -> Result<()> {
        let dir=tempfile::tempdir()?;
        let tools=Arc::new(Tools::new(dir.path(),dir.path()));
        tools.agent_enabled.store(true,Ordering::Release);
        // A supervisor that never gets to it: the send is blocked for as long as the test runs.
        let (tx,mut blocked)=mpsc::unbounded_channel();
        *tools.agent_commands.lock().unwrap()=Some(tx);
        tools.window_action(ui::AgentWindowAction::Send{text:"helo aparenly you recovered".into(),steer:false})?;
        let shown=tools.test_island.lock().unwrap().clone();
        let bubble=shown.iter().find_map(|u|match u {ui::UiUpdate::AgentWindow(ui::AgentWindowUpdate::Entries(e))=>e.first().cloned(),_=>None}).expect("the bubble shows at once");
        assert!(bubble.queued && !bubble.sent && !bubble.error && bubble.kind==ui::AgentLiveKind::Prompt);
        assert_eq!(ui::agent_view::display(&ui::agent_view::Row::Live(bubble.clone())).unwrap().heading,"User · queued");
        assert_eq!(ui::agent_view::typed(&bubble).as_deref(),Some("helo aparenly you recovered"));
        match blocked.try_recv()? {
            Command::Scoped{command,..}=>assert!(matches!(*command,Command::Typed{ref echo,ref text,steer:false} if *echo==bubble.id && text=="helo aparenly you recovered")),
            _=>panic!("a scoped typed message"),
        }
        // The supervisor gone: the send is refused and the bubble says it was not sent.
        drop(blocked);
        assert!(tools.window_action(ui::AgentWindowAction::Send{text:"second".into(),steer:false}).is_err());
        let last=tools.test_island.lock().unwrap().iter().rev().find_map(|u|match u {ui::UiUpdate::AgentWindow(ui::AgentWindowUpdate::Entries(e))=>e.first().cloned(),_=>None}).unwrap();
        assert!(last.error && !last.queued && !last.sent && last.owner_text.as_deref()==Some("second"));
        Ok(())
    }

    #[test]
    fn typed_barrier_keeps_speech_arriving_during_delivery() -> Result<()> {
        let dir=tempfile::tempdir()?;
        let tools=Arc::new(Tools::new(dir.path(),dir.path()));
        tools.send_context.lock().unwrap().append_at("User","old unfinished request",true,1);
        let (entered,started)=tokio::sync::oneshot::channel();
        let (release,wait)=tokio::sync::oneshot::channel();
        tokio::runtime::Runtime::new()?.block_on(async {
            let sender=tools.clone();
            let delivery=tokio::spawn(async move {
                typed_delivery(&sender,"use this folder",true,async {entered.send(()).unwrap();wait.await?;Ok(())}).await
            });
            started.await?;
            let through=tools.send_context.lock().unwrap().append_at("User","Heimdall cancel that",true,capture::host_now_ns());
            release.send(()).unwrap();delivery.await??;
            let context=tools.send_context.lock().unwrap();
            assert!(context.has_pending_through(through),"the queued spoken wake must remain eligible");
            assert_eq!(context.pending_text(),"User: Heimdall cancel that");
            assert!(context.snapshot().turns.iter().any(|t|t.speaker=="User (typed)"));
            Ok::<_,anyhow::Error>(())
        })
    }

    #[test]
    fn ui_gap_review_state_comes_from_ledger_without_history() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let settings = Settings { dry_run: false, ..Default::default() };
        let session = dir.path().join("archive/session-lab");
        drop(unavailable(&settings, dir.path(), &session, "fixture".into())?);
        let db = rusqlite::Connection::open(dir.path().join("pi-spool.sqlite"))?;
        let wire = json!({"timestamp_ms":1234,"message":{"content":[
            {"type":"text","text":"Review this wake"},
            {"type":"image","source":{"media_type":"image/png","archive_image_id":"evidence"}}
        ]}});
        db.execute("INSERT INTO deliveries VALUES('wake',?1,1,0,'needs_reconciliation')", [wire.to_string()])?;
        let bridge = unavailable(&settings, dir.path(), &session, "fixture".into())?;
        let state = agent_window_state(&bridge, &settings, 0, None);
        assert_eq!(state.state, ui::AgentRunState::NeedsReview);
        assert_eq!(state.unresolved, vec!["wake"]);
        assert!(state.can_send, "typed text queues behind the uncertain delivery");
        assert_eq!(state.reviews[0].id, "wake");
        assert_eq!(state.reviews[0].text, "Review this wake");
        assert_eq!(state.reviews[0].timestamp_ms, Some(1234));
        assert_eq!(state.reviews[0].attachments[0].archive_id.as_deref(), Some("evidence"));
        Ok(())
    }

    #[test]
    fn ui_gap_reload_state_carries_reason_and_all_receipts() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let settings = Settings { dry_run: false, ..Default::default() };
        let bridge = unavailable(&settings, dir.path(), &dir.path().join("session"), "fixture".into())?;
        let first = format!("reload-{}", uuid::Uuid::new_v4());
        let second = format!("reload-{}", uuid::Uuid::new_v4());
        let reason = format!("{first}: MCP changed; also requested {second}: tools changed");
        let state = agent_window_state(&bridge, &settings, 0, Some(&reason));
        assert_eq!(state.state, ui::AgentRunState::ReloadPending);
        assert_eq!(state.reload, Some(ui::AgentReloadPending { reason: "MCP changed; also requested tools changed".into(), receipt_ids: vec![first, second] }));
        assert!(!state.can_send);
        assert_eq!(agent_window_state(&bridge, &settings, 0, None).reload, None);
        assert_eq!(reload_context("Session storage maintenance").receipt_ids, Vec::<String>::new());
        Ok(())
    }

    #[test]
    fn ui_gap_notice_outcomes_keep_identity_kind_and_legacy_text() {
        use ui::agent_notice::{NoticeKind, NoticeOutcome as O};
        for (outcome, label, kind) in [
            (O::T3Connected, "T3 connected", NoticeKind::Success),
            (O::T3ConnectionFailed, "T3 connection failed; click Connect T3 to retry", NoticeKind::Warning),
            (O::T3NeedsAgent, "Turn Agent on to connect T3", NoticeKind::Warning),
            (O::QuestionUnavailable, "Question unavailable; ask again", NoticeKind::Warning),
            (O::Answered { choice: "Yes".into() }, "Answered: Yes", NoticeKind::Success),
            (O::Answered { choice: "Tomorrow".into() }, "Answered: Tomorrow", NoticeKind::Success),
        ] {
            let notice = outcome_notice("request-1".into(), outcome.clone());
            assert_eq!(notice.id, "request-1");
            assert_eq!(notice.outcome, Some(outcome));
            assert_eq!(notice.text, label);
            assert_eq!(notice.kind, kind);
            assert_eq!(notice.action, None);
        }
        assert_eq!(outcome_notice("request-2".into(), O::QuestionUnavailable).id, "request-2");
    }

    #[test]
    fn ui_gap_identified_connect_click_keeps_notice_id_and_epoch() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let tools = Arc::new(Tools::new(dir.path(), dir.path()));
        let (commands, mut rx) = mpsc::unbounded_channel();
        *tools.agent_commands.lock().unwrap() = Some(commands);
        let (pipeline, queued) = crossbeam_channel::unbounded();
        let callback = crate::ui_action_callback(tools.clone(), pipeline, ui::Ui::handle());
        let id = "t3-connect-401:original-environment:1234";
        callback(ui::UiAction::AgentNotice(ui::agent_notice::AgentNoticeAction::ConnectT3Notice { id: id.into() }));
        let Command::NoticeAction { action, permit } = rx.try_recv()? else { panic!("wrong command") };
        assert_eq!(action, ui::agent_notice::AgentNoticeAction::ConnectT3Notice { id: id.into() });
        assert!(queued.is_empty());
        permit.check()?;
        tools.agent_enabled.store(false, Ordering::Release);
        tools.agent_enabled.store(true, Ordering::Release);
        assert!(permit.check().is_err());
        Ok(())
    }
    /// "the speed test … doesn't have a Jarvis voice". The sample asked for while Jarvis is not
    /// resident (Heimdall paused) waits for him to load, the slider row says "Loading Jarvis…" meanwhile, and the
    /// lines Jarvis says are what plays; if Jarvis cannot load, the row says so and nothing plays: the lighter voice
    /// (no models in this test, so any use of it would fail the call) never speaks the sample. The hold keeps him
    /// resident briefly afterwards.
    #[test]
    fn the_speed_sample_waits_for_jarvis_and_never_uses_another_voice() -> Result<()> {
        let t = tempfile::tempdir()?;
        let sidecar = |name: &str, script: &str| -> Result<Arc<speak::drama3::Service>> {
            let assets = t.path().join(name);
            std::fs::create_dir_all(assets.join("runtime/bin"))?;
            std::os::unix::fs::symlink("/usr/bin/python3", assets.join("runtime/bin/python3"))?;
            std::fs::write(assets.join("sidecar.py"), script)?;
            Ok(speak::drama3::Service::new(assets))
        };
        // Loads in 0.6 s, then says each line as one sentence of PCM.
        let jarvis = sidecar("jarvis", "import sys,json,base64,time\ntime.sleep(.6)\nprint(json.dumps({'event':'ready'}),flush=True)\nfor line in sys.stdin:\n r=json.loads(line);i=r['id']\n if r['op']!='speak': continue\n data=base64.b64encode(b'\\xcd\\xcc\\x4c\\x3e'*4096).decode()\n print(json.dumps({'event':'pcm','id':i,'rate':44100,'pcm':data}),flush=True)\n print(json.dumps({'event':'sentence_done','id':i,'sentence':0}),flush=True)\n print(json.dumps({'event':'done','id':i,'synthesis_ms':10}),flush=True)\n")?;
        let notes = Arc::new(Mutex::new(Vec::<Option<String>>::new()));
        let n = notes.clone();
        let hold = Arc::new(Mutex::new(None));
        let sample = SampleVoice { hold: hold.clone(), generation: Arc::new(AtomicU64::new(1)), note: Arc::new(move |note| n.lock().unwrap().push(note)) };
        let synth = |drama3: &Arc<speak::drama3::Service>| LazySynth { drama3: Some(drama3.clone()), session: t.path().join("session"), models: t.path().join("no-models"),
            model: Default::default(), backend: "auto".into(), record: None, speed: Arc::new(AtomicU16::new(120)), sample: Some(sample.clone()), voices: None };
        // As play_voice_sample does: ask for Jarvis, then speak.
        jarvis.configure(true);
        assert!(!jarvis.ready() && jarvis.loading());
        let mut packets = vec![];
        let audio = speak::Synthesizer::stream_packets(&mut synth(&jarvis), VOICE_SAMPLE, "en", &|| false, &mut |p| { packets.push(p); Ok(()) })?;
        assert!(jarvis.ready() && !audio.samples.is_empty());
        let played: usize = packets.iter().map(|p| match p { speak::SpeechPacket::Audio(a) => a.samples.len(), _ => 0 }).sum();
        // 4,096 Jarvis samples, stretched to 1.2×.
        assert!(played > 0 && played < 4096, "Jarvis's PCM at User's speed: {played}");
        assert_eq!(*notes.lock().unwrap(), [Some("Loading Jarvis…".to_string()), None]);
        assert!(hold.lock().unwrap().is_some_and(|until: std::time::Instant| until > std::time::Instant::now()), "kept resident for the next move");
        // Resident now: the next sample plays at once; the row only shows the speed (clearing any older note).
        notes.lock().unwrap().clear();
        speak::Synthesizer::stream_packets(&mut synth(&jarvis), VOICE_SAMPLE, "en", &|| false, &mut |_| Ok(()))?;
        assert_eq!(*notes.lock().unwrap(), [None]);
        jarvis.stop();
        // Jarvis cannot load (no weights): the row says so, nothing plays, and no other voice is tried.
        let broken = sidecar("broken", "import json,time\ntime.sleep(.2)\nprint(json.dumps({'event':'fatal','reason':'no weights'}),flush=True)\n")?;
        broken.configure(true);
        notes.lock().unwrap().clear();
        let mut packets = 0;
        let e = speak::Synthesizer::stream_packets(&mut synth(&broken), VOICE_SAMPLE, "en", &|| false, &mut |_| { packets += 1; Ok(()) }).unwrap_err();
        assert!(e.to_string().starts_with("Jarvis unavailable for the voice sample"), "{e:#}");
        assert_eq!(packets, 0);
        assert_eq!(*notes.lock().unwrap(), [Some("Loading Jarvis…".to_string()), Some("Jarvis unavailable".to_string())]);
        // The note clears itself unless a newer sample took the row.
        std::thread::sleep(std::time::Duration::from_millis(4300));
        assert_eq!(notes.lock().unwrap().last().cloned().flatten(), None);
        Ok(())
    }
    #[test]
    #[ignore = "requires pinned local TTS models; offline synthesis only"]
    fn tts_every_component_output_is_archived_without_playback() -> Result<()> {
        let t = tempfile::tempdir()?;
        let session=t.path().join("session");
        let mut synth=LazySynth { drama3:None, backend:"auto".into(), record:None, session:session.clone(),models:Path::new(env!("CARGO_MANIFEST_DIR")).join("../../models/supertonic-3"),model:Default::default(),speed:Arc::new(AtomicU16::new(100)),sample:None,voices:None };
        let audio=speak::Synthesizer::synthesize(&mut synth,"The window is ready.","en",&||false)?;
        assert!(!audio.samples.is_empty());
        let journal=heimdall_archive::Journal::open_read_only(&session)?;
        let records=journal.records()?;
        assert_eq!(records.len(),8);
        assert_eq!(records.iter().filter(|r|r.payload["model"]=="supertonic_vector_estimator").count(),5);
        let vocoder=records.iter().find(|r|r.payload["model"]=="supertonic_vocoder").unwrap();
        let restored: Vec<f32> = vocoder.payload["output"]["vector_ids"].as_array().unwrap().iter()
            .map(|id| journal.vector_values(id.as_str().unwrap())).collect::<Result<Vec<_>>>()?.into_iter().flatten().collect();
        assert_eq!(&restored[..audio.samples.len()],audio.samples.as_slice());
        assert!(records.iter().all(|r|r.payload.get("input").is_some()));
        if let Some(destination)=std::env::var_os("HEIMDALL_TTS_RECEIPT") {
            let mut values=std::collections::BTreeMap::new();
            fn collect(journal:&heimdall_archive::Journal,v:&Value,out:&mut std::collections::BTreeMap<String,Vec<f32>>) -> Result<()> {
                if let Some(ids)=v.get("vector_ids").and_then(Value::as_array) {
                    for id in ids { let id=id.as_str().unwrap(); out.insert(id.into(),journal.vector_values(id)?); }
                }
                match v { Value::Object(m)=>for c in m.values(){collect(journal,c,out)?;},Value::Array(a)=>for c in a{collect(journal,c,out)?;},_=>() }
                Ok(())
            }
            for r in &records {collect(&journal,&r.payload,&mut values)?;}
            std::fs::write(destination,serde_json::to_vec(&json!({"records":records,"vectors":values,"text":"The window is ready.","audio_samples":audio.samples.len(),"sample_rate":audio.sample_rate}))?)?;
        }
        Ok(())
    }
    #[test]
    fn screenshot_tool_with_seeing_off_answers_screen_off() {
        use agent::mcp::ToolHost;
        let t = tempfile::tempdir().unwrap();
        let tools = Tools::new(&t.path().join("no-models"), t.path());
        tools.screen_off.store(true, Ordering::Release);
        let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let result = runtime.block_on(tools.screenshot_now(&tools.agent_enabled.permit().unwrap())).unwrap();
        assert!(result.images.is_empty() && result.warnings.is_empty());
        assert_eq!(result.notes, [SCREEN_OFF]);
        assert_eq!(tools.screenshot_status(), None, "a chosen screen-off is not a failure");
    }
    #[test]
    fn unavailable_tools_publish_status_without_permissions_or_playback() {
        use agent::mcp::ToolHost;
        let t = tempfile::tempdir().unwrap();
        let tools = Tools::new(&t.path().join("no-models"), t.path());
        tools.enabled.store(false, Ordering::Release);
        assert!(tools.speak("Hello.", "en", &tools.agent_enabled.permit().unwrap()).is_err());
        assert_eq!(tools.speech_status().as_deref(), Some("Speech replies off"));
        let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
        assert!(runtime.block_on(tools.screenshot_now(&tools.agent_enabled.permit().unwrap())).is_err());
        assert_eq!(tools.screenshot_status().as_deref(), Some("Screenshot unavailable"));
    }
    /// Review 23 Oct, finding 12: one failed spoken line does not put "Heimdall can't speak right now" up; two in a
    /// row do, and it clears with the next line that plays, Speaking off, or ten minutes after the last failure.
    #[test]
    fn one_failed_line_is_not_speech_unavailable_and_the_line_clears_itself() {
        use agent::mcp::ToolHost;
        struct Flaky(Arc<AtomicBool>);
        impl speak::Synthesizer for Flaky {
            fn synthesize(&mut self, _: &str, _: &str, _: &dyn Fn() -> bool) -> Result<speak::Audio> {
                anyhow::ensure!(!self.0.load(Ordering::Acquire), "synthesis failed");
                Ok(speak::Audio { samples: vec![0.; 160], sample_rate: 16000, first_audio_ms: 0., synthesis_ms: 1. })
            }
        }
        struct Silent;
        impl speak::AudioOutput for Silent {
            fn output(&mut self, _: &speak::Audio, _: &dyn Fn() -> bool) -> Result<()> { Ok(()) }
        }
        let t = tempfile::tempdir().unwrap();
        let tools = Tools::new(&t.path().join("no-models"), t.path());
        let failing = Arc::new(AtomicBool::new(true));
        tools.test_guarded_speech_queue(Flaky(failing.clone()), Silent);
        let say = |text: &str| {
            tools.speak(text, "en", &tools.agent_enabled.permit().unwrap()).unwrap();
            let began = std::time::Instant::now();
            while tools.agent_speech_busy() { assert!(began.elapsed() < std::time::Duration::from_secs(10)); std::thread::sleep(std::time::Duration::from_millis(2)); }
            tools.speech_status()
        };
        assert_eq!(say("One."), None, "a single failed line");
        assert_eq!(say("Two.").as_deref(), Some("Speech unavailable"), "two in a row");
        failing.store(false, Ordering::Release);
        assert_eq!(say("Three."), None, "the next line that played cleared it");
        failing.store(true, Ordering::Release);
        say("Four.");
        assert_eq!(say("Five.").as_deref(), Some("Speech unavailable"));
        tools.speech_failures.lock().unwrap().1 = Some(std::time::Instant::now() - crate::bridge::tools::SPEECH_FAILURE_SHOWN_FOR);
        assert_eq!(tools.speech_status(), None, "ten minutes after the last failure");
        assert_eq!(say("Six."), None, "and the count started again");
        assert_eq!(say("Seven.").as_deref(), Some("Speech unavailable"));
        tools.clear_speech_failures();
        assert_eq!(tools.speech_status(), None, "Speaking off");
    }
    #[test]
    fn ui_off_never_waits_for_a_held_speech_queue_mutex() -> Result<()> {
        for action in [ui::UiAction::SetDryRun(true), ui::UiAction::SetListening(false), ui::UiAction::Quit] {
            let listening = matches!(action, ui::UiAction::SetListening(_));
            let temp = tempfile::tempdir()?;
            let tools = Arc::new(Tools::new(temp.path(), temp.path()));
            let held = tools.queue.lock().unwrap();
            let (tx, rx) = crossbeam_channel::unbounded();
            let (finished, done) = crossbeam_channel::bounded(1);
            let callback_tools = tools.clone();
            let worker = std::thread::spawn(move || {
                let callback = crate::ui_action_callback(callback_tools, tx, ui::Ui::handle());
                let begin = std::time::Instant::now();
                callback(action);
                finished.send(begin.elapsed()).unwrap();
            });
            let elapsed = done.recv_timeout(std::time::Duration::from_millis(200))?;
            assert!(elapsed < std::time::Duration::from_millis(50));
            // Listening off has its own latch: the agent stays on.
            assert_eq!(tools.agent_enabled.load(Ordering::Acquire), listening);
            assert_eq!(rx.len(), 1);
            drop(held);
            worker.join().unwrap();
        }
        Ok(())
    }
    #[test]
    fn agent_off_interrupts_active_and_queued_speech_and_stale_permits() -> Result<()> {
        use agent::mcp::ToolHost;
        struct Synth { started: std::sync::mpsc::Sender<()>, calls: Arc<AtomicU64> }
        impl speak::Synthesizer for Synth {
            fn synthesize(&mut self, _: &str, _: &str, cancel: &dyn Fn() -> bool) -> Result<speak::Audio> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                self.started.send(())?;
                while !cancel() { std::thread::sleep(std::time::Duration::from_millis(1)); }
                anyhow::bail!("cancelled")
            }
        }
        struct Output;
        impl speak::AudioOutput for Output {
            fn output(&mut self, _: &speak::Audio, _: &dyn Fn() -> bool) -> Result<()> { panic!("no playback after Off") }
        }
        let t = tempfile::tempdir()?;
        let tools = Tools::new(t.path(), t.path());
        let calls = Arc::new(AtomicU64::new(0));
        let (tx, rx) = std::sync::mpsc::channel();
        tools.test_guarded_speech_queue(Synth { started: tx, calls: calls.clone() }, Output);
        let permit = tools.agent_enabled.permit()?;
        tools.speak("First.", "en", &permit)?;
        rx.recv_timeout(std::time::Duration::from_secs(2))?;
        tools.speak("Queued.", "en", &permit)?;
        tools.set_agent_enabled(false);
        assert!(tools.speak("Off.", "en", &permit).is_err());
        tools.set_agent_enabled(true);
        assert!(tools.speak("Stale.", "en", &permit).is_err());
        tools.shutdown(); // Joins the worker, so the count includes all queued work.
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
        assert!(runtime.block_on(tools.screenshot_now(&permit)).is_err());
        Ok(())
    }
    #[test]
    fn command_images_archive_both_resolutions_without_duplicate_objects() -> Result<()> {
        let t = tempfile::tempdir()?;
        let session = t.path().join("session");
        let journal = heimdall_archive::Journal::open(&session)?;
        let mut b = screen::ScreenBuffer::new(screen::ScreenConfig{fps:1,..Default::default()})?;
        let make_frame = |id, width, height| screen::Frame::new(screen::DisplayInfo { id, x:0., y:0., width:3840, height:2160 }, capture::host_now_ns(), width, height, vec![id as u8; width as usize * height as usize * 3]);
        let now = capture::host_now_ns();
        for id in [1, 2] { let mut frame=make_frame(id,2,1)?;frame.host_time_ns=now;b.push(frame)?; }
        let b = Arc::new(Mutex::new(b));
        let matched = screen::PhraseMatch { source:"mic".into(), phrase:"look at this".into(), start_ns:now, trigger_ns:now };
        for _ in 0..2 {
            let action = screen::attach_command_with_grabber(&b, &matched, now, &t.path().join("shots"), |id| make_frame(id, 4, 2))?;
            let (images, refs) = archive_screenshot_action(&journal, &action, "mic", &["session:1".into()], now)?;
            assert_eq!(images.len(), 2);
            assert_eq!(refs.len(), 4, "both resolutions stay archived");
            assert_eq!(refs.iter().filter(|s| s["kind"]=="full_resolution").count(), 2);
            for (image, reference) in images.iter().zip(refs.into_iter().filter(|s|s["kind"]=="full_resolution")) {
                assert_eq!(journal.image_bytes(reference["image_id"].as_str().unwrap())?.1, image.bytes);
                // Provider/wake archival reuses the same CAS object.
                assert_eq!(journal.image(&image.bytes, &image.media_type)?, reference["image_id"].as_str().unwrap());
            }
            assert_eq!(std::fs::read_dir(t.path().join("shots"))?.count(), 0);
        }
        assert_eq!(heimdall_archive::store::open(t.path())?.query_row("SELECT count(*) FROM images", [], |r| r.get::<_, i64>(0))?, 4);
        let shots: Vec<_> = journal.records()?.into_iter().filter(|r| r.kind=="screenshot").collect();
        assert_eq!(shots.len(), 8);
        assert_eq!(shots.iter().filter(|r| r.payload["kind"]=="full_resolution").count(), 4);
        Ok(())
    }
    #[test]
    fn agent_status_becomes_island_health() {
        let mut settings=Settings {dry_run:false,..Default::default()};let mut h=AgentHealth::default();
        assert!(h.cues(&agent::Status::Idle,0,&settings).is_empty());
        assert!(matches!(h.cues(&agent::Status::WaitingEngine,0,&settings)[..],[ui::HealthCue::Drop{..}]));
        assert!(matches!(h.cues(&agent::Status::Idle,0,&settings)[..],[ui::HealthCue::Recovered{..}]));
        // S43 / E6.6 (18 Oct F4): after a failed process or turn, no "Agent OK" until a turn settles with error:false.
        assert!(matches!(h.cues(&agent::Status::AuthBlocked("detail".into()),1,&settings)[..],[ui::HealthCue::Restarting{..}]));
        // An idle Pi that has not answered clears the held restart cue; it claims no recovery (E6.6, E1.16).
        let mut idle_first=AgentHealth::default();
        assert!(matches!(idle_first.cues(&agent::Status::AuthBlocked("detail".into()),1,&settings)[..],[ui::HealthCue::Restarting{..}]));
        assert!(matches!(idle_first.cues(&agent::Status::Idle,0,&settings)[..],[ui::HealthCue::Clear{..}]));
        assert!(h.cues(&agent::Status::Working,0,&settings).is_empty(),"recovery claimed without a successful turn");
        h.turn_succeeded();
        assert!(matches!(h.cues(&agent::Status::Idle,0,&settings)[..],[ui::HealthCue::Recovered{..}]));
        assert!(matches!(h.cues(&agent::Status::AuthBlocked("detail".into()),4,&settings)[..],[ui::HealthCue::Problem{..}]));
        // Review of 18 Oct: "Restarting the agent…" only while a restart is really scheduled; otherwise the
        // uncertain delivery asks for review.
        let restarting_label=|cues:Vec<ui::HealthCue>|format!("{cues:?}").contains("Restarting the agent");
        h.restarting=true;assert!(restarting_label(h.cues(&agent::Status::NeedsReconciliation("d".into()),1,&settings)));
        h.restarting=false;let cues=h.cues(&agent::Status::NeedsReconciliation("d".into()),1,&settings);
        assert!(!restarting_label(cues.clone()) && matches!(cues[..],[ui::HealthCue::Problem{..}]),"{cues:?}");
        settings.dry_run=true;assert!(matches!(h.cues(&agent::Status::Idle,0,&settings)[..],[ui::HealthCue::Clear{..}]));
    }
    #[test]
    fn dry_pipeline_logs_labelled_batch_attachments_and_settles_without_process() {
        let t = tempfile::tempdir().unwrap();
        let models = t.path().join("no-models");
        let data = t.path().join("data");
        let session = data.join("archive/session-test");
        let actor = AgentRuntime::start(
            Settings::default(),
            data.clone(),
            session.clone(),
            Arc::new(Tools::new(&models, &data)),
            None,
        )
        .unwrap();
        for (id, speaker) in [("1", "User"), ("2", "Speaker 1")] {
            actor
                .send(Command::Observe(Observation {
                    id: id.into(),
                    timestamp_ms: 1,
                    source: "fake".into(),
                    speaker: speaker.into(),
                    language: "en".into(),
                    text: "look at this".into(),
                }))
                .unwrap();
        }
        actor
            .send(Command::Wake {
                images: vec![Attachment {
                    media_type: "image/png".into(),
                    bytes: vec![1, 2, 3],
                }],
                urgent: false,
            })
            .unwrap();
        actor.barrier().unwrap();
        drop(actor);
        let log = std::fs::read_to_string(crate::archive::run_dir(&data, &session).join("dry-run.jsonl")).unwrap();
        let records: Vec<Value> = log
            .lines()
            .map(|s| serde_json::from_str(s).unwrap())
            .collect();
        assert_eq!(records.len(), 3);
        assert!(!records[0]["would_send"]["shouldQuery"].as_bool().unwrap());
        assert!(records[2]["would_send"]["shouldQuery"].as_bool().unwrap());
        let text = records[2]["would_send"]["message"]["content"][0]["text"]
            .as_str()
            .unwrap();
        assert!(text.contains("observations already in context"));
        let all = records.iter().filter_map(|r|r["would_send"]["message"]["content"][0]["text"].as_str()).collect::<Vec<_>>().join("\n");
        assert_eq!(all.matches(" fake · User] look at this").count(),1);
        assert_eq!(all.matches(" fake · Speaker 1] look at this").count(),1);
        assert_eq!(
            records[2]["would_send"]["message"]["content"][1]["type"],
            "image"
        );
        let c = rusqlite::Connection::open(crate::archive::run_dir(&data, &session).join("dry-run-spool.sqlite")).unwrap();
        let uncertain: i64 = c
            .query_row(
                "SELECT count(*) FROM deliveries WHERE query=1 AND state!='completed'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(uncertain, 0);
        assert!(!data.join("claude-spool.sqlite").exists());
        let journal = heimdall_archive::Journal::open_read_only(&session).unwrap();
        let events = journal.records().unwrap();
        let sent = events
            .iter()
            .find(|r| r.kind == "agent_send" && r.payload["query"] == true)
            .unwrap();
        assert_eq!(sent.utterance_ids, vec!["1", "2"]);
        assert_eq!(sent.source, "agent:dry");
        assert!(sent.host_ns > 0);
        assert!(sent.wall_ns > 0);
        let image_id = sent.payload["payload"]["message"]["content"][1]["source"]
            ["archive_image_id"]
            .as_str()
            .unwrap();
        assert_eq!(journal.image_bytes(image_id).unwrap().1, vec![1, 2, 3]);
        assert!(events.iter().any(|r| r.kind == "agent_wake"));
        let reply = events.iter().find(|r| r.kind == "agent_reply" && r.payload["event"]["kind"] == "complete" && r.payload["event"]["query"] == true).unwrap();
        assert_eq!(reply.utterance_ids, vec!["1", "2"]);
        assert_eq!(reply.payload["attribution"], "exact");
    }
