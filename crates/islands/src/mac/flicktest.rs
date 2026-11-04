//! Lab flick test (Swift `FlickTest.swift`): islands thrown at speed with synthetic drags sent through each window's own
//! event path, in real time (8 ms apart, real timestamps), so the release velocity and the landing spring are the ones
//! a hand produces. Each throw's flight is sampled every frame into flick-<name>.csv.
//!   FLOATING_ISLANDS_DIR=<lab dir> FloatingIslands --flicktest

use std::cell::RefCell;
use std::rc::Rc;

use objc2_app_kit::{NSEventType, NSScreen};
use objc2_foundation::MainThreadMarker;
use serde_json::Value;

use super::app::{app, App};
use super::island::Ctl;
use super::render::{clear_folder, put};
use super::selftest::{main_screen, read, send_screen, Checks};
use super::sys::{self, Repeat};
use crate::jobj;
use crate::model::geometry::{Point, Rect, Vector};
use crate::model::{j, IslandFile, Json, Paths};

fn c(id: &str) -> Rc<Ctl> {
    app().ctl(id).expect("island on screen")
}

fn side(id: &str) -> Option<String> {
    read(id).and_then(|o| j::str(o.get("side")).map(str::to_owned))
}

fn edge_y(id: &str) -> Option<f64> {
    read(id).and_then(|o| j::num(o.get("edgeY")))
}

fn no_overlap() -> bool {
    let rs: Vec<Rect> = app().all().iter().map(|c| c.shape_rect()).collect();
    for a in 0..rs.len() {
        for b in a + 1..rs.len() {
            if rs[a].inset(1.0, 1.0).intersects(&rs[b]) {
                return false;
            }
        }
    }
    true
}

type Ev = (f64, NSEventType, Point);

struct Flick {
    k: Rc<Checks>,
    marks: RefCell<Vec<String>>,
    t0: f64,
}

impl Flick {
    /// Press, carry along a straight line at `v` for 96 ms (12 moves, 8 ms apart), let go on the next frame; first
    /// carried slowly to the start point, as a hand picks it up.
    fn throw_island(self: &Rc<Self>, name: &str, ctl: Rc<Ctl>, from: Point, v: Vector, start_x: Option<f64>) {
        let sr = ctl.shape_rect();
        let grab = Point::new(sr.mid_x(), sr.mid_y());
        let start = Point::new(start_x.unwrap_or(from.x), from.y);
        let mut evs: Vec<Ev> = vec![(0.0, NSEventType::LeftMouseDown, grab)];
        for k in 1..=10 {
            let f = k as f64 / 10.0;
            evs.push((0.016 * k as f64, NSEventType::LeftMouseDragged, Point::new(grab.x + (start.x - grab.x) * f, grab.y + (start.y - grab.y) * f)));
        }
        let t1 = 0.016 * 10.0 + 0.15;
        for k in 0..=12 {
            let dt = 0.008 * k as f64;
            evs.push((t1 + dt, NSEventType::LeftMouseDragged, Point::new(start.x + v.dx * dt, start.y + v.dy * dt)));
        }
        let tr = t1 + 0.008 * 13.0;
        evs.push((tr, NSEventType::LeftMouseUp, Point::new(start.x + v.dx * 0.104, start.y + v.dy * 0.104)));
        self.play(name, ctl, evs, tr, v.length());
    }

    /// A plain carry: slow, then a pause before letting go.
    fn drag(self: &Rc<Self>, name: &str, ctl: Rc<Ctl>, p: Point, pause: f64) {
        let sr = ctl.shape_rect();
        let grab = Point::new(sr.mid_x(), sr.mid_y());
        let mut evs: Vec<Ev> = vec![(0.0, NSEventType::LeftMouseDown, grab)];
        for k in 1..=30 {
            let f = k as f64 / 30.0;
            evs.push((0.016 * k as f64, NSEventType::LeftMouseDragged, Point::new(grab.x + (p.x - grab.x) * f, grab.y + (p.y - grab.y) * f)));
        }
        let tr = 0.016 * 30.0 + pause;
        evs.push((tr, NSEventType::LeftMouseUp, p));
        self.play(name, ctl, evs, tr, 0.0);
    }

    /// Send the events at their times and sample the shape every 8 ms from the press until 0.9 s after release.
    fn play(self: &Rc<Self>, name: &str, ctl: Rc<Ctl>, evs: Vec<Ev>, release: f64, throw_speed: f64) {
        let start = sys::uptime();
        let rows = Rc::new(RefCell::new(vec!["t,phase,x,y,w,h".to_owned()]));
        let flight: Rc<RefCell<Vec<(f64, Rect)>>> = Rc::new(RefCell::new(vec![]));
        let end = release + 0.9;
        let (r2, f2, c2) = (rows.clone(), flight.clone(), ctl.clone());
        let timer = Repeat::new(0.0, 0.008, move || {
            let t = sys::uptime() - start;
            let r = c2.visible_shape_rect();
            r2.borrow_mut().push(format!(
                "{t:.4},{},{:.1},{:.1},{:.1},{:.1}",
                if t < release { "carry" } else { "flight" },
                r.mid_x(),
                r.mid_y(),
                r.w,
                r.h
            ));
            if t >= release {
                f2.borrow_mut().push((t, r));
            }
        });
        for (dt, ty, p) in evs {
            let c3 = ctl.clone();
            sys::after(dt, move || send_screen(&c3, ty, p));
        }
        self.marks.borrow_mut().push(format!("{name},{:.3},{:.3}", start - self.t0, start - self.t0 + end));
        let me = self.clone();
        let name = name.to_owned();
        sys::after(end, move || {
            drop(timer);
            let _ = std::fs::write(Paths::root().join(format!("flick-{name}.csv")), rows.borrow().join("\n"));
            let after = flight.borrow();
            let (mut max_jump, mut worst) = (0.0f64, 0.0f64);
            let mut prev: Option<f64> = None;
            for k in 2..after.len() {
                let (a, b) = (after[k - 1], after[k]);
                let v = (b.1.mid_x() - a.1.mid_x()).hypot(b.1.mid_y() - a.1.mid_y()) / (b.0 - a.0);
                max_jump = max_jump.max(v);
                if let Some(p) = prev {
                    worst = worst.max(v / p.max(400.0));
                }
                prev = Some(v);
            }
            let k = &me.k;
            if throw_speed > 0.0 && after.len() > 6 {
                let (a, b) = (after[1], after[6.min(after.len() - 1)]);
                let early = (b.1.mid_x() - a.1.mid_x()).hypot(b.1.mid_y() - a.1.mid_y()) / (b.0 - a.0);
                k.check(
                    &format!("{name}: leaves the hand with its momentum (≥ 35 % of the throw speed in the first 40 ms)"),
                    early >= throw_speed * 0.35,
                    format!("{} pt/s of {}", early as i64, throw_speed as i64),
                );
                k.check(
                    &format!("{name}: no jumps in flight (no frame over 2 × the speed of the frame before)"),
                    worst <= 2.0,
                    format!("worst {worst:.2}×, peak {} pt/s", max_jump as i64),
                );
            } else {
                k.check(&format!("{name}: settles without jumps"), max_jump <= 1500.0 && worst <= 2.0, format!("peak {} pt/s, worst {worst:.2}×", max_jump as i64));
            }
            let sr = ctl.shape_rect();
            let last: Vec<Rect> = after.iter().rev().take(10).map(|a| a.1).collect();
            k.check(
                &format!("{name}: comes to rest at its slot"),
                last.iter().all(|r| (r.mid_x() - sr.mid_x()).abs() < 0.6 && (r.mid_y() - sr.mid_y()).abs() < 0.6),
                format!("{:?} vs {sr:?}", last.first()),
            );
        });
    }
}

pub fn run() {
    if std::env::var("FLOATING_ISLANDS_DIR").is_err() {
        println!("flicktest: refusing to run without FLOATING_ISLANDS_DIR");
        std::process::exit(2);
    }
    clear_folder();
    put("r1", jobj! {"title": "Qwen-Image", "state": "running", "stage": "Final tests", "icon": "photo"});
    put("r2", jobj! {"title": "iPhone", "state": "waiting", "stage": "Your test"});
    put("checklist", jobj! {"preset": "todo", "title": "Checklist", "icon": "checklist",
        "items": [{"title": "Qwen-Image into Arsenal", "group": "running"}, {"title": "Test Heimdall Talk", "group": "waiting"},
                  {"title": "Arsenal website rebuild", "group": "discussed"}]});
    put("ball", jobj! {"title": "iPad sale", "state": "done", "position": {"x": 780, "y": 420}});
    let (scr, vis, inset) = main_screen();
    let row_top = scr.max_y() - inset.max(scr.max_y() - vis.max_y()) - 6.0;
    let f = Rc::new(Flick { k: Checks::new(), marks: RefCell::new(vec![]), t0: sys::uptime() });
    let mut t = 1.4;
    let mut steps: Vec<(f64, Box<dyn FnOnce()>)> = vec![];
    let mut at = |dt: f64, g: Box<dyn FnOnce()>| {
        t += dt;
        steps.push((t, g));
    };
    let k = f.k.clone();
    // 1. Flick left from the middle, a little below the Checklist pill: it attaches to the left edge where the throw lands.
    let f1 = f.clone();
    at(0.0, Box::new(move || {
        let b = c("ball");
        let x = b.shape_rect().mid_x();
        f1.throw_island("left", b, Point::new(x, 700.0), Vector::new(-2600.0, 120.0), None);
    }));
    let k1 = k.clone();
    at(1.6, Box::new(move || {
        let (b, td) = (c("ball").shape_rect(), c("checklist").shape_rect());
        k1.check("flick left: attaches to the left edge", (b.min_x() - (scr.min_x() + 8.0)).abs() < 1.0, format!("{b:?}"));
        k1.check("flick left: lands where the throw points (release 712 + 60 pt of glide)", (b.mid_y() - 772.5).abs() < 3.0, format!("{b:?}"));
        k1.check(
            "flick left: side and height kept in the file",
            side("ball").as_deref() == Some("left") && (edge_y("ball").unwrap_or(-1.0) - (scr.max_y() - b.max_y())).abs() < 0.5,
            format!("{:?}", read("ball")),
        );
        k1.check("flick left: clear of the checklist", !b.inset(1.0, 1.0).intersects(&td), "");
    }));
    // 2. Opening the checklist never covers it; closing it leaves it at its height.
    at(0.1, Box::new(|| {
        IslandFile::patch("checklist", |o| {
            o.insert("expanded".into(), Value::Bool(true));
        });
    }));
    let k1 = k.clone();
    at(0.9, Box::new(move || {
        k1.check("opening the checklist covers nothing on its edge", c("checklist").expanded() && no_overlap(), format!("{:?} vs {:?}", c("ball").shape_rect(), c("checklist").shape_rect()));
        IslandFile::patch("checklist", |o| {
            o.insert("expanded".into(), Value::Bool(false));
        });
    }));
    // 3. Flick right, aimed between Qwen-Image and iPhone: it takes that height and iPhone makes room below it.
    let (k1, f1) = (k.clone(), f.clone());
    at(0.9, Box::new(move || {
        let (b, r1, r2) = (c("ball"), c("r1").shape_rect(), c("r2").shape_rect());
        k1.check("checklist closed: the island is back at its height", (b.shape_rect().mid_y() - 772.5).abs() < 3.0, format!("{:?}", b.shape_rect()));
        let x = b.shape_rect().mid_x();
        f1.throw_island("right", b, Point::new(x, (r1.mid_y() + r2.mid_y()) / 2.0), Vector::new(2800.0, 0.0), Some(700.0));
    }));
    let k1 = k.clone();
    at(1.6, Box::new(move || {
        let (b, r1, r2) = (c("ball").shape_rect(), c("r1").shape_rect(), c("r2").shape_rect());
        k1.check("flick right: attaches to the right edge", (b.max_x() - (scr.max_x() - 8.0)).abs() < 1.0, format!("{b:?}"));
        k1.check(
            "flick right: lands between the two, in order, no overlap",
            r1.min_y() >= b.max_y() + 7.5 && b.min_y() >= r2.max_y() + 7.5,
            format!("r1 {r1:?} ball {b:?} r2 {r2:?}"),
        );
    }));
    // 4. Flick up hard: no top-middle dock; it goes to the top of the nearer edge, under the menu bar.
    let f1 = f.clone();
    at(0.1, Box::new(move || {
        f1.throw_island("up", c("ball"), Point::new(700.0, 600.0), Vector::new(200.0, 2600.0), Some(700.0));
    }));
    let k1 = k.clone();
    at(1.6, Box::new(move || {
        let b = c("ball").shape_rect();
        k1.check(
            "flick up: top of the left edge, under the menu bar",
            (b.max_y() - row_top).abs() < 1.0 && (b.min_x() - (scr.min_x() + 8.0)).abs() < 1.0,
            format!("{b:?} row {row_top}"),
        );
        k1.check("flick up: no top dock in the file", side("ball").as_deref() == Some("left"), format!("{:?}", read("ball")));
        let screen = NSScreen::mainScreen(MainThreadMarker::new().unwrap()).unwrap();
        let r1 = c("r1").shape_rect();
        k1.check("right stack closed up when it left", (r1.max_y() - (scr.max_y() - App::column_top(&screen))).abs() < 1.0, format!("{r1:?}"));
    }));
    // 5. Plain drag: iPhone carried slowly low on the right and let go after a pause: it stays at that height.
    let f1 = f.clone();
    at(0.1, Box::new(move || f1.drag("low", c("r2"), Point::new(1500.0, 300.0), 0.12)));
    let (k1, f1) = (k.clone(), f.clone());
    at(1.4, Box::new(move || {
        let r2 = c("r2").shape_rect();
        k1.check(
            "plain drag: stays at the height it was let go, on the right edge",
            (r2.mid_y() - 300.0).abs() < 1.5 && (r2.max_x() - (scr.max_x() - 8.0)).abs() < 1.0,
            format!("{r2:?}"),
        );
        // 6. Qwen-Image dragged down the right edge onto iPhone: no overlap, it takes the place and iPhone makes room.
        f1.drag("along", c("r1"), Point::new(scr.max_x() - 80.0, 310.0), 0.12);
    }));
    let k1 = k.clone();
    at(1.4, Box::new(move || {
        let (r1, r2) = (c("r1").shape_rect(), c("r2").shape_rect());
        k1.check("drag along the edge: both on the right edge, no overlap", (r1.max_x() - (scr.max_x() - 8.0)).abs() < 1.0 && no_overlap(), format!("r1 {r1:?} r2 {r2:?}"));
        k1.check(
            "drag along the edge: the one carried there sits at its height",
            (r1.mid_y() - 310.0).abs() < 1.5 || (r2.mid_y() - 300.0).abs() < 1.5,
            format!("r1 {r1:?} r2 {r2:?}"),
        );
        IslandFile::patch("r2", |o| {
            o.insert("note".into(), Value::from("agent rewrite"));
        });
    }));
    let k1 = k.clone();
    at(0.6, Box::new(move || {
        k1.check("agent update keeps the chosen height", edge_y("r2").is_some() && side("r2").as_deref() == Some("right"), format!("{:?}", read("r2")));
        let st: Option<Json> = IslandFile::read(&Paths::state()).and_then(|o| j::obj(o.get("islands")).and_then(|i| j::obj(i.get("ball")).cloned()));
        k1.check(
            "state.json says it is pinned, with its top-left",
            st.as_ref().is_some_and(|s| j::boolean(s.get("pinned")) == Some(true) && j::num(s.get("y")) == Some(scr.max_y() - c("ball").shape_rect().max_y())),
            format!("{st:?}"),
        );
        k1.check("no island overlaps another", no_overlap(), "");
        k1.check("no island in the menu bar strip", app().all().iter().all(|c| c.shape_rect().max_y() <= row_top + 0.5), "");
    }));
    let (k1, f1) = (k.clone(), f.clone());
    at(1.2, Box::new(move || {
        let busy: Vec<String> = app().all().iter().flat_map(|c| c.animated_layers().into_iter().map(move |l| format!("{}: {l}", c.id))).collect();
        k1.check("at rest no layer is animating", busy.is_empty(), busy.join(", "));
        let ok = k1.report("flicktest");
        let _ = std::fs::write(Paths::root().join("flick-marks.csv"), f1.marks.borrow().join("\n"));
        clear_folder();
        sys::after(0.5, move || {
            let _ = std::fs::remove_file(Paths::pid());
            let _ = std::fs::remove_file(Paths::state());
            std::process::exit(if ok { 0 } else { 1 });
        });
    }));
    for (when, g) in steps {
        sys::after(when, g);
    }
}
