# heimdall-eval — offline real-audio evaluation

**Domain: evaluation (tooling, not shipped).** An agent-operated binary that replays the user's past push-to-talk
recordings through the production listen engine, and benchmarks spoken screen phrases. **Testing:**
`cargo nextest run -p heimdall-eval` (instant, no model runs).

It never sends messages, opens an agent session, captures the screen, requests TCC permission or calls a cloud
service. All output (references, receipts, reconstructed audio, decision archives) goes under `lab/eval/data/`, which
is ignored; never commit corpus data, logs or transcripts. `--out` must stay under `lab/eval/data/` (symlink escapes
are rejected before any directory is created).

## Source data gotchas

The Vella store is opened read-only. A session directory is one push-to-talk recording:

- `000000.pcm` … are Float32 little-endian mono 16 kHz chunks. Concatenate them in metadata order, dropping each
  `overlapFrames` prefix. Duration is sample count / 16000.
- `request.wav` holds only the latest transcription request; it is **not** the recording.
- `session.json.created` uses Apple's 2001 epoch, not Unix time. Transcript mtime can be rewritten by recovery, so it
  is recorded but never trusted as the send time.
- The reference is `transcript.txt`: **Vella model output, not human ground truth**.

`lab/eval/build_corpus.py` matches recordings to T3 messages (from `vault-history query` and both T3 mirrors read
with SQLite `mode=ro`, agent/server messages excluded) within ten minutes after the recording, by ordered word overlap
and local alignment. `match_confidence` is a lexical score, not a probability. One message can legitimately match
several recordings (consecutive dictations plus pasted context).

Labels: a user-stopped recording's end is a true turn end. Silero internal silences ≥ 300 ms are pause
opportunities; pauses under 1 s are hard not-done examples, ≥ 2.5 s of continuous quiet is an acceptable send, the
rest are uncertain. Matched messages are send positives and their image metadata gives screenshot labels (an image
was attached, not necessarily asked for by speech). Unmatched messages are unknown, not negatives; addressedness has no
negative corpus yet, so never invent one.

## Run sequence

Use `CARGO_TARGET_DIR=target/tune` for every Cargo command.

1. `python3 lab/eval/build_corpus.py`, then `cargo build --release -p heimdall-eval`.
2. `target/tune/release/heimdall-eval --labels-only --manifest lab/eval/data/manifest.jsonl --out lab/eval/data/vad-labels`.
3. `python3 lab/eval/merge_labels.py`. If the store grew, label only the generated `missing-vad-labels.jsonl`, append
   its labels to the original label file, and merge again.
4. `target/tune/release/heimdall-eval` replays the deterministic stratified 300 (`lab/eval/data/sample-300.jsonl` to
   `lab/eval/data/replay-300`); `--manifest`, `--models`, `--out` choose paths; `--realtime` paces 20 ms arrivals.
   More than 300 files needs `--allow-full-corpus` and explicit authority.
5. `python3 lab/eval/score_receipts.py` re-scores WER and endpoints from the receipts without new inference.

Strata: duration < 10 / < 30 / < 90 / ≥ 90 s, a Polish-character hint, match status and image presence, round-robin
with SHA-256 seed 11. Rare labels are oversampled on purpose, so raw aggregates are not corpus estimates.

Replay: the public listen APIs with the default engine, 20 ms arrivals and 3 s of added terminal silence. A premature
cut is a final emitted inside a labelled internal pause. Added delay is emission sample-clock minus recording end,
floored at zero. WER is word-level Levenshtein over reference words, ignoring case and punctuation; empty references
are excluded and counted. Outputs: `recordings.jsonl`, `dry-run.jsonl`, `metrics.json` (rewritten after each
recording), `decision-archive/`.

Decisions: each final goes through `decide::LocalDecider` (SendNow and FixedCommand questions) with in-recording
context only and the last 30 s of audio; the screen `PhraseMatcher` logs phrase hits without taking frames. This path
predates the 16 Oct rolling-text d1 dispatch (commit 37b3f7a7 removed TCL and the separate send head):
`Decisions::quiet_send` returns nothing, so its send metrics do not describe the installed decider. `--collect`,
`--live-cost`, `lab/eval/tune_split.py` and `lab/eval/score_tuning.py` belong to that earlier tuning round.

## Screen-phrase benchmark

`heimdall-eval screen-phrase-bench` checks that a spoken screen phrase selects the frame on screen when the phrase
began. Synthetic TTS utterances (`lab/screen-phrase-bench/gen_tts.py`, `inventory.json`) are spliced at exact
onsets, mixed with noise, run through the app's listen engine and the production `PhraseMatcher`, and resolved with
`ScreenHolds::pin_now` + `screen::attach_held` against a synthetic 5 fps screen recorded through
`ScreenBuffer::recording` (hardware HEVC, an isolated store under `--out`, deleted unless `--keep-archive`).
Ground-truth math: `src/screen_bench.rs`. Shards: `lab/screen-phrase-bench/run.sh`, aggregated by `report.py`.
Results and method: `lab/screen-phrase-bench/REPORT.md`.
