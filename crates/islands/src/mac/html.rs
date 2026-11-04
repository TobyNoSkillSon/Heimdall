//! Model HTML, drawn once (`model/html.rs` is the gate in front of it). One shared WKWebView, in a window that is never
//! shown, draws every island's `html` part in turn to a bitmap; the island shows that bitmap as a layer's contents, so
//! an open island costs what a picture costs and nothing redraws at rest. A part is drawn again only when its HTML or
//! width changes (the bitmaps are cached by both). The web view, and with it WebKit's web content process, is
//! released `RELEASE_AFTER` after the last render, so at rest no web process runs.
//!
//! Locked down: page JavaScript is off (`allowsContentJavaScript = false`; the app's own height query still runs), a
//! non-persistent data store, the document's CSP allows nothing but inline styles and `file:` images, and the
//! navigation delegate allows only the document itself. A render that takes longer than `DEADLINE` fails, the web
//! view is torn down (taking a stuck page with it), and the island keeps its usual parts.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::rc::Rc;

use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject};
use objc2::{define_class, msg_send, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{NSBackingStoreType, NSImage, NSView, NSWindow, NSWindowStyleMask};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_foundation::{NSNumber, NSURL};

use super::draw::{Cf, Image, Ref};
use super::look::ns;
use super::sys;
use crate::model::html as gate;

/// The web view goes this long after its last render (its web content process exits with it, WebKit's GPU process
/// about a minute later). Longer than the island agent's two minutes between calls, so a busy island's next update
/// finds it warm (measured: ~25 ms of CPU warm, ~0.2 s cold); a held web view costs nothing at rest (`results.md`).
pub const RELEASE_AFTER: f64 = 150.0;

/// `RELEASE_AFTER`, or the lab's `FI_HTML_RELEASE` seconds.
pub fn release_after() -> f64 {
    std::env::var("FI_HTML_RELEASE").ok().and_then(|v| v.parse().ok()).filter(|v: &f64| *v >= 0.0).unwrap_or(RELEASE_AFTER)
}
/// One render's whole budget: load, measure, snapshot.
pub const DEADLINE: f64 = 3.0;
/// Bitmaps kept, newest last (a body's layer holds its own; an island draws at most `parts::HTML_PARTS`; 24 kept
/// stale versions: +19 MB).
const CACHE: usize = 16;

pub struct Drawn {
    pub image: Image,
    /// Points.
    pub width: f64,
    pub height: f64,
}

type Done = Box<dyn FnOnce(Option<Rc<Drawn>>)>;

struct Job {
    key: u64,
    doc: String,
    width: f64,
    done: Vec<Done>,
}

#[derive(Default)]
struct Renderer {
    cache: HashMap<u64, Rc<Drawn>>,
    order: VecDeque<u64>,
    /// Fragments that could not be drawn: why, and how often. Nothing drawn is final; anything else (a deadline on
    /// a busy Mac, a web process gone) is tried once more when asked again.
    failed: HashMap<u64, (String, u32)>,
    queue: VecDeque<Job>,
    busy: Option<Job>,
    generation: u64,
    web: Option<(Retained<NSWindow>, Retained<NSView>, Retained<HtmlWatcher>)>,
    release: Option<sys::Work1>,
    /// Lab counters.
    renders: u64,
    created: u64,
}

thread_local! {
    static R: RefCell<Renderer> = RefCell::new(Renderer::default());
    static LAST_MS: Cell<f64> = const { Cell::new(0.0) };
}

fn mtm() -> MainThreadMarker {
    MainThreadMarker::new().expect("html renders on the main thread")
}

fn class(name: &std::ffi::CStr) -> Option<&'static AnyClass> {
    AnyClass::get(name)
}

/// The bitmap for `fragment` at `width` points, if it is drawn already.
pub fn cached(fragment: &str, width: f64, scale: f64) -> Option<Rc<Drawn>> {
    let k = gate::key(fragment, width, scale);
    R.with(|r| r.borrow().cache.get(&k).cloned())
}

/// Whether drawing `fragment` failed for good (it is not tried again until it changes).
pub fn failed(fragment: &str, width: f64, scale: f64) -> Option<String> {
    let k = gate::key(fragment, width, scale);
    R.with(|r| r.borrow().failed.get(&k).filter(|f| final_failure(f)).map(|f| f.0.clone()))
}

fn final_failure(f: &(String, u32)) -> bool { f.0 == NOTHING || f.1 >= 2 }

const NOTHING: &str = "nothing drawn";

/// Lab: (renders done, web views made, whether one is alive now, the last render's milliseconds).
pub fn stats() -> (u64, u64, bool, f64) {
    R.with(|r| {
        let r = r.borrow();
        (r.renders, r.created, r.web.is_some(), LAST_MS.with(|m| m.get()))
    })
}

/// Draw `fragment` (already through `model::html::sanitize`) at `width` points; `done` gets the bitmap, or `None`
/// when it could not be drawn. Cached bitmaps answer at once; the same fragment asked twice is drawn once.
pub fn request(fragment: &str, width: f64, scale: f64, done: impl FnOnce(Option<Rc<Drawn>>) + 'static) {
    let k = gate::key(fragment, width, scale);
    let hit = R.with(|r| {
        let r = r.borrow();
        if r.failed.get(&k).is_some_and(final_failure) {
            return Some(None);
        }
        r.cache.get(&k).cloned().map(Some)
    });
    if let Some(h) = hit {
        done(h);
        return;
    }
    let start = R.with(|r| {
        let mut r = r.borrow_mut();
        if let Some(j) = r.busy.as_mut().filter(|j| j.key == k) {
            j.done.push(Box::new(done));
            return false;
        }
        if let Some(j) = r.queue.iter_mut().find(|j| j.key == k) {
            j.done.push(Box::new(done));
            return false;
        }
        r.queue.push_back(Job { key: k, doc: gate::document(fragment, width), width, done: vec![Box::new(done)] });
        r.busy.is_none()
    });
    if start {
        sys::soon(next);
    }
}

fn next() {
    let job = R.with(|r| {
        let mut r = r.borrow_mut();
        if r.busy.is_some() {
            return None;
        }
        let j = r.queue.pop_front()?;
        if let Some(w) = r.release.take() {
            w.cancel();
        }
        r.generation += 1;
        r.busy = Some(Job { key: j.key, doc: String::new(), width: j.width, done: j.done });
        Some((j.key, j.doc, j.width, r.generation))
    });
    let Some((key, doc, width, gen)) = job else { return };
    let started = sys::uptime();
    let web = ensure_web();
    // The deadline: a page that never finishes is dropped with its web view.
    sys::after(DEADLINE, move || {
        if R.with(|r| r.borrow().generation == gen && r.borrow().busy.is_some()) {
            finish(gen, None, "took longer than 3 s".into());
            teardown();
        }
    });
    unsafe {
        let _: () = msg_send![&*web, setFrame: CGRect::new(CGPoint::ZERO, CGSize::new(width, gate::MAX_HEIGHT))];
    }
    // The document and copies of the pictures it shows go into a folder of their own, and the web view may read only
    // that folder: whatever a fragment spells, it cannot reach another file (review, 3 Nov).
    let Some(path) = stage(key, &doc) else {
        finish(gen, None, "could not write the document".into());
        return;
    };
    let watcher = R.with(|r| r.borrow().web.as_ref().map(|w| w.2.clone()));
    if let Some(w) = &watcher {
        *w.ivars().loaded.borrow_mut() = Some(Box::new(move |ok: bool| {
            if !ok {
                finish(gen, None, "the page did not load".into());
                return;
            }
            measure(gen, width, started);
        }));
    }
    unsafe {
        let url = NSURL::fileURLWithPath(&ns(&path.to_string_lossy()));
        let dir = NSURL::fileURLWithPath(&ns(&path.parent().unwrap_or(&path).to_string_lossy()));
        let _: Option<Retained<AnyObject>> = msg_send![&*web, loadFileURL: &*url, allowingReadAccessToURL: &*dir];
    }
}

/// The renderer's scratch folder: `<data folder>/.html`, one `<key>/` folder per render, removed after it.
fn scratch() -> PathBuf { crate::model::Paths::root().join(".html") }

/// A picture larger than this is left out of the render (not copied).
const PICTURE_BYTES: u64 = 40 << 20;

/// Writes `<scratch>/<key>/doc.html` with each picture of the gate's own `<img>` tags copied beside it as `pN.ext`
/// and the tag pointing at the copy. Only an `<img` tag's ` src="file://` counts: text cannot spell a tag (a `<` in
/// text is escaped, `<style>` refuses one), and a kept attribute value cannot hold a quote, so nothing else in the
/// document can name a file to copy (re-review, 3 Nov). Returns the document's path.
fn stage(key: u64, doc: &str) -> Option<PathBuf> { stage_in(&scratch().join(format!("{key:016x}")), doc) }

fn stage_in(dir: &std::path::Path, doc: &str) -> Option<PathBuf> {
    let dir = dir.to_path_buf();
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).ok()?;
    let mut doc = doc.to_owned();
    let mut n = 0;
    let mut from = 0;
    while let Some(i) = doc[from..].find("<img") {
        let tag_start = from + i;
        let Some(tag_len) = doc[tag_start..].find('>') else { break };
        let tag_end = tag_start + tag_len;
        let Some(k) = doc[tag_start..tag_end].find(" src=\"file://") else {
            from = tag_end;
            continue;
        };
        let start = tag_start + k + 6;
        let Some(len) = doc[start..tag_end].find('"') else { break };
        let url = doc[start..start + len].to_owned();
        let src = PathBuf::from(gate::percent_decode(&url["file://".len()..]));
        let ext = src.extension().and_then(|e| e.to_str()).unwrap_or("png").to_ascii_lowercase();
        let copy = dir.join(format!("p{n}.{ext}"));
        n += 1;
        let ok = gate::IMAGE_EXT.contains(&ext.as_str())
            && std::fs::metadata(&src).is_ok_and(|m| m.is_file() && m.len() <= PICTURE_BYTES)
            && std::fs::copy(&src, &copy).is_ok();
        let to = if ok { gate::file_url(&copy.to_string_lossy()) } else { String::new() };
        doc.replace_range(start..start + len, &to);
        from = start + to.len();
    }
    let path = dir.join("doc.html");
    std::fs::write(&path, doc.as_bytes()).ok()?;
    Some(path)
}

/// The content's height (the app's own query: page scripts are off, this one runs), then the snapshot.
fn measure(gen: u64, width: f64, started: f64) {
    let Some(web) = R.with(|r| r.borrow().web.as_ref().map(|w| w.1.clone())) else { return };
    let block = block2::RcBlock::new(move |v: *mut AnyObject, _e: *mut AnyObject| {
        if R.with(|r| r.borrow().generation) != gen {
            return;
        }
        let h: f64 = if v.is_null() { 0.0 } else { unsafe { msg_send![&*v, doubleValue] } };
        if !(h > 0.5) {
            finish(gen, None, NOTHING.into());
            return;
        }
        let h = h.min(gate::MAX_HEIGHT).ceil();
        snapshot(gen, width, h, started);
    });
    let js = "Math.ceil(document.getElementById('i').getBoundingClientRect().height)";
    unsafe {
        let _: () = msg_send![&*web, evaluateJavaScript: &*ns(js), completionHandler: &*block];
    }
}

fn snapshot(gen: u64, width: f64, height: f64, started: f64) {
    let Some(web) = R.with(|r| r.borrow().web.as_ref().map(|w| w.1.clone())) else { return };
    let cfg: Retained<AnyObject> = unsafe { msg_send![class(c"WKSnapshotConfiguration").expect("WebKit"), new] };
    unsafe {
        let _: () = msg_send![&*cfg, setRect: CGRect::new(CGPoint::ZERO, CGSize::new(width, height))];
        let _: () = msg_send![&*cfg, setSnapshotWidth: &*NSNumber::new_f64(width)];
    }
    let block = block2::RcBlock::new(move |img: *mut NSImage, _e: *mut AnyObject| {
        if R.with(|r| r.borrow().generation) != gen {
            return;
        }
        let image = if img.is_null() {
            None
        } else {
            let img: &NSImage = unsafe { &*img };
            let cg: *mut objc2_core_graphics::CGImage =
                unsafe { msg_send![img, CGImageForProposedRect: std::ptr::null_mut::<CGRect>(), context: std::ptr::null::<AnyObject>(), hints: std::ptr::null::<AnyObject>()] };
            retain(cg)
        };
        LAST_MS.with(|m| m.set((sys::uptime() - started) * 1000.0));
        match image {
            Some(i) => finish(gen, Some(Rc::new(Drawn { image: i, width, height })), String::new()),
            None => finish(gen, None, "no snapshot".into()),
        }
    });
    unsafe {
        let _: () = msg_send![&*web, takeSnapshotWithConfiguration: &*cfg, completionHandler: &*block];
    }
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGImageRetain(img: Ref) -> Ref;
}

fn retain(cg: *mut objc2_core_graphics::CGImage) -> Option<Image> {
    if cg.is_null() {
        return None;
    }
    // SAFETY: a live image owned by its NSImage; we take our own reference.
    Cf::new(unsafe { CGImageRetain(cg as Ref) }).map(Image)
}

fn finish(gen: u64, drawn: Option<Rc<Drawn>>, why: String) {
    let job = R.with(|r| {
        let mut r = r.borrow_mut();
        if r.generation != gen {
            return None;
        }
        let j = r.busy.take()?;
        match &drawn {
            Some(d) => {
                r.renders += 1;
                r.cache.insert(j.key, d.clone());
                r.order.push_back(j.key);
                while r.order.len() > CACHE {
                    if let Some(old) = r.order.pop_front() {
                        r.cache.remove(&old);
                    }
                }
            }
            None => {
                let n = r.failed.get(&j.key).map_or(0, |f| f.1) + 1;
                r.failed.insert(j.key, (why.clone(), n));
            }
        }
        if r.failed.len() > 64 {
            r.failed.clear();
        }
        Some(j)
    });
    let Some(job) = job else { return };
    let _ = std::fs::remove_dir_all(scratch().join(format!("{:016x}", job.key)));
    if drawn.is_none() {
        super::island::debug(&format!("html: not drawn: {why}"));
    }
    for d in job.done {
        d(drawn.clone());
    }
    let more = R.with(|r| !r.borrow().queue.is_empty());
    if more {
        sys::soon(next);
    } else {
        let w = sys::after_cancellable(release_after(), teardown);
        R.with(|r| r.borrow_mut().release = Some(w));
    }
}

/// Release the web view (WebKit's web content process exits with the last one).
pub fn teardown() {
    let web = R.with(|r| {
        let mut r = r.borrow_mut();
        if let Some(w) = r.release.take() {
            w.cancel();
        }
        r.web.take()
    });
    if let Some((win, web, watcher)) = web {
        unsafe {
            let _: () = msg_send![&*web, stopLoading];
            let _: () = msg_send![&*web, setNavigationDelegate: std::ptr::null::<AnyObject>()];
        }
        *watcher.ivars().loaded.borrow_mut() = None;
        web.removeFromSuperview();
        win.close();
    }
    // A job caught mid-render by a teardown fails; the rest start on a new web view.
    let (stuck, more) = R.with(|r| {
        let r = r.borrow();
        (r.busy.as_ref().map(|_| r.generation), !r.queue.is_empty())
    });
    if let Some(g) = stuck {
        finish(g, None, "the web view went away".into());
    } else if more {
        sys::soon(next);
    }
}

fn ensure_web() -> Retained<NSView> {
    if let Some(w) = R.with(|r| r.borrow().web.as_ref().map(|w| w.1.clone())) {
        return w;
    }
    let mtm = mtm();
    // Render folders a crash left behind.
    let _ = std::fs::remove_dir_all(scratch());
    let cfg: Retained<AnyObject> = unsafe { msg_send![class(c"WKWebViewConfiguration").expect("WebKit"), new] };
    unsafe {
        let _: () = msg_send![&*cfg, setSuppressesIncrementalRendering: true];
        let _: () = msg_send![&*cfg, setMediaTypesRequiringUserActionForPlayback: usize::MAX];
        let prefs: Retained<AnyObject> = msg_send![&*cfg, defaultWebpagePreferences];
        let _: () = msg_send![&*prefs, setAllowsContentJavaScript: false];
        let store: Retained<AnyObject> = msg_send![class(c"WKWebsiteDataStore").expect("WebKit"), nonPersistentDataStore];
        let _: () = msg_send![&*cfg, setWebsiteDataStore: &*store];
    }
    let web: Retained<NSView> = unsafe { msg_send![msg_send![class(c"WKWebView").expect("WebKit"), alloc], initWithFrame: CGRect::new(CGPoint::ZERO, CGSize::new(328.0, 200.0)), configuration: &*cfg] };
    unsafe {
        let _: () = msg_send![&*web, setValue: &*NSNumber::new_bool(false), forKey: &*ns("drawsBackground")];
    }
    let watcher = HtmlWatcher::new();
    unsafe {
        let _: () = msg_send![&*web, setNavigationDelegate: &*watcher];
    }
    // A window that is never ordered in gives the view a backing scale and a layer tree to snapshot.
    let win = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            CGRect::new(CGPoint::new(-20000.0, -20000.0), CGSize::new(600.0, gate::MAX_HEIGHT)),
            NSWindowStyleMask::Borderless,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    unsafe { win.setReleasedWhenClosed(false) };
    if let Some(cv) = win.contentView() {
        cv.addSubview(&web);
    }
    R.with(|r| {
        let mut r = r.borrow_mut();
        r.created += 1;
        r.web = Some((win, web.clone(), watcher));
    });
    web
}

pub struct WatchIvars {
    loaded: RefCell<Option<Box<dyn Fn(bool)>>>,
}

define_class!(
    /// The renderer's navigation delegate: the document loads, nothing else does.
    #[unsafe(super(objc2_foundation::NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "FIHtmlPartWatcher"]
    #[ivars = WatchIvars]
    pub struct HtmlWatcher;

    impl HtmlWatcher {
        #[unsafe(method(webView:didFinishNavigation:))]
        fn did_finish(&self, _web: &AnyObject, _nav: Option<&AnyObject>) {
            if let Some(f) = self.ivars().loaded.borrow().as_ref() {
                f(true);
            }
        }

        #[unsafe(method(webView:didFailNavigation:withError:))]
        fn did_fail(&self, _web: &AnyObject, _nav: Option<&AnyObject>, _e: Option<&AnyObject>) {
            if let Some(f) = self.ivars().loaded.borrow().as_ref() {
                f(false);
            }
        }

        #[unsafe(method(webView:didFailProvisionalNavigation:withError:))]
        fn did_fail_provisional(&self, _web: &AnyObject, _nav: Option<&AnyObject>, _e: Option<&AnyObject>) {
            if let Some(f) = self.ivars().loaded.borrow().as_ref() {
                f(false);
            }
        }

        #[unsafe(method(webViewWebContentProcessDidTerminate:))]
        fn process_gone(&self, _web: &AnyObject) {
            if let Some(f) = self.ivars().loaded.borrow().as_ref() {
                f(false);
            }
        }

        /// Only the document itself (a `file:` URL under the renderer's folder) loads.
        #[unsafe(method(webView:decidePolicyForNavigationAction:decisionHandler:))]
        fn decide(&self, _web: &AnyObject, action: &AnyObject, decision: &block2::DynBlock<dyn Fn(isize)>) {
            let ok = unsafe {
                let req: Option<Retained<AnyObject>> = msg_send![action, request];
                let url: Option<Retained<NSURL>> = req.and_then(|r| msg_send![&*r, URL]);
                url.is_some_and(|u| u.isFileURL() && u.path().is_some_and(|p| doc_allowed(&p.to_string())))
            };
            decision.call((if ok { 1 } else { 0 },));
        }
    }
);

/// The renderer's own documents: `<data folder>/.html/<key>/doc.html`.
fn doc_allowed(path: &str) -> bool {
    let p = std::path::Path::new(path);
    p.file_name().is_some_and(|n| n == "doc.html") && p.parent().and_then(|d| d.parent()) == Some(scratch().as_path())
}

impl HtmlWatcher {
    fn new() -> Retained<HtmlWatcher> {
        let this = Self::alloc(mtm()).set_ivars(WatchIvars { loaded: RefCell::new(None) });
        let w: Retained<HtmlWatcher> = unsafe { msg_send![super(this), init] };
        unsafe {
            if let Some(p) = objc2::runtime::AnyProtocol::get(c"WKNavigationDelegate") {
                let cls = w.class() as *const AnyClass as *mut AnyClass;
                objc2::ffi::class_addProtocol(cls, p);
            }
        }
        w
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only the gate's own `<img>` pictures are copied into the render's folder; a path spelled in text is not.
    #[test]
    fn only_the_gates_pictures_are_copied_for_a_render() {
        let d = std::env::temp_dir().join(format!("fi-stage-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let (pic, secret) = (d.join("a b.png"), d.join("secret.png"));
        std::fs::write(&pic, b"png").unwrap();
        std::fs::write(&secret, b"secret").unwrap();
        let frag = format!("<p>src=\"{}\"</p><img src=\"{}\">", gate::file_url(&secret.to_string_lossy()), pic.display());
        let clean = gate::sanitize(&frag, &|p| p.is_file()).unwrap();
        let out = stage_in(&d.join("render"), &gate::document(&clean.html, 328.0)).unwrap();
        let mut copies: Vec<String> = std::fs::read_dir(d.join("render")).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        copies.sort();
        assert_eq!(copies, ["doc.html", "p0.png"]);
        assert_eq!(std::fs::read(d.join("render/p0.png")).unwrap(), b"png");
        assert!(std::fs::read_to_string(out).unwrap().contains("render/p0.png"));
        let _ = std::fs::remove_dir_all(&d);
    }
}
