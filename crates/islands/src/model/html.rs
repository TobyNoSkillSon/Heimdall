//! Model-drawn island content ("kind of like HTML that can be rendered there, but really,
//! really cheap on compute … ideally HTML"). An `html` part carries a fragment of real HTML and CSS (inline SVG for
//! charts) written by the island agent. This file is the gate in front of the renderer: it keeps a generous subset
//! (layout, tables, text, inline SVG, local images, inline styles and one `<style>` block) and refuses or strips
//! everything that could run code, reach the network, read outside a local image or blow up the renderer.
//!
//! - No scripts: `<script>`, event handlers (`on…`), `javascript:` anywhere, `<iframe>`, `<object>`, `<embed>`, forms
//!   and media elements go; the web view also runs with page JavaScript off.
//! - No network: every `href`, `src` and CSS `url()` that is not a fragment (`#id`) or an allowed local image is
//!   dropped; the document's Content-Security-Policy (`default-src 'none'; img-src file:`) blocks the rest.
//! - Local images only: `<img src>` must be an absolute (or `~/`) path to an existing image file, at most `MAX_IMAGES`.
//! - Bounded: `MAX_BYTES` of input, `MAX_ELEMENTS` elements, `MAX_DEPTH` nesting; the renderer caps the height and the
//!   render time. Over a limit is an error: the island draws its usual parts instead.
//!
//! Chart tags (`<x-line>`, `<x-bars>`, `<x-spark>`, `<x-ring>`, `<x-dots>`, `<x-steps>`) expand into SVG here (`html_charts`).
//!
//! std only: Heimdall's app crate includes this file and `html_charts.rs` as they are (`#[path]`) to check the island
//! agent's answers.

use std::path::Path;

pub const MAX_BYTES: usize = 24 * 1024;
pub const MAX_ELEMENTS: usize = 600;
pub const MAX_DEPTH: usize = 24;
pub const MAX_IMAGES: usize = 4;
/// The tallest a drawn body may be, in points (taller content is cut).
pub const MAX_HEIGHT: f64 = 440.0;

/// Elements kept as they are (attributes filtered).
const KEEP: [&str; 71] = [
    "div", "span", "p", "b", "i", "u", "s", "strong", "em", "small", "sub", "sup", "br", "hr", "h1", "h2", "h3", "h4", "h5", "h6",
    "ul", "ol", "li", "dl", "dt", "dd", "table", "thead", "tbody", "tfoot", "tr", "td", "th", "caption", "colgroup", "col", "img",
    "figure", "figcaption", "section", "header", "footer", "article", "aside", "blockquote", "pre", "code", "mark", "abbr", "time",
    "meter", "progress", "label", "style",
    // Inline SVG.
    "svg", "g", "path", "line", "polyline", "polygon", "rect", "circle", "ellipse", "text", "tspan", "defs", "lineargradient",
    "radialgradient", "stop", "clippath", "mask",
];
/// Elements dropped with everything inside them.
const DROP_WITH_CONTENT: [&str; 22] = [
    "script", "iframe", "frame", "frameset", "object", "embed", "applet", "noscript", "template", "textarea", "select", "title",
    "video", "audio", "canvas", "foreignobject", "xmp", "noembed", "noframes", "math", "portal", "button",
];
const VOID: [&str; 6] = ["br", "hr", "img", "col", "meta", "link"];
pub const IMAGE_EXT: [&str; 10] = ["png", "jpg", "jpeg", "gif", "heic", "webp", "tif", "tiff", "bmp", "svg"];

#[derive(Debug, Clone, PartialEq)]
pub struct Clean {
    /// The fragment as it will be drawn.
    pub html: String,
    /// Every chart tag's data as the gate read it (`series`, `values`, `items`, `value`, `done`, `total`, `target`,
    /// `label`, `target-label`), for the agent check that its numbers are in the signals.
    pub chart_data: Vec<String>,
    /// Every `style` attribute and `<style>` block kept (bar widths and the like carry numbers too).
    pub styles: Vec<String>,
    /// Local images it shows (absolute paths).
    pub images: Vec<String>,
    /// What was stripped, for the agent's repair turn and the log.
    pub stripped: Vec<String>,
    pub elements: usize,
}

fn lower(s: &str) -> String { s.to_ascii_lowercase() }

/// CSS that can reach outside, or move: any `&`, `url(` other than a fragment, `image-set(`, `cross-fade(`, `image(`,
/// `element(`, `src(`, `@import`, `@font-face`, `expression(`, `javascript:`, bindings, `content:` (text the gate and
/// the agent check cannot see), animations and transitions (the body is a still). Checked as written and with
/// entities decoded (inside SVG a `<style>` decodes them: `u&#114;l(`); a backslash escape is refused outright.
fn css_reaches_out(css: &str) -> bool {
    // Any entity: WebKit decodes named ones inside SVG (`&lpar;`), which no list here could keep up with (re-review).
    if css.contains('&') {
        return true;
    }
    for form in [css.to_owned(), unentity(css)] {
        let l: String = lower(&form).chars().filter(|c| !c.is_whitespace()).collect();
        const BANNED: [&str; 17] = ["@import", "@font-face", "@keyframes", "expression(", "javascript:", "binding", "behavior:", "image-set(",
            "cross-fade(", "image(", "element(", "src(", "content:", "animation", "transition", "@namespace", "\\"];
        if BANNED.iter().any(|b| l.contains(b)) {
            return true;
        }
        let mut rest = l.as_str();
        while let Some(i) = rest.find("url(") {
            let after = rest[i + 4..].trim_start_matches(['"', '\'']);
            if !after.starts_with('#') {
                return true;
            }
            rest = &rest[i + 4..];
        }
    }
    false
}

/// A local image path the model may show: absolute or `~/`, an image extension, an existing file.
pub fn local_image(src: &str, exists: &dyn Fn(&Path) -> bool) -> Option<String> {
    let s = src.trim();
    let path = if let Some(p) = s.strip_prefix("file://") {
        percent_decode(p)
    } else if let Some(p) = s.strip_prefix("~/") {
        format!("{}/{}", std::env::var("HOME").unwrap_or_default(), p)
    } else {
        s.to_owned()
    };
    if !path.starts_with('/') || path.contains("/../") || path.ends_with("/..") {
        return None;
    }
    let ext = Path::new(&path).extension().and_then(|e| e.to_str()).map(lower)?;
    (IMAGE_EXT.contains(&ext.as_str()) && exists(Path::new(&path))).then_some(path)
}

pub fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        // Hex digits checked as bytes first: slicing at a byte that is inside a letter (`%aé`) would panic.
        if b[i] == b'%' && i + 2 < b.len() && b[i + 1].is_ascii_hexdigit() && b[i + 2].is_ascii_hexdigit() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A path as a `file://` URL (every byte outside the unreserved set escaped).
pub fn file_url(path: &str) -> String {
    let mut u = String::from("file://");
    for b in path.bytes() {
        if b.is_ascii_alphanumeric() || b"/-_.~".contains(&b) {
            u.push(b as char);
        } else {
            u.push_str(&format!("%{b:02X}"));
        }
    }
    u
}

fn escape_attr(v: &str) -> String { v.replace('&', "&amp;").replace('"', "&quot;").replace('<', "&lt;").replace('>', "&gt;") }

/// Undo the entities a model may put in an attribute, so `jav&#97;script:` is seen for what it is.
fn unentity(v: &str) -> String {
    let mut out = String::new();
    let mut rest = v;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        let end = tail.find(';').filter(|e| *e <= 10);
        match end {
            Some(e) => {
                let ent = &tail[1..e];
                let ch = match ent {
                    "amp" => Some('&'), "lt" => Some('<'), "gt" => Some('>'), "quot" => Some('"'), "apos" => Some('\''),
                    _ if ent.starts_with("#x") || ent.starts_with("#X") => u32::from_str_radix(&ent[2..], 16).ok().and_then(char::from_u32),
                    _ if ent.starts_with('#') => ent[1..].parse::<u32>().ok().and_then(char::from_u32),
                    _ => None,
                };
                match ch {
                    Some(c) => out.push(c),
                    None => out.push_str(&tail[..=e]),
                }
                rest = &tail[e + 1..];
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

struct Tag<'a> {
    name: String,
    attrs: Vec<(String, Option<String>)>,
    closing: bool,
    self_closing: bool,
    raw: &'a str,
}

/// Parse one tag starting at `<` (`s` begins with `<`); returns the tag and its byte length.
fn parse_tag(s: &str) -> Option<(Tag<'_>, usize)> {
    let b = s.as_bytes();
    let mut i = 1;
    let closing = b.get(i) == Some(&b'/');
    if closing {
        i += 1;
    }
    let start = i;
    while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'-' || b[i] == b':') {
        i += 1;
    }
    if i == start {
        return None;
    }
    let name = lower(&s[start..i]);
    let mut attrs = vec![];
    let mut self_closing = false;
    loop {
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= b.len() {
            return None;
        }
        if b[i] == b'>' {
            i += 1;
            break;
        }
        if b[i] == b'/' {
            self_closing = true;
            i += 1;
            continue;
        }
        let a0 = i;
        while i < b.len() && !b[i].is_ascii_whitespace() && b[i] != b'=' && b[i] != b'>' && b[i] != b'/' {
            i += 1;
        }
        let an = lower(&s[a0..i]);
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        let mut value = None;
        if b.get(i) == Some(&b'=') {
            i += 1;
            while i < b.len() && b[i].is_ascii_whitespace() {
                i += 1;
            }
            match b.get(i) {
                Some(&q) if q == b'"' || q == b'\'' => {
                    let v0 = i + 1;
                    let end = s[v0..].find(q as char)? + v0;
                    value = Some(s[v0..end].to_owned());
                    i = end + 1;
                }
                Some(_) => {
                    let v0 = i;
                    while i < b.len() && !b[i].is_ascii_whitespace() && b[i] != b'>' {
                        i += 1;
                    }
                    value = Some(s[v0..i].to_owned());
                }
                None => return None,
            }
        }
        if !an.is_empty() {
            attrs.push((an, value));
        }
    }
    Some((Tag { name, attrs, closing, self_closing, raw: &s[..i] }, i))
}

fn attr_name_ok(n: &str) -> bool { !n.is_empty() && n.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b':' || c == b'_') }

/// Clean a model's fragment. `exists` says whether a local image file is there (tests pass their own).
pub fn sanitize(src: &str, exists: &dyn Fn(&Path) -> bool) -> Result<Clean, String> {
    if src.len() > MAX_BYTES {
        return Err(format!("html is {} bytes; at most {MAX_BYTES}", src.len()));
    }
    if src.trim().is_empty() {
        return Err("html is empty".into());
    }
    let mut out = String::with_capacity(src.len());
    let mut stripped: Vec<String> = vec![];
    let mut images: Vec<String> = vec![];
    let mut chart_data: Vec<String> = vec![];
    let mut styles: Vec<String> = vec![];
    let mut stack: Vec<String> = vec![];
    let mut elements = 0usize;
    let note = |s: String, list: &mut Vec<String>| {
        if !list.contains(&s) && list.len() < 12 {
            list.push(s);
        }
    };
    let mut i = 0;
    while i < src.len() {
        let rest = &src[i..];
        let Some(lt) = rest.find('<') else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..lt]);
        i += lt;
        let rest = &src[i..];
        if rest.starts_with("<!--") {
            i += rest.find("-->").map(|e| e + 3).unwrap_or(rest.len());
            continue;
        }
        if rest.starts_with("<!") || rest.starts_with("<?") {
            i += rest.find('>').map(|e| e + 1).unwrap_or(rest.len());
            continue;
        }
        let Some((tag, len)) = parse_tag(rest) else {
            // A lone `<` is text.
            out.push_str("&lt;");
            i += 1;
            continue;
        };
        i += len;
        let name = tag.name.clone();
        if DROP_WITH_CONTENT.contains(&name.as_str()) {
            if !tag.closing {
                note(format!("<{name}>"), &mut stripped);
                if !tag.self_closing {
                    let close = format!("</{name}");
                    let tail = lower(&src[i..]);
                    i += tail.find(&close).map(|e| e + src[i + e..].find('>').map(|g| g + 1).unwrap_or(close.len())).unwrap_or(src.len() - i);
                }
            }
            continue;
        }
        if let Some(chart) = name.strip_prefix("x-") {
            if tag.closing {
                continue;
            }
            let attrs: Vec<(String, String)> = tag.attrs.iter().map(|(a, v)| (a.clone(), v.as_deref().map(unentity).unwrap_or_default())).collect();
            for (a, v) in &attrs {
                if ["series", "values", "items", "value", "done", "total", "target", "label", "target-label", "now"].contains(&a.as_str()) {
                    chart_data.push(v.clone());
                }
            }
            match super::html_charts::expand(chart, &attrs) {
                Some(svg) => {
                    elements += 8;
                    if elements > MAX_ELEMENTS {
                        return Err(format!("more than {MAX_ELEMENTS} elements"));
                    }
                    out.push_str(&svg);
                }
                None => note(format!("<{name}>"), &mut stripped),
            }
            continue;
        }
        if !KEEP.contains(&name.as_str()) {
            if !tag.closing {
                note(format!("<{name}>"), &mut stripped);
            }
            continue;
        }
        if tag.closing {
            if let Some(pos) = stack.iter().rposition(|t| *t == name) {
                stack.truncate(pos);
                out.push_str(&format!("</{name}>"));
            }
            continue;
        }
        if name == "style" {
            let tail = lower(&src[i..]);
            let end = tail.find("</style").unwrap_or(src.len() - i);
            let css = &src[i..i + end];
            i += end;
            if let Some(g) = src[i..].find('>') {
                i += g + 1;
            }
            if css_reaches_out(css) || css.contains('<') {
                note("a <style> block that reaches outside".into(), &mut stripped);
            } else {
                styles.push(css.to_owned());
                out.push_str("<style>");
                out.push_str(css);
                out.push_str("</style>");
            }
            continue;
        }
        elements += 1;
        if elements > MAX_ELEMENTS {
            return Err(format!("more than {MAX_ELEMENTS} elements"));
        }
        let mut kept = String::new();
        let mut img_src: Option<String> = None;
        for (an, av) in &tag.attrs {
            let v = av.as_deref().map(unentity).unwrap_or_default();
            let lv: String = lower(&v).chars().filter(|c| !c.is_whitespace() && !c.is_control()).collect();
            if !attr_name_ok(an) || an.starts_with("on") || lv.contains("javascript:") || lv.contains("vbscript:") || lv.starts_with("data:") {
                note(format!("{an}="), &mut stripped);
                continue;
            }
            match an.as_str() {
                "src" if name == "img" => {
                    match local_image(&v, exists) {
                        Some(p) if images.len() < MAX_IMAGES || images.contains(&p) => img_src = Some(p),
                        _ => note(format!("img src {v}"), &mut stripped),
                    }
                    continue;
                }
                "href" | "xlink:href" => {
                    // Gradients and clip paths name fragments; anything else is a link nowhere.
                    if !v.trim().starts_with('#') {
                        note(format!("{an}="), &mut stripped);
                        continue;
                    }
                }
                "src" | "srcset" | "action" | "formaction" | "background" | "poster" | "ping" | "http-equiv" | "srcdoc" | "data" | "content" | "manifest" | "codebase" | "lowsrc" | "dynsrc" => {
                    note(format!("{an}="), &mut stripped);
                    continue;
                }
                _ => {}
            }
            if (an == "style" || lv.contains("url(")) && css_reaches_out(&v) {
                note(format!("{an} that reaches outside"), &mut stripped);
                continue;
            }
            if an == "style" {
                styles.push(v.clone());
            }
            match av {
                Some(_) => kept.push_str(&format!(" {an}=\"{}\"", escape_attr(&v))),
                None => kept.push_str(&format!(" {an}")),
            }
        }
        if name == "img" {
            let Some(p) = img_src else { continue };
            if !images.contains(&p) {
                images.push(p.clone());
            }
            kept.push_str(&format!(" src=\"{}\"", escape_attr(&file_url(&p))));
        }
        let _ = tag.raw;
        if VOID.contains(&name.as_str()) || tag.self_closing && is_svg_leaf(&name) {
            out.push_str(&format!("<{name}{kept}{}>", if tag.self_closing { "/" } else { "" }));
            continue;
        }
        if tag.self_closing {
            out.push_str(&format!("<{name}{kept}/>"));
            continue;
        }
        stack.push(name.clone());
        if stack.len() > MAX_DEPTH {
            return Err(format!("nested deeper than {MAX_DEPTH}"));
        }
        out.push_str(&format!("<{name}{kept}>"));
    }
    while let Some(t) = stack.pop() {
        out.push_str(&format!("</{t}>"));
    }
    if elements == 0 && out.trim().is_empty() {
        return Err("nothing left to draw".into());
    }
    Ok(Clean { html: out, images, stripped, elements, chart_data, styles })
}

fn is_svg_leaf(n: &str) -> bool { matches!(n, "path" | "line" | "polyline" | "polygon" | "rect" | "circle" | "ellipse" | "stop") }

/// The visible text of a cleaned fragment (tags, `<style>` blocks and SVG attributes gone): what the user reads, for the
/// agent check that every number on the island is in the signals.
pub fn visible_text(html: &str) -> String {
    let mut out = String::new();
    let mut i = 0;
    let l = lower(html);
    while i < html.len() {
        let rest = &html[i..];
        match rest.find('<') {
            None => {
                out.push_str(rest);
                break;
            }
            Some(lt) => {
                out.push_str(&rest[..lt]);
                out.push(' ');
                i += lt;
                if l[i..].starts_with("<style") {
                    i += l[i..].find("</style>").map(|e| e + 8).unwrap_or(html.len() - i);
                } else {
                    i += html[i..].find('>').map(|e| e + 1).unwrap_or(html.len() - i);
                }
            }
        }
    }
    unentity(&out)
}

/// Heimdall's dark island style for model HTML: the system's neutrals and four accents as CSS variables (
/// neutrals by default, the Goals green and Heimdall's blue as accents, orange and red only for a block or a failure;
/// the retired hues are neutral aliases), SF at 13 pt, tabular figures, and a few helper classes the prompt names. Animations and transitions are off (the body is a still).
pub const BASE_CSS: &str = r#":root{color-scheme:dark;--ink:rgba(255,255,255,.92);--ink2:rgba(255,255,255,.58);--ink3:rgba(255,255,255,.34);--line:rgba(255,255,255,.12);--fill:rgba(255,255,255,.07);--blue:#0A84FF;--green:#30D158;--orange:#FF9F0A;--red:#FF453A;--grey:var(--ink2);--purple:var(--ink2);--pink:var(--ink2);--cyan:var(--ink2);--teal:var(--ink2);--indigo:var(--ink2);--yellow:var(--ink2);--mint:var(--ink2)}
*{box-sizing:border-box;animation:none!important;transition:none!important}
html,body{margin:0;padding:0;background:transparent;overflow:hidden}
body{font:13px/1.35 -apple-system,BlinkMacSystemFont,system-ui,sans-serif;color:var(--ink);-webkit-font-smoothing:antialiased;font-variant-numeric:tabular-nums;-webkit-user-select:none;cursor:default}
#i{display:flow-root;width:100%}
#i>:first-child{margin-top:0}#i>:last-child{margin-bottom:0}
p{margin:0 0 6px}h1,h2,h3,h4{margin:0 0 6px;font-weight:600;letter-spacing:-.01em}h1{font-size:22px}h2{font-size:17px}h3{font-size:14px}h4{font-size:13px}
img{max-width:100%;max-height:180px;object-fit:cover;object-position:top;display:block;border-radius:8px}svg{display:block;overflow:visible}svg text{font-family:-apple-system,system-ui;fill:var(--ink2);font-size:10px}
table{border-collapse:collapse;width:100%}th,td{padding:3px 0;text-align:left;vertical-align:baseline}th{color:var(--ink2);font-weight:500;font-size:11px}td+td,th+th{padding-left:10px}tr+tr td{border-top:.5px solid var(--line)}
ul,ol{margin:0;padding-left:18px}li{margin:1px 0}hr{border:0;border-top:.5px solid var(--line);margin:8px 0}
.row{display:flex;gap:12px;align-items:center}.col{flex:1;min-width:0}.grid2{display:grid;grid-template-columns:1fr 1fr;gap:10px}.grid3{display:grid;grid-template-columns:1fr 1fr 1fr;gap:8px}
.big{font-size:30px;font-weight:600;letter-spacing:-.02em;line-height:1.05}.mid{font-size:19px;font-weight:600;line-height:1.1}.unit{color:var(--ink2);font-size:12px;font-weight:500}
.dim{color:var(--ink2)}.faint{color:var(--ink3)}.num{text-align:right;font-variant-numeric:tabular-nums}.mono{font-family:ui-monospace,SFMono-Regular,Menlo,monospace;font-size:12px}
.label{color:var(--ink2);font-size:11px;font-weight:500}
.card{background:var(--fill);border-radius:10px;padding:8px 10px}
.bar{height:6px;border-radius:3px;background:var(--fill);overflow:hidden}.bar>i{display:block;height:100%;border-radius:3px;background:var(--ink)}
.pill{display:inline-block;padding:1px 7px;border-radius:9px;background:var(--fill);font-size:11px;font-weight:500}
.ok{color:var(--green)}.bad{color:var(--red)}.warn{color:var(--orange)}
"#;

/// The whole document the renderer loads: the CSP, the base style, the fragment at `width` points.
pub fn document(fragment: &str, width: f64) -> String {
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; img-src file:; style-src 'unsafe-inline'; script-src 'none'; font-src 'none'; connect-src 'none'; media-src 'none'; frame-src 'none'; form-action 'none'\"><style>{BASE_CSS}body{{width:{w}px}}</style></head><body><div id=\"i\">{fragment}</div></body></html>",
        w = width.round() as i64
    )
}

/// The render cache key: the fragment and the width it is drawn at.
pub fn key(fragment: &str, width: f64, scale: f64) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    fragment.hash(&mut h);
    (width.round() as i64, (scale * 100.0) as i64).hash(&mut h);
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn yes(_: &Path) -> bool { true }
    fn no(_: &Path) -> bool { false }

    #[test]
    fn layout_svg_and_tables_pass_through() {
        let src = r##"<div class="row"><div class="col"><span class="big">8.1</span> <span class="unit">of 8.5</span></div>
<svg viewBox="0 0 100 40" width="100%" height="40"><defs><linearGradient id="g"><stop offset="0" stop-color="#0A84FF"/></linearGradient></defs>
<polyline points="0,30 50,20 100,5" fill="none" stroke="url(#g)" stroke-width="2"/><circle cx="100" cy="5" r="3"/></svg></div>
<table><tr><th>Round</th><th class="num">Opus</th></tr><tr><td>3</td><td class="num">8.1</td></tr></table>"##;
        let c = sanitize(src, &yes).unwrap();
        assert!(c.stripped.is_empty(), "{:?}", c.stripped);
        assert!(c.html.contains("<polyline points=\"0,30 50,20 100,5\" fill=\"none\" stroke=\"url(#g)\" stroke-width=\"2\"/>"));
        assert!(c.html.contains("<lineargradient id=\"g\">") && c.html.contains("<table>"));
        assert_eq!(visible_text(&c.html).split_whitespace().collect::<Vec<_>>(), ["8.1", "of", "8.5", "Round", "Opus", "3", "8.1"]);
    }

    #[test]
    fn scripts_handlers_and_remote_urls_go() {
        let src = r##"<div onclick="x()" style="color:red">a<script>alert(1)</script>b<iframe src="https://x.com"></iframe>
<img src="https://example.com/a.png"><a href="https://x.com">c</a><span style="background:url(https://x.com/t.png)">d</span>
<svg><a xlink:href="javascript:alert(1)">e</a></svg><div style="background:url(&quot;https://x&quot;)">f</div><p title="jav&#97;script:x">g</p>
<style>@import url(https://x.com/s.css); .a{color:red}</style><style>.b{color:var(--blue)}</style></div>"##;
        let c = sanitize(src, &yes).unwrap();
        for bad in ["script", "alert", "iframe", "https", "onclick", "@import", "javascript"] {
            assert!(!lower(&c.html).contains(bad), "{bad} left in {}", c.html);
        }
        assert!(c.html.contains("style=\"color:red\"") && c.html.contains(".b{color:var(--blue)}"));
        assert_eq!(visible_text(&c.html).split_whitespace().collect::<String>(), "abcdefg");
    }

    #[test]
    fn local_images_only_and_as_file_urls() {
        let c = sanitize(r##"<img src="/Users/t/a b.png"><img src="/etc/passwd"><img src="relative.png"><img src="/x/../y.png">"##, &yes).unwrap();
        assert_eq!(c.images, ["/Users/t/a b.png"]);
        assert!(c.html.contains("src=\"file:///Users/t/a%20b.png\""));
        let missing = sanitize(r##"<p>x</p><img src="/Users/t/gone.png">"##, &no).unwrap();
        assert!(missing.images.is_empty() && !missing.html.contains("img"));
    }

    #[test]
    fn limits_refuse_rather_than_hang() {
        assert!(sanitize(&"x".repeat(MAX_BYTES + 1), &yes).is_err());
        assert!(sanitize(&"<span>".repeat(MAX_DEPTH + 1), &yes).is_err());
        assert!(sanitize(&"<br>".repeat(MAX_ELEMENTS + 1), &yes).is_err());
        assert!(sanitize("  ", &yes).is_err());
        assert!(sanitize("<script>x</script>", &yes).is_err());
        // Unclosed elements are closed; a lone < is text.
        let c = sanitize("<div><b>1 < 2", &yes).unwrap();
        assert_eq!(c.html, "<div><b>1 &lt; 2</b></div>");
    }

    #[test]
    fn chart_tags_expand_and_their_numbers_are_visible() {
        let c = sanitize(r##"<x-line series="6.03 6.57 7.63" target="8.5" ends="1"></x-line><x-nope a="1">"##, &yes).unwrap();
        assert!(c.html.starts_with("<svg") && !c.html.contains("x-line") && c.stripped == ["<x-nope>"]);
        assert!(visible_text(&c.html).contains("8.5") && visible_text(&c.html).contains("7.63"));
    }

    #[test]
    fn css_that_reaches_a_file_or_moves_is_refused_however_it_is_spelled() {
        for css in [
            "<svg><style>.q{background:u&#114;l(/x.png)}</style></svg>",
            "<div style=\"background-image:image-set('/x.png' 1x)\">a</div>",
            "<div style=\"background:-webkit-image-set(url(/x.png) 1x)\">a</div>",
            "<div style=\"background:cross-fade(url(#a), url(#b), 50%)\">a</div>",
            "<style>.a::after{content:'99%'}</style><p class=a>x</p>",
            "<style>@keyframes s{to{opacity:0}} .a{animation:s 1s!important}</style><p>x</p>",
            "<p style=\"transition:all 1s\">x</p>",
            "<p style=\"background:u\\72l(/x.png)\">x</p>",
            "<svg><style>.q{background:url&lpar;x.png)}</style></svg>",
        ] {
            let c = sanitize(css, &|_| true).unwrap();
            let l = lower(&c.html);
            assert!(!l.contains("x.png") && !l.contains("image-set") && !l.contains("content:") && !l.contains("animation") && !l.contains("transition"), "{css} -> {}", c.html);
            assert!(!c.stripped.is_empty(), "{css}");
        }
    }

    /// Any text at all: the gate answers (a fragment or a refusal) and never panics.
    #[test]
    fn any_text_is_answered_without_a_panic() {
        let pieces = ["<", ">", "</", "<div", "<img src=\"file:///x%aé.png\">", "%", "%a", "é", "ł", "&#", "&#x", ";", "\"", "'", "=", " ", "<x-line series=\"",
            "<style>", "</style>", "<svg>", "url(", "&amp;", "<!--", "-->", "<script>", "a", "1.5", "|", "<x-ring value=", "<p style=\"", "\n"];
        let mut seed = 0x9E3779B97F4A7C15u64;
        for _ in 0..3000 {
            let mut t = String::new();
            for _ in 0..(seed % 40) {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                t.push_str(pieces[(seed % pieces.len() as u64) as usize]);
            }
            if let Ok(c) = sanitize(&t, &|_| true) {
                let _ = visible_text(&c.html);
            }
            let _ = visible_text(&t);
            let _ = local_image(&t, &|_| true);
        }
    }

    #[test]
    fn the_document_locks_everything_down() {
        let d = document("<p>x</p>", 328.0);
        assert!(d.contains("default-src 'none'") && d.contains("script-src 'none'") && d.contains("body{width:328px}"));
    }
}
