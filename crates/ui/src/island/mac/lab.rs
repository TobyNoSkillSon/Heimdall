//! Lab-only hooks for `heimdall --lab-scenario` (the post-install check, lab/postinstall/): a frame log, the island's
//! windows made capturable by the lab's own recorder, taps through the click target's real event path, drawing mode
//! parked off every display, and the state a scenario asserts. A normal launch never calls anything here.

use std::io::Write as _;
use std::path::Path;

use objc2_app_kit::{NSApplication, NSEventType, NSScreen, NSWindowSharingType};
use objc2_foundation::{MainThreadMarker, NSPoint};

use super::draw::overlay::{self, Mode};
use super::island::host::with_host;
use crate::island::object::{Class, Event, FrameModel, FrameNeed, Target};

pub(crate) fn uptime() -> f64 {
    let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
    unsafe { libc::clock_gettime(libc::CLOCK_UPTIME_RAW, &mut ts) };
    ts.tv_sec as f64 + ts.tv_nsec as f64 / 1e9
}

/// The island's windows become shareable (`ReadOnly`), so a ScreenCaptureKit filter that names them can record them.
/// The installed app keeps them unshared (`None`) unless Show Islands on Streams is on (`streams`); the archive's own
/// capture excludes them by title either way.
pub(crate) fn shareable() {
    with_host(|h| {
        let (panel, control) = h.panels();
        panel.setSharingType(NSWindowSharingType::ReadOnly);
        control.setSharingType(NSWindowSharingType::ReadOnly);
    });
}

/// The recorder's capture makes AppKit build a window-sharing controller (`NSLocalWindowSharingWindowController`, a
/// ViewBridge connection: 15–110 ms on the main thread) the first time a captured window becomes visible. The
/// installed app's island windows are never shared, so a lab instance pays it before its timeline: both windows
/// ordered in at alpha 0, ignoring the mouse (`on`), then out again.
pub(crate) fn capture_prime(on: bool) {
    with_host(|h| {
        let (panel, control) = h.panels();
        // Already showing (a timeline that starts with the island up): leave it shown, the host believes it is.
        if on && panel.isVisible() {
            return;
        }
        for w in [&panel, &control] {
            if on {
                w.setAlphaValue(0.0);
                w.setIgnoresMouseEvents(true);
                w.orderFrontRegardless();
            } else {
                w.orderOut(None);
                w.setAlphaValue(1.0);
            }
        }
        if !on {
            control.setIgnoresMouseEvents(false);
        }
    });
}

/// Starts the frame log at `path`. Returns the island clock's zero on CLOCK_UPTIME_RAW (the clock ScreenCaptureKit
/// stamps its frames with), so recorded frames and logged frames share one time axis.
pub(crate) fn frame_log(path: &Path) -> Result<f64, String> {
    let file = std::fs::File::create(path).map_err(|e| e.to_string())?;
    with_host(|h| {
        let zero = uptime() - h.now();
        let mut w = std::io::BufWriter::new(file);
        let display = h.place.display;
        let (view_flipped, flipped) = h.flipped();
        let rf = { let f = h.tree.root.frame(); [f.origin.x, f.origin.y, f.size.width, f.size.height] };
        let _ = writeln!(
            w,
            "{}",
            serde_json::json!({"kind":"start","uptime_zero":zero,"center_x":h.place.center_x,"top":h.place.top,"display":{"w":display.width,"h":display.height,"scale":display.scale,"notch":display.notch},
                "root_geometry_flipped":flipped,"view_flipped":view_flipped,"root_frame":rf})
        );
        h.lab_log = Some(w);
        watch_runloop();
        zero
    })
    .ok_or_else(|| "the island is not installed".to_string())
}

// Lab timing: the main run loop's busy spans (after waking to before sleeping) over 1 ms, logged as `busy` lines.
mod rl {
    use std::ffi::c_void;
    pub type Callout = extern "C" fn(*mut c_void, usize, *mut c_void);
    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        pub fn CFRunLoopObserverCreate(alloc: *const c_void, activities: usize, repeats: u8, order: isize, callout: Callout, ctx: *mut c_void) -> *mut c_void;
        pub fn CFRunLoopAddObserver(rl: *mut c_void, obs: *mut c_void, mode: *const c_void);
        pub fn CFRunLoopGetMain() -> *mut c_void;
        pub static kCFRunLoopCommonModes: *const c_void;
    }
}

thread_local! {
    static WOKE: std::cell::Cell<f64> = const { std::cell::Cell::new(0.0) };
    static BUSY: std::cell::RefCell<Vec<(f64, f64)>> = const { std::cell::RefCell::new(Vec::new()) };
}

extern "C" fn rl_woke(_o: *mut std::ffi::c_void, _a: usize, _c: *mut std::ffi::c_void) {
    WOKE.with(|w| w.set(uptime()));
}

extern "C" fn rl_sleeps(_o: *mut std::ffi::c_void, _a: usize, _c: *mut std::ffi::c_void) {
    let (a, b) = (WOKE.with(|w| w.get()), uptime());
    if a > 0.0 && b - a > 0.001 {
        BUSY.with(|v| v.borrow_mut().push((a, b)));
    }
}

fn watch_runloop() {
    unsafe {
        let main = rl::CFRunLoopGetMain();
        let woke = rl::CFRunLoopObserverCreate(std::ptr::null(), 1 << 6, 1, isize::MIN, rl_woke, std::ptr::null_mut());
        let sleeps = rl::CFRunLoopObserverCreate(std::ptr::null(), 1 << 5, 1, isize::MAX, rl_sleeps, std::ptr::null_mut());
        rl::CFRunLoopAddObserver(main, woke, rl::kCFRunLoopCommonModes);
        rl::CFRunLoopAddObserver(main, sleeps, rl::kCFRunLoopCommonModes);
    }
}

pub(crate) fn flush_log() {
    let busy = BUSY.with(|v| std::mem::take(&mut *v.borrow_mut()));
    with_host(|h| {
        if let Some(w) = h.lab_log.as_mut() {
            for (a, b) in &busy {
                let _ = writeln!(w, "{}", serde_json::json!({"kind":"busy","a":a,"b":b}));
            }
        }
    });
    with_host(|h| {
        if let Some(w) = h.lab_log.as_mut() {
            let _ = w.flush();
        }
    });
}

pub(crate) fn log_event(w: &mut std::io::BufWriter<std::fs::File>, t: f64, ev: &Event) {
    if matches!(ev, Event::Tick) {
        return;
    }
    let mut text = format!("{ev:?}");
    if text.len() > 240 {
        let mut cut = 240;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
    }
    let _ = writeln!(w, "{}", serde_json::json!({"kind":"event","t":t,"ev":text}));
}

pub(crate) fn log_frame(w: &mut std::io::BufWriter<std::fs::File>, f: &FrameModel, class: Class, need: FrameNeed, draw: f64, shown: bool, speaking: bool, timing: serde_json::Value) {
    let need = match need {
        FrameNeed::None => "none",
        FrameNeed::Loop { .. } => "loop",
        FrameNeed::Moving => "moving",
    };
    let _ = writeln!(
        w,
        "{}",
        serde_json::json!({"kind":"frame","t":f.t,"visible":f.visible,"moving":f.moving,"glow":f.glow,"w":f.shape.w,"h":f.shape.h,"flare":f.shape.flare,
            "target":[f.target.0,f.target.1],"motion":f.motion.0,"class":format!("{class:?}"),"need":need,"draw_ms":draw*1000.0,"shown":shown,"speaking":speaking,"slots":f.slots.len(),"timing":timing})
    );
}

/// A tap (mouse down and up) on `target`, sent to the island's click target window: AppKit's own dispatch and the
/// island's own hit test, no cursor and no CGEvent.
pub(crate) fn tap(target: &Target) -> bool {
    let Some(control) = with_host(|h| h.panels().1) else { return false };
    if !control.isVisible() {
        return false;
    }
    super::island::checks::tap(&control, target)
}

pub(crate) fn targets() -> Vec<String> {
    with_host(|h| h.island.targets().iter().map(|(t, _)| format!("{t:?}")).collect()).unwrap_or_default()
}

pub(crate) fn class() -> String {
    with_host(|h| format!("{:?}", h.island.class())).unwrap_or_else(|| "none".into())
}

/// Nothing runs: no display link, no timer, the island's windows out.
pub(crate) fn idle() -> bool {
    with_host(|h| h.idle()).unwrap_or(false)
}

/// A display link or a loop timer is running (a pending one-shot deadline is allowed at rest: DESIGN.md §6).
pub(crate) fn animating() -> bool {
    with_host(|h| {
        let (link, looping, _) = h.clock_report();
        link || looping
    })
    .unwrap_or(false)
}

/// What keeps the island's clock alive: display link, loop timer, seconds to the deadline timer, and the scene.
pub(crate) fn clock() -> String {
    with_host(|h| {
        let (link, looping, deadline) = h.clock_report();
        format!("link={link} loop={looping} deadline_in={deadline:?} scene={:?}", h.island.scene())
    })
    .unwrap_or_default()
}

/// Frames painted, clock ticks, and each paint's main-thread seconds (the newest 5,000–10,000).
pub(crate) fn stats() -> (usize, usize, Vec<f64>) {
    with_host(|h| (h.stats.frames, h.stats.ticks, h.stats.seconds.clone())).unwrap_or_default()
}

/// Every window of this process that is on screen and draws: its title and frame (x, y, w, h in screen points). The
/// notch's click target at rest (transparent, drawing nothing, there while a last reply can be pulled down) is not one.
pub(crate) fn windows() -> Vec<(String, [f64; 4])> {
    let Some(mtm) = MainThreadMarker::new() else { return vec![] };
    let catcher = with_host(|h| h.notch_catcher().then(|| h.panels().1)).flatten();
    NSApplication::sharedApplication(mtm)
        .windows()
        .iter()
        .filter(|w| w.isVisible())
        .filter(|w| catcher.as_ref().is_none_or(|c| !std::ptr::eq(&**c as *const _ as *const objc2::runtime::AnyObject, &**w as *const _ as *const objc2::runtime::AnyObject)))
        .map(|w| {
            let f = w.frame();
            (w.title().to_string(), [f.origin.x, f.origin.y, f.size.width, f.size.height])
        })
        .collect()
}

/// Drawing mode with its overlay parked off every display at alpha 0 (the user's clicks never reach it); the island
/// shows the keys at the real notch on its own clock.
pub(crate) fn draw_open() -> Result<(), String> {
    let mtm = MainThreadMarker::new().ok_or("main thread")?;
    let screen = NSScreen::mainScreen(mtm).ok_or("no display")?;
    overlay::open_on(screen, Mode::Check, mtm).map(|_| ())
}

/// One stroke through the parked overlay's own event path; points in fractions of the display.
pub(crate) fn draw_stroke(points: &[(f64, f64)]) -> bool {
    let Some(mtm) = MainThreadMarker::new() else { return false };
    let (Some(panel), Some(screen)) = (overlay::panel(), NSScreen::mainScreen(mtm)) else { return false };
    if !overlay::is_open() || points.len() < 2 {
        return false;
    }
    let size = screen.frame().size;
    let n = panel.windowNumber();
    let at = |p: &(f64, f64)| NSPoint::new(p.0 * size.width, p.1 * size.height);
    panel.sendEvent(&super::island::checks::mouse(NSEventType::LeftMouseDown, at(&points[0]), n));
    for p in &points[1..] {
        panel.sendEvent(&super::island::checks::mouse(NSEventType::LeftMouseDragged, at(p), n));
    }
    panel.sendEvent(&super::island::checks::mouse(NSEventType::LeftMouseUp, at(points.last().unwrap()), n));
    true
}

pub(crate) fn draw_strokes() -> usize {
    overlay::strokes().len()
}

pub(crate) fn draw_at_rest() -> bool {
    overlay::at_rest()
}
