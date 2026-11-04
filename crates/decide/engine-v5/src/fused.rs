//! The residual add fused with the RMSNorms around it (D1E_ADDNORM): one kernel reads the residual `x` and the
//! block output `a` and writes `x + w_post * norm(a)` and `w_next * norm(x + ...)`. Each row is one threadgroup of
//! H/4 threads reading 4 values each, the layout and the roundings of MLX's own `rms_single_row` kernel, so the two
//! norms and the add come out as the separate MLX ops make them. Checks itself against those ops at load.

use anyhow::{anyhow, bail, Result};
use mlx_rs::{error::Exception, fast, Array, Dtype, Stream};
use mlx_sys as sys;
use std::ffi::CString;

pub struct AddNorm {
    kernel: sys::mlx_fast_metal_kernel,
    hidden: i32,
    eps: f32,
}

fn status(code: i32, what: &str) -> std::result::Result<(), Exception> {
    if code == 0 { Ok(()) } else { Err(Exception::custom(format!("addnorm: {what} failed"))) }
}

impl AddNorm {
    pub fn new(hidden: i32, eps: f32) -> Result<Self> {
        if hidden % 4 != 0 || hidden / 4 > 1024 || (hidden / 4) % 32 != 0 {
            bail!("addnorm: hidden size {hidden} does not fit one threadgroup of H/4 threads");
        }
        let source = format!(
            r#"
    constexpr int H = {hidden};
    constexpr float EPS = {eps:e};
    const uint gid = threadgroup_position_in_grid.x;
    const uint lid = thread_index_in_threadgroup;
    const uint lane = thread_index_in_simdgroup;
    const uint sg = simdgroup_index_in_threadgroup;
    threadgroup float sums[32];
    threadgroup float inv[1];
    const size_t base = size_t(gid) * H + lid * 4;
    float ta[4];
    float acc = 0;
    for (int i = 0; i < 4; i++) {{ ta[i] = a[base + i]; acc += ta[i] * ta[i]; }}
    acc = simd_sum(acc);
    if (sg == 0) sums[lane] = 0;
    threadgroup_barrier(mem_flags::mem_threadgroup);
    if (lane == 0) sums[sg] = acc;
    threadgroup_barrier(mem_flags::mem_threadgroup);
    if (sg == 0) {{ acc = simd_sum(sums[lane]); if (lane == 0) inv[0] = metal::precise::rsqrt(acc / H + EPS); }}
    threadgroup_barrier(mem_flags::mem_threadgroup);
    const float n1 = inv[0];
    float tx[4];
    float acc2 = 0;
    for (int i = 0; i < 4; i++) {{
        const bfloat16_t t = w_post[lid * 4 + i] * static_cast<bfloat16_t>(ta[i] * n1);
        const bfloat16_t xn = x[base + i] + t;
        x_out[base + i] = xn;
        tx[i] = xn;
        acc2 += tx[i] * tx[i];
    }}
    acc2 = simd_sum(acc2);
    threadgroup_barrier(mem_flags::mem_threadgroup);
    if (sg == 0) sums[lane] = 0;
    threadgroup_barrier(mem_flags::mem_threadgroup);
    if (lane == 0) sums[sg] = acc2;
    threadgroup_barrier(mem_flags::mem_threadgroup);
    if (sg == 0) {{ acc2 = simd_sum(sums[lane]); if (lane == 0) inv[0] = metal::precise::rsqrt(acc2 / H + EPS); }}
    threadgroup_barrier(mem_flags::mem_threadgroup);
    const float n2 = inv[0];
    for (int i = 0; i < 4; i++) n_out[base + i] = w_next[lid * 4 + i] * static_cast<bfloat16_t>(tx[i] * n2);
"#
        );
        let name = CString::new("d1e_addnorm")?;
        let src = CString::new(source)?;
        let header = CString::new("")?;
        unsafe {
            let ins = sys::mlx_vector_string_new();
            let outs = sys::mlx_vector_string_new();
            for n in ["x", "a", "w_post", "w_next"] {
                let c = CString::new(n)?;
                sys::mlx_vector_string_append_value(ins, c.as_ptr());
            }
            for n in ["x_out", "n_out"] {
                let c = CString::new(n)?;
                sys::mlx_vector_string_append_value(outs, c.as_ptr());
            }
            let kernel = sys::mlx_fast_metal_kernel_new(name.as_ptr(), ins, outs, src.as_ptr(), header.as_ptr(), true, false);
            sys::mlx_vector_string_free(ins);
            sys::mlx_vector_string_free(outs);
            if kernel.ctx.is_null() {
                bail!("addnorm: kernel creation returned null");
            }
            Ok(Self { kernel, hidden, eps })
        }
    }

    /// (x + w_post * norm(a), w_next * norm(that)) for x, a [1, T, H] BF16.
    pub fn apply(&self, x: &Array, a: &Array, w_post: &Array, w_next: &Array)
                 -> std::result::Result<(Array, Array), Exception> {
        let shape = x.shape().to_vec();
        if a.shape() != shape.as_slice() || *shape.last().unwrap() != self.hidden || x.dtype() != Dtype::Bfloat16
            || a.dtype() != Dtype::Bfloat16 {
            return Err(Exception::custom(format!("addnorm: shapes {shape:?} / {:?}", a.shape())));
        }
        let rows: i32 = shape[..shape.len() - 1].iter().product();
        let stream = Stream::thread_local_or_default();
        unsafe {
            let config = sys::mlx_fast_metal_kernel_config_new();
            let r = (|| {
                for _ in 0..2 {
                    status(sys::mlx_fast_metal_kernel_config_add_output_arg(config, shape.as_ptr(), shape.len(),
                                                                           sys::mlx_dtype__MLX_BFLOAT16), "output")?;
                }
                let threads = self.hidden / 4;
                status(sys::mlx_fast_metal_kernel_config_set_grid(config, rows * threads, 1, 1), "grid")?;
                status(sys::mlx_fast_metal_kernel_config_set_thread_group(config, threads, 1, 1), "thread group")?;
                let ins = sys::mlx_vector_array_new();
                for arr in [x, a, w_post, w_next] {
                    sys::mlx_vector_array_append_value(ins, arr.as_ptr());
                }
                let mut outputs = sys::mlx_vector_array_new();
                let code = sys::mlx_fast_metal_kernel_apply(&mut outputs, self.kernel, ins, config, stream.as_ptr());
                sys::mlx_vector_array_free(ins);
                if code != 0 {
                    sys::mlx_vector_array_free(outputs);
                    return Err(Exception::custom("addnorm: apply failed"));
                }
                let mut o1 = sys::mlx_array_new();
                let mut o2 = sys::mlx_array_new();
                let g1 = sys::mlx_vector_array_get(&mut o1, outputs, 0);
                let g2 = sys::mlx_vector_array_get(&mut o2, outputs, 1);
                sys::mlx_vector_array_free(outputs);
                status(g1, "output read")?;
                status(g2, "output read")?;
                Ok((Array::from_ptr(o1), Array::from_ptr(o2)))
            })();
            sys::mlx_fast_metal_kernel_config_free(config);
            r
        }
    }

    /// Against MLX's rms_norm and add on a random block: the outputs must be identical.
    pub fn self_test(&self) -> Result<()> {
        if std::env::var("D1E_TEST_ADDNORM_FAIL").is_ok_and(|v| v == "1") {
            bail!("addnorm self-test: forced to fail by D1E_TEST_ADDNORM_FAIL");
        }
        let f = |err: Exception| anyhow!("addnorm self-test: {err}");
        let h = self.hidden;
        let rnd = |shape: &[i32], s: f32| -> Result<Array> {
            mlx_rs::random::normal::<f32>(shape, None, None, None).map_err(f)?.multiply(Array::from_f32(s)).map_err(f)?
                .as_dtype(Dtype::Bfloat16).map_err(f)
        };
        let (x, a) = (rnd(&[1, 37, h], 3.0)?, rnd(&[1, 37, h], 1.0)?);
        let (w1, w2) = (rnd(&[h], 0.5)?, rnd(&[h], 0.5)?);
        let (gx, gn) = self.apply(&x, &a, &w1, &w2).map_err(f)?;
        let wx = x.add(fast::rms_norm(&a, Some(&w1), self.eps).map_err(f)?).map_err(f)?;
        let wn = fast::rms_norm(&wx, Some(&w2), self.eps).map_err(f)?;
        for (g, w) in [(gx, wx), (gn, wn)] {
            let d = g.as_dtype(Dtype::Float32).map_err(f)?.subtract(w.as_dtype(Dtype::Float32).map_err(f)?).map_err(f)?
                .abs().map_err(f)?.max(None).map_err(f)?.item_exact::<f32>();
            if d != 0.0 {
                bail!("addnorm self-test: max diff {d} (must be identical)");
            }
        }
        Ok(())
    }
}

impl Drop for AddNorm {
    fn drop(&mut self) {
        unsafe { sys::mlx_fast_metal_kernel_free(self.kernel) };
    }
}

/// The attention input block in one kernel (D1E_QKPREP): from the fused q/k/v/gate GEMM output y [1, T, H·D + 2·KV·D + H],
/// q and k get their per-head RMSNorm (MLX's rounding: w * bf16(x * rsqrt(mean(x²) + eps))) and RoPE (float32 against
/// the given cos/sin tables, rounded once), and q, k, v come out head-major [1, heads, T, D] for attention. Replaces
/// the split, two norms, three transposes and the two RoPE kernels. One simdgroup per (token, head), 4 lanes of D each.
pub struct QkPrep {
    kernel: sys::mlx_fast_metal_kernel,
    heads: i32,
    kv: i32,
    hd: i32,
}

impl QkPrep {
    pub fn new(heads: i32, kv: i32, hd: i32, eps: f32) -> Result<Self> {
        if hd != 128 {
            bail!("qkprep: head dim {hd} (128 only)");
        }
        let nc = heads * hd + 2 * kv * hd + heads;
        let source = format!(
            r#"
    constexpr int H = {heads}, KV = {kv}, D = {hd}, NC = {nc};
    constexpr float EPS = {eps:e};
    const uint lane = thread_position_in_grid.x % 32;
    const uint slot = thread_position_in_grid.x / 32;
    const uint t = thread_position_in_grid.y;
    const int T = y_shape[1];
    const device bfloat16_t* row = y + size_t(t) * NC;
    if (slot >= H + KV) {{
        const uint hd = slot - H - KV;
        for (int i = 0; i < 4; i++)
            v_out[(size_t(hd) * T + t) * D + lane * 4 + i] = row[H * D + KV * D + hd * D + lane * 4 + i];
        return;
    }}
    const bool isq = slot < H;
    const uint hd = isq ? slot : slot - H;
    const uint off = isq ? hd * D : H * D + hd * D;
    float xv[4];
    float acc = 0;
    for (int i = 0; i < 4; i++) {{ xv[i] = row[off + lane * 4 + i]; acc += xv[i] * xv[i]; }}
    acc = simd_sum(acc);
    const float inv = metal::precise::rsqrt(acc / D + EPS);
    float nv[4];
    for (int i = 0; i < 4; i++) {{
        const bfloat16_t w = isq ? qn[lane * 4 + i] : kn[lane * 4 + i];
        nv[i] = float(w * static_cast<bfloat16_t>(xv[i] * inv));
    }}
    for (int i = 0; i < 4; i++) {{
        const float partner = simd_shuffle_xor(nv[i], 16);
        const uint j = lane * 4 + i;
        const float rot = (j < D / 2) ? -partner : partner;
        const bfloat16_t o = static_cast<bfloat16_t>(nv[i] * rc[size_t(t) * D + j] + rot * rs[size_t(t) * D + j]);
        if (isq) q_out[(size_t(hd) * T + t) * D + j] = o; else k_out[(size_t(hd) * T + t) * D + j] = o;
    }}
"#
        );
        let name = CString::new("d1e_qkprep")?;
        let src = CString::new(source)?;
        let header = CString::new("")?;
        unsafe {
            let ins = sys::mlx_vector_string_new();
            let outs = sys::mlx_vector_string_new();
            for n in ["y", "qn", "kn", "rc", "rs"] {
                let c = CString::new(n)?;
                sys::mlx_vector_string_append_value(ins, c.as_ptr());
            }
            for n in ["q_out", "k_out", "v_out"] {
                let c = CString::new(n)?;
                sys::mlx_vector_string_append_value(outs, c.as_ptr());
            }
            let kernel = sys::mlx_fast_metal_kernel_new(name.as_ptr(), ins, outs, src.as_ptr(), header.as_ptr(), true, false);
            sys::mlx_vector_string_free(ins);
            sys::mlx_vector_string_free(outs);
            if kernel.ctx.is_null() {
                bail!("qkprep: kernel creation returned null");
            }
            Ok(Self { kernel, heads, kv, hd })
        }
    }

    /// (q [1, H, T, D], k [1, KV, T, D], v [1, KV, T, D]) from y [1, T, NC] and the RoPE tables (cos, sin) [T, D] f32.
    pub fn apply(&self, y: &Array, qn: &Array, kn: &Array, rc: &Array, rs: &Array)
                 -> std::result::Result<(Array, Array, Array), Exception> {
        let t = y.shape()[1];
        let (h, kv, d) = (self.heads, self.kv, self.hd);
        if y.shape()[0] != 1 || y.dtype() != Dtype::Bfloat16 || rc.shape() != [t, d] {
            return Err(Exception::custom(format!("qkprep: y {:?}, tables {:?}", y.shape(), rc.shape())));
        }
        let stream = Stream::thread_local_or_default();
        unsafe {
            let config = sys::mlx_fast_metal_kernel_config_new();
            let r = (|| {
                for heads in [h, kv, kv] {
                    let shape = [1, heads, t, d];
                    status(sys::mlx_fast_metal_kernel_config_add_output_arg(config, shape.as_ptr(), 4,
                                                                           sys::mlx_dtype__MLX_BFLOAT16), "output")?;
                }
                status(sys::mlx_fast_metal_kernel_config_set_grid(config, 32 * (h + 2 * kv), t, 1), "grid")?;
                status(sys::mlx_fast_metal_kernel_config_set_thread_group(config, 32 * 4, 1, 1), "thread group")?;
                let ins = sys::mlx_vector_array_new();
                for arr in [y, qn, kn, rc, rs] {
                    sys::mlx_vector_array_append_value(ins, arr.as_ptr());
                }
                let mut outputs = sys::mlx_vector_array_new();
                let code = sys::mlx_fast_metal_kernel_apply(&mut outputs, self.kernel, ins, config, stream.as_ptr());
                sys::mlx_vector_array_free(ins);
                if code != 0 {
                    sys::mlx_vector_array_free(outputs);
                    return Err(Exception::custom("qkprep: apply failed"));
                }
                let mut o = [sys::mlx_array_new(), sys::mlx_array_new(), sys::mlx_array_new()];
                for (i, slot) in o.iter_mut().enumerate() {
                    status(sys::mlx_vector_array_get(slot, outputs, i), "output read")?;
                }
                sys::mlx_vector_array_free(outputs);
                Ok((Array::from_ptr(o[0]), Array::from_ptr(o[1]), Array::from_ptr(o[2])))
            })();
            sys::mlx_fast_metal_kernel_config_free(config);
            r
        }
    }
}

impl QkPrep {
    /// Against the plain ops (split, RMSNorm, transpose, RoPE in float32) on a random block: within one BF16 step.
    pub fn self_test(&self, eps: f32) -> Result<()> {
        if std::env::var("D1E_TEST_QKPREP_FAIL").is_ok_and(|v| v == "1") {
            bail!("qkprep self-test: forced to fail by D1E_TEST_QKPREP_FAIL");
        }
        let f = |err: Exception| anyhow!("qkprep self-test: {err}");
        let (h, kv, d, t) = (self.heads, self.kv, self.hd, 45);
        let nc = h * d + 2 * kv * d + h;
        let rnd = |shape: &[i32], s: f32, dt: Dtype| -> Result<Array> {
            mlx_rs::random::normal::<f32>(shape, None, None, None).map_err(f)?.multiply(Array::from_f32(s)).map_err(f)?
                .as_dtype(dt).map_err(f)
        };
        let y = rnd(&[1, t, nc], 2.0, Dtype::Bfloat16)?;
        let (qn, kn) = (rnd(&[d], 0.5, Dtype::Bfloat16)?, rnd(&[d], 0.5, Dtype::Bfloat16)?);
        let ang = rnd(&[t, d], 3.0, Dtype::Float32)?;
        let (rc, rs) = (mlx_rs::ops::cos(&ang).map_err(f)?, mlx_rs::ops::sin(&ang).map_err(f)?);
        let (gq, gk, gv) = self.apply(&y, &qn, &kn, &rc, &rs).map_err(f)?;
        let parts = mlx_rs::ops::split_at_indices(&y, &[h * d, h * d + kv * d, h * d + 2 * kv * d], -1).map_err(f)?;
        let rope = |x: &Array| -> Result<Array> {
            let xf = x.as_dtype(Dtype::Float32).map_err(f)?;
            let hp = mlx_rs::ops::split_at_indices(&xf, &[d / 2], -1).map_err(f)?;
            let rot = mlx_rs::ops::concatenate(&[hp[1].negative().map_err(f)?, hp[0].clone()], -1).map_err(f)?;
            xf.multiply(&rc).map_err(f)?.add(rot.multiply(&rs).map_err(f)?).map_err(f)?.as_dtype(Dtype::Bfloat16).map_err(f)
        };
        let wq = rope(&fast::rms_norm(parts[0].reshape(&[1, t, h, d]).map_err(f)?, Some(&qn), eps).map_err(f)?
            .transpose_axes(&[0, 2, 1, 3]).map_err(f)?)?;
        let wk = rope(&fast::rms_norm(parts[1].reshape(&[1, t, kv, d]).map_err(f)?, Some(&kn), eps).map_err(f)?
            .transpose_axes(&[0, 2, 1, 3]).map_err(f)?)?;
        let wv = parts[2].reshape(&[1, t, kv, d]).map_err(f)?.transpose_axes(&[0, 2, 1, 3]).map_err(f)?;
        for (name, g, w) in [("q", gq, wq), ("k", gk, wk), ("v", gv, wv)] {
            let w32 = w.as_dtype(Dtype::Float32).map_err(f)?;
            let dmax = g.as_dtype(Dtype::Float32).map_err(f)?.subtract(&w32).map_err(f)?.abs().map_err(f)?
                .max(None).map_err(f)?.item_exact::<f32>();
            let smax = w32.abs().map_err(f)?.max(None).map_err(f)?.item_exact::<f32>();
            if !(dmax <= smax / 128.0) || !smax.is_finite() {
                bail!("qkprep self-test: {name} max diff {dmax} against max {smax}");
            }
        }
        Ok(())
    }
}

impl Drop for QkPrep {
    fn drop(&mut self) {
        unsafe { sys::mlx_fast_metal_kernel_free(self.kernel) };
    }
}
