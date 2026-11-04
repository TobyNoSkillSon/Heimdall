# Emergency mode

When Heimdall's own agent cannot answer, the relay takes over: a T3 thread acts as Heimdall's agent, with every tool
its built-in agent has, until it answers again. It is the same path as relay mode (docs/claude-relay.md), switched on by
itself, proactively, only in the specific scenarios where the agent runs out of quota. This doc covers what is
emergency-specific: detection, the indicator, probing and the hand-back.

## If you are the relay agent in emergency mode

Your first message carried your system prompt (Heimdall's own, with the relay's notes and its skills index), then
named what failed and your context file. You act through Heimdall's own tools, `mcp__heimdall__*`, the same as its
built-in agent; without them in your list, through the CLI the message names (`<cli> tools`, `<cli> call <tool>
'<json>'`).

1. **Continue Heimdall's work.** Your prompt is at the top of the first message; read the context file. Find what
   the user and Heimdall were doing and left unfinished, pick it up, and tell the user in one short message (your reply to
   the first message): what broke, that you have it, what you are continuing. That is the one exception to "speak only when the user talks to you or must act". When your first message says Heimdall already said the quota sentence aloud, don't
   repeat it. `emergency_status` names your harness and thread, the quota's reset when it is known, and Heimdall's next check,
   which is not the reset.
2. **Answer what arrives.** Each message headed `[For Heimdall, …]` is something that would have woken Heimdall:
   the user's words (`User said`, `User typed`, `User clicked`), a task result, a T3 question, a scheduled wake, the user's
   Stop. Answer it as Heimdall would. Your final reply is spoken and shown like Heimdall's own; `NO_REPLY` keeps quiet. Images
   arrive as file paths.
3. **Work on the cause** when you can do it from here: name the reset time, check the provider or the sign-in, file a
   bug note with the evidence. Model choice, money and accounts are the user's: ask them.
4. **Hand back.** Heimdall checks its agent by itself, asks the user whether to go back to it, and tells you in this
   thread when to wrap up. Calling `emergency_end` early only brings that question forward. When you have
   finished early, call `emergency_end` with what you did. "Handed back" means stop acting now; "still fails" means keep
   covering until the time it names. When told to stop (Agent off), only `emergency_status` and `emergency_end` work, for two more minutes.

Heimdall's conversation mirrors this thread (docs/claude-relay.md, "Context continuity"): your messages whole, each
action one line, so its own agent carries on as if it had been here. No hand-off summary is needed.

## What Heimdall does

| Step | What happens | Code |
|---|---|---|
| Detect | A turn fails on a spent quota or credit (once is enough), a rejected sign-in (twice in a row), any other provider error (3 in a row within 10 min), or work waits 5 min with no event from Pi (a running tool is not silence; a running compaction is not, up to its own 15-minute limit). A spent quota takes its reset from the quota module's `heimdall_quota` limit frame, or from the warning before it while that reset is still ahead. A success clears every count. The user's Stop, aborts and the loop guard are not failures. In relay mode nothing is detected: Pi runs no turns. | `agent::emergency::Detector`, fed in `bridge/supervisor.rs` |
| Controller | Runs whenever Heimdall runs, whatever Agent was at launch, and is started again on the 10 s tick if it ended (launched with Agent off it once never ran, and the failures went nowhere). A message to a controller that is not running is logged. | `Emergency::respawn`, supervisor `emergency_run` |
| Hold | `Emergency::enter` puts the controller in `Starting` before the Bridge holds (otherwise the unanswered wake the hold relayed reaches the controller first, is taken for late input, and starts a Pi turn that shows the quota animation). From then every voice wake, typed message and waking notice is appended to Pi's session as context marked `emergency::HELD` and relayed; nothing starts a turn. Cache pings and idle compactions wait. | `Bridge::hold_turns` |
| Indicator | The warning triangle by the notch for as long as emergency mode is on, and one `Notice`: `Agent out of quota till 03:04 · backup agent on it, nothing to do`. Background Tasks in the menu dim, with one triangle whose tooltip names the reset. Back: `Recovered` "Agent back". | `UiUpdate::AgentRelay`, `Cause::island` |
| Background | Background model work (plan refresher, island updates, day timeline naming, suggester) pauses quietly: no requests, no failure cues or quota notices; it resumes at the hand-back. Relay mode does not pause it. | `background_gate::emergency_pause` |
| Relay | docs/claude-relay.md: harness order, first message and context file, first-sign-of-life acceptance, the connection, replies, Stop, the send arrow. | `crate::emergency` |
| No relay | No harness answered, or `Settings::relay_skip`: the island's large triangle says the quota ran out and no T3 Code relay can drive Heimdall. It waits for the quota, with the reset when known; relays are retried every 10 min. | `crate::emergency` |
| Probe | At the known reset plus a minute, else after 2 min, doubling to 30. ChatGPT: one tiny request on the agent's own model (the only check that sees the plan limit); other API providers: the sign-in check. A model switch probes within 5 s. | `ProviderProbe`, `em::next_probe_ms` |
| Recovery card | Before any relay starts: what can take over (other accounts first, other providers, waiting), with a one-minute countdown; unanswered, the search runs by itself (docs/claude-relay.md, "Accounts and recovery"). Emergency mode is only this and the search: a relay that answers runs normally. | `emergency_recovery`, `Running::offer` |
| Announced | The card is said, or shown on the island with Speaking off (`mcp_actions::say`): "Opus 5.5 on Claude is out of quota until 13:00. In a minute I'll switch to Opus 5.5 on Claude code 2, unless you pick something else on the card.", with an island notice `Claude · Opus 5.5 out till 13:00 · switching in a minute`. The switch shows `Now on Claude code 2 · Opus 5.5`; in relay mode it is also said ("Switched to …"); in emergency mode the relay's own first reply tells the user. | `rec::card_line`, `Running::covering` |
| The original | What was hosting Heimdall when the chain of failures began: relay mode's account and model (Claude via T3, the main account), or the built-in agent and its model in emergency mode. A later failure of a replacement keeps it. Relay mode never has the built-in agent as its original: a stale one is dropped when the state loads. A relay without a recorded route (reused, or from an older build) finds it from its harness line in T3's catalog. When the search lands on the original account itself, the chain is over and nothing asks to go back. | `Running::relay_failed`, `State::host` |
| Limits | A relay run that fails (status `failed`; an interrupted run, the user's Stop, is never a failure) is read from T3's own record (`T3Cover::failure`: the run's `system_notice` and `error` items, and its reply when that is the provider's error, as 0.1.6 keeps them): "You've hit your weekly limit · resets 1pm", "… resets Nov 6 at 9am", the notice's `usage-limit:…:seven_day:<seconds>` id, "402 Payment Required: … balance exhausted". `agent::limits` makes it `Cause::Quota` with the window and reset ("Claude's weekly limit is used up; it resets at 13:00."). The account is kept in `state.json`'s `spent` until that reset (a prepaid balance: 6 hours; a limit without a reset: 30 minutes), left out of the search and the cards; its reset is the card's "back at 13:00" and the original's next check; background helpers on it move off (`Side::account_spent`). The spent thread gets no "stop" message: that started a run on the spent account. | `agent::limits`, `em::relay_cause` |
| Hand back | A passing probe (or `emergency_end` with a passing probe) asks the user, by voice and card, once no relay run is working; never by itself. "Back to …": the thread is told to wrap up, the token dies, the hold ends, and the agent's next turn is the `emergency-back-` notice: Heimdall is back, and the relay's conversation above (mirrored as it happened) is Heimdall's own; no summary. That turn answering ends it: Heimdall says "I'm back." Any failed turn in that trial puts the same thread back on duty with a new token. Input that reached the relay as it ended is passed to the agent as an `emergency-late-` notice that wakes it. | `Running::recover` / `finish` |
| Off | Agent off ends emergency mode: the thread is told to stop, its token is swapped for an end-only one for 2 min (`emergency_status` and `emergency_end`, on a gate of its own), the hold lifts without a turn, nothing is said. Quit is not Agent off: `state.json` keeps the emergency for the next launch. | `Running::stand_down`, `END_GRACE_MS` |

State survives Quit and a restart (`<data>/emergency/state.json`): the hold is back from the first start, the same thread
acts with a new token, and the agent is probed within 30 s. Log lines start with `emergency:` in `heimdall.log`; the
journal records `agent_emergency` (`on` with the cause, `released`).

## Boundaries

- The connection is docs/claude-relay.md's: a 0600 token file readable by the user's account while a relay is on; no file and no
  server otherwise.
- The relay's T3 runs never wake Heimdall: the controller talks to T3 directly, and the T3 observer never follows
  threads titled as relays (`t3::observer::RELAY_TITLES`).
- Pi itself crashing past its restart budget is the supervisor's red pill, not emergency mode.
- Known gaps: the probe sees a spent plan limit only on ChatGPT; elsewhere it checks the sign-in, so a spent prepaid
  balance costs one failing hand-back turn per probe. T3 offers no quota or plan health before a run, so a harness on
  a spent subscription is found by its first run failing (its limit and reset are then read from T3's record). A relay
  account's recovery is its live quota reading (`Tools/subscription_quota.py`): Grok and other accounts without one
  are never offered back by themselves. T3 closing while a thread relays shows as failed sends (retried
  every 30 s).

## Testing

No model inference. `agent::emergency::tests` (detector, harness order and the subscription skip, probe
times, the connection and context files, the first message); `agent::limits::tests` (three failures as T3
keeps them); `agent::sim::emergency_scenarios` (held input is context
and a relay, never a turn); `crates/agent/tests/mcp_scoped.rs`; `heimdall-emergency` lib tests (its `mcp` proxy: another thread is refused, the relay's thread gets the answers whole); `app emergency::tests`
(failover, the full toolset, relay mode, the mirror whole and through a restart, replies, Stop, the send arrow, early acceptance, the hold ordering, a
replaced controller, Agent off at launch then on, hand-back, no relay, Agent off, restart, the
day of failures (the true original, the limit and its reset on the card, no message into the spent thread, the announced card
and switch, the search landing on the original, Codex offered, the hand-over context up to the switch), and the
real supervisor with the fixture Pi on a spent quota: `provider-quota` beside its session). Supervisor tests need the
guardian binary: `cargo build -p agent --bins` first.
