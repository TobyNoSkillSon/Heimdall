# Speakers

Beta: Heimdall stores voiceprints of the people it hears, who never agreed to it; they stay on your Mac.

Who is speaking on a call: local diarization, voice identity, quiet naming, listened-to guests and Heimdall's own
voice. The SDK is `crates/speakers` (code map, live strategies, bake-off and calibration numbers in its README); the app
side is `crates/app/src/speakers_runtime.rs`, `voice_namer.rs` and `pipeline/` (`voice_names.rs`, `listen_to.rs`,
`heimdall_voice.rs`). The SDK captures no audio, draws no UI and requests no permissions.

Nemotron is the 2–6-person default; Sortformer v2.1 is the selectable four-person runner-up.

## Runtime

- `Diarizer::load` verifies the pinned Nemotron 3 Q8_0 GGUF SHA-256, creates the native model, and checks eight-track capacity and 10 ms output cadence. `stream(16000)` uses NeMo-Speech.cpp's C ABI `stream_open/push_f32/finish`, with mono f32 PCM and fixed rate. RAII keeps the model alive until the last stream closes. Both are intentionally `!Send/!Sync`; construct them on their owning worker thread.
- Default geometry is the publisher's ultra-low-latency setting: chunk 3, right context 1, left context 0, FIFO 264, speaker cache 264, update period 222, all in 80 ms encoder frames. The derived input buffer is `(3+1)*80 ms = 320 ms`, excluding computation and frontend startup. The measured first native output was at 340 ms of supplied audio.
- Default segments use 0.5 activity with no gap filling, padding, or minimum-duration filtering, matching NeMo's published `bypass_postprocessing=true` evaluation. This is a documented patch to the native header's CALLHOME defaults; it also governs frozen segments after native probability compaction. No test-set parameter sweep was performed.
- `ReDimNet::load` verifies the exact B6 **vb2+vox2 large-margin** ONNX FP32 artifact. `VoiceEncoder::embed` consumes clean 16 kHz PCM, repeats 2–6 s to six seconds, center crops longer inputs, and returns a checked, normalized 192-value embedding. `ort` CPU uses one thread and disables thread spinning. This is B6, not ReDimNet2+.
- `SpeakerPipeline` accepts 16 kHz PCM, uses a 12-second ring buffer and at most twelve seconds of clean samples per anonymous track. It removes predicted overlap and trims 100 ms at actual speech edges. Twelve seconds of accumulated single-track speech form two distinct six-second embeddings; the latest six-second query drives automatic identification. A new capture gap/call requires a new session; never join missing audio silently. The original mixture remains the capture/archive owner's responsibility.

## Enrolment and naming

`EnrollmentStore::default()` includes the measured calibration, with **no enrolled people**. It matches the best cosine across a name's stored exemplars; the six-second/two-exemplar cutoff is **0.455542** (FAR/FRR 2.08% each; protocol in the crate README, Measured 11 October). Missing or incompatible calibration, low similarity and exact ties return explicit `Unknown`.

`NamingFlow` keys tracks by source + call session + speaker number. A track's first two clean six-second windows either match an enrolled name (`LabelChanged`) or a **coded voice** (`VoiceCoded`): a voice no name matches gets a code label such as `Speaker Fri 23 Oct 21h #3` and its samples are kept in `speakers.json` under `voices` (first two windows plus the newest, at most eight; calls, days, first and last heard). The same voice in a later call is matched by the same calibrated rule and keeps its code. Every write re-reads the file under the app's enrollment lock, so two calls never code one voice twice. Voices ignored on the old prompt (`ignored`) stay unlabelled.

## Naming voices

Nobody is interrupted to name a voice: no popup, no notification, no sound. Two paths share one island card at a time
(`crates/app/src/voice_suggest.rs` and `voice_namer.rs`, wired in `pipeline/voice_names.rs`), on the background model
(Background Tasks ▸ Model, `crate::background_model`; Background Tasks ▸ Voice Names turns both off).

The returning-voice proposal below only looks at a voice heard on two local days in two visits with about an hour of
conversation, so a new person on one call could never qualify for it. The new-voice card covers that case.

**New-voice card** (one card per new voice):

- A coded voice on a track of an open, active call that said three lines and twelve words in the last half hour, the
  newest in the last three minutes, gets one silent island card, in this order: "New voice: “It was way faster than
  last week…”" (the newest sentence of four words or more, cut to the one line); up to two guessed names as one-click
  chips, only when there is a real guess; a name field ("Type the name"); Not important last. Never a question back
  at the user, no second card, no Figure it out, Later, Ask me after
  or Only if sure.
- The guess runs in the same background pass, before the card shows: the voice's lines and the lines around them
  (`voice_namer::context`), the call app, the names Heimdall knows, and the known voice it sounds closest to (cosine
  0.40 or more, under the 0.456 naming threshold). The model (`voice_suggest::SYSTEM`) lists up to three names; code
  keeps only a real guess: confidence high or medium (a low hunch is dropped), said in the transcript, never the user,
  Heimdall or a name turned down for this voice. Sure names first (confidence high, two hint lines where the voice
  says it as its own or answers within 20 s after someone says it, and never addresses it), then the model's order,
  then the voice it sounds like; two at most. Without a model, or when it fails, only the voice it sounds like.
- A chip names the voice and opens nothing else. Return in the field saves the name exactly as typed. Not important
  is Forget: the voice keeps its anonymous code, is never suggested again, and is listed in Speakers with Ask Again.
  Esc in the field, the lip, or five minutes unanswered put it away: the voice rests for three hours.
- The field never takes the keyboard by itself: every card now comes on its own, while the user may be typing in another
  app. One
  click on the field gives it the keyboard. Nor does it take Option plus a digit (no digits are drawn): a stray
  Option+1 in another app never answers it.
- Several new voices: the same simple card, one at a time, for the one that spoke last; the others get theirs later
  under the same rate.
- Rate: one card at a time, at least 10 minutes apart and six a day. The card goes when the voice's call goes quiet.
  The user can also just say it ("that was Alex": `name_voice` with voice "latest", below).
- Not covered: Discord's participant list through Accessibility is not read (no live AX fixture exists), and a
  voice that never reaches twelve seconds of clean speech in a call stays "Unknown", not coded, so it gets no card.

**Returning-voice proposal** (the older rule, kept for regulars): it runs only while a call is open, at most once a
minute, never while a card is up, and only for a coded voice that spoke in the last three minutes, was heard on two or
more local days in two or more visits (runs of lines three hours apart, so one call past midnight is one visit) and
has about an hour of conversation. Per voice, a model call needs 20 minutes since the last one and 15 new lines.

- It reads the archive read-only: the voice's newest 300 lines, the three lines before (within a minute) and one after each, and the names Heimdall already knows.
- Its answer is checked in code: confidence high; at least three hint lines in at least two separate moments (another session or ten minutes apart), each either the voice naming itself ("it's Sam") or someone saying the name with the voice answering next (within 20 s, same call); the voice itself never addressing that name; not the user, not a name Heimdall already knows, not one the user turned down. Names match across the recogniser's spelling drift (five letters or more, same first letter, at most two edits: "Katryn" is "Kathryn").
- Then one island card, only while the voice is in an active call: "Is this Sam? “Hey Sam, you there?”" with Yes (the samples become Sam's, every archived line is relabelled), the name field (another name, typed; not focused), and Not important last. An unanswered card is not shown again for 24 hours.
- The agent's `name_voice {voice, name}` names any voice later by speech ("the last person we talked to about the server is Sam"): it finds the label with `timeline_search`. With voice "latest" ("I was just talking to Dana") it needs no search: the coded voice that spoke last (an open call's voices first, else the archive's last 12 hours), and its reply says what that voice last said and which other unnamed voices spoke within 30 minutes of it, so a wrong pick can be renamed. The transcript's double-click and the Speakers window's rename take the same path (`Pipeline::rename_voice`): every archived line in every session first (`Index::relabel_speaker`, chunk speaker tags included, waited for), then the store, then the open calls' labels. A rename cut short can be repeated. Anonymous labels ("Speaker 2", "Unknown") are never renamed everywhere; a "Speaker N" double-click names only the coded voice behind that track.
- State: `voice-namer.json` in the data folder (per voice: last check, lines then, the card shown, names turned down, when it was suggested, its snooze; the cards shown today). Evidence: `voice_suggested` (with the guesses and any model error), `voice_namer`, `voice_card_answer`, `voice_named`, `voice_forgotten` journal events.

## Listening to guests

"Hey Heimdall, listen to Sam right now … or don't listen to them." Heimdall acts on the requests of a call guest the user
listens to (`crates/app/src/pipeline/listen_to.rs`):

- The flag is per named voice (`Enrollment::listening` in `speakers.json`), off by default and **on until the user turns
  it off**: no expiry at the end of a call, so the Speakers window's switch always shows the truth. The user's row is
  first, its switch locked on; Heimdall is never a guest; coded voices are named first (`name_voice`). A rename keeps
  the flag.
- The user flips it with the row's switch, or by voice through the agent's `listen_to {speaker, on}`. The tool works only
  in a run the user's own words woke (their microphone lines by source, or a typed message, within 10 minutes), turning it on
  only when those words ask to listen to that guest ("listen to Sam", "listen to them"; negations
  only turn it off), and never in a run a guest's line woke or steered (`SpeechPolicy::listen_change`). A guest's
  "listen to me" grants nothing. `name_voice` joining a voice to a listened guest, or renaming one, needs the same.
- The user's microphone is the user only in their voice (`mic_voice.rs`, below): a guest heard through the Mac's speakers lands
  on the microphone as "Unknown", and their "Heimdall, listen to me" grants nothing. Before their voice print exists
  (the first three wakes) the old rule holds.
- Wake: a listened guest's call line that addresses Heimdall by name (`decide::send::addressed_by_name`) wakes the
  agent at once with trigger `guest`, and that run may answer aloud during the call. Their own lines also count like
  the user's: a mention of the
  name or "send it" wakes as the user's would in a call (unless d1 says strongly it is not for Heimdall), and d1's own
  `send` follows the user's in-call gate (`held_in_call` unless they are answering a card). Only a whole line by that guest
  counts; a mixed call final keeps the by-name rule. `listen_to` stays the user's alone.
- Guards: the user's microphone, Heimdall's own audio and non-call sources are never guests; a guest line whose words
  Heimdall was playing through speakers then is its echo (`guest_wake_held`, reason `heimdalls_echo`).
- The agent's copy of each listened guest line is labelled `Sam (listened to)`; d1's conversation keeps plain labels.
  The prompt lets those guests ask by name and has Heimdall ask the user first before anything outward-facing or
  destructive they request. Evidence: `listening_changed`, `agent_context_routing.listened_guest`, `guest_wake_held`.

## Heimdall's own voice

Heimdall recognises its own voice only when it can hear itself, and enrols each voice before it is used:
couple of lines rendered in the background with no audio, run through speaker recognition and associated with
Heimdall (`crates/app/src/pipeline/heimdall_voice.rs`).

- **Enrolment.** Each voice that can speak a reply now (the chosen one, plus Supertonic, the lighter voice) renders seven varied lines silently, never played, on a low-priority thread while Heimdall
  is silent, the user is not talking and Speaking is on. Nothing is loaded or reloaded into the app's voices, checked
  before every line: a loaded voice only while already loaded, Supertonic from a private CPU instance (with 8 GB
  free) when it is not resident, never ElevenLabs (credit). Each line is embedded by ReDimNet as rendered and through a simulated speaker-to-microphone
  path (laptop-speaker band, a small room, the echo's level and noise floor measured on a studio microphone), at natural speed
  and at the user's speed. The samples are kept in `speakers.json` under `heimdall`, keyed by the voice's sample key
  (`supertonic`, `phonon-<voice>`). A voice is enrolled once; a new voice or a new recipe enrols again.
- **Identification.** `EnrollmentStore::identify` scores Heimdall's voices as the name "Heimdall" at 0.6 or more
  (people: 0.456), so a call track in its voice is labelled Heimdall, never coded as a new voice, and a guest is
  never labelled Heimdall by a near match (that label cannot be renamed).
- **Re-enrolment.** A profile carries its recipe and the model weights it was rendered from
  (`heimdall_voice::recipe_for`): a new voice, a new model behind the same voice or a new recipe enrols again in the
  background on the next quiet moment, and the old profile keeps working until then. ElevenLabs is never rendered for
  this (credit): its echo is not labelled Heimdall, but it is not the user's voice either, so it never wakes (below).
- **Who speaks on the user's microphone** (`pipeline/mic_voice.rs`): the mic labels
  speech as the user only when it actually matches their voice signature. Every utterance on their microphone is embedded
  (at most six seconds: the whole of a short one, the six most spoken seconds of a long one), asked at its speech end
  so the final rarely waits (at most 0.7 s), and judged (`heimdall_voice::mic_verdict`): Heimdall's voice at 0.50 or
  more, above the user's score, is Heimdall's own (archived as Heimdall, `archive_only` with reason `heimdalls_voice`; never
  the decider's or the agent's); their print at 0.55 or more, beating Heimdall's by 0.10, is the user. A voice is
  **Unknown** only when that is safe to say: 1.5 s of speech at speaking level scoring under 0.40 against their print,
  or any line not theirs while Heimdall was audible through speakers. Unknown is labelled "Unknown" and given to the
  agent as context, but it never wakes, sends early, answers a card or authorises `listen_to` (d1's gate holds it as
  `held_not_toby`). Anything else (a short or unclear line, no print yet, the encoder down or late) stays the user's,
  behind the text echo guard. Over Heimdall's echo only the audio of the words the text guard kept is judged (the user's
  line then, or Unknown when not theirs; Heimdall's only on Heimdall's own score). Only the primary microphone is judged
  (the print and thresholds come from one studio microphone). Measured on five days of archive:
  0.7% of the user's lines become Unknown; 15% of guests' lines heard on the microphone would still count as the user's (mostly
  short ones); no echo counts as theirs. Evidence: `mic_voice` (scores, verdict, reason, speech time, level). A line d1
  would have sent but held as Unknown counts only toward starting the print again (five such lines that agree with
  each other: the microphone or voice changed).
- **Barge-in.** Every half second while Heimdall speaks, the microphone's last 1.5 s is compared with those voices
  (0.45 or more: Heimdall's echo) and with the user's own voice, learned from their wakes (`user-voice.json`). Only their
  voice stops Heimdall; its echo, other people and unsure windows never do (decision 31, `docs/decision-chain.md`).
- **Encoder.** ReDimNet alone in the bundled `heimdall-speakers-worker` (`EncoderHelper`, CPU, one thread, about 0.24 s
  per window on the M5 Max), started on first use and ended after two minutes idle.

## Discord Accessibility probe

`ax::is_trusted()` uses **AXIsProcessTrusted**, with no prompting options. The snapshot reader never sets `AXManualAccessibility`, clicks, focuses, or changes Discord. It enumerates exported scalar attributes and children with 1500-node / 20-level / three-second bounds and marks truncation. `AXSelected` and focus are never interpreted as speaking. Explicit speaking state may be absent on real Discord; that remains unknown.

No live AX inspection was performed. `tests/fixtures/discord-ax.json` is explicitly a **synthetic recorded-tree-format fixture**, covering participant names, explicit speaking, missing state and unrelated text. It is not evidence that Discord exports its green ring. After the user grants Accessibility, record snapshots during a known speaking transition, retain actual attribute names, and add that real fixture without inferring state from focus/selection.

Agent run line from the project root, after Accessibility is granted:

```sh
PATH="$HOME/.cargo/bin:$PATH" CARGO_TARGET_DIR=target/speakers cargo run -p speakers --bin heimdall-ax-probe
```

An optional PID argument inspects that process instead of discovered Discord bundle-family processes. Run it signed in the permission-holding Heimdall context: a development Cargo or terminal executable may still be untrusted after the app is granted Accessibility; the probe then exits without prompting. Agents never change TCC.

## Measurements

Current numbers (identity v2: 99.2% balanced accuracy on held-out AMI people; DER on the 480 s AMI crop) and how to
reproduce them are in `crates/speakers/README.md`. The first 11 Oct identity protocol (three-second windows, 72.7%
known-name accuracy) is superseded.

## Primary sources

Attribution for weights, corpora and annotations (`crates/speakers/THIRD_PARTY.md` points here).

- Native code and C ABI: https://github.com/NVIDIA/NeMo-Speech.cpp/tree/e59dfb0b5c22655620f41391cc9a000d6f56b802
- Nemotron weights, geometry, benchmark and annotation links: https://huggingface.co/nvidia/Nemotron-3-Diarization/tree/f667ed73aee57d40cc39428eb768b4fd87a0a29e
- Evaluation defaults: https://github.com/NVIDIA-NeMo/Speech/blob/main/examples/speaker_tasks/diarization/neural_diarizer/e2e_diarize_speech.py
- B6 ONNX contract/provenance: https://huggingface.co/soniqo/ReDimNet2-B6-ONNX-FP32/tree/e911e3f063899805f3d94ee5d1db53fff8e9f3e8
- B6 clean verification baselines: https://github.com/PalabraAI/ReDimNet2
- Core ML comparator: https://huggingface.co/aufklarer/ReDimNet2-B6-CoreML
- AMI manual references: https://github.com/pyannote/AMI-diarization-setup/tree/67c2d539286e89f68952d5dcf83912bd9f01dfae
- AMI forced-alignment references: https://github.com/nttcslab-sp/diar-forced-alignment/tree/9527b7c64846fb38316a610f32e9d3466bd6d8b7/AMI/test
- AMI corpus mirror: https://groups.inf.ed.ac.uk/ami/AMICorpusMirror/amicorpus/ES2004a/audio/ES2004a.Array1-01.wav
- Opus reference source: https://github.com/xiph/opus/tree/7b05f44f4baadf34d8d1073f4ff69f1806d5cdb4
