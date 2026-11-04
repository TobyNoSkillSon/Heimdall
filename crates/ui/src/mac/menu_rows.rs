//! [menu lane, night 1027] The menu's rows under synthetic pointer events, offscreen (`heimdall-ui-demo --menu-rows
//! <dir>`; nothing reaches the screen, the pointer or the menu bar). The rows sit in a window that is never ordered
//! in, and get the events AppKit would send them (enter, press, drag, release) with made-up locations:
//! - `a_row_left_mid_drag_drops_its_highlight`: a toggle row (and the status line) pressed and dragged out of drops
//!   its highlight, and gets it back when dragged in (audit item 12).
//! - `prominent_row_highlight_needs_the_pointer`: Support the Developer is lit only under the pointer, not after its
//!   press closed the menu, nor past `menuDidClose:` (item 13).
//! - `cloud_submenu_gains_a_model_added_while_open`: a model added while the menu is open is in Agent ▸ Model's Cloud
//!   and in Background Tasks ▸ Model at once; Providers keeps its items (item 14).
//! - `bug_notes_row_only_in_the_personal_menu`: the built menu has the Bug Notes row in the personal edition and
//!   none in the public one (docs/bugs.md).
//! Each state is written as a PNG at 2×. `--menu-bench` times the menu's sync with 30 apps (audit, menu sync cost).

use std::path::Path;

use objc2::rc::Retained;
use objc2::{msg_send, MainThreadOnly as _};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSButton, NSEvent, NSEventModifierFlags, NSEventType, NSMenu, NSMenuItem,
    NSTrackingAreaOptions, NSView, NSWindow, NSWindowStyleMask,
};
use objc2_foundation::{MainThreadMarker, NSPoint};

use super::controls::{CheckRow, LabelRow};
use super::menu::{CLOUD, SUPPORT_TITLE};
use super::offscreen::{capture_rounded, stacked, MENU_WIDTH};
use super::prominent::ProminentRow;
use super::util::{menu_item, rect};
use crate::model::*;

type Check<'a> = dyn FnMut(&str, bool, String) + 'a;

pub(crate) fn render(dir: &Path) -> Vec<String> {
    let mtm = MainThreadMarker::new().expect("the menu row checks run on the main thread");
    NSApplication::sharedApplication(mtm).setActivationPolicy(NSApplicationActivationPolicy::Prohibited);
    std::fs::create_dir_all(dir).ok();
    let mut failures = vec![];
    let mut check = |name: &str, ok: bool, detail: String| {
        eprintln!("check {} {name}{}", if ok { "ok" } else { "FAILED" }, if ok { String::new() } else { format!(": {detail}") });
        if !ok { failures.push(format!("{name}: {detail}")); }
    };
    drag_out(dir, mtm, &mut check);
    prominent(dir, mtm, &mut check);
    cloud(dir, mtm, &mut check);
    bug_notes_row(mtm, &mut check);
    failures
}

/// The menu's last group (Launch at Login, Support the Developer, Quit) laid out as the menu shows it, in a window
/// that is never ordered in: (window, Launch at Login, Support).
fn last_group(mtm: MainThreadMarker) -> (Retained<NSWindow>, Retained<NSView>, Retained<CheckRow>, Retained<ProminentRow>) {
    let menu = NSMenu::new(mtm);
    let login = menu_item("Launch at Login", Some("power.circle"), None, "", None, mtm);
    // Row 9999 toggles nothing: the press reaches the row, no setting.
    let row = CheckRow::new(9999, "Launch at Login", Some("power.circle"), true, mtm);
    login.setView(Some(&row));
    menu.addItem(&login);
    let support = menu_item(SUPPORT_TITLE, Some("heart"), None, "", None, mtm);
    let prominent = ProminentRow::new(SUPPORT_TITLE, Some("heart"), mtm);
    support.setView(Some(&prominent));
    menu.addItem(&support);
    menu.addItem(&menu_item("Quit Heimdall", Some("power"), None, "q", None, mtm));
    let container = stacked(&menu, MENU_WIDTH, (5.0, 5.0), true, mtm);
    let window = offscreen_window(&container, mtm);
    (window, container, row, prominent)
}

fn offscreen_window(content: &NSView, mtm: MainThreadMarker) -> Retained<NSWindow> {
    let frame = content.frame();
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(NSWindow::alloc(mtm), rect(-20_000.0, -20_000.0, frame.size.width, frame.size.height),
            NSWindowStyleMask::Borderless, NSBackingStoreType::Buffered, false)
    };
    unsafe { window.setReleasedWhenClosed(false) };
    window.setContentView(Some(content));
    window
}

/// A pointer event at `at` (window coordinates) as AppKit would deliver it to the row.
fn event(kind: NSEventType, at: NSPoint, window: &NSWindow) -> Retained<NSEvent> {
    let time = objc2_foundation::NSProcessInfo::processInfo().systemUptime();
    let e = if matches!(kind, NSEventType::MouseEntered | NSEventType::MouseExited) {
        unsafe {
            NSEvent::enterExitEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_trackingNumber_userData(
                kind, at, NSEventModifierFlags(0), time, window.windowNumber(), None, 0, 0, std::ptr::null_mut())
        }
    } else {
        NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
            kind, at, NSEventModifierFlags(0), time, window.windowNumber(), None, 0, 1, 1.0)
    };
    e.expect("a synthetic event")
}

/// Sends `kind` at `at` to `view` through its responder method, as the window would.
fn send(view: &NSView, window: &NSWindow, kind: NSEventType, at: NSPoint) {
    let e = event(kind, at, window);
    unsafe {
        match kind {
            NSEventType::MouseEntered => { let _: () = msg_send![view, mouseEntered: &*e]; }
            NSEventType::MouseExited => { let _: () = msg_send![view, mouseExited: &*e]; }
            NSEventType::LeftMouseDown => { let _: () = msg_send![view, mouseDown: &*e]; }
            NSEventType::LeftMouseDragged => { let _: () = msg_send![view, mouseDragged: &*e]; }
            NSEventType::LeftMouseUp => { let _: () = msg_send![view, mouseUp: &*e]; }
            _ => {}
        }
    }
}

/// `view`'s centre and a point 40 pt below it (outside the 24-pt row), in window coordinates.
fn inside_outside(view: &NSView) -> (NSPoint, NSPoint) {
    let b = view.bounds();
    let centre = view.convertPoint_toView(NSPoint::new(b.size.width / 2.0, b.size.height / 2.0), None);
    (centre, NSPoint::new(centre.x, centre.y - 40.0))
}

fn shot(dir: &Path, name: &str, view: &NSView, check: &mut Check) {
    view.setNeedsDisplay(true);
    let path = dir.join(format!("{name}.png"));
    let ok = capture_rounded(&path, view, true);
    check(&format!("rendered {}", path.display()), ok, "capture failed".into());
}

fn drag_out(dir: &Path, mtm: MainThreadMarker, check: &mut Check) {
    const NAME: &str = "a_row_left_mid_drag_drops_its_highlight";
    let (window, container, row, _) = last_group(mtm);
    let (inside, outside) = inside_outside(&row);
    unsafe { let _: () = msg_send![&*row, updateTrackingAreas]; }
    let during_drag = row.trackingAreas().iter().any(|a| a.options().contains(NSTrackingAreaOptions::EnabledDuringMouseDrag));
    check(&format!("{NAME}: the row's hover is tracked while the button is held"), during_drag, "no EnabledDuringMouseDrag".into());
    send(&row, &window, NSEventType::MouseEntered, inside);
    send(&row, &window, NSEventType::LeftMouseDown, inside);
    check(&format!("{NAME}: pressed under the pointer, lit"), row.highlighted(), "not lit".into());
    shot(dir, "menu-row-held", &container, check);
    send(&row, &window, NSEventType::LeftMouseDragged, outside);
    check(&format!("{NAME}: dragged out with the button held, not lit"), !row.highlighted(), "still lit".into());
    shot(dir, "menu-row-dragged-out", &container, check);
    send(&row, &window, NSEventType::LeftMouseDragged, inside);
    check(&format!("{NAME}: dragged back in, lit again"), row.highlighted(), "not lit".into());
    send(&row, &window, NSEventType::LeftMouseDragged, outside);
    send(&row, &window, NSEventType::LeftMouseUp, outside);
    check(&format!("{NAME}: released outside, not lit"), !row.highlighted(), "lit".into());

    // The status line under the tiles, when a click there opens Setup, follows the same rule.
    let line = LabelRow::new(16.0, mtm);
    line.set("Permissions needed — Set Up…", true, true);
    line.setFrame(rect(0.0, 0.0, MENU_WIDTH, 24.0));
    let holder = NSView::initWithFrame(NSView::alloc(mtm), rect(0.0, 0.0, MENU_WIDTH, 24.0));
    holder.addSubview(&line);
    let window = offscreen_window(&holder, mtm);
    let (inside, outside) = inside_outside(&line);
    send(&line, &window, NSEventType::MouseEntered, inside);
    send(&line, &window, NSEventType::LeftMouseDragged, inside);
    check(&format!("{NAME}: the status line lit under the pointer"), line.highlighted(), "not lit".into());
    send(&line, &window, NSEventType::LeftMouseDragged, outside);
    check(&format!("{NAME}: the status line dragged out of, not lit"), !line.highlighted(), "still lit".into());
    window.close();
}

fn prominent(dir: &Path, mtm: MainThreadMarker, check: &mut Check) {
    const NAME: &str = "prominent_row_highlight_needs_the_pointer";
    let (window, container, _, row) = last_group(mtm);
    let (inside, outside) = inside_outside(&row);
    check(&format!("{NAME}: at rest, not lit"), !row.highlighted(), "lit".into());
    send(&row, &window, NSEventType::MouseEntered, inside);
    check(&format!("{NAME}: under the pointer, lit"), row.highlighted(), "not lit".into());
    shot(dir, "menu-support-pointer", &container, check);
    send(&row, &window, NSEventType::LeftMouseDragged, outside);
    check(&format!("{NAME}: the pointer carried off it with no exit, not lit"), !row.highlighted(), "still lit".into());
    // The press closes the menu and sends the action: no exit follows it.
    send(&row, &window, NSEventType::MouseEntered, inside);
    send(&row, &window, NSEventType::LeftMouseDown, inside);
    check(&format!("{NAME}: after its press, not lit"), !row.highlighted(), "still lit".into());
    shot(dir, "menu-support-after-press", &container, check);
    window.close();

    // In a menu: a hover the menu's close cut off does not survive into the next open.
    let menu = NSMenu::new(mtm);
    let item = menu_item(SUPPORT_TITLE, Some("heart"), None, "", None, mtm);
    let row = ProminentRow::new(SUPPORT_TITLE, Some("heart"), mtm);
    item.setView(Some(&row));
    menu.addItem(&item);
    let e = event(NSEventType::MouseEntered, NSPoint::new(10.0, 10.0), &window);
    unsafe { let _: () = msg_send![&*row, mouseEntered: &*e]; }
    check(&format!("{NAME}: hovered in its menu, lit"), row.highlighted(), "not lit".into());
    super::controls::menu_closed(&menu);
    check(&format!("{NAME}: after menuDidClose, not lit"), !row.highlighted(), format!("{:?}", row.highlight_parts()));
}

/// A demo state as `chrome_render` builds it: Anthropic connected, Opus chosen, the menu built.
fn demo_state(mtm: MainThreadMarker) -> super::State {
    let controller = super::Controller::new(mtm);
    let menu = NSMenu::new(mtm);
    menu.setAutoenablesItems(false);
    let options = crate::Options { fake_permissions: true, ..Default::default() };
    let mut state = super::State::new(mtm, controller, options, Box::new(|_| {}), None, menu, None);
    state.apply(UiUpdate::Listen(ListenState::Listening));
    state.apply(UiUpdate::Permissions(Permission::ALL.iter().map(|p| (*p, PermissionState::Granted)).collect()));
    state.apply(UiUpdate::RuntimeSettings(RuntimeSettings { dry_run: false, ..Default::default() }));
    state.engine = super::offscreen::engine_states().into_iter().find(|(n, _)| *n == "ready").unwrap().1;
    state.api = demo_anthropic_providers(ProviderAuth::Connected);
    state.rebuild_menu();
    state
}

/// The public edition has no bug inbox, so its menu has no Bug Notes row; the personal one keeps it.
fn bug_notes_row(mtm: MainThreadMarker, check: &mut Check) {
    const NAME: &str = "bug_notes_row_only_in_the_personal_menu";
    let state = demo_state(mtm);
    let rows: Vec<String> = state.menu.itemArray().iter().filter(|i| i.action() == Some(objc2::sel!(openBugNotes:))).map(|i| i.title().to_string()).collect();
    let titled = state.menu.itemArray().iter().any(|i| i.title().to_string().starts_with("Bug Notes"));
    if cfg!(feature = "public") {
        check(&format!("{NAME}: the public menu has no Bug Notes row"), rows.is_empty() && !titled, format!("{rows:?}"));
    } else {
        check(&format!("{NAME}: the personal menu keeps one Bug Notes row"), rows == ["Bug Notes"] && titled, format!("{rows:?}"));
    }
}

/// Each model radio's title in `menu`, in order.
fn model_titles(menu: &NSMenu) -> Vec<String> {
    menu.itemArray().iter().filter(|i| !i.isHidden()).filter_map(|i| i.view())
        .filter_map(|v| v.subviews().iter().find_map(|s| s.downcast::<NSButton>().ok()))
        .map(|b| b.attributedTitle().string().to_string().trim().to_string())
        .collect()
}

fn cloud(dir: &Path, mtm: MainThreadMarker, check: &mut Check) {
    const NAME: &str = "cloud_submenu_gains_a_model_added_while_open";
    const NEW: &str = "Claude Fable 5.1";
    let mut state = demo_state(mtm);
    let Some(sub) = state.api_item.as_ref().and_then(|i| i.submenu()) else { check(NAME, false, "no Agent ▸ Model".into()); return };
    let Some(background) = state.background_menu.clone() else { check(NAME, false, "no Background Tasks ▸ Model".into()); return };
    // The menu is open (as `menuWillOpen:` marks it) when the provider's answer comes.
    state.menu_open = true;
    let providers_before: Vec<Retained<NSMenuItem>> = {
        let items = sub.itemArray();
        let from = items.iter().position(|i| i.isSectionHeader() && i.title().to_string() == "Providers").unwrap_or(0);
        items.iter().skip(from).collect()
    };
    let before = model_titles(&sub);
    check(&format!("{NAME}: before, Cloud lacks {NEW}"), !before.iter().any(|t| t.contains(NEW)), format!("{before:?}"));
    let mut api = state.api.clone();
    if let Some(row) = api.catalog.iter_mut().find(|r| r.name == NEW) { row.added = true; }
    api.models.push(ApiModelRow { provider: "anthropic".into(), id: "claude-fable-5-1".into(), name: NEW.into(), added: true, unavailable: None, detail: None });
    state.apply(UiUpdate::ApiProviders(api.clone()));
    let after = model_titles(&sub);
    check(&format!("{NAME}: the open Agent ▸ Model holds the new row"), after.iter().any(|t| t.contains(NEW)), format!("{after:?}"));
    let cloud_at = sub.itemArray().iter().position(|i| i.isSectionHeader() && i.title().to_string() == CLOUD);
    let new_at = sub.itemArray().iter().position(|i| i.view().and_then(|v| v.subviews().iter().find_map(|s| s.downcast::<NSButton>().ok())).is_some_and(|b| b.attributedTitle().string().to_string().contains(NEW)));
    check(&format!("{NAME}: under Cloud"), matches!((cloud_at, new_at), (Some(c), Some(n)) if n > c), format!("{cloud_at:?} {new_at:?}"));
    let tags_ok = state.api_rows.models.iter().enumerate().all(|(i, (b, _, _))| b.tag() == i as isize)
        && state.api_rows.models.iter().any(|(_, p, id)| p == "anthropic" && id == "claude-fable-5-1")
        && state.api_rows.models.len() == state.api_rows.details.len();
    check(&format!("{NAME}: its click reaches its own model (tags index the live rows)"), tags_ok, format!("{} rows", state.api_rows.models.len()));
    let providers_after: Vec<Retained<NSMenuItem>> = {
        let items = sub.itemArray();
        let from = items.iter().position(|i| i.isSectionHeader() && i.title().to_string() == "Providers").unwrap_or(0);
        items.iter().skip(from).collect()
    };
    let same = providers_before.len() == providers_after.len() && providers_before.iter().zip(&providers_after).all(|(a, b)| std::ptr::eq(&**a, &**b));
    check(&format!("{NAME}: Providers keeps its items (an open picker stays open)"), same, format!("{} → {}", providers_before.len(), providers_after.len()));
    let bg = model_titles(&background);
    check(&format!("{NAME}: Background Tasks ▸ Model holds it too"), bg.iter().any(|t| t.contains(NEW)), format!("{bg:?}"));
    let bg_tags = state.background_rows.models.iter().enumerate().all(|(i, (b, _, _))| b.tag() == i as isize);
    check(&format!("{NAME}: Background's tags index its live rows"), bg_tags, String::new());
    // And removed again while open: the row goes.
    api.models.retain(|m| m.id != "claude-fable-5-1");
    state.apply(UiUpdate::ApiProviders(api));
    let gone = model_titles(&sub);
    check(&format!("{NAME}: removed while open, the row goes"), !gone.iter().any(|t| t.contains(NEW)) && gone.len() == before.len(), format!("{gone:?}"));
    // A settings change with the same models re-lays nothing: the rows are the same objects.
    let first = state.api_rows.models.first().map(|(b, _, _)| b.clone());
    state.apply(UiUpdate::ApiProviders(ApiProviders { selected: Some(("anthropic".into(), "claude-sonnet-5-5".into())), ..state.api.clone() }));
    let kept = matches!((&first, state.api_rows.models.first()), (Some(a), Some((b, _, _))) if std::ptr::eq(&**a, &**b));
    check(&format!("{NAME}: the same models keep their rows"), kept, String::new());
    // The live submenu with the new row again, as it shows (its rows above Providers, taken out of it to draw).
    let mut api = state.api.clone();
    api.models.push(ApiModelRow { provider: "anthropic".into(), id: "claude-fable-5-1".into(), name: NEW.into(), added: true, unavailable: None, detail: None });
    state.apply(UiUpdate::ApiProviders(api));
    let shown = NSMenu::new(mtm);
    for item in sub.itemArray().iter().take_while(|i| !(i.isSectionHeader() && i.title().to_string() == "Providers")) {
        sub.removeItem(&item);
        shown.addItem(&item);
    }
    let view = stacked(&shown, MENU_WIDTH, (5.0, 5.0), true, mtm);
    shot(dir, "menu-cloud-new-row", &view, check);
}

/// `--menu-bench`: 1000 syncs of the open menu with 30 apps in App Audio, titles made each time (before the cache)
/// and cached (after), in interleaved blocks of 100; then 1000 state changes with the menu closed, synced (before)
/// and skipped (after). Prints mean microseconds per call. Returns nothing failed.
pub(crate) fn bench() -> Vec<String> {
    let mtm = MainThreadMarker::new().expect("the menu bench runs on the main thread");
    NSApplication::sharedApplication(mtm).setActivationPolicy(NSApplicationActivationPolicy::Prohibited);
    let mut state = demo_state(mtm);
    let mut apps: Vec<String> = std::fs::read_dir("/System/Applications").into_iter().flatten().flatten()
        .map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "app")).map(|p| p.to_string_lossy().into_owned()).collect();
    apps.sort();
    apps.truncate(30);
    let apps: Vec<App> = apps.iter().enumerate().map(|(i, path)| App {
        id: format!("app.{i}"), name: Path::new(path).file_stem().unwrap().to_string_lossy().into_owned(), icon_path: Some(path.clone()),
        playing: i % 3 == 0, recorded: i % 2 == 0,
    }).collect();
    let count = apps.len();
    state.app_audio = AppAudio { apps };
    state.rebuild_menu();
    state.menu_open = true;
    let time = |state: &mut super::State, n: usize, cached: bool| {
        super::menu::APP_TITLE_CACHE.with(|c| c.set(cached));
        let start = std::time::Instant::now();
        for _ in 0..n { super::menu::sync_tracking(state); }
        start.elapsed()
    };
    // Warm-up, both ways.
    time(&mut state, 20, false);
    time(&mut state, 20, true);
    let (mut before, mut after) = (std::time::Duration::ZERO, std::time::Duration::ZERO);
    for _ in 0..10 {
        before += time(&mut state, 100, false);
        after += time(&mut state, 100, true);
    }
    super::menu::APP_TITLE_CACHE.with(|c| c.set(true));
    eprintln!("bench open menu, {count} apps, 1000 syncs: before {:.1} µs/sync, after {:.1} µs/sync", before.as_secs_f64() * 1e3, after.as_secs_f64() * 1e3);
    // Closed: what a state change cost (the whole sync) and costs now (nothing until the next open).
    state.menu_open = false;
    let (mut synced, mut skipped) = (std::time::Duration::ZERO, std::time::Duration::ZERO);
    for _ in 0..10 {
        super::menu::APP_TITLE_CACHE.with(|c| c.set(false));
        let start = std::time::Instant::now();
        for _ in 0..100 { super::menu::sync_tracking(&mut state); }
        synced += start.elapsed();
        super::menu::APP_TITLE_CACHE.with(|c| c.set(true));
        let start = std::time::Instant::now();
        for _ in 0..100 { state.refresh_menu(); }
        skipped += start.elapsed();
    }
    eprintln!("bench closed menu, {count} apps, 1000 changes: before {:.1} µs/change, after {:.2} µs/change", synced.as_secs_f64() * 1e3, skipped.as_secs_f64() * 1e3);
    vec![]
}
