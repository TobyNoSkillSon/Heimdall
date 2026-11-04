# Goals: the shadow job

The one system holds (`docs/one-system.md`): `add_goal` and a new `now` or `next` task (unless a `now`
quotes the user's own words) become proposals for the user's yes; a done task settles with the proof code reads from its line
(`goals/proof.rs`), or as claimed; the clean slate (`islands/slate.rs`) replaced the seed goals with proposals.
Where this page says otherwise, `docs/one-system.md` wins.

The to-do list became **Goals**: only the major jobs, something a week or a month ahead, worked towards slowly; when
a task is accomplished it is hidden under the goal's collapse and the tasks accumulate below it. A background job
goes through the goals like a rolling context, like the agent's own but on a much cheaper model, so it can shadow the
agent completely because the calls hit the cache. That model has one task: did something make us closer to the goal,
or did we finish the goal? Goals, not to-do, because a to-do is obligatory and a goal is self-motivating. Items that
need the user stay small and separate, appear only while they truly need the user and vanish when answered.

Code: `crates/app/src/goals/` (`stream`, `trigger`, `segment`, `ops`, `shadow`, the loop in `mod.rs`), the prompt
`crates/app/prompt/goals.md` (public twin `prompt/public/goals.md`), the lab `crates/app/src/bin/goals-lab.rs`.
Background Tasks ▸ Goals (`ui::BackgroundTask::Goals`, key `goals`, symbol `flag`): on by default, its own model row,
on the shared model until the user picks one (as Plan Lists), the fallback (`background_model::task_endpoint`), Agent off
and emergency mode's pause as every job.

## The reversal, for this job only

Every background job is a stateless call that carries its whole input, with one exception: this
**one job**, because of the cache: the shadow keeps an append-only context and sends it as a
stable prefix, so each call pays for its new lines and reads the rest from cache. Every other job stays stateless.
On API backends the turns are resent (`background_call::Call::history`) and the provider's prefix cache reads them;
on a T3 instance the job keeps one long-lived thread per context segment (`Call::session`, `background_t3::end_session`
when a segment ends).

## The stream

`stream.rs` reads the agent's Pi session as Pi builds context (`pi_session.rs`, shared with the plan refresher): its
messages and its `custom_message` entries (Pi turns them into user messages: relayed transcripts, T3 task notices,
catch-ups). Each entry is rendered once by id: `Incoming:` (what reached Heimdall: the user's words, transcripts),
`Notice:` (notices), `Heimdall:` and its tool calls (arguments cut at 300 characters), tool answers (300 characters,
one line). Thinking and images are dropped; so are receipts, model changes, compactions, edits, the delivery and
clock headers, and the plan refresher's own notices. Lines go to `<data>/goals/stream.jsonl` as `{id, at, lines,
kind}`; the ids there (and in `seen.ids`, the ids of rotated lines, newest 20,000) are the cursor, so a Pi compaction
or a restart on a new session file repeats nothing. Past 5,000 lines or 4 MB the stream rotates, keeping the lines
no turn carried and the 30 before them. The live loop reads only what the file gained (`pi_session::entries_from`),
every 5 s, when its size or mtime changed, from a cursor kept in `<data>/goals/cursor.json` (path, inode, offset,
size, mtime, the last entry id taken); it reads the whole file again when another file is active, the file was
replaced (inode), shrank or its mtime went back, or the new entries' branch does not continue from the last one
taken (Pi went back to a branch an earlier read passed over). On the
very first start the existing session is the past: its last 30 lines open the first segment's header, none is news.
While the job is turned off (or no model is connected) lines are read but are not news when it comes back; while
Agent is off or emergency mode pauses background work they wait, and the loop wakes on `background_gate::resumed`.
A turn carries at most the newest 400 lines and 60 kB; older ones are set aside (logged).

## Triggers (`trigger.rs`)

Events, debounced, never per line. The user's words fire once they have been quiet 60 s, at most 3 min after the first
pending words; a finished task (a `terminal:` notice) 20 s after it; Heimdall's reply and everything else ride on the
next call. At least 45 s between calls; events meanwhile coalesce. Nothing new: no call. A call that failed without
cost (network, server, backend down) retries its lines after 2 min, doubling with each failure in a row up to 30 min;
a gate refusal goes again when the gate opens, without backoff; a run paid for without an answer, or refused over
the $0.50 limit, sets its lines aside and starts a new segment (so a context grown past what a run may cost never
stalls the job). Every time is passed in, so the lab replays a day in simulated time.

## The segment (`segment.rs`)

`<data>/goals/segment.json`: id, header, turns `[{user, assistant, at}]`, and the last stream line sent. Each call:
system = `goals.md`; history = the turns; the new user turn = the new lines (`Prompt::stable`) + `<goals_now>` (the
Goals compact, ids included; finished tasks counted) + the local clock. The first turn of a segment opens with its
header: the Goals as they stood when it began, with every finished task (that is the memory), and the last 30 lines.
After a turn whose segment passes 40,000 estimated tokens (3 bytes a token; the real count runs 25-50 % higher, the
climb measured 64,590 estimated as 95,501 real, so 40,000 keeps a call with a full turn of new lines under Claude Haiku
5.5's 100k price step) a new segment starts and the old session ends. On T3 the segment's thread is also ended when
Background Tasks ▸ Goals is turned off or its model leaves T3.

## Ops (`ops.rs`)

The model answers with ops, not the list (output was the plan refresher's main cost: 2.6-4 M tokens a day). Schema:
`{"ops": [{quote, op, goal, task, title, state, now, kind, detail, ask, icon, about}]}`, every key required, ""
when unused, `quote` first.

| op | does | keys |
|---|---|---|
| `progress` | a task under a goal is done, now or next (a new one by title); the goal's now line and `moved` | goal, task or "" + title, state, now |
| `finish_goal` | the goal is reached | goal, now |
| `pause_goal` | set aside | goal |
| `add_goal` | a new big goal the user named | title, icon, about, now |
| `ask` | something only the user can do now | title, detail, kind, goal or "" |
| `answered`, `drop_ask` | the ask leaves | ask |
| `drop_task` | an abandoned task that is not done | goal, task |

Checked in code, against the file as it is when the answer lands (under the folder lock): an op whose quote is not in
the new lines (normalised: case, spacing, punctuation; at least 4 characters) is dropped and logged; an op that
takes something away (answered, drop_ask, finish_goal, pause_goal, drop_task) needs a quote of 3 words and 15
characters; `answered` must quote the user's own words or a notice of their click, and so must `drop_ask` for an ask the
shadow did not raise (one from `plan_edit` or the old To do list; the shadow marks its own `by: "goals"`); unknown goals,
tasks and asks are refused; at most 6 active goals (an add or a resume beyond is refused), titles 40 characters, now
lines 60, an ask's detail 60, 8 open asks, 12 ops an answer; a task the user ticked (`ticked: true`) never goes back; a
finished task is never dropped (it is the memory). `moved`, a task's `at` and an ask's `asked` come from the clock.
Unknown keys survive; an ask the user answered by a click (done, or `chosen` other than later) leaves.

## The file

`islands/goals.json`, preset `todo`, list `goals`, title Goals, icon `flag`: `items` are the asks (the plan item shape
plus `goal`), `goals` the goals (`id`, `title`, `icon`, `about`: one line, at most 140 characters, saying what work serves it,
read by the model and not drawn; `state` active|done|paused, `now`, `moved`, `tasks` with done|now|next,
`expanded` written by the app). Progress is drawn from the tasks; never a due date or an overdue
colour. First start with no goals: no seed goals in this edition; on a seeded edition only
on a truly first run: never while the file, a closed or kept copy or a `seeded` mark shows goals existed, or To do
was migrated. Seed goals without `about` get theirs once.

There is no to-do list: a to-do is a scheduled item, something that has to be done because it is scheduled. No To do list remains in the UI,
tools or prompts. The old `todo.json` is migrated once, when the islands host starts (whether Background Tasks ▸
Goals is on or not), after a backup of the islands folder (`backup-goals-<stamp>/`): its open items with a time (`due`,
`when`) go to Scheduled; its open waiting items become asks (goal "": the shadow files them under a goal); its running
and discussed items are not shown to the shadow and stay in the kept file, `islands/.todo-migrated.json`; `todo.json`
is removed. Asks are drawn under the goal they serve (goalless ones as a card), so they name one whenever one fits.
A Scheduled item that falls due stays in Scheduled, shown due; the `due` response tells the agent, and one an older
islands app moved into a `todo.json` goes back to Scheduled.

The plan refresher keeps Scheduled (which also takes the user's obligations with a time) and the job islands without a
thread (Settled is this job's); it never reads or writes Goals and makes no other list. `plan_edit` takes lists `goals`,
`scheduled` and `settled` (an old caller's `todo` is still read as Goals, undocumented): add, tick (answered: it
leaves), remove an ask, and `show` reads a list (goals, tasks, asks with ids) and puts it back on screen.

A plan list the user closes is hidden (`hidden: true`), never deleted. Every write of a plan list keeps a copy beside it (`islands/.kept-<list>.json`), so a close that
arrives without the island (an app that still deletes, a close before the watcher saw it) still leaves its content;
`refresh_plan`, each refresher pass and `plan_edit` show bring it back. `islands_remove` on a plan list hides it the same
way. Every writer starts from `goals::ops::current`: the file, else its closed copy (still hidden), else its kept
copy; a Goals file that has no goals while a copy has them (an app wrote a fresh list holding only a due ask) is
merged, the copy's goals with every ask.

## Settled

The shadow job that runs Goals also writes Settled. Settled is proof of progress, drawn as day columns
(`crates/islands/src/mac/lists.rs`, `model/settled.rs`). The job's one purpose, stated in `prompt/goals.md`: what
happened, did it finish a piece of work toward a goal or did the user decide something that moves one; each finished piece
is a done Goals task and a Settled item. `goals/settled.rs` writes `islands/settled.json` in the same hold of the folder
lock as the Goals file: `{id, title, done: true, at, kind: shipped|decided, goal, task, by: "goals", quote}`, `at` the
epoch second of the line the quote came from (never a word: the plan refresher's "Today" stayed "Today" for days), the
task's Goals `at` the same. Code does the rest: no goal, no item; the same task or a title sharing 75 % of content words
within 3 days is one item (and a Goals progress op naming the same thing in other words moves the existing task); what
the user removed (`plan_edit` remove on Goals, Scheduled or Settled records `removed: [{title, quote, at}]`) never comes
back, matched by half the content words or the quote, and is never proposed there again (on Scheduled for two weeks: a
promise may be made again); at most 2 decisions a turn, each on the user's own words; Settled titles up to 64
characters (two lines). The shadow raises no asks: the schema offers `progress`, `decided`, `add_goal`, `finish_goal`, `pause_goal`, `drop_task`; asks from
`plan_edit` still leave through `checked`. Inputs the answer must account for, one entry each: `<finished_tasks>`
(completed task notices, marked "(the task is still running)" or "(repeats its earlier reply)" when the framing says
so) and `<heimdall_reports>` (Heimdall's and the relay's lines with a word of finished work). The plan refresher no
longer reads, shows or writes Settled. Lab: `lab/goals/settled_*.py`,
`goals-lab replay --seed-settled`.

## The lab

```
goals-lab replay --session <pi jsonl> --from <iso> --to <iso> --seed default|empty|<goals.json> --runner <cmd> \
    --out <dir> [--effort low] [--compact-tokens N]
goals-lab stream --session <pi jsonl> --from <iso> --to <iso>
goals-lab prompt
```

`replay` runs the window in simulated time with the real rendering, triggers, turn building, checks and apply, into
`<dir>/islands-root`; the session before `--from` is the past (the first header). The runner is run once per call
with `{"session", "system", "schema", "history": [{"user", "assistant"}], "user", "effort"}` on stdin and prints
`{"text", "usage": {"input", "cache_read", "cache_write", "output"}, "ms"}`; `lab/goals/mock-runner.py` calls no
model. Output: `<dir>/turns.jsonl` (`seg, turn, at, trigger, new_lines, user, answer, ops [{…, kept, reason}],
goals_after, usage, ms, segment_tokens, compacted`) and `<dir>/final.json`. The lab (`lab/goals/`) measures it.

## Checks

`cargo test -p app goals` (stream, cursor across compaction and restart, triggers in simulated time, ops checks and
caps, apply with ticks and clicked asks, compaction, seed and migration, the call's history and session, the lab
replay through the mock runner); `cargo test -p app islands::` for close-hide, plan_edit and responses.
