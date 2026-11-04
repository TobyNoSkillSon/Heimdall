//! Lab self-test (Swift `SelfTest.swift`): real islands, real windows, mouse events sent through each window's own
//! event path (NSWindow.sendEvent → hit test → view). The pointer, keyboard and screen are never driven.
//!   FLOATING_ISLANDS_DIR=<lab dir> FloatingIslands --selftest

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use objc2::rc::Retained;
use objc2::{msg_send, ClassType};
use objc2_app_kit::{NSEvent, NSEventModifierFlags, NSEventType, NSScreen, NSScrollView, NSTextView, NSView};
use objc2_core_foundation::CGPoint;
use objc2_foundation::{MainThreadMarker, NSPoint, NSRect};
use serde_json::Value;

use super::app::{app, App};
use super::island::{notch_rect, Ctl, LAB_PULSE};
use super::look::PILL_H;
use super::parts::Control;
use super::render::{clear_folder, put};
use super::sys;
use crate::jobj;
use crate::model::geometry::{Point, Rect};
use crate::model::{j, time, Attention, Island, IslandFile, Json, Paths};

fn mtm() -> MainThreadMarker {
    MainThreadMarker::new().unwrap()
}

fn rect_from(r: NSRect) -> Rect {
    Rect::new(r.origin.x, r.origin.y, r.size.width, r.size.height)
}

pub fn main_screen() -> (Rect, Rect, f64) {
    let s = NSScreen::mainScreen(mtm()).expect("a screen");
    (rect_from(s.frame()), rect_from(s.visibleFrame()), s.safeAreaInsets().top)
}

pub fn read(id: &str) -> Option<Json> {
    IslandFile::read(&Paths::file(id))
}

pub fn responses() -> Vec<Json> {
    std::fs::read_dir(Paths::responses())
        .map(|d| d.flatten().filter_map(|e| IslandFile::read(&e.path())).collect())
        .unwrap_or_default()
}

/// A mouse event at a window point, sent through the window's own event path.
pub fn send(c: &Ctl, ty: NSEventType, p: CGPoint) {
    let e = NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
        ty,
        p,
        NSEventModifierFlags::empty(),
        sys::uptime(),
        c.panel.windowNumber(),
        None,
        0,
        1,
        if ty == NSEventType::LeftMouseUp { 0.0 } else { 1.0 },
    );
    if let Some(e) = e {
        c.panel.sendEvent(&e);
    }
}

/// The same at a screen point (y up).
pub fn send_screen(c: &Ctl, ty: NSEventType, p: Point) {
    let wp = c.panel.convertPointFromScreen(NSPoint::new(p.x, p.y));
    send(c, ty, wp);
}

/// The open viewer's live root view (not hidden) inside the island's window.
pub fn viewer_root(c: &Ctl) -> Option<Retained<NSView>> {
    c.file_viewer()?.root().filter(|r| !r.isHidden())
}

pub fn viewer_scroll(c: &Ctl) -> Option<Retained<NSScrollView>> {
    let root = viewer_root(c)?;
    root.subviews().iter().find(|v| {
        let k: bool = unsafe { msg_send![&**v, isKindOfClass: NSScrollView::class()] };
        k
    }).map(|v| unsafe { Retained::cast_unchecked::<NSScrollView>(v) })
}

pub fn viewer_text(c: &Ctl) -> Option<String> {
    let sv = viewer_scroll(c)?;
    let d = sv.documentView()?;
    let is_text: bool = unsafe { msg_send![&*d, isKindOfClass: NSTextView::class()] };
    if !is_text {
        return None;
    }
    let tv: Retained<NSTextView> = unsafe { Retained::cast_unchecked(d) };
    Some(tv.string().to_string())
}

pub struct Checks {
    pub results: RefCell<Vec<(String, bool, String)>>,
    pub pending: Cell<i32>,
}

impl Checks {
    pub fn new() -> Rc<Checks> {
        Rc::new(Checks { results: RefCell::new(vec![]), pending: Cell::new(0) })
    }

    pub fn check(&self, name: &str, ok: bool, detail: impl Into<String>) {
        self.results.borrow_mut().push((name.to_owned(), ok, detail.into()));
    }

    /// A check on something that follows a file change or a timer: polled every 20 ms until it holds or `timeout`.
    pub fn eventually(self: &Rc<Self>, name: &str, timeout: f64, cond: impl Fn() -> bool + 'static, detail: impl Fn() -> String + 'static) {
        self.pending.set(self.pending.get() + 1);
        let start = sys::uptime();
        let me = self.clone();
        let name = name.to_owned();
        let cond = Rc::new(cond);
        let detail = Rc::new(detail);
        fn poll(me: Rc<Checks>, name: String, start: f64, timeout: f64, cond: Rc<dyn Fn() -> bool>, detail: Rc<dyn Fn() -> String>) {
            if cond() {
                me.check(&name, true, "");
                me.pending.set(me.pending.get() - 1);
                return;
            }
            if sys::uptime() - start > timeout {
                me.check(&name, false, detail());
                me.pending.set(me.pending.get() - 1);
                return;
            }
            sys::after(0.02, move || poll(me, name, start, timeout, cond, detail));
        }
        poll(me, name, start, timeout, cond, detail);
    }

    /// Polls `cond` for up to `timeout` s, then runs `then` with whether it held (finish waits for it).
    pub fn wait_for(self: &Rc<Self>, timeout: f64, cond: impl Fn() -> bool + 'static, then: impl FnOnce(bool) + 'static) {
        self.pending.set(self.pending.get() + 1);
        let start = sys::uptime();
        type Then = Rc<RefCell<Option<Box<dyn FnOnce(bool)>>>>;
        fn poll(me: Rc<Checks>, start: f64, timeout: f64, cond: Rc<dyn Fn() -> bool>, then: Then) {
            let ok = cond();
            if ok || sys::uptime() - start > timeout {
                me.pending.set(me.pending.get() - 1);
                if let Some(t) = then.borrow_mut().take() {
                    t(ok);
                }
                return;
            }
            sys::after(0.02, move || poll(me, start, timeout, cond, then));
        }
        poll(self.clone(), start, timeout, Rc::new(cond), Rc::new(RefCell::new(Some(Box::new(then)))));
    }

    /// Prints the results; true when all passed.
    pub fn report(&self, label: &str) -> bool {
        let r = self.results.borrow();
        let failed = r.iter().filter(|r| !r.1).count();
        for (n, ok, d) in r.iter() {
            println!("{}  {n}{}", if *ok { "PASS" } else { "FAIL" }, if d.is_empty() || *ok { String::new() } else { format!("  ({d})") });
        }
        println!("{label}: {}/{} passed", r.len() - failed, r.len());
        failed == 0
    }
}

fn after(s: f64, f: impl FnOnce() + 'static) {
    sys::after(s, f);
}

fn c(id: &str) -> Option<Rc<Ctl>> {
    app().ctl(id)
}

fn click(k: &Rc<Checks>, ctl: &Ctl, m: impl Fn(&Control) -> bool) {
    let Some(p) = ctl.control_centre(m) else {
        k.check(&format!("control found on {}", ctl.id), false, "");
        return;
    };
    send(ctl, NSEventType::LeftMouseDown, p);
    send(ctl, NSEventType::LeftMouseUp, p);
}

fn menu_bottom() -> f64 {
    let (scr, vis, inset) = main_screen();
    scr.max_y() - inset.max(scr.max_y() - vis.max_y())
}

type Pulses = Rc<RefCell<HashMap<String, Vec<(f64, Vec<String>)>>>>;

pub fn run() {
    if std::env::var("FLOATING_ISLANDS_DIR").is_err() {
        println!("selftest: refusing to run without FLOATING_ISLANDS_DIR (it rewrites the data folder)");
        std::process::exit(2);
    }
    let k = Checks::new();
    if std::env::var("FI_SELFTEST_ONLY").as_deref() == Ok("refresh") {
        clear_folder();
        refresh_steps(k, 0.5);
        return;
    }
    if std::env::var("FI_SELFTEST_ONLY").as_deref() == Ok("goals") {
        clear_folder();
        // As in the full run: Goals is already on the dock, closed, with an ask, before the steps open it by its file.
        put("goals", jobj! {"preset": "todo", "list": "goals", "title": "Goals", "icon": "flag", "items": [{"id": "g-w", "title": "Pick a release date"}],
            "goals": [{"id": "g-ship", "title": "Ship Heimdall publicly", "tasks": [{"title": "Opening film", "state": "done"}]}]});
        goals_steps(k, 2.0, Box::new(finish));
        return;
    }
    std::env::set_var("FI_OPEN_LOG", Paths::root().join("opened.log"));
    let _ = std::fs::remove_file(Paths::root().join("opened.log"));
    clear_folder();
    put("st-wait", jobj! {"preset": "waiting", "title": "Publish?", "expanded": true, "position": {"x": 354, "y": 180},
        "parts": [{"type": "text", "value": "Publish the preview build?"},
                  {"type": "button", "label": "Publish", "action": "publish"},
                  {"type": "button", "label": "Not now", "action": "later"}]});
    put("st-list", jobj! {"preset": "list", "title": "Quotes", "expanded": true, "position": {"x": 684, "y": 180},
        "parts": [{"type": "list", "items": [{"id": "a", "title": "Quote A", "value": "4 199"},
                                             {"id": "b", "title": "Quote B", "value": "3 870"},
                                             {"id": "c", "title": "Quote C", "value": "4 420"}]}]});
    put("st-note", jobj! {"preset": "note", "title": "Settings", "expanded": true, "position": {"x": 1014, "y": 180},
        "parts": [{"type": "toggle", "id": "voice", "label": "Voice", "on": false},
                  {"type": "slider", "id": "ctx", "min": 0, "max": 100, "step": 1, "value": 20, "unit": "k"}]});
    put("st-drag", jobj! {"preset": "task", "title": "Build", "progress": 0.4});

    let k1 = k.clone();
    after(1.2, move || {
        let k = k1;
        let a = app();
        k.check("four islands on screen", a.controllers.borrow().len() == 4 && a.all().iter().all(|c| c.panel.isVisible()), format!("{}", a.controllers.borrow().len()));
        let w = c("st-wait").unwrap();
        k.check("pinned island opens", w.expanded(), "");
        click(&k, &w, |c| matches!(c, Control::Button { id, .. } if id == "publish"));
        w.lab_confirm(true);
        let l = c("st-list").unwrap();
        click(&k, &l, |c| matches!(c, Control::ListRow { id, .. } if id == "b"));
        l.lab_confirm(true);
        let n = c("st-note").unwrap();
        click(&k, &n, |c| matches!(c, Control::Toggle { .. }));
        if let Some(p) = n.control_centre(|c| matches!(c, Control::Slider { .. })) {
            send(&n, NSEventType::LeftMouseDown, CGPoint::new(p.x - 40.0, p.y));
            send(&n, NSEventType::LeftMouseDragged, CGPoint::new(p.x + 20.0, p.y));
            send(&n, NSEventType::LeftMouseUp, CGPoint::new(p.x + 60.0, p.y));
        }
    });
    let k1 = k.clone();
    after(1.6, move || {
        let k = k1;
        let rs = responses();
        let has = |f: &dyn Fn(&Json) -> bool| rs.iter().any(|r| f(r));
        let s = |r: &Json, key: &str| j::str(r.get(key)).map(str::to_owned);
        k.check("button writes its response", has(&|r| s(r, "island").as_deref() == Some("st-wait") && s(r, "action").as_deref() == Some("publish")), "");
        k.check("button choice kept in the file", read("st-wait").and_then(|o| s(&o, "chosen")).as_deref() == Some("publish"), "");
        k.check("list row writes its response", has(&|r| s(r, "island").as_deref() == Some("st-list") && s(r, "action").as_deref() == Some("b")), "");
        let sel = read("st-list").and_then(|o| j::objs(o.get("parts")).and_then(|p| p.first().and_then(|p| s(p, "selected"))));
        k.check("list selection kept in the file", sel.as_deref() == Some("b"), sel.unwrap_or("nil".into()));
        k.check(
            "toggle writes its response",
            has(&|r| s(r, "island").as_deref() == Some("st-note") && s(r, "kind").as_deref() == Some("toggle") && r.get("value") == Some(&Value::Bool(true))),
            "",
        );
        let sv = rs.iter().find(|r| s(r, "kind").as_deref() == Some("slider")).and_then(|r| j::num(r.get("value")));
        k.check("slider writes its value", sv.is_some_and(|v| v > 20.0), format!("{}", sv.unwrap_or(-1.0)));

        // Drag the task island to just right of the middle and let go: it attaches to the right edge at that height.
        let d = c("st-drag").unwrap();
        let (scr, _, _) = main_screen();
        let sr = d.shape_rect();
        let start = Point::new(sr.mid_x(), sr.mid_y());
        let drop = Point::new(scr.mid_x() + 137.0, scr.mid_y() - 61.0);
        send_screen(&d, NSEventType::LeftMouseDown, start);
        for i in 1..=5 {
            let f = i as f64 / 5.0;
            send_screen(&d, NSEventType::LeftMouseDragged, Point::new(start.x + (drop.x - start.x) * f, start.y + (drop.y - start.y) * f));
        }
        send_screen(&d, NSEventType::LeftMouseUp, drop);
        let sr = d.shape_rect();
        let o = read("st-drag").unwrap_or_default();
        let ey = j::num(o.get("edgeY")).unwrap_or(f64::NAN);
        k.check(
            "drop attaches to the nearer edge at the height it was let go",
            (sr.max_x() - (scr.max_x() - 8.0)).abs() < 1.0 && (sr.mid_y() - drop.y).abs() < 1.5,
            format!("{sr:?}"),
        );
        k.check(
            "drop stores the edge and the height",
            j::str(o.get("side")) == Some("right") && (ey - (scr.max_y() - sr.max_y())).abs() < 0.5 && o.get("position").is_none(),
            format!("{o:?}"),
        );
        // Drop it in the top right corner, partly over the menu bar: it stays on the right edge, just under the menu bar.
        let n = c("st-note").unwrap();
        let nr = n.shape_rect();
        let ns = Point::new(nr.mid_x(), nr.max_y() - 14.0);
        let corner = Point::new(scr.max_x() - nr.w / 2.0 - 20.0, scr.max_y() - 10.0);
        send_screen(&n, NSEventType::LeftMouseDown, ns);
        send_screen(&n, NSEventType::LeftMouseDragged, Point::new((ns.x + corner.x) / 2.0, (ns.y + corner.y) / 2.0));
        send_screen(&n, NSEventType::LeftMouseDragged, corner);
        send_screen(&n, NSEventType::LeftMouseUp, corner);
        let mb = menu_bottom();
        let r = n.shape_rect();
        k.check("stops under the menu bar", r.max_y() <= mb - 5.5 && r.max_y() >= mb - 6.5, format!("top {} menu bottom {mb}", r.max_y()));
        k.check("attaches to the right edge, not the middle", (r.max_x() - (scr.max_x() - 8.0)).abs() < 1.0, format!("{r:?}"));
        let no = read("st-note").unwrap_or_default();
        k.check("the edge is stored", j::str(no.get("side")) == Some("right") && no.get("edgeY").is_some(), format!("{no:?}"));
        k.check("an open island stays open through drag and drop", n.expanded() && n.island().expanded(), "");
        let es = n.expanded_size();
        k.check("drop keeps its size", r.w == es.w && r.h == es.h, format!("{:?} vs {:?}", r.size(), es));
    });
    let k1 = k.clone();
    after(2.3, move || k1.check("an answered question leaves", read("st-wait").is_none(), ""));
    let k1 = k.clone();
    after(2.4, move || {
        let k = k1;
        let d = c("st-drag").unwrap();
        let sr = d.shape_rect();
        let p = Point::new(sr.mid_x(), sr.max_y() - 14.0);
        send_screen(&d, NSEventType::LeftMouseDown, p);
        send_screen(&d, NSEventType::LeftMouseUp, p);
        k.check("click pins the island open", read("st-drag").and_then(|o| j::boolean(o.get("expanded"))) == Some(true) && d.expanded(), "");
        let o = IslandFile::merge(&read("st-drag").unwrap(), &jobj! {"progress": 0.9});
        put("st-drag", o);
        let _ = std::fs::remove_file(Paths::file("st-list"));
    });
    let k1 = k.clone();
    after(3.0, move || {
        let k = k1;
        k.check("file update reaches the island", c("st-drag").and_then(|c| c.island().progress()) == Some(0.9), "");
        k.check("deleting the file removes the island", c("st-list").is_none(), "");
        put("st-drag", IslandFile::merge(&read("st-drag").unwrap(), &jobj! {"position": null, "edgeY": null, "side": null}));
    });
    after(3.3, move || {
        let md = Paths::root().join("selftest-view.md");
        let _ = std::fs::write(&md, "# Viewer\n\nA **markdown** file with a list:\n\n- one\n- two\n\n```\ncode\n```\n");
        put("st-view", jobj! {"preset": "viewer", "title": "Viewer", "path": md.to_string_lossy(), "expanded": true,
            "actions": [{"label": "Do all of it", "action": "do-all"}, {"label": "Not now", "action": "later"}]});
    });
    let k1 = k.clone();
    after(4.4, move || {
        let k = k1;
        let v = c("st-view");
        let (scr, _, _) = main_screen();
        k.check(
            "viewer opens in the middle of the screen",
            v.as_ref().is_some_and(|v| (v.shape_rect().mid_x() - scr.mid_x()).abs() < 2.0 && v.shape_rect().w > 400.0),
            format!("{:?}", v.as_ref().map(|v| v.shape_rect())),
        );
        k.check("viewer shows its text in a scroll view", v.as_ref().and_then(|v| viewer_scroll(v)).is_some(), "");
        if let Some(v) = &v {
            let fv = v.file_viewer();
            if let Some(p) = fv.as_ref().and_then(|f| f.lab_action_point(0.25)) {
                send(v, NSEventType::LeftMouseDown, p);
                send(v, NSEventType::LeftMouseUp, p);
                let asked = fv.as_ref().is_some_and(|f| f.lab_action_confirming());
                k.check("viewer action asks before it sends", asked && !responses().iter().any(|r| j::str(r.get("island")) == Some("st-view")), "");
                if let Some(f) = &fv {
                    f.lab_action_confirm(true);
                }
                k.check(
                    "viewer action writes a button response",
                    responses().iter().any(|r| {
                        j::str(r.get("island")) == Some("st-view") && j::str(r.get("kind")) == Some("button") && j::str(r.get("action")) == Some("do-all")
                    }),
                    "",
                );
            } else {
                k.check("viewer shows its proposed actions", false, "");
            }
            // Carried by its title row: it moves, stays open, keeps its size, and remembers the place.
            let before = v.shape_rect();
            let grab = Point::new(before.mid_x() - 60.0, before.max_y() - 18.0);
            send_screen(v, NSEventType::LeftMouseDown, grab);
            for i in 1..=4 {
                send_screen(v, NSEventType::LeftMouseDragged, Point::new(grab.x + i as f64 * 30.0, grab.y - i as f64 * 15.0));
            }
            send_screen(v, NSEventType::LeftMouseUp, Point::new(grab.x + 120.0, grab.y - 60.0));
            let moved = v.shape_rect();
            k.check(
                "viewer carried by its title row",
                (moved.min_x() - before.min_x() - 120.0).abs() < 2.0
                    && (moved.min_y() - before.min_y() + 60.0).abs() < 2.0
                    && moved.size() == before.size()
                    && v.expanded(),
                format!("{before:?} → {moved:?}"),
            );
            k.check("viewer place is stored", read("st-view").is_some_and(|o| o.contains_key("openPosition")), "");
            // Its drag handle clicked: the file on the lab's own pasteboard, the viewer still open and in place.
            match v.shown_share() {
                Some(h) => {
                    super::share::write_file(&super::share::pasteboard(), std::path::Path::new("/dev/null"));
                    let f = h.frame_in_view(v.shape.frame());
                    let at = v.view.convertPoint_toView(CGPoint::new(f.mid_x(), f.mid_y()), None);
                    let sp = v.panel.convertPointToScreen(at);
                    let p = Point::new(sp.x, sp.y);
                    let before = v.shape_rect();
                    send_screen(v, NSEventType::LeftMouseDown, p);
                    send_screen(v, NSEventType::LeftMouseUp, p);
                    let got = super::share::read_files(&super::share::pasteboard());
                    k.check(
                        "viewer handle click copies its file",
                        got.len() == 1 && got[0].ends_with("selftest-view.md") && v.expanded() && v.shape_rect() == before,
                        format!("{got:?}"),
                    );
                }
                None => k.check("viewer handle click copies its file", false, "no handle"),
            }
            Ctl::close_viewer(v);
            k.check(
                "Esc closes the viewer to its pill",
                read("st-view").and_then(|o| j::boolean(o.get("expanded"))) == Some(false) && !v.expanded(),
                "",
            );
        }
    });
    let k1 = k.clone();
    after(5.2, move || {
        let v = c("st-view");
        k1.check("minimised viewer is a pill under the menu bar", v.as_ref().is_some_and(|v| v.shape_rect().h == PILL_H), format!("{:?}", v.map(|v| v.shape_rect())));
        if let (Some(v), Some(o)) = (c("st-view"), IslandFile::patch("st-view", |o| {
            o.insert("expanded".into(), Value::Bool(true));
        })) {
            v.update(Island::new("st-view", o));
        }
    });
    let k1 = k.clone();
    after(5.9, move || {
        if let Some(v) = c("st-view") {
            if let Some(x) = v.close_centre() {
                send(&v, NSEventType::LeftMouseDown, x);
                send(&v, NSEventType::LeftMouseUp, x);
            } else {
                k1.check("close button shown on an open island", false, "");
            }
        }
    });
    let k1 = k.clone();
    after(6.4, move || {
        let k = k1;
        k.check("X closes an island for good", read("st-view").is_none() && c("st-view").is_none(), "");
        k.check("the agent hears about the close", responses().iter().any(|r| j::str(r.get("island")) == Some("st-view") && j::str(r.get("kind")) == Some("closed")), "");
        if let Some(n) = c("st-note") {
            Ctl::close_island(&n);
        }
    });
    let k1 = k.clone();
    after(6.9, move || {
        k1.check("right-click Close removes an island", read("st-note").is_none(), "");
        let _ = std::fs::remove_file(Paths::root().join("selftest-view.md"));
    });
    let k1 = k.clone();
    after(7.3, move || {
        let k = k1;
        let d = c("st-drag").unwrap();
        let (scr, _, _) = main_screen();
        let screen = NSScreen::mainScreen(mtm()).unwrap();
        let col_top = scr.max_y() - App::column_top(&screen);
        let mb = menu_bottom();
        k.check(
            "clearing the position returns it to the automatic column at the right edge, below browser chrome",
            d.island().position().is_none() && (d.anchor().y - col_top).abs() < 1.0 && (d.shape_rect().max_x() - (scr.max_x() - 8.0)).abs() < 1.0,
            format!("{:?} {:?}", d.anchor(), d.shape_rect()),
        );
        k.check("no island in the menu bar strip", app().all().iter().all(|c| c.shape_rect().max_y() <= mb), "");
        let page = Paths::root().join("compare3.html");
        let _ = std::fs::write(&page, "<h1>compare</h1>");
        put("st-bare", jobj! {"preset": "task", "title": "Male voices", "state": "running", "stage": "Searching", "icon": "waveform",
            "started": time::now() - 725.0, "position": {"x": 480, "y": 400}});
        put("st-done", jobj! {"preset": "task", "title": "Compare", "state": "done", "open": page.to_string_lossy(), "position": {"x": 860, "y": 400}});
        put("st-gone", jobj! {"preset": "result", "title": "Old", "open": "/nonexistent/old.html", "expanded": true, "position": {"x": 1150, "y": 400}});
    });
    after(8.1, move || {
        for id in ["st-bare", "st-done"] {
            let b = c(id).unwrap();
            let sr = b.shape_rect();
            let p = Point::new(sr.mid_x(), sr.mid_y());
            send_screen(&b, NSEventType::LeftMouseDown, p);
            send_screen(&b, NSEventType::LeftMouseUp, p);
        }
    });
    let k1 = k.clone();
    after(8.8, move || {
        let k = k1;
        let b = c("st-bare").unwrap();
        k.check("a click opens a bare task (title and stage only)", b.expanded() && b.shape_rect().h > PILL_H + 20.0, format!("{:?}", b.shape_rect()));
        let texts = b.body_texts();
        k.check("open bare task says its state and stage", texts.iter().any(|t| t == "Running · Searching"), format!("{texts:?}"));
        k.check("open bare task says how long it has run", texts.iter().any(|t| t.starts_with("Started 12 min ago")), format!("{texts:?}"));
        k.check("open bare task has its X", b.close_centre().is_some(), "");
        let dn = c("st-done").unwrap();
        k.check("a click opens a done island", dn.expanded(), "");
        let dt = dn.body_texts();
        k.check("done island shows its open target as a button", dt.iter().any(|t| t == "Open compare3.html"), format!("{dt:?}"));
        k.check("done island says when it finished", dt.iter().any(|t| t.starts_with("Finished just now")), format!("{dt:?}"));
        click(&k, &dn, |c| matches!(c, Control::Open { .. }));
        let g = c("st-gone").unwrap();
        let gt = g.body_texts();
        k.check(
            "a missing file says so instead of a dead button",
            gt.iter().any(|t| t == "Not found: old.html") && g.control_centre(|c| matches!(c, Control::Open { .. })).is_none(),
            format!("{gt:?}"),
        );
        IslandFile::patch("st-bare", |o| {
            o.insert("summary".into(), Value::from("Ten male voices from the library, ranked by clarity"));
            o.insert("steps".into(), serde_json::json!([{"title": "Collect", "state": "done"}, {"title": "Rank", "state": "now"}, {"title": "Write up", "state": "todo"}]));
            o.insert("kv".into(), serde_json::json!([{"label": "Voices", "value": "10"}, {"label": "Source", "value": "Library"}]));
        });
    });
    let k1 = k.clone();
    after(9.4, move || {
        let k = k1;
        let log = std::fs::read_to_string(Paths::root().join("opened.log")).unwrap_or_default();
        k.check("the open button opens the file in the default app", log.contains("compare3.html") && !log.contains("\tnone"), log.trim().to_owned());
        let t = c("st-bare").unwrap().body_texts();
        k.check(
            "details arrive in the open island",
            t.iter().any(|x| x == "Rank") && t.iter().any(|x| x == "Library") && t.iter().any(|x| x.starts_with("Ten male voices")),
            format!("{t:?}"),
        );
    });
    after(9.5, move || {
        let wav = Paths::root().join("selftest-tone.wav");
        write_tone(&wav);
        put("st-audio", jobj! {"preset": "viewer", "title": "Tone", "path": wav.to_string_lossy(), "expanded": true});
    });
    let k1 = k.clone();
    after(10.3, move || {
        let k = k1;
        let Some(v) = c("st-audio") else {
            k.check("audio viewer shows its player", false, "");
            return;
        };
        let Some(fv) = v.file_viewer() else {
            k.check("audio viewer shows its player", false, "");
            return;
        };
        // The live player takes over from the opening still once the shape has landed: wait for it (a busy Mac
        // lands later).
        let (k0, v0, fv0) = (k.clone(), v.clone(), fv.clone());
        k.wait_for(2.0, move || viewer_root(&v0).is_some() && fv0.lab_audio_button().is_some(), move |ok| {
        let (k, v, fv) = (k0, v, fv);
        let Some(play) = fv.lab_audio_button().filter(|_| ok) else {
            k.check("audio viewer shows its player", false, "");
            return;
        };
        fv.lab_audio_mute();
        send(&v, NSEventType::LeftMouseDown, play);
        send(&v, NSEventType::LeftMouseUp, play);
        k.check("audio plays on a click", fv.lab_audio_playing(), "");
        if let Some(mid) = fv.lab_audio_wave_point(0.5) {
            send(&v, NSEventType::LeftMouseDown, mid);
            send(&v, NSEventType::LeftMouseUp, mid);
        }
        let k2 = k.clone();
        after(0.15, move || {
            let s = fv.lab_audio_seconds();
            k2.check("a click on the waveform scrubs", (s - 1.0).abs() < 0.25, format!("{s}"));
            Ctl::close_viewer(&v);
            k2.check("closing the viewer pauses the audio", !fv.lab_audio_playing(), "");
            let _ = std::fs::remove_file(Paths::file("st-audio"));
            let _ = std::fs::remove_file(Paths::root().join("selftest-tone.wav"));
        });
        });
    });
    let pulses: Pulses = Rc::new(RefCell::new(HashMap::new()));
    let (k1, p1) = (k.clone(), pulses.clone());
    after(11.0, move || {
        let (k, pulses) = (k1, p1);
        let (k2, p2) = (k.clone(), pulses.clone());
        LAB_PULSE.with(|f| {
            *f.borrow_mut() = Some(Box::new(move |c: &Rc<Ctl>| {
                p2.borrow_mut().entry(c.id.clone()).or_default().push((sys::uptime(), c.animated_layers()));
                if c.id == "st-bell" && p2.borrow().get("st-bell").map(|v| v.len()) == Some(1) {
                    let k3 = k2.clone();
                    after(Attention::INTERVAL / 2.0, move || {
                        let between: Vec<String> = app()
                            .all()
                            .iter()
                            .filter(|c| c.pulse_count() > 0)
                            // Closed Goals takes one breath when the due Scheduled item arrives on it in this window (a
                            // closed list that changes breathes once: news, not a loop).
                            .flat_map(|c| c.animated_layers().into_iter().filter(|l| !l.ends_with(".work")).filter(move |l| !(c.id == "goals" && l.ends_with(".breath"))).map(move |l| format!("{}: {l}", c.id)))
                            .collect();
                        k3.check("between attention pulses nothing animates", between.is_empty(), between.join(", "));
                        let b = app().ctl("st-bell");
                        k3.check(
                            "attention stays set while the key is",
                            b.as_ref().is_some_and(|b| b.attention_shown().as_deref() == Some("waiting") && b.pulse_count() == 1),
                            "",
                        );
                    });
                }
            }));
        });
        let now = time::now();
        put("checklist", jobj! {"preset": "todo", "title": "Checklist", "expanded": true, "items": [
            {"id": "a1", "title": "Arsenal: push branch", "group": "running"},
            {"id": "a2", "title": "Qwen-Image into Arsenal", "group": "running"},
            {"id": "caches", "title": "Delete old caches", "group": "waiting", "proposal": {"text": "~/.cache/huggingface 12 GB\n~/Library/Caches 4 GB"}},
            {"id": "echo", "title": "Echo: Heimdall hears itself", "group": "discussed"}]});
        put("scheduled", jobj! {"preset": "todo", "list": "scheduled", "title": "Scheduled", "expanded": true, "items": [
            {"id": "menu", "title": "Heimdall: menu revamp", "when": "Tonight"},
            {"id": "tap", "title": "Arsenal: tap", "due": now + 150.0},
            {"id": "due-now", "title": "Review the night pass", "due": now + 3.0, "when": "Soon", "detail": "kept"}]});
        put("settled", jobj! {"preset": "todo", "list": "settled", "title": "Settled", "items": [{"id": "s1", "title": "Night pass stage 1"}]});
        put("goals", jobj! {"preset": "todo", "list": "goals", "title": "Goals", "icon": "flag", "items": [{"id": "g-w", "title": "Pick a release date"}],
            "goals": [{"id": "g-ship", "title": "Ship Heimdall publicly", "tasks": [{"title": "Opening film", "state": "done"}]}]});
        // The retired To do: never drawn, its file left alone (Heimdall migrates it).
        put("todo", jobj! {"preset": "todo", "list": "todo", "title": "To do", "items": [{"id": "old", "title": "Old item", "group": "waiting"}]});
        put("st-q", jobj! {"title": "Port", "state": "waiting", "attention": "question", "position": {"x": 900, "y": 640},
            "parts": [{"type": "button", "label": "Ship", "action": "t3q:r1:0"}]});
        put("st-bell", jobj! {"title": "Build", "state": "running", "attention": "waiting", "position": {"x": 1100, "y": 640}});
        put("st-approvals", jobj! {"preset": "todo", "title": "Approvals", "position": {"x": 700, "y": 640},
            "items": [{"id": "w", "title": "Publish preview", "group": "waiting"}]});
        put("st-notch", jobj! {"title": "New job", "state": "running", "stage": "Starting", "origin": {"from": "notch"}});
    });
    let k1 = k.clone();
    after(11.9, move || {
        let k = k1;
        let (scr, _, _) = main_screen();
        let (Some(td), Some(sc), Some(se)) = (c("checklist"), c("scheduled"), c("settled")) else {
            k.check("plan islands on screen", false, format!("{:?}", app().controllers.borrow().keys().collect::<Vec<_>>()));
            return;
        };
        let (tr, sr, er) = (td.shape_rect(), sc.shape_rect(), se.shape_rect());
        k.check("plan islands dock left", [tr, sr, er].iter().all(|r| (r.min_x() - (scr.min_x() + 8.0)).abs() < 1.0), format!("{tr:?} {sr:?} {er:?}"));
        k.check("plan islands in order Checklist, Scheduled, Settled", tr.min_y() > sr.max_y() && sr.min_y() > er.max_y(), "");
        k.check("the retired To do is not drawn and its file stays", c("todo").is_none() && read("todo").is_some(), "");
        let gl = c("goals");
        k.check(
            "Goals docks first",
            gl.as_ref().is_some_and(|g| g.shape_rect().min_y() > tr.max_y() && (g.shape_rect().min_x() - (scr.min_x() + 8.0)).abs() < 1.0),
            format!("{:?} {tr:?}", gl.as_ref().map(|g| g.shape_rect())),
        );
        k.check("an ask on closed Goals rings the bell", gl.as_ref().is_some_and(|g| g.attention_shown().as_deref() == Some("waiting")), "");
        k.check("Settled is a pill", !se.expanded() && er.h == PILL_H, format!("{er:?}"));
        let t = td.body_texts();
        // Groups are set apart by hairlines keyed `hdr:<group>` (the first group needs none), not by header text.
        k.check(
            "a checklist shows its groups",
            td.row_frame("hdr:waiting").is_some() && td.row_frame("hdr:discussed").is_some() && !t.iter().any(|x| x == "RUNNING"),
            format!("{t:?}"),
        );
        let st = sc.body_texts();
        k.check("Scheduled shows when", st.iter().any(|x| x == "Tonight"), format!("{st:?}"));
        k.check("Scheduled counts down to a due time", st.iter().any(|x| x == "3 min"), format!("{st:?}"));
        k.eventually(
            "a due Scheduled item stays, first, reading Now, and the app hears it",
            4.0,
            || {
                let moved = read("goals").and_then(|o| j::objs(o.get("items")).map(|v| v.iter().any(|i| j::str(i.get("id")) == Some("due-now")))).unwrap_or(false);
                let still = read("scheduled").and_then(|o| j::objs(o.get("items")).map(|v| v.iter().any(|i| j::str(i.get("id")) == Some("due-now")))).unwrap_or(false);
                let sc = c("scheduled");
                still
                    && !moved
                    && sc.as_ref().is_some_and(|s| s.row_order().first().map(String::as_str) == Some("due-now") && s.body_texts().iter().any(|x| x == "Now"))
                    && responses().iter().any(|r| {
                        j::str(r.get("kind")) == Some("due") && j::str(r.get("value")) == Some("due-now") && j::str(r.get("island")) == Some("scheduled")
                    })
            },
            || format!("{:?} / {:?}", read("goals").and_then(|o| o.get("items").cloned()), read("scheduled").and_then(|o| o.get("items").cloned())),
        );
        let (q, bell, ap) = (c("st-q").unwrap(), c("st-bell").unwrap(), c("st-approvals").unwrap());
        k.check("question attention shown", q.attention_shown().as_deref() == Some("question"), format!("{:?}", q.attention_shown()));
        k.check("waiting attention shown", bell.attention_shown().as_deref() == Some("waiting"), "");
        k.check("a plan island with an open waiting item asks for attention", ap.attention_shown().as_deref() == Some("waiting"), "");
        k.check("an open plan island does not pulse", td.attention_shown().is_none(), "");
        click(&k, &td, |c| matches!(c, Control::Tick { key, .. } if key == "a1"));
        let (k2, td2) = (k.clone(), td.clone());
        sys::soon(move || {
            let a = td2.animated_layers();
            k2.check("a tick starts the sink animation", a.iter().any(|l| l.contains("slide")), format!("{a:?}"));
        });
    });
    let (k1, p1) = (k.clone(), pulses.clone());
    after(12.3, move || {
        let (k, pulses) = (k1, p1);
        let td = c("checklist").unwrap();
        let p2 = pulses.clone();
        k.eventually(
            "attention pulses: glow and symbol",
            3.0,
            move || {
                p2.borrow().get("st-q").and_then(|v| v.first().cloned()).is_some_and(|p| {
                    p.1.iter().any(|l| l.contains("attnGlow")) && p.1.iter().any(|l| l.contains("attnRing"))
                })
            },
            move || format!("{:?}", pulses.borrow().get("st-q")),
        );
        let r = responses().into_iter().find(|r| j::str(r.get("kind")) == Some("tick"));
        k.check(
            "tick writes a tick response with the item id",
            r.as_ref().is_some_and(|r| j::str(r.get("island")) == Some("checklist") && j::str(r.get("value")) == Some("a1") && j::str(r.get("action")) == Some("done")),
            format!("{r:?}"),
        );
        let a1 = read("checklist").and_then(|o| j::objs(o.get("items")).and_then(|v| v.into_iter().find(|i| j::str(i.get("id")) == Some("a1")).cloned()));
        k.check("tick updates the file", a1.as_ref().and_then(|a| j::boolean(a.get("done"))) == Some(true), format!("{a1:?}"));
        let order = td.row_order();
        k.check("a ticked item sinks to the bottom of its group", order.iter().take(2).cloned().collect::<Vec<_>>() == ["a2", "a1"], format!("{order:?}"));
        let n = c("st-notch").unwrap();
        let notch = notch_rect(NSScreen::mainScreen(mtm()).as_deref());
        k.check("notch origin: grows out of the notch", n.born_from() == Some(notch) && notch.w > 0.0, format!("{:?} vs {notch:?}", n.born_from()));
        k.eventually("notch origin: the key is dropped", 3.0, || read("st-notch").is_some_and(|o| !o.contains_key("origin")), || format!("{:?}", read("st-notch")));
        let (scr, _, _) = main_screen();
        k.check("notch origin: lands in its slot", (n.shape_rect().max_x() - (scr.max_x() - 8.0)).abs() < 1.0, format!("{:?}", n.shape_rect()));
        IslandFile::patch("checklist", |o| {
            if let Some(Value::Array(items)) = o.get_mut("items") {
                items.push(serde_json::json!({"id": "n1", "title": "Phonon voices as presets", "group": "discussed"}));
            }
        });
        IslandFile::patch("st-q", |o| {
            o.remove("attention");
        });
    });
    let k1 = k.clone();
    after(12.6, move || {
        let k = k1;
        let td = c("checklist").unwrap();
        let td2 = td.clone();
        k.eventually(
            "an added item arrives with a highlight",
            3.0,
            move || td.row_order().iter().any(|r| r == "n1") && td.animated_layers().iter().any(|l| l.contains("highlight")),
            move || format!("{:?} {:?}", td2.animated_layers(), td2.row_order()),
        );
        k.eventually("removing the attention key clears it", 3.0, || c("st-q").is_some_and(|q| q.attention_shown().is_none()), String::new);
        let td = c("checklist").unwrap();
        click(&k, &td, |c| matches!(c, Control::Disclose { key } if key == "caches"));
        let ap = c("st-approvals").unwrap();
        let r = ap.shape_rect();
        let p = Point::new(r.mid_x(), r.mid_y());
        send_screen(&ap, NSEventType::LeftMouseDown, p);
        send_screen(&ap, NSEventType::LeftMouseUp, p);
    });
    let k1 = k.clone();
    after(13.2, move || {
        let k = k1;
        let td = c("checklist").unwrap();
        let t = td.body_texts();
        k.check(
            "an approval opens to its proposal",
            t.iter().any(|x| x.contains("12 GB")) && t.iter().any(|x| x == "Yes") && t.iter().any(|x| x == "Later"),
            format!("{t:?}"),
        );
        let ap = c("st-approvals");
        k.check("opening the island clears its attention", ap.is_some_and(|a| a.attention_shown().is_none() && a.expanded()), "");
        click(&k, &td, |c| matches!(c, Control::Approve { key, action } if key == "caches" && action == "yes"));
        k.check("an approval asks before it sends", td.is_confirming() && !responses().iter().any(|r| j::str(r.get("kind")) == Some("approval")), "");
        td.lab_confirm(true);
        let r = responses().into_iter().find(|r| j::str(r.get("kind")) == Some("approval"));
        k.check(
            "approval writes its response",
            r.as_ref().is_some_and(|r| j::str(r.get("island")) == Some("checklist") && j::str(r.get("action")) == Some("yes") && j::str(r.get("value")) == Some("caches")),
            format!("{r:?}"),
        );
        let it = read("checklist").and_then(|o| j::objs(o.get("items")).and_then(|v| v.into_iter().find(|i| j::str(i.get("id")) == Some("caches")).cloned()));
        k.check("the choice is kept on the item", it.as_ref().and_then(|i| j::str(i.get("chosen"))) == Some("yes"), "");
        k.check("the row shows the choice", td.control_centre(|c| matches!(c, Control::Approve { .. })).is_none(), "");
        IslandFile::patch("checklist", |o| {
            if let Some(Value::Array(items)) = o.get_mut("items") {
                for i in items.iter_mut() {
                    if i.get("id").and_then(Value::as_str) == Some("echo") {
                        i["group"] = Value::from("running");
                    }
                }
            }
        });
    });
    let k1 = k.clone();
    after(13.5, move || {
        let td = c("checklist").unwrap();
        let o = td.row_order();
        let pos = |id: &str| o.iter().position(|x| x == id);
        k1.check("a moved item slides to its new group", pos("echo").is_some_and(|e| e < pos("a1").unwrap_or(0)), format!("{o:?}"));
        IslandFile::patch("checklist", |o| {
            if let Some(Value::Array(items)) = o.get_mut("items") {
                items.retain(|i| i.get("id").and_then(Value::as_str) != Some("n1"));
            }
        });
    });
    let k1 = k.clone();
    after(15.0, move || {
        let k = k1;
        let td = c("checklist").unwrap();
        let t = td.body_texts();
        k.check("after the choice the proposal closes", td.row_frame("prop:caches").is_none() && t.iter().any(|x| x == "Yes"), format!("{t:?}"));
        k.check("a removed item is gone", !td.row_order().iter().any(|r| r == "n1"), format!("{:?}", td.row_order()));
        let (sr, es) = (td.shape_rect(), td.expanded_size());
        k.check("the open list's shape fits its rows", (sr.h - es.h).abs() < 1.0, format!("{sr:?} {es:?}"));
    });
    let k1 = k.clone();
    after(16.9, move || {
        let busy: Vec<String> = app()
            .all()
            .iter()
            .flat_map(|c| {
                let att = c.attention_shown().is_some();
                c.animated_layers().into_iter().filter(move |l| !(att && l.contains("attn")) && !l.ends_with(".work")).map(move |l| format!("{}: {l}", c.id))
            })
            .collect();
        k1.check("at rest no layer is animating", busy.is_empty(), busy.join(", "));
    });
    let (k1, p1) = (k.clone(), pulses.clone());
    after(17.4, move || {
        let p2 = p1.clone();
        k1.eventually(
            "the next attention pulse comes 6 s later",
            4.0,
            move || {
                let b = p2.borrow();
                let Some(p) = b.get("st-bell") else { return false };
                p.len() >= 2 && (p[1].0 - p[0].0 - Attention::INTERVAL).abs() < 0.6 && p[1].1.iter().any(|l| l.contains("attnGlow"))
            },
            move || format!("{:?}", p1.borrow().get("st-bell").map(|v| v.iter().map(|p| p.0).collect::<Vec<_>>())),
        );
    });
    goals_steps(k, 18.5, Box::new(|k| refresh_steps(k, 0.3)));
}

/// Goals (2 Nov, the one system): a goal with its queued line and its front step proposal as a ghost row; a click on
/// the ghost's title shows its why; Yes answers in one click (no "Sure?": the approval response at once); the file then
/// carrying the yes (the proposal's id now a task) fills the ghost in where it sat. `t0`: when they start
/// (FI_SELFTEST_ONLY=goals runs them alone, then `then`).
fn goals_steps(k: Rc<Checks>, t0: f64, then: Box<dyn FnOnce(Rc<Checks>)>) {
    let at = move |t: f64, f: Box<dyn FnOnce()>| sys::after(t0 + t, f);
    let goal = |tasks: serde_json::Value, proposals: serde_json::Value| {
        jobj! {"preset": "todo", "list": "goals", "title": "Goals", "icon": "flag", "expanded": true, "items": [],
            "goals": [{"id": "g-ship", "title": "Ship Heimdall publicly", "now": "Opening film nearly final", "tasks": tasks}],
            "proposals": proposals}
    };
    at(0.0, Box::new(move || {
        put("goals", goal(serde_json::json!([{"id": "t1", "title": "Opening film", "state": "done"}, {"id": "t2", "title": "Privacy scan", "state": "next"}]),
            serde_json::json!([{"id": "p-sign", "title": "Signing", "kind": "step", "goal": "g-ship", "why": "User: sign it first", "shown": time::now()},
                               {"id": "p-two", "title": "Updater", "kind": "step", "goal": "g-ship"}])));
    }));
    // Opened by its file (as an agent's write opens it); the steps run once it is open, the open spring settled.
    let k0 = k.clone();
    at(0.1, Box::new(move || {
        let k = k0.clone();
        k0.wait_for(4.0, || c("goals").is_some_and(|g| g.expanded() && g.row_frame("goal:g-ship").is_some()), move |ok| {
            k.check("Goals opens when its file says so", ok, format!("{:?}", read("goals").map(|o| o.get("expanded").cloned())));
            let k1 = k.clone();
            sys::after(0.5, Box::new(move || {
                let k = k1;
                let Some(g) = c("goals").filter(|g| g.expanded()) else {
                    k.check("Goals open on screen", false, format!("{:?}", app().controllers.borrow().keys().collect::<Vec<_>>()));
                    return;
                };
                let t = g.body_texts();
                let (head, queued, ghost) = (g.row_frame("goal:g-ship"), g.row_frame("row:t2"), g.row_frame("row:p-sign"));
                k.check(
                    "a goal, its queued line, then its front step proposal as a ghost with +1",
                    t.iter().any(|x| x == "Signing") && t.iter().any(|x| x == "+1") && !t.iter().any(|x| x == "Updater")
                        && matches!((head, queued, ghost), (Some(h), Some(q), Some(p)) if h.max_y() <= q.min_y() && q.max_y() <= p.min_y()),
                    format!("{head:?} {queued:?} {ghost:?} {t:?}"),
                );
                click(&k, &g, |c| matches!(c, Control::Disclose { key } if key == "why:p-sign"));
                k.check("a click on the ghost's title shows its why", g.body_texts().iter().any(|x| x == "User: sign it first"), "");
                click(&k, &g, |c| matches!(c, Control::Yes { list, id } if list == "goals" && id == "p-sign"));
                let r = responses().into_iter().find(|r| j::str(r.get("kind")) == Some("approval") && j::str(r.get("island")) == Some("goals"));
                k.check(
                    "Yes answers in one click",
                    !g.is_confirming() && r.as_ref().is_some_and(|r| j::str(r.get("action")) == Some("yes") && j::str(r.get("value")) == Some("p-sign")),
                    format!("{r:?}"),
                );
                k.check("an answered ghost shows no Yes", g.control_centre(|c| matches!(c, Control::Yes { .. })).is_none(), "");
            }));
            let k1 = k.clone();
            sys::after(1.2, Box::new(move || {
                let before = c("goals").and_then(|g| g.row_frame("row:p-sign"));
                put("goals", goal(serde_json::json!([{"id": "t1", "title": "Opening film", "state": "done"}, {"id": "t2", "title": "Privacy scan", "state": "next"},
                                                     {"id": "p-sign", "title": "Signing", "state": "next", "yes": {"by": "click", "at": time::now()}}]),
                    serde_json::json!([{"id": "p-two", "title": "Updater", "kind": "step", "goal": "g-ship", "shown": time::now()}])));
                let k = k1.clone();
                sys::after(0.8, Box::new(move || {
                    let after = c("goals").and_then(|g| g.row_frame("row:p-sign"));
                    k.check(
                        "the yes fills the ghost in where it sat (its id is the task's)",
                        matches!((before, after), (Some(b), Some(a)) if (a.min_y() - b.min_y()).abs() < 1.0),
                        format!("{before:?} {after:?}"),
                    );
                }));
            }));
            let k1 = k.clone();
            sys::after(3.5, Box::new(move || {
                let busy: Vec<String> = c("goals").map(|g| g.animated_layers()).unwrap_or_default();
                k1.check("Goals at rest: nothing animates", busy.is_empty(), busy.join(", "));
                then(k1);
            }));
        });
    }));
}

/// Viewer refresh (bugs 22 Oct 20:45 and 20:46): an open viewer follows its file, keeps its scroll position, and
/// reads the file anew when opened again. `t0`: when the steps start (FI_SELFTEST_ONLY=refresh runs them alone).
fn refresh_steps(k: Rc<Checks>, t0: f64) {
    let after = |t: f64, f: Box<dyn FnOnce()>| sys::after(t - 18.5 + t0, f);
    let refresh = Paths::root().join("selftest-refresh.md");
    let lines = |tag: &str| format!("# {tag}\n\n{}", (1..=120).map(|i| format!("Line {i} of {tag}.")).collect::<Vec<_>>().join("\n\n"));
    let (rf, l1) = (refresh.clone(), lines("First"));
    after(18.5, Box::new(move || {
        let _ = std::fs::write(&rf, l1);
        put("st-refresh", jobj! {"preset": "viewer", "title": "Refresh", "path": rf.to_string_lossy(), "expanded": true});
    }));
    let (k1, rf, l2) = (k.clone(), refresh.clone(), lines("Second"));
    after(19.4, Box::new(move || {
        let k = k1.clone();
        k1.wait_for(
            3.0,
            || c("st-refresh").and_then(|v| viewer_text(&v)).is_some_and(|t| t.starts_with("First")),
            move |ok| {
                k.check("refresh viewer shows its text", ok, c("st-refresh").and_then(|v| viewer_text(&v)).map(|t| t.chars().take(40).collect::<String>()).unwrap_or("nil".into()));
                let Some(v) = c("st-refresh") else { return };
                let Some(sv) = viewer_scroll(&v).filter(|_| ok) else { return };
                let cv = sv.contentView();
                cv.scrollToPoint(CGPoint::new(0.0, 400.0));
                sv.reflectScrolledClipView(&cv);
                super::island::debug("selftest: write Second by rename");
                write_by_rename(&rf, &l2);
                let v2 = v.clone();
                k.eventually(
                    "an open viewer re-renders when its file is replaced",
                    2.0,
                    move || viewer_text(&v).is_some_and(|t| t.starts_with("Second")),
                    move || viewer_text(&v2).map(|t| t.chars().take(40).collect::<String>()).unwrap_or("nil".into()),
                );
            },
        );
    }));
    // Then each step waits for the one before (the file watch's debounce and a busy machine move the reloads).
    let (k1, rf, l3, l4) = (k.clone(), refresh.clone(), lines("Third"), lines("Fourth"));
    after(21.0, Box::new(move || {
        let Some(v) = c("st-refresh") else { return };
        let y = viewer_scroll(&v).map(|s| s.contentView().bounds().origin.y).unwrap_or(-1.0);
        k1.check("a re-render keeps the scroll position", (y - 400.0).abs() < 2.0, format!("{y}"));
        // Written in place (same inode).
        if let Ok(mut f) = std::fs::OpenOptions::new().write(true).truncate(true).open(&rf) {
            use std::io::Write;
            let _ = f.write_all(l3.as_bytes());
        }
        let (v2, k2) = (v.clone(), k1.clone());
        k1.wait_for(2.0, move || viewer_text(&v2).is_some_and(|t| t.starts_with("Third")), move |ok| {
            k2.check(
                "an open viewer re-renders when its file is written in place",
                ok,
                viewer_text(&v).map(|t| t.chars().take(40).collect::<String>()).unwrap_or("nil".into()),
            );
            let k3 = k2.clone();
            sys::after(0.4, move || {
                let Some(v) = c("st-refresh") else { return };
                Ctl::close_viewer(&v);
                write_by_rename(&rf, &l4);
                sys::after(0.5, move || {
                    if let Some(o) = IslandFile::patch("st-refresh", |o| {
                        o.insert("expanded".into(), Value::Bool(true));
                        o.insert("updated".into(), j::number(time::now()));
                    }) {
                        v.update(Island::new("st-refresh", o));
                    }
                    let v2 = v.clone();
                    let k4 = k3.clone();
                    k3.wait_for(3.0, move || viewer_text(&v2).is_some_and(|t| t.starts_with("Fourth")), move |ok| {
                        k4.check(
                            "viewing the same path again reads the file anew",
                            ok,
                            viewer_text(&v).map(|t| t.chars().take(40).collect::<String>()).unwrap_or("nil".into()),
                        );
                        Ctl::close_viewer(&v);
                        let _ = std::fs::remove_file(&rf);
                        finish(k4);
                    });
                });
            });
        });
    }));
}

/// Write a file the way editors and agents save: a temp file renamed over it.
pub fn write_by_rename(path: &std::path::Path, text: &str) {
    let tmp = path.with_extension("tmp-save");
    let _ = std::fs::write(&tmp, text);
    let _ = std::fs::rename(&tmp, path);
}

fn finish(k: Rc<Checks>) {
    if k.pending.get() > 0 {
        after(0.1, move || finish(k));
        return;
    }
    if std::env::var("FI_SELFTEST_ONLY").is_err() {
    k.check(
        "a cleared attention never pulses again",
        c("st-q").map(|c| c.pulse_count()) == Some(1) && c("st-approvals").map(|c| c.pulse_count()) == Some(1),
        format!("{:?} {:?}", c("st-q").map(|c| c.pulse_count()), c("st-approvals").map(|c| c.pulse_count())),
    );
    }
    let ok = k.report("selftest");
    clear_folder();
    for f in ["compare3.html", "opened.log"] {
        let _ = std::fs::remove_file(Paths::root().join(f));
    }
    after(0.5, move || {
        let _ = std::fs::remove_file(Paths::pid());
        let _ = std::fs::remove_file(Paths::state());
        std::process::exit(if ok { 0 } else { 1 });
    });
}

/// A 2 s 22.05 kHz mono tone as a 16-bit WAV (played muted).
pub fn write_tone(path: &std::path::Path) {
    let n = 44_100u32;
    let mut d: Vec<u8> = Vec::with_capacity(44 + 2 * n as usize);
    let push32 = |d: &mut Vec<u8>, v: u32| d.extend_from_slice(&v.to_le_bytes());
    d.extend_from_slice(b"RIFF");
    push32(&mut d, 36 + 2 * n);
    d.extend_from_slice(b"WAVEfmt ");
    push32(&mut d, 16);
    d.extend_from_slice(&1u16.to_le_bytes());
    d.extend_from_slice(&1u16.to_le_bytes());
    push32(&mut d, 22_050);
    push32(&mut d, 44_100);
    d.extend_from_slice(&2u16.to_le_bytes());
    d.extend_from_slice(&16u16.to_le_bytes());
    d.extend_from_slice(b"data");
    push32(&mut d, 2 * n);
    for i in 0..n {
        let v = ((i as f64 * 0.06).sin() * 0.3 * 32767.0) as i16;
        d.extend_from_slice(&v.to_le_bytes());
    }
    let _ = std::fs::write(path, d);
}

/// The self-test's view of a viewer's controls (the action row and the audio player), in window coordinates.
pub trait LabViewer {
    fn lab_action_point(&self, fraction_x: f64) -> Option<CGPoint>;
    fn lab_action_confirming(&self) -> bool;
    fn lab_action_confirm(&self, yes: bool);
    fn lab_audio_button(&self) -> Option<CGPoint>;
    fn lab_audio_wave_point(&self, fraction: f64) -> Option<CGPoint>;
    fn lab_audio_mute(&self);
    fn lab_audio_playing(&self) -> bool;
    fn lab_audio_seconds(&self) -> f64;
}

impl LabViewer for super::viewer::FileViewer {
    fn lab_action_point(&self, fraction_x: f64) -> Option<CGPoint> {
        let row = self.action_row()?;
        let b = row.bounds();
        let p = row.to_window(Point::new(b.size.width * fraction_x, b.size.height / 2.0));
        Some(CGPoint::new(p.x, p.y))
    }
    fn lab_action_confirming(&self) -> bool {
        self.action_row().is_some_and(|r| r.is_confirming())
    }
    fn lab_action_confirm(&self, yes: bool) {
        if let Some(r) = self.action_row() {
            r.confirm(yes);
        }
    }
    fn lab_audio_button(&self) -> Option<CGPoint> {
        let p = self.audio_player()?.button_centre_in_window();
        Some(CGPoint::new(p.x, p.y))
    }
    fn lab_audio_wave_point(&self, fraction: f64) -> Option<CGPoint> {
        let p = self.audio_player()?.wave_point_in_window(fraction);
        Some(CGPoint::new(p.x, p.y))
    }
    fn lab_audio_mute(&self) {
        if let Some(a) = self.audio_player() {
            a.mute();
        }
    }
    fn lab_audio_playing(&self) -> bool {
        self.audio_player().is_some_and(|a| a.is_playing())
    }
    fn lab_audio_seconds(&self) -> f64 {
        self.audio_player().map(|a| a.seconds()).unwrap_or(-1.0)
    }
}
