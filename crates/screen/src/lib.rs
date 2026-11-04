//! Durable screen recording and deterministic commands. No API requests TCC permission.
//! Times are capture/listen's mach_absolute_time nanoseconds, never UNIX time.
mod buffer;
mod gate;
pub use gate::CaptureGate;
mod commands;
mod fullres;
mod gaps;
mod hold;
#[cfg(target_os = "macos")]
mod macos;
pub use buffer::*;
pub use commands::*;
pub use fullres::*;
pub use hold::*;
#[cfg(target_os = "macos")]
pub use macos::{grab_still, screen_permission_granted, ScreenCapture, ScreenEvent};

/// A still of one display (drawing mode holds it under its overlay while a menu is open): packed BGRA8, opaque, in
/// Display P3, without the pointer.
pub struct Still {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

/// This process's windows with this title never appear in a capture: Heimdall's command island (`ui` sets the
/// title; crates/app tests that both literals match). Recorded frames and full-resolution grabs both exclude it.
pub const EXCLUDED_WINDOW_TITLE: &str = "Heimdall Command Island";
/// Every window of this process a capture leaves out: the island, drawing mode's overlay (its light; the user's strokes are
/// composited into the image instead) and Heimdall's pointing. Each exists from UI launch (ordered out), so filters
/// built when recording starts already find it. crates/app tests that these equal the titles `ui` sets.
pub const EXCLUDED_WINDOW_TITLES: &[&str] = &[EXCLUDED_WINDOW_TITLE, "Heimdall Draw Overlay", "Heimdall Pointer", "Heimdall Drop Target"];

/// Whether a shareable window is one of ours to exclude.
pub fn is_excluded_window(owner_pid: Option<i32>, own_pid: i32, title: Option<&str>) -> bool {
    owner_pid == Some(own_pid) && title.is_some_and(|t| EXCLUDED_WINDOW_TITLES.contains(&t))
}

#[cfg(test)]
mod excluded_window_tests {
    use super::*;
    #[test]
    fn only_this_process_island_window_is_excluded() {
        assert!(is_excluded_window(Some(7), 7, Some(EXCLUDED_WINDOW_TITLE)));
        assert!(!is_excluded_window(Some(8), 7, Some(EXCLUDED_WINDOW_TITLE)), "another app's window with the title");
        assert!(!is_excluded_window(Some(7), 7, Some("Transcript")), "Heimdall's other windows stay capturable");
        assert!(is_excluded_window(Some(7), 7, Some("Heimdall Draw Overlay")), "drawing mode's overlay never shows in a capture");
        assert!(is_excluded_window(Some(7), 7, Some("Heimdall Pointer")), "Heimdall's pointing never shows in a capture");
        assert!(!is_excluded_window(None, 7, None));
    }
}
