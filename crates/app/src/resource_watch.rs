//! Heimdall's resource watch: swap and GPU time, read once a minute on its own thread.
//!
//! - Swap (bug note 24 Oct 20:34: 16 GB of swap while several workers ran): the first time swap reaches 2 GiB the
//!   main agent gets one notice (a wake, `NoticeKind::Event`) naming the largest processes, so it can hold or stop the
//!   heavy jobs it started. It warns again only after swap fell under 1.5 GiB and at most once an hour.
//! - GPU (bug note 26 Oct 17:00: the GPU at about 57% with nothing heavy running): the GPU time each process of
//!   Heimdall's tree used, grouped by the Memory row's parts (`ui::memory::members`: Drama 3, the Search Index's
//!   embedding worker, Speakers, the local engine, the islands, and Heimdall's own process, "Listening & Decider", which holds Parakeet, the
//!   decider, Smart Turn and the Supertonic voice together), beside the device's utilisation and the busiest other
//!   processes. One `gpu:` line per 10 minutes when the GPU was busy. The source is the IORegistry's per-client
//!   `accumulatedGPUTime` (what Activity Monitor's GPU Time reads), through `/usr/sbin/ioreg`, about 25 ms a read.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

const GIB: u64 = 1 << 30;
/// Swap at or above this warns the agent.
pub const SWAP_WARN: u64 = 2 * GIB;
/// Swap must fall under this before another warning.
pub const SWAP_REARM: u64 = 3 * GIB / 2;
/// At most one swap warning an hour.
pub const SWAP_EVERY: Duration = Duration::from_secs(3600);
/// How often the watch reads.
const TICK: Duration = Duration::from_secs(60);
/// The GPU line's window.
const GPU_WINDOW: Duration = Duration::from_secs(600);
/// A window is logged when the device averaged at least this, or Heimdall's processes used at least `HEIMDALL_BUSY`.
const DEVICE_BUSY: f64 = 25.0;
const HEIMDALL_BUSY: f64 = 5.0;

/// Whether swap just crossed the warning line: once per crossing, re-armed under `SWAP_REARM`, rate-limited.
#[derive(Debug)]
pub struct SwapWatch {
    armed: bool,
    last: Option<Instant>,
}

impl Default for SwapWatch {
    fn default() -> Self { Self { armed: true, last: None } }
}

impl SwapWatch {
    /// One reading of swap in use. True when the agent should be warned now. A crossing within an hour of the last
    /// warning is used up silently, so a swap level hovering at the line never repeats the warning.
    pub fn step(&mut self, used: u64, now: Instant) -> bool {
        if used < SWAP_REARM {
            self.armed = true;
            return false;
        }
        if used < SWAP_WARN || !self.armed {
            return false;
        }
        self.armed = false;
        if self.last.is_some_and(|t| now.duration_since(t) < SWAP_EVERY) {
            return false;
        }
        self.last = Some(now);
        true
    }
}

/// "2.3 GB", Activity Monitor's units (binary, labelled GB).
fn gb(bytes: u64) -> String {
    ui::memory::memory_label(bytes)
}

/// The agent's swap warning: the level, Heimdall's own total, and the largest processes.
pub fn swap_notice(used: u64, heimdall: u64, largest: &[(String, u64)]) -> String {
    let top = largest.iter().map(|(name, bytes)| format!("{name} {}", gb(*bytes))).collect::<Vec<_>>().join(", ");
    format!(
        "Memory warning: macOS swap reached {} (the warning line is {}). Heimdall and its helpers hold {}. Largest processes now: {top}. \
         Swap growing means the Mac is out of memory: hold or stop heavy jobs you started (model loads, parallel builds) \
         before starting more, and tell User if it keeps growing.",
        gb(used), gb(SWAP_WARN), gb(heimdall)
    )
}

/// Swap in use now (`sysctl vm.swapusage`).
#[cfg(target_os = "macos")]
pub fn swap_used() -> Option<u64> {
    let mut usage: libc::xsw_usage = unsafe { std::mem::zeroed() };
    let mut size = std::mem::size_of::<libc::xsw_usage>();
    let code = unsafe { libc::sysctlbyname(c"vm.swapusage".as_ptr(), (&mut usage as *mut libc::xsw_usage).cast(), &mut size, std::ptr::null_mut(), 0) };
    (code == 0).then_some(usage.xsu_used)
}

/// One GPU client's process as the IORegistry names it: (pid, name), and its GPU time so far in nanoseconds.
pub type GpuTimes = BTreeMap<u32, (String, u64)>;

/// Every process's GPU time from `ioreg -r -c AGXDeviceUserClient -w0 -l`: each user client's `IOUserClientCreator`
/// ("pid 406, WindowServer") and the `accumulatedGPUTime` of its `AppUsage` entries, summed per process.
pub fn parse_gpu_clients(text: &str) -> GpuTimes {
    let mut out = GpuTimes::new();
    for block in text.split("+-o ").skip(1) {
        let Some(creator) = block.split("\"IOUserClientCreator\" = \"pid ").nth(1) else { continue };
        let Some((pid, rest)) = creator.split_once(", ") else { continue };
        let Ok(pid) = pid.trim().parse::<u32>() else { continue };
        let name = rest.split('"').next().unwrap_or("").to_string();
        let ns: u64 = block.split("\"accumulatedGPUTime\"=").skip(1)
            .filter_map(|s| s.split(|c: char| !c.is_ascii_digit()).next()?.parse::<u64>().ok())
            .sum();
        let entry = out.entry(pid).or_insert((name, 0));
        entry.1 += ns;
    }
    out
}

/// The GPU's own "Device Utilization %" from `ioreg -r -d 1 -c IOAccelerator -w0 -l`.
pub fn parse_utilization(text: &str) -> Option<f64> {
    let rest = text.split("\"Device Utilization %\"=").nth(1)?;
    rest.split(|c: char| !c.is_ascii_digit()).next()?.parse::<f64>().ok()
}

#[cfg(target_os = "macos")]
fn ioreg(args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("/usr/sbin/ioreg").args(args).stderr(std::process::Stdio::null()).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// One window of GPU readings: the times at its start, and the device's utilisation once per tick.
#[derive(Debug)]
pub struct GpuWindow {
    started: Instant,
    start: GpuTimes,
    utilization: Vec<f64>,
}

impl GpuWindow {
    pub fn new(at: Instant, times: GpuTimes) -> Self { Self { started: at, start: times, utilization: vec![] } }

    pub fn sample(&mut self, utilization: Option<f64>) {
        if let Some(u) = utilization { self.utilization.push(u); }
    }

    pub fn due(&self, now: Instant) -> bool { now.duration_since(self.started) >= GPU_WINDOW }

    /// The window's `gpu:` line, or None when the GPU was quiet. `part` names the Heimdall part a pid belongs to
    /// (None: not Heimdall's). A process that started inside the window counts from zero; one that ended is gone.
    pub fn line(&self, now: Instant, end: &GpuTimes, part: impl Fn(u32) -> Option<&'static str>) -> Option<String> {
        let window = now.duration_since(self.started).as_nanos().max(1) as f64;
        let pct = |ns: u64| ns as f64 / window * 100.0;
        let mut parts: BTreeMap<&'static str, u64> = BTreeMap::new();
        let mut others: Vec<(String, u64)> = vec![];
        for (pid, (name, ns)) in end {
            let before = self.start.get(pid).filter(|(n, _)| n == name).map_or(0, |(_, b)| *b);
            let used = ns.saturating_sub(before);
            match part(*pid) {
                Some(label) => *parts.entry(label).or_default() += used,
                None if used > 0 => others.push((name.clone(), used)),
                None => {}
            }
        }
        let heimdall: u64 = parts.values().sum();
        let device = (!self.utilization.is_empty()).then(|| self.utilization.iter().sum::<f64>() / self.utilization.len() as f64);
        if device.unwrap_or(0.0) < DEVICE_BUSY && pct(heimdall) < HEIMDALL_BUSY {
            return None;
        }
        let mut parts: Vec<_> = parts.into_iter().filter(|(_, ns)| *ns > 0).collect();
        parts.sort_by(|a, b| b.1.cmp(&a.1));
        others.sort_by(|a, b| b.1.cmp(&a.1));
        let list = |v: &[(String, u64)]| v.iter().take(3).map(|(n, ns)| format!("{n} {:.1}%", pct(*ns))).collect::<Vec<_>>().join(", ");
        let mine = parts.iter().map(|(n, ns)| (n.to_string(), *ns)).collect::<Vec<_>>();
        Some(format!(
            "last {} min: device {}; Heimdall {:.1}% of GPU time ({}); elsewhere: {}",
            (window / 60e9).round(),
            device.map_or("unread".to_string(), |d| format!("{d:.0}% busy")),
            pct(heimdall),
            if mine.is_empty() { "none".into() } else { list(&mine) },
            if others.is_empty() { "none".into() } else { list(&others) },
        ))
    }
}

/// Starts the watch on its own thread for the app's life. The agent is told about swap through `tools.notice`; with
/// Agent off the warning is only logged.
#[cfg(target_os = "macos")]
pub fn start(tools: std::sync::Arc<crate::bridge::Tools>) {
    let spawned = std::thread::Builder::new().name("heimdall-resource-watch".into()).spawn(move || {
        let mut swap = SwapWatch::default();
        let read_gpu = || ioreg(&["-r", "-c", "AGXDeviceUserClient", "-w0", "-l"]).map(|t| parse_gpu_clients(&t));
        let mut window = read_gpu().map(|t| GpuWindow::new(Instant::now(), t));
        loop {
            std::thread::sleep(TICK);
            let now = Instant::now();
            if let Some(used) = swap_used() {
                if swap.step(used, now) {
                    let heimdall = ui::memory::current().total;
                    let text = swap_notice(used, heimdall, &ui::memory::largest(3));
                    crate::diagnostics::line("memory", &format!("swap reached {}; the agent is told", gb(used)));
                    let told = tools.agent_enabled.permit().and_then(|permit| tools.notice(format!("memory-swap:{}", agent::now_ms()), text, &permit));
                    if let Err(e) = told { crate::diagnostics::line("memory", &format!("swap warning not delivered: {e:#}")); }
                }
            }
            let Some(w) = window.as_mut() else { window = read_gpu().map(|t| GpuWindow::new(now, t)); continue };
            w.sample(ioreg(&["-r", "-d", "1", "-c", "IOAccelerator", "-w0", "-l"]).and_then(|t| parse_utilization(&t)));
            if !w.due(now) { continue; }
            let Some(end) = read_gpu() else { continue };
            let (root, own_exe, processes) = ui::memory::tree();
            let members: BTreeMap<u32, &'static str> = ui::memory::members(root, &own_exe, &processes).into_iter().map(|(p, label, _)| (p.pid, label)).collect();
            if let Some(line) = w.line(now, &end, |pid| members.get(&pid).copied()) {
                crate::diagnostics::line("gpu", &line);
            }
            window = Some(GpuWindow::new(now, end));
        }
    });
    if let Err(e) = spawned { eprintln!("Resource watch did not start: {e}"); }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MB: u64 = 1 << 20;

    #[test]
    fn swap_warns_once_per_crossing_and_at_most_hourly() {
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let mut w = SwapWatch::default();
        // Today's 0.8 GB: nothing.
        assert!(!w.step(800 * MB, at(0)));
        // Crossing 2 GB warns once; staying above, or hovering just under and over the line, never repeats it.
        assert!(w.step(2100 * MB, at(60)));
        assert!(!w.step(4 * GIB, at(120)));
        assert!(!w.step(1900 * MB, at(180)));
        assert!(!w.step(2100 * MB, at(240)));
        // Under 1.5 GB re-arms; a new crossing within the hour is used up silently.
        assert!(!w.step(GIB, at(300)));
        assert!(!w.step(3 * GIB, at(360)));
        // Back down and up again after the hour: warned again.
        assert!(!w.step(GIB, at(3000)));
        assert!(w.step(2 * GIB, at(3700)));
    }

    #[test]
    fn swap_already_high_at_start_warns_once() {
        let mut w = SwapWatch::default();
        let now = Instant::now();
        assert!(w.step(16 * GIB, now));
        assert!(!w.step(16 * GIB, now + Duration::from_secs(7200)), "still the same crossing two hours later");
    }

    #[test]
    fn the_swap_notice_names_the_level_heimdall_and_the_largest_processes_and_wakes_the_agent() {
        let text = swap_notice(2150 * MB, 30 * GIB, &[("python3".into(), 24 * GIB), ("Heimdall Drama 3".into(), 23 * GIB), ("rustc".into(), 3 * GIB)]);
        assert!(text.starts_with("Memory warning: macOS swap reached 2.1 GB (the warning line is 2.0 GB)."), "{text}");
        assert!(text.contains("Heimdall and its helpers hold 30.0 GB"), "{text}");
        assert!(text.contains("Largest processes now: python3 24.0 GB, Heimdall Drama 3 23.0 GB, rustc 3.0 GB."), "{text}");
        // Not a context-only id: the agent gets a turn to act on it.
        assert_eq!(agent::notice::NoticeKind::classify("memory-swap:1793026168166", &text), agent::notice::NoticeKind::Event);
    }

    /// ioreg's shape on this Mac (27 Oct): AppUsage before or after the creator, several clients per process,
    /// clients with no usage.
    const AGX: &str = r#"+-o AGXDeviceUserClient  <class AGXDeviceUserClient, id 0x1000014c7, !registered, !matched, active, busy 0, retain 5>
    {
      "AppUsage" = ()
      "IOUserClientCreator" = "pid 406, WindowServer"
    }
+-o AGXDeviceUserClient  <class AGXDeviceUserClient, id 0x1000014c8>
    {
      "AppUsage" = ({"API"="Metal","lastSubmittedTime"=314718144509458,"accumulatedGPUTime"=46002155632208},{"API"="Metal","lastSubmittedTime"=222102488176791,"accumulatedGPUTime"=35492083},{"API"="Metal","lastSubmittedTime"=0,"accumulatedGPUTime"=0})
      "IOUserClientCreator" = "pid 406, WindowServer"
    }
+-o AGXDeviceUserClient  <class AGXDeviceUserClient, id 0x1000014c9>
    {
      "IOUserClientCreator" = "pid 33954, heimdall-embeddi"
      "AppUsage" = ({"API"="Metal","lastSubmittedTime"=1,"accumulatedGPUTime"=30856549791})
    }
"#;

    #[test]
    fn gpu_clients_are_summed_per_process() {
        let t = parse_gpu_clients(AGX);
        assert_eq!(t[&406], ("WindowServer".to_string(), 46002155632208 + 35492083));
        assert_eq!(t[&33954], ("heimdall-embeddi".to_string(), 30856549791));
        assert_eq!(parse_utilization(r#"| "PerformanceStatistics" = {"Device Utilization %"=57,"Renderer Utilization %"=50}"#), Some(57.0));
        assert_eq!(parse_utilization("nothing"), None);
    }

    fn times(v: &[(u32, &str, u64)]) -> GpuTimes {
        v.iter().map(|(pid, n, ns)| (*pid, (n.to_string(), *ns))).collect()
    }

    #[test]
    fn a_busy_window_names_heimdalls_parts_and_the_busiest_elsewhere() {
        let t0 = Instant::now();
        let s = 1_000_000_000u64;
        let mut w = GpuWindow::new(t0, times(&[(406, "WindowServer", 1000 * s), (100, "Heimdall", 10 * s), (101, "heimdall-embeddi", 50 * s), (900, "T3 Code", 0)]));
        for _ in 0..10 { w.sample(Some(57.0)); }
        let end = times(&[
            (406, "WindowServer", 1000 * s + 180 * s), // 30%
            (100, "Heimdall", 10 * s + 6 * s),        // 1%
            (101, "heimdall-embeddi", 50 * s + 24 * s), // 4%
            (102, "Heimdall Drama 3", 3 * s),         // started inside the window: 0.5%
            (900, "T3 Code", 120 * s),                // 20%
            (901, "Finder", 0),
        ]);
        let part = |pid: u32| match pid { 100 => Some("Heimdall"), 101 => Some("Search Index"), 102 => Some("Drama 3"), _ => None };
        let line = w.line(t0 + GPU_WINDOW, &end, part).unwrap();
        assert_eq!(line, "last 10 min: device 57% busy; Heimdall 5.5% of GPU time (Search Index 4.0%, Heimdall 1.0%, Drama 3 0.5%); elsewhere: WindowServer 30.0%, T3 Code 20.0%");
    }

    #[test]
    fn a_quiet_window_says_nothing_and_a_reused_pid_counts_from_zero() {
        let t0 = Instant::now();
        let s = 1_000_000_000u64;
        let mut w = GpuWindow::new(t0, times(&[(100, "Heimdall", 10 * s)]));
        w.sample(Some(8.0));
        assert_eq!(w.line(t0 + GPU_WINDOW, &times(&[(100, "Heimdall", 11 * s)]), |_| Some("Heimdall")), None);
        // pid 100 is now another program: its whole time is this window's, under its own name.
        let mut w = GpuWindow::new(t0, times(&[(100, "Heimdall", 10 * s)]));
        w.sample(Some(40.0));
        let line = w.line(t0 + GPU_WINDOW, &times(&[(100, "ollama", 60 * s)]), |_| None).unwrap();
        assert!(line.ends_with("Heimdall 0.0% of GPU time (none); elsewhere: ollama 10.0%"), "{line}");
    }
}
