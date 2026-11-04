//! Shared model-boundary GPU recovery. ORT erases custom error types, so a
//! thread-local receipt preserves the typed fault until the unit returns.
#![allow(unexpected_cfgs)] // objc 0.2 selector macros use a historical cargo-clippy cfg.
use anyhow::Result;
use std::{
    cell::RefCell,
    sync::atomic::{AtomicUsize, Ordering},
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
    #[cfg(test)]
    static SKIP: RefCell<usize> = const { RefCell::new(0) };
    #[cfg(test)]
    static MISSING: RefCell<bool> = const { RefCell::new(false) };
}
static FALLBACKS: AtomicUsize = AtomicUsize::new(0);
/// Non-consuming aggregate: a healthy turn model cannot clear ASR fallback.
pub fn gpu_status() -> Option<&'static str> {
    (FALLBACKS.load(Ordering::Relaxed) > 0).then_some("Listening using fallback after a GPU error")
}
pub(crate) fn injected() -> bool {
    #[cfg(test)]
    {
        if SKIP.with(|v| { let mut n = v.borrow_mut(); let skip = *n > 0; *n = n.saturating_sub(1); skip }) { return false; }
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
    SKIP.with(|v| *v.borrow_mut() = 0);
    INJECT.with(|v| *v.borrow_mut() = n);
}
#[cfg(test)]
pub(crate) fn inject_after(skip: usize, n: usize) {
    inject(n);
    SKIP.with(|v| *v.borrow_mut() = skip);
}
#[cfg(test)]
pub(crate) fn inject_missing() { MISSING.with(|v| *v.borrow_mut() = true); }
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
pub(crate) fn command_buffer(
    queue: &objc2::runtime::ProtocolObject<dyn objc2_metal::MTLCommandQueue>,
) -> Result<objc2::rc::Retained<objc2::runtime::ProtocolObject<dyn objc2_metal::MTLCommandBuffer>>>
{
    use objc2_metal::MTLCommandQueue;
    #[cfg(test)]
    if MISSING.with(|v| std::mem::replace(&mut *v.borrow_mut(), false)) {
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
    pub fn load(gpu: bool, mut load: impl FnMut(bool) -> Result<T>) -> Result<Self> {
        match load(gpu) {
            Ok(model) => Ok(Self::new(model, gpu)),
            Err(_) if gpu => {
                if let Ok(model) = load(true) { return Ok(Self::new(model, true)); }
                let mut state = Self::new(load(false)?, false);
                state.wants_gpu = true;
                state.retry_at = Some(Instant::now() + Duration::from_secs(30));
                state.mark_cpu();
                Ok(state)
            }
            Err(e) => Err(e),
        }
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
        assert!(state
            .run(
                |gpu| {
                    loads.push(gpu);
                    Ok(gpu)
                },
                work
            )
            .unwrap());
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
        assert!(state
            .run(
                |_| {
                    called = true;
                    Ok(false)
                },
                |_| Err::<(), _>(anyhow::anyhow!("bad input"))
            )
            .is_err());
        assert!(!called);
    }
    #[test]
    fn startup_fault_and_independent_fallback_owners() {
        let mut a = Recovery::load(true, |gpu| { if gpu { anyhow::bail!("device lost"); } Ok(false) }).unwrap();
        let b = Recovery::load(true, |gpu| { if gpu { anyhow::bail!("device lost"); } Ok(false) }).unwrap();
        assert!(gpu_status().is_some());
        a.retry_now();
        assert!(a.run(Ok, |gpu| Ok(*gpu)).unwrap());
        assert!(gpu_status().is_some()); // b still owns fallback.
        assert!(!b.is_gpu());
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
