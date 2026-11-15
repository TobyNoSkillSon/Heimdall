//! Model-boundary GPU recovery: the core is speak's (`gpu_recovery_core.rs`), with the Metal helpers for objc2.
#[path = "../../speak/src/gpu_recovery_core.rs"]
mod shared;
pub use shared::*;
use anyhow::Result;

const STATUS: &str = "Listening using fallback after a GPU error";

#[cfg(target_os = "macos")]
pub(crate) fn command_buffer(
    queue: &objc2::runtime::ProtocolObject<dyn objc2_metal::MTLCommandQueue>,
) -> Result<objc2::rc::Retained<objc2::runtime::ProtocolObject<dyn objc2_metal::MTLCommandBuffer>>>
{
    use objc2_metal::MTLCommandQueue;
    #[cfg(test)]
    if take_missing() {
        return Err(record("simulated missing Metal command buffer").into());
    }
    queue
        .commandBuffer()
        .ok_or_else(|| record("Metal command buffer unavailable (device lost)").into())
}
#[cfg(target_os = "macos")]
pub(crate) fn completed(
    cmd: &objc2::runtime::ProtocolObject<dyn objc2_metal::MTLCommandBuffer>,
) -> Result<()> {
    use objc2_metal::{MTLCommandBuffer, MTLCommandBufferStatus};
    cmd.waitUntilCompleted();
    if injected() {
        return Err(record("simulated device loss").into());
    }
    if cmd.status() != MTLCommandBufferStatus::Completed {
        let code = cmd.error().map(|e| e.code()).unwrap_or(0);
        return Err(record(format!("{:?}, Metal error code {code}", cmd.status())).into());
    }
    Ok(())
}
