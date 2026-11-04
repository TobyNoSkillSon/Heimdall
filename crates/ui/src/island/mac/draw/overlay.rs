//! Drawing mode's overlay: a borderless, transparent, non-activating panel over the whole display under the pointer
//! that takes every click, drag, right click and scroll, so nothing reaches the apps below; the app the user was in stays
//! frontmost and gets the keyboard back when it closes.
//!
//! It is part of the island ("the prototype UI is also part of the island"): its keys and Send are the
//! island's (`Scene::Drawing`), and its light and flash are painted from the island's frame on the island's clock
//! (`paint_light`, called by the island's host each frame). It never animates on its own. A press on one of the
//! island's targets is the island's, never ink, so a trackpad tap on Send always sends.
//!
//! The panel exists from launch (ordered out) under `OVERLAY_WINDOW_TITLE`, so Heimdall's capture filters, built
//! when recording starts, already exclude it. It sits above menus, the menu bar, the Dock and full-screen apps; the
//! island rises above it while it is up. Closed, nothing runs: no layers, no timers, no observers.
//!
//! Opened while a menu is open (Heimdall's or another app's), it holds a still of the screen with the menu under the
//! ink, and Send marks that still (`still`).
//!
//! Entry points (main thread): `install` once, `toggle` (⌃⌘M), `open` / `close` (the agent's `drawing_mode`); the
//! island calls `send_now` (Send or Return), `closed_by_island` (Close or Esc), `undo` and `clear`.

use std::cell::RefCell;
use std::rc::Rc;

use objc2::rc::Retained;
use objc2::{define_class, msg_send, AllocAnyThread, MainThreadOnly};
use objc2::runtime::AnyObject;
use objc2_app_kit::{
    NSBackingStoreType, NSColor, NSCursor, NSEvent, NSEventModifierFlags, NSPanel, NSPopUpMenuWindowLevel, NSScreen, NSTrackingArea,
    NSTrackingAreaOptions, NSView, NSWindowAnimationBehavior, NSWindowStyleMask,
};
use objc2_core_foundation::CFRetained;
use objc2_core_graphics::{CGColor, CGImage};
use objc2_foundation::{MainThreadMarker, NSPoint, NSRect, NSString};
use objc2_quartz_core::{kCALineCapRound, kCALineJoinRound, CALayer, CAShapeLayer, CATransaction};

use super::ink;
use super::still::{self, Hold, Still};
use crate::drawing::{Ink, Marks, MarkedShot, Pt, OVERLAY_WINDOW_TITLE};
use crate::island::object::{Event, Light, Shape, Target};
use crate::mac::island::{self, paint::LightLayers};
use crate::mac::util::{overlay_behavior, pointer_screen, rect};

/// How the overlay is shown: for the user (key, takes input), or for the self-check (input through `sendEvent` only:
/// alpha 0, parked outside every display, never key).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Mode {
    Live,
    Check,
}

struct Open {
    view: Retained<InkView>,
    /// The strokes as vector layers (a rim and an ink layer each, drawn by the GPU; no screen-sized bitmap).
    marks: Retained<CALayer>,
    stroke_layers: Vec<[Retained<CAShapeLayer>; 2]>,
    ink: Ink,
    /// The display's frame (screen coordinates) and id: the strokes' space and the capture's display.
    screen: NSRect,
    display: u32,
    mode: Mode,
    /// A press that went to the island (a target under it), released there too.
    island_press: bool,
    sending: bool,
    /// The still under the ink while a menu was open: its layer and image (Send marks it).
    backdrop: Option<(Retained<CALayer>, CFRetained<CGImage>)>,
    /// Waiting for a still under this token; the keyboard is taken once it is there.
    awaiting: Option<u64>,
}

struct Closing {
    view: Retained<InkView>,
    marks: Retained<CALayer>,
    backdrop: Option<Retained<CALayer>>,
}

struct State {
    panel: Retained<OverlayPanel>,
    container: Retained<NSView>,
    light: Option<LightLayers>,
    open: Option<Open>,
    closing: Option<Closing>,
    /// The last token a still was asked under.
    token: u64,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
    static SINK: RefCell<Option<Rc<dyn Fn(MarkedShot)>>> = const { RefCell::new(None) };
}

fn with_state<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
    STATE.with(|s| s.try_borrow_mut().ok()?.as_mut().map(f))
}

fn with_open<R>(f: impl FnOnce(&mut Open) -> R) -> Option<R> {
    with_state(|s| s.open.as_mut().map(f)).flatten()
}

/// Where Send's marked screenshot goes (the app, as `UiAction::DrawingSent`).
pub(crate) fn set_sink(sink: Rc<dyn Fn(MarkedShot)>) {
    SINK.with(|s| *s.borrow_mut() = Some(sink));
}

/// Creates the (ordered-out) panel at launch.
pub(crate) fn install(mtm: MainThreadMarker) {
    if STATE.with(|s| s.borrow().is_some()) {
        return;
    }
    let frame = rect(0.0, 0.0, 100.0, 100.0);
    let style = NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel;
    let panel: Retained<OverlayPanel> = unsafe {
        msg_send![OverlayPanel::alloc(mtm), initWithContentRect: frame, styleMask: style, backing: NSBackingStoreType::Buffered, defer: false]
    };
    unsafe { panel.setReleasedWhenClosed(false) };
    panel.setTitle(&NSString::from_str(OVERLAY_WINDOW_TITLE));
    panel.setSharingType(crate::mac::streams::sharing());
    // Above menus (a menu stays visible under the ink as a still, and the ink is never under a live one), and under
    // the island, which rises above it while drawing mode is up (`island::pin`).
    panel.setLevel(LEVEL);
    panel.setOpaque(false);
    panel.setBackgroundColor(Some(&NSColor::clearColor()));
    panel.setHasShadow(false);
    // Explicitly false: transparent pixels catch clicks too, instead of passing them to the app below.
    panel.setIgnoresMouseEvents(false);
    panel.setAcceptsMouseMovedEvents(true);
    panel.setHidesOnDeactivate(false);
    panel.setAnimationBehavior(NSWindowAnimationBehavior::None);
    panel.setCollectionBehavior(overlay_behavior());
    let container: Retained<NSView> = unsafe { msg_send![NSView::alloc(mtm), initWithFrame: frame] };
    container.setWantsLayer(true);
    panel.setContentView(Some(&container));
    STATE.with(|s| *s.borrow_mut() = Some(State { panel, container, light: None, open: None, closing: None, token: 0 }));
}

/// The overlay's window level: one above pop-up menus.
pub(crate) const LEVEL: isize = NSPopUpMenuWindowLevel + 1;

pub(crate) fn is_open() -> bool {
    with_open(|_| ()).is_some()
}

/// ⌃⌘M: opens drawing mode, or closes it (discarding the strokes).
pub(crate) fn toggle(mtm: MainThreadMarker) {
    if is_open() {
        still::forget_choice();
        close(mtm);
    } else {
        let _ = open(mtm);
    }
}

/// Opens drawing mode on the display under the pointer. Returns which display, or why not.
pub(crate) fn open(mtm: MainThreadMarker) -> Result<(usize, Retained<NSScreen>), String> {
    let Some(screen) = pointer_screen(mtm) else {
        still::forget_choice();
        return Err("no display".into());
    };
    open_on(screen, Mode::Live, mtm)
}

/// The panel (checks drive it through `sendEvent`).
pub(crate) fn panel() -> Option<Retained<OverlayPanel>> {
    with_state(|s| s.panel.clone())
}

/// Opens on `screen`, holding a still when a menu is open (`still::hold_for`; other apps' menus only on a display).
pub(crate) fn open_on(screen: Retained<NSScreen>, mode: Mode, mtm: MainThreadMarker) -> Result<(usize, Retained<NSScreen>), String> {
    if is_open() {
        let index = display_index(&screen, mtm);
        return Ok((index, screen));
    }
    let hold = still::hold_for(screen.CGDirectDisplayID(), mode == Mode::Live);
    open_held(screen, mode, hold, mtm)
}

pub(crate) fn open_held(screen: Retained<NSScreen>, mode: Mode, hold: Hold, mtm: MainThreadMarker) -> Result<(usize, Retained<NSScreen>), String> {
    if is_open() {
        let index = display_index(&screen, mtm);
        return Ok((index, screen));
    }
    install(mtm);
    finish_closing();
    let frame = screen.frame();
    let (w, h) = (frame.size.width, frame.size.height);
    let bounds = rect(0.0, 0.0, w, h);
    let corner = super::system::top_corner_points(&screen);
    let Some((panel, container)) = with_state(|s| (s.panel.clone(), s.container.clone())) else {
        return Err("drawing mode is not installed".into());
    };
    match mode {
        Mode::Check => {
            // Never on a display: input arrives through `sendEvent` only, and the user never sees it.
            panel.setFrame_display(rect(-40_000.0, -40_000.0, w, h), false);
            panel.setAlphaValue(0.0);
        }
        Mode::Live => {
            panel.setFrame_display(frame, false);
            panel.setAlphaValue(1.0);
        }
    }
    panel.setIgnoresMouseEvents(false);
    container.setFrame(bounds);
    let view: Retained<InkView> = unsafe { msg_send![InkView::alloc(mtm), initWithFrame: bounds] };
    let tracking = unsafe {
        NSTrackingArea::initWithRect_options_owner_userInfo(
            NSTrackingArea::alloc(),
            bounds,
            NSTrackingAreaOptions::MouseMoved | NSTrackingAreaOptions::ActiveAlways | NSTrackingAreaOptions::InVisibleRect,
            Some(&view),
            None,
        )
    };
    view.addTrackingArea(&tracking);
    view.setWantsLayer(true);
    container.addSubview(&view);
    let light = LightLayers::new((w, h), corner);
    let marks = CALayer::new();
    CATransaction::begin();
    CATransaction::setDisableActions(true);
    marks.setFrame(bounds);
    view.layer().expect("layer-backed").addSublayer(&marks);
    container.layer().expect("layer-backed").addSublayer(&light.root);
    CATransaction::commit();
    panel.makeFirstResponder(Some(&view));
    let display = screen.CGDirectDisplayID();
    let token = with_state(|s| {
        if let Some(old) = s.light.replace(light) {
            old.root.removeFromSuperlayer();
        }
        s.token += 1;
        // In checks the strokes' screen is where the panel is parked (the island is parked with it).
        let screen = if mode == Mode::Check { rect(-40_000.0, -40_000.0, w, h) } else { frame };
        let awaiting = matches!(hold, Hold::Wait | Hold::Grab).then_some(s.token);
        s.open = Some(Open { view, marks, stroke_layers: Vec::new(), ink: Ink::default(), screen, display, mode, island_press: false, sending: false, backdrop: None, awaiting });
        s.token
    })
    .unwrap_or(0);
    match hold {
        Hold::Ready(still) => hold_still(still),
        Hold::Wait => still::wait(token),
        Hold::Grab => still::grab(display, token),
        Hold::Live => {}
    }
    publish();
    // Above a menu without the keyboard: the menu stays open until its still is under the ink.
    panel.orderFrontRegardless();
    if mode == Mode::Live {
        // Still while Spaces slide, like the island (checks never reach SkyLight).
        crate::mac::overlay_space::join(&panel, OVERLAY_WINDOW_TITLE);
        NSEvent::setMouseCoalescingEnabled(false);
        if with_open(|o| o.awaiting.is_none()) == Some(true) {
            panel.makeKeyWindow();
        }
        NSCursor::crosshairCursor().set();
    }
    // One clock: the island grows its keys and lights its outline, and the screen's light runs from its flares.
    island::pin(Some(display));
    island::apply(Event::DrawOpen);
    Ok((display_index(&screen, mtm), screen))
}

/// A still asked for under `token` arrived (or could not be taken): it goes under the ink, and then the overlay takes
/// the keyboard, which closes another app's menu under the still. A frame later, so the still is on screen first.
pub(crate) fn deliver(token: u64, still: Option<Still>) {
    let Some(live) = with_open(|o| {
        if o.awaiting != Some(token) {
            return None;
        }
        o.awaiting = None;
        Some(o.mode == Mode::Live)
    })
    .flatten() else {
        return;
    };
    if let Some(still) = still {
        hold_still(still);
    }
    if !live {
        return;
    }
    CATransaction::flush();
    let after = dispatch2::DispatchTime::try_from(KEY_AFTER_STILL).unwrap_or(dispatch2::DispatchTime::NOW);
    let _ = dispatch2::DispatchQueue::main().after(after, move || {
        let current = with_state(|s| s.open.is_some() && s.token == token).unwrap_or(false);
        if current {
            if let Some(panel) = panel() {
                panel.makeKeyWindow();
                NSCursor::crosshairCursor().set();
            }
        }
    });
}

/// From the still's commit to taking the keyboard: two frames at 60 Hz.
const KEY_AFTER_STILL: std::time::Duration = std::time::Duration::from_millis(34);

/// Puts `still` under the ink of the open overlay, when it shows the overlay's display.
fn hold_still(still: Still) {
    let Some(container) = with_state(|s| s.container.clone()) else { return };
    with_open(|o| {
        if o.backdrop.is_some() || still.display != o.display {
            return;
        }
        let layer = CALayer::new();
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        layer.setFrame(rect(0.0, 0.0, o.screen.size.width, o.screen.size.height));
        // SAFETY: a CGImage is a valid layer contents object (toll-free with id).
        unsafe { layer.setContents(Some(&*(&*still.image as *const CGImage as *const AnyObject))) };
        container.layer().expect("layer-backed").insertSublayer_atIndex(&layer, 0);
        CATransaction::commit();
        o.backdrop = Some((layer, still.image));
    });
}

/// Whether the open overlay holds a still (checks).
pub(crate) fn holds_still() -> bool {
    with_open(|o| o.backdrop.is_some()).unwrap_or(false)
}

/// Whether the open overlay waits for a still before it takes the keyboard (checks).
pub(crate) fn awaiting_still() -> bool {
    with_open(|o| o.awaiting.is_some()).unwrap_or(false)
}

/// 1-based, the main display first.
fn display_index(screen: &NSScreen, mtm: MainThreadMarker) -> usize {
    let id = screen.CGDirectDisplayID();
    NSScreen::screens(mtm).iter().position(|s| s.CGDirectDisplayID() == id).map_or(1, |i| i + 1)
}

/// Closes drawing mode and discards its strokes (⌃⌘M, the agent's `drawing_mode` off).
pub(crate) fn close(_mtm: MainThreadMarker) {
    if dismiss() {
        island::apply(Event::DrawClose);
    }
}

/// The island closed drawing mode (Close or Esc): input goes back to the apps now; the light runs back on the
/// island's clock and the panel goes out when it has.
pub(crate) fn closed_by_island() {
    dismiss();
}

/// Ends input at once; the strokes stay until the light has run back (they fade with it). Whether it was open.
fn dismiss() -> bool {
    let Some((open, panel)) = with_state(|s| s.open.take().map(|o| (o, s.panel.clone()))).flatten() else { return false };
    panel.setIgnoresMouseEvents(true);
    publish_none();
    still::release();
    if open.mode == Mode::Live {
        NSEvent::setMouseCoalescingEnabled(true);
        NSCursor::arrowCursor().set();
    }
    let Open { view, marks, backdrop, .. } = open;
    let backdrop = backdrop.map(|(layer, _)| layer);
    with_state(|s| s.closing = Some(Closing { view, marks, backdrop }));
    true
}

/// The island's frame: the screen's light and flash, and the strokes fading with the light once closed. When the
/// light has run back and the flash is over, the panel goes out.
pub(crate) fn paint_light(light: &Light, shape: &Shape) {
    let done = with_state(|s| {
        let Some(l) = s.light.as_mut() else { return false };
        l.apply(light, shape);
        if let Some(c) = &s.closing {
            CATransaction::begin();
            CATransaction::setDisableActions(true);
            c.marks.setOpacity(light.run.clamp(0.0, 1.0) as f32);
            // The held menu fades with them, back to the live screen.
            if let Some(b) = &c.backdrop {
                b.setOpacity(light.run.clamp(0.0, 1.0) as f32);
            }
            CATransaction::commit();
        }
        s.open.is_none() && light.run <= 0.0 && light.flash <= 0.0
    })
    .unwrap_or(false);
    if done {
        finish_closing();
    }
}

/// Drawing mode is over: its layers go and the panel goes out.
fn finish_closing() {
    let Some((closing, light, panel, reopened)) = with_state(|s| {
        let reopened = s.open.is_some();
        let light = if reopened { None } else { s.light.take() };
        Some((s.closing.take(), light, s.panel.clone(), reopened))
    })
    .flatten() else {
        return;
    };
    CATransaction::begin();
    CATransaction::setDisableActions(true);
    if let Some(c) = &closing {
        c.marks.removeFromSuperlayer();
        if let Some(b) = &c.backdrop {
            b.removeFromSuperlayer();
        }
    }
    if let Some(l) = &light {
        l.root.removeFromSuperlayer();
    }
    CATransaction::commit();
    if let Some(c) = closing {
        c.view.removeFromSuperview();
    }
    if !reopened {
        panel.orderOut(None);
        // Not from inside the island's frame: unpinned on the next turn of the main queue, unless drawing mode opened
        // again meanwhile (every open passes here first: it would unpin the island it had just pinned).
        dispatch2::DispatchQueue::main().exec_async(|| {
            if !is_open() {
                island::pin(None);
            }
        });
    }
}

pub(crate) fn undo() {
    with_open(|o| {
        if o.ink.undo() {
            sync_strokes(o);
        }
    });
    publish();
}

pub(crate) fn clear() {
    with_open(|o| {
        if o.ink.clear() {
            sync_strokes(o);
        }
    });
    publish();
}

/// The strokes drawn so far.
pub(crate) fn strokes() -> Vec<Vec<Pt>> {
    with_open(|o| o.ink.strokes.clone()).unwrap_or_default()
}

/// Nothing of drawing mode is alive: no open overlay, no closing light, the panel ordered out.
pub(crate) fn at_rest() -> bool {
    with_state(|s| s.open.is_none() && s.closing.is_none() && !s.panel.isVisible()).unwrap_or(true)
}

/// The strokes, for screenshots taken while the overlay is up; the island's undo and clear light up with ink.
fn publish() {
    let marks = with_open(|o| Marks { display: o.display, view: (o.screen.size.width, o.screen.size.height), strokes: o.ink.strokes.clone() });
    let count = marks.as_ref().map_or(0, |m| m.strokes.len());
    crate::drawing::publish_marks(marks);
    if is_open() {
        island::apply(Event::Ink(count));
    }
}

fn publish_none() {
    crate::drawing::publish_marks(None);
}

/// The island pressed Send: capture the overlay's display without Heimdall's windows, draw the strokes on it and
/// hand the PNG to the app. Input goes back to the apps at once; the composite and the PNG run off the main thread.
pub(crate) fn send_now() {
    let Some((strokes, display, view, held)) = with_open(|o| {
        if o.sending {
            return None;
        }
        o.sending = true;
        Some((o.ink.strokes.clone(), o.display, (o.screen.size.width, o.screen.size.height), o.backdrop.as_ref().map(|(_, image)| image.clone())))
    })
    .flatten() else {
        return;
    };
    dismiss();
    if let Some(image) = held {
        send_held(image, strokes, display, view);
        return;
    }
    let Some(grab) = crate::drawing::grabber() else {
        island::apply(Event::CaptureFailed("Screen capture unavailable".into()));
        return;
    };
    std::thread::spawn(move || {
        let shot = grab(display);
        let Ok((w, h, rgb)) = shot else {
            let reason = shot.err().unwrap_or_default();
            dispatch2::DispatchQueue::main().exec_async(move || {
                eprintln!("Drawing mode: the screen could not be captured: {reason}");
                island::apply(Event::CaptureFailed("Not attached".into()));
            });
            return;
        };
        let count = strokes.len();
        let result = ink::composite_rgb(&rgb, w, h, view, &strokes).and_then(|marked| ink::encode_png(w, h, &marked));
        dispatch2::DispatchQueue::main().exec_async(move || match result {
            Ok(png) => {
                let shot = MarkedShot { png, width: w, height: h, display, strokes: count };
                if let Some(sink) = SINK.with(|s| s.borrow().clone()) {
                    sink(shot);
                }
            }
            Err(e) => {
                eprintln!("Drawing mode: the marked screenshot could not be made: {e}");
                island::apply(Event::CaptureFailed("Not attached".into()));
            }
        });
    });
}

/// Send over a held still: the strokes drawn on the still the user drew on (the menu in it), not on the screen now. Seeing
/// turned off since the still was taken: nothing is sent, as a capture would be refused.
fn send_held(image: CFRetained<CGImage>, strokes: Vec<Vec<Pt>>, display: u32, view: (f64, f64)) {
    if !crate::drawing::seeing() {
        eprintln!("Drawing mode: Seeing is off; the marked screenshot is not sent");
        island::apply(Event::CaptureFailed("Not attached".into()));
        return;
    }
    struct Image(CFRetained<CGImage>);
    // SAFETY: CGImage is immutable and thread-safe.
    unsafe impl Send for Image {}
    let image = Image(image);
    std::thread::spawn(move || {
        let image = image;
        let (w, h) = (CGImage::width(Some(&image.0)) as u32, CGImage::height(Some(&image.0)) as u32);
        let count = strokes.len();
        // Drawn into sRGB, as every marked screenshot is.
        let result = ink::composite_image(&image.0, view, &strokes).and_then(|marked| ink::encode_png(w, h, &marked));
        dispatch2::DispatchQueue::main().exec_async(move || match result {
            Ok(png) => {
                let shot = MarkedShot { png, width: w, height: h, display, strokes: count };
                if let Some(sink) = SINK.with(|s| s.borrow().clone()) {
                    sink(shot);
                }
            }
            Err(e) => {
                eprintln!("Drawing mode: the marked screenshot could not be made: {e}");
                island::apply(Event::CaptureFailed("Not attached".into()));
            }
        });
    });
}

/// Brings the stroke layers in line with the strokes: drops layers of undone strokes, adds a pair for a new one and
/// gives the latest its current path. Only the stroke being drawn changes on a drag.
fn sync_strokes(o: &mut Open) {
    CATransaction::begin();
    CATransaction::setDisableActions(true);
    while o.stroke_layers.len() > o.ink.strokes.len() {
        for layer in o.stroke_layers.pop().expect("a stroke's layers") {
            layer.removeFromSuperlayer();
        }
    }
    if o.stroke_layers.len() < o.ink.strokes.len() {
        let pair = ink::styles().map(|(width, (r, g, b, a))| {
            let layer = CAShapeLayer::new();
            layer.setFrame(o.marks.bounds());
            layer.setFillColor(None);
            layer.setStrokeColor(Some(&CGColor::new_srgb(r, g, b, a)));
            layer.setLineWidth(width);
            unsafe {
                layer.setLineCap(kCALineCapRound);
                layer.setLineJoin(kCALineJoinRound);
            }
            layer
        });
        // Rims go under every ink, as in the composite: rims at the bottom of the stack, inks on top.
        let rims_end = o.stroke_layers.len() as u32;
        o.marks.insertSublayer_atIndex(&pair[0], rims_end);
        o.marks.addSublayer(&pair[1]);
        o.stroke_layers.push(pair);
    }
    if let (Some(stroke), Some(pair)) = (o.ink.strokes.last(), o.stroke_layers.last()) {
        let path = ink::path(stroke);
        for layer in pair {
            layer.setPath(Some(&path));
        }
    }
    CATransaction::commit();
}

fn point(view: &NSView, event: &NSEvent) -> NSPoint {
    view.convertPoint_fromView(event.locationInWindow(), None)
}

/// The view point `p` in screen coordinates.
fn screen_point(o: &Open, p: NSPoint) -> NSPoint {
    NSPoint::new(o.screen.origin.x + p.x, o.screen.origin.y + p.y)
}

const KEY_ESCAPE: u16 = 53;
const KEY_RETURN: u16 = 36;
const KEY_ENTER: u16 = 76;
const KEY_DELETE: u16 = 51;
const KEY_Z: u16 = 6;

/// Keys while drawing mode is up: Esc closes, Return sends (the only key that sends), ⌘Z or ⌃Z or ⌫ undoes the last
/// stroke, ⌘⌫ or ⌃⌫ clears. Esc and Return go through the island, as its Close and Send capsules do. Returns
/// whether the key was ours.
fn key(event: &NSEvent) -> bool {
    let flags = event.modifierFlags();
    let command = flags.contains(NSEventModifierFlags::Command) || flags.contains(NSEventModifierFlags::Control);
    match (event.keyCode(), command) {
        (KEY_ESCAPE, _) => {
            if with_open(|o| !o.sending) == Some(true) {
                island::apply(Event::Click(Target::DrawClose));
            }
        }
        (KEY_RETURN | KEY_ENTER, _) => island::apply(Event::Click(Target::DrawSend)),
        (KEY_Z, true) | (KEY_DELETE, false) => undo(),
        (KEY_DELETE, true) => clear(),
        _ => return false,
    }
    true
}

define_class!(
    /// A borderless panel that can become key without activating the app (borderless windows refuse by default).
    #[unsafe(super(NSPanel))]
    #[thread_kind = MainThreadOnly]
    #[name = "HeimdallDrawOverlayPanel"]
    pub(crate) struct OverlayPanel;

    impl OverlayPanel {
        #[unsafe(method(canBecomeKeyWindow))]
        fn can_become_key(&self) -> bool {
            true
        }

        #[unsafe(method(canBecomeMainWindow))]
        fn can_become_main(&self) -> bool {
            false
        }
    }
);

define_class!(
    /// The drawing surface: takes every mouse and key event; the strokes are layers on it.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "HeimdallDrawInkView"]
    pub(crate) struct InkView;

    impl InkView {
        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }

        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            true
        }

        #[unsafe(method(isOpaque))]
        fn is_opaque(&self) -> bool {
            false
        }

        // No drawing: the surface is a layer whose background is nearly clear (never fully: belt and braces with
        // ignoresMouseEvents false), so it has no screen-sized bitmap.
        #[unsafe(method(wantsUpdateLayer))]
        fn wants_update_layer(&self) -> bool {
            true
        }

        #[unsafe(method(updateLayer))]
        fn update_layer(&self) {
            if let Some(layer) = self.layer() {
                layer.setBackgroundColor(Some(&CGColor::new_srgb(0.0, 0.0, 0.0, 0.01)));
            }
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            let p = point(self, event);
            // On one of the island's targets (Send, Close, undo, clear): the island's press, not ink.
            let Some(sp) = with_open(|o| screen_point(o, p)) else { return };
            if island::host::press_at(sp) {
                with_open(|o| o.island_press = true);
                return;
            }
            with_open(|o| {
                if !o.sending {
                    o.ink.begin(Pt { x: p.x, y: p.y });
                    sync_strokes(o);
                }
            });
            publish();
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            let p = point(self, event);
            with_open(|o| {
                if !o.island_press && o.ink.extend(Pt { x: p.x, y: p.y }) {
                    sync_strokes(o);
                }
            });
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, event: &NSEvent) {
            let p = point(self, event);
            let island_press = with_open(|o| std::mem::take(&mut o.island_press)).unwrap_or(false);
            if island_press {
                if let Some(sp) = with_open(|o| screen_point(o, p)) {
                    island::host::release_at(sp);
                }
                return;
            }
            with_open(|o| o.ink.end());
            publish();
        }

        #[unsafe(method(mouseMoved:))]
        fn mouse_moved(&self, event: &NSEvent) {
            let p = point(self, event);
            let Some((sp, live)) = with_open(|o| (screen_point(o, p), o.mode == Mode::Live)) else { return };
            let over = island::host::hover_at(Some(sp));
            if live {
                if over { NSCursor::arrowCursor().set() } else { NSCursor::crosshairCursor().set() }
            }
        }

        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) {
            key(event);
        }

        #[unsafe(method(performKeyEquivalent:))]
        fn perform_key_equivalent(&self, event: &NSEvent) -> bool {
            let flags = event.modifierFlags();
            (flags.contains(NSEventModifierFlags::Command) || flags.contains(NSEventModifierFlags::Control)) && key(event)
        }

        // Right clicks and scrolls stop here too; nothing below should move while drawing.
        #[unsafe(method(rightMouseDown:))]
        fn right_mouse_down(&self, _event: &NSEvent) {}

        #[unsafe(method(scrollWheel:))]
        fn scroll_wheel(&self, _event: &NSEvent) {}
    }
);
