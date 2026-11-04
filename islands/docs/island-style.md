# Island text

For the agent that puts islands on the user's screen. `islands put` refuses what it can check (marked ✓).

**Title**: 1–3 words naming the thing: "Qwen-Image", "iPad sale", "Male voices". Never an id or a file name. ✓

**State**: the colour the user reads first.
- `running`: working in the background; nothing needed from the user (white).
- `waiting`: needs the user: an answer, a test, a look (orange).
- `done`: finished; give it a `--value` or `--open` when there is a result to see (green).
- `failed`: stopped; the summary says why and what next (red).

**Icon**: a tiny drawing of exactly what the island is about, made for it: an inline SVG (viewBox 0 0 24 24, a few bold plain shapes: path, circle, ellipse, rect, line, polyline, polygon, g; no text, styles, links or images; under 2 KB), or an SF Symbol name when one truly fits ("ipad"). It is drawn at 19 pt as a silhouette in the state's colour, so it must read at that size. Only Goals, Scheduled and Settled keep fixed icons (Settled's is green). ✓

**Colour**: stick to the default, with accent colours only when something is more important. The system's neutrals for almost everything: label white (text, the main line, the stage in hand), secondary grey (units, labels, a second series), tertiary grey and separators (tracks, hairlines, what is behind). Four accents, each for one meaning and at most one per body: the Goals green `#30D158` (done, passing, settled), Heimdall's blue `#0A84FF` (running in a goal, the one thing to watch), orange `#FF9F0A` (waiting on the user, a real block) and red `#FF453A` (a failure). No purple, pink, light blue, teal or indigo. Two series (two judges, baseline and candidate) are told apart by weight and line style (solid white against dashed grey) and by their names at their line ends, never by two hues; the leading or most important one may take the one accent. The band is neutral: the stage in hand white, the stages behind it dim grey, orange only while waiting, red on failure, the green check when done.

**Motion**: running shows a tiny sign of work every few seconds (the arc turns once, or the icon takes one breath); waiting is orange; done is the green check, and only done. Nothing moves in between. A thread-bound island follows its thread live, also through new runs after it finished.

**Stage**: one or two present-tense words for the activity the user cares about: "Downloading", "Final tests", "Your answer". Not the mechanism ("Rendering"). ✓

**Summary**: one plain line: what it is and what happens next. "20 GB of weights, then convert and install". ✓ (one line, 110 characters at most)

**Lists**: `--step` for a sequence, with the current step `=now`; `--item` for a plain list.

**Plan**: three islands docked left: Goals, Scheduled, Settled (a pill until opened). There is no to-do list. Keep each item a short, concrete line that names the project first ("Website: push branch"); no vague items ("decide the end version"). The list shows only that line and a small symbol picked from the project name, so the title must carry the item alone; the `detail` (why, what next, any length) shows only when the user opens the row.
- Goals: a few big aims a week or a month ahead, each with its tasks (finished ones collapse under it); a background job keeps them. Their asks are what needs the user now (a decision, an approval, a test on their device): each is drawn under the goal it serves, one that serves none as a card; give it a `proposal` when there is a decision (what exactly is proposed, with the details; the user answers Yes / No / Later from the list or by voice). An ask leaves once answered.
- Scheduled: agreed for later or tonight, and the user's obligations with a time, with `due`, the absolute time it should happen (`plan_edit` takes a local date-time, "2026-10-24T10:00"; tonight is 23:00). The island counts down by itself; an item that falls due stays in Scheduled, drawn as due now. Never a vague `when` ("Tomorrow", "Sat") when the date is known.
- Settled: done or agreed.

Give every item a stable `id` (the CLI makes one from the text). Add an ask with `islands plan goals add "Website: pick the icon" --goal g-website`; schedule with `islands plan scheduled add "Heimdall: menu revamp" --due "2026-10-23 23:00"`; move with `islands plan scheduled move menu --to settled`; tick off with `islands plan scheduled done 3`. The user ticks items with a click; ticked items sink to the bottom until `islands plan scheduled prune`.

**Attention**: set `attention` to `waiting` (needs the user) or `question` (a question to answer) on an island that needs them; the app pulses it every 6 s while it is closed and stops when they open it. Remove the key when it no longer needs them. Plan islands get it by themselves from an open ask.

**Place**: running work docks right (default), the plan lists left, and `--side top` puts an island in the row under the notch. Leave places the user chose alone.

**Show, don't tell**: text is a poor way to carry news. Prefer what can be seen: a timeline of the current phase's steps for a sequence, a progress bar for a count, bars for a comparison, a line for a trend, one big number for a single measure, the latest picture for visual work, the icon for what it is. One glance answers what this is, whether it works or needs the user, and what comes next. These are guidelines, not templates: every element earns its place, and nothing repeats what another element already shows.

**Running work lives on the right**: what is actually running never sits in a plan list. It is a job island on the right, one per thread, with that thread's items as its steps (Voice lane: ack, benchmark, spotter, echo cancel). The left holds only Goals (with what waits on the user), Scheduled and Settled.
