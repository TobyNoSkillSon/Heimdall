//! Friends ▸ Invite a friend… (personal edition): the invite's QR code on the island's black card, in a floating panel
//! under the notch where the other cards are. The panel is ordered in without activating Heimdall, so the user's app stays
//! in front. It closes on its X, on expiry or after pairing (the app sends `UiUpdate::FriendInvite(None)`). The invite
//! itself stays valid until it expires or is used; closing the panel does not burn it.
//!
//! The QR code is also a drag source: dragged out of the island, its PNG goes to the app it is dropped on as a file,
//! `heimdall-invite-<hex>.png` in the temp dir (mode 0600). The file is written when a drag starts (a press must travel
//! a few points first). It stays on disk for ten minutes after its drag or its panel's close, because the receiving app
//! may read it after the drop. Leftovers older than an hour are swept when an invite opens.

use std::cell::{Cell, RefCell};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant, SystemTime};

use dispatch2::{DispatchQueue, DispatchTime};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{define_class, msg_send, sel, AllocAnyThread as _, DefinedClass, MainThreadOnly};
use objc2_app_kit::{
    NSBackingStoreType, NSBox, NSBoxType, NSButton, NSColor, NSDragOperation, NSDraggingContext, NSDraggingItem,
    NSDraggingSession, NSDraggingSource, NSEvent, NSFont, NSFontWeightSemibold, NSImage, NSImageScaling, NSImageView,
    NSPanel, NSPasteboardItem, NSPasteboardTypeFileURL, NSPasteboardTypePNG, NSPasteboardWriting, NSTextAlignment,
    NSTitlePosition, NSWindowStyleMask,
};
use objc2_foundation::{MainThreadMarker, NSArray, NSData, NSObjectProtocol, NSPoint, NSURL};

use super::speaker::float;
use super::util::*;

/// The QR code's side on screen, in points: readable by a phone camera at arm's length.
pub const QR_PT: f64 = 232.0;
const PAD: f64 = 24.0;
const WIDTH: f64 = QR_PT + 2.0 * PAD;
const HEIGHT: f64 = PAD + QR_PT + 16.0 + 20.0 + 16.0;
/// The island's card corner, in points.
const CORNER: f64 = 22.0;
/// The drag's file: `heimdall-invite-<16 hex digits>.png` in the temp dir.
const FILE_PREFIX: &str = "heimdall-invite-";
const FILE_SUFFIX: &str = ".png";
/// How long a drag's file stays on disk after its drag or its panel's close: the receiving app may read it after a drop.
const RETAIN: Duration = Duration::from_secs(600);
/// Files of this prefix older than this are leftovers of an earlier run, and are swept when an invite opens.
const LEFTOVER_AGE: Duration = Duration::from_secs(3600);
/// Pointer travel, in points, before a press becomes a drag: a shaky click writes no file.
const DRAG_START_PT: f64 = 3.0;

#[derive(Default)]
pub struct InvitePanel {
    /// Render mode: the panel stays invisible and click-through.
    pub hidden: bool,
    current: Option<Retained<NSPanel>>,
    /// The QR view. It lives as long as the app, so a drag still in flight never outlives its source.
    qr: Option<Retained<InviteQr>>,
    files: Rc<RefCell<DragFiles>>,
}

impl InvitePanel {
    /// Shows `png` (the QR code) for `label`, replacing an open panel. Returns false when the PNG cannot be drawn.
    pub fn show(&mut self, label: &str, png: &[u8], target: &AnyObject, mtm: MainThreadMarker) -> bool {
        self.close();
        self.files.borrow_mut().set_invite(&std::env::temp_dir(), png, Instant::now());
        let qr = self.qr.get_or_insert_with(|| InviteQr::new(Rc::clone(&self.files), mtm));
        let Some(window) = build(label, png, qr, target, mtm) else { return false };
        float(&window);
        // Under the notch, where the other cards are.
        if let Some(screen) = pointer_screen(mtm) {
            let frame = screen.frame();
            window.setFrameOrigin(NSPoint::new(
                frame.origin.x + (frame.size.width - WIDTH) / 2.0,
                frame.origin.y + frame.size.height - HEIGHT - 90.0,
            ));
        }
        if self.hidden {
            window.setAlphaValue(0.0);
            window.setIgnoresMouseEvents(true);
        }
        // Ordered in, never made key: a borderless non-activating panel must not take focus from the user's app.
        window.orderFrontRegardless();
        self.current = Some(window);
        true
    }

    /// Closes the panel, if one is open. Its drag file stays ten minutes, then the main queue removes it.
    pub fn close(&mut self) {
        if let Some(window) = self.current.take() {
            window.orderOut(None);
        }
        self.files.borrow_mut().close(Instant::now());
        schedule_reap();
    }

    /// Removes the drag files whose retention is over. The main-queue timer from `schedule_reap` calls this.
    pub fn reap(&mut self) {
        self.files.borrow_mut().reap(Instant::now());
    }

    /// Render mode: the open panel's card, as PNG at `path`. False when no panel is open or the write fails.
    pub fn capture(&self, path: &Path, dark: bool) -> bool {
        let Some(view) = self.current.as_ref().and_then(|w| w.contentView()) else { return false };
        super::offscreen::capture_with(path, &view, dark, CORNER)
    }

    /// The active Space changed: an open panel is ordered in again on the Space the user is now on.
    pub fn space_changed(&self) {
        if let Some(window) = &self.current {
            if window.isVisible() {
                window.orderFrontRegardless();
            }
        }
    }
}

/// Runs the panel's `reap` on the main queue once the retention is over. Nothing is removed earlier than that.
fn schedule_reap() {
    if let Ok(when) = DispatchTime::try_from(RETAIN + Duration::from_secs(1)) {
        let _ = DispatchQueue::main().after(when, || super::with_state(|state| state.friend_invite.reap()));
    }
}

/// The card: a black island-style box with the friend's name over the QR code, and an X at its corner.
fn build(label: &str, png: &[u8], qr: &InviteQr, target: &AnyObject, mtm: MainThreadMarker) -> Option<Retained<NSPanel>> {
    let image = NSImage::initWithData(NSImage::alloc(), &NSData::with_bytes(png))?;
    let window = NSPanel::initWithContentRect_styleMask_backing_defer(
        NSPanel::alloc(mtm),
        rect(0.0, 0.0, WIDTH, HEIGHT),
        NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel,
        NSBackingStoreType::Buffered,
        false,
    );
    unsafe { window.setReleasedWhenClosed(false) };
    window.setOpaque(false);
    window.setBackgroundColor(Some(&NSColor::clearColor()));
    window.setHasShadow(true);

    let card = NSBox::initWithFrame(NSBox::alloc(mtm), rect(0.0, 0.0, WIDTH, HEIGHT));
    card.setBoxType(NSBoxType::Custom);
    card.setTitlePosition(NSTitlePosition::NoTitle);
    card.setBorderWidth(0.0);
    card.setCornerRadius(CORNER);
    card.setFillColor(&NSColor::blackColor());
    let content = card.contentView()?;

    qr.setImage(Some(&image));
    qr.setImageScaling(NSImageScaling::ScaleProportionallyUpOrDown);
    qr.setFrame(rect(PAD, PAD, QR_PT, QR_PT));
    content.addSubview(qr);

    let name = text_label(label, &NSFont::systemFontOfSize_weight(15.0, unsafe { NSFontWeightSemibold }), &NSColor::whiteColor(), mtm);
    name.setAlignment(NSTextAlignment::Center);
    name.setFrame(rect(0.0, PAD + QR_PT + 16.0, WIDTH, 20.0));
    content.addSubview(&name);

    let close = unsafe { NSButton::buttonWithTitle_target_action(&ns(""), Some(target), Some(sel!(friendInviteDone:)), mtm) };
    close.setBordered(false);
    if let Some(x) = symbol_sized("xmark.circle.fill", 18.0, 0.0) {
        close.setImage(Some(&x));
    }
    close.setContentTintColor(Some(&NSColor::colorWithWhite_alpha(1.0, 0.45)));
    accept_first_click(&close);
    close.setFrame(rect(WIDTH - 22.0 - 12.0, HEIGHT - 22.0 - 12.0, 22.0, 22.0));
    content.addSubview(&close);

    window.setContentView(Some(&card));
    Some(window)
}

/// The invite's QR code: an image view that is also the drag source. Dragged out of the island, it hands the invite's
/// PNG to the app it is dropped on, as a file (and as PNG data).
struct InviteQrIvars {
    files: Rc<RefCell<DragFiles>>,
    /// Where the current press began, in window points; None once a drag has started from it.
    press: Cell<Option<NSPoint>>,
}

define_class!(
    /// The invite's QR code. Its drag is Copy into other apps; the island's own drop target never takes it.
    #[unsafe(super(NSImageView))]
    #[thread_kind = MainThreadOnly]
    #[name = "HeimdallInviteQR"]
    #[ivars = InviteQrIvars]
    struct InviteQr;

    impl InviteQr {
        /// The panel never becomes key, so the first press must still reach the card.
        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }

        /// Takes the press itself, so the drag that follows comes to this view. A fresh press also clears any drag
        /// session AppKit never reported as ended: no drag can be live at a new press.
        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            self.ivars().press.set(Some(event.locationInWindow()));
            self.ivars().files.borrow_mut().end_drag(Instant::now());
            schedule_reap();
        }

        /// The drag starts once the pointer has travelled `DRAG_START_PT` from the press, not on its first pixel.
        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            let Some(start) = self.ivars().press.get() else { return };
            let at = event.locationInWindow();
            if (at.x - start.x).hypot(at.y - start.y) < DRAG_START_PT {
                return;
            }
            self.ivars().press.set(None);
            self.start_drag(event);
        }
    }

    unsafe impl NSObjectProtocol for InviteQr {}

    unsafe impl NSDraggingSource for InviteQr {
        #[unsafe(method(draggingSession:sourceOperationMaskForDraggingContext:))]
        fn source_operation_mask(&self, _session: &NSDraggingSession, context: NSDraggingContext) -> NSDragOperation {
            if context == NSDraggingContext::OutsideApplication { NSDragOperation::Copy } else { NSDragOperation::None }
        }

        #[unsafe(method(draggingSession:endedAtPoint:operation:))]
        fn drag_ended(&self, _session: &NSDraggingSession, _at: NSPoint, _operation: NSDragOperation) {
            self.ivars().files.borrow_mut().end_drag(Instant::now());
            schedule_reap();
        }
    }
);

impl InviteQr {
    fn new(files: Rc<RefCell<DragFiles>>, mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(InviteQrIvars { files, press: Cell::new(None) });
        unsafe { msg_send![super(this), initWithFrame: rect(0.0, 0.0, QR_PT, QR_PT)] }
    }

    /// A drag from the card: the invite's PNG becomes a file, and the QR code is the picture that follows the pointer.
    fn start_drag(&self, event: &NSEvent) {
        let item = {
            let mut files = self.ivars().files.borrow_mut();
            // A drag still in flight owns its file: a second one waits for it.
            if files.session.is_some() {
                return;
            }
            match files.begin_drag(&std::env::temp_dir()) {
                Ok(path) => pasteboard_item(&path, files.png()),
                Err(err) => {
                    eprintln!("friends: the invite's PNG could not be written for a drag: {err}");
                    return;
                }
            }
        };
        let writer: &ProtocolObject<dyn NSPasteboardWriting> = ProtocolObject::from_ref(&*item);
        let drag = NSDraggingItem::initWithPasteboardWriter(NSDraggingItem::alloc(), writer);
        let image = self.image();
        let contents = image.as_deref().map(|image| image.as_ref());
        unsafe { drag.setDraggingFrame_contents(self.bounds(), contents) };
        let items = NSArray::from_retained_slice(&[drag]);
        let source: &ProtocolObject<dyn NSDraggingSource> = ProtocolObject::from_ref(self);
        let _session = self.beginDraggingSessionWithItems_event_source(&items, event, source);
    }
}

/// The drag's pasteboard item: the file's URL (the type Finder and Discord take a dropped file by) and the PNG itself.
fn pasteboard_item(path: &Path, png: &[u8]) -> Retained<NSPasteboardItem> {
    let item = NSPasteboardItem::new();
    if let Some(url) = NSURL::fileURLWithPath(&ns(&path.to_string_lossy())).absoluteString() {
        item.setString_forType(&url, unsafe { NSPasteboardTypeFileURL });
    }
    item.setData_forType(&NSData::with_bytes(png), unsafe { NSPasteboardTypePNG });
    item
}

/// The invite's PNG as the drag hands it out. The open panel's file is made by the first drag. A file leaves `current`
/// when its panel closes or a new invite opens, and stays in `retired` for `RETAIN`. A drag in flight holds its file in
/// `session` until AppKit reports the drag ended.
#[derive(Default)]
struct DragFiles {
    png: Vec<u8>,
    current: Option<Rc<TempPng>>,
    session: Option<Rc<TempPng>>,
    /// Files no longer in use by a panel, each with the instant it may be removed.
    retired: Vec<(Instant, Rc<TempPng>)>,
}

impl DragFiles {
    /// A new invite is shown: the old file is retired, and leftovers of an earlier run in `dir` are swept, unless a
    /// drag is still using its file.
    fn set_invite(&mut self, dir: &Path, png: &[u8], now: Instant) {
        self.retire_current(now);
        if self.session.is_none() {
            sweep(dir);
        }
        self.png = png.to_vec();
    }

    /// The panel closes: its file is retired, not removed.
    fn close(&mut self, now: Instant) {
        self.retire_current(now);
    }

    /// A drag starts: the open invite's file, written now if the panel has none, or if its file has gone missing.
    fn begin_drag(&mut self, dir: &Path) -> io::Result<PathBuf> {
        let file = match &self.current {
            Some(file) if file.path.exists() => Rc::clone(file),
            _ => {
                let file = Rc::new(TempPng::create(dir, &self.png)?);
                self.current = Some(Rc::clone(&file));
                file
            }
        };
        let path = file.path.clone();
        self.session = Some(file);
        Ok(path)
    }

    /// AppKit reports the drag ended (or a new press clears it): a file its panel no longer shows is retained for
    /// `RETAIN` from now, a file still shown by its panel stays with the panel.
    fn end_drag(&mut self, now: Instant) {
        let Some(file) = self.session.take() else { return };
        if self.current.as_ref().is_some_and(|current| Rc::ptr_eq(current, &file)) {
            return;
        }
        match self.retired.iter().position(|(_, kept)| Rc::ptr_eq(kept, &file)) {
            Some(at) => self.retired[at].0 = self.retired[at].0.max(now + RETAIN),
            None => self.retired.push((now + RETAIN, file)),
        }
    }

    /// Removes the retired files whose retention is over (their `TempPng` drop removes each file).
    fn reap(&mut self, now: Instant) {
        self.retired.retain(|(until, _)| *until > now);
    }

    fn retire_current(&mut self, now: Instant) {
        if let Some(file) = self.current.take() {
            self.retired.push((now + RETAIN, file));
        }
    }

    fn png(&self) -> &[u8] {
        &self.png
    }
}

/// A PNG under the temp dir, created exclusively (never over an existing file) with mode 0600. Removed when dropped.
struct TempPng {
    path: PathBuf,
}

impl TempPng {
    fn create(dir: &Path, png: &[u8]) -> io::Result<TempPng> {
        for _ in 0..16 {
            let path = dir.join(format!("{FILE_PREFIX}{}{FILE_SUFFIX}", random_hex()?));
            match OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path) {
                Ok(mut file) => {
                    // Owned before the write, so a failed write removes the partial file too.
                    let temp = TempPng { path };
                    file.write_all(png)?;
                    return Ok(temp);
                }
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(err) => return Err(err),
            }
        }
        Err(io::Error::new(io::ErrorKind::AlreadyExists, "no free name for the invite's PNG"))
    }
}

impl Drop for TempPng {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Eight random bytes as 16 hex digits, from the kernel's random source.
fn random_hex() -> io::Result<String> {
    let mut bytes = [0u8; 8];
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// Removes the invite PNGs in `dir` that an earlier run left behind and that are older than `LEFTOVER_AGE`. A younger
/// one may belong to a live app (the render mode shares the temp dir), so it stays.
fn sweep(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let ours = name.starts_with(FILE_PREFIX) && name.ends_with(FILE_SUFFIX);
        let Ok(meta) = entry.metadata() else { continue };
        let old = meta
            .modified()
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age > LEFTOVER_AGE);
        if ours && meta.is_file() && old {
            let _ = fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A scratch dir for one test, named for it, under the temp dir. No test sweeps the real temp dir.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("heimdall-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A small real PNG, as `friends::invite::qr_png` makes one.
    fn png() -> Vec<u8> {
        let mut out = Vec::new();
        let mut encoder = png::Encoder::new(&mut out, 2, 2);
        encoder.set_color(png::ColorType::Grayscale);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.write_header().unwrap().write_image_data(&[0, 255, 255, 0]).unwrap();
        out
    }

    fn invite_files(dir: &Path) -> Vec<PathBuf> {
        let mut found: Vec<PathBuf> = fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                let name = p.file_name().unwrap().to_string_lossy();
                name.starts_with(FILE_PREFIX) && name.ends_with(FILE_SUFFIX)
            })
            .collect();
        found.sort();
        found
    }

    /// The instant after which every retention has run out.
    fn after_retention() -> Instant {
        Instant::now() + RETAIN + Duration::from_secs(1)
    }

    #[test]
    fn the_file_is_written_only_when_a_drag_starts_and_its_item_offers_it_as_a_file_url_and_png() {
        let dir = scratch("item");
        let mut files = DragFiles::default();
        files.set_invite(&dir, &png(), Instant::now());
        assert!(invite_files(&dir).is_empty(), "nothing is written until a drag starts");

        let path = files.begin_drag(&dir).unwrap();
        assert_eq!(invite_files(&dir), vec![path.clone()]);
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with(FILE_PREFIX) && name.ends_with(FILE_SUFFIX), "{name}");
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(fs::read(&path).unwrap(), png());

        let item = pasteboard_item(&path, files.png());
        let url = item.stringForType(unsafe { NSPasteboardTypeFileURL }).expect("a file-URL representation");
        let url = NSURL::URLWithString(&url).expect("the file URL parses");
        assert_eq!(url.path().unwrap().to_string(), path.display().to_string());
        assert!(item.types().containsObject(unsafe { NSPasteboardTypePNG }));
        assert_eq!(item.dataForType(unsafe { NSPasteboardTypePNG }).unwrap().to_vec(), png());

        files.end_drag(Instant::now());
        files.close(Instant::now());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_drag_file_that_has_gone_missing_is_recreated_for_the_next_drag() {
        let dir = scratch("recreate");
        let mut files = DragFiles::default();
        files.set_invite(&dir, &png(), Instant::now());
        let first = files.begin_drag(&dir).unwrap();
        files.end_drag(Instant::now());
        fs::remove_file(&first).unwrap();
        let second = files.begin_drag(&dir).unwrap();
        assert!(second.exists(), "a later drag offers a file that exists");
        files.end_drag(Instant::now());
        files.close(Instant::now());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn closing_the_panel_keeps_the_drag_file_ten_minutes_then_removes_it() {
        let dir = scratch("close");
        let mut panel = InvitePanel::default();
        panel.files.borrow_mut().set_invite(&dir, &png(), Instant::now());
        let path = panel.files.borrow_mut().begin_drag(&dir).unwrap();
        panel.files.borrow_mut().end_drag(Instant::now());
        panel.close();
        panel.files.borrow_mut().reap(Instant::now());
        assert!(path.exists(), "the receiving app may still read it right after the close");
        panel.files.borrow_mut().reap(after_retention());
        assert!(!path.exists());
        assert!(invite_files(&dir).is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_close_during_a_drag_keeps_the_file_until_the_drag_ends_and_then_ten_minutes() {
        let dir = scratch("in-flight");
        let mut panel = InvitePanel::default();
        panel.files.borrow_mut().set_invite(&dir, &png(), Instant::now());
        let path = panel.files.borrow_mut().begin_drag(&dir).unwrap();
        panel.close();
        panel.files.borrow_mut().reap(after_retention() + RETAIN);
        assert!(path.exists(), "the drop still needs the file");
        panel.files.borrow_mut().end_drag(Instant::now());
        panel.files.borrow_mut().reap(Instant::now());
        assert!(path.exists(), "the drag's end starts the ten minutes");
        panel.files.borrow_mut().reap(after_retention());
        assert!(!path.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_new_press_clears_a_drag_whose_end_was_never_reported() {
        let dir = scratch("stuck");
        let mut files = DragFiles::default();
        files.set_invite(&dir, &png(), Instant::now());
        // The drag-ended callback never came, so the session is still held. A fresh press ends it (mouseDown calls
        // end_drag), and the next drag can start.
        files.begin_drag(&dir).unwrap();
        files.end_drag(Instant::now());
        assert!(files.session.is_none());
        assert!(files.begin_drag(&dir).unwrap().exists());
        files.end_drag(Instant::now());
        files.close(Instant::now());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn opening_an_invite_retires_the_first_drag_file_and_sweeps_only_old_leftovers() {
        let dir = scratch("leftover");
        // A leftover of a run that died long ago, a fresh invite file of a live app, and the user's own file.
        let old = dir.join("heimdall-invite-0123456789abcdef.png");
        fs::write(&old, b"an earlier invite").unwrap();
        File::options().write(true).open(&old).unwrap().set_modified(SystemTime::now() - Duration::from_secs(7200)).unwrap();
        let fresh = dir.join("heimdall-invite-fedcba9876543210.png");
        fs::write(&fresh, b"a live invite").unwrap();
        let unrelated = dir.join("notes.png");
        fs::write(&unrelated, b"not ours").unwrap();

        let mut panel = InvitePanel::default();
        let first = {
            panel.files.borrow_mut().set_invite(&dir, &png(), Instant::now());
            panel.files.borrow_mut().begin_drag(&dir).unwrap()
        };
        panel.files.borrow_mut().end_drag(Instant::now());
        panel.close();
        panel.files.borrow_mut().set_invite(&dir, &png(), Instant::now());
        assert!(!old.exists(), "an hour-old leftover goes when an invite opens");
        assert!(fresh.exists(), "a recent invite file is not swept");
        assert!(unrelated.exists());
        assert!(first.exists(), "the first drag file is retained for ten minutes, not removed at once");
        panel.files.borrow_mut().reap(after_retention());
        assert!(!first.exists());
        let _ = fs::remove_dir_all(&dir);
    }
}
