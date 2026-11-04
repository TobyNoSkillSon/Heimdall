# Native EmbeddingGemma 2 helper

Rust `MlxEmbedder` starts `models/embeddinggemma2/native/heimdall-embedding-worker`
with the weights directory. It keeps the existing bounded, ID-matched stdio boundary,
cancellation, lazy loading, shared residency and `Embedder` interface. Serving uses
BF16 unconditionally; Python is only a frozen baseline/reference and benchmark client.

Agent build: `python3 scripts/build_embedding_native.py`. This builds/signs no app,
installs nothing, downloads no model and performs no inference. SwiftPM resolves
the source packages pinned in `Package.resolved`. The script compiles matching MLX
Metal sources with no fast math, carries tokenizer resource bundles, and updates
the native runtime tree hash consumed by `bundle_models.py`. Rebuild the tree/lock
together after any executed source or compiler change. SwiftPM caches remain local.

Checkpoint encoders and the text graph are upstream MLX Swift LM revision
`c53d7d480147684f42dac9d959f648dc35f021d0`, core MLX Swift 0.32.3. Native preprocessing
retains encoded PNG RGB values (including Display P3 sources), Pillow's horizontal
then vertical uint8 bicubic rounding, official semicausal 321-sample framing with
320-sample Hann/512-point magnitude FFT, 160-sample left padding/hop, HTK mel and
0.001 log floor. Every recorder video frame is used, in order. Full audio is retained
through the Rust reader's 120-second flush threshold, including the final callback's
overhang. A 240-second hard file cap bounds host PCM; the 6,000-token window budget
adds its own context bound. Audio-only lower-bound counts window long legacy inputs
before encoding them, avoiding a discarded full-input encoder pass. Windows slice original text
and weight normalized window vectors by their actual token lengths.

The pinned Swift tokenizer's quadratic BPE is replaced at build time with the
rank/left-position neighbor heap in `BPEHeap.swift.inc`. Unicode scalars seed merges,
token arrays cross the internal boundary directly, and space normalization works
at scalar boundaries. A joined-string/split intermediate corrupts combining marks
and ZWJ characters. The build records the executed tokenizer source hash; a changed
upstream patch target fails explicitly. This is a native source patch, not a Python
tokenization service or an opt-in inference switch.

Checks: frozen FP32 vectors and exact image hash through `embeddinggemma2_bench.py`,
native tensor captures via `--dump-image`, exact tokenizer IDs via `--tokenize`,
and `embedding_native.rs` through the real Rust process/cancellation/restart path.
Native benchmark counters include the client and helper process; they do not measure
whole-machine energy. All GPU checks/measurements use the main checkout's lockf lock.

Pass 2 (`native-v2` vectors): the helper keeps the upstream checkpoint loader and vision
tower but runs its own graphs over the loaded parameters. `AudioEncoder.swift` encodes
all audio parts of one input in one padded call with BF16 activations (upstream's strong
FP32 residual scalar promoted every audio layer to FP32) and one Metal kernel for the
chunked local attention. `Trunk.swift` is bit-identical to the upstream text graph with
`HEIMDALL_EMBED_TRUNK=exact`; serving adds FP32 norm weights cast once, no all-true mask
arrays, and the tensor tile kernels (`SmallMGEMM.swift`, vendored verbatim from Vella
c3e298e; B's 2048->512 tile in `TileLinear.swift`). Media placeholder IDs are spliced
between tokenized text runs (`HEIMDALL_EMBED_TOKENIZE=check` compares with full-prompt
tokenization); the BPE patch memoizes words. Images and video frames are decoded and
Pillow-resampled on parallel threads with the same integer arithmetic. The MLX buffer
cache is capped at 256 MB and cleared after every reply (30 Oct): the old min(2 GiB, free/16)
cap kept the last requests' dead buffers resident, 3.7 GB of GPU memory for 1.5 GB of
weights; now 1.6 GB after the same burst, vectors bit-identical, long texts about 17%
slower (lab/memory-cache/README.md). `HEIMDALL_EMBED_CACHE_MB` (MB, or `legacy`) is the
A/B switch; `--cache-policy` prints the limit without loading weights. Lab A/B paths: `HEIMDALL_EMBED_TRUNK=upstream|exact`,
`HEIMDALL_EMBED_AUDIO=upstream`, `HEIMDALL_EMBED_PROFILE=1` (forced-eval phase
attribution on stderr); `scripts/lab/embeddinggemma2_native_{parity,profile}.py` and
`embeddinggemma2_multiarm.py` drive them. C's query-priority `Service` is unchanged.

Transferable lessons from pass 1 and pass 2 are merged into
`System/skills/optimize-mlx-inference/SKILL.md`.

Overhaul (3 Nov 2026). Every optimisation is a switch in `Fast` (Fast.swift) behind a per-part
self-test (Gate.swift, Vella KernelGate pattern): on the first start for a key of GPU
architecture, macOS build, helper binary size/mtime and SmallMGEMM revision, the helper runs
itself as `--self-test <weights> <parts>` in a child process before loading its own weights.
Each part is compared with stock MLX (kernels on random tensors, compiled/batched paths on whole
synthetic inputs); a Metal compile error kills only the child, failing the part it was on. The
verdict is stored as `embedding-gate.json` in `HEIMDALL_EMBED_STATE`. A non-finite vector while
serving redoes the request plain and turns the suspect parts off in the verdict; the first
batched group per process is re-checked at B=1 (cosine >= 0.999). `HEIMDALL_EMBED_SAFE=1`
(Rust sets it after two crashes in a row of one binary) runs everything plain.
Parts: `smallM`, `tile`, `audioKernel` (existing kernels, now gated), `compiled` (shapeless
compiled GELU-gate and residual+norm chains, bit-identical), `compiledVision` (shape-compiled
vision tower), `batch` (op `batch`: up to 8 chunks, length groups padded and masked, same-size
screens through the vision tower together), `blocked` (long global attention in 512-row query
blocks), `fused256` (MLX's fused attention forced for head dim 256) and `flash512` (Heimdall
flash kernel for the head-dim-512 global layers, which MLX has no fused kernel for).
Protocol additions: `{"op":"batch","items":[[Input..]..]}` -> `vectors`/`errors` per item;
`{"op":"trim"}` frees the MLX cache, weights stay resident; `{"op":"lab"}` (only with
HEIMDALL_EMBED_LAB=1) switches parts and the mixed-precision spec (`HEIMDALL_EMBED_QUANT`,
Quant.swift) for the lab harness `lab/embeddinggemma2/embeddinggemma2_overhaul.py`.
On by default after the self-test: smallM, tile, audioKernel, compiled, compiledVision, batch.
Built and gated but off (3 Nov: no peak-memory gain on the longest real inputs, speed not yet
measured on an idle Mac): fused256, flash512, blocked. Mixed precision was measured and rejected
(int8 mixes changed 2.8-4.0 % of top-10 results against a 1.7 % BF16 noise floor); serving stays BF16.
Idle-only speed and energy runs: `lab/embeddinggemma2/run_idle_benchmarks.sh`.
