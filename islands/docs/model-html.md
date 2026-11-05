# Model-drawn island bodies: real HTML, drawn once (3 Nov 2026)

The goal: island bodies far more flexible, as HTML that can be rendered there, but cheap on compute, so the model has
maximum flexibility.

## What it is

A job island's body can be an `html` part: a fragment of real HTML and CSS with inline SVG, written by the island
agent (Claude Haiku 5.5, `crates/app/src/island_agent.rs`). One shared WKWebView, in a window that is never shown,
draws it once into a bitmap; the island shows that bitmap as a layer's contents. Nothing draws at rest, and a part is
drawn again only when its HTML or width changes.

```json
{"type": "html", "html": "<div class=\"row\">…</div><x-line series=\"…\" target=\"8.5\"></x-line>", "islandAgent": true,
 "fallback": [{"type": "line", "points": [6.03, 6.57, 7.63], "unit": "score"}]}
```

- **The gate** (`crates/islands/src/model/html.rs`, std only; the app crate includes the same file to check answers):
  a whitelist of layout, text, table and SVG elements; `<style>` and `style=""` with no `&` at all, checked as written
  and with entities decoded: no `url()` but `#id`, no `image-set`, `cross-fade` or other image functions, no `@import`, `@font-face`,
  `content`, animations, transitions or escapes; no scripts, event handlers, links, forms, media, iframes or
  `javascript:`; images only from local absolute paths that exist (and, for the agent, the island's fresh latest
  picture or the status file's). Limits: 24 KB, 600 elements, depth 24, 4 images, 440 pt tall, two `html` parts per
  island. Over a limit, the part is refused and its `fallback` (the usual slots) is drawn.
- **Chart tags** (`model/html_charts.rs`): `<x-line>`, `<x-bars>`, `<x-ring>`, `<x-dots>`, `<x-spark>`, `<x-steps>` (a row of state pills, one lit) expand into SVG
  before drawing. The model gives the data; the gate computes the geometry, which a small model gets wrong by hand.
  They draw in neutrals unless an accent is named: a line's series are solid white, dashed grey, then faint, each
  named at its end (`names`), so two judges need no legend and no second colour.
- **The style**: `BASE_CSS` gives SF at 13 px, tabular figures, the neutrals (`--ink`, `--ink2`, `--ink3`, `--line`,
  `--fill`), four accents (`--green` #30D158, the Goals green; `--blue` #0A84FF, Heimdall's blue; `--orange` and `--red`
  for a block or a failure) and a few classes (`row`, `col`, `grid2`, `big`, `unit`, `label`, `card`, `bar`). The
  palette is `islands/docs/island-style.md`'s **Colour**; the retired hues (`--purple`, `--cyan` …) are neutral aliases.
- **The renderer** (`crates/islands/src/mac/html.rs`): page JavaScript off (`allowsContentJavaScript = false`; the
  app's own height query still runs), a non-persistent data store, a CSP of `default-src 'none'; img-src file:`, a
  navigation delegate that allows only the renderer's own document. Each render gets its own folder (the document
  and copies of the pictures in the gate's own `<img>` tags) and the web view may read only that folder, so no spelling of a path reaches another
  file. Jobs run one at a time; a render over 3 s fails and the web view is torn down with it (a one-off failure is
  tried once more; nothing drawn is final). Bitmaps are cached by fragment and width (16 kept). The web view is released
  150 s after the last render; WebKit's GPU process leaves about a minute later.
- **In the island** (`mac/parts.rs` `resolve_html`, `mac/island.rs`): a closed island's bitmap is drawn ahead when its
  file changes, so it opens ready; an open island's update waits for its bitmap (milliseconds warm) and then changes
  once; a bitmap not ready yet, refused or failed shows the `fallback` parts instead.
- **The agent** (`island_agent.rs`, prompt addendum `prompt/island-agent-html.md`): an optional `html` key in its
  answer, checked by the same gate (nothing may be stripped), every number it shows in the signals (its text, the
  chart tags' data as the gate parsed it, percentages in its styles; shares of a status file's progress count), no
  bookkeeping counter, its `parts` written as the fallback. The frozen design prompt stays as it was.

## The body leads (4 Nov)

On 4 Nov three job islands showed only the stage bar and "open turn, N helpers", and the instructions behind them
looked stale. They were: the island agent's frozen
round-3 prompt told it to draw nothing while reading, during sub-agents and in review, and the HTML addendum to leave
`html` out without a number, so most moments got no body; the now line had no place on the island since 3 Nov.

- A job island shows its header, the agent's body (`html`, a picture, a chart), and one bottom row: the agent's now
  line (13 pt, 12 pt when long) and the clock. The stage band folds behind that row (`mac/parts.rs` `job_shows_band`):
  a click on the row toggles the island's `details`, a small chevron says which; with no body, the band is the body.
- The addendum (`prompt/island-agent-html.md`) now overrides the round-3 rules it contradicts and names the one telling
  thing per kind of job (climb curve, training ring and loss, a night pass's branches as pills, the picture of visual
  work, rings for counts), with seven drawn bodies.
- The signals add `merged`: the branches `git merge` brought in this run (`stages::merged_in`).
- Long jobs keep `island-status.json` (Heimdall's `writing-briefs` skill, `docs/nightly-build.md`, `hill-climb`).

## The status file

A task may keep `island-status.json` (any folder; it names the absolute path in a command, as it would a picture).
The binding finds the newest one a run named (`stages::status_in`); the island agent gets its content as `status`
(at most 16 KB, one JSON object), and a rewrite of it wakes the agent (`Reason::Status`, within the usual two-minute
spacing). Keys, all optional:

```json
{"kind": "climb", "updated": "2026-11-03T03:58:00+01:00", "now": "round 12 judging",
 "progress": {"done": 11, "total": 16, "unit": "rounds"},
 "target": {"value": 8.5, "label": "mean of both judges"},
 "series": [{"name": "Opus", "unit": "score", "points": [2.23, 4.99]}, {"name": "Sol", "unit": "score", "points": [3.09, 5.5]}],
 "values": [{"label": "mean, round 11", "value": 6.09}],
 "items": [{"label": "Tool calls", "state": "pass"}],
 "picture": "/absolute/latest.png"}
```

The hill-climb skill writes it after every judged round or kept attempt (`skills/workers/hill-climb/SKILL.md`).

## Why HTML, and what it costs

Measured offscreen on the M5 Max on battery, 3 Nov, with a video playing and other agents' jobs
running: four open job islands (climb, download, test run, labelling), this branch's build with the islands as HTML
against the same build with the same islands as today's slots. CPU seconds and footprints, not wall time. Evidence
and method: `lab/islands-html/results.md`.

| | today's slots | HTML |
|---|---|---|
| at rest, web view released (150 s after the last change) | ~0 CPU, no web process | ~0 CPU, no web process (WebKit's GPU process leaves ~1 min later) |
| at rest, web view held | — | 0.04–0.12 s CPU a minute in the app (WebKit re-reading screens while a video churns the display); web processes ~0 |
| app footprint | 15–16 MB | 27 MB; +33 MB in three WebKit processes while held |
| first frames of four islands | 0.06 s CPU | 0.30–0.33 s CPU; first render 387 ms wall (web process start) |
| one update, cold | < 0.01 s | ~0.25 s CPU |
| one update, warm | < 0.01 s | ~25 ms CPU; 5–16 ms wall |

A native markup subset would cost what today's slots cost, but every chart, table and layout would need its own
drawing code and the model could only use what was built. HTML costs about a quarter of a CPU-second per island
update at worst, against updates at most every two minutes per island, nothing at rest once released, and about 11 MB
of memory: not clearly too high, so HTML is the format. The WebKit the viewer island already uses is the same framework; the shared
renderer adds its own web process only while it draws.

## Also in this change

- The palette: stick to the default, with accent colours only when something is more important. The band, the chart tags' defaults and the climb body lost
  their purple and light blue; neutrals everywhere, green and blue as accents, orange and red only for a wait or a
  failure. Before and after: `lab/islands-html/shots/islands.html`.
- No line under a job island's title: the errand (`sent`) stays in the file and in the agent's signals.
- The band names each stage once (`band_named`): a job that went back to Reading names "Reading" where it is now, not
  under every earlier stretch; consecutive same-stage segments merge.
- The app ignores a screen-parameters notice that changes no screen (`App::screens_changed`): with a web view in the
  process, a video on the display sent ~120 a second and each relaid out every island. Today's app takes the same
  storm whenever a report viewer island is open.
- `FloatingIslands --offscreen` (lab): the whole app with no window ordered in, for cost runs; `--html-bench OUT DIR`
  draws fragments and times them. Lab runs never show islands on the user's screen.

## Gaps

- The bitmap is drawn at the scale of the main display; on a mix of 1x and 2x displays an island on the other one may
  be a little soft (untested). An open island whose width changes with an update shows its fallback for a moment.

- The bodies in the lab were written to the prompt's spec by hand from recorded data; Haiku has not yet written one
  (no model call from the lab). The first live answers are the real test of the prompt.
- Heimdall Remote and the Swift rollback build do not draw `html` parts (Remote shows nothing for one; the fallback
  is not used there).
- Clicks on an HTML body do nothing; the island's `open` target is the click.
