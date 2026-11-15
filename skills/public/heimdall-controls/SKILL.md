---
name: heimdall-controls
description: Changing Heimdall's own controls with the `settings` tool, and typing dictated text into the field the user selected. Microphones, app audio, the tiles, voice and speed, your name and the user's, models and providers, islands, what needs the user's own click, and what a refusal means. Use when the user names a microphone, device, voice or Heimdall switch in a few words ("switch to the desk mic", "talk faster", "stop looking", "call yourself Ivy"), asks what a setting is, and when they say "write…" or "type…" with the caret in a text field.
metadata:
  class: public
---

# Heimdall controls

A few words naming a device or a switch are a complete instruction. Do it at once with `settings` (action `set`), no
question, then say it is done in one short line. The tool reads the value back: on `key: before → after` say it is
done; on "sent but not yet shown" say it was sent, never that it is done.

## Find the key and the value

- `settings` action `list`: every key with its value, by menu section. `get` with a key: its value, what `set`
  takes, and the current choices.
- Devices, apps and voices go by the names the tool lists, never by names you assume. Match what the user said to
  the closest choice; speech recognition mangles device names, so go by sound and context.
- If the device they named is not in the list (not connected), say that in one line and pick no other.

| They say | Key | Value |
|---|---|---|
| "switch to the desk mic", "use the Mac mic" | `microphone` | a listed microphone's name, or `built-in` |
| "also use the other mic" | `extra_microphone` | item: the microphone; `true` or `false` |
| "record Zoom too", "stop recording Music" | `app_audio` | item: the app; `true` or `false` |
| "stop listening", "listen again" | `listening` | `false` / `true` |
| "stop looking", "stop recording the screen" | `seeing` | `false` / `true` |
| "be quiet", "stop talking out loud" | `speaking` | `false` (replies still show on the island) |
| "talk faster", "slower" | `voice_speed` | percent of natural speed, 80 to 160 |
| "use the Bryony voice" | `voice` | a voice the current speaking model lists |
| "call yourself Ivy", "call me Sam" | `name`, `user_name` | text, at most 32 characters |
| "no beep when I talk to you" | `wake_sound` | `false` |
| "wait a moment before you say 'on it'" | `ack_delay` | seconds, 0 to 10 |
| "hide the islands", "islands on the left screen" | `show_islands`, `islands_display` | `false`; a display's name, or `follow` |

"Listen to Sam" about a call guest is `listen_to`, not a setting. Naming a voice is `name_voice`.

## Voice and name

- Phonon S, the built-in voice, suggests Edwin, Spencer, Preston and Bryony: `get voice` lists those four, and any
  other Phonon S preset is accepted by its exact name. `speaking_model` is `phonon` or `own_engine` (the user's own speech server, added under Personalisation ▸
  Voice ▸ Add Provider ▸ Own Voice Engine…).
- How you sound is the user's setting: change it only when they ask.
- Your `name` is the name you answer to; there is no separate wake-word setting. Short names wake less reliably; say so
  once if they pick one.
- `personalise` is the older tool for the same things; use `settings`.

## Models and providers

- `agent_model`: `local`, `local/<engine model>`, or an added model's name or id; its provider must be connected.
  It and `agent_context` restart you (the tool says how). `agent_thinking` takes a level the model lists.
- `agent_relay` on: a Claude Code subscription answers as you through T3; `agent_relay_model` and
  `agent_relay_account` take what T3 lists.
- `background_model` and `background_task` (item `islands`, `plan`, `goals`, `voice_names`, `day_timeline`, `suggestions`): the
  model background jobs use, and which jobs run.
- Money: `agent_speed` `fast` (priority tier, billed higher on an API key) and `background_model` item `paid_fallback`
  `true` only when the user asks for exactly that.
- Which model to pick and why: the `models` and `speed-and-models` skills.

## What needs the user

- Not in the tool, so the user does it in the menu or System Settings: provider sign-ins and API keys, adding or
  removing models, macOS permissions (Microphone, Screen Recording, Accessibility), keyboard shortcuts, Quit. There is
  no speaker-device setting: Heimdall plays to the Mac's default output.
- Turning yourself off (`agent` false, or `engine` false while you run on it) needs `owner_asked` quoting at least
  three words of one of their recent lines.
- `computer_control` (Control Apps) turns on only with `owner_asked` quoting a recent line of theirs that asks for
  it; the tool refuses anything else, and a file edit cannot turn it on. When a request needs it and it is off, say
  so in one line: they switch it on in the menu (Agent ▸ Control Apps) or tell you to, in any words of their own.
  Never ask them to recite a set phrase. Off always works.
- No setting needs a restart. Never quit or restart Heimdall.

## Refusals

A refused change starts with `NOT_CHANGED:` and says why: say that reason in one line, never that it worked. An
ambiguous name lists its matches: pick the one they meant, or ask. `COMPUTER_OFF` means Control Apps is
off: tell them, and that they can switch it on in the menu (Agent ▸ Control Apps).

## "Write this": dictation into the selected field

The user puts the caret in a field (a chat box, a form, a document) and says what to write. Do it in one call, no
questions, no task:

1. Clean their words into the text they meant: drop the "write…" lead-in, false starts, repeats and filler; fix what
   speech recognition mangled; keep their meaning, tone, language and length. A command stays a command, exactly.
   Rephrase only when they ask ("make it polite").
2. `computer` action `type`, `app` the frontmost app (the turn's screenshot or the transcript's app tag names it),
   `text` the cleaned text, no `title`: it types at the caret of the focused field and never presses Return.
3. Typing needs Control Apps on and Accessibility granted; on COMPUTER_OFF, say Control Apps is off in Heimdall's
   menu. Otherwise say one short line: "Written." If the tool misses (no focused field, a refused app such as a terminal), say that.

Sending is separate: only when they say "and send it" or "press Send", press the button with `owner_asked` quoting
their words that name it. Needs Control Apps on.
