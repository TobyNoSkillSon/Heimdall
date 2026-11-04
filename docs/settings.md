# Live settings

Every setting applies in the running app; none waits for a relaunch. Menu actions apply immediately. Atomic edits or
replacements of `<data-dir>/settings.json` are detected every two seconds by modification time, length and inode.
Known values are validated before applying them; a malformed revision is reported once and the previous configuration
stays. Unknown fields are ignored at every depth. Deleting the file does not reset a running app. The raw archive,
transcript database and session identity stay open during ordinary setting changes. Fields not listed here are
documented at their declaration in `crates/app/src/settings.rs` (`Settings`).

Beta: the menu can hang briefly.

## Switches and user settings

| Setting / control | How it applies |
|---|---|
| Primary `microphone` | HAL reconciles only affected input devices; existing app taps stay armed. A disconnected choice keeps its labelled built-in fallback. |
| `extra_microphones` | Only opt-in extra inputs open or close; other microphones and app audio keep their source generations. |
| `recorded_apps` / App Audio | HAL arms or removes only the changed app's processes. None recorded by default. |
| `screen.resolution`, `screen.fps` | ScreenCaptureKit streams update on their owner thread and start a new codec epoch. Default half native pixels, 5 fps; stored footage keeps its geometry (`docs/screen.md`). |
| Listening tile / `paused` | Off drains capture and listen, records the deliberate gap and unloads the decider. The agent, speech and the screen keep running on their own switches, and the agent is not told. Its stop intent has its own latch (`Tools::listen_latch`), persisted beside Agent's but never part of delivery. Setup's Allow All orders On at the click (`SetupAllowed`); the permission dialogs' completion carries no intent, so an Off clicked meanwhile wins. |
| Seeing tile / `screen_off` | Off stops ScreenCaptureKit and clears pending command holds; footage stays in the archive. Voice screenshot commands add `[screenshot not taken: screen capture unavailable]`; the agent's screenshot tools fail plainly and never name the switch. On records again once setup was allowed and Screen Recording is granted, whatever Listening says (22 Oct). Written only when true. |
| Agent tile / `dry_run` | Off closes the transport gate at once, then only the agent actor changes to dry run. Queued observations stay local; capture and transcription continue. Default Off. |
| Speaking tile / `speak_replies` | Atomic speech gate; Off interrupts playback without touching capture, agent or archive. |
| Agent ▸ Model: `agent_model`, `api_models` | The agent restarts on the new model or budget; a thinking change for the running model needs no restart. `docs/models.md`. |
| Agent ▸ Model ▸ Claude Code subscription: `agent_relay`, `agent_relay_model`, `agent_relay_account` (tool keys of the same names) | The relay on or off, its T3 model and its T3 Claude instance (none: automatic); a change restarts a running relay. Only what T3's catalog lists is offered (`docs/claude-relay.md`). |
| `agent_speed` (hidden) | Per request, OpenAI routes only. `docs/models.md` ("Speed"). |
| Background Tasks ▸ Model / `background_model`, `background_efforts`, `task_models`, `background_off` | From each job's next pass (`background_efforts`: a thinking level per model, written only when one is set; a model without one runs each job's own effort; `task_models`: a task's own model, always written, the personal island agent on Claude Haiku 5.5 through T3 by default). Providers include `t3:<instance>`, a Claude Code instance in T3. `docs/models.md` ("Background Tasks ▸ Model"), `docs/background-call.md`. |
| Personalisation ▸ Name & Icon / `identity` | Name (also the wake word: "Ivy, hello"), user name and mark (`docs/personalise.md`). An older file's `wake_words` loads and is ignored. The menu, islands and mark change at once; the wake spotter and decider from the next decision; the recognizer's name bias with a speech-model reload; the agent's prompt by a Pi reload after its turn. Written only when not Heimdall's own. |
| Personalisation ▸ Wake Sound / `wake_sound` | macOS's Tink at a quarter volume when a spoken request goes to the agent (not typed ones, not with Agent off). On by default. Independent of the Speaking tile. Its sound on the microphone is archived as Heimdall's, never a turn or a wake (`bridge/speech_policy.rs`). |
| `ack_delay_ms` / the `settings` tool's `ack_delay` | No menu row. Quiet after the user's last voiced audio before the spoken ack ("On it.") and the island's send arrow and thinking dots, one setting for both (`pipeline/ack.rs` `hold_island`). Default 0: the ack and dots come as the wake goes, about 1.6 s after their last voice. Never delays the wake itself; at most 10000. The tool takes seconds as the user says them (`2`, `2.5`, `"1500 ms"`) and sends `UiAction::SetAckDelay`. Read at each wake. |
| `free_space_floor_bytes` | Shared atomic floor for the raw writer, transcript writer and screenshot/TTS guards; a change invalidates raw-writer capacity credit. Minimum 1 GiB. |
| `launch_at_login` | SMAppService registers or unregisters in place. A rejected registration is named in Reload and retried there. Only a signed bundle can exercise it. |
| `archive_format`, `archive_choice_pending`, `retention_days` | Fixed policy: normalized to native lossless, no pending choice, permanent retention. Archive shows two read-only captions. |
| `setup_requested` | Whether already-granted capture may start; editing it requests no permission. Only the setup UI requests grants. |
| `system_audio_confirmed` | Records that delivery was observed; grants nothing. |
| `disabled_sources`, `voice`, `harness` | Ignored legacy fields; never written. |
| Speaker name / skip / rename / forget | Naming/enrollment actor path; no capture or model restart. |

## Models and backends

Backend choices live in the settings file, so they change on the running app without environment variables.
Probe-only environment switches remain for the standalone parity and measurement tools.

| Model / choice | How it applies |
|---|---|
| Parakeet Ultra / `parakeet_backend` (`metal`, `coreml`) | A candidate is built on the listen inference thread, then only the recognizer is replaced. All sources' PCM, VAD, transcript/timestamp state and evidence hook stay. A failed candidate keeps the old model. |
| Smart Turn / `turn_backend` (`cpu`, `metal`) | Same transaction, replacing only the turn detector. Metal is the default: 9.2 vs 21.4 ms and 0.030 vs 0.110 J per decision (process CPU + IOReport GPU), with 7 flips in 8,338 held-out pauses, all within 0.006 of the threshold (not exact parity). Metal needs the bundled matching assets; missing assets keep CPU and name the failure in Reload. A settings.json from before this default holds `"cpu"` only as the old default (there is no UI for it), so the launch migrates it to `metal` once and records `turn_backend_migrated: true`; a later deliberate `"cpu"` sticks. |
| Silero | One pinned native CPU kernel; not selectable. Its per-source state survives ASR/turn swaps. |
| Decider version / `decider_model` (`v6`, `v5`; default `v6`) | Which trained decider runs (4 Nov, "Install V6"). V5 ships beside V6 in all three precisions as the rollback: set `"decider_model": "v5"` in the file or through the `settings` tool. Honoured at every load, and a live change hot-swaps the decider exactly as a precision change does (below), at the same precision; a failed one goes back to the version that ran. If the chosen version has no folder on this Mac the other one loads at the same precision, and the log and the Decider section say so. A file without the field, or with an unknown value, runs V6. The log names what loaded: `decider: d1-2B-heimdall-v6-4bit (Decider V6, 4-bit) loaded in 0.4 s`. |
| Decider (d1-2B) / `decider_precision` (`16bit`, `8bit`, `4bit`; default `4bit`) | The menu's Decider section (click, then Confirm), Setup's Decider step or the file: the d1 owner finishes the decision in flight, destroys the old instance, loads the new folder and asks it one short decision before the menu stops spinning (`pipeline/decider.rs`, `decider_swap.rs`). Sentences arriving meanwhile wait in the owner's queue. A load that fails goes back to the previous precision, which is written back, and the reason shows under the section's header. A missing folder loads the nearest present one and says so. A file without the field, or with an unknown value, runs the 4-bit. Send uses a text-only 2,000-token context and argmax plus explicit triggers. `HEIMDALL_D1_LIBRARY` and the native/JSON transport override are launch-only developer inputs. |
| Supertonic / `tts_backend` (`auto`, `metal`, `cpu`) | Interrupts and drains the speech queue, then builds the chosen synthesizer for the next reply. |
| Nemotron / ReDimNet / `speakers_backend` (`default`, `redimnet`, `nemotron`, `all`) | The actor finishes and archives the old call segment, retires the helper, keeps naming, starts the new helper with the backend and resumes at the exact audio-clock boundary: a new call/track boundary. |
| Alternate diarization strategies | Research only; no production setting. |
| `--models`, debug traces, tensor captures | Process launch inputs, not settings. |

## Reload

**Reload** is normally disabled. A candidate that cannot load or a failed native apply leaves a named pending
component (`Transcription Backend`, `Screen Recording`, `Agent Session` or `Launch at Login`) and turns the native
row orange. Reload retries only those components in process; success removes the name. It never asks the user to quit.
Pi recovery and GPU fallback keep their own component statuses rather than claiming a model loaded.

Agent and Listening Off persist through the stop-intent latch (`<data>/stop-intent.json`), which survives a relaunch
even when settings.json still holds a stale On; `docs/resilience.md` ("Relaunch keeps everything") has the rules.

## Why microphones are final-only

Measured 12 Oct 2026 on the M5 Max (busy Mac), one public clip replayed four times in one resident process: final-only
decoding cut encoder calls per speech minute from 30.49 to 7.62 (75%) and listen-engine CPU per speech minute from
0.538 s to 0.370 s (31%), with identical transcript text. Listen-engine CPU only, not whole-app energy. Forced
15-second rollover decoding remains; responsive policies remain for lab tests only.

## Not verified on the signed install

Genuine talking plus app audio through swaps; real ScreenCaptureKit reconfiguration, history and display/wake races;
login registration; a real speaker-helper backend change. The 12 Oct receipts (`target/hotswap/evidence/`, replay of
six settings-file replacements with 528,000 of 528,000 samples reconstructed per source) used synthetic screen frames
and no native capture.
