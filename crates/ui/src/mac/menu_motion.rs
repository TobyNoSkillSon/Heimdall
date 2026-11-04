//! An open menu that changes height (a model row's disclosure, Add Provider, a search narrowing a list) resizes at the
//! display's rate. AppKit resizes an open menu window through `updateWindowFrameTo:offset:animated:completion:`: either
//! in one frame, or (`animated` YES) through NSWindow's old blocking resize (`NSMoveHelper`, a nested run loop): measured on 30 Oct,
//! macOS 27 (lab/menu-motion), a Phonon S disclosure held the main thread for 122 ms with the window 5 pt into its
//! 24 pt change, then finished in a few frames: the user's "runs in like five frames". Here that call returns at once and
//! the frame steps on the window's own display link, each step through AppKit's own unanimated update (which lays the
//! rows out for that frame), the completion run with the last one. A window not on screen (a menu's first placement)
//! or a move without a new size goes straight through. Nothing runs at rest: the link exists only while a resize is under way.
//!
//! The rows move with the window (31 Oct). AppKit lays the menu's rows (an NSTableView's row views) out for the new
//! contents at once, before the window has moved: a section opening had its last rows cut off by the window's old
//! bottom edge and then revealed, a section folding left an empty band at the bottom that shrank (recorded frame by
//! frame, lab/menu-motion). A change the menu announces first (`will_change`, a section header's press) records where
//! every row stood; each step of the resize then shows every row that stays where the same progress puts it (a layer
//! translation, no layout), so the rows below a change ride on the moving bottom edge. Rows that arrive are attached
//! under the row above them and uncovered as the rows below slide away; rows that leave are drawn from a picture taken
//! before the change, under the row above them, covered by the rows sliding up. The last step clears it all: at rest
//! the menu is AppKit's own, nothing drawn on top.

use std::cell::RefCell;
use std::sync::OnceLock;
use std::time::Instant;

use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, Bool, Imp, Sel};
use objc2::{define_class, msg_send, sel, MainThreadOnly};
use objc2_foundation::{MainThreadMarker, NSObject, NSPoint, NSRect, NSRunLoop, NSRunLoopCommonModes, NSSize};
use objc2_app_kit::{NSMenuItem, NSTableRowView, NSTableView, NSView, NSWindow};
use objc2_core_foundation::{CGAffineTransform, CGPoint, CGRect, CGSize};
use objc2_quartz_core::{CALayer, CATransaction};

use super::submenus_left::replace;

/// A resize's length: as long as a submenu's own fade-in, so the two motions read as one family.
pub(crate) const DURATION_S: f64 = 0.2;

type Update = unsafe extern "C-unwind" fn(&NSWindow, Sel, NSRect, NSPoint, Bool, *mut AnyObject);
static UPDATE: OnceLock<Update> = OnceLock::new();

struct Resize {
    window: Retained<NSWindow>,
    from: NSRect,
    to: NSRect,
    offset: NSPoint,
    start: Instant,
    completion: Option<Retained<AnyObject>>,
    /// Completions of requests that retargeted this resize: run with the last step.
    more: Vec<Retained<AnyObject>>,
    link: Retained<AnyObject>,
    /// The rows' own motion, when the change was announced (`will_change`).
    rows: Option<Rows>,
}

thread_local! {
    static RESIZES: RefCell<Vec<Resize>> = const { RefCell::new(Vec::new()) };
    /// A change announced and not yet laid out: where the rows stood before it.
    static PENDING: RefCell<Option<Before>> = const { RefCell::new(None) };
    /// While rows move: places them once more just before Core Animation commits a run-loop turn.
    static BEFORE_COMMIT: RefCell<Option<objc2_core_foundation::CFRetained<objc2_core_foundation::CFRunLoopObserver>>> = const { RefCell::new(None) };
    static TICKER: std::cell::OnceCell<Retained<Ticker>> = const { std::cell::OnceCell::new() };
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "HeimdallMenuMotion"]
    struct Ticker;

    impl Ticker {
        #[unsafe(method(menuMotionFrame:))]
        fn frame(&self, link: &AnyObject) {
            tick(link);
        }
    }
);

/// Wraps the menu window's frame update once per process.
pub(crate) fn install() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let Some(class) = AnyClass::get(c"NSPopupMenuWindow") else { return };
        unsafe {
            if let Some(old) = replace(class, sel!(updateWindowFrameTo:offset:animated:completion:), std::mem::transmute::<Update, Imp>(update)) {
                UPDATE.set(std::mem::transmute::<Imp, Update>(old)).ok();
            }
        }
    });
}

/// Eased progress: quick out of the press, settling into place (an ease-out cubic).
pub(crate) fn ease(t: f64) -> f64 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t).powi(3)
}

/// The frame `t` of the way from `a` to `b`, on whole points so rows never sit between pixels.
pub(crate) fn between(a: NSRect, b: NSRect, t: f64) -> NSRect {
    let mix = |x: f64, y: f64| (x + (y - x) * t).round();
    NSRect::new(NSPoint::new(mix(a.origin.x, b.origin.x), mix(a.origin.y, b.origin.y)), NSSize::new(mix(a.size.width, b.size.width), mix(a.size.height, b.size.height)))
}

fn same(a: NSRect, b: NSRect) -> bool {
    (a.origin.x - b.origin.x).abs() < 0.5 && (a.origin.y - b.origin.y).abs() < 0.5 && (a.size.width - b.size.width).abs() < 0.5 && (a.size.height - b.size.height).abs() < 0.5
}

fn original(window: &NSWindow, frame: NSRect, offset: NSPoint, completion: Option<&AnyObject>) {
    if let Some(old) = UPDATE.get() {
        let block = completion.map_or(std::ptr::null_mut(), |c| c as *const AnyObject as *mut AnyObject);
        unsafe { old(window, sel!(updateWindowFrameTo:offset:animated:completion:), frame, offset, Bool::NO, block) };
    }
}

/// Ends a resize under way on `window` where it stands, its completion run (a new one starts from there).
fn settle(window: &NSWindow) -> Option<NSRect> {
    let r = RESIZES.with(|r| {
        let mut r = r.borrow_mut();
        let i = r.iter().position(|x| std::ptr::eq(&*x.window, window))?;
        Some(r.remove(i))
    })?;
    unsafe { let _: () = msg_send![&*r.link, invalidate]; }
    if let Some(rows) = &r.rows { rows.clear(); }
    let at = window.frame();
    original(window, at, r.offset, r.completion.as_deref());
    for c in &r.more { original(window, at, r.offset, Some(c)); }
    Some(at)
}

unsafe extern "C-unwind" fn update(this: &NSWindow, cmd: Sel, frame: NSRect, offset: NSPoint, animated: Bool, completion: *mut AnyObject) {
    let _ = animated;
    let Some(old) = UPDATE.get() else { return };
    let from = this.frame();
    let ticker = MainThreadMarker::new().map(|mtm| TICKER.with(|t| t.get_or_init(|| unsafe { msg_send![Ticker::alloc(mtm), init] }).clone()));
    // A menu already on screen changing size animates, whichever way AppKit asked (it snaps some changes and runs the
    // blocking resize for others, 30 Oct); its first placement and a move without a new size go straight through.
    let resized = (from.size.width - frame.size.width).abs() >= 0.5 || (from.size.height - frame.size.height).abs() >= 0.5;
    // A request without a new size while a resize is under way retargets it (AppKit, hiding a section's rows one at a
    // time, asks for sizes up and down within one turn: settling there ended the motion mid-fold, 31 Oct); an
    // announced change without one still moves its rows.
    let under_way = RESIZES.with(|r| r.borrow().iter().any(|x| std::ptr::eq(&*x.window, this)));
    let announced = PENDING.with(|p| p.borrow().as_ref().is_some_and(|b| std::ptr::eq(&*b.window, this)));
    if !this.isVisible() || from.size.width <= 0.0 || (!resized && !under_way && !announced) || ticker.is_none() {
        take_pending(this);
        settle(this);
        unsafe { old(this, cmd, frame, offset, Bool::NO, completion) };
        return;
    }
    let completion: Option<Retained<AnyObject>> = if completion.is_null() { None } else { unsafe { msg_send![completion, copy] } };
    // A resize already under way is retargeted from where the window stands, never finished here first: each row a
    // click shows or hides asks for a new size, and finishing the running resize laid the window out synchronously once
    // per row (climb 2, measured live: 14-38 ms held per header click, the sample in every case under `settle`).
    let before = take_pending(this);
    let retargeted = RESIZES.with(|r| {
        let mut r = r.borrow_mut();
        let x = r.iter_mut().find(|x| std::ptr::eq(&*x.window, this))?;
        x.from = from;
        x.to = frame;
        x.offset = offset;
        x.start = Instant::now();
        if let Some(c) = completion.clone() { x.more.push(c); }
        // A new change on a resize under way: the rows go on from where they stand (`will_change` read them so).
        // A later request without a new change (a live row update): the rows go on from where they are, not from the
        // start again (code review, 31 Oct).
        if before.is_none() { if let Some(rows) = x.rows.as_mut() { rows.base = rows.reached.get(); } }
        if let Some(before) = before.clone() {
            if let Some(old) = x.rows.take() { old.clear(); }
            let rows = Rows::new(before);
            // Before the window server shows AppKit's new layout: the rows where they stood.
            rows.shared().0.place(0.0);
            x.rows = Some(rows);
        }
        Some(())
    });
    if retargeted.is_some() { return }
    let ticker = ticker.unwrap();
    let link: Option<Retained<AnyObject>> = unsafe { msg_send![this, displayLinkWithTarget: &*ticker, selector: sel!(menuMotionFrame:)] };
    let Some(link) = link else {
        unsafe { old(this, cmd, frame, offset, Bool::NO, completion.as_deref().map_or(std::ptr::null_mut(), |c| c as *const AnyObject as *mut AnyObject)) };
        return;
    };
    unsafe {
        let range = objc2_quartz_core::CAFrameRateRange { minimum: 60.0, maximum: 120.0, preferred: 120.0 };
        let _: () = msg_send![&*link, setPreferredFrameRateRange: range];
        let _: () = msg_send![&*link, addToRunLoop: &*NSRunLoop::currentRunLoop(), forMode: NSRunLoopCommonModes];
    }
    let rows = before.map(Rows::new);
    if let Some(rows) = &rows { rows.shared().0.place(0.0); }
    RESIZES.with(|r| r.borrow_mut().push(Resize { window: objc2::Message::retain(this), from, to: frame, offset, start: Instant::now(), completion, more: vec![], link, rows }));
}

fn tick(link: &AnyObject) {
    let step = RESIZES.with(|r| {
        let mut r = r.borrow_mut();
        let i = r.iter().position(|x| std::ptr::eq(&*x.link, link))?;
        let t = r[i].start.elapsed().as_secs_f64() / DURATION_S;
        // A menu closed mid-resize (Escape, a click outside) ends where it was going, at once.
        if t >= 1.0 || !r[i].window.isVisible() {
            let done = r.remove(i);
            return Some((done.window.clone(), done.to, done.offset, Some(done.completion), Some(done.link), done.more, done.rows, None));
        }
        let x = &mut r[i];
        let frame = between(x.from, x.to, ease(t));
        let p = progress(x, frame.size.height);
        Some((x.window.clone(), frame, x.offset, None, None, vec![], None, x.rows.as_ref().map(|rows| rows.shared().0).map(|h| (h, p))))
    });
    let Some((window, frame, offset, completion, finished, more, ended, moving)) = step else {
        // A link whose resize was settled elsewhere.
        unsafe { let _: () = msg_send![link, invalidate]; }
        return;
    };
    if let Some(link) = finished { unsafe { let _: () = msg_send![&*link, invalidate]; } }
    original(&window, frame, offset, completion.flatten().as_deref());
    // The retargeting requests' completions, at the frame already reached (no new size, no layout).
    for c in more { original(&window, frame, offset, Some(&c)); }
    // After AppKit's own layout for this step: the rows where this step puts them, or AppKit's own at the end.
    if let Some(rows) = ended { rows.clear(); }
    if let Some((rows, p)) = moving { rows.place(p); }
}

/// AppKit hid `window` or set it up for another menu (menu windows are reused): a resize under way there ends without
/// touching the window again; its completion belonged to the old menu.
pub(crate) fn forget(window: &NSWindow) {
    let gone = RESIZES.with(|r| {
        let mut r = r.borrow_mut();
        let i = r.iter().position(|x| std::ptr::eq(&*x.window, window))?;
        Some(r.remove(i))
    });
    if let Some(gone) = gone {
        unsafe { let _: () = msg_send![&*gone.link, invalidate]; }
        if let Some(rows) = &gone.rows { rows.clear(); }
    }
    take_pending(window);
}


/// AppKit adds or hides a section's rows one at a time after a change starts, all within the run-loop turn, and Core
/// Animation commits that turn's layout before the next display tick: one frame showed the rows in their new places
/// (31 Oct, recorded). While rows move, an observer places them again just before that commit (Core Animation's own
/// observer runs at order 2,000,000); it goes with the last motion, so nothing runs at rest.
fn watch_commits() {
    use objc2_core_foundation::{CFRunLoop, CFRunLoopActivity, CFRunLoopObserver};
    BEFORE_COMMIT.with(|o| {
        if o.borrow().is_some() { return }
        let block = block2::RcBlock::new(|_: *mut CFRunLoopObserver, _: CFRunLoopActivity| place_running());
        let observer = unsafe { CFRunLoopObserver::with_handler(None, CFRunLoopActivity::BeforeWaiting.0, true, 1_999_000, Some(&block)) };
        if let (Some(observer), Some(main)) = (observer, CFRunLoop::main()) {
            main.add_observer(Some(&observer), unsafe { objc2_core_foundation::kCFRunLoopCommonModes });
            *o.borrow_mut() = Some(observer);
        }
    });
}

/// The rows' progress through a resize: the window's own, from the height it stands at (whole points), so the last rows
/// sit exactly on its bottom edge; a resize retargeted without a new change goes on from the progress already reached.
fn progress(x: &Resize, height: f64) -> f64 {
    let dh = x.to.size.height - x.from.size.height;
    let raw = if dh.abs() >= 0.5 { (height - x.from.size.height) / dh } else { ease(x.start.elapsed().as_secs_f64() / DURATION_S) };
    let base = x.rows.as_ref().map_or(0.0, |r| r.base);
    base + (1.0 - base) * raw.clamp(0.0, 1.0)
}

/// Every moving resize's rows where its window stands now; the observer goes once none moves.
fn place_running() {
    let moving: Vec<(RowsHandle, f64)> = RESIZES.with(|r| r.borrow().iter().filter_map(|x| {
        let rows = x.rows.as_ref()?;
        Some((rows.shared().0, progress(x, x.window.frame().size.height)))
    }).collect());
    if moving.is_empty() {
        BEFORE_COMMIT.with(|o| if let Some(observer) = o.borrow_mut().take() { observer.invalidate(); });
        return;
    }
    for (rows, p) in moving { rows.place(p); }
}

/// The menu's rows in a window, as AppKit has them: the table that lays them out and each row view with its item,
/// top first (the table's coordinates, y down).
pub(crate) fn table_of(window: &NSWindow) -> Option<Retained<NSTableView>> {
    fn find(view: &NSView) -> Option<Retained<NSTableView>> {
        if let Some(table) = objc2::Message::retain(view).downcast::<NSTableView>().ok() { return Some(table) }
        view.subviews().iter().find_map(|v| find(&v))
    }
    find(&*window.contentView()?)
}

struct Row {
    item: Retained<NSMenuItem>,
    view: Retained<NSTableRowView>,
    y: f64,
    h: f64,
}

fn rows_of(table: &NSTableView) -> Vec<Row> {
    let mut rows: Vec<Row> = table.subviews().iter().filter_map(|v| {
        let view = v.downcast::<NSTableRowView>().ok()?;
        if view.isHidden() { return None }
        let holder = view.subviews().firstObject()?;
        if !objc2_foundation::NSObjectProtocol::respondsToSelector(&*holder, sel!(menuItem)) { return None }
        let item: Option<Retained<NSMenuItem>> = unsafe { msg_send![&*holder, menuItem] };
        let f = view.frame();
        Some(Row { item: item?, view, y: f.origin.y, h: f.size.height })
    }).collect();
    rows.sort_by(|a, b| a.y.total_cmp(&b.y));
    rows
}

/// Where the rows stood before an announced change, as they showed (a resize under way included), and a picture of
/// the table then.
#[derive(Clone)]
pub(crate) struct Before {
    window: Retained<NSWindow>,
    table: Retained<NSTableView>,
    /// Each row's item, top as shown, height, and top as laid out (where the picture has it), top first.
    rows: Vec<(Retained<NSMenuItem>, f64, f64, f64)>,
    /// The items the change hides.
    leaving: Vec<usize>,
    /// Pictures of the table's rows as laid out before the change (text and symbols; the menu's material is the
    /// window's): the leaving rows' band and the bottom band an opening pushes below the window's edge, each with its
    /// top as laid out.
    ghost: Option<(Retained<objc2_core_graphics::CGImage>, f64, f64)>,
    tail: Option<(Retained<objc2_core_graphics::CGImage>, f64, f64)>,
    at: Instant,
}

fn key(item: &Retained<NSMenuItem>) -> usize {
    Retained::as_ptr(item) as usize
}

/// A change to `window`'s rows is about to be made (`leaving`: the items it will hide; `arriving`: about how tall the
/// rows it shows are): where the rows stand now, and pictures of the bands it needs. The resize the change asks for
/// moves the rows with the window.
pub(crate) fn will_change(window: &NSWindow, leaving: &[Retained<NSMenuItem>], arriving: f64) {
    PENDING.with(|p| p.borrow_mut().take());
    if !window.isVisible() { return }
    let Some(table) = table_of(window) else { return };
    // A resize under way: the rows as they show at this moment.
    let shown: Vec<(usize, f64)> = RESIZES.with(|r| r.borrow().iter().find(|x| std::ptr::eq(&*x.window, window))
        .and_then(|x| x.rows.as_ref().map(|rows| rows.shifts.borrow().clone())).unwrap_or_default());
    let rows: Vec<(Retained<NSMenuItem>, f64, f64, f64)> = rows_of(&table).into_iter().map(|r| {
        let shift = shown.iter().find(|(k, _)| *k == key(&r.item)).map_or(0.0, |(_, s)| *s);
        (r.item, r.y + shift, r.h, r.y)
    }).collect();
    // Only the bands the motion draws from (a picture of the whole table held the press 10 ms longer, 31 Oct): the
    // leaving rows, and the bottom rows an opening pushes below the window's edge before the table draws them there.
    let picture = |top: f64, bottom: f64| -> Option<(Retained<objc2_core_graphics::CGImage>, f64, f64)> {
        if bottom - top < 1.0 { return None }
        let rect = NSRect::new(NSPoint::new(0.0, top), NSSize::new(table.bounds().size.width, bottom - top));
        let rep = table.bitmapImageRepForCachingDisplayInRect(rect)?;
        table.cacheDisplayInRect_toBitmapImageRep(rect, &rep);
        Some((rep.CGImage()?, top, bottom - top))
    };
    let laid: Vec<(f64, f64)> = rows.iter().filter(|r| leaving.iter().any(|l| std::ptr::eq(&**l, &*r.0))).map(|r| (r.3, r.3 + r.2)).collect();
    let ghost = picture(laid.iter().map(|r| r.0).fold(f64::MAX, f64::min), laid.iter().map(|r| r.1).fold(f64::MIN, f64::max));
    let end = rows.iter().map(|r| r.3 + r.2).fold(0.0, f64::max);
    let tail = (arriving > 0.0).then(|| picture((end - arriving - 24.0).max(0.0), end)).flatten();
    let leaving = leaving.iter().map(key).collect();
    let window = objc2::Message::retain(window);
    PENDING.with(|p| *p.borrow_mut() = Some(Before { window, table, rows, leaving, ghost, tail, at: Instant::now() }));
}

/// The change announced for `window`, if it is the one being laid out now (an announcement left over from a change
/// that asked for no resize is dropped).
fn take_pending(window: &NSWindow) -> Option<Before> {
    PENDING.with(|p| {
        let mut p = p.borrow_mut();
        let before = p.take()?;
        (std::ptr::eq(&*before.window, window) && before.at.elapsed().as_secs_f64() < 0.25).then_some(before)
    })
}

/// A picture's band shown over the table: a clipping layer and the picture inside it.
#[derive(Clone)]
struct Band {
    clip: Retained<CALayer>,
    picture: Retained<CALayer>,
    /// The picture's top in the table as laid out before the change, and its height.
    top: f64,
    h: f64,
}

impl Band {
    fn new(host: &CALayer, (image, top, h): &(Retained<objc2_core_graphics::CGImage>, f64, f64), w: f64, scale: f64) -> Band {
        let clip = CALayer::new();
        clip.setMasksToBounds(true);
        clip.setFrame(cg(0.0, 0.0, w, 0.0));
        let picture = CALayer::new();
        unsafe { picture.setContents(Some(&*(&**image as *const objc2_core_graphics::CGImage as *const AnyObject))) };
        picture.setContentsScale(scale);
        picture.setFrame(cg(0.0, 0.0, w, *h));
        clip.addSublayer(&picture);
        host.addSublayer(&clip);
        Band { clip, picture, top: *top, h: *h }
    }

    /// Shows the rows laid out from `from` before the change (the table's points, y down) at `top` in the table,
    /// `shown` points of them; none when `at` is `None`.
    fn show(&self, host: &CALayer, table_h: f64, w: f64, at: Option<BandAt>) {
        let BandAt { from, top, shown } = at.unwrap_or(BandAt { from: self.top, top: 0.0, shown: 0.0 });
        // Rows above the pictured band are not in it: show from where it starts.
        let skip = (self.top - from).max(0.0);
        let (from, top, shown) = (from + skip, top + skip, (shown - skip).clamp(0.0, (self.top + self.h - from - skip).max(0.0)));
        let flipped = host.contentsAreFlipped();
        self.clip.setFrame(cg(0.0, if flipped { top } else { table_h - top - shown }, w, shown));
        // The clip's coordinates follow its host's orientation.
        let inside = from - self.top;
        self.picture.setFrame(cg(0.0, if flipped { -inside } else { shown - (self.h - inside) }, w, self.h));
    }
}

/// The rows' motion through one resize.
struct Rows {
    before: Before,
    /// The progress reached when the resize was retargeted without a new change (`progress`).
    base: f64,
    /// The progress last placed.
    reached: std::rc::Rc<std::cell::Cell<f64>>,
    /// Set once the rows are AppKit's again: a placement after that (AppKit re-entering a step) does nothing.
    cleared: std::rc::Rc<std::cell::Cell<bool>>,
    /// Each moved row's current offset from its layout place (item pointer, points down), for a change on top.
    shifts: std::rc::Rc<RefCell<Vec<(usize, f64)>>>,
    /// The row views given an offset or a mask, to clear.
    touched: std::rc::Rc<RefCell<Vec<Retained<NSTableRowView>>>>,
    /// The leaving rows' band and the last rows' band, from the pictures.
    ghost: Option<Band>,
    tail: Option<Band>,
}

/// A handle the tick places rows through without holding the resize list.
struct RowsHandle {
    table: Retained<NSTableView>,
    old: Vec<(Retained<NSMenuItem>, f64, f64, f64)>,
    leaving: Vec<usize>,
    shifts: std::rc::Rc<RefCell<Vec<(usize, f64)>>>,
    touched: std::rc::Rc<RefCell<Vec<Retained<NSTableRowView>>>>,
    ghost: Option<Band>,
    tail: Option<Band>,
    reached: std::rc::Rc<std::cell::Cell<f64>>,
    cleared: std::rc::Rc<std::cell::Cell<bool>>,
}

fn cg(x: f64, y: f64, w: f64, h: f64) -> CGRect {
    CGRect::new(CGPoint::new(x, y), CGSize::new(w, h))
}

impl Rows {
    fn new(before: Before) -> Rows {
        watch_commits();
        let w = before.table.bounds().size.width;
        let scale = before.window.backingScaleFactor();
        let (mut ghost, mut tail) = (None, None);
        if let Some(host) = before.table.layer() {
            CATransaction::begin();
            CATransaction::setDisableActions(true);
            ghost = before.ghost.as_ref().map(|p| Band::new(&host, p, w, scale));
            tail = before.tail.as_ref().map(|p| Band::new(&host, p, w, scale));
            CATransaction::commit();
        }
        Rows { before, base: 0.0, reached: Default::default(), cleared: Default::default(), shifts: Default::default(), touched: Default::default(), ghost, tail }
    }

    fn shared(&self) -> (RowsHandle, f64) {
        (RowsHandle { table: self.before.table.clone(), old: self.before.rows.clone(), leaving: self.before.leaving.clone(), shifts: self.shifts.clone(),
            touched: self.touched.clone(), ghost: self.ghost.clone(), tail: self.tail.clone(), reached: self.reached.clone(), cleared: self.cleared.clone() }, self.reached.get())
    }

    /// AppKit's rows again, as they are laid out: no offsets, no masks, no picture.
    fn clear(&self) {
        self.shared().0.clear();
    }
}

impl RowsHandle {
    fn clear(&self) {
        self.cleared.set(true);
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        for view in self.touched.borrow_mut().drain(..) {
            if let Some(layer) = view.layer() {
                layer.setAffineTransform(CGAffineTransform { a: 1.0, b: 0.0, c: 0.0, d: 1.0, tx: 0.0, ty: 0.0 });
                unsafe { layer.setMask(None) };
            }
        }
        for band in self.ghost.iter().chain(&self.tail) { band.clip.removeFromSuperlayer(); }
        CATransaction::commit();
        self.shifts.borrow_mut().clear();
    }

    /// Every row where progress `p` puts it (`plan`), through its layer only.
    fn place(&self, p: f64) {
        if self.cleared.get() { return }
        self.reached.set(p);
        // The table draws a row only once its laid-out place is inside the window: a row laid out below the edge as it
        // grows has a view with nothing drawn in it yet (31 Oct, recorded: the last rows blank). Those come from the
        // picture (`plan`'s tail) and their own views stay masked out until AppKit draws them.
        let visible = self.table.visibleRect();
        let drawn_to = visible.origin.y + visible.size.height + 0.5;
        let (rows, undrawn): (Vec<Row>, Vec<Row>) = rows_of(&self.table).into_iter().partition(|r| r.y + r.h <= drawn_to);
        let Some(host) = self.table.layer() else { return };
        let down = if host.contentsAreFlipped() { 1.0 } else { -1.0 };
        let height = self.table.bounds().size.height;
        let now: Vec<(usize, f64, f64)> = rows.iter().map(|r| (key(&r.item), r.y, r.h)).collect();
        let old: Vec<Old> = self.old.iter().map(|(i, y, h, laid)| Old { key: key(i), y: *y, h: *h, laid: *laid, leaving: self.leaving.contains(&key(i)) }).collect();
        let plan = plan(&old, &now, p);
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        let mut shifts = self.shifts.borrow_mut();
        shifts.clear();
        for (row, place) in rows.iter().zip(&plan.rows) {
            let Some(layer) = row.view.layer() else { continue };
            let moved = place.shift.abs() >= 0.01 || place.visible.is_some();
            let touched = self.touched.borrow().iter().any(|v| std::ptr::eq(&**v, &*row.view));
            if !moved && !touched { continue }
            layer.setAffineTransform(CGAffineTransform { a: 1.0, b: 0.0, c: 0.0, d: 1.0, tx: 0.0, ty: down * place.shift });
            match place.visible {
                // Only the part of an arriving row the rows below have uncovered shows.
                Some((top, bottom)) => {
                    let mask = layer.mask().unwrap_or_else(|| {
                        let mask = CALayer::new();
                        mask.setBackgroundColor(Some(&objc2_core_graphics::CGColor::new_srgb(0.0, 0.0, 0.0, 1.0)));
                        mask
                    });
                    let h = (bottom - top).max(0.0);
                    let y = if layer.contentsAreFlipped() { top } else { row.h - bottom };
                    mask.setFrame(cg(0.0, y, row.view.frame().size.width, h));
                    unsafe { layer.setMask(Some(&mask)) };
                }
                None => unsafe { layer.setMask(None) },
            }
            if moved && !touched { self.touched.borrow_mut().push(row.view.clone()); }
            shifts.push((key(&row.item), place.shift));
        }
        for row in &undrawn {
            let Some(layer) = row.view.layer() else { continue };
            // An empty mask: nothing of the undrawn view shows.
            let mask = layer.mask().unwrap_or_else(CALayer::new);
            mask.setFrame(cg(0.0, 0.0, 0.0, 0.0));
            unsafe { layer.setMask(Some(&mask)) };
            if !self.touched.borrow().iter().any(|v| std::ptr::eq(&**v, &*row.view)) { self.touched.borrow_mut().push(row.view.clone()); }
        }
        let w = self.table.bounds().size.width;
        if let Some(band) = &self.ghost { band.show(&host, height, w, plan.ghost); }
        if let Some(band) = &self.tail { band.show(&host, height, w, plan.tail); }
        CATransaction::commit();
    }
}

/// A row as it was before the change.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Old {
    pub key: usize,
    /// Its top as shown, and its height.
    pub y: f64,
    pub h: f64,
    /// Its top as laid out (where the picture has it).
    pub laid: f64,
    /// The change hides it.
    pub leaving: bool,
}

/// Where each row shows at progress `p` (0: as before the change, 1: its layout place).
#[derive(Debug, PartialEq)]
pub(crate) struct Place {
    /// Points down from its layout place.
    pub shift: f64,
    /// For an arriving row, the part that shows (top, bottom in its own points); `None`: all of it.
    pub visible: Option<(f64, f64)>,
}

/// A band of the picture to show: from `from` in the picture, at `top` in the table, `shown` points of it.
#[derive(Debug, PartialEq, Clone, Copy)]
pub(crate) struct BandAt {
    pub from: f64,
    pub top: f64,
    pub shown: f64,
}

#[derive(Debug, PartialEq)]
pub(crate) struct Plan {
    /// One per row now laid out, in `now`'s order.
    pub rows: Vec<Place>,
    /// The leaving rows, from the picture.
    pub ghost: Option<BandAt>,
    /// The staying rows the table has no views for yet (the last ones, below the window's edge as it grows), from the
    /// picture.
    pub tail: Option<BandAt>,
}

/// The rows' places at progress `p`: `old` the rows before the change and `now` as laid out after it (each
/// `(key, top, height)`, top first, in the table's points with y down).
///
/// A row in both moves from where it showed to its layout place. An arriving row rides under the staying row above
/// it and shows only above the staying row below it (as it slides away). The leaving rows' picture hangs under the
/// staying row above them and shows down to the staying row below. Staying rows the table has no view for yet are
/// drawn from the picture, moving as the staying row above them does. So the rows after the change ride on the
/// window's bottom edge, which moves by the same progress.
pub(crate) fn plan(old: &[Old], now: &[(usize, f64, f64)], p: f64) -> Plan {
    let p = p.clamp(0.0, 1.0);
    let shift_of = |key: usize, y: f64| old.iter().find(|o| o.key == key).map(|o| (o.y - y) * (1.0 - p));
    // Each staying row's shift and where it shows.
    let staying: Vec<Option<(f64, f64, f64)>> = now.iter().map(|(k, y, h)| shift_of(*k, *y).map(|s| (s, y + s, y + s + h))).collect();
    let mut rows = Vec::with_capacity(now.len());
    for (i, (_, y, h)) in now.iter().enumerate() {
        if let Some((s, _, _)) = staying[i] { rows.push(Place { shift: s, visible: None }); continue }
        let above = staying[..i].iter().rev().flatten().next();
        let below = staying[i + 1..].iter().flatten().next();
        let shift = above.map_or(0.0, |a| a.0);
        let top = y + shift;
        let floor = below.map_or(f64::MAX, |b| b.1);
        let ceiling = above.map_or(f64::MIN, |a| a.2);
        let (vt, vb) = ((ceiling - top).max(0.0).min(*h), (floor - top).max(0.0).min(*h));
        let visible = if vt <= 0.0 && vb >= *h { None } else { Some((vt, vb.max(vt))) };
        rows.push(Place { shift, visible });
    }
    // A staying row's place as shown now, by its key.
    let shown = |key: usize| now.iter().position(|n| n.0 == key).and_then(|i| staying[i]);
    let ghost = (|| {
        let start = old.iter().position(|o| o.leaving)?;
        let end = old.iter().rposition(|o| o.leaving)?;
        let (first, last) = (&old[start], &old[end]);
        let (gtop, gh) = (first.y, last.y + last.h - first.y);
        // The staying rows around the band, by their order (a press during a motion leaves rows overlapping).
        let above = old[..start].iter().rev().filter(|o| !o.leaving).find_map(|o| shown(o.key));
        let below = old[end + 1..].iter().filter(|o| !o.leaving).find_map(|o| shown(o.key));
        let top = above.map_or(gtop, |a| a.2);
        let floor = below.map_or(f64::MAX, |b| b.1);
        Some(BandAt { from: first.laid, top, shown: (floor - top).clamp(0.0, gh) })
    })();
    let tail = (|| {
        // The first staying row without a view, and the last before it with one: the rows from there down move with it.
        let at = old.iter().position(|o| !o.leaving && shown(o.key).is_none() && !now.iter().any(|n| n.0 == o.key))?;
        let anchor = old[..at].iter().rev().filter(|o| !o.leaving).find_map(|o| shown(o.key).map(|s| (o, s)))?;
        let d = anchor.1 .1 - anchor.0.y;
        let missing = &old[at];
        let end = old.iter().filter(|o| !o.leaving).map(|o| o.laid + o.h).fold(0.0, f64::max);
        Some(BandAt { from: missing.laid, top: missing.y + d, shown: end - missing.laid })
    })();
    Plan { rows, ghost, tail }
}

/// Resizes under way (checks: none once every motion has ended).
pub(crate) fn running() -> usize {
    RESIZES.with(|r| r.borrow().len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eases_out_and_ends_in_place() {
        assert_eq!(ease(0.0), 0.0);
        assert_eq!(ease(1.0), 1.0);
        assert_eq!(ease(2.0), 1.0);
        // Most of the way in the first half, as a settling motion.
        assert!(ease(0.5) > 0.8);
        let a = NSRect::new(NSPoint::new(1204.0, 30.0), NSSize::new(211.0, 236.0));
        let b = NSRect::new(NSPoint::new(1204.0, 6.0), NSSize::new(211.0, 260.0));
        assert!(same(between(a, b, 1.0), b) && same(between(a, b, 0.0), a));
        let mid = between(a, b, 0.5);
        assert_eq!((mid.origin.y, mid.size.height), (18.0, 248.0));
        // The top edge stays put while a menu grows down.
        assert_eq!(mid.origin.y + mid.size.height, a.origin.y + a.size.height);
    }

    fn old(rows: &[(usize, f64, f64)], leaving: &[usize]) -> Vec<Old> {
        rows.iter().map(|&(key, y, h)| Old { key, y, h, laid: y, leaving: leaving.contains(&key) }).collect()
    }

    /// 31 Oct, recorded live: the rows jumped to their new places in the first frame while the window's edge took
    /// 200 ms to follow (cut-off rows opening, an empty band folding). The rows below a change ride on the bottom edge
    /// at every step, and arriving rows show only where the rows below have uncovered them.
    #[test]
    fn rows_ride_on_the_edge_through_an_opening_and_a_fold() {
        // Header A (0), header B (24), Quit (48); B opens two rows (48, 72), Quit goes to 96.
        let before = [(1, 0.0, 24.0), (2, 24.0, 24.0), (9, 48.0, 24.0)];
        let after = [(1, 0.0, 24.0), (2, 24.0, 24.0), (5, 48.0, 24.0), (6, 72.0, 24.0), (9, 96.0, 24.0)];
        let (h0, h1) = (72.0, 120.0);
        for p in [0.0, 0.1, 0.25, 0.5, 0.8, 1.0] {
            let plan = plan(&old(&before, &[]), &after, p);
            let bottom = h0 + (h1 - h0) * p;
            // Quit's bottom on the window's bottom edge; the headers still.
            assert!((96.0 + plan.rows[4].shift + 24.0 - bottom).abs() < 1e-9, "p {p}");
            assert_eq!((plan.rows[0].shift, plan.rows[1].shift), (0.0, 0.0));
            // The arriving rows show above Quit only.
            let quit_top = 96.0 + plan.rows[4].shift;
            for (i, y) in [(2, 48.0), (3, 72.0)] {
                let shown = plan.rows[i].visible.map_or(24.0, |(t, b)| b - t);
                assert!((shown - (quit_top - y).clamp(0.0, 24.0)).abs() < 1e-9, "p {p} row {i}");
            }
            assert_eq!((plan.ghost, plan.tail), (None, None));
        }
        assert_eq!(plan(&old(&before, &[]), &after, 1.0).rows.iter().filter(|r| r.visible.is_some() || r.shift != 0.0).count(), 0, "at the end, AppKit's own layout");
        // The fold back: the two rows leave (drawn from the picture, band 48..96), Quit rides up on the edge.
        let fold = plan(&old(&after, &[5, 6]), &before, 0.5);
        assert!((48.0 + fold.rows[2].shift - 72.0).abs() < 1e-9, "Quit halfway: {fold:?}");
        // The picture hangs under B and shows down to Quit.
        assert_eq!(fold.ghost, Some(BandAt { from: 48.0, top: 48.0, shown: 24.0 }));
        assert_eq!(plan(&old(&after, &[5, 6]), &before, 1.0).ghost.map(|g| g.shown), Some(0.0));
        // Switching: A's row (24) folds above while B opens below; B rides up, its row under it, Quit to the edge.
        let before = [(1, 0.0, 24.0), (3, 24.0, 24.0), (2, 48.0, 24.0), (9, 72.0, 24.0)];
        let after = [(1, 0.0, 24.0), (2, 24.0, 24.0), (5, 48.0, 24.0), (9, 72.0, 24.0)];
        let plan = plan(&old(&before, &[3]), &after, 0.5);
        assert_eq!(plan.rows[1].shift, 12.0);
        assert_eq!(plan.rows[2].shift, 12.0, "the arriving row rides under its header");
        assert_eq!(plan.rows[3].shift, 0.0, "the height did not change: Quit stays");
        assert_eq!(plan.ghost, Some(BandAt { from: 24.0, top: 24.0, shown: 12.0 }), "A's row shows down to B as B slides up over it");
    }

    /// Code review, 31 Oct: a second press while a section is still opening (Quit shown at 60, over the arriving rows'
    /// band 48..96): the leaving rows show only down to Quit, never over it.
    #[test]
    fn a_press_during_a_motion_never_draws_leaving_rows_over_the_rows_below() {
        let before = [Old { key: 1, y: 0.0, h: 24.0, laid: 0.0, leaving: false }, Old { key: 2, y: 24.0, h: 24.0, laid: 24.0, leaving: false },
            Old { key: 5, y: 48.0, h: 24.0, laid: 48.0, leaving: true }, Old { key: 6, y: 72.0, h: 24.0, laid: 72.0, leaving: true },
            Old { key: 9, y: 60.0, h: 24.0, laid: 96.0, leaving: false }];
        let after = [(1, 0.0, 24.0), (2, 24.0, 24.0), (9, 48.0, 24.0)];
        let plan = plan(&before, &after, 0.0);
        assert_eq!(plan.ghost, Some(BandAt { from: 48.0, top: 48.0, shown: 12.0 }));
    }

    /// 31 Oct, recorded live: the table makes row views only inside its window, so while a section opens the last rows
    /// (laid out below the edge) have none; they showed late, leaving the bottom empty. They are drawn from the picture,
    /// riding with the last row that has a view.
    #[test]
    fn rows_without_views_ride_from_the_picture() {
        let before = [(1, 0.0, 24.0), (7, 24.0, 24.0), (8, 48.0, 24.0), (9, 72.0, 24.0)];
        // Row 1 opens two rows; the table has views down to 7 only (8 and 9 lie below the window's edge).
        let after = [(1, 0.0, 24.0), (5, 24.0, 24.0), (6, 48.0, 24.0), (7, 72.0, 24.0)];
        let plan = plan(&old(&before, &[]), &after, 0.25);
        // Row 7 shows at 24 + 48 * 0.25 = 36; 8 and 9 under it, from the picture's 48 down.
        assert_eq!(plan.rows[3].shift, -36.0);
        assert_eq!(plan.tail, Some(BandAt { from: 48.0, top: 60.0, shown: 48.0 }));
    }}
