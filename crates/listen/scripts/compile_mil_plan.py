#!/usr/bin/env python3
"""Compile the pinned Ultra MIL into data-only Rust Metal plans (no Python runtime).

Weights stay in Heimdall's original model files. Unsupported syntax fails closed.
Run only when deliberately updating the pinned model, then review the plan diff.
"""
import argparse
import hashlib
import json
import math
import pathlib
import re


def fields(s):
    out, start, depth, quoted = [], 0, 0, False
    for i, c in enumerate(s):
        if c == '"' and (i == 0 or s[i - 1] != '\\'):
            quoted = not quoted
        if not quoted:
            if c in '([<': depth += 1
            if c in ')]>': depth -= 1
            if c == ',' and depth == 0:
                out.append(s[start:i].strip()); start = i + 1
    out.append(s[start:].strip())
    return out


def literal(s):
    m = re.fullmatch(r'tensor<(\w+), \[([^]]*)\]>\((.*)\)', s)
    if not m:
        raise ValueError('Unsupported literal: ' + s[:150])
    dtype, dims, body = m.groups()
    shape = [int(x) for x in fields(dims)] if dims else []
    if body.startswith('BLOBFILE('):
        return dict(dtype=dtype, shape=shape, offset=int(re.search(r'offset = tensor<uint64, \[\]>\((\d+)\)', body)[1]))
    def scalar(x):
        if x.startswith('"'): return json.loads(x)
        if x in ('true', 'false'): return x == 'true'
        if '0x' in x.lower(): return float.fromhex(x)
        return float(x) if dtype.startswith('fp') else int(x)
    def flatten(x):
        return sum((flatten(v) for v in fields(x[1:-1])), []) if x.startswith('[') else [scalar(x)]
    values = flatten(body)
    assert len(values) == math.prod(shape)
    return dict(dtype=dtype, shape=shape, values=values)


def compile_plan(path):
    text = path.read_text()
    constants, nodes, shapes = {}, [], {}
    inputs = re.search(r'func main<[^>]+>\((.*)\) \{', text)[1]
    for dtype, dims, name in re.findall(r'tensor<(\w+), \[([^]]*)\]> (\w+)', inputs):
        shapes[name] = [int(x) for x in fields(dims)]
    for line in text.splitlines():
        if not line.lstrip().startswith('tensor<'): continue
        left, rest = line.strip().split(' = ', 1)
        m = re.fullmatch(r'(\w+)\((.*?)\)\[(.*)\];', rest)
        if not m: raise ValueError(line[:150])
        op, args, attrs = m.groups()
        outs = [dict(name=name, shape=[int(x) for x in fields(dims)] if dims else [], dtype=dtype)
                for dtype, dims, name in re.findall(r'tensor<(\w+), \[([^]]*)\]> (\w+)', left)]
        for out in outs: shapes[out['name']] = out['shape']
        attrs = dict(x.split(' = ', 1) for x in fields(attrs))
        if op == 'const':
            constants[outs[0]['name']] = literal(attrs['val']); continue
        if op == 'constexpr_affine_dequantize':
            constants[outs[0]['name']] = dict(dtype='fp16', shape=outs[0]['shape'],
                dequant={k: literal(attrs[k]) for k in ['quantized_data', 'scale', 'zero_point']},
                axis=literal(attrs['axis'])['values'][0]); continue
        args = dict(x.split(' = ', 1) for x in fields(args)) if args else {}
        nodes.append(dict(op=op, args=args, outputs=outs))
    outputs = fields(re.search(r'} -> \(([^)]+)\)', text)[1])
    return dict(mil_sha256=hashlib.sha256(path.read_bytes()).hexdigest(), shapes=shapes,
                constants=constants, nodes=nodes, outputs=outputs)


def main():
    p = argparse.ArgumentParser(); p.add_argument('models', type=pathlib.Path)
    p.add_argument('out', type=pathlib.Path); a = p.parse_args(); a.out.mkdir(parents=True, exist_ok=True)
    for name in ['Encoder', 'Decoder', 'JointDecisionv3']:
        plan = compile_plan(a.models / (name + '.mlmodelc') / 'model.mil')
        (a.out / (name + '.json')).write_text(json.dumps(plan, separators=(',', ':')) + '\n')
        print(name, len(plan['nodes']), 'nodes', len(plan['constants']), 'constants')


if __name__ == '__main__': main()
