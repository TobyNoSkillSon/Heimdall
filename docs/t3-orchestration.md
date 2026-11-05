# T3 orchestration for Heimdall

Beta: needs T3 Code 0.0.46 or newer with Orchestrator V2, and is tested only against T3 nightlies.

How Heimdall drives T3 Code: the live contract of its T3 tools, how it follows tasks and when it wakes the agent,
then the design notes from the research that still explain the choices. Code: `crates/t3/` (map in
`crates/t3/README.md`); the agent's tool descriptions are in `crates/agent/src/mcp_contract.rs`. Integration first
built against T3 Code Nightly 0.1.1. Where research and code differ, the
code wins.

Heimdall's Pi is an outside client of T3, not a T3 provider session: it authenticates as its own MCP client, passes
explicit project and thread ids, and never borrows a running worker's token or reads T3's database.

## Connection

With T3 Code running and no usable credential (none, expiring within three days, or for another environment),
Heimdall runs T3's own CLI, `~/.t3/bin/t3 auth pairing create` (five-minute one-time code; scopes
orchestration:read/operate, settings:write, source-control:write), submits it to `POST /oauth/mcp/decision` as the
sign-in page's "pairing code" answer with access `full-access`, and exchanges the code at `/oauth/mcp/token` for the
30-day bearer kept in the Keychain. No browser, no click; a failed pairing waits 10 minutes before the next try. The
session shows as "Heimdall" in T3 Settings → Connections. A 401 (revoked there) is the user's decision: Heimdall stops
and shows "Connect T3 · click"; the click pairs the same way, the browser approval being only the fallback.

The origin comes from `~/.t3/userdata/server-runtime.json` (a hint, not proof the server is live) and is verified
with `GET /.well-known/t3/environment` and its `environmentId`. MCP is Streamable HTTP at `<origin>/mcp`, protocol
`2025-06-18`; T3 has no refresh grant, so expiry or revocation means pairing again. A T3 restart re-initializes the
MCP session with the same bearer. A closed T3 shows "T3 Code closed" on the island once per outage and "T3
connected" when it is back, never a sign-in card.

## The agent's T3 tools

"maximum one tool call … responses from them should be easy"; "we should literally take
the same tool calls and whatever that agent in T3 code has access to". The tools carry the names T3 gives its own
thread agents, one call per action, plain compact answers (`crates/t3/src/text.rs`, `threads.rs`). T3's own
`delegate_task`, `task_status` and `task_cancel` need a calling thread inside T3 (the catalog marks them "Needs an
agent running inside a T3 thread"), so Heimdall implements the trio for its outside client with T3's meaning.

| Tool | Input | Result |
| --- | --- | --- |
| `delegate_task` | `task` (the whole brief), `title`, `target` {`model`, optional `providerInstanceId` (found from the model), `options` such as `{"effort":"high"}`}; optional absolute `path` (default: the workspace root), `isolation` (`shared`/`worktree`), `branch` | One line: title, task id (= thread id), model and options, and the job island bound to it. Starts a full-access thread in the deepest registered project containing `path`; a wrong model or option answers with the live choices. |
| `task_status` | Optional `taskId` | Without: one line per open task (id, title, state: working / waiting on N sub-agents or background commands / done / failed, model). With: its state line and latest reply in full (64 KB, then the `t3_thread_read` call that continues it). |
| `task_cancel` | `taskId`, optional `reason` | Interrupts the running turn (waits up to 10 s for it to stop) and archives the thread; its ending wakes nothing. Not recursive: the task's own sub-agents may finish. |
| `t3_thread_list` | Optional `query` (title text), `limit` (10; 50 max) | One line per thread across all projects, active first: title, state, how long ago, "your task", thread id. Sub-agent threads are left out. |
| `t3_thread_read` | `threadId` or `threadIds` (up to 5); optional `last` (messages: 3, activity: 10), `view` (`messages`/`activity`), `itemId` (+ `textOffset`), `details` | State line, then the last messages whole, oldest first (48 KB budget; the newest is continued by `itemId`/`textOffset` if it alone is larger). `activity`: one line per timeline item with its item id. `details` adds the latest runs and settings. |
| `t3_thread_send` | `threadId`, `message`, optional `mode` (`auto`/`steer`/`queue`/`restart`), `confirm` | One line: delivery and state. A queued delivery names its queued run id, which `t3_queue` (action `edit` or `cancel`) takes as `queuedRunId` to change or take back the message. While Heimdall's own earlier message to that thread still waits in T3's queue, a send (not `steer` or `restart`) joins it instead (`t3_queue` edit, one turn reads both, recorded under the call's operation id so a replay joins nothing twice; bug
note 2026-10-24T23-42-04). After a turn of one of Heimdall's tasks failed on its sign-in, a send to it answers `NOT_SENT` until the automatic retry has gone out (10 minutes past its time at most) and for 10 minutes after a failure that follows the retry (`AUTH_COOLDOWN_MS`; `t3_call` sends too). |
| `t3_tools` | Optional `query`, or `tool` for one tool's schema | The rest of the live catalog (83 tools in the researched build; the inside-only ones answer with the Heimdall tool to use). |
| `t3_call` | `tool`, `args`, `confirm` | Calls any catalog tool. Args are checked against the live input schema; `clientRequestId` is filled with the action id; T3's error text comes back with secret-looking parts redacted; non-idempotent calls are recorded so a replay never repeats them. Thread reads, lists, sends and the model catalog come back in the compact forms. |

- **Briefs.** The T3 layer puts the default task prompt (`agent::harness::WORKER_PROMPT`, from
  `crates/agent/prompt/worker.md`) above every `delegate_task` brief (`Workers::set_brief_preamble`); see
  `docs/skills.md`. Which models a delegate may use is the `models` skill's rule (Anthropic subscription only), carried by the prompts; `delegate_task` itself accepts any model T3 lists.
- **Job islands.** Every task Heimdall starts (`delegate_task`, a launch or fork through `t3_call`) gets a bound job
  island before the call returns (the task hook, `Workers::task_hook`); an output `.html` named in the brief becomes
  its open button.
- **Worktrees.** T3 binds a thread only to its project's root, an existing worktree of that project's repository, or
  a new worktree from it, and provisions a worktree by running git at the project root (a project folder that is not itself a repository fails T3's git check). So T3's own worktree is
  used only when `path` is in the project's own repository. For `path` in another repository (a repository nested
  inside the project) Heimdall makes the worktree itself from the repository holding `path` (`git worktree add`
  from that checkout's HEAD, beside the main checkout as `<repo>-<branch>`, branch `branch` or
  `heimdall/<title>-<id>`). A worktree this action made is reused (its id is in the worktree's git folder); another
  task given the same branch is `WORKTREE_BRANCH_IN_USE`. The thread runs at the root with "Working folder: <that
  worktree> (a new git worktree of …)" before its brief; the start line names the worktree. Outside any repository
  isolation is `ISOLATION_UNAVAILABLE`. A shared task in another repository runs at the root with "Working folder:
  …" before its brief, never with that repository as `worktreePath`.
- **Confirmation.** These return CONFIRM_REQUIRED with an exact question: deleting a project or scheduled task,
  changing environment settings, and, on a thread Heimdall did not start, interrupting, stopping a workflow,
  restarting, archiving, editing or cancelling its queue, answering its question, merging it back, handing off its
  worktree, changing its model settings, or any other call T3 marks destructive (`crates/t3/src/orchestrator.rs`,
  `named_guard`, `DESTRUCTIVE`). Only the user's click on Yes approves one call; their voice does not. Sends, pins, forks
  and launches stay free (the user has full use of their threads). T3 0.1.6's action tools (`t3_queue`,
  `t3_pending_request`, `t3_thread_transfer`) are judged per action under the old tool names (`operation`): their
  reads (`list`, `read`, `status`) are free, a fork is free and becomes Heimdall's, a `merge_back` asks unless Heimdall
  started both its source (`threadId`) and its target (`targetThreadId`), and the other writes follow the rules
  above and always keep a replay record.

## Approvals in Heimdall's own threads

Superseded by docs/harness-autonomy.md: every harness runs in its no-prompt mode and the hooks approve
everything but a short deny list, in every session. T3 launches each Claude instance with
`CLAUDE_CODE_EVEN_MORE_DANGEROUSLY_SKIP_PERMISSIONS=1`, so the rm checks `bypassPermissions` keeps no longer ask.

- **The hooks.** `heimdall-emergency hook permission` (`PermissionRequest`) answers any prompt Claude still raises:
  allow, unless `deny.rs` refuses it, in which case the agent reads why. A question is left to the harness; a
  `[heimdall-bg]` background call is declined so it stops. `hook pretool` (`PreToolUse`) refuses the deny list before
  any tool runs. Each answer is a line in `<data>/logs/approvals.log`. A `PreToolUse` "allow" cannot pass Claude's rm
  checks, which is why the permission hook and the launch variable are both needed.
- **Every session.** The hook no longer looks for Heimdall's launches: the user's threads, Heimdall's, and `delegate_task`
  children at any depth get the same answer. The launch and thread records (`<data>/approvals/launched`, `threads`)
  are still written and identify background calls.
- **T3's leftover card.** T3 never closes the card the hook answered: it stays "waiting" and `pendingRequestCount`
  stays up until the provider session closes, sometimes hours after the run ended. T3 0.1.6 gives an MCP client no
  way to resolve it (`t3_pending_request` respond answers questions only), so Heimdall cannot clear it in T3; it
  treats it as stale instead:
  - While a run is live, a card counts as a wait only while its tool call has not run (`text::open_approvals`, by the
    tool call id both items carry). The answer is kept per thread until its `updatedAt` or pending count changes
    (`waits_on_approval`).
  - Once the run has ended, no approval counts (`threads::live_requests`, `threads::asks`): task_status, thread
    labels, the observer's wake, the islands and archiving a cleared task all ignore it. Questions T3 still lists
    are still reported, and keep a cleared task open.
  - Reading a thread's requests never stands between a finished run and its terminal notice (the read once failed on such a thread and the notice never went out).
- **Installing it** changes Claude's user settings: `heimdall-emergency hook install` adds the `PermissionRequest`
  and `PreToolUse` entries to `~/.claude/settings.json` (the bundled CLI's path and `--data-dir`, timeout 10 s), replacing an earlier copy
  and keeping everything else (a linked settings file is written through its link; keys may be reordered);
  `hook uninstall` takes them out. Heimdall's startup check (`harness apply`) installs them.

## Following tasks and waking the agent

- **Connection state is not agent news.** `Workers::connection()` reports `Unknown` (before the first attempt),
  `Connected`, `Closed` (T3 Code not running) or `SignInRequired` (never paired, expired, revoked, or rejected
  with a 401); the app reads it. The island shows "T3 Code closed" once per outage, "Connect T3 · click" once per
  credential state, and "Worker monitoring unavailable/recovered" once per episode. None of these reaches the
  agent's outbox, so an unconnected or closed T3 never wakes Heimdall (EXPECTED E7.6).
- **Wake policy (E7).** The agent is woken for three things only:
  - a task Heimdall started (`delegate_task`, or a launch or fork through `t3_call`) that is done or failed, with its
    reply in full (`workers.rs`, ids `terminal:{thread}:{run}:{state}`);
  - the end of a run Heimdall sent a message into on someone else's thread (`t3_thread_send`; the agent should not need a wake-up and then a prompt to read it). After that run's result the
    thread stays followed for the runs T3 starts by itself (a sub-agent or background command of that work finishing,
    each reported) until a run
    starts with a message (the user's own: it and their later runs wake nothing) or nothing new happens for 60 minutes
    after the last notice (`TRAIL_MS`). It stays the user's for every confirmation rule;
  - a question or an approval wait on any thread (observer, one "T3 update" per 30 seconds): T3 says "waiting", or
    counts a request while the run is live (`threads::asks`, the same rule as the "waits on a question or approval"
    labels; bug note 2026-11-01T00-15-42). A finished thread's leftover count wakes nothing. A task's approval
    wait wakes once per request (`approval:{thread}:{run}:{tool call}`, or `…:{run}` when T3 shows no card) with the
    card's text (the command and Claude's reason) and the `t3-thread://` link that opens the thread; the same wait is
    kept for the task's job island (`Workers::approval_wait`) until the prompt is gone or the run ends. More approvals
    counted than cards the hook already answered is a wait, so an answered card in view never hides a second prompt
    whose card is elsewhere, such as a sub-agent's (`text::approval_wait`; bug note 2026-11-04T18-30-26: on 4 Nov the
    hook approved the beauty climb's rm prompt at 17:55:23 and the command ran the same second, but T3 kept its card
    "Waiting for approval" until the user clicked it at 17:58, which T3 then logged as a failed delivery).

  "Done" is T3's own `task_status` rule (orchestration-v2 `delegatedTaskProgress`): a turn that ends while the
  thread's sub-agents or background commands still run is `waiting_for_children`, not a result.
  T3's MCP read does not expose its projection, so each pass follows the thread's activity: `subagent` items still
  pending/running/waiting, and "Command running in background with ID: …" items without their `Command "…" finished`
  notification (or a KillShell/TaskStop of that id). Inherited (forked) items do not count. That notification is
  only a timeline item when it starts a turn, and it quotes the command's description rather than its command line
  (four finished downloads once held a research task's final reply for 30 minutes), so once a
  turn has ended a background command counts only while a process writes its output file (Claude's "Output is being
  written to: …", checked with one `lsof` call; a missing file or a failed probe proves nothing). A held turn is superseded
  by the next turn; if the children close and no new turn starts within 45 s, the held turn's reply is delivered
  when those 45 s end (a timer, not the next 30-second pass). A sub-agent that closed while the turn ran holds it the
  same 45 s only when nothing shows the turn took its result in: a tool call of the turn that finished after the
  sub-agent closed, followed by a model call, means Claude handed it over inside the turn and T3 starts no turn for
  it (an Explore agent once closed four minutes before its turn ended, and the
  result came 71 s late). A followed run's event-backed wait that times out is re-armed at once;
  background commands still running after 3 minutes (`BACKGROUND_HOLD_MS`) no longer hold it, and
  the notice names them (a task once waited 35 minutes behind a waiter polling for exactly that). A turn held for sub-agents is delivered after 30 minutes anyway, naming the ones it still
  waits on (`SUBAGENT_HOLD_MS`). A Claude Monitor is background work too, from its `Monitor` tool item (its task id)
  until T3's `Monitor "…" was stopped` (or finished), a TaskStop of that id, or its own `timeout_ms` (5 minutes unless
  named, 30 at most; a persistent one counts 30 minutes); its output file proves nothing, as Claude writes the events
  there itself. A Monitor item still starting (no task id yet) is read again on the next pass, while its
  turn runs and for 2 minutes at most (`MONITOR_START_MS`). Each monitor event
  starts a turn with T3's "Background activity updated" (every
  tick of a labelling and a download task once woke Heimdall); once a notice has named the task's background work, such a
  turn is progress while a monitor runs, unless a sub-agent or background command closed since that notice (then it
  hands that result over). A monitor re-armed with a description a notice already named counts as named; a new one is
  named once. A monitor alone never holds back the turn a command's own "finished" notification starts, and a mixed
  `Monitor "…" and command "…" finished` closes both. The worker prompt asks for one report, on exit or failure. A failed tool call inside a running task wakes nothing; a failed run does, except a task turn
  that failed on its provider's sign-in (`text::auth_failure`: "Failed to authenticate: OAuth session expired and
  could not be refreshed", an expired token, a 401; such turns once failed for six minutes
  and then worked). Such a failure, and more of them before the retry, wake nothing; 5 minutes after the first
  (`AUTH_RETRY_MS`) Heimdall sends the task one "carry on" message (`AUTH_RETRY_MESSAGE`, idempotent by the failed
  run), unless the task has moved on by itself (another turn started or ended). Only the failed turn's own first line
  counts. Its result arrives as usual with the failed turns' replies before it; a sign-in failure after it is reported with a line saying the retry failed too
  and to send nothing until the sign-in works. One retry per episode (2 hours); any other ending clears it.
  Heimdall's own interrupts and archives wake nothing (day review T3).
  A run T3 cancelled before it started is no state of the thread (`threads::current_run`): T3's thread status is
  its latest run's, so the turn T3 queued to hand over a finished sub-agent and then dropped made the setup-wizard
  task read "cancelled" while its turn still ran (bug note 2026-11-04T22-25-15). The scan, `needs_scan`,
  task_status and archiving a cleared task follow the active run, else the newest run that started
  (`threads::followed_run`, review 5 Nov), with two exceptions: a run Heimdall's own message started (`sent_runs`)
  that T3 dropped before it started is the thread's state, "cancelled", reported once as "nothing of it ran; send it
  again" (after the turn it was queued behind, if that one ended unreported); and a dropped run an older build recorded
  the ending against stays the followed run while it is the newer one, so nothing is reported twice and the task can
  be archived. The task island (`islands/binding.rs`) still reads plain `current_run`.

  Finished runs of other threads never wake the agent. Sub-agent threads and archived threads wake nothing. A line is
  "title, outcome, last assistant message" (or the question text, or "waits for an approval"); each line id is
  `t3:{thread}:{run}:done` or `t3:{thread}:{run}:attention:{request ids|approval}`, so a run reported twice wakes once.
  The observer's first pass after each start is a silent baseline. Threads launched through `t3_call` in older builds
  (`owned_threads` only) still report through the observer.
- **Restart.** Everything followed lives in `t3-workers.sqlite` (tasks, watched runs, cursors,
  children, held turns, notices) and every notice goes through the outbox in `mcp-actions.sqlite` under a
  deterministic id, so after a restart the same tasks are followed: a run that ended while Heimdall was down is
  reported by the first pass, the outbox drains at the start of every pass (even with T3 closed), and a notice whose
  delivery a quit cut short is offered again under the same id, which the agent's deliveries deduplicate. The first
  pass of each app process sends one "Heimdall restarted" notice, one line per followed task or thread with its
  state; a model switch in the same process sends none.
- **Heartbeat.** `Workers::heartbeat` (for an app tick; each watcher pass runs the same check) drains the outbox,
  scans a followed thread whose latest run is recorded finished with no notice and nothing holding it, and delivers
  once more, as `{id}:again`, a result whose outbox row was dropped for a newer delivery epoch. Never a duplicate id.

## Design notes from the research

Research against `T3 Code (Nightly)` `0.0.46-nightly.20261013.3042`, the installed app's source, T3's public docs
and a read-only live probe. Kept: the findings that still explain the code or bound what Heimdall can do. The live catalog is `t3_tools`.

### Why an outside client

- T3's supported route for an outside agent is OAuth MCP (authorization code with PKCE S256, or the pairing-code
  answer Heimdall uses). Ordinary `t3 auth session issue` tokens lack the `mcp-client` audience; a CLI session with
  `--subject mcp-client` authenticates but is an implementation detail, used only by the 14 Oct probe, never in
  production. ACP's `acp-mcp-call` forwards the credentials T3 gave a provider process; using it would steal that
  thread's scope.
- Approval ceilings are `read-only`, `approval-required`, `auto-accept-edits`, `auto` and `full-access`. A client acts
  only on threads whose runtime mode does not exceed its grant, so Heimdall's workers need the full-access grant;
  a lower ceiling never silently upgrades. Access is environment-wide, not project-scoped: Heimdall enforces its own
  owned-task boundary (`Workers::owns`).
- T3's optional `pi` provider (T3 starting Pi in RPC mode with its own MCP bridge) is a different integration and
  need not be enabled for Heimdall.
- Never integrate by reading or writing T3's SQLite database or its signing keys under `~/.t3/userdata/secrets/`.

### Why an MCP wait plus a 30-second scan

- `t3_thread_wait` subscribes to stored `run.updated` events after the thread's current sequence and returns only
  at a terminal status (replay closes the read/subscribe race). Idle threads and terminal runs return at once, so
  waits are armed only for a new run or a timed-out active one.
- Questions and approvals have no event over MCP: `pendingRequestCount` counts both, the question list
  (`t3_pending_request` action `list`, one tool with `list`/`read`/`respond` since T3 0.1.6) excludes
  approvals, and no MCP tool reads an approval's details. A positive count with no question ids is reported as "waits
  for an approval", never invented.
- T3 has a richer WebSocket feed (`orchestration.subscribeShell`, `subscribeThread`), but it deliberately rejects
  MCP-client credentials there so clients cannot bypass their approved ceiling. Using it needs a second
  `orchestration:read` credential and a more volatile protocol; not worth it until question latency demonstrably
  matters. `GET /mcp` is no event feed (405).
- `schedule_task` with a webhook schedule is inbound (an HTTP request starts a T3 task), not T3 notifying Heimdall.

### Limits T3 sets

- No MCP tool deletes a thread, changes its runtime permission mode or approves a permission request; archive is the
  closure. The user resolves approvals in T3.
- `t3_thread_interrupt` interrupts one run; it is not the UI's recursive Stop (`thread.stop`, which also stops
  descendant work). So `task_cancel` cannot promise to stop a task's own sub-agents.
- `t3_thread_launch`, attachment sends, fork, configure and organize take no retry key. Heimdall records a launch
  intent with a unique `[heimdall:<id>]` marker before launching and reconciles a lost response by it, never by
  launching again. Send and interrupt take `clientRequestId`, reused for the same operation only.
- Item text can change in place while streaming and `textTruncated` continues by `itemId` and `textOffset` (UTF-16
  code units), so a final reply is collected whole by id before it is delivered.
- A run that completed is not a task that finished: sub-agents, background commands or a queue can outlive the
  turn. That is why "done" follows T3's `waiting_for_children` rule above.

### Model switching

`t3_thread_configure` changes model, options or provider instance. Same-instance changes apply on the next turn
(the running turn keeps its model); a different instance switches provider with T3's own handoff, which may detach
the old session. ACP sessions without a model-switch capability reject a change. Policy, not a T3 limit: one model
family per thread; a change of family is a new thread with a handoff brief. On a thread Heimdall did not start,
configuring needs the user's click.

### Sources

- Installed app source: `/Applications/T3 Code (Nightly).app/Contents/Resources/app.asar` (researched build, SHA-256
  `3897fd01cb458cc1b7744b347545d95ea032a92f3a02501e22de3312cc05224f`): `auth/mcpOAuth`, `mcp/McpHttpServer`,
  `mcp/threadAccess`, `ThreadManagementService`, `ProviderSwitchService`, `ProviderSessionTransitionPolicy`.
- https://github.com/pingdotgg/t3code; outside agents:
  https://github.com/pingdotgg/t3code/blob/main/docs/user/outside-agents.md; providers:
  https://github.com/pingdotgg/t3code/blob/main/docs/internals/providers.md; WebSocket contract:
  https://github.com/pingdotgg/t3code/blob/main/packages/contracts/src/orchestrationV2.ts.
- Live evidence: `crates/t3/tests/fixtures/t3-live-v2-evidence.json` (outside-client read-only check: eight provider
  instances, two projects, zero threads created, credential revoked), `lab/history/evidence/t3-live-catalog.json`
  (the 83-tool catalog), probe `lab/t3-probe/t3_probe.py`.
