# capture: audio sources

Records the chosen microphones and chosen apps' audio on one host clock, archives raw samples losslessly and hands
16 kHz mono chunks to listen. Behaviour, limits and gotchas: [docs/capture.md](../../docs/capture.md).

## Files

| File | What it holds |
|---|---|
| `src/lib.rs` | `Capture::{start, start_with_archive, discover_only, idle, push_input}`, `set_microphones`, `set_recorded_apps`, `refresh_permissions`, `set_echo_cancellation`, `echo_cancellation`, `echo_control`; `CaptureConfig`, `CaptureConsent::after_setup`, `MicrophoneState`, `select_microphone_from`; `ask_system_audio`, `system_audio`; the `ArchiveSink` trait; `PushOnly` |
| `src/source.rs` | `SourceInfo`, `SourceId`, `SourceKind`, `AppIdentity` and `app_identity` (the App Audio key) |
| `src/time.rs` | `host_now_ns`, `ClockMapper` |
| `src/pipeline.rs` | callback packets, drain pump, heap stage, listen queue; `Packet::clean`; `PushInput`, `PushSource`; `CLOSE_WAIT`, `settle_stopped_archives` |
| `src/commit.rs` | the `heimdall-archive-commit` thread |
| `src/resample.rs` | native rate to 16 kHz mono |
| `src/archive.rs` | `Archive` (one capture segment in the store), `read_blocks`, `open_read_only` |
| `src/tap_gate.rs` | `TapGate`, `TapAccess`: the System Audio Recording ask, apart from the microphones |
| `src/echo.rs` | echo mux (SPSC ring, crossfades), `EchoCancel` |
| `src/macos/mod.rs` | HAL owner thread, health timer, retries and recovery |
| `src/macos/hal.rs` | discovery, listeners, `source_objects`, `snapshot` |
| `src/macos/tap.rs` | process taps, microphone IOProcs, `ask_system_audio` |
| `src/macos/vpio.rs` | the voice-processing unit, its gates and the system snapshot, behind `vpio::Platform` (faked in tests) |
| `src/bin/heimdall-capture-probe.rs` | idle probe and post-grant live probe ([docs/capture.md](../../docs/capture.md)) |

Events arrive on `CaptureReceiver` as `CaptureEvent::{SourceAdded, Audio(AudioChunk), SourceRemoved, …}`. Depends on
`heimdall-archive` and `heimdall-home`. The app drives it from `crates/app/src/live_loop.rs` and
`crates/app/src/pipeline/sources.rs`.

## Tests

- `cargo test -p capture`: synthetic sources, no device. `tests/hardening.rs` commits once a second while a real
  throttled compactor moves old audio to cold; it takes seconds.
- `tests/tap_onset.rs` and `tests/app_audio.rs` are ignored lab tests that need real taps and an existing grant: set
  `HEIMDALL_TAP_LAB_DIR` (an empty directory) and `HEIMDALL_TAP_LAB_WAV` (a 48 kHz PCM16 WAV), then run with
  `-- --ignored`. Rules for live runs: [docs/capture.md](../../docs/capture.md).

## See also

- [docs/capture.md](../../docs/capture.md): pipeline, sources, permission, echo guard, probe
- [docs/archive.md](../../docs/archive.md): the store the samples go into
