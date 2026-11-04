# Heimdall archive

How the archive store keeps every captured source, model output and screen frame, ages it, and gives it back. Code
map and writer rules: `crates/archive/README.md`. Capture's side: `docs/capture.md`. Video specifics:
`docs/screen.md`. Search index: `docs/chunk-index.md` (memory search needs macOS 26 or newer on Apple silicon). Movies of a window: `docs/archive-clip.md`.

Every captured source is kept, including sources not listened to and
Heimdall's own voice. No speech detector removes audio and there is no retention deletion. Agents operate the
archive and its tools.

## One store

`~/Library/Application Support/Heimdall/archive/` (lab runs use an isolated root). No per-launch folders, no loose
object files:

```text
heimdall.sqlite        the index: sessions, capture segments, sources, gaps, callback blocks, the audio-object,
                       image, chunk and log catalogues, evidence rows, payload/vector CAS, transcript, synthesized clips
packs/hot.sqlite       hot content: native PCM objects and original screenshots (rolling, about one day)
packs/YYYY-MM.sqlite   cold content of that UTC month: FLAC/Opus/zstd audio, cold screenshots, packed
                       metadata chunks, compressed agent logs, synthesized clip audio
.compact.lock          lifetime lock: readers shared; collection exclusive, one bounded batch, taken without waiting
.compact.owner         one compactor per store across processes (never taken by a reader)
```

- A session (one app launch) is a row. Code addresses it as `<archive>/<session-name>` (`session-<ms>-<uuid>`), a
  path that is never created on disk. A capture segment (one capture timeline) is a row under its session; block,
  evidence and utterance IDs are store-wide.
- Content keys are content hashes, so audio objects, screenshots, JSON payloads and vectors are stored once across
  all sessions.
- Why packs: the index stays small enough to query and back up; bulk bytes live in a bounded number of large files,
  one per month. Hot PCM has its own pack so a day of about 20 GB churn never moves pages in the index or a finished
  month. Month packs are almost append-only and already compressed, so there is no whole-file sealing.
- Per-launch working files that are not archive content (agent dry-run log and spool, temporary screenshot PNGs, the
  decide sink) live in `<data>/run/<session-name>/`. A clean finish archives the logs into the store's `logs` and
  removes the directory; the next launch does this for a crashed one.
- "Open Archive Folder" reveals `archive/` (`heimdall.sqlite` and `packs/`).
- SQLite is WAL; readers must include the WAL. Copying a live `.sqlite` alone is unsafe; use
  `export::snapshot_archive`.

## Durability and locks

- Content first, catalogue second; catalogue removal first, content second. A blob commits to its pack
  (`synchronous=FULL`, `F_FULLFSYNC`) before the index transaction that references it, and every publishing
  transaction re-checks under the index write lock that its blobs exist. A crash leaves at most an unreferenced blob.
- Removing content only queues `(pack, key)` in `gc_blobs`. The collector deletes a blob only under the exclusive
  lifetime lock and the index write lock, and only when no catalogue row references it. A publisher that finds its
  blob gone rewrites it (capture does when identical audio returns). An hourly sweep, bounded per pack, removes
  unreferenced blobs older than an hour; keys of unknown kinds are never touched.
- Opening the index or a pack writes nothing: `auto_vacuum` and WAL mode are set only when they differ (a new file).
  Setting them on every open was a write that waited for the shared lock and an fsync, and froze the live loop 2.0 s
  (26 Oct 23:01; `tests/open_no_write.rs`). Every connection sets `synchronous=FULL`, `fullfsync=ON`,
  `checkpoint_fullfsync=ON`, and caps the WAL at 64 MB after checkpoints (the index WAL sat at 102 MB, 27 Oct).
- One writer per process (`live.rs`, thread `heimdall-archive-live`, user-initiated QoS) makes every index write, in
  the order writes were handed over. Live jobs (journals, transcript, listen evidence, capture, screen, images, logs,
  synthesized clips) commit in groups (≤256 jobs or 200 ms, a savepoint each); background jobs (compaction publish
  steps, collection, embeddings) are one bounded transaction each and run when no live job waits, or once per 250 ms
  between live groups. Inside the process nothing polls SQLite's busy handler any more; only another process (a
  CLI) can make the writer wait, and it then keeps every job and retries (twenty writers on their own connections, 60 s waits and a compactor holding the lock for minutes once restarted
  the agent on "database is locked").
- Nothing on a live path waits long: `Journal::event` waits for its commit at most 1 s and only while the writer keeps
  up; `note` and listen evidence never wait; capture's committer and the clip writer wait for their own commit
  (`call`); `archive_log` waits at most 1 s, then its caller keeps the source and tries again. Busy, a full disk and
  I/O errors are retried and never reach a caller; a job's own error drops that job alone (a job that keeps failing
  alone with an I/O error is dropped after 8 tries). Checkpoints are PASSIVE, on their own thread, once the WAL holds
  1,000 pages. Rules in `crates/archive/README.md`, "Writers".
- A `call_within` past its wait returns `live::Busy` ("the archive is busy"): not a failure; the job commits later
  and `Busy::settled` turns true once the writer has run it.
- The writer names what holds it (2 Nov 15:51 it stood still 110 s, 1 Nov 00:15 for 133 s, and no line said why).
  Every step (a write's preparation, a live write, a commit, taking the lock, a background job) carries the code
  that handed it over (`#[track_caller]`). A step of 2 s or more logs `Archive writer held 12.3 s by a background
  job from crates/archive/src/compact.rs:181`; one still running at 10 s logs `Archive writer busy … so far` once;
  the `caught up` line adds the longest step. `Queue::holder` gives the step running now.
- The WAL is capped at 64 MB only when a checkpoint copies every page and the next write restarts it; a reader
  holding an old snapshot stops that, and one large transaction fills it at once (991 MB on 2 Nov 15:55). Past the
  cap the checkpointer logs its size and whether the checkpoint was cut short by a reader, once per doubling.
- The restart needs a write that begins after a checkpoint copied every frame, with no commit in between. The
  checkpointer's full-sync checkpoint outlasts the writer's 200 ms groups, so a commit nearly always lands during it
  (3-4 Nov: the WAL grew to 4.2 GB, "every page copied" 1,094 times in a day). Past the cap, once the checkpointer has
  copied every frame it saw, the writer runs one small PASSIVE checkpoint itself between two groups (at most every
  5 s) and its next write restarts the WAL. A checkpoint of 2 s or more is logged (`Archive checkpoint took …`): on
  4 Nov 03:36 and 04:19 the writer stood 5 s in a page read while the checkpointer was in its final truncate.
- Readers (export, views, clips, MCP frames) hold the shared lifetime lock, so nothing they see disappears; a
  reader waits at most one collection batch, never for compaction.

## Schema

`crates/archive/src/store.rs` holds the schema.

- `sessions(id,name,started_wall_ns,complete_wall_ns,migrated_wall_ns)`; `segments(id,session,name,version,
  wall_unix_ns,host_origin_ns,codec)`, one capture timeline each.
- `sources`, `gaps`, `blocks(id,segment,source_id,host_ns,rate,channels,frames,codec,discontinuity,object_id,
  offset_frames,gain,wall_ns)`: native geometry stays in the original columns; `offset_frames` counts native frames.
  Decoded rate and channels belong to the referenced object. Rows never carry PCM.
- `audio_objects(id,rate,channels,frames,codec,pack,stored_sha256)`, blob `a/<id>`. `f32-zstd-planar-v1` reversibly
  separates channels and the four float-byte planes before zstd; old `f32-zstd` stays readable.
- `audio_spans` (cold signal matching), `evidence(id,session,kind,source,utterance_ids,host_ns,wall_ns,payload_id)`,
  `payloads`, `packed_payloads`, `vectors`, `images(…,pack,stored_sha256,…)` (blob `i/<id>.<ext>`), `audio_activity`,
  `utterances`.
- `cold_chunks(id,kind,scope,first_id,last_id,pack,raw_bytes,sha256)` (blob `c/<sha256>`), `cold_sources`,
  `packed_audio_refs`, `logs` (blob `l/<sha256>`, zstd 9), synthesized `clips`, `gc_blobs`, `migrations`.
- Video: `video_segments` (per-display one-minute codec epochs, geometry, host interval, wall anchor, HEVC
  configuration, fps, tier, closed, owner PID), `video_samples` (host PTS and end, keyframe, SHA-256, pack),
  `video_gaps`. Samples are `v/<sha256>` blobs under the same protocol.
- Search projection: `timeline_chunks`, `chunk_tags`, `chunk_links`, `chunk_embeddings` (`docs/chunk-index.md`).
  These are separate from HAC3 `cold_chunks`.

Packing: old rows of one segment (timeline) or one session (evidence) go into `cold_chunks`, dictionary-zstd19 HAC3
table streams with column names, raw length and SHA-256, removed from the live tables in the publishing transaction.
A chunk packs the longest run of up to 8192 rows whose new bodies fit 16 MiB (decode bound 128 MiB). A body that moved
into a chunk keeps a `packed_payloads` row, so a later session repeating it reads it from there. Ad-hoc SQL against
`heimdall.sqlite` sees live rows only; `View::session` gives the old per-session schema (`blocks`, `sources`, `gaps`,
`evidence`, `payloads`, `utterances`) as temporary tables, live and packed.

The frozen zstd dictionary is `records::DICTIONARY`, version 1; it must stay to decode old data. A new dictionary
needs new codec and version IDs.

Clocks: `host_ns` is capture's monotonic host clock, not Unix time. Each block also records its wall time. Events
carry source, utterance IDs, host ns and Unix wall ns. Tier eligibility uses a block's end, not mtime.

## What is journalled

Listening off is an explicit gap, not missing data: each open source gets a `gap` evidence record (reason
`heimdall off`) and the session gets `heimdall_off` (payload `reason`: `setting` or `launched off`), later
`heimdall_on`.

| Evidence | Durable path |
|---|---|
| Every ASR decode, partial and final | Raw recognizer result before reconciliation, `is_final`, source-generation `revision`, source ID and input host interval; final aligned utterance in the transcript index |
| VAD and SmartTurn inputs and probabilities | Listen hook before UI coalescing; speech start/end and source generation |
| Every emitted partial and rolled segment | `listen_event` journal with text, timed words and consensus hints |
| d1, TCL and send-head inputs and outputs | Decide archive sink: typed input with source/time/audio hash and transform; embeddings in vectors; raw distributions |
| Speaker probabilities, segments, embeddings | Native revisions as lossless changed-frame deltas; each six-second embedding with its source spans; `speaker_segments_finalized` |
| Names and labels | Naming and confirmation requests and label changes; final index projection updated |
| Command candidates, chain answers, actions | `command_decision` schema 1 and `command_action` (approval apart from execution); see `docs/decision-chain.md` |
| Recorded versus supplied agent context | `agent_context_routing` before any harness; provider transport records prove delivery |
| Screen commands and their captures | Action, command/capture host clocks and image IDs; temporary PNGs removed only after journalling |
| Agent wakes, sends, replies | Common envelope plus actual Pi RPC JSON, recorded before transport; raw provider events before normalization |
| Agent transport lifecycle | `agent_transport_send` (attempt), `agent_transport_sent` (completed write), `agent_transport_cancelled` (bytes and uncertainty); a partial envelope becomes `needs_reconciliation` and is never retried automatically |
| Speak-back and every TTS component | Requested text, language, state; every component output and denoising step as lossless CAS vector chunks |
| Every synthesized clip | Store `clips` table: lossless `f32-zstd-planar-v1` audio object plus text, voice, generator and settings; never deleted |
| Barge-in decisions | `barge_in`: onset, each checked window with its scores against the user's voice and Heimdall's voices, the verdict, outcome and timings (schema 2, 29 Oct; schema 1 records also hold a side transcript and the decider's output); see `docs/decision-chain.md` |
| Suggest decisions | `suggest_decision`: Decider V5's P(yes) for a line, the bar, and whether a suggester pass started; see `docs/suggester.md` |
| End-of-turn replies | `voice_reply` and `voice_reply_playback` |
| Capture lifecycle | `gap`, `source_removed` (with reason), `source_readded` (a device back within 5 s), `obsolete_worker_event`, `obsolete_capture_event` |

- ASR partials are kept even when the source emits only finals or the UI coalesces one away; the final projection
  never overwrites raw decodes. Bind a revision by source, generation and input clocks.
- Audio inputs are source + host interval references with transforms; d1/TCL keep the input PCM SHA-256. No decision
  WAV copies. Images in envelopes are CAS references; restore original bytes and base64 to rebuild a hot wire
  payload. Cold images keep pixels, not the original PNG encoding: use the MIME from the export index.
- `listen:<source>:<utterance-start-ns>` names an utterance before its app ID exists; `utterance_binding` relates it
  to the final `session:id`.
- A final whose waveform is missing or evicted is kept as `decision.status: unavailable_audio` with the error and
  `given_to_agent: false`; it stays searchable and no unrelated PCM stands in.
- The live diarizer keeps at most 60 s of probabilities and cuts a stable prefix down to a 30 s tail; raw revisions
  are archived before the frontier advances. An 8192-segment pending limit fails loudly if the consumer stalls.
- Commit latency: capture about 1 s after arrival; listen evidence a writer group later (milliseconds), up to 65,536
  queued notes behind a held lock, then counted as dropped (`diagnostic_drops`). A hard power loss loses those tails. A clean finish drains workers, the provider
  bridge and evidence before marking the session complete.
- Permission denial, overflows, unsupported capture geometry and unavailable models are kept as gaps or errors,
  never invented outputs.
- The dry-run agent log (`<data>/run/<session>/dry-run.jsonl`) rotates at 1 MiB into a store log
  `dry-run-<sha256>.jsonl` (zstd 9, independently decoded, month pack and index committed before truncation).

## Compaction (cold tier)

One background thread at background QoS (`compact.rs`) prepares (reads, encodes, verifies, writes the cold blob);
every reference move is one bounded job on the store's writer, at user-initiated QoS.

- Gate: runs while the Mac is idle on AC power (no input for 5 minutes); otherwise it waits unless free disk is within
  2 GiB of the free-space floor (capture stops at the floor). Unknown power or idle counts as no. Asked before each
  pass, at most once a second inside a pass, every 30 s while closed.
- A pass migrates at most one legacy folder, drains the GC queue (hourly orphan sweep), then publishes up to 32 audio
  groups (oldest first, store-wide) and 16 timeline plus 16 evidence chunks, continuing after 250 ms while work
  remains, scanning every five minutes when idle. Then video tiering (`docs/screen.md`) and timeline catch-up
  (`docs/chunk-index.md`).
- A segment whose group fails is skipped for the rest of the pass and reported, so it cannot hold up other audio.
  Failures keep originals readable and retryable; repeated reports are bounded to once a minute.
- Audio groups hold at most 10 s (about 1,000 capture rows, one writer job), split at the speech/non-speech
  boundary; a row that changed while its group was encoded fails the group. New chunks hold at most 1,024 rows
  (decode accepts 8,192); payloads recompress 64 per job; collection removes 256 blobs per job; reclamation frees
  512 pages per job.
- Codecs: speech and missing or incomplete VAD coverage → FLAC mono 16 kHz 16-bit. Runs fully covered by VAD
  probability < 0.1 → Opus AUDIO mode, mono 16 kHz, 12 kb/s VBR, 20 ms packets, complexity 10, with a 250 ms speech
  guard at both ends. No DTX and no removal. Exact +0 silence is duration markers, bounded to one hour so a silent
  source still ages out. Nonfinite or sub-output-sample inputs stay native float/zstd19; finite values outside
  [−1, 1] stay mono 16 kHz float/zstd19 without clipping.
- Why FLAC for speech: on 11 Oct every lossy setting tried (Opus 24–64 kb/s AUDIO and VOIP, AAC 24/48 kb/s) changed
  Parakeet words on public clips or moved speaker-embedding cosine beyond repeat noise; FLAC PCM16 kept recognition
  with minimum cosine 0.999993 (15 clips, 259 words;
  a lab script reproduces it). A lower speech bitrate needs a larger qualified corpus.
- Screenshots go cold as pixel-exact WebP only when smaller than the original.
- Publication: the new object commits to its month pack, is read back and independently decoded, then one index
  transaction checks the blob, commits all references and drops catalogue rows nothing references, queueing their
  blobs. Originals go only after no block, packed reference or synthesized clip references them.
- Readers never wait for compaction: a pass holds `.compact.owner`, which no reader takes; collection takes the
  lifetime lock exclusively without waiting and skips while a reader holds it.
- Quit preempts: `compact::preempt_all` interrupts the preparation's running statement through a progress handler; a
  writer job in flight is one bounded transaction and finishes or rolls back. A step interrupted that way (SQLite
  code 9) logs `archive compaction stopped for Quit…` and sets no status; any other error of that pass keeps its text
  and sets "Archive compaction delayed".
- Measured (`tests/hardening.rs`, throttle model): the longest write-lock hold under compaction plus every live writer
  is about 0.2 s on an 11.7 GB index, against 1.2 s (and 12.9 s on a small store) on main fc403503; installed fc403503
  logged "Archive write lock held 20.3 s by heimdall-cold-archive" on 1 Nov 02:04.

## Deduplication

- Native hot objects hash rate + channels + float bits; simultaneous paths with the same content share one blob.
- Cold groups are fingerprinted after mono 16 kHz normalization. Equal host intervals, matching fingerprints and
  correlation above 0.99999 with a bounded residual allow one shared signal with per-reference gain, preferring direct
  process capture, then higher native rate. Distinct words, timing, phase, echoes or room colour are never merged;
  this is not echo cancellation.

## Storage budget

Derived from measured containers (13 Oct, busy M5 Max), not a captured day. Three sources × 10 h/day (one 48 kHz mono
mic, two 48 kHz stereo app streams): hot about 20 GB/day (the one-day rolling window); cold 0.75 GB/day typical, 1.9
GB/day all-speech; about one year of typical use in 300 GB. FLAC speech dominates. Reproduce with
`heimdall-archive-size` (`NEW_DIR WAV...`, `--non-speech`, `--outputs-only`, `--without-partials`).
Compaction CPU:
225 CPU-seconds per source-hour of audio (`heimdall-archive bench`, 13 Oct).

## Migration from per-launch folders

`heimdall-archive migrate ARCHIVE_ROOT [--keep]` moves every legacy `session-*/` folder and the legacy clip folder into the
store, one folder at a time, under the exclusive lifetime lock. The running app's compactor migrates any it finds,
one per pass. The pre-store app must not run: a folder without `COMPLETE` written in the last two minutes is refused.

1. Read every legacy container (sealed `.sqlite.zst` decoded to a private temp file, HAC1/HAC2 chunks, WAL, schema-1
   inline PCM, `quick_check`).
2. Write it into the store: one segment per capture database; PCM becomes hot objects, cold files and screenshots
   month-pack blobs; rows already cold and older than a day go straight into HAC3 chunks; logs, spool and unknown
   files become `logs` entries. Pending legacy GC is honoured.
3. Verify through the session view: per-table counts and an order-independent SHA-256 digest over every row must
   equal the legacy side's; every audio object and screenshot is read back and matched.
4. Record counts, bytes and digest in `migrations`, then (unless `--keep`) delete the legacy folder.

A failed folder stays untouched, its partial rows are removed, and the run continues and exits non-zero. Reruns are
idempotent. The compactor retries a failed folder after an hour, and migrates a folder only with its size plus 10 GiB
free. A legacy lines list with different bytes is a conflict: both lists and the folder stay.

## Export and read-back

- `heimdall-export ARCHIVE_ROOT NEW_DATASET_DIR` (build `CARGO_TARGET_DIR=target/archive cargo build --release -p
  heimdall-archive --bins`) refuses existing destinations and destinations inside the archive. No capture, model,
  provider or network.
- Read one session with `View::session`, `Journal::open_read_only` or capture's `Archive::open_read_only`, each given
  `<archive>/<session-name>`. `export::snapshot_archive` copies the whole store (SQLite online backup) for offline
  export.
- Dataset: `manifest.jsonl` (final utterances), `evidence.jsonl`, `vectors.jsonl`, `sources.jsonl`, `blocks.jsonl`,
  `audio.jsonl`, `images.jsonl`, `images/`, `audio/` (float32 WAVs, once per unique PCM object), `dataset.json`.
- Manifest audio references are clipped to each utterance with `offset_samples`, `samples`, `rate`, `channels`,
  `gain`, `tier` and native geometry. A loader slices `offset_samples .. offset_samples + samples`, multiplies by
  `gain`, and aligns word times to the utterance's `start_ns`. Hot WAVs can be stereo. Missing audio is explicit
  (`audio_missing`, `audio_gaps`, `audio_complete`); export never fills gaps or labels unknown speakers as the user.

## Verification

`cargo nextest run -p heimdall-archive` (isolate with `CARGO_TARGET_DIR=target/review-archive`) covers hot bit
patterns, zero markers, gain dedup, 24 h eligibility, FLAC/Opus alignment, chunk read-back across sessions, blob GC,
six publication failpoints and two migration failpoints with reopen and read-back, migration of the pre-store
fixture (`tests/fixtures/legacy-archive`), aligned export, writer contention, open-without-write, and compaction
beside a live capture owner. No live permissions are requested.
