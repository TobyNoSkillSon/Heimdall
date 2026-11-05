    use super::*;
    use agent::delivery::StopReason;
    use serde_json::{json, Value};

    /// The live loop's side for the `settings` tool's round trip: a fake Mac's microphones and playing apps, and each
    /// action handled where live_loop.rs handles the menu's (the latches first, as `ui_action_callback` does).
    struct Rig {
        _t: tempfile::TempDir,
        a: Pipeline,
        seen: capture::MicrophoneState,
        heard: Vec<settings::HeardApp>,
    }

    fn rig() -> Rig {
        let (t, mut a) = app();
        let dev = |uid: &str, name: &str, built_in: bool| capture::InputDevice { uid: uid.into(), name: name.into(), built_in, default: false };
        let seen = capture::MicrophoneState {
            devices: vec![dev("BuiltIn", "MacBook Pro Microphone", true), dev("usb-1", "Shure MV7", false)],
            in_use: Some("BuiltIn".into()),
            ..Default::default()
        };
        let ego = capture::AppIdentity { key: "com.ego.browser".into(), name: "Ego Browser".into(), path: "/Applications/Ego Browser.app".into() };
        let heard = vec![settings::HeardApp { app: ego, playing: true }];
        a.menu_microphones(&seen);
        a.menu_apps(&heard);
        Rig { _t: t, a, seen, heard }
    }

    impl Rig {
        fn get(&self, key: &str, item: Option<&str>) -> Value {
            self.a.settings_view().get(key, item).unwrap()
        }
        /// `settings set`: planned on the view, every action dispatched as the menu's.
        fn set(&mut self, key: &str, item: Option<&str>, value: Value) -> Result<()> {
            let plan = crate::settings_tool::plan(&self.a.settings_view(), key, item, &value, None)?;
            for action in plan.actions {
                self.dispatch(action)?;
            }
            Ok(())
        }
        fn dispatch(&mut self, action: ui::UiAction) -> Result<()> {
            let a = &mut self.a;
            match action {
                ui::UiAction::SelectMicrophone { id } => { a.settings.choose_microphone(&id, &self.seen); a.menu_microphones(&self.seen); }
                ui::UiAction::SetMicrophoneOn { id, on } => { a.settings.set_microphone_on(&id, on, &self.seen); a.menu_microphones(&self.seen); }
                ui::UiAction::SetAppRecorded { id, on } => { a.settings.set_app_recorded(&id, on, &mut self.heard); a.menu_apps(&self.heard); }
                ui::UiAction::SetSeeing(on) => a.set_seeing(on)?,
                ui::UiAction::SetListening(on) => { a.tools.menu_intent(StopReason::Heimdall, on); a.heimdall_intent_processed()?; }
                ui::UiAction::SetScreenRecording(choice) => {
                    let mut next = a.settings.clone();
                    next.screen = settings::ScreenSettings::from_ui(choice);
                    a.apply_menu_settings(next)?;
                }
                other => a.action(other)?,
            }
            Ok(())
        }
        /// The last menu settings the app sent (what the menu's rows redraw from).
        fn menu(&self) -> ui::Settings {
            self.a.hooks.ui.lock().unwrap().iter().rev().find_map(|u| match u { ui::UiUpdate::Settings(s) => Some(s.clone()), _ => None }).unwrap()
        }
        fn runtime(&self) -> ui::RuntimeSettings {
            self.a.hooks.ui.lock().unwrap().iter().rev().find_map(|u| match u { ui::UiUpdate::RuntimeSettings(s) => Some(s.clone()), _ => None }).unwrap()
        }
    }

    /// Tiles: Speaking and Seeing change, the view reads them back and the menu's update carries them; a word that
    /// is not on or off sends nothing.
    #[test]
    fn settings_tool_tiles_reach_the_menu() {
        let mut r = rig();
        r.set("speaking", None, json!(false)).unwrap();
        assert_eq!((r.get("speaking", None), r.menu().speak_replies, r.a.settings.speak_replies), (json!(false), false, false));
        r.set("speaking", None, json!("on")).unwrap();
        assert!(r.menu().speak_replies);
        r.set("seeing", None, json!(false)).unwrap();
        assert_eq!((r.get("seeing", None), r.runtime().seeing), (json!(false), false));
        r.set("listening", None, json!(false)).unwrap();
        assert_eq!((r.get("listening", None), r.a.settings.paused), (json!(false), true));
        r.set("listening", None, json!(true)).unwrap();
        assert_eq!(r.get("listening", None), json!(true));
        let before = r.a.settings.clone();
        assert!(r.set("speaking", None, json!("loudly")).is_err());
        assert_eq!(r.a.settings, before);
    }

    /// Microphones and App Audio by the names the menu lists: chosen, read back, and in the lists the menu redraws.
    #[test]
    fn settings_tool_devices_and_app_audio_by_name() {
        let mut r = rig();
        r.set("microphone", None, json!("shure mv7")).unwrap();
        assert_eq!(r.a.settings.microphone.as_ref().map(|m| m.uid.as_str()), Some("usb-1"));
        assert_eq!(r.a.menu_microphones(&r.seen.clone()).primary.as_deref(), Some("usb-1"));
        assert_eq!(r.get("microphone", None), json!("Shure MV7"));
        r.set("microphone", None, json!("built-in")).unwrap();
        r.set("extra_microphone", Some("Shure"), json!(true)).unwrap();
        assert_eq!(r.get("extra_microphone", None), json!(["Shure MV7"]));
        assert!(r.set("microphone", None, json!("Blue Yeti")).is_err());
        // "Enable app audio from Ego browser."
        r.set("app_audio", Some("Ego browser"), json!(true)).unwrap();
        assert_eq!(r.a.settings.recorded_app_keys(), ["com.ego.browser"]);
        assert!(r.a.menu_apps(&r.heard.clone()).apps.iter().any(|a| a.id == "com.ego.browser" && a.recorded));
        assert_eq!(r.get("app_audio", Some("ego")), json!(true));
        r.set("app_audio", Some("Ego"), json!(false)).unwrap();
        assert!(r.a.settings.recorded_app_keys().is_empty());
        assert!(r.set("app_audio", Some("Safari"), json!(true)).is_err());
    }

    /// Seeing ▸ Recording, Show Islands on Streams, Show Islands: changed, read back, in the menu's update.
    #[test]
    fn settings_tool_seeing_and_windows_reach_the_menu() {
        let mut r = rig();
        r.set("recording_resolution", None, json!("720p")).unwrap();
        r.set("recording_fps", None, json!(10)).unwrap();
        assert_eq!(r.runtime().screen, ui::ScreenRecording { resolution: ui::ScreenResolution::P720, fps: 10 });
        assert_eq!((r.get("recording_resolution", None), r.get("recording_fps", None)), (json!("720p"), json!(10)));
        assert!(r.set("recording_fps", None, json!(240)).is_err());
        r.set("islands_on_streams", None, json!(true)).unwrap();
        assert!(r.menu().islands_on_streams && r.get("islands_on_streams", None) == json!(true));
        r.set("islands_on_streams", None, json!(false)).unwrap();
        r.set("show_islands", None, json!(false)).unwrap();
        assert!(!r.menu().islands_shown);
        r.set("show_islands", None, json!(true)).unwrap();
        assert!(r.menu().islands_shown && r.get("show_islands", None) == json!(true));
    }

    /// Personalisation: the voice speed, the wake sound, the voice and the mark through the menu's own actions.
    #[test]
    fn settings_tool_personalisation_reaches_the_menu() {
        let mut r = rig();
        r.set("voice_speed", None, json!(130)).unwrap();
        assert_eq!((r.menu().voice_speed_percent, r.get("voice_speed", None)), (130, json!(130)));
        assert!(r.set("voice_speed", None, json!(300)).is_err());
        assert_eq!(r.a.settings.voice_speed_percent, 130);
        r.set("wake_sound", None, json!(false)).unwrap();
        assert!(!r.menu().wake_sound && r.get("wake_sound", None) == json!(false));
        let svg = r.a.data.join("mark.svg");
        std::fs::create_dir_all(&r.a.data).unwrap();
        std::fs::write(&svg, r#"<svg viewBox="0 0 16 16"><circle cx="8" cy="8" r="6"/></svg>"#).unwrap();
        r.set("icon", None, json!(svg.display().to_string())).unwrap();
        let shown = r.a.hooks.ui.lock().unwrap().iter().rev().find_map(|u| match u { ui::UiUpdate::Identity(i) => Some(i.clone()), _ => None });
        assert_eq!(shown.map(|i| i.icon), Some(ui::identity::Icon::Image { path: svg.clone() }));
        assert_eq!(r.get("icon", None), json!(svg.display().to_string()));
        assert!(r.set("icon", None, json!("relative/mark.svg")).is_err());
        assert!(r.set("name", None, json!("Ivy!")).is_err());
        assert_eq!(r.a.settings.identity.name, "Heimdall");
        r.set("speaking_model", None, json!("phonon")).unwrap();
        assert_eq!((r.a.settings.speech_voice.local, r.get("speaking_model", None)), (crate::speech_voice::LocalModel::Phonon, json!("phonon")));
        assert!(r.set("speaking_model", None, json!("drama4")).is_err());
        if let Some(preset) = r.a.settings_view().presets.iter().find(|p| **p != r.a.settings.speech_voice.phonon_preset).cloned() {
            r.set("voice", None, json!(preset.to_lowercase())).unwrap();
            assert_eq!(r.a.settings.speech_voice.phonon_preset, preset);
        }
    }

    /// `ack_delay` goes the menu's way (the action, `apply_menu_settings`, the save) and is live at
    /// once: the next ack and island hold read it.
    #[test]
    fn settings_tool_ack_delay_applies_live_and_persists() {
        let mut r = rig();
        r.a.test_persist_settings();
        assert_eq!(r.get("ack_delay", None), json!(0));
        r.set("ack_delay", None, json!(2.5)).unwrap();
        assert_eq!((r.a.settings.ack_delay_ms, r.get("ack_delay", None)), (2_500, json!(2.5)));
        let saved = settings::Settings::load(&r.a.data.join("settings.json")).unwrap();
        assert_eq!(saved.ack_delay_ms, 2_500, "saved as a menu change is");
        assert!(r.set("ack_delay", None, json!(11)).is_err());
        assert_eq!(r.a.settings.ack_delay_ms, 2_500);
    }

    /// Claude Code subscription through the tool: on, its model and its account (Automatic too) through the
    /// menu's actions, read back and in the menu's update; what T3 does not list sends nothing; off returns to the model.
    #[test]
    fn settings_tool_claude_via_t3_model_and_account() {
        let mut r = rig();
        r.a.tools.relay_catalog.set(
            vec![ui::RelayModel { id: "claude-opus-5-5".into(), name: "Claude Opus 5.5".into() }, ui::RelayModel { id: "claude-sonnet-5-5".into(), name: "Claude Sonnet 5.5".into() }],
            vec![ui::RelayAccount { id: "claudeAgent".into(), name: "Claude".into() }, ui::RelayAccount { id: "claudeAgent_claude_code_2".into(), name: "Claude (work)".into() }]);
        let model = r.a.settings.agent_model.clone();
        r.set("agent_relay", None, json!(true)).unwrap();
        assert_eq!((r.get("agent_relay", None), r.menu().agent_relay, r.a.settings.agent_model == model), (json!(true), true, true));
        r.set("agent_relay_model", None, json!("Sonnet")).unwrap();
        assert_eq!((r.get("agent_relay_model", None), r.menu().agent_relay_model.as_deref()), (json!("claude-sonnet-5-5"), Some("claude-sonnet-5-5")));
        r.set("agent_relay_account", None, json!("claudeAgent_claude_code_2")).unwrap();
        assert_eq!((r.get("agent_relay_account", None), r.menu().agent_relay_account.as_deref()), (json!("claudeAgent_claude_code_2"), Some("claudeAgent_claude_code_2")));
        r.set("agent_relay_account", None, json!("automatic")).unwrap();
        assert_eq!((r.get("agent_relay_account", None), r.a.settings.agent_relay_account.clone()), (json!("automatic"), None));
        let before = r.a.settings.clone();
        assert!(r.set("agent_relay_account", None, json!("claudeAgent_3")).is_err());
        assert!(r.set("agent_relay_model", None, json!("claude-haiku-9")).is_err());
        assert_eq!(r.a.settings, before, "refused: nothing sent");
        r.a.settings.agent_model = agent::provider::AgentModel::Local;
        r.set("agent_relay", None, json!(false)).unwrap();
        assert_eq!((r.get("agent_relay", None), r.menu().agent_relay), (json!(false), false));
        assert_eq!(r.a.settings.agent_relay_model.as_deref(), Some("claude-sonnet-5-5"), "kept for the next time it runs");
    }

    /// Background Tasks and the agent's thinking: changed, read back, in the menu's update; bad values send nothing.
    #[test]
    fn settings_tool_background_and_agent_reach_the_menu() {
        let mut r = rig();
        r.set("background_task", Some("Voice Names"), json!(false)).unwrap();
        assert_eq!(r.menu().background_off, [ui::BackgroundTask::VoiceNames]);
        assert_eq!(r.get("background_task", Some("voice_names")), json!(false));
        r.set("background_task", Some("voice_names"), json!(true)).unwrap();
        assert!(r.menu().background_off.is_empty());
        r.set("background_model", None, json!("anthropic/claude-sonnet-5-5")).unwrap();
        assert_eq!(r.menu().background_model, ("anthropic".to_string(), "claude-sonnet-5-5".to_string()));
        assert!(r.set("background_model", None, json!("nowhere/x")).is_err());
        r.set("background_model", None, json!("anthropic/claude-haiku-5-5")).unwrap();
        r.a.settings.agent_model = agent::provider::AgentModel::Local;
        r.a.settings.engine_model = crate::inference_engine::Config::bundled().default_model;
        r.set("agent_thinking", None, json!("high")).unwrap();
        assert_eq!((r.menu().engine_thinking.as_str(), r.get("agent_thinking", None)), ("high", json!("high")));
        assert!(r.set("agent_thinking", None, json!("max")).is_err(), "the local model has no max");
    }
