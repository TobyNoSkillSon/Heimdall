# Timeline chunk index

How the archive's search index cuts the shared capture clock into searchable chunks, and how agents search it.
Code: `crates/archive/src/timeline.rs`, `screen_timeline.rs`, `embedding.rs`. The MCP tool's arguments and output
(`timeline_search`): `docs/mcp.md`.

The model-free timeline lives in `heimdall.sqlite`, alongside the archive catalogues. It never copies
waveforms, images or video. These semantic chunks are separate from `cold_chunks`, the HAC3 storage
containers. EmbeddingGemma 2 runs separately in an offline, resident BF16 MLX helper; capture and
timeline projection never run inference.

## Schema and clock

`timeline_chunks` holds `id`, `session`, `kind`, `role`, `source`, capture `start_ns/end_ns`, Unix
`start_wall_ns/end_wall_ns`, verbatim `text`, structured `details`, and `content_hash`. IDs are deterministic
within the archive session. Capture segments provide wall/host anchors; legacy finals without an anchor use
the recorded final arrival wall time minus the source interval's duration. A host timestamp alone never
identifies time across sessions/reboots.

`chunk_tags(chunk_id,key,value)` has exact, indexed, multiple-valued tags: `speaker`, `source`, `app`
(the owning bundle, ahead of a helper bundle), `language`, `command_kind`, `agent`, `listening`, and `window_title`.
Speaker names come from the existing naming pipeline. Anonymous tracks use `unknown speaker N`; without a
track the tag is `unknown speaker`. Language comes from the pipeline's Observation, not a new detector;
old rows without that evidence say `unknown`. Agent/Listening tags say `unknown` before their first recorded
state. A span crossing a recorded state change carries both exact values.

`chunk_links(chunk_id,relation,target)` identifies utterance IDs, stable evidence IDs, overlapping chunks,
representative image IDs, video segments and source/time audio references. A `source_span` is JSON with
`session`, `source`, `start_ns`, `end_ns`; `timeline::audio_refs` resolves it against live and packed callback
rows at read time. The result preserves original native geometry, object ID, offset, gain, clipping interval,
and gaps. The audio decoder remains `audio::read_block`; no embedding input silently fills a capture gap.

`chunk_embeddings(chunk_id,model_id,model_version,dimensions,vector_id,content_hash,created_wall_ns)` starts
empty. `vector_id` references the existing lossless float32 CAS `vectors` table. A changed quote, label, span,
context, or representative frame invalidates associated embedding rows; vector CAS bytes remain shared.

`timeline_state`, `timeline_finals`, and `timeline_speakers` retain the small state/boundary/name projections.
`timeline_backfill` checkpoints evidence, final-row and callback IDs per session. `timeline_audio_backfill`
checkpoints individual source/bucket groups, so interrupting an interleaved audio batch cannot skip a source.
Projection/checkpoint updates commit together. Original evidence remains the source of truth.

## Chunk boundaries

- `text/utterance` and `speech_audio/utterance`: one pair per final transcript row, with identical source and
  clock intervals. Timed words supply the quote, excluding the app's bracketed action notes. No partial or
  one-second audio chunk becomes a separate speech chunk.
- `text/turn` and `speech_audio/turn`: the complete final, including the stream's rolled prefixes, plus the
  preceding turn's last sentence. Both modalities include that sentence's source interval and link its chunk.
  The current final's boundary is recorded as `smart_turn`, `forced`, or `legacy_final`; forced closure is
  never represented as a positive Smart Turn decision. Overlap does not cross recorded discontinuities,
  source closure or Listening Off, nor join Heimdall's own voice on a microphone (its speaker echo) with someone
  else's words (bug note 2026-10-24T13-35-27). Late labels/quotes also rebuild the next turn's overlap.
- `text/command`: each `command_decision` evidence record is a small separately linked chunk, preserving
  candidate, chain, merged and abandoned phases rather than pretending every proposal executed.
- `speech_audio/speaker_turn`: each diarization segment, keyed by source, generation origin and segment index.
  Revised boundaries replace that projection; removed provisional tails disappear. Confirmed names apply to
  the same track and survive later segment revisions. Original diarization evidence remains immutable.
- `other_audio/sound`: app sound in 20-second capture-clock buckets, with partial edge buckets on start/stop.
  Exact digital-zero callback blocks create none. These are explicitly unclassified sound, excluding known
  final/diarized speech through `sound_ranges_ns`; a bucket fully covered by speech is omitted by search and
  embedding jobs. Quiet nonzero audio is retained without inventing a silence classifier. Sparse nonzero
  spans in one bucket can bracket digital silence; the source reference preserves the exact callback geometry.
- `moment/window`: fixed 45-second wall-clock windows, linked to the original chunks, quoted text, sources,
  speakers and app tags. Screen chunks add frontmost app/window context when available. No historical window
  title or frontmost app is invented when the archive did not record one.

Live final projection runs inside the transcript transaction (`app::index::Index::record/relabel`). The
journal hook runs commands, state, language evidence and speaker updates inside their existing evidence
transaction. Capture projects source spans once per existing flush, not once per callback, and performs no
inference. Schema creation is additive. Existing transcript search and archive/export formats stay usable.

## Screen recording integration

The recorder's `video::Writer::append_screen` / `extend_screen` publish the recording and its timeline
projection in the same transaction. `timeline::recording` uses the existing RGB frame once per second:
a 32 × 18 colour grid, averaging four point samples per cell. A new state starts when at least 15% of
cells change by an average of 24 RGB levels against the last accepted state's grid, when the frontmost
bundle changes, or when its window ID/title changes. Comparing against the state rather than the previous
frame catches accumulated slow scroll; isolated carets, cursors and small codec noise do not split it.
These are deliberately heuristic visual changes, not semantic page/scroll classification; transitions shorter
than the one-second sampling interval and changes below the area threshold can be missed. No model runs.

Chunks cap at 45 seconds. Idle sample coverage crosses that cap without encoding another image;
codec epoch rotation alone preserves the state. Timestamp jitter up to half the recorded frame period
keeps one state; larger coverage holes or explicit recording gaps split it. Actual frame availability
still resolves through `video_samples`, so the index does not claim extra encoded frames. Stop/removal flushes only committed coverage and closes
it. Explicit capture gaps remain unrepresented. The representative is a `video_frame` link and structured
`{segment,host_ns}` reference to a covered sample, with a `video_segment` link. There are no copied
representative pixels or image rows. Frontmost bundle and window title become exact `app` and
`window_title` tags; the window ID also participates in change detection. The read-only NSWorkspace /
CoreGraphics catalogue uses existing recording permission and never requests AX or TCC access.

State boundaries are retained in `video_screen_events`. Unchanged coverage projects at most once per
five seconds; boundaries and closure flush immediately, using the recorder's existing commit/fsync.
`timeline_video_backfill` checkpoints committed segment coverage. The additive tag migration preserves
existing tags. The legacy image-based `screen_frame` hook remains supported for its callers.

The bounded timeline backfill also consumes one closed video epoch per call, decoding
committed HEVC samples and computing the same signal at one Hz. Codec reference frames advance without
copying RGB. The existing 128 MiB encoded-epoch bound applies; no whole-session video or RGB history is
loaded. Admission is checked before each one-Hz analyzed frame and before the atomic checkpoint/projection
publication. Codec reference frames do not consume the background worker's 256-record admission budget;
at most one second of reference frames advances between checks.
A small `timeline_screen_detector` checkpoint preserves the accepted colour signal across contiguous
legacy epochs. New recordings replay their recorded boundary metadata directly. Historical recordings
without frontmost/window metadata remain untagged for those fields; no app or title is inferred from pixels.

Cost (busy M5 Max, 14 Oct, accelerated replay, 5 fps at 1728 × 1116, per recorded display; CPU, not watts): live
signal + projection about 0.3 CPU s/hour static and 0.39 with a page change every 10 s, adding 1,400 and 4,860 index
rows/hour; backfill of a recorded HEVC epoch about 19 CPU s per recorded hour. The projection adds no fsync of its own.
Gotcha: the background admission budget (256 records) counts one-Hz analyses, not codec reference frames; counting
every reference frame exhausted it before a 300-frame epoch finished.

`crates/archive/tests/screen_chunks.rs` covers PNG and HEVC recording fixtures, title/app/same-title window switches,
scroll, caret and noise rejection, accumulated change, idle caps, gaps, references, codec continuity, interrupted and
resumed backfill, unknown historical metadata, additive tag migration and rollback.

## Migration and agent API

The existing background compactor runs bounded catch-up after its normal pass, under its power gate and
background QoS. Each call reads at most an 8192-ID evidence or capture window, checks admission between
records/source groups, and resumes from committed checkpoints. Irrelevant model payloads are not parsed as
JSON. It reads only overlapping cold HAC3 containers, not a reconstructed whole session on every batch.
The explicit agent CLI is `heimdall-archive chunk-backfill LAB_ARCHIVE_ROOT`; it drains these same bounded
calls and emits CPU/size/count evidence. For real-data measurements, use a consistent private copy, never a
live index copied without its committed WAL. The CLI measurement has a nine-minute wall deadline.

Heimdall's agent searches through MCP `timeline_search` (`docs/mcp.md`). Rust `timeline::search` and the agent CLI
`heimdall-archive chunk-search ARCHIVE_ROOT SEARCH_JSON` take a `Search`: optional `text`, Unix
`start_wall_ns/end_wall_ns`, `session`, `kinds`, exact conjunctive `tags: [{key,value}]`, and `limit` (1–100).
Time means overlapping capture time, not evidence arrival time. `text` is literal case-insensitive matching
with SQLite's built-in case folding (ASCII, not full Unicode linguistic matching). Results include complete
tags, links, clocks, quotes and details. The agent API uses the same epoch permits, query interrupt, progress
hook and response-body gating as existing search, so Agent Off revokes in-flight access.

The MCP tool's `vector: {"query":"…"}` is embedded by the app with the same provider used for indexing. Rust/CLI
callers pass
`vector: {"model_id":"…","model_version":"…","values":[…]}`. Search ranks exact cosine similarity,
with deterministic ID ordering for ties, under the same capture-time/kind/tag/text filters. Results add
top-level `similarity` without changing captured `details`. Only current content hashes and matching
model/version/dimensions participate. Wrong identities, zero/nonfinite vectors and unavailable embeddings
fail explicitly. The scan is linear; it keeps only the requested top-k in memory. No ANN backend exists yet.

## Embedding model integration

`embedding::MlxEmbedder` implements `timeline::Embedder`, using the official pinned checkpoint,
MLX-VLM `3d87e884`, the matching Transformers processor, task-specific query/document prefixes,
mean pooling including the prompt, and final normalization. The stored dimension is 768, float32 CAS.
Model weights/activations are BF16; FP32 pooling/attention accumulation follows the model's arithmetic.

Rule: production is BF16 or FP16 unconditionally. FP32 exists only as the
independent parity reference. Its cosine/top-k comparison is informational, never a release gate or a
reason to select FP32. The BF16/FP16 interleaved measurement found no meaningful aggregate speed winner
on this archive mix, so the source checkpoint's BF16 path is the default. See the optimization log.

Text and command fields remain verbatim/structured evidence. Speech/app sound is decoded from native
callback geometry with gain/offset/clipping and stereo mixing before one continuous-interval resample.
Gaps, discontinuities, segment changes and format changes remain separate parts; labelled speech is
excluded from app-sound input. Screen states use their committed representative frame and title;
moments fuse quotes, their paired/unioned/clipped speech intervals, app sound and representative frames.
The model's video path accepts already-decoded frames; the recording API still owns video decoding.
Historical containers get a bounded source/time geometry catalogue once, so a waveform read decodes only
overlapping containers. New containers publish their geometry atomically.

Long legacy finals retain all text/media. Host PCM parts are bounded to two minutes; if the combined input
exceeds 8,192 tokens, original text offsets and all media are packed into bounded windows. A token-weighted
mean of their normalized vectors is normalized again. Nothing silently truncates the final.

The app's shared provider indexes chunks overlapping the last hour with bounded `embed_live` calls.
Older backlog uses `embed_batch` with the live `BackfillGate`: AC and at least 300 seconds idle;
unknown power/idle fails closed. Replay starts neither job. The helper is lazy and unloads after 120 seconds
without work; no download occurs at runtime. Missing media/runtime failures are recorded with a five-minute
retry keyed by content revision. A bad item cannot starve the rest of the queue.
The gate is checked before each inference and publication. Inference holds the archive reader lifetime lock
but no SQLite write transaction. A closing gate or changed content hash discards the in-flight result;
nonfinite vectors and wrong dimensions fail explicitly. Successful vectors commit atomically with their
model/version/revision record. Query cancellation/Agent Off cancels queued/in-flight helper access.

The sealed bundle includes a standalone Python interpreter and pinned dependencies, not a symlink into
another app or a developer venv. `crates/archive/embedding/assets.lock.json` pins the complete runtime tree
and checkpoint files. The production path accepts only half precision, enforces memory admission/Metal
allocation bounds and rejects swap growth. Full benchmark procedure and measurements are in
a lab log outside this tree.

## Measured cost

On a private copy of the real archive (14 Oct, 11 sessions, 23 recorded hours, sparse speech): 653 index rows and
170 KiB per recorded hour; the one-time migration took 15.8 CPU s per recorded hour (debug build). Live final
projection adds about 0.76 ms per final (`heimdall-archive chunk-live-cost LAB_ARCHIVE_ROOT`). Dense speech or many
apps need their own input rates. Watts unmeasured.

## Verification

`crates/archive/tests/timeline.rs` covers live boundaries, exact tags, capture-time search, literal quotes,
late labels and successor overlap, gap isolation, command phases, diarization revisions/names, sound exclusion,
fake screen frames, cold evidence migration, interrupted interleaved-source catch-up, partial-tail append safety, transaction rollback,
audio links after cold packing, vector unavailability, versioned embedding restart, gate closure and invalid
vectors. App loopback plumbing checks the real MCP search against a final generated by the pipeline.
