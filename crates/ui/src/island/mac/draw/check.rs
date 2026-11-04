//! `heimdall-ui-demo --drawing <dir>`: drawing mode and pointing checked in process, without touching the user's
//! screen, cursor or keyboard. The overlay is parked outside every display at alpha 0, never key; the island's
//! windows are parked with it at alpha 0 on a virtual clock. Synthetic events go through `NSWindow.sendEvent`
//! (AppKit's own hit-testing and dispatch). Send runs end to end with a stand-in capture (a synthetic page) in place
//! of ScreenCaptureKit. Covers recon 18 Oct: Send taking a trackpad tap (through the overlay and through the
//! island's click target), and drawing mode opening on a fresh island (the empty-track crash). Over an open menu
//! (24 Oct): the overlay above menus with the island above it, a still held under the ink before the keyboard is
//! taken (another app's menu) or straight away (Heimdall's menu), and Send marking that still, not the screen now.

use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;

use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy, NSBezierPath, NSColor, NSEventModifierFlags, NSEventType, NSGraphicsContext, NSMainMenuWindowLevel, NSPopUpMenuWindowLevel, NSScreen, NSWindowSharingType};
use objc2_core_foundation::CFRetained;
use objc2_core_graphics::{kCGColorSpaceSRGB, CGBitmapContextCreate, CGBitmapContextCreateImage, CGColorSpace, CGContext, CGImage, CGImageAlphaInfo, CGImageByteOrderInfo};
use objc2_foundation::{MainThreadMarker, NSPoint, NSRect, NSSize};

use super::overlay::{self, Mode};
use super::still::{self, Hold};
use super::{ink, point};
use crate::drawing::{self, MarkedShot};
use crate::island::object::{Class, Event, Scene, Target};
use crate::mac::island::checks::{advance, key, mouse, spin_until, tap, Checks};
use crate::mac::island::host::{with_host, Host, Place, HOST};

fn r(x: f64, y: f64, w: f64, h: f64) -> NSRect {
    NSRect::new(NSPoint::new(x, y), NSSize::new(w, h))
}

/// A stand-in screen (a light page with a dark header) at `scale`.
fn page(w: f64, h: f64, scale: f64) -> CFRetained<CGImage> {
    let space = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB })).unwrap();
    let info = CGImageAlphaInfo::PremultipliedLast.0 | CGImageByteOrderInfo::Order32Big.0;
    let ctx = unsafe { CGBitmapContextCreate(std::ptr::null_mut(), (w * scale) as usize, (h * scale) as usize, 8, 0, Some(&space), info) }.unwrap();
    CGContext::scale_ctm(Some(&ctx), scale, scale);
    let ns = NSGraphicsContext::graphicsContextWithCGContext_flipped(&ctx, false);
    NSGraphicsContext::saveGraphicsState_class();
    NSGraphicsContext::setCurrentContext(Some(&ns));
    NSColor::colorWithWhite_alpha(0.98, 1.0).setFill();
    NSBezierPath::fillRect(r(0.0, 0.0, w, h));
    NSColor::colorWithWhite_alpha(0.07, 1.0).setFill();
    NSBezierPath::fillRect(r(0.0, h - 64.0, w, 64.0));
    NSGraphicsContext::restoreGraphicsState_class();
    CGBitmapContextCreateImage(Some(&ctx)).unwrap()
}

pub(crate) fn run(dir: &Path) -> Vec<String> {
    let mtm = MainThreadMarker::new().expect("render_drawing runs on the main thread");
    NSApplication::sharedApplication(mtm).setActivationPolicy(NSApplicationActivationPolicy::Prohibited);
    std::fs::create_dir_all(dir).ok();
    let mut c = Checks::new();
    let Some(screen) = NSScreen::mainScreen(mtm) else {
        c.check(false, "a display to lay drawing mode out on");
        return c.failed;
    };
    let frame = screen.frame();
    let (w, h, scale) = (frame.size.width, frame.size.height, screen.backingScaleFactor());
    // The stand-in capture: the page at the display's pixel size.
    let grab_page = page(w, h, scale);
    let (pw, ph) = (CGImage::width(Some(&grab_page)) as u32, CGImage::height(Some(&grab_page)) as u32);
    let rgb = ink::composite_image(&grab_page, (1.0, 1.0), &[]).unwrap_or_default();
    drawing::set_grabber(move |_| Ok((pw, ph, rgb.clone())));
    let shots: Rc<RefCell<Vec<MarkedShot>>> = Default::default();
    let sink = shots.clone();
    overlay::install(mtm);
    overlay::set_sink(Rc::new(move |s| sink.borrow_mut().push(s)));
    point::install(mtm);
    // The island parked with the overlay (Mode::Check puts it at -40 000, -40 000): the same display geometry.
    let real = Place::of(&screen);
    let place = Place { center_x: -40_000.0 + (real.center_x - frame.origin.x), top: -40_000.0 + h, ..real };
    let mut host = Host::new(mtm, place, true);
    host.clock = Some(0.0);
    HOST.with(|hst| *hst.borrow_mut() = Some(host));
    crate::mac::island::set_sink(Box::new(|_| {}));

    // Window properties.
    let panel = overlay::panel().expect("overlay panel");
    c.check(panel.title().to_string() == drawing::OVERLAY_WINDOW_TITLE, "the overlay carries the capture filters' title");
    c.check(panel.sharingType() == NSWindowSharingType::None, "the overlay is unshared by default (Show Islands on Streams off)");
    c.check(panel.level() == NSPopUpMenuWindowLevel + 1, "the overlay sits above menus");
    let pointer = point::panel().expect("pointer panel");
    c.check(pointer.title().to_string() == drawing::POINTER_WINDOW_TITLE && pointer.sharingType() == NSWindowSharingType::None && pointer.ignoresMouseEvents(), "the pointing window is titled for the filters, unshared by default, click-through");

    // Opening on a fresh island: the island grows its keys and the light runs from its flares (recon 17:54 crash).
    let opened = overlay::open_on(screen.clone(), Mode::Check, mtm).is_ok();
    c.check(opened && overlay::is_open(), "drawing mode opens");
    c.check(!panel.ignoresMouseEvents(), "the open overlay takes every click");
    advance(1.2);
    let (class, light) = with_host(|h| {
        let now = h.now();
        (h.island.class(), h.island.frame(now).light)
    })
    .unwrap();
    c.check(class == Class::Drawing, "the island shows drawing mode's keys");
    let (island_level, control_level) = with_host(|h| (h.panels().0.level(), h.panels().1.level())).unwrap();
    c.check(island_level == NSPopUpMenuWindowLevel + 2 && control_level == NSPopUpMenuWindowLevel + 3, &format!("while drawing, the island and its click target sit above the overlay ({island_level}, {control_level})"));
    c.check(!overlay::holds_still(), "with no menu open, drawing mode is live (no still)");
    let pointer_level = point::panel().map(|p| p.level());
    c.check(pointer_level == Some(overlay::LEVEL), "while drawing, pointing sits with the overlay, above menus");
    c.check((light.run - 1.0).abs() < 1e-6 && light.opacity > 0.0, "the light has run round the screen on the island's clock");
    let track_ok = with_host(|h| h.island.track(h.now()).len() >= 1).unwrap_or(false);
    c.check(track_ok, "the island's track is never empty");

    // A stroke away from the island.
    let n = panel.windowNumber();
    let a = NSPoint::new(w * 0.3, h * 0.5);
    panel.sendEvent(&mouse(NSEventType::LeftMouseDown, a, n));
    panel.sendEvent(&mouse(NSEventType::LeftMouseDragged, NSPoint::new(a.x + 80.0, a.y - 30.0), n));
    panel.sendEvent(&mouse(NSEventType::LeftMouseDragged, NSPoint::new(a.x + 160.0, a.y + 10.0), n));
    panel.sendEvent(&mouse(NSEventType::LeftMouseUp, NSPoint::new(a.x + 160.0, a.y + 10.0), n));
    c.check(overlay::strokes().len() == 1, "a drag draws one stroke");
    advance(0.5);
    let inked = with_host(|h| matches!(h.island.scene(), Scene::Drawing { ink: true, .. })).unwrap_or(false);
    c.check(inked, "with ink, the island's undo and clear light up");
    // Undo is an ear target now.
    let undo_target = with_host(|h| h.island.targets().iter().any(|(t, _)| *t == Target::DrawUndo)).unwrap_or(false);
    c.check(undo_target, "undo is a target once there is ink");

    // Send by a tap through the overlay (where a trackpad tap lands when the click target is not there).
    let sent = tap(&panel, &Target::DrawSend);
    c.check(sent, "Send is a target");
    let got = spin_until(3.0, || !shots.borrow().is_empty());
    c.check(got, "a tap on Send through the overlay sends the marked screenshot");
    if let Some(shot) = shots.borrow().first() {
        c.check(shot.strokes == 1 && shot.png.len() > 1000 && shot.width == pw, &format!("the screenshot carries the stroke ({shot:?})"));
        std::fs::write(dir.join("draw-sent.png"), &shot.png).ok();
    }
    advance(0.3);
    // The flash is the confirmation: the keys retract with the light, no "Attached" stage.
    let gone = with_host(|h| matches!(h.island.scene(), Scene::Rest)).unwrap_or(false);
    c.check(gone, "after Send the keys simply retract");
    advance(3.0);
    spin_until(1.0, overlay::at_rest);
    c.check(overlay::at_rest(), "after Send the light runs back and the overlay goes out");

    // Send by a tap on the island's click target.
    shots.borrow_mut().clear();
    let _ = overlay::open_on(screen.clone(), Mode::Check, mtm);
    advance(1.2);
    let control = with_host(|h| h.panels().1).unwrap();
    let tapped = tap(&control, &Target::DrawSend);
    let got = spin_until(3.0, || !shots.borrow().is_empty());
    c.check(tapped && got, "a tap on Send through the island's click target sends too");
    advance(3.0);
    spin_until(1.0, overlay::at_rest);

    // Send mid-motion: the ears change for a screenshot while drawing; Send stays where it is and takes the tap.
    shots.borrow_mut().clear();
    let _ = overlay::open_on(screen.clone(), Mode::Check, mtm);
    advance(1.2);
    crate::mac::island::apply(Event::Command(crate::island::CommandCue::Done { id: None, kind: crate::island::CommandKind::LookAtThis, label: "Screenshot".into() }));
    advance(0.05);
    let moving = with_host(|h| h.island.needs_frames(h.now()) == crate::island::object::FrameNeed::Moving).unwrap_or(false);
    let tapped = tap(&panel, &Target::DrawSend);
    let got = spin_until(3.0, || !shots.borrow().is_empty());
    c.check(moving && tapped && got, "Send takes a tap while the ears change");
    advance(3.0);
    spin_until(1.0, overlay::at_rest);

    // Esc closes; the light runs back; nothing stays.
    let _ = overlay::open_on(screen.clone(), Mode::Check, mtm);
    advance(1.2);
    panel.sendEvent(&key(53, "\u{1b}", NSEventModifierFlags::empty(), panel.windowNumber()));
    spin_until(0.5, || !overlay::is_open());
    c.check(!overlay::is_open(), "Esc closes drawing mode");
    advance(2.0);
    spin_until(1.0, overlay::at_rest);
    c.check(overlay::at_rest(), "closed, nothing of drawing mode is alive");
    let rest = with_host(|h| h.island.class() == Class::Rest).unwrap_or(false);
    c.check(rest, "the island retracts with it");

    // ⌘Z undoes.
    let _ = overlay::open_on(screen.clone(), Mode::Check, mtm);
    advance(1.2);
    panel.sendEvent(&mouse(NSEventType::LeftMouseDown, a, n));
    panel.sendEvent(&mouse(NSEventType::LeftMouseDragged, NSPoint::new(a.x + 50.0, a.y), n));
    panel.sendEvent(&mouse(NSEventType::LeftMouseUp, NSPoint::new(a.x + 50.0, a.y), n));
    panel.sendEvent(&key(6, "z", NSEventModifierFlags::Command, panel.windowNumber()));
    c.check(overlay::strokes().is_empty(), "⌘Z undoes the stroke");
    overlay::close(mtm);
    advance(2.0);
    spin_until(1.0, overlay::at_rest);

    let (island_level, control_level) = with_host(|h| (h.panels().0.level(), h.panels().1.level())).unwrap();
    c.check(island_level == NSMainMenuWindowLevel + 3 && control_level == NSMainMenuWindowLevel + 4, "closed, the island is back under menus");
    c.check(point::panel().map(|p| p.level()) == Some(NSMainMenuWindowLevel + 2), "closed, pointing is back under the island");

    // Over another app's open menu: the overlay goes up waiting for a still, holds it under the ink, and Send marks
    // the still (a dark menu on a white screen), not the screen now (the stand-in capture: the light page).
    let (sw, sh) = (pw as usize, ph as usize);
    let menu_px = (sw * 6 / 10, sh * 3 / 10, sw / 8, sh / 4);
    let mut bgra = vec![250u8; sw * sh * 4];
    for y in menu_px.1..menu_px.1 + menu_px.3 {
        for x in menu_px.0..menu_px.0 + menu_px.2 {
            bgra[(y * sw + x) * 4..(y * sw + x) * 4 + 3].copy_from_slice(&[30, 30, 30]);
        }
    }
    let still_bytes = std::sync::Arc::new(bgra);
    let stills = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (bytes, count) = (still_bytes.clone(), stills.clone());
    drawing::set_still_grabber(move |_| {
        count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        std::thread::sleep(std::time::Duration::from_millis(60));
        Ok((pw, ph, (*bytes).clone()))
    });
    let menu_center = |shot: &MarkedShot| -> Option<u8> {
        let mut reader = png::Decoder::new(std::io::Cursor::new(&shot.png)).read_info().ok()?;
        let mut buf = vec![0; reader.output_buffer_size()?];
        let info = reader.next_frame(&mut buf).ok()?;
        let (x, y) = (menu_px.0 + menu_px.2 / 2, menu_px.1 + menu_px.3 / 2);
        let channels = info.color_type.samples();
        Some(buf[(y * info.width as usize + x) * channels])
    };
    shots.borrow_mut().clear();
    let opened = overlay::open_held(screen.clone(), Mode::Check, Hold::Grab, mtm).is_ok();
    c.check(opened && overlay::awaiting_still() && !overlay::holds_still(), "over another app's menu the overlay goes up first, without the keyboard, waiting for its still");
    let held = spin_until(3.0, overlay::holds_still);
    c.check(held && !overlay::awaiting_still(), "the still arrives and goes under the ink");
    advance(1.2);
    panel.sendEvent(&mouse(NSEventType::LeftMouseDown, a, n));
    panel.sendEvent(&mouse(NSEventType::LeftMouseDragged, NSPoint::new(a.x + 60.0, a.y + 20.0), n));
    panel.sendEvent(&mouse(NSEventType::LeftMouseUp, NSPoint::new(a.x + 60.0, a.y + 20.0), n));
    let sent = tap(&panel, &Target::DrawSend);
    let got = spin_until(3.0, || !shots.borrow().is_empty());
    let center = shots.borrow().first().and_then(menu_center);
    c.check(sent && got && center.is_some_and(|v| v < 80), &format!("Send marks the held still with the menu on it (menu centre {center:?})"));
    if let Some(shot) = shots.borrow().first() {
        c.check(shot.strokes == 1 && shot.width == pw && shot.height == ph, &format!("the held still's shot carries the stroke at native size ({shot:?})"));
        std::fs::write(dir.join("draw-sent-over-menu.png"), &shot.png).ok();
    }
    advance(3.0);
    spin_until(1.0, overlay::at_rest);
    c.check(overlay::at_rest(), "after Send over a menu, the still fades with the light and nothing stays");

    // Esc while the still is still being taken: drawing mode closes and the late still is dropped.
    let _ = overlay::open_held(screen.clone(), Mode::Check, Hold::Grab, mtm);
    panel.sendEvent(&key(53, "\u{1b}", NSEventModifierFlags::empty(), panel.windowNumber()));
    spin_until(0.5, || !overlay::is_open());
    std::thread::sleep(std::time::Duration::from_millis(120));
    spin_until(0.2, || false);
    c.check(!overlay::is_open() && !overlay::holds_still(), "Esc before the still arrives closes, and the late still is dropped");
    advance(2.0);
    spin_until(1.0, overlay::at_rest);

    // Seeing turned off while drawing over a held still: Send attaches nothing.
    let seeing = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let on = seeing.clone();
    drawing::set_seeing(move || on.load(std::sync::atomic::Ordering::SeqCst));
    shots.borrow_mut().clear();
    let _ = overlay::open_held(screen.clone(), Mode::Check, Hold::Grab, mtm);
    spin_until(3.0, overlay::holds_still);
    advance(1.2);
    seeing.store(false, std::sync::atomic::Ordering::SeqCst);
    tap(&panel, &Target::DrawSend);
    spin_until(1.0, || !shots.borrow().is_empty());
    c.check(shots.borrow().is_empty(), "Seeing off since the still was taken: Send attaches nothing");
    advance(3.0);
    spin_until(1.0, overlay::at_rest);
    // And with Seeing off, a menu still is neither taken nor held.
    stills.store(0, std::sync::atomic::Ordering::SeqCst);
    still::menu_opened_on(screen.CGDirectDisplayID(), std::time::Duration::ZERO);
    spin_until(0.3, || false);
    still::menu_closed();
    still::chosen_in_menu();
    let _ = overlay::open_on(screen.clone(), Mode::Check, mtm);
    c.check(stills.load(std::sync::atomic::Ordering::SeqCst) == 0 && !overlay::holds_still() && !overlay::awaiting_still(), "Seeing off: no still of the menu is taken or held");
    overlay::close(mtm);
    advance(2.0);
    spin_until(1.0, overlay::at_rest);
    seeing.store(true, std::sync::atomic::Ordering::SeqCst);

    // Heimdall's own menu: a still once it opens; Drawing Mode chosen in it holds that still at once.
    let display = screen.CGDirectDisplayID();
    stills.store(0, std::sync::atomic::Ordering::SeqCst);
    still::menu_opened_on(display, std::time::Duration::ZERO);
    spin_until(2.0, || stills.load(std::sync::atomic::Ordering::SeqCst) > 0);
    spin_until(0.3, || false);
    still::menu_closed();
    still::chosen_in_menu();
    shots.borrow_mut().clear();
    let _ = overlay::open_on(screen.clone(), Mode::Check, mtm);
    c.check(overlay::holds_still() && !overlay::awaiting_still(), "Drawing Mode chosen in Heimdall's menu holds the menu's still at once");
    advance(1.2);
    let sent = tap(&panel, &Target::DrawSend);
    let got = spin_until(3.0, || !shots.borrow().is_empty());
    let center = shots.borrow().first().and_then(menu_center);
    c.check(sent && got && center.is_some_and(|v| v < 80), &format!("Send from Heimdall's menu marks the menu's still (menu centre {center:?})"));
    advance(3.0);
    spin_until(1.0, overlay::at_rest);
    // Chosen while its still is being taken (asked for while the menu was open): the overlay waits for it.
    stills.store(0, std::sync::atomic::Ordering::SeqCst);
    still::menu_opened_on(display, std::time::Duration::ZERO);
    spin_until(1.0, || stills.load(std::sync::atomic::Ordering::SeqCst) > 0);
    still::menu_closed();
    still::chosen_in_menu();
    let _ = overlay::open_on(screen.clone(), Mode::Check, mtm);
    c.check(overlay::awaiting_still(), "chosen before the menu's still is taken, the overlay waits for it");
    let held = spin_until(3.0, overlay::holds_still);
    c.check(held, "and holds it when it arrives");
    overlay::close(mtm);
    advance(2.0);
    spin_until(1.0, overlay::at_rest);
    // Chosen before its still was even asked for: that still shows the screen after the menu closed, so none is held.
    still::menu_opened_on(display, std::time::Duration::from_millis(100));
    still::menu_closed();
    still::chosen_in_menu();
    let _ = overlay::open_on(screen.clone(), Mode::Check, mtm);
    let waited = overlay::awaiting_still();
    let answered = spin_until(3.0, || !overlay::awaiting_still());
    c.check(waited && answered && !overlay::holds_still(), "a still asked for after the menu closed is not held (drawing mode goes on live)");
    overlay::close(mtm);
    advance(2.0);
    spin_until(1.0, overlay::at_rest);
    // The next open with no menu chosen and none open is live again.
    let _ = overlay::open_on(screen.clone(), Mode::Check, mtm);
    c.check(!overlay::holds_still() && !overlay::awaiting_still(), "after that, drawing mode is live again");
    overlay::close(mtm);
    advance(2.0);
    spin_until(1.0, overlay::at_rest);

    // Pointing: the marks are on the island's clock (arrive, hold, fade without a voice).
    match point::show(&crate::drawing::parse("size 1728 1117\nbox 600 400 200 80 \"Here\"").expect("a spec"), 0.0, mtm) {
        Ok(line) => c.note(&line),
        Err(e) => c.check(false, &format!("pointing shows ({e})")),
    }
    advance(0.5);
    c.check(point::shown_container().is_some_and(|l| l.opacity() > 0.99), "the marks arrive with the island");
    advance(5.0);
    c.check(point::at_rest(), "without a voice the marks fade and their window goes out");
    std::fs::write(dir.join("drawing-checks.txt"), &c.report).ok();
    c.failed
}

/// `heimdall-ui-demo --drawing-over-menu <other|own> [secs]`: a live check on the user's display (brief, no pointer
/// events): real drawing mode opens over an open menu and every change is printed with its uptime, for a recorder in
/// another process to line up with its frames. `other`: another app's menu must already be open; the overlay goes up,
/// takes its still and then the keyboard. `own`: this process's status item opens its menu, which is cancelled after
/// 0.8 s, and Drawing Mode is chosen as Heimdall's item would be. The stills come from macOS's `screencapture` (this
/// binary has no Screen Recording path of its own); the windows turn shareable once the still is taken, so the
/// recorder sees what the user sees. Closes after `seconds` and exits.
/// The main display's screen: the one the lab's stills (`screencapture -D 1`) show.
fn main_screen(mtm: MainThreadMarker) -> Option<objc2::rc::Retained<NSScreen>> {
    let main = objc2_core_graphics::CGMainDisplayID();
    NSScreen::screens(mtm).iter().find(|s| s.CGDirectDisplayID() == main)
}

pub(crate) fn live_over_menu(own: bool, seconds: f64) -> ! {
    use objc2_app_kit::{NSStatusBar, NSStatusItem};
    use objc2_foundation::{NSRunLoop, NSRunLoopCommonModes, NSString, NSTimer};
    let mtm = MainThreadMarker::new().expect("--drawing-over-menu runs on the main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    let t = || crate::mac::lab::uptime();
    let grab = move |display: u32| -> drawing::Still {
        let path = std::env::temp_dir().join(format!("heimdall-drawing-over-menu-{}.png", std::process::id()));
        let started = t();
        let ok = std::process::Command::new("/usr/sbin/screencapture").args(["-x", "-D", "1", "-t", "png"]).arg(&path).status().is_ok_and(|s| s.success());
        let png = std::fs::read(&path).map_err(|e| e.to_string());
        let _ = std::fs::remove_file(&path);
        if !ok {
            return Err("screencapture failed".into());
        }
        let png = png?;
        let mut reader = png::Decoder::new(std::io::Cursor::new(&png)).read_info().map_err(|e| e.to_string())?;
        let mut buf = vec![0; reader.output_buffer_size().ok_or("no size")?];
        let info = reader.next_frame(&mut buf).map_err(|e| e.to_string())?;
        let channels = info.color_type.samples();
        let mut bgra = Vec::with_capacity(info.width as usize * info.height as usize * 4);
        for px in buf[..info.width as usize * info.height as usize * channels].chunks_exact(channels) {
            bgra.extend_from_slice(&[px[2], px[1], px[0], 255]);
        }
        println!("{:.3} still taken of display {display}: {}x{} in {:.0} ms", t(), info.width, info.height, (t() - started) * 1000.0);
        // Shareable from now on, before the still goes under the ink (the main queue keeps this order).
        dispatch2::DispatchQueue::main().exec_async(|| crate::mac::streams::set(true));
        Ok((info.width, info.height, bgra))
    };
    drawing::set_still_grabber(grab);
    crate::mac::island::install(mtm);
    crate::mac::island::set_sink(Box::new(|_| {}));
    overlay::install(mtm);
    overlay::set_sink(Rc::new(|s| println!("sent {s:?}")));
    point::install(mtm);
    // The main display: the one `screencapture -D 1` takes (the lab's display).
    let display = objc2_core_graphics::CGMainDisplayID();
    println!("{:.3} start: another app's menu on the display: {}", t(), still::menu_on_display(display));
    // Every change, polled every 10 ms in every run loop mode (also while a menu tracks).
    let last: Rc<RefCell<String>> = Default::default();
    let poll = block2::RcBlock::new(move |_: std::ptr::NonNull<NSTimer>| {
        let key = overlay::panel().is_some_and(|p| p.isKeyWindow());
        let now = format!(
            "open {} awaiting {} still {} key {} active {} other-menu {}",
            overlay::is_open(),
            overlay::awaiting_still(),
            overlay::holds_still(),
            key,
            NSApplication::sharedApplication(MainThreadMarker::new().unwrap()).isActive(),
            still::menu_on_display(display)
        );
        if *last.borrow() != now {
            println!("{:.3} {now}", t());
            *last.borrow_mut() = now;
        }
    });
    let timer = unsafe { NSTimer::timerWithTimeInterval_repeats_block(0.01, true, &poll) };
    unsafe { NSRunLoop::mainRunLoop().addTimer_forMode(&timer, NSRunLoopCommonModes) };
    let started = t();
    if own {
        let item: objc2::rc::Retained<NSStatusItem> = NSStatusBar::systemStatusBar().statusItemWithLength(-2.0);
        let menu = objc2_app_kit::NSMenu::new(mtm);
        for title in ["Drawing Mode", "Probe Two", "Probe Three"] {
            // SAFETY: a plain item without an action.
            let entry = unsafe { objc2_app_kit::NSMenuItem::initWithTitle_action_keyEquivalent(objc2::MainThreadOnly::alloc(mtm), &NSString::from_str(title), None, &NSString::from_str("")) };
            menu.addItem(&entry);
        }
        item.setMenu(Some(&menu));
        if let Some(b) = item.button(mtm) {
            b.setTitle(&NSString::from_str("H"));
        }
        let cancel_menu = menu.clone();
        let cancel = block2::RcBlock::new(move |_: std::ptr::NonNull<NSTimer>| {
            println!("{:.3} the menu is cancelled (as a click on its item closes it)", t());
            cancel_menu.cancelTracking();
        });
        let timer = unsafe { NSTimer::timerWithTimeInterval_repeats_block(0.8, false, &cancel) };
        unsafe { NSRunLoop::mainRunLoop().addTimer_forMode(&timer, NSRunLoopCommonModes) };
        let open = block2::RcBlock::new(move |_: std::ptr::NonNull<NSTimer>| {
            let mtm = MainThreadMarker::new().unwrap();
            println!("{:.3} the menu opens", t());
            still::menu_opened(true, mtm);
            if let Some(b) = item.button(mtm) {
                // SAFETY: the status button opens its menu (and blocks until it closes).
                unsafe { b.performClick(None) };
            }
            println!("{:.3} the menu closed; Drawing Mode chosen in it", t());
            still::menu_closed();
            still::chosen_in_menu();
            let r = main_screen(mtm).ok_or_else(|| "no display".to_string()).and_then(|s| overlay::open_on(s, Mode::Live, mtm));
            println!("{:.3} drawing mode opened: {:?}", t(), r.map(|(i, _)| i));
        });
        let timer = unsafe { NSTimer::timerWithTimeInterval_repeats_block(0.3, false, &open) };
        unsafe { NSRunLoop::mainRunLoop().addTimer_forMode(&timer, NSRunLoopCommonModes) };
    } else {
        // After launch has settled (an app is activated as it finishes launching; Heimdall has been running).
        let open = block2::RcBlock::new(move |_: std::ptr::NonNull<NSTimer>| {
            let mtm = MainThreadMarker::new().unwrap();
            println!("{:.3} opening; this app active: {}", t(), NSApplication::sharedApplication(mtm).isActive());
            let r = main_screen(mtm).ok_or_else(|| "no display".to_string()).and_then(|s| overlay::open_on(s, Mode::Live, mtm));
            println!("{:.3} drawing mode opened: {:?}", t(), r.map(|(i, _)| i));
        });
        let timer = unsafe { NSTimer::timerWithTimeInterval_repeats_block(0.6, false, &open) };
        unsafe { NSRunLoop::mainRunLoop().addTimer_forMode(&timer, NSRunLoopCommonModes) };
    }
    let end = block2::RcBlock::new(move |_: std::ptr::NonNull<NSTimer>| {
        let mtm = MainThreadMarker::new().unwrap();
        println!("{:.3} closing after {:.1} s", t(), t() - started);
        overlay::close(mtm);
    });
    let timer = unsafe { NSTimer::timerWithTimeInterval_repeats_block(seconds + 0.6, false, &end) };
    unsafe { NSRunLoop::mainRunLoop().addTimer_forMode(&timer, NSRunLoopCommonModes) };
    let quit = block2::RcBlock::new(move |_: std::ptr::NonNull<NSTimer>| {
        crate::mac::overlay_space::release();
        std::process::exit(0);
    });
    let timer = unsafe { NSTimer::timerWithTimeInterval_repeats_block(seconds + 2.1, false, &quit) };
    unsafe { NSRunLoop::mainRunLoop().addTimer_forMode(&timer, NSRunLoopCommonModes) };
    app.run();
    std::process::exit(0)
}
