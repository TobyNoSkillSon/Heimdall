//! The swap, run by `heimdall-install-tool update --plan <plan.json>` after the app has quit.
//!
//! 1. Wait for the app that handed off to exit (2 minutes); if it does not, nothing is touched.
//! 2. Take the lock (`<state dir>/.lock`), repair any interrupted earlier swap ([`recover`]).
//! 3. Check the staged bundle again (folder, ID, version, executables, links, signature), refuse a downgrade, refuse
//!    a destination signed by another identity, refuse while any process still runs the destination's executable.
//! 4. Copy the staged app to `<Applications>/.heimdall-update/incoming/` (same volume as the destination) and check
//!    its signature is unchanged.
//! 5. Two renames, recorded in `.heimdall-update/journal.json`: destination to `.heimdall-update/previous/`, incoming
//!    to the destination. A failure in between moves everything back.
//! 6. Launch the new app and wait for its readiness status ([`crate::readiness`]): ready or degraded keeps it, and the
//!    previous app becomes the one rollback (`.heimdall-update/rollback/`, replacing any older rollback). No answer,
//!    an exit, or a launch model failing past the settle time: the new app is stopped, the previous one is moved
//!    back and relaunched, and `update-result.json` says why.
//!
//! Settings, the home folder, models and archives are outside the bundle and never touched.

use crate::bundle::{self, Signature};
use crate::prepare::Staged;
use crate::readiness::{self, State};
use crate::{atomic_write, Product, Result, UpdateError, Version};
use serde::{Deserialize, Serialize};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The hidden folder beside the destination: incoming copy, previous app during the swap, the one rollback.
pub const TXN: &str = ".heimdall-update";
pub const JOURNAL: &str = "journal.json";
pub const RESULT: &str = "update-result.json";
pub const LOG: &str = "update.log";
pub const LOCK: &str = ".lock";
pub const PLAN: &str = "plan.json";

/// The app's hand-off to the install tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    pub staged: Staged,
    /// The installed app to replace (the running app's bundle).
    pub destination: PathBuf,
    /// The version being replaced, for messages.
    pub from: String,
    /// The app that handed off; the install starts once it has exited.
    pub wait_for_pid: Option<i32>,
    /// `~/Library/Application Support/<bundle id>/updates`: lock, log, result and readiness files.
    pub state_dir: PathBuf,
    #[serde(default)]
    pub allow_downgrade: bool,
}

/// Paths of the transaction folder for a destination.
#[derive(Debug, Clone)]
pub struct Slots {
    pub txn: PathBuf,
    pub incoming: PathBuf,
    pub previous: PathBuf,
    pub rollback: PathBuf,
    pub journal: PathBuf,
}

impl Slots {
    pub fn for_destination(destination: &Path) -> Slots {
        let folder = destination.file_name().map(|n| n.to_owned()).unwrap_or_default();
        let txn = destination.parent().unwrap_or(Path::new("/")).join(TXN);
        Slots {
            incoming: txn.join("incoming").join(&folder),
            previous: txn.join("previous").join(&folder),
            rollback: txn.join("rollback").join(&folder),
            journal: txn.join(JOURNAL),
            txn,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Step {
    /// Between the first and second rename, or before the first.
    Swapping,
    /// Both renames done; the new app has not reported ready.
    AwaitingReady,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Journal {
    pub step: Step,
    pub from: String,
    pub to: String,
    pub destination: PathBuf,
}

/// A failed or recovered update, for the next app launch to report (it reads and deletes the file). A successful
/// update leaves none.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateResult {
    pub ok: bool,
    pub from: String,
    pub to: String,
    pub message: String,
    pub at: u64,
}

/// Reads and removes the result, if any.
pub fn take_result(state_dir: &Path) -> Option<UpdateResult> {
    let path = state_dir.join(RESULT);
    let data = std::fs::read(&path).ok()?;
    let _ = std::fs::remove_file(&path);
    serde_json::from_slice(&data).ok()
}

fn write_result(state_dir: &Path, result: &UpdateResult) {
    let _ = atomic_write(&state_dir.join(RESULT), &serde_json::to_vec(result).unwrap());
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Appends a stamped line to `<state dir>/update.log` (reset past 1 MB). Never speech or transcripts.
pub fn append_log(state_dir: &Path, line: &str) {
    use std::io::Write;
    let path = state_dir.join(LOG);
    if std::fs::metadata(&path).map(|m| m.len() > 1_000_000).unwrap_or(false) {
        let _ = std::fs::remove_file(&path);
    }
    let _ = std::fs::create_dir_all(state_dir);
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(file, "{} {line}", now_secs());
    }
}

/// An exclusive, non-blocking flock on `<dir>/.lock`, held until dropped. The swap's lock lives in the
/// `.heimdall-update` folder beside the destination, which every user of that Applications folder shares.
pub struct Lock(#[allow(dead_code)] std::fs::File);

impl Lock {
    pub fn try_take(dir: &Path) -> Result<Option<Lock>> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(LOCK);
        if std::fs::symlink_metadata(&path).map(|m| m.file_type().is_symlink()).unwrap_or(false) {
            return Err(UpdateError::new("the update lock is a link; refused"));
        }
        // Another user's lock file may be read-only to us; a read-only descriptor still takes the flock.
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .or_else(|_| std::fs::File::open(&path))?;
        // SAFETY: flock on a descriptor we own.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Ok(None);
        }
        Ok(Some(Lock(file)))
    }
}

/// Time for the readiness wait; tests drive a fake one.
pub trait Clock {
    fn now(&self) -> f64;
    fn sleep(&self, seconds: f64);
}

/// Monotonic, and it stands still while the Mac sleeps, so a closed lid never times out a healthy update.
pub struct RealClock(pub std::time::Instant);

impl Clock for RealClock {
    fn now(&self) -> f64 {
        self.0.elapsed().as_secs_f64()
    }
    fn sleep(&self, seconds: f64) {
        std::thread::sleep(std::time::Duration::from_secs_f64(seconds));
    }
}

/// Test-only fault injection; production never sets one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    None,
    /// An error before the first rename.
    ErrorBeforeSwap,
    /// An error after the first rename: the swap undoes itself.
    ErrorBetweenRenames,
    /// The process dies after the first rename: nothing runs after it (as a SIGKILL or a power cut).
    CrashBetweenRenames,
    /// The process dies after both renames, before readiness.
    CrashAfterSwap,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Ready(String),
    Degraded(String),
    Failed(String),
}

type Check<T> = Box<dyn Fn(&Path) -> Result<T>>;

pub struct Installer {
    pub plan: Plan,
    pub product: Product,
    pub verify: Check<Signature>,
    /// The bundle verifies (deep, strict) against a requirement.
    pub satisfies: Box<dyn Fn(&Path, &str) -> Result<()>>,
    /// Copies the staged app next to the destination (ditto in production).
    pub copy: Box<dyn Fn(&Path, &Path) -> Result<()>>,
    pub launch: Check<()>,
    /// Stops the app running from the destination.
    pub terminate: Box<dyn Fn(&Path)>,
    pub is_alive: Box<dyn Fn(i32) -> bool>,
    /// The pid runs the destination's executable.
    pub is_installed_app: Box<dyn Fn(i32, &Path) -> bool>,
    /// Pids running the destination's executable now.
    pub running: Box<dyn Fn(&Path) -> Vec<i32>>,
    pub clock: Box<dyn Clock>,
    pub fault: Fault,
    /// How long the relaunched app has to write its first status (HEIMDALL_UPDATE_READY_SECONDS, default 120).
    pub start_timeout: f64,
    /// How long it may stay `starting` after that.
    pub load_timeout: f64,
    /// A `failing` status this long counts as a failed update.
    pub settle: f64,
    pub poll: f64,
    pub quit_timeout: f64,
    pub log: Box<dyn Fn(&str)>,
}

impl Installer {
    pub fn new(plan: Plan, product: Product) -> Installer {
        let state_dir = plan.state_dir.clone();
        let start_timeout = std::env::var("HEIMDALL_UPDATE_READY_SECONDS").ok().and_then(|s| s.parse().ok()).unwrap_or(120.0);
        let executable = product.executable;
        Installer {
            plan,
            product,
            verify: Box::new(bundle::verify_signed),
            satisfies: Box::new(bundle::satisfies),
            copy: Box::new(|from, to| {
                let status = Command::new("/usr/bin/ditto").arg(from).arg(to).status()?;
                if status.success() {
                    Ok(())
                } else {
                    Err(UpdateError::new("could not copy the new version beside the installed one"))
                }
            }),
            launch: Box::new(|app| {
                let status = Command::new("/usr/bin/open").arg(app).status()?;
                if status.success() {
                    Ok(())
                } else {
                    Err(UpdateError::new(format!("{} did not open", app.display())))
                }
            }),
            terminate: Box::new(move |app| terminate_app(app, &product)),
            is_alive: Box::new(|pid| readiness::alive(pid)),
            is_installed_app: Box::new(move |pid, app| readiness::runs(pid, &app.join("Contents/MacOS").join(executable))),
            running: Box::new(move |app| running_pids(&app.join("Contents/MacOS").join(executable))),
            clock: Box::new(RealClock(std::time::Instant::now())),
            fault: Fault::None,
            start_timeout,
            load_timeout: 15.0 * 60.0,
            settle: 60.0,
            poll: 0.5,
            quit_timeout: 120.0,
            log: Box::new(move |line| append_log(&state_dir, line)),
        }
    }

    fn destination(&self) -> &Path {
        &self.plan.destination
    }

    /// Installs, relaunches and checks readiness; rolls back on failure and returns the reason. On any failure the
    /// previous version is installed and started again, and update-result.json explains what happened.
    pub fn run(&self) -> Result<Outcome> {
        let _ = std::fs::remove_file(self.plan.state_dir.join(RESULT));
        let result = self.run_inner();
        if !matches!(self.fault, Fault::CrashBetweenRenames | Fault::CrashAfterSwap) {
            let _ = std::fs::remove_dir_all(&self.plan.staged.directory);
        }
        result
    }

    fn run_inner(&self) -> Result<Outcome> {
        if let Some(pid) = self.plan.wait_for_pid {
            if !self.wait(self.quit_timeout, &|| !(self.is_alive)(pid)) {
                // The app still runs: nothing to relaunch. The result tells it why at its next launch.
                let message = "Heimdall did not quit; the installed app was left unchanged";
                self.fail(message);
                return Err(UpdateError::new(message));
            }
        }
        match self.after_quit() {
            Ok(outcome) => Ok(outcome),
            Err(Stop::Handled(error)) => Err(error),
            Err(Stop::Refused(_)) if self.crashed() => Err(UpdateError::new("crashed (test fault)")),
            Err(Stop::Refused(error)) => {
                // Refused before the swap committed: the previous app is in place, not running (it quit to hand
                // off). Say why and start it again.
                self.fail(&error.0);
                if self.destination().exists() {
                    let _ = (self.launch)(self.destination());
                }
                Err(error)
            }
        }
    }

    /// Everything after the app has exited. `Stop::Refused`: nothing was swapped; `Stop::Handled`: a rollback ran
    /// and already wrote the result and relaunched.
    fn after_quit(&self) -> std::result::Result<Outcome, Stop> {
        let slots = Slots::for_destination(self.destination());
        if std::fs::symlink_metadata(&slots.txn).map(|m| m.file_type().is_symlink()).unwrap_or(false) {
            return Err(Stop::Refused(UpdateError::new(format!("linked destination left unchanged: {}", slots.txn.display()))));
        }
        let Some(_lock) = Lock::try_take(&slots.txn).map_err(Stop::Refused)? else {
            return Err(Stop::Refused(UpdateError::new("another update is running; the installed app was left unchanged")));
        };
        if let Some(note) = recover_locked(self.destination(), &self.plan.state_dir, &self.product).map_err(Stop::Refused)? {
            (self.log)(&note);
        }
        let swapped = self.install().map_err(Stop::Refused)?;
        (self.log)(&format!("installed {} ({}); starting", self.destination().display(), self.plan.staged.version));
        let _ = std::fs::remove_file(readiness::path(&self.plan.state_dir));
        let outcome = match (self.launch)(self.destination()) {
            Ok(()) => self.await_ready(),
            Err(error) => Outcome::Failed(format!("it did not open ({error})")),
        };
        match &outcome {
            Outcome::Ready(line) | Outcome::Degraded(line) => {
                (self.log)(&format!("{}: {line}", if matches!(outcome, Outcome::Ready(_)) { "ready" } else { "running, degraded" }));
                self.commit(swapped).map_err(Stop::Handled)?;
                Ok(outcome)
            }
            Outcome::Failed(reason) => Err(Stop::Handled(self.roll_back(swapped, reason))),
        }
    }

    fn crashed(&self) -> bool {
        matches!(self.fault, Fault::CrashBetweenRenames | Fault::CrashAfterSwap)
    }

    /// Steps 3 to 5. Returns whether a previous app was moved aside.
    fn install(&self) -> Result<bool> {
        let destination = self.destination();
        let slots = Slots::for_destination(destination);
        let staged = &self.plan.staged.app;
        if destination.file_name().and_then(|n| n.to_str()) != Some(self.product.app_folder) {
            return Err(UpdateError::new(format!("choose a destination named {}", self.product.app_folder)));
        }
        for path in [destination, destination.parent().unwrap_or(Path::new("/")), &slots.txn] {
            if std::fs::symlink_metadata(path).map(|m| m.file_type().is_symlink()).unwrap_or(false) {
                return Err(UpdateError::new(format!("linked destination left unchanged: {}", path.display())));
            }
        }
        let version = Version::parse(&self.plan.staged.version)
            .ok_or_else(|| UpdateError::new("the staged version is not a version"))?;
        if staged.file_name().and_then(|n| n.to_str()) != Some(self.product.app_folder) {
            return Err(UpdateError::new("the staged app has the wrong name"));
        }
        bundle::check_bundle(staged, &version, &self.product)?;
        bundle::reject_external_links(staged)?;
        let replacement = (self.verify)(staged)?;
        if destination.exists() {
            self.check_version(destination, staged)?;
            let current = bundle::info(destination)?;
            if current.bundle_id != self.product.bundle_id {
                return Err(UpdateError::new("the destination is not an installed Heimdall Alpha"));
            }
            let signature = (self.verify)(destination)
                .map_err(|_| UpdateError::new("the installed app's signature does not verify; left unchanged"))?;
            let other = || UpdateError::new("the new version is signed by another identity; left unchanged");
            if signature != replacement {
                return Err(other());
            }
            // The requirement text alone is chosen by whoever signed; the bundle must also satisfy it.
            (self.satisfies)(staged, &self.requirement(&signature)).map_err(|_| other())?;
            if !(self.running)(destination).is_empty() {
                return Err(UpdateError::new("Heimdall is running again; the installed app was left unchanged"));
            }
        }
        std::fs::create_dir_all(slots.incoming.parent().unwrap())?;
        std::fs::create_dir_all(slots.previous.parent().unwrap())?;
        let _ = std::fs::remove_dir_all(&slots.incoming);
        let prepared = (|| {
            (self.copy)(staged, &slots.incoming)?;
            let changed = || UpdateError::new("the copied bundle's signature changed");
            if (self.verify)(&slots.incoming)? != replacement {
                return Err(changed());
            }
            (self.satisfies)(&slots.incoming, &self.requirement(&replacement)).map_err(|_| changed())?;
            if self.fault == Fault::ErrorBeforeSwap {
                return Err(UpdateError::new("injected failure before the swap"));
            }
            Ok(())
        })();
        if let Err(error) = prepared {
            let _ = std::fs::remove_dir_all(&slots.incoming);
            return Err(error);
        }
        let journal = |step| Journal {
            step,
            from: self.plan.from.clone(),
            to: self.plan.staged.version.clone(),
            destination: destination.to_path_buf(),
        };
        atomic_write(&slots.journal, &serde_json::to_vec(&journal(Step::Swapping)).unwrap())?;
        let had_previous = destination.exists();
        let mut moved_old = false;
        let mut moved_new = false;
        let swap = (|| {
            if had_previous {
                std::fs::rename(destination, &slots.previous)?;
                moved_old = true;
            }
            match self.fault {
                Fault::CrashBetweenRenames => return Err(Crashed),
                Fault::ErrorBetweenRenames => return Err(UpdateError::new("injected failure between the renames")),
                _ => {}
            }
            std::fs::rename(&slots.incoming, destination)?;
            moved_new = true;
            atomic_write(&slots.journal, &serde_json::to_vec(&journal(Step::AwaitingReady)).unwrap())?;
            if self.fault == Fault::CrashAfterSwap {
                return Err(Crashed);
            }
            Ok(())
        })();
        if let Err(error) = swap {
            if self.crashed() {
                return Err(error);
            }
            if moved_new {
                let _ = std::fs::rename(destination, &slots.incoming);
            }
            if moved_old {
                if std::fs::rename(&slots.previous, destination).is_err() {
                    return Err(UpdateError::new(format!(
                        "restoring the previous app failed; it is at {}",
                        slots.previous.display()
                    )));
                }
            }
            let _ = std::fs::remove_dir_all(&slots.incoming);
            let _ = std::fs::remove_file(&slots.journal);
            return Err(error);
        }
        Ok(had_previous)
    }

    /// The requirement a bundle with this signature must satisfy: the designated requirement, or for ad-hoc
    /// (development and tests) the product's identifier.
    fn requirement(&self, signature: &Signature) -> String {
        match signature {
            Signature::Designated(text) => text.clone(),
            Signature::Adhoc => format!("identifier \"{}\"", self.product.bundle_id),
        }
    }

    /// Refuses an older version, or an older build of the same version, unless the plan allows a downgrade. Signing
    /// checks run either way.
    fn check_version(&self, installed: &Path, staged: &Path) -> Result<()> {
        let unreadable = || UpdateError::new("cannot read the installed version; left unchanged");
        let current = bundle::info(installed).map_err(|_| unreadable())?;
        let replacement = bundle::info(staged)?;
        let a = Version::parse(&current.version).ok_or_else(unreadable)?;
        let b = Version::parse(&replacement.version).ok_or_else(unreadable)?;
        let build = |s: &str| s.parse::<u64>().ok();
        let older_build = a == b && matches!((build(&current.build), build(&replacement.build)), (Some(x), Some(y)) if y < x);
        if (b < a || older_build) && !self.plan.allow_downgrade {
            return Err(UpdateError::new(format!(
                "refusing to replace Heimdall Alpha {} (build {}) with older {} (build {}); left unchanged",
                current.version, current.build, replacement.version, replacement.build
            )));
        }
        Ok(())
    }

    /// Polls the readiness status until the new app is ready, degraded or has failed.
    pub fn await_ready(&self) -> Outcome {
        let status_file = readiness::path(&self.plan.state_dir);
        let destination = self.destination().to_path_buf();
        let version = self.plan.staged.version.clone();
        let start = self.clock.now();
        let mut answered_at: Option<f64> = None;
        let mut failing_since: Option<f64> = None;
        loop {
            let data = std::fs::read(&status_file).ok();
            let answering = readiness::read_pid(data.as_deref())
                .map(|pid| (self.is_installed_app)(pid, &destination))
                .unwrap_or(false);
            let now = self.clock.now();
            if !answering {
                if answered_at.is_some() {
                    return Outcome::Failed("the new version exited".into());
                }
                if now - start > self.start_timeout {
                    return Outcome::Failed(format!("no answer after {} s", self.start_timeout as u64));
                }
            } else {
                let answered = *answered_at.get_or_insert(now);
                match readiness::evaluate(data.as_deref(), &version, &|pid| (self.is_installed_app)(pid, &destination)) {
                    State::Ready(line) => return Outcome::Ready(line),
                    State::Degraded(line) => return Outcome::Degraded(line),
                    State::Failing(reason) => {
                        let since = *failing_since.get_or_insert(now);
                        if now - since >= self.settle {
                            return Outcome::Failed(reason);
                        }
                    }
                    State::Waiting(reason) => {
                        failing_since = None;
                        if now - answered > self.load_timeout {
                            return Outcome::Failed(format!("still {reason} after {} min", (self.load_timeout / 60.0) as u64));
                        }
                    }
                }
            }
            self.clock.sleep(self.poll);
        }
    }

    /// The new version runs: the previous app becomes the one rollback, replacing any older one.
    fn commit(&self, had_previous: bool) -> Result<()> {
        let slots = Slots::for_destination(self.destination());
        if had_previous && slots.previous.exists() {
            promote_previous(&slots)?;
        }
        let _ = std::fs::remove_file(&slots.journal);
        clean_scratch(&slots);
        (self.log)(&format!("updated {} -> {}", self.plan.from, self.plan.staged.version));
        Ok(())
    }

    fn roll_back(&self, had_previous: bool, reason: &str) -> UpdateError {
        let destination = self.destination();
        let slots = Slots::for_destination(destination);
        let failure = format!("Heimdall Alpha {} did not become ready: {reason}", self.plan.staged.version);
        (self.log)(&format!("{failure}; restoring {}", self.plan.from));
        (self.terminate)(destination);
        if !had_previous || !slots.previous.exists() {
            let message = format!("{failure}; there was no previous app to restore");
            self.fail(&message);
            let _ = std::fs::remove_file(&slots.journal);
            return UpdateError::new(message);
        }
        let failed = slots.txn.join(format!("failed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&failed);
        let restored = std::fs::create_dir_all(&failed)
            .and_then(|_| std::fs::rename(destination, failed.join(self.product.app_folder)))
            .and_then(|_| {
                std::fs::rename(&slots.previous, destination).inspect_err(|_| {
                    let _ = std::fs::rename(failed.join(self.product.app_folder), destination);
                })
            });
        if restored.is_err() {
            let message = format!("{failure}. Restoring {} failed; the previous app is at {}", self.plan.from, slots.previous.display());
            self.fail(&message);
            let _ = (self.launch)(destination);
            return UpdateError::new(message);
        }
        let _ = std::fs::remove_dir_all(&failed);
        let _ = std::fs::remove_file(&slots.journal);
        let message = format!("{failure}. Heimdall Alpha {} was restored", self.plan.from);
        self.fail(&message); // written first: the restored app reports it at launch
        let _ = (self.launch)(destination);
        UpdateError::new(message)
    }

    fn fail(&self, message: &str) {
        (self.log)(&format!("failed: {message}"));
        write_result(
            &self.plan.state_dir,
            &UpdateResult {
                ok: false,
                from: self.plan.from.clone(),
                to: self.plan.staged.version.clone(),
                message: message.to_owned(),
                at: now_secs(),
            },
        );
    }

    fn wait(&self, timeout: f64, done: &dyn Fn() -> bool) -> bool {
        let deadline = self.clock.now() + timeout;
        while self.clock.now() < deadline {
            if done() {
                return true;
            }
            self.clock.sleep(self.poll.min(0.2));
        }
        done()
    }
}

/// Why the install stopped after the app exited.
enum Stop {
    Refused(UpdateError),
    Handled(UpdateError),
}

/// The marker error of an injected crash: the caller stops without any cleanup.
#[allow(non_upper_case_globals)]
const Crashed: UpdateError = UpdateError(String::new());

/// Moves `previous` into the rollback slot, replacing the older rollback only once the move is certain.
fn promote_previous(slots: &Slots) -> Result<()> {
    std::fs::create_dir_all(slots.rollback.parent().unwrap())?;
    let retired = slots.txn.join(format!("retired-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&retired);
    if slots.rollback.exists() {
        std::fs::rename(&slots.rollback, &retired)?;
    }
    if let Err(error) = std::fs::rename(&slots.previous, &slots.rollback) {
        let _ = std::fs::rename(&retired, &slots.rollback);
        return Err(error.into());
    }
    let _ = std::fs::remove_dir_all(&retired);
    let _ = std::fs::remove_dir(slots.previous.parent().unwrap());
    Ok(())
}

/// Repairs an interrupted swap. Called by the app at every start (before anything else) and by the install tool
/// before each update. Does nothing while an install tool holds the lock (a normal update in progress).
///
/// - The destination is missing and the previous app is in `.heimdall-update/previous/`: the crash fell between the
///   two renames. The previous app is moved back (rolled back).
/// - Both renames were done (the journal says so, or the destination holds the new version): the new bundle was
///   fully checked before the swap, so the update is rolled forward and the previous app becomes the rollback.
/// - Neither rename happened: the leftover incoming copy is removed.
///
/// Returns a note when something was repaired; it is also written as the update result for the app to report.
pub fn recover(destination: &Path, state_dir: &Path, product: &Product) -> Result<Option<String>> {
    let slots = Slots::for_destination(destination);
    if !slots.txn.is_dir() {
        return Ok(None); // never updated: nothing to repair, and nothing is created beside the app
    }
    let Some(_lock) = Lock::try_take(&slots.txn)? else { return Ok(None) };
    recover_locked(destination, state_dir, product)
}

fn recover_locked(destination: &Path, state_dir: &Path, product: &Product) -> Result<Option<String>> {
    let slots = Slots::for_destination(destination);
    let journal: Option<Journal> = std::fs::read(&slots.journal).ok().and_then(|d| serde_json::from_slice(&d).ok());
    let leftovers = slots.previous.exists() || slots.incoming.exists() || journal.is_some();
    if !leftovers {
        clean_scratch(&slots);
        return Ok(None);
    }
    let (from, to) = journal.as_ref().map(|j| (j.from.clone(), j.to.clone())).unwrap_or_default();
    let note = if !destination.exists() && slots.previous.exists() {
        std::fs::rename(&slots.previous, destination)?;
        Some(format!("An update to {to} was interrupted; Heimdall Alpha {from} was restored"))
    } else if destination.exists() && slots.previous.exists() {
        let installed = bundle::info(destination).ok().map(|i| i.version);
        let swapped = journal.as_ref().is_some_and(|j| j.step == Step::AwaitingReady)
            || (installed.is_some() && installed.as_deref() == Some(to.as_str()));
        if swapped && bundle::info(destination).is_ok_and(|i| i.bundle_id == product.bundle_id) {
            promote_previous(&slots)?;
            Some(format!("An update to {to} was interrupted after the swap; {to} was kept and {from} is the rollback"))
        } else {
            // Something else was installed at the destination after the first rename (a reinstall): keep it, and
            // keep the copy moved aside as the rollback.
            promote_previous(&slots)?;
            Some("A copy left by an interrupted update was kept as the rollback".to_owned())
        }
    } else if !destination.exists() && !slots.previous.exists() && slots.incoming.exists() && journal.is_some() {
        // Neither the old app nor a previous copy: the incoming copy is the only app left. Install it.
        std::fs::rename(&slots.incoming, destination)?;
        Some(format!("An interrupted update to {to} was completed"))
    } else {
        None
    };
    let _ = std::fs::remove_dir_all(&slots.incoming);
    let _ = std::fs::remove_file(&slots.journal);
    clean_scratch(&slots);
    if let Some(message) = &note {
        append_log(state_dir, message);
        write_result(state_dir, &UpdateResult { ok: true, from, to, message: message.clone(), at: now_secs() });
    }
    Ok(note)
}

/// Removes failed and retired copies and empty working folders; never the rollback.
fn clean_scratch(slots: &Slots) {
    if let Ok(entries) = std::fs::read_dir(&slots.txn) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("failed-") || name.starts_with("retired-") {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }
    for dir in [slots.incoming.parent(), slots.previous.parent()].into_iter().flatten() {
        let _ = std::fs::remove_dir(dir);
    }
}

/// Puts the one rollback back in place and keeps the replaced version as the rollback (an exchange). Refuses while
/// the app runs or when there is no rollback.
pub fn roll_back_installed(destination: &Path, state_dir: &Path, product: &Product) -> Result<String> {
    let slots = Slots::for_destination(destination);
    if !slots.txn.is_dir() {
        return Err(UpdateError::new("there is no previous version to go back to"));
    }
    let Some(_lock) = Lock::try_take(&slots.txn)? else { return Err(UpdateError::new("an update is running")) };
    // A tool that died while waiting for readiness left the previous app in `previous/`: make it the rollback first.
    recover_locked(destination, state_dir, product)?;
    if !slots.rollback.exists() {
        return Err(UpdateError::new("there is no previous version to go back to"));
    }
    if !running_pids(&destination.join("Contents/MacOS").join(product.executable)).is_empty() {
        return Err(UpdateError::new("quit Heimdall Alpha first"));
    }
    let back = bundle::info(&slots.rollback)?.version;
    // Not `retired-`: cleanup never deletes a parked app, so a failed last rename loses no version.
    let parking = slots.txn.join(format!("parked-{}", std::process::id()));
    std::fs::create_dir_all(&parking)?;
    let parked = parking.join(product.app_folder);
    std::fs::rename(destination, &parked)?;
    if let Err(error) = std::fs::rename(&slots.rollback, destination) {
        let _ = std::fs::rename(&parked, destination);
        return Err(error.into());
    }
    std::fs::rename(&parked, &slots.rollback).map_err(|e| {
        UpdateError::new(format!("rolled back, but the replaced version stays at {}: {e}", parked.display()))
    })?;
    let _ = std::fs::remove_dir(&parking);
    append_log(state_dir, &format!("rolled back to {back}"));
    Ok(back)
}

/// Pids whose executable is the same file as `executable`.
pub fn running_pids(executable: &Path) -> Vec<i32> {
    extern "C" {
        fn proc_listallpids(buffer: *mut libc::c_void, size: libc::c_int) -> libc::c_int;
    }
    let mut pids = vec![0i32; 16_384];
    // SAFETY: the buffer is writable for the byte size passed.
    let n = unsafe { proc_listallpids(pids.as_mut_ptr().cast(), (pids.len() * 4) as libc::c_int) };
    if n <= 0 {
        return vec![];
    }
    pids.truncate(n as usize);
    pids.into_iter().filter(|&pid| pid > 0 && readiness::runs(pid, executable)).collect()
}

/// Stops every process running one of the bundle's executables (the app and its workers): SIGTERM, then SIGKILL
/// after 15 s. Used only on a new version that failed to become ready.
pub fn terminate_app(app: &Path, product: &Product) {
    let executables: Vec<PathBuf> = product.required.iter().map(|f| app.join("Contents").join(f)).collect();
    let pids: Vec<i32> = executables.iter().flat_map(|e| running_pids(e)).collect();
    for &pid in &pids {
        // SAFETY: signalling a pid we just matched by executable identity.
        unsafe { libc::kill(pid, libc::SIGTERM) };
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while std::time::Instant::now() < deadline && pids.iter().any(|&p| unsafe { libc::kill(p, 0) } == 0) {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    for &pid in &pids {
        if executables.iter().any(|e| readiness::runs(pid, e)) {
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }
}

/// What the app reports as busy (a recording or speech in progress, a model loading, a setup step). The app
/// implements it; the updater asks it when the user confirms and again right before the hand-off.
pub trait BusyGate {
    /// Why an update cannot start now ("recording", "loading a model"), or None when idle.
    fn busy(&self) -> Option<String>;
}

/// [`hand_off`] unless the app is busy, checked with no wait in between.
pub fn hand_off_when_idle(plan: &Plan, running_app: &Path, gate: &dyn BusyGate) -> Result<()> {
    if let Some(reason) = gate.busy() {
        return Err(UpdateError::new(format!("Heimdall is {reason}, so the update was not installed. Try again when it has finished")));
    }
    hand_off(plan, running_app)
}

/// The app's side of the hand-off: writes the plan into the staging folder and starts the running bundle's
/// `Contents/Helpers/heimdall-install-tool update --plan <plan>` in its own session. The caller then quits the app;
/// the tool waits for it to exit.
pub fn hand_off(plan: &Plan, running_app: &Path) -> Result<()> {
    let tool = running_app.join("Contents/Helpers/heimdall-install-tool");
    let executable = std::fs::metadata(&tool).map(|m| m.is_file()).unwrap_or(false);
    if !executable {
        return Err(UpdateError::new("the installer tool is missing from the app; reinstall Heimdall Alpha"));
    }
    let plan_path = plan.staged.directory.join(PLAN);
    atomic_write(&plan_path, &serde_json::to_vec_pretty(plan).unwrap())?;
    spawn_detached(&tool, &["update".as_ref(), "--plan".as_ref(), plan_path.as_os_str()])
}

fn spawn_detached(tool: &Path, args: &[&std::ffi::OsStr]) -> Result<()> {
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;
    let mut command = Command::new(tool);
    command.args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    // SAFETY: setsid is async-signal-safe; the child leaves the app's session so the app's quit does not end it.
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    command.spawn().map_err(|e| UpdateError::new(format!("could not start the installer: {e}")))?;
    Ok(())
}
