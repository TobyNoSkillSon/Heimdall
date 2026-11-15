# app: the heimdall binary

The `heimdall` menu-bar app: CLI and entry modes, the live loop, the conductor pipeline, agent glue, settings, background
jobs, archive and search glue, setup, updates and diagnostics. It owns the capture, listen, speakers, screen, decide,
agent, speak and UI pipeline; each of those is its own crate.

Read this when: you need to find where the app wires something together, add a command-line mode, run an offline
replay, or locate the app-side half of a feature.
Code: `crates/app/src/`; the crate map of the whole workspace is [architecture.md](../../docs/architecture.md).

## Entry points

`src/main.rs` checks these first-argument modes before the normal launch, each prints and exits:

| Mode | What it does | Code |
|---|---|---|
| `setup …` | a coding agent drives Setup through the running app | `setup_cli.rs`, [setup.md](../../docs/setup.md) |
| `--schedule-wake …` | writes one wake into the data folder's schedule | `agent::wakes`, [mcp.md](../../docs/mcp.md) |
| `--background-probe [raw]` | one small real call on the background model | `background_probe.rs`, [background-call.md](../../docs/background-call.md) |
| `--suggest-replay …`, `--suggest-export …` | the suggester's offline replay and label export | `suggester_replay.rs`, [suggester.md](../../docs/suggester.md) |

Then `args()` parses the launch flags (`heimdall --help` prints them):

- `--replay WAV_OR_DIR [--source mic|discord|app|mixed] [--ui] [--realtime] [--streams 1..8] [--macmon PATH] [--data-dir DIR]`
- `--idle-seconds 10..60 [--data-dir DIR]`: no capture, no permissions.
- `--models DIR`: runtime model root; an installed bundle uses `Contents/Resources/models`, a checkout `models/`.
- `--set-dry-run true|false`, `--free-space-floor-gib N`: write the setting and exit.
- `--lab-scenario FILE --data-dir DIR`: the post-install check's UI instance on an isolated profile (`lab_run.rs`).
- `--night-replay-smoke`, `--night-lab-worker`: night diagnostics, explicit isolated `--data-dir` required.

Other binaries: `speech-split-replay` (`src/speech-split-replay.rs`), and the lab-only `goals-lab`
(`src/bin/goals-lab.rs`, [goals.md](../../docs/goals.md)) and `heimdall-vella-*-lab` (`src/bin/`). Features: `friends`
(default), `public` (the public edition, [editions.md](../../docs/editions.md)), `lab-timeline` (`mcp_timeline_live.rs`).

## Where things live

| Area | Files | Behaviour |
|---|---|---|
| Live loop | `live_loop.rs` (capture run, permissions, settings reload, Listening and Seeing switches), `replay_modes.rs` | [capture.md](../../docs/capture.md), [screen.md](../../docs/screen.md) |
| Conductor | `pipeline/` | [src/pipeline/README.md](src/pipeline/README.md) |
| Agent glue | `bridge/`, `mcp_actions.rs`, `mcp_timeline.rs`, `mcp_frames.rs`, `mcp_clip.rs`, `pi_session.rs`, `preflight.rs`, `context_policy.rs` | [src/bridge/README.md](src/bridge/README.md), [mcp.md](../../docs/mcp.md) |
| Settings | `settings.rs`, `settings_tool.rs`, `identity.rs` | [settings.md](../../docs/settings.md), [personalise.md](../../docs/personalise.md) |
| Models and providers | `api_providers.rs`, `inference_engine.rs`, `inference_engine/`, `model_store.rs`, `relay_catalog.rs` | [models.md](../../docs/models.md), [inference-engine.md](../../docs/inference-engine.md) |
| Background jobs | `background_call.rs`, `background_model.rs`, `background_gate.rs`, `background_t3.rs`, `background_fallback.rs`, `background_wire.rs`, `night_runtime.rs` | [background-call.md](../../docs/background-call.md) |
| Islands and plan | `islands/`, `island_agent.rs`, `plan_refresher.rs`, `goals/`, `chat_history.rs`, `day_timeline.rs`, `fast_read.rs`, `drops.rs`, `drawing.rs` | [islands-capabilities.md](../../docs/islands-capabilities.md), [one-system.md](../../docs/one-system.md), [goals.md](../../docs/goals.md) |
| Archive and search | `archive.rs`, `index.rs`, `embeddings.rs` | [archive.md](../../docs/archive.md), [chunk-index.md](../../docs/chunk-index.md) |
| Speakers and voices | `speakers_runtime.rs`, `speech_split.rs`, `voice_namer.rs`, `voice_suggest.rs`, `speech_voice.rs`, `own_voice.rs`, `voice_bank.rs` | [speakers.md](../../docs/speakers.md), [voices.md](../../docs/voices.md) |
| Relay and recovery | `emergency.rs`, `emergency_local.rs`, `emergency_recovery.rs`, `watchdog.rs`, `health.rs`, `hang.rs`, `resource_watch.rs` | [claude-relay.md](../../docs/claude-relay.md), [resilience.md](../../docs/resilience.md) |
| Diagnostics | `diagnostics.rs`, `diagnostics_lines.rs`, `diagnostics_log.rs`, `diagnostics_host.rs`, `latency.rs`, `metrics.rs` | [resilience.md](../../docs/resilience.md) |
| Setup, home, updates | `setup_wizard.rs`, `setup_cli.rs`, `home_change.rs`, `harness_autonomy.rs`, `updates.rs`, `edition.rs` | [setup.md](../../docs/setup.md), [home.md](../../docs/home.md), [harness-autonomy.md](../../docs/harness-autonomy.md), [updates.md](../../docs/updates.md) |
| Vella link | `vella.rs`, `vella_asr.rs` | [vella.md](../../docs/vella.md) |
| Suggester | `suggester.rs`, `suggester_replay.rs` | [suggester.md](../../docs/suggester.md) |

## Run an offline replay

1. `cargo run -p app -- --replay path/to/file.wav` (or a directory of WAVs). Replay always forces dry-run and an
   isolated data directory under the macOS temporary directory unless `--data-dir` is given, with fake mic, Discord
   and app sources and fake permission status.
2. Add `--realtime --streams N` to feed 20 ms chunks from a paced producer into the bounded live worker, or
   `--source mixed --streams 3` for mic partials, Discord diarization and an app final-only stream.
3. Add `--macmon PATH` for rootless macmon power brackets with process CPU sampled every 100 ms (owned speaker helper
   included); macmon watts cover the whole machine, not this process.
4. Check: each file is finalised before the next and the run ends after the final inference drain; receipts go to
   ignored `target/release-candidate/` directories.

This simulates capture offline; it does not validate microphone or screen permissions after a grant.
`--idle-seconds 10..60` measures the resident core with no capture or models (passive device discovery and Mach
wakeups included, AppKit windows excluded).

## Limits held here

- Replay and live worker queue: 768 chunks per source (`pipeline/sources.rs`), 15.36 s at 20 ms packets; overflow is an
  indexed gap with the raw audio kept.
- Speaker actor per source: a 4,096-packet queue (81.92 s, 5 MiB at 320-sample packets, `speakers_runtime.rs`); the
  helper is reaped after `speakers::call::QUIET_TIMEOUT` (120 s) of quiet, even when capture stops.
- Raw capture disk credit: at most 64 KiB reserved from a fresh free-space probe, spent per write, expired within 1 s
  (`archive.rs`); near the floor each write is checked. Floor, margins and retention: [archive.md](../../docs/archive.md).
- Captured Heimdall playback is context and cannot wake the agent; no audio callback runs inference.

## Tests

- `cargo test -p app` (or `cargo nextest run -p app`); the pipeline tests are the slow part
  ([src/pipeline/README.md](src/pipeline/README.md)). Main-level test modules are in `src/main_tests/`; module tests
  sit beside their files as `*_tests.rs`; fixtures in `tests/fixtures/`.
- Every workspace test with the RAM disk: `scripts/check.sh --tests`.
- Building a bundle: `scripts/bundle.sh` copies only verified runtime assets, signs the speaker helper, nested native
  code and the app, and never installs, launches or resets permissions ([install.md](../../docs/install.md)).

## See also

- [architecture.md](../../docs/architecture.md): the workspace crates and data flow
- [decision-chain.md](../../docs/decision-chain.md): dry-run (Agent off), barge-in and the send decision
- [agent-window.md](../../docs/agent-window.md): the Agent window and island card
