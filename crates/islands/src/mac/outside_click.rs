//! Whether a click in another app counts as a click outside an open viewer (bug note 2026-11-04T21-56-52).
//!
//! An open viewer closes on a click outside it (a global mouse-down monitor). Heimdall's drawing mode puts a
//! transparent overlay over the whole display, one level above pop-up menus, and the user draws on it, over the viewer
//! too: those presses are ink, not clicks outside, so while the overlay is up the viewer stays open (and the capture
//! Heimdall sends includes it and what he marked on it). The overlay is found in the window list by its owner and
//! level; window titles of other apps need Screen Recording, which FloatingIslands does not have.

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSString};

/// Heimdall's drawing overlay level: `NSPopUpMenuWindowLevel + 1` (crates/ui `draw::overlay::LEVEL`).
pub const HEIMDALL_DRAW_LEVEL: i64 = 102;

/// One on-screen window, as the window list reports it.
#[derive(Clone, Debug, PartialEq)]
pub struct Win {
    pub owner: String,
    pub layer: i64,
    pub alpha: f64,
}

/// Heimdall's drawing overlay is on screen: a visible window owned by Heimdall at the overlay's level.
pub fn heimdall_drawing(windows: &[Win]) -> bool {
    windows.iter().any(|w| w.layer == HEIMDALL_DRAW_LEVEL && w.alpha > 0.0 && w.owner.starts_with("Heimdall"))
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGWindowListCopyWindowInfo(option: u32, relative_to: u32) -> *mut AnyObject;
}

/// The on-screen windows (front to back) from the window list.
fn on_screen() -> Vec<Win> {
    // kCGWindowListOptionOnScreenOnly | kCGWindowListExcludeDesktopElements.
    let raw = unsafe { CGWindowListCopyWindowInfo(1 | 16, 0) };
    // SAFETY: a +1 CFArray of CFDictionary, toll-free bridged.
    let Some(list) = (unsafe { Retained::<NSArray<NSDictionary<NSString, AnyObject>>>::from_raw(raw.cast()) }) else { return Vec::new() };
    let num = |d: &NSDictionary<NSString, AnyObject>, k: &str| -> Option<f64> { Some(d.objectForKey(&NSString::from_str(k))?.downcast_ref::<NSNumber>()?.doubleValue()) };
    list.iter()
        .map(|d| Win {
            owner: d.objectForKey(&NSString::from_str("kCGWindowOwnerName")).and_then(|o| o.downcast_ref::<NSString>().map(|s| s.to_string())).unwrap_or_default(),
            layer: num(&d, "kCGWindowLayer").unwrap_or(0.0) as i64,
            alpha: num(&d, "kCGWindowAlpha").unwrap_or(1.0),
        })
        .collect()
}

/// A mouse-down in another app closes an open viewer, unless Heimdall's drawing overlay is up.
pub fn closes_viewer() -> bool {
    !heimdall_drawing(&on_screen())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(owner: &str, layer: i64, alpha: f64) -> Win {
        Win { owner: owner.into(), layer, alpha }
    }

    /// Bug note 2026-11-04T21-56-52: drawing over an open viewer closed it; with the overlay up it stays open.
    #[test]
    fn heimdall_drawing_overlay_keeps_the_viewer_open() {
        assert!(heimdall_drawing(&[w("Safari", 0, 1.0), w("Heimdall", HEIMDALL_DRAW_LEVEL, 1.0)]));
        assert!(heimdall_drawing(&[w("Heimdall Lab", HEIMDALL_DRAW_LEVEL, 1.0)]), "a lab build is Heimdall too");
        // Heimdall's notch (other levels), a pop-up menu, another app at that level, a hidden window: not drawing.
        assert!(!heimdall_drawing(&[w("Heimdall", 25, 1.0), w("Heimdall", 101, 1.0)]));
        assert!(!heimdall_drawing(&[w("Raycast", HEIMDALL_DRAW_LEVEL, 1.0)]));
        assert!(!heimdall_drawing(&[w("Heimdall", HEIMDALL_DRAW_LEVEL, 0.0)]));
        assert!(!heimdall_drawing(&[]));
    }
}
