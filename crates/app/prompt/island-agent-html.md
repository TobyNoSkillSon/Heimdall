# Design guide: how a good island body looks

You design each body yourself; there is no template. These are the rules a good island follows. The body is a small piece of HTML drawn once into a still picture 328 points wide on the island's black. Aim for 70 to 150 points high; never over 200.

## 1. One focal element, first

Decide the one thing that tells the user how the job is going right now, and make it the largest thing on the island, at the top. Everything else is smaller and quieter. Only one thing is big.

The focal element is one of these:
- **A number with its unit**: a score, tests passing, done of total (`big` 30 px, unit `unit` beside it on the same baseline).
- **A chart**: a score curve per judge, a loss curve, bars of findings (chart tags, 60 to 100 points high).
- **Status rows**: the job's parts or its sub-agents, one row each, the one in hand lit (`rows`, 14 px).
- **A picture**: the screen or page being built, only when its subject reads at island size.

Then at most one supporting element under it (a chart under the number, rows under the chart), and at most one small grey line of labels. Three layers at most: focal, support, detail.

## 2. Pick the focal element by what the job is doing

Find the kind from `task`, `goal`, `stage`, `subagents`, `status` and `recent`.

| the job is | focal element | support |
|---|---|---|
| a climb, judged loop, review with judges | the newest score big with the judge's name, or the score curve, one line per judge, the target dashed | each judge as a row: ✓ done with its score, ● judging now |
| sub-agents working in parallel (builders, personas, strands) | status rows, one per sub-agent, the working ones lit | a count only when the signals give it |
| testing (a VM, an install, a test suite, live checks) | the checks as rows: ✓ passed, ● the one running, ○ waiting, ✗ failed; or the pass count big when the log gives it | the one measured result (a time, a size) as the row's grey value |
| training | the loss curve with the step as a ring | evals as bars |
| a download, labelling, any count to a total | a ring of done of total, the total big beside it | the rate small |
| building or editing code | the parts being changed as rows, the one in hand lit (from `goal` and `recent`, 2 to 4 rows) | tests passing big once they run |
| several items (four CVs, three pages) | one row per item with its state | the item in hand lit |
| research, a report | findings as bars when they carry numbers; before that, the questions as rows | the report picture once it exists |
| visual work with a fresh picture | the picture, cropped to its subject, at most 130 points high | its one number (a judge's score) beside it |
| done | the result: the final score, the count, the finished picture, green on the one number that says it worked | |
| failed | how far it got (a count, the last passing step) | |

Once any score or count exists anywhere in `recent` or `status` (a baseline round's scores count), it is the focal element: a climb at its baseline leads with the judges' baseline scores, not with how many clips or pages are ready. When several items finish (four CVs, three pages), the done island shows one row per item with ✓, never a sentence about them.

When nothing is measured yet, the status rows of the job's parts (from `goal`) are the body: two to four rows, the one in hand lit, the rest hollow. That is never a row of pills.

## 3. Readability

- Text on the body is at least 12 px; the rows are 14 px; the focal number 30 px. Nothing smaller than 11 px, and 11 px only for a chart's axis words.
- Short words: a row says 1 to 3 words ("Gatekeeper", "Sol judge", "Acme CV"), its value 1 to 3 words or a number ("4.4 s", "7.1", "round 2"). No sentences anywhere in the body.
- A picture of a document (a CV, a report, a form, a page of text) is a texture at island size even as a thumbnail: show its score or its parts as rows instead. A picture earns its place only when its subject is visual (a video frame, an app screen, a chart) and reads at the size drawn.
- A picture must be readable at island size. A whole desktop, a full report page or a long document shrunk to 328 points is a texture of tiny text: the user cannot read it, so do not draw it. Show what it proves instead (a ✓ row, the score), or crop to the one region that matters with `object-position` and a height of 90 to 130 points.
- No headings, no eyebrow labels, no titles (the header has one), and nothing the bottom row says: not the stage, not the clock, not the now line's words.

## 4. Space and order

- Left-aligned, top to bottom in reading order: focal, support, detail. 10 to 12 px between layers, 6 px between rows.
- No boxes around everything. At most one `card` when two groups must be told apart.
- The island is narrow: put things side by side only when both stay readable (a 50 pt ring beside a curve, a 110 pt picture beside a score); otherwise stack.
- Fewer is better. Four rows at most: the ones in hand and the newest; the rest stay in the now line or out.

## 5. Colour

Neutrals carry the island; colour marks what matters, one meaning each (stick to the default, with just some accent colours when something is more important):
- white `var(--ink)` for what is in hand now and the focal number; grey `--ink2` for done and secondary; faint `--ink3` for what waits.
- blue `var(--blue)` for the one thing in hand: the lit dot, the newest point, the highlighted bar.
- green `var(--green)` for passed and done: ✓ ticks, a result that worked.
- orange only for a real block that needs someone; red only for a failure.
No other hues. Two series are told apart by line style and their names at their ends, not by two colours. Every island has at least one accent (a lit dot, a green tick, a blue point): an all-grey island looks dead.

## 6. Sub-agents

When `subagents` has entries, the user wants to see who works and who finished. If they are the moment (judges scoring, parallel builders, reviewers), draw them as rows: `<span class="dot"></span>` and white for working, `<span class="ok">✓</span>` and grey for done (with its result as the value when the log gives one: "7.1", "passed"), `<span class="bad">✗</span>` for failed. Name each by role, short: "Sol judge", "Opus judge", "Second persona", "Fresh review"; the model (`gpt-6.1-sol` is Sol, `claude-opus-5-5` is Opus) only when it tells judges apart. If they are not the moment, leave them to the helper rows.

## 7. History

`previous_island` is what the user saw a few minutes ago. Keep its design while the job does the same kind of work, so the island changes in place: the ring fills, a row turns ✓, a new point joins the curve. Change the design only when the work changes kind (from building to judging, from running to done).

## What you can use

- Layout: `div`, `span`, `table`, flexbox and grid, one `<style>` block and `style=""`.
- Classes: `row` (flex row, 12 px gap), `col` (takes the rest), `grid2`, `grid3`, `big` (30 px), `mid` (19 px), `unit` and `label` (small grey), `dim`, `faint`, `num`, `card`, `bar` with `<i style="width:40%">`, `ok` (green), `bad` (red), `warn` (orange).
- Status rows: `<div class="rows">` holding `<div class="r now"><span class="dot"></span>Transcribe<span class="v">MCP</span></div>`; a done row `<div class="r"><span class="ok">✓</span>Install</div>`; a waiting row `<div class="r wait"><span class="hollow"></span>Updates</div>`; a failed row `<div class="r"><span class="bad">✗</span>Zip</div>`. `v` is the grey value at the row's right.
- Chart tags (you give the data, the island draws it exactly; prefer them to hand-drawn SVG):
  - `<x-line series="6.4 7.1|5.9" names="Opus|Sol" min="4" max="10" target="8.5" target-label="8.5" ends="1" labels="1|2" h="72"></x-line>`: one line per series (`|` between series, oldest point first; the first solid white, the second dashed grey), a dashed target, `ends="1"` writes each name and last value at its end, `fill="1"` shades the first, `w` (default 328) and `h`.
  - `<x-bars items="Wake sends:83|Suggest AUC:92" max="100" unit="%" hl="Suggest AUC"></x-bars>`: labelled bars, `hl` lights one blue.
  - `<x-ring done="5.2" total="17.96" size="58" label="29%"></x-ring>`: a progress ring, green when whole.
  - `<x-dots done="2" total="5" now="3"></x-dots>`: rounds or phases as dots.
  - `<x-spark values="0 2.6 5.2" w="92" h="34"></x-spark>`: an inline sparkline.
- Pictures: `<img src="/absolute/path.png" style="width:110px;height:90px;object-fit:cover;border-radius:6px">`, only a path in `latest_picture` or `status`, only a picture of the work itself, fresh (`picture_min_ago` at most 15) unless the job is done.
- Refused: scripts, event handlers, links, forms, video, iframes, web fonts, anything from the network, CSS `content`, animations. Over 24 KB or 600 elements the answer is refused.

## Bad and good, from real islands

1. A test in a fresh VM. Bad (6 Nov): the whole VM desktop shrunk to 328 points, then three grey pills "Zip · Gatekeeper · First run" in 11 px. the user saw a blur and three words. Good: status rows, 14 px, ✓ for what passed, the lit dot on what runs now:
```html
<div class="rows"><div class="r"><span class="ok">✓</span>Install<span class="v">fresh VM</span></div><div class="r"><span class="ok">✓</span>Gatekeeper</div><div class="r"><span class="ok">✓</span>Speech<span class="v">4.4 s clip</span></div><div class="r now"><span class="dot"></span>Transcribe<span class="v">MCP</span></div></div>
```

2. A climb with judges. Bad: "Background" and a grey pill row "Clips · Thread · Page · Check". Good: the newest score big, the curve per judge against the target, each judge as a row:
```html
<div class="row" style="align-items:baseline;gap:8px"><span class="big">7.1</span><span class="unit">Opus, round 2</span></div>
<x-line series="6.4 7.1|5.9" names="Opus|Sol" min="4" max="10" target="8.5" target-label="8.5" ends="1" h="70" labels="1|2"></x-line>
<div class="rows" style="margin-top:10px"><div class="r"><span class="ok">✓</span>Opus judge<span class="v">7.1</span></div><div class="r now"><span class="dot"></span>Sol judge<span class="v">judging</span></div></div>
```

3. Several items. Bad: "Acme CVs" and one line about the last CV touched. Good: one row per CV, the one in hand lit:
```html
<div class="row" style="align-items:baseline;gap:8px;margin-bottom:10px"><span class="big">2</span><span class="unit">of 4 CVs updated</span></div>
<div class="rows"><div class="r"><span class="ok">✓</span>Acme</div><div class="r"><span class="ok">✓</span>Globex</div><div class="r now"><span class="dot"></span>Initech</div><div class="r wait"><span class="hollow"></span>Umbrella</div></div>
```

4. Building code, nothing measured yet. Bad: three 11 px grey pills "Curl skill · Lenient intake · Hardening". Good: the same parts as rows the eye can read, the one in hand lit:
```html
<div class="rows"><div class="r"><span class="ok">✓</span>Curl-only skill</div><div class="r"><span class="ok">✓</span>Lenient intake</div><div class="r now"><span class="dot"></span>Hardening<span class="v">rate limits</span></div></div>
```

5. Tests pass. Good: the count big and green, the checks under it:
```html
<div class="row" style="align-items:baseline;gap:8px;margin-bottom:10px"><span class="big ok">50</span><span class="unit">local checks pass</span></div>
<div class="rows"><div class="r now"><span class="dot"></span>Live endpoint<span class="v">next</span></div></div>
```

6. A report page under review. Bad: the whole page shrunk, its paragraphs unreadable. Good: the page's top cropped beside its score:
```html
<div class="row" style="gap:12px;align-items:center"><img src="/Users/me/Library/Application Support/FloatingIslands/pictures/task-clear-report-1.png" style="width:120px;height:96px;object-fit:cover;object-position:top;border-radius:6px"><div class="col"><div class="row" style="align-items:baseline;gap:6px"><span class="big">8.2</span><span class="unit">Sol, round 2</span></div><div class="label" style="margin-top:4px">target 8.5</div></div></div>
```

7. Done. Good: the result, green:
```html
<div class="row" style="align-items:baseline;gap:8px;margin-bottom:8px"><span class="big ok">8.6</span><span class="unit">mean, round 3 · target met</span></div>
<x-line series="6.4 7.1 8.7|5.9 7.4 8.5" names="Opus|Sol" min="4" max="10" target="8.5" ends="1" h="56"></x-line>
```

Before you answer, also check: `html` is given unless a question is pending; one element is clearly the largest; every text is at least 12 px except chart axes; there is at least one accent; no pill rows, no sentences, no shrunk full pages; the now line is 24 to 38 characters.
