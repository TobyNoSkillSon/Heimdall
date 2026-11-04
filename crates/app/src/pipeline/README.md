# pipeline — the conductor

`Pipeline` is the live state machine from captured audio to agent turns. One struct (`mod.rs`), one `impl Pipeline`
block per concern:

| File | Owns |
|---|---|
| `mod.rs` | types, the `Pipeline` struct, construction, `ui()`, `finish()` |
| `sources.rs` | running/paused, sources arriving and leaving, microphones, the listen worker's lifecycle and events, Discord relabelling, gaps; a device back within 5 s under a new generation journals `source_readded` (`device_key` strips only the generation of a `kind:name:generation` id, so `screen:1` and `screen:2` stay two devices; docs/capture.md) |
| `utterances.rs` | listen events into utterances, admission before commands, finals handed to the agent, waiting calls |
| `commands.rs` | spoken commands: proposals from partials, window decodes and finals, the decider's verdict, execution, island cues |
| `speaker_tracks.rs` | transcript lines, speaker tracks and identities |
| `voice_names.rs` | quiet speaker naming: the voice namer's minute check in calls, its "Is this John?" card, the renames every naming path shares |
| `transcript_naming.rs` | naming a "Speaker N" by double-click in the transcript window |
| `listen_to.rs` | listening to guests: each named voice's listen flag (`speakers.json`), the agent's `listen_to`; a listened guest's own line counts like the user's |
| `mic_voice.rs` | who speaks on the user's microphone: each utterance embedded whole and judged against their voice print and Heimdall's voices; Heimdall's voice archived only, another voice "Unknown" and never a wake (`mic_voice` evidence) |
| `settings_apply.rs` | applying and saving settings, the local engine, the menu's values; Seeing off swaps in `screen::ScreenBuffer::idle()`, a placeholder with no store (docs/screen.md) |
| `actions.rs` | `ui::UiAction` handling; refreshed provider figures are saved and handed to the agent's supervisor like a thinking change (`Command::Thinking`), so the next start uses them without interrupting a turn (docs/models.md) |
| `menu_problem.rs` | the menu's orange line: only problems the user can act on |
| `send_worker.rs` | the decider worker (d1 send/wait/not-for-Heimdall) and its foreground bounds |
| `ack.rs` | the instant acknowledgement: an ack clip as the wake goes, or after `Settings::ack_delay_ms` of quiet (0 by default); the island's send arrow and thinking dots held to the same moment (`hold_island`, `UiUpdate::IslandHold`); the wake's `[Heimdall already said: …]` or `[Heimdall is saying: …]` line; talk-over stop. The line passes `voicebank::ack_gate` (only when their words ask for what it claims), and no ack plays when replies come right behind it |
| `barge_in.rs` | the user talking over Heimdall: their voice, matched by speaker ID every half second, stops it; its echo and other voices never do (no transcript, no decider) |
| `output_route.rs` | headphones or speakers, read from Core Audio each time a barge-in arms |
| `echo_guard.rs` | Heimdall's own voice through speakers: mic words aligned, fuzzily, with the sentences playing then are its echo, archived only (`echo_guard` evidence); the user's words over it stay their. Backup to Apple's echo cancellation (`bridge/echo_cancel.rs`) |
| `heimdall_voice.rs` | Heimdall's own voices: each voice that can speak enrolled silently from lines it renders (never played), kept in `speakers.json`; the user's voice learned from their wakes (`user-voice.json`); the speaker encoder in its own helper; the barge-in's voice check and the whole-utterance verdict (`mic_verdict`) |
| `drops.rs` | drops onto the notch: held while the user's utterance is open or starts within `GRACE` (3 s), then joined to that turn (their words, the drop's lines, its images, one wake); otherwise alone as their typed message (`crates/app/src/drops.rs` writes the lines) |
| `vella.rs` | dictating to Vella (docs/vella.md): Vella's notifications into `crate::vella::Link` (intervals on the host clock, its keybind, the 45 s timeout on the settings tick), the island's Vella token, and the dictated final: an utterance on the user's microphone that dictation covers by half is archived (`dictated_to_vella`), observed and added to the conversation as `[dictated to Vella] …`, and never sent early, a spoken command, decided, a wake, a barge-in or an ack talk-over stop. Triggers are judged per word (a word is dictated when its middle lies inside a dictation): in any line, only words outside dictation can name Heimdall, say "send it", follow up, stop or be a command, and words said after they stop still wake; d1 and the agent read dictated runs as `[dictated to Vella: …]` |
| `heard_twice.rs` | one utterance heard on two microphones reaches the decider and the agent once |
| `suggest.rs` | the in-conversation suggester's window, card and pick (`crate::suggester`; docs/suggester.md) |
| `speech_split.rs` | speech correction and per-word attribution (CPU helper; `speech_split/replay.rs`) |
| `resilience.rs` | each part's health, freeze checks, the lower recovery rungs, the agent pre-flight (docs/resilience.md) |

**Interface:** constructed and driven by `live_loop.rs` / `replay_modes.rs` (`Pipeline::new`, `capture_event`,
`worker_event`, `action`, `apply_settings`, `finish`). Talks to the agent through `bridge::AgentRuntime` and to the
UI through `ui::Ui`. Journal writes (`self.index.note`, `event`) never block the loop: notes go straight to the
archive's background writer (docs/archive.md).

**Testing:** `cargo nextest run -p app -E 'test(/^pipeline::/)'`. This is the slowest domain. The plumbing tests need
`scripts/check.sh`'s RAM-disk TMPDIR (the archive commits with F_FULLFSYNC); on the SSD some exceed nextest's 120 s
limit. Unit tests live in `tests/unit/` by topic, helpers in `tests/unit/mod.rs`; scenario files sit in `tests/`
(`plumbing_tests.rs`, `resilience_tests.rs`, `start_latency_tests.rs` and per-feature files), each declared at the
foot of `mod.rs`; `send_worker_tests/` and `speech_split_tests/` hold those modules' tests.
