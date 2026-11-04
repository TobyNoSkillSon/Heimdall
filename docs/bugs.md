# Bug notes

Personal edition only. The public edition has no bug notes, inbox, scout, menu row or fixing round; its prompt
(`crates/agent/prompt/public/heimdall.md`, "Bugs in Heimdall") offers a GitHub report instead (`docs/editions.md`).
The repo is `TobyNoSkillSon/Heimdall`.

Beta: the public bug-report path has not been run against the live repo yet.

Heimdall files bug notes itself with its `bug_note` MCP tool; nothing the user says starts one. The user tells
Heimdall something is broken, or Heimdall notices a failure or waste on its own (a tool returning a wall of JSON, a
slow step), and it writes what happened, when and what was expected. The fixing round handles the notes and clears
them from the inbox.

## The tool

`bug_note` is served only by Heimdall's own MCP server, so the main Heimdall agent has it and T3 workers do not. The
one exception is an emergency cover agent, whose scoped token reaches it as `<cli> bug` (`docs/emergency.md`).

| Field | Meaning |
|---|---|
| `text` (1–8000 characters, required) | Plain English: what happened, when, what was expected, the user's words if they described it. |
| `evidence` (1–32,000 characters, optional) | Anything Heimdall already has: an error line, an excerpt of the bad output. |

The result is one line, `Bug note saved; N open.` A replayed call with the same operation ID files nothing new (action
receipt `bug_note`); with the Agent off nothing is written. The `bug-notes` skill (`skills/heimdall/bug-notes/SKILL.md`, public edition
`skills/public/bug-notes/`) tells Heimdall when to file and to stay quiet about it.

## The inbox

One inbox: `<data-dir>/bugs/inbox/` (inside `<home>/.heimdall`, `home.md`); in the personal app the code checkout's
inbox (where the fixing round works) when the hidden `bug_inbox` setting names it, else `<home>/Projects/Heimdall/bugs/inbox` when that checkout's
`BRIEF.md` exists (never in replays, lab runs or tests); `HEIMDALL_BUG_INBOX` overrides both. `heimdall-night bugs` reads the same: the variable, the setting, then the
data folder. `heimdall_night::bug_notes` owns the format:

- `<id>.md`, what a person reads (heading with the local time, the text, an **Evidence** block), then `<id>.json`, the
  record (`schema: 3`, `kind: bug_note`, `id`, `created_unix_ms`, `created_local`, `filed_by: heimdall`,
  `note.text`, optional `evidence`). A `.json` always means a complete note.
- `<id>` is the local time it was filed (`2026-10-19T12-32-53`), then `-2`, `-3` within one second. Concurrent writers
  claim a name with a hard link, so two notes never share one.
- Files 0600, folders 0700: notes can quote the user. `bugs/inbox/`, `bugs/resolved/` and `bugs/command-reviews/` are
  git-ignored.

Older spoken notes (`schema: 2`, same `kind` and `note.text`) are listed and resolved the same way.

## Cause classes

Every note gets a `cause`, including a flag for when the decider is at fault. The record field and the Markdown section `## Cause` carry:

| Field | Meaning |
|---|---|
| `class` | One of `code`, `decider`, `agent-prompt`, `model-quality`, `environment`, `unknown`. |
| `reasoning` | One to three sentences that name the evidence. A dot inside a path or a number does not end one. |
| `confidence` | 0 to 1. |
| `evidence` | Up to 8 lines quoted from the log, the transcript or the note. |
| `suspected_files`, `suspected_functions` | Repo paths and function names, checked against the repo (a name it does not hold is dropped). |
| `repro_hint` | One line on how to reproduce or check it. |
| `decider_case` | Decider notes only: `label` (`send`, `wait` or `not_for_heimdall`), `variant` (`plain`, `ack`, or `unknown` when the log does not say) and the archive transcript `window` around the note. |
| `by`, `model`, `at_unix_ms`, `cost_usd` | Who wrote it: `scout`, `triage` or `coordinator`. |

- `code`: a defect in Heimdall's own code. `agent-prompt`: the main agent chose wrongly and its instructions could change.
- `decider`: the wake, send or barge-in judgement by the decider model was wrong (it woke Heimdall when it should not, or
  missed it). This is not a code fault unless the code path that feeds the decider is wrong.
- `model-quality`: TTS, ASR or another model's output is poor with no code defect.
- `environment`: OS, hardware, a device, a permission or an outside account.
- `unknown`: the evidence does not settle it; the reasoning says what would.

`heimdall_night::bug_notes::check_cause` refuses a class, label or confidence outside these, and a reasoning longer than
three sentences. The scout trims its own reasoning to three sentences instead of refusing it.

## The bug scout

Filing a note starts one scout for it (`crates/night/src/bug_scout.rs`; the app calls `bug_scout::enqueue` from
`bugs::Filing::file`, in the installed app only, never in a replay or a lab run). The scout is a headless
`claude -p --model claude-haiku-5-5` call that reads the repo with `Read`, `Grep` and `Glob` only (no Bash, Write or Edit),
run from the repo folder. It starts from a prepared context (the note and its evidence, Heimdall's log lines about
2.5 minutes either side, the archive's utterances in the 3 minutes around the note, and repo lines that share words with
the note) and follows the code from there. It writes its cause into the note with `set_cause`, and that is all it does:
it never edits code and never delivers to a thread, so it never wakes the main agent.

Each suspected site is `file`, `function` and `lines`. A site is kept only if the file is under the repo and the named
function's `fn` line falls within the cited lines (three lines of slack); the kept site records `at_line`. Sites the repo
does not hold are dropped, so a note's sites are real.

Limits: one scout per note, and a note scouted by an older scout (`scout_version`) is scouted again; one at a time,
through a queue of 8 notes (a full queue leaves the note for the fixing round); 20 tool calls; 4,096 output tokens per reply;
$0.50 a call as a safety ceiling; 180 seconds. `HEIMDALL_BUG_SCOUT=off` turns it off. A failed scout writes nothing and
logs one line to `heimdall.log`; the note waits for the fixing round.

The fixing round and the coordinator use the same binary:

```
cargo run -p heimdall-night -- bugs scout <id>        # one note now (prints the cause)
cargo run -p heimdall-night -- bugs scout --open      # every open note without a cause, oldest first
cargo run -p heimdall-night -- bugs set-cause <id> '<json>'   # by hand: class, reasoning, confidence, "by":"coordinator"
cargo run -p heimdall-night -- bugs export-decider-cases <out.jsonl>
```

`HEIMDALL_BUG_INBOX` points the commands at another inbox (a copy, for a test), `HEIMDALL_DATA` at another Heimdall data
folder, `HEIMDALL_CODE_ROOT` at another repo, `HEIMDALL_CLAUDE_BIN` at another `claude`.

## Routing by class

| Class | Route |
|---|---|
| `code`, `agent-prompt` | The fixing round fixes it. |
| `decider` | Not a code fix. Becomes a labelled decider case: `bugs export-decider-cases` writes one row per case (the next decider build reads them; see below). The note is then resolved as exported. |
| `model-quality` | Listed in the morning report's model shortlist with its evidence. The user picks models; the shortlist's candidates are checked against current sources first. |
| `environment` | Listed for the user in the report; no code change unless the cause is in Heimdall's setup. |
| `unknown` | The fixing round may try it if a suspected file exists; otherwise it stays open for the coordinator, who says what would settle it. |

Decider rows (`export-decider-cases`) are one JSON object per line, shaped like `lab/decider-v4` rows: `id` `bug:<note>`,
`task` `dispatch`, `variant`, `label`, `split` `bug_cases`, `origin` `bug-note`, `state` (the note and its window as a JSON
string) and `state_exact` `false`. The state is not the decider's own prompt bytes: the next decider build must refit it
from the journal (`lab/decider-v4/scripts/build_points.py`, `fmt.fit`) before training on or evaluating a row. Until then, use the rows
as labelled cases for review, not as exact training input.

The old recorder's crash checkpoints lived in `<data-dir>/bugs/pending/` (App Support for the installed app). That
folder was never an inbox; startup removes it when it is empty and logs a line if anything is left in it.

## The menu

"Bug Notes (N)" counts the inbox; plain "Bug Notes" when it is empty. The row opens the Bug Notes window: each note's
time and text, newest first, selectable. The count is read when the app starts, after each `bug_note` call, and
every time the menu opens, so notes the fixing round cleared disappear on the next open.

## The fixing round

The round that fixes Heimdall reads the open notes, fixes what it can, and resolves each note it handled.
Resolving moves `<id>.md` and `<id>.json` to `bugs/resolved/` with a `## Resolved <time>` section (and a `resolution`
field in the record) saying what was done; notes it did not handle stay in the inbox. When every note was handled
the inbox is empty. Agents run it with the `heimdall-night` binary:

```
cargo run -p heimdall-night -- bugs list
cargo run -p heimdall-night -- bugs resolve <id> <what was done, with the commit>
cargo run -p heimdall-night -- bugs check <base>..HEAD
cargo run -p heimdall-night -- bugs reconcile main
cargo run -p heimdall-night -- bugs scout --open
```

`check` lists each open note whose ID a commit in the range names (a fix that names its note must resolve it) and exits
1 when there is one. The fixing round runs it after its last merge.

## Closing notes

Whoever works on the bugs from that folder closes the tickets when done. Any agent that fixes a note in `bugs/inbox/`, or confirms it is already
fixed, not reproducible or a duplicate, resolves it with `bugs resolve` before its final reply, not only the fixing
round. The resolution says what was done and cites the commit (and the branch, if it is not on `main` yet); a fix
commit names the note's ID. A note that only needs a decider training run stays open, tagged `## Shelved: needs
Decider v5` (or the next decider version); a request is built or resolved with the reason it was not.

`bugs reconcile [REV]` (default `main`, run from a checkout) resolves every open note that a commit on `REV` from the
last 60 days names by ID, citing the newest such commit. The fixing round runs it first, so a note whose fix already landed never waits for a person.

`resolve` refuses a blank resolution or an unknown ID and never overwrites an earlier resolved note with the same
name (`<id>-r2`). Notes written by hand as a lone `<id>.md` are resolved the same way.

## Automatic command reviews

Unsure or failed spoken-command decisions write `kind: command_decision_auto` records (with the complete
`command_decision` payload) to `bugs/command-reviews/` beside the inbox. They are training examples, not bug notes:
they never enter the inbox or the menu count.

## Verification

`heimdall-night` `bug_notes::tests`: unicode and long text kept whole, evidence that cannot close its own code block,
24 concurrent writers in one second, damaged and foreign files skipped, the fixing round clearing handled notes and
keeping the rest, hand-written notes, and name collisions in `resolved`. App `bugs::tests`: the tool files once per
operation, replay and Agent off, concurrent calls, the menu count following the store, reviews kept apart, and the
old checkpoint folder. Pipeline `bug_notes` tests: every former trigger phrase ("bug note", "bugnote", "bug notes",
"But note,", "Heimdall bug", "zgłoś błąd") on the partial, window and final paths starts nothing, and the menu count
follows the inbox when the menu opens. Agent tests pin the tool's schema and the filing rules (read from the prompt and the edition's skills together).
