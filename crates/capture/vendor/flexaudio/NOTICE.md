# flexaudio extraction

Upstream: https://github.com/Studio-Sadola/flexaudio
Revision: d3fe9bdb75fc883371757a780fb20b5efc39317a (workspace 0.5.1).
Copyright (c) 2026 tubome / Studio Sadola. MIT license in LICENSE.

The original tap, buffer, mic, process-list and common sources were kept here as
provenance, never compiled, until 14 Nov 2026; they are in git history and at the
upstream revision above.
Heimdall's src/macos/tap.rs and hal.rs adapt the process-tap/aggregate dictionary,
native callback, property reads and teardown primitives. The microphone owner
and callback-to-raw-sink pattern from flexaudio-mic is adapted to a HAL device
IOProc, sharing the tap callback's buffer-list conversion. This replaces CPAL's
name-only selection with exact HAL object/device UIDs and removes mic preflight
prompts and all per-source owner/intake/watchdog polling. No upstream VAD,
mixing, playback self-probe, capture-health policy or stream engine is adopted.

Changes: query tap ASBD before allocating the sink; native channel preservation;
shared callback-signalled pipeline; preallocated bounded queues; one HAL owner;
HAL event listeners, sleep/wake observers, app ownership and process-birth IDs;
lossless native archive before mono resampling. MIT attribution is also copied
into the signed bundle. Registry dependencies retain their own licences.
