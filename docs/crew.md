# Crewmates

A crewmate is a permanent worker under Heimdall: a name, a role (often a whole project to run), a model, a
standing-brief file, schedules, and a T3 thread as its body. Heimdall keeps its record, inbox and history; the thread
can be archived, deleted, interrupted or moved to another account and is reopened, resumed or replaced from that
record. Everything reaches a crewmate through Heimdall's crew layer, and every message in and result out is recorded.

Read this when: you change the crew tools, the local `heimdall --crew` command, how a crew thread survives, or what
reaches the agent from the crew.
Code: `crates/t3/src/crew.rs` (store, tools, pass, local command); hooks in `crates/t3/src/workers.rs` (marked
"crewmates"); tool schemas in `crates/agent/src/mcp_contract.rs` (`crew_tools`); wiring in
`crates/app/src/mcp_actions.rs` and `crates/app/src/main.rs`. Design: `lab/crewmates/README.md`.

Both editions have the crew: it needs only T3, which both have.

## The agent's tools

| Tool | Does |
|---|---|
| `crew_add` | name, role; optional folder, brief, model, effort, thread (adopt an existing thread), message (first task). A retired name comes back with its history. |
| `crew_tell` | name, message. Into the running turn, a new turn, the reopened archived thread, or a new thread started from the brief and history when the old one is gone; queued while T3 is down. The answer says which. |
| `crew_status` | Without name: one line each and what each received and did since the agent last looked (the digest). With name: record, schedules, inbox, last result, history; `days` for a window. |
| `crew_change` | role, brief, folder (the crewmate is told), model or effort (`t3_thread_configure`), thread (point at another thread), paused. |
| `crew_schedule` | add (`when`, `message`), remove (`id`), list. |
| `crew_retire` | Stops its turn, archives its thread, ends its schedules and inbox; history stays. |

`delegate_task` stays for one-off jobs and fresh reviews.

## Rules the code holds

- **Only through the crew.** `Workers::crew_guard` refuses `t3_thread_send`, `task_cancel` and every non-read
  `t3_call` on a crew thread (`CREW_THREAD`), whoever calls: the agent, the relay, the phone. Reads stay free.
- **Never lost.** Each message is an inbox row in `crew.sqlite` before delivery is tried; a failed delivery stays
  pending and the pass tries again, backing off to `RETRY_MAX_MS` (10 min); after `MAX_ATTEMPTS` one notice says it
  still waits, and it is never given up. Sends and starts are idempotent by the row's id (`crew-send:<id>`,
  `crew-start:<id>:…`); a start's brief is composed once and kept with the row, and a launch that never happened goes
  again under a new id after the launch record had time to settle. The other crew tools answer a replay of the same
  operation as the first time.
- **Never closed.** A crew thread is exempt from the self-close and from clearing its island (`archive_cleared`).
- **What wakes the agent.** A result of the agent's own message: the usual task notice. A result of a schedule or a
  project tool: recorded only, shown in the digest, unless the run failed or the reply asks something of the user
  (`text::waits_on_user`). A first failed or interrupted turn: resumed by the pass with one `crew:resumed:` notice; a
  failure of that resumed turn is the usual notice. A usage limit and a sign-in failure keep their own handling (the
  account move, the sign-in retry); the crew records the new account.
- **Interrupted is resumed once.** A crew thread the user stops by hand in T3 is resumed once too, and the notice says
  so; `crew_change paused` or `crew_retire` is how a crewmate is stopped.
- **Sub-job wake.** A crewmate whose turn ended with sub-agents or background commands open is woken once they have
  all ended, no turn of it has started since, and `WAKE_GRACE_MS` (2 min) has passed for T3's own wake.
- **Lost body.** Each thread is read every `BODY_CHECK_MS` (10 min). Gone from T3: no longer followed, recorded, and
  the next message (at once if it was working) starts a new thread from its intro, the last result and the latest
  history (`SUMMARY_BYTES`, 8 KB).
- **History.** Every message in (with its source: Heimdall, a project tool, a schedule, a resume or wake), every
  result out, and every event (thread started, lost, adopted, schedule fired, changed) with its time, kept for good
  (each entry at most 32 KB).

## The local command

`heimdall --crew tell <name> <file|-> [--id KEY]` queues one message for an existing crewmate in
`<data>/agent/crew.sqlite` and exits; the running app delivers it within one pass (30 s), marked as from a project
tool. `--id` makes a retry the same message. `heimdall --crew list` prints the crew. It cannot create, change or
retire a crewmate, read or start threads, or call any other tool. `heimdall --crew import <workers.md>` is the
one-time migration of the old registry file (threads are adopted on the next pass, and only threads Heimdall itself
started: an import cannot hand a crewmate one of the user's own threads).

## Schedules

`daily HH:MM`, `weekdays HH:MM`, `weekly <mon…sun> HH:MM`, `every N min` or `every N h` (at least 15 min),
`once YYYY-MM-DD HH:MM`, in local time. A firing goes into the inbox under `schedule:<id>:<due>`, so it is never
doubled or lost across restarts; a paused crewmate's schedules wait.

## Checks

`cargo test -p t3 --test tasks crew` (stand-in T3: create, the guard, archived and gone threads, the inbox while T3 is
down, restart recovery, sub-job wake, account move, the local command and digest, schedules and retiring, adoption and
import, the watcher's pass) and `cargo test -p t3 --lib crew` (registry parsing, schedule times).
