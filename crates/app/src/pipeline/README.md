# pipeline: the conductor

`Pipeline` is the live state machine from captured audio to agent turns: one struct (`mod.rs`) and one
`impl Pipeline` block per concern. Behaviour lives in the topic pages linked below; this is the map.

## Entry points

- Built and driven by `crates/app/src/live_loop.rs` and `crates/app/src/replay_modes.rs`: `Pipeline::new`,
  `capture_event`, `worker_event`, `action`, `apply_settings`, `finish`.
- Talks to the agent through `bridge::AgentRuntime` and to the UI through `ui::Ui`.
- Journal writes (`self.index.note`, `event`) never block the loop: they go to the archive's background writer
  ([archive.md](../../../../docs/archive.md)).

## Files

| File | Owns |
|---|---|
| `mod.rs` | types, the `Pipeline` struct, construction, `ui()`, `finish()`; test modules declared at its foot |
| `sources.rs` | running and paused, sources arriving and leaving, microphones, the listen worker's lifecycle and events, Discord relabelling, gaps, `source_readded` ([capture.md](../../../../docs/capture.md)) |
| `utterances.rs` | listen events into utterances, admission before commands, finals handed to the agent, waiting calls |
| `commands.rs` | spoken commands: proposals from partials, window decodes and finals, the decider's verdict, execution, island cues |
| `send_worker.rs` | the decider worker (send, wait, not for Heimdall) and its foreground bounds ([decision-chain.md](../../../../docs/decision-chain.md)) |
| `decider.rs`, `decider_swap.rs` | the Decider section's state and the decider hot-swap (precision or version), ordered with every decision |
| `hotkeys.rs` | the live decider label keys: ⌃⌘\ forces a wake, ⌃⌘' asks for a suggestion; both kept as labels |
| `heard_twice.rs` | one utterance heard on two microphones reaches the decider and the agent once |
| `speaker_tracks.rs` | transcript lines, speaker tracks and identities ([speakers.md](../../../../docs/speakers.md)) |
| `voice_names.rs` | quiet speaker naming: the voice namer's check in calls, its "Is this John?" card, the shared rename path |
| `transcript_naming.rs` | naming a "Speaker N" by double-click in the transcript window |
| `listen_to.rs` | listening to call guests: each named voice's listen flag in `speakers.json`, the agent's `listen_to` |
| `mic_voice.rs` | who speaks on the user's microphone: each utterance judged against the user's voice print and Heimdall's voices (`mic_voice` evidence) |
| `heimdall_voice.rs` | Heimdall's own voices enrolled silently, the user's voice learned from wakes, the speaker encoder helper, the barge-in voice check |
| `barge_in.rs` | the user talking over Heimdall stops it, matched by speaker ID every half second |
| `output_route.rs` | headphones or speakers, read from Core Audio each time a barge-in arms |
| `echo_guard.rs` | Heimdall's own voice heard through the speakers is archived only (`echo_guard` evidence); a backup to `bridge/echo_cancel.rs` |
| `playback.rs` | sound from the Mac's own output heard back on the microphone is not the user |
| `ack.rs` | the instant acknowledgement, `Settings::ack_delay_ms`, and the island's send arrow held to the same moment (`hold_island`) |
| `drops.rs` | drops onto the notch: joined to the user's open turn within `GRACE` (3 s), else sent alone |
| `vella.rs` | dictation to Vella: dictated lines never wake ([vella.md](../../../../docs/vella.md)) |
| `suggest.rs` | the in-conversation suggester's window, card and pick ([suggester.md](../../../../docs/suggester.md)) |
| `speech_split.rs`, `speech_split/` | speech correction and per-word attribution (CPU helper; `speech_split/replay.rs`) |
| `settings_apply.rs` | applying and saving settings, starting the local engine, the menu's values ([settings.md](../../../../docs/settings.md)) |
| `engine_models.rs` | the menu's Engine section: precisions, downloads, deletes, which one loads ([inference-engine.md](../../../../docs/inference-engine.md)) |
| `actions.rs` | `ui::UiAction` handling |
| `menu_problem.rs` | the menu's orange line: only problems the user can act on |
| `resilience.rs` | each part's health, freeze checks, the lower recovery rungs, the agent pre-flight ([resilience.md](../../../../docs/resilience.md)) |
| `catch_up.rs` | the island's "catching up" while the archive's write lock holds writes back |
| `model_downloads.rs` | first-run model downloads; listening starts when they land ([models.md](../../../../docs/models.md)) |
| `setup_ready.rs`, `setup_phrase.rs` | Setup's Getting ready rows (`<data>/setup/status.json`) and its last spoken step ([setup.md](../../../../docs/setup.md)) |

## Tests

- `cargo nextest run -p app -E 'test(/^pipeline::/)'`, or `cargo test -p app pipeline::`. This is the slowest domain.
- The plumbing tests need `scripts/check.sh`'s RAM-disk `TMPDIR` (the archive commits with `F_FULLFSYNC`); on the SSD
  some exceed nextest's 120 s limit.
- Unit tests live in `tests/unit/` by topic, helpers in `tests/unit/mod.rs`. Scenario files sit in `tests/`
  (`plumbing_tests.rs`, `resilience_tests.rs`, `start_latency_tests.rs` and per-feature files), each declared at the
  foot of `mod.rs`. `send_worker_tests/` and `speech_split_tests/` hold those modules' tests.
