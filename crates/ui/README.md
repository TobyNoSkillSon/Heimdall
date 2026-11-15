# ui: the menu bar item and windows

The whole native UI of Heimdall, in AppKit through objc2 (no web views, no Swift): the menu bar helmet and its menu,
Setup and permissions, the transcript, Speakers, the Agent window, and the notch island (`src/island/`, its own README).

Read this when: you want to change a menu row, a window or the UI protocol, find which file draws a section, run the
crate's tests, or render the UI offscreen.
Code: `crates/ui/src/lib.rs`, `crates/ui/src/model.rs`, `crates/ui/src/mac/`, `crates/ui/src/bin/demo.rs`.

## Use it from the app

```rust
ui::set_attachment_loader(|id| archive_bytes(id)); // before run: Agent window images, read off the main thread
let ui = ui::Ui::handle();                        // Copy + Send: push from any thread
ui.send(ui::UiUpdate::Sources(sources));          // applied on the main queue
ui::run(ui::Options::default(), move |action| { /* UiAction, on the main thread */ }); // never returns
```

- `UiUpdate` (app to UI) and `UiAction` (UI to app) in `src/model.rs` are the whole protocol. Pending state updates
  coalesce to the latest value per field (`src/updates.rs`); events stay in order. Updates sent before `run` wait for it.
- `Ui::on_main(f)` runs `f` on the main queue after the updates already sent; the watchdog uses it as the UI heartbeat.
- `UiUpdate::Transcript` inserts or replaces by `id`: send a partial, then the same id with `is_final: true`.
- On `UiAction::Quit` the app shuts down and answers `UiUpdate::Terminate`. Only with `Options::fake_permissions` (the
  demo) does the UI quit by itself, after 2 s (`State::quit` in `mac/mod.rs`).
- `Options`: `fake_permissions` (never call permission APIs), `render_dir` and `render_checks_only` (the demo's
  `--render`/`--checks`), `system_profile` (path of `system.json`), `lab` (no menu bar item, no ⌃⌘M, shareable island
  windows), `friends` (personal edition only).
- `ui::permissions::status()` never prompts.

## Where things are

| File | What it holds |
|---|---|
| `src/lib.rs` | `Ui`, `Options`, `run`, attachment loader, `display_sizes`, the `render_*` entry points behind the demo's flags |
| `src/model.rs` | `UiUpdate`, `UiAction`, `Tile`, `MenuSection`, `BackgroundTask` and the menu's plain state; its tests |
| `src/mac/mod.rs` | the main-thread `State`, the `HeimdallUIController` target of every control, the update queue, menu sync |
| `src/mac/menu.rs` | `menu::build` (the whole menu, top to bottom), the model pickers (`Picker`: Agent, Background, Speaking), `apply_sections` |
| `src/mac/controls.rs` | the switch tiles, `CheckRow` (Launch at Login, section disclosures), `LabelRow` (status and session lines) |
| `src/mac/prominent.rs` | `ProminentRow`, Support the Developer (the one row on a lighter fill) |
| `src/mac/helmet.rs` | the menu bar helmet (template, crossed out while nothing is heard), `menu_bar_attention_image()` (red; nothing sets it yet), the app icon geometry |
| `src/mac/engine_section.rs`, `engine_rows.rs` | Engine section; Engine ▸ Add Model's per-precision rows |
| `src/mac/decider_section.rs` | Decider section: precision pick and Confirm (`SetDeciderPrecision`) |
| `src/mac/model_choice.rs`, `provider_rows.rs`, `provider_key.rs` | Agent ▸ Model's click-then-Confirm, Add Provider's rows, the connect panel |
| `src/mac/speaking_menu.rs`, `personalise.rs` | Personalisation: the voice picker, Name & Icon |
| `src/mac/background_picker.rs`, `background_paused.rs` | Background Tasks ▸ Model and the task toggles; their greyed emergency look |
| `src/mac/menu_group.rs`, `menu_motion.rs`, `submenus_left.rs` | open-section grouping, open-menu resize at display rate, every submenu opening to the left |
| `src/mac/shortcuts.rs`, `src/shortcuts.rs` | Carbon hot keys; the table (`Shortcut::ALL`: ⌃⌘H, ⌃⌘\\, ⌃⌘', ⌃⌘., ⌃⌘/, ⌃⌘M, ⌃⌘,) |
| `src/mac/setup/` | the Setup window, one page per step (`pages.rs`), its drawn kit, microphone and downloads steps |
| `src/mac/permissions.rs` | `status`, `request_all` |
| `src/mac/agent.rs`, `src/agent_view.rs`, `src/agent_debug.rs` | the Agent window (draws); its rows and edit rules; its telemetry strip |
| `src/mac/transcript.rs`, `speakers.rs`, `speaker.rs` | the transcript, Speakers, the floating name entry |
| `src/mac/bugs.rs`, `friend_invite.rs` | personal-edition windows (the `public` feature drops the Bug Notes row) |
| `src/identity.rs`, `speech_voice.rs`, `islands_display.rs`, `memory.rs` | name and mark, voice state, island display lock, process memory |
| `src/mac/*_render.rs`, `offscreen.rs`, `render.rs`, `menu_live.rs`, `menu_rows.rs` | lab-only render and check harnesses |
| `src/island/` | the notch island: [src/island/README.md](src/island/README.md) |
| `crates/ui/assets/` | `Heimdall.icns` (bundled by `scripts/bundle.sh`), provider marks, Vella's mark; each folder's `SOURCES.md` |

## Change the menu

- What each setting does is on its topic page (See also). This section holds only the rules the code enforces.
- The menu stays open for every setting. Tiles and Launch at Login are views that take their own clicks
  (`mac/controls.rs`); submenu radio and checkbox rows use the whole row as the click target. Rows that open a window
  or Finder, and Quit, close the menu.
- Menu sync (`mac/mod.rs`, `mac/menu.rs`): while the menu is closed nothing syncs and `menu_stale` is set; the next open
  rebuilds it (`menuNeedsUpdate:`), or syncs it in `menuWillOpen:` if the rebuild could not take the state. While it is
  open, a state change syncs rows in place. A model added or removed shows at once in Agent ▸ Model (only the rows
  above Providers are laid again, so an open picker stays open) and in Background Tasks ▸ Model; a provider connected
  while open appears on the next open.
- Optimistic state: tiles, App Audio checkboxes, Launch at Login, voice rows and Name & Icon change at the click; the
  app confirms with the matching update (Listening shows the clicked state until a `Listen` update). A refused Seeing
  or Screen Recording click makes the app redraw the menu from the settings in force (`screen_setting_rejected` in
  `crates/app/src/live_loop.rs`).
- Tile actions: `Tile::toggle` in `src/model.rs` (Listening `SetListening`, Seeing `SetSeeing`, Speaking
  `SetSpeakReplies`, Agent `SetDryRun`, Engine `SetEngine`). The five are independent: each turns off only itself.
- Foldable sections are `MenuSection::ALL` (Agent, Engine, Decider, Personalisation, Background Tasks). A fold sends
  `SetSectionsCollapsed` with the whole folded list; the app keeps it in settings `menu_collapsed`.
- Row highlight (`mac/controls.rs`, one rule for `CheckRow`, `LabelRow` and `ProminentRow`): a row is highlighted only
  while the pointer is inside it. A press-drag out drops it (tracking stays on during a mouse drag); a press that closes
  the menu and `menuDidClose:` drop every row's hover. Keyboard highlight is the menu's own.
- Gotcha, macOS 27: a view row gets nothing on Return, so tiles and Launch at Login toggle only by pointer or by an
  accessibility press (AXPress on each tile's checkbox).
- Gotcha, macOS 27: menu SF Symbols are hidden for apps linked against SDK 26+ unless `preferredImageVisibility` is set
  (`mac/util.rs`).
- A `LabelRow` truncates in the middle at the menu's width and has a tooltip only while its text is cut.
- `UiUpdate::ComponentStatus` keeps independently owned warnings in the orange line under the tiles while listening
  runs; `message: None` clears only that component. Paused, setup and failed listening states take precedence.

## Permissions

`request_all` runs only from Setup's Allow All, and never with `Options::fake_permissions`. It asks only for the
permissions not asked before (a once-only ledger in user defaults): Microphone, then `UiAction::RequestSystemAudio`
(macOS has no status or request call for System Audio Recording Only, so the app starts a tap and reports the state),
then Screen Recording and Accessibility. Allow All emits `UiAction::SetupAllowed` first. While Setup is open and
unresolved, the UI re-checks every 5 s and whenever an app becomes active (`start_setup_polling` in `mac/mod.rs`).
What each permission is for: [setup.md](../../docs/setup.md).

## Test

1. `cargo test -p ui`. The dev-dependency turns on the `lab` feature, so the render harnesses build too. The full
   check (`scripts/check.sh`) runs the same tests under `cargo nextest`.
2. Public edition menu: `cargo test -p ui --features public --test menu_rows` (no Bug Notes row).
3. The island alone: `cargo test -p ui --lib island::`.

Integration tests in `crates/ui/tests/` (each drives a demo harness offscreen unless noted):

- `agent_event_path.rs`: Agent window and question card through their real controls (`--agent`).
- `bug_notes_window.rs`: Bug Notes cards and the evidence click (`--bug-notes`).
- `menu_rows.rs`: rows under synthetic pointer events, a model added while open (`--menu-rows`).
- `island_composer.rs`, `island_pulldown_drop.rs`, `island_chat_bench.rs`, `island_motion_nudge.rs`: see the island README.
- `menu_event_path.rs`: `#[ignore]`d; opens the real menu on screen (`--checks`). Run it only when the user is away.

In-crate test modules: `model.rs` (tiles, menu lane, permissions, screen recording, microphones, app audio, secret
text), `updates.rs`, `shortcuts.rs`, `identity.rs`, `speech_voice.rs`, `islands_display.rs`, `memory.rs`,
`agent_view.rs`, `agent_debug.rs`, and in `mac/`: `menu.rs`, `controls.rs`, `util.rs`, `menu_group.rs`,
`submenus_left.rs`, `model_choice.rs`, `provider_rows.rs`, `provider_key.rs`, `engine_rows.rs`, `decider_section.rs`,
`background_picker.rs`, `speaking_menu.rs`, `friend_invite.rs`.

## Render and check offscreen

Build: `cargo build -p ui --features lab --bin heimdall-ui-demo`. Without a flag it prints its help and exits. Each
flag writes into `<dir>` and exits 1 on a failure.

- Menu: `--offscreen <dir> [--icns <file>]` (helmet, app icon and `.icns`, tiles, Recording rows; `--icns
  crates/ui/assets/Heimdall.icns` rebuilds the bundled icon), `--chrome` (whole menu, transcript naming),
  `--menu-rows`, `--menu-climb`, `--menu-bench` (no dir: sync time with 30 apps).
- Sections and pickers: `--engine-section`, `--engine-models`, `--decider-section`, `--model-choice`,
  `--relay-account`, `--claude-relay`, `--background-picker`, `--speaking`, `--personalise`, `--memory`,
  `--islands-display`, `--forms` (connect panels).
- Windows: `--agent`, `--bug-notes`, `--wizard` (every Setup step), `--friends <dir> <qr.png>`.
- Island: `--island`, `--motion`, `--gallery` and the rest, listed in [src/island/README.md](src/island/README.md).
- On screen (run only when the user is away): `--render <dir>` and `--checks <dir>` (the real menu opens about a
  quarter second per render; mouse events go into the menu window's own queue, the cursor never moves),
  `--menu-live`, `--menu-sections-live`, `--submenus-left`, `--streams` (a near-invisible probe window), `--drop-live`,
  `--drawing-over-menu`, and `--live` (the live demo; never from an agent).

## See also

- [settings.md](../../docs/settings.md): every setting the menu changes
- [models.md](../../docs/models.md): model pickers, providers, context sliders
- [inference-engine.md](../../docs/inference-engine.md): the Engine section and tile
- [decision-chain.md](../../docs/decision-chain.md): the decider and its precisions
- [personalise.md](../../docs/personalise.md), [voices.md](../../docs/voices.md): Name & Icon, voices
- [screen.md](../../docs/screen.md): Seeing ▸ Recording; [capture.md](../../docs/capture.md): microphones, App Audio
- [agent-window.md](../../docs/agent-window.md): the Agent window and the island question card
- [setup.md](../../docs/setup.md): Setup and permissions
- [claude-relay.md](../../docs/claude-relay.md), [emergency.md](../../docs/emergency.md): relay and emergency mode
- [speakers.md](../../docs/speakers.md): Speakers and transcript naming
- [architecture.md](../../docs/architecture.md): where the UI sits in Heimdall
