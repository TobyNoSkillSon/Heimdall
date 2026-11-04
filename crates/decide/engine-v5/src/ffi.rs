//! C ABI in the shape of Heimdall's decider dylib (crates/decide/src/d1.rs): create a text backend from a model path,
//! decide on a JSON request, free returned strings, destroy the handle, report a version.
//!
//! Request:  {"state": "<text>", "questions": {"<key>": {"type": "choice"|"noul", "instructions": .., "criteria": ..}}}
//! Response: {"usage": {"input_tokens": N, "output_tokens": 0},
//!            "answers": {"<key>": {"choice": "<best>", "probabilities": {"<name>": p}}  (a choice)
//!                        "<key>": {"noul": p_yes, "probabilities": {"true": p, "false": p}}},  (a noul)
//!            "backend": "<path that ran>", "precision": "bf16"|"int8"|"int4"}
//! Each question is read as its own prompt (the lone-question layout d1's `set` attention gives one question);
//! `input_tokens` is the longest of those prompts.
//!
//! - Every entry point catches panics; every call that touches MLX holds one process-wide lock, so handles may be
//!   used from any thread, one call at a time.
//! - An MLX error that names Metal or the GPU comes back with the `HEIMDALL_GPU_FAULT` marker Heimdall's recovery
//!   looks for; `heimdall_d1_test_fail_next` makes the next decide fail that way.
//! - `timeout_ms` (> 0) is checked before each question's forward: a request past it fails with "timeout". A forward
//!   that has started is not cut short.

use crate::{prompt, Engine, Question};
use serde_json::{json, Map, Value};
use std::ffi::{c_char, c_void, CStr, CString};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const VERSION: &str = concat!("d1v5-engine@", env!("CARGO_PKG_VERSION"), ";mlx-rs-0.32;text-only;json-v1;affine-q8q4-v1\0");
pub const GPU_FAULT: &str = "HEIMDALL_GPU_FAULT";

/// One lock for all MLX work in the process (MLX's streams and caches are process-wide).
static MLX: Mutex<()> = Mutex::new(());

fn mlx_lock() -> std::sync::MutexGuard<'static, ()> {
    MLX.lock().unwrap_or_else(|p| p.into_inner())
}

struct Handle {
    engine: Engine,
    gpu: bool,
    fail_next: AtomicBool,
}

fn set_error(error: *mut *mut c_char, message: &str) {
    if !error.is_null() {
        let text = CString::new(message.replace('\0', " ")).unwrap();
        unsafe { *error = text.into_raw() };
    }
}

/// An error message, marked as a GPU fault when MLX's text says Metal or the GPU failed.
pub fn classify(message: &str) -> String {
    let lower = message.to_lowercase();
    let gpu = ["metal", "command buffer", "mtl", "gpu", "device"].iter().any(|k| lower.contains(k));
    if gpu && !message.contains(GPU_FAULT) {
        format!("{GPU_FAULT}: {message}")
    } else {
        message.to_string()
    }
}

fn run_on<T>(gpu: bool, f: impl FnOnce() -> T) -> T {
    if gpu {
        f()
    } else {
        mlx_rs::with_device(mlx_rs::Device::cpu(), f)
    }
}

/// Answers one request, `stop` checked before each question's forward (Some(reason) stops).
pub fn answer(engine: &Engine, request: &Value, stop: &dyn Fn() -> Option<&'static str>) -> anyhow::Result<Value> {
    let state = prompt::state_text(request.get("state").unwrap_or(&Value::Null))?;
    let questions = request
        .get("questions")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow::anyhow!("\"questions\" must be an object"))?;
    let mut answers = Map::new();
    let mut longest = 0usize;
    for (key, qv) in questions {
        if let Some(reason) = stop() {
            anyhow::bail!("{reason}");
        }
        let q = Question::from_json(qv).map_err(|e| anyhow::anyhow!("{key}: {e}"))?;
        let (probs, tokens) = engine.decide_question(&state, &q)?;
        longest = longest.max(tokens);
        let mut dist = Map::new();
        for (name, p) in &probs {
            dist.insert(name.clone(), json!(p));
        }
        let best = probs
            .iter()
            .fold(None::<&(String, f32)>, |b, x| match b {
                Some(y) if y.1 >= x.1 => Some(y),
                _ => Some(x),
            })
            .map(|x| x.0.clone())
            .unwrap_or_default();
        let a = match q {
            Question::Noul { .. } => json!({"noul": probs[0].1, "probabilities": Value::Object(dist)}),
            Question::Choice { .. } => json!({"choice": best, "probabilities": Value::Object(dist)}),
        };
        answers.insert(key.clone(), a);
    }
    Ok(json!({"usage": {"input_tokens": longest, "output_tokens": 0}, "answers": Value::Object(answers),
              "backend": engine.backend().name(), "precision": engine.precision()}))
}

/// `model`: a d1-2B checkpoint folder (not a file). `projector` is ignored (text only); `_threads` likewise.
#[no_mangle]
pub extern "C" fn heimdall_d1_create_text_backend(
    model: *const c_char,
    _projector: *const c_char,
    _threads: i32,
    gpu: bool,
    error: *mut *mut c_char,
) -> *mut c_void {
    let result = catch_unwind(AssertUnwindSafe(|| -> anyhow::Result<*mut c_void> {
        if model.is_null() {
            anyhow::bail!("null model path");
        }
        let path = unsafe { CStr::from_ptr(model) }.to_str()?.to_string();
        let _lock = mlx_lock();
        let engine = run_on(gpu, || Engine::load(&path))?;
        Ok(Box::into_raw(Box::new(Handle { engine, gpu, fail_next: AtomicBool::new(false) })) as *mut c_void)
    }));
    match result {
        Ok(Ok(p)) => p,
        Ok(Err(e)) => {
            set_error(error, &classify(&format!("{e:#}")));
            std::ptr::null_mut()
        }
        Err(_) => {
            set_error(error, "panic while loading the d1v5 engine");
            std::ptr::null_mut()
        }
    }
}

/// Clears MLX's cache when dropped (`cache::release`).
struct Release;

impl Drop for Release {
    fn drop(&mut self) {
        crate::cache::release();
    }
}

fn decide_impl(handle: *mut c_void, request: *const c_char, timeout_ms: i32, error: *mut *mut c_char,
               interrupted: &dyn Fn() -> bool) -> *mut c_char {
    let result = catch_unwind(AssertUnwindSafe(|| -> anyhow::Result<String> {
        if handle.is_null() || request.is_null() {
            anyhow::bail!("null handle or request");
        }
        let h = unsafe { &*(handle as *const Handle) };
        let body: Value = serde_json::from_str(unsafe { CStr::from_ptr(request) }.to_str()?)?;
        let start = Instant::now();
        let _lock = mlx_lock();
        // after every request (answered, failed or panicked), still under the lock: the activations go back to the
        // system, not into MLX's cache
        let _release = Release;
        if h.fail_next.swap(false, Ordering::AcqRel) {
            anyhow::bail!("{GPU_FAULT}: injected by heimdall_d1_test_fail_next");
        }
        let limit = (timeout_ms > 0).then(|| Duration::from_millis(timeout_ms as u64));
        let stop = || -> Option<&'static str> {
            if interrupted() {
                Some("interrupted")
            } else if limit.is_some_and(|l| start.elapsed() > l) {
                Some("timeout")
            } else {
                None
            }
        };
        let out = run_on(h.gpu, || answer(&h.engine, &body, &stop))?;
        Ok(serde_json::to_string(&out)?)
    }));
    match result {
        Ok(Ok(text)) => CString::new(text).unwrap().into_raw(),
        Ok(Err(e)) => {
            set_error(error, &classify(&format!("{e:#}")));
            std::ptr::null_mut()
        }
        Err(_) => {
            set_error(error, "panic in heimdall_d1_decide");
            std::ptr::null_mut()
        }
    }
}

/// JSON in, JSON out. `timeout_ms` > 0 is checked before each question's forward (see the module notes).
#[no_mangle]
pub extern "C" fn heimdall_d1_decide(handle: *mut c_void, request: *const c_char, timeout_ms: i32,
                                     error: *mut *mut c_char) -> *mut c_char {
    decide_impl(handle, request, timeout_ms, error, &|| false)
}

/// As `heimdall_d1_decide`, with `interrupted(ctx)` polled before each question's forward.
#[no_mangle]
pub extern "C" fn heimdall_d1_decide_interruptible(
    handle: *mut c_void,
    request: *const c_char,
    timeout_ms: i32,
    interrupted: unsafe extern "C" fn(*mut c_void) -> bool,
    ctx: *mut c_void,
    error: *mut *mut c_char,
) -> *mut c_char {
    decide_impl(handle, request, timeout_ms, error, &|| unsafe { interrupted(ctx) })
}

/// Token count of `text` under the checkpoint's tokenizer (no special tokens added); -1 on failure.
#[no_mangle]
pub extern "C" fn heimdall_d1_tokens(handle: *mut c_void, text: *const c_char) -> i32 {
    catch_unwind(AssertUnwindSafe(|| {
        if handle.is_null() || text.is_null() {
            return -1;
        }
        let h = unsafe { &*(handle as *const Handle) };
        // Counted as the prompt carries the text (`Prompt::plain`: special-token strings broken up), so Heimdall's
        // window fit sees the tokens the forward pass reads; a raw count undercounts "<|im_end|>" and the like.
        match unsafe { CStr::from_ptr(text) }.to_str().ok().and_then(|t| h.engine.prompt.encode_text(&h.engine.prompt.plain(t)).ok()) {
            Some(ids) => ids.len() as i32,
            None => -1,
        }
    }))
    .unwrap_or(-1)
}

/// The next decide on `handle` fails with the GPU-fault marker (Heimdall's recovery tests).
#[no_mangle]
pub extern "C" fn heimdall_d1_test_fail_next(handle: *mut c_void) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if !handle.is_null() {
            unsafe { &*(handle as *const Handle) }.fail_next.store(true, Ordering::Release);
        }
    }));
}

#[no_mangle]
pub extern "C" fn heimdall_d1_free(text: *mut c_char) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if !text.is_null() {
            unsafe { drop(CString::from_raw(text)) };
        }
    }));
}

#[no_mangle]
pub extern "C" fn heimdall_d1_destroy(handle: *mut c_void) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if !handle.is_null() {
            let _lock = mlx_lock();
            unsafe { drop(Box::from_raw(handle as *mut Handle)) };
            // give the freed weights and activations back to the system rather than keeping them in MLX's cache
            // (always, also under D1E_CACHE_LIMIT_MB=off, as before)
            let _ = mlx_rs::memory::clear_cache();
        }
    }));
}

#[no_mangle]
pub extern "C" fn heimdall_d1_version() -> *const c_char {
    VERSION.as_ptr() as *const c_char
}

/// MLX keeps no device state that needs recreating between calls; always ready.
#[no_mangle]
pub extern "C" fn heimdall_d1_recreate_device() -> bool {
    true
}
