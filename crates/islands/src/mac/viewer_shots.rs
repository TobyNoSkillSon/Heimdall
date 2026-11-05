//! Offscreen renders of the viewer (Swift `ViewerShots.swift`), one PNG per file kind, at real size (2×), on a dark
//! backdrop with the island's chrome (black continuous-corner shape, hairline, symbol, title, close button). No window
//! is shown: the views live in a window that is never ordered in; WebKit content comes from its own snapshot, video
//! from its first frame.
//!   FloatingIslands --viewer-shots <out-dir> <fixtures-dir>
//! Then a refresh check (`viewer-refresh` lines in report.txt): a markdown viewer watching a lab copy of the plan,
//! rewritten in place, replaced by rename, removed and restored.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{msg_send, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{NSAppearanceCustomization, NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSImage, NSWindow, NSWindowStyleMask};
use objc2_core_foundation::{CGPoint, CGRect};
use objc2_foundation::NSArray;

use super::draw::{self, Bitmap, Cf, Image, Ref};
use super::look::{self, cgr, ns, Weight};
use super::sys;
use super::viewer::{FileViewer, FlippedView};
use crate::model::geometry::{Rect, Size};
use crate::model::{ViewerAction, ViewerKind};

struct Case {
    name: &'static str,
    title: &'static str,
    file: &'static str,
    actions: Vec<ViewerAction>,
    click: Option<usize>,
}

fn case(name: &'static str, title: &'static str, file: &'static str) -> Case {
    Case { name, title, file, actions: vec![], click: None }
}

fn plan_actions() -> Vec<ViewerAction> {
    vec![ViewerAction::new("Do all of it", "do-all"), ViewerAction::new("Only caches", "caches"), ViewerAction::new("Not now", "later")]
}

struct Run {
    out: PathBuf,
    fixtures: PathBuf,
    cases: Vec<Case>,
    next: Cell<usize>,
    host: Retained<FlippedView>,
    window: Retained<NSWindow>,
    report: RefCell<Vec<String>>,
}

const SCREEN: Size = Size::new(1728.0, 1117.0);

pub fn run_viewer_shots(out: &str, fixtures: &str) -> ! {
    let mtm = MainThreadMarker::new().expect("main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Prohibited);
    let out = PathBuf::from(out);
    let _ = std::fs::create_dir_all(&out);
    let cases = vec![
        case("markdown", "Cleanup plan", "plan.md"),
        case("code", "sizes.rs", "sizes.rs"),
        case("image", "Dusk", "dusk.png"),
        case("pdf", "Report", "report.pdf"),
        case("audio", "Melody", "melody.m4a"),
        case("video", "Clip", "clip.mp4"),
        case("html", "Latency", "page.html"),
        case("quicklook", "Letter", "letter.docx"),
        case("missing", "Old notes", "gone.md"),
        Case { actions: plan_actions(), ..case("actions", "Cleanup plan", "plan.md") },
        Case { actions: plan_actions(), click: Some(1), ..case("actions-chosen", "Cleanup plan", "plan.md") },
    ];
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            cgr(Rect::new(-30000.0, -30000.0, 1200.0, 1000.0)),
            NSWindowStyleMask::Borderless,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    unsafe { window.setReleasedWhenClosed(false) };
    if let Some(a) = objc2_app_kit::NSAppearance::appearanceNamed(unsafe { objc2_app_kit::NSAppearanceNameDarkAqua }) {
        window.setAppearance(Some(&a));
    }
    let host = FlippedView::new(cgr(Rect::new(0.0, 0.0, 1200.0, 1000.0)));
    window.setContentView(Some(&host));
    let run = Rc::new(Run {
        out,
        fixtures: PathBuf::from(fixtures),
        cases,
        next: Cell::new(0),
        host,
        window,
        report: RefCell::new(vec![]),
    });
    let r = run.clone();
    sys::soon(move || next(r));
    app.run();
    std::process::exit(0)
}

fn next(run: Rc<Run>) {
    let i = run.next.get();
    if i >= run.cases.len() {
        let r2 = run.clone();
        refresh_check(run.clone(), Box::new(move |lines| {
            r2.report.borrow_mut().extend(lines);
            events_check(r2.clone(), Box::new(move |lines| {
            run.report.borrow_mut().extend(lines);
            let run3 = run.clone();
            bridge_check(run.clone(), Box::new(move |lines| {
            let run = run3;
            run.report.borrow_mut().extend(lines);
            let text = run.report.borrow().join("\n") + "\n";
            let _ = std::fs::write(run.out.join("report.txt"), &text);
            print!("{text}");
            std::process::exit(0);
        }));
        }));
        }));
        return;
    }
    run.next.set(i + 1);
    let c = &run.cases[i];
    let v = FileViewer::new(&run.fixtures.join(c.file).to_string_lossy(), c.actions.clone());
    let clicked: Rc<RefCell<Option<String>>> = Rc::default();
    let k = clicked.clone();
    v.set_on_action(Box::new(move |a| *k.borrow_mut() = Some(a.action)));
    let resized = Rc::new(Cell::new(false));
    let rz = resized.clone();
    v.set_on_resize(Box::new(move || rz.set(true)));
    let size = v.island_size(SCREEN);
    let body = FileViewer::body_rect(size);
    for s in run.host.subviews().iter() {
        s.removeFromSuperview();
    }
    let root = v.make_view(body.size());
    root.setFrame(cgr(Rect::new(20.0, 20.0, body.w, body.h)));
    run.host.addSubview(&root);
    if let (Some(k), Some(row)) = (c.click, v.action_row()) {
        row.click(k);
        row.confirm(true);
    }
    // Let async loads land (text, PDF, waveform, poster, page load).
    let wait = if matches!(v.kind(), ViewerKind::Html | ViewerKind::Other | ViewerKind::Video | ViewerKind::Pdf) { 2.5 } else { 0.8 };
    sys::after(wait, move || {
        let c = &run.cases[i];
        let size = if resized.get() { v.island_size(SCREEN) } else { size };
        let body = FileViewer::body_rect(size);
        if resized.get() {
            root.setFrame(cgr(Rect::new(20.0, 20.0, body.w, body.h)));
            v.layout(body.size());
        }
        let r2 = run.clone();
        let v2 = v.clone();
        finish(&v, c.title, size, body, Box::new(move |img| {
            let c = &r2.cases[i];
            let png = r2.out.join(format!("viewer-{}.png", c.name));
            if let Some(img) = &img {
                draw::write_png(img, &png);
            }
            let mut line = format!("{}: kind={} island={}x{} actions={}", c.name, v2.kind().name(), size.w as i64, size.h as i64, c.actions.len());
            if let Some(a) = clicked.borrow().as_ref() {
                line += &format!(" clicked={a}");
            }
            if img.is_none() {
                line += " FAILED";
            }
            r2.report.borrow_mut().push(line);
            v2.teardown();
            next(r2);
        }));
    });
}

pub(super) fn finish(v: &Rc<FileViewer>, title: &str, size: Size, body: Rect, done: Box<dyn FnOnce(Option<Image>)>) {
    let Some(content) = v.snapshot(body.size()) else { return done(None) };
    let kind = v.kind();
    let title = title.to_owned();
    if let Some(web) = v.web_view() {
        let cfg: Retained<AnyObject> = unsafe { msg_send![objc2::class!(WKSnapshotConfiguration), new] };
        let frame = web.frame();
        unsafe {
            let _: () = msg_send![&*cfg, setRect: web.bounds()];
        }
        let done = RefCell::new(Some(done));
        let block = block2::RcBlock::new(move |img: *mut NSImage, _e: *mut AnyObject| {
            let overlay = if img.is_null() {
                None
            } else {
                let img: &NSImage = unsafe { &*img };
                let cg: *mut objc2_core_graphics::CGImage = unsafe {
                    msg_send![img, CGImageForProposedRect: std::ptr::null_mut::<CGRect>(), context: std::ptr::null::<AnyObject>(), hints: std::ptr::null::<AnyObject>()]
                };
                retain_image(cg).map(|i| (i, frame))
            };
            if let Some(d) = done.borrow_mut().take() {
                d(compose(&title, &kind, size, body, &content, overlay));
            }
        });
        unsafe {
            let _: () = msg_send![&*web, takeSnapshotWithConfiguration: &*cfg, completionHandler: &*block];
        }
        return;
    }
    done(compose(&title, &kind, size, body, &content, None))
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGImageRetain(img: Ref) -> Ref;
}

fn retain_image(cg: *mut objc2_core_graphics::CGImage) -> Option<Image> {
    if cg.is_null() {
        return None;
    }
    // SAFETY: a live image owned by its NSImage; we take our own reference.
    Cf::new(unsafe { CGImageRetain(cg as Ref) }).map(Image)
}

fn palette_symbol(name: &str, point: f64, colors: &[Retained<objc2_app_kit::NSColor>]) -> Option<Retained<NSImage>> {
    use objc2_app_kit::NSImageSymbolConfiguration;
    let img = NSImage::imageWithSystemSymbolName_accessibilityDescription(&ns(name), None)?;
    let cfg = NSImageSymbolConfiguration::configurationWithPointSize_weight(point, Weight::Semibold.ns());
    let pal = NSImageSymbolConfiguration::configurationWithPaletteColors(&NSArray::from_retained_slice(colors));
    img.imageWithSymbolConfiguration(&cfg.configurationByApplyingConfiguration(&pal))
}

fn draw_symbol(img: &NSImage, r: CGRect) {
    unsafe {
        let _: () = msg_send![img, drawInRect: r, fromRect: CGRect::ZERO, operation: 2usize, fraction: 1.0f64, respectFlipped: true, hints: std::ptr::null::<AnyObject>()];
    }
}

/// The island as it looks open: backdrop, black shape, hairline, header, then the body bitmap.
fn compose(title: &str, kind: &ViewerKind, size: Size, body: Rect, content: &Image, overlay: Option<(Image, CGRect)>) -> Option<Image> {
    let (margin, s) = (28.0, 2.0);
    let (w, h) = (size.w + 2.0 * margin, size.h + 2.0 * margin);
    let b = Bitmap::new((w * s) as usize, (h * s) as usize)?;
    b.scale(s, s);
    // Flip to y-down so everything below reads like the island's own coordinates.
    b.translate(0.0, h);
    b.scale(1.0, -1.0);
    b.fill(cgr(Rect::new(0.0, 0.0, w, h)), &objc2_app_kit::NSColor::colorWithSRGBRed_green_blue_alpha(0.17, 0.18, 0.21, 1.0).CGColor());
    let shape = cgr(Rect::new(margin, margin, size.w, size.h));
    let radius = look::radius(size.h);
    b.rounded_path(shape, radius);
    b.fill_path(&look::black().CGColor());
    b.with_appkit(true, || {
        // Header: symbol, title, close.
        let mut title_x = margin + look::PAD + 28.0;
        if let Some(sym) = palette_symbol(FileViewer::symbol(kind), look::ICON_SIZE, &[look::white(1.0)]) {
            let ss = sym.size();
            title_x = margin + look::PAD + ss.width.max(22.0) + look::HEADER_GAP;
            draw_symbol(&sym, CGRect::new(CGPoint::new(margin + look::PAD, margin + (look::PILL_H - ss.height) / 2.0 + 4.0), ss));
        }
        let a = look::attrs(&look::title_font(), &look::ink(), None, false);
        unsafe {
            let _: () = msg_send![&*ns(title), drawAtPoint: CGPoint::new(title_x, margin + 12.0), withAttributes: &*a];
        }
        if let Some(x) = palette_symbol("xmark.circle.fill", 18.0, &[look::white(0.9), look::white(0.18)]) {
            let xs = x.size();
            let r = CGRect::new(
                CGPoint::new(margin + size.w - look::PAD - 4.0 - xs.width / 2.0, margin + look::PILL_H / 2.0 + 4.0 - xs.height / 2.0),
                xs,
            );
            draw_symbol(&x, r);
        }
    });
    // Body (bitmaps are y-up: draw them flipped back).
    let draw_image = |img: &Image, r: CGRect| {
        b.save();
        b.translate(r.origin.x, r.origin.y + r.size.height);
        b.scale(1.0, -1.0);
        b.draw(img, CGRect::new(CGPoint::new(0.0, 0.0), r.size));
        b.restore();
    };
    b.save();
    b.rounded_path(shape, radius);
    b.clip_path();
    let bo = body.offset(margin, margin);
    draw_image(content, cgr(bo));
    if let Some((img, frame)) = overlay {
        draw_image(&img, CGRect::new(CGPoint::new(frame.origin.x + bo.x, frame.origin.y + bo.y), frame.size));
    }
    b.restore();
    b.rounded_path(shape, radius);
    b.stroke_path(&look::hairline(), look::HAIRLINE_WIDTH);
    b.image()
}

// MARK: Refresh check

/// The controller's rebuild, as Swift's `reloadViewer`: offset → teardown → a new viewer with the restore offset.
fn rebuild(run: &Run, old: &Rc<FileViewer>) -> Rc<FileViewer> {
    let offset = old.scroll_offset();
    let path = old.path().to_string_lossy().into_owned();
    old.teardown();
    let v = FileViewer::new(&path, vec![]);
    v.set_restore_offset(offset);
    show(run, &v);
    v
}

fn show(run: &Run, v: &Rc<FileViewer>) {
    let body = FileViewer::body_rect(v.island_size(SCREEN));
    for s in run.host.subviews().iter() {
        s.removeFromSuperview();
    }
    let root = v.make_view(body.size());
    root.setFrame(cgr(Rect::new(20.0, 20.0, body.w, body.h)));
    run.host.addSubview(&root);
    root.layoutSubtreeIfNeeded();
    v.layout(body.size());
}

/// Viewer refresh, the viewer's half (the watch is the controller's): a markdown viewer on a lab file, scrolled to
/// 400, then the file replaced by rename (stale, rebuilt, place kept), written in place (stale, same inode), made
/// shorter (scroll clamped), removed ("Not found") and restored (markdown again). Synchronous text: no waiting.
fn refresh_check(run: Rc<Run>, done: Box<dyn FnOnce(Vec<String>)>) {
    let dir = run.out.join("refresh-lab");
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::create_dir_all(&dir);
    let file = dir.join("notes.md");
    let lines_of = |tag: &str, n: usize| -> String {
        let mut s = format!("# {tag}\n\n");
        s += &(1..=n).map(|i| format!("Line {i} of {tag}.")).collect::<Vec<_>>().join("\n\n");
        s
    };
    let mut out = vec![];
    let mut check = |name: &str, ok: bool, detail: String| {
        out.push(format!("viewer-refresh {}: {name} {detail}", if ok { "PASS" } else { "FAIL" }));
    };
    let _ = std::fs::write(&file, lines_of("First", 120));
    let v = FileViewer::new(&file.to_string_lossy(), vec![]);
    show(&run, &v);
    let sv = v.scroll_view();
    if let Some(sv) = &sv {
        sv.contentView().scrollToPoint(CGPoint::new(0.0, 400.0));
        sv.reflectScrolledClipView(&sv.contentView());
    }
    check("viewer shows its text in a scroll view", v.text_prefix(5).as_deref() == Some("First") && sv.is_some(), String::new());
    check("an unchanged file is not stale", !v.is_stale(), String::new());
    // Replaced by rename (editors and agents save this way): a new inode.
    let tmp = dir.join("notes.md.tmp");
    let _ = std::fs::write(&tmp, lines_of("Second", 120));
    let _ = std::fs::rename(&tmp, &file);
    check("a rename-replace makes it stale", v.is_stale(), String::new());
    let v = rebuild(&run, &v);
    let y = v.scroll_offset().map(|p| p.y).unwrap_or(-1.0);
    check("the rebuilt viewer shows the new text", v.text_prefix(6).as_deref() == Some("Second"), v.text_prefix(6).unwrap_or_default());
    check("a re-render keeps the scroll position", (y - 400.0).abs() < 2.0, format!("{y:.0}"));
    // Written in place: the same inode, a new size and time.
    let ino = v.stamp().map(|s| s.ino);
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().write(true).truncate(true).open(&file).unwrap();
        let _ = f.write_all(lines_of("Third", 110).as_bytes());
    }
    check("an in-place write makes it stale", v.is_stale() && super::viewer::stamp(&file).map(|s| s.ino) == ino, String::new());
    let v = rebuild(&run, &v);
    let y = v.scroll_offset().map(|p| p.y).unwrap_or(-1.0);
    check("in-place write re-renders, place kept", v.text_prefix(5).as_deref() == Some("Third") && (y - 400.0).abs() < 2.0, format!("{y:.0}"));
    // Much shorter: the place clamps to the new length.
    let _ = std::fs::write(&file, lines_of("Fourth", 34));
    let v = rebuild(&run, &v);
    let (y, max) = match v.scroll_view() {
        Some(sv) => (
            sv.contentView().bounds().origin.y,
            sv.documentView().map(|d| d.frame().size.height).unwrap_or(0.0) - sv.contentView().bounds().size.height,
        ),
        None => (-1.0, 0.0),
    };
    check("scroll clamped to a shorter file", v.text_prefix(6).as_deref() == Some("Fourth") && max > 0.0 && max < 400.0 && (y - max).abs() < 1.0, format!("{y:.0} of {max:.0}"));
    // Removed: "Not found"; back: markdown again.
    let _ = std::fs::remove_file(&file);
    check("a removed file makes it stale", v.is_stale(), String::new());
    let v = rebuild(&run, &v);
    check("a removed file shows Not found", v.kind() == ViewerKind::Missing && v.stamp().is_none(), v.kind().name());
    let _ = std::fs::write(&file, lines_of("Fifth", 3));
    check("a returning file makes it stale", v.is_stale(), String::new());
    let v = rebuild(&run, &v);
    check("a returning file comes back", v.kind() == ViewerKind::Markdown && v.text_prefix(5).as_deref() == Some("Fifth"), v.kind().name());
    v.teardown();
    let _ = std::fs::remove_dir_all(&dir);
    done(out);
}

// MARK: Events check

/// A mouse event at a window point, delivered to the view under it.
fn click(run: &Run, p: crate::model::geometry::Point, kind: objc2_app_kit::NSEventType) {
    let e = objc2_app_kit::NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
        kind,
        CGPoint::new(p.x, p.y),
        objc2_app_kit::NSEventModifierFlags::empty(),
        sys::uptime(),
        run.window.windowNumber(),
        None,
        0,
        1,
        1.0,
    );
    // A window never ordered in drops mouse events in `sendEvent`; do what it does for a visible one: the view
    // under the point (hitTest from the frame view) gets mouseDown:/mouseUp:. The real panel path is the self-test's.
    let (Some(e), Some(cv)) = (e, run.window.contentView()) else { return };
    let frame_view = unsafe { cv.superview() }.unwrap_or(cv.clone());
    let q = frame_view.convertPoint_fromView(CGPoint::new(p.x, p.y), None);
    let Some(hit) = frame_view.hitTest(q) else { return };
    unsafe {
        if kind == objc2_app_kit::NSEventType::LeftMouseDown {
            let _: () = msg_send![&*hit, mouseDown: &*e];
        } else {
            let _: () = msg_send![&*hit, mouseUp: &*e];
        }
    }
}

fn press(run: &Run, p: crate::model::geometry::Point) {
    click(run, p, objc2_app_kit::NSEventType::LeftMouseDown);
    click(run, p, objc2_app_kit::NSEventType::LeftMouseUp);
}

/// The proposed actions and the audio player driven by mouse events: a click asks "Sure?", a second click on the
/// same button after the settle time sends it; the player (muted: lab runs only with FI_MUTE=1) plays on a click,
/// scrubs on a waveform click, and stops on pause.
/// The report bridge (`docs/one-system.md`): `report-bridge.html`, copied into the output folder, loads offscreen;
/// `window.heimdall` is there, a pick reaches the island's handler, a save writes `picks.json` beside the page and a
/// path outside the folder is refused.
fn bridge_check(run: Rc<Run>, done: Box<dyn FnOnce(Vec<String>)>) {
    let dir = run.out.join("bridge");
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::create_dir_all(&dir);
    let page = dir.join("report-bridge.html");
    if std::fs::copy(run.fixtures.join("report-bridge.html"), &page).is_err() {
        return done(vec!["viewer-bridge FAIL: no fixture report-bridge.html".into()]);
    }
    let v = FileViewer::new(&page.to_string_lossy(), vec![]);
    type Got = Vec<(String, serde_json::Value, Option<String>)>;
    let got: Rc<RefCell<Got>> = Rc::default();
    let g2 = got.clone();
    v.set_on_report(Rc::new(move |a, val, l| g2.borrow_mut().push((a, val, l))));
    let body = FileViewer::body_rect(v.island_size(SCREEN));
    for s in run.host.subviews().iter() {
        s.removeFromSuperview();
    }
    let root = v.make_view(body.size());
    root.setFrame(cgr(Rect::new(20.0, 20.0, body.w, body.h)));
    run.host.addSubview(&root);
    sys::after(2.0, move || {
        if let Some(w) = v.web_view() {
            let js = "window.heimdall.send('pick','b','Take B');window.heimdall.send('save',{file:'picks.json',data:{take:'b'}},'Save picks');window.heimdall.send('save',{file:'../evil.json',data:1});window.webkit.messageHandlers.heimdall.postMessage({action:'raw',value:{n:2}});";
            let _: () = unsafe { msg_send![&*w, evaluateJavaScript: &*ns(js), completionHandler: std::ptr::null::<AnyObject>()] };
        }
        sys::after(0.8, move || {
            let g = got.borrow();
            let mut lines = vec![];
            let mut check = |name: &str, ok: bool, detail: String| lines.push(format!("viewer-bridge {}: {name} {detail}", if ok { "PASS" } else { "FAIL" }));
            check("a pick reaches the island", g.first().is_some_and(|m| m.0 == "pick" && m.1 == "b" && m.2.as_deref() == Some("Take B")), format!("{:?}", g.first()));
            let saved: Option<serde_json::Value> = std::fs::read_to_string(dir.join("picks.json")).ok().and_then(|t| serde_json::from_str(&t).ok());
            check("save writes picks.json beside the page", saved == Some(serde_json::json!({"take": "b"})) && g.get(1).is_some_and(|m| m.0 == "save"), format!("{saved:?}"));
            check("a path outside the folder is refused", !run.out.join("evil.json").exists() && g.get(2).is_some_and(|m| m.1.get("refused") == Some(&serde_json::Value::Bool(true))), format!("{:?}", g.get(2)));
            check("a raw postMessage object arrives too", g.get(3).is_some_and(|m| m.0 == "raw" && m.1 == serde_json::json!({"n": 2})), format!("{:?}", g.get(3)));
            drop(g);
            v.teardown();
            let _ = std::fs::remove_dir_all(&dir);
            done(lines);
        });
    });
}

fn events_check(run: Rc<Run>, done: Box<dyn FnOnce(Vec<String>)>) {
    let lines: Rc<RefCell<Vec<String>>> = Rc::default();
    let check = {
        let lines = lines.clone();
        move |name: &str, ok: bool, detail: String| {
            lines.borrow_mut().push(format!("viewer-events {}: {name} {detail}", if ok { "PASS" } else { "FAIL" }));
        }
    };
    if !super::viewer_audio::muted_by_env() {
        check("skipped: needs FI_MUTE=1", true, String::new());
        let out = lines.borrow().clone();
        return done(out);
    }
    let v = FileViewer::new(&run.fixtures.join("plan.md").to_string_lossy(), plan_actions());
    let sent: Rc<RefCell<Option<String>>> = Rc::default();
    let s2 = sent.clone();
    v.set_on_action(Box::new(move |a| *s2.borrow_mut() = Some(a.action)));
    let body = FileViewer::body_rect(v.island_size(SCREEN));
    for s in run.host.subviews().iter() {
        s.removeFromSuperview();
    }
    let root = v.make_view(body.size());
    root.setFrame(cgr(Rect::new(20.0, 20.0, body.w, body.h)));
    run.host.addSubview(&root);
    check("viewer shows its text in a scroll view", v.has_scroll_view() && v.has_text(), String::new());
    let Some(row) = v.action_row() else {
        check("viewer shows its proposed actions", false, String::new());
        let out = lines.borrow().clone();
        return done(out);
    };
    let b0 = row.button_frame(0).unwrap_or_default();
    let p = row.to_window(crate::model::geometry::Point::new(b0.mid_x(), b0.mid_y()));
    press(&run, p);
    check("an action asks before it sends", row.is_confirming() && sent.borrow().is_none(), String::new());
    let (c2, run2) = (check.clone(), run.clone());
    sys::after(0.45, move || {
        press(&run2, p);
        c2("a second click on it sends", sent.borrow().as_deref() == Some("do-all") && !row.is_confirming(), format!("{:?}", sent.borrow()));
        c2("the choice shows", row.chosen().as_deref() == Some("do-all"), String::new());
        v.teardown();
        // Audio.
        let a = FileViewer::new(&run2.fixtures.join("melody.m4a").to_string_lossy(), vec![]);
        let body = FileViewer::body_rect(a.island_size(SCREEN));
        for s in run2.host.subviews().iter() {
            s.removeFromSuperview();
        }
        let root = a.make_view(body.size());
        root.setFrame(cgr(Rect::new(20.0, 20.0, body.w, body.h)));
        run2.host.addSubview(&root);
        let (c3, run3) = (c2.clone(), run2.clone());
        sys::after(0.8, move || {
            let Some(p) = a.audio_player() else {
                c3("audio viewer shows its player", false, String::new());
                let out = lines.borrow().clone();
                return done(out);
            };
            c3("the player is muted (FI_MUTE)", p.is_muted(), String::new());
            c3("the waveform is read", p.has_peaks(), String::new());
            press(&run3, p.button_centre_in_window());
            c3("audio plays on a click", p.is_playing(), String::new());
            press(&run3, p.wave_point_in_window(0.5));
            let c4 = c3.clone();
            sys::after(0.15, move || {
                let s = p.seconds();
                c4("a click on the waveform scrubs to the middle", (s - 3.03) > -0.1 && (s - 3.03) < 0.4, format!("{s:.2} s"));
                a.pause();
                c4("closing pauses it", !p.is_playing(), String::new());
                a.teardown();
                let out = lines.borrow().clone();
                done(out);
            });
        });
    });
}
