# One system: Goals, Scheduled, Settled, proposals, job islands, viewer reports

The design: unanswered proposals are archived after two days and do not come back, and the system starts from a clean slate.

Rules that hold in code:
- Nothing enters Goals or Scheduled without the user's yes, by voice (`plan_edit accept`, or `add` quoting their own words)
  or one click (the Yes on a proposal). Everything else Heimdall, the Goals job or the plan refresher wants there is a
  **proposal**.
- Nothing settles without proof: a commit, a test run, an output file, an install, or the user's own quote. A finished
  piece with none of these is **claimed**: drawn dashed, never counted.
- Running comes from open T3 turns (thread-bound job islands, `islands::binding`), their stage from tool calls
  (`islands::stages`). A goal's task is lit blue only while a running job island names that goal.
- Not sure is said: a running job island not heard from for 15 min dims and says so; the guarantee line says "Not
  sure" when Heimdall's sweep is older than 30 min or a source is silent.
- Only real blocks wait on the user (an open ask, a job island waiting on them or failed, a missed promise). Proposals never do.
- At most one proposal per list shows; the rest queue behind it as "+N".
- A proposal nobody answers is archived 48 h after the user first saw it: out of the list, into the record, no badge, no
  count. It does not come back.
- Nothing animates at rest.

## Files (the islands folder, `islands/<list>.json`)

Every list keeps the existing plan-island keys (`id`, `preset: "todo"`, `list`, `title`, `icon`, `side`, `order`,
`hidden`, `expanded`, unknown keys kept) and gains:

- `slate`: epoch seconds of the clean slate (written once, on all three lists).
- `proposals`: the queue, oldest first. The **front** is the first entry not past 48 h since its `shown`; it is the only
  one drawn. "+N" counts the entries after it that are not past 48 h.

A proposal: `{id, title, why, by, proposed, shown}` plus, per list:
- Goals: `kind: "goal"` (a new goal: `icon`, `about`) or `kind: "step"` (a next step under `goal`, a goal id).
- Scheduled: `due` (epoch seconds, required), optional `goal`, `icon`.
`why` is the words behind it (the user's line or the finished job it came from), shown when its title is clicked. `by` is
who drafted it (`heimdall`, `goals`, `refresher`, `slate`). `proposed` is when it was drafted; `front` when it became
the one on screen (Heimdall hears of it then); `shown` when the islands app first drew it to the user, its list open and on
screen (the app's `seen` response). Only `shown` starts the two days: a list left closed never loses a proposal the user
did not see.

A yes keeps the proposal's `id`: the ghost fills in where it already sits. Accepted entries carry
`yes: {by: "voice" | "click", at, quote?}`.

### goals.json

- `goals`: as before (`docs/goals.md`): `{id, title, icon, about, state, now, moved, tasks: [{id, title, state:
  done|now|next, at}]}`.
- `items`: Goals' asks. No longer drawn in Goals: an open ask is a real block and shows on Scheduled's guarantee line.
- Drawn per active goal: a progress ring round its disc (green share = done tasks, blue share = running islands, over
  done + running + queued + 1 for a front step proposal of this goal), its title, a segmented bar (one segment per
  done task, green; one per running island of this goal, blue; one per queued task, dim; one dashed segment for its
  front step proposal), the latest move (`now`) in grey, "N running on the right" in blue when job islands of this goal
  run, the queued tasks (`next`, and `now` with no running island behind it) as grey-dot lines (at most 3), and the
  front step proposal as a ghost row ("Prepare release notes  [Yes] +2"). A front goal proposal is a ghost goal after the
  goals. Finished tasks are not listed (they are Settled's).

### scheduled.json

- `items`: promises, each with `due` (no time, no promise: an item without `due` is not drawn). `goal` and `icon`
  optional.
- `guarantee`, written by Heimdall's sweep: `{checked, waiting: [{id, title, line, goal, choices: [{label, action}],
  source}], unsure: [{id, title, line, since}]}`. An unsure entry with `since` (epoch s) reads "<title> not heard
  from in N min", N from the app's clock; without it, its `line`. A waiting entry's `line` leaves out the goal (the app
  draws the goal's symbol and title before it: "Ship the beta · blocks 1.0").
- Drawn: the guarantee line on top (amber: the first waiting item with its choices and "+N"; not sure; or "Nothing
  waiting on you · checked 2 min ago"); `checked` older than 30 min, or no `guarantee`, reads "Not sure" ("Not checked
  for 40 min"). Then the week strip (now → 7 days: a white mark per promise, a dashed ring for the front proposal),
  then the rows by time ("Tue 10:00", the goal's symbol, the title), the front proposal as a ghost row in its time's
  place with Yes. Nothing promised and nothing proposed: "Nothing promised".
- A click on a waiting item's choice writes `{island: "scheduled", kind: "waiting", action, value: <waiting id>}`.

### settled.json

- `items`: `{id, title, at, goal, task, kind: shipped|decided, quote, by, proof?}`; `proof: {kind: commit|test|file|
  install|quote, value, label}`. No `proof`: claimed.
- Drawn: the count of proved items since `slate` (when the slate is under 7 days old: "since the clean slate"; else
  the week's, "this week"), "N claimed" with a dashed pill when there are claims, the 7 day columns (a green pill per
  proved item, a dashed one per claim; days before the slate a dim baseline; today lit), then the chosen day's newest 3
  rows (time, the goal's symbol, title, the proof chip: "commit", "test", "file", "install", "quote"; claims a dashed
  "claimed" chip), "› N earlier today". The chip opens the proof in place (a file also opens).

### Job islands (right)

New keys: `sent` (what it was sent to do, one line, written once and never replaced: the binding takes it from the
task title; an island still without one gets it once from the island agent, asked for it in that call's message only
then and held to one imperative line of at most 90 characters (two lines on the island), no path, status or invented number), `goal` (a goal id: a chip with
the goal's symbol and title). Drawn: title, `sent`, the goal chip, the stage band (kept), the fact line ("open turn, 3
helpers" while running, the elapsed time on the right). Done: the band ends in a green check; an `open` report shows
as "This report" (opens in the viewer). Running and `updated` older than 15 min: dimmed, "not heard from in 22 min".

## Responses

- `{island: "goals" | "scheduled", kind: "seen", value: <proposal id>}`: the app drew this proposal to the user (open, on
  screen, islands not hidden), once per proposal per app run. Heimdall stamps `shown`; nobody is told.

- `{island: "goals" | "scheduled", kind: "approval", action: "yes", value: <proposal id>}`: the proposal is accepted (a
  click).
- `{island: "scheduled", kind: "waiting", action, value: <waiting id>}`: a choice on the guarantee line. Heimdall
  applies it to its source (an ask's proposal action, a job island's button) and tells the agent.
- `{island: <viewer id>, kind: "report", action, value, label}`: a report page in the viewer called
  `window.heimdall.send(action, value, label)` (or `window.webkit.messageHandlers.heimdall.postMessage({action, value,
  label})`). `action: "save"` with `value: {file, data}` also writes `data` as JSON to `file` (a bare `*.json` name)
  beside the page, so a comparison page's picks persist from the viewer.

## The clean slate (first start of this build)

Once, when `goals.json` carries no `slate`: the three lists, their kept and closed copies and `.todo-migrated.json`
are copied to `<islands root>/record/clean-slate-<YYYYMMDD-HHMMSS>/`; then Goals, Scheduled and Settled are written
empty with `slate`, Settled keeping its `removed` memory. Nothing is deleted. Goals gets the old active goals back as
proposals (`by: "slate"`, one shown at a time); the old Scheduled and Settled items stay in the record only. A truly
first run (no lists at all) starts empty.

## The record

`<islands root>/record/`: `clean-slate-*/` folders and `proposals.jsonl` (one line per archived or declined proposal:
`{list, proposal, outcome: "archived" | "declined", at}`). Declined and archived proposals are never proposed again:
a new proposal whose title matches one in the record (same words, `goals::settled::same`) is refused. So is one that
matches what the user removed from that list (`plan_edit` remove on Goals, Scheduled or Settled writes the list's
`removed`; matched in the same or other words, with no two-week end), and every list keeps its `removed` through the
clean slate.
