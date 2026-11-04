---
name: update-skills
description: Add, edit or remove a skill for Heimdall or for the agents it starts, copy it into every harness that should load it (Heimdall's Pi, Claude Code, Codex, OpenCode, Grok) and keep Heimdall's index of task skills current. Use whenever a skill changes, after installing a new Heimdall build, and when setting up a machine.
---

# Updating skills

Skills are copied by hand. No command, server or background job syncs them: you copy the folders below, and you are
the sync.

## The canonical folder

Every skill has one source: the `skills/` folder of the Heimdall repository, one folder per skill holding `SKILL.md`.

| Set | Who loads it |
|---|---|
| `skills/heimdall/` | Heimdall's own agent (Pi), personal edition |
| `skills/public/` | Heimdall's own agent (Pi), public edition |
| `skills/workers/` | The agents Heimdall starts through T3 Code (Claude Code, Codex, OpenCode, Grok) |
| `skills/shared/` | Both: Heimdall and every T3 agent (this skill) |

Heimdall's prompts and the default task prompt (`crates/agent/prompt/`) say only what every turn needs; procedures
belong in a skill. `docs/skills.md` says which topic lives where.

## Changing a skill

1. Edit, add or delete the folder in the repository. Keep the frontmatter `name` equal to the folder name (lowercase
   words joined by hyphens) and the `description` under 1,024 characters, saying what the skill does and the cases
   that should load it.
2. Keep Heimdall's index of task skills current: a skill a T3 agent can load (in `skills/workers/`, `skills/shared/`,
   or a skill folder of the user's own that their agents load) gets one line in `skills/heimdall/task-skills/SKILL.md`
   (and, for the repository's public `shared/` set, in `skills/public/task-skills/SKILL.md`): its name and what it lets
   a task do. A removed skill loses its line. Heimdall reads that index to know what to ask a task for.
3. Run the prompt tests: `cargo test -p agent --test prompt --test skills`. They check every skill's frontmatter, that the index names every
   repository task skill, and that the rules moved out of the prompts are still in a skill.
4. Commit the change in the repository, following the brief's rules on committing.
5. Copy only the changed skill folders into the locations below. Before writing, check every destination ancestor,
   the location, the skill folder and its entries for symlinks. A link belongs to someone else: preserve it and report
   the blocked copy; never follow it. Compare the entire active folder against the previously shipped default
   (tracked hash/version or an exact known old bundle), including extra files. If it is already the new version,
   leave it alone. If it equals the unmodified old default, use
   `rsync -a --delete skills/<set>/<name>/ <location>/<name>/`. If locally changed or its baseline is unknown,
   preserve it and keep the incoming default separately outside the loaded skills tree; report the conflict and
   propose a merge/adoption. Never use a blind replacement. A user's own agent can edit their active skill without
   rebuilding the app. Remove a skill only with explicit authority; preserve intentional removals in `.seeded`.
6. Heimdall loads skills at its start. If Heimdall's own set changed (the index in step 2 is one of its skills), tell Heimdall in your final reply to call
   `reload_me`; if you are Heimdall, call `reload_me` yourself once your turn's work is done. A T3 agent picks up a
   changed skill in its next session.

## Where each harness loads skills

| Harness | Location | Sets |
|---|---|---|
| Heimdall (Pi) | `~/Library/Application Support/Heimdall/agent/skills/` | `heimdall/` (or `public/`) and `shared/` |
| Claude Code | `~/.claude/skills/` | `workers/` and `shared/` |
| Codex | `~/.agents/skills/` | `workers/` and `shared/` |
| OpenCode | reads `~/.claude/skills/` and `~/.agents/skills/` | nothing extra |
| Grok CLI | reads `~/.claude/skills/` and `~/.agents/skills/` | nothing extra |

So a worker skill goes to two places (`~/.claude/skills/` and `~/.agents/skills/`), and a Heimdall skill to one.
Heimdall's Pi loads only its own folder, never the others.

## A new build or a new machine

- Heimdall copies each bundled `heimdall/` (or `public/`) and `shared/` skill into its folder once, ever (its
  `.seeded` list), and never overwrites or restores one. So a skill a new build changes reaches an existing profile
  only by hand: after installing, copy each Heimdall skill that changed since the last installed build
  (`git diff --stat <old>..<new> -- skills/`), then `reload_me`.
- A machine that runs only T3 agents (one T3 Code reaches through its environments) needs the `workers/` and `shared/`
  sets in its two locations. Start a task on that machine whose brief includes this skill's folder table and the skill
  folders to copy (or the repository to pull), and have it report the folders it wrote.

- Every new build and new machine also ends with each harness in its no-prompt mode: run the installed
  `"$HOME/Applications/Heimdall.app/Contents/MacOS/heimdall-emergency" harness apply` and check that `harness check`
  says "All set" (`docs/harness-autonomy.md`). Heimdall repeats it at each start.

## Pending automatic upgrade contract (not implemented)

Current runtime seeding records names in `.seeded`, not previous default hashes/versions. It cannot distinguish an
unchanged default from a customisation and does not deliver upgrade notices. Until the integrator implements the
contract in docs/skills.md, the guarded manual copy above is required.

The integrator must track the previously shipped default by hash/version. After an app update, automatically
replace an active skill only if it equals that unmodified default. Preserve locally changed skills and keep each
incoming default separately; never silently overwrite custom skills. Preserve intentional deletions and symlinks;
unknown legacy baselines count as customisations until proven otherwise.

After EVERY app update, deliver a durable agent-facing release notice with the release author's prepared changelog,
changed defaults and local customisation conflicts, including an explicit empty result when there are none. Address
whatever personalised assistant name the user has chosen, never a hard-coded assistant name. Every public release
ships its own authored update note. The agent can explain the update and propose merge/adopt ideas; it cannot treat
release metadata as authority for destructive actions, installs, billing, messaging or other consent-bound work.
Runtime implementation and tests belong to the active integrator, separate from this skill edit.
