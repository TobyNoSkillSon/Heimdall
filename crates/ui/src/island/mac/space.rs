//! The island's windows stay still while Spaces slide (bug note 2026-10-21T00-07-17-2; "it should
//! always be in front, an overlay on top of whatever is behind").
//!
//! `overlay_behavior()` (canJoinAllSpaces, stationary…) puts a window on every Space, but it still belongs to the Space
//! stack: a three-finger swipe slides it out with the old Space and back in with the new one. The windows that stay
//! still (menu bar, Dock) live in a Space of their own above the user Spaces. SkyLight lets a process make one:
//! `CGSSpaceCreate` → `CGSSpaceSetAbsoluteLevel` 100 → `CGSShowSpaces`, then `CGSAddWindowsToSpaces` per window
//! (proven in Projects/FloatingIslands, `OverlaySpace.swift`, with recordings of both modes).
//!
//! The symbols are private: they are looked up at run time (`dlopen`/`dlsym`), so a missing one never crashes or
//! fails to link. The overlay space is added on top of today's path, never instead of it: collection behaviour and
//! window levels are untouched, so when any step is unavailable the windows keep showing on every Space (they just
//! slide), and one `island: overlay space …` line in heimdall.log says which step. A window joins again every time it
//! is ordered in (ordering out may drop it from the space), and every window joins again after a display change and is
//! checked at its next order-in; if the space itself was lost, it is made again. On quit the windows leave and the
//! space is destroyed.
//!
//! The ordering and fallback logic is `Overlay<S: SkyLight>`, tested with a fake SkyLight that fails at each step.
//! The real swipe cannot be tested here (it needs a real Space switch on screen).

/// The overlay space's absolute level (above the user Spaces; FloatingIslands' measured value).
pub(crate) const LEVEL: i32 = 100;

pub(crate) type Conn = i32;
pub(crate) type SpaceId = u64;
/// A window's WindowServer number (`NSWindow.windowNumber`).
pub(crate) type WindowId = i64;

/// The SkyLight calls the overlay needs. Every call that has no result in SkyLight returns nothing here too; a missing
/// required symbol is reported by `missing` before any call is made.
pub(crate) trait SkyLight {
    /// The first required symbol this SkyLight lacks.
    fn missing(&self) -> Option<&'static str>;
    fn connection(&mut self) -> Conn;
    /// 0: no space was made.
    fn create(&mut self, conn: Conn) -> SpaceId;
    fn set_level(&mut self, conn: Conn, space: SpaceId, level: i32);
    fn show(&mut self, conn: Conn, space: SpaceId);
    fn add(&mut self, conn: Conn, windows: &[WindowId], space: SpaceId);
    /// The spaces `window` is in; `None` when this SkyLight cannot say (optional symbol missing).
    fn spaces_of(&mut self, conn: Conn, window: WindowId) -> Option<Vec<SpaceId>>;
    /// Clean-up calls; false when the optional symbol is missing.
    fn remove(&mut self, conn: Conn, windows: &[WindowId], space: SpaceId) -> bool;
    fn hide(&mut self, conn: Conn, space: SpaceId) -> bool;
    fn destroy(&mut self, conn: Conn, space: SpaceId) -> bool;
}

#[derive(Clone, Debug, PartialEq)]
enum State {
    Untried,
    Ready { conn: Conn, space: SpaceId },
    Off,
    Released,
}

/// Whether a joined window is in the space.
enum Check {
    In,
    Unverified,
    Out,
}

pub(crate) struct Overlay<S: SkyLight> {
    sky: S,
    state: State,
    /// Every window that joined (number, title), in joining order: re-joined after a display change, removed on quit.
    members: Vec<(WindowId, String)>,
    /// Windows whose membership was checked (and logged) in the current space.
    checked: Vec<WindowId>,
    /// heimdall.log lines not yet written (`island: …`).
    lines: Vec<String>,
}

impl<S: SkyLight> Overlay<S> {
    pub(crate) fn new(sky: S) -> Self {
        Overlay { sky, state: State::Untried, members: Vec::new(), checked: Vec::new(), lines: Vec::new() }
    }

    /// The lines to write to heimdall.log since the last call.
    pub(crate) fn take_lines(&mut self) -> Vec<String> {
        std::mem::take(&mut self.lines)
    }

    /// The space, once made; `None` while untried, off or released.
    #[cfg(test)]
    pub(crate) fn space(&self) -> Option<SpaceId> {
        match self.state {
            State::Ready { space, .. } => Some(space),
            _ => None,
        }
    }

    /// The overlay is off for good (a step failed or a symbol is missing): windows keep today's path.
    #[cfg(test)]
    pub(crate) fn is_off(&self) -> bool {
        self.state == State::Off
    }

    /// Makes the space on first use: create → absolute level → show.
    fn ensure(&mut self) -> Option<(Conn, SpaceId)> {
        match self.state {
            State::Ready { conn, space } => return Some((conn, space)),
            State::Off | State::Released => return None,
            State::Untried => {}
        }
        if let Some(symbol) = self.sky.missing() {
            self.off(&format!("SkyLight has no {symbol}"));
            return None;
        }
        let conn = self.sky.connection();
        if conn == 0 {
            self.off("no SkyLight connection");
            return None;
        }
        let space = self.make(conn)?;
        self.state = State::Ready { conn, space };
        Some((conn, space))
    }

    fn make(&mut self, conn: Conn) -> Option<SpaceId> {
        let space = self.sky.create(conn);
        if space == 0 {
            self.off("CGSSpaceCreate made no space");
            return None;
        }
        self.sky.set_level(conn, space, LEVEL);
        self.sky.show(conn, space);
        Some(space)
    }

    /// A window was ordered in: it joins the space (every time; ordering out may drop it). The first join of each
    /// window in a space is checked and logged.
    pub(crate) fn join(&mut self, window: WindowId, title: &str) {
        if window <= 0 {
            return;
        }
        let Some((conn, space)) = self.ensure() else { return };
        self.sky.add(conn, &[window], space);
        if !self.members.iter().any(|(w, _)| *w == window) {
            self.members.push((window, title.to_owned()));
        }
        if self.checked.contains(&window) {
            return;
        }
        match self.check(conn, space, window) {
            Check::In => self.joined(space, window, title, "verified"),
            Check::Unverified => self.joined(space, window, title, "unverified: SkyLight cannot list a window's spaces"),
            Check::Out => {
                // The space may have been lost (WindowServer reset): make it again once, with every window.
                let Some(space) = self.remake(conn, space) else { return };
                match self.check(conn, space, window) {
                    Check::Out => self.off(&format!("{title} is not in space {space} after joining it")),
                    _ => self.joined(space, window, title, "verified after making the space again"),
                }
            }
        }
    }

    fn check(&mut self, conn: Conn, space: SpaceId, window: WindowId) -> Check {
        match self.sky.spaces_of(conn, window) {
            None => Check::Unverified,
            Some(spaces) if spaces.contains(&space) => Check::In,
            Some(_) => Check::Out,
        }
    }

    fn joined(&mut self, space: SpaceId, window: WindowId, title: &str, how: &str) {
        if !self.checked.contains(&window) {
            self.checked.push(window);
        }
        self.lines.push(format!("island: overlay space {space} (level {LEVEL}) joined by {title} ({how})"));
    }

    /// Destroys `old` (best effort), makes a new space and adds every member. `None`: off.
    fn remake(&mut self, conn: Conn, old: SpaceId) -> Option<SpaceId> {
        self.sky.destroy(conn, old);
        self.checked.clear();
        self.state = State::Untried;
        let space = self.make(conn)?;
        self.state = State::Ready { conn, space };
        let windows: Vec<WindowId> = self.members.iter().map(|(w, _)| *w).collect();
        if !windows.is_empty() {
            self.sky.add(conn, &windows, space);
        }
        self.lines.push(format!("island: overlay space {old} was lost; made space {space} and re-joined {} window(s)", windows.len()));
        Some(space)
    }

    /// The displays changed (added, removed, rearranged, resolution): the space is shown again and every window joins
    /// again. Membership is not checked here: the island panel is ordered out at rest, and an ordered-out window is in
    /// no space, so a check now would read a healthy space as lost and turn the overlay off (night 3 review). Each
    /// window is checked again at its next order-in (`join`), which makes the space again if it was lost.
    pub(crate) fn screens_changed(&mut self) {
        let State::Ready { conn, space } = self.state else { return };
        self.sky.show(conn, space);
        let windows: Vec<WindowId> = self.members.iter().map(|(w, _)| *w).collect();
        if windows.is_empty() {
            return;
        }
        self.sky.add(conn, &windows, space);
        self.checked.clear();
        self.lines.push(format!("island: overlay space {space} shown again after a display change; {} window(s) re-joined (checked at their next order-in)", windows.len()));
    }

    /// Quit: every window leaves, the space is hidden and destroyed. Nothing joins afterwards.
    pub(crate) fn release(&mut self) {
        let State::Ready { conn, space } = self.state else {
            self.state = State::Released;
            return;
        };
        let windows: Vec<WindowId> = self.members.iter().map(|(w, _)| *w).collect();
        let removed = windows.is_empty() || self.sky.remove(conn, &windows, space);
        let hidden = self.sky.hide(conn, space);
        let destroyed = self.sky.destroy(conn, space);
        self.state = State::Released;
        self.lines.push(if removed && hidden && destroyed {
            format!("island: overlay space {space} released")
        } else {
            format!("island: overlay space {space} left to WindowServer at exit (SkyLight lacks its clean-up calls)")
        });
    }

    /// One line, once: the overlay is off and the windows keep today's path.
    fn off(&mut self, why: &str) {
        if let State::Ready { conn, space } = self.state {
            if !self.members.is_empty() {
                let windows: Vec<WindowId> = self.members.iter().map(|(w, _)| *w).collect();
                self.sky.remove(conn, &windows, space);
            }
            self.sky.hide(conn, space);
            self.sky.destroy(conn, space);
        }
        self.state = State::Off;
        self.lines.push(format!(
            "island: overlay space unavailable ({why}); island windows keep canJoinAllSpaces at their levels and slide with Space swipes"
        ));
    }
}

/// SkyLight itself, its symbols resolved once at run time.
#[cfg(target_os = "macos")]
pub(crate) mod real {
    use super::{Conn, SkyLight, SpaceId, WindowId};
    use objc2::rc::Retained;
    use objc2_foundation::{NSArray, NSNumber};
    use std::ffi::c_void;

    type DefaultConnection = unsafe extern "C" fn() -> Conn;
    type SpaceCreate = unsafe extern "C" fn(Conn, usize, *const c_void) -> SpaceId;
    type SpaceSetLevel = unsafe extern "C" fn(Conn, SpaceId, i32);
    type Spaces = unsafe extern "C" fn(Conn, *const c_void);
    type WindowsSpaces = unsafe extern "C" fn(Conn, *const c_void, *const c_void);
    type CopySpaces = unsafe extern "C" fn(Conn, i32, *const c_void) -> *mut c_void;
    type SpaceDestroy = unsafe extern "C" fn(Conn, SpaceId);

    const SKYLIGHT: &std::ffi::CStr = c"/System/Library/PrivateFrameworks/SkyLight.framework/SkyLight";
    /// Every kind of space. Window managers' 0x7 (current, other, user) leaves out spaces a process made, so the
    /// overlay space is never listed under it (measured on this Mac, 21 Oct: 0x7 → user spaces only, 0xffff → the
    /// overlay space too, at once after `CGSAddWindowsToSpaces`).
    const ALL_SPACES: i32 = 0xffff;

    pub(crate) struct Real {
        connection: Option<DefaultConnection>,
        create: Option<SpaceCreate>,
        level: Option<SpaceSetLevel>,
        show: Option<Spaces>,
        add: Option<WindowsSpaces>,
        copy: Option<CopySpaces>,
        remove: Option<WindowsSpaces>,
        hide: Option<Spaces>,
        destroy: Option<SpaceDestroy>,
    }

    /// The first of `names` SkyLight exports (its `CGS` names, then the `SLS` ones).
    fn find<T: Copy>(handle: *mut c_void, names: &[&std::ffi::CStr]) -> Option<T> {
        assert_eq!(std::mem::size_of::<T>(), std::mem::size_of::<*mut c_void>());
        if handle.is_null() {
            return None;
        }
        names.iter().find_map(|name| {
            let p = unsafe { libc::dlsym(handle, name.as_ptr()) };
            (!p.is_null()).then(|| unsafe { std::mem::transmute_copy::<*mut c_void, T>(&p) })
        })
    }

    impl Real {
        pub(crate) fn load() -> Real {
            let h = unsafe { libc::dlopen(SKYLIGHT.as_ptr(), libc::RTLD_LAZY) };
            Real {
                connection: find(h, &[c"_CGSDefaultConnection", c"CGSMainConnectionID", c"SLSMainConnectionID"]),
                create: find(h, &[c"CGSSpaceCreate", c"SLSSpaceCreate"]),
                level: find(h, &[c"CGSSpaceSetAbsoluteLevel", c"SLSSpaceSetAbsoluteLevel"]),
                show: find(h, &[c"CGSShowSpaces", c"SLSShowSpaces"]),
                add: find(h, &[c"CGSAddWindowsToSpaces", c"SLSAddWindowsToSpaces"]),
                copy: find(h, &[c"CGSCopySpacesForWindows", c"SLSCopySpacesForWindows"]),
                remove: find(h, &[c"CGSRemoveWindowsFromSpaces", c"SLSRemoveWindowsFromSpaces"]),
                hide: find(h, &[c"CGSHideSpaces", c"SLSHideSpaces"]),
                destroy: find(h, &[c"CGSSpaceDestroy", c"SLSSpaceDestroy"]),
            }
        }
    }

    fn numbers(values: &[u64]) -> Retained<NSArray<NSNumber>> {
        let v: Vec<Retained<NSNumber>> = values.iter().map(|&n| NSNumber::new_u64(n)).collect();
        NSArray::from_retained_slice(&v)
    }

    fn windows(values: &[WindowId]) -> Retained<NSArray<NSNumber>> {
        numbers(&values.iter().map(|&w| w as u64).collect::<Vec<_>>())
    }

    /// An `NSArray` is its toll-free bridged `CFArrayRef`.
    fn cf(a: &NSArray<NSNumber>) -> *const c_void {
        a as *const NSArray<NSNumber> as *const c_void
    }

    impl SkyLight for Real {
        fn missing(&self) -> Option<&'static str> {
            if self.connection.is_none() {
                Some("_CGSDefaultConnection")
            } else if self.create.is_none() {
                Some("CGSSpaceCreate")
            } else if self.level.is_none() {
                Some("CGSSpaceSetAbsoluteLevel")
            } else if self.show.is_none() {
                Some("CGSShowSpaces")
            } else if self.add.is_none() {
                Some("CGSAddWindowsToSpaces")
            } else {
                None
            }
        }
        fn connection(&mut self) -> Conn {
            self.connection.map_or(0, |f| unsafe { f() })
        }
        fn create(&mut self, conn: Conn) -> SpaceId {
            // FloatingIslands' arguments: flag 1, no options.
            self.create.map_or(0, |f| unsafe { f(conn, 1, std::ptr::null()) })
        }
        fn set_level(&mut self, conn: Conn, space: SpaceId, level: i32) {
            if let Some(f) = self.level {
                unsafe { f(conn, space, level) }
            }
        }
        fn show(&mut self, conn: Conn, space: SpaceId) {
            if let Some(f) = self.show {
                let a = numbers(&[space]);
                unsafe { f(conn, cf(&a)) }
            }
        }
        fn add(&mut self, conn: Conn, ws: &[WindowId], space: SpaceId) {
            if let Some(f) = self.add {
                let (w, s) = (windows(ws), numbers(&[space]));
                unsafe { f(conn, cf(&w), cf(&s)) }
            }
        }
        fn spaces_of(&mut self, conn: Conn, window: WindowId) -> Option<Vec<SpaceId>> {
            let f = self.copy?;
            let w = windows(&[window]);
            let raw = unsafe { f(conn, ALL_SPACES, cf(&w)) };
            if raw.is_null() {
                return Some(Vec::new());
            }
            // A Copy function: the array is ours to release.
            let list: Retained<NSArray<NSNumber>> = unsafe { Retained::from_raw(raw as *mut NSArray<NSNumber>)? };
            Some(list.iter().map(|n| n.unsignedLongLongValue()).collect())
        }
        fn remove(&mut self, conn: Conn, ws: &[WindowId], space: SpaceId) -> bool {
            let Some(f) = self.remove else { return false };
            let (w, s) = (windows(ws), numbers(&[space]));
            unsafe { f(conn, cf(&w), cf(&s)) };
            true
        }
        fn hide(&mut self, conn: Conn, space: SpaceId) -> bool {
            let Some(f) = self.hide else { return false };
            let a = numbers(&[space]);
            unsafe { f(conn, cf(&a)) };
            true
        }
        fn destroy(&mut self, conn: Conn, space: SpaceId) -> bool {
            let Some(f) = self.destroy else { return false };
            unsafe { f(conn, space) };
            true
        }
    }
}

#[cfg(target_os = "macos")]
mod live {
    use super::{real::Real, Overlay};
    use std::cell::RefCell;
    use std::io::Write;

    thread_local! {
        static OVERLAY: RefCell<Option<Overlay<Real>>> = const { RefCell::new(None) };
    }

    fn with(f: impl FnOnce(&mut Overlay<Real>)) {
        let lines = OVERLAY.with(|o| {
            let Ok(mut o) = o.try_borrow_mut() else { return Vec::new() };
            let o = o.get_or_insert_with(|| Overlay::new(Real::load()));
            f(o);
            o.take_lines()
        });
        for line in lines {
            // stderr is heimdall.log in the installed app; one write per line.
            let _ = std::io::stderr().lock().write_all(format!("{line}\n").as_bytes());
        }
    }

    /// A live island window was ordered in: it joins the overlay space (call after `orderFrontRegardless`). Never for
    /// offscreen checks and renders.
    pub(crate) fn join(window: &objc2_app_kit::NSWindow, title: &str) {
        let number = window.windowNumber() as i64;
        with(|o| o.join(number, title));
    }

    pub(crate) fn screens_changed() {
        with(|o| o.screens_changed());
    }

    pub(crate) fn release() {
        with(|o| o.release());
    }

    /// Display changes and quit, for the app's lifetime (event-driven; nothing runs between them).
    pub(crate) fn watch() {
        let center = objc2_foundation::NSNotificationCenter::defaultCenter();
        let screens = block2::RcBlock::new(|_: std::ptr::NonNull<objc2_foundation::NSNotification>| {
            dispatch2::DispatchQueue::main().exec_async(screens_changed);
        });
        let quit = block2::RcBlock::new(|_: std::ptr::NonNull<objc2_foundation::NSNotification>| release());
        unsafe {
            let a = center.addObserverForName_object_queue_usingBlock(
                Some(objc2_app_kit::NSApplicationDidChangeScreenParametersNotification),
                None,
                None,
                &screens,
            );
            let b = center.addObserverForName_object_queue_usingBlock(
                Some(objc2_app_kit::NSApplicationWillTerminateNotification),
                None,
                None,
                &quit,
            );
            std::mem::forget((a, b));
        }
    }
}

#[cfg(target_os = "macos")]
pub(crate) use live::{join, release, watch};

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake SkyLight: records every call, and fails where told.
    #[derive(Default)]
    struct Fake {
        missing: Option<&'static str>,
        conn: Conn,
        /// Spaces `create` hands out in turn (0: fails); then 0.
        spaces: Vec<SpaceId>,
        /// Whether a window's spaces can be listed, and which space ids "lose" their windows (WindowServer reset).
        can_list: bool,
        lost: Vec<SpaceId>,
        no_cleanup: bool,
        calls: Vec<String>,
        /// Membership as SkyLight holds it.
        held: Vec<(WindowId, SpaceId)>,
        /// Windows ordered out (the island panel at rest): in no space while out.
        out: Vec<WindowId>,
    }

    fn fake() -> Fake {
        Fake { conn: 7, spaces: vec![501, 502, 503], can_list: true, ..Fake::default() }
    }

    impl SkyLight for Fake {
        fn missing(&self) -> Option<&'static str> {
            self.missing
        }
        fn connection(&mut self) -> Conn {
            self.calls.push("connection".into());
            self.conn
        }
        fn create(&mut self, _conn: Conn) -> SpaceId {
            let s = if self.spaces.is_empty() { 0 } else { self.spaces.remove(0) };
            self.calls.push(format!("create {s}"));
            s
        }
        fn set_level(&mut self, _conn: Conn, space: SpaceId, level: i32) {
            self.calls.push(format!("level {space} {level}"));
        }
        fn show(&mut self, _conn: Conn, space: SpaceId) {
            self.calls.push(format!("show {space}"));
        }
        fn add(&mut self, _conn: Conn, windows: &[WindowId], space: SpaceId) {
            self.calls.push(format!("add {windows:?} {space}"));
            if !self.lost.contains(&space) {
                for &w in windows {
                    if !self.held.contains(&(w, space)) {
                        self.held.push((w, space));
                    }
                }
            }
        }
        fn spaces_of(&mut self, _conn: Conn, window: WindowId) -> Option<Vec<SpaceId>> {
            self.calls.push(format!("spaces {window}"));
            let out = self.out.contains(&window);
            self.can_list.then(|| self.held.iter().filter(|(w, _)| *w == window && !out).map(|(_, s)| *s).collect())
        }
        fn remove(&mut self, _conn: Conn, windows: &[WindowId], space: SpaceId) -> bool {
            self.calls.push(format!("remove {windows:?} {space}"));
            self.held.retain(|(w, s)| !(windows.contains(w) && *s == space));
            !self.no_cleanup
        }
        fn hide(&mut self, _conn: Conn, space: SpaceId) -> bool {
            self.calls.push(format!("hide {space}"));
            !self.no_cleanup
        }
        fn destroy(&mut self, _conn: Conn, space: SpaceId) -> bool {
            self.calls.push(format!("destroy {space}"));
            self.held.retain(|(_, s)| *s != space);
            !self.no_cleanup
        }
    }

    fn calls(o: &Overlay<Fake>) -> Vec<&str> {
        o.sky.calls.iter().map(String::as_str).collect()
    }

    #[test]
    fn the_space_is_made_created_levelled_shown_and_then_joined_in_floating_islands_order() {
        let mut o = Overlay::new(fake());
        o.join(41, "Heimdall Island");
        assert_eq!(calls(&o), ["connection", "create 501", "level 501 100", "show 501", "add [41] 501", "spaces 41"]);
        assert_eq!(o.space(), Some(501));
        assert_eq!(o.take_lines(), ["island: overlay space 501 (level 100) joined by Heimdall Island (verified)"]);
    }

    #[test]
    fn every_order_in_joins_again_but_logs_and_checks_once_per_window() {
        let mut o = Overlay::new(fake());
        o.join(41, "Heimdall Island");
        o.join(42, "Heimdall Island Control");
        o.take_lines();
        o.sky.calls.clear();
        // Ordered out and in again (ordering out may drop a window from the space): joins, no new check, no new line.
        o.join(41, "Heimdall Island");
        o.join(41, "Heimdall Island");
        assert_eq!(calls(&o), ["add [41] 501", "add [41] 501"]);
        assert!(o.take_lines().is_empty());
        // The space is made once.
        assert_eq!(o.sky.held, [(41, 501), (42, 501)]);
    }

    #[test]
    fn a_window_without_a_number_never_reaches_skylight() {
        let mut o = Overlay::new(fake());
        o.join(0, "Heimdall Island");
        o.join(-1, "Heimdall Island");
        assert!(calls(&o).is_empty());
        assert!(o.take_lines().is_empty());
    }

    #[test]
    fn each_missing_required_symbol_falls_back_with_one_line_and_no_calls() {
        for symbol in ["_CGSDefaultConnection", "CGSSpaceCreate", "CGSSpaceSetAbsoluteLevel", "CGSShowSpaces", "CGSAddWindowsToSpaces"] {
            let mut o = Overlay::new(Fake { missing: Some(symbol), ..fake() });
            o.join(41, "Heimdall Island");
            o.join(42, "Heimdall Island Control");
            o.screens_changed();
            assert!(calls(&o).is_empty(), "{symbol}: {:?}", calls(&o));
            assert!(o.is_off());
            assert_eq!(
                o.take_lines(),
                [format!("island: overlay space unavailable (SkyLight has no {symbol}); island windows keep canJoinAllSpaces at their levels and slide with Space swipes")]
            );
        }
    }

    #[test]
    fn no_connection_falls_back_before_any_space_is_made() {
        let mut o = Overlay::new(Fake { conn: 0, ..fake() });
        o.join(41, "Heimdall Island");
        o.join(41, "Heimdall Island");
        assert_eq!(calls(&o), ["connection"]);
        assert_eq!(o.take_lines().len(), 1);
        assert!(o.is_off());
    }

    #[test]
    fn a_failed_create_falls_back_without_levelling_showing_or_adding() {
        let mut o = Overlay::new(Fake { spaces: vec![0], ..fake() });
        o.join(41, "Heimdall Island");
        o.join(41, "Heimdall Island");
        assert_eq!(calls(&o), ["connection", "create 0"]);
        assert_eq!(
            o.take_lines(),
            ["island: overlay space unavailable (CGSSpaceCreate made no space); island windows keep canJoinAllSpaces at their levels and slide with Space swipes"]
        );
    }

    #[test]
    fn without_a_way_to_list_spaces_the_join_is_kept_and_logged_unverified() {
        let mut o = Overlay::new(Fake { can_list: false, ..fake() });
        o.join(41, "Heimdall Island");
        assert!(!o.is_off());
        assert_eq!(o.take_lines(), ["island: overlay space 501 (level 100) joined by Heimdall Island (unverified: SkyLight cannot list a window's spaces)"]);
    }

    #[test]
    fn a_lost_space_is_made_again_once_with_every_window() {
        let mut o = Overlay::new(Fake { lost: vec![501], ..fake() });
        o.join(41, "Heimdall Island");
        assert_eq!(
            calls(&o),
            ["connection", "create 501", "level 501 100", "show 501", "add [41] 501", "spaces 41", "destroy 501", "create 502", "level 502 100", "show 502", "add [41] 502", "spaces 41"]
        );
        assert_eq!(o.space(), Some(502));
        assert_eq!(
            o.take_lines(),
            ["island: overlay space 501 was lost; made space 502 and re-joined 1 window(s)", "island: overlay space 502 (level 100) joined by Heimdall Island (verified after making the space again)"]
        );
    }

    #[test]
    fn a_window_that_never_joins_turns_the_overlay_off_and_cleans_up() {
        let mut o = Overlay::new(Fake { lost: vec![501, 502], ..fake() });
        o.join(41, "Heimdall Island");
        assert!(o.is_off());
        let c = calls(&o);
        assert_eq!(&c[c.len() - 3..], ["remove [41] 502", "hide 502", "destroy 502"]);
        let lines = o.take_lines();
        assert_eq!(lines.last().unwrap(), "island: overlay space unavailable (Heimdall Island is not in space 502 after joining it); island windows keep canJoinAllSpaces at their levels and slide with Space swipes");
        // Off for good: later windows do not reach SkyLight.
        o.sky.calls.clear();
        o.join(42, "Heimdall Island Control");
        assert!(calls(&o).is_empty());
    }

    #[test]
    fn a_display_change_shows_the_space_again_and_rejoins_every_window() {
        let mut o = Overlay::new(fake());
        o.join(41, "Heimdall Island");
        o.join(42, "Heimdall Island Control");
        o.join(43, "Heimdall Drawing");
        o.take_lines();
        o.sky.calls.clear();
        o.screens_changed();
        assert_eq!(calls(&o), ["show 501", "add [41, 42, 43] 501"]);
        assert_eq!(o.take_lines(), ["island: overlay space 501 shown again after a display change; 3 window(s) re-joined (checked at their next order-in)"]);
        // Each window's next order-in checks it in the space again.
        o.sky.calls.clear();
        o.join(41, "Heimdall Island");
        assert_eq!(calls(&o), ["add [41] 501", "spaces 41"]);
        assert_eq!(o.take_lines(), ["island: overlay space 501 (level 100) joined by Heimdall Island (verified)"]);
    }

    /// Night 3 review: the island panel (the first member) is ordered out at rest, and an ordered-out window is in no
    /// space. A display change must not read that as a lost space: the old check remade the space, found the panel
    /// out again and turned the overlay off for the rest of the process.
    #[test]
    fn a_display_change_while_the_panel_rests_ordered_out_keeps_the_overlay() {
        let mut o = Overlay::new(fake());
        o.join(41, "Heimdall Island");
        o.join(42, "Heimdall Island Control");
        o.take_lines();
        o.sky.out.push(41);
        for _ in 0..2 {
            o.screens_changed();
            assert!(!o.is_off(), "{:?}", o.take_lines());
            assert_eq!(o.space(), Some(501));
        }
        assert!(!calls(&o).iter().any(|c| c.starts_with("destroy") || c.starts_with("remove")), "{:?}", calls(&o));
        // The panel is ordered in: it joins the same space, checked there.
        o.sky.out.clear();
        o.take_lines();
        o.join(41, "Heimdall Island");
        assert_eq!(o.space(), Some(501));
        assert_eq!(o.take_lines(), ["island: overlay space 501 (level 100) joined by Heimdall Island (verified)"]);
    }

    #[test]
    fn a_display_change_that_lost_the_space_makes_it_again_at_the_next_order_in() {
        let mut o = Overlay::new(fake());
        o.join(41, "Heimdall Island");
        o.join(42, "Heimdall Island Control");
        o.take_lines();
        // WindowServer dropped the space with the display change.
        o.sky.held.clear();
        o.sky.lost.push(501);
        o.sky.calls.clear();
        o.screens_changed();
        assert_eq!(calls(&o), ["show 501", "add [41, 42] 501"]);
        o.take_lines();
        // The panel's next order-in finds it out of the space: the space is made again with every window.
        o.sky.calls.clear();
        o.join(41, "Heimdall Island");
        assert_eq!(
            calls(&o),
            ["add [41] 501", "spaces 41", "destroy 501", "create 502", "level 502 100", "show 502", "add [41, 42] 502", "spaces 41"]
        );
        assert_eq!(o.space(), Some(502));
        assert_eq!(o.sky.held, [(41, 502), (42, 502)]);
        assert_eq!(
            o.take_lines(),
            ["island: overlay space 501 was lost; made space 502 and re-joined 2 window(s)", "island: overlay space 502 (level 100) joined by Heimdall Island (verified after making the space again)"]
        );
        // Another window's next order-in is checked and logged once in the new space.
        o.sky.calls.clear();
        o.join(42, "Heimdall Island Control");
        assert_eq!(calls(&o), ["add [42] 502", "spaces 42"]);
        assert_eq!(o.take_lines(), ["island: overlay space 502 (level 100) joined by Heimdall Island Control (verified)"]);
    }

    #[test]
    fn a_recreated_window_joins_the_space_it_finds() {
        // A window made again (new window number, same title) joins as a new member.
        let mut o = Overlay::new(fake());
        o.join(41, "Heimdall Drawing");
        o.take_lines();
        o.sky.calls.clear();
        o.join(77, "Heimdall Drawing");
        assert_eq!(calls(&o), ["add [77] 501", "spaces 77"]);
        assert_eq!(o.sky.held, [(41, 501), (77, 501)]);
        assert_eq!(o.take_lines(), ["island: overlay space 501 (level 100) joined by Heimdall Drawing (verified)"]);
    }

    #[test]
    fn a_display_change_before_any_window_or_after_fallback_does_nothing() {
        let mut o = Overlay::new(fake());
        o.screens_changed();
        assert!(calls(&o).is_empty());
        let mut off = Overlay::new(Fake { missing: Some("CGSShowSpaces"), ..fake() });
        off.join(41, "Heimdall Island");
        off.take_lines();
        off.screens_changed();
        assert!(calls(&off).is_empty() && off.take_lines().is_empty());
    }

    #[test]
    fn quit_removes_every_window_hides_and_destroys_the_space_and_nothing_joins_after() {
        let mut o = Overlay::new(fake());
        o.join(41, "Heimdall Island");
        o.join(42, "Heimdall Island Control");
        o.take_lines();
        o.sky.calls.clear();
        o.release();
        assert_eq!(calls(&o), ["remove [41, 42] 501", "hide 501", "destroy 501"]);
        assert_eq!(o.take_lines(), ["island: overlay space 501 released"]);
        o.sky.calls.clear();
        o.join(41, "Heimdall Island");
        o.screens_changed();
        o.release();
        assert!(calls(&o).is_empty());
        assert!(o.take_lines().is_empty());
    }

    #[test]
    fn quit_without_clean_up_symbols_says_so_and_quit_before_any_space_touches_nothing() {
        let mut o = Overlay::new(Fake { no_cleanup: true, ..fake() });
        o.join(41, "Heimdall Island");
        o.take_lines();
        o.release();
        assert_eq!(o.take_lines(), ["island: overlay space 501 left to WindowServer at exit (SkyLight lacks its clean-up calls)"]);
        let mut never = Overlay::new(fake());
        never.release();
        assert!(calls(&never).is_empty() && never.take_lines().is_empty());
    }
}
