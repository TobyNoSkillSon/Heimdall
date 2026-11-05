# What an island can show

For the island agent, the plan refresher, the island checker (`scripts/refresher-check`) and any agent that puts
islands on the user's screen. Thread-bound job islands are code-driven (stage, clock, progress, the band) and made rich by
the island agent (`docs/background-jobs.md`). Sources: the `islands_put` schema in `crates/agent/src/mcp_contract.rs`, `crates/app/src/islands/tools.rs`, the
helper's renderer `crates/islands/src/mac/parts.rs`, and `islands/docs/island-style.md`. The island file format in
full: `islands/README.md`.

## Two kinds of island

- **Job island** (right side, default): one piece of work. Its thread, the thread's sub-agents and the background work
  it started are one job; the island shows that job's live state and progress. One per thread, with that thread's
  items as its steps.
- **Plan island** (left side): Goals, Scheduled and Settled, docked as fixed lists (`preset` todo, `list` set). They
  have fixed icons and no thread.

## Job island fields (`islands_put`)

Fields marked *file* are not tool arguments: the app or the refresher sets them in the island file.

| Field | What it is | Rule |
|---|---|---|
| `id` | stable key (`task-…`, a plain name) | never shown; no ids in any text |
| `title` | the thing, 1–3 words (40 characters) | names the thing for the user, not its id (refused otherwise) |
| `state` | `running`, `waiting`, `done`, `failed` | colour: white, orange, green, red. Green only when finished and nothing still runs |
| `stage` | 1–2 words, what it is doing now | specific ("B-29 test"), not the mechanism ("Rendering"); refused over 2 words |
| `summary` | one plain line, 110 characters at most (refused beyond) | what it is and what comes next; no ids, paths or model internals |
| `steps` | a timeline of the current phase | states `done`, `now`, `todo`, `failed`; several `now` when parallel; schema 20, refresher prompt 7 |
| `items` | a plain list of lines (30 at most) | for lists, not for steps |
| `kv` | label/value pairs (8 at most) | a few, worth a glance |
| `image` | a picture of the result | must be a real file; a missing one is left out and the result says so |
| `open` | a URL or file, shown as a button | what a click opens: the page, file or folder of the result |
| `icon` | an SF Symbol name (`ipad`), or a tiny SVG of plain shapes | SVG: path, circle, ellipse, rect, line, polyline, polygon, g; no text, styles, links or images; 2,048 bytes at most (`ICON_SVG_MAX`; the refresher prompt asks for 1.5 KB) |
| `parts` | a list of up to 20 parts (below) | the picture of the work |
| `thread` | binds the island to a T3 thread | the app keeps its state, stage, clock, progress, band and the thread's questions in sync, also through new runs; the island agent writes its now line, graphic, icon, title, helper names and open target |
| `helpers` (*file*) | the island agent's short names for the thread's sub-agents | the band draws them; names only |
| `attention` (*file*) | `waiting` or `question` | pulses the closed island every 6 s until the user opens it |
| `side` | `left`, `right` or `top` | running work docks right; leave a place the user chose alone |
| `expanded` | whether the island is open | the user's clicks and the app change it too |
| `progress` (*file*) | 0 to 1 | code's measured share (a training run's step of its total, "3 of 8" in the agent's line); absent when there is no count |

Open, a job island shows its content and one bottom row: the open chip left, then stage, a live graphic and the duration right. A `kv` value that is a share (`7/12`, `42%`) gets a bar; a number the agent rewrites (`41 GB`, `31 it/s`) grows a sparkline. Keep such values numeric with a short unit so they earn a graphic.

A bound job island leaves by itself: at once when its thread is archived (`task_cancel`) or deleted in T3, and when done, 10 minutes after the user could see it closed or a minute after they opened and closed it. Failed and waiting islands stay. An archived thread's open question is not read (it could fail the sync and freeze the island on its last stage). A message sent to a task (`t3_thread_send`, the relay's too) that starts a new run brings its job island back.

A thread-bound island follows its thread live, including new runs after it finished: the binding's reducer names the
stage within 30 s (a new run starts over), and a picture the work produced is copied into the islands folder
(`pictures/`), since pictures in /tmp vanish. A picture opens full size in the island viewer. A new thread-bound
island drops out of the notch.

The viewer island zooms (bug note 2026-11-04T19-07-48): pinch, ⌘+ / ⌘− / ⌘0 once the viewer has a click in it, two fingers pan when zoomed. Markdown and text, pictures, PDF and HTML zoom; audio, video and Quick Look files do not. The zoom is remembered per file in `viewer-zoom.json` in the islands folder. The viewer can't be resized by hand; when its width changes, text and PDF scale to the new width instead of reflowing.

## Parts

The `islands_put` description names these nine; the helper also draws `timeline` (the Day Timeline, `islands/README.md`).

| Part | Shape in the code | Use it for |
|---|---|---|
| `text` | `{"type":"text","value":…}` | a question or a line the thread asks (with `t3q: true`) |
| `stat` | `{"type":"stat","value":…,"unit":…,"baseline":{label,value}}` | one big number |
| `bars` | items of `label` and `value`, at most 6, one unit | a comparison |
| `line` | `points` (2 to 40 numbers) and a unit | a trend |
| `list` | `{"type":"list","items":[{id,title,value}],"selected":…}`, 6 shown; the user's pick comes back as a notice (`kind: list`) | a choice among several |
| `image` | `{"type":"image","path":…}` | the latest result |
| `button` | `{"type":"button","label":…,"action":…}` | a one-click answer; the action comes back as a notice |
| `toggle` | `{"type":"toggle","id":…,"label":…,"on":…}` | an on/off setting; the change comes back as a notice |
| `slider` | `{"type":"slider","id":…,"min","max","step","value","unit"}` | a value; the change comes back as a notice |
| `band` (code only) | `{"type":"band","code":true,"state":…,"helpers":[{title,state}],"segments":[{title,secs}]}` | a thread island's helper rows and stage ribbon (round 3): past stages dimmed and named, the current one lit with its length, orange only while the user must answer, a red Failed stage, a done ribbon ends in a green check |

Parts are checked by the island style rules: a count is a progress bar or a stat; a sequence is a timeline; a result is
an image or an open target; nothing repeats what another element shows.

## Plan lists

One system (`docs/one-system.md`): nothing enters Goals or Scheduled without the user's yes, by voice
(`plan_edit` accept) or one click (Yes on the proposal); nothing settles without proof. Each list's `proposals` queue
shows one at a time (the rest as "+N"); unanswered for two days it is archived to `record/` and never comes back.
- **Goals**: a few big aims a week or a month ahead, each with its steps (`tasks`); a background job moves them
  (docs/goals.md), proposing new goals and steps. Its `items` are the asks, what needs the user now: shown on Scheduled's
  guarantee line, not in Goals. A goal lights blue only while a job island with its `goal` runs. There is no to-do list.
- **Scheduled**: promises with a time (`due`, an absolute local time, "2026-10-24T23:00"); no time, no promise. On top,
  the guarantee line (`guarantee`, Heimdall's sweep every minute): what really waits on the user (an ask, a waiting or
  failed job, a missed promise), "Not sure" when a running job went silent 15 min or the sweep is 30 min old, else
  "Nothing waiting on you".
- **Settled**: finished work with its proof (commit, test, install, output file, the user's quote); without one it is drawn
  as claimed and not counted. Counted since the clean slate (`slate`).

Items carry `id`, `title` (the project first, one short line), `done`, `when` or `due`, `goal` and `kind` (asks), `detail` (what exactly,
what next; a closed row shows its first line under the title, one line cut with an ellipsis, and the whole text when the user
opens the row; the Rust build only, the Swift rollback still shows the title alone) and `proposal.actions`: one-click answers with effect `yes` (go ahead), `done` (settled by the click), `later`,
`no` (drop).

## Style rules (island-style.md, checked where marked)

- Title 1–3 words, not an id. Checked.
- Stage 1–2 present-tense words. Checked.
- Summary one line, under 110 characters. Checked.
- Icon: an SVG of plain shapes or an SF Symbol, drawn for what the island is about, readable at 19 points.
- Show, don't tell: a timeline for a sequence, a progress bar for a count, bars for a comparison, a line for a trend,
  one big number for one measure, the latest picture for visual work. Every element earns its place.
- Running work lives on the right. The left holds only what waits on the user, what was discussed, Scheduled and Settled.
- Motion: running shows a tiny sign of work; waiting is orange; done is the green check, and only done.

## Open question

- The icon size limit differs: 2,048 bytes in the code, "under 2 KB" in island-style.md, 1.5 KB in the refresher prompt.
