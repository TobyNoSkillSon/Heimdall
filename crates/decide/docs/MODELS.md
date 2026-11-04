# Deciding model provenance

Checked 11 October 2026 against the immutable source snapshots in
`scripts/models.lock.json` and `scripts/sources.lock.json`.

## Intelligent Wakeup

Selected `TCLResearchEurope/int_wakeup-whisper_base-ctx20-ft4`, revision
`b917d9777f1c8316d12fa5c963c8f1a6e08f8835`. Selection criterion: follow-up
addressedness and false accepts in the publisher's matched English synthetic
benchmark. Whisper contextual recall .759 versus Zipformer .716; false accepts
3.6/hour versus 3.98/hour. Zipformer's direct recall is better (.981 versus .937)
and its variable input avoids the 30-second Whisper pad. Its Mac speed was not
measured here; these source figures are not Heimdall accuracy claims.

The current encoder comes from the fine-tuned checkpoint. The **different,
pristine context encoder** comes from pinned `openai/whisper-base` revision
`e37978b90ca9030d5170a5c07aadb050351a65bb`, with TCL's separate context pooling
head. Fine-tuned representations are never reused as historical context.
The upstream card warns that sharing the fine-tuned encoder drops validation
F1 .896→.718 and contextual recall .759→.290. ONNX current/context/classifier
exports are float32; hashes are in `scripts/exports.lock.json`.

Agent role is 1. Human IDs 2..15 are assigned by first appearance and retained
for the conversation. Call `clear_context_cache` on a new conversation. Up to
20 prior turns are right aligned in the classifier input; missing slots are
masked, and the current turn occupies the last position. The bounded audio-hash
cache contains only pristine context embeddings. Audio input is mono 16 kHz,
up to 30 seconds. Whisper preprocessing right-pads and does **not** normalize
waveform variance; it differs from Smart Turn's frontend.

TCL's model card declares **Apache-2.0**. Its hosted code references a LICENSE
file that the pinned model repository does not include. Its training dataset is
**CC-BY-NC-4.0**. This implementation is a local research integration; commercial
redistribution needs the missing licence/provenance clarified. The wake word
baked into training is **Sigma**, not Heimdall. Five parity fixtures include a
positive Sigma clip, human-directed speech, English and Polish public audio.

Sources:
- https://huggingface.co/TCLResearchEurope/int_wakeup-whisper_base-ctx20-ft4
- https://huggingface.co/TCLResearchEurope/int_wakeup-zipformer_small-ctx20-ft4
- https://huggingface.co/datasets/TCLResearchEurope/intelligent_wakeup
- https://huggingface.co/openai/whisper-base

## d1-2B (Decider V6, and V5 the rollback)

Current default: `d1/d1-2B-heimdall-v6`, a full fine-tune of the text trunk from V5
(`runs/v5/final`), `training-material/d1/runs/v6/final` (step 1006, refitted default temperature 1.57), trained on
`data/v6` (V5 replay plus Polish and eight more languages, corrected wait labels,
names and hotkey lessons). Its six release gates pass
(`runs/v6/report.json`); int8 scored identically to BF16 there and int4 within noise. Same architecture, tokenizer,
questions and engine as V5; `-8bit` and `-4bit` from `runs/v6/quant/v6/int8` and `int4`. The app's version and precision are the settings `decider_model` and `decider_precision` (`src/precision.rs`). The
weights are published at https://huggingface.co/Toby-NoSkillSon/heimdall-d1-v6 (int8 default) and are not in the repo.

V5, kept in the bundle as the rollback (`decider_model: "v5"`), live from 29 October to 4 November: `d1/d1-2B-heimdall-v5`, local fine-tune of `LiquidAI/d1-2B` at
`cfe5760ff5d5998f91d9ac56bd4b2dbd96996d41`, `training-material/d1/runs/v5/final` (step 2000), trained on
`data/v5` (dispatch with English ack lines, command_check, suggest; names header on
every row). The folder holds the five files the engine reads: config.json, tokenizer.json, tokenizer_config.json,
model.safetensors (BF16, 3.84 GB) and calibration.json (default temperature 2.06). Runtime: the d1v5 engine
(`crates/decide/engine-v5`, Rust on mlx-rs 0.32 / MLX v0.32.2) as `runtime-v5/libd1v5_engine.dylib` plus
`mlx.metallib`. SHA-256 and sizes: `scripts/models.lock.json`, `scripts/runtime-v5.lock.json`. Licence: LFM Open
License v1.0 (`licenses/LFM-Open-License-1.0.txt`, taken from d1-2B).

## d1-omni-600M

Default from 25 to 29 October (now loadable, not bundled): `d1-omni-600M-heimdall-v4-F16.gguf`, local fine-tune from
the `d1-train` base (sha 0713bb05), `training-material/d1/runs/v4` (same dispatch question
with instant-ack lines as v3, `src/send_v3.rs`). Installed over the v3 parity and dispatch numbers in `lab/decider-v4/PLAN.md`. Rollback model: v3
(24 Oct, `runs/v3`, still bundled). Earlier defaults: v2 (17 Oct, `runs/v2`), v1 (14 Oct,
`runs/v1`), base HF `02b55d70`. Rolling back is reverting the commit that switched
`DEFAULT_MODEL`. SHA-256 and byte size are pinned in `scripts/models.lock.json`.
Production requests are text only for every question, including fixed commands.
The projector is neither loaded (including GPU recovery) nor bundled. The legacy
media tags/waveforms remain readable for archive evaluation. Bundling excludes the original model;
development parity tests and original feature-collection receipts still pin it.
`scripts/evaluate_tuned_d1.py` checks native held-out quality, temperature handling,
other-question changes and interleaved timing; raw receipts remain private.

Original reference: official **F16 GGUF**, revision
`04397145ed8381350403aa556db0ec6a49dd8c07`, through pinned llama.cpp
`62a6f74bab748b1755f4cd8dfc0871caf865c6c8`. The Rust crate loads the owned C ABI
library, which invokes llama.cpp's native typed-decision handler in process.
There is no HTTP listener, child daemon, Python interpreter or external app at
runtime. Metal and Accelerate are enabled. Historical audio/image tensor parity
is exercised through a separate native reference handle with its own projector;
that handle is never used by the app.

This is a typed decision model, not a token generator. The API exposes only
send-now, the six documented fixed commands, and choosing a supplied candidate.
One asynchronous owner runs one question at a time, with full recomputation
(the model is non-causal). Every question is fitted to a 2,000-token text window
(oldest context dropped first, current utterance kept). Typed choice options,
escaping, marker readout and checkpoint temperature handling remain native.
The Rust production boundary never sends files or PCM, including when decoding
an old archived audio/image input. Capture audio and clip references remain in
the archive. A model probability is not calibrated for Heimdall's dispatch policy.

F16 was selected for reference parity and to avoid the reported audio answer
changes from quantization. An ONNX conversion is now available, including audio;
its published q8 max probability error is .031 for voice and .036 for text.
It was inspected but not downloaded/benchmarked here. The supported native
multimodal route, faithful original typed head, and the approved download budget
favoured GGUF. This is a measured usable native runtime, not a claim that every
alternative was timed or that it is the fastest possible runtime.

### Licence

Weights and upstream d1 code are © Liquid AI, Inc. under **LFM Open License
v1.0**, copied verbatim in `LFM-OPEN-LICENSE-1.0.txt`. It grants use, modification
and redistribution, subject to the commercial-use limit. Section 1 defines the
threshold as annual revenue **$10,000,000 or more**; Section 5 conditions commercial
rights on not exceeding the threshold and excludes entities exceeding it.
The exact boundary wording is inconsistent, so treat reaching $10M as requiring
Liquid AI clarification/a separate grant. This is not Apache/MIT licensing.
Recipients must receive the licence; modifications must be marked; copyright,
patent, trademark and attribution notices must be retained, including any NOTICE.
No automatic SaaS, inference-provider or downstream-application licence exception
was found in the supplied terms. Heimdall's crate interfaces are original MIT
code; that does not relicense the model. The original Python reference is never
bundled. llama.cpp/ggml retain their MIT notices (`LLAMA-CPP-LICENSE.txt`). Include
licences for statically compiled third-party dependencies when packaging.

Sources:
- https://huggingface.co/LiquidAI/d1-omni-600M
- https://huggingface.co/LiquidAI/d1-omni-600M-GGUF
- https://huggingface.co/LiquidAI/d1-omni-600M/blob/02b55d7076f15129e59ab3f94783f32c4b088674/LICENSE
- https://github.com/ggml-org/llama.cpp/blob/62a6f74bab748b1755f4cd8dfc0871caf865c6c8/tools/server/server-decision.cpp
- https://huggingface.co/onnx-community/d1-omni-600M-ONNX

### Reference and fallback

The baseline is LiquidAI's original pinned PyTorch code, reconstructed from the
**same shipped F16 GGUF tensors**, and evaluated in float32. All 1033 GGUF tensors
map to 1084 original state entries with strict completeness checks. Official
conversion folds FastConformer batch normalization into an affine operation;
the reference uses zero mean and variance `1-epsilon` to represent that identical
affine. Vision patch embedding's storage permutation is inverted. There is no
second 2.35-GB original checkpoint download. This establishes runtime parity for
shipped F16; it does not measure F16-versus-original-F32 checkpoint error.

`d1-3B` remains a documented fallback, **not downloaded or implemented** because
omni's native runtime works. Build it only if omni has no workable runtime, as authorized in the task. The official d1-3B GGUF
uses the same native typed-decision route, supports text/image, and has **no audio**.
The same LFM licence applies. Source: https://huggingface.co/LiquidAI/d1-3B-GGUF

## Test data

FLEURS excerpts are Google FLEURS (Conneau et al., 2022), **CC-BY-4.0**. Exact
archive members, references, immutable revision and hashes are in
`tests/fixtures/manifest.json`; these are copied from the existing listen test
assets, not downloaded again. Source: https://huggingface.co/datasets/google/fleurs
The small PNG is a synthetic QA input. Sigma/human clips and the 30-utterance
smoke corpus use the installed macOS Samantha voice, locally synthesized with
no microphone or network. They are not real-world voice accuracy evidence.
No Vella recordings or agent-history records were read or copied by this worker.
