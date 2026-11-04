//! Bounded protobuf wire edit of the hash-verified ONNX model. Preserves all
//! unknown fields and original tensors; no Python/export or sidecar at runtime.
use anyhow::{Result, bail, ensure};
use std::collections::{HashMap, HashSet};
struct Field<'a> {
    key: u64,
    raw: &'a [u8],
    data: &'a [u8],
}
fn varint(bytes: &[u8], i: &mut usize) -> Result<u64> {
    let mut n = 0;
    for shift in (0..70).step_by(7) {
        let b = *bytes
            .get(*i)
            .ok_or_else(|| anyhow::anyhow!("truncated protobuf"))?;
        *i += 1;
        ensure!(shift < 63 || b < 2, "protobuf integer overflow");
        n |= ((b & 127) as u64) << shift;
        if b & 128 == 0 {
            return Ok(n);
        }
    }
    bail!("protobuf integer overflow")
}
fn fields(bytes: &[u8]) -> Result<Vec<Field<'_>>> {
    let mut i = 0;
    let mut out = Vec::new();
    while i < bytes.len() {
        let start = i;
        let key = varint(bytes, &mut i)?;
        ensure!(key >> 3 > 0, "invalid protobuf field");
        let data = match key & 7 {
            0 => {
                let begin = i;
                varint(bytes, &mut i)?;
                &bytes[begin..i]
            }
            1 | 5 => {
                let n = if key & 7 == 1 { 8 } else { 4 };
                let begin = i;
                i = i
                    .checked_add(n)
                    .ok_or_else(|| anyhow::anyhow!("protobuf overflow"))?;
                bytes
                    .get(begin..i)
                    .ok_or_else(|| anyhow::anyhow!("truncated protobuf"))?
            }
            2 => {
                let n = usize::try_from(varint(bytes, &mut i)?)?;
                let begin = i;
                i = i
                    .checked_add(n)
                    .ok_or_else(|| anyhow::anyhow!("protobuf overflow"))?;
                bytes
                    .get(begin..i)
                    .ok_or_else(|| anyhow::anyhow!("truncated protobuf"))?
            }
            _ => bail!("unsupported protobuf wire type"),
        };
        out.push(Field {
            key,
            raw: &bytes[start..i],
            data,
        });
    }
    Ok(out)
}
fn put(n: u64, out: &mut Vec<u8>) {
    let mut n = n;
    while n >= 128 {
        out.push(n as u8 | 128);
        n >>= 7;
    }
    out.push(n as u8);
}
fn field(tag: u64, data: &[u8], out: &mut Vec<u8>) {
    put(tag * 8 + 2, out);
    put(data.len() as u64, out);
    out.extend_from_slice(data);
}
pub fn rewrite(bytes: &[u8]) -> Result<(Vec<u8>, usize)> {
    let mut out = Vec::with_capacity(bytes.len() + 8192);
    let mut count = 0;
    for f in fields(bytes)? {
        if f.key != 7 * 8 + 2 {
            out.extend_from_slice(f.raw);
            continue;
        }
        let mut graph = Vec::with_capacity(f.data.len());
        let nodes = fields(f.data)?;
        let info: Vec<_> = nodes
            .iter()
            .map(|node| -> Result<_> {
                if node.key != 10 {
                    return Ok((Vec::new(), Vec::new(), Vec::new(), Vec::new()));
                }
                let fs = fields(node.data)?;
                Ok((
                    fs.iter()
                        .filter(|f| f.key == 10)
                        .map(|f| f.data.to_vec())
                        .collect::<Vec<_>>(),
                    fs.iter()
                        .find(|f| f.key == 18)
                        .map(|f| f.data.to_vec())
                        .unwrap_or_default(),
                    fs.iter()
                        .find(|f| f.key == 26)
                        .map(|f| f.data.to_vec())
                        .unwrap_or_default(),
                    fs.iter()
                        .find(|f| f.key == 34)
                        .map(|f| f.data.to_vec())
                        .unwrap_or_default(),
                ))
            })
            .collect::<Result<_>>()?;
        let mut consumers: HashMap<Vec<u8>, Vec<usize>> = HashMap::new();
        for (i, (inputs, _, _, _)) in info.iter().enumerate() {
            for input in inputs {
                consumers.entry(input.clone()).or_default().push(i);
            }
        }
        let mut removed = HashSet::new();
        let mut blocks = HashMap::new();
        for (i, (inputs, _, name, op)) in info.iter().enumerate() {
            if op != b"Conv"
                || !name.starts_with(b"/model/backbone/")
                || !name.ends_with(b"/conv_block/conv1/Conv")
            {
                continue;
            }
            let mut chain = vec![i];
            for expected in [
                b"Conv".as_slice(),
                b"Relu",
                b"BatchNormalization",
                b"Conv",
                b"Conv",
            ] {
                let (_, output, _, _) = &info[*chain.last().unwrap()];
                let next = consumers
                    .get(output)
                    .filter(|v| v.len() == 1)
                    .and_then(|v| v.first())
                    .copied()
                    .ok_or_else(|| anyhow::anyhow!("unexpected ConvBlock branch"))?;
                ensure!(
                    info[next].3 == expected && info[next].0.first() == Some(output),
                    "unexpected ConvBlock operation"
                );
                chain.push(next);
            }
            let c0 = &info[chain[0]].0;
            let c1 = &info[chain[1]].0;
            let bn = &info[chain[3]].0;
            let c2 = &info[chain[4]].0;
            let c3 = &info[chain[5]].0;
            ensure!(
                inputs.len() == 2
                    && c1.len() == 3
                    && bn.len() == 5
                    && c2.len() == 2
                    && c3.len() == 3,
                "unexpected ConvBlock parameters"
            );
            let mut block = Vec::new();
            for input in [
                c0[0].as_slice(),
                &c0[1],
                &c1[1],
                &c1[2],
                &bn[1],
                &bn[2],
                &bn[3],
                &bn[4],
                &c2[1],
                &c3[1],
                &c3[2],
            ] {
                field(1, input, &mut block)
            }
            field(2, &info[chain[5]].1, &mut block);
            field(3, name, &mut block);
            field(4, b"ConvBlock", &mut block);
            field(7, b"dev.heimdall.fp16", &mut block);
            let end = chain[5];
            removed.extend(chain);
            blocks.insert(end, block);
            count += 4;
        }
        ensure!(blocks.len() == 24, "unexpected ConvBlock count");
        for (i, node) in nodes.into_iter().enumerate() {
            if let Some(block) = blocks.get(&i) {
                field(1, block, &mut graph);
                continue;
            }
            if removed.contains(&i) {
                continue;
            }
            if node.key != 10 {
                graph.extend_from_slice(node.raw);
                continue;
            }
            let fs = fields(node.data)?;
            let name = fs
                .iter()
                .find(|f| f.key == 26)
                .map(|f| f.data)
                .unwrap_or_default();
            let conv = fs.iter().any(|f| f.key == 34 && f.data == b"Conv")
                && name.starts_with(b"/model/backbone/");
            if !conv {
                graph.extend_from_slice(node.raw);
                continue;
            }
            let mut nn = Vec::new();
            for a in fs {
                if a.key != 34 && a.key != 58 {
                    nn.extend_from_slice(a.raw)
                }
            }
            field(4, b"Conv", &mut nn);
            field(7, b"dev.heimdall.fp16", &mut nn);
            field(1, &nn, &mut graph);
            count += 1;
        }
        field(7, &graph, &mut out);
    }
    ensure!(
        count == 169,
        "unexpected ReDimNet graph ({count} backbone convolutions)"
    );
    let mut opset = Vec::new();
    field(1, b"dev.heimdall.fp16", &mut opset);
    put(16, &mut opset);
    put(1, &mut opset);
    field(8, &opset, &mut out);
    Ok((out, count))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_truncated_and_overflowing_wire_data() {
        for b in [&[10, 255][..], &[0][..], &[10, 3, 1][..], &[255; 12][..]] {
            assert!(fields(b).is_err());
        }
    }
}
