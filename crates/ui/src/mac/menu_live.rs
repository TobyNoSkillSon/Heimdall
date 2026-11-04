//! Live check (`heimdall-ui-demo --menu-live <dir>`): the real menu, built on the demo's state, pops up on the main
//! display for a few seconds while code drives it. No mouse or key events are posted and the cursor never moves:
//! submenus open through NSMenu's own implementation object (as `submenus_left::check` does), a row's disclosure is
//! pressed with `performClick:`, and a view row gets its `mouseDown:` handed an event made for it (never posted).
//!
//! - Expansion: Speaking's Phonon S row opens and closes its Voice row; every frame AppKit gives the submenu window
//!   (`setFrame:display:`) is recorded with its time, and the window is sampled at every display refresh, so the
//!   report says how many distinct sizes the change showed and over how long (`<dir>/expand.json`).
//! - Stays open: Agent ▸ Model ▸ Add Provider ▸ Built-in Splash Engine, then Providers ▸ Built-in Splash Engine ▸
//!   Remove, each pressed as a click: the menu and its submenus are still on screen afterwards, and the action ran.
//!
//! Returns the failed checks.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};

use std::time::Instant;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Bool, Sel};
use objc2::{msg_send, sel};
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy, NSButton, NSEvent, NSEventModifierFlags, NSEventType, NSMenu, NSMenuItem, NSScreen, NSView, NSWindow};
use objc2_foundation::{MainThreadMarker, NSPoint, NSRunLoop, NSRunLoopCommonModes, NSTimer};

use super::submenus_left::check::{composite, menu_windows};
thread_local! {
    static START: Cell<Option<Instant>> = const { Cell::new(None) };
    static RUN: RefCell<Option<Run>> = const { RefCell::new(None) };
}

fn now_ms() -> f64 {
    START.with(|s| s.get().map_or(0.0, |t| t.elapsed().as_secs_f64() * 1000.0))
}

/// Toggles a disclosure as a click does, without `performClick:`'s pressed-look delay.
fn toggle(button: &NSButton) {
    let on = button.state() == objc2_app_kit::NSControlStateValueOn;
    button.setState(if on { objc2_app_kit::NSControlStateValueOff } else { objc2_app_kit::NSControlStateValueOn });
    unsafe { button.sendAction_to(button.action(), button.target().as_deref()); }
}

/// One scripted step: returns true once done (the next step starts on the next tick).
type Step = Box<dyn FnMut(&mut Ctx) -> bool>;

struct Ctx {
    mtm: MainThreadMarker,
    root: Retained<NSMenu>,
    dir: PathBuf,
    /// When the current step began.
    since: f64,
    /// Menus opened by the script, the root first.
    open: Vec<Retained<NSMenu>>,
    /// Display-rate samples while recording: (ms, submenu window height).
    samples: Vec<(f64, f64)>,
    /// How long the press itself held the main thread.
    press_ms: f64,
    sampling: Option<Retained<NSWindow>>,
    report: Vec<String>,
    failures: Vec<String>,
}

struct Run {
    steps: Vec<Step>,
    at: usize,
    ctx: Ctx,
}

fn check(ctx: &mut Ctx, name: &str, ok: bool, detail: String) {
    println!("{} {name}{}", if ok { "ok  " } else { "FAIL" }, if ok { String::new() } else { format!(": {detail}") });
    if !ok { ctx.failures.push(format!("{name}: {detail}")); }
}

/// The submenu path (item indexes) from `root` to `target`.
fn path_to(root: &NSMenu, target: &NSMenu) -> Option<Vec<isize>> {
    if std::ptr::eq(root, target) { return Some(vec![]) }
    for (i, item) in root.itemArray().iter().enumerate() {
        if let Some(sub) = item.submenu() {
            if let Some(mut rest) = path_to(&sub, target) {
                rest.insert(0, i as isize);
                return Some(rest);
            }
        }
    }
    None
}

/// The first item anywhere under `root` whose action is `action`.
fn find_action(root: &NSMenu, action: Sel) -> Option<Retained<NSMenuItem>> {
    for item in root.itemArray().iter() {
        // Only what can be reached: a hidden row's submenu cannot open.
        if item.isHidden() { continue }
        if item.action() == Some(action) { return Some(item) }
        if let Some(sub) = item.submenu() {
            if let Some(found) = find_action(&sub, action) { return Some(found) }
        }
    }
    None
}

/// Opens the submenu at `index` of `menu`, as the pointer resting on its row does.
fn open_submenu(menu: &NSMenu, index: isize) {
    unsafe {
        let imp: Option<Retained<AnyObject>> = msg_send![menu, _menuImpl];
        let Some(imp) = imp else { return };
        let _: () = msg_send![&*imp, highlightItemAtIndex: index];
        let _: Bool = msg_send![&*imp, openSubmenuFromHighlightedItem];
    }
}

/// A step that opens the menus down to `target()` one level per step, waiting for each window (and its fade).
fn open_to(target: impl Fn() -> Option<Retained<NSMenu>> + 'static) -> Step {
    Box::new(move |ctx| {
        let windows = menu_windows(ctx.mtm);
        if ctx.open.is_empty() {
            if windows.is_empty() { return false }
            ctx.open.push(ctx.root.clone());
        }
        // Each opened level's window is up and has settled.
        if windows.len() < ctx.open.len() || now_ms() - ctx.since < 350.0 { return false }
        let Some(target) = target() else { check(ctx, "the scripted row exists", false, "not found".into()); return true };
        // Close what is not on the way (a level opened for an earlier step).
        let Some(path) = path_to(&ctx.root, &target) else { check(ctx, "the scripted menu is in the tree", false, String::new()); return true };
        let keep = ctx.open.iter().enumerate().take_while(|(depth, menu)| {
            *depth == 0 || path_to(&ctx.root, menu).is_some_and(|p| p.len() == *depth && path.starts_with(&p))
        }).count();
        ctx.open.truncate(keep.max(1));
        let depth = ctx.open.len() - 1;
        if depth == path.len() { return true }
        let parent = ctx.open[depth].clone();
        let Some(sub) = parent.itemAtIndex(path[depth]).and_then(|i| i.submenu()) else { return true };
        open_submenu(&parent, path[depth]);
        ctx.open.push(sub);
        ctx.since = now_ms();
        false
    })
}

fn wait(ms: f64) -> Step {
    Box::new(move |ctx| now_ms() - ctx.since >= ms)
}

/// The window showing `menu` (the one with the most recent `setFrame`, else the topmost submenu window).
fn window_of(menu: &NSMenu, mtm: MainThreadMarker) -> Option<Retained<NSWindow>> {
    let view = menu.itemArray().iter().find_map(|i| i.view());
    if let Some(w) = view.and_then(|v| v.window()) { return Some(w) }
    menu_windows(mtm).into_iter().last()
}

/// Presses `view` as a click does: its `mouseDown:` with an event made for its window at its centre (never posted).
fn click_view(view: &NSView) -> bool {
    let Some(window) = view.window() else { return false };
    let b = view.bounds();
    let centre = view.convertPoint_toView(NSPoint::new(b.size.width / 2.0, b.size.height / 2.0), None);
    let event = NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
        NSEventType::LeftMouseDown, centre, NSEventModifierFlags(0), 0.0, window.windowNumber(), None, 0, 1, 1.0);
    let Some(event) = event else { return false };
    let target = view.hitTest(view.convertPoint_toView(centre, None)).unwrap_or_else(|| objc2::Message::retain(view));
    unsafe { let _: () = msg_send![&*target, mouseDown: &*event]; }
    true
}

/// Records the change one press makes to `menu`'s window: its frames from AppKit and a sample every display refresh.
fn record_press(name: &'static str, menu: impl Fn() -> Option<Retained<NSMenu>> + 'static, press: impl Fn() + 'static) -> Vec<Step> {
    let menu = std::rc::Rc::new(menu);
    let m = menu.clone();
    vec![
        Box::new(move |ctx| {
            let window = m().and_then(|m| window_of(&m, ctx.mtm));
            ctx.samples.clear();
            ctx.sampling = window;
            START.with(|s| s.set(Some(Instant::now())));
            ctx.since = 0.0;
            press();
            ctx.press_ms = now_ms();
            true
        }),
        Box::new(move |ctx| {
            if now_ms() < 900.0 { return false }
            ctx.sampling = None;
            // Distinct heights the window showed, sampled at display rate, and when they changed.
            let mut changes: Vec<(f64, f64)> = vec![];
            for (t, h) in &ctx.samples {
                if changes.last().is_none_or(|(_, last)| (last - h).abs() > 0.25) { changes.push((*t, *h)); }
            }
            // The longest the main thread went without a tick (a display refresh is 8.3 ms at 120 Hz).
            let mut ticks: Vec<f64> = vec![0.0];
            ticks.extend(ctx.samples.iter().map(|(t, _)| *t));
            let gap = ticks.windows(2).map(|w| w[1] - w[0]).fold(0.0, f64::max);
            let span = match (changes.get(1), changes.last()) { (Some(a), Some(b)) => b.0 - a.0, _ => 0.0 };
            let line = format!(
                "{{\"press\":\"{name}\",\"samples\":{},\"distinct_heights\":{},\"changes\":[{}],\"change_span_ms\":{span:.1},\"press_ms\":{:.1},\"max_gap_ms\":{gap:.1}}}",
                ctx.samples.len(),
                changes.len(),
                changes.iter().map(|(t, h)| format!("[{t:.1},{h:.1}]")).collect::<Vec<_>>().join(","),
                ctx.press_ms,
            );
            println!("{line}");
            ctx.report.push(line);
            let shot = ctx.dir.join(format!("{name}.png"));
            composite(&shot, &menu_windows(ctx.mtm));
            ctx.since = now_ms();
            true
        }),
    ]
}

fn with<R>(f: impl FnOnce(&mut super::State) -> R) -> Option<R> {
    super::with_state_now(f)
}

/// One tick inside the menu's tracking loop (`menuLiveStep:`, every display refresh).
pub(crate) fn step() {
    let Some(mtm) = MainThreadMarker::new() else { return };
    let finished = RUN.with(|r| {
        let mut r = r.borrow_mut();
        let Some(run) = r.as_mut() else { return false };
        if let Some(w) = &run.ctx.sampling {
            let t = now_ms();
            run.ctx.samples.push((t, w.frame().size.height));
            WIDTHS.with(|v| v.borrow_mut().push(w.frame().size.width));
        }
        let _ = mtm;
        if run.at >= run.steps.len() { return true }
        if (run.steps[run.at])(&mut run.ctx) {
            if std::env::var_os("MENU_LIVE_TRACE").is_some() { eprintln!("step {} done at {:.0} ms, {} menu windows, {} opened", run.at + 1, now_ms(), menu_windows(mtm).len(), run.ctx.open.len()); }
            run.at += 1;
            run.ctx.since = now_ms();
        }
        run.at >= run.steps.len()
    });
    if finished {
        if let Some(menu) = RUN.with(|r| r.borrow().as_ref().map(|r| r.ctx.root.clone())) { menu.cancelTracking(); }
    }
}

fn pop_up(steps: Vec<Step>, root: Retained<NSMenu>, dir: &Path, controller: &AnyObject, mtm: MainThreadMarker) -> (Vec<String>, Vec<String>) {
    let ctx = Ctx { mtm, root: root.clone(), dir: dir.into(), since: 0.0, open: vec![], samples: vec![], press_ms: 0.0, sampling: None, report: vec![], failures: vec![] };
    START.with(|s| s.set(Some(Instant::now())));
    RUN.with(|r| *r.borrow_mut() = Some(Run { steps, at: 0, ctx }));
    let timer = unsafe { NSTimer::timerWithTimeInterval_target_selector_userInfo_repeats(1.0 / 240.0, controller, sel!(menuLiveStep:), None, true) };
    unsafe { NSRunLoop::currentRunLoop().addTimer_forMode(&timer, NSRunLoopCommonModes) };
    // Where the status item's menu hangs: under the menu bar at the main display's right edge. MENU_LIVE_DROP_PT lowers
    // the menu's top below the menu bar (default 8): a menu popped up right at the menu bar comes up scrolled, its scroll
    // arrows over the tiles (31 Oct), which the status item's menu never is. (A status item of the demo's own is no
    // help: with a full menu bar AppKit hides it and its menu opens at the screen's left edge, out of the recording.)
    let drop = std::env::var("MENU_LIVE_DROP_PT").ok().and_then(|v| v.parse::<f64>().ok()).unwrap_or(8.0);
    let anchor = NSScreen::mainScreen(mtm).map(|s| s.visibleFrame()).map(|v| NSPoint::new(v.origin.x + v.size.width - 316.0, v.origin.y + v.size.height - drop)).unwrap_or(NSPoint::new(800.0, 800.0));
    // Blocks in the menu's tracking loop until the last step cancels it.
    root.popUpMenuPositioningItem_atLocation_inView(None, anchor, None);
    timer.invalidate();
    let run = RUN.with(|r| r.borrow_mut().take());
    run.map(|r| {
        let mut failures = r.ctx.failures;
        if r.at < r.steps.len() { failures.push(format!("the menu closed at step {} of {}", r.at + 1, r.steps.len())); }
        (r.ctx.report, failures)
    }).unwrap_or_default()
}

pub(crate) fn run(dir: &Path) -> Vec<String> {
    let mtm = MainThreadMarker::new().expect("--menu-live runs on the main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    std::fs::create_dir_all(dir).ok();
    super::submenus_left::install();
    // MENU_LIVE_BEFORE: AppKit's own resize, for the before measurement.
    if std::env::var_os("MENU_LIVE_BEFORE").is_none() { super::menu_motion::install(); super::menu_group::install(); }
    let (a, b) = twice(|| expansion(dir, mtm));
    let (c, d) = twice(|| stays_open(dir, mtm));
    std::fs::write(dir.join("expand.json"), format!("[\n{}\n]\n", a.iter().chain(&c).cloned().collect::<Vec<_>>().join(",\n"))).ok();
    b.into_iter().chain(d).collect()
}

/// The demo menu in a state with Phonon's presets and the built-in engine not yet added, as the live state.
fn live_state(mtm: MainThreadMarker) -> (Retained<NSMenu>, Retained<AnyObject>, Option<super::State>) {
    let mut state = super::offscreen::demo_menu(mtm, |s| {
        s.speech_voice = crate::speech_voice::demo_speech_voice(false, crate::speech_voice::LocalVoice::Phonon, true);
        s.api = super::model_choice_render::providers();
        s.engine.state = crate::EngineState::Off;
        s.settings_value.engine_added = false;
        s.add_provider_choice.insert(super::menu::Picker::Agent, true);
        // One big section open at a time (climb 1): Personalisation, where the voice rows the expansion presses live.
        s.settings_value.collapsed = crate::model::MenuSection::ALL.iter().copied().filter(|x| *x != crate::model::MenuSection::Personalisation).collect();
    });
    state.on_action = Box::new(|_| {});
    let root = state.menu.clone();
    let controller: Retained<AnyObject> = Retained::into_super(Retained::into_super(state.controller.clone()));
    let previous = super::STATE.with(|s| s.replace(Some(state)));
    (root, controller, previous)
}

/// A step that opens `section` as its header's click does (the one open section the next steps need).
fn open_section(section: crate::model::MenuSection) -> Step {
    Box::new(move |ctx| {
        let open = with(|s| !s.settings_value.collapsed.contains(&section)).unwrap_or(false);
        if !open { super::check_row_clicked(super::menu::section_row(section)); }
        ctx.open.truncate(1);
        true
    })
}

fn expansion(dir: &Path, mtm: MainThreadMarker) -> (Vec<String>, Vec<String>) {
    let (root, controller, previous) = live_state(mtm);
    let mut failures = vec![];
    let mut report = vec![];

    // 1. Phonon S opens its Voice row, then closes it.
    let phonon = || with(|s| s.speaking_rows.phonon_disclosure()).flatten();
    let phonon_menu = move || phonon().and_then(|b| b.enclosingMenuItem()).and_then(|i| unsafe { i.menu() });
    let mut steps: Vec<Step> = vec![open_to(phonon_menu), wait(300.0)];
    // MENU_LIVE_CYCLES: more rounds, for a screen recording of the motion.
    let cycles = std::env::var("MENU_LIVE_CYCLES").ok().and_then(|c| c.parse().ok()).unwrap_or(1);
    for _ in 0..cycles {
        steps.extend(record_press("phonon-expand", phonon_menu, move || { if let Some(b) = phonon() { toggle(&b); } }));
        steps.push(wait(200.0));
        steps.extend(record_press("phonon-collapse", phonon_menu, move || { if let Some(b) = phonon() { toggle(&b); } }));
        steps.push(wait(200.0));
    }
    let (r, f) = pop_up(steps, root.clone(), dir, &controller, mtm);
    report.extend(r);
    failures.extend(f);
    super::STATE.with(|s| *s.borrow_mut() = previous);
    (report, failures)
}

/// Runs a scenario, once more if the menu was closed from outside before it finished (a click elsewhere on the
/// screen ends any menu's tracking).
fn twice(scenario: impl Fn() -> (Vec<String>, Vec<String>)) -> (Vec<String>, Vec<String>) {
    let first = scenario();
    if !first.1.iter().any(|f| f.starts_with("the menu closed")) { return first }
    eprintln!("the menu closed before the script ended; once more");
    scenario()
}

fn stays_open(dir: &Path, mtm: MainThreadMarker) -> (Vec<String>, Vec<String>) {
    let (root, controller, previous) = live_state(mtm);
    let mut report = vec![];
    let mut failures = vec![];
    // 2. Add Provider ▸ Built-in Splash Engine, then Providers ▸ Built-in Splash Engine ▸ Remove: the menu stays open.
    // Agent ▸ Model as it is on screen (the menu the live relay updates, `menu::relay_live_agent_models`).
    let agent = |s: &super::State| s.api_rows.models.first().map(|(b, _, _)| b).or(s.api_rows.local.as_ref())
        .and_then(|b| b.enclosingMenuItem()).and_then(|i| unsafe { i.menu() })
        .or_else(|| s.api_item.as_ref().and_then(|i| i.submenu()));
    let add_item = move || with(|s| agent(s).and_then(|m| find_action(&m, sel!(addBuiltInEngine:)))).flatten();
    let remove_item = move || with(|s| agent(s).and_then(|m| find_action(&m, sel!(removeBuiltInEngine:)))).flatten();
    let speaking = |s: &super::State| s.speaking_rows.phonon_disclosure().and_then(|b| b.enclosingMenuItem()).and_then(|i| unsafe { i.menu() });
    let refresh_item = move || with(|s| speaking(s).and_then(|m| find_action(&m, sel!(speechRefresh:)))).flatten();
    let disconnect_item = move || with(|s| speaking(s).and_then(|m| find_action(&m, sel!(speechDisconnect:)))).flatten();
    let press = |item: Option<Retained<NSMenuItem>>| -> bool { item.and_then(|i| i.view()).is_some_and(|v| click_view(&v)) };
    let steps: Vec<Step> = vec![
        open_section(crate::model::MenuSection::Agent),
        wait(300.0),
        open_to(move || add_item().and_then(|i| unsafe { i.menu() })),
        Box::new(move |ctx| {
            let item = add_item();
            let ok = press(item);
            check(ctx, "Add Provider ▸ Built-in Splash Engine is a row that takes the click itself", ok, "a native item: AppKit closes the menu on its click".into());
            true
        }),
        wait(500.0),
        Box::new(move |ctx| {
            let up = menu_windows(ctx.mtm).len();
            let added = with(|s| s.settings_value.engine_added).unwrap_or(false);
            check(ctx, "after Built-in Splash Engine: the menu and Agent ▸ Model are still open, the engine added", up >= 2 && added, format!("{up} menu windows, added {added}"));
            composite(&ctx.dir.join("after-add.png"), &menu_windows(ctx.mtm));
            ctx.open.truncate(1);
            true
        }),
        open_to(move || remove_item().and_then(|i| unsafe { i.menu() })),
        Box::new(move |ctx| {
            let ok = press(remove_item());
            check(ctx, "Providers ▸ Built-in Splash Engine ▸ Remove is a row that takes the click itself", ok, "a native item".into());
            true
        }),
        wait(500.0),
        Box::new(move |ctx| {
            let up = menu_windows(ctx.mtm).len();
            let added = with(|s| s.settings_value.engine_added).unwrap_or(true);
            check(ctx, "after Remove: the menu is still open, the engine removed", up >= 2 && !added, format!("{up} menu windows, added {added}"));
            composite(&ctx.dir.join("after-remove.png"), &menu_windows(ctx.mtm));
            ctx.open.truncate(1);
            true
        }),
        // 3. Speaking ▸ Model ▸ ElevenLabs ▸ Refresh Voices, then Disconnect: the submenus stay open (Refresh), the menu
        // stays open and the picker drops ElevenLabs (Disconnect).
        open_section(crate::model::MenuSection::Personalisation),
        wait(300.0),
        open_to(move || refresh_item().and_then(|i| unsafe { i.menu() })),
        Box::new(move |ctx| {
            ctx.since = menu_windows(ctx.mtm).len() as f64;
            let ok = press(refresh_item());
            check(ctx, "ElevenLabs ▸ Refresh Voices is a row that takes the click itself", ok, "a native item".into());
            true
        }),
        wait(500.0),
        Box::new(move |ctx| {
            let up = menu_windows(ctx.mtm).len();
            check(ctx, "after Refresh Voices: the menu and both submenus are still open", up >= 3, format!("{up} menu windows"));
            true
        }),
        Box::new(move |ctx| {
            let ok = press(disconnect_item());
            check(ctx, "ElevenLabs ▸ Disconnect is a row that takes the click itself", ok, "a native item".into());
            true
        }),
        wait(500.0),
        Box::new(move |ctx| {
            let up = menu_windows(ctx.mtm).len();
            let connected = with(|s| s.speech_voice.elevenlabs.connected).unwrap_or(true);
            // ElevenLabs is no longer offered: disconnected, it leaves the open picker's Providers.
            let listed = disconnect_item().is_some();
            check(ctx, "after Disconnect: the menu is still open, ElevenLabs gone from Providers in place", up >= 2 && !connected && !listed, format!("{up} menu windows, connected {connected}, still listed {listed}"));
            composite(&ctx.dir.join("after-disconnect.png"), &menu_windows(ctx.mtm));
            true
        }),
    ];
    let (r, f) = pop_up(steps, root, dir, &controller, mtm);
    report.extend(r);
    failures.extend(f);
    super::STATE.with(|s| *s.borrow_mut() = previous);
    (report, failures)
}


/// Actions that deliberately leave the menu: each opens a window, panel, the browser or Finder, quits, or (Shortcuts ▸)
/// does what its global key does. Every other click inside a Heimdall menu keeps it open.
pub(crate) const LEAVING: &[&str] = &[
    "connectProvider:", "inviteFriend:", "openAgent:", "openAgentModels:", "openBugNotes:", "openHomeFolder:",
    "openModelLicence:", "openSpeakers:", "openTranscript:", "quit:", "showArchive:", "speechConnect:",
    "speechOwnConnect:", "supportDeveloper:", "runShortcut:", "updateRow:",
];

/// Items under `menu` a click would close the menu on: native rows (no view of their own) with an action, enabled,
/// that open no submenu, as (title, action).
pub(crate) fn closing_items(menu: &NSMenu) -> Vec<(String, String)> {
    let mut out = vec![];
    for item in menu.itemArray().iter() {
        if let Some(sub) = item.submenu() {
            out.extend(closing_items(&sub));
            continue;
        }
        let Some(action) = item.action() else { continue };
        if item.view().is_some() || item.isSeparatorItem() { continue }
        out.push((item.title().to_string(), action.name().to_str().unwrap_or("").to_string()));
    }
    out
}

/// Offscreen: in the demo menu with every provider and voice row present, only `LEAVING` actions close the menu, and
/// the in-place rows send their action with the menu left open. Returns the failed checks.
pub(crate) fn audit(mtm: MainThreadMarker) -> Vec<String> {
    let mut failures = vec![];
    for engine_added in [false, true] {
        let state = super::offscreen::demo_menu(mtm, |s| {
            s.speech_voice = crate::speech_voice::demo_speech_voice(false, crate::speech_voice::LocalVoice::Phonon, true);
            s.api = super::model_choice_render::providers();
            s.engine.state = crate::EngineState::Off;
            s.settings_value.engine_added = engine_added;
            s.add_provider_choice.insert(super::menu::Picker::Agent, true);
        });
        let closing: Vec<_> = closing_items(&state.menu).into_iter().filter(|(_, a)| !LEAVING.contains(&a.as_str())).collect();
        let ok = closing.is_empty();
        println!("{} engine added {engine_added}: no click inside the menu closes it but a deliberate exit{}", if ok { "ok  " } else { "FAIL" }, if ok { String::new() } else { format!(": {closing:?}") });
        if !ok { failures.push(format!("closing items: {closing:?}")) }
        // The rows the user named are in-place rows.
        let action = if engine_added { sel!(removeBuiltInEngine:) } else { sel!(addBuiltInEngine:) };
        let row = find_action(&state.menu, action).and_then(|i| i.view()).and_then(|v| v.downcast::<super::prominent::ProminentRow>().ok());
        let ok = row.is_some_and(|r| r.is_in_place());
        println!("{} {} keeps the menu open", if ok { "ok  " } else { "FAIL" }, if engine_added { "Remove" } else { "Add Provider ▸ Built-in Splash Engine" });
        if !ok { failures.push(format!("{action:?} is not an in-place row")) }
    }
    failures
}

/// Live (`heimdall-ui-demo --menu-sections-live <dir>`): the whole menu on the user's setup (`menu_climb_render::owner`,
/// Agent open) pops up on the main display, and its section headers are pressed by code as a click does
/// (`check_row_clicked`), each change recorded on the root menu window at every display refresh: its width and height,
/// how long the press held the main thread, the longest gap between ticks. opening Background Tasks
/// lags "because it gets longer and also wider"; the width must never change. Writes `<dir>/sections.json`.
pub(crate) fn sections(dir: &Path) -> Vec<String> {
    let mtm = MainThreadMarker::new().expect("--menu-sections-live runs on the main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    std::fs::create_dir_all(dir).ok();
    super::submenus_left::install();
    if std::env::var_os("MENU_LIVE_BEFORE").is_none() { super::menu_motion::install(); super::menu_group::install(); }
    let (report, failures) = twice(|| {
        let mut state = super::offscreen::demo_menu(mtm, |s| {
            super::menu_climb_render::owner(s);
            s.settings_value.collapsed = crate::model::MenuSection::ALL.iter().copied().filter(|x| *x != crate::model::MenuSection::Agent).collect();
        });
        state.on_action = app_echo(state.settings_value.clone());
        let root = state.menu.clone();
        let controller: Retained<AnyObject> = Retained::into_super(Retained::into_super(state.controller.clone()));
        let previous = super::STATE.with(|s| s.replace(Some(state)));
        use crate::model::MenuSection::*;
        // MENU_LIVE_PRESS=click: a header pressed as a real click is, a mouse-down and a mouse-up posted to this app's
        // own event queue at the header's centre (never a system event, the cursor untouched), so the menu's tracking
        // sees the mouse-up wherever the rows have moved by then; otherwise `check_row_clicked`, as accessibility does.
        let clicks = std::env::var("MENU_LIVE_PRESS").is_ok_and(|p| p == "click");
        let header = move |section: crate::model::MenuSection| move || {
            if clicks { click_header(section) } else { super::check_row_clicked(super::menu::section_row(section)) }
        };
        let mut steps: Vec<Step> = vec![Box::new(|ctx: &mut Ctx| { if menu_windows(ctx.mtm).is_empty() { return false } ctx.open.push(ctx.root.clone()); true }), wait_for_go(), wait(400.0)];
        // Every section opened from another open section (the app echoes each fold), folded, and opened from all folded.
        for (name, section) in [("open-background", Background), ("fold-background", Background), ("open-engine", Engine),
            ("open-decider", Decider), ("fold-decider", Decider), ("open-personalisation", Personalisation), ("open-agent", Agent),
            ("fold-agent", Agent), ("reopen-background", Background), ("switch-engine", Engine), ("fold-engine", Engine),
            ("reopen-personalisation", Personalisation), ("fold-personalisation", Personalisation), ("reopen-decider", Decider),
            ("reopen-agent", Agent)] {
            steps.extend(record_root(name, header(section)));
            steps.push(wait(250.0));
        }
        let out = pop_up(steps, root, dir, &controller, mtm);
        super::STATE.with(|s| *s.borrow_mut() = previous);
        out
    });
    std::fs::write(dir.join("sections.json"), format!("[\n{}\n]\n", report.join(",\n"))).ok();
    failures
}

/// As `record_press`, on the root menu's window, its width with its height.
fn record_root(name: &'static str, press: impl Fn() + 'static) -> Vec<Step> {
    vec![
        Box::new(move |ctx| {
            ctx.sampling = menu_windows(ctx.mtm).into_iter().next();
            ctx.samples.clear();
            WIDTHS.with(|w| w.borrow_mut().clear());
            START.with(|s| s.set(Some(Instant::now())));
            ctx.since = 0.0;
            PRESS_UPTIME.with(|p| p.set(uptime()));
            press();
            ctx.press_ms = now_ms();
            true
        }),
        Box::new(move |ctx| {
            if now_ms() < 700.0 { return false }
            ctx.sampling = None;
            let widths = WIDTHS.with(|w| w.borrow().clone());
            let mut changes: Vec<(f64, f64)> = vec![];
            for (t, h) in &ctx.samples {
                if changes.last().is_none_or(|(_, last)| (last - h).abs() > 0.25) { changes.push((*t, *h)); }
            }
            let mut ticks: Vec<f64> = vec![0.0];
            ticks.extend(ctx.samples.iter().map(|(t, _)| *t));
            let gap = ticks.windows(2).map(|w| w[1] - w[0]).fold(0.0, f64::max);
            let span = match (changes.get(1), changes.last()) { (Some(a), Some(b)) => b.0 - a.0, _ => 0.0 };
            let (wmin, wmax) = widths.iter().fold((f64::MAX, 0.0f64), |(a, b), w| (a.min(*w), b.max(*w)));
            let line = format!(
                "{{\"press\":\"{name}\",\"uptime\":{:.6},\"samples\":{},\"distinct_heights\":{},\"from\":{:.1},\"to\":{:.1},\"change_span_ms\":{span:.1},\"press_ms\":{:.1},\"max_gap_ms\":{gap:.1},\"width_min\":{wmin:.1},\"width_max\":{wmax:.1},\"changes\":[{}]}}",
                PRESS_UPTIME.with(|p| p.get()), ctx.samples.len(), changes.len(), changes.first().map_or(0.0, |c| c.1), changes.last().map_or(0.0, |c| c.1), ctx.press_ms,
                changes.iter().map(|(t, h)| format!("[{t:.1},{h:.1}]")).collect::<Vec<_>>().join(","),
            );
            println!("{line}");
            ctx.report.push(line);
            composite(&ctx.dir.join(format!("{name}.png")), &menu_windows(ctx.mtm));
            ctx.since = now_ms();
            true
        }),
    ]
}

thread_local! {
    /// The root window's width at each tick while `record_root` samples.
    static WIDTHS: RefCell<Vec<f64>> = const { RefCell::new(Vec::new()) };
    /// When the current press began, on CLOCK_UPTIME_RAW (the frame recorder's clock).
    static PRESS_UPTIME: Cell<f64> = const { Cell::new(0.0) };
}

/// The menu window's view tree (MENU_LIVE_DUMP): class, frame, layer, and the item a view belongs to.
fn dump_views(view: &NSView, depth: usize, out: &mut String) {
    let f = view.frame();
    let layer: bool = view.wantsLayer() || view.layer().is_some();
    let flipped = view.isFlipped();
    let hidden = view.isHidden();
    let item: Option<Retained<NSMenuItem>> = if objc2_foundation::NSObjectProtocol::respondsToSelector(view, sel!(menuItem)) { unsafe { msg_send![view, menuItem] } } else { None };
    out.push_str(&format!("{}{} ({:.1},{:.1} {:.1}x{:.1}) layer={layer} flipped={flipped}{}{}\n", "  ".repeat(depth), view.class().name().to_string_lossy(),
        f.origin.x, f.origin.y, f.size.width, f.size.height, if hidden { " HIDDEN" } else { "" }, item.map(|i| format!(" item=\"{}\"", i.title())).unwrap_or_default()));
    for sub in view.subviews().iter() { dump_views(&sub, depth + 1, out); }
}

fn uptime() -> f64 {
    let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
    unsafe { libc::clock_gettime(libc::CLOCK_UPTIME_RAW, &mut ts) };
    ts.tv_sec as f64 + ts.tv_nsec as f64 / 1e9
}

/// MENU_LIVE_GO=<file>: the presses wait (up to 20 s) for the frame recorder to create the file once it captures. The
/// root menu window's frame goes first to `<file>.frame` (x, top from the main display's top, width, height, in points),
/// where the recorder finds which strip of the display to capture.
fn wait_for_go() -> Step {
    let go = std::env::var_os("MENU_LIVE_GO").map(PathBuf::from);
    let mut told = false;
    Box::new(move |ctx| {
        let Some(go) = &go else { return true };
        if !told {
            let Some(window) = menu_windows(ctx.mtm).into_iter().next() else { return false };
            let f = window.frame();
            let top = NSScreen::screens(ctx.mtm).firstObject().map_or(0.0, |s| s.frame().size.height) - (f.origin.y + f.size.height);
            let mut name = go.clone().into_os_string();
            name.push(".frame");
            std::fs::write(PathBuf::from(name), format!("{} {} {} {}", f.origin.x, top, f.size.width, f.size.height)).ok();
            if std::env::var_os("MENU_LIVE_DUMP").is_some() {
                let mut out = String::new();
                if let Some(frame_view) = window.contentView().and_then(|c| unsafe { c.superview() }) { dump_views(&frame_view, 0, &mut out); }
                std::fs::write(ctx.dir.join("views.txt"), out).ok();
            }
            told = true;
        }
        go.exists() || now_ms() - ctx.since > 20_000.0
    })
}

/// The app's side of a header press (crates/app pipeline/actions.rs), so the live menu meets the echoes the app sends:
/// each `SetSectionsCollapsed` saved and the whole settings published back after the save (MENU_ECHO_MS, default 2 ms)
/// from another thread through the app's own update path (`Ui::send`, which coalesces only what is still pending).
/// MENU_NO_ECHO: none. (The before build's harness echoes main cc9afdc2's one action per section the same way.)
fn app_echo(base: crate::model::Settings) -> Box<dyn Fn(crate::model::UiAction)> {
    use crate::model::UiAction;
    if std::env::var_os("MENU_NO_ECHO").is_some() { return Box::new(|_| {}) }
    let delay = std::time::Duration::from_secs_f64(std::env::var("MENU_ECHO_MS").ok().and_then(|v| v.parse::<f64>().ok()).unwrap_or(2.0) / 1000.0);
    let (tx, rx) = std::sync::mpsc::channel::<UiAction>();
    std::thread::spawn(move || {
        for action in rx {
            let UiAction::SetSectionsCollapsed(list) = action else { continue };
            std::thread::sleep(delay);
            let mut settings = base.clone();
            settings.collapsed = list;
            crate::Ui::handle().send(crate::model::UiUpdate::Settings(settings));
        }
    });
    Box::new(move |action| { let _ = tx.send(action); })
}

/// A header's click as the menu's tracking meets it: a mouse-down at the row's centre, the mouse-up 90 ms later at the
/// same place on screen, both posted to this app's own queue (no system event; the cursor never moves).
fn click_header(section: crate::model::MenuSection) {
    let Some(row) = with(|s| s.sections.get(&section).map(|(h, _)| h.clone())).flatten() else { return };
    let Some(window) = row.window() else { return };
    let b = row.bounds();
    let at = row.convertPoint_toView(NSPoint::new(b.size.width / 2.0, b.size.height / 2.0), None);
    let screen = window.convertPointToScreen(at);
    let post = move |kind: NSEventType, window: &NSWindow| {
        let local = window.convertPointFromScreen(screen);
        let event = NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
            kind, local, NSEventModifierFlags(0), objc2_foundation::NSProcessInfo::processInfo().systemUptime(), window.windowNumber(), None, 0, 1, 1.0);
        if let (Some(event), Some(mtm)) = (event, MainThreadMarker::new()) { NSApplication::sharedApplication(mtm).postEvent_atStart(&event, false); }
    };
    post(NSEventType::LeftMouseDown, &window);
    let window = window.clone();
    let up = block2::RcBlock::new(move |_: std::ptr::NonNull<NSTimer>| post(NSEventType::LeftMouseUp, &window));
    let timer = unsafe { NSTimer::timerWithTimeInterval_repeats_block(0.09, false, &up) };
    unsafe { NSRunLoop::currentRunLoop().addTimer_forMode(&timer, NSRunLoopCommonModes) };
}
