//! Drawing over an open menu ("I would like the Heimdall menu to stay open while I'm drawing. Same
//! goes for any other app in the Dock or in the top bar"): the overlay holds a still of the screen with the menu on
//! it, the user draws on that still, and Send marks that still.
//!
//! A live menu cannot stay open under drawing mode. Heimdall's own menu runs its tracking loop on Heimdall's main
//! thread, which is the overlay's thread, and choosing Drawing Mode in it closes it. Another app's menu survives an
//! overlay ordered above it, but closes the moment the overlay takes the keyboard (measured 24 Oct with a probe app's
//! status menu: still open after `orderFrontRegardless`, closed by `makeKeyWindow`), and Esc and Return need the
//! keyboard. So:
//!
//! - Heimdall's menu: a still is taken shortly after it (or its Shortcuts submenu) opens, held while it is open, and
//!   used when Drawing Mode is chosen in it (a click, or ⌃⌘M as the item's key equivalent). AppKit flashes the item
//!   and fades the menu out (about 0.4 s, measured) before the action runs, so the menu blinks out and comes back as
//!   the still.
//! - Another app's menu (a menu-bar, status, Dock or context menu, or another surface over the menu bar): when one is
//!   on the display as drawing mode opens, the overlay goes up above it without the keyboard, takes the still, puts
//!   it under the ink, and only then takes the keyboard, which closes the real menu under the still.
//! - No menu: drawing mode opens live, as before.
//!
//! The still is the display at native resolution in Display P3 without the pointer and without Heimdall's own
//! windows (its menus are in it). It is held in memory only and dropped when drawing mode closes, or soon after
//! Heimdall's menu closes without drawing. Seeing off takes no still (the app's still grabber refuses).

use std::cell::RefCell;
use std::time::{Duration, Instant};

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_core_foundation::{CFArray, CFRetained, CGRect};
use objc2_core_graphics::{
    kCGColorSpaceDisplayP3, CGBitmapInfo, CGColorRenderingIntent, CGColorSpace, CGDataProvider, CGDisplayBounds, CGImage, CGImageAlphaInfo,
    CGImageByteOrderInfo, CGWindowListCopyWindowInfo, CGWindowListOption,
};
use objc2_foundation::{MainThreadMarker, NSArray, NSDictionary, NSNumber, NSString};

/// How long after Heimdall's menu (or a submenu) opens its still is taken: a menu is on screen at full opacity within
/// 40 ms of `menuWillOpen` (measured 24 Oct, no fade-in), and the pointer is still on the item that opened it.
pub(crate) const MENU_SETTLE: Duration = Duration::from_millis(80);
/// How long a still of Heimdall's menu is kept after the menu closes (the item's action runs right after the close).
pub(crate) const MENU_KEEP: Duration = Duration::from_secs(2);

/// A still, ready to go under the ink: the display it shows and the image (Display P3, native pixels).
#[derive(Clone)]
pub(crate) struct Still {
    pub display: u32,
    pub image: CFRetained<CGImage>,
}

/// CGImage is immutable and thread-safe; the still is made off the main thread and handed to it.
struct Handoff(Option<Still>);
unsafe impl Send for Handoff {}

/// What the overlay does with the screen as it opens.
#[derive(Clone)]
pub(crate) enum Hold {
    /// Draw live (no menu was open).
    Live,
    /// Hold this still now.
    Ready(Still),
    /// Heimdall's menu's still is still being taken: wait for it (`overlay::deliver`).
    Wait,
    /// Another app's menu is open: take a still now and hold it (`overlay::deliver`).
    Grab,
}

#[derive(Default)]
struct Menu {
    /// Bumped at every open of the menu or a submenu; a still belongs to the open that asked for it.
    generation: u64,
    open: bool,
    closed_at: Option<Instant>,
    /// Drawing Mode was chosen in the menu: the next open holds the menu's still.
    chosen: bool,
    still: Option<Still>,
    pending: bool,
    /// The overlay waits for the pending still under this token.
    waiting: Option<u64>,
}

thread_local! {
    static MENU: RefCell<Menu> = RefCell::new(Menu::default());
}

/// Heimdall's menu (`main`) or one of its submenus opened (main thread): a still, once the menu is drawn, of the
/// display it is on (found by its window then; the pointer's display until then). The main menu's opening forgets the
/// last open's still and choice.
pub(crate) fn menu_opened(main: bool, mtm: MainThreadMarker) {
    let Some(display) = crate::mac::util::pointer_screen(mtm).map(|s| s.CGDirectDisplayID()) else { return };
    if main {
        MENU.with(|m| {
            let mut m = m.borrow_mut();
            m.still = None;
            m.chosen = false;
        });
    }
    menu_opened_on(display, MENU_SETTLE);
}

pub(crate) fn menu_opened_on(display: u32, settle: Duration) {
    let Some(grab) = crate::drawing::still_grabber() else { return };
    if !crate::drawing::seeing() {
        return;
    }
    let generation = MENU.with(|m| {
        let mut m = m.borrow_mut();
        m.generation += 1;
        m.open = true;
        m.closed_at = None;
        m.pending = true;
        m.generation
    });
    std::thread::spawn(move || {
        std::thread::sleep(settle);
        let display = own_menu_display().unwrap_or(display);
        let requested = Instant::now();
        let still = grab(display).and_then(|(w, h, bgra)| image_p3(w, h, bgra)).map(|image| Still { display, image });
        if let Err(e) = &still {
            // Once per process for the same reason: a missing permission refused every menu open into the log.
            static LAST: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());
            let line = format!("{e}");
            let mut last = LAST.lock().unwrap();
            if *last != line {
                eprintln!("Drawing mode: no still of Heimdall's menu: {line}");
                *last = line;
            }
        }
        let handoff = Handoff(still.ok());
        dispatch2::DispatchQueue::main().exec_async(move || {
            let handoff = handoff;
            menu_still_arrived(generation, requested, handoff.0);
        });
    });
}

fn menu_still_arrived(generation: u64, requested: Instant, still: Option<Still>) {
    let waiting = MENU.with(|m| {
        let mut m = m.borrow_mut();
        if m.generation != generation {
            return None;
        }
        m.pending = false;
        // Asked for after the menu closed (Drawing Mode chosen within the settle time): the menu is not in it. That,
        // or a failed still, keeps the last good one (the main menu's, under a submenu).
        let late = m.closed_at.is_some_and(|closed| requested >= closed);
        if let Some(still) = still.filter(|_| !late) {
            m.still = Some(still);
        }
        m.waiting.take().map(|token| (token, m.still.clone()))
    });
    if let Some((token, still)) = waiting {
        super::overlay::deliver(token, still);
    }
}

/// Heimdall's menu closed (main thread). Its still is dropped soon unless drawing mode takes it.
pub(crate) fn menu_closed() {
    let generation = MENU.with(|m| {
        let mut m = m.borrow_mut();
        m.open = false;
        m.closed_at = Some(Instant::now());
        m.generation
    });
    let after = dispatch2::DispatchTime::try_from(MENU_KEEP + Duration::from_millis(100)).unwrap_or(dispatch2::DispatchTime::NOW);
    let _ = dispatch2::DispatchQueue::main().after(after, move || {
        MENU.with(|m| {
            let mut m = m.borrow_mut();
            if m.generation == generation && !m.open && m.waiting.is_none() {
                m.still = None;
                m.chosen = false;
            }
        });
    });
}

/// Drawing Mode was chosen in Heimdall's menu (main thread, just before the overlay opens).
pub(crate) fn chosen_in_menu() {
    MENU.with(|m| m.borrow_mut().chosen = true);
}

/// The choice was not used (drawing mode closed instead of opening, or could not open).
pub(crate) fn forget_choice() {
    MENU.with(|m| m.borrow_mut().chosen = false);
}

/// The overlay opens on `display`: what it holds. `screen` says whether to look for other apps' menus on the
/// display (off in checks, where nothing is on a display).
pub(crate) fn hold_for(display: u32, screen: bool) -> Hold {
    // Seeing off: no still is held, an old one included (Send would refuse a capture then too).
    if !crate::drawing::seeing() {
        forget_choice();
        return Hold::Live;
    }
    let menu = MENU.with(|m| {
        let mut m = m.borrow_mut();
        let chosen = std::mem::take(&mut m.chosen);
        let recent = m.open || m.closed_at.is_some_and(|t| t.elapsed() <= MENU_KEEP);
        if !chosen || !recent {
            return None;
        }
        if m.pending {
            return Some(Hold::Wait);
        }
        m.still.clone().filter(|s| s.display == display).map(Hold::Ready)
    });
    if let Some(hold) = menu {
        return hold;
    }
    if screen && crate::drawing::still_grabber().is_some() && menu_on_display(display) {
        return Hold::Grab;
    }
    Hold::Live
}

/// The overlay waits for Heimdall's menu's pending still under `token`.
pub(crate) fn wait(token: u64) {
    MENU.with(|m| m.borrow_mut().waiting = Some(token));
}

/// The overlay closed: nothing waits and the menu's still goes.
pub(crate) fn release() {
    MENU.with(|m| {
        let mut m = m.borrow_mut();
        m.waiting = None;
        if !m.open {
            m.still = None;
        }
    });
}

/// A still of `display` now, off the main thread, delivered to the overlay under `token`.
pub(crate) fn grab(display: u32, token: u64) {
    let Some(grab) = crate::drawing::still_grabber() else {
        super::overlay::deliver(token, None);
        return;
    };
    std::thread::spawn(move || {
        let still = grab(display).and_then(|(w, h, bgra)| image_p3(w, h, bgra)).map(|image| Still { display, image });
        if let Err(e) = &still {
            eprintln!("Drawing mode: no still of the open menu: {e}");
        }
        let handoff = Handoff(still.ok());
        dispatch2::DispatchQueue::main().exec_async(move || {
            let handoff = handoff;
            super::overlay::deliver(token, handoff.0);
        });
    });
}

/// Packed BGRA8 (Display P3, opaque) as an image over the same bytes (no copy of the ~31 MB still); the image frees
/// them when it goes.
pub(crate) fn image_p3(w: u32, h: u32, bgra: Vec<u8>) -> Result<CFRetained<CGImage>, String> {
    let (wu, hu) = (w as usize, h as usize);
    if wu == 0 || hu == 0 || bgra.len() != wu * hu * 4 {
        return Err(format!("bad still: {w}x{h} with {} bytes", bgra.len()));
    }
    let space = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceDisplayP3 })).ok_or("no Display P3 colour space")?;
    unsafe extern "C-unwind" fn release(info: *mut std::ffi::c_void, _data: std::ptr::NonNull<std::ffi::c_void>, _size: usize) {
        // SAFETY: `info` is the box made below, released once by CoreGraphics.
        drop(unsafe { Box::from_raw(info.cast::<Vec<u8>>()) });
    }
    let bytes = Box::new(bgra);
    let (data, len) = (bytes.as_ptr(), bytes.len());
    let info = Box::into_raw(bytes);
    // SAFETY: the bytes live in the box until `release`. Should the provider not be made (it never is in practice),
    // the box is leaked rather than freed, as CoreGraphics' ownership is then unknown.
    let provider = unsafe { CGDataProvider::with_data(info.cast(), data.cast(), len, Some(release)) }.ok_or("no data provider")?;
    let bitmap = CGBitmapInfo(CGImageAlphaInfo::NoneSkipFirst.0 | CGImageByteOrderInfo::Order32Little.0);
    // SAFETY: the provider holds exactly w × h × 4 bytes in that layout.
    unsafe { CGImage::new(wu, hu, 8, 32, wu * 4, Some(&space), bitmap, Some(&provider), std::ptr::null(), false, CGColorRenderingIntent::RenderingIntentDefault) }
        .ok_or_else(|| "no image".into())
}

/// A window on screen as the window list reports it (bounds in global display coordinates, y down).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Listed {
    pub pid: i32,
    pub layer: i64,
    pub alpha: f64,
    pub bounds: (f64, f64, f64, f64),
}

/// kCGPopUpMenuWindowLevel: every menu measured on 24 Oct sits there (another app's context and status menus, the
/// Dock's menus, owned by DockHelper). Only menus count: windows that stay up above the menu bar (Floating Islands
/// at 25, Heimdall's island at 27) would otherwise freeze every drawing.
const POP_UP_MENU_LEVEL: i64 = 101;

/// Whether `w` is another app's open menu (a menu-bar, status, Dock or context menu) on `display` (global, y down).
pub(crate) fn is_menu(w: &Listed, own_pid: i32, display: (f64, f64, f64, f64)) -> bool {
    let (x, y, width, height) = w.bounds;
    let (dx, dy, dw, dh) = display;
    let overlaps = x < dx + dw && x + width > dx && y < dy + dh && y + height > dy;
    w.pid != own_pid && w.layer == POP_UP_MENU_LEVEL && w.alpha > 0.0 && width >= 8.0 && height >= 8.0 && overlaps
}

/// The display Heimdall's own open menu is on (its pop-up window's top left), if one is on screen.
fn own_menu_display() -> Option<u32> {
    let own = std::process::id() as i32;
    let menu = listed().into_iter().find(|w| w.pid == own && w.layer == POP_UP_MENU_LEVEL)?;
    let point = objc2_core_foundation::CGPoint::new(menu.bounds.0 + 1.0, menu.bounds.1 + 1.0);
    let (mut id, mut count) = (0u32, 0u32);
    // SAFETY: room for one display id and the count.
    let err = unsafe { objc2_core_graphics::CGGetDisplaysWithPoint(point, 1, &mut id, &mut count) };
    (err == objc2_core_graphics::CGError::Success && count == 1).then_some(id)
}

/// Whether another app has a menu open on `display` now.
pub(crate) fn menu_on_display(display: u32) -> bool {
    let b: CGRect = CGDisplayBounds(display);
    let bounds = (b.origin.x, b.origin.y, b.size.width, b.size.height);
    let own = std::process::id() as i32;
    listed().iter().any(|w| is_menu(w, own, bounds))
}

/// The on-screen windows (no permission needed for layers and bounds).
fn listed() -> Vec<Listed> {
    let Some(list) = CGWindowListCopyWindowInfo(CGWindowListOption::OptionOnScreenOnly | CGWindowListOption::ExcludeDesktopElements, 0) else {
        return vec![];
    };
    // Toll-free bridged: a CFArray of CFDictionaries is an NSArray of NSDictionaries.
    let list: &CFArray = &list;
    let list = unsafe { &*(list as *const CFArray as *const NSArray<NSDictionary<NSString, AnyObject>>) };
    let number = |d: &NSDictionary<NSString, AnyObject>, key: &str| -> Option<f64> {
        let v: Retained<AnyObject> = d.objectForKey(&NSString::from_str(key))?;
        v.downcast_ref::<NSNumber>().map(|n| n.doubleValue())
    };
    list.iter()
        .filter_map(|d| {
            let bounds = d.objectForKey(&NSString::from_str("kCGWindowBounds"))?;
            let bounds = bounds.downcast_ref::<NSDictionary>()?;
            let bounds = unsafe { &*(bounds as *const NSDictionary as *const NSDictionary<NSString, AnyObject>) };
            Some(Listed {
                pid: number(&d, "kCGWindowOwnerPID")? as i32,
                layer: number(&d, "kCGWindowLayer")? as i64,
                alpha: number(&d, "kCGWindowAlpha").unwrap_or(1.0),
                bounds: (number(bounds, "X")?, number(bounds, "Y")?, number(bounds, "Width")?, number(bounds, "Height")?),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const DISPLAY: (f64, f64, f64, f64) = (0.0, 0.0, 1728.0, 1117.0);
    const OWN: i32 = 42;

    fn w(pid: i32, layer: i64, bounds: (f64, f64, f64, f64)) -> Listed {
        Listed { pid, layer, alpha: 1.0, bounds }
    }

    #[test]
    fn another_apps_menus_count_and_the_menu_bar_pointer_and_own_windows_do_not() {
        // The user's screen on 24 Oct, nothing open: the pointer, the menu bar, Heimdall's island and a Floating Islands
        // window that stays up at 25.
        let rest = [
            w(300, 2147483630, (869.0, 693.0, 32.0, 32.0)),
            w(300, 24, (0.0, 0.0, 1728.0, 33.0)),
            w(OWN, 27, (474.0, 0.0, 780.0, 639.0)),
            w(28022, 25, (8.0, 276.0, 132.0, 36.0)),
        ];
        assert!(rest.iter().all(|x| !is_menu(x, OWN, DISPLAY)));
        // The probe app's context menu and status menu, as measured (layer 101).
        assert!(is_menu(&w(7, 101, (1300.0, 812.0, 107.0, 106.0)), OWN, DISPLAY));
        assert!(is_menu(&w(7, 101, (1087.0, 34.0, 107.0, 106.0)), OWN, DISPLAY));
        // Tooltips and other windows above the menu bar are not menus.
        assert!(!is_menu(&w(8, 200, (1400.0, 34.0, 120.0, 20.0)), OWN, DISPLAY));
        // Heimdall's own menu is handled by its still, not here.
        assert!(!is_menu(&w(OWN, 101, (1087.0, 34.0, 300.0, 600.0)), OWN, DISPLAY));
        // Floating windows, screen savers, invisible and tiny windows, and other displays do not count.
        assert!(!is_menu(&w(9, 3, (10.0, 10.0, 400.0, 400.0)), OWN, DISPLAY));
        assert!(!is_menu(&w(9, 1000, (0.0, 0.0, 1728.0, 1117.0)), OWN, DISPLAY));
        assert!(!is_menu(&Listed { alpha: 0.0, ..w(9, 101, (10.0, 10.0, 200.0, 200.0)) }, OWN, DISPLAY));
        assert!(!is_menu(&w(9, 101, (10.0, 10.0, 1.0, 1.0)), OWN, DISPLAY));
        assert!(!is_menu(&w(9, 101, (1728.0, 0.0, 200.0, 200.0)), OWN, DISPLAY));
        assert!(is_menu(&w(9, 101, (1700.0, 0.0, 200.0, 200.0)), OWN, DISPLAY));
    }

    #[test]
    fn heimdalls_menu_still_is_held_only_when_drawing_was_chosen_in_the_open_menu() {
        let still = Still { display: 1, image: image_p3(2, 2, vec![0; 16]).unwrap() };
        let set = |open: bool, closed: Option<Instant>, chosen: bool, pending: bool| {
            MENU.with(|m| *m.borrow_mut() = Menu { generation: 1, open, closed_at: closed, chosen, still: Some(still.clone()), pending, waiting: None })
        };
        let kind = |h: Hold| match h {
            Hold::Live => "live",
            Hold::Ready(_) => "ready",
            Hold::Wait => "wait",
            Hold::Grab => "grab",
        };
        // Chosen in the menu, which just closed: its still.
        set(false, Some(Instant::now()), true, false);
        assert_eq!(kind(hold_for(1, false)), "ready");
        // Chosen is used once.
        assert_eq!(kind(hold_for(1, false)), "live");
        // ⌃⌘M from another app right after the menu closed: live, not the old menu.
        set(false, Some(Instant::now()), false, false);
        assert_eq!(kind(hold_for(1, false)), "live");
        // Chosen long after the close (a stale flag): live.
        set(false, Some(Instant::now() - MENU_KEEP - Duration::from_millis(10)), true, false);
        assert_eq!(kind(hold_for(1, false)), "live");
        // The still is still being taken: wait for it.
        set(true, None, true, true);
        assert_eq!(kind(hold_for(1, false)), "wait");
        // Drawing on another display than the menu's still: live.
        set(false, Some(Instant::now()), true, false);
        assert_eq!(kind(hold_for(2, false)), "live");
        MENU.with(|m| *m.borrow_mut() = Menu::default());
    }

    #[test]
    fn a_still_asked_for_after_the_menu_closed_is_not_the_menus() {
        let old = Still { display: 1, image: image_p3(2, 2, vec![0; 16]).unwrap() };
        let new = Still { display: 1, image: image_p3(3, 3, vec![0; 36]).unwrap() };
        let closed = Instant::now();
        MENU.with(|m| *m.borrow_mut() = Menu { generation: 3, open: false, closed_at: Some(closed), chosen: false, still: Some(old.clone()), pending: true, waiting: None });
        menu_still_arrived(3, closed + Duration::from_millis(1), Some(new.clone()));
        let kept = MENU.with(|m| m.borrow().still.as_ref().map(|s| CGImage::width(Some(&s.image))));
        assert_eq!(kept, Some(2), "the late still is dropped; the earlier one stays");
        MENU.with(|m| m.borrow_mut().pending = true);
        menu_still_arrived(3, closed - Duration::from_millis(1), Some(new));
        let kept = MENU.with(|m| m.borrow().still.as_ref().map(|s| CGImage::width(Some(&s.image))));
        assert_eq!(kept, Some(3), "one asked for while the menu was open replaces it");
        // A still of an older open is ignored.
        menu_still_arrived(2, closed - Duration::from_millis(1), Some(old));
        assert_eq!(MENU.with(|m| m.borrow().still.as_ref().map(|s| CGImage::width(Some(&s.image)))), Some(3));
        MENU.with(|m| *m.borrow_mut() = Menu::default());
    }

    #[test]
    fn a_still_becomes_a_display_p3_image_of_its_size() {
        let image = image_p3(3, 2, vec![10; 24]).unwrap();
        assert_eq!((CGImage::width(Some(&image)), CGImage::height(Some(&image))), (3, 2));
        // The image reads the bytes it was given: drawn into sRGB, a grey stays grey.
        let rgb = super::super::ink::composite_image(&image, (3.0, 2.0), &[]).unwrap();
        assert!(rgb.iter().all(|v| v.abs_diff(10) <= 2), "{rgb:?}");
        assert!(image_p3(3, 2, vec![10; 23]).is_err());
        assert!(image_p3(0, 2, vec![]).is_err());
    }
}
