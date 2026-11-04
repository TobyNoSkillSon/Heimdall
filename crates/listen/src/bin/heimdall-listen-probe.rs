//! Short agent-operated resident-path probe. No capture, permissions or output IO.
#[cfg(target_os = "macos")]
fn cpu_seconds() -> f64 {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) } != 0 {
        return f64::NAN;
    }
    let u = unsafe { usage.assume_init() };
    u.ru_utime.tv_sec as f64
        + u.ru_utime.tv_usec as f64 / 1e6
        + u.ru_stime.tv_sec as f64
        + u.ru_stime.tv_usec as f64 / 1e6
}
fn main() -> anyhow::Result<()> {
    #[cfg(target_os = "macos")]
    {
        use listen::{AudioChunk, ListenEngine};
        use std::{
            path::Path,
            time::{Duration, Instant},
        };
        let idle = || {
            let start = Instant::now();
            let cpu = cpu_seconds();
            std::thread::sleep(Duration::from_secs(3));
            (start.elapsed().as_secs_f64(), cpu_seconds() - cpu)
        };
        let baseline = idle();
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let models = root.join("../../models");
        let asr = listen::asr::Parakeet::load(&models.join("parakeet-ultra"))?;
        let mut engine = ListenEngine::load(&models, asr)?;
        let resident = idle();
        let audio = listen::file::read_wav(&root.join("tests/fixtures/en-2.wav"))?;
        let mut runs = vec![];
        for _ in 0..4 {
            let start = Instant::now();
            let cpu = cpu_seconds();
            let mut first = None;
            for (i, chunk) in audio.chunks(320).enumerate() {
                let events = engine.push(AudioChunk {
                    source: "probe",
                    generation: 0,
                    host_time_ns: i as u64 * 20_000_000,
                    samples: chunk,
                })?;
                if first.is_none() && events.iter().any(|e|matches!(e,listen::ListenEvent::PartialTranscript{text,..} if !text.is_empty())) {first=Some((start.elapsed().as_secs_f64(),(i+1) as f64*0.020));}
            }
            let _ = engine.flush_source("probe")?;
            runs.push(serde_json::json!({"wall_seconds":start.elapsed().as_secs_f64(),"cpu_seconds":cpu_seconds()-cpu,"rtf":start.elapsed().as_secs_f64()/(audio.len() as f64/16000.0),"first_partial_compute_seconds":first.map(|x|x.0),"first_partial_audio_seconds":first.map(|x|x.1)}));
        }
        println!(
            "{}",
            serde_json::json!({"hardware":"Apple M5 Max","machine_busy":true,"clip":"en-2","audio_seconds":audio.len() as f64/16000.0,"unloaded_idle":{"wall_seconds":baseline.0,"cpu_seconds":baseline.1},"resident_idle":{"wall_seconds":resident.0,"cpu_seconds":resident.1},"runs_first_is_warmup":runs})
        );
    }
    Ok(())
}
