//! Permission-free idle measurement, or an explicitly gated post-grant live probe.
use anyhow::Result;
use capture::{Capture, CaptureConfig, CaptureConsent, CaptureEvent};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    time::{Duration, Instant},
};
fn cpu_seconds() -> f64 {
    let mut r = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    unsafe {
        if libc::getrusage(libc::RUSAGE_SELF, r.as_mut_ptr()) != 0 {
            return 0.;
        }
        let r = r.assume_init();
        r.ru_utime.tv_sec as f64
            + r.ru_stime.tv_sec as f64
            + (r.ru_utime.tv_usec + r.ru_stime.tv_usec) as f64 / 1e6
    }
}
fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.is_empty() || args.iter().any(|s| s == "--help") {
        println!("heimdall-capture-probe --idle-seconds N --archive-dir PATH\nLive after grant (from Heimdall.app): --after-permission-grant --live-seconds N [--archive-dir PATH]\nNo mode starts live capture by default.");
        return Ok(());
    }
    let value = |flag: &str| {
        args.iter()
            .position(|s| s == flag)
            .and_then(|i| args.get(i + 1))
    };
    let mut config = CaptureConfig::default();
    if let Some(path) = value("--archive-dir") {
        config.archive_dir = capture::session_address(&PathBuf::from(path));
    }
    let idle = value("--idle-seconds")
        .map(|s| s.parse::<u64>())
        .transpose()?;
    let live = value("--live-seconds")
        .map(|s| s.parse::<u64>())
        .transpose()?;
    anyhow::ensure!(
        idle.is_some() ^ live.is_some(),
        "choose exactly one idle or live mode"
    );
    if idle.is_some() {
        anyhow::ensure!(
            value("--archive-dir").is_some(),
            "idle measurement requires an isolated --archive-dir"
        );
    }
    if live.is_some() {
        anyhow::ensure!(
            args.iter().any(|s| s == "--after-permission-grant"),
            "live probe requires explicit --after-permission-grant"
        );
    }
    let seconds = idle.or(live).unwrap();
    anyhow::ensure!(
        seconds > 0 && seconds <= 86400,
        "probe seconds must be 1..86400"
    );
    #[cfg(target_os = "macos")]
    {
        let (capture, receiver) = if idle.is_some() {
            Capture::idle(config)?
        } else {
            Capture::start(config, CaptureConsent::after_user_grant())?
        };
        let started = Instant::now();
        let cpu = cpu_seconds();
        let mut added = 0;
        let mut removed = 0;
        let mut chunks = 0;
        let mut samples = 0;
        let mut gaps = 0;
        // Per-source receipt makes a successfully created but silent tap visible.
        let mut sources: BTreeMap<String, (usize, usize, usize, f32, u64, u64)> = BTreeMap::new();
        while started.elapsed() < Duration::from_secs(seconds) {
            let wait = Duration::from_secs(seconds)
                .saturating_sub(started.elapsed())
                .min(Duration::from_secs(1));
            match receiver.recv_timeout(wait) {
                Ok(CaptureEvent::SourceAdded(info)) => {
                    added += 1;
                    println!("added {}", serde_json::to_string(&info)?);
                }
                Ok(CaptureEvent::SourceRemoved {
                    source_id, reason, ..
                }) => {
                    removed += 1;
                    println!("removed {} {reason}", source_id.0);
                }
                Ok(CaptureEvent::Audio(c)) => {
                    chunks += 1;
                    samples += c.samples.len();
                    let source = sources
                        .entry(c.source_id.0)
                        .or_insert((0, 0, 0, 0., c.host_ns, c.host_ns));
                    source.0 += 1;
                    source.1 += c.samples.len();
                    source.2 += c.samples.iter().filter(|v| **v != 0.).count();
                    source.3 = c.samples.iter().fold(source.3, |peak, v| peak.max(v.abs()));
                    source.5 = c.host_ns;
                }
                Ok(CaptureEvent::Diagnostic { detail, .. }) => println!("diagnostic {detail}"),
                Ok(CaptureEvent::Gap { detail, .. }) => {
                    gaps += 1;
                    println!("gap {detail}");
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                Err(e) => anyhow::bail!("capture stopped: {e}"),
            }
        }
        let wall = started.elapsed().as_secs_f64();
        let consumed = cpu_seconds() - cpu;
        println!("mode={} wall_seconds={wall:.3} cpu_seconds={consumed:.6} cpu_one_core_percent={:.4} added={added} removed={removed} chunks={chunks} samples={samples} gaps={gaps}",if idle.is_some(){"idle-no-streams"}else{"live"},consumed/wall*100.);
        for (id, (chunks, samples, nonzero, peak, first_ns, last_ns)) in sources {
            println!("source={id} chunks={chunks} samples={samples} nonzero={nonzero} peak={peak:.8} first_host_ns={first_ns} last_host_ns={last_ns}");
        }
        drop(capture); // Stop native IO and checkpoint the independent raw receipt.
    }
    #[cfg(not(target_os = "macos"))]
    anyhow::bail!("native probe requires macOS");
    Ok(())
}
