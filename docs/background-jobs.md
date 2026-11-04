# Background jobs

There is no single long-lived background thread. Each background job is its own stateless call that
carries its whole input (one exception since 31 Oct, Goals: a rolling, cached context, `docs/goals.md`), through one shared call path whose backend is selectable (Background Tasks ▸ Model: a T3
Claude Code instance on the subscription, Codex or the ChatGPT sign-in, an API key).
`crate::background_call::ask` is that path (`docs/background-call.md`); each job names its log and ledger label.

| Job | Label | Wakes on | Input | Writes |
|---|---|---|---|---|
| Island agent (`island_agent.rs`) | island agent | code signals of one bound job island: a stage change, a run end, a new picture, a finished helper, a new eval, training loss (5 new points, once per 10 min), a 30 min heartbeat when anything changed; 2 min between calls per island, 90 s coalescing, one call in flight | that island's `<signals>` (task, brief, recent lines, helpers, counters, latest picture and its age, question, metrics, previous island) | the island's now line, graphic parts, icon, title, helper names, open target, and its `sent` line once when it has none |
| Plan lists (`plan_refresher.rs`) | plan refresher | the user's words or `refresh_plan` (5 min lane); notices, islands (15/30 min lanes) | the conversation, Scheduled and Settled, job islands without a thread, thread islands as one read-only line each | proposals for Scheduled (promises with a real time, for the user's yes); job islands without a thread (an iPad sale) |
| Goals (`goals/`) | goals | the user's words (60 s after they go quiet, 3 min at most), a finished task (20 s); 45 s between calls | its own segment: the turns so far, the new stream lines, the Goals now, the clock (`docs/goals.md`) | ops on `goals.json`: tasks done (Settled, with the proof read from their line), new goals and steps proposed for the user's yes, goals finished or paused, asks answered |
| Suggestions (`suggester.rs`) | suggester | the suggest key | the moment's conversation | the suggestion card |
| Voice names, Day timeline | voice namer, day timeline | their own clocks | their own inputs | their own islands |

Each job has its own toggle in Background Tasks (`ui::BackgroundTask`: Islands, Plan Lists, Goals, Voice Names, Day
Timeline, Suggestions).
The Islands task resolves its own model (`background_model::task_endpoint(Task::Islands)`, Claude Haiku 5.5 through T3
by default); the others use the shared model. Emergency mode's pause stops every job (`background_gate`).

## Code first

A thread-bound job island is code-driven (`islands/stages.rs`, `islands/binding.rs`): after every T3 pass (30 s) the
binding reads the thread's new activity items, its sub-agents and the TrainingLab run its commands point at, and writes
the stage, the clock, progress and the band (helper rows and the stage ribbon) with no model. Stage words: Reading,
Researching, Writing, Drafting, Editing, Building, Testing, Checking, Committing, Downloading, Labelling, Training,
Sub-agents, Reviewing, Your answer, Paused, Done, Failed. Training comes only from a live run; a background labelling
run or download holds its stage while the agent only watches it. The island agent is woken by these signals to make
the island rich; it never writes state, stage or the band.

## Adding a job

A new job adds a `ui::BackgroundTask`, its own stateless call with its whole input (never a shared session that grows;
Goals' own rolling segment is the one exception, for its cache),
its log label, and its trigger. It never reads what another job owns.
