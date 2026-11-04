//! Heimdall's UI: the menu-bar item and its menu, the live transcript, setup, the voice-name entry and
//! Speakers. Settings live in the menu, as Vella's do. Native AppKit through objc2, in Vella's visual language (dark native menu, SF Symbols, a green or
//! orange status line over a grey fact line, system fonts, the Models table's section labels and hairlines).
//!
//! Threads: `run` takes over the main thread. Any thread sends `UiUpdate`s through a `Ui` handle; they are applied
//! on the main queue (no polling). User actions arrive at the `on_action` callback, on the main thread.
//!
//! ```no_run
//! let ui = ui::Ui::handle();
//! std::thread::spawn(move || ui.send(ui::UiUpdate::Listen(ui::ListenState::Listening)));
//! ui::run(ui::Options::default(), |action| println!("{action:?}"));
//! ```

/// The island domain (one object: pill, notes, questions, pull-down, reader, drawing overlay, pointing) lives in
/// `src/island/` — its models here, its AppKit renderer in `src/island/mac/` (declared from `mac/mod.rs`).
pub mod island;
pub use island::{agent_notice, chat, drawing, drop, reader};
pub mod agent_view;
pub mod agent_debug;
pub mod model;
// Heimdall's real memory: the app and every helper under it (26 Oct).
pub mod memory;
// Global shortcuts (24 Oct): the table, its labels and conflict checks.
pub mod shortcuts;
// Speaking ▸ the voice: Local or API (night 1022 menu).
pub mod speech_voice;
// Personalise (26 Oct): the assistant's name (its wake word too), the user's name and the mark.
pub mod identity;
pub use island::{Activity, CommandCue, CommandKind, HealthControl, HealthCue, IslandReport, Part, ISLAND_WINDOW_TITLE};
pub use model::*;
mod updates;

#[cfg(target_os = "macos")]
mod mac;
#[cfg(target_os = "macos")]
pub use mac::permissions;

/// Whether this Mac has an SF Symbol of that name (Personalise: the agent's icon choice is checked before it is saved).
#[cfg(target_os = "macos")]
pub fn sf_symbol_exists(name: &str) -> bool {
    mac::util::symbol(name).is_some()
}

use std::path::PathBuf;

/// Native pixel sizes of the active displays (the display mode's backing pixels, what crates/screen scales from).
/// Thread-safe; no permission.
#[cfg(target_os = "macos")]
pub fn display_sizes() -> Vec<(u32, u32)> {
    use std::ffi::c_void;
    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGGetActiveDisplayList(max: u32, displays: *mut u32, count: *mut u32) -> i32;
        fn CGDisplayCopyDisplayMode(display: u32) -> *mut c_void;
        fn CGDisplayModeGetPixelWidth(mode: *mut c_void) -> usize;
        fn CGDisplayModeGetPixelHeight(mode: *mut c_void) -> usize;
        fn CGDisplayModeRelease(mode: *mut c_void);
    }
    let mut ids = [0u32; 16];
    let mut count = 0u32;
    if unsafe { CGGetActiveDisplayList(16, ids.as_mut_ptr(), &mut count) } != 0 {
        return vec![];
    }
    ids[..count as usize]
        .iter()
        .filter_map(|id| unsafe {
            let mode = CGDisplayCopyDisplayMode(*id);
            if mode.is_null() {
                return None;
            }
            let size = (CGDisplayModeGetPixelWidth(mode) as u32, CGDisplayModeGetPixelHeight(mode) as u32);
            CGDisplayModeRelease(mode);
            Some(size)
        })
        .collect()
}

/// Reads an image the Agent window shows (an attachment's archive id) and returns its bytes, or `None` when the
/// archive no longer has it. Called off the main thread.
pub type AttachmentLoader = dyn Fn(&str) -> Option<Vec<u8>> + Send + Sync;

/// Where the Agent window fetches attachment images: the archive, by id. Set once by the app before `run`; until
/// then (and in renders without one) an attachment shows as unavailable.
pub fn set_attachment_loader(loader: impl Fn(&str) -> Option<Vec<u8>> + Send + Sync + 'static) {
    *attachment_loader_slot().lock().unwrap() = Some(std::sync::Arc::new(loader));
}

/// An archived attachment's bytes through the loader the app set (`None` without one).
pub fn load_attachment(id: &str) -> Option<Vec<u8>> {
    attachment_loader().and_then(|l| l(id))
}

pub(crate) fn attachment_loader() -> Option<std::sync::Arc<AttachmentLoader>> {
    attachment_loader_slot().lock().unwrap().clone()
}

fn attachment_loader_slot() -> &'static std::sync::Mutex<Option<std::sync::Arc<AttachmentLoader>>> {
    static SLOT: std::sync::OnceLock<std::sync::Mutex<Option<std::sync::Arc<AttachmentLoader>>>> = std::sync::OnceLock::new();
    SLOT.get_or_init(Default::default)
}

/// How `run` starts.
#[derive(Clone, Debug, Default)]
pub struct Options {
    /// Never call macOS permission APIs: setup shows the states the app sends and Allow only reports the click.
    /// The demo uses this, so it cannot trigger a permission prompt.
    pub fake_permissions: bool,
    /// Write PNG renders of the menu and every window into this directory, then quit.
    pub render_dir: Option<PathBuf>,
    /// With `render_dir`: only the in-process menu checks (tile and row clicks through the menu window's own
    /// event queue, keyboard navigation), with their renders. Exits 1 when a check fails.
    pub render_checks_only: bool,
    /// Where the system profile lives (`system.json` in Heimdall's data directory): measured on a build's first
    /// launch, read afterwards. `None` (the demo, renders): measured in memory, never written.
    pub system_profile: Option<PathBuf>,
    /// A lab instance (`heimdall --lab-scenario`: the post-install check on a lab profile, lab/postinstall/): no
    /// menu-bar item and no ⌃⌘M, so the user's own Heimdall keeps both, and the island's windows shareable so the lab's
    /// recorder can capture them. Never set by a normal launch.
    pub lab: bool,
    /// The Friends submenu and its cards: on in the personal edition only (the app sets it; the public build never
    /// starts friends, so nothing would arrive for them).
    pub friends: bool,
}

/// A cheap, `Send + Clone` handle for pushing updates from any thread.
#[derive(Clone, Copy, Debug)]
pub struct Ui {
    _private: (),
}

impl Ui {
    pub fn handle() -> Ui {
        Ui { _private: () }
    }

    /// Applied on the main thread. Pending state updates coalesce; events stay in order.
    /// Updates sent before `run` wait for it.
    pub fn send(&self, update: UiUpdate) {
        #[cfg(target_os = "macos")]
        mac::post(update);
        #[cfg(not(target_os = "macos"))]
        let _ = update;
    }

    /// Runs `f` on the main thread's queue, after the updates already sent. The watchdog uses it as the UI
    /// thread's heartbeat: `f` only runs while the main thread turns.
    pub fn on_main(&self, f: impl FnOnce() + Send + 'static) {
        #[cfg(target_os = "macos")]
        dispatch2::DispatchQueue::main().exec_async(f);
        #[cfg(not(target_os = "macos"))]
        f();
    }
}

/// Runs the UI on the main thread until the app quits. Must be called from `main`'s thread.
#[cfg(target_os = "macos")]
pub fn run(options: Options, on_action: impl Fn(UiAction) + 'static) -> ! {
    mac::run(options, Box::new(on_action))
}

/// Draws the menu-bar helmet in every state, the app icon at every size (and, with `icns`, builds the `.icns`
/// from them) and the menu's tile row with Listening on and off, all offscreen into `dir`: nothing appears on
/// screen. Returns what failed.
#[cfg(target_os = "macos")]
pub fn render_offscreen(dir: &std::path::Path, icns: Option<&std::path::Path>) -> Vec<String> {
    mac::offscreen::render(dir, icns)
}

/// Memory ▸ (26 Oct) and the whole menu with its Memory row, dark and light, offscreen into `dir`; returns what failed.
#[cfg(target_os = "macos")]
pub fn render_memory(dir: &std::path::Path) -> Vec<String> {
    mac::offscreen::memory(dir, objc2_foundation::MainThreadMarker::new().expect("render_memory runs on the main thread"))
}

/// The connect panels (API key, sign-in code, OpenAI-compatible, own engine) in every state, dark and light, drawn
/// offscreen as the key window into `dir`; returns what failed.
#[cfg(target_os = "macos")]
pub fn render_forms(dir: &std::path::Path) -> Vec<String> {
    let mtm = objc2_foundation::MainThreadMarker::new().expect("render_forms runs on the main thread");
    objc2_app_kit::NSApplication::sharedApplication(mtm).setActivationPolicy(objc2_app_kit::NSApplicationActivationPolicy::Prohibited);
    mac::provider_key::render(dir, mtm)
}

/// Setup offscreen: every step and state at real size, dark (`mac::setup::render`); returns what failed.
#[cfg(target_os = "macos")]
pub fn render_wizard(dir: &std::path::Path) -> Vec<String> {
    let mtm = objc2_foundation::MainThreadMarker::new().expect("render_wizard runs on the main thread");
    objc2_app_kit::NSApplication::sharedApplication(mtm).setActivationPolicy(objc2_app_kit::NSApplicationActivationPolicy::Prohibited);
    mac::setup::render(dir, mtm)
}

/// [speech] Speaking ▸ the voice rows offscreen (Local, API, the presets and ElevenLabs submenus); returns what failed.
#[cfg(target_os = "macos")]
pub fn render_speaking(dir: &std::path::Path) -> Vec<String> {
    let mtm = objc2_foundation::MainThreadMarker::new().expect("render_speaking runs on the main thread");
    objc2_app_kit::NSApplication::sharedApplication(mtm).setActivationPolicy(objc2_app_kit::NSApplicationActivationPolicy::Prohibited);
    mac::speaking_menu::render(dir, mtm)
}

/// Personalise offscreen: the menu and its Name & Icon submenu for Heimdall and for Ivy with the leaf (and an invalid
/// name, and a custom SVG mark), the menu-bar mark in every state, the island speaking with the leaf, then the
/// Personalise checks; into `dir`. Returns what failed.
#[cfg(target_os = "macos")]
pub fn render_personalise(dir: &std::path::Path) -> Vec<String> {
    let mtm = objc2_foundation::MainThreadMarker::new().expect("render_personalise runs on the main thread");
    objc2_app_kit::NSApplication::sharedApplication(mtm).setActivationPolicy(objc2_app_kit::NSApplicationActivationPolicy::Prohibited);
    mac::personalise::render(dir, mtm)
}

/// The animation gallery: renders every island card (or only the named ones; `object::gallery`) natively, frame by
/// frame, into `dir/anims/` for `lab/animation-gallery/gallery.html`, and judges every frame (symmetry, clipping,
/// smoothness, rest); nothing appears on screen. Returns what failed, judge findings included.
#[cfg(target_os = "macos")]
pub fn render_gallery(dir: &std::path::Path, only: &[String], fps: f64) -> Vec<String> {
    mac::gallery::run(dir, only, fps)
}

/// Draws the Agent window in every run state and the island's questions offscreen into `dir` (nothing reaches the
/// screen), and drives their controls in-process (target/action, accessibility press, hit-testing, taps on the
/// island's click target). Returns what failed.
/// [chrome] Night 1022 stage 3: the menu's chrome (Agent caption, Debug Agent, the retry line, Support the Developer)
/// and the transcript's speaker naming, rendered and checked offscreen into `dir`. Returns the failed checks.
#[cfg(target_os = "macos")]
pub fn render_chrome(dir: &std::path::Path) -> Vec<String> {
    mac::chrome_render::render(dir)
}

/// The Engine section's models: the quick-pick, Add Model's every Splash model and precision, download, progress and
/// trash, rendered and checked offscreen into `dir`; returns what failed.
pub fn render_engine_models(dir: &std::path::Path) -> Vec<String> {
    mac::engine_render::render(dir)
}

/// The Engine section in every engine state, the Engine tile beside it, This Mac in Agent ▸ Model, and the menu's
/// foldable sections, rendered and checked offscreen into `dir`; returns what failed.
pub fn render_engine_section(dir: &std::path::Path) -> Vec<String> {
    mac::engine_section_render::render(dir)
}

/// The whole menu in every state the UX climb judges (lab/climb/menu-ux).
pub fn render_menu_climb(dir: &std::path::Path) -> Vec<String> {
    mac::menu_climb_render::render(dir)
}

/// The Decider section in every state (at rest, a pick waiting for Confirm, swapping, failed, a missing folder), folded,
/// and Setup's Decider step, rendered and checked offscreen into `dir`; returns what failed.
pub fn render_decider_section(dir: &std::path::Path) -> Vec<String> {
    mac::decider_section_render::render(dir)
}

/// Agent ▸ Model's click-then-confirm, the relay's models and the built-in engine's Add Provider, rendered and checked
/// offscreen into `dir`; returns what failed.
pub fn render_model_choice(dir: &std::path::Path) -> Vec<String> {
    let mut failures = mac::model_choice_render::render(dir);
    // Every Heimdall menu: only a deliberate exit (a window, panel, Quit) closes it on a click.
    if let Some(mtm) = objc2_foundation::MainThreadMarker::new() { failures.extend(mac::menu_live::audit(mtm)); }
    failures
}

/// Background Tasks ▸ Model: providers opening in place to their models and thinking levels, T3's instances, rendered
/// and checked offscreen into `dir`; returns what failed.
#[cfg(target_os = "macos")]
pub fn render_background_picker(dir: &std::path::Path) -> Vec<String> {
    mac::background_picker_render::render(dir)
}
/// Agent ▸ Model ▸ Claude Code subscription's account rows, rendered offscreen with their checks (`heimdall-ui-demo --relay-account`).
pub fn render_relay_account(dir: &std::path::Path) -> Vec<String> {
    mac::model_choice_render::render_accounts(dir)
}

/// The Claude relay's menu states (docs/claude-relay.md) rendered and checked offscreen into `dir`; returns what failed.
#[cfg(target_os = "macos")]
pub fn render_claude_relay(dir: &std::path::Path) -> Vec<String> {
    mac::relay_render::render(dir)
}

/// [menu lane, night 1027] The menu's rows under synthetic pointer events and a model added while the menu is open,
/// checked and rendered offscreen into `dir`. Returns the failed checks.
#[cfg(target_os = "macos")]
pub fn render_menu_rows(dir: &std::path::Path) -> Vec<String> {
    mac::menu_rows::render(dir)
}

/// [menu lane, night 1027] Times the open menu's sync with 30 apps, before and after the title cache, and a closed
/// menu's state change, offscreen; prints the numbers.
#[cfg(target_os = "macos")]
pub fn bench_menu_sync() -> Vec<String> {
    mac::menu_rows::bench()
}

/// Friends (personal edition) for the renders: `friends-submenu.png` (two friends, one revoked) and `friend-invite.png`
/// (the invite panel with `qr_png`, a QR code's PNG). Returns the files that failed.
#[cfg(target_os = "macos")]
pub fn render_friends(dir: &std::path::Path, qr_png: &std::path::Path) -> Vec<String> {
    let mtm = objc2_foundation::MainThreadMarker::new().expect("render_friends runs on the main thread");
    objc2_app_kit::NSApplication::sharedApplication(mtm).setActivationPolicy(objc2_app_kit::NSApplicationActivationPolicy::Prohibited);
    match std::fs::read(qr_png) {
        Ok(bytes) => mac::offscreen::friends(dir, &bytes, mtm),
        Err(e) => vec![format!("{}: {e}", qr_png.display())],
    }
}

/// The Bug Notes window offscreen with synthesized notes (open, fixed, closed, unknown class, evidence open; narrow,
/// default and wide), and a click on a card's evidence row. Returns the failed checks.
#[cfg(target_os = "macos")]
pub fn render_bug_notes(dir: &std::path::Path) -> Vec<String> {
    mac::bugs::render(dir)
}

#[cfg(target_os = "macos")]
pub fn render_agent(dir: &std::path::Path) -> Vec<String> {
    mac::agent_render::render(dir)
}

/// The island checked offscreen: stills of every card's states (`island-*.png` in `dir`), its windows' properties,
/// clicks through its real event path, and its cost at rest. Returns the failed checks.
/// The island chat's composer and ⌃⌘H checked offscreen (renders into `dir`). Returns the failed checks.
#[cfg(target_os = "macos")]
pub fn render_composer(dir: &std::path::Path) -> Vec<String> {
    mac::island::checks::render_composer(dir)
}

/// The island's looping content (thinking dots, the loading spinner) offscreen: stills at eight points of its cycle
/// from the model and from the layers' own animation, and the main thread's ticks and commits a second while it
/// loops on a live clock. Returns the failed checks.
#[cfg(target_os = "macos")]
pub fn render_island_loops(dir: &std::path::Path) -> Vec<String> {
    mac::island::loops::run(dir)
}

#[cfg(target_os = "macos")]
pub fn render_island(dir: &std::path::Path) -> Vec<String> {
    mac::island::checks::render(dir)
}

/// `heimdall-ui-demo --relay-activity <dir>`: the pulled-down conversation while the agent's turn works and when it
/// has ended, for a Pi turn and a relay turn (bug note 30 Oct 03:00), offscreen at real size. Returns the failures.
#[cfg(target_os = "macos")]
pub fn render_relay_activity(dir: &std::path::Path) -> Vec<String> {
    mac::island::checks::render_relay_activity(dir)
}

/// `heimdall-ui-demo --recovery-countdown <dir>`: the quota recovery card with its countdown (0:47, 0:00) and the same
/// card without one, offscreen at real size. Returns the failures.
#[cfg(target_os = "macos")]
pub fn render_recovery_countdown(dir: &std::path::Path) -> Vec<String> {
    mac::island::checks::render_recovery_countdown(dir)
}

/// `heimdall-ui-demo --catching-up <dir>`: Heimdall catching up (`HealthCue::Busy`): its arrival, the count falling
/// in place, its token at rest, a restart over it and cleared, offscreen at real size. Returns the failures.
#[cfg(target_os = "macos")]
pub fn render_catching_up(dir: &std::path::Path) -> Vec<String> {
    mac::island::checks::render_catching_up(dir)
}

/// Show Islands on Streams, in process: the switch over every island window, and a stand-in island window
/// captured by macOS's `screencapture` with it on (in the capture) and off (absent). Returns the failed checks.
#[cfg(target_os = "macos")]
pub fn render_streams(dir: &std::path::Path) -> Vec<String> {
    mac::streams::check(dir)
}

/// The real menu popped up for a few seconds (no events, the cursor never moves): a row's expansion recorded frame by
/// frame, and clicks inside the menu that must keep it open. Returns the failed checks.
#[cfg(target_os = "macos")]
pub fn render_menu_live(dir: &std::path::Path) -> Vec<String> {
    mac::menu_live::run(dir)
}

#[cfg(target_os = "macos")]
/// LIVE: the whole menu on screen a few seconds, its sections opened and folded by code, each change on real frames.
pub fn render_menu_sections_live(dir: &std::path::Path) -> Vec<String> {
    mac::menu_live::sections(dir)
}

/// Every submenu opens to the left, on the real menu popped up for about a second per scenario (no events, the
/// cursor never moves); screenshots of the nested levels into `dir`. Returns the failed checks.
#[cfg(target_os = "macos")]
pub fn render_submenus_left(dir: &std::path::Path) -> Vec<String> {
    mac::submenus_left::check::run(dir)
}

/// Drawing mode and pointing, in process: the overlay's window properties, strokes and keys through AppKit's event
/// path, Send by a tap through the overlay and through the island's click target (also while the island moves), a
/// Send end to end through a stand-in capture, and the light and marks on the island's clock. Nothing reaches the
/// screen. Returns the failed checks.
#[cfg(target_os = "macos")]
pub fn render_drawing(dir: &std::path::Path) -> Vec<String> {
    mac::draw::check::run(dir)
}

/// Drawing mode over an open menu, live on the main display for `seconds` (a lab check: brief, no pointer events);
/// `own` opens this process's own status menu first, otherwise another app's menu must be open. Prints each change.
#[cfg(target_os = "macos")]
pub fn drawing_over_menu(own: bool, seconds: f64) -> ! {
    mac::draw::check::live_over_menu(own, seconds)
}

/// The drop target at the notch, offscreen: synthetic pasteboards through a fake drag session, what the app
/// receives, the island's confirmation, rest, and the motion's contact sheet (`drop-sheet.png` in `dir`). Returns the
/// failed checks.
#[cfg(target_os = "macos")]
pub fn render_drop(dir: &std::path::Path) -> Vec<String> {
    mac::drop_target::check::run(dir)
}

/// A drop on the open pull-down, offscreen: through the click target, the catcher and the composer's field; the
/// attachment rendered into `dir`. Returns the failed checks.
#[cfg(target_os = "macos")]
pub fn render_pulldown_drop(dir: &std::path::Path) -> Vec<String> {
    mac::drop_target::check::run_pulldown(dir)
}

/// The real drop target over the notch for `seconds`, watching real drags from other apps; each drop printed, never
/// sent. Live checks only (a second drop target over the user's notch).
#[cfg(target_os = "macos")]
pub fn drop_live(seconds: f64) -> ! {
    mac::drop_target::check::live(seconds)
}

/// The conversation bench (`heimdall-ui-demo --bench-chat N`, `--bench-layout N`).
#[cfg(target_os = "macos")]
pub fn bench_chat(n: usize, layout_only: bool) -> String {
    if layout_only {
        mac::island::bench::layout_variants(n)
    } else {
        mac::island::bench::run(n)
    }
}

/// The visual judge alone: every island card played at 120 Hz and its frames judged (`dir/anims/judge.json`, with the
/// main thread's cost per frame). Returns a report and the number of findings.
#[cfg(target_os = "macos")]
pub fn render_motion(dir: &std::path::Path) -> (String, usize) {
    let mtm = objc2_foundation::MainThreadMarker::new().expect("render_motion runs on the main thread");
    objc2_app_kit::NSApplication::sharedApplication(mtm).setActivationPolicy(objc2_app_kit::NSApplicationActivationPolicy::Prohibited);
    let failures = mac::gallery::judge_all(dir, 120.0);
    let n = failures.len();
    (failures.iter().map(|f| format!("FAILED: {f}\n")).collect(), n)
}

/// A contact sheet of a simulated island sequence (`--seed N`, `--grid a,b,phase` or `--card name`): its key frames
/// on the user's desktop crop, labelled, offscreen. Returns the sheet's path or why not.
#[cfg(target_os = "macos")]
pub fn render_sheet(dir: &std::path::Path, what: &str, arg: &str) -> Result<std::path::PathBuf, String> {
    let mtm = objc2_foundation::MainThreadMarker::new().ok_or("render_sheet runs on the main thread")?;
    objc2_app_kit::NSApplication::sharedApplication(mtm).setActivationPolicy(objc2_app_kit::NSApplicationActivationPolicy::Prohibited);
    mac::gallery::sheet(dir, what, arg)
}

/// Lab-only hooks for `heimdall --lab-scenario` (lab/postinstall/). Main thread only; a normal launch never calls them.
#[cfg(target_os = "macos")]
pub mod lab {
    use std::path::Path;

    pub use crate::island::object::{Target, Token};

    /// Starts the island's frame log (JSONL); returns the island clock's zero on CLOCK_UPTIME_RAW.
    pub fn frame_log(path: &Path) -> Result<f64, String> {
        crate::mac::lab::frame_log(path)
    }
    pub fn flush_log() {
        crate::mac::lab::flush_log()
    }
    /// Both island windows ordered in at alpha 0 (`true`) or out again: the recorder's window-sharing set-up happens
    /// before the timeline instead of in its first motion.
    pub fn capture_prime(on: bool) {
        crate::mac::lab::capture_prime(on)
    }
    /// A tap on `target` through the island's click target window (AppKit dispatch, the island's own hit test).
    pub fn tap(target: &Target) -> bool {
        crate::mac::lab::tap(target)
    }
    pub fn targets() -> Vec<String> {
        crate::mac::lab::targets()
    }
    pub fn class() -> String {
        crate::mac::lab::class()
    }
    pub fn idle() -> bool {
        crate::mac::lab::idle()
    }
    pub fn animating() -> bool {
        crate::mac::lab::animating()
    }
    pub fn clock() -> String {
        crate::mac::lab::clock()
    }
    pub fn stats() -> (usize, usize, Vec<f64>) {
        crate::mac::lab::stats()
    }
    pub fn windows() -> Vec<(String, [f64; 4])> {
        crate::mac::lab::windows()
    }
    pub fn draw_open() -> Result<(), String> {
        crate::mac::lab::draw_open()
    }
    pub fn draw_stroke(points: &[(f64, f64)]) -> bool {
        crate::mac::lab::draw_stroke(points)
    }
    pub fn draw_strokes() -> usize {
        crate::mac::lab::draw_strokes()
    }
    pub fn draw_at_rest() -> bool {
        crate::mac::lab::draw_at_rest()
    }
}
