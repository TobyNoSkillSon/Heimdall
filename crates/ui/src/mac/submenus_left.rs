//! Every submenu opens to the left of its parent, at every level ("All of the Heimdall menus
//! should open to the left. Not to the right."). AppKit has no setting for the side: a submenu opens right unless
//! the right has no room, and `confinementRectForMenu:onScreen:` is never asked for a submenu
//! (lab/menus-left/README.md). So the menu window class's `setFrame:display:` is wrapped: when AppKit places a
//! submenu on its parent's right, the frame is mirrored to the parent's left with the same overlap, before the
//! window is first ordered in, so it never shows on the right first. Where the left has no room the frame stays
//! where AppKit put it. Only `NSPopupMenuWindow` changes; a macOS without that class or selector keeps the native
//! side.
//!
//! Only a window AppKit presents as a submenu moves: `presentAsSubmenuRelativeToView:animated:
//! fromParentBoundsInWindow:withDelay:` names the row it opens from, just before the frame is set. The record goes
//! when AppKit sets the window up for a menu again (`setupWithImpl:`, every use, before present and frame; menu
//! windows are reused) and when it hides it (`_setVisible:` NO). A top-level menu (the status menu, a pop-up
//! button's, a context menu) has no record and is never touched. Order of calls measured on 24 Oct, macOS 27
//! (lab/menus-left/probe.swift, `trace`).

use std::cell::RefCell;
use std::ffi::c_char;
use std::sync::OnceLock;

use objc2::rc::{Retained, Weak};
use objc2::runtime::{AnyClass, Bool, Imp, NSObjectProtocol, Sel};
use objc2::sel;
use objc2_app_kit::{NSView, NSWindow, NSWindowNumberListOptions};
use objc2_foundation::{NSPoint, NSRect};

const MENU_WINDOW: &std::ffi::CStr = c"NSPopupMenuWindow";

type SetFrame = unsafe extern "C-unwind" fn(&NSWindow, Sel, NSRect, Bool);
type Present = unsafe extern "C-unwind" fn(&NSWindow, Sel, Option<&NSView>, Bool, NSRect, Bool);
type Setup = unsafe extern "C-unwind" fn(&NSWindow, Sel, *mut objc2::runtime::AnyObject);
type SetVisible = unsafe extern "C-unwind" fn(&NSWindow, Sel, Bool);

static SET_FRAME: OnceLock<SetFrame> = OnceLock::new();
static PRESENT: OnceLock<Present> = OnceLock::new();
static SETUP: OnceLock<Setup> = OnceLock::new();
static SET_VISIBLE: OnceLock<SetVisible> = OnceLock::new();

thread_local! {
    /// Open submenu windows and the window each opens from, as AppKit presented them.
    static PARENTS: RefCell<Vec<(Weak<NSWindow>, Weak<NSWindow>)>> = const { RefCell::new(Vec::new()) };
}

/// Wraps the menu window's frame setter once per process; menus made before or after both follow it.
pub(crate) fn install() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let Some(class) = AnyClass::get(MENU_WINDOW) else { return };
        unsafe {
            // These three or none: a frame is only moved for a submenu recorded since the window's last set-up.
            let sels = [
                sel!(presentAsSubmenuRelativeToView:animated:fromParentBoundsInWindow:withDelay:),
                sel!(setupWithImpl:),
                sel!(setFrame:display:),
            ];
            if sels.iter().any(|s| class.instance_method(*s).is_none()) { return }
            if let Some(old) = replace(class, sels[1], std::mem::transmute::<Setup, Imp>(setup)) {
                SETUP.set(std::mem::transmute::<Imp, Setup>(old)).ok();
            }
            if let Some(old) = replace(class, sel!(_setVisible:), std::mem::transmute::<SetVisible, Imp>(set_visible)) {
                SET_VISIBLE.set(std::mem::transmute::<Imp, SetVisible>(old)).ok();
            }
            if let Some(old) = replace(class, sels[0], std::mem::transmute::<Present, Imp>(present)) {
                PRESENT.set(std::mem::transmute::<Imp, Present>(old)).ok();
            }
            if let Some(old) = replace(class, sels[2], std::mem::transmute::<SetFrame, Imp>(set_frame)) {
                SET_FRAME.set(std::mem::transmute::<Imp, SetFrame>(old)).ok();
            }
        }
    });
}

/// Puts `imp` on `class` itself (an override when the method is inherited, so `NSWindow`'s stays as it is) and
/// returns the implementation it stands in for.
pub(crate) unsafe fn replace(class: &AnyClass, sel: Sel, imp: Imp) -> Option<Imp> {
    let method = class.instance_method(sel)?;
    let old = method.implementation();
    let types: *const c_char = objc2::ffi::method_getTypeEncoding(method);
    if objc2::ffi::class_addMethod(class as *const AnyClass as *mut AnyClass, sel, imp, types).as_bool() {
        Some(old)
    } else {
        Some(method.set_implementation(imp))
    }
}

fn is(weak: &Weak<NSWindow>, window: &NSWindow) -> bool {
    weak.load().is_some_and(|w| std::ptr::eq(&*w, window))
}

unsafe extern "C-unwind" fn present(this: &NSWindow, cmd: Sel, view: Option<&NSView>, animated: Bool, bounds: NSRect, delay: Bool) {
    if let Some(parent) = view.and_then(|v| v.window()) {
        PARENTS.with(|p| {
            let mut p = p.borrow_mut();
            p.retain(|(c, w)| !is(c, this) && c.load().is_some() && w.load().is_some());
            p.push((Weak::new(this), Weak::from_retained(&parent)));
        });
    }
    if let Some(old) = PRESENT.get() {
        old(this, cmd, view, animated, bounds, delay);
    }
}

fn forget(window: &NSWindow) {
    PARENTS.with(|p| p.borrow_mut().retain(|(c, _)| !is(c, window)));
    // A resize under way (menu_motion) ends with the menu the window showed.
    super::menu_motion::forget(window);
}

/// Set up for a menu (each use of a reused window): not a submenu until presented as one.
unsafe extern "C-unwind" fn setup(this: &NSWindow, cmd: Sel, implementation: *mut objc2::runtime::AnyObject) {
    forget(this);
    if let Some(old) = SETUP.get() {
        old(this, cmd, implementation);
    }
}

/// Hidden: no longer anyone's submenu.
unsafe extern "C-unwind" fn set_visible(this: &NSWindow, cmd: Sel, visible: Bool) {
    if !visible.as_bool() {
        forget(this);
    }
    if let Some(old) = SET_VISIBLE.get() {
        old(this, cmd, visible);
    }
}

unsafe extern "C-unwind" fn set_frame(this: &NSWindow, cmd: Sel, frame: NSRect, display: Bool) {
    let frame = parent_of(this)
        .and_then(|(parent, others)| {
            let screen = parent.screen().map(|s| s.visibleFrame())?;
            leftward(parent.frame(), frame, screen, &others)
        })
        .unwrap_or(frame);
    if let Some(old) = SET_FRAME.get() {
        old(this, cmd, frame, display);
    }
}

/// `frame` mirrored to `parent`'s left when AppKit put it on the right, keeping AppKit's overlap and height, held
/// inside `screen` (the parent's display) vertically; `None` when it is already on the left (AppKit's own flip, a
/// placeholder frame), the left has no room in `screen`, or it would cover one of the parent's ancestors (`others`:
/// after a fallback to the right near the left edge, deeper levels keep going right).
pub(crate) fn leftward(parent: NSRect, frame: NSRect, screen: NSRect, others: &[NSRect]) -> Option<NSRect> {
    let mid = |r: NSRect| r.origin.x + r.size.width / 2.0;
    if frame.size.width <= 0.0 || mid(frame) < mid(parent) {
        return None;
    }
    let overlap = parent.origin.x + parent.size.width - frame.origin.x;
    let x = parent.origin.x + overlap - frame.size.width;
    // AppKit fitted the height to the display it chose on the right; on the parent's display, keep it inside.
    let top = screen.origin.y + screen.size.height;
    let y = if frame.size.height <= screen.size.height {
        frame.origin.y.min(top - frame.size.height).max(screen.origin.y)
    } else {
        frame.origin.y
    };
    let left = NSRect::new(NSPoint::new(x, y), frame.size);
    (x >= screen.origin.x && !others.iter().any(|o| covers(&left, o))).then_some(left)
}

/// `a` and `b` overlap by more than a submenu's usual 5 pt overlap.
pub(crate) fn covers(a: &NSRect, b: &NSRect) -> bool {
    let w = (a.origin.x + a.size.width).min(b.origin.x + b.size.width) - a.origin.x.max(b.origin.x);
    let h = (a.origin.y + a.size.height).min(b.origin.y + b.size.height) - a.origin.y.max(b.origin.y);
    w > 8.0 && h > 0.0
}

/// For a window AppKit presented as a submenu: the open window it opens from, and the frames of that window's own
/// ancestors (its parent's parent and up to the top-level menu). A sibling submenu still fading out is not among
/// them, so it never blocks the mirror. `None` for every other window, top-level menus included.
fn parent_of(window: &NSWindow) -> Option<(Retained<NSWindow>, Vec<NSRect>)> {
    let recorded = |w: &NSWindow| PARENTS.with(|p| p.borrow().iter().find(|(c, _)| is(c, w)).and_then(|(_, p)| p.load()));
    let parent = recorded(window).filter(|p| p.isVisible())?;
    let mut ancestors = vec![];
    let mut at = parent.clone();
    // Menus nest a handful deep; the bound only guards a cycle.
    for _ in 0..16 {
        let Some(up) = recorded(&at).filter(|w| w.isVisible()) else { break };
        ancestors.push(up.frame());
        at = up;
    }
    Some((parent, ancestors))
}

/// Submenu windows on record (the check's: none once every menu has closed).
pub(crate) fn recorded() -> usize {
    PARENTS.with(|p| p.borrow().iter().filter(|(c, _)| c.load().is_some()).count())
}

/// Live check (`heimdall-ui-demo --submenus-left <dir>`): the real menu, built on the demo's state, pops up on the
/// main display for about a second per scenario while submenus open through NSMenu's own implementation object
/// (`_menuImpl`'s `highlightItemAtIndex:` and `openSubmenuFromHighlightedItem`, test-only private calls). No mouse or
/// key events are posted and the cursor never moves. Each open menu window's frame is read back, and the frame each
/// submenu had when it was ordered in; `<scenario>.png` draws the open menus from their own views. Returns the failed
/// checks.
pub(crate) mod check {
    use std::cell::RefCell;
    use std::path::{Path, PathBuf};

    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2::{msg_send, sel};
    use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy, NSMenu, NSScreen};
    use objc2_foundation::{MainThreadMarker, NSPoint, NSRect, NSRunLoop, NSRunLoopCommonModes, NSTimer};

    use super::*;

    /// Where the menu opens and the submenus to open in turn: a title, or `None` for the first row with a submenu; a
    /// title starting with `^` is a sibling in the menu the previous submenu opened from (the pointer moving down a
    /// menu from one submenu row to the next).
    struct Scenario {
        name: &'static str,
        anchor: fn(NSRect) -> NSPoint,
        path: Vec<Option<&'static str>>,
        /// Every child opens left of its parent (else: the left had no room, so each stays on screen).
        left: bool,
    }

    struct Run {
        menu: Retained<NSMenu>,
        path: Vec<Option<&'static str>>,
        /// Menus open now, the main menu first.
        open: Vec<Retained<NSMenu>>,
        /// Path steps taken.
        step: usize,
        ticks: usize,
        frames: Vec<NSRect>,
        shot: Option<PathBuf>,
    }

    thread_local! {
        static RUN: RefCell<Option<Run>> = const { RefCell::new(None) };
        /// Each submenu window's frame, and its parent's, at the moment it is ordered in: what the first frame shows.
        static ORDERED: RefCell<Vec<(NSRect, NSRect)>> = const { RefCell::new(Vec::new()) };
    }

    type Order = unsafe extern "C-unwind" fn(&NSWindow, Sel, isize, isize);
    static RECORD_ORDER: OnceLock<Order> = OnceLock::new();

    unsafe extern "C-unwind" fn record_order(this: &NSWindow, cmd: Sel, place: isize, relative: isize) {
        if place != 0 {
            if let Some((parent, _)) = parent_of(this) {
                ORDERED.with(|o| o.borrow_mut().push((this.frame(), parent.frame())));
            }
        }
        if let Some(old) = RECORD_ORDER.get() {
            old(this, cmd, place, relative);
        }
    }

    /// Open menu windows, the main menu first.
    pub(crate) fn menu_windows(mtm: MainThreadMarker) -> Vec<Retained<NSWindow>> {
        let Some(class) = AnyClass::get(MENU_WINDOW) else { return vec![] };
        let app = NSApplication::sharedApplication(mtm);
        let Some(numbers) = NSWindow::windowNumbersWithOptions(NSWindowNumberListOptions(0), mtm) else { return vec![] };
        let mut windows: Vec<_> = numbers.iter().filter_map(|n| app.windowWithWindowNumber(n.integerValue()))
            .filter(|w| w.isVisible() && w.isKindOfClass(class)).collect();
        windows.reverse();
        windows
    }

    pub(crate) fn run(dir: &Path) -> Vec<String> {
        let mtm = MainThreadMarker::new().expect("the left-opening check runs on the main thread");
        let app = NSApplication::sharedApplication(mtm);
        app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
        std::fs::create_dir_all(dir).ok();
        install();
        if let Some(class) = AnyClass::get(MENU_WINDOW) {
            if let Some(old) = unsafe { replace(class, sel!(orderWindow:relativeTo:), std::mem::transmute::<Order, Imp>(record_order)) } {
                RECORD_ORDER.set(unsafe { std::mem::transmute::<Imp, Order>(old) }).ok();
            }
        }
        let state = crate::mac::offscreen::demo_menu(mtm, |_| {});
        print_tree(&state.menu, 0);
        let width = state.menu.size().width;
        let scenarios = [
            Scenario { name: "model-two-levels", anchor: |v| NSPoint::new(v.origin.x + v.size.width * 0.62, v.origin.y + v.size.height * 0.92), path: vec![Some("Model"), None], left: true },
            // Model ▸ open, then Voice ▸ below it: Model's submenu closes as Voice's opens, and Voice's still opens left.
            Scenario { name: "right-edge-model-then-voice", anchor: |v| NSPoint::new(v.origin.x + v.size.width - 316.0, v.origin.y + v.size.height), path: vec![Some("Model"), Some("^Voice")], left: true },
            Scenario { name: "right-edge-voice", anchor: |v| NSPoint::new(v.origin.x + v.size.width - 316.0, v.origin.y + v.size.height), path: vec![Some("Voice")], left: true },
            // Under the menu bar at the right edge, where the status item's menu opens on the notch display.
            Scenario { name: "right-edge-model", anchor: |v| NSPoint::new(v.origin.x + v.size.width - 316.0, v.origin.y + v.size.height), path: vec![Some("Model"), None], left: true },
            Scenario { name: "right-edge-shortcuts", anchor: |v| NSPoint::new(v.origin.x + v.size.width - 316.0, v.origin.y + v.size.height), path: vec![Some("Shortcuts")], left: true },
            Scenario { name: "right-edge-recording", anchor: |v| NSPoint::new(v.origin.x + v.size.width - 316.0, v.origin.y + v.size.height), path: vec![Some("Recording")], left: true },
            Scenario { name: "right-edge-microphone", anchor: |v| NSPoint::new(v.origin.x + v.size.width - 316.0, v.origin.y + v.size.height), path: vec![Some("Microphone")], left: true },
            // No room on the left: the submenu stays where AppKit puts it, on screen.
            Scenario { name: "left-edge-model", anchor: |v| NSPoint::new(v.origin.x + 4.0, v.origin.y + v.size.height), path: vec![Some("Model"), None], left: false },
        ];
        let mut failures = vec![];
        for sc in scenarios {
            let Some(screen) = NSScreen::mainScreen(mtm) else { failures.push("no main screen".into()); break };
            let visible = screen.visibleFrame();
            let shot = sc.left.then(|| dir.join(format!("{}.png", sc.name)));
            // A second try when the menu closed before it was read: a click elsewhere on the screen ends tracking.
            let mut frames = vec![];
            for _ in 0..2 {
                ORDERED.with(|o| o.borrow_mut().clear());
                RUN.with(|r| *r.borrow_mut() = Some(Run { menu: state.menu.clone(), path: sc.path.clone(), open: vec![], step: 0, ticks: 0, frames: vec![], shot: shot.clone() }));
                let timer = unsafe { NSTimer::timerWithTimeInterval_target_selector_userInfo_repeats(0.3, &state.controller, sel!(submenusLeftStep:), None, true) };
                unsafe { NSRunLoop::currentRunLoop().addTimer_forMode(&timer, NSRunLoopCommonModes) };
                // Blocks in the menu's tracking loop until the last step cancels it.
                state.menu.popUpMenuPositioningItem_atLocation_inView(None, (sc.anchor)(visible), None);
                timer.invalidate();
                frames = RUN.with(|r| r.borrow_mut().take()).map(|r| r.frames).unwrap_or_default();
                if !frames.is_empty() { break }
                eprintln!("{}: the menu closed before it was read; once more", sc.name);
            }
            let fmt = |f: &NSRect| format!("({:.0}, {:.0}, {:.0}×{:.0})", f.origin.x, f.origin.y, f.size.width, f.size.height);
            let levels = sc.path.iter().filter(|p| !p.is_some_and(|t| t.starts_with('^'))).count() + 1;
            let mut ok = frames.len() == levels;
            for pair in frames.windows(2) {
                let (parent, child) = (pair[0], pair[1]);
                let is_left = child.origin.x + child.size.width <= parent.origin.x + 8.0;
                let on_screen = child.origin.x >= visible.origin.x - 0.5 && child.origin.x + child.size.width <= visible.origin.x + visible.size.width + 0.5;
                ok &= on_screen && (!sc.left || is_left);
            }
            // First frames: every submenu was already on its parent's left when it was ordered in (no right-hand flash).
            let ordered = ORDERED.with(|o| o.borrow().clone());
            ok &= ordered.len() >= levels - 1
                && (!sc.left || ordered.iter().all(|(f, p)| f.origin.x + f.size.width <= p.origin.x + 8.0));
            // Closed: no submenu stays on record (a top-level menu reusing its window is never moved).
            let left_over = recorded();
            ok &= left_over == 0;
            // No submenu covers a menu other than its parent.
            for (i, child) in frames.iter().enumerate().skip(2) {
                ok &= !frames[..i - 1].iter().any(|o| covers(child, o));
            }
            let line = format!("{}: {} levels of {levels}, {} ordered in on the left, {left_over} on record after closing, at x {:.0}, main menu {width:.0} wide: {}", sc.name, frames.len(),
                ordered.iter().filter(|(f, p)| f.origin.x + f.size.width <= p.origin.x + 8.0).count(),
                (sc.anchor)(visible).x, frames.iter().map(fmt).collect::<Vec<_>>().join(" → "));
            eprintln!("check {line}: {}", if ok { "ok" } else { "FAILED" });
            if !ok { failures.push(line); }
        }
        failures
    }

    fn windows_now(mtm: MainThreadMarker) -> Vec<Retained<NSWindow>> { menu_windows(mtm) }

    /// Each open menu window drawn from its own views (`cacheDisplayInRect`, as the menu renders do: no screen
    /// capture, so nothing else on the screen), laid out at its screen position on a flat dark ground.
    pub(crate) fn composite(path: &Path, windows: &[Retained<NSWindow>]) -> bool {
        use objc2::AllocAnyThread as _;
        use objc2_app_kit::{NSBitmapImageFileType, NSBitmapImageRep, NSColor, NSCompositingOperation, NSGraphicsContext, NSRectFill};
        use objc2_foundation::{NSDictionary, NSSize, NSString};
        let pad = 16.0;
        let frames: Vec<NSRect> = windows.iter().map(|w| w.frame()).collect();
        let Some(first) = frames.first() else { return false };
        let (mut x0, mut y0, mut x1, mut y1) = (first.origin.x, first.origin.y, first.origin.x + first.size.width, first.origin.y + first.size.height);
        for f in &frames {
            x0 = x0.min(f.origin.x); y0 = y0.min(f.origin.y);
            x1 = x1.max(f.origin.x + f.size.width); y1 = y1.max(f.origin.y + f.size.height);
        }
        let scale = windows[0].backingScaleFactor();
        let size = NSSize::new(x1 - x0 + 2.0 * pad, y1 - y0 + 2.0 * pad);
        let Some(out) = (unsafe {
            NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bytesPerRow_bitsPerPixel(
                NSBitmapImageRep::alloc(), std::ptr::null_mut(), (size.width * scale) as isize, (size.height * scale) as isize, 8, 4, true, false,
                objc2_app_kit::NSDeviceRGBColorSpace, 0, 0)
        }) else { return false };
        out.setSize(size);
        // Each window's frame view (title-less: the menu's material and rows), drawn into its own bitmap first.
        let reps: Vec<_> = windows.iter().filter_map(|w| {
            let view = w.contentView().and_then(|c| unsafe { c.superview() }).or_else(|| w.contentView())?;
            let rep = view.bitmapImageRepForCachingDisplayInRect(view.bounds())?;
            view.cacheDisplayInRect_toBitmapImageRep(view.bounds(), &rep);
            Some(rep)
        }).collect();
        let Some(context) = NSGraphicsContext::graphicsContextWithBitmapImageRep(&out) else { return false };
        NSGraphicsContext::saveGraphicsState_class();
        NSGraphicsContext::setCurrentContext(Some(&context));
        NSColor::colorWithWhite_alpha(0.3, 1.0).set();
        NSRectFill(NSRect::new(NSPoint::new(0.0, 0.0), size));
        for (rep, f) in reps.iter().zip(&frames) {
            let at = NSRect::new(NSPoint::new(f.origin.x - x0 + pad, f.origin.y - y0 + pad), f.size);
            unsafe { rep.drawInRect_fromRect_operation_fraction_respectFlipped_hints(at, NSRect::ZERO, NSCompositingOperation::SourceOver, 1.0, true, None) };
        }
        NSGraphicsContext::restoreGraphicsState_class();
        let data = unsafe { out.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new()) };
        reps.len() == windows.len() && data.is_some_and(|d| d.writeToFile_atomically(&NSString::from_str(&path.to_string_lossy()), true))
    }

    fn print_tree(menu: &NSMenu, depth: usize) {
        if depth > 2 { return }
        for item in menu.itemArray().iter() {
            if let Some(sub) = item.submenu() {
                eprintln!("{}{} ▸", "  ".repeat(depth), item.title());
                print_tree(&sub, depth + 1);
            }
        }
    }

    /// One step inside the tracking loop (the controller's `submenusLeftStep:` timer).
    pub(crate) fn step() {
        let Some(mtm) = MainThreadMarker::new() else { return };
        let done = RUN.with(|r| {
            let mut r = r.borrow_mut();
            let run = r.as_mut()?;
            run.ticks += 1;
            let windows = menu_windows(mtm);
            if run.open.is_empty() && !windows.is_empty() { run.open.push(run.menu.clone()); }
            let depth = run.open.len();
            // Done once every step is taken and exactly the open menus show (a closed sibling has faded).
            if run.ticks > 20 || (run.step == run.path.len() && windows.len() == depth) {
                run.frames = windows.iter().map(|w| w.frame()).collect();
                return Some((run.menu.clone(), run.shot.take(), run.frames.clone()));
            }
            // Wait for the last opened submenu's window.
            if depth == 0 || windows.len() < depth || run.step == run.path.len() { return None }
            let sibling = run.path[run.step].and_then(|t| t.strip_prefix('^'));
            if sibling.is_some() {
                run.open.pop();
            }
            let menu = run.open[run.open.len() - 1].clone();
            let index = match sibling.or(run.path[run.step]) {
                // A row's own title, without its value (climb 2, `menu::row_title`).
                Some(title) => menu.itemArray().iter().position(|i| crate::mac::menu::row_title(&i) == title && i.submenu().is_some()),
                None => menu.itemArray().iter().position(|i| i.submenu().is_some()),
            };
            run.step += 1;
            let Some(index) = index else { run.ticks = 99; return None };
            let item = menu.itemAtIndex(index as isize)?;
            unsafe {
                let imp: Option<Retained<AnyObject>> = msg_send![&*menu, _menuImpl];
                let Some(imp) = imp else { run.ticks = 99; return None };
                let _: () = msg_send![&*imp, highlightItemAtIndex: index as isize];
                let _: Bool = msg_send![&*imp, openSubmenuFromHighlightedItem];
            }
            run.open.push(item.submenu()?);
            None
        });
        if let Some((menu, shot, frames)) = done {
            if let (Some(path), false) = (shot, frames.is_empty()) {
                let ok = composite(&path, &windows_now(mtm));
                eprintln!("render {} {}", path.display(), if ok { "ok" } else { "FAILED" });
            }
            menu.cancelTracking();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use objc2_foundation::NSSize;

    fn r(x: f64, y: f64, w: f64, h: f64) -> NSRect { NSRect::new(NSPoint::new(x, y), NSSize::new(w, h)) }
    const SCREEN: NSRect = NSRect { origin: NSPoint { x: 0.0, y: 0.0 }, size: NSSize { width: 1728.0, height: 1080.0 } };

    #[test]
    fn mirrors_a_right_hand_submenu_with_the_same_overlap() {
        // Measured on 24 Oct: the parent at x 900, 127 wide; AppKit's submenu at 1022 (5 pt overlap), 143 wide.
        let left = leftward(r(900.0, 997.0, 127.0, 82.0), r(1022.0, 997.0, 143.0, 58.0), SCREEN, &[]).unwrap();
        assert_eq!((left.origin.x, left.origin.y, left.size.width), (762.0, 997.0, 143.0));
    }

    #[test]
    fn leaves_appkits_own_left_flip_and_placeholders() {
        assert!(leftward(r(1500.0, 900.0, 200.0, 80.0), r(1305.0, 900.0, 200.0, 58.0), SCREEN, &[]).is_none());
        assert!(leftward(r(900.0, 997.0, 127.0, 82.0), r(0.0, -10.0, 10.0, 10.0), SCREEN, &[]).is_none());
    }

    #[test]
    fn never_covers_another_open_menu() {
        // Measured on 24 Oct at the left edge: the main menu at 6, Model ▸ fell back right to 313; its submenu would
        // have mirrored onto the main menu at 21.
        let main = r(6.0, 124.0, 312.0, 959.0);
        assert!(leftward(r(313.0, 607.0, 272.0, 121.0), r(580.0, 472.0, 297.0, 169.0), SCREEN, &[main]).is_none());
        // Ancestors on the right, as in a leftward cascade, leave the left free.
        let left = leftward(r(804.0, 520.0, 272.0, 121.0), r(1071.0, 385.0, 297.0, 169.0), SCREEN, &[r(1071.0, 37.0, 312.0, 965.0)]);
        assert_eq!(left.map(|l| l.origin.x), Some(512.0));
    }

    #[test]
    fn keeps_a_mirrored_submenu_inside_the_parents_display_vertically() {
        // AppKit fitted the submenu to a taller display on the right; mirrored onto the parent's, it is pulled down
        // under the menu bar, and up off the bottom.
        let high = leftward(r(900.0, 900.0, 312.0, 100.0), r(1207.0, 1000.0, 200.0, 300.0), SCREEN, &[]).unwrap();
        assert_eq!((high.origin.x, high.origin.y), (705.0, 780.0));
        let low = leftward(r(900.0, 0.0, 312.0, 100.0), r(1207.0, -150.0, 200.0, 300.0), SCREEN, &[]).unwrap();
        assert_eq!(low.origin.y, 0.0);
    }

    #[test]
    fn stays_right_when_the_left_has_no_room() {
        assert!(leftward(r(60.0, 900.0, 200.0, 80.0), r(255.0, 900.0, 220.0, 58.0), SCREEN, &[]).is_none());
        // The room is the parent's display's: one whose frame starts left of zero has it.
        let wide = r(-1920.0, 0.0, 1920.0 + 1728.0, 1080.0);
        assert_eq!(leftward(r(60.0, 900.0, 200.0, 80.0), r(255.0, 900.0, 220.0, 58.0), wide, &[]).unwrap().origin.x, -155.0);
    }
}
