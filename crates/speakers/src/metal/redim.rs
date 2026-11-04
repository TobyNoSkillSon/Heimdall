use super::*;
use ort::{
    operator::{
        BoxedKernel, ComputeContext, KernelContext, Operator, OperatorDomain, OperatorInput,
        OperatorOutput,
    },
    session::{Session, builder::GraphOptimizationLevel},
    value::TensorElementType,
};

struct ConvOp {
    gpu: Arc<Mutex<Gpu>>,
}
struct BlockOp {
    gpu: Arc<Mutex<Gpu>>,
}
struct Block {
    convs: [Convolution; 4],
    scale: Buffer,
    bias: Buffer,
}
impl BlockOp {
    fn initialize(&self, ctx: &ComputeContext, shape: [usize; 4]) -> ort::Result<Block> {
        let values = |index| -> ort::Result<Vec<f32>> {
            Ok(ctx
                .input(index)?
                .ok_or_else(|| ort::Error::new("missing ConvBlock parameter"))?
                .try_extract_tensor::<f32>()?
                .1
                .to_vec())
        };
        let mut convs = Vec::new();
        for (wi, bi, dw) in [
            (1, None, true),
            (2, Some(3), false),
            (8, None, true),
            (9, Some(10), false),
        ] {
            let w = ctx
                .input(wi)?
                .ok_or_else(|| ort::Error::new("missing ConvBlock weights"))?;
            let (ws, w) = w.try_extract_tensor::<f32>()?;
            let c = shape[1];
            if ws.len() != 4
                || ws[0] != c as i64
                || ws[1] != if dw { 1 } else { c as i64 }
                || ws[2] != if dw { 3 } else { 1 }
                || ws[3] != if dw { 3 } else { 1 }
            {
                return Err(ort::Error::new("unsupported ConvBlock weight geometry"));
            }
            let bias = bi.map(values).transpose()?;
            let spec = ConvSpec {
                input: shape,
                output_channels: c,
                kernel: if dw { [3, 3] } else { [1, 1] },
                strides: [1, 1],
                pads: if dw { [1, 1] } else { [0, 0] },
                dilations: [1, 1],
                groups: if dw { c } else { 1 },
            };
            convs.push(
                Convolution::new(self.gpu.clone(), w, bias.as_deref(), spec)
                    .map_err(|e| ort::Error::new(e.to_string()))?,
            );
        }
        let gamma = values(4)?;
        let beta = values(5)?;
        let mean = values(6)?;
        let var = values(7)?;
        if [gamma.len(), beta.len(), mean.len(), var.len()]
            .iter()
            .any(|n| *n != shape[1])
        {
            return Err(ort::Error::new("unsupported ConvBlock batch norm"));
        }
        let scale: Vec<_> = gamma
            .iter()
            .zip(&var)
            .map(|(s, v)| s / (v + 1e-5).sqrt())
            .collect();
        let bias: Vec<_> = beta
            .iter()
            .zip(&mean)
            .zip(&scale)
            .map(|((b, m), s)| b - m * s)
            .collect();
        let g = self
            .gpu
            .lock()
            .map_err(|_| ort::Error::new("Metal lock poisoned"))?;
        Ok(Block {
            convs: convs.try_into().ok().unwrap(),
            scale: g.buffer(&scale),
            bias: g.buffer(&bias),
        })
    }
}
impl Block {
    #[allow(unexpected_cfgs)]
    fn run(&self, data: &[f32], shape: [usize; 4], out: &mut [f32]) -> Result<()> {
        objc2::rc::autoreleasepool(|_| {
            ensure!(
                data.len() == shape.iter().product::<usize>() && out.len() == data.len(),
                "invalid ConvBlock tensor length"
            );
            let mut g = self.convs[0]
                .gpu
                .lock()
                .map_err(|_| anyhow!("Metal lock poisoned"))?;
            let (x, xo) = g.borrow(data.as_ptr() as usize, std::mem::size_of_val(data));
            let (y, yo) = g.borrow(out.as_mut_ptr() as usize, std::mem::size_of_val(out));
            let a = g.allocate(std::mem::size_of_val(data));
            let b = g.allocate(std::mem::size_of_val(data));
            let scratch = g.allocate(
                shape[1].next_multiple_of(32)
                    * (shape[0] * shape[2] * shape[3]).next_multiple_of(32)
                    * 2,
            );
            let cmd = crate::gpu_recovery::command_buffer(&g.queue)?;
            let e = cmd.new_compute_command_encoder();
            self.convs[0].encode_same(&g, e, &x, xo, &a, 0, shape, &scratch)?;
            self.convs[1].encode_same(&g, e, &a, 0, &b, 0, shape, &scratch)?;
            e.set_compute_pipeline_state(&g.relu_bn);
            e.set_buffer(0, Some(&b), 0);
            e.set_buffer(1, Some(&self.scale), 0);
            e.set_buffer(2, Some(&self.bias), 0);
            bytes(
                e,
                3,
                &[
                    data.len() as u32,
                    shape[1] as u32,
                    (shape[2] * shape[3]) as u32,
                    0,
                ],
            );
            dispatch(e, data.len());
            e.memory_barrier_with_resources(&[&b]);
            self.convs[2].encode_same(&g, e, &b, 0, &a, 0, shape, &scratch)?;
            self.convs[3].encode_same(&g, e, &a, 0, &y, yo, shape, &scratch)?;
            e.end_encoding();
            cmd.commit();
            crate::gpu_recovery::completed(cmd)?;
            let start: f64 = unsafe { ::metal::objc::msg_send![cmd, GPUStartTime] };
            let end: f64 = unsafe { ::metal::objc::msg_send![cmd, GPUEndTime] };
            g.gpu_seconds += (end - start).max(0.);
            g.convolutions += 4;
            heimdall_metal_graph_time(end - start);
            Ok(())
        })
    }
}
impl Operator for BlockOp {
    type Kernel<'attr> = BoxedKernel<'attr>;
    fn name(&self) -> &str {
        "ConvBlock"
    }
    fn inputs(&self) -> impl IntoIterator<Item = OperatorInput> {
        (0..11).map(|_| OperatorInput::required(TensorElementType::Float32))
    }
    fn outputs(&self) -> impl IntoIterator<Item = OperatorOutput> {
        [OperatorOutput::required(TensorElementType::Float32)]
    }
    fn create_kernel<'attr>(&self, _: &KernelContext<'attr>) -> ort::Result<Self::Kernel<'attr>> {
        let op = BlockOp {
            gpu: self.gpu.clone(),
        };
        let mut block: Option<Block> = None;
        Ok(Box::new(move |ctx: &ComputeContext| {
            let x = ctx
                .input(0)?
                .ok_or_else(|| ort::Error::new("missing ConvBlock input"))?;
            let (shape, data) = x.try_extract_tensor::<f32>()?;
            if shape.len() != 4 {
                return Err(ort::Error::new("ConvBlock requires NCHW input"));
            }
            let xs = [
                shape[0] as usize,
                shape[1] as usize,
                shape[2] as usize,
                shape[3] as usize,
            ];
            if block.is_none() {
                block = Some(op.initialize(ctx, xs)?);
            }
            let mut y = ctx
                .output(0, shape.to_vec())?
                .ok_or_else(|| ort::Error::new("missing ConvBlock output"))?;
            block
                .as_ref()
                .unwrap()
                .run(data, xs, y.try_extract_tensor_mut::<f32>()?.1)
                .map_err(|e| ort::Error::new(e.to_string()))?;
            Ok(())
        }))
    }
}
impl Operator for ConvOp {
    type Kernel<'attr> = BoxedKernel<'attr>;
    fn name(&self) -> &str {
        "Conv"
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
    fn create_kernel<'attr>(&self, ctx: &KernelContext<'attr>) -> ort::Result<Self::Kernel<'attr>> {
        let gpu = self.gpu.clone();
        let stride = ctx.get::<Vec<i64>>("strides").unwrap_or_default();
        let pad = ctx.get::<Vec<i64>>("pads").unwrap_or_default();
        let dilation = ctx.get::<Vec<i64>>("dilations").unwrap_or_default();
        let group = ctx.get::<i64>("group").unwrap_or(1);
        let auto = ctx.get::<String>("auto_pad").unwrap_or_default();
        if !auto.is_empty() && auto != "NOTSET" && auto != "SAME_UPPER" {
            return Err(ort::Error::new("unsupported auto_pad"));
        }
        let mut convolution: Option<Convolution> = None;
        Ok(Box::new(move |ctx: &ComputeContext| {
            let x = ctx
                .input(0)?
                .ok_or_else(|| ort::Error::new("missing convolution input"))?;
            let (shape, data) = x.try_extract_tensor::<f32>()?;
            let rank = shape.len();
            if rank != 3 && rank != 4 {
                return Err(ort::Error::new("unsupported Conv rank"));
            }
            let xs = if rank == 3 {
                [shape[0] as usize, shape[1] as usize, 1, shape[2] as usize]
            } else {
                [
                    shape[0] as usize,
                    shape[1] as usize,
                    shape[2] as usize,
                    shape[3] as usize,
                ]
            };
            let w = ctx
                .input(1)?
                .ok_or_else(|| ort::Error::new("missing convolution weights"))?;
            let (ws, weights) = w.try_extract_tensor::<f32>()?;
            let kernel = if rank == 3 {
                [1, ws[2] as usize]
            } else {
                [ws[2] as usize, ws[3] as usize]
            };
            let pair = |v: &[i64], default| {
                if rank == 3 {
                    [1, v.first().copied().unwrap_or(default) as usize]
                } else {
                    [
                        v.first().copied().unwrap_or(default) as usize,
                        v.get(1).copied().unwrap_or(default) as usize,
                    ]
                }
            };
            let strides = pair(&stride, 1);
            let dilations = pair(&dilation, 1);
            let mut pads = if rank == 3 {
                [0, pad.first().copied().unwrap_or(0) as usize]
            } else {
                pair(&pad, 0)
            };
            if auto == "SAME_UPPER" {
                for i in 0..2 {
                    let total = ((xs[i + 2].div_ceil(strides[i]) - 1) * strides[i]
                        + dilations[i] * (kernel[i] - 1)
                        + 1)
                    .saturating_sub(xs[i + 2]);
                    if !total.is_multiple_of(2) {
                        return Err(ort::Error::new("asymmetric SAME padding unsupported"));
                    }
                    pads[i] = total / 2;
                }
            }
            let spec = ConvSpec {
                input: xs,
                output_channels: ws[0] as usize,
                kernel,
                strides,
                pads,
                dilations,
                groups: group as usize,
            };
            let map = |e: anyhow::Error| ort::Error::new(e.to_string());
            let out = spec.output().map_err(map)?;
            if convolution.is_none() {
                let bias = ctx.input(2)?;
                let values = bias
                    .as_ref()
                    .map(|b| b.try_extract_tensor::<f32>())
                    .transpose()?
                    .map(|(_, v)| v);
                convolution =
                    Some(Convolution::new(gpu.clone(), weights, values, spec).map_err(map)?);
            }
            let output_shape = if rank == 3 {
                vec![out[0] as i64, out[1] as i64, out[3] as i64]
            } else {
                out.map(|v| v as i64).to_vec()
            };
            let mut y = ctx
                .output(0, output_shape)?
                .ok_or_else(|| ort::Error::new("missing convolution output"))?;
            convolution
                .as_ref()
                .unwrap()
                .run_into(data, xs, y.try_extract_tensor_mut::<f32>()?.1)
                .map_err(map)?;
            Ok(())
        }))
    }
}
pub(crate) fn session(path: &std::path::Path) -> Result<Session> {
    let (bytes, count) = super::proto::rewrite(&std::fs::read(path)?)?;
    let gpu = Arc::new(Mutex::new(Gpu::new()?));
    eprintln!("speakers ReDimNet: custom FP16 Metal ({count} convolutions), FP32 accumulation");
    let map = |e: ort::Error<ort::session::builder::SessionBuilder>| anyhow!(e.to_string());
    Ok(Session::builder()?
        .with_intra_threads(1)
        .map_err(map)?
        .with_inter_threads(1)
        .map_err(map)?
        .with_intra_op_spinning(false)
        .map_err(map)?
        .with_inter_op_spinning(false)
        .map_err(map)?
        .with_parallel_execution(false)
        .map_err(map)?
        .with_no_environment_execution_providers()
        .map_err(map)?
        .with_optimization_level(GraphOptimizationLevel::Level3)
        .map_err(map)?
        .with_operators(
            OperatorDomain::new("dev.heimdall.fp16")?
                .add(BlockOp { gpu: gpu.clone() })?
                .add(ConvOp { gpu })?,
        )
        .map_err(map)?
        .commit_from_memory(&bytes)?)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn four_convolution_block_matches_captured_ort() {
        let row: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/fixtures/metal-block.json")).unwrap();
        let shape: [usize; 4] = serde_json::from_value(row["shape"].clone()).unwrap();
        let c = shape[1];
        let gpu = Arc::new(Mutex::new(Gpu::new().unwrap()));
        let mut convs = Vec::new();
        for (i, layer) in row["convolutions"].as_array().unwrap().iter().enumerate() {
            let w: Vec<f32> = serde_json::from_value(layer["weights"].clone()).unwrap();
            let b: Option<Vec<f32>> = serde_json::from_value(layer["bias"].clone()).unwrap();
            let dw = i % 2 == 0;
            let spec = ConvSpec {
                input: shape,
                output_channels: c,
                kernel: if dw { [3, 3] } else { [1, 1] },
                strides: [1, 1],
                pads: if dw { [1, 1] } else { [0, 0] },
                dilations: [1, 1],
                groups: if dw { c } else { 1 },
            };
            convs.push(Convolution::new(gpu.clone(), &w, b.as_deref(), spec).unwrap());
        }
        let v = |name: &str| -> Vec<f32> { serde_json::from_value(row[name].clone()).unwrap() };
        let scale: Vec<_> = v("gamma")
            .iter()
            .zip(v("variance"))
            .map(|(s, n)| s / (n + 1e-5).sqrt())
            .collect();
        let bias: Vec<_> = v("beta")
            .iter()
            .zip(v("mean"))
            .zip(&scale)
            .map(|((b, m), s)| b - m * s)
            .collect();
        let g = gpu.lock().unwrap();
        let scale = g.buffer(&scale);
        let bias = g.buffer(&bias);
        drop(g);
        let block = Block {
            convs: convs.try_into().ok().unwrap(),
            scale,
            bias,
        };
        let input = v("input");
        let expected = v("output");
        let mut actual = vec![0.; input.len()];
        block.run(&input, shape, &mut actual).unwrap();
        let max = actual
            .iter()
            .zip(&expected)
            .map(|(a, b)| (a - b).abs())
            .fold(0_f32, f32::max);
        assert!(actual.iter().all(|v| v.is_finite()));
        assert!(max < 0.0003, "ConvBlock max delta {max}");
        assert_eq!(gpu.lock().unwrap().stats().0, 4);
    }
}
