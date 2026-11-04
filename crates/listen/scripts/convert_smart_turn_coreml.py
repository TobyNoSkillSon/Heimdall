#!/usr/bin/env python3
"""Agent-only: convert User's Smart Turn checkpoint to a Core ML ML Program.

Rebuilds the Whisper-tiny encoder + attention-pooling head from the state dict
(no transformers import), checks it against the shipped ONNX on fixed feature
tensors, then converts with coremltools. Usage:
  convert_smart_turn_coreml.py <owner-turn.pt> <owner-turn.onnx> <features.bin>... --out <dir>
"""
import argparse, json, math, pathlib, sys
import numpy as np
import torch
import torch.nn.functional as F

a = argparse.ArgumentParser()
a.add_argument('checkpoint')
a.add_argument('onnx')
a.add_argument('features', nargs='+')
a.add_argument('--out', type=pathlib.Path, required=True)
a.add_argument('--layout', choices=['bsc', 'bc1s'], default='bc1s')
a.add_argument('--ln-scale', type=float, default=8.0, help='LayerNorm input divisor (fp16 range)')
args = a.parse_args()
sd = torch.load(args.checkpoint, map_location='cpu', weights_only=True)
D, H, L = 384, 6, 4


class Turn(torch.nn.Module):
    """Whisper-tiny encoder (pre-LN, exact GELU) + Smart Turn v3 head."""

    def __init__(self, layout):
        super().__init__()
        self.layout = layout
        self.p = torch.nn.ParameterDict({k.replace('.', '__'): torch.nn.Parameter(v, requires_grad=False) for k, v in sd.items()})

    def w(self, k):
        return self.p[k.replace('.', '__')]

    def ln(self, x, k, dim):
        # LayerNorm over channel dim; written explicitly so bc1s keeps channels on axis 1.
        # The final layers' residual stream reaches ~180, so squared deviations approach
        # fp16's 65504 limit. LayerNorm is scale-invariant: normalise x/s with eps/s^2.
        s = args.ln_scale
        x = x * (1.0 / s)
        mean = x.mean(dim=dim, keepdim=True)
        var = ((x - mean) ** 2).mean(dim=dim, keepdim=True)
        g, b = self.w(k + '.weight'), self.w(k + '.bias')
        shape = [1] * x.dim()
        shape[dim] = -1
        return (x - mean) * torch.rsqrt(var + 1e-5 / (s * s)) * g.view(shape) + b.view(shape)

    def lin(self, x, k, bias=True):
        w = self.w(k + '.weight')
        b = self.w(k + '.bias') if bias else None
        if self.layout == 'bc1s':  # x: [B, C, 1, S]; 1x1 conv is the ANE-native matmul
            return F.conv2d(x, w[:, :, None, None], b)
        return F.linear(x, w, b)

    def forward(self, features):  # [1, 80, 800]
        x = F.gelu(F.conv1d(features, self.w('encoder.conv1.weight'), self.w('encoder.conv1.bias'), padding=1))
        x = F.gelu(F.conv1d(x, self.w('encoder.conv2.weight'), self.w('encoder.conv2.bias'), stride=2, padding=1))
        pos = self.w('encoder.embed_positions.weight')  # [400, 384]
        if self.layout == 'bc1s':
            x = (x + pos.t()[None]).unsqueeze(2)  # [1, 384, 1, 400]
            cdim = 1
        else:
            x = x.permute(0, 2, 1) + pos[None]  # [1, 400, 384]
            cdim = -1
        hd = D // H
        for i in range(L):
            k = f'encoder.layers.{i}'
            h = self.ln(x, k + '.self_attn_layer_norm', cdim)
            q = self.lin(h, k + '.self_attn.q_proj') * (hd ** -0.5)
            kk = self.lin(h, k + '.self_attn.k_proj', bias=False)
            v = self.lin(h, k + '.self_attn.v_proj')
            if self.layout == 'bc1s':
                outs = []
                for j in range(H):
                    qs, ks, vs = (t[:, j * hd:(j + 1) * hd] for t in (q, kk, v))  # [1, hd, 1, S]
                    s = torch.einsum('bchq,bchk->bkhq', qs, ks)  # [1, Sk, 1, Sq]
                    s = s.softmax(dim=1)
                    outs.append(torch.einsum('bkhq,bchk->bchq', s, vs))
                att = torch.cat(outs, dim=1)
            else:
                B, S, _ = q.shape
                q, kk, v = (t.view(B, S, H, hd).transpose(1, 2) for t in (q, kk, v))
                att = (q @ kk.transpose(-1, -2)).softmax(-1) @ v
                att = att.transpose(1, 2).reshape(B, S, D)
            x = x + self.lin(att, k + '.self_attn.out_proj')
            h = self.ln(x, k + '.final_layer_norm', cdim)
            x = x + self.lin(F.gelu(self.lin(h, k + '.fc1')), k + '.fc2')
        x = self.ln(x, 'encoder.layer_norm', cdim)
        if self.layout == 'bc1s':
            x = x.squeeze(2).transpose(1, 2)  # [1, S, C] for the small head
        a = F.linear(torch.tanh(F.linear(x, self.w('pool_attention.0.weight'), self.w('pool_attention.0.bias'))),
                     self.w('pool_attention.2.weight'), self.w('pool_attention.2.bias'))
        a = a.softmax(dim=1)
        pooled = (x * a).sum(dim=1)
        h = F.linear(pooled, self.w('classifier.0.weight'), self.w('classifier.0.bias'))
        h = F.layer_norm(h, (256,), self.w('classifier.1.weight'), self.w('classifier.1.bias'))
        h = F.gelu(F.linear(F.gelu(h), self.w('classifier.4.weight'), self.w('classifier.4.bias')))
        return torch.sigmoid(F.linear(h, self.w('classifier.6.weight'), self.w('classifier.6.bias')))


def load(p):
    return np.fromfile(p, dtype='<f4').reshape(1, 80, 800)


import onnxruntime as ort
sess = ort.InferenceSession(args.onnx, providers=['CPUExecutionProvider'])
model = Turn(args.layout).eval()
feats = [load(p) for p in args.features]
report = dict(layout=args.layout, torch_vs_onnx=[])
with torch.no_grad():
    for f in feats:
        ref = float(sess.run(None, {'input_features': f})[0].reshape(-1)[0])
        got = float(model(torch.from_numpy(f)).reshape(-1)[0])
        report['torch_vs_onnx'].append(dict(onnx=ref, torch=got, diff=abs(ref - got)))
assert max(r['diff'] for r in report['torch_vs_onnx']) < 1e-4, report
import coremltools as ct
traced = torch.jit.trace(model, torch.from_numpy(feats[0]))
args.out.mkdir(parents=True, exist_ok=True)
for precision in ['fp16']:
    ml = ct.convert(traced, inputs=[ct.TensorType(name='input_features', shape=(1, 80, 800), dtype=np.float32)],
                    outputs=[ct.TensorType(name='probability', dtype=np.float32)],
                    convert_to='mlprogram', minimum_deployment_target=ct.target.macOS15,
                    compute_precision=ct.precision.FLOAT16 if precision == 'fp16' else ct.precision.FLOAT32)
    path = args.out / f'owner-turn-{args.layout}-s{args.ln_scale:g}-{precision}.mlpackage'
    ml.save(str(path))
    report[precision] = str(path)
print(json.dumps(report, indent=1))
