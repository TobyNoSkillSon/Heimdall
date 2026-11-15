# heimdall-archive

The archive store: one SQLite index plus content packs for every capture object, model record, transcript line,
screenshot and screen frame, with its writer, cold compaction, export, migration, the timeline index and clips.

Read this when: you change code in `crates/archive`, you add a writer or reader of the store, you look for where a
store behaviour lives.
Code: `crates/archive/src/`. Behaviour: [docs/archive.md](../../docs/archive.md).

## Files

| File | What it holds |
|---|---|
| `src/lib.rs` | Module list, `wall_ns`, `hash`, `full_sync` (`F_FULLFSYNC`), `atomic_write`, log compression, `DAY_NS`. |
| `src/store.rs` | Paths (`INDEX`, `PACKS`, `HOT`), `SCHEMA`, `open`, `read_index`, `tune` (opening writes nothing), pack connections (`put`, `get`, `ensure_blob`), blob keys, `queue_gc`, `collect`, `sweep`, `reclaim`, `archive_log`, `read_log`, `ensure_session`, `create_segment`, `mark_complete`, `ungated_writes`. |
| `src/live.rs` | The store's writer `Queue` (thread `heimdall-archive-live`): `shared`, `write`, `enqueue`, `call`, `call_within`, `background`, `turn`, `holder`, `settle`; the checkpointer thread; WAL and slow-step log lines. |
| `src/readers.rs` | Statement-trace watch of every index connection; names long read snapshots (`LONG_READ`). |
| `src/records.rs` | `Journal` (`append`, `event`, `note`, `image`, `vector`), `records::Writer` (bounded listen evidence writer), `DICTIONARY`, VAD activity (`non_speech_ranges_in`). |
| `src/compact.rs` | `Compactor`, `compact_pass`, `encode_cold` (cold codec policy), deduplication, `preempt_all`, the locks (`reader_lock`, exclusive lock, `.compact.owner`). |
| `src/chunks.rs` | HAC3 `cold_chunks`: packing (`pack_blocks`, `pack_evidence`), decode, packed payload lookups. |
| `src/audio.rs`, `src/opus.rs` | Hot objects (`f32-zstd-planar-v1`), FLAC and Opus encode and decode, `read_block`, `read_object`, resampling. |
| `src/video.rs`, `src/video_native.rs` | Screen recording writer, decoder and tiering (VideoToolbox, HEVC). |
| `src/timeline.rs`, `src/screen_timeline.rs` | The timeline chunk index, backfill and `timeline::search`. |
| `src/embedding.rs`, `src/vector_index.rs`, `src/rerank.rs`, `embedding/` | Embedding worker client and its pinned runtime, the in-memory vector copy, the reranker. |
| `src/event_timeline.rs` | Read-only decoded event timeline for agents (`heimdall-archive timeline`). |
| `src/clip.rs`, `src/clip_writer.rs` | A time window as a movie (`heimdall-clip`, MCP `archive_clip`). |
| `src/snapshot.rs` | `View::session`: one session as TEMP tables of live and packed rows. |
| `src/export.rs`, `src/export_join.rs` | Dataset export. |
| `src/migrate.rs`, `src/legacy.rs` | Migration from per-launch folders and the legacy container reader. |
| `src/diagnostics.rs` | Optional hooks at the evidence boundary (the app's log lines and checks). |

Binaries in `src/bin/`:

- `heimdall-archive`: `migrate`, `compact`, `tier`, `timeline`, `audio-cost`, `bench`, `compact-file`, `pack-file`,
  `codec`, and the index tools `chunk-backfill`, `chunk-search`, `chunk-live-cost`, `embedding-index`,
  `embedding-search`, `embedding-fixtures`, `vector-search-lab`.
- `heimdall-export`, `heimdall-clip`, `heimdall-archive-size`, `heimdall-vocabulary-fixtures`.

Workspace dependency: `heimdall-home` only. `capture`, `screen`, `night`, `voicebank` and `app` write through this
crate.

## Hand a write to the writer

Every index write in a process goes through the store's writer (`live::Queue::shared`). `Journal`, `records::Writer`,
`video::Writer`, capture's `Archive`, voice clips and the store helpers (`ensure_session`, `create_segment`,
`mark_complete`, `archive_log`, `audio::publish`) already do.

1. Commit any blob to its pack before the job (on the caller, or in a `live::Prepare`), then re-check it inside the
   job with `store::ensure_blob`.
2. Pick the call: `write` waits for its commit at most `live::COMMIT_WAIT` (1 s); `enqueue` never waits; `call`
   returns a value (an ID) or confirms a commit the caller depends on; `call_within` bounds that wait and returns
   `live::Busy` past it; `background` runs one bounded transaction when no live job waits.
3. Write the job so it can run again: it runs inside a savepoint of a group and reruns after a held lock or full disk
   rolled the group back, so it must not consume what it captured.
4. Keep background jobs small in rows, not seconds: `chunks::PACK_ROWS`, `compact::GROUP_NS`, `store::COLLECT_KEYS`,
   `store::RECLAIM_PAGES`.
5. Check: `cargo test -p heimdall-archive --test one_writer` (`store::ungated_writes` must stay zero).

Rules:

- Never hand a write over from inside a job, and never call `call` while your own connection holds an open
  transaction: it waits for itself.
- Opening the index or a pack must stay write-free; a PRAGMA that writes on every open freezes the live loop
  (`tests/open_no_write.rs`).
- Exceptions: the timeline backfill and legacy migration write on their own connection in short transactions at raised
  QoS. The backfill writes in the writer's turn (`Queue::turn`; an outermost `timeline::transaction` takes one): the
  writer stands aside while it writes, and the backfill ends a turn once live jobs have waited `LIVE_FIRST` (250 ms).
  Inside a turn, handing a write to the writer is refused. Why: polling the lock instead lost every poll to the
  writer and failed with "database is locked" (`tests/contention.rs`).

## Tests

`cargo test -p heimdall-archive`. The full check runs it under `cargo nextest`; isolate the build with
`CARGO_TARGET_DIR=target/review-archive`. Tests use temp stores and request no permissions; about a dozen take
0.5–3.5 s (hardware HEVC round trips, compactor gating, writer contention).

| Test file | Covers |
|---|---|
| `tests/archive.rs` | Hot bit patterns, zero markers, gain dedup, 24 h eligibility, FLAC/Opus alignment, chunk read-back, blob GC, publication and migration failpoints, migration of `tests/fixtures/legacy-archive`, export. |
| `tests/one_writer.rs` | Every index write goes through the writer. |
| `tests/open_no_write.rs` | Opening the index or a pack writes nothing. |
| `tests/contention.rs`, `tests/live_lock.rs`, `tests/held_lock_backlog.rs` | Writer contention, live writes under a held lock, backlog drain. |
| `tests/hardening.rs` | Every writer under compaction with throttled I/O (`tests/support/throttle.rs`), another process holding the lock, a long reader, a full disk image, crashes. |
| `tests/fd_bound.rs` | Bounded file handles to the index and packs. |
| `tests/timeline.rs`, `tests/screen_chunks.rs`, `tests/vector_search.rs`, `tests/embedding_native.rs` | Timeline index, screen chunks, vector search, the embedding worker (scripted fake). |
| `tests/clip.rs`, `tests/night_export.rs` | Clips over a synthetic store; export with night output. |

Opt-in lab tests run with `-- --ignored`, one at a time: `hardening.rs` `realistic_size_with_a_long_reader` (an
11.5 GB index, release build), `write_volume.rs` (disk writes under VAD notes, about two minutes), `lab_probe.rs`
(lock-hold and write-cost probes; one needs `LAB_ARCHIVE`), `screen_chunks.rs` (HEVC CPU probe), and
`embedding_native.rs` (needs an isolated native model root).

## See also

- [docs/archive.md](../../docs/archive.md): layout, schema, writer, compaction, export, migration
- [docs/capture.md](../../docs/capture.md): how capture writes PCM into the store
- [docs/screen.md](../../docs/screen.md): screen recording and video tiering
- [docs/archive-clip.md](../../docs/archive-clip.md): clips
- [docs/chunk-index.md](../../docs/chunk-index.md): timeline index, search, reranker
