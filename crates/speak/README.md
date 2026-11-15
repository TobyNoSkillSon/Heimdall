# speak: speech synthesis and playback

Heimdall's voice output: the Supertonic 3 synthesizer (FP16 Metal shaders), Phonon S, the streaming-voice seam, the
speech queue, CoreAudio playback with route recovery, speed change, and GPU failure recovery.

Read this when: you change how replies are synthesized or played, the playback hooks, output-device handling, voice
speed, Supertonic's Metal path or the TTS model assets.
Code: `crates/speak/src/`; the app's voice routing is `crates/app/src/speech_voice.rs`.

## Modules

| Path | Owns |
|---|---|
| `lib.rs` | `Supertonic` (`load`, `load_with_backend`, `render`), `TtsBackend`, `VOICE` (`M3`), `DENOISING_STEPS` 5, `SPEED` 1.05, `SpeechQueue`, `Synthesizer`, `AudioOutput`, `WavOutput`, `SpeechPacket`, `PlaybackStream`, `Cancellation` |
| `onnx.rs` | Supertonic's ONNX sessions, text preprocessing, `chunk_text`, `write_wav_file` |
| `metal.rs`, `pointwise.metal`, `graph_patch.rs`, `build.rs` | the FP16 pointwise shader, its ONNX wire edit, and the precompiled metallib |
| `playback.rs` | `CoreAudioOutput`, prebuffering, underrun handling, route recovery, the speaking, spoken-text and level hooks, `set_voice_gain` |
| `route.rs` | `default_output`, `default_output_id`, `default_output_is_bluetooth`: the default output read without opening it |
| `stretch.rs` | WSOLA speed change with pitch kept (`MIN_PERCENT` 80, `MAX_PERCENT` 160) |
| `voice.rs` | `StreamingVoice`, `stream_sentence`, `stream_line`, `PLAYBACK_RATE` 44,100 Hz |
| `phonon.rs`, `phonon/voices.json` | Phonon S in process |
| `gpu_recovery.rs`, `gpu_recovery_core.rs` | GPU fault handling shared with `decide` and `speakers` |
| `coverage.rs` | the word check: alignment with protected meaning and repeated-phrase rejection |
| `drama3.rs`, `memory_line.rs`, `compute_flag.rs` | the personal edition's own voice engine client and its memory and compute coordination |
| `src/bin/` | `heimdall-tts-wav` (render to WAV), `heimdall-tts-probe` (resident latency harness, WAV only), `heimdall-tts-module` (module probe) |
| `scripts/` | `fetch_models.py`, `fetch_phonon.py`, `models.lock.json`, `phonon.lock.json`, `capture_shader_reference.py` |

The public bundle ships Phonon S as its only built-in voice ([editions.md](../../docs/editions.md)).

## Supertonic

- `Supertonic::load(models/supertonic-3)` loads the four ONNX components, frontend and style through statically
  linked `ort` 2.0.0-rc.13. No Python, Node or external ONNX Runtime at run time. English and Polish share the models
  and the one style `VOICE`.
- On the qualified device (`metal::Gpu::qualified_device`: "Apple M5 Max") the 56 dominant vector-estimator pointwise projections run as FP16 Metal tensor-unit shaders with
  FP32 accumulation; other devices stay on CPU until measured. Everything else keeps its FP32 CPU arithmetic.
- CPU sessions: two intra-op threads, one inter-op thread, idle spinning off. Five denoising steps, speed 1.05, fixed
  Gaussian seed 42. Each 120-byte text chunk runs its own duration, text, denoising and vocoder inference; chunking
  keeps all text and long unbroken words stay whole.
- `HEIMDALL_TTS_BACKEND=cpu` selects the CPU path, `metal` forces the shader path; `load_with_backend` selects
  explicitly without touching the environment. `HEIMDALL_TTS_PROFILE_DIR` turns on ORT node profiling; never set it
  while timing.
- The shader is precompiled as Metal 4.0 with fast math off and embedded; synthesis never compiles shaders. The ONNX
  wire edit preserves unknown fields, rewrites only constant dense 1×1 convolutions, is idempotent and never changes
  model files. Frontend and inference are adapted from the upstream MIT Rust example (`UPSTREAM-LICENSE`); the shader
  tile is adapted from Vella (`VELLA-LICENSE`).

## Speech queue and playback

- `SpeechQueue::new(tts, CoreAudioOutput)` owns a sleeping worker and a 32-job channel. `speak(text, "en"|"pl")`
  returns the job ID at once; results and errors arrive on `events`.
- `interrupt()` (barge-in, Speaking off) advances the cancellation epoch, drops queued jobs, checks cancellation
  between denoising steps and stops playback. A native ONNX call in progress finishes, but its result is not played.
- Normal completion keeps the device open until the last sample has passed the speaker: device latency plus a 20 ms
  tail (`TAIL_SECONDS`).
- Speaking flag (`set_speaking_hook`): one count across every queue; on at the first sample, off 150 ms
  (`SPEAKING_LINGER`) after the last sample of the last queued job or at once on `interrupt()`; released after 1.5 s
  (`SPEAKING_SILENCE_CAP`) while a job waits for its first sample.
- `SpeechPacket::Sentence` marks where a sentence's audio starts; playback reports it through `set_spoken_text_hook`
  when the speaker reaches it, and loudness every 30 ms (`LEVEL_PERIOD`) through `set_level_hook`.
- Streaming first sentence: `Prebuffer` estimates the played length from the text (15 characters per second divided
  by the pace the queue passes as `Synthesizer::pace`) and the generation rate, and starts with
  `length × (1 − rate)` buffered, ×1.15 + 0.12 s, at most 1.2 s (`PREBUFFER_CAP`).
- Underrun inside a sentence: fade out over 5 ms, wait for 0.35 s of audio (`REBUFFER_SECONDS`) or the next sentence,
  fade in, log one `voice: playback underrun` line (at most one per 10 s) and lower the remembered rate.
- Nothing to play for 2 s (`IDLE_RELEASE`) closes the device until audio arrives, so a stalled reply never holds
  Bluetooth headphones. Logged: `voice: output is now <device> (<transport>, <rate>, <channels>)` on a route change,
  CoreAudio's text on a stream error, no first audio after 10 s, a gap of 5 s or more, and a reply that played nothing.

## Output recovery mid-reply

1. A 4 s stall (`OUTPUT_STALL`), a stream error, or a new default output held for 250 ms (polled every 100 ms) closes
   the stream.
2. Playback reopens on the current default and replays from the start of the sentence that had reached the speaker
   (the cursor keeps at least the last 3 sentences and any unfinished, up to 32).
3. At most two reopens per reply (`MAX_REOPENS`); a third loss fails the reply.
4. Each recovery logs `voice: output moved to <device> mid-reply; resumed from sentence N`.

The device seam (`Outputs`, `OutputStream`, `Render`) lets tests drive a fake device through the same render code.
Replies never put Bluetooth headsets into call mode: playback is a plain output stream, and `tests/no_call_mode.rs`
fails if any speak source opens an input or a voice-processing unit.

## Phonon S and streaming voices

- `speak::phonon` runs Phonon S (`gradium/phonon-31m-en`, Apache-2.0) in process on the CPU through the pinned `ptts`
  git crate (rev `49b9033`, built without `hub`, so it never downloads), with the 37 English presets of
  `gradium/phonon-voices` in `phonon/voices.json`.
- `models/phonon-31m-en` is filled by `crates/speak/scripts/fetch_phonon.py` from the local Hugging Face cache, pinned
  in `crates/speak/scripts/phonon.lock.json` and bundled by `scripts/bundle_models.py`.
- `StreamingVoice` is implemented by Phonon S here and by ElevenLabs and the own-engine voice in
  `crates/app/src/speech_voice.rs`. `stream_sentence` and `stream_line` add sentence labels for the island, resample to
  44.1 kHz and apply the same WSOLA stretch.
- Voice speed is applied after synthesis by `stretch`; stored bank lines are kept at natural speed and stretched on
  playback. The setting and its default: [settings.md](../../docs/settings.md).

## GPU failure recovery

- Every command buffer is checked before readback; a failure returns `gpu_recovery::GpuError`, and ORT's string
  boundary keeps a fault receipt so finite output cannot hide a failed custom operator.
- A failed TTS chunk drops its session, device, queue and pipelines and retries once, then uses the CPU graph. GPU is
  tried again on the next unit at least 30 s later. `gpu_recovery::gpu_status()` gives the menu line
  `Voice running on CPU after a GPU error`. Another model's success never clears this one's status; CPU overrides stay
  CPU.
- Fault hooks exist only under `cfg(test)`.

## Assets

`crates/speak/scripts/fetch_models.py` downloads Supertonic 3 at the immutable Hugging Face revision
`3cadd1ee6394adea1bd021217a0e650ede09a323` into the gitignored `models/supertonic-3`: four ONNX files,
frontend and config, the ten upstream preset styles (M1 to M5, F1 to F5; only M3 is used) and the model notice. URLs,
sizes and SHA-256 are in `crates/speak/scripts/models.lock.json`. The weights and styles carry the OpenRAIL-M licence;
keep the notices when bundling.

## Shader qualification

1. Freeze reference tensors once: `python3 crates/speak/scripts/capture_shader_reference.py` (pinned development
   dependencies in `crates/speak/scripts/shader-requirements.txt`).
2. Run the shader check (`lab/speak/scripts/shader_check.py` in the development repository) with the resident Rust
   binaries. Gates: relative RMS under 0.5% per module, waveform under 5%, spectral distance under 2%, identical output
   lengths, no word-error increase per sentence. `--rounds 2` runs ABBA then BAAB.
3. `--reuse-quality` reuses an already qualified arithmetic path for measurement only; confirm equal shader and model
   inputs and identical final output before using it across a build change.
4. Run nothing else on the CPU or GPU during energy brackets.

## Tests

- `cargo test -p speak` (fast, no model). `tests/word_check_takes.rs` replays logged word-check rejections when its
  lab file is present and skips otherwise.
- Opt-in model tests: `cargo test -p speak phonon_synthesizes -- --ignored` (Phonon S); `CARGO_TARGET_DIR=target/shader-speak
  cargo test -p speak -- --include-ignored` runs the golden test, which writes `models/tts-golden/en.wav` and `pl.wav`
  with `TtsBackend::Cpu` and checks 44.1 kHz mono, finite non-silent PCM, seeded length and RMS. It is a numerical
  regression, not a listening verdict.
- `gpu_fault_*` tests drive one fault, two in a row and an expired backoff through the real command and model
  boundaries, with no microphone or playback.
- Offline output without a device: `render(...).write_wav(...)`, `heimdall-tts-wav`, or `WavOutput::new(path)`.

## See also

- [voices.md](../../docs/voices.md): the voice picker
- [settings.md](../../docs/settings.md): voice speed and the Speaking settings
- [resilience.md](../../docs/resilience.md): failures and logs
- [models.md](../../docs/models.md): model downloads
