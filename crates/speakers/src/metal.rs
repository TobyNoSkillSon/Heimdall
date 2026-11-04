//! Debug-gated GPU path. One shader library/queue per encoder; shared by its
//! operators. No model tensors or queues survive the owning call-scoped model.
use ::metal::*;
use anyhow::{Result, anyhow, ensure};
use half::f16;
use objc::{sel, sel_impl};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
static GPU_NANOS: AtomicU64 = AtomicU64::new(0);
/// Summed command-buffer GPU intervals. Concurrent buffers can overlap;
/// this measures submitted work intervals, not whole-device busy time.
pub fn gpu_seconds() -> f64 {
    GPU_NANOS.load(Ordering::Relaxed) as f64 / 1e9
}
#[unsafe(no_mangle)]
extern "C" fn heimdall_metal_graph_time(seconds: f64) {
    if seconds.is_finite() && seconds > 0. {
        GPU_NANOS.fetch_add((seconds * 1e9) as u64, Ordering::Relaxed);
    }
}

mod nemotron;
mod proto;
mod redim;
pub(crate) use nemotron::{NemotronGpu, drain_native_scratch};
pub(crate) use redim::session;

pub fn enabled(model: &str) -> bool {
    std::env::var("HEIMDALL_SPEAKERS_METAL").is_ok_and(|v| v == "1" || v == model)
}
pub struct Gpu {
    device: Device,
    queue: CommandQueue,
    pack: ComputePipelineState,
    linear: ComputePipelineState,
    mv: ComputePipelineState,
    gemm: ComputePipelineState,
    big: ComputePipelineState,
    grouped: ComputePipelineState,
    relu_bn: ComputePipelineState,
    gpu_seconds: f64,
    convolutions: u64,
}
impl Drop for Gpu {
    fn drop(&mut self) {
        if std::env::var_os("HEIMDALL_SPEAKERS_SHADER_TRACE").is_some() {
            eprintln!(
                "speakers shader convolutions={} gpu_seconds={:.6}",
                self.convolutions, self.gpu_seconds
            );
        }
    }
}
impl Gpu {
    pub fn new() -> Result<Self> {
        let device = Device::system_default().ok_or_else(|| anyhow!("no Metal device"))?;
        let library = device
            .new_library_with_data(include_bytes!(concat!(
                env!("OUT_DIR"),
                "/speakers.metallib"
            )))
            .map_err(|e| anyhow!(e))?;
        let pipeline = |name| -> Result<_> {
            let f = library.get_function(name, None).map_err(|e| anyhow!(e))?;
            device
                .new_compute_pipeline_state_with_function(&f)
                .map_err(|e| anyhow!(e))
        };
        Ok(Self {
            queue: device.new_command_queue(),
            pack: pipeline("pack_conv")?,
            linear: pipeline("pack_linear")?,
            mv: pipeline("projection_mv")?,
            gemm: pipeline("gemm")?,
            big: pipeline("gemm_big")?,
            grouped: pipeline("grouped_conv")?,
            relu_bn: pipeline("relu_bn")?,
            device,
            gpu_seconds: 0.,
            convolutions: 0,
        })
    }
    fn buffer<T>(&self, v: &[T]) -> Buffer {
        self.device.new_buffer_with_data(
            v.as_ptr().cast(),
            std::mem::size_of_val(v) as u64,
            MTLResourceOptions::StorageModeShared,
        )
    }
    fn allocate(&self, bytes: usize) -> Buffer {
        self.device
            .new_buffer(bytes as u64, MTLResourceOptions::StorageModeShared)
    }
    // Caller retains the CPU allocation through GPU completion. Both input and
    // output tensor borrows outlive this buffer; no ownership/free is transferred.
    fn borrow(&self, ptr: usize, length: usize) -> (Buffer, u64) {
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
        let base = ptr / page * page;
        let offset = ptr - base;
        (
            self.device.new_buffer_with_bytes_no_copy(
                base as *const std::ffi::c_void,
                (length + offset).next_multiple_of(page) as u64,
                MTLResourceOptions::StorageModeShared,
                None,
            ),
            offset as u64,
        )
    }
    fn weights(&self, w: &[f32], n: usize, k: usize, padded: bool) -> Result<Buffer> {
        ensure!(
            w.len() == n * k && w.iter().all(|v| v.is_finite()),
            "invalid convolution weights"
        );
        let kp = if padded { k.next_multiple_of(32) } else { k };
        let np = if padded { n.next_multiple_of(32) } else { n };
        let mut h = vec![f16::ZERO; np * kp];
        for r in 0..n {
            for c in 0..k {
                h[r * kp + c] = f16::from_f32(w[r * k + c]);
            }
        }
        ensure!(h.iter().all(|v| v.is_finite()), "FP16 weight overflow");
        Ok(self.buffer(&h))
    }
    #[allow(clippy::too_many_arguments)] // Explicit buffers/offsets mirror Metal bindings.
    fn encode_gemm(
        &self,
        e: &ComputeCommandEncoderRef,
        a: &BufferRef,
        w: &BufferRef,
        b: &BufferRef,
        out: &BufferRef,
        offset: u64,
        d: [u32; 4],
        layout: [u32; 4],
    ) {
        let big = layout[0] > 0 && d[0] >= 128 && d[2] >= 64;
        e.set_compute_pipeline_state(if big { &self.big } else { &self.gemm });
        e.set_buffer(0, Some(a), 0);
        e.set_buffer(1, Some(w), 0);
        e.set_buffer(2, Some(b), 0);
        e.set_buffer(3, Some(out), offset);
        bytes(e, 4, &d);
        bytes(e, 5, &layout);
        e.dispatch_thread_groups(
            MTLSize::new(
                d[2].div_ceil(if big { 128 } else { 32 }) as u64,
                d[0].div_ceil(if big { 64 } else { 32 }) as u64,
                1,
            ),
            MTLSize::new(if big { 256 } else { 32 }, 1, 1),
        );
    }
    pub fn stats(&self) -> (u64, f64) {
        (self.convolutions, self.gpu_seconds)
    }
}
fn bytes<T>(e: &ComputeCommandEncoderRef, index: u64, value: &T) {
    e.set_bytes(
        index,
        std::mem::size_of::<T>() as u64,
        (value as *const T).cast(),
    );
}
fn dispatch(e: &ComputeCommandEncoderRef, count: usize) {
    e.dispatch_threads(MTLSize::new(count as u64, 1, 1), MTLSize::new(256, 1, 1));
}
#[derive(Clone, Copy, Debug)]
pub struct ConvSpec {
    pub input: [usize; 4],
    pub output_channels: usize,
    pub kernel: [usize; 2],
    pub strides: [usize; 2],
    pub pads: [usize; 2],
    pub dilations: [usize; 2],
    pub groups: usize,
}
impl ConvSpec {
    pub fn output(&self) -> Result<[usize; 4]> {
        let [b, c, h, w] = self.input;
        let o = self.output_channels;
        ensure!(
            b > 0
                && c > 0
                && h > 0
                && w > 0
                && o > 0
                && self.groups > 0
                && c.is_multiple_of(self.groups)
                && o.is_multiple_of(self.groups),
            "invalid convolution dimensions"
        );
        let mut out = [b, o, 0, 0];
        for (i, size) in [h, w].into_iter().enumerate() {
            ensure!(
                self.kernel[i] > 0 && self.strides[i] > 0 && self.dilations[i] > 0,
                "invalid convolution geometry"
            );
            let extent = self.dilations[i]
                .checked_mul(self.kernel[i] - 1)
                .and_then(|v| v.checked_add(1))
                .ok_or_else(|| anyhow!("convolution extent overflow"))?;
            let padded = self.pads[i]
                .checked_mul(2)
                .and_then(|v| size.checked_add(v))
                .ok_or_else(|| anyhow!("convolution padding overflow"))?;
            ensure!(padded >= extent, "convolution kernel exceeds padded input");
            out[i + 2] = (padded - extent) / self.strides[i] + 1;
        }
        for dims in [self.input, out] {
            ensure!(
                dims.iter()
                    .try_fold(1_usize, |v, n| v.checked_mul(*n))
                    .is_some_and(|v| v <= 64 * 1024 * 1024),
                "convolution tensor exceeds memory budget"
            );
        }
        Ok(out)
    }
}
pub struct Convolution {
    gpu: Arc<Mutex<Gpu>>,
    weights: Buffer,
    bias: Buffer,
    has_bias: bool,
    channels: usize,
    output_channels: usize,
    kernel: [usize; 2],
    strides: [usize; 2],
    pads: [usize; 2],
    dilations: [usize; 2],
    groups: usize,
}
impl Convolution {
    #[allow(clippy::too_many_arguments)] // Explicit source/destination binding offsets.
    fn encode_same(
        &self,
        g: &Gpu,
        e: &ComputeCommandEncoderRef,
        x: &BufferRef,
        xoffset: u64,
        y: &BufferRef,
        yoffset: u64,
        shape: [usize; 4],
        scratch: &BufferRef,
    ) -> Result<()> {
        let [b, c, h, w] = shape;
        ensure!(
            c == self.channels
                && c == self.output_channels
                && self.strides == [1, 1]
                && self.dilations == [1, 1]
                && ((self.groups == c && self.kernel == [3, 3] && self.pads == [1, 1])
                    || (self.groups == 1 && self.kernel == [1, 1] && self.pads == [0, 0])),
            "unsupported ConvBlock geometry"
        );
        let m = b * h * w;
        let kp = c.next_multiple_of(32);
        let mp = m.next_multiple_of(32);
        let p = [
            b,
            c,
            h,
            w,
            c,
            h,
            w,
            self.kernel[0],
            self.kernel[1],
            1,
            1,
            self.pads[0],
            self.pads[1],
            1,
            1,
            self.groups,
            kp,
            mp,
        ]
        .map(|v| v as u32);
        if self.groups == 1 {
            e.set_compute_pipeline_state(&g.pack);
            e.set_buffer(0, Some(x), xoffset);
            e.set_buffer(1, Some(scratch), 0);
            bytes(e, 2, &p);
            dispatch(e, kp * mp);
            e.memory_barrier_with_resources(&[scratch]);
            g.encode_gemm(
                e,
                scratch,
                &self.weights,
                &self.bias,
                y,
                yoffset,
                [m as u32, kp as u32, c as u32, mp as u32],
                [(h * w) as u32, self.has_bias as u32, 0, 0],
            );
        } else {
            e.set_compute_pipeline_state(&g.grouped);
            e.set_buffer(0, Some(x), xoffset);
            e.set_buffer(1, Some(&self.weights), 0);
            e.set_buffer(2, Some(&self.bias), 0);
            e.set_buffer(3, Some(y), yoffset);
            bytes(e, 4, &p);
            bytes(e, 5, &(self.has_bias as u32));
            dispatch(e, b * c * h * w);
        }
        e.memory_barrier_with_resources(&[y, scratch]);
        Ok(())
    }
    pub fn new(
        gpu: Arc<Mutex<Gpu>>,
        weights: &[f32],
        bias: Option<&[f32]>,
        spec: ConvSpec,
    ) -> Result<Self> {
        spec.output()?;
        ensure!(
            bias.is_none_or(|b| b.len() == spec.output_channels && b.iter().all(|v| v.is_finite())),
            "invalid convolution bias"
        );
        let g = gpu.lock().map_err(|_| anyhow!("Metal lock poisoned"))?;
        let w = g.weights(
            weights,
            spec.output_channels,
            spec.input[1] / spec.groups * spec.kernel[0] * spec.kernel[1],
            spec.groups == 1,
        )?;
        let b = g.buffer(bias.unwrap_or(&[0.]));
        drop(g);
        Ok(Self {
            gpu,
            weights: w,
            bias: b,
            has_bias: bias.is_some(),
            channels: spec.input[1],
            output_channels: spec.output_channels,
            kernel: spec.kernel,
            strides: spec.strides,
            pads: spec.pads,
            dilations: spec.dilations,
            groups: spec.groups,
        })
    }
    pub fn run(&self, x: &[f32], shape: [usize; 4]) -> Result<Vec<f32>> {
        ensure!(
            x.iter().all(|v| v.is_finite()),
            "nonfinite convolution input"
        );
        let spec = ConvSpec {
            input: shape,
            output_channels: self.output_channels,
            kernel: self.kernel,
            strides: self.strides,
            pads: self.pads,
            dilations: self.dilations,
            groups: self.groups,
        };
        let mut values = vec![0.; spec.output()?.iter().product()];
        self.run_into(x, shape, &mut values)?;
        ensure!(
            values.iter().all(|v| v.is_finite()),
            "nonfinite GPU convolution output"
        );
        Ok(values)
    }
    #[allow(unexpected_cfgs)] // objc 0.2 selector macros check its historical cargo-clippy feature.
    pub(crate) fn run_into(&self, x: &[f32], shape: [usize; 4], values: &mut [f32]) -> Result<()> {
        objc2::rc::autoreleasepool(|_| {
            let spec = ConvSpec {
                input: shape,
                output_channels: self.output_channels,
                kernel: self.kernel,
                strides: self.strides,
                pads: self.pads,
                dilations: self.dilations,
                groups: self.groups,
            };
            let out = spec.output()?;
            ensure!(
                shape[1] == self.channels && x.len() == shape.iter().product::<usize>(),
                "invalid convolution input"
            );
            let mut g = self
                .gpu
                .lock()
                .map_err(|_| anyhow!("Metal lock poisoned"))?;
            let m = out[0] * out[2] * out[3];
            let k = shape[1] * self.kernel[0] * self.kernel[1];
            let kp = k.next_multiple_of(32);
            let mp = m.next_multiple_of(32);
            ensure!(
                kp.checked_mul(mp).is_some_and(|v| v <= 128 * 1024 * 1024),
                "convolution scratch exceeds memory budget"
            );
            let p = [
                shape[0],
                shape[1],
                shape[2],
                shape[3],
                out[1],
                out[2],
                out[3],
                self.kernel[0],
                self.kernel[1],
                self.strides[0],
                self.strides[1],
                self.pads[0],
                self.pads[1],
                self.dilations[0],
                self.dilations[1],
                self.groups,
                kp,
                mp,
            ]
            .map(|v| v as u32);
            ensure!(
                values.len() == out.iter().product::<usize>(),
                "invalid convolution output length"
            );
            // Share the ORT arena pages directly. The slices remain borrowed until
            // waitUntilCompleted; no deallocator/free is attached. Kernels address
            // only the logical tensor slice, including when arena tensors share a page.
            let (xb, xoffset) = g.borrow(x.as_ptr() as usize, std::mem::size_of_val(x));
            let (y, yoffset) =
                g.borrow(values.as_mut_ptr() as usize, std::mem::size_of_val(values));
            let cmd = crate::gpu_recovery::command_buffer(&g.queue)?;
            let e = cmd.new_compute_command_encoder();
            let scratch;
            if self.groups == 1 {
                scratch = Some(g.allocate(kp * mp * 2));
                let a = scratch.as_ref().unwrap();
                e.set_compute_pipeline_state(&g.pack);
                e.set_buffer(0, Some(&xb), xoffset);
                e.set_buffer(1, Some(a), 0);
                bytes(e, 2, &p);
                dispatch(e, kp * mp);
                e.memory_barrier_with_resources(&[a]);
                g.encode_gemm(
                    e,
                    a,
                    &self.weights,
                    &self.bias,
                    &y,
                    yoffset,
                    [m as u32, kp as u32, out[1] as u32, mp as u32],
                    [(out[2] * out[3]) as u32, self.has_bias as u32, 0, 0],
                );
            } else {
                scratch = None;
                e.set_compute_pipeline_state(&g.grouped);
                e.set_buffer(0, Some(&xb), xoffset);
                e.set_buffer(1, Some(&self.weights), 0);
                e.set_buffer(2, Some(&self.bias), 0);
                e.set_buffer(3, Some(&y), yoffset);
                bytes(e, 4, &p);
                bytes(e, 5, &(self.has_bias as u32));
                dispatch(e, out.iter().product());
            }
            e.end_encoding();
            cmd.commit();
            crate::gpu_recovery::completed(cmd)?;
            // command-buffer timestamps measure this process's GPU interval;
            // unlike whole-device macmon utilization they exclude other queues.
            let start: f64 = unsafe { ::metal::objc::msg_send![cmd, GPUStartTime] };
            let end: f64 = unsafe { ::metal::objc::msg_send![cmd, GPUEndTime] };
            g.gpu_seconds += (end - start).max(0.);
            g.convolutions += 1;
            heimdall_metal_graph_time(end - start);
            drop(scratch);
            Ok(())
        })
    }
}
