# Heimdall docs

Heimdall is a macOS menu-bar voice assistant: it listens on the Mac, decides on the Mac when speech is meant for it,
hands those turns to an agent, and answers by voice, on screen and through the agent's tools. These pages are written
for the people and agents who set it up, use it and change it.

Read this when: you do not know which page holds what you need.

A bare name (`models.md`) is a page in this folder. Paths that start with `crates/`, `islands/`, `scripts/` or
`skills/` are from the repository root; in the installed app the same files sit inside this folder. Each fact has one
page; follow a page's link rather than restating it.

## Start here

- You want to know what Heimdall is and how the pieces fit → [architecture.md](architecture.md)
- You want the short version of how it is built → [condensed-technical-explanation.md](condensed-technical-explanation.md)
- You installed the app and want it working: permissions, downloads, the agent, the first spoken answer →
  [setup.md](setup.md)
- You want the coding agents (Claude Code, T3, Codex, OpenCode, Grok) to work without asking, and the deny list that
  still guards them → [harness-autonomy.md](harness-autonomy.md)
- You want to know where Heimdall keeps its files, or move its home folder → [home.md](home.md)
- You want to build the app from source, install it, roll back or uninstall → [install.md](install.md)
- You want to update, or understand the in-app updater → [updates.md](updates.md)
- You want to know what the public and personal editions contain → [editions.md](editions.md)
- Something is wrong: a symptom, its cause, the fix, and where the logs are → [resilience.md](resilience.md)
- You want to change the code: build, test, where code goes, the docs standard → [contributing.md](contributing.md)

## The agent and its tools

- Which models and providers the agent runs, budgets, compaction, first-run downloads → [models.md](models.md)
- Sign in with a ChatGPT or Codex subscription → [chatgpt.md](chatgpt.md)
- Connect an account or an MCP server → [connectors.md](connectors.md)
- The agent's own MCP tools: families, the contract, how to add one → [mcp.md](mcp.md)
- Where a rule belongs (prompt or skill), and how a skill reaches each harness → [skills.md](skills.md)
- The one call path every background job takes, the jobs, adding one → [background-call.md](background-call.md)
- Driving T3 Code: tasks, briefs, job islands, worktrees, wakes → [t3-orchestration.md](t3-orchestration.md)
- Crewmates: permanent workers Heimdall owns, their tools, survival and the `heimdall --crew` command → [crew.md](crew.md)
- The Agent window and the island's agent card → [agent-window.md](agent-window.md)
- The agent as a T3 thread (relay mode) → [claude-relay.md](claude-relay.md)
- What happens when the agent fails, and what the relay does then → [emergency.md](emergency.md)
- How the Pi process is driven, owned and cleaned up → [crates/agent/README.md](../crates/agent/README.md)
- Where the app meets the agent → [crates/app/src/bridge/README.md](../crates/app/src/bridge/README.md)

## Settings and personal touches

- Every setting, its default, how a change applies, and the `settings` tool → [settings.md](settings.md)
- The assistant's name, the user's name, the mark → [personalise.md](personalise.md)
- The voice picker → [voices.md](voices.md)

## Listening and deciding

- When a sentence wakes the agent, barge-in, spoken commands, Agent Off → [decision-chain.md](decision-chain.md)
- Who is speaking: diarization, naming voices, call guests → [speakers.md](speakers.md)
- Name biasing and the language prior in transcription → [listen-vocabulary.md](listen-vocabulary.md)
- Dictation with Vella that must not wake Heimdall → [vella.md](vella.md)
- Suggestion cards during conversations → [suggester.md](suggester.md)
- The local inference engine → [inference-engine.md](inference-engine.md)
- Code maps → [crates/listen/README.md](../crates/listen/README.md),
  [crates/speakers/README.md](../crates/speakers/README.md), [crates/decide/README.md](../crates/decide/README.md),
  [crates/speak/README.md](../crates/speak/README.md),
  [crates/app/src/pipeline/README.md](../crates/app/src/pipeline/README.md)

## Capture, archive and screen

- Microphones, app audio and the echo guard → [capture.md](capture.md)
- Screen recording and the screen commands → [screen.md](screen.md)
- The archive: storage, journal, cleanup, export → [archive.md](archive.md)
- A movie of a time window → [archive-clip.md](archive-clip.md)
- Timeline search and the reranker → [chunk-index.md](chunk-index.md)
- Code maps → [crates/capture/README.md](../crates/capture/README.md),
  [crates/screen/README.md](../crates/screen/README.md), [crates/archive/README.md](../crates/archive/README.md),
  [crates/night/README.md](../crates/night/README.md)

## Islands and windows

- What an island can show: `islands_put` fields and parts → [islands-capabilities.md](islands-capabilities.md)
- Goals, Scheduled, Settled, job islands and reports as one system → [one-system.md](one-system.md)
- The Goals list and its shadow job → [goals.md](goals.md)
- The floating viewer: open a file, page size, pinch, choices back → [viewer.md](viewer.md)
- How island text must read → [islands/docs/island-style.md](../islands/docs/island-style.md)
- HTML drawn inside a job island → [islands/docs/model-html.md](../islands/docs/model-html.md)
- The Floating Islands helper and the `islands` CLI → [islands/README.md](../islands/README.md)
- The menu, windows and the notch island → [crates/ui/README.md](../crates/ui/README.md),
  [crates/ui/src/island/README.md](../crates/ui/src/island/README.md)
