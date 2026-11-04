//! Rootless process CPU sampling and whole-machine macmon brackets. No TCC APIs.
use crate::{cpu, settings};
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
// A helper is part of the application cost. Retain its final CPU counter before
// reaping so expiry cannot make the aggregate counter fall or hide its work.
#[derive(Default)]
struct CpuState {
    finished: Usage,
    live: std::collections::HashMap<u32, Usage>,
}
#[derive(Default, Clone, Copy)]
struct Usage { cpu: f64, interrupts: u64, platform: u64 }
impl Usage {
    fn retain(&mut self, next: Self) { self.cpu = self.cpu.max(next.cpu); self.interrupts = self.interrupts.max(next.interrupts); self.platform = self.platform.max(next.platform); }
    fn add(&mut self, next: Self) { self.cpu += next.cpu; self.interrupts += next.interrupts; self.platform += next.platform; }
}
#[derive(Default)]
pub struct SpeakerCpu(Mutex<CpuState>);
impl SpeakerCpu {
    pub fn register(&self, pid: u32) {
        self.0.lock().unwrap().live.insert(pid, Usage::default());
    }
    pub fn retire(&self, pid: u32) {
        let mut state = self.0.lock().unwrap();
        if let Some(mut value) = state.live.remove(&pid) {
            value.retain(child_usage(pid).unwrap_or(value));
            state.finished.add(value);
        }
    }
    fn total(&self) -> Usage {
        let mut state = self.0.lock().unwrap();
        let mut total = state.finished;
        for (pid, value) in &mut state.live {
            value.retain(child_usage(*pid).unwrap_or(*value));
            total.add(*value);
        }
        total
    }
}
#[cfg(target_os = "macos")]
fn child_usage(pid: u32) -> Option<Usage> {
    extern "C" {
        fn proc_pid_rusage(pid: i32, flavor: i32, info: *mut libc::rusage_info_v2) -> i32;
    }
    let mut info: libc::rusage_info_v2 = unsafe { std::mem::zeroed() };
    (unsafe { proc_pid_rusage(pid as i32, 2, &mut info) } == 0)
        .then(|| Usage { cpu: mach_seconds(info.ri_user_time) + mach_seconds(info.ri_system_time), interrupts: info.ri_interrupt_wkups, platform: info.ri_pkg_idle_wkups })
}
#[cfg(target_os = "macos")]
fn mach_seconds(ticks: u64) -> f64 {
    static SCALE: std::sync::OnceLock<f64> = std::sync::OnceLock::new();
    let scale = SCALE.get_or_init(|| {
        #[repr(C)]
        struct Timebase { numer: u32, denom: u32 }
        unsafe extern "C" { fn mach_timebase_info(info: *mut Timebase) -> i32; }
        let mut timebase = Timebase { numer: 0, denom: 0 };
        let code = unsafe { mach_timebase_info(&mut timebase) };
        assert_eq!(code, 0, "Mach timebase unavailable");
        ticks_to_seconds(1, timebase.numer, timebase.denom)
    });
    ticks as f64 * scale
}
fn ticks_to_seconds(ticks: u64, numer: u32, denom: u32) -> f64 {
    ticks as f64 * numer as f64 / denom as f64 / 1e9
}
#[cfg(not(target_os = "macos"))]
fn child_usage(_pid: u32) -> Option<Usage> {
    None
}
pub struct Window {
    start: Instant,
    cpu: f64,
    helper_start: Usage,
    power_start: crate::Power,
    helpers: Arc<SpeakerCpu>,
    stop: Arc<AtomicBool>,
    sampler: Option<JoinHandle<Vec<f64>>>,
    macmon: Option<Child>,
    power_path: PathBuf,
}
impl Window {
    pub fn start(
        label: &str,
        dir: &Path,
        macmon: Option<&Path>,
        helpers: Arc<SpeakerCpu>,
    ) -> Result<Self> {
        settings::private_dir(dir)?;
        let power_path = dir.join(format!("{label}-power.jsonl"));
        let process = if let Some(binary) = macmon {
            Some(
                Command::new(binary)
                    .args(["pipe", "-i", "200"])
                    .stdout(Stdio::from(std::fs::File::create(&power_path)?))
                    .stderr(Stdio::from(std::fs::File::create(
                        dir.join(format!("{label}-macmon.stderr")),
                    )?))
                    .spawn()
                    .context("launch rootless macmon")?,
            )
        } else {
            None
        };
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let helper_start = helpers.total();
        let sampling_helpers = helpers.clone();
        let sampler = thread::spawn(move || {
            let mut previous = cpu() + sampling_helpers.total().cpu;
            let mut time = Instant::now();
            let mut values = vec![];
            while !stopped.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(100));
                let now = cpu() + sampling_helpers.total().cpu;
                let elapsed = time.elapsed().as_secs_f64();
                values.push((now - previous) / elapsed * 100.);
                previous = now;
                time = Instant::now();
            }
            values
        });
        Ok(Self {
            start: Instant::now(),
            cpu: cpu(),
            helper_start,
            power_start: crate::power()?,
            helpers,
            stop,
            sampler: Some(sampler),
            macmon: process,
            power_path,
        })
    }
    pub fn finish(mut self) -> Result<Value> {
        let elapsed = self.start.elapsed().as_secs_f64();
        let parent_used = cpu() - self.cpu;
        let helper_end = self.helpers.total();
        let helper_used = helper_end.cpu - self.helper_start.cpu;
        let power_end = crate::power()?;
        let interrupts = power_end.interrupts.saturating_sub(self.power_start.interrupts) + helper_end.interrupts.saturating_sub(self.helper_start.interrupts);
        let platform = power_end.platform.saturating_sub(self.power_start.platform) + helper_end.platform.saturating_sub(self.helper_start.platform);
        let used = parent_used + helper_used;
        self.stop.store(true, Ordering::Release);
        let samples = self.sampler.take().unwrap().join().unwrap_or_default();
        if let Some(mut child) = self.macmon.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let power: Vec<Value> = if self.power_path.exists() {
            std::fs::read_to_string(&self.power_path)?
                .lines()
                .filter_map(|s| serde_json::from_str(s).ok())
                .collect()
        } else {
            vec![]
        };
        let means: serde_json::Map<_, _> = [
            "all_power",
            "sys_power",
            "cpu_power",
            "gpu_power",
            "ane_power",
        ]
        .into_iter()
        .filter_map(|key| {
            let vals: Vec<_> = power.iter().filter_map(|p| p[key].as_f64()).collect();
            (!vals.is_empty()).then(|| {
                (
                    key.into(),
                    json!(vals.iter().sum::<f64>() / vals.len() as f64),
                )
            })
        })
        .collect();
        Ok(
            json!({"wall_seconds":elapsed,"cpu_seconds":used,"parent_cpu_seconds":parent_used,"helper_cpu_seconds":helper_used,"cpu_scope":"application plus owned speaker helpers; excludes macmon","cpu_average_percent_one_core":used/elapsed*100.,"cpu_peak_percent_one_core_100ms":samples.into_iter().fold(0_f64,f64::max),"cpu_sample_period_ms":100,"interrupt_wakeups":interrupts,"platform_idle_wakeups":platform,"interrupt_wakeups_per_second":interrupts as f64/elapsed,"platform_idle_wakeups_per_second":platform as f64/elapsed,"macmon_samples":power.len(),"mean_watts":means,"power_boundary":"whole-machine SoC/system; includes other work; not per-app watts"}),
        )
    }
}
impl Drop for Window {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(mut c) = self.macmon.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        if let Some(t) = self.sampler.take() {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn helper_ticks_use_timebase_not_nanoseconds() {
        assert_eq!(super::ticks_to_seconds(24_000_000, 125, 3), 1.0);
    }
}
