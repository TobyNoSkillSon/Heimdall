# Archive clips

Beta: clip export has had little live use; check a clip before relying on it.

The `archive_clip` tool brings back movie export, dropped on 14 October.

One time window of the archive becomes one movie file:

- **Video**: the screen recording of one display (default: the display with the most recorded frames in the window),
  each archived frame at its own capture time, re-encoded with the hardware H.264 encoder (HEVC on request).
  Where nothing was recorded the picture is black; a frame is never held across a hole.
- **Audio**: every archived source with sound in the window on its own AAC track, named after the source
  (`Mic: Shure MV7i`, `Call: Discord Helper`, `App: ego lite (ego Helper)`, `Heimdall`), in the order mic, calls,
  apps, Heimdall. A source that was exact digital silence for the whole window gets no track and is listed as
  left out. All tracks are enabled, so QuickTime plays them together; editors see them separately.
- **Transcript**: speaker-labelled subtitles ("You: …", "Guest: …", the source when nobody is named), short lines cut
  at word times, overlapping speakers stacked. They go in as a soft subtitle track (3GPP timed text) and as
  `FILE.srt` beside the movie.

Native only: AVAssetWriter, VideoToolbox and CoreMedia (`crates/archive/src/clip_writer.rs`). No ffmpeg, no Python.

## Calling it

- **Heimdall's agent**: MCP tool `archive_clip` with `start`, `end` (RFC 3339 with an offset, at most two hours
  apart), optional `display_id`, `width`, `fps` and `path` (absolute `.mp4`/`.mov`). Without `path` the movie goes
  to `~/Library/Application Support/Heimdall/clips/heimdall-YYYY-MM-DD-HHMM-HHMM.mp4`. The answer is plain lines:
  the file, then what each track holds. A build still running after 55 s keeps going; the answer says where the file
  will appear.
- **T3 tasks and sub-agents**: the same library as a command at a stable path inside the installed app:

  ```sh
  ~/Applications/Heimdall.app/Contents/MacOS/heimdall-clip --from "2026-10-24 00:40" --to "00:47" --out ~/clip.mp4
  ```

  Times are local: `"2026-10-24 00:40"`, `"00:47"` (on the `--from` day; past midnight rolls to the next day),
  RFC 3339, `now`, `-10m`, or `+90s` after `--from`. Options: `--display N`, `--width PX`, `--fps N` (constant rate
  instead of capture times), `--hevc`, `--no-subtitles`, `--include-silent`, `--force` (replace an existing file),
  `--json` (the report as JSON), `--archive DIR` (another store). It refuses an existing file without `--force`.
- Both refuse while Seeing is off ("no screen frames are available"), as `screen_frames` does, and a build stops when
  Seeing goes off: the command reads the switch from `settings.json` beside the archive every ten seconds of clip.

## Safety beside the running app

The build only reads. The index is opened `SQLITE_OPEN_READ_ONLY` with `query_only`; the whole plan (sessions,
frames, callbacks, cold chunks, transcript) is read in one read transaction, so a row the compactor moves into a cold
chunk meanwhile is seen exactly once. Content packs are opened read-only. The shared lifetime lock (a `flock` on the
store's existing `.compact.lock`) is held for the whole build, so nothing the plan references can be collected or
re-tiered; the compactor skips its passes until the build ends. A live writer holding the index write lock does not
block the build (WAL). Nothing is written to the store. The movie is written to a hidden partial file beside the
destination and renamed into place only when complete; a `.NAME.building` claim (holding the builder's pid) keeps
two builds off one file, across processes. Inside the app, `archive_clip` runs the bundled `heimdall-clip` as a
child process, so an AVFoundation failure or a long window's memory stays out of Heimdall; Agent off kills it, at
most two run at once, and a development build without the helper builds in process. A damaged frame or audio object
costs its own stretch (black for the rest of that one-minute epoch, silence for that second) and the answer says so.

## Time and sync

Video samples, audio callbacks and transcript words all carry capture's host clock within one app launch. Each item
is placed at `offset + (host − host_from)` of its stretch, so sound and picture are in sync by construction. The host
clock stops while the Mac sleeps, so wall − host jumps at each wake: the anchors that read both together (capture
segments, precisely; video epochs, one a minute, to about 0.3 s) split a launch into stretches of unchanged offset,
and the window is laid out by wall clock across stretches and across launches (a crashed launch ends where the next
starts). Time asleep or between launches is black and silent.

Audio callbacks are joined sample for sample while their timestamps agree within 4 ms, so host-clock jitter never
clicks; a larger difference (a gap, device-clock drift) re-anchors the callback at its own timestamp. The report
gives each track's largest placement offset. The output keeps each source's capture rate (48 kHz when AAC does not
take it) and up to two channels; other rates (cold 16 kHz audio) are linearly interpolated, with each callback's
length rounded on its own timeline so consecutive callbacks abut exactly.

## What the archive actually keeps

- **Screen**: every connected display, half native size (1728 × 1116 on the built-in panel), HEVC, 5 fps, for about
  24 hours; then each one-minute epoch is re-encoded to at most 1 fps, kept indefinitely. Idle screens extend the last
  frame's coverage instead of storing new frames. Seeing off, lock, sleep and errors are holes.
- **Audio**: raw audio is kept, every source separately (each microphone, each app's process tap, Heimdall's own
  voice), as the native float stream, losslessly, for about 24 hours. Then speech becomes FLAC 16 kHz mono and the
  rest Opus 12 kb/s 16 kHz mono; nothing is deleted. Exact digital silence is stored as duration markers.
- **Microphone while Heimdall talks**: the raw microphone, also while echo cancellation runs (the cleaned copy goes
  only to listen; `docs/capture.md`, "Echo guard").
- **Transcript**: final utterances with word times, speaker and source; the clip uses the final projection.

## Measured

A 7:00 window of the live archive (24 Oct, M5 Max under normal load, build before the review fixes): 8.3 s wall,
6.2 s CPU, 441 MB peak memory, a 134 MB H.264 1728 × 1116 movie. Read back by a lab check: all
1,927 frame times within 0.006 ms of the archive's capture times, each AAC track at 0.00 ms lag and correlation
0.997–1.000 against the placed PCM. The live rerun on the final build is still to do. The screen recording itself had
seven unrecorded holes of 1–4.4 s in that window (fullscreen video; `crates/screen/src/gaps.rs` now logs them).

## Tests

`cargo nextest run -p heimdall-archive -E 'test(clip)'`: the synthetic-store integration test (`tests/clip.rs`:
hot and cold-packed audio, a silent source, HEVC epochs with a hole, a transcript line, a writer holding the index
lock throughout, constant-fps resampling, cancellation leaving nothing behind, a damaged frame and audio object), the
writer tests (both containers, 20 s of subtitles beside audio and video) and unit tests for cues, labels, times and
the midnight roll, launches and sleeps, callback spans at mixed rates, and scaling. `crates/agent/tests/mcp_v2.rs` covers the tool's schema and validation; the app test
covers the Seeing-off refusal.
