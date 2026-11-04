//! Lab (Swift `LaneRender.swift`): the three island fixes of 22 Oct afternoon, driven through the real event path
//! offscreen and rendered.
//!   FLOATING_ISLANDS_DIR=<lab dir> FloatingIslands --render-lane <out dir>   (lab/lane_islands.sh runs it)
//! 1. A 40-item Scheduled list is capped under the display's height and scrolls (trackpad gesture with momentum,
//!    rubber band at the end, a wheel step). 2. Answers ask "Sure?" first. 3. A new island with no `origin` drops out of
//!    the notch; one already there at launch does not. Writes PNGs, frame folders for films, and checks.txt.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::ffi::c_void;
use std::rc::Rc;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{class, msg_send};
use objc2_app_kit::{NSEvent, NSEventModifierFlags, NSEventType, NSScreen, NSView};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_foundation::{MainThreadMarker, NSRect};
use serde_json::Value;

use super::app::{app, App};
use super::confirm;
use super::draw::{self, Bitmap};
use super::island::{notch_rect, Ctl};
use super::look::*;
use super::parts::Control;
use super::render::{put, screen, Render};
use super::selftest::{read, responses};
use super::sys;
use crate::jobj;
use crate::model::geometry::Rect;
use crate::model::{j, IslandFile, Json, Paths};

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventCreateScrollWheelEvent2(source: *const c_void, units: u32, count: u32, w1: i32, w2: i32, w3: i32) -> *mut c_void;
    fn CGEventSetIntegerValueField(e: *mut c_void, field: u32, v: i64);
    fn CGEventSetDoubleValueField(e: *mut c_void, field: u32, v: f64);
    fn CGEventSetLocation(e: *mut c_void, p: CGPoint);
    fn CFRelease(r: *mut c_void);
}

fn mtm() -> MainThreadMarker {
    MainThreadMarker::new().unwrap()
}

fn rect_from(r: NSRect) -> Rect {
    Rect::new(r.origin.x, r.origin.y, r.size.width, r.size.height)
}

type Step = (f64, Box<dyn FnOnce()>);

struct Lane {
    render: Rc<Render>,
    checks: RefCell<Vec<(String, bool, String)>>,
    steps: RefCell<VecDeque<Step>>,
    trace: RefCell<Vec<f64>>,
}

fn c(id: &str) -> Option<Rc<Ctl>> {
    app().ctl(id)
}

impl Lane {
    fn check(&self, name: &str, ok: bool, detail: impl Into<String>) {
        self.checks.borrow_mut().push((name.to_owned(), ok, detail.into()));
    }

    /// Steps run in order, each `dt` after the previous one ran (a busy main thread delays the rest, never reorders).
    fn at(self: &Rc<Self>, dt: f64, f: impl FnOnce() + 'static) {
        self.steps.borrow_mut().push_back((dt, Box::new(f)));
        if self.steps.borrow().len() == 1 {
            let me = self.clone();
            sys::soon(move || me.next());
        }
    }

    fn next(self: &Rc<Self>) {
        let Some((dt, f)) = self.steps.borrow_mut().pop_front() else { return };
        let me = self.clone();
        sys::after(dt, move || {
            f();
            me.next();
        });
    }

    fn clear_responses(&self) {
        if let Ok(d) = std::fs::read_dir(Paths::responses()) {
            for e in d.flatten() {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }

    fn click(&self, ctl: &Ctl, m: impl Fn(&Control) -> bool) {
        let Some(p) = ctl.control_centre(m) else {
            self.check(&format!("control found on {}", ctl.id), false, "");
            return;
        };
        send(ctl, NSEventType::LeftMouseDown, p);
        send(ctl, NSEventType::LeftMouseUp, p);
    }

    /// Frames at 60 fps for `duration` seconds into `<name>/NNN.png`.
    fn film(self: &Rc<Self>, name: &str, rect: Rect, duration: f64) {
        let dir = self.render.out.join(name);
        let _ = std::fs::create_dir_all(&dir);
        for k in 0..(duration * 60.0) as usize {
            let (me, dir) = (self.clone(), dir.clone());
            sys::after(k as f64 / 60.0, move || {
                if let Some(img) = me.render.image(rect) {
                    draw::write_png(&img, &dir.join(format!("{k:03}.png")));
                }
            });
        }
    }

    /// A viewer island as it looks: the shape from the layer tree, its live view drawn on top.
    fn viewer_shot(&self, name: &str, ctl: &Ctl) {
        let rect = ctl.shape_rect().inset(-20.0, -20.0).integral();
        let s = 2.0;
        let Some(b) = Bitmap::new((rect.w * s) as usize, (rect.h * s) as usize) else { return };
        b.scale(s, s);
        b.fill(CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(rect.w, rect.h)), &rgb(0x22252B, 1.0).CGColor());
        let f = rect_from(ctl.panel.frame());
        b.save();
        b.translate(f.min_x() - rect.min_x(), f.max_y() - rect.min_y());
        b.scale(1.0, -1.0);
        shown(&ctl.root).renderInContext(b.cg());
        if let Some(live) = ctl.file_viewer().and_then(|v| v.root()).filter(|r| !r.isHidden()) {
            if let Some(l) = live.layer() {
                let fr = live.frame();
                b.save();
                b.translate(fr.origin.x, fr.origin.y);
                shown(&l).renderInContext(b.cg());
                b.restore();
            }
        }
        b.restore();
        if let Some(img) = b.image() {
            draw::write_png(&img, &self.render.out.join(format!("{name}.png")));
        }
    }
}

/// A click as the window would route it: to the view under the point (the island, or a viewer's action row).
/// Offscreen windows are never ordered in, so the hit test is done here instead of by the window server.
fn send(ctl: &Ctl, ty: NSEventType, p: CGPoint) {
    let Some(e) = NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
        ty,
        p,
        NSEventModifierFlags::empty(),
        sys::uptime(),
        ctl.panel.windowNumber(),
        None,
        0,
        1,
        if ty == NSEventType::LeftMouseUp { 0.0 } else { 1.0 },
    ) else {
        return;
    };
    let Some(content) = ctl.panel.contentView() else { return };
    let Some(frame) = (unsafe { content.superview() }) else { return };
    let q = frame.convertPoint_fromView(p, None);
    let target: Option<Retained<NSView>> = unsafe { msg_send![&*content, hitTest: q] };
    let Some(t) = target else { return };
    unsafe {
        if ty == NSEventType::LeftMouseDown {
            let _: () = msg_send![&*t, mouseDown: &*e];
        } else {
            let _: () = msg_send![&*t, mouseUp: &*e];
        }
    }
}

/// A scroll event over the island's middle, as the system sends it, delivered to the island's view.
fn deliver(ctl: &Ctl, e: *mut c_void) {
    let r = ctl.shape_rect();
    let h = NSScreen::screens(mtm()).iter().next().map(|s| s.frame().size.height).unwrap_or(1117.0);
    unsafe { CGEventSetLocation(e, CGPoint::new(r.mid_x(), h - r.mid_y())) };
    let ns: Option<Retained<NSEvent>> = unsafe { msg_send![class!(NSEvent), eventWithCGEvent: e as *mut objc2_core_graphics::CGEvent] };
    if let Some(ns) = ns {
        let _: () = unsafe { msg_send![&*ctl.view, scrollWheel: &*ns] };
    }
    unsafe { CFRelease(e) };
}

/// A trackpad scroll: pixel deltas, phases (1 began, 2 changed, 4 ended; momentum 1 begin, 2 continue, 3 end).
fn scroll(ctl: &Ctl, dy: f64, phase: i64, momentum: i64) {
    let e = unsafe { CGEventCreateScrollWheelEvent2(std::ptr::null(), 0, 1, dy as i32, 0, 0) };
    if e.is_null() {
        return;
    }
    unsafe {
        CGEventSetIntegerValueField(e, 88, 1); // kCGScrollWheelEventIsContinuous
        CGEventSetDoubleValueField(e, 96, dy); // kCGScrollWheelEventPointDeltaAxis1
        CGEventSetIntegerValueField(e, 99, phase); // kCGScrollWheelEventScrollPhase
        CGEventSetIntegerValueField(e, 123, momentum); // kCGScrollWheelEventMomentumPhase
    }
    deliver(ctl, e);
}

/// A mouse wheel: `lines` notches (no phases).
fn wheel(ctl: &Ctl, lines: i32) {
    let e = unsafe { CGEventCreateScrollWheelEvent2(std::ptr::null(), 1, 1, lines, 0, 0) };
    if !e.is_null() {
        deliver(ctl, e);
    }
}

fn height_cap() -> f64 {
    NSScreen::mainScreen(mtm()).map(|s| App::height_cap(&s)).unwrap_or(0.0)
}

const LONG_DETAIL: &str = "Three caches hold 16.4 GB of weights no project has loaded since September; every one of them can be downloaded again, so deleting them loses nothing.";

pub fn run(out: &str) {
    if std::env::var("FLOATING_ISLANDS_DIR").is_err() {
        println!("render-lane: refusing to run without FLOATING_ISLANDS_DIR (it removes island files)");
        std::process::exit(2);
    }
    let render = Render::new(out);
    let _ = std::fs::create_dir_all(&render.out);
    let l = Rc::new(Lane { render, checks: RefCell::new(vec![]), steps: RefCell::new(VecDeque::new()), trace: RefCell::new(vec![]) });
    let scr = screen();
    // 3. Notch: `launch` was in the folder before the app started (the script wrote it).
    let m = l.clone();
    l.at(0.8, move || {
        let ln = c("launch");
        m.check("an island there at launch appears in place (no notch flight)", ln.as_ref().is_some_and(|c| c.born_from().is_none()), format!("{:?}", ln.map(|c| c.born_from())));
        put("job-new", jobj! {"title": "Port voices", "state": "running", "stage": "Starting", "icon": "waveform", "side": "right"});
        let n = notch_rect(NSScreen::mainScreen(mtm()).as_deref());
        let r = Rect::new(n.min_x() - 40.0, scr.max_y() - 260.0, scr.max_x() - n.min_x() + 40.0, 260.0);
        m.render.sheet("notch-flight", &[], Some(r), &[0.02, 0.05, 0.08, 0.12, 0.16, 0.22, 0.3, 0.45, 0.8], Some(3));
        m.film("film-notch", r, 0.9);
    });
    let m = l.clone();
    l.at(1.6, move || {
        let jn = c("job-new");
        let notch = notch_rect(NSScreen::mainScreen(mtm()).as_deref());
        m.check(
            "a new island with no origin key drops out of the notch",
            jn.as_ref().is_some_and(|c| c.born_from() == Some(notch)),
            format!("{:?}", jn.as_ref().map(|c| c.born_from())),
        );
        m.check("it lands in the right column", jn.as_ref().is_some_and(|c| c.shape_rect().max_x() > scr.max_x() - 20.0), format!("{:?}", jn.map(|c| c.shape_rect())));
        m.check("the file never gains an origin key", read("job-new").is_some_and(|o| !o.contains_key("origin")), "");
        m.render.shot("notch-landed", None, Some(Rect::new(scr.mid_x() - 200.0, scr.max_y() - 260.0, scr.w / 2.0 + 200.0, 260.0)));
    });
    // 1. A list taller than the display.
    l.at(0.6, move || {
        let whens = ["Tonight", "Tomorrow", "Fri 10:00", "Sat", "Next week"];
        let titles = ["Arsenal: check the site", "Heimdall: menu revamp", "Phonon voices as presets", "Review the night pass", "iPad sale: photos", "Vella: TestFlight build"];
        let items: Vec<Value> = (1..=40)
            .map(|k| serde_json::json!({"id": format!("s{k}"), "title": format!("{k}. {}", titles[k % 6]), "when": whens[k % 5], "detail": "One quiet line about what happens then."}))
            .collect();
        put("scheduled", jobj! {"preset": "todo", "list": "scheduled", "expanded": true, "items": items});
    });
    let m = l.clone();
    l.at(0.6, move || {
        let Some(sc) = c("scheduled") else {
            m.check("scheduled shown", false, "");
            return;
        };
        let s = NSScreen::mainScreen(mtm()).unwrap();
        let vis = rect_from(s.visibleFrame());
        let room = vis.h;
        let sr = sc.shape_rect();
        m.check(
            "a 40-item list is capped under the visible screen height",
            sr.h < room - 20.0 && sr.h > room - 120.0,
            format!("island {} pt, visible screen {} pt", sr.h as i64, room as i64),
        );
        m.check("it stays on the display", vis.inset(-1.0, -1.0).contains_rect(&sr), format!("{sr:?} in {vis:?}"));
        m.check("it scrolls", sc.scrolls(), "");
        m.check("all 40 rows are there (none dropped)", sc.row_order().len() == 40, format!("{} rows", sc.row_order().len()));
        m.render.shot("scroll-layout-fullscreen", None, Some(scr));
        m.render.shot("scroll-top", Some(&["scheduled"]), None);
        IslandFile::patch("scheduled", |o| {
            if let Some(Value::Array(items)) = o.get_mut("items") {
                for k in 41..=60 {
                    items.push(serde_json::json!({"id": format!("s{k}"), "title": format!("{k}. Later item"), "when": "Next week"}));
                }
            }
        });
    });
    let m = l.clone();
    l.at(0.6, move || {
        let Some(sc) = c("scheduled") else { return };
        m.check(
            "rewritten to 60 items it keeps its height",
            sc.shape_rect().h == height_cap() && sc.row_order().len() == 60,
            format!("{} {}", sc.shape_rect().h, sc.row_order().len()),
        );
        let mut k = 0.0;
        let mut ev = |dy: f64, phase: i64, momentum: i64| {
            let at = k;
            k += 1.0 / 60.0;
            let c2 = sc.clone();
            sys::after(at, move || scroll(&c2, dy, phase, momentum));
        };
        ev(0.0, 1, 0);
        for _ in 0..10 {
            ev(-34.0, 2, 0);
        }
        ev(0.0, 4, 0);
        let mut v = -40.0;
        ev(v, 0, 1);
        for _ in 0..26 {
            v *= 0.9;
            ev(v, 0, 2);
        }
        ev(0.0, 0, 3);
        m.render.sheet("scroll-flick", &["scheduled"], None, &[0.0, 0.1, 0.2, 0.3, 0.45, 0.6, 0.8], Some(7));
        m.film("film-scroll", m.render.around(&["scheduled"], 20.0), 1.0);
    });
    let m = l.clone();
    l.at(1.4, move || {
        let Some(sc) = c("scheduled") else { return };
        let sy = sc.st.borrow().scroll_y;
        m.check("the flick scrolled with momentum past the fingers' travel", sy > 340.0 + 100.0, format!("scrollY {}", sy as i64));
        m.check("the indicator has faded at rest", sc.animated_layers().is_empty(), format!("{:?}", sc.animated_layers()));
        m.render.shot("scroll-middle", Some(&["scheduled"]), None);
        sc.set_scroll(sc.max_scroll() - 30.0);
        let mut k = 0.0;
        let mut ev = |dy: f64, phase: i64| {
            let at = k;
            k += 1.0 / 60.0;
            let c2 = sc.clone();
            sys::after(at, move || scroll(&c2, dy, phase, 0));
        };
        ev(0.0, 1);
        for _ in 0..12 {
            ev(-26.0, 2);
        }
        ev(0.0, 4);
        m.render.sheet("scroll-rubber-band", &["scheduled"], None, &[0.1, 0.2, 0.26, 0.32, 0.4, 0.55, 0.9], Some(7));
    });
    let m = l.clone();
    l.at(0.25, move || {
        let Some(sc) = c("scheduled") else { return };
        let sy = sc.st.borrow().scroll_y;
        m.check("pulled past the end it gives way", sy > sc.max_scroll() + 5.0, format!("scrollY {} max {}", sy as i64, sc.max_scroll() as i64));
    });
    let m = l.clone();
    l.at(1.3, move || {
        let Some(sc) = c("scheduled") else { return };
        let sy = sc.st.borrow().scroll_y;
        m.check(
            "let go, it springs back to the end",
            (sy - sc.max_scroll()).abs() < 0.5 && sc.animated_layers().is_empty(),
            format!("scrollY {} max {} {:?}", sy as i64, sc.max_scroll() as i64, sc.animated_layers()),
        );
        m.render.shot("scroll-bottom", Some(&["scheduled"]), None);
        sc.set_scroll(0.0);
        m.trace.borrow_mut().clear();
        let mut k = 0.0;
        let mut ev = |dy: f64, phase: i64, momentum: i64| {
            let at = k;
            k += 1.0 / 60.0;
            let (c2, m2) = (sc.clone(), m.clone());
            sys::after(at, move || {
                scroll(&c2, dy, phase, momentum);
                let y = c2.st.borrow().scroll_y;
                m2.trace.borrow_mut().push(y);
            });
        };
        ev(0.0, 1, 0);
        for _ in 0..10 {
            ev(-60.0, 2, 0);
        }
        ev(0.0, 4, 0);
        let mut v = -90.0;
        ev(v, 0, 1);
        for _ in 0..40 {
            v *= 0.95;
            ev(v, 0, 2);
        }
        ev(0.0, 0, 3);
        m.render.sheet("scroll-flick-into-end", &["scheduled"], None, &[0.3, 0.45, 0.55, 0.65, 0.8, 1.0, 1.3], Some(7));
    });
    let m = l.clone();
    l.at(1.6, move || {
        let Some(sc) = c("scheduled") else { return };
        let mx = sc.max_scroll();
        let t = m.trace.borrow();
        let flips = t.windows(2).filter(|w| (w[0] > mx + 0.5) != (w[1] > mx + 0.5)).count();
        let sy = sc.st.borrow().scroll_y;
        m.check(
            "a flick into the end overshoots once and settles (no shaking)",
            flips <= 2 && (sy - mx).abs() < 0.5 && sc.animated_layers().is_empty(),
            format!("{flips} edge crossings, scrollY {} of {}", sy as i64, mx as i64),
        );
        for i in 0..3 {
            let c2 = sc.clone();
            sys::after(i as f64 * 0.08, move || wheel(&c2, 3));
        }
    });
    let m = l.clone();
    l.at(0.8, move || {
        let Some(sc) = c("scheduled") else { return };
        let sy = sc.st.borrow().scroll_y;
        m.check(
            "a wheel scrolls up in eased steps",
            sy < sc.max_scroll() - 100.0 && sy > 0.0 && sc.animated_layers().is_empty(),
            format!("scrollY {} of {}", sy as i64, sc.max_scroll() as i64),
        );
        IslandFile::patch("scheduled", |o| {
            o.insert("expanded".into(), Value::Bool(false));
        });
    });
    l.at(0.8, || {
        IslandFile::patch("scheduled", |o| {
            o.insert("expanded".into(), Value::Bool(true));
        });
    });
    let m = l.clone();
    l.at(0.9, move || {
        let Some(sc) = c("scheduled") else { return };
        let sy = sc.st.borrow().scroll_y;
        m.check("reopened, it starts at the top", sy == 0.0, format!("scrollY {sy}"));
        let _ = std::fs::remove_file(Paths::file("scheduled"));
    });
    // 2. Confirm before an answer goes.
    l.at(0.8, || {
        put("checklist", jobj! {"preset": "todo", "expanded": true, "side": "left", "items": [
            {"id": "arsenal", "title": "Arsenal: push branch", "group": "running"},
            {"id": "caches", "title": "Delete old caches", "group": "waiting", "detail": LONG_DETAIL,
             "proposal": {"text": "~/.cache/huggingface  12.4 GB\n~/Library/Caches/pip  3.1 GB"}},
            {"id": "echo", "title": "Echo: Heimdall hears itself", "group": "discussed"}]});
    });
    let m = l.clone();
    l.at(0.8, move || {
        let Some(td) = c("checklist") else {
            m.check("todo shown", false, "");
            return;
        };
        // Closed, a row is its title only; opened, the whole explanation follows (titles-only rows, 23 Oct).
        let t = td.body_texts();
        let missing: Vec<&str> = crate::model::PLAN_SYMBOLS.iter().map(|p| p.1).filter(|s| !symbol_exists(s)).collect();
        m.check("every plan row symbol is a real SF Symbol", missing.is_empty(), format!("{missing:?}"));
        m.check("a closed row shows its title only", t.iter().any(|x| x == "Delete old caches") && !t.iter().any(|x| x.contains("Three caches")), format!("{t:?}"));
        m.click(&td, |c| matches!(c, Control::Disclose { key } if key == "caches"));
    });
    let m = l.clone();
    l.at(0.9, move || {
        let Some(td) = c("checklist") else { return };
        let t = td.body_texts();
        m.check("an opened row shows its whole explanation", t.iter().any(|x| x == LONG_DETAIL), format!("{t:?}"));
        m.render.shot("confirm-proposal-before", Some(&["checklist"]), None);
        let is_yes = |c: &Control| matches!(c, Control::Approve { key, action } if key == "caches" && action == "yes");
        let yes = td.control_centre(is_yes);
        m.click(&td, is_yes);
        m.check("a proposal's Yes asks first: nothing sent", td.is_confirming() && responses().is_empty(), format!("{:?}", responses()));
        m.render.sheet("confirm-proposal-in", &["checklist"], None, &[0.0, 0.04, 0.08, 0.12, 0.2], Some(5));
        let m2 = m.clone();
        sys::after(0.4, move || {
            m2.render.shot("confirm-proposal-sure", Some(&["checklist"]), None);
            if let Some(y) = yes {
                send(&td, NSEventType::LeftMouseDown, y);
                send(&td, NSEventType::LeftMouseUp, y);
            }
            let r = responses().into_iter().find(|r| j::str(r.get("kind")) == Some("approval"));
            m2.check(
                "a second click on the same button sends it",
                !td.is_confirming() && r.as_ref().is_some_and(|r| j::str(r.get("action")) == Some("yes") && j::str(r.get("value")) == Some("caches")),
                format!("{r:?}"),
            );
        });
    });
    let m = l.clone();
    l.at(1.0, move || {
        m.render.shot("confirm-proposal-sent", Some(&["checklist"]), None);
        m.clear_responses();
        let _ = std::fs::remove_file(Paths::file("checklist"));
        put("ask", jobj! {"preset": "waiting", "title": "Website concept", "state": "waiting", "expanded": true, "summary": "Which demo goes on the Arsenal site?",
            "parts": [{"type": "button", "label": "Photo to 3D", "action": "concept-3d"}, {"type": "button", "label": "Conversation", "action": "concept-talk"},
                      {"type": "button", "label": "Later", "action": "later"}]});
    });
    let m = l.clone();
    l.at(0.9, move || {
        let Some(a) = c("ask") else {
            m.check("ask shown", false, "");
            return;
        };
        m.render.shot("confirm-button-before", Some(&["ask"]), None);
        let b3d = |c: &Control| matches!(c, Control::Button { id, .. } if id == "concept-3d");
        m.click(&a, b3d);
        m.check("a button asks first: nothing sent", a.is_confirming() && responses().is_empty(), "");
        m.click(&a, b3d);
        m.check("a double-click does not confirm", a.is_confirming() && responses().is_empty(), "");
    });
    let m = l.clone();
    l.at(0.4, move || {
        let Some(a) = c("ask") else { return };
        let Some(p) = a.lab_confirm_centre(true) else {
            m.check("cancel found", false, "");
            return;
        };
        m.render.shot("confirm-button-sure", Some(&["ask"]), None);
        send(&a, NSEventType::LeftMouseDown, p);
        send(&a, NSEventType::LeftMouseUp, p);
        m.check("Cancel puts the buttons back and sends nothing", !a.is_confirming() && responses().is_empty(), "");
    });
    let m = l.clone();
    l.at(0.4, move || {
        let Some(a) = c("ask") else { return };
        m.render.shot("confirm-button-cancelled", Some(&["ask"]), None);
        m.click(&a, |c| matches!(c, Control::Button { id, .. } if id == "concept-talk"));
        let m2 = m.clone();
        sys::after(0.3, move || m2.render.shot("confirm-button-middle", Some(&["ask"]), None));
    });
    let m = l.clone();
    l.at(confirm::WINDOW + 0.4, move || {
        let Some(a) = c("ask") else { return };
        m.check(&format!("left alone, it lapses after {} s and sends nothing", confirm::WINDOW as i64), !a.is_confirming() && responses().is_empty(), "");
        m.click(&a, |c| matches!(c, Control::Button { id, .. } if id == "later"));
        let r = responses().into_iter().find(|r| j::str(r.get("kind")) == Some("button"));
        m.check("Later goes in one click", !a.is_confirming() && r.as_ref().is_some_and(|r| j::str(r.get("action")) == Some("later")), format!("{r:?}"));
        m.clear_responses();
        let _ = std::fs::remove_file(Paths::file("ask"));
        let md = Paths::root().join("concepts.md");
        let _ = std::fs::write(&md, "# Arsenal concepts\n\n1. **Photo to 3D**: one photo becomes a model you can turn.\n2. **Conversation**: a real recording, speakers and words in sync.\n3. **Receipt**: a photo of a receipt becomes a table.\n");
        put("view-concepts", jobj! {"preset": "viewer", "title": "Arsenal concepts", "path": md.to_string_lossy(), "expanded": true,
            "actions": [{"label": "Photo to 3D", "action": "concept-3d"}, {"label": "Conversation", "action": "concept-talk"}, {"label": "Receipt", "action": "concept-receipt"}]});
    });
    let m = l.clone();
    l.at(1.6, move || {
        let Some(v) = c("view-concepts") else {
            m.check("viewer actions shown", false, "");
            return;
        };
        let Some(row) = v.file_viewer().and_then(|f| f.action_row()) else {
            m.check("viewer actions shown", false, "");
            return;
        };
        m.viewer_shot("confirm-viewer-before", &v);
        let b = row.bounds();
        let p = row.to_window(crate::model::geometry::Point::new(b.size.width / 2.0, b.size.height / 2.0));
        let p = CGPoint::new(p.x, p.y);
        send(&v, NSEventType::LeftMouseDown, p);
        send(&v, NSEventType::LeftMouseUp, p);
        m.check("a viewer action asks first: nothing sent", row.is_confirming() && responses().is_empty(), format!("{:?}", responses()));
        let m2 = m.clone();
        sys::after(0.5, move || {
            m2.viewer_shot("confirm-viewer-sure", &v);
            send(&v, NSEventType::LeftMouseDown, p);
            send(&v, NSEventType::LeftMouseUp, p);
            let r = responses().into_iter().find(|r| j::str(r.get("kind")) == Some("button"));
            m2.check("a second click on it sends it", !row.is_confirming() && r.as_ref().is_some_and(|r| j::str(r.get("action")) == Some("concept-talk")), format!("{r:?}"));
            let m3 = m2.clone();
            sys::after(0.3, move || m3.viewer_shot("confirm-viewer-sent", &v));
        });
    });
    let m = l.clone();
    l.at(1.6, move || {
        let lines: Vec<String> = m
            .checks
            .borrow()
            .iter()
            .map(|(n, ok, d)| format!("{} {n}{}", if *ok { "ok  " } else { "FAIL" }, if d.is_empty() { String::new() } else { format!("  ({d})") }))
            .collect();
        let _ = std::fs::write(m.render.out.join("checks.txt"), lines.join("\n") + "\n");
        println!("{}", lines.join("\n"));
        for l in m.render.log.borrow().iter() {
            println!("{l}");
        }
        super::render::clear_folder();
        std::process::exit(if m.checks.borrow().iter().all(|c| c.1) { 0 } else { 1 });
    });
}

#[allow(dead_code)]
fn _unused(_: &AnyObject, _: &Json) {}
