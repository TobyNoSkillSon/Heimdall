use anyhow::{Result, ensure};
use ort::{
    session::{Session, builder::GraphOptimizationLevel},
    value::Tensor,
};
use serde::{Deserialize, Serialize};
use std::path::Path;

pub const DIMENSION: usize = 192;
pub const SAMPLE_RATE: usize = 16000;
pub const MIN_SAMPLES: usize = 2 * SAMPLE_RATE;
pub const WINDOW_SAMPLES: usize = 6 * SAMPLE_RATE;
pub const MODEL_ID: &str =
    "redimnet2-b6-vb2+vox2-lm:d960e9f713eaba712f44498e82221b6fc12fcd7eaacd367c9eac45d3a6ce59ec";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Embedding(Vec<f32>);
impl Embedding {
    pub fn new(mut values: Vec<f32>) -> Result<Self> {
        ensure!(
            values.len() == DIMENSION && values.iter().all(|v| v.is_finite()),
            "invalid speaker embedding"
        );
        let norm = values.iter().map(|v| v * v).sum::<f32>().sqrt();
        ensure!(norm.is_finite() && norm > 0., "zero speaker embedding");
        for v in &mut values {
            *v /= norm;
        }
        Ok(Self(values))
    }
    pub fn values(&self) -> &[f32] {
        &self.0
    }
    pub fn cosine(&self, other: &Self) -> f32 {
        self.0
            .iter()
            .zip(&other.0)
            .map(|(a, b)| a * b)
            .sum::<f32>()
            .clamp(-1., 1.)
    }
    pub(crate) fn validate(&self) -> Result<()> {
        ensure!(
            self.0.len() == DIMENSION && self.0.iter().all(|v| v.is_finite()),
            "invalid stored embedding"
        );
        let norm = self.0.iter().map(|v| v * v).sum::<f32>();
        ensure!(
            (norm - 1.).abs() < 0.001,
            "stored embedding is not unit-normalized"
        );
        Ok(())
    }
}
pub trait VoiceEncoder {
    fn embed(&mut self, clean_pcm_16k: &[f32]) -> Result<Embedding>;
}
pub struct ReDimNet {
    session: crate::gpu_recovery::Recovery<Session>,
    path: std::path::PathBuf,
}
impl ReDimNet {
    /// Sliding-window experiment: repeat short speech to the pinned six-second input.
    /// This does not carry the measured six-second identity calibration guarantee.
    pub fn embed_short(&mut self, audio: &[f32]) -> Result<Embedding> {
        ensure!(
            audio.len() >= SAMPLE_RATE / 2 && audio.iter().all(|v| v.is_finite()),
            "need half a second of finite PCM"
        );
        let input: Vec<_> = (0..WINDOW_SAMPLES)
            .map(|i| audio[i % audio.len()])
            .collect();
        self.infer(input)
    }
    fn infer(&mut self, input: Vec<f32>) -> Result<Embedding> {
        self.session.run(|gpu| Self::session(&self.path, gpu), |session| {
            let output = session.run(ort::inputs!["audio"=>Tensor::from_array(([1, WINDOW_SAMPLES], input.clone()))?])?;
            let (_, values) = output["embedding"].try_extract_tensor::<f32>()?;
            Embedding::new(values.to_vec())
        })
    }
    pub fn load(path: &Path) -> Result<Self> {
        crate::artifacts::verify(
            path,
            51223453,
            "d960e9f713eaba712f44498e82221b6fc12fcd7eaacd367c9eac45d3a6ce59ec",
        )?;
        #[cfg(target_os = "macos")]
        let gpu = crate::metal::enabled("redim");
        #[cfg(not(target_os = "macos"))]
        let gpu = false;
        Ok(Self {
            session: crate::gpu_recovery::Recovery::new(Self::session(path, gpu)?, gpu),
            path: path.into(),
        })
    }
    fn session(path: &Path, gpu: bool) -> Result<Session> {
        #[cfg(target_os = "macos")]
        if gpu {
            return crate::metal::session(path);
        }
        Ok(Session::builder()?
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
            .with_memory_pattern(false)
            .map_err(|e| anyhow::anyhow!(e.to_string()))?
            .with_optimization_level(GraphOptimizationLevel::Level3)
            .map_err(|e| anyhow::anyhow!(e.to_string()))?
            .commit_from_file(path)?)
    }
}
/// Match the pinned export: repeat 2–6 s clean speech; center crop >6 s. Never concatenate voices.
pub fn model_window(audio: &[f32]) -> Result<Vec<f32>> {
    ensure!(
        audio.len() >= MIN_SAMPLES && audio.iter().all(|v| v.is_finite()),
        "need at least two seconds of finite clean PCM"
    );
    if audio.len() >= WINDOW_SAMPLES {
        let start = (audio.len() - WINDOW_SAMPLES) / 2;
        return Ok(audio[start..start + WINDOW_SAMPLES].to_vec());
    }
    Ok((0..WINDOW_SAMPLES)
        .map(|i| audio[i % audio.len()])
        .collect())
}
impl VoiceEncoder for ReDimNet {
    fn embed(&mut self, audio: &[f32]) -> Result<Embedding> {
        let input = model_window(audio)?;
        self.infer(input)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn window_contract() {
        assert!(model_window(&vec![1.; MIN_SAMPLES - 1]).is_err());
        let a: Vec<_> = (0..MIN_SAMPLES).map(|v| v as f32).collect();
        let w = model_window(&a).unwrap();
        assert_eq!(w.len(), WINDOW_SAMPLES);
        assert_eq!(w[MIN_SAMPLES], 0.);
        let a: Vec<_> = (0..WINDOW_SAMPLES + 10).map(|v| v as f32).collect();
        assert_eq!(model_window(&a).unwrap()[0], 5.);
        assert!(Embedding::new(vec![0.; DIMENSION]).is_err());
    }
}

#[cfg(all(test, target_os = "macos"))]
mod fault_model_tests {
    use super::*;
    #[test]
    #[ignore = "requires pinned offline ReDimNet assets"]
    fn gpu_fault_redim_recreates_falls_back_and_recovers() -> Result<()> {
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../models/speakers/ReDimNet2B6.onnx");
        let mut model = ReDimNet {
            session: crate::gpu_recovery::Recovery::new(ReDimNet::session(&path, true)?, true),
            path,
        };
        let audio: Vec<_> = (0..WINDOW_SAMPLES)
            .map(|i| (i as f32 * 0.03).sin() * 0.1)
            .collect();
        let reference = model.infer(audio.clone())?;
        for faults in [1, 2, 0] {
            if faults == 0 {
                model.session.retry_now();
            }
            crate::gpu_recovery::inject(faults);
            assert!(model.infer(audio.clone())?.cosine(&reference) > 0.999);
            assert_eq!(model.session.is_gpu(), faults != 2);
        }
        Ok(())
    }
}
