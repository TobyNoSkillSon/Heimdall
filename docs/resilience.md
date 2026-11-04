# Resilience: what can fail, how Heimdall notices, how it recovers, what the user sees

Heimdall runs all the time, so when it fails it either fails in a way it can recover from, or shows that something is
not right. A watchdog catches freezes, but only real ones: the part must actually be frozen.

## Rules

- **Frozen, not quiet.** A freeze is work waiting with no progress: a microphone that is open but delivers no audio
  (silence still delivers samples), a listen worker holding queued audio while finishing nothing, a live loop that
  stops handling its 2-second tick, a UI thread that stops running a block posted every 10 s. Silence, Listening off,
  an idle agent and a sleeping Mac (uptime clocks, re-baselined at wake) never count.
- **Ladder, never a loop.** Restart the part, then restart listening (in-process Off/On, models kept), then relaunch
  Heimdall. Each part's rungs count within 10 minutes. Automatic relaunches: at most 2 per 30 minutes, counted across
  launches in atomically replaced `<data>/recovery.json`. An unreadable history or a failed durable count write
  forbids automatic relaunch and shows the red problem pill. Past every bound: a red pill with what to do.
- **Relaunch keeps everything.** The callback closes delivery synchronously and queues an independent writer for
  `<data>/stop-intent.json`. Automatic relaunch waits for the latest intent to be durable before exit; a failed or
  stalled writer blocks relaunch with the red pill. The final flush and exit hold the same acceptance lock as
  menu and settings-file intents: no newer intent can be accepted between them. Helper failure releases
  the lock and keeps the app alive. Launch combines settings with a stop-only recovery overlay before delivery
  initializes; a persisted On never overrides settings Off.
  Settings saves follow the latch and retire only the same durably saved generation. Committed archive records
  are on disk. A rung-3 relaunch from a working loop goes through a normal Quit (drains audio and deliveries);
  one from a frozen loop exits after 1.5 s and loses only the uncommitted tail of the current archive commit. The
  relaunch runs through LaunchServices (`open Heimdall.app`), so macOS attributes permissions exactly as when the user
  opens it: no permission prompt.
- **Quit always ends**: once teardown makes no progress for 8 s (each finished step and each bounded wait counts),
  and at most 30 s after the click, the process exits with `_exit` (no exit handlers: ggml's Metal destructors abort
  while a model is loaded). Quit first preempts archive compaction (`compact::preempt_all`, which also lifts the
  compactor out of background QoS), so teardown writes never wait behind a compaction step.
- **Switches never wait for the archive**: Listening on creates its capture segment on the intake thread; Listening
  off waits at most 2 s for the last archive commit (Quit settles the rest); Seeing on waits at most 2 s for its
  stream and a later tick adopts a slower one; Seeing off stops the stream on its own thread. Screen starts and
  stops run in one ordered lane, and a start cancelled before its turn never starts.
- **Hang evidence**: a UI thread or live loop silent past 2 s writes every thread's stack to `logs/hangs/` (one per
  thread per 10 minutes, newest 20 kept) and a summary line to heimdall.log (`crates/app/src/hang.rs`).
- **Swap and GPU watch** (`crates/app/src/resource_watch.rs`, read once a minute): when macOS swap reaches 2 GB the
  agent gets one notice (a wake) naming the swap, Heimdall's own total and the three largest processes, so it can
  hold the heavy jobs it started; it warns again only after swap fell under 1.5 GB, and at most once an hour. With
  Agent off the warning is a `memory:` log line only. Every 10 minutes in which the GPU averaged 25% or more, or
  Heimdall's processes used 5% of GPU time, one `gpu:` line gives the device's utilisation, Heimdall's GPU time per
  part (the Memory row's parts: Voice, Search Index, Speakers, Local Model, Islands, and Heimdall's own process, "Listening & Decider",
  where Parakeet, the decider, Smart Turn and Supertonic run together) and the three busiest other processes. The
  source is the IORegistry's per-client `accumulatedGPUTime` (Activity Monitor's GPU Time). The cross-task memory
  guard for workers is not Heimdall's code.

Code: `crates/app/src/watchdog.rs` (freeze watchdog, relaunch), `crates/app/src/pipeline/resilience.rs` (checks and
lower rungs in the live loop), `crates/app/src/health.rs` (de-duplicated health, ladder, microphone switch),
`crates/app/src/preflight.rs` (agent pre-flight), `crates/ui/src/island/` (model and renderer) (the island).

## What the user sees

| State | Look | Renders (`lab/ui-renders/`) |
|---|---|---|
| Restarting | Orange reload glyph, "Restarting transcription…", with a static orange outline until it ends | `island-health-restarting*.png` |
| Back | Green check, "Transcription OK", 1.7 s, then gone | `island-health-ok.png` |
| Needs the user | Red pill, two lines (what is wrong, what to do), a static red outline while it is held; menu-bar helmet red; a click runs the action and collapses it to a red glyph (outlined too). Never a command or a raw error: those go to the log | `island-health-problem-*.png`, `island-health-alert-problem.png` |
| Part down, returns by itself | Orange, said once (2.6 s), then a small glyph at the notch until it returns | `island-health-drop-mic.png`, `island-health-alert-drop.png` |
| Notice | Accent colour, 3.2 s: microphone switch, harness update | `island-health-mic-*.png`, `island-health-agent-updated.png` |

Health shows whether or not Heimdall listens; Listening off clears the listening parts' conditions (the agent's,
speech's and the screen's stay: those keep running). Commands keep working
beside it: a heard command or its result takes the island for its moment, then the health pill returns (no second
pulse). A health message that arrives while a command or another message holds the island waits its turn and then
shows for its full time; a message stays readable for at least 1.2 s before one about something else replaces it,
while one about the same condition (a restart's OK) replaces it at once. At rest a restart outranks
a problem, a problem outranks a pending command, and the small glyph comes last. The Listening
glyph is the helmet, with a ring while the models load (`island-listening-*.png`).

## Failure catalogue

| Failure | Detected by | Recovery | Island | recorded as |
|---|---|---|---|---|
| Chosen mic unplugged, Bluetooth dropped, or removed while recording | HAL device-list listener (event, no polling) | Capture opens the built-in mic in the same pass; back to the chosen one when it returns | "AirPods Pro → MacBook Pro Microphone"; back after it stays 5 s: "MacBook Pro Microphone → AirPods Pro" | `source_removed` / `gap` with device reason; `microphone_cue` and `health` switch transition |
| Chosen mic present but failing (no callbacks for 3 s, or start error) | Capture's HAL owner | Retries at 2, 4, 8, 16 s; **the built-in mic records between retries** (before: up to ~40 s of nothing); after 4 failures built-in until a HAL event or the user's choice | The same switch notice, once (retries never flicker it) | `source_removed` / `gap` with start/callback reason; `microphone_cue` on fallback |
| An extra mic the user switched on disconnects | Capture's microphone report | It reopens when it returns; the primary is unaffected | "<mic> disconnected" once, then an orange mic glyph at the notch; "<mic> back" | `microphone_cue` and `health` disconnect/recovery; `source_removed` cause |
| System default input changes | — | Ignored by design: Heimdall uses the user's choice, else built-in; no restart, no gap | Nothing | None: deliberately ignored, no failure |
| Default output, sample-rate or format change | HAL route and format listeners | Sources restart with the new format in the same pass (tens of ms) | Nothing | `source_removed` / `gap` route/format reason; next `source_added` |
| No microphone at all (built-in gone with the lid closed, chosen one absent) | Capture's report: nothing in use | Waits for a HAL event | Red "No microphone · Click to pick one in the menu" (opens the menu); "Microphone OK" | `microphone_cue` and `health` missing/recovered |
| Capture stuck (an open mic delivers nothing for 15 s) | Live-loop tick; microphones only (an app tap is silent when its app is); skipped when the loop itself was busy | Restart capture → restart listening → relaunch; a restarted capture is watched until one of its microphones opens | Orange "Restarting the microphone…"; "Microphone OK" when audio flows | `capture_stalled` silent duration/rung; `recovery` reason; `health` |
| Listen worker dies (crash, panic, model error) | Its end marker | Restart, ≤3 per 10 min → relaunch | Orange "Restarting transcription…"; "Transcription OK" when its models are ready; red "Transcription keeps stopping" past the relaunch bound | `listen_worker_ended` reason/restart decision; `health` failure/recovery |
| Listen worker hangs (queued audio, nothing finished for 30 s; models not loaded in 2 min) | Worker progress probe (`ListenWorker::progress`) on the tick | Left behind (a model call cannot be interrupted), its output discarded, a new worker started, same bound → relaunch | Same as above | `listen_worker_hung` reason/waiting work; `recovery`, `gap`, `health` |
| Decider does not load | At use | Next try after 10 s, doubling to 5 min (before: a multi-second reload at every decision); agent starts regardless | "Commands paused · reloading" (orange glyph); after 3: red "Commands and replies are off" (click restarts Heimdall); "Commands OK" | `decider_load_failed` raw error/attempt/retry delay; `health` |
| Decider owner panics, exits or hangs | Independent supervisor; response disconnect/panic, or 1 s + the request's UTF-8 token upper bound (capped at 3,000 ms; the model call itself measured p99 93 ms, max 205 ms, 25-26 Oct); loading has 30 s, the first 3 requests after a load 10 s, checked every 100 ms. Training rows are written on their own thread, outside the deadline (the 26 Oct hangs were 6 s archive writes after 72 ms answers) | Replace only the owner and retry the same in-flight request with its original one-shot completion. From the third failure within 10 min the replacement pauses before it loads: 30 s, doubling to 5 min; meanwhile its requests fail at once (sentences stay pending, the agent is still woken by name). Never a relaunch: an agent answer in flight is never lost to the decider. At most 3 owner threads hold a model; past that a replacement waits for a stuck one to finish. Abandoned results cannot execute commands or send | `decisions`: orange "Commands paused · reloading"; while paused "Commands paused · decider not answering"; "Commands OK" at the replacement's first answer. A replacement that cannot start: red "Commands and replies are off" (click restarts Heimdall) | `decider_owner_failed` raw cause/attempt/`pause_s`, diagnostics problem/invariant, `decision_archive_failed`, `health` |
| Decider inference fails | At the call sites | 3 in a row: reload at the next decision | Orange "Restarting decisions…"; "Commands OK" | `decision` / `command_decision` error; `decider_reloaded`; `health` |
| Screen stream stops or errors; display change | Screen crate's own recovery events | Its retries; display changes reconfigure the stream; an ended stream owner restarts once per 10 min | Orange "Restarting screen capture…" → "Screen capture OK"; waiting for a display: "Screen capture paused"; second end: "Screen capture stopped" | `health` component `screen` raw SCK/SQLite/owner/pause/retry event; `video_gaps`; `screen_command` / `command_action` keeps retrieval cause; recovery clears pill and component status |
| Permission revoked (Microphone, Screen Recording while Seeing, Accessibility) | Passive status read when the user switches apps; never a prompt | Capture keeps its consent; returns when granted | Red "Microphone permission is off · Click to open Settings" (opens the pane); "… permission back" | `health` permission-specific denied/back transition |
| System Audio Recording revoked | Taps deliver exact silence for 10 s while a recorded app plays | As above | Red "System Audio permission is off", same click | `health` `permission:system` inferred denial/back transition |
| Disk below the free-space floor | Disk check on every archive write | Recording pauses and resumes by itself | Red "Disk almost full · not recording" (click opens the archive); "Recording again". Any other archive stop: red "Archive database is damaged" (SQLite corruption) or "Archive stopped · not recording", both "Click to show it in Finder"; the error itself goes to the log and the journal | `health` component `archive` raw free bytes/floor cause and recovery |
| Archive database locked or I/O error | Archive pipeline stops (capture stops with it: nothing is recorded unarchived) | Restart capture (reopens the archive) → restart listening → relaunch | Orange "Restarting recording…"; red "Recording keeps stopping" past the bound | `gap` raw archive-pipeline reason; `recovery` ladder/trigger; `health` |
| Archive store damaged (SQLite "malformed" / "not a database") | Archive pipeline stops, or the launch fails opening the store | None: the archive is one store, so a restart or relaunch reopens the same file | Red "Archive database is damaged", "Click to show it in Finder" | `archive_stopped` and `health` raw cause if the store accepts writes; otherwise stderr only (damaged store cannot journal itself) |
| Sleep and wake | Capture and screen stop at sleep, restart at wake | Watchdog clocks pause in sleep and re-baseline at wake | Nothing | `health` screen pause cause / recreated-display recovery; `video_gaps`; capture `gap` / source lifecycle |
| Live loop freezes (the 13 Oct 11:16 hang: stuck inside one handler) | Its 2-s tick not handled for 60 s (180 s before the first) | Relaunch | Orange "Restarting Heimdall…", then "Heimdall OK" from the new process; red "Heimdall keeps freezing · Click to restart it" past the bound | `health` watchdog stall reason; `recovery` relaunch request/blocked cause; durable `recovery.json` attempt; `relaunched` |
| UI thread freezes | A block posted to the main queue every 10 s not run for 60 s | Relaunch | Same | `health` watchdog UI-stall reason; `recovery` relaunch request/blocked cause; `relaunched` |
| Live loop ends on an error | Its thread returns | Relaunch within the bound | Orange "Restarting Heimdall…"; red "Heimdall stopped" past it | `health` Heimdall failed with full returned error before Listen Failed/relaunch; `recovery`, `relaunched` |
| Quit hangs | 8 s without teardown progress, 30 s at most | Process exits (no relaunch) | Nothing | `health` watchdog quit-stall reason before forced exit (best effort if archive is writable) |
| Owned Pi/Node missing, corrupt or wrong version | Read-only pre-flight of the runtime sealed in the app (files, receipt, exact Node and Pi versions) | None in place: nothing is downloaded or repaired | Red "Agent runtime is missing from Heimdall" or "Agent runtime in Heimdall is damaged" · "Reinstall Heimdall" | `agent_preflight` full failure; `agent_start_failed`; `health` |
| Pi crash/exit, broken pipe, malformed/oversized/truncated RPC, or active turn without progress | EOF/framer/write/inactivity deadline (180 s in a turn, 600 s in a tool, 15 min in a compaction); stderr is not progress | Stop/reap process group; same active session; three durable restart attempts per ten minutes with backoff; a restart that opens resets the failure count | Orange "Restarting the agent…"; red "The agent could not recover · Check free disk space, then reload it in Open Agent" only when the restart budget refuses; "Agent OK" | `agent_transport_*`, `agent_reply`, `agent_start_failed`; `health` agent-supervisor error and restart/stop status with raw cause |
| A compaction dies with Pi (its silence limit, a crash, a stop wait cut at its limit) | Supervisor: Pi ended while `compaction_in_progress` | Counted once per process; Heimdall's own compactions held 10 min, then 20, from the third until Pi completes one (`bridge/compaction_hold.rs`). Before 27 Oct each restart re-summarised the same 230K-token history and was killed again, six times, each billed | The Pi restart's cues above | `agent_compaction` `killed` (kills, held_s, error); `agent_compaction_killed` diagnostic |
| Wake failed or interrupted (provider error, Pi crash, reload, app restart) | Live: the delivery's ledger state (a tool started → `effects_possible`). After a restart: Pi's session file (receipts, and the `heimdall_effects` record written before each tool runs) | Resolves itself, never waits for a click: a successful receipt completes; any sign a tool started settles it as done without replay (exactly-once for tool effects); no sign of a tool resends it once with its speech marked as repeated, and a second failure hands its speech to the next wake. Wakes and typed text arriving meanwhile queue. The Agent-window review stays only for a delivery recovery could not decide | Restarting pill while Pi restarts; one notice if the turn will not answer ("Agent couldn't answer; your words go with the next request", "Agent stopped partway after an error") | `agent_delivery_resolved` (id, outcome, reason); durable delivery ledger and `agent_reply` receipts |
| Same tool with identical normalized arguments and identical result, consecutively | Pi extension hooks count five completed identical call/result pairs; the sixth matching call is blocked before execution with a tool error; changing output or an intervening tool/command breaks the streak | First block explains how to escape; two more blocked attempts in the same user turn make the supervisor clear both queues and abort, preserving the Pi session. Counters reset on settlement/reload/Off. One `agent_loop_guard` journal record carries tool, SHA-256 arguments hash, repeat and blocked counts | One notice “Agent stopped a repeating loop” and one Agent-window line; possible prior tool effects stay in review | `agent_loop_guard` tool/hash/repeat/blocked counts; `agent_reply` loop-stop notice |
| Agent Off during a turn or blocked stdin write | Shared stop generation/permit notification | Revoke at once, stop process group/tools; old queued notice epochs cannot revive after On; epoch saved in intent/settings. A running compaction or state write finishes first (`Stopping`, up to `COMPACTION_WAIT` 15 min of Pi silence / `WRITE_WAIT` 10 s); a model switch parked behind that compaction is kept for the next Agent on | Agent caption "Stopping after compaction…" or "Stopping after saving…", then Agent Off; uncertain performed effects remain in review | `agent_stop` / `agent_restart_wait` (waiting, stopped, cap, interrupted); `settings_applied` / stop-intent durable generation; `health` |
| Reload during a turn / reload_me from a tool | Supervisor command | Wait for tool result and true settlement; restart on the active session; reread extensions/settings/MCP/prompt; timestamped note without extra inference | Open Agent shows pending reload and the reason | `agent_reload` reason/note; agent transport/session receipts |
| Long session grows past 32 MiB | Settlement file-size check | Compact, stop Pi, durably archive entire original, atomically retain safe checkpoint/recent active branch; no historical tool-result edits | Compaction/reload in Open Agent; a disk/unsafe-checkpoint failure is visible | Archived complete Pi session in store logs; compaction receipt; `health` supervisor failure if maintenance fails |
| Quit/watchdog relaunch while Pi exists | stdin closure, process-group cleanup, inherited OS session lock and parent guard | Clean stop or wait for orphan retirement; never two Pi processes on one session | Restart state, or honest failure after the restart budget | `health` watchdog/relaunch cause; `agent_process_ownership` cleanup capability; durable session owner |
| Agent ledger/config/session write fails or disk is full | Durable write result | No fabricated success, dropped receipt or blind replay; keep prior pointer/session; stop/retry within budget | Red recovery/review pill; disk action points at free space | `health` supervisor raw durable-write failure and stopped/review status; prior ledger/session kept |
| Agent runtime thread ended | Tick | Restart the runtime → after 3 in 10 min a problem | Orange "Restarting the agent…"; red "The agent keeps stopping" | `agent_runtime_ended` rung; `health` restart/result; raw restart failure via pipeline health |
| Agent can't answer: spent quota or credit, rejected sign-in twice, 3 provider errors in 10 min, 5 min without an answer | `agent::emergency::Detector` in the supervisor | Emergency mode (`docs/emergency.md`): turns held as context, a T3 cover thread answers through `heimdall-emergency`, probes until the agent answers, then hands back | Notice "Agent out of quota till 03:04 · backup agent on it, nothing to do"; "Agent back" and "I'm back." | `agent_emergency`; `emergency:` lines |
| Speech synthesis fails | Speech queue failure event | Next reply tries again | "Speech unavailable" (orange glyph); "Speech OK" | `tts_error` raw synthesis cause; `health` speech component/drop/recovery |
| settings.json invalid | Settings watcher | Last good settings stay | "settings.json is invalid · using the last good one"; "Settings OK" | `health` settings parse/apply cause, invalid/back cue; settings-save failure/recovery |
Pipeline and agent health cues use the existing `health` evidence kind. Component status records carry
`component`, `state`, `reason` and the previous message. Screen-owner events preserve the raw SCK cause and
retry information, including lock/asleep/no-display pauses; a recreated stream with at least one display clears
both the component status and held health cue. A configuration acknowledgement alone cannot clear capture health.
Failed T3 connections and unavailable confirmations also record their returned cause as `health`.
Component status and agent health are de-duplicated, including changes to a cause behind an unchanged label;
screen pause repeats are de-duplicated too. Normal ticks append nothing.

The archive is also the destination for these diagnostics. A corrupt/full/unwritable database, or its mutex held by
a frozen thread, can prevent a diagnostic commit. This is reported to stderr; no success is fabricated and the
watchdog does not wait for the frozen thread's session mutex. Its independent diagnostic commit gets at most
250 ms before recovery/forced Quit continues; a late commit may complete while the process stays alive.
Recovery attempts also have `recovery.json`.
The independent rotating stdout/stderr sink below remains available when the archive cannot journal itself.

## Screen health gotchas

From an earlier inspection:

- A screen recovery clears its health cue only with a nonzero display count (`Pipeline::screen_event`); configuration
  success alone never clears a capture error.
- Every explicit video write transaction begins `IMMEDIATE`. A deferred transaction that read first and then wrote
  failed at once with SQLITE_BUSY_SNAPSHOT (517) beside a concurrent audio commit, bypassing the 60-second busy
  timeout: "Screen database is locked".
- Video sample retrieval uses a read-only connection per pack; a failed screenshot keeps the archive/SQLite cause
  instead of only "no screen frame".
- Lock/asleep/no-display stops are explicit pause events; exhausted retries wait for an OS refresh, or for the
  archive catching up (a drained backlog refreshes a stream that gave up, whether or not the island showed it).
- The archive's "Catching up" island shows only a real fall-behind: the oldest unsaved note waiting 10 minutes, or
  writes dropped. A backlog that drains within minutes (routine under load: 36 s behind, nine
  islands in ten minutes) shows nothing (catching up is fine as long as it keeps up over the
  span of a couple of hours).
- An epoch's first row that finds the archive busy (`live::Busy`, its 2 s wait over) is no stream failure: the stream
  stays, that display's frames are dropped without asking the archive again, and recording resumes once the writer
  has reached the abandoned begin (at most a minute later it tries anyway). A 110 s archive stall once spent
  all five restarts in 28 s and the screen stayed off after the archive caught up.

## Agent query: what went wrong today

Use `heimdall-archive timeline` instead of hand-written SQL/decompression. It emits JSON Lines in local time
order, with decoded payloads, stable evidence ids, session/source/utterance ids, and host/wall timestamps.
Final `utterance` lines come from the transcript projection and include their decoded decision; their negative
ids are projection row ids, **not evidence ids**. Other lines retain the original positive evidence id.

```sh
heimdall-archive timeline --since 10:28 --until 10:34
heimdall-archive timeline --since 10:28 --until 10:34 --problems
heimdall-archive timeline --since 2026-10-16T10:28 --until 2026-10-16T10:34 --kinds agent_wake,agent_send,agent_reply,agent_state,agent_transport_reply,invariant_violation
```

The default archive is `<data>/archive` in Heimdall's normal Application Support directory. `--archive ROOT`
selects an isolated archive. `HH:MM[:SS]` means today in the Mac's local timezone; `YYYY-MM-DDTHH:MM[:SS]`
selects a date. The interval includes `--since` and excludes `--until`; omitting `--until` includes later events.
Low-level `model_output` rows are omitted by default; request `--kinds model_output` explicitly. `--problems`
examines these too, and selects structured errors, failed/stalled/restarting/unavailable transitions, memory
refusals, diagnostic drops and invariant violations. Recovery/clear/intentional-stop transitions remain in the
full timeline and must be retained when reconstructing an outage. Decode failures print the affected evidence
and payload ids and error, rather than silently dropping an event.

All persistent database handles (index **and packs**) are opened `SQLITE_OPEN_READ_ONLY`. Existing live WAL
commits are included in a read transaction; temporary projection tables stay in memory. The reader never calls
schema initialization, checkpointing, retention or the ordinary writer-backed pack helpers. It takes a shared
lock on the **existing** `.compact.lock` to keep old blobs alive. A busy compactor yields an explicit retry error,
and historical packs without an existing lock are refused. WAL-format databases lacking existing WAL/SHM
sidecars are refused before SQLite opens them, rather than creating sidecars or ignoring committed WAL data.
For a closed historical database without sidecars, prepare a consistent isolated snapshot first. No lock file,
archive directory, WAL or SHM file is created by the reader. Live SQLite read-lock bookkeeping still uses the
existing SHM; this is not an immutable filesystem read or a content snapshot across separately invoked commands.

`video_gaps` and `chunk_embedding_errors` are projections, not evidence events. Screen, archive and embedding
health transitions appear in this timeline; the current embedding retry table is not misrepresented as a history.

## Problem context and sparse behavioural checks

The common evidence insertion boundary enriches structured problem records from both synchronous journals and
the bounded evidence writer. `system_context` records the actual sample wall time, installed source commit,
free/wired/compressed bytes, available-memory **estimate** (free + inactive + purgeable), swap used and its delta
since the preceding problem sample, macOS memory pressure level, CPU load averages, thermal state, battery/AC
source and battery percentage. It captures Heimdall's RSS and its voice
sidecar's RSS without launching a command or querying any model. Inaccessible readings are null or
absent, never zero masquerading as a measurement. No command arguments are stored. Packaged builds read their
bundled `source-commit.txt`; a nonpackaged binary honestly reports `unbundled`.

Runtime context contains the five effective switches, agent runtime/queue/status and voice readiness; switch
snapshots carry their host timestamp and update on settings evidence and the existing two-second health tick.
Context limits agent ids to sixteen short references and error/state copy to 512 bytes, with an explicit
truncation flag; the original state evidence retains the full details. Thus a context sample is timed explicitly; it does not claim
that every component snapshot was taken atomically. Bounded writer records sample when inserted by its worker,
not by blocking the live loop on a system probe. Synchronous journal writes sample on their existing writer path.

Checks emit `invariant_violation` with the original evidence ids and input numbers:

- Agent completion signals (`agent_end` and successful `heimdall_receipt`) without an exact completion after
  **five seconds**. The signal ids merge into one condition and it reports once until settlement.
- Agent queue depth above one, once on entry; recovery permits a subsequent violation to be reported.
- Decider, command, voice-reply and voice failures, once per failure run; memory-refusal numbers stay in the
  original record and in the linked violation.
- Duplicate image ids in one wake, and repeated screenshot references for the same utterance.
- Command execution without a matching final cue dispatch, or a successful cue dispatch without command
  execution, after five seconds; final command records include the correlation id. Listening cues are excluded
  from that pairing.
- Pipeline health cue dispatch without the corresponding health evidence record.

Idle ticks append nothing. State is bounded, contended monitor locks are skipped rather than waited on, and
ordinary diagnostic enqueues use the existing bounded nonblocking writer; rejected enqueues are counted by
`diagnostic_drops`. These checks diagnose evidence; they do not change when a decider wakes or sends a turn.
They are process-local checks, not a replacement for the agent's durable recovery ledger.

**Dispatch is not visibility.** `command_cue` and `health_cue` explicitly say `stage: dispatched` and whether a UI
was attached. Detecting an island cue replaced before its first rendered frame, or a health pill rendered without
an event, requires the island renderer's own presented/replaced callbacks. This engineering branch does not
change island drawing or assert that a dispatched cue was seen. The UI integration owner must add that evidence;
until then those two renderer invariants remain unverified.

## What `heimdall.log` says

One plain line for every failure, fallback, retry, timeout and state change, each starting with its local time
(`2026-10-19 18:48:32.390 voice: voice stopped: …`), so a problem can be found by time and read without the
archive. Evidence records become lines at the archive's front door (`diagnostics_lines.rs`, on the caller's thread):
a stuck or full archive still leaves its line. Covered: component health and its recovery (every `health` record:
listening, decider, speakers, screen, speech and its GPU fallback, the voice, archive, settings, capture
gaps), island health cues and notices, the voice's load, unload, memory refusals and exits (with the guard's reason),
every rejected voice take (what was heard) and every switch to the lighter voice (notice, sentence, reason), the
agent's state changes and problems, agent start/stop/reload/queue fallbacks, the decider's wakes and failures, barge-in
outcomes, lighter-voice failures, failed invariants, relaunches, capture stalls and diagnostics, speaker-helper loads,
T3 connection changes and failed tool calls (`tool: …`; the full call is in `tools.log`). A line that loops (a memory
refusal every 30 s, a flapping part) is written once per 10 minutes with a count; distinct events never fold.
Lines are bounded and masked: API keys, bearer tokens, `api_key=`/`token:`/`password` values and long base64/hex runs.
The per-turn JSON stream (each final with its decider record and 8.6 KB prompt, every agent payload) goes to stdout
only in replay and CLI runs; the installed app leaves it to the archive. On 19 Oct that stream filled the 8 MiB
rotation in about 4 hours; the same day replayed through the new lines is 467 lines, 48 KB.

## Independent stdout/stderr capture

Normal application startup redirects stdout and stderr (including panic output and inherited helper output)
to a pipe consumed by a dedicated sleeping reader. It puts the local time before every line (empty lines stay empty;
a line split across reads gets it once) and persists each chunk to private `<data>/logs/heimdall.log`,
rotating at **2 MiB** into `.1`, `.2`, `.3`: at most **8 MiB** across four files, including a continuous output
burst. No archive lock, schema, model or network service is involved. The reader has no idle timer. Normal exit
allows at most 250 ms for the final drain; force-exit/power loss can lose an unconsumed pipe tail. A helper that
keeps stdout open cannot hold application exit. A logging write failure falls back to the original stderr and
keeps draining, so a dead log reader cannot break application output; if the filesystem cannot accept writes,
durability cannot be promised. Failure to initialize this sink is reported without stopping the application.

The subprocess regression starts with stdout/stderr attached to `/dev/null` (the Finder case), exercises both
journal paths, and verifies durable stdout, stderr, a panic, bounded rotation, problem context and linked invariant
evidence. No Finder launch, installed bundle, real recording, GPU inference or user profile is exercised.

## Agent pre-flight

Agent On checks the runtime sealed inside the app on a helper thread (`preflight.rs`): files, the integrity receipt
and the exact Node and Pi versions. Nothing is downloaded or repaired; a missing or damaged runtime says "Reinstall
Heimdall". Each check has a deadline. There is no account login or cloud pre-flight. Off during the check wins.

## Not covered, and why

- **A hung listen worker's memory.** A model call cannot be interrupted, so the hung thread and its models (~2 GB)
  stay until the process ends. That is why hang restarts share the 3-per-10-minutes bound that leads to a relaunch.
- **A permanently stuck native decider call.** Its Rust owner can be abandoned but cannot be killed safely. A replacement retries the same request; if the process-wide native lock is still held, repeated deadlines reach the relaunch ladder and reclaim the native state at process exit. Quit never joins the hung owner.
- **A capture owner stuck inside Core Audio.** The stall check restarts capture; if dropping the stuck capture blocks
  the live loop too, the watchdog relaunches. Not exercised against a real stuck HAL call.
- **Observation API errors** can remain in the Agent window instead of reaching the island; the supervisor's
  `health` record and `agent_delivery` record retain the cause.
- **Installed-app verification** belongs to integration: this branch exercises the actual Pi supervisor in an
  isolated data directory and does not launch the installed app or touch its permissions.
- **Device and permission faults are not exercised live.** The decisions (stall vs quiet, ladder, bounds, cues,
  pre-flight, microphone switch, relaunch helper) are tested in code and through the real `Pipeline`; unplugging a
  device, revoking a permission and a real freeze were not performed on the installed app.
- **The resting indicator** (something always at the notch while Heimdall runs) is two offscreen proposals,
  `island-resting-dot*.png` and `island-resting-helmet*.png`, not wired.
