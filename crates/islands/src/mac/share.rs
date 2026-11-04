//! The viewer's drag handle: the open file's own document icon in the title row, left of the close button (
//! "something at the top that I can drag into a chat, which copies the entire file"). Dragged, it hands any app a real
//! file (a file URL on the drag pasteboard: chats, Mail and Finder attach or copy it); clicked, it puts the same file on
//! the clipboard. An HTML page with local files goes as its self-contained copy (`model/share.rs`).
//!
//! At rest it is the quiet icon only; the pointer over it lifts it and shows a soft rounded backdrop (one tracking
//! area, events only, nothing runs at rest); a click turns it into a green check for a moment.

use std::cell::{Cell, RefCell};
use std::sync::{Arc, Mutex};
use std::path::{Path, PathBuf};

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{msg_send, AnyThread};
use objc2_app_kit::{
    NSDraggingItem, NSEvent, NSImage, NSPasteboard, NSPasteboardTypeFileURL, NSPasteboardWriting, NSTrackingArea, NSTrackingAreaOptions, NSView,
    NSWorkspace,
};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_foundation::{NSArray, NSString, NSURL};
use objc2_quartz_core::CALayer;

use super::look::*;
use super::sys::{self, Work1};
use crate::model::geometry::{Point, Rect};
use crate::model::share::{self, Shared};
use crate::model::Paths;

/// The handle's hit and hover box (points).
pub const SIZE: f64 = 26.0;
/// The document icon inside it.
const ICON: f64 = 20.0;
/// The icon's opacity at rest and under the pointer.
const REST: f32 = 0.45;
const LIFTED: f32 = 1.0;
/// Room the handle takes in the title row, left of the close button.
pub const ROOM: f64 = 30.0;

thread_local! {
    /// Lab: what the latest drag or copy handed over.
    pub static LAB_LAST: RefCell<Option<(String, PathBuf)>> = const { RefCell::new(None) };
    /// Lab: where the latest drag session ended (screen point) and the operation the target took (0: none).
    pub static LAB_ENDED: Cell<Option<(f64, f64, usize)>> = const { Cell::new(None) };
}

pub struct Handle {
    path: PathBuf,
    pub layer: Retained<CALayer>,
    backdrop: Retained<CALayer>,
    icon: Retained<CALayer>,
    check: Retained<CALayer>,
    tracking: RefCell<Option<Retained<NSTrackingArea>>>,
    hovered: std::rc::Rc<Cell<bool>>,
    /// Pressed at this view point; `None` once a drag began or the press ended.
    down: Cell<Option<Point>>,
    copied: RefCell<Option<Work1>>,
    /// The latest drag started a session.
    pub began: Cell<bool>,
    /// The latest copy made, with what it was made from: reused while those files are unchanged. Filled in the
    /// background when the pointer comes over the handle, so a drag of a large report starts at once.
    made: Arc<Mutex<Option<Made>>>,
}

struct Made {
    stamps: Vec<share::Stamp>,
    path: PathBuf,
}

/// A copy made earlier for `deps`, if they are unchanged and it is still there.
fn current(made: &Mutex<Option<Made>>) -> Option<PathBuf> {
    let m = made.lock().ok()?;
    let m = m.as_ref()?;
    let paths: Vec<PathBuf> = m.stamps.iter().map(|s| s.0.clone()).collect();
    (share::fingerprint(&paths) == m.stamps && m.path.exists()).then(|| m.path.clone())
}

fn make(path: &Path, made: &Mutex<Option<Made>>) -> std::io::Result<PathBuf> {
    // Stamped before it is read: a change while it is made shows as stale next time.
    let (s, deps) = share::shareable_with_deps(path, &Paths::shares())?;
    if let Shared::Bundled { inlined, .. } = &s {
        super::island::debug(&format!("share: {} files inlined into {}", inlined, s.path().display()));
    }
    let out = s.path().to_path_buf();
    if let Ok(mut m) = made.lock() {
        *m = Some(Made { stamps: share::fingerprint(&deps), path: out.clone() });
    }
    Ok(out)
}

/// The file's own icon as Finder draws it (an HTML page shows its browser's document icon).
pub fn file_icon(path: &Path) -> Retained<NSImage> {
    NSWorkspace::sharedWorkspace().iconForFile(&NSString::from_str(&path.to_string_lossy()))
}

fn image_layer(img: &NSImage, size: f64, scale: f64) -> Retained<CALayer> {
    let l = layer();
    l.setBounds(cgr(Rect::new(0.0, 0.0, size, size)));
    // SAFETY: a framework constant.
    l.setContentsGravity(unsafe { objc2_quartz_core::kCAGravityResizeAspect });
    l.setContentsScale(scale);
    let c = img.layerContentsForContentsScale(scale);
    // SAFETY: layer contents from NSImage for this scale.
    unsafe { l.setContents(Some(&c)) };
    l
}

impl Handle {
    pub fn new(path: &Path, scale: f64) -> Handle {
        let l = layer();
        l.setBounds(cgr(Rect::new(0.0, 0.0, SIZE, SIZE)));
        let backdrop = boxed(0.0, 0.0, SIZE, SIZE, &white(0.12), 7.0);
        backdrop.setOpacity(0.0);
        l.addSublayer(&backdrop);
        let icon = image_layer(&file_icon(path), ICON, scale);
        icon.setPosition(pt(SIZE / 2.0, SIZE / 2.0));
        icon.setOpacity(REST);
        l.addSublayer(&icon);
        let check = sym("checkmark", 15.0, &[green()], scale);
        check.setPosition(pt(SIZE / 2.0, SIZE / 2.0));
        check.setOpacity(0.0);
        l.addSublayer(&check);
        Handle {
            path: path.to_path_buf(),
            layer: l,
            backdrop,
            icon,
            check,
            tracking: RefCell::new(None),
            hovered: std::rc::Rc::new(Cell::new(false)),
            down: Cell::new(None),
            copied: RefCell::new(None),
            began: Cell::new(false),
            made: Arc::new(Mutex::new(None)),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The handle in the island view's coordinates (flipped), given the shape's frame there.
    pub fn frame_in_view(&self, shape: CGRect) -> Rect {
        rect_of(self.layer.frame()).offset(shape.origin.x, shape.origin.y)
    }

    /// Hover follows one tracking area over the handle (re-made when the island moves within its window or resizes).
    pub fn track(&self, view: &NSView, frame: Rect) {
        if let Some(t) = self.tracking.borrow_mut().take() {
            view.removeTrackingArea(&t);
        }
        let opts = NSTrackingAreaOptions::MouseEnteredAndExited | NSTrackingAreaOptions::ActiveAlways;
        // SAFETY: the view owns the area; it is removed with the handle.
        let t = unsafe {
            NSTrackingArea::initWithRect_options_owner_userInfo(NSTrackingArea::alloc(), ns_rect_of(frame), opts, Some(view), None)
        };
        view.addTrackingArea(&t);
        *self.tracking.borrow_mut() = Some(t);
    }

    pub fn untrack(&self, view: &NSView) {
        if let Some(t) = self.tracking.borrow_mut().take() {
            view.removeTrackingArea(&t);
        }
        self.set_hover(false);
    }

    pub fn set_hover(&self, on: bool) {
        if self.hovered.replace(on) == on {
            return;
        }
        if on && current(&self.made).is_none() {
            let (path, made) = (self.path.clone(), self.made.clone());
            std::thread::spawn(move || {
                let _ = make(&path, &made);
            });
        }
        fade(&self.backdrop, if on { 1.0 } else { 0.0 }, if on { 0.12 } else { 0.2 }, 0.0);
        if self.check.opacity() == 0.0 {
            fade(&self.icon, if on { LIFTED } else { REST }, if on { 0.12 } else { 0.2 }, 0.0);
        }
    }

    pub fn hovered(&self) -> bool {
        self.hovered.get()
    }

    pub fn press(&self, p: Point) {
        self.down.set(Some(p));
    }

    pub fn pressed(&self) -> bool {
        self.down.get().is_some()
    }

    /// A press moved: past 3 points it becomes the file drag (true: the handle took the event; `began` says whether a
    /// session started).
    pub fn dragged(&self, view: &NSView, e: &NSEvent, p: Point, frame: Rect) -> bool {
        let Some(d) = self.down.get() else { return false };
        if (p.x - d.x).hypot(p.y - d.y) < 3.0 {
            return true;
        }
        self.down.set(None);
        self.began.set(self.begin_drag(view, e, frame));
        true
    }

    /// The press ended without a drag: the file goes on the clipboard.
    pub fn released(&self) -> bool {
        if self.down.take().is_none() {
            return false;
        }
        self.copy();
        true
    }

    /// What goes out for the file now: its self-contained copy for an HTML page with local files, else the file.
    pub fn shared(&self) -> PathBuf {
        if let Some(p) = current(&self.made) {
            return p;
        }
        match make(&self.path, &self.made) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("share: {} ({e}); the file goes as it is", self.path.display());
                self.path.clone()
            }
        }
    }

    /// The real drag: one item, the file's URL, its icon as the drag image starting from the handle.
    pub fn begin_drag(&self, view: &NSView, e: &NSEvent, frame: Rect) -> bool {
        let path = self.shared();
        let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
        let writer: &ProtocolObject<dyn NSPasteboardWriting> = ProtocolObject::from_ref(&*url);
        let item = NSDraggingItem::initWithPasteboardWriter(NSDraggingItem::alloc(), writer);
        let img = file_icon(&path);
        let side = 40.0;
        let f = CGRect::new(CGPoint::new(frame.mid_x() - side / 2.0, frame.mid_y() - side / 2.0), CGSize::new(side, side));
        // SAFETY: an NSImage as the item's contents.
        unsafe { item.setDraggingFrame_contents(f, Some(&*img as &AnyObject)) };
        let Some(source) = super::panel::drag_source(view) else { return false };
        let items = NSArray::from_retained_slice(&[item]);
        // AppKit may refuse a session (another drag is still running): nothing is dragged then. Not the generated
        // binding, which panics on nil.
        // SAFETY: the documented signature; the result may be nil.
        let s: Option<Retained<objc2_app_kit::NSDraggingSession>> = unsafe { msg_send![view, beginDraggingSessionWithItems: &*items, event: e, source: source] };
        let Some(s) = s else {
            eprintln!("share: no drag session for {}", path.display());
            return false;
        };
        s.setAnimatesToStartingPositionsOnCancelOrFail(true);
        LAB_LAST.with(|l| *l.borrow_mut() = Some(("drag".into(), path)));
        self.set_hover(false);
        true
    }

    /// The file on the clipboard as a file URL (what Finder's Copy puts there and chats paste as a file), then a check.
    pub fn copy(&self) {
        let path = self.shared();
        let pb = pasteboard();
        write_file(&pb, &path);
        LAB_LAST.with(|l| *l.borrow_mut() = Some(("copy".into(), path)));
        fade(&self.icon, 0.0, 0.1, 0.0);
        fade(&self.check, 1.0, 0.1, 0.0);
        let (icon, check, hovered) = (self.icon.clone(), self.check.clone(), self.hovered.clone());
        let w = sys::after_cancellable(1.2, move || {
            fade(&check, 0.0, 0.25, 0.0);
            fade(&icon, if hovered.get() { LIFTED } else { REST }, 0.25, 0.0);
        });
        if let Some(old) = self.copied.borrow_mut().replace(w) {
            old.cancel();
        }
    }

    pub fn teardown(&self, view: &NSView) {
        self.untrack(view);
        if let Some(w) = self.copied.borrow_mut().take() {
            w.cancel();
        }
        self.layer.removeFromSuperlayer();
    }
}

fn ns_rect_of(r: Rect) -> CGRect {
    CGRect::new(CGPoint::new(r.x, r.y), CGSize::new(r.w, r.h))
}

/// The general clipboard; a lab run (FLOATING_ISLANDS_DIR) gets a pasteboard of its own, never the user's.
pub fn pasteboard() -> Retained<NSPasteboard> {
    match std::env::var("FLOATING_ISLANDS_DIR") {
        Ok(d) if !d.is_empty() => NSPasteboard::pasteboardWithName(&NSString::from_str("FloatingIslands.lab")),
        _ => NSPasteboard::generalPasteboard(),
    }
}

/// Puts `path` on `pb` as a file.
pub fn write_file(pb: &NSPasteboard, path: &Path) {
    pb.clearContents();
    let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
    let writer: Retained<ProtocolObject<dyn NSPasteboardWriting>> = ProtocolObject::from_retained(url);
    pb.writeObjects(&NSArray::from_retained_slice(&[writer]));
}

/// The file URLs on a pasteboard, as a drop target or a paste reads them.
pub fn read_files(pb: &NSPasteboard) -> Vec<PathBuf> {
    let Some(items) = pb.pasteboardItems() else { return vec![] };
    items
        .iter()
        // SAFETY: a framework constant.
        .filter_map(|i| i.stringForType(unsafe { NSPasteboardTypeFileURL }))
        .filter_map(|s| NSURL::URLWithString(&s))
        .filter_map(|u| u.path())
        .map(|p| PathBuf::from(p.to_string()))
        .collect()
}
