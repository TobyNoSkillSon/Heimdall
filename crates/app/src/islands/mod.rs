//! Floating Islands inside Heimdall (night 1022): the bundled Swift helper's file protocol and everything Heimdall
//! owns above it. Contract: `docs/archive/briefs/night-1022-islands.md`.
#![allow(dead_code)] // the stage-2 parts wire these in

pub mod files;
pub mod nudge;
// ---- host (night 1022): helper supervisor, watcher, agent tools, responses, thread binding ----
pub mod binding;
pub mod digest;
pub mod live;
pub mod plan;
pub mod proposals;
pub mod guarantee;
pub mod slate;
pub mod report_link;
pub mod responses;
pub mod stages;
pub mod streams;
pub mod supervisor;
pub mod tools;
pub mod train;
pub mod visibility;
pub mod watch;
// ---- end host ----

use serde_json::{Map, Value};
use std::sync::Mutex;

/// What the host's one watcher on the islands folder publishes. Other parts (the notch nudge, the plan refresher)
/// subscribe instead of watching the folder themselves.
#[derive(Clone, Debug, PartialEq)]
pub enum IslandsEvent {
    /// Every island on disk after a change in `islands/` (id, object), sorted by id.
    Changed(Vec<(String, Map<String, Value>)>),
    /// One of the user's clicks, after the host consumed it from `responses/` and acted on it.
    Response(Map<String, Value>),
}

type Listener = Box<dyn Fn(&IslandsEvent) + Send + Sync>;
static LISTENERS: Mutex<Vec<Listener>> = Mutex::new(Vec::new());

/// Called once at startup by each part that wants island events; listeners run on the watcher's thread and must
/// return quickly (hand work to their own thread or channel).
pub fn subscribe(listener: impl Fn(&IslandsEvent) + Send + Sync + 'static) {
    LISTENERS.lock().unwrap_or_else(|e| e.into_inner()).push(Box::new(listener));
}

/// Called by the host's watcher.
pub fn publish(event: &IslandsEvent) {
    for l in LISTENERS.lock().unwrap_or_else(|e| e.into_inner()).iter() { l(event) }
}

// ---- host (night 1022) ----
use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};

/// How a response line reaches the agent: `Answer` wakes it like an `ask` click; `Context` rides along without a
/// turn (agent::notice::NoticeKind).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class { Answer, Context }

impl Class {
    /// The outbox id prefix `agent::notice::NoticeKind::classify` reads.
    pub fn prefix(self) -> &'static str {
        match self { Class::Answer => "islands-answer:", Class::Context => "islands-context:" }
    }
}

pub type Deliver = Arc<dyn Fn(Class, &str) + Send + Sync>;

/// Heimdall's side of the islands: the folder, thread bindings, the way lines reach the agent, and the binding
/// loop's job queue. One per live app (`live::start`); tests build their own.
pub struct Host {
    pub root: files::Root,
    pub bindings: binding::Bindings,
    deliver: Mutex<Option<Deliver>>,
    t3: Mutex<Option<tokio::sync::mpsc::UnboundedSender<binding::Job>>>,
    /// Every island the watcher has seen, last version (a closed island's title outlives its file).
    seen: Mutex<BTreeMap<String, Map<String, Value>>>,
}

impl Host {
    pub fn new(root: files::Root, bindings: Option<std::path::PathBuf>) -> Host {
        Host { root, bindings: binding::Bindings::load(bindings), deliver: Mutex::new(None), t3: Mutex::new(None), seen: Mutex::new(BTreeMap::new()) }
    }
    pub fn set_deliver(&self, d: Deliver) { *self.deliver.lock().unwrap_or_else(|e| e.into_inner()) = Some(d) }
    pub fn deliver(&self, class: Class, text: &str) {
        let d = self.deliver.lock().unwrap_or_else(|e| e.into_inner()).clone();
        match d {
            Some(d) => d(class, text),
            None => eprintln!("islands: not delivered (no agent attached): {text}"),
        }
    }
    /// The island agent's inbox: every bound island's signals after each sync (`island_agent::spawn_live`).
    pub fn set_island_agent(&self, tx: Option<tokio::sync::mpsc::UnboundedSender<crate::island_agent::Snapshot>>) { self.bindings.set_agent(tx) }
    pub fn set_t3(&self, tx: Option<tokio::sync::mpsc::UnboundedSender<binding::Job>>) { *self.t3.lock().unwrap_or_else(|e| e.into_inner()) = tx }
    pub fn t3_job(&self, job: binding::Job) {
        let tx = self.t3.lock().unwrap_or_else(|e| e.into_inner()).clone();
        match tx {
            Some(tx) if tx.send(job).is_ok() => {}
            // The binding loop starts with the agent; bindings persist and sync at its next start.
            _ => eprintln!("islands: thread binding loop not running; job waits for the next start"),
        }
    }
    pub fn remember(&self, all: &[(String, Map<String, Value>)]) {
        let mut seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());
        for (id, o) in all { seen.insert(id.clone(), o.clone()); }
    }
    /// The island as on disk now, else as last seen, else (a plan list) the copy kept at its last write: a list closed
    /// before the watcher ever saw it still has its title and content (bug note 2026-10-31T15-57-16).
    pub fn seen(&self, id: &str) -> Option<Map<String, Value>> {
        self.root.read(id)
            .or_else(|| self.seen.lock().unwrap_or_else(|e| e.into_inner()).get(id).cloned())
            .or_else(|| plan::list_of(id, None).and_then(|l| crate::goals::ops::kept(&self.root, l)))
    }
}

static HOST: OnceLock<Arc<Host>> = OnceLock::new();

/// The live app's host, once `live::start` ran; `None` in replays, lab modes and tests.
pub fn host() -> Option<Arc<Host>> { HOST.get().cloned() }
pub(crate) fn install(h: Arc<Host>) -> bool { HOST.set(h).is_ok() }

/// An error's first line, bounded, for logs and lines to the agent.
pub fn short_error(e: &anyhow::Error) -> String {
    let t = format!("{e:#}");
    let first = t.lines().next().unwrap_or("").to_string();
    if first.chars().count() > 200 { first.chars().take(200).collect::<String>() + "…" } else { first }
}
// ---- end host ----

/// A picture an island can show: an absolute, `~/` or `file://` path (as the app reads it) to an existing file of at
/// most 200 MB whose first bytes are PNG, JPEG, GIF, WebP, HEIC/HEIF or TIFF. Anything else would draw as a broken
/// image, so writers leave it out.
pub fn picture(path: &str) -> bool {
    use std::io::Read;
    let t = path.trim();
    let decoded;
    let t = match t.strip_prefix("file://") {
        // As the app's URL reads it: percent escapes decoded.
        Some(rest) => { decoded = percent_decode(rest); decoded.as_str() }
        None => t,
    };
    let expanded = match t.strip_prefix("~/") {
        Some(rest) => match std::env::var_os("HOME") { Some(h) => std::path::PathBuf::from(h).join(rest), None => return false },
        None => std::path::PathBuf::from(t),
    };
    let p = expanded.as_path();
    if !p.is_absolute() { return false }
    let Ok(meta) = std::fs::metadata(p) else { return false };
    if !meta.is_file() || meta.len() < 12 || meta.len() > 200 << 20 { return false }
    let mut head = [0u8; 12];
    if std::fs::File::open(p).and_then(|mut f| f.read_exact(&mut head)).is_err() { return false }
    head.starts_with(b"\x89PNG")
        || head.starts_with(&[0xFF, 0xD8, 0xFF])
        || head.starts_with(b"GIF8")
        || (head.starts_with(b"RIFF") && &head[8..12] == b"WEBP")
        || (&head[4..8] == b"ftyp" && matches!(&head[8..12], b"heic" | b"heix" | b"mif1" | b"msf1" | b"avif"))
        || head.starts_with(b"II*\0")
        || head.starts_with(b"MM\0*")
}

/// Largest inline SVG icon (bytes).
pub const ICON_SVG_MAX: usize = 2048;

/// An island icon the app can draw safely, trimmed: an SF Symbol name (`ipad`, `film.stack`) or a tiny inline SVG
/// of plain shapes (`svg`, `g`, `path`, `circle`, `ellipse`, `rect`, `line`, `polyline`, `polygon`), at most
/// `ICON_SVG_MAX` bytes, with no scripts, styles, links, images, text or event attributes. `None` otherwise.
pub fn icon(s: &str) -> Option<String> {
    let t = s.trim();
    if t.is_empty() { return None }
    if !t.starts_with('<') {
        let ok = t.len() <= 64 && t.split('.').all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()));
        return ok.then(|| t.to_owned());
    }
    // An XML header is harmless and models write one: it goes.
    let t = match t.strip_prefix("<?xml") { Some(rest) => rest.split_once("?>").map_or("", |(_, r)| r).trim(), None => t };
    let lower = t.to_ascii_lowercase();
    if t.len() > ICON_SVG_MAX || !lower.starts_with("<svg") || !lower.ends_with("</svg>") { return None }
    // The one namespace allowed; after it, no colon anywhere (no `svg:style`, no `xlink:href`), no styles, links,
    // escapes, entities, comments or processing instructions.
    let bare = lower.replace(r#"xmlns="http://www.w3.org/2000/svg""#, "");
    if ["href", "url", "style", "script", "javascript", "<!", "<?", "&", "\\", ":"].iter().any(|bad| bare.contains(bad)) { return None }
    // No event attributes: no attribute name starting with "on" (after a space, tab, newline or quote).
    if bare.as_bytes().windows(3).any(|w| (w[0].is_ascii_whitespace() || w[0] == b'"' || w[0] == b'\'') && w[1] == b'o' && w[2] == b'n') { return None }
    const SHAPES: [&str; 9] = ["svg", "g", "path", "circle", "ellipse", "rect", "line", "polyline", "polygon"];
    let mut rest = bare.as_str();
    while let Some(i) = rest.find('<') {
        let tag = &rest[i + 1..];
        let name: String = tag.trim_start_matches('/').chars().take_while(|c| c.is_ascii_alphanumeric()).collect();
        if !SHAPES.contains(&name.as_str()) { return None }
        rest = &tag[tag.find('>')? + 1..];
    }
    Some(t.to_owned())
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |c: u8| (c as char).to_digit(16);
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) { out.push((h * 16 + l) as u8); i += 3; continue }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod icon_tests {
    #[test]
    fn icons_are_symbols_or_plain_shape_svgs() {
        assert_eq!(super::icon(" film.stack ").as_deref(), Some("film.stack"));
        assert_eq!(super::icon("ipad").as_deref(), Some("ipad"));
        for bad in ["Film Stack", "film..stack", "../x", "", "a".repeat(65).as_str()] { assert!(super::icon(bad).is_none(), "{bad}") }
        let ok = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><g><rect x="3" y="5" width="18" height="14" rx="2"/><path d="M7 5v14M17 5v14"/></g></svg>"#;
        assert_eq!(super::icon(ok).as_deref(), Some(ok));
        for bad in [
            r#"<svg viewBox="0 0 24 24"><script>alert(1)</script></svg>"#,
            r#"<svg viewBox="0 0 24 24"><image href="/etc/x.png"/></svg>"#,
            r#"<svg viewBox="0 0 24 24" onload="x()"><path d="M0 0"/></svg>"#,
            r#"<svg viewBox="0 0 24 24"><path style="fill:url(#a)" d="M0 0"/></svg>"#,
            r#"<svg viewBox="0 0 24 24"><text>Hi</text></svg>"#,
            r#"<svg viewBox="0 0 24 24"><path d="M0 0"/>"#,
            r#"<svg viewBox="0 0 24 24"><svg:style>*{}</svg:style></svg>"#,
            r#"<svg viewBox="0 0 24 24"><path d="M0 0" fill="\75rl(#a)"/></svg>"#,
            r#"<svg viewBox="0 0 24 24"><path d="a>b" onclick="x()"/></svg>"#,
            r#"<svg viewBox="0 0 24 24"><!-- hi --><path d="M0 0"/></svg>"#,
        ] { assert!(super::icon(bad).is_none(), "{bad}") }
        let xml = r#"<?xml version="1.0"?><svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><circle cx="12" cy="12" r="8" fill="none" stroke="currentColor" stroke-width="2"/></svg>"#;
        assert!(super::icon(xml).is_some_and(|i| i.starts_with("<svg")), "an XML header is dropped, the icon kept");
        let big = format!("<svg viewBox=\"0 0 24 24\"><path d=\"{}\"/></svg>", "M1 1".repeat(600));
        assert!(super::icon(&big).is_none(), "too big");
    }
}

#[cfg(test)]
mod picture_tests {
    #[test]
    fn only_real_pictures_count() {
        let d = tempfile::tempdir().unwrap();
        let png = d.path().join("shot.png");
        std::fs::write(&png, b"\x89PNG\r\n\x1a\n0000000000").unwrap();
        let fake = d.path().join("fake.png");
        std::fs::write(&fake, b"not a picture at all").unwrap();
        assert!(super::picture(png.to_str().unwrap()));
        assert!(!super::picture(fake.to_str().unwrap()), "an image name is not enough");
        assert!(!super::picture(d.path().join("none").to_str().unwrap()), "a missing file");
        assert!(!super::picture(d.path().to_str().unwrap()), "a folder");
        assert!(!super::picture("shot.png"), "a relative path");
        assert!(super::picture(&format!("file://{}", png.display())), "a file URL, as the app reads it");
        let spaced = d.path().join("a shot.png");
        std::fs::copy(&png, &spaced).unwrap();
        assert!(super::picture(&format!("file://{}", spaced.display().to_string().replace(' ', "%20"))), "percent escapes decoded");
        assert_eq!(super::percent_decode("a%2"), "a%2");
        let home = std::env::var("HOME").unwrap();
        if let Ok(rest) = png.strip_prefix(&home) { assert!(super::picture(&format!("~/{}", rest.display()))) }
    }
}
