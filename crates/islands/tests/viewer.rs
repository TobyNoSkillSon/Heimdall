//! Swift `IslandModelTests/ViewerTests.swift`: viewer kinds, sniffing, path forms and proposed actions.

use std::path::{Path, PathBuf};

use islands::jobj;
use islands::model::island::viewer_path;
use islands::model::*;
use serde_json::{json, Value};

struct Dir(PathBuf);
impl Dir {
    fn new(tag: &str) -> Dir {
        let d = std::env::temp_dir().join(format!("vk-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        Dir(d)
    }
    fn file(&self, name: &str, data: &[u8]) -> PathBuf {
        let u = self.0.join(name);
        std::fs::write(&u, data).unwrap();
        u
    }
}
impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn kind(p: &Path) -> String {
    ViewerKind::detect(p).name()
}

#[test]
fn kinds_by_extension() {
    let d = Dir::new("ext");
    let cases = [
        ("notes.md", "markdown"), ("README.markdown", "markdown"),
        ("main.swift", "code:swift"), ("lib.rs", "code:rs"), ("x.py", "code:py"), ("data.json", "code:json"),
        ("log.txt", "text"), ("server.log", "text"), ("table.csv", "text"),
        ("photo.png", "image"), ("photo.JPG", "image"), ("shot.heic", "image"), ("anim.gif", "image"),
        ("paper.pdf", "pdf"),
        ("song.mp3", "audio"), ("voice.m4a", "audio"), ("take.wav", "audio"), ("clip.aiff", "audio"), ("x.flac", "audio"),
        ("clip.mov", "video"), ("clip.mp4", "video"), ("clip.m4v", "video"),
        ("page.html", "html"), ("page.htm", "html"), ("logo.svg", "html"),
        ("deck.key", "quicklook"), ("sheet.xlsx", "quicklook"), ("doc.docx", "quicklook"), ("arch.zip", "quicklook"),
    ];
    for (name, want) in cases {
        assert_eq!(kind(&d.file(name, b"hello\n")), want, "{name}");
    }
}

#[test]
fn unknown_types_are_sniffed() {
    let d = Dir::new("sniff");
    assert_eq!(kind(&d.file("Makefile", b"hello\n")), "text");
    assert_eq!(kind(&d.file("notes.zzunknown", b"just words\n")), "text");
    assert_eq!(kind(&d.file("blob.zzunknown", &[0x00, 0xFF, 0x10, 0x00])), "quicklook");
    assert_eq!(kind(&d.file("empty.zzunknown", b"")), "text");
    // A multi-byte character cut by the 4 KB sniff window is still text.
    let s = "a".repeat(4095) + "é tail";
    assert_eq!(kind(&d.file("cut.zzunknown", s.as_bytes())), "text");
}

#[test]
fn missing_and_folders() {
    let d = Dir::new("missing");
    assert_eq!(ViewerKind::detect(&d.0.join("gone.md")), ViewerKind::Missing);
    assert_eq!(ViewerKind::detect(&d.0), ViewerKind::Other);
}

#[test]
fn path_forms() {
    assert_eq!(viewer_path("file:///tmp/a%20b.md"), "/tmp/a b.md");
    let home = std::env::var("HOME").unwrap();
    assert_eq!(viewer_path("~/x.md"), format!("{home}/x.md"));
    assert_eq!(viewer_path("/tmp/../tmp/y.md"), "/tmp/y.md");
}

#[test]
fn actions_parse() {
    let raw = json!([
        {"label": "Do all of it", "action": "do-all"},
        {"label": "Only the first"},
        "Skip=skip",
        {"action": "nolabel"},
        {"label": "  ", "action": "blank"},
        {"label": "Again", "action": "do-all"},
        42
    ]);
    assert_eq!(
        ViewerAction::parse(Some(&raw)),
        [ViewerAction::new("Do all of it", "do-all"), ViewerAction::new("Only the first", "only-the-first"), ViewerAction::new("Skip", "skip")]
    );
    assert_eq!(ViewerAction::parse(None), []);
    assert_eq!(ViewerAction::parse(Some(&Value::from("nope"))), []);
    assert_eq!(Island::new("v", jobj! {"actions": raw}).viewer_actions().len(), 3);
}

#[test]
fn cli_action_form() {
    assert_eq!(ViewerAction::from_cli("Do all=do-all"), Some(ViewerAction::new("Do all", "do-all")));
    assert_eq!(ViewerAction::from_cli("Ship it!"), Some(ViewerAction::new("Ship it!", "ship-it")));
    assert_eq!(ViewerAction::from_cli("Keep = "), Some(ViewerAction::new("Keep", "keep")));
    assert_eq!(ViewerAction::from_cli("a=b=c"), Some(ViewerAction::new("a", "b=c")));
    assert_eq!(ViewerAction::from_cli("=x"), None);
    assert_eq!(ViewerAction::from_cli(""), None);
}

#[test]
fn rows_of_three_primary_first() {
    let a: Vec<ViewerAction> = (1..=5).map(|k| ViewerAction::new(&format!("A{k}"), &format!("a{k}"))).collect();
    let rows = ViewerAction::rows(&a);
    assert_eq!(rows.iter().map(Vec::len).collect::<Vec<_>>(), [3, 2]);
    assert_eq!(rows[0][0].action, "a1");
    assert_eq!(ViewerAction::rows(&[]).len(), 0);
}
