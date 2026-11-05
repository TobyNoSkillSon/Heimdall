# Command decisions and agent context

Beta: the decider sometimes wakes Heimdall when you were not talking to it.

The decider (V6 by default; V5 is the rollback, Settings `decider_model`) does three jobs, and the app asks it nothing else: the wake (send, wait or not for Heimdall, with
the acknowledgement line, on every sentence), suggest yes/no on every line (`docs/suggester.md`), and confirming the
two screen commands below. Barge-in, the end-of-turn reply line, the fixed-command question and cancel-task left the
decider. There is no follow-up window: a reply right after Heimdall
spoke is the decider's to send like any sentence. The user's saying Heimdall's name and "send it" still wake at once.

Design:. A spoken command is decided once, as soon as it is detected, by d1 reading text.
The final transcript no longer gates commands and no transcript rule overrides the decider.

## Detection to decision

1. **Detection.** Phrases are matched only in Parakeet's text on the primary microphone (English or Polish): a
   partial transcript (screen phrases), a 15-s window decode, or the turn's final. A screen phrase holds the
   screen from the phrase's first word as soon as a partial or window carries it. Later detections of the same
   command (within 1 s) merge into it. (The keyword spotter was removed: it caught 0 of 7 screen
   phrases in 24 h.)
2. **Text for the decider.** A window or final detection uses its own words over the last <=15 s of the
   utterance up to 200 ms after the trigger. A partial's candidate waits for the window or the final.
3. **Decision.** One question, specific to the command the phrase proposes, with the original criteria:
   "Did the user, right now, ask Heimdall to look at their screen?" or "Did the user, right now, ask Heimdall to take a
   screenshot?", exactly as the decider's training rows ask them. Answers `yes`, `false_alarm` (discussion,
   quotation or narration), `unsure`. Text only (`DecisionMedia::Text`); no audio reaches the decider.
   Context is the recent delivered turns and the pending agent transcript.
4. **Outcome.** `yes` runs the command at once on the screen pinned at the phrase (screenshot attached to the
   turn) and the island shows its result. `false_alarm`/`unsure`
   shows nothing and releases the held screen. `unsure` and errors write an automatic review entry.
5. **Partial only:** a candidate only a partial carried is decided from the final's words; if the final no
   longer carries the phrase it is abandoned (`partial_phrase_retracted`) and its held screen released.

The main transcript is unchanged: the utterance is transcribed by the live stream when the user finishes, never
split at a command. Turns over 15 s are decoded in the stream's existing 15-s windows (segments), as before.

The spoken commands are "look at this" and "take a screenshot" (`pipeline::COMMAND_PHRASES`), the two decider confirms.
Cancel-task and the "stop completely" family were removed ("cancel the task", said
while discussing commands, once cancelled the agent's task); stopping the agent is the Agent window's or island's Stop, or
a request to the agent. The other screen aliases ("take a look", "look at the screen", the Polish ones) and the
pointing phrases are ordinary speech now. "Bug note" is not a spoken command: Heimdall files bug notes with its
`bug_note` tool (`docs/bugs.md`). "Stop speaking" and "open ..." were removed earlier: stopping is the barge-in
below, and an "open that in the browser" is ordinary speech that reaches the agent like any other turn.

`COMMAND_POLICY` is `command-check-v9;text-only;side-transcript-15s;single-question;native-escapes;no-transcript-veto;v5-two-screen-commands`.
The former audio route, two-question chain, confidence floors and transcript-grammar veto
(`command-directed-v4`, `directed-clause-v1..3`) are historical; their records remain in older archives.

The legacy `Question::SendNow` and `Question::FixedCommand` requests stay in the decide crate for d1 reference-golden
parity only; the app asks neither (the per-turn fixed-command question was removed). All local command
actions go through this chain.

## Barge-in

Decision 31 (it replaces the earlier one-second decider design): while
Heimdall's voice plays, speech that speaker ID confidently matches to the user stops it at once. Other voices and
Heimdall's own echo never do. No side transcript and no decider question (it would need a transcript, which is too slow).
Outside playback nothing is checked. There is no stop command. Code: `crates/app/src/pipeline/barge_in.rs`,
`pipeline/heimdall_voice.rs`.

1. **Arming.** The user's speech starts on the primary microphone while Heimdall is speaking (agent speech queued,
   synthesizing or playing, with Speaking on), or Heimdall starts speaking while the user's speech is
   already going on. Heimdall's own app audio, other microphones and dictation to Vella never arm it.
2. **The check.** From the onset, every 0.5 s the microphone's last 1.5 s (the 300 ms before the onset included, so
   the first check lands 1.2 s after the onset) is embedded by the speaker encoder (ReDimNet, in the speakers helper)
   and scored against the user's voice profile and Heimdall's own voices (`heimdall_voice::user_verdict`).
3. **Stop** when two windows in a row are the user's voice: at least 0.60 against their profile (`USER_MIN`), beating the
   score against Heimdall's voices by 0.10 (`USER_MARGIN`), and not Heimdall's own voice (`HEIMDALL_MIN`). A window
   quieter than RMS 0.0095 is not speech and is never scored: near-silent windows on the microphone score about 0.7
   against the profile. Stopping is `Tools::interrupt` (agent speech); the island shows nothing.
   Measured on archive audio (1.5 s windows, a profile from 12 of the user's other
   wakes): no echo stops, 0.7% of ten Discord guests' utterances and 6.7% of media clips would stop it, and the user over
   speaker echo stops it in 82% of mixes, median 1.04 s after the first word (audio time, plus about 0.24 s of
   encoder). The 18% that never stop were the user at or below the echo's level or one-word clips. Not measured: a live
   overlap, people in the room, guests heard through the speakers.
4. **Continue** on anything else: Heimdall's echo, another person, no confident speaker, a failed or lost check.
   While the speech goes on the next window is checked half a second later, so the user's voice starting over steady
   background talk still stops it. Speech that ends before the first check (a cough, "mhm") is not judged.
5. **Echo guard through speakers.** The microphone hears Heimdall's voice there: a window scoring as Heimdall's own
   voice is never the user's, and their score must beat Heimdall's by the margin. Heimdall's voices are enrolled silently
   from rendered lines (`docs/speakers.md`). The transcript echo guard (`pipeline/echo_guard.rs`) still keeps the echo
   out of finals and early sends, and every finished utterance on the microphone is judged by voice as a whole
   (`pipeline/mic_voice.rs`): only the user's voice print reaches the decider as the user; Heimdall's voice is archived only and
   another voice is "Unknown", never a wake. These checks do not depend on the barge-in's loudness gate: once
   Apple's echo cancellation left Heimdall's echo at RMS 0.003-0.006, every barge-in window was skipped as
   quiet, and the text guard was the only filter.
6. **The user's voice profile** (`user-voice.json` in Heimdall's data folder, `heimdall_voice::UserVoice`) is learned from the user's
   own primary-microphone turns that woke Heimdall outside calls while Heimdall was silent: each turn's middle six
   seconds, embedded by the same encoder; at most 16 samples, the newest kept; a sample joins only if it matches at
   least half of those kept at the speaker model's same-speaker threshold, and five misfits that agree with each other
   replace the profile (the mic changed). It is used from three samples on. Until then (a fresh
   install, before the first three wakes) a window confidently not Heimdall's voice stops it, so the user can always
   interrupt.

Speaking off still stops Heimdall at once; Listening off does not. Each checked window is a `barge_in`
evidence record (schema 2): the window, both scores, the verdict and the outcome (`docs/archive.md`).

## Evidence schema 1

Synchronous journal appends use SQLite WAL with `synchronous=FULL`, `fullfsync=ON` and
`checkpoint_fullfsync=ON`; authorization is committed before executing a command.

Evidence kind `command_decision` has phases `candidate`, `chain` and `abandoned` (`target_selection` appears only in older archives).
The candidate row records the proposal and its origin immediately, even if the source later
closes without a final. The chain row carries:

- `candidate`: source, trigger host nanoseconds, phrase guess, score (always 1 for text matches).
- `utterance_id`, `phrase_start_ns`, resident model `runtime`.
- `stage` (`early` at detection, `final` when the final decided it), `transcript` (the decider's text) and
  `decider_text`: its origin (`window_decode` or `final`; older archives may show `side_parakeet`) and
  timed words.
- `steps`: the one question, exact rendered instructions/options and state, typed answer with full
  distribution, complete native output, input tokens and error.
- `policy` version, explicit `aliases` with original byte spans, each step's `d1_ms`, `policy_choice` and optional
  `policy_probabilities` (`policy_veto` is always null); native answers/output are never overwritten.
- Policy `intent`, optional policy `selection`, optional `approved`, optional `error`. Approval is distinct from successful
  execution; `command_action` records the latter's error and dry-run flag. Screenshots keep their existing image
  evidence and additive transcript notes.

A false alarm has null selection/approval. An unsure answer has null approval and an automatic review entry;
inference errors also produce automatic entries. Source closure/gaps archive pending proposals as abandoned, without acting.

Automatic records go to `bugs/command-reviews/` beside the bug inbox, written privately and atomically as JSON and
Markdown: `kind: command_decision_auto`, `auto: true`, with the complete `command_decision` payload. They never enter
the bug inbox or the Bug Notes menu count. False alarms are journalled but create no review entry.

## Recorded versus supplied context

Routing happens in the shared app pipeline, before `Command::Observe` reaches any harness:

- Every opted-in microphone supplies context as the user.
- Known call apps supply context with their diarized speaker label or enrolled name.
- Every other recorded app remains in the raw archive and transcript index, available to `transcript_search`.
  It cannot enter pending text, d1 recent context, link-candidate context, the harness's observation store or wake it.
- Heimdall's own audible reply keeps its existing d1 conversational-context role; it is never a new harness
  observation or trigger.

Exact known bundles are `com.hnc.Discord`, `com.hnc.DiscordPTB`, `com.hnc.DiscordCanary`,
`com.tinyspeck.slackmacgap`, `us.zoom.xos`, `com.microsoft.teams2`, `com.microsoft.teams` and
`com.apple.FaceTime`. Capture's owning-app bundle takes precedence over helper-process identity; names and
substring matches cannot grant context access. This is a source-class policy: non-call audio emitted by one of
these call apps is also eligible. Browser-hosted calls are archive-only until their source class is explicitly
supported.

The existing diarization/name-enrollment actor now runs for every known call app. Discord Accessibility hints
are used only for Discord sources. If a call final lacks a diarized track, its archive status is
`awaiting_diarization`; it is held until the actor supplies a track/name. Unknown people retain an explicit
`Speaker n` label. At most 64 finals (at most about 123 MB of 30-second f32 waveform tails) wait across sources.
Overflow, gaps or closure preserve the transcript with a named archive-only reason. They never pass undiarized
mixed speech under an invented name. Multi-speaker utterances keep the existing dominant-track assignment;
this change does not split words between simultaneous speakers.

`agent_context_routing` records each archive-only/queued disposition and the exact labelled observation. In
eligible rows, `given_to_agent` means queued to the harness-independent bridge; the existing transport evidence
records what actually crossed the provider boundary. The Agent window still renders those transport records,
so excluded recordings never appear as sent messages. No menu switch was added.

## Checks and limits

The unit/integration checks cover all 24 intent/selection combinations, native option parsing and malformed
responses, inference errors, escape archival/inbox separation, successful screenshot attachment, spoken-note
lifecycle, external timestamp proposals, archive-only app routing and delayed named call delivery. The native
d1 test runs both new question heads and confirms an identical legacy SendNow/fixed-command result before and
after them. Existing native text/audio/image parity and send/timing tests also pass.

Real recognition and intent recall still need labelled
recordings.

## Proposals and merging

Each proposal is immediately journalled with its origin: `parakeet_partial`, `parakeet_window` or
`parakeet_final` (older archives may also show `keyword_spotter`). Proposals on the same source
within one second merge across origins; same-origin occurrences at distinct timestamps remain separate. Final
word timing becomes the representative trigger. A chain retains every original proposal in `proposals`. A late
duplicate adds a `phase: merged` row linked to the original utterance instead of another chain/action.
Primary changes and source gaps abandon old proposals.

The one-second coalescing window is an engineering timing allowance, not a learned acoustic alignment.

## Agent Off at the MCP boundary

The provider transport and MCP server share the same `DeliveryGate`. Each tool request receives one epoch
permit: Off prevents execution, cancels its future, and invalidates its HTTP response body even if Agent
is switched back On before delivery. The original permit is passed into speech, screenshot and search
implementations. Speech enqueue is serialized with Off; Agent Off interrupts active synthesis, playback and queued jobs
immediately. Listening off stops no speech.

Pending archive searches interrupt their active SQLite query and check cancellation between sessions/rows.
Screenshot work checks its epoch before capture and after acquisition; cancelled results remove their
scratch PNGs and cannot be delivered. A native capture already executing can finish before its result is
discarded. Bytes already yielded to the HTTP transport before Off cannot be recalled. The old listener is
closed before awaiting harness shutdown on restart. Loopback regression tests exercise all tools while
Off, in-flight cancellations, rapid Off/On, response-body suppression and queued speech without TCC or
playback.

## Live labels: ⌃⌘\ and ⌃⌘'

The user can label in real time when Heimdall should have woken up or suggested, without saying its name. Two global shortcuts (the physical backslash and quote keys, whatever the layout; listed in
the menu's Shortcuts) label lines for decider training. Code: `crates/app/src/pipeline/hotkeys.rs`.

- **⌃⌘\ Wake Heimdall.** Everything said since the last wake goes to the agent at once, as the user's own wake (wake
  sound, ack, relay or harness alike). Their latest unsent line (else anyone's) is labelled `send`.
- **⌃⌘' Suggest Now.** A suggestion pass starts at once, past the pacing and the "talking to Heimdall" hold; an open
  card makes way. The latest decided line is labelled `suggest` = yes, with the decider's suggest question asked once more on the
  exact snapshot it saw for that line.

- **The island's Stop (5 Nov).** While Heimdall speaks the notch's left ear is a stop glyph; a press cuts the voice
  at once (lines held for after a call go too) and labels the line V5 woke the speaking run for (`outcome` `send`)
  `not_for_heimdall`, source `owner-stop` ("this moment wasn't for Heimdall"). Only while speech plays at the press,
  only for the run whose reply was cut (a reply a T3 result or notice woke, a typed message, a wake key or a relaunch
  labels nothing), and once per line however often Stop is pressed. The turn goes on, but the rest of that run's
  replies are shown on the island unsaid until the user's own words reach the agent again.

A line still being decided when a key is pressed is labelled when its decision lands. Each press is also a `hotkey`
evidence record (`key`, and for wake `woke`, `through`, `lines`; for stop `speaking`, `labelled`).

Labels are JSON lines appended to `decider-v6-labels.jsonl` in the archive store (beside the index, normally
`~/Library/Application Support/Heimdall/archive/decider-v6-labels.jsonl`), schema 1:

| field | meaning |
|---|---|
| `task` | `send` (⌃⌘\, Stop) or `suggest` (⌃⌘') |
| `label` | `send`, `yes`, or `not_for_heimdall` (Stop) |
| `source` | `owner-hotkey`, or `owner-stop` (with `speaking`: whether audio played at the press) |
| `at_ms` | Unix ms of the label |
| `utterance_id`, `speaker`, `text`, `line_source` | the labelled line (the observation id the archive uses) |
| `decider_input` | the exact decider request for that line: the send decision's `prompt`, or the suggest request |
| `v5` | the decider's own answer: send `choice`, `probabilities`, `outcome`, `trigger`, `d1_gate`; suggest `yes`, `choice` (or `error`) |
| `woke`, `also_sent` | send only: whether the wake went out (false with Agent off) and the other unsent lines' ids it carried |

## A goodbye ends a call

The call window (`CALL_ACTIVE_NS`, 2 min after the last call line) also ends at a goodbye in the call, the user's or another speaker's
(`decide::send::farewell`, English and Polish, ending any sentence of the line with at most one word after it;
`call_active`), and at the user's own "I'm off the call" or "you can talk to me now" (`decide::send::off_call`). A call line
within 5 s after the goodbye is the call's own goodbye; any later call line starts the call again. In one case
the user said goodbye, another speaker answered, and the user left Discord; for two minutes their lines to Heimdall, sent by the decider with
high scores, were held as call chatter (`held_in_call`), and replies were held "in a call". Both the send gate
and the speech hold (`speech_policy::in_conversation`) use the goodbyes. The speech hold has its own, shorter evidence
(it once held three replies after the call): 45 s after the last call line
(`speech_policy::CONVERSATION_NS`), or while a call source's voice is open, and over 3 s after the last call source
stops. A reply held in a call is said, newest only, as soon as the user's own words reach the agent after it.

## The name as an address (5 Nov)

`decide::send::addressed_by_name` reads the name as an address when it opens a sentence (after interjections) or is set
off by a comma. The name as the subject of a statement is a mention: no punctuation after it and a third-person verb
next ("Yeah, Heimdall is searching already.", "Heimdall was down all night"); an inverted question ("Heimdall is it
done?", "Heimdall did you see that") and modals ("Heimdall can you…") stay addresses. In a call a mention waits for d1
(`call_trigger_waits`). Bug 2026-11-05T02-32-17: at 00:13:07 the user's "Yeah, Heimdall is searching already." to the
other person in the call woke the agent at once though d1 said not_for_heimdall 0.99. In a call, the user's lines that
d1 sends and the call gate holds feed the suggester instead (`docs/suggester.md`).
