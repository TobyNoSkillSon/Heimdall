//! Offscreen renders and checks of the viewer's zoom (bug note 2026-11-04T19-07-48), at real size (2×), in a window
//! that is never ordered in:
//!   FLOATING_ISLANDS_DIR=<lab dir> FloatingIslands --viewer-zoom-shots <out-dir> <fixtures-dir>
//! Fixtures: report.md, dusk.png, page.html, report.pdf. PNGs: zoom-1.0, zoom-1.5 (three ⌘= key equivalents through
//! the root view), zoom-0.5, fit-wider (zoom 1 in a viewer 1.4× wider than it first laid out), image-2.0. Checks go to
//! report.txt, one `ok`/`FAIL` line each; the zoom memory is the lab folder's `viewer-zoom.json`.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use objc2::rc::Retained;
use objc2::{msg_send, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{NSAppearanceCustomization, NSEvent, NSEventModifierFlags, NSEventType, NSWindow, NSWindowStyleMask};
use objc2_core_foundation::CGPoint;

use super::draw;
use super::look::{cgr, ns};
use super::sys;
use super::viewer::{FileViewer, FlippedView};
use super::viewer_shots::finish;
use crate::model::geometry::{Rect, Size};
use crate::model::zoom;

const SCREEN: Size = Size::new(1728.0, 1117.0);

struct Run {
    out: PathBuf,
    fixtures: PathBuf,
    host: Retained<FlippedView>,
    _window: Retained<NSWindow>,
    lines: RefCell<Vec<String>>,
}

impl Run {
    fn check(&self, name: &str, ok: bool, detail: String) {
        self.lines.borrow_mut().push(format!("{} {name}{}", if ok { "ok  " } else { "FAIL" }, if detail.is_empty() { String::new() } else { format!(" ({detail})") }));
    }

    fn file(&self, name: &str) -> String {
        self.fixtures.join(name).to_string_lossy().into_owned()
    }

    /// Puts the viewer in the host at `body` and lays it out.
    fn show(&self, v: &Rc<FileViewer>, body: Rect) {
        for s in self.host.subviews().iter() {
            s.removeFromSuperview();
        }
        let root = v.make_view(body.size());
        root.setFrame(cgr(Rect::new(20.0, 20.0, body.w, body.h)));
        self.host.addSubview(&root);
        root.layoutSubtreeIfNeeded();
        v.layout(body.size());
    }

    /// Renders the viewer with its chrome to `<out>/<name>.png`, then `then`.
    fn shot(self: &Rc<Self>, v: &Rc<FileViewer>, name: &'static str, title: &str, size: Size, then: impl FnOnce() + 'static) {
        let body = FileViewer::body_rect(size);
        let me = self.clone();
        finish(v, title, size, body, Box::new(move |img| {
            let png = me.out.join(format!("{name}.png"));
            let wrote = img.as_ref().is_some_and(|i| draw::write_png(i, &png));
            me.check(&format!("render {name}"), wrote, String::new());
            then();
        }));
    }
}

fn near(a: f64, b: f64) -> bool {
    (a - b).abs() < 0.01
}

/// ⌘ + `c` as AppKit delivers it, sent to the root view's key-equivalent path.
fn command_key(v: &FileViewer, c: &str) -> bool {
    let Some(root) = v.root() else { return false };
    let e = {
        NSEvent::keyEventWithType_location_modifierFlags_timestamp_windowNumber_context_characters_charactersIgnoringModifiers_isARepeat_keyCode(
            NSEventType::KeyDown,
            CGPoint::new(0.0, 0.0),
            NSEventModifierFlags::Command,
            0.0,
            0,
            None,
            &ns(c),
            &ns(c),
            false,
            if c == "=" { 24 } else if c == "-" { 27 } else { 29 },
        )
    };
    let Some(e) = e else { return false };
    unsafe { msg_send![&*root, performKeyEquivalent: &*e] }
}

pub fn run(out: &str, fixtures: &str) -> ! {
    if std::env::var("FLOATING_ISLANDS_DIR").is_err() {
        println!("viewer-zoom-shots: needs FLOATING_ISLANDS_DIR (the zoom memory is written there)");
        std::process::exit(2);
    }
    let mtm = MainThreadMarker::new().expect("main thread");
    let out = PathBuf::from(out);
    let _ = std::fs::create_dir_all(&out);
    let _ = std::fs::remove_file(zoom::Memory::default_path());
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            cgr(Rect::new(-30000.0, -30000.0, 1600.0, 1100.0)),
            NSWindowStyleMask::Borderless,
            objc2_app_kit::NSBackingStoreType::Buffered,
            false,
        )
    };
    unsafe { window.setReleasedWhenClosed(false) };
    if let Some(a) = objc2_app_kit::NSAppearance::appearanceNamed(unsafe { objc2_app_kit::NSAppearanceNameDarkAqua }) {
        window.setAppearance(Some(&a));
    }
    let host = FlippedView::new(cgr(Rect::new(0.0, 0.0, 1600.0, 1100.0)));
    window.setContentView(Some(&host));
    let run = Rc::new(Run { out, fixtures: PathBuf::from(fixtures), host, _window: window, lines: RefCell::new(vec![]) });
    let r = run.clone();
    sys::soon(move || markdown(r));
    objc2_app_kit::NSApplication::sharedApplication(mtm).run();
    std::process::exit(0)
}

fn markdown(run: Rc<Run>) {
    let path = run.file("report.md");
    let v = FileViewer::new(&path, vec![]);
    let size = v.island_size(SCREEN);
    let body = FileViewer::body_rect(size);
    run.show(&v, body);
    run.check("a new file opens at zoom 1", near(v.zoom(), 1.0) && v.zoom_state().is_some_and(|(m, f)| near(m, 1.0) && near(f, 1.0)), format!("{:?}", v.zoom_state()));
    let natural = v.text_layout_width().unwrap_or(0.0);
    let top = |v: &FileViewer| v.scroll_offset().map(|o| o.y.abs() < 1.0).unwrap_or(false);
    run.check("text lays out at the viewer's width", near(natural, body.w), format!("{natural} vs {}", body.w));
    let r = run.clone();
    run.shot(&v.clone(), "zoom-1.0", "Disk cleanup report", size, move || {
        let run = r;
        // Three ⌘= through the root's key equivalents: 1 → 1.1 → 1.25 → 1.5.
        let took = (0..3).all(|_| command_key(&v, "="));
        run.check("⌘= is taken by the viewer", took, String::new());
        run.check("three ⌘= zoom to 1.5", near(v.zoom(), 1.5) && v.zoom_state().is_some_and(|(m, _)| near(m, 1.5)), format!("{:?}", v.zoom_state()));
        run.check("zoomed in, the lines keep their breaks (natural width)", v.text_layout_width().is_some_and(|w| near(w, natural)), format!("{:?}", v.text_layout_width()));
        run.check("zoomed at the top, the title stays in view", top(&v), format!("{:?}", v.scroll_offset()));
        run.check("⌘A is not a zoom key", !command_key(&v, "a"), String::new());
        let saved = zoom::Memory::load(&zoom::Memory::default_path()).get(&v.path().to_string_lossy());
        run.check("the zoom is remembered for the file", near(saved, 1.5), format!("{saved}"));
        let size = v.island_size(SCREEN);
        run.show(&v, FileViewer::body_rect(size));
        let r = run.clone();
        run.shot(&v.clone(), "zoom-1.5", "Disk cleanup report", size, move || {
            let run = r;
            v.teardown();
            reopen(run, path);
        });
    });
}

fn reopen(run: Rc<Run>, path: String) {
    let v = FileViewer::new(&path, vec![]);
    let size = v.island_size(SCREEN);
    run.show(&v, FileViewer::body_rect(size));
    run.check("reopened, the file comes back at 1.5", near(v.zoom(), 1.5) && v.zoom_state().is_some_and(|(m, _)| near(m, 1.5)), format!("{:?}", v.zoom_state()));
    command_key(&v, "0");
    run.check("⌘0 goes back to 1", near(v.zoom(), 1.0) && v.zoom_state().is_some_and(|(m, _)| near(m, 1.0)), format!("{:?}", v.zoom_state()));
    let saved = zoom::Memory::load(&zoom::Memory::default_path()).get(&path);
    run.check("back at 1, the memory forgets the file", near(saved, 1.0), format!("{saved}"));
    let natural = v.text_layout_width().unwrap_or(0.0);
    v.set_zoom(0.5);
    run.check("zoomed out to 0.5, the text reflows twice as wide", v.text_layout_width().is_some_and(|w| (w - 2.0 * natural).abs() < 1.5), format!("{:?} vs {}", v.text_layout_width(), natural));
    v.set_zoom(0.1);
    run.check("zoom stops at 0.5", near(v.zoom(), 0.5), format!("{}", v.zoom()));
    let size = v.island_size(SCREEN);
    run.show(&v, FileViewer::body_rect(size));
    let r = run.clone();
    run.shot(&v.clone(), "zoom-0.5", "Disk cleanup report", size, move || {
        let run = r;
        v.set_zoom(1.0);
        v.teardown();
        fit_wider(run, path);
    });
}

fn fit_wider(run: Rc<Run>, path: String) {
    let v = FileViewer::new(&path, vec![]);
    let size = v.island_size(SCREEN);
    let body = FileViewer::body_rect(size);
    run.show(&v, body);
    let natural = v.text_layout_width().unwrap_or(0.0);
    // The viewer grows 1.4× wider (another screen, a resize): the content scales to the new width.
    let wide = Size::new((size.w + body.w * 0.4).round(), size.h);
    let wbody = FileViewer::body_rect(wide);
    run.show(&v, wbody);
    let want = wbody.w / body.w;
    run.check(
        "a wider viewer scales its content to the width",
        v.zoom_state().is_some_and(|(m, f)| near(f, want) && near(m, want)) && near(v.zoom(), 1.0),
        format!("{:?}, want {want:.3}", v.zoom_state()),
    );
    run.check("fit to width at the top keeps the title in view", v.scroll_offset().map(|o| o.y.abs() < 1.0).unwrap_or(false), format!("{:?}", v.scroll_offset()));
    run.check("fit to width keeps the line breaks", v.text_layout_width().is_some_and(|w| near(w, natural)), format!("{:?}", v.text_layout_width()));
    let r = run.clone();
    run.shot(&v.clone(), "fit-wider", "Disk cleanup report", wide, move || {
        let run = r;
        // ⌘= on top of the fit: the chosen zoom steps, the fit stays.
        command_key(&v, "=");
        run.check("⌘= over a fit steps the zoom, keeps the fit", near(v.zoom(), 1.1) && v.zoom_state().is_some_and(|(m, f)| near(f, want) && near(m, 1.1 * want)), format!("{:?}", v.zoom_state()));
        v.set_zoom(1.0);
        v.teardown();
        image(run);
    });
}

fn image(run: Rc<Run>) {
    let v = FileViewer::new(&run.file("dusk.png"), vec![]);
    let size = v.island_size(SCREEN);
    run.show(&v, FileViewer::body_rect(size));
    run.check("a picture opens filling its viewer", v.zoom_state().is_some_and(|(m, _)| near(m, 1.0)), format!("{:?}", v.zoom_state()));
    command_key(&v, "-");
    run.check("a picture does not zoom out past its fit", near(v.zoom(), 1.0), format!("{}", v.zoom()));
    v.set_zoom(2.0);
    run.check("a picture zooms to 2", v.zoom_state().is_some_and(|(m, _)| near(m, 2.0)), format!("{:?}", v.zoom_state()));
    let r = run.clone();
    run.shot(&v.clone(), "image-2.0", "Dusk", size, move || {
        let run = r;
        v.set_zoom(1.0);
        v.teardown();
        web(run);
    });
}

fn web(run: Rc<Run>) {
    let v = FileViewer::new(&run.file("page.html"), vec![]);
    let size = v.island_size(SCREEN);
    run.show(&v, FileViewer::body_rect(size));
    let r = run.clone();
    sys::after(2.5, move || {
        let run = r;
        command_key(&v, "=");
        command_key(&v, "=");
        run.check("⌘= twice zooms a page to 1.25 (page zoom)", near(v.zoom(), 1.25) && v.zoom_state().is_some_and(|(m, _)| near(m, 1.25)), format!("{:?}", v.zoom_state()));
        let saved = zoom::Memory::load(&zoom::Memory::default_path()).get(&v.path().to_string_lossy());
        run.check("a page's zoom is remembered", near(saved, 1.25), format!("{saved}"));
        v.set_zoom(1.0);
        v.teardown();
        pdf(run);
    });
}

fn pdf(run: Rc<Run>) {
    let v = FileViewer::new(&run.file("report.pdf"), vec![]);
    let size = v.island_size(SCREEN);
    run.show(&v, FileViewer::body_rect(size));
    let r = run.clone();
    sys::after(2.0, move || {
        let run = r;
        run.check("a PDF opens fitted to its width", v.zoom_state().is_some_and(|(m, f)| near(m / f, 1.0)), format!("{:?}", v.zoom_state()));
        command_key(&v, "=");
        run.check("⌘= zooms a PDF to 1.1× its fit", near(v.zoom(), 1.1) && v.zoom_state().is_some_and(|(m, f)| near(m / f, 1.1)), format!("{:?}", v.zoom_state()));
        // A wider viewer: the fit follows the width, the chosen zoom stays.
        let wide = Size::new(size.w + 200.0, size.h);
        run.show(&v, FileViewer::body_rect(wide));
        run.check("a wider PDF viewer keeps the zoom over the new fit", near(v.zoom(), 1.1) && v.zoom_state().is_some_and(|(m, f)| near(m / f, 1.1)), format!("{:?}", v.zoom_state()));
        command_key(&v, "0");
        run.check("⌘0 gives a PDF back to fit-to-width", v.zoom_state().is_some_and(|(m, f)| near(m / f, 1.0)), format!("{:?}", v.zoom_state()));
        v.teardown();
        let mem = zoom::Memory::load(&zoom::Memory::default_path());
        run.check("everything back at 1 leaves the memory empty", mem.is_empty(), format!("{} entries", mem.len()));
        let text = run.lines.borrow().join("\n") + "\n";
        let _ = std::fs::write(run.out.join("report.txt"), &text);
        print!("{text}");
        std::process::exit(if text.contains("FAIL") { 1 } else { 0 });
    });
}
