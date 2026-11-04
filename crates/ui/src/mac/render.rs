//! `Options::render_dir`: writes PNGs of every window and of the real menu, then quits. Windows are drawn at alpha 0
//! (invisible on screen) through `cacheDisplayInRect`, title bar included, at the screen's backing scale; no screen
//! capture, so no Screen Recording permission. The menu is the real NSMenu: it opens for about a quarter second,
//! its window is drawn the same way, then tracking is cancelled. While it is open, the checks post real
//! mouse and key events into the menu window's queue (in-process; the cursor never moves) and read the result.

use std::cell::RefCell;
use std::path::{Path, PathBuf};

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::sel;
use objc2_app_kit::{NSApplication, NSBitmapImageFileType, NSMenu, NSView, NSWindow};
use objc2_foundation::{MainThreadMarker, NSDictionary, NSPoint, NSRunLoop, NSRunLoopCommonModes, NSTimer};

use super::util::ns;
use super::{with_state, State};

struct Render {
    dir: PathBuf,
    step: usize,
    /// The menu whose tracking loop runs, and what is left to do inside it.
    tracking: Option<(Retained<NSMenu>, std::collections::VecDeque<InMenu>)>,
    /// Only the in-process menu checks, no renders of windows.
    checks_only: bool,
    failures: usize,
    /// The menu closed before its actions ran out (a Return): the rest run on timers, then the next step.
    closed_early: bool,
    /// The check sequence running: (its number, failures before it, attempt).
    sequence: Option<(usize, usize, usize)>,
}

thread_local! {
    static RENDER: RefCell<Option<Render>> = const { RefCell::new(None) };
}

pub fn start(dir: PathBuf, checks_only: bool) {
    std::fs::create_dir_all(&dir).ok();
    RENDER.with(|r| *r.borrow_mut() = Some(Render { dir, step: 0, tracking: None, checks_only, failures: 0, closed_early: false, sequence: None }));
    with_state(|s| schedule(s, 0.8));
}

fn schedule(s: &State, delay: f64) {
    let timer = unsafe {
        NSTimer::timerWithTimeInterval_target_selector_userInfo_repeats(delay, &s.controller, sel!(renderStep:), None, false)
    };
    unsafe { NSRunLoop::currentRunLoop().addTimer_forMode(&timer, NSRunLoopCommonModes) };
}

/// Draws `view` into a PNG at the window's backing scale.
pub fn capture(view: &NSView, path: &Path) -> bool {
    let bounds = view.bounds();
    let Some(rep) = view.bitmapImageRepForCachingDisplayInRect(bounds) else { return false };
    view.cacheDisplayInRect_toBitmapImageRep(bounds, &rep);
    let Some(data) = (unsafe { rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new()) })
    else {
        return false;
    };
    data.writeToFile_atomically(&ns(&path.to_string_lossy()), true)
}

/// The whole window: the frame view holds the title bar and the content.
fn capture_window(window: &NSWindow, path: &Path) {
    window.setAlphaValue(0.0);
    window.setIgnoresMouseEvents(true);
    window.orderFrontRegardless();
    window.displayIfNeeded();
    let view = window.contentView().and_then(|c| unsafe { c.superview() }).or_else(|| window.contentView());
    if let Some(view) = view {
        view.layoutSubtreeIfNeeded();
        view.displayIfNeeded();
        let ok = capture(&view, path);
        eprintln!("render {} {}", path.display(), if ok { "ok" } else { "FAILED" });
    }
    window.orderOut(None);
}

/// The menu-bar button: its own view, drawn the same way.
fn capture_status_item(s: &State, path: &Path) {
    if let Some(button) = s.status_item.as_ref().and_then(|i| i.button(s.mtm)) {
        button.displayIfNeeded();
        let ok = capture(&button, path);
        eprintln!("render {} {}", path.display(), if ok { "ok" } else { "FAILED" });
    }
}

/// The menu-bar button drawn in the light and the dark menu-bar appearance over a flat bar of that appearance's
/// colour, 24 pt tall: `status-bar-<state>-light.png` and `-dark.png`. A real bar is translucent over the wallpaper.
fn capture_status_bars(s: &State, dir: &Path, state: &str) {
    use objc2_app_kit::{
        NSAppearance, NSAppearanceCustomization as _, NSAppearanceNameAqua, NSAppearanceNameDarkAqua, NSBitmapImageRep,
        NSColor, NSGraphicsContext, NSImage,
    };
    use objc2::AllocAnyThread as _;
    let Some(button) = s.status_item.as_ref().and_then(|i| i.button(s.mtm)) else { return };
    let before = button.appearance();
    for (name, appearance, bar) in [
        ("light", unsafe { NSAppearanceNameAqua }, (0.93, 0.93, 0.94)),
        ("dark", unsafe { NSAppearanceNameDarkAqua }, (0.13, 0.13, 0.14)),
    ] {
        let path = dir.join(format!("status-bar-{state}-{name}.png"));
        button.setAppearance(NSAppearance::appearanceNamed(appearance).as_deref());
        button.display();
        let bounds = button.bounds();
        let Some(glyph) = button.bitmapImageRepForCachingDisplayInRect(bounds) else { continue };
        button.cacheDisplayInRect_toBitmapImageRep(bounds, &glyph);
        let (w, h) = (glyph.pixelsWide(), glyph.pixelsHigh());
        let scale = w as f64 / bounds.size.width;
        let bar_h = (24.0 * scale).round() as isize;
        let pad = (8.0 * scale).round() as isize;
        let ok = (|| {
            let out = unsafe {
                NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bytesPerRow_bitsPerPixel(
                    NSBitmapImageRep::alloc(), std::ptr::null_mut(), w + 2 * pad, bar_h, 8, 4, true, false,
                    objc2_app_kit::NSDeviceRGBColorSpace, 0, 0,
                )
            }?;
            let context = NSGraphicsContext::graphicsContextWithBitmapImageRep(&out)?;
            NSGraphicsContext::saveGraphicsState_class();
            NSGraphicsContext::setCurrentContext(Some(&context));
            NSColor::colorWithSRGBRed_green_blue_alpha(bar.0, bar.1, bar.2, 1.0).setFill();
            objc2_app_kit::NSRectFill(super::util::rect(0.0, 0.0, (w + 2 * pad) as f64, bar_h as f64));
            let image = NSImage::initWithSize(NSImage::alloc(), objc2_foundation::NSSize::new(w as f64, h as f64));
            image.addRepresentation(&glyph);
            let y = ((bar_h - h) / 2) as f64;
            image.drawInRect(super::util::rect(pad as f64, y, w as f64, h as f64));
            context.flushGraphics();
            NSGraphicsContext::restoreGraphicsState_class();
            let data = unsafe { out.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new()) }?;
            Some(data.writeToFile_atomically(&ns(&path.to_string_lossy()), true))
        })()
        .unwrap_or(false);
        eprintln!("render {} {}", path.display(), if ok { "ok" } else { "FAILED" });
    }
    button.setAppearance(before.as_deref());
}

/// One thing to do while a menu tracks, a quarter second apart: draw it, or exercise it in-process.
enum InMenu {
    /// Draw the open menu's window to this file.
    Capture(String),
    /// Post a left click (down, up) at a tile's centre into the menu window's event queue.
    ClickTile(usize),
    /// Post a click on Launch at Login's row.
    ClickLogin,
    /// Post a key press into the menu window (arrows, Return).
    Key(u16, char),
    /// A click on the menu's top padding, where there is no item: a freshly opened menu takes its first release
    /// as the end of the click that opened it.
    Prime,
    /// Check the state after the previous step; a failure is counted and printed.
    Check(&'static str, fn(&State) -> Result<String, String>),
}

/// Main-menu and submenu renders, in the order they run. `Some(title)` opens that submenu on its own.
const SUBMENUS: [&str; 3] = ["Microphone", "App Audio", "Recording"];

pub fn step() {
    // Inside a menu's tracking loop: the next in-menu action.
    let next = RENDER.with(|r| r.borrow_mut().as_mut().and_then(|r| r.tracking.as_mut().and_then(|t| t.1.pop_front())));
    if let Some(action) = next {
        in_menu(action);
        let more = RENDER.with(|r| r.borrow().as_ref().and_then(|r| r.tracking.as_ref()).is_some_and(|t| !t.1.is_empty()));
        if more {
            schedule_tracking(0.25);
        } else if let Some((menu, _)) = RENDER.with(|r| r.borrow_mut().as_mut().and_then(|r| r.tracking.take())) {
            if RENDER.with(|r| std::mem::take(&mut r.borrow_mut().as_mut().unwrap().closed_early)) {
                with_state(|s| schedule(s, 0.3));
            } else {
                // Ends the tracking loop; `open_menu` then schedules the next step.
                menu.cancelTracking();
            }
        }
        return;
    }

    let (step, dir, checks_only) = RENDER.with(|r| {
        let mut r = r.borrow_mut();
        let r = r.as_mut().unwrap();
        r.step += 1;
        (r.step, r.dir.clone(), r.checks_only)
    });
    if checks_only {
        return checks(step);
    }
    let dark = |name: &str| vec![InMenu::Capture(format!("{name}.png"))];
    match step {
        1 => with_state(move |s| {
            s.show_transcript();
            capture_window(&s.transcript.as_ref().unwrap().window, &dir.join("transcript.png"));
            schedule(s, 0.3);
        }),
        2 => with_state(move |s| {
            s.show_setup();
            capture_window(&s.setup.as_ref().unwrap().window, &dir.join("setup.png"));
            schedule(s, 0.3);
        }),
        // On, off, and Agent off (still listening), each on a light and a dark menu bar.
        3 => with_state(move |s| {
            capture_status_item(s, &dir.join("status-item.png"));
            capture_status_bars(s, &dir, "on");
            s.apply(crate::UiUpdate::Listen(crate::ListenState::Paused));
            capture_status_item(s, &dir.join("status-item-paused.png"));
            capture_status_bars(s, &dir, "paused");
            s.apply(crate::UiUpdate::Listen(crate::ListenState::Listening));
            let settings = s.runtime_settings.clone();
            s.apply(crate::UiUpdate::RuntimeSettings(crate::RuntimeSettings { dry_run: true, ..settings.clone() }));
            capture_status_bars(s, &dir, "agent-off");
            s.apply(crate::UiUpdate::RuntimeSettings(settings));
            schedule(s, 0.3);
        }),
        4 => with_state(move |s| {
            s.show_speakers();
            capture_window(&s.speakers.as_ref().unwrap().window, &dir.join("speakers.png"));
            schedule(s, 0.3);
        }),
        5 => with_state(move |s| {
            for (class, first) in s.name_entry.first_click_check() {
                eprintln!("first click: {class} accepts={first}");
            }
            if let Some(window) = s.name_entry.visible_window() {
                if let Some(field) = s.name_entry.field() {
                    field.setStringValue(&ns("Marek"));
                }
                capture_window(&window, &dir.join("voice-name-entry.png"));
                // The real target/action path: press Name as an accessibility press would (after this step).
                if let Some(confirm) = s.name_entry.confirm_button() {
                    let confirm = super::MainOnly(confirm);
                    dispatch2::DispatchQueue::main().exec_async(move || {
                        let confirm = confirm;
                        unsafe { confirm.0.performClick(None) };
                    });
                }
            } else {
                eprintln!("render voice-name-entry: no entry was opened");
            }
            schedule(s, 0.3);
        }),
        // The first frame, before any interaction: every tile on.
        6 => open_menu(None, dark("menu")),
        7 => {
            set_appearance(false);
            open_menu(None, dark("menu-light"))
        }
        // Every submenu, dark then light.
        // Steps past the last submenu stay numbered for the renders after them; they only move on.
        8..=19 if (step - 8) / 2 >= SUBMENUS.len() => with_state(|s| schedule(s, 0.0)),
        8..=19 => {
            let index = (step - 8) / 2;
            let light = (step - 8) % 2 == 1;
            set_appearance(!light);
            let name = format!("menu-{}{}", SUBMENUS[index].to_lowercase().replace(' ', "-"), if light { "-light" } else { "" });
            open_menu(Some(SUBMENUS[index]), dark(&name))
        }
        // Every tile off, dark then light: Listening off adds "Off since".
        20 => {
            set_appearance(true);
            super::with_state_now(|s| all_tiles(s, false));
            open_menu(None, dark("menu-tiles-off"))
        }
        21 => {
            set_appearance(false);
            open_menu(None, dark("menu-tiles-off-light"))
        }
        22 => {
            set_appearance(true);
            super::with_state_now(|s| all_tiles(s, true));
            super::with_state_now(|s| s.apply(crate::UiUpdate::Listen(crate::ListenState::NeedsSetup)));
            open_menu(None, dark("menu-setup-needed"))
        }
        23 => {
            super::with_state_now(|s| s.apply(crate::UiUpdate::Listen(crate::ListenState::Failed("audio capture stopped".into()))));
            open_menu(None, dark("menu-failed"))
        }
        24 => with_state(move |s| {
            s.apply(crate::UiUpdate::Listen(crate::ListenState::Listening));
            let alert = super::speakers::forget_alert("Dave", s.mtm);
            alert.layout();
            capture_window(&alert.window(), &dir.join("speakers-forget.png"));
            let empty = super::transcript::TranscriptWindow::new(s.mtm);
            capture_window(&empty.window, &dir.join("transcript-empty.png"));
            schedule(s, 0.3);
        }),
        25 => with_state(move |s| {
            let all: Vec<_> = crate::Permission::ALL.iter().map(|p| (*p, crate::PermissionState::Granted)).collect();
            s.apply(crate::UiUpdate::Permissions(all));
            s.show_setup();
            capture_window(&s.setup.as_ref().unwrap().window, &dir.join("setup-allowed.png"));
            s.apply(crate::UiUpdate::Speakers(Vec::new()));
            s.show_speakers();
            capture_window(&s.speakers.as_ref().unwrap().window, &dir.join("speakers-empty.png"));
            schedule(s, 0.3);
        }),
        // Healthy and set up: no line under the tiles.
        26 => open_menu(None, dark("menu-healthy")),
        // The chosen AirPods are not connected: still checked, the built-in mic stands in.
        27 => {
            super::with_state_now(|s| s.apply(crate::UiUpdate::Microphones(microphones(false, None, &[]))));
            open_menu(Some("Microphone"), dark("menu-microphone-missing"))
        }
        // AirPods as the primary, the built-in mic switched on as an extra.
        28 => {
            super::with_state_now(|s| {
                s.apply(crate::UiUpdate::Microphones(microphones(true, Some("AC-3E-B1:input"), &["BuiltInMicrophoneDevice"])))
            });
            open_menu(Some("Microphone"), dark("menu-microphone-extra"))
        }
        // The Agent window with a live session, then Agent off (would-send entries), then the empty state.
        29 => with_state(move |s| {
            s.show_agent();
            capture_window(&s.agent_window.as_ref().unwrap().window, &dir.join("agent.png"));
            s.apply(crate::UiUpdate::RuntimeSettings(crate::RuntimeSettings { dry_run: true, ..s.runtime_settings.clone() }));
            s.apply(crate::UiUpdate::AgentSession(crate::AgentSession { agent_on: false, ..s.agent_session.clone() }));
            s.apply(crate::UiUpdate::AgentWindow(crate::AgentWindowUpdate::State(crate::agent_view::demo_state(crate::AgentRunState::Off))));
            capture_window(&s.agent_window.as_ref().unwrap().window, &dir.join("agent-off.png"));
            schedule(s, 0.3);
        }),
        30 => open_menu(None, dark("menu-agent-off")),
        31 => with_state(move |s| {
            s.apply(crate::UiUpdate::AgentSession(crate::AgentSession {
                model: Some("claude-opus-5-5".into()),
                session_id: None,
                can_open: false,
                agent_on: true,
                harness: crate::Harness::Pi,
            }));
            s.apply(crate::UiUpdate::AgentWindow(crate::AgentWindowUpdate::History(Vec::new())));
            s.apply(crate::UiUpdate::RuntimeSettings(crate::RuntimeSettings { dry_run: false, ..s.runtime_settings.clone() }));
            s.show_agent();
            capture_window(&s.agent_window.as_ref().unwrap().window, &dir.join("agent-empty.png"));
            schedule(s, 0.3);
        }),
        32 => {
            super::with_state_now(|s| {
                let screen = crate::ScreenRecording { resolution: crate::ScreenResolution::Native, fps: 10 };
                s.apply(crate::UiUpdate::RuntimeSettings(crate::RuntimeSettings { screen, ..s.runtime_settings.clone() }))
            });
            open_menu(Some("Recording"), dark("menu-recording-native"))
        }
        33 => {
            super::with_state_now(|s| s.apply(crate::UiUpdate::AppAudio(crate::AppAudio::default())));
            open_menu(Some("App Audio"), dark("menu-app-audio-empty"))
        }
        // Bug Notes: the list with notes (one empty), the menu row with its count, then the empty list.
        34 => with_state(move |s| {
            s.apply(crate::UiUpdate::BugNotes(crate::demo_bug_notes()));
            s.show_bug_notes();
            capture_window(&s.bug_window.as_ref().unwrap().window, &dir.join("bug-notes.png"));
            schedule(s, 0.3);
        }),
        35 => open_menu(None, dark("menu-bug-notes")),
        36 => with_state(move |s| {
            let empty = super::bugs::BugNotesWindow::new(s.mtm, false);
            empty.show(&[]);
            capture_window(&empty.window, &dir.join("bug-notes-empty.png"));
            schedule(s, 0.3);
        }),
        37 => with_state(|s| {
            s.runtime_settings.pending_reload=vec!["Screen Recording".into()];
            s.rebuild_menu();
            // [chrome] No Reload row: the status line offers the retry; its click decision is the real one.
            if super::menu::status_click(s)==super::menu::StatusClick::Retry { s.emit(crate::UiAction::Reload); }
            schedule(s,0.3);
        }),
        38 => open_menu(None, dark("menu-reload-pending")),
        // Seeing off: the recording choices stay selectable.
        39 => {
            super::with_state_now(|s| {
                s.runtime_settings.pending_reload.clear();
                let screen = crate::ScreenRecording::default();
                s.apply(crate::UiUpdate::RuntimeSettings(crate::RuntimeSettings { screen, seeing: false, ..s.runtime_settings.clone() }))
            });
            open_menu(Some("Recording"), dark("menu-recording-seeing-off"))
        }
        // The in-process event checks, with renders of what they leave on screen.
        40..=42 => checks(step - 39),
        _ => finish(),
    }
}

/// The menu-stays-open checks on their own (`Options::render_checks_only`), or as the render's last steps. A
/// sequence with a failure runs again from the default state, up to three attempts: the menu's tracking loop can
/// drop a synthetic event or close for outside input on a Mac in use, while a real regression fails every attempt.
fn checks(n: usize) {
    let retry = RENDER.with(|r| {
        let mut r = r.borrow_mut();
        let r = r.as_mut().unwrap();
        let (seq, before, attempt) = r.sequence.take()?;
        let failed = r.failures - before;
        if failed > 0 && attempt < 3 {
            eprintln!("checks {seq}: attempt {attempt} had {failed} failure(s); retrying");
            r.failures = before;
            r.step -= 1;
            return Some((seq, attempt + 1));
        }
        eprintln!("checks {seq}: {} on attempt {attempt}", if failed == 0 { "passed" } else { "FAILED" });
        None
    });
    let (n, attempt) = retry.unwrap_or((n, 1));
    if n > 2 {
        return finish();
    }
    RENDER.with(|r| {
        let mut r = r.borrow_mut();
        let r = r.as_mut().unwrap();
        r.sequence = Some((n, r.failures, attempt));
    });
    super::with_state_now(|s| {
        all_tiles(s, true);
        s.settings_value.launch_at_login = true;
    });
    match n {
        1 => open_menu(None, main_menu_checks()),
        2 => open_menu(None, keyboard_checks()),
        _ => finish(),
    }
}

/// Pointer clicks on a tile and on Launch at Login, through the menu window's own event queue: each toggles in
/// place, emits its action, and the menu keeps tracking.
fn main_menu_checks() -> Vec<InMenu> {
    vec![
        InMenu::Prime,
        InMenu::Check("first frame: every tile on", |s| {
            let on: Vec<bool> = s.tile_views.iter().map(|t| t.is_on()).collect();
            if on == [true; 5] { Ok(format!("{on:?}")) } else { Err(format!("{on:?}")) }
        }),
        InMenu::ClickTile(1),
        InMenu::Check("click Seeing: off, menu still open", |s| {
            let tile = s.tile_views[1].is_on();
            let open = menu_open(s);
            if !tile && !s.runtime_settings.seeing && open { Ok(format!("tile on={tile} seeing={} open={open}", s.runtime_settings.seeing)) }
            else { Err(format!("tile on={tile} seeing={} open={open}", s.runtime_settings.seeing)) }
        }),
        InMenu::Capture("menu-click-seeing.png".into()),
        InMenu::ClickTile(3),
        InMenu::Check("click Agent: off, emitted first, menu still open", |s| {
            let (tile, open) = (s.tile_views[3].is_on(), menu_open(s));
            if !tile && s.runtime_settings.dry_run && open { Ok(format!("tile on={tile} dry_run=true open={open}")) }
            else { Err(format!("tile on={tile} dry_run={} open={open}", s.runtime_settings.dry_run)) }
        }),
        InMenu::ClickTile(2),
        InMenu::Check("click Speaking: off, menu still open", |s| {
            let (tile, open) = (s.tile_views[2].is_on(), menu_open(s));
            if !tile && !s.settings_value.speak_replies && open { Ok(format!("tile on={tile} open={open}")) }
            else { Err(format!("tile on={tile} speak={} open={open}", s.settings_value.speak_replies)) }
        }),
        InMenu::ClickTile(0),
        InMenu::Check("click Listening: off, menu still open", |s| {
            let (tile, open) = (s.tile_views[0].is_on(), menu_open(s));
            if !tile && open { Ok(format!("tile on={tile} open={open}")) } else { Err(format!("tile on={tile} open={open}")) }
        }),
        // The demo answers with Listen(Paused): the live sync path writes the line the first frame would.
        InMenu::Check("status line after a live sync: grey 'Listening off since …', as built", |s| {
            let Some((item, row)) = &s.status_line else { return Err("no status line".into()) };
            let grey = row.text_color().is_some_and(|c| objc2_foundation::NSObjectProtocol::isEqual(&*c, Some(&objc2_app_kit::NSColor::secondaryLabelColor())));
            let text = row.text();
            if !item.isHidden() && grey && text.starts_with("Listening off since") { Ok(text) } else { Err(format!("{text:?} grey={grey} hidden={}", item.isHidden())) }
        }),
        // [menu lane] The five switches are independent: with Listening off every tile switches.
        InMenu::Check("Listening off: every tile still available", |s| {
            let enabled: Vec<bool> = s.tile_views.iter().map(|t| t.is_enabled()).collect();
            if enabled == [true; 5] { Ok(format!("{enabled:?}")) } else { Err(format!("{enabled:?}")) }
        }),
        InMenu::ClickTile(4),
        InMenu::Check("click Engine with Listening off: off, menu still open", |s| {
            let (look, open) = (s.tile_views[4].look(), menu_open(s));
            if look == crate::TileLook::Off && s.engine.state == crate::EngineState::Off && open { Ok(format!("{look:?} open={open}")) }
            else { Err(format!("{look:?} state={:?} open={open}", s.engine.state)) }
        }),
        InMenu::ClickTile(1),
        InMenu::Check("click Seeing with Listening off: it switches on its own, menu still open", |s| {
            let (tile, open) = (s.tile_views[1].is_on(), menu_open(s));
            if tile && s.runtime_settings.seeing && open { Ok(format!("tile on={tile} open={open}")) }
            else { Err(format!("tile on={tile} seeing={} open={open}", s.runtime_settings.seeing)) }
        }),
        InMenu::ClickLogin,
        InMenu::Check("click Launch at Login: toggled, menu still open", |s| {
            let (row, open) = (s.login_row.as_ref().is_some_and(|r| r.is_on()), menu_open(s));
            if row == s.settings_value.launch_at_login && !row && open { Ok(format!("row on={row} open={open}")) }
            else { Err(format!("row on={row} setting={} open={open}", s.settings_value.launch_at_login)) }
        }),
        InMenu::Capture("menu-click-all-off.png".into()),
        // Back on, Listening first (the others need it), so later renders start from the default.
        InMenu::ClickTile(0),
        InMenu::ClickTile(1),
        InMenu::ClickTile(2),
        InMenu::ClickTile(3),
        InMenu::ClickTile(4),
        InMenu::ClickLogin,
        // The engine shows Starting at the click and Ready once the demo answers: either counts as on here.
        InMenu::Check("click every tile again: all on, menu still open", |s| {
            let on: Vec<bool> = s.tile_views.iter().map(|t| t.look() != crate::TileLook::Off).collect();
            let open = menu_open(s);
            if on == [true; 5] && s.settings_value.launch_at_login && open { Ok(format!("{on:?} open={open}")) } else { Err(format!("{on:?} open={open}")) }
        }),
    ]
}

/// Keyboard: ↑ from the bottom passes Quit and Support the Developer and reaches Launch at Login, highlighted like a
/// native row; ↑ again highlights the native Open Archive Folder row, for comparing the two highlights.
fn keyboard_checks() -> Vec<InMenu> {
    const UP: (u16, char) = (126, '\u{F700}');
    vec![
        InMenu::Key(UP.0, UP.1),
        InMenu::Key(UP.0, UP.1),
        InMenu::Key(UP.0, UP.1),
        InMenu::Check("keyboard ↑↑↑: Launch at Login highlighted", |s| {
            let item = s.login_row.as_ref().and_then(|r| r.enclosingMenuItem());
            if item.is_some_and(|i| i.isHighlighted()) { Ok("highlighted".into()) } else { Err("not highlighted".into()) }
        }),
        InMenu::Capture("menu-keyboard-login.png".into()),
        InMenu::Key(UP.0, UP.1),
        // Shortcuts ▸ (24 Oct) is the row above Launch at Login; Name & Icon moved into Personalisation (26 Oct).
        InMenu::Check("keyboard ↑: native Shortcuts row highlighted next", |s| {
            let title = s.menu.highlightedItem().map(|i| i.title().to_string());
            if title.as_deref() == Some(super::menu::SHORTCUTS_TITLE) { Ok(super::menu::SHORTCUTS_TITLE.into()) } else { Err(format!("{title:?}")) }
        }),
        InMenu::Capture("menu-keyboard-native-row.png".into()),
    ]
}

fn menu_open(s: &State) -> bool {
    s.menu_open && any_menu_window()
}

fn any_menu_window() -> bool {
    let mtm = MainThreadMarker::new().unwrap();
    NSApplication::sharedApplication(mtm)
        .windows()
        .iter()
        .any(|w| w.isVisible() && w.class().name().to_string_lossy().contains("Menu"))
}

fn all_tiles(s: &mut State, on: bool) {
    s.apply(crate::UiUpdate::Listen(if on { crate::ListenState::Listening } else { crate::ListenState::Paused }));
    let runtime = crate::RuntimeSettings { dry_run: !on, seeing: on, ..s.runtime_settings.clone() };
    s.apply(crate::UiUpdate::RuntimeSettings(runtime));
    s.apply(crate::UiUpdate::Settings(crate::Settings { speak_replies: on, ..s.settings_value.clone() }));
    s.engine.state = if on { crate::EngineState::Ready } else { crate::EngineState::Off };
    s.refresh_menu();
}

/// Dark is what the user sees: the app is darkAqua (Vella) and this Mac's menus follow its Dark system appearance.
/// Light renders put Aqua on the menu and its submenus themselves, as a light-mode Mac would draw them.
fn set_appearance(dark: bool) {
    super::with_state_now(|s| s.render_light = !dark);
}

fn set_menu_appearance(menu: &NSMenu, appearance: Option<&objc2_app_kit::NSAppearance>) {
    use objc2_app_kit::NSAppearanceCustomization as _;
    menu.setAppearance(appearance);
    for item in menu.itemArray().iter() {
        if let Some(sub) = item.submenu() {
            set_menu_appearance(&sub, appearance);
        }
    }
}

fn finish() {
    let failures = RENDER.with(|r| r.borrow().as_ref().map_or(0, |r| r.failures));
    eprintln!("render checks: {failures} failed");
    let mtm = MainThreadMarker::new().unwrap();
    for window in NSApplication::sharedApplication(mtm).windows().iter() {
        window.orderOut(None);
    }
    std::process::exit(if failures == 0 { 0 } else { 1 });
}

fn in_menu(action: InMenu) {
    let dir = RENDER.with(|r| r.borrow().as_ref().map(|r| r.dir.clone())).unwrap();
    match action {
        InMenu::Capture(name) => capture_menu(&dir, &name),
        InMenu::ClickTile(i) => {
            let view = super::with_state_now(|s| s.tile_views.get(i).map(|v| Retained::into_super(v.clone()))).flatten();
            post_click(view.as_deref(), &format!("tile {i}"));
        }
        InMenu::ClickLogin => {
            let view = super::with_state_now(|s| s.login_row.clone().map(Retained::into_super)).flatten();
            post_click(view.as_deref(), "Launch at Login");
        }
        InMenu::Key(code, ch) => post_key(code, ch),
        InMenu::Prime => prime_menu(),
        InMenu::Check(name, check) => {
            let result = super::with_state_now(|s| check(s)).unwrap_or_else(|| Err("state busy".into()));
            match result {
                Ok(detail) => eprintln!("check {name}: ok ({detail})"),
                Err(detail) => {
                    eprintln!("check {name}: FAILED ({detail})");
                    RENDER.with(|r| r.borrow_mut().as_mut().unwrap().failures += 1);
                }
            }
        }
    }
}

/// The open menu's window, drawn to `name`. With a submenu open on its own it is the only menu window.
fn capture_menu(dir: &Path, name: &str) {
    let mtm = MainThreadMarker::new().unwrap();
    let mut done = false;
    for window in NSApplication::sharedApplication(mtm).windows().iter() {
        let class = window.class().name().to_string_lossy().to_string();
        if window.isVisible() && class.contains("Menu") {
            let view = window.contentView().and_then(|c| unsafe { c.superview() }).or_else(|| window.contentView());
            if let Some(view) = view {
                view.displayIfNeeded();
                done = capture(&view, &dir.join(name));
                eprintln!("render {name} via {class} {}", if done { "ok" } else { "FAILED" });
            }
            break;
        }
    }
    if !done {
        eprintln!("render {name}: no menu window found");
    }
}

/// A left click at `view`'s centre, posted to the application's event queue for the menu window: the real
/// AppKit path (the menu's tracking loop dequeues it, hit-tests and delivers it), no cursor movement.
fn post_click(view: Option<&NSView>, what: &str) {
    use objc2_app_kit::{NSEvent, NSEventModifierFlags, NSEventType};
    let mtm = MainThreadMarker::new().unwrap();
    let Some((view, window)) = view.and_then(|v| v.window().map(|w| (v, w))) else {
        eprintln!("click {what}: not on screen");
        RENDER.with(|r| r.borrow_mut().as_mut().unwrap().failures += 1);
        return;
    };
    let b = view.bounds();
    let at = view.convertPoint_toView(NSPoint::new(b.size.width / 2.0, b.size.height / 2.0), None);
    // The press must land on the view itself, as a pointer there would.
    let hit = window.contentView().and_then(|c| unsafe { c.superview() }.unwrap_or(c).hitTest(at));
    // A row host takes its button's clicks, so landing on an ancestor row counts.
    let lands = hit.as_deref().is_some_and(|h| {
        std::ptr::eq(h, view) || h.isDescendantOf(view) || (view.isDescendantOf(h) && h.class().name().to_string_lossy() == "HeimdallControlRow")
    });
    let time = objc2_foundation::NSProcessInfo::processInfo().systemUptime();
    let event = |ty| NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
        ty, at, NSEventModifierFlags(0), time, window.windowNumber(), None, next_event_number(), 1, 1.0,
    );
    for ty in [NSEventType::MouseMoved, NSEventType::LeftMouseDown, NSEventType::LeftMouseUp] {
        if let Some(e) = event(ty) {
            NSApplication::sharedApplication(mtm).postEvent_atStart(&e, false);
        }
    }
    eprintln!("click {what}: hit-test lands on it: {lands}");
}

fn prime_menu() {
    use objc2_app_kit::{NSEvent, NSEventModifierFlags, NSEventType};
    let mtm = MainThreadMarker::new().unwrap();
    let app = NSApplication::sharedApplication(mtm);
    let Some(window) = app.windows().iter().find(|w| w.isVisible() && w.class().name().to_string_lossy().contains("Menu")) else { return };
    let frame = window.frame();
    let at = NSPoint::new(frame.size.width / 2.0, frame.size.height - 2.0);
    let time = objc2_foundation::NSProcessInfo::processInfo().systemUptime();
    for ty in [NSEventType::MouseMoved, NSEventType::LeftMouseDown, NSEventType::LeftMouseUp] {
        let event = NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
            ty, at, NSEventModifierFlags(0), time, window.windowNumber(), None, next_event_number(), 1, 1.0,
        );
        if let Some(event) = event {
            app.postEvent_atStart(&event, false);
        }
    }
}

/// Each synthetic event gets its own number, as real ones do.
fn next_event_number() -> isize {
    use std::sync::atomic::{AtomicIsize, Ordering};
    static NEXT: AtomicIsize = AtomicIsize::new(1_000_000);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

fn post_key(code: u16, ch: char) {
    use objc2_app_kit::{NSEvent, NSEventModifierFlags, NSEventType};
    let mtm = MainThreadMarker::new().unwrap();
    let app = NSApplication::sharedApplication(mtm);
    if !app.windows().iter().any(|w| w.isVisible() && w.class().name().to_string_lossy().contains("Menu")) {
        eprintln!("key {code}: no menu window");
        return;
    }
    let time = objc2_foundation::NSProcessInfo::processInfo().systemUptime();
    let chars = ns(&ch.to_string());
    let flags = if (0xF700..=0xF8FF).contains(&(ch as u32)) { NSEventModifierFlags::NumericPad | NSEventModifierFlags::Function } else { NSEventModifierFlags(0) };
    // Window 0: the open menu takes key events addressed to no window, as it does real keystrokes.
    let event = NSEvent::keyEventWithType_location_modifierFlags_timestamp_windowNumber_context_characters_charactersIgnoringModifiers_isARepeat_keyCode(
        NSEventType::KeyDown, NSPoint::new(0.0, 0.0), flags, time, 0, None, &chars, &chars, false, code,
    );
    if let Some(event) = event {
        app.postEvent_atStart(&event, false);
    }
}

/// Opens the status menu (or one of its submenus on its own) and runs `actions` inside its tracking loop.
fn open_menu(submenu: Option<&'static str>, actions: Vec<InMenu>) {
    let Some((menu, button)) = super::with_state_now(|s| {
        s.rebuild_menu();
        let aqua = s.render_light.then(|| objc2_app_kit::NSAppearance::appearanceNamed(unsafe { objc2_app_kit::NSAppearanceNameAqua })).flatten();
        set_menu_appearance(&s.menu, aqua.as_deref());
        let menu = match submenu {
            Some(title) => s.menu.itemWithTitle(&ns(title)).and_then(|i| i.submenu()).unwrap(),
            None => s.menu.clone(),
        };
        (menu, s.status_item.as_ref().and_then(|i| i.button(s.mtm)))
    }) else {
        return;
    };
    RENDER.with(|r| r.borrow_mut().as_mut().unwrap().tracking = Some((menu.clone(), actions.into())));
    // A submenu opened on its own stands in for one opened from the status menu: updates sync it in place.
    if submenu.is_some() {
        super::with_state_now(|s| s.menu_open = true);
    }
    schedule_tracking(0.25);
    // Blocks in the menu's tracking loop until the last action cancels it.
    match button {
        Some(button) => {
            let height = button.bounds().size.height;
            menu.popUpMenuPositioningItem_atLocation_inView(None, NSPoint::new(0.0, height + 4.0), Some(&button));
        }
        None => {
            menu.popUpMenuPositioningItem_atLocation_inView(None, NSPoint::new(200.0, 800.0), None);
        }
    }
    // Tracking ended early (a Return): the remaining actions run on timers, after the item's action arrives.
    let early = RENDER.with(|r| r.borrow().as_ref().and_then(|r| r.tracking.as_ref()).is_some_and(|t| !t.1.is_empty()));
    if early {
        RENDER.with(|r| r.borrow_mut().as_mut().unwrap().closed_early = true);
        if submenu.is_some() {
            super::with_state_now(|s| s.menu_open = false);
        }
        schedule_tracking(0.3);
        return;
    }
    RENDER.with(|r| r.borrow_mut().as_mut().unwrap().tracking = None);
    if submenu.is_some() {
        super::with_state_now(|s| s.menu_open = false);
    }
    with_state(|s| schedule(s, 0.3));
}

fn schedule_tracking(delay: f64) {
    let Some(target) = super::with_state_now(|s| s.target()) else { return };
    let target: &AnyObject = &target;
    let timer = unsafe {
        NSTimer::timerWithTimeInterval_target_selector_userInfo_repeats(delay, target, sel!(renderStep:), None, false)
    };
    unsafe { NSRunLoop::currentRunLoop().addTimer_forMode(&timer, NSRunLoopCommonModes) };
}

/// Render fixture: the built-in mic and AirPods chosen as primary (connected or not).
fn microphones(airpods_connected: bool, in_use: Option<&str>, extras: &[&str]) -> crate::Microphones {
    crate::Microphones {
        devices: vec![
            crate::Microphone {
                id: "BuiltInMicrophoneDevice".into(),
                name: "MacBook Pro Microphone".into(),
                connected: true,
                built_in: true,
            },
            crate::Microphone {
                id: "AC-3E-B1:input".into(),
                name: "AirPods Pro".into(),
                connected: airpods_connected,
                built_in: false,
            },
        ],
        primary: Some("AC-3E-B1:input".into()),
        in_use: Some(in_use.unwrap_or("BuiltInMicrophoneDevice").into()),
        extras: extras.iter().map(|e| e.to_string()).collect(),
    }
}
