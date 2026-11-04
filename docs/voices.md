# Speaking voices

Beta: Phonon S speaks English only, and replies held during a call can be dropped.

What the Speaking ▸ Model picker offers and how each voice is chosen (23 Oct 2026; Model ▸ since 26 Oct). Code:
`crates/app/src/speech_voice.rs` (`VoiceChoice`, `Voices`, `Route`), `crates/ui/src/mac/speaking_menu.rs`.

The public voice is Phonon S, English only. Anyone who wants their own voice model adds a local provider through a
local API, as with Own engine. The picker has two things:

- **Phonon S** (`crates/speak/src/phonon.rs`, Gradium's 31M English model, 37 presets) shows four suggested voices
  only (`ui::speech_voice::SUGGESTED_PRESETS`). They were chosen from the presets' catalogue descriptions, not by ear:
  - Edwin (UK man): "efficient and firm, sincere". The default, `speak::phonon::DEFAULT_PRESET`.
  - Spencer (UK man): "slow, patient delivery".
  - Preston (US man): "trim, focused … steady, low-set core".
  - Bryony (UK woman): "full, rich … softly relaxed".

  A saved preset outside these stays listed so the choice stays visible.
- **Own Voice Engine…**: any OpenAI-compatible speech server on this Mac (`crates/app/src/own_voice.rs`). Its panel
  carries one line of help: "Run any OpenAI-compatible speech server on this Mac and add its address here, such as
  http://localhost:8880/v1." The key is optional.
  - Voices come from `GET <base>/audio/voices` when the server lists them, else OpenAI's standard names.
  - The model is the first speech-looking id from `GET <base>/models`, else `tts-1`.
  - Speech is `POST <base>/audio/speech` with `response_format: "wav"`, one sentence at a time.
  - A server that is down, or hangs longer than 15 s, makes the lighter voice say that sentence instead.

Supertonic and ElevenLabs are no longer offered. A saved Supertonic, or a connected ElevenLabs, keeps its rows until
changed.

The saved Own Voice Engine choice is a flag beside `source` (`own: true`), not a new `source` value, so the previous
build still reads the settings file.

## Samples

Clicking a voice keeps the menu open, moves the check and plays a short sample: "Hi, I'm Heimdall. This is how I
sound." (`crates/app/src/bridge/voice_preview.rs`).

- Samples are cached at natural speed in `<data>/voice-samples/<voice>.wav` and stretched to the user's speed when they
  play.
- Phonon S's suggested voices (and the saved preset) are rendered ahead at idle, 90 s after start, at low priority. The pass yields to clicks and
  replies, and Phonon S is unloaded afterwards unless it is the voice. ElevenLabs and Own Engine samples are rendered on their first click.
- A sample never starts while the user is talking (or pausing), while Heimdall is audible or agent speech is
  queued. It stops when one of those begins or another voice is clicked. It also stops the Speed slider's sample,
  and the Speed sample stops it.
- The echo guard is told the sample's words, so the microphone hearing it through the speakers is not taken for
  the user.

## Voice opens to the left

Every submenu opens to the left, Voice included. AppKit has no setting for the side, so
`crates/ui/src/mac/submenus_left.rs` mirrors each submenu's window frame before it first shows; see
`lab/menus-left/README.md`.
