//! The viewer island resizes like a window (bug note 2026-11-07T01-40-23, the user: "this island I cannot resize it, which
//! is a problem"): a drag on its edges or corners moves those edges, the content reflows to the new size, and the size
//! is kept for that file (`viewer-size.json`, beside the zoom's memory) for the next time it opens. Pure maths here;
//! the controller (`mac::island`) drives it.

use std::path::{Path, PathBuf};

use serde_json::Value;

use super::geometry::{Point, Rect, Size};
use super::{IslandFile, Json, Paths};

/// How far in from a side or the bottom a press grabs that edge (points; inside the 16 pt margin around the content).
pub const GRIP: f64 = 8.0;
/// The top edge's grip: thinner, the title row is for carrying the island.
pub const TOP_GRIP: f64 = 5.0;
/// A corner takes this much of both edges.
pub const CORNER: f64 = 18.0;
/// Files whose size is remembered (the oldest go first).
pub const KEEP: usize = 200;

/// The edges a press grabbed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Edges {
    pub left: bool,
    pub right: bool,
    pub top: bool,
    pub bottom: bool,
}

impl Edges {
    pub const fn any(&self) -> bool {
        self.left || self.right || self.top || self.bottom
    }
}

/// The edges under `p` in an island of `size` (island coordinates: origin top-left, y down), if any. Near a corner
/// both of its edges (a bottom corner: its whole `CORNER` square); the top-right corner is the close button's, so only its right edge counts there.
pub fn edges_at(p: Point, size: Size) -> Option<Edges> {
    if p.x < 0.0 || p.y < 0.0 || p.x > size.w || p.y > size.h {
        return None;
    }
    let (from_l, from_r, from_t, from_b) = (p.x, size.w - p.x, p.y, size.h - p.y);
    let (near_l, near_r, near_t, near_b) = (from_l <= GRIP, from_r <= GRIP, from_t <= TOP_GRIP, from_b <= GRIP);
    let (corner_l, corner_r, corner_b) = (from_l <= CORNER, from_r <= CORNER, from_b <= CORNER);
    let e = Edges {
        left: near_l || corner_l && (near_t || corner_b),
        right: near_r || corner_r && (near_t || corner_b),
        // The close button sits in the top-right corner: that corner resizes sideways only.
        top: near_t && !corner_r,
        // A bottom corner takes the whole square of CORNER points.
        bottom: near_b || corner_b && (corner_l || corner_r),
    };
    e.any().then_some(e)
}

/// The island's rect after its grabbed `edges` moved by `(dx, dy)` (screen points, y up), from `start`. The other edges
/// stay put; it never gets smaller than `min` or crosses out of `bounds` (the display's visible area under the menu
/// bar), whichever edge was grabbed.
pub fn resize(start: Rect, edges: Edges, dx: f64, dy: f64, min: Size, bounds: Rect) -> Rect {
    let (mut l, mut r, mut b, mut t) = (start.min_x(), start.max_x(), start.min_y(), start.max_y());
    if edges.left {
        l = (l + dx).max(bounds.min_x()).min(r - min.w);
    }
    if edges.right {
        r = (r + dx).min(bounds.max_x()).max(l + min.w);
    }
    if edges.bottom {
        b = (b + dy).max(bounds.min_y()).min(t - min.h);
    }
    if edges.top {
        t = (t + dy).min(bounds.max_y()).max(b + min.h);
    }
    Rect::new(l.round(), b.round(), (r - l).round(), (t - b).round())
}

// ---- page's own size (13 Nov, the user on a job-ad-beside-CV page at 920 pt: "look how small that is") ----

/// The size an HTML page asks the viewer for, in CSS px at zoom 1 (= points): the page's own area, the web view,
/// without the island's title row and margins. `<meta name="heimdall-viewer" content="width=1500, height=1000">`;
/// either may be left out.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PageAsk {
    pub w: Option<f64>,
    pub h: Option<f64>,
}

impl PageAsk {
    pub fn is_empty(&self) -> bool {
        self.w.is_none() && self.h.is_none()
    }
}

/// How much of a page's start is read for its `<meta name="heimdall-viewer">` (it belongs in `<head>`).
pub const ASK_SCAN: usize = 64 * 1024;
/// The largest number a page may ask (a page asking more gets its display's room).
pub const ASK_MAX: f64 = 10_000.0;

/// The size `html` asks for (`PageAsk`), from the first `<meta name="heimdall-viewer" content="…">` in its first
/// `ASK_SCAN` bytes; `None` when it asks nothing. Names and keys are case-blind, quotes single, double or none;
/// a number that is not finite, under 1 or over `ASK_MAX` counts as not given.
pub fn page_ask(html: &str) -> Option<PageAsk> {
    let pairs = viewer_meta(html)?;
    let mut ask = PageAsk::default();
    for (k, v) in pairs {
        let n = v.trim_end_matches("px").trim_end_matches("pt").parse::<f64>().ok().filter(|n| n.is_finite() && *n >= 1.0 && *n <= ASK_MAX);
        match k.as_str() {
            "width" => ask.w = n.or(ask.w),
            "height" => ask.h = n.or(ask.h),
            _ => {}
        }
    }
    (!ask.is_empty()).then_some(ask)
}

/// The `key=value` pairs of the first `<meta name="heimdall-viewer" content="…">` in `html`'s first `ASK_SCAN` bytes
/// (keys lowercased, both trimmed; `width = 1500` reads as `width=1500`; pairs part at commas, semicolons or spaces);
/// `None` when there is no such tag. The page's size (`page_ask`) and its pinch (`page_pinch::page_pinch`) read it.
pub fn viewer_meta(html: &str) -> Option<Vec<(String, String)>> {
    let mut end = html.len().min(ASK_SCAN);
    while !html.is_char_boundary(end) {
        end -= 1;
    }
    let head = &html[..end];
    let lower = head.to_ascii_lowercase();
    let mut from = 0;
    while let Some(i) = lower[from..].find("<meta") {
        let start = from + i + 5;
        let close = lower[start..].find('>').map(|j| start + j).unwrap_or(lower.len());
        from = close;
        let attrs = tag_attrs(&head[start..close]);
        let name = attrs.iter().find(|(k, _)| k == "name").map(|(_, v)| v.trim().to_ascii_lowercase());
        if name.as_deref() != Some("heimdall-viewer") {
            continue;
        }
        let content = attrs.iter().find(|(k, _)| k == "content").map(|(_, v)| v.as_str()).unwrap_or("");
        let mut tight = content.to_owned();
        while tight.contains(" =") || tight.contains("= ") {
            tight = tight.replace(" =", "=").replace("= ", "=");
        }
        let pairs = tight
            .split([',', ';', ' ', '\t', '\n'])
            .filter_map(|part| part.split_once('='))
            .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned()))
            .collect();
        return Some(pairs);
    }
    None
}

/// The first `ASK_SCAN` bytes of `file`, as text (lossy).
pub fn page_head_of(file: &Path) -> Option<String> {
    use std::io::Read;
    let mut buf = Vec::with_capacity(ASK_SCAN);
    std::fs::File::open(file).ok()?.take(ASK_SCAN as u64).read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// The attributes of one tag's inside (`name="x" content='y' z=w`), keys lowercased.
fn tag_attrs(s: &str) -> Vec<(String, String)> {
    let b = s.as_bytes();
    let (mut i, mut out) = (0, vec![]);
    while i < b.len() {
        while i < b.len() && (b[i].is_ascii_whitespace() || b[i] == b'/') {
            i += 1;
        }
        let k0 = i;
        while i < b.len() && !b[i].is_ascii_whitespace() && b[i] != b'=' && b[i] != b'/' {
            i += 1;
        }
        let key = s[k0..i].to_ascii_lowercase();
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        let mut val = String::new();
        if i < b.len() && b[i] == b'=' {
            i += 1;
            while i < b.len() && b[i].is_ascii_whitespace() {
                i += 1;
            }
            if i < b.len() && (b[i] == b'"' || b[i] == b'\'') {
                let q = b[i];
                let v0 = i + 1;
                i = v0;
                while i < b.len() && b[i] != q {
                    i += 1;
                }
                val = s[v0..i].to_owned();
                i += 1;
            } else {
                let v0 = i;
                while i < b.len() && !b[i].is_ascii_whitespace() {
                    i += 1;
                }
                // `content=width=1500/>`: the slash closes the tag, it is not the value's.
                val = s[v0..i].trim_end_matches('/').to_owned();
            }
        }
        if !key.is_empty() {
            out.push((key, val));
        }
    }
    out
}

/// The size `file` asks for, read from its first `ASK_SCAN` bytes (`page_ask`).
pub fn page_ask_of(file: &Path) -> Option<PageAsk> {
    page_ask(&page_head_of(file)?)
}

/// The widest and tallest a viewer's content may be on a `screen` of this size: what the page asks, else the
/// kind's own limits (`default_w` × `default_h`), never more than the screen's room for content (`screen` less 16 pt
/// across and `extra` around the content: the island's margins, title row and action rows).
pub fn content_limits(ask: Option<PageAsk>, default_w: f64, default_h: f64, screen: Size, extra: Size) -> Size {
    let ask = ask.unwrap_or_default();
    let room = Size::new((screen.w - 16.0 - extra.w).max(1.0), (screen.h - 16.0 - extra.h).max(1.0));
    Size::new(ask.w.map_or(default_w, |w| w.min(room.w)), ask.h.map_or(default_h, |h| h.min(room.h)))
}

/// The viewer's size for a page that asks for one: the content's island fitted to the display's `room` (under the
/// menu bar, 8 pt in) and never under `min`. A page that asks nothing keeps its content's size as before.
pub fn asked_island(natural: Size, ask: Option<PageAsk>, min: Size, room: Size) -> Size {
    match ask {
        Some(_) => fit(natural, min, room),
        None => natural,
    }
}

/// Whether the size the user gave a file by hand still stands: always for a page that asks nothing; for one that asks,
/// only while it asks what it asked when he dragged (a newer request is new information from the page's author;
/// a drag made against no request, or before requests were kept, yields to one).
pub fn hand_size_stands(kept: Option<&Kept>, ask: Option<PageAsk>) -> Option<Size> {
    let k = kept?;
    match ask {
        None => Some(k.size),
        Some(a) => (k.asked == Some(a)).then_some(k.size),
    }
}

/// Where an open viewer of `size` sits with no place of its own on the display `sf` (y up) whose islands stay under
/// `row_top`: centred, 20 pt high; one too big for that is pulled down under `row_top` and kept 8 pt in from the
/// sides and the bottom. Today's sizes (at most 78 % of the display tall) never move.
pub fn centred_origin(size: Size, sf: Rect, row_top: f64) -> (f64, f64) {
    let (mut x, mut y) = (sf.mid_x() - size.w / 2.0, sf.mid_y() - size.h / 2.0 + 20.0);
    let top = sf.max_y().min(row_top);
    if y + size.h > top {
        y = (top - size.h).max(sf.min_y() + 8.0);
    }
    if x < sf.min_x() + 8.0 {
        x = sf.min_x() + 8.0;
    }
    (x.round(), y.round())
}

// ---- end page's own size ----

/// A remembered size on a display: never smaller than `min`, never larger than `max` (the display's room).
pub fn fit(size: Size, min: Size, max: Size) -> Size {
    let max = Size::new(max.w.max(min.w), max.h.max(min.h));
    Size::new(size.w.clamp(min.w, max.w).round(), size.h.clamp(min.h, max.h).round())
}

impl Paths {
    /// The viewer's size per file (`viewer_size::Memory`).
    pub fn viewer_size() -> PathBuf {
        Self::root().join("viewer-size.json")
    }
}

/// One file's kept size, and what its page asked (`PageAsk`) when the user gave it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Kept {
    pub size: Size,
    pub asked: Option<PageAsk>,
}

/// The size the user gave each file's viewer, newest last.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Memory {
    entries: Vec<(String, Kept)>,
}

impl Memory {
    /// The default file (`FLOATING_ISLANDS_DIR` moves it with the rest of the folder).
    pub fn default_path() -> PathBuf {
        Paths::viewer_size()
    }

    /// Reads `path`; a missing or broken file is an empty memory.
    pub fn load(path: &Path) -> Memory {
        let Ok(data) = std::fs::read(path) else { return Memory::default() };
        let Ok(Value::Object(o)) = serde_json::from_slice::<Value>(&data) else { return Memory::default() };
        let mut m = Memory::default();
        if let Some(Value::Array(a)) = o.get("files") {
            for e in a {
                let (Some(p), Some(w), Some(h)) = (e.get("path").and_then(Value::as_str), e.get("width").and_then(Value::as_f64), e.get("height").and_then(Value::as_f64)) else {
                    continue;
                };
                if w.is_finite() && h.is_finite() && w > 0.0 && h > 0.0 && !p.is_empty() {
                    let num = |k: &str| e.get(k).and_then(Value::as_f64).filter(|n| n.is_finite() && *n > 0.0);
                    let asked = PageAsk { w: num("askedWidth"), h: num("askedHeight") };
                    m.set_kept(p, Kept { size: Size::new(w, h), asked: (!asked.is_empty()).then_some(asked) });
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
        let files: Vec<Value> = self
            .entries
            .iter()
            .map(|(p, k)| {
                let mut o = crate::jobj! {"path": p.clone(), "width": k.size.w.round(), "height": k.size.h.round()};
                if let Some(a) = k.asked {
                    if let Some(w) = a.w {
                        o.insert("askedWidth".into(), Value::from(w));
                    }
                    if let Some(h) = a.h {
                        o.insert("askedHeight".into(), Value::from(h));
                    }
                }
                o.into()
            })
            .collect();
        let mut o = Json::new();
        o.insert("files".into(), Value::Array(files));
        IslandFile::write_atomic(&o, path)
    }

    pub fn get(&self, file: &str) -> Option<Size> {
        self.kept(file).map(|k| k.size)
    }

    pub fn kept(&self, file: &str) -> Option<Kept> {
        self.entries.iter().find(|(p, _)| p == file).map(|(_, k)| *k)
    }

    /// Remember `size` for `file` (made against no page request); the oldest go past `KEEP`.
    pub fn set(&mut self, file: &str, size: Size) {
        self.set_kept(file, Kept { size, asked: None });
    }

    /// Remember `kept` for `file`; the oldest go past `KEEP`.
    pub fn set_kept(&mut self, file: &str, kept: Kept) {
        self.entries.retain(|(p, _)| p != file);
        self.entries.push((file.to_owned(), kept));
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

/// The size remembered for `file` in the memory at `store`.
pub fn remembered(store: &Path, file: &Path) -> Option<Size> {
    Memory::load(store).get(&file.to_string_lossy())
}

/// The size and request remembered for `file` in the memory at `store`.
pub fn remembered_kept(store: &Path, file: &Path) -> Option<Kept> {
    Memory::load(store).kept(&file.to_string_lossy())
}

/// Remember `size` for `file` in the memory at `store`; nothing is written when it is already so.
pub fn remember(store: &Path, file: &Path, size: Size) -> std::io::Result<bool> {
    remember_asked(store, file, size, None)
}

/// Remember `size` for `file`, given while its page asked `asked`; nothing is written when it is already so.
pub fn remember_asked(store: &Path, file: &Path, size: Size, asked: Option<PageAsk>) -> std::io::Result<bool> {
    let key = file.to_string_lossy();
    let mut m = Memory::load(store);
    let kept = Kept { size, asked };
    if m.kept(&key) == Some(kept) {
        return Ok(false);
    }
    m.set_kept(&key, kept);
    m.save(store).map(|_| true)
}
