use crate::features::WhisperFeatures;
use anyhow::{ensure, Result};
use ort::{
    session::{builder::GraphOptimizationLevel, Session},
    value::Tensor,
};
use std::path::Path;

pub const VAD_SAMPLES: usize = 512;
#[derive(Clone)]
pub struct VadState {
    pub(crate) hidden: Vec<f32>,
    context: [f32; 64],
}
impl Default for VadState {
    fn default() -> Self {
        Self {
            hidden: vec![0.0; 256],
            context: [0.0; 64],
        }
    }
}
pub(crate) fn session(path: &Path) -> Result<Session> {
    Ok(Session::builder()?
        .with_intra_threads(1)
        .map_err(|e| anyhow::anyhow!(e.to_string()))?
        .with_inter_threads(1)
        .map_err(|e| anyhow::anyhow!(e.to_string()))?
        .with_no_environment_execution_providers()
        .map_err(|e| anyhow::anyhow!(e.to_string()))?
        .with_intra_op_spinning(false)
        .map_err(|e| anyhow::anyhow!(e.to_string()))?
        .with_inter_op_spinning(false)
        .map_err(|e| anyhow::anyhow!(e.to_string()))?
        .with_parallel_execution(false)
        .map_err(|e| anyhow::anyhow!(e.to_string()))?
        .with_optimization_level(GraphOptimizationLevel::Level3)
        .map_err(|e| anyhow::anyhow!(e.to_string()))?
        .commit_from_file(path)?)
}
/// Silero VAD v5 at 16 kHz, computed natively from the pinned ONNX constants
/// (see `silero.rs`); state layout and outputs are the ONNX model's.
pub struct Silero {
    native: crate::silero::Native,
}
impl Silero {
    pub fn load(path: &Path) -> Result<Self> {
        Ok(Self {
            native: crate::silero::Native::load(path)?,
        })
    }
    pub fn probability(&mut self, audio: &[f32; 512], state: &mut VadState) -> Result<f32> {
        let mut input = [0_f32; 576];
        input[..64].copy_from_slice(&state.context);
        input[64..].copy_from_slice(audio);
        let mut next = state.hidden.clone();
        let (h, c) = next.split_at_mut(128);
        let p = self.native.step(&input, h, c);
        ensure!(
            p.is_finite() && (0.0..=1.0).contains(&p) && next.iter().all(|v| v.is_finite()),
            "Invalid VAD probability"
        );
        state.hidden = next;
        state.context.copy_from_slice(&audio[448..]);
        Ok(p)
    }
    /// One shared model across streams. No state advances on a failed inference.
    pub fn probabilities(
        &mut self,
        audio: &[[f32; 512]],
        states: &mut [VadState],
    ) -> Result<Vec<f32>> {
        ensure!(
            (1..=32).contains(&audio.len()) && states.len() == audio.len(),
            "VAD batch bounds"
        );
        let mut next = states.to_vec();
        let p = audio
            .iter()
            .zip(&mut next)
            .map(|(a, s)| self.probability(a, s))
            .collect::<Result<Vec<_>>>()?;
        states.clone_from_slice(&next);
        Ok(p)
    }
}
/// The ONNX Runtime Silero this crate used before the native kernel: the parity
/// reference for tests and the agent-only probe, never the app path.
#[cfg(any(test, feature = "placement-probe"))]
pub struct OrtSilero {
    session: Session,
}
#[cfg(any(test, feature = "placement-probe"))]
impl OrtSilero {
    pub fn load(path: &Path) -> Result<Self> {
        Ok(Self {
            session: session(path)?,
        })
    }
    pub fn probability(&mut self, audio: &[f32; 512], state: &mut VadState) -> Result<f32> {
        let mut input = Vec::with_capacity(576);
        input.extend_from_slice(&state.context);
        input.extend_from_slice(audio);
        let result = self.session.run(ort::inputs![
            "input"=>Tensor::from_array(([1,576],input))?,
            "state"=>Tensor::from_array(([2,1,128],state.hidden.clone()))?,
            "sr"=>Tensor::from_array((Vec::<usize>::new(),vec![16000_i64]))?
        ])?;
        let (_, p) = result["output"].try_extract_tensor::<f32>()?;
        let (_, h) = result["stateN"].try_extract_tensor::<f32>()?;
        ensure!(
            h.len() == 256 && p.len() == 1,
            "Silero output schema changed"
        );
        state.hidden.copy_from_slice(h);
        state.context.copy_from_slice(&audio[448..]);
        Ok(p[0])
    }
}
#[cfg(test)]
mod silero_parity {
    use super::*;
    /// Native and ONNX Runtime Silero over real fixture speech with carried state:
    /// probabilities within 2e-5 and no start/continue (0.4/0.35) decision flips.
    #[test]
    #[ignore = "needs the .wav audio fixtures that are not in the public repository"]
    fn native_matches_onnx_on_fixture_streams() -> Result<()> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let model = root.join("../../models/silero/silero_vad.onnx");
        let (mut native, mut reference) = (Silero::load(&model)?, OrtSilero::load(&model)?);
        for clip in ["en-1", "en-2", "pl-1", "pl-2", "mixed", "continuous"] {
            let audio = crate::file::read_wav(&root.join(format!("tests/fixtures/{clip}.wav")))?;
            let (mut a, mut b) = (VadState::default(), VadState::default());
            for block in audio.as_chunks::<512>().0 {
                let (p, q) = (
                    native.probability(block, &mut a)?,
                    reference.probability(block, &mut b)?,
                );
                assert!((p - q).abs() < 2e-5, "{clip}: {p} vs {q}");
                assert!(
                    [0.35, 0.4].iter().all(|t| (p >= *t) == (q >= *t)),
                    "{clip} flip"
                );
            }
            assert!(a
                .hidden
                .iter()
                .zip(&b.hidden)
                .all(|(x, y)| (x - y).abs() < 1e-3));
        }
        Ok(())
    }
}
/// Agent-only probe: Smart Turn as a compiled Core ML ML Program (fp16), fed the same
/// frontend features. Measured, not shipped (see lab/history/docs/optimisation/small-models-2026-10-13.md).
#[cfg(all(target_os = "macos", feature = "placement-probe"))]
pub struct CoreMlTurn {
    model: crate::coreml::Model,
    input: crate::coreml::Array,
}
#[cfg(all(target_os = "macos", feature = "placement-probe"))]
impl CoreMlTurn {
    /// `units`: 0 CPU, 1 CPU+GPU, 2 all, 3 CPU+Neural Engine (MLComputeUnits).
    pub fn load(path: &Path, units: isize) -> Result<Self> {
        Ok(Self {
            model: crate::coreml::Model::load(path, objc2_core_ml::MLComputeUnits(units))?,
            input: crate::coreml::Array::zeros(
                &[1, 80, 800],
                objc2_core_ml::MLMultiArrayDataType::Float32,
            )?,
        })
    }
    pub fn probability(&mut self, features: &[f32]) -> Result<f32> {
        self.input.copy_f32(features)?;
        let p = self
            .model
            .predict(&[("input_features", &self.input)])?
            .array("probability")?
            .get(0);
        ensure!(
            p.is_finite() && (0.0..=1.0).contains(&p),
            "Invalid Smart Turn probability"
        );
        Ok(p)
    }
}
struct SmartTurnModel {
    session: Option<Session>,
    #[cfg(target_os = "macos")]
    metal: Option<crate::metal::MetalTurn>,
    features: WhisperFeatures,
}
impl SmartTurnModel {
    pub fn load_backend(path: &Path, backend: &str) -> Result<Self> {
        ensure!(
            matches!(backend, "cpu" | "metal"),
            "Unknown turn backend {backend}"
        );
        #[cfg(not(target_os = "macos"))]
        ensure!(backend == "cpu", "Metal requires macOS");
        Ok(Self {
            session: if backend == "cpu" {
                Some(session(path)?)
            } else {
                None
            },
            #[cfg(target_os = "macos")]
            metal: if backend == "metal" {
                Some(crate::metal::MetalTurn::load(path)?)
            } else {
                None
            },
            features: WhisperFeatures::default(),
        })
    }
    pub fn probability(&mut self, audio: &[f32]) -> Result<f32> {
        let input = self.features.extract(audio);
        #[cfg(target_os = "macos")]
        if let Some(gpu) = &mut self.metal {
            return gpu.probability(&input);
        }
        let result = self
            .session
            .as_mut()
            .expect("selected CPU turn backend")
            .run(ort::inputs!["input_features"=>Tensor::from_array(([1,80,800],input))?])?;
        let (_, p) = result[0].try_extract_tensor::<f32>()?;
        ensure!(
            p.len() == 1 && p[0].is_finite() && (0.0..=1.0).contains(&p[0]),
            "Invalid Smart Turn probability"
        );
        Ok(p[0]) // Already sigmoid; no second sigmoid.
    }
}

/// A turn prediction is replayed from immutable audio after a GPU fault.
pub struct SmartTurn {
    state: crate::gpu_recovery::Recovery<SmartTurnModel>,
    path: std::path::PathBuf,
}
impl SmartTurn {
    pub fn load(path: &Path) -> Result<Self> {
        let backend = std::env::var("HEIMDALL_SMART_TURN_BACKEND").unwrap_or_else(|_| "cpu".into());
        Self::load_backend(path, &backend)
    }
    pub fn load_backend(path: &Path, backend: &str) -> Result<Self> {
        ensure!(
            matches!(backend, "cpu" | "metal"),
            "Unknown turn backend {backend}"
        );
        #[cfg(not(target_os = "macos"))]
        ensure!(backend == "cpu", "Metal requires macOS");
        Ok(Self {
            state: crate::gpu_recovery::Recovery::load(backend == "metal", |gpu| {
                SmartTurnModel::load_backend(path, if gpu { "metal" } else { "cpu" })
            })?,
            path: path.to_owned(),
        })
    }
    pub fn probability(&mut self, audio: &[f32]) -> Result<f32> {
        ensure!(audio.iter().all(|v| v.is_finite()), "Non-finite turn audio");
        self.state.run(
            |gpu| SmartTurnModel::load_backend(&self.path, if gpu { "metal" } else { "cpu" }),
            |model| model.probability(audio),
        )
    }
}

#[cfg(all(test, target_os = "macos"))]
mod fault_model_tests {
    use super::*;
    #[test]
    #[ignore = "requires tuned offline turn Metal assets"]
    fn gpu_fault_turn_recreates_falls_back_and_restores() -> Result<()> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let path = root.join("../../models/smart-turn/owner-turn.onnx");
        let audio = crate::file::read_wav(&root.join("tests/fixtures/en-2.wav"))?;
        let mut model = SmartTurn::load_backend(&path, "metal")?;
        let expected = model.probability(&audio)?;
        crate::gpu_recovery::inject(1);
        assert_eq!(model.probability(&audio)?, expected);
        assert!(model.state.is_gpu());
        crate::gpu_recovery::inject(2);
        let actual = model.probability(&audio)?;
        assert!(!model.state.is_gpu());
        let mut cpu = SmartTurn::load_backend(&path, "cpu")?;
        assert_eq!(actual, cpu.probability(&audio)?);
        assert_eq!(model.probability(&audio)?, actual);
        model.state.retry_now();
        assert_eq!(model.probability(&audio)?, expected);
        assert!(model.state.is_gpu());
        Ok(())
    }
}
