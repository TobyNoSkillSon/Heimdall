use anyhow::{bail, Context, Result};
use listen::{AudioChunk, ListenEngine, ListenEvent, Recognizer};
use std::{path::PathBuf, time::Instant};
fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let mut paths = vec![];
    let mut models = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../models");
    let mut whole = false;
    let mut realtime = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--models" => models = PathBuf::from(args.next().context("Missing model directory")?),
            "--whole" => whole = true,
            "--realtime" => realtime = true,
            _ => paths.push(PathBuf::from(arg)),
        }
    }
    if paths.is_empty() {
        bail!("Usage: heimdall-listen-file WAV... [--models DIR] [--whole] [--realtime]");
    }
    #[cfg(not(target_os = "macos"))]
    bail!("Parakeet Core ML requires macOS");
    #[cfg(target_os = "macos")]
    {
        let load = Instant::now();
        let mut asr = listen::asr::Parakeet::load(&models.join("parakeet-ultra"))?;
        eprintln!(
            "load_seconds={:.6} backend={}; preprocessor=CPUOnly(0)",
            load.elapsed().as_secs_f64(),
            asr.backend()
        );
        if whole {
            for path in paths {
                let audio = listen::file::read_wav(&path)?;
                let start = Instant::now();
                let transcript = asr.transcribe(&audio, 0)?;
                println!("{}", serde_json::to_string(&transcript)?);
                eprintln!(
                    "file={} audio_seconds={:.3} decode_seconds={:.6} rtf={:.6}",
                    path.display(),
                    audio.len() as f64 / 16000.0,
                    start.elapsed().as_secs_f64(),
                    start.elapsed().as_secs_f64() / (audio.len() as f64 / 16000.0)
                );
            }
        } else {
            let mut engine = ListenEngine::load(&models, asr)?;
            for path in paths {
                let audio = listen::file::read_wav(&path)?;
                let start = Instant::now();
                let mut first = false;
                let source = format!("mic:{}", path.to_string_lossy());
                for (i, chunk) in audio.chunks(320).enumerate() {
                    if realtime {
                        let target = std::time::Duration::from_secs_f64(i as f64 * 0.020);
                        if target > start.elapsed() {
                            std::thread::sleep(target - start.elapsed());
                        }
                    }
                    for event in engine.push(AudioChunk {
                        source: &source,
                        generation: 0,
                        host_time_ns: i as u64 * 20_000_000,
                        samples: chunk,
                    })? {
                        if matches!(&event,ListenEvent::PartialTranscript{text,..} if !text.is_empty())
                            && !first
                        {
                            eprintln!(
                                "first_partial_seconds={:.6} audio_received_seconds={:.3}",
                                start.elapsed().as_secs_f64(),
                                (i + 1) as f64 * 0.020
                            );
                            first = true;
                        }
                        println!("{}", serde_json::to_string(&event)?);
                    }
                }
                for event in engine.flush_source(&source)? {
                    println!("{}", serde_json::to_string(&event)?);
                }
                eprintln!(
                    "file={} audio_seconds={:.3} engine_seconds={:.6} rtf={:.6} realtime_replay={}",
                    path.display(),
                    audio.len() as f64 / 16000.0,
                    start.elapsed().as_secs_f64(),
                    start.elapsed().as_secs_f64() / (audio.len() as f64 / 16000.0),
                    realtime
                );
            }
        }
    }
    Ok(())
}
