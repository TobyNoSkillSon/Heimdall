# Heimdall's MCP tools

Heimdall serves one MCP server to its own agent (Pi). This doc is that server's contract: where the code is, the
rules every tool follows, the tool families and where each one's details live, the scheduled-wake tools in full,
and the reasons behind the design. Checked against the code.

Each tool's name, description and input schema live in the code, not here: `tools()` in
`crates/agent/src/mcp_contract.rs` (35 tools). The snapshot is
`crates/agent/tests/fixtures/mcp-v2-schema.json`; `MCP_SCHEMA_WRITE=1 cargo test -p agent --test mcp_v2
full_catalog_schema_snapshot` rewrites it from the catalog, and the diff shows what changed.

## Where it lives

| Part | Code |
|---|---|
| Server: HTTP, auth, JSON-RPC, dispatch, result size check | `agent::mcp::McpServer` (`crates/agent/src/mcp.rs`) |
| Catalog and argument validator (`validate_tool`, no coercion) | `crates/agent/src/mcp_contract.rs` |
| Results as plain lines | `agent::plain` (`crates/agent/src/plain.rs`) |
| What each tool does on the Mac (`ToolHost`) | `bridge::Tools` (`crates/app/src/bridge/tools.rs`) |
| Durable receipts, replay and the notice outbox | `agent::actions::Actions`, `<data>/agent/mcp-actions.sqlite` (`crates/app/src/mcp_actions.rs`) |
| Pi's side: one Pi tool per catalog entry, named `mcp__heimdall__<name>` | `crates/agent/runtime/heimdall.ts`; see `crates/agent/README.md`, "MCP and reload" |
| Every call and its outcome, one line each | `<data>/logs/tools.log` (`agent::tool_log`), named to the agent as `HEIMDALL_TOOL_LOG` |
| The cover agent's scoped server (a token that reaches only some tools) | `agent::mcp::Scope`; `docs/emergency.md` |

## The wire

- Loopback only: `127.0.0.1` on an ephemeral port, path `/mcp`, POST, HTTP/1. At most 16 connections, 75 s each,
  128 KB per request.
- A fresh bearer per server generation. Pi gets the URL in its environment and the bearer on a pipe
  (`HEIMDALL_MCP_TOKEN_FD`), so the token is in no process's environment or argv. Loopback alone is no
  authentication: any process of the user's account can reach the port.
- Any `Origin` header is refused (403), a wrong `Host` is 421; the only client is the non-browser bridge.
- One dialect, MCP `2025-06-18`, the revision the pinned `pi-mcp` client speaks (`heimdall.ts` refuses any other).
  `initialize` must ask for it; every later request carries `MCP-Protocol-Version`. Requests are JSON and accept
  both `application/json` and `text/event-stream`.
- A bad envelope, an unknown method or an unknown tool is a JSON-RPC error. A valid call that fails is a result
  with `isError: true` and one line of text, at most 1,024 characters.

## Rules every tool follows

- **Strict input.** Every object rejects unknown fields; strings, arrays and numbers have bounds; closed choices are
  enums. A wrong type is an error, never a silent default. A rule the schema cannot express is checked in the
  handler and answered with the field to fix.
- **Errors name the fix.** A tool error starts with a code and says the next step:
  `INVALID_ARGUMENT: <field>: …`, `CAPTURE_UNAVAILABLE`, `INDEX_UNAVAILABLE`, `CURSOR_EXPIRED`, `OUTPUT_TOO_LARGE`,
  `REPLAY_CONFLICT`, `CONFIRM_REQUIRED`, and the rest (`grep -rhoE '"[A-Z][A-Z_]{4,}: ' crates/` lists them). An
  unavailable source is an error, never zero hits; no hits is an empty success.
- **Plain lines out.** Every result reaches the model as short plain lines, never raw JSON.
  `agent::plain` converts any JSON text block at the boundary and never shortens content; `timeline_search` hits,
  `screen_frames` metadata and the T3 tools have their own one-line forms. Images go as MCP image blocks labelled
  from their own bytes.
- **Bounded pages.** Text pages are at most 8 KB (`TEXT_BYTES`); a result larger than 8 MB of images plus that is
  `OUTPUT_TOO_LARGE`. A long item is continued by its id and a cursor, never cut. Inter-agent replies arrive whole,
  continued by a read call when they do not fit.
- **Honest receipts.** An action answers with what actually happened: speech queued is not played, a reload
  scheduled is not a restart done, a notice shown is not read.
- **Nothing invented.** A result never fabricates a total count to fill a field; a missing speaker or app is said
  as unknown.
- **Annotations are hints, not permissions.** A read-only or idempotent annotation grants nothing; a tool is marked
  idempotent only when replaying the same action id really deduplicates its effect.
- **One action identity.** The bridge sends each call's Pi tool-call id as `_meta.heimdallActionId`. A replay of
  the same id returns the first receipt; the same id with different arguments is `REPLAY_CONFLICT`.
- **Agent off wins.** Every call holds a delivery permit (`agent::delivery::DeliveryGate`). Agent off, or the
  server's retirement at a reload, revokes it: no result bytes are delivered after that, even for work already done.
  A call that writes Heimdall's own state (`writes_state`: plan and islands files, bug notes, wakes)
  runs to its end once admitted, and its answer is still withheld. Agent off cannot undo a
  shell command or T3 work already dispatched.
- **Observations are not orders.** Screen contents, app audio and transcripts are evidence with their source,
  speaker and time; authority comes only from the user's words and the assembled turn.

## Changing the catalog

The tool list sits in the model's prompt prefix. Its order and text stay fixed for a Pi process's life, and a new
tool family is appended at the end (`tools()` says so at each `extend`), so an unchanged prefix keeps the provider's
prompt cache. A changed description or schema changes the prefix even when the name stays.

For a new or changed tool:

1. Say which outcome of the user's it serves and why no existing tool covers it. Prefer one tool per distinct purpose;
   two operations on one thing share a tool through an `action` enum.
2. Write a short description that settles the model's actual choice: what it does, what its answer means (queued or
   done), what to do next. Put field limits in the schema.
3. Define success, empty, unavailable, busy and failed answers, the page budget and the continuation, the side
   effect and its replay, and whether it `writes_state`.
4. Add the schema to the snapshot, a validation case in `crates/agent/tests/mcp_v2.rs`, and a fake-host case for
   its answer and Agent off. Keep the default tests free of model inference, the microphone and Screen Recording.
5. The catalog is compiled into the app, so a change reaches the user with the next installed build; the bridge has no
   hard-coded list and Pi lists the new catalog at its first start on that build.

## Tool families

| Family | Tools | Details |
|---|---|---|
| Talking | `speak` (the turn's final reply takes the same path, `agent::reply`) | `crates/agent/README.md`, `crates/voicebank/README.md` |
| The archive | `timeline_search`, `screen_frames`, `archive_clip` | below; the index behind it: `docs/chunk-index.md`; clips: `docs/archive-clip.md` |
| The island's pull-down | `island_notice`, `ask`, `ask_close`, `fast_read` | `lab/ask-card/CONTRACT.md`, `crates/rsvp/README.md`, `crates/ui/src/island/README.md` |
| The user's screen | `look`, `point`, `drawing_mode` | `crates/app/src/drawing.rs`, `crates/ui/README.md`; the point grammar is `POINT_GUIDE` in `mcp_contract.rs` |
| Floating islands and the plan | `islands_put`, `islands_view`, `islands_remove`, `plan_edit`, `refresh_plan` | `docs/islands-capabilities.md`, below |
| Scheduled wakes | `schedule_wake`, `list_wakes`, `cancel_wake` | below |
| T3 Code | `delegate_task`, `task_status`, `task_cancel`, `t3_thread_list`, `t3_thread_read`, `t3_thread_send`, `t3_tools`, `t3_call` | `docs/t3-orchestration.md` |
| Bugs | `bug_note` | `docs/bugs.md` |
| Voices and guests | `name_voice`, `listen_to` | `docs/speakers.md`, `docs/personalise.md` |
| Settings | `settings`, `personalise` (older name) | `docs/personalise.md`, `docs/settings.md` |
| Itself | `reload_me` | `crates/agent/README.md`, "MCP and reload" |

### The archive tools: what the schema does not say

Code: `crates/app/src/mcp_timeline.rs` (its module doc is the full account), `crates/app/src/index.rs`.

- `timeline_search` answers one short line per distinct moment ("a lot more token
  efficient"): the rows of one moment merge, a 45-second window merges with the utterance it holds, and consecutive
  screen states of one app merge. Each hit has a short `[handle]` (`h3`; the newest 8,192 are kept) that `id` takes,
  and so do `screen_frames.chunk_id` and `island_notice`'s `archive_id` (`resolve_handles`).
  `output`: `full` adds whole texts and ids, `frames` the screenshots, `clip` a movie of the hits.
- A first page needs a `query`, a `vector`, a time boundary or tags. `id` and `cursor` each go alone. A cursor is
  bound to its query, the archive watermark and the caller's delivery permit; an expired or retired one is
  `CURSOR_EXPIRED`: repeat the query. At most 512 live cursors (`BUSY`).
- `vector: {"query": "…"}` searches by meaning with the archive's own embeddings; hits stop at a similarity floor.
  Without embeddings it fails; it never falls back to literal search. A store not ready is `INDEX_UNAVAILABLE`,
  never zero hits.
- `screen_frames`: `at` excludes `chunk_id`; the current frame takes `count` 1; a time with no committed frame in its
  ten-second lookup window is `FRAME_NOT_FOUND`, never an old image. No screen store is `CAPTURE_UNAVAILABLE`.

### Floating islands: the user's clicks

Code: `crates/app/src/islands/`; the helper is the bundled `Contents/Helpers/FloatingIslands.app`, supervised by
Heimdall. What an island can show: `docs/islands-capabilities.md`.

The user's clicks come back as one notice line each. `islands-answer:` ids wake the agent like an `ask` click
(approvals, buttons, list, toggle, slider); `islands-context:` ids ride along with the next turn (ticks, opens,
closes, thread answers). An approval moves its item out of Waiting (Yes: Running, Later: Discussed, No: off the
plan); a tick sets `done`. An island given `thread` follows that T3 thread (state, attention, its question as `t3q:`
buttons) until it settles, kept in `islands-bindings.json`.

## Scheduled wakes

"You should be able to put a schedule on yourself, when to wake yourself up … When I tell you 'at
seven, do this', you receive that notification later". Code: `crates/agent/src/wakes.rs`; the schedule is
`<data>/agent/wakes.sqlite`.

| Tool | Input | Result |
|---|---|---|
| `schedule_wake` | `message` (1–2,000 characters) and either `at` (`07:00` = the next 07:00, local; `2026-10-23 07:00` = that day) or `in_minutes` (1–43,200) | `Wake wake-3f2a1c set for 07:00 tomorrow (in 5 h 8 min).` Past times and anything over 30 days ahead are refused; at most 100 pending. |
| `list_wakes` | none | One line per pending wake, soonest first: `wake-3f2a1c · 07:00 tomorrow · Check the TTS task`, or `No wakes scheduled.` |
| `cancel_wake` | `id` | `Wake wake-3f2a1c for 07:00 cancelled.`; repeating is harmless; a fired one says so; an unknown id is `WAKE_NOT_FOUND`. |

Local time is wall-clock time on the due day: a DST change between scheduling and firing does not move it; a time
that occurs twice is the first one, a skipped time is the first minute after the gap.

A heartbeat runs beside the T3 watcher while the agent runtime runs. It sleeps until the next due wake, at most 30 s,
and wakes at once when a wake is scheduled or cancelled (an idle pass costs about 15 µs, measured: about 40 ms of CPU
a day). Each pass, while the agent is on:

1. claims due wakes; one more than 2 minutes past due is marked late, so a wake due while Heimdall or the agent was
   off arrives once at the next start, with its original time;
2. puts each into the action outbox as `selfwake:<id>` and re-arms one an Agent Off/On dropped before delivery (same
   id, so the agent's deduplication keeps it to one delivery);
3. drains the outbox: every undelivered notice, the T3 watcher's included, is retried every 30 s; one still
   undelivered after 2 minutes is logged as stuck, once;
4. marks the wakes the agent took.

`Wakes::add_check(name, check)` runs another check on every pass before the drain; a notice it enqueues under a
deterministic id is delivered in the same pass.

The agent receives `Scheduled wake wake-3f2a1c: you set it at 01:52 for 07:00.` and the note, or, from the command
line, `Scheduled message wake-… from a local agent (not User), …`. Its notice kind is `scheduled`: it starts a turn
even right after a start, it is not turned into context after waiting 30 minutes behind a turn, and one queued but
unsent at a restart stays a wake. Like every notice it waits for a running turn and the 30-second gap between
notice-only turns.

Log lines in `heimdall.log`: `Scheduled wake <id> set for 07:00.`, `… fired: due 07:00.` (or `…, late (now 08:13).`),
`… delivered to the agent.`, `… re-sent after an agent restart dropped it.`, `Heartbeat: notice <id> undelivered for
2 min; retrying every 30 s.`

From outside the app (agents only), the Heimdall binary writes a wake into the same schedule, safe while the app
runs, prints one line and exits (status 2 with a one-line error):

```
Heimdall --schedule-wake [--data-dir DIR] (--in SECONDS | --at HH:MM) --message TEXT [--id OPERATION]
```

`DIR` defaults to `~/Library/Application Support/Heimdall`; the installed binary is
`~/Applications/Heimdall.app/Contents/MacOS/Heimdall`. `--in 0` is due at once; `--id` makes a retried command the
same wake. The running app fires it within 30 s of its time; a stopped app at its next start, marked late.

## Tests

No model inference in any of these.

- `crates/agent/tests/mcp.rs`: loopback auth, Origin, protocol and envelope errors, unknown tools, Agent off and
  Off → On cancelling pending work, retirement never delivering a private result, plain-line results, a state write
  finishing through Agent off.
- `crates/agent/tests/mcp_v2.rs`: the schema snapshot, every tool's validation (unknown keys, wrong types, blank
  text, bounds, dates, tags, Unicode), receipts, replay and reload, ask cards, wakes, bug notes, settings.
- `crates/agent/tests/mcp_scoped.rs`: the cover agent's token reaches its tools only and dies with its server.
- `agent::mcp` unit tests: a ready response is suppressed at body poll after retirement or Off → On.

Model-facing checks run through Pi on the shipped model, with Pi's own tools in the roster (they compete with
Heimdall's). Correct tool choice and a correct answer are separate measurements from a call that returns 200.

## Why it is built this way

From the design research (PostHog's MCP service, GitHub, Cloudflare and Sentry MCP, Anthropic's and OpenAI's
tool-writing guides, the MCP specification). Only the conclusions that still shape the code are kept.

- **Direct typed tools, discovery only for a large surface.** PostHog routes 1,000+ tools through one `exec`
  dispatcher with search and schema verbs; Cloudflare's API server searches before it executes. That pays for a
  huge catalog and costs extra steps on a small one. Heimdall's own tools are direct; T3's catalog of 80+ tools is
  the one large surface, so it alone goes through discovery (`t3_tools`, `t3_call`).
- **One representation per result.** PostHog keeps UI data out of the model's text and found that duplicated rows
  in several channels waste context. Heimdall sends plain lines only.
- **Errors steer the next call.** PostHog's per-endpoint recovery hints (shorter window, cheaper count call) reduce
  repeated failures; silent argument repair hides them. Heimdall names the field and the fix and never repairs.
- **Restrictions at dispatch.** GitHub's MCP enforces read-only and scope rules when a call arrives, not only by
  hiding definitions; the cover agent's scope works the same way.
- **No hidden second model in a tool.** Sentry's AI search needs another LLM provider. Heimdall's literal search is
  deterministic; vector search embeds the query with the provider that indexed the archive (`docs/chunk-index.md`)
  and nothing else.
- **The dialect the client speaks.** The specification's newest revision (2026-07-28 when checked) drops the
  initialization handshake, but Pi's client speaks `2025-06-18`; claiming a newer version over an older
  implementation would break both. Revisit when the pinned Pi moves.
- **Measure outcomes, not availability.** PostHog's probe runner checks that tools answer, not that the intent was
  met; Anthropic's guide measures errors, tokens, calls and outcomes on held-out real tasks.

Sources: https://github.com/PostHog/posthog/tree/07cad4a646bf4fd4e0c94998a471ac01e3847c53/services/mcp,
https://github.com/github/github-mcp-server/blob/main/docs/server-configuration.md,
https://github.com/cloudflare/mcp, https://github.com/getsentry/sentry-mcp,
https://www.anthropic.com/engineering/writing-tools-for-agents,
https://developers.openai.com/api/docs/guides/function-calling,
https://modelcontextprotocol.io/specification/2026-07-28.
