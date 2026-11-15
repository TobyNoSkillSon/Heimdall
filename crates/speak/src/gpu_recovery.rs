//! Model-boundary GPU recovery: the shared core (`gpu_recovery_core.rs`) and the Metal helpers for the `metal` crate.
//! decide and speakers include this file too.
#![allow(unexpected_cfgs)] // objc 0.2 selector macros use a historical cargo-clippy cfg.
#[path = "gpu_recovery_core.rs"]
mod shared;
pub use shared::*;
use anyhow::Result;

const STATUS: &str = "Voice running on CPU after a GPU error";

#[cfg(target_os = "macos")]
pub(crate) fn command_buffer(queue: &metal::CommandQueueRef) -> Result<&metal::CommandBufferRef> {
    use metal::foreign_types::ForeignTypeRef;
    use objc::{msg_send, sel, sel_impl};
    let buffer: *mut objc::runtime::Object = unsafe { msg_send![queue, commandBuffer] };
    if buffer.is_null() {
        return Err(record("Metal command buffer unavailable (device lost)").into());
    }
    Ok(unsafe { metal::CommandBufferRef::from_ptr(buffer.cast()) })
}
#[cfg(target_os = "macos")]
pub(crate) fn completed(cmd: &metal::CommandBufferRef) -> Result<()> {
    use objc::{msg_send, sel, sel_impl};
    cmd.wait_until_completed();
    if injected() {
        return Err(record("simulated device loss").into());
    }
    if cmd.status() != metal::MTLCommandBufferStatus::Completed {
        let code: i64 = unsafe {
            let error: *mut objc::runtime::Object = msg_send![cmd, error];
            if error.is_null() {
                0
            } else {
                msg_send![error, code]
            }
        };
        return Err(record(format!("{:?}, Metal error code {code}", cmd.status())).into());
    }
    Ok(())
}
