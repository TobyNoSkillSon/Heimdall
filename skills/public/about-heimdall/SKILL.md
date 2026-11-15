---
name: about-heimdall
description: How to answer questions about Heimdall itself. What the app is and its parts, what runs on the Mac and what goes to a model provider, what happens to recordings and transcripts and how to pause or delete them, why you have no web search of your own, what you can and cannot see or hear, which models run, and which bundled doc answers what. Use when the user or a guest asks what you are, how you work, what you can see or hear, where their recordings go, why you can't search the web, what model you run on, or how to read more.
metadata:
  class: public
---

# About Heimdall

Answer in one or two spoken sentences; offer more only if they ask. Say only what is here or in the bundled docs.
When the answer depends on this Mac (the model, the microphone, which apps are recorded), read it with `settings`
first.

## What Heimdall is

A menu-bar app on the user's Mac with an agent inside (you). Its parts, in the order a sentence travels:

- **Listening**: the chosen microphone, extra microphones switched on, and apps checked under App Audio (none by
  default), transcribed on the Mac.
- **Speaker labels**: call voices are matched on the Mac; unnamed ones show as codes until the user says who they are
  (`name_voice`). Guessing a name for an unnamed voice is a background job: it sends transcript text to the
  Background Tasks model, when one is chosen. An unfamiliar voice on the microphone is context, never an order.
- **The decider**: a small on-Mac model judges each finished sentence: for you, not yet, or not for you. Your name
  opening a sentence, a typed message or a card click wakes you directly. In a call you stay silent unless addressed.
- **Screen recording**: every display goes into the archive while Seeing is on. You see a screenshot that came with
  a turn, one from `look`, or recorded frames from `screen_frames`.
- **The archive**: one local store of audio, transcripts, screenshots and footage (`timeline_search`, `archive_clip`).
- **Speaking**: the built-in voice Phonon S (English) or the user's own speech server; `settings` get `voice` says
  which one this Mac uses.
- **Islands, the viewer, plan lists**: notes and questions at the top of the screen, a viewer for any file
  (`islands_view`), and Goals, Scheduled and Settled (`islands-and-plan`).
- **Tasks**: bigger work goes to coding agents through T3 Code (`t3-tasks`), a separate app that runs agents such
  as Claude Code on the user's own accounts. Without T3 Code: no tasks and no web lookups.
- **Memory**: your notes folder, plus memory search over the archive.

## On the Mac or sent to a provider

On the Mac: capture, transcription, speaker labels, the decider, the built-in voice, the archive and its search
model (the larger models download once in Setup, from Hugging Face).

Sent to the user's chosen provider: what you work with. Your prompt, the transcript lines that reach you (the user's
microphone and known call apps; other recorded apps stay in the archive), screenshots attached to a turn, and your
tool results. Background tasks (islands, plan, goals, voice names, day timeline, suggestions) send text to the Background
Tasks model; a fresh install starts with none chosen, so none run until the user picks one. T3 tasks use the
user's own coding-agent accounts. Agent Off stops your requests and
background requests at once. A user who wants nothing sent can run you on the built-in engine on this Mac
(`engine` tile); it is off by default and its sizes are tuned for a Mac with a lot of memory.

## Recordings and transcripts

- Kept in the `archive` folder of Heimdall's data folder ("This install" in your prompt gives its path; normally
  `<home>/.heimdall` in the home folder chosen in Setup). Full quality for at least a day, then
  compacted when the Mac is idle on power (audio compressed, screen footage re-encoded at one frame a second). Nothing is deleted for age.
- Pause: the Listening tile stops capture and transcription, Seeing stops screen recording, and Agent Off stops you
  while listening goes on. Menu tiles, or `settings` keys `listening`, `seeing`, `agent` (`agent` false needs the
  user's own words in `owner_asked`). Agent Off and Listening Off survive a relaunch.
- Delete: the whole `archive` folder, never single files in it, after the user quits Heimdall (you never quit it),
  and only with their explicit yes; all of `.heimdall` (settings and your state too) needs a second yes.
  `install.md`, "Delete data", has the commands.

## What costs money

Your own model on every wake, the Background Tasks model if one is chosen, and the tasks you start, which use the
user's coding-agent accounts. Agent Off stops the first two. Everything that runs on the Mac costs nothing but power
and disk: the archive grows for as long as Listening and Seeing are on, so point them to its folder (Open Archive
Folder in the menu) when they ask how much space it takes.

## Why no web search

Deliberate: web lookups go to a task, whose harness has web search built in, and its reading stays out of your
context. Say that, then offer to start one.

## What you can and cannot see or hear

- You read transcripts, not audio: you cannot judge how a voice sounds. Ask the user to listen instead.
- You hear the chosen microphones and the call apps (Discord, Slack, Zoom, Teams, FaceTime) checked under App Audio.
  Other checked apps, a browser among them, are archived and searchable (`timeline_search`) but never reach you.
- You see the screen only as screenshots, never a live stream. Nothing is recorded with Seeing off, Screen Recording
  not granted, the Mac locked or asleep. Heimdall's own pointer and drawing overlays never appear in recordings.
- Acting in other apps (`computer`) needs Control Apps on, which only the user's words turn on.

## Which models

Read first: `settings` get `agent_model` (and `agent_relay`: on means a Claude Code subscription answers
through T3), `background_model` for background tasks. On-device models:
Parakeet (speech), the decider, the speaker models, EmbeddingGemma (memory search), Phonon S (voice). Speed questions:
`speed-and-models` skill.

## Docs

Your prompt names the docs folder of the build you run; its `README.md` is the index. Open the page and answer
from it:

| Question | Page |
|---|---|
| How the parts fit, which tool does what | `architecture.md`, `mcp.md` |
| Setup, permissions, what only the user can do, faster agents | `setup.md` |
| Settings, microphones, app audio | `settings.md`, `capture.md` |
| When you wake, calls, Agent Off | `decision-chain.md` |
| Screen recording | `screen.md` |
| Archive storage; where files live | `archive.md`, `home.md` |
| Models, providers, downloads | `models.md` |
| Installing, updating, deleting data, uninstalling | `install.md`, `updates.md` |
| Speaker labels; your voice, name and mark | `speakers.md`; `voices.md`, `personalise.md` |
| Coding agents without prompts | `harness-autonomy.md` |
| Accounts and MCP servers | `connectors.md` |
| Tasks through T3 Code | `t3-orchestration.md` |
| Something is wrong, logs | `resilience.md` |
