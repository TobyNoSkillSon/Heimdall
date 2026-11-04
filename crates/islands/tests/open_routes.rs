//! Where a click on a job island opens what it points at ("every image, and every file a job island
//! opens on click, must open in Heimdall's own floating viewer"), and how the viewer knows where it came from (07:49:
//! it grows out of the clicked element, not the notch). Model level: no windows, no events.

use islands::jobj;
use islands::model::island::{body_rect_on_screen, frame_on_arrival, frame_on_update, image_part_target, launchable, open_request, Target};
use islands::model::*;
use std::path::{Path, PathBuf};

fn dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("islands-open-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn file(d: &Path, name: &str, body: &[u8]) -> String {
    let p = d.join(name);
    std::fs::write(&p, body).unwrap();
    p.to_string_lossy().into_owned()
}

const PNG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 0];

fn viewer(t: &str) -> bool { matches!(OpenTarget::route(t), Some(OpenRoute::Viewer(_))) }

#[test]
fn every_kind_the_viewer_shows_opens_there_and_nothing_launches() {
    use std::os::unix::fs::PermissionsExt;
    let d = dir("kinds");
    // The viewer's kinds (viewer_kind.rs): Markdown, text and code, pictures, PDF, audio, video, HTML, and declared
    // types QuickLook draws. A script is shown as text, never run.
    for (name, body) in [
        ("plan.md", b"# Plan".as_slice()), ("notes.txt", b"hi"), ("main.rs", b"fn main() {}"), ("data.json", b"{}"),
        ("shot.png", PNG), ("SHOT.PNG", PNG), ("photo.JPG", b"\xff\xd8\xff"), ("report.html", b"<h1>r</h1>"), ("page.htm", b"<p>"),
        ("paper.pdf", b"%PDF-1.4"), ("take.wav", b"RIFF"), ("song.MP3", b"ID3"), ("clip.mp4", b"\0\0\0\x18ftyp"), ("clip.mov", b"\0\0\0\x14ftyp"),
        ("letter.docx", b"PK"), ("sheet.xlsx", b"PK"), ("build.sh", b"#!/bin/sh\nrm -rf /"),
    ] {
        let p = file(&d, name, body);
        assert!(viewer(&p), "{name} opens in the viewer");
    }
    let script = file(&d, "run.py", b"print(1)");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(viewer(&script), "an executable script is shown as text");
    // Review, 30 Oct: a click never launches anything. Folders and bundles are only revealed in Finder; launchable
    // types, executables that are not text and files of no known type open nowhere; other URL schemes too.
    let app = d.join("Tool.app");
    std::fs::create_dir_all(&app).unwrap();
    assert_eq!(OpenTarget::route(app.to_str().unwrap()), Some(OpenRoute::Reveal(app.to_string_lossy().into_owned())), "revealed, never opened");
    assert_eq!(OpenTarget::route(d.to_str().unwrap()), Some(OpenRoute::Reveal(d.to_string_lossy().into_owned())));
    for name in ["setup.pkg", "Install.mpkg", "run.command", "go.scpt", "flow.workflow", "site.webloc", "x.terminal", "app.dmg", "blob.zzqx"] {
        let p = file(&d, name, &[0, 159, 146, 150, 0, 1]);
        assert_eq!(OpenTarget::route(&p), None, "{name} opens nowhere");
    }
    let bin = file(&d, "tool", &[0xcf, 0xfa, 0xed, 0xfe, 0, 0, 0, 0]);
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(launchable(Path::new(&bin)));
    assert_eq!(OpenTarget::route(&bin), None, "an executable binary");
    let png = file(&d, "fake.png", PNG);
    std::fs::set_permissions(&png, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(OpenTarget::route(&png), None, "an executable bit on what is not text");
    assert_eq!(OpenTarget::route(&d.join("gone.md").to_string_lossy()), None, "missing");
    assert_eq!(OpenTarget::route("https://example.com/a.png"), Some(OpenRoute::System(Target::Url("https://example.com/a.png".into()))), "web pages stay in the browser");
    assert_eq!(OpenTarget::route("http://127.0.0.1:8765/report.html"), Some(OpenRoute::System(Target::Url("http://127.0.0.1:8765/report.html".into()))));
    for scheme in ["x-apple.systempreferences://com.apple.preference.security", "ssh://host", "vscode://file/x", "smb://server/share"] {
        assert_eq!(OpenTarget::route(scheme), None, "{scheme}: no app is launched by a URL");
    }
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn file_urls_and_home_paths_reach_the_viewer_and_relative_paths_do_not() {
    let home = std::env::var("HOME").unwrap();
    let d = PathBuf::from(&home).join(format!(".islands-open-test-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    let p = file(&d, "a b.png", PNG);
    let name = d.file_name().unwrap().to_string_lossy().into_owned();
    assert!(viewer(&format!("~/{name}/a b.png")), "a ~ path");
    assert!(viewer(&format!("file://{}", p.replace(' ', "%20"))), "a file URL, percent escapes decoded");
    file(&d, "plain.png", PNG);
    assert!(!viewer(&format!("{name}/plain.png")), "a relative path is never resolved (review, 30 Oct)");
    assert!(!viewer(&format!("./{name}/plain.png")));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_picture_without_open_opens_itself_and_one_with_open_opens_that() {
    assert_eq!(image_part_target(&jobj! {"type": "image", "path": "/x/shot.png"}).as_deref(), Some("/x/shot.png"), "a picture without `open` opens in the viewer itself");
    assert_eq!(image_part_target(&jobj! {"type": "image", "path": "/x/shot.png", "open": "/x/report.html"}).as_deref(), Some("/x/report.html"));
    assert_eq!(image_part_target(&jobj! {"type": "image", "src": "/x/old.png"}).as_deref(), Some("/x/old.png"));
    assert_eq!(image_part_target(&jobj! {"type": "image", "path": " "}), None);
    // The latest-picture thumbnail (an island's `image`) becomes a picture part that opens itself, as before.
    let d = dir("pic");
    let p = file(&d, "latest.png", PNG);
    let (route, origin) = open_request(&p, "decider", Some(Rect::new(1372.0, 739.0, 141.0, 176.0)), Some(&p)).unwrap();
    assert_eq!(route, OpenRoute::Viewer(p.clone()));
    let o = origin.unwrap();
    assert_eq!((o.rect, o.image.as_deref(), o.island.as_deref()), (Rect::new(1372.0, 739.0, 141.0, 176.0), Some(p.as_str()), Some("decider")));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_click_carries_its_frame_into_the_viewer_and_no_frame_means_the_notch() {
    let d = dir("frame");
    let p = file(&d, "report.md", b"# R");
    // The clicked element's body rect becomes a screen rect: body (x 16, y 120) under a shape at (0, 0) in a window at
    // (1290, 665) 360 x 332 (y up), with the body 48 pt down and 16 pt in.
    let s = body_rect_on_screen(Rect::new(93.0, 50.0, 141.0, 160.0), Rect::new(0.0, 0.0, 360.0, 332.0), Rect::new(1290.0, 665.0, 360.0, 332.0), 16.0, 48.0, 0.0);
    assert_eq!(s, Rect::new(1399.0, 665.0 + 332.0 - 98.0 - 160.0, 141.0, 160.0));
    let scrolled = body_rect_on_screen(Rect::new(93.0, 50.0, 141.0, 160.0), Rect::new(0.0, 0.0, 360.0, 332.0), Rect::new(1290.0, 665.0, 360.0, 332.0), 16.0, 48.0, 30.0);
    assert_eq!(scrolled.y, s.y + 30.0, "scrolled content sits higher");
    // A row click (a file, no picture): the frame without an image. A menu open without a frame: no origin.
    let (_, o) = open_request(&p, "task", Some(s), None).unwrap();
    assert_eq!(o.as_ref().map(|o| o.image.is_none()), Some(true));
    assert_eq!(open_request(&p, "task", None, None).unwrap().1, None, "no frame: the viewer comes out of the notch");
    assert_eq!(open_request("https://example.com", "task", Some(s), None).unwrap().1, None, "a web page opens no viewer");
    // The origin as the viewer island's file carries it, read back.
    let o = o.unwrap();
    let island = Island::new("view-report.md", jobj! {"preset": "viewer", "origin": o.json()});
    assert_eq!(island.born_from_frame(), Some(o));
    assert!(!island.born_from_notch());
    assert_eq!(Island::new("v", jobj! {"origin": {"from": "notch"}}).born_from_frame(), None);
    assert_eq!(Island::new("v", jobj! {"origin": {"from": "frame", "x": 1, "y": 2, "w": 0, "h": 4}}).born_from_frame(), None, "an empty frame is no origin");
    let _ = std::fs::remove_dir_all(&d);
}

/// The viewer island file a click writes, and one `islands_view` writes (no frame: the notch, a stale origin gone).
#[test]
fn the_viewer_file_carries_the_origin_only_from_a_click() {
    let data = dir("data");
    std::env::set_var("FLOATING_ISLANDS_DIR", &data);
    let p = file(&data, "shot.png", PNG);
    let o = open_request(&p, "decider", Some(Rect::new(10.0, 20.0, 30.0, 40.0)), Some(&p)).unwrap().1.unwrap();
    let (id, written) = IslandFile::view_from(&p, None, None, vec![], Some(o.json()));
    assert_eq!(Island::new(&id, written).born_from_frame(), Some(o));
    let (_, again) = IslandFile::view(&p, None, None, vec![]);
    assert!(!again.contains_key("origin"), "opened again without a click: no stale origin");
    std::env::remove_var("FLOATING_ISLANDS_DIR");
    let _ = std::fs::remove_dir_all(&data);
}

/// Review, 30 Oct: the frame origin is used once. One still in a viewer's file at launch is dropped (no grow from a
/// stale frame); a viewer on screen opened again by a click takes the new island and its key goes at once.
#[test]
fn a_frame_origin_is_used_once() {
    let o = FrameOrigin { rect: Rect::new(1.0, 2.0, 30.0, 40.0), image: None, island: Some("decider".into()) };
    let v = Island::new("view-x", jobj! {"preset": "viewer", "origin": o.json()});
    assert_eq!(frame_on_arrival(&v, true), (Some(o.clone()), false), "a click just now: grow from it (the key goes after the grow)");
    assert_eq!(frame_on_arrival(&v, false), (None, true), "in the file at launch: stale, removed, no grow");
    assert_eq!(frame_on_arrival(&Island::new("v", jobj! {"preset": "viewer"}), false), (None, false));
    assert_eq!(frame_on_update(&v, true).and_then(|f| f.island), Some("decider".into()), "on screen already: the island it now belongs to");
    assert_eq!(frame_on_update(&v, false), None);
}
