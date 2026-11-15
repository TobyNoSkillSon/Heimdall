---
name: writing-briefs
description: How to write a `delegate_task` brief and brief work the user will look at (pages, apps, images, comparisons). Use before every `delegate_task` except a one-line lookup, and before briefing a review.
metadata:
  class: public
---

# Writing briefs

Heimdall puts the default task rules above every brief by itself: Heimdall is untouchable, never move the user's
cursor, ask before system changes, never propose contacting anyone, done means checked the way the user meets it, and
the cleanup clause. Write only the task:

- The brief is the task's only context: the goal, the user's exact words, relevant files, what it may change, limits
  (money, publishing, memory), checks, what done means and what to return. Done means checked the way the user will
  meet it: the running app, the rendered page, not a passing test alone.
- Give every task a `path`: the project folder it works in, or the user's home folder for general work.
- Name the paths you know and let the task look; do not explore first. Set `isolation` to worktree when it will
  change code other work might touch.
- Read the editable `models` skill before choosing a target; respect explicit requests and actual access.
  For local-model work, include its precision rule explicitly in the brief.
- Name the task skill that fits (`task-skills` lists them).
- Keep what the user liked and fix only what they rejected; when they narrow one thing, keep the rest at full
  ambition.
- A brief that needs archived footage (what `archive_clip` gives you) asks the task to run `heimdall-clip --from "…"
  --to "…"` from Heimdall's app bundle (`Contents/MacOS/heimdall-clip`), not SQL.
- Where the user must choose among items, name the JSON file the picks are saved to; they click their picks rather
  than say them.
- A task that makes an HTML page names its output path in the brief, so the island opens it from the start; the task
  writes that absolute path in its thread once the file exists and in its final reply, so its island's open shows it.
- A long job with an arc (training, a judged improvement loop, a batch of merges, labelling, a download) keeps its island's
  status file: the brief asks the task to rewrite `<its folder>/island-status.json` whole after each step that moves
  the arc (a judged round, a merged branch, an eval, a thousand rows) and to name its absolute path in a command, so
  its island draws the one telling thing (the score curve against the target, branches merged of total, step of total
  with the loss) instead of a stage bar. Its JSON keys, all optional, every number measured: kind, now, progress
  {done, total, unit}, target {value, label}, series [{name, unit, points}], values [{label, value, unit}], items
  [{title, state: done|now|waiting|failed}], picture (an absolute path). At most 16 KB.

A task with several deliverables ends a turn as each one is ready, so the user hears of it then.

## Visual work

Briefs for anything the user will look at (a page, app, chart, image) ask the task to render the result at its real
size, check it, fix what fails and return the screenshot it checked. Read that screenshot before you tell the user it
is ready; never announce something visual you have not seen. When they have to compare outputs (audio samples,
images), the task builds one simple local comparison page, side by side; never hand them a bare folder of files.
Each item there gets players or images, a label and a clickable choice; picks persist, and Done saves them to the JSON
file the brief names. Every report a task delivers opens in the floating viewer (`islands_view` or its island).
