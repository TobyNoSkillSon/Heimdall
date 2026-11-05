//! `Heimdall setup` (docs/setup.md, "Heimdall setup: setup from a coding agent"): a coding agent sets Heimdall up end
//! to end. The command never changes settings itself: it writes a request into `<data>/setup/requests/`, and the
//! running app turns each choice into the Setup window's own actions (`actions_for`, `Pipeline::setup_requests_tick`).
//! `--check` reads `<data>/setup/status.json`, which the app keeps current with Getting ready's rows (setup_ready.rs):
//! the window and the command share one set of actions and one set of checks.
//!
//! A key comes from an environment variable the agent names (`--custom-key-env`); it is checked and kept by this
//! process as the window keeps it, and never written to a request or printed.
use anyhow::{bail, Context as _, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

// ---- the status the app keeps ----

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    /// When the app wrote it (ms since the epoch): a file older than `STALE` means Heimdall is not running.
    pub at_ms: u64,
    /// The first Setup's Start was pressed.
    pub done: bool,
    pub rows: Vec<StatusRow>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusRow {
    pub key: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// `ready`, `working`, `needs_user`, `failed`, or `optional` (can't run here now and not needed to start).
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
    /// The fix the window offers ("Retry", "Sign In…").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
}

/// The app rewrites the status every `HEARTBEAT`; one older than this is from an app no longer running.
pub const STALE: Duration = Duration::from_secs(30);
pub const HEARTBEAT: Duration = Duration::from_secs(10);

fn setup_dir(data: &Path) -> PathBuf {
    data.join("setup")
}

impl Status {
    /// Getting ready's rows as the command reports them: a red row whose fix is a step of Setup needs the user; one
    /// whose fix is an action (Retry) failed.
    pub fn of(models: &ui::ModelDownloads, ready: &ui::SetupReadiness, done: bool) -> Status {
        // The decider has its own row (its download, then its load): its model row gives way to it.
        let decider_row = ready.rows.iter().any(|r| r.key == "decider");
        let memory_row = ready.rows.iter().any(|r| r.key == "memory");
        let mut rows: Vec<StatusRow> = models.rows.iter().filter(|r| !(decider_row && r.title == "Decider") && !(memory_row && r.title == "EmbeddingGemma 2")).map(|r| {
            let (state, why) = match &r.state {
                ui::DownloadState::Done => ("ready", None),
                ui::DownloadState::Waiting | ui::DownloadState::Downloading => ("working", Some(format!("{} of {} bytes", r.done, r.total))),
                ui::DownloadState::Failed(why) => ("failed", Some(why.clone())),
            };
            StatusRow { key: format!("model:{}", r.title.to_lowercase().replace(' ', "-")), title: r.title.clone(), detail: r.detail.clone(), state: state.into(), why,
                fix: matches!(r.state, ui::DownloadState::Failed(_)).then(|| "Retry".into()) }
        }).collect();
        rows.extend(ready.rows.iter().map(|r| {
            let (state, why, fix) = match &r.state {
                ui::ReadyState::Ready => ("ready", None, None),
                // Not needed to start: `--check` and `--wait` don't wait for it.
                ui::ReadyState::Optional { why, fix } => ("optional", Some(why.clone()), fix.as_ref().map(|f| f.title().to_string())),
                ui::ReadyState::Working { words, .. } | ui::ReadyState::Blocked { words } => ("working", Some(words.clone()), None),
                ui::ReadyState::Problem { why, fix: Some(f @ ui::ReadyFix::Step { .. }) } => ("needs_user", Some(why.clone()), Some(f.title().to_string())),
                ui::ReadyState::Problem { why, fix } => ("failed", Some(why.clone()), fix.as_ref().map(|f| f.title().to_string())),
            };
            StatusRow { key: r.key.clone(), title: r.title.clone(), detail: r.detail.clone(), state: state.into(), why, fix }
        }));
        Status { at_ms: agent::now_ms(), done, rows }
    }

    pub fn save(&self, data: &Path) -> Result<()> {
        let dir = setup_dir(data);
        std::fs::create_dir_all(&dir)?;
        let temp = dir.join("status.json.tmp");
        std::fs::write(&temp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&temp, dir.join("status.json"))?;
        Ok(())
    }

    pub fn load(data: &Path) -> Option<Status> {
        serde_json::from_slice(&std::fs::read(setup_dir(data).join("status.json")).ok()?).ok()
    }

    pub fn fresh(&self, now_ms: u64) -> bool {
        now_ms.saturating_sub(self.at_ms) < STALE.as_millis() as u64
    }

    /// Every row green (and some rows at all).
    pub fn ready(&self) -> bool {
        !self.rows.is_empty() && self.rows.iter().all(|r| r.state == "ready" || r.state == "optional")
    }

    /// The command's exit code for this status: 0 ready, 2 the user is needed, 4 a provider failed, 1 still working.
    pub fn exit_code(&self) -> i32 {
        if self.ready() { return 0; }
        if self.rows.iter().any(|r| r.state == "needs_user") { return 2; }
        if self.rows.iter().any(|r| r.key == "agent" && r.state == "failed") { return 4; }
        if self.rows.iter().any(|r| r.state == "failed") { return 1; }
        3
    }
}

// ---- the requests the command sends ----

/// One choice, as the command sends it. The app turns each into the window's actions (`actions_for`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "do", rename_all = "kebab-case")]
pub enum Command {
    Provider { provider: String },
    /// A custom server this process already verified and kept: the app's hub takes it up.
    CustomConnected { provider: String },
    DeciderPrecision { precision: String },
    Voice { name: String },
    UserName { name: String },
    Name { name: String },
    Mark { mark: String },
    Home { path: PathBuf },
    T3 { connect: bool },
    Harnesses { set: bool },
    Vella,
    Arsenal,
    OpenPermissions,
    Wizard,
    Finish,
}

pub fn write_request(data: &Path, commands: &[Command]) -> Result<PathBuf> {
    let dir = setup_dir(data).join("requests");
    std::fs::create_dir_all(&dir)?;
    let name = format!("{}-{}.json", agent::now_ms(), std::process::id());
    let temp = dir.join(format!(".{name}"));
    std::fs::write(&temp, serde_json::to_vec_pretty(commands)?)?;
    let path = dir.join(name);
    std::fs::rename(&temp, &path)?;
    Ok(path)
}

/// The requests waiting, oldest first; each file is removed as it is read (one that does not parse too, logged).
pub fn take_requests(data: &Path) -> Vec<Command> {
    let dir = setup_dir(data).join("requests");
    let Ok(entries) = std::fs::read_dir(&dir) else { return vec![] };
    let mut files: Vec<PathBuf> = entries.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "json")
        && !p.file_name().is_some_and(|n| n.to_string_lossy().starts_with('.'))).collect();
    files.sort();
    let mut commands = vec![];
    for f in files {
        match std::fs::read(&f).ok().and_then(|b| serde_json::from_slice::<Vec<Command>>(&b).ok()) {
            Some(c) => commands.extend(c),
            None => crate::diagnostics::line("setup", &format!("request {} not understood; dropped", f.display())),
        }
        let _ = std::fs::remove_file(&f);
    }
    commands
}

/// What the app knows when it applies a command.
#[derive(Clone, Debug, Default)]
pub struct Context {
    pub t3_ready: bool,
    pub t3_connected: bool,
    /// This Mac's model to download: (id, quant), when its weights are not here.
    pub engine_download: Option<(String, String)>,
    pub arsenal_found: bool,
}

/// One command as the Setup window's actions, in the order the window would send them.
pub fn actions_for(c: &Command, ctx: &Context) -> Vec<ui::UiAction> {
    use ui::UiAction as A;
    match c {
        Command::Provider { provider } => match provider.as_str() {
            "claude-code" => {
                let mut v = vec![A::ClaudeViaT3];
                if !ctx.t3_ready { v.push(A::InstallT3) } else if !ctx.t3_connected { v.push(A::ConnectT3) }
                v
            }
            "codex" => vec![A::StartProviderLogin { provider: agent::provider::CHATGPT.into() }],
            "opencode-go" => vec![A::StartProviderLogin { provider: agent::provider::OPENCODE_GO.into() }],
            "local" => {
                let mut v = vec![A::SetEngine(true)];
                if let Some((id, quant)) = &ctx.engine_download { v.push(A::DownloadEngineModel { id: id.clone(), quant: quant.clone(), then_select: true }) }
                v
            }
            _ => vec![],
        },
        Command::CustomConnected { .. } => vec![],
        Command::DeciderPrecision { precision } => vec![A::SetDeciderPrecision(precision.clone()), A::StartModelDownloads],
        Command::Voice { name } => vec![A::SpeechVoice(ui::speech_voice::SpeechVoiceAction::SetPreset(name.clone()))],
        Command::UserName { name } => vec![A::Identity(ui::identity::IdentityAction::SetUserName(name.clone()))],
        Command::Name { name } => vec![A::Identity(ui::identity::IdentityAction::SetName(name.clone()))],
        Command::Mark { mark } => {
            let icon = if mark == "helmet" { ui::identity::Icon::Helmet } else { ui::identity::Icon::Symbol { name: mark.clone() } };
            let mut v = vec![A::Identity(ui::identity::IdentityAction::SetIcon(icon.clone()))];
            // Its persona, as the window's mark click brings it; a --name or --voice given comes after and wins.
            if let Some((name, voice)) = ui::identity::persona(&icon) {
                v.push(A::Identity(ui::identity::IdentityAction::SetName(name.into())));
                v.push(A::SpeechVoice(ui::speech_voice::SpeechVoiceAction::SetPreset(voice.into())));
            }
            v
        }
        Command::Home { path } => vec![A::ChooseHome(path.clone()), A::ConfirmHomeMove],
        Command::T3 { connect: true } => if ctx.t3_ready { if ctx.t3_connected { vec![] } else { vec![A::ConnectT3] } } else { vec![A::InstallT3] },
        Command::T3 { connect: false } => vec![],
        Command::Harnesses { set } => vec![A::SetHarnessAutonomy(*set)],
        Command::Vella => vec![A::InstallVella],
        Command::Arsenal => if ctx.arsenal_found { vec![A::SetArsenal(true)] } else { vec![A::InstallArsenal] },
        Command::OpenPermissions => vec![A::SetupAllowed, A::RequestSystemAudio],
        Command::Wizard => vec![],
        Command::Finish => vec![A::SetupDone],
    }
}

/// One action as a click: through the menu's own callback (its intent latch included), as the `settings` tool does.
pub fn send(tools: &crate::bridge::Tools, action: ui::UiAction) {
    match tools.mcp_actions.menu.lock().unwrap().clone() {
        Some(menu) => menu(action),
        None => crate::diagnostics::line("setup", "the menu is not running; a Heimdall setup choice was not applied"),
    }
}

// ---- the command ----

/// Parsed flags.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Args {
    pub mode: Mode,
    pub json: bool,
    pub defaults: bool,
    pub provider: Option<String>,
    pub custom_url: Option<String>,
    pub custom_kind: Option<String>,
    pub custom_key_env: Option<String>,
    pub precision: Option<String>,
    pub voice: Option<String>,
    pub user_name: Option<String>,
    pub name: Option<String>,
    pub mark: Option<String>,
    pub home: Option<PathBuf>,
    pub t3: Option<bool>,
    pub harnesses: Option<bool>,
    pub vella: bool,
    pub arsenal: bool,
    pub open_permissions: bool,
    pub finish: bool,
    pub timeout: Option<u64>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    #[default]
    Apply,
    Help,
    Plan,
    Check,
    Wait,
    Wizard,
}

pub const PROVIDERS: [&str; 5] = ["claude-code", "codex", "opencode-go", "local", "custom"];
pub const MARKS: [&str; 12] = ["helmet", "sparkle", "leaf", "moon.stars", "bird", "cat", "hare", "flame", "bolt", "crown", "eye", "atom"];

pub fn parse(args: &[String]) -> Result<Args> {
    let mut a = Args::default();
    let mut it = args.iter();
    let value = |it: &mut std::slice::Iter<String>, flag: &str| it.next().cloned().with_context(|| format!("{flag} needs a value"));
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--help" | "-h" => a.mode = Mode::Help,
            "--plan" => a.mode = Mode::Plan,
            "--check" => a.mode = Mode::Check,
            "--wait" => a.mode = Mode::Wait,
            "--wizard" => a.mode = Mode::Wizard,
            "--json" => a.json = true,
            "--defaults" => a.defaults = true,
            "--provider" => {
                let p = value(&mut it, flag)?;
                if !PROVIDERS.contains(&p.as_str()) { bail!("--provider is one of {}", PROVIDERS.join(", ")) }
                a.provider = Some(p);
            }
            "--custom-url" => a.custom_url = Some(value(&mut it, flag)?),
            "--custom-kind" => {
                let k = value(&mut it, flag)?;
                if !["openai", "anthropic"].contains(&k.as_str()) { bail!("--custom-kind is openai or anthropic") }
                a.custom_kind = Some(k);
            }
            "--custom-key-env" => a.custom_key_env = Some(value(&mut it, flag)?),
            "--decider-precision" => {
                let p = value(&mut it, flag)?;
                a.precision = Some(match p.as_str() { "4" | "4bit" => "4bit", "8" | "8bit" => "8bit", "16" | "16bit" => "16bit", _ => bail!("--decider-precision is 4, 8 or 16") }.into());
            }
            "--voice" => a.voice = Some(value(&mut it, flag)?),
            "--user-name" => a.user_name = Some(value(&mut it, flag)?),
            "--name" => a.name = Some(value(&mut it, flag)?),
            "--mark" => {
                let m = value(&mut it, flag)?;
                if !MARKS.contains(&m.as_str()) { bail!("--mark is one of {}", MARKS.join(", ")) }
                a.mark = Some(m);
            }
            "--home" => a.home = Some(PathBuf::from(value(&mut it, flag)?)),
            "--t3" => a.t3 = Some(match value(&mut it, flag)?.as_str() { "connect" => true, "skip" => false, _ => bail!("--t3 is connect or skip") }),
            "--harnesses" => a.harnesses = Some(match value(&mut it, flag)?.as_str() { "set" => true, "leave" => false, _ => bail!("--harnesses is set or leave") }),
            "--vella" => a.vella = match value(&mut it, flag)?.as_str() { "add" => true, "skip" => false, _ => bail!("--vella is add or skip") },
            "--arsenal" => a.arsenal = match value(&mut it, flag)?.as_str() { "add" => true, "skip" => false, _ => bail!("--arsenal is add or skip") },
            "--open-permissions" => a.open_permissions = true,
            "--finish" => a.finish = true,
            "--timeout" => a.timeout = Some(value(&mut it, flag)?.parse().context("--timeout is seconds")?),
            other => bail!("unknown flag {other} (see --help)"),
        }
    }
    if a.provider.as_deref() == Some("custom") && a.custom_url.is_none() { bail!("--provider custom needs --custom-url") }
    Ok(a)
}

/// `--defaults`' provider: the coding agent it runs inside, when that is a subscription Heimdall can use.
pub fn default_provider(env: impl Fn(&str) -> Option<String>) -> Option<&'static str> {
    if env("CLAUDECODE").as_deref() == Some("1") { return Some("claude-code") }
    if env("CODEX_HOME").is_some() || env("CODEX_SANDBOX").is_some() { return Some("codex") }
    None
}

/// The one question, when the provider can't be decided.
pub const QUESTION: &str = "Which should run Heimdall's agent: your Claude Code, Codex or OpenCode Go subscription, this Mac (a 17.4 GB model), or a server of your own?";

/// The commands the flags ask for (the provider resolved), in the window's order.
pub fn commands(a: &Args, provider: Option<&str>) -> Vec<Command> {
    let mut c = vec![];
    if let Some(n) = &a.user_name { c.push(Command::UserName { name: n.clone() }) }
    if let Some(p) = provider.filter(|p| *p != "custom") { c.push(Command::Provider { provider: p.into() }) }
    let precision = a.precision.clone().or_else(|| a.defaults.then(|| "4bit".to_string()));
    if let Some(p) = precision { c.push(Command::DeciderPrecision { precision: p }) }
    // A mark brings its persona (name and voice); the name and voice given come after it and win.
    if let Some(m) = &a.mark { c.push(Command::Mark { mark: m.clone() }) }
    if let Some(v) = &a.voice { c.push(Command::Voice { name: v.clone() }) }
    if let Some(n) = &a.name { c.push(Command::Name { name: n.clone() }) }
    if a.open_permissions { c.push(Command::OpenPermissions) }
    let t3 = a.t3.or_else(|| (provider == Some("claude-code")).then_some(true));
    if let Some(connect) = t3 { c.push(Command::T3 { connect }) }
    if let Some(set) = a.harnesses { c.push(Command::Harnesses { set }) }
    if a.vella { c.push(Command::Vella) }
    if a.arsenal { c.push(Command::Arsenal) }
    // Last: moving the home relaunches Heimdall, and nothing queued after it would survive.
    if let Some(h) = &a.home { c.push(Command::Home { path: h.clone() }) }
    c
}

/// `--plan`: what would happen, as lines (step, detail).
pub fn plan(a: &Args, provider: Option<&str>) -> Vec<(String, String)> {
    let lock = crate::model_store::Lock::embedded();
    let precision = a.precision.clone().unwrap_or_else(|| "4bit".into());
    let p = decide::precision::Precision::from_key(&precision).unwrap_or_default();
    let mut lines = vec![];
    let mut total = 0;
    for id in crate::model_store::first_run_groups(p) {
        if let Some(g) = lock.group(&id) {
            total += g.bytes();
            lines.push((format!("download:{id}"), format!("{} {} from Hugging Face", g.title, crate::pipeline::setup_ready::gb(g.bytes()))));
        }
    }
    match provider {
        Some("claude-code") => lines.push(("agent".into(), "Claude Code through T3 Code (installed if missing, connected); needs Claude Code signed in".into())),
        Some("codex") => lines.push(("agent".into(), "Codex: the user approves the ChatGPT sign-in in the browser".into())),
        Some("opencode-go") => lines.push(("agent".into(), "OpenCode Go: the user approves the device sign-in in the browser".into())),
        Some("local") => {
            lines.push(("agent".into(), "This Mac: Qwen3.8 27B on the built-in engine, needs about 20 GB of free memory".into()));
            lines.push(("download:engine".into(), "Qwen3.8 27B 17.4 GB from Hugging Face".into()));
            total += 17_378_626_464;
        }
        Some("custom") => lines.push(("agent".into(), format!("Custom server {}", a.custom_url.clone().unwrap_or_default()))),
        _ => lines.push(("agent".into(), format!("needs the user: {QUESTION}"))),
    }
    lines.push(("download:total".into(), crate::pipeline::setup_ready::gb(total)));
    if a.open_permissions {
        lines.push(("permissions".into(), "the user clicks Allow for the Microphone (required), and may turn on Screen Recording and Accessibility in System Settings (optional)".into()));
    }
    lines
}

fn out(json: bool, step: &str, state: &str, detail: &str) {
    if json {
        println!("{}", serde_json::json!({"step": step, "state": state, "detail": detail}));
    } else {
        println!("{step}: {detail}");
    }
}

fn print_status(json: bool, s: &Status) {
    for r in &s.rows {
        let detail = [r.detail.clone(), r.why.clone(), r.fix.clone().map(|f| format!("fix: {f}"))].into_iter().flatten().collect::<Vec<_>>().join(" \u{b7} ");
        out(json, &format!("ready:{}", r.key), &r.state, &detail);
    }
}

pub const HELP: &str = "Heimdall setup: set Heimdall up from a coding agent (docs/setup.md is the full reference).

  setup --plan [flags] [--json]       what would happen; changes nothing
  setup --defaults [flags] [--json]   the window's suggestions, with any flag given taking precedence
  setup [flags] [--json]              only the flags given
  setup --check [--json]              Getting ready's checklist now (exit 0 all green, 2 needs the user, 4 agent failed)
  setup --wait [--timeout S] [--json] --check until green, the user is needed, or the timeout (default 1800 s)
  setup --wizard                      open the Setup window

Flags: --provider claude-code|codex|opencode-go|local|custom  --custom-url URL  --custom-kind openai|anthropic
  --custom-key-env VAR (the variable's NAME; never a key)  --decider-precision 4|8|16  --voice NAME  --user-name TEXT
  --name TEXT  --mark helmet|sparkle|leaf|moon.stars|bird|cat|hare|flame|bolt|crown|eye|atom  --home PATH
  --t3 connect|skip  --harnesses set|leave  --vella add|skip  --arsenal add|skip  --open-permissions  --finish

For an agent: ask the user one question only if --defaults can't pick the provider (exit 2 says so). Run --plan and
show the user; run with --open-permissions and tell the user, one sentence each, the clicks only they can make
(Microphone, required: Allow in the prompt; Screen Recording and Accessibility, optional: System Settings > Privacy &
Security; a browser sign-in). Then --wait; when it exits 0, run --finish. A red Microphone row (\"No microphone
found\", \"No sound from ...\") needs the user to plug one in or pick one in Setup. Heimdall must be running.";

/// `Heimdall setup …`: prints its lines and returns the exit code.
pub fn run(args: &[String], data: &Path, env: impl Fn(&str) -> Option<String>) -> i32 {
    let a = match parse(args) {
        Ok(a) => a,
        Err(e) => { eprintln!("{e:#}"); return 1; }
    };
    if a.mode == Mode::Help { println!("{HELP}"); return 0; }
    let provider = a.provider.as_deref().or_else(|| if a.defaults { default_provider(&env) } else { None });
    if a.mode == Mode::Plan {
        for (step, detail) in plan(&a, provider) { out(a.json, &step, if step == "agent" && provider.is_none() { "needs_user" } else { "planned" }, &detail); }
        return if provider.is_none() && (a.defaults || a.provider.is_some()) { 2 } else { 0 };
    }
    let now = agent::now_ms();
    let status = Status::load(data).filter(|s| s.fresh(now));
    if status.is_none() {
        out(a.json, "heimdall", "failed", "Heimdall isn't running: open Heimdall Alpha, then run this again");
        return 1;
    }
    match a.mode {
        Mode::Check => {
            let s = status.unwrap_or_default();
            print_status(a.json, &s);
            // Still on its way is "anything else" (1) for --check; --wait waits it out.
            return match s.exit_code() { 3 => 1, code => code };
        }
        Mode::Wait => {
            let until = std::time::Instant::now() + Duration::from_secs(a.timeout.unwrap_or(1800));
            loop {
                let s = Status::load(data).unwrap_or_default();
                if !s.fresh(agent::now_ms()) {
                    out(a.json, "heimdall", "failed", "Heimdall stopped running");
                    return 1;
                }
                let code = s.exit_code();
                if code == 0 || code == 2 || code == 4 || std::time::Instant::now() >= until {
                    print_status(a.json, &s);
                    return if code == 3 { 1 } else { code };
                }
                std::thread::sleep(Duration::from_secs(5));
            }
        }
        Mode::Wizard => {
            return match write_request(data, &[Command::Wizard]) {
                Ok(_) => { out(a.json, "wizard", "done", "Setup opens in Heimdall"); 0 }
                Err(e) => { out(a.json, "wizard", "failed", &format!("{e:#}")); 1 }
            };
        }
        _ => {}
    }
    let mut commands = commands(&a, provider);
    if provider == Some("custom") {
        let kind = if a.custom_kind.as_deref() == Some("anthropic") { "anthropic-compatible" } else { "openai-compatible" };
        let key = match &a.custom_key_env {
            Some(var) => match env(var) { Some(k) => k, None => { out(a.json, "agent", "failed", &format!("${var} is not set")); return 4; } },
            None => String::new(),
        };
        match crate::api_providers::verify_and_save_endpoint(kind, a.custom_url.as_deref().unwrap_or_default(), &key) {
            Ok(_) => { out(a.json, "agent", "done", "the custom server answered and is kept"); commands.insert(0, Command::CustomConnected { provider: kind.into() }) }
            Err(why) => { out(a.json, "agent", "failed", &why); return 4; }
        }
    }
    if a.finish {
        let ready = status.as_ref().is_some_and(Status::ready);
        if !ready {
            out(a.json, "finish", "needs_user", "not every Getting ready row is green yet: run --check");
            if let Some(s) = &status { print_status(a.json, s) }
            return 2;
        }
        commands.push(Command::Finish);
    }
    if commands.is_empty() && provider.is_none() && a.defaults {
        out(a.json, "agent", "needs_user", QUESTION);
        return 2;
    }
    match write_request(data, &commands) {
        Ok(_) => {
            for c in &commands { out(a.json, &step_name(c), "done", "handed to Heimdall") }
            if provider.is_none() && a.defaults { out(a.json, "agent", "needs_user", QUESTION); return 2; }
            if matches!(provider, Some("codex" | "opencode-go")) { out(a.json, "agent", "needs_user", "approve the sign-in in the browser page Heimdall opened"); return 2; }
            if a.open_permissions { out(a.json, "permissions", "needs_user", "click Allow for the Microphone (required); Screen Recording and Accessibility in System Settings are optional"); return 2; }
            0
        }
        Err(e) => { out(a.json, "request", "failed", &format!("{e:#}")); 1 }
    }
}

fn step_name(c: &Command) -> String {
    serde_json::to_value(c).ok().and_then(|v| v["do"].as_str().map(str::to_owned)).unwrap_or_default()
}

#[cfg(test)]
#[path = "setup_cli_tests.rs"]
mod tests;
