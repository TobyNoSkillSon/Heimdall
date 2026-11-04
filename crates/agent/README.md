# Heimdall's built-in Pi agent

**Domain: agent harness.** Owns the one supervised Pi RPC process and its ownership (guardian, process registry),
the durable delivery ledger and gates (`delivery`), runtime pins (`runtime`), API providers and the Console sign-in
(`provider`, `console`), the MCP host and tool contract (`mcp`, `mcp_contract`, `plain`: every tool result as plain lines), sessions, transcript views and the
action ledger. App glue lives in `crates/app/src/bridge/`; T3 control moved to `crates/t3`.
**Interface:** `agent::Bridge<H: harness::Harness>`, `harness::{Pi, PiConfig}`, `delivery::{DeliveryGate, DeliveryPermit}`,
`mcp::ToolHost`, `provider::*`, `runtime::*`, `view::*`, `actions::Actions`, `guardian_mode`.
**Testing:** `cargo nextest run -p agent`, none over 5 s (fake Pi, local fake providers).
Ignored lab tests use the real pinned Pi or real API keys (`anthropic_lab`, `opencode_lab`, `chatgpt_lab`; env vars in each file).
The harness simulator (`src/sim/`, feature `sim`, always on in tests) runs lab/harness/SCENARIOS.md on a virtual clock
against an in-process Pi: `cargo test -p agent --lib sim::` (about 5 s: the scenarios, the
18 Oct 17:39–17:54 replay gate `s64_…`, 1,000 seeded random interleavings checked against the global properties —
`HEIMDALL_SIM_CASES=<n>` for a longer sweep — and the coverage map of all 64 scenario ids). `sim::night_scenarios`
adds talk-over at every phase of a turn, chunked speech, compaction and wake races and two
seeded sweeps of them; its `r4*`/`r9*` cases add replies and tool calls being written, the
restart bound, other speakers' lines and clicks during compactions, and two more sweeps. `sim::recovery_scenarios`
checks where a start's catch-up begins (after a compaction, a plain restart, a failed request,
hours off) and sweeps random starts for repeated lines; its `c7`–`c10` cases bring back what
the model never read after an idle compaction, a failed request or a crash, and word a refusal by whether a repair
ran. `sim::delivery_scenarios` checks one wake per utterance (two microphones, repeated wakes) and that
the user's request queued around a reload, compaction or crash is answered after the start, with a seeded sweep. `history` and `runtime/history-repair.test.mjs` cover the
repair of a history the provider refuses; `opencode_lab::a_refused_history_is_repaired_and_the_turn_answers` runs it
through the real pinned Pi.

Heimdall supervises one Pi RPC process, one logical session and the durable delivery ledger. The agent's model is
either a local OpenAI-compatible server's (`http://127.0.0.1:8100/v1`) or an added API model
(`agent_model` in settings; providers, defaults, budgets and thinking in `docs/models.md`). Recorded-versus-supplied
routing stays in the app pipeline.
Eligible observations persist locally; Pi has no no-query observation append. Each voice wake carries the assembled
labelled transcript and the pipeline's images. Transcript lines use `[Mic · <user name>] words` or `[Discord · <name>] words`,
under `Transcript:` (or `Observation:` for append-only adapters). Event ids, timestamps and language stay outside
model text. The durable envelope's `observation_ids` array identifies exactly the supplied rows across batching,
coalescing and retries (including observations referenced from existing context); archive send/reply attribution
and Pi transport records use that metadata. A successful append records each observation's `supplied` bit in the
spool: later wakes carry the instruction, images and a short reference to those observations, without their text.
Only adapters guaranteeing durable no-query context retention may opt in. Pi cannot append without querying,
so its wakes carry the locally spooled lines once. An uncertain append requires reconciliation before replay.
Legacy queued
rows and archived UI messages are decoded without rewriting their source records. Voice batches use at most half the advertised context in text bytes (8 MiB ceiling), leaving margin for the
stable system/tool prefix, images and response. Excess observations stay queued with an explicit backlog note.
Typed messages and notices are direct inputs and never acknowledge unprovided observations. The user's input joins the
running turn. While the turn has neither acted (no tool call) nor replied, and has not started writing either (no
text or tool-call block streaming), their new words restart it (`Harness::rejoin`: Pi aborts the request in flight, the
earlier deliveries complete without an answer of their own, and the new words go out at once, so one answer takes
both; otherwise a question said twice was answered twice because a steer waits for the request in
flight). The harness takes in the events Pi already wrote before it decides, so a reply or tool call that started
streaming is never cut; a turn restarts at most `MAX_RESTARTS` (2) times; and a wake the decider sent for someone
else's line (`Speaker::Other`: call or system audio) never restarts it. Once the turn has acted, voice wakes, follow-ups,
typed input and late screenshots use `streamingBehavior: "steer"` and enter the active run before its next model
call. Notices never steer (`Bridge::notice`, `notice.rs`): a
question answer, a T3 question or approval, an owned worker's result wait until the turn settles, coalesce into one
message (answers first), and at most one notice-only turn runs per 30 s; a notice older than 30 minutes becomes context.
A task's result (`terminal:` ids, `NoticeKind::Result`) and a scheduled wake are the exceptions: each starts one turn also
right after a start while the user is silent, does not age into context, and one still queued at a start goes out after it,
once (pi-exit, 21 Oct; `sim::result_scenarios`).
Fast-read results, worker progress, unheard speech and T3 connection state are context only: appended with
`heimdall_context`, no turn. Notice ids are deterministic and the same words within 24 hours are delivered once.
Every transcript line carries the clock time it was heard (`[17:52:38 Mic · <user name>] words`); a pending line older than
30 minutes stays in the archive instead of riding a wake. All harness policy reads time through `clock::ClockHandle`. New wakes exclude speech already supplied to
other unresolved deliveries, using their exact observation IDs rather than a highest-sequence cursor.
One wake per utterance (one line heard on two microphones woke the agent
twice otherwise): the same labelled words within 3 s are one utterance, so the second copy is kept settled with its reason
(`notice::HEARD_TWICE_MS`), and a wake whose utterance is already with the agent (that copy, or another wake carries
it), with nothing new and no image, starts no delivery. Words only a start's catch-up holds still get their wake. The app's pipeline drops another microphone's copy before the decider (`pipeline/heard_twice.rs`).
A successful settlement/receipt settles only that delivery's exact observations, preserving earlier unresolved
speech even when later wakes complete first. Error recovery and legacy queued retries resend their speech once
per attempt, marked `(resent after a delivery interruption; ignore repeated speech)`. Pi may retain the original
copy after an engine error; the marked resend favors instruction preservation over deduplication during recovery.
No context-retention query runs before a wake or steer. The durable `transcript_attempted` envelope field distinguishes
a fresh fallback queue from a retry; missing legacy evidence defaults to resending. Pi consumption events remain
available for transport diagnostics and turn settlement, but no observation consumption flag controls replay.
SQLite queues input only
when it cannot be admitted (engine downtime, Pi's own compaction, pending reload, rate limiting, or a transport
without steering). Heimdall's own compactions (`maintenance`, `Bridge::maintain`) never sit in front of the user: they
start only while the agent is idle and nobody has spoken to it or been answered for 30 s (a start counts), when the
history fills 85% of the request budget (`Limits::compact_at`; Pi's own in-turn check sits higher, at
most 90%: `docs/models.md` "Limits"), or when the provider's hour cache has gone cold on a history past the
break-even size (`harness::cold_cache_break_even`). On a provider with the hour cache (Anthropic), once the last request is 55 minutes old and the agent
is idle, `Bridge::keep_cache_warm` sends one short cache-refresh message (`maintenance::PING_TEXT`): its request reads
the cached history, which refreshes the hour, and the agent answers with a quick stall check or a silent `NO_REPLY`.
Pings stop 3 hours after the last real request, never go out with the Agent toggle off
(`bridge::startup::background_calls_allowed`), and a ping that cannot go out at once or fails is dropped: never
resent, queued or reported. After a start, until the user talks, the ping asks only for `NO_REPLY` (the restart message). OpenCode Go, ChatGPT
and the local engine get none (no hour cache Heimdall can keep alive). Nothing cancels a running compaction ("When the
agent is compacting I should not be able to interrupt you"; a cancelled summary still bills about 190K uncached input
tokens): their wakes, typed messages, late screenshots, clicks and notices queue and go out as it ends, and their Stop
sends Pi no `abort` while it compacts (a turn compacting inside its run stops as the compaction ends). A model switch
or new key waits for it too (`agent_restart_wait`); Agent off during that wait keeps the switch, and the next Agent on
starts the switched model. Agent off waits for a running compaction (`Stopping`, "Stopping after compaction…") as long
as the harness lets it run. Notices waiting for the user or for the notice gap do not hold one back. The Pi extension no longer compacts inside a wake's prompt. The durable envelope records `queue_reason`, and the journal records `agent_queue_fallback` once per
reason transition. Ambient fallback batches coalesce; typed entries remain ordering barriers and the backlog
flushes as steering as soon as Pi can accept it. This checkout has no live Claude Code or Codex adapters;
their old fixtures are historical evidence, not supported steering transports. Dry run cannot instantiate the live runtime, MCP server or model.

One conversation: every start (Agent on, a model switch, a reload, crash recovery, a relaunch or reinstall) resumes
the session file named by `agent/active-session.json`; only Edit and resend forks it into a new file that carries the
same history. A start passes `--provider/--model` for the selected model and requires Pi's `get_state` to report that
provider and model; Pi's CLI selection wins over the model recorded in a resumed session. Storage rotation past 32 MB
rewrites the same file to its header, the last compaction checkpoint and the kept tail.

Starts (lab/harness/EXPECTED.md E4): every way a fresh Pi begins serving — launch with the agent on, Agent on, a
model switch, a reload, crash or supervisor recovery — calls `Bridge::begin_start`. Everything pending from before is
history: queued wakes, typed text and notices (except open T3 questions, which wait for the user) are cancelled — but
the user's own wake or typed text queued within 5 minutes (`notice::START_RESUME_MS`; it waited for a pending reload,
Pi's compaction or the restart and never reached the model) stays queued with its speech and goes out after the start
context, so they get their answer; spool
rows that never reached the model become catch-up lines if heard within 30 minutes, else archive-only, whatever the
conversation's end or its last compaction (no summary holds what the model never read); a row whose words Pi already
read (a request that then failed: `in_history`, set when Pi reports it took the delivery in, or at recovery from Pi's
session file) stays out with its reason; a delivery
interrupted before any tool ran is not resent across a start (inside one process it is resent once, within two
minutes). Before any wake, one context-only `heimdall_context` message is added: the catch-up (archived speech the spool does
not hold, plus those leftovers; all speakers with clock times, newest 128 KB), a reload's reason, and last the restart
message ("Heimdall, you have just been restarted (or turned
on) at HH:MM. Everything above is historical context. Don't do anything unless User talks to you."). The archived
part of the catch-up begins where the conversation's own record ends (`Bridge::catchup_since`): the newest message in
the session (a failed request counts: Pi keeps its words) or the start of the last compaction that finished (recorded
in the spool at `compaction_start`), whichever is later, and never more than 30 minutes back; the header names the
earliest line. So nothing the history or a summary holds is sent again ("after the compaction … what we
need to send is any audio that happened after the compaction started"), and nothing the model never read is lost. If Pi's context
estimate plus the catch-up would pass the compaction threshold, Heimdall compacts first and adds it after (a wake
during that compaction waits for it; the context still goes in before the wake). Until
the user addresses Heimdall, no notice wakes the agent (worker results are context). A start context that cannot be added
is retried once on the next start, then dropped with a log line.

Reply path (`reply`): a settled turn's final text is what the user hears (Speaking on) or reads on the island; the
app routes it through the `speak` path (`mcp_actions::Host::say`) and sends the reply as written to the island's
pull-down (`UiUpdate::LastReply`). `speak` is for updates during the work; sentences already said in this run (by
`speak` or an earlier reply of the run, also re-split or inside a longer spoken sentence) are not said again.
`NO_REPLY` (or an empty reply, or the old "No request." habit) is a silent turn; aborted and failed replies say
nothing. The harness reports each finished reply (`stop`/`length`) at its own `turn_end` with Pi's `stop_reason`
(`heimdall_final_answer`): Pi keeps one run across steering, so a question added while the first answer streams
gets its own reply and the first is still said. Aborted and failed replies report at `agent_settled`; a run that
settles without any reply reports an empty aborted one, which closes the turn's reply state.
Tool log (`tool_log`): every tool call's outcome is one plain line in `<data>/logs/tools.log` (rotated at 1 MiB to
`tools.log.1`), written from Pi's `tool_execution_*` events, secrets masked; the prompt names the path
(`HEIMDALL_TOOL_LOG`).
Outbox (`actions`): notices to the agent fit its live message limit (`Actions::set_message_limit` at each start,
`notice_budget` for producers). A notice the agent still rejects as too long goes as its producer's short form (or
its first line and a pointer); it never blocks the notices behind it.
Scheduled wakes (`wakes`): `schedule_wake`/`list_wakes`/`cancel_wake` and `Heimdall --schedule-wake`
share `agent/wakes.sqlite`; a heartbeat beside the T3 watcher fires due wakes through the outbox as `selfwake:<id>`
notices (kind `scheduled`: a turn also right after a start, kept as a wake across a start), retries every undelivered
outbox notice every 30 s and logs one stuck after 2 minutes. Details in `docs/mcp.md` ("Scheduled wakes").

Prompt: the installed app always uses the prompt compiled into its build; only development and test binaries read
`prompt/heimdall.md` from the checkout (or a lab prompt).

Reloads: the reason ("Reloaded: …") is the line before the restart message in the replacement's start context, and
the reload (with its `reload_me` receipts) completes only when that replacement took the context. A failed
replacement writes nothing and its reload stays pending for the automatic restart. A pending reload left by an earlier
run is cleared at startup (its receipts completed) without a note. The Agent window always opens on the Pi conversation's settled history, agent on or
off; speech captured while off follows it.

## Pins and ownership

`runtime/package.json` selects exact Pi 1.4.0; `runtime/package-lock.json` commits the versions and SHA-512
integrities of every npm dependency. `runtime/node-lock.json` pins the official Apple Silicon macOS Node 26.8.1
archive and SHA-256. Runtime code reads those files; there is no second Rust version constant and no global
Node/Pi lookup. Node is owned too because relying on a global Node update would break the Pi pin's guarantee.

The runtime ships inside the signed app, in `Contents/Resources/agent-runtime`, and runs from there read-only:
nothing is downloaded, copied or repaired at launch. `heimdall-agent-runtime build` (called by `scripts/bundle.sh`)
verifies the Node archive against its pin, keeps only the `node` executable (local symbols stripped; no npm,
headers or docs), installs the lock with npm from that archive (`npm ci --omit=dev --ignore-scripts
--no-bin-links`), keeps only the packages something loads (Pi's CLI bundle has the provider SDKs compiled in; the
build rescans its imports and fails if one would not ship), drops source maps and type declarations, and adds
Heimdall's runtime files. The archive and npm cache stay in `target/agent-runtime`, so a change to Heimdall's own
files rebuilds offline. The bundle script signs `node` with the app identity, then seals the receipt: every file's
SHA-256 and the pin of the committed sources. Pre-flight and launch check that receipt and the exact Node and Pi
versions; a missing or damaged runtime says "Reinstall Heimdall". `scripts/verify_release_candidate.py` repeats
the check independently and confirms each shipped package is the locked version. Development and test binaries
(outside an app bundle) use `HEIMDALL_AGENT_RUNTIME`, else `target/agent-runtime/current`; the app ignores the
variable. Earlier builds' downloaded copies (`<data>/agent/runtime`, `runtime.previous`) are deleted at the first
launch of a bundled build, with one line in `heimdall.log`. No package upgrade happens implicitly. A deliberate Pi
upgrade changes the package pin and regenerates its lock; a Node upgrade changes the Node lock. Repeat protocol,
default-path and short live checks before adopting either.

Pi gets `PI_CODING_AGENT_DIR=<data>/agent/config`, an explicit session file and the home folder as its working directory.
It ignores project `.pi` resources through `--no-approve`, while loading the home folder's AGENTS/CLAUDE context
files. Its child environment excludes inherited provider credentials, proxies and global agent options; the installed
app's Pi receives no provider credential in its environment at all.
Automatic model-catalog refresh, telemetry and version checks are disabled. The managed model/provider,
thinking default (`runtime::THINKING`) and compaction reserves are regenerated from verified local-server metadata or the
selected API model's limits (`provider::ApiModel::limits`).

Thinking levels per model, and the on-the-wire rewrite of Off for Sonnet and Haiku 5.5, are in `docs/models.md`
("Thinking").
Other settings and extensions in the owned config survive reload. No `~/.pi` configuration is read or changed.

`crates/agent/prompt/heimdall.md` is the one system-prompt source. Launch/reload reads it into the owned
`APPEND_SYSTEM.md`; a packaged binary can use its compiled copy if the source checkout is absent. The coordinator
replaces the placeholder. A prompt upgrade must be built into a packaged binary if no checkout is available.
The relay (a T3 thread acting as the agent, docs/claude-relay.md) gets the same prompt inline, composed at build time
with `prompt/relay.md` and the skills index (`relay_prompt`); the checked-in `prompt/relay.generated.md` is guarded by
a test: `HEIMDALL_WRITE_RELAY_PROMPT=1 cargo test -p agent relay_prompt` regenerates it.

## API providers

Which providers exist, where each model's figures come from, the context budget and the limits Pi is given are in
`docs/models.md`; the menu's rows are in `crates/ui/README.md`. Anthropic Messages, OpenAI chat completions and OpenAI Responses on OpenCode Go run through Pi's built-in
`opencode-go` provider, which adds OpenCode's required `x-opencode-session` header; Pi 1.4.0's catalog serves them from
`https://opencode.ai/inference/go`. Pi models in the catalog get only `modelOverrides` (window, output); others get a
full definition on OpenCode's documented `https://opencode.ai/inference/go/{anthropic,openai/v1}` endpoints.
Google-endpoint and unknown models show as unavailable with the reason and cannot be added.

Connect (`console.rs`): OpenCode's Console device login, the one OpenCode's own apps use. It is undocumented and
uses OpenCode's public CLI client id (`opencode-cli`), so it can break when OpenCode changes it. Connect
OpenCode Go… posts `https://opencode.ai/console/auth/device/code`, the panel shows the code and the browser opens
the approval page; Heimdall polls `/auth/device/token` (honouring `slow_down`) until approved, cancelled or expired
(10 minutes), then reads `/api/orgs` (the token's organization, else the first by name) and `/api/v2/config` (the
account's OpenCode Go endpoint and headers, when it names them). The session (access, refresh, expiry, org) is
kept in the login Keychain (service `Heimdall API provider: OpenCode Go`, account `opencode-go-console`) and
refreshed with the same token URL when within 5 minutes of expiry; a 400 `invalid_grant` or Console 401 means
"OpenCode Go: sign in again". Pi sends the access token as `x-api-key` and `Authorization: Bearer`, with `x-org-id`.
The installed app's Pi gets it per request from `heimdall --provider-token opencode-go --data <dir>` (the same
signed binary, no secret in the command, refreshes under a file lock because refresh tokens rotate); lab builds
hand the current token over in `HEIMDALL_OPENCODE_TOKEN`. The Console's endpoint applies only to models its config
lists, and never a format-specific base (`…/openai/v1`, `…/anthropic`) to a model of the other format; other models
keep Pi's endpoint (as OpenCode does). Console header values are written as Pi literals (`$`→`$$`, leading
`!`→`$!`). The token command stays inside Pi's 10 s limit (3 s lock wait, 5 s refresh): when slow or offline with a
token that has not expired, it uses that token; a rotated token is saved before it is printed. A Console 403 is
"sign in with your organization's SSO", a failed sign-in shows under Connect… and on the Agent line, and a spent
code turns Open Browser into Try Again.

Use API Key… in the same panel is the fallback: it pre-fills only a key-shaped clipboard value, read when the key
field opens, verifies the key against `/v1/models` (401/403 = rejected or no subscription) and keeps it in the
Keychain (account `opencode-go`). One connection per provider: a sign-in replaces a key and the reverse. Only the
installed app at `~/Applications/Heimdall.app` touches the Keychain, with authenticated items skipped so no lookup
shows a prompt; other binaries keep credentials in memory or read `HEIMDALL_OPENCODE_API_KEY`.

**ChatGPT** (`chatgpt.rs`, provider id `openai`): Sign in with ChatGPT, OpenAI's token sharing, so the agent runs on
the user's ChatGPT plan. The flow, errors and tests are in `docs/chatgpt.md`.

Anthropic keys that are not scoped to a workspace (`sk-ant-usr-…`) are refused with 400 "must include the
anthropic-workspace-id header". The key check then lists the organisation's workspaces
(`/v1/organizations/workspaces?include_default=true`), picks the only active one or the Default one, and repeats
naming it. The id is kept beside the key (service `Heimdall API provider: Anthropic`, account `anthropic-workspace`;
the key's account is `anthropic`) and goes as `anthropic-workspace-id` on every request: the key check, the model list
and, through the provider headers in Pi's model file, every Pi request including compaction summaries. A key stored
without its workspace finds and keeps it on the next check. Other refusals reach the key panel in Anthropic's words. Pi's model file
holds the token command in the installed app (`heimdall --provider-token anthropic --data <dir>`, which prints the
pasted key from the Keychain) and an environment reference only in lab builds. Pasted keys and Console tokens reach
Pi through that command, so neither Pi's config, argv nor environment holds them (`runtime::pi_api_key`,
`harness::spawn_with`); the test `installed_pi_child_has_no_provider_key_or_mcp_token_in_env_or_argv` checks the child.

Until the selected API model's key works the agent waits
(`Status::WaitingProvider`) with the reason in the menu and the Agent window and messages queue; a missing or
rejected key waits for Connect (a key change restarts the agent), an unreachable provider is retried once a minute.

`runtime/provider-retry.mjs` handles OpenCode Go's HTTP responses: 429 with a reset (`retry-after`, `retry-after-ms`,
`x-ratelimit-reset`) up to 15 minutes waits and resends the same request (tool turns are not replayed), shown in the
Agent window and suspending the inactivity watchdog; a longer or unstated reset fails the turn once with
"usage limit reached" (and the reset time when stated), leaving only Pi's own two retries. 401/403 and 413 pass
through with a named reason for the menu; the Agent window explains Pi's raw provider errors the same way. A 401/403
is final: Pi 1.4.0 retries any error whose text says "upstream request failed", which is how OpenCode Go words every
rejected key, so the wrapper replaces that phrase with Heimdall's words and Pi fails the turn once. Each attempt must
start answering within two minutes; a started stream is never cut.

The app's `bridge::opencode_start_lab` (explicit, same variable) runs the production start path with real pinned Pi
and the production extension against a fake OpenCode reached through a lab Console sign-in: Haiku resuming a
conversation last held by the local model (same file, same id, old history in the request), and a catch-up that
compacts first, then survives a forced rotation. `real_conversation_compacts_before_the_first_haiku_answer` is the
paid acceptance on a copy of a real session (`HEIMDALL_REAL_SESSION`, `HEIMDALL_REAL_OPENCODE_TOKEN`).

`tests/opencode_lab.rs` (explicit, `HEIMDALL_OPENCODE_LAB`) drives the pinned Pi runtime, the production extension
and MCP against a local fake of OpenCode Go: a streamed Haiku tool call over Anthropic Messages, Haiku on a
Console sign-in (token in the environment, then from Pi's per-request command), a chat-completions tool call, compaction of a 121,298-token history on the switch to Haiku, a rejected key, and throttles with and
without a reset.

## RPC and recovery

Commands use unique IDs; successful prompt responses acknowledge acceptance, not completion. Pi's documented
`agent_settled` ends a session-level run; `agent_end` can precede retry, compaction and follow-up work. Each
transport prompt carries `[Heimdall delivery: <ledger id>]` in its user text, then the local date and time at sending
(`[Now: Tuesday 20 Oct 2026, 19:22]`, `clock::now_line`; the Agent window and edit-and-resend strip it). This leaves the system prefix and
tool declarations unchanged and makes message ownership recoverable. Every compaction Heimdall starts passes
`prompt/compaction.md` as Pi's `customInstructions`: no tool how-tos (the system prompt describes current tools), only
open items with their last-seen state and time, absolute dates; Pi's own threshold and overflow compactions run without it. Tool execution events mark possible
effects before completion. The extension persists completion/effects receipts in the session before settlement.
A successful persisted receipt reconciles a crash; missing or failed receipts require review. An uncertain
write or tool-bearing error is never blindly replayed. A 503/connection failure without possible effects is
returned to the durable queue and retried on the calm readiness tick.

The supervisor consumes the app engine manager’s readiness through one shared signal (Ready or Idle can receive a prompt).
Without a manager, isolated runs probe the local server's `/ready` every 10 seconds and before a prompt.
Verified `/v1/models` `max_model_len` sets the model context. It neither starts another engine manager nor
loads/downloads weights. Engine downtime does not spend the Pi restart budget. A process exit, invalid frame,
broken pipe or active-turn inactivity restarts Pi with durable backoff: three attempts in ten minutes
(`supervision::RestartBudget`), then an honest red pill; a restart that opens resets the failure count, so "The agent
could not recover" comes only when the budget refuses. An idle Pi is never a hang. Streaming progress resets the
inactivity deadline: 180 s in a turn, 600 s while a tool runs, and 15 minutes while a compaction runs
(`PiConfig::compaction_inactivity` = `delivery::COMPACTION_WAIT`), because Pi writes nothing while the model
summarises (27 Oct 03:24–03:46 the 180 s limit killed six Sol compactions of a 230K-token session in a row).
Explicit reload/Off-On retries can reset the budget.

A compaction that dies with its Pi (that limit, a crash, a stop wait cut at its limit) counts as failed, once per
process (`bridge/compaction_hold.rs`, journal `agent_compaction` `killed`): Heimdall's own compactions (idle, a start's
catch-up, session rotation) are held 10 minutes, then 20, and from the third kill until Pi completes a compaction
(a `compaction_end` with a result) or Heimdall relaunches. A successful reply does not lift a kill's hold. A
compaction Pi answers as failed holds them until a reply succeeds.

Stdout is LF-delimited JSON, including split UTF-8 and embedded Unicode separators. Frames are bounded at
64 MiB; partial EOF and malformed JSON fail closed. Stderr is drained as diagnostic evidence, not parsed as
RPC or counted as model progress. Writes are bounded and carry the original `DeliveryPermit` through archive
preparation. Off revokes the turn's epoch, stops the process group and its tool children, and preserves the
existing durable stop-intent contract. The stop epoch is saved in both intent and settings, so a valid queued
notice survives relaunch while Off→On still cancels older notices. The session lock is inherited only by the intended child, so a new
Heimdall cannot start a second Pi while an orphan still owns the file. An extension guard terminates the isolated
process group if its parent disappears. Shutdown closes stdin, then kills/reaps within a bound.

Stop (E3): the Agent window's Stop or the island's is handled by the harness, never asked of the model (the spoken
stop phrases left the command set on 29 Oct): speech stops, waiting notices become context, voice wakes queued before Stop are not delivered (their speech is
context for the user's next wake), Pi aborts and the turn settles without a resend. Only Heimdall aborts Pi, so an aborted
request (a Stop, or a rejoin for the user's new words) reads "Turn stopped" in the Agent window, never as an error
(`view.rs`). Provider refusals that a resend cannot
fix (a 400 other than the thinking signature, no answer within two minutes) fail the turn once with a plain-language
island line; the words ride the next wake without the rejected images (`notice::provider_rejection`).

A refused history is never a dead agent (20 Oct 19:20–19:24: `look` labelled a JPEG `image/png`, and every request
after it was refused, through a compaction, a reload and Agent off/on). Every image is labelled with the type its
bytes are (`history::sniff`: `Attachment::block`, every MCP tool result). When a run still ends with a 400 that names
a block of the history (`messages.N.…`), `runtime/history-repair.mjs` acts at Pi's `agent_before_settle` boundary
with Pi's own append-only entries, in the same run: every image breaking the provider's documented rules (type,
5 MB, 8000 px, 2000 px above 20 images) is relabelled or replaced by a one-line note (`context_edit`), the failed
attempt is omitted and the turn continues once; with nothing repairable, or a second refusal, the history restarts
from a summary (a compaction that keeps nothing: the last summary plus the conversation since as plain text, with a
marker where the request came in) followed by the request itself — every user message since the last completed reply,
in its own words without images, as a hidden `heimdall_request` message framed as what is answered now — so the
model's last message is the request, never a summary that says nothing in it is one (that
framing drew NO_REPLY and lost the words); the turn continues once more; a third refusal fails the turn. Every Pi start
runs the same image repair on the session before Pi reads it (`history::repair`, under the session lock; one backup
copy beside the file, `.repair-backup`), and first ends or cuts away a last line left without a line break by a stop
mid-write. Each step is a `heimdall_history_repair` entry (an Agent window row; "The model refuses this conversation
even after repair" when it gave up), one plain line in `heimdall.log` and an island notice. The extension writes
nothing to Pi's RPC output itself: Pi announces the entry (`entry_appended`) and the harness turns it into one
`heimdall_history_repair` event. The island says "still refuses this conversation after repair" only when the run's
repair acted.

## MCP and reload

Pi includes a native MCP client. Its built-in tool wrapper does not expose the per-call metadata the
Heimdall durable outbox needs. The shipped `runtime/heimdall.ts` therefore uses Pi's pinned `pi-mcp` library as
a small generic bridge. It negotiates exactly `2025-06-18`, discovers every tool, registers schemas/annotations
and forwards text/image results. Each call sends `_meta.heimdallActionId` equal to Pi's stable tool-call ID.
No mutation retry happens in the bridge. The MCP URL is in the launch environment; the bearer travels on a pipe
(`HEIMDALL_MCP_TOKEN_FD`) that the extension reads and closes, so it is in no process's environment or argv. The bridge contains no hard-coded tool list.

Reload is a process restart on the active session, after settlement; an explicit abort clears both Pi queues
before aborting. This rereads settings, extensions, the MCP list and system prompt. A short timestamped reason
is appended as session context without triggering an extra model turn. It is never inserted into the system
prefix. `Tools::reload` is the MCP callback; its caller must commit its receipt before invoking it and return
without waiting for the restart. See `docs/agent-ui-handoff.md` for exact callbacks and UI types.

Pi's compaction settings (`keepRecentTokens`, `reserveTokens`) come from `provider::Limits` (`docs/models.md`).
The hot session is checked at settlement. At 32 MiB the supervisor
requests compaction, then stops Pi and archives the entire original into the existing compressed archive before
atomically retaining the checkpoint and recent active branch. IDs, session identity, system/tool checkpoint and
recent messages survive. Unsafe/missing checkpoints never permit pruning. Originals are preserved; historical
tool-result contents are never edited in place (that would also invalidate a local server's prefix cache): a
repair of a refused history appends Pi's `context_edit` entries instead. A kept edit whose target was summarised
changes nothing the model sees and goes with the summarised history (it once made rotation refuse and loop
maintenance compactions); so does a line Pi cannot parse.

## Verification

Ordinary tests use scripted Pi processes and fake loopback HTTP only. The ignored app test
`bridge::live_pi_smoke::real_pinned_pi_splash_mcp_reload_and_automatic_recovery` is the explicit end check:
real pinned Pi, real already-ready local server, isolated data, one trivial prompt, one MCP call, one reload and an
owned Pi kill recovered by the actual app supervisor. It records first streamed-token latency, idle Pi RSS/CPU,
process/session continuity and the durable restart count. It never launches the installed app or requests
permissions. The smoke wrapper has a ten-minute model/run bound and cannot start/download an engine.

Primary references: the installed Pi 1.4.0 README and docs (`rpc`, `rpc-commands`, `json`, `configuration`,
`settings`, `compaction`, `session-format`, `extensions`, `mcp`, `models`), and their installed type/implementation
files. Pin sources: https://registry.npmjs.org/@earendil-works/pi-coding-agent and
https://nodejs.org/dist/v26.8.1/SHASUMS256.txt. Historical Claude smoke receipts remain as archival evidence;
no runtime path or test depends on those adapters.
