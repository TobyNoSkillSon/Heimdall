//! The top of the recovery ladder (docs/resilience.md): a watchdog thread that relaunches Heimdall when its live loop
//! or its UI thread has really frozen, and makes Quit always end.
//!
//! Frozen, not quiet ("make sure that it actually froze, that it wasn't just everything was quiet").
//! The live loop handles a 2-second settings tick whatever else happens, idle or busy, Listening on or off; the
//! UI thread runs a block the watchdog posts every 10 seconds. Each records a heartbeat. A heartbeat older than a
//! minute means that thread is stuck inside one piece of work: silence, Listening off or an idle agent still beat.
//! Time is uptime (`Instant`), which stops while the Mac sleeps, and a wake re-baselines the beats, so a night
//! asleep is never a freeze.
//!
//! Relaunch keeps everything: an independent intent writer is flushed before exit and restored before delivery
//! initializes; settings saves follow the latch, and the archive's committed records survive the exit. At most two automatic relaunches in 30 minutes,
//! counted across launches in `recovery.json`; past that Heimdall shows a red pill instead of looping.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// A heartbeat older than this is a freeze.
pub const STALL: Duration = Duration::from_secs(60);
/// The live loop's first heartbeat may take this long (models load before the loop starts).
pub const STARTUP_STALL: Duration = Duration::from_secs(180);
/// Quit ends once its teardown makes no progress for this long ("I clicked Quit Heimdall. Nothing
/// happened": the old flat 45 s let a stuck teardown look like a dead menu). Quit preempts archive compaction at the
/// click, so a healthy step ends well inside it; each finished teardown step (`quit_progress`) restarts the count.
pub const QUIT_LIMIT: Duration = Duration::from_secs(8);
/// And Quit always ends this long after the click, progress or not.
pub const QUIT_CAP: Duration = Duration::from_secs(30);
/// How often the watchdog looks (and pings the UI thread).
pub const CHECK_EVERY: Duration = Duration::from_secs(10);
/// While quitting, how often the Quit limit is checked: to the quarter second, not to the next round.
const QUIT_POLL: Duration = Duration::from_millis(250);
/// Automatic relaunches allowed per window.
pub const RELAUNCHES: usize = 2;
pub const RELAUNCH_WINDOW_SECONDS: u64 = 30 * 60;

/// The thresholds (the constants above; tests shorten them).
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub stall: Duration,
    pub startup: Duration,
    pub quit: Duration,
    pub every: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Limits { stall: STALL, startup: STARTUP_STALL, quit: QUIT_LIMIT, every: CHECK_EVERY }
    }
}

/// Heartbeats in milliseconds of uptime since `epoch`; 0 means "not yet".
pub struct Beats {
    limits: Limits,
    epoch: Instant,
    live: AtomicU64,
    ui: AtomicU64,
    quit: AtomicU64,
    /// The last teardown step that finished after Quit (0: none yet; the click counts).
    quit_progress: AtomicU64,
    /// The live loop has beaten at least once since it started.
    live_started: AtomicBool,
    /// The live loop runs (set when it starts; cleared while it shuts down on purpose).
    armed: AtomicBool,
}

impl Default for Beats {
    fn default() -> Self {
        Beats {
            limits: Limits::default(),
            epoch: Instant::now(),
            live: AtomicU64::new(0),
            ui: AtomicU64::new(0),
            quit: AtomicU64::new(0),
            quit_progress: AtomicU64::new(0),
            live_started: AtomicBool::new(false),
            armed: AtomicBool::new(false),
        }
    }
}

/// What froze.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stall {
    /// The live loop (capture events, the pipeline, every menu action) stopped turning.
    Live,
    /// The UI thread (menu, island, windows) stopped turning.
    Ui,
    /// Quit did not finish in time.
    Quit,
}

impl Stall {
    pub fn reason(self) -> &'static str {
        match self {
            Stall::Live => "the live loop stopped responding",
            Stall::Ui => "the menu stopped responding",
            Stall::Quit => "quit did not finish",
        }
    }
}

impl Beats {
    pub fn with_limits(limits: Limits) -> Beats {
        Beats { limits, ..Beats::default() }
    }
    fn now_ms(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64 + 1
    }
    /// The live loop starts (or restarts its count): the startup allowance applies until its first tick.
    pub fn arm_live(&self) {
        self.live.store(self.now_ms(), Ordering::Release);
        self.live_started.store(false, Ordering::Release);
        self.armed.store(true, Ordering::Release);
    }
    /// The live loop handled its settings tick.
    pub fn live_beat(&self) {
        self.live.store(self.now_ms(), Ordering::Release);
        self.live_started.store(true, Ordering::Release);
    }
    pub fn ui_beat(&self) {
        self.ui.store(self.now_ms(), Ordering::Release);
    }
    /// Quit began: the loop's shutdown is no longer watched as a freeze, only against `QUIT_LIMIT`.
    pub fn quitting(&self) {
        self.armed.store(false, Ordering::Release);
        let _ = self.quit.compare_exchange(0, self.now_ms(), Ordering::AcqRel, Ordering::Acquire);
    }
    /// A teardown step finished: the Quit limit counts from now (never past `QUIT_CAP` from the click).
    pub fn quit_progress(&self) {
        if self.is_quitting() {
            self.quit_progress.store(self.now_ms(), Ordering::Release);
        }
    }
    /// Quit (or a deliberate shutdown) has begun: an error while shutting down never relaunches Heimdall.
    pub fn is_quitting(&self) -> bool {
        self.quit.load(Ordering::Acquire) != 0
    }
    /// How long the running live loop has gone without its settings tick (hang.rs). None before its first tick and
    /// while it shuts down.
    pub fn live_silence(&self) -> Option<Duration> {
        if !self.armed.load(Ordering::Acquire) || !self.live_started.load(Ordering::Acquire) || self.is_quitting() {
            return None;
        }
        let beat = self.live.load(Ordering::Acquire);
        (beat != 0).then(|| Duration::from_millis(self.now_ms().saturating_sub(beat)))
    }
    /// After a sleep: every heartbeat counts from now.
    fn rebaseline(&self) {
        let now = self.now_ms();
        for beat in [&self.live, &self.ui] {
            if beat.load(Ordering::Acquire) != 0 {
                beat.store(now, Ordering::Release);
            }
        }
    }

    /// The stall at `now_ms`, if any. Pure over the stored beats, for tests.
    pub fn check_at(&self, now_ms: u64) -> Option<Stall> {
        let age = |beat: &AtomicU64| {
            let b = beat.load(Ordering::Acquire);
            (b != 0).then(|| Duration::from_millis(now_ms.saturating_sub(b)))
        };
        if let Some(quit) = age(&self.quit) {
            let quiet = age(&self.quit_progress).map_or(quit, |p| p.min(quit));
            return (quiet > self.limits.quit || quit > QUIT_CAP).then_some(Stall::Quit);
        }
        if self.armed.load(Ordering::Acquire) {
            let limit = if self.live_started.load(Ordering::Acquire) { self.limits.stall } else { self.limits.startup };
            if age(&self.live).is_some_and(|a| a > limit) {
                return Some(Stall::Live);
            }
        }
        if age(&self.ui).is_some_and(|a| a > self.limits.stall) {
            return Some(Stall::Ui);
        }
        None
    }

    #[cfg(test)]
    fn set_for_test(&self, live: u64, ui: u64, started: bool, armed: bool) {
        self.live.store(live, Ordering::Release);
        self.ui.store(ui, Ordering::Release);
        self.live_started.store(started, Ordering::Release);
        self.armed.store(armed, Ordering::Release);
    }
}

static QUIT_WATCH: std::sync::Mutex<Option<std::sync::Weak<Beats>>> = std::sync::Mutex::new(None);
/// The live app's beats, for teardown code that has no handle to them (`progress`).
pub fn watch_quit_progress(beats: &Arc<Beats>) {
    *QUIT_WATCH.lock().unwrap() = Some(Arc::downgrade(beats));
}
/// A teardown step moved on (a bounded wait still waiting counts: its own deadline ends it, `QUIT_CAP` ends all).
/// A no-op outside Quit and outside the live app.
pub fn progress() {
    if let Some(beats) = QUIT_WATCH.lock().unwrap().as_ref().and_then(std::sync::Weak::upgrade) {
        beats.quit_progress();
    }
}

/// The relaunch history, in `<data>/recovery.json`. Survives the relaunch it records.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct RecoveryLog {
    /// Unix seconds and reason of each automatic relaunch.
    #[serde(default)]
    pub relaunches: Vec<(u64, String)>,
    /// Set by a relaunch, taken by the next launch: it says "Heimdall OK" once it runs again.
    #[serde(default)]
    pub pending_ok: Option<(u64, String)>,
    /// The full last loop/teardown error, including Quit failures without a relaunch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<(u64, String)>,
}

pub fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

static RECOVERY_WRITE: std::sync::Mutex<()> = std::sync::Mutex::new(());
#[cfg(test)]
thread_local! { static FAIL_RECOVERY_WRITE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }

impl RecoveryLog {
    pub fn path(data: &Path) -> PathBuf {
        data.join("recovery.json")
    }
    pub fn load(data: &Path) -> Result<RecoveryLog> {
        match std::fs::read(Self::path(data)) {
            Ok(bytes) => serde_json::from_slice(&bytes).context("read relaunch history"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound
                && std::fs::symlink_metadata(Self::path(data)).is_err() => Ok(Self::default()),
            Err(e) => Err(e).context("read relaunch history"),
        }
    }
    fn save(&self, data: &Path) -> Result<()> {
        #[cfg(test)]
        anyhow::ensure!(!FAIL_RECOVERY_WRITE.get(), "injected recovery-history I/O failure");
        heimdall_archive::atomic_write(&Self::path(data), &serde_json::to_vec_pretty(self)?)
    }
    /// Whether an automatic relaunch is allowed at `now` (unix seconds).
    pub fn allows(&self, now: u64) -> bool {
        self.relaunches.iter().filter(|(t, _)| now.saturating_sub(*t) < RELAUNCH_WINDOW_SECONDS).count() < RELAUNCHES
    }
    /// Records a relaunch (automatic ones count toward the bound) and asks the next launch to confirm it.
    pub fn record(data: &Path, reason: &str, automatic: bool) -> Result<()> {
        let _write = RECOVERY_WRITE.lock().unwrap();
        let mut log = Self::load(data)?;
        let now = unix_now();
        anyhow::ensure!(!automatic || log.allows(now), "automatic relaunch budget exhausted");
        log.relaunches.retain(|(t, _)| now.saturating_sub(*t) < RELAUNCH_WINDOW_SECONDS);
        if automatic {
            log.relaunches.push((now, reason.into()));
        }
        log.pending_ok = Some((now, reason.into()));
        log.save(data)
    }
    pub fn record_error(data: &Path, error: &str) -> Result<()> {
        let _write = RECOVERY_WRITE.lock().unwrap();
        let mut log = Self::load(data)?;
        log.last_error = Some((unix_now(), error.into()));
        log.save(data)
    }
    /// At launch: the reason of a relaunch in the last five minutes, once.
    pub fn take_pending(data: &Path) -> Option<String> {
        let _write = RECOVERY_WRITE.lock().unwrap();
        let mut log = Self::load(data).ok()?;
        let (at, reason) = log.pending_ok.take()?;
        log.save(data).ok()?;
        (unix_now().saturating_sub(at) < 300).then_some(reason)
    }
}

/// How to start Heimdall again: through LaunchServices for the app bundle (so macOS attributes its permissions to
/// Heimdall itself, exactly as when the user opens it), else the same binary and arguments.
pub fn launch_command() -> Result<Vec<String>> {
    let exe = std::env::current_exe().context("current executable")?;
    let bundle = exe.ancestors().nth(3).filter(|p| p.extension().is_some_and(|e| e == "app"));
    Ok(match bundle {
        Some(app) => vec!["/usr/bin/open".into(), app.to_string_lossy().into_owned()],
        None => std::iter::once(exe.to_string_lossy().into_owned()).chain(std::env::args().skip(1)).collect(),
    })
}

/// Starts a detached helper that waits for process `pid` to exit, then runs `launch`. The helper outlives this
/// process (its own process group, no inherited terminal or pipes).
pub fn spawn_relaunch(pid: u32, launch: &[String]) -> Result<()> {
    spawn_relaunch_child(pid, launch).map(|_| ())
}

fn spawn_relaunch_child(pid: u32, launch: &[String]) -> Result<std::process::Child> {
    agent::spawn_relaunch(pid, launch).context("start the relaunch helper")
}

/// What the watchdog does when it finds a stall. The live implementation shows the island, relaunches or exits;
/// tests record.
pub trait OnStall: Send + 'static {
    fn stalled(&mut self, stall: Stall);
    /// The stalled thread moved again (only possible when no relaunch happened).
    fn resumed(&mut self, stall: Stall);
}

/// Runs the watchdog until the process exits. Each round pings the UI thread through `ping` and checks the beats.
pub fn spawn(beats: Arc<Beats>, ping: impl Fn(Arc<Beats>) + Send + 'static, mut on_stall: impl OnStall) -> Result<()> {
    std::thread::Builder::new().name("heimdall-watchdog".into()).spawn(move || {
        let mut last = (Instant::now(), SystemTime::now());
        let mut stalled: Option<Stall> = None;
        // The UI's first beat comes from the first ping.
        ping(beats.clone());
        let mut round = Instant::now();
        loop {
            std::thread::sleep(beats.limits.every.min(QUIT_POLL));
            if round.elapsed() < beats.limits.every {
                if beats.is_quitting() {
                    report_transition(beats.check_at(beats.now_ms()), &mut stalled, &mut on_stall);
                }
                continue;
            }
            round = Instant::now();
            let now = (Instant::now(), SystemTime::now());
            // The wall clock ran well past uptime: the Mac slept. Nothing could beat while asleep.
            let wall = now.1.duration_since(last.1).unwrap_or_default();
            if wall > now.0.duration_since(last.0) + Duration::from_secs(5) {
                beats.rebaseline();
            }
            last = now;
            report_transition(beats.check_at(beats.now_ms()), &mut stalled, &mut on_stall);
            ping(beats.clone());
        }
    })?;
    Ok(())
}

fn report_transition(now: Option<Stall>, stalled: &mut Option<Stall>, handler: &mut impl OnStall) {
    match (now, *stalled) {
        (Some(stall), None) => { *stalled=Some(stall); handler.stalled(stall); }
        (None, Some(stall)) => { *stalled=None; handler.resumed(stall); }
        _ => {}
    }
}

/// The live stall handling: orange "Restarting Heimdall…", then relaunch; past the bound a red pill that
/// relaunches on click. Quit past its limit just ends the process.
pub struct Relauncher {
    pub ui: ui::Ui,
    pub data: PathBuf,
    pub tools: Arc<crate::bridge::Tools>,
    /// Shown red (the bound was used up); cleared if the thread recovers on its own.
    pub showing_problem: bool,
}

impl OnStall for Relauncher {
    fn stalled(&mut self, stall: Stall) {
        eprintln!("Watchdog: {}", stall.reason());
        if stall == Stall::Quit {
            record_forced_quit(&self.data);
            self.tools.record_diagnostic("health", serde_json::json!({"component":"watchdog", "state":"stalled", "reason":stall.reason()}));
            force_exit(0);
        }
        self.tools.record_diagnostic("health", serde_json::json!({"component":"watchdog", "state":"stalled", "reason":stall.reason()}));
        if RecoveryLog::load(&self.data).is_ok_and(|log| log.allows(unix_now())) {
            self.ui.send(ui::UiUpdate::Health(ui::HealthCue::Restarting {
                key: "heimdall".into(),
                part: ui::Part::Heimdall,
                label: "Restarting Heimdall…".into(),
            }));
            match relaunch_now(&self.data, stall.reason(), true, &self.tools) {
                Ok(()) => return,
                Err(e) => {
                    self.tools.record_diagnostic("recovery", serde_json::json!({"state":"blocked", "reason":format!("{e:#}"), "trigger":stall.reason()}));
                    eprintln!("Relaunch blocked; staying up: {e:#}");
                },
            }
        }
        self.showing_problem = true;
        self.ui.send(ui::UiUpdate::Health(ui::HealthCue::Problem {
            key: "heimdall".into(),
            part: ui::Part::Heimdall,
            label: "Heimdall keeps freezing".into(),
            action: "Click to restart it".into(),
            control: ui::HealthControl::Relaunch,
        }));
    }
    fn resumed(&mut self, stall: Stall) {
        if std::mem::take(&mut self.showing_problem) {
            self.tools.record_diagnostic("health", serde_json::json!({"component":"watchdog", "state":"recovered", "reason":stall.reason()}));
            self.ui.send(ui::UiUpdate::Health(ui::HealthCue::Recovered {
                key: "heimdall".into(),
                part: ui::Part::Heimdall,
                label: "Heimdall OK".into(),
            }));
        }
    }
}

/// Ends the process at once, without exit handlers. `exit` runs C++ static destructors, and ggml's Metal teardown
/// aborts while another thread still holds a model (24 Oct 02:31:37: the forced Quit ended in SIGABRT and a crash
/// report instead of an exit). Committed archive data is on disk; the log thread gets a moment for the last lines.
pub fn force_exit(code: i32) -> ! {
    std::thread::sleep(Duration::from_millis(150));
    unsafe { libc::_exit(code) }
}

/// Persist the forced-Quit reason independently of SQLite before exiting. A stuck
/// recovery writer or filesystem must not turn the watchdog's exit into another stall.
fn record_forced_quit(data: &Path) {
    let data = data.to_path_buf();
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let spawned = std::thread::Builder::new().name("heimdall-quit-recovery".into()).spawn(move || {
        let _ = tx.send(RecoveryLog::record_error(&data, Stall::Quit.reason()));
    });
    match spawned {
        Ok(_) => match rx.recv_timeout(Duration::from_millis(250)) {
            Ok(Ok(())) => {},
            Ok(Err(e)) => eprintln!("forced-Quit recovery log not saved: {e:#}"),
            Err(e) => eprintln!("forced-Quit recovery log pending: {e}; exiting"),
        },
        Err(e) => eprintln!("forced-Quit recovery writer failed: {e}"),
    }
}

/// Relaunches from a frozen state: records it, starts the helper, gives the island a moment to show the orange
/// pill, and exits. Committed archive records are on disk; only an uncommitted tail of the current second is lost.
pub fn relaunch_now(data: &Path, reason: &str, automatic: bool, tools: &crate::bridge::Tools) -> Result<()> {
    // Wait before creating a helper: a persistence failure must leave the process alive with the red pill.
    tools.record_diagnostic("recovery", serde_json::json!({"state":"relaunch-requested", "reason":reason, "automatic":automatic}));
    tools.flush_intent()?;
    RecoveryLog::record(data, reason, automatic)?;
    std::thread::sleep(Duration::from_millis(1500));
    // On closes after the orange pill. Off stays responsive throughout flush
    // and helper creation, and its durable acknowledgement precedes exit.
    let mut helper = None;
    let result = tools.with_final_intent(|| {
        helper = Some(spawn_relaunch_child(std::process::id(), &launch_command()?)?);
        Ok(())
    }, || force_exit(75));
    // A late stop-write failure leaves us alive; retire the waiting helper so
    // a later ordinary Quit cannot unexpectedly relaunch this failed attempt.
    if let Some(mut helper) = helper {
        let _ = helper.kill();
        let _ = helper.wait();
    }
    result
}

fn record_then_launch(data: &Path, reason: &str, automatic: bool, launch: impl FnOnce() -> Result<()>) -> Result<()> {
    RecoveryLog::record(data, reason, automatic)?;
    launch()
}

pub fn start_relaunch(data: &Path, reason: &str, automatic: bool, tools: &crate::bridge::Tools) -> Result<()> {
    tools.record_diagnostic("recovery", serde_json::json!({"state":"relaunch-requested", "reason":reason, "automatic":automatic}));
    tools.flush_intent()?;
    record_then_launch(data, reason, automatic, || spawn_relaunch(std::process::id(), &launch_command()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forced_quit_with_locked_archive_persists_reason_before_process_exit() {
        const CHILD_DATA: &str = "HEIMDALL_TEST_FORCED_QUIT_DATA";
        if let Some(data) = std::env::var_os(CHILD_DATA) {
            let data = PathBuf::from(data);
            let session = data.join("archive/session-forced-quit");
            let journal = heimdall_archive::Journal::open(&session).unwrap();
            let locked = heimdall_archive::store::open(journal.root()).unwrap();
            locked.execute_batch("BEGIN IMMEDIATE").unwrap();
            let tools = Arc::new(crate::bridge::Tools::new(&data.join("no-models"), &data));
            *tools.archive_session.lock().unwrap() = Some(session);
            let mut watchdog = Relauncher { ui: ui::Ui::handle(), data, tools, showing_problem: false };
            watchdog.stalled(Stall::Quit); // Production exit, with the archive lock still held.
            panic!("forced Quit returned without exiting");
        }
        let dir = tempfile::tempdir().unwrap();
        RecoveryLog::record(dir.path(), "previous manual restart", false).unwrap();
        let before = RecoveryLog::load(dir.path()).unwrap();
        let started = Instant::now();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "watchdog::tests::forced_quit_with_locked_archive_persists_reason_before_process_exit", "--nocapture"])
            .env(CHILD_DATA, dir.path()).spawn().unwrap();
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() { break status; }
            if started.elapsed() > Duration::from_secs(5) {
                child.kill().unwrap(); child.wait().unwrap();
                panic!("forced Quit did not exit within its bounded diagnostic wait");
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        assert!(status.success());
        eprintln!("forced Quit with archive locked: exited in {:?}", started.elapsed());
        let after = RecoveryLog::load(dir.path()).unwrap();
        assert_eq!(after.last_error.unwrap().1, Stall::Quit.reason());
        assert_eq!(after.relaunches, before.relaunches);
        assert_eq!(after.pending_ok, before.pending_ok);
    }

    #[test]
    fn forced_quit_recovery_attempt_is_bounded_if_another_recovery_write_is_stuck() {
        let dir = tempfile::tempdir().unwrap();
        let write = RECOVERY_WRITE.lock().unwrap();
        let started = Instant::now();
        record_forced_quit(dir.path());
        assert!(started.elapsed() < Duration::from_secs(1));
        drop(write);
        // Drain the detached write before deleting its isolated directory.
        let deadline = Instant::now() + Duration::from_secs(2);
        while RecoveryLog::load(dir.path()).unwrap().last_error.is_none() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn quit_error_is_durable_without_consuming_a_relaunch() {
        let t = tempfile::tempdir().unwrap();
        std::fs::write(RecoveryLog::path(t.path()), br#"{"relaunches":[],"pending_ok":null}"#).unwrap();
        let error = "source teardown errors: process:call: speaker actor queue unavailable: disconnected";
        RecoveryLog::record_error(t.path(), error).unwrap();
        let log = RecoveryLog::load(t.path()).unwrap();
        assert_eq!(log.last_error.unwrap().1, error);
        assert!(log.relaunches.is_empty());
        assert!(log.pending_ok.is_none());
    }

    #[test]
    fn a_frozen_loop_is_a_stall_but_quiet_and_startup_are_not() {
        let b = Beats::default();
        // Not armed (offline modes, or before the loop): never a live stall.
        b.set_for_test(1, 0, true, false);
        assert_eq!(b.check_at(10 * 60_000), None);
        // Armed, beating every 2 s whatever happens: never a stall, however long nothing else happens.
        for minute in 0..120u64 {
            let now = minute * 60_000 + 1;
            b.set_for_test(now.saturating_sub(2_000).max(1), now.saturating_sub(5_000).max(1), true, true);
            assert_eq!(b.check_at(now), None, "minute {minute}");
        }
        // The loop stuck inside one handler (13 Oct 11:16: stop_live_worker waited forever): a stall after 60 s.
        b.set_for_test(1_000, 70_000, true, true);
        assert_eq!(b.check_at(60_000), None);
        assert_eq!(b.check_at(61_002), Some(Stall::Live));
        // Before its first tick the loop may take its startup allowance.
        b.set_for_test(1_000, 100_000, false, true);
        assert_eq!(b.check_at(120_000), None);
        assert_eq!(b.check_at(181_002), Some(Stall::Live));
        // The UI thread stuck.
        b.set_for_test(100_000, 30_000, true, true);
        assert_eq!(b.check_at(100_500), Some(Stall::Ui));
    }

    #[test]
    fn quit_is_bounded_and_a_sleep_is_not_a_freeze() {
        let b = Beats::default();
        b.arm_live();
        b.live_beat();
        b.ui_beat();
        assert!(!b.is_quitting());
        b.quitting();
        assert!(b.is_quitting(), "a teardown error after Quit must not relaunch");
        let quit = b.quit.load(Ordering::Acquire);
        let limit = QUIT_LIMIT.as_millis() as u64;
        assert_eq!(b.check_at(quit + limit - 1_000), None, "shutdown is not a freeze");
        assert_eq!(b.check_at(quit + limit + 1_000), Some(Stall::Quit));
        // A teardown that keeps finishing steps gets more time, never past the cap.
        b.quit_progress.store(quit + limit, Ordering::Release);
        assert_eq!(b.check_at(quit + limit + 1_000), None, "a step just finished");
        assert_eq!(b.check_at(quit + 2 * limit + 1_000), Some(Stall::Quit), "no step for the limit");
        b.quit_progress.store(quit + QUIT_CAP.as_millis() as u64, Ordering::Release);
        assert_eq!(b.check_at(quit + QUIT_CAP.as_millis() as u64 + 1), Some(Stall::Quit), "the cap holds");
        let b = Beats::default();
        b.set_for_test(1_000, 1_000, true, true);
        b.rebaseline();
        assert_eq!(b.check_at(b.now_ms() + 1_000), None);
    }

    #[test]
    fn unreadable_recovery_history_cannot_reset_the_relaunch_budget() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(RecoveryLog::path(dir.path()), b"{truncated").unwrap();
        assert!(RecoveryLog::load(dir.path()).is_err());
        let mut launched = false;
        assert!(record_then_launch(dir.path(), "freeze", true, || { launched = true; Ok(()) }).is_err());
        assert!(!launched);
        let mut watchdog = Relauncher { ui: ui::Ui::handle(), data: dir.path().into(), tools: Arc::new(crate::bridge::Tools::new(dir.path(), dir.path())), showing_problem: false };
        watchdog.stalled(Stall::Live);
        assert!(watchdog.showing_problem, "unreadable budget shows the red problem pill");
        assert_eq!(std::fs::read(RecoveryLog::path(dir.path())).unwrap(), b"{truncated");
    }

    #[test]
    fn failed_recovery_write_never_launches_and_preserves_the_previous_history() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path();
        RecoveryLog::record(data, "previous", false).unwrap();
        let before = std::fs::read(RecoveryLog::path(data)).unwrap();
        FAIL_RECOVERY_WRITE.set(true);
        let mut launched = false;
        let result = record_then_launch(data, "freeze", true, || { launched = true; Ok(()) });
        let mut watchdog = Relauncher { ui: ui::Ui::handle(), data: data.into(), tools: Arc::new(crate::bridge::Tools::new(data, data)), showing_problem: false };
        watchdog.stalled(Stall::Live);
        FAIL_RECOVERY_WRITE.set(false);
        assert!(watchdog.showing_problem, "failed count persistence shows the red problem pill");
        assert!(result.is_err() && !launched, "a failed durable write cannot launch the helper");
        assert_eq!(std::fs::read(RecoveryLog::path(data)).unwrap(), before);
        assert_eq!(RecoveryLog::load(data).unwrap().relaunches.len(), 0);
    }

    #[test]
    fn failed_stop_intent_flush_blocks_relaunch_and_shows_the_red_problem() {
        use agent::delivery::StopReason;
        let dir = tempfile::tempdir().unwrap();
        let tools = Arc::new(crate::bridge::Tools::new(dir.path(), dir.path()));
        tools.initialize_delivery(&crate::settings::Settings { dry_run: false, ..Default::default() });
        std::fs::create_dir(dir.path().join("stop-intent.json")).unwrap();
        tools.menu_intent(StopReason::Agent, false);
        let mut watchdog = Relauncher { ui: ui::Ui::handle(), data: dir.path().into(), tools, showing_problem: false };
        watchdog.stalled(Stall::Live);
        assert!(watchdog.showing_problem);
        assert!(!RecoveryLog::path(dir.path()).exists(), "no relaunch may be recorded or launched before stop intent is durable");
        assert!(watchdog.tools.agent_enabled.permit().is_err());
    }

    #[test]
    fn simultaneous_relaunch_requests_cannot_overrun_or_lose_the_budget() {
        let dir = tempfile::tempdir().unwrap();
        let launched = std::sync::atomic::AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..4 {
                scope.spawn(|| {
                    let _ = record_then_launch(dir.path(), "freeze", true, || {
                        launched.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    });
                });
            }
        });
        assert_eq!(launched.load(Ordering::SeqCst), RELAUNCHES);
        assert_eq!(RecoveryLog::load(dir.path()).unwrap().relaunches.len(), RELAUNCHES);
    }
    #[test]
    fn relaunches_are_bounded_across_launches_and_the_next_launch_confirms_once() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path();
        assert!(RecoveryLog::load(data).unwrap().allows(unix_now()));
        RecoveryLog::record(data, "the live loop stopped responding", true).unwrap();
        assert_eq!(RecoveryLog::take_pending(data).as_deref(), Some("the live loop stopped responding"));
        assert_eq!(RecoveryLog::take_pending(data), None, "said once");
        RecoveryLog::record(data, "again", true).unwrap();
        assert!(!RecoveryLog::load(data).unwrap().allows(unix_now()), "two in 30 minutes: no third automatic relaunch");
        // The user's own click relaunches whatever the count, and does not use it up.
        RecoveryLog::record(data, "clicked", false).unwrap();
        assert_eq!(RecoveryLog::load(data).unwrap().relaunches.len(), 2);
        let old = RecoveryLog { relaunches: vec![(unix_now() - RELAUNCH_WINDOW_SECONDS - 1, "old".into()); 5], pending_ok: None, last_error: None };
        assert!(old.allows(unix_now()));
    }

    #[test]
    fn the_helper_waits_for_the_old_process_then_launches() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("launched");
        let mut old = std::process::Command::new("/bin/sleep").arg("0.6").spawn().unwrap();
        spawn_relaunch(old.id(), &["/usr/bin/touch".into(), marker.to_string_lossy().into_owned()]).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        assert!(!marker.exists(), "launched while the old process still ran");
        old.wait().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !marker.exists() {
            assert!(Instant::now() < deadline, "the helper never launched");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    #[test]
    fn controlled_watchdog_reports_stall_once_and_resume_at_exact_threshold() {
        struct Record(std::sync::mpsc::Sender<(bool, Stall)>);
        impl OnStall for Record {
            fn stalled(&mut self, s: Stall) { self.0.send((true,s)).unwrap(); }
            fn resumed(&mut self, s: Stall) { self.0.send((false,s)).unwrap(); }
        }
        let limits=Limits { stall:Duration::from_millis(300),startup:Duration::from_millis(300),quit:Duration::from_secs(60),every:Duration::from_millis(20) };
        let beats=Beats::with_limits(limits);let (tx,rx)=std::sync::mpsc::channel();
        let mut state=None;let mut record=Record(tx);
        beats.set_for_test(100,100,true,true);
        for now in [100,200,400] { report_transition(beats.check_at(now),&mut state,&mut record); }
        assert!(rx.try_recv().is_err());
        report_transition(beats.check_at(401),&mut state,&mut record);
        assert_eq!(rx.try_recv().unwrap(),(true,Stall::Live));
        report_transition(beats.check_at(800),&mut state,&mut record);
        assert!(rx.try_recv().is_err());
        beats.set_for_test(800,800,true,true);
        report_transition(beats.check_at(801),&mut state,&mut record);
        assert_eq!(rx.try_recv().unwrap(),(false,Stall::Live));
    }
    #[test]
    fn watchdog_real_thread_smoke_uses_handshakes_without_heartbeat_races() {
        struct Record(std::sync::mpsc::Sender<(bool, Stall)>);
        impl OnStall for Record {
            fn stalled(&mut self, s:Stall) { let _=self.0.send((true,s)); }
            fn resumed(&mut self, s:Stall) { let _=self.0.send((false,s)); }
        }
        let beats=Arc::new(Beats { epoch:Instant::now()-Duration::from_secs(61), limits:Limits{stall:Duration::from_secs(60),startup:Duration::from_secs(60),quit:Duration::from_secs(60),every:Duration::from_millis(20)}, ..Beats::default() });
        let (tx,rx)=std::sync::mpsc::channel();let (ping,pings)=std::sync::mpsc::channel();
        spawn(beats.clone(),move |b| { b.ui_beat(); let _=ping.send(()); },Record(tx)).unwrap();
        pings.recv_timeout(Duration::from_secs(3)).unwrap();
        // Inject an already stale live timestamp; the real thread owns checking
        // and callbacks. The ping keeps UI fresh without a competing live beat.
        beats.live.store(1,Ordering::Release);
        beats.live_started.store(true,Ordering::Release);beats.armed.store(true,Ordering::Release);
        assert_eq!(rx.recv_timeout(Duration::from_secs(3)).unwrap(),(true,Stall::Live));
        beats.live_beat();
        assert_eq!(rx.recv_timeout(Duration::from_secs(3)).unwrap(),(false,Stall::Live));
        beats.quitting();
    }

    #[test]
    fn an_app_bundle_relaunches_through_launch_services() {
        let launch = launch_command().unwrap();
        // Tests run from target/, not a bundle: the same binary again.
        assert!(launch[0].contains("heimdall") || launch[0].contains("deps"), "{launch:?}");
        assert_ne!(launch[0], "/usr/bin/open");
    }
}
