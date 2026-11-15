# Speaking voices

What the Personalisation ▸ Voice picker offers, how each voice speaks, and how voice samples play.

Read this when: you want to change the assistant's voice, connect your own speech server, or change the picker or its
samples.
Code: `crates/app/src/speech_voice.rs` (`VoiceChoice`, `Voices`, `Route`), `crates/app/src/own_voice.rs`,
`crates/ui/src/mac/speaking_menu.rs` (the picker), `crates/app/src/bridge/voice_preview.rs` (samples),
`crates/speak/src/phonon.rs`.

## What the picker offers

- **Phonon S**: the built-in voice, Gradium's 31M-parameter English model with 37 presets. The picker shows four
  suggested presets (`ui::speech_voice::SUGGESTED_PRESETS`), chosen from the presets' catalogue descriptions:
  - Edwin (UK man), the default (`speak::phonon::DEFAULT_PRESET`)
  - Spencer (UK man)
  - Preston (US man)
  - Bryony (UK woman)

  A saved preset outside these stays listed, so the choice stays visible.
- **Own Voice Engine…**: any OpenAI-compatible speech server on this Mac (below).

Supertonic and ElevenLabs are no longer offered. A saved Supertonic speaks as Phonon S in this edition; a connected
ElevenLabs account keeps its rows until the voice is changed. The personal edition has a voice of its own; the
edition flag (`edition::PUBLIC`) and the folder check (`Voices::public`) route a saved one to Phonon S here.

## Connect your own speech server

1. Run an OpenAI-compatible speech server on this Mac.
2. In the menu: Personalisation ▸ Voice ▸ Add Provider ▸ Own Voice Engine…, and enter its base address, such as
   `http://localhost:8880/v1`. The key is optional and is kept in the Keychain.
3. Pick one of its voices under Own Engine. Check: clicking a voice plays its sample.

How Heimdall talks to it (`own_voice.rs`):

- Voices come from `GET <base>/audio/voices` when the server lists them, else OpenAI's standard names
  (`STANDARD_VOICES`).
- The model is the first speech-looking id from `GET <base>/models`, else `tts-1`.
- Speech is `POST <base>/audio/speech` with `response_format: "wav"`, one sentence at a time.
- A server that is down, or takes longer than 15 s, makes Phonon S (the fallback voice in this edition,
  `bridge/speech.rs` `streamed_line`) say that sentence instead. A failure after part of the sentence played drops the
  rest of that sentence and goes on with the next.

The choice is stored as `own: true` beside `source` in `speech_voice`, not as a new `source` value, so the previous
build still reads the settings file.

## Samples

Clicking a voice keeps the menu open, moves the check and plays a short sample, "Hi, I'm Heimdall. This is how I
sound." (`PREVIEW_TEXT`).

- Samples are cached at natural speed in `<data>/voice-samples/<voice>.wav` and stretched to the chosen speed when
  they play.
- Phonon S's presets are rendered ahead 90 s after start on a low-priority thread, one at a time, only while
  Heimdall is silent and the user is not talking. A click or a reply ends the render; it is tried again later. Phonon
  S is unloaded afterwards unless it is the voice. Own Engine and ElevenLabs samples render on their first click.
- A sample never starts while the user is talking (or in their pause), while Heimdall is audible or while agent
  speech is queued. It stops when one of those begins or another voice is clicked. It and the Speed slider's sample
  stop each other.
- The echo guard is told the sample's words, so the microphone hearing it through the speakers is not taken for the
  user.

## Submenus open to the left

Every submenu opens to the left, Voice included. AppKit has no setting for the side, so
`crates/ui/src/mac/submenus_left.rs` mirrors each submenu's window frame before it first shows.

## Checks

- `cargo test -p app --bin heimdall speech_voice` and `cargo test -p app --bin heimdall own_voice` (a fake speech
  server).
- `heimdall-ui-demo --speaking <dir>`: offscreen renders of the picker in each state.

## See also

- [personalise.md](personalise.md): name, mark and the rest of Personalisation
- [settings.md](settings.md): `speech_voice`, `voice_speed_percent` and the `settings` tool's `speaking_model` and `voice`
- [editions.md](editions.md): what each edition bundles
