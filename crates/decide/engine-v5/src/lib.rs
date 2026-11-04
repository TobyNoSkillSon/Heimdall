//! d1v5-engine: the Decider V5 (a LiquidAI d1-2B fine-tune) as a library. A text state and one question in, each
//! option's probability out, in one forward pass read at the answer slot.
//!
//! - `prompt`: the exact token ids d1-2B's own runner builds (guarded against prep.py).
//! - `model`: the text trunk in plain MLX (an op-for-op port of lab/decider-v5/d1_2b.py `scores`).
//! - `Engine`: both, plus the checkpoint's calibration temperature; `Backend` picks the forward path.
//! - `ffi`: the C ABI in the shape of Heimdall's decider dylib (`heimdall_d1_*`).
//! - `cache`: MLX's buffer cache capped and cleared, so the process holds the weights, not dead buffers.

pub mod cache;
pub mod ffi;
pub mod fused;
pub mod mem;
pub mod metal;
pub mod model;
pub mod mpp;
pub mod qmm;
pub mod prompt;

use anyhow::{Context, Result};
use serde_json::Value;
use std::cell::RefCell;
use std::path::{Path, PathBuf};

pub use model::Opts;
pub use prompt::{Encoded, Prompt, Question};

/// The forward path. `Plain` is the plain-MLX port, which runs on any Apple-silicon Mac MLX supports (M3, M4, M5
/// and others) and is the permanent fallback. `Fast` is the same model with speed options (`Opts`) on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Plain,
    Fast(Opts),
}

impl Backend {
    /// From the environment (`Opts::from_env`): the fast path by default, `Plain` under D1E_FALLBACK=1 or when
    /// D1E_FAST=0 and no option switch is on.
    pub fn select() -> Self {
        let opts = Opts::from_env();
        if opts.any() { Backend::Fast(opts) } else { Backend::Plain }
    }

    pub fn opts(self) -> Opts {
        match self {
            Backend::Plain => Opts::plain(),
            Backend::Fast(o) => o,
        }
    }

    pub fn name(self) -> String {
        match self {
            Backend::Plain => "plain-mlx".into(),
            Backend::Fast(o) => format!("mlx+{}", o.names().join("+")),
        }
    }
}

pub fn fallback_forced() -> bool {
    std::env::var("D1E_FALLBACK").map(|v| v == "1").unwrap_or(false)
}

/// calibration.json: a default temperature and optional per-bucket ones (calibration.Calibration.temperature).
#[derive(Debug, Clone)]
pub struct Calibration {
    pub default: f64,
    pub temperatures: serde_json::Map<String, Value>,
}

impl Calibration {
    pub fn load(dir: &Path) -> Result<Self> {
        let path = dir.join("calibration.json");
        if !path.exists() {
            return Ok(Self { default: 1.0, temperatures: Default::default() });
        }
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&path)?)?;
        Ok(Self {
            default: v.get("default").and_then(Value::as_f64).unwrap_or(1.0),
            temperatures: v.get("temperatures").and_then(Value::as_object).cloned().unwrap_or_default(),
        })
    }

    pub fn temperature(&self, q: &Question) -> f64 {
        self.temperatures.get(&q.bucket()).and_then(Value::as_f64).unwrap_or(self.default)
    }
}

/// prompt.probabilities: the scores through the temperature, as a softmax (float64, as the reference computes it).
pub fn softmax(scores: &[f32], temperature: f64) -> Vec<f32> {
    let s: Vec<f64> = scores.iter().map(|&x| if temperature == 1.0 { x as f64 } else { x as f64 / temperature }).collect();
    let m = s.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let ex: Vec<f64> = s.iter().map(|x| (x - m).exp()).collect();
    let total: f64 = ex.iter().sum();
    ex.iter().map(|x| (x / total) as f32).collect()
}

pub struct Engine {
    pub prompt: Prompt,
    model: RefCell<Option<model::Model>>,
    pub calibration: Calibration,
    /// the path asked for (environment) and the one running (options that failed to build or run are off)
    pub requested: Backend,
    backend: RefCell<Backend>,
    /// why options were turned off, in order
    notes: RefCell<Vec<String>>,
    dir: PathBuf,
    /// D1E_STATE_TOKENS: the state cut to this many tokens before the prompt is built (lossy; off by default)
    pub state_tokens: Option<usize>,
    /// the cache cap during a forward that dequantizes weights where used (cache::forward_limit, read at load)
    forward_cache: Option<usize>,
}

impl Engine {
    /// A d1-2B checkpoint folder: config.json, tokenizer files, model.safetensors, calibration.json. The backend
    /// from the environment (`Backend::select`).
    pub fn load(dir: impl AsRef<Path>) -> Result<Self> {
        Self::load_with(dir, Backend::select())
    }

    pub fn load_with(dir: impl AsRef<Path>, requested: Backend) -> Result<Self> {
        let dir = dir.as_ref();
        cache::apply()?;
        let prompt = Prompt::load(dir).with_context(|| format!("prompt from {}", dir.display()))?;
        let m = model::Model::load(dir, requested.opts()).with_context(|| format!("weights from {}", dir.display()))?;
        let running = if m.opts.any() { Backend::Fast(m.opts) } else { Backend::Plain };
        let notes = m.dropped.clone();
        for n in &notes {
            eprintln!("d1v5-engine: option off: {n}");
        }
        let calibration = Calibration::load(dir)?;
        // the load's weight transforms (fuse, interleave) freed the original tensors into the cache
        cache::release();
        Ok(Self { prompt, model: RefCell::new(Some(m)), calibration, requested, backend: RefCell::new(running),
                  notes: RefCell::new(notes), dir: dir.to_path_buf(),
                  state_tokens: std::env::var("D1E_STATE_TOKENS").ok().and_then(|v| v.parse().ok()).filter(|n| *n > 0),
                  forward_cache: cache::forward_limit() })
    }

    /// The linear layers' storage of the loaded model: "bf16", "fp16", "int8", "int4" or "mixed".
    pub fn precision(&self) -> String {
        self.model.borrow().as_ref().map(|m| m.precision()).unwrap_or_default()
    }

    /// The path running now.
    pub fn backend(&self) -> Backend {
        *self.backend.borrow()
    }

    /// The largest |activation| seen so far when probing (D1E_PROBE_MAX=1), else None.
    pub fn max_activation(&self) -> Option<f32> {
        let m = self.model.borrow();
        let m = m.as_ref()?;
        std::env::var("D1E_PROBE_MAX").is_ok_and(|v| v == "1").then(|| m.max_seen.get())
    }

    /// D1E_PROFILE=1: the last call's [graph build and launch, trunk eval, readout] in ms.
    pub fn profile(&self) -> Option<[f64; 3]> {
        self.model.borrow().as_ref()?.profile.get()
    }

    /// Why requested options are off (failed to build, or failed at run time and fell back to plain).
    pub fn notes(&self) -> Vec<String> {
        self.notes.borrow().clone()
    }

    /// The prompt's ids and each option's token forms.
    pub fn encode(&self, state: &str, q: &Question) -> Result<Encoded> {
        match self.state_tokens {
            Some(n) => self.prompt.encode(&self.prompt.cap_state(state, n)?, q),
            None => self.prompt.encode(state, q),
        }
    }

    /// Raw option scores (temperature 1) for an encoded row. When an optimised path fails, the engine reloads the
    /// plain path from the checkpoint and answers on it from then on.
    pub fn scores(&self, enc: &Encoded) -> Result<Vec<f32>> {
        let first = {
            let m = self.model.borrow();
            let m = m.as_ref().context("no model loaded")?;
            // quantized weights dequantized where used: their buffers recycle through a larger cache for this forward
            let cap = if m.opts.qdeq { self.forward_cache } else { None };
            cache::with_forward_limit(cap, || m.scores(&enc.ids, enc.slot, &enc.groups))
        };
        match first {
            Ok(s) => Ok(s),
            Err(err) if self.backend() != Backend::Plain => {
                let note = format!("{} failed at run time, now plain: {err:#}", self.backend().name());
                eprintln!("d1v5-engine: {note}");
                self.notes.borrow_mut().push(note);
                *self.model.borrow_mut() = None; // free the optimised weights before loading the plain ones
                let _ = mlx_rs::memory::clear_cache();
                let plain = model::Model::load(&self.dir, Opts::plain())?;
                cache::release();
                *self.model.borrow_mut() = Some(plain);
                *self.backend.borrow_mut() = Backend::Plain;
                self.model.borrow().as_ref().unwrap().scores(&enc.ids, enc.slot, &enc.groups)
            }
            Err(err) => Err(err),
        }
    }
    /// Option names and raw scores for a state and a parsed question, and the tokens read.
    pub fn decide_question(&self, state: &str, q: &Question) -> Result<(Vec<(String, f32)>, usize)> {
        let enc = self.encode(state, q)?;
        let scores = self.scores(&enc)?;
        let probs = softmax(&scores, self.calibration.temperature(q));
        Ok((enc.names.into_iter().zip(probs).collect(), enc.ids.len()))
    }

    /// Each option's calibrated probability, in the question's option order. `question_json`: the API's question
    /// JSON ({"type": "choice", "instructions": .., "criteria": {name: description}}, or a noul).
    pub fn decide(&self, state: &str, question_json: &str) -> Result<Vec<(String, f32)>> {
        let q: Value = serde_json::from_str(question_json).context("question JSON")?;
        let q = Question::from_json(&q)?;
        Ok(self.decide_question(state, &q)?.0)
    }
}
