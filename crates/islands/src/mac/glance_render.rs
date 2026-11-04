//! Lab: a job island's states with its glance (the bottom row, `mac/glance.rs`), offscreen at real size.
//!   FLOATING_ISLANDS_DIR=<lab dir> FloatingIslands --render-glance <out dir>
//! One island at a time, open: running with a stage, with and without an open chip; running with numbers the agent
//! rewrote (progress, a loss, a share), so its sparklines have a history; the progress ring in the header; waiting;
//! done with and without a chip; failed; a long title, stage and file name; the closed pill. Then a rewrite while open,
//! as a contact sheet (only the newest step draws), and what still moves at rest.

use std::rc::Rc;

use serde_json::Value;

use super::app::app;
use super::render::{clear_folder, put, Render};
use super::sys;
use crate::jobj;
use crate::model::{time, IslandFile, Json, Paths};

/// The decider's training: progress, loss and speed as the agent rewrote them.
const PROGRESS: [f64; 7] = [0.18, 0.24, 0.31, 0.37, 0.45, 0.52, 0.58];
const LOSS: [f64; 7] = [0.92, 0.81, 0.77, 0.70, 0.66, 0.64, 0.61];
const SPEED: [f64; 7] = [28.0, 31.0, 30.0, 33.0, 32.0, 34.0, 34.0];
const MEMORY: [f64; 7] = [38.0, 41.0, 41.0, 44.0, 46.0, 46.0, 47.0];

fn states(page: &str, long_page: &str) -> Vec<(&'static str, Json)> {
    let now = time::now();
    let min = 60.0;
    vec![
        ("running-stage-chip", jobj! {"title": "Jarvis voice", "icon": "waveform", "state": "running", "stage": "Building page",
            "summary": "Cleaning the 214 takes, then the compare page with before and after for each",
            "steps": [{"title": "Cut silences", "state": "done"}, {"title": "Denoise takes", "state": "done"}, {"title": "Compare page", "state": "now"}, {"title": "Your pick", "state": "todo"}],
            "open": page, "started": now - 7.0 * min, "updated": now - 2.0 * min, "expanded": true}),
        ("running-stage-nochip", jobj! {"title": "Jarvis voice", "icon": "waveform", "state": "running", "stage": "Denoising",
            "summary": "Cleaning the 214 takes before the compare page",
            "steps": [{"title": "Cut silences", "state": "done"}, {"title": "Denoise takes", "state": "now"}, {"title": "Compare page", "state": "todo"}],
            "started": now - 3.0 * min, "expanded": true}),
        ("running-ring", jobj! {"title": "Arsenal site", "state": "running", "stage": "Uploading", "progress": 0.42,
            "summary": "Pushing the rebuilt pages; the header's ring is the progress", "started": now - 12.0 * min, "expanded": true}),
        ("waiting", jobj! {"title": "Voice pick", "icon": "waveform", "state": "waiting", "stage": "Your answer",
            "summary": "Two finalists are ready; the page has both side by side", "open": page, "started": now - 4.0 * min, "expanded": true}),
        ("done-chip", jobj! {"title": "Jarvis voice", "icon": "waveform", "state": "done", "stage": "Done", "value": "214 / 214",
            "summary": "All takes cleaned; the compare page has every one before and after",
            "open": page, "started": now - 31.0 * min, "ended": now - 4.0 * min, "expanded": true}),
        ("done-nochip", jobj! {"title": "Ack lines", "state": "done", "stage": "Done",
            "summary": "Four acks re-recorded in the butler voice", "started": now - 9.0 * min, "ended": now - 1.0 * min, "expanded": true}),
        ("failed", jobj! {"title": "Heimdall build", "state": "failed", "stage": "Build broke",
            "summary": "The UI crate fails to link: a missing AppKit symbol after the update",
            "started": now - 14.0 * min, "updated": now - 6.0 * min, "expanded": true}),
        ("narrow", jobj! {"title": "Shards", "icon": "square.stack.3d.up", "state": "running", "stage": "Copying", "progress": 0.58,
            "width": 240, "summary": "A narrow island: the row drops the clock, then the graphic, never overlaps",
            "open": long_page, "started": now - 23.0 * 60.0 * min / 60.0 - 59.0 * min, "expanded": true}),
        ("long-title", jobj! {"title": "Jarvis voice cleanup and retakes", "icon": "waveform", "state": "running",
            "stage": "Rebuilding the speaker index from scratch", "summary": "Long names everywhere: the row gives way, nothing overlaps",
            "open": long_page, "started": now - 75.0 * min, "expanded": true}),
    ]
}

fn series(step: usize) -> Json {
    let now = time::now();
    jobj! {"title": "Decider v4", "icon": "brain", "state": "running", "stage": "Training",
        "summary": "Fine-tuning on the 9k labelled turns; the gate runs at the end",
        "progress": PROGRESS[step],
        "kv": [{"label": "Memory", "value": format!("{} GB", MEMORY[step])}, {"label": "Speed", "value": format!("{} it/s", SPEED[step])},
               {"label": "Shards", "value": format!("{}/12", 2 + step)}],
        "parts": [{"type": "line", "points": LOSS[..=step].to_vec(), "unit": "loss"}],
        "open": Paths::root().join("decider-v4.html").to_string_lossy().into_owned(),
        "started": now - 22.0 * 60.0, "expanded": true}
}

pub fn run(out: &str) {
    let r = Render::new(out);
    let _ = std::fs::create_dir_all(&r.out);
    clear_folder();
    let page = Paths::root().join("jarvis-cleanup.html");
    let long_page = Paths::root().join("jarvis-voice-cleanup-before-and-after-every-take.html");
    for p in [&page, &long_page, &Paths::root().join("decider-v4.html")] {
        let _ = std::fs::write(p, "<html></html>");
    }
    let mut t = 0.6;
    let mut ids: Vec<String> = vec![];
    for (id, o) in states(&page.to_string_lossy(), &long_page.to_string_lossy()) {
        let (r1, id1, o1) = (r.clone(), id.to_owned(), o);
        sys::after(t, move || put(&id1, o1));
        t += 1.8;
        let (r1, id1) = (r1.clone(), id.to_owned());
        sys::after(t, move || {
            r1.shot(&id1, Some(&[id1.as_str()]), None);
            report_rest(&id1);
            let _ = std::fs::remove_file(Paths::file(&id1));
        });
        t += 0.5;
        ids.push(id.to_owned());
    }
    // The series: written, then rewritten as the agent would, a quarter second apart.
    for step in 0..PROGRESS.len() - 1 {
        sys::after(t, move || put("running-series", series(step)));
        t += 0.25;
    }
    t += 1.4;
    let r1 = r.clone();
    sys::after(t, move || {
        r1.shot("running-series", Some(&["running-series"]), None);
        report_rest("running-series");
        // One more rewrite while open: only the newest step draws, the bars grow from where they were.
        put("running-series", series(PROGRESS.len() - 1));
        sys::after(0.05, || {
            if let Some(c) = app().ctl("running-series") {
                println!("glance: running-series 0.05 s after the rewrite, moving: {}", c.animated_layers().join(", "));
            }
        });
        r1.sheet("anim-series-update", &["running-series"], None, &[0.03, 0.08, 0.14, 0.22, 0.32, 0.5, 0.9], Some(4));
    });
    t += 3.0;
    let r1 = r.clone();
    sys::after(t, move || {
        r1.shot("running-series-after", Some(&["running-series"]), None);
        report_rest("running-series");
        IslandFile::patch("running-series", |o| {
            o.insert("expanded".into(), Value::Bool(false));
        });
    });
    t += 1.2;
    let r1 = r.clone();
    sys::after(t, move || {
        r1.shot("running-series-pill", Some(&["running-series"]), None);
        for l in r1.log.borrow().iter() {
            println!("{l}");
        }
        clear_folder();
        std::process::exit(0);
    });
    let _: Rc<Render> = r;
}

/// What still animates on the island once its arrival settled (a running island's sign of work aside).
fn report_rest(id: &str) {
    let Some(c) = app().ctl(id) else { return println!("glance: {id}: not on screen") };
    let busy: Vec<String> = c.animated_layers().into_iter().filter(|l| !l.contains("work")).collect();
    let texts = c.body_texts().join(" | ");
    println!("glance: {id}: at rest {}; body: {texts}", if busy.is_empty() { "nothing animating".to_owned() } else { busy.join(", ") });
}
