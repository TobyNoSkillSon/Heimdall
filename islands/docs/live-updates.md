# How islands stay up to date (agreed with)

As built (30 Oct, background-jobs redesign; the job model: `docs/background-jobs.md`):
- Thread-bound job islands are code-driven: after every T3 pass (30 s) the binding (`crates/app/src/islands/binding.rs`)
  reads the thread's new activity items, its sub-agents and a live TrainingLab run into a reducer
  (`islands/stages.rs`) and writes the stage, the clock (`started`), progress and the band (helper rows and the stage
  ribbon, a native `band` part) with no model. A new run starts over.
- The island agent (`crates/app/src/island_agent.rs`) is woken by those signals (a stage change, a run end, a new
  picture, a finished helper, an eval, new loss points, a 30 min heartbeat) and makes the island rich: the now line,
  the moment's graphic (loss curve, eval bars, the latest picture, a number with its limit), the icon, the title, the
  helpers' short names and the open target. One stateless call per island, its instructions the island-design
  round-3 prompt.
- The plan refresher keeps the three lists and the job islands that follow no thread (an iPad sale). The user's own words
  or a `refresh_plan` request run a pass within 5 minutes (at most 12 an hour); other changes (notices, Heimdall's
  lines, islands) wait 15 minutes, 30 once the user has been quiet for an hour. It reads no thread; each
  thread island is one read-only line in its prompt. An item it added within the hour leaves only when two passes in
  a row drop it.

The original agreement, kept for its intent (the "shadow" is now the code plus the island agent):

- The agent owns meaning: it creates, names, summarises and removes islands. It handles static ones itself (an image, a file to open; the plan lists keep themselves).
- Islands for running work bind to a T3 thread (about 90% of cases). The state (running, waiting on the user, done, failed) follows the thread directly, cheaply and immediately. Every task Heimdall starts (`delegate_task`, or a launch or fork through `t3_call`) gets its job island at once, before the call returns: running, docked right, out of the notch, titled from the task's title (`task-<title>` id, named in the tool's answer so the agent can add details); a thread that already has an island keeps it.
- A tiny cheap model (the "shadow") refreshes the text: about every 5 minutes by default (fixed, not agent-chosen), it reads what happened in the bound thread since its last look, then updates the stage (1–2 words), the summary and the steps (e.g. "hit a bug, fixing it"). It doesn't watch every tool call.
- No integrations with arbitrary apps. Maybe later: one generic "watch a file or command" binding for downloads and builds.
- When something needs the user, Heimdall wakes the agent, which talks to them; the shadow never speaks.

## Additions
- The plan lists get their own background runners too: Goals' shadow follows the conversation and moves goals as their tasks finish, raising and clearing the asks (docs/goals.md); the plan refresher keeps Scheduled and Settled. There is no to-do list.
- Every update should show visibly: a brief, subtle highlight or pulse on the island or item that changed, then still again (no idle animation).

## Whole islands, not text
- Text is a poor way to transmit information: the refresher (Claude Haiku 5.5, every 5 minutes) builds each job island whole: a timeline of the current phase's steps (done / now / todo), progress where it is countable, the latest picture the work produced as a thumbnail, a compact open target, and one short line of text.
- Only the current phase is shown: once a phase ends (a choice was made, the rebuild started), its steps go and the next phase's steps are laid out.
- It reads the bound thread's recent activity (tool calls, files written) and its sub-agents, not only the last message, so the steps are the real ones. Islands without a thread (iPad sale, iPhone, tests) are rebuilt from the conversation.
- An island is never "done" while its thread, a sub-agent or background work still runs; a picture is shown only when it is a real image file the work produced.
