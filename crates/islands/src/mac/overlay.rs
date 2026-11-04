//! Keeping islands still while Spaces slide (Swift `OverlaySpace.swift`).
//!
//! `canJoinAllSpaces` puts a window on every Space, but WindowServer still slides it out with the old Space and in
//! with the new one. The windows that stay still (menu bar, Dock, notch overlays) live in a Space of their own above
//! the user Spaces; SkyLight lets a process create one: CGSSpaceCreate + CGSSpaceSetAbsoluteLevel + CGSShowSpaces,
//! then CGSAddWindowsToSpaces. The symbols are private, so they are looked up at run time; if any is missing the
//! islands fall back to canJoinAllSpaces (they still show on every Space, they just slide).
//! `FI_OVERLAY=standard` keeps plain windows for comparison recordings.

use std::ffi::c_void;

use objc2::rc::Retained;
use objc2_foundation::{NSArray, NSNumber};

type Conn = i32;
type SpaceId = u64;
type DefaultConnection = unsafe extern "C" fn() -> Conn;
type SpaceCreate = unsafe extern "C" fn(Conn, i32, *const c_void) -> SpaceId;
type SpaceSetLevel = unsafe extern "C" fn(Conn, SpaceId, i32);
type ShowSpaces = unsafe extern "C" fn(Conn, *const c_void);
type WindowsToSpaces = unsafe extern "C" fn(Conn, *const c_void, *const c_void);

pub struct OverlaySpace {
    conn: Conn,
    space: SpaceId,
    add: Option<WindowsToSpaces>,
    pub active: bool,
}

thread_local! {
    static SHARED: OverlaySpace = OverlaySpace::new();
}

fn array(values: &[u64]) -> Retained<NSArray<NSNumber>> {
    let n: Vec<Retained<NSNumber>> = values.iter().map(|v| NSNumber::new_u64(*v)).collect();
    NSArray::from_retained_slice(&n)
}

impl OverlaySpace {
    fn new() -> OverlaySpace {
        let off = OverlaySpace { conn: 0, space: 0, add: None, active: false };
        if std::env::var("FI_OVERLAY").as_deref() == Ok("standard") {
            return off;
        }
        // SAFETY: dlopen/dlsym of SkyLight; each symbol is cast to its known C signature.
        unsafe {
            let h = libc::dlopen(c"/System/Library/PrivateFrameworks/SkyLight.framework/SkyLight".as_ptr(), libc::RTLD_LAZY);
            if h.is_null() {
                return off;
            }
            let sym = |n: &std::ffi::CStr| {
                let p = libc::dlsym(h, n.as_ptr());
                (!p.is_null()).then_some(p)
            };
            let (Some(dc), Some(create), Some(level), Some(show), Some(add)) = (
                sym(c"_CGSDefaultConnection"),
                sym(c"CGSSpaceCreate"),
                sym(c"CGSSpaceSetAbsoluteLevel"),
                sym(c"CGSShowSpaces"),
                sym(c"CGSAddWindowsToSpaces"),
            ) else {
                return off;
            };
            let dc: DefaultConnection = std::mem::transmute(dc);
            let create: SpaceCreate = std::mem::transmute(create);
            let level: SpaceSetLevel = std::mem::transmute(level);
            let show: ShowSpaces = std::mem::transmute(show);
            let add: WindowsToSpaces = std::mem::transmute(add);
            let conn = dc();
            let space = create(conn, 1, std::ptr::null());
            level(conn, space, 100);
            let a = array(&[space]);
            show(conn, Retained::as_ptr(&a) as *const c_void);
            OverlaySpace { conn, space, add: Some(add), active: space != 0 }
        }
    }

    /// Move a window into the overlay space (once its window number exists).
    pub fn adopt(window_number: isize) {
        SHARED.with(|s| {
            let (true, Some(add)) = (s.active, s.add) else { return };
            if window_number <= 0 {
                return;
            }
            let w = array(&[window_number as u64]);
            let sp = array(&[s.space]);
            // SAFETY: CGSAddWindowsToSpaces with two CFArrays of numbers.
            unsafe { add(s.conn, Retained::as_ptr(&w) as *const c_void, Retained::as_ptr(&sp) as *const c_void) };
        });
    }

    pub fn is_active() -> bool {
        SHARED.with(|s| s.active)
    }
}
