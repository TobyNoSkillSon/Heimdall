//! The viewer's drag handle hands out (`model/share.rs`): an HTML report with local files as one self-contained copy,
//! anything else as it is. The offscreen render of the copy against the original is `islands/lab/share_check.sh`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use islands::model::share::{base64, shareable, sweep, Shared};

struct Dir(PathBuf);
impl Dir {
    fn new(tag: &str) -> Dir {
        let d = std::env::temp_dir().join(format!("share-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        Dir(d.canonicalize().unwrap())
    }
    fn file(&self, name: &str, data: &[u8]) -> PathBuf {
        let u = self.0.join(name);
        std::fs::create_dir_all(u.parent().unwrap()).unwrap();
        std::fs::write(&u, data).unwrap();
        u
    }
}
impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn data_uri(mime: &str, bytes: &[u8]) -> String {
    format!("data:{mime};base64,{}", base64(bytes))
}

const PNG1: &[u8] = b"\x89PNG\r\n\x1a\nshot-one";
const PNG2: &[u8] = b"\x89PNG\r\n\x1a\nshot two";
const TILE: &[u8] = b"\x89PNG\r\n\x1a\ntile";
const FONT: &[u8] = b"wOF2font";

fn report(d: &Dir) -> PathBuf {
    d.file("report-src/shot-1.png", PNG1);
    d.file("report-src/shot two.png", PNG2);
    d.file("report-src/tile.png", TILE);
    d.file("report-src/fonts/inter.woff2", FONT);
    d.file("report-src/report.css", b"@font-face{src:url('fonts/inter.woff2')}\n.s{background:url(tile.png)}");
    d.file("report-src/fill.js", b"document.title='x';");
    d.file("secret.txt", b"outside");
    d.file(
        "pages/report.html",
        concat!(
            "<!doctype html><html><head><link rel=\"stylesheet\" href=\"../report-src/report.css\">",
            "<link rel=\"canonical\" href=\"../report-src/shot-1.png\">",
            "<style>.b{background:url(\"../report-src/tile.png\")}</style></head><body>",
            "<!-- <img src=\"../report-src/shot-1.png\"> -->",
            "<img src=\"local/inside.png\">",
            "<img src='../report-src/shot-1.png?v=2#frag' alt=\"a\">",
            "<img srcset=\"../report-src/shot%20two.png 1x, ../report-src/shot-1.png 2x\" src=../report-src/shot-1.png>",
            "<div style=\"background-image:url('../report-src/tile.png')\"></div>",
            "<img src=\"https://example.com/remote.png\"><img src=\"data:image/png;base64,AAAA\">",
            "<img src=\"missing.png\"><a href=\"other.html\">older</a>",
            "<script src=\"../report-src/fill.js\"></script><script>const u='../report-src/tile.png';</script>",
            "</body></html>"
        )
        .as_bytes(),
    );
    d.file("pages/local/inside.png", PNG2);
    d.0.join("pages/report.html")
}

/// The page's folder is `pages/`: what lies beside it in `report-src/` is outside it, so only `local/` is inlined. A
/// report in its usual shape (`report-src/` beside the page) is the next test.
#[test]
fn only_the_pages_own_folder_is_inlined() {
    let d = Dir::new("scope");
    let page = report(&d);
    let shares = d.0.join("shares");
    let s = shareable(&page, &shares).unwrap();
    let Shared::Bundled { path, inlined } = s else { panic!("bundled: {s:?}") };
    assert_eq!(inlined, 1);
    let html = std::fs::read_to_string(&path).unwrap();
    assert!(html.contains(&data_uri("image/png", PNG2)));
    assert!(html.contains("../report-src/shot-1.png"), "outside the page's folder stays a reference");
    assert!(!html.contains(&base64(b"outside")));
}

#[test]
fn a_report_with_local_files_becomes_one_self_contained_file() {
    let d = Dir::new("report");
    let src = report(&d);
    // The usual shape: the page beside its report-src/ folder.
    let html = std::fs::read_to_string(&src).unwrap().replace("../report-src/", "report-src/").replace("local/inside.png", "../secret.txt");
    let page = d.file("report.html", html.as_bytes());
    let shares = d.0.join("shares");
    let s = shareable(&page, &shares).unwrap();
    let Shared::Bundled { path, inlined } = s else { panic!("bundled: {s:?}") };
    // Same name, alone in a fresh folder under the shares folder; the original untouched.
    assert_eq!(path.file_name().unwrap(), "report.html");
    assert!(path.starts_with(&shares));
    assert_eq!(std::fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
    assert_eq!(std::fs::read_to_string(&page).unwrap(), html);
    let out = std::fs::read_to_string(&path).unwrap();
    // Every load of a file beside it is inlined: stylesheet (with its font and image), images (query and fragment
    // dropped, %20 decoded), srcset, style blocks and attributes, the script.
    for left in ["report-src/shot-1.png'", "report-src/shot-1.png?", "report-src/report.css", "report-src/fill.js", "shot%20two", "url(\"report-src", "url('report-src"] {
        assert!(!out.contains(left), "{left} left in {out}");
    }
    assert!(out.contains(&format!("src=\"{}\"", data_uri("image/png", PNG1))));
    assert!(out.contains(&format!("srcset=\"{} 1x, {} 2x\"", data_uri("image/png", PNG2), data_uri("image/png", PNG1))));
    assert!(out.contains(&format!("url({})", data_uri("image/png", TILE))));
    assert!(out.contains(&format!("src=\"{}\"", data_uri("text/javascript;charset=utf-8", b"document.title='x';"))));
    let css = "@font-face{src:url(".to_owned() + &data_uri("font/woff2", FONT) + ")}\n.s{background:url(" + &data_uri("image/png", TILE) + ")}";
    assert!(out.contains(&format!("href=\"{}\"", data_uri("text/css;charset=utf-8", css.as_bytes()))), "{out}");
    // Left alone: a non-loading link, comments, remote and data URIs, a missing file, links between pages, script
    // text, and anything outside the page's folder.
    assert!(out.contains("rel=\"canonical\" href=\"report-src/shot-1.png\""));
    assert!(out.contains("<!-- <img src=\"report-src/shot-1.png\"> -->"));
    assert!(out.contains("https://example.com/remote.png") && out.contains("data:image/png;base64,AAAA"));
    assert!(out.contains("src=\"missing.png\"") && out.contains("href=\"other.html\""));
    assert!(out.contains("const u='report-src/tile.png';"));
    assert!(out.contains("src=\"../secret.txt\"") && !out.contains(&base64(b"outside")));
    assert_eq!(inlined, 10);
}

#[test]
fn other_files_go_as_they_are() {
    let d = Dir::new("plain");
    let shares = d.0.join("shares");
    for (name, data) in [("dusk.png", PNG1 as &[u8]), ("notes.md", b"# Notes\n![x](report-src/a.png)"), ("report.pdf", b"%PDF-1.4")] {
        let p = d.file(name, data);
        assert_eq!(shareable(&p, &shares).unwrap(), Shared::AsIs(p.clone()), "{name}");
    }
    // A page that loads nothing from beside it is already whole.
    let p = d.file("plain.html", b"<html><body><img src=\"https://x.dev/a.png\"><p>Hi</p></body></html>");
    assert_eq!(shareable(&p, &shares).unwrap(), Shared::AsIs(p.clone()));
    assert!(!shares.exists(), "nothing made for files that go as they are");
    assert!(shareable(&d.0.join("gone.html"), &shares).is_err());
}

#[test]
fn a_symlink_out_of_the_folder_is_not_followed() {
    let d = Dir::new("link");
    let outside = Dir::new("link-outside");
    let secret = outside.file("key.txt", b"secret key");
    std::os::unix::fs::symlink(&secret, d.0.join("key.png")).unwrap();
    d.file("shot.png", PNG1);
    let page = d.file("p.html", b"<img src=\"key.png\"><img src=\"shot.png\">");
    let Shared::Bundled { path, inlined } = shareable(&page, &d.0.join("shares")).unwrap() else { panic!() };
    assert_eq!(inlined, 1);
    let out = std::fs::read_to_string(path).unwrap();
    assert!(out.contains("src=\"key.png\"") && !out.contains(&base64(b"secret key")));
}

#[test]
fn two_shares_of_one_page_never_collide_and_old_ones_are_swept() {
    let d = Dir::new("sweep");
    d.file("a.png", PNG1);
    let page = d.file("r.html", b"<img src=a.png>");
    let shares = d.0.join("shares");
    let a = shareable(&page, &shares).unwrap();
    let b = shareable(&page, &shares).unwrap();
    assert_ne!(a.path(), b.path());
    assert!(a.path().exists() && b.path().exists());
    assert_eq!(sweep(&shares, Duration::from_secs(3600)), 0, "young copies stay");
    assert_eq!(sweep(&shares, Duration::ZERO), 2);
    assert!(!a.path().exists() && !b.path().exists());
    assert_eq!(sweep(Path::new("/nonexistent/shares"), Duration::ZERO), 0);
}

/// Review (fresh Opus, 2 Nov): a non-ASCII file name or CSS text crashed the inliner (a slice inside a letter).
#[test]
fn non_ascii_names_and_css_are_safe() {
    let d = Dir::new("utf8");
    d.file("zrzut-ąę.png", PNG1);
    d.file("résumé.png", PNG2);
    let page = d.file(
        "r.html",
        "<html><head><title>Raport — ąę “x”</title><style>.a::before{content:\"ab€ url(zrzut-ąę.png)\"}.b{background:url(zrzut-ąę.png)}/* é */</style></head>\
         <body><p>Zażółć</p><img src=\"résumé.png\" alt=\"é\"><img src=\"zrzut-%C4%85%C4%99.png\"><textarea><img src=\"zrzut-ąę.png\"></textarea></body></html>"
            .as_bytes(),
    );
    let Shared::Bundled { path, inlined } = shareable(&page, &d.0.join("shares")).unwrap() else { panic!() };
    let out = std::fs::read_to_string(path).unwrap();
    assert_eq!(inlined, 3, "{out}");
    assert!(out.contains("content:\"ab€ url(zrzut-ąę.png)\""), "a string's text is not a load");
    assert!(out.contains(&format!(".b{{background:url({})}}", data_uri("image/png", PNG1))));
    assert!(out.contains(&format!("src=\"{}\"", data_uri("image/png", PNG2))));
    assert!(out.contains("<textarea><img src=\"zrzut-ąę.png\"></textarea>"), "a text area's text is not markup");
    assert!(out.contains("<title>Raport — ąę “x”</title>") && out.contains("Zażółć"));
}

/// Review (fresh Opus, 2 Nov): a page could carry hidden or unrelated files beside it out through hidden elements.
#[test]
fn a_page_cannot_smuggle_files_out() {
    let d = Dir::new("smuggle");
    d.file(".ssh/id_ed25519", b"PRIVATE KEY");
    d.file(".env", b"TOKEN=1");
    d.file(".hidden.png", PNG1);
    d.file("taxes.pdf", b"%PDF tax");
    d.file("notes.txt", b"private notes");
    d.file("key", b"bare key");
    d.file("ok.png", PNG2);
    let page = d.file(
        "r.html",
        b"<link rel=preload href=\".ssh/id_ed25519\"><script src=\".env\"></script><img src=\".hidden.png\">\
          <img src=\"taxes.pdf\" hidden><script src=\"notes.txt\"></script><link rel=stylesheet href=\"key\">\
          <div style=\"background:url(notes.txt)\"></div><img src=\"ok.png\">",
    );
    let Shared::Bundled { path, inlined } = shareable(&page, &d.0.join("shares")).unwrap() else { panic!() };
    let out = std::fs::read_to_string(path).unwrap();
    assert_eq!(inlined, 1, "{out}");
    for secret in [&b"PRIVATE KEY"[..], b"TOKEN=1", PNG1, b"%PDF tax", b"private notes", b"bare key"] {
        assert!(!out.contains(&base64(secret)), "{}", String::from_utf8_lossy(secret));
    }
    // An embedded PDF (what the page visibly shows) still goes.
    let page = d.file("doc.html", b"<embed src=\"taxes.pdf\">");
    assert!(matches!(shareable(&page, &d.0.join("shares")).unwrap(), Shared::Bundled { inlined: 1, .. }));
}

#[test]
fn a_page_in_the_home_folder_or_a_relative_path_is_safe() {
    // A relative path resolves against the working folder, never the root.
    let d = Dir::new("rel");
    d.file("a.png", PNG1);
    d.file("r.html", b"<img src=\"a.png\"><img src=\"/etc/hosts\">");
    let prev = std::env::current_dir().unwrap();
    std::env::set_current_dir(&d.0).unwrap();
    let got = shareable(Path::new("r.html"), &d.0.join("shares"));
    std::env::set_current_dir(prev).unwrap();
    let Shared::Bundled { path, inlined } = got.unwrap() else { panic!() };
    assert_eq!(inlined, 1);
    assert!(std::fs::read_to_string(path).unwrap().contains("src=\"/etc/hosts\""));
}

#[test]
fn entity_encoded_style_base_href_and_data_srcset() {
    let d = Dir::new("edge");
    d.file("sub/a.png", PNG1);
    d.file("b.png", PNG2);
    let page = d.file(
        "r.html",
        b"<div style=\"background:url(&quot;b.png&quot;)\"></div>\
          <img srcset=\"data:image/png;base64,AA,BB 1x, b.png 2x\">\
          <base href=\"sub/index.html\"><img src=\"a.png\">",
    );
    let Shared::Bundled { path, inlined } = shareable(&page, &d.0.join("shares")).unwrap() else { panic!() };
    let out = std::fs::read_to_string(path).unwrap();
    assert_eq!(inlined, 2, "{out}");
    assert!(out.contains(&format!("style=\"background:url({})\"", data_uri("image/png", PNG2))), "{out}");
    assert!(out.contains("srcset=\"data:image/png;base64,AA,BB 1x, b.png 2x\""), "a set with a data: candidate stays");
    assert!(out.contains(&format!("src=\"{}\"", data_uri("image/png", PNG1))), "a base naming a file resolves in its folder");
}

#[test]
fn a_copy_knows_what_it_was_made_from() {
    use islands::model::share::{fingerprint, shareable_with_deps};
    let d = Dir::new("deps");
    let a = d.file("a.png", PNG1);
    let page = d.file("r.html", b"<img src=\"a.png\">");
    let (_, deps) = shareable_with_deps(&page, &d.0.join("shares")).unwrap();
    assert_eq!(deps, vec![page.clone(), a.clone()]);
    let before = fingerprint(&deps);
    std::thread::sleep(Duration::from_millis(20));
    std::fs::write(&a, b"\x89PNG changed, longer").unwrap();
    assert_ne!(fingerprint(&deps), before, "a changed image makes the copy stale");
}
