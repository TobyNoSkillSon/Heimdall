//! Drawing on the screen, both ways.
//!
//! The user's drawing mode: ⌃⌘M (or the agent's `drawing_mode` tool) puts a click-catching overlay on the display he is
//! on; he drags to draw, ⌘Z/⌫ undoes, ⌘⌫ clears, Esc or ⌃⌘M closes and discards, Return or the island's Send
//! capsule captures that display without Heimdall's windows, draws the strokes on it and hands the image to the app
//! (`UiAction::DrawingSent`), which attaches it to his pending utterance without waking the agent. While the overlay
//! is up, any screenshot of that display composites his current strokes (`current_marks`, `mark_png`). No voice
//! command opens or closes it.
//!
//! Heimdall pointing: the agent's `point` tool draws marks (`spec`) on the screen, display-only, then they fade.
//!
//! Everything that touches AppKit runs on the main thread (`open`, `close`, `point`); the image helpers and the
//! marks snapshot are thread-safe.

pub mod ink;
pub mod spec;

pub use ink::{Ink, Pt};
pub use spec::{parse, Spec, AGENT_GUIDE};

use std::sync::{Arc, Mutex, OnceLock};

/// The drawing overlay's window title. Heimdall's screen captures exclude this process's windows with this title
/// (`screen::EXCLUDED_WINDOW_TITLES`, kept equal by a test in crates/app), so a screenshot never shows the overlay.
pub const OVERLAY_WINDOW_TITLE: &str = "Heimdall Draw Overlay";
/// The pointing window's title, excluded from captures the same way.
pub const POINTER_WINDOW_TITLE: &str = "Heimdall Pointer";

/// The user's strokes on the open overlay: the display they are on, the overlay's size in points (the strokes'
/// coordinate space, y up) and the strokes themselves.
#[derive(Clone, Debug, PartialEq)]
pub struct Marks {
    pub display: u32,
    pub view: (f64, f64),
    pub strokes: Vec<Vec<Pt>>,
}

fn marks_slot() -> &'static Mutex<Option<Marks>> {
    static SLOT: OnceLock<Mutex<Option<Marks>>> = OnceLock::new();
    SLOT.get_or_init(Default::default)
}

/// The strokes on the open overlay now, if it is open and has any. Any thread.
pub fn current_marks() -> Option<Marks> {
    marks_slot().lock().unwrap().clone().filter(|m| !m.strokes.is_empty())
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn publish_marks(marks: Option<Marks>) {
    *marks_slot().lock().unwrap() = marks;
}

/// The marked screenshot drawing mode's Send produced: PNG (sRGB) at the display's native pixel size.
#[derive(Clone, PartialEq, Eq)]
pub struct MarkedShot {
    pub png: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub display: u32,
    /// How many strokes it carries.
    pub strokes: usize,
}

impl std::fmt::Debug for MarkedShot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "MarkedShot({}x{}, display {}, {} strokes, {} PNG bytes)", self.width, self.height, self.display, self.strokes, self.png.len())
    }
}

/// A native capture of one display without Heimdall's windows: width, height and packed RGB8, or why not.
pub type Grab = Result<(u32, u32, Vec<u8>), String>;
type Grabber = dyn Fn(u32) -> Grab + Send + Sync;

fn grabber_slot() -> &'static Mutex<Option<Arc<Grabber>>> {
    static SLOT: OnceLock<Mutex<Option<Arc<Grabber>>>> = OnceLock::new();
    SLOT.get_or_init(Default::default)
}

/// How drawing mode's Send captures a display: Heimdall's own Screen Recording path (crates/screen), set once by the
/// app before `run`. Called off the main thread. Without one, Send says the screen cannot be captured.
pub fn set_grabber(grab: impl Fn(u32) -> Grab + Send + Sync + 'static) {
    *grabber_slot().lock().unwrap() = Some(Arc::new(grab));
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn grabber() -> Option<Arc<Grabber>> {
    grabber_slot().lock().unwrap().clone()
}

/// A still of one display drawing mode holds under its overlay: width, height and packed BGRA8 in Display P3, without
/// the pointer, or why not.
pub type Still = Result<(u32, u32, Vec<u8>), String>;
type StillGrabber = dyn Fn(u32) -> Still + Send + Sync;

fn still_slot() -> &'static Mutex<Option<Arc<StillGrabber>>> {
    static SLOT: OnceLock<Mutex<Option<Arc<StillGrabber>>>> = OnceLock::new();
    SLOT.get_or_init(Default::default)
}

/// How drawing mode takes the still it holds while a menu is open ("I would like the Heimdall menu to
/// stay open while I'm drawing"): Heimdall's Screen Recording path, set once by the app. Called off the main thread.
/// Without one, drawing mode opens live as before.
pub fn set_still_grabber(grab: impl Fn(u32) -> Still + Send + Sync + 'static) {
    *still_slot().lock().unwrap() = Some(Arc::new(grab));
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn still_grabber() -> Option<Arc<StillGrabber>> {
    still_slot().lock().unwrap().clone()
}

type SeeingFn = dyn Fn() -> bool + Send + Sync;

fn seeing_slot() -> &'static Mutex<Option<Arc<SeeingFn>>> {
    static SLOT: OnceLock<Mutex<Option<Arc<SeeingFn>>>> = OnceLock::new();
    SLOT.get_or_init(Default::default)
}

/// Whether Seeing is on now, asked before a held still is used or sent (Seeing off: no screen leaves Heimdall). Set
/// once by the app; unset (checks), it is on.
pub fn set_seeing(on: impl Fn() -> bool + Send + Sync + 'static) {
    *seeing_slot().lock().unwrap() = Some(Arc::new(on));
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn seeing() -> bool {
    let on = seeing_slot().lock().unwrap().clone();
    on.is_none_or(|f| f())
}

#[cfg(target_os = "macos")]
pub use crate::mac::draw::api::*;
