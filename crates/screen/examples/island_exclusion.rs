//! Show Islands on Streams leaves Heimdall's own captures clean: a window titled like the island, shared
//! (`ReadOnly`, as with the switch on) and tinted 3 % red behind the notch for about a second, is absent from
//! Heimdall's grab (`native_grabber`, the filter the recorder uses too), present in the same grab under another title
//! (the control) and in macOS's `screencapture` (as others see it). Needs Screen Recording; prints the pixels, exits 1 on a failure.
//!
//! `cargo run -p screen --example island_exclusion`

#[cfg(target_os = "macos")]
fn main() {
    use objc2::MainThreadOnly;
    use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSColor, NSMainMenuWindowLevel, NSPanel, NSScreen, NSWindowSharingType, NSWindowStyleMask};
    use objc2_foundation::{MainThreadMarker, NSDate, NSPoint, NSRect, NSRunLoop, NSSize, NSString};

    let mtm = MainThreadMarker::new().expect("main thread");
    NSApplication::sharedApplication(mtm).setActivationPolicy(NSApplicationActivationPolicy::Prohibited);
    let screen = NSScreen::screens(mtm).firstObject().expect("a display");
    let (frame, scale) = (screen.frame(), screen.backingScaleFactor());
    let side = 24.0;
    let (x, y) = (frame.origin.x + frame.size.width / 2.0 - side / 2.0, frame.origin.y + frame.size.height - side);
    let panel = NSPanel::initWithContentRect_styleMask_backing_defer(
        NSPanel::alloc(mtm),
        NSRect::new(NSPoint::new(x, y), NSSize::new(side, side)),
        NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel,
        NSBackingStoreType::Buffered,
        false,
    );
    unsafe { panel.setReleasedWhenClosed(false) };
    panel.setSharingType(NSWindowSharingType::ReadOnly);
    panel.setOpaque(false);
    panel.setHasShadow(false);
    panel.setIgnoresMouseEvents(true);
    panel.setBackgroundColor(Some(&NSColor::colorWithSRGBRed_green_blue_alpha(1.0, 0.0, 0.0, 0.03)));
    panel.setLevel(NSMainMenuWindowLevel + 3);
    let spin = |s: f64| NSRunLoop::currentRunLoop().runUntilDate(&NSDate::dateWithTimeIntervalSinceNow(s));
    let display = objc2_core_graphics::CGMainDisplayID();
    let grab = screen::native_grabber().expect("a grabber on macOS");
    // The window's centre in the grab's pixels (the primary display, top-left origin).
    let red = |title: &str| -> Option<u8> {
        panel.setTitle(&NSString::from_str(title));
        spin(0.2);
        let f = grab(display).map_err(|e| eprintln!("grab: {e:#}")).ok()?;
        let s = f.width as f64 / frame.size.width;
        let (px, py) = (((x - frame.origin.x + side / 2.0) * s) as usize, ((side / 2.0) * s) as usize);
        Some(f.rgb[(py * f.width as usize + px) * 3])
    };
    let before = red(screen::EXCLUDED_WINDOW_TITLE);
    panel.orderFrontRegardless();
    let island = red(screen::EXCLUDED_WINDOW_TITLE);
    let control = red("Heimdall Exclusion Control");
    panel.setTitle(&NSString::from_str(screen::EXCLUDED_WINDOW_TITLE));
    let dir = std::env::temp_dir().join(format!("island-exclusion-{}", std::process::id()));
    std::fs::create_dir_all(&dir).ok();
    let out = dir.join("shared.png");
    let region = format!("-R{},0,{},{}", (x - frame.origin.x).round() as i64, side as i64, side as i64);
    let shared = std::process::Command::new("/usr/sbin/screencapture").args(["-x", &region]).arg(&out).status().is_ok_and(|s| s.success())
        .then(|| centre_red(&out))
        .flatten();
    panel.orderOut(None);
    std::fs::remove_dir_all(&dir).ok();
    println!("scale {scale}; red at the window's centre: nothing shown {before:?}, the island's title {island:?}, another title {control:?}, screencapture {shared:?}");
    let mut failed = vec![];
    if !(before.is_some() && island == before) {
        failed.push("Heimdall's grab shows a shared window titled like the island");
    }
    if !matches!((before, control), (Some(b), Some(c)) if c >= b.saturating_add(3)) {
        failed.push("the control: the same window under another title shows in Heimdall's grab");
    }
    if !matches!((before, shared), (Some(b), Some(c)) if c >= b.saturating_add(3)) {
        failed.push("macOS's screencapture shows the shared island");
    }
    for f in &failed {
        eprintln!("FAILED: {f}");
    }
    if failed.is_empty() {
        println!("ok: shared, the island stays out of Heimdall's own captures; the control shows the grab would see it");
    }
    std::process::exit(if failed.is_empty() { 0 } else { 1 });
}

/// The red of a PNG's centre pixel.
#[cfg(target_os = "macos")]
fn centre_red(path: &std::path::Path) -> Option<u8> {
    let mut decoder = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(path).ok()?));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buf).ok()?;
    let (w, h, n) = (info.width as usize, info.height as usize, info.color_type.samples());
    buf.get(((h / 2) * w + w / 2) * n).copied()
}

#[cfg(not(target_os = "macos"))]
fn main() {}
