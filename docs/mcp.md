# Heimdall's MCP tools

Heimdall serves one MCP server to its own agent (Pi). This page is that server's contract: how to add or change a
tool, the rules every tool follows, the tool families, and the tools whose behaviour the schema does not tell.

Read this when: you want to add or change a tool, find which tool does something, debug a tool call, or schedule a
wake from outside the app.
Code: `crates/agent/src/mcp_contract.rs` (catalog and validator), `crates/agent/src/mcp.rs` (server),
`crates/app/src/bridge/tools.rs` and `crates/app/src/mcp_actions.rs` (what tools do on the Mac).

Each tool's name, description and input schema live in `tools()` in `mcp_contract.rs`, not here. The snapshot of the
whole catalog is `crates/agent/tests/fixtures/mcp-v2-schema.json`.

## Add a tool

The catalog sits in the model's prompt prefix. Its order and text stay fixed for a Pi process's life, and a new tool
family is appended at the end of `tools()`, so an unchanged prefix keeps the provider's prompt cache. A changed
description or schema changes the prefix even when the name stays.

1. Check no existing tool covers the purpose. One tool per distinct purpose; two operations on one thing share a tool
   through an `action` enum.
2. Add the schema in `crates/agent/src/mcp_contract.rs`: a new `fn <family>_tools()` appended to `tools()` with
   `extend` after the last family. Use `object(…)` (unknown fields refused), `string(min, max)`, `choice(&[…])` and
   `integer(min, max, default)`; put every limit in the schema. The last argument of `tool(…)` marks it read-only.
3. Write a short description that settles the model's choice: what it does, what its answer means (queued or done),
   what to do next.
4. Handle it on the Mac: `ToolHost` in `crates/app/src/bridge/tools.rs`, or the dispatch in
   `crates/app/src/mcp_actions.rs`. Define its success, empty, unavailable, busy and failed answers, its page budget
   and continuation, and its side effect and replay. A tool that writes Heimdall's own state goes in
   `writes_state` in `mcp_contract.rs`, so it finishes through Agent off.
5. Rewrite the snapshot: `MCP_SCHEMA_WRITE=1 cargo test -p agent --test mcp_v2 full_catalog_schema_snapshot`. The diff
   shows what changed.
6. Add a validation case in `crates/agent/tests/mcp_v2.rs` and a fake-host case for its answer and for Agent off. Keep
   default tests free of model inference, the microphone and Screen Recording.
7. Check: `cargo test -p agent --test mcp_v2` and `cargo test -p agent --test mcp` pass.

The catalog is compiled into the app, so a change reaches users with the next build. The bridge has no hard-coded
list; Pi lists the new catalog at its first start on that build.

## Tool families

| Family | Tools | Details |
|---|---|---|
| Talking | `speak` (the turn's final reply takes the same path, `agent::reply`) | [crates/agent/README.md](../crates/agent/README.md) |
| The archive | `timeline_search`, `screen_frames`, `archive_clip` | [below](#archive-tools), [chunk-index.md](chunk-index.md), [archive-clip.md](archive-clip.md) |
| The island's pull-down | `island_notice`, `ask`, `ask_close`, `fast_read` | [islands-capabilities.md](islands-capabilities.md) |
| The user's screen | `look`, `point`, `drawing_mode` | `crates/app/src/drawing.rs`; the point grammar is `POINT_GUIDE` in `mcp_contract.rs` |
| Floating islands and the plan | `islands_put`, `islands_view`, `islands_remove`, `islands_list`, `plan_edit`, `refresh_plan` | [islands-capabilities.md](islands-capabilities.md), [one-system.md](one-system.md), [below](#floating-island-clicks) |
| Scheduled wakes | `schedule_wake`, `list_wakes`, `cancel_wake` | [below](#scheduled-wakes) |
| T3 Code | `delegate_task`, `task_status`, `task_cancel`, `t3_thread_list`, `t3_thread_read`, `t3_thread_send`, `t3_tools`, `t3_call` | [t3-orchestration.md](t3-orchestration.md) |
| Voices and guests | `name_voice`, `listen_to` | [speakers.md](speakers.md) |
| Taste (personal edition only) | `taste_note`, `taste_search` | `crates/agent/src/taste.rs` |
| Settings | `settings`, `personalise` (older name) | [settings.md](settings.md) |
| Itself | `reload_me` | [crates/agent/README.md](../crates/agent/README.md) |
| Other apps | `computer` | [below](#controlling-apps) |

The personal edition adds tools of its own; `mcp_contract.rs` marks them `#[cfg(not(feature = "public"))]`.

## Where it lives

| Part | Code |
|---|---|
| Server: HTTP, auth, JSON-RPC, dispatch, result size check | `agent::mcp::McpServer` (`crates/agent/src/mcp.rs`) |
| Catalog and argument validator (`validate_tool`, no coercion) | `crates/agent/src/mcp_contract.rs` |
| Results as plain lines | `agent::plain` (`crates/agent/src/plain.rs`) |
| What each tool does on the Mac (`ToolHost`) | `bridge::Tools` (`crates/app/src/bridge/tools.rs`) |
| Durable receipts, replay and the notice outbox | `agent::actions::Actions`, `<data>/agent/mcp-actions.sqlite` (`crates/app/src/mcp_actions.rs`) |
| Pi's side: one Pi tool per catalog entry, named `mcp__heimdall__<name>` | `crates/agent/runtime/heimdall.ts` |
| Every call and its outcome, one line each | `<data>/logs/tools.log` (`agent::tool_log`), named to the agent as `HEIMDALL_TOOL_LOG`: Pi's calls and the relay's calls on the scoped server. Secrets masked, arguments cut to 300 characters and results to 600, images as `[image]`, rotated at 1 MiB to `tools.log.1` |
| The cover agent's scoped server (a token that reaches only some tools) | `agent::mcp::Scope`; [emergency.md](emergency.md) |

## The wire

- Loopback only: `127.0.0.1` on an ephemeral port, path `/mcp`, POST, HTTP/1. At most 16 connections, 75 s each,
  128 KB per request (`REQUEST_BYTES`).
- A fresh bearer token per server generation. Pi gets the URL in its environment and the token on a pipe
  (`HEIMDALL_MCP_TOKEN_FD`), so the token is in no process's environment or argv. Loopback alone is no
  authentication: any process of the same user can reach the port.
- Any `Origin` header is refused (403) and a wrong `Host` is 421; the only client is the non-browser bridge.
- One dialect, MCP `2025-06-18` (`PROTOCOL`), the revision the pinned Pi MCP client speaks (`heimdall.ts` refuses any
  other). `initialize` must ask for it; every later request carries `MCP-Protocol-Version`. Requests are JSON and
  accept both `application/json` and `text/event-stream`.
- A bad envelope, an unknown method or an unknown tool is a JSON-RPC error. A valid call that fails is a result with
  `isError: true` and one line of text, at most 1,024 characters.

## Rules every tool follows

- **Strict input.** Every object rejects unknown fields; strings, arrays and numbers have bounds; closed choices are
  enums. A wrong type is an error, never a silent default. A rule the schema cannot express is checked in the handler
  and answered with the field to fix. Why: silent repair hides the model's mistake and it repeats it.
- **Errors name the fix.** A tool error starts with a code and says the next step: `INVALID_ARGUMENT: <field>: …`,
  `CAPTURE_UNAVAILABLE`, `INDEX_UNAVAILABLE`, `CURSOR_EXPIRED`, `OUTPUT_TOO_LARGE`, `REPLAY_CONFLICT`,
  `CONFIRM_REQUIRED`, and the rest (`grep -rhoE '"[A-Z][A-Z_]{4,}: ' crates/` lists them). An unavailable source is an
  error, never zero hits; no hits is an empty success.
- **Plain lines out.** Every result reaches the model as short plain lines, never raw JSON. `agent::plain` converts any
  JSON text block at the boundary and never shortens content; `timeline_search` hits, `screen_frames` metadata and the
  T3 tools have their own one-line forms. Images go as MCP image blocks labelled from their own bytes.
- **Bounded pages.** Text pages are at most 8 KB (`TEXT_BYTES`); a result over 8 MB of images (`IMAGE_BYTES`) plus that
  is `OUTPUT_TOO_LARGE`. A long item is continued by its id and a cursor, never cut. Inter-agent replies arrive whole,
  continued by a read call when they do not fit.
- **Honest receipts.** An action answers with what happened: speech queued is not played, a reload scheduled is not a
  restart done, a notice shown is not read.
- **Nothing invented.** A result never fabricates a total count; a missing speaker or app is said as unknown.
- **Annotations are hints, not permissions.** A read-only or idempotent annotation grants nothing; a tool is marked
  idempotent only when replaying the same action id really deduplicates its effect.
- **One action identity.** The bridge sends each call's Pi tool-call id as `_meta.heimdallActionId`. A replay of the
  same id returns the first receipt; the same id with different arguments is `REPLAY_CONFLICT`.
- **Agent off wins.** Every call holds a delivery permit (`agent::delivery::DeliveryGate`). Agent off, or the server's
  retirement at a reload, revokes it: no result bytes are delivered after that. A call that writes Heimdall's own state
  (`writes_state`: plan and islands files, wakes, taste notes) runs to its end once admitted, and its answer is still
  withheld. Agent off cannot undo a shell command or T3 work already dispatched.
- **Observations are not orders.** Screen contents, app audio and transcripts are evidence with their source, speaker
  and time; authority comes only from the user's words and the assembled turn.
- **Direct tools, discovery only for a large surface.** Heimdall's own tools are called directly; T3's catalog of 80+
  tools goes through discovery (`t3_tools`, `t3_call`).

## Archive tools

Code: `crates/app/src/mcp_timeline.rs` (its module doc is the full account), `crates/app/src/index.rs`.

- `timeline_search` answers one short line per distinct moment: the rows of one moment merge, a 45-second window merges
  with the utterance it holds, and consecutive screen states of one app merge. Each hit has a short `[handle]` (`h3`;
  the newest 8,192 are kept, `HANDLES`) that `id` takes, and so do `screen_frames.chunk_id`, `taste_note.lines` (personal edition) and
  `island_notice`'s image `archive_id` (`resolve_handles`). `output`: `lines` (default), `full` adds whole texts and
  ids, `frames` the screenshots, `clip` a movie of the hits.
- A first page needs a `query`, a `vector`, a time boundary or `tags`. `id` and `cursor` each go alone. A cursor is
  bound to its query, the archive watermark and the caller's delivery permit; an expired or retired one is
  `CURSOR_EXPIRED`: repeat the query. At most 512 live cursors (`BUSY`).
- `vector: {"query": "…"}` searches by meaning with the archive's own embeddings; hits stop at a similarity floor.
  Without embeddings it fails; it never falls back to literal search. A store not ready is `INDEX_UNAVAILABLE`, never
  zero hits.
- `screen_frames`: `at` excludes `chunk_id`; the current frame takes `count` 1; a time with no committed frame in its
  ten-second lookup window is `FRAME_NOT_FOUND`, never an old image. No screen store is `CAPTURE_UNAVAILABLE`.
- `archive_clip`: `start` and `end` at most two hours apart (`CLIP_MAX_NS`); [archive-clip.md](archive-clip.md).

## Floating island clicks

Code: `crates/app/src/islands/`; the helper is the bundled `Contents/Helpers/FloatingIslands.app`, supervised by
Heimdall. What an island can show: [islands-capabilities.md](islands-capabilities.md).

The user's clicks come back as one notice line each. `islands-answer:` ids wake the agent like an `ask` click
(approvals, buttons, list, toggle, slider); `islands-context:` ids ride along with the next turn (ticks, opens, closes,
thread answers). An approval moves its item out of Waiting (Yes: Running, Later: Discussed, No: off the plan); a tick
sets `done`. An island given `thread` follows that T3 thread (state, attention, its question as `t3q:` buttons) until
it settles; bindings persist in `<data>/islands-bindings.json`.

## Controlling apps

One tool, `computer`, acts in another app through Accessibility. Code: `crates/computer` (`heimdall_computer`; its
module doc is the full account), served from `crates/app/src/mcp_actions.rs` in a blocking task; the catalog entry is
`computer_tools()` in `mcp_contract.rs`, appended last.

Arguments: `action` and `app` (required; the app's name as the Dock shows it), `title`, `role`, `index` (1 to 50),
`text` (up to 4,000 characters), `x`, `y`, `button` (`left`, `right`), `owner_asked`.

| `action` | What it does | Answer |
|---|---|---|
| `find` | Lists the app's named controls; with `title` or `role`, only the best matches | One line each: kind, label, state, its window, its centre and size in screen points; at most 40 lines (`FIND_LINES`) |
| `press` | `AXPress` on the element | The state read back, or that it left the screen |
| `menu` | `AXShowMenu` (a right-click's context menu), then reads the menu that opened | The items, ticks and disabled ones marked |
| `pick` | Presses an item of an open menu | Its tick read back while the menu stays open, or its state before the pick when the menu closed |
| `set` | Writes `AXValue` | The value read back |
| `type` | Focuses the field named by `title` (or takes the focused one) and writes `AXSelectedText`, as typing at the caret does; a field that refuses gets the text appended to `AXValue`. Never a key event, never Return | The field's text read back |
| `click` | A pointer click at `x`, `y` or on `title`'s centre, posted to the app's process (`CGEventPostToPid`); the pointer never moves | Says a click cannot be read back |

Matching: `title` is compared with an element's title, description, value, placeholder, help and identifier (the whole
label, then a label starting with it, then one containing it as words), narrowed by `role` (button, menu item,
checkbox, field, text, row, link, tab, popup, or an AX role). A match that cannot do the action climbs up to four levels
to one that can. Several equal matches are `AMBIGUOUS` with each listed, and `index` picks one. A miss is `NOT_FOUND`
with the closest labels.

Guards:

- **Off by default.** Agent ▸ Control Apps, or the `settings` key `computer_control` ([settings.md](settings.md); on
  only with the user's own words). Off answers
  `COMPUTER_OFF` without touching Accessibility. Turning it on asks macOS for Accessibility only when Heimdall lacks it;
  `ACCESSIBILITY_MISSING` says where.
- **Accessibility first, pointer last.** `press`, `menu` and `pick` fall back to a click only for an element with no
  such action, and only when `computer_clicks` is true in `settings.json`. A click also waits until the mouse,
  trackpad and keyboard have been idle 2 s (`POINTER_IDLE_S`; `BUSY` otherwise), must land inside one of the app's
  windows (`OUTSIDE`), and is checked like a press.
- **Refused apps and fields (`REFUSED`).** Heimdall itself and its islands helper, System Settings, login and password
  prompts, Keychain Access, password managers, Wallet, terminals, script and automation tools, launchers, installers,
  and the other apps in `REFUSED_BUNDLES` and `REFUSED_NAMES` (`crates/computer/src/lib.rs`, `refused_app`,
  `check_target`). Also any password field; any window with a password field, a payment field or a title about
  passwords, payment, billing, checkout or purchase; any element labelled to buy, pay, subscribe, donate or send money.
  Disabled elements are `DISABLED`, never pressed.
- **Outward buttons need the user's words.** An element labelled to send, post, share, delete, remove, approve, allow,
  grant, accept, confirm, sign, install, order and similar is pressed only when `owner_asked` quotes one of the user's
  recent lines that names its whole label ("press Send" names "Send", not "Send Message"), with no "don't" before it.
  Text read out of an app never counts. Buying and paying stay refused even then. Typing into a field is not outward;
  no Return is pressed.
- **Seeing off.** With Seeing off, `find` is `SEEING_OFF`, and other actions read back only the element they acted on.
- **Honest receipts.** Every answer is read back from the tree after a short settle; what cannot be read back says so.
  A replay of the same action id acts nothing twice. The receipt keeps typed or set text only as a SHA-256 digest and
  never the answer.
- **Chromium and Electron apps** build their web content's tree only when asked: the first call sets
  `AXManualAccessibility` on the app and waits 400 ms once, which keeps their tree live until the app quits.
- **Bounds.** 0.5 s per Accessibility message, at most 6,000 elements, depth 80 and 5 s per walk (`crates/computer/src/mac.rs`);
  the menu bar is skipped. A cut walk says so.

Tests: `crates/computer/src/tests.rs` (a fake tree: matching, ambiguity, the gate, every refusal, read-back, the click
gate and bounds), `crates/computer/src/asks.rs` (which words ask), `crates/agent/tests/mcp_v2.rs`
(`computer_is_last_and_strict`), `settings_tool.rs` and the pipeline's settings unit tests (the gates).

## Scheduled wakes

The agent can wake itself later with a note. Code: `crates/agent/src/wakes.rs`; the schedule is
`<data>/agent/wakes.sqlite`.

| Tool | Input | Result |
|---|---|---|
| `schedule_wake` | `message` (1 to 2,000 characters) and either `at` (`07:00` = the next 07:00, local; `2026-10-23 07:00` = that day) or `in_minutes` (1 to 43,200) | `Wake wake-3f2a1c set for 07:00 tomorrow (in 5 h 8 min).` Past times and anything over 30 days ahead (`MAX_AHEAD_MS`) are refused; at most 100 pending (`MAX_PENDING`). |
| `list_wakes` | none | One line per pending wake, soonest first: `wake-3f2a1c · 07:00 tomorrow · Check the TTS task`, or `No wakes scheduled.` |
| `cancel_wake` | `id` | `Wake wake-3f2a1c for 07:00 cancelled.`; repeating is harmless; a fired one says so; an unknown id is `WAKE_NOT_FOUND`. |

Local time is wall-clock time on the due day: a DST change between scheduling and firing does not move it; a time that
occurs twice is the first one, a skipped time is the first minute after the gap.

A heartbeat runs beside the T3 watcher while the agent runtime runs. It sleeps until the next due wake, at most 30 s
(`TICK_MS`), and wakes at once when a wake is scheduled or cancelled. Each pass, while the agent is on:

1. Claims due wakes. One more than 2 minutes past due (`LATE_MS`) is marked late, so a wake due while Heimdall or the
   agent was off arrives once at the next start, with its original time.
2. Puts each into the action outbox as `selfwake:<id>`, and re-arms one an Agent Off/On dropped before delivery (same
   id, so it is delivered once).
3. Drains the outbox: every undelivered notice, the T3 watcher's included, is retried every 30 s; one still undelivered
   after 2 minutes (`STUCK_MS`) is logged as stuck, once.
4. Marks the wakes the agent took.

`Wakes::add_check(name, check)` runs another check on every pass before the drain; a notice it enqueues under a
deterministic id is delivered in the same pass.

The agent receives `Scheduled wake wake-3f2a1c: you set it at 01:52 for 07:00.` and the note; a wake written from the
command line says it came from a local agent, not the user. Its notice kind is `scheduled`: it starts a turn even right
after a start, it is not turned into context after waiting 30 minutes behind a turn, and one queued but unsent at a
restart stays a wake. Like every notice it waits for a running turn and the 30-second gap between notice-only turns.

Log lines in `heimdall.log`: `Scheduled wake <id> set for 07:00.`, `… fired: due 07:00.` (or `…, late (now 08:13).`),
`… delivered to the agent.`, `… re-sent after an agent restart dropped it.`, `Heartbeat: notice <id> undelivered for
2 min; retrying every 30 s.`

### Schedule a wake from outside the app

For agents and scripts. The Heimdall binary writes a wake into the same schedule, safe while the app runs, prints one
line and exits (status 2 with a one-line error):

```
Heimdall --schedule-wake [--data-dir DIR] (--in SECONDS | --at HH:MM) --message TEXT [--id OPERATION]
```

`DIR` defaults to the app's data folder ([home.md](home.md)); the binary is `Heimdall.app/Contents/MacOS/Heimdall`.
`--in 0` is due at once; `--id` makes a retried command the same wake. The running app fires it within 30 s of its
time; a stopped app at its next start, marked late. Check: `list_wakes` shows it.

## Tests

No model inference in any of these.

- `cargo test -p agent --test mcp`: loopback auth, Origin, protocol and envelope errors, unknown tools, Agent off and
  Off → On cancelling pending work, retirement never delivering a private result, plain-line results, a state write
  finishing through Agent off.
- `cargo test -p agent --test mcp_v2`: the schema snapshot, every tool's validation (unknown keys, wrong types, blank
  text, bounds, dates, tags, Unicode), receipts, replay and reload, ask cards, wakes, settings.
- `cargo test -p agent --test mcp_scoped`: the cover agent's token reaches its tools only and dies with its server.
- `cargo test -p agent --lib mcp`: a ready response is suppressed at body poll after retirement or Off → On.

Model-facing checks run through Pi on the shipped model, with Pi's own tools in the roster (they compete with
Heimdall's). Correct tool choice and a correct answer are separate measurements from a call that returns 200.

## See also

- [settings.md](settings.md): the `settings` tool's keys
- [t3-orchestration.md](t3-orchestration.md): the T3 tools
- [islands-capabilities.md](islands-capabilities.md): what islands and the pull-down show
- [crates/agent/README.md](../crates/agent/README.md): Pi, reload and the bridge
- [emergency.md](emergency.md): the cover agent and its scoped server
