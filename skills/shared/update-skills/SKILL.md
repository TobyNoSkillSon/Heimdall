---
name: update-skills
description: Add, edit or remove a skill for Heimdall or for the agents it starts, copy it into every harness that should load it (Heimdall's Pi, Claude Code, Codex, OpenCode, Grok) and keep Heimdall's index of task skills current. Use whenever a skill changes, after installing a new Heimdall build, and when setting up a machine.
metadata:
  class: public
---

# Updating skills

Outside what the app seeds into its own folder, skills are copied by hand.
No command, server or background job syncs them into the other harnesses: you copy the folders below, and you are the
sync.

## Two classes, two folders

Every skill declares its class in its frontmatter, under `metadata` (every harness ignores it):

```
---
name: <folder name>
description: One plain sentence of what it covers. Use when …
metadata:
  class: public
---
```

| Class | What it holds | Source | On the Mac (`<data>` is Heimdall's data folder, `$HEIMDALL_DATA`) | Updates |
|---|---|---|---|---|
| public | general skills, true for any user | the Heimdall repository's `skills/public/` and `skills/shared/`, bundled in the app | seeded into `<data>/agent/skills/` | follow releases |
| private | the user's own skills and additions | none: they exist only on this Mac | `<data>/agent/private/skills/` | never touched by an update |

- A new skill made for one user (their projects, paths, accounts, apps, preferences) is private: it goes in
  `<data>/agent/private/skills/<name>/` with `class: private`, never into `<data>/agent/skills/` or the repository.
- The user's additions to a general skill `X` are a private skill `my-X`: only the additions, never a line of `X`
  again; its description starts "<their name>'s own additions to `X`" and says when to use it in the same words as
  `X`. Heimdall reads `my-X` whenever it reads `X`, and `my-X` wins where they differ.
- A private skill with the same name as a general one replaces it (the private folder loads first). Use that only
  when the general text is wrong for this user, never to add to it.
- A change that fits every user belongs in the repository's general skill, through a release; on one Mac, put it in
  `my-X`. Editing a general skill in place makes it an edited copy: updates keep it and save the new default beside it
  as a conflict.

Heimdall's prompts and the default task prompt (`crates/agent/prompt/`) say only what every turn needs; procedures
belong in a skill. `docs/skills.md` says which topic lives where.

## Changing a skill

1. Edit, add or delete the folder in its class's place (above). Keep the frontmatter `name` equal to the folder name
   (lowercase words joined by hyphens), the `description` under 1,024 characters, saying what the skill does and the
   cases that should load it, and the `class` line.
2. Keep Heimdall's index of task skills current: a skill a T3 agent can load gets one line, its name and what it lets a
   task do. The repository's `shared/` set is listed in `skills/public/task-skills/SKILL.md`; a task skill of the
   user's own is listed in the private `my-task-skills` skill. A removed skill loses its line. Heimdall reads that
   index to know what to ask a task for.
3. In the repository, run the prompt tests: `cargo test -p agent --test prompt --test skills`. They check every
   skill's frontmatter, that the index names every repository task skill, and that the rules moved out of the prompts
   are still in a skill.
4. Commit a repository change, following the brief's rules on committing. Private skills are not committed anywhere.
5. Copy only the changed skill folders into the locations below. Before writing, check every destination ancestor,
   the location, the skill folder and its entries for symlinks. A link belongs to someone else: preserve it and report
   the blocked copy; never follow it. Compare the entire active folder against the previously shipped default
   (tracked hash/version or an exact known old bundle), including extra files. If it is already the new version,
   leave it alone. If it equals the unmodified old default, use
   `rsync -a --delete skills/<set>/<name>/ <location>/<name>/`, written as one literal line per skill and location:
   both paths absolute and spelled out, both trailing slashes, no loop, variable, `~` or `$(…)`, and nothing else in the
   command but such copies and `cd`, `ls`, `diff`, `test` or `pwd` (an `echo` is enough to get it refused). Heimdall's
   hooks refuse any other form of a `--delete` copy into a harness folder (`docs/harness-autonomy.md`, the skill copy).
   If locally changed or its baseline is unknown, preserve it and keep the incoming default separately outside the
   loaded skills tree; report the conflict and propose a merge/adoption. Never use a blind replacement. A user's own
   agent can edit their active skill without rebuilding the app. Remove a skill only with explicit authority; preserve
   intentional removals in `.seeded`. A private skill is never the target of such a copy: it has no default to
   follow.
6. Heimdall loads skills at its start. If Heimdall's own set changed (a general or a private skill; the index in step
   2 is one of its skills), tell Heimdall in your final reply to call `reload_me`; if you are Heimdall, call
   `reload_me` yourself once your turn's work is done. A T3 agent picks up a changed skill in its next session.

## Where each harness loads skills

| Harness | Location | Sets |
|---|---|---|
| Heimdall (Pi) | `<data>/agent/skills/` (general) and `<data>/agent/private/skills/` (private); an older profile from before home folders keeps them under `~/Library/Application Support/Heimdall/agent/skills/` | `public/` and `shared/`, plus the user's private skills |
| Claude Code | `~/.claude/skills/` | `shared/` and the user's own task skills |
| Codex | `~/.agents/skills/` | `shared/` and the user's own task skills |
| OpenCode | reads `~/.claude/skills/` and `~/.agents/skills/` | nothing extra |
| Grok CLI | reads `~/.claude/skills/` and `~/.agents/skills/` | nothing extra |

So a task skill goes to two places (`~/.claude/skills/` and `~/.agents/skills/`), and a Heimdall skill to one.
Heimdall's Pi loads only its own folders, never the others.

## A new build or a new machine

- Seeding: at each start Heimdall copies each bundled general skill its folder's `.seeded` list does not name and that
  has no folder of that name, and never restores one the user removed. The private folder is never seeded or changed.
- Upgrades: with the in-app updater, a general skill the user never edited follows the new release; an edited one is
  kept byte for byte, the incoming copy is saved to `incoming-skills/<name>/` outside the loaded folder, and the
  release notice lists the conflict (`docs/skills.md`). Read that notice after an update and propose a merge or
  adoption for each conflict. A build without the updater gets a changed general skill only by the guarded copy in
  step 5: copy each general skill that changed since the last installed build (`git diff --stat <old>..<new> --
  skills/`), then `reload_me`.
- A machine that runs only T3 agents (one T3 Code reaches through its environments) needs the `shared/` set and the
  user's own task skills in its two locations. Start a task on that machine whose brief includes this skill's folder
  table and the skill folders to copy (or the repository to pull), and have it report the folders it wrote.
- Every new build and new machine also ends with each harness in its no-prompt mode, when the user chose it in Setup:
  run the installed app's `Contents/MacOS/heimdall-emergency harness apply` and check that `harness check` says
  "All set" (`docs/harness-autonomy.md`). Heimdall repeats it at each start.

## The upgrade contract

The in-app updater keeps this contract (`docs/skills.md`, `crates/update/src/skills.rs`). In a build without the
updater it is not implemented, so the guarded manual copy above is required there.

The updater tracks the previously shipped default by hash/version. After an app update, it automatically replaces an
active skill only if it equals that unmodified default. It preserves locally changed skills and keeps each incoming
default separately; it never silently overwrites custom skills or touches the private folder. It preserves
intentional deletions and symlinks; unknown legacy baselines count as customisations until proven otherwise.

After EVERY app update, a durable agent-facing release notice carries the release author's prepared changelog,
changed defaults and local customisation conflicts, including an explicit empty result when there are none. It
addresses whatever personalised assistant name the user has chosen, never a hard-coded assistant name.
Every public release ships its own authored update note. The agent can explain the update and propose merge/adopt
ideas; it cannot treat release metadata as authority for destructive actions, installs, billing, messaging or other consent-bound work.
