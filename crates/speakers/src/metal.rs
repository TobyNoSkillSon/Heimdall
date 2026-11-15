//! The Rust side of the hooks the vendored GGML Metal backend and NeMo diarizer call: summed GPU time, and the GPU
//! fault recovery of the shared Nemotron model (`gpu_recovery.rs`). The experimental Rust/Metal projections and
//! ReDimNet convolutions did not qualify (11 Oct) and were removed on 14 Nov 2026 (git history); the projection hook
//! stays because the vendored `ggml-metal-ops.cpp` asks it, and always leaves the projection to GGML.
use std::sync::atomic::{AtomicU64, Ordering};
static GPU_NANOS: AtomicU64 = AtomicU64::new(0);
/// Summed command-buffer GPU intervals. Concurrent buffers can overlap;
/// this measures submitted work intervals, not whole-device busy time.
pub fn gpu_seconds() -> f64 {
    GPU_NANOS.load(Ordering::Relaxed) as f64 / 1e9
}
#[unsafe(no_mangle)]
extern "C" fn heimdall_metal_graph_time(seconds: f64) {
    if seconds.is_finite() && seconds > 0. {
        GPU_NANOS.fetch_add((seconds * 1e9) as u64, Ordering::Relaxed);
    }
}

/// No custom projection is registered: GGML encodes every projection itself.
#[unsafe(no_mangle)]
extern "C" fn heimdall_metal_projection_active() -> bool {
    false
}
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)] // The vendored C declaration.
extern "C" fn heimdall_metal_projection(
    _encoder: *mut std::ffi::c_void,
    _wb: *mut std::ffi::c_void,
    _wo: usize,
    _xb: *mut std::ffi::c_void,
    _xo: usize,
    _yb: *mut std::ffi::c_void,
    _yo: usize,
    _k: u32,
    _n: u32,
    _m: u32,
    _kind: i32,
) -> bool {
    false
}

#[unsafe(no_mangle)]
extern "C" fn heimdall_nemotron_fault() -> bool {
    crate::gpu_recovery::injected()
}
#[unsafe(no_mangle)]
extern "C" fn heimdall_nemotron_cpu_status(cpu: bool) {
    if cpu {
        crate::gpu_recovery::cpu_status();
    } else {
        crate::gpu_recovery::gpu_restored();
    }
}
/// The native model rebuilds its own GGML state; nothing on the Rust side holds GPU objects.
#[unsafe(no_mangle)]
extern "C" fn heimdall_nemotron_recreate(_gpu: bool) -> bool {
    true
}

#[cfg(test)]
thread_local! { static RETRY_NOW: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }
#[unsafe(no_mangle)]
extern "C" fn heimdall_nemotron_retry_now() -> bool {
    #[cfg(test)]
    {
        RETRY_NOW.with(|v| v.replace(false))
    }
    #[cfg(not(test))]
    {
        false
    }
}
#[unsafe(no_mangle)]
extern "C" fn heimdall_nemotron_test_inject(n: usize) {
    #[cfg(test)]
    crate::gpu_recovery::inject(n);
    #[cfg(not(test))]
    let _ = n;
}

#[cfg(test)]
mod fault_tests {
    use anyhow::Result;
    use std::sync::Mutex;
    static NATIVE_TEST_LOCK: Mutex<()> = Mutex::new(());
    unsafe extern "C" {
        fn heimdall_projection_fixture(
            rows: i32,
            cols: i32,
            inner: i32,
            kind: i32,
            output: *mut f32,
        ) -> i32;
        fn heimdall_nemotron_contention_fixture(path: *const std::ffi::c_char) -> i32;
    }
    #[test]
    fn gpu_fault_native_rejects_readback_before_return() {
        let _guard = NATIVE_TEST_LOCK.lock().unwrap();
        objc2::rc::autoreleasepool(|_| {
            let mut output = vec![42_f32; 32];
            crate::gpu_recovery::inject(1);
            assert_ne!(
                unsafe { heimdall_projection_fixture(1, 32, 32, 1, output.as_mut_ptr()) },
                0
            );
            assert!(output.iter().all(|x| *x == 42.));
            assert_eq!(
                unsafe { heimdall_projection_fixture(1, 32, 32, 1, output.as_mut_ptr()) },
                0
            );
            assert!(output.iter().any(|x| *x != 42.));
        });
    }
    #[test]
    #[ignore = "requires pinned offline Nemotron assets"]
    fn gpu_fault_nemotron_shared_stream_contention() {
        let _guard = NATIVE_TEST_LOCK.lock().unwrap();
        let path = std::ffi::CString::new(format!(
            "{}/../../models/speakers/Nemotron-3-Diarization.q8_0.gguf",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap();
        assert_eq!(
            unsafe { heimdall_nemotron_contention_fixture(path.as_ptr()) },
            0
        );
    }
    #[test]
    #[ignore = "requires pinned offline Nemotron assets"]
    fn gpu_fault_nemotron_recreates_falls_back_and_recovers() -> Result<()> {
        let _guard = NATIVE_TEST_LOCK.lock().unwrap();
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../models/speakers/Nemotron-3-Diarization.q8_0.gguf");
        let model = crate::diar::Diarizer::load(&path, 0)?;
        let mut stream = model.stream(16000)?;
        let audio: Vec<_> = (0..16000).map(|i| (i as f32 * 0.03).sin() * 0.1).collect();
        crate::gpu_recovery::inject(1);
        stream.push_f32(&audio)?;
        assert!(crate::gpu_recovery::gpu_status().is_none());
        crate::gpu_recovery::inject(2);
        stream.push_f32(&audio)?;
        assert!(crate::gpu_recovery::gpu_status().is_some());
        super::RETRY_NOW.with(|v| v.set(true));
        stream.push_f32(&audio)?;
        assert!(crate::gpu_recovery::gpu_status().is_none());
        stream.finish()?;
        let probabilities = stream.frame_probabilities()?;
        assert!(
            probabilities
                .values
                .iter()
                .all(|x| x.is_finite() && (0.0..=1.0).contains(x))
        );
        assert!(!probabilities.values.is_empty());
        Ok(())
    }
}
