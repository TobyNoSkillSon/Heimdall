//! Background jobs share the idle window with listening; speech always wins.
use anyhow::{Context, Result};
use std::{
    collections::HashSet,
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{Arc, Condvar, Mutex},
    thread,
    time::{Duration, Instant},
};
pub const SPEECH_QUIET: Duration = Duration::from_secs(120);
pub const POLL: Duration = Duration::from_millis(250);

pub fn child_mode() -> Result<bool> {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("--speech-split-helper") => { heimdall_night::correction::helper(PathBuf::from(args.next().context("correction models")?))?; }
        Some("worker") => {
            heimdall_night::gate::background()?;
            std::env::set_var("HEIMDALL_SPEAKERS_METAL", "0");
            heimdall_night::job::worker(&PathBuf::from(args.next().context("night request")?))?;
        }
        Some("--night-process") => {
            heimdall_night::gate::background()?;
            std::env::set_var("HEIMDALL_SPEAKERS_METAL", "0");
            let archive = PathBuf::from(args.next().context("night archive")?);
            let models = PathBuf::from(args.next().context("night models")?);
            let output = PathBuf::from(args.next().context("night output")?);
            let enrollments = args.next().map(PathBuf::from);
            let outcome = heimdall_night::job::process(&archive, &models, &output, enrollments.as_deref())?;
            if outcome.pending { std::process::exit(76); }
        }
        _ => return Ok(false),
    }
    Ok(true)
}
struct State {
    permit: bool,
    stopped: bool,
    child: Option<Child>,
    retry: Option<Instant>,
    completed_in_window: bool,
    speaking: HashSet<String>,
    quiet_since: Instant,
}
impl Default for State {
    fn default() -> Self {
        Self {
            permit: false,
            stopped: false,
            child: None,
            retry: None,
            completed_in_window: false,
            speaking: HashSet::new(),
            quiet_since: Instant::now(),
        }
    }
}
fn stop_child(state: &mut State) {
    if let Some(mut child) = state.child.take() {
        unsafe {
            libc::kill(-(child.id() as i32), libc::SIGKILL);
        }
        let _ = child.kill();
        let _ = child.wait();
    }
}
#[derive(Clone)]
pub struct Activity(Arc<(Mutex<State>, Condvar)>);
impl Activity {
    /// Called directly on the live VAD output thread before the UI/ASR queue.
    /// Killing/waiting is owned by the scheduler, never by the capture/command path.
    pub fn speech(&self, source: &str, speaking: bool) {
        let (lock, wake) = &*self.0;
        let mut s = lock.lock().unwrap();
        if speaking {
            s.speaking.insert(source.into());
        } else {
            s.speaking.remove(source);
        }
        s.quiet_since = Instant::now();
        s.completed_in_window = false;
        wake.notify_one();
    }
}
pub struct Scheduler {
    activity: Activity,
    thread: Option<thread::JoinHandle<()>>,
}
impl Scheduler {
    pub fn start(
        data: PathBuf,
        models: PathBuf,
        floor: Arc<std::sync::atomic::AtomicU64>,
    ) -> Result<Self> {
        let output = data.join("night");
        heimdall_night::storage::cleanup(&output)?;
        crate::settings::private_dir(&output)?;
        let floor_file = output.join("live-floor");
        let mut previous = floor.load(std::sync::atomic::Ordering::Acquire);
        crate::settings::private_write(&floor_file, previous.to_string().as_bytes())?;
        let probe_file = floor_file.clone();
        Self::start_managed(
            move || {
                let current = floor.load(std::sync::atomic::Ordering::Acquire);
                if current != previous {
                    crate::settings::private_write(&probe_file, current.to_string().as_bytes())?;
                    previous = current;
                    anyhow::bail!("disk floor changed; restart night job");
                }
                heimdall_night::gate::state()
            },
            move || {
                use std::os::unix::process::CommandExt;
                let mut cmd = Command::new(std::env::current_exe()?);
                cmd.arg("--night-process")
                    .arg(data.join("archive"))
                    .arg(&models)
                    .arg(data.join("night"));
                let enrollments = data.join("speakers.json");
                if enrollments.is_file() {
                    cmd.arg(enrollments);
                }
                cmd.env("HEIMDALL_NIGHT_FLOOR_FILE", &floor_file);
                cmd.process_group(0)
                    .stdout(Stdio::null())
                    .stderr(Stdio::inherit());
                Ok(cmd.spawn()?)
            },
            SPEECH_QUIET,
            move || heimdall_night::storage::cleanup(&output),
        )
    }
    // Only explicit isolated diagnostics inject observations/shorten the quiet wait.
    pub(crate) fn start_with(
        probe: impl FnMut() -> Result<heimdall_night::gate::State> + Send + 'static,
        spawn: impl FnMut() -> Result<Child> + Send + 'static,
        quiet: Duration,
    ) -> Result<Self> {
        Self::start_managed(probe, spawn, quiet, || Ok(()))
    }
    fn start_managed(
        mut probe: impl FnMut() -> Result<heimdall_night::gate::State> + Send + 'static,
        mut spawn: impl FnMut() -> Result<Child> + Send + 'static,
        quiet: Duration,
        mut cleanup: impl FnMut() -> Result<()> + Send + 'static,
    ) -> Result<Self> {
        let shared = Arc::new((Mutex::new(State::default()), Condvar::new()));
        let background = shared.clone();
        let thread = thread::Builder::new()
            .name("heimdall-night-scheduler".into())
            .spawn(move || {
                if let Err(e) = heimdall_night::gate::background_thread() {
                    eprintln!("Night scheduler unavailable: {e:#}");
                    return;
                }
                let (lock, wake) = &*background;
                let retire = |s: &mut State| {
                    let owned = s.child.is_some();
                    stop_child(s);
                    owned
                };
                let reclaim = |cleanup: &mut dyn FnMut() -> Result<()>| {
                    if let Err(e) = cleanup() {
                        eprintln!("Night scratch cleanup: {e:#}");
                    }
                };
                loop {
                    let mut s = lock.lock().unwrap();
                    if !s.permit && !s.stopped {
                        if retire(&mut s) {
                            drop(s);
                            reclaim(&mut cleanup);
                            s = lock.lock().unwrap();
                        }
                        s = wake.wait_while(s, |s| !s.permit && !s.stopped).unwrap();
                    }
                    if s.stopped {
                        let retired = retire(&mut s);
                        drop(s);
                        if retired {
                            reclaim(&mut cleanup);
                        }
                        break;
                    }
                    let mut retired = false;
                    if let Some(status) = s.child.as_mut().and_then(|c| c.try_wait().ok().flatten())
                    {
                        retired = retire(&mut s);
                        s.completed_in_window = status.success();
                        s.retry = if status.code()==Some(76) {None}else{Some(Instant::now())};
                    }
                    // Probe without holding a lock needed by live VAD events.
                    drop(s);
                    if retired {
                        reclaim(&mut cleanup);
                    }
                    let observation = probe();
                    let mut s = lock.lock().unwrap();
                    let eligible = s.permit
                        && s.speaking.is_empty()
                        && s.quiet_since.elapsed() >= quiet
                        && observation.is_ok_and(|o| o.eligible());
                    if !eligible {
                        let retired = retire(&mut s);
                        s.completed_in_window = false;
                        if retired {
                            drop(s);
                            reclaim(&mut cleanup);
                            s = lock.lock().unwrap();
                        }
                    } else if s.child.is_none()
                        && !s.completed_in_window
                        && s.retry
                            .is_none_or(|at| at.elapsed() >= Duration::from_secs(60))
                    {
                        match spawn() {
                            Ok(child) => s.child = Some(child),
                            Err(e) => {
                                eprintln!("Night pass deferred: {e:#}");
                                s.retry = Some(Instant::now());
                            }
                        }
                    }
                    wake.notify_all();
                    let _ = wake.wait_timeout(s, POLL).unwrap();
                }
            })?;
        Ok(Self {
            activity: Activity(shared),
            thread: Some(thread),
        })
    }
    pub fn activity(&self) -> Activity {
        self.activity.clone()
    }
    pub fn allow(&self, permit: bool) {
        let (lock, wake) = &*self.activity.0;
        let mut s = lock.lock().unwrap();
        if s.permit != permit {
            s.permit = permit;
            s.completed_in_window = false;
            wake.notify_one();
        }
    }
    pub(crate) fn running(&self) -> bool {
        self.activity.0 .0.lock().unwrap().child.is_some()
    }
    #[cfg(test)]
    fn expire_quiet(&self) {
        self.activity.0 .0.lock().unwrap().quiet_since = Instant::now() - SPEECH_QUIET;
        self.activity.0 .1.notify_one();
    }
}
impl Drop for Scheduler {
    fn drop(&mut self) {
        let (lock, wake) = &*self.activity.0;
        {
            let mut s = lock.lock().unwrap();
            s.stopped = true;
            wake.notify_one();
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn wait(s: &Scheduler, running: bool) {
        let begin = Instant::now();
        while s.running() != running {
            assert!(begin.elapsed() < Duration::from_secs(3));
            thread::sleep(Duration::from_millis(5));
        }
    }
    #[test]
    fn listening_on_ac_idle_silence_preempts_all_streams_input_and_power_and_resumes() {
        use std::os::unix::process::CommandExt;
        let gate = Arc::new(Mutex::new(heimdall_night::gate::State {
            on_ac: true,
            idle_seconds: 300.,
        }));
        let g = gate.clone();
        let scheduler = Scheduler::start_with(
            move || Ok(*g.lock().unwrap()),
            || {
                Ok(Command::new("/bin/sleep")
                    .arg("60")
                    .process_group(0)
                    .spawn()?)
            },
            SPEECH_QUIET,
        )
        .unwrap();
        scheduler.allow(true); // Listening remains On for the entire test.
        thread::sleep(Duration::from_millis(50));
        assert!(!scheduler.running());
        scheduler.expire_quiet();
        wait(&scheduler, true);
        for source in ["mic:primary", "process:discord", "process:music"] {
            scheduler.activity().speech(source, true);
            wait(&scheduler, false);
            scheduler.expire_quiet();
            thread::sleep(Duration::from_millis(50));
            assert!(!scheduler.running());
            scheduler.activity().speech(source, false);
            scheduler.expire_quiet();
            wait(&scheduler, true);
        }
        for denied in [
            heimdall_night::gate::State {
                on_ac: true,
                idle_seconds: 0.,
            },
            heimdall_night::gate::State {
                on_ac: false,
                idle_seconds: 500.,
            },
            heimdall_night::gate::State {
                on_ac: true,
                idle_seconds: f64::NAN,
            },
        ] {
            *gate.lock().unwrap() = denied;
            wait(&scheduler, false);
            *gate.lock().unwrap() = heimdall_night::gate::State {
                on_ac: true,
                idle_seconds: 300.,
            };
            wait(&scheduler, true);
        }
        scheduler.allow(false);
        wait(&scheduler, false);
    }
    #[test]
    fn preemption_reclaims_owned_scratch_before_next_window() {
        use std::os::unix::process::CommandExt;
        let t = tempfile::tempdir().unwrap();
        let output = t.path().to_path_buf();
        let owned = output.join("night-snapshot-preempt");
        let spawn_dir = owned.clone();
        let cleanup_dir = output.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let scheduler = Scheduler::start_managed(
            || {
                Ok(heimdall_night::gate::State {
                    on_ac: true,
                    idle_seconds: 300.,
                })
            },
            move || {
                std::fs::create_dir_all(&spawn_dir)?;
                std::fs::write(spawn_dir.join("OWNER"), heimdall_night::storage::OWNER)?;
                std::fs::write(spawn_dir.join("private-pcm"), [1; 1024])?;
                Ok(Command::new("/bin/sleep")
                    .arg("60")
                    .process_group(0)
                    .spawn()?)
            },
            Duration::ZERO,
            move || {
                heimdall_night::storage::cleanup(&cleanup_dir)?;
                tx.send(())?;
                Ok(())
            },
        )
        .unwrap();
        scheduler.allow(true);
        wait(&scheduler, true);
        assert!(owned.exists());
        scheduler.activity().speech("mic", true);
        rx.recv_timeout(Duration::from_secs(3)).unwrap();
        assert!(!owned.exists());
        assert!(!scheduler.running());
    }
    #[test]
    fn completed_window_does_not_spin_and_speech_opens_a_new_window() {
        use std::os::unix::process::CommandExt;
        let (tx, rx) = std::sync::mpsc::channel();
        let s = Scheduler::start_with(
            || {
                Ok(heimdall_night::gate::State {
                    on_ac: true,
                    idle_seconds: 300.,
                })
            },
            move || {
                tx.send(()).unwrap();
                Ok(Command::new("/usr/bin/true").process_group(0).spawn()?)
            },
            Duration::ZERO,
        )
        .unwrap();
        s.allow(true);
        rx.recv_timeout(Duration::from_secs(3)).unwrap();
        assert!(rx.recv_timeout(Duration::from_millis(500)).is_err());
        s.activity().speech("mic", true);
        thread::sleep(Duration::from_millis(20));
        s.activity().speech("mic", false);
        s.activity.0 .0.lock().unwrap().retry = None;
        rx.recv_timeout(Duration::from_secs(3)).unwrap();
    }
}
