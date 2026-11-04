use anyhow::{Result, ensure};
use speakers::{
    call::{CallScopedSpeakers, QUIET_TIMEOUT},
    identity::EnrollmentStore,
    naming::NamingFlow,
};
use std::{path::PathBuf, process::Command, time::Instant};
fn rss(pid: u32) -> Result<u64> {
    let out = Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()?;
    ensure!(out.status.success(), "PID absent");
    Ok(std::str::from_utf8(&out.stdout)?.trim().parse::<u64>()? * 1024)
}
#[cfg(target_os = "macos")]
fn footprint(pid: u32) -> Result<u64> {
    // Darwin rusage_info_v0: UUID followed by ten uint64 counters.
    #[repr(C)]
    struct Usage {
        uuid: [u8; 16],
        counters: [u64; 10],
    }
    unsafe extern "C" {
        fn proc_pid_rusage(pid: i32, flavor: i32, buffer: *mut Usage) -> i32;
    }
    let mut usage = Usage {
        uuid: [0; 16],
        counters: [0; 10],
    };
    ensure!(
        unsafe { proc_pid_rusage(pid as i32, 0, &mut usage) } == 0,
        "cannot inspect helper footprint"
    );
    Ok(usage.counters[7])
}
#[cfg(not(target_os = "macos"))]
fn footprint(pid: u32) -> Result<u64> {
    rss(pid)
}
fn main() -> Result<()> {
    let models = PathBuf::from(std::env::args().nth(1).unwrap_or("models/speakers".into()));
    let executable = std::env::current_exe()?.with_file_name("heimdall-speakers-worker");
    let parent = std::process::id();
    let before = rss(parent)?;
    let mut call = CallScopedSpeakers::new(
        executable,
        models.clone(),
        0,
        NamingFlow::new(EnrollmentStore::default()),
    );
    let start = Instant::now();
    call.activate("offline".into(), 1, 0., start)?;
    let pid = call.worker_pid().unwrap();
    let mut wav = hound::WavReader::open(models.join("ami-480.wav"))?;
    let audio = wav
        .samples::<i16>()
        .take(120 * 16000)
        .map(|v| v.map(|x| x as f32 / 32768.))
        .collect::<Result<Vec<_>, _>>()?;
    for chunk in audio.chunks(320) {
        call.push(chunk, true, start)?;
    }
    let during_parent = rss(parent)?;
    let during_worker = rss(pid)?;
    let during_worker_footprint = footprint(pid)?;
    let events = call.expire(start + QUIET_TIMEOUT)?;
    ensure!(
        !call.is_active() && call.worker_pid().is_none(),
        "helper not expired"
    );
    ensure!(rss(pid).is_err(), "helper still alive");
    let after = rss(parent)?;
    // A second activation must work after expiry without leaking a child or losing naming.
    call.activate("offline".into(), 2, 30., Instant::now())?;
    call.push(&audio[..16000], true, Instant::now())?;
    call.stop()?;
    println!(
        "{}",
        serde_json::json!({"before_parent_bytes":before,"during_parent_bytes":during_parent,"during_worker_bytes":during_worker,"during_worker_physical_footprint_bytes":during_worker_footprint,"during_total_bytes":during_parent+during_worker,"after_parent_bytes":after,"after_worker_bytes":0,"worker_reaped":true,"expiry_seconds":QUIET_TIMEOUT.as_secs(),"audio_seconds":120,"events":events.len(),"reactivation":true})
    );
    Ok(())
}
