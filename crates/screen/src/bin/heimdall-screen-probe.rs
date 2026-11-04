//! Deferred live measurement. --help performs no permission or capture calls.
use anyhow::{ensure, Context, Result};
#[cfg(target_os = "macos")]
fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("heimdall-screen-probe --after-grant [--seconds 12] [--powermetrics] [--full-resolution --data-dir PATH --commands 1]\nMeasures capture-off, 1 fps, 2 fps. Full-resolution mode writes temporary private PNGs under the explicit data directory, then removes them. Never requests permission.\n--powermetrics uses sudo -n (no prompt); missing authorization is reported.");
        return Ok(());
    }
    ensure!(
        args.iter().any(|a| a == "--after-grant"),
        "probe requires --after-grant; no capture or permission request performed"
    );
    let mut seconds = 12u64;
    let mut data_dir = None;
    let mut commands = 1u64;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--after-grant" | "--powermetrics" | "--full-resolution" | "--recording" => (),
            "--commands" => {
                i += 1;
                commands = args
                    .get(i)
                    .ok_or_else(|| anyhow::anyhow!("missing commands"))?
                    .parse()?;
                ensure!((1..=3).contains(&commands), "commands must be 1..3");
            }
            "--data-dir" => {
                i += 1;
                data_dir = Some(std::path::PathBuf::from(
                    args.get(i)
                        .ok_or_else(|| anyhow::anyhow!("missing data directory"))?,
                ));
            }
            "--seconds" => {
                i += 1;
                seconds = args
                    .get(i)
                    .ok_or_else(|| anyhow::anyhow!("missing seconds"))?
                    .parse()?;
            }
            other => anyhow::bail!("unknown argument: {other}"),
        }
        i += 1;
    }
    ensure!(
        (10..=30).contains(&seconds),
        "window must be 10..30 seconds"
    );
    ensure!(
        screen::screen_permission_granted(),
        "Screen Recording is not granted to this responsible process; refused without prompting"
    );
    let gpu = args.iter().any(|a| a == "--powermetrics");
    let hardware = std::process::Command::new("/usr/sbin/sysctl")
        .args(["-n", "machdep.cpu.brand_string"])
        .output()?;
    println!(
        "{}",
        serde_json::json!({"hardware": String::from_utf8_lossy(&hardware.stdout).trim(), "unix_seconds": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs(), "baseline": if args.iter().any(|a| a == "--full-resolution") { "same process, ring only before/after each command" } else { "same process, capture off" }, "machine": "shared; background load not controlled", "frame_storage": "hardware HEVC in archive packs, no RGB history"})
    );
    if args.iter().any(|a|a=="--recording") {
        let data=data_dir.context("recording mode requires an isolated --data-dir")?;
        ensure!(!data.exists(),"recording data directory must be new");
        let forbidden=[heimdall_home::data_dir(),heimdall_home::Env::current().legacy_data()];
        ensure!(!forbidden.iter().any(|p|data.starts_with(p)),"real Heimdall profile is prohibited");
        measure(0,10,gpu)?;
        let root=data.join("archive");
        let capture=screen::ScreenCapture::start_recording(Default::default(),root.join("session-recording-probe"),std::sync::Arc::new(|_|true))?;
        std::thread::sleep(std::time::Duration::from_secs(2));
        let phrase_ns=capture::host_now_ns().saturating_sub(500_000_000);
        let before=heimdall_archive::store::bytes_on_disk(&root)?;
        let start=std::time::Instant::now();
        measure_capture(5,seconds,gpu,Some(&capture))?;
        let elapsed=start.elapsed().as_secs_f64();
        let selected=capture.buffer.lock().unwrap().select(phrase_ns,capture::host_now_ns());
        ensure!(!selected.frames.is_empty() && selected.missing_displays.is_empty(),"recorded phrase frame unavailable");
        for frame in selected.frames {
            ensure!(frame.host_time_ns<=phrase_ns,"future phrase frame");
            let path=data.join(format!("phrase-frame-{}.png",frame.display.id));
            let mut encoder=png::Encoder::new(std::fs::File::create(&path)?,frame.width,frame.height);encoder.set_color(png::ColorType::Rgb);encoder.set_depth(png::BitDepth::Eight);encoder.write_header()?.write_image_data(&frame.rgb)?;
            println!("{}",serde_json::json!({"phrase_host_ns":phrase_ns,"selected_host_ns":frame.host_time_ns,"display":frame.display.id,"width":frame.width,"height":frame.height,"frame_png":path}));
        }
        drop(capture);
        let after=heimdall_archive::store::bytes_on_disk(&root)?;
        println!("{}",serde_json::json!({"recording_archive":root,"store_bytes_start":before,"store_bytes_end":after,"growth_bytes":after.saturating_sub(before),"derived_store_mb_per_hour":after.saturating_sub(before) as f64/elapsed*3600./1_000_000.,"measurement_seconds":elapsed,"content":"live user desktop, no scripted UI interaction; includes index/pack allocation","resolution":"half native pixels","fps":5,"energy_scope":"process CPU; GPU/media engine only if authorized counters exist"}));
        return Ok(())
    }
    if args.iter().any(|a| a == "--full-resolution") {
        return measure_full_resolution(
            &data_dir.ok_or_else(|| {
                anyhow::anyhow!("full-resolution mode requires an isolated --data-dir")
            })?,
            seconds,
            gpu,
            commands,
        );
    }
    ensure!(
        data_dir.is_none(),
        "--data-dir is only for --full-resolution mode"
    );
    measure(0, seconds, gpu)?;
    for fps in [1, 2] {
        let capture = screen::ScreenCapture::start(screen::ScreenConfig {
            fps,
            ..Default::default()
        })?;
        // Fill history before measured steady-state interval; do not include startup.
        std::thread::sleep(std::time::Duration::from_secs(10));
        while let Ok(event) = capture.events.try_recv() {
            if let screen::ScreenEvent::Error(error) = event {
                anyhow::bail!("screen stream error: {error}");
            }
        }
        measure_capture(fps, seconds, gpu, Some(&capture))?;
        drop(capture);
    }
    Ok(())
}
#[cfg(target_os = "macos")]
fn measure_full_resolution(
    data: &std::path::Path,
    seconds: u64,
    gpu: bool,
    commands: u64,
) -> Result<()> {
    use std::{
        sync::{
            atomic::{AtomicBool, AtomicU64, Ordering},
            Arc,
        },
        time::{Duration, Instant},
    };
    // A measurement must not populate the real profile or an existing lab directory.
    ensure!(
        !data.exists(),
        "full-resolution probe data directory must be new"
    );
    std::fs::create_dir_all(data)?;
    let capture = screen::ScreenCapture::start(Default::default())?;
    std::thread::sleep(Duration::from_secs(2));
    println!(
        "{}",
        serde_json::json!({"window":"ring_only_before_command"})
    );
    measure_capture(1, seconds, gpu, Some(&capture))?;
    for command in 1..=commands {
        let before = usage()?;
        let peak = Arc::new(AtomicU64::new(before.1));
        let done = Arc::new(AtomicBool::new(false));
        let monitor_peak = peak.clone();
        let monitor_done = done.clone();
        let monitor = std::thread::spawn(move || {
            while !monitor_done.load(Ordering::Acquire) {
                if let Ok((_, rss)) = usage() {
                    monitor_peak.fetch_max(rss, Ordering::Relaxed);
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        });
        let now = capture::host_now_ns();
        let matched = screen::PhraseMatch {
            source: "probe".into(),
            phrase: "screenshot_now".into(),
            start_ns: now,
            trigger_ns: now,
        };
        let start = Instant::now();
        let result =
            screen::attach_command_screenshot(&capture.buffer, &matched, now, &data.join("shots"));
        let elapsed = start.elapsed().as_nanos();
        done.store(true, Ordering::Release);
        monitor
            .join()
            .map_err(|_| anyhow::anyhow!("memory sampler panicked"))?;
        let action = result?;
        let after = usage()?;
        println!(
            "{}",
            serde_json::json!({"event":"full_resolution_command","command":command,"action":action,"command_elapsed_ns":elapsed,"command_cpu_ns":after.0.saturating_sub(before.0),"rss_before_bytes":before.1,"rss_after_bytes":after.1,"rss_sample_peak_bytes":peak.load(Ordering::Relaxed).max(after.1),"ring_rgb_bytes":capture.buffer.lock().unwrap().bytes(),"timestamp":"ring = stream PTS; full_resolution = screenshot callback receipt host time","machine":"shared/busy", "optimized_build": !cfg!(debug_assertions)})
        );
        for shot in &action.shots {
            let reader =
                png::Decoder::new(std::io::BufReader::new(std::fs::File::open(&shot.path)?))
                    .read_info()?;
            ensure!(
                reader.info().width == shot.width && reader.info().height == shot.height,
                "persisted PNG dimensions differ"
            );
            std::fs::remove_file(&shot.path)?;
        }
        println!(
            "{}",
            serde_json::json!({"window":"ring_only_after_command"})
        );
        measure_capture(1, seconds, gpu, Some(&capture))?;
        ensure!(
            action.full_resolution_errors.is_empty(),
            "supplemental capture errors: {:?}",
            action.full_resolution_errors
        );
    }
    Ok(())
}
#[cfg(target_os = "macos")]
fn measure(fps: u32, seconds: u64, gpu: bool) -> Result<()> {
    measure_capture(fps, seconds, gpu, None)
}
#[cfg(target_os = "macos")]
#[allow(deprecated)] // Darwin ABI is stable; libc recommends a crate solely for these bindings.
fn usage() -> Result<(u64, u64)> {
    let mut u: libc::rusage_info_v2 = unsafe { std::mem::zeroed() };
    let result = unsafe {
        libc::proc_pid_rusage(
            std::process::id() as i32,
            libc::RUSAGE_INFO_V2,
            &mut u as *mut _ as *mut libc::rusage_info_t,
        )
    };
    ensure!(result == 0, "proc_pid_rusage failed");
    // proc_pid_rusage CPU counters are Mach absolute ticks on Apple Silicon.
    let mut timebase: libc::mach_timebase_info = unsafe { std::mem::zeroed() };
    ensure!(
        unsafe { libc::mach_timebase_info(&mut timebase) } == 0 && timebase.denom != 0,
        "invalid Mach timebase"
    );
    let cpu_ns = (u.ri_user_time as u128 + u.ri_system_time as u128) * timebase.numer as u128
        / timebase.denom as u128;
    Ok((u64::try_from(cpu_ns)?, u.ri_resident_size))
}
#[cfg(target_os = "macos")]
fn measure_capture(
    fps: u32,
    seconds: u64,
    gpu: bool,
    capture: Option<&screen::ScreenCapture>,
) -> Result<()> {
    use std::{
        io::Read,
        process::{Command, Stdio},
        time::{Duration, Instant},
    };
    // sudo -n can never display an authorization/password prompt.
    let mut power = if gpu {
        Some(PowerChild(
            Command::new("/usr/bin/sudo")
                .args([
                    "-n",
                    "/usr/bin/powermetrics",
                    "--samplers",
                    "tasks,gpu_power",
                    "--show-process-gpu",
                    "-i",
                    "1000",
                    "-n",
                    &seconds.to_string(),
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()?,
        ))
    } else {
        None
    };
    let power_readers = power.as_mut().map(|child| {
        let stdout = child.0.stdout.take().unwrap();
        let stderr = child.0.stderr.take().unwrap();
        (
            std::thread::spawn(move || {
                let mut s = String::new();
                let _ = stdout.take(4 * 1024 * 1024).read_to_string(&mut s);
                s
            }),
            std::thread::spawn(move || {
                let mut s = String::new();
                let _ = stderr.take(8192).read_to_string(&mut s);
                s
            }),
        )
    });
    let (cpu_start, rss_start) = usage()?;
    let start = Instant::now();
    let mut peak_rss = rss_start;
    for _ in 0..seconds {
        std::thread::sleep(Duration::from_secs(1));
        peak_rss = peak_rss.max(usage()?.1);
        if let Some(capture) = capture {
            while let Ok(event) = capture.events.try_recv() {
                if let screen::ScreenEvent::Error(error) = event {
                    anyhow::bail!("stream error: {error}");
                }
            }
        }
    }
    let elapsed = start.elapsed().as_secs_f64();
    let (cpu_end, rss_end) = usage()?;
    let (buffer_bytes, frames, displays) = capture
        .map(|c| {
            let mut b = c.buffer.lock().unwrap();
            b.expire(capture::host_now_ns());
            (b.bytes(), b.frame_count(), b.display_count())
        })
        .unwrap_or_default();
    if capture.is_some() {
        ensure!(
            frames > 0 && displays > 0,
            "no screen frames delivered; no cost claim possible"
        );
    }
    let gpu_receipt = if let Some(mut child) = power {
        if child.0.try_wait()?.is_none() {
            let _ = child.0.kill();
        }
        let status = child.0.wait()?;
        let (out, err) = power_readers.unwrap();
        let out = out.join().unwrap_or_default();
        let err = err.join().unwrap_or_default();
        // Retain exact measurement lines rather than assuming unsupported counters.
        let lines: Vec<_> = out
            .lines()
            .filter(|l| {
                l.contains("GPU")
                    || l.contains("WindowServer")
                    || l.contains("heimdall-screen")
                    || l.contains("GPU active")
                    || l.contains("PID")
            })
            .collect();
        serde_json::json!({"system_gpu_active_percent_mean":counter_mean(&out, "GPU active residency:"), "system_gpu_power_mw_mean":counter_mean(&out, "GPU Power:"), "status":status.code(), "scope":"system GPU and per-process GPU time; includes compositor/background work", "lines":lines, "error":err.trim()})
    } else {
        serde_json::json!({"unmeasured":"enable --powermetrics with existing privileged authorization; process CPU is not GPU cost"})
    };
    println!(
        "{}",
        serde_json::json!({"fps":fps,"window_seconds":elapsed,"process_cpu_percent_one_core":(cpu_end-cpu_start) as f64/1e9/elapsed*100.,"rss_start_bytes":rss_start,"rss_end_bytes":rss_end,"rss_peak_sampled_bytes":peak_rss,"buffer_rgb_accounted_bytes":buffer_bytes,"frames":frames,"displays":displays,"gpu":gpu_receipt})
    );
    Ok(())
}
#[cfg(not(target_os = "macos"))]
fn main() -> Result<()> {
    anyhow::bail!("screen probe requires macOS")
}

#[cfg(target_os = "macos")]
struct PowerChild(std::process::Child);
#[cfg(target_os = "macos")]
impl Drop for PowerChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn counter_mean(text: &str, prefix: &str) -> Option<f64> {
    let numbers: Vec<f64> = text
        .lines()
        .filter_map(|line| {
            line.trim()
                .strip_prefix(prefix)?
                .split_whitespace()
                .next()?
                .trim_end_matches('%')
                .parse::<f64>()
                .ok()
                .filter(|n| n.is_finite())
        })
        .collect();
    if numbers.is_empty() {
        None
    } else {
        Some(numbers.iter().sum::<f64>() / numbers.len() as f64)
    }
}
#[cfg(test)]
mod tests {
    #[test]
    fn gpu_receipt_means_use_only_supported_counters() {
        assert_eq!(
            super::counter_mean(
                "GPU active residency: 10.00% (400 MHz)\nGPU active residency: 20.00%",
                "GPU active residency:"
            ),
            Some(15.)
        );
        assert_eq!(
            super::counter_mean("GPU Power: 120 mW\nGPU Power: 80 mW", "GPU Power:"),
            Some(100.)
        );
        assert_eq!(super::counter_mean("permission denied", "GPU Power:"), None);
    }
}
