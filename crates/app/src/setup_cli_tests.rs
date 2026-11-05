use super::*;

fn args(s: &[&str]) -> Args {
    parse(&s.iter().map(|a| a.to_string()).collect::<Vec<_>>()).unwrap()
}

#[test]
fn flags_parse_and_refuse_what_the_reference_does_not_list() {
    let a = args(&["--defaults", "--provider", "claude-code", "--decider-precision", "8", "--mark", "sparkle", "--name", "Juno", "--json"]);
    assert_eq!((a.defaults, a.provider.as_deref(), a.precision.as_deref(), a.mark.as_deref(), a.name.as_deref(), a.json), (true, Some("claude-code"), Some("8bit"), Some("sparkle"), Some("Juno"), true));
    assert_eq!(args(&["--check"]).mode, Mode::Check);
    assert_eq!(args(&["--wait", "--timeout", "60"]).timeout, Some(60));
    let bad = |s: &[&str]| parse(&s.iter().map(|a| a.to_string()).collect::<Vec<_>>()).is_err();
    assert!(bad(&["--provider", "anthropic"]), "no API-key provider in Setup");
    assert!(bad(&["--decider-precision", "6"]));
    assert!(bad(&["--provider", "custom"]), "custom needs its URL");
    assert!(bad(&["--custom-key", "sk-secret"]), "a key is never a flag's value; only the variable's name is");
    assert!(bad(&["--mark", "dragon"]));
}

#[test]
fn defaults_take_the_agent_the_command_runs_in() {
    let env = |pairs: &'static [(&'static str, &'static str)]| move |k: &str| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string());
    assert_eq!(default_provider(env(&[("CLAUDECODE", "1")])), Some("claude-code"));
    assert_eq!(default_provider(env(&[("CODEX_HOME", "/Users/a/.codex")])), Some("codex"));
    assert_eq!(default_provider(env(&[])), None, "the one question");
}

/// The command's choices are the Setup window's own actions, in its order (docs/setup.md).
#[test]
fn each_choice_is_the_windows_own_actions() {
    use ui::UiAction as A;
    let none = Context::default();
    assert_eq!(actions_for(&Command::Provider { provider: "claude-code".into() }, &none), [A::ClaudeViaT3, A::InstallT3], "T3 Code installed when missing");
    let t3 = Context { t3_ready: true, ..none.clone() };
    assert_eq!(actions_for(&Command::Provider { provider: "claude-code".into() }, &t3), [A::ClaudeViaT3, A::ConnectT3]);
    assert_eq!(actions_for(&Command::Provider { provider: "codex".into() }, &none), [A::StartProviderLogin { provider: "openai".into() }]);
    let local = Context { engine_download: Some(("qwen3.8-27b".into(), "UD-Q4_K_XL".into())), ..none.clone() };
    assert_eq!(actions_for(&Command::Provider { provider: "local".into() }, &local),
        [A::SetEngine(true), A::DownloadEngineModel { id: "qwen3.8-27b".into(), quant: "UD-Q4_K_XL".into(), then_select: true }]);
    assert_eq!(actions_for(&Command::DeciderPrecision { precision: "8bit".into() }, &none), [A::SetDeciderPrecision("8bit".into()), A::StartModelDownloads]);
    // A mark brings its persona; a --name given comes after it (`commands` orders them).
    let mark = actions_for(&Command::Mark { mark: "sparkle".into() }, &none);
    assert_eq!(mark[1], A::Identity(ui::identity::IdentityAction::SetName("Lumi".into())));
    let a = args(&["--mark", "sparkle", "--name", "Juno"]);
    let c = commands(&a, None);
    assert!(c.iter().position(|c| matches!(c, Command::Mark { .. })) < c.iter().position(|c| matches!(c, Command::Name { .. })));
    assert_eq!(actions_for(&Command::OpenPermissions, &none), [A::SetupAllowed, A::RequestSystemAudio]);
    // The home move relaunches Heimdall: it goes last, after every other choice (review 5 Nov).
    let h = commands(&args(&["--home", "/Users/a/Work", "--t3", "connect", "--arsenal", "add", "--name", "Juno"]), None);
    assert!(matches!(h.last(), Some(Command::Home { .. })), "{h:?}");
    assert_eq!(actions_for(&Command::Finish, &none), [A::SetupDone]);
    assert_eq!(actions_for(&Command::Arsenal, &Context { arsenal_found: true, ..none.clone() }), [A::SetArsenal(true)]);
    // --defaults with claude-code connects T3 Code (the user's yes, given to their agent).
    let d = commands(&args(&["--defaults"]), Some("claude-code"));
    assert!(d.contains(&Command::T3 { connect: true }) && d.contains(&Command::DeciderPrecision { precision: "4bit".into() }));
}

#[test]
fn requests_round_trip_once_and_never_carry_a_key() {
    let dir = tempfile::tempdir().unwrap();
    let c = vec![Command::Provider { provider: "codex".into() }, Command::Name { name: "Juno".into() }];
    let path = write_request(dir.path(), &c).unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(!text.to_lowercase().contains("key"), "{text}");
    std::fs::write(dir.path().join("setup/requests/0-bad.json"), "not json").unwrap();
    assert_eq!(take_requests(dir.path()), c, "the bad one is dropped, the good one read");
    assert!(take_requests(dir.path()).is_empty(), "each is taken once");
}

/// `--check` reads the window's own rows: a red row with a step to go back to needs the user; the agent failing is 4.
#[test]
fn the_status_is_getting_readys_rows_and_its_exit_codes() {
    let models = ui::ModelDownloads { rows: vec![ui::ModelDownload { title: "Parakeet".into(), state: ui::DownloadState::Done, total: 1, done: 1, ..Default::default() }] };
    let row = |key: &str, state| ui::ReadyRow { key: key.into(), title: key.into(), detail: None, symbol: String::new(), state };
    let green = ui::SetupReadiness { rows: vec![row("agent", ui::ReadyState::Ready)] };
    let s = Status::of(&models, &green, false);
    assert_eq!((s.ready(), s.exit_code()), (true, 0));
    let choose = ui::SetupReadiness { rows: vec![row("agent", ui::ReadyState::Problem { why: "No agent chosen yet".into(), fix: Some(ui::ReadyFix::Step { title: "Choose…".into(), step: ui::SetupStep::Providers }) })] };
    assert_eq!(Status::of(&models, &choose, false).exit_code(), 2);
    let failed = ui::SetupReadiness { rows: vec![row("agent", ui::ReadyState::Problem { why: "HTTP 500".into(), fix: Some(ui::ReadyFix::Action { title: "Retry".into(), action: Box::new(ui::UiAction::CheckAgent) }) })] };
    assert_eq!(Status::of(&models, &failed, false).exit_code(), 4);
    let working = ui::SetupReadiness { rows: vec![row("voice", ui::ReadyState::Working { words: "Loading…".into(), progress: None })] };
    assert_eq!(Status::of(&models, &working, false).exit_code(), 3, "still on its way");
    // Saved and read back; an old file means Heimdall is not running.
    let dir = tempfile::tempdir().unwrap();
    s.save(dir.path()).unwrap();
    let back = Status::load(dir.path()).unwrap();
    assert_eq!(back, s);
    assert!(back.fresh(back.at_ms + 1000) && !back.fresh(back.at_ms + STALE.as_millis() as u64 + 1));
}

/// 5 Nov: `--check` has the Microphone row (no input needs the user) and one Decider row, its download
/// folded into it rather than listed twice.
#[test]
fn the_check_has_the_microphone_row_and_one_decider_row() {
    let models = ui::ModelDownloads { rows: ["Parakeet", "Decider"].iter().map(|t| ui::ModelDownload { title: (*t).into(), state: ui::DownloadState::Downloading, total: 2, done: 1, ..Default::default() }).collect() };
    let row = |key: &str, title: &str, state| ui::ReadyRow { key: key.into(), title: title.into(), detail: None, symbol: String::new(), state };
    let no_mic = ui::ReadyState::Problem { why: "No microphone found".into(), fix: Some(ui::ReadyFix::Step { title: "Choose…".into(), step: ui::SetupStep::Microphone }) };
    let ready = ui::SetupReadiness { rows: vec![row("microphone", "Microphone", no_mic), row("decider", "Decider", ui::ReadyState::Working { words: "Downloading".into(), progress: Some((1, 2)) })] };
    let s = Status::of(&models, &ready, false);
    let keys: Vec<&str> = s.rows.iter().map(|r| r.key.as_str()).collect();
    assert_eq!(keys, ["model:parakeet", "microphone", "decider"], "{keys:?}");
    let mic = &s.rows[1];
    assert_eq!((mic.state.as_str(), mic.why.as_deref(), mic.fix.as_deref()), ("needs_user", Some("No microphone found"), Some("Choose…")));
    assert_eq!(s.exit_code(), 2, "no microphone needs the user");
    // Memory search that can't run here is optional: reported, never waited for.
    let memory = ui::SetupReadiness { rows: vec![row("memory", "Memory search", ui::ReadyState::Optional { why: "Needs 8 GB free memory".into() })] };
    let done = ui::ModelDownloads { rows: vec![ui::ModelDownload { title: "EmbeddingGemma 2".into(), state: ui::DownloadState::Done, total: 1, done: 1, ..Default::default() }] };
    let m = Status::of(&done, &memory, false);
    assert_eq!((m.rows.len(), m.rows[0].state.as_str(), m.exit_code()), (1, "optional", 0), "{:?}", m.rows);
    // Without a decider row (the personal edition) its download stays listed.
    assert_eq!(Status::of(&models, &ui::SetupReadiness::default(), false).rows.len(), 2);
}

#[test]
fn without_a_running_heimdall_the_command_says_so() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(run(&["--check".into()], dir.path(), |_| None), 1);
    assert_eq!(run(&["--help".into()], dir.path(), |_| None), 0);
    // The plan needs no running app; without a provider it asks the one question (2).
    assert_eq!(run(&["--plan".into(), "--defaults".into()], dir.path(), |_| None), 2);
    assert_eq!(run(&["--plan".into(), "--provider".into(), "local".into()], dir.path(), |_| None), 0);
    let p = plan(&args(&["--provider", "local"]), Some("local"));
    assert!(p.iter().any(|(s, d)| s == "download:engine" && d.contains("17.4 GB")));
    assert!(p.iter().any(|(s, d)| s == "download:total" && d == "21.2 GB"), "{p:?}");
}
