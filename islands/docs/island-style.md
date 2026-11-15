# Island text and look

How an island's words, colours, icon and motion must read. This page owns the island style rules; the tools and the
CLI refuse what they can check (marked "Checked").

Read this when: you want to write an island's title, stage, summary or items, pick its state, icon or colours, draw a
body or part, or judge whether an island reads at a glance.
Code: `crates/app/src/islands/tools.rs` (`style_problem`, for `islands_put` and `islands_view`),
`crates/islands/src/cli/put.rs` (`style_problem`, for `islands put`), `crates/app/src/islands/mod.rs` (`ICON_SVG_MAX`),
`crates/islands/src/mac/look.rs` (colours), `crates/islands/src/model/html.rs` (`BASE_CSS`).

## Words

- **Title**: 1–3 words naming the thing: "Qwen-Image", "iPad sale", "Male voices". Never an id or a file name; a new
  island needs one. Checked: word count, and a lower-case title equal to the id is refused. `islands_put` caps it at
  40 characters.
- **Stage**: one or two present-tense words for the activity the user cares about: "Downloading", "Final tests", "Your
  answer". Not the mechanism ("Rendering"). Checked: more than 2 words is refused.
- **Summary**: one plain line: what it is and what happens next. "20 GB of weights, then convert and install". No ids,
  paths or model internals. Checked: one line, 110 characters at most.
- **Lists**: `steps` (`--step`) for a sequence, with the current step `now`; `items` (`--item`) for a plain list.

## State and colour

- `running`: working, nothing needed from the user (white).
- `waiting`: needs the user: an answer, a test, a look (orange).
- `done`: finished; give it a `value` or an `open` target when there is a result to see (green). Green only when
  finished and nothing still runs.
- `failed`: stopped; the summary says why and what next (red).

Colour is the system's neutrals for almost everything: label white (text, the main line, the stage in hand), secondary
grey (units, labels, a second series), tertiary grey and separators (tracks, hairlines, what is behind). Four accents,
each for one meaning and at most one per body:

| Accent | Value | Meaning |
|---|---|---|
| green | `#30D158` | done, passing, settled |
| blue | `#0A84FF` | running in a goal, the one thing to watch |
| orange | `#FF9F0A` | waiting on the user, a real block |
| red | `#FF453A` | a failure |

- No purple, pink, light blue, teal or indigo (in `BASE_CSS` those names are neutral aliases).
- Two series (two judges, baseline and candidate) differ by weight and line style (solid white against dashed grey)
  and by their names at their line ends, never by two hues; the leading one may take the one accent.
- The stage band is neutral: the stage in hand white, earlier stages dim grey, orange only while waiting, red on
  failure, the green check when done.

## Icon

A tiny drawing of exactly what the island is about: an inline SVG (viewBox `0 0 24 24`, a few bold plain shapes: path,
circle, ellipse, rect, line, polyline, polygon, g; no text, styles, links or images; at most 2,048 bytes,
`ICON_SVG_MAX`), or an SF Symbol name when one truly fits (`ipad`). It is drawn at 19 pt (`look::ICON_SIZE`) as a
silhouette in the state's colour, so it must read at that size. Checked. Only Goals, Scheduled and Settled keep fixed
icons.

## Show, don't tell

- Prefer what can be seen: a timeline for a sequence, a progress bar for a count, bars for a comparison, a line for a
  trend, one big number for one measure, the latest picture for visual work, the icon for what it is.
- One glance answers what this is, whether it works or needs the user, and what comes next.
- Every element earns its place; nothing repeats what another element shows.
- Keep `kv` values numeric with a short unit (`7/12`, `42%`, `41 GB`) so they earn a bar or sparkline.

## Motion

Running shows a tiny sign of work every few seconds (the arc turns once, or the icon takes one breath); waiting is
orange; done is the green check, and only done. Nothing moves in between, and nothing animates at rest.

## Place and attention

- Running work is a job island on the right, one per thread, with that thread's items as its steps; it never sits in a
  plan list. The left holds Goals, Scheduled and Settled. `side: top` puts an island in the row under the notch. Leave a
  place the user chose alone.
- `attention`: `waiting` (needs the user) or `question` (a question to answer). The helper pulses the closed island
  every 6 s until the user opens it. Remove the key when it no longer needs the user. Plan lists get it from an open ask.

## Plan items

- Each item is a short, concrete line that names the project first ("Website: push branch"); no vague items ("decide
  the end version"). The list shows only that line and a symbol from the project name, so the title must carry the
  item alone. `detail` (why, what next, any length) shows only when the user opens the row.
- Give every item a stable `id` (the CLI makes one from the text).
- Scheduled items carry `due`, the absolute time (`plan_edit` takes a local date-time, `2026-10-24T10:00`; tonight is
  23:00). Never a vague `when` ("Tomorrow", "Sat") when the date is known.
- An ask in Goals that needs a decision gets a `proposal`: what exactly is proposed, with the details.
- What may enter each list, and who writes it: [one-system.md](../../docs/one-system.md).

## See also

- [islands-capabilities.md](../../docs/islands-capabilities.md): the fields and parts an island can carry
- [model-html.md](model-html.md): HTML bodies and their CSS palette
- [islands/README.md](../README.md): the file protocol and CLI
