# ui — Heimdall's menu bar and windows

**Domain: menu and windows (+ the island's home).** This crate is the whole native UI. The island — pill, notes,
questions, pull-down, reader, drawing overlay, pointing, animation gallery — lives in `src/island/` with its own
README; everything else here is the menu bar item and menu (`mac/menu.rs`, `mac/controls.rs`), setup and
permissions, transcript, speakers, the Agent window (`agent_view.rs`, `mac/agent.rs`) and their offscreen renders.
**Interface:** `Ui::handle()` / `Ui::send(UiUpdate)` from any thread, `run(Options, on_action)` on the main thread,
`UiAction` back to the app.
**Testing:** `cargo nextest run -p ui`, about 11 s (the agent-window event-path test is most of it). Renders and
checks are flags of `heimdall-ui-demo` written into `lab/` (`--help` lists them; Demo and renders below); never run it
without a flag (it opens live windows).

Native AppKit through objc2, in Vella's visual language: a dark native menu with SF Symbols (made visible on macOS 27
through `preferredImageVisibility`), Vella's in-menu radio rows, NSAlert prompts, and the Models table's hairlines.
No web views, no Swift.

## API (for crates/app)

```rust
let ui = ui::Ui::handle();                 // Copy + Send: push from any thread
ui.send(ui::UiUpdate::Sources(sources));   // applied on the main queue, in order
ui::run(ui::Options::default(), move |action| { /* main thread */ }); // never returns
```

- Menu bar: Heimdall's helmet (`src/mac/helmet.rs`, our own drawing of the crescent-horned helmet), a template image
  in the bar's ink; crossed out, like a `.slash` symbol, whenever Heimdall hears nothing (off, setup needed, stopped,
  or every source off). `helmet::menu_bar_attention_image()` is the "needs attention" state for when Heimdall
  cannot recover: the same helmet, not crossed out, in `systemRed`, not a template (for the watchdog to set; nothing
  sets it yet). The same geometry draws the gold app icon (`assets/Heimdall.icns`, bundled by
  `scripts/bundle.sh` as `CFBundleIconFile`).
- Menu top: five switch tiles (`ui::Tile`) in two rows, Control Center style. The top row is what Heimdall takes in:
  **Listening** (`SetListening`; off stops microphone and app-audio capture and transcription, unloads the decider,
  crosses out the menu-bar helmet) and **Seeing** (`SetSeeing`; off stops screen capture entirely). The bottom row is
  what it does: **Speaking** (`SetSpeakReplies`), **Agent** (`SetDryRun`; emitted before the tile redraws, so the
  app's callback closes delivery synchronously; off stops only the reasoning agent, while listening and the decider
  keep running) and **Engine**, the local engine (`SetEngine`, symbol `cpu`). On is the accent blue with
  white ink, off a quiet fill. The five switches are independent and each turns off only itself:
  every tile is always available, and with Listening off the agent still runs (notices, wakes, typed messages),
  speaks if Speaking is on, and the screen records if Seeing is on. Engine's tile follows `UiUpdate::Engine` in one look per
  state, never blended: off the quiet fill; starting or loading the quiet fill with a native spinner
  in the glyph's place, the blue filling in from the left as the engine reports a percentage (accessibility reports it
  mixed); ready or idle solid blue;
  on with no model loaded the soft accent the Engine section shares; failed solid red with a warning glyph. One click
  always toggles: every state but Off turns off, a failed engine included; Off turns on. Under them, one line
  only when the tiles don't say it: a problem in orange (Set Up… opens Setup; a failure that already says it stopped
  is shown alone) or grey "Listening off since 14:05". It and the Agent session line are `LabelRow` views: one colour on
  every path, a long message truncates in the middle at the menu's width with the whole text in the tooltip. They
  appear and disappear in place while the menu is open, which moves the rows below; reserving their height would leave
  an empty gap under the tiles at rest.
- Foldable sections (30 Oct): **Agent**, **Engine**, **Personalisation** and **Background Tasks** have a header row of
  their own (`CheckRow::disclosure`: the section's symbol, its title, a small chevron on a round chip, right when
  folded, down when open, so it never reads as a submenu arrow). A click folds the section to its
  header or opens it, in the open menu (the resize runs at display rate, `menu_motion`); the choice is sent
  (`SetSectionsCollapsed`, the whole folded list in one action) and kept in settings (`menu_collapsed`), so the next launch opens it the same. Folded
  headers stack without separators (`menu::apply_sections`).
- **Engine** (`mac/engine_section.rs`, after Agent): the header's trailing edge says what the engine does (Off,
  Starting or Loading with a spinner or ring, Pick a model in the soft accent while on with none loaded, Ready, Failed
  in red with the reason whole on the line under it). Then only the precisions on this Mac, one row each (a grey
  precision after the model's name): a click loads it (`LoadEngineModel`; the agent's model never changes); the loaded
  one rests on Support the Developer's lighter fill; off, the rows are grey; waiting, they rest on the soft accent. A
  download in flight shows inline, greyed, a small spinner and a bar, and joins the list when it lands. Last, Add
  Model ▸: every catalog model, each a submenu of its precisions (`engine_rows`): a click on one not here downloads it
  (its size and arrow too), a download shows a bar and a stop, the trash asks once (a red Delete pill), a precision
  over the 48 GB ceiling warns with its RAM; a model downloading shows its percentage on its row.
- **Decider** (`mac/decider_section.rs`, after Engine; checks `--decider-section`, renders `lab/decider-section`): the
  header names the precision that runs (a spinner while it loads or swaps, red Failed when nothing runs); the version
  caption ("Decider V5"); a reason line (red) or a missing folder's stand-in (orange); the precisions, largest first,
  each with one instance's memory, the running one lit. A click marks another (soft accent) and shows the glowing
  Confirm under the list; Confirm sends `SetDeciderPrecision`. The current one, Escape or closing the menu cancels; no
  clicks while swapping. Setup's Decider step (after Model Provider) offers the same three with memory and disk;
  Continue sends a changed pick.
- Sections: **Listening** (Microphone ▸, App Audio ▸) · **Seeing** (Recording ▸) · **Agent** (the caption, Model ▸,
  Debug Agent) · **Personalisation** (26 Oct 16:45, in place of Speaking: the voice caption, Model ▸, Speed, Wake Sound,
  Name & Icon ▸: the mark grid and the Name and Your Name fields, `mac/personalise.rs`; the name is also the wake word,
  so there is no wake-words field; the Speaking tile stays the on/off) · **Background Tasks** (the caption, Model ▸: the
  one model every background job runs, `SetBackgroundModel`, Claude Haiku 5.5 on Anthropic by default; then a toggle
  per job, `model::BackgroundTask`: Islands, the plan refresher, Voice Names, the voice namer, and Day Timeline, the left island of where the day went, `SetBackgroundTask`; in emergency mode, `UiUpdate::AgentRelay(Emergency)`, the toggles
  are greyed under one orange warning triangle whose tooltip is the only words, the reset as HH:MM when known,
  `mac/background_paused.rs`) · the windows and files (Open
  Transcript, Speakers…, Open Archive Folder, Shortcuts ▸) · Launch at Login, Support the Developer… (the one row resting
  on a lighter background, Vella's Copy Last Transcript look; opens the developer's page), Quit. Agent ▸ Model, Personalisation ▸
  Model and Background Tasks ▸ Model are one pop-up picker design (`menu::Picker`):
  On This Mac (the local engine alone, its loaded model's name in grey after it: no model, precision or download
  choices, which are the Engine section's; the own engine's models; the local voices), then Cloud (the models
  added from connected providers; ElevenLabs), each a radio row with its provider's mark; a row with settings opens in
  place with a disclosure (a model's context slider, its stops and default in `docs/models.md`, thinking level and price, the built-in
  engine's included, `SetEngineContext`; Phonon S's presets; ElevenLabs' voices). Then Providers, the connected ones
  with their own submenus, subscriptions first (Claude Code subscription ▸ Remove once added, greyed while it is the
  agent), then this Mac, then API keys (the local engine ▸ grey facts: the state, "Port 8100", "Context 262,144
  tokens", "Cache 41.2 of 100 GiB", the licence, Remove; a cloud provider ▸ Add Model with a search over its list, Sign In Again…, Replace
  Key… or Change URL…, Disconnect). Last, Add Provider, a collapsible section (a `CheckRow` disclosure; closed while an
  online provider is connected, the user's choice kept for the run): the providers not connected yet in three groups from
  `ApiProviderRow::group`, Subscription (ChatGPT…, OpenCode Go…), On This Mac (Own Engine…), API Key (Anthropic…,
  OpenRouter…; OpenAI-Compatible is Own Engine now and shows only under Providers when already connected; in Speaking ▸ Voice only Own Voice Engine…, beside Phonon S's four suggested voices). ChatGPT's Connect is a browser-only sign-in: the
  panel says "Continue in your browser" with Open Browser and Cancel, no code and no Use API Key… (`key_fallback`
  false; the app answers `ProviderLogin` with an empty code). Background Tasks ▸ Model has no details, no Providers and no Add
  Provider: its last row, Add Model…, closes the menu and pops up Agent ▸ Model under the helmet (`openAgentModels:`),
  where all provider work lives. Every picker is as wide as its own rows (a native menu's width; an opened model's
  details may widen it), never its nested pickers' names. A `LabelRow` has a tooltip only while its text is cut.
  OpenAI-Compatible is not offered in Add Provider (it is the Own Engine, any URL, key optional);
  one connected before stays in Providers. Speaking ▸ Voice: Phonon S is the one built-in voice (a saved Supertonic
  voice keeps its checked row until another is chosen), then Own Engine's voices (`OwnVoiceState`, provider
  `own-voice`), ElevenLabs, Providers and Add Provider in the same groups; every voice row, presets and listed voices
  included, is a radio that keeps the menu open and sends its `SpeechVoiceAction` (SetLocal, SetPreset, SetOwnVoice,
  SetElevenLabsVoice), again on a re-click. Every submenu opens to the left of its parent, at every
  level (`mac/submenus_left.rs`, lab/menus-left/).
  Agent ▸ Model ends its Cloud rows with Claude Code subscription once it is added (`Settings::relay_added`: Add
  Provider ▸ Claude Code subscription adds it and leaves it there, never the agent)
  (`SelectAgentRelay`; checked while `Settings::agent_relay`,
  when no model row is; any model row ends it, docs/claude-relay.md).
  The Agent caption reads "model · thinking level · provider" ("Claude Opus 5.5 · High · Anthropic"); its item
  is Debug Agent. There is no Reload row: a setting that did not apply live shows orange under the tiles ("Screen
  Recording didn't apply — Retry") and a click there sends `Reload` (Setup's line outranks it). In the transcript a
  double-click on an unidentified "[Discord · Speaker 2]" opens "Who is Speaker 2?" and sends
  `NameTranscriptSpeaker`; `heimdall-ui-demo --chrome <dir>` renders and checks all of this offscreen. **App Audio ▸** lists one checkbox per app playing or heard this session plus the apps already chosen, each with
  its Finder icon, "— Playing" while it sounds; nothing checked by default. **Recording ▸**: the continuous screen
  recording's Resolution (smallest first; with one display each row names the frame size it records, from the display's
  native pixels: "834 × 540", "1114 × 720", "Half · 1728 × 1116" (default), "Native · 3456 × 2234"; with several displays
  "960 × 540", "1280 × 720", "Half Native", "Native") and Frame Rate (1 / 2 / 5 (default) / 10 per second). A fixed cap
  that records what Half or Native records on every display is left out; 1080p is not offered (it is Half's size on 4K
  and just under it on the Retina laptop) but a saved 1080p or an edited rate still shows, checked. No memory, storage
  or energy figures and no caption. With Seeing off the choices stay selectable and apply when it is on again. Last,
  under a separator, the Show Islands on Streams switch (moved from the window rows).
- The menu stays open for every setting. Tiles and Launch at Login are views that take their own clicks
  (`mac/controls.rs`); submenu radio and checkbox rows are Vella's SettingsMenuItem, with the whole row as the click
  target. Rows that open a window or Finder, and Quit, close the menu. Arrow keys highlight every row; macOS 27 sends a
  view row nothing on Return, so Launch at Login and the tiles toggle by pointer or accessibility press (AXPress on
  each tile's checkbox element).
- Shortcuts ▸: ⌃⌘H Message Heimdall (the notch's conversation pulled down with its composer holding the keyboard;
  again while typing pulls it up), ⌃⌘. Listening, ⌃⌘/ Seeing, ⌃⌘M Drawing Mode, ⌃⌘, Show Islands (`shortcuts.rs`,
  Carbon hot keys, no permission). ⌃⌘H is no macOS default (Apple's list, checked 26 Oct) nor in the user's
  `com.apple.symbolichotkeys`.
- Agent window (`mac/agent.rs`, content in `agent_view.rs`): the live Pi stream from `UiUpdate::AgentWindow`
  (`History` replaces, `Entries` insert or replace by id, so a streaming reply is one block), the agent's notices
  in full, and images read from the archive by id through `ui::set_attachment_loader` (off the main thread). The
  header says the run state and offers Stop (while a turn runs) and Reload (disabled with Agent off). The composer
  sends with Send or Return (Shift-Return is a new line); while a turn runs Send waits for it and Interrupt steers.
  A typed message that is saved and on the active branch has Edit while the agent is ready (greyed with the reason
  otherwise); resending says it doesn't undo what the agent already did. NeedsReview replaces the composer with the
  interrupted turn, what ran after it, and Completed / Abandon (once per delivery id). Off disables the composer,
  engine waiting queues, Failed keeps Reload. Raw errors stay in the log. The legacy `AgentHistory`/`AgentEntries`
  updates are not drawn (the live stream carries the same entries).
- The island at the notch (questions, notes, the conversation, the reader, drawing mode, pointing, notch drops) is
  one object with its own README: `src/island/README.md`.
- Transcript: utterances with no words or notes draw nothing.
- `UiUpdate` and `UiAction` (`src/model.rs`) are the whole protocol. `Transcript` inserts or replaces by `id` (send a
  partial, then the same id with `is_final: true`). On `UiAction::Quit`, shut down and send `UiUpdate::Terminate`; the
  UI quits by itself after two seconds otherwise.
- The UI changes its own state optimistically for the tiles, App Audio checkboxes and Launch at Login;
  the app confirms by sending the matching update (Listening shows the clicked state until a `Listen` update).
  A refused Seeing or Screen Recording click is noted and the menu redrawn from the settings in force, so the tile
  leaves its clicked state (`live_loop::screen_setting_rejected`).
- Menu sync (`mac/mod.rs`, `mac/menu.rs`): while the menu is closed nothing syncs; the next open rebuilds it from the
  state (`menuNeedsUpdate:`, 15–40 ms on the main thread), or syncs what changed if the rebuild could not take the
  state. While it is open, a state change syncs rows in place; a model added or removed shows at once in Agent ▸
  Model (only the rows above Providers are laid again, so an open picker stays open) and in Background Tasks ▸ Model;
  a provider connected while open appears on the next open. App Audio titles (Finder icon + name) are cached.
- Row highlight (`mac/controls.rs`, one rule for `CheckRow`, the status `LabelRow` and `ProminentRow`): a row shows
  the pointer's highlight only while the pointer is inside it. A press-drag out drops it (tracking is enabled during
  mouse drag, else no exit arrives until release); a press that closes the menu and `menuDidClose:` drop every
  row's hover. Keyboard focus highlight is the menu's own.

## Permissions

`ui::permissions::status()` never prompts. Setup's Allow All first emits `UiAction::SetupAllowed` (Heimdall on, ordered
at the click, so an Off clicked while the dialogs are up wins), then calls `request_all`: Microphone, then
`UiAction::RequestSystemAudio` (no On/Off intent) (the app starts one short audio tap; macOS has no status or request call for System
Audio Recording Only, so the app reports `Unconfirmed` — Setup's "Ready", not a missing permission — then `Granted`
once real sound arrives from a tap, or `Denied` when an app checked in App Audio plays for 10 s while its tap stays
digitally silent), Screen Recording,
Accessibility. While Setup is open the UI re-checks once a second; the timer stops when it closes.
`Options::fake_permissions` disables every permission call (the demo).

## Demo and renders

`heimdall-ui-demo --offscreen <dir> [--icns <file>]` draws, with nothing on screen, the menu-bar helmet on and off on
light and dark bars at 1× and 2× (`menubar-*.png`), the app icon at every size macOS reads (`Heimdall.iconset/`,
and the `.icns` built from it with `--icns crates/ui/assets/Heimdall.icns`), the real tile row with Listening on
and off (`tiles-*.png`), the menu's first frame (the tiles and the line under them) in every engine state with
Listening on and off (`menu-top-engine-{off,starting,ready,error}-listening-{on,off}-{dark,light}.png`), Seeing ▸
Recording's real rows (`menu-recording*.png`: default dark and light, two displays, a saved 1080p at 3 fps) and the
engine submenu in each state (`menu-engine-*.png`), API Providers not connected, connected, rejected and with the local
model selected (`menu-api-*.png`), its Add Model picker loaded, loading and failed, the provider's own submenu, the
connect panel signing in (`provider-sign-in-*.png`) and with the API-key fallback (`provider-key-panel*.png`) and the whole menu with Haiku selected (`menu-haiku-*.png`), all on the menu's
background colour sampled from the real menu,
not its material. The submenu renders draw section headers and separators in an approximation of AppKit's metrics;
the radio rows and fact lines are the menu's own views. Grey lines take the grey sampled from the real menu, since
AppKit draws translucent label text far brighter offscreen than in the menu. The whole menu's first frame
(`menu.png`, `menu-light.png`, `menu-healthy.png`, `menu-tiles-off*.png`, `menu-setup-needed.png`,
`menu-failed.png`) is the real `menu::build` on a UI state with no status item, stacked the same way: view rows are
the menu's own; native rows (symbol, title, chevron, ⌘Q, grey when disabled), headers and separators are drawn in
the real menu's measured metrics and sampled colours. The tiles' blue is the offscreen accent, a little brighter
than the menu's material shows it. Renders that need clicks or keys in the open menu (`menu-click-*`,
`menu-keyboard-*`, the submenu captures) come from `--render`/`--checks`, which put the menu on screen.

`heimdall-ui-demo` runs the UI with fake data. `heimdall-ui-demo --render <dir>` writes PNGs of every window and the
real menu, then quits. Windows draw through `cacheDisplayInRect` at alpha 0 and are click-through (no screen capture,
no permission). The menu really opens for about a quarter second per render, at the status item, in dark (what the user
sees) and light (`-light`). The render pass also types a name into the voice-name entry and presses Name in-process, then runs the menu
checks: with the menu open it posts mouse events into the menu window's own event queue (the cursor never moves) on
each tile, Launch at Login and a Voice row, and reads back that each toggled in place with the menu still open, then
posts arrow keys and checks the highlight. A freshly opened menu takes its first release as the end of the opening
click, so each sequence starts with a click on the menu's top padding. `heimdall-ui-demo --checks <dir>` runs only the
checks (exit 1 on failure; the ignored `tests/menu_event_path.rs` wraps it). A popped-up menu takes the keyboard and
closes on outside clicks, so agents run these while the user is idle; outside input shows as lost menus and gets a retry.
`heimdall-ui-demo --motion <dir>` is the island's visual judge: every island card played at 120 Hz offscreen and its
frames judged (`<dir>/anims/judge.json`, with the main thread's cost per frame); exit 1 on a finding. The island's
other lab flags: `src/island/README.md`.
`heimdall-ui-demo --claude-relay <dir>` renders Background Tasks normal, in relay mode and in emergency mode (with
and without a reset) and Agent ▸ Model with Claude Code subscription off and on, and checks the in-place switch while the menu is
open (lab/ui-renders/claude-relay/).
`heimdall-ui-demo --model-choice <dir>` drives Agent ▸ Model's click-then-confirm through the real state (a click
proposes, Confirm glows under the row and sends exactly what was clicked, closing drops it), Claude Code subscription's model
rows and Add Provider ▸ the local engine, and renders each state (lab/ui-renders/model-choice/).
`heimdall-ui-demo --menu-rows <dir>` sends synthetic pointer events to the real menu rows in a window never ordered in
(drag-out highlight, Support the Developer after its press, a model added while open) and writes each state at 2×
(`tests/menu_rows.rs`); `--menu-bench` times the open menu's sync with 30 apps. `--bench-chat N` and `--bench-layout N`
time the island conversation (`tests/island_chat_bench.rs`).
`heimdall-ui-demo --agent <dir>` draws the Agent window in every run state and the question card offscreen
(`agent-*.png`, `island-question*.png`, `island-connect-t3-*.png`) and drives their controls in-process; it is safe
while the user works (nothing is ordered in) and `tests/agent_event_path.rs` runs it. Current renders: `lab/ui-renders/`.

Build with `CARGO_TARGET_DIR=target/ui`.

`ComponentStatus` keeps independently owned warnings in the existing orange menu
header while `ListenState::Listening` remains active. `None` clears only that
component. Paused/setup/failed listening states take precedence; the pause action
and hearing icon still reflect actual listening rather than a fallback warning.
