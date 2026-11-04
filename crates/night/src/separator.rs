use anyhow::{ensure, Result};
use ort::{
    session::{builder::GraphOptimizationLevel, Session},
    value::Tensor,
};
use std::{io::Read, path::Path};
pub const MODEL_ID: &str =
    "ipsilondev/MossFormer2-SS-16K-ONNX@abd8b1268dfded6b0e972ddae4c2dd6a3faf4f8a";
pub const MODEL_SHA256: &str = "1776c854784a2400feb7582ac4e9f922073cd706cb4fd216c98abad937d642b1";
pub const MAX_SAMPLES: usize = 8 * 16000;
pub struct Separator {
    session: Session,
}
pub fn file_hash(path: &Path) -> Result<String> {
    // Reuse archive's hash for bounded files; model is pinned at 90.9 MB.
    ensure!(
        std::fs::metadata(path)?.len() <= 100_000_000,
        "separator file too large"
    );
    let mut bytes = Vec::new();
    std::fs::File::open(path)?.read_to_end(&mut bytes)?;
    Ok(heimdall_archive::hash(&bytes))
}
impl Separator {
    pub fn load(path: &Path) -> Result<Self> {
        ensure!(
            file_hash(path)? == MODEL_SHA256,
            "separator checksum mismatch"
        );
        let session = Session::builder()?
            .with_intra_threads(2)
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
            .with_memory_pattern(false)
            .map_err(|e| anyhow::anyhow!(e.to_string()))?
            .with_optimization_level(GraphOptimizationLevel::Level3)
            .map_err(|e| anyhow::anyhow!(e.to_string()))?
            .commit_from_file(path)?;
        Ok(Self { session })
    }
    pub fn separate(&mut self, audio: &[f32]) -> Result<[Vec<f32>; 2]> {
        ensure!(
            (8000..=MAX_SAMPLES).contains(&audio.len()) && audio.iter().all(|v| v.is_finite()),
            "separation needs 0.5–8 seconds of finite mono16k PCM"
        );
        // The exported encoder/decoder rounds down to a multiple of eight. Padding
        // belongs inside this model boundary, never in capture or the archived clock.
        let mut padded = audio.to_vec();
        padded.resize(audio.len().div_ceil(8) * 8, 0.);
        let output = self
            .session
            .run(ort::inputs!["inputs"=>Tensor::from_array(([1,padded.len()],padded.clone()))?])?;
        let mut tracks = [Vec::new(), Vec::new()];
        let rms =
            (audio.iter().map(|v| (*v as f64).powi(2)).sum::<f64>() / audio.len() as f64).sqrt();
        for (track, name) in tracks.iter_mut().zip(["spk0", "spk1"]) {
            let (shape, values) = output[name].try_extract_tensor::<f32>()?;
            ensure!(
                shape.as_ref() == [1, padded.len() as i64] && values.iter().all(|v| v.is_finite()),
                "invalid separator output"
            );
            let values = &values[..audio.len()];
            let own = (values.iter().map(|v| (*v as f64).powi(2)).sum::<f64>()
                / values.len() as f64)
                .sqrt();
            // Same RMS renormalization as the pinned export, with exact silence preserved.
            let gain = if own > 1e-12 { rms / own } else { 0. };
            *track = values.iter().map(|v| (*v as f64 * gain) as f32).collect();
            ensure!(
                track.iter().all(|v| v.is_finite()),
                "nonfinite normalized separation"
            );
        }
        Ok(tracks)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn unaligned_pcm_returns_every_original_sample_on_both_tracks() {
        let path=std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../models/night/model.int8.onnx");
        if !path.exists() {eprintln!("pinned separator missing; native check unavailable");return;}
        let mut separator=super::Separator::load(&path).unwrap();
        let audio:Vec<_>=(0..8003).map(|i|(i as f32*0.17).sin()*0.1).collect();
        let tracks=separator.separate(&audio).unwrap();
        assert!(tracks.iter().all(|t|t.len()==8003&&t.iter().all(|v|v.is_finite())));
    }
}
