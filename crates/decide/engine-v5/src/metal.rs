//! The short-conv block as one custom Metal kernel (D1E_METAL), through mlx-c's `mlx_fast_metal_kernel` (mlx-rs does
//! not wrap it). Input: the in_proj output `bcx` (1, T, 3C) holding B, C and x side by side, and the depthwise conv
//! weight (C, k). Output (1, T, C): C * causal_conv(B * x), with the plain path's BF16 roundings (B * x, each
//! product, each partial sum in the same order, C * y), so it reproduces the plain ops in one pass.

use anyhow::{anyhow, bail, Result};
use mlx_rs::{error::Exception, ops, Array, Dtype, Stream};
use mlx_sys as sys;
use std::ffi::CString;

pub struct ConvKernel {
    kernel: sys::mlx_fast_metal_kernel,
    channels: i32,
    width: i32,
    dtype: Dtype,
}

fn status(code: i32, what: &str) -> std::result::Result<(), Exception> {
    if code == 0 {
        Ok(())
    } else {
        Err(Exception::custom(format!("metal kernel: {what} failed")))
    }
}

impl ConvKernel {
    pub fn new(channels: i32, width: i32, dtype: Dtype) -> Result<Self> {
        let ty = match dtype {
            Dtype::Bfloat16 => "bfloat16_t",
            Dtype::Float16 => "half",
            other => bail!("metal kernel: no {other:?} variant"),
        };
        let source = format!(
            r#"
    const uint ch = thread_position_in_grid.x;
    const uint t = thread_position_in_grid.y;
    const int C = {channels};
    const int K = {width};
    // the plain path's roundings: B*x, each product and each partial sum to BF16 (padding contributes exact zeros)
    {ty} acc = {ty}(0.0f);
    for (int i = 0; i < K; ++i) {{
        const int s = int(t) - (K - 1) + i;
        {ty} bx = {ty}(0.0f);
        if (s >= 0) {{
            const size_t row = size_t(s) * 3 * C;
            bx = {ty}(float(bcx[row + ch]) * float(bcx[row + 2 * C + ch]));
        }}
        const {ty} prod = {ty}(float(bx) * float(w[ch * K + i]));
        acc = (i == 0) ? prod : {ty}(float(acc) + float(prod));
    }}
    const float c = float(bcx[size_t(t) * 3 * C + C + ch]);
    out[size_t(t) * C + ch] = {ty}(c * float(acc));
"#
        );
        let name = CString::new(format!("d1e_short_conv_{ty}"))?;
        let src = CString::new(source)?;
        let header = CString::new("")?;
        unsafe {
            let inputs = sys::mlx_vector_string_new();
            let outputs = sys::mlx_vector_string_new();
            for n in ["bcx", "w"] {
                let c = CString::new(n)?;
                sys::mlx_vector_string_append_value(inputs, c.as_ptr());
            }
            let o = CString::new("out")?;
            sys::mlx_vector_string_append_value(outputs, o.as_ptr());
            let kernel = sys::mlx_fast_metal_kernel_new(name.as_ptr(), inputs, outputs, src.as_ptr(), header.as_ptr(),
                                                        true, false);
            sys::mlx_vector_string_free(inputs);
            sys::mlx_vector_string_free(outputs);
            if kernel.ctx.is_null() {
                bail!("mlx_fast_metal_kernel_new returned null");
            }
            Ok(Self { kernel, channels, width, dtype })
        }
    }

    /// C * causal_conv(B * x) for `bcx` (1, T, 3C) and `w` (C, k), one thread per (channel, position).
    pub fn apply(&self, bcx: &Array, w: &Array) -> std::result::Result<Array, Exception> {
        let shape = bcx.shape();
        if shape.len() != 3 || shape[0] != 1 || shape[2] != 3 * self.channels {
            return Err(Exception::custom(format!("metal kernel: bcx shape {shape:?}")));
        }
        if w.shape() != [self.channels, self.width] {
            return Err(Exception::custom(format!("metal kernel: w shape {:?}", w.shape())));
        }
        let t = shape[1];
        let bcx = bcx.as_dtype(self.dtype)?;
        let w = w.as_dtype(self.dtype)?;
        let stream = Stream::thread_local_or_default();
        unsafe {
            let config = sys::mlx_fast_metal_kernel_config_new();
            let out_shape = [1i32, t, self.channels];
            let r = (|| {
                status(sys::mlx_fast_metal_kernel_config_add_output_arg(config, out_shape.as_ptr(), 3,
                                                                       if self.dtype == Dtype::Float16 { sys::mlx_dtype__MLX_FLOAT16 } else { sys::mlx_dtype__MLX_BFLOAT16 }), "output")?;
                status(sys::mlx_fast_metal_kernel_config_set_grid(config, self.channels, t, 1), "grid")?;
                status(sys::mlx_fast_metal_kernel_config_set_thread_group(config, self.channels.min(256), 1, 1),
                       "thread group")?;
                let inputs = sys::mlx_vector_array_new();
                sys::mlx_vector_array_append_value(inputs, bcx.as_ptr());
                sys::mlx_vector_array_append_value(inputs, w.as_ptr());
                let mut outputs = sys::mlx_vector_array_new();
                let code = sys::mlx_fast_metal_kernel_apply(&mut outputs, self.kernel, inputs, config, stream.as_ptr());
                sys::mlx_vector_array_free(inputs);
                if code != 0 {
                    sys::mlx_vector_array_free(outputs);
                    return Err(Exception::custom("metal kernel: apply failed"));
                }
                let mut res = sys::mlx_array_new();
                let got = sys::mlx_vector_array_get(&mut res, outputs, 0);
                sys::mlx_vector_array_free(outputs);
                status(got, "output read")?;
                Ok(Array::from_ptr(res))
            })();
            sys::mlx_fast_metal_kernel_config_free(config);
            r
        }
    }

    /// The kernel against the plain ops on a random block (no weights): fails when they disagree, so the engine
    /// runs without it.
    pub fn self_test(&self) -> Result<()> {
        // a test hook for the load-time fallback: the check fails as a wrong kernel would
        if std::env::var("D1E_TEST_METAL_SELFTEST_FAIL").is_ok_and(|v| v == "1") {
            bail!("metal self-test: forced to fail by D1E_TEST_METAL_SELFTEST_FAIL");
        }
        let f = |err: Exception| anyhow!("metal self-test: {err}");
        let (c, k, t) = (self.channels, self.width, 37);
        let bcx = mlx_rs::random::normal::<f32>(&[1, t, 3 * c], None, None, None).map_err(f)?
            .as_dtype(self.dtype).map_err(f)?;
        let w = mlx_rs::random::normal::<f32>(&[c, k], None, None, None).map_err(f)?
            .multiply(Array::from_f32(0.3)).map_err(f)?.as_dtype(self.dtype).map_err(f)?;
        let got = self.apply(&bcx, &w).map_err(f)?;
        // the plain path's ops
        let parts = ops::split_equal(&bcx, 3, -1).map_err(f)?;
        let bx = parts[0].multiply(&parts[2]).map_err(f)?;
        let padded = ops::pad(&bx, &[(0, 0), (k - 1, 0), (0, 0)][..], None, None).map_err(f)?;
        let wcols = ops::split_equal(&w, k, 1).map_err(f)?;
        let mut y: Option<Array> = None;
        for i in 0..k {
            let s = ops::split_at_indices(&padded, &[i, i + t], 1).map_err(f)?[1].clone();
            let term = s.multiply(wcols[i as usize].reshape(&[-1]).map_err(f)?).map_err(f)?;
            y = Some(match y { None => term, Some(y) => y.add(term).map_err(f)? });
        }
        let want = parts[1].multiply(y.unwrap()).map_err(f)?;
        let diff = got.as_dtype(Dtype::Float32).map_err(f)?.subtract(want.as_dtype(Dtype::Float32).map_err(f)?)
            .map_err(f)?.abs().map_err(f)?.max(None).map_err(f)?;
        let scale = want.as_dtype(Dtype::Float32).map_err(f)?.abs().map_err(f)?.max(None).map_err(f)?;
        let (d, s) = (diff.item_exact::<f32>(), scale.item_exact::<f32>());
        if !(d <= 1e-6 * s) {
            bail!("metal self-test: max diff {d} against max {s}");
        }
        Ok(())
    }
}

impl Drop for ConvKernel {
    fn drop(&mut self) {
        unsafe { sys::mlx_fast_metal_kernel_free(self.kernel) };
    }
}
