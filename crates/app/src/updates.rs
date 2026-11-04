//! The public edition's in-app updates (docs/updates.md), without UI: the menu and alerts live in crates/ui.
//!
//! Policy: on for the public edition only (`edition::PUBLIC`) and only when running from an app bundle;
//! `HEIMDALL_UPDATE=0` turns it off. The personal edition never checks: the user's app installs through the night pass.
//! - At launch, before anything else touches the bundle: repair an interrupted swap and pick up the last update's
//!   result for an alert.
//! - Check at launch when due, then every 24 hours (an hourly wake-up, so a Mac that slept checks soon after waking).
//!   One request to the GitHub releases list; nothing else is sent. A failed check retries in about an hour.
//! - Never while busy: the periodic check waits, and Update is refused while busy. A verified download waits up to
//!   15 minutes for idle.
//! - Download only after the user says Update. Then the install tool takes over and the app quits.
//!
//! State lives in `~/Library/Application Support/<bundle id>/updates/`: it does not move with the home folder, so the
//! install tool and the relaunched app agree on it even when a home move is applied at that launch.

use crate::edition;
use heimdall_update::fetch::{Fetch, HttpFetch, MAX_LIST};
use heimdall_update::install::{self, BusyGate, Plan};
use heimdall_update::prepare::Prepare;
use heimdall_update::readiness::{self, Phase as ReadyPhase};
use heimdall_update::state::{check_due, Event, Machine, Phase, DAY};
use heimdall_update::{bundle::IdentityCheck, release, signing, source::Source, Product, Release, Version};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// What the menu sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateCommand {
    /// "Check for Updates…": check now and report the answer, even "up to date".
    CheckForUpdates,
    /// "Update to X…": the UI shows the offer's confirmation alert ([`Updates::offer_text`]).
    ShowOffer,
    /// The alert's Update button.
    Install,
}

/// What the UI shows: an alert, or a menu refresh.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    /// The menu row changed (call [`Updates::menu_row`] again).
    Changed,
    /// Confirmation alert: title, message; buttons Update (send `Install`) and Later.
    Offer { title: String, message: String },
    UpToDate(String),
    /// An update did not start or failed; nothing changed.
    Failed(String),
    /// The last update was rolled back or recovered (read once at launch).
    Result(String),
}

/// The app's side: busy state, alerts and quitting.
pub trait Host: BusyGate + Send + Sync {
    /// Called from any thread; the UI hops to the main thread.
    fn notify(&self, notice: Notice);
    /// Quit through the app's normal Quit path (the install tool waits for this process to exit).
    fn quit(&self);
}

/// The menu row: title, whether it is enabled, and the command it sends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuRow {
    pub title: String,
    pub enabled: bool,
    pub command: UpdateCommand,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Saved {
    last_check: Option<u64>,
    /// The offer found earlier, kept across launches until this version catches up.
    offer: Option<serde_json::Value>,
}

const SAVED: &str = "check.json";
const IDLE_WAIT: Duration = Duration::from_secs(15 * 60);

pub fn enabled() -> bool {
    // Alpha: updates are off until the release feed exists; `HEIMDALL_UPDATE=1` turns them on.
    enabled_for(edition::PUBLIC, std::env::var("HEIMDALL_UPDATE").as_deref() != Ok("1"), || running_app().is_some())
}

/// Updates run only in the public edition, not turned off, from an app bundle.
pub fn enabled_for(public: bool, turned_off: bool, in_bundle: impl FnOnce() -> bool) -> bool {
    public && !turned_off && in_bundle()
}

/// The running `.app` bundle, when this executable sits in `<App>.app/Contents/MacOS/`.
pub fn running_app() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?.canonicalize().ok()?;
    let app = exe.parent()?.parent()?.parent()?;
    (exe.parent()?.ends_with("Contents/MacOS") && app.extension().is_some_and(|e| e == "app")).then(|| app.to_path_buf())
}

pub fn state_dir() -> PathBuf {
    heimdall_home::Env::current().pointer_dir().join("updates")
}

/// Records this launch's readiness for the install tool (`starting` early, `ready` once the start finished,
/// `degraded` or `failing` for a launch model refused or failing). A no-op when updates are off.
pub fn report(phase: ReadyPhase, detail: &str) {
    if enabled() {
        let _ = readiness::report(&state_dir(), edition::VERSION, phase, detail);
    }
}

pub struct Updates {
    state: Arc<Mutex<State>>,
    host: Arc<dyn Host>,
}

struct State {
    machine: Machine,
    current: Version,
    checking: bool,
    dir: PathBuf,
    app: PathBuf,
}

impl Updates {
    /// Starts updates at launch; None when they are off. Call first thing, before the bundle's resources are read.
    pub fn start(host: Arc<dyn Host>) -> Option<Arc<Updates>> {
        if !enabled() {
            return None;
        }
        let app = running_app()?;
        let updates = Arc::new(Updates::new(host, app, state_dir(), Version::parse(edition::VERSION)?));
        updates.launch();
        let weak = Arc::downgrade(&updates);
        std::thread::Builder::new()
            .name("updates".into())
            .spawn(move || loop {
                std::thread::sleep(Duration::from_secs(3600));
                let Some(updates) = weak.upgrade() else { return };
                updates.check_if_due();
            })
            .ok()?;
        Some(updates)
    }

    fn new(host: Arc<dyn Host>, app: PathBuf, dir: PathBuf, current: Version) -> Updates {
        let state = State { machine: Machine::default(), current, checking: false, dir, app };
        Updates { state: Arc::new(Mutex::new(state)), host }
    }

    fn launch(&self) {
        let (dir, app) = {
            let s = self.state.lock().unwrap();
            (s.dir.clone(), s.app.clone())
        };
        let _ = install::recover(&app, &dir, &Product::ALPHA);
        if let Some(result) = install::take_result(&dir) {
            self.host.notify(Notice::Result(result.message));
        }
        self.restore_offer();
        self.check_if_due();
    }

    fn saved(dir: &Path) -> Saved {
        std::fs::read(dir.join(SAVED)).ok().and_then(|d| serde_json::from_slice(&d).ok()).unwrap_or_default()
    }

    fn save(dir: &Path, saved: &Saved) {
        let _ = std::fs::create_dir_all(dir);
        let _ = std::fs::write(dir.join(SAVED), serde_json::to_vec(saved).unwrap_or_default());
    }

    fn restore_offer(&self) {
        let mut s = self.state.lock().unwrap();
        let mut saved = Self::saved(&s.dir);
        let offer = saved.offer.as_ref().and_then(|v| Release::from_json(v).ok());
        match offer {
            Some(release) if release.version > s.current => {
                s.machine.handle(Event::Checked(Some(release)));
            }
            Some(_) => {
                saved.offer = None;
                Self::save(&s.dir, &saved);
            }
            None => {}
        }
    }

    /// The periodic check: due, not busy, not already checking or updating.
    pub fn check_if_due(&self) {
        let due = {
            let s = self.state.lock().unwrap();
            check_due(Self::saved(&s.dir).last_check, now(), DAY)
        };
        if due && self.host.busy().is_none() {
            self.check(false);
        }
    }

    fn check(&self, user: bool) {
        let state = self.state.clone();
        let host = self.host.clone();
        {
            let mut s = state.lock().unwrap();
            if s.checking || s.machine.phase.busy() {
                return;
            }
            s.checking = true;
        }
        std::thread::spawn(move || {
            let (dir, current) = {
                let s = state.lock().unwrap();
                (s.dir.clone(), s.current.clone())
            };
            // Recorded before the request, so failures and relaunches never retry more than hourly.
            let mut saved = Self::saved(&dir);
            saved.last_check = Some(now());
            Self::save(&dir, &saved);
            let result = check_once(&current);
            let mut s = state.lock().unwrap();
            s.checking = false;
            match result {
                Ok(offer) => {
                    saved.offer = offer.as_ref().map(release_json);
                    Self::save(&dir, &saved);
                    s.machine.handle(Event::Checked(offer.clone()));
                    drop(s);
                    host.notify(Notice::Changed);
                    if user && offer.is_none() {
                        host.notify(Notice::UpToDate(format!("Heimdall Alpha {current} is the newest version.")));
                    }
                }
                Err(error) => {
                    saved.last_check = Some(now().saturating_sub(23 * 3600));
                    Self::save(&dir, &saved);
                    s.machine.handle(Event::CheckFailed(error.0.clone()));
                    drop(s);
                    if user {
                        host.notify(Notice::Failed(format!("Could not check for updates: {error}.")));
                    }
                }
            }
        });
    }

    pub fn menu_row(&self) -> MenuRow {
        let s = self.state.lock().unwrap();
        menu_row(&s.machine.phase)
    }

    /// The confirmation alert for the current offer.
    pub fn offer_text(&self) -> Option<(String, String)> {
        let s = self.state.lock().unwrap();
        let Phase::Available(release) = &s.machine.phase else { return None };
        Some(offer_text(release))
    }

    pub fn send(&self, command: UpdateCommand) {
        match command {
            UpdateCommand::CheckForUpdates => self.check(true),
            UpdateCommand::ShowOffer => {
                if let Some((title, message)) = self.offer_text() {
                    self.host.notify(Notice::Offer { title, message });
                }
            }
            UpdateCommand::Install => self.install(),
        }
    }

    fn install(&self) {
        // Asked before taking the state lock: the host may answer from the main thread.
        let busy = self.host.busy();
        let release = {
            let mut s = self.state.lock().unwrap();
            let Phase::Available(release) = s.machine.phase.clone() else { return };
            if let Some(reason) = busy {
                let message = format!("Heimdall is {reason}. Try again when it has finished.");
                s.machine.handle(Event::Failed(message.clone()));
                drop(s);
                self.host.notify(Notice::Failed(message));
                return;
            }
            s.machine.handle(Event::Confirmed);
            release
        };
        self.host.notify(Notice::Changed);
        let state = self.state.clone();
        let host = self.host.clone();
        std::thread::spawn(move || {
            let (dir, app, from) = {
                let s = state.lock().unwrap();
                (s.dir.clone(), s.app.clone(), s.current.to_string())
            };
            let result = download_and_hand_off(&release, &dir, &app, &from, &*host, &state);
            if let Err(error) = result {
                state.lock().unwrap().machine.handle(Event::Failed(error.0.clone()));
                host.notify(Notice::Changed);
                host.notify(Notice::Failed(format!("{error}. Nothing changed.")));
            }
        });
    }
}

fn download_and_hand_off(
    release: &Release,
    dir: &Path,
    app: &Path,
    from: &str,
    host: &dyn Host,
    state: &Mutex<State>,
) -> heimdall_update::Result<()> {
    let fetch = HttpFetch::new()?;
    let prepare = Prepare {
        product: Product::ALPHA,
        source: Source::from_env()?,
        fetch: &fetch,
        keys: signing::TRUSTED_KEYS,
        identity: IdentityCheck::new(&Product::ALPHA),
        running_app: app.to_path_buf(),
        work_root: std::env::temp_dir(),
    };
    let staged = prepare.run(release, &mut |line| install::append_log(dir, line))?;
    let cleanup = |e: heimdall_update::UpdateError| {
        let _ = std::fs::remove_dir_all(&staged.directory);
        e
    };
    let busy = host.busy().is_some();
    state.lock().unwrap().machine.handle(Event::Verified { busy });
    host.notify(Notice::Changed);
    let deadline = std::time::Instant::now() + IDLE_WAIT;
    while host.busy().is_some() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_secs(1));
    }
    state.lock().unwrap().machine.handle(Event::BecameIdle);
    let plan = Plan {
        staged: staged.clone(),
        destination: app.to_path_buf(),
        from: from.to_owned(),
        wait_for_pid: Some(std::process::id() as i32),
        state_dir: dir.to_path_buf(),
        allow_downgrade: false,
    };
    // The busy check and the hand-off follow each other with no wait in between.
    install::hand_off_when_idle(&plan, app, host).map_err(cleanup)?;
    host.quit();
    // Still here well after the install tool stopped waiting (2 minutes): the quit was cancelled. The tool left
    // everything unchanged; offer the update again.
    std::thread::sleep(Duration::from_secs(150));
    let _ = std::fs::remove_dir_all(&staged.directory);
    Err(heimdall_update::UpdateError::new("Heimdall did not quit, so the update was not installed"))
}

fn check_once(current: &Version) -> heimdall_update::Result<Option<Release>> {
    let source = Source::from_env()?;
    let list = HttpFetch::new()?.get(&source.list_url, MAX_LIST)?;
    Ok(release::choose(&Release::parse_list(&list)?, current, &Product::ALPHA))
}

fn release_json(release: &Release) -> serde_json::Value {
    serde_json::json!({
        "tag_name": release.tag, "name": release.name, "body": release.body, "draft": release.draft,
        "prerelease": release.prerelease, "published_at": if release.published { "saved" } else { "" },
        "assets": release.assets.iter().map(|a| serde_json::json!({"name": a})).collect::<Vec<_>>(),
    })
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// The menu row for a phase: "Check for Updates…" when nothing is offered.
pub fn menu_row(phase: &Phase) -> MenuRow {
    match phase {
        Phase::Idle => MenuRow { title: "Check for Updates…".into(), enabled: true, command: UpdateCommand::CheckForUpdates },
        Phase::Available(r) => MenuRow { title: format!("Update to {}…", r.version), enabled: true, command: UpdateCommand::ShowOffer },
        Phase::Downloading(r) => MenuRow { title: format!("Downloading {}…", r.version), enabled: false, command: UpdateCommand::ShowOffer },
        Phase::WaitingForIdle(r) => MenuRow { title: format!("Update to {} when idle…", r.version), enabled: false, command: UpdateCommand::ShowOffer },
        Phase::Installing(r) => MenuRow { title: format!("Installing {}…", r.version), enabled: false, command: UpdateCommand::ShowOffer },
    }
}

pub fn offer_text(release: &Release) -> (String, String) {
    let notes = release.short_notes(360);
    let mut message = "Heimdall restarts; your settings, memories and home folder stay.".to_owned();
    if !notes.is_empty() {
        message += "\n\n";
        message += &notes;
    }
    (format!("Update to Heimdall Alpha {}?", release.version), message)
}

// ---- The live app's side: the host, the menu's clicks and the skill upgrade.

static RUNNING: std::sync::OnceLock<Arc<Updates>> = std::sync::OnceLock::new();

/// Why an update must wait, from what the app says it is doing; None when idle.
pub fn busy_reason(agent_working: bool, speaking: bool, model_loading: bool) -> Option<String> {
    if agent_working {
        Some("working on a task".into())
    } else if speaking {
        Some("speaking".into())
    } else if model_loading {
        Some("loading a model".into())
    } else {
        None
    }
}

/// The menu's row as the UI draws it.
pub fn ui_row(row: &MenuRow) -> ui::UpdateRow {
    ui::UpdateRow { title: row.title.clone(), enabled: row.enabled }
}

/// A notice as the UI shows it: the menu row again, or a native alert.
pub fn ui_update(notice: Notice, row: impl FnOnce() -> Option<MenuRow>) -> Option<ui::UiUpdate> {
    let alert = |title: &str, message: String, offer: bool| Some(ui::UiUpdate::UpdateAlert(ui::UpdateAlert { title: title.into(), message, offer }));
    match notice {
        Notice::Changed => row().map(|r| ui::UiUpdate::UpdateRow(Some(ui_row(&r)))),
        Notice::Offer { title, message } => alert(&title, message, true),
        Notice::UpToDate(message) => alert("You\u{2019}re up to date", message, false),
        Notice::Failed(message) | Notice::Result(message) => alert("Software Update", message, false),
    }
}

/// The live app's host: busy from the agent, Heimdall's speech and the built-in engine; notices go to the UI; quit
/// is the menu's Quit.
pub struct AppHost {
    pub ui: ui::Ui,
    pub tools: std::sync::Weak<crate::bridge::Tools>,
}

impl BusyGate for AppHost {
    fn busy(&self) -> Option<String> {
        let tools = self.tools.upgrade()?;
        busy_reason(tools.agent_in_run(), tools.heimdall_speaking(), tools.engine_loading.load(std::sync::atomic::Ordering::Acquire))
    }
}

impl Host for AppHost {
    fn notify(&self, notice: Notice) {
        if let Some(update) = ui_update(notice, || RUNNING.get().map(|u| u.menu_row())) {
            self.ui.send(update);
        }
    }
    fn quit(&self) {
        self.ui.send(ui::UiUpdate::Quit);
    }
}

/// At launch (public edition, from the app bundle): starts updates, shows the menu row, and has each agent start
/// upgrade the bundled skills. Nothing in the personal edition.
pub fn launch(ui: ui::Ui, tools: &Arc<crate::bridge::Tools>) {
    let host = Arc::new(AppHost { ui, tools: Arc::downgrade(tools) });
    let Some(updates) = Updates::start(host) else { return };
    ui.send(ui::UiUpdate::UpdateRow(Some(ui_row(&updates.menu_row()))));
    let _ = RUNNING.set(updates);
    agent::skills::after_seed(upgrade_skills);
}

/// The menu row clicked: its current command.
pub fn row_clicked() {
    if let Some(u) = RUNNING.get() {
        u.send(u.menu_row().command);
    }
}

/// The offer's Update.
pub fn install_clicked() {
    if let Some(u) = RUNNING.get() {
        u.send(UpdateCommand::Install);
    }
}

/// After the agent's skill seeding: unedited bundled skills follow this release; edited ones are kept.
fn upgrade_skills(sets: &[PathBuf], profile: &Path) {
    match heimdall_update::skills::upgrade(sets, profile, &state_dir(), edition::VERSION) {
        Ok(r) if !r.updated.is_empty() || !r.conflicts.is_empty() => {
            eprintln!("Bundled skills: updated {:?}; kept edited {:?}", r.updated, r.conflicts)
        }
        Ok(_) => {}
        Err(e) => eprintln!("Bundled skills not upgraded: {e}"),
    }
}

#[cfg(test)]
#[path = "updates_tests.rs"]
mod tests;
