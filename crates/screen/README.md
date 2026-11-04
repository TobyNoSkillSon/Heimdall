# screen — recording and screen commands

ScreenCaptureKit recording of every display into the archive store (hardware HEVC), and deterministic spoken screen
commands (phrase matching, holds, screenshot attachments). Capture never requests permissions. Behaviour, matching
rules and costs: `docs/screen.md`.

**Code map:**
- `macos.rs` — `ScreenCapture` (`start_recording`, the SCK owner thread, gates, display notifications), `grab_still`,
  full-resolution grabs, `screen_permission_granted`.
- `buffer.rs` — `ScreenBuffer`, the recording facade (the name is historical; no RGB ring): `recording` writes the
  real store, `new` is an isolated lossless replay fixture, `idle` the store-less placeholder while Seeing is off.
  `ScreenConfig` (fps, half-native or a resolution cap), `select`, causal frame choice.
- `gate.rs` — `CaptureGate` (sleep, lock, brightness). `gaps.rs` — diagnosis only: one log line per display when frames reaching the archive leave a gap, naming what SCK sent meanwhile.
- `hold.rs` — `ScreenHold` / `ScreenHolds`: pin a range at detection, grab natively, `attach_held` on approval.
- `fullres.rs` — `attach_command_screenshot` (MCP `screenshot_now` and tests).
- `commands.rs` — `SCREEN_PHRASES`, `PhraseMatcher`, `PhraseMatch`, `near_misses`, `ScreenCommands`
  (`process_listen_event` behind feature `listen-adapter`, `process_utterance`, `process_utterance_live`), `Shot`,
  `ShotKind`, `Action`.
- `lib.rs` — `EXCLUDED_WINDOW_TITLES`, `is_excluded_window`.
- `bin/heimdall-screen-probe.rs` — post-permission cost probe (`docs/screen.md`, "Checks and costs").

Encoding, decoding and tiering live in `heimdall-archive` (`video.rs`, `video_native.rs`). Depends on `capture`,
`heimdall-archive`, optional `listen`.

**Testing:** `cargo nextest run -p screen`; a few tests take 1–2.5 s (hardware codec). One ignored CPU-cost lab test.
