//! The bundled Floating Islands helper's supervisor (night 1022). Heimdall launches
//! `Contents/Helpers/FloatingIslands.app` with `--max-hours 0` (no lifetime limit) and `--parent-pid <Heimdall>`
//! (the helper quits when Heimdall exits, however it ends: no orphan after a crash or a kill), first asking a standalone
//! FloatingIslands found through `.app.pid` to quit (SIGTERM, as `islands quit` does); relaunches it when it exits,
//! with backoff and a cap on quick failures (each logged); and quits it on Heimdall's orderly quit.

use super::files::Root;
use anyhow::{Context, Result};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// Waits before relaunch number 1, 2, …; past the last one the helper is given up on until Heimdall restarts.
pub const BACKOFF: [Duration; 5] = [Duration::from_secs(1), Duration::from_secs(5), Duration::from_secs(30), Duration::from_secs(120), Duration::from_secs(600)];
/// A run at least this long was healthy: the next exit starts the backoff again.
pub const HEALTHY_RUN: Duration = Duration::from_secs(60);
/// How long a standalone helper gets to quit after SIGTERM (the CLI's `islands quit` waits the same 5 s).
pub const TAKEOVER_WAIT: Duration = Duration::from_secs(5);

/// The process operations, real or fake.
pub trait Launcher: Send + Sync + 'static {
    /// Start the helper; returns its pid.
    fn spawn(&self) -> Result<i32>;
    /// Block until `pid` (spawned by us) exits; returns a description of how it ended.
    fn wait(&self, pid: i32) -> String;
    /// Whether `pid` is a running FloatingIslands process.
    fn is_helper(&self, pid: i32) -> bool;
    fn terminate(&self, pid: i32);
    fn sleep(&self, d: Duration) { std::thread::sleep(d) }
}

pub struct Supervisor<L: Launcher> {
    launcher: L,
    root: Root,
    child: AtomicI32,
    quitting: AtomicBool,
    /// Wakes a backoff wait early on quit.
    wake: (Mutex<()>, Condvar),
    backoff: Vec<Duration>,
    healthy: Duration,
}

impl<L: Launcher> Supervisor<L> {
    pub fn new(launcher: L, root: Root) -> Arc<Self> {
        Arc::new(Supervisor { launcher, root, child: AtomicI32::new(0), quitting: AtomicBool::new(false), wake: (Mutex::new(()), Condvar::new()), backoff: BACKOFF.to_vec(), healthy: HEALTHY_RUN })
    }
    #[cfg(test)]
    fn with_timing(launcher: L, root: Root, backoff: Vec<Duration>, healthy: Duration) -> Arc<Self> {
        Arc::new(Supervisor { launcher, root, child: AtomicI32::new(0), quitting: AtomicBool::new(false), wake: (Mutex::new(()), Condvar::new()), backoff, healthy })
    }

    /// A standalone helper on this folder (`.app.pid` alive, a FloatingIslands, not ours) is asked to quit.
    /// Returns whether one was found.
    pub fn take_over(&self) -> bool {
        let Some(pid) = std::fs::read_to_string(self.root.pid()).ok().and_then(|s| s.trim().parse::<i32>().ok()) else { return false };
        if pid <= 1 || pid == self.child.load(Ordering::Acquire) || pid == std::process::id() as i32 || !self.launcher.is_helper(pid) { return false }
        eprintln!("islands: taking over the standalone FloatingIslands (pid {pid}): asking it to quit");
        self.launcher.terminate(pid);
        let end = Instant::now() + TAKEOVER_WAIT;
        while self.launcher.is_helper(pid) && Instant::now() < end { self.launcher.sleep(Duration::from_millis(50)) }
        if self.launcher.is_helper(pid) { eprintln!("islands: pid {pid} still running after {} s; launching anyway", TAKEOVER_WAIT.as_secs()) }
        true
    }

    /// Run until quit: take over, launch, wait, relaunch with backoff. Blocks; run it on its own thread.
    pub fn run(&self) {
        let mut quick_failures = 0usize;
        while !self.quitting.load(Ordering::Acquire) {
            self.take_over();
            let started = Instant::now();
            match self.launcher.spawn() {
                Ok(pid) => {
                    self.child.store(pid, Ordering::Release);
                    eprintln!("islands: helper started (pid {pid})");
                    let how = self.launcher.wait(pid);
                    self.child.store(0, Ordering::Release);
                    if self.quitting.load(Ordering::Acquire) { eprintln!("islands: helper quit with Heimdall ({how})"); break }
                    eprintln!("islands: helper exited after {} s ({how})", started.elapsed().as_secs());
                }
                Err(e) => eprintln!("islands: helper did not start: {}", super::short_error(&e)),
            }
            if started.elapsed() >= self.healthy { quick_failures = 0 }
            let Some(wait) = self.backoff.get(quick_failures).copied() else {
                eprintln!("islands: helper failed {} times in a row; not relaunching until Heimdall restarts", quick_failures);
                break;
            };
            quick_failures += 1;
            eprintln!("islands: relaunching the helper in {} s (attempt {quick_failures} of {})", wait.as_secs(), self.backoff.len());
            let guard = self.wake.0.lock().unwrap_or_else(|e| e.into_inner());
            let _ = self.wake.1.wait_timeout_while(guard, wait, |_| !self.quitting.load(Ordering::Acquire));
        }
    }

    /// Heimdall's orderly quit: no relaunch, and the helper is asked to quit (it removes its pid and state files).
    pub fn quit(&self) {
        self.quitting.store(true, Ordering::Release);
        self.wake.1.notify_all();
        let pid = self.child.load(Ordering::Acquire);
        if pid > 0 { self.launcher.terminate(pid) }
    }

    pub fn child(&self) -> i32 { self.child.load(Ordering::Acquire) }
}

/// The real processes.
pub struct Native {
    pub exe: PathBuf,
    /// `FLOATING_ISLANDS_DIR` for the helper; `None`: unset (the default folder, shared with the agent's islands).
    pub dir: Option<PathBuf>,
    children: Mutex<Vec<std::process::Child>>,
}

impl Native {
    pub fn new(exe: PathBuf, dir: Option<PathBuf>) -> Native { Native { exe, dir, children: Mutex::new(Vec::new()) } }
}

/// `Heimdall.app/Contents/Helpers/FloatingIslands.app/Contents/MacOS/FloatingIslands` beside the running binary.
pub fn bundled_helper(exe: &std::path::Path) -> Option<PathBuf> {
    let contents = exe.parent()?.parent()?;
    let helper = contents.join("Helpers/FloatingIslands.app/Contents/MacOS/FloatingIslands");
    helper.is_file().then_some(helper)
}

fn pid_path(pid: i32) -> Option<String> {
    let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: proc_pidpath writes at most the buffer's size.
    let n = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
    (n > 0).then(|| String::from_utf8_lossy(&buf[..n as usize]).into_owned())
}

/// Folders whose pages the helper's viewer lets show HTTPS pictures and video (`islands::mac::viewer::web_rules`, bug
/// note 2026-11-04T00-46-09): the report folders in Heimdall's home, never the home itself (a downloaded page must
/// not get a way to send what it reads out). The helper refuses any folder that would trust too much.
pub const WEB_ROOTS_ENV: &str = "FLOATING_ISLANDS_WEB_ROOTS";
pub const REPORT_DIRS: [&str; 3] = ["Reports", "Assistant/Research", "Assistant/Plans"];

pub fn web_roots(home: &std::path::Path) -> String {
    if !home.is_absolute() || home.parent().is_none() { return String::new() }
    REPORT_DIRS.iter().map(|d| home.join(d).to_string_lossy().into_owned()).filter(|p| !p.contains(':')).collect::<Vec<_>>().join(":")
}

/// The helper's arguments: no lifetime limit, and quit when `parent` (this Heimdall) exits.
pub fn helper_args(parent: u32) -> Vec<String> {
    vec!["--max-hours".into(), "0".into(), "--parent-pid".into(), parent.to_string()]
}

impl Launcher for Native {
    fn spawn(&self) -> Result<i32> {
        let mut c = std::process::Command::new(&self.exe);
        c.args(helper_args(std::process::id())).stdin(std::process::Stdio::null());
        match &self.dir { Some(d) => { c.env("FLOATING_ISLANDS_DIR", d); } None => { c.env_remove("FLOATING_ISLANDS_DIR"); } }
        c.env(WEB_ROOTS_ENV, web_roots(&heimdall_home::home_dir()));
        let child = c.spawn().with_context(|| format!("launch {}", self.exe.display()))?;
        let pid = child.id() as i32;
        self.children.lock().unwrap_or_else(|e| e.into_inner()).push(child);
        Ok(pid)
    }
    fn wait(&self, pid: i32) -> String {
        let mut status = 0;
        // SAFETY: waitpid on our own child; the Child handle is dropped afterwards without waiting again.
        let r = unsafe { libc::waitpid(pid, &mut status, 0) };
        self.children.lock().unwrap_or_else(|e| e.into_inner()).retain(|c| c.id() as i32 != pid);
        if r < 0 { return format!("wait failed: {}", std::io::Error::last_os_error()) }
        if libc::WIFEXITED(status) { format!("exit {}", libc::WEXITSTATUS(status)) } else if libc::WIFSIGNALED(status) { format!("signal {}", libc::WTERMSIG(status)) } else { format!("status {status}") }
    }
    fn is_helper(&self, pid: i32) -> bool {
        // SAFETY: signal 0 only checks that the process exists.
        let exists = unsafe { libc::kill(pid, 0) == 0 };
        exists && pid_path(pid).is_some_and(|p| p.ends_with("/FloatingIslands"))
    }
    fn terminate(&self, pid: i32) {
        // SAFETY: SIGTERM to the helper (ours, or a FloatingIslands found by its pid file and path).
        unsafe { libc::kill(pid, libc::SIGTERM) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_helper_follows_heimdall_s_pid() {
        assert_eq!(helper_args(4242), ["--max-hours", "0", "--parent-pid", "4242"]);
    }

    /// Bug note 2026-11-04T00-46-09: the helper's viewer trusts Heimdall's report folders for remote pictures and
    /// video, never the home folder itself or `/`.
    #[test]
    fn the_helper_trusts_the_report_folders_for_remote_media() {
        assert_eq!(WEB_ROOTS_ENV, "FLOATING_ISLANDS_WEB_ROOTS");
        assert_eq!(web_roots(std::path::Path::new("/Users/x/Work")), "/Users/x/Work/Reports:/Users/x/Work/Assistant/Research:/Users/x/Work/Assistant/Plans");
        assert!(!web_roots(std::path::Path::new("/Users/x/Work")).split(':').any(|p| p == "/Users/x/Work"));
        assert_eq!(web_roots(std::path::Path::new("Vault")), "");
        assert_eq!(web_roots(std::path::Path::new("/")), "");
    }

    use std::sync::mpsc;

    /// A fake: each spawn gets the next pid; `exits` decides when each run ends.
    struct Fake {
        next: AtomicI32,
        alive: Mutex<Vec<i32>>,
        log: Mutex<Vec<String>>,
        exits: Mutex<mpsc::Receiver<()>>,
        fail_spawn: AtomicBool,
    }
    impl Launcher for Arc<Fake> {
        fn spawn(&self) -> Result<i32> {
            if self.fail_spawn.load(Ordering::Acquire) { self.log.lock().unwrap().push("spawn failed".into()); anyhow::bail!("no such file") }
            let pid = self.next.fetch_add(1, Ordering::AcqRel);
            self.alive.lock().unwrap().push(pid);
            self.log.lock().unwrap().push(format!("spawn {pid}"));
            Ok(pid)
        }
        fn wait(&self, pid: i32) -> String {
            let _ = self.exits.lock().unwrap().recv();
            self.alive.lock().unwrap().retain(|p| *p != pid);
            "exit 1".into()
        }
        fn is_helper(&self, pid: i32) -> bool { self.alive.lock().unwrap().contains(&pid) }
        fn terminate(&self, pid: i32) {
            self.log.lock().unwrap().push(format!("term {pid}"));
            self.alive.lock().unwrap().retain(|p| *p != pid);
        }
        fn sleep(&self, _: Duration) {}
    }
    fn fake(first_pid: i32) -> (Arc<Fake>, mpsc::Sender<()>) {
        let (tx, rx) = mpsc::channel();
        (Arc::new(Fake { next: AtomicI32::new(first_pid), alive: Mutex::new(vec![]), log: Mutex::new(vec![]), exits: Mutex::new(rx), fail_spawn: AtomicBool::new(false) }), tx)
    }
    fn until(what: impl Fn() -> bool) {
        let end = Instant::now() + Duration::from_secs(5);
        while !what() { assert!(Instant::now() < end, "timed out"); std::thread::sleep(Duration::from_millis(5)) }
    }

    #[test]
    fn takes_over_a_standalone_helper_then_relaunches_on_exit_and_quits_with_heimdall() {
        let d = tempfile::tempdir().unwrap();
        let root = Root(d.path().to_path_buf());
        let (f, exit) = fake(100);
        // A standalone helper (pid 42) wrote the pid file.
        f.alive.lock().unwrap().push(42);
        std::fs::write(root.pid(), "42\n").unwrap();
        let s = Supervisor::with_timing(f.clone(), root.clone(), vec![Duration::from_millis(1); 3], Duration::from_secs(60));
        let run = { let s = s.clone(); std::thread::spawn(move || s.run()) };
        until(|| s.child() == 100);
        assert_eq!(f.log.lock().unwrap()[..2], ["term 42".to_string(), "spawn 100".to_string()]);
        // Our own helper writes the pid file; it is never taken for a standalone one.
        std::fs::write(root.pid(), "100").unwrap();
        exit.send(()).unwrap();
        until(|| s.child() == 101);
        assert!(!f.log.lock().unwrap().contains(&"term 100".to_string()));
        s.quit();
        exit.send(()).unwrap();
        run.join().unwrap();
        let log = f.log.lock().unwrap().clone();
        assert_eq!(log.last().unwrap(), "term 101");
        assert_eq!(log.iter().filter(|l| l.starts_with("spawn")).count(), 2);
    }

    #[test]
    fn gives_up_after_the_backoff_runs_out_of_quick_failures() {
        let d = tempfile::tempdir().unwrap();
        let (f, _exit) = fake(1);
        f.fail_spawn.store(true, Ordering::Release);
        let s = Supervisor::with_timing(f.clone(), Root(d.path().to_path_buf()), vec![Duration::from_millis(1); 3], Duration::from_secs(60));
        s.run();
        assert_eq!(f.log.lock().unwrap().len(), 4, "the first try and three relaunches");
    }

    #[test]
    fn a_stale_pid_file_or_a_foreign_process_is_left_alone() {
        let d = tempfile::tempdir().unwrap();
        let root = Root(d.path().to_path_buf());
        let (f, _exit) = fake(1);
        let s = Supervisor::new(f.clone(), root.clone());
        assert!(!s.take_over(), "no pid file");
        std::fs::write(root.pid(), "77").unwrap();
        assert!(!s.take_over(), "pid 77 is not a FloatingIslands");
        std::fs::write(root.pid(), "garbage").unwrap();
        assert!(!s.take_over());
        assert!(f.log.lock().unwrap().is_empty());
    }

    #[test]
    fn quit_during_backoff_returns_at_once() {
        let d = tempfile::tempdir().unwrap();
        let (f, _exit) = fake(1);
        f.fail_spawn.store(true, Ordering::Release);
        let s = Supervisor::with_timing(f.clone(), Root(d.path().to_path_buf()), vec![Duration::from_secs(600)], Duration::from_secs(60));
        let run = { let s = s.clone(); std::thread::spawn(move || s.run()) };
        until(|| !f.log.lock().unwrap().is_empty());
        let t = Instant::now();
        s.quit();
        run.join().unwrap();
        assert!(t.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn finds_the_helper_inside_the_app_bundle_only() {
        let d = tempfile::tempdir().unwrap();
        let exe = d.path().join("Heimdall.app/Contents/MacOS/heimdall");
        assert!(bundled_helper(&exe).is_none());
        let helper = d.path().join("Heimdall.app/Contents/Helpers/FloatingIslands.app/Contents/MacOS/FloatingIslands");
        std::fs::create_dir_all(helper.parent().unwrap()).unwrap();
        std::fs::write(&helper, "").unwrap();
        assert_eq!(bundled_helper(&exe), Some(helper));
    }
}
