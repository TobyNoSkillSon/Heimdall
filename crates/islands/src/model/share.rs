//! What the viewer's drag handle hands to another app ("something at the top that I can drag into a chat,
//! which copies the entire file, so I can send the report to someone else"). Any file goes as it is; an HTML page that
//! loads files from beside it (screenshots in `report-src/`, a stylesheet, fonts, scripts) goes as one self-contained
//! copy with those files inlined as data URIs, so it arrives whole on someone else's machine. The copy has the page's
//! own name, sits in its own folder under `Paths::shares()`, and is swept a day later (a chat or mail app may read a
//! dropped or pasted file long after the drop).
//!
//! Only files inside the page's own folder are inlined: what the viewer itself can load (its read access is that
//! folder), so the copy carries what the user saw and nothing from elsewhere on the Mac. Within it, hidden files and
//! folders are never inlined, and each element takes only its own kind of file (an image only an image, a script
//! only JavaScript, a stylesheet only CSS), so a page cannot smuggle a key or a document out through a hidden
//! element. A page directly in the home folder or at the root is not bundled at all. Remote URLs, `data:` URIs,
//! links between pages (`<a href>`) and anything missing stay as they are. Scripts' own loads (`fetch`, module
//! imports) are not followed.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use super::Paths;

/// One inlined file at most this big; bigger ones stay references (a report with a 300 MB video is not one HTML).
pub const MAX_ASSET: u64 = 64 << 20;
/// All inlined files together.
pub const MAX_TOTAL: u64 = 160 << 20;
/// How long a shared copy is kept.
pub const KEEP: Duration = Duration::from_secs(24 * 3600);
/// Nested pages and stylesheets inlined inside each other at most this deep.
const DEPTH: usize = 4;

impl Paths {
    /// Shared copies: the temp folder, or the lab's own folder in a lab run.
    pub fn shares() -> PathBuf {
        match std::env::var("FLOATING_ISLANDS_DIR") {
            Ok(d) if !d.is_empty() => Self::root().join("shares"),
            _ => std::env::temp_dir().join("FloatingIslands-shares"),
        }
    }
}

/// What a drag or copy hands over for `path`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Shared {
    /// The file itself.
    AsIs(PathBuf),
    /// A self-contained copy of an HTML page: where it is, and how many local files went into it.
    Bundled { path: PathBuf, inlined: usize },
}

impl Shared {
    pub fn path(&self) -> &Path {
        match self {
            Shared::AsIs(p) => p,
            Shared::Bundled { path, .. } => path,
        }
    }
}

fn is_html(path: &Path) -> bool {
    let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    matches!(ext.as_str(), "html" | "htm" | "xhtml")
}

/// The file to hand over for `path`, made in `dir` (normally `Paths::shares()`) when it has to be bundled. Copies
/// older than `KEEP` are swept first. A page with no local files, or one that cannot be read as text, goes as it is.
pub fn shareable(path: &Path, dir: &Path) -> std::io::Result<Shared> {
    shareable_with_deps(path, dir).map(|(s, _)| s)
}

/// The same, with every file the result was made from (the page first): `fingerprint` of them tells whether a copy
/// made earlier is still the page as it is now.
pub fn shareable_with_deps(path: &Path, dir: &Path) -> std::io::Result<(Shared, Vec<PathBuf>)> {
    let meta = std::fs::metadata(path)?;
    let as_is = || (Shared::AsIs(path.to_path_buf()), vec![path.to_path_buf()]);
    if !meta.is_file() || !is_html(path) {
        return Ok(as_is());
    }
    let Ok(html) = std::fs::read_to_string(path) else { return Ok(as_is()) };
    // The page's own folder bounds what is inlined; one that cannot be resolved, or the home folder or the root
    // (everything would be "beside" the page), bundles nothing.
    let Some(root) = path.parent().and_then(|p| std::fs::canonicalize(if p.as_os_str().is_empty() { Path::new(".") } else { p }).ok()) else {
        return Ok(as_is());
    };
    let home = std::env::var_os("HOME").and_then(|h| Path::new(&h).canonicalize().ok());
    if root.parent().is_none() || Some(&root) == home.as_ref() {
        return Ok(as_is());
    }
    let mut b = Bundler { root: root.clone(), total: 0, inlined: 0, deps: vec![path.to_path_buf()] };
    let out = b.html(&html, &root, 0);
    if b.inlined == 0 {
        return Ok(as_is());
    }
    sweep(dir, KEEP);
    let name = path.file_name().map(|n| n.to_owned()).unwrap_or_else(|| "page.html".into());
    let folder = fresh_folder(dir)?;
    let dest = folder.join(name);
    std::fs::write(&dest, out)?;
    Ok((Shared::Bundled { path: dest, inlined: b.inlined }, b.deps))
}

/// What a file is now (modification time and size; `None` once it is gone), to tell a copy made from it is current.
pub type Stamp = (PathBuf, Option<SystemTime>, Option<u64>);

pub fn fingerprint(paths: &[PathBuf]) -> Vec<Stamp> {
    paths
        .iter()
        .map(|p| {
            let m = std::fs::metadata(p).ok();
            (p.clone(), m.as_ref().and_then(|m| m.modified().ok()), m.map(|m| m.len()))
        })
        .collect()
}

/// A new folder of its own under `dir` (the copy keeps the page's name, so two shares never collide).
fn fresh_folder(dir: &Path) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let ms = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    for n in 0..1000 {
        let f = dir.join(format!("{ms}-{}-{n}", std::process::id()));
        match std::fs::create_dir(&f) {
            Ok(()) => return Ok(f),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::other("no free share folder"))
}

/// Removes share folders in `dir` last changed more than `age` ago. Returns how many went.
pub fn sweep(dir: &Path, age: Duration) -> usize {
    let Ok(rd) = std::fs::read_dir(dir) else { return 0 };
    let now = SystemTime::now();
    let mut n = 0;
    for e in rd.flatten() {
        let Ok(m) = e.metadata() else { continue };
        let old = m.modified().ok().and_then(|t| now.duration_since(t).ok()).is_some_and(|d| d >= age);
        if old && m.is_dir() && std::fs::remove_dir_all(e.path()).is_ok() {
            n += 1;
        }
    }
    n
}

// MARK: Inlining

struct Bundler {
    root: PathBuf,
    total: u64,
    inlined: usize,
    deps: Vec<PathBuf>,
}

/// What each kind of reference may load, by MIME type (an image element only an image, and so on).
fn allowed(kind: &str, mime: &str) -> bool {
    let image = mime.starts_with("image/");
    let media = mime.starts_with("video/") || mime.starts_with("audio/") || mime == "text/vtt";
    match kind {
        "img" | "image" | "feimage" | "input" | "poster" | "srcset" => image,
        "script" => mime == "text/javascript",
        "css" => mime == "text/css",
        "url" => image || mime.starts_with("font/") || mime == "text/css",
        "link" => image || mime.starts_with("font/") || mime == "text/css" || mime == "text/javascript",
        "source" | "video" | "audio" | "track" => image || media,
        "embed" | "object" | "iframe" | "frame" => image || mime == "text/html" || mime == "application/pdf",
        _ => false,
    }
}

/// Elements whose `src` is a file the page loads.
const SRC_TAGS: [&str; 10] = ["img", "script", "source", "video", "audio", "track", "embed", "input", "iframe", "frame"];

impl Bundler {
    /// The page with every local load it makes inlined. `base` is the folder its relative URLs resolve in.
    fn html(&mut self, html: &str, base: &Path, depth: usize) -> String {
        let mut base = base.to_path_buf();
        let mut edits: Vec<(usize, usize, String)> = vec![];
        let bytes = html.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] != b'<' {
                i += 1;
                continue;
            }
            if html[i..].starts_with("<!--") {
                i = html[i..].find("-->").map(|k| i + k + 3).unwrap_or(bytes.len());
                continue;
            }
            let Some(tag) = parse_tag(html, i) else {
                i += 1;
                continue;
            };
            let name = tag.name.to_ascii_lowercase();
            let attr = |n: &str| tag.attrs.iter().find(|a| a.name.eq_ignore_ascii_case(n));
            if name == "base" {
                // A local base moves where relative URLs resolve; a remote one leaves them remote.
                if let Some(a) = attr("href") {
                    match local_ref(&a.value) {
                        // A folder (`sub/`) or a file in it (`sub/index.html`): relative URLs resolve in the folder.
                        Some(r) if !r.is_empty() => base = if r.ends_with('/') { base.join(r) } else { base.join(r).parent().map(Path::to_path_buf).unwrap_or(base) },
                        Some(_) => {}
                        None => return html.to_owned(),
                    }
                }
            }
            for a in &tag.attrs {
                let an = a.name.to_ascii_lowercase();
                let new = match an.as_str() {
                    "src" if SRC_TAGS.contains(&name.as_str()) => self.file_uri(&a.value, &base, depth, &name),
                    "href" if name == "link" => {
                        let rel = attr("rel").map(|r| r.value.to_ascii_lowercase()).unwrap_or_default();
                        let loads = ["stylesheet", "icon", "preload", "apple-touch-icon", "modulepreload"].iter().any(|k| rel.split_whitespace().any(|w| w == *k));
                        if loads { self.file_uri(&a.value, &base, depth, if rel.contains("stylesheet") { "css" } else { "link" }) } else { None }
                    }
                    "href" | "xlink:href" if name == "image" || name == "feimage" => self.file_uri(&a.value, &base, depth, &name),
                    "poster" if name == "video" => self.file_uri(&a.value, &base, depth, "poster"),
                    "data" if name == "object" => self.file_uri(&a.value, &base, depth, &name),
                    "srcset" | "imagesrcset" => self.srcset(&a.value, &base, depth),
                    "style" => {
                        // The attribute's own text (`&quot;` and the like) read as the CSS it stands for.
                        let raw = decode_entities(&a.value);
                        let css = self.css(&raw, &base, depth);
                        (css != raw).then_some(css)
                    }
                    _ => None,
                };
                if let Some(v) = new {
                    edits.push((a.start, a.end, format!("\"{}\"", v.replace('"', "&quot;"))));
                }
            }
            i = tag.end;
            // Raw text: a style block's CSS is inlined; a script's, a title's or a text area's text is left alone.
            if ["style", "script", "title", "textarea", "xmp"].contains(&name.as_str()) && !tag.self_closing {
                let close = format!("</{name}");
                let end = find_ci(html, i, &close).unwrap_or(bytes.len());
                if name == "style" {
                    let body = &html[i..end];
                    let css = self.css(body, &base, depth);
                    if css != body {
                        edits.push((i, end, css));
                    }
                }
                i = end;
            }
        }
        splice(html, edits)
    }

    /// `url(…)` and `@import` in a stylesheet, inlined.
    fn css(&mut self, css: &str, base: &Path, depth: usize) -> String {
        let mut edits: Vec<(usize, usize, String)> = vec![];
        let b = css.as_bytes();
        let mut i = 0;
        while i < b.len() {
            if b[i..].starts_with(b"/*") {
                i = css[i..].find("*/").map(|k| i + k + 2).unwrap_or(b.len());
                continue;
            }
            if at_ci(b, i, b"url(") && !is_ident_char(css[..i].chars().last()) {
                let start = i;
                let mut j = i + 4;
                while j < b.len() && b[j].is_ascii_whitespace() {
                    j += 1;
                }
                let (value, after) = if j < b.len() && (b[j] == b'"' || b[j] == b'\'') {
                    let q = b[j];
                    let k = css[j + 1..].find(q as char).map(|k| j + 1 + k).unwrap_or(b.len());
                    (&css[j + 1..k], (k + 1).min(b.len()))
                } else {
                    let k = css[j..].find(')').map(|k| j + k).unwrap_or(b.len());
                    (css[j..k].trim_end(), k)
                };
                let close = css[after..].find(')').map(|k| after + k + 1).unwrap_or(b.len());
                let kind = if css[..start].trim_end().to_ascii_lowercase().ends_with("@import") { "css" } else { "url" };
                if let Some(uri) = self.file_uri(value, base, depth, kind) {
                    edits.push((start, close, format!("url({uri})")));
                }
                i = close;
                continue;
            }
            if at_ci(b, i, b"@import") {
                let mut j = i + 7;
                while j < b.len() && b[j].is_ascii_whitespace() {
                    j += 1;
                }
                if j < b.len() && (b[j] == b'"' || b[j] == b'\'') {
                    let q = b[j];
                    let k = css[j + 1..].find(q as char).map(|k| j + 1 + k).unwrap_or(b.len());
                    if let Some(uri) = self.file_uri(&css[j + 1..k], base, depth, "css") {
                        edits.push((j, (k + 1).min(b.len()), format!("url({uri})")));
                    }
                    i = (k + 1).min(b.len());
                    continue;
                }
            }
            // A string's text (`content: "url(x)"`) is not a load.
            if b[i] == b'"' || b[i] == b'\'' {
                let q = b[i];
                let mut k = i + 1;
                while k < b.len() && b[k] != q && b[k] != b'\n' {
                    k += if b[k] == b'\\' { 2 } else { 1 };
                }
                i = (k + 1).min(b.len());
                continue;
            }
            i += css[i..].chars().next().map(char::len_utf8).unwrap_or(1);
        }
        splice(css, edits)
    }

    fn srcset(&mut self, v: &str, base: &Path, depth: usize) -> Option<String> {
        // A `data:` candidate has commas of its own: such a set is left as it is.
        if v.contains("data:") {
            return None;
        }
        let mut changed = false;
        let parts: Vec<String> = v
            .split(',')
            .map(|c| {
                let c = c.trim();
                let (url, rest) = c.split_once(char::is_whitespace).unwrap_or((c, ""));
                match self.file_uri(url, base, depth, "srcset") {
                    Some(u) => {
                        changed = true;
                        if rest.is_empty() { u } else { format!("{u} {}", rest.trim()) }
                    }
                    None => c.to_owned(),
                }
            })
            .collect();
        changed.then(|| parts.join(", "))
    }

    /// A `data:` URI for a local reference inside the page's folder, its own loads inlined when it is a stylesheet or
    /// a page; `None` leaves the reference as it is.
    fn file_uri(&mut self, raw: &str, base: &Path, depth: usize, kind: &str) -> Option<String> {
        let rel = local_ref(raw)?;
        if rel.is_empty() {
            return None;
        }
        let p = if rel.starts_with('/') { PathBuf::from(&rel) } else { base.join(&rel) };
        let p = p.canonicalize().ok()?;
        // Inside the page's folder, and nothing hidden on the way (`.ssh/`, `.env`).
        let inside = p.strip_prefix(&self.root).ok()?;
        if inside.components().any(|c| c.as_os_str().to_string_lossy().starts_with('.')) {
            return None;
        }
        let mime = mime_of(&p);
        if !allowed(kind, mime) {
            return None;
        }
        let meta = std::fs::metadata(&p).ok()?;
        if !meta.is_file() || meta.len() > MAX_ASSET || self.total + meta.len() > MAX_TOTAL {
            return None;
        }
        let data = std::fs::read(&p).ok()?;
        self.deps.push(p.clone());
        let data = match mime {
            "text/css" if depth < DEPTH => match String::from_utf8(data) {
                Ok(s) => self.css(&s, p.parent()?, depth + 1).into_bytes(),
                Err(e) => e.into_bytes(),
            },
            "text/html" if depth < DEPTH && kind != "link" => match String::from_utf8(data) {
                Ok(s) => self.html(&s, p.parent()?, depth + 1).into_bytes(),
                Err(e) => e.into_bytes(),
            },
            _ => data,
        };
        self.total += data.len() as u64;
        self.inlined += 1;
        let charset = if mime.starts_with("text/") || mime.ends_with("javascript") || mime.ends_with("+xml") { ";charset=utf-8" } else { "" };
        Some(format!("data:{mime}{charset};base64,{}", base64(&data)))
    }
}

fn is_ident_char(c: Option<char>) -> bool {
    c.is_some_and(|c| c.is_alphanumeric() || c == '-' || c == '_')
}

/// A reference to a local file, as a decoded relative or absolute path without query or fragment; `None` for
/// anything remote, a `data:`/`blob:`/`javascript:` URI or a bare fragment.
pub fn local_ref(raw: &str) -> Option<String> {
    let v = decode_entities(raw.trim());
    if v.starts_with('#') || v.starts_with("//") {
        return None;
    }
    let path = if let Some(rest) = strip_prefix_ci(&v, "file://") {
        let rest = rest.strip_prefix("localhost").unwrap_or(rest);
        if !rest.starts_with('/') {
            return None;
        }
        rest.to_owned()
    } else {
        // A scheme (`https:`, `data:`, `mailto:`): letters, digits, `+ - .` and a colon before any `/`.
        if let Some(c) = v.find(':') {
            let scheme = &v[..c];
            if !scheme.is_empty() && scheme.chars().all(|ch| ch.is_ascii_alphanumeric() || "+-.".contains(ch)) {
                return None;
            }
        }
        v.clone()
    };
    let path = path.split(['?', '#']).next().unwrap_or("");
    // `..` stays: the caller resolves the path and checks it is inside the page's folder.
    percent_decode(path)
}

fn strip_prefix_ci<'a>(s: &'a str, p: &str) -> Option<&'a str> {
    // Bytes, not a slice of `s`: a cut inside a multi-byte letter (`résumé.png`) would panic.
    (at_ci(s.as_bytes(), 0, p.as_bytes())).then(|| &s[p.len()..])
}

/// `pat` (ASCII) at byte `i` of `b`, ignoring case.
fn at_ci(b: &[u8], i: usize, pat: &[u8]) -> bool {
    b.get(i..i + pat.len()).is_some_and(|w| w.eq_ignore_ascii_case(pat))
}

fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_owned();
    }
    s.replace("&amp;", "&").replace("&quot;", "\"").replace("&#39;", "'").replace("&apos;", "'").replace("&lt;", "<").replace("&gt;", ">")
}

fn percent_decode(s: &str) -> Option<String> {
    if !s.contains('%') {
        return Some(s.to_owned());
    }
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let h = std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok());
            if let Some(h) = h {
                out.push(h);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8(out).ok()
}

pub fn mime_of(p: &Path) -> &'static str {
    let ext = p.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "heic" => "image/heic",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        "bmp" => "image/bmp",
        "css" => "text/css",
        "js" | "mjs" | "cjs" => "text/javascript",
        "json" => "application/json",
        "html" | "htm" => "text/html",
        "txt" | "md" | "csv" => "text/plain",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "mp4" | "m4v" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "mp3" => "audio/mpeg",
        "m4a" => "audio/mp4",
        "wav" => "audio/wav",
        "ogg" | "oga" => "audio/ogg",
        "flac" => "audio/flac",
        "vtt" => "text/vtt",
        "pdf" => "application/pdf",
        "wasm" => "application/wasm",
        _ => "application/octet-stream",
    }
}

pub fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if c.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

// MARK: Tags

struct Attr {
    name: String,
    value: String,
    /// The value's bytes in the source, quotes included (`start..end`).
    start: usize,
    end: usize,
}

struct Tag {
    name: String,
    attrs: Vec<Attr>,
    end: usize,
    self_closing: bool,
}

/// An opening tag at `at` (`<name attr="v" …>`), or `None` for anything else (`</x>`, `<!doctype>`, a lone `<`).
fn parse_tag(s: &str, at: usize) -> Option<Tag> {
    let b = s.as_bytes();
    let mut i = at + 1;
    if i >= b.len() || !b[i].is_ascii_alphabetic() {
        return None;
    }
    let ns = i;
    while i < b.len() && !b[i].is_ascii_whitespace() && b[i] != b'>' && b[i] != b'/' {
        i += 1;
    }
    let name = s[ns..i].to_owned();
    let mut attrs = vec![];
    loop {
        while i < b.len() && (b[i].is_ascii_whitespace() || (b[i] == b'/' && b.get(i + 1) != Some(&b'>'))) {
            i += 1;
        }
        if i >= b.len() {
            return None;
        }
        if b[i] == b'>' {
            return Some(Tag { name, attrs, end: i + 1, self_closing: false });
        }
        if b[i] == b'/' {
            return Some(Tag { name, attrs, end: (i + 2).min(b.len()), self_closing: true });
        }
        let an = i;
        while i < b.len() && !b[i].is_ascii_whitespace() && !matches!(b[i], b'=' | b'>' | b'/') {
            i += 1;
        }
        let aname = s[an..i].to_owned();
        let mut j = i;
        while j < b.len() && b[j].is_ascii_whitespace() {
            j += 1;
        }
        if j < b.len() && b[j] == b'=' {
            j += 1;
            while j < b.len() && b[j].is_ascii_whitespace() {
                j += 1;
            }
            if j < b.len() && (b[j] == b'"' || b[j] == b'\'') {
                let q = b[j] as char;
                let k = s[j + 1..].find(q).map(|k| j + 1 + k)?;
                attrs.push(Attr { name: aname, value: s[j + 1..k].to_owned(), start: j, end: k + 1 });
                i = k + 1;
            } else {
                let vs = j;
                while j < b.len() && !b[j].is_ascii_whitespace() && b[j] != b'>' {
                    j += 1;
                }
                attrs.push(Attr { name: aname, value: s[vs..j].to_owned(), start: vs, end: j });
                i = j;
            }
        } else {
            attrs.push(Attr { name: aname, value: String::new(), start: i, end: i });
        }
    }
}

fn find_ci(s: &str, from: usize, needle: &str) -> Option<usize> {
    let hay = s.as_bytes();
    let n = needle.as_bytes();
    (from..hay.len().saturating_sub(n.len() - 1)).find(|&k| hay[k..k + n.len()].eq_ignore_ascii_case(n))
}

/// `s` with each `(start, end, text)` range replaced (ranges in order, not overlapping).
fn splice(s: &str, mut edits: Vec<(usize, usize, String)>) -> String {
    if edits.is_empty() {
        return s.to_owned();
    }
    edits.sort_by_key(|e| e.0);
    let mut out = String::with_capacity(s.len());
    let mut at = 0;
    for (a, b, t) in edits {
        if a < at {
            continue;
        }
        out.push_str(&s[at..a]);
        out.push_str(&t);
        at = b;
    }
    out.push_str(&s[at..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_known_values() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn local_refs() {
        assert_eq!(local_ref("report-src/a%20b.png?v=2#x").as_deref(), Some("report-src/a b.png"));
        assert_eq!(local_ref("file:///tmp/x.png").as_deref(), Some("/tmp/x.png"));
        assert_eq!(local_ref("a&amp;b.png").as_deref(), Some("a&b.png"));
        assert_eq!(local_ref("résumé.png").as_deref(), Some("résumé.png"));
        assert_eq!(local_ref("zrzut-ąę.png").as_deref(), Some("zrzut-ąę.png"));
        assert_eq!(local_ref("é").as_deref(), Some("é"));
        for remote in ["https://x.dev/a.png", "//cdn/x.js", "data:image/png;base64,AA", "#top", "mailto:a@b", "javascript:void(0)", "blob:abc"] {
            assert_eq!(local_ref(remote), None, "{remote}");
        }
    }
}
