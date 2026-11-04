//! Agent-owned offline real-time-paced helper check; no microphone or permissions.
use anyhow::{ensure, Result};
use speakers::{
    call::CallScopedSpeakers, identity::EnrollmentStore, live::LiveStrategy, naming::NamingFlow,
};
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};
fn cpu_seconds() -> f64 {
    let mut u: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut u) };
    (u.ru_utime.tv_sec + u.ru_stime.tv_sec) as f64
        + (u.ru_utime.tv_usec + u.ru_stime.tv_usec) as f64 / 1e6
}
fn child_usage(pid: u32) -> Result<(f64, u64)> {
    let out = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "time=,rss="])
        .output()?;
    let line = String::from_utf8(out.stdout)?;
    let columns: Vec<_> = line.split_whitespace().collect();
    ensure!(columns.len() == 2, "missing helper process");
    let mut seconds = 0.;
    for part in columns[0].split(':') {
        seconds = seconds * 60. + part.parse::<f64>()?;
    }
    Ok((seconds, columns[1].parse::<u64>()? * 1024))
}
fn main() -> Result<()> {
    let a: Vec<_> = std::env::args().skip(1).collect();
    ensure!(a.len() >= 5, "helper models wav strategy receipt [seconds]");
    let seconds = a
        .get(5)
        .map(|x| x.parse::<usize>())
        .transpose()?
        .unwrap_or(30);
    let mut r = hound::WavReader::open(&a[2])?;
    ensure!(
        r.spec().channels == 1 && r.spec().sample_rate == 16000 && r.spec().bits_per_sample == 16,
        "expected mono PCM16 16kHz"
    );
    let pcm: Vec<_> = r
        .samples::<i16>()
        .take(seconds * 16000)
        .map(|s| s.map(|x| x as f32 / 32768.))
        .collect::<std::result::Result<_, _>>()?;
    let idle_before = cpu_seconds();
    let idle_start = Instant::now();
    std::thread::sleep(Duration::from_secs(3));
    let idle_cpu = cpu_seconds() - idle_before;
    let idle_wall = idle_start.elapsed().as_secs_f64();
    let mut call = CallScopedSpeakers::new(
        PathBuf::from(&a[0]),
        PathBuf::from(&a[1]),
        0,
        NamingFlow::new(EnrollmentStore::default()),
    );
    call.set_strategy(LiveStrategy::parse(&a[3])?)?;
    call.activate("paced".into(), 1, 0., Instant::now())?;
    let pid = call.worker_pid().unwrap();
    let (child_before, _) = child_usage(pid)?;
    let before = cpu_seconds();
    let start = Instant::now();
    let mut fed = 0;
    let mut max_late = 0_f64;
    let mut work = 0.;
    let frames = pcm.len().div_ceil(160);
    let mut previous = vec![0_u8; frames];
    let mut changed = vec![0.; frames];
    let mut first_output_wall = None;
    let update = |segments: Vec<speakers::diar::SpeakerSegment>,
                  wall: f64,
                  previous: &mut Vec<u8>,
                  changed: &mut Vec<f64>| {
        let mut current = vec![0_u8; frames];
        for seg in segments {
            let a = (seg.start_time * 100.).round().max(0.) as usize;
            let b = (seg.end_time * 100.).round().max(0.) as usize;
            for mask in &mut current[a.min(frames)..b.min(frames)] {
                *mask |= 1 << (seg.speaker - 1);
            }
        }
        for i in 0..frames {
            if previous[i] != current[i] {
                changed[i] = wall;
            }
        }
        *previous = current;
    };
    for chunk in pcm.chunks(320) {
        let deadline = start + Duration::from_secs_f64(fed as f64 / 16000.);
        if let Some(wait) = deadline.checked_duration_since(Instant::now()) {
            std::thread::sleep(wait);
        }
        max_late = max_late.max(
            Instant::now()
                .saturating_duration_since(deadline)
                .as_secs_f64(),
        );
        let t = Instant::now();
        call.push(chunk, true, Instant::now())?;
        work += t.elapsed().as_secs_f64();
        fed += chunk.len();
        use speakers::pipeline::Diarization;
        if first_output_wall.is_none()
            && call.pipeline_mut().unwrap().stream_ref().frame_count() > 0
        {
            first_output_wall = Some(start.elapsed().as_secs_f64());
        }
        if fed % 7680 == 0 {
            update(
                call.pipeline_mut().unwrap().segments()?,
                start.elapsed().as_secs_f64(),
                &mut previous,
                &mut changed,
            );
        }
    }
    let t = Instant::now();
    call.pipeline_mut().unwrap().finish()?;
    work += t.elapsed().as_secs_f64();
    update(
        call.pipeline_mut().unwrap().segments()?,
        start.elapsed().as_secs_f64(),
        &mut previous,
        &mut changed,
    );
    let mut latency: Vec<_> = (0..frames)
        .filter(|i| previous[*i] != 0)
        .map(|i| (changed[i] - i as f64 / 100.).max(0.))
        .collect();
    latency.sort_by(f64::total_cmp);
    let stable_p95 = latency
        .get((latency.len() as f64 * 0.95).floor() as usize)
        .copied();
    let elapsed = start.elapsed().as_secs_f64();
    let parent_cpu = cpu_seconds() - before;
    let (child_after, child_rss) = child_usage(pid)?;
    let segments = call.pipeline_mut().unwrap().segments()?.len();
    call.stop()?;
    let reaped = unsafe { libc::kill(pid as i32, 0) } == -1;
    std::fs::write(
        &a[4],
        serde_json::to_vec_pretty(
            &serde_json::json!({"build_profile":if cfg!(debug_assertions) { "debug" } else { "release" },"strategy":call.strategy(),"audio_seconds":fed as f64/16000.,"wall_seconds":elapsed,"parent_cpu_seconds":parent_cpu,"combined_cpu_percent_of_one_core":100.*(parent_cpu+child_after-child_before)/elapsed,"idle_cpu_seconds":idle_cpu,"idle_wall_seconds":idle_wall,"idle_cpu_percent_of_one_core":100.*idle_cpu/idle_wall,"child_cpu_seconds":child_after-child_before,"child_current_rss_bytes":child_rss,"push_work_seconds":work,"push_rtf":work/(fed as f64/16000.),"max_feed_lateness_seconds":max_late,"first_output_wall_seconds":first_output_wall,"stable_label_wall_p95_seconds":stable_p95,"latency_definition":"real-time-paced last sampled active-set change minus frame start; 0.48s resolution; includes compute and revisions, wrong final labels also count stable","segments":segments,"child_pid":pid,"child_reaped":reaped,"power_watts":null,"scope":"real-time-paced offline file via actual helper; parent CPU measured, child ps CPU delta excludes loading (0.01s resolution); no installed capture or power probe"}),
        )?,
    )?;
    ensure!(reaped, "helper not reaped");
    Ok(())
}
