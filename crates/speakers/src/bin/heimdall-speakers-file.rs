use anyhow::{ensure, Result};
use speakers::{
    diar::Diarizer,
    embedding::{ReDimNet, VoiceEncoder},
};
use std::{path::PathBuf, time::Instant};
fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    ensure!(
        args.len() >= 3,
        "usage: heimdall-speakers-file <models-dir> <wav> <output-json> [seconds] [cpu]"
    );
    let models = PathBuf::from(&args[0]);
    let mut wav = hound::WavReader::open(&args[1])?;
    let spec = wav.spec();
    ensure!(
        spec.channels == 1 && spec.sample_rate == 16000 && spec.bits_per_sample == 16,
        "expected mono 16-kHz PCM16"
    );
    let limit = args
        .get(3)
        .map(|v| v.parse::<usize>())
        .transpose()?
        .unwrap_or(usize::MAX / 16000)
        * 16000;
    let audio: Vec<f32> = wav
        .samples::<i16>()
        .take(limit)
        .map(|v| v.map(|s| s as f32 / 32768.))
        .collect::<std::result::Result<_, _>>()?;
    let loaded = Instant::now();
    let diar = Diarizer::load(
        &models.join("Nemotron-3-Diarization.q8_0.gguf"),
        if args.get(4).is_some_and(|s| s == "cpu") {
            -1
        } else {
            0
        },
    )?;
    let mut encoder = ReDimNet::load(&models.join("ReDimNet2B6.onnx"))?;
    let load_seconds = loaded.elapsed().as_secs_f64();
    let mut stream = diar.stream(16000)?;
    let start = Instant::now();
    let mut first_result_audio = None;
    let mut fed = 0;
    for chunk in audio.chunks(320) {
        stream.push_f32(chunk)?;
        fed += chunk.len();
        if first_result_audio.is_none() && stream.frame_count() > 0 {
            first_result_audio = Some(fed as f64 / 16000.);
        }
    }
    stream.finish()?;
    let processing_seconds = start.elapsed().as_secs_f64();
    let segments = stream.completed_segments()?;
    if let Some(path) = args.get(5) {
        let probs = stream.frame_probabilities()?;
        let bytes: Vec<_> = probs.values.iter().flat_map(|v| v.to_le_bytes()).collect();
        std::fs::write(path, bytes)?;
    }
    let embed_start = Instant::now();
    let embedding = if audio.len() >= 96000 {
        Some(encoder.embed(&audio[..96000])?)
    } else {
        None
    };
    let embedding_seconds = embed_start.elapsed().as_secs_f64();
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
    #[cfg(target_os = "macos")]
    let peak_bytes = usage.ru_maxrss as u64;
    #[cfg(not(target_os = "macos"))]
    let peak_bytes = usage.ru_maxrss as u64 * 1024;
    let value = serde_json::json!({"model":"Nemotron 3 Q8_0", "preset":"v3-streaming with published ultra-low-latency overrides","postprocessing":"0.5 activity, no pads/gap filling/minimum duration", "native_frame_seconds":diar.seconds_per_frame,"capacity":diar.speakers,"audio_seconds":audio.len() as f64/16000.,"load_seconds":load_seconds,"processing_seconds":processing_seconds,"rtf":processing_seconds/(audio.len() as f64/16000.),"first_result_audio_seconds":first_result_audio,"peak_memory_bytes":peak_bytes,"embedding_seconds":embedding_seconds,"embedding":embedding,"segments":segments});
    std::fs::write(&args[2], serde_json::to_vec_pretty(&value)?)?;
    println!(
        "{}",
        serde_json::to_string(
            &serde_json::json!({"audio_seconds":value["audio_seconds"],"rtf":value["rtf"],"peak_memory_bytes":peak_bytes,"first_result_audio_seconds":first_result_audio,"segments":segments.len()})
        )?
    );
    Ok(())
}
