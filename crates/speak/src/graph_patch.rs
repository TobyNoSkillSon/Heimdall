//! Lossless protobuf wire edit: preserve all unowned ONNX fields verbatim.
use anyhow::{bail, ensure, Result};
use std::collections::HashMap;
pub const DOMAIN: &str = "heimdall.metal";
struct Field<'a> {
    tag: u64,
    wire: u64,
    payload: &'a [u8],
    raw: &'a [u8],
}
fn varint(b: &[u8], p: &mut usize) -> Result<u64> {
    let mut v = 0;
    for shift in (0..70).step_by(7) {
        ensure!(*p < b.len(), "truncated protobuf");
        let x = b[*p];
        *p += 1;
        ensure!(shift < 63 || x <= 1, "protobuf overflow");
        v |= ((x & 127) as u64) << shift;
        if x < 128 {
            return Ok(v);
        }
    }
    bail!("invalid varint")
}
fn fields(b: &[u8]) -> Result<Vec<Field<'_>>> {
    let mut p = 0;
    let mut out = vec![];
    while p < b.len() {
        let start = p;
        let key = varint(b, &mut p)?;
        let wire = key & 7;
        let begin;
        match wire {
            0 => {
                begin = p;
                varint(b, &mut p)?;
            }
            1 => {
                begin = p;
                p += 8;
            }
            2 => {
                let n = usize::try_from(varint(b, &mut p)?)?;
                begin = p;
                p = p
                    .checked_add(n)
                    .ok_or_else(|| anyhow::anyhow!("length overflow"))?;
            }
            5 => {
                begin = p;
                p += 4;
            }
            _ => bail!("unsupported protobuf wire {wire}"),
        }
        ensure!(p <= b.len(), "truncated field");
        out.push(Field {
            tag: key >> 3,
            wire,
            payload: &b[begin..p],
            raw: &b[start..p],
        });
    }
    Ok(out)
}
fn putvar(mut v: u64, out: &mut Vec<u8>) {
    while v >= 128 {
        out.push((v as u8) | 128);
        v >>= 7;
    }
    out.push(v as u8);
}
fn putbytes(tag: u64, b: &[u8], out: &mut Vec<u8>) {
    putvar(tag * 8 + 2, out);
    putvar(b.len() as u64, out);
    out.extend_from_slice(b);
}
fn text<'a>(fs: &'a [Field<'a>], tag: u64) -> &'a [u8] {
    fs.iter().find(|f| f.tag == tag).map_or(&[], |f| f.payload)
}
fn dims(b: &[u8]) -> Result<Vec<u64>> {
    let mut ds = vec![];
    for f in fields(b)? {
        if f.tag == 1 {
            let mut p = 0;
            while p < f.payload.len() {
                ds.push(varint(f.payload, &mut p)?);
                if f.wire == 0 {
                    break;
                }
            }
        }
    }
    Ok(ds)
}
fn attr_ints(b: &[u8]) -> Result<Vec<u64>> {
    let mut r = vec![];
    for f in fields(b)? {
        if f.tag == 8 || f.tag == 3 {
            let mut p = 0;
            while p < f.payload.len() {
                r.push(varint(f.payload, &mut p)?);
                if f.wire == 0 {
                    break;
                }
            }
        }
    }
    Ok(r)
}
pub fn patch(b: &[u8]) -> Result<(Vec<u8>, usize)> {
    let mut output = vec![];
    let mut count = 0;
    for f in fields(b)? {
        if f.tag != 7 {
            output.extend_from_slice(f.raw);
            continue;
        }
        let graph = fields(f.payload)?;
        let mut weights = HashMap::new();
        for x in &graph {
            if x.tag == 5 {
                let t = fields(x.payload)?;
                weights.insert(text(&t, 8).to_vec(), dims(x.payload)?);
            }
        }
        let mut g = vec![];
        for x in graph {
            if x.tag != 1 {
                g.extend_from_slice(x.raw);
                continue;
            }
            let node = fields(x.payload)?;
            let inputs: Vec<_> = node
                .iter()
                .filter(|v| v.tag == 1)
                .map(|v| v.payload)
                .collect();
            let mut eligible = text(&node, 4) == b"Conv"
                && text(&node, 7).is_empty()
                && (2..=3).contains(&inputs.len());
            let shape = inputs.get(1).and_then(|v| weights.get(*v));
            eligible &= shape.is_some_and(|s| {
                s.len() == 3
                    && s[2] == 1
                    && s[0] >= 256
                    && s[1] >= 256
                    && s[0] % 32 == 0
                    && s[1] % 32 == 0
            });
            if inputs.len() == 3 {
                eligible &= weights
                    .get(inputs[2])
                    .is_some_and(|bias| shape.is_some_and(|s| bias == &[s[0]]));
            }
            for a in node.iter().filter(|v| v.tag == 5) {
                let af = fields(a.payload)?;
                let name = text(&af, 1);
                let vals = attr_ints(a.payload)?;
                eligible &= match name {
                    b"group" => vals == [1],
                    b"strides" | b"dilations" | b"kernel_shape" => vals == [1],
                    b"pads" => vals == [0, 0],
                    b"auto_pad" => text(&af, 4) == b"NOTSET",
                    _ => false,
                };
            }
            if eligible {
                let mut n = vec![];
                for v in node {
                    if v.tag != 4 && v.tag != 7 {
                        n.extend_from_slice(v.raw);
                    }
                }
                putbytes(4, b"Pointwise", &mut n);
                putbytes(7, DOMAIN.as_bytes(), &mut n);
                putbytes(1, &n, &mut g);
                count += 1;
            } else {
                g.extend_from_slice(x.raw);
            }
        }
        putbytes(7, &g, &mut output);
    }
    if count > 0 {
        let mut op = vec![];
        putbytes(1, DOMAIN.as_bytes(), &mut op);
        putvar(16, &mut op);
        putvar(1, &mut op);
        putbytes(8, &op, &mut output);
    }
    Ok((output, count))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture(k: u64, group: u64) -> Vec<u8> {
        let mut w = vec![];
        for d in [256, k, 1] {
            putvar(8, &mut w);
            putvar(d, &mut w);
        }
        putbytes(8, b"w", &mut w);
        let mut node = vec![];
        putbytes(1, b"x", &mut node);
        putbytes(1, b"w", &mut node);
        putbytes(2, b"y", &mut node);
        putbytes(4, b"Conv", &mut node);
        let mut attr = vec![];
        putbytes(1, b"group", &mut attr);
        putvar(24, &mut attr);
        putvar(group, &mut attr);
        putbytes(5, &attr, &mut node);
        let mut graph = vec![];
        putbytes(1, &node, &mut graph);
        putbytes(5, &w, &mut graph);
        putbytes(42, b"unknown graph data", &mut graph);
        let mut m = vec![];
        putbytes(7, &graph, &mut m);
        putbytes(99, b"unknown model data", &mut m);
        m
    }
    #[test]
    fn patch_is_lossless_and_rejects_unsupported_shapes() {
        for (k, g) in [(256, 2), (257, 1), (64, 1)] {
            let m = fixture(k, g);
            let (out, n) = patch(&m).unwrap();
            assert_eq!(n, 0);
            assert_eq!(m, out);
        }
        let m = fixture(256, 1);
        let (out, n) = patch(&m).unwrap();
        assert_eq!(n, 1);
        assert!(out.windows(18).any(|w| w == b"unknown graph data"));
        assert!(out.windows(18).any(|w| w == b"unknown model data"));
        let (again, n) = patch(&out).unwrap();
        assert_eq!(n, 0);
        assert_eq!(again, out);
        assert!(patch(&[58, 255]).is_err());
        assert!(patch(&[58, 100, 0]).is_err());
    }
}
