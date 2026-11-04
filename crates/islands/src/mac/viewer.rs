//! The viewer island's content (Swift `Viewer.swift`): any file, rendered by type (`ViewerKind`): markdown rendered,
//! text and code monospaced with light colouring, images, PDF (every page, scrolling), audio (a player with a
//! waveform), video (AVPlayerView), HTML (WebKit), anything else through QuickLook; proposed-action buttons on top.
//! The size is known at once from cheap header reads; heavy loads (big text, PDF pages, waveforms) run off the main
//! thread.
//!
//! Viewer refresh (night 23 Oct, as Swift 9c9332a): a viewer remembers the file it loaded (`Stamp`: inode, size,
//! modification time in ns) and is stale when the file no longer matches. The watch is not here: the controller
//! watches the open viewer's file (`mac::filewatch`, Swift's `FileWatch`) and on a change, or on a new view of the same path, rebuilds a stale viewer: it reads
//! `scroll_offset`, tears the old one down, makes a new one with `set_restore_offset`, and reopens it. The restore is
//! applied once the content has a height (after layout, after async text, after the PDF document arrives), clamped
//! to the new length.
//!
//! WebKit, PDFKit, AVKit and QuickLookUI have no objc2 crates here: they are called through `msg_send!` on their
//! runtime classes and linked below.
#![allow(unused_unsafe)]

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};

use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject};
use objc2::{define_class, msg_send, AllocAnyThread, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSAppearance, NSAppearanceCustomization, NSBezierPath, NSBitmapImageRep, NSEvent, NSGraphicsContext, NSImage, NSImageView, NSLineBreakMode,
    NSScrollView, NSTextField, NSTextView, NSView,
};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_core_graphics::CGImage;
use objc2_foundation::{NSAttributedString, NSDictionary, NSNumber, NSString, NSURL};
use objc2_quartz_core::{CALayer, CATextLayer};

use super::confirm;
use super::draw::{self, Cf, Image, Ref};
use super::look::{self, any, cgr, ns};
use super::sys::{self, Work1};
use super::viewer_audio::{self, AudioPlayerView};
use super::viewer_text;
use crate::model::geometry::{Point, Rect, Size};
use crate::model::{ViewerAction, ViewerKind};

#[link(name = "WebKit", kind = "framework")]
extern "C" {}
#[link(name = "PDFKit", kind = "framework")]
extern "C" {}
#[link(name = "AVKit", kind = "framework")]
extern "C" {}
#[link(name = "Quartz", kind = "framework")]
extern "C" {}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGPDFDocumentCreateWithURL(url: Ref) -> Ref;
    fn CGPDFDocumentGetNumberOfPages(doc: Ref) -> usize;
    fn CGPDFDocumentGetPage(doc: Ref, n: usize) -> Ref;
    fn CGPDFPageGetBoxRect(page: Ref, b: i32) -> CGRect;
    fn CGPDFPageGetRotationAngle(page: Ref) -> i32;
    fn CGContextDrawPDFPage(c: Ref, page: Ref);
    fn CGContextSaveGState(c: Ref);
    fn CGContextRestoreGState(c: Ref);
    fn CGContextClipToRect(c: Ref, r: CGRect);
    fn CGContextSetFillColorWithColor(c: Ref, color: Ref);
    fn CGContextFillRect(c: Ref, r: CGRect);
    fn CGContextTranslateCTM(c: Ref, tx: f64, ty: f64);
    fn CGContextScaleCTM(c: Ref, sx: f64, sy: f64);
    fn CGContextDrawImage(c: Ref, r: CGRect, img: Ref);
    fn CGImageRetain(img: Ref) -> Ref;
}

const CROP_BOX: i32 = 1;
pub const PDF_INSET: f64 = 6.0;
pub const PDF_GAP: f64 = 10.0;
pub const ACTION_H: f64 = 34.0;
pub const ACTION_GAP: f64 = 8.0;

/// A value carried to another thread and back to the main thread without being touched there (Rc handles, views,
/// results made on a worker). It opens only with a `MainThreadMarker`; dropped off the main thread (a worker that
/// panicked before handing it back) it is leaked rather than released there.
pub struct MainOnly<T>(std::mem::ManuallyDrop<T>);
// SAFETY: the value is reachable only through `into_inner`, which needs the main thread, and is never dropped
// elsewhere (see `Drop`).
unsafe impl<T> Send for MainOnly<T> {}

impl<T> MainOnly<T> {
    /// # Safety
    /// The caller moves the value to another thread only to bring it back to the main thread unopened.
    pub unsafe fn new(v: T) -> MainOnly<T> {
        MainOnly(std::mem::ManuallyDrop::new(v))
    }

    pub fn into_inner(self, _mtm: MainThreadMarker) -> T {
        let mut me = std::mem::ManuallyDrop::new(self);
        // SAFETY: taken once; `me` is never dropped, so the value is not dropped twice.
        unsafe { std::mem::ManuallyDrop::take(&mut me.0) }
    }
}

impl<T> Drop for MainOnly<T> {
    fn drop(&mut self) {
        if MainThreadMarker::new().is_some() {
            // SAFETY: dropped once, on the main thread.
            unsafe { std::mem::ManuallyDrop::drop(&mut self.0) }
        }
    }
}

fn mtm() -> MainThreadMarker {
    MainThreadMarker::new().expect("the viewer runs on the main thread")
}

fn class(name: &std::ffi::CStr) -> Option<&'static AnyClass> {
    AnyClass::get(name)
}

fn file_url(p: &Path) -> Retained<NSURL> {
    NSURL::fileURLWithPath(&ns(&p.to_string_lossy()))
}

/// The viewer's file: `file://` URLs and `~` accepted, `.` and `..` resolved (`ViewerKind.url(for:)`).
pub fn viewer_path(raw: &str) -> PathBuf {
    let u: Option<Retained<NSURL>> = if raw.starts_with("file://") {
        NSURL::URLWithString(&ns(raw))
    } else {
        let expanded = ns(raw).stringByExpandingTildeInPath();
        Some(NSURL::fileURLWithPath(&expanded))
    };
    // `URL.standardizedFileURL`: `URLByStandardizingPath` for file URLs (also drops a leading /private).
    let p = u.and_then(|u| if u.isFileURL() { u.URLByStandardizingPath() } else { u.standardizedURL() }).and_then(|u| u.path());
    p.map(|p| PathBuf::from(p.to_string())).unwrap_or_else(|| PathBuf::from(raw))
}

/// A file's identity and version as `stat` sees it (Swift `FileStamp`): a rename-replace changes the inode, an
/// in-place write the size or modification time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stamp {
    pub ino: u64,
    pub size: i64,
    pub mtime_ns: i64,
}

pub fn stamp(p: &Path) -> Option<Stamp> {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::metadata(p).ok()?;
    Some(Stamp { ino: m.ino(), size: m.size() as i64, mtime_ns: m.mtime() * 1_000_000_000 + m.mtime_nsec() })
}

// MARK: Views

define_class!(
    /// The viewer's root: y runs down.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "FIViewerRoot"]
    pub struct ViewerRootView;

    impl ViewerRootView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }
    }
);

define_class!(
    /// A plain flipped view (the "Not found" message, the renders' host).
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "FIViewerFlipped"]
    pub struct FlippedView;

    impl FlippedView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }
    }
);

impl ViewerRootView {
    pub fn new(frame: CGRect) -> Retained<ViewerRootView> {
        unsafe { msg_send![Self::alloc(mtm()), initWithFrame: frame] }
    }
}

impl FlippedView {
    pub fn new(frame: CGRect) -> Retained<FlippedView> {
        unsafe { msg_send![Self::alloc(mtm()), initWithFrame: frame] }
    }
}

/// The viewer's web content rules: every load over the network is blocked (WebKit's filters take no alternation,
/// so one rule per scheme; `^[a-z]+://` would block files too).
pub const OFFLINE_RULES: &str = r#"[{"trigger":{"url-filter":"^https?://"},"action":{"type":"block"}},{"trigger":{"url-filter":"^wss?://"},"action":{"type":"block"}},{"trigger":{"url-filter":"^ftp://"},"action":{"type":"block"}}]"#;

/// Report folders whose pages may show remote pictures and video, besides the islands folder's docs: Heimdall passes
/// its report folders (`supervisor.rs`), `:`-separated absolute paths.
pub const WEB_ROOTS_ENV: &str = "FLOATING_ISLANDS_WEB_ROOTS";

/// The pages that may show HTTPS pictures and video: the islands folder's docs (pulled-out replies) and the report
/// folders in `WEB_ROOTS_ENV` (bug note 2026-11-04T00-46-09: a video report showed every video crossed out). A folder
/// that would trust too much is never taken (`trusted_root`).
pub fn web_roots() -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let mut roots = vec![crate::model::Paths::root().join("docs")];
    if let Ok(v) = std::env::var(WEB_ROOTS_ENV) {
        roots.extend(v.split(':').map(PathBuf::from));
    }
    roots.retain(|r| trusted_root(r, &home));
    roots
}

/// Whether `root` may be trusted with remote media: an absolute folder that is not `/`, not the home folder or a folder
/// above it, and not a place downloads or scratch files land (Downloads, temporary folders, a `Temp` folder), so a
/// downloaded page never gets a way to send what it reads out (review, 30 Oct).
pub fn trusted_root(root: &Path, home: &Path) -> bool {
    if !root.is_absolute() || root.parent().is_none() {
        return false;
    }
    if home.as_os_str().len() > 1 && home.starts_with(root) {
        return false;
    }
    let scratch = ["/tmp", "/private/tmp", "/private/var", "/var/folders"];
    if scratch.iter().any(|s| root.starts_with(s)) || root.starts_with(home.join("Downloads")) {
        return false;
    }
    !root.components().any(|c| matches!(c.as_os_str().to_str(), Some("Temp" | "tmp" | "Downloads")))
}

/// The viewer's content rules for pages under `trusted` (bug note 2026-11-04T00-46-09). Everything in
/// `OFFLINE_RULES` stays blocked for every page: scripts, fetch and XHR, sockets, frames, pings and plain http. A page
/// under a trusted folder may also load HTTPS pictures and video (`image`, `media`), as a report written there for
/// the user embeds them; a page anywhere else (Downloads, a temporary folder) stays wholly offline, so a downloaded page
/// still cannot read its folder and send it out (review, 30 Oct). A symlinked root is matched both ways.
pub fn web_rules(trusted: &[PathBuf]) -> String {
    let mut rules: Vec<serde_json::Value> = serde_json::from_str(OFFLINE_RULES).expect("offline rules");
    let mut tops: Vec<String> = Vec::new();
    for root in trusted {
        let canonical = root.canonicalize().ok();
        for r in std::iter::once(root.clone()).chain(canonical) {
            for p in top_url_patterns(&r) {
                if !tops.contains(&p) {
                    tops.push(p);
                }
            }
        }
    }
    if !tops.is_empty() {
        rules.push(serde_json::json!({
            "trigger": {"url-filter": "^https://", "resource-type": ["image", "media"], "if-top-url": tops},
            "action": {"type": "ignore-previous-rules"},
        }));
    }
    serde_json::Value::Array(rules).to_string()
}

/// Content-rule patterns for every page under `root`: its `file://` URL as Foundation writes it (what the viewer
/// loads, `file_url`) and fully percent-encoded (WebKit may encode more), regex characters escaped, ending in `/` so a
/// sibling with a longer name never matches.
pub fn top_url_patterns(root: &Path) -> Vec<String> {
    let mut path = root.to_string_lossy().trim_end_matches('/').to_owned();
    path.push('/');
    let mut strict = String::from("file://");
    for b in path.bytes() {
        match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'/' | b'-' | b'_' | b'~' | b'.' => strict.push(b as char),
            _ => strict.push_str(&format!("%{b:02X}")),
        }
    }
    let mut out = vec![];
    let foundation = file_url(Path::new(&path)).absoluteString().map(|s| s.to_string());
    for url in foundation.into_iter().chain([strict]) {
        let url = if url.ends_with('/') { url } else { url + "/" };
        let mut p = String::from("^");
        for c in url.chars() {
            if ".*+?^$()[]{}|\\".contains(c) {
                p.push('\\');
            }
            p.push(c);
        }
        if !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

/// A navigation the viewer's page may make: a local file or the blank page, nothing else.
pub fn web_navigation_allowed(url: &str) -> bool {
    url.starts_with("file://") || url == "about:blank" || url.starts_with("about:srcdoc")
}

pub struct WebIvars {
    loaded: RefCell<Option<Box<dyn Fn()>>>,
}

define_class!(
    /// Tells the viewer when a page has loaded (to measure its height).
    #[unsafe(super(objc2_foundation::NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "FIViewerWebLoadWatcher"]
    #[ivars = WebIvars]
    pub struct WebLoadWatcher;

    impl WebLoadWatcher {
        #[unsafe(method(webView:didFinishNavigation:))]
        fn did_finish(&self, _web: &AnyObject, _nav: Option<&AnyObject>) {
            if let Some(f) = self.ivars().loaded.borrow().as_ref() {
                f();
            }
        }

        /// The page stays a local file (review, 30 Oct: a downloaded page could read its folder and send it out): a
        /// navigation to anything but a file is cancelled; a web link the user clicks opens in his browser instead.
        #[unsafe(method(webView:decidePolicyForNavigationAction:decisionHandler:))]
        fn decide(&self, _web: &AnyObject, action: &AnyObject, decision: &block2::DynBlock<dyn Fn(isize)>) {
            let (url, clicked) = unsafe {
                let req: Option<Retained<AnyObject>> = msg_send![action, request];
                let url: Option<Retained<objc2_foundation::NSURL>> = req.and_then(|r| msg_send![&*r, URL]);
                let kind: isize = msg_send![action, navigationType];
                (url, kind == 0)
            };
            let abs = url.as_ref().and_then(|u| u.absoluteString()).map(|s| s.to_string()).unwrap_or_default();
            let allow = web_navigation_allowed(&abs);
            if !allow && clicked && (abs.starts_with("https://") || abs.starts_with("http://")) {
                if let Some(u) = &url { objc2_app_kit::NSWorkspace::sharedWorkspace().openURL(u); }
            }
            decision.call((if allow { 1 } else { 0 },));
        }
    }
);

impl WebLoadWatcher {
    fn new(f: Box<dyn Fn()>) -> Retained<WebLoadWatcher> {
        let this = Self::alloc(mtm()).set_ivars(WebIvars { loaded: RefCell::new(Some(f)) });
        let w: Retained<WebLoadWatcher> = unsafe { msg_send![super(this), init] };
        // WebKit asks `conformsToProtocol:` of some delegates: declare it.
        unsafe {
            if let Some(p) = objc2::runtime::AnyProtocol::get(c"WKNavigationDelegate") {
                let cls = w.class() as *const AnyClass as *mut AnyClass;
                objc2::ffi::class_addProtocol(cls, p);
            }
        }
        w
    }
}

// MARK: The report bridge: a page in the viewer talks back to Heimdall (`docs/one-system.md`, Responses).

/// Defines `window.heimdall.send(action, value, label)` before the page's own scripts run, and tells the page it is
/// there with a `heimdall-ready` event (on `document` and `window`, at DOMContentLoaded or at once if later).
pub const BRIDGE_SCRIPT: &str = r#"(()=>{const post=m=>{try{window.webkit.messageHandlers.heimdall.postMessage(JSON.stringify(m))}catch(e){}};
window.heimdall=Object.freeze({send(action,value,label){post({action:String(action),value:value===undefined?null:value,label:label==null?null:String(label)})}});
const ready=()=>{for(const t of[document,window])t.dispatchEvent(new Event('heimdall-ready'))};
if(document.readyState==='loading')document.addEventListener('DOMContentLoaded',ready,{once:true});else ready();})();"#;

/// What a report message does once received: (action, value, label).
pub type OnReport = Rc<dyn Fn(String, serde_json::Value, Option<String>)>;

type OnMessage = Rc<dyn Fn(serde_json::Value)>;

pub struct BridgeIvars {
    on_message: RefCell<Option<OnMessage>>,
}

define_class!(
    /// The `heimdall` script message handler: each message's body (a JSON string, or an object WebKit bridged) as JSON.
    #[unsafe(super(objc2_foundation::NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "FIViewerReportBridge"]
    #[ivars = BridgeIvars]
    pub struct ReportBridge;

    impl ReportBridge {
        #[unsafe(method(userContentController:didReceiveScriptMessage:))]
        fn received(&self, _ucc: &AnyObject, message: &AnyObject) {
            let body: Option<Retained<AnyObject>> = unsafe { msg_send![message, body] };
            let Some(v) = body.and_then(|b| json_of(&b)) else { return };
            let f = self.ivars().on_message.borrow().clone();
            if let Some(f) = f {
                f(v);
            }
        }
    }
);

impl ReportBridge {
    fn new(f: OnMessage) -> Retained<ReportBridge> {
        let this = Self::alloc(mtm()).set_ivars(BridgeIvars { on_message: RefCell::new(Some(f)) });
        let b: Retained<ReportBridge> = unsafe { msg_send![super(this), init] };
        unsafe {
            if let Some(p) = objc2::runtime::AnyProtocol::get(c"WKScriptMessageHandler") {
                let cls = b.class() as *const AnyClass as *mut AnyClass;
                objc2::ffi::class_addProtocol(cls, p);
            }
        }
        b
    }
}

/// A script message's body as JSON: a string stays a string (the bridge sends JSON text); anything else WebKit
/// bridged (dictionary, array, number) goes through NSJSONSerialization.
fn json_of(o: &AnyObject) -> Option<serde_json::Value> {
    unsafe {
        let is_str: bool = msg_send![o, isKindOfClass: objc2::class!(NSString)];
        if is_str {
            let s: &NSString = &*(o as *const AnyObject as *const NSString);
            return Some(serde_json::Value::String(s.to_string()));
        }
        let js = class(c"NSJSONSerialization")?;
        let ok: bool = msg_send![js, isValidJSONObject: o];
        if !ok {
            return None;
        }
        let data: Option<Retained<objc2_foundation::NSData>> = msg_send![js, dataWithJSONObject: o, options: 0usize, error: std::ptr::null_mut::<*mut AnyObject>()];
        serde_json::from_slice(&data?.to_vec()).ok()
    }
}

// MARK: Proposed actions: island buttons (34 pt capsules, at most three a row, the first one primary).

type Cap = (ViewerAction, Retained<CALayer>, Retained<CATextLayer>);

pub struct RowIvars {
    actions: Vec<ViewerAction>,
    on_action: RefCell<Option<Rc<dyn Fn(ViewerAction)>>>,
    caps: RefCell<Vec<Cap>>,
    chosen: RefCell<Option<String>>,
    pressed: Cell<Option<usize>>,
    reset_work: RefCell<Option<Work1>>,
    /// "Sure?" over the clicked button's row and the timer that puts the row back.
    confirming: RefCell<Option<(usize, confirm::Row, Work1)>>,
}

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "FIViewerActionRow"]
    #[ivars = RowIvars]
    pub struct ActionRow;

    impl ActionRow {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _e: Option<&NSEvent>) -> bool {
            true
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, e: &NSEvent) {
            let iv = self.ivars();
            if iv.chosen.borrow().is_some() || iv.confirming.borrow().is_some() {
                return;
            }
            iv.pressed.set(self.index_at(self.local(e)));
            self.restyle();
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, e: &NSEvent) {
            self.up(self.local(e));
            self.ivars().pressed.set(None);
            self.restyle();
        }
    }
);

impl ActionRow {
    pub fn new(actions: Vec<ViewerAction>) -> Retained<ActionRow> {
        let this = Self::alloc(mtm()).set_ivars(RowIvars {
            actions,
            on_action: RefCell::new(None),
            caps: RefCell::new(vec![]),
            chosen: RefCell::new(None),
            pressed: Cell::new(None),
            reset_work: RefCell::new(None),
            confirming: RefCell::new(None),
        });
        let v: Retained<ActionRow> = unsafe { msg_send![super(this), initWithFrame: CGRect::ZERO] };
        v.setWantsLayer(true);
        if let Some(l) = v.layer() {
            look::no_implicit(&l);
        }
        v
    }

    pub fn set_on_action(&self, f: Rc<dyn Fn(ViewerAction)>) {
        *self.ivars().on_action.borrow_mut() = Some(f);
    }

    fn scale(&self) -> f64 {
        self.window().map(|w| w.backingScaleFactor()).unwrap_or(2.0)
    }

    fn local(&self, e: &NSEvent) -> Point {
        let p = self.convertPoint_fromView(e.locationInWindow(), None);
        Point::new(p.x, p.y)
    }

    fn retained(&self) -> Retained<ActionRow> {
        unsafe { Retained::retain(self as *const Self as *mut Self) }.expect("self")
    }

    pub fn layout_buttons(&self) {
        self.cancel_confirm();
        let iv = self.ivars();
        for (_, c, t) in iv.caps.borrow_mut().drain(..) {
            c.removeFromSuperlayer();
            t.removeFromSuperlayer();
        }
        let Some(root) = self.layer() else { return };
        let width = self.bounds().size.width;
        let scale = self.scale();
        let mut y = 0.0;
        for row in ViewerAction::rows(&iv.actions) {
            let n = row.len() as f64;
            let w = (width - ACTION_GAP * (n - 1.0)) / n;
            for (k, a) in row.into_iter().enumerate() {
                let cap = look::layer();
                cap.setFrame(cgr(Rect::new(k as f64 * (w + ACTION_GAP), y, w, ACTION_H)));
                cap.setCornerRadius(ACTION_H / 2.0);
                look::continuous(&cap);
                let t = CATextLayer::new();
                look::no_implicit(&t);
                t.setContentsScale(scale);
                unsafe {
                    t.setAlignmentMode(objc2_quartz_core::kCAAlignmentCenter);
                    t.setTruncationMode(objc2_quartz_core::kCATruncationEnd);
                }
                root.addSublayer(&cap);
                root.addSublayer(&t);
                iv.caps.borrow_mut().push((a, cap, t));
            }
            y += ACTION_H + ACTION_GAP;
        }
        self.restyle();
    }

    fn restyle(&self) {
        let iv = self.ivars();
        let chosen = iv.chosen.borrow().clone();
        let font = look::button_font();
        let lh = look::line_height(&font) + 1.0;
        look::quiet(|| {
            for (i, (a, cap, t)) in iv.caps.borrow().iter().enumerate() {
                let primary = i == 0;
                let is_chosen = chosen.as_deref() == Some(a.action.as_str());
                let dim = chosen.is_some() && !is_chosen;
                let fill = if dim {
                    look::white(0.08)
                } else if is_chosen || primary {
                    look::white(1.0)
                } else {
                    look::white(0.16)
                };
                let ink = if dim {
                    look::white(0.35)
                } else if is_chosen || primary {
                    look::black()
                } else {
                    look::ink()
                };
                look::set_bg(cap, &fill);
                cap.setOpacity(if iv.pressed.get() == Some(i) { 0.7 } else { 1.0 });
                let label = if is_chosen { format!("✓  {}", a.label) } else { a.label.clone() };
                let s = look::attributed(&label, &look::attrs(&font, &ink, None, false));
                unsafe { t.setString(Some(any(&*s))) };
                let f = cap.frame();
                t.setFrame(CGRect::new(
                    CGPoint::new(f.origin.x + 8.0, f.origin.y + f.size.height / 2.0 - lh / 2.0),
                    CGSize::new(f.size.width - 16.0, lh),
                ));
            }
        });
    }

    fn index_at(&self, p: Point) -> Option<usize> {
        self.ivars().caps.borrow().iter().position(|(_, c, _)| look::rect_of(c.frame()).contains(p))
    }

    fn up(&self, p: Point) {
        let iv = self.ivars();
        let confirming = iv.confirming.borrow().as_ref().map(|(i, row, _)| (*i, row.clone()));
        if let Some((i, row)) = confirming {
            match confirm::hit(&row, p, sys::uptime()) {
                confirm::Hit::Confirm => {
                    self.cancel_confirm();
                    self.send(i);
                }
                confirm::Hit::Cancel | confirm::Hit::Outside => self.cancel_confirm(),
                confirm::Hit::Ignored => {}
            }
            return;
        }
        if iv.chosen.borrow().is_some() {
            return;
        }
        if let Some(p0) = iv.pressed.get() {
            if self.index_at(p) == Some(p0) {
                self.click(p0);
            }
        }
    }

    /// A click (also the self-test's and the renders' path): an answer asks "Sure?" first; "Later" goes at once.
    pub fn click(&self, i: usize) {
        let action = match self.ivars().caps.borrow().get(i) {
            Some((a, _, _)) => a.action.clone(),
            None => return,
        };
        if confirm::needed(&action) {
            self.ask(i)
        } else {
            self.send(i)
        }
    }

    /// Lab: answer the "Sure?" row.
    pub fn confirm(&self, yes: bool) {
        let Some(i) = self.ivars().confirming.borrow().as_ref().map(|c| c.0) else { return };
        self.cancel_confirm();
        if yes {
            self.send(i);
        }
    }

    pub fn is_confirming(&self) -> bool {
        self.ivars().confirming.borrow().is_some()
    }

    /// The action whose choice is showing (for 1.6 s after a click).
    pub fn chosen(&self) -> Option<String> {
        self.ivars().chosen.borrow().clone()
    }

    fn ask(&self, i: usize) {
        self.cancel_confirm();
        let iv = self.ivars();
        let frames: Vec<Rect> = iv.caps.borrow().iter().map(|(_, c, _)| look::rect_of(c.frame())).collect();
        let Some(clicked) = frames.get(i).copied() else { return };
        let row = frames.iter().filter(|f| (f.min_y() - clicked.min_y()).abs() < 1.0).fold(clicked, |a, b| a.union(b));
        let r = confirm::row(row, clicked, &look::black(), self.scale());
        if let Some(l) = self.layer() {
            l.addSublayer(&r.layer);
        }
        let weak = objc2::rc::Weak::from_retained(&self.retained());
        let w = sys::after_cancellable(confirm::WINDOW, move || {
            if let Some(v) = weak.load() {
                v.cancel_confirm();
            }
        });
        *iv.confirming.borrow_mut() = Some((i, r, w));
    }

    fn cancel_confirm(&self) {
        let Some((_, row, w)) = self.ivars().confirming.borrow_mut().take() else { return };
        w.cancel();
        confirm::dismiss(&row);
    }

    /// Report it to the agent and show the choice for a moment.
    fn send(&self, i: usize) {
        let Some(a) = self.ivars().caps.borrow().get(i).map(|c| c.0.clone()) else { return };
        let f = self.ivars().on_action.borrow().clone();
        if let Some(f) = f {
            f(a.clone());
        }
        self.show_choice(&a.action);
    }

    pub fn show_choice(&self, action: &str) {
        let iv = self.ivars();
        *iv.chosen.borrow_mut() = Some(action.to_owned());
        self.restyle();
        if let Some(w) = iv.reset_work.borrow_mut().take() {
            w.cancel();
        }
        let weak = objc2::rc::Weak::from_retained(&self.retained());
        let w = sys::after_cancellable(1.6, move || {
            if let Some(v) = weak.load() {
                *v.ivars().chosen.borrow_mut() = None;
                v.restyle();
            }
        });
        *iv.reset_work.borrow_mut() = Some(w);
    }

    /// A point of this row (y down) in its window's coordinates (AppKit, y up).
    pub fn to_window(&self, p: Point) -> Point {
        let q = self.convertPoint_toView(CGPoint::new(p.x, p.y), None);
        Point::new(q.x, q.y)
    }

    /// The row's frame in its window's coordinates (AppKit, y up).
    pub fn frame_in_window(&self) -> Rect {
        look::rect_of(self.convertRect_toView(self.bounds(), None))
    }

    /// Button `i`'s frame in this row's coordinates (y down).
    pub fn button_frame(&self, i: usize) -> Option<Rect> {
        self.ivars().caps.borrow().get(i).map(|(_, c, _)| look::rect_of(c.frame()))
    }
}

// MARK: The viewer

pub struct FileViewer {
    me: Weak<FileViewer>,
    path: PathBuf,
    kind: RefCell<ViewerKind>,
    actions: Vec<ViewerAction>,
    on_action: RefCell<Option<Rc<dyn Fn(ViewerAction)>>>,
    /// A report page's messages (`window.heimdall.send`), after a `save` is written.
    on_report: RefCell<Option<OnReport>>,
    bridge: RefCell<Option<Retained<ReportBridge>>>,
    on_resize: RefCell<Option<Rc<dyn Fn()>>>,
    web_height: Cell<Option<f64>>,
    web_watcher: RefCell<Option<Retained<WebLoadWatcher>>>,

    root: RefCell<Option<Retained<ViewerRootView>>>,
    content: RefCell<Option<Retained<NSView>>>,
    action_row: RefCell<Option<Retained<ActionRow>>>,
    audio: RefCell<Option<Retained<AudioPlayerView>>>,
    video: RefCell<Option<Retained<NSView>>>,
    poster: RefCell<Option<Image>>,
    web: RefCell<Option<Retained<NSView>>>,
    pdf: RefCell<Option<Retained<NSView>>>,
    quicklook: RefCell<Option<Retained<NSView>>>,
    image_view: RefCell<Option<Retained<NSImageView>>>,
    text_view: RefCell<Option<Retained<NSTextView>>>,
    loaded_text: RefCell<Option<Retained<NSAttributedString>>>,
    text_loading: Cell<bool>,
    image_info: Cell<Option<CGSize>>,
    pdf_page_sizes: RefCell<Option<Vec<CGSize>>>,
    video_aspect: Cell<Option<f64>>,
    /// The last text height measured, by width: a resize measures each width once.
    measured: RefCell<Option<(Retained<NSAttributedString>, f64, f64)>>,

    /// Async loads carry the generation they started in; teardown bumps it so late results are dropped.
    generation: Cell<u64>,
    /// What was loaded (`None`: no file), taken at `new`.
    stamp: Option<Stamp>,
    /// A scroll position to restore once the content is laid out (a reload keeps the user's place).
    restore_offset: Cell<Option<CGPoint>>,
}

impl FileViewer {
    pub fn new(path: &str, actions: Vec<ViewerAction>) -> Rc<FileViewer> {
        let p = viewer_path(path);
        let st = stamp(&p);
        let kind = ViewerKind::detect(&p);
        let v = Rc::new_cyclic(|me| FileViewer {
            me: me.clone(),
            path: p,
            kind: RefCell::new(kind),
            actions,
            on_action: RefCell::new(None),
            on_report: RefCell::new(None),
            bridge: RefCell::new(None),
            on_resize: RefCell::new(None),
            web_height: Cell::new(None),
            web_watcher: RefCell::new(None),
            root: RefCell::new(None),
            content: RefCell::new(None),
            action_row: RefCell::new(None),
            audio: RefCell::new(None),
            video: RefCell::new(None),
            poster: RefCell::new(None),
            web: RefCell::new(None),
            pdf: RefCell::new(None),
            quicklook: RefCell::new(None),
            image_view: RefCell::new(None),
            text_view: RefCell::new(None),
            loaded_text: RefCell::new(None),
            text_loading: Cell::new(false),
            image_info: Cell::new(None),
            pdf_page_sizes: RefCell::new(None),
            video_aspect: Cell::new(None),
            measured: RefCell::new(None),
            generation: Cell::new(0),
            stamp: st,
            restore_offset: Cell::new(None),
        });
        v.read_now();
        v
    }

    /// Reads small text at once (the island's size is known before it opens).
    fn read_now(&self) {
        let kind = self.kind();
        if matches!(kind, ViewerKind::Markdown | ViewerKind::Text(_)) && self.file_size() <= viewer_text::SYNC_TEXT_LIMIT {
            *self.loaded_text.borrow_mut() = Some(Retained::into_super(viewer_text::read_text(&self.path, &kind)));
        }
    }

    fn file_size(&self) -> u64 {
        std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0)
    }

    pub fn kind(&self) -> ViewerKind {
        self.kind.borrow().clone()
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn actions(&self) -> &[ViewerAction] {
        &self.actions
    }

    pub fn set_on_report(&self, f: OnReport) {
        *self.on_report.borrow_mut() = Some(f);
    }

    /// A message from the page: `save` writes its data beside the page first (a bare `*.json` name only), then the
    /// owner hears of it (the island writes the `report` response).
    fn report(&self, body: serde_json::Value) {
        let Some((action, mut value, label)) = crate::model::onesys::report_message(&body) else { return };
        if action == "save" {
            match crate::model::onesys::save_request(&value) {
                Some((file, data)) => match crate::model::onesys::save_beside(&self.path, &file, &data) {
                    Ok(p) => {
                        if let Some(o) = value.as_object_mut() {
                            o.insert("saved".into(), serde_json::Value::from(p.to_string_lossy().into_owned()));
                        }
                    }
                    Err(e) => eprintln!("viewer: report save failed: {e}"),
                },
                None => {
                    if let Some(o) = value.as_object_mut() {
                        o.insert("refused".into(), serde_json::Value::Bool(true));
                    }
                }
            }
        }
        let f = self.on_report.borrow().clone();
        if let Some(f) = f {
            f(action, value, label);
        }
    }

    pub fn set_on_action(&self, f: Box<dyn Fn(ViewerAction)>) {
        let f: Rc<dyn Fn(ViewerAction)> = Rc::from(f);
        *self.on_action.borrow_mut() = Some(f.clone());
        if let Some(r) = self.action_row.borrow().as_ref() {
            r.set_on_action(f);
        }
    }

    pub fn set_on_resize(&self, f: Box<dyn Fn()>) {
        *self.on_resize.borrow_mut() = Some(Rc::from(f));
    }

    fn resized(&self) {
        let f = self.on_resize.borrow().clone();
        if let Some(f) = f {
            f();
        }
    }

    // MARK: Size

    /// The pill's symbol for a kind of file.
    pub fn symbol(kind: &ViewerKind) -> &'static str {
        match kind {
            ViewerKind::Markdown => "doc.richtext",
            ViewerKind::Text(_) => "chevron.left.forwardslash.chevron.right",
            ViewerKind::Image => "photo",
            ViewerKind::Pdf => "doc.text",
            ViewerKind::Audio => "waveform",
            ViewerKind::Video => "play.rectangle",
            ViewerKind::Html => "globe",
            ViewerKind::Other => "doc",
            ViewerKind::Missing => "questionmark.folder",
        }
    }

    /// The widest the viewer gets for its kind on a screen this size.
    pub fn max_width(kind: &ViewerKind, screen: Size) -> f64 {
        match kind {
            ViewerKind::Audio => 460.0,
            ViewerKind::Missing => 320.0,
            ViewerKind::Html | ViewerKind::Video | ViewerKind::Pdf | ViewerKind::Other => (screen.w * 0.56).max(480.0).min(920.0),
            _ => (screen.w * 0.5).max(420.0).min(760.0),
        }
    }

    /// Height of the proposed-action rows plus the gap under them (0 without actions).
    pub fn actions_height(&self) -> f64 {
        let rows = ViewerAction::rows(&self.actions).len();
        if rows == 0 {
            0.0
        } else {
            rows as f64 * ACTION_H + (rows - 1) as f64 * ACTION_GAP + 14.0
        }
    }

    /// The open island's size on a screen this size: chrome (title row, padding), the action rows, the content.
    pub fn island_size(&self, screen: Size) -> Size {
        let chrome = look::BODY_TOP + look::BOTTOM_PAD;
        let kind = self.kind();
        let max_w = Self::max_width(&kind, screen) - 2.0 * look::PAD;
        let max_h = (screen.h * 0.78).max(240.0) - chrome - self.actions_height();
        let c = self.content_size(max_w, max_h);
        let w = (c.w + 2.0 * look::PAD).max(if self.actions.is_empty() { 280.0 } else { 360.0 });
        Size::new(w.ceil(), (c.h + self.actions_height() + chrome).ceil())
    }

    /// Where the viewer's body sits in an open island of `size` (shape coordinates, y down).
    pub fn body_rect(size: Size) -> Rect {
        Rect::new(look::PAD, look::BODY_TOP, size.w - 2.0 * look::PAD, size.h - look::BODY_TOP - look::BOTTOM_PAD)
    }

    /// The content area's size (without the action rows), within `max_w` × `max_h`.
    pub fn content_size(&self, max_w: f64, max_h: f64) -> Size {
        let max_h = max_h.max(80.0);
        match self.kind() {
            ViewerKind::Missing => Size::new(max_w, 44.0),
            ViewerKind::Markdown | ViewerKind::Text(_) => {
                let Some(t) = self.loaded_text.borrow().clone() else { return Size::new(max_w, max_h) };
                let w = max_w - 8.0;
                let cached = self.measured.borrow().as_ref().and_then(|(mt, mw, mh)| (Retained::as_ptr(mt) == Retained::as_ptr(&t) && *mw == w).then_some(*mh));
                let h = cached.unwrap_or_else(|| {
                    let h = viewer_text::text_height(&t, w);
                    *self.measured.borrow_mut() = Some((t.clone(), w, h));
                    h
                });
                Size::new(max_w, max_h.min((h + 6.0).max(44.0)).ceil())
            }
            ViewerKind::Image => {
                if self.image_info.get().is_none() {
                    self.image_info.set(draw::image_pixel_size(&self.path));
                }
                let Some(px) = self.image_info.get().filter(|p| p.width > 0.0 && p.height > 0.0) else {
                    return Size::new(max_w, max_h * 0.6);
                };
                // Never larger than the image's own points (a 2× screen), never cut.
                let (nw, nh) = (px.width / 2.0, px.height / 2.0);
                let k = 1f64.min(max_w / nw).min(max_h / nh);
                Size::new((nw * k).floor().max(240.0), (nh * k).floor().max(60.0))
            }
            ViewerKind::Pdf => {
                if self.pdf_page_sizes.borrow().is_none() {
                    *self.pdf_page_sizes.borrow_mut() = Some(pdf_page_sizes(&self.path, 40));
                }
                let pages = self.pdf_page_sizes.borrow().clone().unwrap_or_default();
                let Some(p0) = pages.first().filter(|p| p.width > 0.0) else { return Size::new(max_w, max_h) };
                // The first page fits whole (a white page is a lot of light; it never fills a wide screen).
                let pw = (max_w - 2.0 * PDF_INSET).min((max_h - 2.0 * PDF_INSET) * p0.width / p0.height);
                let total = pages.iter().map(|p| p.height * pw / p.width.max(1.0) + PDF_GAP).sum::<f64>() + 2.0 * PDF_INSET - PDF_GAP;
                Size::new((pw + 2.0 * PDF_INSET).max(380.0).floor(), max_h.min(total).ceil())
            }
            ViewerKind::Audio => Size::new(max_w, viewer_audio::HEIGHT),
            ViewerKind::Video => {
                if self.video_aspect.get().is_none() {
                    self.video_aspect.set(Some(video_aspect(&self.path).unwrap_or(9.0 / 16.0)));
                }
                let a = self.video_aspect.get().unwrap_or(9.0 / 16.0);
                let (mut w, mut h) = (max_w, max_w * a);
                if h > max_h {
                    h = max_h;
                    w = h / a;
                }
                Size::new(w.max(320.0).floor(), h.floor())
            }
            ViewerKind::Html => Size::new(max_w, max_h.min(self.web_height.get().unwrap_or(max_h).max(160.0))),
            ViewerKind::Other => Size::new(max_w.min(720.0), max_h.min(540.0)),
        }
    }

    // MARK: Views

    /// The live view, built once: the action rows on top, the renderer under them. `size` is the whole body area.
    pub fn make_view(&self, size: Size) -> Retained<NSView> {
        if let Some(r) = self.root.borrow().clone() {
            self.layout(size);
            return Retained::into_super(r);
        }
        let r = ViewerRootView::new(cgr(Rect::new(0.0, 0.0, size.w, size.h)));
        if let Some(a) = NSAppearance::appearanceNamed(unsafe { objc2_app_kit::NSAppearanceNameDarkAqua }) {
            r.setAppearance(Some(&a));
        }
        *self.root.borrow_mut() = Some(r.clone());
        if !self.actions.is_empty() {
            let row = ActionRow::new(self.actions.clone());
            if let Some(f) = self.on_action.borrow().clone() {
                row.set_on_action(f);
            }
            r.addSubview(&row);
            *self.action_row.borrow_mut() = Some(row);
        }
        let c = self.build_content();
        r.addSubview(&c);
        *self.content.borrow_mut() = Some(c);
        self.layout(size);
        Retained::into_super(r)
    }

    pub fn root(&self) -> Option<Retained<NSView>> {
        self.root.borrow().clone().map(Retained::into_super)
    }

    pub fn layout(&self, size: Size) {
        let Some(root) = self.root.borrow().clone() else { return };
        root.setFrameSize(CGSize::new(size.w, size.h));
        let top = self.actions_height();
        if let Some(row) = self.action_row.borrow().clone() {
            row.setFrame(cgr(Rect::new(0.0, 0.0, size.w, (top - 14.0).max(0.0))));
            row.layout_buttons();
        }
        let content = self.content.borrow().clone();
        if let Some(c) = content {
            let was_empty = look::rect_of(c.frame()).is_empty();
            c.setFrame(cgr(Rect::new(0.0, top, size.w, (size.h - top).max(0.0))));
            if was_empty {
                if let Some(pdf) = self.pdf.borrow().clone() {
                    to_top(&pdf);
                }
            }
        }
        self.apply_restore();
    }

    fn build_content(&self) -> Retained<NSView> {
        let mtm = mtm();
        match self.kind() {
            ViewerKind::Missing => {
                let name = self.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                message_view("Not found", &name)
            }
            ViewerKind::Markdown | ViewerKind::Text(_) => self.build_text(),
            ViewerKind::Image => {
                let v = NSImageView::new(mtm);
                v.setImageScaling(objc2_app_kit::NSImageScaling::ScaleProportionallyUpOrDown);
                v.setAnimates(false);
                v.setWantsLayer(true);
                if let Some(l) = v.layer() {
                    l.setCornerRadius(10.0);
                    look::continuous(&l);
                    l.setMasksToBounds(true);
                }
                self.set_image(&v);
                *self.image_view.borrow_mut() = Some(v.clone());
                Retained::into_super(Retained::into_super(v))
            }
            ViewerKind::Pdf => {
                let cls = class(c"PDFView").expect("PDFKit");
                let v: Retained<NSView> = unsafe { msg_send![cls, new] };
                unsafe {
                    let _: () = msg_send![&*v, setAutoScales: true];
                    let _: () = msg_send![&*v, setDisplayMode: 1isize];
                    let _: () = msg_send![&*v, setDisplaysPageBreaks: true];
                    let _: () = msg_send![&*v, enablePageShadows: false];
                    let _: () = msg_send![&*v, setBackgroundColor: &*look::black()];
                    let m = objc2_foundation::NSEdgeInsets { top: PDF_GAP / 2.0, left: PDF_INSET, bottom: PDF_GAP / 2.0, right: PDF_INSET };
                    let _: () = msg_send![&*v, setPageBreakMargins: m];
                }
                *self.pdf.borrow_mut() = Some(v.clone());
                self.load_pdf(&v);
                v
            }
            ViewerKind::Audio => {
                let a = AudioPlayerView::new(&self.path, mtm);
                *self.audio.borrow_mut() = Some(a.clone());
                Retained::into_super(a)
            }
            ViewerKind::Video => {
                let cls = class(c"AVPlayerView").expect("AVKit");
                let v: Retained<NSView> = unsafe { msg_send![cls, new] };
                let player = unsafe { objc2_av_foundation::AVPlayer::playerWithURL(&file_url(&self.path), mtm) };
                unsafe {
                    player.setActionAtItemEnd(objc2_av_foundation::AVPlayerActionAtItemEnd::Pause);
                    if viewer_audio::muted_by_env() {
                        player.setMuted(true);
                    }
                    let _: () = msg_send![&*v, setControlsStyle: 1isize];
                    let _: () = msg_send![&*v, setVideoGravity: objc2_av_foundation::AVLayerVideoGravityResizeAspect];
                    let _: () = msg_send![&*v, setShowsFullScreenToggleButton: false];
                    let _: () = msg_send![&*v, setAllowsPictureInPicturePlayback: false];
                    let _: () = msg_send![&*v, setPlayer: &*player];
                }
                v.setWantsLayer(true);
                if let Some(l) = v.layer() {
                    l.setCornerRadius(10.0);
                    look::continuous(&l);
                    l.setMasksToBounds(true);
                }
                *self.video.borrow_mut() = Some(v.clone());
                let gen = self.generation.get();
                let path = self.path.clone();
                let back = unsafe { MainOnly::new(self.me.clone()) };
                std::thread::spawn(move || {
                    let img = unsafe { MainOnly::new(video_poster(&path)) };
                    sys::soon(move || {
                        let (back, img) = (back.into_inner(self::mtm()), img.into_inner(self::mtm()));
                        if let Some(me) = back.upgrade() {
                            if me.generation.get() == gen {
                                *me.poster.borrow_mut() = img;
                            }
                        }
                    });
                });
                v
            }
            ViewerKind::Html => {
                let w = self.build_web();
                *self.web.borrow_mut() = Some(w.clone());
                w
            }
            ViewerKind::Other => {
                let cls = class(c"QLPreviewView").expect("Quartz");
                let frame = cgr(Rect::new(0.0, 0.0, 400.0, 300.0));
                let q: Option<Retained<NSView>> = unsafe { msg_send![msg_send![cls, alloc], initWithFrame: frame, style: 0usize] };
                let q = q.unwrap_or_else(|| unsafe { msg_send![cls, new] });
                unsafe {
                    let _: () = msg_send![&*q, setShouldCloseWithWindow: false];
                    let _: () = msg_send![&*q, setAutostarts: false];
                    let _: () = msg_send![&*q, setPreviewItem: &*file_url(&self.path)];
                }
                *self.quicklook.borrow_mut() = Some(q.clone());
                q
            }
        }
    }

    fn set_image(&self, v: &NSImageView) {
        let img = draw::thumbnail(&self.path, 2400, false).map(|i| {
            NSImage::initWithCGImage_size(NSImage::alloc(), i.cg(), CGSize::new(0.0, 0.0))
        });
        v.setImage(img.as_deref());
    }

    /// PDFDocument parses lazily but can still take a while on a big file: open it off the main thread.
    fn load_pdf(&self, v: &Retained<NSView>) {
        let gen = self.generation.get();
        let path = self.path.clone();
        let back = unsafe { MainOnly::new((self.me.clone(), v.clone())) };
        std::thread::spawn(move || {
            let doc: Option<Retained<AnyObject>> = objc2::rc::autoreleasepool(|_| unsafe {
                let cls = AnyClass::get(c"PDFDocument")?;
                msg_send![msg_send![cls, alloc], initWithURL: &*file_url(&path)]
            });
            let doc = unsafe { MainOnly::new(doc) };
            sys::soon(move || {
                let (me, v) = back.into_inner(mtm());
                let doc = doc.into_inner(mtm());
                let Some(me) = me.upgrade() else { return };
                if me.generation.get() != gen {
                    return;
                }
                unsafe {
                    let _: () = msg_send![&*v, setDocument: doc.as_deref()];
                    let _: () = msg_send![&*v, setAutoScales: true];
                }
                to_top(&v);
                me.apply_restore();
            });
        });
    }

    fn build_web(&self) -> Retained<NSView> {
        let cfg_cls = class(c"WKWebViewConfiguration").expect("WebKit");
        let cfg: Retained<AnyObject> = unsafe { msg_send![cfg_cls, new] };
        unsafe {
            let _: () = msg_send![&*cfg, setSuppressesIncrementalRendering: true];
            if viewer_audio::muted_by_env() {
                // Lab runs: no page media starts on its own.
                let _: () = msg_send![&*cfg, setMediaTypesRequiringUserActionForPlayback: usize::MAX];
            }
        }
        let cls = class(c"WKWebView").expect("WebKit");
        let w: Retained<NSView> = unsafe { msg_send![msg_send![cls, alloc], initWithFrame: CGRect::ZERO, configuration: &*cfg] };
        unsafe {
            let _: () = msg_send![&*w, setValue: &*NSNumber::new_bool(false), forKey: &*ns("drawsBackground")];
            let _: () = msg_send![&*w, setUnderPageBackgroundColor: &*look::black()];
        }
        let me = self.me.clone();
        let web = objc2::rc::Weak::from_retained(&w);
        let watcher = WebLoadWatcher::new(Box::new(move || {
            let Some(w) = web.load() else { return };
            let me = me.clone();
            let block = block2::RcBlock::new(move |v: *mut AnyObject, _e: *mut AnyObject| {
                let Some(me) = me.upgrade() else { return };
                if v.is_null() {
                    return;
                }
                let h: f64 = unsafe { msg_send![&*v, doubleValue] };
                if h > 0.0 && Some(h) != me.web_height.get() {
                    me.web_height.set(Some(h));
                    me.resized();
                }
            });
            let js = "(()=>{const b=document.body;if(!b)return 0;const c=getComputedStyle(b);return Math.ceil(b.getBoundingClientRect().height+parseFloat(c.marginTop)+parseFloat(c.marginBottom))})()";
            unsafe {
                let _: () = msg_send![&*w, evaluateJavaScript: &*ns(js), completionHandler: &*block];
            }
        }));
        unsafe {
            let _: () = msg_send![&*w, setNavigationDelegate: &*watcher];
        }
        // Offline (review, 30 Oct): every web load (pages, scripts, images, fetch, sockets) is blocked by a content rule
        // list before the page loads, so a page that reads its folder can send nothing out; local files still load.
        // A page under a trusted folder may show HTTPS pictures and video (`web_rules`, bug note 2026-11-04T00-46-09).
        // The page loads only once the rules are in place; rules that fail to compile leave it unloaded.
        let url = file_url(&self.path);
        let dir = file_url(self.path.parent().unwrap_or(Path::new("/")));
        let (wv, ucc): (Retained<NSView>, Retained<AnyObject>) = (w.clone(), unsafe { msg_send![&*cfg, userContentController] });
        // The report bridge: `window.heimdall` before the page's scripts, its messages to the island's responses.
        unsafe {
            let me = self.me.clone();
            let bridge = ReportBridge::new(Rc::new(move |v| {
                if let Some(me) = me.upgrade() {
                    me.report(v);
                }
            }));
            let _: () = msg_send![&*ucc, addScriptMessageHandler: &*bridge, name: &*ns("heimdall")];
            if let Some(us_cls) = class(c"WKUserScript") {
                let us: Retained<AnyObject> = msg_send![msg_send![us_cls, alloc], initWithSource: &*ns(BRIDGE_SCRIPT), injectionTime: 0isize, forMainFrameOnly: true];
                let _: () = msg_send![&*ucc, addUserScript: &*us];
            }
            *self.bridge.borrow_mut() = Some(bridge);
        }
        let done = block2::RcBlock::new(move |list: *mut AnyObject, err: *mut AnyObject| {
            if list.is_null() {
                let why: String = if err.is_null() { String::new() } else { unsafe { let d: Retained<objc2_foundation::NSString> = msg_send![&*err, localizedDescription]; d.to_string() } };
                eprintln!("viewer: the offline rules did not compile ({why}); the page is not loaded");
                return;
            }
            unsafe {
                let _: () = msg_send![&*ucc, addContentRuleList: &*list];
                let _: Option<Retained<AnyObject>> = msg_send![&*wv, loadFileURL: &*url, allowingReadAccessToURL: &*dir];
            }
        });
        unsafe {
            let store: Option<Retained<AnyObject>> = class(c"WKContentRuleListStore").and_then(|c| msg_send![c, defaultStore]);
            match store {
                Some(store) => {
                    let rules = web_rules(&web_roots());
                    let _: () = msg_send![&*store, compileContentRuleListForIdentifier: &*ns("fi-viewer-web"), encodedContentRuleList: &*ns(&rules), completionHandler: &*done];
                }
                None => eprintln!("viewer: no content rule store; the page is not loaded"),
            }
        }
        *self.web_watcher.borrow_mut() = Some(watcher);
        w
    }

    fn build_text(&self) -> Retained<NSView> {
        let mtm = mtm();
        let sv = NSScrollView::new(mtm);
        sv.setDrawsBackground(false);
        sv.setHasVerticalScroller(true);
        sv.setScrollerStyle(objc2_app_kit::NSScrollerStyle::Overlay);
        sv.setScrollerKnobStyle(objc2_app_kit::NSScrollerKnobStyle::Light);
        sv.setAutohidesScrollers(true);
        // TextKit 1: code blocks and tables are text blocks (backgrounds, cells).
        let tv = NSTextView::initUsingTextLayoutManager(NSTextView::alloc(mtm), false);
        tv.setDrawsBackground(false);
        tv.setEditable(false);
        tv.setSelectable(true);
        tv.setTextContainerInset(CGSize::new(4.0, 2.0));
        if let Some(tc) = unsafe { tv.textContainer() } {
            tc.setWidthTracksTextView(true);
        }
        tv.setVerticallyResizable(true);
        tv.setAutoresizingMask(objc2_app_kit::NSAutoresizingMaskOptions::ViewWidthSizable);
        unsafe {
            let cursor = objc2_app_kit::NSCursor::pointingHandCursor();
            let blue = look::blue();
            let link = NSDictionary::<NSString, AnyObject>::from_slices(&[&*ns("NSColor"), &*ns("NSCursor")], &[any(&*blue), any(&*cursor)]);
            tv.setLinkTextAttributes(Some(&link));
            let sel_bg = look::white(0.22);
            let sel = NSDictionary::<NSString, AnyObject>::from_slices(&[&*ns("NSBackgroundColor")], &[any(&*sel_bg)]);
            tv.setSelectedTextAttributes(&sel);
        }
        sv.setDocumentView(Some(&tv));
        *self.text_view.borrow_mut() = Some(tv.clone());
        let loaded = self.loaded_text.borrow().clone();
        if let Some(t) = loaded {
            set_text(&tv, &t);
        } else if !self.text_loading.get() {
            self.load_text_async();
        }
        Retained::into_super(sv)
    }

    fn load_text_async(&self) {
        self.text_loading.set(true);
        let gen = self.generation.get();
        let path = self.path.clone();
        let kind = self.kind();
        let back = unsafe { MainOnly::new(self.me.clone()) };
        std::thread::spawn(move || {
            let t = objc2::rc::autoreleasepool(|_| unsafe { MainOnly::new(viewer_text::read_text(&path, &kind)) });
            sys::soon(move || {
                let (back, t) = (back.into_inner(mtm()), t.into_inner(mtm()));
                let Some(me) = back.upgrade() else { return };
                if me.generation.get() != gen {
                    return;
                }
                me.text_loading.set(false);
                let t: Retained<NSAttributedString> = Retained::into_super(t);
                *me.loaded_text.borrow_mut() = Some(t.clone());
                if let Some(tv) = me.text_view.borrow().clone() {
                    set_text(&tv, &t);
                }
                me.resized();
                me.apply_restore();
            });
        });
    }

    // MARK: Lifecycle

    /// A still of the viewer at `size` (shown while the shape flies open; also the offscreen renders).
    pub fn snapshot(&self, size: Size) -> Option<Image> {
        let r = self.make_view(size);
        r.layoutSubtreeIfNeeded();
        let rep: Retained<NSBitmapImageRep> = r.bitmapImageRepForCachingDisplayInRect(r.bounds())?;
        r.cacheDisplayInRect_toBitmapImageRep(r.bounds(), &rep);
        let rb = r.bounds();
        let kind = self.kind();
        let content = self.content.borrow().clone();
        if kind == ViewerKind::Pdf {
            if let (Some(v), Some(ctx)) = (content.as_ref(), NSGraphicsContext::graphicsContextWithBitmapImageRep(&rep)) {
                let url = file_url(&self.path);
                // SAFETY: CoreGraphics on a live bitmap context; the document is owned and released.
                unsafe {
                    if let Some(doc) = Cf::new(CGPDFDocumentCreateWithURL(&*url as *const NSURL as Ref)) {
                        let cg = &*ctx.CGContext() as *const _ as Ref;
                        let f = v.convertRect_toView(v.bounds(), Some(&r));
                        let box_y_up = CGRect::new(CGPoint::new(f.origin.x, rb.size.height - f.origin.y - f.size.height), f.size);
                        CGContextSaveGState(cg);
                        CGContextClipToRect(cg, box_y_up);
                        CGContextSetFillColorWithColor(cg, &*look::black().CGColor() as *const _ as Ref);
                        CGContextFillRect(cg, box_y_up);
                        let mut y = f.origin.y + PDF_INSET; // y down, in the root's coordinates
                        let pages = CGPDFDocumentGetNumberOfPages(doc.raw()).max(1);
                        for i in 1..=pages {
                            let page = CGPDFDocumentGetPage(doc.raw(), i);
                            if y >= f.origin.y + f.size.height || page.is_null() {
                                break;
                            }
                            let b = CGPDFPageGetBoxRect(page, CROP_BOX);
                            let w = f.size.width - 2.0 * PDF_INSET;
                            let h = b.size.height * w / b.size.width.max(1.0);
                            let pr = CGRect::new(CGPoint::new(f.origin.x + PDF_INSET, rb.size.height - y - h), CGSize::new(w, h));
                            CGContextSetFillColorWithColor(cg, &*look::white(1.0).CGColor() as *const _ as Ref);
                            CGContextFillRect(cg, pr);
                            CGContextSaveGState(cg);
                            CGContextTranslateCTM(cg, pr.origin.x, pr.origin.y);
                            CGContextScaleCTM(cg, w / b.size.width, h / b.size.height);
                            CGContextTranslateCTM(cg, -b.origin.x, -b.origin.y);
                            CGContextDrawPDFPage(cg, page);
                            CGContextRestoreGState(cg);
                            y += h + PDF_GAP;
                        }
                        CGContextRestoreGState(cg);
                        ctx.flushGraphics();
                    }
                }
            }
        }
        // Out-of-process or layer-only content does not draw through cacheDisplay: paint the first video frame.
        let poster = self.poster.borrow().clone();
        let video = self.video.borrow().clone();
        if let (Some(poster), Some(v), Some(ctx)) = (poster, video, NSGraphicsContext::graphicsContextWithBitmapImageRep(&rep)) {
            let f = v.convertRect_toView(v.bounds(), Some(&r));
            let flipped = CGRect::new(CGPoint::new(f.origin.x, rb.size.height - f.origin.y - f.size.height), f.size);
            let a = poster.height() as f64 / poster.width().max(1) as f64;
            let mut pr = flipped;
            if flipped.size.width * a > flipped.size.height {
                pr.size.width = flipped.size.height / a;
            } else {
                pr.size.height = flipped.size.width * a;
            }
            pr.origin = CGPoint::new(
                flipped.origin.x + flipped.size.width / 2.0 - pr.size.width / 2.0,
                flipped.origin.y + flipped.size.height / 2.0 - pr.size.height / 2.0,
            );
            NSGraphicsContext::saveGraphicsState_class();
            NSGraphicsContext::setCurrentContext(Some(&ctx));
            NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(flipped, 10.0, 10.0).addClip();
            unsafe { CGContextDrawImage(&*ctx.CGContext() as *const _ as Ref, pr, poster.0.raw()) };
            ctx.flushGraphics();
            NSGraphicsContext::restoreGraphicsState_class();
        }
        let cg: Option<Retained<CGImage>> = unsafe { msg_send![&*rep, CGImage] };
        // SAFETY: retain the rep's image; the Image owns that reference.
        cg.and_then(|c| Cf::new(unsafe { CGImageRetain(&*c as *const CGImage as Ref) })).map(Image)
    }

    /// The viewer closed to its pill: media stop (nothing plays or animates at rest).
    pub fn pause(&self) {
        if let Some(a) = self.audio.borrow().as_ref() {
            a.pause();
        }
        if let Some(v) = self.video.borrow().as_ref() {
            let p: Option<Retained<AnyObject>> = unsafe { msg_send![&**v, player] };
            if let Some(p) = p {
                let _: () = unsafe { msg_send![&*p, pause] };
            }
        }
        if let Some(w) = self.web.borrow().as_ref() {
            let js = ns("document.querySelectorAll('video,audio').forEach(m=>m.pause())");
            let _: () = unsafe { msg_send![&**w, evaluateJavaScript: &*js, completionHandler: std::ptr::null::<AnyObject>()] };
        }
    }

    /// Stops the current content's media and work, and forgets its views (the root stays).
    fn drop_content(&self) {
        self.pause();
        if let Some(a) = self.audio.borrow_mut().take() {
            a.teardown();
        }
        if let Some(v) = self.video.borrow_mut().take() {
            let p: Option<Retained<AnyObject>> = unsafe { msg_send![&*v, player] };
            if let Some(p) = p {
                let _: () = unsafe { msg_send![&*p, replaceCurrentItemWithPlayerItem: std::ptr::null::<AnyObject>()] };
            }
        }
        if let Some(w) = self.web.borrow_mut().take() {
            unsafe {
                let _: () = msg_send![&*w, stopLoading];
                let _: () = msg_send![&*w, setNavigationDelegate: std::ptr::null::<AnyObject>()];
            }
        }
        self.web_watcher.borrow_mut().take();
        if let Some(q) = self.quicklook.borrow_mut().take() {
            let _: () = unsafe { msg_send![&*q, close] };
        }
        self.pdf.borrow_mut().take();
        self.image_view.borrow_mut().take();
        self.text_view.borrow_mut().take();
        self.poster.borrow_mut().take();
        if let Some(c) = self.content.borrow_mut().take() {
            c.removeFromSuperview();
        }
    }

    /// The island left or changed file: stop everything and drop the views.
    pub fn teardown(&self) {
        self.generation.set(self.generation.get() + 1);
        self.text_loading.set(false);
        self.drop_content();
        if let Some(row) = self.action_row.borrow_mut().take() {
            row.removeFromSuperview();
        }
        if let Some(r) = self.root.borrow_mut().take() {
            r.removeFromSuperview();
        }
    }

    pub fn show_choice(&self, action: &str) {
        if let Some(r) = self.action_row.borrow().as_ref() {
            r.show_choice(action);
        }
    }

    // MARK: Viewer refresh

    /// The file no longer matches what was loaded (inode, size or modification time; gone or come back).
    pub fn is_stale(&self) -> bool {
        stamp(&self.path) != self.stamp
    }

    /// The stamp taken when the viewer was made (`None`: no file then).
    pub fn stamp(&self) -> Option<Stamp> {
        self.stamp
    }

    /// The content's scroll view (text, markdown, PDF).
    pub fn scroll_view(&self) -> Option<Retained<NSScrollView>> {
        if let Some(tv) = self.text_view.borrow().as_ref() {
            return tv.enclosingScrollView();
        }
        if let Some(p) = self.pdf.borrow().as_ref() {
            let doc: Option<Retained<NSView>> = unsafe { msg_send![&**p, documentView] };
            return doc.and_then(|d| d.enclosingScrollView());
        }
        None
    }

    /// Where the content is scrolled to now (`None`: nothing scrolls).
    pub fn scroll_offset(&self) -> Option<CGPoint> {
        self.scroll_view().map(|sv| sv.contentView().bounds().origin)
    }

    /// The place a rebuilt viewer still has to restore (`None` once applied).
    pub fn restore_offset(&self) -> Option<CGPoint> {
        self.restore_offset.get()
    }

    /// A scroll position to restore once the content is laid out (set before `make_view` on a rebuilt viewer).
    pub fn set_restore_offset(&self, p: Option<CGPoint>) {
        self.restore_offset.set(p);
        self.apply_restore();
    }

    /// Scroll to the restore offset, clamped to the new length, once the document has a height.
    fn apply_restore(&self) {
        let Some(o) = self.restore_offset.get() else { return };
        let Some(sv) = self.scroll_view() else { return };
        let Some(doc) = sv.documentView() else { return };
        let tv = self.text_view.borrow().clone();
        if let Some(tv) = &tv {
            if unsafe { tv.textStorage() }.map(|s| s.length()).unwrap_or(0) == 0 {
                return;
            }
        }
        if let Some(p) = self.pdf.borrow().as_ref() {
            let d: Option<Retained<AnyObject>> = unsafe { msg_send![&**p, document] };
            if d.is_none() {
                return;
            }
        }
        if let Some(tv) = &tv {
            // The text's full height first: clamping against a half-laid-out document would lose the place.
            if sv.contentView().bounds().size.width <= 0.0 {
                return;
            }
            unsafe {
                if let (Some(lm), Some(tc)) = (tv.layoutManager(), tv.textContainer()) {
                    lm.ensureLayoutForTextContainer(&tc);
                }
            }
            tv.sizeToFit();
        }
        doc.layoutSubtreeIfNeeded();
        let clip = sv.contentView();
        let max_y = (doc.frame().size.height - clip.bounds().size.height).max(0.0);
        clip.scrollToPoint(CGPoint::new(0.0, o.y.max(0.0).min(max_y)));
        sv.reflectScrolledClipView(&clip);
        self.restore_offset.set(None);
    }

    // MARK: Lab hooks

    pub fn action_row(&self) -> Option<Retained<ActionRow>> {
        self.action_row.borrow().clone()
    }
    pub fn audio_player(&self) -> Option<Retained<AudioPlayerView>> {
        self.audio.borrow().clone()
    }
    pub fn has_text(&self) -> bool {
        self.loaded_text.borrow().is_some()
    }
    /// The text view's string, its first `n` characters (lab checks of a reload).
    pub fn text_prefix(&self, n: usize) -> Option<String> {
        let tv = self.text_view.borrow().clone()?;
        Some(tv.string().to_string().chars().take(n).collect())
    }
    /// Whether the root holds a scroll view (text and markdown render in one).
    pub fn has_scroll_view(&self) -> bool {
        let Some(r) = self.root.borrow().clone() else { return false };
        let cls = objc2::class!(NSScrollView);
        r.subviews().iter().any(|v| unsafe { msg_send![&*v, isKindOfClass: cls] })
    }
    pub fn web_view(&self) -> Option<Retained<NSView>> {
        self.web.borrow().clone()
    }
    pub fn is_web_loaded(&self) -> bool {
        match self.web.borrow().as_ref() {
            Some(w) => !unsafe { msg_send![&**w, isLoading] },
            None => true,
        }
    }
    pub fn video_view(&self) -> Option<Retained<NSView>> {
        self.video.borrow().clone()
    }
    pub fn poster_image(&self) -> Option<Image> {
        self.poster.borrow().clone()
    }
    pub fn content_view(&self) -> Option<Retained<NSView>> {
        self.content.borrow().clone()
    }
}

fn set_text(tv: &NSTextView, t: &NSAttributedString) {
    if let Some(s) = unsafe { tv.textStorage() } {
        s.setAttributedString(t);
    }
}

fn message_view(title: &str, detail: &str) -> Retained<NSView> {
    let mtm = mtm();
    let v = FlippedView::new(CGRect::ZERO);
    let t = NSTextField::labelWithString(&ns(title), mtm);
    t.setFont(Some(&look::body_strong()));
    t.setTextColor(Some(&look::ink()));
    let d = NSTextField::labelWithString(&ns(detail), mtm);
    d.setFont(Some(&look::small_font()));
    d.setTextColor(Some(&look::ink2()));
    d.setLineBreakMode(NSLineBreakMode::ByTruncatingMiddle);
    t.setFrame(cgr(Rect::new(0.0, 0.0, 280.0, 20.0)));
    d.setFrame(cgr(Rect::new(0.0, 22.0, 280.0, 18.0)));
    d.setAutoresizingMask(objc2_app_kit::NSAutoresizingMaskOptions::ViewWidthSizable);
    t.setAutoresizingMask(objc2_app_kit::NSAutoresizingMaskOptions::ViewWidthSizable);
    v.addSubview(&t);
    v.addSubview(&d);
    Retained::into_super(v)
}

/// The first page's top, whatever scale or frame PDFView settled on.
fn to_top(v: &NSView) {
    unsafe {
        let _: () = msg_send![v, layoutDocumentView];
        let doc: Option<Retained<AnyObject>> = msg_send![v, document];
        let Some(doc) = doc else { return };
        let page: Option<Retained<AnyObject>> = msg_send![&*doc, pageAtIndex: 0usize];
        let Some(page) = page else { return };
        let box_kind: isize = msg_send![v, displayBox];
        let b: CGRect = msg_send![&*page, boundsForBox: box_kind];
        let r = CGRect::new(CGPoint::new(b.origin.x, b.origin.y + b.size.height - 1.0), CGSize::new(1.0, 1.0));
        let _: () = msg_send![v, goToRect: r, onPage: &*page];
    }
}

/// Page sizes from the PDF's page dictionaries (no rendering).
pub fn pdf_page_sizes(path: &Path, limit: usize) -> Vec<CGSize> {
    let url = file_url(path);
    // SAFETY: CoreGraphics reads; the document is owned and released.
    unsafe {
        let Some(doc) = Cf::new(CGPDFDocumentCreateWithURL(&*url as *const NSURL as Ref)) else { return vec![] };
        let n = CGPDFDocumentGetNumberOfPages(doc.raw()).min(limit);
        (1..=n)
            .filter_map(|i| {
                let p = CGPDFDocumentGetPage(doc.raw(), i);
                if p.is_null() {
                    return None;
                }
                let b = CGPDFPageGetBoxRect(p, CROP_BOX);
                Some(if CGPDFPageGetRotationAngle(p) % 180 == 0 { b.size } else { CGSize::new(b.size.height, b.size.width) })
            })
            .collect()
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Affine {
    a: f64,
    b: f64,
    c: f64,
    d: f64,
    tx: f64,
    ty: f64,
}

unsafe impl objc2::Encode for Affine {
    const ENCODING: objc2::Encoding = objc2::Encoding::Struct(
        "CGAffineTransform",
        &[f64::ENCODING, f64::ENCODING, f64::ENCODING, f64::ENCODING, f64::ENCODING, f64::ENCODING],
    );
}

/// Height over width of the first video track, as shown (its transform applied).
pub fn video_aspect(path: &Path) -> Option<f64> {
    objc2::rc::autoreleasepool(|_| unsafe {
        let asset_cls = AnyClass::get(c"AVURLAsset")?;
        let asset: Retained<AnyObject> = msg_send![asset_cls, URLAssetWithURL: &*file_url(path), options: std::ptr::null::<AnyObject>()];
        let tracks: Retained<objc2_foundation::NSArray<AnyObject>> = msg_send![&*asset, tracksWithMediaType: objc2_av_foundation::AVMediaTypeVideo?];
        let t = tracks.firstObject()?;
        let s: CGSize = msg_send![&*t, naturalSize];
        let m: Affine = msg_send![&*t, preferredTransform];
        let (w, h) = ((m.a * s.width + m.c * s.height).abs(), (m.b * s.width + m.d * s.height).abs());
        (w > 0.0).then(|| h / w)
    })
}

/// The first frame of a video, as shown (its transform applied), at most 1800 px.
pub fn video_poster(path: &Path) -> Option<Image> {
    objc2::rc::autoreleasepool(|_| unsafe {
        let asset_cls = AnyClass::get(c"AVURLAsset")?;
        let asset: Retained<AnyObject> = msg_send![asset_cls, URLAssetWithURL: &*file_url(path), options: std::ptr::null::<AnyObject>()];
        let g: Retained<AnyObject> = msg_send![AnyClass::get(c"AVAssetImageGenerator")?, assetImageGeneratorWithAsset: &*asset];
        let _: () = msg_send![&*g, setAppliesPreferredTrackTransform: true];
        let _: () = msg_send![&*g, setMaximumSize: CGSize::new(1800.0, 1800.0)];
        let zero = objc2_core_media::kCMTimeZero;
        let img: *mut CGImage = msg_send![&*g, copyCGImageAtTime: zero, actualTime: std::ptr::null_mut::<objc2_core_media::CMTime>(), error: std::ptr::null_mut::<*mut AnyObject>()];
        Cf::new(img as Ref).map(Image)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Review, 30 Oct: a page in the viewer stays local. Its navigations stay on files; every network load is blocked.
    #[test]
    fn a_viewer_page_stays_local() {
        for ok in ["file:///Users/x/report.html", "file:///Users/x/other.html#a", "about:blank"] { assert!(web_navigation_allowed(ok), "{ok}") }
        for no in ["https://evil.example/?d=x", "http://127.0.0.1:8765/", "data:text/html,x", "javascript:alert(1)", "ftp://x", "blob:file:///1"] {
            assert!(!web_navigation_allowed(no), "{no}");
        }
        let rules: serde_json::Value = serde_json::from_str(OFFLINE_RULES).unwrap();
        let filters: Vec<String> = rules.as_array().unwrap().iter().map(|r| { assert_eq!(r["action"]["type"], "block"); r["trigger"]["url-filter"].as_str().unwrap().to_owned() }).collect();
        for f in &filters { assert!(!f.contains('|') && !f.contains("file"), "WebKit takes no alternation; files still load: {f}") }
        for url in ["https://x", "http://x", "wss://x", "ws://x", "ftp://x"] {
            assert!(filters.iter().any(|f| url.starts_with(&f.replace("^", "").replace("s?", "s")) || url.starts_with(&f.replace("^", "").replace("s?", ""))), "{url} blocked");
        }
    }

    /// Bug note 2026-11-04T00-46-09: a report under a trusted folder shows its HTTPS pictures and video; every other
    /// load stays blocked for it, and a page elsewhere stays wholly offline.
    #[test]
    fn a_trusted_page_shows_https_pictures_and_video_and_nothing_else() {
        let rules: serde_json::Value = serde_json::from_str(&web_rules(&[PathBuf::from("/Users/x/My Work")])).unwrap();
        let rules = rules.as_array().unwrap();
        let offline: serde_json::Value = serde_json::from_str(OFFLINE_RULES).unwrap();
        assert_eq!(&rules[..3], offline.as_array().unwrap().as_slice(), "the offline rules come first, unchanged");
        let allow = rules.last().unwrap();
        assert_eq!(allow["action"]["type"], "ignore-previous-rules");
        assert_eq!(allow["trigger"]["url-filter"], "^https://", "https only: plain http, sockets and ftp stay blocked");
        assert_eq!(allow["trigger"]["resource-type"], serde_json::json!(["image", "media"]), "no scripts, fetch, XHR, frames or pings");
        let tops: Vec<&str> = allow["trigger"]["if-top-url"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
        assert_eq!(tops, ["^file:///Users/x/My%20Work/"], "a missing folder has no canonical twin; both encodings agree");
        // Nothing trusted: the offline rules alone.
        assert_eq!(serde_json::from_str::<serde_json::Value>(&web_rules(&[])).unwrap(), offline);
    }

    /// A folder that would trust too much is never trusted: `/`, the home folder or above it, Downloads, scratch.
    #[test]
    fn only_report_folders_are_trusted_with_remote_media() {
        let home = Path::new("/Users/x");
        for ok in ["/Users/x/Work/Assistant/Research", "/Users/x/Work/Reports", "/Users/x/Library/Application Support/FloatingIslands/docs"] {
            assert!(trusted_root(Path::new(ok), home), "{ok}");
        }
        for no in ["/", "/Users", "/Users/x", "/Users/x/", "/Users/x/Downloads", "/Users/x/Downloads/r", "/Users/x/Work/Assistant/Temp/a", "/tmp/r", "/private/var/folders/r", "relative/r"] {
            assert!(!trusted_root(Path::new(no), home), "{no}");
        }
    }

    #[test]
    fn a_top_url_pattern_matches_only_pages_under_its_folder() {
        assert_eq!(top_url_patterns(Path::new("/Users/x/Work/")), ["^file:///Users/x/Work/"]);
        assert_eq!(top_url_patterns(Path::new("/Users/x/Library/Application Support/Floating.Islands")), ["^file:///Users/x/Library/Application%20Support/Floating\\.Islands/"]);
        // Foundation leaves `(`, `)` and `+` in a file URL; the strict form encodes them. Regex characters are escaped.
        assert_eq!(top_url_patterns(Path::new("/a/(b)+c")), ["^file:///a/\\(b\\)\\+c/", "^file:///a/%28b%29%2Bc/"]);
        // A symlinked root (/tmp on macOS) is matched both as given and resolved.
        let dir = std::env::temp_dir().join(format!("fi-web-roots-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let rules: serde_json::Value = serde_json::from_str(&web_rules(std::slice::from_ref(&dir))).unwrap();
        let tops = rules.as_array().unwrap().last().unwrap()["trigger"]["if-top-url"].as_array().unwrap().len();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(tops >= 1);
    }

    #[test]
    fn stamps_tell_versions_apart() {
        let dir = std::env::temp_dir().join(format!("fi-viewer-stamp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("a.md");
        assert_eq!(stamp(&p), None);
        std::fs::write(&p, "one").unwrap();
        let a = stamp(&p).unwrap();
        // Same size, replaced by rename (as editors save): a new inode.
        let tmp = dir.join("a.md.tmp");
        std::fs::write(&tmp, "two").unwrap();
        std::fs::rename(&tmp, &p).unwrap();
        let b = stamp(&p).unwrap();
        assert_ne!(a, b);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn paths_expand_and_standardize() {
        let home = std::env::var("HOME").unwrap();
        assert_eq!(viewer_path("~/x/../y.md"), PathBuf::from(format!("{home}/y.md")));
        assert_eq!(viewer_path("file:///tmp/a%20b.md"), PathBuf::from("/tmp/a b.md"));
        // As Swift's standardizedFileURL: an existing /private path loses the prefix.
        assert_eq!(viewer_path("/private/tmp"), PathBuf::from("/tmp"));
    }

    #[test]
    fn symbols_per_kind() {
        assert_eq!(FileViewer::symbol(&ViewerKind::Markdown), "doc.richtext");
        assert_eq!(FileViewer::symbol(&ViewerKind::Missing), "questionmark.folder");
        assert_eq!(FileViewer::body_rect(Size::new(400.0, 200.0)), Rect::new(16.0, 48.0, 368.0, 136.0));
    }
}
