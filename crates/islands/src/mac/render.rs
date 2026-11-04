//! Lab renders (Swift `Render.swift`): islands built offscreen (no window is ever ordered on screen), their layer trees
//! drawn into PNGs at 2× on a dark desktop-like backdrop; animations as contact sheets of presentation frames.
//!   FLOATING_ISLANDS_DIR=<lab dir> FloatingIslands --render <out dir>
//! With FLOATING_ISLANDS_RENDER_EXISTING=1 the lab folder's own islands are drawn as they are (one PNG per island);
//! FLOATING_ISLANDS_RENDER_OPEN="key,…" opens those plan rows first (else the first waiting proposal), Day
//! Timeline rows keyed `tl:<start>`, Settled's `settled:all` and `proof:<key>`, and a proposal's `why:<id>`.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use objc2::msg_send;
use objc2_app_kit::NSScreen;
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_foundation::MainThreadMarker;
use serde_json::Value;

use super::app::app;
use super::draw::{self, Bitmap};
use super::island::notch_rect;
use super::look::*;
use super::parts::Control;
use super::sys;
use crate::jobj;
use crate::model::geometry::Rect;
use crate::model::{j, IslandFile, Json, Paths};

const SCALE: f64 = 2.0;

pub struct Render {
    pub out: PathBuf,
    pub log: RefCell<Vec<String>>,
}

fn rect_from(r: objc2_foundation::NSRect) -> Rect {
    Rect::new(r.origin.x, r.origin.y, r.size.width, r.size.height)
}

pub fn screen() -> Rect {
    let mtm = MainThreadMarker::new().unwrap();
    NSScreen::mainScreen(mtm).map(|s| rect_from(s.frame())).unwrap_or(Rect::new(0.0, 0.0, 1728.0, 1117.0))
}

pub fn clear_folder() {
    let _ = std::fs::remove_file(Paths::visibility());
    for d in [Paths::islands(), Paths::responses()] {
        if let Ok(rd) = std::fs::read_dir(&d) {
            for e in rd.flatten() {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

pub fn put(id: &str, mut o: Json) {
    o.entry("id").or_insert(Value::from(id));
    let _ = IslandFile::write_atomic(&o, &Paths::file(id));
}

fn patch_items(id: &str, f: impl FnOnce(Vec<Json>) -> Vec<Json>) {
    IslandFile::patch(id, |o| {
        let items: Vec<Json> = j::objs(o.get("items")).map(|v| v.into_iter().cloned().collect()).unwrap_or_default();
        o.insert("items".into(), Value::Array(f(items).into_iter().map(Value::Object).collect()));
    });
}

impl Render {
    pub fn new(out: &str) -> Rc<Render> {
        Rc::new(Render { out: PathBuf::from(out), log: RefCell::new(vec![]) })
    }

    /// The rect around some islands (where their shapes are on screen now), with room around them.
    pub fn around(&self, ids: &[&str], pad: f64) -> Rect {
        let a = app();
        ids.iter()
            .filter_map(|id| a.ctl(id))
            .map(|c| rect_from(c.panel.frame()).union(&c.visible_shape_rect()))
            .fold(Rect::NULL, |a, b| a.union(&b))
            .inset(-pad, -pad)
            .integral()
    }

    /// Draw the islands' current presentation into an image of `rect` (screen points, y up).
    pub fn image(&self, rect: Rect) -> Option<draw::Image> {
        let b = Bitmap::new((rect.w * SCALE) as usize, (rect.h * SCALE) as usize)?;
        b.scale(SCALE, SCALE);
        b.vertical_gradient(&rgb(0x2B2E35, 1.0).CGColor(), &rgb(0x1A1C21, 1.0).CGColor(), rect.h);
        // What is on screen: while every island is hidden, only those back for news.
        let mut cs: Vec<_> = app().all().into_iter().filter(|c| app().on_screen(&c.id) || c.leaving()).collect();
        cs.sort_by_key(|c| c.panel.level());
        for c in cs {
            let f = rect_from(c.panel.frame());
            if !f.intersects(&rect) {
                continue;
            }
            b.save();
            b.translate(f.min_x() - rect.min_x(), f.max_y() - rect.min_y());
            b.scale(1.0, -1.0);
            // Its own window only: a translucent island (a fade) is drawn through a transparency layer the size of
            // the clip.
            b.clip(CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(f.w, f.h)));
            let root = shown(&c.root);
            // renderInContext leaves out sublayer transforms: the shape's scale (hide and show, `model/toggle.rs`)
            // is applied here, about the shape's centre as on screen.
            let shape = shown(&c.shape);
            let s: f64 = shape.key_value("transform.scale").map(|v| unsafe { msg_send![&*v, doubleValue] }).unwrap_or(1.0);
            if (s - 1.0).abs() > 1e-6 {
                let p = shape.position();
                b.translate(p.x, p.y);
                b.scale(s, s);
                b.translate(-p.x, -p.y);
            }
            root.renderInContext(b.cg());
            b.restore();
        }
        b.image()
    }

    pub fn write(&self, img: &draw::Image, name: &str) {
        let url = self.out.join(format!("{name}.png"));
        draw::write_png(img, &url);
        self.log.borrow_mut().push(format!("render: {} {}×{}", url.display(), img.width(), img.height()));
    }

    pub fn shot(&self, name: &str, ids: Option<&[&str]>, rect: Option<Rect>) {
        let r = rect.unwrap_or_else(|| {
            let all: Vec<String> = app().controllers.borrow().keys().cloned().collect();
            let ids: Vec<&str> = ids.map(|i| i.to_vec()).unwrap_or_else(|| all.iter().map(|s| s.as_str()).collect());
            self.around(&ids, 20.0)
        });
        if let Some(img) = self.image(r) {
            self.write(&img, name);
        }
        if std::env::var("FI_RENDER_DEBUG").is_ok() {
            for id in ids.unwrap_or(&[]) {
                if let Some(c) = app().ctl(id) {
                    eprintln!("debug {name} {id}: {}", c.debug_state());
                }
            }
        }
    }

    /// Frames of an animation at the given seconds after now, side by side (or in `cols` columns), each labelled with
    /// its time. The crop is the union of where the islands are across the frames.
    pub fn sheet(self: &Rc<Self>, name: &str, ids: &[&str], rect: Option<Rect>, times: &[f64], cols: Option<usize>) {
        let frames: Rc<RefCell<Vec<(f64, draw::Image)>>> = Rc::new(RefCell::new(vec![]));
        let rects: Rc<RefCell<Vec<Rect>>> = Rc::new(RefCell::new(vec![]));
        let t0 = sys::media_time();
        let n = times.len();
        let ids: Vec<String> = ids.iter().map(|s| s.to_string()).collect();
        for t in times {
            let (me, frames, rects, ids, name) = (self.clone(), frames.clone(), rects.clone(), ids.clone(), name.to_owned());
            sys::after(*t, move || {
                if let Some(img) = me.image(screen()) {
                    frames.borrow_mut().push((sys::media_time() - t0, img));
                }
                if rect.is_none() {
                    let idr: Vec<&str> = ids.iter().map(|s| s.as_str()).collect();
                    rects.borrow_mut().push(me.around(&idr, 20.0));
                }
                if frames.borrow().len() != n {
                    return;
                }
                let crop = rect.unwrap_or_else(|| rects.borrow().iter().fold(Rect::NULL, |a, b| a.union(b))).intersection(&screen()).integral();
                me.compose(&name, &frames.borrow(), crop, cols.unwrap_or(n));
            });
        }
    }

    pub fn compose(&self, name: &str, frames: &[(f64, draw::Image)], crop: Rect, cols: usize) {
        self.compose_from(name, frames, screen(), crop, cols)
    }

    /// The same for frames drawn of `scr` (screen points) rather than the whole screen.
    pub fn compose_from(&self, name: &str, frames: &[(f64, draw::Image)], scr: Rect, crop: Rect, cols: usize) {
        let (cw, ch, gap, label) = (crop.w * SCALE, crop.h * SCALE, 12.0 * SCALE, 22.0 * SCALE);
        let rows = frames.len().div_ceil(cols);
        let w = (cols as f64 * cw + (cols as f64 + 1.0) * gap) as usize;
        let h = (rows as f64 * (ch + label) + (rows as f64 + 1.0) * gap) as usize;
        let Some(b) = Bitmap::new(w, h) else { return };
        b.fill(CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(w as f64, h as f64)), &rgb(0x111214, 1.0).CGColor());
        b.with_appkit(false, || {
            for (k, (t, img)) in frames.iter().enumerate() {
                let (col, row) = (k % cols, k / cols);
                let x = gap + col as f64 * (cw + gap);
                let y_top = h as f64 - gap - row as f64 * (ch + label + gap);
                let px = CGRect::new(CGPoint::new((crop.min_x() - scr.min_x()) * SCALE, (scr.max_y() - crop.max_y()) * SCALE), CGSize::new(cw, ch));
                if let Some(part) = img.crop(px) {
                    b.draw(&part, CGRect::new(CGPoint::new(x, y_top - label - ch), CGSize::new(cw, ch)));
                }
                let s = ns(&format!("{t:.2} s"));
                let a = attrs(&digits(12.0 * SCALE, Weight::Medium), &white(0.75), None, false);
                let p = CGPoint::new(x + 2.0 * SCALE, y_top - label + 4.0 * SCALE);
                let _: () = unsafe { msg_send![&*s, drawAtPoint: p, withAttributes: &*a] };
            }
        });
        if let Some(img) = b.image() {
            self.write(&img, name);
        }
    }
}

pub fn run(out: &str) {
    if std::env::var("FLOATING_ISLANDS_DIR").is_err() {
        println!("render: needs FLOATING_ISLANDS_DIR");
        std::process::exit(2);
    }
    let r = Render::new(out);
    let _ = std::fs::create_dir_all(&r.out);
    if std::env::var("FLOATING_ISLANDS_RENDER_EXISTING").as_deref() == Ok("1") {
        existing(r);
        return;
    }
    clear_folder();
    put(
        "checklist",
        jobj! {"preset": "todo", "title": "Checklist", "expanded": true, "items": [
            {"id": "arsenal", "title": "Arsenal: push branch", "group": "running"},
            {"id": "qwen", "title": "Qwen-Image into Arsenal", "group": "running", "detail": "Converting weights"},
            {"id": "caches", "title": "Delete old caches", "group": "waiting",
             "proposal": {"text": "~/.cache/huggingface  12.4 GB\n~/Library/Caches/pip  3.1 GB\nlab/drama3 renders  0.9 GB"}},
            {"id": "ipad", "title": "iPad sale: answer the buyer", "group": "waiting"},
            {"id": "echo", "title": "Echo: Heimdall hears itself", "group": "discussed"},
            {"id": "site", "title": "Arsenal website rebuild", "group": "discussed", "done": true}]},
    );
    // Due times count down from the render's clock: within the hour (full white), later today, tomorrow, days.
    let now = crate::model::time::now();
    let cal = crate::model::due::Cal::current();
    let tomorrow10 = cal.add_days(1, now).and_then(|t| cal.at_time(10, 0, 0, t)).unwrap_or(now + 86400.0);
    let sched_items = serde_json::json!([
        {"id": "menu", "title": "Heimdall: menu revamp", "due": now + 25.0 * 60.0},
        {"id": "phonon", "title": "Phonon voices as presets", "due": now + 3.0 * 3600.0 + 60.0},
        {"id": "review", "title": "Review night pass", "due": tomorrow10},
        {"id": "tap", "title": "Arsenal: Homebrew tap", "due": now + 3.0 * 86400.0},
        {"id": "old", "title": "Vella: research notes", "when": "Sat"}]);
    put("scheduled", jobj! {"preset": "todo", "list": "scheduled", "expanded": true, "items": sched_items});
    // Settled: the week's finished work by its true times (`at`), a decision among it, two days before the record began
    // and one legacy item whose written day is a word (it counts only before this week).
    let day = |back: i64, h: i64| cal.add_days(-back, now).and_then(|t| cal.at_time(h, 0, 0, t)).map(|t| t.min(now - 60.0)).unwrap_or(now);
    put("settled", jobj! {"preset": "todo", "list": "settled", "items": [
        {"id": "s1", "title": "Night pass stage 1: bug fixes", "at": day(4, 11), "kind": "shipped"},
        {"id": "s2", "title": "Islands bundled in Heimdall", "at": day(3, 16), "kind": "shipped"},
        {"id": "s3", "title": "Menu: Speaking becomes Personalisation", "at": day(3, 17), "kind": "decided"},
        {"id": "s4", "title": "Arsenal: Klein and readiness fixes merged", "at": day(1, 21), "kind": "shipped"},
        {"id": "s5", "title": "Settled shows the week's finished work", "at": day(0, 0), "kind": "decided"},
        {"id": "s6", "title": "Old pass", "when": "Today"}]});
    put("job-q", jobj! {"title": "iPhone build", "state": "waiting", "stage": "Your answer", "attention": "question", "icon": "iphone"});
    put("job-bell", jobj! {"title": "Decider data", "state": "waiting", "stage": "Your test", "attention": "waiting"});
    put("job-run", jobj! {"title": "Arsenal site", "state": "running", "stage": "Building", "icon": "globe"});

    let t = Rc::new(std::cell::Cell::new(1.6));
    let at = |dt: f64, f: Box<dyn FnOnce()>| {
        t.set(t.get() + dt);
        sys::after(t.get(), f);
    };
    let r1 = r.clone();
    at(0.0, Box::new(move || {
        r1.shot("layout", None, Some(screen()));
        // At rest nothing animates (the self-test's check, offscreen): any layer still moving is named.
        for c in app().all() {
            let busy: Vec<String> = c.animated_layers().into_iter().filter(|l| !l.ends_with(".work")).collect();
            if !busy.is_empty() {
                eprintln!("render: not at rest: {}: {}", c.id, busy.join(", "));
            }
        }
        r1.shot("checklist-open", Some(&["checklist"]), None);
        r1.shot("scheduled-open", Some(&["scheduled"]), None);
        r1.shot("settled-pill", Some(&["settled"]), None);
        r1.shot("jobs-attention-rest", Some(&["job-q", "job-bell", "job-run"]), None);
    }));
    let r1 = r.clone();
    at(0.2, Box::new(move || {
        let a = app();
        if let Some(c) = a.ctl("job-q") {
            c.pulse_attention();
        }
        if let Some(c) = a.ctl("job-bell") {
            c.pulse_attention();
        }
        r1.sheet("anim-attention", &["job-q", "job-bell"], None, &[0.0, 0.1, 0.2, 0.29, 0.4, 0.6, 0.9, 1.3], None);
    }));
    let r1 = r.clone();
    at(2.0, Box::new(move || {
        if let Some(c) = app().ctl("checklist") {
            c.lab_perform(Control::Disclose { key: "caches".into() });
        }
        r1.sheet("anim-approval-open", &["checklist"], None, &[0.0, 0.05, 0.1, 0.16, 0.24, 0.36, 0.6, 1.0], None);
    }));
    let r1 = r.clone();
    at(1.6, Box::new(move || {
        r1.shot("approval-open", Some(&["checklist"]), None);
        if let Some(c) = app().ctl("checklist") {
            c.lab_perform(Control::Approve { key: "caches".into(), action: "yes".into() });
        }
    }));
    let r1 = r.clone();
    at(0.4, Box::new(move || r1.shot("approval-chosen", Some(&["checklist"]), None)));
    let r1 = r.clone();
    at(1.6, Box::new(move || {
        r1.shot("approval-closed-with-choice", Some(&["checklist"]), None);
        patch_items("checklist", |mut v| {
            v.push(jobj! {"id": "voice", "title": "Heimdall: AirPods only while speaking", "group": "running"});
            v
        });
        r1.sheet("anim-add", &["checklist"], None, &[0.0, 0.05, 0.1, 0.16, 0.24, 0.4, 0.8, 1.5], None);
    }));
    let r1 = r.clone();
    at(2.0, Box::new(move || {
        if let Some(c) = app().ctl("checklist") {
            c.lab_perform(Control::Tick { key: "arsenal".into(), title: String::new() });
        }
        r1.sheet("anim-tick", &["checklist"], None, &[0.0, 0.05, 0.1, 0.16, 0.24, 0.36, 0.6, 1.0], None);
    }));
    let r1 = r.clone();
    at(1.6, Box::new(move || {
        r1.shot("checklist-ticked", Some(&["checklist"]), None);
        patch_items("checklist", |v| {
            v.into_iter()
                .map(|mut i| {
                    if j::str(i.get("id")) == Some("echo") {
                        i.insert("group".into(), Value::from("running"));
                    }
                    i
                })
                .collect()
        });
        r1.sheet("anim-move", &["checklist"], None, &[0.0, 0.05, 0.1, 0.16, 0.24, 0.36, 0.6, 1.4], None);
    }));
    let r1 = r.clone();
    at(2.0, Box::new(move || {
        patch_items("checklist", |v| v.into_iter().filter(|i| j::str(i.get("id")) != Some("ipad")).collect());
        r1.sheet("anim-remove", &["checklist"], None, &[0.0, 0.05, 0.1, 0.16, 0.24, 0.36, 0.6, 1.0], None);
    }));
    let r1 = r.clone();
    at(1.6, Box::new(move || {
        IslandFile::patch("settled", |o| {
            o.insert("expanded".into(), Value::Bool(true));
        });
        // Its first frames as it opens (the file watch takes a moment, so the sheet runs a little long).
        r1.sheet("anim-settled-open", &["settled"], None, &[0.0, 0.04, 0.08, 0.12, 0.18, 0.26, 0.4, 0.9], None);
    }));
    let r1 = r.clone();
    at(1.0, Box::new(move || {
        r1.shot("settled-open", Some(&["settled"]), None);
        let moved: RefCell<Option<Json>> = RefCell::new(None);
        patch_items("scheduled", |v| {
            *moved.borrow_mut() = v.iter().find(|i| j::str(i.get("id")) == Some("phonon")).cloned();
            v.into_iter().filter(|i| j::str(i.get("id")) != Some("phonon")).collect()
        });
        let m = moved.borrow().clone().unwrap_or_default();
        patch_items("settled", |mut v| {
            v.push(m);
            v
        });
        r1.sheet("anim-between-lists", &["scheduled", "settled"], None, &[0.0, 0.05, 0.1, 0.16, 0.24, 0.36, 0.6, 1.5], None);
    }));
    let r1 = r.clone();
    at(2.0, Box::new(move || {
        put("job-new", jobj! {"title": "Port voices", "state": "running", "stage": "Starting", "icon": "waveform", "origin": {"from": "notch"}});
        let mtm = MainThreadMarker::new().unwrap();
        let n = notch_rect(NSScreen::mainScreen(mtm).as_deref());
        let s = screen();
        let rr = Rect::new(n.min_x() - 40.0, s.max_y() - 300.0, s.max_x() - n.min_x() + 40.0, 300.0);
        r1.sheet("anim-notch", &[], Some(rr), &[0.02, 0.06, 0.1, 0.15, 0.2, 0.28, 0.4, 0.8], Some(2));
    }));
    // Hide all and show again (⌃⌘, / Show Islands): the files and every island's frame and openness come back
    // exactly as they were.
    let before: Rc<RefCell<Option<(Vec<(String, Rect, bool)>, Vec<(String, Json)>)>>> = Rc::new(RefCell::new(None));
    let (r1, b1) = (r.clone(), before.clone());
    at(2.2, Box::new(move || {
        r1.shot("visibility-shown", None, Some(screen()));
        *b1.borrow_mut() = Some((frames(), files()));
        let _ = crate::model::Visibility::set_hidden(true);
        // Chained from here: the flag reaches the app through its file watch (150 ms coalesced).
        let (r2, b2) = (r1.clone(), b1.clone());
        when_hidden(true, 40, move |seen| {
            r2.shot("visibility-hidden", None, Some(screen()));
            put("job-late", jobj! {"title": "Late result", "state": "done", "value": "3 / 3", "icon": "checkmark"});
            let (r3, b3) = (r2.clone(), b2.clone());
            sys::after(0.5, move || {
                // At rest nothing draws: an island off screen keeps no pulse or clock timer (`Ctl::refresh_timers`).
                let all = app().all();
                let off: Vec<_> = all.iter().filter(|c| !app().on_screen(&c.id)).collect();
                let armed: Vec<String> = off.iter().filter(|c| c.timers_armed() != Default::default()).map(|c| format!("{} {:?}", c.id, c.timers_armed())).collect();
                println!("render: hidden, {} islands off screen, timers armed {}", off.len(), if armed.is_empty() { "none".to_owned() } else { armed.join(", ") });
                let _ = std::fs::remove_file(Paths::file("job-late"));
                let (r4, b4) = (r3.clone(), b3.clone());
                sys::after(0.5, move || {
                    let _ = crate::model::Visibility::set_hidden(false);
                    let (r5, b5) = (r4.clone(), b4.clone());
                    when_hidden(false, 40, move |shown| {
                        let (r6, b6) = (r5.clone(), b5.clone());
                        // The layout's spring settles before the frames are compared.
                        sys::after(0.8, move || {
                            r6.shot("visibility-shown-again", None, Some(screen()));
                            let (f0, d0) = b6.borrow_mut().take().unwrap_or_default();
                            let (f1, d1) = (frames(), files());
                            let same = f0 == f1 && d0.len() == d1.len() && d0.iter().zip(&d1).all(|(a, b)| a.0 == b.0 && j::obj_eq(&a.1, &b.1));
                            println!("render: hidden {seen}, shown again {shown}; hide/show {} ({} islands)", if same { "restores every island exactly" } else { "CHANGED islands" }, f1.len());
                            if !same {
                                println!("render: before {f0:?}\nrender: after {f1:?}");
                            }
                            let armed: Vec<String> = app().all().iter().filter(|c| c.timers_armed() != Default::default()).map(|c| format!("{} {:?}", c.id, c.timers_armed())).collect();
                            println!("render: shown again, timers armed {}", armed.join(", "));
                            VISIBILITY_DONE.with(|d| d.set(true));
                        });
                    });
                });
            });
        });
    }));
    let r1 = r.clone();
    let r2 = r.clone();
    at(4.5, Box::new(move || after_visibility(Box::new(move || flash(r2, Box::new(move || {
        r1.shot("layout-after", None, Some(screen()));
        let busy: Vec<String> = app()
            .all()
            .iter()
            .flat_map(|c| {
                let att = c.attention_shown().is_some();
                c.animated_layers().into_iter().filter(move |l| !(att && l.contains("attn"))).map(move |l| format!("{}: {l}", c.id))
            })
            .collect();
        println!("render: at rest {}", if busy.is_empty() { "nothing animating".to_owned() } else { busy.join(", ") });
        for l in r1.log.borrow().iter() {
            println!("{l}");
        }
        clear_folder();
        std::process::exit(0);
    }))))));
}

/// Hidden islands back for news (`model/flash.rs`): with every island hidden, a job finishes and only it fades in at
/// its place, pulses green, stays and fades out; then a question arrives and stays. Shown again afterwards.
fn flash(r: Rc<Render>, next: Box<dyn FnOnce()>) {
    use crate::model::flash::{FADE_OUT, HOLD};
    let _ = crate::model::Visibility::set_hidden(true);
    when(|| app().hidden(), 40, move |_| {
        let crop = r.around(&["job-run", "job-q", "job-bell"], 40.0).intersection(&screen()).integral();
        r.shot("flash-hidden", None, Some(crop));
        IslandFile::patch("job-run", |o| {
            o.insert("state".into(), Value::from("done"));
            o.insert("value".into(), Value::from("Live"));
        });
        let t0 = sys::media_time();
        let r1 = r.clone();
        when(move || app().on_screen("job-run"), 40, move |up| {
            let seen = sys::media_time() - t0;
            r1.sheet("anim-flash", &[], Some(crop), &[0.05, 0.2, 0.5, 1.2, 2.5, HOLD + 0.2, HOLD + 0.4, HOLD + FADE_OUT + 0.3], Some(4));
            let r2 = r1.clone();
            sys::after(HOLD + FADE_OUT + 0.6, move || {
                let gone = !app().on_screen("job-run");
                put("job-ask", jobj! {"title": "Voice pick", "state": "waiting", "stage": "Your answer", "attention": "question", "icon": "waveform",
                    "parts": [{"type": "button", "label": "Warm", "action": "warm"}, {"type": "button", "label": "Bright", "action": "bright"}]});
                let r3 = r2.clone();
                sys::after(HOLD + FADE_OUT + 1.5, move || {
                    let stays = app().on_screen("job-ask");
                    r3.shot("flash-question-stays", None, Some(r3.around(&["job-run", "job-q", "job-bell", "job-ask"], 40.0).intersection(&screen()).integral()));
                    println!("render: flash: news seen {up} after {seen:.2} s, gone after the hold {gone}; a question still up {:.1} s later {stays}; nothing else on screen {}",
                        HOLD + FADE_OUT + 1.5, app().all().iter().filter(|c| app().on_screen(&c.id)).count() == 1);
                    let _ = std::fs::remove_file(Paths::file("job-ask"));
                    let _ = crate::model::Visibility::set_hidden(false);
                    when(|| !app().hidden(), 40, move |_| sys::after(0.8, next));
                });
            });
        });
    });
}

/// Runs `f` once `cond` holds (checked every 50 ms, at most `tries` times); `f` gets whether it did.
fn when(cond: impl Fn() -> bool + 'static, tries: u32, f: impl FnOnce(bool) + 'static) {
    if cond() || tries == 0 {
        return f(cond());
    }
    sys::after(0.05, move || when(cond, tries - 1, f));
}

thread_local! {
    static VISIBILITY_DONE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Runs `f` once the hide/show check has printed (at most 10 s later).
fn after_visibility(f: Box<dyn FnOnce()>) {
    fn wait(f: Box<dyn FnOnce()>, left: u32) {
        if VISIBILITY_DONE.with(|d| d.get()) || left == 0 {
            f();
        } else {
            sys::after(0.1, move || wait(f, left - 1));
        }
    }
    wait(f, 100);
}

/// Runs `f` once the app's hidden state is `want` (checked every 50 ms, at most `tries` times); `f` gets whether it
/// got there.
fn when_hidden(want: bool, tries: u32, f: impl Fn(bool) + 'static) {
    if app().hidden() == want || tries == 0 {
        f(app().hidden() == want);
        return;
    }
    sys::after(0.05, move || when_hidden(want, tries - 1, f));
}

/// Every island's frame on screen and whether it is open, by id.
fn frames() -> Vec<(String, Rect, bool)> {
    app().all().iter().filter(|c| !c.removing()).map(|c| (c.id.clone(), c.shape_rect(), c.expanded())).collect()
}

/// Every island file, by id.
fn files() -> Vec<(String, Json)> {
    let mut v: Vec<(String, Json)> = std::fs::read_dir(Paths::islands())
        .map(|d| d.flatten().filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let id = name.strip_suffix(".json").filter(|_| !name.starts_with('.'))?.to_owned();
            IslandFile::read(&e.path()).map(|o| (id, o))
        }).collect())
        .unwrap_or_default();
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v
}

/// The folder's islands as they are, each alone, once their arrival has settled; then the whole layout.
fn existing(r: Rc<Render>) {
    sys::after(1.0, || {
        for c in app().all() {
            let i = c.island();
            if i.preset() == "todo" && i.expanded() {
                // FLOATING_ISLANDS_RENDER_OPEN="key,key" opens those rows instead ("none": every row closed).
                if let Ok(keys) = std::env::var("FLOATING_ISLANDS_RENDER_OPEN") {
                    for it in i.plan_items().into_iter().filter(|p| keys.split(',').any(|k| k == p.key)) {
                        c.lab_perform(Control::Disclose { key: it.key });
                    }
                    // Settled's own keys ("settled:all": today's every row), a proposal's why ("why:<id>"), a proof
                    // shown under its row ("proof:<key>").
                    for k in keys.split(',').filter(|k| ["settled:", "why:", "proof:"].iter().any(|p| k.starts_with(p))) {
                        c.lab_perform(Control::Disclose { key: k.into() });
                    }
                } else if let Some(it) = i.plan_items().into_iter().find(|p| p.proposal.is_some() && !p.done && p.group.as_deref() == Some("waiting")) {
                    c.lab_perform(Control::Disclose { key: it.key });
                }
            }
            if i.expanded() && super::island::has_timeline(&i) {
                // Day Timeline rows open by their key (`tl:<start>`), from the same variable.
                for key in std::env::var("FLOATING_ISLANDS_RENDER_OPEN").unwrap_or_default().split(',').filter(|k| k.starts_with("tl:")) {
                    c.lab_perform(Control::Disclose { key: key.into() });
                }
            }
        }
    });
    let r1 = r.clone();
    sys::after(2.0, move || {
        let ids: Vec<String> = app().controllers.borrow().keys().cloned().collect();
        for id in &ids {
            r1.shot(id, Some(&[id.as_str()]), None);
        }
        r1.shot("layout", None, Some(screen()));
        for id in ids.iter().filter(|id| app().ctl(id).is_some_and(|c| c.island().look() == "task")) {
            r1.sheet(&format!("work-{id}"), &[id.as_str()], None, &[2.0, 2.25, 2.45, 2.65, 2.85, 3.3, 4.2], None);
        }
    });
    // FLOATING_ISLANDS_RENDER_CLICK=<island>: click its first picture (code, not events) and draw the viewer growing
    // out of it, its motion frozen (FI_LAB_SCRUB) and drawn every 16 ms for 0.64 s (`viewer-open/frame-NNN.png`,
    // cropped to FLOATING_ISLANDS_RENDER_CROP "x,y,w,h" in screen points, y up, else the whole screen), a contact
    // sheet of every third frame.
    if let Ok(id) = std::env::var("FLOATING_ISLANDS_RENDER_CLICK") {
        let r2 = r.clone();
        sys::after(2.6, move || {
            let Some(c) = app().ctl(&id) else { println!("render: no island {id}"); return };
            let crop = std::env::var("FLOATING_ISLANDS_RENDER_CROP").ok().and_then(|s| {
                let v: Vec<f64> = s.split(',').filter_map(|x| x.trim().parse().ok()).collect();
                (v.len() == 4).then(|| Rect::new(v[0], v[1], v[2], v[3]))
            }).unwrap_or_else(screen);
            let _ = std::fs::create_dir_all(r2.out.join("viewer-open"));
            // FLOATING_ISLANDS_RENDER_CLICK_WHAT=chip clicks the open chip (a row: no picture) instead.
            let chip = std::env::var("FLOATING_ISLANDS_RENDER_CLICK_WHAT").as_deref() == Ok("chip");
            if !(if chip { c.lab_click_chip() } else { c.lab_click_picture() }) { println!("render: nothing to click on {id}"); return }
            fn draw(r: Rc<Render>, crop: Rect, tries: u32) {
                let Some(v) = app().all().into_iter().find(|c| c.island().preset() == "viewer" && c.lab_scrub(0.0)) else {
                    if tries > 0 { sys::after(0.01, move || draw(r, crop, tries - 1)) } else { println!("render: no frozen viewer") }
                    return;
                };
                let mut frames = vec![];
                for k in 0..=40 {
                    let t = k as f64 * 0.016;
                    v.lab_scrub(t);
                    if let Some(img) = r.image(screen()) {
                        if let Some(part) = img.crop(CGRect::new(CGPoint::new((crop.min_x() - screen().min_x()) * SCALE, (screen().max_y() - crop.max_y()) * SCALE), CGSize::new(crop.w * SCALE, crop.h * SCALE))) {
                            r.write(&part, &format!("viewer-open/frame-{:03}", (t * 1000.0).round() as i64));
                        }
                        if k % 3 == 0 { frames.push((t, img)) }
                    }
                }
                r.compose("viewer-open/sheet", &frames, crop, 7);
                // The end state is the last frame; let the motion go (offscreen, a viewer's own image view draws no
                // contents for renderInContext once it runs freely, so no settled shot is taken).
                v.lab_unfreeze();
            }
            draw(r2.clone(), crop, 150);
        });
    }
    sys::after(6.8, move || {
        for l in r.log.borrow().iter() {
            println!("{l}");
        }
        std::process::exit(0);
    });
}

pub fn write_image(img: &draw::Image, path: &Path) -> bool {
    draw::write_png(img, path)
}
