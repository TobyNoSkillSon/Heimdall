# heimdall-eval: offline real-audio evaluation

A development binary, never shipped. It replays recorded push-to-talk dictations through the production listen engine
and scores word error rate and turn ends, and it benchmarks spoken screen phrases.

Read this when: you measure a listen-engine change on real recordings, rebuild the evaluation corpus, or run the
screen-phrase benchmark.
Code: `crates/eval/src/main.rs` (replay), `src/lib.rs` (manifest, recording reader, pauses, WER, output-path guard),
`src/screen_bench.rs` (benchmark ground-truth math), `src/screen_bench_run.rs` (benchmark runner).

## Limits

- It never sends messages, opens an agent session, captures the screen, requests a macOS permission or calls a cloud
  service.
- All output goes under `lab/eval/data/`, which git ignores. `--out` must resolve inside it; `lab/eval/data` itself
  must not be a symlink, and symlink escapes are refused before any directory is created. Never commit corpus data,
  logs or transcripts.
- Replay is capped at 300 recordings; more needs `--allow-full-corpus` and explicit authority.
- The corpus scripts (`lab/eval/*.py`) live only in the development repository.

## Source data

The [Vella](../../docs/vella.md) session store, opened read-only. One session directory is one push-to-talk recording:

- `000000.pcm` … are Float32 little-endian mono 16 kHz chunks. Concatenate them in metadata order, dropping each
  `overlapFrames` prefix. Duration is samples / 16000.
- `request.wav` holds only the latest transcription request; it is not the recording.
- `session.json` `created` uses Apple's 2001 epoch, not Unix time. Transcript mtime can be rewritten by recovery, so
  it is recorded but never trusted as the send time.
- The reference text is `transcript.txt`: Vella's model output, not human ground truth.

`lab/eval/build_corpus.py` matches recordings to sent chat messages (read-only, agent and server messages excluded)
within ten minutes after the recording, by ordered word overlap and local alignment. `match_confidence` is a lexical
score, not a probability. One message can match several recordings.

Labels: a user-stopped recording's end is a true turn end. Silero internal silences of 300 ms or more are pause
opportunities: under 1 s is a hard not-done example, 2.5 s or more of continuous quiet is an acceptable send, the rest
are uncertain. Matched messages are send positives; an attached image gives a screenshot label (attached, not
necessarily asked for). Unmatched messages are unknown, not negatives. There is no negative addressedness corpus; do
not invent one.

## Run a replay

Use `CARGO_TARGET_DIR=target/tune` for every Cargo command.

1. `python3 lab/eval/build_corpus.py`, then `cargo build --release -p heimdall-eval`.
2. `target/tune/release/heimdall-eval --labels-only --manifest lab/eval/data/manifest.jsonl --out lab/eval/data/vad-labels`.
3. `python3 lab/eval/merge_labels.py`. If the store grew, label only the generated `missing-vad-labels.jsonl`, append
   its labels to the original label file, and merge again.
4. `target/tune/release/heimdall-eval` replays the stratified 300 (`lab/eval/data/sample-300.jsonl` to
   `lab/eval/data/replay-300`). `--manifest`, `--models`, `--out` choose paths; `--realtime` paces 20 ms arrivals.
5. `python3 lab/eval/score_receipts.py` re-scores WER and endpoints from the receipts without new inference.
6. Check: `lab/eval/data/replay-300/metrics.json` exists and is rewritten after each recording.

Strata: duration under 10 / 30 / 90 s and 90 s or more, a Polish-character hint, match status and image presence,
round-robin with SHA-256 seed 11. Rare labels are oversampled, so raw aggregates are not corpus estimates.

Replay uses the public listen APIs with the default engine, 20 ms arrivals and 3 s of added terminal silence. A
premature cut is a final emitted inside a labelled internal pause. Added delay is emission sample clock minus
recording end, floored at zero. WER is word-level Levenshtein over reference words, ignoring case and punctuation;
empty references are excluded and counted. Outputs: `recordings.jsonl`, `dry-run.jsonl`, `metrics.json`.

The screen `PhraseMatcher` logs phrase hits on the replayed words without taking frames; the screenshot metric scores
those hits alone. The replay asks the decider nothing; `heimdall-send-replay` in `crates/decide` replays the live send
path.

## Screen-phrase benchmark

`heimdall-eval screen-phrase-bench` checks that a spoken screen phrase selects the frame on screen when the phrase
began. Synthetic TTS utterances (`lab/screen-phrase-bench/gen_tts.py`, `inventory.json`) are spliced at exact onsets,
mixed with noise, run through the listen engine and the production `PhraseMatcher`, and resolved with
`ScreenHolds::pin_now` and `screen::attach_held` against a synthetic 5 fps screen recorded through
`ScreenBuffer::recording` (hardware HEVC, an isolated store under `--out`, deleted unless `--keep-archive`).
Shards: `lab/screen-phrase-bench/run.sh`, aggregated by `report.py`.

## Tests

- `cargo test -p heimdall-eval` (instant, no model runs).

## See also

- [screen.md](../../docs/screen.md): screen phrases and held frames
- [decision-chain.md](../../docs/decision-chain.md): the send decision the replay leaves out
- [vella.md](../../docs/vella.md): the Vella link
