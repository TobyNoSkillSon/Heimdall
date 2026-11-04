//! Short reviewer memory check. Run in an isolated process with pinned local assets.
use anyhow::Result;
use ort::{
    session::{Session, builder::GraphOptimizationLevel},
    value::Tensor,
};
use speakers::{
    diar::Diarizer,
    embedding::{ReDimNet, VoiceEncoder},
};
use std::{path::PathBuf, process::Command};
fn rss(phase: &str) -> Result<()> {
    let value = Command::new("/bin/ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()?;
    println!(
        "{phase}: current RSS KiB {}",
        String::from_utf8_lossy(&value.stdout).trim()
    );
    Ok(())
}
fn main() -> Result<()> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../models/speakers");
    let audio: Vec<f32> = hound::WavReader::open(root.join("ami-480.wav"))?
        .samples::<i16>()
        .take(96000)
        .map(|s| s.map(|v| v as f32 / 32768.))
        .collect::<Result<_, _>>()?;
    rss("unloaded")?;
    let model = Diarizer::load(&root.join("Nemotron-3-Diarization.q8_0.gguf"), 0)?;
    rss("diarizer loaded")?;
    let mut stream = model.stream(16000)?;
    for chunk in audio[..80000].chunks(320) {
        stream.push_f32(chunk)?;
    }
    stream.finish()?;
    rss("diarizer after 5 seconds")?;
    drop(stream);
    drop(model);
    rss("diarizer dropped")?;
    if std::env::args().any(|a| a == "arena-off") {
        let mut encoder = Session::builder()
            .map_err(|e| anyhow::anyhow!(e.to_string()))?
            .with_intra_threads(1)
            .map_err(|e| anyhow::anyhow!(e.to_string()))?
            .with_inter_threads(1)
            .map_err(|e| anyhow::anyhow!(e.to_string()))?
            .with_intra_op_spinning(false)
            .map_err(|e| anyhow::anyhow!(e.to_string()))?
            .with_inter_op_spinning(false)
            .map_err(|e| anyhow::anyhow!(e.to_string()))?
            .with_parallel_execution(false)
            .map_err(|e| anyhow::anyhow!(e.to_string()))?
            .with_no_environment_execution_providers()
            .map_err(|e| anyhow::anyhow!(e.to_string()))?
            .with_execution_providers([ort::ep::CPU::default().with_arena_allocator(false).build()])
            .map_err(|e| anyhow::anyhow!(e.to_string()))?
            .with_optimization_level(GraphOptimizationLevel::Level3)
            .map_err(|e| anyhow::anyhow!(e.to_string()))?
            .commit_from_file(root.join("ReDimNet2B6.onnx"))
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        rss("encoder arena-off loaded")?;
        let now = std::time::Instant::now();
        {
            let output = encoder
                .run(ort::inputs!["audio" => Tensor::from_array(([1, 96000], audio.clone()))?])?;
            let (_, values) = output["embedding"].try_extract_tensor::<f32>()?;
            let baseline: serde_json::Value = serde_json::from_slice(&std::fs::read(
                root.join("../../target/review-speakers/raw-480.json"),
            )?)?;
            let reference = baseline["embedding"].as_array().unwrap();
            let difference = values
                .iter()
                .zip(reference)
                .map(|(a, b)| (*a - b.as_f64().unwrap() as f32).abs())
                .fold(0_f32, f32::max);
            println!(
                "arena-off inference seconds {}; max embedding difference {}",
                now.elapsed().as_secs_f64(),
                difference
            );
            assert!(difference < 1e-6);
        }
        rss("encoder arena-off after one embedding")?;
        drop(encoder);
        rss("encoder arena-off dropped")?;
        return Ok(());
    }
    let mut encoder = ReDimNet::load(&root.join("ReDimNet2B6.onnx"))?;
    rss("encoder loaded")?;
    encoder.embed(&audio)?;
    rss("encoder after one embedding")?;
    drop(encoder);
    rss("encoder dropped")?;
    Ok(())
}
