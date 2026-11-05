//! The live island: one window at the notch showing `Tree`, one transparent click target over the island's targets,
//! and the clock. Everything is driven by the object's own answers:
//! - `needs_frames` Moving: the view's display link (every refresh of the display, 120 Hz on ProMotion);
//! - Loop: one repeating timer at the loop's rate (the drawing light's breathing, 20 fps). The thinking dots and the
//!   spinner loop as their layers' own Core Animation (`paint.rs`, `Island::needs_ticks`): no timer, no commits;
//! - None: nothing, except one one-shot timer at `deadline()`.
//! At rest the window is ordered out and no timer or link exists (DESIGN.md §6: 0 % CPU at rest).
//!
//! Input: the click target turns mouse down/up into `Press` and `Click` on what `hit` finds (a trackpad tap is a down
//! and an up on the same target), the pointer into `Hover` (its target) and `Pointer` (over the shape or not), the wheel into `Scroll`. At rest, while there is a last
//! reply, the click target stays over the notch alone (transparent, drawing nothing, no timer): pulling it down — a
//! click, a drag down or a two-finger swipe down — shows the reply; dragging its lip band up pulls it up again. Drawing mode's overlay asks the
//! same `hit` before it treats a press as ink, so Send takes a tap wherever the pointer is (recon 18 Oct: "I cannot
//! click send with my finger"). Outputs go to the app and the overlay on the next turn of the main queue, never
//! re-entrantly.

use std::cell::RefCell;
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::Instant;

use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{define_class, msg_send, sel, AllocAnyThread, MainThreadOnly};
use objc2_app_kit::{
    NSBackingStoreType, NSColor, NSEvent, NSEventType, NSMainMenuWindowLevel, NSPanel, NSScreen, NSTrackingArea, NSTrackingAreaOptions, NSView,
    NSWindowAnimationBehavior, NSWindowStyleMask,
};
use objc2_foundation::{MainThreadMarker, NSPoint, NSRect, NSRunLoop, NSRunLoopCommonModes, NSTimer};
use objc2_quartz_core::CAFrameRateRange;

use super::super::util::{ns, overlay_behavior, rect};
use super::field::{sync_digits, Fields, KeyPanel};
use super::measure::AppKitMeasure;
use super::paint::Tree;
use crate::island::object::{Display, Event, FrameNeed, Island, Output, Point, Target, Token};
use crate::island::{HealthControl, ISLAND_WINDOW_TITLE};
use crate::model::UiAction;

/// The island window's width: the widest layout (480) with room for the spring's overshoot, the flares and the glow.
pub(crate) const WINDOW_W: f64 = crate::island::object::sim::WINDOW_W;
/// The click target's window title.
pub(crate) const CONTROL_WINDOW_TITLE: &str = "Heimdall Island Control";
/// A drag or swipe this long (points) pulls the reply down or up.
pub(crate) const PULL: f64 = 8.0;
/// An apply that took longer than a 120 Hz frame delays the motion it started by that much (`Island::delay_motion`).
const FRAME_LATE: f64 = 1.0 / 120.0;

/// Where the island sits: the display it is on (island geometry) and, in screen coordinates (y up), its notch's
/// centre line and the display's top edge.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Place {
    pub display: Display,
    pub center_x: f64,
    pub top: f64,
    pub id: u32,
}

impl Place {
    pub(crate) fn of(screen: &NSScreen) -> Place {
        let frame = screen.frame();
        let inset = screen.safeAreaInsets().top;
        let scale = screen.backingScaleFactor();
        let (left, right) = (screen.auxiliaryTopLeftArea().size.width, screen.auxiliaryTopRightArea().size.width);
        let notch = (inset > 0.0 && left > 0.0 && right > 0.0).then(|| (frame.size.width - left - right, inset));
        let center_x = match notch {
            Some((core, _)) => frame.origin.x + left + core / 2.0,
            None => frame.origin.x + frame.size.width / 2.0,
        };
        let display = Display { width: frame.size.width, height: frame.size.height, scale, notch }.sanitized();
        Place { display, center_x, top: frame.origin.y + frame.size.height, id: screen.CGDirectDisplayID() }
    }

    /// The user's 16" MacBook Pro, placed far off every display (checks and renders: nothing reaches the screen).
    pub(crate) fn offscreen() -> Place {
        Place { display: Display::MBP16, center_x: -60_000.0, top: 60_000.0, id: 0 }
    }

    pub(crate) fn window_h(&self) -> f64 {
        self.display.island_max_h()
    }

    /// An island point in screen coordinates.
    pub(crate) fn to_screen(&self, p: Point) -> NSPoint {
        NSPoint::new(self.center_x + p.x, self.top - p.y)
    }

    pub(crate) fn from_screen(&self, p: NSPoint) -> Point {
        Point::new(p.x - self.center_x, self.top - p.y)
    }
}

/// Main-thread cost accounting (offscreen measurement and the live window's own numbers).
#[derive(Clone, Debug, Default)]
pub(crate) struct Stats {
    pub frames: usize,
    pub seconds: Vec<f64>,
    pub ticks: usize,
}

pub(crate) struct Host {
    pub island: Island,
    pub tree: Tree,
    pub place: Place,
    pub(crate) epoch: Instant,
    panel: Retained<NSPanel>,
    view: Retained<HostView>,
    control: Retained<NSPanel>,
    control_on: Option<NSRect>,
    /// The view's display link, made once and kept: paused at rest, running while the island moves.
    link: Option<Retained<AnyObject>>,
    link_on: bool,
    loop_timer: Option<(f64, Retained<NSTimer>)>,
    deadline_timer: Option<(f64, Retained<NSTimer>)>,
    /// While the pointer counts as over the island or a target and no other clock runs: `pointer_left`'s poll
    /// (`POINTER_POLL`), so a stale hover never outlives an exit event that did not come.
    pointer_timer: Option<Retained<NSTimer>>,
    pressed: Option<Target>,
    /// Where the press on `pressed` came from (`click_source`): a click on a card option grants only from his hand.
    pressed_source: crate::island::click_source::EventSource,
    /// The native text fields (the Other rows, the composer) and the digit keys the card on screen has registered.
    pub(crate) fields: Fields,
    digits: usize,
    /// Where the press began (screen points), for a drag that pulls the reply down or up.
    pressed_at: Option<NSPoint>,
    /// A swipe down over the notch at rest, in points, and when it last moved.
    swipe: (f64, Option<Instant>),
    /// Night 1022: a press on the pulled-down reply's body or band, where it began, and whether it became a pull.
    pull_from: Option<NSPoint>,
    pulling: bool,
    hovered: Option<Target>,
    /// The pointer is over the island's shape (what the island was last told with `Event::Pointer`).
    over: bool,
    /// Checks and renders: windows built but never ordered onto a display.
    pub offscreen: bool,
    /// While drawing mode is up, the island stays on the overlay's display.
    pub pinned: Option<u32>,
    pub stats: Stats,
    /// Virtual time for checks (`None`: the wall clock since launch).
    pub clock: Option<f64>,
    /// Lab instances only (`heimdall --lab-scenario`): every event and painted frame as JSONL (`mac::lab`).
    pub lab_log: Option<std::io::BufWriter<std::fs::File>>,
    /// Lab timing of the frame being made: the display link's timestamp and target timestamp and the tick's start
    /// (CLOCK_UPTIME_RAW seconds), the event's apply, prewarm and renderer wake (ms).
    lab_tick: LabTick,
    /// The drop target's swell and the display its drag is on (`set_drop`): the tree wears it while the island is on
    /// that display, also when the island moves there mid-drag.
    drop: Option<(crate::island::drop::Swell, u32)>,
}

#[derive(Clone, Copy, Debug, Default)]
struct LabTick {
    link: Option<(f64, f64)>,
    start: f64,
    apply_ms: f64,
    prewarm_ms: f64,
    wake_ms: f64,
}

/// The bench's timings of the last apply (`heimdall-ui-demo --bench-chat`): the island's apply, prewarm, the frame
/// after it, its layer tree and its commit, in nanoseconds.
pub(crate) static B_APPLY_NS: AtomicU64 = AtomicU64::new(0);
pub(crate) static B_PREWARM_NS: AtomicU64 = AtomicU64::new(0);
pub(crate) static B_AFTER_NS: AtomicU64 = AtomicU64::new(0);
pub(crate) static B_TREE_NS: AtomicU64 = AtomicU64::new(0);
pub(crate) static B_COMMIT_NS: AtomicU64 = AtomicU64::new(0);

type Sink = Box<dyn Fn(UiAction)>;
type HealthSink = Box<dyn Fn(String, HealthControl)>;

thread_local! {
    pub(crate) static HOST: RefCell<Option<Host>> = const { RefCell::new(None) };
    static SINK: RefCell<Option<Sink>> = const { RefCell::new(None) };
    static HEALTH: RefCell<Option<HealthSink>> = const { RefCell::new(None) };
}

pub(crate) fn set_sink(sink: Sink) {
    SINK.with(|s| *s.borrow_mut() = Some(sink));
}

pub(crate) fn set_health(h: HealthSink) {
    HEALTH.with(|s| *s.borrow_mut() = Some(h));
}

pub(crate) fn emit(action: UiAction) {
    SINK.with(|s| {
        if let Ok(s) = s.try_borrow() {
            if let Some(sink) = s.as_ref() {
                sink(action);
            }
        }
    });
}

/// Runs `f` on the host (main thread), if it exists and is not already borrowed.
pub(crate) fn with_host<R>(f: impl FnOnce(&mut Host) -> R) -> Option<R> {
    HOST.with(|h| h.try_borrow_mut().ok()?.as_mut().map(f))
}

/// The island's and its click target's window levels: above the menu bar and its items, below menus (pop-up menu
/// level 101); while drawing mode is up, above its overlay, which sits above menus.
pub(crate) fn levels(drawing: bool) -> (isize, isize) {
    let island = if drawing { crate::mac::draw::overlay::LEVEL + 1 } else { NSMainMenuWindowLevel + 3 };
    (island, island + 1)
}

fn make_panel(mtm: MainThreadMarker, title: &str, level: isize, mouse: bool) -> Retained<NSPanel> {
    let style = NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel;
    // The click target alone may become key (for a text field in it; `KeyPanel`).
    let panel = if mouse {
        KeyPanel::new(rect(0.0, 0.0, 10.0, 10.0), style, mtm)
    } else {
        NSPanel::initWithContentRect_styleMask_backing_defer(NSPanel::alloc(mtm), rect(0.0, 0.0, 10.0, 10.0), style, NSBackingStoreType::Buffered, false)
    };
    panel.setTitle(&ns(title));
    panel.setSharingType(crate::mac::streams::sharing());
    panel.setIgnoresMouseEvents(!mouse);
    panel.setOpaque(false);
    panel.setHasShadow(false);
    panel.setBackgroundColor(Some(&NSColor::clearColor()));
    panel.setFloatingPanel(true);
    panel.setBecomesKeyOnlyIfNeeded(true);
    panel.setHidesOnDeactivate(false);
    panel.setAnimationBehavior(NSWindowAnimationBehavior::None);
    unsafe { panel.setReleasedWhenClosed(false) };
    panel.setLevel(level);
    panel.setCollectionBehavior(overlay_behavior());
    // The island is always dark: its fields' text, caret and selection take the dark look.
    objc2_app_kit::NSAppearanceCustomization::setAppearance(&*panel, objc2_app_kit::NSAppearance::appearanceNamed(unsafe { objc2_app_kit::NSAppearanceNameDarkAqua }).as_deref());
    panel
}

impl Host {
    /// Builds the island's windows (ordered out) on `place`.
    pub(crate) fn new(mtm: MainThreadMarker, place: Place, offscreen: bool) -> Host {
        let mut island = Island::new(place.display, Box::new(AppKitMeasure::default()));
        let epoch = Instant::now();
        let wall = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64() * 1000.0);
        island.set_wall_origin(wall);
        let size = (WINDOW_W, place.window_h());
        let tree = Tree::new(size, place.display.scale);
        let (island_level, control_level) = levels(false);
        let panel = make_panel(mtm, ISLAND_WINDOW_TITLE, island_level, false);
        let view: Retained<HostView> = unsafe { msg_send![HostView::alloc(mtm), initWithFrame: rect(0.0, 0.0, size.0, size.1)] };
        view.setLayer(Some(&tree.root));
        view.setWantsLayer(true);
        panel.setContentView(Some(&view));
        let control = make_panel(mtm, CONTROL_WINDOW_TITLE, control_level, true);
        let cv: Retained<ControlView> = unsafe { msg_send![ControlView::alloc(mtm), initWithFrame: rect(0.0, 0.0, 10.0, 10.0)] };
        cv.setAutoresizingMask(objc2_app_kit::NSAutoresizingMaskOptions::ViewWidthSizable | objc2_app_kit::NSAutoresizingMaskOptions::ViewHeightSizable);
        let tracking = unsafe {
            NSTrackingArea::initWithRect_options_owner_userInfo(
                NSTrackingArea::alloc(),
                rect(0.0, 0.0, 10.0, 10.0),
                NSTrackingAreaOptions::MouseMoved | NSTrackingAreaOptions::MouseEnteredAndExited | NSTrackingAreaOptions::ActiveAlways | NSTrackingAreaOptions::InVisibleRect,
                Some(&cv),
                None,
            )
        };
        cv.addTrackingArea(&tracking);
        control.setContentView(Some(&cv));
        if offscreen {
            panel.setAlphaValue(0.0);
            control.setAlphaValue(0.0);
        }
        let mut host = Host {
            island,
            tree,
            place,
            epoch,
            panel,
            view,
            control,
            control_on: None,
            link: None,
            link_on: false,
            loop_timer: None,
            deadline_timer: None,
            pointer_timer: None,
            pressed: None,
            pressed_source: crate::island::click_source::EventSource::NONE,
            fields: Fields::default(),
            digits: 0,
            pressed_at: None,
            swipe: (0.0, None),
            pull_from: None,
            pulling: false,
            hovered: None,
            over: false,
            offscreen,
            pinned: None,
            stats: Stats::default(),
            clock: None,
            lab_log: None,
            lab_tick: LabTick::default(),
            drop: None,
        };
        host.make_link();
        host.place_windows();
        let control = host.control.clone();
        host.fields.prepare_composer(&control, mtm);
        host
    }

    /// A live window just ordered in joins the overlay space, so Space swipes slide only what is behind it
    /// (`overlay_space`). Checks and renders (offscreen, virtual clock) never reach SkyLight.
    pub(crate) fn join_overlay(&self, window: &NSPanel, title: &str) {
        if !self.offscreen && self.clock.is_none() {
            super::super::overlay_space::join(window, title);
        }
    }

    pub(crate) fn now(&self) -> f64 {
        self.clock.unwrap_or_else(|| self.epoch.elapsed().as_secs_f64())
    }

    fn place_windows(&self) {
        let (w, h) = (WINDOW_W, self.place.window_h());
        let frame = rect(self.place.center_x - w / 2.0, self.place.top - h, w, h);
        self.panel.setFrame_display(frame, false);
        self.view.setFrame(rect(0.0, 0.0, w, h));
    }

    /// The island moves to the display the user is on (when it starts showing, unless drawing mode pins it).
    fn follow_pointer(&mut self, mtm: MainThreadMarker) {
        if self.offscreen || self.clock.is_some() {
            return;
        }
        let screen = match self.pinned {
            Some(id) => NSScreen::screens(mtm).iter().find(|s| s.CGDirectDisplayID() == id),
            None => super::super::util::pointer_screen(mtm),
        };
        let Some(screen) = screen else { return };
        let place = Place::of(&screen);
        if place != self.place {
            let size_changed = place.window_h() != self.place.window_h() || place.display.scale != self.place.display.scale;
            self.place = place;
            if size_changed {
                // A new tree for the new size and scale (the old rasters were drawn at the old scale).
                let tree = Tree::new((WINDOW_W, place.window_h()), place.display.scale);
                // Notes are drawn again at the new scale (kept as they were, a 1x table showed stretched on the
                // Retina display: bug note 2026-10-24T14-01-17).
                let notes = std::mem::take(&mut self.tree.rasters.notes).into_iter().map(|(id, doc)| (id, doc.at_scale(place.display.scale))).collect();
                let order = std::mem::take(&mut self.tree.rasters.note_order);
                self.tree = tree;
                self.tree.rasters.notes = notes;
                self.tree.rasters.note_order = order;
                self.view.setLayer(Some(&self.tree.root));
            }
            self.place_windows();
            let now = self.now();
            self.island.apply(Event::Display(place.display), now);
        }
    }

    /// The one way anything reaches the island.
    pub(crate) fn apply(&mut self, ev: Event) {
        let now = self.now();
        let was_visible = self.island.frame(now).visible;
        if !was_visible && !matches!(ev, Event::Tick | Event::Hover(_) | Event::Pointer(_) | Event::Press(_) | Event::Scroll(_) | Event::WithdrawReply(_) | Event::Chat(_)) {
            if let Some(mtm) = MainThreadMarker::new() {
                self.follow_pointer(mtm);
            }
        }
        let arrival = !matches!(ev, Event::Tick | Event::Hover(_) | Event::Pointer(_) | Event::Press(_) | Event::Scroll(_) | Event::SpeechLevel(_) | Event::WithdrawReply(_) | Event::Chat(_));
        // Something arrives while the island rests: the symbol renderer is woken before the motion's clock starts,
        // so its set-up lands before the first frame instead of between two frames of the growing shape.
        let started = Instant::now();
        let woke = arrival && !was_visible && !self.offscreen && self.clock.is_none() && !self.panel.isVisible() && self.tree.wake_renderer();
        let wake_ms = started.elapsed().as_secs_f64() * 1000.0;
        let now = if woke { self.now() } else { now };
        if let Some(log) = self.lab_log.as_mut() {
            super::super::lab::log_event(log, now, &ev);
        }
        let t0 = Instant::now();
        let motion_before = self.island.motion().0;
        self.island.apply(ev, now);
        let t1 = Instant::now();
        B_APPLY_NS.store((t1 - t0).as_nanos() as u64, Relaxed);
        if arrival {
            // Text and symbols are drawn when their state arrives (the settled look's slots), not on the frame where
            // they first fade in mid-motion.
            let settled = self.island.frame(now + 2.0);
            self.tree.prewarm(&settled);
        }
        B_PREWARM_NS.store(t1.elapsed().as_nanos() as u64, Relaxed);
        // A motion this apply started begins when its first frame can paint: a slow apply or prewarm delays the
        // opening, never skips it (live clock only; virtual time does not move while the work runs).
        let started_motion = self.island.motion().0;
        if self.clock.is_none() && started_motion != motion_before {
            let late = self.now() - now;
            if late > FRAME_LATE {
                self.island.delay_motion(started_motion, late);
            }
        }
        if self.lab_log.is_some() {
            self.lab_tick.apply_ms = (t1 - t0).as_secs_f64() * 1000.0;
            self.lab_tick.prewarm_ms = t1.elapsed().as_secs_f64() * 1000.0;
            self.lab_tick.wake_ms = if woke { wake_ms } else { 0.0 };
        }
        // The frame painted is the state at paint time, after the drawing ahead (live clock only).
        let now = if self.clock.is_none() { self.now() } else { now };
        let ta = Instant::now();
        self.after(now);
        B_AFTER_NS.store(ta.elapsed().as_nanos() as u64, Relaxed);
    }

    /// ⌃⌘H: pulls the conversation down as the user's and puts the keyboard in its composer once the composer is placed.
    /// Pressed again while his conversation is open, however it opened (⌃⌘H, a click) and wherever the keyboard is,
    /// it pulls it up (bug note 2026-10-26T19-14-21-2: "I should be just able to open the island whenever I want").
    /// A pull-down an agent's note opened becomes his conversation, with the keyboard; one hidden under the reader or
    /// drawing mode is never closed unseen.
    pub(crate) fn open_chat(&mut self) {
        if self.island.owners_reply_open() && self.island.class() == crate::island::object::Class::Reply {
            self.apply(Event::Click(Target::ReplyBand));
            return;
        }
        self.fields.focus_composer();
        self.apply(Event::OpenChat);
        if !self.island.reply_open() {
            self.fields.cancel_focus();
        }
    }

    /// After a change: outputs, a frame, the clock.
    fn after(&mut self, now: f64) {
        self.pointer_left(now);
        let outputs = self.island.take_outputs();
        if !outputs.is_empty() {
            DispatchQueue::main().exec_async(move || outputs.into_iter().for_each(dispatch));
        }
        self.render(now);
        self.schedule(now);
    }

    /// Paints the frame at `now` and puts the windows where it needs them.
    pub(crate) fn render(&mut self, now: f64) {
        let started = Instant::now();
        let up = if self.lab_log.is_some() { super::super::lab::uptime() } else { 0.0 };
        let f = self.island.frame(now);
        self.tree.drop = self.drop.filter(|(_, id)| *id == self.place.id).map(|(s, _)| s);
        let tt = Instant::now();
        // Its clock skips loops of dots and spinner (`Island::needs_ticks`): their layers loop on their own.
        self.tree.loop_layers = true;
        self.tree.apply(&f);
        B_TREE_NS.store(tt.elapsed().as_nanos() as u64, Relaxed);
        B_COMMIT_NS.store((self.tree.last_commit_ms * 1e6) as u64, Relaxed);
        let draw = started.elapsed().as_secs_f64();
        self.stats.frames += 1;
        self.stats.seconds.push(draw);
        let shown = self.panel.isVisible();
        if self.stats.seconds.len() > 10_000 {
            self.stats.seconds.drain(..5_000);
        }
        let windows = Instant::now();
        // The light and the pointing marks are on the same frame (their windows' own state, not the host's).
        super::super::draw::overlay::paint_light(&f.light, &f.shape);
        super::super::draw::point::set_opacity(f.pointing);
        // The drop target follows the island's frame (its catcher's hit area, and who draws the swell).
        super::super::drop_target::island_moved(self.place.id, f.visible.then_some(f.shape));
        let show = f.visible || f.glow > 0.001;
        // Offscreen hosts order in too (alpha 0, parked far from every display), so synthetic events reach them.
        if show && !self.panel.isVisible() {
            self.panel.orderFrontRegardless();
            self.join_overlay(&self.panel, ISLAND_WINDOW_TITLE);
        } else if !show && !f.moving && self.panel.isVisible() {
            self.panel.orderOut(None);
        }
        // The click target covers the island while it offers anything to click; at rest that is the notch alone,
        // while there is a last reply to pull down.
        let want = (!self.island.targets().is_empty()).then(|| {
            let (w, h) = self.island.target_size();
            rect(self.place.center_x - w / 2.0, self.place.top - h, w, h)
        });
        if want != self.control_on {
            match want {
                Some(r) => {
                    self.control.setFrame_display(r, false);
                    self.control.orderFrontRegardless();
                    self.join_overlay(&self.control, CONTROL_WINDOW_TITLE);
                }
                None => self.control.orderOut(None),
            }
            self.control_on = want;
        }
        // ⌃⌘H's focus request lives only while its conversation is open: closed some other way first (its band, a
        // send), a later click on the notch never takes the keyboard.
        if !self.island.reply_open() {
            self.fields.cancel_focus();
        }
        // Fields and digit keys follow the frame: a field sits on its slot in the click target's window, and the digits
        // are registered only while a card on screen answers them (never in renders or checks).
        if let Some(mtm) = MainThreadMarker::new() {
            match self.control.contentView() {
                Some(cv) if want.is_some() => {
                    let cards: HashSet<String> = self.island.card_ids().into_iter().collect();
                    self.fields.sync(&f, &cv, self.island.target_size(), &cards, mtm);
                }
                _ => self.fields.hide(),
            }
            // A field that had the keyboard left (sent, pulled up): the click target gives the keyboard back to the app
            // the user was in. Ordering the non-activating panel out does that; it comes straight back for the notch's
            // target, without the keyboard.
            if self.fields.take_released() && self.control.isKeyWindow() {
                self.control.orderOut(None);
                if self.control_on.is_some() {
                    self.control.orderFrontRegardless();
                    self.join_overlay(&self.control, CONTROL_WINDOW_TITLE);
                }
            }
            // Every live frame, not only at rest: a tab change or a close during a motion releases or takes its digits
            // at once (the user: the keys follow the card's current tab).
            if !self.offscreen {
                self.digits = sync_digits(self.digits, self.island.card_keys(), mtm);
            }
        }
        if self.lab_log.is_some() {
            let (class, need) = (self.island.class(), self.island.needs_frames(now));
            let speaking = self.island.speaking(now);
            let lt = std::mem::take(&mut self.lab_tick);
            let timing = serde_json::json!({
                "up": up, "tick_up": lt.start, "link_ts": lt.link.map(|l| l.0), "link_target": lt.link.map(|l| l.1),
                "apply_ms": lt.apply_ms, "prewarm_ms": lt.prewarm_ms, "wake_ms": lt.wake_ms, "commit_ms": self.tree.last_commit_ms,
                "win_ms": windows.elapsed().as_secs_f64() * 1000.0, "end_up": super::super::lab::uptime(),
            });
            if let Some(log) = self.lab_log.as_mut() {
                super::super::lab::log_frame(log, &f, class, need, draw, shown, speaking, timing);
            }
        }
    }

    /// Where a drop goes now (`drop::route`): the open pull-down's composer while he writes there (text, or the
    /// keyboard in it), else the agent.
    pub(crate) fn drop_route(&self) -> crate::island::drop::Route {
        self.island.drop_route(self.fields.composer_focused())
    }

    /// While a drag is under way the click target lets it through to the drop target's catcher (30 Oct 03:00: the
    /// open pull-down took no drop). The click target joins the overlay space, which macOS orders above the catcher's
    /// ordinary stack whatever their levels, and it registers for no drags: over the pull-down (and over the notch at
    /// rest) a drag found it first and was never offered to the catcher. Clicks are back the moment the drag ends.
    pub(crate) fn pass_drags(&self, on: bool) {
        if self.control.ignoresMouseEvents() != on {
            self.control.setIgnoresMouseEvents(on);
        }
    }

    /// The island's shape while it shows on display `display`.
    pub(crate) fn island_on(&self, display: u32) -> Option<crate::island::object::Shape> {
        let f = self.island.frame(self.now());
        (f.visible && display == self.place.id).then_some(f.shape)
    }

    /// A drag over the island (`island::drop::Swell`, or `None`) on display `display`: repainted at once while the
    /// island shows there. Returns the island's shape while it shows on that display, so the drop target draws no
    /// shape of its own then; on another display the island keeps its own shape.
    pub(crate) fn set_drop(&mut self, swell: Option<crate::island::drop::Swell>, display: u32) -> Option<crate::island::object::Shape> {
        let now = self.now();
        let f = self.island.frame(now);
        let drop = swell.map(|s| (s, display));
        if drop != self.drop {
            self.drop = drop;
            if f.visible {
                self.render(now);
            }
        }
        (f.visible && display == self.place.id).then_some(f.shape)
    }

    /// Runs the clock the island asks for: a display link while it moves, a timer while only a loop moves, a
    /// one-shot timer for the next deadline, and nothing at rest.
    pub(crate) fn schedule(&mut self, now: f64) {
        if self.clock.is_some() {
            return;
        }
        // A loop of animated content alone runs in the render server (its layers' own animation): no timer.
        let need = match self.island.needs_ticks(now) {
            // A window parked off every display gets no display-link callbacks: offscreen hosts step motion with a
            // 120 Hz timer instead.
            FrameNeed::Moving if self.offscreen => FrameNeed::Loop { epoch: now, period: 1.0, fps: 120.0 },
            other => other,
        };
        match need {
            FrameNeed::Moving => {
                self.stop_loop();
                self.start_link();
            }
            FrameNeed::Loop { fps, .. } => {
                self.stop_link();
                if self.loop_timer.as_ref().is_none_or(|(f, _)| *f != fps) {
                    self.stop_loop();
                    let timer = unsafe {
                        NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(1.0 / fps.max(1.0), &self.view, sel!(islandLoop:), None, true)
                    };
                    unsafe { NSRunLoop::currentRunLoop().addTimer_forMode(&timer, NSRunLoopCommonModes) };
                    self.loop_timer = Some((fps, timer));
                }
            }
            FrameNeed::None => {
                self.stop_link();
                self.stop_loop();
            }
        }
        match self.island.deadline() {
            Some(d) if self.deadline_timer.as_ref().is_none_or(|(at, _)| (*at - d).abs() > 1e-6) => {
                if let Some((_, t)) = self.deadline_timer.take() {
                    t.invalidate();
                }
                let timer = unsafe {
                    NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats((d - now).max(0.0), &self.view, sel!(islandDeadline:), None, false)
                };
                unsafe { NSRunLoop::currentRunLoop().addTimer_forMode(&timer, NSRunLoopCommonModes) };
                self.deadline_timer = Some((d, timer));
            }
            None => {
                if let Some((_, t)) = self.deadline_timer.take() {
                    t.invalidate();
                }
            }
            _ => {}
        }
        // A pointer counted as over the island is checked against where it is a few times a second while nothing
        // else ticks (the link and the loop timer run `pointer_left` on their own ticks). It stops once neither
        // `over` nor a hover is set: at rest nothing ticks.
        if pointer_watch(self.offscreen, self.over, self.hovered.is_some(), self.link_on || self.loop_timer.is_some()) {
            if self.pointer_timer.is_none() {
                let timer = unsafe {
                    NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(POINTER_POLL, &self.view, sel!(islandPointer:), None, true)
                };
                unsafe { NSRunLoop::currentRunLoop().addTimer_forMode(&timer, NSRunLoopCommonModes) };
                self.pointer_timer = Some(timer);
            }
        } else if let Some(t) = self.pointer_timer.take() {
            t.invalidate();
        }
    }

    /// `pointer_left`'s poll: paints only when the pointer turned out to have left (or moved to another target).
    fn pointer_poll(&mut self) {
        let now = self.now();
        let before = (self.over, self.hovered.clone());
        self.pointer_left(now);
        if (self.over, self.hovered.clone()) != before {
            self.after(now);
        } else {
            self.schedule(now);
        }
    }

    /// The view's display link, made at launch and paused: a motion only un-pauses it, so the first motion does not
    /// create a link (and wait for the display's first callbacks) on its own first frames.
    fn make_link(&mut self) {
        if self.link.is_some() {
            return;
        }
        let link: Option<Retained<AnyObject>> = unsafe { msg_send![&*self.view, displayLinkWithTarget: &*self.view, selector: sel!(islandFrame:)] };
        let Some(link) = link else { return };
        unsafe {
            let _: () = msg_send![&*link, addToRunLoop: &*NSRunLoop::currentRunLoop(), forMode: NSRunLoopCommonModes];
            let _: () = msg_send![&*link, setPaused: true];
        }
        self.link = Some(link);
    }

    /// Starts the frame clock: un-pauses the link, which is kept between motions. While it runs the display is asked
    /// for 120 Hz.
    fn start_link(&mut self) {
        if self.link_on {
            return;
        }
        self.make_link();
        if let Some(link) = self.link.as_ref() {
            let range = CAFrameRateRange { minimum: 120.0, maximum: 120.0, preferred: 120.0 };
            unsafe {
                let _: () = msg_send![&**link, setPreferredFrameRateRange: range];
                let _: () = msg_send![&**link, setPaused: false];
            }
            self.link_on = true;
        }
    }

    /// Pauses the frame clock: no callbacks, no cost, the object kept for the next motion.
    fn stop_link(&mut self) {
        if !self.link_on {
            return;
        }
        if let Some(link) = self.link.as_ref() {
            unsafe {
                let _: () = msg_send![&**link, setPaused: true];
            }
        }
        self.link_on = false;
    }

    fn stop_loop(&mut self) {
        if let Some((_, t)) = self.loop_timer.take() {
            t.invalidate();
        }
    }

    /// A frame, loop step or deadline: time passed.
    fn tick(&mut self) {
        if self.lab_log.is_some() {
            self.lab_tick.start = super::super::lab::uptime();
        }
        let now = self.now();
        self.stats.ticks += 1;
        if self.island.deadline().is_some_and(|d| d <= now + 1e-4) {
            self.island.apply(Event::Tick, now);
            self.deadline_timer = None;
        }
        self.after(now);
    }

    /// Whether nothing runs: no link, no loop timer, no deadline timer, the island's window out and the click target
    /// out (or only the notch's, at rest: transparent, drawing nothing).
    pub(crate) fn idle(&self) -> bool {
        !self.link_on && self.loop_timer.is_none() && self.deadline_timer.is_none() && self.pointer_timer.is_none() && !self.panel.isVisible() && (!self.control.isVisible() || self.notch_catcher())
    }

    /// The click target is up at rest over the notch alone (the last reply's pull-down).
    pub(crate) fn notch_catcher(&self) -> bool {
        self.island.class() == crate::island::object::Class::Rest && !self.island.targets().is_empty()
    }

    /// What keeps the clock running (lab reports): a display link, a loop timer, and the deadline timer's due time
    /// from now.
    pub(crate) fn clock_report(&self) -> (bool, bool, Option<f64>) {
        let now = self.now();
        (self.link_on, self.loop_timer.is_some(), self.deadline_timer.as_ref().map(|(at, _)| at - now))
    }

    /// Whether the hosting view and its root layer are flipped at runtime (the island's y runs down from the top).
    pub(crate) fn flipped(&self) -> (bool, bool) {
        (self.view.isFlipped(), self.tree.root.isGeometryFlipped())
    }

    pub(crate) fn panels(&self) -> (Retained<NSPanel>, Retained<NSPanel>) {
        (self.panel.clone(), self.control.clone())
    }

    /// What a click at screen point `p` lands on.
    pub(crate) fn hit_screen(&self, p: NSPoint) -> Option<Target> {
        self.island.hit(self.now(), self.place.from_screen(p))
    }

    /// The words a Copy at screen point `p` takes (`copy`).
    pub(crate) fn copy_at(&self, p: NSPoint) -> Option<String> {
        self.island.copy_at(self.now(), self.place.from_screen(p))
    }

    /// What ⌘C copies with no field selection (`copy`).
    pub(crate) fn copy_shown(&self) -> Option<String> {
        self.island.copy_shown()
    }

    fn pointer_down(&mut self, p: NSPoint, source: crate::island::click_source::EventSource) {
        let target = self.hit_screen(p);
        if matches!(target, Some(Target::DrawSend | Target::DrawClose)) {
            self.apply(Event::Press(target.clone()));
        }
        // Night 1022: a press on the pulled-down reply's body (or its band) may become a pull-out drag.
        let on_reply = self.island.class() == crate::island::object::Class::Reply && self.island.covers(self.now(), self.place.from_screen(p));
        self.pull_from = (on_reply && matches!(target, None | Some(Target::ReplyBand))).then_some(p);
        self.pulling = false;
        self.pressed = target;
        self.pressed_source = source;
        self.pressed_at = Some(p);
    }

    /// A drag from the notch down pulls the last reply down (or brings back a question tucked away); a drag up from its
    /// lip band, or a question card's, pulls it up. Either acts once, as soon as it has moved `PULL` points.
    fn pointer_dragged(&mut self, p: NSPoint) {
        // Night 1022: dragging the pulled-down reply down stretches it (a rubber band, from `PULL` points on);
        // released past `PULL_OUT` it leaves as a floating document island (`Event::Pull`).
        if let Some(start) = self.pull_from {
            let down = start.y - p.y;
            if self.pulling || down >= PULL {
                self.pulling = true;
                self.pressed = None;
                self.pressed_at = None;
                self.apply(Event::Pull(Some((down - PULL).max(0.0))));
                return;
            }
        }
        let (Some(start), Some(pressed)) = (self.pressed_at, self.pressed.clone()) else { return };
        // Screen points run up: down is a smaller y.
        let down = start.y - p.y;
        let pull = match pressed {
            Target::Token(Token::Reply) if down >= PULL => Some(pressed),
            Target::ReplyBand | Target::AskTuck(_) if -down >= PULL => Some(pressed),
            _ => None,
        };
        if let Some(target) = pull {
            self.pressed = None;
            self.pressed_at = None;
            self.apply(Event::Click(target));
        }
    }

    /// The wheel: over the notch at rest, a swipe down of `PULL` points pulls the last reply down; anywhere else it
    /// scrolls the body under the pointer. `fingers_down` is the physical motion (natural scrolling or not).
    pub(crate) fn wheel(&mut self, dy: f64, fingers_down: f64) {
        if self.notch_catcher() {
            let now = Instant::now();
            let fresh = self.swipe.1.is_some_and(|at| now.duration_since(at).as_secs_f64() < 0.3);
            let total = if fresh { self.swipe.0 + fingers_down } else { fingers_down };
            self.swipe = (total, Some(now));
            if total >= PULL {
                self.swipe = (0.0, None);
                if let Some((target, _)) = self.island.targets().iter().find(|(t, _)| *t == Target::Token(Token::Reply)).cloned() {
                    self.apply(Event::Click(target));
                }
            }
            return;
        }
        self.apply(Event::Scroll(dy));
    }

    /// The pointer counted as over the island, or over a target, may have left without an exit event (the click
    /// target ordered out or moved under a still pointer): checked against where it is now, so an agent's note never
    /// waits on a stale "over" and the peek lip never stays drawn at rest on a stale hover (night 1022 review 5).
    /// Leaving the island also tells it the target under the pointer now (the scene drops its hover with
    /// `Pointer(false)`). Offscreen hosts are driven by synthetic events only.
    fn pointer_left(&mut self, now: f64) {
        if (!self.over && self.hovered.is_none()) || self.offscreen {
            return;
        }
        let p = self.place.from_screen(NSEvent::mouseLocation());
        let left = self.over && !self.island.covers(now, p);
        if left {
            self.over = false;
            self.island.apply(Event::Pointer(false), now);
        }
        if left || self.hovered.is_some() {
            let target = self.island.hit(now, p);
            if left || target != self.hovered {
                self.hovered = target.clone();
                self.island.apply(Event::Hover(target), now);
            }
        }
    }

    /// `source`: where the release came from (`click_source`). A press and release on the same card option or Send,
    /// both from the HID system, are `Event::HardwareClick` (the only way a click grants anything); every other click
    /// is `Event::Click`, and on a card the log says where it came from.
    fn pointer_up(&mut self, p: NSPoint, source: crate::island::click_source::EventSource) {
        self.pull_from = None;
        if std::mem::take(&mut self.pulling) {
            self.pressed = None;
            self.pressed_at = None;
            self.apply(Event::Pull(None));
            return;
        }
        let target = self.hit_screen(p);
        self.pressed_at = None;
        let pressed = self.pressed.take();
        if matches!(pressed, Some(Target::DrawSend | Target::DrawClose)) {
            self.apply(Event::Press(None));
        }
        let down = std::mem::replace(&mut self.pressed_source, crate::island::click_source::EventSource::NONE);
        if let (Some(a), Some(b)) = (pressed, target) {
            if a == b {
                let card = matches!(a, Target::AskPick { .. } | Target::AskSend(_));
                let by_hand = crate::island::click_source::real_click(&down, &source);
                if card && !by_hand {
                    eprintln!("island: a click on {a:?} is not User's own pointer (press: {}; release: {}): it grants nothing", down.describe(), source.describe());
                }
                self.apply(if card && by_hand { Event::HardwareClick(a) } else { Event::Click(a) });
            }
        }
    }

    fn pointer_moved(&mut self, p: Option<NSPoint>) {
        let over = p.is_some_and(|p| self.island.covers(self.now(), self.place.from_screen(p)));
        let left = self.over && !over;
        if over != self.over {
            self.over = over;
            self.apply(Event::Pointer(over));
        }
        let target = p.and_then(|p| self.hit_screen(p));
        // Leaving dropped the scene's hover: told again even when the target is the same (the notch at rest).
        if left || target != self.hovered {
            self.hovered = target.clone();
            self.apply(Event::Hover(target));
        }
    }
}

/// How often a pointer counted as over the island is checked while no other clock runs (seconds).
const POINTER_POLL: f64 = 0.125;

/// Whether `pointer_left` needs its own poll: a live window, the pointer counted as over the island or a target, and
/// no link or loop timer ticking (their ticks run it). Before 28 Oct the thinking dots' 30 fps timer ran it; the dots
/// now loop in the render server.
fn pointer_watch(offscreen: bool, over: bool, hovered: bool, ticking: bool) -> bool {
    !offscreen && (over || hovered) && !ticking
}

impl Drop for Host {
    fn drop(&mut self) {
        if let Some(link) = self.link.take() {
            unsafe {
                let _: () = msg_send![&*link, invalidate];
            }
        }
        // Its timers target the view, which outlives it: left running they would tick whichever host comes next.
        self.stop_loop();
        if let Some((_, t)) = self.deadline_timer.take() {
            t.invalidate();
        }
        if let Some(t) = self.pointer_timer.take() {
            t.invalidate();
        }
    }
}

/// An output reaches its owner (main queue, after the change that produced it).
pub(crate) fn dispatch(o: Output) {
    match o {
        Output::Notice(a) => emit(UiAction::AgentNotice(a)),
        Output::Typed(text) => emit(UiAction::AgentWindow(crate::model::AgentWindowAction::Send { text, steer: false })),
        Output::TypedDrop { text, items } => {
            let at_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
            emit(UiAction::TypedWithDrop { text, dropped: crate::island::drop::Dropped { items, at_ms } })
        }
        Output::Health { key, control } => {
            let handled = HEALTH.with(|h| {
                h.try_borrow().ok().and_then(|h| h.as_ref().map(|f| f(key.clone(), control))).is_some()
            });
            if !handled {
                emit(UiAction::Health { key, control });
            }
        }
        Output::Read(r) => emit(UiAction::FastRead(r)),
        Output::Report(r) => emit(UiAction::Island(r)),
        Output::PullOut { text, notes } => emit(UiAction::PullOut { text, notes }),
        Output::StopSpeech => emit(UiAction::StopSpeaking),
        Output::Copy(text) => {
            let board = objc2_app_kit::NSPasteboard::generalPasteboard();
            board.clearContents();
            board.setString_forType(&objc2_foundation::NSString::from_str(&text), unsafe { objc2_app_kit::NSPasteboardTypeString });
        }
        Output::DrawSend => super::super::draw::overlay::send_now(),
        Output::DrawClose => super::super::draw::overlay::closed_by_island(),
        Output::DrawUndo => super::super::draw::overlay::undo(),
        Output::DrawClear => super::super::draw::overlay::clear(),
    }
}

/// Screen point of an event in a window.
/// Where a mouse event came from (`crate::island::click_source`), read from its underlying `CGEvent`.
pub(crate) fn event_source(event: &NSEvent) -> crate::island::click_source::EventSource {
    use objc2_core_graphics::{CGEvent, CGEventField};
    use crate::island::click_source::EventSource;
    let left_button = matches!(event.r#type(), NSEventType::LeftMouseDown | NSEventType::LeftMouseUp);
    let Some(cg) = event.CGEvent() else { return EventSource { left_button, ..EventSource::NONE } };
    let field = |f: CGEventField| CGEvent::integer_value_field(Some(&cg), f);
    EventSource {
        left_button,
        source_pid: Some(field(CGEventField::EventSourceUnixProcessID)),
        source_state: field(CGEventField::EventSourceStateID),
        user_data: field(CGEventField::EventSourceUserData),
    }
}

fn screen_point(view: &NSView, event: &NSEvent) -> NSPoint {
    let p = event.locationInWindow();
    match view.window() {
        Some(w) => {
            let f = w.frame();
            NSPoint::new(f.origin.x + p.x, f.origin.y + p.y)
        }
        None => p,
    }
}

define_class!(
    /// The island's layer-hosting view; the target of its display link and timers.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "HeimdallIslandHostView"]
    pub(crate) struct HostView;

    impl HostView {
        // The layer's y runs down from the notch. AppKit sets the hosted root layer's geometry from this, so the
        // island is drawn top-down in the window (without it the shape hangs upside down from the window's bottom).
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(islandFrame:))]
        fn island_frame(&self, link: &AnyObject) {
            with_host(|h| {
                if h.lab_log.is_some() {
                    let (ts, target): (f64, f64) = unsafe { (msg_send![link, timestamp], msg_send![link, targetTimestamp]) };
                    h.lab_tick.link = Some((ts, target));
                }
                h.tick()
            });
        }

        #[unsafe(method(islandLoop:))]
        fn island_loop(&self, _timer: &NSTimer) {
            with_host(|h| h.tick());
        }

        #[unsafe(method(islandPointer:))]
        fn island_pointer(&self, _timer: &NSTimer) {
            with_host(|h| h.pointer_poll());
        }

        #[unsafe(method(islandDeadline:))]
        fn island_deadline(&self, _timer: &NSTimer) {
            with_host(|h| {
                h.deadline_timer = None;
                h.tick()
            });
        }
    }
);

define_class!(
    /// The island's click target: transparent, over the island while it offers something to click.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "HeimdallIslandControl"]
    pub(crate) struct ControlView;

    impl ControlView {
        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            if super::copy::control_click(self, event) {
                return;
            }
            let p = screen_point(self, event);
            let source = event_source(event);
            with_host(|h| h.pointer_down(p, source));
        }

        /// Right-click over words: Copy (`copy`).
        #[unsafe(method_id(menuForEvent:))]
        fn menu_for_event(&self, event: &NSEvent) -> Option<Retained<objc2_app_kit::NSMenu>> {
            super::copy::menu_for(self, event)
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, event: &NSEvent) {
            let p = screen_point(self, event);
            let source = event_source(event);
            with_host(|h| h.pointer_up(p, source));
        }

        #[unsafe(method(mouseMoved:))]
        fn mouse_moved(&self, event: &NSEvent) {
            let p = screen_point(self, event);
            with_host(|h| h.pointer_moved(Some(p)));
        }

        #[unsafe(method(mouseExited:))]
        fn mouse_exited(&self, _event: &NSEvent) {
            with_host(|h| h.pointer_moved(None));
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            let p = screen_point(self, event);
            with_host(|h| h.pointer_dragged(p));
        }

        #[unsafe(method(scrollWheel:))]
        fn scroll_wheel(&self, event: &NSEvent) {
            let raw = event.scrollingDeltaY();
            // With natural scrolling the fingers moving down give a positive delta; without it, a negative one.
            let fingers_down = if event.isDirectionInvertedFromDevice() { raw } else { -raw };
            with_host(|h| h.wheel(-raw, fingers_down));
        }
    }
);

/// Drawing mode's own controls (Send, Close, undo, clear): the only targets that take a press while the overlay is
/// up. Any other press is ink, even over an open island: it never becomes a click that collapses or closes it (bug
/// note 2026-11-04T21-56-52).
pub(crate) fn draw_control(t: &Target) -> bool {
    matches!(t, Target::DrawSend | Target::DrawClose | Target::DrawUndo | Target::DrawClear)
}

/// The overlay (drawing mode) asks before inking: a press on one of drawing mode's controls is the island's.
pub(crate) fn press_at(p: NSPoint) -> bool {
    with_host(|h| {
        let take = h.hit_screen(p).is_some_and(|t| draw_control(&t));
        if take {
            // Drawing mode's overlay hands the press over: not the island's own event, so it grants nothing.
            h.pointer_down(p, crate::island::click_source::EventSource::NONE);
        }
        take
    })
    .unwrap_or(false)
}

pub(crate) fn release_at(p: NSPoint) {
    with_host(|h| h.pointer_up(p, crate::island::click_source::EventSource::NONE));
}

/// The overlay's pointer: hovers the island; `true` over one of drawing mode's controls (the arrow), else the
/// crosshair draws.
pub(crate) fn hover_at(p: Option<NSPoint>) -> bool {
    with_host(|h| {
        h.pointer_moved(p);
        h.hovered.as_ref().is_some_and(draw_control)
    })
    .unwrap_or(false)
}

#[cfg(test)]
mod pointer_tests {
    use super::pointer_watch;

    /// Bug note 2026-11-04T21-56-52: a press over an open island in drawing mode was handed to the island and became a
    /// click that collapsed it. Only drawing mode's own controls take a press; anything else is ink.
    #[test]
    fn in_drawing_mode_only_the_drawing_controls_take_a_press() {
        use super::{draw_control, Target, Token};
        for t in [Target::DrawSend, Target::DrawClose, Target::DrawUndo, Target::DrawClear] {
            assert!(draw_control(&t), "{t:?}");
        }
        for t in [Target::Token(Token::Reply), Target::ReplyBand, Target::ReplyPin, Target::Hush, Target::ReaderPlay, Target::Composer, Target::NoteClose("n".into())] {
            assert!(!draw_control(&t), "{t:?} is ink in drawing mode");
        }
    }

    /// Review of the 28 Oct loop fix: the dots' 30 fps timer also caught a pointer that left without an exit event.
    /// With the dots in the render server, a hover or "over" with no other clock gets its own poll; at rest (neither
    /// set), offscreen, or while the link or a loop timer ticks, none.
    #[test]
    fn a_stale_hover_is_polled_only_while_nothing_else_ticks() {
        assert!(pointer_watch(false, true, false, false));
        assert!(pointer_watch(false, false, true, false));
        assert!(!pointer_watch(false, false, false, false), "at rest nothing ticks");
        assert!(!pointer_watch(false, true, true, true), "the link or the loop timer already runs pointer_left");
        assert!(!pointer_watch(true, true, true, false), "offscreen hosts are driven by synthetic events");
    }
}
