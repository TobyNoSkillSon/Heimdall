# What an island can show

The fields and parts an island carries, how a job island follows its work, and when it leaves. For the island agent,
the plan refresher, the island checker (`scripts/refresher-check` reads this page) and any agent that puts islands on
the user's screen.

Read this when: you want to put or update an island with `islands_put`, pick a part for the work, bind an island to a
T3 thread, know why an island left or stayed, or read what is up with `islands_list`.
Code: `crates/agent/src/mcp_contract.rs` (`islands_put`, `islands_list`, `islands_remove` schemas),
`crates/app/src/islands/tools.rs`, `crates/app/src/islands/binding.rs`, `crates/app/src/islands/kept_report.rs`,
`crates/app/src/islands/list.rs`, `crates/islands/src/mac/parts.rs` (the renderer).

## Two kinds of island

- **Job island** (right side, the default): one piece of work. A thread, its sub-agents and the background work it
  started are one job; the island shows that job's live state and progress, one per thread.
- **Plan island** (left side): Goals, Scheduled and Settled, fixed lists with fixed icons and no thread
  ([one-system.md](one-system.md)).

The viewer is a separate island for one file: [viewer.md](viewer.md).

## Job island fields

Tool arguments of `islands_put` (only `id` is required; it merges into the island with that id, and an empty list
clears it). Rows marked *file* are not arguments: the app writes them into the island file. How the words must read:
[island-style.md](../islands/docs/island-style.md).

| Field | What it is | Limit or rule |
|---|---|---|
| `id` | stable key (`task-…`, a plain name) | 64 characters; never shown |
| `title` | the thing, 1–3 words | 40 characters; checked |
| `state` | `running`, `waiting`, `done`, `failed` | |
| `stage` | 1–2 words, what it is doing now | checked |
| `summary` | one plain line | 110 characters; checked |
| `steps` | `{title, state}` timeline of the current phase | 20; states `done`, `now`, `todo`, `failed`; several `now` when parallel |
| `items` | `{title, group, done}` plain list | 30; groups `running`, `waiting`, `discussed` |
| `kv` | `{label, value}` pairs | 8 |
| `image` | a picture of the result | must be a real file; a missing one is left out and the answer says so |
| `open` | a URL or file, shown as a button | what a click opens |
| `icon` | an SF Symbol name or a tiny SVG | 2,048 bytes (`ICON_SVG_MAX`); see island-style.md |
| `parts` | the open island's content (below) | 20 |
| `side` | `left`, `right` or `top` | running work docks right |
| `expanded` | whether it is open | the user's clicks change it too |
| `thread` | binds it to a T3 thread | see below |
| `sent` | what the job was sent to do | one line, 110 characters; written once, never replaced |
| `goal` | the Goals id it serves (`plan_edit show`) | a chip; lights that goal while it runs |
| `progress` (*file*) | 0 to 1 | code's measured share; absent when there is no count |
| `helpers` (*file*) | the island agent's short names for the thread's sub-agents | the band draws them |
| `attention` (*file*) | `waiting` or `question` | pulses the closed island every 6 s until opened |

Open, a job island shows its content and one bottom row: the open target's chip on the left; the stage, a live graphic
and the duration on the right. A `kv` value that is a share (`7/12`, `42%`) gets a bar; a number the agent rewrites
(`41 GB`, `31 it/s`) grows a sparkline. The full file format: [islands/README.md](../islands/README.md).

## Bound to a thread

- `thread` binds an island to a T3 thread. Every task Heimdall starts gets its job island before the call returns:
  running, docked right, titled from the task title as a placeholder, id `task-<title>`.
- The binding then keeps state, stage, clock, progress, the band and the thread's questions in sync, through new runs
  too (a new run starts over). The island agent writes the now line, the graphic, the icon, the title (over the
  placeholder and its own earlier name, never over a name `islands_put` gave), the helpers' names and the open target.
  Cadence and triggers: [one-system.md](one-system.md).
- A picture the work produced is copied into the islands folder (`pictures/`), since files in `/tmp` vanish.
- A message sent to a task (`t3_thread_send`) that starts a new run brings its job island back.

## When a job island leaves

- At once when its thread is archived (`task_cancel`) or deleted in T3.
- Done, 60 s after its result reached the user (`binding::RESULT_GRACE`): the run's final notice was delivered to
  Heimdall, which spoke it or showed it on the island. That minute counts whether the user is at the Mac or islands are
  hidden; it waits while the user has the island open and restarts when it is closed. A result told before the
  island's current run started does not count.
- Done without a told result: after 2 min shown closed (`DONE_SHOWN_FOR`), or 30 s after the user opened and closed it
  (`DONE_AFTER_OPENED`). Hidden time does not count.
- Failed and waiting islands stay.
- A done island that delivered a file (its `open` target, else the page, document, picture or recording its final reply
  named, else its picture) turns in place into a report island (`kept_report.rs`): its title and icon, the green check
  (`preset: "result"`), its open button, unbound. It stays until the user opens or closes it; at most 3
  (`KEPT_AT_MOST`), the oldest going first. A file the user already opened from it after it finished, or a job with no
  file, goes as before. Every such file is also written to Settled as its proof.
- Viewers and report islands are never bound, so none of this closes them.

## What is up: `islands_list`

Read-only (`crates/app/src/islands/list.rs`): one line per job, viewer, kept report or other island with its id (for
`islands_remove`), kind, bound task, state, time up, time since it finished, open or closed on screen, and for a done
job whether its result was told and when it leaves. Hidden islands are left out; the shown plan lists come in one line.

```
job 'Voice isolation' (id task-voice-isolation, task mcp:…): done 23 min ago, up 41 min, closed on screen, result told 22 min ago; leaves 1 min after that unless the user has it open.
```

`islands_remove` closes islands by id; on a plan list it hides the list instead.

## Parts

The `islands_put` description names the first nine; the renderer also draws `progress`, `html`, `band` and `timeline`.
Consecutive buttons share a row, at most three.

| Part | Shape | Use it for |
|---|---|---|
| `text` | `{type, value}` | a question or a line the thread asks (with `t3q: true`) |
| `stat` | `{type, value, unit, baseline: {label, value}}` | one big number |
| `bars` | `{type, items: [{label, value, highlight}], unit, max}`, 6 drawn | a comparison |
| `line` | `{type, points, unit}`, at least 2 points | a trend |
| `list` | `{type, items: [{id, title, value}], selected}`, 6 drawn | a choice; the pick comes back as a notice (`kind: list`) |
| `image` | `{type, path}` | the latest result |
| `button` | `{type, label, action}` | a one-click answer; comes back as a notice |
| `toggle` | `{type, id, label, on}` | an on/off setting; the change comes back as a notice |
| `slider` | `{type, id, min, max, step, value, unit}` | a value; the change comes back as a notice |
| `progress` | `{type, value}` | a bar from 0 to 1 |
| `html` | a fragment with a `fallback` | a model-drawn body ([model-html.md](../islands/docs/model-html.md)) |
| `band` (code only) | `{type, code: true, state, helpers: [{title, state}], segments: [{title, secs}]}` | a thread island's helper rows and stage ribbon |
| `timeline` | `{type, from, now, blocks: [{start, end, kind, title, line}]}` | the Day Timeline (`crates/islands/src/model/timeline.rs`) |

Pick by what the work is: a count is a progress bar or a stat; a sequence is a timeline of steps; a result is an image
or an open target; nothing repeats what another element shows.

## See also

- [island-style.md](../islands/docs/island-style.md): how titles, stages, colours and icons must read
- [one-system.md](one-system.md): the plan lists and how job islands stay current
- [viewer.md](viewer.md): the viewer island
- [model-html.md](../islands/docs/model-html.md): HTML bodies and the status file
- [islands/README.md](../islands/README.md): the file protocol and CLI
- [t3-orchestration.md](t3-orchestration.md): starting and following T3 tasks
