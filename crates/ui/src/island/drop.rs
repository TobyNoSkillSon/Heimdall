//! Drops onto the notch ("I just drag something to the notch and I can drop it to you").
//!
//! Platform-neutral: what a drag's pasteboard holds (`Snapshot`, read by `mac/drop`), what each item is
//! (`classify`), the island's word for it (`summary`: "1 image", "Link", "Folder"), and the drop's motion
//! (`Motion`: the notch swells and glows under a drag, then swallows the item with a soft bounce), a pure function
//! of time like the island's own. While the island shows more than the notch (speaking, a line, the conversation) the
//! same swell and glow belong to the island's own shape (`Swell`), never to a separate shape over the notch. The app turns a `Dropped` into the agent's message (`crates/app/src/drops.rs`).

use std::path::{Path, PathBuf};

use crate::island::object::geom::{clamp01, lerp, smooth, Display, Point, Shape};
use crate::island::object::spring::Spring;

/// The drop target's window title: the screen capture filters exclude it (`screen::EXCLUDED_WINDOW_TITLES`).
pub const WINDOW_TITLE: &str = "Heimdall Drop Target";

/// What a dropped item is: its icon, its word on the island, and how the agent gets it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DropKind {
    /// Sent as an image attachment, with its path.
    Image,
    /// The path, its duration and size.
    Video,
    Audio,
    /// The path and what is inside.
    Folder,
    /// The path, and its text inline when it is small.
    Document,
    /// Any other file: the path.
    File,
    Link,
    Text,
    /// Several items of different kinds (the island's word only).
    Mixed,
}

impl DropKind {
    /// SF Symbol for the island's confirmation and the drop's stand-in thumbnail.
    pub fn symbol(self) -> &'static str {
        match self {
            DropKind::Image => "photo.fill",
            DropKind::Video => "film.fill",
            DropKind::Audio => "waveform",
            DropKind::Folder => "folder.fill",
            DropKind::Document => "doc.text.fill",
            DropKind::File => "doc.fill",
            DropKind::Link => "link",
            DropKind::Text => "text.quote",
            DropKind::Mixed => "square.stack.fill",
        }
    }

    /// One item's word, and the plural for a count ("1 image", "3 images").
    fn words(self) -> (&'static str, &'static str) {
        match self {
            DropKind::Image => ("image", "images"),
            DropKind::Video => ("video", "videos"),
            DropKind::Audio => ("recording", "recordings"),
            DropKind::Folder => ("folder", "folders"),
            DropKind::Document => ("document", "documents"),
            DropKind::File => ("file", "files"),
            DropKind::Link => ("link", "links"),
            DropKind::Text => ("text", "texts"),
            DropKind::Mixed => ("item", "items"),
        }
    }

    /// The agent's tag for an item ("image", "folder").
    pub fn tag(self) -> &'static str {
        self.words().0
    }
}

/// One dropped thing. Files and folders carry `path`; links `url`; selected text `text`. An image that came as
/// data (no file) was saved first and carries the saved `path`, with `url` its web source when the drag had one.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DropItem {
    pub kind: DropKind,
    pub path: Option<PathBuf>,
    pub url: Option<String>,
    pub text: Option<String>,
}

impl DropItem {
    fn file(kind: DropKind, path: PathBuf) -> DropItem {
        DropItem { kind, path: Some(path), url: None, text: None }
    }
}

/// One drop: everything the user let go of at once, in the drag's order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dropped {
    pub items: Vec<DropItem>,
    /// Wall-clock milliseconds of the drop.
    pub at_ms: u64,
}

/// Image bytes a drag carried without a file (a picture dragged out of a browser or a screenshot's thumbnail).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageData {
    pub bytes: Vec<u8>,
    /// "png", "tiff", "jpg"…
    pub ext: &'static str,
}

/// What a drag's pasteboard holds, read once at the drop (`mac/drop/pasteboard.rs`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    /// File URLs as paths (Finder, a screenshot thumbnail, an app's document).
    pub files: Vec<PathBuf>,
    /// Other URLs (a link out of a browser or a mail).
    pub urls: Vec<String>,
    /// Plain text (a selection).
    pub text: Option<String>,
    pub image: Option<ImageData>,
}

/// Selected text longer than this (bytes) is kept as a file and sent by its path.
pub const TEXT_INLINE_MAX: usize = 16 * 1024;

/// Where drops that are not files yet are kept: `drops/` in Heimdall's data folder.
pub fn drops_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("HEIMDALL_DROPS_DIR") {
        return PathBuf::from(dir);
    }
    heimdall_home::data_dir().join("drops")
}

const IMAGE: &[&str] = &["png", "jpg", "jpeg", "gif", "heic", "heif", "tif", "tiff", "bmp", "webp", "avif"];
const VIDEO: &[&str] = &["mov", "mp4", "m4v", "avi", "mkv", "webm", "mpg", "mpeg", "3gp", "mts", "m2ts"];
const AUDIO: &[&str] = &["mp3", "m4a", "wav", "aif", "aiff", "aac", "flac", "ogg", "opus", "caf", "m4b", "alac"];
const DOCUMENT: &[&str] = &[
    "pdf", "txt", "md", "markdown", "rtf", "rtfd", "doc", "docx", "pages", "odt", "csv", "tsv", "json", "jsonl", "yaml",
    "yml", "toml", "xml", "html", "htm", "css", "js", "mjs", "ts", "tsx", "jsx", "rs", "py", "swift", "c", "h", "cc",
    "cpp", "hpp", "m", "mm", "go", "java", "kt", "rb", "php", "sh", "zsh", "bash", "sql", "log", "ini", "cfg", "conf",
    "tex", "xls", "xlsx", "numbers", "key", "ppt", "pptx", "epub", "ipynb", "lua", "zig", "cs", "scala", "r", "jl",
];

/// A file's kind from its extension (a folder is a folder, a package like `.app` or `.rtfd` is a file).
pub fn kind_of(path: &Path, is_dir: bool) -> DropKind {
    let ext = path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).unwrap_or_default();
    let ext = ext.as_str();
    if IMAGE.contains(&ext) {
        DropKind::Image
    } else if VIDEO.contains(&ext) {
        DropKind::Video
    } else if AUDIO.contains(&ext) {
        DropKind::Audio
    } else if DOCUMENT.contains(&ext) {
        DropKind::Document
    } else if is_dir && ext.is_empty() {
        DropKind::Folder
    } else if is_dir {
        // A bundle (.app, .xcodeproj, .photoslibrary): a folder on disk, one thing to the user.
        if ["app", "bundle", "framework", "xcodeproj", "xcworkspace", "photoslibrary", "playground"].contains(&ext) {
            DropKind::File
        } else {
            DropKind::Folder
        }
    } else if path.file_name().is_some_and(|n| ["Makefile", "Dockerfile", "README", "LICENSE"].contains(&n.to_string_lossy().as_ref())) {
        DropKind::Document
    } else {
        DropKind::File
    }
}

/// A file the drag handed over that will not stay: a screenshot's floating thumbnail lives in a temporary folder
/// until it moves to the Desktop, so its path would go stale. Such images are copied into the drops folder.
pub fn is_temporary(path: &Path) -> bool {
    let s = path.to_string_lossy();
    s.starts_with("/var/folders/") || s.starts_with("/private/var/folders/") || s.contains("/TemporaryItems/") || s.starts_with("/tmp/") || s.starts_with("/private/tmp/")
}

/// Whether text is one link and nothing else (a dragged address bar, a link copied as text).
pub fn as_link(text: &str) -> Option<String> {
    let t = text.trim();
    if t.is_empty() || t.contains(char::is_whitespace) || t.len() > 4096 {
        return None;
    }
    let lower = t.to_ascii_lowercase();
    ["https://", "http://", "mailto:", "ftp://"].iter().any(|p| lower.starts_with(p)).then(|| t.to_string())
}

/// The items a snapshot holds. Files win over everything (Finder also puts their names as text); otherwise links,
/// then image data (with its web source), then text. `save` keeps bytes as a file in the drops folder and returns
/// its path (`name` is the file name it should get); `is_dir` says whether a path is a folder.
pub fn classify(s: Snapshot, is_dir: &dyn Fn(&Path) -> bool, save: &mut dyn FnMut(&str, &[u8]) -> Option<PathBuf>, stamp: &str) -> Vec<DropItem> {
    let mut items = Vec::new();
    if !s.files.is_empty() {
        for (n, path) in s.files.into_iter().enumerate() {
            let dir = is_dir(&path);
            let kind = kind_of(&path, dir);
            let path = if kind == DropKind::Image && is_temporary(&path) {
                let name = format!("{stamp}-{}", path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| format!("{n}.png")));
                std::fs::read(&path).ok().and_then(|bytes| save(&name, &bytes)).unwrap_or(path)
            } else {
                path
            };
            items.push(DropItem::file(kind, path));
        }
        return items;
    }
    let links: Vec<String> = s.urls.iter().filter(|u| !u.trim().is_empty()).cloned().collect();
    if let Some(image) = s.image {
        if let Some(path) = save(&format!("{stamp}.{}", image.ext), &image.bytes) {
            // A picture out of a browser comes with its address: one image, its source beside it.
            items.push(DropItem { kind: DropKind::Image, path: Some(path), url: links.first().cloned(), text: None });
            return items;
        }
    }
    if !links.is_empty() {
        items.extend(links.into_iter().map(|u| DropItem { kind: DropKind::Link, path: None, url: Some(u), text: None }));
        return items;
    }
    if let Some(text) = s.text.filter(|t| !t.trim().is_empty()) {
        if let Some(url) = as_link(&text) {
            items.push(DropItem { kind: DropKind::Link, path: None, url: Some(url), text: None });
        } else if text.len() > TEXT_INLINE_MAX {
            match save(&format!("{stamp}.txt"), text.as_bytes()) {
                Some(path) => items.push(DropItem::file(DropKind::Document, path)),
                None => items.push(DropItem { kind: DropKind::Text, path: None, url: None, text: Some(text) }),
            }
        } else {
            items.push(DropItem { kind: DropKind::Text, path: None, url: None, text: Some(text) });
        }
    }
    items
}

/// The island's word for a drop and its icon: "1 image", "3 images", "Link", "Folder", "4 items".
pub fn summary(items: &[DropItem]) -> (DropKind, String) {
    let Some(first) = items.first() else { return (DropKind::Mixed, "Nothing".into()) };
    let kind = if items.iter().all(|i| i.kind == first.kind) { first.kind } else { DropKind::Mixed };
    let (one, many) = kind.words();
    let label = match (kind, items.len()) {
        (DropKind::Image, 1) => "1 image".to_string(),
        (_, 1) => capitalised(one),
        (_, n) => format!("{n} {many}"),
    };
    (kind, label)
}

fn capitalised(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
}

/// Where a drop goes (30 Oct 03:00, the user: "dropping should attach to the message being typed (or send it, like a
/// notch drop)").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    /// To the agent now, as a drop onto the notch (`UiAction::Dropped`).
    Agent,
    /// Into the open pull-down's composer: it rides with the message he is typing (`Event::ComposerAttach`).
    Composer,
}

/// A drop goes into the composer while the pull-down shows it and he is writing there (text in it, or the keyboard
/// in it); anywhere else, or with an empty unfocused composer, it goes to the agent as a notch drop does.
pub fn route(pulled_down: bool, composer_has_text: bool, composer_focused: bool) -> Route {
    if pulled_down && (composer_has_text || composer_focused) {
        Route::Composer
    } else {
        Route::Agent
    }
}

/// A drop's file stamp: local date and time to the millisecond, `2026-10-24-014501-000`.
pub fn stamp(ms: u64) -> String {
    #[cfg(target_os = "macos")]
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        let t = (ms / 1000) as libc::time_t;
        libc::localtime_r(&t, &mut tm);
        format!("{:04}-{:02}-{:02}-{:02}{:02}{:02}-{:03}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday, tm.tm_hour, tm.tm_min, tm.tm_sec, ms % 1000)
    }
    #[cfg(not(target_os = "macos"))]
    format!("{ms}")
}

// ---- The motion ------------------------------------------------------------------------------------------------

/// The notch's swell under a drag: this much wider and taller than the notch.
pub const SWELL: (f64, f64) = (26.0, 10.0);
/// The swell's spring (a touch of overshoot, like the island's growth).
pub const SWELL_SPRING: Spring = Spring { response: 0.36, damping: 0.78 };
/// The item's flight into the island (critically damped: it lands, it does not orbit).
pub const FLY: Spring = Spring { response: 0.40, damping: 1.0 };
/// The swallow: the shape takes the item with one soft bounce, then returns into the notch.
pub const GULP: Spring = Spring { response: 0.42, damping: 0.55 };
/// When the item is inside (its flight's progress at which the shape swallows).
pub const GULP_AT: f64 = 0.86;
/// How far the swallow kicks the shape (points per second, outward).
pub const GULP_KICK: f64 = 260.0;
/// The thumbnail's side while it is carried, in points.
pub const THUMB: f64 = 64.0;
/// Reduce Motion: fades only, this long.
pub const FADE: f64 = 0.22;
/// The confirmation ("1 image") is asked for this long after the drop (with Reduce Motion: at once).
pub const CONFIRM_AFTER: f64 = 0.34;

/// The drop target's phase.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Phase {
    Rest,
    /// A drag is over the target since `at` (the swell grows from where it was, `from`).
    Over { at: f64, from: (f64, f64, f64) },
    /// The drag left at `at`: back into the notch.
    Left { at: f64, from: (f64, f64, f64) },
    /// Let go at `at`, from `p` (island coordinates: x from the notch's centre line, y down from the top edge).
    Dropped { at: f64, p: Point, from: (f64, f64, f64) },
}

/// One frame of the drop target.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DropFrame {
    /// The black shape over the notch (the notch itself at rest: nothing drawn).
    pub shape: Shape,
    /// The glow round the shape, 0–1.
    pub glow: f64,
    /// The carried item: centre (island coordinates), scale of `THUMB`, opacity.
    pub thumb: Option<(Point, f64, f64)>,
    /// Anything drawn at all (the window is ordered out otherwise).
    pub visible: bool,
}

/// The swell and glow on the island itself while it shows ("the Dynamic Island as a whole isn't
/// lighting up and swelling a little; right now only the notch does"): the island's shape this much wider and taller,
/// lit round its edge, following the same springs as the notch's swell. One effect on one shape.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Swell {
    pub w: f64,
    pub h: f64,
    /// The swollen notch's concave joins: the island's are never narrower while it swells (it grows out of the notch
    /// with the notch's own, narrower joins; the hand-over keeps the swell's).
    pub flare: f64,
    /// The glow round the shape, 0–1.
    pub glow: f64,
}

impl Swell {
    /// `shape` swollen.
    pub fn on(&self, shape: Shape) -> Shape {
        Shape { w: shape.w + self.w, h: shape.h + self.h, flare: shape.flare.max(self.flare) }
    }
}

/// The drop target's motion: `Over`, `Left` and `Dropped` change it; `frame(t)` is pure.
#[derive(Clone, Debug)]
pub struct Motion {
    display: Display,
    reduce: bool,
    phase: Phase,
}

impl Motion {
    pub fn new(display: Display, reduce_motion: bool) -> Motion {
        Motion { display, reduce: reduce_motion, phase: Phase::Rest }
    }

    pub fn set_display(&mut self, display: Display) {
        self.display = display;
    }

    pub fn set_reduce_motion(&mut self, on: bool) {
        self.reduce = on;
    }

    fn rest(&self) -> Shape {
        let (w, h) = self.display.notch_size();
        let r = self.display.rest();
        Shape { w, h: if self.display.has_notch() { h } else { 0.0 }, flare: r.flare }
    }

    fn swollen(&self) -> Shape {
        let (w, h) = self.display.notch_size();
        Shape { w: w + SWELL.0, h: h + SWELL.1, flare: crate::island::object::geom::FLARE }
    }

    /// Where the shape is at `t`: (w, h, glow).
    fn state(&self, t: f64) -> (f64, f64, f64) {
        let f = self.frame(t);
        (f.shape.w, f.shape.h, f.glow)
    }

    /// A drag came over the target.
    pub fn over(&mut self, t: f64) {
        if matches!(self.phase, Phase::Over { .. }) {
            return;
        }
        let from = self.state(t);
        self.phase = Phase::Over { at: t, from };
    }

    /// The drag left without dropping (or the drop was refused).
    pub fn left(&mut self, t: f64) {
        if matches!(self.phase, Phase::Rest | Phase::Left { .. } | Phase::Dropped { .. }) {
            return;
        }
        let from = self.state(t);
        self.phase = Phase::Left { at: t, from };
    }

    /// Let go at `p` (island coordinates).
    pub fn dropped(&mut self, t: f64, p: Point) {
        let from = self.state(t);
        self.phase = Phase::Dropped { at: t, p, from };
    }

    /// Whether a drag is over the target now.
    pub fn is_over(&self) -> bool {
        matches!(self.phase, Phase::Over { .. })
    }

    /// When the confirmation should be asked for, after a drop at `at`.
    pub fn confirm_at(&self, at: f64) -> f64 {
        if self.reduce { at } else { at + CONFIRM_AFTER }
    }

    /// Whether anything moves at `t` (the clock runs only then).
    pub fn moving(&self, t: f64) -> bool {
        match self.phase {
            Phase::Rest => false,
            Phase::Over { at, from } => t - at < self.settle(from, true),
            Phase::Left { at, from } => t - at < self.settle(from, false),
            Phase::Dropped { at, .. } => t - at < self.drop_len(),
        }
    }

    /// Whether the target is at rest at `t` (nothing drawn, nothing moving): the window can go.
    pub fn at_rest(&self, t: f64) -> bool {
        match self.phase {
            Phase::Rest => true,
            Phase::Over { .. } => false,
            _ => !self.moving(t),
        }
    }

    fn settle(&self, from: (f64, f64, f64), over: bool) -> f64 {
        if self.reduce {
            return FADE;
        }
        let target = if over { self.swollen() } else { self.rest() };
        SWELL_SPRING.settle_exact(from.0, 0.0, target.w, 0.2).max(SWELL_SPRING.settle_exact(from.1, 0.0, target.h, 0.2)).max(0.2)
    }

    /// A drop's whole length: the flight, the swallow and the return into the notch.
    pub fn drop_len(&self) -> f64 {
        if self.reduce {
            return FADE * 2.0;
        }
        let land = self.gulp_time();
        let (_, h0) = (self.swollen().w, self.swollen().h);
        land + GULP.settle_exact(h0, GULP_KICK, self.rest().h, 0.2).max(0.3)
    }

    /// When the flight reaches `GULP_AT` of its way.
    fn gulp_time(&self) -> f64 {
        // Critically damped from 0 to 1: solve progress(τ) = GULP_AT by stepping (closed form needs Lambert W).
        let mut tau = 0.0;
        while tau < 2.0 && FLY.eval(0.0, 0.0, 1.0, tau).0 < GULP_AT {
            tau += 1.0 / 480.0;
        }
        tau
    }

    /// The swell and glow at `t` for the island's own shape (`None` at rest). The notch's swell as a share of
    /// `SWELL` (a plain display's band grows from nothing, the island by `SWELL` alone); Reduce Motion: the glow only.
    pub fn swell(&self, t: f64) -> Option<Swell> {
        let f = self.frame(t);
        if !f.visible {
            return None;
        }
        if self.reduce {
            return Some(Swell { w: 0.0, h: 0.0, flare: 0.0, glow: f.glow });
        }
        let (rest, swollen) = (self.rest(), self.swollen());
        let share = |v: f64, r: f64, s: f64, by: f64| ((v - r) / (s - r).max(1e-6)).max(0.0) * by;
        Some(Swell { w: share(f.shape.w, rest.w, swollen.w, SWELL.0), h: share(f.shape.h, rest.h, swollen.h, SWELL.1), flare: f.shape.flare, glow: f.glow })
    }

    pub fn frame(&self, t: f64) -> DropFrame {
        let rest = self.rest();
        let hidden = DropFrame { shape: rest, glow: 0.0, thumb: None, visible: false };
        let swollen = self.swollen();
        let flare = |w: f64, h: f64| {
            // The concave joins grow in with the swell (a band on a plain display has none at rest).
            let k = clamp01((h - rest.h) / (swollen.h - rest.h).max(1e-6));
            Shape { w, h, flare: lerp(rest.flare, swollen.flare, k.max(if self.display.has_notch() { 1.0 } else { 0.0 })) }
        };
        match self.phase {
            Phase::Rest => hidden,
            Phase::Over { at, from } | Phase::Left { at, from } => {
                let over = matches!(self.phase, Phase::Over { .. });
                let tau = t - at;
                let target = if over { swollen } else { rest };
                let target_glow = if over { 1.0 } else { 0.0 };
                if self.reduce {
                    // The shape stays the notch's size; only the glow fades (and the band on a plain display).
                    let k = clamp01(tau / FADE);
                    let glow = lerp(from.2, target_glow, k);
                    let shape = if self.display.has_notch() { rest } else { Shape { w: swollen.w, h: if over || glow > 0.0 { swollen.h } else { 0.0 }, flare: swollen.flare } };
                    return DropFrame { shape, glow, thumb: None, visible: glow > 0.002 };
                }
                let (w, _) = SWELL_SPRING.eval(from.0, 0.0, target.w, tau);
                let (h, _) = SWELL_SPRING.eval(from.1, 0.0, target.h, tau);
                let glow = lerp(from.2, target_glow, smooth(0.0, 0.25, tau));
                let (w, h) = (w.max(rest.w), h.max(rest.h));
                let done = !over && !self.moving(t);
                if done {
                    return hidden;
                }
                DropFrame { shape: flare(w, h), glow, thumb: None, visible: true }
            }
            Phase::Dropped { at, p, from } => {
                let tau = t - at;
                if tau >= self.drop_len() {
                    return hidden;
                }
                let inside = Point::new(0.0, (swollen.h * 0.5).max(8.0));
                if self.reduce {
                    let k = clamp01(tau / FADE);
                    let out = clamp01((tau - FADE) / FADE);
                    let glow = lerp(from.2, 0.0, out);
                    let shape = if self.display.has_notch() { rest } else { Shape { w: swollen.w, h: if out < 1.0 { swollen.h } else { 0.0 }, flare: swollen.flare } };
                    return DropFrame { shape, glow, thumb: Some((p, 1.0, 1.0 - k)).filter(|_| k < 1.0), visible: true };
                }
                // The flight: from where it was let go to inside the shape, shrinking, gone as it enters.
                let (k, _) = FLY.eval(0.0, 0.0, 1.0, tau);
                let c = Point::new(lerp(p.x, inside.x, k), lerp(p.y, inside.y, k));
                let scale = lerp(1.0, 0.18, smooth(0.0, 1.0, k));
                let alpha = 1.0 - smooth(0.62, GULP_AT + 0.04, k);
                let thumb = (alpha > 0.002).then_some((c, scale, alpha));
                let tg = self.gulp_time();
                let (w, h, glow) = if tau < tg {
                    // Holding open for it (from wherever the hover left the shape).
                    let (w, _) = SWELL_SPRING.eval(from.0, 0.0, swollen.w, tau);
                    let (h, _) = SWELL_SPRING.eval(from.1, 0.0, swollen.h, tau);
                    (w, h, lerp(from.2, 1.0, smooth(0.0, 0.15, tau)))
                } else {
                    // The swallow: a kick outward, then home into the notch with one soft bounce.
                    let s = tau - tg;
                    let (h0, w0) = (SWELL_SPRING.eval(from.1, 0.0, swollen.h, tg).0, SWELL_SPRING.eval(from.0, 0.0, swollen.w, tg).0);
                    let (h, _) = GULP.eval(h0, GULP_KICK, rest.h, s);
                    let (w, _) = GULP.eval(w0, GULP_KICK * 0.8, rest.w, s);
                    (w, h, 1.0 - smooth(0.0, 0.32, s))
                };
                DropFrame { shape: flare(w.max(rest.w), h.max(rest.h)), glow, thumb, visible: true }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_dir(_: &Path) -> bool {
        false
    }

    fn classify_simple(s: Snapshot) -> (Vec<DropItem>, Vec<(String, usize)>) {
        let mut saved = vec![];
        let items = classify(s, &no_dir, &mut |name, bytes| {
            saved.push((name.to_string(), bytes.len()));
            Some(PathBuf::from("/drops").join(name))
        }, "2026-10-24-014501-000");
        (items, saved)
    }

    #[test]
    fn a_drop_on_the_pull_down_attaches_only_while_he_is_writing() {
        assert_eq!(route(true, true, false), Route::Composer, "text typed");
        assert_eq!(route(true, false, true), Route::Composer, "the keyboard in the empty composer");
        assert_eq!(route(true, true, true), Route::Composer);
        assert_eq!(route(true, false, false), Route::Agent, "an empty composer: sent like a notch drop");
        assert_eq!(route(false, true, true), Route::Agent, "no pull-down: a notch drop");
    }

    #[test]
    fn files_map_by_extension_and_folders_are_folders() {
        let dir = |p: &Path| p.extension().is_none() || p.extension().is_some_and(|e| e == "app");
        let files = ["/a/shot.PNG", "/a/clip.mov", "/a/talk.m4a", "/a/Work", "/a/notes.md", "/a/paper.pdf", "/a/main.rs", "/a/Heimdall.app", "/a/archive.zip", "/a/Makefile"];
        let s = Snapshot { files: files.iter().map(PathBuf::from).collect(), text: Some("shot.PNG".into()), ..Default::default() };
        let items = classify(s, &dir, &mut |_, _| None, "x");
        let kinds: Vec<_> = items.iter().map(|i| i.kind).collect();
        use DropKind::*;
        // The Makefile has no extension: as a file it is a document (`dir` says folder for extensionless paths here).
        assert_eq!(kinds, vec![Image, Video, Audio, Folder, Document, Document, Document, File, File, Folder]);
        assert!(items.iter().all(|i| i.path.is_some() && i.text.is_none()), "files win over their names as text");
    }

    #[test]
    fn a_temporary_screenshot_is_copied_into_the_drops_folder() {
        let tmp = std::env::temp_dir().join(format!("heimdall-drop-test-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let shot = tmp.join("Screenshot 2026-10-24 at 01.45.01.png");
        std::fs::write(&shot, b"png bytes").unwrap();
        assert!(is_temporary(&shot), "{}", shot.display());
        let (items, saved) = classify_simple(Snapshot { files: vec![shot.clone()], ..Default::default() });
        assert_eq!(items[0].kind, DropKind::Image);
        assert_eq!(items[0].path.as_deref(), Some(Path::new("/drops/2026-10-24-014501-000-Screenshot 2026-10-24 at 01.45.01.png")));
        assert_eq!(saved, vec![("2026-10-24-014501-000-Screenshot 2026-10-24 at 01.45.01.png".to_string(), 9)]);
        // A video in a temporary folder keeps its path (only images are copied: they are small and seen at once).
        let (items, saved) = classify_simple(Snapshot { files: vec![tmp.join("x.mov")], ..Default::default() });
        assert_eq!((items[0].kind, saved.len()), (DropKind::Video, 0));
        std::fs::remove_dir_all(&tmp).ok();
        assert!(!is_temporary(Path::new("/Users/owner/Desktop/a.png")));
    }

    #[test]
    fn links_text_and_image_data() {
        let (items, _) = classify_simple(Snapshot { urls: vec!["https://example.com/a".into()], text: Some("Example".into()), ..Default::default() });
        assert_eq!(items, vec![DropItem { kind: DropKind::Link, path: None, url: Some("https://example.com/a".into()), text: None }]);
        let (items, _) = classify_simple(Snapshot { text: Some("  https://news.ycombinator.com/item?id=1  ".into()), ..Default::default() });
        assert_eq!((items[0].kind, items[0].url.as_deref()), (DropKind::Link, Some("https://news.ycombinator.com/item?id=1")));
        let (items, _) = classify_simple(Snapshot { text: Some("Selected words, https://x.y inside".into()), ..Default::default() });
        assert_eq!((items[0].kind, items[0].text.as_deref()), (DropKind::Text, Some("Selected words, https://x.y inside")));
        // A picture out of a browser: one image, saved, with its address.
        let (items, saved) = classify_simple(Snapshot { urls: vec!["https://cdn.example.com/cat.jpg".into()], image: Some(ImageData { bytes: vec![1, 2, 3], ext: "tiff" }), ..Default::default() });
        assert_eq!(items.len(), 1);
        assert_eq!((items[0].kind, items[0].url.as_deref()), (DropKind::Image, Some("https://cdn.example.com/cat.jpg")));
        assert_eq!(saved, vec![("2026-10-24-014501-000.tiff".to_string(), 3)]);
        // Long text is kept as a file.
        let long = "word ".repeat(TEXT_INLINE_MAX / 4);
        let (items, saved) = classify_simple(Snapshot { text: Some(long), ..Default::default() });
        assert_eq!((items[0].kind, saved[0].0.as_str()), (DropKind::Document, "2026-10-24-014501-000.txt"));
        // Nothing usable: nothing.
        assert!(classify_simple(Snapshot { text: Some("   ".into()), ..Default::default() }).0.is_empty());
    }

    #[test]
    fn several_links_are_several_items() {
        let (items, _) = classify_simple(Snapshot { urls: vec!["https://a.com".into(), "https://b.com".into()], ..Default::default() });
        assert_eq!(items.len(), 2);
        assert_eq!(summary(&items).1, "2 links");
    }

    #[test]
    fn the_islands_words() {
        let item = |kind| DropItem { kind, path: None, url: None, text: None };
        assert_eq!(summary(&[item(DropKind::Image)]), (DropKind::Image, "1 image".into()));
        assert_eq!(summary(&[item(DropKind::Image), item(DropKind::Image)]), (DropKind::Image, "2 images".into()));
        assert_eq!(summary(&[item(DropKind::Link)]), (DropKind::Link, "Link".into()));
        assert_eq!(summary(&[item(DropKind::Folder)]), (DropKind::Folder, "Folder".into()));
        assert_eq!(summary(&[item(DropKind::Text)]), (DropKind::Text, "Text".into()));
        assert_eq!(summary(&[item(DropKind::Image), item(DropKind::Folder), item(DropKind::Link)]), (DropKind::Mixed, "3 items".into()));
    }

    #[test]
    fn the_swell_grows_under_a_drag_and_returns_into_the_notch() {
        let mut m = Motion::new(Display::MBP16, false);
        assert!(!m.frame(0.0).visible && m.at_rest(0.0));
        m.over(1.0);
        let f = m.frame(1.6);
        assert!((f.shape.w - (185.0 + SWELL.0)).abs() < 0.5 && (f.shape.h - (32.0 + SWELL.1)).abs() < 0.5, "{f:?}");
        assert!(f.glow > 0.99 && f.visible);
        assert!(!m.moving(2.0) && !m.at_rest(2.0), "swollen and still while the drag stays");
        m.left(2.0);
        assert!(m.moving(2.1));
        assert!(m.at_rest(3.0) && !m.frame(3.0).visible);
        // Never smaller than the notch on the way.
        for i in 0..200 {
            let f = m.frame(2.0 + i as f64 / 200.0);
            assert!(f.shape.w >= 185.0 - 1e-9 && f.shape.h >= 32.0 - 1e-9);
        }
    }

    #[test]
    fn a_drop_flies_in_shrinking_and_the_shape_bounces_once_then_rests() {
        let mut m = Motion::new(Display::MBP16, false);
        m.over(0.0);
        let p = Point::new(120.0, 70.0);
        m.dropped(0.5, p);
        let start = m.frame(0.5);
        let (c, s, a) = start.thumb.unwrap();
        assert_eq!((c, s, a), (p, 1.0, 1.0));
        let mut last_d = f64::INFINITY;
        let mut max_h: f64 = 0.0;
        let mut gone = None;
        let end = 0.5 + m.drop_len();
        let mut t = 0.5;
        while t < end + 0.05 {
            let f = m.frame(t);
            if let Some((c, s, _)) = f.thumb {
                let d = (c.x.powi(2) + (c.y - 21.0).powi(2)).sqrt();
                assert!(d <= last_d + 1e-9, "the item only gets closer");
                assert!(s <= 1.0 + 1e-9);
                last_d = d;
            } else if gone.is_none() && t > 0.5 {
                gone = Some(t - 0.5);
            }
            max_h = max_h.max(f.shape.h);
            assert!(f.shape.h >= 32.0 - 1e-9);
            t += 1.0 / 120.0;
        }
        let gone = gone.expect("the item disappears into the shape");
        assert!(gone < 0.45, "taken within a blink: {gone}");
        assert!(max_h > 32.0 + SWELL.1 + 2.0, "the swallow bounces past the swell: {max_h}");
        assert!(m.drop_len() < 1.4, "{}", m.drop_len());
        assert!(!m.frame(end + 0.01).visible && m.at_rest(end + 0.01));
    }

    #[test]
    fn reduce_motion_only_fades() {
        let mut m = Motion::new(Display::MBP16, true);
        m.over(0.0);
        let f = m.frame(1.0);
        assert_eq!((f.shape.w, f.shape.h), (185.0, 32.0));
        assert!(f.glow > 0.99);
        m.dropped(1.0, Point::new(50.0, 60.0));
        for i in 0..60 {
            let f = m.frame(1.0 + i as f64 / 120.0);
            assert_eq!((f.shape.w, f.shape.h), (185.0, 32.0));
            if let Some((c, s, _)) = f.thumb {
                assert_eq!((c, s), (Point::new(50.0, 60.0), 1.0), "no flight, no shrinking");
            }
        }
        assert_eq!(m.confirm_at(1.0), 1.0);
        assert!(m.at_rest(1.0 + m.drop_len() + 0.01));
    }

    #[test]
    fn the_island_swells_by_the_notchs_swell_and_glows_with_it() {
        let mut m = Motion::new(Display::MBP16, false);
        assert_eq!(m.swell(0.0), None, "nothing at rest");
        m.over(0.0);
        let s = m.swell(1.0).unwrap();
        assert!((s.w - SWELL.0).abs() < 0.5 && (s.h - SWELL.1).abs() < 0.5 && s.glow > 0.99, "{s:?}");
        let island = Shape { w: 480.0, h: 170.0, flare: crate::island::object::geom::FLARE };
        let on = s.on(island);
        assert!((on.w - 506.0).abs() < 0.5 && (on.h - 180.0).abs() < 0.5 && on.flare == island.flare);
        // Growing out of the notch, the island's joins are the notch's (narrower): the swell's are kept.
        let growing = s.on(Display::MBP16.rest());
        assert_eq!(growing, m.frame(1.0).shape, "on the notch, the island's swell is exactly the notch's");
        // Every frame on the way in and out is the notch's own swell, so a hand-over between the two is seamless.
        for i in 0..240 {
            let t = i as f64 / 120.0;
            if i == 120 {
                m.left(1.0);
            }
            let (f, s) = (m.frame(t), m.swell(t));
            match s {
                Some(s) => assert!((s.w - (f.shape.w - 185.0)).abs() < 1e-9 && (s.h - (f.shape.h - 32.0)).abs() < 1e-9 && s.glow == f.glow),
                None => assert!(!f.visible),
            }
        }
        assert_eq!(m.swell(2.5), None);
        // The swallow kicks the island too, and it comes back to its own size.
        m.over(3.0);
        m.dropped(3.5, Point::new(0.0, 120.0));
        let peak = (0..120).filter_map(|i| m.swell(3.5 + i as f64 / 120.0)).map(|s| s.h).fold(0.0, f64::max);
        assert!(peak > SWELL.1 + 2.0, "{peak}");
        assert_eq!(m.swell(3.5 + m.drop_len() + 0.01), None);
    }

    #[test]
    fn reduce_motion_and_a_plain_display_on_the_island() {
        let mut m = Motion::new(Display::MBP16, true);
        m.over(0.0);
        assert_eq!(m.swell(1.0).map(|s| (s.w, s.h)), Some((0.0, 0.0)), "Reduce Motion: no swell");
        let mut m = Motion::new(Display::PLAIN, false);
        m.over(0.0);
        let s = m.swell(1.0).unwrap();
        assert!((s.h - SWELL.1).abs() < 0.5, "a plain display's island swells by the swell, not the band: {s:?}");
    }

    #[test]
    fn a_plain_display_grows_a_band_from_the_top_edge() {
        let mut m = Motion::new(Display::PLAIN, false);
        assert_eq!(m.frame(0.0).shape.h, 0.0);
        m.over(0.0);
        assert!(m.frame(0.05).shape.h > 0.0);
        assert!((m.frame(1.0).shape.h - (32.0 + SWELL.1)).abs() < 0.5);
        m.left(1.0);
        assert!(!m.frame(2.0).visible);
    }
}
