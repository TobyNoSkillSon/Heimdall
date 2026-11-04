//! What a viewer island shows: the renderer picked from the file's type, and the proposed actions on top of it.

use std::io::Read;
use std::path::Path;

use serde_json::Value;

use super::{j, Json};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ViewerKind {
    Markdown,
    /// Plain text or source code; `language` is a lowercase hint for light colouring (`None`: plain).
    Text(Option<String>),
    Image,
    Pdf,
    Audio,
    Video,
    Html,
    /// Anything else: QuickLook.
    Other,
    Missing,
}

const MARKDOWN: [&str; 5] = ["md", "markdown", "mdown", "mkd", "mdx"];
const CODE: &[&str] = &[
    "swift", "rs", "py", "js", "mjs", "cjs", "ts", "tsx", "jsx", "c", "h", "m", "mm", "cpp", "cc", "hpp", "go", "rb", "java", "kt",
    "sh", "zsh", "bash", "fish", "json", "jsonl", "toml", "yaml", "yml", "css", "scss", "sql", "lua", "php", "cs", "xml", "plist",
    "ini", "conf", "cfg", "env", "log", "csv", "tsv", "txt", "text", "diff", "patch", "metal", "glsl", "zig", "nix", "makefile",
    "dockerfile",
];
/// `CharacterSet.whitespaces`: spaces and tabs.
const WS: [char; 2] = [' ', '\t'];
const PLAIN: [&str; 5] = ["txt", "text", "log", "csv", "tsv"];

impl ViewerKind {
    /// Picks the renderer by extension and UTType; files with no known type are sniffed (UTF-8 text → text, else
    /// QuickLook).
    pub fn detect(path: &Path) -> ViewerKind {
        let Ok(meta) = std::fs::metadata(path) else { return ViewerKind::Missing };
        if meta.is_dir() {
            return ViewerKind::Other;
        }
        let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
        let name = path.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
        if MARKDOWN.contains(&ext.as_str()) {
            return ViewerKind::Markdown;
        }
        if ext == "html" || ext == "htm" || ext == "xhtml" || ext == "svg" {
            return ViewerKind::Html;
        }
        if CODE.contains(&ext.as_str()) {
            return ViewerKind::Text(if PLAIN.contains(&ext.as_str()) { None } else { Some(ext) });
        }
        if ["makefile", "dockerfile", "license", "readme"].contains(&name.as_str()) {
            return ViewerKind::Text(None);
        }
        #[cfg(target_os = "macos")]
        if let Some(k) = uttype_kind(&ext) {
            return k;
        }
        if looks_like_text(path) {
            ViewerKind::Text(None)
        } else {
            ViewerKind::Other
        }
    }

    /// Whether Heimdall's viewer island can show `path` (every image and every file a job island
    /// opens goes to the viewer; only what it cannot show goes to the system app): Markdown, text and code, pictures,
    /// PDFs, audio, video, HTML pages, and any other file type macOS declares (QuickLook draws it). Not a folder (an
    /// app is one), a missing file, or a file of no known type that is not text.
    pub fn shows(path: &Path) -> bool {
        match ViewerKind::detect(path) {
            ViewerKind::Missing => false,
            ViewerKind::Other => {
                if std::fs::metadata(path).map(|m| m.is_dir()).unwrap_or(true) {
                    return false;
                }
                #[cfg(target_os = "macos")]
                {
                    let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
                    return declared(&ext);
                }
                #[allow(unreachable_code)]
                false
            }
            _ => true,
        }
    }

    /// A short name for logs and tests.
    pub fn name(&self) -> String {
        match self {
            ViewerKind::Markdown => "markdown".into(),
            ViewerKind::Text(l) => l.as_ref().map(|l| format!("code:{l}")).unwrap_or_else(|| "text".into()),
            ViewerKind::Image => "image".into(),
            ViewerKind::Pdf => "pdf".into(),
            ViewerKind::Audio => "audio".into(),
            ViewerKind::Video => "video".into(),
            ViewerKind::Html => "html".into(),
            ViewerKind::Other => "quicklook".into(),
            ViewerKind::Missing => "missing".into(),
        }
    }
}

/// A file type macOS knows (QuickLook can draw it): declared, not a dynamic type made up for an unknown extension.
#[cfg(target_os = "macos")]
fn declared(ext: &str) -> bool {
    use objc2_foundation::NSString;
    use objc2_uniform_type_identifiers::*;
    if ext.is_empty() {
        return false;
    }
    UTType::typeWithFilenameExtension(&NSString::from_str(ext)).is_some_and(|t| t.isDeclared() && !t.isDynamic())
}

#[cfg(target_os = "macos")]
fn uttype_kind(ext: &str) -> Option<ViewerKind> {
    use objc2_foundation::NSString;
    use objc2_uniform_type_identifiers::*;
    if ext.is_empty() {
        return None;
    }
    let t = UTType::typeWithFilenameExtension(&NSString::from_str(ext))?;
    // SAFETY: the UTType constants are immutable framework statics.
    unsafe {
        let c = |u: &UTType| t.conformsToType(u);
        if c(UTTypePDF) {
            return Some(ViewerKind::Pdf);
        }
        if c(UTTypeHTML) {
            return Some(ViewerKind::Html);
        }
        if c(UTTypeImage) {
            return Some(ViewerKind::Image);
        }
        if c(UTTypeAudio) {
            return Some(ViewerKind::Audio);
        }
        if c(UTTypeMovie) || c(UTTypeVideo) || c(UTTypeAudiovisualContent) {
            return Some(ViewerKind::Video);
        }
        if c(UTTypeSourceCode) || c(UTTypeScript) {
            return Some(ViewerKind::Text(Some(ext.to_owned())));
        }
        if c(UTTypePlainText) || c(UTTypeJSON) || c(UTTypeXML) || c(UTTypeCommaSeparatedText) {
            return Some(ViewerKind::Text(None));
        }
    }
    if t.isDeclared() && !t.isDynamic() {
        return Some(ViewerKind::Other);
    }
    None
}

/// The first 4 KB decode as UTF-8 and hold no NUL bytes (a character cut at the window's end is still text).
pub fn looks_like_text(path: &Path) -> bool {
    let Ok(mut f) = std::fs::File::open(path) else { return false };
    let mut d = vec![0u8; 4096];
    let mut n = 0;
    while n < d.len() {
        match f.read(&mut d[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(_) => return false,
        }
    }
    d.truncate(n);
    if d.is_empty() {
        return true;
    }
    if d.contains(&0) {
        return false;
    }
    (0..4).filter(|cut| d.len() > *cut).any(|cut| std::str::from_utf8(&d[..d.len() - cut]).is_ok())
}

/// A proposed action on a viewer: `{"label": "Do all of it", "action": "do-all"}`. A click writes a `button` response.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ViewerAction {
    pub label: String,
    pub action: String,
}

impl ViewerAction {
    pub fn new(label: &str, action: &str) -> ViewerAction {
        ViewerAction { label: label.into(), action: action.into() }
    }

    /// Lowercase words joined by dashes ("Do all of it" → "do-all-of-it").
    pub fn slug(label: &str) -> String {
        let lower = label.to_lowercase();
        let words: Vec<&str> = lower.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).collect();
        if words.is_empty() {
            "ok".into()
        } else {
            words.join("-")
        }
    }

    /// An island's `actions`: objects with a label (and an action, else the label's slug) or bare strings. Entries
    /// without a label are skipped; duplicates by action keep the first.
    pub fn parse(value: Option<&Value>) -> Vec<ViewerAction> {
        let Some(arr) = j::arr(value) else { return vec![] };
        let mut out: Vec<ViewerAction> = vec![];
        for e in arr {
            let a = match e {
                Value::String(s) => Self::from_cli(s),
                Value::Object(o) => {
                    let label = j::str(o.get("label")).or(j::str(o.get("title"))).map(|s| s.trim()).unwrap_or("");
                    if label.is_empty() {
                        None
                    } else {
                        let act = j::str(o.get("action")).or(j::str(o.get("id"))).map(|s| s.trim_matches(WS)).unwrap_or("");
                        Some(ViewerAction { label: label.to_owned(), action: if act.is_empty() { Self::slug(label) } else { act.to_owned() } })
                    }
                }
                _ => None,
            };
            if let Some(a) = a {
                if !out.iter().any(|x| x.action == a.action) {
                    out.push(a);
                }
            }
        }
        out
    }

    /// The CLI's `--action "Label=action"` (no `=`: the action is the label's slug).
    pub fn from_cli(s: &str) -> Option<ViewerAction> {
        let mut it = s.splitn(2, '=');
        let label = it.next().unwrap_or("").trim_matches(WS);
        if label.is_empty() {
            return None;
        }
        let act = it.next().map(|a| a.trim_matches(WS)).unwrap_or("");
        Some(ViewerAction { label: label.to_owned(), action: if act.is_empty() { Self::slug(label) } else { act.to_owned() } })
    }

    pub fn json(&self) -> Json {
        j::object([("label", Value::from(self.label.clone())), ("action", Value::from(self.action.clone()))])
    }

    /// Rows of at most three, in order (the first is the primary).
    pub fn rows(actions: &[ViewerAction]) -> Vec<Vec<ViewerAction>> {
        actions.chunks(3).map(|c| c.to_vec()).collect()
    }
}
