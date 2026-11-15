# Personalise: name, user name, mark, voice

The assistant's name (which is also its wake word), what it calls the user, the mark it shows in the menu bar and
notch, and its voice. All of it changes live.

Read this when: you want to rename the assistant, change the user's name or the mark, find out why another name wakes
less reliably, or change how a rename reaches the agent.
Code: `crates/ui/src/identity.rs` (`Identity`, `Icon`), `crates/app/src/identity.rs` (tool arguments, SVG marks),
`crates/decide/src/names.rs` (wake spotting and decider names), `crates/agent/src/identity.rs` (the agent's prompt),
`crates/ui/src/mac/personalise.rs` (Name & Icon ▸).

## Where it lives

One setting in `<data>/settings.json`, written only when it differs from Heimdall's own:

```json
"identity": {"name": "Ivy", "user_name": "Evelyn", "icon": {"kind": "symbol", "name": "leaf"}}
```

- `name`: the assistant's name and its wake word ("Ivy, hello"). There is no separate wake-word field; an older file's
  `wake_words` loads and is dropped at the next save.
- `user_name`: what the assistant calls the user.
- `icon`: missing (the helmet), `{"kind": "symbol", "name": "…"}` (any SF Symbol), or
  `{"kind": "image", "path": "/abs/mark.svg"}` (PNG, PDF or SVG, drawn as a one-colour template).
- A hand-edited identity that does not validate is ignored with one log line and Heimdall's own takes its place; the
  rest of the file still loads.
- The voice is the separate `speech_voice` setting ([voices.md](voices.md)).

The app, its bundle and its data folder keep the name Heimdall whatever the identity says.

## Change it

- In the menu: the Personalisation section holds Voice ▸, Speed, Wake Sound and Name & Icon ▸ (the mark grid, Name,
  Your Name).
- From the agent: the `settings` tool's keys `name`, `user_name`, `icon`, `speaking_model`, `voice`, `voice_speed` and
  `wake_sound` ([settings.md](settings.md) has the tool). An `icon` value starting with `<` is SVG source the agent drew:
  plain filled shapes in one colour on a square `viewBox`, no scripts, images or outside links. It is saved as
  `<data>/identity/mark-<hash>.svg`, and only the mark in use is kept.
- By hand: edit `identity` in `settings.json` ([settings.md](settings.md) has the procedure).

Check: the menu's Quit row and tooltip name the new assistant, and the mark in the menu bar changes.

## How a change applies

No change needs a Heimdall restart.

| Part | Reads | When |
|---|---|---|
| Menu (Quit, tooltip, Personalise), island text, the mark in the bar and notch | `ui::identity` | at once |
| Wake spotter and decider context (`decide::names`) | name, user name | the next decision |
| Speech recognizer's name bias (`listen::vocabulary`) | name | a speech-model reload the app starts |
| Agent prompt (`agent::identity`, filled at each Pi start) | name, user name | a Pi reload after the running turn; the conversation is kept |
| Task preamble (`prompt/worker.md`) | name, user name | the next start |
| Voice | `speech_voice` | the next sentence |

A new name or user name makes the `settings` tool answer "end the turn now"; Pi reloads after the turn.

No prompt, tool description or notice names a person. They say `{user}`, which `agent::identity::fill` replaces with the
user name where the text is built ("the user" while no name is chosen). Text the user wrote and tool output are never
rewritten. `crates/agent/tests/owner_name.rs` fails if a prompt or a message the agent reads carries a literal name.

## Transcript labels stay fixed

The archive and the decider's history label the user's lines with `decide::names::USER_LABEL` and the assistant's with
`ASSISTANT_LABEL`, whatever names are chosen. Changing those labels would rewrite the archive and the training data.

- The decider reads the labels as the configured names, after a header naming both (`decide::names`).
- Everything that reaches the agent (deliveries, notices, tool results, background prompts) passes
  `agent::identity::for_agent`. In this edition it always rewords: the user's label becomes the user's name, or `User`
  while no name is chosen (`PUBLIC_USER`), and the prompt's note tells the agent how transcripts label each side.

## Wake spotting for other names

- Heimdall keeps its observed misspellings and edit budgets exactly (`names_parity` test).
- Any other name wakes when the exact name appears anywhere in a line. At a sentence start, a one-word name also wakes
  within an edit distance of 2 (7 or more letters), 1 (5 or 6) or 0 (shorter), so "Ivy" never wakes on "ivory"
  (`decide::send`).
- Short names miss more speech-recognition variants.
- The decider is sent the names header on every request. It was trained with names swapped, but how well it
  generalises past the default names is unmeasured: expect more waits and misses with other names.

## Checks

- `cargo test -p ui --lib identity`, `cargo test -p decide --test names_parity`, `cargo test -p agent --lib identity`,
  `cargo test -p agent --test mcp_v2`, `cargo test -p app --bin heimdall identity`,
  `cargo test -p app --bin heimdall settings_tool`.
- `heimdall-ui-demo --personalise <dir>`: offscreen renders and scenario checks of the menu, the submenu and the marks.

## See also

- [settings.md](settings.md): the `settings` tool and every other setting
- [voices.md](voices.md): the voice picker
- [decision-chain.md](decision-chain.md): the wake spotter and the decider
- [listen-vocabulary.md](listen-vocabulary.md): the recognizer's name bias
