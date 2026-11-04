# Heimdall speakers

**Domain: transcription (who is speaking).** Diarization, voice identity and passive name suggestions (vendored
ggml/opus, Metal). **Interface:** `diar`, `identity::EnrollmentStore`, `naming::{NamingFlow, SpeakerEvent, TrackId}`,
`embedding`. No workspace dependencies.
**Testing:** `cargo nextest run -p speakers`; two tests take ~15 s (native model streams). Six ignored lab tests
(GPU faults, held-out accuracy). Bake-off and calibration scripts: `lab/speakers/scripts/`.

Code map for the speakers SDK: native diarization, voice profiles and naming events. How naming, guests and
Heimdall's own voice behave in the app: [docs/speakers.md](../../docs/speakers.md).

Live multi-speaker inference uses `CallScopedSpeakers` and the bundled
`heimdall-speakers-worker`. The host activates it when a multi-speaker source
becomes active, feeds mono 16 kHz PCM in chunks of at most one second, supplies
its VAD speech flag, and calls `expire(Instant::now())` from its timer even when
capture stops. `QUIET_TIMEOUT` (120 s since 21 Oct; 15 s restarted a Discord session eight times in 57 min on
ordinary pauses) without speech finishes the pipeline and kills and reaps its owned child. Activation after expiry needs a new session ID and
correct audio origin. Run this synchronous interface on the dedicated
speaker thread. Bundle the helper beside the app; pass its explicit path. There
is no Python dependency in the app. Capture/app wiring is outside this crate.

Naming state stays in the parent, including coded voices and saved profiles. Collection excludes predicted overlap, trims boundaries, and waits for twelve
seconds of clean speech: two distinct six-second embeddings. Confirmation saves
both atomically. A six-second query matches the maximum cosine similarity to an
identity's stored exemplars; the default cutoff is 0.455542. Existing profiles
are preserved. Legacy one-window enrollment remains readable but does not carry
the measured two-window guarantee. `Diarizer`/`SpeakerPipeline` remain available
for offline tests; using them directly in the host retains library-global caches.

## Live strategy selection

`LiveStrategy::default()` remains `Nemotron` for a mixed 2–6-person call.
`LiveStrategy::RUNNER_UP` is `Sortformer`, with a hard four-person ceiling:
call `validate_participants(count)` before selecting it. The API accepts explicit
`nemotron`, `sortformer`, `embedding`, and `hybrid` choices; missing weights,
unsupported platforms and unavailable choices return errors rather than fallback.
`CallScopedSpeakers::set_strategy` works only in standby. Activation sends the
selection and confirmed enrollment profiles to its helper; the helper returns its
actual track count and frame cadence. Rebuild/bundle parent and helper together.

All four implementations use the existing `Diarization` trait and the same
`SpeakerPipeline`/`NamingFlow`. Nemotron keeps the original Q8 native path.
Sortformer v2.1 runs the pinned FP16 compiled Core ML artifact with CPU/GPU,
causal 128-bin mel features, a 0.48-second chunk and 0.56-second right context.
It exposes four tracks at 80 ms cadence, versus Nemotron's eight at 10 ms.
Only confirmed chunk output is committed; source cache compression is checked
against independently generated Swift fixtures and mel against PyTorch.

The experimental embedding strategy uses Silero speech detection, 3-second
windows every 0.5 seconds, B6 embeddings and bounded online cosine clusters.
Confirmed enrolled exemplars remain immutable; unmatched windows create new
anonymous tracks until the eight-track limit, then remain unlabeled. Windows
with at least 0.5 seconds of detected speech repeat clean samples to B6's six-
second input. The 0.43815252 cutoff comes from the earlier three-second identity
calibration; an anonymous calibration pilot preferred 3 seconds over 1.5/2.
Mixed windows and overlap remain its main limitation. This experimental bank
accepts at most eight enrolled identities and returns an error for larger stores. The tested hybrid replaces
Silero's speech mask with Nemotron's predicted exclusive speech, then uses the
same clustering. It is one hybrid, not a search over all change-point methods.

The runner-up weights belong at `models/speakers/Sortformer_v2.1.mlmodelc`.
Agent-owned `scripts/fetch_bakeoff.py` verifies every pinned file; the compiled
artifact totals 240,564,360 bytes. Detailed evaluation receipts, source audio and
manual AMI annotations remain in ignored `models/speakers/bakeoff/`. Community-1
upstream returned HTTP 401/GatedRepoError, but the pinned public Core ML export
was accessible and was evaluated separately with the official Python host and
a causal rolling prototype. Offline scores remain quality references.

## Bake-off reproduction

The agent-owned `scripts/bakeoff.py` prepares the fixed corpus, replays native
strategies and scores actual final Rust names against manual AMI word timestamps.
`community_eval.py` evaluates the verified public Community-1 export with its
official pyannote.audio host: 30-second rolling windows every five seconds,
five-second hold, first output after ten seconds of audio. Persistent centroid
matching uses cosine 0.4, selected on the two calibration clips' raw named-word
score; held-out clips never select parameters. This is a QA prototype, not a
Python dependency in the app. Offline quality is reported separately.

`release_performance.py` measures two reversed-order rounds on the same
480-second AMI input and checks release/debug segment and final-name equivalence.
It also exercises the actual helper at real-time pace with idle CPU brackets and
checks reaping. Every performance receipt records `build_profile=release`;
debug or mixed-profile quality summaries expose no timing/RTF. The default
and alternatives run with the same actual Rust naming protocol. `crosscheck_der.py`
checks every completed receipt against pyannote.metrics, zero collar with overlap
included. Detailed tables, denominators and gaps are in the current lab/history/STATE.md
speaker section; manual-word attribution is not recognition accuracy.

## Reference and native build

The checkpoint stores its Hann window in bf16. Regenerating a float32 Hann
window was a frontend bug; `native/nemotron3_window.h` contains the checkpoint's
exact promoted coefficients. Native source revisions are in `vendor/pins.json`;
local patches are the window override, opt-in feature dump, and debug-gated
Rust projection hook/command-buffer timing. Flags/compiler
are in `native/build-lock.json`, Apple clang signature is enforced, and the
workspace Cargo.lock pins Rust dependencies. Host C/C++ uses no fast math; Metal
uses the pinned ggml runtime's existing compilation options. Builds do not fetch
native source. Tested Rust 1.99.0, CMake 4.4.3, Apple clang 21.0.0, macOS arm64.

Agent verification uses `CARGO_TARGET_DIR=target/diar`. `cargo test -p
speakers` runs unit/fixture and available local native tests; fixture/real-model
tests explicitly print a skip if their ignored assets are missing. Run
`cargo test -p speakers --test identity_v2 -- --ignored --nocapture` for the
384-query identity replay and `cargo test -p speakers --test review_native
fresh_streams_and_feed_partition_preserve_predictions -- --ignored --nocapture`
for interleaved stream/feed partition equivalence. Strict clippy covers all targets.

`reference.lock.json` under scripts pins NeMo Speech, lhotse, Python, Torch and
checkpoint. Use a throwaway `ref/venv` to run `scripts/reference_parity.py` on
`models/speakers/ami-480.wav`, first without `--gguf`, then with the shipped Q8
GGUF. It emits mel features, frame probabilities and threshold-0.5 segments.
The CPU FP32 reference is explicitly distinct from the publisher's NeMo bf16 run.
The file runner accepts optional sixth argument for raw f32 probabilities;
`HEIMDALL_DIAR_FEATURE_DUMP` appends row-major 128-bin f32 mel features to the
specified path (use a fresh file). `heimdall-speakers-score --der RTTM JSON 480`
scores the exact cropped segments with zero collar, overlap and optimal mapping.
The first-chunk golden fixture checks the actual native file-runner path.

## Measured 11 October 2026

Apple M5 Max, busy shared Mac, native Release/Rust debug. AMI ES2004a Array1-01
first 480 seconds, forced-alignment reference, zero collar/overlap included.
Opus VOIP mono 16 kHz, 20 ms packets, lookahead removed, no loss/jitter.

| DER (%) | Before native | Corrected native | CPU NeMo reference |
| --- | ---: | ---: | ---: |
| Raw PCM | 41.619 | 40.253 | 40.218 (same Q8 tensors) |
| Opus 24 kb/s | 29.942 | 29.820 | unmeasured |
| Opus 64 kb/s | 35.622 | 40.572 | unmeasured |

Original checkpoint CPU FP32 raw DER is 42.017%. Publisher full AMI SDM bf16
12.95% covers another dataset extent/precision and is not a same-crop baseline.
Full-crop frontend RMS error fell from 0.0031088 to 0.00000227, max error from
0.82519 to 0.00047684. Same-Q8 probabilities have RMS error 0.011424, max 0.3580
(late streaming-cache numerical drift), and 99.9112% threshold-bit agreement.
DER differs by 0.0343 percentage points: approximate numerical/segment parity,
not bit-exact parity. Raw RTF is 0.05651 (1.0 means real time). The remaining
high crop DER and the 64 kb/s regression are unresolved quality limitations;
this does not establish release readiness or reproduce the full AMI benchmark.

Identity preparation/encoding/reporting is agent-run `scripts/calibrate_identity.py`.
It verifies pinned model/annotation hashes, keeps source manifests and full
results under ignored `models/speakers/identity-v2/`, and downloads 558,288,259
bytes. AMI calibration has seven qualifying people in ES2002/IS1000 a/b; test has
eight different people in ES2004/IS1009 a/b. Enrollment uses meeting a and query
uses meeting b. Calibration/test people and sessions are disjoint. Evaluate
2/3/6-second crops and exactly 1/2/4 distinct raw-source exemplars. Calibration
alone selects six seconds, two exemplars (tie retains existing two-source policy),
cutoff 0.455542, FAR/FRR 2.0833% each on 336 genuine/336 max-impostor trials.
Codec copies are correlated, not additional independent source windows.

AMI test balanced accuracy is 99.21875% (127/128 in each raw/24/64 condition),
95% speaker-cluster bootstrap interval 97.65625–100% (10,000 replicates, seed
20261011). Matched three-second/two-exemplar 24 kb/s baseline is 82.8125%,
interval 57.8125–97.65625%. Previous tiny four-person protocol's 58.6% balanced
accuracy/40-of-55 result is historical, not a matched before/after denominator.
Rust replay reproduces all 384 outcomes; embedding max delta is 3.13e-7 versus
Python ORT. Unknown rejection is 99.21875/97.65625/99.21875% for raw/24/64.
These intervals describe eight sampled speakers, not broad population certainty.

VoxConverse external check includes eight clips, 24 clip-local identities; enroll
and query source intervals are disjoint within each clip. It is not cross-session
identity validation. Balanced accuracy raw/24/64 is 99.40476/99.70238/99.40476%;
24 kb/s interval is 99.10714–100%. Unknown rejection is only 89.0566% at raw/24
and 89.4340% at 64: domain false accepts remain. Reference-clean oracle windows
make these identity figures distinct from end-to-end diarized naming accuracy.
No B6 codec accuracy loss appeared at six seconds, so ReDimNet2+ was not swapped
in merely to repair short-window/source-coverage errors.

In-process model destruction retained 621.6 MiB (earlier review: 610 MiB), even
with autorelease pools and allocator pressure. The call helper's parent+child
current RSS on a 120-second replay is 8.97 MiB before, 921.70 MiB during, and
22.52 MiB after; child RSS becomes zero and PID is reaped. The parent retains
its offline input buffer/naming allocations. Reactivation and killed-worker recovery pass. RSS excludes external GPU drivers.
Receipts are in ignored `ref/`.


## Experimental speaker shaders

The default remains Q8 GGML Metal for Nemotron and single-threaded CPU ORT for
ReDimNet. The Nemotron 3 checkpoint uses a RoPE transformer, rather than a
conformer. `HEIMDALL_SPEAKERS_METAL=nemotron`, `redim`, or `1` selects the
respective experimental Rust/Metal path; unset or `0` selects the baseline.
Selection is inherited by the existing call helper. The app's settings field `speakers_backend` (`default`,
`redimnet`, `nemotron`, `all`) chooses among these. No model, GPU queue or
converted weight cache is retained in the parent; expiry still kills and reaps
the helper. Nemotron clones share the owning model; a second independent shader
model in one process returns an error. ReDimNet operators share one queue and
weight cache per session. No batched Nemotron API is introduced.

Nemotron's immutable Q8/F16 projections encode into GGML's existing command
encoder; attention, scheduling, streaming caches and CPU feature extraction stay
on their existing paths. Large projections use half operands and float
accumulators. Short projections preserve FP32 activations and reconstruct Q8
products from two half planes to retain the original arithmetic boundary.
ReDimNet replaces 169 backbone convolutions with 73 single operators and 24
four-convolution blocks: 97 command submissions per embedding. Blocks fuse
ReLU/batch normalization, keep their intermediates on the GPU, and borrow the
ORT arena without copying. Remaining ONNX operators stay on the CPU. The pinned
ONNX hash is checked before a bounded in-memory graph rewrite; there is no
runtime export or sidecar model. Metal 4 tensor kernels are compiled offline,
embedded in the Rust binary, and use no fast math. Vella supplied prior art,
without a runtime dependency.

Static fixtures capture CPU ORT convolutions/blocks and current GGML Metal
projections. Agent-owned `scripts/shader_speakers.py` uses the existing held-out
bake-off corpus, frozen QA binaries, per-receipt binary hashes, the original DER
and named-word definitions, and the 400-window AMI cross-session and 843-window VoxConverse identity replays.
Performance uses reversed before/after pairs, processing timestamps and rootless
macmon. SoC energy is whole-device energy on a busy shared Mac; GPU time sums
command-buffer intervals and can include overlapping execution. Helper memory
reports both RSS and Darwin physical footprint. Qualification receipts stay in
ignored `models/speakers/shader-speakers/`; final results and promotion decisions
are recorded in lab/history/STATE.md. These paths are candidates until both quality and
speed/energy gates pass.


The 11 October candidate did not qualify for promotion: Nemotron's AMI 24 kb/s
DER regressed and the combined call path took more time/energy. ReDimNet retained
all 1,179 held-out query outcomes across existing and regenerated banks (AMI and
clip-local VoxConverse); warm processing time fell 8.4%, but overlapping busy-Mac
energy measurements did not establish an energy win. The default stays unchanged.
