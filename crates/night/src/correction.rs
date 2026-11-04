//! Call-scoped CPU work. No ASR lives here: both tracks return to live Parakeet's
//! lowest-priority lane. EOF tears down the process, including ORT global caches.
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use speakers::embedding::VoiceEncoder;
use std::{
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    time::{Duration, Instant},
};

#[derive(Clone, Serialize, Deserialize)]
pub struct Input {
    pub region: crate::dataset::Region,
    pub start_ns: u64,
    pub samples: Vec<f32>,
    pub enrollments: speakers::identity::EnrollmentStore,
}
#[derive(Serialize, Deserialize)]
pub struct Track {
    pub samples: Vec<f32>,
    pub identity: Value,
}
#[derive(Serialize, Deserialize)]
pub struct Output {
    pub tracks: Vec<Track>,
    pub cpu_seconds: f64,
    pub separation_seconds: f64,
    pub total_seconds: f64,
}
pub struct CpuModels {
    models: PathBuf,
}
impl CpuModels {
    pub fn load(models: &Path) -> Result<Self> {
        Ok(Self {
            models: models.into(),
        })
    }
    pub fn run(&mut self, input: &Input) -> Result<Output> {
        ensure!(
            input.region.speakers == 2
                && input.region.end_ns > input.region.start_ns
                && input.region.end_ns - input.region.start_ns <= 6_000_000_000,
            "unsupported correction region"
        );
        let cpu = cpu_seconds();
        let start = Instant::now();
        let mut separator =
            crate::separator::Separator::load(&self.models.join("night/model.int8.onnx"))?;
        let separating = Instant::now();
        let tracks = separator.separate(&input.samples)?;
        let separation_seconds = separating.elapsed().as_secs_f64();
        drop(separator);
        release_cpu_pages();
        // Model arenas/weights must not coexist: the parent already owns live
        // Parakeet. Reloading these smaller CPU models trades latency for bounded RSS.
        let mut encoder =
            speakers::embedding::ReDimNet::load(&self.models.join("speakers/ReDimNet2B6.onnx"))?;
        let mut output = vec![];
        for samples in tracks {
            let identity = if samples.len() >= 32000 {
                serde_json::to_value(input.enrollments.identify(&encoder.embed(&samples)?))?
            } else {
                serde_json::json!({"Unknown":{"reason":"short_context_uncalibrated"}})
            };
            output.push(Track { samples, identity });
        }
        drop(encoder);
        release_cpu_pages();
        Ok(Output {
            tracks: output,
            cpu_seconds: cpu_seconds() - cpu,
            separation_seconds,
            total_seconds: start.elapsed().as_secs_f64(),
        })
    }
}
fn release_cpu_pages() {
    #[cfg(target_os = "macos")]
    unsafe {
        extern "C" {
            fn malloc_zone_pressure_relief(zone: *mut libc::c_void, goal: usize) -> usize;
        }
        malloc_zone_pressure_relief(std::ptr::null_mut(), 0);
    }
}
pub fn cpu_seconds() -> f64 {
    let mut r = unsafe { std::mem::zeroed::<libc::rusage>() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut r) };
    r.ru_utime.tv_sec as f64
        + r.ru_utime.tv_usec as f64 / 1e6
        + r.ru_stime.tv_sec as f64
        + r.ru_stime.tv_usec as f64 / 1e6
}
pub fn swap_bytes() -> Result<u64> {
    let value = crate::gate::bounded_output(
        std::process::Command::new("/usr/sbin/sysctl").args(["-n", "vm.swapusage"]),
    )?;
    let fields: Vec<_> = value.split_whitespace().collect();
    let index = fields
        .iter()
        .position(|v| *v == "used")
        .context("swap used missing")?;
    let used = fields.get(index + 2).context("swap value missing")?;
    let scale = match used.as_bytes().last() {
        Some(b'M') => 1024. * 1024.,
        Some(b'G') => 1024. * 1024. * 1024.,
        Some(b'K') => 1024.,
        _ => anyhow::bail!("swap unit"),
    };
    Ok((used[..used.len() - 1].parse::<f64>()? * scale).round() as u64)
}
/// The helper's own resident memory cap. Only the helper is counted: Heimdall's own
/// RSS (4.7 GiB on 20 Oct, with Jarvis, Parakeet and the archive) is not this
/// process's to bound; counting it killed 65 of 96 requests on 20 Oct.
/// Measured peak for a maximum seven-second input: 1.68 GiB (20 Oct, M5 Max).
pub const HELPER_RSS_CAP_KIB: u64 = 3 * 1024 * 1024;
/// A request the helper has not answered by then is abandoned by both sides.
pub const REQUEST_DEADLINE: Duration = Duration::from_secs(180);
/// Why the watchdog ended the helper. The exit code carries the cause to the client.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GuardExit {
    Memory = 75,
    Swap = 76,
    ParentGone = 77,
    Deadline = 78,
    Probe = 79,
}
impl GuardExit {
    pub fn from_code(code: i32) -> Option<Self> {
        [Self::Memory, Self::Swap, Self::ParentGone, Self::Deadline, Self::Probe]
            .into_iter()
            .find(|c| *c as i32 == code)
    }
    pub fn describe(self) -> String {
        match self {
            Self::Memory => format!(
                "stopped by its memory guard (own RSS above {} MiB)",
                HELPER_RSS_CAP_KIB / 1024
            ),
            Self::Swap => "stopped because Mac swap grew while it was loaded".into(),
            Self::ParentGone => "stopped because Heimdall exited".into(),
            Self::Deadline => format!(
                "stopped: a request ran past {} s",
                REQUEST_DEADLINE.as_secs()
            ),
            Self::Probe => "stopped: its memory/swap probe failed".into(),
        }
    }
}
/// The watchdog's verdict for one tick: `None` keeps the helper running.
pub fn guard(
    parent_alive: bool,
    own_rss_kib: Option<u64>,
    swap_start: u64,
    swap_now: Option<u64>,
    busy_for: Option<Duration>,
) -> Option<GuardExit> {
    if !parent_alive {
        return Some(GuardExit::ParentGone);
    }
    if busy_for.is_some_and(|d| d >= REQUEST_DEADLINE) {
        return Some(GuardExit::Deadline);
    }
    let (Some(rss), Some(swap)) = (own_rss_kib, swap_now) else {
        return Some(GuardExit::Probe);
    };
    if rss >= HELPER_RSS_CAP_KIB {
        return Some(GuardExit::Memory);
    }
    if swap > swap_start {
        return Some(GuardExit::Swap);
    }
    None
}
fn own_rss_kib() -> Option<u64> {
    let rss = crate::gate::bounded_output(
        std::process::Command::new("/bin/ps").args(["-o", "rss=", "-p", &std::process::id().to_string()]),
    )
    .ok()?;
    rss.trim().parse().ok()
}
/// Only the dedicated CPU child calls this. Battery, input and speech do not veto
/// this path. Parent death, the helper's own RSS, zero swap growth and per-request
/// deadlines do; the exit code names which one (`GuardExit`).
pub fn helper(models: PathBuf) -> Result<()> {
    crate::gate::background()?;
    let parent = unsafe { libc::getppid() };
    let swap = swap_bytes()?;
    let busy = std::sync::Arc::new(std::sync::Mutex::new(Some(Instant::now())));
    let observed = busy.clone();
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(2));
        let busy_for = observed.lock().unwrap().map(|at| at.elapsed());
        let verdict = guard(
            unsafe { libc::getppid() } == parent,
            own_rss_kib(),
            swap,
            swap_bytes().ok(),
            busy_for,
        );
        if let Some(exit) = verdict {
            unsafe { libc::_exit(exit as i32) };
        }
    });
    let load_cpu = cpu_seconds();
    let mut models = CpuModels::load(&models)?;
    let mut load_cpu = cpu_seconds() - load_cpu;
    *busy.lock().unwrap() = None;
    let mut input = std::io::BufReader::new(std::io::stdin());
    let mut output = std::io::stdout().lock();
    loop {
        let mut line = String::new();
        use std::io::Read;
        let n = (&mut input)
            .take(8 * 1024 * 1024 + 1)
            .read_line(&mut line)?;
        if n == 0 {
            return Ok(());
        }
        ensure!(
            n <= 8 * 1024 * 1024 && line.ends_with('\n'),
            "correction request exceeds bound"
        );
        *busy.lock().unwrap() = Some(Instant::now());
        let result =
            (|| -> Result<Output> { models.run(&serde_json::from_str::<Input>(&line)?) })();
        let value = match result {
            Ok(mut result) => {
                result.cpu_seconds += load_cpu;
                load_cpu = 0.;
                serde_json::json!({"ok":result})
            }
            Err(error) => serde_json::json!({"error":format!("{error:#}")}),
        };
        serde_json::to_writer(&mut output, &value)?;
        output.write_all(b"\n")?;
        output.flush()?;
        *busy.lock().unwrap() = None;
    }
}

pub struct Client {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
}
impl Client {
    pub fn pid(&self) -> u32 {
        self.child.id()
    }
    pub fn start(executable: &std::path::Path, models: &std::path::Path) -> Result<Self> {
        let mut child = Command::new(executable)
            .arg("--speech-split-helper")
            .arg(models)
            .env("HEIMDALL_SPEAKERS_METAL", "0")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;
        Ok(Self {
            input: child.stdin.take().unwrap(),
            output: BufReader::new(child.stdout.take().unwrap()),
            child,
        })
    }
    /// A lost helper (exited, killed, pipe closed or silent past the deadline) returns
    /// `HelperLost` with its cause; a fresh helper may succeed. Any other error is the
    /// request's own and would fail again.
    pub fn run(&mut self, input: &Input) -> Result<Output> {
        let bytes = serde_json::to_vec(input)?;
        ensure!(bytes.len() <= 8 * 1024 * 1024, "correction request bound");
        let sent = (|| -> std::io::Result<()> {
            self.input.write_all(&bytes)?;
            self.input.write_all(b"\n")?;
            self.input.flush()
        })();
        if let Err(error) = sent {
            return Err(self.lost(&format!("request not delivered ({error})")).into());
        }
        let mut reader = Deadline {
            reader: &mut self.output,
            until: Instant::now() + REQUEST_DEADLINE,
        };
        let mut line = String::new();
        let read = BufReader::new((&mut reader).take(8 * 1024 * 1024 + 1)).read_line(&mut line);
        match read {
            Err(error) if error.kind() == std::io::ErrorKind::TimedOut => {
                let _ = self.child.kill();
                return Err(self.lost(&format!("no reply within {} s", REQUEST_DEADLINE.as_secs())).into());
            }
            Err(error) => return Err(self.lost(&format!("reply unreadable ({error})")).into()),
            Ok(_) if !line.ends_with('\n') && line.len() <= 8 * 1024 * 1024 => {
                return Err(self.lost("reply cut off").into());
            }
            Ok(_) => {}
        }
        ensure!(
            line.len() <= 8 * 1024 * 1024 && line.ends_with('\n'),
            "correction helper reply exceeds 8 MiB"
        );
        let value: serde_json::Value = serde_json::from_str(&line)?;
        ensure!(
            value.get("error").is_none(),
            "CPU helper: {}",
            value["error"]
        );
        Ok(serde_json::from_value(
            value
                .get("ok")
                .context("correction result missing")?
                .clone(),
        )?)
    }
    /// The helper's exit status after its pipe failed, waited for briefly, as a cause.
    fn lost(&mut self, symptom: &str) -> HelperLost {
        let until = Instant::now() + Duration::from_secs(2);
        let status = loop {
            match self.child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) if Instant::now() < until => std::thread::sleep(Duration::from_millis(20)),
                _ => {
                    let _ = self.child.kill();
                    break self.child.wait().ok();
                }
            }
        };
        HelperLost(format!("correction helper {}: {symptom}", exit_cause(status)))
    }
}
/// The helper process ended or stopped answering; the request itself may be fine.
#[derive(Debug)]
pub struct HelperLost(pub String);
impl std::fmt::Display for HelperLost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for HelperLost {}
pub fn exit_cause(status: Option<std::process::ExitStatus>) -> String {
    use std::os::unix::process::ExitStatusExt;
    match status {
        None => "ended (status unavailable)".into(),
        Some(s) => match (s.code(), s.signal()) {
            (Some(code), _) => GuardExit::from_code(code)
                .map(GuardExit::describe)
                .unwrap_or_else(|| format!("exited with code {code}")),
            (None, Some(9)) => "was killed (SIGKILL; macOS memory pressure or a forced stop)".into(),
            (None, Some(signal)) => format!("was killed by signal {signal}"),
            (None, None) => "ended".into(),
        },
    }
}
struct Deadline<'a> {
    reader: &'a mut BufReader<ChildStdout>,
    until: Instant,
}
impl Read for Deadline<'_> {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        use std::os::fd::AsRawFd;
        if self.reader.buffer().is_empty() {
            let remaining = self.until.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(std::io::ErrorKind::TimedOut.into());
            }
            let mut fd = libc::pollfd {
                fd: self.reader.get_ref().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let n = unsafe {
                libc::poll(
                    &mut fd,
                    1,
                    remaining.as_millis().min(i32::MAX as u128).max(1) as i32,
                )
            };
            if n <= 0 {
                return Err(std::io::ErrorKind::TimedOut.into());
            }
        }
        self.reader.read(bytes)
    }
}
impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const GIB_KIB: u64 = 1024 * 1024;
    #[test]
    fn guard_ignores_the_parents_memory_and_names_each_cause() {
        // 20 Oct: Heimdall held 2.6, later 4.7 GiB; the helper peaks at 1.7 GiB. The old
        // combined 3.8 GiB cap ended most requests; the helper's own RSS is the budget.
        let idle = guard(true, Some(GIB_KIB), 100, Some(100), None);
        assert_eq!(idle, None);
        assert_eq!(guard(true, Some(GIB_KIB), 100, Some(100), Some(Duration::from_secs(20))), None);
        assert_eq!(guard(true, Some(HELPER_RSS_CAP_KIB), 100, Some(100), None), Some(GuardExit::Memory));
        assert_eq!(guard(true, Some(GIB_KIB), 100, Some(101), None), Some(GuardExit::Swap));
        assert_eq!(guard(true, Some(GIB_KIB), 100, Some(99), None), None, "swap shrinking is fine");
        assert_eq!(guard(false, Some(GIB_KIB), 100, Some(100), None), Some(GuardExit::ParentGone));
        assert_eq!(guard(true, Some(GIB_KIB), 100, Some(100), Some(REQUEST_DEADLINE)), Some(GuardExit::Deadline));
        assert_eq!(guard(true, None, 100, Some(100), None), Some(GuardExit::Probe));
        assert_eq!(guard(true, Some(GIB_KIB), 100, None, None), Some(GuardExit::Probe));
        // Parent death outranks everything; a stuck request outranks a probe failure.
        assert_eq!(guard(false, None, 100, None, Some(REQUEST_DEADLINE)), Some(GuardExit::ParentGone));
        assert_eq!(guard(true, None, 100, None, Some(REQUEST_DEADLINE)), Some(GuardExit::Deadline));
    }
    #[test]
    fn every_exit_code_round_trips_to_a_readable_cause() {
        use std::os::unix::process::ExitStatusExt;
        for exit in [GuardExit::Memory, GuardExit::Swap, GuardExit::ParentGone, GuardExit::Deadline, GuardExit::Probe] {
            assert_eq!(GuardExit::from_code(exit as i32), Some(exit));
            let text = exit_cause(Some(std::process::ExitStatus::from_raw((exit as i32) << 8)));
            assert_eq!(text, exit.describe());
            assert!(text.starts_with("stopped"), "{text}");
        }
        assert!(exit_cause(Some(std::process::ExitStatus::from_raw(1 << 8))).contains("code 1"));
        assert!(exit_cause(Some(std::process::ExitStatus::from_raw(9))).contains("SIGKILL"));
        assert!(exit_cause(Some(std::process::ExitStatus::from_raw(11))).contains("signal 11"));
        assert!(exit_cause(None).contains("unavailable"));
        assert!(GuardExit::Memory.describe().contains("3072 MiB"));
    }
    fn fake_helper(dir: &Path, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("helper.sh");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        path
    }
    fn input(samples: usize) -> Input {
        Input {
            region: crate::dataset::Region { session: "s".into(), source: "call".into(), start_ns: 0, end_ns: 1_000_000_000, speakers: 2, utterance_ids: vec![], audio: vec![] },
            start_ns: 0,
            samples: vec![0.25; samples],
            enrollments: speakers::identity::EnrollmentStore::uncalibrated(),
        }
    }
    fn lost(error: anyhow::Error) -> String {
        error.downcast::<HelperLost>().expect("a lost helper is typed so the owner can retry").0
    }
    #[test]
    fn a_helper_ended_by_its_guard_reports_the_guard_not_a_truncated_reply() {
        let t = tempfile::tempdir().unwrap();
        // Reads the request, then exits the way the watchdog does.
        let exe = fake_helper(t.path(), "read line\nexit 75");
        let mut client = Client::start(&exe, t.path()).unwrap();
        let text = lost(client.run(&input(16)).err().unwrap());
        assert!(text.contains("memory guard") && text.contains("reply cut off"), "{text}");
        assert!(!text.contains("truncated"), "{text}");
    }
    #[test]
    fn a_helper_gone_before_a_large_request_is_lost_with_its_exit_cause() {
        let t = tempfile::tempdir().unwrap();
        let exe = fake_helper(t.path(), "exit 76");
        let mut client = Client::start(&exe, t.path()).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        // ~1 MB: larger than the pipe buffer, like a real six-second region.
        let text = lost(client.run(&input(100_000)).err().unwrap());
        assert!(text.contains("swap grew") && text.contains("not delivered"), "{text}");
    }
    #[test]
    fn a_killed_helper_and_a_bad_reply_are_told_apart() {
        let t = tempfile::tempdir().unwrap();
        let exe = fake_helper(t.path(), "read line\nkill -9 $$");
        let text = lost(Client::start(&exe, t.path()).unwrap().run(&input(16)).err().unwrap());
        assert!(text.contains("SIGKILL"), "{text}");
        // The helper's own error reply is the request's fault: not a lost helper.
        let exe = fake_helper(t.path(), "read line\necho '{\"error\":\"unsupported correction region\"}'\nread line");
        let error = Client::start(&exe, t.path()).unwrap().run(&input(16)).err().unwrap();
        assert!(error.downcast_ref::<HelperLost>().is_none());
        assert!(format!("{error:#}").contains("unsupported correction region"));
    }
}
