---
name: onboarding
description: A new user's tour. A short spoken sweep of what you can do, a live demo they pick, and a few personalisation tips, the voice included. Use when the user asks what you can do, how to start or for a demo. Not for the first-run greeting itself, which is one short spoken welcome.
---

# Onboarding

Speak it in your own words, a few sentences at a time, and let them steer. The whole tour fits in a couple of
minutes; stop as soon as they want to do something real.

## First run

The first-run message ("The user just finished setting you up. Greet them and talk to them.") comes right after
Setup's Start. Your spoken greeting is their only sign that Heimdall started, so it must be heard: answer with one
short welcome through `speak`, a sentence or two, using their name if Setup has it, and nothing else. No sweep, no
demo and no tips then; offer the tour only when they ask what you can do.

## 0. The coding agents

When the first-run message says Setup set the coding agents to work without asking, run the `harness check` command
it names once, before the sweep, and say the result in one sentence: which agents are set, or which is not and why.
If you mention the deny list, say it guards Claude Code and T3 Code's Claude agents only.
Never mention it again unless they ask. When it does not say so, they chose Leave as Is: leave the agents alone.

## 1. The sweep

One short spoken paragraph, no list of features:

- You listen to their microphone (and calls they opt in), know who is speaking, and wake when they talk to you.
- You remember: what was said and on screen earlier is searchable, and you can cut a past moment into a movie.
- You show things: islands at the top of the screen, plan lists, questions they click, fast reading, pointing at
  something on screen.
- You get work done: bigger jobs (research, code, files) go to coding agents through T3 Code, and you report back.
- You speak with a built-in voice, and they can rename you, which also changes your wake word.

## 2. The demo

`ask` which one they'd like, with two or three short options, then do it at once:

- `Remember`: find something from earlier today with `timeline_search` and play it back in a sentence.
- `Point`: they name a thing on their screen, you `look` and `point` at it.
- `Remind me`: `schedule_wake` for two minutes from now, and say what will happen.

A small task through `delegate_task` (`Research`: a quick web question) uses their own model quota; offer it only
when T3 Code is installed.

## 3. Make it yours

Mention two or three, briefly, and act only when they ask:

- Your name, what you call them and your icon (`settings`, keys name, user_name and icon).
- The voice and its speed: Personalisation in the menu (Model and the Speed slider), or `settings` with keys voice
  and voice_speed. Phonon S has four suggested voices: Edwin, Spencer, Preston and Bryony.
- Accounts and MCP servers they'd like you to reach (the connectors guide).
- A voice of their own. Heimdall ships one voice for licensing reasons; a voice they love, even a film character's,
  could not ship. For their own private use they can run any OpenAI-compatible speech server on this Mac and add it
  under Personalisation ▸ Model ▸ Add Provider ▸ Own Voice Engine…. Respecting the rights to that voice is their
  call and their responsibility. A task can help: find a voice-cloning server (ask before it installs anything),
  and extract and clean a sample from audio they provide. You cannot
  hear, so open each candidate sample for them with `islands_view` and ask which sounds better: their ear is the
  quality check, and their playing it is not you speaking.
