//! Lab recording of hide and show (`model/toggle.rs`): islands built offscreen (no window is ever ordered on screen,
//! nothing is clicked), their presentation drawn into bitmaps about every 1/60 s as the motion plays in real time.
//!   FLOATING_ISLANDS_DIR=<lab dir> FloatingIslands --render-toggle <out dir>
//! Writes `frames/<scene>-<n>.png` (each frame's real time in `frames/<scene>.txt`), a contact sheet per scene, and
//! prints whether every island ends exactly where it began, whole, with an open viewer live again. Scenes: hide, show,
//! a double press (hide, then show 0.12 s later), and news for an island while it shrinks away (it comes back whole
//! and stays up). FLOATING_ISLANDS_REDUCE_MOTION=1 records the plain fade instead.

use std::cell::RefCell;
use std::rc::Rc;

use super::app::app;
use super::draw;
use super::look::*;
use super::render::{clear_folder, put, screen, Render};
use super::sys;
use crate::jobj;
use crate::model::geometry::Rect;

const FPS: f64 = 60.0;

type Places = Vec<(String, Rect, Rect, f64, f32, bool)>;

/// Every island's place, window, scale, opacity on screen now, and whether an open viewer's live view is shown.
fn places() -> Places {
    // Committed, so the presentation read is of now (it is cached per transaction).
    objc2_quartz_core::CATransaction::flush();
    app()
        .all()
        .iter()
        .map(|c| {
            let scale: f64 = shown(&c.shape).key_value("transform.scale").map(|v| unsafe { objc2::msg_send![&*v, doubleValue] }).unwrap_or(-1.0);
            let live = !(c.is_viewer() && c.expanded()) || c.file_viewer().and_then(|v| v.root()).is_some_and(|r| !r.isHidden() && r.alphaValue() > 0.99);
            (c.id.clone(), c.shape_rect(), c.visible_shape_rect(), scale, shown(&c.root).opacity(), live)
        })
        .collect()
}

/// Full size, fully lit, an open viewer live.
fn whole(p: &Places) -> bool {
    p.iter().all(|x| (x.3 - 1.0).abs() < 1e-6 && x.4 == 1.0 && x.5)
}

/// Islands whose window is not exactly their shape now (at rest none is).
fn loose_windows() -> Vec<String> {
    app().all().iter().filter(|c| super::island::rect_of_window(c) != c.shape_rect()).map(|c| format!("{} window {:?} shape {:?}", c.id, super::island::rect_of_window(c), c.shape_rect())).collect()
}

/// The same islands in the same places, their windows exactly their shapes.
fn same(a: &Places, b: &Places) -> bool {
    let windows = app().all().iter().all(|c| super::island::rect_of_window(c) == c.shape_rect());
    windows && a.len() == b.len() && a.iter().zip(b.iter()).all(|(x, y)| x.0 == y.0 && x.1 == y.1 && x.2 == y.2)
}

/// Frames for `secs` seconds from now, kept in memory while recording and written afterwards.
fn record(r: Rc<Render>, scene: &'static str, secs: f64, then: Box<dyn FnOnce()>) {
    let frames: Rc<RefCell<Vec<(f64, draw::Image)>>> = Rc::new(RefCell::new(vec![]));
    let t0 = sys::media_time();
    // Only the band the islands are in: a whole 2× screen per frame is too slow for 60 Hz.
    let crop = app().all().iter().map(|c| c.shape_rect()).fold(Rect::NULL, |a, b| a.union(&b)).inset(-30.0, -30.0).intersection(&screen()).integral();
    fn tick(r: Rc<Render>, scene: &'static str, t0: f64, secs: f64, crop: Rect, frames: Rc<RefCell<Vec<(f64, draw::Image)>>>, then: Box<dyn FnOnce()>) {
        // Committed, so the presentation read below is of now (it is cached per transaction).
        objc2_quartz_core::CATransaction::flush();
        let t = sys::media_time() - t0;
        if std::env::var_os("FI_DEBUG").is_some() {
            eprintln!("{scene} {t:.3} {:?}", places().iter().map(|p| (p.0.clone(), (p.3 * 100.0).round() / 100.0, p.4)).collect::<Vec<_>>());
        }
        if let Some(img) = r.image(crop) {
            frames.borrow_mut().push((t, img));
        }
        if t >= secs {
            let dir = r.out.join("frames");
            let _ = std::fs::create_dir_all(&dir);
            let mut times = String::new();
            for (n, (t, img)) in frames.borrow().iter().enumerate() {
                draw::write_png(img, &dir.join(format!("{scene}-{n:03}.png")));
                times.push_str(&format!("{n:03} {t:.4}\n"));
            }
            let _ = std::fs::write(dir.join(format!("{scene}.txt")), times);
            // The sheet: every second frame.
            let all = frames.borrow();
            let pick: Vec<(f64, draw::Image)> = all.iter().step_by(2).map(|(t, i)| (*t, i.clone())).collect();
            r.compose_from(&format!("sheet-{scene}"), &pick, crop, crop, 4);
            println!("render-toggle: {scene}: {} frames in {:.2} s", all.len(), t);
            drop(all);
            then();
            return;
        }
        // The next slot on the 60 Hz grid from the start (a late frame skips ahead rather than drift).
        let next = ((t * FPS).floor() + 1.0) / FPS;
        let wait = (t0 + next - sys::media_time()).max(0.0);
        sys::after(wait, move || tick(r, scene, t0, secs, crop, frames, then));
    }
    tick(r, scene, t0, secs, crop, frames, then);
}

/// A real-looking desk: an open list and pills on the top row, jobs down the side columns.
fn desk() {
    clear_folder();
    put("todo", jobj! {"preset": "todo", "list": "todo", "expanded": true, "items": [
        {"id": "arsenal", "title": "Arsenal: push branch", "group": "running"},
        {"id": "qwen", "title": "Qwen-Image into Arsenal", "group": "running", "detail": "Converting weights"},
        {"id": "ipad", "title": "iPad sale: answer the buyer", "group": "waiting"},
        {"id": "site", "title": "Arsenal website rebuild", "group": "discussed", "done": true}]});
    put("settled", jobj! {"preset": "todo", "list": "settled", "items": [{"id": "s1", "title": "Night pass stage 1: bug fixes"}]});
    put("job-q", jobj! {"title": "iPhone build", "state": "waiting", "stage": "Your answer", "icon": "iphone"});
    put("job-run", jobj! {"title": "Arsenal site", "state": "running", "stage": "Building", "icon": "globe"});
    put("job-done", jobj! {"title": "Voice bank", "state": "done", "value": "12 / 12", "icon": "checkmark"});
    put("job-left", jobj! {"title": "Decider data", "state": "running", "stage": "Labelling", "icon": "waveform", "side": "left"});
    let md = crate::model::Paths::root().join("toggle-view.md");
    let _ = std::fs::write(&md, "# Night pass\n\nThree fixes are in, **one** waits for you:\n\n- islands toggle\n- notch drop\n");
    put("view", jobj! {"preset": "viewer", "title": "Night pass", "path": md.to_string_lossy(), "expanded": true});
}

pub fn run(out: &str) {
    let r = Render::new(out);
    let _ = std::fs::create_dir_all(&r.out);
    desk();

    let before: Rc<RefCell<Places>> = Rc::new(RefCell::new(vec![]));
    let (r1, b1) = (r.clone(), before.clone());
    // Settled first (the arrival springs are long over).
    sys::after(2.0, move || {
        r1.write(&r1.image(screen()).unwrap(), "rest-before");
        *b1.borrow_mut() = places();
        app().set_hidden(true);
        let (r2, b2) = (r1.clone(), b1.clone());
        record(r1.clone(), "hide", 0.5, Box::new(move || {
            sys::after(0.3, move || {
                app().set_hidden(false);
                let (r3, b3) = (r2.clone(), b2.clone());
                record(r2.clone(), "show", 0.8, Box::new(move || {
                    sys::after(1.0, move || {
                        let mid = places();
                        // Twice fast: hidden, then shown again 0.12 s later, mid-shrink.
                        app().set_hidden(true);
                        sys::after(0.12, || app().set_hidden(false));
                        let (r4, b4) = (r3.clone(), b3.clone());
                        record(r3.clone(), "double", 0.9, Box::new(move || {
                            sys::after(1.0, move || {
                                r4.write(&r4.image(screen()).unwrap(), "rest-after");
                                let after = places();
                                let b = b4.borrow().clone();
                                let say = |p: &Places| format!("{} and {}", if same(p, &b) { "every island exactly in place" } else { "MOVED islands" }, if whole(p) { "whole" } else { "NOT whole" });
                                println!("render-toggle: {} islands; after show {}; after the double press {}", b.len(), say(&mid), say(&after));
                                if !(same(&mid, &b) && same(&after, &b) && whole(&b) && whole(&mid) && whole(&after)) {
                                    println!("render-toggle: before {b:?}\nrender-toggle: after show {mid:?}\nrender-toggle: after double {after:?}");
                                }
                                news(r4, b);
                            });
                        }));
                    });
                }));
            });
        }));
    });
}

/// News for an island while it shrinks away: it turns around where it is, comes back whole and stays up for the news
/// while the rest are hidden; shown again, everything is as it was.
/// Hidden through `visibility.json` as Heimdall does: the folder's watch reads the file again on any change nearby
/// (the news is one), so a hide the file does not say would be undone.
fn news(r: Rc<Render>, before: Places) {
    let _ = crate::model::Visibility::set_hidden(true);
    fn when_hidden(left: u32, f: Box<dyn FnOnce()>) {
        if app().hidden() || left == 0 {
            return f();
        }
        sys::after(0.005, move || when_hidden(left - 1, f));
    }
    when_hidden(400, Box::new(move || news_now(r, before)));
}

fn news_now(r: Rc<Render>, before: Places) {
    crate::model::IslandFile::patch("job-run", |o| {
        o.insert("state".into(), serde_json::Value::from("done"));
        o.insert("value".into(), serde_json::Value::from("Live"));
    });
    // And the open viewer, gone by now: it comes back for its news live, not as its picture.
    sys::after(0.6, || {
        // A reply, as Heimdall marks one.
        crate::model::IslandFile::patch("view", |o| {
            o.insert("updated".into(), serde_json::Value::from(crate::model::time::now()));
        });
    });
    // When the news lands (the store's watch), whether the island was still shrinking.
    fn watch(was_leaving: bool, left: u32, f: Box<dyn FnOnce(bool)>) {
        let up = app().on_screen("job-run");
        if up || left == 0 {
            return f(up && was_leaving);
        }
        let leaving = app().ctl("job-run").is_some_and(|c| c.leaving());
        sys::after(0.005, move || watch(leaving, left - 1, f));
    }
    let r1 = r.clone();
    record(r.clone(), "news", 0.9, Box::new(|| {}));
    watch(false, 200, Box::new(move |mid_shrink| {
        // After the viewer's own fade in (its news lands 0.6 s in, through the store's watch), within the hold.
        sys::after(1.8, move || {
            let p: Places = places().into_iter().filter(|x| x.0 == "job-run" || x.0 == "view").collect();
            let up: Vec<String> = app().all().iter().filter(|c| c.id != "job-run" && c.id != "view" && (app().on_screen(&c.id) || c.leaving())).map(|c| c.id.clone()).collect();
            let viewer_up = app().on_screen("view");
            println!("render-toggle: news mid-shrink {mid_shrink}; it is back {}, the viewer up for news {viewer_up} and live; also up: {up:?}", if whole(&p) && p.len() == 2 { "whole" } else { "NOT whole" });
            if !whole(&p) {
                println!("render-toggle: up for news {p:?}; loose windows {:?}", loose_windows());
            }
            let _ = crate::model::Visibility::set_hidden(false);
            sys::after(1.4, move || {
                let after = places();
                let changed: Vec<String> = after.iter().zip(&before).filter(|(a, b)| a.0 != "job-run" && a.0 != "view" && (a.1 != b.1 || a.2 != b.2)).map(|(a, _)| a.0.clone()).collect();
                println!("render-toggle: shown again {} and {}", if changed.is_empty() { "every island exactly in place" } else { "MOVED islands" }, if whole(&after) { "whole" } else { "NOT whole" });
                if !changed.is_empty() || !whole(&after) {
                    println!("render-toggle: after news {after:?}");
                }
                let busy: Vec<String> = app().all().iter().flat_map(|c| c.animated_layers().into_iter().map(move |l| format!("{}: {l}", c.id))).collect();
                println!("render-toggle: at rest {}", if busy.is_empty() { "nothing animating".to_owned() } else { busy.join(", ") });
                drop(r1);
                clear_folder();
                std::process::exit(0);
            });
        });
    }));
}

/// The same on the real screen, for a capture through the window server: `FLOATING_ISLANDS_DIR=<lab dir>
/// FI_OVERLAY=standard FloatingIslands --record-toggle <out dir>`, recorded by `lab/islands-toggle/recorder.swift`,
/// which takes this lab instance's own windows only (never Heimdall's, no cursor). Nothing is clicked and the cursor is
/// never moved. Writes `band.txt` (`x,y,w,h`, points from the top left) once the desk has settled, waits for the
/// recorder's `go`, then hides at 0.5 s, shows at 1.3 s, and presses twice fast (hide at 2.5 s, show again mid-shrink),
/// each through `visibility.json` as Heimdall does (its watch coalesces 150 ms, so presses closer than that net out
/// to nothing); `events.txt` has each write on the recorder's clock. Prints whether every island ended exactly where
/// it began.
pub fn live(out: &str) {
    // Lab timers on time: a background accessory app's timers are otherwise coalesced (App Nap), late by 100 ms.
    let activity = objc2_foundation::NSProcessInfo::processInfo().beginActivityWithOptions_reason(
        objc2_foundation::NSActivityOptions::UserInitiated | objc2_foundation::NSActivityOptions::LatencyCritical,
        &objc2_foundation::NSString::from_str("lab recording"),
    );
    std::mem::forget(activity);
    let out = std::path::PathBuf::from(out);
    let _ = std::fs::create_dir_all(&out);
    let _ = std::fs::write(crate::model::Paths::streams(), r#"{"shown": true}"#);
    desk();
    sys::after(2.0, move || {
        let scr = screen();
        let band = app().all().iter().map(|c| c.shape_rect()).fold(Rect::NULL, |a, b| a.union(&b)).inset(-40.0, -40.0).intersection(&scr).integral();
        let before = places();
        let _ = std::fs::write(out.join("band.txt"), format!("{},{},{},{}", band.x - scr.x, scr.max_y() - band.max_y(), band.w, band.h));
        println!("record-toggle: band {band:?}");
        fn wait_go(out: std::path::PathBuf, left: u32, f: Box<dyn FnOnce(std::path::PathBuf)>) {
            if out.join("go").exists() || left == 0 {
                return f(out);
            }
            sys::after(0.05, move || wait_go(out, left - 1, f));
        }
        wait_go(out, 200, Box::new(move |out| {
            let events: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
            let at = |dt: f64, name: &'static str, hidden: bool| {
                let ev = events.clone();
                sys::after(dt, move || {
                    ev.borrow_mut().push_str(&format!("{:.4} {name}\n", sys::media_time()));
                    let _ = crate::model::Visibility::set_hidden(hidden);
                });
            };
            at(0.5, "hide", true);
            at(1.3, "show", false);
            // The second press written as soon as the first has reached the islands, so it arrives through the
            // watch about 0.17 s into the shrink.
            let ev = events.clone();
            sys::after(2.5, move || {
                ev.borrow_mut().push_str(&format!("{:.4} hide-again\n", sys::media_time()));
                let _ = crate::model::Visibility::set_hidden(true);
                fn when_hidden(ev: Rc<RefCell<String>>, left: u32) {
                    if app().hidden() || left == 0 {
                        ev.borrow_mut().push_str(&format!("{:.4} show-again\n", sys::media_time()));
                        let _ = crate::model::Visibility::set_hidden(false);
                        return;
                    }
                    sys::after(0.01, move || when_hidden(ev, left - 1));
                }
                when_hidden(ev, 100);
            });
            sys::after(4.2, move || {
                let after = places();
                let same = same(&after, &before) && whole(&after);
                let gone: Vec<_> = app().all().iter().filter(|c| !c.panel.isVisible()).map(|c| c.id.clone()).collect();
                println!("record-toggle: {} islands {}; windows not on screen: {gone:?}", after.len(), if same { "every island exactly in place and whole" } else { "MOVED or NOT whole" });
                if !same {
                    println!("record-toggle: before {before:?}\nrecord-toggle: after {after:?}\nrecord-toggle: loose windows {:?}", loose_windows());
                }
                let _ = std::fs::write(out.join("events.txt"), events.borrow().as_str());
                clear_folder();
                let _ = std::fs::remove_file(crate::model::Paths::streams());
                std::process::exit(0);
            });
        }));
    });
}
