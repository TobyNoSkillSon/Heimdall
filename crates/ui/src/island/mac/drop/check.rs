//! `heimdall-ui-demo --drop <dir>`: the drop target checked in process, offscreen. Synthetic pasteboards (files, a
//! folder, a screenshot as data, a link, text, several items) go through a fake drag session sent to the view's own
//! drag callbacks (`draggingEntered:`, `draggingUpdated:`, `performDragOperation:`) on the catcher; the items the app receives, the
//! island's confirmation, the window's properties and rest are checked; the motion's frames are rendered over the user's
//! desktop crop into a contact sheet (`drop-sheet.png`, Reduce Motion in its last row). Over the island while it shows
//! (speaking, a line, the conversation) the swell and glow are checked to be the island's own and the catcher to
//! cover its frame, also as it resizes mid-drag (`drop-island-sheet.png`). Nothing reaches the screen.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{define_class, msg_send, AllocAnyThread, DefinedClass};
use objc2_app_kit::{
    NSAttributedStringNSStringDrawing as _, NSColor, NSDragOperation, NSMainMenuWindowLevel, NSPasteboard, NSPasteboardItem, NSPasteboardTypeFileURL, NSPasteboardTypePNG, NSPasteboardTypeString,
    NSPasteboardTypeURL, NSPasteboardWriting, NSWindowSharingType,
};
use objc2_foundation::{MainThreadMarker, NSArray, NSObject, NSPoint, NSString, NSURL};

use super::super::island::checks::Checks;
use super::super::island::host::with_host;
use super::super::island::raster::Canvas;
use super::{with_target, PANEL_W};
use crate::island::drop::{self, DropKind, Dropped, WINDOW_TITLE};
use crate::island::object::{Event, Point};
use crate::model::UiAction;

struct FakeIvars {
    at: Cell<NSPoint>,
    pb: RefCell<Option<Retained<NSPasteboard>>>,
}

define_class!(
    /// A drag session's info as the view receives it: where the drag is (window points) and its pasteboard.
    #[unsafe(super(NSObject))]
    #[name = "HeimdallFakeDrag"]
    #[ivars = FakeIvars]
    struct FakeDrag;

    impl FakeDrag {
        #[unsafe(method(draggingLocation))]
        fn dragging_location(&self) -> NSPoint {
            self.ivars().at.get()
        }

        #[unsafe(method_id(draggingPasteboard))]
        fn dragging_pasteboard(&self) -> Retained<NSPasteboard> {
            self.ivars().pb.borrow().clone().expect("a pasteboard")
        }

        /// A source that allows a copy, as Finder and a screenshot's thumbnail do (`takes`).
        #[unsafe(method(draggingSourceOperationMask))]
        fn dragging_source_operation_mask(&self) -> NSDragOperation {
            NSDragOperation::Copy
        }

        /// Another app's drag: no source in this process.
        #[unsafe(method(draggingSource))]
        fn dragging_source(&self) -> *mut objc2::runtime::AnyObject {
            std::ptr::null_mut()
        }
    }
);

impl FakeDrag {
    fn new(pb: Retained<NSPasteboard>) -> Retained<FakeDrag> {
        let this = FakeDrag::alloc().set_ivars(FakeIvars { at: Cell::new(NSPoint::new(0.0, 0.0)), pb: RefCell::new(Some(pb)) });
        unsafe { msg_send![super(this), init] }
    }
}

/// An island point as the catcher's own point (what `draggingLocation` reports).
fn window_point(p: Point) -> NSPoint {
    with_target(|t| {
        let s = t.place.to_screen(p);
        let f = t.catcher().0.frame();
        NSPoint::new(s.x - f.origin.x, s.y - f.origin.y)
    })
    .unwrap()
}

/// A drag session: enters far out, moves into the zone at `p`, and lets go there. Returns whether the view accepted
/// the drop.
fn drag_and_drop(view: &super::DropView, pb: Retained<NSPasteboard>, p: Point) -> bool {
    let fake = FakeDrag::new(pb);
    fake.ivars().at.set(window_point(Point::new(200.0, 160.0)));
    let outside: NSDragOperation = unsafe { msg_send![view, draggingEntered: &*fake] };
    fake.ivars().at.set(window_point(p));
    let inside: NSDragOperation = unsafe { msg_send![view, draggingUpdated: &*fake] };
    let ready: bool = unsafe { msg_send![view, prepareForDragOperation: &*fake] };
    let done: bool = unsafe { msg_send![view, performDragOperation: &*fake] };
    outside == NSDragOperation::None && inside == NSDragOperation::Copy && ready && done
}

fn pasteboard(items: &[&[(&objc2_app_kit::NSPasteboardType, PbValue)]]) -> Retained<NSPasteboard> {
    let pb = NSPasteboard::pasteboardWithUniqueName();
    pb.clearContents();
    let written: Vec<Retained<ProtocolObject<dyn NSPasteboardWriting>>> = items
        .iter()
        .map(|types| {
            let item = NSPasteboardItem::new();
            for (t, v) in types.iter() {
                match v {
                    PbValue::Str(s) => {
                        item.setString_forType(&NSString::from_str(s), t);
                    }
                    PbValue::Data(d) => {
                        item.setData_forType(&objc2_foundation::NSData::with_bytes(d), t);
                    }
                }
            }
            ProtocolObject::from_retained(item)
        })
        .collect();
    pb.writeObjects(&NSArray::from_retained_slice(&written));
    pb
}

enum PbValue {
    Str(String),
    Data(Vec<u8>),
}

fn file_url(p: &Path) -> PbValue {
    let u = NSURL::fileURLWithPath(&NSString::from_str(&p.to_string_lossy()));
    PbValue::Str(u.absoluteString().map(|s| s.to_string()).unwrap_or_default())
}

/// Both clocks on by `dt`: the drop target's and the island's.
fn advance(dt: f64) {
    let steps = (dt / (1.0 / 120.0)).round().max(1.0) as usize;
    for _ in 0..steps {
        with_target(|t| {
            let now = t.clock.unwrap_or(0.0) + dt / steps as f64;
            t.clock = Some(now);
            t.after(now);
        });
        with_host(|h| {
            let now = h.clock.unwrap_or(0.0) + dt / steps as f64;
            h.clock = Some(now);
            h.apply(Event::Tick);
        });
    }
}

fn taken(actions: &RefCell<Vec<UiAction>>) -> Option<Dropped> {
    super::super::island::checks::spin_until(1.0, || actions.borrow().iter().any(|a| matches!(a, UiAction::Dropped(_))));
    actions.borrow_mut().drain(..).filter_map(|a| if let UiAction::Dropped(d) = a { Some(d) } else { None }).last()
}

/// A frame: the desktop crop, the island's window, the drop target's window, cropped round the notch.
struct Sheet {
    bg: Canvas,
    x0: f64,
    center_x: f64,
}

const CROP_W: f64 = 520.0;
const CROP_H: f64 = 150.0;

impl Sheet {
    fn frame(&self) -> Option<Canvas> {
        let w = self.bg.pw as f64 / self.bg.scale;
        let h = self.bg.ph as f64 / self.bg.scale;
        let out = Canvas::new(w, h, self.bg.scale)?;
        let img = self.bg.image()?;
        objc2_core_graphics::CGContext::save_g_state(Some(&out.ctx));
        objc2_core_graphics::CGContext::translate_ctm(Some(&out.ctx), 0.0, h);
        objc2_core_graphics::CGContext::scale_ctm(Some(&out.ctx), 1.0, -1.0);
        objc2_core_graphics::CGContext::draw_image(Some(&out.ctx), cg(0.0, 0.0, w, h), Some(&img));
        objc2_core_graphics::CGContext::restore_g_state(Some(&out.ctx));
        let island_x = self.center_x - super::super::island::host::WINDOW_W / 2.0 - self.x0;
        with_host(|hst| hst.tree.render_into(&out, (island_x, 0.0)));
        let drop_x = self.center_x - PANEL_W / 2.0 - self.x0;
        with_target(|t| t.render_into(&out, (drop_x, 0.0)));
        Some(out)
    }

    /// The frame cropped round the notch, `w` × `h` points, as an image.
    fn crop_to(&self, frame: &Canvas, w: f64, h: f64) -> Option<objc2_core_foundation::CFRetained<objc2_core_graphics::CGImage>> {
        let img = frame.image()?;
        let s = frame.scale;
        let x = (self.center_x - self.x0 - w / 2.0) * s;
        objc2_core_graphics::CGImage::with_image_in_rect(Some(&img), cg(x, 0.0, w * s, h * s))
    }

    /// The frame cropped round the notch, as an image.
    fn crop(&self, frame: &Canvas) -> Option<objc2_core_foundation::CFRetained<objc2_core_graphics::CGImage>> {
        let img = frame.image()?;
        let s = frame.scale;
        let x = (self.center_x - self.x0 - CROP_W / 2.0) * s;
        objc2_core_graphics::CGImage::with_image_in_rect(Some(&img), cg(x, 0.0, CROP_W * s, CROP_H * s))
    }
}

fn cg(x: f64, y: f64, w: f64, h: f64) -> objc2_core_foundation::CGRect {
    objc2_core_foundation::CGRect::new(objc2_core_foundation::CGPoint::new(x, y), objc2_core_foundation::CGSize::new(w, h))
}

/// A sample screenshot to drop: a dark window with a sidebar, a title bar and a few coloured rows.
fn sample_image(_sheet: &Sheet, path: &Path) -> bool {
    let (w, h) = (360.0, 230.0);
    let Some(c) = Canvas::new(w, h, 2.0) else { return false };
    c.draw(|| {
        let fill = |r: f64, g: f64, b: f64, x: f64, y: f64, w: f64, h: f64, rad: f64| {
            NSColor::colorWithSRGBRed_green_blue_alpha(r, g, b, 1.0).setFill();
            objc2_app_kit::NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(super::super::util::rect(x, y, w, h), rad, rad).fill();
        };
        fill(0.11, 0.11, 0.12, 0.0, 0.0, w, h, 0.0);
        fill(0.16, 0.16, 0.17, 0.0, 0.0, 92.0, h, 0.0);
        for (i, (r, g, b)) in [(1.0, 0.37, 0.34), (1.0, 0.74, 0.18), (0.16, 0.78, 0.25)].iter().enumerate() {
            fill(*r, *g, *b, 12.0 + i as f64 * 14.0, 12.0, 9.0, 9.0, 4.5);
        }
        for i in 0..5 {
            fill(0.28, 0.28, 0.30, 12.0, 40.0 + i as f64 * 22.0, 64.0, 10.0, 5.0);
        }
        let rows = [(0.04, 0.52, 1.0, 180.0), (0.75, 0.75, 0.78, 220.0), (0.75, 0.75, 0.78, 160.0), (0.55, 0.36, 0.96, 200.0), (0.75, 0.75, 0.78, 120.0)];
        for (i, (r, g, b, len)) in rows.iter().enumerate() {
            fill(*r, *g, *b, 110.0, 40.0 + i as f64 * 30.0, *len, 12.0, 6.0);
        }
    });
    c.png().is_some_and(|png| std::fs::write(path, png).is_ok())
}

pub(crate) fn run(dir: &Path) -> Vec<String> {
    let mtm = MainThreadMarker::new().expect("render_drop runs on the main thread");
    objc2_app_kit::NSApplication::sharedApplication(mtm).setActivationPolicy(objc2_app_kit::NSApplicationActivationPolicy::Prohibited);
    std::fs::create_dir_all(dir).ok();
    let lab = dir.join("drop-lab");
    std::fs::remove_dir_all(&lab).ok();
    std::fs::create_dir_all(lab.join("Folder/inner")).ok();
    let drops = lab.join("drops");
    // The drops folder of this run alone (never the user's).
    std::env::set_var("HEIMDALL_DROPS_DIR", &drops);
    let mut c = Checks::new();
    let actions: std::rc::Rc<RefCell<Vec<UiAction>>> = Default::default();
    let sink = actions.clone();
    super::super::island::set_sink(Box::new(move |a| sink.borrow_mut().push(a)));
    super::super::island::install_offscreen(mtm);
    super::install(mtm, false);

    // The user's display and desktop.
    let d = super::super::gallery::notch_display(mtm);
    let Ok((bg, source)) = super::super::gallery::render_background(&d, true) else {
        c.check(false, "a desktop to render over");
        return c.failed;
    };
    c.note(&format!("display: {} ({}); desktop: {source}", d.name, if d.display.has_notch() { "notch" } else { "no notch" }));
    let (x0, _) = super::super::gallery::crop(&d);
    let sheet = Sheet { bg, x0, center_x: d.center_x };
    with_host(|h| {
        h.place.display = d.display;
        let now = h.now();
        h.island.apply(Event::Display(d.display), now);
    });
    with_target(|t| {
        t.place.display = d.display;
        t.motion.set_display(d.display);
    });

    // The windows: titled for the capture filters, unshared, above the island's click target, never shown at rest. The
    // drag goes to the catcher, never to the drawn window: the drawn one joins the overlay space, where macOS offers no
    // drag ("There is no animation").
    let (panel, drawn_view) = with_target(|t| (t.panel(), t.view.clone())).unwrap();
    let (catcher, view) = with_target(|t| t.catcher()).unwrap();
    for (w, name) in [(&panel, "drawn window"), (&catcher, "catcher")] {
        c.check(w.title().to_string() == WINDOW_TITLE, &format!("{name}: titled for the capture filters"));
        c.check(w.sharingType() == NSWindowSharingType::None, &format!("{name}: unshared"));
        c.check(w.level() == NSMainMenuWindowLevel + 5, &format!("{name}: above the island and its click target"));
        c.check(w.ignoresMouseEvents(), &format!("{name}: click-through before any drag"));
    }
    let island_frame = with_host(|h| h.panels().0.frame()).unwrap();
    c.check(panel.frame() == island_frame, "the drawn window: the island window's frame");
    let cf = catcher.frame();
    c.check(cf.size.width == super::ZONE_W && cf.size.height == super::ZONE_H && (cf.origin.x + cf.size.width / 2.0 - island_frame.origin.x - island_frame.size.width / 2.0).abs() < 0.01, "the catcher: the notch's drop zone alone while the island rests");
    c.check(!panel.isVisible() && !catcher.isVisible() && with_target(|t| t.idle()).unwrap(), "ordered out with nothing running before any drag");
    let registered = |v: &super::DropView| v.registeredDraggedTypes().count();
    c.check(registered(&view) > 0 && registered(&drawn_view) == 0, "only the catcher is registered for drags");

    // Files of every kind.
    let shot = lab.join("Screenshot 2026-10-24 at 01.45.01.png");
    c.check(sample_image(&sheet, &shot), "a sample screenshot written");
    let clip = lab.join("clip.mov");
    let notes = lab.join("notes.md");
    std::fs::write(&clip, b"not really a movie").ok();
    std::fs::write(&notes, "# Notes\n\nDrop me on the notch.\n").ok();
    std::fs::write(lab.join("Folder/a.txt"), "a").ok();
    let folder = lab.join("Folder");
    let file_t = unsafe { NSPasteboardTypeFileURL };
    let url_t = unsafe { NSPasteboardTypeURL };
    let str_t = unsafe { NSPasteboardTypeString };
    let png_t = unsafe { NSPasteboardTypePNG };

    let cases: Vec<(&str, Retained<NSPasteboard>, Vec<DropKind>, &str)> = vec![
        ("an image file", pasteboard(&[&[(file_t, file_url(&shot)), (str_t, PbValue::Str("Screenshot".into()))]]), vec![DropKind::Image], "1 image"),
        ("a video", pasteboard(&[&[(file_t, file_url(&clip))]]), vec![DropKind::Video], "Video"),
        ("a folder", pasteboard(&[&[(file_t, file_url(&folder))]]), vec![DropKind::Folder], "Folder"),
        ("a document", pasteboard(&[&[(file_t, file_url(&notes))]]), vec![DropKind::Document], "Document"),
        ("a link", pasteboard(&[&[(url_t, PbValue::Str("https://www.apple.com/macbook-pro/".into())), (str_t, PbValue::Str("MacBook Pro".into()))]]), vec![DropKind::Link], "Link"),
        ("selected text", pasteboard(&[&[(str_t, PbValue::Str("Heimdall should see this sentence.".into()))]]), vec![DropKind::Text], "Text"),
        ("a screenshot as data", pasteboard(&[&[(png_t, PbValue::Data(std::fs::read(&shot).unwrap_or_default()))]]), vec![DropKind::Image], "1 image"),
        (
            "several items",
            pasteboard(&[&[(file_t, file_url(&shot))], &[(file_t, file_url(&folder))], &[(file_t, file_url(&notes))]]),
            vec![DropKind::Image, DropKind::Folder, DropKind::Document],
            "3 items",
        ),
    ];
    for (what, pb, kinds, label) in cases {
        with_target(|t| t.arm());
        let ok = drag_and_drop(&view, pb.clone(), Point::new(60.0, 60.0));
        c.check(ok, &format!("{what}: the view takes the drop through its drag callbacks"));
        let got = taken(&actions);
        let got_kinds: Vec<DropKind> = got.as_ref().map(|d| d.items.iter().map(|i| i.kind).collect()).unwrap_or_default();
        c.check(got_kinds == kinds, &format!("{what}: the app gets {kinds:?} (got {got_kinds:?})"));
        if what == "a screenshot as data" {
            let saved = got.as_ref().and_then(|d| d.items[0].path.clone());
            c.check(saved.as_ref().is_some_and(|p| p.starts_with(&drops) && p.exists()), &format!("the screenshot data is saved in the drops folder ({saved:?})"));
        }
        if what == "a link" {
            c.check(got.as_ref().and_then(|d| d.items[0].url.clone()).as_deref() == Some("https://www.apple.com/macbook-pro/"), "the link is the URL itself, not its title");
        }
        with_target(|t| t.disarm());
        advance(0.45);
        let label_shown = with_host(|h| {
            let now = h.now();
            h.island.frame(now).slots.iter().any(|s| matches!(s.content.as_ref(), crate::island::object::Content::Text { lines, .. } if lines.join(" ").contains(label)))
        })
        .unwrap_or(false);
        c.check(label_shown, &format!("{what}: the island confirms \"{label}\""));
        advance(4.0);
        c.check(with_target(|t| t.idle() && t.motion.at_rest(t.now())).unwrap(), &format!("{what}: back to rest, window out, no clock"));
    }

    // A drag that passes by: swells in the zone, back into the notch outside it, never takes anything.
    with_target(|t| t.arm());
    let pb = pasteboard(&[&[(str_t, PbValue::Str("passing by".into()))]]);
    let fake = FakeDrag::new(pb.clone());
    fake.ivars().at.set(window_point(Point::new(0.0, 30.0)));
    let _: NSDragOperation = unsafe { msg_send![&*view, draggingEntered: &*fake] };
    advance(0.5);
    let swollen = with_target(|t| t.motion.frame(t.now())).unwrap();
    c.check(swollen.glow > 0.9 && swollen.shape.w > d.display.notch_size().0 + 10.0, "a drag over the notch swells it and lights it");
    let _: () = unsafe { msg_send![&*view, draggingExited: &*fake] };
    with_target(|t| t.disarm());
    advance(1.0);
    c.check(taken(&actions).is_none(), "a drag that leaves sends nothing");
    c.check(with_target(|t| t.idle()).unwrap(), "and the window goes");

    // Over the notch, then released just outside the zone with no exit callback: never left swollen or taking clicks.
    with_target(|t| t.arm());
    c.check(!catcher.ignoresMouseEvents() && catcher.isVisible(), "armed for a drag: the catcher is up and takes it");
    c.check(panel.ignoresMouseEvents(), "armed for a drag: the drawn window still never takes the mouse");
    let pb = pasteboard(&[&[(str_t, PbValue::Str("almost".into()))]]);
    let fake = FakeDrag::new(pb.clone());
    fake.ivars().at.set(window_point(Point::new(0.0, 40.0)));
    let _: NSDragOperation = unsafe { msg_send![&*view, draggingEntered: &*fake] };
    advance(0.3);
    fake.ivars().at.set(window_point(Point::new(200.0, 120.0)));
    let _: () = unsafe { msg_send![&*view, draggingEnded: &*fake] };
    c.check(catcher.ignoresMouseEvents() && !catcher.isVisible(), "the drag ended: the catcher goes at once, clicks pass through");
    advance(1.0);
    c.check(with_target(|t| t.idle() && t.motion.at_rest(t.now())).unwrap() && taken(&actions).is_none(), "released outside after hovering: back to rest, nothing sent");

    // The contact sheet: hover, the drop, the swallow and the island's word; Reduce Motion last.
    let mut rows: Vec<(String, Vec<(f64, objc2_core_foundation::CFRetained<objc2_core_graphics::CGImage>)>)> = vec![];
    for reduce in [false, true] {
        with_target(|t| t.motion.set_reduce_motion(reduce));
        let grab = |row: &mut Vec<_>, t0: f64| {
            let now = with_target(|t| t.now()).unwrap();
            if let Some(f) = sheet.frame() {
                if let Some(img) = sheet.crop(&f) {
                    row.push((now - t0, img));
                }
                if row.len() == 5 && !reduce {
                    std::fs::write(dir.join("drop-hover.png"), f.png().unwrap_or_default()).ok();
                }
            }
        };
        let mut hover = vec![];
        with_target(|t| t.arm());
        let t0 = with_target(|t| t.now()).unwrap();
        grab(&mut hover, t0);
        let pb = pasteboard(&[&[(file_t, file_url(&shot))]]);
        let fake = FakeDrag::new(pb.clone());
        fake.ivars().at.set(window_point(Point::new(140.0, 86.0)));
        let _: NSDragOperation = unsafe { msg_send![&*view, draggingEntered: &*fake] };
        for dt in if reduce { vec![0.08, 0.3] } else { vec![0.04, 0.05, 0.08, 0.25] } {
            advance(dt);
            grab(&mut hover, t0);
        }
        let t1 = with_target(|t| t.now()).unwrap();
        let _: bool = unsafe { msg_send![&*view, performDragOperation: &*fake] };
        with_target(|t| t.disarm());
        let mut flight = vec![];
        grab(&mut flight, t1);
        let steps: Vec<f64> = if reduce { vec![0.1, 0.12, 0.2, 0.5] } else { vec![0.06, 0.06, 0.06, 0.06, 0.06, 0.06, 0.08, 0.1, 0.12, 0.2, 0.5] };
        for dt in steps {
            advance(dt);
            grab(&mut flight, t1);
        }
        let name = if reduce { "Reduce Motion" } else { "" };
        if reduce {
            let mut all = hover;
            all.extend(flight);
            rows.push((name.into(), all));
        } else {
            rows.push(("hover".into(), hover));
            let (a, b) = flight.split_at(6.min(flight.len()));
            rows.push(("drop".into(), a.to_vec()));
            rows.push(("swallow".into(), b.to_vec()));
        }
        advance(4.0);
        let _ = taken(&actions);
    }
    match write_sheet(&rows, &dir.join("drop-sheet.png")) {
        Ok(n) => c.check(n >= 20, &format!("contact sheet: {n} frames in drop-sheet.png")),
        Err(e) => c.check(false, &format!("contact sheet ({e})")),
    }
    with_target(|t| t.motion.set_reduce_motion(false));
    island_states(&mut c, &sheet, &view, &actions, &shot, dir);
    std::fs::write(dir.join("drop-checks.txt"), &c.report).ok();
    std::fs::remove_dir_all(&lab).ok();
    c.failed
}

/// A drag entering at `p` (island points), let go there. Whether the catcher took it.
fn drop_at(view: &super::DropView, pb: Retained<NSPasteboard>, p: Point) -> bool {
    let fake = FakeDrag::new(pb);
    fake.ivars().at.set(window_point(p));
    let over: NSDragOperation = unsafe { msg_send![view, draggingEntered: &*fake] };
    advance(0.2);
    let ready: bool = unsafe { msg_send![view, prepareForDragOperation: &*fake] };
    let done: bool = unsafe { msg_send![view, performDragOperation: &*fake] };
    let _: () = unsafe { msg_send![view, draggingEnded: &*fake] };
    over == NSDragOperation::Copy && ready && done
}

/// What the app got since the last call (after the main queue's turn the drop target hands it on).
fn sent(actions: &RefCell<Vec<UiAction>>) -> Vec<UiAction> {
    super::super::island::checks::spin_until(0.3, || false);
    std::mem::take(&mut *actions.borrow_mut()).into_iter().filter(|a| matches!(a, UiAction::Dropped(_) | UiAction::TypedWithDrop { .. } | UiAction::AgentWindow(_))).collect()
}

/// `heimdall-ui-demo --pulldown-drop <dir>` (30 Oct 03:00, the user: "With the pull-down open, I can't drop an
/// image/screenshot onto it"): the open pull-down, offscreen, through the real island, its click target and composer
/// field, and the drop target's catcher. While a drag is armed the click target lets drags through (it sat over the
/// catcher in the overlay space and took none, so the catcher was never offered the drag); a drop far below the notch's
/// own zone with the composer empty goes to the app as a notch drop; with the keyboard in it or words typed it goes into
/// the composer as a thumbnail, rides with Return as `UiAction::TypedWithDrop`, and its badge removes it. Renders the
/// pull-down with the attachment at the display's scale (`pulldown-drop-*.png`). Returns the failed checks.
pub(crate) fn run_pulldown(dir: &Path) -> Vec<String> {
    use crate::island::object::{gallery, Content, Target as Hit, Token};
    use objc2_app_kit::NSEventModifierFlags;
    let mtm = MainThreadMarker::new().expect("run_pulldown runs on the main thread");
    objc2_app_kit::NSApplication::sharedApplication(mtm).setActivationPolicy(objc2_app_kit::NSApplicationActivationPolicy::Prohibited);
    std::fs::create_dir_all(dir).ok();
    let lab = dir.join("pulldown-drop-lab");
    std::fs::remove_dir_all(&lab).ok();
    std::fs::create_dir_all(&lab).ok();
    // The drops folder of this run alone (never the user's).
    std::env::set_var("HEIMDALL_DROPS_DIR", lab.join("drops"));
    let mut c = Checks::new();
    let actions: std::rc::Rc<RefCell<Vec<UiAction>>> = Default::default();
    let sink = actions.clone();
    super::super::island::set_sink(Box::new(move |a| sink.borrow_mut().push(a)));
    super::super::island::install_offscreen(mtm);
    super::install(mtm, false);
    let d = super::super::gallery::notch_display(mtm);
    let Ok((bg, _)) = super::super::gallery::render_background(&d, true) else {
        c.check(false, "a desktop to render over");
        return c.failed;
    };
    let (x0, _) = super::super::gallery::crop(&d);
    let sheet = Sheet { bg, x0, center_x: d.center_x };
    with_host(|h| {
        h.place.display = d.display;
        let now = h.now();
        h.island.apply(Event::Display(d.display), now);
    });
    with_target(|t| {
        t.place.display = d.display;
        t.motion.set_display(d.display);
    });
    let shot = lab.join("Screenshot 2026-10-30 at 03.00.26.png");
    c.check(sample_image(&sheet, &shot), "a sample screenshot written");
    let file_t = unsafe { NSPasteboardTypeFileURL };
    let image_pb = || pasteboard(&[&[(file_t, file_url(&shot))]]);
    let control = with_host(|h| h.panels().1).expect("host");
    let field = with_host(|h| h.fields.composer()).flatten();
    let attached = || with_host(|h| h.island.composer_attached().to_vec()).unwrap_or_default();
    // A file from a temporary folder (where a screenshot's thumbnail lives) is copied into the drops folder first, under
    // a stamped name: an item is known by its file name's end.
    let is = |i: &crate::island::drop::DropItem, f: &Path| i.kind == DropKind::Image && i.path.as_ref().zip(f.file_name()).is_some_and(|(p, n)| p.to_string_lossy().ends_with(&*n.to_string_lossy()));
    let thumbs = || with_host(|h| h.island.frame(h.clock.unwrap_or(0.0)).slots.iter().filter(|s| !s.id.contains('~') && matches!(&*s.content, Content::Thumb { image: true, .. })).count()).unwrap_or(0);

    // The user's conversation pulled down by a click on the notch (not focused, empty composer).
    super::super::island::apply(Event::Chat(gallery::chat_day()));
    super::super::island::apply(Event::LastReply(gallery::CHAT_REPLY.into()));
    advance(0.5);
    c.check(super::super::island::checks::tap(&control, &Hit::Token(Token::Reply)), "the notch pulls the conversation down");
    advance(1.2);
    let shape = island_shape();
    c.check(shape.is_some_and(|s| s.h > 300.0), &format!("the pull-down is open ({shape:?})"));
    let Some(s) = shape else { return c.failed };
    // The root cause: the click target covers the whole pull-down and takes the mouse; while a drag is armed it must
    // let the drag through to the catcher.
    let cf = control.frame();
    c.check(!control.ignoresMouseEvents() && cf.size.height >= s.h - 1.0, &format!("at rest the click target covers the pull-down ({:.0} × {:.0}) and takes clicks", cf.size.width, cf.size.height));
    with_target(|t| t.arm());
    c.check(control.ignoresMouseEvents(), "armed for a drag: the click target lets it through to the catcher");
    // Low in the pull-down, by the composer: far outside the notch's own 340 × 100 zone.
    let low = Point::new(-s.w / 2.0 + 60.0, s.h - 60.0);
    c.check(low.y > super::ZONE_H + 100.0, &format!("the drop point is {:.0} pt below the notch's own zone", low.y - super::ZONE_H));
    c.check(drop_at(&view_of(), image_pb(), low), "empty composer: the catcher takes a drop low on the pull-down");
    c.check(!control.ignoresMouseEvents(), "the drag ended: the click target takes clicks again");
    let got = sent(&actions);
    let notch_drop = got.iter().any(|a| matches!(a, UiAction::Dropped(d) if d.items.len() == 1 && d.items[0].kind == DropKind::Image && is(&d.items[0], &shot)));
    c.check(notch_drop && attached().is_empty(), &format!("empty composer, no keyboard: sent at once like a notch drop ({got:?})"));
    advance(4.0);

    // He types: the drop goes into the composer.
    with_host(|h| h.fields.focus_composer());
    advance(0.3);
    let focused = with_host(|h| h.fields.composer_focused()).unwrap_or(false);
    c.check(focused, "the composer has the keyboard");
    let typed = "what is in this screenshot?";
    super::super::island::checks::type_in(&field, typed, true);
    advance(0.4);
    with_target(|t| t.arm());
    c.check(drop_at(&view_of(), image_pb(), low), "typing: the catcher takes the drop on the pull-down");
    let got = sent(&actions);
    c.check(got.is_empty(), &format!("nothing goes to the agent before he sends ({got:?})"));
    advance(1.2);
    c.check(attached().len() == 1 && is(&attached()[0], &shot), &format!("the image is attached to the message ({:?})", attached()));
    c.check(thumbs() == 1, "the composer shows its thumbnail");
    c.check(field.as_ref().is_some_and(|f| f.stringValue().to_string() == typed && f.currentEditor().is_some()), "his words stay in the field, the keyboard with them");
    let full = dir.join("pulldown-drop-attached.png");
    c.check(super::super::island::checks::composite(&full, None), "rendered: the pull-down with the attached screenshot");
    c.check(super::super::island::checks::composite(&dir.join("pulldown-drop-attached-2x.png"), Some(4.0)), "and its composer at 2x");
    if let Some(f) = sheet.frame() {
        if let Some(img) = sheet.crop_to(&f, ISLAND_CROP_W, s.h + 40.0) {
            let c2 = Canvas::new(ISLAND_CROP_W, s.h + 40.0, f.scale);
            if let Some(c2) = c2 {
                // The canvas runs y down: flipped back for CoreGraphics' image drawing.
                objc2_core_graphics::CGContext::translate_ctm(Some(&c2.ctx), 0.0, s.h + 40.0);
                objc2_core_graphics::CGContext::scale_ctm(Some(&c2.ctx), 1.0, -1.0);
                objc2_core_graphics::CGContext::draw_image(Some(&c2.ctx), cg(0.0, 0.0, ISLAND_CROP_W, s.h + 40.0), Some(&img));
                std::fs::write(dir.join("pulldown-drop-desktop.png"), c2.png().unwrap_or_default()).ok();
            }
        }
    }
    // Return sends his words with the image, once.
    control.sendEvent(&super::super::island::checks::key(36, "\r", NSEventModifierFlags::empty(), control.windowNumber()));
    advance(0.3);
    let got = sent(&actions);
    let with_drop = got.iter().any(|a| matches!(a, UiAction::TypedWithDrop { text, dropped } if text == typed && dropped.items.len() == 1 && is(&dropped.items[0], &shot)));
    c.check(with_drop && got.len() == 1, &format!("Return: his words and the image go as one message ({got:?})"));
    advance(1.0);
    c.check(attached().is_empty() && thumbs() == 0 && field.as_ref().is_some_and(|f| f.stringValue().to_string().is_empty()), "sent: the composer is empty again");

    // The keyboard in the empty composer: a drop attaches; its badge removes one; Return sends the rest alone.
    with_host(|h| h.fields.focus_composer());
    advance(0.3);
    with_target(|t| t.arm());
    drop_at(&view_of(), image_pb(), low);
    advance(0.5);
    let second = lab.join("second.png");
    std::fs::copy(&shot, &second).ok();
    with_target(|t| t.arm());
    drop_at(&view_of(), pasteboard(&[&[(file_t, file_url(&second))]]), low);
    advance(1.2);
    c.check(sent(&actions).is_empty() && attached().len() == 2, &format!("focused and empty: drops attach ({} attached)", attached().len()));
    // Settled (a tap during a motion takes only targets that stay put).
    advance(2.0);
    c.check(super::super::island::checks::tap(&control, &Hit::ComposerDetach(0)), "the first thumbnail's badge is a target");
    advance(1.0);
    c.check(attached().len() == 1 && is(&attached()[0], &second), &format!("its badge removed it ({:?})", attached()));
    c.check(super::super::island::checks::composite(&dir.join("pulldown-drop-no-words.png"), None), "rendered: an attachment with no words");
    control.sendEvent(&super::super::island::checks::key(36, "\r", NSEventModifierFlags::empty(), control.windowNumber()));
    advance(0.3);
    let got = sent(&actions);
    c.check(got.iter().any(|a| matches!(a, UiAction::TypedWithDrop { text, dropped } if text.is_empty() && dropped.items.len() == 1 && is(&dropped.items[0], &second))), &format!("Return with no words sends the drop ({got:?})"));
    std::fs::write(dir.join("pulldown-drop-checks.txt"), &c.report).ok();
    std::fs::remove_dir_all(&lab).ok();
    c.failed
}

fn view_of() -> Retained<super::DropView> {
    with_target(|t| t.catcher().1).expect("the drop target")
}

/// `heimdall-ui-demo --drop-live <seconds>`: the real drop target and island in this process, watching real drags for
/// `seconds`, the drawn window in the overlay space as in the installed app and shared (a screenshot shows it). Every
/// drop is printed, never sent anywhere; dropped data goes to a temporary folder. `lab/drop-on-notch-live/` drives a
/// Finder drag onto it. It is a second Heimdall over the notch: only while the user is away from the Mac.
pub(crate) fn live(seconds: f64) -> ! {
    let mtm = MainThreadMarker::new().expect("--drop-live runs on the main thread");
    let app = objc2_app_kit::NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(objc2_app_kit::NSApplicationActivationPolicy::Accessory);
    std::env::set_var("HEIMDALL_DROPS_DIR", std::env::temp_dir().join("heimdall-drop-live"));
    super::super::streams::set(true);
    super::super::island::install(mtm);
    super::super::island::set_sink(Box::new(|a| {
        if let UiAction::Dropped(d) = a {
            for i in &d.items {
                println!("dropped: {:?} {:?}{}", i.kind, i.path.as_deref().or(i.url.as_deref().map(Path::new)), i.text.as_ref().map_or(String::new(), |t| format!(" {t:?}")));
            }
        }
    }));
    super::install(mtm, true);
    println!("drop-live: watching drags for {seconds} s");
    // The overlay space goes with the process: released on the main thread, then exit.
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs_f64(seconds));
        dispatch2::DispatchQueue::main().exec_async(|| {
            super::super::overlay_space::release();
            std::process::exit(0);
        });
    });
    app.run();
    std::process::exit(0)
}

/// The island's shape now, when it shows.
fn island_shape() -> Option<crate::island::object::Shape> {
    with_host(|h| {
        let f = h.island.frame(h.now());
        f.visible.then_some(f.shape)
    })
    .flatten()
}

/// A shape's outline as drawn: its bounds (points, the flares included).
fn bounds(s: crate::island::object::Shape) -> (f64, f64) {
    let p = super::super::island::paint::outline(&s, 0.0);
    let b = objc2_core_graphics::CGPath::bounding_box(Some(&p));
    (b.size.width, b.size.height)
}

/// What is drawn for the drop now, read from the layers' own paths: the black shape's bounds and who draws it (true:
/// the island's tree; false: the drop window).
fn drawn() -> (f64, f64, bool) {
    let by_island = with_target(|t| t.layers.shape.isHidden()).unwrap();
    if by_island {
        let (w, h) = with_host(|h| h.tree.shape_size()).unwrap();
        return (w, h, true);
    }
    with_target(|t| {
        let b = t.layers.shape.path().map(|p| objc2_core_graphics::CGPath::bounding_box(Some(&p))).unwrap_or(objc2_core_foundation::CGRect::ZERO);
        (b.size.width, b.size.height, false)
    })
    .unwrap()
}

/// Some speech: the voice and its words, kept alive.
fn speak(text: &str) {
    super::super::island::apply(Event::Speaking(true));
    super::super::island::apply(Event::SpokenText(Some(text.into())));
    super::super::island::apply(Event::SpeechLevel(0.55));
}

/// Over the island while it shows: each state with the hover off and on (the swell and glow on the island's own shape,
/// none of the drop window's, the catcher over the island's frame), a drag held as the island grows and retracts
/// under it, and a drop onto the open conversation. `drop-island-sheet.png`.
fn island_states(c: &mut Checks, sheet: &Sheet, view: &super::DropView, actions: &RefCell<Vec<UiAction>>, shot: &Path, dir: &Path) {
    use crate::island::object::{gallery, Target as Hit, Token};
    let str_t = unsafe { NSPasteboardTypeString };
    let file_t = unsafe { NSPasteboardTypeFileURL };
    let mut rows: Vec<(String, Vec<(String, objc2_core_foundation::CFRetained<objc2_core_graphics::CGImage>)>, f64)> = vec![];
    let grab = |label: &str, h: f64, row: &mut Vec<(String, _)>| {
        if let Some(img) = sheet.frame().and_then(|f| sheet.crop_to(&f, ISLAND_CROP_W, h)) {
            row.push((label.to_string(), img));
        }
    };
    let states: [(&str, f64); 4] = [("collapsed", 150.0), ("speaking", 230.0), ("a line", 230.0), ("conversation", 620.0)];
    for (name, crop_h) in states {
        let enter: Box<dyn Fn()> = match name {
            "speaking" => Box::new(|| speak("The build is green, and Dave pushed the audio fix an hour ago.")),
            "a line" => Box::new(|| {
                super::super::island::apply(Event::Notice {
                    notice: crate::agent_notice::AgentNotice::line("drop-line", "Tests passed on main: 280 in the UI crate, the drop target included.", crate::agent_notice::NoticeKind::Info),
                    body: None,
                })
            }),
            "conversation" => Box::new(|| {
                super::super::island::apply(Event::Chat(gallery::chat_day()));
                super::super::island::apply(Event::LastReply(gallery::CHAT_REPLY.into()));
                super::super::island::apply(Event::Click(Hit::Token(Token::Reply)));
            }),
            _ => Box::new(|| {}),
        };
        enter();
        advance(0.9);
        if name == "speaking" {
            super::super::island::apply(Event::SpeechLevel(0.5));
        }
        let shape = island_shape();
        c.check(shape.is_some() == (name != "collapsed"), &format!("{name}: the island {}", if name == "collapsed" { "rests" } else { "shows" }));
        let mut row = vec![];
        grab("hover off", crop_h, &mut row);
        with_target(|t| t.arm());
        if let Some(s) = shape {
            // The island's frame in screen points (its window's centre line, its top edge) inside the catcher's.
            let (cf, cx, top) = with_target(|t| (t.catcher().0.frame(), t.place.center_x, t.place.top)).unwrap();
            let inside = cf.origin.x <= cx - s.w / 2.0 - 10.0 && cf.origin.x + cf.size.width >= cx + s.w / 2.0 + 10.0 && cf.origin.y <= top - s.h - 10.0 && (cf.origin.y + cf.size.height - top).abs() < 0.01;
            let snug = cf.size.width <= (s.w + 2.0 * super::ISLAND_MARGIN).max(super::ZONE_W) + 0.01 && cf.size.height <= (s.h + super::ISLAND_MARGIN).max(super::ZONE_H) + 0.01;
            c.check(inside && snug, &format!("{name}: the catcher covers the island's frame and the swell, no more ({:.0} × {:.0} for {:.0} × {:.0})", cf.size.width, cf.size.height, s.w, s.h));
        }
        // Into the island near its bottom edge: outside the notch's own zone when the island is tall.
        let p = Point::new(shape.map_or(0.0, |s| s.w / 2.0 - 30.0), shape.map_or(30.0, |s| s.h - 12.0));
        let fake = FakeDrag::new(pasteboard(&[&[(str_t, PbValue::Str("hover".into()))]]));
        fake.ivars().at.set(window_point(p));
        let op: NSDragOperation = unsafe { msg_send![view, draggingEntered: &*fake] };
        c.check(op == NSDragOperation::Copy, &format!("{name}: a drag anywhere over the island is taken (at {:.0}, {:.0})", p.x, p.y));
        advance(0.12);
        grab("hover 0.12 s", crop_h, &mut row);
        advance(0.5);
        if name == "speaking" {
            super::super::island::apply(Event::SpeechLevel(0.5));
        }
        let (w, h, by_island) = drawn();
        match island_shape() {
            Some(s) => {
                let (bw, bh) = bounds(s);
                c.check(by_island && (w - bw - drop::SWELL.0).abs() < 0.6 && (h - bh - drop::SWELL.1).abs() < 0.6, &format!("{name}: the island's own drawn shape swells by the swell ({bw:.0} × {bh:.0} → {w:.0} × {h:.0})"));
                let glow = with_host(|h| h.tree.drop.map_or(0.0, |d| d.glow)).unwrap_or(0.0);
                let own = with_target(|t| t.layers.shape.isHidden() && t.layers.glow.isHidden()).unwrap();
                c.check(glow > 0.99 && own, &format!("{name}: the island glows; the drop window draws no shape of its own"));
            }
            None => {
                let f = with_target(|t| t.motion.frame(t.now())).unwrap();
                let b = bounds(f.shape);
                c.check(!by_island && (f.shape.w - d_notch().0 - drop::SWELL.0).abs() < 0.6 && (w - b.0).abs() < 0.01 && (h - b.1).abs() < 0.01, &format!("collapsed: the notch swells as before, drawn by the drop window ({w:.1} × {h:.1}, {:.1} × {:.1})", b.0, b.1));
            }
        }
        grab("hover on", crop_h, &mut row);
        let _: () = unsafe { msg_send![view, draggingExited: &*fake] };
        advance(0.6);
        if name == "speaking" {
            super::super::island::apply(Event::SpeechLevel(0.5));
        }
        c.check(with_host(|h| h.tree.drop.is_none()).unwrap() && with_target(|t| t.motion.at_rest(t.now())).unwrap(), &format!("{name}: the drag left: the island back to its own shape, unlit"));
        grab("left", crop_h, &mut row);
        if name == "conversation" {
            // A drop onto the open conversation, low on it: taken, the item flies into the island, which swallows it.
            let fake = FakeDrag::new(pasteboard(&[&[(file_t, file_url(shot))]]));
            let s = island_shape().unwrap();
            let p = Point::new(-120.0, s.h - 60.0);
            fake.ivars().at.set(window_point(p));
            let _: NSDragOperation = unsafe { msg_send![view, draggingEntered: &*fake] };
            advance(0.4);
            let ok: bool = unsafe { msg_send![view, performDragOperation: &*fake] };
            c.check(ok && taken(actions).is_some(), &format!("conversation: a drop low on the open conversation is taken (at {:.0}, {:.0})", p.x, p.y));
            with_target(|t| t.disarm());
            advance(0.1);
            grab("dropped 0.1 s", crop_h, &mut row);
            advance(0.12);
            grab("swallow 0.22 s", crop_h, &mut row);
            advance(2.0);
            c.check(with_host(|h| h.tree.drop.is_none()).unwrap(), "conversation: after the swallow, the island is its own shape again");
            super::super::island::apply(Event::Click(Hit::ReplyBand));
        } else {
            with_target(|t| t.disarm());
        }
        rows.push((name.to_string(), row, crop_h));
        super::super::island::apply(Event::Stop);
        advance(6.0);
        let _ = taken(actions);
    }

    // Held over the notch while the island grows (Heimdall starts speaking) and retracts (the voice ends): one shape
    // throughout, no jump at either hand-over beyond the island's own motion.
    with_target(|t| t.arm());
    let fake = FakeDrag::new(pasteboard(&[&[(str_t, PbValue::Str("held".into()))]]));
    fake.ivars().at.set(window_point(Point::new(0.0, 24.0)));
    let _: NSDragOperation = unsafe { msg_send![view, draggingEntered: &*fake] };
    advance(0.5);
    let mut row = vec![];
    grab("swollen notch", 230.0, &mut row);
    let rest = with_target(|t| t.place.display.rest()).unwrap();
    let base = || bounds(island_shape().unwrap_or(rest));
    let (mut last, mut last_base) = (drawn(), base());
    let (mut worst, mut handovers, mut t) = (0.0f64, 0, 0.0f64);
    speak("Heimdall starts talking while the drag is held over the notch.");
    let marks: [f64; 4] = [0.03, 0.08, 0.16, 0.4];
    let mut seen_island = false;
    while t < 4.6 {
        advance(1.0 / 120.0);
        t += 1.0 / 120.0;
        if t < 3.0 && ((t * 120.0).round() as i64) % 30 == 0 {
            super::super::island::apply(Event::SpeechLevel(0.5));
        }
        if (2.9..2.9 + 1.0 / 120.0).contains(&t) {
            super::super::island::apply(Event::SpeechCut);
        }
        let now = drawn();
        let b = base();
        if now.2 != last.2 {
            handovers += 1;
        }
        seen_island |= now.2;
        let jump = ((now.0 - last.0).abs() - (b.0 - last_base.0).abs()).max((now.1 - last.1).abs() - (b.1 - last_base.1).abs());
        worst = worst.max(jump);
        if marks.iter().any(|m| (t - m).abs() < 0.5 / 120.0) {
            grab(&format!("speaks +{t:.2} s"), 230.0, &mut row);
        }
        if ((t - 3.0).abs() < 0.5 / 120.0) || ((t - 3.12).abs() < 0.5 / 120.0) || ((t - 4.5).abs() < 0.5 / 120.0) {
            grab(&format!("voice ends +{:.2} s", t - 2.9), 230.0, &mut row);
        }
        last = now;
        last_base = b;
    }
    c.check(seen_island && handovers == 2, &format!("held over the notch: the island takes the swell as it grows and hands it back as it retracts ({handovers} hand-overs)"));
    c.check(worst < 1.0, &format!("held over the notch: the drawn shape never jumps beyond the island's own motion (worst {worst:.2} pt a frame)"));
    let (w, _, by_island) = drawn();
    let f = with_target(|t| t.motion.frame(t.now())).unwrap();
    c.check(!by_island && (f.shape.w - d_notch().0 - drop::SWELL.0).abs() < 0.6 && w == bounds(f.shape).0, "held over the notch: back to the swollen notch after the voice");
    let _: () = unsafe { msg_send![view, draggingExited: &*fake] };
    with_target(|t| t.disarm());
    advance(1.0);
    c.check(with_target(|t| t.idle()).unwrap() && with_host(|h| h.tree.drop.is_none()).unwrap(), "held over the notch: after the drag, nothing up, nothing running");
    rows.push(("held while the island resizes".into(), row, 230.0));

    // The island speaking on another display while a drag comes to this notch: this notch swells by itself, the
    // other island keeps its own shape, and the catcher here is the notch's zone alone.
    speak("Speaking on the other display.");
    advance(0.9);
    let id = with_target(|t| t.place.id).unwrap();
    with_target(|t| t.place.id = id + 7);
    with_target(|t| t.arm());
    let fake = FakeDrag::new(pasteboard(&[&[(str_t, PbValue::Str("other display".into()))]]));
    fake.ivars().at.set(window_point(Point::new(0.0, 24.0)));
    let _: NSDragOperation = unsafe { msg_send![view, draggingEntered: &*fake] };
    advance(0.5);
    super::super::island::apply(Event::SpeechLevel(0.5));
    let (w, h, by_island) = drawn();
    let f = with_target(|t| t.motion.frame(t.now())).unwrap();
    let b = bounds(f.shape);
    let swollen = (f.shape.w - d_notch().0 - drop::SWELL.0).abs() < 0.6 && (w - b.0).abs() < 0.01 && (h - b.1).abs() < 0.01;
    let cf = with_target(|t| t.catcher().0.frame()).unwrap();
    let island_unswollen = with_host(|h| h.tree.drop.is_none()).unwrap() && island_shape().is_some();
    c.check(!by_island && swollen && island_unswollen && cf.size.width == super::ZONE_W && cf.size.height == super::ZONE_H, "an island on another display: this notch swells by itself, that island keeps its shape, the catcher is the notch's zone");
    let _: () = unsafe { msg_send![view, draggingExited: &*fake] };
    with_target(|t| t.disarm());
    // The drag stays; the island now shows on this display (it followed the pointer): it wears the swell at once.
    with_target(|t| t.place.id = id);
    with_target(|t| t.arm());
    let _: NSDragOperation = unsafe { msg_send![view, draggingEntered: &*fake] };
    with_target(|t| t.place.id = id + 7);
    advance(0.4);
    // The island moves to the drag's display between the drop target's ticks (its swell settled, its clock stopped):
    // only the island renders.
    with_host(|h| {
        h.place.id = id + 7;
        let now = h.now();
        h.render(now);
    });
    let (w, h, by_island) = drawn();
    let s = island_shape().map(bounds).unwrap_or_default();
    c.check(by_island && (w - s.0 - drop::SWELL.0).abs() < 0.6 && (h - s.1 - drop::SWELL.1).abs() < 0.6, &format!("the island arrives on the drag's display: it swells on its first frame there ({:.0} × {:.0} → {w:.0} × {h:.0})", s.0, s.1));
    let _: () = unsafe { msg_send![view, draggingExited: &*fake] };
    with_target(|t| t.disarm());
    with_host(|h| h.place.id = id);
    with_target(|t| t.place.id = id);
    super::super::island::apply(Event::Stop);
    advance(6.0);
    match write_island_sheet(&rows, &dir.join("drop-island-sheet.png")) {
        Ok(n) => c.check(n >= 20, &format!("island contact sheet: {n} frames in drop-island-sheet.png")),
        Err(e) => c.check(false, &format!("island contact sheet ({e})")),
    }
}

fn d_notch() -> (f64, f64) {
    with_target(|t| t.place.display.notch_size()).unwrap()
}

const ISLAND_CROP_W: f64 = 820.0;

/// Rows of labelled frames, each row its own height.
fn write_island_sheet(rows: &[(String, Vec<(String, objc2_core_foundation::CFRetained<objc2_core_graphics::CGImage>)>, f64)], path: &Path) -> Result<usize, String> {
    let cols = rows.iter().map(|r| r.1.len()).max().unwrap_or(0).min(4);
    let (cw, gap, label) = (ISLAND_CROP_W, 12.0, 22.0);
    let lines: Vec<(usize, f64)> = rows.iter().map(|r| (r.1.len().div_ceil(cols.max(1)), r.2)).collect();
    let w = cols as f64 * (cw + gap) + gap;
    let h = lines.iter().map(|(n, ch)| *n as f64 * (ch + label + gap)).sum::<f64>() + gap;
    let out = Canvas::new(w, h, 2.0).ok_or("no bitmap")?;
    out.draw(|| {
        NSColor::colorWithSRGBRed_green_blue_alpha(0.07, 0.07, 0.08, 1.0).setFill();
        objc2_app_kit::NSBezierPath::fillRect(super::super::util::rect(0.0, 0.0, w, h));
    });
    let (mut n, mut y0) = (0, gap);
    for (name, frames, ch) in rows {
        for (i, (text, img)) in frames.iter().enumerate() {
            let (col, row) = (i % cols, i / cols);
            let x = gap + col as f64 * (cw + gap);
            let y = y0 + row as f64 * (ch + label + gap);
            objc2_core_graphics::CGContext::save_g_state(Some(&out.ctx));
            objc2_core_graphics::CGContext::translate_ctm(Some(&out.ctx), x, y + ch);
            objc2_core_graphics::CGContext::scale_ctm(Some(&out.ctx), 1.0, -1.0);
            objc2_core_graphics::CGContext::draw_image(Some(&out.ctx), cg(0.0, 0.0, cw, *ch), Some(img));
            objc2_core_graphics::CGContext::restore_g_state(Some(&out.ctx));
            let text = if i == 0 { format!("{name}  ·  {text}") } else { text.clone() };
            out.draw(|| {
                let font = objc2_app_kit::NSFont::monospacedDigitSystemFontOfSize_weight(12.0, unsafe { objc2_app_kit::NSFontWeightRegular });
                let s = super::super::util::attributed(&text, &font, &NSColor::colorWithSRGBRed_green_blue_alpha(1.0, 1.0, 1.0, 0.6));
                s.drawAtPoint(NSPoint::new(x + 2.0, y + ch + 4.0));
            });
            n += 1;
        }
        y0 += frames.len().div_ceil(cols.max(1)) as f64 * (ch + label + gap);
    }
    std::fs::write(path, out.png().ok_or("PNG")?).map_err(|e| e.to_string())?;
    Ok(n)
}

/// Rows of frames, each labelled with its time since the row's start.
fn write_sheet(rows: &[(String, Vec<(f64, objc2_core_foundation::CFRetained<objc2_core_graphics::CGImage>)>)], path: &PathBuf) -> Result<usize, String> {
    let cols = rows.iter().map(|r| r.1.len()).max().unwrap_or(0).min(6);
    let lines: usize = rows.iter().map(|r| r.1.len().div_ceil(cols.max(1))).sum();
    let (cw, ch, gap, label) = (CROP_W, CROP_H, 10.0, 20.0);
    let w = cols as f64 * (cw + gap) + gap;
    let h = lines as f64 * (ch + label + gap) + gap;
    let out = Canvas::new(w, h, 2.0).ok_or("no bitmap")?;
    let mut n = 0;
    out.draw(|| {
        NSColor::colorWithSRGBRed_green_blue_alpha(0.07, 0.07, 0.08, 1.0).setFill();
        objc2_app_kit::NSBezierPath::fillRect(super::super::util::rect(0.0, 0.0, w, h));
    });
    let mut line = 0;
    for (name, frames) in rows {
        for (i, (t, img)) in frames.iter().enumerate() {
            let (col, row) = (i % cols, line + i / cols);
            let x = gap + col as f64 * (cw + gap);
            let y = gap + row as f64 * (ch + label + gap);
            objc2_core_graphics::CGContext::save_g_state(Some(&out.ctx));
            objc2_core_graphics::CGContext::translate_ctm(Some(&out.ctx), x, y + ch);
            objc2_core_graphics::CGContext::scale_ctm(Some(&out.ctx), 1.0, -1.0);
            objc2_core_graphics::CGContext::draw_image(Some(&out.ctx), cg(0.0, 0.0, cw, ch), Some(img));
            objc2_core_graphics::CGContext::restore_g_state(Some(&out.ctx));
            let text = if i == 0 && !name.is_empty() { format!("{name}  {t:.2} s") } else { format!("{t:.2} s") };
            out.draw(|| {
                let font = objc2_app_kit::NSFont::monospacedDigitSystemFontOfSize_weight(11.0, unsafe { objc2_app_kit::NSFontWeightRegular });
                let s = super::super::util::attributed(&text, &font, &NSColor::colorWithSRGBRed_green_blue_alpha(1.0, 1.0, 1.0, 0.55));
                s.drawAtPoint(NSPoint::new(x + 2.0, y + ch + 3.0));
            });
            n += 1;
        }
        line += frames.len().div_ceil(cols.max(1));
    }
    std::fs::write(path, out.png().ok_or("PNG")?).map_err(|e| e.to_string())?;
    Ok(n)
}
