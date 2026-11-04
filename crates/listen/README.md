# Heimdall listen engine

**Domain: transcription.** Silero VAD → Parakeet ASR (Metal plans) → Smart Turn, per source, on one inference thread,
with vocabulary biasing and an English/Polish language prior (`docs/listen-vocabulary.md`). **Interface:** `ListenEngine`, `ListenWorker`/`WorkerEvent`, `ListenEvent`, `asr::Parakeet`,
`onnx::{Silero, VadState}`, `vocabulary`. No workspace dependencies.
**Testing:** `cargo nextest run -p listen`; about a dozen tests take 0.5–5 s (golden transcripts on fixtures, needs
`models/`). Three ignored GPU-fault tests and one ignored lab run, `tests/latency_lab.rs` (Smart Turn and final-decode
time after an endpoint, `HEIMDALL_LISTEN_LAB=1`). Measurement scripts: `lab/listen/`. `timing` records when each final's
endpoint, Smart Turn and decode happened (host clock) for the app's latency line (`lab/latency/MAP.md`).

Rust-owned Silero → Parakeet Ultra (`FluidInference/parakeet-ultra-coreml`) → Smart Turn v3.2. No Swift bridge, Vella runtime,
FluidAudio, Python or running server is used by the app. `ort` links its CPU
runtime into the executable. Models are local and inference never accesses the
network. macOS/Apple Silicon is required for Parakeet.

## Integration

`VerificationWorker`/`VerificationClient` (`verification.rs`) is a separate CPU Parakeet decoder for a sidecar voice's word
check: one request at a time, at most 15 s of audio each, refusing with "verification queue full" or "verification
deadline".

Live capture uses `ListenWorker::spawn(QueueConfig::default(), factory)`. The
factory loads `Parakeet` and constructs `ListenEngine` **on the worker thread**;
Core ML models never cross threads and retain their non-`Send`/non-`Sync` ownership.
`ListenWorker::push` performs bounded admission, not inference. A mutex protects
only short queue operations; no inference or consumer wait holds it. Drain
`pop_event` independently. `shutdown` stops admission, drains retained audio and
flushes; only shutdown waits for outstanding inference. Never call the synchronous
`ListenEngine::push` from a capture callback; it remains the offline/core path.

Defaults: at most 32 sources; each holds at most 16 PCM chunks of at most 1,600
samples (100 ms) and 32 events. Sources are scheduled round-robin, so continuous
Discord/browser speech cannot monopolize the mic. Oversize chunks and excess
source IDs return an error without enqueueing. Input overflow drops the oldest
queued PCM, returns its interval in `PushOutcome::DroppedOldest`, and delivers a
`WorkerEvent::Gap` before retained audio. Gaps coalesce per source. They reset
VAD/ASR state instead of joining across missing samples. Consecutive partials may
be replaced by the latest; final/segment events are never silently coalesced.
Output overflow discards that source's queued deliveries, reports `OutputOverflow`
with an interval and resets its acoustic state. Consumers must clear provisional
text on gaps/errors and keep already archived audio for later recovery. Source
flushes close admission until their final events have drained. Startup failures
are explicit worker errors. Capture continues while model startup is in progress,
with the same bounded queue and overflow behavior.

Adapt capture's `AudioChunk` into listen's borrowed `AudioChunk`: source is
`source_id.0`, timestamp is `host_ns`, samples are already mono 16 kHz. Capture's
source IDs include generation. Maintain an adapter generation counter and increment
it when capture reports `discontinuity`/`Gap`; pass that counter as listen's
`generation`. `SourceRemoved` calls `remove_source`. A global capture gap applies
to every active source. Host times are monotonic nanoseconds, never UNIX time or
raw mach ticks. Capture handles stream-clock drift before this boundary.

`push` returns speech start/end, partial and final events. An inference error
returns `Err` and invalidates that source's unfinished utterance; its next arrival
emits `Discontinuity` and starts fresh VAD/ASR state. Consumers must handle the error
and clear provisional text on discontinuity. Other sources keep their state.
Each source has its own
Silero state, context, pending 512-sample accumulator and transcript lifecycle.
`stable_words` is a provisional consensus hint: three matching decodes and
three seconds of age. It may decrease and those words/times may still change;
consumers must replace the current tail, including confirmed words. Later
corrections and final decoding own every word still in the acoustic window.
Only `TranscriptSegment` words are committed: append each segment once. A segment
is emitted during ongoing speech when a full 15-second window rolls; it is **not**
a pause or turn completion. `PartialTranscript`/`FinalUtterance` now carry only the
remaining tail. Keep the preceding segments when assembling the full transcript.
A 640-ms word-alignment margin and ~2-second acoustic overlap prevent seam-word
duplication. Internal audio/word/consensus storage and partial payloads remain
bounded during indefinite speech. A final's start/end refer to its tail, not the
original turn start after prior segments have been emitted. Empty final decodes
are suppressed; VAD speech events alone do not establish recognized words.

Every source, including capture IDs `mic:*` and offline `mic`, defaults to `PartialPolicy::FinalOnly`.
There are no repeated live Parakeet decodes: a finished turn decodes once; ongoing speech only decodes at the
15-second rollover so storage stays bounded. The transcript updates per finished sentence, with intermediate
segments for long speech. The screen ring's default is now 20 seconds to preserve the trigger-word frame until
that decode (see `docs/screen.md`). Explicit lab overrides still support `Responsive` (384-ms first partial,
then adaptive 2.5/3-second cadence) and `Slow { interval_samples }`. All time/sample settings use 16 kHz.

`ListenWorker::spawn_configurable` runs backend changes on the inference owner between batches. Candidates are
built before replacing the affected recognizer/turn detector; source PCM, VAD state, word/timestamp state and
archive hook stay in place. Failed candidates keep the old instance and report a reload result. Capture admission
continues through its bounded queue while a candidate loads. `docs/settings.md` summarises the backend A/B and smoke results.

Capture discontinuities travel with their PCM through the worker queue. Captured pre-gap audio
is finalized before the gap, and the next packet starts a fresh acoustic epoch; queued finals and
segments are retained. Source disabling still discards inference explicitly. Discovery and tap
recovery diagnostics are archived independently and do not reset healthy streams.

This is buffered partial decoding, not a causal encoder cache. Last-eight-second
Smart Turn audio is maintained independently of ASR window rolls.

Defaults: VAD start 0.4 / continue 0.35, one-second preroll, candidate pause
300 ms, semantic completion >0.5, hard silence fallback 1.5 s. These endpoint
values are engine defaults, not main-agent dispatch policy. Word times derive
from TDT frame emission/duration with FluidAudio's one-frame correction (80 ms
resolution); they are acoustic estimates, not manually verified alignments.
Final utterance intervals contain their words; speech-end events retain the VAD
boundary when its estimate differs from acoustic alignment by a few frames.

Smart Turn exactly keeps the last 8 seconds, left-pads first, normalizes waveform,
uses periodic Hann / 400-point FFT / 160 hop / reflection centering / Slaney 80
mel bands / log10 floor / max−8 clipping / `(log+4)/4`, and drops the final frame.
The ONNX probability is already sigmoid. CPU sessions use one thread, no inherited
accelerator providers and no spinning workers.

## Agent-operated verification

`python3 crates/listen/scripts/fetch_models.py` verifies SHA-256 for every file in
`models.lock.json`; all URLs pin immutable revisions. ~643 MB of model bytes,
under the authorized 1.5 GB. Corrupt/missing assets are fetched through a temporary
file and only installed after their digest matches. The app does not fetch.

`CARGO_TARGET_DIR=target/listen-opt cargo test -p listen` exercises DSP reference tensors, source state separation, endpoint
fallback/restarts, window bounds, exact whole/lexical stream goldens, revisable confirmation,
seam alignment, EOF near the window cap, startup/input/output failures, and
five-minute continuous interleaved three-source storage/overflow checks.
Tests need the pinned models and fail clearly when they are missing. Tests open
only local WAV files and never initialize audio capture or request permissions.

`CARGO_TARGET_DIR=target/listen-opt cargo run --release -p listen --bin heimdall-listen-file -- <wav>` runs the same
engine and prints JSON events, including final word timestamps and completion
probability. `--whole` runs the ASR reference path without VAD/turn/partials;
`--realtime` replays 20-ms arrivals for first-partial wall-clock latency. `--models`
selects a local model root. The built replay binary is
`target/listen-opt/release/heimdall-listen-file`; each file path becomes a `mic:` event
source ID, selecting the normal final-only mic policy, and replay timestamps start at zero. A harness can instead call `push`
with its original host-clock epoch and then `flush_source` at EOF.
The placement binary prints Core ML compute-plan
assignments; these are planned execution devices, not a hardware power trace.

Public fixtures are a fixed four-clip EN/PL FLEURS set plus a synthetic bilingual
concatenation and a continuous English seam check. See
`tests/fixtures/manifest.json` for pinned origins, hashes and
references, `golden.json` for whole-ASR output and `engine-golden.json` for default
stream output. WER counts case-insensitive Unicode words, removing punctuation;
it does not silently normalize digits to spelled numbers. Generation of DSP
fixtures uses the pinned Transformers 4.57.6 numpy frontend in
`scripts/reference_features.py`, without downloading neural models. Runtime Rust
has no Python dependency.

Historical measurements and gaps are in the listen section of `lab/history/STATE.md`.

## Resident ASR probe (agents)

`heimdall-listen-opt-probe` is also the `--asr-probe` of the personal edition's voice-bank fills. It keeps models resident and accepts JSON-lines requests
with `path` (mono 16 kHz WAV) or `pcm_paths` (Vella raw Float32 segments), optional
`source`, `whole`, and `realtime`. Encoder calls are counted at the actual selected
encoder prediction. ASR seconds cover preprocessing/encoder/decoder together;
compute share uses unpaced engine work, never deliberate replay sleeps. A fixed
0.35 VAD threshold supplies the same speech-time denominator in both arms.
`lab/listen/scripts/measure_efficiency.py` runs order-balanced ABBA replays against a saved
baseline binary, whole-file ASR, and the final-only policy, then real-time latency
and six short rootless macmon power brackets. Use `CARGO_TARGET_DIR=target/listen-opt`.
All manifests, private silver references and raw receipts stay under ignored
`models/reference/listen-opt/`; private audio is read in place, never copied to git.
The public conversational manifest is `tests/fixtures/efficiency-public.json`;
these additional clips are local-only downloads, not repository fixture audio.
Private WER is agreement with Vella's saved transcript, not human ground truth.
Whole-machine power under background work is reported with the idle bracket and
spread; it is not a battery-life or per-process power claim. Timing excludes model
loading and transcript serialization; actual live event-consumer latency remains
an app integration check.


## FP16 shader backends (agents)

Parakeet defaults to the Rust Metal encoder, two-layer prediction LSTM and joint
network (f340d47). `HEIMDALL_PARAKEET_BACKEND=coreml` explicitly selects the
Core ML/ANE rollback. The CPU Core ML
preprocessor supplies identical mel features to both paths. Rust owns the TDT
policy; Metal executes its token/duration decisions and recurrent-state commits
in resident blocks of 32 steps, with one CPU completion wait per block.
`HEIMDALL_METAL_TDT=step` restores the earlier host-controlled Metal decoder for
diagnosis. Word timestamps, window alignment, confirmation and partial policies
remain Rust-owned.
The model is shared across sources; only their recurrent/utterance state differs.

`HEIMDALL_SMART_TURN_BACKEND=cpu|metal` selects the tuned turn backend for offline tools (CPU there); the app defaults to Metal: 9.2 vs 21.4 ms and 0.030 vs 0.110 J per decision (CPU + GPU), see `lab/history/docs/optimisation/small-models-2026-10-13.md`. Metal requires
the tuned checkpoint's verified `.metal.json`/`.metal.bin` pair and Apple9 GPU
or newer with Metal 4.0; it does not silently execute a stock model.
Derive the private pair with `scripts/compile_turn_plan.py` using conversion-only
ONNX 1.23.2 and NumPy. Bundle those files alongside the private checkpoint; they
contain private weights and must never enter Git. Both original and derived
weights are checked by SHA-256. Runtime uses no Python, MLX, Swift, or Vella.

Silero runs as a native NEON kernel (`src/silero.rs`) over the pinned ONNX's
16-kHz constants: 0.03 vs 0.11 ms and 0.23 vs 0.49 mJ per paced frame against ONNX
Runtime, zero 0.35/0.4 flips over 28.5 h of eval audio (ORT stays the test
reference). The worker drains one already-ready chunk per source; eligible
independent VAD frames are evaluated together, each with its own `[h, c]` state. It adds no wait. Discontinuities, duplicate sources,
and multi-frame chunks retain the serial path. The six-stream recurrent check
uses distinct signals, rather than duplicating a single stream. Already-ready
partial/final ASR inputs from eligible single-frame arrivals also share a Metal
encoder batch (2–32 streams). Cached activation plans reuse one resident weight
bank and queue; no per-stream model copies or extra wait for batch admission.
Seams, EOF flushes, discontinuities and multi-frame intake retain serial decoding.

Build/test with `CARGO_TARGET_DIR=target/shader-listen`. `lab/listen/scripts/measure_shaders.py`
keeps both ASR paths resident and uses matched ABBA replays of all six pinned EN/PL
fixtures, then separate rootless macmon brackets for ASR, tuned turn, and six-stream
CPU VAD batching. Speech-time denominators, binary/source hashes, per-bracket
power spread, idle drift, raw samples, and outputs remain under ignored
`models/reference/shader-listen/`. SoC joules are derived after interpolated idle
subtraction on a busy Mac; accelerator joules are reported separately. These are
short-window comparisons, not per-process or battery-life measurements.

`heimdall-listen-shader-probe` checks small GEMM, convolution, layout, and reduction
kernels via the test suite; `encoder-layers` reuses all 24 saved Core ML layer
outputs in one GPU forward; `decoder` reuses teacher-forced decoder references.
`turn-static` compares every graph-operation output to saved CPU ONNX tensors.
The conversion-only reference ONNX exposes intermediate outputs without changing
its operations. `turn` checks full clips and quarter/half prefixes, including
incomplete speech. Captures are local-only and reused, never regenerated to
hide a regression. Vella's vendored tensor GEMM source and MIT notice are under
`src/metal/`; its attribution and exact modifications are in `THIRD_PARTY.md`.

`encoder-isolated` teacher-forces each of the 24 layers with its saved preceding
Core ML activation. `encoder-profile` and `decoder-profile` capture per-dispatch
GPU stage timestamps (resolved nanoseconds, with pass-boundary overhead).
`encoder-batch` compares three/six distinct signals and lengths against serial
outputs and checks weight-buffer identity. Bias/activation/convolution-layout
fusion retains each original FP16 rounding boundary.
`lab/listen/scripts/check_encoder_mlx.py` independently evaluates the pinned MIL in the
existing read-only Vella MLX lab interpreter; this is a conversion diagnostic,
not Vella's checkpoint runtime or a Heimdall dependency. A fresh ANE capture and
Core ML CPU output are separate references, never substituted silently.
`lab/listen/scripts/qualify_encoder.py` runs resident ABBA+BAAB four-call brackets on the
six public clips, then ABAB macmon energy brackets with saved `ps` snapshots.
It rejects competing builds/benchmarks and contended idle using the family
energy gate. `--skip-energy --out <receipt-directory>` preserves a previous
power run while checking the final binary's task path. Numerical/timestamp gaps
and unqualified energy remain documented in `lab/history/STATE.md`; the later coordinator
selection in f340d47 makes Parakeet Metal the default without a new energy claim.

## GPU recovery

Every command buffer is synchronously checked before readback. Typed `GpuError`
includes missing buffers and Metal status/NSError codes. Parakeet retries a complete
window/batch after dropping and rebuilding all Metal resources, then uses Core ML
for 30 seconds after a second GPU fault. A partial TDT result or successful batch
prefix never escapes a failed operation. Smart Turn uses the same policy with CPU
ONNX fallback; Silero is CPU-only. Explicit CPU/Core ML overrides remain there.
`gpu_recovery::gpu_status()` aggregates independent fallback owners without clearing
their status when a different model succeeds. The app publishes changes through
`UiUpdate::ComponentStatus`. Listen and TTS use embedded precise Metal 4 metallibs;
no runtime shader compiler or source-tree resource path is required.

The opt-in `gpu_fault` tests exercise real offline model completion boundaries,
missing command buffers, late TDT loss, six-stream batches, fallback/backoff and
GPU restoration. They simulate loss at a completed command, not a physical driver
crash. The release-candidate workspace receipt retains their actual result.

Vella's borrowed dictation uses `ListenWorker::transcribe_external` (docs/vella.md, Channel 2): at most ten minutes,
one decoding and one waiting, decoded through `long::LongTranscription` (the live window/roll/seam arithmetic) one
15-s window per turn, alternating with draining the live backlog queued during that window. Results arrive on the
returned receiver, never as a `WorkerEvent`, and never touch a source's stream.

Background overlap correction uses `ListenWorker::transcribe_correction`, a bounded
two-request lane on the same recognizer. Command side requests win first; all
queued live source work drains before a correction starts. A decode already
running is synchronous and cannot be preempted. Correction results are side
transcripts and never modify a source's streaming state.
