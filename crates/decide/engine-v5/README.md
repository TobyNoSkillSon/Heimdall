# d1v5-engine

Inference engine for the Decider V5 (a LiquidAI d1-2B fine-tune), in Rust on MLX (`mlx-rs` 0.32, MLX v0.32.2).
A text state and one question in; each option's probability out, from one forward pass read at the answer slot.
The goal and guards are frozen in `plan.md`. This is version 1: the plain-MLX path, which stays as the permanent
fallback once faster paths are added. Nothing here has been timed; speed work is the next step (plan.md).

## In Heimdall

Copied into Heimdall on 29 Oct from `lab/decider-v5/engine` of the decider-mlx-trainer branch (code only: the guard
data in `check/` holds transcripts and stays there). It is not a workspace member; the app loads the dylib.

- Build and place: `scripts/build_decider_v5.sh` from the repository root. It runs `build/fetch-deps.sh` when
  `target/deps-src` is missing, builds at nice 19 with two jobs, offline, and copies `libd1v5_engine.dylib` and
  `mlx.metallib` to `models/decide/runtime-v5/` (git-ignored). `--lock` rewrites
  `crates/decide/scripts/runtime-v5.lock.json`, which `scripts/bundle_models.py` checks.
- Sources outside git: MLX v0.32.2 and its C++ dependencies (mlx, fmt, json, metal_cpp, gguflib, about 110 MB) live in
  `target/deps-src`. `build/fetch-deps.sh` copies them (APFS clones) from any local mlx-sys build under
  `Projects/*/target/*/*/build/mlx-sys-*/out/build/_deps` tagged v0.32.2, else clones the pinned tags (metal_cpp has
  no clone fallback: it must come from a local MLX build). `.cargo/config.toml` points mlx-sys's cmake at them
  (`build/offline.cmake`). The Rust crates come from the local cargo registry (`--offline`, versions in Cargo.lock).
- Change made here: `heimdall_d1_tokens` counts the text as the prompt carries it (`Prompt::plain`), so Heimdall's
  window fit no longer undercounts special-token strings such as `<|im_end|>` in a transcript (the
  production send test's 1,500-repeat case went over 2,000 tokens before).
- The dylib embeds its build folder's paths: a rebuild in another checkout has another SHA-256, so rebuild with
  `--lock` and commit the lock, or reuse the placed files.

## Build

```
cd engine
CARGO_BUILD_JOBS=2 CMAKE_BUILD_PARALLEL_LEVEL=2 nice -n 19 ~/.cargo/bin/cargo build --release --offline
```

- Standalone crate (its own empty `[workspace]`), own `target/`.
- `.cargo/config.toml` points mlx-sys's cmake at local copies of MLX v0.32.2 and its dependencies in
  `target/deps-src` (`build/offline.cmake`), so the build fetches nothing. If `target/` is cleaned, run
  `build/fetch-deps.sh` first (copies from a local MLX build when one exists, else clones the pinned tags).
- `mlx.metallib` is written to `target/mlx-metal/` (not `~/.mlx`), and the built libraries look for it there.
  Shipping the dylib elsewhere means placing `mlx.metallib` next to it (MLX looks beside the binary first).
- First build from scratch: 4 min 33 s at nice 19 with two jobs (29 Oct, while training ran). Rebuilds of the
  crate alone: about 6 s.

Outputs in `target/release/`: `libd1v5_engine.dylib` (C ABI), `libd1v5_engine.rlib`, `check`, `bench`, `abbench`, `memcheck`.

## API

Rust:

```rust
let engine = d1v5_engine::Engine::load("/path/to/checkpoint")?;   // config.json, tokenizer files,
                                                                    // model.safetensors, calibration.json
let probs: Vec<(String, f32)> = engine.decide(state, question_json)?;  // option order of the question
```

`question_json` is the API's question: `{"type": "choice", "instructions": "...", "criteria": {"name": "description"}}`
(single choice) or `{"type": "noul", "instructions": "...", "criteria": {"true": "...", "false": "..."}}` (optional
criteria; reported under `true` / `false`). Other kinds (score, multiple choice, reads, boxes) are refused.
Probabilities are `softmax(scores / T)` with `T` from calibration.json (`temperatures[bucket]`, else `default`).
Lower-level: `Engine::encode` (ids, slot, option token groups) and `Engine::scores` (raw scores, temperature 1).

C ABI (`src/ffi.rs`), the shape of Heimdall's decider dylib (`crates/decide/src/d1.rs`):

| symbol | notes |
|---|---|
| `heimdall_d1_create_text_backend(model, projector, threads, gpu, &err)` | `model` is the checkpoint folder; projector and threads ignored; `gpu=false` runs on MLX's CPU device |
| `heimdall_d1_decide(handle, json, timeout_ms, &err)` | JSON in, JSON out (below); `timeout_ms` > 0 checked before each question's forward, a started forward is not cut short |
| `heimdall_d1_decide_interruptible(handle, json, timeout_ms, cb, ctx, &err)` | `cb(ctx)` polled before each question's forward |
| `heimdall_d1_tokens(handle, text)` | token count of the text as the prompt carries it (special-token strings broken up), no special tokens added |
| `heimdall_d1_free(str)`, `heimdall_d1_destroy(handle)` | destroy also clears MLX's buffer cache |

Memory: the engine caps MLX's buffer cache at 64 MB when it loads, clears it after the load's weight transforms and
after every decide (`src/cache.rs`). Uncapped, MLX kept the transforms' freed tensors (2.0 GB) and every decision's dead
activations: 11.9 GB of GPU memory (IOAccelerator) after 30 decisions for 3.5 GB of weights; capped, 3.5 GB, outputs
bit-identical (30 Oct, lab/memory-cache/README.md). `D1E_CACHE_LIMIT_MB` (MB, or `off` for MLX's own) is the A/B
switch; `memcheck burst|latency` measures it in its own process.
| `heimdall_d1_version()` | `d1v5-engine@0.1.0;mlx-rs-0.32;text-only;json-v1;affine-q8q4-v1` (the last marker: loads MLX affine 8/4-bit checkpoints) |
| `heimdall_d1_recreate_device()` | always true |
| `heimdall_d1_test_fail_next(handle)` | the next decide fails with `HEIMDALL_GPU_FAULT` |

Every entry point catches panics; one process-wide lock serialises all MLX work; MLX errors that name Metal or the
GPU carry the `HEIMDALL_GPU_FAULT` marker. Questions are checked as Python's `check_question` checks them (a type is
required, instructions must be non-blank, `multiple` only on a choice), with Python's `strip` and `isalpha`
semantics for option labels. What Heimdall itself would need to change to load this is in `INTEGRATION.md`.

The dylib runs the default fast path unless the environment says otherwise.

Request `{"state": "<text>", "questions": {"<key>": <question>}}`; reply
`{"usage": {"input_tokens": N, "output_tokens": 0}, "answers": {"<key>": {"choice": "<argmax>", "probabilities": {...}}},
"backend": "plain-mlx", "precision": "bf16"}` (`precision`: the loaded linear layers' storage, `bf16`, `int8` or `int4`), a noul's answer `{"noul": p_yes, "probabilities": {"true": .., "false": ..}}`.
Each question is read as its own prompt (d1's lone-question layout); `input_tokens` is the longest prompt.

## Speed options and the fallback switch

The default, with no environment set, is the kept fast path: prune, fuse, compile and metal. It is
`Backend::Fast(Opts::fast())`. `D1E_FALLBACK=1` forces the plain path whatever else is set. `D1E_FAST=0` starts from
plain instead of the fast path. Each switch below set to `1` turns its option on and `0` turns it off, on top of
either starting point.

Fallback without being asked:
- **At load:** an option that fails to build or fails its self-check is turned off and named in `Engine::notes()`
  and on stderr. For example, the Metal kernel is checked against the plain ops and must match exactly. The other
  options stay on.
- **At run time:** a failure makes the engine reload the plain path from the checkpoint and answer on it from then on.

Test hooks: `D1E_TEST_METAL_SELFTEST_FAIL=1` (load-time) and `D1E_TEST_FAIL_FAST=1` (run-time).

| switch | default | what it does |
|---|---|---|
| `D1E_PRUNE` | on | after the last full-attention layer only the slot's receptive field: that layer's queries, output and FFN for the last 7 positions (keys and values from every token), the final conv layer on those 7 |
| `D1E_FUSE` | on | q, k, v and gate as one weight; w1 and w3 as one weight |
| `D1E_COMPILE` | on | the whole trunk as one compiled MLX graph; the row padded to a multiple of 128 with pad keys masked out of the full-attention layers (`D1E_PAD_MULTIPLE` overrides, for diagnosis) |
| `D1E_METAL` | on | the short-conv block (B*x, causal width-7 depthwise conv, times C) as one custom Metal kernel through mlx-c's `mlx_fast_metal_kernel`, with the plain path's roundings (bit-identical) |
| `D1E_Q8` | off | 8-bit affine weights, group 64, for every linear layer (quantized_matmul); measured slower, dropped. A checkpoint stored quantized loads as stored instead (below) |
| `D1E_FP16` | off | compute in float16: weights cast at load, activations FP16, norms and softmax as MLX does them |
| `D1E_PROBE_MAX` | off | diagnostic: the largest absolute activation (every matmul output, residual, conv, q, k, attention output, FFN product), run eagerly one layer at a time |

`bench` and `check` print the requested and running path, the options on, and why any are off.

Guard: plan.md's own, against the plain path's results (`--plain`): same argmax, max |dp| <= 0.03. The raw-score
difference is reported only; it moves in BF16 steps of the readout (0.125 above 16). Results on 30 rows of the base
checkpoint (29 Oct):

| options | argmax | max dp | max score diff | passes |
|---|---|---|---|---|
| plain after the refactor | 30/30 | 0 | 0 | yes (bit-identical to v1) |
| prune | 30/30 | 0.012 | 0.063 | yes |
| fuse | 30/30 | 0.011 | 0.125 | yes |
| compile | 30/30 | 0.023 | 0.125 | yes |
| metal | 30/30 | 0 | 0 | yes (bit-identical) |
| q8 | 30/30 | 0.034 | 0.156 | no (dp) |
| default: prune+fuse+compile+metal | 30/30 | 0.023 | 0.094 | yes |
| default + fp16 | 30/30 | 0.016 | 0.102 | yes |
| default, metal self-check forced to fail | 30/30 | 0.023 | 0.094 | yes: ran prune+fuse+compile |
| default, forced run-time failure | 30/30 | 0 | 0 | yes: fell back to plain |
| `D1E_FAST=0` | 30/30 | 0 | 0 | yes: plain |

Largest absolute activation on the 30 rows (`D1E_PROBE_MAX`, every position, no pruning): 28.4 in FP16 and 28.5 in
BF16, far under FP16's 65,504. Logits inside attention are bounded by |q|·|k|·128·scale, about 9,000, and MLX's
attention keeps its softmax in float32. Evidence: `check/result_<name>.json`. Run one with
`check/run_guard.sh NAME D1E_X=1 -- --plain check/result_plain_30.json`.

## Exact speed switches (plan3.md; all on by default since 29 Oct, guarded; D1E_FAST=0 or D1E_FALLBACK=1 for plain)

- `D1E_WINDOW=1`: block-local sliding attention (each 256-query block against its own and the previous key block).
- `D1E_PAD_MULTIPLE=32`: the compiled path pads rows to 32 instead of 128.
- `D1E_TENSOR_MM=1` (or a comma list of weight-name suffixes such as `w2`): linear layers through Metal 4 tensor ops
  (MPP `matmul2d`), on GPUs with neural accelerators (M5 and later). Checks itself against MLX's matmul at load.
- `D1E_SWIGLU=1` (with `D1E_TENSOR_MM`): the SwiGLU product in the w1/w3 GEMM's epilogue.
- `D1E_ADDNORM=1`: the residual add fused with the RMSNorms around it (bit-identical to the separate ops).
- `D1E_QKPREP=1`: q/k RMSNorm, RoPE and the head-major layout in one kernel from the qkvg GEMM output.
  Each switch takes `0` to turn its part off.
- `D1E_PROFILE=1` makes `bench` print the stage split (render, graph build, trunk evaluation, readout).

Test hooks: `D1E_TEST_TENSOR_MM_FAIL=1` and `D1E_TEST_ADDNORM_FAIL=1`. The guard is `check/guard3.sh NAME VARS...`.

## Lossy switches (plan2.md; off by default)

- `D1E_STATE_TOKENS=N`: the state is cut to N tokens before the prompt is built. Whole conversation lines go oldest
  first; the header lines and everything from "Agent:" (else "Pending speech since last send") on are kept.
- `D1E_SKIP_LAYERS=i,j`: conv layers left out (before the last full-attention layer only).

`check decide --checkpoint DIR --out FILE` writes every guard row's probabilities under the current environment.
`check/agree.py FULL VARIANT...` applies plan2.md's rule to them: send family counted as one wake, the ack line
reported separately, and agreement overall and on the rows where the full model wakes or says yes. Timing is done with
`check/abtime.sh` (interleaved rounds). Results are in `ledger.jsonl`.

## Checks

Guard data lives in `check/`:

- `gen_guard.py` (PyTorch venv `Projects/Heimdall/crates/decide/ref/.venv`, CPU, no weights) writes
  `guard_rows.jsonl`: 100 test rows per task with seed 1701 (command_check has only 68 test rows, so 268 rows),
  with prep.py's ids, slot, option names and token groups, and `specials.json` (the tokenizer's special tokens).
- `gen_edge.py` (same venv) writes `edge_rows.jsonl`: 6 shapes the test split lacks (special tokens in the state,
  letter labels, 31 options with empty descriptions, nouls, non-ASCII).
- `ref_probs.py` (MLX venv `lab/decider-mlx/.venv`) writes reference probabilities from the trainer's own path
  (`train_v5.predict` → `d1_2b.scores`, padded micro-batches) for the first N rows per task: raw scores, softmax at
  temperature 1, and at the checkpoint's calibration temperature for the question's bucket (else the default). Refuses to load when memory is short.

```
./target/release/check ids   --checkpoint DIR [--rows check/edge_rows.jsonl]      # no GPU
<mlx venv python> check/ref_probs.py --checkpoint DIR --per-task 10 --out check/ref_X.jsonl
./target/release/check probs --checkpoint DIR --ref check/ref_X.jsonl --out check/result_X.json
D1E_FALLBACK=1 ./target/release/check probs ...                                      # the fallback guard
./target/release/check abi   --checkpoint DIR                                        # C ABI through dlopen
```

`check probs` passes a row when the argmax matches and max |p - p_ref| <= 0.03 at the checkpoint's temperature; it
refuses to load unless used memory + 5 GB stays under 89 GB. For the trained model, rerun with
`--checkpoint .../runs/v5/final` (and regenerate the reference with the same folder; `--per-task 0` for all 268).

`bench` measures plan.md's metric (median warm ms over 20 rows of 1,800-2,100 tokens after 5 warm-ups). It has
not been run: run it only with training paused and the Mac idle.

## Verified (29 Oct, base checkpoint `models--LiquidAI--d1-2B/.../cfe5760f`, M5 Max, training running alongside)

- Token ids: 268 of 268 guard rows equal prep.py's ids, slot, option names and token groups (100 dispatch, 100
  suggest, 68 command_check); 6 of 6 edge rows equal; the special-token list equals Python's (122 tokens).
- Probabilities, 30 rows (first 10 per task): argmax equal on 30 of 30; max |p - p_ref| 0.0144 at the calibration
  temperature 1.337 and 0.0186 at temperature 1; max raw-score difference 0.094 (BF16, padded batch vs one row).
  Same result with `D1E_FALLBACK=1`. Evidence: `check/result_base_30.json`, `check/result_base_30_fallback.json`.
- C ABI: dlopen of the built dylib, create / tokens / decide with a choice and a noul / bad-request error / destroy;
  reply in the shape Heimdall parses; identical probabilities to `Engine::decide`.
- Memory: the engine peaks at about 4.1 GB in MLX (weights 3.2 GB); the Mac stayed at or under 86.6 GB in use.

## Not verified

- The trained V5 checkpoint (it does not exist yet), and the remaining 238 guard rows on the GPU.
- Speed: nothing timed.
- JSON-object states (pretty-printed like Python's `json.dumps(indent=2)`; float and escape formatting may differ).
- Several questions in one request: each is its own prompt here, while d1's runner packs up to 16 after one state
  under `set` attention, so multi-question answers can differ slightly from the Python runner's.
- Heimdall would not load this dylib as is (version string, GGUF path, generation by file name). The exact change is
  in `INTEGRATION.md`; that call was not made.
- Speed of every option: nothing timed here. The build-time fallback (a Metal kernel failing its self-test) has not
  been exercised; the run-time fallback has.
- The CPU path (`gpu=false`), the timeout, and calls from several threads at once (serialised by the lock, not
  exercised).

## Quantized checkpoints: int8 and int4 (30 Oct)

The engine loads the MLX affine checkpoints `models/decide/d1/d1-2B-heimdall-v5-8bit` and `-4bit` as stored: the
text trunk's linear layers (q/k/v/gate/o, conv in/out projections, w1/w2/w3) as packed uint32 codes with BF16
`.scales`/`.biases` per group of 64 inputs, keys without the `model.language_model.` prefix, conv kernels already
(C, k); embedding (the tied head), norms and conv kernels BF16. Detection is by key: any `.scales` tensor makes the
checkpoint quantized, and then config.json's `quantization` block is required. Each weight's bit width is read from
its shapes (packed columns x 32 / (scale columns x group)) and must equal `bits` (linear) or `embedding_bits`
(embedding, which is dequantized row by row where it is read when stored quantized; with no `embedding_bits`, as
mlx-lm writes its exports, the embedding's own width is taken). A mismatch refuses the load. Only MLX's
`quantization` block is read; scales and biases must be one float dtype (these checkpoints: BF16).
`Engine::precision()` and the reply's `precision` say what loaded.

How each option meets quantized weights:
- prune, compile, metal, window, addnorm, qkprep: unchanged (they never touch a linear weight's storage).
- fuse: q/k/v/gate and w1/w3 stacked as weight, scales and biases together (rows stack exactly); only parts at one
  bit width fuse.
- swiglu: the interleaved w1/w3 copy reorders weight, scales and biases the same way (exact).
- `D1E_QDEQ` (default on for quantized checkpoints): a weight read by a prefill-sized input (33 rows or more) is
  dequantized where it is used (`mx.dequantize`, the value quantized_matmul computes) and goes through the BF16
  tensor-op GEMM, with the SwiGLU epilogue for w1/w3. The pruned tail's 7 rows stay on quantized_matmul, which reads
  the packed weight once. No dequantized copy outlives its GEMM.
- `D1E_FORWARD_CACHE_MB` (default 256, quantized + qdeq only): MLX's cache cap is raised to this during one forward
  and restored and cleared after it, so each layer's dequantized weights (the fused w1/w3 is 63 MB in BF16) are
  reused by the next layer instead of being fresh allocations the system zero-fills. Between decisions the 64 MB
  policy is unchanged. `0` turns it off.
- `D1E_QTMM` (default off, measured slower): a tensor-op GEMM that dequantizes each 64-input chunk into
  threadgroup memory and multiplies it there (`src/qmm.rs`), with its own SwiGLU variant. `D1E_QTMM_TILE=TMxTNxKCxSG`,
  `D1E_QTMM_VEC=1` (16-byte code loads), `D1E_QTMM_PAD=N` (tile row padding), `D1E_QTMM_DB=1` (two tiles) are for
  diagnosis. Self-checks at load against dequantize + MLX's matmul; test hook `D1E_TEST_QTMM_FAIL=1`.
- `D1E_QDEQ=0` on a quantized checkpoint: quantized_matmul everywhere; tensor_mm and swiglu are named off (nothing
  would read them). `D1E_Q8=1` on a BF16 checkpoint runs as before (quantized_matmul, tensor_mm off), not qdeq.
- fp16 and q8 turn themselves off on a quantized checkpoint (named in the notes); the plain path (`D1E_FALLBACK=1`)
  is quantized_matmul everywhere; a run-time failure reloads that plain path from the same checkpoint.
- Every kernel self-test now evaluates before it reads its result, so a kernel that fails to compile turns its
  option off instead of panicking (this was found with a broken qtmm variant).

Defaults per precision: BF16 `prune+fuse+compile+metal+window+tensor_mm+swiglu+addnorm+qkprep` (unchanged);
int8 and int4 the same plus `qdeq` and the 256 MB forward cache cap.

### Parity with the Python MLX models (frozen guard, 500 moments)

Against `lab/decider-v5/quant/out/{bf16,int8,int4}.json` of the decider-mlx-trainer branch (Python `qeval.py`, the
same checkpoints, served temperature 2.0626), on the 500 moments of `quant/rows.json`. Guard, frozen before the speed
work: top answer equal on at least 99% (495), every flip a near-tie (reference top-two gap under 0.05), p99 of each
row's max |dp| at most 0.03. Evidence: `lab/decider-quant/` (`guard.sh`, `parity.py`, `results.jsonl`).

| precision, path | agree | flips (reference gap) | p99 \|dp\| | max \|dp\| | pass |
|---|---|---|---|---|---|
| BF16 plain | 498/500 | 0.002, 0.005 | 0.014 | 0.025 | yes |
| BF16 default | 498/500 | 0.012, 0.002 | 0.021 | 0.033 | yes |
| int8 plain | 498/500 | 0.000, 0.013 | 0.019 | 0.024 | yes |
| int8 quantized_matmul fast path | 498/500 | 0.000, 0.004 | 0.019 | 0.042 | yes |
| int8 default (qdeq) | 496/500 | 0.000, 0.004, 0.005, 0.022 | 0.018 | 0.031 | yes |
| int4 plain | 498/500 | 0.009, 0.011 | 0.019 | 0.043 | yes |
| int4 quantized_matmul fast path | 499/500 | 0.009 | 0.016 | 0.033 | yes |
| int4 default (qdeq) | 497/500 | 0.009, 0.000, 0.011 | 0.020 | 0.039 | yes |

The BF16 rows are the engine-against-Python noise floor: the quantized rows sit inside it. qtmm gives the same
answers as qdeq (it multiplies the same dequantized values). Fallbacks on int4 (10 rows against the default): qtmm,
tensor GEMM, metal and addnorm self-tests forced to fail, a forced run-time failure, fp16 and q8 asked for: every top
answer equal, max |dp| 0.0098. The C ABI check passes on both checkpoints.

### Speed (busy Mac, 30 Oct)

Prefill GEMMs at M about 1,900 are compute-bound, so 8 or 4 bits cannot beat BF16 on speed; the climb removed what
quantization added. Measured with `abbench` (engines side by side in one process, the 20 bench rows alternated,
MLX's cache cleared after every decision as the C ABI does; 5 cycles; other agents were compiling throughout):

| | BF16 (same run) | before: quantized_matmul | after: default | ratio to BF16, before -> after |
|---|---|---|---|---|
| int8 | 119.2 ms (105.5-171.0) | 140.8 ms (122.7-182.8) | 130.2 ms (115.7-186.6) | 1.176 -> 1.095 |
| int4 | 118.2 ms (100.0-174.0) | 135.6 ms (113.8-186.8) | 131.1 ms (108.1-216.1) | 1.146 -> 1.079 |

`abbench --rows FILE --cycles 5 --v "name|CHECKPOINT|VARS" ...` (wrapper `lab/decider-quant/ab.sh`) refuses settings
that are read at run time or are process-wide (`D1E_PAD_MULTIPLE`, `D1E_CACHE_LIMIT_MB`, ...); `bench --release 1`
clears the cache after every decision too (a quantized forward clears its own, so compare precisions that way).
Median, then the fastest and slowest single decision. Separate-process rounds (`bench`, 3 rounds) agree in
direction but overlap (BF16 alone ranged 101.6-149.0 ms between rounds). Every attempt is in
`lab/decider-quant/ledger.jsonl`. What did not pay: the fused qtmm kernel in three rounds (word loads 1.45-1.8x
slower; 16-byte loads and larger row tiles 1.16x; padded rows and double buffering 1.19x, all against MLX's own
1.05-1.15x), because each 64-row tile re-dequantizes its weight panel about 30 times per decision; qdeq under the
64 MB cache cap (allocation churn); scheduling the next weight's dequantize beside the current GEMM (mlx `depends`,
both orderings: within noise); the forward cache cap for BF16 (1.002).

### Memory (measured 30 Oct, GB = 10^9 bytes)

From `bench` in its own process on the default path, the 20 bench rows (1,800-1,878 tokens):

| | on disk (model.safetensors) | MLX active after load | MLX peak during load | MLX peak during a decision | process footprint, lifetime peak |
|---|---|---|---|---|---|
| BF16 | 3.836 (includes the unused vision tower) | 3.458 | 3.568 | 4.156 | 4.81 |
| int8 | 2.083 | 2.083 | 2.194 | 3.075 | 3.46 |
| int4 | 1.350 | 1.350 | 1.461 | 2.352 | 2.74 |

The load's own peak is one layer's fuse above the resident weights (no dequantized copy of the model is ever made).
The decision peak is the activations (about 1 GB at 1,900 tokens) on top of the weights. The process footprint adds
the tokenizer, kernels, MLX's cache and the forward cap's recycled buffers; it is the number closest to what Activity
Monitor shows for the engine. The plain path's MLX peaks are within 0.2 GB of these.

