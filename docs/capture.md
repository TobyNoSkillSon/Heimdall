# Audio capture

Beta: echo cancellation ships off, so barge-in works best with headphones.

How `crates/capture` records the microphones and app audio. Code map: `crates/capture/README.md`. Where the samples
go after capture: `docs/archive.md`.

Nothing in compilation, synthetic tests, `Capture::discover_only` or `Capture::idle` creates a tap, starts a device,
checks screen or AX access, requests a permission or plays a probe. Live capture runs only in the signed app after
the user's setup grant.

## Microphones first, system audio on its own

The microphone never waits for system audio. macOS asks for System Audio Recording when a process
makes its first process tap, and that call blocks until the prompt is answered: in the VM runs of 5 Nov 06:56-07:16 an
unanswered prompt held the HAL thread's first pass for about three minutes, its start timed out, and the microphone
never opened. Now (`src/tap_gate.rs`):

- The first tap is asked for once per process, on its own thread (`TapGate::access`: a private global tap made and
  destroyed, `macos::tap::ask_system_audio`). The app sends it at Setup's Allow All too (`capture::ask_system_audio`,
  non-blocking), so the prompt shows there.
- Each HAL pass opens the selected microphones at once and makes per-process taps only once the ask came back
  (`tap_gate::opens`). The first pass (and the start's ready signal) never includes a tap that is still being asked.
- The ask coming back wakes the HAL thread; the taps join on that pass, and the open microphones are not stopped or
  reopened (`tap_gate::stops`: only sleep, a route change, a wake, or a source no longer wanted stop one).
- A failed ask is `Off`: no taps, a capture diagnostic "System audio off: …; the microphone runs without it", asked
  again after a new grant (`refresh_permissions`, the menu's or Setup's Allow). Still out after 15 s: "System audio
  waits for its permission; …". macOS reports a denial only as silent taps; the app reads that from the audio.
- `capture::system_audio()` says where the ask is, for the app's Getting ready note.

## Stream contract (for app and listen)

```rust,ignore
let (capture, events) = capture::Capture::start(config, capture::CaptureConsent::after_setup(mic, system_audio))?;
// CaptureEvent::Audio chunks feed listen, keyed by chunk.source_id.0.
// SourceAdded/SourceRemoved bound each source's model state.
// Gap or chunk.discontinuity resets that source's VAD/ASR accumulators.
```

- `CaptureReceiver::{recv, recv_timeout, try_recv}` is a channel. `AudioChunk` is mono f32 at exactly 16 kHz, normally
  320 samples (20 ms), a final chunk possibly shorter.
- `host_ns` is the first output sample's time on Core Audio's machine-wide `mach_absolute_time` clock, in ns. It is
  not wall time and not a per-source origin. Join by session + host time: host time alone crosses boots, and it
  pauses during sleep (sleep/wake makes new generations and explicit breaks, never invented samples).
- Resampler latency is removed by dropping its delayed head; output is timestamped from the latest native callback
  anchor plus the resampler position, so device clock drift does not accumulate. The first sample origin is
  continuous (a fresh source's first chunk is not a gap). Clock regressions, dropped frames and listen overflow mark
  a real discontinuity and reset the resampler. The archive keeps the original native host time.
- SourceAdded means armed, not sounding. Digital zeros (a paused app) are normal audio: no alarm, no gap. Silence
  never proves a permission or route failure.

## Source identity

- IDs carry the device UID, or PID plus process birth time, and a capture generation (`mic:<uid>:<generation>`,
  `process:<pid>:<birth>:<generation>`). The app treats a source back within 5 s of its removal under a new
  generation as a route change: it journals `source_readded` and logs `capture: source_readded (…)`. It strips only
  the generation to match the device, so `screen:1` and `screen:2` stay two devices (`crates/app/src/pipeline/sources.rs`).
  Each capture health line names the microphones open then (`island: capture recovered: Microphone OK (input: Shure
  MV7i)`), so the log alone says which input is live.
- Each process/output-device/output-stream route has its own stream. A process with no output device in its HAL
  membership gets an include-only process mixdown tap, not an invented default device.
- Labels carry hardware name/UID or process name/bundle ID/PID; metadata keeps owning app, route, native PCM encoding,
  channels and rate. Electron and Chromium helpers resolve to the outer `.app` and its Info.plist bundle ID and name.
- Heimdall's own process is always captured, whatever App Audio says. Downstream code may refuse its TTS as a wake
  trigger; capture never removes it from the archive.

## App Audio: only chosen apps are recorded

No app is recorded by default ("pick and choose what app is being recorded, i dont think recording
eeryng by defoult and automatically makes much sense").

- `CaptureConfig.recorded_apps` lists app keys; `Capture::set_recorded_apps` changes them live. The filter runs at
  discovery, before any tap: an unrecorded app gets no tap, no queue, no aggregate device and nothing archived.
- The key is `capture::app_identity`: the outer `.app`'s bundle ID, else its path, else the process's own HAL bundle
  ID (an XPC service such as `com.apple.WebKit.GPU`, which plays Safari's audio), else the executable path. No PID or
  generation, so a choice holds across relaunches and new helpers.
- Checking an app taps its current processes at once; unchecking ends them as removals with reason
  `app audio switched off`.
- `CaptureConfig.playing_apps` receives the sorted apps producing output now (Heimdall excluded) whenever the set
  changes, in capture and discovery-only mode, from HAL listeners with no tap.
- Gotcha: HAL never notifies `kAudioProcessPropertyIsRunningOutput` changes on this macOS (probe 12 Oct: `afplay`
  went 0 → 1 with no callback). `kAudioProcessPropertyIsRunning` fires about 50 ms after IO starts and when it stops
  (up to about 2 s after a player goes quiet). Discovery watches both and reads the output flag at that event.
- The app keeps the session's heard apps (`settings::merge_heard`, at most 24 neither recorded nor playing) and
  saves choices in `settings.json` as `recorded_apps` (key, name, `.app` path for the icon), omitted while empty. The
  old PID-keyed `disabled_sources` is read and ignored.
- Onset: processes of recorded apps and Heimdall are armed with an include-only, auto-start-gated tap as soon as HAL
  publishes the process, before its first output. Waiting for `IsRunningOutput` lost about 0.8 s of each new
  player (12 Oct); arming at publication lost 0 ms. `kAudioAggregateDeviceTapAutoStartKey` stays on (turning it off
  gave no repeatable gain).

## Microphones

- One primary input: The user's chosen device when connected, else the built-in mic (found by Core Audio's built-in
  transport type, not by name). First run chooses built-in. With neither, no mic opens.
- Extra mics open only when switched on in the menu; they start off. No other input is ever opened or used as a
  fallback, so an unchosen AirPods mic never forces AirPods into Bluetooth headset mode.
- A mic's IOProc reads input only: its output streams are switched off (`kAudioDevicePropertyIOProcStreamUsage`), so
  a duplex USB mic does not make Heimdall look like a call that claims the user's AirPods back from their iPhone.
- `CaptureConfig.microphone` (primary UID, `None` = built-in) and `extra_microphones` seed the HAL owner;
  `Capture::set_microphones` changes them live. `CaptureConfig.microphones` receives a `MicrophoneState` (every
  input, the choices, the primary in use, the mics opened) on change; discovery-only mode reports it too.
- Selection is recomputed on every HAL pass: a chosen mic disconnecting switches to built-in, reconnecting switches
  back. Each switch is a removal (reason `microphone switched off (primary: …)`) and a new source with a new
  generation, so listen resets and the archive records the boundary as for a hotplug.
- A mic whose four recovery attempts are exhausted is skipped (built-in stands in for a primary) until a HAL event
  for it or a new choice in the menu.
- Saved in `settings.json` as `microphone` and `extra_microphones` with names, omitted at their defaults.

## Device lifecycle and buffers

- Read-only discovery uses HAL device-list, process-list, default-route and service-restart listeners, plus device
  alive/rate/format and process output-activity/IO-running/device-membership listeners.
- Taps stay armed across pauses and are destroyed on process exit, route or format change, sleep, consent
  revocation or shutdown. Sleep stops streams; wake reconciles fresh sources and formats.
- Native owners share one control thread; callbacks only wake the drain pump. No intake sleeps, activity polls or
  per-stream watchdogs. One shared 3 s health timer runs only while sources are active; it rechecks sources without
  callbacks with bounded recovery (four failures, then wait for a relevant HAL event) and never retries an exited
  process forever. A dormant process without callbacks is not a stall.
- Each source preallocates 256 packets of up to 8,192 samples (8 MiB; 2.73 s at 512 frames/48 kHz), sized after a
  commit stall lost 928 ms past the old 171 ms queue. Callbacks only copy and convert into it: no allocation,
  logging, compression, SQLite or HAL polls. Reentrant callbacks are refused.
- The pump drains packets into a heap stage of at most 30 s per source (16 MiB cap for many-channel devices), so
  the archive's once-a-second commit can wait behind another writer's index lock without losing audio (22 Oct: a
  7.6 s stall lost 235,520 frames past the ring). The intake thread resamples for listen and only queues archive
  work; its own thread (`heimdall-archive-commit`, `crates/capture/src/commit.rs`) commits, so listen keeps hearing
  every source while a commit waits on the lock (27 Oct: a compaction step held it, the Shure looked silent for 16 s
  four times and the stall ladder relaunched Heimdall). Overflow and invalid native buffers become explicit archive gaps;
  native stop/destroy errors become gap events.
- Listen's queue holds at most 512 chunks globally; a slow listener gets a gap and discontinuity while the archive
  continues. If archiving fails, capture stops rather than record unarchived audio.
- A capture stop waits at most 2 s (`CLOSE_WAIT`) for the archive thread's last commit, then hands it off;
  `capture::settle_stopped_archives` lets Quit wait for the rest. The live loop never waits for a stop: a restart or
  Listening off stops the old run on its own thread (`CaptureRun::retire`, `crates/app/src/live_loop.rs`), and Quit
  waits up to 4 s for those (`settle_retired`). A restart's old run still forwards its last events, so generations
  count across every run in the process and a new run's sources never share an id with the stopped one's.

## What capture archives

Each capture run is one segment of the app's session in the archive store (`capture::Archive`,
`crates/capture/src/archive.rs`). Raw blocks precede downmix, resampling and any speech detector; no VAD output
removes audio.

- PCM is normalized f32 at the native rate and channel count, zstd level 1. Float32 and signed integer PCM up to 24
  bits are exact; signed 32-bit and float64 round to float32. Endian and high-aligned integer layouts are supported.
  No gain, denoise, clipping, channel mixing or rate conversion before archiving.
- Non-PCM formats, mixed stream encodings or rates, more than 32 channels and unreadable properties report a gap,
  never an assumed 48 kHz stereo.
- Exactly +0 runs are frame counts; −0 and every nonzero bit stay PCM, so quiet speech a detector missed survives.
- Callback rows commit about once a second (hot blobs first, then one index transaction); a crash loses at most
  the uncommitted second.
- `Archive::read_blocks` reconstructs samples in capture order with zero runs and discontinuity flags;
  `Archive::open_read_only` reopens a session for the same.

## Echo guard on speakers (ships off, untested live)

Through the MacBook speakers the mic hears Heimdall's replies. While the app asks (`set_echo_cancellation(true)`:
Heimdall speaking on speakers, or an ack waiting to play, plus a short tail), an input-only Apple voice-processing
unit (`macos/vpio.rs`) runs on the primary mic's device on its own worker thread (1–3 s cold; a release during
creation wins). The app asks only when settings `echo_cancel_off` is false; it ships true until a live check passes.

- The mic's own IOProc keeps running and hands listen the cleaned mono audio frame for frame (`echo.rs`: SPSC ring,
  10 ms prefill and crossfades, raw on underrun, skip ahead past 100 ms), with its own timestamps and source, so
  ClockMapper and listen see no restart.
- The archive always keeps the raw samples: each mic packet carries the cleaned copy beside
  them (`pipeline::Packet::clean`).
- Refused (each a logged `echo cancellation:` diagnostic) unless the default output is speakers (not Bluetooth, the
  headphone jack, a USB device with inputs, AirPlay or virtual), the default input, alert output and mic are not
  Bluetooth, no Bluetooth headset is connected, and the mic is not built-in (`CLEAN_BUILT_IN_MIC = false` until a live
  format check). Checked before the unit exists, after Create, Initialize and Start, and every tick.
- Torn down on release, a mic source change, a route change off speakers, 1 s without processed input, or 120 s.
  Ducking is Min (about −8 dB).
- Every unit closes in reverse, then the system is compared with a snapshot (default devices, nominal rates) and
  put back. Two failed starts, or any change found, switch echo cancellation off until Heimdall restarts (23 Oct
  17:38: every app's output went silent on AirPods after failed starts).
- `hal::snapshot` skips the unit's `VPAUAggregateAudioDevice` and built-in speakers shown as an input.
- `echo_cancellation()` reads `EchoCancel::{Off, Starting, Active, Refused, Failed}`; `echo_control()` gives a
  `Clone + Send + Sync` handle for another thread, inert after drop.

## Stable signing

`scripts/bundle.sh` pins Apple Development certificate `51B366DDFD3BEA342E2AA8545D90146C9797F043`, refuses an
unavailable identity, never falls back to ad-hoc signing, and compares the old and new designated requirements
before replacing the bundle. Keeping that identity is what keeps TCC grants across rebuilds; renewing the
certificate or switching identity needs an explicit plan. Accessibility's usage string grants nothing.

## Live probe (agents run it, not the user)

Before any grant only the no-stream idle probe may run:

```sh
PATH="$HOME/.cargo/bin:$PATH" CARGO_TARGET_DIR=target/capture nice -n 19 cargo build --release -p capture -j 2
./target/capture/release/heimdall-capture-probe --idle-seconds 30 --archive-dir target/capture-idle-measurement
```

Live mode needs the probe as the stable-identity app's main executable, launched through LaunchServices (never the
nested binary from Terminal):

```sh
./scripts/bundle.sh --probe
open -n --stdout "$PWD/target/capture-live-probe.log" --stderr "$PWD/target/capture-live-probe.err" "$PWD/dist/Heimdall.app" --args --after-permission-grant --live-seconds 60 --archive-dir "$PWD/target/capture-live-probe"
```

The probe refuses live mode without `--after-permission-grant`. Each source prints its metadata and a PCM receipt
(chunks, samples, nonzero count, peak, first/last host time), so a created-but-silent tap shows. Rebuild the normal
bundle with `scripts/bundle.sh --edition personal` afterwards.

Ignored lab tests (need the existing grant, isolated lab paths, short windows; they never replace or relaunch the
installed app): `crates/capture/tests/tap_onset.rs` (a fresh `afplay` against a 48 kHz PCM16 WAV: waveform match,
fresh-stream discontinuities, capture CPU) and `crates/capture/tests/app_audio.rs` (unrecorded app untapped, recorded
app tapped, unchecked again). Run with `HEIMDALL_TAP_LAB_DIR` (empty dir) and `HEIMDALL_TAP_LAB_WAV`, `-- --ignored`.

Not yet driven live: physical hotplug, PID reuse, Bluetooth and default-route changes, sleep/wake, an active source
losing callbacks, and a rebuild preserving TCC access. No ScreenCaptureKit fallback exists for audio.

Reference costs (busy M5 Max, process CPU only, not watts): idle discovery with no streams 0.0016 % of one core
(11 Oct); 30 armed dormant taps added about 0.0006 points; one active digital-silence stream 0.25 % (12 Oct). A
recorded `afplay` started 163.6 ms into capture with waveform correlation 0.963 (12 Oct).
