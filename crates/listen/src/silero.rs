//! Native Silero VAD v5, 16 kHz path, from the pinned `silero_vad.onnx` constants.
//!
//! The ONNX graph wraps this ~0.7 M multiply-add network in nested `If` subgraphs
//! (sample-rate and batch-rank branches, per-call LSTM weight slicing); its
//! interpreter cost was most of each 32-ms frame. The arithmetic here is the
//! graph's 16 kHz branch, in f32:
//! reflect-pad 64 -> STFT conv (258x256, hop 128) -> magnitude [129x4] ->
//! 4 x (conv k3 pad1, strides 1,2,2,1, ReLU) -> LSTM cell (128) ->
//! ReLU -> 1x1 conv -> sigmoid.
use anyhow::{bail, ensure, Context, Result};
use std::{collections::HashMap, path::Path};

const BRANCH: &str = "If_0_then_branch__Inline_0__";
const STRIDES: [usize; 4] = [1, 2, 2, 1];

struct Conv {
    weight: Vec<f32>, // [out][in*3], ONNX [out][in][k] row-major
    bias: Vec<f32>,
    inputs: usize,
    outputs: usize,
}
pub(crate) struct Weights {
    basis: Vec<f32>, // [258][256]
    convs: Vec<Conv>,
    lstm: Vec<f32>, // [512][W_ih row | W_hh row], gates i, f, g, o
    bias: Vec<f32>, // b_ih + b_hh folded once (identical sum per call)
    head: Vec<f32>,
    head_bias: f32,
}
#[derive(Default)]
struct Scratch {
    padded: Vec<f32>,
    columns: Vec<f32>,
    a: Vec<f32>,
    b: Vec<f32>,
    gates: Vec<f32>,
}
pub(crate) struct Native {
    w: Weights,
    s: Scratch,
}

/// out[i*n + j] = a_row_i . b_j, for `a` = m rows of length k and n rows `b`.
/// Register-blocked: each loaded vector feeds several FMAs (a single dot product
/// needs two loads per FMA and was load-bound at ~3 multiply-adds per cycle).
fn gemm_nt(a: &[f32], k: usize, b: &[&[f32]], out: &mut [f32]) {
    match b.len() {
        1 => tiles::<8, 1>(a, k, b.try_into().unwrap(), out),
        2 => tiles::<4, 2>(a, k, b.try_into().unwrap(), out),
        4 => tiles::<4, 4>(a, k, b.try_into().unwrap(), out),
        n => unreachable!("unsupported Silero tile width {n}"),
    }
}
fn tiles<const R: usize, const N: usize>(a: &[f32], k: usize, b: &[&[f32]; N], out: &mut [f32]) {
    let m = a.len() / k;
    let mut i = 0;
    while i + R <= m {
        block::<R, N>(&a[i * k..(i + R) * k], k, b, &mut out[i * N..(i + R) * N]);
        i += R;
    }
    while i < m {
        block::<1, N>(&a[i * k..(i + 1) * k], k, b, &mut out[i * N..(i + 1) * N]);
        i += 1;
    }
}
#[inline(always)]
fn block<const R: usize, const N: usize>(a: &[f32], k: usize, b: &[&[f32]; N], out: &mut [f32]) {
    assert!(a.len() >= R * k && b.iter().all(|b| b.len() >= k) && out.len() >= R * N);
    let k4 = k / 4 * 4;
    #[cfg(target_arch = "aarch64")]
    // SAFETY: every 4-lane load below reads indices < r*k + k4 <= a.len() and
    // < k4 <= b[j].len(), checked by the assertion above.
    unsafe {
        use std::arch::aarch64::*;
        let mut acc = [[vdupq_n_f32(0.0); N]; R];
        let mut kk = 0;
        while kk < k4 {
            let bv: [float32x4_t; N] = std::array::from_fn(|j| vld1q_f32(b[j].as_ptr().add(kk)));
            for (r, acc) in acc.iter_mut().enumerate() {
                let av = vld1q_f32(a.as_ptr().add(r * k + kk));
                for j in 0..N {
                    acc[j] = vfmaq_f32(acc[j], av, bv[j]);
                }
            }
            kk += 4;
        }
        for r in 0..R {
            for j in 0..N {
                let mut s = vaddvq_f32(acc[r][j]);
                for kk in k4..k {
                    s = a[r * k + kk].mul_add(b[j][kk], s);
                }
                out[r * N + j] = s;
            }
        }
    }
    #[cfg(not(target_arch = "aarch64"))]
    for r in 0..R {
        for j in 0..N {
            out[r * N + j] = (0..k).fold(0.0, |s, kk| a[r * k + kk].mul_add(b[j][kk], s));
        }
    }
    let _ = k4;
}
#[inline(always)]
fn sigmoid(x: f32) -> f32 {
    1.0 / (1.0 + (-x).exp())
}

impl Conv {
    /// `x` is [inputs][len]; writes [outputs][out_len] into `y`.
    fn run(
        &self,
        x: &[f32],
        len: usize,
        stride: usize,
        cols: &mut Vec<f32>,
        y: &mut Vec<f32>,
    ) -> usize {
        let out_len = (len + 2 - 3) / stride + 1;
        let k = self.inputs * 3;
        // Column t holds the padded receptive field, laid out like the weight row.
        cols.clear();
        cols.resize(out_len * k, 0.0);
        for t in 0..out_len {
            let col = &mut cols[t * k..(t + 1) * k];
            for c in 0..self.inputs {
                for j in 0..3 {
                    let pos = (t * stride + j) as isize - 1;
                    if (0..len as isize).contains(&pos) {
                        col[c * 3 + j] = x[c * len + pos as usize];
                    }
                }
            }
        }
        y.clear();
        y.resize(self.outputs * out_len, 0.0);
        let rows: Vec<&[f32]> = cols.chunks_exact(k).collect();
        gemm_nt(&self.weight, k, &rows, y);
        for o in 0..self.outputs {
            for v in &mut y[o * out_len..(o + 1) * out_len] {
                *v = (*v + self.bias[o]).max(0.0);
            }
        }
        out_len
    }
}

impl Native {
    pub(crate) fn load(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
        let mut t = constants(&bytes)?;
        let mut take = |name: &str, shape: &[usize]| -> Result<Vec<f32>> {
            let (dims, data) = t
                .remove(name)
                .with_context(|| format!("Silero constant {name}"))?;
            ensure!(
                dims == shape,
                "Silero {name} shape {dims:?}, expected {shape:?}"
            );
            Ok(data)
        };
        let basis = take("stft.forward_basis_buffer", &[258, 1, 256])?;
        let mut convs = Vec::new();
        for (i, (inputs, outputs)) in [(129, 128), (128, 64), (64, 64), (64, 128)]
            .into_iter()
            .enumerate()
        {
            convs.push(Conv {
                weight: take(
                    &format!("encoder.{i}.reparam_conv.weight"),
                    &[outputs, inputs, 3],
                )?,
                bias: take(&format!("encoder.{i}.reparam_conv.bias"), &[outputs])?,
                inputs,
                outputs,
            });
        }
        let w_ih = take("decoder.rnn.weight_ih", &[512, 128])?;
        let w_hh = take("decoder.rnn.weight_hh", &[512, 128])?;
        let b_ih = take("decoder.rnn.bias_ih", &[512])?;
        let b_hh = take("decoder.rnn.bias_hh", &[512])?;
        let head = take("decoder.decoder.2.weight", &[1, 128, 1])?;
        let head_bias = take("decoder.decoder.2.bias", &[1])?[0];
        Ok(Self {
            w: Weights {
                basis,
                convs,
                lstm: w_ih
                    .as_chunks::<128>()
                    .0
                    .iter()
                    .zip(w_hh.as_chunks::<128>().0)
                    .flat_map(|(i, h)| i.iter().chain(h).copied())
                    .collect(),
                bias: b_ih.iter().zip(&b_hh).map(|(a, b)| a + b).collect(),
                head,
                head_bias,
            },
            s: Scratch::default(),
        })
    }

    /// `input` is 64 context + 512 new samples; `h`/`c` advance in place.
    pub(crate) fn step(&mut self, input: &[f32; 576], h: &mut [f32], c: &mut [f32]) -> f32 {
        let (w, s) = (&self.w, &mut self.s);
        // Right reflect padding of 64 samples (edge sample not repeated).
        s.padded.clear();
        s.padded.extend_from_slice(input);
        s.padded.extend((0..64).map(|i| input[574 - i]));
        // STFT as conv: four 256-sample frames at hop 128; magnitude of 129 bins.
        let frames: [&[f32]; 4] = std::array::from_fn(|t| &s.padded[t * 128..t * 128 + 256]);
        s.b.clear();
        s.b.resize(258 * 4, 0.0);
        gemm_nt(&w.basis, 256, &frames, &mut s.b);
        s.a.clear();
        s.a.extend((0..129 * 4).map(|i| (s.b[i] * s.b[i] + s.b[516 + i] * s.b[516 + i]).sqrt()));
        let mut len = 4;
        for (conv, stride) in w.convs.iter().zip(STRIDES) {
            len = conv.run(&s.a, len, stride, &mut s.columns, &mut s.b);
            std::mem::swap(&mut s.a, &mut s.b);
        }
        debug_assert_eq!((len, s.a.len()), (1, 128));
        s.b.clear();
        s.b.extend_from_slice(&s.a);
        s.b.extend_from_slice(h);
        s.gates.clear();
        s.gates.resize(512, 0.0);
        gemm_nt(&w.lstm, 256, &[&s.b], &mut s.gates);
        for (g, b) in s.gates.iter_mut().zip(&w.bias) {
            *g += b;
        }
        let g = &s.gates;
        for j in 0..128 {
            c[j] = sigmoid(g[128 + j]) * c[j] + sigmoid(g[j]) * g[256 + j].tanh();
            h[j] = sigmoid(g[384 + j]) * c[j].tanh();
        }
        let logit = h
            .iter()
            .zip(&w.head)
            .map(|(x, k)| x.max(0.0) * k)
            .sum::<f32>()
            + w.head_bias;
        sigmoid(logit)
    }
}

// Minimal protobuf walk: ModelProto.graph -> If.then_branch -> Constant tensors.
fn varint(b: &[u8], i: &mut usize) -> Result<u64> {
    let mut v = 0_u64;
    for shift in (0..64).step_by(7) {
        let byte = *b.get(*i).context("truncated varint")?;
        *i += 1;
        v |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(v);
        }
    }
    bail!("varint overflow")
}
/// (field, wire type, payload or varint value) for each top-level field of a message.
fn fields(b: &[u8]) -> Result<Vec<(u64, &[u8], u64)>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let key = varint(b, &mut i)?;
        match key & 7 {
            0 => out.push((key >> 3, &b[0..0], varint(b, &mut i)?)),
            1 => {
                ensure!(i + 8 <= b.len(), "truncated fixed64");
                i += 8;
            }
            2 => {
                let n = varint(b, &mut i)? as usize;
                ensure!(i + n <= b.len(), "truncated field");
                out.push((key >> 3, &b[i..i + n], 0));
                i += n;
            }
            5 => {
                ensure!(i + 4 <= b.len(), "truncated fixed32");
                i += 4;
            }
            w => bail!("unsupported protobuf wire type {w}"),
        }
    }
    Ok(out)
}
fn child(b: &[u8], field: u64) -> Result<Vec<&[u8]>> {
    Ok(fields(b)?
        .into_iter()
        .filter(|f| f.0 == field)
        .map(|f| f.1)
        .collect())
}
type Tensor = (Vec<usize>, Vec<f32>);
fn tensor(b: &[u8]) -> Result<Option<Tensor>> {
    let mut dims = Vec::new();
    let mut kind = 0;
    let mut data = Vec::new();
    for (field, payload, value) in fields(b)? {
        match field {
            1 if payload.is_empty() => dims.push(value as usize),
            1 => {
                let mut i = 0;
                while i < payload.len() {
                    dims.push(varint(payload, &mut i)? as usize);
                }
            }
            2 => kind = value,
            4 | 9 => data.extend(
                payload
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|x| f32::from_le_bytes(*x)),
            ),
            _ => {}
        }
    }
    if kind != 1 {
        return Ok(None); // not float32 (e.g. the int64 hop length)
    }
    ensure!(
        data.len() == dims.iter().product::<usize>().max(1),
        "Silero tensor size"
    );
    Ok(Some((dims, data)))
}
fn constants(model: &[u8]) -> Result<HashMap<String, Tensor>> {
    let graph = *child(model, 7)?.first().context("ONNX graph")?;
    for node in child(graph, 1)? {
        if child(node, 4)?.first().copied() != Some(b"If".as_slice()) {
            continue;
        }
        for attribute in child(node, 5)? {
            if child(attribute, 1)?.first().copied() != Some(b"then_branch".as_slice()) {
                continue;
            }
            let branch = *child(attribute, 6)?.first().context("then_branch graph")?;
            let mut out = HashMap::new();
            for n in child(branch, 1)? {
                if child(n, 4)?.first().copied() != Some(b"Constant".as_slice()) {
                    continue;
                }
                let Some(name) = child(n, 2)?
                    .first()
                    .and_then(|o| std::str::from_utf8(o).ok())
                else {
                    continue;
                };
                let Some(name) = name.strip_prefix(BRANCH).filter(|n| !n.starts_with('/')) else {
                    continue;
                };
                for a in child(n, 5)? {
                    if let Some(t) = child(a, 5)?.first() {
                        if let Some(t) = tensor(t)? {
                            out.insert(name.to_owned(), t);
                        }
                    }
                }
            }
            return Ok(out);
        }
    }
    bail!("Silero 16 kHz branch not found")
}
