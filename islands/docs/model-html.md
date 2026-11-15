# Model-drawn island bodies

A job island's body can be an `html` part: a fragment of real HTML and CSS with inline SVG, written by the island
agent. One hidden web view draws it once into a bitmap; the island shows the bitmap. Nothing draws at rest, and a part
is drawn again only when its HTML or width changes. This page also owns the task status file the island agent reads.

Read this when: you want to change what HTML a body may hold, add a chart tag, change the renderer, change what the
island agent may draw, or have a long task feed its island through `island-status.json`.
Code: `crates/islands/src/model/html.rs` (the gate), `crates/islands/src/model/html_charts.rs` (chart tags),
`crates/islands/src/mac/html.rs` (the renderer), `crates/islands/src/mac/parts.rs` (`resolve_html`, `job_shows_band`),
`crates/app/src/island_agent.rs`, `crates/app/prompt/island-agent-html.md`, `crates/app/src/islands/stages.rs`
(`status_in`, `merged_in`).

A whole HTML page in the viewer is a different thing: [viewer.md](../../docs/viewer.md).

## The part

```json
{"type": "html", "html": "<div class=\"row\">…</div><x-line series=\"…\" target=\"8.5\"></x-line>", "islandAgent": true,
 "fallback": [{"type": "line", "points": [6.03, 6.57, 7.63], "unit": "score"}]}
```

`fallback` holds ordinary parts ([islands-capabilities.md](../../docs/islands-capabilities.md)). They are drawn while
the bitmap is not ready, when the gate refuses the fragment, when the render fails, and for any `html` part past the
second in one island (`parts::HTML_PARTS` = 2).

## The gate

`model/html.rs` is plain Rust with no dependencies; the app includes the same file to check the island agent's answers.

- A whitelist of layout, text, table and SVG elements.
- `<style>` and `style=""` with no `&` at all, checked as written and with entities decoded: no `url()` but `#id`, no
  `image-set`, `cross-fade` or other image functions, no `@import`, `@font-face`, `content`, animations, transitions or
  escapes.
- No scripts, event handlers, links, forms, media, iframes or `javascript:`.
- Images only from local absolute paths that exist (`IMAGE_EXT`); for the island agent, only the island's latest picture
  or the status file's `picture`.
- Limits: 24 KB (`MAX_BYTES`), 600 elements (`MAX_ELEMENTS`), depth 24 (`MAX_DEPTH`), 4 images (`MAX_IMAGES`), 440 pt
  tall (`MAX_HEIGHT`). Over a limit, the part is refused and its fallback drawn.

## Chart tags

`model/html_charts.rs` expands these into SVG before drawing. The model gives the data; the gate computes the geometry,
which a small model gets wrong by hand.

- `<x-line series="2.2 5.0|3.1 5.5" names="A|B" min max target>`: one or more series, each named at its end.
- `<x-bars items="Wake:83|Suggest:92" max unit colors hl>`: labelled bars.
- `<x-ring value="0.29" size color label>`: a progress ring.
- `<x-dots done="11" total="16" now="12">`: a row of dots: done, now, still to come.
- `<x-spark values="1 3 2 5" w h color>`: an inline sparkline.
- `<x-steps items="Engine:done|Decider:now|Install:wait">`: a row of state pills, one lit.

They draw in neutrals unless an accent is named: a line's series are solid white, dashed grey, then faint, so two
series need no legend and no second colour.

## The style

`BASE_CSS` gives SF at 13 px, tabular figures, the neutrals (`--ink`, `--ink2`, `--ink3`, `--line`, `--fill`), four
accents (`--green`, `--blue`, `--orange`, `--red`) and a few classes (`row`, `col`, `grid2`, `big`, `unit`, `label`,
`card`, `bar`). Other colour names (`--purple`, `--cyan`, …) are neutral aliases. What each accent means:
[island-style.md](island-style.md).

## The renderer

`mac/html.rs`:

- Page JavaScript is off (`allowsContentJavaScript = false`; the app's own height query still runs), the data store is
  non-persistent, the CSP is `default-src 'none'; img-src file:`, and only the renderer's own document may load.
- Each render gets its own folder holding the document and copies of the gate's `<img>` pictures; the web view may read
  only that folder, so no spelling of a path reaches another file.
- Renders run one at a time. One over 3 s (`DEADLINE`) fails and the web view is torn down; a one-off failure is tried
  once more.
- Bitmaps are cached by fragment and width (16 kept, `CACHE`). The web view is released 150 s after the last render
  (`RELEASE_AFTER`; the lab overrides it with `FI_HTML_RELEASE`).
- Why HTML and not a native subset: every chart and layout would need its own drawing code, while a warm HTML update
  costs about 25 ms of CPU (measured offscreen on an M5 Max) and nothing runs at rest once the web view is released.

## In the island

- A closed island's bitmap is drawn ahead when its file changes, so it opens ready; an open island's update waits for
  its bitmap and then changes once (`mac/parts.rs` `resolve_html`, `mac/island.rs`).
- A job island shows its header, the body (`html`, a picture, a chart), and one bottom row: the agent's now line and the
  clock. The stage band folds behind that row (`job_shows_band`): a click on the row toggles the island's `details`; with
  no body, the band is the body. The band names each stage once (`band_named`).
- The bitmap is drawn at the main display's scale, so on a mix of 1x and 2x displays an island on the other display may
  be a little soft. An open island whose width changes shows its fallback for a moment.
- Clicks on an HTML body do nothing; the island's `open` target is the click.

## What the island agent may draw

`island_agent.rs` with the prompt addendum `crates/app/prompt/island-agent-html.md`:

- An optional `html` key in its answer, checked by the same gate; nothing may be stripped.
- Every number it shows must appear in its signals: its text, the chart tags' data as the gate parsed it, percentages
  in its styles; shares of a status file's `progress` count. No bookkeeping counters.
- Its `parts` are written as the fallback.
- Its signals include `merged`, the branches `git merge` brought in this run (`stages::merged_in`).

## The status file

A task may keep `island-status.json` in any folder and name its absolute path in a command, as it would a picture. The
binding finds the newest one a run named (`stages::status_in`); the island agent gets its content as `status` (at most
16 KB, `STATUS_BYTES`, one JSON object), and a rewrite wakes the agent (`Reason::Status`, within its usual spacing,
[one-system.md](../../docs/one-system.md)). Keys, all optional:

```json
{"kind": "climb", "updated": "2026-11-03T04:12:00+01:00", "now": "round 13 judging",
 "progress": {"done": 12, "total": 16, "unit": "rounds"},
 "target": {"value": 8.6, "label": "mean of both judges"},
 "series": [{"name": "A", "unit": "score", "points": [2.23, 4.99]}, {"name": "B", "unit": "score", "points": [3.09, 5.5]}],
 "values": [{"label": "mean, round 11", "value": 6.09}],
 "items": [{"label": "Tool calls", "state": "pass"}],
 "picture": "/absolute/latest.png"}
```

The hill-climb skill writes it after every judged round or kept attempt (the `hill-climb` skill, where a user has one), and
the writing-briefs skill tells long jobs to keep one (`skills/public/writing-briefs/SKILL.md`).

## Check it

- `cargo test -p islands html` (the gate and chart tags).
- `cargo test -p app island_agent` (the answer checks and the status file).

## See also

- [island-style.md](island-style.md): colours and how a body should read
- [islands-capabilities.md](../../docs/islands-capabilities.md): the other parts and job island fields
- [one-system.md](../../docs/one-system.md): when the island agent is called
- [islands/README.md](../README.md): the file protocol
