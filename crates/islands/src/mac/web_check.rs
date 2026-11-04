//! Lab: remote pictures and video in the viewer, offscreen (bug note 2026-11-04T00-46-09: a report's 11 HTTPS videos
//! showed crossed out). No window is ordered in and nothing plays: the viewer's own web view (`FileViewer`, its content
//! rules and navigation guard) loads one page from a trusted folder and the same page from an untrusted one, and the
//! page's elements are read back through JavaScript.
//!   FLOATING_ISLANDS_DIR=<lab dir> FLOATING_ISLANDS_WEB_ROOTS=<out>/trusted \
//!     FloatingIslands --web-media-check <out> [video-url] [image-url]
//! Each page holds an HTTPS video (preload metadata, muted), an HTTPS picture, a video from a loopback plain-http
//! server and a `fetch` to the video's URL. Trusted: the video reaches its metadata and the picture draws; the
//! loopback server is never asked and the fetch is blocked. Untrusted: nothing remote loads. With no network the
//! HTTPS half is reported SKIPPED and the blocking half still runs. Lines go to `<out>/report.txt`; exit 0 on pass.

use std::cell::RefCell;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{msg_send, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{NSBackingStoreType, NSView, NSWindow, NSWindowStyleMask};
use objc2_foundation::NSString;

use super::look::{cgr, ns};
use super::sys;
use super::viewer::{web_roots, FileViewer, FlippedView, WEB_ROOTS_ENV};
use crate::model::geometry::{Rect, Size};

const VIDEO: &str = "https://csbhr.github.io/projects/vivid-vr/assets/videos-teaser/demo1_input.mp4";
const IMAGE: &str = "https://github.githubassets.com/favicons/favicon.png";
/// How long a page gets to settle.
const WAIT_S: f64 = 25.0;

type Lines = Rc<RefCell<Vec<String>>>;

fn log(lines: &Lines, s: String) {
    println!("{s}");
    lines.borrow_mut().push(s);
}

/// What the page's elements said (`STATE_JS`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Seen {
    /// The HTTPS video: readyState, networkState, error code (0: none), videoWidth.
    pub video: [i64; 4],
    /// The loopback plain-http video, the same four.
    pub loopback: [i64; 4],
    /// The HTTPS picture: whether it finished (loaded or failed), and its natural width (0: not drawn).
    pub image_done: bool,
    pub image_w: i64,
    /// The fetch's result: "ok <status>", "blocked" or "pending".
    pub fetch: String,
}

impl Seen {
    pub fn parse(json: &str) -> Option<Seen> {
        let v: serde_json::Value = serde_json::from_str(json).ok()?;
        let four = |k: &str| -> Option<[i64; 4]> {
            let a = v[k].as_array()?;
            Some([a.first()?.as_i64()?, a.get(1)?.as_i64()?, a.get(2)?.as_i64()?, a.get(3)?.as_i64()?])
        };
        let i = v["i"].as_array()?;
        Some(Seen { video: four("v")?, loopback: four("lo")?, image_done: i.first()?.as_bool()?, image_w: i.get(1)?.as_i64()?, fetch: v["f"].as_str()?.to_owned() })
    }

    /// The HTTPS video has its metadata (HAVE_METADATA or more) and a picture size.
    pub fn video_loaded(&self) -> bool {
        self.video[0] >= 1 && self.video[3] > 0
    }

    /// The HTTPS video failed or has no source (an error, or NETWORK_NO_SOURCE).
    pub fn video_blocked(&self) -> bool {
        self.video[0] == 0 && (self.video[2] != 0 || self.video[1] == 3)
    }

    /// The picture finished without drawing (blocked or failed).
    pub fn image_blocked(&self) -> bool {
        self.image_done && self.image_w == 0
    }

    /// Done waiting: the fetch has answered, the picture has finished, and the video failed or loaded.
    pub fn settled(&self) -> bool {
        self.fetch != "pending" && self.image_done && (self.video_blocked() || self.video_loaded())
    }
}

const STATE_JS: &str = "(()=>{const m=e=>e?[e.readyState,e.networkState,e.error?e.error.code:0,e.videoWidth||0]:[-1,-1,-1,-1];\
const i=document.getElementById('i');return JSON.stringify({v:m(document.getElementById('v')),lo:m(document.getElementById('lo')),\
i:i?[i.complete,i.naturalWidth]:[false,0],f:window.__fetch||'pending'})})()";

/// The page: `loopback` is the plain-http server's root (a video and a fetch go there and must never arrive). The HTTPS
/// fetch is `no-cors`, so only a block (not CORS) makes it fail.
fn page(video: &str, image: &str, loopback: &str) -> String {
    format!(
        "<!doctype html><html><body style=\"background:#000;color:#fff\">\
<video id=\"v\" muted preload=\"metadata\" src=\"{video}\"></video>\
<img id=\"i\" src=\"{image}\">\
<video id=\"lo\" muted preload=\"metadata\" src=\"{loopback}/clip.mp4\"></video>\
<script>fetch(\"{loopback}/fetch\").catch(()=>0);\
fetch(\"{video}\",{{method:\"HEAD\",mode:\"no-cors\"}}).then(r=>window.__fetch=\"ok \"+r.status,()=>window.__fetch=\"blocked\")</script>\
</body></html>"
    )
}

/// A plain-http server on loopback that counts every connection (none may arrive: plain http stays blocked).
fn loopback_server(hits: Arc<AtomicUsize>) -> Option<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").ok()?;
    let port = listener.local_addr().ok()?.port();
    std::thread::spawn(move || {
        for s in listener.incoming().flatten() {
            hits.fetch_add(1, Ordering::SeqCst);
            let mut s = s;
            let mut buf = [0u8; 2048];
            let _ = s.read(&mut buf);
            let _ = s.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        }
    });
    Some(port)
}

/// Whether `url` answers over the network now (a range request, 8 s at most).
fn reachable(url: &str) -> bool {
    std::process::Command::new("/usr/bin/curl")
        .args(["-s", "-L", "-o", "/dev/null", "-r", "0-1023", "--max-time", "8", "-w", "%{http_code}", url])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .is_some_and(|c| c == "200" || c == "206")
}

struct Shown {
    _window: Retained<NSWindow>,
    _host: Retained<FlippedView>,
    viewer: Rc<FileViewer>,
}

fn show(path: &Path) -> Shown {
    let mtm = MainThreadMarker::new().expect("main thread");
    // Never ordered in, far from every display.
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(NSWindow::alloc(mtm), cgr(Rect::new(-30000.0, -30000.0, 900.0, 900.0)), NSWindowStyleMask::Borderless, NSBackingStoreType::Buffered, false)
    };
    unsafe { window.setReleasedWhenClosed(false) };
    let host = FlippedView::new(cgr(Rect::new(0.0, 0.0, 900.0, 900.0)));
    window.setContentView(Some(&host));
    let viewer = FileViewer::new(&path.to_string_lossy(), vec![]);
    let body = Size::new(820.0, 800.0);
    let root = viewer.make_view(body);
    root.setFrame(cgr(Rect::new(0.0, 0.0, body.w, body.h)));
    host.addSubview(&root);
    Shown { _window: window, _host: host, viewer }
}

fn read_state(web: &NSView, then: Box<dyn FnOnce(Option<Seen>)>) {
    let then = RefCell::new(Some(then));
    let block = block2::RcBlock::new(move |v: *mut AnyObject, _e: *mut AnyObject| {
        let Some(then) = then.borrow_mut().take() else { return };
        let s = if v.is_null() {
            None
        } else {
            let s: &NSString = unsafe { &*(v as *const NSString) };
            Seen::parse(&s.to_string())
        };
        then(s);
    });
    unsafe {
        let _: () = msg_send![web, evaluateJavaScript: &*ns(STATE_JS), completionHandler: &*block];
    }
}

/// Polls the page every half second until it settles or `WAIT_S` passes; `done` gets the last state.
fn watch(shown: Rc<Shown>, started: std::time::Instant, done: Box<dyn FnOnce(Option<Seen>)>) {
    let Some(web) = shown.viewer.web_view() else { return done(None) };
    read_state(&web, Box::new(move |s| {
        let over = started.elapsed().as_secs_f64() > WAIT_S;
        if over || s.as_ref().is_some_and(Seen::settled) {
            shown.viewer.teardown();
            return done(s);
        }
        sys::after(0.5, move || watch(shown, started, done));
    }));
}

fn finish(out: &Path, lines: &Lines, failed: bool) -> ! {
    log(lines, format!("web-media-check: {}", if failed { "FAIL" } else { "PASS" }));
    let _ = std::fs::write(out.join("report.txt"), lines.borrow().join("\n") + "\n");
    std::process::exit(if failed { 1 } else { 0 })
}

pub fn run(out: &str, video: Option<&str>, image: Option<&str>) -> ! {
    let mtm = MainThreadMarker::new().expect("main thread");
    let app = objc2_app_kit::NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(objc2_app_kit::NSApplicationActivationPolicy::Prohibited);
    let out = PathBuf::from(out);
    let (video, image) = (video.unwrap_or(VIDEO).to_owned(), image.unwrap_or(IMAGE).to_owned());
    let lines: Lines = Rc::default();
    let trusted = out.join("trusted");
    let untrusted = out.join("untrusted");
    let _ = std::fs::create_dir_all(&trusted);
    let _ = std::fs::create_dir_all(&untrusted);
    let roots: Vec<PathBuf> = web_roots().iter().filter_map(|r| r.canonicalize().ok()).collect();
    let (t, u) = (trusted.canonicalize().unwrap_or(trusted.clone()), untrusted.canonicalize().unwrap_or(untrusted.clone()));
    if !roots.iter().any(|r| t.starts_with(r)) || roots.iter().any(|r| u.starts_with(r)) {
        log(&lines, format!("setup: FAIL ({WEB_ROOTS_ENV} must hold {} and not {})", t.display(), u.display()));
        finish(&out, &lines, true)
    }
    let hits = Arc::new(AtomicUsize::new(0));
    let Some(port) = loopback_server(hits.clone()) else {
        log(&lines, "setup: FAIL (no loopback port)".into());
        finish(&out, &lines, true)
    };
    let loopback = format!("http://127.0.0.1:{port}");
    let html = page(&video, &image, &loopback);
    let (tp, up) = (trusted.join("report.html"), untrusted.join("report.html"));
    let _ = std::fs::write(&tp, &html);
    let _ = std::fs::write(&up, &html);
    let online = reachable(&video);
    log(&lines, format!("network: {} ({video})", if online { "reachable" } else { "unreachable: the HTTPS half is SKIPPED" }));

    let started = std::time::Instant::now();
    let shown = Rc::new(show(&tp));
    let failed = Rc::new(std::cell::Cell::new(false));
    let check = {
        let (lines, failed) = (lines.clone(), failed.clone());
        Rc::new(move |name: &str, ok: bool, detail: String| {
            failed.set(failed.get() || !ok);
            log(&lines, format!("{} {name} {detail}", if ok { "PASS" } else { "FAIL" }));
        })
    };
    let (o, l, h) = (out.clone(), lines.clone(), hits.clone());
    watch(shown, started, Box::new(move |s| {
        let s = s.unwrap_or_default();
        log(&l, format!("trusted page after {:.1} s: {s:?}", started.elapsed().as_secs_f64()));
        if online {
            check("trusted: the HTTPS video reaches its metadata", s.video_loaded(), format!("{:?}", s.video));
            check("trusted: the HTTPS picture draws", s.image_w > 0, format!("done {} width {}", s.image_done, s.image_w));
        }
        check("trusted: a script's HTTPS fetch is blocked", s.fetch == "blocked", s.fetch.clone());
        check("trusted: the plain-http video never loads", s.loopback[0] == 0, format!("{:?}", s.loopback));
        let started = std::time::Instant::now();
        let shown = Rc::new(show(&up));
        watch(shown, started, Box::new(move |s| {
            let s = s.unwrap_or_default();
            log(&l, format!("untrusted page after {:.1} s: {s:?}", started.elapsed().as_secs_f64()));
            check("untrusted: the HTTPS video is blocked", s.video_blocked(), format!("{:?}", s.video));
            check("untrusted: the HTTPS picture is blocked", s.image_blocked(), format!("done {} width {}", s.image_done, s.image_w));
            check("untrusted: a script's HTTPS fetch is blocked", s.fetch == "blocked", s.fetch.clone());
            let n = h.load(Ordering::SeqCst);
            check("both: the loopback server got no request (plain-http video and fetch)", n == 0, format!("{n} requests"));
            finish(&o, &l, failed.get())
        }));
    }));
    app.run();
    std::process::exit(1)
}

#[cfg(test)]
mod tests {
    use super::Seen;

    #[test]
    fn page_state_reads_loaded_blocked_and_pending() {
        let loaded = Seen::parse(r#"{"v":[4,1,0,640],"lo":[0,3,4,0],"i":[true,32],"f":"blocked"}"#).unwrap();
        assert!(loaded.video_loaded() && !loaded.video_blocked() && loaded.settled() && !loaded.image_blocked());
        let no_picture_yet = Seen { image_done: false, image_w: 0, ..loaded.clone() };
        assert!(!no_picture_yet.settled() && !no_picture_yet.image_blocked(), "a picture still loading is not blocked yet");
        let blocked = Seen::parse(r#"{"v":[0,3,4,0],"lo":[0,3,4,0],"i":[true,0],"f":"blocked"}"#).unwrap();
        assert!(blocked.video_blocked() && !blocked.video_loaded() && blocked.settled() && blocked.image_blocked());
        let waiting = Seen::parse(r#"{"v":[0,2,0,0],"lo":[0,2,0,0],"i":[false,0],"f":"pending"}"#).unwrap();
        assert!(!waiting.settled());
        assert_eq!(Seen::parse("not json"), None);
    }
}
