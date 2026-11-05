//! Setup's actions on the app side (the wizard itself is `ui::mac::setup`): when it opens by itself, the home
//! folder choice and move (through `crate::home_change`), T3 Code (detection, the Connect consent, install), the
//! coding agents set to work without asking (`crate::harness_autonomy`; the public edition's Set Up or Leave as Is), and
//! Vella (detection; a Homebrew install only on its button). The provider rows send the Add Provider menu's own
//! actions, handled where the menu's are.

use crate::pipeline::Pipeline;
use crate::home_change;
use heimdall_home::{Env, Location, MovePlan};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// Whether Setup opens by itself at launch, and where: at the step a first Setup was on when Heimdall stopped
/// (`resume`; VM run 5 Nov: the watchdog relaunched it mid-Setup and the window vanished); else at Welcome, the whole
/// tour, until setup was requested (both editions); then, in the public edition only, the home step while no home
/// folder was chosen. The personal edition never opens it for its unmoved (legacy) profile; its owner opens it from the
/// menu.
pub fn first_launch(public: bool, setup_requested: bool, resume: Option<ui::SetupStep>, needs_choice: impl FnOnce() -> bool) -> Option<ui::SetupStep> {
    if resume.is_some() {
        return resume;
    }
    if !setup_requested {
        return Some(ui::SetupStep::Welcome);
    }
    (public && needs_choice()).then_some(ui::SetupStep::Home)
}

/// Whether Setup's recorded step is past the listening page, whose Continue starts the model downloads.
pub(crate) fn past_listening(step: Option<&str>) -> bool {
    let at = |s: ui::SetupStep| ui::SetupStep::ALL.iter().position(|x| *x == s);
    step.and_then(ui::SetupStep::from_key).is_some_and(|s| at(s) > at(ui::SetupStep::Decider))
}

/// Whether the model downloads are wanted at launch (public edition): after Setup's Allow All, or once Setup passed
/// the listening page (review 5 Nov: quit on Voice or Allow, relaunch, and nothing downloaded; Start never enabled).
pub(crate) fn downloads_wanted(settings: &crate::settings::Settings) -> bool {
    settings.setup_requested || past_listening(settings.setup_step.as_deref())
}

/// The first Setup has not been finished: never greeted and its Done not pressed yet.
pub fn setup_unfinished(s: &crate::settings::Settings) -> bool {
    s.first_run_greeted == Some(false) && !s.first_run_greeting_due
}

/// Where a relaunch reopens Setup (`first_launch`): the saved step, while the first Setup is not finished.
pub fn resume_step(s: &crate::settings::Settings) -> Option<ui::SetupStep> {
    s.setup_step.as_deref().filter(|_| setup_unfinished(s)).and_then(ui::SetupStep::from_key)
}

/// The one wake Setup's Done sends, once per profile ("we just send it as a prompt"). Only the public
/// edition ships the `onboarding` skill. `check`: the installed CLI and Heimdall's data folder, when Setup set the
/// coding agents to work without asking (`harness_autonomy::wanted`); the agent verifies that once and reports it in a
/// sentence.
pub fn greeting(public: bool, check: Option<(&Path, &Path)>) -> String {
    // The spoken greeting is the user's only sign that Heimdall started, so it is short and spoken.
    let mut text = String::from(GREETING);
    if public {
        // The tour waits until they ask for it (the `onboarding` skill's "First run").
        text.push_str(" A tour of what you can do waits until they ask (the `onboarding` skill).");
    }
    if let Some((cli, data)) = check {
        let quote = |p: &Path| format!("'{}'", p.display().to_string().replace('\'', "'\\''"));
        text.push_str(&format!(
            " Setup has set the coding agents on this Mac (Claude Code, T3 Code, Codex, OpenCode and Grok, whichever are \
             installed) to work without asking for permission. For Claude Code and T3 Code's Claude agents a short deny \
             list still refuses deletes outside the home folder, disk erasing, the Keychain and system settings; Codex, \
             OpenCode and Grok are not guarded by it. Verify it once with `{} --data-dir {} harness check` and tell them \
             in one sentence which agents are set, or what is not and why. Do not mention it again.",
            quote(cli),
            quote(data)
        ));
    }
    text
}

/// The first-run message's intent, then what the reply is: the spoken greeting is the confirmation
/// that Heimdall started; nothing on screen says so.
pub const GREETING: &str = "The user just finished setting you up. Greet them and talk to them. Your first reply is a short \
     spoken welcome: a sentence or two with `speak`, no list of what you can do.";

/// The greeting goes to the agent once Setup's Done was pressed (`due`) on a profile never greeted
/// (`Settings::first_run_greeted`), and is then marked sent. A profile set up before it, or one already greeted, gets
/// nothing. A delivery that fails (Agent off) stays due for the agent's next start (`Pipeline::ensure_agent`).
/// Returns whether the settings changed (to be saved).
pub fn greet(greeted: &mut Option<bool>, due: &mut bool, text: &str, send: impl FnOnce(&str, &str) -> anyhow::Result<()>) -> bool {
    if *greeted != Some(false) {
        return std::mem::take(due);
    }
    if let Err(e) = send(agent::notice::FIRST_RUN, text) {
        crate::diagnostics::line("setup", &format!("first-run greeting not delivered yet: {e:#}"));
        return false;
    }
    *greeted = Some(true);
    *due = false;
    true
}

impl Pipeline {
    /// The first-run greeting, when Setup's Done is waiting for it (`greet`); saved when anything changed.
    pub(crate) fn greet_if_due(&mut self) {
        // Agent off, or its switch-on check still running: the greeting waits for the agent's start.
        if !self.settings.first_run_greeting_due || self.settings.dry_run || self.preflight_pending() {
            return;
        }
        let tools = self.tools.clone();
        let public = crate::edition::PUBLIC;
        let check = crate::harness_autonomy::wanted(public, self.settings.harness_autonomy).then(crate::harness_autonomy::bundled_cli).flatten();
        let text = greeting(public, check.as_deref().map(|cli| (cli, self.data.as_path())));
        let s = &mut self.settings;
        if greet(&mut s.first_run_greeted, &mut s.first_run_greeting_due, &text, |id, text| tools.agent_enabled.permit().and_then(|permit| tools.notice(id, text, &permit))) {
            self.save_or_note();
        }
    }
}

/// Whether Heimdall may connect to T3 Code: the personal edition always (as before); the public edition only
/// after Setup's Connect, which is saved.
pub fn t3_allowed(public: bool, connect_saved: bool) -> bool {
    !public || connect_saved
}

static CONSENT: OnceLock<Arc<AtomicBool>> = OnceLock::new();

/// The switch every T3 client of this process checks (`t3::Client::with_consent`). Closed in the public edition
/// until the settings say otherwise.
pub fn t3_consent() -> Arc<AtomicBool> {
    CONSENT.get_or_init(|| Arc::new(AtomicBool::new(t3_allowed(crate::edition::PUBLIC, false)))).clone()
}

/// At launch, from the saved settings.
pub fn load_consent(settings: &crate::settings::Settings) {
    t3_consent().store(t3_allowed(crate::edition::PUBLIC, settings.t3_connect_allowed), Ordering::Release);
}

/// The move shown in Setup, waiting for its confirm.
static PLAN: Mutex<Option<MovePlan>> = Mutex::new(None);
static INSTALLING: AtomicBool = AtomicBool::new(false);

pub fn home_state(loc: &Location, needs_choice: bool, pending: Option<&MovePlan>, error: Option<String>) -> ui::HomeState {
    ui::HomeState {
        home: loc.home.clone(),
        data: loc.data.clone(),
        needs_choice,
        pending: pending.map(|p| ui::HomeMove { to_home: p.to_home.clone(), to: p.to.clone(), bytes: p.bytes }),
        error,
    }
}

/// Choose…: what moving there will do (`home_change::plan`), or why not. Writes nothing.
pub fn choose(loc: &Location, planned: Result<MovePlan, String>) -> (Option<MovePlan>, ui::HomeState) {
    match planned {
        Ok(plan) => {
            let state = home_state(loc, loc.needs_choice(), Some(&plan), None);
            (Some(plan), state)
        }
        Err(e) => (None, home_state(loc, loc.needs_choice(), None, Some(e))),
    }
}

/// Setup's line when the move is recorded but Heimdall could not start its fresh copy.
pub const RESTART_FAILED: &str = "Heimdall couldn\u{2019}t restart. Quit and open it again to finish the move.";

/// The move shown was confirmed: recorded (`home_change::request`, which refuses a folder named at launch), then a
/// fresh copy is started (`home_change::relaunch`) and this one quits through the menu's Quit. A refusal or a
/// failed restart shows in Setup, and nothing quits.
pub fn confirm(
    loc: &Location,
    plan: Option<MovePlan>,
    request: impl FnOnce(&Path) -> Result<MovePlan, String>,
    relaunch: impl FnOnce() -> Result<(), String>,
) -> Vec<ui::UiUpdate> {
    let Some(plan) = plan else { return vec![] };
    if let Err(e) = request(&plan.to_home) {
        return vec![ui::UiUpdate::Home(home_state(loc, loc.needs_choice(), None, Some(e)))];
    }
    match relaunch() {
        Ok(()) => vec![ui::UiUpdate::Quit],
        Err(e) => {
            eprintln!("setup: home move recorded; the restart failed: {e}");
            // The reason goes to the log; Setup says what to do, on one line.
            vec![ui::UiUpdate::Home(home_state(loc, false, None, Some(RESTART_FAILED.into())))]
        }
    }
}

/// T3 Code as Setup shows it.
pub fn t3_state(detection: &t3::install::Detection, connect_allowed: bool, install: ui::T3Install) -> ui::T3Setup {
    ui::T3Setup {
        found: detection.best().map(|a| ui::T3App { path: a.path.clone(), name: a.name.clone(), version: a.version.clone(), supports_v2: a.supports_v2() }),
        connect_allowed,
        install,
    }
}

fn detect_t3() -> t3::install::Detection {
    let home = Env::current().user_home;
    t3::install::detect(&home, &t3::install::app_dirs(&home))
}

pub(crate) fn current_t3(install: ui::T3Install) -> ui::T3Setup {
    t3_state(&detect_t3(), t3_consent().load(Ordering::Acquire), install)
}

pub const VELLA_ID: &str = "dev.vella.dictation";
pub const VELLA_CASK: &str = "tobynoskillson/tap/vella";
static VELLA_INSTALLING: AtomicBool = AtomicBool::new(false);

/// Vella in these folders: an `.app` whose Info.plist names Vella's bundle ID.
pub fn find_vella(dirs: &[std::path::PathBuf]) -> Option<ui::VellaApp> {
    find_app(dirs, VELLA_ID).map(|(path, version)| ui::VellaApp { path, version })
}

/// The first `.app` in these folders whose Info.plist names `id`: its path and version.
fn find_app(dirs: &[std::path::PathBuf], id: &str) -> Option<(std::path::PathBuf, String)> {
    let read = |plist: &str, key: &str| -> Option<String> {
        let after = &plist[plist.find(&format!("<key>{key}</key>"))?..];
        let start = after.find("<string>")? + "<string>".len();
        Some(after[start..start + after[start..].find("</string>")?].trim().to_string())
    };
    dirs.iter().filter_map(|d| std::fs::read_dir(d).ok()).flat_map(|d| d.flatten()).map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "app"))
        .find_map(|path| {
            let plist = std::fs::read_to_string(path.join("Contents/Info.plist")).ok()?;
            (read(&plist, "CFBundleIdentifier")? == id).then(|| (path, read(&plist, "CFBundleShortVersionString").unwrap_or_default()))
        })
}

/// Arsenal (github.com/TobyNoSkillSon/Arsenal): its bundle ID, its Homebrew cask (the public tap), and the MCP bridge
/// inside the app, which finds the running app and its access token on its own (Arsenal's README, "Connect an agent
/// over MCP").
pub const ARSENAL_ID: &str = "dev.arsenal.app";
pub const ARSENAL_CASK: &str = "tobynoskillson/tap/arsenal";
pub const ARSENAL_BRIDGE: &str = "Contents/Helpers/arsenal-mcp";
static ARSENAL_INSTALLING: AtomicBool = AtomicBool::new(false);

/// Arsenal in these folders (`/Applications`, `~/Applications`), when its bridge is inside.
pub fn find_arsenal(dirs: &[std::path::PathBuf]) -> Option<ui::ArsenalApp> {
    find_app(dirs, ARSENAL_ID).filter(|(path, _)| path.join(ARSENAL_BRIDGE).is_file()).map(|(path, version)| ui::ArsenalApp { path, version })
}

pub fn arsenal_install_command(brew: &Path) -> std::process::Command {
    let mut command = std::process::Command::new(brew);
    command.args(["install", "--cask", ARSENAL_CASK]).stdin(std::process::Stdio::null());
    command
}

pub fn current_arsenal(install: ui::T3Install, settings: &crate::settings::Settings) -> ui::ArsenalSetup {
    let home = Env::current().user_home;
    let brews: Vec<&Path> = BREWS.iter().map(Path::new).collect();
    let found = find_arsenal(&t3::install::app_dirs(&home));
    let on = settings.arsenal_mcp.as_deref().is_some_and(Path::is_file);
    ui::ArsenalSetup { found, brew: find_brew(&brews).is_some(), install, on }
}

/// Setup ▸ Arsenal ▸ Use it (or an install that landed): the bridge of the Arsenal found; Not now: none. Returns
/// whether the setting changed.
pub fn set_arsenal(settings: &mut crate::settings::Settings, on: bool, found: Option<&ui::ArsenalApp>) -> bool {
    let next = if on { found.map(|a| a.path.join(ARSENAL_BRIDGE)) } else { None };
    if settings.arsenal_mcp == next { return false; }
    settings.arsenal_mcp = next;
    true
}

/// Homebrew, where its installer puts it (Apple silicon first).
pub fn find_brew(candidates: &[&Path]) -> Option<std::path::PathBuf> {
    candidates.iter().find(|p| p.is_file()).map(|p| p.to_path_buf())
}

const BREWS: [&str; 2] = ["/opt/homebrew/bin/brew", "/usr/local/bin/brew"];

/// The one command the Install button runs.
pub fn vella_install_command(brew: &Path) -> std::process::Command {
    let mut command = std::process::Command::new(brew);
    command.args(["install", "--cask", VELLA_CASK]).stdin(std::process::Stdio::null());
    command
}

fn current_vella(install: ui::T3Install) -> ui::VellaSetup {
    let home = Env::current().user_home;
    let brews: Vec<&Path> = BREWS.iter().map(Path::new).collect();
    ui::VellaSetup { found: find_vella(&t3::install::app_dirs(&home)), brew: find_brew(&brews).is_some(), install }
}

/// Handles one Setup action; returns what the UI shows next.
pub fn action(p: &mut Pipeline, a: ui::UiAction) -> Vec<ui::UiUpdate> {
    let loc = &home_change::current();
    match a {
        ui::UiAction::SetupOpened => {
            let pending = PLAN.lock().unwrap().clone();
            let install = if INSTALLING.load(Ordering::Acquire) { ui::T3Install::Working("Installing T3 Code…".into()) } else { ui::T3Install::Idle };
            let vella = if VELLA_INSTALLING.load(Ordering::Acquire) { ui::T3Install::Working("Installing Vella with Homebrew…".into()) } else { ui::T3Install::Idle };
            let arsenal = if ARSENAL_INSTALLING.load(Ordering::Acquire) { ui::T3Install::Working("Installing Arsenal with Homebrew…".into()) } else { ui::T3Install::Idle };
            vec![
                ui::UiUpdate::Home(home_state(loc, loc.needs_choice(), pending.as_ref(), None)),
                ui::UiUpdate::T3Setup(current_t3(install)),
                ui::UiUpdate::VellaSetup(current_vella(vella)),
                ui::UiUpdate::ArsenalSetup(current_arsenal(arsenal, &p.settings)),
                ui::UiUpdate::HarnessSetup(crate::harness_autonomy::current(&p.data, p.settings.harness_autonomy)),
            ]
        }
        ui::UiAction::ChooseHome(path) => {
            let (plan, state) = choose(loc, home_change::plan(&path));
            *PLAN.lock().unwrap() = plan;
            vec![ui::UiUpdate::Home(state)]
        }
        ui::UiAction::ConfirmHomeMove => {
            let plan = PLAN.lock().unwrap().take();
            confirm(loc, plan, home_change::request, home_change::relaunch)
        }
        ui::UiAction::KeepHome => match home_change::keep() {
            Ok(()) => vec![ui::UiUpdate::Home(home_state(loc, false, None, None))],
            Err(e) => vec![ui::UiUpdate::Home(home_state(loc, loc.needs_choice(), None, Some(e)))],
        },
        // The step on screen, kept only while the first Setup is not done. A step past the listening page with no
        // download begun (a relaunch, the CLI) starts them, so Getting ready lists every model.
        ui::UiAction::SetupAt(step) => {
            if setup_unfinished(&p.settings) && p.settings.setup_step.as_deref() != Some(step.key().as_str()) {
                p.settings.setup_step = Some(step.key());
                p.save_or_note();
            }
            if past_listening(Some(&step.key())) && crate::model_store::global().is_some_and(|d| d.rows().is_empty()) {
                p.start_model_downloads();
            }
            vec![]
        }
        ui::UiAction::SetupDone => {
            if p.settings.setup_step.take().is_some() { p.save_or_note(); }
            // Start: the agent was checked on Getting ready; it starts now, so its first-run greeting can go.
            if crate::edition::PUBLIC && p.settings.dry_run && matches!(p.agent_check, crate::pipeline::setup_ready::AgentCheck::Passed(_)) {
                crate::setup_cli::send(&p.tools, ui::UiAction::SetDryRun(false));
            }
            // A fresh setup ends with every agent harness in its no-prompt mode (docs/harness-autonomy.md): always in the
            // personal edition, after Set Up in the public one.
            crate::harness_autonomy::start(&p.data, p.settings.harness_autonomy, true);
            // Saved before the send, so a greeting that cannot go yet still goes at the agent's next start.
            if p.settings.first_run_greeted == Some(false) && !p.settings.first_run_greeting_due {
                p.settings.first_run_greeting_due = true;
                p.save_or_note();
            }
            p.greet_if_due();
            // Closing Setup no longer quits (the menu's copy of `setup_done`).
            p.runtime_settings_ui();
            vec![]
        }
        ui::UiAction::ConnectT3 => {
            p.settings.t3_connect_allowed = true;
            p.save_or_note();
            t3_consent().store(true, Ordering::Release);
            crate::harness_autonomy::start(&p.data, p.settings.harness_autonomy, true);
            vec![ui::UiUpdate::T3Setup(current_t3(ui::T3Install::Idle))]
        }
        // Coding Agents: Set Up sets every harness found now and at each start; Leave as Is touches nothing. Saved.
        ui::UiAction::SetHarnessAutonomy(on) => {
            p.settings.harness_autonomy = Some(on);
            p.save_or_note();
            crate::harness_autonomy::start(&p.data, Some(on), false);
            vec![]
        }
        ui::UiAction::InstallT3 => {
            if INSTALLING.swap(true, Ordering::AcqRel) {
                return vec![];
            }
            let ui_handle = p.ui;
            std::thread::Builder::new()
                .name("t3-install".into())
                .spawn(move || {
                    let send = |u: ui::UiUpdate| if let Some(h) = &ui_handle { h.send(u) };
                    let user_apps = Env::current().user_home.join("Applications");
                    let progress = |stage: t3::install::Stage| {
                        let text = match stage {
                            t3::install::Stage::Checking => "Finding the newest T3 Code…",
                            t3::install::Stage::Downloading => "Downloading T3 Code…",
                            t3::install::Stage::Verifying => "Checking T3 Code's signature…",
                            t3::install::Stage::Copying => "Installing T3 Code…",
                        };
                        send(ui::UiUpdate::T3Setup(current_t3(ui::T3Install::Working(text.into()))));
                    };
                    let result = t3::install::install(&t3::install::Https, t3::install::FEED, t3::install::arch(), &user_apps, &t3::install::official_requirement(), &progress);
                    INSTALLING.store(false, Ordering::Release);
                    match result {
                        Ok(app) => {
                            eprintln!("setup: T3 Code installed at {}", app.display());
                            send(ui::UiUpdate::T3Setup(current_t3(ui::T3Install::Idle)));
                        }
                        Err(e) => {
                            eprintln!("setup: T3 Code install failed: {e}");
                            send(ui::UiUpdate::T3Setup(current_t3(ui::T3Install::Failed(e))));
                        }
                    }
                })
                .ok();
            vec![ui::UiUpdate::T3Setup(current_t3(ui::T3Install::Working("Finding the newest T3 Code…".into())))]
        }
        ui::UiAction::InstallArsenal => {
            let brews: Vec<&Path> = BREWS.iter().map(Path::new).collect();
            let Some(brew) = find_brew(&brews) else { return vec![ui::UiUpdate::ArsenalSetup(current_arsenal(ui::T3Install::Idle, &p.settings))] };
            if ARSENAL_INSTALLING.swap(true, Ordering::AcqRel) {
                return vec![];
            }
            let ui_handle = p.ui;
            let settings = p.settings.clone();
            std::thread::Builder::new()
                .name("arsenal-install".into())
                .spawn(move || {
                    let result = arsenal_install_command(&brew).output();
                    ARSENAL_INSTALLING.store(false, Ordering::Release);
                    let home = Env::current().user_home;
                    let install = match result {
                        // Installed where Heimdall does not find it with its bridge: said, not retried silently.
                        Ok(out) if out.status.success() && find_arsenal(&t3::install::app_dirs(&home)).is_none() => {
                            ui::T3Install::Failed("Arsenal installed, but its MCP bridge was not found.".into())
                        }
                        Ok(out) if out.status.success() => ui::T3Install::Idle,
                        Ok(out) => {
                            let err = String::from_utf8_lossy(&out.stderr);
                            eprintln!("setup: brew install --cask {ARSENAL_CASK} failed: {err}");
                            ui::T3Install::Failed(err.lines().rfind(|l| !l.trim().is_empty()).unwrap_or("Homebrew could not install Arsenal.").trim().to_string())
                        }
                        Err(e) => ui::T3Install::Failed(format!("Homebrew did not start: {e}")),
                    };
                    // Setup sends Use it (`SetArsenal(true)`) when it sees Arsenal here.
                    if let Some(h) = &ui_handle { h.send(ui::UiUpdate::ArsenalSetup(current_arsenal(install, &settings))) }
                })
                .ok();
            vec![ui::UiUpdate::ArsenalSetup(current_arsenal(ui::T3Install::Working("Installing Arsenal with Homebrew…".into()), &p.settings))]
        }
        ui::UiAction::InstallVella => {
            let brews: Vec<&Path> = BREWS.iter().map(Path::new).collect();
            let Some(brew) = find_brew(&brews) else { return vec![ui::UiUpdate::VellaSetup(current_vella(ui::T3Install::Idle))] };
            if VELLA_INSTALLING.swap(true, Ordering::AcqRel) {
                return vec![];
            }
            let ui_handle = p.ui;
            std::thread::Builder::new()
                .name("vella-install".into())
                .spawn(move || {
                    let result = vella_install_command(&brew).output();
                    VELLA_INSTALLING.store(false, Ordering::Release);
                    let install = match result {
                        Ok(out) if out.status.success() => ui::T3Install::Idle,
                        Ok(out) => {
                            let err = String::from_utf8_lossy(&out.stderr);
                            eprintln!("setup: brew install --cask {VELLA_CASK} failed: {err}");
                            ui::T3Install::Failed(err.lines().rfind(|l| !l.trim().is_empty()).unwrap_or("Homebrew could not install Vella.").trim().to_string())
                        }
                        Err(e) => ui::T3Install::Failed(format!("Homebrew did not start: {e}")),
                    };
                    if let Some(h) = &ui_handle { h.send(ui::UiUpdate::VellaSetup(current_vella(install))) }
                })
                .ok();
            vec![ui::UiUpdate::VellaSetup(current_vella(ui::T3Install::Working("Installing Vella with Homebrew…".into())))]
        }
        _ => vec![],
    }
}

#[cfg(test)]
#[path = "setup_wizard_tests.rs"]
mod tests;
