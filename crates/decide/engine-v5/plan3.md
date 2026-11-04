# Engine climb 3: exact inference toward the hardware floor

Goal: faster inference with Metal kernels, as close to the maximum theoretical speed as possible.

Rules: the same weights at the same precision (BF16), every layer, the full prompt. No lossy switch (`D1E_STATE_TOKENS`,
`D1E_SKIP_LAYERS`, `D1E_Q8`) is used. Guard on runs/v5/final:
- the 30-row probability check against the MLX reference: same argmax, |dp| <= 0.03;
- decision agreement with the full fast path of at least 99.5% on check/agree_rows.jsonl (1,252 rows).

## The floor

**Work per decision.** Exact counts from the model at the bench rows' real length: median T = 1,852 tokens (20 rows of
1,800-2,100 tokens). There are 24 layers: 12 conv, 6 full attention, 6 sliding attention. Hidden size 2,048, FFN
7,680, 16 query heads and 4 KV heads of 128.

| part | TFLOP (fast path, pruned tail) | TFLOP (no pruning) |
|---|---|---|
| GEMMs (projections and FFN) | 4.987 | 5.428 |
| attention (QK and PV; full T^2, sliding T x 256) | 0.159 | 0.186 |
| depthwise conv | 0.001 | 0.001 |
| **total** | **5.146** | 5.615 |

Pruning is exact, so the work that must be done is 5.146 TFLOP. Compiling pads T to a multiple of 128 (1,920). That
adds 3.7% of GEMM work, which is not required work.

**Peak matmul rate on this M5 Max (40-core GPU), measured on 29 Oct.** Shapes M = 1,920 against K = 2,048 x N =
2,048 / 6,144 / 15,360 and K = 7,680 x N = 2,048; warm, 30 to 50 repetitions; GPU timestamps for MPP, wall time for
MLX.

| kernel | best TFLOPS | notes |
|---|---|---|
| (a) MLX 0.32 BF16 matmul (already the NAX path on the neural accelerators) | 61.4 | K=7,680 shape: 54.5 |
| (a) MLX 0.32 FP16 matmul | 62.5 | K=7,680 shape: 51.6 |
| (c) MPP tensor ops (`matmul2d`, Metal 4), FP16, 128x64 tile, 4 simdgroups | **64.0** | K=7,680 shape: 64.0 |
| (c) MPP tensor ops, BF16, 64x128 tile, 4 simdgroups | 63.7 | K=7,680 shape: 62.7 |

The measured peak is about 64 TFLOPS. Apple publishes no TFLOPS figure for the M5 Max GPU or its neural
accelerators. Its newsroom gives "a Neural Accelerator in each GPU core" and 614 GB/s for the 40-core GPU part
(https://www.apple.com/newsroom/2026/03/apple-debuts-m5-pro-and-m5-max-to-supercharge-the-most-demanding-pro-workflows/,
https://support.apple.com/en-us/126319). Third-party pages disagree by about 2x (33 to 70 TFLOPS FP16), so they are
not used. The floor uses the measured 64 TFLOPS.

**Memory.** The weights that are read are 2.92 GB of linear weights plus a few embedding rows. Activations, with ideal
fusion, are the residual read and written per layer: about 0.73 GB. That is about 3.65 GB at 614 GB/s: **5.9 ms**.

**Floor** = max(compute 5.146 TFLOP / 64 TFLOPS = **80.4 ms**, memory 5.9 ms) = **80.4 ms**, compute-bound. Derived
from the measured peak.

## Where the time goes (measured, fast path, trained checkpoint)

One decision, by bench stage (median of 20 rows):
- render and tokenize: 0.9 ms
- graph build and launch: 0.1 ms
- trunk evaluation (GPU): about 111 ms
- readout: 0.2 ms

The host side is about 1.2 ms of about 112 ms.

GPU parts timed alone at T = 1,920 with MLX: the GEMMs take about 92 ms (about 56 TFLOPS on average, against 80.8 ms at
64 TFLOPS for that padded work). The K = 7,680 down-projection runs at 53 TFLOPS. The SwiGLU product and the residual
adds and norms take about 7 ms. Sliding attention computes all T^2 scores under a mask: 4.1 ms for 6 layers. Full
attention takes 2.9 ms for 5 layers.

## Attempts, in order of measured gain per effort

1. Pad to a smaller multiple than 128: less padded GEMM work.
2. Block-local sliding attention (each 256-query block against its own and the previous key block, the same mask):
   the sliding layers' attention work falls by about 70%.
3. An MPP tensor-op GEMM for the shapes where it beats MLX (the K = 7,680 down-projection), as an option with a
   load-time self-check and a fallback to MLX's matmul.
4. SwiGLU fused into the w1/w3 GEMM epilogue (saves the 2 x T x 7,680 write and read).
5. Residual add fused with RMSNorm.

Each attempt is timed in interleaved rounds against the current fast path in the same session, guarded, and logged
in ledger.jsonl.

## Result (29 Oct; 5 interleaved rounds, the Mac in use, trained checkpoint)

| attempt | before (ms) | after (ms) | guard | kept |
|---|---|---|---|---|
| pad to 32 instead of 128 | — | within noise alone | padding only | in the stack |
| block-local sliding attention (`D1E_WINDOW`) + pad 32 | 115.0 | 112.1 (median of 6) | 30/30, dp 0.009; 100% agreement | yes |
| MPP tensor-op GEMM for every linear (`D1E_TENSOR_MM=1`) | 115.9 | 110.3 alone; 105.0 with the above | 30/30, dp 0.007 | yes |
| SwiGLU in the w1/w3 GEMM epilogue (`D1E_SWIGLU`) | 107.0 | 103.8 | 30/30, dp 0.006 | yes |
| residual add fused with both RMSNorms (`D1E_ADDNORM`) | 104.2 | 102.6 | identical alone; stack 30/30, 99.92% of 1,252 | yes |
| per-shape GEMM tiles | — | no gain | — | no |

Stack: `D1E_WINDOW=1 D1E_PAD_MULTIPLE=32 D1E_TENSOR_MM=1 D1E_SWIGLU=1 D1E_ADDNORM=1` on the default fast path. It runs
at 102.6 ms against the fast path's 117.3 ms in the same rounds (-12.5%). That is 78% of the 80.4 ms floor (80% in the
first, cool round: 100.2 ms).

What is left, derived: the GEMMs already run at about 61 TFLOPS of the 64 measured. About 82 ms of the remaining time
is GEMM work at that rate. The rest is about 20 ms: attention, the q/k norms and RoPE, the conv block, the norms, and
the gaps between about 400 kernel launches. Each of those is 1 to 3 ms. Sustained runs also lose about 3% to heat
against the first round.

The default is unchanged. Every new kernel checks itself at load and turns off when it fails. Forced failures of the
tensor GEMM and add+norm self-checks give the fast path's answers. On a GPU without neural accelerators (M3, M4) the
tensor GEMM and its SwiGLU epilogue stay off; block-local sliding attention and add+norm are plain Metal and MLX
and run anywhere.

## Climb 3b (29 Oct, later)

The full stack is now the engine default, and `D1E_FAST=0` and `D1E_FALLBACK=1` still give plain MLX.
- **Launch gaps (Metal System Trace, 20 decisions):** idle time inside a decision is 0.85 ms and the host gap between
  decisions is 1.45 ms, so gaps are not the problem.
- **`D1E_QKPREP` (default on):** one kernel does q/k RMSNorm, RoPE and the head-major layout straight from the qkvg
  GEMM output. Median 102.3 -> 99.2 ms, answers unchanged.
- **Host:** the question suffix is cached and the prompt is tokenized with encode_fast; ids are still 268/268. Gain
  within noise.

Final, interleaved: the new default runs at 102.5 ms against 118.7 ms for the old fast path and 104.6 ms for the
previous stack. These rounds were on a warm Mac; the first, cool round was 99.8 ms. That is 78% of the 80.4 ms floor
(81% cool). Guard: 30/30, max dp 0.0056; 99.92% agreement on the 1,252 rows.

The remainder is about 82 ms of GEMMs at about 61 of 64 TFLOPS and about 9 ms of attention, the conv block and the
norms, plus heat: sustained rounds run about 3% slower than the first.
