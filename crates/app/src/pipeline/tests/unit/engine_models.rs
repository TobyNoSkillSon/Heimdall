    //! The Engine section's models and precisions through the pipeline (pipeline/engine_models.rs): fixture installations
    //! in the test's data folder and a fake installer script; no real download, cache or engine.
    use super::*;
    use crate::inference_engine::models::{fixture, Installer};
    use std::time::Duration;

    const QWEN: &str = "qwen3.8-27b";
    const DRAFT: &str = "incoai/Qwen3.8-27B-DFlash2";
    fn source(id: &str, quant: &str) -> String {
        crate::inference_engine::Config::bundled().quant(id, quant).unwrap().source.clone()
    }
    fn last_engine(a: &Pipeline) -> (ui::EngineState, Vec<ui::EngineModel>, Option<u64>) {
        a.hooks.ui.lock().unwrap().iter().rev().find_map(|u| match u {
            ui::UiUpdate::Engine { state, models, memory_limit_bytes, .. } => Some((state.clone(), models.clone(), *memory_limit_bytes)),
            _ => None,
        }).expect("an engine update")
    }
    fn row(a: &Pipeline, id: &str) -> ui::EngineModel {
        last_engine(a).1.into_iter().find(|m| m.id == id).unwrap()
    }
    fn downloaded(a: &Pipeline, id: &str, quant: &str) -> bool {
        row(a, id).quants.iter().find(|q| q.quant == quant).unwrap().downloaded
    }
    /// Give the pipeline a fake installer that "downloads" `source` by linking an assembly built beforehand.
    fn fake(a: &mut Pipeline, dir: &std::path::Path, mode: &str, source: &str) {
        let roots = a.engine_models_mut().roots.clone();
        let assembly = fixture::install_quant(&roots, source, DRAFT);
        std::fs::remove_file(roots.models.join(source)).unwrap();
        let script = crate::inference_engine::models_tests::fake_installer(dir, mode, Some(&assembly));
        a.engine_models_mut().installer = Installer::Program(script);
        a.engine_refresh();
    }
    fn wait_download(a: &mut Pipeline) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while a.engine_downloading().is_some() {
            assert!(Instant::now() < deadline, "the fake download did not end");
            a.engine_download_events();
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// "dont download them just add in setting". A saved engine-on whose precision is not here never
    /// starts (Splash would download it), the tile says why; Add Provider and the menu opening download nothing; the
    /// rows list every precision with its disk state and the 48 GB ceiling.
    #[test]
    fn nothing_downloads_at_startup_or_on_add_provider_and_rows_show_the_disk() {
        let t = tempfile::tempdir().unwrap();
        let settings = Settings { engine_on: true, engine_model: QWEN.into(), agent_model: agent::provider::AgentModel::Local, ..Default::default() };
        let mut a = Pipeline::new(t.path().join("data"), t.path().join("models"), settings, None, true).unwrap();
        assert_eq!(a.engine_startup(), (false, QWEN.into(), "UD-Q4_K_XL".into()), "off until the weights are here");
        a.initial_ui();
        let (state, models, limit) = last_engine(&a);
        // On with its pick missing: waiting for a pick (30 Oct), not a fault, and nothing loads or downloads.
        assert_eq!(state, ui::EngineState::Waiting);
        assert_eq!((models.len(), models.iter().map(|m| m.quants.len()).sum::<usize>(), limit), (6, 24, Some(48 << 30)));
        assert!(models.iter().all(|m| m.download.is_none() && m.quants.iter().all(|q| !q.downloaded && q.size_bytes.is_some() && q.ram_bytes.is_some())));
        for action in [ui::UiAction::AddBuiltInEngine, ui::UiAction::RefreshApiCatalog, ui::UiAction::SetEngine(true)] {
            a.action(action).unwrap();
            assert!(a.engine_downloading().is_none());
        }
        assert_eq!(a.engine_sent(), Some((false, QWEN.into(), "UD-Q4_K_XL".into())), "the retry does not start it either");
        // Weights appear (another download, a restore): the menu opening sees them and the saved intent starts it.
        fixture::install_quant(&a.engine_models_mut().roots.clone(), &source(QWEN, "UD-Q4_K_XL"), DRAFT);
        a.action(ui::UiAction::RefreshApiCatalog).unwrap();
        assert!(downloaded(&a, QWEN, "UD-Q4_K_XL") && !downloaded(&a, QWEN, "Q8_0"));
        assert_eq!(a.engine_sent(), Some((true, QWEN.into(), "UD-Q4_K_XL".into())));
    }

    /// The menu's Confirm sends SetEngineQuant, then SelectEngineModel. A precision not on this Mac downloads (the
    /// fake installer) and becomes the agent's model when it lands; the agent keeps its model meanwhile.
    #[test]
    fn confirming_a_missing_precision_downloads_it_then_selects_it() {
        let (t, mut a) = app();
        let agent = a.settings.agent_model.clone();
        fake(&mut a, t.path(), "ok", &source(QWEN, "Q8_0"));
        a.action(ui::UiAction::SetEngineQuant { id: QWEN.into(), quant: "Q8_0".into() }).unwrap();
        a.action(ui::UiAction::SelectEngineModel(QWEN.into())).unwrap();
        assert_eq!(a.engine_downloading(), Some((QWEN.into(), "Q8_0".into(), true)));
        assert_eq!(a.settings.agent_model, agent, "nothing switches before the weights are here");
        assert_eq!(a.settings.engine_quant.get(QWEN), None, "the saved precision changes only when the weights land");
        assert!(row(&a, QWEN).download.is_some_and(|d| d.quant == "Q8_0" && d.error.is_none()));
        wait_download(&mut a);
        assert!(a.settings.agent_model.is_local() && a.settings.engine_on && a.settings.engine_model == QWEN);
        assert_eq!(a.engine_sent(), Some((true, QWEN.into(), "Q8_0".into())));
        assert!(downloaded(&a, QWEN, "Q8_0") && row(&a, QWEN).download.is_none());
        assert_eq!(a.settings.engine_quant.get(QWEN).map(String::as_str), Some("Q8_0"));
        let args = std::fs::read_to_string(t.path().join("args")).unwrap();
        assert!(args.starts_with(&format!("pull {} --models-dir {}", source(QWEN, "Q8_0"), a.data.join("splash/models").display())), "{args}");
    }

    /// A pick of another model while a download runs wins: the download lands without switching the agent.
    #[test]
    fn a_pick_meanwhile_keeps_the_download_from_switching_the_agent() {
        let (t, mut a) = app();
        fake(&mut a, t.path(), "ok", &source("flint", "Q4_K_M"));
        a.action(ui::UiAction::DownloadEngineModel { id: "flint".into(), quant: "Q4_K_M".into(), then_select: true }).unwrap();
        a.action(ui::UiAction::SelectEngineModel(QWEN.into())).unwrap();
        assert_eq!(a.settings.engine_model, QWEN, "the installed default runs at once");
        wait_download(&mut a);
        assert_eq!(a.settings.engine_model, QWEN, "User's later pick stands");
        assert!(downloaded(&a, "flint", "Q4_K_M"));
        // A plain download (no then_select) never switches either.
        let before = a.settings.clone();
        a.action(ui::UiAction::DownloadEngineModel { id: "flint".into(), quant: "Q4_K_M".into(), then_select: false }).unwrap();
        assert!(a.engine_downloading().is_none() && a.settings == before, "already here: nothing to do");
    }

    /// A failed download shows Splash's reason on its row; Cancel stops one and clears the row; one at a time.
    #[test]
    fn a_failed_download_shows_its_reason_and_cancel_clears_it() {
        let (t, mut a) = app();
        fake(&mut a, t.path(), "fail", &source("flint", "Q8_0"));
        a.action(ui::UiAction::DownloadEngineModel { id: "flint".into(), quant: "Q8_0".into(), then_select: false }).unwrap();
        wait_download(&mut a);
        let d = row(&a, "flint").download.unwrap();
        assert_eq!((d.quant.as_str(), d.error.as_deref()), ("Q8_0", Some("the Hub refused the request (401)")));
        assert!(d.total_bytes.is_some(), "a download error carries a total");
        // Hang: a second download is refused while it runs; Cancel ends it.
        let dir = t.path().join("hang");
        std::fs::create_dir_all(&dir).unwrap();
        let script = crate::inference_engine::models_tests::fake_installer(&dir, "hang", None);
        a.engine_models_mut().installer = Installer::Program(script);
        a.action(ui::UiAction::DownloadEngineModel { id: "flint".into(), quant: "Q8_0".into(), then_select: false }).unwrap();
        assert!(row(&a, "flint").download.is_some_and(|d| d.error.is_none()), "the retry clears the old reason");
        a.action(ui::UiAction::DownloadEngineModel { id: "ornith-1.5-122b".into(), quant: "IQ2_XXS-UNCENSORED".into(), then_select: false }).unwrap();
        assert_eq!(row(&a, "ornith-1.5-122b").download.and_then(|d| d.error).as_deref(), Some("Another download is running (Flint 40B A2.5B Q8_0)"));
        a.action(ui::UiAction::CancelEngineDownload { id: "flint".into() }).unwrap();
        wait_download(&mut a);
        assert!(row(&a, "flint").download.is_none() && !downloaded(&a, "flint", "Q8_0"));
    }

    /// A precision switch of the running model restarts it on the new one when its weights are here; until then it
    /// keeps serving the old one. Delete is refused while the engine serves that precision, then removes it.
    #[test]
    fn a_quant_switch_restarts_only_with_weights_and_delete_refuses_the_serving_precision() {
        let (_t, mut a) = app();
        a.action(ui::UiAction::SelectEngineModel(QWEN.into())).unwrap();
        assert_eq!(a.engine_sent(), Some((true, QWEN.into(), "UD-Q4_K_XL".into())));
        a.action(ui::UiAction::SetEngineQuant { id: QWEN.into(), quant: "Q8_0".into() }).unwrap();
        assert_eq!(a.engine_sent(), Some((true, QWEN.into(), "UD-Q4_K_XL".into())), "Q8_0 is not here: the old one keeps serving");
        assert_eq!(a.settings.engine_quant.get(QWEN).map(String::as_str), Some("UD-Q4_K_XL"), "and stays saved");
        fixture::install_quant(&a.engine_models_mut().roots.clone(), &source(QWEN, "Q8_0"), DRAFT);
        a.action(ui::UiAction::RefreshApiCatalog).unwrap();
        a.action(ui::UiAction::SetEngineQuant { id: QWEN.into(), quant: "Q8_0".into() }).unwrap();
        a.action(ui::UiAction::SelectEngineModel(QWEN.into())).unwrap();
        assert_eq!(a.engine_sent(), Some((true, QWEN.into(), "Q8_0".into())), "restarted on the new precision");
        // Unknown precisions change nothing.
        a.action(ui::UiAction::SetEngineQuant { id: QWEN.into(), quant: "UD-Q4_K_M".into() }).unwrap();
        assert_eq!(a.settings.engine_quant.get(QWEN).map(String::as_str), Some("Q8_0"));
        // Serving Q8_0: its delete is refused with the reason on the row; the idle UD-Q4_K_XL deletes.
        a.action(ui::UiAction::DeleteEngineModel { id: QWEN.into(), quant: "Q8_0".into() }).unwrap();
        assert!(downloaded(&a, QWEN, "Q8_0"));
        let refused = row(&a, QWEN).download.unwrap();
        assert!(refused.error.is_some_and(|e| e.contains("using it")) && refused.quant == "Q8_0");
        assert_eq!(refused.total_bytes, None, "a delete refusal: no total (download errors always carry one)");
        a.action(ui::UiAction::RefreshApiCatalog).unwrap();
        assert!(row(&a, QWEN).download.is_none(), "the menu opening clears old reasons");
        a.action(ui::UiAction::DeleteEngineModel { id: QWEN.into(), quant: "UD-Q4_K_XL".into() }).unwrap();
        assert!(!downloaded(&a, QWEN, "UD-Q4_K_XL") && downloaded(&a, QWEN, "Q8_0"));
        a.action(ui::UiAction::SetEngine(false)).unwrap();
        a.action(ui::UiAction::DeleteEngineModel { id: QWEN.into(), quant: "Q8_0".into() }).unwrap();
        assert!(!downloaded(&a, QWEN, "Q8_0"));
        assert!(!a.data.join("hf-cache/hub/models--huihui-ai--Huihui-Qwen3.8-27B-abliterated-GGUF/blobs").read_dir().unwrap().any(|_| true), "its blobs went");
    }

    /// Settings from before precisions read with none chosen (the default); an old choice the catalog no longer offers
    /// serves the default; the map is written only once something is chosen.
    #[test]
    fn engine_quant_settings_migrate() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("settings.json");
        std::fs::write(&p, br#"{"engine_model":"qwen3.8-27b","engine_on":false}"#).unwrap();
        let s = Settings::load(&p).unwrap();
        assert!(s.engine_quant.is_empty());
        let config = crate::inference_engine::Config::bundled();
        assert_eq!(config.model(QWEN).unwrap().chosen(s.engine_quant_choice(QWEN)).source, source(QWEN, "UD-Q4_K_XL"));
        s.save(&p).unwrap();
        assert!(serde_json::from_slice::<serde_json::Value>(&std::fs::read(&p).unwrap()).unwrap().get("engine_quant").is_none());
        std::fs::write(&p, br#"{"engine_model":"qwen3.8-27b","engine_quant":{"qwen3.8-27b":"UD-Q4_K_M","flint":"Q6_K"}}"#).unwrap();
        let s = Settings::load(&p).unwrap();
        assert_eq!(config.model(QWEN).unwrap().chosen(s.engine_quant_choice(QWEN)).quant, "UD-Q4_K_XL");
        assert_eq!(config.model("flint").unwrap().chosen(s.engine_quant_choice("flint")).quant, "Q6_K");
        s.save(&p).unwrap();
        assert_eq!(Settings::load(&p).unwrap().engine_quant, s.engine_quant);
    }

    /// Review 5f550efc #4: a confirmed missing precision while another download runs is refused without saving it,
    /// and the earlier download still selects its model when it lands.
    #[test]
    fn a_refused_confirm_keeps_the_saved_precision_and_the_earlier_then_select() {
        let (t, mut a) = app();
        fake(&mut a, t.path(), "ok", &source("flint", "Q4_K_M"));
        a.action(ui::UiAction::DownloadEngineModel { id: "flint".into(), quant: "Q4_K_M".into(), then_select: true }).unwrap();
        a.action(ui::UiAction::SetEngineQuant { id: QWEN.into(), quant: "Q8_0".into() }).unwrap();
        a.action(ui::UiAction::SelectEngineModel(QWEN.into())).unwrap();
        assert_eq!(a.settings.engine_quant.get(QWEN), None, "nothing saved for the refused pick");
        assert!(row(&a, QWEN).download.and_then(|d| d.error).is_some_and(|e| e.contains("Another download is running (Flint 40B A2.5B")));
        assert_eq!(a.engine_downloading(), Some(("flint".into(), "Q4_K_M".into(), true)));
        wait_download(&mut a);
        assert!(a.settings.agent_model.is_local() && a.settings.engine_model == "flint", "the earlier pick lands");
        assert_eq!(a.engine_sent(), Some((true, "flint".into(), "Q4_K_M".into())));
    }

    /// Review 5f550efc #5: a confirmed pick needing more than the 48 GB ceiling neither downloads nor switches; the
    /// row says why. Its download button still downloads it, without switching.
    #[test]
    fn a_pick_over_the_memory_ceiling_is_refused_but_its_download_button_works() {
        let (t, mut a) = app();
        let (ornith, big) = ("ornith-1.5-122b", "Q4_K_M-UNCENSORED");
        let before = a.settings.clone();
        fake(&mut a, t.path(), "ok", &source(ornith, big));
        a.action(ui::UiAction::SetEngineQuant { id: ornith.into(), quant: big.into() }).unwrap();
        a.action(ui::UiAction::SelectEngineModel(ornith.into())).unwrap();
        assert!(a.engine_downloading().is_none());
        assert_eq!((&a.settings.agent_model, &a.settings.engine_model, &a.settings.engine_quant, a.settings.engine_on),
            (&before.agent_model, &before.engine_model, &before.engine_quant, before.engine_on), "nothing switched");
        let d = row(&a, ornith).download.unwrap();
        assert_eq!((d.quant.as_str(), d.error.as_deref()), (big, Some("Needs 80 GB of memory; the engine's limit is 48 GB")));
        a.action(ui::UiAction::DownloadEngineModel { id: ornith.into(), quant: big.into(), then_select: true }).unwrap();
        assert_eq!(a.engine_downloading(), Some((ornith.into(), big.into(), false)), "downloads, never selects itself");
        wait_download(&mut a);
        assert!(downloaded(&a, ornith, big) && a.settings.agent_model == before.agent_model);
        // On disk now, a pick is still refused.
        a.action(ui::UiAction::SetEngineQuant { id: ornith.into(), quant: big.into() }).unwrap();
        a.action(ui::UiAction::SelectEngineModel(ornith.into())).unwrap();
        assert!(a.settings.engine_quant.get(ornith).is_none() && a.settings.engine_model == before.engine_model);
    }


    /// Review 13e04dcb #1: while any download runs, no precision deletes (Splash locks the models root only when a
    /// pull publishes); the reason shows on the row.
    #[test]
    fn no_delete_while_any_download_runs() {
        let (t, mut a) = app();
        let script = crate::inference_engine::models_tests::fake_installer(t.path(), "hang", None);
        a.engine_models_mut().installer = Installer::Program(script);
        a.action(ui::UiAction::DownloadEngineModel { id: "flint".into(), quant: "Q8_0".into(), then_select: false }).unwrap();
        a.action(ui::UiAction::DeleteEngineModel { id: QWEN.into(), quant: "UD-Q4_K_XL".into() }).unwrap();
        assert!(downloaded(&a, QWEN, "UD-Q4_K_XL"));
        let d = row(&a, QWEN).download.unwrap();
        assert_eq!((d.error.as_deref(), d.total_bytes), (Some("A download is running (Flint 40B A2.5B Q8_0): wait for it or cancel it first"), None));
        a.action(ui::UiAction::CancelEngineDownload { id: "flint".into() }).unwrap();
        wait_download(&mut a);
        a.action(ui::UiAction::DeleteEngineModel { id: QWEN.into(), quant: "UD-Q4_K_XL".into() }).unwrap();
        assert!(!downloaded(&a, QWEN, "UD-Q4_K_XL"));
    }

    /// 30 Oct: the engine on with no model picked waits for the user (no model named, nothing running); a precision landing
    /// on disk meanwhile never loads by itself; his pick loads it and never touches the agent's model.
    #[test]
    fn engine_on_without_a_model_waits_and_a_pick_loads_it_without_touching_the_agent() {
        let (t, mut a) = app();
        let agent = a.settings.agent_model.clone();
        assert_eq!(a.settings.engine_model, "", "a new profile has no model picked");
        a.action(ui::UiAction::SetEngine(true)).unwrap();
        let waiting = a.hooks.ui.lock().unwrap().iter().rev().find_map(|u| match u { ui::UiUpdate::Engine { state, model_id, .. } => Some((state.clone(), model_id.clone())), _ => None });
        assert_eq!(waiting, Some((ui::EngineState::Waiting, String::new())), "on, no model: waiting, none named");
        assert!(a.engine_sent().is_some_and(|s| !s.0), "nothing runs");
        // Flint downloads and lands: it is on this Mac now, and still nothing loads.
        fake(&mut a, t.path(), "ok", &source("flint", "Q4_K_M"));
        a.action(ui::UiAction::DownloadEngineModel { id: "flint".into(), quant: "Q4_K_M".into(), then_select: false }).unwrap();
        wait_download(&mut a);
        assert!(downloaded(&a, "flint", "Q4_K_M"), "it joins the list");
        assert!(a.engine_sent().is_some_and(|s| !s.0) && last_engine(&a).0 == ui::EngineState::Waiting, "a landing loads nothing");
        assert_eq!(a.settings.agent_model, agent);
        // The user's pick loads it; the agent keeps its model.
        a.action(ui::UiAction::LoadEngineModel { id: "flint".into(), quant: "Q4_K_M".into() }).unwrap();
        assert_eq!(a.engine_sent(), Some((true, "flint".into(), "Q4_K_M".into())));
        assert!(a.settings.engine_on && a.settings.engine_model == "flint");
        assert_eq!(a.settings.agent_model, agent, "loading a model never switches the agent's");
        // Another pick of a precision not on this Mac is refused on its row; what runs stays.
        a.action(ui::UiAction::LoadEngineModel { id: QWEN.into(), quant: "Q8_0".into() }).unwrap();
        assert_eq!(a.engine_sent(), Some((true, "flint".into(), "Q4_K_M".into())));
        assert!(row(&a, QWEN).download.and_then(|d| d.error).is_some_and(|e| e.contains("download it first")));
        // The installed default pick switches the engine (off, then on the new one); still not the agent.
        let q4 = crate::inference_engine::Config::bundled().model(QWEN).unwrap().quant.clone();
        a.action(ui::UiAction::LoadEngineModel { id: QWEN.into(), quant: q4.clone() }).unwrap();
        assert_eq!(a.engine_sent(), Some((true, QWEN.into(), q4)));
        assert_eq!(a.settings.agent_model, agent);
    }

    /// The picked precision deleted (the engine off): no model is picked, so its weights coming back never load by
    /// themselves; an engine turned on then waits.
    #[test]
    fn deleting_the_picked_precision_clears_the_pick() {
        let (_t, mut a) = app();
        let q4 = crate::inference_engine::Config::bundled().model(QWEN).unwrap().quant.clone();
        a.action(ui::UiAction::LoadEngineModel { id: QWEN.into(), quant: q4.clone() }).unwrap();
        a.action(ui::UiAction::SetEngine(false)).unwrap();
        a.action(ui::UiAction::DeleteEngineModel { id: QWEN.into(), quant: q4.clone() }).unwrap();
        assert!(!downloaded(&a, QWEN, &q4));
        assert_eq!(a.settings.engine_model, "", "the pick goes with its weights");
        fixture::install_quant(&a.engine_models_mut().roots.clone(), &source(QWEN, &q4), DRAFT);
        a.action(ui::UiAction::SetEngine(true)).unwrap();
        a.action(ui::UiAction::RefreshApiCatalog).unwrap();
        assert!(a.engine_sent().is_some_and(|s| !s.0), "back on disk, still nothing loads until User picks it");
        assert_eq!(last_engine(&a).0, ui::EngineState::Waiting);
    }

    /// 30 Oct 01:2x: the main model back after the local stand-in is asked, never switched. The card's Yes picks it as
    /// the user's pick in the menu would; No or another card changes nothing.
    #[test]
    fn the_main_model_back_is_asked_and_only_a_yes_switches() {
        let (_t, mut a) = app();
        let answer = |id: &str, choice: &str| ui::UiAction::AgentNotice(ui::agent_notice::AgentNoticeAction::Answer { request_id: id.into(),
            answers: vec![ui::agent_notice::QuestionAnswer { question_id: "q1".into(), labels: vec![choice.into()], other: None }], by_hand: vec![] });
        a.action(ui::UiAction::SelectLocalAgent).unwrap();
        let card = crate::emergency_local::back_card("Claude Haiku 5.5", 42);
        let m = agent::provider::ApiModel::haiku();
        // The card as `offer_back` leaves it open.
        let set = || crate::emergency_local::set_back(Some(crate::emergency_local::Back { request_id: card.id.clone(), provider: m.provider.clone(), id: m.id.clone() }));
        set();
        a.action(answer(&card.id, crate::emergency_local::NO)).unwrap();
        assert!(a.settings.agent_model.is_local(), "No: the agent stays on the local engine");
        set();
        a.action(answer("emergency-back:7", crate::emergency_local::YES)).unwrap();
        assert!(a.settings.agent_model.is_local(), "another card's Yes does nothing");
        a.settings.api_models = vec![m.clone()];
        a.action(answer(&card.id, crate::emergency_local::YES)).unwrap();
        // With a key it is the agent's again; without one the pick is refused like any menu pick.
        assert_eq!(a.settings.agent_model.api().is_some(), m.provider_info().is_some_and(agent::provider::has_credential));
    }

