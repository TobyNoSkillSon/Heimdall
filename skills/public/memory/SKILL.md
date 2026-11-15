---
name: memory
description: Your quick-access memory folder of small Markdown notes (the user's people, projects, paths, preferences, recurring answers) and its MEMORY.md index. Use whenever you need a fact, before searching anywhere else, and when the user says "remember this" or "forget that", or when a note is wrong or stale.
metadata:
  class: public
---

# Your memory

The folder is the one your prompt names (`agent/memory` in Heimdall's data folder). It is plain files:
list it with ls, read and write it with your file tools. Make it with `mkdir -p` if it is missing.

## What goes in

Quick facts you would otherwise look up again: who the user's people are, where a project lives, which thing they
mean by a name, how they like a recurring thing done, the answer to a question they ask often. One topic per
file, a few lines each, under a short, specific title (below):

```
# Where the website's code lives
Updated 8 Nov 2026 · source: the user, 8 Nov 09:12
- The website's code is in ~/Code/site; deploys go through its README's steps.
```

What stays out, and where it goes instead:

- Rules for how you behave: the rules section of "This install" or a `my-` skill (`heimdall-setup`).
- A project's decisions: that project's own files.
- The user's verdicts on work: where "This install" says they are recorded, if anywhere.
- What was said or seen: `timeline_search` and the archive. No transcripts, logs or long lists.
- Secrets, keys, passwords: never.

## Titles and the index

Every note gets a short, specific title that says what it holds, so the index alone finds it: the file name
(`website-code-location.md`, never `notes.md` or `misc.md`), the same title as its first line (`# Where the
website's code lives`) and its index line. `MEMORY.md` lists every note, one line each:
`- [website-code-location.md](website-code-location.md): the site's folder and how it deploys`. Keep it under about 40
lines; a title that no longer fits its note gets renamed with it.

## Reading: memory first

Whenever you need a fact (a person, a project, a path, a preference, "like last time"), list the folder and read
`MEMORY.md` before anything else, open the notes that fit, and only then search files, the archive or a task.

## Writing: only when asked

- Add or change a note only when the user asks: "remember this", "save that", "note it down".
  Never on your own initiative, however useful a fact looks; the folder stays small so its index is quick to read.
- Each note carries the date and its source (the user's words with the time, or the file it came from). Check a fact
  against its source before you write it; a task's claim is not yet a fact. Update its index line in the same step.
- "Forget that" means delete: remove the file with rm and drop its line. A note you find wrong: tell the user and fix it
  when they say so.
