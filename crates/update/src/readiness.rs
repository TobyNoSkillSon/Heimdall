//! The readiness handshake between a freshly installed app and the install tool.
//!
//! The app writes `<state dir>/ready.json`: `{"app_pid", "version", "phase", "detail"}`. Phases: `starting` (launched,
//! still loading), `ready` (its start finished), `degraded` (running, but a launch model was refused for memory: kept,
//! since the previous version could not fix that either), `failing` (a launch model fails with an error; the app may
//! still retry it). The tool accepts a status only from the installed app's own process (its executable is the
//! destination's, by file identity) at the new version.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const FILE: &str = "ready.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    Starting,
    Ready,
    Degraded,
    Failing,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    pub app_pid: i32,
    pub version: String,
    pub phase: Phase,
    #[serde(default)]
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    Ready(String),
    Waiting(String),
    Failing(String),
    Degraded(String),
}

pub fn path(state_dir: &Path) -> PathBuf {
    state_dir.join(FILE)
}

/// The app's side: records its phase (atomic write). Errors are the caller's to log.
pub fn report(state_dir: &Path, version: &str, phase: Phase, detail: &str) -> crate::Result<()> {
    let status = Status { app_pid: std::process::id() as i32, version: version.to_owned(), phase, detail: detail.to_owned() };
    crate::atomic_write(&path(state_dir), &serde_json::to_vec(&status).unwrap())
}

pub fn read_pid(data: Option<&[u8]>) -> Option<i32> {
    serde_json::from_slice::<Status>(data?).ok().map(|s| s.app_pid)
}

/// The tool's rule. `is_installed_app(pid)`: the pid is alive and runs the installed app's executable.
pub fn evaluate(data: Option<&[u8]>, version: &str, is_installed_app: &dyn Fn(i32) -> bool) -> State {
    let Some(data) = data else { return State::Waiting("the new version has not written its status yet".into()) };
    let Ok(status) = serde_json::from_slice::<Status>(data) else {
        return State::Waiting("status file is incomplete".into());
    };
    if !is_installed_app(status.app_pid) {
        return State::Waiting("status is from an earlier launch".into());
    }
    if status.version != version {
        // The installed app's own process names another version: a mispackaged build (Info.plist and binary
        // disagree). Waiting cannot fix it; it counts as failing, so the rollback comes after the settle time and not
        // the load timeout (VM finding P-09, 28 Oct).
        return State::Failing(format!("the new app reports version {} but {version} was installed", status.version));
    }
    let pid = status.app_pid;
    match status.phase {
        Phase::Starting => State::Waiting(format!("starting{}", suffix(&status.detail))),
        Phase::Ready => State::Ready(format!("running (pid {pid}){}", suffix(&status.detail))),
        Phase::Degraded => State::Degraded(format!("running (pid {pid}){}", suffix(&status.detail))),
        Phase::Failing => State::Failing(if status.detail.is_empty() { "failing".into() } else { status.detail }),
    }
}

fn suffix(detail: &str) -> String {
    if detail.is_empty() {
        String::new()
    } else {
        format!(": {detail}")
    }
}

/// True when `pid` is alive and its executable is the same file as `executable`.
pub fn runs(pid: i32, executable: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    if pid <= 0 {
        return false;
    }
    if !alive(pid) {
        return false;
    }
    let Some(running) = pid_path(pid) else { return false };
    match (std::fs::metadata(running), std::fs::metadata(executable)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
    }
}

/// The process exists, including another user's (signal 0 answers EPERM for those).
pub fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    pid > 0 && (unsafe { libc::kill(pid, 0) } == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM))
}

/// The executable path of a process (proc_pidpath).
pub fn pid_path(pid: i32) -> Option<PathBuf> {
    extern "C" {
        fn proc_pidpath(pid: libc::c_int, buffer: *mut libc::c_void, size: u32) -> libc::c_int;
    }
    let mut buffer = vec![0u8; 4 * libc::PATH_MAX as usize];
    // SAFETY: the buffer is writable for its full length, which is passed as the size.
    let n = unsafe { proc_pidpath(pid, buffer.as_mut_ptr().cast(), buffer.len() as u32) };
    if n <= 0 {
        return None;
    }
    buffer.truncate(n as usize);
    Some(PathBuf::from(String::from_utf8_lossy(&buffer).into_owned()))
}
