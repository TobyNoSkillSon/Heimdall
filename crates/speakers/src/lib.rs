//! Local diarization, voice identity and passive name suggestions. No permission requests.
mod artifacts;
pub mod call;
pub mod codec;
pub mod diar;
pub mod embedding;
// Shared with speak; its command-buffer helpers and `Recovery::retry_now` serve speak's own Metal encoders.
#[allow(dead_code)]
#[path = "../../speak/src/gpu_recovery.rs"]
pub mod gpu_recovery;
pub mod identity;
#[cfg(target_os = "macos")]
pub mod metal;
pub mod naming;
pub mod pipeline;
pub mod scoring;

#[cfg(not(target_os = "macos"))]
mod native_fault_hooks {
    #[unsafe(no_mangle)]
    extern "C" fn heimdall_nemotron_recreate(_: bool) -> bool {
        true
    }
    #[unsafe(no_mangle)]
    extern "C" fn heimdall_nemotron_cpu_status(_: bool) {}
    #[unsafe(no_mangle)]
    extern "C" fn heimdall_nemotron_retry_now() -> bool {
        false
    }
    #[unsafe(no_mangle)]
    extern "C" fn heimdall_nemotron_test_inject(_: usize) {}
}
