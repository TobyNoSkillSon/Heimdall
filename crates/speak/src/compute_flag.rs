//! Arsenal yields to Jarvis: the red/green compute flag and the RAM-yield status (bug note 2026-11-09T12-43-19).
//!
//! "whenever Heimdall asks for room to generate speech, Arsenal's generation is halted (red flag).
//! When Heimdall finishes it raises a green flag, and only then does Arsenal continue." And for a massive Arsenal load,
//! Heimdall unloads Drama and gives that RAM to Arsenal; the listener (VAD), smart-turn, Parakeet and the decider are
//! never unloaded; embeddings and search may catch up later.
//!
//! The contract both apps share is docs/personal/arsenal-yield.md (Arsenal's twin: its docs/HEIMDALL.md). In short:
//! - `<Heimdall data>/compute/flag.json`, written by Heimdall (atomic rename): version 1, `state` red or green, a
//!   `seq` that grows with every raise, Heimdall's `pid`, and `expires` (unix seconds). Red is refreshed every
//!   `REFRESH` while Jarvis generates, so a Heimdall that died leaves a red that lapses by itself within `TTL`
//!   (`is_red`, the reader's rule, which Arsenal mirrors).
//! - `<Heimdall data>/compute/yield.json`, written by Heimdall when a job asks for room (`"yield": true` in its
//!   `memory-needs` file): what was asked, what was let go and what is kept.
//! - `<Heimdall data>/compute/arsenal.json`, written by Arsenal: its acknowledgement (paused or running, for which
//!   `seq`, when). Heimdall does not read it.
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const VERSION: u32 = 1;
/// The folder in Heimdall's data directory that holds the shared files.
pub const DIR: &str = "compute";
pub const FLAG_FILE: &str = "flag.json";
pub const YIELD_FILE: &str = "yield.json";
/// A red not refreshed for this long is stale (Heimdall crashed or hung): readers treat it as green.
pub const TTL: Duration = Duration::from_secs(10);
/// Red is rewritten this often while it is up.
pub const REFRESH: Duration = Duration::from_secs(2);
/// Green waits this long after the last line ends: the next line of a reply follows within it, and Arsenal is not
/// stopped and started again between sentences.
pub const LINGER: Duration = Duration::from_millis(750);
/// The longest one red may last however often it is refreshed (a stuck raise must not starve Arsenal for good).
/// Arsenal enforces the same cap from `since`.
pub const MAX_RED: Duration = Duration::from_secs(180);

/// What Heimdall lets go when a job asks for room, and what it never lets go.
pub const YIELDS: [&str; 2] = ["Jarvis (Drama 3)", "embeddings (memory search indexing)"];
pub const KEEPS: [&str; 4] = ["listener (voice activity)", "smart-turn", "Parakeet (transcription)", "decider"];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Red,
    Green,
}

/// `flag.json`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FlagFile {
    pub version: u32,
    pub state: State,
    pub seq: u64,
    pub holder: String,
    pub pid: u32,
    pub reason: String,
    /// When this red went up (unix seconds); for green, when green was written.
    pub since: f64,
    /// Red counts until then (unix seconds); refreshed while red is up.
    pub expires: f64,
}

pub fn unix(t: SystemTime) -> f64 {
    t.duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.)
}

/// The reader's rule (Arsenal mirrors it, `ComputeFlag.isRed`): red only when the file is version 1, says red, has not
/// expired, has lasted no longer than `MAX_RED`, and its writer is alive. Anything else, a missing or malformed file
/// included, is green: Arsenal is never stopped by a flag it cannot read.
pub fn is_red(text: &str, now: SystemTime, alive: &dyn Fn(u32) -> bool) -> bool {
    let Ok(flag) = serde_json::from_str::<FlagFile>(text) else { return false };
    let now = unix(now);
    flag.version == VERSION
        && flag.state == State::Red
        && now < flag.expires
        && now - flag.since <= MAX_RED.as_secs_f64()
        && alive(flag.pid)
}

/// Whether a process lives (signal 0; EPERM means it lives under another user).
pub fn pid_alive(pid: u32) -> bool {
    if pid == 0 || pid > i32::MAX as u32 {
        return false;
    }
    let r = unsafe { libc::kill(pid as i32, 0) };
    r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Writes `value` to `path` whole: a temporary file beside it, then a rename.
pub fn write_atomic(path: &Path, value: &impl Serialize) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&tmp, serde_json::to_vec(value)?)?;
    std::fs::rename(&tmp, path)
}

#[derive(Clone, Copy, Debug)]
pub struct Timing {
    pub ttl: Duration,
    pub refresh: Duration,
    pub linger: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self { ttl: TTL, refresh: REFRESH, linger: LINGER }
    }
}

#[derive(Default)]
struct Inner {
    /// Raises alive now.
    raised: usize,
    /// Red is on disk: its seq, since, and when it was last written.
    red: Option<(u64, SystemTime, Instant)>,
    seq: u64,
    /// The last raise ended at.
    lowered: Option<Instant>,
    reason: String,
    stop: bool,
    /// The last write failed (logged once per failure run).
    failing: bool,
}

/// Heimdall's side of the flag: `raise` returns a guard; red stays up while any guard lives, plus `LINGER`.
pub struct Flag {
    path: PathBuf,
    timing: Timing,
    inner: Mutex<Inner>,
    wake: Condvar,
}

/// Red is up while this lives.
pub struct Red(Arc<Flag>);

impl Drop for Red {
    fn drop(&mut self) {
        let mut inner = self.0.inner.lock().unwrap();
        inner.raised = inner.raised.saturating_sub(1);
        if inner.raised == 0 {
            inner.lowered = Some(Instant::now());
        }
        drop(inner);
        self.0.wake.notify_all();
    }
}

impl Flag {
    /// The flag in `<data>/compute/flag.json`. Writes green at once: a red left by a Heimdall that crashed is cleared
    /// as soon as the next one starts.
    pub fn start(data: &Path) -> Arc<Self> {
        Self::start_with(data.join(DIR).join(FLAG_FILE), Timing::default())
    }
    pub fn start_with(path: PathBuf, timing: Timing) -> Arc<Self> {
        let flag = Arc::new(Self { path, timing, inner: Mutex::new(Inner::default()), wake: Condvar::new() });
        flag.write(State::Green, 0, SystemTime::now(), "started");
        let worker = flag.clone();
        let _ = std::thread::Builder::new().name("heimdall-compute-flag".into()).spawn(move || worker.run());
        flag
    }
    /// Red goes up now (written before this returns, so the caller's generation starts after it).
    pub fn raise(self: &Arc<Self>, reason: &str) -> Red {
        let mut inner = self.inner.lock().unwrap();
        inner.raised += 1;
        inner.lowered = None;
        if inner.red.is_none() {
            inner.seq += 1;
            let (seq, since) = (inner.seq, SystemTime::now());
            inner.reason = reason.to_owned();
            inner.red = Some((seq, since, Instant::now()));
            let ok = self.write(State::Red, seq, since, reason);
            self.note(&mut inner, ok);
        }
        drop(inner);
        self.wake.notify_all();
        Red(self.clone())
    }
    /// Red is up (on disk) now.
    #[cfg(test)]
    pub fn red(&self) -> bool {
        self.inner.lock().unwrap().red.is_some()
    }
    /// The thread: refreshes red while raised; writes green `linger` after the last guard drops. Idle (green), it
    /// sleeps until the next raise: nothing runs at rest.
    fn run(self: Arc<Self>) {
        let mut inner = self.inner.lock().unwrap();
        loop {
            if inner.stop {
                return;
            }
            let Some((seq, since, written)) = inner.red else {
                inner = self.wake.wait(inner).unwrap();
                continue;
            };
            let now = Instant::now();
            if inner.raised == 0 {
                let lowered = inner.lowered.unwrap_or(now);
                if now.duration_since(lowered) >= self.timing.linger {
                    inner.red = None;
                    let ok = self.write(State::Green, seq, SystemTime::now(), "Jarvis finished");
                    self.note(&mut inner, ok);
                    continue;
                }
                let wait = self.timing.linger - now.duration_since(lowered);
                inner = self.wake.wait_timeout(inner, wait.min(self.timing.refresh)).unwrap().0;
                continue;
            }
            if now.duration_since(written) >= self.timing.refresh {
                let reason = inner.reason.clone();
                inner.red = Some((seq, since, now));
                let ok = self.write(State::Red, seq, since, &reason);
                self.note(&mut inner, ok);
                continue;
            }
            let wait = self.timing.refresh - now.duration_since(written);
            inner = self.wake.wait_timeout(inner, wait).unwrap().0;
        }
    }
    fn note(&self, inner: &mut Inner, ok: bool) {
        if !ok && !inner.failing {
            eprintln!("compute flag: cannot write {}", self.path.display());
        }
        inner.failing = !ok;
    }
    fn write(&self, state: State, seq: u64, since: SystemTime, reason: &str) -> bool {
        let now = SystemTime::now();
        let expires = match state {
            State::Red => unix(now) + self.timing.ttl.as_secs_f64(),
            State::Green => unix(now),
        };
        let file = FlagFile {
            version: VERSION,
            state,
            seq,
            holder: "heimdall".into(),
            pid: std::process::id(),
            reason: reason.into(),
            since: if state == State::Red { unix(since) } else { unix(now) },
            expires,
        };
        write_atomic(&self.path, &file).is_ok()
    }
    /// Stops the thread and writes green (app exit).
    pub fn shutdown(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.stop = true;
        let seq = inner.seq;
        inner.red = None;
        drop(inner);
        self.wake.notify_all();
        self.write(State::Green, seq, SystemTime::now(), "Heimdall quit");
    }
}

static GLOBAL: OnceLock<Arc<Flag>> = OnceLock::new();

/// The app's flag (`Flag::start` on its data directory); tests and tooling never install one.
pub fn install(flag: Arc<Flag>) {
    let _ = GLOBAL.set(flag);
}

/// Red up for Jarvis's generation, when the app installed the flag.
pub fn raise(reason: &str) -> Option<Red> {
    GLOBAL.get().map(|flag| flag.raise(reason))
}

// ---------------------------------------------------------------------------------------------------------------
// RAM yield status.

/// `yield.json`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct YieldFile {
    pub version: u32,
    /// `none`: nobody asks. `yielding`: asked, something still loaded. `yielded`: everything Heimdall lets go is out.
    pub state: String,
    pub asked_by: Vec<String>,
    /// Of `YIELDS`, what is out of memory now.
    pub released: Vec<String>,
    pub keeps: Vec<String>,
    pub pid: u32,
    pub at: f64,
}

/// The yield's state from what was asked and what is out.
pub fn yield_state(asked: bool, jarvis_out: bool, embeddings_out: bool) -> &'static str {
    match (asked, jarvis_out && embeddings_out) {
        (false, _) => "none",
        (true, false) => "yielding",
        (true, true) => "yielded",
    }
}

pub fn yield_file(asked_by: Vec<String>, jarvis_out: bool, embeddings_out: bool, now: SystemTime) -> YieldFile {
    let mut released = Vec::new();
    if jarvis_out {
        released.push(YIELDS[0].to_owned());
    }
    if embeddings_out {
        released.push(YIELDS[1].to_owned());
    }
    YieldFile {
        version: VERSION,
        state: yield_state(!asked_by.is_empty(), jarvis_out, embeddings_out).into(),
        asked_by,
        released,
        keeps: KEEPS.iter().map(|s| (*s).to_owned()).collect(),
        pid: std::process::id(),
        at: unix(now),
    }
}

static YIELD_ASKED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// A job asks Heimdall for room now: the embeddings indexer stays unloaded until it no longer does.
pub fn yield_asked() -> bool {
    YIELD_ASKED.load(std::sync::atomic::Ordering::Acquire)
}
pub fn set_yield_asked(asked: bool) {
    YIELD_ASKED.store(asked, std::sync::atomic::Ordering::Release);
}

#[cfg(test)]
#[path = "compute_flag_tests.rs"]
mod tests;
