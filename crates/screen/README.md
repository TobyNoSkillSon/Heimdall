# screen

Screen recording of every display into the archive store (ScreenCaptureKit to hardware HEVC) and the deterministic
matching, holds and attachments behind spoken screen commands. No API in the crate requests a permission.

Behaviour, limits and procedures: [docs/screen.md](../../docs/screen.md).

## Files and entry points

- `src/macos.rs`: `ScreenCapture` (`start_recording`, the SCK owner thread, recovery, display and wake observers),
  `ScreenEvent`, `grab_still` (drawing mode's still), the full-resolution grab, `capture_blocker`,
  `screen_permission_granted`, `frontmost_context` (screen-state metadata).
- `src/buffer.rs`: `ScreenBuffer`, the recording facade (it keeps no RGB ring): `recording` writes the real store,
  `new` is an isolated lossless replay fixture, `lab` an isolated temporary store with the real encoder, `idle` is
  the store-less placeholder while Seeing is off. `ScreenConfig` (`size`, `validate`), `select` (causal frame choice), `ARCHIVE_RETRY`.
- `src/gate.rs`: `CaptureGate` (Seeing, awake, unlocked, brightness).
- `src/gaps.rs`: diagnosis only; one log line per display when archived frames leave a gap.
- `src/hold.rs`: `ScreenHold`, `ScreenHolds`, `attach_held`, `native_grabber`, the hold limits and
  `FULL_RESOLUTION_SAME_MOMENT_NS`.
- `src/fullres.rs`: `attach_command_screenshot` (used by the probe) and `attach_command_with_grabber` (tests).
- `src/commands.rs`: `SCREEN_PHRASES`, `PhraseMatcher`, `PhraseMatch`, `set_name_words`, `Shot`, `ShotKind`,
  `Action`, `ScreenActionEvent`; `TimedWord` from `listen::Word` behind feature `listen-adapter`.
- `src/lib.rs`: `Still`, `EXCLUDED_WINDOW_TITLES`, `is_excluded_window`.
- `src/bin/heimdall-screen-probe.rs`: cost probe for a build that already holds Screen Recording.
- `examples/island_exclusion.rs`: checks that Heimdall's own windows stay out of its captures.

Encoding, decoding and tiering live in `crates/archive` (`src/video.rs`, `src/video_native.rs`). The app drives the
crate from `crates/app/src/pipeline/commands.rs` (spoken commands), `crates/app/src/live_loop.rs` (starting and
stopping capture), `crates/app/src/mcp_frames.rs` (`screen_frames`) and `crates/app/src/drawing.rs` (`look`).

Depends on `capture`, `heimdall-archive`, `heimdall-home`, optional `listen`.

## Tests

- `cargo test -p screen`. A few tests take 1 to 2.5 s (hardware codec).
- Ignored, run by hand: `chunk_metadata_cost::frontmost_metadata_cpu` and `still_probe::still_of_the_main_display`
  (needs Screen Recording already granted; never asks).
- `cargo run -p screen --example island_exclusion` (needs Screen Recording).
