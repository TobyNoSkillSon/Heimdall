# d1v5-engine

The decider's inference engine, in Rust on MLX (`mlx-rs` 0.32, MLX v0.32.2): a text state and questions in, each
option's probability out, from one forward pass read at the answer slot. Heimdall loads it as a dylib through
`crates/decide/src/d1.rs`; it runs Decider V6 and V5 (same architecture and tokenizer) at 16, 8 or 4 bits.

Read this when: you want to build or place the decider runtime, change the engine or its C ABI, or turn a speed path
off to diagnose a wrong answer.
Code: `crates/decide/engine-v5/src/`, `scripts/build_decider_v5.sh`.

## Build and place it

The crate is standalone (its own empty `[workspace]`, own `target/`), so workspace builds never compile MLX.

1. From the repository root run `scripts/build_decider_v5.sh`. It runs `build/fetch-deps.sh` when
   `target/deps-src` lacks MLX or metal_cpp, fetches the Rust crates once if the cargo registry lacks them, builds at
   nice 19 with two jobs (`HEIMDALL_JOBS` overrides), offline, and copies
   `libd1v5_engine.dylib` and `mlx.metallib` to `models/decide/runtime-v5/` (git-ignored;
   `HEIMDALL_D1V5_RUNTIME_DIR` overrides).
2. `scripts/build_decider_v5.sh --lock` also rewrites `crates/decide/scripts/runtime-v5.lock.json`;
   `scripts/bundle_models.py` refuses files that differ from it.
3. Check: the script prints the size and SHA-256 of both files; the app's log names the loaded decider.

- The dylib embeds its build folder's paths, so a build in another checkout has another SHA-256: rebuild with `--lock`
  and commit the lock, or reuse the placed files.
- `build/fetch-deps.sh` puts MLX v0.32.2 and its C++ dependencies (mlx, fmt, json, metal_cpp, gguflib) in
  `target/deps-src`: copied from a local mlx-sys build when it finds one, else cloned at pinned tags; metal_cpp is
  then downloaded from the `METAL_CPP_URL` that MLX's `CMakeLists.txt` pins.
- `.cargo/config.toml` points mlx-sys's cmake at those copies (`build/offline.cmake`) and writes `mlx.metallib` to
  `target/mlx-metal/`. A dylib shipped elsewhere needs `mlx.metallib` beside it (MLX looks beside the binary first).
- Rust crates come from the local cargo registry (`--offline`, versions in `Cargo.lock`); the script runs
  `cargo fetch --locked` first only when that offline fetch fails.

## Files

| file | what it holds |
|---|---|
| `src/lib.rs` | `Engine` (`load`, `decide`, `encode`, `scores`, `notes`, `precision`), `Backend`, the option switches |
| `src/ffi.rs` | the C ABI and `VERSION` |
| `src/model.rs` | the text trunk in MLX, fast and plain paths, quantized loading |
| `src/prompt.rs` | the prompt d1-2B's runner builds (a port of its `prompt.py`), `Prompt::plain`, option token groups |
| `src/metal.rs`, `src/fused.rs`, `src/mpp.rs`, `src/qmm.rs` | custom Metal kernels: short-conv block, fused add-norm and q/k prep, tensor-op GEMM, quantized tensor-op GEMM |
| `src/cache.rs` | MLX buffer-cache cap |
| `src/mem.rs` | used-memory reading for the binaries' memory guard |
| `src/bin/` | `check`, `bench`, `abbench`, `memcheck` (below) |

## API

Rust: `Engine::load(checkpoint_dir)` reads `config.json`, the tokenizer files, `model.safetensors` and
`calibration.json`; `engine.decide(state, question_json)` returns `(option, probability)` in the question's option
order. Probabilities are `softmax(scores / T)` with `T` from `calibration.json` (`temperatures[bucket]`, else
`default`).

Questions: `{"type": "choice", "instructions": …, "criteria": {name: description}}` or `{"type": "noul", …,
"criteria": {"true": …, "false": …}}`. Other kinds are refused. They are checked as the Python runner's
`check_question` checks them.

C ABI (`src/ffi.rs`):

| symbol | notes |
|---|---|
| `heimdall_d1_create_text_backend(model, projector, threads, gpu, &err)` | `model` is the checkpoint folder; projector and threads ignored; `gpu=false` runs on MLX's CPU device |
| `heimdall_d1_decide(handle, json, timeout_ms, &err)` | JSON in, JSON out; `timeout_ms` > 0 is checked before each question's forward, a started forward is not cut short |
| `heimdall_d1_decide_interruptible(handle, json, timeout_ms, cb, ctx, &err)` | `cb(ctx)` polled before each question's forward |
| `heimdall_d1_tokens(handle, text)` | token count of the text as the prompt carries it (special-token strings broken up), no special tokens added |
| `heimdall_d1_free(str)`, `heimdall_d1_destroy(handle)` | destroy also clears MLX's buffer cache |
| `heimdall_d1_version()` | `d1v5-engine@0.1.0;mlx-rs-0.32;text-only;json-v1;affine-q8q4-v1`; the app accepts only `d1v5-engine@…` (`d1::check_version`) |
| `heimdall_d1_recreate_device()` | always true |
| `heimdall_d1_test_fail_next(handle)` | the next decide fails with `HEIMDALL_GPU_FAULT` |

Request `{"state": "<text>", "questions": {"<key>": <question>}}`; reply `{"usage": {"input_tokens": N,
"output_tokens": 0}, "answers": {"<key>": {"choice": "<argmax>", "probabilities": {…}}}, "backend": …, "precision":
"bf16"|"int8"|"int4"}`; a noul answers `{"noul": p_yes, "probabilities": {"true": …, "false": …}}`. Each question is
read as its own prompt; `input_tokens` is the longest.

Every entry point catches panics; one process-wide lock serialises all MLX work; MLX errors that name Metal or the GPU
carry `HEIMDALL_GPU_FAULT`. The engine caps MLX's buffer cache at 64 MB and clears it after loading and after every
decide (`src/cache.rs`); `D1E_CACHE_LIMIT_MB` (MB, or `off`) changes the cap.

## Speed paths and the fallback

With no environment set the engine runs its fast path, every exact option on. Each option checks itself at load
against the plain ops; one that fails to build or self-check is turned off and named in `Engine::notes()` and on
stderr. A run-time failure reloads the plain path from the checkpoint and answers on it from then on.

- `D1E_FALLBACK=1` forces the plain path whatever else is set. `D1E_FAST=0` starts from plain instead of fast.
- Each switch set to `1` turns its option on and `0` off, on top of either starting point.

| switch | default | what it does |
|---|---|---|
| `D1E_PRUNE` | on | after the last full-attention layer, compute only the last 7 positions |
| `D1E_FUSE` | on | q, k, v and gate as one weight; w1 and w3 as one weight |
| `D1E_COMPILE` | on | the trunk as one compiled MLX graph, rows padded (`D1E_PAD_MULTIPLE`) |
| `D1E_METAL` | on | the short-conv block as one custom Metal kernel, bit-identical to plain |
| `D1E_WINDOW` | on | block-local sliding attention |
| `D1E_TENSOR_MM` | on | linear layers through Metal 4 tensor ops (M5 GPUs and later); a comma list of weight suffixes limits it |
| `D1E_SWIGLU` | on | the SwiGLU product in the w1/w3 GEMM's epilogue (with `D1E_TENSOR_MM`) |
| `D1E_ADDNORM` | on | residual add fused with the RMSNorms around it |
| `D1E_QKPREP` | on | q/k RMSNorm, RoPE and head-major layout in one kernel |
| `D1E_QDEQ` | on for quantized checkpoints | prefill-sized inputs dequantize the weight where used and take the tensor-op GEMM |
| `D1E_FORWARD_CACHE_MB` | 256 (quantized with qdeq) | cache cap during one forward; `0` off |
| `D1E_QTMM` | off | quantized tensor-op GEMM (`src/qmm.rs`); `D1E_QTMM_TILE`, `_VEC`, `_PAD`, `_DB` for diagnosis |
| `D1E_Q8` | off | 8-bit weights for a BF16 checkpoint |
| `D1E_FP16` | off | compute in float16 |
| `D1E_PROBE_MAX` | off | diagnostic: the largest absolute activation, layer by layer |
| `D1E_STATE_TOKENS`, `D1E_SKIP_LAYERS` | off | lossy: cut the state to N tokens; skip conv layers |
| `D1E_PROFILE` | off | `bench` prints the stage split |

Test hooks force a failure: `D1E_TEST_METAL_SELFTEST_FAIL`, `D1E_TEST_TENSOR_MM_FAIL`, `D1E_TEST_ADDNORM_FAIL`,
`D1E_TEST_QKPREP_FAIL`, `D1E_TEST_QTMM_FAIL` (load time) and `D1E_TEST_FAIL_FAST` (run time).

## Quantized checkpoints

The engine loads MLX affine checkpoints (the `-8bit` and `-4bit` folders) as stored: the trunk's linear layers as
packed codes with BF16 scales and biases per group of 64 inputs; embedding, norms and conv kernels BF16. Any `.scales`
tensor marks a checkpoint quantized, and then `config.json`'s `quantization` block is required; each weight's bit
width must match it or the load is refused. `fp16` and `q8` turn themselves off on a quantized checkpoint, and the
plain path uses `quantized_matmul` everywhere.

## Tests and checks

- `cargo test --manifest-path crates/decide/engine-v5/Cargo.toml --release --offline` (package `d1v5-engine`): unit
  tests in `src/model.rs`, `src/prompt.rs` and `src/cache.rs`. Build at low priority as `scripts/build_decider_v5.sh`
  does.
- The binaries load the 2B model; run one at a time, never inside the app. Each refuses to load when used memory plus
  the model would pass `--max-used-gb` (89 by default; `memcheck` 100).
  - `check ids|probs|abi --checkpoint DIR …`: token ids against the Python runner's, probabilities against a reference
    file (same argmax, max |p − p_ref| ≤ 0.03), and the C ABI through `dlopen`. `--plain FILE` also compares with the
    plain path. Guard rows are not in this repository; pass `--rows`.
  - `bench --checkpoint DIR`: median warm decision time; run only on an idle Mac.
  - `abbench --rows FILE --v "name|CHECKPOINT|VARS" …`: variants interleaved in one process.
  - `memcheck burst|latency`: resident memory through the C ABI.

## See also

- [crates/decide/README.md](../README.md): the decide crate that loads this engine
- [docs/decision-chain.md](../../../docs/decision-chain.md): what the decider is asked and when
- [docs/models.md](../../../docs/models.md): the decider checkpoints, sizes and memory
- [docs/settings.md](../../../docs/settings.md): `decider_model`, `decider_precision`
