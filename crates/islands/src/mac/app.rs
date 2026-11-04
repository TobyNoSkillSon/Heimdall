//! The app: the islands folder's watcher, one controller per island, the docks and their layout, the state readback
//! (`state.json`), drops, and Scheduled items' due times (Swift `App.swift`, plus night 1023's due countdown).

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::rc::Rc;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_app_kit::{NSEvent, NSScreen, NSStatusBar, NSWorkspace};
use objc2_foundation::{MainThreadMarker, NSNotification, NSNotificationCenter, NSRect, NSString};
use serde_json::Value;

use super::island::Ctl;
use super::look::Spring;
use super::store::Store;
use super::sys;
use crate::model::due::{Cal, Due};
use crate::model::geometry::{Point, Rect, Size, Vector};
use crate::model::flash::{self, Cmd, Flash};
use crate::model::{edge_column, edge_drop, j, left_edge_column, time, top_row, Island, IslandFile, Json, Paths, PlanList, Visibility};

thread_local! {
    static APP: RefCell<Option<Rc<App>>> = const { RefCell::new(None) };
}

/// The running app, if any (`None` in unit tests).
pub fn try_app() -> Option<Rc<App>> {
    APP.with(|a| a.borrow().clone())
}

pub fn app() -> Rc<App> {
    APP.with(|a| a.borrow().clone()).expect("the app is running")
}

fn mtm() -> MainThreadMarker {
    MainThreadMarker::new().expect("main thread")
}

fn rect_from(r: NSRect) -> Rect {
    Rect::new(r.origin.x, r.origin.y, r.size.width, r.size.height)
}

pub const BROWSER_CHROME: f64 = 88.0;
/// The side columns' first island starts this far up the display, as a share of its height from the bottom.
pub const COLUMN_START: f64 = 0.6;

pub struct App {
    pub controllers: RefCell<BTreeMap<String, Rc<Ctl>>>,
    store: RefCell<Option<Rc<Store>>>,
    seq: Cell<usize>,
    monitors: RefCell<Vec<Retained<AnyObject>>>,
    current_screen: Cell<Rect>,
    /// The screens as last laid out (frames, visible frames, backing scales): a screen-parameters notice that changes none of
    /// them is ignored (3 Nov: a WebKit web view in the process brought ~120 such notices a second, each a relayout).
    screens_seen: RefCell<Vec<(Rect, Rect, f64)>>,
    relayout_queued: Cell<bool>,
    running: Cell<bool>,
    state_queued: Cell<bool>,
    observers: RefCell<Vec<Retained<objc2::runtime::ProtocolObject<dyn objc2_foundation::NSObjectProtocol>>>>,
    due_timer: RefCell<Option<sys::WallTimer>>,
    due_next: Cell<Option<f64>>,
    /// Settled's one wall-clock timer for the next local midnight, when its week moves on a day.
    day_timer: RefCell<Option<sys::WallTimer>>,
    /// Scheduled items this run told due (id, due bits): held until Heimdall's `told` stamp is in the file.
    due_told: RefCell<std::collections::HashSet<(String, u64)>>,
    cal: Cal,
    /// Every island off screen (`visibility.json`); the islands, their files and their places are untouched.
    hidden: Cell<bool>,
    visibility_watch: RefCell<Option<Rc<super::filewatch::FileWatch>>>,
    /// While hidden, the islands back on screen for news (`model/flash.rs`), and its one one-shot timer.
    flash: RefCell<Flash>,
    flash_timer: RefCell<Option<sys::Work1>>,
}

impl App {
    pub fn install() -> Rc<App> {
        let a = Rc::new(App {
            controllers: RefCell::new(BTreeMap::new()),
            store: RefCell::new(None),
            seq: Cell::new(0),
            monitors: RefCell::new(vec![]),
            current_screen: Cell::new(Rect::ZERO),
            screens_seen: RefCell::new(vec![]),
            relayout_queued: Cell::new(false),
            running: Cell::new(false),
            state_queued: Cell::new(false),
            observers: RefCell::new(vec![]),
            due_timer: RefCell::new(None),
            due_next: Cell::new(None),
            day_timer: RefCell::new(None),
            due_told: RefCell::new(Default::default()),
            cal: Cal::current(),
            hidden: Cell::new(false),
            visibility_watch: RefCell::new(None),
            flash: RefCell::new(Flash::new()),
            flash_timer: RefCell::new(None),
        });
        APP.with(|x| *x.borrow_mut() = Some(a.clone()));
        a
    }

    pub fn start(self: &Rc<Self>) {
        Paths::ensure();
        // Hidden before the first island is made, so a launch while hidden never flashes them.
        self.hidden.set(Visibility::hidden());
        *self.visibility_watch.borrow_mut() = Some(super::filewatch::FileWatch::new(&Paths::visibility(), Box::new(|| app().set_hidden(Visibility::hidden()))));
        let store = Store::new(
            Paths::islands(),
            Box::new(|up, rm| {
                app().apply(up, rm);
            }),
        );
        *self.store.borrow_mut() = Some(store.clone());
        store.start();
        // From here on a new island has just been made: it drops out of the notch.
        self.running.set(true);
        let center = NSNotificationCenter::defaultCenter();
        let observe = |center: &NSNotificationCenter, name: &str, f: fn()| {
            let block = block2::RcBlock::new(move |_n: std::ptr::NonNull<NSNotification>| f());
            let main = objc2_foundation::NSOperationQueue::mainQueue();
            let o = unsafe { center.addObserverForName_object_queue_usingBlock(Some(&NSString::from_str(name)), None, Some(&main), &block) };
            self.observers.borrow_mut().push(o);
        };
        observe(&center, "NSApplicationDidChangeScreenParametersNotification", || app().screens_changed());
        // Due times: re-evaluated on wake from sleep and on a clock or time-zone change.
        observe(&center, "NSSystemClockDidChangeNotification", || app().clock_changed());
        observe(&center, "NSSystemTimeZoneDidChangeNotification", || {
            objc2_foundation::NSTimeZone::resetSystemTimeZone();
            app().clock_changed()
        });
        let ws = NSWorkspace::sharedWorkspace().notificationCenter();
        observe(&ws, "NSWorkspaceDidWakeNotification", || app().clock_changed());
        self.screens_changed();
        self.review_due(false);
        self.arm_day();
        super::streams::watch();
    }

    /// Whether every island is off screen now.
    pub fn hidden(&self) -> bool {
        self.hidden.get()
    }

    /// Every island's window out of (or back onto) the screen at once. Nothing else changes: open stays open, places
    /// and files stay, timers run; islands that arrive or change meanwhile are laid out as usual and appear with the
    /// rest. Shown again, the layout is redone for the display the pointer is on now.
    pub fn set_hidden(&self, hidden: bool) {
        if hidden == self.hidden.replace(hidden) {
            return;
        }
        // Shown or hidden by the user: islands up for news are simply part of the rest now (already on screen, they stay
        // as they are instead of growing in again).
        let up = self.flash.borrow_mut().clear();
        for id in &up {
            if let Some(c) = self.ctl(id) {
                c.flash_reset();
            }
        }
        if let Some(t) = self.flash_timer.borrow_mut().take() {
            t.cancel();
        }
        // Each shrinks away or grows back in turn across the stack (`model/toggle.rs`).
        super::island::debug(&format!("set_hidden {hidden}"));
        let all = self.all();
        let reduce = super::island::reduce_motion();
        let delays = crate::model::toggle::delays(&all.iter().map(|c| c.shape_rect()).collect::<Vec<_>>(), !hidden, reduce);
        for (c, d) in all.iter().zip(delays) {
            c.set_screen_hidden(hidden, d, reduce, up.contains(&c.id));
        }
        if !hidden {
            self.relayout();
            // An open list shown again: its proposal is in front of the user now.
            for c in &all { c.tell_seen(&c.island()); }
        }
    }

    /// Whether an island's window belongs on screen: every island while shown; while hidden, one up for news.
    pub fn on_screen(&self, id: &str) -> bool {
        !self.hidden.get() || self.flash.borrow().is_up(id)
    }

    /// News for an island while every island is hidden (`flash::news`): it comes back for a moment.
    fn flash_news(&self, c: &Rc<Ctl>, sticky: bool) {
        if !self.hidden.get() || !self.running.get() || c.removing() {
            return;
        }
        let cmds = self.flash.borrow_mut().news(&c.id, sys::media_time(), sticky);
        self.flash_run(cmds);
        // Already open (a viewer shown again opens before its news is known): held from the start.
        self.flash_sync(c);
    }

    /// An island up for news opened or closed, or its question was seen: held or released (`Ctl::refresh_attention`).
    pub fn flash_sync(&self, c: &Ctl) {
        if !self.flash.borrow().is_up(&c.id) {
            return;
        }
        let now = sys::media_time();
        let cmds = {
            let mut f = self.flash.borrow_mut();
            let mut cmds = f.set_open(&c.id, c.expanded() || c.is_confirming(), now);
            cmds.extend(f.set_sticky(&c.id, c.attention_shown().is_some(), now));
            cmds
        };
        self.flash_run(cmds);
    }

    fn flash_run(&self, cmds: Vec<Cmd>) {
        for cmd in cmds {
            match cmd {
                Cmd::Reveal(id) => {
                    if let Some(c) = self.ctl(&id) {
                        c.flash_in();
                    }
                }
                Cmd::Pulse(id) => {
                    // After the fade in (or the arrival) has begun to show it.
                    sys::after(flash::FADE_IN, move || {
                        if let Some(c) = app().ctl(&id).filter(|_| app().flash.borrow().is_up(&id)) {
                            c.flash_pulse();
                        }
                    });
                }
                Cmd::Conceal(id) => {
                    if let Some(c) = self.ctl(&id) {
                        c.flash_out();
                    }
                }
                Cmd::Gone(id) => {
                    if let Some(c) = self.ctl(&id) {
                        c.flash_gone();
                    }
                }
            }
        }
        if let Some(t) = self.flash_timer.borrow_mut().take() {
            t.cancel();
        }
        let now = sys::media_time();
        if let Some(at) = self.flash.borrow().next_wake(now) {
            *self.flash_timer.borrow_mut() = Some(sys::after_cancellable(at - now, || app().flash_wake()));
        }
    }

    /// The flash timer: the pointer over an island holds it (looked at only now, no tracking), then the clock runs.
    fn flash_wake(&self) {
        let now = sys::media_time();
        let p = NSEvent::mouseLocation();
        let p = Point::new(p.x, p.y);
        let up = self.flash.borrow().up();
        let mut cmds = vec![];
        for id in up {
            // Lab renders never read the user's pointer.
            let over = !super::island::offscreen() && self.ctl(&id).is_some_and(|c| c.visible_shape_rect().contains(p));
            // Pointed at while fading out: back before the clock runs.
            cmds.extend(self.flash.borrow_mut().set_hover(&id, over, now));
        }
        cmds.extend(self.flash.borrow_mut().tick(now));
        self.flash_run(cmds);
    }

    pub fn ctl(&self, id: &str) -> Option<Rc<Ctl>> {
        self.controllers.borrow().get(id).cloned()
    }

    pub fn all(&self) -> Vec<Rc<Ctl>> {
        self.controllers.borrow().values().cloned().collect()
    }

    fn apply(self: &Rc<Self>, upserts: Vec<Island>, removed: Vec<String>) {
        for id in &removed {
            let c = self.controllers.borrow_mut().remove(id);
            self.flash.borrow_mut().removed(id);
            if let Some(c) = c {
                let p = c.panel.clone();
                c.remove(move || p.close());
            }
        }
        if !removed.is_empty() {
            self.state_changed();
        }
        for i in upserts {
            // Hidden, or the retired To do list (never drawn; its file is left for Heimdall to migrate).
            if i.hidden() || PlanList::legacy_todo(&i.id, &i.raw) {
                let c = self.controllers.borrow_mut().remove(&i.id);
                self.flash.borrow_mut().removed(&i.id);
                if let Some(c) = c {
                    let p = c.panel.clone();
                    c.remove(move || p.close());
                }
                continue;
            }
            let existing = self.ctl(&i.id);
            if let Some(c) = existing {
                let old = c.island();
                c.update(i.clone());
                if flash::news(Some(&old), &i).is_some() {
                    self.flash_news(&c, c.attention_shown().is_some());
                }
                continue;
            }
            self.seq.set(self.seq.get() + 1);
            let expanded = i.expanded();
            let c = Ctl::new(i.clone(), self.seq.get());
            c.set_from_notch(self.running.get());
            self.controllers.borrow_mut().insert(i.id.clone(), c.clone());
            if expanded {
                c.set_initial_expanded();
            }
            // Arrived while hidden: it drops in at its place as usual, the one island on screen (`place` asks).
            self.flash_news(&c, !expanded && crate::model::Attention::of(&i).is_some());
        }
        self.relayout();
        // What an island reads of the others may have changed (a job island ran or stopped under a goal, a goal was
        // renamed): those open bodies are drawn again; then the one label timer is set for the next change.
        for c in self.all() {
            c.refresh_due_labels();
        }
        if self.running.get() {
            self.review_due(false);
        }
    }

    // MARK: State readback (state.json), written shortly after anything changes; nothing at rest.

    pub fn state_changed(&self) {
        if self.state_queued.get() {
            return;
        }
        self.state_queued.set(true);
        sys::after(0.15, || {
            let a = app();
            a.state_queued.set(false);
            a.write_state();
        });
    }

    fn write_state(&self) {
        let mut out = Json::new();
        let screens: Vec<Rect> = NSScreen::screens(mtm()).iter().map(|s| rect_from(s.frame())).collect();
        let cur = self.current_screen.get();
        for (id, c) in self.controllers.borrow().iter() {
            if c.removing() {
                continue;
            }
            let r = c.shape_rect();
            let scr = screens.iter().copied().find(|f| f.intersects(&r)).unwrap_or(cur);
            let i = c.island();
            let mut o = Json::new();
            o.insert("open".into(), Value::Bool(c.expanded()));
            o.insert("placed".into(), Value::Bool(i.position().is_some()));
            o.insert("x".into(), j::number(r.min_x() - scr.min_x()));
            o.insert("y".into(), j::number(scr.max_y() - r.max_y()));
            o.insert("width".into(), j::number(r.w));
            o.insert("height".into(), j::number(r.h));
            if let Some(side) = i.side() {
                o.insert("side".into(), Value::from(side));
            }
            if i.edge_y().is_some() {
                o.insert("pinned".into(), Value::Bool(true));
            }
            out.insert(id.clone(), Value::Object(o));
        }
        let mut st = Json::new();
        st.insert("islands".into(), Value::Object(out));
        st.insert("display".into(), Value::Object(crate::jobj! {"width": j::number(cur.w), "height": j::number(cur.h)}));
        let _ = IslandFile::write_atomic(&st, &Paths::state());
    }

    /// The display the islands live on: the one with the pointer.
    pub fn active_screen(&self) -> Option<Retained<NSScreen>> {
        let p = NSEvent::mouseLocation();
        let screens = NSScreen::screens(mtm());
        for s in screens.iter() {
            let f = rect_from(s.frame());
            // NSMouseInRect for an unflipped rect: x in [minX, maxX), y in (minY, maxY].
            if p.x >= f.min_x() && p.x < f.max_x() && p.y > f.min_y() && p.y <= f.max_y() {
                return Some(s.clone());
            }
        }
        NSScreen::mainScreen(mtm()).or_else(|| screens.iter().next())
    }

    /// Coalesced: many file changes in one run-loop turn produce one layout pass.
    pub fn relayout(&self) {
        if self.relayout_queued.get() {
            return;
        }
        self.relayout_queued.set(true);
        sys::soon(|| {
            let a = app();
            a.relayout_queued.set(false);
            a.layout_now(Spring::GROW);
        });
    }

    fn band(screen: &NSScreen) -> f64 {
        let f = rect_from(screen.frame());
        let v = rect_from(screen.visibleFrame());
        screen.safeAreaInsets().top.max(f.max_y() - v.max_y()).max(NSStatusBar::systemStatusBar().thickness())
    }

    /// The top of the row just under the menu bar: islands never enter the menu bar strip.
    pub fn row_top(screen: &NSScreen) -> f64 {
        rect_from(screen.frame()).max_y() - Self::band(screen) - 6.0
    }

    /// Where the side columns start, from the display's top: 60 % of the display's height up from its bottom, so the
    /// top corners, where websites keep their controls, stay empty; never higher than under the
    /// menu bar plus a browser's chrome. A tall stack still moves up as far as it must to stay on screen (`edge_column`).
    pub fn column_top(screen: &NSScreen) -> f64 {
        (rect_from(screen.frame()).h * (1.0 - COLUMN_START)).max(Self::band(screen) + BROWSER_CHROME)
    }

    /// The tallest an island may be on `screen`.
    pub fn height_cap(screen: &NSScreen) -> f64 {
        (Self::row_top(screen) - (rect_from(screen.visibleFrame()).min_y() + 8.0) - 24.0).max(200.0)
    }

    pub fn layout_now(&self, spring: Spring) {
        let Some(screen) = self.active_screen() else { return };
        let f = rect_from(screen.frame());
        self.current_screen.set(f);
        let top = Self::row_top(&screen);
        let live: Vec<Rc<Ctl>> = self.all().into_iter().filter(|c| !c.removing()).collect();
        let cap = Self::height_cap(&screen);
        for c in &live {
            c.set_height_cap(cap);
        }
        let placed: Vec<&Rc<Ctl>> = live.iter().filter(|c| c.island().position().is_some()).collect();
        for c in &placed {
            let p = c.island().position().unwrap();
            let size = c.size();
            c.place(Point::new(f.min_x() + p.0 + size.w / 2.0, f.max_y() - p.1), f, top, spring);
        }
        let local = |r: Rect| Rect::new(r.min_x() - f.min_x(), f.max_y() - r.max_y(), r.w, r.h);
        let mut obstacles: Vec<Rect> = placed.iter().map(|c| local(c.shape_rect())).collect();
        let visible = rect_from(screen.visibleFrame());
        for side in ["top", "left", "right"] {
            let stack = dock_order(live.iter().filter(|c| c.island().side().as_deref() == Some(side)).cloned().collect());
            if stack.is_empty() {
                continue;
            }
            let sizes: Vec<Size> = stack.iter().map(|c| c.slot_size()).collect();
            let bottom = f.max_y() - visible.min_y() - 8.0;
            let pins: Vec<Option<f64>> = stack.iter().map(|c| c.island().edge_y()).collect();
            let slots = match side {
                "left" => left_edge_column(&sizes, &pins, f.w, Self::column_top(&screen), f.max_y() - top, bottom, 8.0, 8.0, &obstacles),
                "top" => top_row(&sizes, f.w, f.max_y() - top, None, 8.0, 8.0),
                _ => edge_column(&sizes, &pins, f.w, Self::column_top(&screen), f.max_y() - top, bottom, 8.0, 8.0, &obstacles),
            };
            for (c, r) in stack.iter().zip(&slots) {
                c.place(Point::new(f.min_x() + r.mid_x(), f.max_y() - r.min_y()), f, top, spring);
            }
            if side == "top" {
                obstacles.extend(slots);
            }
        }
    }

    /// Where a released island goes: thrown or dropped, it attaches to the nearer side edge at the height it lands and
    /// keeps that height. Writes the island's file.
    pub fn drop_island(&self, c: &Ctl, r: Rect, v: Vector) -> Option<Json> {
        let screen = self.active_screen()?;
        let f = rect_from(screen.frame());
        let (side, top) = edge_drop(r, v, f, Self::row_top(&screen), rect_from(screen.visibleFrame()).min_y() + 8.0, 500.0);
        IslandFile::patch(&c.id, |o| {
            o.remove("position");
            o.insert("side".into(), Value::from(side.clone()));
            o.insert("edgeY".into(), j::number(f.max_y() - top));
        })
    }

    fn screens_changed(&self) {
        let now: Vec<(Rect, Rect, f64)> = NSScreen::screens(mtm()).iter().map(|s| (rect_from(s.frame()), rect_from(s.visibleFrame()), s.backingScaleFactor())).collect();
        if *self.screens_seen.borrow() == now {
            return;
        }
        *self.screens_seen.borrow_mut() = now;
        for m in self.monitors.borrow_mut().drain(..) {
            // SAFETY: monitors we added.
            unsafe { NSEvent::removeMonitor(&m) };
        }
        if NSScreen::screens(mtm()).count() > 1 {
            let check = || {
                let a = app();
                if let Some(s) = a.active_screen() {
                    if rect_from(s.frame()) != a.current_screen.get() {
                        a.relayout();
                    }
                }
            };
            let mask = objc2_app_kit::NSEventMask::MouseMoved | objc2_app_kit::NSEventMask::LeftMouseDragged;
            let g = block2::RcBlock::new(move |_e: std::ptr::NonNull<NSEvent>| check());
            if let Some(m) = NSEvent::addGlobalMonitorForEventsMatchingMask_handler(mask, &g) {
                self.monitors.borrow_mut().push(m);
            }
            let l = block2::RcBlock::new(move |e: std::ptr::NonNull<NSEvent>| -> *mut NSEvent {
                check();
                e.as_ptr()
            });
            if let Some(m) = unsafe { NSEvent::addLocalMonitorForEventsMatchingMask_handler(objc2_app_kit::NSEventMask::MouseMoved, &l) } {
                self.monitors.borrow_mut().push(m);
            }
        }
        self.relayout();
    }

    // MARK: Scheduled due times: the app counts down; a due item stays in Scheduled, drawn as due now.

    /// The wall clock jumped (wake, clock or time-zone change): review at once and set the timer again.
    pub fn clock_changed(&self) {
        // Labels read the local clock: an open Scheduled list is redrawn now, not at its next change.
        if let Some(c) = self.ctl("scheduled") {
            c.refresh_due_labels();
        }
        self.review_due(true);
        if let Some(c) = self.ctl("settled") {
            c.refresh_day();
        }
        self.arm_day();
    }

    /// Settled's week is the last seven local days: at each local midnight its count and columns move on a day.
    fn arm_day(&self) {
        let now = time::now();
        let next = self.cal.add_days(1, self.cal.start_of_day(now)).unwrap_or(now + 86400.0);
        *self.day_timer.borrow_mut() = sys::after_wall((next - now).max(1.0) + 1.0, || {
            let a = app();
            let fired = a.day_timer.borrow_mut().take();
            drop(fired);
            if let Some(c) = a.ctl("settled") {
                c.refresh_day();
            }
            a.arm_day();
        });
    }

    /// Tells the app once of each Scheduled item that fell due (a `due` response; the item stays where it is, drawn as
    /// due now, at the top), then sets one one-shot wall-clock timer for the next moment a label changes or an item
    /// falls due. No due items: no timer.
    pub fn review_due(&self, force: bool) {
        let now = time::now();
        // Each due item is told once per due time: Heimdall stamps `told: <due>` on it when it handles the response (the
        // islands app only reads it); until the stamp arrives, this run's own set keeps it from being told twice.
        let sched = IslandFile::read(&Paths::file("scheduled")).unwrap_or_default();
        let untold = Due::untold(&sched, now);
        {
            let mut told = self.due_told.borrow_mut();
            for (id, due) in &untold {
                if told.insert((id.clone(), due.to_bits())) {
                    IslandFile::respond_object(&crate::model::Responses::due(id));
                }
            }
            // Stamped by Heimdall, rescheduled or gone: forgotten here (the file decides from now on).
            told.retain(|(id, d)| untold.iter().any(|(i, due)| i == id && due.to_bits() == *d));
        }
        if let Some(c) = self.ctl("scheduled") {
            c.refresh_due_labels();
        }
        // Labels are seen only while Scheduled is open: closed, the timer wakes only when an item falls due (opening it
        // reviews again).
        let open = self.ctl("scheduled").is_some_and(|c| c.expanded());
        // The one-system labels of every open island (checked N min ago, a proposal's 48 h, a quiet job island) share it.
        let next = crate::model::Plan::items(&sched)
            .iter()
            .filter_map(|i| i.due)
            .filter_map(|d| if open { Due::next_change(d, now, &self.cal) } else { (d > now).then_some(d) })
            .chain(self.all().iter().filter_map(|c| c.next_label_change(now)).filter(|t| *t > now))
            .fold(None, |a: Option<f64>, b| Some(a.map_or(b, |a| a.min(b))));
        if !force && next == self.due_next.get() && self.due_timer.borrow().is_some() {
            return;
        }
        *self.due_timer.borrow_mut() = None;
        self.due_next.set(next);
        let Some(next) = next else { return };
        let w = sys::after_wall((next - now).max(0.05) + 0.02, || {
            let a = app();
            let fired = a.due_timer.borrow_mut().take();
            drop(fired);
            a.due_next.set(None);
            for c in a.all() {
                c.refresh_due_labels();
            }
            a.review_due(false);
        });
        *self.due_timer.borrow_mut() = w;
    }
}

fn dock_order(mut cs: Vec<Rc<Ctl>>) -> Vec<Rc<Ctl>> {
    cs.sort_by(|a, b| {
        let ka = (a.island().order().unwrap_or(a.seq as f64), a.seq);
        let kb = (b.island().order().unwrap_or(b.seq as f64), b.seq);
        ka.partial_cmp(&kb).unwrap_or(std::cmp::Ordering::Equal)
    });
    cs
}
