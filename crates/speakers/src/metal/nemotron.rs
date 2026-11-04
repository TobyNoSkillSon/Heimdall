//! Encodes into GGML's existing command encoder. No extra queue submission or
//! per-layer host sync; the native graph keeps its scheduling/attention/cache.
use super::*;
use ::metal::foreign_types::ForeignTypeRef;
use std::{
    collections::HashMap,
    ffi::c_void,
    sync::{OnceLock, Weak},
};
struct Native {
    gpu: Option<Gpu>,
    disabled: bool,
    weights: HashMap<(usize, u32, u32), Buffer>,
    scratch: Vec<Buffer>,
    dispatches: u64,
}
#[cfg(test)]
static NATIVE_TEST_LOCK: Mutex<()> = Mutex::new(());
static ACTIVE: OnceLock<Mutex<Weak<Mutex<Native>>>> = OnceLock::new();
pub(crate) struct NemotronGpu {
    _owner: Arc<Mutex<Native>>,
}
impl NemotronGpu {
    pub(crate) fn new() -> Result<Self> {
        let mut active = ACTIVE
            .get_or_init(|| Mutex::new(Weak::new()))
            .lock()
            .map_err(|_| anyhow!("Metal registration poisoned"))?;
        ensure!(
            active.upgrade().is_none(),
            "custom Nemotron path requires one shared model instance"
        );
        let owner = Arc::new(Mutex::new(Native {
            gpu: Some(Gpu::new()?),
            disabled: false,
            weights: HashMap::new(),
            scratch: Vec::new(),
            dispatches: 0,
        }));
        *active = Arc::downgrade(&owner);
        eprintln!("speakers Nemotron: custom FP16 Metal projections, original Q8 dequantized once");
        Ok(Self { _owner: owner })
    }
}
// GGML graph command buffers use unretained references. Hold activation buffers
// until the native invocation synchronizes/readbacks, then release them.
pub(crate) fn drain_native_scratch() {
    if let Some(owner) = ACTIVE.get().and_then(|a| a.lock().ok()?.upgrade()) {
        if let Ok(mut native) = owner.lock() {
            native.scratch.clear();
        }
    }
}

// Avoid even the native buffer-binding/shape work on the baseline path.
#[unsafe(no_mangle)]
extern "C" fn heimdall_metal_projection_active() -> bool {
    ACTIVE.get().is_some_and(|active| {
        active.lock().is_ok_and(|owner| {
            owner
                .upgrade()
                .is_some_and(|n| n.lock().is_ok_and(|n| !n.disabled))
        })
    })
}

/// Falls back when no live custom model is registered. The C++ guard admits
/// contiguous Q8_0/F16 weights and FP32 activations; all extents are multiples
/// of 32 in K. Borrowed ObjC objects belong to the calling graph.
#[unsafe(no_mangle)]
#[allow(clippy::chunks_exact_to_as_chunks)] // Mirror GGML's 34-byte Q8 wire blocks.
unsafe extern "C" fn heimdall_metal_projection(
    encoder: *mut c_void,
    wb: *mut c_void,
    wo: usize,
    xb: *mut c_void,
    xo: usize,
    yb: *mut c_void,
    yo: usize,
    k: u32,
    n: u32,
    m: u32,
    kind: i32,
) -> bool {
    let Some(owner) = ACTIVE.get().and_then(|a| a.lock().ok()?.upgrade()) else {
        return false;
    };
    let Ok(mut native) = owner.lock() else {
        return false;
    };
    if native.disabled {
        return false;
    }
    let e = unsafe { ComputeCommandEncoderRef::from_ptr(encoder.cast()) };
    let w = unsafe { BufferRef::from_ptr(wb.cast()) };
    let x = unsafe { BufferRef::from_ptr(xb.cast()) };
    let y = unsafe { BufferRef::from_ptr(yb.cast()) };
    let key = (unsafe { w.contents().add(wo) } as usize, k, n);
    if !native.weights.contains_key(&key) {
        let mut values = vec![0_f32; k as usize * n as usize];
        if kind == 8 {
            // ggml block_q8_0: half scale + 32 signed codes.
            let raw = unsafe {
                std::slice::from_raw_parts(
                    w.contents().add(wo).cast::<u8>(),
                    values.len() / 32 * 34,
                )
            };
            for (block, output) in raw.chunks_exact(34).zip(values.chunks_exact_mut(32)) {
                let scale = f16::from_bits(u16::from_le_bytes([block[0], block[1]])).to_f32();
                for (v, q) in output.iter_mut().zip(&block[2..]) {
                    *v = scale * (*q as i8) as f32;
                }
            }
        } else {
            let raw = unsafe {
                std::slice::from_raw_parts(w.contents().add(wo).cast::<f16>(), values.len())
            };
            for (v, h) in values.iter_mut().zip(raw) {
                *v = h.to_f32();
            }
        }
        let np = n.next_multiple_of(32) as usize;
        let k = k as usize;
        let mut halves = vec![f16::ZERO; np * k * 2];
        for (index, v) in values.iter().enumerate() {
            let hi = f16::from_f32(*v);
            halves[index] = hi;
            halves[np * k + index] = f16::from_f32(*v - hi.to_f32());
        }
        let packed = native.gpu.as_ref().unwrap().buffer(&halves);
        native.weights.insert(key, packed);
        if std::env::var_os("HEIMDALL_SPEAKERS_SHADER_TRACE").is_some() {
            eprintln!("speakers shader projection K={k} N={n} M={m} type={kind}");
        }
    }
    if m <= 8 {
        e.set_compute_pipeline_state(&native.gpu.as_ref().unwrap().mv);
        e.set_buffer(0, Some(x), xo as u64);
        e.set_buffer(1, Some(&native.weights[&key]), 0);
        e.set_buffer(2, Some(y), yo as u64);
        bytes(e, 3, &[m, k, n, 0]);
        e.dispatch_thread_groups(
            MTLSize::new(n.div_ceil(4) as u64, 1, 1),
            MTLSize::new(128, 1, 1),
        );
        e.memory_barrier_with_resources(&[y]);
        native.dispatches += 1;
        return true;
    }
    let mp = m.next_multiple_of(32);
    let a = native
        .gpu
        .as_ref()
        .unwrap()
        .allocate(mp as usize * k as usize * 2);
    e.set_compute_pipeline_state(&native.gpu.as_ref().unwrap().linear);
    e.set_buffer(0, Some(x), xo as u64);
    e.set_buffer(1, Some(&a), 0);
    bytes(e, 2, &[m, k, n, mp]);
    dispatch(e, mp as usize * k as usize);
    e.memory_barrier_with_resources(&[&a]);
    let weights = &native.weights[&key];
    native.gpu.as_ref().unwrap().encode_gemm(
        e,
        &a,
        weights,
        weights,
        y,
        yo as u64,
        [m, k, n, mp],
        [0, 0, 0, 0],
    );
    e.memory_barrier_with_resources(&[y]);
    native.scratch.push(a);
    native.dispatches += 1;
    true
}
impl Drop for Native {
    fn drop(&mut self) {
        if std::env::var_os("HEIMDALL_SPEAKERS_SHADER_TRACE").is_some() {
            eprintln!(
                "speakers shader projections={} cached_weights={}",
                self.dispatches,
                self.weights.len()
            );
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    unsafe extern "C" {
        fn heimdall_projection_fixture(
            rows: i32,
            cols: i32,
            inner: i32,
            kind: i32,
            out: *mut f32,
        ) -> i32;
    }
    #[test]
    fn projections_match_current_ggml_on_fixed_tensors() {
        let _guard = NATIVE_TEST_LOCK.lock().unwrap();
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/fixtures/metal-projection.json"))
                .unwrap();
        objc2::rc::autoreleasepool(|_| {
            for row in fixture["cases"].as_array().unwrap() {
                let [m, n, k, kind]: [i32; 4] =
                    serde_json::from_value(row["shape"].clone()).unwrap();
                let reference: Vec<f32> = serde_json::from_value(row["output"].clone()).unwrap();
                let owner = NemotronGpu::new().unwrap();
                let mut output = vec![0_f32; reference.len()];
                assert_eq!(
                    unsafe { heimdall_projection_fixture(m, n, k, kind, output.as_mut_ptr()) },
                    0
                );
                drain_native_scratch();
                let native = owner._owner.lock().unwrap();
                assert_eq!(native.dispatches, 1);
                drop(native);
                let max = output
                    .iter()
                    .zip(&reference)
                    .map(|(a, b)| (a - b).abs())
                    .fold(0_f32, f32::max);
                let rms = (output
                    .iter()
                    .zip(&reference)
                    .map(|(a, b)| (a - b).powi(2))
                    .sum::<f32>()
                    / output.len() as f32)
                    .sqrt();
                println!("Projection ({m},{n},{k},{kind}): max={max} rms={rms}");
                assert!(
                    max < 0.001 && rms < 0.0003,
                    "projection ({m},{n},{k},{kind}): max={max} rms={rms}"
                );
            }
        });
    }
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
#[unsafe(no_mangle)]
extern "C" fn heimdall_nemotron_recreate(gpu: bool) -> bool {
    if let Some(owner) = ACTIVE.get().and_then(|a| a.lock().ok()?.upgrade()) {
        if let Ok(mut native) = owner.lock() {
            native.scratch.clear();
            native.weights.clear();
            // Registration remains stable while the shared native model rebuilds.
            native.gpu.take();
            if gpu {
                match Gpu::new() {
                    Ok(fresh) => native.gpu = Some(fresh),
                    Err(_) => {
                        native.disabled = true;
                        return false;
                    }
                }
            }
            native.disabled = !gpu;
        }
    }
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
    use super::*;
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
        objc::rc::autoreleasepool(|| {
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
        RETRY_NOW.with(|v| v.set(true));
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
