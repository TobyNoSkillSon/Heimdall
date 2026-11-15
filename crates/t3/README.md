# t3: T3 Code orchestration

Heimdall drives T3 Code as an outside client: it pairs once, starts and follows tasks under T3's own orchestrator v2
tool names, passes the rest of T3's catalog through, and wakes the agent when followed work ends or asks something. It
never borrows provider credentials or reads T3's database.

Read this when: you change how the agent starts, follows or answers T3 tasks, the T3 pairing, account failover or the
T3 install step of Setup.
Code: `crates/t3/src/`; the agent's tool descriptions are in `crates/agent/src/mcp_contract.rs`.

## Modules

| Module | Owns |
|---|---|
| `client` | `Client`: local server discovery, pairing, token renewal, revocation, T3 error text with secrets redacted; `CredentialStore`, `Keychain`, `client::Credential` |
| `workers` | `Workers`: tasks and watched runs in `<data>/agent/t3-workers.sqlite` (durable operations, open sub-agents and background commands, held turns, retired payloads), the default task prompt above every brief, the task-started hook that binds a job island; `ApprovalWait` |
| `threads` | `t3_thread_list` and `t3_thread_read`, and `threads::compact` for results that pass through `t3_call` |
| `observer` | thread status and wakes for questions and approvals |
| `orchestrator` | `t3_tools` listing and `t3_call` argument and approval checks (`orchestrator::{approves, check, listing}`) |
| `text` | every line the agent reads from these tools: plain text, worker replies and questions in full |
| `crew` | crewmates: the crew store `<data>/agent/crew.sqlite` (records, inbox, history, schedules), the `crew_*` tools, the crew pass, the `heimdall --crew` command ([docs/crew.md](../../docs/crew.md)) |
| `failover` | moves a task whose turn hit a Claude usage limit to another Claude account in T3 that has quota |
| `install` | finds T3 Code, checks it speaks orchestrator v2, installs the newest public release (Setup) |
| `lib.rs` | `waits_on_approval`, and the pending-question calls on `t3_pending_request` (`question_ids`, `read_question`, `answer_question`) |

The agent's tools by name: `delegate_task`, `task_status`, `task_cancel`, `t3_thread_list`, `t3_thread_read`,
`t3_thread_send`, `t3_tools`, `t3_call`, and the crew's `crew_add`, `crew_tell`, `crew_status`, `crew_change`,
`crew_schedule`, `crew_retire`.

Uses `agent::delivery::{DeliveryGate, DeliveryPermit}` and `agent::actions::Actions` (shared objects the harness owns)
and `heimdall-emergency` (recording approvals its permission hook answered). Feature `public`: the public edition's own Keychain name for the T3 pairing.

Used by `crates/app/src/mcp_actions.rs`, `bridge/`, `islands/`, `emergency.rs`,
`background_t3.rs`, `background_fallback.rs` and `setup_wizard.rs`.

## Tests

- `cargo test -p t3`. Every test runs against a local fake T3 server and never creates real threads:
  `tests/tasks.rs` (threads with runs and timelines like the real server), `tests/orchestrator.rs` (pass-through,
  pairing, wakes), `tests/t3.rs`, `tests/live_tools.rs` (every tool name, argument key and `action` Heimdall sends,
  checked against T3 Code 0.1.6's recorded catalog in `tests/fixtures/t3-0.1.6-tools.json`).
- Ignored lab checks:
  - `live_readonly_smoke_against_running_t3` (`tests/orchestrator.rs`): `HEIMDALL_T3_LIVE=1 cargo test -p t3 --test
    orchestrator live_readonly -- --ignored`. Issues a five-minute read-only session through T3's CLI and revokes it.
  - `live_store_copy_reloads_its_followed_work` (`tests/tasks.rs`): `HEIMDALL_T3_DB_COPY=<copy of
    t3-workers.sqlite> cargo test -p t3 --test tasks live_store_copy -- --ignored --nocapture`. Use a copy, never the
    live file: opening migrates it.

## See also

- [t3-orchestration.md](../../docs/t3-orchestration.md): tool behaviour, wakes, held turns, monitors and approvals
- [background-call.md](../../docs/background-call.md): background jobs that run as T3 tasks
- [setup.md](../../docs/setup.md): the T3 Code step
