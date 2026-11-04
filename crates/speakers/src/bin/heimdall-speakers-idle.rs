use anyhow::Result;
use speakers::{diar::Diarizer, embedding::ReDimNet};
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};
fn cpu_seconds() -> f64 {
    let mut r: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut r) };
    r.ru_utime.tv_sec as f64
        + r.ru_stime.tv_sec as f64
        + (r.ru_utime.tv_usec + r.ru_stime.tv_usec) as f64 / 1e6
}
fn idle() -> serde_json::Value {
    let cpu = cpu_seconds();
    let time = Instant::now();
    std::thread::sleep(Duration::from_secs(3));
    serde_json::json!({"wall_seconds":time.elapsed().as_secs_f64(),"process_cpu_seconds":cpu_seconds()-cpu})
}
fn main() -> Result<()> {
    let root = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("models/speakers"));
    let unloaded = idle();
    let diar = Diarizer::load(&root.join("Nemotron-3-Diarization.q8_0.gguf"), 0)?;
    let encoder = ReDimNet::load(&root.join("ReDimNet2B6.onnx"))?;
    let resident = idle();
    std::hint::black_box((diar, encoder));
    println!(
        "{}",
        serde_json::json!({"unloaded":unloaded,"resident":resident,"power_watts":null,"note":"Resident models, no audio/predictions, shared busy Mac. This measures process CPU only, not ANE/GPU driver or system energy."})
    );
    Ok(())
}
