//! The viewer's zoom (bug note 2026-11-04T19-07-48): pinch, ⌘+ / ⌘− / ⌘0, and fit-to-width when the viewer's width
//! changes, remembered per file. Pure maths and the small JSON memory; the AppKit side is `mac::viewer`.
//!
//! Two numbers: the **zoom** the user chose (1 = as the viewer first laid the file out, remembered per path) and the
//! **fit** factor, the viewport width over the width the content was first laid out at (1 until the viewer's width
//! changes). What the view shows is `zoom × fit`, so a wider viewer scales its content up instead of reflowing it.

use std::path::{Path, PathBuf};

use serde_json::Value;

use super::file::{IslandFile, Paths};
use super::Json;

/// The zoom range for content that can sit smaller than its frame (text, PDF, web pages).
pub const MIN: f64 = 0.5;
pub const MAX: f64 = 4.0;

/// ⌘+ / ⌘− walk these, as a browser does.
pub const STEPS: [f64; 14] = [0.5, 0.67, 0.75, 0.8, 0.9, 1.0, 1.1, 1.25, 1.5, 1.75, 2.0, 2.5, 3.0, 4.0];

/// How many files the memory keeps (the least recently zoomed go first).
pub const KEEP: usize = 200;

const EPS: f64 = 0.005;

/// What a zoom keeps within: text, PDF and pages zoom out to half; a picture never goes below its fitted size.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Range {
    pub min: f64,
    pub max: f64,
}

impl Range {
    pub const CONTENT: Range = Range { min: MIN, max: MAX };
    pub const PICTURE: Range = Range { min: 1.0, max: MAX };

    /// `z` within the range; anything not a finite positive number is 1.
    pub fn clamp(self, z: f64) -> f64 {
        if !z.is_finite() || z <= 0.0 {
            return 1.0;
        }
        z.clamp(self.min, self.max)
    }
}

/// A zoom key: ⌘= or ⌘+ in, ⌘− out, ⌘0 back to 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    In,
    Out,
    Reset,
}

/// The zoom key for a key-down: `chars` without modifiers (shift still applies), with ⌘ and without ⌃ or ⌥.
pub fn key_for(chars: &str, command: bool, control_or_option: bool) -> Option<Key> {
    if !command || control_or_option {
        return None;
    }
    match chars {
        "=" | "+" => Some(Key::In),
        "-" | "_" | "\u{2212}" => Some(Key::Out),
        "0" => Some(Key::Reset),
        _ => None,
    }
}

/// The next step above `z` (the top of the range at most).
pub fn step_in(z: f64, r: Range) -> f64 {
    let z = r.clamp(z);
    STEPS.iter().copied().find(|s| *s > z + EPS && *s <= r.max + EPS).unwrap_or(r.max)
}

/// The next step below `z` (the bottom of the range at least).
pub fn step_out(z: f64, r: Range) -> f64 {
    let z = r.clamp(z);
    STEPS.iter().rev().copied().find(|s| *s < z - EPS && *s >= r.min - EPS).unwrap_or(r.min)
}

/// The zoom after a key.
pub fn apply(z: f64, key: Key, r: Range) -> f64 {
    match key {
        Key::In => step_in(z, r),
        Key::Out => step_out(z, r),
        Key::Reset => 1.0,
    }
}

/// Fit-to-width: the viewport over the width the content was laid out at (1 when either is unknown).
pub fn fit_width(natural: f64, viewport: f64) -> f64 {
    if !(natural.is_finite() && viewport.is_finite()) || natural <= 0.0 || viewport <= 0.0 {
        return 1.0;
    }
    viewport / natural
}

/// What the view shows: the chosen zoom times the fit.
pub fn effective(zoom: f64, fit: f64) -> f64 {
    zoom * fit
}

/// The chosen zoom back from what the view shows (after a pinch), within the range.
pub fn user_zoom(effective: f64, fit: f64, r: Range) -> f64 {
    if !fit.is_finite() || fit <= 0.0 {
        return r.clamp(effective);
    }
    r.clamp(effective / fit)
}

/// The width text lays out at, in its own points, at magnification `m`: its natural width, or wider when zoomed out
/// so it still fills the viewport (zoomed in, the lines keep their breaks and the view pans, as on a map).
pub fn layout_width(natural: f64, viewport: f64, m: f64) -> f64 {
    if !m.is_finite() || m <= 0.0 {
        return natural;
    }
    natural.max(viewport / m)
}

// MARK: Pictures (bug note 2026-11-10T10-52-07)
//
// A tall picture (a long screenshot, a before/after strip) opens fitted to the viewer's width and scrolls, instead of
// shrunk to the viewer's height; zoom 1 is that fit, zoom below 1 shows more of it, down to the whole picture.

/// The lowest a picture's zoom goes, however tall it is (below this a strip is a sliver).
pub const PICTURE_FLOOR: f64 = 0.1;

/// A picture at zoom 1 in a view `view_w` × `view_h` (points): the document it sits in and the zoom that shows it
/// whole.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PictureFit {
    /// The document: as wide as the view, at least as tall, taller when the picture is.
    pub doc_w: f64,
    pub doc_h: f64,
    /// The picture's width in the document: the view's width, never more than its own points unless the view's
    /// height needs it (a small picture is not blown up; one that fits whole fills as before).
    pub shown_w: f64,
    /// The zoom that shows the whole document (1 when it already fits).
    pub whole: f64,
}

/// Where a picture of `nat_w` × `nat_h` points sits at zoom 1 in a view `view_w` × `view_h`.
pub fn picture_fit(nat_w: f64, nat_h: f64, view_w: f64, view_h: f64) -> PictureFit {
    let ok = |v: f64| v.is_finite() && v > 0.0;
    if !(ok(nat_w) && ok(nat_h) && ok(view_w) && ok(view_h)) {
        return PictureFit { doc_w: view_w.max(0.0), doc_h: view_h.max(0.0), shown_w: view_w.max(0.0), whole: 1.0 };
    }
    let aspect = nat_h / nat_w;
    let shown_w = view_w.min(nat_w.max(view_h / aspect));
    // Within a point of the view (its height was floored to whole points) it fits: no hairline scroll.
    let tall = shown_w * aspect;
    let doc_h = if tall <= view_h + 1.0 { view_h } else { tall };
    PictureFit { doc_w: view_w, doc_h, shown_w, whole: (view_h / doc_h).min(1.0) }
}

/// A picture's zoom range: in to 4×, out to the whole picture (1 when it already fits whole).
pub fn picture_range(whole: f64) -> Range {
    let min = if whole.is_finite() { whole.clamp(PICTURE_FLOOR, 1.0) } else { 1.0 };
    Range { min, max: MAX }
}

/// The content size a picture of `nat_w` × `nat_h` points asks for within `max_w` × `max_h`: its width (never more
/// than its own points), and its height at that width, capped by `max_h` (the rest scrolls).
pub fn picture_content(nat_w: f64, nat_h: f64, max_w: f64, max_h: f64) -> (f64, f64) {
    let k = 1f64.min(max_w / nat_w);
    ((nat_w * k).floor().max(240.0), (nat_h * k).min(max_h).floor().max(60.0))
}

/// The most pixels a picture is loaded at (about 190 MB decoded).
pub const PICTURE_MAX_PIXELS: f64 = 48.0e6;

/// The long side a picture of `w` × `h` pixels is loaded at: its own, so a tall strip fitted to the width (and zoomed
/// in) stays sharp, unless that is more than `PICTURE_MAX_PIXELS`; never under 2400 (the old fixed size).
pub fn picture_load_side(w: f64, h: f64) -> usize {
    if !(w.is_finite() && h.is_finite()) || w <= 0.0 || h <= 0.0 {
        return 2400;
    }
    let s = (PICTURE_MAX_PIXELS / (w * h)).sqrt().min(1.0);
    (w.max(h) * s).floor().max(2400.0) as usize
}

/// ⌘-scroll: the zoom factor for one scroll event's vertical delta (as a browser: away from you zooms in).
/// `precise`: a trackpad's points; else a wheel's lines.
pub fn scroll_factor(dy: f64, precise: bool) -> f64 {
    if !dy.is_finite() {
        return 1.0;
    }
    let k = if precise { 0.006 } else { 0.1 };
    (dy * k).exp().clamp(0.5, 2.0)
}

/// Double-click on a picture: at its fit, in to 2× there; zoomed either way, back to the fit.
pub fn double_click_zoom(z: f64) -> f64 {
    if same(z, 1.0) {
        2.0
    } else {
        1.0
    }
}

/// Whether two zooms are the same for saving (a pinch never lands exactly).
pub fn same(a: f64, b: f64) -> bool {
    (a - b).abs() < EPS
}

/// Per-file zoom memory: `<islands folder>/viewer-zoom.json`, beside `state.json`. Most recently zoomed last.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Memory {
    entries: Vec<(String, f64)>,
}

impl Memory {
    /// The default file (`FLOATING_ISLANDS_DIR` moves it with the rest of the folder).
    pub fn default_path() -> PathBuf {
        Paths::viewer_zoom()
    }

    /// Reads `path`; a missing or broken file is an empty memory.
    pub fn load(path: &Path) -> Memory {
        let Ok(data) = std::fs::read(path) else { return Memory::default() };
        let Ok(Value::Object(o)) = serde_json::from_slice::<Value>(&data) else { return Memory::default() };
        let mut m = Memory::default();
        if let Some(Value::Array(a)) = o.get("files") {
            for e in a {
                let (Some(p), Some(z)) = (e.get("path").and_then(Value::as_str), e.get("zoom").and_then(Value::as_f64)) else { continue };
                if z.is_finite() && z > 0.0 && !p.is_empty() {
                    m.set(p, z);
                }
            }
        }
        m
    }

    /// Writes `path` (atomically, its folder made if missing).
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d)?;
        }
        let files: Vec<Value> = self.entries.iter().map(|(p, z)| crate::jobj! {"path": p.clone(), "zoom": (z * 1000.0).round() / 1000.0}.into()).collect();
        let mut o = Json::new();
        o.insert("files".into(), Value::Array(files));
        IslandFile::write_atomic(&o, path)
    }

    /// The zoom remembered for `file` (1 when none).
    pub fn get(&self, file: &str) -> f64 {
        self.entries.iter().find(|(p, _)| p == file).map(|(_, z)| *z).unwrap_or(1.0)
    }

    /// Remember `z` for `file` (1 forgets it); the oldest go past `KEEP`.
    pub fn set(&mut self, file: &str, z: f64) {
        self.entries.retain(|(p, _)| p != file);
        if !same(z, 1.0) {
            self.entries.push((file.to_owned(), z));
        }
        let n = self.entries.len();
        if n > KEEP {
            self.entries.drain(0..n - KEEP);
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// The zoom remembered for `file` in the memory at `store`.
pub fn remembered(store: &Path, file: &Path) -> f64 {
    Memory::load(store).get(&file.to_string_lossy())
}

/// Remember `z` for `file` in the memory at `store`; nothing is written when it is already so.
pub fn remember(store: &Path, file: &Path, z: f64) -> std::io::Result<bool> {
    let key = file.to_string_lossy();
    let mut m = Memory::load(store);
    if same(m.get(&key), z) {
        return Ok(false);
    }
    m.set(&key, z);
    m.save(store).map(|_| true)
}
