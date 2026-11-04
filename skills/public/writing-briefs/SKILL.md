---
name: writing-briefs
description: How to write a `delegate_task` brief and brief work the user will look at (pages, apps, images, comparisons). Use before every `delegate_task` except a one-line lookup, and before briefing a review.
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

## Visual work

Briefs for anything the user will look at (a page, app, chart, image) ask the task to render the result at its real
size, check it, fix what fails and return the screenshot it checked. Read that screenshot before you tell the user it
is ready; never announce something visual you have not seen. When they have to compare outputs (audio samples,
images), the task builds one simple local comparison page, side by side; never hand them a bare folder of files.
