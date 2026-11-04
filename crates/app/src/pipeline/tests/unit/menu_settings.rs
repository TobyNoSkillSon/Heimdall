    use super::*;
    #[test]
    fn live_settings_keep_archive_sources_and_reject_invalid_candidates() {
        let t=tempfile::tempdir().unwrap();
        let mut app=Pipeline::new(t.path().into(),PathBuf::from("unused"),Settings::default(),None,true).unwrap();
        let session=app.session.clone();
        app.sources.insert("mic".into(),crate::fake_source("mic",0,1));
        let mut next=app.settings.clone();next.speak_replies=false;next.paused=true;
        next.free_space_floor_bytes=11*1024*1024*1024;next.tts_backend="cpu".into();next.parakeet_backend="coreml".into();next.turn_backend="cpu".into();next.speakers_backend="redimnet".into();
        app.apply_settings(next.clone()).unwrap();
        assert_eq!(app.session,session);assert!(app.sources.contains_key("mic"));assert!(app.agent.is_none());
        assert!(!app.tools.enabled.load(Ordering::Acquire));assert!(!app.tools.agent_enabled.load(Ordering::Acquire));
        assert_eq!(app.tools.archive_floor.load(Ordering::Acquire),next.free_space_floor_bytes);
        assert_eq!(*app.tools.tts_backend.lock().unwrap(),"cpu");
        let mut invalid=next.clone();invalid.free_space_floor_bytes=0;assert!(app.apply_settings(invalid).is_err());assert_eq!(app.settings,next);
        app.action(ui::UiAction::SetArchiveFormat(2)).unwrap();app.action(ui::UiAction::SetRetentionDays(7)).unwrap();
        assert_eq!(app.session,session);assert!(app.sources.contains_key("mic"));assert_eq!(app.settings.retention_days,0);
        app.action(ui::UiAction::RestartSession).unwrap();
        app.settings_result("Screen Recording",Some("injected failure".into()));assert!(app.pending_reload.contains_key("Screen Recording"));
        app.settings_result("Screen Recording",None);assert!(app.pending_reload.is_empty());
    }
    #[test]
    // Starts from the personal edition's pre-selected model and pre-added rows; the public edition starts blank
    // (edition_blank_start_is_public_only).
    #[cfg(not(feature = "public"))]
    /// 30 Oct: the agent's model and the engine are independent. This Mac's row puts the agent on the engine without
    /// touching it; a cloud pick leaves the engine running; a removed selection leaves nothing chosen, never another model.
    fn api_selection_leaves_the_engine_alone_and_the_picker_edits_the_list() {
        use agent::provider::{AgentModel, ApiModel};
        let (_temp,mut a)=app();
        let p=agent::provider::provider(agent::provider::OPENCODE_GO).unwrap();
        if std::env::var_os(p.key_env).is_some() { return; }
        // The engine on its own (the Engine section), then the agent on it: neither touches the other.
        let quant=crate::inference_engine::Config::bundled().model("qwen3.8-27b").unwrap().quant.clone();
        a.action(ui::UiAction::LoadEngineModel{id:"qwen3.8-27b".into(),quant}).unwrap();
        assert!(a.settings.engine_on && a.settings.engine_model=="qwen3.8-27b" && a.settings.agent_model==AgentModel::default(), "loading a model never picks it for the agent");
        a.action(ui::UiAction::SelectLocalAgent).unwrap();
        assert!(a.settings.engine_on && a.settings.agent_model.is_local());
        // Haiku without a key is refused.
        let haiku=ui::UiAction::SelectApiModel{provider:"opencode-go".into(),id:"claude-haiku-5-5".into()};
        agent::provider::forget_key(p).unwrap();
        a.action(haiku.clone()).unwrap();
        assert!(a.settings.agent_model.is_local());
        // With a key, Haiku is the agent's model and the engine keeps running.
        agent::provider::save_key(p,"sk-lab-0123456789abcdefghij",None).unwrap();
        a.action(haiku).unwrap();
        assert_eq!(a.settings.agent_model,AgentModel::default());
        assert!(a.settings.engine_on, "a cloud pick never stops the engine");
        // This Mac again: the agent on the engine; the engine as it was.
        a.action(ui::UiAction::SelectLocalAgent).unwrap();
        assert!(a.settings.agent_model.is_local() && a.settings.engine_on && a.settings.engine_model=="qwen3.8-27b");
        // Removing the selected model leaves nothing chosen (no switch to another); an unknown model is never added.
        a.action(ui::UiAction::SelectApiModel{provider:"opencode-go".into(),id:"claude-haiku-5-5".into()}).unwrap();
        a.action(ui::UiAction::SetApiModelAdded{provider:"opencode-go".into(),id:"claude-haiku-5-5".into(),on:false}).unwrap();
        let go=|a:&Pipeline| a.settings.api_models.iter().filter(|m|m.provider=="opencode-go").count();
        assert!(go(&a)==0 && a.settings.agent_model==AgentModel::Unchosen);
        a.action(ui::UiAction::SetApiModelAdded{provider:"opencode-go".into(),id:"not-in-the-catalog".into(),on:true}).unwrap();
        assert_eq!(go(&a),0);
        let mut next=a.settings.clone();next.api_models=vec![ApiModel::haiku()];a.apply_menu_settings(next).unwrap();
        assert_eq!(a.settings.api_models,[ApiModel::haiku()]);
        agent::provider::forget_key(p).unwrap();
    }
    /// The thinking selector: a level the model takes is kept with the model (Low until chosen), an unknown level is
    /// ignored, the agent is not restarted for it; the local engine keeps one level per model.
    #[test]
    // Starts from the personal edition's pre-selected model and pre-added rows; the public edition starts blank
    // (edition_blank_start_is_public_only).
    #[cfg(not(feature = "public"))]
    fn thinking_choices_persist_per_model_without_a_restart() {
        use agent::provider::{ANTHROPIC, OPUS, SONNET};
        let (_temp,mut a)=app();
        let level=|a:&Pipeline,id:&str| a.settings.api_models.iter().find(|m|m.key()==(ANTHROPIC,id)).unwrap().thinking_level(None);
        assert_eq!((level(&a,OPUS),level(&a,SONNET)),("low","low"));
        a.action(ui::UiAction::SetApiModelThinking{provider:ANTHROPIC.into(),id:SONNET.into(),level:"off".into()}).unwrap();
        a.action(ui::UiAction::SetApiModelThinking{provider:ANTHROPIC.into(),id:OPUS.into(),level:"max".into()}).unwrap();
        a.action(ui::UiAction::SetApiModelThinking{provider:ANTHROPIC.into(),id:OPUS.into(),level:"extreme".into()}).unwrap();
        assert_eq!((level(&a,OPUS),level(&a,SONNET)),("max","off"));
        // Kept in the settings file like the context slider: a saved file reads back the same choices.
        let path=a.data.join("thinking-settings.json");
        a.settings.save(&path).unwrap();
        let saved=crate::settings::Settings::load(&path).unwrap();
        assert_eq!(saved.api_models.iter().find(|m|m.id==SONNET&&m.provider==ANTHROPIC).unwrap().thinking.as_deref(),Some("off"));
        // A thinking change on the selected model is not a model change: no restart is asked for.
        let mut next=a.settings.clone();
        next.agent_model=agent::provider::AgentModel::Api{provider:ANTHROPIC.into(),id:OPUS.into()};
        let with_level=|mut s:crate::settings::Settings,l:&str|{s.api_models.iter_mut().find(|m|m.key()==(ANTHROPIC,OPUS)).unwrap().thinking=Some(l.into());s};
        let view=|s:&crate::settings::Settings| s.selected_api_model().map(|m| agent::provider::ApiModel{thinking:None,..m.clone()});
        assert_eq!(view(&with_level(next.clone(),"low")),view(&with_level(next,"high")));
        // The local engine: Low until chosen, kept per model, its levels only; with no model picked nothing is kept
        // (it would be kept for no model).
        assert_eq!(a.ui_settings_value().engine_thinking,"low");
        a.action(ui::UiAction::SetEngineThinking("high".into())).unwrap();
        assert!(a.settings.engine_thinking.is_empty(),"no model picked: nothing kept");
        a.settings.engine_model=crate::inference_engine::Config::bundled().default_model;
        a.action(ui::UiAction::SetEngineThinking("high".into())).unwrap();
        a.action(ui::UiAction::SetEngineThinking("max".into())).unwrap();
        assert_eq!((a.settings.engine_thinking_level(),a.ui_settings_value().engine_thinking.as_str()),("high","high"));
        assert_eq!(a.settings.engine_thinking.get(&a.settings.engine_model).map(String::as_str),Some("high"));
    }
    // [menu lane] The built-in engine's context slider and Background Tasks ▸ Model: kept per engine model / in the
    // file, sent back to the menu, invalid choices refused, and every background job reads the one model.
    #[test]
    // Starts from the personal edition's pre-selected model and pre-added rows; the public edition starts blank
    // (edition_blank_start_is_public_only).
    #[cfg(not(feature = "public"))]
    fn engine_context_and_background_model_persist_and_reach_the_menu_and_jobs() {
        let (_temp,mut a)=app();
        assert_eq!(a.ui_settings_value().engine_context,None);
        a.action(ui::UiAction::SetEngineContext(128_000)).unwrap();
        assert!(a.settings.engine_context.is_empty(),"no model picked: nothing kept (review 30 Oct)");
        a.settings.engine_model=crate::inference_engine::Config::bundled().default_model;
        assert_eq!(a.settings.engine_context_budget(262_144),262_144,"the whole window until chosen");
        a.action(ui::UiAction::SetEngineContext(128_000)).unwrap();
        assert_eq!(a.ui_settings_value().engine_context,Some(128_000));
        assert_eq!(a.settings.engine_context_budget(262_144),128_000);
        // A value between stops snaps to the nearest; a smaller engine window caps it.
        a.action(ui::UiAction::SetEngineContext(190_000)).unwrap();
        assert_eq!(a.settings.engine_context_budget(262_144),195_072);
        assert_eq!(a.settings.engine_context_budget(100_000),100_000);
        assert!(a.action(ui::UiAction::SetEngineContext(5)).is_err() || a.settings.engine_context.values().all(|t| *t>=8192));
        // Background Tasks ▸ Model: Haiku on Anthropic by default; a local choice persists and is published.
        assert_eq!(a.ui_settings_value().background_model,("anthropic".to_string(),"claude-haiku-5-5".to_string()));
        a.action(ui::UiAction::SetBackgroundModel{provider:ui::LOCAL_ENGINE.into(),id:"qwen3.8-27b".into()}).unwrap();
        assert_eq!(a.ui_settings_value().background_model.0,ui::LOCAL_ENGINE);
        a.action(ui::UiAction::SetBackgroundModel{provider:"nowhere".into(),id:"x".into()}).unwrap();
        assert_eq!(a.settings.background_model.provider,ui::LOCAL_ENGINE,"an unknown provider is refused");
        a.action(ui::UiAction::SetBackgroundModel{provider:"anthropic".into(),id:"claude-sonnet-5-5".into()}).unwrap();
        assert_eq!(crate::background_model::current().id,"claude-sonnet-5-5");
        assert_eq!(crate::plan_refresher::request_body("x")["model"],"claude-sonnet-5-5");
        assert_eq!(crate::voice_namer::request_body("x")["model"],"claude-sonnet-5-5");
        // Background Tasks ▸ Voice Names off: published, shown, saved; Islands stays on.
        a.action(ui::UiAction::SetBackgroundTask{task:ui::BackgroundTask::VoiceNames,on:false}).unwrap();
        assert_eq!(a.ui_settings_value().background_off,[ui::BackgroundTask::VoiceNames]);
        assert!(!crate::background_model::enabled(ui::BackgroundTask::VoiceNames) && crate::background_model::enabled(ui::BackgroundTask::Islands));
        let path=a.data.join("menu-lane-settings.json");
        a.settings.save(&path).unwrap();
        let saved=crate::settings::Settings::load(&path).unwrap();
        assert_eq!((saved.background_model,saved.engine_context.get(&saved.engine_model).copied(),saved.background_off),
            (a.settings.background_model.clone(),Some(190_000),vec!["voice_names".to_string()]));
        a.action(ui::UiAction::SetBackgroundTask{task:ui::BackgroundTask::VoiceNames,on:true}).unwrap();
        assert!(a.settings.background_off.is_empty() && crate::background_model::enabled(ui::BackgroundTask::VoiceNames));
        a.action(ui::UiAction::SetBackgroundModel{provider:"anthropic".into(),id:"claude-haiku-5-5".into()}).unwrap();
    }
    // Background Tasks ▸ Model ▸ a model's thinking level: saved, shown, published to the jobs' calls; an empty level
    // removes it; a relaunch publishes it before any menu change.
    #[test]
    fn a_background_models_level_persists_and_reaches_the_menu_and_the_calls() {
        let (_temp,mut a)=app();
        let t3="t3:claudeAgent_claude_code_2";
        a.action(ui::UiAction::SetBackgroundModel{provider:t3.into(),id:"claude-haiku-5-5".into()}).unwrap();
        assert_eq!(crate::background_model::current().provider,t3);
        a.action(ui::UiAction::SetBackgroundEffort{provider:t3.into(),id:"claude-haiku-5-5".into(),level:"low".into()}).unwrap();
        a.action(ui::UiAction::SetBackgroundEffort{provider:"anthropic".into(),id:"claude-haiku-5-5".into(),level:"max".into()}).unwrap();
        assert_eq!(a.ui_settings_value().background_efforts,[(t3.to_string(),"claude-haiku-5-5".to_string(),"low".to_string()),("anthropic".to_string(),"claude-haiku-5-5".to_string(),"max".to_string())]);
        assert_eq!(crate::background_model::effort_for(t3,"claude-haiku-5-5").as_deref(),Some("low"));
        a.action(ui::UiAction::SetBackgroundEffort{provider:t3.into(),id:"claude-haiku-5-5".into(),level:"high".into()}).unwrap();
        assert_eq!(a.settings.background_efforts.len(),2,"one level per model");
        a.action(ui::UiAction::SetBackgroundEffort{provider:"nowhere".into(),id:"x".into(),level:"low".into()}).unwrap();
        a.action(ui::UiAction::SetBackgroundEffort{provider:"anthropic".into(),id:"claude-haiku-5-5".into(),level:"two words".into()}).unwrap();
        assert_eq!(a.settings.background_efforts.len(),2,"refused picks change nothing");
        a.action(ui::UiAction::SetBackgroundEffort{provider:"anthropic".into(),id:"claude-haiku-5-5".into(),level:String::new()}).unwrap();
        assert_eq!(crate::background_model::effort_for("anthropic","claude-haiku-5-5"),None);
        let path=a.data.join("effort-settings.json");
        a.settings.save(&path).unwrap();
        let saved=crate::settings::Settings::load(&path).unwrap();
        assert_eq!(saved.background_efforts,a.settings.background_efforts);
        crate::background_model::publish_efforts(&[]);
        let t=tempfile::tempdir().unwrap();
        let _b=Pipeline::new(t.path().join("data"),t.path().join("missing-models"),saved,None,true).unwrap();
        assert_eq!(crate::background_model::effort_for(t3,"claude-haiku-5-5").as_deref(),Some("high"));
        crate::background_model::publish(&Default::default(),&[]);
        crate::background_model::publish_efforts(&[]);
    }
    // Background Tasks ▸ a task's own Model: the island agent's default, a pick, back to the
    // shared model; saved, shown, published; a relaunch publishes it; the shared model stays his.
    #[test]
    fn a_tasks_own_model_persists_and_reaches_the_menu_and_its_endpoint() {
        let (_temp,mut a)=app();
        let islands=ui::BackgroundTask::Islands;
        let default_ui:Vec<(ui::BackgroundTask,String,String)>=crate::background_model::default_task_models().iter().filter_map(|m|m.ui()).collect();
        assert_eq!(a.ui_settings_value().task_models,default_ui);
        a.action(ui::UiAction::SetBackgroundModel{provider:"anthropic".into(),id:"claude-sonnet-5-5".into()}).unwrap();
        a.action(ui::UiAction::SetTaskModel{task:islands,provider:"anthropic".into(),id:"claude-opus-5-5".into()}).unwrap();
        assert_eq!(crate::background_model::task_endpoint(islands).model.as_deref(),Some("claude-opus-5-5"));
        assert_eq!(crate::background_model::task_endpoint(ui::BackgroundTask::Plan).model.as_deref(),Some("claude-sonnet-5-5"));
        assert!(a.ui_settings_value().task_models.contains(&(islands,"anthropic".to_string(),"claude-opus-5-5".to_string())));
        a.action(ui::UiAction::SetTaskModel{task:islands,provider:"nowhere".into(),id:"x".into()}).unwrap();
        assert_eq!(crate::background_model::task_model(islands).id,"claude-opus-5-5","a model the menu would not offer is refused");
        a.action(ui::UiAction::SetTaskModel{task:islands,provider:String::new(),id:String::new()}).unwrap();
        // Islands leaves the list; Goals keeps its own default (31 Oct).
        assert!(!a.settings.task_models.iter().any(|m|m.task=="islands") && !a.ui_settings_value().task_models.iter().any(|m|m.0==islands));
        assert_eq!(crate::background_model::task_endpoint(islands).model.as_deref(),Some("claude-sonnet-5-5"),"back on the shared model");
        a.action(ui::UiAction::SetTaskModel{task:ui::BackgroundTask::VoiceNames,provider:"anthropic".into(),id:"claude-haiku-5-5".into()}).unwrap();
        let path=a.data.join("task-model-settings.json");
        a.settings.save(&path).unwrap();
        let saved=crate::settings::Settings::load(&path).unwrap();
        assert_eq!(saved.task_models,a.settings.task_models,"Islands stays on the shared model after a reload");
        crate::background_model::publish_tasks(&[]);
        let t=tempfile::tempdir().unwrap();
        let _b=Pipeline::new(t.path().join("data"),t.path().join("missing-models"),saved,None,true).unwrap();
        assert_eq!(crate::background_model::task_model(ui::BackgroundTask::VoiceNames).id,"claude-haiku-5-5");
        crate::background_model::publish(&Default::default(),&[]);
        crate::background_model::publish_tasks(&crate::background_model::default_task_models());
    }
    // [menu lane] Review fix: a relaunch publishes the saved background model before any menu change.
    #[test]
    fn a_relaunch_hands_the_saved_background_model_to_the_jobs() {
        let t=tempfile::tempdir().unwrap();
        let saved=crate::settings::Settings{background_model:crate::background_model::BackgroundModel{provider:"anthropic".into(),id:"claude-opus-5-5".into()},background_off:vec!["islands".into()],..Default::default()};
        let _a=Pipeline::new(t.path().join("data"),t.path().join("missing-models"),saved,None,true).unwrap();
        assert_eq!(crate::background_model::current().id,"claude-opus-5-5");
        assert_eq!(crate::plan_refresher::request_body("x")["model"],"claude-opus-5-5");
        assert!(!crate::background_model::enabled(ui::BackgroundTask::Islands));
        crate::background_model::publish(&Default::default(),&[]);
    }
    #[test]
    /// 30 Oct 01:2x: a connection is not a pick. Anthropic connected adds Claude Opus 5.5 to the list (again, if the user
    /// had removed it); the agent keeps its model until the user picks Opus himself.
    fn anthropic_connection_adds_opus_without_switching_and_the_slider_and_credit_persist() {
        use agent::provider::{AgentModel, ApiModel, ANTHROPIC, OPUS};
        let (_temp,mut a)=app();
        let p=agent::provider::provider(ANTHROPIC).unwrap();
        if std::env::var_os(p.key_env).is_some() { return; }
        agent::provider::forget_key(p).unwrap();
        let before=a.settings.agent_model.clone();
        // Without a key nothing is added.
        let mut next=a.settings.clone();next.api_models.retain(|m|m.id!=OPUS);a.apply_menu_settings(next).unwrap();
        a.action(ui::UiAction::AddProviderDefault{provider:ANTHROPIC.into()}).unwrap();
        assert!(!a.settings.api_models.iter().any(|m|m.key()==(ANTHROPIC,OPUS)));
        // Connected: Opus joins the list; the agent's model stays.
        agent::provider::save_key(p,"sk-ant-api03-lab-0123456789abcdefghij",None).unwrap();
        a.action(ui::UiAction::AddProviderDefault{provider:ANTHROPIC.into()}).unwrap();
        assert!(a.settings.api_models.iter().any(|m|m.key()==(ANTHROPIC,OPUS)));
        assert_eq!(a.settings.agent_model,before,"a connection never switches the agent's model");
        // The user's own pick switches it.
        a.action(ui::UiAction::SelectApiModel{provider:ANTHROPIC.into(),id:OPUS.into()}).unwrap();
        assert_eq!(a.settings.agent_model,AgentModel::Api{provider:ANTHROPIC.into(),id:OPUS.into()});
        assert!(!a.settings.engine_on);
        // The slider: a stop is kept, anything else snaps to the nearest stop.
        a.action(ui::UiAction::SetApiModelContext{provider:ANTHROPIC.into(),id:OPUS.into(),tokens:501_714}).unwrap();
        let opus=|a:&Pipeline| a.settings.api_models.iter().find(|m|m.key()==(ANTHROPIC,OPUS)).unwrap().clone();
        assert_eq!((opus(&a).context_budget,opus(&a).limits().context),(Some(501_714),501_714));
        a.action(ui::UiAction::SetApiModelContext{provider:ANTHROPIC.into(),id:OPUS.into(),tokens:130_000}).unwrap();
        assert_eq!(opus(&a).context_budget,Some(128_000));
        // Credits: $200 (Max 20x) is kept; a value the menu never offers is ignored.
        a.action(ui::UiAction::SetProviderCredit{provider:ANTHROPIC.into(),usd:200}).unwrap();
        a.action(ui::UiAction::SetProviderCredit{provider:ANTHROPIC.into(),usd:7}).unwrap();
        assert_eq!(a.settings.anthropic_credit_usd,200);
        let _=ApiModel::haiku();
        agent::provider::forget_key(p).unwrap();
    }
    /// Audit 27 Oct, F2: the account's list reaches rows added earlier (Sol at 272K, Terra as the placeholder); the user's
    /// thinking stays his, a refresh never raises his budget, and the selection is untouched.
    #[test]
    fn a_catalog_refresh_updates_added_chatgpt_rows_and_keeps_owner_s_choices() {
        use agent::provider::{Api, ApiModel, AgentModel, CHATGPT};
        let (_temp,mut a)=app();
        let row=|id:&str,context:u64,max_output:u64,budget:Option<u64>| ApiModel{provider:CHATGPT.into(),id:id.into(),name:id.into(),api:Api::OpenaiResponses,context,max_output,
            images:max_output>0,reasoning:true,cost:None,context_budget:budget,thinking:Some("high".into()),base_url:None,efforts:vec![]};
        let mut next=a.settings.clone();
        next.api_models.push(row("gpt-6.1-sol",272_000,128_000,Some(272_000)));
        next.api_models.push(row("gpt-6.1-terra",128_000,0,None));
        a.settings=next;
        let selected=a.settings.agent_model.clone();
        let fresh=|id:&str| ApiModel{efforts:vec!["low".into(),"medium".into(),"high".into(),"max".into()],..row(id,500_000,128_000,None)};
        a.tools.api.set_catalog(CHATGPT,vec![fresh("gpt-6.1-sol"),fresh("gpt-6.1-terra")]);
        a.test_persist_settings();
        a.action(ui::UiAction::RefreshApiModels{provider:CHATGPT.into()}).unwrap();
        // Saved, so a reload or crash restart starts Pi on the new figures (night review).
        let saved=crate::settings::Settings::load(&a.data.join("settings.json")).unwrap();
        assert_eq!(saved.api_models.iter().find(|m|m.id=="gpt-6.1-terra").map(|m|m.context),Some(500_000),"the refresh is on disk");
        let get=|a:&Pipeline,id:&str| a.settings.api_models.iter().find(|m|m.id==id).unwrap().clone();
        let (sol,terra)=(get(&a,"gpt-6.1-sol"),get(&a,"gpt-6.1-terra"));
        assert_eq!((sol.context,sol.budget(),sol.thinking.as_deref()),(500_000,272_000,Some("high")),"kept exactly: his 272K stays");
        assert_eq!((terra.context,terra.max_output,terra.images,terra.context_steps().len(),terra.budget()),(500_000,128_000,true,4,500_000),"an untouched slider takes the whole window");
        assert_eq!(a.settings.agent_model,selected);
        assert!(matches!(selected,AgentModel::Api{..}|AgentModel::Local|AgentModel::Unchosen));
        // The slider shows his 272K as a stop, and moving back onto it keeps it (night review: it saved 252K).
        a.action(ui::UiAction::SetApiModelContext{provider:CHATGPT.into(),id:"gpt-6.1-sol".into(),tokens:271_000}).unwrap();
        assert_eq!(get(&a,"gpt-6.1-sol").budget(),272_000);
    }
    #[test]
    fn island_telemetry_never_persists_settings_or_refreshes_runtime_ui() {
        let (_temp, mut a) = app();
        a.test_persist_settings();
        let path = a.data.join("settings.json");
        std::fs::write(&path, b"settings untouched by telemetry").unwrap();
        let before = a.hooks.ui.lock().unwrap().len();
        for report in [
            ui::IslandReport::Shown { id: Some(42), phase: "done".into(), kind: "command".into(), label: "Screenshot".into(), waited_ms: 0 },
            ui::IslandReport::Cut { id: Some(42), phase: "done".into(), kind: "command".into(), label: "Screenshot".into(), shown_ms: 700, minimum_ms: 700, why: "expired".into() },
        ] { a.action(ui::UiAction::Island(report)).unwrap(); }
        assert_eq!(std::fs::read(&path).unwrap(), b"settings untouched by telemetry");
        assert_eq!(a.hooks.ui.lock().unwrap().len(), before);
        a.evidence.flush().unwrap();
        let records = a.index.evidence.records().unwrap();
        assert_eq!(records.iter().filter(|r| r.kind == "island_report").count(), 2);
    }
    /// A setting applied live is saved even when its journal entry fails (evidence writer down, below the disk floor):
    /// `apply_menu_settings`'s journal error used to return before the action's save, so the file kept the old value.
    #[test]
    fn a_failed_settings_journal_entry_still_saves_the_setting() {
        let (_temp, mut a) = app();
        a.test_persist_settings();
        // No disk can satisfy this floor: the journal refuses `settings_applied`.
        a.settings.free_space_floor_bytes = u64::MAX / 4;
        let wake = !a.settings.wake_sound;
        let result = a.action(ui::UiAction::SetWakeSound(wake));
        assert_eq!(a.settings.wake_sound, wake);
        let saved = Settings::load(&a.data.join("settings.json")).unwrap();
        assert_eq!(saved.wake_sound, wake, "applied live but never saved");
        result.unwrap();
    }
    /// A refused Seeing click (here an invalid floor fails validation) redraws the menu from the real settings: the
    /// tile no longer stays on the state it showed when clicked.
    #[test]
    fn a_refused_seeing_click_resends_the_real_runtime_settings() {
        let (_temp, mut a) = app();
        a.settings.free_space_floor_bytes = 0;
        let seeing = !a.settings.screen_off;
        let e = a.set_seeing(!seeing).unwrap_err();
        a.hooks.ui.lock().unwrap().clear();
        crate::live_loop::screen_setting_rejected(&mut a, "Seeing", &e);
        let sent: Vec<_> = a.hooks.ui.lock().unwrap().iter().filter_map(|u| match u { ui::UiUpdate::RuntimeSettings(r) => Some(r.seeing), _ => None }).collect();
        assert_eq!(sent, [seeing], "the menu kept the clicked state");
    }
    /// Agent ▸ Model ▸ Claude Code subscription (docs/claude-relay.md): picking it turns the relay on and keeps the model; the
    /// menu's state says so; any model row turns it off with that model.
    #[test]
    fn claude_via_t3_keeps_the_model_and_any_model_row_ends_it() {
        let (_temp,mut a)=app();
        let model=a.settings.agent_model.clone();
        a.action(ui::UiAction::SelectAgentRelay).unwrap();
        assert!(a.settings.agent_relay && a.settings.agent_model==model, "the relay on, the model kept");
        assert!(a.ui_settings_value().agent_relay, "the menu draws its check");
        a.action(ui::UiAction::SelectLocalAgent).unwrap();
        assert!(!a.settings.agent_relay && a.settings.agent_model.is_local(), "a model row (This Mac's) ends the relay");
        a.action(ui::UiAction::SelectAgentRelay).unwrap();
        let m=agent::provider::ApiModel::haiku();
        a.settings.api_models=vec![m.clone()];
        a.action(ui::UiAction::SelectApiModel{provider:m.provider.clone(),id:m.id.clone()}).unwrap();
        // Without a stored key the pick is refused and the relay stays; with one it ends the relay.
        assert_eq!(a.settings.agent_relay, a.settings.agent_model.api().is_none(), "an API model row ends it too");
    }
    /// 28 Oct 17:26-17:28: switching back and forth lands on what was confirmed. Claude Code subscription ▸ a model turns the relay
    /// on with that T3 model (never the model clicked before it); a pick the app refuses sends the menu the real state
    /// back, so its radio returns to what runs; the provider default chosen on a first connection ends the relay too.
    #[test]
    fn relay_model_picks_land_on_what_was_confirmed_and_a_refused_pick_resends_the_truth() {
        let (_temp,mut a)=app();
        let model=a.settings.agent_model.clone();
        a.action(ui::UiAction::SelectRelayModel("claude-sonnet-5-5".into())).unwrap();
        assert!(a.settings.agent_relay && a.settings.agent_relay_model.as_deref()==Some("claude-sonnet-5-5") && a.settings.agent_model==model);
        a.action(ui::UiAction::SelectRelayModel("claude-opus-5-5".into())).unwrap();
        assert_eq!(a.settings.agent_relay_model.as_deref(), Some("claude-opus-5-5"), "another T3 model moves the relay");
        // A model without a stored key is refused: the relay stays, and the menu hears so at once.
        let m=agent::provider::ApiModel::haiku();
        a.settings.api_models=vec![m.clone()];
        if !m.provider_info().is_some_and(agent::provider::has_credential) {
            a.hooks.ui.lock().unwrap().clear();
            a.action(ui::UiAction::SelectApiModel{provider:m.provider.clone(),id:m.id.clone()}).unwrap();
            assert!(a.settings.agent_relay, "refused: the relay stays");
            let told: Vec<bool> = a.hooks.ui.lock().unwrap().iter().filter_map(|u| match u { ui::UiUpdate::Settings(s) => Some(s.agent_relay), _ => None }).collect();
            assert_eq!(told, [true], "the menu is told what runs");
        }
    }
    /// Agent ▸ Model ▸ Claude Code subscription ▸ an account (30 Oct 06:34): the relay on that T3 Claude instance with its model
    /// kept, Automatic back to `None`; the menu's state carries it; the same pick again changes nothing.
    #[test]
    fn relay_account_picks_turn_the_relay_on_and_keep_its_model() {
        let (_temp,mut a)=app();
        let model=a.settings.agent_model.clone();
        a.action(ui::UiAction::SelectRelayModel("claude-sonnet-5-5".into())).unwrap();
        a.action(ui::UiAction::SelectRelayAccount(Some("claudeAgent_claude_code_2".into()))).unwrap();
        assert!(a.settings.agent_relay && a.settings.agent_model==model);
        assert_eq!((a.settings.agent_relay_account.as_deref(), a.settings.agent_relay_model.as_deref()), (Some("claudeAgent_claude_code_2"), Some("claude-sonnet-5-5")));
        assert_eq!(a.ui_settings_value().agent_relay_account.as_deref(), Some("claudeAgent_claude_code_2"), "the menu draws its check");
        a.hooks.ui.lock().unwrap().clear();
        a.action(ui::UiAction::SelectRelayAccount(Some("claudeAgent_claude_code_2".into()))).unwrap();
        assert!(a.hooks.ui.lock().unwrap().is_empty(), "the account it runs on: nothing to apply");
        a.action(ui::UiAction::SelectRelayAccount(None)).unwrap();
        assert_eq!(a.settings.agent_relay_account, None, "Automatic");
        // Off, an account pick turns the relay on, as a model pick does.
        a.action(ui::UiAction::SelectLocalAgent).unwrap();
        assert!(!a.settings.agent_relay);
        a.action(ui::UiAction::SelectRelayAccount(Some("claudeAgent".into()))).unwrap();
        assert!(a.settings.agent_relay && a.settings.agent_relay_account.as_deref()==Some("claudeAgent"));
    }
    /// "Stay on …" after a failure (30 Oct 06:34): relay mode, account and model land in one settings change, and the
    /// same choice again applies nothing (the relay is not restarted twice). A non-Claude account keeps the menu's model.
    #[test]
    fn staying_on_the_replacement_is_one_settings_change() {
        let (_temp,mut a)=app();
        a.action(ui::UiAction::SelectRelayAccount(Some("claudeAgent".into()))).unwrap();
        a.hooks.ui.lock().unwrap().clear();
        a.action(ui::UiAction::SelectRelayRoute { model: Some("claude-opus-5-5".into()), account: "claudeAgent_claude_code_2".into() }).unwrap();
        assert_eq!((a.settings.agent_relay, a.settings.agent_relay_account.as_deref(), a.settings.agent_relay_model.as_deref()), (true, Some("claudeAgent_claude_code_2"), Some("claude-opus-5-5")));
        a.hooks.ui.lock().unwrap().clear();
        a.action(ui::UiAction::SelectRelayRoute { model: Some("claude-opus-5-5".into()), account: "claudeAgent_claude_code_2".into() }).unwrap();
        assert!(a.hooks.ui.lock().unwrap().is_empty(), "already so: nothing applied");
        a.action(ui::UiAction::SelectRelayRoute { model: None, account: "codex".into() }).unwrap();
        assert_eq!((a.settings.agent_relay_account.as_deref(), a.settings.agent_relay_model.as_deref()), (Some("codex"), Some("claude-opus-5-5")));
    }
    /// Part 0 (listed in Cloud and in Add Provider at once): Add Provider's and Setup's "Claude Code
    /// subscription" adds the relay as a provider (kept across launches) and never makes it the agent; Remove puts it
    /// back, refused while it is the agent; picking it in the picker adds it too.
    #[test]
    fn the_claude_subscription_provider_row_adds_the_relay_without_switching() {
        let (temp,mut a)=app();
        let model=a.settings.agent_model.clone();
        assert!(!a.settings.relay_added && !a.ui_settings_value().relay_added);
        a.action(ui::UiAction::ClaudeViaT3).unwrap();
        assert!(a.settings.relay_added && !a.settings.agent_relay && a.settings.agent_model==model, "added, the agent untouched");
        assert!(a.ui_settings_value().relay_added, "the menu is told: its row in Cloud, gone from Add Provider");
        let path=temp.path().join("relay-added.json");
        a.settings.save(&path).unwrap();
        assert!(crate::settings::Settings::load(&path).unwrap().relay_added, "kept across launches");
        a.action(ui::UiAction::ClaudeViaT3).unwrap();
        assert!(a.settings.relay_added && !a.settings.agent_relay, "a second add changes nothing");
        a.action(ui::UiAction::SelectAgentRelay).unwrap();
        a.action(ui::UiAction::RemoveClaudeViaT3).unwrap();
        assert!(a.settings.relay_added && a.settings.agent_relay, "Remove is refused while it is the agent");
        a.action(ui::UiAction::SelectLocalAgent).unwrap();
        a.action(ui::UiAction::RemoveClaudeViaT3).unwrap();
        assert!(!a.settings.relay_added && !a.ui_settings_value().relay_added, "removed: back in Add Provider");
        a.action(ui::UiAction::SelectRelayModel("claude-opus-5-5".into())).unwrap();
        assert!(a.settings.relay_added, "picked in the picker: added too");
    }
    /// Nothing can cover (`emergency_local`): No shows the quiet waiting line with the reset; Yes picks the local
    /// model through the menu's path; a stale card's answer does nothing.
    #[test]
    fn the_no_relay_card_waits_or_switches_to_the_local_model() {
        use crate::emergency_local::{set_open, Offer, CARD_PREFIX, NO, YES};
        let (_temp,mut a)=app();
        let answer=|id:&str,choice:&str| ui::UiAction::AgentNotice(ui::agent_notice::AgentNoticeAction::Answer{request_id:id.into(),answers:vec![ui::agent_notice::QuestionAnswer{question_id:"q1".into(),labels:vec![choice.into()],other:None}], by_hand: vec![]});
        let model=crate::inference_engine::Config::bundled().default_model;
        let pick=|a:&Pipeline| (a.settings.agent_model.clone(),a.settings.engine_on,a.settings.engine_model.clone(),a.settings.agent_relay);
        let before=pick(&a);
        set_open(Some(Offer{request_id:format!("{CARD_PREFIX}1"),model:model.clone(),reset_ms:None}));
        a.action(answer(&format!("{CARD_PREFIX}1"),NO)).unwrap();
        assert_eq!(pick(&a),before, "No changes no setting");
        set_open(Some(Offer{request_id:format!("{CARD_PREFIX}2"),model:model.clone(),reset_ms:None}));
        a.action(answer(&format!("{CARD_PREFIX}9"),YES)).unwrap();
        assert_eq!(pick(&a),before, "another card's Yes does nothing");
        a.action(answer(&format!("{CARD_PREFIX}2"),YES)).unwrap();
        assert!(a.settings.agent_model.is_local() && a.settings.engine_on && a.settings.engine_model==model, "Yes: the local model, engine on");
    }
    /// 28 Oct 17:26: Add Provider ▸ Local Splash Engine sent `SetEngine(true)`, which made the local model the agent's
    /// and left the agent waiting while a 27B loaded. Adding the engine lists it under This Mac and changes nothing else.
    #[test]
    fn adding_the_built_in_engine_never_switches_the_agent_model() {
        let (_temp,mut a)=app();
        let before=(a.settings.agent_model.clone(),a.settings.engine_on,a.settings.engine_model.clone(),a.settings.agent_relay);
        assert!(!a.settings.engine_added && !a.ui_settings_value().engine_added);
        a.action(ui::UiAction::AddBuiltInEngine).unwrap();
        assert!(a.settings.engine_added && a.ui_settings_value().engine_added, "listed, and the menu is told");
        assert_eq!((a.settings.agent_model.clone(),a.settings.engine_on,a.settings.engine_model.clone(),a.settings.agent_relay),before,
            "the agent's model, the engine and the relay stay as they were");
        // Adding twice is a no-op; engine-model actions on precisions the catalog does not offer change nothing.
        let added=a.settings.clone();
        for action in [ui::UiAction::AddBuiltInEngine,
            ui::UiAction::SetEngineQuant{id:"qwen3.8-27b".into(),quant:"UD-Q8_K_XL".into()},
            ui::UiAction::DownloadEngineModel{id:"qwen3.8-27b".into(),quant:"UD-Q4_K_M".into(),then_select:true},
            ui::UiAction::CancelEngineDownload{id:"qwen3.8-27b".into()},
            ui::UiAction::DeleteEngineModel{id:"qwen3.8-27b".into(),quant:"UD-Q4_K_M".into()}] {
            a.action(action).unwrap();
        }
        assert_eq!(a.settings,added);
        // Remove: back in Add Provider with the engine off; refused while the local model is the agent's.
        a.action(ui::UiAction::RemoveBuiltInEngine).unwrap();
        if added.agent_model.is_local() { assert!(a.settings.engine_added, "refused while the agent runs on it"); }
        else { assert!(!a.settings.engine_added && !a.settings.engine_on && a.settings.agent_model==added.agent_model); }
    }
    /// The Engine tile runs or stops the engine; the agent's model stays whatever it was.
    #[test]
    fn the_engine_tile_never_switches_the_agent_model() {
        let (_temp,mut a)=app();
        let agent=a.settings.agent_model.clone();
        assert!(!agent.is_local(), "starts on an API model (or none chosen)");
        a.action(ui::Tile::Engine.toggle(false)).unwrap();
        assert!(a.settings.engine_on && a.settings.engine_added);
        assert_eq!(a.settings.agent_model,agent);
        a.action(ui::Tile::Engine.toggle(true)).unwrap();
        assert!(!a.settings.engine_on && a.settings.engine_added);
        assert_eq!(a.settings.agent_model,agent);
    }
    /// The relay's T3 model reaches the menu's settings.
    #[test]
    fn the_relay_model_reaches_the_menu() {
        let (_temp,mut a)=app();
        assert_eq!(a.ui_settings_value().agent_relay_model,None);
        let mut next=a.settings.clone();next.agent_relay_model=Some("claude-opus-5-5".into());a.apply_menu_settings(next).unwrap();
        assert_eq!(a.ui_settings_value().agent_relay_model.as_deref(),Some("claude-opus-5-5"));
    }

