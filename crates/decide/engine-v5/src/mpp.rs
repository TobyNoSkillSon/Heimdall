//! Linear layers through Metal 4 tensor ops (MetalPerformancePrimitives `matmul2d`, which the M5's GPU neural
//! accelerators run), as an MLX custom kernel: y[M, N] = x[M, K] · w[N, K]^T, BF16 (or FP16) in and out, float
//! accumulation inside the op. On by default (D1E_TENSOR_MM=0 turns it off); it checks itself against MLX's matmul at load and is
//! turned off on any failure or on a GPU without neural accelerators (before the M5), where MLX's own kernels run.

use anyhow::{anyhow, bail, Result};
use mlx_rs::{error::Exception, ops, Array, Dtype, Stream};
use mlx_sys as sys;
use std::ffi::CString;

/// Tile: 64 rows of x by 128 rows of w per threadgroup of 4 simdgroups (the best of the measured configurations).
const TM: i32 = 64;
const TN: i32 = 128;
const SG: i32 = 4;

pub struct TensorGemm {
    kernel: sys::mlx_fast_metal_kernel,
    dtype: Dtype,
    /// the w1/w3 GEMM with the SwiGLU product in its epilogue (D1E_SWIGLU), on interleaved [w1_0, w3_0, w1_1, ...]
    swiglu: Option<sys::mlx_fast_metal_kernel>,
}

pub(crate) fn make_kernel(name: &str, inputs: &[&str], source: &str, header: &str) -> Result<sys::mlx_fast_metal_kernel> {
    let name = CString::new(name)?;
    let src = CString::new(source)?;
    let header = CString::new(header)?;
    unsafe {
        let ins = sys::mlx_vector_string_new();
        let outs = sys::mlx_vector_string_new();
        for n in inputs {
            let c = CString::new(*n)?;
            sys::mlx_vector_string_append_value(ins, c.as_ptr());
        }
        let o = CString::new("y")?;
        sys::mlx_vector_string_append_value(outs, o.as_ptr());
        let kernel = sys::mlx_fast_metal_kernel_new(name.as_ptr(), ins, outs, src.as_ptr(), header.as_ptr(), true, false);
        sys::mlx_vector_string_free(ins);
        sys::mlx_vector_string_free(outs);
        if kernel.ctx.is_null() {
            bail!("tensor gemm: kernel creation returned null");
        }
        Ok(kernel)
    }
}

/// One custom-kernel launch: `inputs` in order, one output of `shape`, a grid of `grid` threads in groups of `tg`.
pub(crate) fn launch(kernel: sys::mlx_fast_metal_kernel, inputs: &[&Array], shape: &[i32], dtype: Dtype, grid: (i32, i32),
          tg: i32) -> std::result::Result<Array, Exception> {
    let stream = Stream::thread_local_or_default();
    unsafe {
        let config = sys::mlx_fast_metal_kernel_config_new();
        let r = (|| {
            let dt = if dtype == Dtype::Float16 { sys::mlx_dtype__MLX_FLOAT16 } else { sys::mlx_dtype__MLX_BFLOAT16 };
            status(sys::mlx_fast_metal_kernel_config_add_output_arg(config, shape.as_ptr(), shape.len(), dt), "output")?;
            status(sys::mlx_fast_metal_kernel_config_set_grid(config, grid.0, grid.1, 1), "grid")?;
            status(sys::mlx_fast_metal_kernel_config_set_thread_group(config, tg, 1, 1), "thread group")?;
            let ins = sys::mlx_vector_array_new();
            for a in inputs {
                sys::mlx_vector_array_append_value(ins, a.as_ptr());
            }
            let mut outputs = sys::mlx_vector_array_new();
            let code = sys::mlx_fast_metal_kernel_apply(&mut outputs, kernel, ins, config, stream.as_ptr());
            sys::mlx_vector_array_free(ins);
            if code != 0 {
                sys::mlx_vector_array_free(outputs);
                return Err(Exception::custom("tensor gemm: apply failed"));
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

fn status(code: i32, what: &str) -> std::result::Result<(), Exception> {
    if code == 0 { Ok(()) } else { Err(Exception::custom(format!("tensor gemm: {what} failed"))) }
}

/// The GPU has neural accelerators (Apple M5 or later), read from the chip name.
pub fn has_neural_accelerators() -> bool {
    let out = std::process::Command::new("sysctl").args(["-n", "machdep.cpu.brand_string"]).output();
    let name = out.map(|o| String::from_utf8_lossy(&o.stdout).to_string()).unwrap_or_default();
    // "Apple M5", "Apple M5 Max", ... ; later generations keep the neural accelerators
    name.split_whitespace()
        .find_map(|w| w.strip_prefix('M').and_then(|n| n.parse::<u32>().ok()))
        .is_some_and(|gen| gen >= 5)
}

impl TensorGemm {
    pub fn new(dtype: Dtype, swiglu: bool) -> Result<Self> {
        let ty = match dtype {
            Dtype::Bfloat16 => "bfloat",
            Dtype::Float16 => "half",
            other => bail!("tensor gemm: no {other:?} variant"),
        };
        let header = "#include <metal_tensor>\n#include <MetalPerformancePrimitives/MetalPerformancePrimitives.h>\n\
                      using namespace mpp::tensor_ops;\n";
        let source = format!(
            r#"
    const int M = int(dims[0]), N = int(dims[1]), K = int(dims[2]);
    auto X = tensor<device {ty}, dextents<int32_t, 2>, tensor_inline>((device {ty}*)x, dextents<int32_t, 2>(K, M));
    auto W = tensor<device {ty}, dextents<int32_t, 2>, tensor_inline>((device {ty}*)w, dextents<int32_t, 2>(K, N));
    auto Y = tensor<device {ty}, dextents<int32_t, 2>, tensor_inline>((device {ty}*)y, dextents<int32_t, 2>(N, M));
    constexpr auto desc = matmul2d_descriptor({TM}, {TN}, dynamic_length_v<int>, false, true, false);
    matmul2d<desc, execution_simdgroups<{SG}>> op;
    const uint n0 = threadgroup_position_in_grid.x * {TN};
    const uint m0 = threadgroup_position_in_grid.y * {TM};
    auto xs = X.slice(0, m0);
    auto ws = W.slice(0, n0);
    auto ys = Y.slice(n0, m0);
    op.run(xs, ws, ys);
"#
        );
        let kernel = make_kernel(&format!("d1e_tensor_gemm_{ty}"), &["x", "w", "dims"], &source, header)?;
        let swiglu = if swiglu {
            let src = format!(
                r#"
    const int M = int(dims[0]), N = int(dims[1]), K = int(dims[2]);   // N = 2 x FFN width, w interleaved
    auto X = tensor<device {ty}, dextents<int32_t, 2>, tensor_inline>((device {ty}*)x, dextents<int32_t, 2>(K, M));
    auto W = tensor<device {ty}, dextents<int32_t, 2>, tensor_inline>((device {ty}*)w, dextents<int32_t, 2>(K, N));
    constexpr auto desc = matmul2d_descriptor({TM}, {TN}, dynamic_length_v<int>, false, true, false,
                                              matmul2d_descriptor::mode::multiply_accumulate);
    matmul2d<desc, execution_simdgroups<{SG}>> op;
    const uint n0 = threadgroup_position_in_grid.x * {TN};
    const uint m0 = threadgroup_position_in_grid.y * {TM};
    auto xs = X.slice(0, m0);
    auto ws = W.slice(0, n0);
    auto acc = op.get_destination_cooperative_tensor<decltype(xs), decltype(ws), float>();
    for (uint16_t e = 0; e < acc.get_capacity(); ++e) if (acc.is_valid_element(e)) acc[e] = 0.f;
    op.run(xs, ws, acc);
    threadgroup {ty} tile[{TM} * {TN}];
    for (uint16_t e = 0; e < acc.get_capacity(); ++e) {{
        if (!acc.is_valid_element(e)) continue;
        auto ix = acc.get_multidimensional_index(e);
        tile[ix[1] * {TN} + ix[0]] = {ty}(acc[e]);   // the GEMM output rounded as MLX's matmul writes it
    }}
    threadgroup_barrier(mem_flags::mem_threadgroup);
    const int half_n = N / 2;
    for (uint i = thread_index_in_threadgroup; i < {TM} * ({TN} / 2); i += {SG} * 32) {{
        const uint r = i / ({TN} / 2), c = i % ({TN} / 2);
        const uint gm = m0 + r, gn = n0 / 2 + c;
        if (int(gm) >= M || int(gn) >= half_n) continue;
        const float a = float(tile[r * {TN} + 2 * c]), b = float(tile[r * {TN} + 2 * c + 1]);
        const {ty} sg = {ty}(1.0f / (1.0f + metal::exp(-a)));        // sigmoid(a), rounded
        const {ty} sa = {ty}(a * float(sg));                          // silu(a) = a * sigmoid(a), rounded
        y[size_t(gm) * half_n + gn] = {ty}(float(sa) * b);            // silu(a) * b
    }}
"#
            );
            Some(make_kernel(&format!("d1e_swiglu_gemm_{ty}"), &["x", "w", "dims"], &src, header)?)
        } else {
            None
        };
        Ok(Self { kernel, dtype, swiglu })
    }

    pub fn has_swiglu(&self) -> bool {
        self.swiglu.is_some()
    }

    /// silu(x · w1^T) * (x · w3^T) for `w` the interleaved [w1_0, w3_0, w1_1, w3_1, ...] rows (2F, K) -> [..., F]
    pub fn apply_swiglu(&self, x: &Array, w: &Array) -> std::result::Result<Array, Exception> {
        let kernel = self.swiglu.ok_or_else(|| Exception::custom("tensor gemm: no swiglu kernel"))?;
        let xs = x.shape().to_vec();
        let k = *xs.last().unwrap();
        let n = w.shape()[0];
        let m: i32 = xs[..xs.len() - 1].iter().product();
        let x2 = x.reshape(&[m, k])?.as_dtype(self.dtype)?;
        let w = w.as_dtype(self.dtype)?;
        let dims = Array::from_slice(&[m as u32, n as u32, k as u32], &[3]);
        let out = launch(kernel, &[&x2, &w, &dims], &[m, n / 2], self.dtype,
                         ((n + TN - 1) / TN * 32 * SG, (m + TM - 1) / TM), 32 * SG)?;
        let mut shape = xs[..xs.len() - 1].to_vec();
        shape.push(n / 2);
        out.reshape(&shape)
    }

    /// x [..., K] · w[N, K]^T -> [..., N]
    pub fn apply(&self, x: &Array, w: &Array) -> std::result::Result<Array, Exception> {
        let xs = x.shape().to_vec();
        let k = *xs.last().unwrap();
        let (n, kw) = (w.shape()[0], w.shape()[1]);
        if kw != k {
            return Err(Exception::custom(format!("tensor gemm: x {xs:?} against w {:?}", w.shape())));
        }
        let m: i32 = xs[..xs.len() - 1].iter().product();
        let x2 = x.reshape(&[m, k])?.as_dtype(self.dtype)?;
        let w = w.as_dtype(self.dtype)?;
        let dims = Array::from_slice(&[m as u32, n as u32, k as u32], &[3]);
        let out = launch(self.kernel, &[&x2, &w, &dims], &[m, n], self.dtype,
                         ((n + TN - 1) / TN * 32 * SG, (m + TM - 1) / TM), 32 * SG)?;
        let mut shape = xs[..xs.len() - 1].to_vec();
        shape.push(n);
        out.reshape(&shape)
    }

    /// Against MLX's matmul on unaligned shapes (partial tiles at both edges): every element within one rounding step
    /// of the output type of MLX's result, relative to the largest magnitude.
    pub fn self_test(&self) -> Result<()> {
        if std::env::var("D1E_TEST_TENSOR_MM_FAIL").is_ok_and(|v| v == "1") {
            bail!("tensor gemm self-test: forced to fail by D1E_TEST_TENSOR_MM_FAIL");
        }
        let f = |err: Exception| anyhow!("tensor gemm self-test: {err}");
        for (m, n, k) in [(77, 200, 256), (130, 3088, 2048), (1852, 256, 7680)] {
            let x = mlx_rs::random::normal::<f32>(&[1, m, k], None, None, None).map_err(f)?
                .as_dtype(self.dtype).map_err(f)?;
            let w = mlx_rs::random::normal::<f32>(&[n, k], None, None, None).map_err(f)?
                .multiply(Array::from_f32(0.02)).map_err(f)?.as_dtype(self.dtype).map_err(f)?;
            let got = self.apply(&x, &w).map_err(f)?.as_dtype(Dtype::Float32).map_err(f)?;
            let want = ops::matmul(&x, w.transpose().map_err(f)?).map_err(f)?.as_dtype(Dtype::Float32).map_err(f)?;
            let diff = got.subtract(&want).map_err(f)?.abs().map_err(f)?.max(None).map_err(f)?;
            let scale = want.abs().map_err(f)?.max(None).map_err(f)?;
            mlx_rs::transforms::eval([&diff, &scale]).map_err(f)?; // a compile failure errors here, not in item_exact
            let (d, s) = (diff.item_exact::<f32>(), scale.item_exact::<f32>());
            if !(d <= s / 64.0) || !s.is_finite() || s == 0.0 {
                bail!("tensor gemm self-test {m}x{n}x{k}: max diff {d} against max {s}");
            }
        }
        if self.swiglu.is_some() {
            for (m, f, k) in [(77, 192, 256), (1852, 512, 2048)] {
                let x = mlx_rs::random::normal::<f32>(&[1, m, k], None, None, None).map_err(f_)?.as_dtype(self.dtype).map_err(f_)?;
                let w13 = mlx_rs::random::normal::<f32>(&[2 * f, k], None, None, None).map_err(f_)?
                    .multiply(Array::from_f32(0.03)).map_err(f_)?.as_dtype(self.dtype).map_err(f_)?;
                // the plain path: [w1; w3] stacked, matmul, split, silu(a) * b
                let ab = ops::matmul(&x, w13.transpose().map_err(f_)?).map_err(f_)?;
                let parts = ops::split_equal(&ab, 2, -1).map_err(f_)?;
                let a = &parts[0];
                let want = a.multiply(mlx_rs::nn::sigmoid(a).map_err(f_)?).map_err(f_)?.multiply(&parts[1]).map_err(f_)?
                    .as_dtype(Dtype::Float32).map_err(f_)?;
                let inter = interleave(&w13).map_err(f_)?;
                let got = self.apply_swiglu(&x, &inter).map_err(f_)?.as_dtype(Dtype::Float32).map_err(f_)?;
                let diff = got.subtract(&want).map_err(f_)?.abs().map_err(f_)?.max(None).map_err(f_)?;
                let scale = want.abs().map_err(f_)?.max(None).map_err(f_)?;
                mlx_rs::transforms::eval([&diff, &scale]).map_err(f_)?;
                let (d, s) = (diff.item_exact::<f32>(), scale.item_exact::<f32>());
                if !(d <= s / 64.0) || !s.is_finite() || s == 0.0 {
                    bail!("swiglu gemm self-test {m}x{f}x{k}: max diff {d} against max {s}");
                }
            }
        }
        Ok(())
    }
}

fn f_(err: Exception) -> anyhow::Error {
    anyhow!("tensor gemm self-test: {err}")
}

/// [w1; w3] stacked rows (2F, K) -> interleaved [w1_0, w3_0, w1_1, w3_1, ...] (2F, K): the same values, reordered.
pub fn interleave(w13: &Array) -> std::result::Result<Array, Exception> {
    let (n, k) = (w13.shape()[0], w13.shape()[1]);
    w13.reshape(&[2, n / 2, k])?.transpose_axes(&[1, 0, 2])?.reshape(&[n, k])
}

impl Drop for TensorGemm {
    fn drop(&mut self) {
        unsafe {
            sys::mlx_fast_metal_kernel_free(self.kernel);
            if let Some(k) = self.swiglu {
                sys::mlx_fast_metal_kernel_free(k);
            }
        }
    }
}
