use anyhow::{ensure, Result};
use speakers::{
    diar::Diarizer,
    embedding::{ReDimNet, VoiceEncoder},
};
use std::{path::PathBuf, process::Command};
fn rss() -> Result<u64> {
    let out = Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()?;
    ensure!(out.status.success(), "RSS query failed");
    Ok(std::str::from_utf8(&out.stdout)?.trim().parse::<u64>()? * 1024)
}
fn main() -> Result<()> {
    let models = PathBuf::from(std::env::args().nth(1).unwrap_or("models/speakers".into()));
    let before = rss()?;
    let during;
    {
        let diar = Diarizer::load(&models.join("Nemotron-3-Diarization.q8_0.gguf"), 0)?;
        let mut encoder = ReDimNet::load(&models.join("ReDimNet2B6.onnx"))?;
        let mut wav = hound::WavReader::open(models.join("ami-480.wav"))?;
        let audio = wav
            .samples::<i16>()
            .take(30 * 16000)
            .map(|v| v.map(|x| x as f32 / 32768.))
            .collect::<Result<Vec<_>, _>>()?;
        let mut stream = diar.stream(16000)?;
        for chunk in audio.chunks(320) {
            stream.push_f32(chunk)?;
        }
        stream.finish()?;
        std::hint::black_box(encoder.embed(&audio[..96000])?);
        during = rss()?;
    }
    let dropped = rss()?;
    #[cfg(target_os = "macos")]
    unsafe {
        unsafe extern "C" {
            fn malloc_zone_pressure_relief(zone: *mut libc::c_void, goal: usize) -> usize;
        }
        malloc_zone_pressure_relief(std::ptr::null_mut(), 0);
    }
    let relieved = rss()?;
    std::thread::sleep(std::time::Duration::from_secs(2));
    println!(
        "{}",
        serde_json::json!({"before_bytes":before,"during_bytes":during,"dropped_bytes":dropped,"relieved_bytes":relieved,"after_bytes":rss()?})
    );
    Ok(())
}
