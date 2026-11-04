# t3 — T3 Code orchestration

Heimdall controls T3 Code as an outside client: it pairs once, starts tasks and follows them for the agent under T3's
own orchestrator v2 tool names (`delegate_task`, `task_status`, `task_cancel`, `t3_thread_list/read/send`), passes the
rest of the orchestrator's catalog through (`t3_tools` / `t3_call`), and wakes the agent when a task is done (T3's
`waiting_for_children` rule), when a run it sent a message into ends, or when any thread asks a question. It never
borrows provider credentials or reads T3's database.

**Owns:** `Client` (local server discovery, pairing, token renewal, revocation, T3's error text with secrets
redacted; `CredentialStore`, `Keychain`), `Workers` (tasks and watched runs in `t3-workers.sqlite`: durable
operations, open sub-agents and background commands, held turns, retired payloads; the default task prompt above
every brief and the task-started hook that binds a job island), `threads` (the thread tools and
the compact forms of T3 results), `observer` (thread status and wakes for questions and approvals), `orchestrator`
(pass-through listing, approval checks), `text` (every line the agent reads: plain text, replies and questions in
full).

**Interface:** `t3::{Client, CredentialStore, Keychain, Workers}`, `t3::client::Credential`,
`t3::orchestrator::{approves, check, listing}`, `t3::threads::compact`. Uses `agent::delivery::{DeliveryGate,
DeliveryPermit}` and `agent::actions::Actions` (shared runtime objects owned by the harness); everything else is local.

**Used by:** `crates/app/src/mcp_actions.rs` and `bridge/` (the agent's T3 tools), `islands/` (thread-bound islands),
`emergency.rs` (the cover thread) and `plan_refresher.rs`.

**Testing:** `cargo nextest run -p t3`: 83 tests plus two ignored lab tests (counted from the test attributes, 27 Oct),
with fake T3 servers (`tests/tasks.rs` has threads with runs and timelines like the real one; plus the plain-text
formats). The ignored ones: `live_readonly_smoke_against_running_t3` reads a running T3 without mutating it;
`live_store_copy_reloads_its_followed_work` shows what a copy of the live store reloads after a restart.
A Claude Monitor counts as background work until its stop, a TaskStop or its timeout; a persistent one (no
timeout) counts 30 minutes, after which its event turns wake Heimdall again as ordinary turns (accepted: such
monitors are rare, and a forgotten one must not hold results for good).
Reference: `docs/t3-orchestration.md`; live evidence fixtures in `tests/fixtures/`.
