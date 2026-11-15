//! Offscreen checks of a page's own pinch (`model::page_pinch`; and 17:03: "it should be the
//! same as the browser if its the same its perfect"), in the viewer's real web view, in an island panel that is
//! never ordered in:
//!   FLOATING_ISLANDS_DIR=<lab dir> FloatingIslands --viewer-pinch-shots <out-dir> <page.html> <sequences.json>
//! Trackpad magnify events are made in code (CGEvent gesture fields, as the window server sends them) and go through
//! the view under the point (as NSWindow routes a gesture), never the real trackpad or cursor. The CV's state is read
//! through a lab-only script in every frame (`PROBE_JS`, the page reloaded with it), since the hub frames the page
//! from another (file) origin.
//!
//! `sequences.json`: `[{name, fx, fy, steps: [magnification, …], to?: [fx, fy], pan?: [dy, …], burst?: bool}]`; the
//! pointer starts at (`fx`, `fy`) of the CV panel's rect and, with `to`, moves there in even steps. Each sequence starts
//! at the fit (a double-tap); a begin, one change per step, an end. Per step (except `burst`, which sends the whole
//! gesture in one run-loop turn, so the viewer holds events while the page answers) it writes the CV's zoom and
//! scroll after the step to `pinch.json`, for `lab/pinch_parity.py` to compare against Chromium on the same page.
//! Also: a page that says nothing, a page whose hook throws and one whose hook is missing still magnify whole;
//! the page's own zoom and the remembered zoom never move. Renders before.png (fit) and after.png (zoomed).

use std::cell::RefCell;
use std::ffi::c_void;
use std::path::PathBuf;
use std::rc::Rc;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{class, msg_send, MainThreadMarker};
use objc2_app_kit::{NSAppearanceCustomization, NSEvent, NSScreen, NSView};
use objc2_core_foundation::CGPoint;
use serde_json::{json, Value};

use super::draw;
use super::look::{cgr, ns};
use super::panel::IslandPanel;
use super::sys;
use super::viewer::{FileViewer, FlippedView};
use super::viewer_shots::finish;
use crate::model::geometry::{Rect, Size};
use crate::model::zoom;

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventCreate(source: *const c_void) -> *mut c_void;
    fn CGEventCreateScrollWheelEvent2(source: *const c_void, units: u32, count: u32, w1: i32, w2: i32, w3: i32) -> *mut c_void;
    fn CGEventSetType(e: *mut c_void, t: u32);
    fn CGEventSetIntegerValueField(e: *mut c_void, field: u32, v: i64);
    fn CGEventSetDoubleValueField(e: *mut c_void, field: u32, v: f64);
    fn CGEventSetLocation(e: *mut c_void, p: CGPoint);
    fn CFRelease(r: *mut c_void);
}

const SCREEN: Size = Size::new(1728.0, 1117.0);
const ROOM: Size = Size::new(1712.0, 1117.0 - 38.0 - 6.0 - 8.0);
const AT: f64 = 8.0;
/// Between per-step events: about one frame at 120 Hz, then the state is read.
const STEP: f64 = 0.012;

/// Lab only, in every frame: a framed review page answers `{pinchProbe}` with its CV's state.
const PROBE_JS: &str = "if(top!==self)addEventListener('message',e=>{if(!(e.data&&e.data.pinchProbe))return;const cv=document.querySelector('.cv');\
if(!cv||!window.layoutFacts)return;const r=cv.getBoundingClientRect();\
parent.postMessage({pinchState:{z:layoutFacts().zoom,sl:cv.scrollLeft,st:cv.scrollTop,cv:{l:r.left,t:r.top,w:r.width,h:r.height}}},'*')});";

/// The CV's state (zoom, scroll, its rect in the top page's coordinates), through the frame (`PROBE_JS`) or the page.
const STATE_JS: &str = "try{const fr=document.querySelector('iframe');let s;\
if(fr){s=await new Promise((res,rej)=>{const t=setTimeout(()=>rej('no answer from the frame'),1000);\
addEventListener('message',function h(e){if(e.data&&e.data.pinchState){removeEventListener('message',h);clearTimeout(t);res(e.data.pinchState)}});\
fr.contentWindow.postMessage({pinchProbe:1},'*')});const o=fr.getBoundingClientRect();s.cv.l+=o.left;s.cv.t+=o.top}\
else{const cv=document.querySelector('.cv'),r=cv.getBoundingClientRect();s={z:layoutFacts().zoom,sl:cv.scrollLeft,st:cv.scrollTop,cv:{l:r.left,t:r.top,w:r.width,h:r.height}}}\
s.vw=innerWidth;s.vh=innerHeight;s.vv=window.visualViewport?visualViewport.scale:1;return JSON.stringify(s)}catch(e){return JSON.stringify({error:String(e)})}";

struct Run {
    out: PathBuf,
    panel: Retained<IslandPanel>,
    host: Retained<FlippedView>,
    lines: RefCell<Vec<String>>,
    results: RefCell<Vec<Value>>,
}

impl Run {
    fn check(&self, name: &str, ok: bool, detail: String) {
        self.lines.borrow_mut().push(format!("{} {name}{}", if ok { "ok  " } else { "FAIL" }, if detail.is_empty() { String::new() } else { format!(" ({detail})") }));
    }

    fn show(&self, v: &Rc<FileViewer>) -> Size {
        let min = Size::new(320.0, 200.0);
        let size = crate::model::viewer_size::asked_island(v.island_size(SCREEN), v.page_ask(), min, ROOM);
        for s in self.host.subviews().iter() {
            s.removeFromSuperview();
        }
        let body = FileViewer::body_rect(size);
        let root = v.make_view(body.size());
        root.setFrame(cgr(Rect::new(AT, AT, body.w, body.h)));
        self.host.addSubview(&root);
        root.layoutSubtreeIfNeeded();
        v.layout(body.size());
        size
    }

    /// Window point (y up) of a point in the web view (from its top-left).
    fn window_point(&self, web: &NSView, x: f64, y: f64) -> CGPoint {
        let yy = if web.isFlipped() { y } else { web.bounds().size.height - y };
        web.convertPoint_toView(CGPoint::new(x, yy), None)
    }

    fn screen_cg(&self, p: CGPoint) -> CGPoint {
        let main_h = NSScreen::screens(MainThreadMarker::new().unwrap()).iter().next().map(|s| s.frame().size.height).unwrap_or(1117.0);
        let s = self.panel.convertPointToScreen(p);
        CGPoint::new(s.x, main_h - s.y)
    }

    /// One trackpad magnify event (phase: 1 began, 2 changed, 4 ended) at window point `p`, to the view under it.
    fn magnify(&self, p: CGPoint, m: f64, phase: i64) {
        let e = unsafe { CGEventCreate(std::ptr::null()) };
        if e.is_null() {
            return;
        }
        unsafe {
            CGEventSetType(e, 29); // a gesture
            CGEventSetIntegerValueField(e, 110, 8); // its HID type: zoom (a magnify)
            CGEventSetDoubleValueField(e, 113, m); // the magnification
            CGEventSetIntegerValueField(e, 132, phase);
            CGEventSetLocation(e, self.screen_cg(p));
        }
        let ns_e: Option<Retained<NSEvent>> = unsafe { msg_send![class!(NSEvent), eventWithCGEvent: e as *mut objc2_core_graphics::CGEvent] };
        unsafe { CFRelease(e) };
        let Some(ev) = ns_e else { return };
        // To the view under the point, as NSWindow routes a gesture (a window never ordered in routes none itself);
        // WebKit's inner views pass it up the responder chain to the web view.
        if let Some(t) = self.hit(p) {
            let _: () = unsafe { msg_send![&*t, magnifyWithEvent: &*ev] };
        }
    }

    fn hit(&self, p: CGPoint) -> Option<Retained<NSView>> {
        let content = self.panel.contentView()?;
        let frame = unsafe { content.superview() }?;
        content.hitTest(frame.convertPoint_fromView(p, None))
    }

    /// A two-finger double-tap at window point `p`: to the web view's `smartMagnifyWithEvent:` (AppKit's own route
    /// for it is the view under the pointer; a synthetic one is not routed by a window that was never ordered in).
    fn smart(&self, web: &NSView, p: CGPoint) {
        let e = unsafe { CGEventCreate(std::ptr::null()) };
        if e.is_null() {
            return;
        }
        unsafe {
            CGEventSetType(e, 29);
            CGEventSetIntegerValueField(e, 110, 8);
            CGEventSetLocation(e, self.screen_cg(p));
        }
        let ns_e: Option<Retained<NSEvent>> = unsafe { msg_send![class!(NSEvent), eventWithCGEvent: e as *mut objc2_core_graphics::CGEvent] };
        unsafe { CFRelease(e) };
        if let Some(ev) = ns_e {
            let _: () = unsafe { msg_send![web, smartMagnifyWithEvent: &*ev] };
        }
    }

    /// A two-finger scroll of `dy` points at window point `p`, to the view under it (as AppKit routes it).
    fn pan(&self, p: CGPoint, dy: f64) {
        let e = unsafe { CGEventCreateScrollWheelEvent2(std::ptr::null(), 0, 1, dy as i32, 0, 0) };
        if e.is_null() {
            return;
        }
        unsafe {
            CGEventSetIntegerValueField(e, 88, 1);
            CGEventSetDoubleValueField(e, 96, dy);
            CGEventSetLocation(e, self.screen_cg(p));
        }
        let ns_e: Option<Retained<NSEvent>> = unsafe { msg_send![class!(NSEvent), eventWithCGEvent: e as *mut objc2_core_graphics::CGEvent] };
        unsafe { CFRelease(e) };
        let Some(ev) = ns_e else { return };
        if let Some(t) = self.hit(p) {
            let _: () = unsafe { msg_send![&*t, scrollWheel: &*ev] };
        }
    }

    fn done(&self) -> ! {
        let _ = std::fs::write(self.out.join("pinch.json"), serde_json::to_string_pretty(&*self.results.borrow()).unwrap_or_default());
        let text = self.lines.borrow().join("\n") + "\n";
        let _ = std::fs::write(self.out.join("report.txt"), &text);
        print!("{text}");
        std::process::exit(if text.contains("FAIL") { 1 } else { 0 });
    }
}

fn state(web: &NSView, done: impl FnOnce(Value) + 'static) {
    let done = RefCell::new(Some(done));
    let block = block2::RcBlock::new(move |v: *mut AnyObject, e: *mut AnyObject| {
        let s = if v.is_null() {
            if e.is_null() {
                "null".to_owned()
            } else {
                let d: Retained<objc2_foundation::NSString> = unsafe { msg_send![&*e, localizedDescription] };
                format!("{{\"error\":{}}}", json!(d.to_string()))
            }
        } else {
            let d: Retained<objc2_foundation::NSString> = unsafe { msg_send![&*v, description] };
            d.to_string()
        };
        if let Some(f) = done.borrow_mut().take() {
            f(serde_json::from_str(&s).unwrap_or(json!({"error": s})));
        }
    });
    let world: Retained<AnyObject> = unsafe { msg_send![class!(WKContentWorld), pageWorld] };
    let args = objc2_foundation::NSDictionary::<objc2_foundation::NSString, AnyObject>::new();
    unsafe {
        let _: () = msg_send![web, callAsyncJavaScript: &*ns(STATE_JS), arguments: &*args, inFrame: std::ptr::null::<AnyObject>(), inContentWorld: &*world, completionHandler: &*block];
    }
}

/// Adds `PROBE_JS` to every frame and reloads the page.
fn add_probe(web: &NSView) {
    unsafe {
        let cfg: Retained<AnyObject> = msg_send![web, configuration];
        let ucc: Retained<AnyObject> = msg_send![&*cfg, userContentController];
        let us: Retained<AnyObject> = msg_send![msg_send![class!(WKUserScript), alloc], initWithSource: &*ns(PROBE_JS), injectionTime: 1isize, forMainFrameOnly: false];
        let _: () = msg_send![&*ucc, addUserScript: &*us];
        let _: Option<Retained<AnyObject>> = msg_send![web, reload];
    }
}

/// The web view's page zoom × magnification (the viewer's own zoom of the page).
fn view_zoom(web: &NSView) -> (f64, f64) {
    unsafe { (msg_send![web, pageZoom], msg_send![web, magnification]) }
}

pub fn run(out: &str, page: &str, seqs: &str) -> ! {
    let mtm = MainThreadMarker::new().expect("main thread");
    let out = PathBuf::from(out);
    let _ = std::fs::create_dir_all(&out);
    let _ = std::fs::remove_file(zoom::Memory::default_path());
    let panel = IslandPanel::make(mtm);
    panel.setFrame_display(cgr(Rect::new(0.0, 0.0, 1720.0, 1110.0)), false);
    if let Some(a) = objc2_app_kit::NSAppearance::appearanceNamed(unsafe { objc2_app_kit::NSAppearanceNameDarkAqua }) {
        panel.setAppearance(Some(&a));
    }
    let host = FlippedView::new(cgr(Rect::new(0.0, 0.0, 1720.0, 1110.0)));
    panel.setContentView(Some(&host));
    let run = Rc::new(Run { out, panel, host, lines: RefCell::new(vec![]), results: RefCell::new(vec![]) });
    let seqs: Vec<Value> = std::fs::read_to_string(seqs).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
    let page = page.to_owned();
    let r = run.clone();
    sys::soon(move || open_page(r, page, seqs));
    objc2_app_kit::NSApplication::sharedApplication(mtm).run();
    std::process::exit(0)
}

fn open_page(run: Rc<Run>, page: String, seqs: Vec<Value>) {
    let v = FileViewer::new(&page, vec![]);
    let size = run.show(&v);
    let r = run.clone();
    sys::after(3.0, move || {
        let run = r;
        let Some(web) = v.web_view() else {
            run.check("the page has a web view", false, String::new());
            run.done();
        };
        run.check("the page says pinch=page", v.page_takes_pinch(), String::new());
        add_probe(&web);
        let r = run.clone();
        sys::after(2.5, move || opened(r, v, web, size, seqs));
    });
}

fn opened(run: Rc<Run>, v: Rc<FileViewer>, web: Retained<NSView>, size: Size, seqs: Vec<Value>) {
    {
        let b = web.bounds().size;
        run.results.borrow_mut().push(json!({"web": {"w": b.width, "h": b.height}}));
        let body = FileViewer::body_rect(size);
        let (r2, v2, w2) = (run.clone(), v.clone(), web.clone());
        finish(&v, "Applications", size, body, Box::new(move |img| {
            let wrote = img.as_ref().is_some_and(|i| draw::write_png(i, &r2.out.join("before.png")));
            r2.check("render before.png (fit)", wrote, String::new());
            state(&w2.clone(), move |s0| {
                r2.check("the CV is readable through the page", s0.get("error").is_none(), s0.to_string());
                let vz0 = view_zoom(&w2);
                sequence(r2, v2, w2, seqs, 0, vz0);
            });
        }));
    }
}

/// The pointer's web-view point for fractions of the CV's rect (`cv` from `STATE_JS`).
fn cv_point(cv: &Value, fx: f64, fy: f64) -> (f64, f64) {
    let g = |k: &str| cv.get(k).and_then(|x| x.as_f64()).unwrap_or(0.0);
    (g("l") + fx * g("w"), g("t") + fy * g("h"))
}

fn sequence(run: Rc<Run>, v: Rc<FileViewer>, web: Retained<NSView>, seqs: Vec<Value>, i: usize, vz0: (f64, f64)) {
    if i >= seqs.len() {
        return after_sequences(run, v, web, vz0);
    }
    let seq = seqs[i].clone();
    // Back to the fit: a double-tap at the CV's middle (the page's own reset).
    state(&web.clone(), move |s| {
        let cv = s["cv"].clone();
        let (mx, my) = cv_point(&cv, 0.5, 0.5);
        let was = s["z"].as_f64().unwrap_or(1.0);
        run.smart(&web, run.window_point(&web, mx, my));
        {
            sys::after(0.15, move || {
                state(&web.clone(), move |s| {
                    let z = s["z"].as_f64().unwrap_or(0.0);
                    let vm = view_zoom(&web).1;
                    run.check(&format!("{}: a two-finger double-tap returns the CV to fit (from {was:.3}), the page unmoved", seq["name"].as_str().unwrap_or("?")), (z - 1.0).abs() < 1e-6 && (vm - 1.0).abs() < 1e-6, format!("zoom {z}, magnification {vm}"));
                    let steps: Vec<f64> = seq["steps"].as_array().map(|a| a.iter().filter_map(|x| x.as_f64()).collect()).unwrap_or_default();
                    let (fx, fy) = (seq["fx"].as_f64().unwrap_or(0.5), seq["fy"].as_f64().unwrap_or(0.5));
                    let to = seq["to"].as_array().map(|a| (a[0].as_f64().unwrap_or(fx), a[1].as_f64().unwrap_or(fy)));
                    let n = steps.len().max(1) as f64;
                    let pts: Vec<(f64, f64)> = (0..=steps.len())
                        .map(|k| {
                            let t = k as f64 / n;
                            let (tx, ty) = to.unwrap_or((fx, fy));
                            cv_point(&s["cv"], fx + (tx - fx) * t, fy + (ty - fy) * t)
                        })
                        .collect();
                    let rec = Rc::new(RefCell::new(json!({"name": seq["name"], "start": s, "points": pts, "after": [], "pans": []})));
                    if seq["burst"].as_bool() == Some(true) {
                        let wp = |k: usize| run.window_point(&web, pts[k].0, pts[k].1);
                        run.magnify(wp(0), 0.0, 1);
                        for (k, m) in steps.iter().enumerate() {
                            run.magnify(wp(k + 1), *m, 2);
                        }
                        run.magnify(wp(steps.len()), 0.0, 4);
                        sys::after(0.3, move || {
                            state(&web.clone(), move |s| {
                                rec.borrow_mut()["after"].as_array_mut().unwrap().push(s);
                                run.results.borrow_mut().push(rec.borrow().clone());
                                sequence(run, v, web, seqs, i + 1, vz0);
                            });
                        });
                        return;
                    }
                    let p0 = run.window_point(&web, pts[0].0, pts[0].1);
                    run.magnify(p0, 0.0, 1);
                    let pans: Vec<f64> = seq["pan"].as_array().map(|a| a.iter().filter_map(|x| x.as_f64()).collect()).unwrap_or_default();
                    sys::after(STEP, move || step(run, v, web, seqs, i, vz0, steps, pts, pans, rec, 0));
                });
            });
        }
    });
}

#[allow(clippy::too_many_arguments)]
fn step(
    run: Rc<Run>,
    v: Rc<FileViewer>,
    web: Retained<NSView>,
    seqs: Vec<Value>,
    i: usize,
    vz0: (f64, f64),
    steps: Vec<f64>,
    pts: Vec<(f64, f64)>,
    pans: Vec<f64>,
    rec: Rc<RefCell<Value>>,
    k: usize,
) {
    if k < steps.len() {
        let p = run.window_point(&web, pts[k + 1].0, pts[k + 1].1);
        run.magnify(p, steps[k], 2);
        sys::after(STEP, move || {
            let w = web.clone();
            state(&w, move |s| {
                rec.borrow_mut()["after"].as_array_mut().unwrap().push(s);
                step(run, v, web, seqs, i, vz0, steps, pts, pans, rec, k + 1);
            });
        });
        return;
    }
    if k == steps.len() {
        let p = run.window_point(&web, pts[k].0, pts[k].1);
        run.magnify(p, 0.0, 4);
    }
    // Then the pans (two fingers, zoomed), one state each.
    let j = k - steps.len();
    if j < pans.len() {
        let p = run.window_point(&web, pts[steps.len()].0, pts[steps.len()].1);
        run.pan(p, pans[j]);
        sys::after(0.05, move || {
            let w = web.clone();
            state(&w, move |s| {
                if let Some(a) = rec.borrow_mut()["pans"].as_array_mut() {
                    a.push(s);
                }
                step(run, v, web, seqs, i, vz0, steps, pts, pans, rec, k + 1);
            });
        });
        return;
    }
    sys::after(0.05, move || {
        let w = web.clone();
        state(&w, move |s| {
            rec.borrow_mut()["end"] = s;
            let vz = view_zoom(&web);
            rec.borrow_mut()["view_zoom"] = json!([vz.0, vz.1]);
            run.results.borrow_mut().push(rec.borrow().clone());
            sequence(run, v, web, seqs, i + 1, vz0);
        });
    });
}

fn after_sequences(run: Rc<Run>, v: Rc<FileViewer>, web: Retained<NSView>, vz0: (f64, f64)) {
    let vz = view_zoom(&web);
    let moved = (vz.0 - vz0.0).abs() > 1e-6 || (vz.1 - vz0.1).abs() > 1e-6;
    run.check("the viewer's own zoom of the page never moved (page zoom, magnification)", !moved, format!("{vz0:?} -> {vz:?}"));
    let zoom_file = zoom::Memory::default_path();
    let remembered = std::fs::read_to_string(&zoom_file).unwrap_or_default();
    run.check("the remembered zoom was not written", !remembered.contains(&*v.path().to_string_lossy()), remembered.chars().take(120).collect());
    // An `after.png`: zoomed in on the CV.
    let r = run.clone();
    state(&web.clone(), move |s| {
        let z0 = s["z"].as_f64().unwrap_or(0.0);
        let (x, y) = cv_point(&s["cv"], 0.3, 0.15);
        let p = r.window_point(&web, x, y);
        r.magnify(p, 0.0, 1);
        for _ in 0..6 {
            r.magnify(p, 0.2, 2);
        }
        r.magnify(p, 0.0, 4);
        sys::after(0.5, move || {
            state(&web.clone(), move |s| {
                let z = s["z"].as_f64().unwrap_or(0.0);
                let want = (z0 * 1.2f64.powi(6)).min(6.0);
                r.check("a pinch over the CV zooms the CV (burst of six +0.2)", (z - want).abs() < 1e-3, format!("zoom {z0:.4} -> {z:.4}, expected {want:.4}"));
                let vz = view_zoom(&web);
                r.check("... and not the page", (vz.1 - 1.0).abs() < 1e-6, format!("magnification {:.4}", vz.1));
                let size = r.show_size(&v);
                let body = FileViewer::body_rect(size);
                let (r2, v2) = (r.clone(), v.clone());
                finish(&v, "Applications", size, body, Box::new(move |img| {
                    let wrote = img.as_ref().is_some_and(|i| draw::write_png(i, &r2.out.join("after.png")));
                    r2.check("render after.png (CV zoomed)", wrote, String::new());
                    others(r2, v2, web, 0);
                }));
            });
        });
    });
}

impl Run {
    fn show_size(&self, v: &FileViewer) -> Size {
        crate::model::viewer_size::asked_island(v.island_size(SCREEN), v.page_ask(), Size::new(320.0, 200.0), ROOM)
    }
}

/// Pages that keep today's pinch: (name, html). Each must magnify whole.
fn other_pages() -> Vec<(&'static str, String)> {
    let body = "<body style='background:#111;color:#eee;font:15px -apple-system;height:900px'><p>Some text.</p></body>";
    vec![
        ("a page that says nothing magnifies whole", format!("<!doctype html><html><head></head>{body}</html>")),
        (
            "a page whose hook throws magnifies whole",
            format!("<!doctype html><html><head><meta name='heimdall-viewer' content='pinch=page'><script>window.heimdallPinch=()=>{{throw new Error('broken')}}</script></head>{body}</html>"),
        ),
        ("a page that says pinch=page but has no hook magnifies whole", format!("<!doctype html><html><head><meta name='heimdall-viewer' content='pinch=page'></head>{body}</html>")),
        (
            "a page whose hook declines magnifies whole",
            format!("<!doctype html><html><head><meta name='heimdall-viewer' content='pinch=page'><script>window.heimdallPinch=()=>false</script></head>{body}</html>"),
        ),
    ]
}

fn others(run: Rc<Run>, prev: Rc<FileViewer>, prev_web: Retained<NSView>, i: usize) {
    let _ = (&prev, &prev_web);
    let pages = other_pages();
    if i >= pages.len() {
        run.done();
    }
    let (name, html) = pages[i].clone();
    let file = run.out.join(format!("other-{i}.html"));
    let _ = std::fs::write(&file, html);
    let v = FileViewer::new(&file.to_string_lossy(), vec![]);
    run.show(&v);
    let r = run.clone();
    sys::after(2.0, move || {
        let run = r;
        let Some(web) = v.web_view() else {
            run.check(name, false, "no web view".into());
            return others(run, v.clone(), prev_web, i + 1);
        };
        let b = web.bounds().size;
        let p = run.window_point(&web, b.width / 2.0, b.height / 2.0);
        let before = view_zoom(&web).1;
        run.magnify(p, 0.0, 1);
        sys::after(0.03, move || {
            for _ in 0..5 {
                run.magnify(p, 0.1, 2);
            }
            run.magnify(p, 0.0, 4);
            sys::after(0.4, move || {
                let after = view_zoom(&web).1;
                run.check(name, after > before * 1.3 && !v.page_has_pinch(), format!("magnification {before:.3} -> {after:.3}"));
                others(run, v.clone(), web, i + 1);
            });
        });
    });
}
