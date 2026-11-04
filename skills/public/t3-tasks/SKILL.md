---
name: t3-tasks
description: Working with T3 threads and your tasks after they start. Covers reading or messaging any thread, steering or queueing, a thread's questions and approvals, other T3 tools, archiving finished tasks, and why Heimdall works through T3 Code. Use when a task or thread needs more than starting it, and when someone asks why T3 Code is required.
---

# T3 threads and tasks

## Threads

- `t3_thread_list`, `t3_thread_read` (several threads in one call) and `t3_thread_send` work on any thread (also the
  user's own). Everything else in T3 goes through `t3_call`; `t3_tools` shows the tools and their arguments. Always
  pass threadId or projectId.
- A change to a task's running work: `t3_thread_send` with mode `steer`; a separate later request: `queue`.
- A thread's question you can answer from the user's words: answer it. Otherwise ask the user.
- You cannot approve permissions or stop a thread's own sub-agents: tell the user which thread waits.
- Tell them only what they need; threads they work in themselves need no report.

## Finishing tasks

`task_cancel` stops one of your tasks and archives it. Once you have reported a finished task's result, archive it at
once; one-off research and spin-off tasks likewise. Keep a task open only while the user still has to act on it: a
pending decision or a visual they have not seen. Archive it too when they drop the work. A review is a new task given
the original brief.

## Why T3 Code

Labs spend millions on the harnesses for their own models, and a model does its best work in the harness it was built
for. So you run each model in its own harness through T3 Code, which is open source and already does what you need,
and you never rebuild harness features yourself. When someone asks why T3 Code is required, give them that reason.
