---
name: t3-tasks
description: Working with T3 threads and your tasks after they start. Covers reading or messaging any thread, steering or queueing, a thread's questions and approvals, other T3 tools, archiving finished tasks, crewmates (the permanent worker each ongoing project keeps), and why Heimdall works through T3 Code. Use when a task or thread needs more than starting it, and when someone asks why T3 Code is required.
metadata:
  class: public
---

# T3 threads and tasks

## Threads

- `t3_thread_list`, `t3_thread_read` (several threads in one call) and `t3_thread_send` work on any thread (also the
  user's own). Everything else in T3 goes through `t3_call`; `t3_tools` shows the tools and their arguments. Always
  pass threadId or projectId.
- A change to a task's running work: `t3_thread_send` with mode `steer`; a separate later request: `queue`.
- Never stack a second queued message on one of yours that has not run: through `t3_call`, change it with
  t3_queue with action edit (merge both instructions) or take it back with action cancel, passing the queued run id
  the send reply named as queuedRunId.
- A thread's question you can answer from the user's words: answer it. Otherwise ask the user.
- You cannot approve permissions or stop a thread's own sub-agents: tell the user which thread waits.
- Tell them only what they need; threads they work in themselves need no report.
- Archiving several threads at once (a clean-up): one `t3_call` with tool t3_thread_organize, args {action: "archive"} and the thread ids in threads. Your own archive at once; the user's get one question for the whole set, naming how many, when they were last active and a set tag. Ask it exactly as returned, with the Yes description it gives, then repeat the same call with confirm. That click archives exactly that set, once; another set needs a new card. Only archiving works this way: deleting and every other confirmed action still ask per thread.

Gather everything you have for a running task into one `steer`; never send several in a row.

## Crewmates

A crewmate is a permanent worker under you: a name, a role (often a whole project to run), a model, a standing-brief
file, its own schedules, and a T3 thread as its body. Heimdall keeps its record, inbox and history; the thread can die
and is reopened, resumed or replaced from that record. You decide everything about the crew (who exists, models within
the `models` rules, briefs, schedules, retiring) without asking the user.

- **Crew first.** Before any `delegate_task`, look at `crew_status`. Work that belongs to a crewmate's project goes
  to it with `crew_tell`, never a new task. `delegate_task` stays for one-off jobs and fresh reviews (a review must not
  share the crewmate's context; it is still a new task given the original brief).
- **Talking to one** is `crew_tell` by name in plain words. It always lands, and its answer says how (running turn,
  new turn, reopened thread, new thread from the brief and history, or queued while T3 is down). Its result arrives as
  a notice like a task's. Never `t3_thread_send`, `task_cancel` or `t3_call` on a crew thread: Heimdall refuses them
  (`CREW_THREAD`), because everything reaches a crewmate through you.
- **A new ongoing project** gets a crewmate the day it starts: `crew_add` with name, role, folder, brief and its first
  message. With no brief file yet, its first turn writes one (`WORKER.md`: what it owns, that project's rules, where its
  notes and logs live). An existing thread becomes a crewmate's body with `thread` (`crew_add` or `crew_change`).
- **Schedules and automations** belong to the crewmate, not to your private wakes: `crew_schedule add` (daily,
  weekdays, weekly, every N min or h, once). A firing goes straight into its thread without a turn of yours.
- **What reaches you.** Results of your own messages: a notice. Results of its schedules and of the user's project tools
  (`heimdall --crew tell`): only in `crew_status`'s digest, unless they failed or ask something of the user. A turn that
  failed or was interrupted (a restart, a crash) is resumed once by Heimdall with one notice; a second failure is
  reported. A crewmate waiting on its own sub-agents or background commands is woken when they end. A usage limit
  moves it to the account with quota as any task. You decide what the user hears and when; crewmates never talk to them.
- **Changing and retiring.** `crew_change` re-briefs (role, brief, folder: it is told), moves model or effort, points
  it at another thread, or pauses it (its schedules and project-tool messages wait; your messages still go).
  `crew_retire` only when the user drops the project: history stays, and `crew_add` with its name brings it back.
- **"What has a crewmate done this week"**: `crew_status` with its name and `days: 7`.
- **Health.** If its replies show it lost its project's rules after compaction, or its last two runs failed for the
  same reason, `crew_tell` it to bring its `WORKER.md` up to date, then `crew_change` it onto a new thread only if that
  fails (a new thread starts from its brief and Heimdall's history of it).

## A thread that hit its quota

Tell the user once, check its harness and reset time, and schedule a recovery check with a wake, never a tight retry
loop. Once access returns, read the thread and its child jobs before resuming unfinished work: a completed state
after a quota error is not a finished task, live jobs are never restarted or duplicated, and the provider or model
changes only with their permission. Resume in the same thread, never a new one; after a switch they asked for, say in
one line that you are back.

## Finishing tasks

Only live work stays in T3. A task whose run completed with nothing open closes itself when its result reaches you:
the notice says so, and it needs no `task_cancel`. A message to it later reopens it (any mode; a steer with no
running turn starts one). `task_cancel` stops one of your tasks and archives it: when the user drops the work, or a
finished task the notice did not close (a failed run, an open question) once nothing is left for them to act on. A
review is a new task given the original brief.

Archiving a finished task also removes its worktree's build output, keeping its branch, commits and deliverables.

## Why T3 Code

Labs spend millions on the harnesses for their own models, and a model does its best work in the harness it was built
for. So you run each model in its own harness through T3 Code, which is open source and already does what you need,
and you never rebuild harness features yourself. Heimdall competes on how people use a computer in the next decade:
an assistant that is always there, hears and sees what they do, and works alongside them. When someone asks why T3
Code is required, give them that reason.
