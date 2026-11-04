//! What the user dropped onto the notch (`ui::drop`), as the agent's message: one line per item with its path, link or
//! text, and what it is (an image's size, a video's duration, a folder's contents, a small document's text inline).
//! Images also ride as attachments the model sees. `pipeline/drops.rs` decides when the message goes.

use std::path::Path;

use agent::protocol::Attachment;
use ui::drop::{DropItem, DropKind, Dropped};

/// A document's text goes inline up to this many bytes.
pub(crate) const INLINE_MAX: u64 = 8 * 1024;
/// At most this many images of one drop are attached; the rest go by path.
pub(crate) const MAX_IMAGES: usize = 6;

/// A drop ready for the agent.
#[derive(Clone, Debug)]
pub(crate) struct Message {
    /// The lines after "the user dropped onto the notch:".
    pub lines: Vec<String>,
    pub images: Vec<Attachment>,
    /// The island's word ("1 image"), for the evidence log.
    pub label: String,
}

impl Message {
    /// Sent alone: the user's typed message.
    pub(crate) fn alone(&self) -> String {
        format!("User dropped onto the notch:\n{}", self.lines.join("\n"))
    }

    /// Joined to his words: the lines under what he said.
    pub(crate) fn with_speech(&self) -> String {
        format!("[and dropped onto the notch as he said it:\n{}]", self.lines.join("\n"))
    }

    /// Dropped on the open pull-down while he typed (30 Oct 03:00): his typed words, the lines under them; with no
    /// words, as a drop alone.
    pub(crate) fn with_typed(&self, typed: &str) -> String {
        let typed = typed.trim();
        if typed.is_empty() {
            return self.alone();
        }
        format!("{typed}\n[and dropped onto the notch with it:\n{}]", self.lines.join("\n"))
    }
}

/// The message for a drop. Reads the files (sizes, folders, small documents, images); never fails: what cannot be
/// read is sent by its path. Files in iCloud that are not downloaded are never read (reading would download them).
pub(crate) fn describe(d: &Dropped) -> Message {
    let mut images = Vec::new();
    let lines = d.items.iter().map(|item| line(item, &mut images)).collect();
    Message { lines, images, label: ui::drop::summary(&d.items).1 }
}

/// The stand-in when describing takes too long: each item by its path, link or text, nothing read.
pub(crate) fn paths_only(d: &Dropped) -> Message {
    let lines = d
        .items
        .iter()
        .map(|item| match item.kind {
            DropKind::Link => format!("- link: {}", item.url.as_deref().unwrap_or_default()),
            DropKind::Text => format!("- text:\n```\n{}\n```", item.text.as_deref().unwrap_or_default().trim_end()),
            k => format!("- {}: {}", k.tag(), item.path.as_deref().map(|p| p.display().to_string()).unwrap_or_default()),
        })
        .collect();
    Message { lines, images: vec![], label: ui::drop::summary(&d.items).1 }
}

/// A file whose contents are in iCloud only (`SF_DATALESS`): its metadata is local, its bytes are not.
pub(crate) fn dataless(p: &Path) -> bool {
    use std::os::macos::fs::MetadataExt;
    const SF_DATALESS: u32 = 0x4000_0000;
    std::fs::metadata(p).is_ok_and(|m| m.st_flags() & SF_DATALESS != 0)
}

fn line(item: &DropItem, images: &mut Vec<Attachment>) -> String {
    let path = item.path.as_deref();
    let shown = path.map(|p| p.display().to_string()).unwrap_or_default();
    if path.is_some_and(dataless) {
        return format!("- {}: {shown} (in iCloud, not downloaded)", item.kind.tag());
    }
    match item.kind {
        DropKind::Link => format!("- link: {}", item.url.as_deref().unwrap_or_default()),
        DropKind::Text => format!("- text:\n```\n{}\n```", item.text.as_deref().unwrap_or_default().trim_end()),
        DropKind::Image => {
            let mut facts = vec![];
            let attached = path.and_then(|p| (images.len() < MAX_IMAGES).then(|| image_attachment(p)).flatten());
            if let Some((w, h)) = path.and_then(pixel_size) {
                facts.push(format!("{w}×{h}"));
            }
            if let Some(s) = path.and_then(file_size) {
                facts.push(s);
            }
            if let Some(a) = attached {
                images.push(a);
                facts.insert(0, "attached".into());
            } else {
                facts.insert(0, "not attached".into());
            }
            let source = item.url.as_deref().map(|u| format!(", from {u}")).unwrap_or_default();
            format!("- image: {shown} ({}){source}", facts.join(", "))
        }
        DropKind::Video | DropKind::Audio => {
            let mut facts = vec![];
            if let Some(s) = path.and_then(duration) {
                facts.push(clock(s));
            }
            if let Some(s) = path.and_then(file_size) {
                facts.push(s);
            }
            let tag = if item.kind == DropKind::Video { "video" } else { "audio" };
            format!("- {tag}: {shown}{}", paren(&facts))
        }
        DropKind::Folder => format!("- folder: {shown}{}", path.and_then(folder_summary).map(|s| format!(" ({s})")).unwrap_or_default()),
        DropKind::Document | DropKind::File | DropKind::Mixed => {
            let tag = if item.kind == DropKind::Document { "document" } else { "file" };
            let size = path.and_then(file_size);
            let mut out = format!("- {tag}: {shown}{}", size.map(|s| format!(" ({s})")).unwrap_or_default());
            // Only documents are read into the message: a dropped key or `.env` stays a path.
            if let Some(text) = path.filter(|_| item.kind == DropKind::Document).and_then(inline_text) {
                out.push_str(&format!(":\n```\n{}\n```", text.trim_end()));
            }
            out
        }
    }
}

fn paren(facts: &[String]) -> String {
    if facts.is_empty() { String::new() } else { format!(" ({})", facts.join(", ")) }
}

/// "48 MB", "2 KB", "512 bytes".
pub(crate) fn human_size(n: u64) -> String {
    const K: f64 = 1000.0;
    let f = n as f64;
    if f < K {
        format!("{n} bytes")
    } else if f < K * K {
        format!("{:.0} KB", f / K)
    } else if f < K * K * K {
        format!("{:.1} MB", f / K / K).replace(".0 MB", " MB")
    } else {
        format!("{:.1} GB", f / K / K / K).replace(".0 GB", " GB")
    }
}

fn file_size(p: &Path) -> Option<String> {
    let m = std::fs::metadata(p).ok()?;
    m.is_file().then(|| human_size(m.len()))
}

/// "1:23", "1:02:03".
pub(crate) fn clock(seconds: f64) -> String {
    let s = seconds.round().max(0.0) as u64;
    if s >= 3600 { format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60) } else { format!("{}:{:02}", s / 60, s % 60) }
}

/// "14 items: 9 files, 5 folders" (hidden ones not counted), "empty".
pub(crate) fn folder_summary(p: &Path) -> Option<String> {
    let (mut files, mut folders) = (0usize, 0usize);
    for e in std::fs::read_dir(p).ok()?.flatten() {
        if e.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        if e.file_type().is_ok_and(|t| t.is_dir()) { folders += 1 } else { files += 1 }
    }
    let plural = |n: usize, one: &str| format!("{n} {one}{}", if n == 1 { "" } else { "s" });
    Some(match (files, folders) {
        (0, 0) => "empty".into(),
        (f, 0) => plural(f, "file"),
        (0, d) => plural(d, "folder"),
        (f, d) => format!("{}: {}, {}", plural(f + d, "item"), plural(f, "file"), plural(d, "folder")),
    })
}

/// A small text file's contents (UTF-8, no NULs, at most `INLINE_MAX` bytes).
pub(crate) fn inline_text(p: &Path) -> Option<String> {
    let m = std::fs::metadata(p).ok()?;
    if !m.is_file() || m.len() > INLINE_MAX || m.len() == 0 {
        return None;
    }
    let bytes = std::fs::read(p).ok()?;
    let text = String::from_utf8(bytes).ok()?;
    (!text.contains('\0')).then_some(text)
}

fn pixel_size(p: &Path) -> Option<(u32, u32)> {
    // PNG and JPEG by their headers; other formats through Spotlight's metadata.
    if let Ok(dim) = image::ImageReader::open(p).and_then(|r| r.with_guessed_format()).map_err(|_| ()).and_then(|r| r.into_dimensions().map_err(|_| ())) {
        return Some(dim);
    }
    let w = mdls(p, "kMDItemPixelWidth")?.parse::<f64>().ok()?;
    let h = mdls(p, "kMDItemPixelHeight")?.parse::<f64>().ok()?;
    Some((w as u32, h as u32))
}

/// A media file's duration: Spotlight's, else `afinfo`'s estimate (audio tracks; Spotlight may not have indexed a
/// file that just arrived).
fn duration(p: &Path) -> Option<f64> {
    if let Some(s) = mdls(p, "kMDItemDurationSeconds").and_then(|s| s.parse::<f64>().ok()) {
        return Some(s);
    }
    let out = run(std::process::Command::new("/usr/bin/afinfo").arg(p))?;
    let text = String::from_utf8_lossy(&out);
    text.lines().find_map(|l| l.trim().strip_prefix("estimated duration:")).and_then(|v| v.split_whitespace().next()?.parse().ok())
}

fn mdls(p: &Path, key: &str) -> Option<String> {
    let out = run(std::process::Command::new("/usr/bin/mdls").args(["-raw", "-name", key]).arg(p))?;
    let v = String::from_utf8_lossy(&out).trim().to_string();
    (!v.is_empty() && v != "(null)").then_some(v)
}

/// A helper tool's output when it succeeds within `TOOL_LIMIT` (a network volume or a stuck file never holds a drop).
fn run(cmd: &mut std::process::Command) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut child = cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null()).spawn().ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut out = Vec::new();
        let _ = stdout.read_to_end(&mut out);
        out
    });
    let began = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let out = reader.join().ok()?;
                return status.success().then_some(out);
            }
            Ok(None) if began.elapsed() < TOOL_LIMIT => std::thread::sleep(std::time::Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

/// How long `sips`, `mdls` or `afinfo` may take for one file.
const TOOL_LIMIT: std::time::Duration = std::time::Duration::from_secs(4);

/// The image as the agent sees it: a PNG as is (fitted by `context_attachment`); anything else (JPEG, HEIC, TIFF, GIF,
/// WebP) converted to a PNG by `sips` first, fitted to the context's edge.
pub(crate) fn image_attachment(p: &Path) -> Option<Attachment> {
    let mut head = [0u8; 8];
    std::io::Read::read_exact(&mut std::fs::File::open(p).ok()?, &mut head).ok()?;
    if head.starts_with(b"\x89PNG") {
        return crate::bridge::context_attachment(std::fs::read(p).ok()?).ok();
    }
    let out = std::env::temp_dir().join(format!("heimdall-drop-{}-{}.png", std::process::id(), agent::now_ms()));
    let edge = crate::bridge::CONTEXT_EDGE_PX.to_string();
    let ok = run(std::process::Command::new("/usr/bin/sips").args(["-s", "format", "png", "-Z", &edge]).arg(p).arg("--out").arg(&out)).is_some();
    let png = ok.then(|| std::fs::read(&out).ok()).flatten();
    std::fs::remove_file(&out).ok();
    crate::bridge::context_attachment(png?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn item(kind: DropKind, path: Option<PathBuf>, url: Option<&str>, text: Option<&str>) -> DropItem {
        DropItem { kind, path, url: url.map(str::to_owned), text: text.map(str::to_owned) }
    }

    fn png(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbImage::from_pixel(w, h, image::Rgb([30, 120, 220]));
        let mut out = vec![];
        img.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png).unwrap();
        out
    }

    #[test]
    fn every_kind_has_its_line() {
        let dir = tempfile::tempdir().unwrap();
        let shot = dir.path().join("shot.png");
        std::fs::write(&shot, png(40, 20)).unwrap();
        let notes = dir.path().join("notes.md");
        std::fs::write(&notes, "# Plan\nShip it.\n").unwrap();
        let big = dir.path().join("big.txt");
        std::fs::write(&big, "x".repeat(INLINE_MAX as usize + 1)).unwrap();
        let folder = dir.path().join("Folder");
        std::fs::create_dir_all(folder.join("sub")).unwrap();
        std::fs::write(folder.join("a.txt"), "a").unwrap();
        std::fs::write(folder.join(".DS_Store"), "x").unwrap();
        let clip = dir.path().join("clip.mov");
        std::fs::write(&clip, vec![0u8; 2_400_000]).unwrap();
        let d = Dropped {
            items: vec![
                item(DropKind::Image, Some(shot.clone()), Some("https://example.com/shot.png"), None),
                item(DropKind::Document, Some(notes.clone()), None, None),
                item(DropKind::Document, Some(big.clone()), None, None),
                item(DropKind::Folder, Some(folder.clone()), None, None),
                item(DropKind::Video, Some(clip.clone()), None, None),
                item(DropKind::Link, None, Some("https://www.apple.com"), None),
                item(DropKind::Text, None, None, Some("selected words\n")),
            ],
            at_ms: 0,
        };
        let m = describe(&d);
        assert_eq!(m.images.len(), 1, "the image is attached");
        assert_eq!(m.images[0].media_type, "image/png");
        assert_eq!(m.label, "7 items");
        let l = &m.lines;
        assert_eq!(l[0], format!("- image: {} (attached, 40×20, {}), from https://example.com/shot.png", shot.display(), human_size(std::fs::metadata(&shot).unwrap().len())));
        assert_eq!(l[1], format!("- document: {} (16 bytes):\n```\n# Plan\nShip it.\n```", notes.display()));
        assert_eq!(l[2], format!("- document: {} (8 KB)", big.display()), "too long to inline");
        assert_eq!(l[3], format!("- folder: {} (2 items: 1 file, 1 folder)", folder.display()));
        assert!(l[4].starts_with(&format!("- video: {} (", clip.display())) && l[4].ends_with("2.4 MB)"), "{}", l[4]);
        assert_eq!(l[5], "- link: https://www.apple.com");
        assert_eq!(l[6], "- text:\n```\nselected words\n```");
        assert!(m.alone().starts_with("User dropped onto the notch:\n- image: "));
        assert!(m.with_speech().starts_with("[and dropped onto the notch as he said it:\n- image: ") && m.with_speech().ends_with(']'));
    }

    #[test]
    fn the_prompt_knows_the_words_a_drop_arrives_with() {
        let prompt = include_str!("../../agent/prompt/heimdall.md");
        let m = Message { lines: vec!["- link: https://a.b".into()], images: vec![], label: "Link".into() };
        assert!(prompt.contains("\"dropped onto the notch\""));
        assert!(m.alone().contains("dropped onto the notch") && m.with_speech().contains("dropped onto the notch"));
        assert!(m.with_typed("what is this?").starts_with("what is this?\n[and dropped onto the notch with it:\n- link: https://a.b"));
        assert_eq!(m.with_typed("  "), m.alone(), "no words: a drop alone");
    }

    #[test]
    fn only_documents_are_read_inline_and_the_stand_in_reads_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let env = dir.path().join(".env");
        std::fs::write(&env, "API_KEY=secret").unwrap();
        let d = Dropped { items: vec![item(DropKind::File, Some(env.clone()), None, None)], at_ms: 0 };
        let m = describe(&d);
        assert_eq!(m.lines[0], format!("- file: {} (14 bytes)", env.display()));
        assert!(!m.alone().contains("secret"));
        let q = paths_only(&Dropped { items: vec![item(DropKind::Image, Some(env.clone()), None, None), item(DropKind::Link, None, Some("https://a.b"), None)], at_ms: 0 });
        assert_eq!(q.lines, vec![format!("- image: {}", env.display()), "- link: https://a.b".to_string()]);
        assert!(!dataless(&env));
    }

    #[test]
    fn a_missing_file_still_goes_by_its_path() {
        let d = Dropped { items: vec![item(DropKind::Image, Some(PathBuf::from("/nowhere/gone.png")), None, None)], at_ms: 0 };
        let m = describe(&d);
        assert!(m.images.is_empty());
        assert_eq!(m.lines[0], "- image: /nowhere/gone.png (not attached)");
    }

    #[test]
    fn small_helpers() {
        assert_eq!(clock(83.4), "1:23");
        assert_eq!(clock(3723.0), "1:02:03");
        assert_eq!(human_size(512), "512 bytes");
        assert_eq!(human_size(2_000), "2 KB");
        assert_eq!(human_size(48_000_000), "48 MB");
        assert_eq!(human_size(1_250_000_000), "1.2 GB");
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(folder_summary(dir.path()).as_deref(), Some("empty"));
        std::fs::create_dir(dir.path().join("x")).unwrap();
        assert_eq!(folder_summary(dir.path()).as_deref(), Some("1 folder"));
    }

    #[test]
    fn more_images_than_the_cap_go_by_path() {
        let dir = tempfile::tempdir().unwrap();
        let items: Vec<_> = (0..MAX_IMAGES + 2)
            .map(|i| {
                let p = dir.path().join(format!("{i}.png"));
                std::fs::write(&p, png(8, 8)).unwrap();
                item(DropKind::Image, Some(p), None, None)
            })
            .collect();
        let m = describe(&Dropped { items, at_ms: 0 });
        assert_eq!(m.images.len(), MAX_IMAGES);
        assert!(m.lines.last().unwrap().contains("(not attached"));
        assert_eq!(m.label, format!("{} images", MAX_IMAGES + 2));
    }
}
