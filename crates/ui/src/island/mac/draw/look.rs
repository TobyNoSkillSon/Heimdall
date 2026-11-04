//! The agent's `look`: bring an app forward, switching Space if it lives on another one, and wait until macOS says
//! the switch is done (the active-Space notification, not a fixed delay:, measured 0.9 s here), so
//! the capture that follows shows it. Windows on another full-screen Space cannot be captured until shown, which is
//! why this switches first. The capture itself is the app's (crates/screen).

use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use block2::RcBlock;
use objc2_app_kit::{NSWorkspace, NSWorkspaceActiveSpaceDidChangeNotification, NSWorkspaceDidActivateApplicationNotification};
use objc2_core_foundation::{CGPoint, CGRect};
use objc2_foundation::NSNotification;

use super::api::PointerDisplay;

/// Same-Space activation needs only the window to come forward; this long without a Space change means none comes.
const SAME_SPACE: Duration = Duration::from_millis(200);
/// After the Space change is reported, a couple of frames for the window to settle.
const SETTLE: Duration = Duration::from_millis(80);
const LIMIT: Duration = Duration::from_millis(1500);

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventCreate(source: *const c_void) -> *mut c_void;
    fn CGEventGetLocation(event: *mut c_void) -> CGPoint;
    fn CGGetDisplaysWithPoint(point: CGPoint, max: u32, displays: *mut u32, count: *mut u32) -> i32;
    fn CGGetActiveDisplayList(max: u32, displays: *mut u32, count: *mut u32) -> i32;
    fn CGDisplayBounds(display: u32) -> CGRect;
    fn CGMainDisplayID() -> u32;
}
#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRelease(object: *const c_void);
}

pub(crate) fn pointer_display() -> Option<PointerDisplay> {
    unsafe {
        let event = CGEventCreate(std::ptr::null());
        if event.is_null() {
            return None;
        }
        let at = CGEventGetLocation(event);
        CFRelease(event);
        let (mut id, mut count) = (0u32, 0u32);
        if CGGetDisplaysWithPoint(at, 1, &mut id, &mut count) != 0 || count == 0 {
            id = CGMainDisplayID();
        }
        let mut ids = [0u32; 16];
        let mut n = 0u32;
        CGGetActiveDisplayList(16, ids.as_mut_ptr(), &mut n);
        let main = CGMainDisplayID();
        // The main display first, then the rest in the system's order (as `NSScreen.screens` lists them).
        let mut order: Vec<u32> = vec![main];
        order.extend(ids[..n as usize].iter().copied().filter(|d| *d != main));
        let index = order.iter().position(|d| *d == id).map_or(1, |i| i + 1);
        let bounds = CGDisplayBounds(id);
        Some(PointerDisplay { id, index, width: bounds.size.width, height: bounds.size.height })
    }
}

fn frontmost_is(app: &str) -> bool {
    NSWorkspace::sharedWorkspace()
        .frontmostApplication()
        .and_then(|a| a.localizedName())
        .is_some_and(|n| n.to_string().eq_ignore_ascii_case(app))
}

enum Seen {
    Space(Instant),
    Active(Instant),
}

pub(crate) fn bring_forward(app: &str) -> Result<Duration, String> {
    if objc2_foundation::MainThreadMarker::new().is_some() {
        return Err("bring_forward blocks; call it off the main thread".into());
    }
    let started = Instant::now();
    // Already frontmost may be the start of a switch someone else began (18 Oct 13:49: the agent's own `open -a
    // Finder` ran beside this look, Finder was frontmost before its window came forward, and the capture showed the
    // old screen). Watch for a Space change for the same-Space window before capturing; never reopen.
    let already = frontmost_is(app);
    let center = NSWorkspace::sharedWorkspace().notificationCenter();
    let (tx, rx) = mpsc::channel::<Seen>();
    let (space_tx, active_tx) = (tx.clone(), tx);
    // The workspace posts these on the main thread; the blocks only send the moment here.
    let on_space = RcBlock::new(move |_: NonNull<NSNotification>| {
        let _ = space_tx.send(Seen::Space(Instant::now()));
    });
    let on_active = RcBlock::new(move |_: NonNull<NSNotification>| {
        let _ = active_tx.send(Seen::Active(Instant::now()));
    });
    let observers = unsafe {
        [
            center.addObserverForName_object_queue_usingBlock(Some(NSWorkspaceActiveSpaceDidChangeNotification), None, None, &on_space),
            center.addObserverForName_object_queue_usingBlock(Some(NSWorkspaceDidActivateApplicationNotification), None, None, &on_active),
        ]
    };
    let opened = already || std::process::Command::new("/usr/bin/open").args(["-a", app]).status().is_ok_and(|s| s.success());
    let result = if !opened {
        Err(format!("could not open {app}"))
    } else {
        let (mut space, mut active) = (None::<Instant>, None::<Instant>);
        loop {
            let now = Instant::now();
            let same_space_from = if already { Some(started) } else { active };
            if space.is_some_and(|t| now - t >= SETTLE) || (space.is_none() && same_space_from.is_some_and(|t| now - t >= SAME_SPACE)) || now - started > LIMIT {
                break;
            }
            match rx.recv_timeout(Duration::from_millis(20)) {
                Ok(Seen::Space(t)) => space = Some(t),
                Ok(Seen::Active(t)) => active = active.or(Some(t)),
                Err(_) => {}
            }
        }
        Ok(started.elapsed())
    };
    for o in observers {
        unsafe { center.removeObserver(o.as_ref()) };
    }
    result
}
