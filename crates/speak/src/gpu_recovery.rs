//! Shared model-boundary GPU recovery. ORT erases custom error types, so a
//! thread-local receipt preserves the typed fault until the unit returns.
#![allow(unexpected_cfgs)] // objc 0.2 selector macros use a historical cargo-clippy cfg.
use anyhow::Result;
use std::{
    cell::RefCell,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    time::{Duration, Instant},
};

#[derive(Clone, Debug)]
pub struct GpuError(pub String);
impl std::fmt::Display for GpuError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "GPU command failed: {}", self.0)
    }
}
impl std::error::Error for GpuError {}
thread_local! {
    static FAULT: RefCell<Option<GpuError>> = const { RefCell::new(None) };
    #[cfg(test)]
    static INJECT: RefCell<usize> = const { RefCell::new(0) };
}
static FALLBACKS: AtomicUsize = AtomicUsize::new(0);
static NATIVE_CPU: AtomicBool = AtomicBool::new(false);
/// Menu integration reads this without consuming any model's fallback status.
pub fn gpu_status() -> Option<&'static str> {
    (FALLBACKS.load(Ordering::Relaxed) > 0 || NATIVE_CPU.load(Ordering::Relaxed))
        .then_some("Voice running on CPU after a GPU error")
}
pub fn cpu_status() {
    NATIVE_CPU.store(true, Ordering::Relaxed);
}
pub fn gpu_restored() {
    NATIVE_CPU.store(false, Ordering::Relaxed);
}
pub(crate) fn injected() -> bool {
    #[cfg(test)]
    {
        INJECT.with(|v| {
            let mut n = v.borrow_mut();
            let fail = *n > 0;
            *n = n.saturating_sub(1);
            fail
        })
    }
    #[cfg(not(test))]
    {
        false
    }
}
#[cfg(test)]
pub(crate) fn inject(n: usize) {
    INJECT.with(|v| *v.borrow_mut() = n);
}
pub(crate) fn record(message: impl Into<String>) -> GpuError {
    let error = GpuError(message.into());
    FAULT.with(|v| *v.borrow_mut() = Some(error.clone()));
    error
}
pub(crate) fn checked<T>(f: impl FnOnce() -> Result<T>) -> Result<T> {
    FAULT.with(|v| v.borrow_mut().take());
    let result = f();
    if let Some(error) = FAULT.with(|v| v.borrow_mut().take()) {
        return Err(error.into());
    }
    match result {
        Err(error) if error.to_string().contains("GPU command failed:") => {
            Err(GpuError(error.to_string()).into())
        }
        result => result,
    }
}
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
pub struct Recovery<T> {
    pub(crate) model: Option<T>,
    gpu: bool,
    wants_gpu: bool,
    cpu_reported: bool,
    retry_at: Option<Instant>,
}
impl<T> Recovery<T> {
    pub fn is_gpu(&self) -> bool {
        self.gpu
    }
    #[cfg(test)]
    pub(crate) fn retry_now(&mut self) {
        self.retry_at = Some(Instant::now());
    }
    pub fn new(model: T, gpu: bool) -> Self {
        Self {
            model: Some(model),
            gpu,
            wants_gpu: gpu,
            cpu_reported: false,
            retry_at: None,
        }
    }
    fn mark_cpu(&mut self) {
        if !self.cpu_reported {
            self.cpu_reported = true;
            FALLBACKS.fetch_add(1, Ordering::Relaxed);
        }
    }
    fn mark_gpu(&mut self) {
        if self.cpu_reported {
            self.cpu_reported = false;
            FALLBACKS.fetch_sub(1, Ordering::Relaxed);
        }
    }
    /// Drop before rebuilding: never overlap old and replacement weights/queues.
    fn replace(&mut self, gpu: bool, load: &mut impl FnMut(bool) -> Result<T>) -> Result<()> {
        self.model.take();
        self.gpu = gpu;
        self.model = Some(load(gpu)?);
        Ok(())
    }
    pub fn run<R>(
        &mut self,
        mut load: impl FnMut(bool) -> Result<T>,
        mut work: impl FnMut(&mut T) -> Result<R>,
    ) -> Result<R> {
        if self.wants_gpu
            && !self.gpu
            && self.retry_at.is_some_and(|t| Instant::now() >= t)
            && self.replace(true, &mut load).is_err()
        {
            self.retry_at = Some(Instant::now() + Duration::from_secs(30));
            self.replace(false, &mut load)?;
        }
        if self.model.is_none() {
            self.replace(false, &mut load)?;
        }
        match checked(|| work(self.model.as_mut().unwrap())) {
            Ok(output) => {
                if self.gpu {
                    self.mark_gpu();
                }
                return Ok(output);
            }
            Err(error) if self.gpu && error.downcast_ref::<GpuError>().is_some() => {}
            Err(error) => return Err(error),
        }
        // A single immediate retry on a fresh device, queue, pipelines and buffers.
        if self.replace(true, &mut load).is_ok() {
            match checked(|| work(self.model.as_mut().unwrap())) {
                Ok(output) => {
                    self.mark_gpu();
                    return Ok(output);
                }
                Err(error) if error.downcast_ref::<GpuError>().is_some() => {}
                Err(error) => return Err(error),
            }
        }
        self.retry_at = Some(Instant::now() + Duration::from_secs(30));
        self.mark_cpu();
        self.replace(false, &mut load)?;
        checked(|| work(self.model.as_mut().unwrap()))
    }
}

impl<T> Drop for Recovery<T> {
    fn drop(&mut self) {
        self.mark_gpu();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fault_receipt_overrides_even_finite_stale_success() {
        let result = checked(|| {
            record("lost device");
            Ok(vec![42.0_f32])
        });
        assert!(result.unwrap_err().downcast_ref::<GpuError>().is_some());
    }
    #[test]
    fn recreate_fallback_backoff_and_restore() {
        let mut state = Recovery::new(true, true);
        let mut loads = Vec::new();
        inject(1);
        let work = |gpu: &mut bool| {
            if *gpu && injected() {
                Err(record("lost").into())
            } else {
                Ok(*gpu)
            }
        };
        assert!(
            state
                .run(
                    |gpu| {
                        loads.push(gpu);
                        Ok(gpu)
                    },
                    work
                )
                .unwrap()
        );
        assert_eq!(loads, [true]);
        inject(2);
        assert!(!state.run(Ok, work).unwrap());
        assert!(!state.gpu);
        assert!(!state.run(Ok, work).unwrap());
        state.retry_at = Some(Instant::now());
        assert!(state.run(Ok, work).unwrap());
        assert!(state.gpu);
    }
    #[test]
    fn failed_recreation_uses_cpu_and_input_errors_do_not_retry() {
        let mut state = Recovery::new(true, true);
        let mut rebuilds = 0;
        inject(1);
        let output = state
            .run(
                |gpu| {
                    rebuilds += 1;
                    if gpu {
                        anyhow::bail!("device unavailable");
                    }
                    Ok(false)
                },
                |gpu| {
                    if *gpu && injected() {
                        Err(record("lost device").into())
                    } else {
                        Ok(17)
                    }
                },
            )
            .unwrap();
        assert_eq!(output, 17);
        assert_eq!(rebuilds, 2);
        assert!(!state.is_gpu());
        let mut called = false;
        assert!(
            state
                .run(
                    |_| {
                        called = true;
                        Ok(false)
                    },
                    |_| Err::<(), _>(anyhow::anyhow!("bad input"))
                )
                .is_err()
        );
        assert!(!called);
    }
    #[test]
    fn concurrent_streams_fault_without_cross_contamination() {
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(6));
        let handles: Vec<_> = (0..6)
            .map(|i| {
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let mut state = Recovery::new((true, i), true);
                    if i == 2 {
                        inject(2);
                    }
                    barrier.wait();
                    state
                        .run(
                            |gpu| Ok((gpu, i)),
                            |(gpu, id)| {
                                if *gpu && injected() {
                                    Err(record("device loss").into())
                                } else {
                                    Ok((*gpu, *id))
                                }
                            },
                        )
                        .unwrap()
                })
            })
            .collect();
        for (i, h) in handles.into_iter().enumerate() {
            assert_eq!(h.join().unwrap(), (i != 2, i));
        }
    }
}
