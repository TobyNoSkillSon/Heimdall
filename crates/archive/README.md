# heimdall-archive — the archive store

One store (SQLite index + content-addressed packs) for every capture object, model output, transcript and screen
frame: cold compaction, export, migration from the per-launch layout, the timeline chunk index and its search, clips
and snapshots. Behaviour and formats: `docs/archive.md`, `docs/chunk-index.md`, `docs/archive-clip.md`.

**Code map:**
- `store.rs` — paths, schema, `open` / `read_index` / pack connections (`tune`: opening writes nothing), `BUSY`.
- `live.rs` — the store's writer: every index write of the process (`Queue::write`, `enqueue`, `call`, `background`, thread `heimdall-archive-live`).
- `records.rs` — `Journal` (evidence writer, `append`, `note`), `DICTIONARY`.
- `compact.rs` — `Compactor`, passes, gate, `preempt_all`, the locks (`reader_lock` shared, the exclusive one only while collection deletes blobs, `.compact.owner` for the compactor).
- `chunks.rs` — HAC3 `cold_chunks` packing. `audio.rs`, `opus.rs` — hot objects, FLAC/Opus.
- `video.rs`, `video_native.rs` — recording writer, decoder, tiering (VideoToolbox).
- `timeline.rs`, `screen_timeline.rs` — the chunk index and `timeline::search`; `event_timeline.rs` — the read-only
  decoded event timeline for agents; `embedding.rs`
  plus `embedding/` — the MLX embedding worker client and its pinned runtime.
- `clip.rs`, `clip_writer.rs` — a window as a movie (`heimdall-clip`, MCP `archive_clip`).
- `snapshot.rs` — `View::session`, one session as the old per-session TEMP tables. `export.rs`, `export_join.rs` —
  dataset export and `snapshot_archive`. `migrate.rs`, `legacy.rs` — the per-launch layout. `diagnostics.rs` — optional
  evidence-boundary hooks.
- Bins: `heimdall-archive` (`migrate`, `bench`, `chunk-backfill`, `chunk-search`, `chunk-live-cost`, …),
  `heimdall-export`, `heimdall-clip`, `heimdall-archive-size`.

No workspace dependencies; `capture`, `screen`, `night`, `voicebank` and `app` write through it.

**Writers:**
- One rule: every index write in a process goes through the store's writer (`live::Queue::shared`, thread
  `heimdall-archive-live`). `Journal`, `records::Writer`, `video::Writer`, capture's `Archive`, the voice bank and the
  store helpers (`ensure_session`, `create_segment`, `mark_complete`, `archive_log`, `audio::publish`) hand their writes
  to it. `store::ungated_writes` counts index commits made anywhere else; `tests/one_writer.rs` keeps it at zero.
- `write` waits for its commit at most `live::COMMIT_WAIT` (1 s) while the writer keeps up; `enqueue` never waits;
  `call` returns a value (an id) or confirms a commit the caller depends on; `call_within` bounds that wait;
  `background` runs one bounded transaction when no live job waits (compaction publish steps).
- A job runs inside the writer's transaction (a savepoint of a group) and may run again after a held lock or a full
  disk rolled the group back: it must not consume what it captured. Never hand a write over from inside a job, and
  never call `call` while your own connection holds an open transaction.
- Blobs go into their pack before the job (`Prepare`, or on the caller); the job re-checks them with `ensure_blob`
  under the index lock.
- Background work prepares on its own thread and keeps each writer job small (rows, not seconds): chunks of
  `chunks::PACK_ROWS`, audio groups of `compact::GROUP_NS`, `store::COLLECT_KEYS`, `store::RECLAIM_PAGES`.
- Exceptions, both idle-only maintenance on their own connection in short transactions at raised QoS: the timeline
  backfill and legacy migration.
- Opening the index or a pack must stay write-free; a PRAGMA that writes on every open freezes the live loop
  (`tests/open_no_write.rs`).

**Testing:** `cargo nextest run -p heimdall-archive`; a dozen tests take 0.5–3.5 s (hardware HEVC round trips,
compactor gating, writer contention). `tests/hardening.rs` runs every writer under compaction with throttled
background I/O (`tests/support/throttle.rs`), another process holding the lock, a long reader, a full disk image and
crashes; its opt-in `realistic_size_with_a_long_reader` builds an 11.7 GB index (about 3 minutes). Ignored lab tests measure recorded-hour CPU and rows and crash a writer child.
