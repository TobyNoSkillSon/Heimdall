//! The viewer's zoom (bug note 2026-11-04T19-07-48): range, ⌘ keys, steps, fit-to-width, layout width and the per-file
//! memory.

use std::path::{Path, PathBuf};

use islands::model::zoom::{self, Key, Memory, Range};

struct Dir(PathBuf);
impl Dir {
    fn new(tag: &str) -> Dir {
        let d = std::env::temp_dir().join(format!("zoom-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        Dir(d)
    }
}
impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn near(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9
}

#[test]
fn zoom_stays_in_range_and_junk_is_one() {
    let r = Range::CONTENT;
    assert_eq!(r.clamp(0.1), 0.5);
    assert_eq!(r.clamp(9.0), 4.0);
    assert_eq!(r.clamp(1.3), 1.3);
    for junk in [f64::NAN, f64::INFINITY, 0.0, -2.0] {
        assert_eq!(r.clamp(junk), 1.0, "{junk}");
    }
    // A picture never goes below its fitted size.
    assert_eq!(Range::PICTURE.clamp(0.6), 1.0);
    assert_eq!(Range::PICTURE.clamp(5.0), 4.0);
}

#[test]
fn command_keys_map_to_zoom() {
    assert_eq!(zoom::key_for("=", true, false), Some(Key::In));
    assert_eq!(zoom::key_for("+", true, false), Some(Key::In));
    assert_eq!(zoom::key_for("-", true, false), Some(Key::Out));
    assert_eq!(zoom::key_for("_", true, false), Some(Key::Out));
    assert_eq!(zoom::key_for("0", true, false), Some(Key::Reset));
    // Without ⌘, or with ⌃ / ⌥, the keys are the text's.
    assert_eq!(zoom::key_for("=", false, false), None);
    assert_eq!(zoom::key_for("-", true, true), None);
    assert_eq!(zoom::key_for("a", true, false), None);
    assert_eq!(zoom::key_for("", true, false), None);
}

#[test]
fn steps_walk_like_a_browser() {
    let r = Range::CONTENT;
    assert_eq!(zoom::step_in(1.0, r), 1.1);
    assert_eq!(zoom::step_in(1.1, r), 1.25);
    assert_eq!(zoom::step_out(1.0, r), 0.9);
    assert_eq!(zoom::step_out(0.9, r), 0.8);
    // From between steps (after a pinch), the next step either way.
    assert_eq!(zoom::step_in(1.37, r), 1.5);
    assert_eq!(zoom::step_out(1.37, r), 1.25);
    // A pinch that landed a hair off a step still moves a whole step.
    assert_eq!(zoom::step_in(1.499, r), 1.75);
    // The ends hold.
    assert_eq!(zoom::step_in(4.0, r), 4.0);
    assert_eq!(zoom::step_out(0.5, r), 0.5);
    // A picture stops at its fit going out.
    assert_eq!(zoom::step_out(1.1, Range::PICTURE), 1.0);
    assert_eq!(zoom::step_out(1.0, Range::PICTURE), 1.0);
}

#[test]
fn every_step_is_reachable_and_in_order() {
    let r = Range::CONTENT;
    let mut z = r.min;
    let mut seen = vec![z];
    while z < r.max {
        let n = zoom::step_in(z, r);
        assert!(n > z, "{z} -> {n}");
        z = n;
        seen.push(z);
    }
    assert_eq!(seen, zoom::STEPS.to_vec());
    let mut back = vec![z];
    while z > r.min {
        z = zoom::step_out(z, r);
        back.push(z);
    }
    back.reverse();
    assert_eq!(back, zoom::STEPS.to_vec());
}

#[test]
fn apply_follows_the_key() {
    let r = Range::CONTENT;
    assert_eq!(zoom::apply(1.0, Key::In, r), 1.1);
    assert_eq!(zoom::apply(1.0, Key::Out, r), 0.9);
    assert_eq!(zoom::apply(2.7, Key::Reset, r), 1.0);
}

#[test]
fn fit_to_width_scales_with_the_viewport() {
    // Laid out at 700 points, shown at 1050: everything half again as big.
    assert!(near(zoom::fit_width(700.0, 1050.0), 1.5));
    assert!(near(zoom::fit_width(700.0, 350.0), 0.5));
    assert!(near(zoom::fit_width(700.0, 700.0), 1.0));
    for (n, v) in [(0.0, 500.0), (500.0, 0.0), (f64::NAN, 500.0), (-1.0, 500.0)] {
        assert_eq!(zoom::fit_width(n, v), 1.0, "{n} {v}");
    }
}

#[test]
fn effective_and_user_zoom_round_trip() {
    let r = Range::CONTENT;
    assert!(near(zoom::effective(1.5, 1.2), 1.8));
    assert!(near(zoom::user_zoom(1.8, 1.2, r), 1.5));
    // A pinch past the range keeps the chosen zoom in it.
    assert_eq!(zoom::user_zoom(10.0, 1.0, r), 4.0);
    // An unknown fit counts as 1.
    assert!(near(zoom::user_zoom(2.0, 0.0, r), 2.0));
}

#[test]
fn text_keeps_its_lines_zoomed_in_and_fills_zoomed_out() {
    // Zoomed in: the natural width (the view pans).
    assert_eq!(zoom::layout_width(700.0, 700.0, 2.0), 700.0);
    // Zoomed out to half: twice as wide, so the text still fills the viewport.
    assert_eq!(zoom::layout_width(700.0, 700.0, 0.5), 1400.0);
    // Fit to a wider viewport at zoom 1: the natural width, scaled up.
    assert_eq!(zoom::layout_width(700.0, 1050.0, 1.5), 700.0);
    assert_eq!(zoom::layout_width(700.0, 700.0, f64::NAN), 700.0);
}

#[test]
fn memory_remembers_per_file_and_forgets_one() {
    let mut m = Memory::default();
    assert_eq!(m.get("/a.md"), 1.0);
    m.set("/a.md", 1.5);
    m.set("/b.html", 0.75);
    assert_eq!(m.get("/a.md"), 1.5);
    assert_eq!(m.get("/b.html"), 0.75);
    // Back to 1 forgets the file.
    m.set("/a.md", 1.0);
    assert_eq!(m.get("/a.md"), 1.0);
    assert_eq!(m.len(), 1);
}

#[test]
fn memory_keeps_the_most_recent() {
    let mut m = Memory::default();
    for i in 0..zoom::KEEP + 5 {
        m.set(&format!("/f{i}.md"), 2.0);
    }
    assert_eq!(m.len(), zoom::KEEP);
    assert_eq!(m.get("/f0.md"), 1.0);
    assert_eq!(m.get("/f4.md"), 1.0);
    assert_eq!(m.get("/f5.md"), 2.0);
    // Zooming an old file again makes it recent.
    m.set("/f5.md", 3.0);
    m.set("/new.md", 2.0);
    assert_eq!(m.get("/f5.md"), 3.0);
    assert_eq!(m.get("/f6.md"), 1.0);
}

#[test]
fn memory_saves_and_loads() {
    let d = Dir::new("mem");
    let store = d.0.join("sub").join("viewer-zoom.json");
    let mut m = Memory::default();
    m.set("/Users/x/Reports/a.md", 1.75);
    m.set("/Users/x/b.pdf", 0.5);
    m.save(&store).unwrap();
    let back = Memory::load(&store);
    assert_eq!(back, m);
    assert_eq!(back.get("/Users/x/Reports/a.md"), 1.75);
}

#[test]
fn memory_survives_missing_and_broken_files() {
    let d = Dir::new("broken");
    assert!(Memory::load(&d.0.join("none.json")).is_empty());
    let p = d.0.join("viewer-zoom.json");
    std::fs::write(&p, b"{not json").unwrap();
    assert!(Memory::load(&p).is_empty());
    std::fs::write(&p, br#"{"files":[{"path":"/a.md","zoom":2},{"path":"/b.md"},{"path":"","zoom":2},{"path":"/c.md","zoom":-1},{"path":"/d.md","zoom":1.5}]}"#).unwrap();
    let m = Memory::load(&p);
    assert_eq!(m.len(), 2);
    assert_eq!(m.get("/a.md"), 2.0);
    assert_eq!(m.get("/d.md"), 1.5);
}

#[test]
fn remember_writes_only_on_change() {
    let d = Dir::new("remember");
    let store = d.0.join("viewer-zoom.json");
    let f = Path::new("/Users/x/Reports/r.md");
    assert_eq!(zoom::remembered(&store, f), 1.0);
    // Already 1: nothing written.
    assert!(!zoom::remember(&store, f, 1.0).unwrap());
    assert!(!store.exists());
    assert!(zoom::remember(&store, f, 1.5).unwrap());
    assert_eq!(zoom::remembered(&store, f), 1.5);
    // A pinch a hair off the same zoom writes nothing.
    assert!(!zoom::remember(&store, f, 1.501).unwrap());
    assert!(zoom::remember(&store, f, 1.0).unwrap());
    assert_eq!(zoom::remembered(&store, f), 1.0);
    // Another file is its own.
    assert!(zoom::remember(&store, Path::new("/other.md"), 2.0).unwrap());
    assert_eq!(zoom::remembered(&store, f), 1.0);
}

#[test]
fn the_memory_sits_in_the_islands_folder() {
    assert!(Memory::default_path().ends_with("viewer-zoom.json"));
    assert_eq!(Memory::default_path().parent(), islands::model::Paths::state().parent());
}
