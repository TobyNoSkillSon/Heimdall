# Archive clips

`heimdall-clip` and the `archive_clip` tool turn one time window of the archive into one movie file: the screen of
one display, every audio source on its own track, and the transcript as subtitles.

Read this when: you want footage, audio or the transcript of a past time window as a movie, you are changing how a
clip is planned, placed or encoded, or a clip came out black, silent or out of sync.
Code: `crates/archive/src/clip.rs` (plan, placement, cues, time parsing), `crates/archive/src/clip_writer.rs`
(AVAssetWriter), `crates/archive/src/bin/heimdall-clip.rs` (the command), `crates/app/src/mcp_clip.rs` (the tool).

Native only: AVAssetWriter, VideoToolbox and CoreMedia. No ffmpeg, no Python.

## What the movie holds

- Video: the screen recording of one display, by default the display with the most recorded frames in the window.
  Each archived frame sits at its own capture time, re-encoded with the hardware H.264 encoder (HEVC with `--hevc`).
  Where nothing was recorded the picture is black; a frame is never held across a hole longer than `VIDEO_GAP_NS`
  (250 ms).
- Audio: every archived source with sound in the window gets its own AAC track named after the source (`Mic: <device>`,
  `Call: <app>`, `App: <app>`, `Heimdall`), ordered mic, calls, apps, other, Heimdall (`describe_source`). A call is
  an app whose bundle id is in `CALL_APPS`. A source that was exact digital silence for the whole window gets no
  track and is listed as left out (`--include-silent` keeps it). All tracks are enabled, so QuickTime plays them
  together and editors see them apart.
- Transcript: speaker-labelled subtitles (the speaker's name, else the source), short lines cut at word times
  (`CUE_NS` 5.5 s, `CUE_CHARS` 84), overlapping speakers stacked. They go in as a soft 3GPP timed-text track and as
  `NAME.srt` beside the movie whenever there is speech (`--no-subtitles` drops only the soft track).
- The answer: the file, then what each track holds, black gaps, silent sources and any damaged stretch.

## Make a clip as the agent

Call `archive_clip` ([mcp.md](mcp.md) lists every tool):

- `start`, `end`: RFC 3339 with an offset; `end` after `start`, at most `CLIP_MAX_NS` (two hours) apart.
- `display_id` (numeric string), `width` (64..4096; height keeps the aspect), `fps` (1..60, a constant rate instead
  of capture times).
- `path`: absolute, ending `.mp4`, `.mov` or `.m4v`. Default: `<home>/.heimdall/clips/heimdall-YYYY-MM-DD-HHMM-HHMM.mp4`
  in local time, with `-2`, `-3` … when taken ([home.md](home.md)).

Behaviour:

- The installed app runs the bundled `heimdall-clip` beside its own executable as a child process, so an AVFoundation
  failure or a long window's memory stays out of Heimdall. A development build without the helper builds in process.
- At most `MOST_AT_ONCE` (2) builds run at once (`CLIP_BUSY` beyond). Agent off kills the child.
- A build still running after `ANSWER_WITHIN` (55 s) keeps going; the answer says where the file will appear.
  Why: the MCP connection closes after 75 s.
- Errors: `PATH_EXISTS` (the file, its `.srt` or a build of it exists), `ARCHIVE_UNAVAILABLE`, `CLIP_FAILED`, and
  the Seeing-off refusal below. A path inside the archive is refused.

## Make a clip from a task or a shell

1. Run the command inside the installed app:

   ```sh
   "$HOME/Applications/Heimdall.app/Contents/MacOS/heimdall-clip" --from "2026-10-24 00:40" --to "00:47" --out "$PWD/clip.mp4"
   ```

2. Times are local: `"2026-10-24 00:40"`, `"00:47"`, RFC 3339 with an offset, `now`, or relative `-10m`, `+90s`, `-2h`
   (`s`, `m`, `h`). A clock-only `--from` later than now means yesterday. A clock-only `--to` is on the `--from` day,
   or the next day when that would not be after `--from` (`23:50` to `00:10`). A relative `--to` counts from `--from`.
3. The options: `--display N`, `--width PX`, `--fps N`, `--hevc`, `--no-subtitles`, `--include-silent`, `--force`
   (replace an existing movie and its `.srt`), `--json` (the report as JSON), `--archive DIR` (default
   `<home>/.heimdall/archive`). Without `--out` the file goes to the clips folder as above.
4. Check: the command prints the report and exits 0; the movie and its `.srt` exist at the printed path.

It refuses an existing file without `--force`, a window over two hours, and an output inside the archive.

## Seeing off

Both refuse while Seeing is off with `CAPTURE_UNAVAILABLE: no screen frames are available` (`clip::SEEING_OFF`, the
same words as `screen_frames`). The app checks its own flag; the command reads `screen_off` from `settings.json`
beside the archive, and re-reads it every ten seconds of clip, so a build stops when Seeing goes off
([settings.md](settings.md), [screen.md](screen.md)).

## Safe beside the running app

- The build only reads. The index is opened `SQLITE_OPEN_READ_ONLY`; the whole plan is read in one read transaction,
  so a row the compactor moves into a cold container meanwhile is seen exactly once. Content packs open read-only.
- It holds the archive's shared reader lock (`compact::reader_lock`) for the whole build, so nothing the plan
  references is collected or re-tiered meanwhile ([archive.md](archive.md)). A writer holding the index write lock
  does not block it.
- The movie is written to a hidden `.NAME.partial-…` file beside the destination and renamed when complete. A
  `.NAME.building` claim holding the builder's pid keeps two builds off one file, across processes.
- A damaged frame costs the rest of its video epoch (black, since later frames depend on it); a damaged audio object
  costs its second (silence). The answer says so.

## Time and sync

- Video samples, audio callbacks and transcript words share capture's host clock within one app launch. Each item
  sits at `offset + (host − host_from)` of its stretch, so sound and picture are in sync by construction.
- The host clock stops during sleep. Capture segments (precise) and video epochs (one a minute, about 0.3 s coarse)
  anchor wall against host and split a launch into stretches of unchanged offset; the window is laid out by wall
  clock across stretches and launches. Time asleep or between launches is black and silent.
- Consecutive audio callbacks within `SNAP_NS` (4 ms) are joined sample for sample, so host-clock jitter never clicks;
  a larger difference re-anchors the callback at its own timestamp. The report gives each track's largest placement
  offset.
- Each track keeps its source's most common capture rate when AAC takes it (8 to 48 kHz), else 48 kHz, and up to
  two channels; other rates are linearly interpolated. AAC runs at 192 kb/s mono or 256 kb/s stereo at 44.1 kHz and
  above; below that the encoder picks.

What the archive keeps (rates, tiers, retention) belongs to [archive.md](archive.md); the clip shows what is there.

## Tests

`cargo test -p heimdall-archive clip`:

- `crates/archive/tests/clip.rs`: a synthetic store with hot and cold audio, a silent source, HEVC epochs with a hole,
  a transcript line, a writer holding the index lock, constant-fps resampling, cancellation leaving nothing behind,
  damaged frame and audio objects, and the command itself.
- Unit tests in `clip.rs` and `clip_writer.rs`: cues, labels, times and the midnight roll, launches and sleeps,
  mixed-rate callback spans, scaling, both containers.
- `crates/agent/tests/mcp_v2.rs`: the tool's schema and validation; `crates/app/src/mcp_clip.rs`: the Seeing-off
  refusal.

## See also

- [archive.md](archive.md): what the archive keeps and the reader lock
- [screen.md](screen.md): the screen recorder and Seeing
- [capture.md](capture.md): the audio sources behind each track
- [chunk-index.md](chunk-index.md): finding the window to clip with `timeline_search`
- [mcp.md](mcp.md): `archive_clip` and `timeline_search` output `clip`
- [home.md](home.md): where `<home>/.heimdall` is
