//! The drop target at the notch (`island::drop`). One window over the notch, ordered in only
//! while a drag is under way anywhere: a global monitor sees the drag pasteboard change as a drag starts (a press-drag
//! whose source writes it late is polled while the button is held, `DragWatch`), the window comes up on the display
//! with the pointer, and goes once the button is up and its motion has ended. With no drag
//! it is ordered out, so it never takes a click or covers the menu bar.
//!
//! Two windows, same frame. The drawn one joins the island's overlay space (`overlay_space`) like every island window,
//! so it stays still while Spaces slide and draws above them; it never takes the mouse. The drag is taken by a second,
//! empty window, the catcher, which stays in the ordinary window stack: macOS never offers a drag to a window in a
//! SkyLight space of a process's own (measured 24 Oct with a live Finder drag: the joined window got no
//! `draggingEntered:`, the same window outside the space got every callback; the user, 12:26: "There is no animation").
//! `lab/drop-on-notch-live/` repeats that drag against a replica.
//!
//! While the island shows more than the notch (speaking, a line, the conversation) the swell and glow are the island's
//! own (`Host::set_drop`, `island::drop::Swell`): its shape swells and lights as one, this window draws no shape of
//! its own, and the catcher covers the island's frame as it changes ("Those two are not
//! connected, they're separate effects").
//!
//! A drag over it swells the notch and lights its edge; a drop reads the pasteboard (`pasteboard.rs`), classifies the
//! items (`island::drop::classify`, saving image data and long text into the drops folder), sends
//! `UiAction::Dropped` to the app, and plays the swallow: the item's thumbnail flies into the shape, shrinking, the
//! shape takes it with one soft bounce and returns into the notch; then the island's own result cue says what it
//! took ("1 image"). Reduce Motion: only fades.

pub(crate) mod check;
pub(crate) mod pasteboard;

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::time::Instant;

use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{define_class, msg_send, sel, AllocAnyThread, MainThreadOnly};
use objc2_app_kit::{
    NSBackingStoreType, NSColor, NSDragOperation, NSDraggingInfo, NSEvent, NSEventMask, NSImage, NSMainMenuWindowLevel, NSPanel, NSPasteboard,
    NSPasteboardNameDrag, NSView, NSWindowAnimationBehavior, NSWindowSharingType, NSWindowStyleMask, NSWorkspace,
};
use objc2_core_foundation::{CFRetained, CGAffineTransform, CGPoint, CGRect, CGSize};
use objc2_core_graphics::{CGColor, CGImage, CGPath};
use objc2_foundation::{MainThreadMarker, NSPoint, NSRunLoop, NSRunLoopCommonModes, NSString, NSTimer};
use objc2_quartz_core::{CALayer, CAShapeLayer, CATransaction};

use super::island::host::Place;
use super::island::paint::outline;
use super::island::raster::Canvas;
use super::util::{ns, overlay_behavior, rect};
use crate::island::drop::{self, DropFrame, DropItem, DropKind, Dropped, Motion, THUMB, WINDOW_TITLE};
use crate::island::object::{Event, Point, Shape};
use crate::island::{CommandCue, CommandKind};
use crate::model::UiAction;

/// The drawn window: the island window's frame (room for the swell, its glow and the item's flight from anywhere
/// over the island).
pub(crate) const PANEL_W: f64 = super::island::host::WINDOW_W;
/// Where a drop is taken: this wide round the notch's centre line, this far down from the top edge.
pub(crate) const ZONE_W: f64 = 340.0;
pub(crate) const ZONE_H: f64 = 100.0;
/// And over the island while it shows: its shape and this much round it (the swell and a little more).
pub(crate) const ISLAND_MARGIN: f64 = 18.0;

/// The drop zone in island coordinates (x from the notch's centre line, y down from the top edge): (half width,
/// height), the notch's zone or the island's frame with its margin, whichever is larger.
pub(crate) fn zone(island: Option<Shape>) -> (f64, f64) {
    match island {
        Some(s) => ((ZONE_W / 2.0).max(s.w / 2.0 + ISLAND_MARGIN), ZONE_H.max(s.h + ISLAND_MARGIN)),
        None => (ZONE_W / 2.0, ZONE_H),
    }
}

/// How often a press-drag's drag pasteboard is read while it has not changed yet (within the 30-50 ms asked).
const POLL_S: f64 = 0.04;

fn drag_count() -> isize {
    NSPasteboard::pasteboardWithName(unsafe { NSPasteboardNameDrag }).changeCount()
}

/// [night 1027, audit 21] What the drag watch does next. The catcher arms when the drag pasteboard's change count
/// changes (a drag session started somewhere). Before, it was read only on a LeftMouseDragged: a source that writes
/// the pasteboard after its last motion event (Chromium, Ego Lite, is suspected) left the catcher down under a still
/// pointer until the hand moved again, so a link dropped on the notch took only after a hold. Now a press-drag that
/// has not changed the count is polled every `POLL_S` until it does (arm) or the button comes up (stop): the poll runs
/// only while a button is held in a drag, never at rest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Arming {
    /// A new drag: arm the catcher (and stop any poll).
    Arm,
    /// Start polling.
    Poll,
    /// Nothing new; a poll already running keeps running.
    Wait,
    /// The button is up with no new drag: stop the poll.
    Stop,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct DragWatch {
    seen: isize,
    polling: bool,
}

impl DragWatch {
    pub(crate) fn new(count: isize) -> DragWatch {
        DragWatch { seen: count, polling: false }
    }

    /// A LeftMouseDragged anywhere, with the drag pasteboard's count now.
    pub(crate) fn dragged(&mut self, count: isize) -> Arming {
        if count != self.seen {
            self.seen = count;
            self.polling = false;
            return Arming::Arm;
        }
        if self.polling {
            return Arming::Wait;
        }
        self.polling = true;
        Arming::Poll
    }

    /// The poll ended from outside (armed, dropped): the next press-drag starts its own.
    pub(crate) fn stopped(&mut self) {
        self.polling = false;
    }

    /// A poll tick: the count now, and whether the left button is still down.
    pub(crate) fn tick(&mut self, count: isize, button_down: bool) -> Arming {
        if count != self.seen {
            self.seen = count;
            self.polling = false;
            return Arming::Arm;
        }
        if !button_down {
            self.polling = false;
            return Arming::Stop;
        }
        Arming::Wait
    }
}

/// Bug note 2026-10-28T17-29-59: at 17:29 the user let go of a screenshot and a link over the notch several times and the
/// app got nothing (no `notch_drop` in the journal, no file in the drops folder between 17:20 and 19:12), while the
/// same process took drops before and after; a drag that died in the drop target left no trace. What the target saw
/// of one armed drag, said at its end when it was let go over the zone and nothing was taken.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Seen {
    /// macOS offered it to the catcher over the zone (`draggingEntered:`/`draggingUpdated:`).
    pub offered: bool,
    /// macOS offered it, but the notch does not take it (Heimdall's own invite, or its source allows no copy).
    pub refused: bool,
    /// It was dropped on the catcher (`performDragOperation:`).
    pub performed: bool,
    /// Its items went to the app.
    pub taken: bool,
}

/// What the end of an armed drag does (`Seen::end`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum End {
    /// Taken, or let go away from the notch.
    Nothing,
    /// Let go over the notch, and macOS never offered it there: the catcher was covered (bug note
    /// 2026-11-03T18-33-35: twice while the agent worked, the notch never saw a screenshot let go over it). Its items
    /// are still on the drag pasteboard: they are taken from there, as if dropped.
    TakeLate,
    /// Let go over the notch and not taken, and nothing more to do: why.
    Missed(&'static str),
}

impl Seen {
    /// What a drag let go over the zone (`over`) or elsewhere does at its end. A drop is never refused for the agent's
    /// state: nothing here knows it, and the app queues or steers a drop like a typed message.
    pub(crate) fn end(self, over: bool) -> End {
        match self.missed(over) {
            None => End::Nothing,
            Some(_) if !self.offered && !self.refused && !self.performed => End::TakeLate,
            Some(why) => End::Missed(why),
        }
    }

    /// Why a drag let go over the zone (`over`) reached nothing; `None` when it was taken or let go elsewhere.
    pub(crate) fn missed(self, over: bool) -> Option<&'static str> {
        if self.taken || !over {
            return None;
        }
        Some(if self.refused && !self.offered {
            "the notch does not take it (Heimdall's own invite, or a drag that allows no copy)"
        } else if !self.offered {
            "macOS never offered it to the notch (a window or menu over it, or the catcher not up)"
        } else if !self.performed {
            "offered, but macOS never dropped it on the notch"
        } else {
            "dropped, but nothing usable was on its pasteboard"
        })
    }
}

/// What is known at the end of a drag that macOS never offered to the notch, for a late take (`late_take`). A late
/// take must never deliver a drop twice or take a drag that was not meant for the notch: when in doubt it is skipped.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct LateFacts {
    /// The drag started in one of Heimdall's own windows (its invite PNG, the Agent window's image).
    pub own: bool,
    /// A key went down while the drag was under way (Esc cancels a drag).
    pub key_pressed: bool,
    /// Another app's window is over the release point, in front of the catcher: it may have taken the drop.
    pub covered_by_other: bool,
    /// The drag pasteboard changed after the button came up: it is no longer this drag's.
    pub rewritten: bool,
}

/// Whether a drag let go over the notch and never offered to it is taken from the drag pasteboard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Late {
    /// Read its pasteboard; take its images and files (`late_items`).
    Take,
    /// Not taken, and why (said in the log).
    Skip(&'static str),
}

pub(crate) fn late_take(f: LateFacts) -> Late {
    if f.own {
        Late::Skip("it started in Heimdall itself")
    } else if f.key_pressed {
        Late::Skip("a key was pressed during the drag (Esc cancels one)")
    } else if f.covered_by_other {
        Late::Skip("another app's window over the notch may have taken it")
    } else if f.rewritten {
        Late::Skip("its pasteboard changed after it was let go")
    } else {
        Late::Take
    }
}

/// What a late take keeps: images and files already on disk (or saved from image data). Links and text are left (a
/// tab or a selection let go near the menu bar is not a drop), and promised files (Photos, Mail) cannot be received
/// once the drag is over.
pub(crate) fn late_items(mut items: Vec<DropItem>) -> Vec<DropItem> {
    items.retain(|i| i.path.is_some() && !matches!(i.kind, DropKind::Link | DropKind::Text));
    items
}

/// Where and when the button came up in an armed drag: the first sign wins (the mouse-up monitor, else the first
/// watch tick that sees it up), with the drag pasteboard's count then.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Release {
    /// Screen point (AppKit coordinates).
    pub at: Option<(f64, f64)>,
    pub count: Option<isize>,
}

impl Release {
    pub(crate) fn up(&mut self, at: (f64, f64), count: isize) {
        if self.at.is_none() {
            *self = Release { at: Some(at), count: Some(count) };
        }
    }

    /// The button went down again before the drag was judged over: a release seen before does not count.
    pub(crate) fn down(&mut self) {
        *self = Release::default();
    }

    /// The pasteboard changed since the release (`now`: its count now). Unknown release: not rewritten.
    pub(crate) fn rewritten(&self, now: isize) -> bool {
        self.count.is_some_and(|c| c != now)
    }
}

#[cfg(test)]
mod seen_tests {
    use super::Seen;

    #[test]
    fn a_drag_let_go_over_the_notch_and_not_taken_says_where_it_stopped() {
        let never = Seen::default();
        assert!(never.missed(true).unwrap().starts_with("macOS never offered"));
        assert_eq!(never.missed(false), None, "let go elsewhere: not a drop for the notch");
        let offered = Seen { offered: true, ..Seen::default() };
        assert!(offered.missed(true).unwrap().starts_with("offered, but"));
        let empty = Seen { offered: true, performed: true, ..Seen::default() };
        assert!(empty.missed(true).unwrap().contains("nothing usable"));
        assert_eq!(Seen { offered: true, performed: true, taken: true, ..Seen::default() }.missed(true), None);
    }

    /// Bug note 2026-11-03T18-33-35: at 18:32:57 and 18:33:16 on 3 Nov, while the agent worked and then showed a
    /// line, the user let a screenshot go over the notch; the catcher was armed (the line is said only for an armed drag)
    /// but macOS never offered the drag to it. Such a drop is taken from the drag pasteboard at the drag's end.
    #[test]
    fn a_drop_over_the_notch_that_macos_never_offered_is_still_taken() {
        use super::End;
        assert_eq!(Seen::default().end(true), End::TakeLate, "covered catcher: taken late, whatever the agent is doing");
        assert_eq!(Seen::default().end(false), End::Nothing, "let go elsewhere: never taken");
        // Offered and dropped on the notch: the catcher took it, nothing to add.
        assert_eq!(Seen { offered: true, performed: true, taken: true, ..Seen::default() }.end(true), End::Nothing);
        // Offered, never dropped (Esc over the notch, or macOS slid it back): left alone.
        assert!(matches!(Seen { offered: true, ..Seen::default() }.end(true), End::Missed(w) if w.starts_with("offered, but")));
        // Dropped with nothing usable: the pasteboard was already read; reading it again finds nothing more.
        assert!(matches!(Seen { offered: true, performed: true, ..Seen::default() }.end(true), End::Missed(_)));
        // Heimdall's own invite (or a move-only drag) is refused when offered: never taken late either.
        assert!(matches!(Seen { refused: true, ..Seen::default() }.end(true), End::Missed(w) if w.starts_with("the notch does not take it")));
    }

    /// The late take never delivers twice nor takes a drag that was not a drop: each doubt skips it.
    #[test]
    fn a_late_take_is_skipped_whenever_the_drag_may_not_have_been_a_drop_for_the_notch() {
        use super::{late_take, Late, LateFacts};
        assert_eq!(late_take(LateFacts::default()), Late::Take);
        for (facts, why) in [
            (LateFacts { own: true, ..LateFacts::default() }, "Heimdall itself"),
            (LateFacts { key_pressed: true, ..LateFacts::default() }, "Esc"),
            (LateFacts { covered_by_other: true, ..LateFacts::default() }, "another app's window"),
            (LateFacts { rewritten: true, ..LateFacts::default() }, "changed after"),
        ] {
            assert!(matches!(late_take(facts), Late::Skip(w) if w.contains(why)), "{facts:?}");
        }
    }

    #[test]
    fn a_late_take_keeps_images_and_files_only() {
        use super::late_items;
        use crate::island::drop::{DropItem, DropKind};
        let file = |kind, p: &str| DropItem { kind, path: Some(p.into()), url: None, text: None };
        let link = DropItem { kind: DropKind::Link, path: None, url: Some("https://x".into()), text: None };
        let text = DropItem { kind: DropKind::Text, path: None, url: None, text: Some("hi".into()) };
        let kept = late_items(vec![file(DropKind::Image, "/d/shot.png"), link.clone(), text.clone(), file(DropKind::Document, "/d/a.pdf")]);
        assert_eq!(kept.iter().map(|i| i.kind).collect::<Vec<_>>(), [DropKind::Image, DropKind::Document]);
        assert!(late_items(vec![link, text]).is_empty(), "a tab or a selection let go near the menu bar is no drop");
        assert!(late_items(vec![]).is_empty(), "promised files only: nothing to take once the drag is over");
    }

    /// The release is read when the button comes up (the mouse-up monitor), not where the pointer is a tick later; a
    /// pasteboard the source rewrote before that release is still this drag's, one written after it is not.
    #[test]
    fn the_release_is_the_first_button_up_and_judges_the_pasteboard_from_there() {
        use super::Release;
        let mut r = Release::default();
        assert!(!r.rewritten(7), "no release seen: nothing to compare");
        r.up((100.0, 980.0), 7);
        r.up((400.0, 500.0), 8);
        assert_eq!(r.at, Some((100.0, 980.0)), "the first sign wins: the watch tick later never moves it");
        assert!(!r.rewritten(7), "written late by its source (count 7 at the release): still this drag's");
        assert!(r.rewritten(8), "written after the release: a newer drag's");
        r.down();
        assert_eq!(r, Release::default(), "the button went down again: the drag goes on");
    }
}

#[cfg(test)]
mod drag_watch_tests {
    use super::{Arming, DragWatch};

    /// A fake drag pasteboard counter and the button, played through the watch the way the monitor and timer do.
    #[test]
    fn a_drag_that_writes_its_pasteboard_after_its_last_motion_arms_without_more_motion() {
        let mut w = DragWatch::new(5);
        // The press-drag starts; the source has not written the pasteboard yet.
        assert_eq!(w.dragged(5), Arming::Poll);
        assert_eq!(w.dragged(5), Arming::Wait, "one poll per drag");
        // The pointer stops over the notch; no more motion events. The source writes the pasteboard late.
        assert_eq!(w.tick(5, true), Arming::Wait);
        assert_eq!(w.tick(6, true), Arming::Arm, "the poll arms on the first change, with the pointer still");
        // Armed: the next drag needs a new count.
        assert_eq!(w.dragged(6), Arming::Poll);
        assert_eq!(w.tick(6, false), Arming::Stop);
    }

    #[test]
    fn a_plain_press_drag_polls_only_until_the_button_comes_up() {
        let mut w = DragWatch::new(9);
        // Selecting text, moving a window: no drag session, the count never changes.
        assert_eq!(w.dragged(9), Arming::Poll);
        assert_eq!(w.tick(9, true), Arming::Wait);
        assert_eq!(w.tick(9, false), Arming::Stop, "the button up ends the poll: nothing runs at rest");
        // A drag that writes before its first motion event arms at once, as before.
        assert_eq!(w.dragged(10), Arming::Arm);
    }
}

struct Layers {
    root: Retained<CALayer>,
    glow: Retained<CAShapeLayer>,
    shape: Retained<CAShapeLayer>,
    thumb: Retained<CALayer>,
    /// The thumbnail's size in points (aspect kept).
    thumb_size: (f64, f64),
    size: (f64, f64),
}

impl Layers {
    fn new(size: (f64, f64)) -> Layers {
        let (w, h) = size;
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        let root = CALayer::new();
        root.setGeometryFlipped(true);
        root.setFrame(cg_rect(0.0, 0.0, w, h));
        let black = CGColor::new_srgb(0.0, 0.0, 0.0, 1.0);
        let glow = CAShapeLayer::new();
        glow.setFrame(cg_rect(0.0, 0.0, w, h));
        glow.setFillColor(Some(&black));
        // A cool white light round the edge, soft and wide (the island's own outline light is a line; this is a halo).
        glow.setShadowColor(Some(&CGColor::new_srgb(0.80, 0.88, 1.0, 1.0)));
        glow.setShadowOffset(CGSize::new(0.0, 0.0));
        glow.setShadowRadius(14.0);
        glow.setShadowOpacity(0.0);
        let shape = CAShapeLayer::new();
        shape.setFrame(cg_rect(0.0, 0.0, w, h));
        shape.setFillColor(Some(&black));
        shape.setLineWidth(1.0);
        let thumb = CALayer::new();
        thumb.setShadowColor(Some(&black));
        thumb.setShadowOpacity(0.45);
        thumb.setShadowRadius(10.0);
        thumb.setShadowOffset(CGSize::new(0.0, 4.0));
        thumb.setHidden(true);
        root.addSublayer(&glow);
        root.addSublayer(&thumb);
        root.addSublayer(&shape);
        CATransaction::commit();
        Layers { root, glow, shape, thumb, thumb_size: (THUMB, THUMB), size }
    }

    fn set_thumb(&mut self, image: Option<(CFRetained<CGImage>, (f64, f64))>) {
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        match image {
            Some((img, size)) => {
                unsafe { self.thumb.setContents(Some(as_object(&*img))) };
                self.thumb_size = size;
            }
            None => unsafe { self.thumb.setContents(None) },
        }
        CATransaction::commit();
    }

    /// The one draw call: geometry and opacity only. `island`: the island shows and wears the swell itself, so only
    /// the carried item is drawn here.
    fn paint(&self, f: &DropFrame, island: bool) {
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        let cx = self.size.0 / 2.0;
        self.root.setHidden(!f.visible);
        self.glow.setHidden(island);
        self.shape.setHidden(island);
        let path = outline(&f.shape, cx);
        let p: &CGPath = &path;
        self.glow.setPath(Some(p));
        self.shape.setPath(Some(p));
        self.glow.setShadowOpacity((0.55 * f.glow) as f32);
        self.shape.setStrokeColor(Some(&CGColor::new_srgb(1.0, 1.0, 1.0, 0.16 * f.glow)));
        match f.thumb {
            Some((c, s, a)) => {
                let (w, h) = self.thumb_size;
                self.thumb.setHidden(false);
                self.thumb.setBounds(cg_rect(0.0, 0.0, w, h));
                self.thumb.setPosition(CGPoint::new(cx + c.x, c.y));
                self.thumb.setAffineTransform(CGAffineTransform { a: s, b: 0.0, c: 0.0, d: s, tx: 0.0, ty: 0.0 });
                self.thumb.setOpacity(a as f32);
            }
            None => self.thumb.setHidden(true),
        }
        CATransaction::commit();
    }
}

fn as_object<T>(r: &T) -> &AnyObject {
    unsafe { &*(r as *const T as *const AnyObject) }
}

fn cg_rect(x: f64, y: f64, w: f64, h: f64) -> CGRect {
    CGRect::new(CGPoint::new(x, y), CGSize::new(w.max(0.0), h.max(0.0)))
}

pub(crate) struct Target {
    /// The drawn window (in the overlay space, never takes the mouse).
    panel: Retained<NSPanel>,
    view: Retained<DropView>,
    /// The empty window that takes the drag (outside the overlay space, click-through except while a drag is under way).
    catcher: Retained<NSPanel>,
    catch_view: Retained<DropView>,
    layers: Layers,
    pub motion: Motion,
    pub place: Place,
    epoch: Instant,
    /// Virtual time for checks (`None`: the wall clock).
    pub clock: Option<f64>,
    /// The frame clock while something moves.
    timer: Option<Retained<NSTimer>>,
    /// While a drag is under way: whether the button is still down (nothing tells another app's drag ended).
    watch: Option<Retained<NSTimer>>,
    monitor: Option<Retained<AnyObject>>,
    local_monitor: Option<Retained<AnyObject>>,
    /// Where promised files (Photos, Mail) are received, kept while they arrive.
    promise_queue: Retained<objc2_foundation::NSOperationQueue>,
    /// The drag pasteboard's change count last seen (a new count is a new drag), and whether a press-drag waits for it.
    drag: DragWatch,
    /// While the button is down in a press-drag that has not written the drag pasteboard yet: its count, polled.
    poll: Option<Retained<NSTimer>>,
    pub armed: bool,
    /// What this armed drag reached (`Seen`, bug note 2026-10-28T17-29-59).
    seen: Seen,
    up_ticks: u8,
    /// Where the button came up in an armed drag (`Release`): the drag's end is judged there.
    release: Release,
    /// The last drag event came from one of Heimdall's own windows (the local monitor), and whether the armed drag did.
    last_own: bool,
    own: bool,
    /// When the drag armed (a key pressed since then may have cancelled it).
    armed_at: Instant,
    pub offscreen: bool,
    /// The island's shape while it shows (`Host::render` and `set_drop` say): the swell is its own then.
    pub island: Option<Shape>,
    /// The island's confirmation, due at.
    confirm: Option<(f64, CommandCue)>,
    /// Checks: the last items taken.
    pub last: Option<Dropped>,
}

thread_local! {
    pub(crate) static TARGET: RefCell<Option<Target>> = const { RefCell::new(None) };
}

pub(crate) fn with_target<R>(f: impl FnOnce(&mut Target) -> R) -> Option<R> {
    TARGET.with(|t| t.try_borrow_mut().ok()?.as_mut().map(f))
}

fn reduce_motion() -> bool {
    NSWorkspace::sharedWorkspace().accessibilityDisplayShouldReduceMotion()
}

/// Creates the drop target at launch (ordered out, so the capture filters exclude it from the start) and starts
/// watching for drags. Renders and checks pass `watch: false`.
pub(crate) fn install(mtm: MainThreadMarker, watch: bool) {
    if TARGET.with(|t| t.borrow().is_some()) {
        return;
    }
    let place = if watch { super::util::pointer_screen(mtm).map(|s| Place::of(&s)).unwrap_or_else(Place::offscreen) } else { Place::offscreen() };
    let mut target = Target::new(mtm, place, !watch);
    if watch {
        target.watch_drags();
    }
    TARGET.with(|t| *t.borrow_mut() = Some(target));
}

impl Target {
    fn new(mtm: MainThreadMarker, place: Place, offscreen: bool) -> Target {
        // The drawn window: shared like the island's windows when Show Islands on Streams is on, never takes the mouse.
        let panel = make_panel(mtm, super::streams::sharing());
        let size = (PANEL_W, place.window_h());
        let view: Retained<DropView> = unsafe { msg_send![DropView::alloc(mtm), initWithFrame: rect(0.0, 0.0, size.0, size.1)] };
        let layers = Layers::new(size);
        view.setLayer(Some(&layers.root));
        view.setWantsLayer(true);
        panel.setContentView(Some(&view));
        // The catcher: nothing drawn, so never shared; click-through until a drag arms it.
        let catcher = make_panel(mtm, NSWindowSharingType::None);
        let catch_view: Retained<DropView> = unsafe { msg_send![DropView::alloc(mtm), initWithFrame: rect(0.0, 0.0, ZONE_W, ZONE_H)] };
        catch_view.setWantsLayer(true);
        catch_view.registerForDraggedTypes(&pasteboard::types());
        catcher.setContentView(Some(&catch_view));
        if offscreen {
            panel.setAlphaValue(0.0);
            catcher.setAlphaValue(0.0);
        }
        let drag = DragWatch::new(drag_count());
        let mut t = Target {
            panel,
            view,
            catcher,
            catch_view,
            layers,
            motion: Motion::new(place.display, false),
            place,
            epoch: Instant::now(),
            clock: offscreen.then_some(0.0),
            timer: None,
            watch: None,
            monitor: None,
            local_monitor: None,
            promise_queue: objc2_foundation::NSOperationQueue::new(),
            drag,
            poll: None,
            armed: false,
            seen: Seen::default(),
            up_ticks: 0,
            release: Release::default(),
            last_own: false,
            own: false,
            armed_at: Instant::now(),
            offscreen,
            island: None,
            confirm: None,
            last: None,
        };
        t.place_window();
        t
    }

    pub(crate) fn now(&self) -> f64 {
        self.clock.unwrap_or_else(|| self.epoch.elapsed().as_secs_f64())
    }

    pub(crate) fn panel(&self) -> Retained<NSPanel> {
        self.panel.clone()
    }

    pub(crate) fn catcher(&self) -> (Retained<NSPanel>, Retained<DropView>) {
        (self.catcher.clone(), self.catch_view.clone())
    }

    /// The drawn window over the island's window; the catcher over the drop zone (`place_catcher`).
    fn place_window(&mut self) {
        let h = self.place.window_h();
        if self.layers.size.1 != h {
            // Another display's height: layers for the new size (nothing is drawn while the target moves).
            self.layers = Layers::new((PANEL_W, h));
            self.view.setLayer(Some(&self.layers.root));
        }
        self.panel.setFrame_display(rect(self.place.center_x - PANEL_W / 2.0, self.place.top - h, PANEL_W, h), false);
        self.view.setFrame(rect(0.0, 0.0, PANEL_W, h));
        self.place_catcher();
    }

    /// The catcher's hit area: the drop zone, the island's current frame while it shows (it follows the island as it
    /// resizes mid-drag), never more, so drags to the apps behind pass everywhere else.
    fn place_catcher(&self) {
        let (hw, h) = zone(self.island);
        let frame = rect(self.place.center_x - hw, self.place.top - h, 2.0 * hw, h);
        if self.catcher.frame() != frame {
            self.catcher.setFrame_display(frame, false);
            self.catch_view.setFrame(rect(0.0, 0.0, 2.0 * hw, h));
        }
    }

    /// The island showed, moved or went (from its own frame, `Host::render`, on display `display`): the catcher
    /// follows, and a hand-over between the island's swell and this window's is repainted on the same turn. An island
    /// on another display is no island here.
    pub(crate) fn island_moved(&mut self, display: u32, island: Option<Shape>) {
        let island = island.filter(|_| display == self.place.id);
        if island == self.island {
            return;
        }
        let handover = island.is_some() != self.island.is_some();
        self.island = island;
        if self.armed {
            self.place_catcher();
        }
        if handover {
            // Only the paint: the clock and the confirmation stay with `after` (the island is mid-render here).
            self.layers.paint(&self.motion.frame(self.now()), island.is_some());
        }
    }

    /// A global monitor on drags: the drag pasteboard's count changes when a drag session starts in any app.
    fn watch_drags(&mut self) {
        // The button coming up is watched too: an armed drag's release point and pasteboard count are read then.
        let mask = NSEventMask::LeftMouseDragged | NSEventMask::LeftMouseUp;
        let block = block2::RcBlock::new(|e: std::ptr::NonNull<NSEvent>| {
            let up = unsafe { e.as_ref() }.r#type() == objc2_app_kit::NSEventType::LeftMouseUp;
            with_target(|t| if up { t.mouse_up() } else { t.mouse_dragged(false) });
        });
        self.monitor = NSEvent::addGlobalMonitorForEventsMatchingMask_handler(mask, &block);
        // A drag that starts in one of Heimdall's own windows (the Agent window's image) is not another app's event.
        let local = block2::RcBlock::new(|e: std::ptr::NonNull<NSEvent>| -> *mut NSEvent {
            let up = unsafe { e.as_ref() }.r#type() == objc2_app_kit::NSEventType::LeftMouseUp;
            with_target(|t| if up { t.mouse_up() } else { t.mouse_dragged(true) });
            e.as_ptr()
        });
        self.local_monitor = unsafe { NSEvent::addLocalMonitorForEventsMatchingMask_handler(mask, &local) };
    }

    /// The button came up: an armed drag's release.
    fn mouse_up(&mut self) {
        if self.armed {
            let p = NSEvent::mouseLocation();
            self.release.up((p.x, p.y), drag_count());
        }
    }

    fn mouse_dragged(&mut self, own: bool) {
        self.last_own = own;
        if self.armed {
            // Carried to another display mid-drag: the target moves there (while nothing is drawn).
            self.follow_pointer();
            return;
        }
        match self.drag.dragged(drag_count()) {
            Arming::Arm => self.arm(),
            Arming::Poll => self.start_poll(),
            Arming::Wait | Arming::Stop => {}
        }
    }

    /// A press-drag whose drag pasteboard has not changed yet: watch it every 40 ms while the button is down.
    fn start_poll(&mut self) {
        if self.poll.is_some() {
            return;
        }
        if self.clock.is_some() {
            // Checks run on virtual time: no timer, and the next press-drag asks again.
            self.drag.stopped();
            return;
        }
        let timer = unsafe { NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(POLL_S, &self.view, sel!(dropPoll:), None, true) };
        unsafe { NSRunLoop::currentRunLoop().addTimer_forMode(&timer, NSRunLoopCommonModes) };
        self.poll = Some(timer);
    }

    fn stop_poll(&mut self) {
        self.drag.stopped();
        if let Some(t) = self.poll.take() {
            t.invalidate();
        }
    }

    fn poll_tick(&mut self) {
        match self.drag.tick(drag_count(), NSEvent::pressedMouseButtons() & 1 != 0) {
            Arming::Arm => self.arm(),
            Arming::Stop => self.stop_poll(),
            Arming::Poll | Arming::Wait => {}
        }
    }

    fn follow_pointer(&mut self) {
        if self.offscreen || self.clock.is_some() || !self.motion.at_rest(self.now()) {
            return;
        }
        let Some(mtm) = MainThreadMarker::new() else { return };
        let Some(screen) = super::util::pointer_screen(mtm) else { return };
        let place = Place::of(&screen);
        if place != self.place {
            self.place = place;
            self.motion.set_display(place.display);
            self.ask_island();
            self.place_window();
        }
    }

    /// The island's frame as it is now on this display (it may show on another one, or not at all).
    fn ask_island(&mut self) {
        let id = self.place.id;
        if let Some(island) = super::island::host::with_host(|h| h.island_on(id)) {
            self.island = island;
        }
    }

    /// A drag started: up on the display with the pointer, still drawing nothing.
    pub(crate) fn arm(&mut self) {
        self.stop_poll();
        if !self.offscreen && self.clock.is_none() {
            self.follow_pointer();
            self.motion.set_reduce_motion(reduce_motion());
        }
        if !self.armed {
            self.seen = Seen::default();
        }
        if !self.armed {
            self.own = self.last_own;
            self.armed_at = Instant::now();
        }
        self.armed = true;
        self.up_ticks = 0;
        self.release = Release::default();
        self.ask_island();
        self.place_catcher();
        // The island's click target (over the pull-down, or the notch at rest) would take the drag first. A host busy
        // this instant lets it through on the next turn of the main queue, while the drag is still under way.
        if super::island::host::with_host(|h| h.pass_drags(true)).is_none() {
            DispatchQueue::main().exec_async(|| {
                if with_target(|t| t.armed) == Some(true) {
                    super::island::host::with_host(|h| h.pass_drags(true));
                }
            });
        }
        // The catcher takes the drag wherever it is over the window, drawn or not; only while a drag is under way. It
        // never joins the overlay space (a window there is never offered a drag).
        self.catcher.setIgnoresMouseEvents(false);
        if !self.catcher.isVisible() {
            self.catcher.orderFrontRegardless();
        }
        if !self.panel.isVisible() {
            self.panel.orderFrontRegardless();
            if !self.offscreen && self.clock.is_none() {
                super::overlay_space::join(&self.panel, WINDOW_TITLE);
            }
        }
        if self.watch.is_none() && self.clock.is_none() {
            let timer = unsafe { NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(0.2, &self.view, sel!(dropWatch:), None, true) };
            unsafe { NSRunLoop::currentRunLoop().addTimer_forMode(&timer, NSRunLoopCommonModes) };
            self.watch = Some(timer);
        }
    }

    /// The drag is over (the button is up): the window goes once its motion has ended.
    pub(crate) fn disarm(&mut self) {
        let release = std::mem::take(&mut self.release);
        if self.armed && !self.offscreen && self.clock.is_none() {
            // Judged while the catcher is still up (what is in front of it is read).
            self.end_drag(release);
        }
        self.armed = false;
        // Clicks back for the click target; a host busy this instant gets it on the next turn of the main queue.
        if super::island::host::with_host(|h| h.pass_drags(false)).is_none() {
            DispatchQueue::main().exec_async(|| {
                super::island::host::with_host(|h| h.pass_drags(false));
            });
        }
        // No clicks from here on, through the swallow too: the catcher goes at once (it draws nothing).
        self.catcher.setIgnoresMouseEvents(true);
        self.catcher.orderOut(None);
        if let Some(t) = self.watch.take() {
            t.invalidate();
        }
        // A drag still counted as over the target never stays swollen (a drop goes on regardless: `left` keeps it).
        let now = self.now();
        self.motion.left(now);
        self.after(now);
    }

    /// An armed drag ended (`release`: where the button came up): a drop over the notch that macOS never offered to
    /// the catcher is taken from the drag pasteboard when nothing says it was meant elsewhere (`End::TakeLate`,
    /// `late_take`, bug note 2026-11-03T18-33-35); any other miss is said in the log.
    fn end_drag(&mut self, release: Release) {
        let screen = release.at.map(|(x, y)| NSPoint::new(x, y)).unwrap_or_else(NSEvent::mouseLocation);
        let at = self.place.from_screen(screen);
        let why = match self.seen.end(self.in_zone(at)) {
            End::Nothing => return,
            End::Missed(why) => why,
            End::TakeLate => {
                let cover = covering_app(&self.catcher, screen);
                let facts = LateFacts {
                    own: self.own,
                    key_pressed: key_down_since(self.armed_at),
                    covered_by_other: cover.is_some(),
                    rewritten: release.rewritten(drag_count()),
                };
                match late_take(facts) {
                    Late::Take => match self.take_late(at) {
                        Ok(n) => {
                            eprintln!("drop: macOS never offered a drag let go over the notch to it; {n} item(s) were taken from its pasteboard");
                            return;
                        }
                        Err(why) => why,
                    },
                    Late::Skip(why) if cover.is_some() => {
                        eprintln!("drop: a drag let go over the notch was not taken: macOS never offered it to the notch; {why} ({})", cover.unwrap_or_default());
                        return;
                    }
                    Late::Skip(why) => why,
                }
            }
        };
        eprintln!("drop: a drag let go over the notch was not taken: {why}");
    }

    /// The late take: this drag's images and files from the drag pasteboard, routed like a drop (the composer, or the
    /// agent). Its count of items, or why nothing was taken.
    fn take_late(&mut self, p: Point) -> Result<usize, &'static str> {
        let pb = NSPasteboard::pasteboardWithName(unsafe { NSPasteboardNameDrag });
        let snap = pasteboard::snapshot(&pb);
        if snap.files.is_empty() && snap.image.is_none() {
            return Err(if pasteboard::promises(&pb).is_empty() {
                "macOS never offered it to the notch, and it held no picture or file (a link or text is not taken late)"
            } else {
                "macOS never offered it to the notch, and its files were only promised (they cannot arrive once the drag is over)"
            });
        }
        let at_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
        let stamp = drop::stamp(at_ms);
        let dir = drop::drops_dir();
        let only = drop::Snapshot { urls: vec![], text: None, ..snap };
        let items = late_items(drop::classify(only, &|p: &Path| p.is_dir(), &mut |name, bytes| save(&dir, name, bytes), &stamp));
        if items.is_empty() {
            return Err("macOS never offered it to the notch, and nothing on its pasteboard could be read");
        }
        let n = items.len();
        self.seen.taken = true;
        let now = self.now();
        if !self.attach(&items, now) {
            self.taken(Dropped { items, at_ms }, p, now);
        }
        Ok(n)
    }

    /// The button up on two checks in a row (0.2–0.4 s): the drag is over, and the drop, if there was one, has been
    /// delivered (it comes on the release, before the second check).
    fn watch_tick(&mut self) {
        if NSEvent::pressedMouseButtons() & 1 != 0 {
            self.up_ticks = 0;
            self.release.down();
            return;
        }
        // No mouse-up seen (a drag session may keep it from the monitors): the first tick that sees the button up.
        let p = NSEvent::mouseLocation();
        self.release.up((p.x, p.y), drag_count());
        self.up_ticks += 1;
        if self.up_ticks >= 2 {
            self.disarm();
        }
    }

    /// Island coordinates of a point in the catcher (the window the drag reports in).
    fn island_point(&self, window_point: NSPoint) -> Point {
        let f = self.catcher.frame();
        self.place.from_screen(NSPoint::new(f.origin.x + window_point.x, f.origin.y + window_point.y))
    }

    pub(crate) fn in_zone(&self, p: Point) -> bool {
        let (hw, h) = zone(self.island);
        p.x.abs() <= hw && p.y >= -2.0 && p.y <= h
    }

    /// The drag moved over the window (`p` island coordinates): over the zone the notch swells.
    pub(crate) fn hover(&mut self, p: Option<Point>) -> bool {
        let now = self.now();
        let inside = p.is_some_and(|p| self.in_zone(p));
        if inside {
            self.motion.over(now);
        } else {
            self.motion.left(now);
        }
        self.after(now);
        inside
    }

    /// Let go at `p` over the zone: the items go to the app, the swallow plays, the island confirms.
    pub(crate) fn take(&mut self, pb: &NSPasteboard, p: Point) -> bool {
        self.seen.performed = true;
        let now = self.now();
        let at_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
        let stamp = drop::stamp(at_ms);
        let dir = drop::drops_dir();
        let snap = pasteboard::snapshot(pb);
        let mut items = drop::classify(snap, &|p: &Path| p.is_dir(), &mut |name, bytes| save(&dir, name, bytes), &stamp);
        if items.is_empty() {
            let promises = pasteboard::promises(pb);
            if !promises.is_empty() {
                // Where they go is decided now, at the drop, not when the files arrive.
                let to_composer = super::island::host::with_host(|h| h.drop_route()) == Some(drop::Route::Composer);
                self.receive_promises(promises, dir.join(&stamp), at_ms, p, to_composer);
                self.seen.taken = true;
                return true;
            }
        }
        items.retain(|i| i.path.is_some() || i.url.is_some() || i.text.is_some());
        if items.is_empty() {
            self.motion.left(now);
            self.after(now);
            return false;
        }
        self.seen.taken = true;
        if self.attach(&items, now) {
            return true;
        }
        self.taken(Dropped { items, at_ms }, p, now);
        true
    }

    /// A drop on the open pull-down while the user writes in its composer (`drop::route`): its items go into the composer
    /// and ride with his message; the swell settles and the composer shows them, with no flight into the notch and
    /// no cue. False when the drop goes to the agent as a notch drop.
    fn attach(&mut self, items: &[DropItem], now: f64) -> bool {
        let to_composer = super::island::host::with_host(|h| h.drop_route()) == Some(drop::Route::Composer);
        if !to_composer {
            return false;
        }
        self.last = Some(Dropped { items: items.to_vec(), at_ms: 0 });
        self.motion.left(now);
        self.after(now);
        let items = items.to_vec();
        // On the next turn of the main queue, never re-entrantly from inside the drag's callback.
        DispatchQueue::main().exec_async(move || super::island::apply(Event::ComposerAttach(items)));
        true
    }

    fn taken(&mut self, dropped: Dropped, p: Point, now: f64) {
        let thumb = thumbnail(&dropped.items, self.place.display.scale);
        self.layers.set_thumb(thumb);
        self.motion.dropped(now, p);
        let (kind, label) = drop::summary(&dropped.items);
        self.confirm = Some((self.motion.confirm_at(now), CommandCue::Done { id: None, kind: CommandKind::Drop(kind), label }));
        self.last = Some(dropped.clone());
        // To the app on the next turn of the main queue, never re-entrantly from inside the drag's callback.
        DispatchQueue::main().exec_async(move || super::island::host::emit(UiAction::Dropped(dropped)));
        self.after(now);
    }

    /// Photos and Mail hand promises: the files are written into the drops folder, then taken.
    fn receive_promises(&mut self, promises: Vec<Retained<objc2_app_kit::NSFilePromiseReceiver>>, dir: PathBuf, at_ms: u64, p: Point, to_composer: bool) {
        if std::fs::create_dir_all(&dir).is_err() {
            return;
        }
        let now = self.now();
        if to_composer {
            // Into the composer: the swell settles, the thumbnails come with the files.
            self.motion.left(now);
        } else {
            // The swallow plays at once with a stand-in; the message waits for the files.
            self.layers.set_thumb(thumbnail(&[DropItem { kind: DropKind::File, path: None, url: None, text: None }], self.place.display.scale));
            self.motion.dropped(now, p);
        }
        self.after(now);
        // One callback per file: a receiver may promise several.
        let total: usize = promises.iter().map(|r| r.fileNames().count().max(1)).sum();
        let got: std::sync::Arc<std::sync::Mutex<Vec<Option<PathBuf>>>> = Default::default();
        let queue = self.promise_queue.clone();
        let url = objc2_foundation::NSURL::fileURLWithPath(&NSString::from_str(&dir.to_string_lossy()));
        for r in promises {
            let got = got.clone();
            let block = block2::RcBlock::new(move |file: std::ptr::NonNull<objc2_foundation::NSURL>, error: *mut objc2_foundation::NSError| {
                let path = if error.is_null() { unsafe { file.as_ref() }.path().map(|s| PathBuf::from(s.to_string())) } else { None };
                let mut g = got.lock().unwrap();
                g.push(path);
                if g.len() == total {
                    let files: Vec<PathBuf> = g.iter().flatten().cloned().collect();
                    DispatchQueue::main().exec_async(move || {
                        let items: Vec<DropItem> = files.into_iter().map(|f| DropItem { kind: drop::kind_of(&f, f.is_dir()), path: Some(f), url: None, text: None }).collect();
                        if items.is_empty() {
                            eprintln!("drop: no promised file arrived");
                            return;
                        }
                        if to_composer {
                            super::island::apply(Event::ComposerAttach(items));
                            return;
                        }
                        let (kind, label) = drop::summary(&items);
                        let dropped = Dropped { items, at_ms };
                        with_target(|t| {
                            t.last = Some(dropped.clone());
                            let now = t.now();
                            t.confirm = Some((now, CommandCue::Done { id: None, kind: CommandKind::Drop(kind), label }));
                            t.after(now);
                        });
                        super::island::host::emit(UiAction::Dropped(dropped));
                    });
                }
            });
            unsafe { r.receivePromisedFilesAtDestination_options_operationQueue_reader(&url, &objc2_foundation::NSDictionary::new(), &queue, &block) };
        }
    }

    /// After a change or a tick: the frame, the island's confirmation when due, the clock, the window.
    pub(crate) fn after(&mut self, now: f64) {
        let f = self.motion.frame(now);
        // While the island shows, the swell is its own (one shape); the island says whether it shows.
        let swell = self.motion.swell(now);
        // `None`: no island, or it is mid-render (its own `island_moved` call keeps `self.island` then).
        if let Some(island) = super::island::host::with_host(|h| h.set_drop(swell, self.place.id)).filter(|i| *i != self.island) {
            self.island = island;
            self.place_catcher();
        }
        self.layers.paint(&f, self.island.is_some());
        if let Some((at, _)) = &self.confirm {
            if now >= *at - 1e-6 {
                let (_, cue) = self.confirm.take().unwrap();
                super::island::apply(Event::Command(cue));
            }
        }
        let moving = self.motion.moving(now) || self.confirm.is_some();
        if moving && self.timer.is_none() && self.clock.is_none() {
            let timer = unsafe { NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(1.0 / 120.0, &self.view, sel!(dropFrame:), None, true) };
            unsafe { NSRunLoop::currentRunLoop().addTimer_forMode(&timer, NSRunLoopCommonModes) };
            self.timer = Some(timer);
        } else if !moving {
            if let Some(t) = self.timer.take() {
                t.invalidate();
            }
        }
        if !self.armed && self.motion.at_rest(now) && !moving && self.panel.isVisible() {
            self.panel.orderOut(None);
        }
    }

    /// Whether nothing runs and nothing is up (checks).
    pub(crate) fn idle(&self) -> bool {
        self.timer.is_none() && self.watch.is_none() && self.poll.is_none() && !self.panel.isVisible() && !self.catcher.isVisible()
    }

    /// Renders the window's layers into `canvas` at `offset` points (offscreen: the contact sheet).
    pub(crate) fn render_into(&self, canvas: &Canvas, offset: (f64, f64)) {
        let ctx = &canvas.ctx;
        objc2_core_graphics::CGContext::save_g_state(Some(ctx));
        objc2_core_graphics::CGContext::translate_ctm(Some(ctx), offset.0, offset.1);
        self.layers.root.renderInContext(ctx);
        objc2_core_graphics::CGContext::restore_g_state(Some(ctx));
    }
}

impl Drop for Target {
    fn drop(&mut self) {
        for m in [self.monitor.take(), self.local_monitor.take()].into_iter().flatten() {
            unsafe { NSEvent::removeMonitor(&m) };
        }
        self.stop_poll();
    }
}

/// One of the two windows: borderless, non-activating, clear, click-through, above the island and its click target, on
/// every Space, titled for the capture filters.
fn make_panel(mtm: MainThreadMarker, sharing: NSWindowSharingType) -> Retained<NSPanel> {
    let panel = NSPanel::initWithContentRect_styleMask_backing_defer(
        NSPanel::alloc(mtm),
        rect(0.0, 0.0, 10.0, 10.0),
        NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel,
        NSBackingStoreType::Buffered,
        false,
    );
    panel.setTitle(&ns(WINDOW_TITLE));
    panel.setSharingType(sharing);
    panel.setIgnoresMouseEvents(true);
    panel.setOpaque(false);
    panel.setHasShadow(false);
    panel.setBackgroundColor(Some(&NSColor::clearColor()));
    panel.setFloatingPanel(true);
    panel.setBecomesKeyOnlyIfNeeded(true);
    panel.setHidesOnDeactivate(false);
    panel.setAnimationBehavior(NSWindowAnimationBehavior::None);
    unsafe { panel.setReleasedWhenClosed(false) };
    panel.setLevel(NSMainMenuWindowLevel + 5);
    panel.setCollectionBehavior(overlay_behavior());
    panel
}

/// The island's frame changed (`Host::render`). Silently nothing when the drop target is mid-call (it asks the island
/// itself then, `Target::after`).
pub(crate) fn island_moved(display: u32, island: Option<Shape>) {
    with_target(|t| t.island_moved(display, island));
}

/// The drawn window, for Show Islands on Streams.
pub(crate) fn panel() -> Option<Retained<NSPanel>> {
    with_target(|t| t.panel())
}

/// Keeps `bytes` as `name` in the drops folder.
fn save(dir: &Path, name: &str, bytes: &[u8]) -> Option<PathBuf> {
    std::fs::create_dir_all(dir).ok()?;
    let safe: String = name.chars().map(|c| if c == '/' || c == ':' { '-' } else { c }).collect();
    let path = dir.join(safe);
    std::fs::write(&path, bytes).ok()?;
    Some(path)
}

/// The carried item's picture: the image itself (rounded), a file's Finder icon, or a symbol on a dark tile; a few
/// cards fanned behind it when there are several. Its size in points, aspect kept, within `THUMB`.
pub(crate) fn thumbnail(items: &[DropItem], scale: f64) -> Option<(CFRetained<CGImage>, (f64, f64))> {
    let first = items.first()?;
    let file = first.path.as_ref().map(|p| NSString::from_str(&p.to_string_lossy()));
    // The picture itself only from a local file of sensible size (an iCloud-only file would download on the main
    // thread); otherwise its Finder icon.
    let readable = first.path.as_ref().is_some_and(|p| {
        use std::os::macos::fs::MetadataExt;
        std::fs::metadata(p).is_ok_and(|m| m.len() <= 64 * 1024 * 1024 && m.st_flags() & 0x4000_0000 == 0)
    });
    let picture: Option<Retained<NSImage>> = match (first.kind, &file) {
        (DropKind::Image, Some(f)) if readable => NSImage::initWithContentsOfFile(NSImage::alloc(), f),
        _ => None,
    };
    let icon = match (&picture, &file) {
        (None, Some(f)) if !matches!(first.kind, DropKind::Link | DropKind::Text) => Some(NSWorkspace::sharedWorkspace().iconForFile(f)),
        _ => None,
    };
    let (w, h) = match &picture {
        Some(img) if img.size().width > 0.0 && img.size().height > 0.0 => {
            let s = img.size();
            let k = THUMB / s.width.max(s.height);
            ((s.width * k).max(24.0), (s.height * k).max(24.0))
        }
        _ => (THUMB, THUMB),
    };
    let fan = (items.len().min(3) as f64 - 1.0) * 5.0;
    let canvas = Canvas::new(w + fan, h + fan, scale)?;
    canvas.draw(|| {
        let r = |x: f64, y: f64, w: f64, h: f64| rect(x, y, w, h);
        // Cards behind, lighter as they go back.
        for k in (1..items.len().min(3)).rev() {
            let o = k as f64 * 5.0;
            NSColor::colorWithSRGBRed_green_blue_alpha(0.32, 0.32, 0.34, 1.0 - 0.25 * k as f64).setFill();
            objc2_app_kit::NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(r(o, o, w, h), 10.0, 10.0).fill();
        }
        let tile = r(0.0, 0.0, w, h);
        match (&picture, &icon) {
            (Some(img), _) => {
                NSColor::blackColor().setFill();
                let clip = objc2_app_kit::NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(tile, 10.0, 10.0);
                objc2_app_kit::NSGraphicsContext::saveGraphicsState_class();
                clip.addClip();
                unsafe {
                    img.drawInRect_fromRect_operation_fraction_respectFlipped_hints(tile, objc2_foundation::NSRect::ZERO, objc2_app_kit::NSCompositingOperation::SourceOver, 1.0, true, None)
                };
                objc2_app_kit::NSGraphicsContext::restoreGraphicsState_class();
                NSColor::colorWithSRGBRed_green_blue_alpha(1.0, 1.0, 1.0, 0.22).setStroke();
                let rim = objc2_app_kit::NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(r(0.25, 0.25, w - 0.5, h - 0.5), 10.0, 10.0);
                rim.setLineWidth(0.5);
                rim.stroke();
            }
            (None, Some(icon)) => unsafe {
                icon.drawInRect_fromRect_operation_fraction_respectFlipped_hints(tile, objc2_foundation::NSRect::ZERO, objc2_app_kit::NSCompositingOperation::SourceOver, 1.0, true, None)
            },
            (None, None) => {
                NSColor::colorWithSRGBRed_green_blue_alpha(0.17, 0.17, 0.18, 1.0).setFill();
                objc2_app_kit::NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(tile, 14.0, 14.0).fill();
                if let Some(sym) = super::util::symbol_sized(first.kind.symbol(), 26.0, unsafe { objc2_app_kit::NSFontWeightSemibold }) {
                    let config = objc2_app_kit::NSImageSymbolConfiguration::configurationWithPaletteColors(&objc2_foundation::NSArray::from_retained_slice(&[NSColor::whiteColor()]));
                    let sym = sym.imageWithSymbolConfiguration(&config).unwrap_or(sym);
                    let s = sym.size();
                    let at = r(((w - s.width) / 2.0).round(), ((h - s.height) / 2.0).round(), s.width, s.height);
                    unsafe { sym.drawInRect_fromRect_operation_fraction_respectFlipped_hints(at, objc2_foundation::NSRect::ZERO, objc2_app_kit::NSCompositingOperation::SourceOver, 1.0, true, None) };
                }
            }
        }
    });
    Some((canvas.image()?, (w + fan, h + fan)))
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventSourceSecondsSinceLastEventType(state: i32, event_type: u32) -> f64;
    fn CGWindowListCopyWindowInfo(option: u32, relative_to: u32) -> *mut AnyObject;
}

/// A key went down anywhere since `since` (the HID system's own clock of input; no permission needed).
fn key_down_since(since: Instant) -> bool {
    // kCGEventSourceStateHIDSystemState, kCGEventKeyDown.
    let ago = unsafe { CGEventSourceSecondsSinceLastEventType(1, 10) };
    ago < since.elapsed().as_secs_f64()
}

/// Another app's window in front of `catcher` under the screen point `p` (AppKit coordinates): its owner's name. The
/// drag's own image and the cursor (levels from the dragging level up) and clear windows are not counted.
fn covering_app(catcher: &NSPanel, p: NSPoint) -> Option<String> {
    use objc2_foundation::{NSArray, NSDictionary, NSNumber};
    let mtm = MainThreadMarker::new()?;
    let primary_h = objc2_app_kit::NSScreen::screens(mtm).firstObject()?.frame().size.height;
    let (x, y) = (p.x, primary_h - p.y);
    // kCGWindowListOptionOnScreenAboveWindow | kCGWindowListExcludeDesktopElements.
    let raw = unsafe { CGWindowListCopyWindowInfo(2 | 16, catcher.windowNumber() as u32) };
    let list: Retained<NSArray<NSDictionary<NSString, AnyObject>>> = unsafe { Retained::from_raw(raw.cast())? };
    let me = std::process::id() as i64;
    let num = |d: &NSDictionary<NSString, AnyObject>, k: &str| -> Option<f64> {
        let v = d.objectForKey(&NSString::from_str(k))?;
        Some(v.downcast_ref::<NSNumber>()?.doubleValue())
    };
    for d in list.iter() {
        let pid = num(&d, "kCGWindowOwnerPID").unwrap_or(0.0) as i64;
        let layer = num(&d, "kCGWindowLayer").unwrap_or(0.0);
        let alpha = num(&d, "kCGWindowAlpha").unwrap_or(1.0);
        if pid == me || layer >= 500.0 || alpha <= 0.01 {
            continue;
        }
        let Some(b) = d.objectForKey(&NSString::from_str("kCGWindowBounds")) else { continue };
        let Some(b) = b.downcast_ref::<NSDictionary>() else { continue };
        let b: &NSDictionary<NSString, AnyObject> = unsafe { &*(b as *const NSDictionary as *const NSDictionary<NSString, AnyObject>) };
        let (bx, by, bw, bh) = (num(b, "X").unwrap_or(0.0), num(b, "Y").unwrap_or(0.0), num(b, "Width").unwrap_or(0.0), num(b, "Height").unwrap_or(0.0));
        if x >= bx && x <= bx + bw && y >= by && y <= by + bh {
            let owner = d.objectForKey(&NSString::from_str("kCGWindowOwnerName")).and_then(|o| o.downcast_ref::<NSString>().map(|s| s.to_string()));
            return Some(format!("{} at level {layer:.0}", owner.unwrap_or_else(|| format!("pid {pid}"))));
        }
    }
    None
}

/// Whether the notch takes this drag: its source allows a copy, and it is not Heimdall's own invite drag. The invite's PNG
/// is dragged out to another app (crates/ui/src/mac/friend_invite.rs), so it is not a drop for the notch.
fn takes(sender: &ProtocolObject<dyn NSDraggingInfo>) -> bool {
    if !sender.draggingSourceOperationMask().contains(NSDragOperation::Copy) {
        return false;
    }
    let own_invite = sender
        .draggingSource()
        .is_some_and(|source| source.class().name().to_str() == Ok("HeimdallInviteQR"));
    !own_invite
}

/// A drag entered or moved: Copy over the zone (the notch swells), nothing elsewhere.
fn dragged(sender: &ProtocolObject<dyn NSDraggingInfo>) -> NSDragOperation {
    if !takes(sender) {
        with_target(|t| t.seen.refused = true);
        return NSDragOperation::None;
    }
    let at = sender.draggingLocation();
    let inside = with_target(|t| {
        let p = t.island_point(at);
        let inside = t.hover(Some(p));
        t.seen.offered |= inside;
        inside
    })
    .unwrap_or(false);
    if inside { NSDragOperation::Copy } else { NSDragOperation::None }
}

define_class!(
    /// The drop target's view: its layers, the drag's callbacks, the frame clock's target.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "HeimdallDropView"]
    pub(crate) struct DropView;

    impl DropView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(draggingEntered:))]
        fn dragging_entered(&self, sender: &ProtocolObject<dyn NSDraggingInfo>) -> NSDragOperation {
            dragged(sender)
        }

        #[unsafe(method(draggingUpdated:))]
        fn dragging_updated(&self, sender: &ProtocolObject<dyn NSDraggingInfo>) -> NSDragOperation {
            dragged(sender)
        }

        #[unsafe(method(draggingExited:))]
        fn dragging_exited(&self, _sender: Option<&ProtocolObject<dyn NSDraggingInfo>>) {
            with_target(|t| t.hover(None));
        }

        #[unsafe(method(prepareForDragOperation:))]
        fn prepare_for_drag(&self, sender: &ProtocolObject<dyn NSDraggingInfo>) -> bool {
            if !takes(sender) {
                return false.into();
            }
            let at = sender.draggingLocation();
            with_target(|t| t.motion.is_over() || t.in_zone(t.island_point(at))).unwrap_or(false)
        }

        #[unsafe(method(performDragOperation:))]
        fn perform_drag(&self, sender: &ProtocolObject<dyn NSDraggingInfo>) -> bool {
            let at = sender.draggingLocation();
            let pb = sender.draggingPasteboard();
            with_target(|t| {
                let p = t.island_point(at);
                t.take(&pb, p)
            })
            .unwrap_or(false)
        }

        #[unsafe(method(draggingEnded:))]
        fn dragging_ended(&self, _sender: &ProtocolObject<dyn NSDraggingInfo>) {
            with_target(|t| t.disarm());
        }

        #[unsafe(method(dropFrame:))]
        fn drop_frame(&self, _timer: &NSTimer) {
            with_target(|t| {
                let now = t.now();
                t.after(now)
            });
        }

        #[unsafe(method(dropWatch:))]
        fn drop_watch(&self, _timer: &NSTimer) {
            with_target(|t| t.watch_tick());
        }

        #[unsafe(method(dropPoll:))]
        fn drop_poll(&self, _timer: &NSTimer) {
            with_target(|t| t.poll_tick());
        }
    }
);
