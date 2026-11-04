//! CPU graph boundary, GPU pointwise arithmetic. One device/queue per TTS model.
use anyhow::{Result, ensure};
use half::f16;
use metal::{Buffer, ComputePipelineState, Device, MTLResourceOptions, MTLSize};
use ort::operator::{
    ComputeContext, Kernel, KernelContext, Operator, OperatorDomain, OperatorInput, OperatorOutput,
};
use ort::value::{TensorElementType, TensorValueType};
use std::sync::Arc;

pub struct Gpu {
    device: Device,
    queue: metal::CommandQueue,
    pack: ComputePipelineState,
    pointwise: ComputePipelineState,
}
impl Gpu {
    /// The shader/energy qualification currently covers this device only.
    pub fn qualified_device() -> bool {
        Device::system_default().is_some_and(|device| device.name() == "Apple M5 Max")
    }
    #[allow(unexpected_cfgs)] // objc 0.2's selector macro checks its obsolete cargo-clippy feature.
    pub fn new() -> Result<Arc<Self>> {
        let device = Device::system_default().ok_or_else(|| anyhow::anyhow!("no Metal device"))?;
        let library = device
            .new_library_with_data(include_bytes!(concat!(env!("OUT_DIR"), "/pointwise.metallib")))
            .map_err(anyhow::Error::msg)?;
        let pipeline = |name| -> Result<_> {
            let f = library
                .get_function(name, None)
                .map_err(anyhow::Error::msg)?;
            device
                .new_compute_pipeline_state_with_function(&f)
                .map_err(anyhow::Error::msg)
        };
        let pack = pipeline("pack")?;
        let pointwise = pipeline("pointwise")?;
        let queue = device.new_command_queue();
        Ok(Arc::new(Self {
            device,
            queue,
            pack,
            pointwise,
        }))
    }
    fn bytes<T>(&self, x: &[T]) -> Buffer {
        self.device.new_buffer_with_data(
            x.as_ptr().cast(),
            std::mem::size_of_val(x) as u64,
            MTLResourceOptions::StorageModeShared,
        )
    }
    fn buffer(&self, n: usize) -> Buffer {
        self.device
            .new_buffer(n as u64, MTLResourceOptions::StorageModeShared)
    }
    pub fn domain(self: &Arc<Self>) -> ort::Result<OperatorDomain> {
        OperatorDomain::new(super::graph_patch::DOMAIN)?.add(Pointwise(self.clone()))
    }
}
struct Pointwise(Arc<Gpu>);
impl Operator for Pointwise {
    type Kernel<'a> = PointwiseKernel;
    fn name(&self) -> &str {
        "Pointwise"
    }
    fn inputs(&self) -> impl IntoIterator<Item = OperatorInput> {
        [
            OperatorInput::required(TensorElementType::Float32),
            OperatorInput::required(TensorElementType::Float32),
            OperatorInput::optional(TensorElementType::Float32),
        ]
    }
    fn outputs(&self) -> impl IntoIterator<Item = OperatorOutput> {
        [OperatorOutput::required(TensorElementType::Float32)]
    }
    fn create_kernel<'a>(&self, ctx: &KernelContext<'a>) -> ort::Result<Self::Kernel<'a>> {
        let w = ctx.constant_input::<TensorValueType<f32>>(1)?;
        let (shape, values) = w.try_extract_tensor::<f32>()?;
        if shape.len() != 3 || shape[2] != 1 {
            return Err(ort::Error::new("invalid Metal pointwise weight rank"));
        }
        let n = shape[0] as usize;
        let k = shape[1] as usize;
        let weights: Vec<_> = values.iter().map(|x| f16::from_f32(*x)).collect();
        let bias = if ctx.inputs()?.len() == 3 {
            let v = ctx.constant_input::<TensorValueType<f32>>(2)?;
            v.try_extract_tensor::<f32>()?.1.to_vec()
        } else {
            vec![0.; n]
        };
        if shape.len() != 3
            || shape[2] != 1
            || !n.is_multiple_of(32)
            || !k.is_multiple_of(32)
            || bias.len() != n
            || bias.iter().any(|x| !x.is_finite())
            || weights.iter().any(|x| !x.is_finite())
        {
            return Err(ort::Error::new("invalid Metal pointwise weights"));
        }
        Ok(PointwiseKernel {
            gpu: self.0.clone(),
            w: self.0.bytes(&weights),
            bias: self.0.bytes(&bias),
            n,
            k,
            buffers: None,
        })
    }
}
pub struct PointwiseKernel {
    gpu: Arc<Gpu>,
    w: Buffer,
    bias: Buffer,
    n: usize,
    k: usize,
    buffers: Option<Buffers>,
}
struct Buffers {
    shape: [usize; 2],
    x: Buffer,
    a: Buffer,
    y: Buffer,
}
impl PointwiseKernel {
    fn run(&mut self, x: &[f32], batch: usize, time: usize, y: &mut [f32]) -> Result<()> {
        ensure!(
            batch > 0
                && time > 0
                && x.len() == batch * self.k * time
                && y.len() == batch * self.n * time,
            "invalid pointwise shape"
        );
        ensure!(
            x.iter().all(|v| v.is_finite() && v.abs() <= 65504.),
            "FP16 activation overflow"
        );
        let p = time.div_ceil(32) * 32;
        let m = batch * p;
        ensure!(
            m.checked_mul(self.k)
                .is_some_and(|v| v <= u32::MAX as usize)
                && batch
                    .checked_mul(self.n)
                    .and_then(|v| v.checked_mul(time))
                    .is_some_and(|v| v <= u32::MAX as usize),
            "Metal index overflow"
        );
        if self
            .buffers
            .as_ref()
            .is_none_or(|b| b.shape != [batch, time])
        {
            self.buffers = Some(Buffers {
                shape: [batch, time],
                x: self.gpu.buffer(x.len() * 4),
                a: self.gpu.buffer(m * self.k * 2),
                y: self.gpu.buffer(y.len() * 4),
            });
        }
        let b = self.buffers.as_ref().unwrap();
        unsafe {
            std::ptr::copy_nonoverlapping(x.as_ptr(), b.x.contents().cast(), x.len());
        }
        let cmd = crate::gpu_recovery::command_buffer(&self.gpu.queue)?;
        let enc = cmd.new_compute_command_encoder();
        enc.set_compute_pipeline_state(&self.gpu.pack);
        enc.set_buffer(0, Some(&b.x), 0);
        enc.set_buffer(1, Some(&b.a), 0);
        let pd = [self.k as u32, time as u32, p as u32, batch as u32];
        enc.set_bytes(2, 16, pd.as_ptr().cast());
        enc.dispatch_threads(
            MTLSize::new((m * self.k) as u64, 1, 1),
            MTLSize::new(256, 1, 1),
        );
        enc.set_compute_pipeline_state(&self.gpu.pointwise);
        enc.set_buffer(0, Some(&b.a), 0);
        enc.set_buffer(1, Some(&self.w), 0);
        enc.set_buffer(2, Some(&self.bias), 0);
        enc.set_buffer(3, Some(&b.y), 0);
        let d = [m as u32, self.k as u32, self.n as u32, p as u32];
        let bt = [batch as u32, time as u32];
        enc.set_bytes(4, 16, d.as_ptr().cast());
        enc.set_bytes(5, 8, bt.as_ptr().cast());
        enc.dispatch_thread_groups(
            MTLSize::new((m / 32) as u64, (self.n / 32) as u64, 1),
            MTLSize::new(128, 1, 1),
        );
        enc.end_encoding();
        cmd.commit();
        crate::gpu_recovery::completed(cmd)?;
        unsafe {
            std::ptr::copy_nonoverlapping(b.y.contents().cast(), y.as_mut_ptr(), y.len());
        }
        Ok(())
    }
}
impl Kernel for PointwiseKernel {
    fn compute(&mut self, ctx: &ComputeContext) -> ort::Result<()> {
        objc::rc::autoreleasepool(|| {
            let x = ctx.input(0)?.ok_or_else(|| ort::Error::new("missing x"))?;
            let (s, x) = x.try_extract_tensor::<f32>()?;
            if s.len() != 3 || s[1] != self.k as i64 {
                return Err(ort::Error::new("invalid pointwise input"));
            }
            let mut y = ctx
                .output(0, [s[0], self.n as i64, s[2]])?
                .ok_or_else(|| ort::Error::new("missing y"))?;
            self.run(
                x,
                s[0] as usize,
                s[2] as usize,
                y.try_extract_tensor_mut::<f32>()?.1,
            )
            .map_err(|e| ort::Error::new(e.to_string()))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "requires the qualified M5 Max Metal 4 device; include with offline shader checks"]
    fn fixed_pointwise_batch_padding_bias_and_buffer_reuse() {
        objc::rc::autoreleasepool(|| {
            let gpu = Gpu::new().unwrap();
            let n = 256;
            let k = 256;
            let weights: Vec<f32> = (0..n * k)
                .map(|i| ((i * 17 % 127) as f32 - 63.) / 512.)
                .collect();
            let half: Vec<_> = weights.iter().map(|x| f16::from_f32(*x)).collect();
            let bias: Vec<f32> = (0..n).map(|i| i as f32 / 1000.).collect();
            let mut kernel = PointwiseKernel {
                gpu: gpu.clone(),
                w: gpu.bytes(&half),
                bias: gpu.bytes(&bias),
                n,
                k,
                buffers: None,
            };
            for (batch, time) in [(1, 1), (2, 37), (2, 37), (1, 64)] {
                let x: Vec<f32> = (0..batch * k * time)
                    .map(|i| ((i * 13 % 113) as f32 - 56.) / 100.)
                    .collect();
                let mut y = vec![0.; batch * n * time];
                kernel.run(&x, batch, time, &mut y).unwrap();
                let mut err = 0.;
                let mut norm = 0.;
                for b in 0..batch {
                    for c in 0..n {
                        for t in 0..time {
                            let mut ref_y = bias[c] as f64;
                            for j in 0..k {
                                ref_y +=
                                    x[(b * k + j) * time + t] as f64 * weights[c * k + j] as f64;
                            }
                            let diff = y[(b * n + c) * time + t] as f64 - ref_y;
                            err += diff * diff;
                            norm += ref_y * ref_y;
                            assert!(
                                diff.abs() < 0.002,
                                "batch={b} channel={c} time={t} diff={diff}"
                            );
                        }
                    }
                }
                assert!((err / norm).sqrt() < 0.001);
            }
            assert!(
                kernel
                    .run(&[f32::INFINITY; 256], 1, 1, &mut [0.; 256])
                    .is_err()
            );
        });
    }
}

#[cfg(test)]
mod fault_tests {
    use super::*;
    #[test]
    fn gpu_fault_readback_is_rejected_immediately() {
        objc::rc::autoreleasepool(|| {
            let gpu = Gpu::new().unwrap();
            let w = vec![f16::from_f32(0.01); 256 * 256];
            let mut kernel = PointwiseKernel {
                gpu: gpu.clone(),
                w: gpu.bytes(&w),
                bias: gpu.bytes(&vec![0_f32; 256]),
                n: 256,
                k: 256,
                buffers: None,
            };
            let mut output = vec![42_f32; 256];
            crate::gpu_recovery::inject(1);
            let err = kernel
                .run(&vec![1_f32; 256], 1, 1, &mut output)
                .unwrap_err();
            assert!(
                err.downcast_ref::<crate::gpu_recovery::GpuError>()
                    .is_some()
            );
            assert!(output.iter().all(|x| *x == 42.));
            kernel.run(&vec![1_f32; 256], 1, 1, &mut output).unwrap();
            assert!(output.iter().all(|x| (*x - 2.56).abs() < 0.002));
        });
    }
}
