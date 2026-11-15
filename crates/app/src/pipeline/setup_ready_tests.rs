use super::*;

fn state(r: &ui::ReadyRow) -> &ui::ReadyState {
    &r.state
}

/// 5 Nov: the agent's row is green only after a real request answered; no agent says Choose…; a refused
/// sign-in sends back to the provider step; anything else retries.
#[test]
fn the_agent_row_waits_for_a_real_answer() {
    let s = crate::settings::Settings { agent_model: agent::provider::AgentModel::Unchosen, agent_relay: false, ..Default::default() };
    let none = (vec![], vec![]);
    let r = agent_row(agent_target(&s, &none), &AgentCheck::None, false);
    assert!(matches!(state(&r), ui::ReadyState::Problem { fix: Some(ui::ReadyFix::Step { step: ui::SetupStep::Providers, .. }), .. }), "{r:?}");
    let api = crate::settings::Settings { agent_model: agent::provider::AgentModel::Api { provider: "opencode-go".into(), id: "claude-haiku-5-5".into() }, ..s.clone() };
    let target = agent_target(&api, &none);
    let key = match &target { Some(Ok((k, _, m))) => { assert_eq!((m.provider.as_str(), m.id.as_str()), ("opencode-go", "claude-haiku-5-5")); k.clone() } other => panic!("{other:?}") };
    assert!(matches!(state(&agent_row(target.clone(), &AgentCheck::Running(key.clone()), false)), ui::ReadyState::Working { .. }));
    assert_eq!(state(&agent_row(target.clone(), &AgentCheck::Passed(key.clone()), false)), &ui::ReadyState::Ready);
    assert!(matches!(state(&agent_row(target.clone(), &AgentCheck::Passed("api:other:x".into()), false)), ui::ReadyState::Working { .. }), "a check of another agent is no answer");
    let refused = agent_row(target.clone(), &AgentCheck::Failed(key.clone(), "HTTP 401: access rejected / no subscription".into()), false);
    assert!(matches!(state(&refused), ui::ReadyState::Problem { fix: Some(ui::ReadyFix::Step { step: ui::SetupStep::Providers, .. }), .. }));
    let down = agent_row(target, &AgentCheck::Failed(key, "HTTP 503".into()), false);
    assert!(matches!(state(&down), ui::ReadyState::Problem { fix: Some(ui::ReadyFix::Action { .. }), .. }));
    // The relay: T3's account and model; none listed yet says to connect T3 Code.
    let relay = crate::settings::Settings { agent_relay: true, ..s.clone() };
    assert!(matches!(agent_target(&relay, &none), Some(Err(_))));
    let listed = (vec![ui::RelayModel { id: "claude-sonnet-5-5".into(), name: "Claude Sonnet 5.5".into() }], vec![ui::RelayAccount { id: "claudeAgent".into(), name: "Claude".into() }]);
    match agent_target(&relay, &listed) { Some(Ok((_, label, m))) => { assert_eq!(m.provider, "t3:claudeAgent"); assert!(label.contains("Claude Code")) } other => panic!("{other:?}") }
    // This Mac's model still downloading: the agent waits for it.
    let local = crate::settings::Settings { agent_model: agent::provider::AgentModel::Local, engine_model: "qwen3.8-27b".into(), ..s };
    assert!(matches!(state(&agent_row(agent_target(&local, &none), &AgentCheck::None, true)), ui::ReadyState::Blocked { .. }));
}

#[test]
fn voice_listening_and_decider_rows_say_why_they_wait() {
    assert_eq!(state(&voice_row(true, Some("Edwin"), true, false, false)), &ui::ReadyState::Ready);
    assert!(matches!(state(&voice_row(true, Some("Edwin"), false, true, false)), ui::ReadyState::Working { .. }));
    assert!(matches!(state(&voice_row(true, Some("Edwin"), false, false, true)), ui::ReadyState::Problem { fix: Some(ui::ReadyFix::Action { .. }), .. }), "a failed learning offers Retry");
    assert!(matches!(state(&voice_row(false, Some("Edwin"), false, false, false)), ui::ReadyState::Problem { .. }), "Speaking off");
    assert_eq!(voice_row(true, Some("Edwin"), true, false, false).detail.as_deref(), Some("Phonon S · Edwin"));
    assert_eq!(state(&listening_row(true, true, false, true, true)), &ui::ReadyState::Ready);
    // VM run 5 Nov 05:07: never green beside a red microphone; the Microphone row carries the reason and the fix.
    assert!(matches!(state(&listening_row(true, true, false, true, false)), ui::ReadyState::Blocked { words } if words == "Waits for the microphone"), "no spinner beside the red row");
    assert!(matches!(state(&listening_row(true, false, false, false, false)), ui::ReadyState::Problem { fix: Some(ui::ReadyFix::Step { step: ui::SetupStep::Permissions, .. }), .. }));
    assert!(matches!(state(&listening_row(false, true, false, false, false)), ui::ReadyState::Blocked { .. }), "waits for its models");
    assert!(matches!(state(&listening_row(true, true, false, false, false)), ui::ReadyState::Working { .. }), "loading Parakeet is its own progress");
    assert_eq!(state(&decider_row(&decider::Phase::Ready, "V6 4-bit".into(), None)), &ui::ReadyState::Ready);
    assert!(matches!(state(&decider_row(&decider::Phase::Failed("x".into()), "V6 4-bit".into(), None)), ui::ReadyState::Working { .. }), "it retries by itself");
}

/// VM run 5 Nov 07:16 (3 of 3): the capture never started (its HAL thread waited on macOS's System Audio prompt, which
/// came only with that first capture, on Getting ready, and nobody answered), and the two rows waited on each other
/// forever: Microphone "Waits for listening", Listening "Waits for the microphone". Once listening is on, the
/// Microphone row says the capture is opening, or why it failed with Retry; it waits for listening only before.
#[test]
fn microphone_and_listening_never_wait_on_each_other() {
    let name = || Some("BlackHole 2ch".to_string());
    let failure = capture_start(true, false, Some("Capture: HAL discovery startup failed: Err(Timeout)".into()));
    let failed = microphone_row(1, name(), false, None, failure);
    assert!(matches!(state(&failed), ui::ReadyState::Problem { why, fix: Some(ui::ReadyFix::Action { action, .. }) }
        if why == "Couldn't start listening: the audio system did not answer" && **action == ui::UiAction::RetrySystemAudio), "{failed:?}");
    assert!(matches!(state(&microphone_row(1, name(), false, None, capture_start(true, true, None))), ui::ReadyState::Working { words, .. } if words == "Opening the microphone\u{2026}"));
    assert!(matches!(capture_start(true, false, None), CaptureStart::Failed(_)), "listening on with no capture at all is a failure, never a wait");
    assert_eq!(capture_start(false, false, None), CaptureStart::NotYet);
    assert_eq!(capture_start(true, true, Some("old".into())), CaptureStart::Starting, "a retry in flight");
    // Every state of listening on, with the microphone not yet heard: never both rows waiting.
    for start in [capture_start(true, true, None), capture_start(true, false, None), capture_start(true, false, Some("x".into()))] {
        let mic = microphone_row(1, name(), false, None, start);
        let listening = listening_row(true, true, false, true, mic.state == ui::ReadyState::Ready);
        assert!(!(matches!(mic.state, ui::ReadyState::Blocked { .. }) && matches!(listening.state, ui::ReadyState::Blocked { .. })), "{mic:?} / {listening:?}");
    }
}

/// System audio is optional. Its ask out, failed, or denied (read from its silent taps: VM run 5
/// Nov 08:36 left no trace) is a grey note that never holds Start; off offers Allow…; asked and running, or never
/// asked, says nothing.
#[test]
fn system_audio_is_a_note_never_a_blocker() {
    use capture::tap_gate::TapAccess;
    assert!(system_audio_row(None, false).is_none() && system_audio_row(Some(TapAccess::Open), false).is_none());
    let allow = Some(ui::ReadyFix::Action { title: "Allow\u{2026}".into(), action: Box::new(ui::UiAction::AskSystemAudio) });
    for (access, denied, why, fix) in [
        (Some(TapAccess::Waiting), false, "Waits for its permission", None),
        (Some(TapAccess::Off("create process tap: 'what'".into())), false, "Off: not allowed", allow.clone()),
        (Some(TapAccess::Open), true, "Off: not allowed", allow.clone()),
        (None, true, "Off: not allowed", allow.clone()),
    ] {
        let r = system_audio_row(access, denied).unwrap();
        assert_eq!(r.state, ui::ReadyState::Optional { why: why.into(), fix });
        assert!(ui::SetupReadiness { rows: vec![r] }.ready(), "never holds Start");
    }
}

/// Review 5 Nov (merge): the Microphone row is green only for sound from the input actually capturing, and only once
/// that is the one chosen (a mic picked while capture was starting left it green on the old input).
#[test]
fn the_microphone_row_hears_only_the_chosen_input() {
    assert!(hearing_chosen(None, Some("built-in")), "nothing chosen: the input in use");
    assert!(hearing_chosen(Some("usb"), Some("usb")));
    assert!(!hearing_chosen(Some("usb"), Some("built-in")), "still the old input");
    assert!(!hearing_chosen(Some("usb"), None), "no sound");
}

/// VM run 5 Nov 05:43: memory search never ran on a 16 GB Mac, yet its row was green. Green once the worker loaded;
/// too little memory says so; neither holds Start back; before its model is in, its download row stands.
#[test]
fn the_memory_search_row_says_whether_it_can_run() {
    use crate::embeddings::Search;
    assert!(memory_row(false, Search::Running, None).is_none(), "the download row stands until the model is in");
    assert_eq!(state(&memory_row(true, Search::Running, Some(1)).unwrap()), &ui::ReadyState::Ready);
    let no_memory = ui::ReadyState::Optional { why: "Needs 6 GB free memory".into(), fix: None };
    assert_eq!(state(&memory_row(true, Search::NoMemory, Some(64 << 30)).unwrap()), &no_memory, "the worker's refusal");
    assert_eq!(state(&memory_row(true, Search::NotYet, Some(5 << 30)).unwrap()), &no_memory, "too little free memory to load");
    assert!(matches!(state(&memory_row(true, Search::NotYet, Some(40 << 30)).unwrap()), ui::ReadyState::Optional { .. }), "not green before it ran");
    assert!(crate::embeddings::memory_refusal("EmbeddingGemma 2: embedding admission needs 6 GiB free"));
    assert!(crate::embeddings::memory_refusal(&heimdall_archive::embedding::low_memory_refusal()), "the host's words for exit 75");
    let rows = ui::SetupReadiness { rows: vec![memory_row(true, Search::NoMemory, None).unwrap()] };
    assert!(rows.ready(), "memory search is not needed to start");
}

/// 5 Nov: the decider is its own row, its download first, then loaded and answering.
#[test]
fn the_decider_row_is_its_download_then_its_load() {
    use crate::model_store::{Row, State};
    let dl = |state, done| Row { id: "decider".into(), title: "Decider".into(), done, total: 1_370_000_000, state };
    let ready = decider::Phase::Ready;
    assert_eq!(state(&decider_row(&ready, "V6 4-bit".into(), Some(&dl(State::Downloading, 612_000_000)))),
        &ui::ReadyState::Working { words: "Downloading".into(), progress: Some((612_000_000, 1_370_000_000)) }, "a download not done outranks the phase");
    assert!(matches!(state(&decider_row(&ready, "V6 4-bit".into(), Some(&dl(State::Waiting, 0)))), ui::ReadyState::Blocked { words } if words.contains("GB to download")));
    let failed = decider_row(&ready, "V6 4-bit".into(), Some(&dl(State::Failed("checksum mismatch".into()), 0)));
    assert!(matches!(state(&failed), ui::ReadyState::Problem { fix: Some(ui::ReadyFix::Action { .. }), .. }), "Retry");
    assert_eq!(state(&decider_row(&ready, "V6 4-bit".into(), Some(&dl(State::Done, 1_370_000_000)))), &ui::ReadyState::Ready);
    assert!(matches!(state(&decider_row(&decider::Phase::Loading, "V6 4-bit".into(), Some(&dl(State::Done, 1)))), ui::ReadyState::Working { .. }), "downloaded, not loaded yet");
}

/// 5 Nov: the microphone is required. Green only once sound arrives from it; no input at all is a red
/// row that says so and offers Choose…, and silence turns red after a while instead of spinning forever.
#[test]
fn the_microphone_row_is_green_only_when_sound_arrives() {
    use std::time::Duration;
    let name = || Some("MacBook Pro Microphone".to_string());
    let choose = |r: &ui::ReadyRow| matches!(state(r), ui::ReadyState::Problem { fix: Some(ui::ReadyFix::Step { step: ui::SetupStep::Microphone, .. }), .. });
    let none = microphone_row(0, None, false, None, CaptureStart::NotYet);
    assert!(choose(&none) && matches!(state(&none), ui::ReadyState::Problem { why, .. } if why == "No microphone found"), "{none:?}");
    assert_eq!(state(&microphone_row(1, name(), true, Some(Duration::from_secs(1)), CaptureStart::NotYet)), &ui::ReadyState::Ready);
    assert!(matches!(state(&microphone_row(1, name(), false, None, CaptureStart::NotYet)), ui::ReadyState::Blocked { words } if words == "Waits for listening"));
    assert!(matches!(state(&microphone_row(1, name(), false, Some(Duration::from_secs(3)), CaptureStart::NotYet)), ui::ReadyState::Working { .. }));
    let silent = microphone_row(1, name(), false, Some(MIC_SILENT + Duration::from_secs(1)), CaptureStart::NotYet);
    assert!(choose(&silent) && matches!(state(&silent), ui::ReadyState::Problem { why, .. } if why.contains("MacBook Pro Microphone")), "{silent:?}");
}

/// This Mac: its model's download as a row (with progress), Download when it never started, green once loaded.
#[test]
fn this_macs_model_is_a_row_with_its_download() {
    let quant = ui::EngineQuant { quant: "UD-Q4_K_XL".into(), size_bytes: Some(17_378_626_464), downloaded: false, ..Default::default() };
    let m = ui::EngineModel { id: "qwen3.8-27b".into(), label: "Qwen3.8 27B".into(), quant: "UD-Q4_K_XL".into(), quants: vec![quant.clone()], ..Default::default() };
    let r = engine_row(Some(&m), &ui::EngineState::Off);
    assert!(matches!(state(&r), ui::ReadyState::Problem { why, fix: Some(ui::ReadyFix::Action { title, .. }) } if why.contains("17.4 GB") && title == "Download"), "{r:?}");
    let going = ui::EngineModel { download: Some(ui::EngineDownload { quant: "UD-Q4_K_XL".into(), done_bytes: 1_000, total_bytes: Some(17_378_626_464), error: None }), ..m.clone() };
    assert_eq!(state(&engine_row(Some(&going), &ui::EngineState::Off)), &ui::ReadyState::Working { words: "Downloading".into(), progress: Some((1_000, 17_378_626_464)) });
    let here = ui::EngineModel { quants: vec![ui::EngineQuant { downloaded: true, ..quant }], ..m };
    assert!(matches!(state(&engine_row(Some(&here), &ui::EngineState::Loading { percent: None })), ui::ReadyState::Working { .. }));
    assert_eq!(state(&engine_row(Some(&here), &ui::EngineState::Ready)), &ui::ReadyState::Ready);
    assert_eq!(gb(17_378_626_464), "17.4 GB");
}

/// VM run 14 Nov, upgrade from alpha.1: the provider key alpha.1 saved answered errSecAuthFailed to the new build, yet
/// the agent's row was green (the test endpoint answered without a key). A key the Keychain door cannot read is a
/// problem on the row, whose words send the user to save it again; unlocking cannot fix an item an earlier build saved.
#[test]
fn an_unreadable_agent_key_is_a_problem_not_a_green_tick() {
    let s = crate::settings::Settings { agent_model: agent::provider::AgentModel::Api { provider: "opencode-go".into(), id: "claude-haiku-5-5".into() }, agent_relay: false, ..Default::default() };
    let target = agent_target(&s, &(vec![], vec![]));
    let key = match &target { Some(Ok((k, _, _))) => k.clone(), other => panic!("{other:?}") };
    let green = agent_row(target, &AgentCheck::Passed(key), false);
    assert_eq!(state(&green), &ui::ReadyState::Ready);
    assert_eq!(agent_secret(&s), agent::provider::provider("opencode-go").map(|p| p.keychain_service));
    assert_eq!(agent_secret(&crate::settings::Settings { agent_relay: true, ..s.clone() }), Some(t3::client::KEYCHAIN_SERVICE));
    assert_eq!(agent_key_problem(green.clone(), None, false), green, "a key that reads keeps the row");
    let unreadable = agent_key_problem(green.clone(), Some(keychain::Cause::Unreadable), false);
    match state(&unreadable) {
        ui::ReadyState::Problem { why, fix: Some(ui::ReadyFix::Step { step: ui::SetupStep::Providers, title }) } => {
            assert!(why.contains("re-enter it in Providers") && !why.contains("unlock"), "{why}");
            assert_eq!(title, "Re-enter Key\u{2026}");
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(state(&agent_key_problem(green.clone(), Some(keychain::Cause::Locked), false)), ui::ReadyState::Problem { why, .. } if why.starts_with("Keychain locked")));
    assert!(matches!(state(&agent_key_problem(green.clone(), Some(keychain::Cause::Unreadable), true)), ui::ReadyState::Problem { fix: Some(ui::ReadyFix::Step { step: ui::SetupStep::T3, .. }), .. }));
    // No agent chosen: that row's own problem stands.
    let unchosen = agent_row(None, &AgentCheck::None, false);
    assert_eq!(agent_key_problem(unchosen.clone(), Some(keychain::Cause::Unreadable), false), unchosen);
}

/// VM run 14 Nov, upgrade from alpha.1: macOS kept the microphone grant with the old signature; the capture heard
/// silence and the row said "No sound from BlackHole 2ch" with Choose…, which cannot fix it. Denied says so, with
/// Allow… (the page whose button opens System Settings).
#[test]
fn a_denied_microphone_says_so_instead_of_no_sound() {
    let name = || Some("BlackHole 2ch".to_string());
    let silent = microphone_row(1, name(), false, Some(MIC_SILENT + std::time::Duration::from_secs(1)), CaptureStart::NotYet);
    assert!(matches!(state(&silent), ui::ReadyState::Problem { why, .. } if why.starts_with("No sound from")));
    assert_eq!(microphone_access(silent.clone(), false), silent);
    let denied = microphone_access(silent, true);
    assert!(matches!(state(&denied), ui::ReadyState::Problem { why, fix: Some(ui::ReadyFix::Step { step: ui::SetupStep::Permissions, .. }) } if why == "Microphone access is off in System Settings"), "{denied:?}");
    // No microphone at all stays that.
    let none = microphone_row(0, None, false, None, CaptureStart::NotYet);
    assert_eq!(microphone_access(none.clone(), true), none);
}
