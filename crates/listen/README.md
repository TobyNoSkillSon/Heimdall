# listen

Heimdall's transcription engine: Silero VAD → Parakeet TDT ASR (Metal) → Smart Turn, per source, on one inference
thread, with name biasing and a language prior. Local models only; inference never touches the network. macOS on
Apple Silicon is required for Parakeet. No workspace dependencies and no Python, Swift or server at run time.

Read this when: you are changing how audio becomes words, when a turn ends, the listen worker's queues, the GPU
fallback, or the models the engine loads.
Code: `crates/listen/src/`.

## Files and entry points

| File | What it holds |
| --- | --- |
| `src/lib.rs` | public surface: `ListenEngine`, `ListenEvent`, `AudioChunk`, `Recognizer`, `Transcript`, `Word` |
| `src/engine.rs` | per-source VAD, ASR windows, Smart Turn checks, `ListenConfig`, `PartialPolicy`, `OWNER_TURN_CHECKS` |
| `src/worker.rs` | `ListenWorker` (`spawn`, `spawn_reloadable`, `spawn_configurable`), `QueueConfig`, `WorkerEvent`, side, correction and external lanes |
| `src/asr.rs` | `asr::Parakeet` (`load_backend`, `set_vocabulary`), TDT decoding, word times |
| `src/metal/` | Metal encoder, prediction and joint kernels and plans; vendored tensor GEMM (MIT, see `THIRD_PARTY.md`) |
| `src/coreml.rs` | Core ML recovery and rollback backend |
| `src/onnx.rs`, `src/silero.rs` | `onnx::{Silero, SmartTurn, VadState}`; Silero as a native NEON kernel (ORT stays the test reference) |
| `src/features.rs` | Smart Turn's Whisper-style log-mel frontend |
| `src/vocabulary.rs` | name biasing and the language prior ([listen-vocabulary.md](../../docs/listen-vocabulary.md)) |
| `src/long.rs` | `LongTranscription`: 15 s windows, roll and seam arithmetic shared by live and long decodes |
| `src/onset.rs` | moves word starts forward onto voiced audio (`VoicedHistory`) |
| `src/timing.rs` | when each final's endpoint, Smart Turn and decode happened (host clock), for the app's latency line |
| `src/gpu_recovery.rs` | GPU fault recovery shared with `crates/speak` |
| `src/verification.rs` | `VerificationWorker`/`VerificationClient`: a separate CPU decoder for the personal edition's word check |
| `src/bin/heimdall-listen-file.rs` | replay a WAV through the engine and print JSON events |
| `src/bin/heimdall-listen-opt-probe.rs`, `heimdall-listen-shader-probe.rs`, `heimdall-small-model-probe.rs` | resident probes for A/B runs and kernel checks |
| `crates/listen/scripts/fetch_models.py`, `crates/listen/scripts/models.lock.json` | pinned model fetch with SHA-256 checks |

## Contracts for callers

- Live capture uses `ListenWorker::spawn(QueueConfig::default(), factory)`. The factory loads `Parakeet` and builds
  `ListenEngine` on the worker thread; Core ML models never cross threads. `push` only admits audio; drain
  `pop_event` independently. `shutdown` stops admission, drains and flushes. Never call the synchronous
  `ListenEngine::push` from a capture callback.
- `QueueConfig::default()`: at most 32 sources, each at most 16 chunks of at most 1,600 samples (100 ms) and 32
  events. Sources are scheduled round-robin. Oversize chunks and extra sources are refused without queueing.
- Input overflow drops the oldest PCM (`PushOutcome::DroppedOldest`) and delivers `WorkerEvent::Gap` first. Output
  overflow drops that source's queued deliveries and reports `OutputOverflow`. Both reset acoustic state; consumers
  clear provisional text on gaps, errors and `Discontinuity`, and keep archived audio for recovery.
- Adapting capture's `AudioChunk`: source is `source_id.0`, timestamp is `host_ns` (monotonic nanoseconds), samples
  are mono 16 kHz. Bump a generation counter on every capture discontinuity or gap and pass it as `generation`;
  `SourceRemoved` calls `remove_source`. Capture handles clock drift before this boundary ([capture.md](../../docs/capture.md)).
- An inference error invalidates that source's unfinished utterance; its next audio starts with `Discontinuity`.
  Other sources keep their state.
- Only `TranscriptSegment` words are committed; append each once. A segment comes when a full 15 s window rolls
  during speech; it is not a turn end. `PartialTranscript` and `FinalUtterance` carry only the remaining tail.
  `stable_words` is a hint (three matching decodes, three seconds old) and may still change.
- Every source defaults to `PartialPolicy::FinalOnly`: a finished turn decodes once, long speech decodes at each 15 s
  roll. `Responsive` (first partial at 384 ms, then every 2.5–3 s) is a lab override.
- `spawn_configurable` swaps backends between batches on the inference thread; a failed candidate keeps the old one.
- `transcribe_side` (command requests) and `transcribe_correction` (bounded background lane) share the recognizer;
  queued live work drains first and results never touch a source's stream. `transcribe_external` serves Vella's
  borrowed dictation: at most `EXTERNAL_MAX_SAMPLES` (10 minutes), `EXTERNAL_SLOTS` (2: one decoding, one waiting),
  one 15 s window per turn alternating with live work ([vella.md](../../docs/vella.md)).
- `VerificationClient`: one request at a time, at most 15 s of audio, refusing with "verification queue full" or
  "verification deadline".

## Turn ends

- `ListenConfig::default()`: VAD start 0.4, continue 0.35, one-second preroll, Smart Turn asked after a 300 ms pause
  over 0.5, silence ceiling 1.5 s, at most 32 sources.
- With a tuned turn policy present (`apply_owner_policy`), every source keeps the policy's one check (1.5 s, over 0.5,
  ceiling 2.5 s), and the user's microphones (`mic*`, `phone:*`) are also asked at 300 ms and 1 s over 0.95
  (`OWNER_TURN_CHECKS`). `SpeechEnd` comes at the first check. Resumed speech restarts the checks.
- Smart Turn keeps the last 8 s of audio, independent of ASR window rolls. CPU ONNX sessions use one thread and no
  spinning.

## Backends and GPU recovery

- `HEIMDALL_PARAKEET_BACKEND`: `metal` (default) or `coreml`. `HEIMDALL_METAL_TDT=step` restores the host-driven Metal
  decoder for diagnosis. `HEIMDALL_SMART_TURN_BACKEND`: `cpu` (default for offline tools) or `metal`. The app passes
  its own choice from the `parakeet_backend` and `turn_backend` settings ([settings.md](../../docs/settings.md)).
- Metal Smart Turn needs the tuned checkpoint's verified `.metal.json`/`.metal.bin` pair (derived with
  `crates/listen/scripts/compile_turn_plan.py`; private weights, never in Git) and an Apple9 GPU or newer.
- Every command buffer is checked before readback. Parakeet retries a whole window or batch after rebuilding all Metal
  resources, then uses Core ML for 30 s after a second fault. Smart Turn falls back to CPU ONNX the same way; Silero
  is CPU only. `gpu_recovery::gpu_status()` reports fallbacks; the app shows them through `UiUpdate::ComponentStatus`
  ([resilience.md](../../docs/resilience.md)).

## Tests

1. Fetch the pinned models (about 643 MB, into `models/`): `python3 crates/listen/scripts/fetch_models.py`.
2. Run `cargo test -p listen`. Golden tests need the models and fail clearly without them; tests open only local WAV
   files and never start audio capture.
3. Opt-in, ignored by default: three `gpu_fault_*` tests (`src/asr.rs`, `src/onnx.rs`; real models, simulated loss at
   a completed command) and `tests/latency_lab.rs` (`HEIMDALL_LISTEN_LAB=1`). Run with
   `cargo test -p listen -- --ignored <name>`.

- Fixtures: `tests/fixtures/manifest.json` pins four EN/PL FLEURS clips, a bilingual concatenation and a continuous
  English seam check; `golden.json` is whole-ASR output, `engine-golden.json` default stream output. DSP fixtures come
  from `crates/listen/scripts/reference_features.py` (pinned Transformers frontend, no neural models).
- Replay: `cargo run --release -p listen --bin heimdall-listen-file -- <wav>` prints JSON events; `--whole` runs ASR
  only, `--realtime` paces 20 ms arrivals, `--models <dir>` picks a model root. Each file becomes a `mic:` source.
- Measurement scripts live in `lab/listen/scripts/`.

## See also

- [listen-vocabulary.md](../../docs/listen-vocabulary.md): name biasing and the language prior
- [capture.md](../../docs/capture.md): the audio sources that feed the worker
- [models.md](../../docs/models.md): the speech models and their downloads
- [inference-engine.md](../../docs/inference-engine.md): the built-in local language-model engine
- [vella.md](../../docs/vella.md): dictation through `transcribe_external`
- [speakers.md](../../docs/speakers.md): who said each line
