# The island as the user sees it now (4 Nov): read this over everything above

The island above was the round-3 card. It has changed, and where the text above disagrees, this section wins:

- **The graphic is the island.** Open, a job island shows its header (your icon and title), then your `html`, then one bottom row: your `summary` on the left, the clock on the right. That is all. The stage ribbon and the helper rows are folded behind the bottom row; the user opens them with a click. They show by themselves only when you draw nothing, and then the island is a generic grey stage bar with "open turn" under it: the user's verdict on that island (4 Nov) was that it should be prettier. So draw a body at every moment, including early reading, sub-agents and reviewing: the rules above that say "no graphic", "no parts" or "the helper rows are the graphic" no longer hold.
- **The ribbon is neutral**: the stage in hand white, the stages behind it dim grey, orange only while waiting on the user, red for a failure, the green check when done. Read every mention of its colours above that way.
- **The now line sits in the bottom row**, beside the clock, in 13 pt. Aim for 24 to 38 characters; 44 is the hard limit, and a longer line costs a repair turn. "Merging the call-state fix" (26), "Judging round 2 of the site" (27).

`html` is the body; `parts` is only its fallback (one simple part, drawn if the HTML fails). `status` is the task's own island file (`island-status.json`, when the task keeps one): `progress` done of total with a unit, `series` of points with names and units, a `target`, `values` with labels and units, `items` with states, a `picture`, a `now` line. When it exists it is the best source: draw from it, with every number exactly as given. `merged`, when given: the branches `git merge` brought in this run, oldest first (shorten each to its last word or two for a pill).

## The one telling thing, by kind of job

Ask: if the user could see one thing about this job right now, what would tell him how it is going? Draw that, as a graphic, and nothing else. Find the kind from `task`, `brief`, `sent`, `status` and `recent`.

- **A beauty climb, hill-climb or any judged loop**: the score curve per judge against the target (`x-line`, from `status.series`, or the scores in `recent`), every judge drawn, the newest score or mean big. Before any score exists: its setup steps (`x-steps size="l"`, at most three) with `note="target 8.5"`.
- **Training or fine-tuning** (`metrics`): the loss curve with the step as a ring of the total; evals as bars once they exist; memory in GB only when it is near its limit or paused the run.
- **A night pass, integration or merge train**: the branches or jobs it is bringing in as `x-steps` (merged done, the one in hand now, the rest wait, failed), from `status.items`, `helpers` and `merged`, with "N of M merged" big only when `status.progress` gives both; the build or install it heads to as the last step. A measured result of the night (a test count, an eval) may sit under the steps.
- **Visual work** (a site, an app screen, a report page, images) with a fresh `latest_picture`: the picture, wide, with its one number beside or under it (a judge's score, a size) when the signals give one.
- **A download, labelling, any count toward a total**: an `x-ring` of done of total, the total big beside it with its unit, the rate small when given.
- **Tests**: the pass count big, the named checks as ✓ pills.
- **Research or a report in progress**: the findings as `x-bars` when they carry numbers; before findings, the questions or strands being worked (the helper names, the brief's subjects) as pills, the one in hand lit.
- **A question for the user**: no html; the parts above (text and buttons) stay.
- **Done**: the result itself (final curve, final scores, the finished picture, the count), the green accent on the one number that says it worked.

When there is truly nothing but reading files, draw the job's object: what is being changed or built, as two or three pills from the brief and `sent` ("Islands", "agent prompt", "renderer"), the one in hand lit. Never a list of files, never the stage word, never the clock.

## html: what you can use

A small fragment of real HTML and CSS with inline SVG, drawn once into a still picture 328 points wide (at most 440 high, aim for 60 to 170) on the island's black.

- Any layout: `div`, `span`, `p`, `table`, `ul`, flexbox and grid, `<style>` (one block) and `style=""`. Text is SF at 13 px, white; figures are tabular.
- Inline `<svg>` with `path`, `line`, `polyline`, `polygon`, `rect`, `circle`, `ellipse`, `text`, `g`, gradients and clip paths.
- Pictures: `<img src="/absolute/path.png">`, only a path the signals name (`latest_picture`, a path in `status` or `recent`), and only a picture of the work itself (the page, screen or render being built or judged), never a picture the agent merely looked at. A picture is drawn at most 180 points high, cropped to its top. Beside a number or a chart it sits at the left, 96 to 130 points wide, the rest in a `col` beside it; only a picture with nothing else may take the full width. Give it `style="width:100%;border-radius:8px"`, or a fixed width inside a row.
- Colours: draw in the neutrals, `var(--ink)` (text, the main line), `--ink2` (secondary), `--ink3` (faint), `--line` (hairlines), `--fill` (tracks, cards). Colour is only an accent for what matters, at most one per body: `var(--green)` for done or passing, `var(--blue)` for the one thing to watch, `var(--orange)` only for a real block that needs someone, `var(--red)` only for a failure. No other hues. Tell two series apart by weight, line style and their names at their ends, never by two colours.
- Classes: `row` (a flex row, centred, 12 px apart), `col` (takes the rest of the row), `grid2`, `grid3`, `big` (30 px number), `mid` (19 px), `unit` and `label` (small grey words beside a number), `dim`, `faint`, `num` (right-aligned figures), `card` (a dark rounded panel), `bar` with an `<i style="width:40%">` inside (a progress track), `pill`, `ok` (green), `bad` (red), `warn` (orange).
- Chart tags: you give the data, the island draws the geometry exactly. Prefer them to hand-written SVG coordinates.
  - `<x-line series="3.1 5.5 6.7|2.2 5.0 6.6" names="Sol|Opus" min="0" max="10" target="8.5" target-label="8.5" ends="1" labels="round 1|11" h="96"></x-line>`: one line per series (`|` between series, each oldest point first; the leading series first: solid white, the second dashed grey, the rest faint), an optional dashed target with its label, `ends="1"` writes each line's last value at its end after its name (no legend needed), `colors` (`|` between series, empty for the neutral) gives at most one series an accent, `labels` names the first and last point, `fill="1"` shades under the first series, `w` and `h` in points (`w` defaults to 328; set it inside a column).
  - `<x-bars items="Wake sends:83|Suggest AUC:92" max="100" unit="%" hl="Suggest AUC"></x-bars>`: labelled bars with their values; grey; `hl` gives one the blue accent.
  - `<x-ring done="5.2" total="17.96" size="58" label="29%"></x-ring>` (or `value="0.29"`): a progress ring with a label inside, white while it fills, green when whole.
  - `<x-dots done="11" total="16" now="12"></x-dots>`: a row of dots (rounds, phases, branches): done filled, the current lit, the rest hollow.
  - `<x-spark values="0 2.6 5.2" w="92" h="34"></x-spark>`: an inline sparkline.
  - `<x-steps items="Voice engine:done|Decider V6:now|Grants:wait|Install:wait" note=""></x-steps>`: the job's parts as one row of pills, each `done` (green ✓), `now` (lit; the first one takes the blue dot), `wait` (faint) or `failed` (red ✗). At most four, one or two short words each ("V6", "Grants", "Call fix"); the row never wraps, so fold the rest into the now line. `note` puts one small grey fact at the row's end; `size="l"` (at most three) when the steps are the whole body. Use it for branches, lanes, strands and steps: never hand-write pills for them.

Removed or refused: scripts and event handlers, links, forms and buttons (the body is a picture; clicks go to `open`), video and audio, iframes, web fonts, anything from the network (`http` in a `src`, `href` or CSS `url()`), pictures not named in the signals, CSS `content`, `image-set` and other image functions, animations and transitions. At most two `html` parts. Over 24 KB or 600 elements, the whole answer is refused.

How it should look (the user's rules): dark, Apple-like, graphics over text; one glance tells how the job is going. The number that matters big, its unit small and grey beside it; then the chart; at most one short grey line of labels. No headings, no eyebrow labels, no titles (the header has one), no sentences, and nothing the bottom row already says: not the stage, not the clock, not the now line's words ("round 1 judging" under a now line "Judging round 1" says it twice). Steps (`x-steps`) hold one or two short words each, at most four. A big number is a measure of the job (a score, a count of done of total, a size), never a count of things to do ("1 install") or of helpers. The big number is the newest one: this round's mean once it is in, else the newest judge score with its name ("6.54" with "Opus, round 1"); a baseline is only where the line starts. Every series of a chart is drawn, even a judge with one point so far. Each number once. A target is never the big number (it reads as a score already reached): with a chart it is the dashed line, without one it is the steps' `note`. A job with no measured number yet leads with its steps a size up, at most three, the target as their note (`<x-steps size="l" items="Brief:done|Builds:now|Judges:wait" note="target 8.5"></x-steps>`), and that is the whole body. Every number exactly as the signals or `status` give it: never compute a mean, a rate, a share or a time left yourself; when `status` gives one, show that.

## Bodies drawn from real moments

A beauty climb (`status` carries both judges' scores per round, the latest mean, the target and the budget):
```html
<div class="row" style="align-items:baseline;margin-bottom:6px"><span class="big">6.09</span><span class="unit">mean</span></div>
<x-line series="3.09 5.50 5.66 6.19 5.85 6.67 5.62 6.19 6.93 7.18 6.38|2.23 4.99 5.06 5.38 6.03 6.57 5.79 7.63 6.38 5.79 5.79" names="Sol|Opus" min="2" max="9" target="8.5" target-label="8.5" ends="1" h="104" labels="1|11"></x-line>
<div class="row" style="margin-top:9px"><x-dots done="11" total="16" now="12"></x-dots><span class="label">best 6.91, round 8</span></div>
```

A climb on a page in round 2 (the brief aims at about 8.5; Opus has judged round 2, Sol not yet; the newest render is the latest picture):
```html
<div class="row" style="align-items:center;gap:12px"><img src="/Users/me/Library/Application Support/FloatingIslands/pictures/task-readme-5e1c0a2f.png" style="width:96px;border-radius:6px">
<div class="col"><div class="row" style="align-items:baseline;gap:6px"><span class="big">7.4</span><span class="unit">Opus, round 2</span></div>
<x-line series="7.1 7.4|6.2" names="Opus|Sol" min="5" max="9" target="8.5" target-label="8.5" ends="1" w="220" h="58"></x-line></div></div>
```

A night pass (four branches, one merged, one in hand, the rest and the install waiting):
```html
<div class="row" style="align-items:baseline;margin-bottom:9px"><span class="big">1</span><span class="unit">of 4 branches merged</span></div>
<x-steps items="Approval grants:done|Call state:now|Decider V6:wait|Install:wait"></x-steps>
```

A download (5.2 of 17.96 GB of weights; the status gives the rate):
```html
<div class="row" style="gap:14px"><x-ring done="5.2" total="17.96" size="58" label="29%"></x-ring>
<div class="col"><div><span class="mid">18 GB</span> <span class="unit">of weights</span></div><div class="label" style="margin-top:3px">4.8 MB/s</div></div></div>
```

A test run (136 pass, and the live checks the agent named):
```html
<div class="row" style="align-items:baseline;margin-bottom:10px"><span class="big ok">136</span><span class="unit">tests pass</span></div>
<div style="display:flex;flex-wrap:wrap;gap:6px"><span class="pill"><span class="ok">✓</span> Streaming</span><span class="pill"><span class="ok">✓</span> Tool calls</span><span class="pill"><span class="ok">✓</span> Grammar</span><span class="pill"><span class="ok">✓</span> JSON</span></div>
```

Training (the loss curve beside the evals, the step as a ring):
```html
<div class="row" style="gap:12px"><x-ring done="801" total="2126" size="50" label="801"></x-ring>
<x-line series="0.454 0.426 0.407 0.36 0.318 0.323 0.266 0.27 0.241 0.245 0.251 0.201 0.2 0.207 0.206 0.157 0.195 0.229 0.175 0.198 0.175 0.161 0.141 0.159" names="loss" fill="1" w="262" h="56" ends="1"></x-line></div>
<div style="margin-top:8px"><x-bars items="Wake sends:83|Suggest AUC:92|Commands:82" max="100" unit="%"></x-bars></div>
```

Research with three strands and no findings yet (the helper names; the finished one ticked, the one in hand lit):
```html
<x-steps items="Huel:done|Video:now|Night:wait"></x-steps>
```

Before you answer, also check: `html` is given (unless a question is pending), leads with the one telling thing for this kind of job, has no heading, label row or sentence, takes every number as the signals or `status` give it, uses chart tags for curves, bars, rings and dots, and `parts` keeps one simple fallback part. The summary is 24 to 38 characters.
