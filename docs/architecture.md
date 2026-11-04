# Heimdall architecture

Heimdall is a Rust macOS assistant. It listens to the user's microphone and recorded apps, transcribes locally, decides
locally when something is meant for it, hands those turns to an agent (Pi, on an API or local model), and answers
through the notch island, speech and tools. One app binary (`crates/app`, bin `heimdall`), native AppKit UI through
objc2; the Floating Islands helper is a second Rust app bundled inside it.

## Repository layout

- `crates/` — shipping source, one crate per domain where the boundary is clean (table below).
- `islands/` — the Swift Floating Islands app, kept as the rollback for the Rust port in `crates/islands`
  (`ISLANDS_HELPER=swift` in `scripts/bundle_islands.sh`); `islands/README.md` is the file protocol both follow.
- `skills/` — the skills bundled into the app (`heimdall`, `public`, `shared`, `workers` sets; `docs/skills.md`).
- `scripts/` — build, check and release: `bundle.sh` (editions, `docs/editions.md`), `check.sh` (full check,
  `docs/install.md`), `verify_release_candidate.py`, `bundle_models.py` and its locks, `bundle_islands.sh`,
  `check_public_bundle.py`, `install_watchdog.sh`, and their tests.
- `docs/` — current reference; `docs/README.md` indexes it.
- `lab/` — experiments: prototypes, renders, research and measurement scripts. Not part of the public tree; nothing in `lab/` is built or shipped.
- `bugs/` — bug notes (`docs/bugs.md`). `models/` (ignored) — pinned model files. `target/`, `dist/` (ignored) —
  build output and signed candidates.

Rules for working here: each domain keeps few dependencies; duplicating a small helper beats a shared utility
crate; a domain's tests run on their own (`cargo nextest run -p <crate>`); every fixed bug leaves a test.
Heimdall runs in Rust; remove Python wherever it can go.

## Domains

| Domain | Home | Owns |
|---|---|---|
| Capture | `crates/capture`, `crates/screen` | microphone and app-audio taps, host clock, raw archive writes; screen recording and screen commands |
| Transcription | `crates/listen`, `crates/speakers` | Silero VAD → Parakeet ASR (Core ML + Metal) → Smart Turn; diarization and voice identity (`heimdall-speakers-worker`) |
| Decider | `crates/decide` | the native d1 model: send / wait / not-for-Heimdall and command decisions |
| Conductor | `crates/app/src/pipeline/` | the live state machine from audio to turns: sources, utterances, commands, acks, echo guard, speech split, UI actions |
| Agent harness | `crates/agent`, `crates/app/src/bridge/` | the Pi process and its ownership, durable delivery ledger, runtime pins, MCP host, skills, wakes; app glue: supervisor, tools, delivery, speech glue, window feed |
| Providers | `crates/agent/src/{provider,console,chatgpt}.rs`, `crates/app/src/api_providers.rs` | API model catalog, keys, limits, ChatGPT sign-in |
| Background jobs | `crates/app/src/{background_*,plan_refresher,suggester,voice_namer,day_timeline}.rs` | Messages API calls outside Pi (`background_call.rs`): plan refresher, suggester cards, voice namer, day timeline; their quota gate |
| Emergency mode | `crates/emergency-cli`, `crates/{app,agent}/src/emergency.rs` | when the agent cannot answer, a T3 cover thread takes the turns through the `heimdall-emergency` CLI (bundled in `Contents/MacOS`) and a scoped MCP server (`docs/emergency.md`) |
| T3 orchestration | `crates/t3` | outside-client T3 Code control: pairing, worker threads, orchestrator pass-through, wakes |
| Speech | `crates/speak`, `crates/voicebank` | Phonon S TTS, playback; `voicebank` (pre-generated voice lines and the clip store) is personal edition only |
| Island | `crates/ui/src/island/`, `crates/rsvp` | the notch object: pill, notes, questions, pull-down, fast reader (timing from `rsvp`), drawing overlay, pointing |
| Floating Islands | `crates/islands`, `crates/app/src/islands/` | the `FloatingIslands.app` helper and `islands` CLI; the app's side: tools, plan lists, helper supervisor, notch nudge |
| Menu and windows | `crates/ui` (outside `src/island/`) | menu bar item and menu, setup, transcript, speakers, agent window, offscreen renders (`heimdall-ui-demo`) |
| Archive and search | `crates/archive`, `crates/app/src/{archive,index,embeddings,mcp_timeline,mcp_frames,mcp_clip}.rs` | the single archive store, timeline chunks, compaction, export, migration, `heimdall-clip`; search for the agent |
| Night | `crates/night`, `crates/app/src/{night_*,speech_split}.rs` | personal edition only: overlap separation and training export; the bug-note CLI (`docs/bugs.md`) |
| App shell | `crates/app/src/{main,live_loop,replay_modes,settings,diagnostics*,health,watchdog,edition}.rs` | CLI, the live loop, offline replay modes, settings file, diagnostics, freeze watchdog, edition switch |
| Evaluation | `crates/eval` | offline evaluation binary, writes only under `lab/eval/data` (`crates/eval/README.md`) |

Crate dependency graph from the `Cargo.toml` path dependencies (arrows point at dependencies):

```
app (bin heimdall)
 ├─► ui ────────► rsvp
 ├─► t3 ────────► agent
 ├─► agent
 ├─► decide
 ├─► speak
 ├─► voicebank ─► speak, archive   (personal edition only)
 ├─► night ─────► listen, speakers, archive   (personal edition only)
 ├─► screen ────► capture, archive, listen (feature listen-adapter)
 ├─► capture ───► archive
 ├─► speakers
 ├─► listen
 ├─► archive
 └─► emergency-cli (dev-dependency only: tests drive the real CLI)
eval ───────────► listen, decide, screen
leaves: agent, archive, decide, emergency-cli, islands, listen, rsvp, speak, speakers
```

## Data flow: microphone → decider → agent → island

```
Core Audio tap / mic ──CaptureEvent::Audio──► live_loop pump ──► Pipeline::capture_event      (capture → conductor)
        │                                                     └─► raw archive (capture)
        ▼
listen worker: Silero VAD → Parakeet ASR → Smart Turn ──ListenEvent──► Pipeline::listen       (transcription)
Parakeet partial / window / final text ──phrase match──► pipeline::commands (proposal + screen hold → island "heard" cue)
        │
        ▼
pipeline::utterances: turn text + context window
        ├── spoken command ──► decide (d1 command verdict) ──► execute: screenshot / cancel
        └── turn end ──► send_worker: d1 send / wait / not_for_heimdall                         (decider)
                              │ send
                              ▼
bridge::AgentRuntime ◄── Command::Observe / Wake ──  admit_agent_final                          (agent harness)
        │  agent::Bridge<RecordedAdapter> → Pi RPC → provider (API model)
        │  every payload archived before transport; delivery ledger is durable
        ▼
Pi tool calls ──MCP──► bridge::Tools (screenshot, timeline search, island notices/questions, speak,
                                      drawing/pointing, floating islands, T3 workers via crates/t3)
        │
        ├──► ui::Ui.send(UiUpdate::…) ──► island (pill, note, question, pull-down, reader)       (island)
        └──► speak (Phonon S) ──► playback, level tap ──► island voice dots (speech)
```

UI actions travel back the other way: `ui::UiAction` (menu, island clicks, drawing Send) → `Pipeline::action`
(`pipeline/actions.rs`) or the agent supervisor.

## Testing

- Fast and static first: unit and scenario tests run in milliseconds (`cargo nextest run -p <crate>`); offscreen
  renders and frame checks for UI (`heimdall-ui-demo --render/--gallery` write to `lab/`); fake providers and fake
  T3/Pi fixtures instead of paid calls.
- `#[ignore]` tests are explicit lab runs (real models, real Pi, real T3) that need an environment variable or local
  assets; each crate's README names them. Pi-dependent tests find the runtime through `HEIMDALL_AGENT_RUNTIME`, else
  `target/agent-runtime/current` (`crates/agent/README.md`).
- `scripts/check.sh --tests` runs every workspace test under nextest with a RAM-disk `TMPDIR`; `scripts/check.sh`
  also builds, signs and verifies the personal candidate (`docs/install.md`). Pipeline and archive tests depend on
  that RAM disk: the archive commits with `F_FULLFSYNC`, and on the SSD some exceed nextest's 120 s limit. The
  conductor (`app` pipeline) is the slowest domain.

Each domain's README has its own "Testing" lines. Start there.
