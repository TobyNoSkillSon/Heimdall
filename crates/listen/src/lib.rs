//! Offline-capable per-source listening engine. Call on one serialized inference
//! thread. Capture provides host-clock nanoseconds; the app adapts its chunks.
pub mod asr;
#[cfg(target_os = "macos")]
mod coreml;
mod engine;
pub mod features;
pub mod gpu_recovery;
pub mod long;
#[cfg(target_os = "macos")]
pub mod metal;
pub mod onnx;
pub mod onset;
mod silero;
pub mod timing;
pub mod vocabulary;
pub use asr::{Recognizer, Transcript, Word};
pub use engine::*;

pub mod file;
mod verification;
pub use verification::*;
mod worker;
pub use worker::*;

#[cfg(target_os = "macos")]
pub mod placement;
