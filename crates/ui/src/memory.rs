//! Heimdall's real memory: its own process and every process under it (Drama 3's sidecar, the embedding worker,
//! the islands, the agent and its tools), each read as the physical footprint Activity Monitor's Memory column shows.
//! Metal buffers are part of a process's footprint (its "IOAccelerator" pages), so a model's GPU memory is counted
//! once, in the process that loaded it.
//!
//! Activity Monitor lists each helper as its own row and never adds it to Heimdall's. A process can hand a GPU
//! buffer's accounting to another only with Apple's restricted memory-transfer entitlement (measured 26 Oct:
//! `setOwnerWithIdentity:` answers "no access" without it, and an ad-hoc signature claiming it is killed at launch).
//! So this total is the one place where everything Heimdall holds is added up.

use std::path::Path;

/// The part Heimdall's own process holds: what runs inside it rather than in a helper (listening and transcription,
/// the decider, Supertonic, screen and audio capture, the archive, the menu; docs/resilience.md). Named for that, not "Heimdall", so it no longer reads
/// as the total of the parts beside it ("not clear whether Heimdall is something separate or a
/// combination of all of those"); the Memory row's value and the submenu's Total row are the sum.
pub const CORE_LABEL: &str = "Listening & Decider";
/// Its tooltip: everything the part holds.
pub const CORE_DETAIL: &str = "Heimdall's own processes: listening and transcription (Parakeet, Smart Turn), the decider, Supertonic's voice, screen and audio capture, the archive and the menu";

/// One process as read: its parent, executable path and physical footprint in bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Process {
    pub pid: u32,
    pub ppid: u32,
    pub path: String,
    pub footprint: u64,
}

/// What one part of Heimdall holds (its processes summed).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemoryPart {
    pub label: String,
    pub icon: &'static str,
    pub bytes: u64,
}

/// Heimdall's total and its parts, largest first.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MemoryReport {
    pub total: u64,
    pub parts: Vec<MemoryPart>,
}

/// `root` and every process descended from it, each with the part it belongs to (`classify`): the memory row's
/// grouping, which the app's GPU readout shares (`crates/app/src/resource_watch.rs`). `own_exe` is Heimdall's
/// executable: another process running it (the agent's guardian) counts as Heimdall. Processes outside the tree are
/// left out.
pub fn members<'a>(root: u32, own_exe: &str, processes: &'a [Process]) -> Vec<(&'a Process, &'static str, &'static str)> {
    let mut pids = vec![root];
    let mut i = 0;
    while i < pids.len() {
        let parent = pids[i];
        for child in processes.iter().filter(|p| p.ppid == parent) {
            if !pids.contains(&child.pid) { pids.push(child.pid) }
        }
        i += 1;
    }
    processes.iter().filter(|p| pids.contains(&p.pid)).map(|process| {
        let (label, icon) = if process.pid == root { (CORE_LABEL, "waveform") } else { classify(&process.path, own_exe) };
        (process, label, icon)
    }).collect()
}

impl MemoryReport {
    /// `root` and every process descended from it, grouped by what they are (`members`).
    pub fn of_tree(root: u32, own_exe: &str, processes: &[Process]) -> Self {
        let mut parts: Vec<MemoryPart> = vec![];
        for (process, label, icon) in members(root, own_exe, processes) {
            match parts.iter_mut().find(|p| p.label == label) {
                Some(part) => part.bytes += process.footprint,
                None => parts.push(MemoryPart { label: label.into(), icon, bytes: process.footprint }),
            }
        }
        parts.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.label.cmp(&b.label)));
        MemoryReport { total: parts.iter().map(|p| p.bytes).sum(), parts }
    }
}

/// The part a helper belongs to, by its executable path, and its symbol.
fn classify(path: &str, own_exe: &str) -> (&'static str, &'static str) {
    let name = Path::new(path).file_name().and_then(|n| n.to_str()).unwrap_or("");
    if path == own_exe || name == "Heimdall" {
        (CORE_LABEL, "waveform")
    } else if path.contains("/drama3/") || name == DRAMA3_PROCESS {
        ("Drama 3", "text.bubble")
    } else if name.contains("embedding-worker") {
        ("Search Index", "magnifyingglass")
    } else if name == "FloatingIslands" || name == "islands" {
        ("Islands", "rectangle.on.rectangle")
    } else if name.contains("speakers") {
        ("Speakers", "person.2")
    } else if path.contains("/splash/") {
        // The built-in engine (Agent ▸ Model ▸ Local), a child of Heimdall's.
        ("Local Model", "cpu")
    } else if path.contains("/agent-runtime/") {
        ("Agent", "terminal")
    } else {
        ("Other", "ellipsis.circle")
    }
}

/// The name Drama 3's sidecar runs under, so Activity Monitor lists it as Heimdall's and not as "python3"
/// (the bundle adds this name beside `runtime/bin/python3`; crates/speak starts it when present).
pub const DRAMA3_PROCESS: &str = "Heimdall Drama 3";

/// Memory as Activity Monitor writes it (binary units, labelled GB/MB): "27.6 GB", "812 MB".
pub fn memory_label(bytes: u64) -> String {
    let mb = bytes as f64 / (1u64 << 20) as f64;
    if mb >= 1024.0 {
        format!("{:.1} GB", mb / 1024.0)
    } else {
        format!("{mb:.0} MB")
    }
}

/// Reads this process's tree now: its own pid, every descendant's footprint and path. A process that ended
/// between listing and reading is left out. About a millisecond for Heimdall's dozen processes.
#[cfg(target_os = "macos")]
pub fn current() -> MemoryReport {
    let (root, own_exe, processes) = tree();
    MemoryReport::of_tree(root, &own_exe, &processes)
}

/// This process's tree as read now: its pid, Heimdall's executable path, and every process in it.
#[cfg(target_os = "macos")]
pub fn tree() -> (u32, String, Vec<Process>) {
    let root = std::process::id();
    let own_exe = std::env::current_exe().ok().and_then(|p| p.canonicalize().ok()).map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
    let mut processes = vec![];
    let mut queue = vec![(root, 0u32)];
    while let Some((pid, ppid)) = queue.pop() {
        if processes.iter().any(|p: &Process| p.pid == pid) { continue; }
        let Some(footprint) = footprint(pid) else { continue };
        processes.push(Process { pid, ppid, path: path(pid), footprint });
        queue.extend(children(pid).into_iter().map(|c| (c, pid)));
    }
    (root, own_exe, processes)
}

/// The largest processes on the Mac this user can read, by physical footprint, largest first: (name, bytes).
/// Processes of other users (root daemons) are not readable and are left out.
#[cfg(target_os = "macos")]
pub fn largest(n: usize) -> Vec<(String, u64)> {
    let mut pids = vec![0i32; 8192];
    let count = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), (pids.len() * 4) as i32) };
    if count <= 0 { return vec![]; }
    pids.truncate((count as usize).min(pids.len()));
    let mut all: Vec<(String, u64)> = pids.into_iter().filter(|p| *p > 0).filter_map(|pid| {
        let bytes = footprint(pid as u32)?;
        let path = path(pid as u32);
        let name = Path::new(&path).file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
        (!name.is_empty()).then_some((name, bytes))
    }).collect();
    all.sort_by(|a, b| b.1.cmp(&a.1));
    all.truncate(n);
    all
}

#[cfg(target_os = "macos")]
fn footprint(pid: u32) -> Option<u64> {
    let mut info: libc::rusage_info_v4 = unsafe { std::mem::zeroed() };
    let code = unsafe { libc::proc_pid_rusage(pid as i32, libc::RUSAGE_INFO_V4, &mut info as *mut _ as *mut libc::rusage_info_t) };
    (code == 0).then_some(info.ri_phys_footprint)
}

#[cfg(target_os = "macos")]
fn path(pid: u32) -> String {
    let mut buffer = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    let n = unsafe { libc::proc_pidpath(pid as i32, buffer.as_mut_ptr().cast(), buffer.len() as u32) };
    if n <= 0 { return String::new(); }
    String::from_utf8_lossy(&buffer[..n as usize]).into_owned()
}

#[cfg(target_os = "macos")]
fn children(pid: u32) -> Vec<u32> {
    let mut buffer = vec![0i32; 1024];
    let n = unsafe { libc::proc_listchildpids(pid as i32, buffer.as_mut_ptr().cast(), (buffer.len() * 4) as i32) };
    if n <= 0 { return vec![]; }
    buffer.truncate((n as usize).min(1024));
    buffer.into_iter().filter(|p| *p > 0).map(|p| p as u32).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXE: &str = "/Users/t/Applications/Heimdall.app/Contents/MacOS/Heimdall";
    const GB: u64 = 1 << 30;
    const MB: u64 = 1 << 20;

    fn p(pid: u32, ppid: u32, path: &str, footprint: u64) -> Process {
        Process { pid, ppid, path: path.into(), footprint }
    }

    /// The 25 Oct evening, as measured 26 Oct: Heimdall 2.8 GB, Drama 3 24 GB as "python3", the embedding worker
    /// 3.7 GB, the islands, the agent's guardian. Activity Monitor's Heimdall row showed only the first.
    fn evening() -> Vec<Process> {
        vec![
            p(100, 1, EXE, 2848 * MB),
            p(101, 100, "/Users/t/Applications/Heimdall.app/Contents/Helpers/FloatingIslands.app/Contents/MacOS/FloatingIslands", 46 * MB),
            p(102, 100, "/Users/t/Applications/Heimdall.app/Contents/Resources/models/drama3/runtime/bin/python3", 24566 * MB),
            p(103, 100, EXE, 4 * MB),
            p(104, 100, "/Users/t/Applications/Heimdall.app/Contents/Resources/models/embeddinggemma2/native/heimdall-embedding-worker", 3799 * MB),
            // Arsenal and an unrelated python: not Heimdall's.
            p(200, 1, "/Users/t/Applications/Arsenal.app/Contents/MacOS/Arsenal", 150 * MB),
            p(201, 200, "/usr/bin/python3", 9 * GB),
        ]
    }

    #[test]
    fn total_adds_every_helper_and_nothing_outside_the_tree() {
        let report = MemoryReport::of_tree(100, EXE, &evening());
        assert_eq!(report.total, (2848 + 46 + 24566 + 4 + 3799) * MB);
        let labels: Vec<_> = report.parts.iter().map(|p| (p.label.as_str(), p.bytes)).collect();
        assert_eq!(labels, vec![("Drama 3", 24566 * MB), ("Search Index", 3799 * MB), (CORE_LABEL, 2852 * MB), ("Islands", 46 * MB)]);
        assert_eq!(memory_label(report.total), "30.5 GB");
    }

    #[test]
    fn grandchildren_count_and_the_renamed_sidecar_is_drama3() {
        let processes = vec![
            p(100, 1, EXE, GB),
            p(103, 100, EXE, 4 * MB),
            // An agent tool under the guardian, and the sidecar under its new name.
            p(300, 103, "/bin/zsh", 2 * MB),
            p(301, 300, "/opt/homebrew/bin/cargo", 500 * MB),
            p(102, 100, &format!("/x/models/drama3/runtime/bin/{DRAMA3_PROCESS}"), 20 * GB),
            p(105, 100, "/x/Resources/agent-runtime/node/bin/node", 300 * MB),
            p(106, 100, "/opt/homebrew/Cellar/splash/1.3.1/libexec/python/bin/python3", 18 * GB),
        ];
        let report = MemoryReport::of_tree(100, EXE, &processes);
        assert_eq!(report.total, GB + 4 * MB + 2 * MB + 500 * MB + 20 * GB + 300 * MB + 18 * GB);
        assert_eq!(report.parts[0].label, "Drama 3");
        assert_eq!(report.parts[1].label, "Local Model");
        let find = |l: &str| report.parts.iter().find(|p| p.label == l).map(|p| p.bytes);
        assert_eq!(find("Drama 3"), Some(20 * GB));
        assert_eq!(find("Other"), Some(502 * MB));
        assert_eq!(find("Agent"), Some(300 * MB));
        assert_eq!(find(CORE_LABEL), Some(GB + 4 * MB));
    }

    #[test]
    fn a_cycle_or_a_lone_root_never_loops() {
        let processes = vec![p(100, 101, EXE, MB), p(101, 100, "/bin/sh", MB)];
        assert_eq!(MemoryReport::of_tree(100, EXE, &processes).total, 2 * MB);
        assert_eq!(MemoryReport::of_tree(100, EXE, &[p(100, 1, EXE, 7 * MB)]).parts.len(), 1);
    }

    #[test]
    fn labels_match_activity_monitor_units() {
        assert_eq!(memory_label(812 * MB), "812 MB");
        assert_eq!(memory_label(24566 * MB), "24.0 GB");
        assert_eq!(memory_label(1024 * MB), "1.0 GB");
    }

    /// The Mac's largest processes: this user's are readable, largest first, this test process among them when asked
    /// for all.
    #[cfg(target_os = "macos")]
    #[test]
    fn largest_reads_this_users_processes_largest_first() {
        let top = largest(3);
        assert!(!top.is_empty() && top.len() <= 3, "{top:?}");
        assert!(top.windows(2).all(|w| w[0].1 >= w[1].1), "{top:?}");
        let all = largest(usize::MAX);
        let me = std::env::current_exe().unwrap().file_name().unwrap().to_string_lossy().into_owned();
        assert!(all.iter().any(|(name, _)| *name == me), "this process is missing");
    }

    /// The real reading on a tree this test builds: a child holding 64 MB of touched memory is counted in this
    /// process's total, under "Other".
    #[cfg(target_os = "macos")]
    #[test]
    fn current_counts_a_real_child() {
        let mut child = std::process::Command::new("/usr/bin/perl")
            .args(["-e", "my $x = 'a' x (64*1024*1024); print \"ready\\n\"; $| = 1; sleep 30"])
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        use std::io::BufRead as _;
        let mut line = String::new();
        std::io::BufReader::new(child.stdout.take().unwrap()).read_line(&mut line).unwrap();
        let after = current();
        child.kill().ok();
        child.wait().ok();
        // Only the child's own share: other tests in this process allocate and free meanwhile.
        let other = after.parts.iter().find(|p| p.label == "Other").map_or(0, |p| p.bytes);
        assert!(other >= 64 * MB, "child not counted: {after:?}");
        assert!(after.total >= other + after.parts.iter().find(|p| p.label == CORE_LABEL).unwrap().bytes);
    }

    #[test]
    fn heimdalls_own_process_is_named_for_what_it_holds() {
        // a "Heimdall" row beside the others read as the total.
        let report = MemoryReport::of_tree(100, EXE, &evening());
        assert!(report.parts.iter().all(|p| p.label != "Heimdall"));
        assert!(report.parts.iter().any(|p| p.label == CORE_LABEL));
        assert_eq!(report.total, report.parts.iter().map(|p| p.bytes).sum::<u64>());
    }

}
