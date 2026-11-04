# The background call path and its backends

Every background job (island agent, plan lists, voice names, day timeline, suggestions, deep search) is a short,
stateless call through one path, `app::background_call` (`ask`, `ask_blocking`, `ask_tools_blocking`). Each call
carries its own full input; no job keeps a session, except the Goals shadow (below). Background
Tasks ▸ Model picks the backend, and no job knows which one ran.

## Backends

| Choice (`settings.json` `background_model.provider`) | Request | Billing, ledger |
|---|---|---|
| `anthropic` (API key) | Messages API, one-hour prompt cache, structured output | Anthropic list prices, $0.50 a run |
| `openai` (Sign in with ChatGPT, the Codex subscription) | Responses, streamed | rounds at $0 |
| `opencode-go`, `openrouter`, Custom, Own Engine | Messages or chat completions, as the model's catalog entry says | rounds at $0 |
| `t3:<instance>` (a Claude Code instance in T3, e.g. `t3:claudeAgent_claude_code_2`) | one short-lived T3 thread per call (below) | one line at $0 |

`background_model::endpoint_for` resolves the choice; a provider without its credential gets no call. While the
choice is spent or unavailable the task's next calls go to a fallback (below). Every call passes `background_gate`
first: Agent off or emergency mode's pause stop it quietly. Each run is one line in
`agent/background-runs.jsonl`; a T3 run's `model` is `t3:<instance>/<model>`.

## A task's own model

A task may run its own model instead of Background Tasks ▸ Model (`settings.json` `task_models`: task, provider,
id; `UiAction::SetTaskModel`, an empty provider puts the task back on the shared model). The
island agent defaults to Claude Haiku 5.5 through T3 on the second Claude Code account
(`ui::ISLANDS_DEFAULT`, `t3:claudeAgent_claude_code_2`), while the saved Background Tasks ▸ Model keeps driving the
other jobs. Every task reads its own (`background_model::reads_own_model`): Islands, Plan Lists, Voice Names, Day
Timeline and Suggestions (each with its own model pick); a task without an entry runs the
shared model. Suggestions is a tool loop, so a T3 pick for it is refused (`background_model::task_accepts`, in the
menu action and the settings tool). Deep search is not a task and stays on the shared model. A settings file from before the
split (no `task_models`) whose Islands was off also turns Plan Lists off, once, because Islands off used to stop the
plan refresher; there are no task models by default. The field is always written, so a task put back on the
shared model stays there. A job with its own model reads `background_model::task_endpoint(task)` (the island agent:
`task_endpoint(Task::Islands)`); the model's thinking level applies as for any model. The settings tool sets it as
`background_model` with the task as item (`shared` to go back).

## When a backend runs out: the fallback (`background_fallback.rs`)

A simpler form of the main agent's emergency mode. When a call fails because a quota or limit ran
out (a usage-limit reply, a 429, Claude's "usage limit reached", Codex's plan limit, spent credit) or the backend is
unavailable (T3 not running or not connected, a refused connection, a 5xx), the task's next call goes to the next
backend, and it keeps working:

1. the same Claude model on another Claude Code account (T3's Claude instances, `claudeAgent` and
   `claudeAgent_claude_code_2`; with more than one left, the one whose quota renews soonest first, by
   `agent::accounts::earliest_reset_first`; an account the quota reading shows spent is skipped);
2. Codex through the ChatGPT sign-in (Responses), on the shared model when that is a Codex model, else a default ChatGPT model;
   never a Codex T3 instance;
3. OpenCode Go (the shared model when it lists it, else its default), then
   the Anthropic API key last, only when the user allowed a paid fallback: `settings.json` `background_fallback_paid`
   (false by default; `UiAction::SetBackgroundFallbackPaid`, no menu row; the settings tool sets it as
   `background_model` with item `paid_fallback`).

A pick that is not a Claude model takes Claude Haiku 5.5 on the Claude accounts and the API key. A tool loop
(Suggestions) has no T3 step. Only a pick on a cloud subscription or API falls back (a T3 instance, the ChatGPT sign-in,
OpenCode Go, the Anthropic API: `background_fallback::falls_back`); Custom (OpenAI- or
Anthropic-compatible) and Own Engine never do, because they may be on this Mac or the private network and a local
choice never sends the plan or a transcript to the cloud. A step that is not configured (no credential, T3 not
connected) is skipped; nothing usable: no call. A pick that was never connected (no key, signed out) makes no call, as
before; only T3 not connected counts as unavailable. Deep search and the shared `background_model::endpoint` /
`tool_endpoint` are not tasks: they stay on Background Tasks ▸ Model, with no fallback and no sign.

Rules:
- Decided when a job resolves its endpoint (`background_model::task_endpoint`, `endpoint`, `tool_endpoint` call
  `background_fallback::route`), right before each call: a call that started on a backend finishes or fails there; the
  next call goes to the fallback. Every call stays one-off.
- Spent or down is `background_gate`'s episodes, marked where a call fails, by one classification for every wire
  (`background_gate::fault`): spent for a usage or rate limit (429), a plan limit or spent credit; down for overloaded or
  a 5xx, no connection, T3 not running (the episode `t3:*`, which no provider id can name; a bare "t3:" is the default
  instance) or the sign-in lost (401, "please run /login"). A job's own failure (a 400, a prompt too long, a malformed
  answer, a refused T3 launch) changes no backend and moves no task. A backend is skipped until its stated reset, else
  until a probe is due: 10 minutes after it failed, doubling with each failed probe up to an hour (failures of calls
  already running inside the wait renew nothing), read between calls only. The task goes back to its pick at the first
  call after the pick is usable again. Down counts for routing only: a down pick with nowhere else to go is tried again
  on the next pass as before the fallback, and its calls are never refused (a whole-request timeout marks nothing; the
  logs say "down", "unreachable" or "overloaded"). Only a spent plan or credit refuses calls ("… usage is spent").
- The accounts' quota is read by `Tools/subscription_quota.py` (when that script exists; read-only, no prompt) only when the
  choice depends on it (`background_fallback::quota_matters`: the step chosen is a Claude account with another one
  beside it, or Codex), at most every 20 minutes (`accounts::FALLBACK_TTL`), on its own thread; no call waits for it.
  The script runs in its own process group, killed whole at the timeout, so no `claude` or `codex` process is left.
- Each move is logged once ("Background tasks · Islands: t3:claudeAgent_claude_code_2/claude-haiku-5-5 is spent or
  unavailable until 09:00; its calls run on t3:claudeAgent/claude-haiku-5-5 (another Claude Code account) until it is
  back"), and so is the return. `UiUpdate::BackgroundRoutes` lists the tasks off their pick with what they run on now,
  sent whenever that changes (an empty list when all are back), from inside the one lock that changes it, so lists
  arrive in order. A route clears without waiting for the task's next call when the task is turned off, its pick
  changes (every settings change) or its pick answers again (`background_fallback::refresh_routes`). No card, no
  failure notice.
- Emergency mode's pause (`background_gate::emergency_pause`) is unchanged: while the main agent is covered no
  background job runs. Outside it a spent T3 instance no longer stops its task: the task falls back.

## Merging with the relay's account selection

One reader, one reading: `agent::accounts` (`Script`, `Board`) is the only
quota source; the relay's own Rust reader (`app::subscription_quota`) is gone. The board is made once in the pipeline
(`background_fallback::set_quota_board`) with T3's Claude instances and their folders
(`accounts::config_dirs_from_t3`, read again at every run), and the relay reads it through `background_fallback::quota_board`
(`emergency::T3Cover::quotas`, `Board::fresh`: at most a minute old, up to 20 s wait, none after a failed read), converted by
`agent::routing::quotas_from`.

The rankings stay two on purpose: the background fallback takes `accounts::earliest_reset_first` (any window's soonest
reset), the relay `agent::routing`'s rule (exhausted never, known before unknown, a
5-hour window at 95% last, weekly reset first, then 5-hour). Both rank the same reading.

## Thinking level per model

Background Tasks ▸ Model lets the user pick a level for each model from that model's own list
(`settings.json` `background_efforts`: provider, id, level; `UiAction::SetBackgroundEffort`; an empty level removes
the entry). A model with a level runs every job at it; a model without one runs each job's own default
(`background_call::with_effort`). The level reaches each format's own field: Messages `output_config.effort`,
Responses `reasoning.effort` and chat completions `reasoning_effort` (mapped to the nearest level the model lists),
and T3 `modelSelection.options.effort` (or `reasoningEffort`, as T3's catalog names the option; a level the model does
not list falls back to its default). The menu's "off" is each format's least: "low" on Messages and T3, "minimal" on
the OpenAI formats. The prompt hash includes the effort, so each level keeps its own prompt cache.

## A rolling session (Goals only)

The Goals job is like a rolling context, like the agent's own but on a much cheaper model, so it can shadow the agent
completely because the calls hit the cache. This reverses the stateless rule for this one job only;
every other job leaves both fields empty and sends the same bytes as before (`a_job_without_history_sends_the_same_bytes_as_before_rolling_sessions`
pins the day timeline's request on every format).

A `Call` carries `history: Vec<Turn>` (the earlier turns, oldest first, each `Turn { user: Prompt, assistant: String }`:
the message as sent and the answer's text as it came back) and `session: Option<String>` (the current segment's key,
"goals-<segment id>"). The job appends each answered turn and starts a new key when it compacts; `ask` sends the history
before the new prompt, so the provider's prefix cache serves all but the newest turn.

- Messages: for each turn its user blocks (unmarked) and its answer, then the new user message. Breakpoints: the system;
  the end of the last earlier answer (the previous run's prefix, read from cache); the new message's last stable
  stretch (the next run reads it). Three of the API's four, one-hour TTL. The budget counts the history as cached
  prefix (a cold one-hour write at worst); the $0.50 limit stands.
- Responses and chat completions: the same turns as input items or messages, in the same order every call; Responses'
  `prompt_cache_key` (and with it the `session_id` / `x-client-request-id` headers) is `heimdall-<session>`, so every
  turn of a segment routes to the cache that holds its prefix.
- T3: one thread per session key. The first call launches it (title `[heimdall-bg] <job> <session> <tag>`, the
  background project, `approval-required`, the no-tools instruction) with the whole history in its first message; each
  later call sends only the new input into it (`t3_thread_send`, mode `auto`) and follows that run as any call, reading
  only that turn's items. The thread stays open after an answer; anything else (a tool, no JSON, a failed run, the
  deadline) interrupts and archives it. A remembered thread that has not answered exactly `history.len()` turns on the
  same instance, model, effort, prompt and project (a restart, a fallback that ran elsewhere, a failed turn), or that
  refuses the send, is archived and a fresh launch carries the whole history. A job's new session key archives its old
  thread; `background_call::t3::end_session(key)` does so on the job's word. The map is in memory only: a restart
  relaunches once, and before each session launch the job's session threads no session holds (`[heimdall-bg] <job>
  <key> <tag>`: a restart's, or one whose archive failed while T3 was away) are archived: at most five a sweep, within
  one cleanup deadline (10 s), skipping those T3 lists as archived and those this process archived (T3's list has no
  archived filter), so the launch keeps its budget. A send T3 queued without a
  run id never takes the previous turn's finished run for the new one.
- Fallback is unchanged: a call that moves to another backend sends the whole history there (a cold cache once), and
  its T3 thread, now a turn behind, is replaced when the task comes back.
- Usage and the ledger are as for any call: one line a call, cache reads and writes as the provider reports them.

## A call on T3 (`background_t3.rs`)

1. The thread lives in the T3 project "Heimdall background", an empty folder `background-t3` in Heimdall's data
   folder, found by its folder or made once. No project's instructions reach the model, and the user's projects stay clean.
2. `t3_thread_launch` on the instance: title `[heimdall-bg] <job> <tag>` (the tag is the start of the launch's
   `clientRequestId`, so a launch whose answer was lost, by a timeout or an answer without its thread id, is still
   found by its title, interrupted and archived), `modelSelection {instanceId, model, options}`,
   runtime mode `approval-required` (T3's most restrictive), workspace root. The one message carries the instruction
   to use no tools and answer with only the JSON, then the job's system prompt, the JSON schema and the whole prompt,
   stable parts first.
3. `t3_thread_wait` in 15-second slices (a wait that answers "idle" before the launched run has started keeps
   waiting for that run); between slices the activity is read. A tool item, or the thread waiting on
   an approval or a question, interrupts the run and fails the call (`Stopped`). T3's own `system_notice` and `error`
   items are not tools: they are the run's failure. A spent account shows "Claude usage limit reached …" as a notice
   (its id carrying the reset), and that marks the instance spent until then (`agent::limits`), so the task falls
   back; it once read as a tool request and Goals and the island agent stayed on a spent account for five hours. When the main agent's relay finds an account spent, background helpers on it move
   off too (`emergency::Side::account_spent`). Nothing ever waits for the user:
   Heimdall's permission hook declines a prompt in a thread Heimdall launched (`heimdall-emergency hook permission`,
   which finds the launch the client recorded; background launches have their own list,
   `approvals/launched-background`, so they never push a long-running task out of the main one), and the T3 observer
   never follows a `[heimdall-bg]` thread, so no
   call wakes Heimdall's agent.
4. The last assistant message is read in full. The JSON is cut out of it (a fence or prose around it is dropped) and
   checked against the schema's top-level shape: its type, the required keys, and each present property's type.
5. The thread is archived whatever happened. T3 has no thread delete, so archived threads stay in that project.
6. The work has `TIMEOUT` less 30 s; a run past it is interrupted and counted as timed out. The interrupt and the
   archive get 10 s each of their own, outside the work's budget, so the whole call stays within `TIMEOUT` (600 s).
   A T3 call whose own deadline runs out never drops the app's shared T3 connection (`t3::client::TIMEOUT`): only a
   real transport or protocol failure reconnects.

Outcomes: an answer, or `Stopped` for a tool request, no text, no JSON, the wrong shape or the deadline (the job
treats that input as seen). A harness error reply (`agent::emergency::harness_error_reply`: "API Error …", "Claude AI
usage limit reached|…") or a failed run is a plain error, retried next pass. A usage-limit reply starts the instance's
quota episode under provider id `t3:<instance>`, with the reset Claude states, and any other harness error marks the
instance down; T3 not answering marks every instance down. The next answer ends it; meanwhile the task falls back.
Emergency mode pauses T3 calls like every background request.

Not on T3: tool loops (`ask_tools_blocking`: the suggester, deep search). Their tools run inside Heimdall and a T3
thread cannot call them; a loop typed out as text would be a fake, so the call fails with `NoToolsOnT3` ("not on
T3") and the job should be pointed at an API model.

### Which instances are offered

`UiUpdate::T3Instances` comes from T3's live `orchestrator_capabilities`, sent at the moments the relay's model list
is (`Msg::RelayModels`, when the menu opens): every enabled instance that can run a thread in `approval-required`
and lists models, with each model's effort values and default. Every
usable instance is listed. A choice is checked
against the last published list (`background_t3::choice_ok`).

### Checks

`cargo test -p app background_t3` runs the T3 path against a scripted T3 (launch, wait, reply JSON, archive; a rolling
session's launch with history, its next turn sent alone into the same thread, a mismatch or refused send relaunching,
`end_session` and a new key archiving, a failed turn interrupted and archived; a cut-off
reply; harness error and quota; a tool request and an approval wait; the deadline; malformed answers; a failed run;
efforts; tool loops; Agent off and emergency mode; the instances from a captured catalog; choices). The opt-in live
check `background_t3::tests::live_one_tiny_haiku_call_on_claude_code_2` (`-- --ignored`) runs one tiny Haiku call on
`claudeAgent_claude_code_2` with a 15-minute T3 session of its own (`t3 auth session issue`, revoked afterwards) and
a temporary project it deletes, so the running app's connection is never touched.
