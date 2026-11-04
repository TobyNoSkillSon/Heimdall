# capture — audio sources

Core Audio process taps for recorded apps, the microphones (primary + extras, built-in fallback), one host-clock
timeline, resampling to 16 kHz for listen, the optional echo guard, and lossless raw writes into the archive store.
Behaviour, numbers and gotchas: `docs/capture.md`.

**Code map:**
- `lib.rs` — `Capture::{start, start_with_archive, discover_only, idle}`, `set_microphones`, `set_recorded_apps`,
  `refresh_permissions`, `set_echo_cancellation` / `echo_cancellation` / `echo_control`; `CaptureConfig`,
  `CaptureConsent::after_setup`, `MicrophoneState`, `select_microphone`.
- `source.rs` — `SourceInfo`, `SourceId`, `AppIdentity` (`app_identity`: the App Audio key).
- `time.rs` — `host_now_ns`, `ClockMapper`.
- `pipeline.rs` — callback packets → drain pump → heap stage → archive thread and listen queue; `Packet::clean`;
  `CLOSE_WAIT`, `settle_stopped_archives`.
- `resample.rs` — native → 16 kHz mono.
- `archive.rs` — `Archive` (one capture segment in the store), `read_blocks`, `open_read_only`; the `ArchiveSink` trait is in `lib.rs`.
- `echo.rs` — the echo mux (SPSC ring, crossfades); `macos/vpio.rs` — the voice-processing unit, its gates and the
  system snapshot (all behind `vpio::Platform`, faked in tests).
- `macos/hal.rs` (discovery, listeners, `snapshot`), `macos/tap.rs` (process taps, mic IOProcs), `macos/mod.rs`
  (owner thread, health timer, recovery).
- `bin/heimdall-capture-probe.rs` — idle and post-permission live probe.

Events arrive on `CaptureReceiver` (`CaptureEvent::{SourceAdded, Audio(AudioChunk), SourceRemoved, …}`). Depends only
on `heimdall-archive`.

**Testing:** `cargo nextest run -p capture` — synthetic sources, no device, each test under 0.5 s. The ignored lab
tests in `tests/tap_onset.rs` and `tests/app_audio.rs` need real taps and the existing permission (`docs/capture.md`,
"Live probe").
