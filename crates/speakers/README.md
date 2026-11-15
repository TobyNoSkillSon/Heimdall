# speakers

Heimdall's speaker SDK: native Nemotron diarization (vendored NeMo-Speech.cpp and ggml, Metal), ReDimNet voice
embeddings (CPU ORT), voice profiles and naming events. No audio capture, no UI, no permission requests and no
workspace dependencies. How the app uses it: [docs/speakers.md](../../docs/speakers.md).

Read this when: you are changing the diarizer, the voice encoder, `speakers.json`, naming events or the call helper
process, or you need to reproduce the calibration and accuracy numbers.
Code: `crates/speakers/src/`, `crates/speakers/native/`, `crates/speakers/vendor/`.

## Files and entry points

| File | What it holds |
| --- | --- |
| `src/diar.rs` | `Diarizer::load` (pinned GGUF check, eight tracks, ultra-low-latency geometry), `DiarStream` (`push_f32`, `finish`, `segments`, `completed_segments`) |
| `src/embedding.rs` | `ReDimNet`, `VoiceEncoder`, `Embedding` (192 values, unit norm), `MODEL_ID`, `WINDOW_SAMPLES` (6 s) |
| `src/pipeline.rs` | `SpeakerPipeline`: 12 s ring, clean intervals (no overlap, 100 ms edge trim), two 6 s windows per track |
| `src/identity.rs` | `EnrollmentStore` (`speakers.json`: names, coded voices, ignored, Heimdall's voices, calibration), `identify`, `near_miss`, `widen`, the constants `WIGGLE`, `WIDEN_MARGIN`, `ASK_BAND`, `HEIMDALL_IDENTIFY_MIN`, `VOICE_SAMPLES` |
| `src/naming.rs` | `NamingFlow`, `TrackId`, `SpeakerEvent` (`LabelChanged`, `VoiceCoded`), code labels, locked re-read-and-write |
| `src/call.rs` | `CallScopedSpeakers` (live session, `expire`, `QUIET_TIMEOUT` 120 s), `EncoderHelper` (encoder only), `run_helper` |
| `src/metal.rs` | hooks the vendored ggml and NeMo code call: GPU time, GPU fault recovery, a projection hook that leaves work to ggml |
| `src/scoring.rs`, `src/codec.rs` | RTTM parsing and DER scoring; Opus round trips for codec tests |
| `src/bin/heimdall-speakers-worker.rs` | the helper process the app bundles; runs `call::run_helper` |
| `src/bin/heimdall-speakers-file.rs` | `<models-dir> <wav> <output-json> [seconds] [cpu] [probabilities.f32]` |
| `src/bin/heimdall-speakers-score.rs` | `--der reference.rttm segments.json seconds`, `--calibrate trials.json out.json dataset` |
| `src/bin/heimdall-speakers-bakeoff.rs`, `-codec.rs`, `-memory.rs`, `-memory-call.rs` | causal replay, Opus copies, memory checks |
| `examples/live_timeline.rs`, `examples/paced_call.rs` | record the live timeline for model-free replay tests; real-time paced helper check |
| `native/` | C ABI glue, `nemotron3_window.h` (the checkpoint's exact bf16 Hann window), `build-lock.json` (flags, compiler) |
| `vendor/pins.json` | native source revisions; local patches are the window override, an opt-in feature dump and debug-gated timing |
| `crates/speakers/scripts/fetch_models.py` | pinned Nemotron GGUF, ReDimNet ONNX and AMI test assets into `models/speakers/` |
| `crates/speakers/scripts/reference_parity.py`, `crates/speakers/scripts/reference.lock.json` | NeMo CPU reference run (pinned NeMo Speech, lhotse, Python, Torch, checkpoint) |

## Live use

- The host creates `CallScopedSpeakers` with the explicit path of the bundled `heimdall-speakers-worker`, activates it
  when a multi-speaker source becomes active, feeds mono 16 kHz PCM in chunks of at most one second with its VAD
  speech flag, and calls `expire(Instant::now())` from a timer even when capture stops.
- `QUIET_TIMEOUT` without speech finishes the pipeline and kills and reaps the child, returning Metal and ORT memory
  to the OS. Activation after expiry needs a new session ID and the right audio origin.
- Run the synchronous interface on a dedicated speaker thread. `Diarizer` and `DiarStream` are `!Send`/`!Sync`.
- Naming state stays in the parent. Using `Diarizer`/`SpeakerPipeline` in-process (offline tests) keeps
  library-global caches alive after the model is dropped.
- Legacy one-window enrolments stay readable but lack the two-window calibration guarantee.

## Tests

1. Fetch the assets: `python3 crates/speakers/scripts/fetch_models.py` (writes `models/speakers/`).
2. Run `cargo test -p speakers`. Tests whose ignored assets are missing print a skip.
3. Opt-in, ignored by default:
   - `cargo test -p speakers --test identity_v2 -- --ignored --nocapture`: the 384-query identity replay.
   - `cargo test -p speakers --test review_native -- --ignored --nocapture`: stream/feed partition equivalence and
     the held-out identity windows.
   - `gpu_fault_nemotron_*` in `src/metal.rs` (simulated device loss; real model).
4. Reference parity: in a throwaway `ref/venv`, run `crates/speakers/scripts/reference_parity.py` on
   `models/speakers/ami-480.wav`, first without `--gguf`, then with the shipped Q8 GGUF. It emits mel features, frame
   probabilities and threshold-0.5 segments. `HEIMDALL_DIAR_FEATURE_DUMP=<fresh file>` appends the native 128-bin f32
   mel features; `heimdall-speakers-score --der` scores segments with zero collar, overlap included.

The first-chunk golden fixture (`tests/fixtures/reference-first-chunk.json`) checks the native file-runner path. The
builds fetch no native source; Apple clang's signature is enforced; host C/C++ uses no fast math.

## Calibration and measurements

- Identity calibration (`tests/fixtures/calibration.json`, loaded by `EnrollmentStore::default()`): AMI ES2002 and
  IS1000 meetings a/b, seven people, enrolment from meeting a and queries from meeting b, two raw 6 s windows per name.
  Cutoff `0.455542`, FAR and FRR 2.0833% each on 336 genuine and 336 impostor trials. Calibration alone chose six
  seconds and two samples.
- Held-out test (ES2004 and IS1009, eight other people), measured on Apple Silicon: balanced accuracy 99.22% in raw,
  Opus 24 kb/s and Opus 64 kb/s (127/128 each); Unknown rejection 99.22/97.66/99.22%. Rust replay reproduces all 384
  outcomes. VoxConverse (eight clips, clip-local identities, not cross-session): balanced accuracy about 99.4%, but
  Unknown rejection only about 89%. These are oracle-clean windows, not end-to-end diarized naming accuracy.
- Diarization, measured on the first 480 s of AMI ES2004a Array1-01 with forced-alignment references, zero collar,
  overlap included: DER 40.25% raw, 29.82% Opus 24 kb/s, 40.57% Opus 64 kb/s; the CPU NeMo reference on the same Q8
  tensors gives 40.22% raw. Real-time factor 0.057. The high crop DER and the 64 kb/s result are open quality limits.
- Memory, measured on a 120 s replay: the helper's parent plus child RSS is about 9 MiB before, 922 MiB during and
  23 MiB after; the child is reaped. In-process teardown keeps about 620 MiB, which is why the models live in a child.
- Scripts that produced these (bake-off, calibration, DER cross-check) live in `lab/speakers/scripts/`; receipts stay
  in ignored `ref/` and `models/speakers/identity-v2/`.

## See also

- [docs/speakers.md](../../docs/speakers.md): naming, guests, Heimdall's own voice and the user's voice in the app
- [docs/capture.md](../../docs/capture.md): the call sources that feed the helper
- [docs/models.md](../../docs/models.md): model downloads
- [docs/resilience.md](../../docs/resilience.md): helper failures and GPU fallback
- `THIRD_PARTY.md`: licences; sources are listed in [docs/speakers.md](../../docs/speakers.md)
