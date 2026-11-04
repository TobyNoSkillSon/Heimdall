    use super::*;
    fn go() -> &'static ProviderInfo {
        provider(OPENCODE_GO).unwrap()
    }
    #[test]
    #[cfg(not(feature = "public"))]
    fn haiku_is_the_default_selection_and_round_trips() {
        let selected = AgentModel::default();
        assert_eq!(selected.api(), Some((OPENCODE_GO, HAIKU)));
        let text = serde_json::to_string(&selected).unwrap();
        assert_eq!(text, r#"{"api":{"provider":"opencode-go","id":"claude-haiku-5-5"}}"#);
        assert_eq!(serde_json::from_str::<AgentModel>(&text).unwrap(), selected);
        assert_eq!(serde_json::from_str::<AgentModel>("\"local\"").unwrap(), AgentModel::Local);
    }
    #[test]
    fn budgets_default_to_the_whole_window_and_follow_the_slider_stops() {
        // An untouched slider is the model's whole window (bug note 2026-10-28T19-41-00; it was the stop nearest 500K).
        assert_eq!(ApiModel::haiku().budget(), 1_000_000);
        let mut window_262 = ApiModel::haiku();
        window_262.context = 262_144;
        assert_eq!(window_262.budget(), 262_144);
        // At the slider's minimum: Heimdall compacts at 85% (108,800), Pi's own check at 112,000 (the budget less the
        // reserve). An answer may take the model's whole output limit (F7).
        let l = ApiModel { context_budget: Some(128_000), ..ApiModel::haiku() }.limits();
        assert_eq!((l.context, l.response, l.reserve, l.recent), (128_000, 128_000, 16_000, 20_000));
        assert_eq!((l.compact_at(), l.threshold()), (108_800, 112_000));
        // Pi is told the real window and a reserve that keeps the same threshold.
        assert_eq!((l.window, l.pi_reserve()), (1_000_000, 888_000));
        // A 1M window: seven equal intervals from 128K.
        let mut opus = ApiModel::anthropic(OPUS).unwrap();
        assert_eq!(opus.context_steps(), [128_000, 252_571, 377_142, 501_714, 626_285, 750_857, 875_428, 1_000_000]);
        opus.context_budget = Some(500_000);
        assert_eq!(opus.budget(), 500_000, "a saved value inside the window is kept exactly, as its own stop");
        assert!(opus.context_steps().contains(&500_000));
        // Pi's in-turn check at 90% at most (F5): a busy stretch cannot run the history to 97% of the budget.
        assert_eq!(opus.limits().threshold(), 500_000 * 90 / 100);
        opus.context_budget = Some(5_000_000);
        assert_eq!(opus.budget(), 1_000_000);
        // A 262K window: two intervals. A window under 128K is one fixed stop with its own output limit.
        let mut small = ApiModel::haiku();
        small.context = 262_144;
        assert_eq!(small.context_steps(), [128_000, 195_072, 262_144]);
        small.context = 32_768;
        small.max_output = 2_000;
        assert_eq!(small.context_steps(), [32_768]);
        assert_eq!(small.limits(), Limits { context: 32_768, response: 2_000, reserve: 2_000, recent: 8_192, window: 32_768, carry: 0 });
        assert_eq!(small.limits().pi_reserve(), 32_768 - 32_768 * 90 / 100);
    }
    #[test]
    fn spend_follows_prices_tiers_and_the_hour_cache() {
        let opus = ApiModel::anthropic(OPUS).unwrap();
        // 2,000 new input, 40,000 read from cache, 3,000 written to the one-hour cache, 500 output.
        let u = Usage { input: 2_000, output: 500, cache_read: 40_000, cache_write: 3_000, cache_write_1h: 0 };
        let cost = opus.request_cost(&u, true).unwrap();
        // 2000×4 + 500×20 + 40000×0.20 + 3000×8 = 8000 + 10000 + 8000 + 24000 = 50,000 per million = $0.05.
        assert!((cost - 0.05).abs() < 1e-9, "{cost}");
        // Five-minute writes cost 1.25× input instead; Anthropic's own split wins over the assumption.
        assert!((opus.request_cost(&u, false).unwrap() - 0.041).abs() < 1e-9);
        let split = Usage { cache_write_1h: 1_000, ..u };
        assert!((opus.request_cost(&split, true).unwrap() - (0.041 + 1_000.0 * (8.0 - 5.0) / 1e6)).abs() < 1e-9);
        // Haiku above 100K prompt tokens pays its higher tier for the whole request.
        let haiku = ApiModel::anthropic(HAIKU).unwrap();
        let big = Usage { input: 120_000, output: 1_000, ..Default::default() };
        assert!((haiku.request_cost(&big, true).unwrap() - (120_000.0 * 0.5 + 1_000.0 * 2.5) / 1e6).abs() < 1e-12);
        let small = Usage { input: 90_000, output: 1_000, ..Default::default() };
        assert!((haiku.request_cost(&small, true).unwrap() - (90_000.0 * 0.1 + 1_000.0 * 0.5) / 1e6).abs() < 1e-12);
        let mut unknown = opus.clone();
        unknown.cost = None;
        assert_eq!(unknown.request_cost(&u, true), None);
        assert_eq!(Usage::from_pi(&json!({"input":5,"output":1,"cacheRead":7,"cacheWrite":9,"cacheWrite1h":9,"totalTokens":22})), Some(Usage { input: 5, output: 1, cache_read: 7, cache_write: 9, cache_write_1h: 9 }));
        assert_eq!(Usage::from_pi(&json!({"input":0,"output":0})), None);
    }
    #[test]
    fn thinking_levels_follow_anthropics_rules_and_pis_catalog() {
        let opus = ApiModel::anthropic(OPUS).unwrap();
        let mut sonnet = ApiModel::anthropic(SONNET).unwrap();
        let haiku = ApiModel::anthropic(HAIKU).unwrap();
        // Opus 5.5 cannot turn thinking off; Sonnet and Haiku 5.5 can, on either provider. Everyone starts at Low.
        assert_eq!(opus.thinking_levels(None), ["low", "medium", "high", "max"]);
        assert_eq!(sonnet.thinking_levels(None), ["off", "low", "medium", "high", "max"]);
        assert_eq!(ApiModel::haiku().thinking_levels(None), ["off", "low", "medium", "high", "max"]);
        assert_eq!((opus.thinking_level(None), sonnet.thinking_level(None)), ("low", "low"));
        sonnet.thinking = Some("off".into());
        assert_eq!(sonnet.thinking_level(None), "off");
        let mut o = opus.clone();
        o.thinking = Some("off".into());
        assert_eq!(o.thinking_level(None), "low", "Off on Opus runs its least thinking");
        // Pi sends Off itself except for Anthropic's managed-effort Sonnet and Haiku.
        assert_eq!((opus.thinking_off(), sonnet.thinking_off(), haiku.thinking_off(), ApiModel::haiku().thinking_off()), (None, Some("between_tools"), Some("disabled"), None));
        // Other models: Pi's catalog map. Kimi K3 thinks only at max; GLM 5.3 maps off/medium to null; DeepSeek V4
        // Pro only high and max; a model without a map takes Off to High; a model without reasoning only Off.
        let entry = |map: Value| json!({"reasoning": true, "thinkingLevelMap": map});
        let mut m = ApiModel { id: "kimi-k3".into(), ..ApiModel::haiku() };
        let kimi = entry(json!({"off":null,"minimal":null,"low":null,"medium":null,"high":null,"xhigh":null,"max":"max"}));
        assert_eq!(m.thinking_levels(Some(&kimi)), ["max"]);
        assert_eq!(m.thinking_level(Some(&kimi)), "max");
        m.id = "glm-5.3".into();
        assert_eq!(m.thinking_levels(Some(&entry(json!({"off":null,"minimal":null,"low":"low","medium":null,"high":"high","xhigh":null,"max":"max"})))), ["low", "high", "max"]);
        m.id = "deepseek-v4-pro".into();
        let pro = entry(json!({"minimal":null,"low":null,"medium":null,"high":"high","max":"max"}));
        assert_eq!(m.thinking_levels(Some(&pro)), ["off", "high", "max"]);
        assert_eq!(m.thinking_level(Some(&pro)), "high", "Low rounds up to the next level the model has");
        m.id = "minimax-m3".into();
        assert_eq!(m.thinking_levels(Some(&json!({"reasoning": true}))), ["off", "low", "medium", "high"]);
        assert_eq!(m.thinking_levels(Some(&json!({"reasoning": false}))), ["off"]);
        assert_eq!(clamp_thinking(&LOCAL_THINKING, "max"), "high");
        // The choice is kept with the model.
        let text = serde_json::to_string(&sonnet).unwrap();
        assert!(text.contains(r#""thinking":"off""#));
        assert_eq!(serde_json::from_str::<ApiModel>(&text).unwrap(), sonnet);
        assert!(!serde_json::to_string(&opus).unwrap().contains("thinking\""));
    }
    #[test]
    fn local_limits_match_the_previous_formula() {
        let l = Limits::for_context(262_144, None);
        assert_eq!((l.response, l.reserve, l.recent), (16_384, 16_384, 20_000));
    }
    #[test]
    fn unsupported_families_say_why() {
        let mut m = ApiModel::haiku();
        assert_eq!(m.unsupported(), None);
        m.api = Api::Google;
        assert_eq!(m.unsupported().unwrap(), "Google endpoint not supported by Pi");
        m.api = Api::Unknown;
        assert_eq!(m.unsupported().unwrap(), "Endpoint unknown");
        m.api = Api::OpenaiCompletions;
        m.context = 0;
        assert_eq!(m.unsupported().unwrap(), "Context limit unknown");
    }
    #[test]
    fn catalog_prefers_pi_then_models_dev_then_marks_unknown() {
        let pi = json!({"anthropic-messages":{"chat:claude-haiku-5-5":{"id":"claude-haiku-5-5","name":"Claude Haiku 5.5","api":"anthropic-messages","reasoning":true,"input":["text","image"],"contextWindow":1000000,"maxTokens":128000,"cost":{"input":0.1,"tiers":[{"inputTokensAbove":100000}]},"type":"chat"}},
            "openai-completions":{"chat:kimi-k3":{"id":"kimi-k3","name":"Kimi K3","api":"openai-completions","input":["text"],"contextWindow":1048576,"maxTokens":131072,"type":"chat"}}});
        let dev = json!({"npm":"@ai-sdk/openai-compatible","models":{
            "beam-free":{"name":"Beam Free","limit":{"context":1048576,"output":131072},"cost":{"input":0,"output":0}},
            "gemini-4-argon-contributor":{"name":"Gemini 4 Argon","provider":{"npm":"@ai-sdk/google"},"limit":{"context":1048576,"output":65536}},
            "grok-4.7":{"name":"Grok 4.7","provider":{"npm":"@ai-sdk/openai"},"limit":{"context":500000,"output":500000},"cost":{"input":2,"output":6,"tiers":[{"input":4,"output":12,"tier":{"type":"context","size":200000}}]}}}});
        let ids: Vec<Value> = ["claude-haiku-5-5", "kimi-k3", "beam-free", "gemini-4-argon-contributor", "grok-4.7", "omen-alpha"].map(|id| json!({"id":id})).to_vec();
        let rows = merge_catalog(go(), &ids, Some(&pi), Some(&dev));
        assert_eq!((rows[0].api, rows[0].context, rows[0].max_output, rows[0].name.as_str()), (Api::AnthropicMessages, 1_000_000, 128_000, "Claude Haiku 5.5"));
        assert_eq!(rows[0].cost.as_ref().unwrap()["tiers"][0]["inputTokensAbove"], 100_000);
        assert_eq!((rows[1].api, rows[1].context), (Api::OpenaiCompletions, 1_048_576));
        assert_eq!((rows[2].api, rows[2].name.as_str()), (Api::OpenaiCompletions, "Beam Free"));
        assert_eq!(rows[3].unsupported().unwrap(), "Google endpoint not supported by Pi");
        assert_eq!(rows[4].api, Api::OpenaiResponses);
        assert_eq!(rows[4].cost.as_ref().unwrap()["tiers"][0]["inputTokensAbove"], 200_000);
        assert_eq!((rows[5].name.as_str(), rows[5].unsupported().unwrap().as_str()), ("omen-alpha", "Endpoint unknown"));
        // Models Pi lacks get OpenCode's documented endpoints.
        assert_eq!(rows[2].pi_definition().unwrap()["baseUrl"], "https://opencode.ai/inference/go/openai/v1");
        assert!(rows[3].pi_definition().is_err());
    }
    /// The user's account listing on 27 Oct (trimmed; measured) and Pi 1.2.0's own entries for Sol and Luna.
    fn chatgpt_rows() -> Vec<ApiModel> {
        let body: Value = serde_json::from_str(include_str!("chatgpt-listing-2026-10-27.json")).unwrap();
        let pi: Value = serde_json::from_str(include_str!("pi-1.2.0-openai-gpt6.json")).unwrap();
        merge_catalog(provider(CHATGPT).unwrap(), &crate::chatgpt::listing(&body), Some(&pi), None)
    }
    /// F2 (audit 27 Oct): the account lists 500,000 for Sol and Terra and 272,000 for Luna; Heimdall kept Pi's 272,000
    /// for Sol and a 128K, text-only, no-output placeholder for Terra, whose slider could not move.
    #[test]
    fn chatgpt_models_take_the_accounts_window_images_and_efforts() {
        let pi: Value = serde_json::from_str(include_str!("pi-1.2.0-openai-gpt6.json")).unwrap();
        let rows = chatgpt_rows();
        assert_eq!(rows.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), ["gpt-6.1-sol", "gpt-6.1-terra", "gpt-6-luna"], "hidden models stay out");
        let [sol, terra, luna] = [&rows[0], &rows[1], &rows[2]];
        // The slider's top is the account's window (decision 27 Oct: 500K, not the untested 872K maximum).
        assert_eq!([sol.context, terra.context, luna.context], [500_000, 500_000, 272_000]);
        assert_eq!(terra.context_steps(), [128_000, 252_000, 376_000, 500_000]);
        assert_eq!(*sol.context_steps().last().unwrap(), 500_000);
        // Terra, which Pi's catalog lacks, takes its sibling Sol's format and output limit, and the account's images.
        assert_eq!((terra.api, terra.max_output, terra.images, terra.reasoning, terra.name.as_str()), (Api::OpenaiResponses, 128_000, true, true, "GPT-6.1 Terra"));
        assert!(terra.cost.is_none() && terra.unsupported().is_none() && luna.images && sol.images);
        let mut t = terra.clone();
        t.context_budget = Some(500_000);
        let def = t.pi_definition().unwrap();
        assert_eq!((def["contextWindow"].as_u64(), def["maxTokens"].as_u64(), def["input"].clone()), (Some(500_000), Some(128_000), json!(["text", "image"])));
        // F11: the levels are the account's; Off only where Pi knows the model takes "none" (Luna).
        assert_eq!(terra.efforts, ["low", "medium", "high", "xhigh", "max", "ultra"]);
        assert_eq!(terra.thinking_levels(None), ["low", "medium", "high", "max"]);
        assert_eq!(sol.thinking_levels(pi_entry(&pi, "gpt-6.1-sol")), ["low", "medium", "high", "max"]);
        assert_eq!(luna.thinking_levels(pi_entry(&pi, "gpt-6-luna")), ["off", "low", "medium", "high", "max"]);
        assert_eq!((def["thinkingLevelMap"]["max"].as_str(), def["thinkingLevelMap"]["xhigh"].as_str(), def["thinkingLevelMap"]["off"].clone()), (Some("max"), Some("xhigh"), Value::Null));
        // A sibling is a versioned family on the Responses format only: a bare name or another format borrows nothing.
        let p = provider(CHATGPT).unwrap();
        let other = json!({"openai-completions":{"chat:gpt-6.1-mini":{"id":"gpt-6.1-mini","api":"openai-completions","type":"chat","maxTokens":64000}},
            "openai-responses":{"chat:codex-pro":{"id":"codex-pro","api":"openai-responses","type":"chat","maxTokens":99000}}});
        let listed = [json!({"id":"gpt-6.1-terra","context_window":500000}), json!({"id":"codex-max","context_window":300000})];
        let rows = merge_catalog(p, &listed, Some(&other), None);
        assert_eq!((rows[0].max_output, rows[1].max_output), (0, 0), "no sibling borrowed");
        assert_eq!((rows[0].context, rows[1].context), (500_000, 300_000));
    }
    /// F2: rows are captured when added, so the Terra placeholder already in settings.json is repaired on load from its
    /// sibling, and every added row takes the listing's newer figures without shrinking the user's budget.
    #[test]
    fn a_stored_terra_placeholder_is_repaired_and_rows_refresh_without_shrinking_the_budget() {
        let p = provider(CHATGPT).unwrap();
        let rows = chatgpt_rows();
        // The user's settings.json, 27 Oct: Sol at 272K (max then), Terra the placeholder, both on high.
        let sol = ApiModel { context: 272_000, context_budget: Some(272_000), thinking: Some("high".into()), efforts: vec![], ..rows[0].clone() };
        let terra = ApiModel { provider: p.id.into(), id: "gpt-6.1-terra".into(), name: "GPT-6.1 Terra".into(), api: Api::OpenaiResponses, context: 128_000, max_output: 0,
            images: false, reasoning: true, cost: None, context_budget: None, thinking: Some("high".into()), base_url: None, efforts: vec![] };
        assert!(terra.is_chatgpt_placeholder() && !sol.is_chatgpt_placeholder());
        let mut stored = vec![ApiModel::haiku(), sol.clone(), terra.clone()];
        assert!(repair_placeholders(&mut stored));
        assert_eq!((stored[2].context, stored[2].max_output, stored[2].images, stored[2].thinking.as_deref()), (272_000, 128_000, true, Some("high")));
        assert!(stored[2].context_steps().len() > 1, "Terra's slider moves");
        assert!(!repair_placeholders(&mut stored), "once");
        // The listing refresh: 500K for both. A refresh never changes the user's budget: 272K stays, as its own stop
        // between 252K and 376K, and the slider reaches 500K for him to choose.
        let sol_now = stored[1].refreshed(&rows[0]);
        assert_eq!((sol_now.context, sol_now.budget(), sol_now.thinking.as_deref(), sol_now.name.as_str()), (500_000, 272_000, Some("high"), sol.name.as_str()));
        let at_stop = ApiModel { context_budget: Some(376_000), ..sol_now.clone() };
        assert_eq!(at_stop.refreshed(&rows[0]).budget(), 376_000);
        assert_eq!(sol_now.context_steps(), [128_000, 252_000, 272_000, 376_000, 500_000]);
        let shrunk = ApiModel { context: 200_000, ..rows[0].clone() };
        assert!(sol_now.refreshed(&shrunk).budget() <= 200_000, "a window that shrank below the budget lowers it");
        let below_all = ApiModel { context: 1_000_000, context_budget: Some(128_000), ..sol_now.clone() };
        assert_eq!(below_all.refreshed(&rows[0]).budget(), 128_000);
        let terra_now = stored[2].refreshed(&rows[1]);
        assert_eq!((terra_now.context, terra_now.budget(), terra_now.context_budget), (500_000, 500_000, None), "an untouched slider takes the whole window");
        assert_eq!(sol_now.refreshed(&rows[0]), sol_now, "a second refresh changes nothing");
        // A catalog that knows less (Pi's catalog unreadable: no output limit; the old placeholder) takes nothing away.
        assert_eq!(sol_now.refreshed(&terra).context, 500_000);
        assert_eq!(sol_now.refreshed(&ApiModel { max_output: 0, cost: None, efforts: vec![], ..rows[0].clone() }), sol_now);
    }
    /// Bug note 2026-10-28T19-41-00 ("by default we should set all the models and all the providers to
    /// the maximum level, so we always have as many tokens as possible"): every model the user has not set runs its whole
    /// window, the one a start switches to included; the budget he set himself stays his. Before: Opus, Sonnet and Haiku
    /// (1M) ran 501,714, the slider stop nearest 500K.
    #[test]
    fn every_untouched_model_runs_its_whole_window_and_a_saved_budget_stays() {
        let mut rows: Vec<ApiModel> = [OPUS, SONNET, HAIKU].iter().map(|id| ApiModel::anthropic(id).unwrap()).collect();
        rows.push(ApiModel::haiku());
        rows.extend(chatgpt_rows());
        for m in &rows {
            assert_eq!((m.key(), m.budget(), m.limits().context, m.limits().window), (m.key(), m.context, m.context, m.context), "{} on {}", m.id, m.provider);
        }
        // 28 Oct 17:27: the 334,007-token history switched to an untouched Opus fits its budget; nothing to carry or cut.
        let l = ApiModel::anthropic(OPUS).unwrap().limits().carrying(Some(334_007));
        assert_eq!((l.context, l.carry, l.threshold(), l.compact_at()), (1_000_000, 0, 900_000, 850_000));
        // The user's own slider choice (his Opus at 252,571, his Luna at 128K) is kept exactly.
        let mut opus = ApiModel::anthropic(OPUS).unwrap();
        opus.context_budget = Some(252_571);
        assert_eq!(opus.budget(), 252_571);
        assert_eq!(ApiModel { context_budget: Some(128_000), ..chatgpt_rows()[2].clone() }.budget(), 128_000);
    }
    /// F7: an answer may take the model's own output limit (Pi trims it to the window per request); the reserve that
    /// sets the compaction threshold stays separate.
    #[test]
    fn pi_gets_the_models_output_limit_and_the_reserve_stays_separate() {
        let mut opus = ApiModel::anthropic(OPUS).unwrap();
        opus.context_budget = Some(252_571);
        let l = opus.limits();
        assert_eq!((l.response, l.reserve, l.compact_at(), l.threshold()), (128_000, 16_384, 214_685, 227_313));
        // Unknown output limit and local models: the reserve, as before.
        assert_eq!(Limits::for_context(262_144, None).response, 16_384);
        assert_eq!(Limits::for_context(128_000, Some(0)).response, 16_000);
    }
    /// F6: Opus or Sol at 230K switched to Luna's 128K budget compacted in front of the next request. Pi now carries a
    /// history that fits Luna's real window to the first quiet moment, where Heimdall's own compaction (85%) runs.
    #[test]
    fn a_switch_to_a_smaller_budget_carries_the_history_to_the_first_quiet_moment() {
        let luna = ApiModel { context_budget: Some(128_000), ..chatgpt_rows()[2].clone() };
        let l = luna.limits();
        assert_eq!((l.context, l.window, l.threshold(), l.compact_at()), (128_000, 272_000, 112_000, 108_800));
        let carried = l.carrying(Some(230_000));
        assert_eq!((carried.pi_threshold(), carried.pi_reserve(), carried.compact_at(), carried.threshold()), (246_000, 26_000, 108_800, 112_000));
        // A history inside the budget, or past the window less the reserve, keeps the budget's threshold.
        assert_eq!(l.carrying(Some(100_000)), l);
        assert_eq!(l.carrying(Some(256_000)), l);
        assert_eq!(l.carrying(None), l);
        // Close to the window: never past it less the reserve.
        assert_eq!(l.carrying(Some(250_000)).pi_threshold(), 256_000);
    }
    #[test]
    fn keys_are_normalized_and_clipboard_matches_only_key_shapes() {
        assert_eq!(normalize_key("  sk-abc  ").unwrap(), "sk-abc");
        assert!(normalize_key("").is_err());
        assert!(normalize_key("sk-a b").is_err());
        assert!(looks_like_key("sk-0123456789abcdefghijklmnop"));
        assert!(!looks_like_key("hello world"));
        assert!(!looks_like_key("sk-short"));
        assert!(!looks_like_key("https://opencode.ai/console"));
    }
    #[test]
    fn lab_binary_keeps_keys_in_memory_and_never_searches_keychain() {
        let _lab = crate::provider::LAB_ITEMS_TEST.blocking_lock();
        assert!(!installed_app());
        let p = go();
        if std::env::var_os(p.key_env).is_some() {
            return;
        }
        forget_key(p).unwrap();
        assert_eq!(credential(p).unwrap_err().to_string(), "Connect OpenCode Go");
        save_key(p, " sk-lab-0123456789abcdefghij ", None).unwrap();
        assert_eq!(credential(p).unwrap(), Credential::Key("sk-lab-0123456789abcdefghij".into()));
        // A Console sign-in replaces the key, and the reverse.
        let s = crate::console::Session { access: "a".into(), refresh: "r".into(), expires_ms: 1, org_id: None, server: "https://opencode.ai/console".into(), base_url: None, headers: Default::default(), models: vec![], client_id: None };
        save_console_session(p, &s).unwrap();
        assert_eq!(credential(p).unwrap(), Credential::Console(s));
        assert!(api_key(p).is_err());
        save_key(p, "sk-lab-0123456789abcdefghij", None).unwrap();
        assert!(console_session(p).is_none());
        forget_key(p).unwrap();
        assert!(!has_credential(p));
    }
    #[test]
    fn provider_errors_are_explained_in_words() {
        let e = |raw| explain_error(OPENCODE_GO, raw);
        assert_eq!(e("401 {\"type\":\"error\"}").unwrap(), "OpenCode Go: access rejected / no subscription");
        assert_eq!(e("403 forbidden").unwrap(), "OpenCode Go: access rejected / no subscription");
        assert_eq!(e("Upstream request failed: Invalid credential").unwrap(), "OpenCode Go: access rejected / no subscription");
        assert_eq!(e("413 too large").unwrap(), "OpenCode Go: request too large for this model");
        assert_eq!(e("429 limit").unwrap(), "OpenCode Go: usage limit reached");
        assert_eq!(e("socket hang up"), None);
        assert_eq!(e("Failed to resolve API key for provider \"opencode-go\" from shell command: '/A/heimdall' --provider-token opencode-go --data '/d'").unwrap(), "OpenCode Go: sign-in could not be refreshed");
        assert_eq!(e("404 Not Found").unwrap(), "OpenCode Go: endpoint not found for this model");
        assert_eq!(explain_error("splash", "401 x"), None);
        // Anthropic's own errors (the bodies are Anthropic's documented shapes).
        let a = |raw| explain_error(ANTHROPIC, raw).unwrap();
        assert_eq!(a(r#"401 {"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#), "Anthropic: key rejected");
        assert_eq!(a(r#"403 {"type":"error","error":{"type":"permission_error","message":"not allowed"}}"#), "Anthropic: key not allowed to use this model");
        assert_eq!(a(r#"400 {"type":"error","error":{"type":"invalid_request_error","message":"Your credit balance is too low to access the Anthropic API. Please go to Plans & Billing to upgrade or purchase credits."}}"#), "Anthropic credits used up");
        assert_eq!(a(r#"402 {"type":"error","error":{"type":"billing_error","message":"payment required"}}"#), "Anthropic credits used up");
        assert_eq!(a(r#"429 {"type":"error","error":{"type":"rate_limit_error","message":"Number of request tokens has exceeded your per-minute rate limit"}}"#), "Anthropic: rate limit reached");
        assert_eq!(a(r#"529 {"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#), "Anthropic overloaded · try again shortly");
        assert_eq!(a(r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#), "Anthropic overloaded · try again shortly");
    }
    #[tokio::test]
    async fn anthropic_key_check_and_listing_use_x_api_key_and_the_listing_limits() {
        use axum::{http::HeaderMap, routing::get, Json, Router};
        let app = Router::new().route("/v1/models", get(|headers: HeaderMap| async move {
            let h = |n: &str| headers.get(n).and_then(|v| v.to_str().ok()).map(str::to_owned);
            if h("anthropic-version").as_deref() != Some(ANTHROPIC_VERSION) || h("authorization").is_some() {
                return Err(axum::http::StatusCode::BAD_REQUEST);
            }
            match h("x-api-key").as_deref() {
                Some("sk-ant-good") => Ok(Json(json!({"data":[
                    {"type":"model","id":"claude-opus-5-5","display_name":"Claude Opus 5.5","max_input_tokens":1000000,"max_tokens":128000,"capabilities":{"image_input":{"supported":true}}},
                    {"type":"model","id":"claude-mythos-7","display_name":"Claude Mythos 7","max_input_tokens":2000000,"max_tokens":64000,"capabilities":{"image_input":{"supported":false}}}],
                    "has_more":false}))),
                _ => Err(axum::http::StatusCode::UNAUTHORIZED),
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/models", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, app).await });
        let p = provider(ANTHROPIC).unwrap();
        assert_eq!(check_key(p, "sk-ant-good", None, Some(&url)).await, KeyCheck::Valid { workspace: None });
        assert_eq!(check_key(p, "sk-ant-bad", None, Some(&url)).await, KeyCheck::Rejected);
        assert_eq!(live_models(p, None, None, Some(&url)).await.unwrap_err().to_string(), "Connect Anthropic", "the list needs a key");
        let pi = json!({"anthropic-messages":{"chat:claude-opus-5-5":{"id":"claude-opus-5-5","name":"Claude Opus 5.5","api":"anthropic-messages","reasoning":true,"input":["text","image"],"contextWindow":1000000,"maxTokens":128000,"cost":{"input":4,"output":20,"cacheRead":0.2,"cacheWrite":5},"type":"chat"}}});
        let rows = fetch_catalog(p, Some("sk-ant-good"), None, Some(&pi), (Some(&url), None)).await.unwrap();
        assert_eq!((rows[0].name.as_str(), rows[0].reasoning, rows[0].cost.as_ref().unwrap()["input"].as_f64()), ("Claude Opus 5.5", true, Some(4.0)));
        // A model Pi does not know yet: the listing's own window and output limit, no thinking request, no price.
        assert_eq!((rows[1].name.as_str(), rows[1].context, rows[1].max_output, rows[1].images, rows[1].reasoning), ("Claude Mythos 7", 2_000_000, 64_000, false, false));
        assert_eq!(rows[1].unsupported(), None);
        assert_eq!(rows[1].pi_definition().unwrap()["baseUrl"], "https://api.anthropic.com");
        assert!(rows[1].cost.is_none());
        task.abort();
    }
    #[tokio::test]
    async fn key_check_and_model_list_against_a_fake_provider() {
        use axum::{http::HeaderMap, routing::get, Json, Router};
        let app = Router::new().route(
            "/v1/models",
            get(|headers: HeaderMap| async move {
                let auth = headers.get("authorization").and_then(|v| v.to_str().ok()).map(str::to_owned);
                match auth.as_deref() {
                    Some("Bearer sk-good") | None => Ok(Json(json!({"object":"list","data":[{"id":"claude-haiku-5-5"},{"id":"omen-alpha"}]}))),
                    _ => Err(axum::http::StatusCode::UNAUTHORIZED),
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/models", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, app).await });
        let p = go();
        assert_eq!(check_key(p, "sk-good", None, Some(&url)).await, KeyCheck::Valid { workspace: None });
        assert_eq!(check_key(p, "sk-bad", None, Some(&url)).await, KeyCheck::Rejected);
        assert_eq!(check_key(p, "sk-good", None, Some("http://127.0.0.1:9/v1/models")).await, KeyCheck::Unreachable);
        let ids = |v: Vec<Value>| v.iter().map(|m| m["id"].as_str().unwrap().to_owned()).collect::<Vec<_>>();
        assert_eq!(ids(live_models(p, None, None, Some(&url)).await.unwrap()), ["claude-haiku-5-5", "omen-alpha"]);
        assert_eq!(live_models(p, Some("sk-bad"), None, Some(&url)).await.unwrap_err().to_string(), "OpenCode Go: access rejected / no subscription");
        // Pi knows Haiku; the unknown model asks models.dev, which is down here: an unavailable row, not an error.
        let pi = json!({"anthropic-messages":{"chat:claude-haiku-5-5":{"id":"claude-haiku-5-5","api":"anthropic-messages","contextWindow":1000000,"maxTokens":128000,"type":"chat"}}});
        let rows = fetch_catalog(p, Some("sk-good"), None, Some(&pi), (Some(&url), Some("http://127.0.0.1:9/api.json"))).await.unwrap();
        assert_eq!(rows.iter().map(|m| (m.id.as_str(), m.unsupported().is_none())).collect::<Vec<_>>(), [("claude-haiku-5-5", true), ("omen-alpha", false)]);
        task.abort();
    }
    /// The user's key (18 Oct) belongs to no single workspace: Anthropic answers 400 "This API key is not scoped to a
    /// workspace, so this request must include the anthropic-workspace-id header". The fake reproduces that and the
    /// organisation's workspace list; the check picks the Default workspace (Research is active too, Old is
    /// archived), repeats the request naming it, and reports the id to keep. A saved workspace the key can no longer
    /// use is looked up again; a scoped key never sends the header; any other refusal is shown in Anthropic's words.
    #[tokio::test]
    async fn unscoped_anthropic_keys_name_the_default_workspace() {
        use axum::{extract::Query, http::HeaderMap, routing::get, Json, Router};
        use std::sync::{Arc, Mutex};
        let seen: Arc<Mutex<Vec<(String, Option<String>)>>> = Arc::default();
        let s1 = seen.clone();
        let s2 = seen.clone();
        let unscoped = r#"{"type":"error","error":{"type":"invalid_request_error","message":"This API key is not scoped to a workspace, so this request must include the anthropic-workspace-id header"}}"#;
        let app = Router::new()
            .route("/v1/models", get(move |headers: HeaderMap| {
                let s = s1.clone();
                async move {
                    let h = |n: &str| headers.get(n).and_then(|v| v.to_str().ok()).map(str::to_owned);
                    s.lock().unwrap().push(("models".into(), h("anthropic-workspace-id")));
                    let models = Json(json!({"data":[{"type":"model","id":"claude-opus-5-5","max_input_tokens":1000000}],"has_more":false}));
                    match (h("x-api-key").as_deref(), h("anthropic-workspace-id").as_deref()) {
                        (Some("sk-ant-usr-lab"), Some("wrkspc_default")) => Ok(models),
                        (Some("sk-ant-usr-lab"), Some(_)) => Err((axum::http::StatusCode::FORBIDDEN, r#"{"type":"error","error":{"type":"permission_error","message":"This API key cannot use workspace wrkspc_gone"}}"#.to_string())),
                        (Some("sk-ant-usr-lab"), None) => Err((axum::http::StatusCode::BAD_REQUEST, unscoped.to_string())),
                        (Some("sk-ant-api03-fake-scoped"), None) => Ok(models),
                        (Some("sk-ant-api03-fake-org-limit"), _) => Err((axum::http::StatusCode::BAD_REQUEST, r#"{"type":"error","error":{"type":"invalid_request_error","message":"Your organization has been disabled."}}"#.to_string())),
                        _ => Err((axum::http::StatusCode::UNAUTHORIZED, r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#.to_string())),
                    }
                }
            }))
            .route("/v1/organizations/workspaces", get(move |headers: HeaderMap, Query(q): Query<std::collections::HashMap<String, String>>| {
                let s = s2.clone();
                async move {
                    s.lock().unwrap().push(("workspaces".into(), q.get("include_default").cloned()));
                    if headers.get("x-api-key").and_then(|v| v.to_str().ok()) != Some("sk-ant-usr-lab") || headers.get("anthropic-version").is_none() {
                        return Err(axum::http::StatusCode::UNAUTHORIZED);
                    }
                    Ok(Json(json!({"data":[
                        {"type":"workspace","id":"wrkspc_research","name":"Research","archived_at":null},
                        {"type":"workspace","id":"wrkspc_default","name":"Default","archived_at":null},
                        {"type":"workspace","id":"wrkspc_old","name":"Old","archived_at":"2026-09-01T00:00:00Z"}],"has_more":false})))
                }
            }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/models?limit=1000", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, app).await });
        let p = provider(ANTHROPIC).unwrap();
        let default = KeyCheck::Valid { workspace: Some("wrkspc_default".into()) };
        assert_eq!(check_key(p, "sk-ant-usr-lab", None, Some(&url)).await, default);
        assert_eq!(*seen.lock().unwrap(), [("models".into(), None), ("workspaces".into(), Some("true".into())), ("models".into(), Some("wrkspc_default".into()))]);
        seen.lock().unwrap().clear();
        // The saved workspace goes along and works: one request.
        assert_eq!(check_key(p, "sk-ant-usr-lab", Some("wrkspc_default"), Some(&url)).await, default);
        assert_eq!(seen.lock().unwrap().len(), 1);
        // A saved workspace the key can no longer use is replaced.
        assert_eq!(check_key(p, "sk-ant-usr-lab", Some("wrkspc_gone"), Some(&url)).await, default);
        seen.lock().unwrap().clear();
        assert_eq!(check_key(p, "sk-ant-api03-fake-scoped", None, Some(&url)).await, KeyCheck::Valid { workspace: None });
        assert_eq!(*seen.lock().unwrap(), [("models".to_string(), None)], "a scoped key sends no workspace header");
        assert_eq!(check_key(p, "sk-ant-api03-fake-org-limit", None, Some(&url)).await, KeyCheck::Refused("Anthropic: Your organization has been disabled.".into()));
        assert_eq!(check_key(p, "sk-ant-bad", None, Some(&url)).await, KeyCheck::Rejected);
        // The model list (the picker) names the workspace too.
        assert!(live_models(p, Some("sk-ant-usr-lab"), Some("wrkspc_default"), Some(&url)).await.is_ok());
        assert_eq!(live_models(p, Some("sk-ant-usr-lab"), None, Some(&url)).await.unwrap_err().to_string(),
            "Anthropic: This API key is not scoped to a workspace, so this request must include the anthropic-workspace-id header");
        task.abort();
    }
    #[test]
    fn workspace_ids_are_kept_beside_the_key_and_go_with_it() {
        let _lab = crate::provider::LAB_ITEMS_TEST.blocking_lock();
        assert!(!installed_app());
        let p = provider(ANTHROPIC).unwrap();
        if std::env::var_os(p.key_env).is_some() { return; }
        save_key(p, "sk-ant-usr-lab-0123456789abcdefghij", Some("wrkspc_01ABCdef")).unwrap();
        assert_eq!(workspace_id(p).as_deref(), Some("wrkspc_01ABCdef"));
        assert_eq!(workspace_account(p), "anthropic-workspace");
        save_key(p, "sk-ant-api03-lab-0123456789abcdefghij", None).unwrap();
        assert_eq!(workspace_id(p), None, "a scoped key replaces the workspace");
        save_workspace(p, Some("wrkspc_again")).unwrap();
        forget_key(p).unwrap();
        assert_eq!(workspace_id(p), None, "disconnect forgets it");
        assert!(save_workspace(p, Some("bad id; rm")).is_ok() && workspace_id(p).is_none(), "only an id-shaped value is kept");
        // OpenCode keys never carry one.
        save_workspace(go(), Some("wrkspc_x")).unwrap();
        assert_eq!(workspace_id(go()), None);
    }

    // The public edition's blank start: no model chosen, and the state survives the settings file.
    #[test]
    #[cfg(feature = "public")]
    fn the_public_edition_starts_with_no_model_chosen() {
        let selected = AgentModel::default();
        assert_eq!(selected, AgentModel::Unchosen);
        assert!(!selected.is_chosen() && !selected.is_local() && selected.api().is_none());
    }
    #[test]
    fn an_unchosen_model_round_trips_as_none() {
        assert_eq!(serde_json::to_string(&AgentModel::Unchosen).unwrap(), r#""none""#);
        assert_eq!(serde_json::from_str::<AgentModel>(r#""none""#).unwrap(), AgentModel::Unchosen);
        assert!(AgentModel::Local.is_chosen());
    }

    /// The provider catalog both editions offer: OpenRouter and the generic Own Engine are gone; the two custom providers
    /// take any URL with an optional key.
    #[test]
    fn the_catalog_has_the_two_custom_providers_and_no_retired_ones() {
        let ids: Vec<&str> = PROVIDERS.iter().map(|p| p.id).collect();
        assert_eq!(ids, [CHATGPT, OPENCODE_GO, ANTHROPIC, CUSTOM, CUSTOM_ANTHROPIC]);
        assert!(provider(RETIRED_OPENROUTER).is_none() && provider(RETIRED_OWN_ENGINE).is_none());
        for id in [CUSTOM, CUSTOM_ANTHROPIC] {
            let p = provider(id).unwrap();
            assert!(p.custom_url && !p.key_required && !p.subscription && p.connect_default.is_none(), "{id}");
        }
        assert_eq!(provider(CUSTOM_ANTHROPIC).unwrap().key_header, KeyHeader::XApiKey);
    }

    /// Personal Keychain names never change (existing items keep working).
    #[test]
    #[cfg(not(feature = "public"))]
    fn keychain_names_of_the_personal_edition_are_unchanged() {
        let names: Vec<&str> = PROVIDERS.iter().map(|p| p.keychain_service).collect();
        assert_eq!(names, ["Heimdall API provider: ChatGPT", "Heimdall API provider: OpenCode Go", "Heimdall API provider: Anthropic",
            "Heimdall API provider: OpenAI-Compatible", "Heimdall API provider: Anthropic-Compatible"]);
        assert_eq!(RETIRED_OWN_ENGINE_INFO.keychain_service, "Heimdall API provider: Own Engine");
    }

    /// The public edition has its own Keychain names, so it can never read the personal app's keys.
    #[test]
    #[cfg(feature = "public")]
    fn the_public_edition_keychain_names_are_its_own() {
        for p in PROVIDERS.iter().chain([&RETIRED_OWN_ENGINE_INFO]) {
            assert!(p.keychain_service.starts_with("Heimdall Alpha API provider: "), "{}", p.keychain_service);
            assert_ne!(p.keychain_service, format!("Heimdall API provider: {}", &p.keychain_service["Heimdall Alpha API provider: ".len()..]));
        }
        assert!(APP_BUNDLE == "Heimdall Alpha.app");
    }

    /// An Anthropic-compatible server: its base is the root (a typed `/v1` is dropped), its list is `/v1/models` with
    /// `x-api-key`, and every model runs on the Messages API at that base.
    #[test]
    fn anthropic_compatible_lists_and_runs_on_the_messages_api() {
        let p = provider(CUSTOM_ANTHROPIC).unwrap();
        for typed in ["https://api.example.dev/anthropic", "https://api.example.dev/anthropic/", "https://api.example.dev/anthropic/v1", "https://api.example.dev/anthropic/v1/messages"] {
            assert_eq!(normalize_base_url_for(p, typed).unwrap(), "https://api.example.dev/anthropic", "{typed}");
        }
        assert_eq!(models_url_at(p, "http://127.0.0.1:8080"), "http://127.0.0.1:8080/v1/models");
        let openai = provider(CUSTOM).unwrap();
        assert_eq!(normalize_base_url_for(openai, "https://api.groq.com/openai/v1/").unwrap(), "https://api.groq.com/openai/v1");
        assert_eq!(models_url_at(openai, "https://api.groq.com/openai/v1"), "https://api.groq.com/openai/v1/models");
        let listed = [json!({"id":"claude-local","display_name":"Claude Local","max_input_tokens":200000,"max_tokens":64000}), json!({"id":"bare"})];
        let rows = merge_catalog_at(p, &listed, None, None, Some("http://127.0.0.1:8080"));
        assert!(rows.iter().all(|r| r.api == Api::AnthropicMessages && r.base_url.as_deref() == Some("http://127.0.0.1:8080")), "{rows:?}");
        assert_eq!((rows[0].name.as_str(), rows[0].context), ("Claude Local", 200_000));
        assert_eq!(rows[1].context, MIN_CONTEXT);
        let def = rows[0].pi_definition().unwrap();
        assert_eq!((def["api"].as_str(), def["baseUrl"].as_str()), (Some("anthropic-messages"), Some("http://127.0.0.1:8080")));
        // An OpenAI-compatible server stays on chat completions even when it states a window in Anthropic's field.
        let rows = merge_catalog_at(openai, &[json!({"id":"llama","max_input_tokens":131072})], None, None, Some("http://localhost:1234/v1"));
        assert_eq!((rows[0].api, rows[0].base_url.as_deref()), (Api::OpenaiCompletions, Some("http://localhost:1234/v1")));
    }

    /// Own Engine's saved URL and key carry over to the OpenAI-compatible custom provider once, only when that one has
    /// none; the old items stay.
    #[test]
    fn own_engine_endpoint_moves_to_the_custom_provider_once() {
        let _lab = crate::provider::LAB_ITEMS_TEST.blocking_lock();
        let custom = provider(CUSTOM).unwrap();
        forget_key(custom).unwrap();
        forget_key(&RETIRED_OWN_ENGINE_INFO).unwrap();
        assert!(!adopt_retired_endpoint().unwrap(), "nothing to move");
        item_write(&RETIRED_OWN_ENGINE_INFO, "own-engine-base", b"http://localhost:1234/v1").unwrap();
        item_write(&RETIRED_OWN_ENGINE_INFO, RETIRED_OWN_ENGINE, NO_KEY.as_bytes()).unwrap();
        assert!(adopt_retired_endpoint().unwrap());
        assert_eq!(base_url(custom).as_deref(), Some("http://localhost:1234/v1"));
        assert_eq!(api_key(custom).unwrap(), NO_KEY);
        assert!(base_url(&RETIRED_OWN_ENGINE_INFO).is_some(), "the old items stay");
        // A custom provider connected since is never overwritten.
        save_endpoint(custom, "https://api.groq.com/openai/v1", Some("gsk_lab0123456789abcdefghijkl")).unwrap();
        assert!(!adopt_retired_endpoint().unwrap());
        assert_eq!(base_url(custom).as_deref(), Some("https://api.groq.com/openai/v1"));
        forget_key(custom).unwrap();
        forget_key(&RETIRED_OWN_ENGINE_INFO).unwrap();
    }
