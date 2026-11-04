# Personalise: name, user name, mark, voice

"Heimdall is just the default for me. Some people may want a different name, a different icon, a
different wake-up call … they want a female voice and the name Ivy … you should know how to do it."
No Wake Words field; the wake is the name ("Heimdall, hello", "Ivy, hello").

## Where it lives

One setting in `<data>/settings.json`, written only when it is not Heimdall's own:

```json
"identity": {"name": "Ivy", "user_name": "Evelyn", "icon": {"kind": "symbol", "name": "leaf"}}
```

- The name is the wake word. The spotter adds the greetings ("Hey Ivy", "Ivy, hello") and, for Heimdall, its observed
  misspellings. A file from before 16:43 with `wake_words` still loads; the field is ignored and dropped at the next save.
- `icon`: missing (the helmet), `{"kind":"symbol","name":"…"}` (any SF Symbol), or `{"kind":"image","path":"/abs/mark.svg"}`
  (PNG, PDF or SVG, drawn as a one-colour template).
- The voice is the existing `speech_voice` setting.
- In the menu: the Personalisation section (in place of Speaking): the voice caption, Model ▸, Speed, Wake Sound, then
  Name & Icon ▸ (the mark grid, Name, Your Name).

The app, the bundle and the binaries keep the name Heimdall (`Heimdall.app`, `~/Library/Application Support/Heimdall`).

## How a change applies

Everything is live. No change needs a Heimdall restart.

| Part | Reads | When |
|---|---|---|
| Menu (Quit, tooltip, Personalise), island text, the mark in the bar and notch | `ui::identity` | at once |
| Wake spotter and decider context (`decide::names`) | name, user name | the next decision |
| Speech recognizer's name bias (`listen::vocabulary`) | name | a speech-model reload the app starts |
| Agent prompt (`agent::identity`, filled at each Pi start) | name, user name | a Pi reload after the running turn; the conversation is kept |
| Voice | `speech_voice` | the next sentence |

Transcript labels stay internal: the user's lines carry the user's configured name and the assistant's are `Heimdall` in the archive, the
agent's transcripts and the decider's history. The decider reads them as the configured names (`decide::names`), and
the agent's prompt says they are labels. Changing those labels would rewrite the archive and the training data.

## For the agent: the `settings` tool

The agent changes these with `settings` (crates/app/src/settings_tool.rs), the tool for every menu setting:
`settings {"action":"set","key":"name","value":"Ivy"}`, keys `name`, `user_name`, `icon` (`"helmet"`, an SF Symbol, an
absolute PNG/PDF/SVG path, or SVG source it drew: one colour, a square `viewBox`, solid filled shapes, saved under
`<data>/identity/`), `speaking_model`, `voice`, `voice_speed`, `wake_sound`, `ack_delay` (seconds: `2`, `2.5`). `personalise` stays as the older name and
takes its old arguments (`wake_words` is ignored).

A new name or user name answers "end the turn now": Pi reloads after the turn with the new prompt. Never quit, kill or
restart Heimdall for any of it.

## The settings tool

"the agent can change any Heimdall setting … that's kind of the whole point".

- `list`: every setting with its value, by menu section. `get key`: the value and the choices. `set key value`.
- Keys (`SETTINGS_KEYS` in `crates/agent/src/mcp_contract.rs`, 27 on 27 Oct): tiles (`listening`, `seeing`, `speaking`, `agent`); Listening (`microphone`, `extra_microphone`,
  `app_audio`); Seeing (`recording_resolution`, `recording_fps`, `islands_on_streams`); Agent (`agent_model`,
  `agent_thinking`, `agent_speed` (standard or fast: the priority tier on the Codex subscription or the OpenAI API;
  only when the user asks), `agent_context`); Personalisation (above); Background Tasks (`background_model`,
  `background_task`); `show_islands`; `launch_at_login`.
- `item` names the microphone, app or task; they go by the names the menu shows ("Ego" finds "Ego Browser"; an
  ambiguous part is refused with the matches). An app is listed once it has played sound this session or is recorded.
- A change is planned on the pipeline as the menu's own `UiAction`s and sent through the menu's callback
  (`ui_action_callback`), so the latches, the live loop, `apply_menu_settings`, the save and the menu's redraw are a
  click's. The tool then reads the value back for up to about three seconds and answers `key: before → after`.
- `agent` false is refused unless `toby_asked` quotes
  at least three of the user's words found in one of their last 30 lines (spoken or typed); an allowed one is logged
  (`settings_tool` evidence record and the app log).
- Launch at Login goes through the app's own `SMAppService` path, nothing else.
- Not in the tool: provider sign-ins and API keys (secrets stay with the user), adding or removing API models, the Anthropic
  credit, speakers (the agent has `name_voice` and `listen_to`), shortcuts (actions, not settings), and Quit.
  There is no speaker-device setting: Heimdall plays to the Mac's default output.

## Wake spotting for other names

Heimdall keeps its observed misspellings and edit budgets exactly (the names parity test). Other names: the exact name
anywhere in a line wakes; at a sentence start a one-word name also wakes within an edit budget of 2 (7+
letters), 1 (5–6) or 0 (shorter), so "Ivy" never wakes on "ivory". Short names miss more ASR variants. The decider model was trained mostly on the name Heimdall; with other names it sees the names header
and swapped labels it has not learned well, so expect more waits and misses.

## Checks

- `cargo test -p ui --lib identity`, `-p decide --test names_parity`, `-p agent --lib identity`,
  `-p agent --test mcp_v2`, `-p app --bin heimdall identity`, `-p app --bin heimdall settings_tool`.
- `heimdall-ui-demo --personalise <dir>`: renders and scenario checks of the menu, the submenu and the marks.
