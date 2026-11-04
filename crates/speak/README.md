# Heimdall speech

**Domain: speech.** Supertonic TTS (FP16 Metal shaders), Phonon S, the sidecar voice client, the word
check, the speech queue and playback. **Interface:** `Supertonic`, `phonon`, `drama3::Service`, `drama3::speak_line`,
`coverage`, `SpeechQueue`, `Synthesizer`, `AudioOutput`, `stretch`, `VOICE`.
**Testing:** `cargo nextest run -p speak`, all under 2 s; the sidecar contract test is
`python3 -B -m unittest discover -s crates/speak/drama3 -p test_sidecar.py`. Ignored GPU/golden and Phonon model tests.
`tests/word_check_takes.rs` replays the 602 logged word-check rejections without a model.

Behaviour lives in the docs: the voice picker in `docs/voices.md`.

Supertonic 3 runs locally through statically linked `ort` 2.0.0-rc.13. No Python, Node, Mobius, separate ONNX Runtime installation or external TTS app is required at runtime. English and Polish share the same four models and one style, `speak::VOICE` (M3). The frontend/inference implementation is adapted from the upstream MIT Rust example, with the upstream notice in `UPSTREAM-LICENSE`; model weights carry their own OpenRAIL-M notice in the assets.

`Supertonic::load(models/supertonic-3)` loads the four ONNX components and matching frontend/style. On the qualified Apple M5 Max, 56 dominant vector-estimator pointwise projections use custom FP16 Metal tensor-unit shaders with FP32 accumulation/output. Other devices retain CPU until separately measured. Duration, text encoder, vocoder, normalization, attention and shape operations retain their original FP32 CPU arithmetic. CPU sessions use two intra-op threads, one inter-op thread, idle spinning disabled. Five denoising steps, speed 1.05 and fixed Gaussian seed 42 remain unchanged. Model config determines latent dimensions/rate; no model-size constants are guessed. Each 120-byte target text chunk has separate duration/text/denoising/vocoder inference. Polish normalization follows upstream NFKD and language wrapping. Chunking preserves all text; long unbroken words remain in one chunk.

`SpeechQueue::new(tts, CoreAudioOutput)` owns a sleeping worker and a bounded 32-job channel. `speak(text,"en"|"pl")` uses `try_send` and returns the job ID immediately. Each chunk is synthesized and output before generating the next; results/errors arrive on `events`. Call `interrupt()` when speech must stop (the app's barge-in decision, Speaking off): it advances the cancellation epoch, drops earlier queued jobs, checks cancellation between denoising steps, and stops current playback. CoreAudio is CPAL's macOS backend, opened lazily on first output. Playback checks cancellation every 5 ms; actual hardware cancellation latency still needs a speaker-authorized test. Normal completion keeps the device running until the last real sample has passed the speaker: the render callback's device latency (cpal's playback instant minus its callback instant) plus a 20 ms tail, measured in device frames. The speaking flag (`set_speaking_hook`) is one count across every queue. Each queue holds its claim across the jobs queued back to back (synthesis gaps included): on when the first sample reaches the speaker, off 150 ms after the last sample of the last queued job, or at once on `interrupt()`. While a job waits for its first sample, the claim is released after 1.5 s of silence and taken again by the next sample that plays. Generation cancellation waits for any current native ONNX call to finish, but does not play its result.

The first sentence streams while the sidecar voice generates it, so playback holds it before opening the device until the buffer
covers the expected shortfall (a 150 ms hole after the first word at 160%). `Prebuffer` estimates
the sentence's played length from its text (15 characters per second, divided by the voice speed the queue passes as
`Synthesizer::pace`) and the generation rate from the last measured one blended with this stream's arrivals, and
starts with `length × (1 − rate)` buffered (×1.15, +0.12 s), at most 1.2 s after the first chunk. A stream that still
runs dry inside a sentence fades out over 5 ms, waits for 0.35 s of audio (or the next sentence), fades back in, and
logs one `voice: playback underrun` line (duration, position, sentence, speed; at most one per 10 s, the rest counted)
and lowers the remembered rate. Nothing to play for 2 s stops and releases the device until audio arrives, so a
stalled reply never holds Bluetooth headphones. Each stream logs `voice: output is now <device> (<transport>, <rate>,
<channels>)` when the route changes, a stream error carries CoreAudio's text, and a reply with no first audio after
10 s, a gap of 5 s or more mid-reply, or one that ends with nothing played are each logged. `route::default_output`
reads the default output device's name and transport without opening it.

A reply survives its output dying or moving (4 Nov; 2 Nov 14:46 an AirPods stream stopped calling back and the reply
was lost): a 4 s stall, a stream error, or a new default output held for 250 ms (`route::default_output_id`, polled
every 100 ms) closes the stream, reopens on the current default, and replays from the start of the sentence that had
reached the speaker (the cursor keeps the last three sentences). At most two reopens per reply; a third loss fails
as before. Each recovery logs one line, "voice: output moved to <device> mid-reply; resumed from sentence N". The
device seam (`Outputs`, `OutputStream`, `Render`) lets tests drive a fake device through the same render code.

Daytime/offline checks use `render(...).write_wav(...)`, `heimdall-tts-wav`, or `WavOutput::new(path)`. The WAV queue sink appends every chunk and flushes a valid header. None initialize an output device. The callback converts mono PCM to the device channel count/sample rate (linear interpolation), supports f32/i16/u16 devices, and reports output errors.

## Phonon S and streaming voices

`speak::phonon` runs Phonon S (`gradium/phonon-31m-en`, Apache-2.0) in process on the CPU through the pinned `ptts` git
crate (`gradium-ai/xn-ptts` rev 49b9033, built without `hub`, so it never downloads), with the 37 English presets of
`gradium/phonon-voices` in `phonon/voices.json`. `models/phonon-31m-en` (74.5 MB) is filled by `scripts/fetch_phonon.py`
from the local HF cache, locked in `scripts/phonon.lock.json` and bundled by `scripts/bundle_models.py`.
`speak::voice::StreamingVoice` is implemented by Phonon S, ElevenLabs and Own Engine (`app/src/speech_voice.rs`);
`stream_sentence` (per sentence, what the app calls) and `stream_line` add sentence labels for the island, linear resampling to 44.1 kHz and the same WSOLA stretch as the sidecar voice. Opt-in model test:
`cargo nextest run -p speak phonon_synthesizes --run-ignored only`.

## Assets and verification

The agent-run fetch script `scripts/fetch_models.py` downloads immutable HF revision `3cadd1ee6394adea1bd021217a0e650ede09a323` to the project's gitignored `models/supertonic-3`. Four ONNX files, frontend/config, the ten upstream preset voice styles (only M3 is used) and the model notice: 401,291,751 bytes.

## Supertonic voice style

Supertonic is the lighter built-in voice, in one fixed style, `speak::VOICE` = M3, upstream's "polished, authoritative male voice" (supertonic-py `docs/voices.md`), the preset closest to a calm, measured delivery. The ten style files are the upstream presets M1–M5 and F1–F5 from `Supertone/supertonic-3` at the pinned revision, 290,198–292,423 bytes each, under the repository's OpenRAIL-M `LICENSE`, the same notice as the weights. URLs, sizes and SHA-256 are in `scripts/models.lock.json`; `scripts/bundle_models.py` copies every locked asset into the app bundle's manifest. Preserve these assets and notices when bundling/signing. The script is development machinery, not a runtime dependency.

The CPU golden test explicitly selects `TtsBackend::Cpu`, writes `models/tts-golden/en.wav` and `pl.wav`, reads each WAV back, and verifies 44.1 kHz mono, finite/non-silent PCM, seeded waveform length and RMS. It records first-PCM latency, synthesis time, actual audio duration and RTF in `measurements.json`. RTF = synthesis seconds / output seconds; 1.0 means exactly real-time generation. Time-to-first-PCM excludes model loading, queue delay and CoreAudio buffering. Model load is measured separately, so cold first-request latency can be derived as load + first-PCM. The golden is a numerical/output regression, not a human listening verdict or a language-intelligibility score.

Agents use `CARGO_TARGET_DIR=target/shader-speak cargo test -p speak -- --include-ignored` for these local golden assets. The fixed kernel test exercises real Metal, bias, two-item batches, tail padding, buffer reuse and nonfinite rejection. Normal tests skip model-dependent golden inference. No permission request, microphone capture or speaker playback was performed. The WAV binary links system frameworks/libraries only, with no external ONNX dylib.

## Metal qualification and debug path

`HEIMDALL_TTS_BACKEND=cpu` selects the old path; `metal` forces the candidate. `Supertonic::load_with_backend` provides the same explicit selection for regression tests without mutating process environment. `HEIMDALL_TTS_PROFILE_DIR` enables optional native ORT node profiling; never set it during timing/energy measurement.

`pointwise.metal` adapts the 32×32 split-K tensor tile from Vella's `lab/models/Shared/vk-formats27/kernels.metal`, with attribution in the vendored source. It is precompiled as Metal 4.0 with fast math disabled and embedded as a metallib; runtime synthesis does not compile shaders. Inputs/weights are FP16, partial sums and the bias/output are FP32. Rust converts immutable weights once per loaded operator and reuses bounded activation/output buffers. One shared device/queue belongs to the four-module TTS instance; `SpeechQueue` owns that instance, rather than per-request or per-stream model copies. The ONNX wire edit preserves unknown fields, only selects supported constant dense 1×1 convolutions, and is idempotent. It does not modify model files.

The text-encoder/vocoder shader trial lost on individually synchronized dispatches (fixed-module CPU 2.3/34 ms, GPU 7.5/39 ms), so those paths were rejected. The vector estimator dominates the native Rust profile: its pointwise projections occupy 269.7 ms of 371.9 ms total profiled node work on the English fixture. Profiling overhead is excluded from benchmark figures.

Agent-operated development verification uses `scripts/capture_shader_reference.py` once to freeze small deterministic tensors and discovery profiles, then `scripts/shader_check.py` with the resident Rust binaries. Development-only Python dependencies are pinned in `scripts/shader-requirements.txt`. The current Rust runtime is ONNX Runtime 1.28.0 (ort rc.13); Python 1.31.0 supplies discovery profiling only. Module parity references come from the actual Rust CPU path. Four seeded English/Polish WAV pairs have identical lengths, at most 1.646% waveform relative RMS difference and 0.558% normalized STFT magnitude distance (1024-sample Hann windows, hop 256). For Parakeet, both arms receive identical 44.1→16 kHz polyphase conversion. Both achieve 0/23 word errors, with identical lexical transcripts. Duration/text/vocoder outputs are exact; vector-estimator relative RMS error is 0.03871%. These checks establish numerical/task parity on this set; no human listening test was performed.

The short shader-check gates are relative RMS <0.5% per module, waveform <5%, spectral distance <2%, identical output lengths and no WER increase per sentence. Retain captured CPU tensors. `--reuse-quality` is measurement-only reuse of an already qualified arithmetic path; verify equivalent shader/model inputs and final module/WAV output identity before using it across a build change. `--rounds 2` runs ABBA then BAAB. Do not run builds or other worker-owned CPU/GPU jobs beside the energy brackets.

11 Oct 2026, busy M5 Max, warmed resident models: English/Polish median first PCM **308.65/328.29 → 148.24/157.23 ms** (64 requests per arm/language). Median rootless-macmon SoC energy per output second **2.532/3.828 → 0.907/0.941 J** (four brackets per arm/language, 16 utterances each). This includes background activity; noisy idle brackets prevent TTS-only attribution. Before/after energy bracket ranges are EN 2.470–2.732/0.856–1.209, PL 3.069–9.836/0.870–1.213 J/s speech. Timing extrema are EN 306.5–322.0/141.5–161.2, PL 321.8–804.2/151.0–174.3 ms. Raw receipts, WAVs, profile traces, hashes and default-path smoke live in ignored `models/reference/shader-speak/`. The ordinary no-override default writes the same English WAV as the gated Metal candidate (158.06-ms first PCM, 349.38-ms load); cold latency includes that load. Coordinator owns fresh Sol review and installed-app integration.

## Sidecar voice client, speed and island hooks

`drama3::Service` supervises the sidecar (`drama3/sidecar.py`, `memory.py`, `text.py`); `drama3::speak_line` says a
line sentence by sentence, applies `coverage::accept_repeated_lone_word` and `coverage::keepable`, and emits
`voice_switch` events. A dropped
`Process` kills the worker and reaps it on a `drama3-reaper` thread (`reap_detached`), never on the caller.

Voice speed (Speaking ▸ Speed, 0.8 to 1.6x, default 1.15x) is applied to every voice after synthesis by `stretch`
(WSOLA, pitch kept); bank lines are stored at natural speed and stretched on playback. `SpeechPacket::Sentence` marks
where each sentence's audio starts; CoreAudio playback reports it through `set_spoken_text_hook` when the speaker
reaches it, and the loudness of each 30 ms through `set_level_hook`.

## GPU failure recovery

All command buffers are checked synchronously before readback. A GPU failure is
returned as `gpu_recovery::GpuError`; ORT's string boundary preserves a fault
receipt so finite output cannot conceal a failed custom operator. The shared
implementation lives in `src/gpu_recovery.rs` and is included by decide and speakers.

A failed TTS chunk drops its complete session/device/queue/pipelines and retries
once, then uses the original CPU graph. GPU recovery is attempted on the next unit
at least 30 seconds later. `gpu_recovery::gpu_status()` supplies the persistent menu
line `Voice running on CPU after a GPU error`. Successful work in another model
cannot clear a model's fallback status. CPU overrides remain CPU-only.

Fault hooks are armed by Rust `cfg(test)` only. Offline model tests named
`gpu_fault_*` exercise the actual command and model boundaries with one fault,
two consecutive faults, and an expired backoff; no microphone or playback is used.
