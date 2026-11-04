# Screen recording and screen commands

How `crates/screen` records every display into the archive and how spoken screen commands pick their frames. Code
map: `crates/screen/README.md`. The menu rows: `crates/ui/README.md` (Seeing ▸ Recording). Where the video lives in the
store: `docs/archive.md`.

Beta: recording can stop after an archive lock; restart listening if the archive stops growing. Memory search needs macOS 26 or newer on Apple silicon.

## Recording

- ScreenCaptureKit captures every connected display at half native pixels (3456 × 2234 → 1728 × 1116), 5 fps.
  Coded sizes are always even: a requested 1117-row height came out 1116 from the encoder, so SCK is asked for the
  even size (`ScreenConfig::size`).
- No RGB history is kept. A bounded three-surface SCK queue, one encoder per display and one transient RGB frame
  replace the old ring (removed 14 Oct).
- Capture checks existing permission (`CGPreflightScreenCaptureAccess`) and never requests it.
- Gates: Seeing off stops capture. Asleep, locked or login-window displays and known zero-brightness panels give no
  footage. Console and display-asleep state are read without permissions; lock, sleep, wake and display
  notifications invalidate the capture generation, and a callback re-checks the gate before publishing. External
  panels without a brightness query fall back to display-asleep; the built-in panel uses a read-only DisplayServices
  query.
- Excluded windows: Heimdall's own windows titled `Heimdall Command Island`, `Heimdall Draw Overlay`, `Heimdall Pointer`
  and `Heimdall Drop Target` never appear in a recording or a grab (`screen::EXCLUDED_WINDOW_TITLES`; drawing mode
  composites the user's strokes into the image instead).
- Seeing off swaps in `ScreenBuffer::idle()`, a placeholder with no store, and clears pending command holds; footage
  already recorded stays. Building a real empty store there held the live loop 2 s of fsync (26 Oct 23:01).
- Screen starts and stops run in one ordered lane off the live loop (`docs/resilience.md`).

## Encoding and storage

- VideoToolbox hardware HEVC only: no software fallback, no external codec process. Quality 0.8, no frame reordering,
  a keyframe at least every 60 s (and every 300 frames).
- One-minute codec epochs; a resolution or rate change or a capture gap starts a new one. An epoch whose first row
  meets a busy archive leaves one closed, empty row; the display's frames wait (the stream kept) until the writer is
  back, then a new epoch starts (`docs/resilience.md`).
- Each encoded sample is its own fsynced pack blob (`v/<sha256>`) plus an index row: crash-safe without a MOV trailer.
  A crash loses only the in-flight frame; committed samples decode from the epoch's saved HEVC parameter sets.
  A dead owner's open epochs are closed at the next recording start, after checking the recorded PID (another live
  writer is left alone).
- SCK idle frames extend the previous sample's coverage without encoding or storing a blob.
- Clock: SCK presentation times converted from `SCStream.synchronizationClock` into the host clock with
  `CMSyncConvertTime`, the same clock as audio, words and evidence. Join by session + host time. Invalid Core Media
  times are refused, never converted to zero.
- Tiering: once a whole epoch is 24 h old, the compactor re-encodes it to HEVC quality 0.5, at most 1 fps, only on AC
  power after five minutes without input (unknown power or idle closes this gate). At most four epochs per pass; the
  gate is checked between frames; new samples are decoded and fsynced before one short publication transaction; an
  interrupted re-encode keeps the hot footage. A pending command's pinned epochs are not tiered underneath it.

Settings: `screen.resolution` (default `half_native`; `p540`, `p720`, `p1080`, `native` stay valid) and `screen.fps`
(default 5, accepted 1–30). `ScreenSettings::ui/from_ui` map them one to one onto the menu's choices.

## Reading footage back

- `heimdall_archive::video` (`range`, `at`, `samples`, native `Decoder`) and the store catalogue reconstruct frames.
- MCP `screen_frames` (`crates/app/src/mcp_frames.rs`): an archived time is exact (the frame at it, or the last one
  before it). `timeline_search` prints whole-second times, so a whole-second `at` with nothing at or before it means
  that second: its first frame, from a segment that began inside it. Never the second's last frame.
- Movies of any window: `archive_clip` / `heimdall-clip` (`docs/archive-clip.md`).
- Screen states for search are indexed live and in bounded catch-up (`docs/chunk-index.md`).

## Screen commands

A spoken phrase ("look at this", "zobacz to", `SCREEN_PHRASES` in `crates/screen/src/commands.rs`) attaches what was on
every display when the phrase started.

**Timing.** Phrase start is the first word's `start_ns`, which listen has already snapped to the first Silero-voiced
32 ms block (`listen::onset`; Parakeet alone starts a word after silence 0.15–0.45 s early). `PhraseMatch.start_ns` selects and labels the frame; `trigger_ns` records
completion. A phrase spans at most 4 s from its first word's start to its last word's start (Parakeet stretches a last
word's end over trailing noise).

**Holds.** At detection the app pins a recording range (`ScreenHold`, `crates/screen/src/hold.rs`) under the archive
lifetime lock, keeping no RGB: `LOOKBACK_NS` 5 s before the detection, detections within 1 s merged, at most 32 holds
and 512 MiB of held native pixels. The full-resolution grab starts at detection on its own thread (75–90 ms per
display). On approval `attach_held` decodes the phrase-time frame and writes PNGs.

**Frame choice.** Per display, the last frame at or before the phrase start whose coverage still includes it; never a
later frame, never an old one from before a gap. A missing display or a start outside the recording fails the
attachment (`ScreenActionEvent::Failed`), shown in the transcript.

**The attachment.** `ScreenActionEvent::Action` carries the note `[screenshot 14:02:11 attached]`, `image_path` (first
display) and `shots` for every display, each with display id and frame time. The original utterance stays beside the
note. Voice commands and MCP `screenshot_now` add one fresh native-resolution PNG per display
(`attach_command_screenshot`; `Shot.kind` `ring` = the recorded frame, `full_resolution` = the grab; `ShotKind::Ring`
is a wire name kept for compatibility). A full-resolution `frame_ns` is the grab's receipt time, not the phrase time.
The agent gets the full-resolution frame instead of the recorded one only when it was grabbed within 1.5 s of the
trigger word (`FULL_RESOLUTION_SAME_MOMENT_NS`); a later grab (a phrase first heard in the final, grabbed at the
decision) comes second, beside the phrase-time frame, and the note says both times: `[screenshot 21:28:16 attached,
and a sharper one from 21:28:24]` (bug note 2026-10-25T21-31-53).
Grab deadlines: 5 s per capture, 10 s for display enumeration. Grab failures (permission, the 20 MiB PNG limit, a
generation change) keep the recorded PNGs and go to `full_resolution_errors` (archived, logged, returned to MCP as
text). A wake, stop or reconfiguration during the grab discards its results.

**Files.** PNGs are written 0600 in 0700 directories: voice commands under `<data>/run/<session>/shots/`, MCP
`screenshot_now` under `<data>/shots/`. `archive_screenshot_action` stores each image in the archive's SHA-256 image
store, references it from the journal, then removes the PNG.

### Phrase matching

- Matching folds case and Polish accents, keeps token boundaries, and allows one edit in one word of five or more
  characters. Polish imperatives must match exactly after folding (`spojrzą`, `spojrzy`, `zobaczy` never fire).
- `zobacz to` allows at most two hesitation words between (`yyy`, `eee`, `um`, `uh`, `no`, `proszę`), commas included.
  Content words and negation are never skipped. Long pauses and sentence boundaries stop a match.
- The longest overlapping alias wins. A partial that could still extend a short alias waits: `spójrz na to` fires
  when `to` arrives; a lone `spójrz` fires at the final or on a word that rules out the longer alias.
- Negation before the phrase in the same clause suppresses it (`don't`, `wouldn't`, `shouldn't`, `couldn't`, `can't`,
  `cannot`, `won't`, `didn't`, `doesn't`, Polish equivalents); a sentence end or contrast (`but`, `ale`) resets it.
- An English command is an imperative (statements that merely mention looking or taking a screenshot raised false cues). Its phrase opens its clause (utterance start, sentence or comma break, a
  pause over 1 s, `but`, `so`) after at most four lead words (interjections, the name, `please`, `just`, `like`,
  `look`); or follows a request frame (`can/could/would/will you`, `want/need/like you to`, `let's`, `go ahead and`);
  or joins another command with `and`/`then` and starts with an imperative verb from a fixed list (`open Safari and
  take a screenshot`). Inside a statement it never fires (`we should take a look…`, `he told me to look at this
  invoice`).
- Quoted speech never fires, in either language: a phrase right after a speech verb (`said`, `told`, `asked`,
  `powiedziałem`, `mówi`, `zapytałam`…), optionally with the person told (`told him,`, `powiedziałem mu,`), or after
  `was like` (22 Oct benchmark: 9 of 9 such lines fired before the rule). `Like I said, …`, `jak mówiłem, …` and `I told
  you, …` still fire.
- Polish phrases and other tables (the app's cancel/stop phrases, `COMMAND_OTHER_PHRASES`) keep the older rule (no
  observed false triggers). `PhraseMatcher::new(table)` runs the same rules over any table.
- `near_misses(table, words)` scans finished words offline for phrases that did not fire (`partial`, `negated`, `loose`,
  `embedded`); it never triggers.
- Words are borrowed, never filtered.

### Utterance lifecycle

- Command identity is source + stable utterance ID + covered word positions, never ASR timestamps, so revisions,
  finals and re-decodes cannot refire it. IDs stay recorded until a source discontinuity (no 60 s expiry).
- `ScreenCommands::process_listen_event` (feature `listen-adapter`, enabled in the app) needs the source's
  `SpeechStart`, partial, final and discontinuity events. A speech start after an internal pause keeps the current ID
  until the final; a completed final keeps its scope on replay.
- Without the adapter use `process_utterance` (or `PhraseMatcher::observe_utterance`) with an explicit ID and final
  flag. Legacy `process_partial` and `observe` treat each source as one cumulative transcript: never feed them word
  arrays that restart positions per utterance.
- The app routes only eligible sources and speakers to the matcher: app playback never triggers a command.
  `process_utterance_live` is the live path; replay and `process_utterance` never touch real capture.

## Checks and costs

- `cargo nextest run -p screen` covers capture gates, phrase-start selection at 5 fps, holds without RGB, display and
  generation races, the disk guard, minute rotation, idle coverage, a hardware codec round trip, tiering, real SIGKILL
  recovery, full-resolution fallback, and 78 labelled utterances plus archived lines
  (`archive_lines_fire_only_as_commands`: 24 commands that fire, 11 statements that do not).
- Not driven live: unlock, sleep and brightness transitions and several physical displays (gate and generation
  coverage only).
- Probe: `heimdall-screen-probe --after-grant` with `--recording`, `--full-resolution --commands N --data-dir NEW_DIR`,
  `--seconds 10–30`, `--powermetrics` (tries `sudo -n`; GPU stays unmeasured without it). Run it inside the
  authorized signed app identity; if preflight refuses, stop: never prompt, reset TCC or grant the probe separately.
- Measured (busy M5 Max, one internal display): recording at 1728 × 1116, 5 fps used 3.47 % of one core against
  0.0033 % capture-off and grew the store 1.95 MB/hour on a mostly static desktop (14 Oct; a changing desktop gave
  161.5 MB/hour); a native grab took a median 78 ms per display (12 Oct). Process CPU only; watts unmeasured.
