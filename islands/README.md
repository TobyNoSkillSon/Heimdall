# Floating Islands

Small black islands that float over every app, Space and full-screen window. The helper app `FloatingIslands` draws
one island per JSON file in its data folder; Heimdall and the `islands` CLI write those files, and the user's clicks
come back as response files. This page owns the file protocol, the CLI, placement and the helper's checks.

Read this when: you want to write an island file by hand or from code, read the user's clicks, use the `islands` CLI,
change where islands dock or how they move, or run the helper's lab checks.
Code: `crates/islands` (`src/model/` pure models, `src/mac/` AppKit and Core Animation through objc2, `src/cli.rs` and
`src/cli/` the CLI; binaries `FloatingIslands` and `islands`), `scripts/bundle_islands.sh`,
`crates/app/src/islands/supervisor.rs` (Heimdall's launcher).

## Build and run

```
nice -n 19 cargo build --release -p islands -j 2   # target/release/FloatingIslands and target/release/islands
target/release/FloatingIslands [--max-hours N]      # quits itself after N h (default 24; 0 = no limit)
target/release/FloatingIslands --parent-pid PID     # quits when that process exits
target/release/islands clear && target/release/islands quit
```

- Heimdall bundles the helper (`scripts/bundle_islands.sh`) and starts it with `--max-hours 0 --parent-pid <its pid>`
  (`supervisor::helper_args`).
- The helper is an agent app (no Dock icon or menu bar) and runs only while launched; nothing is installed.
- The lab harnesses (`--selftest`, `--flicktest`, `--render`, `--viewer-shots`, …) exist only with `--features lab`;
  the bundled helper is built without them and refuses their flags (`src/mac/mod.rs`).
- `FLOATING_ISLANDS_DIR` moves the data folder. Lab runs always use their own folder, never the user's.

## The data folder

Default `Library/Application Support/FloatingIslands/` in the user's home (`model/file.rs` `Paths`).

| Path | Written by | What it is |
|---|---|---|
| `islands/<id>.json` | Heimdall, the CLI | one island. Create, rewrite or delete it and the island appears, changes or leaves at once (one kqueue watch, no polling). Write a temp file and rename (the CLI does) |
| `responses/<id>-<ms>-<pid>-<n>.json` | the helper | one click: `{island, kind, action, value, at}`. The reader consumes it by deleting it |
| `state.json` | the helper | what is on screen per island: `open`, `placed`, `side`, `x`, `y`, `width`, `height` |
| `visibility.json` | Heimdall's Show Islands (⌃⌘,), `islands hide-all` / `show-all` | `{"hidden": true}` takes every island off screen; no island file changes |
| `streams.json` | Heimdall's Show Islands on Streams | `{"shown": true}` puts island windows in screen shares and screenshots; missing or false keeps them out (the default) |
| `display.json` | Heimdall's `islands_display` setting | `{"display": "<name>"}` locks islands to that display; null or missing follows the pointer |
| `viewer-size.json`, `viewer-zoom.json` | the helper | the viewer's size and zoom per file ([viewer.md](../docs/viewer.md)) |

Coordinates everywhere (files, `--at`, `move`, `list`, `state`) are screen points from the display's top-left
corner, y down, naming the island's top-left corner.

While islands are hidden, an island with news comes back alone for a moment (`model/flash.rs`): it arrives, turns
done, failed or waiting, gets a question or a new waiting item, or a viewer or note shows new content (stage and
progress updates do not count). It fades in (`FADE_IN` 0.3 s) with one pulse, stays `HOLD` 4 s after its latest news
and fades out (`FADE_OUT` 0.6 s); the pointer over it, opening it, or an unseen question holds it, and released it
lingers `LINGER` 1.5 s. While hidden, an open viewer is not closed by clicks elsewhere.

### Response kinds

| `kind` | When | `action`, `value` |
|---|---|---|
| `button`, `list`, `toggle`, `slider` | a part's control | the control's action or value |
| `open`, `closed` | the open button; the X or Close | |
| `tick` | a plan item ticked | `done` or `undo`; the item id |
| `due` | a Scheduled item fell due (it stays in Scheduled) | `now`; the item id |
| `approval` | a proposal or item answer | `yes`, `no`, `later` or the proposal's own; the item or proposal id |
| `waiting` | a choice on Scheduled's guarantee line | the choice; the waiting item's id |
| `seen` | a proposal first drawn to the user | the proposal id |
| `report` | a page in the viewer called `window.heimdall.send` | see [viewer.md](../docs/viewer.md) |

What Heimdall does with plan-list responses: [one-system.md](../docs/one-system.md).

## The island file

```json
{
  "id": "tests",
  "preset": "task",                 // task | waiting | result | list | note | viewer | todo
  "title": "Tests",                 // 1-3 words (island-style.md)
  "state": "running",               // running | waiting | done | failed
  "progress": 0.42,                 // 0..1, only a live measured value; absent: no count
  "stage": "Building",              // 1-2 words, what it is doing now
  "summary": "One line",
  "steps": [{"title": "Download", "state": "done"}],   // done | now | failed | todo
  "items": ["Plain", {"title": "Port", "group": "running", "done": false}],   // group: running | waiting | discussed
  "kv": [{"label": "Size", "value": "20 GB"}],
  "image": "/path/picture.png",     // shown open; a click opens it
  "open": "https://… or /path",     // a button that opens it in the default browser or app
  "value": "412",                   // a result's headline
  "parts": [ … ],                   // the open island's content (islands-capabilities.md)
  "path": "/path/any.file",         // viewer only (viewer.md)
  "actions": [{"label": "Do all of it", "action": "do-all"}],   // viewer only
  "started": 1792596000, "updated": 1792596100, "ended": null,  // epoch s, stamped by the CLI
  "expanded": false,                // open; changes only by a click or the file
  "hidden": false,                  // off screen, file and place kept
  "side": "right",                  // left | right | top (absent: right; a plan list: left)
  "order": 1,                       // place in its dock, lower first
  "edgeY": 340,                     // pinned on its side edge at this height (set when the user throws or drops it)
  "position": {"x": 20, "y": 160},  // placed freely; wins over side
  "list": "goals",                  // plan list (preset todo): goals | scheduled | settled
  "attention": "waiting",           // waiting | question: a pulse every 6 s while closed
  "origin": {"from": "notch"},      // grow out of the notch; the helper drops the key
  "sent": "Promote the models",     // job island: what it was sent to do
  "goal": "g-website",              // job island: the goal it works for
  "width": 300, "icon": "SF.Symbol.name", "chosen": "action-id"
}
```

- What each field means for the agent, the parts and how a job island looks: [islands-capabilities.md](../docs/islands-capabilities.md).
- The plan lists' own keys (`slate`, `proposals`, `guarantee`, `goals`, `removed`) and what they draw:
  [one-system.md](../docs/one-system.md).
- Unknown keys are kept. A file that does not parse is never replaced.

## Plan lists in the helper

- `goals`, `scheduled` and `settled` dock left in that order, each `preset: "todo"` with `list` naming it and the list
  name as its id (`PlanList::ALL`). The Day Timeline island (`preset: "note"`, a `timeline` part) docks after them at
  `order` 3. There is no To do list: a `todo.json`, or a file whose `list` is `todo`, is not drawn.
- Closing a plan list (its X, right-click › Close) hides it: the helper writes `hidden: true` and `expanded: false` and
  a `closed` response, keeping every item. `islands show <id>` or Heimdall brings it back. Any other island's file is
  removed on close.
- Every helper-side write of a plan list refreshes its kept copy `.kept-<list>.json` (a Goals file without goals never
  replaces a copy that has some); a writer that finds the file gone starts from `.closed-<list>.json` or the kept copy.
- Item keys: `id` (stable; rows are diffed by it to animate, else by title), `title`, `done`, `due` (epoch seconds),
  `when` (a label for items without `due`), `detail`, `icon`, `goal`, `proposal: {text, actions}`, `chosen`, `told`.
- A row is its tick ring, a symbol (the item's `icon`, else `PLAN_SYMBOLS` in `model/plan.rs` from the title's project
  word) and the title on one line. A click on the ring ticks it (`tick` response); a click elsewhere on a row with a
  `detail` or an open `proposal` opens it in place (actions default to Yes / No / Later). Ticked rows sink, struck.
- A Scheduled item counts down from its `due` (`in 12 min`, `in 3 h`, `tomorrow 10:00`, `in 2 days`, `26 Oct`). Falling
  due, it moves to the top reading "Now" and the helper writes one `due` response, also for items that fell due while
  it was not running, and never again once Heimdall stamped `told: <its due>` on the item.

## The CLI

`islands --help` is the agent's reference; `islands put --help` and `islands plan --help` cover one command each
(`src/cli/usage.rs`). `put` refuses a missing or id-like title, a title over 3 words, a stage over 2 words and a
summary over one line ([island-style.md](docs/island-style.md)).

```
islands put <id> [--title T] [--state running|waiting|done|failed] [--stage WORDS] [--summary TEXT]
                 [--step "TEXT[=done|now|failed]"]... [--item "TEXT[=running|waiting|discussed|done]"]... [--kv "Key=Value"]...
                 [--progress F|none] [--image PATH] [--open URL|PATH] [--icon SF.Symbol] [--value V] [--preset P] [--parts JSON]
                 [--side left|right|top | --at X,Y | --auto] [--order N] [--expanded | --collapsed] [--from-notch] [--json JSON | -]
islands plan [goals|scheduled|settled] [list] | goals add "TEXT" [--when W] [--detail T] [--proposal TEXT] [--item-id ID] [--goal ID]
             | LIST done|undo|rm N|ID|"TEXT"... | LIST move N|ID|"TEXT" [--to goals] [--due TIME] [--when W] | LIST prune
islands view <path> [--id ID] [--title T] [--action "Label=action"]...
islands open|close|hide|show <id>...      (open/close aliases: expand/collapse, pin/unpin)
islands hide-all|show-all
islands move <id> X Y | <id> --side left|right|top | <id> --auto
islands rm <id>... | --all · islands clear · islands list [--json] · islands state [<id>] [--json]
islands responses [--consume] [--json] · islands quit · islands dir
```

- `put` merges into the existing file, so a place the user chose survives unless `--side`, `--at` or `--auto` is given.
- `--step`, `--item` and `--kv` replace their lists on each call (`none` clears). `--open` and `--image` resolve a
  relative path against the caller's folder; a bare domain gets https.
- `plan add` and `move` refuse Scheduled and Settled: promises are proposed for the user's yes and Settled fills from
  proof, both through Heimdall's `plan_edit` ([one-system.md](../docs/one-system.md)). `ISLANDS_LAB_PLAN_WRITES=1`
  lifts this for lab fixtures.
- `put`, `rm` and `clear` never touch a plan list; `clear` also deletes pending responses.

## Placement and motion

- Islands never enter the menu bar strip. Three docks: a column at the right edge (the default) and one at the left
  (the plan lists), 8 pt in, starting 60 % of the display's height up from its bottom (`COLUMN_START`) and stacking
  downward 8 pt apart; and a row under the notch, 6 pt below the menu bar. Docks step past placed islands and close up
  when one leaves.
- Every island that appears while the helper runs drops out of the notch into its slot; islands present at launch
  appear in place. `origin: {"from": "notch"}` asks for the same explicitly.
- Opening an island pushes the rest of its stack along on the same spring; its content scales with the shape. No
  island is taller than its display: longer content scrolls inside it.
- A click on a pill opens it; a click on an open island's title row minimises it (also right-click › Minimise, Esc on
  a viewer, `islands close`). The X (24 pt target) or right-click › Close closes it for good (`closed` response). The
  pointer passing over an island does nothing, and nothing closes on a timer.
- Answers ask first: a button, a proposal's action or a viewer's action turns its row into `Sure?  Confirm  Cancel`;
  Confirm within 4 s (`confirm::WINDOW`) sends it. "Later" goes in one click.
- Drag anywhere to carry an island. Released at 500 pt/s or faster (velocity over the last 80 ms), it is thrown: the
  throw is projected as a scroll decelerates (0.998 per ms, `layout::edge_drop`) and it pins to the nearer side edge at
  the height it lands (`side`, `edgeY`), on the `TOSS` spring. `put` keeps the place; `--side`, `--order`, `--at`,
  `--auto` and `move` replace it.
- Springs are `look::Spring` (`GROW`, `CALM`, `SNAP`, `ZOOM`, `TOSS`); the corner radius is
  `clamp(10 + (h − 32) × 0.4, 10, 24)` (`look::radius`). Every island has a white hairline (`HAIRLINE_WIDTH`) so it
  reads on black.
- Islands stay still during Space swipes: their panels join a private SkyLight space (`mac/overlay.rs`;
  `FI_OVERLAY=standard` turns it off for comparison).
- At rest nothing animates: no display link, no loops. An island arms at most three repeating timers, only while its
  window is on screen (`model/toggle.rs` `timers`): a running island's breath every 4 s (`WORK_INTERVAL`), an open
  body's duration on the minute, and the attention pulse every 6 s (`Attention::INTERVAL`). Hidden islands run none.

## Check it

Nothing here drives the user's pointer or keyboard. The harness flags need `--features lab`.

- `cargo test -p islands`: models, placement, plan lists and responses, flash, kept copies, open routes, viewer size,
  zoom, pinch and share (`crates/islands/tests/`).
- `FLOATING_ISLANDS_DIR=<lab dir> FloatingIslands --selftest`: real windows driven through each window's own event
  path (buttons, list, toggle, slider, drag, snap, plan ticks and approvals, attention, notch birth); also checks that
  nothing animates at rest.
- `FLOATING_ISLANDS_DIR=<lab dir> FloatingIslands --flicktest`: synthetic throws in real time; checks the edge, the
  height, room made in the stack, the files and rest at the slot.
- `FLOATING_ISLANDS_DIR=<lab dir> FloatingIslands --render <out>`: offscreen PNGs of the plan islands and contact sheets
  of each animation; no window shown.
- `islands/lab/one_system.sh [out]`: every one-system state offscreen; reports any layer not at rest.
- `islands/lab/lane_islands.sh <out>`: the confirm row offscreen.
- Viewer checks: [viewer.md](../docs/viewer.md).

## See also

- [islands-capabilities.md](../docs/islands-capabilities.md): what an island can show, `islands_put` fields and parts
- [one-system.md](../docs/one-system.md): the plan lists, proposals and how job islands stay current
- [viewer.md](../docs/viewer.md): the floating viewer
- [island-style.md](docs/island-style.md): how island text reads
- [model-html.md](docs/model-html.md): model-drawn HTML bodies in job islands
