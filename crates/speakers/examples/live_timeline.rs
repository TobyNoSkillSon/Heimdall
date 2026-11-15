//! Records the live diarizer's timeline on a recorded file, for replay tests that need no model (the app's
//! `bridge/call_interrupt.rs` decision-delay test). Audio goes in 20 ms packets as the app feeds a call source, as
//! fast as the helper takes it: every time is supplied-audio time, not wall time, so this is not a speed measurement.
//!
//! `live_timeline HELPER MODELS WAV SECONDS OUT.json`: for each new diarizer frame, the audio supplied so far
//! (`heard_s`), the stable horizon (`stable_s`) and the segments ending within `RECENT_S` of it; then the final full
//! segments.
use anyhow::{ensure, Result};
use speakers::{call::CallScopedSpeakers, identity::EnrollmentStore, naming::NamingFlow, pipeline::Diarization};
use std::{path::PathBuf, time::Instant};

/// Segments that ended longer ago than this are left out of a frame's record (they cannot reach an open voice).
const RECENT_S: f64 = 4.0;

fn round(x: f64) -> f64 {
    (x * 1000.).round() / 1000.
}
fn main() -> Result<()> {
    let a: Vec<_> = std::env::args().skip(1).collect();
    ensure!(a.len() == 5, "live_timeline HELPER MODELS WAV SECONDS OUT.json");
    let seconds: usize = a[3].parse()?;
    let mut r = hound::WavReader::open(&a[2])?;
    ensure!(r.spec().channels == 1 && r.spec().sample_rate == 16000 && r.spec().bits_per_sample == 16, "expected mono PCM16 16kHz");
    let pcm: Vec<f32> = r.samples::<i16>().take(seconds * 16000).map(|s| s.map(|x| x as f32 / 32768.)).collect::<std::result::Result<_, _>>()?;
    let mut call = CallScopedSpeakers::new(PathBuf::from(&a[0]), PathBuf::from(&a[1]), 0, NamingFlow::new(EnrollmentStore::default()));
    call.activate("timeline".into(), 1, 0., Instant::now())?;
    let mut frames = vec![];
    let mut last_frame = -1;
    let mut fed = 0usize;
    for chunk in pcm.chunks(320) {
        call.push(chunk, true, Instant::now())?;
        fed += chunk.len();
        let p = call.pipeline_mut().unwrap();
        let frame = p.stream_ref().frame_count();
        if frame == last_frame {
            continue;
        }
        last_frame = frame;
        let heard_s = fed as f64 / 16000.;
        let stable_s = p.origin_seconds() + p.stream_ref().stable_frames() as f64 * p.stream_ref().seconds_per_frame();
        let segments: Vec<_> = p.segments()?.into_iter().filter(|s| s.end_time >= stable_s.min(heard_s) - RECENT_S)
            .map(|s| serde_json::json!([s.speaker, round(s.start_time), round(s.end_time)])).collect();
        frames.push(serde_json::json!({"heard_s": round(heard_s), "stable_s": round(stable_s), "segments": segments}));
    }
    let p = call.pipeline_mut().unwrap();
    let all: Vec<_> = p.segments()?.into_iter().map(|s| serde_json::json!([s.speaker, round(s.start_time), round(s.end_time)])).collect();
    call.stop()?;
    let out = serde_json::json!({
        "audio": a[2].rsplit('/').next(),
        "seconds": seconds,
        "packet_ms": 20,
        "frames": frames,
        "final_segments": all,
    });
    std::fs::write(&a[4], serde_json::to_vec(&out)?)?;
    eprintln!("{} frames over {} s", out["frames"].as_array().unwrap().len(), seconds);
    Ok(())
}
