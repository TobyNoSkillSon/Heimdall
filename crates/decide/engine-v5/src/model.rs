//! d1-2B's text trunk (LFM3) in MLX, read at the answer slot.
//!
//! The plain path (`Opts::plain()`) is an op-for-op port of lab/decider-v5/d1_2b.py `scores()` on its plain path (no
//! OPTS), for one unpadded row: BF16 throughout as the checkpoint stores it; RoPE in float32 and cast back, as the
//! reference does; full-attention layers see the whole row (no mask); sliding layers are causal within the window.
//!
//! Speed options (`Opts`, each behind its own env switch, all composable):
//! - `prune`: after the last full-attention layer only the answer slot's receptive field is computed: that layer's
//!   queries, output and FFN for the last `1 + (k-1) * convs_after` positions (keys and values from every token), and
//!   the conv layers after it on those positions only.
//! - `fuse`: q, k, v and gate as one weight; w1 and w3 as one weight (one matmul each).
//! - `compile`: the whole trunk as one compiled MLX graph, the row padded to a multiple of 128 (pad keys masked out of
//!   the full-attention layers; padding sits after every real token, so causal and sliding layers never see it).
//! - `metal`: the short-conv block (B*x through the causal depthwise conv, times C) as one custom Metal kernel.
//! - `q8`: 8-bit affine weights (group 64) for every linear layer, through quantized_matmul.
//!
//! Quantized checkpoints (an MLX `quantization` block in config.json, `.scales`/`.biases` beside each quantized weight,
//! keys without the language-model prefix) load as stored: every quantized linear runs through quantized_matmul at
//! its own bit width (read from the tensor shapes and checked against config.json), and a quantized embedding is
//! dequantized row by row where it is read. `qdeq` (D1E_QDEQ) instead dequantizes each weight where it is used, for
//! prefill-sized inputs only, and runs the dense GEMM (the tensor-op one when it is on); see `Ctx::lin_rows`.

use crate::metal::ConvKernel;
use anyhow::{anyhow, bail, Context, Result};
use mlx_rs::{error::Exception, fast, nn, ops, Array, Dtype};
use serde_json::Value;
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;
use std::rc::Rc;

const PREFIX: &str = "model.language_model.";
pub const PAD_MULTIPLE: i32 = 32;
const Q_GROUP: i32 = 64;
const Q_BITS: i32 = 8;
/// D1E_QDEQ: inputs of at least this many rows dequantize the weight and run the dense GEMM; fewer rows (the pruned
/// tail's 7) stay on quantized_matmul, which reads the packed weight once.
const QDEQ_MIN_ROWS: i32 = 33;

type R<T> = std::result::Result<T, Exception>;
type W = HashMap<String, Array>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Conv,
    Full,
    Sliding,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub hidden: i32,
    pub layers: Vec<Kind>,
    pub heads: i32,
    pub kv: i32,
    pub hd: i32,
    pub eps: f32,
    pub window: i32,
    pub kernel: i32,
    pub pad_id: i32,
    /// (theta, partial_rotary_factor) for full and sliding attention
    pub rope_full: (f64, f64),
    pub rope_sliding: (f64, f64),
}

impl Config {
    /// config.json text_config, as d1_2b.load_config reads it.
    pub fn load(dir: &Path) -> Result<Self> {
        let c: Value = serde_json::from_str(&std::fs::read_to_string(dir.join("config.json"))?)?;
        let t = c.get("text_config").context("config.json has no text_config")?;
        let int = |k: &str| -> Result<i32> { Ok(t[k].as_i64().with_context(|| format!("text_config.{k}"))? as i32) };
        let layers = t["layer_types"]
            .as_array()
            .context("layer_types")?
            .iter()
            .map(|v| match v.as_str() {
                Some("conv") => Ok(Kind::Conv),
                Some("full_attention") => Ok(Kind::Full),
                Some("sliding_attention") => Ok(Kind::Sliding),
                other => bail!("unknown layer type {other:?}"),
            })
            .collect::<Result<Vec<_>>>()?;
        let rope = |k: &str| -> Result<(f64, f64)> {
            let r = &t["rope_parameters"][k];
            Ok((
                r["rope_theta"].as_f64().with_context(|| format!("rope_parameters.{k}.rope_theta"))?,
                r.get("partial_rotary_factor").and_then(Value::as_f64).unwrap_or(1.0),
            ))
        };
        Ok(Self {
            hidden: int("hidden_size")?,
            layers,
            heads: int("num_attention_heads")?,
            kv: int("num_key_value_heads")?,
            hd: int("head_dim")?,
            eps: t["norm_eps"].as_f64().context("norm_eps")? as f32,
            window: int("sliding_window")?,
            kernel: int("conv_L_cache")?,
            pad_id: t.get("pad_token_id").and_then(Value::as_i64).unwrap_or(0) as i32,
            rope_full: rope("full_attention")?,
            rope_sliding: rope("sliding_attention")?,
        })
    }

    fn last_full(&self) -> Option<usize> {
        self.layers.iter().rposition(|k| *k == Kind::Full)
    }

    /// Positions the answer slot depends on after the last full-attention layer, when only conv layers follow it.
    pub fn prune_keep(&self) -> Option<usize> {
        let last = self.last_full()?;
        let after = &self.layers[last + 1..];
        if after.iter().any(|k| *k != Kind::Conv) {
            return None;
        }
        Some(1 + (self.kernel as usize - 1) * after.len())
    }
}

/// Which speed options are on. All off is the plain path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Opts {
    pub prune: bool,
    pub fuse: bool,
    pub compile: bool,
    pub metal: bool,
    pub q8: bool,
    pub fp16: bool,
    /// block-local sliding attention: each window-sized query block against its own and the previous key block
    pub window: bool,
    /// linear layers through Metal 4 tensor ops (M5 and later)
    pub tensor_mm: bool,
    /// SwiGLU in the w1/w3 tensor-op GEMM's epilogue
    pub swiglu: bool,
    /// residual add fused with the RMSNorms around it
    pub addnorm: bool,
    /// q/k RMSNorm + RoPE + head-major layout in one kernel
    pub qkprep: bool,
    /// quantized weights: dequantized where they are used for prefill-sized inputs, then the dense GEMM
    pub qdeq: bool,
    /// quantized weights: a tensor-op GEMM that dequantizes tiles into threadgroup memory (prefill-sized inputs)
    pub qtmm: bool,
}

impl Opts {
    pub fn plain() -> Self {
        Self::default()
    }

    /// The kept fast path: prune, fuse, compile and metal.
    pub fn fast() -> Self {
        Self { prune: true, fuse: true, compile: true, metal: true, q8: false, fp16: false, window: true, tensor_mm: true,
               swiglu: true, addnorm: true, qkprep: true, qdeq: true, qtmm: false }
    }

    /// From the environment. D1E_FALLBACK=1 forces the plain path. Otherwise the fast path (`Opts::fast`) is the
    /// default and D1E_FAST=0 starts from plain instead; then each switch (D1E_PRUNE, D1E_FUSE, D1E_COMPILE,
    /// D1E_METAL, D1E_Q8, D1E_FP16) set to "1" turns its option on and "0" off.
    pub fn from_env() -> Self {
        let var = |k: &str| std::env::var(k).ok();
        if var("D1E_FALLBACK").as_deref() == Some("1") {
            return Self::plain();
        }
        let base = if var("D1E_FAST").as_deref() == Some("0") { Self::plain() } else { Self::fast() };
        let flag = |k: &str, default: bool| match var(k).as_deref() {
            Some("1") => true,
            Some("0") => false,
            _ => default,
        };
        Self { prune: flag("D1E_PRUNE", base.prune), fuse: flag("D1E_FUSE", base.fuse),
               compile: flag("D1E_COMPILE", base.compile), metal: flag("D1E_METAL", base.metal),
               q8: flag("D1E_Q8", base.q8), fp16: flag("D1E_FP16", base.fp16),
               window: flag("D1E_WINDOW", base.window),
               // D1E_TENSOR_MM: "1"/"all", "0", or a comma list of weight-name suffixes (on, for those only)
               tensor_mm: match var("D1E_TENSOR_MM").as_deref() { Some("0") => false, Some(_) => true, None => base.tensor_mm },
               swiglu: flag("D1E_SWIGLU", base.swiglu), addnorm: flag("D1E_ADDNORM", base.addnorm),
               qkprep: flag("D1E_QKPREP", base.qkprep), qdeq: flag("D1E_QDEQ", base.qdeq),
               qtmm: flag("D1E_QTMM", base.qtmm) }
    }

    pub fn any(&self) -> bool {
        *self != Self::plain()
    }

    pub fn names(&self) -> Vec<&'static str> {
        let mut v = vec![];
        for (on, n) in [(self.prune, "prune"), (self.fuse, "fuse"), (self.compile, "compile"), (self.metal, "metal"),
                        (self.q8, "q8"), (self.fp16, "fp16"), (self.window, "window"),
                        (self.tensor_mm, "tensor_mm"), (self.swiglu, "swiglu"), (self.addnorm, "addnorm"), (self.qkprep, "qkprep"),
                        (self.qdeq, "qdeq"), (self.qtmm, "qtmm")] {
            if on {
                v.push(n);
            }
        }
        v
    }
}

/// The compiled path's padding multiple: 128, or D1E_PAD_MULTIPLE (a diagnostic; 1 compiles every length).
fn pad_multiple() -> i32 {
    std::env::var("D1E_PAD_MULTIPLE").ok().and_then(|v| v.parse().ok()).filter(|m: &i32| *m >= 1).unwrap_or(PAD_MULTIPLE)
}

/// D1E_SKIP_LAYERS: conv layers to leave out, e.g. "11,13". Only conv layers before the last full-attention layer
/// are taken (attention layers and the pruned tail are never skipped).
fn skip_layers(layers: &[Kind]) -> Vec<usize> {
    let last_full = layers.iter().rposition(|k| *k == Kind::Full).unwrap_or(0);
    std::env::var("D1E_SKIP_LAYERS")
        .ok()
        .map(|v| v.split(',').filter_map(|x| x.trim().parse().ok()).collect::<Vec<usize>>())
        .unwrap_or_default()
        .into_iter()
        .filter(|&i| i < last_full && layers.get(i) == Some(&Kind::Conv))
        .collect()
}

type CompiledFn = Box<dyn FnMut(&[Array]) -> std::result::Result<Vec<Array>, Exception>>;

pub struct Model {
    pub cfg: Config,
    pub opts: Opts,
    p: W,
    kernel: Option<Rc<ConvKernel>>,
    /// weight names in the order the compiled graph takes them, after its fixed inputs
    names: Vec<String>,
    compiled: RefCell<Option<CompiledFn>>,
    /// what was asked for but could not be built (reported, then run without)
    pub dropped: Vec<String>,
    /// the compute dtype
    pub dt: Dtype,
    /// D1E_PROBE_MAX=1: intermediates' max |value| of the current call (the eager path only)
    probe: Option<RefCell<Vec<Array>>>,
    /// the largest |activation| seen over all calls while probing
    pub max_seen: std::cell::Cell<f32>,
    /// D1E_SKIP_LAYERS (comma list of conv layer indices; lossy): layers left out
    pub skip: Vec<usize>,
    /// D1E_PROFILE=1: the last call's [graph build and launch, trunk eval (GPU), readout] in ms
    pub profile: std::cell::Cell<Option<[f64; 3]>>,
    /// D1E_TENSOR_MM: the tensor-op GEMM and the weight-name suffixes it serves
    tmm: Option<Rc<crate::mpp::TensorGemm>>,
    tmm_only: Rc<Vec<String>>,
    /// D1E_ADDNORM: the fused residual add + RMSNorm kernel
    addnorm: Option<Rc<crate::fused::AddNorm>>,
    /// D1E_QKPREP: the fused q/k norm + RoPE kernel
    qkprep: Option<Rc<crate::fused::QkPrep>>,
    /// quantized weights: each one's bit width (by weight name), and the group size
    qbits: Rc<HashMap<String, i32>>,
    group: i32,
    /// D1E_QTMM: the quantized tensor-op GEMM
    qtmm: Option<Rc<crate::qmm::QuantGemm>>,
}

pub fn e(err: Exception) -> anyhow::Error {
    anyhow!("mlx: {err}")
}

fn cast(a: &Array, dt: Dtype) -> R<Array> {
    if a.dtype() == dt {
        Ok(a.clone())
    } else {
        a.as_dtype(dt)
    }
}

fn norm(x: &Array, w: &Array, eps: f32) -> R<Array> {
    fast::rms_norm(x, Some(w), eps)
}

fn silu(x: &Array) -> R<Array> {
    x.multiply(nn::sigmoid(x)?)
}

/// a[start:stop] along `axis`
fn range(a: &Array, axis: i32, start: i32, stop: i32) -> R<Array> {
    let n = a.shape()[axis.rem_euclid(a.ndim() as i32) as usize];
    let mut cuts = vec![];
    if start > 0 {
        cuts.push(start);
    }
    if stop < n {
        cuts.push(stop);
    }
    let parts = ops::split_at_indices(a, &cuts, axis)?;
    Ok(parts[if start > 0 { 1 } else { 0 }].clone())
}

/// Rows of `x` as a GEMM sees them: the product of every axis but the last.
fn rows_of(x: &Array) -> i32 {
    let s = x.shape();
    s[..s.len() - 1].iter().product()
}

/// Embedding rows for `ids` (any shape), dequantized when the embedding is stored quantized.
fn embed_rows(w: &W, qbits: &HashMap<String, i32>, group: i32, ids: &Array) -> R<Array> {
    const E: &str = "embed_tokens.weight";
    match qbits.get(E) {
        None => w[E].take_axis(ids, 0),
        Some(&bits) => {
            let (q, s, b) = (w[E].take_axis(ids, 0)?, w[&format!("{E}.scales")].take_axis(ids, 0)?,
                             w[&format!("{E}.biases")].take_axis(ids, 0)?);
            ops::dequantize(q, s, &b, group, bits)
        }
    }
}

/// The checkpoint's MLX quantization block (config.json `quantization`): group size, linear bits, embedding bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuantConfig {
    pub group: i32,
    pub bits: i32,
    /// None: not stated (mlx-lm's own exports quantize the embedding at `bits` and write no such field); the width
    /// read from the embedding's shapes is then taken as it is
    pub embedding_bits: Option<i32>,
}

impl QuantConfig {
    pub fn load(dir: &Path) -> Result<Option<Self>> {
        let c: Value = serde_json::from_str(&std::fs::read_to_string(dir.join("config.json"))?)?;
        // MLX's own block; a transformers `quantization_config` (another format) is not this engine's to read
        let Some(q) = c.get("quantization") else { return Ok(None) };
        let int = |k: &str| q.get(k).and_then(Value::as_i64).map(|v| v as i32);
        if let Some(mode) = q.get("mode").and_then(Value::as_str) {
            if mode != "affine" {
                bail!("config.json quantization mode {mode:?}: only affine is supported");
            }
        }
        let bits = int("bits").context("config.json quantization.bits")?;
        Ok(Some(Self { group: int("group_size").unwrap_or(Q_GROUP), bits, embedding_bits: int("embedding_bits") }))
    }
}

fn is_linear(name: &str) -> bool {
    name.starts_with("layers.")
        && ["proj.weight", ".w1.weight", ".w2.weight", ".w3.weight", ".w13.weight", ".qkvg.weight"]
            .iter()
            .any(|s| name.ends_with(s))
}

/// The forward on one weight table; shared by the eager and compiled paths.
struct Ctx<'a> {
    w: &'a W,
    cfg: &'a Config,
    kernel: Option<&'a ConvKernel>,
    /// the compute dtype: BF16, or FP16 under D1E_FP16
    dt: Dtype,
    /// when set, every intermediate's max |value| is collected here (D1E_PROBE_MAX)
    probe: Option<&'a RefCell<Vec<Array>>>,
    /// layers left out (D1E_SKIP_LAYERS; lossy): their residual passes through unchanged
    skip: &'a [usize],
    /// block-local sliding attention (D1E_WINDOW)
    window: bool,
    /// the fused residual add + RMSNorm kernel (D1E_ADDNORM)
    addnorm: Option<&'a crate::fused::AddNorm>,
    /// the fused q/k norm + RoPE + head-major layout kernel (D1E_QKPREP)
    qkprep: Option<&'a crate::fused::QkPrep>,
    /// tensor-op GEMM (D1E_TENSOR_MM) and the weight-name suffixes it serves (empty: every linear layer)
    tmm: Option<(&'a crate::mpp::TensorGemm, &'a [String])>,
    /// quantized weights' bit widths by weight name, and the group size
    qbits: &'a HashMap<String, i32>,
    group: i32,
    /// D1E_QDEQ: dequantize where used for inputs of at least QDEQ_MIN_ROWS rows, then the dense GEMM
    qdeq: bool,
    /// D1E_QTMM: the quantized tensor-op GEMM, for inputs of at least QDEQ_MIN_ROWS rows (before qdeq)
    qtmm: Option<&'a crate::qmm::QuantGemm>,
}

impl Ctx<'_> {
    fn w(&self, k: &str) -> &Array {
        &self.w[k]
    }

    /// Probing: evaluates the layer so far and folds its maxima into one value, so the graph (and memory) stays one
    /// layer deep.
    fn flush(&self, x: &Array) -> R<()> {
        if let Some(p) = self.probe {
            let parts = std::mem::take(&mut *p.borrow_mut());
            mlx_rs::transforms::eval(parts.iter().chain(std::iter::once(x)))?;
            let m = parts.iter().map(|a| a.item_exact::<f32>()).fold(0f32, |a, b| {
                if a.is_nan() || b.is_nan() { f32::NAN } else { a.max(b) }
            });
            p.borrow_mut().push(Array::from_f32(m));
        }
        Ok(())
    }

    fn track(&self, a: &Array) -> R<()> {
        if let Some(p) = self.probe {
            p.borrow_mut().push(a.as_dtype(Dtype::Float32)?.abs()?.max(None)?);
        }
        Ok(())
    }

    /// x @ w.T, through quantized_matmul when the weight is quantized; `rows` takes only rows [a, b) of the weight.
    fn lin_rows(&self, x: &Array, name: &str, rows: Option<(i32, i32)>) -> R<Array> {
        let scales = format!("{name}.scales");
        let pick = |a: &Array| -> R<Array> {
            match rows {
                Some((s, t)) => range(a, 0, s, t),
                None => Ok(a.clone()),
            }
        };
        if let Some(sc) = self.w.get(&scales) {
            let bits = *self.qbits.get(name)
                .ok_or_else(|| Exception::custom(format!("{name}: quantized without a known bit width")))?;
            let b = pick(&self.w[&format!("{name}.biases")])?;
            let (wq, s) = (pick(self.w(name))?, pick(sc)?);
            let big = rows_of(x) >= QDEQ_MIN_ROWS;
            let y = if let (Some(q), true) = (self.qtmm.filter(|q| q.serves(bits, false)), big) {
                q.apply(x, &wq, &s, &b, bits, false)?
            } else if self.qdeq && big {
                let wd = ops::dequantize(&wq, &s, &b, self.group, bits)?;
                match self.tmm.filter(|(_, only)| only.is_empty() || only.iter().any(|s| name.ends_with(s.as_str()))) {
                    Some((g, _)) => g.apply(x, &wd)?,
                    None => ops::matmul(x, wd.transpose()?)?,
                }
            } else {
                ops::quantized_matmul(x, wq, s, &b, true, self.group, bits)?
            };
            self.track(&y)?;
            Ok(y)
        } else if let Some((g, only)) = self.tmm.filter(|(_, only)| only.is_empty() || only.iter().any(|s| name.ends_with(s.as_str()))) {
            let _ = only;
            let y = g.apply(x, &pick(self.w(name))?)?;
            self.track(&y)?;
            Ok(y)
        } else {
            let y = ops::matmul(x, pick(self.w(name))?.transpose()?)?;
            self.track(&y)?;
            Ok(y)
        }
    }

    fn lin(&self, x: &Array, name: &str) -> R<Array> {
        self.lin_rows(x, name, None)
    }

    /// d1_2b.inv_freq
    fn inv_freq(&self, kind: Kind) -> R<Array> {
        let (theta, partial) = if kind == Kind::Full { self.cfg.rope_full } else { self.cfg.rope_sliding };
        let dim = self.cfg.hd;
        let angles = ((partial * dim as f64) as i32) / 2;
        let exps = Array::arange::<_, f32>(0, 2 * angles, 2)?.divide(Array::from_f32(dim as f32))?;
        let f = Array::from_f32(1.0).divide(ops::power(Array::from_f32(theta as f32), &exps)?)?;
        ops::concatenate(&[f, ops::zeros::<f32>(&[dim / 2 - angles])?], 0)
    }

    /// d1_2b.rope_tables for one kind: cos and sin [T, D] in float32, rounded through BF16 as the reference does.
    fn rope_table(&self, kind: Kind, t: i32) -> R<(Array, Array)> {
        let pos = Array::arange::<_, f32>(0, t, 1)?;
        let freqs = pos.expand_dims(1)?.multiply(self.inv_freq(kind)?.expand_dims(0)?)?;
        let emb = ops::concatenate(&[&freqs, &freqs], -1)?;
        let cos = ops::cos(&emb)?.as_dtype(self.dt)?.as_dtype(Dtype::Float32)?;
        let sin = ops::sin(&emb)?.as_dtype(self.dt)?.as_dtype(Dtype::Float32)?;
        Ok((cos, sin))
    }

    fn rope(x: &Array, cs: &(Array, Array)) -> R<Array> {
        let xf = x.as_dtype(Dtype::Float32)?;
        let d = x.shape()[x.ndim() - 1];
        let half = d / 2;
        let parts = ops::split_at_indices(&xf, &[half], -1)?;
        let rot = ops::concatenate(&[parts[1].negative()?, parts[0].clone()], -1)?;
        xf.multiply(&cs.0)?.add(rot.multiply(&cs.1)?)?.as_dtype(x.dtype())
    }

    fn mlp(&self, pre: &str, x: &Array) -> R<Array> {
        let inter = format!("{pre}w13i.weight");
        if self.w.contains_key(&inter) {
            let big = rows_of(x) >= QDEQ_MIN_ROWS;
            let ab = match (self.qbits.get(&inter).copied(), self.tmm) {
                (None, Some((g, _))) => g.apply_swiglu(x, self.w(&inter))?,
                (None, None) => return Err(Exception::custom("interleaved w13 without the tensor GEMM")),
                (Some(bits), _) if big && self.qtmm.is_some_and(|q| q.serves(bits, true)) => {
                    self.qtmm.unwrap().apply(x, self.w(&inter), self.w(&format!("{inter}.scales")),
                                             self.w(&format!("{inter}.biases")), bits, true)?
                }
                (Some(bits), Some((g, _))) if big && self.qdeq => {
                    let wd = ops::dequantize(self.w(&inter), self.w(&format!("{inter}.scales")),
                                             self.w(&format!("{inter}.biases")), self.group, bits)?;
                    g.apply_swiglu(x, &wd)?
                }
                (Some(_), _) => {
                    // few rows: quantized_matmul on the interleaved rows, then a = even columns, b = odd columns
                    let y = self.lin(x, &inter)?;
                    let mut shape = y.shape().to_vec();
                    let n = shape.pop().unwrap();
                    shape.extend([n / 2, 2]);
                    let parts = ops::split_equal(y.reshape(&shape)?, 2, -1)?;
                    let (a, b) = (parts[0].squeeze_axes(&[-1])?, parts[1].squeeze_axes(&[-1])?);
                    silu(&a)?.multiply(b)?
                }
            };
            self.track(&ab)?;
            return self.lin(&ab, &format!("{pre}w2.weight"));
        }
        let fused = format!("{pre}w13.weight");
        let (a, b) = if self.w.contains_key(&fused) {
            let ab = ops::split_equal(self.lin(x, &fused)?, 2, -1)?;
            (ab[0].clone(), ab[1].clone())
        } else {
            (self.lin(x, &format!("{pre}w1.weight"))?, self.lin(x, &format!("{pre}w3.weight"))?)
        };
        let ab = silu(&a)?.multiply(b)?;
        self.track(&ab)?;
        self.lin(&ab, &format!("{pre}w2.weight"))
    }

    fn conv(&self, pre: &str, x: &Array) -> R<Array> {
        let bcx = self.lin(x, &format!("{pre}in_proj.weight"))?;
        let w = self.w(&format!("{pre}conv.weight")); // (C, k)
        if let Some(kernel) = self.kernel {
            let cy = kernel.apply(&bcx, w)?;
            self.track(&cy)?;
            return self.lin(&cy, &format!("{pre}out_proj.weight"));
        }
        let t = x.shape()[1];
        let parts = ops::split_equal(bcx, 3, -1)?;
        let (b, c, xx) = (&parts[0], &parts[1], &parts[2]);
        let bx = b.multiply(xx)?;
        let k = self.cfg.kernel;
        let wcols = ops::split_equal(w, k, 1)?; // k arrays (C, 1)
        let col = |i: usize| -> R<Array> { wcols[i].reshape(&[-1]) };
        let widths = [(0, 0), (k - 1, 0), (0, 0)];
        let padded = ops::pad(&bx, &widths[..], None, None)?;
        let mut y = range(&padded, 1, 0, t)?.multiply(col(0)?)?;
        for i in 1..k {
            y = y.add(range(&padded, 1, i, i + t)?.multiply(col(i as usize)?)?)?;
        }
        self.track(&bx)?;
        self.track(&y)?;
        let cy = c.multiply(&y)?;
        self.track(&cy)?;
        self.lin(&cy, &format!("{pre}out_proj.weight"))
    }

    /// q, k, v and gate projections; with `q_rows`, q and gate only at those positions (prune).
    fn projections(&self, pre: &str, x: &Array, xq: &Array) -> R<(Array, Array, Array, Array)> {
        let (h, kv, d) = (self.cfg.heads, self.cfg.kv, self.cfg.hd);
        let fused = format!("{pre}qkvg.weight");
        let pruned = x.shape()[1] != xq.shape()[1];
        if self.w.contains_key(&fused) {
            let (q_end, k_end, v_end) = (h * d, h * d + kv * d, h * d + 2 * kv * d);
            if !pruned {
                let parts = ops::split_at_indices(self.lin(x, &fused)?, &[q_end, k_end, v_end], -1)?;
                return Ok((parts[0].clone(), parts[1].clone(), parts[2].clone(), parts[3].clone()));
            }
            let rows = v_end + h;
            let q = self.lin_rows(xq, &fused, Some((0, q_end)))?;
            let kv_out = self.lin_rows(x, &fused, Some((q_end, v_end)))?;
            let kvp = ops::split_at_indices(kv_out, &[kv * d], -1)?;
            let g = self.lin_rows(xq, &fused, Some((v_end, rows)))?;
            return Ok((q, kvp[0].clone(), kvp[1].clone(), g));
        }
        Ok((
            self.lin(xq, &format!("{pre}q_proj.weight"))?,
            self.lin(x, &format!("{pre}k_proj.weight"))?,
            self.lin(x, &format!("{pre}v_proj.weight"))?,
            self.lin(xq, &format!("{pre}gate_proj.weight"))?,
        ))
    }

    /// Attention; `xq`/`csq` are the query rows and their RoPE rows (the same as `x`/`cs` unless pruned).
    #[allow(clippy::too_many_arguments)]
    fn attn(&self, pre: &str, x: &Array, xq: &Array, cs: &(Array, Array), csq: &(Array, Array),
            mask: Option<&Array>, sliding: bool) -> R<Array> {
        let (b, t, tq) = (x.shape()[0], x.shape()[1], xq.shape()[1]);
        let (h, kv, d) = (self.cfg.heads, self.cfg.kv, self.cfg.hd);
        let fused = format!("{pre}qkvg.weight");
        if let (Some(prep), true) = (self.qkprep, tq == t && self.w.contains_key(&fused)) {
            let y = self.lin(x, &fused)?;
            let (q, k, v) = prep.apply(&y, self.w(&format!("{pre}q_norm.weight")), self.w(&format!("{pre}k_norm.weight")),
                                       &cs.0, &cs.1)?;
            let gg = range(&y, 2, h * d + 2 * kv * d, h * d + 2 * kv * d + h)?;
            return self.attend(pre, &q, &k, &v, &gg, mask, sliding, b, tq);
        }
        let (qq, kk, vv, gg) = self.projections(pre, x, xq)?;
        let q = norm(&qq.reshape(&[b, tq, h, d])?, self.w(&format!("{pre}q_norm.weight")), self.cfg.eps)?;
        let k = norm(&kk.reshape(&[b, t, kv, d])?, self.w(&format!("{pre}k_norm.weight")), self.cfg.eps)?;
        let v = vv.reshape(&[b, t, kv, d])?;
        let q = q.transpose_axes(&[0, 2, 1, 3])?;
        let k = k.transpose_axes(&[0, 2, 1, 3])?;
        let v = v.transpose_axes(&[0, 2, 1, 3])?;
        let q = Self::rope(&q, csq)?;
        let k = Self::rope(&k, cs)?;
        self.attend(pre, &q, &k, &v, &gg, mask, sliding, b, tq)
    }

    /// Attention from head-major q, k, v, then the sigmoid gate and o_proj.
    #[allow(clippy::too_many_arguments)]
    fn attend(&self, pre: &str, q: &Array, k: &Array, v: &Array, gg: &Array, mask: Option<&Array>, sliding: bool,
              b: i32, tq: i32) -> R<Array> {
        let (h, d) = (self.cfg.heads, self.cfg.hd);
        let t = k.shape()[2];
        let (q, k, v) = (q.clone(), k.clone(), v.clone());
        let scale = (d as f64).powf(-0.5) as f32;
        let mask: Option<fast::ScaledDotProductAttentionMask> = mask.map(Into::into);
        self.track(&q)?;
        self.track(&k)?;
        let w = self.cfg.window;
        let o = if sliding && self.window && t > 2 * w && tq == t {
            self.windowed(&q, &k, &v, w, scale)?
        } else {
            fast::scaled_dot_product_attention(&q, &k, &v, scale, mask, None::<&Array>)? // (B, H, Tq, D)
        };
        self.track(&o)?;
        let g = nn::sigmoid(&gg)?.transpose_axes(&[0, 2, 1])?.expand_dims(-1)?; // (B, H, Tq, 1)
        let o = o.multiply(g)?.transpose_axes(&[0, 2, 1, 3])?.reshape(&[b, tq, h * d])?;
        self.lin(&o, &format!("{pre}o_proj.weight"))
    }

    /// Causal attention within `w` as local blocks: each block of `w` queries sees its own block and the one before
    /// it, under the same mask (d1_2b.py `_windowed`): the same result at about 2w keys a query instead of T.
    fn windowed(&self, q: &Array, k: &Array, v: &Array, w: i32, scale: f32) -> R<Array> {
        let (b, h, t, d) = (q.shape()[0], q.shape()[1], q.shape()[2], q.shape()[3]);
        let kvh = k.shape()[1];
        let nb = (t + w - 1) / w;
        let pad = nb * w - t;
        let padt = |a: &Array| -> R<Array> { ops::pad(a, &[(0, 0), (0, 0), (0, pad), (0, 0)][..], None, None) };
        let (qp, kp, vp) = (padt(q)?, padt(k)?, padt(v)?);
        let with_previous = |a: &Array| -> R<Array> {
            let tb = a.reshape(&[b, kvh, nb, w, d])?;
            let shifted = ops::pad(&tb, &[(0, 0), (0, 0), (1, 0), (0, 0), (0, 0)][..], None, None)?;
            let shifted = range(&shifted, 2, 0, nb)?;
            ops::concatenate(&[shifted, tb], 3)
        };
        let (kb, vb) = (with_previous(&kp)?, with_previous(&vp)?);
        let qpos = Array::arange::<_, i32>(0, w, 1)?.add(Array::from_int(w))?.reshape(&[w, 1])?;
        let kpos = Array::arange::<_, i32>(0, 2 * w, 1)?.reshape(&[1, 2 * w])?;
        let blk = Array::arange::<_, i32>(0, nb, 1)?.reshape(&[nb, 1, 1])?;
        let allowed = kpos.le(&qpos)?.logical_and(kpos.gt(&qpos.subtract(Array::from_int(w))?)?)?
            .logical_and(blk.gt(Array::from_int(0))?.logical_or(kpos.ge(Array::from_int(w))?)?)?; // (nb, w, 2w)
        let qf = qp.reshape(&[b, h, nb, w, d])?.transpose_axes(&[0, 2, 1, 3, 4])?.reshape(&[b * nb, h, w, d])?;
        let kf = kb.transpose_axes(&[0, 2, 1, 3, 4])?.reshape(&[b * nb, kvh, 2 * w, d])?;
        let vf = vb.transpose_axes(&[0, 2, 1, 3, 4])?.reshape(&[b * nb, kvh, 2 * w, d])?;
        let m = ops::broadcast_to(allowed.reshape(&[1, nb, 1, w, 2 * w])?, &[b, nb, 1, w, 2 * w])?
            .reshape(&[b * nb, 1, w, 2 * w])?;
        let o = fast::scaled_dot_product_attention(&qf, &kf, &vf, scale, &m, None::<&Array>)?;
        let o = o.reshape(&[b, nb, h, w, d])?.transpose_axes(&[0, 2, 1, 3, 4])?.reshape(&[b, h, nb * w, d])?;
        range(&o, 2, 0, t)
    }

    /// The block's residual and FFN. With the fused add+norm kernel (D1E_ADDNORM), `next` names the next layer's
    /// input norm, and that layer's normed input comes back with the residual.
    fn residual(&self, pre: &str, x: &Array, h: &Array, next: Option<&str>) -> R<(Array, Option<Array>)> {
        let eps = self.cfg.eps;
        if let Some(an) = self.addnorm {
            let (x, n) = an.apply(x, h, self.w(&format!("{pre}post_operator_norm.weight")),
                                  self.w(&format!("{pre}ffn_norm.weight")))?;
            self.track(&x)?;
            let f = self.mlp(&format!("{pre}feed_forward."), &n)?;
            return match next {
                Some(nw) => {
                    let (x, hn) = an.apply(&x, &f, self.w(&format!("{pre}post_ffn_norm.weight")), self.w(nw))?;
                    self.track(&x)?;
                    Ok((x, Some(hn)))
                }
                None => {
                    let x = x.add(norm(&f, self.w(&format!("{pre}post_ffn_norm.weight")), eps)?)?;
                    self.track(&x)?;
                    Ok((x, None))
                }
            };
        }
        let x = x.add(norm(h, self.w(&format!("{pre}post_operator_norm.weight")), eps)?)?;
        self.track(&x)?;
        let f = self.mlp(&format!("{pre}feed_forward."), &norm(&x, self.w(&format!("{pre}ffn_norm.weight")), eps)?)?;
        let x = x.add(norm(&f, self.w(&format!("{pre}post_ffn_norm.weight")), eps)?)?;
        self.track(&x)?;
        Ok((x, None))
    }

    /// Causal within the window: key j is visible to query i when i - window < j <= i.
    fn sliding_mask(&self, t: i32) -> R<Array> {
        let idx = Array::arange::<_, i32>(0, t, 1)?;
        let keys = idx.expand_dims(0)?;
        let queries = idx.expand_dims(1)?;
        let lower = queries.subtract(Array::from_int(self.cfg.window))?;
        keys.le(&queries)?.logical_and(keys.gt(&lower)?)
    }

    /// The final-normed hidden state at the slot, [1, H]. `ids` [1, T]; `full_mask` the full-attention key mask (None:
    /// every key); `slot` [1]; `keep` the positions kept after the last full-attention layer (prune; ends at the slot).
    fn trunk(&self, ids: &Array, full_mask: Option<&Array>, slot: &Array, keep: Option<&Array>) -> R<Array> {
        let t = ids.shape()[1];
        let eps = self.cfg.eps;
        let mut x = cast(&embed_rows(self.w, self.qbits, self.group, ids)?, self.dt)?; // (1, T, H)
        self.track(&x)?;
        let full = self.rope_table(Kind::Full, t)?;
        let sliding_cs = self.rope_table(Kind::Sliding, t)?;
        let sliding = self.sliding_mask(t)?;
        let last_full = self.cfg.last_full();
        let mut carried: Option<Array> = None;
        let n_layers = self.cfg.layers.len();
        for (i, kind) in self.cfg.layers.iter().enumerate() {
            if self.skip.contains(&i) {
                carried = None;
                continue;
            }
            let pre = format!("layers.{i}.");
            let h = match carried.take() {
                Some(h) => h,
                None => norm(&x, self.w(&format!("{pre}operator_norm.weight")), eps)?,
            };
            let next_name = (i + 1 < n_layers && !self.skip.contains(&(i + 1)))
                .then(|| format!("layers.{}.operator_norm.weight", i + 1));
            let next = next_name.as_deref();
            match (kind, keep) {
                (Kind::Full, Some(keep)) if Some(i) == last_full => {
                    let hq = h.take_axis(keep, 1)?;
                    let csq = (full.0.take_axis(keep, 0)?, full.1.take_axis(keep, 0)?);
                    let a = self.attn(&format!("{pre}self_attn."), &h, &hq, &full, &csq, full_mask, false)?;
                    (x, carried) = self.residual(&pre, &x.take_axis(keep, 1)?, &a, next)?;
                }
                (Kind::Conv, _) => {
                    let a = self.conv(&format!("{pre}conv."), &h)?;
                    (x, carried) = self.residual(&pre, &x, &a, next)?;
                }
                (Kind::Full, _) => {
                    let a = self.attn(&format!("{pre}self_attn."), &h, &h, &full, &full, full_mask, false)?;
                    (x, carried) = self.residual(&pre, &x, &a, next)?;
                }
                (Kind::Sliding, _) => {
                    let a = self.attn(&format!("{pre}self_attn."), &h, &h, &sliding_cs, &sliding_cs, Some(&sliding), true)?;
                    (x, carried) = self.residual(&pre, &x, &a, next)?;
                }
            }
            self.flush(&x)?;
        }
        let at = match keep {
            Some(k) => Array::from_slice(&[k.shape()[0] - 1], &[1]),
            None => slot.clone(),
        };
        let h = x.take_axis(&at, 1)?.reshape(&[1, -1])?;
        let h = norm(&h, self.w("embedding_norm.weight"), eps)?;
        self.track(&h)?;
        Ok(h)
    }
}

impl Model {
    /// The text-trunk tensors of `dir/model.safetensors` (keys without the language-model prefix, conv weights
    /// squeezed to (C, k)), in BF16, evaluated; then the options' weight transforms.
    pub fn load(dir: &Path, opts: Opts) -> Result<Self> {
        let cfg = Config::load(dir)?;
        let qcfg = QuantConfig::load(dir)?;
        let path = dir.join("model.safetensors");
        let all = Array::load_safetensors(&path).map_err(|err| anyhow!("{}: {err}", path.display()))?;
        // a quantized checkpoint stores `X.scales`/`X.biases` beside `X.weight`; inside, they are `X.weight.scales`
        let quantized = all.keys().any(|k| k.ends_with(".scales"));
        let mut dropped = vec![];
        let fp16 = opts.fp16 && !quantized;
        if opts.fp16 && quantized {
            dropped.push("fp16: the checkpoint is quantized (BF16 scales); BF16 compute".to_string());
        }
        let dt = if fp16 { Dtype::Float16 } else { Dtype::Bfloat16 };
        let mut p = HashMap::new();
        for (k, v) in all {
            // the original checkpoint's keys carry the language-model prefix; the quantized trunk's do not
            let name = match k.strip_prefix(PREFIX) {
                Some(n) => n.to_string(),
                None if k.starts_with("layers.") || k.starts_with("embed_tokens.") || k.starts_with("embedding_norm.") => k.clone(),
                None => continue,
            };
            let name = match (name.strip_suffix(".scales"), name.strip_suffix(".biases")) {
                (Some(base), _) => format!("{base}.weight.scales"),
                (_, Some(base)) => format!("{base}.weight.biases"),
                _ => name,
            };
            let v = if name.ends_with("conv.conv.weight") && v.ndim() == 3 { v.squeeze_axes(&[1]).map_err(e)? } else { v };
            // packed codes stay as stored; scales and biases keep their stored dtype (BF16)
            let v = if v.dtype() == Dtype::Uint32 || name.ends_with(".scales") || name.ends_with(".biases") { v } else {
                cast(&v, dt).map_err(e)?
            };
            p.insert(name, v);
        }
        validate(&cfg, &p)?;
        let (group, qbits) = quant_bits(&p, qcfg)?;
        mlx_rs::transforms::eval(p.values()).map_err(e)?;
        let probe = std::env::var("D1E_PROBE_MAX").is_ok_and(|v| v == "1");
        let cfg_layers = cfg.layers.clone();
        let mut m = Self { cfg, opts: Opts { fp16, ..Opts::plain() }, dt, p, kernel: None, names: vec![],
                           compiled: RefCell::new(None), dropped,
                           probe: probe.then(|| RefCell::new(vec![])), max_seen: std::cell::Cell::new(0.0),
                           skip: skip_layers(&cfg_layers),
                           profile: std::cell::Cell::new(std::env::var("D1E_PROFILE").is_ok_and(|v| v == "1")
                               .then_some([0.0; 3])),
                           tmm: None, tmm_only: Rc::new(vec![]), addnorm: None, qkprep: None,
                           qbits: Rc::new(qbits), group, qtmm: None };
        m.apply_opts(opts);
        Ok(m)
    }

    /// Turns on what `opts` asks for; an option that fails to build is dropped and named in `dropped`.
    fn apply_opts(&mut self, opts: Opts) {
        if opts.prune {
            if self.cfg.prune_keep().is_some() {
                self.opts.prune = true;
            } else {
                self.dropped.push("prune: a non-conv layer follows the last full-attention layer".into());
            }
        }
        if opts.fuse {
            match self.fuse() {
                Ok(()) => self.opts.fuse = true,
                Err(err) => self.dropped.push(format!("fuse: {err:#}")),
            }
        }
        if opts.q8 && !self.qbits.is_empty() {
            self.dropped.push("q8: the checkpoint is already quantized".into());
        } else if opts.q8 {
            match self.quantize() {
                Ok(()) => self.opts.q8 = true,
                Err(err) => self.dropped.push(format!("q8: {err:#}")),
            }
        }
        if opts.metal {
            match ConvKernel::new(self.cfg.hidden, self.cfg.kernel, self.dt).and_then(|k| k.self_test().map(|_| k)) {
                Ok(k) => {
                    self.kernel = Some(Rc::new(k));
                    self.opts.metal = true;
                }
                Err(err) => self.dropped.push(format!("metal: {err:#}")),
            }
        }
        self.opts.window = opts.window;
        if opts.qkprep {
            if self.dt != Dtype::Bfloat16 {
                self.dropped.push("qkprep: BF16 only".into());
            } else {
                match crate::fused::QkPrep::new(self.cfg.heads, self.cfg.kv, self.cfg.hd, self.cfg.eps)
                    .and_then(|k| k.self_test(self.cfg.eps).map(|_| k)) {
                    Ok(k) => {
                        self.qkprep = Some(Rc::new(k));
                        self.opts.qkprep = true;
                    }
                    Err(err) => self.dropped.push(format!("qkprep: {err:#}")),
                }
            }
        }
        if opts.addnorm {
            if self.dt != Dtype::Bfloat16 {
                self.dropped.push("addnorm: BF16 only".into());
            } else {
                match crate::fused::AddNorm::new(self.cfg.hidden, self.cfg.eps).and_then(|k| k.self_test().map(|_| k)) {
                    Ok(k) => {
                        self.addnorm = Some(Rc::new(k));
                        self.opts.addnorm = true;
                    }
                    Err(err) => self.dropped.push(format!("addnorm: {err:#}")),
                }
            }
        }
        // D1E_TENSOR_MM: "1" or "all" for every linear layer, else a comma list of weight-name suffixes ("w2")
        if opts.tensor_mm {
            let v = std::env::var("D1E_TENSOR_MM").unwrap_or_else(|_| "1".into());
            {
                let only: Vec<String> = if v == "1" || v == "all" { vec![] } else {
                    v.split(',').map(|s| format!("{}.weight", s.trim())).collect()
                };
                let all_quantized = !self.qbits.is_empty() && !self.p.keys().any(|k| is_linear(k) && !self.qbits.contains_key(k));
                if !crate::mpp::has_neural_accelerators() {
                    self.dropped.push("tensor_mm: no GPU neural accelerators (before M5); MLX matmul".into());
                } else if self.opts.q8 {
                    self.dropped.push("tensor_mm: not with q8".into());
                } else if all_quantized && !opts.qdeq {
                    self.dropped.push("tensor_mm: every linear layer is quantized; they reach it only through qdeq".into());
                } else {
                    match crate::mpp::TensorGemm::new(self.dt, opts.swiglu && self.p.keys().any(|k| k.ends_with("w13.weight"))).and_then(|g| g.self_test().map(|_| g)) {
                        Ok(g) => {
                            self.opts.tensor_mm = true;
                            self.opts.qdeq = opts.qdeq && !self.qbits.is_empty() && !self.opts.q8;
                            self.tmm = Some(Rc::new(g));
                            self.tmm_only = Rc::new(only);
                        }
                        Err(err) => self.dropped.push(format!("tensor_mm: {err:#}")),
                    }
                }
            }
        }
        if opts.qdeq && !self.qbits.is_empty() && !self.opts.qdeq && !self.opts.q8 {
            // without the tensor GEMM: dequantize where used, then MLX's dense matmul
            self.opts.qdeq = true;
        }
        // D1E_QTMM: the quantized tensor-op GEMM for every bit width stored (and its SwiGLU variant)
        if opts.qtmm && !self.qbits.is_empty() && (self.opts.q8 || self.group != crate::qmm::GROUP || self.dt != Dtype::Bfloat16) {
            self.dropped.push(format!("qtmm: needs a stored checkpoint at group {} with BF16 compute", crate::qmm::GROUP));
        } else if opts.qtmm && !self.qbits.is_empty() {
            let mut bits: Vec<i32> = self.qbits.iter().filter(|(k, _)| k.starts_with("layers.")).map(|(_, b)| *b).collect();
            bits.sort();
            bits.dedup();
            let wanted: Vec<(i32, bool)> = bits.iter().flat_map(|&b| [(b, false), (b, opts.swiglu)]).collect();
            if !crate::mpp::has_neural_accelerators() {
                self.dropped.push("qtmm: no GPU neural accelerators (before M5)".into());
            } else {
                match crate::qmm::QuantGemm::new(&wanted).and_then(|q| q.self_test().map(|_| q)) {
                    Ok(q) => {
                        self.qtmm = Some(Rc::new(q));
                        self.opts.qtmm = true;
                    }
                    Err(err) => self.dropped.push(format!("qtmm: {err:#}")),
                }
            }
        }
        self.interleave_for_swiglu(opts.swiglu);

        if opts.compile {
            self.opts.compile = true;
            self.build_compiled();
        }
    }

    /// The SwiGLU epilogues read w1/w3 interleaved: a reordered copy replaces each fused w13 (for a quantized weight
    /// its scales and biases are reordered the same way: exact). Only when a kernel will read it: the BF16 tensor
    /// GEMM's epilogue for dense weights; for quantized ones the quantized kernel's, or the BF16 one through qdeq.
    fn interleave_for_swiglu(&mut self, asked: bool) {
        let tmm_swiglu = self.tmm.as_ref().is_some_and(|g| g.has_swiglu());
        self.opts.swiglu = false;
        if !asked {
            return;
        }
        let names: Vec<String> = self.p.keys().filter(|k| k.ends_with("feed_forward.w13.weight")).cloned().collect();
        for n in names {
            let ok = match self.qbits.get(&n) {
                None => tmm_swiglu,
                Some(&b) => self.qtmm.as_ref().is_some_and(|q| q.serves(b, true)) || (self.opts.qdeq && tmm_swiglu),
            };
            if !ok {
                if self.qbits.contains_key(&n) && !self.dropped.iter().any(|d| d.starts_with("swiglu:")) {
                    self.dropped.push("swiglu: no kernel reads quantized w1/w3 without qdeq or qtmm".into());
                }
                continue;
            }
            let parts: Vec<String> = [n.clone(), format!("{n}.scales"), format!("{n}.biases")]
                .into_iter().filter(|k| self.p.contains_key(k)).collect();
            let reordered: std::result::Result<Vec<Array>, Exception> = parts.iter()
                .map(|k| crate::mpp::interleave(&self.p[k]).and_then(|a| a.eval().map(|_| a)))
                .collect();
            match reordered {
                Ok(arrays) => {
                    for (k, a) in parts.iter().zip(arrays) {
                        self.p.remove(k);
                        self.p.insert(k.replace("w13.weight", "w13i.weight"), a);
                    }
                    if let Some(b) = Rc::make_mut(&mut self.qbits).remove(&n) {
                        Rc::make_mut(&mut self.qbits).insert(n.replace("w13.weight", "w13i.weight"), b);
                    }
                    self.opts.swiglu = true;
                }
                Err(err) => self.dropped.push(format!("swiglu: {err}")),
            }
        }
    }

    /// "bf16" (or "fp16"), "int8", "int4", or "mixed": the linear layers' storage.
    pub fn precision(&self) -> String {
        let mut bits: Vec<i32> = self.qbits.iter().filter(|(k, _)| k.starts_with("layers.")).map(|(_, b)| *b).collect();
        bits.sort();
        bits.dedup();
        match bits.as_slice() {
            [] => if self.dt == Dtype::Float16 { "fp16".into() } else { "bf16".into() },
            [b] => format!("int{b}"),
            _ => "mixed".into(),
        }
    }

    /// q/k/v/gate rows stacked into `qkvg`, w1/w3 into `w13`, per layer, the parts removed.
    fn fuse(&mut self) -> Result<()> {
        const PARTS: [(&str, &[&str]); 2] = [
            ("self_attn.qkvg.weight", &["self_attn.q_proj.weight", "self_attn.k_proj.weight", "self_attn.v_proj.weight",
                                        "self_attn.gate_proj.weight"]),
            ("feed_forward.w13.weight", &["feed_forward.w1.weight", "feed_forward.w3.weight"]),
        ];
        for i in 0..self.cfg.layers.len() {
            for (fused, parts) in PARTS {
                let names: Vec<String> = parts.iter().map(|n| format!("layers.{i}.{n}")).collect();
                if !names.iter().all(|n| self.p.contains_key(n)) {
                    continue;
                }
                // quantized parts fuse only at one bit width; their scales and biases stack the same way (exact)
                let bits: Vec<Option<i32>> = names.iter().map(|n| self.qbits.get(n).copied()).collect();
                if bits.iter().any(|b| *b != bits[0]) {
                    continue;
                }
                let out = format!("layers.{i}.{fused}");
                let suffixes: &[&str] = if bits[0].is_some() { &["", ".scales", ".biases"] } else { &[""] };
                for sfx in suffixes {
                    let arrays: Vec<Array> = names.iter().map(|n| self.p[&format!("{n}{sfx}")].clone()).collect();
                    let f = ops::concatenate(&arrays, 0).map_err(e)?;
                    f.eval().map_err(e)?;
                    for n in &names {
                        self.p.remove(&format!("{n}{sfx}"));
                    }
                    self.p.insert(format!("{out}{sfx}"), f);
                }
                if let Some(b) = bits[0] {
                    let q = Rc::make_mut(&mut self.qbits);
                    for n in &names {
                        q.remove(n);
                    }
                    q.insert(out, b);
                }
            }
        }
        Ok(())
    }

    /// Every linear layer's weight as 8-bit affine groups of 64: the weight, `.scales` and `.biases`.
    fn quantize(&mut self) -> Result<()> {
        let names: Vec<String> = self.p.keys().filter(|k| is_linear(k)).cloned().collect();
        if names.is_empty() {
            bail!("no linear weights");
        }
        for n in names {
            let (wq, s, b) = ops::quantize(&self.p[&n], Q_GROUP, Q_BITS).map_err(e)?;
            mlx_rs::transforms::eval([&wq, &s, &b]).map_err(e)?;
            self.p.insert(n.clone(), wq);
            self.p.insert(format!("{n}.scales"), s);
            self.p.insert(format!("{n}.biases"), b);
            Rc::make_mut(&mut self.qbits).insert(n, Q_BITS);
        }
        Ok(())
    }

    fn build_compiled(&mut self) {
        let mut names: Vec<String> = self.p.keys().cloned().collect();
        names.sort();
        self.names = names.clone();
        let cfg = self.cfg.clone();
        let kernel = self.kernel.clone();
        let dt = self.dt;
        let skip = self.skip.clone();
        let window = self.opts.window;
        let tmm = self.tmm.clone();
        let addnorm = self.addnorm.clone();
        let qkprep = self.qkprep.clone();
        let tmm_only = self.tmm_only.clone();
        let qbits = self.qbits.clone();
        let (group, qdeq) = (self.group, self.opts.qdeq);
        let qtmm = self.qtmm.clone();
        let fixed = if self.opts.prune { 4 } else { 3 };
        let f = move |args: &[Array]| -> std::result::Result<Vec<Array>, Exception> {
            let w: W = names.iter().cloned().zip(args[fixed..].iter().cloned()).collect();
            let cx = Ctx { w: &w, cfg: &cfg, kernel: kernel.as_deref(), dt, probe: None, skip: &skip, window,
                             tmm: tmm.as_deref().map(|g| (g, tmm_only.as_slice())), addnorm: addnorm.as_deref(),
                             qkprep: qkprep.as_deref(), qbits: &qbits, group, qdeq,
                             qtmm: qtmm.as_deref()};
            let keep = if fixed == 4 { Some(&args[3]) } else { None };
            Ok(vec![cx.trunk(&args[0], Some(&args[1]), &args[2], keep)?])
        };
        let compiled = mlx_rs::transforms::compile::compile(f, false);
        *self.compiled.borrow_mut() = Some(Box::new(compiled));
    }

    fn ctx(&self) -> Ctx<'_> {
        Ctx { w: &self.p, cfg: &self.cfg, kernel: self.kernel.as_deref(), dt: self.dt, probe: self.probe.as_ref(), skip: &self.skip,
              window: self.opts.window, tmm: self.tmm.as_deref().map(|g| (g, self.tmm_only.as_slice())),
              addnorm: self.addnorm.as_deref(), qkprep: self.qkprep.as_deref(), qbits: &self.qbits, group: self.group,
              qdeq: self.opts.qdeq, qtmm: self.qtmm.as_deref()}
    }

    /// The slot's final-normed hidden state, [1, H], on the options that are on.
    fn hidden(&self, ids: &[u32], slot: usize) -> Result<Array> {
        // a test hook for the run-time fallback: an optimised path that fails on every call
        if self.opts.any() && std::env::var("D1E_TEST_FAIL_FAST").is_ok_and(|v| v == "1") {
            bail!("D1E_TEST_FAIL_FAST: injected failure of the optimised path");
        }
        let t = ids.len() as i32;
        let keep_n = if self.opts.prune { self.cfg.prune_keep().unwrap() } else { 0 };
        // prune reads the positions ending at the slot; a row too short for them runs whole
        let prune = self.opts.prune && slot + 1 >= keep_n && keep_n > 0;
        let keep = prune.then(|| {
            let v: Vec<i32> = ((slot + 1 - keep_n) as i32..=slot as i32).collect();
            Array::from_slice(&v, &[keep_n as i32])
        });
        let slot_a = Array::from_slice(&[slot as i32], &[1]);
        let compiled = &mut *self.compiled.borrow_mut();
        // probing reads every intermediate, so it runs eagerly
        let eager = self.probe.is_some();
        if let (true, Some(run)) = (!eager && self.opts.compile && (prune || !self.opts.prune), compiled.as_mut()) {
            let m = pad_multiple();
            let tp = (t + m - 1) / m * m;
            let mut padded: Vec<i32> = ids.iter().map(|&i| i as i32).collect();
            padded.resize(tp as usize, self.cfg.pad_id);
            let valid: Vec<bool> = (0..tp).map(|j| j < t).collect();
            let mut args = vec![Array::from_slice(&padded, &[1, tp]), Array::from_slice(&valid, &[1, 1, 1, tp]), slot_a];
            if let Some(k) = keep {
                args.push(k);
            }
            args.extend(self.names.iter().map(|n| self.p[n].clone()));
            let out = run(&args).map_err(e)?;
            return out.into_iter().next().context("compiled trunk returned nothing");
        }
        let ids_i: Vec<i32> = ids.iter().map(|&i| i as i32).collect();
        let ids_a = Array::from_slice(&ids_i, &[1, t]);
        self.ctx().trunk(&ids_a, None, &slot_a, keep.as_ref()).map_err(e)
    }

    /// Each option's score (the best of its token forms) at the row's answer slot, float32. `ids`: the whole row;
    /// `groups`: each option's token forms.
    pub fn scores(&self, ids: &[u32], slot: usize, groups: &[Vec<u32>]) -> Result<Vec<f32>> {
        if ids.is_empty() || slot >= ids.len() || groups.is_empty() || groups.iter().any(Vec::is_empty) {
            bail!("bad row: {} ids, slot {slot}, {} groups", ids.len(), groups.len());
        }
        let t0 = std::time::Instant::now();
        let h = self.hidden(ids, slot)?;
        let profiling = self.profile.get().is_some();
        let (t1, t2) = if profiling {
            let t1 = std::time::Instant::now();
            h.eval().map_err(e)?;
            (t1, std::time::Instant::now())
        } else {
            (t0, t0)
        };
        let width = groups.iter().map(Vec::len).max().unwrap();
        let k = groups.len();
        // a short group repeats its first form (prompt._table), which leaves its best as it is
        let mut table = Vec::with_capacity(k * width);
        for g in groups {
            for j in 0..width {
                table.push(*g.get(j).unwrap_or(&g[0]) as i32);
            }
        }
        let opt = Array::from_slice(&table, &[1, k as i32, width as i32]);
        let emb = cast(&embed_rows(&self.p, &self.qbits, self.group, &opt).map_err(e)?, self.dt).map_err(e)?; // (1, K, F, H)
        let hh = h.reshape(&[1, 1, self.cfg.hidden, 1]).map_err(e)?; // h[:, None, :, None]
        let z = ops::matmul(&emb, &hh).map_err(e)?; // (1, K, F, 1)
        let z = z.squeeze_axes(&[-1]).map_err(e)?.as_dtype(Dtype::Float32).map_err(e)?;
        let z = z.max_axis(-1, false).map_err(e)?.reshape(&[-1]).map_err(e)?;
        z.eval().map_err(e)?;
        if profiling {
            let t3 = std::time::Instant::now();
            let ms = |a: std::time::Instant, b: std::time::Instant| (b - a).as_secs_f64() * 1e3;
            self.profile.set(Some([ms(t0, t1), ms(t1, t2), ms(t2, t3)]));
        }
        if let Some(p) = &self.probe {
            let parts = std::mem::take(&mut *p.borrow_mut());
            mlx_rs::transforms::eval(parts.iter()).map_err(e)?;
            let m = parts.iter().map(|a| a.item_exact::<f32>()).fold(self.max_seen.get(), |a, b| {
                if b.is_nan() || a.is_nan() { f32::NAN } else { a.max(b) }
            });
            self.max_seen.set(m);
        }
        Ok(z.as_slice::<f32>().to_vec())
    }
}

/// Each quantized weight's bit width from its shapes (packed uint32 columns x 32 / (scale columns x group)), checked
/// against config.json's quantization block (linear layers at `bits`, the embedding at `embedding_bits`).
fn quant_bits(p: &W, q: Option<QuantConfig>) -> Result<(i32, HashMap<String, i32>)> {
    let mut out = HashMap::new();
    let scaled: Vec<&String> = p.keys().filter(|k| k.ends_with(".weight.scales")).collect();
    if scaled.is_empty() {
        return Ok((q.map_or(Q_GROUP, |q| q.group), out));
    }
    let q = q.context("quantized tensors but no quantization block in config.json")?;
    for sk in scaled {
        let name = sk.strip_suffix(".scales").unwrap();
        let w = p.get(name).with_context(|| format!("{sk} without its weight"))?;
        let b = p.get(&format!("{name}.biases")).with_context(|| format!("{name}: scales without biases"))?;
        let s = &p[sk];
        if w.dtype() != Dtype::Uint32 || w.ndim() != 2 || s.ndim() != 2 || s.shape() != b.shape() || s.shape()[0] != w.shape()[0] {
            bail!("{name}: packed {:?} {:?}, scales {:?}, biases {:?}", w.dtype(), w.shape(), s.shape(), b.shape());
        }
        let k = s.shape()[1] * q.group;
        let bits = w.shape()[1] * 32 / k;
        if bits * k != w.shape()[1] * 32 || ![2, 3, 4, 5, 6, 8].contains(&bits) {
            bail!("{name}: {} packed columns for {k} inputs at group {}", w.shape()[1], q.group);
        }
        let want = if name == "embed_tokens.weight" { q.embedding_bits } else { Some(q.bits) };
        if want.is_some_and(|w| w != bits) {
            bail!("{name}: stored at {bits} bits, config.json says {}", want.unwrap());
        }
        if !matches!(s.dtype(), Dtype::Bfloat16 | Dtype::Float16 | Dtype::Float32) || s.dtype() != b.dtype() {
            bail!("{name}: scales {:?}, biases {:?}", s.dtype(), b.dtype());
        }
        out.insert(name.to_string(), bits);
    }
    Ok((q.group, out))
}

fn validate(cfg: &Config, p: &W) -> Result<()> {
    for k in ["embed_tokens.weight", "embedding_norm.weight"] {
        if !p.contains_key(k) {
            bail!("missing {PREFIX}{k}");
        }
    }
    for (i, kind) in cfg.layers.iter().enumerate() {
        let mut need = vec!["operator_norm", "post_operator_norm", "ffn_norm", "post_ffn_norm", "feed_forward.w1",
                            "feed_forward.w2", "feed_forward.w3"];
        match kind {
            Kind::Conv => need.extend(["conv.in_proj", "conv.out_proj", "conv.conv"]),
            _ => need.extend(["self_attn.q_proj", "self_attn.k_proj", "self_attn.v_proj", "self_attn.gate_proj",
                              "self_attn.o_proj", "self_attn.q_norm", "self_attn.k_norm"]),
        }
        for n in need {
            let k = format!("layers.{i}.{n}.weight");
            if !p.contains_key(&k) {
                bail!("missing {PREFIX}{k}");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packed(n: i32, k: i32, bits: i32) -> (Array, Array, Array) {
        (Array::from_slice(&vec![0u32; (n * k * bits / 32) as usize], &[n, k * bits / 32]),
         Array::from_slice(&vec![1f32; (n * k / 64) as usize], &[n, k / 64]),
         Array::from_slice(&vec![0f32; (n * k / 64) as usize], &[n, k / 64]))
    }

    fn table(entries: &[(&str, i32, i32, i32)]) -> W {
        let mut p = W::new();
        for &(name, n, k, bits) in entries {
            let (w, s, b) = packed(n, k, bits);
            p.insert(name.to_string(), w);
            p.insert(format!("{name}.scales"), s);
            p.insert(format!("{name}.biases"), b);
        }
        p
    }

    #[test]
    fn bits_read_from_shapes_and_checked_against_config() {
        let q4 = QuantConfig { group: 64, bits: 4, embedding_bits: Some(16) };
        let p = table(&[("layers.0.feed_forward.w2.weight", 16, 7680, 4), ("layers.0.conv.in_proj.weight", 24, 2048, 4)]);
        let (g, bits) = quant_bits(&p, Some(q4)).unwrap();
        assert_eq!(g, 64);
        assert_eq!(bits["layers.0.feed_forward.w2.weight"], 4);
        assert_eq!(bits.len(), 2);
        let p8 = table(&[("layers.0.conv.in_proj.weight", 24, 2048, 8)]);
        assert!(quant_bits(&p8, Some(q4)).is_err(), "stored 8-bit, config says 4");
        assert!(quant_bits(&p8, None).is_err(), "quantized tensors need a quantization block");
        let q8 = QuantConfig { bits: 8, ..q4 };
        assert_eq!(quant_bits(&p8, Some(q8)).unwrap().1["layers.0.conv.in_proj.weight"], 8);
        // a quantized embedding is held to embedding_bits
        let pe = table(&[("embed_tokens.weight", 32, 2048, 4)]);
        assert!(quant_bits(&pe, Some(q4)).is_err());
        assert_eq!(quant_bits(&pe, Some(QuantConfig { embedding_bits: Some(4), ..q4 })).unwrap().1["embed_tokens.weight"], 4);
        // not stated (mlx-lm's convention): the embedding's own width is taken
        assert_eq!(quant_bits(&pe, Some(QuantConfig { embedding_bits: None, ..q4 })).unwrap().1["embed_tokens.weight"], 4);
        // a dense table has no quantized weights
        assert!(quant_bits(&W::new(), None).unwrap().1.is_empty());
    }

    #[test]
    fn quantization_block_parsed_from_config() {
        let dir = std::env::temp_dir().join(format!("d1e-qcfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let write = |v: &str| std::fs::write(dir.join("config.json"), v).unwrap();
        write(r#"{"quantization": {"group_size": 64, "bits": 4, "mode": "affine", "embedding_bits": 16}}"#);
        assert_eq!(QuantConfig::load(&dir).unwrap(), Some(QuantConfig { group: 64, bits: 4, embedding_bits: Some(16) }));
        write(r#"{"quantization": {"bits": 8}}"#);
        assert_eq!(QuantConfig::load(&dir).unwrap(), Some(QuantConfig { group: 64, bits: 8, embedding_bits: None }));
        write(r#"{"quantization_config": {"quant_method": "bitsandbytes"}}"#);
        assert_eq!(QuantConfig::load(&dir).unwrap(), None, "not MLX's block");
        write(r#"{"quantization": {"bits": 4, "mode": "mxfp4"}}"#);
        assert!(QuantConfig::load(&dir).is_err());
        write(r#"{"text_config": {}}"#);
        assert_eq!(QuantConfig::load(&dir).unwrap(), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Stacking (fuse) and interleaving (SwiGLU) a quantized weight's rows together with its scales and biases gives
    /// exactly the dequantized weight stacked or interleaved: the transforms the loader applies are exact.
    #[test]
    fn quantized_rows_stack_and_interleave_exactly() {
        for bits in [4, 8] {
            let w = mlx_rs::random::normal::<f32>(&[96, 256], None, None, None).unwrap().as_dtype(Dtype::Bfloat16).unwrap();
            let (q, s, b) = ops::quantize(&w, 64, bits).unwrap();
            let deq = |q: &Array, s: &Array, b: &Array| ops::dequantize(q, s, b, 64, bits).unwrap();
            let full = deq(&q, &s, &b);
            let halves = |a: &Array| ops::split_equal(a, 2, 0).unwrap();
            let (hq, hs, hb) = (halves(&q), halves(&s), halves(&b));
            let stacked = ops::concatenate(&[deq(&hq[0], &hs[0], &hb[0]), deq(&hq[1], &hs[1], &hb[1])], 0).unwrap();
            assert!(stacked.array_eq(&full, None).unwrap().item::<bool>(), "{bits}-bit stack");
            let il = |a: &Array| crate::mpp::interleave(a).unwrap();
            let interleaved = deq(&il(&q), &il(&s), &il(&b));
            assert!(interleaved.array_eq(&il(&full), None).unwrap().item::<bool>(), "{bits}-bit interleave");
        }
    }
}
