//! Floating Islands in Rust: small black islands over every app and Space, driven entirely by files.
//!
//! The same program as `islands/` (Swift): the same data folder and file protocol (`islands/README.md`), the same
//! command-line flags, the same look and motion (Core Animation layers through objc2, springs in the render server,
//! nothing running at rest).
//!
//! - `model`: the island file format, plan lists, due times, placement maths and the viewer's kinds; shared by the
//!   app, the CLI and the tests (no AppKit, apart from Foundation's calendar and UTType for exact parity).
//! - `mac`: the FloatingIslands app (AppKit + Core Animation).
//! - `cli`: the `islands` command.

pub mod model;

pub mod cli;

#[cfg(target_os = "macos")]
pub mod mac;
