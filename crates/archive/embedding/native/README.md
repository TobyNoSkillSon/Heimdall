# Native EmbeddingGemma 2 helper

The Swift MLX process behind semantic search: it embeds text, screen frames and audio with EmbeddingGemma 2, and in
rerank mode scores documents with Qwen3-Reranker-0.6B. Serving is BF16; Python is only a frozen reference and
benchmark client.

Read this when: you change or rebuild the embedding helper, its fast paths, its memory policy, its tokenizer patch or
rerank mode.
Code: `crates/archive/embedding/native/Sources/EmbeddingWorker/`, the Rust clients `crates/archive/src/embedding.rs`
(`MlxEmbedder`) and `crates/archive/src/rerank.rs`; build script `scripts/build_embedding_native.py`.

## Files

| File | Owns |
|---|---|
| `Worker.swift` | entry point, stdio protocol, media splicing, the MLX cache policy, `--self-test`, `--tokenize`, `--dump-image`, `--cache-policy`, `--rerank` |
| `Trunk.swift` | the text graph over loaded parameters; bit-identical to upstream with `HEIMDALL_EMBED_TRUNK=exact` |
| `AudioEncoder.swift` | all audio parts of one input in one padded BF16 call, one Metal kernel for chunked local attention |
| `Media.swift` | image and video decode with Pillow's integer bicubic arithmetic, on parallel threads |
| `Fast.swift` | the optional fast paths, one switch each, plus the head-dim 256/512 flash attention kernel |
| `Gate.swift` | the per-part self-test and its stored verdict |
| `SmallMGEMM.swift`, `TileLinear.swift` | small-row GEMM/GEMV kernels (vendored verbatim from Vella commit `c3e298e`) and the 2048 to 512 tensor tile |
| `Quant.swift` | mixed-precision spec, lab only |
| `Rerank.swift` | the Qwen3 reranker graph and its protocol |
| `BPEHeap.swift.inc` | the tokenizer patch (below) |
| `Package.swift`, `Package.resolved` | pinned SwiftPM sources: MLX Swift LM `c53d7d480147684f42dac9d959f648dc35f021d0`, MLX Swift 0.32.3 |
| `rerank_parity.py` | opt-in reranker parity check (model inference) |

## Build it

1. `python3 scripts/build_embedding_native.py`. It builds no app, installs nothing, downloads no model and runs no
   inference. It compiles the MLX Metal sources with fast math off, carries tokenizer resource bundles, and updates the
   runtime tree hash that `scripts/bundle_models.py` checks.
2. Rebuild the tree and the lock together after any source or compiler change.
3. Check: `cargo test -p heimdall-archive --test embedding_native` (the cache-policy test runs the built helper
   without loading weights).

## How Rust runs it

- `MlxEmbedder` starts `models/embeddinggemma2/native/heimdall-embedding-worker` with the weights directory over a
  bounded, ID-matched stdio boundary with cancellation, lazy loading and shared residency.
- After `SAFE_AFTER_CRASHES` (2) crashes in a row of one helper build, Rust sets `HEIMDALL_EMBED_SAFE=1` and the helper
  runs every part plain until it is rebuilt.
- Rust passes `HEIMDALL_EMBED_STATE` (where the gate verdict lives) and strips the lab variables.

## Fast paths and the gate

- Parts: `smallM`, `tile`, `audioKernel`, `compiled`, `compiledVision`, `batch` are on after their self-test
  (`Gate.defaults`); `fused256`, `flash512`, `blocked` are built and gated but off.
- On the first start for a key of GPU architecture, macOS build, helper size and mtime and `SmallMGEMM` revision, the
  helper runs itself as `--self-test <weights> <parts>` in a child process and compares each part with stock MLX. A
  Metal compile error kills only the child. The verdict is `embedding-gate.json` under `HEIMDALL_EMBED_STATE`.
- While serving, a non-finite vector redoes the request plain and turns the suspect parts off; the first batched group
  per process is re-checked at batch 1 (cosine at least 0.999).
- Mixed precision was measured and rejected: serving stays BF16.

## Memory

- The MLX buffer cache is capped at 256 MB (`CachePolicy.defaultBytes`) and cleared after every reply.
- `--cache-policy` prints the limit without loading weights. `HEIMDALL_EMBED_CACHE_MB` (MB, or `legacy` for the old
  uncleared `min(2 GiB, max(256 MB, free/16))`) is the lab A/B switch.

## Preprocessing and windows

- Images keep encoded PNG RGB values (Display P3 included) and Pillow's horizontal-then-vertical uint8 bicubic
  rounding. Audio: semicausal 321-sample frames, 320-sample Hann window, 512-point magnitude FFT, 160-sample hop and
  left padding, HTK mel, 0.001 log floor. Every recorder video frame is used, in order.
- The Rust reader flushes audio at 120 s, keeping the last callback's overhang; `Media.swift` refuses PCM over 240 s
  (16 kHz float). A fresh helper gets 240 s for its self-test and weight load (`STARTUP` in `embedding.rs`). Windows hold at most 6,000 tokens; windows slice the original text and weight normalised window vectors
  by token count. Media placeholder IDs are spliced between tokenised text runs.

## Tokenizer patch

The pinned Swift tokenizer's quadratic BPE is replaced at build time by the rank and left-position neighbour heap in
`BPEHeap.swift.inc`. Unicode scalars seed merges, and space normalisation works at scalar boundaries. Why: a
joined-string intermediate corrupts combining marks and ZWJ characters. The build records the patched source hash; a
changed upstream patch target fails the build. `HEIMDALL_EMBED_TOKENIZE=check` compares spliced tokenisation with
full-prompt tokenisation.

## Lab switches

`HEIMDALL_EMBED_TRUNK=upstream|exact`, `HEIMDALL_EMBED_AUDIO=upstream`, `HEIMDALL_EMBED_PROFILE=1` (forced-eval phase
timing on stderr), `HEIMDALL_EMBED_FAST=plain|part,part`, and with `HEIMDALL_EMBED_LAB=1` the `{"op":"lab"}` request
and `HEIMDALL_EMBED_QUANT`. Protocol: `{"op":"batch","items":[[Input..]..]}` returns `vectors`/`errors` per item (up
to 8 chunks, length groups padded and masked); `{"op":"trim"}` frees the MLX cache and keeps the weights.

## Rerank mode

- `heimdall-embedding-worker --rerank <Qwen3-Reranker-0.6B dir>` serves the reranker (revision `e61197ed`,
  `model_version` `e61197ed:bf16:rerank-v1`); the embedding paths are unchanged.
- Method: Qwen's yes/no prompt; the system, instruction and query prefix run once per request and their keys and values
  are shared by every document batch; documents are `" " + text` cut to `max_doc_tokens`, then the suffix; stable
  length-sorted right-padded batches; score = FP32 softmax over the last position's [no, yes] rows, P(yes). Weights
  must be BF16 (any other dtype is refused); activations are BF16.
- Memory: cache limit 256 MB, cleared after every reply; memory limit 3 GB. Before start and each request it needs
  3 GB free and no swap growth (over 16 MB within 120 s); otherwise the reply is `{"id":N,"error":"rerank paused: …"}`,
  or at start exit 75 with `rerank paused:` on stderr. A missing or bad model dir exits 1. Nothing is printed before
  the first request.
- Request: `{"id":N,"query":"…","documents":["…"],"max_doc_tokens":256,"instruction":"…","batch":32}`; at most 500
  documents (`maxDocuments`), empty ones scored, lines up to 16 MB, `max_doc_tokens` clamped to 2048, `batch` to 128.
- Reply: `{"id","model_id":"Qwen/Qwen3-Reranker-0.6B","model_version","scores":[P(yes) in input order],"stats":{"seconds",
  "load_seconds","tokens","peak_bytes","active_bytes","batch"}}`. `{"id":N,"op":"usage"}` returns process usage and
  MLX counters.
- Default batch 32 (`defaultBatch`): batch 16 and 32 were fastest on 100 documents of 60 to 200 tokens, measured on
  an M5 Max in use; peak about 2 GB for every batch size (weights 1.19 GB).
- Parity check (opt-in, runs the model): `uv run --offline --no-project --with mlx-lm==0.32.2 python3
  crates/archive/embedding/native/rerank_parity.py`; `--tokenizer` compares 68 English, Polish, emoji and
  special-token prompts with `transformers`. The remaining score gap (max |ΔP| about 0.02) is BF16 rounding of the
  yes/no logits.

## Tests

- `cargo test -p heimdall-archive --test embedding_native`: the cache-policy check against the built helper.
- Ignored: `native_default_query_and_cancellation_restart` needs `HEIMDALL_EMBED_NATIVE_LAB_MODELS` set to an isolated
  model root and runs real inference; run it alone, one GPU job at a time.
- Native benchmark counters cover the client and helper process, not whole-machine energy.

## See also

- [chunk-index.md](../../../../docs/chunk-index.md): timeline search and how the reranker is used
- [models.md](../../../../docs/models.md): the EmbeddingGemma 2 and reranker downloads
- [archive.md](../../../../docs/archive.md): what gets embedded
