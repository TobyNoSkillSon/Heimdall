//! Quantized linear layers through Metal 4 tensor ops (D1E_QTMM): y[M, N] = x[M, K] · dequantize(w)[N, K]^T for MLX
//! affine weights (packed uint32 codes, BF16 scale and bias per group of 64 inputs), BF16 in and out.
//!
//! Each threadgroup owns a TM x TN output tile. Per K chunk (`KC` inputs, whole groups) its threads dequantize the
//! TN weight rows into threadgroup memory, `bf16(fma(scale, code, bias))` (one rounding: the value `mx.dequantize`
//! writes), and one `matmul2d` multiplies the x rows (read from device memory) by that tile into a float accumulator.
//! So the arithmetic is the dense tensor GEMM's on the dequantized weight, with no dequantized copy in memory. The
//! SwiGLU variant takes the interleaved w1/w3 rows and writes silu(a) * b, as `mpp`'s BF16 one does.
//!
//! Prefill-sized inputs only (the caller keeps quantized_matmul for a few rows). Checks itself at load against
//! dequantize + MLX's matmul on unaligned shapes at every bit width it serves; any failure turns it off.

use crate::mpp::{launch, make_kernel};
use anyhow::{anyhow, bail, Result};
use mlx_rs::{error::Exception, ops, Array, Dtype};
use mlx_sys as sys;
use std::collections::HashMap;

pub const GROUP: i32 = 64;

#[derive(Clone, Copy, Debug)]
pub struct Tile {
    pub tm: i32,
    pub tn: i32,
    pub kc: i32,
    pub sg: i32,
    /// two weight tiles in threadgroup memory: the next chunk is dequantized while this one multiplies (one barrier
    /// per chunk instead of two)
    pub double: bool,
    /// 16-byte loads of the packed codes (4 words a thread a step) instead of one word
    pub vec4: bool,
    /// extra bf16 elements at the end of each weight-tile row in threadgroup memory (bank-conflict padding)
    pub pad: i32,
}

impl Tile {
    /// D1E_QTMM_TILE=TMxTNxKCxSG overrides (diagnosis and tuning).
    pub fn from_env() -> Self {
        let flag = |k: &str, d: bool| match std::env::var(k).as_deref() { Ok("1") => true, Ok("0") => false, _ => d };
        let pad = std::env::var("D1E_QTMM_PAD").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
        let d = Tile { tm: 64, tn: 128, kc: 64, sg: 4, double: flag("D1E_QTMM_DB", false), vec4: flag("D1E_QTMM_VEC", false), pad };
        let Ok(v) = std::env::var("D1E_QTMM_TILE") else { return d };
        let p: Vec<i32> = v.split('x').filter_map(|s| s.parse().ok()).collect();
        match p.as_slice() {
            [tm, tn, kc, sg] if *kc > 0 && kc % GROUP == 0 && *tm > 0 && *tn > 0 && tn % 2 == 0 && *sg > 0 =>
                Tile { tm: *tm, tn: *tn, kc: *kc, sg: *sg, ..d },
            _ => d,
        }
    }
}

pub struct QuantGemm {
    tile: Tile,
    /// (bits, swiglu) -> kernel
    kernels: HashMap<(i32, bool), sys::mlx_fast_metal_kernel>,
}

fn source(bits: i32, swiglu: bool, t: Tile) -> String {
    let Tile { tm, tn, kc, sg, double, vec4, pad } = t;
    let per_word = 32 / bits;
    let mask = (1u32 << bits) - 1;
    let threads = sg * 32;
    let nbuf = if double { 2 } else { 1 };
    // one step of the dequantize: VW packed words of one tile row (VW = 4 with 16-byte loads), all in one group
    let vw = if vec4 { 4 } else { 1 };
    let load = if vec4 {
        "const uint4 word4 = *((const device uint4*)(w + size_t(n) * words_row + k0 / PW + j));\n\
                const uint words[4] = {word4.x, word4.y, word4.z, word4.w};"
    } else {
        "const uint words[1] = {w[size_t(n) * words_row + k0 / PW + j]};"
    };
    let common = format!(
        r#"
    const int M = int(dims[0]), N = int(dims[1]), K = int(dims[2]);
    constexpr int TM = {tm}, TN = {tn}, KC = {kc}, PW = {per_word}, VW = {vw}, LD = {kc} + {pad};
    constexpr int WPR = KC / PW;              // packed words per tile row per chunk
    const int words_row = K / PW;             // packed words per weight row
    const int groups_row = K / 64;
    threadgroup bfloat wt[{nbuf} * TN * LD];
    auto X = tensor<device bfloat, dextents<int32_t, 2>, tensor_inline>((device bfloat*)x, dextents<int32_t, 2>(K, M));
    constexpr auto desc = matmul2d_descriptor(TM, TN, KC, false, true, false, matmul2d_descriptor::mode::multiply_accumulate);
    matmul2d<desc, execution_simdgroups<{sg}>> op;
    const uint n0 = threadgroup_position_in_grid.x * TN;
    const uint m0 = threadgroup_position_in_grid.y * TM;
    auto xs0 = X.slice(0, m0);
    auto W0 = tensor<threadgroup bfloat, dextents<int32_t, 2>, tensor_inline>(wt, dextents<int32_t, 2>(KC, TN),
                                                                             array<int32_t, 2>({{1, LD}}));
    auto acc = op.get_destination_cooperative_tensor<decltype(xs0), decltype(W0), float>();
    for (uint16_t e = 0; e < acc.get_capacity(); ++e) if (acc.is_valid_element(e)) acc[e] = 0.f;
    const uint tid = thread_index_in_threadgroup;
    auto dequant = [&](int k0, threadgroup bfloat* buf) {{
        for (uint i = tid * VW; i < uint(TN * WPR); i += {threads} * VW) {{
            const uint r = i / WPR, j = i % WPR;
            const uint n = n0 + r;
            threadgroup bfloat* dst = buf + r * LD + j * PW;
            if (int(n) < N) {{
                {load}
                const uint g = size_t(n) * groups_row + (k0 + j * PW) / 64;
                const float sc = float(scales[g]), bi = float(biases[g]);
                for (int u = 0; u < VW; ++u)
                    for (int v = 0; v < PW; ++v)
                        dst[u * PW + v] = bfloat(metal::fma(sc, float((words[u] >> ({bits} * v)) & {mask}u), bi));
            }} else {{
                for (int v = 0; v < VW * PW; ++v) dst[v] = bfloat(0.0f);
            }}
        }}
    }};
    if ({nbuf} == 1) {{
        for (int k0 = 0; k0 < K; k0 += KC) {{
            dequant(k0, wt);
            threadgroup_barrier(mem_flags::mem_threadgroup);
            auto xs = X.slice(k0, m0);
            op.run(xs, W0, acc);
            threadgroup_barrier(mem_flags::mem_threadgroup);
        }}
    }} else {{
        dequant(0, wt);
        threadgroup_barrier(mem_flags::mem_threadgroup);
        int b = 0;
        for (int k0 = 0; k0 < K; k0 += KC) {{
            if (k0 + KC < K) dequant(k0 + KC, wt + (b ^ 1) * TN * LD);
            auto Wb = tensor<threadgroup bfloat, dextents<int32_t, 2>, tensor_inline>(wt + b * TN * LD, dextents<int32_t, 2>(KC, TN),
                                                                                     array<int32_t, 2>({{1, LD}}));
            auto xs = X.slice(k0, m0);
            op.run(xs, Wb, acc);
            threadgroup_barrier(mem_flags::mem_threadgroup);
            b ^= 1;
        }}
    }}
"#
    );
    let epilogue = if swiglu {
        format!(
            r#"
    threadgroup bfloat* tile = wt;            // reused: TM x TN fits when TM * TN <= TN * KC, else a second buffer
    threadgroup bfloat tile2[(TM > {nbuf} * LD) ? TM * TN : 1];
    threadgroup bfloat* out = (TM > {nbuf} * LD) ? tile2 : tile;
    for (uint16_t e = 0; e < acc.get_capacity(); ++e) {{
        if (!acc.is_valid_element(e)) continue;
        auto ix = acc.get_multidimensional_index(e);
        out[ix[1] * TN + ix[0]] = bfloat(acc[e]);
    }}
    threadgroup_barrier(mem_flags::mem_threadgroup);
    const int half_n = N / 2;
    for (uint i = tid; i < uint(TM * (TN / 2)); i += {threads}) {{
        const uint r = i / (TN / 2), c = i % (TN / 2);
        const uint gm = m0 + r, gn = n0 / 2 + c;
        if (int(gm) >= M || int(gn) >= half_n) continue;
        const float a = float(out[r * TN + 2 * c]), b = float(out[r * TN + 2 * c + 1]);
        const bfloat sg = bfloat(1.0f / (1.0f + metal::exp(-a)));
        const bfloat sa = bfloat(a * float(sg));
        y[size_t(gm) * half_n + gn] = bfloat(float(sa) * b);
    }}
"#
        )
    } else {
        r#"
    for (uint16_t e = 0; e < acc.get_capacity(); ++e) {
        if (!acc.is_valid_element(e)) continue;
        auto ix = acc.get_multidimensional_index(e);
        const uint gn = n0 + ix[0], gm = m0 + ix[1];
        if (int(gm) < M && int(gn) < N) y[size_t(gm) * N + gn] = bfloat(acc[e]);
    }
"#
        .to_string()
    };
    common + &epilogue
}

impl QuantGemm {
    /// Kernels for each (bits, swiglu) pair asked for.
    pub fn new(wanted: &[(i32, bool)]) -> Result<Self> {
        let tile = Tile::from_env();
        if tile.tn % 2 != 0 || tile.kc % GROUP != 0 {
            bail!("qtmm: tile {tile:?}");
        }
        let header = "#include <metal_tensor>\n#include <MetalPerformancePrimitives/MetalPerformancePrimitives.h>\n\
                      using namespace mpp::tensor_ops;\n";
        let mut kernels = HashMap::new();
        for &(bits, swiglu) in wanted {
            if ![4, 8].contains(&bits) {
                bail!("qtmm: no {bits}-bit variant");
            }
            if kernels.contains_key(&(bits, swiglu)) {
                continue;
            }
            let name = format!("d1e_qtmm_{bits}{}_{}x{}x{}x{}{}{}", if swiglu { "_swiglu" } else { "" }, tile.tm, tile.tn,
                               tile.kc, tile.sg, if tile.double { "_db" } else { "" }, if tile.vec4 { "_v4" } else { "" });
            let name = format!("{name}_p{}", tile.pad);
            let k = make_kernel(&name, &["x", "w", "scales", "biases", "dims"], &source(bits, swiglu, tile), header)?;
            kernels.insert((bits, swiglu), k);
        }
        Ok(Self { tile, kernels })
    }

    pub fn serves(&self, bits: i32, swiglu: bool) -> bool {
        self.kernels.contains_key(&(bits, swiglu))
    }

    /// x [..., K] · dequantize(w, s, b)[N, K]^T -> [..., N] (or [..., N/2] with the SwiGLU epilogue on interleaved rows)
    pub fn apply(&self, x: &Array, w: &Array, s: &Array, b: &Array, bits: i32, swiglu: bool)
                 -> std::result::Result<Array, Exception> {
        let kernel = *self.kernels.get(&(bits, swiglu))
            .ok_or_else(|| Exception::custom(format!("qtmm: no kernel for {bits} bits, swiglu {swiglu}")))?;
        let xs = x.shape().to_vec();
        let k = *xs.last().unwrap();
        let n = w.shape()[0];
        if s.shape()[1] * GROUP != k || w.shape()[1] * (32 / bits) != k || k % self.tile.kc != 0 {
            return Err(Exception::custom(format!("qtmm: x {xs:?} against packed {:?}, scales {:?}", w.shape(), s.shape())));
        }
        let m: i32 = xs[..xs.len() - 1].iter().product();
        let x2 = x.reshape(&[m, k])?.as_dtype(Dtype::Bfloat16)?;
        let dims = Array::from_slice(&[m as u32, n as u32, k as u32], &[3]);
        let n_out = if swiglu { n / 2 } else { n };
        let t = self.tile;
        let out = launch(kernel, &[&x2, w, s, b, &dims], &[m, n_out], Dtype::Bfloat16,
                         ((n + t.tn - 1) / t.tn * 32 * t.sg, (m + t.tm - 1) / t.tm), 32 * t.sg)?;
        let mut shape = xs[..xs.len() - 1].to_vec();
        shape.push(n_out);
        out.reshape(&shape)
    }

    /// Against dequantize + MLX's matmul (and the plain SwiGLU) on unaligned shapes: every element within one BF16
    /// rounding step of the largest magnitude, as `mpp::TensorGemm::self_test` judges.
    pub fn self_test(&self) -> Result<()> {
        if std::env::var("D1E_TEST_QTMM_FAIL").is_ok_and(|v| v == "1") {
            bail!("qtmm self-test: forced to fail by D1E_TEST_QTMM_FAIL");
        }
        let f = |err: Exception| anyhow!("qtmm self-test: {err}");
        for (&(bits, swiglu), _) in &self.kernels {
            for (m, n, k) in [(77, 200, 256), (130, 3088, 2048), (1852, 256, 7680)] {
                let n = if swiglu { n + n % 4 } else { n };
                let x = mlx_rs::random::normal::<f32>(&[1, m, k], None, None, None).map_err(f)?
                    .as_dtype(Dtype::Bfloat16).map_err(f)?;
                let w = mlx_rs::random::normal::<f32>(&[n, k], None, None, None).map_err(f)?
                    .multiply(Array::from_f32(0.02)).map_err(f)?.as_dtype(Dtype::Bfloat16).map_err(f)?;
                let (wq, s, b) = ops::quantize(&w, GROUP, bits).map_err(f)?;
                let wd = ops::dequantize(&wq, &s, &b, GROUP, bits).map_err(f)?;
                let want = if swiglu {
                    // the plain path on stacked [w1; w3]: matmul, split, silu(a) * b; the kernel reads them interleaved
                    let ab = ops::matmul(&x, wd.transpose().map_err(f)?).map_err(f)?;
                    let parts = ops::split_equal(&ab, 2, -1).map_err(f)?;
                    let a = &parts[0];
                    a.multiply(mlx_rs::nn::sigmoid(a).map_err(f)?).map_err(f)?.multiply(&parts[1]).map_err(f)?
                } else {
                    ops::matmul(&x, wd.transpose().map_err(f)?).map_err(f)?
                }
                .as_dtype(Dtype::Float32).map_err(f)?;
                let got = if swiglu {
                    let il = |a: &Array| crate::mpp::interleave(a);
                    self.apply(&x, &il(&wq).map_err(f)?, &il(&s).map_err(f)?, &il(&b).map_err(f)?, bits, true).map_err(f)?
                } else {
                    self.apply(&x, &wq, &s, &b, bits, false).map_err(f)?
                }
                .as_dtype(Dtype::Float32).map_err(f)?;
                let diff = got.subtract(&want).map_err(f)?.abs().map_err(f)?.max(None).map_err(f)?;
                let scale = want.abs().map_err(f)?.max(None).map_err(f)?;
                // a kernel that fails to compile errors here (item_exact would panic on it)
                mlx_rs::transforms::eval([&diff, &scale]).map_err(f)?;
                let (d, sc) = (diff.item_exact::<f32>(), scale.item_exact::<f32>());
                if !(d <= sc / 64.0) || !sc.is_finite() || sc == 0.0 {
                    bail!("qtmm self-test {bits}-bit swiglu={swiglu} {m}x{n}x{k}: max diff {d} against max {sc}");
                }
            }
        }
        Ok(())
    }
}

impl Drop for QuantGemm {
    fn drop(&mut self) {
        unsafe {
            for k in self.kernels.values() {
                sys::mlx_fast_metal_kernel_free(*k);
            }
        }
    }
}
