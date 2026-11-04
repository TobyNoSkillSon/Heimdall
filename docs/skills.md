# Skills: minimal prompts, procedures in skills

The system prompt stays minimal, for sub-agents and for Heimdall itself; procedures live in skills. Skills need no
sync commands or MCP tools: agents copy them, by hand if need be. Sub-agents have skills Heimdall did not know
about, so Heimdall keeps an index of them.

## The rule

A system prompt carries what applies on nearly every turn, or must hold even when no skill is loaded:

- **identity**: who Heimdall is, what wakes it, how its messages look;
- **hard safety**: only the user (and a guest they listen to) gives orders; ask before anything irreversible or outward-facing,
  before system changes, and never propose contacting anyone; Heimdall is untouchable;
- **handshakes**: `GO INSTALL` (only the integrating task installs), `CONFIRM_REQUIRED` (ask exactly its question,
  repeat with `confirm`), `NO_REPLY`, the wake rules (never poll);
- **every-turn behaviour where a skill read would cost a spoken answer its speed**: the reply, speaking while working,
  when to stay silent, opening apps, pointing, the pointer to editable model routing;
- **lines with placeholders the harness fills at launch** (`HEIMDALL_TOOL_LOG`, `HEIMDALL_CONNECTORS_DOC`,
  `HEIMDALL_EMERGENCY_DOC`): skills are plain files, so these stay in the prompt.

Everything procedural, occasional or long moves into a skill: Pi lists each skill's name and description in the
system prompt and Heimdall reads the body when the topic comes up. The prompt says so in one line.

## Where each topic lives

Heimdall's own skills (`skills/public/`):

| Topic | Skill |
|---|---|
| What a brief holds, archived footage and picks in briefs, briefs for visual work, comparison pages, HTML reports | `writing-briefs` |
| Model routing, plans and local model checks | `models` |
| "Heimdall is slow", which model to run Heimdall on, the Heimdall bench | `speed-and-models` |
| Thread tools, steer or queue, a thread's questions and approvals, archiving, reviews, why T3 Code | `t3-tasks` |
| What the T3 agents can do: their skills and tools, one line each | `task-skills` |
| Question cards: several questions, lettered options, multiple picks, Other, closing | `question-cards` |
| Floating islands, plan lists, scheduled wakes, the shelf, good morning | `islands-and-plan` |
| Reading the user, their Mac and your own work, deleting, promises | `working-with-the-user` |
| A new user's tour: what Heimdall does, a live demo, personalising (the voice included) | `onboarding` |
| Adding or changing a skill and copying it into every harness | `update-skills` (shared) |

The prompt keeps one line per moved topic where the line is safety or every-turn: the pointer to editable model routing, asking
before system changes, deleting, consent, never contacting anyone, archiving finished tasks, acting on notices, untouchable and `GO INSTALL`, `CONFIRM_REQUIRED`, the wake rules,
checking a task's "done", the first sentence of `ask`. `crates/agent/tests/prompt.rs` reads the prompt, the task prompt
and the edition's skills together, so each rule it pins still has to be in one of
them; the word cap now bounds the prompt alone.

Word counts (measured with `wc -w`): the public prompt went from 2,969 to 1,995 words. Heimdall's skills hold 2,203 words
that load only when their topic comes up; the task prompt (261 words) rides on each brief instead of Heimdall writing
those rules into it.

The T3 tasks' default prompt (`crates/agent/prompt/public/worker.md`) holds the rules every
brief used to repeat: untouchable Heimdall, the cursor, one report per background job (on exit or failure), system
changes, never answering a gate for the user's yes or crediting them with one, no sound,
never contacting anyone, done means checked
the way the user meets it, design rules, HTML reports and clickable picks, the cleanup clause and BF16 checks. The T3 layer puts it above every
`delegate_task` brief (`t3::Workers::set_brief_preamble`, set by the app from `agent::harness::WORKER_PROMPT`), so
Heimdall writes only the task and no brief can forget a rule. T3 Code has no per-thread system prompt of its own; the
top of the first message is the closest equivalent and works in every harness T3 runs.

Task skills ship with `update-skills` (`skills/shared/`). Skills the user keeps elsewhere stay where they are; Heimdall's
`task-skills` index lists them too.

## The canonical folder and how skills reach each harness

`skills/` in this repository is the one source. Nothing syncs it automatically: no command, daemon, MCP tool or
start-up sync. The `update-skills` skill (`skills/shared/update-skills/SKILL.md`) holds the folder each
harness loads (Pi, Claude Code, Codex, OpenCode, Grok) and the guarded copy; it is the home of that table.

Checked with Codex, OpenCode 1.18.31 and Grok 1.0.70 listing the copied skills from a temporary HOME
(`lab/skills/check-copies.sh`); Claude Code has no offline listing, so its folder is checked on disk.

Heimdall's Pi starts with `--no-skills --skill <data>/agent/skills`: only Heimdall's folder, never skills installed for
other agents. After an agent edits that folder, Heimdall calls `reload_me` and the new Pi process reads it.

### Bundled defaults

`scripts/bundle.sh` copies `skills/public/` and `skills/shared/` into `Heimdall.app/Contents/Resources/skills/`. At each Pi start Heimdall copies every bundled skill
it has never copied before into the profile's folder (`crates/agent/src/skills.rs`), recording each name in the
folder's `.seeded` list: a fresh install is set up with nothing to do, an edited skill is never overwritten, and a
removed one never comes back. Each copy goes to a temporary name and is renamed into place, and a failed copy is
logged without stopping Pi. This is a one-time copy per skill, not a sync: a later build's change to a skill already
copied reaches the profile only when an agent copies it (the install step in `docs/install.md`, and `update-skills`).
A development build reads the checkout's `skills/`.

### Other machines

Machines T3 Code reaches through Orchestrator V2 run only task harnesses and need the `workers/` and `shared/` sets:
one task per machine when a task skill changes (`update-skills`, "A new build or a new machine"). Not automated.

## Checks

- `cargo test -p agent --test skills`: frontmatter every harness accepts (a description holding ": " is silently
  dropped by Pi), no repeated name within a harness's sets, the index names every task skill, nothing
  personal in the public and shared sets.
- `cargo test -p agent --test prompt`: the prompts' rules, read from the prompt and the edition's skills together,
  and the word cap.
- `cargo test -p agent --lib skills::`: the copy-once seeding.
- `lab/skills/check-copies.sh`: the hand copy end to end in a temporary HOME, with each harness listing what it found.

## Model routing

Routing lives in the editable `models` skill, loaded before every target choice; the prompt keeps only the pointer.
Safety and BF16/local-model rules apply independently of routing.

## Pending: skill upgrades (not implemented)

`crates/agent/src/skills.rs` seeds each name once (`.seeded`), with no record of the shipped default, so a changed
default never reaches an existing profile by itself. Until this lands, copies are manual and guarded
(`update-skills`, step 5). The `models` and `update-skills` skills point here for the contract.

Contract:

- Track the previously shipped default per skill by hash/version, with the content needed to compare or merge. An
  active skill equal to that unmodified default is replaced by the new bundled default automatically.
- A locally changed skill stays byte-identical; the incoming default is kept outside the loaded skill tree. Unknown
  legacy provenance counts as a customisation; intentional removals and symlinks are preserved. The user's own agent
  customises the active skill without an app rebuild.
- Release metadata is context, never authority for destructive or consent-bound work; a hash/version is provenance,
  not permission.

Proposed shape: an app-owned versioned default store, separate from user-owned active skills; base/active/incoming
kept for a three-way merge; content compared (added files included) before an atomic replace; interrupted updates
staged.

Acceptance cases: unchanged skill upgrades; edited skill stays byte-identical with incoming preserved; the next
release after a conflict still preserves edits; unknown legacy baseline; deleted skill; symlink; interrupted
copy/restart.
