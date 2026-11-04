# Heimdall docs: the index

For agents. One line per doc: the question it answers, then the file. A bare name (`models.md`) is in this folder,
and `docs/models.md` in a doc body means the same file. `crates/…` and `islands/…` paths sit beside `docs/` at the
repository root in a checkout, and inside this folder in the installed app. Each fact has one home: follow a doc's
pointer rather than another doc's summary.

## Yourself and your architecture

- How the code is laid out, which crate owns what, how audio becomes a turn and a reply, how to test → `architecture.md`
- What each live setting does and how a change applies → `settings.md`
- How Heimdall's name, the user's name, the mark and the voice change (the `settings` tool) → `personalise.md`
- What fails, how it is noticed and recovered, what the user sees → `resilience.md`
- What the public and personal editions bundle and run → `editions.md`
- Where Heimdall's files and the home folder are, and how home moves → `home.md`
- What Setup configures on a new Mac: the coding agents (Claude Code, T3, Codex, OpenCode, Grok) set to work without
  asking, the deny list that still guards them, and the public edition's Set Up or Leave as Is → `harness-autonomy.md`
- How bugs are reported → `bugs.md`

## The agent, models and providers

- Which models you run, their windows, the context budget, compaction thresholds, thinking, speed, the background-job
  model and its quota gate → `models.md`
- How Pi is driven: delivery, starts, pins (Pi 1.4.0), compaction's 15-minute silence limit and kill backoff, connect
  flows → `crates/agent/README.md`
- How Sign in with ChatGPT works → `chatgpt.md`
- How Pi's child processes are owned and cleaned up → `harness-process-ownership.md`
- Where the app meets the agent: supervisor, tools, delivery, background jobs → `crates/app/src/bridge/README.md`
- Your MCP tools: the live contract, the families, how to add one → `mcp.md`
- How to connect an account or an MCP server → `connectors.md`

## Modifying yourself

- Where a rule belongs (prompt or skill), and how a skill reaches each harness → `skills.md`
- How to build from source, install and uninstall → `install.md`
- The app crate's code map and replay modes → `crates/app/README.md`
- Offline evaluation runs → `crates/eval/README.md`

## Voice and speech

- The voice picker → `voices.md`
- How transcription biases names, and the language prior → `listen-vocabulary.md`
- Diarization, naming voices, listened-to guests, your own voice → `speakers.md`
- Code maps → `crates/listen/README.md`, `crates/speak/README.md`, `crates/speakers/README.md`,
  `crates/voicebank/README.md`

## The decider

- How spoken commands, barge-in, context routing and Agent Off are decided → `decision-chain.md`
- The d1 model (Decider V6 default, V5 rollback; weights at https://huggingface.co/Toby-NoSkillSon/heimdall-d1-v6),
  its files and tests → `crates/decide/README.md`

## Capture, archive and screen

- How mics and App Audio are captured, and echo cancellation → `capture.md`
- How the screen is recorded and screen commands pick frames → `screen.md`
- How the archive is stored, journalled, compacted and exported → `archive.md`
- How to make a movie of a time window → `archive-clip.md`
- How the timeline is chunked and `timeline_search` finds it → `chunk-index.md`
- The native EmbeddingGemma helper behind semantic search → `crates/archive/embedding/native/README.md`
- Which pipeline file owns a live-loop concern → `crates/app/src/pipeline/README.md`
- Code maps → `crates/capture/README.md`, `crates/screen/README.md`, `crates/archive/README.md`,
  `crates/night/README.md`

## Islands and UI

- What an island can show: `islands_put` fields, parts, plan lists → `islands-capabilities.md`
- How island text must read → `islands/docs/island-style.md`
- When the plan refresher runs and what it updates → `islands/docs/live-updates.md`
- The Floating Islands helper and the `islands` CLI → `islands/README.md`
- The notch island: pill, chat, reader, drawing, layout → `crates/ui/src/island/README.md`
- The menu and windows → `crates/ui/README.md`
- Suggestion cards during the user's conversations → `suggester.md`
- The fast reader → `crates/rsvp/README.md`

## T3 orchestration

- How you drive T3: connection, tasks, briefs, job islands, worktrees, wakes → `t3-orchestration.md`
- The `t3` crate → `crates/t3/README.md`

## Emergency

- Your agent as a T3 thread (relay mode, chosen or on failure): what it gets, the harness order → `claude-relay.md`
- What happens when your agent fails, and what the relay does then → `emergency.md`
