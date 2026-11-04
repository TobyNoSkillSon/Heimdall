//! The main queue, one-shot timers and kernel event sources (file, signal, process exit) through libdispatch, the way
//! the Swift build uses `DispatchQueue.main` and `DispatchSource`. Everything here runs on the main thread: closures
//! may capture `Rc`s.

use std::cell::Cell;
use std::ffi::c_void;
use std::rc::Rc;

#[allow(non_upper_case_globals)]
extern "C" {
    static _dispatch_main_q: c_void;
    static _dispatch_source_type_vnode: c_void;
    static _dispatch_source_type_signal: c_void;
    static _dispatch_source_type_proc: c_void;
    static _dispatch_source_type_timer: c_void;
    fn dispatch_source_set_timer(source: *mut c_void, start: u64, interval: u64, leeway: u64);
    fn dispatch_time(when: u64, delta: i64) -> u64;
    fn dispatch_walltime(when: *const libc::timespec, delta: i64) -> u64;
    fn dispatch_after_f(when: u64, queue: *const c_void, ctx: *mut c_void, work: extern "C" fn(*mut c_void));
    fn dispatch_async_f(queue: *const c_void, ctx: *mut c_void, work: extern "C" fn(*mut c_void));
    fn dispatch_source_create(kind: *const c_void, handle: usize, mask: usize, queue: *const c_void) -> *mut c_void;
    fn dispatch_set_context(obj: *mut c_void, ctx: *mut c_void);
    fn dispatch_source_set_event_handler_f(source: *mut c_void, handler: extern "C" fn(*mut c_void));
    fn dispatch_source_set_cancel_handler_f(source: *mut c_void, handler: extern "C" fn(*mut c_void));
    fn dispatch_source_get_data(source: *mut c_void) -> usize;
    fn dispatch_resume(obj: *mut c_void);
    fn dispatch_source_cancel(source: *mut c_void);
    fn dispatch_release(obj: *mut c_void);
}

#[link(name = "QuartzCore", kind = "framework")]
extern "C" {
    fn CACurrentMediaTime() -> f64;
}

/// Core Animation's clock (mach absolute time in seconds).
pub fn media_time() -> f64 {
    // SAFETY: a pure clock read.
    unsafe { CACurrentMediaTime() }
}

/// Seconds since boot (`ProcessInfo.systemUptime`, the clock NSEvent timestamps use).
pub fn uptime() -> f64 {
    media_time()
}

fn main_q() -> *const c_void {
    // SAFETY: the main queue is a static object.
    unsafe { &_dispatch_main_q as *const c_void }
}

type Work = Box<dyn FnOnce()>;

extern "C" fn run_work(ctx: *mut c_void) {
    // SAFETY: `ctx` is the Box<Work> leaked by `after`/`soon`, run exactly once.
    let w: Box<Work> = unsafe { Box::from_raw(ctx as *mut Work) };
    w();
}

/// Runs `f` on the main queue after `secs` (`DispatchQueue.main.asyncAfter`).
pub fn after(secs: f64, f: impl FnOnce() + 'static) {
    let w: Box<Work> = Box::new(Box::new(f));
    let ns = (secs.max(0.0) * 1e9) as i64;
    // SAFETY: the context is a leaked Box consumed once by `run_work` on the main queue.
    unsafe { dispatch_after_f(dispatch_time(0, ns), main_q(), Box::into_raw(w) as *mut c_void, run_work) };
}

/// Runs `f` on the next turn of the main queue (`DispatchQueue.main.async`).
pub fn soon(f: impl FnOnce() + 'static) {
    let w: Box<Work> = Box::new(Box::new(f));
    // SAFETY: as in `after`.
    unsafe { dispatch_async_f(main_q(), Box::into_raw(w) as *mut c_void, run_work) };
}

/// A one-shot on the wall clock (`wallDeadline`, 500 ms leeway): a Mac that slept wakes to it on time. Dropping it
/// cancels it.
pub struct WallTimer {
    _src: Source,
}

pub fn after_wall(secs: f64, f: impl FnOnce() + 'static) -> Option<WallTimer> {
    let f = std::cell::RefCell::new(Some(f));
    // SAFETY: a timer source on the main queue; the handler runs at most once.
    unsafe {
        let src = Source::make(&_dispatch_source_type_timer, 0, 0, -1, Box::new(move |_| {
            if let Some(f) = f.borrow_mut().take() {
                f();
            }
        }))?;
        let ns = (secs.max(0.0) * 1e9) as i64;
        dispatch_source_set_timer(src.raw, dispatch_walltime(std::ptr::null(), ns), u64::MAX, 500_000_000);
        Some(WallTimer { _src: src })
    }
}

/// A repeating timer on the wall clock that fires on whole multiples of `every` seconds (a 60 s one on the minute), with
/// `leeway` so the system can fire it together with other work; dropping it stops it.
pub struct Ticker {
    _src: Source,
}

/// Seconds from `now` (epoch seconds) to the next whole multiple of `every`, never zero.
pub fn to_next_multiple(now: f64, every: f64) -> f64 {
    let left = every - now.rem_euclid(every);
    if left <= 1e-3 {
        left + every
    } else {
        left
    }
}

pub fn ticker(every: f64, leeway: f64, f: impl Fn() + 'static) -> Option<Ticker> {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0);
    let first = to_next_multiple(now, every);
    // SAFETY: a timer source on the main queue.
    unsafe {
        let src = Source::make(&_dispatch_source_type_timer, 0, 0, -1, Box::new(move |_| f()))?;
        dispatch_source_set_timer(src.raw, dispatch_walltime(std::ptr::null(), (first * 1e9) as i64), (every * 1e9) as u64, (leeway * 1e9) as u64);
        Some(Ticker { _src: src })
    }
}

/// A cancellable one-shot (`DispatchWorkItem`): cancelling it stops the work from running.
#[derive(Clone, Default)]
pub struct Work1 {
    live: Rc<Cell<bool>>,
}

impl Work1 {
    pub fn cancel(&self) {
        self.live.set(false);
    }
    pub fn is_live(&self) -> bool {
        self.live.get()
    }
}

/// `after`, cancellable.
pub fn after_cancellable(secs: f64, f: impl FnOnce() + 'static) -> Work1 {
    let live = Rc::new(Cell::new(true));
    let l = live.clone();
    after(secs, move || {
        if l.get() {
            l.set(false);
            f();
        }
    });
    Work1 { live }
}

/// A repeating timer on the main queue (first fire after `first`, then every `every`), stopped when dropped.
pub struct Repeat {
    live: Rc<Cell<bool>>,
}

impl Repeat {
    pub fn new(first: f64, every: f64, f: impl Fn() + 'static) -> Repeat {
        let live = Rc::new(Cell::new(true));
        fn arm(secs: f64, every: f64, live: Rc<Cell<bool>>, f: Rc<dyn Fn()>) {
            after(secs, move || {
                if !live.get() {
                    return;
                }
                f();
                if live.get() {
                    arm(every, every, live, f);
                }
            });
        }
        arm(first, every, live.clone(), Rc::new(f));
        Repeat { live }
    }
}

impl Drop for Repeat {
    fn drop(&mut self) {
        self.live.set(false);
    }
}

struct SourceCtx {
    handler: Box<dyn Fn(usize)>,
    fd: i32,
    raw: *mut c_void,
}

extern "C" fn source_event(ctx: *mut c_void) {
    // SAFETY: the context outlives the source until its cancel handler runs.
    let c = unsafe { &*(ctx as *const SourceCtx) };
    // SAFETY: `raw` is this context's live source.
    let data = unsafe { dispatch_source_get_data(c.raw) };
    (c.handler)(data);
}

extern "C" fn source_cancel(ctx: *mut c_void) {
    // SAFETY: the cancel handler runs once, after the last event handler: the context can go.
    let c: Box<SourceCtx> = unsafe { Box::from_raw(ctx as *mut SourceCtx) };
    if c.fd >= 0 {
        // SAFETY: the descriptor this source owned.
        unsafe { libc::close(c.fd) };
    }
}

/// A kernel event source on the main queue, cancelled (and its descriptor closed) when dropped.
pub struct Source {
    raw: *mut c_void,
}

pub const VNODE_DELETE: usize = 0x1;
pub const VNODE_WRITE: usize = 0x2;
pub const VNODE_EXTEND: usize = 0x4;
pub const VNODE_ATTRIB: usize = 0x8;
pub const VNODE_RENAME: usize = 0x20;
pub const VNODE_REVOKE: usize = 0x40;
const PROC_EXIT: usize = 0x8000_0000;

impl Source {
    fn make(kind: *const c_void, handle: usize, mask: usize, fd: i32, handler: Box<dyn Fn(usize)>) -> Option<Source> {
        // SAFETY: plain libdispatch calls; the context is freed by the cancel handler.
        unsafe {
            let raw = dispatch_source_create(kind, handle, mask, main_q());
            if raw.is_null() {
                if fd >= 0 {
                    libc::close(fd);
                }
                return None;
            }
            let ctx = Box::into_raw(Box::new(SourceCtx { handler, fd, raw }));
            dispatch_set_context(raw, ctx as *mut c_void);
            dispatch_source_set_event_handler_f(raw, source_event);
            dispatch_source_set_cancel_handler_f(raw, source_cancel);
            dispatch_resume(raw);
            Some(Source { raw })
        }
    }

    /// Watches a file or folder (`O_EVTONLY`) for the given vnode events.
    pub fn vnode(path: &std::path::Path, mask: usize, handler: impl Fn(usize) + 'static) -> Option<Source> {
        use std::os::unix::ffi::OsStrExt;
        let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
        // SAFETY: open for events only; the source owns the descriptor.
        let fd = unsafe { libc::open(c.as_ptr(), libc::O_EVTONLY) };
        if fd < 0 {
            return None;
        }
        // SAFETY: a framework static.
        Self::make(unsafe { &_dispatch_source_type_vnode }, fd as usize, mask, fd, Box::new(handler))
    }

    pub fn signal(sig: i32, handler: impl Fn() + 'static) -> Option<Source> {
        // SAFETY: as above.
        Self::make(unsafe { &_dispatch_source_type_signal }, sig as usize, 0, -1, Box::new(move |_| handler()))
    }

    pub fn process_exit(pid: i32, handler: impl Fn() + 'static) -> Option<Source> {
        // SAFETY: as above.
        Self::make(unsafe { &_dispatch_source_type_proc }, pid as usize, PROC_EXIT, -1, Box::new(move |_| handler()))
    }
}

impl Drop for Source {
    fn drop(&mut self) {
        // SAFETY: cancel then drop our reference; the cancel handler frees the context.
        unsafe {
            dispatch_source_cancel(self.raw);
            dispatch_release(self.raw);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::to_next_multiple;

    #[test]
    fn ticker_lands_on_the_minute() {
        assert!((to_next_multiple(120.0 + 15.5, 60.0) - 44.5).abs() < 1e-9);
        assert!((to_next_multiple(1_793_000_000.25, 60.0) - (60.0 - 20.25)).abs() < 1e-6);
        // Exactly on the minute: the next one, not now.
        assert_eq!(to_next_multiple(180.0, 60.0), 60.0);
    }
}
