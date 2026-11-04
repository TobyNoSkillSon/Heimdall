//! Lab: the viewer's drag handle end to end, offscreen (no window is ordered in, the user's pointer and clipboard are never
//! touched).
//!   FLOATING_ISLANDS_DIR=<lab dir> FloatingIslands --share-check <out dir> <page.html> <plain file>
//! 1. The page and its shared copy drawn by the viewer's own web view (`page/original.png`, `page/bundled.png`), and
//!    the lone page copied into an empty folder (`page/bare.png`: what a lone .html would look like to someone else).
//! 2. The real viewer island on the page: its title row at rest, under the pointer and just after a click
//!    (`titlebar-*.png`, the whole island `viewer-rest.png`); the click's copy read back from the lab pasteboard.
//! 3. A real drag session begun from the handle with a made-up mouse event (never posted: the pointer stays where it
//!    is); the drag pasteboard read back the way a drop target reads it. The lab script then reads it again from
//!    another process (`lab/tools/drop_target.swift`).
//! 4. The same for the plain file (it goes as it is).
//! Lines go to `<out>/report.txt`.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{msg_send, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{NSApplication, NSBackingStoreType, NSEvent, NSEventModifierFlags, NSEventType, NSImage, NSPasteboard, NSWindow, NSWindowStyleMask};
use objc2_core_foundation::{CGPoint, CGRect};

use super::app::app;
use super::draw::{self, Cf, Image, Ref};
use super::island::Ctl;
use super::look::cgr;
use super::render::{clear_folder, put, Render};
use super::share;
use super::sys;
use super::viewer::{FileViewer, FlippedView};
use crate::jobj;
use crate::model::geometry::{Rect, Size};
use crate::model::share::Shared;
use crate::model::Paths;

type Lines = Rc<RefCell<Vec<String>>>;

fn log(lines: &Lines, s: String) {
    println!("{s}");
    lines.borrow_mut().push(s);
}

fn finish(out: &Path, lines: &Lines) -> ! {
    let _ = std::fs::write(out.join("report.txt"), lines.borrow().join("\n") + "\n");
    clear_folder();
    super::terminate()
}

pub fn run(out: &str, page: &str, plain: &str) {
    if std::env::var("FLOATING_ISLANDS_DIR").is_err() {
        println!("share-check: needs FLOATING_ISLANDS_DIR");
        std::process::exit(2);
    }
    let out = PathBuf::from(out);
    let _ = std::fs::create_dir_all(out.join("page"));
    let page = PathBuf::from(page).canonicalize().expect("page");
    let plain = PathBuf::from(plain).canonicalize().expect("plain file");
    let lines: Lines = Rc::default();
    clear_folder();
    let shared = crate::model::share::shareable(&page, &Paths::shares()).expect("shareable");
    let bundled = match &shared {
        Shared::Bundled { path, inlined } => {
            let alone = std::fs::read_dir(path.parent().unwrap()).map(|d| d.count()).unwrap_or(0);
            log(&lines, format!("bundle: {} files inlined, {} bytes, alone in its folder: {}", inlined, std::fs::metadata(path).map(|m| m.len()).unwrap_or(0), alone == 1));
            path.clone()
        }
        Shared::AsIs(_) => {
            log(&lines, "bundle: FAILED (the page went as it is)".into());
            finish(&out, &lines)
        }
    };
    let bare_dir = out.join("page/bare-folder");
    let _ = std::fs::remove_dir_all(&bare_dir);
    let _ = std::fs::create_dir_all(&bare_dir);
    let bare = bare_dir.join(page.file_name().unwrap());
    let _ = std::fs::copy(&page, &bare);
    let pages = vec![("original", page.clone()), ("bundled", bundled.clone()), ("bare", bare)];
    let (o2, l2) = (out.clone(), lines.clone());
    draw_pages(Rc::new(pages), 0, out.clone(), Box::new(move || island_checks(o2, l2, page, plain, bundled)));
}

// MARK: 1. The page and its copy, drawn

fn draw_pages(pages: Rc<Vec<(&'static str, PathBuf)>>, i: usize, out: PathBuf, done: Box<dyn FnOnce()>) {
    if i >= pages.len() {
        return done();
    }
    let mtm = MainThreadMarker::new().unwrap();
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(NSWindow::alloc(mtm), cgr(Rect::new(-30000.0, -30000.0, 1200.0, 1400.0)), NSWindowStyleMask::Borderless, NSBackingStoreType::Buffered, false)
    };
    unsafe { window.setReleasedWhenClosed(false) };
    let host = FlippedView::new(cgr(Rect::new(0.0, 0.0, 1200.0, 1400.0)));
    window.setContentView(Some(&host));
    let (name, path) = pages[i].clone();
    let v = FileViewer::new(&path.to_string_lossy(), vec![]);
    let body = Size::new(820.0, 1200.0);
    let root = v.make_view(body);
    root.setFrame(cgr(Rect::new(0.0, 0.0, body.w, body.h)));
    host.addSubview(&root);
    sys::after(2.5, move || {
        let _keep = (&window, &host);
        let Some(web) = v.web_view() else {
            println!("share-check: {name}: no web view");
            return draw_pages(pages, i + 1, out, done);
        };
        let cfg: Retained<AnyObject> = unsafe { msg_send![objc2::class!(WKSnapshotConfiguration), new] };
        unsafe {
            let _: () = msg_send![&*cfg, setRect: web.bounds()];
        }
        let state = RefCell::new(Some((pages, out, done, v, window, host)));
        let block = block2::RcBlock::new(move |img: *mut NSImage, _e: *mut AnyObject| {
            let Some((pages, out, done, v, window, _host)) = state.borrow_mut().take() else { return };
            if !img.is_null() {
                let img: &NSImage = unsafe { &*img };
                let cg: *mut objc2_core_graphics::CGImage = unsafe {
                    msg_send![img, CGImageForProposedRect: std::ptr::null_mut::<CGRect>(), context: std::ptr::null::<AnyObject>(), hints: std::ptr::null::<AnyObject>()]
                };
                if let Some(i) = retain(cg) {
                    draw::write_png(&i, &out.join(format!("page/{name}.png")));
                }
            }
            v.teardown();
            window.close();
            sys::soon(move || draw_pages(pages, i + 1, out, done));
        });
        unsafe {
            let _: () = msg_send![&*web, takeSnapshotWithConfiguration: &*cfg, completionHandler: &*block];
        }
    });
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

// MARK: 2-4. The island: title row, click, drag

fn island_checks(out: PathBuf, lines: Lines, page: PathBuf, plain: PathBuf, bundled: PathBuf) {
    put("share-view", jobj! {"preset": "viewer", "title": "Night pass", "path": page.to_string_lossy(), "expanded": true});
    let r = Render::new(&out.to_string_lossy());
    sys::after(2.5, move || {
        let Some(c) = app().ctl("share-view") else {
            log(&lines, "island: FAILED (no viewer island)".into());
            finish(&out, &lines)
        };
        let Some(h) = c.shown_share() else {
            log(&lines, "handle: FAILED (no drag handle on the open viewer)".into());
            finish(&out, &lines)
        };
        let shape = c.visible_shape_rect();
        let bar = Rect::new(shape.x, shape.max_y() - 52.0, shape.w, 52.0).inset(-12.0, -12.0).integral();
        r.shot("viewer-rest", Some(&["share-view"]), None);
        r.shot("titlebar-rest", None, Some(bar));
        log(&lines, format!("handle: shown at rest, opacity {:.2}, hovered {}", h.layer.opacity(), h.hovered()));
        c.share_hover(true);
        let (out2, lines2) = (out.clone(), lines.clone());
        sys::after(0.4, move || {
            let (out, lines) = (out2, lines2);
            r.shot("titlebar-hover", None, Some(bar));
            log(&lines, format!("hover: lifted {}", h.hovered()));
            // A click: pressed and released in place, as the view's mouse handlers do.
            h.press(crate::model::geometry::Point::new(0.0, 0.0));
            let took = h.released();
            sys::after(0.15, move || {
                r.shot("titlebar-copied", None, Some(bar));
                c.share_hover(false);
                let got = share::read_files(&share::pasteboard());
                let same = got.first().is_some_and(|p| std::fs::read(p).ok() == std::fs::read(&bundled).ok());
                log(&lines, format!("click: took {took}, lab pasteboard holds {:?}, the bundled copy: {}", got, same));
                let (out3, lines3) = (out.clone(), lines.clone());
                drag(&c, &h, &lines, "page", Box::new(move || plain_checks(out3, lines3, plain)));
            });
        });
    });
}

/// A real drag session from the handle, begun and ended off every display; the drag pasteboard read back as a drop
/// target reads it, here and from another process.
fn drag(c: &Rc<Ctl>, h: &Rc<share::Handle>, lines: &Lines, tag: &str, then: Box<dyn FnOnce()>) {
    // A session follows the real pointer until its button is up: with the user's button down it could carry the file
    // where he points, so the lab drag runs only with every button up, and ends at once (below).
    if NSEvent::pressedMouseButtons() != 0 {
        log(lines, format!("drag {tag}: SKIPPED (a mouse button is down)"));
        return then();
    }
    // Off every display: where the session starts and ends there is no window to drop on.
    c.panel.setFrameOrigin(objc2_foundation::NSPoint::new(-30000.0, -30000.0));
    let f = h.frame_in_view(c.shape.frame());
    let p = c.view.convertPoint_toView(CGPoint::new(f.mid_x() + 6.0, f.mid_y()), None);
    // Mouse events made in code: the drag is handed to the session and the button's release goes into this app's own
    // event queue. Nothing reaches the window server, so the pointer does not move.
    let ev = |t: NSEventType| {
        NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
            t, p, NSEventModifierFlags::empty(), sys::uptime(), c.panel.windowNumber(), None, 0, 1, if t == NSEventType::LeftMouseUp { 0.0 } else { 1.0 },
        )
    };
    let (Some(e), Some(up)) = (ev(NSEventType::LeftMouseDragged), ev(NSEventType::LeftMouseUp)) else {
        log(lines, format!("drag {tag}: FAILED (no event)"));
        return then();
    };
    share::LAB_LAST.with(|l| *l.borrow_mut() = None);
    share::LAB_ENDED.with(|l| l.set(None));
    h.press(crate::model::geometry::Point::new(f.mid_x(), f.mid_y()));
    h.dragged(&c.view, &e, crate::model::geometry::Point::new(f.mid_x() + 6.0, f.mid_y()), f);
    let began = h.began.get();
    NSApplication::sharedApplication(MainThreadMarker::new().unwrap()).postEvent_atStart(&up, true);
    // SAFETY: a framework constant.
    let pb = NSPasteboard::pasteboardWithName(unsafe { objc2_app_kit::NSPasteboardNameDrag });
    let got = share::read_files(&pb);
    let handed = share::LAB_LAST.with(|l| l.borrow().clone());
    log(lines, format!("drag {tag}: session begun {began}, handed {:?}, drag pasteboard holds {:?}", handed.map(|h| h.1), got));
    // The drop target in another process (FI_DROP_TARGET, `lab/tools/drop_target.swift`): reads the drag pasteboard
    // as a drop does and copies each file into its inbox.
    if let (Ok(bin), Ok(inbox)) = (std::env::var("FI_DROP_TARGET"), std::env::var("FI_DROP_INBOX")) {
        let inbox = format!("{inbox}/{tag}");
        match std::process::Command::new(bin).arg(&inbox).output() {
            Ok(o) => log(lines, format!("drop target {tag}: {}", String::from_utf8_lossy(&o.stdout).trim())),
            Err(e) => log(lines, format!("drop target {tag}: FAILED ({e})")),
        }
    }
    let (lines, tag) = (lines.clone(), tag.to_owned());
    sys::after(0.8, move || {
        let ended = share::LAB_ENDED.with(|l| l.get());
        log(&lines, format!("drag {tag}: session ended {:?} (screen point, operation; 0 = dropped nowhere)", ended));
        then();
    });
}

fn plain_checks(out: PathBuf, lines: Lines, plain: PathBuf) {
    put("share-plain", jobj! {"preset": "viewer", "title": "Dusk", "path": plain.to_string_lossy(), "expanded": true});
    let _ = std::fs::remove_file(Paths::file("share-view"));
    let r = Render::new(&out.to_string_lossy());
    sys::after(2.0, move || {
        let Some(c) = app().ctl("share-plain") else {
            log(&lines, "plain: FAILED (no viewer island)".into());
            finish(&out, &lines)
        };
        let Some(h) = c.shown_share() else {
            log(&lines, "plain: FAILED (no drag handle)".into());
            finish(&out, &lines)
        };
        r.shot("plain-rest", Some(&["share-plain"]), None);
        let shared = h.shared();
        log(&lines, format!("plain: goes as it is: {}", shared == plain));
        h.copy();
        let got = share::read_files(&share::pasteboard());
        log(&lines, format!("plain click: lab pasteboard holds {:?}", got));
        let l2 = lines.clone();
        drag(&c, &h, &lines, "plain", Box::new(move || finish(&out, &l2)));
    });
}
