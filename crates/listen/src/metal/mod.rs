//! Owned FP16 Metal executor for the pinned Ultra graph. No Vella runtime dependency.
//! Constants retain the Core ML asset's affine dequantization and FP16 boundaries.
use anyhow::{anyhow, bail, ensure, Context, Result};
use half::f16;
use objc2::{rc::Retained, runtime::ProtocolObject};
use objc2_foundation::{NSRange, NSString};
use objc2_metal::*;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{cell::RefCell, collections::HashMap, ffi::c_void, path::Path, ptr::NonNull, rc::Rc};
type Buffer = Retained<ProtocolObject<dyn MTLBuffer>>;
type Pipeline = Retained<ProtocolObject<dyn MTLComputePipelineState>>;
#[derive(Deserialize)]
struct Literal {
    dtype: String,
    shape: Vec<usize>,
    values: Option<Vec<serde_json::Value>>,
    offset: Option<usize>,
    dequant: Option<HashMap<String, Literal>>,
    axis: Option<usize>,
    #[serde(default)]
    raw: bool,
}
#[derive(Deserialize)]
struct Out {
    name: String,
    shape: Vec<usize>,
    #[serde(rename = "dtype")]
    _dtype: String,
}
#[derive(Deserialize)]
struct Node {
    op: String,
    args: HashMap<String, String>,
    outputs: Vec<Out>,
}
#[derive(Deserialize)]
struct Plan {
    mil_sha256: String,
    #[serde(default)]
    weights_sha256: Option<String>,
    shapes: HashMap<String, Vec<usize>>,
    constants: HashMap<String, Literal>,
    nodes: Vec<Node>,
    outputs: Vec<String>,
}
#[derive(Clone)]
struct Slot {
    buffer: Buffer,
    id: usize,
    zero: bool,
}
struct Step {
    kernel: String,
    tensor: String,
    pipeline: Pipeline,
    buffers: Vec<(usize, Buffer)>,
    params: Vec<i32>,
    param_slot: usize,
    grid: MTLSize,
    group: MTLSize,
}
fn size(x: usize, y: usize, z: usize) -> MTLSize {
    MTLSize {
        width: x,
        height: y,
        depth: z,
    }
}
fn product(s: &[usize]) -> usize {
    s.iter().product()
}
fn strides(s: &[usize]) -> Vec<usize> {
    let mut v = vec![1; s.len()];
    for i in (0..s.len().saturating_sub(1)).rev() {
        v[i] = v[i + 1] * s[i + 1];
    }
    v
}
fn dims6(s: &[usize]) -> [i32; 6] {
    let mut d = [1; 6];
    for (a, b) in d[6 - s.len()..].iter_mut().zip(s) {
        *a = *b as i32;
    }
    d
}
fn broadcast(s: &[usize]) -> [i32; 6] {
    let mut v = [0; 6];
    for (i, n) in s.iter().enumerate() {
        if *n > 1 {
            v[6 - s.len() + i] = strides(s)[i] as i32;
        }
    }
    v
}
fn numeric(v: &serde_json::Value) -> Result<f32> {
    Ok(if let Some(b) = v.as_bool() {
        if b {
            1.0
        } else {
            0.0
        }
    } else {
        v.as_f64().context("numeric literal")? as f32
    })
}
fn decode(l: &Literal, blob: &[u8]) -> Result<Vec<f32>> {
    let count = product(&l.shape);
    if let Some(d) = &l.dequant {
        let q = decode(&d["quantized_data"], blob)?;
        let sc = decode(&d["scale"], blob)?;
        let z = decode(&d["zero_point"], blob)?;
        let axis = l.axis.context("affine axis")?;
        ensure!(axis < l.shape.len(), "affine axis bounds");
        let stride = product(&l.shape[axis + 1..]);
        ensure!(
            sc.len() == l.shape[axis] && z.len() == sc.len(),
            "affine schema"
        );
        return Ok(q
            .iter()
            .enumerate()
            .map(|(i, q)| {
                let j = (i / stride) % sc.len();
                f16::from_f32((*q - z[j]) * sc[j]).to_f32()
            })
            .collect());
    }
    if let Some(v) = &l.values {
        ensure!(v.len() == count, "literal count");
        return v.iter().map(numeric).collect();
    }
    let off = l.offset.context("literal storage")?;
    let start = if l.raw {
        off
    } else {
        let header = blob.get(off..off + 64).context("blob header range")?;
        ensure!(
            u32::from_le_bytes(header[0..4].try_into()?) == 0xdeadbeef,
            "blob sentinel"
        );
        u64::from_le_bytes(header[16..24].try_into()?) as usize
    };
    let width = match l.dtype.as_str() {
        "fp16" | "int16" | "uint16" => 2,
        "fp32" | "int32" => 4,
        "int8" | "uint8" | "bool" => 1,
        _ => bail!("Unsupported blob dtype {}", l.dtype),
    };
    let bytes = blob
        .get(start..start + count * width)
        .context("blob data bounds")?;
    bytes
        .chunks_exact(width)
        .map(|b| {
            Ok(match l.dtype.as_str() {
                "fp16" => f16::from_bits(u16::from_le_bytes(b.try_into()?)).to_f32(),
                "fp32" => f32::from_le_bytes(b.try_into()?),
                "int32" => i32::from_le_bytes(b.try_into()?) as f32,
                "int16" => i16::from_le_bytes(b.try_into()?) as f32,
                "uint16" => u16::from_le_bytes(b.try_into()?) as f32,
                "int8" => (b[0] as i8) as f32,
                _ => b[0] as f32,
            })
        })
        .collect()
}
struct Builder {
    device: Retained<ProtocolObject<dyn MTLDevice>>,
    pipelines: HashMap<String, Pipeline>,
    slots: HashMap<String, Slot>,
    spare: Vec<Slot>,
    next: usize,
    steps: Vec<Step>,
    plan: Plan,
    blob: Vec<u8>,
    constant_bank: Rc<RefCell<HashMap<String, Slot>>>,
}
impl Builder {
    fn alloc(&mut self, n: usize) -> Result<Slot> {
        let bytes = (n * 2).max(2);
        if let Some((i, _)) = self
            .spare
            .iter()
            .enumerate()
            .filter(|(_, b)| b.buffer.length() >= bytes)
            .min_by_key(|(_, b)| b.buffer.length())
        {
            return Ok(self.spare.swap_remove(i));
        }
        let b = self
            .device
            .newBufferWithLength_options(bytes, MTLResourceOptions::StorageModeShared)
            .ok_or_else(|| crate::gpu_recovery::record("Metal buffer allocation unavailable"))?;
        let s = Slot {
            buffer: b,
            id: self.next,
            zero: false,
        };
        self.next += 1;
        Ok(s)
    }
    fn get(&mut self, name: &str) -> Result<Slot> {
        if let Some(s) = self.slots.get(name) {
            return Ok(s.clone());
        }
        if let Some(mut s) = self.constant_bank.borrow().get(name).cloned() {
            // Slot IDs belong to this activation plan; buffers belong to the
            // single encoder weight bank shared by every ready-batch size.
            s.id = self.next;
            self.next += 1;
            self.slots.insert(name.into(), s.clone());
            return Ok(s);
        }
        let l = self
            .plan
            .constants
            .get(name)
            .with_context(|| format!("Unknown graph tensor {name}"))?;
        let raw = decode(l, &self.blob)?;
        let zero = raw.iter().all(|v| *v == 0.);
        let a: Vec<u16> = raw
            .into_iter()
            .map(|x| f16::from_f32(x).to_bits())
            .collect();
        let buffer = self
            .device
            .newBufferWithLength_options(
                (a.len() * 2).max(2),
                MTLResourceOptions::StorageModeShared,
            )
            .ok_or_else(|| crate::gpu_recovery::record("constant buffer allocation unavailable"))?;
        let s = Slot {
            buffer,
            id: self.next,
            zero,
        };
        self.next += 1;
        unsafe {
            std::ptr::copy_nonoverlapping(
                a.as_ptr(),
                s.buffer.contents().as_ptr().cast::<u16>(),
                a.len(),
            );
        }
        self.slots.insert(name.into(), s.clone());
        self.constant_bank
            .borrow_mut()
            .insert(name.into(), s.clone());
        Ok(s)
    }
    fn value(&self, n: &Node, key: &str) -> Result<Vec<f32>> {
        let l = &self.plan.constants[n
            .args
            .get(key)
            .with_context(|| format!("{} lacks {key}", n.op))?];
        decode(l, &self.blob)
    }
    fn arg(&mut self, n: &Node, k: &str) -> Result<Slot> {
        self.get(
            n.args
                .get(k)
                .with_context(|| format!("{} lacks {k}", n.op))?,
        )
    }
    fn shape(&self, n: &Node, k: &str) -> Vec<usize> {
        self.plan.shapes[&n.args[k]].clone()
    }
    fn emit(
        &mut self,
        name: &str,
        buffers: Vec<(usize, Buffer)>,
        params: Vec<i32>,
        param_slot: usize,
        grid: MTLSize,
        group: MTLSize,
    ) -> Result<()> {
        self.steps.push(Step {
            kernel: name.into(),
            tensor: String::new(),
            pipeline: self
                .pipelines
                .get(name)
                .with_context(|| format!("Unknown shader {name}"))?
                .clone(),
            buffers,
            params,
            param_slot,
            grid,
            group,
        });
        Ok(())
    }
    fn element(
        &mut self,
        op: i32,
        inputs: &[(&Slot, Vec<usize>)],
        out: &Slot,
        shape: &[usize],
    ) -> Result<()> {
        let mut p = vec![0; 33];
        p[0] = product(shape) as i32;
        p[1] = op;
        p[2..8].copy_from_slice(&dims6(shape));
        let mut bs = vec![(3, out.buffer.clone())];
        for (i, (s, sh)) in inputs.iter().enumerate() {
            p[8 + i * 6..14 + i * 6].copy_from_slice(&broadcast(sh));
            let os = dims6(shape);
            let ds = dims6(sh);
            let mut contiguous = true;
            let mut started = false;
            for d in 0..6 {
                if ds[d] > 1 {
                    started = true;
                }
                if started && ds[d] != os[d] {
                    contiguous = false;
                }
            }
            if contiguous {
                p[26 + i] = product(sh) as i32;
            }
            bs.push((i, s.buffer.clone()));
        }
        for i in inputs.len()..3 {
            bs.push((i, inputs[0].0.buffer.clone()));
        }
        self.emit(
            "element",
            bs,
            p,
            4,
            size(product(shape).div_ceil(256), 1, 1),
            size(256, 1, 1),
        )
    }
    #[allow(clippy::too_many_arguments)] // Tensor layout ABI: shape, strides, offsets and tiling.
    fn layout(
        &mut self,
        x: &Slot,
        out: &Slot,
        osh: &[usize],
        ish: &[usize],
        ss: &[usize],
        begin: &[i32],
        tile: bool,
    ) -> Result<()> {
        let mut p = vec![0; 33];
        p[0] = product(osh) as i32;
        p[1] = if tile { 2 } else { 0 };
        p[2..8].copy_from_slice(&dims6(osh));
        p[8..14].copy_from_slice(&dims6(ish));
        for (i, s) in ss.iter().enumerate() {
            p[14 + 6 - ss.len() + i] = *s as i32;
        }
        for (i, s) in begin.iter().enumerate() {
            p[20 + 6 - begin.len() + i] = *s;
        }
        self.emit(
            "layout",
            vec![(0, x.buffer.clone()), (3, out.buffer.clone())],
            p,
            4,
            size(product(osh).div_ceil(256), 1, 1),
            size(256, 1, 1),
        )
    }
    #[allow(clippy::too_many_arguments)] // Explicit GEMM dimensions and batch planes.
    fn gemm(
        &mut self,
        x: &Slot,
        w: &Slot,
        out: &Slot,
        m: usize,
        k: usize,
        n: usize,
        batch: usize,
    ) -> Result<()> {
        self.gemm_epilogue(x, w, out, m, k, n, batch, None, 0, 0)
    }
    #[allow(clippy::too_many_arguments)]
    fn gemm_epilogue(
        &mut self,
        x: &Slot,
        w: &Slot,
        out: &Slot,
        m: usize,
        k: usize,
        n: usize,
        batch: usize,
        bias: Option<&Slot>,
        flags: i32,
        spatial: usize,
    ) -> Result<()> {
        // Vk-formats27: FP16 operands, float accumulation, identical reduction geometry.
        ensure!(
            k.is_multiple_of(32),
            "tensor GEMM K {k} must be padded to 32"
        );
        let big =
            m >= 64 && !(m <= 256 && std::env::var("HEIMDALL_METAL_GEMM").as_deref() == Ok("tile"));
        let name = if big { "biga_f16" } else { "tile4_f16" };
        let mut bs: Vec<_> = (0..7)
            .map(|i| {
                (
                    i,
                    if i == 0 {
                        x.buffer.clone()
                    } else if i == 1 {
                        w.buffer.clone()
                    } else if i == 6 {
                        out.buffer.clone()
                    } else {
                        x.buffer.clone()
                    },
                )
            })
            .collect();
        bs.push((8, bias.unwrap_or(x).buffer.clone()));
        self.emit(
            name,
            bs,
            vec![
                m as i32,
                k as i32,
                n as i32,
                flags | if bias.is_some() { 8 } else { 0 },
                spatial as i32,
                batch as i32,
            ],
            7,
            if big {
                size(n.div_ceil(128), m.div_ceil(64), batch)
            } else {
                size(m.div_ceil(32), n.div_ceil(32), batch)
            },
            size(if big { 256 } else { 128 }, 1, 1),
        )
    }
    fn linear(
        &mut self,
        x: &Slot,
        sh: &[usize],
        w: &Slot,
        bias: Option<&Slot>,
        out: &Slot,
        n: usize,
    ) -> Result<()> {
        let bias = bias.filter(|b| !b.zero);
        let k = *sh.last().context("linear rank")?;
        let m = product(sh) / k;
        if m == 1 {
            return self.emit(
                "matvec",
                vec![
                    (0, x.buffer.clone()),
                    (1, w.buffer.clone()),
                    (2, bias.unwrap_or(x).buffer.clone()),
                    (3, out.buffer.clone()),
                ],
                vec![1, k as i32, n as i32, i32::from(bias.is_some())],
                4,
                size(n, 1, 1),
                size(32, 1, 1),
            );
        }
        if let Some(b) = bias {
            self.gemm_epilogue(x, w, out, m, k, n, 1, Some(b), 0, 0)?;
        } else {
            self.gemm(x, w, out, m, k, n, 1)?;
        }
        Ok(())
    }
    fn node(&mut self, n: &Node) -> Result<()> {
        let o = &n.outputs[0];
        let sh = &o.shape;
        ensure!(sh.len() <= 6, "rank limit");
        if ["reshape", "expand_dims", "squeeze", "identity", "cast"].contains(&n.op.as_str()) {
            let x = self.arg(n, "x")?;
            ensure!(
                product(&self.shape(n, "x")) == product(sh),
                "alias shape count"
            );
            self.slots.insert(o.name.clone(), x);
            return Ok(());
        }
        let y = self.alloc(product(sh))?;
        match n.op.as_str() {
            "add" | "mul" | "less" | "logical_and" => {
                let a = self.arg(n, "x")?;
                let b = self.arg(n, "y")?;
                let op = match n.op.as_str() {
                    "add" => 1,
                    "mul" => 2,
                    "less" => 7,
                    _ => 8,
                };
                self.element(
                    op,
                    &[(&a, self.shape(n, "x")), (&b, self.shape(n, "y"))],
                    &y,
                    sh,
                )?;
            }
            "floor" | "relu" | "silu" | "sigmoid" | "logical_not" | "tanh" | "erf" => {
                let a = self.arg(n, "x")?;
                let op = match n.op.as_str() {
                    "floor" => 3,
                    "relu" => 4,
                    "silu" => 5,
                    "sigmoid" => 6,
                    "logical_not" => 9,
                    "tanh" => 11,
                    _ => 12,
                };
                self.element(op, &[(&a, self.shape(n, "x"))], &y, sh)?;
            }
            "select" => {
                let a = self.arg(n, "a")?;
                let b = self.arg(n, "b")?;
                let c = self.arg(n, "cond")?;
                self.element(
                    10,
                    &[
                        (&a, self.shape(n, "a")),
                        (&b, self.shape(n, "b")),
                        (&c, self.shape(n, "cond")),
                    ],
                    &y,
                    sh,
                )?;
            }
            "transpose" => {
                let x = self.arg(n, "x")?;
                let ish = self.shape(n, "x");
                let ss = strides(&ish);
                let perm = self.value(n, "perm")?;
                let perm: Vec<usize> = perm
                    .iter()
                    .map(|i| (*i as isize).rem_euclid(ish.len() as isize) as usize)
                    .collect();
                let stride: Vec<usize> = perm.iter().map(|i| ss[*i]).collect();
                let dims: Vec<usize> = perm.iter().map(|i| ish[*i]).collect();
                self.layout(&x, &y, sh, &dims, &stride, &vec![0; sh.len()], false)?;
            }
            "pad" | "slice_by_index" | "tile" => {
                let x = self.arg(n, "x")?;
                let ish = self.shape(n, "x");
                let mut begin = vec![0; ish.len()];
                if n.op == "pad" {
                    let v = self.value(n, "pad")?;
                    ensure!(self.value(n, "constant_val")? == [0.0], "only zero padding");
                    for i in 0..ish.len() {
                        begin[i] = -(v[2 * i] as i32);
                    }
                }
                if n.op == "slice_by_index" {
                    begin = self.value(n, "begin")?.iter().map(|x| *x as i32).collect();
                }
                self.layout(&x, &y, sh, &ish, &strides(&ish), &begin, n.op == "tile")?;
            }
            "split" => {
                let x = self.arg(n, "x")?;
                let ish = self.shape(n, "x");
                let axis = self.value(n, "axis")?[0] as usize;
                for (i, o) in n.outputs.iter().enumerate() {
                    let out = if i == 0 {
                        y.clone()
                    } else {
                        self.alloc(product(&o.shape))?
                    };
                    let mut begin = vec![0; ish.len()];
                    begin[axis] = (i * o.shape[axis]) as i32;
                    self.layout(&x, &out, &o.shape, &ish, &strides(&ish), &begin, false)?;
                    self.slots.insert(o.name.clone(), out);
                }
                return Ok(());
            }
            "layer_norm" => {
                ensure!(self.value(n, "axes")? == [-1.0], "layernorm last axis");
                let x = self.arg(n, "x")?;
                let g = self.arg(n, "gamma")?;
                let b = self.arg(n, "beta")?;
                let c = *sh.last().context("layernorm rank")?;
                let eps = self.value(n, "epsilon")?[0];
                self.emit(
                    "norm",
                    vec![
                        (0, x.buffer),
                        (1, g.buffer),
                        (2, b.buffer),
                        (3, y.buffer.clone()),
                    ],
                    vec![product(sh) as i32, c as i32, eps.to_bits() as i32],
                    4,
                    size(product(sh) / c, 1, 1),
                    size(256, 1, 1),
                )?;
            }
            "softmax" => {
                let axis = self.value(n, "axis")?[0] as isize;
                ensure!(
                    axis == -1 || (axis == 1 && sh == &[1, 400, 1]),
                    "unsupported softmax axis"
                );
                let x = self.arg(n, "x")?;
                let c = if axis == 1 {
                    400
                } else {
                    *sh.last().context("softmax rank")?
                };
                self.emit(
                    "softmax_rows",
                    vec![(0, x.buffer), (3, y.buffer.clone())],
                    vec![product(sh) as i32, c as i32],
                    4,
                    size(product(sh) / c, 1, 1),
                    size(256, 1, 1),
                )?;
            }
            "linear" => {
                let x = self.arg(n, "x")?;
                let w = self.arg(n, "weight")?;
                let b = n
                    .args
                    .get("bias")
                    .map(|s| s.to_owned())
                    .map(|s| self.get(&s))
                    .transpose()?;
                self.linear(
                    &x,
                    &self.shape(n, "x"),
                    &w,
                    b.as_ref(),
                    &y,
                    *sh.last().context("linear rank")?,
                )?;
            }
            "gather" => {
                ensure!(self.value(n, "axis")? == [0.0], "embedding axis");
                let x = self.arg(n, "x")?;
                let t = self.arg(n, "indices")?;
                self.emit(
                    "embedding",
                    vec![(0, x.buffer), (1, t.buffer), (3, y.buffer.clone())],
                    vec![product(sh) as i32],
                    4,
                    size(product(sh).div_ceil(256), 1, 1),
                    size(256, 1, 1),
                )?;
            }
            "stack" => {
                ensure!(self.value(n, "axis")? == [0.0], "decoder stack axis");
                let names: Vec<_> = n.args["values"]
                    .trim_matches(['(', ')'])
                    .split(',')
                    .map(str::trim)
                    .collect();
                ensure!(names.len() == 2, "stack requires two states");
                let a = self.get(names[0])?;
                let b = self.get(names[1])?;
                self.emit(
                    "stack_two",
                    vec![(0, a.buffer), (1, b.buffer), (3, y.buffer.clone())],
                    vec![product(sh) as i32, (product(sh) / 2) as i32],
                    4,
                    size(product(sh).div_ceil(256), 1, 1),
                    size(256, 1, 1),
                )?;
            }
            "lstm" => {
                let x = self.arg(n, "x")?;
                let h = self.arg(n, "initial_h")?;
                let c = self.arg(n, "initial_c")?;
                let wi = self.arg(n, "weight_ih")?;
                let wh = self.arg(n, "weight_hh")?;
                let bias = self.arg(n, "bias")?;
                let width = 640;
                ensure!(
                    product(sh) == width && n.outputs.len() == 3,
                    "Ultra single-step LSTM schema"
                );
                let gates = self.alloc(width * 4 * 2)?;
                let cy = self.alloc(width)?;
                self.emit(
                    "lstm_project",
                    vec![
                        (0, x.buffer),
                        (1, h.buffer),
                        (2, wi.buffer),
                        (3, wh.buffer),
                        (4, bias.buffer),
                        (5, gates.buffer.clone()),
                    ],
                    vec![width as i32],
                    6,
                    size(width * 4, 1, 1),
                    size(32, 1, 1),
                )?;
                self.emit(
                    "lstm_cell",
                    vec![
                        (0, gates.buffer.clone()),
                        (1, c.buffer),
                        (2, y.buffer.clone()),
                        (3, cy.buffer.clone()),
                    ],
                    vec![width as i32],
                    4,
                    size(width.div_ceil(256), 1, 1),
                    size(256, 1, 1),
                )?;
                self.slots.insert(n.outputs[0].name.clone(), y.clone());
                self.slots.insert(n.outputs[1].name.clone(), y);
                self.slots.insert(n.outputs[2].name.clone(), cy);
                self.spare.push(gates);
                return Ok(());
            }
            "conv" | "conv_relu" | "conv_silu" => self.conv(n, &y)?,
            "reduce_sum" => {
                let x = self.arg(n, "x")?;
                let ish = self.shape(n, "x");
                ensure!(
                    self.value(n, "axes")? == [1.0] && ish.len() == 3 && ish[0] == 1,
                    "sum time schema"
                );
                self.emit(
                    "sum_time",
                    vec![(0, x.buffer), (3, y.buffer.clone())],
                    vec![product(sh) as i32, ish[1] as i32],
                    4,
                    size(product(sh).div_ceil(256), 1, 1),
                    size(256, 1, 1),
                )?;
            }
            "matmul" => {
                ensure!(
                    self.value(n, "transpose_x")? == [0.0]
                        && self.value(n, "transpose_y")? == [0.0],
                    "matmul transpose flags"
                );
                let x = self.arg(n, "x")?;
                let w = self.arg(n, "y")?;
                let xs = self.shape(n, "x");
                let ws = self.shape(n, "y");
                let rank = xs.len();
                let m = xs[rank - 2];
                let k = xs[rank - 1];
                let nn = ws[rank - 1];
                let batch = product(&xs[..rank - 2]);
                let weight_batch = product(&ws[..rank - 2]);
                ensure!(
                    batch.is_multiple_of(weight_batch),
                    "matmul batch broadcast schema"
                );
                let kp = k.div_ceil(32) * 32;
                let xp = if kp != k {
                    let xx = self.alloc(batch * m * kp)?;
                    self.layout(
                        &x,
                        &xx,
                        &[batch, m, kp],
                        &[batch, m, k],
                        &[m * k, k, 1],
                        &[0; 3],
                        false,
                    )?;
                    xx
                } else {
                    x
                };
                let wp = self.alloc(weight_batch * nn * kp)?;
                self.layout(
                    &w,
                    &wp,
                    &[weight_batch, nn, kp],
                    &[weight_batch, nn, k],
                    &[k * nn, 1, nn],
                    &[0; 3],
                    false,
                )?;
                self.gemm(&xp, &wp, &y, m, kp, nn, batch)?;
                self.steps.last_mut().unwrap().params[5] = weight_batch as i32;
                self.spare.push(wp);
                if kp != k {
                    self.spare.push(xp);
                }
            }
            _ => bail!("Unsupported Metal graph op {}", n.op),
        }
        self.slots.insert(o.name.clone(), y);
        Ok(())
    }
    fn conv(&mut self, n: &Node, y: &Slot) -> Result<()> {
        let x = self.arg(n, "x")?;
        let w = self.arg(n, "weight")?;
        let ish = self.shape(n, "x");
        let wsh = self.shape(n, "weight");
        let osh = &n.outputs[0].shape;
        let groups = self.value(n, "groups")?[0] as usize;
        let co = osh[1];
        let b = if n.args.contains_key("bias") {
            self.arg(n, "bias")?
        } else {
            let name = format!("__zero_{co}");
            if !self.plan.constants.contains_key(&name) {
                self.plan.constants.insert(
                    name.clone(),
                    Literal {
                        dtype: "fp16".into(),
                        shape: vec![co],
                        values: Some(vec![serde_json::json!(0); co]),
                        offset: None,
                        dequant: None,
                        axis: None,
                        raw: false,
                    },
                );
            }
            self.get(&name)?
        };
        let rank = ish.len();
        ensure!(rank == 3 || rank == 4, "convolution rank");
        ensure!(
            self.value(n, "dilations")?.iter().all(|v| *v == 1.0),
            "convolution dilation"
        );
        let ci = ish[1];
        let co = osh[1];
        let hi = if rank == 4 { ish[2] } else { 1 };
        let wi = ish[rank - 1];
        let ho = if rank == 4 { osh[2] } else { 1 };
        let wo = osh[rank - 1];
        let kh = if rank == 4 { wsh[2] } else { 1 };
        let kw = wsh[rank - 1];
        if groups == 1 && kh == 1 && kw == 1 {
            let spatial = hi * wi;
            let xp = self.alloc(ish[0] * spatial * ci)?;
            self.layout(
                &x,
                &xp,
                &[ish[0], spatial, ci],
                &[ish[0], spatial, ci],
                &[ci * spatial, 1, spatial],
                &[0; 3],
                false,
            )?;
            self.gemm_epilogue(
                &xp,
                &w,
                y,
                ish[0] * spatial,
                ci,
                co,
                1,
                (!b.zero).then_some(&b),
                32 | if n.op == "conv_relu" { 16 } else { 0 },
                spatial,
            )?;
            self.spare.push(xp);
            return Ok(());
        }
        let stride = self.value(n, "strides")?;
        let pad = self.value(n, "pad")?;
        let sh = if rank == 4 { stride[0] as i32 } else { 1 };
        let sw = stride[rank - 3] as i32;
        let ph = if rank == 4 { pad[0] as i32 } else { 0 };
        let pw = pad[(rank - 3) * 2] as i32;
        if groups == 1 {
            let k = ci * kh * kw;
            let kp = k.div_ceil(32) * 32;
            let rows = ish[0] * ho * wo;
            let xp = self.alloc(rows * kp)?;
            self.emit(
                "im2col",
                vec![(0, x.buffer.clone()), (3, xp.buffer.clone())],
                vec![
                    (rows * kp) as i32,
                    kp as i32,
                    ci as i32,
                    hi as i32,
                    wi as i32,
                    ho as i32,
                    wo as i32,
                    kh as i32,
                    kw as i32,
                    sh,
                    sw,
                    ph,
                    pw,
                ],
                4,
                size((rows * kp).div_ceil(256), 1, 1),
                size(256, 1, 1),
            )?;
            let wp = if kp == k {
                w.clone()
            } else {
                let wp = self.alloc(co * kp)?;
                self.layout(&w, &wp, &[co, kp], &[co, k], &[k, 1], &[0; 2], false)?;
                wp
            };
            self.gemm_epilogue(
                &xp,
                &wp,
                y,
                rows,
                kp,
                co,
                1,
                (!b.zero).then_some(&b),
                32 | if n.op == "conv_relu" { 16 } else { 0 },
                ho * wo,
            )?;
            self.spare.push(xp);
            if kp != k {
                self.spare.push(wp);
            }
            return Ok(());
        }
        self.emit(
            "convolution",
            vec![
                (0, x.buffer),
                (1, w.buffer),
                (2, b.buffer),
                (3, y.buffer.clone()),
            ],
            vec![
                product(osh) as i32,
                ci as i32,
                hi as i32,
                wi as i32,
                co as i32,
                ho as i32,
                wo as i32,
                kh as i32,
                kw as i32,
                sh,
                sw,
                ph,
                pw,
                groups as i32,
                if n.op == "conv_relu" {
                    1
                } else if n.op == "conv_silu" {
                    2
                } else {
                    0
                },
            ],
            4,
            size(product(osh).div_ceil(256), 1, 1),
            size(256, 1, 1),
        )
    }
}

pub struct MetalEncoder {
    queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    steps: Vec<Step>,
    inputs: HashMap<String, Slot>,
    outputs: HashMap<String, Slot>,
    _buffers: Vec<Buffer>,
    shapes: HashMap<String, Vec<usize>>,
    pipelines: HashMap<String, Pipeline>,
    constant_bank: Rc<RefCell<HashMap<String, Slot>>>,
    batch_models: HashMap<usize, Box<MetalEncoder>>,
}
impl MetalEncoder {
    pub fn load(path: &Path) -> Result<Self> {
        let mut plan: Plan = serde_json::from_str(include_str!("plans/Encoder.json"))?;
        if let Ok(tensor) = std::env::var("HEIMDALL_METAL_CAPTURE_TENSOR") {
            ensure!(plan.shapes.contains_key(&tensor), "Unknown capture tensor");
            plan.outputs.push(tensor);
        }
        let mil = std::fs::read(path.join("Encoder.mlmodelc/model.mil"))?;
        ensure!(
            format!("{:x}", Sha256::digest(&mil)) == plan.mil_sha256,
            "Pinned Metal encoder graph does not match weights"
        );
        let blob = std::fs::read(path.join("Encoder.mlmodelc/weights/weight.bin"))?;
        Self::build(plan, blob, &["mel", "mel_length"])
    }
    fn build(plan: Plan, blob: Vec<u8>, input_names: &[&str]) -> Result<Self> {
        Self::build_shared(plan, blob, input_names, None)
    }
    fn build_shared(
        mut plan: Plan,
        blob: Vec<u8>,
        input_names: &[&str],
        shared: Option<&Self>,
    ) -> Result<Self> {
        // Remove only materializations whose exact FP16 boundaries the epilogue
        // retains. Diagnostic captures keep the requested intermediate alive.
        let mut uses = HashMap::new();
        for n in &plan.nodes {
            for a in n.args.values() {
                *uses.entry(a.clone()).or_insert(0usize) += 1;
            }
        }
        let mut nodes = std::mem::take(&mut plan.nodes).into_iter().peekable();
        while let Some(mut n) = nodes.next() {
            if n.op == "conv"
                && uses.get(&n.outputs[0].name) == Some(&1)
                && !plan.outputs.contains(&n.outputs[0].name)
            {
                if let Some(next) = nodes.peek() {
                    let groups = decode(&plan.constants[&n.args["groups"]], &blob)?[0];
                    if next.args.get("x") == Some(&n.outputs[0].name)
                        && (next.op == "relu" || (next.op == "silu" && groups > 1.))
                    {
                        let next = nodes.next().unwrap();
                        n.op = format!("conv_{}", next.op);
                        n.outputs = next.outputs;
                    }
                }
            }
            plan.nodes.push(n);
        }
        let device = if let Some(g) = shared {
            g.queue.device()
        } else {
            MTLCreateSystemDefaultDevice().context("Metal GPU unavailable")?
        };
        ensure!(
            device.supportsFamily(MTLGPUFamily::Apple9),
            "Ultra tensor shaders require an Apple9 GPU or newer"
        );
        let pipelines = if let Some(g) = shared {
            g.pipelines.clone()
        } else {
            let data = dispatch2::DispatchData::from_bytes(include_bytes!(concat!(
                env!("OUT_DIR"),
                "/listen.metallib"
            )));
            let lib = device
                .newLibraryWithData_error(&data)
                .map_err(|e| anyhow!("Metal shader load: {e}"))?;
            let mut pipelines = HashMap::new();
            for name in [
                "element",
                "layout",
                "norm",
                "softmax_rows",
                "convolution",
                "matvec",
                "im2col",
                "sum_time",
                "embedding",
                "lstm_project",
                "lstm_cell",
                "stack_two",
                "tdt_gather",
                "tdt_decision",
                "tdt_commit",
                "biga_f16",
                "tile4_f16",
            ] {
                let f = lib
                    .newFunctionWithName(&NSString::from_str(name))
                    .context("Missing shader function")?;
                let pipe = device
                    .newComputePipelineStateWithFunction_error(&f)
                    .map_err(|e| anyhow!("Metal pipeline {name}: {e}"))?;
                pipelines.insert(name.into(), pipe);
            }
            pipelines
        };
        let queue = if let Some(g) = shared {
            g.queue.clone()
        } else {
            device
                .newCommandQueue()
                .ok_or_else(|| crate::gpu_recovery::record("Metal queue unavailable"))?
        };
        let constant_bank = shared.map(|g| g.constant_bank.clone()).unwrap_or_default();
        let mut b = Builder {
            device,
            pipelines: pipelines.clone(),
            slots: HashMap::new(),
            spare: vec![],
            next: 0,
            steps: vec![],
            plan,
            blob,
            constant_bank: constant_bank.clone(),
        };
        let mut inputs = HashMap::new();
        for &name in input_names {
            let s =
                b.alloc(product(&b.plan.shapes[name]) * if name == "targets" { 2 } else { 1 })?;
            b.slots.insert(name.into(), s.clone());
            inputs.insert(name.into(), s);
        }
        let nodes = std::mem::take(&mut b.plan.nodes);
        let mut last = HashMap::new();
        for (i, n) in nodes.iter().enumerate() {
            for s in n.args.values() {
                if s.starts_with('(') {
                    for v in s.trim_matches(['(', ')']).split(',') {
                        last.insert(v.trim().into(), i);
                    }
                } else {
                    last.insert(s.clone(), i);
                }
            }
        }
        for name in &b.plan.outputs {
            last.insert(name.clone(), usize::MAX);
        }
        for (i, n) in nodes.iter().enumerate() {
            let first_step = b.steps.len();
            b.node(n)
                .with_context(|| format!("{} {}", n.op, n.outputs[0].name))?;
            for step in &mut b.steps[first_step..] {
                step.tensor = n.outputs[0].name.clone();
            }
            let expired: Vec<_> = b
                .slots
                .keys()
                .filter(|k| {
                    last.get(*k) == Some(&i)
                        && !b.plan.constants.contains_key(*k)
                        && !inputs.contains_key(*k)
                })
                .cloned()
                .collect();
            for name in expired {
                let s = b.slots.remove(&name).unwrap();
                if !b.slots.values().any(|v| v.id == s.id) && !b.spare.iter().any(|v| v.id == s.id)
                {
                    b.spare.push(s);
                }
            }
        }
        let outputs = b
            .plan
            .outputs
            .clone()
            .iter()
            .map(|n| Ok((n.clone(), b.get(n)?)))
            .collect::<Result<HashMap<_, _>>>()?;
        let buffers = b
            .slots
            .values()
            .chain(b.spare.iter())
            .map(|s| s.buffer.clone())
            .collect();
        Ok(Self {
            queue,
            steps: b.steps,
            inputs,
            outputs,
            _buffers: buffers,
            shapes: b.plan.shapes,
            pipelines,
            constant_bank,
            batch_models: HashMap::new(),
        })
    }
    pub fn encode(&mut self, mel: &[f32], length: usize) -> Result<(Vec<f32>, usize)> {
        ensure!(
            mel.len() == 128 * 1501 && length <= 1501,
            "Metal encoder input schema"
        );
        for (name, v) in [("mel", mel), ("mel_length", &[length as f32][..])] {
            self.write(name, v)?;
        }
        self.run()?;
        let output = self.read("encoder")?;
        ensure!(
            output.iter().all(|x| x.is_finite()),
            "Non-finite Metal encoder output"
        );
        Ok((output, self.read("encoder_length")?[0] as usize))
    }
    /// Already-ready streams share all weights/pipelines, one command buffer
    /// and one wait. No admission delay. Activation plans are cached by size.
    pub fn encode_batch(
        &mut self,
        mels: &[Vec<f32>],
        lengths: &[usize],
    ) -> Result<Vec<(Vec<f32>, usize)>> {
        let count = mels.len();
        ensure!(
            count == lengths.len() && (1..=32).contains(&count),
            "encoder batch schema"
        );
        ensure!(
            mels.iter().all(|m| m.len() == 128 * 1501) && lengths.iter().all(|n| *n <= 1501),
            "encoder batch inputs"
        );
        if count == 1 {
            return Ok(vec![self.encode(&mels[0], lengths[0])?]);
        }
        if !self.batch_models.contains_key(&count) {
            let mut p: Plan = serde_json::from_str(include_str!("plans/Encoder.json"))?;
            for (name, shape) in &mut p.shapes {
                if !p.constants.contains_key(name) && shape.first() == Some(&1) {
                    shape[0] = count;
                }
            }
            for n in &mut p.nodes {
                for o in &mut n.outputs {
                    o.shape = p.shapes[&o.name].clone();
                }
            }
            // All weight tensors are in the shared constant bank. Shape, axis
            // and convolution metadata are inline in the pinned plan, so a
            // new activation shape needs no second weight-file read/upload.
            let g = Self::build_shared(p, vec![], &["mel", "mel_length"], Some(self))?;
            self.batch_models.insert(count, Box::new(g));
        }
        let g = &self.batch_models[&count];
        unsafe {
            let ptr = g.inputs["mel"].buffer.contents().as_ptr().cast::<u16>();
            for (i, m) in mels.iter().enumerate() {
                for (j, v) in m.iter().enumerate() {
                    *ptr.add(i * m.len() + j) = f16::from_f32(*v).to_bits();
                }
            }
        }
        g.write(
            "mel_length",
            &lengths.iter().map(|n| *n as f32).collect::<Vec<_>>(),
        )?;
        g.run()?;
        let data = g.read("encoder")?;
        let n = g.read("encoder_length")?;
        ensure!(
            data.iter().all(|v| v.is_finite()),
            "Non-finite batch encoder output"
        );
        Ok(data
            .as_chunks::<{ 1024 * 188 }>()
            .0
            .iter()
            .zip(n)
            .map(|(v, n)| (v.to_vec(), n as usize))
            .collect())
    }
    fn run(&self) -> Result<()> {
        let cmd = crate::gpu_recovery::command_buffer(&self.queue)?;
        let enc = cmd
            .computeCommandEncoder()
            .ok_or_else(|| crate::gpu_recovery::record("Metal encoder unavailable"))?;
        self.encode_steps(&enc);
        enc.endEncoding();
        cmd.commit();
        crate::gpu_recovery::completed(&cmd)?;
        Ok(())
    }
    fn encode_steps(&self, enc: &ProtocolObject<dyn MTLComputeCommandEncoder>) {
        for s in &self.steps {
            enc.setComputePipelineState(&s.pipeline);
            for (i, b) in &s.buffers {
                unsafe {
                    enc.setBuffer_offset_atIndex(Some(b), 0, *i);
                }
            }
            unsafe {
                enc.setBytes_length_atIndex(
                    NonNull::new(s.params.as_ptr() as *mut c_void).unwrap(),
                    s.params.len() * 4,
                    s.param_slot,
                );
            }
            enc.dispatchThreadgroups_threadsPerThreadgroup(s.grid, s.group);
        }
    }
    /// Diagnostic only: GPU stage-boundary timestamps, one pass per dispatch,
    /// one command buffer and one completion wait. Pass overhead is included.
    fn profile(&self) -> Result<serde_json::Value> {
        let device = self.queue.device();
        ensure!(
            device.supportsCounterSampling(MTLCounterSamplingPoint::AtStageBoundary),
            "GPU stage timestamp sampling unavailable"
        );
        let sets = device.counterSets().context("GPU counters unavailable")?;
        let set = sets
            .iter()
            .find(|s| s.name().to_string().to_lowercase().contains("timestamp"))
            .with_context(|| {
                format!(
                    "Timestamp counter set; available {:?}",
                    sets.iter()
                        .map(|s| s.name().to_string())
                        .collect::<Vec<_>>()
                )
            })?;
        let desc = MTLCounterSampleBufferDescriptor::new();
        desc.setCounterSet(Some(&set));
        desc.setStorageMode(MTLStorageMode::Shared);
        unsafe {
            desc.setSampleCount(self.steps.len() * 2);
        }
        let samples = device
            .newCounterSampleBufferWithDescriptor_error(&desc)
            .map_err(|e| anyhow!("GPU timestamp buffer: {e}"))?;
        let cmd = crate::gpu_recovery::command_buffer(&self.queue)?;
        for (i, s) in self.steps.iter().enumerate() {
            let pass = MTLComputePassDescriptor::new();
            let a = unsafe { pass.sampleBufferAttachments().objectAtIndexedSubscript(0) };
            a.setSampleBuffer(Some(&samples));
            unsafe {
                a.setStartOfEncoderSampleIndex(2 * i);
                a.setEndOfEncoderSampleIndex(2 * i + 1);
            }
            let enc = cmd
                .computeCommandEncoderWithDescriptor(&pass)
                .ok_or_else(|| crate::gpu_recovery::record("profile encoder unavailable"))?;
            enc.setComputePipelineState(&s.pipeline);
            for (i, b) in &s.buffers {
                unsafe {
                    enc.setBuffer_offset_atIndex(Some(b), 0, *i);
                }
            }
            unsafe {
                enc.setBytes_length_atIndex(
                    NonNull::new(s.params.as_ptr() as *mut c_void).unwrap(),
                    s.params.len() * 4,
                    s.param_slot,
                );
            }
            enc.dispatchThreadgroups_threadsPerThreadgroup(s.grid, s.group);
            enc.endEncoding();
        }
        cmd.commit();
        crate::gpu_recovery::completed(&cmd)?;
        let data = unsafe { samples.resolveCounterRange(NSRange::new(0, self.steps.len() * 2)) }
            .context("resolve GPU timestamps")?;
        let bytes = unsafe { data.as_bytes_unchecked() };
        ensure!(bytes.len() == self.steps.len() * 16, "timestamp count");
        let times: Vec<_> = bytes
            .as_chunks::<8>()
            .0
            .iter()
            .map(|b| u64::from_ne_bytes(*b))
            .collect();
        // Legacy MTLCounterResultTimestamp is resolved in nanoseconds.
        // queryTimestampFrequency describes raw Metal 4 timestamps instead.
        let freq = 1_000_000_000u64;
        let rows: Vec<_> = self.steps.iter().enumerate().map(|(i,s)|serde_json::json!({"kernel":s.kernel,"tensor":s.tensor,"gpu_us":(times[2*i+1]-times[2*i]) as f64*1e6/freq as f64,"grid":[s.grid.width,s.grid.height,s.grid.depth]})).collect();
        Ok(
            serde_json::json!({"timestamp_hz":freq,"command_gpu_seconds":cmd.GPUEndTime()-cmd.GPUStartTime(),"steps":rows}),
        )
    }
    fn write(&self, name: &str, v: &[f32]) -> Result<()> {
        ensure!(product(&self.shapes[name]) == v.len(), "input tensor shape");
        let b = &self.inputs[name].buffer;
        unsafe {
            let p = b.contents().as_ptr().cast::<u16>();
            for (i, v) in v.iter().enumerate() {
                *p.add(i) = f16::from_f32(*v).to_bits();
            }
        }
        Ok(())
    }
    fn read(&self, name: &str) -> Result<Vec<f32>> {
        let n = product(&self.shapes[name]);
        let b = &self.outputs[name].buffer;
        Ok(
            unsafe { std::slice::from_raw_parts(b.contents().as_ptr().cast::<u16>(), n) }
                .iter()
                .map(|v| f16::from_bits(*v).to_f32())
                .collect(),
        )
    }
}

/// Capture the current encoder once, then reuse its exact mel/output for port checks.
/// Private clips and captures belong only in the ignored local receipt directory.
pub fn compare_encoder(path: &Path, audio: &[f32], receipt: &Path) -> Result<serde_json::Value> {
    use crate::coreml::{Array, Model};
    use objc2_core_ml::*;
    let tagged =
        receipt.join(std::env::var("HEIMDALL_REFERENCE_TAG").unwrap_or_else(|_| ".".into()));
    let receipt = &tagged;
    let receipt = if std::env::var("HEIMDALL_REFERENCE_CPU").as_deref() == Ok("1") {
        receipt.join("cpu")
    } else if let Ok(tensor) = std::env::var("HEIMDALL_METAL_CAPTURE_TENSOR") {
        receipt.join(tensor)
    } else {
        receipt.to_path_buf()
    };
    std::fs::create_dir_all(&receipt)?;
    let mp = receipt.join("mel.bin");
    let ep = receipt.join("encoder.bin");
    let lp = receipt.join("length.json");
    if !mp.exists() {
        let mut pre = Model::load(&path.join("Preprocessor.mlmodelc"), MLComputeUnits::CPUOnly)?;
        let encoder_path = if let Ok(tensor) = std::env::var("HEIMDALL_METAL_CAPTURE_TENSOR") {
            let debug = receipt.join("Encoder.mlmodelc");
            std::fs::create_dir_all(debug.join("weights"))?;
            for name in ["coremldata.bin", "weights/weight.bin"] {
                if !debug.join(name).exists() {
                    std::fs::hard_link(path.join("Encoder.mlmodelc").join(name), debug.join(name))?;
                }
            }
            let mil = std::fs::read_to_string(path.join("Encoder.mlmodelc/model.mil"))?;
            let changed = mil.replace("x = audio_signal_cast_fp16)", &format!("x = {tensor})"));
            ensure!(changed != mil, "debug output rewrite");
            std::fs::write(debug.join("model.mil"), changed)?;
            debug
        } else {
            path.join("Encoder.mlmodelc")
        };
        let units = if std::env::var("HEIMDALL_REFERENCE_CPU").as_deref() == Ok("1") {
            MLComputeUnits::CPUOnly
        } else {
            MLComputeUnits::CPUAndNeuralEngine
        };
        let mut encoder = Model::load(&encoder_path, units)?;
        let a = Array::zeros(&[1, 240000], MLMultiArrayDataType::Float32)?;
        let mut padded = vec![0.; 240000];
        padded[..audio.len()].copy_from_slice(audio);
        a.copy_f32(&padded)?;
        let len = Array::zeros(&[1], MLMultiArrayDataType::Int32)?;
        len.set(0, audio.len() as f32);
        let out = pre.predict(&[("audio_signal", &a), ("audio_length", &len)])?;
        let mel = out.array("mel")?;
        let n = out.array("mel_length")?;
        let out = encoder.predict(&[("mel", &mel), ("mel_length", &n)])?;
        let enc = out.array("encoder")?;
        let bytes = |v: Vec<f32>| v.iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<_>>();
        std::fs::write(&mp, bytes(mel.values()?))?;
        std::fs::write(&ep, bytes(enc.values()?))?;
        std::fs::write(
            &lp,
            serde_json::to_vec(
                &serde_json::json!({"mel":n.get(0),"encoder":out.array("encoder_length")?.get(0)}),
            )?,
        )?;
    }
    let floats = |p: &Path| -> Result<Vec<f32>> {
        let b = std::fs::read(p)?;
        ensure!(b.len() % 4 == 0, "capture alignment");
        Ok(b.as_chunks::<4>()
            .0
            .iter()
            .map(|v| f32::from_le_bytes(*v))
            .collect())
    };
    let mel = floats(&mp)?;
    let reference = floats(&ep)?;
    let len: serde_json::Value = serde_json::from_slice(&std::fs::read(lp)?)?;
    let mut gpu = MetalEncoder::load(path)?;
    let start = std::time::Instant::now();
    let (mut result, n) = gpu.encode(&mel, len["mel"].as_f64().unwrap() as usize)?;
    if let Ok(tensor) = std::env::var("HEIMDALL_METAL_CAPTURE_TENSOR") {
        let data = gpu.read(&tensor)?;
        ensure!(
            gpu.shapes[&tensor] == [1, 188, 1024],
            "capture shape must precede final transpose"
        );
        result = (0..1024 * 188)
            .map(|i| data[(i % 188) * 1024 + i / 188])
            .collect();
    }
    let mut error = 0.;
    let mut power = 0.;
    let mut max = 0f32;
    for (x, y) in result.iter().zip(&reference) {
        error += f64::from(x - y).powi(2);
        power += f64::from(*y).powi(2);
        max = max.max((x - y).abs());
    }
    std::fs::write(
        receipt.join("metal.bin"),
        result
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect::<Vec<_>>(),
    )?;
    Ok(
        serde_json::json!({"relative_l2":(error/power).sqrt(),"max_abs":max,"reference_len":len["encoder"],"metal_len":n,"seconds":start.elapsed().as_secs_f64(),"first_reference":&reference[..8],"first_metal":&result[..8]}),
    )
}

/// Reuse all 24 captured Core ML layer outputs with a single GPU forward.
pub fn compare_encoder_layers(path: &Path, receipt: &Path) -> Result<serde_json::Value> {
    let mut plan: Plan = serde_json::from_str(include_str!("plans/Encoder.json"))?;
    let names: Vec<String> = plan
        .nodes
        .iter()
        .filter(|n| {
            n.op == "layer_norm" && n.args.get("gamma").is_some_and(|g| g.contains("norm_out"))
        })
        .map(|n| n.outputs[0].name.clone())
        .collect();
    plan.outputs = names.clone();
    let graph = MetalEncoder::build(
        plan,
        std::fs::read(path.join("Encoder.mlmodelc/weights/weight.bin"))?,
        &["mel", "mel_length"],
    )?;
    let floats = |p: &Path| -> Result<Vec<f32>> {
        let b = std::fs::read(p)?;
        ensure!(b.len() % 4 == 0, "Layer reference alignment");
        Ok(b.as_chunks::<4>()
            .0
            .iter()
            .map(|v| f32::from_le_bytes(*v))
            .collect())
    };
    let len: serde_json::Value =
        serde_json::from_slice(&std::fs::read(receipt.join("length.json"))?)?;
    graph.write("mel", &floats(&receipt.join("mel.bin"))?)?;
    graph.write("mel_length", &[len["mel"].as_f64().unwrap() as f32])?;
    graph.run()?;
    let mut rows = vec![];
    for (i, name) in names.iter().enumerate() {
        let a = floats(&if i == 23 {
            receipt.join("encoder.bin")
        } else {
            receipt.join(name).join("encoder.bin")
        })?;
        let raw = graph.read(name)?;
        let b: Vec<_> = (0..1024 * 188)
            .map(|j| raw[j % 188 * 1024 + j / 188])
            .collect();
        let err = a
            .iter()
            .zip(&b)
            .map(|(a, b)| f64::from(a - b).powi(2))
            .sum::<f64>();
        let power = a.iter().map(|a| f64::from(*a).powi(2)).sum::<f64>();
        let max = a
            .iter()
            .zip(&b)
            .map(|(a, b)| (a - b).abs())
            .fold(0f32, f32::max);
        rows.push(serde_json::json!({"layer":i,"tensor":name,"relative_l2":(err/power).sqrt(),"max_abs":max}));
    }
    let report = serde_json::json!(rows);
    std::fs::write(
        receipt.join("layers-final.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    Ok(report)
}

/// Teacher-force each layer with the saved preceding Core ML activation.
/// This separates an implementation error from accumulated upstream rounding.
pub fn compare_encoder_isolated(path: &Path, receipt: &Path) -> Result<serde_json::Value> {
    let source: serde_json::Value = serde_json::from_str(include_str!("plans/Encoder.json"))?;
    let nodes = source["nodes"].as_array().context("encoder nodes")?;
    let ends: Vec<usize> = nodes
        .iter()
        .enumerate()
        .filter_map(|(i, n)| {
            (n["op"] == "layer_norm"
                && n["args"]["gamma"]
                    .as_str()
                    .is_some_and(|g| g.contains("norm_out")))
            .then_some(i)
        })
        .collect();
    let floats = |p: &Path| -> Result<Vec<f32>> {
        let b = std::fs::read(p)?;
        ensure!(b.len() % 4 == 0, "reference alignment");
        Ok(b.as_chunks::<4>()
            .0
            .iter()
            .map(|v| f32::from_le_bytes(*v))
            .collect())
    };
    let length: serde_json::Value =
        serde_json::from_slice(&std::fs::read(receipt.join("length.json"))?)?;
    let blob = std::fs::read(path.join("Encoder.mlmodelc/weights/weight.bin"))?;
    let mut rows = vec![];
    for (layer, &end) in ends.iter().enumerate() {
        let start = if layer == 0 { 37 } else { ends[layer - 1] + 1 };
        let input = if layer == 0 {
            "linear_0_cast_fp16"
        } else {
            nodes[ends[layer - 1]]["outputs"][0]["name"]
                .as_str()
                .unwrap()
        };
        let output = nodes[end]["outputs"][0]["name"].as_str().unwrap();
        let mut value = source.clone();
        value["nodes"] = serde_json::json!(nodes[..13]
            .iter()
            .chain(nodes[27..37].iter())
            .chain([&nodes[64], &nodes[80]])
            .chain(nodes[start..=end].iter())
            .collect::<Vec<_>>());
        value["outputs"] = serde_json::json!([output]);
        let g = MetalEncoder::build(
            serde_json::from_value(value)?,
            blob.clone(),
            &["mel_length", input],
        )?;
        let a = floats(&receipt.join(input).join("encoder.bin"))?;
        let a: Vec<_> = (0..a.len()).map(|i| a[i % 1024 * 188 + i / 1024]).collect();
        g.write(input, &a)?;
        g.write("mel_length", &[length["mel"].as_f64().unwrap() as f32])?;
        g.run()?;
        let raw = g.read(output)?;
        let b: Vec<_> = (0..raw.len())
            .map(|i| raw[i % 188 * 1024 + i / 188])
            .collect();
        let a = floats(&if layer == 23 {
            receipt.join("encoder.bin")
        } else {
            receipt.join(output).join("encoder.bin")
        })?;
        let err = a
            .iter()
            .zip(&b)
            .map(|(a, b)| f64::from(a - b).powi(2))
            .sum::<f64>();
        let power = a.iter().map(|a| f64::from(*a).powi(2)).sum::<f64>();
        rows.push(serde_json::json!({"layer":layer,"relative_l2":(err/power).sqrt(),"max_abs":a.iter().zip(b).map(|(a,b)|(a-b).abs()).fold(0f32,f32::max)}));
    }
    Ok(serde_json::json!(rows))
}

/// Diagnose FP16 overflow across one fixed clip without accepting a failed output.
pub fn encoder_finite(path: &Path, audio: &[f32]) -> Result<serde_json::Value> {
    use crate::coreml::{Array, Model};
    use objc2_core_ml::*;
    ensure!(audio.len() <= 240000, "Static clip length");
    let mut pre = Model::load(&path.join("Preprocessor.mlmodelc"), MLComputeUnits::CPUOnly)?;
    let a = Array::zeros(&[1, 240000], MLMultiArrayDataType::Float32)?;
    let mut padded = vec![0.; 240000];
    padded[..audio.len()].copy_from_slice(audio);
    a.copy_f32(&padded)?;
    let n = Array::zeros(&[1], MLMultiArrayDataType::Int32)?;
    n.set(0, audio.len() as f32);
    let features = pre.predict(&[("audio_signal", &a), ("audio_length", &n)])?;
    let mut plan: Plan = serde_json::from_str(include_str!("plans/Encoder.json"))?;
    let names: Vec<String> = plan
        .nodes
        .iter()
        .flat_map(|n| n.outputs.iter().map(|o| o.name.clone()))
        .collect();
    plan.outputs = names.clone();
    let graph = MetalEncoder::build(
        plan,
        std::fs::read(path.join("Encoder.mlmodelc/weights/weight.bin"))?,
        &["mel", "mel_length"],
    )?;
    graph.write("mel", &features.array("mel")?.values()?)?;
    graph.write("mel_length", &[features.array("mel_length")?.get(0)])?;
    graph.run()?;
    let mut rows = vec![];
    for name in names {
        let v = graph.read(&name)?;
        let count = v.iter().filter(|v| !v.is_finite()).count();
        if count > 0 {
            rows.push(serde_json::json!({"name":name,"nonfinite":count,"first":v.iter().position(|v|!v.is_finite())}));
            if rows.len() == 12 {
                break;
            }
        }
    }
    Ok(serde_json::json!(rows))
}

pub fn inspect_encoder(path: &Path, receipt: &Path, name: &str) -> Result<serde_json::Value> {
    let bytes = std::fs::read(receipt.join("mel.bin"))?;
    let mel: Vec<f32> = bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|v| f32::from_le_bytes(*v))
        .collect();
    let len: serde_json::Value =
        serde_json::from_slice(&std::fs::read(receipt.join("length.json"))?)?;
    let mut gpu = MetalEncoder::load(path)?;
    gpu.encode(&mel, len["mel"].as_f64().unwrap() as usize)?;
    let v = gpu.read(name)?;
    Ok(
        serde_json::json!({"name":name,"shape":gpu.shapes[name],"min":v.iter().copied().fold(f32::INFINITY,f32::min),"max":v.iter().copied().fold(f32::NEG_INFINITY,f32::max),"mean":v.iter().map(|x|f64::from(*x)).sum::<f64>()/v.len() as f64,"zeros":v.iter().filter(|x|**x==0.).count(),"first":&v[..v.len().min(16)]}),
    )
}

pub fn profile_encoder(path: &Path, receipt: &Path) -> Result<serde_json::Value> {
    let bytes = std::fs::read(receipt.join("mel.bin"))?;
    let mel: Vec<_> = bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|v| f32::from_le_bytes(*v))
        .collect();
    let len: serde_json::Value =
        serde_json::from_slice(&std::fs::read(receipt.join("length.json"))?)?;
    let mut g = MetalEncoder::load(path)?;
    g.encode(&mel, len["mel"].as_f64().unwrap() as usize)?;
    g.profile()
}

pub fn compare_encoder_batch(path: &Path, receipt: &Path) -> Result<serde_json::Value> {
    let bytes = std::fs::read(receipt.join("mel.bin"))?;
    let base: Vec<_> = bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|v| f32::from_le_bytes(*v))
        .collect();
    let mut g = MetalEncoder::load(path)?;
    let mut rows = vec![];
    for count in [3, 6] {
        let mels: Vec<Vec<f32>> = (0..count)
            .map(|i| {
                base.iter()
                    .enumerate()
                    .map(|(j, v)| v + (i as f32) * 0.002 * ((j % 7) as f32 - 3.))
                    .collect()
            })
            .collect();
        let lengths: Vec<_> = (0..count).map(|i| 877 - i * 61).collect();
        let serial = mels
            .iter()
            .zip(&lengths)
            .map(|(m, n)| g.encode(m, *n))
            .collect::<Result<Vec<_>>>()?;
        let batched = g.encode_batch(&mels, &lengths)?;
        let max = serial
            .iter()
            .zip(&batched)
            .map(|((a, na), (b, nb))| {
                ensure!(na == nb, "batch encoder length");
                Ok(a.iter()
                    .zip(b)
                    .map(|(a, b)| f64::from(a - b).powi(2))
                    .sum::<f64>()
                    .sqrt()
                    / a.iter().map(|a| f64::from(*a).powi(2)).sum::<f64>().sqrt())
            })
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .fold(0f64, f64::max);
        let mut times = vec![];
        for arm in [
            "serial", "batch", "batch", "serial", "batch", "serial", "serial", "batch",
        ] {
            let start = std::time::Instant::now();
            if arm == "batch" {
                g.encode_batch(&mels, &lengths)?;
            } else {
                for (m, n) in mels.iter().zip(&lengths) {
                    g.encode(m, *n)?;
                }
            }
            times.push(serde_json::json!({"arm":arm,"seconds":start.elapsed().as_secs_f64()}));
        }
        let child = &g.batch_models[&count];
        let shared = g.constant_bank.borrow().iter().all(|(name, s)| {
            child
                .constant_bank
                .borrow()
                .get(name)
                .is_some_and(|v| v.buffer == s.buffer)
        });
        ensure!(shared, "encoder shared weight bank");
        rows.push(serde_json::json!({"streams":count,"max_relative_l2":max,"shared_weights":shared,"times":times}));
    }
    Ok(serde_json::json!(rows))
}

/// One shared tuned turn model; per-stream features/state remain outside its weights.
pub struct MetalTurn {
    graph: MetalEncoder,
}
impl MetalTurn {
    pub fn load(path: &Path) -> Result<Self> {
        let plan: Plan =
            serde_json::from_slice(&std::fs::read(path.with_extension("metal.json"))?)?;
        ensure!(
            format!("{:x}", Sha256::digest(std::fs::read(path)?)) == plan.mil_sha256,
            "Tuned turn Metal data does not match ONNX"
        );
        let weights = std::fs::read(path.with_extension("metal.bin"))?;
        ensure!(
            plan.weights_sha256.as_deref() == Some(&format!("{:x}", Sha256::digest(&weights))),
            "Tuned turn derived weight checksum"
        );
        let graph = MetalEncoder::build(plan, weights, &["input_features"])?;
        Ok(Self { graph })
    }
    pub fn probability(&mut self, features: &[f32]) -> Result<f32> {
        self.graph.write("input_features", features)?;
        self.graph.run()?;
        let p = self.graph.read("probability")?[0];
        ensure!(
            p.is_finite() && (0.0..=1.0).contains(&p),
            "Invalid Metal turn probability"
        );
        Ok(p)
    }
}

/// Compare every tuned-turn graph operation against a cached CPU ONNX tensor.
/// A conversion-only debug ONNX adds outputs without changing its computation.
pub fn compare_turn(path: &Path, audio: &[f32], receipt: &Path) -> Result<serde_json::Value> {
    std::fs::create_dir_all(receipt)?;
    let mut plan: Plan =
        serde_json::from_slice(&std::fs::read(path.with_extension("metal.json"))?)?;
    ensure!(
        format!("{:x}", Sha256::digest(std::fs::read(path)?)) == plan.mil_sha256,
        "Turn reference checkpoint"
    );
    let weights = std::fs::read(path.with_extension("metal.bin"))?;
    ensure!(
        plan.weights_sha256.as_deref() == Some(&format!("{:x}", Sha256::digest(&weights))),
        "Turn derived weight checksum"
    );
    let names: Vec<String> = serde_json::from_slice(&std::fs::read(receipt.join("outputs.json"))?)?;
    ensure!(
        names.iter().all(|n| plan.shapes.contains_key(n)),
        "Turn reference output schema"
    );
    let features_path = receipt.join("features.bin");
    if !features_path.exists() {
        let features = crate::features::WhisperFeatures::default().extract(audio);
        let mut reference = crate::onnx::session(&receipt.join("reference.onnx"))?;
        let shape = plan.shapes["input_features"].clone();
        let result=reference.run(ort::inputs!["input_features"=>ort::value::Tensor::from_array((shape,features.clone()))?])?;
        for (i, name) in names.iter().enumerate() {
            let (_, v) = result[name.as_str()].try_extract_tensor::<f32>()?;
            std::fs::write(
                receipt.join(format!("reference-{i}.bin")),
                v.iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<_>>(),
            )?;
        }
        std::fs::write(
            &features_path,
            features
                .iter()
                .flat_map(|v| v.to_le_bytes())
                .collect::<Vec<_>>(),
        )?;
    }
    let floats = |p: &Path| -> Result<Vec<f32>> {
        let b = std::fs::read(p)?;
        ensure!(b.len() % 4 == 0, "Reference float alignment");
        Ok(b.as_chunks::<4>()
            .0
            .iter()
            .map(|v| f32::from_le_bytes(*v))
            .collect())
    };
    plan.outputs = names.clone();
    let gpu = MetalEncoder::build(plan, weights, &["input_features"])?;
    gpu.write("input_features", &floats(&features_path)?)?;
    gpu.run()?;
    let mut rows = vec![];
    for (i, name) in names.iter().enumerate() {
        let a = floats(&receipt.join(format!("reference-{i}.bin")))?;
        let b = gpu.read(name)?;
        ensure!(a.len() == b.len(), "Turn captured tensor length");
        let e = a
            .iter()
            .zip(&b)
            .map(|(a, b)| f64::from(a - b).powi(2))
            .sum::<f64>();
        let power = a.iter().map(|a| f64::from(*a).powi(2)).sum::<f64>();
        let max = a
            .iter()
            .zip(&b)
            .map(|(a, b)| (a - b).abs())
            .fold(0f32, f32::max);
        rows.push(serde_json::json!({"name":name,"relative_l2":(e/power).sqrt(),"max_abs":max}));
    }
    let report = serde_json::json!(rows);
    std::fs::write(
        receipt.join("comparison.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    Ok(report)
}

/// GPU prediction and joint networks; the caller retains the Rust TDT search policy.
pub struct MetalDecode {
    prediction: MetalEncoder,
    joint: MetalEncoder,
    tdt: Option<TdtBuffers>,
    vocabulary: Option<BiasBuffers>,
    bias_node: usize,
    bias_quiet_frames: usize,
}
struct BiasBuffers {
    trie: crate::vocabulary::BiasTrie,
    next: Buffer,
    bonus: Buffer,
    state: Buffer,
}
struct TdtBuffers {
    encoded: Buffer,
    state: Buffer,
    records: Buffer,
}
impl MetalDecode {
    pub fn load(path: &Path) -> Result<Self> {
        let mut plan: Plan = serde_json::from_str(include_str!("plans/Decoder.json"))?;
        let mil = std::fs::read(path.join("Decoder.mlmodelc/model.mil"))?;
        ensure!(
            format!("{:x}", Sha256::digest(&mil)) == plan.mil_sha256,
            "Decoder model identity"
        );
        // Runtime only emits nonnegative vocabulary IDs; gather them exactly as uint32.
        for n in &mut plan.nodes {
            if n.op == "gather" {
                n.args.insert("indices".into(), "targets".into());
            }
        }
        let mut needed: std::collections::HashSet<String> = plan.outputs.iter().cloned().collect();
        plan.nodes = plan
            .nodes
            .into_iter()
            .rev()
            .filter(|n| {
                if n.outputs.iter().any(|v| needed.contains(&v.name)) {
                    for arg in n.args.values() {
                        if arg.starts_with('(') {
                            needed.extend(
                                arg.trim_matches(['(', ')'])
                                    .split(',')
                                    .map(|v| v.trim().into()),
                            );
                        } else {
                            needed.insert(arg.clone());
                        }
                    }
                    true
                } else {
                    false
                }
            })
            .collect();
        plan.nodes.reverse();
        let prediction = MetalEncoder::build(
            plan,
            std::fs::read(path.join("Decoder.mlmodelc/weights/weight.bin"))?,
            &["targets", "h_in", "c_in"],
        )?;
        let mut plan: Plan = serde_json::from_str(include_str!("plans/JointDecisionv3.json"))?;
        let mil = std::fs::read(path.join("JointDecisionv3.mlmodelc/model.mil"))?;
        ensure!(
            format!("{:x}", Sha256::digest(&mil)) == plan.mil_sha256,
            "Joint model identity"
        );
        // Rust owns token/duration decisions and their probability, as on the current path.
        let index = plan
            .nodes
            .iter()
            .position(|n| n.outputs[0].name == "linear_2_cast_fp16")
            .context("joint logits node")?;
        plan.nodes.truncate(index + 1);
        plan.outputs = vec!["linear_2_cast_fp16".into()];
        let joint = MetalEncoder::build(
            plan,
            std::fs::read(path.join("JointDecisionv3.mlmodelc/weights/weight.bin"))?,
            &["encoder_step", "decoder_step"],
        )?;
        Ok(Self {
            prediction,
            joint,
            tdt: None,
            vocabulary: None,
            bias_node: 0,
            bias_quiet_frames: 0,
        })
    }
    pub(crate) fn set_vocabulary(
        &mut self,
        config: &crate::vocabulary::Vocabulary,
        vocab: &[String],
    ) -> Result<()> {
        let trie = crate::vocabulary::BiasTrie::build(config, vocab)?;
        let device = self.prediction.queue.device();
        let alloc = |bytes| {
            device
                .newBufferWithLength_options(bytes, MTLResourceOptions::StorageModeShared)
                .ok_or_else(|| crate::gpu_recovery::record("TDT vocabulary buffer unavailable"))
        };
        let next = alloc(trie.next.len() * 4)?;
        let bonus = alloc(trie.bonus.len() * 4)?;
        let state = alloc(8)?;
        unsafe {
            std::ptr::copy_nonoverlapping(
                trie.next.as_ptr(),
                next.contents().as_ptr().cast(),
                trie.next.len(),
            );
            std::ptr::copy_nonoverlapping(
                trie.bonus.as_ptr(),
                bonus.contents().as_ptr().cast(),
                trie.bonus.len(),
            );
            *state.contents().as_ptr().cast::<u32>() = 0;
            *state.contents().as_ptr().cast::<u32>().add(1) = 0;
        }
        self.vocabulary = Some(BiasBuffers {
            trie,
            next,
            bonus,
            state,
        });
        self.bias_node = 0;
        Ok(())
    }
    pub(crate) fn reset_vocabulary(&mut self) {
        self.bias_node = 0;
        self.bias_quiet_frames = 0;
    }
    /// Keep prediction/decision state on the GPU; Rust consumes token records
    /// every 32 steps, with the same blank/duration/10-symbol search policy.
    pub fn decode(&mut self, encoded: &[f32], frames: usize) -> Result<Vec<crate::asr::Token>> {
        ensure!(
            encoded.len() == 1024 * 188 && frames <= 188,
            "TDT encoder schema"
        );
        if self.tdt.is_none() {
            let device = self.prediction.queue.device();
            let alloc = |bytes| {
                device
                    .newBufferWithLength_options(bytes, MTLResourceOptions::StorageModeShared)
                    .ok_or_else(|| crate::gpu_recovery::record("resident TDT buffer unavailable"))
            };
            self.tdt = Some(TdtBuffers {
                encoded: alloc(1024 * 188 * 2)?,
                state: alloc(32)?,
                records: alloc(32 * 16)?,
            });
        }
        // Decoder probes without a configured vocabulary use a zero root row.
        if self.vocabulary.is_none() {
            self.set_vocabulary(
                &crate::vocabulary::Vocabulary {
                    words: vec![],
                    boost: 0.,
                    foreign_penalty: 0.,
                },
                &vec![String::new(); 8192],
            )?;
        }
        let b = self.tdt.as_ref().unwrap();
        let bias = self.vocabulary.as_ref().unwrap();
        unsafe {
            *bias.state.contents().as_ptr().cast::<u32>() = 0;
            *bias.state.contents().as_ptr().cast::<u32>().add(1) = 0;
            let p = b.encoded.contents().as_ptr().cast::<u16>();
            for (i, v) in encoded.iter().enumerate() {
                *p.add(i) = f16::from_f32(*v).to_bits();
            }
            std::ptr::write_bytes(b.state.contents().as_ptr(), 0, 32);
            let state = b.state.contents().as_ptr().cast::<u32>();
            *state.add(1) = frames as u32;
            *state.add(3) = 1;
            for name in ["h_in", "c_in"] {
                std::ptr::write_bytes(
                    self.prediction.inputs[name].buffer.contents().as_ptr(),
                    0,
                    1280 * 2,
                );
            }
            *self.prediction.inputs["targets"]
                .buffer
                .contents()
                .as_ptr()
                .cast::<u32>() = 8192;
        }
        let gather = vec![
            (0, b.encoded.clone()),
            (1, b.state.clone()),
            (3, self.joint.inputs["encoder_step"].buffer.clone()),
        ];
        let decide = vec![
            (0, self.joint.outputs["linear_2_cast_fp16"].buffer.clone()),
            (1, b.state.clone()),
            (2, b.records.clone()),
            (3, self.prediction.inputs["targets"].buffer.clone()),
            (4, bias.next.clone()),
            (5, bias.bonus.clone()),
            (6, bias.state.clone()),
        ];
        let commit = vec![
            (0, b.state.clone()),
            (1, self.prediction.outputs["h_out"].buffer.clone()),
            (2, self.prediction.outputs["c_out"].buffer.clone()),
            (3, self.prediction.outputs["decoder"].buffer.clone()),
            (4, self.prediction.inputs["h_in"].buffer.clone()),
            (5, self.prediction.inputs["c_in"].buffer.clone()),
            (6, self.joint.inputs["decoder_step"].buffer.clone()),
        ];
        let dispatch = |enc: &ProtocolObject<dyn MTLComputeCommandEncoder>,
                        name: &str,
                        buffers: &[(usize, Buffer)],
                        groups: usize| {
            enc.setComputePipelineState(&self.prediction.pipelines[name]);
            for (i, b) in buffers {
                unsafe {
                    enc.setBuffer_offset_atIndex(Some(b), 0, *i);
                }
            }
            enc.dispatchThreadgroups_threadsPerThreadgroup(size(groups, 1, 1), size(256, 1, 1));
        };
        let mut tokens = vec![];
        let mut initial = true;
        for _ in 0..128 {
            unsafe {
                *b.state.contents().as_ptr().cast::<u32>().add(6) = 0;
            }
            let cmd = crate::gpu_recovery::command_buffer(&self.prediction.queue)?;
            let enc = cmd
                .computeCommandEncoder()
                .ok_or_else(|| crate::gpu_recovery::record("TDT encoder unavailable"))?;
            if initial {
                self.prediction.encode_steps(&enc);
                dispatch(&enc, "tdt_commit", &commit, 5);
                initial = false;
            }
            for _ in 0..32 {
                dispatch(&enc, "tdt_gather", &gather, 4);
                self.joint.encode_steps(&enc);
                dispatch(&enc, "tdt_decision", &decide, 1);
                self.prediction.encode_steps(&enc);
                dispatch(&enc, "tdt_commit", &commit, 5);
            }
            enc.endEncoding();
            cmd.commit();
            crate::gpu_recovery::completed(&cmd)?;
            let state =
                unsafe { std::slice::from_raw_parts(b.state.contents().as_ptr().cast::<u32>(), 8) };
            ensure!(state[7] == 0, "Nonfinite GPU TDT logits");
            let records = unsafe {
                std::slice::from_raw_parts(b.records.contents().as_ptr().cast::<u32>(), 32 * 4)
            };
            for r in records.as_chunks::<4>().0 {
                if r[0] != 8192 {
                    tokens.push(crate::asr::Token {
                        id: r[0] as usize,
                        frame: r[1] as usize,
                        duration: r[2] as usize,
                        probability: f32::from_bits(r[3]),
                    });
                }
            }
            ensure!(tokens.len() < 2048, "TDT token guard reached");
            if state[0] as usize >= frames {
                return Ok(tokens);
            }
        }
        bail!("TDT block guard reached")
    }
    pub fn predict(
        &mut self,
        token: usize,
        h: &[f32],
        c: &[f32],
    ) -> Result<(Vec<f32>, Vec<f32>, Vec<f32>)> {
        ensure!(token <= 8192, "decoder token bounds");
        unsafe {
            *self.prediction.inputs["targets"]
                .buffer
                .contents()
                .as_ptr()
                .cast::<u32>() = token as u32;
        }
        self.prediction.write("h_in", h)?;
        self.prediction.write("c_in", c)?;
        self.prediction.run()?;
        Ok((
            self.prediction.read("decoder")?,
            self.prediction.read("h_out")?,
            self.prediction.read("c_out")?,
        ))
    }
    pub fn decision(&mut self, enc: &[f32], dec: &[f32]) -> Result<(usize, usize, f32)> {
        self.joint.write("encoder_step", enc)?;
        self.joint.write("decoder_step", dec)?;
        self.joint.run()?;
        let logits = self.joint.read("linear_2_cast_fp16")?;
        ensure!(
            logits.iter().all(|x| x.is_finite()),
            "Nonfinite joint logits"
        );
        let argmax = |v: &[f32]| {
            v.iter()
                .enumerate()
                .fold((0, f32::NEG_INFINITY), |best, (i, x)| {
                    if *x > best.1 {
                        (i, *x)
                    } else {
                        best
                    }
                })
                .0
        };
        let raw_token = argmax(&logits[..8193]);
        if self.bias_quiet_frames > 8 {
            self.bias_node = 0;
        }
        let token = if let Some(bias) = &self.vocabulary {
            let candidate = (0..8193)
                .fold((0, f32::NEG_INFINITY), |best, i| {
                    let value = logits[i] + bias.trie.bonus[self.bias_node * 8193 + i];
                    if value > best.1 {
                        (i, value)
                    } else {
                        best
                    }
                })
                .0;
            if raw_token == 8192
                && bias.trie.next[self.bias_node * 8193 + candidate] & 0x8000_0000 == 0
            {
                raw_token
            } else {
                candidate
            }
        } else {
            raw_token
        };
        if let Some(bias) = &self.vocabulary {
            self.bias_node = bias.trie.advance(self.bias_node, token);
        }
        let duration = argmax(&logits[8193..]);
        self.bias_quiet_frames = if token == 8192 {
            self.bias_quiet_frames + duration.max(1)
        } else {
            0
        };
        let max = logits[token];
        let sum: f64 = logits[..8193]
            .iter()
            .map(|v| f64::from(*v - max).exp())
            .sum();
        let prob = f16::from_f32((1.0 / sum) as f32).to_f32();
        Ok((token, duration, prob))
    }
}

/// Profile prediction/joint kernels on one saved teacher-forced step. Stage
/// timestamps include pass-boundary overhead; they are diagnostic attribution,
/// not an additive estimate of the resident 32-step production command buffer.
pub fn profile_decoder(path: &Path, receipt: &Path) -> Result<serde_json::Value> {
    let rows: Vec<serde_json::Value> =
        serde_json::from_slice(&std::fs::read(receipt.join("decoder.json"))?)?;
    let row = rows.first().context("saved decoder reference")?;
    let floats = |key: &str| -> Result<Vec<f32>> {
        row[key]
            .as_array()
            .context("decoder tensor")?
            .iter()
            .map(|v| v.as_f64().map(|x| x as f32).context("decoder float"))
            .collect()
    };
    let mut g = MetalDecode::load(path)?;
    g.predict(
        row["token"].as_u64().context("decoder token")? as usize,
        &floats("h_in")?,
        &floats("c_in")?,
    )?;
    let prediction = g.prediction.profile()?;
    g.decision(&floats("encoder")?, &floats("projection")?)?;
    Ok(serde_json::json!({"prediction":prediction,"joint":g.joint.profile()?}))
}

pub fn compare_decoder(path: &Path, receipt: &Path) -> Result<serde_json::Value> {
    use crate::coreml::{Array, Model};
    use objc2_core_ml::*;
    std::fs::create_dir_all(receipt)?;
    let capture = receipt.join("decoder.json");
    if !capture.exists() {
        let mut decoder = Model::load(
            &path.join("Decoder.mlmodelc"),
            MLComputeUnits::CPUAndNeuralEngine,
        )?;
        let mut joint = Model::load(
            &path.join("JointDecisionv3.mlmodelc"),
            MLComputeUnits::CPUAndNeuralEngine,
        )?;
        let f = MLMultiArrayDataType::Float32;
        let target = Array::zeros(&[1, 1], MLMultiArrayDataType::Int32)?;
        let length = Array::zeros(&[1], MLMultiArrayDataType::Int32)?;
        length.set(0, 1.);
        let h = Array::zeros(&[2, 1, 640], f)?;
        let c = Array::zeros(&[2, 1, 640], f)?;
        let enc = Array::zeros(&[1, 1024, 1], f)?;
        let dec = Array::zeros(&[1, 640, 1], f)?;
        let mut hv: Vec<f32> = (0..1280).map(|i| ((i % 31) as f32 - 15.) / 128.).collect();
        let mut cv: Vec<f32> = (0..1280).map(|i| ((i % 23) as f32 - 11.) / 64.).collect();
        let ev: Vec<f32> = (0..1024).map(|i| ((i % 17) as f32 - 8.) / 256.).collect();
        enc.copy_f32(&ev)?;
        let mut rows = vec![];
        for token in [8192, 8191, 11, 42] {
            target.set(0, token as f32);
            h.copy_f32(&hv)?;
            c.copy_f32(&cv)?;
            let out = decoder.predict(&[
                ("targets", &target),
                ("target_length", &length),
                ("h_in", &h),
                ("c_in", &c),
            ])?;
            let pv = out.array("decoder")?.values()?;
            let hn = out.array("h_out")?.values()?;
            let cn = out.array("c_out")?.values()?;
            dec.copy_f32(&pv)?;
            let jo = joint.predict(&[("encoder_step", &enc), ("decoder_step", &dec)])?;
            rows.push(serde_json::json!({"token":token,"h_in":hv,"c_in":cv,"encoder":ev,"projection":pv,"h_out":hn,"c_out":cn,"joint_token":jo.array("token_id")?.get(0) as usize,"duration":jo.array("duration")?.get(0) as usize,"probability":jo.array("token_prob")?.get(0)}));
            hv = hn;
            cv = cn;
        }
        std::fs::write(&capture, serde_json::to_vec(&rows)?)?;
    }
    let rows: Vec<serde_json::Value> = serde_json::from_slice(&std::fs::read(capture)?)?;
    let mut gpu = MetalDecode::load(path)?;
    let floats = |v: &serde_json::Value| {
        v.as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_f64().unwrap() as f32)
            .collect::<Vec<_>>()
    };
    let mut result = vec![];
    for row in rows {
        let token = row["token"].as_u64().unwrap() as usize;
        let (p, h, c) = gpu.predict(token, &floats(&row["h_in"]), &floats(&row["c_in"]))?;
        let mut max = 0f32;
        let mut error = 0.;
        let mut norm = 0.;
        for (key, v) in [("projection", &p), ("h_out", &h), ("c_out", &c)] {
            for (x, y) in v.iter().zip(floats(&row[key])) {
                max = max.max((*x - y).abs());
                error += f64::from(*x - y).powi(2);
                norm += f64::from(y).powi(2);
            }
        }
        let (t, d, prob) = gpu.decision(&floats(&row["encoder"]), &floats(&row["projection"]))?;
        result.push(serde_json::json!({"token":token,"max_abs":max,"relative_l2":(error/norm).sqrt(),"joint_token_matches":row["joint_token"].as_u64().unwrap() as usize==t,"duration_matches":row["duration"].as_u64().unwrap() as usize==d,"probability_abs_error":(prob-row["probability"].as_f64().unwrap() as f32).abs()}));
    }
    Ok(serde_json::json!(result))
}
#[cfg(test)]
mod tests {
    use super::*;
    fn literal(values: Vec<f32>, shape: &[usize]) -> serde_json::Value {
        serde_json::json!({"dtype":"fp16","shape":shape,"values":values})
    }
    fn graph(
        op: &str,
        args: serde_json::Value,
        constants: serde_json::Value,
        shapes: serde_json::Value,
        shape: &[usize],
    ) -> Result<Vec<f32>> {
        let mut shapes = shapes;
        shapes["out"] = serde_json::json!(shape);
        let plan: Plan = serde_json::from_value(
            serde_json::json!({"mil_sha256":"", "shapes":shapes,"constants":constants,"nodes":[{"op":op,"args":args,"outputs":[{"name":"out","shape":shape,"dtype":"fp16"}]}],"outputs":["out"]}),
        )?;
        let g = MetalEncoder::build(plan, vec![], &[])?;
        g.run()?;
        g.read("out")
    }
    fn fixed(n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| f16::from_f32(((i * 17 % 101) as f32 - 50.) / 64.).to_f32())
            .collect()
    }
    fn close(a: &[f32], b: &[f32], tol: f32) {
        assert_eq!(a.len(), b.len());
        assert!(
            a.iter().chain(b).all(|v| v.is_finite()),
            "nonfinite comparison"
        );
        let e = a
            .iter()
            .zip(b)
            .map(|(x, y)| (x - y).abs())
            .fold(0., f32::max);
        assert!(e <= tol, "max error {e} > {tol}");
    }
    #[test]
    #[should_panic(expected = "nonfinite comparison")]
    fn parity_assertion_rejects_nan() {
        close(&[f32::NAN], &[0.], 1.);
    }
    #[test]
    fn gpu_vocabulary_completion_blank_gate_expiry_and_acoustic_confidence() {
        // A weak name token may win on context, but confidence stays acoustic.
        for (complete, frame, expected) in [(true, 0, 10), (false, 0, 8192), (true, 20, 8192)] {
            let mut logits = vec![-20.; 8198];
            logits[10] = 2.;
            logits[8192] = 5.;
            logits[8194] = 2.; // duration = 1
            let plan: Plan = serde_json::from_value(serde_json::json!({"mil_sha256":"","shapes":{"logits":[8198]},"constants":{"logits":literal(logits.clone(),&[8198])},"nodes":[],"outputs":["logits"]})).unwrap();
            let g = MetalEncoder::build(plan, vec![], &[]).unwrap();
            let device = g.queue.device();
            let alloc = |n| {
                device
                    .newBufferWithLength_options(n, MTLResourceOptions::StorageModeShared)
                    .unwrap()
            };
            let state = alloc(32);
            let records = alloc(16);
            let target = alloc(4);
            let next = alloc(3 * 8193 * 4);
            let bonus = alloc(3 * 8193 * 4);
            let bias = alloc(8);
            unsafe {
                for (b, bytes) in [
                    (&state, 32),
                    (&next, 3 * 8193 * 4),
                    (&bonus, 3 * 8193 * 4),
                    (&bias, 8),
                ] {
                    std::ptr::write_bytes(b.contents().as_ptr(), 0, bytes);
                }
                let s = state.contents().as_ptr().cast::<u32>();
                *s = frame;
                *s.add(1) = 30;
                *bias.contents().as_ptr().cast::<u32>() = 1;
                *next.contents().as_ptr().cast::<u32>().add(8193 + 10) =
                    2 | if complete { 0x8000_0000 } else { 0 };
                *bonus.contents().as_ptr().cast::<f32>().add(8193 + 10) = 4.;
            }
            let cmd = g.queue.commandBuffer().unwrap();
            let enc = cmd.computeCommandEncoder().unwrap();
            enc.setComputePipelineState(&g.pipelines["tdt_decision"]);
            for (i, b) in [
                g.outputs["logits"].buffer.clone(),
                state,
                records.clone(),
                target,
                next,
                bonus,
                bias,
            ]
            .iter()
            .enumerate()
            {
                unsafe {
                    enc.setBuffer_offset_atIndex(Some(b), 0, i);
                }
            }
            enc.dispatchThreadgroups_threadsPerThreadgroup(size(1, 1, 1), size(256, 1, 1));
            enc.endEncoding();
            cmd.commit();
            cmd.waitUntilCompleted();
            assert_eq!(cmd.status(), MTLCommandBufferStatus::Completed);
            let record =
                unsafe { std::slice::from_raw_parts(records.contents().as_ptr().cast::<u32>(), 4) };
            assert_eq!(record[0], expected);
            let sum = logits[..8193]
                .iter()
                .map(|v| f64::from(*v - 5.).exp())
                .sum::<f64>();
            let probability =
                f16::from_f32((f64::from(logits[expected as usize] - 5.).exp() / sum) as f32)
                    .to_f32();
            assert_eq!(f32::from_bits(record[3]), probability);
        }
    }
    #[test]
    fn gpu_tdt_block_preserves_ties_probability_and_ten_symbol_advance() {
        let mut logits = vec![-2.; 8198];
        logits[7358] = 4.;
        logits[7359] = 4.;
        for (i, v) in [1., 0., -1., -2., -3.].into_iter().enumerate() {
            logits[8193 + i] = v;
        }
        let plan:Plan=serde_json::from_value(serde_json::json!({"mil_sha256":"","shapes":{"logits":[8198]},"constants":{"logits":literal(logits.clone(),&[8198])},"nodes":[],"outputs":["logits"]})).unwrap();
        let g = MetalEncoder::build(plan, vec![], &[]).unwrap();
        let device = g.queue.device();
        let alloc = |n| {
            device
                .newBufferWithLength_options(n, MTLResourceOptions::StorageModeShared)
                .unwrap()
        };
        let state = alloc(32);
        let records = alloc(32 * 16);
        let target = alloc(4);
        let zero_row = alloc(8193 * 4);
        let bias_state = alloc(8);
        unsafe {
            std::ptr::write_bytes(zero_row.contents().as_ptr(), 0, 8193 * 4);
            std::ptr::write_bytes(bias_state.contents().as_ptr(), 0, 8);
            std::ptr::write_bytes(state.contents().as_ptr(), 0, 32);
            *state.contents().as_ptr().cast::<u32>().add(1) = 3;
        }
        let cmd = g.queue.commandBuffer().unwrap();
        let enc = cmd.computeCommandEncoder().unwrap();
        enc.setComputePipelineState(&g.pipelines["tdt_decision"]);
        for (i, b) in [
            g.outputs["logits"].buffer.clone(),
            state.clone(),
            records.clone(),
            target,
            zero_row.clone(),
            zero_row,
            bias_state,
        ]
        .iter()
        .enumerate()
        {
            unsafe {
                enc.setBuffer_offset_atIndex(Some(b), 0, i);
            }
        }
        for _ in 0..32 {
            enc.dispatchThreadgroups_threadsPerThreadgroup(size(1, 1, 1), size(256, 1, 1));
        }
        enc.endEncoding();
        cmd.commit();
        cmd.waitUntilCompleted();
        assert_eq!(cmd.status(), MTLCommandBufferStatus::Completed);
        let state =
            unsafe { std::slice::from_raw_parts(state.contents().as_ptr().cast::<u32>(), 8) };
        assert_eq!(state[0], 3);
        assert_eq!(state[6], 32);
        assert_eq!(state[7], 0);
        let expected = f16::from_f32(
            (1. / logits[..8193]
                .iter()
                .map(|v| f64::from(*v - 4.).exp())
                .sum::<f64>()) as f32,
        )
        .to_f32();
        let data = unsafe {
            std::slice::from_raw_parts(records.contents().as_ptr().cast::<u32>(), 32 * 4)
        };
        for (i, r) in data.as_chunks::<4>().0.iter().enumerate() {
            assert_eq!(r[0], if i < 30 { 7358 } else { 8192 });
            assert_eq!(r[1], (i / 10).min(3) as u32);
            assert_eq!(r[2], 0);
            assert_eq!(f32::from_bits(r[3]), expected);
        }
    }
    #[test]
    fn fp16_tensor_tiles_match_fixed_dot_products_including_fringe() {
        for (m, k, n) in [(5, 32, 64), (67, 64, 96), (1, 64, 35)] {
            let x = fixed(m * k);
            let w = fixed(n * k);
            let bias = fixed(n);
            let y=graph("linear",serde_json::json!({"x":"x","weight":"w","bias":"b"}),serde_json::json!({"x":literal(x.clone(),&[m,k]),"w":literal(w.clone(),&[n,k]),"b":literal(bias.clone(),&[n])}),serde_json::json!({"x":[m,k],"w":[n,k],"b":[n]}),&[m,n]).unwrap();
            let expected: Vec<_> = (0..m * n)
                .map(|i| {
                    let s = (0..k)
                        .map(|j| f64::from(x[i / n * k + j]) * f64::from(w[i % n * k + j]))
                        .sum::<f64>() as f32;
                    // Multirow graph rounds GEMM before its bias, single-row fused matvec rounds once.
                    f16::from_f32(
                        (if m == 1 { s } else { f16::from_f32(s).to_f32() }) + bias[i % n],
                    )
                    .to_f32()
                })
                .collect();
            close(&y, &expected, 0.008);
        }
    }
    #[test]
    fn fp16_norm_softmax_and_layout_match_fixed_references() {
        let x = fixed(3 * 33);
        let ones = vec![1.; 33];
        let zeros = vec![0.; 33];
        let y=graph("layer_norm",serde_json::json!({"x":"x","gamma":"g","beta":"b","axes":"ax","epsilon":"ep"}),serde_json::json!({"x":literal(x.clone(),&[3,33]),"g":literal(ones,&[33]),"b":literal(zeros,&[33]),"ax":literal(vec![-1.],&[1]),"ep":literal(vec![1e-5],&[])}),serde_json::json!({"x":[3,33],"g":[33],"b":[33],"ax":[1],"ep":[]}),&[3,33]).unwrap();
        let expected: Vec<_> = x
            .chunks(33)
            .flat_map(|r| {
                let mean = r.iter().map(|v| f64::from(*v)).sum::<f64>() / 33.;
                let var = r
                    .iter()
                    .map(|v| (f64::from(*v) - mean).powi(2))
                    .sum::<f64>()
                    / 33.;
                r.iter().map(move |v| {
                    f16::from_f32(((f64::from(*v) - mean) / (var + 1e-5).sqrt()) as f32).to_f32()
                })
            })
            .collect();
        close(&y, &expected, 0.002);
        let y = graph(
            "softmax",
            serde_json::json!({"x":"x","axis":"ax"}),
            serde_json::json!({"x":literal(x.clone(),&[3,33]),"ax":literal(vec![-1.],&[])}),
            serde_json::json!({"x":[3,33],"ax":[]}),
            &[3, 33],
        )
        .unwrap();
        let expected: Vec<_> = x
            .chunks(33)
            .flat_map(|r| {
                let sum = r.iter().map(|v| f64::from(*v).exp()).sum::<f64>();
                r.iter()
                    .map(move |v| f16::from_f32((f64::from(*v).exp() / sum) as f32).to_f32())
            })
            .collect();
        close(&y, &expected, 0.0001);
        let y = graph(
            "transpose",
            serde_json::json!({"x":"x","perm":"p"}),
            serde_json::json!({"x":literal(x.clone(),&[3,33]),"p":literal(vec![-1.,-2.],&[2])}),
            serde_json::json!({"x":[3,33],"p":[2]}),
            &[33, 3],
        )
        .unwrap();
        let expected: Vec<_> = (0..99).map(|i| x[(i % 3) * 33 + i / 3]).collect();
        close(&y, &expected, 0.);
    }
    #[test]
    fn reductions_preserve_statistics_across_all_simd_groups() {
        let (rows, cols) = (9, 1024);
        let mut x = fixed(rows * cols);
        for r in 0..rows {
            x[r * cols + (r * 113) % cols] = 16. + r as f32;
        }
        let y=graph("layer_norm",serde_json::json!({"x":"x","gamma":"g","beta":"b","axes":"ax","epsilon":"ep"}),serde_json::json!({"x":literal(x.clone(),&[rows,cols]),"g":literal(vec![1.;cols],&[cols]),"b":literal(vec![0.;cols],&[cols]),"ax":literal(vec![-1.],&[1]),"ep":literal(vec![1e-5],&[])}),serde_json::json!({"x":[rows,cols],"g":[cols],"b":[cols],"ax":[1],"ep":[]}),&[rows,cols]).unwrap();
        let expected: Vec<_> = x
            .chunks(cols)
            .flat_map(|row| {
                let mean = row.iter().map(|v| f64::from(*v)).sum::<f64>() / cols as f64;
                let var = row
                    .iter()
                    .map(|v| (f64::from(*v) - mean).powi(2))
                    .sum::<f64>()
                    / cols as f64;
                row.iter().map(move |v| {
                    f16::from_f32(((f64::from(*v) - mean) / (var + 1e-5).sqrt()) as f32).to_f32()
                })
            })
            .collect();
        close(&y, &expected, 0.016);
        let y = graph(
            "softmax",
            serde_json::json!({"x":"x","axis":"ax"}),
            serde_json::json!({"x":literal(x.clone(),&[rows,cols]),"ax":literal(vec![-1.],&[])}),
            serde_json::json!({"x":[rows,cols],"ax":[]}),
            &[rows, cols],
        )
        .unwrap();
        let expected: Vec<_> = x
            .chunks(cols)
            .flat_map(|row| {
                let sum = row.iter().map(|v| f64::from(*v).exp()).sum::<f64>();
                row.iter()
                    .map(move |v| f16::from_f32((f64::from(*v).exp() / sum) as f32).to_f32())
            })
            .collect();
        close(&y, &expected, 0.001);
    }
    #[test]
    fn fp16_batched_attention_matmul_pads_k_and_preserves_planes() {
        let (batch, m, k, n) = (2, 5, 33, 7);
        let x = fixed(batch * m * k);
        let w = fixed(batch * k * n);
        let y=graph("matmul",serde_json::json!({"x":"x","y":"w","transpose_x":"f","transpose_y":"f"}),serde_json::json!({"x":literal(x.clone(),&[1,batch,m,k]),"w":literal(w.clone(),&[1,batch,k,n]),"f":literal(vec![0.],&[])}),serde_json::json!({"x":[1,batch,m,k],"w":[1,batch,k,n],"f":[]}),&[1,batch,m,n]).unwrap();
        let expected: Vec<_> = (0..batch * m * n)
            .map(|i| {
                let b = i / (m * n);
                let row = i / n % m;
                let col = i % n;
                f16::from_f32(
                    (0..k)
                        .map(|j| {
                            f64::from(x[(b * m + row) * k + j])
                                * f64::from(w[(b * k + j) * n + col])
                        })
                        .sum::<f64>() as f32,
                )
                .to_f32()
            })
            .collect();
        close(&y, &expected, 0.004);
    }
    #[test]
    fn fp16_dense_and_depthwise_convolution_match_fixed_references() {
        for (groups, co, stride, wo) in [(1, 2, 1, 7), (3, 3, 2, 4)] {
            let ci = 3;
            let wi = 7;
            let kw = 3;
            let x = fixed(ci * wi);
            let w = fixed(co * (ci / groups) * kw);
            let bias = fixed(co);
            let y=graph("conv",serde_json::json!({"x":"x","weight":"w","bias":"b","groups":"g","strides":"s","pad":"p","dilations":"d"}),serde_json::json!({"x":literal(x.clone(),&[1,ci,wi]),"w":literal(w.clone(),&[co,ci/groups,kw]),"b":literal(bias.clone(),&[co]),"g":literal(vec![groups as f32],&[]),"s":literal(vec![stride as f32],&[1]),"p":literal(vec![1.,1.],&[2]),"d":literal(vec![1.],&[1])}),serde_json::json!({"x":[1,ci,wi],"w":[co,ci/groups,kw],"b":[co],"g":[],"s":[1],"p":[2],"d":[1]}),&[1,co,wo]).unwrap();
            let expected: Vec<_> = (0..co * wo)
                .map(|i| {
                    let oc = i / wo;
                    let t = i % wo;
                    let mut sum = f64::from(bias[oc]);
                    for c in 0..ci / groups {
                        for k in 0..kw {
                            let tt = (t * stride + k) as isize - 1;
                            if tt >= 0 && tt < wi as isize {
                                let ic = (oc / (co / groups)) * (ci / groups) + c;
                                sum += f64::from(x[ic * wi + tt as usize])
                                    * f64::from(w[(oc * (ci / groups) + c) * kw + k]);
                            }
                        }
                    }
                    f16::from_f32(sum as f32).to_f32()
                })
                .collect();
            close(&y, &expected, 0.004);
        }
    }
}
