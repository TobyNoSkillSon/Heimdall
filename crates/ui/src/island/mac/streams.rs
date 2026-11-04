//! Show Islands on Streams ("whenever we are streaming, for example on Discord, I want
//! people to be able to see the dynamic island"). Off by default: the island, its click target, drawing mode's overlay,
//! the pointing window and the drop target's drawn window are unshared (`NSWindowSharingType::None`), so Discord, OBS, QuickTime and screenshots
//! never show them. On: they are shared read-only like any window, live, the windows that exist and those made later.
//!
//! Heimdall's own captures exclude these windows either way (crates/screen's filter, by this process and the
//! windows' titles), so the archive and the agent's screenshots never show the island.
//! The floating islands follow the same switch through the helper's `streams.json` (crates/app `islands::streams`).

use std::sync::atomic::{AtomicBool, Ordering};

use objc2_app_kit::NSWindowSharingType;

static ON: AtomicBool = AtomicBool::new(false);

/// The sharing type every island window takes now; windows made later read it too.
pub(crate) fn sharing() -> NSWindowSharingType {
    if ON.load(Ordering::Relaxed) { NSWindowSharingType::ReadOnly } else { NSWindowSharingType::None }
}

/// Shows the islands on streams (true) or hides them again. A change re-applies to every live island window; the same
/// value again touches nothing (a lab instance's own `lab::shareable` stays).
pub(crate) fn set(on: bool) {
    if ON.swap(on, Ordering::Relaxed) == on {
        return;
    }
    if !apply() {
        // The island's host was busy (borrowed): once more right after, with whatever the switch says then.
        dispatch2::DispatchQueue::main().exec_async(|| {
            apply();
        });
    }
}

/// The switch onto every live island window. False when the island's host exists but was busy, or the drop target was.
fn apply() -> bool {
    let sharing = sharing();
    if let Some(panel) = super::draw::overlay::panel() {
        panel.setSharingType(sharing);
    }
    if let Some(panel) = super::draw::point::panel() {
        panel.setSharingType(sharing);
    }
    // The drop target's drawn window (its catcher draws nothing and stays unshared).
    if let Some(panel) = super::drop_target::panel() {
        panel.setSharingType(sharing);
    }
    let applied = super::island::host::with_host(|h| {
        let (panel, control) = h.panels();
        panel.setSharingType(sharing);
        control.setSharingType(sharing);
    });
    let drop_busy = super::drop_target::TARGET.with(|t| t.try_borrow().is_err());
    (applied.is_some() || super::island::host::HOST.with(|h| h.try_borrow().is_ok())) && !drop_busy
}

/// `heimdall-ui-demo --streams <dir>`: the switch over every island window (the island, its click target, drawing
/// mode's overlay, pointing, the drop target's drawn window), off by default, live both ways, and an island made after the switch; then the pixels: that
/// island window (its level, in the overlay space) shown 24 pt square under the notch, tinted 3 % red, captured by
/// macOS's `screencapture` as a window (`-l`) and as part of the display (`-R`): in the capture with the switch on,
/// absent with it off. It sits behind the notch's hardware (on a display without one, a 3 % tint in the menu bar) for
/// about a second. Writes `streams-checks.txt` and the captures to `dir`.
pub(crate) fn check(dir: &std::path::Path) -> Vec<String> {
    use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy, NSColor, NSScreen, NSWindow};

    use super::island::checks::{spin, Checks};
    use super::island::host::{Host, Place, HOST};

    let mtm = objc2_foundation::MainThreadMarker::new().expect("the streams check runs on the main thread");
    NSApplication::sharedApplication(mtm).setActivationPolicy(NSApplicationActivationPolicy::Prohibited);
    std::fs::create_dir_all(dir).ok();
    let mut c = Checks::new();
    super::island::set_sink(Box::new(|_| {}));
    super::draw::overlay::install(mtm);
    super::draw::point::install(mtm);
    super::island::install_offscreen(mtm);
    super::drop_target::install(mtm, false);
    let windows = || -> Vec<(&'static str, objc2::rc::Retained<NSWindow>)> {
        let mut all = vec![];
        if let Some((panel, control)) = super::island::host::with_host(|h| h.panels()) {
            all.push(("island", objc2::rc::Retained::into_super(panel)));
            all.push(("click target", objc2::rc::Retained::into_super(control)));
        }
        if let Some(p) = super::draw::overlay::panel() {
            all.push(("drawing overlay", objc2::rc::Retained::into_super(objc2::rc::Retained::into_super(p))));
        }
        if let Some(p) = super::draw::point::panel() {
            all.push(("pointing", objc2::rc::Retained::into_super(p)));
        }
        if let Some(p) = super::drop_target::panel() {
            all.push(("drop target", objc2::rc::Retained::into_super(p)));
        }
        all
    };
    let all_are = |want: NSWindowSharingType| windows().iter().all(|(_, w)| w.sharingType() == want);
    c.check(windows().len() == 5, "the island, its click target, drawing mode's overlay, pointing and the drop target are installed");
    c.check(all_are(NSWindowSharingType::None), "off by default: every island window is unshared");
    set(true);
    c.check(all_are(NSWindowSharingType::ReadOnly), "on: every live island window is shared read-only at once");
    // A host made after the switch (as after a display change) reads it.
    let mut host = Host::new(mtm, Place::offscreen(), true);
    host.clock = Some(0.0);
    HOST.with(|h| *h.borrow_mut() = Some(host));
    c.check(all_are(NSWindowSharingType::ReadOnly), "on: an island made after the switch is shared too");
    set(false);
    c.check(all_are(NSWindowSharingType::None), "off again: every island window is unshared at once");
    // The switch while the island's host is busy (a frame in flight): it lands right after.
    HOST.with(|h| {
        let _busy = h.borrow_mut();
        set(true);
    });
    spin(0.05);
    c.check(all_are(NSWindowSharingType::ReadOnly), "on while the island is busy: applied right after");
    set(false);
    // The same value again touches nothing: a lab instance's shared windows stay shared.
    let lab = windows();
    lab[0].1.setSharingType(NSWindowSharingType::ReadOnly);
    set(false);
    c.check(lab[0].1.sharingType() == NSWindowSharingType::ReadOnly, "the same value again leaves the windows as they are");
    lab[0].1.setSharingType(NSWindowSharingType::None);

    // The pixels, through macOS's own capture.
    let Some(screen) = NSScreen::screens(mtm).firstObject() else {
        c.check(false, "a display to capture");
        return c.failed;
    };
    let place = Place::of(&screen);
    let frame = screen.frame();
    let side = 24.0;
    // The island window itself (made after the switch above, by the island's own `make_panel`), shown for the capture.
    let Some((probe, _)) = super::island::host::with_host(|h| h.panels()) else {
        c.check(false, "the island window to capture");
        return c.failed;
    };
    probe.setBackgroundColor(Some(&NSColor::colorWithSRGBRed_green_blue_alpha(1.0, 0.0, 0.0, 0.03)));
    probe.setAlphaValue(1.0);
    let (x, y) = (place.center_x - side / 2.0, frame.origin.y + frame.size.height - side);
    probe.setFrame_display(super::util::rect(x, y, side, side), true);
    probe.orderFrontRegardless();
    super::overlay_space::join(&probe, crate::island::ISLAND_WINDOW_TITLE);
    let number = probe.windowNumber();
    // `-R` takes global points from the primary display's top-left.
    let region = format!("{},{},{},{}", (x - frame.origin.x).round() as i64, 0, side as i64, side as i64);
    let capture = |name: &str| -> (Option<[u8; 4]>, Option<[u8; 4]>) {
        spin(0.15);
        let window = dir.join(format!("{name}-window.png"));
        let display = dir.join(format!("{name}-display.png"));
        let _ = std::fs::remove_file(&window);
        let shot = |args: &[&str], out: &std::path::Path| {
            let ok = std::process::Command::new("/usr/sbin/screencapture").args(args).arg(out).stderr(std::process::Stdio::null()).status().is_ok_and(|s| s.success());
            ok.then(|| center(out)).flatten()
        };
        (shot(&["-x", "-o", &format!("-l{number}")], &window), shot(&["-x", &format!("-R{region}")], &display))
    };
    let off = capture("off");
    set(true);
    let on = capture("on");
    set(false);
    let off_again = capture("off-again");
    probe.orderOut(None);
    super::overlay_space::release();
    let red = |p: Option<[u8; 4]>| p.map(|p| p[0] as i32);
    c.note(&format!("window capture centre (RGBA): off {:?}, on {:?}, off again {:?}", off.0, on.0, off_again.0));
    c.note(&format!("display capture centre (RGBA): off {:?}, on {:?}, off again {:?}", off.1, on.1, off_again.1));
    c.check(on.0.is_some_and(|p| p[0] > 0 && p[3] > 0), "on: the window capture holds the island window's own pixels");
    c.check(off.0.is_none() && off_again.0.is_none(), "off: macOS cannot capture the island window at all");
    let (a, b, z) = (red(off.1), red(on.1), red(off_again.1));
    c.check(matches!((a, b, z), (Some(a), Some(b), Some(z)) if b >= a + 3 && b >= z + 3), "on: the display capture shows the island window's tint over the menu bar");
    c.check(matches!((a, z), (Some(a), Some(z)) if (a - z).abs() <= 1), "off: the display capture is the menu bar alone, before and after");
    std::fs::write(dir.join("streams-checks.txt"), &c.report).ok();
    c.failed
}

/// The centre pixel of a PNG, as 8-bit RGBA.
fn center(path: &std::path::Path) -> Option<[u8; 4]> {
    let mut decoder = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(path).ok()?));
    decoder.set_transformations(png::Transformations::normalize_to_color8() | png::Transformations::ALPHA);
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buf).ok()?;
    let (w, h) = (info.width as usize, info.height as usize);
    let channels = info.color_type.samples();
    let i = ((h / 2) * w + w / 2) * channels;
    match channels {
        4 => Some([buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]),
        2 => Some([buf[i], buf[i], buf[i], buf[i + 1]]),
        _ => None,
    }
}
