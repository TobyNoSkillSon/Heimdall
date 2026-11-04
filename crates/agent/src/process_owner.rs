//! Generation ownership survives setsid, double fork and reparenting. No cached
//! PID or process-group number is authority: every signal rechecks kernel birth.
use anyhow::{Context, Result};
use std::{
    collections::HashSet,
    path::Path,
    time::{Duration, Instant},
};

pub(crate) const TOKEN: &str = "HEIMDALL_AGENT_OWNER";
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, serde::Serialize, serde::Deserialize)]
pub(crate) struct Identity {
    pub pid: i32,
    sec: u64,
    usec: u64,
}
#[derive(Clone)]
struct Process {
    id: Identity,
    parent: i32,
}
impl Identity {
    #[cfg(test)]
    pub fn invalidate_for_test(&mut self) {
        self.usec = self.usec.wrapping_add(1);
    }
    pub fn read(pid: i32) -> Option<Self> {
        info(pid).map(|p| p.id)
    }
    pub fn kill(self) {
        if Self::read(self.pid) == Some(self) {
            unsafe {
                libc::kill(self.pid, libc::SIGKILL);
            }
        }
    }
}
fn info(pid: i32) -> Option<Process> {
    let mut b: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of_val(&b) as i32;
    let n = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            &mut b as *mut _ as *mut _,
            size,
        )
    };
    // Zombies cannot execute, fork or hold a pipe. Exclude them from retirement.
    if n != size || b.pbi_uid != unsafe { libc::getuid() } || b.pbi_status == 5 {
        return None;
    }
    Some(Process {
        id: Identity {
            pid,
            sec: b.pbi_start_tvsec,
            usec: b.pbi_start_tvusec,
        },
        parent: b.pbi_ppid as i32,
    })
}
/// Kernel boot second, as wall time. Clock steps move it, so it only judges
/// receipts written before boot sessions were recorded.
fn boot_sec() -> Option<u64> {
    let mut tv: libc::timeval = unsafe { std::mem::zeroed() };
    let mut size = std::mem::size_of_val(&tv);
    let mut mib = [libc::CTL_KERN, libc::KERN_BOOTTIME];
    let n = unsafe {
        libc::sysctl(mib.as_mut_ptr(), 2, &mut tv as *mut _ as *mut _, &mut size, std::ptr::null_mut(), 0)
    };
    (n == 0 && tv.tv_sec > 0).then_some(tv.tv_sec as u64)
}
/// A receipt whose boot session could not be read: it never counts as another boot's.
const UNKNOWN_BOOT: &str = "unknown";
/// Guardians born this long before kern.boottime predate it even after a plausible clock step.
const LEGACY_BOOT_MARGIN_SEC: u64 = 600;
/// The kernel's per-boot UUID. Unlike wall time it cannot drift or be stepped.
fn boot_session() -> Option<String> {
    let mut buffer = [0u8; 64];
    let mut size = buffer.len();
    let n = unsafe {
        libc::sysctlbyname(c"kern.bootsessionuuid".as_ptr(), buffer.as_mut_ptr().cast(), &mut size, std::ptr::null_mut(), 0)
    };
    let text = std::ffi::CStr::from_bytes_until_nul(&buffer).ok()?.to_str().ok()?;
    (n == 0 && uuid::Uuid::parse_str(text).is_ok()).then(|| text.to_owned())
}
fn processes() -> Result<Vec<Process>> {
    // proc_listpids reports byte counts, including in the sizing call. Allow
    // births during enumeration; retry rather than silently truncate authority.
    let mut capacity = unsafe { libc::proc_listpids(1, 0, std::ptr::null_mut(), 0) };
    anyhow::ensure!(capacity > 0, "cannot enumerate owned processes");
    loop {
        let mut pids = vec![0i32; capacity as usize / 4 + 512];
        let bytes = (pids.len() * 4) as i32;
        let n = unsafe { libc::proc_listpids(1, 0, pids.as_mut_ptr() as *mut _, bytes) };
        anyhow::ensure!(n > 0, "cannot enumerate owned processes");
        if n == bytes {
            capacity = bytes * 2;
            continue;
        }
        return Ok(pids[..n as usize / 4]
            .iter()
            .filter_map(|&pid| info(pid))
            .collect());
    }
}
fn carries(pid: i32, token: &str) -> bool {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    // A null-buffer sizing call can report argv bytes only on macOS. Allocate
    // KERN_ARGMAX so the second call includes the environment as well.
    static ARG_MAX: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    let mut size = *ARG_MAX.get_or_init(|| {
        let mut limit = 0i32;
        let mut n = std::mem::size_of_val(&limit);
        let mut key = [libc::CTL_KERN, libc::KERN_ARGMAX];
        let result = unsafe {
            libc::sysctl(
                key.as_mut_ptr(),
                2,
                &mut limit as *mut _ as *mut _,
                &mut n,
                std::ptr::null_mut(),
                0,
            )
        };
        if result == 0 && limit > 0 {
            limit as usize
        } else {
            2 * 1024 * 1024
        }
    });
    let mut bytes = vec![0u8; size];
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            bytes.as_mut_ptr() as *mut _,
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    } != 0
        || size < 4
    {
        return false;
    }
    bytes.truncate(size);
    let argc = i32::from_ne_bytes(bytes[..4].try_into().unwrap());
    if argc < 0 {
        return false;
    }
    // KERN_PROCARGS2: argc, executable NUL, padding NULs, argc argv
    // strings, then environment. Never mistake a matching argument for ownership.
    let mut pos = 4;
    while pos < size && bytes[pos] != 0 {
        pos += 1;
    }
    while pos < size && bytes[pos] == 0 {
        pos += 1;
    }
    for _ in 0..argc {
        while pos < size && bytes[pos] != 0 {
            pos += 1;
        }
        pos += 1;
    }
    let expected = format!("{TOKEN}={token}");
    bytes
        .get(pos..)
        .is_some_and(|env| env.split(|&b| b == 0).any(|s| s == expected.as_bytes()))
}
fn responsible_pid(pid: i32) -> i32 {
    type Lookup = unsafe extern "C" fn(i32) -> i32;
    static LOOKUP: std::sync::OnceLock<Option<Lookup>> = std::sync::OnceLock::new();
    let lookup = LOOKUP.get_or_init(|| unsafe {
        let symbol = libc::dlsym(libc::RTLD_DEFAULT, c"responsibility_get_pid_responsible_for_pid".as_ptr());
        if symbol.is_null() { None } else { Some(std::mem::transmute::<*mut libc::c_void, Lookup>(symbol)) }
    });
    lookup.map_or(-1, |lookup| unsafe { lookup(pid) })
}
const GUARDIAN_MODE: &str = "--heimdall-guardian";
unsafe extern "C" {
    fn heimdall_guardian_main(argc: i32, argv: *mut *mut libc::c_char) -> i32;
}
/// Called before any app/UI/audio/model initialization. No AppKit calls.
pub fn guardian_mode() -> Option<i32> {
    use std::os::unix::ffi::OsStrExt;
    let args: Vec<_> = std::env::args_os().collect();
    if args.get(1).is_some_and(|a| a == "--heimdall-relaunch") {
        return Some(relaunch_mode(&args).unwrap_or(125));
    }
    if !args.get(1).is_some_and(|a| a == GUARDIAN_MODE || a == "--heimdall-guardian-root") {
        return None;
    }
    let c_args: Vec<_> = args.iter().map(|a| std::ffi::CString::new(a.as_bytes()).unwrap()).collect();
    let mut pointers: Vec<_> = c_args.iter().map(|a| a.as_ptr() as *mut libc::c_char).collect();
    pointers.push(std::ptr::null_mut());
    let status = unsafe { heimdall_guardian_main(args.len() as i32, pointers.as_mut_ptr()) };
    if status == 124 {
        // EOF authority belongs to this guardian, not a numeric parent PID.
        // Reuse exactly the normal ownership boundary and retirement receipt.
        let Some(root) = args.get(2) else { return Some(125); };
        let root = std::path::PathBuf::from(root);
        let mut owner = Owner::new();
        let Ok(token) = std::env::var(TOKEN) else { return Some(125); };
        owner.token = token;
        owner.root = Identity::read(std::process::id() as i32);
        if owner.root.is_none() { return Some(125); }
        owner.registry = Some(root.clone());
        owner.cleanup_limited = std::fs::read_to_string(root.join("responsibility.status"))
            .map(|s| s.trim() != "0").unwrap_or(true)
            || std::fs::read(root.join("generation.owner")).ok()
                .and_then(|b| serde_json::from_slice::<Owner>(&b).ok())
                .is_some_and(|o| o.cleanup_limited);
        if let Err(error) = owner.cleanup() {
            // Preserve the receipt without claiming retirement if ownership
            // cannot be exhausted. Replacement continues to fail closed.
            let _ = std::fs::write(root.join("cleanup.error"), format!("{error:#}"));
            return Some(125);
        }
    }
    Some(status)
}
fn main_executable() -> Result<std::path::PathBuf> {
    std::env::current_exe().context("Heimdall main executable")
}
/// Production always reuses the app's signed main executable. No extraction.
pub(crate) fn guardian() -> Result<std::path::PathBuf> {
    guardian_for_executable(main_executable()?)
}
fn guardian_for_executable(executable: std::path::PathBuf) -> Result<std::path::PathBuf> {
    #[cfg(test_guardian_available)]
    if executable.parent().and_then(Path::file_name).is_some_and(|n| n == "deps") {
        // Only Cargo's test harness has no guardian dispatch at main. The explicit
        // override lets the lifecycle lab exercise the real candidate's dispatch.
        return Ok(std::env::var_os("HEIMDALL_TEST_GUARDIAN_EXECUTABLE").map(std::path::PathBuf::from)
            .unwrap_or_else(|| executable.parent().unwrap().parent().unwrap().join("heimdall-agent-runtime")));
    }
    Ok(executable)
}
pub(crate) fn admit(
    dir: &Path,
    owner: &mut Owner,
    guardian_pid: i32,
    durable: bool,
    admission_deadline: Option<Instant>,
) -> Result<Identity> {
    owner.registry = if durable { Some(dir.into()) } else { None };
    owner.root = Identity::read(guardian_pid);
    anyhow::ensure!(
        owner.root.is_some(),
        "process guardian died before admission"
    );
    if durable {
        owner.persist(dir)?;
    }
    // A cold signed main executable can spend seconds in macOS launch validation.
    // Preparation retains its caller's absolute deadline; Pi startup is bounded
    // independently and still checks guardian death throughout admission.
    let deadline = admission_deadline.unwrap_or_else(|| Instant::now() + Duration::from_secs(15));
    let pid = loop {
        anyhow::ensure!(
            Instant::now() < deadline,
            "owned process admission timed out"
        );
        if let Ok(text) = std::fs::read_to_string(dir.join("child.pid")) {
            if let Ok(pid) = text.trim().parse::<i32>() {
                break pid;
            }
        }
        anyhow::ensure!(
            Identity::read(guardian_pid) == owner.root,
            "process guardian died before admission"
        );
        anyhow::ensure!(
            Instant::now() < deadline,
            "process guardian admission timed out"
        );
        std::thread::sleep(Duration::from_millis(5));
    };
    let child = Identity::read(pid).context("owned child died before admission")?;
    owner.cleanup_limited = std::fs::read_to_string(dir.join("responsibility.status"))
        .map(|status| status.trim() != "0").unwrap_or(true)
        || responsible_pid(pid) != guardian_pid;
    // Record reduced coverage before releasing tool execution. Private API
    // availability must never be a condition for agent admission.
    if durable { owner.persist(dir)?; }
    Ok(child)
}
#[cfg(test)]
thread_local! { static TEST_BEFORE_RETIRE: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) }; }
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct Owner {
    pub token: String,
    pub root: Option<Identity>,
    pub registry: Option<std::path::PathBuf>,
    #[serde(default)]
    pub cleanup_limited: bool,
    /// Boot session the generation was born in; absent in older receipts.
    #[serde(default)]
    pub boot: Option<String>,
    #[serde(skip)]
    pub retired: std::sync::Arc<std::sync::atomic::AtomicBool>,
}
impl Owner {
    pub fn new() -> Self {
        Self {
            token: uuid::Uuid::new_v4().to_string(),
            root: None,
            registry: None,
            cleanup_limited: false,
            boot: Some(boot_session().unwrap_or_else(|| UNKNOWN_BOOT.into())),
            retired: Default::default(),
        }
    }
    /// A reboot killed everything a generation owned, so it is fully retired. Receipts
    /// from before boot sessions were recorded fall back to the guardian's birth well
    /// before kern.boottime (a clock step moves boottime, not birth times).
    fn from_earlier_boot(&self) -> bool {
        match &self.boot {
            Some(boot) => uuid::Uuid::parse_str(boot).is_ok() && boot_session().is_some_and(|now| now != *boot),
            None => self.root.zip(boot_sec()).is_some_and(|(root, boot)| root.sec + LEGACY_BOOT_MARGIN_SEC < boot),
        }
    }
    pub fn persist(&self, dir: &Path) -> Result<()> {
        crate::runtime::atomic_write(&dir.join("generation.owner"), &serde_json::to_vec(self)?)
    }
    pub fn cleanup(&self) -> Result<()> {
        let retired = self.retired.load(std::sync::atomic::Ordering::Acquire)
            || self
                .registry
                .as_ref()
                .is_some_and(|dir| dir.join("generation.retired").exists());
        if retired
            && self
                .root
                .is_none_or(|root| Identity::read(root.pid) != Some(root))
        {
            return Ok(());
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut known = HashSet::new();
        if let Some(root) = self.root {
            known.insert(root);
        }
        loop {
            let all = processes()?;
            let responsible_root = self.root.filter(|id| Identity::read(id.pid) == Some(*id));
            for p in &all {
                let responsible = !self.cleanup_limited && responsible_root.is_some_and(|root| {
                    responsible_pid(p.id.pid) == root.pid
                        && Identity::read(root.pid) == Some(root)
                        && Identity::read(p.id.pid) == Some(p.id)
                });
                if responsible {
                    known.insert(p.id);
                }
                if carries(p.id.pid, &self.token) && Identity::read(p.id.pid) == Some(p.id) {
                    known.insert(p.id);
                }
            }
            // Also catch children which scrubbed their environment while their
            // parent is still visible. A reparented scrubber is a documented limit.
            loop {
                let before = known.len();
                for p in &all {
                    if all.iter().any(|parent| {
                        parent.id.pid == p.parent
                            && known.contains(&parent.id)
                            && Identity::read(parent.id.pid) == Some(parent.id)
                            && Identity::read(p.id.pid) == Some(p.id)
                    }) {
                        known.insert(p.id);
                    }
                }
                if before == known.len() {
                    break;
                }
            }
            let alive: Vec<_> = all
                .iter()
                .filter(|p| known.contains(&p.id) && Some(p.id) != self.root)
                .map(|p| p.id)
                .collect();
            if alive.is_empty() {
                let retired = self.retired.load(std::sync::atomic::Ordering::Acquire)
                    || self
                        .registry
                        .as_ref()
                        .is_some_and(|dir| dir.join("generation.retired").exists());
                #[cfg(test)]
                TEST_BEFORE_RETIRE.with(|hook| { if let Some(hook)=hook.borrow_mut().take() { hook(); } });
                // A guardian from before the last boot died with everything it
                // owned: a reboot is a complete retirement (crash).
                let before_boot = self.from_earlier_boot();
                anyhow::ensure!(self.root.is_none() || self.root.is_some_and(|root|Identity::read(root.pid)==Some(root)) || retired || before_boot,
                    "process guardian died before retirement; escaped ownership cannot be verified, replacement blocked");
                if !retired {
                    // Commit retirement while the guardian is still alive and
                    // every descendant is dead. A crash in this final gap is safe.
                    if let Some(dir) = &self.registry {
                        crate::runtime::atomic_write(&dir.join("generation.retired"), b"")?;
                    }
                    self.retired
                        .store(true, std::sync::atomic::Ordering::Release);
                }
                if let Some(root) = self.root {
                    if root.pid != std::process::id() as i32 { root.kill(); }
                }
                return Ok(());
            }
            // Freeze all current owners before killing so they cannot keep
            // forking during cleanup. Birth identity is checked for STOP too.
            if let Some(root) = responsible_root {
                if root.pid != std::process::id() as i32 && Identity::read(root.pid) == Some(root) {
                    unsafe {
                        libc::kill(root.pid, libc::SIGSTOP);
                    }
                }
            }
            for id in &alive {
                if Identity::read(id.pid) == Some(*id) {
                    unsafe {
                        libc::kill(id.pid, libc::SIGSTOP);
                    }
                }
            }
            for id in alive {
                id.kill();
            }
            anyhow::ensure!(Instant::now() < deadline, "owned generation did not retire");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
pub(crate) fn cleanup_registry(dir: &Path) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir)? {
        let p = entry?.path();
        if p.is_dir() {
            cleanup_registry(&p)?;
            std::fs::remove_dir_all(&p)?;
        } else if p.file_name().is_some_and(|n| n == "generation.owner") {
            let mut owner: Owner = serde_json::from_slice(&std::fs::read(&p)?)?;
            anyhow::ensure!(
                uuid::Uuid::parse_str(&owner.token).is_ok(),
                "invalid process owner token"
            );
            // Use the registry being read, not a stale absolute path supplied
            // in a recovered receipt.
            owner.registry = Some(dir.into());
            owner.cleanup().context("retire previous tool generation")?;
            // Preserve generation marker until registry removal. Concurrent Off,
            // shutdown and crash paths can safely repeat a token-based scan.
        }
    }
    // Removal is done by replacement launch only, after cleanup succeeded. It
    // never gives old destructors authority over the replacement's unique token.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn production_guardian_path_is_exactly_current_main_executable() {
        let app = std::path::PathBuf::from("/lab/Heimdall.app/Contents/MacOS/Heimdall");
        assert_eq!(guardian_for_executable(app.clone()).unwrap(), app);
        let cli = std::path::PathBuf::from("/lab/debug/heimdall");
        assert_eq!(guardian_for_executable(cli.clone()).unwrap(), cli);
        // Selection's only exception is Cargo's debug test fixture, never a bundle.
        let source = include_str!("../../app/src/main.rs");
        let main = source.split("fn main() -> Result<()> {").nth(1).unwrap();
        assert!(main.trim_start().starts_with("if let Some(status) = agent::guardian_mode()"));
    }

    #[test]
    fn kernel_environment_finds_only_exact_token_and_birth() {
        let mut owner = Owner::new();
        let mut child = std::process::Command::new("/usr/bin/python3")
            .args(["-c", "import time;time.sleep(60)"])
            .env(TOKEN, &owner.token)
            .spawn()
            .unwrap();
        owner.root = Identity::read(child.id() as i32);
        std::thread::sleep(Duration::from_millis(20));
        let found = carries(child.id() as i32, &owner.token);
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(found, "KERN_PROCARGS2 did not find inherited token");
    }
}

/// CLOEXEC on both ends in Heimdall; clear it only in the guardian fork.
/// The child closes its endpoint before exec so detached tools cannot keep the
/// supervisor alive. EOF is lifetime authority without any PID-reuse assumption.
pub(crate) fn supervisor_channel() -> Result<(std::os::unix::net::UnixStream, std::os::unix::net::UnixStream)> {
    Ok(std::os::unix::net::UnixStream::pair()?)
}
pub(crate) unsafe fn inherit_fd(fd: i32) -> std::io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Relaunch waits on an immutable kernel birth, never on kill(pid, 0).
pub fn spawn_relaunch(pid: u32, launch: &[String]) -> Result<std::process::Child> {
    anyhow::ensure!(!launch.is_empty(), "nothing to launch");
    let identity = Identity::read(pid as i32);
    spawn_relaunch_identity(identity, launch)
}
fn spawn_relaunch_identity(identity: Option<Identity>, launch: &[String]) -> Result<std::process::Child> {
    use std::os::unix::process::CommandExt;
    Ok(std::process::Command::new(guardian()?)
        .arg("--heimdall-relaunch").arg(serde_json::to_string(&identity)?).args(launch)
        .stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null()).process_group(0).spawn()?)
}
fn relaunch_mode(args: &[std::ffi::OsString]) -> Option<i32> {
    use std::os::unix::process::CommandExt;
    let identity: Option<Identity> = serde_json::from_str(args.get(2)?.to_str()?).ok()?;
    if let Some(id) = identity {
        while Identity::read(id.pid) == Some(id) { std::thread::sleep(Duration::from_millis(200)); }
    }
    let program = args.get(3)?;
    let _ = std::process::Command::new(program).args(&args[4..]).exec();
    Some(125)
}

#[cfg(test)]
mod relaunch_regressions {
    use super::*;
    #[test]
    fn stale_relaunch_birth_does_not_wait_for_or_signal_unrelated_reused_pid() {
        let d=tempfile::tempdir().unwrap();let marker=d.path().join("launched");
        let mut old=std::process::Command::new("/bin/sleep").arg("60").spawn().unwrap();
        let mut stale=Identity::read(old.id() as i32).unwrap();stale.invalidate_for_test();
        let mut helper=spawn_relaunch_identity(Some(stale),&["/usr/bin/touch".into(),marker.to_string_lossy().into_owned()]).unwrap();
        let deadline=Instant::now()+Duration::from_secs(3);
        while !marker.exists() {assert!(Instant::now()<deadline);std::thread::sleep(Duration::from_millis(20));}
        assert!(old.try_wait().unwrap().is_none());old.kill().unwrap();old.wait().unwrap();helper.wait().unwrap();
    }
}

#[cfg(test)]
mod final_identity_regressions {
    use super::*;
    #[test]
    fn guardian_from_before_boot_retires_its_generation() {
        let d = tempfile::tempdir().unwrap();
        let mut owner = Owner::new();
        owner.root = Some(Identity { pid: i32::MAX, sec: boot_sec().unwrap() + 1, usec: 0 });
        owner.boot = Some(uuid::Uuid::new_v4().to_string().to_uppercase());
        owner.registry = Some(d.path().into());
        owner.persist(d.path()).unwrap();
        cleanup_registry(d.path()).unwrap();
        assert!(d.path().join("generation.retired").exists());
    }
    #[test]
    fn receipt_without_boot_session_retires_when_guardian_predates_boot() {
        let d = tempfile::tempdir().unwrap();
        let mut owner = Owner::new();
        owner.root = Some(Identity { pid: i32::MAX, sec: boot_sec().unwrap() - LEGACY_BOOT_MARGIN_SEC - 1, usec: 0 });
        owner.boot = None;
        owner.registry = Some(d.path().into());
        owner.persist(d.path()).unwrap();
        cleanup_registry(d.path()).unwrap();
        assert!(d.path().join("generation.retired").exists());
    }
    #[test]
    fn unknown_boot_session_or_a_legacy_guardian_near_boot_stays_blocked() {
        // A failed boot-session read, or an old receipt whose guardian a clock step
        // could have put just before boottime: neither is proof of a reboot.
        for (boot, sec) in [(Some(UNKNOWN_BOOT.to_owned()), 1), (None, boot_sec().unwrap() - 1)] {
            let d = tempfile::tempdir().unwrap();
            let mut owner = Owner::new();
            owner.root = Some(Identity { pid: i32::MAX, sec, usec: 0 });
            owner.boot = boot;
            owner.registry = Some(d.path().into());
            owner.persist(d.path()).unwrap();
            let error = cleanup_registry(d.path()).unwrap_err();
            assert!(format!("{error:#}").contains("guardian died before retirement"));
            assert!(!d.path().join("generation.retired").exists());
        }
    }
    #[test]
    fn dead_guardian_from_this_boot_stays_blocked_even_if_clock_stepped() {
        // Wall time says "before boot", the boot session says this boot: blocked.
        let d = tempfile::tempdir().unwrap();
        let mut owner = Owner::new();
        assert!(owner.boot.as_deref().is_some_and(|b| b != UNKNOWN_BOOT), "kern.bootsessionuuid unreadable");
        owner.root = Some(Identity { pid: i32::MAX, sec: boot_sec().unwrap() - 1, usec: 0 });
        owner.registry = Some(d.path().into());
        owner.persist(d.path()).unwrap();
        let error = cleanup_registry(d.path()).unwrap_err();
        assert!(format!("{error:#}").contains("guardian died before retirement"));
        assert!(!d.path().join("generation.retired").exists());
    }
    #[test]
    fn guardian_death_during_final_scan_cannot_commit_false_retirement() {
        let d=tempfile::tempdir().unwrap();let mut owner=Owner::new();
        let mut root=std::process::Command::new("/usr/bin/python3").args(["-c","import time;time.sleep(60)"]).env(TOKEN,&owner.token).spawn().unwrap();
        owner.root=Identity::read(root.id() as i32);owner.registry=Some(d.path().into());owner.persist(d.path()).unwrap();
        let id=owner.root.unwrap();
        TEST_BEFORE_RETIRE.with(|hook|*hook.borrow_mut()=Some(Box::new(move || {
            id.kill();root.wait().unwrap();
        })));
        let result=owner.cleanup();
        assert!(result.unwrap_err().to_string().contains("guardian died before retirement"));
        assert!(!d.path().join("generation.retired").exists());
    }
}
