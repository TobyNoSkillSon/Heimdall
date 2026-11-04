# The relay: Heimdall's agent as a T3 thread

Heimdall's main agent can be a thread in T3 Code (Claude on the user's Anthropic subscription)
instead of Pi's model. Heimdall relays to that thread everything that would wake Pi, and the thread acts through
Heimdall's own tools. One mechanism, two ways in:

- **Relay mode** is the user's choice: Agent ▸ Model ▸ Claude Code subscription (`Settings::agent_relay`; its row is there once Add Provider ▸ Claude Code subscription added it, `Settings::relay_added`, which never makes it the agent; Providers ▸ Claude Code subscription ▸ Remove offers it again, refused while it is the agent). It stays on across restarts
  until they pick another model. The row opens to T3's Claude models (`orchestrator_capabilities`, read on each menu
  open); the one confirmed is `Settings::agent_relay_model` (none: T3's first). A new one moves a running relay to a
  thread on it. Like every row in Agent ▸ Model, a click only proposes; Confirm switches (`ui::mac::model_choice`).
- Under the models, past a separator, the relay's account : Automatic, then each T3 Claude provider
  instance that can run, by T3's display name (`agent::emergency::relay_accounts`). The one confirmed is
  `Settings::agent_relay_account` (the instance id, e.g. `claudeAgent_claude_code_2`; none: automatic), sent as
  `UiAction::SelectRelayAccount` and handed to the controller as `emergency::Options::relay_account`. A new one restarts
  a running relay like a new model. Both lists come from `crate::relay_catalog` (read when the supervisor connects to
  T3 and on each menu open); the `settings` tool offers and checks the same lists (`agent_relay`, `agent_relay_model`,
  `agent_relay_account`).
- While the relay answers, Pi's own calls (a compaction on its model) report no provider problems to the menu: they
  are not the agent's ("Anthropic credits used up" must not show while Claude runs in T3).
- **Emergency mode** switches the same relay on by itself when Pi's agent fails (docs/emergency.md: a spent quota, a
  rejected sign-in, a failing provider, no answer). It adds only what is emergency-specific: detection, the indicator,
  probing the main agent and the hand-back.

Relay mode and emergency mode are the same mechanism: relay is a toggle, and emergency does the same thing only when the agent's quota runs out.

## What the relay thread gets

| | Pi | Relay thread |
|---|---|---|
| Input | wakes, typed, notices, task results, scheduled wakes, images | the same, one `t3_thread_send` each, in order, `[For Heimdall, …]`; images as private files (0600) |
| Tools | the main MCP server, as `mcp__heimdall__*` | the same tools, names and definitions (`mcp_contract::tools()`) plus `emergency_status`/`emergency_end`, as `mcp__heimdall__*` through `heimdall-emergency mcp`; the CLI's `call` as the fallback |
| Replies | the final answer is spoken and shown | the run's last assistant message goes through `Tools::agent_final_answer`: spoken, shown, `NO_REPLY` silent, de-duplicated against `speak` |
| Island | send arrow, working dots, ready | send arrow on each relayed input, working while the T3 run runs, ready when it ends |
| Stop | aborts the turn | `t3_thread_interrupt` on the running run, plus the Stop line in the thread |
| Record | Pi's session | Pi's session too, mirrored (below): every input, message and action, never a turn |
| Prompt | `prompt/heimdall.md` with its runtime fills, and the skills index | the same prompt, fills and skills index, plus `prompt/relay.md`, whole at the top of the first message (Takeover) |

Pi keeps running as the record: it takes context appends (local, no model call) so that Heimdall has everything when
Pi answers again. While the relay acts, Pi makes no model calls of its own: cache pings and idle compactions are held in
both modes. Session rotation at its size limit still compacts (one summary call on Pi's model), as before.

Background model work (plan refresher, island updates, day timeline naming, suggester) is not moved to the relay. In
relay mode it keeps running on Background Tasks ▸ Model (for example a ChatGPT model); that is the existing setting,
so no new one was needed. In emergency mode it pauses quietly (`background_gate::emergency_pause`): no requests, no
failure cues or quota notices, the Background Tasks rows dimmed with one triangle whose tooltip names the reset; it
resumes at the hand-back.

Background Tasks ▸ Model can itself be a Claude Code instance in T3 (`t3:<instance>`, e.g. Haiku on
`claudeAgent_claude_code_2`): each job's call is then one short-lived `[heimdall-bg]` thread in the "Heimdall
background" project, separate from the relay thread and never followed by the T3 observer (docs/background-call.md).
Those calls pause in emergency mode like every background job, and an instance whose usage limit a call meets gets no
further call until the reset Claude states. Only Claude instances are offered there, never Codex.

## Context continuity, both ways

There is no syncing and no hand-off to send: the information in the T3 Code thread messages is transferred whole to the freshly refreshed agent. The tool sets differ, so no tool
call is ever replayed: messages go across whole, and each action as one plain line with its result.

- **Going in**: the context file carries Heimdall's conversation from Pi's session, the last 100,000 tokens of it:
  every message whole, each of Heimdall's tool actions one line (`Heimdall used plan_edit: … → Added …`), and everything
  that reached the session as context: input held for a relay (`To Heimdall (relayed to the relay agent): …`) and the
  relay's mirrored lines. In relay mode those are all the session holds, so without them the file stopped at Pi's
  last own turn.
- **While the relay acts**: Pi's session mirrors the thread (`mirror`, from `t3_thread_read`'s activity view, paged,
  each item's whole text): the relay agent's messages whole (`[Relay agent] …`), Heimdall's own messages to it, each
  other action one line (`[Relay agent ran] git log -1 → completed`). Its Heimdall tool calls arrive live as
  `[Relay agent] said to the user: "…" → Speech queued: 1.`, so their CLI commands are left out of the mirror; the
  relayed inputs are in Pi's session already (held), and the takeover's first message is Pi's own context. The
  mirror runs every 5 minutes of a long run and when each run ends; each entry's id is its thread and position, and
  the position is in `state.json`, so a restart goes on without repeats. A message over 64 KB goes in as consecutive
  parts, whole.
- **Coming back**: the last of the thread is mirrored, then the hand-back turn says only that Heimdall is back and
  that the relay's conversation above is Heimdall's own; relay off adds one line that the user switched back. Input the relay had
  not finished (its run still working, or not yet delivered) wakes Pi, in order, after the hold lifts (`Emergency::open`,
  `Side::unfinished`); input before a Stop is dropped. A relay model change hands it to the new thread instead. No summary, no
  hand-off, no "what it did" list.
- **Size**: Pi's session grows by what the relay says and does. Relay mode compacts it as usual while held
  (`Bridge::set_hold_compacts`); emergency mode holds compaction (the provider is down) and the main model compacts
  after the hand-back as it always does at its threshold. When the agent's provider refuses a compaction (spent credit
  or quota, sign-in, a history too long for it), Heimdall summarises it on the first model whose window holds it
  (Background Tasks ▸ Model, then another signed-in subscription) and Pi writes that as its own
  compaction; if none can, the history stays as it was with one notice, or, when it is past the agent's budget, only
  the newest whole turns up to 80% of the budget stay in the context (the rest stays in the session file), with one
  notice (`bridge/compaction_fallback.rs`).

## How the thread reaches the tools (the connection)

Heimdall's MCP server is the one source of the agent's tools (one tool surface to maintain):
Pi's agent and the relay both get its `tools/list` and its dispatch, so a tool added there reaches both. Pi's bridge
(`runtime/heimdall.ts`) registers each as `mcp__heimdall__<name>`; the relay thread's Claude sees the same names from
the server `heimdall` in its user config.

- **The connection.** A second MCP server on its own loopback port with a 256-bit token, written to
  `<data>/emergency/connection.json` (folder 0700, file 0600, atomic) with the thread it was issued for: `pending`
  while T3 launches it, the thread's id as soon as `t3_thread_launch` returns. One token per launched thread; revoked
  when the relay ends; the server dies with the process. Outside a relay there is no file and no server. It is gated by
  the agent's own delivery gate: Agent off refuses it exactly as it refuses Pi. It serves the main server's tools on the
  same dispatch path (validation, crucial writes, permits) plus the relay's own two.
- **The MCP route.** T3 offers an outside client no way to give one thread an MCP server (`t3_thread_launch` and
  `t3_thread_configure` take none, checked). So Claude Code's user config names one stdio server, `heimdall`:
  `heimdall-emergency mcp`, bundled beside Heimdall. Every Claude session starts its own copy, and T3 puts the session's
  thread in its environment (`T3CODE_THREAD_ID`, checked live: the id `t3_thread_launch` returns, passed on to the
  MCP servers Claude starts). The proxy reads the
  connection on each request: the connection's thread gets the scoped server's list and answers whole (images inline, as Pi gets
  them); a `pending` connection lets a call wait up to 20 s for the thread to be named; every other session lists no tools
  and its calls are refused (`mcp::NOT_RELAY`) without a request to Heimdall. Its instructions carry the rule for every
  other session (`mcp::RULE`): leave them alone unless the user directly asks or the thread has the relay's prompt.
- **After an update.** The relay thread outlives Heimdall's restarts, and Claude keeps each tool definition it
  loaded (ToolSearch) in its conversation, so a new build's tools were called the old way (plan_edit was called without `quote` for hours after the install that added it). The state keeps each
  tool's definition digest as the thread learned it (`State::tools`, set at takeover); the next relayed input after
  a change starts with one line naming the changed tools and the ToolSearch `select:` that loads them again. State
  from before this field gets a general line once.
- **Installing it** writes one `mcpServers.heimdall` entry into every Claude config T3 may run with (`~/.claude.json`,
  `$CLAUDE_CONFIG_DIR/.claude.json`, each `~/.claude-*/.claude.json`): `heimdall-emergency mcp install` (docs/install.md);
  `uninstall` takes it out. Without it, or on a harness without Claude's config (Codex, Grok), the relay prompt sends
  the agent to the CLI: `tools` (the same server's list) and `call <tool> '<json>'`, images as private files. The CLI
  has no tool code of its own: its old shortcuts (`speak`, `plan`, `end` ...) are gone.
- **The rule for everyone else.** Heimdall's task rules (`prompt/worker.md`, above every brief) and the server's
  instructions say: Heimdall's tools may be listed; use one only when the user directly asks, or under an explicit
  relay instruction, which then requires them. The relay prompt says the relay must use them, and that its own
  sub-agents are not the relay. A direct ask in another thread is still refused by the connection: only the relay acts.
- **Protection.** The token is Pi's equal, minus the file-descriptor hand-off: any process of the user's account can read the
  0600 file while the relay is on, and the thread check is against the environment, which a process of the same user
  could set. It keeps Heimdall's other threads and sub-agents from driving Heimdall by accident, not a hostile local
  process; that is accepted, as Pi's token is equally reachable by the same user (its process memory). The token never
  leaves the Mac and never goes into T3's transcript. Sub-agents inside the relay's own Claude session share its
  process and environment, so the connection cannot tell them apart; the prompt rule covers them.
- **Restarts and new tokens.** The proxy holds no state: a new port or token is read on the next call. A call that
  meets a retired token, a server that is gone, Agent off or a missing connection file never reached Heimdall's dispatch, so
  it is tried again for up to 5 s, on the new connection when one comes. When the session's connection changes (named, a new
  token, ended) the proxy sends `notifications/tools/list_changed`, so a session started before its connection lists the
  tools then (that Claude Code re-lists on it is not checked live). Each call runs on its own thread; a cancel does not
  stop a call, as for Pi.
- **Outside T3.** A caller with no `T3CODE_THREAD_ID` (a script, a terminal) is refused by both the server and the
  CLI: every relay runs in a T3 thread. T3 sets the id for Claude's commands and the MCP servers it starts (checked live on other local MCP servers); for Codex and Grok relays it is not checked.

## Takeover

The first message (`em::Brief`) is the relay's system prompt, then the protocol, plus a context file. T3 threads take
no system prompt, so the prompt goes inline:

- **The relay prompt**, between `<system_prompt>` and `</system_prompt>`, first. `crates/agent/build.rs` composes it
  per edition from one source: `prompt/relay.md` (what differs in a T3 thread: the CLI for every tool, its quirks,
  replies spoken and shown on the island, island tools, models, the approval hook), Heimdall's own prompt
  (`prompt/heimdall.md`), and the index of the edition's bundled skills (name, description, `SKILL.md` location, as
  Pi lists them). At takeover (`agent::relay_prompt::text`) it gets the same fills as Pi's prompt
  (`harness::fill_prompt`: names, tool log, docs, connectors, emergency protocol), the CLI line, and the
  index of the profile's own `<data>/agent/skills` when that folder exists (what Pi loads), else the bundled sets.
  About 24,500 characters.
- **The snapshot**: `prompt/relay.generated.md` (`prompt/public/relay.generated.md`) is the composed template, checked
  in. `relay_prompt::tests` fails when Heimdall's prompt, a bundled skill or `relay.md` changed without it, and names
  the command that rewrites it: `HEIMDALL_WRITE_RELAY_PROMPT=1 cargo test -p agent relay_prompt` (add
  `--features public` for the public edition).
- `<data>/emergency/context.md` (0600): Heimdall's recent conversation, the last 100,000 tokens of it (bounded at
  400 KB, about 4 bytes a token, cut on a line), the open plan (16 KB), the T3 tasks the user follows, and the cause.
  T3 caps a first message at 120,000 characters, so the conversation goes in the file, and the message holds the
  prompt, the protocol, the cause, the last 12 KB of the conversation, the plan and the file's path. With the longest
  tail and plan it is about 56,000 characters; `em::tests` asserts it stays under the cap.
- Emergency: "This is Heimdall's emergency protocol: continue the user's work." The thread reads the context, finds what the user
  and Heimdall left unfinished, picks it up, and sends the user one short message about it (a `notice`; `speak` only if
  they must act). This is the one exception to "don't talk to the user unless they talk to you".
- Relay: the same prompt and file, without the emergency lines.
- A recent thread reused for a new takeover gets the same whole first message again.

A thread counts as answering as soon as it shows it: its first CLI call, its first real assistant message, or its run
still working after 90 s; a harness error reply fails it at once. The wait is the first sign of life, typically a few seconds.

## Which T3 harness

What `orchestrator_capabilities` tells an outside client (checked live against T3 0.1.3): each
provider instance's `providerInstanceId`, `driverKind`, `displayName`, `constraints` ("Provider instance is disabled."),
`runtimeModes` (absent means all), `canRunChildTask`, and a short list of models with their option values. It does not
say the subscription or plan, the quota left, reset times, or sign-in health: Codex shows enabled with its quota spent.
No other T3 call does either (`t3_environment_read`: server identity and preferences; `t3_thread_configure` without
`modelSelection`: a thread's selection and modes; `t3_thread_read`: a thread's runs and messages). So a harness is judged by trying it:
the first run's reply (`harness_error_reply`) is the only health signal.

T3 tells nothing about quota, but each account's own CLI does. The relay reads it from the same place as the background
jobs' fallback (`agent::accounts`, docs/background-call.md, "Merging with the relay's account selection"): one
process-wide reading of `Tools/subscription_quota.py` when that script exists (without it every
account is "quota unknown"), the accounts' config folders from T3's `providerInstances.*.config.homePath`, no prompt,
no token read; T3's folders are read again at every run. The relay asks for a reading at most a minute old and waits
up to 20 s for a stale one to refresh (`Board::fresh`; a failed read is "quota unknown", never the older reading);
`agent::routing::quotas_from` turns it into the ranking's windows. Known gap: the script reads one Codex sign-in, so a
second Codex instance in T3 shows "quota unknown" and ranks after the accounts whose quota is known.

Ranking: enabled, offers full access, has a model, not on the failed provider's subscription
(`shares_subscription`: ChatGPT ↔ Codex, OpenCode Go ↔ OpenCode), then the order by driver kind
(`Settings::relay_order`, default `claude, codex, *`: the main assistant may go
Claude → Codex; delegates stay Claude only).
`*` is every other harness in T3's order, `-x` never. Relay mode (Claude Code subscription chosen) uses the same order, so when
Claude does not answer another allowed harness takes the thread; set `relay_order` to `claude` alone to keep it Claude only. Each with the first model T3 lists for it. Nothing assumes
a particular set of harnesses.

## Accounts and recovery

`agent::routing` picks the account; `emergency_recovery` builds the cards.

- **Which account.** Among a driver's eligible accounts, the one whose quota resets soonest. When the windows disagree, the documented rule is: an exhausted account (a window at 100%, or the provider
  says a limit is reached) is never eligible, however soon it resets; a known quota ranks before an unknown one
  (unknown is not headroom); a five-hour window at 95% or more ranks after the others; then the earliest weekly reset
  (to the hour), then the earliest five-hour reset (to the quarter hour), then T3's order. Agent ▸ Model ▸ Claude via
  T3's account row (`Settings::agent_relay_account`) puts the chosen account first; Automatic uses the rule.
- **Stays.** The rule runs when the relay starts and after a failure, never while it works.
- **On failure** (Pi's provider, or the relay's own account: a run failing with a quota or sign-in error), a quick scan
  (catalog, every account's quota), then the recovery card in the pull-down with a one-minute
  countdown: the same model on the other accounts of the failed provider first, then the other providers (the main
  assistant only), and "Wait for <original>", each with its quota or "quota unknown". The card has four places:
  the failed provider's other accounts first, then one account of every other provider, then the rest; everything stays
  in the automatic search. Codex is left out after ChatGPT failed only while its own quota is unknown or spent. No pick in the minute (or the card closed): the search runs in that order by itself. Input is held
  meanwhile; what the failed relay had not finished goes to the next one, images included; finished actions are only
  in the conversation, never replayed.
- **Phases.** `Choosing` (the card), `Starting` (searching), `Covering` (a replacement runs: normally, the relay
  indicator, background work on), `NoCover` with `waiting` (the user chose to wait). The
  emergency indicator shows only while choosing and searching.
- **The original.** Remembered (`Original`: the built-in agent and its model, or a relay account and model): what was
  hosting Heimdall when the chain of failures began, relay mode's account in relay mode, never the built-in agent
  there (docs/emergency.md, "The original"). Checked at its reset plus a minute (the reset T3 reported for the failed
  run, else the quota reading's): Pi by its probe, a relay account by its live quota (the reset passing is not proof).
  When it works, Heimdall says so once and shows "Back to …" / "Stay on …", only when no relay run is working. It
  never switches back by itself; the relay's `emergency_end` also only leads to the question. Back: the hand-back to
  Pi, or the relay moves to the original account. Stay (or silence): the replacement stays, becomes the user's relay
  setting (the menu's own actions), and they are not asked again. Waiting for a relay account: once its quota reads usable
  the relay starts on it.

## When nothing can relay

Order: the main agent, then the T3 relays, then the main agent again at the hand-back.

When no harness answers (none suitable, all failed, or skipped), the island shows a large warning triangle: the quota
ran out and no T3 Code relay can drive Heimdall. It waits for the quota, with the reset time when known; relays are
retried every 10 minutes.

## What does not move, what is not covered

- Background model work stays where it is (above).
- Barge-in and Stop: relay speech goes through the same speech queue as Pi's, so `Tools::interrupt` cuts it, and
  each relay run settles like a Pi turn (reply window, address, skip latch). A report that barge-in did not cut cover speech is not reproduced offline; the settle fix is the
  difference found, not a proven cause.
- The pull-down during a relay run is a Pi turn's : the thinking dots under the conversation
  while the run works, and each Heimdall tool call's row (`speak` line, note, started task, question) as it happens.
  T3 reports no tool events, but every Heimdall call the relay makes passes through `emergency::Own`, which hands it
  to the conversation reader (`chat_history::relay_call`) to read as Pi's own call; its outbox record adds nothing.
  The relay's other actions (shell commands, file edits) show nowhere live, as Pi's do not. After a relaunch, today's
  relay rows are rebuilt from Pi's session only for `speak` lines and replies.
- If the relay's own harness runs out (Claude's quota), relay mode logs it and the island shows the no-relay triangle;
  there is no second relay in relay mode.
- T3 closing while a thread relays is noticed only as failed sends (retried every 30 s).

## Code

`crates/app/src/emergency.rs` (the controller: `Reason::Chosen | Reason::Emergency`), `crates/agent/src/emergency.rs`
(brief, harness ranking, connection file), `crates/agent/src/mcp.rs` (`Scope { allow: Allow::All }`),
`crates/app/src/bridge/supervisor.rs` (the hold for both reasons, the controller supervised), `crates/emergency-cli`
(`tools`, `call`, `mcp`: the stdio server for the relay's Claude, `mcp install`).

## Turning it on from other code

The one switch is `Settings::agent_relay` (`crates/app/src/settings.rs`); the agent model stays as it is. A menu row or
provider row selects it with `ui::UiAction::SelectAgentRelay` (`crates/ui/src/model.rs`), handled in
`crates/app/src/pipeline/actions.rs` like a model pick: it sets `agent_relay`, saves, and restarts the agent, whose
start calls `Emergency::choose(true)`. Picking any other model row turns it off. Do not add a second switch.
