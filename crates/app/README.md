# Heimdall application

**Domain: the app — shell and conductor.** `main.rs` (CLI, `main`, UI callbacks), `live_loop.rs` (capture run,
permissions, listening/screen control), `replay_modes.rs` (offline WAV/idle/smoke modes), `pipeline/` (the conductor,
see its README), `bridge/` (agent glue, see its README), plus settings, diagnostics, health, watchdog, providers
(`api_providers.rs`, `inference_engine.rs`; docs/models.md), background jobs' model and
gate (`background_model.rs`, `background_gate.rs`), archive/search glue (`archive.rs`, `index.rs`, `embeddings.rs`,
`mcp_*.rs`) and speech-split glue. The map of all domains is `docs/architecture.md`.
**Testing:** `cargo nextest run -p app` (pipeline is the slow part, see `pipeline/README.md`). Main-level
test modules are in `main_tests/`. Full suite with the RAM disk: `scripts/check.sh --tests`.

`heimdall` owns the capture/listen/speakers/screen/decide/agent/speak/UI pipeline.
First launch opens the existing four-permission setup window. Capture and screen
services start only after its explicit setup action. Passive permission checks and
offline modes never request permission. Local inference is prepared before live
capture admission; TTS loads on the first reply. Discord VAD SpeechStart activates
a bounded speaker worker (up to 81.92 seconds, 5 MiB per source at 20-ms
packets) that owns the helper, inference, naming and quiet timer. Intake and ASR
continue during loading and inference;
provisional speaker labels are corrected in the UI/index once diarization catches
up. Its 15-second quiet deadline
finishes and reaps the helper even when capture stops. The app consumes the
shared diarization trait through CallScopedSpeakers, preserving naming evidence
in the parent while model allocations leave with the helper. No audio callback
runs inference.

Listening ▸ off (persisted as `paused`, on its own stop latch beside Agent's) stops microphone and app-audio
capture, the listen worker's sources and d1; the agent, speech and the screen stream keep running on their own
switches, and the agent is never told (each switch turns off only itself). The archive gets a `gap` per open source (reason "heimdall off") and a session `heimdall_off` marker, then
`heimdall_on` when turned back on (`launched off` when it starts paused). Seeing ▸ Recording choices persist as `screen`
(resolution, fps) and reconfigure the live recording, starting a new codec epoch (docs/screen.md).
The Agent window is fed from dry-run sends or the live Pi transport recorder, through `agent::view`.
The single owned Pi RPC process uses the selected model (the local engine's or an added API model, docs/models.md;
`api_providers.rs` holds keys, catalog and menu rows), its own pinned Node/runtime/config and an explicit persistent
session. Open Agent actions and streaming types are documented in `docs/agent-ui-handoff.md`.
Transcript lines use device/app names; capture gaps stay in the archive and only actionable
ones (permission revoked, device disconnected, archive stopped) show once as a plain line with the clock time.

Dry-run (Agent off) is the default and a persistent menu setting. It uses a logging harness,
never starts a provider or MCP service, and settles its synthetic delivery ledger.
TCL addressedness follows decide's advisory flag while its current export is unfit;
d1 send probability and observed quiet control dispatch, including retained thoughts.
Finals have labelled transcript/word/decision records in the session archive;
listen segments and gaps are separate indexed events. Screen command occurrences
use the utterance ID established at SpeechStart, including continued segments.
A standalone `spójrz` waits for finalization. Captured Heimdall playback is context
and cannot wake the agent. Mic SpeechStart interrupts the speech queue directly
from the listen event forwarder, even while a decision is running.

The archive retains every source and model output. The first 24 hours use native
lossless PCM; older audio and evidence compact continuously, with speech/unknown
coverage in FLAC, non-speech in Opus, and exact zero duration markers. Retention
keeps everything. Legacy archive-format settings cannot change this policy.
Raw capture reserves at most 64 KiB from a fresh free-space check, consumes that
credit per write, and expires it within one second. Near the floor it checks each
write. Index/screenshot writes still probe directly (10 GiB initially, 1 GiB minimum).
The existing 16 MiB safety margin and one-second native commit deadline remain;
large concurrent external writes can still exhaust free space between probes,
and any write error stops archival explicitly.
Archive failures and delayed compaction surface in the menu without claiming that
listening stopped. Source disabling discards queued inference and raw archival,
and reenabling rejects older event timestamps.

Independent menu statuses use `UiUpdate::ComponentStatus`: ASR/turn fallback,
decision fallback, speech fallback/failure, each speaker helper's fallback/failure,
TCL advisory failure, archive pauses/compaction failure, and agent engine-wait,
process-recovery or reconciliation state. A healthy component cannot clear another's
warning; paused/setup states retain priority. Detailed failures stay in transcript
notes or the agent line; the status header uses short messages.

Offline validation uses `--replay WAV_OR_DIRECTORY`. It forces dry-run and an
isolated data directory under the macOS temporary directory unless `--data-dir`
is supplied, with fake mic/Discord/app sources. `--ui` optionally
shows transcript output with fake permission status. `--realtime --streams N`
feeds 20-ms chunks from an independent paced producer into the bounded live
worker. Its queue holds 768 chunks per source (15.36 s at 20-ms packets; at most
150 MiB for 32 sources at the largest allowed 1,600-sample chunk, about 30 MiB at
the usual 320 samples; `pipeline/sources.rs`), sized for the 14.02-second
cold-backend backlog a settings-swap replay measured on 12 Oct. Overflow remains
an indexed gap with the raw audio kept. `--source mixed --streams 3`
includes mic partials, Discord diarization
and an app final-only stream. Each file is finalized before the next file; the
measurement includes the final inference drain. This is an offline capture
simulation, not post-permission microphone/screen/power validation.

`--macmon PATH` adds rootless macmon power brackets; process CPU is sampled every
100 ms, including owned speaker helper CPU with libproc counters retained before
reaping. Parent and helper CPU seconds are also reported separately. Model
preparation is timed separately, then ten seconds of digital silence
bracket speech. macmon watts cover the busy whole machine, not this process.
`--idle-seconds 10..60` measures the no-capture/no-model resident core plus passive
HAL discovery and Mach wakeups, excluding AppKit windows. Measurement receipts and
private replay inputs stay in ignored `target/release-candidate/` directories.
Earlier worker evidence and archives are retained under
`lab/eval/data/release-candidate/retained-targets/` after build cleanup.

`--models` selects a runtime model root; installed bundles resolve
`Contents/Resources/models`. `scripts/bundle.sh` copies only verified runtime
assets and active paired turn/send policy exports. It signs the speaker helper,
nested native code and the app with the stable development
identity. It never installs, launches, or resets permissions. The coordinator
owns fresh review, dispatch-quality acceptance, real UI/setup and post-permission QA.
