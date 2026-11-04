//! Drawing's public API (re-exported as `ui::drawing::*` on macOS). `open`, `close`, `is_open` and `point` run on the
//! main thread (the app calls them through `Ui::on_main`); the image helpers and `pointer_display` are thread-safe;
//! `bring_forward` blocks and must not run on the main thread.

use std::time::Duration;

use objc2_foundation::MainThreadMarker;

use crate::drawing::{Marks, Pt, Spec};

fn main_thread() -> Result<MainThreadMarker, String> {
    MainThreadMarker::new().ok_or_else(|| "drawing mode runs on the main thread".to_string())
}

/// Opens the user's drawing mode on the display he is on. Returns the line the agent reads.
pub fn open() -> Result<String, String> {
    let mtm = main_thread()?;
    let was_open = super::overlay::is_open();
    let (index, _) = super::overlay::open(mtm)?;
    Ok(if was_open {
        format!("Drawing mode is already open on display {index}.")
    } else {
        format!("Drawing mode is open on display {index}: User draws, Return attaches the marked screenshot to his next message, Esc closes.")
    })
}

/// Closes drawing mode, discarding the strokes. Whether it was open.
pub fn close() -> Result<bool, String> {
    let mtm = main_thread()?;
    let was_open = super::overlay::is_open();
    super::overlay::close(mtm);
    Ok(was_open)
}

pub fn is_open() -> bool {
    super::overlay::is_open()
}

/// Heimdall points: checks the commands, shows the marks (replacing any still showing) and returns at once with one
/// line ("showing 2 marks on display 1 (1728x1117 pt)"), or the first mistake ("line 2: …").
pub fn point(text: &str) -> Result<String, String> {
    let mtm = main_thread()?;
    let spec: Spec = crate::drawing::parse(text)?;
    super::point::show(&spec, 1.0, mtm)
}

/// Packed RGB8 with the strokes drawn on top; `view` is the overlay's size in points (the strokes' space).
pub fn composite_rgb(rgb: &[u8], w: u32, h: u32, view: (f64, f64), strokes: &[Vec<Pt>]) -> Result<Vec<u8>, String> {
    super::ink::composite_rgb(rgb, w, h, view, strokes)
}

/// A PNG screenshot of `marks.display` with the user's strokes composited (take-a-look while drawing mode is up).
pub fn mark_png(png: &[u8], marks: &Marks) -> Result<Vec<u8>, String> {
    super::ink::mark_png(png, marks)
}

pub fn encode_png(w: u32, h: u32, rgb: &[u8]) -> Result<Vec<u8>, String> {
    super::ink::encode_png(w, h, rgb)
}

pub fn resize_rgb(rgb: &[u8], w: u32, h: u32, to: (u32, u32)) -> Result<Vec<u8>, String> {
    super::ink::resize_rgb(rgb, w, h, to)
}

/// The display under the pointer: its id, its 1-based index (the main display first), its size in points.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PointerDisplay {
    pub id: u32,
    pub index: usize,
    pub width: f64,
    pub height: f64,
}

/// Where the pointer is now (any thread: CoreGraphics only).
pub fn pointer_display() -> Option<PointerDisplay> {
    super::look::pointer_display()
}

/// Brings `app` forward (switching to its Space when it lives on another one) and waits until macOS says the switch
/// is done, rather than a fixed delay. Returns how long it took. Blocks; never call it on the main thread.
pub fn bring_forward(app: &str) -> Result<Duration, String> {
    super::look::bring_forward(app)
}
