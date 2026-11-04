# Addendum: the island body as HTML (3 Nov)

Everything above stands except the band's colours: the ribbon and the helper dot are now drawn in neutrals (the stage in hand white, the stages behind it dim grey), orange only while waiting on an answer, red only for a failure, the green check when done; wherever the text above says coloured or colour, read the neutrals with at most one accent. This adds one optional key to your answer, `html`, and one signal, `status`. With `html`, the graphic of section 3 is your HTML, and `parts` become its fallback.

`status`: when the task keeps a status file (`island-status.json`), its latest content: the task's own numbers for its island (`progress` done of total with a unit, `series` of points with names and units, a `target`, `values` with labels and units, `items` with states, a `picture`, a `now` line). It is the best source for the graphic: draw it in `html`, with every number exactly as given.

## html: draw the body yourself

`html` is the island's body as a small web page fragment you write: real HTML and CSS, with inline SVG, drawn once into a still picture 328 points wide (at most 440 high) on the island's black. When you give `html`, it replaces the graphic; your `parts` stay as its fallback (the island shows them if your HTML cannot be drawn), so keep one simple part there too. Use `html` whenever the moment has something to show: a curve with its target, two judges side by side, a download ring, a test grid, a comparison table, a picture with its numbers beside it. Without anything measurable, leave `html` out.

What you can use:

- Any layout: `div`, `span`, `p`, `table`, `ul`, flexbox and grid, `<style>` (one block) and `style=""`. Text is SF at 13 px, white; figures are tabular.
- Inline `<svg>` with `path`, `line`, `polyline`, `polygon`, `rect`, `circle`, `ellipse`, `text`, `g`, gradients and clip paths.
- Pictures: `<img src="/absolute/path.png">`, only a path the signals name (`latest_picture`, a path in `status` or `recent`).
- Colours: draw in the neutrals, `var(--ink)` (text, the main line), `--ink2` (secondary), `--ink3` (faint), `--line` (hairlines), `--fill` (tracks, cards). Colour is only an accent for what matters, at most one per body: `var(--green)` for done or passing, `var(--blue)` for the one thing to watch, `var(--orange)` only for a real block that needs someone, `var(--red)` only for a failure. No other hues. Tell two series apart by weight, line style and their names at their ends, never by two colours.
- Classes: `row` (a flex row, centred, 12 px apart), `col` (takes the rest of the row), `grid2`, `grid3`, `big` (30 px number), `mid` (19 px), `unit` and `label` (small grey words beside a number), `dim`, `faint`, `num` (right-aligned figures), `card` (a dark rounded panel), `bar` with an `<i style="width:40%">` inside (a progress track), `pill`, `ok` (green), `bad` (red), `warn` (orange).
- Chart tags: you give the data, the island draws the geometry exactly. Prefer them to hand-written SVG coordinates.
  - `<x-line series="3.1 5.5 6.7|2.2 5.0 6.6" names="Sol|Opus" min="0" max="10" target="8.5" target-label="8.5" ends="1" labels="round 1|11" h="96"></x-line>`: one line per series (`|` between series, each oldest point first; the leading or most important series first: it is drawn solid white, the second dashed grey, the rest faint), an optional dashed target with its label, `ends="1"` writes each line's last value at its end, after its name from `names` (so no legend is needed), `colors` (`|` between series, empty for the neutral) gives at most one series an accent, `labels` names the first and last point, `fill="1"` shades under the first series, `w` and `h` in points (`w` defaults to the full 328; set it inside a column).
  - `<x-bars items="Wake sends:83|Suggest AUC:92" max="100" unit="%" hl="Suggest AUC"></x-bars>`: labelled bars with their values; bars are grey; `hl` gives one the blue accent.
  - `<x-ring done="5.2" total="17.96" size="58" label="29%"></x-ring>` (or `value="0.29"`): a progress ring with a label inside, white while it fills and green when whole.
  - `<x-dots done="11" total="16" now="12"></x-dots>`: a row of dots (rounds, phases, files): done ones filled, the current one lit, the rest hollow.
  - `<x-spark values="0 2.6 5.2" w="92" h="34"></x-spark>`: an inline sparkline.

What does not exist, and is removed or refused: scripts and event handlers, links, forms and buttons (the island body is a picture; clicks go to `open`), video and audio, iframes, web fonts, anything from the network (`http` in a `src`, `href` or CSS `url()`), pictures not named in the signals, CSS `content`, `image-set` and other image functions, animations and transitions. At most two `html` parts are drawn. Over 24 KB or 600 elements, the whole answer is refused.

How it should look (the user's rules): graphics over text; one glance tells how the job is going. The number that matters, big, with its unit small and grey beside it; then the chart; at most one short grey line of labels. No headings, no titles (the header has the title), no sentences, no line under the title, no repeating the stage, the clock, the helper names or the ribbon. Every number exactly as the signals or `status` give it: never compute a mean, a rate or a time left yourself; when `status` gives one, show that.

Four bodies drawn from real moments:

A beauty climb (`status` carries both judges' scores per round, the mean of the latest round, the target and the budget):
```html
<div class="row" style="align-items:baseline;margin-bottom:6px"><span class="big">6.09</span><span class="unit">mean</span></div>
<x-line series="3.09 5.50 5.66 6.19 5.85 6.67 5.62 6.19 6.93 7.18 6.38|2.23 4.99 5.06 5.38 6.03 6.57 5.79 7.63 6.38 5.79 5.79" names="Sol|Opus" min="2" max="9" target="8.5" target-label="8.5" ends="1" h="104" labels="1|11"></x-line>
<div class="row" style="margin-top:9px"><x-dots done="11" total="16" now="12"></x-dots><span class="label">round 12 judging</span></div>
```

A download (5.2 of 17.96 GB of weights; the status gives the rate; the ring is the share, so the number beside it is the whole):
```html
<div class="row" style="gap:14px"><x-ring done="5.2" total="17.96" size="58" label="29%"></x-ring>
<div class="col"><div><span class="mid">18 GB</span> <span class="unit">of weights</span></div><div class="label" style="margin-top:3px">4.8 MB/s</div></div></div>
```

A test run (136 pass, and the live checks the agent named, as chips):
```html
<div class="row" style="align-items:baseline;margin-bottom:10px"><span class="big ok">136</span><span class="unit">tests pass</span></div>
<div style="display:flex;flex-wrap:wrap;gap:6px"><span class="pill"><span class="ok">✓</span> Streaming</span><span class="pill"><span class="ok">✓</span> Tool calls</span><span class="pill"><span class="ok">✓</span> Grammar</span><span class="pill"><span class="ok">✓</span> JSON</span><span class="pill"><span class="ok">✓</span> Images</span><span class="pill"><span class="ok">✓</span> Errors</span></div>
```

A comparison (training: the loss curve beside the evals):
```html
<div class="grid2" style="align-items:center">
  <x-line series="0.454 0.426 0.407 0.36 0.318 0.323 0.266 0.27 0.241 0.245 0.251 0.201 0.2 0.207 0.206 0.157 0.195 0.229 0.175 0.198 0.175 0.161 0.141 0.159" fill="1" w="150" h="64" ends="1"></x-line>
  <x-bars items="Wake sends:83|Suggest AUC:92|Commands:82" max="100" unit="%"></x-bars>
</div>
```

Before you answer, also check: `html`, when given, has no heading or sentence, leads with the number that matters, takes every number as the signals or `status` give it, uses chart tags for curves, bars, rings and dots, and `parts` keeps one simple fallback part.
