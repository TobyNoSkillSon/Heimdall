//! Independent background decoder. It owns no live-listening state or queue.
use crate::{Recognizer, Transcript};
use anyhow::{ensure, Result};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Condvar, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
struct Request {
    samples: Vec<f32>,
    deadline: Instant,
    cancelled: Arc<AtomicBool>,
    reply: mpsc::SyncSender<Result<VerifiedTranscript, String>>,
    queued: Instant,
    generation: u64,
}
#[derive(Debug)]
pub struct VerifiedTranscript {
    pub transcript: Transcript,
    pub queue_ms: f64,
    pub decode_ms: f64,
}
#[derive(Default)]
struct State {
    enabled: bool,
    stopping: bool,
    ready: bool,
    resident: bool,
    generation: u64,
    failed: Option<String>,
    request: Option<Request>,
    busy: bool,
    active: Option<Arc<AtomicBool>>,
}
struct Shared {
    state: Mutex<State>,
    changed: Condvar,
}
#[derive(Clone)]
pub struct VerificationClient {
    shared: Arc<Shared>,
}
type Factory = Box<dyn FnMut() -> Result<Box<dyn Recognizer>> + Send>;
/// The slot owns the native thread handle, even after its controller is dropped.
/// Only is_finished() releases its memory reservation or permits another owner.
#[derive(Default)]
struct ThreadSlot(Mutex<Option<(std::sync::Weak<Shared>, JoinHandle<()>)>>);
impl ThreadSlot {
    fn retained_bytes(&self) -> u64 {
        let slot = self.0.lock().unwrap();
        match slot.as_ref() {
            Some((owner, thread))
                if !thread.is_finished()
                    && owner
                        .upgrade()
                        .is_none_or(|s| !s.state.lock().unwrap().enabled) =>
            {
                2_000_000_000
            }
            _ => 0,
        }
    }
}
pub struct VerificationWorker {
    shared: Arc<Shared>,
    factory: Arc<Mutex<Factory>>,
    slot: Arc<ThreadSlot>,
}
fn background() -> Result<()> {
    #[cfg(target_os = "macos")]
    ensure!(
        unsafe { libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_UTILITY, 0) }
            == 0,
        "verification utility QoS unavailable"
    );
    Ok(())
}
fn pool<T>(f: impl FnOnce() -> T) -> T {
    #[cfg(target_os = "macos")]
    {
        objc2::rc::autoreleasepool(|_| f())
    }
    #[cfg(not(target_os = "macos"))]
    {
        f()
    }
}
impl VerificationWorker {
    /// The factory runs on its private thread only while residency is enabled.
    pub fn spawn<R: Recognizer + 'static>(
        mut factory: impl FnMut() -> Result<R> + Send + 'static,
    ) -> Result<Arc<Self>> {
        // Production controllers share one process-wide slot. Tests explicitly
        // use isolated slots, except the test of competing controller ownership.
        #[cfg(not(test))]
        let slot = {
            static SLOT: std::sync::OnceLock<Arc<ThreadSlot>> = std::sync::OnceLock::new();
            SLOT.get_or_init(|| Arc::new(ThreadSlot::default())).clone()
        };
        #[cfg(test)]
        let slot = Arc::new(ThreadSlot::default());
        Self::with_slot(
            move || factory().map(|r| Box::new(r) as Box<dyn Recognizer>),
            slot,
        )
    }
    fn with_slot(
        factory: impl FnMut() -> Result<Box<dyn Recognizer>> + Send + 'static,
        slot: Arc<ThreadSlot>,
    ) -> Result<Arc<Self>> {
        Ok(Arc::new(Self {
            shared: Arc::new(Shared {
                state: Mutex::new(State::default()),
                changed: Condvar::new(),
            }),
            factory: Arc::new(Mutex::new(Box::new(factory))),
            slot,
        }))
    }
    /// Nonzero until an unloaded owner's native thread has actually exited.
    pub fn retained_bytes(&self) -> u64 {
        self.slot.retained_bytes()
    }
    pub fn configure(&self, enabled: bool) {
        let mut slot = self.slot.0.lock().unwrap();
        let mut state = self.shared.state.lock().unwrap();
        if !enabled {
            state.enabled = false;
            state.ready = false;
            state.generation += 1;
            if let Some(request) = state.request.take() {
                request.cancelled.store(true, Ordering::Release);
            }
            if let Some(active) = &state.active {
                active.store(true, Ordering::Release);
            }
            self.shared.changed.notify_all();
            return;
        }
        if state.enabled {
            return;
        }
        if slot
            .as_ref()
            .is_some_and(|(_, thread)| !thread.is_finished())
        {
            state.failed = Some("previous verification thread is still unloading".into());
            return;
        }
        if let Some((_, thread)) = slot.take() {
            let _ = thread.join();
        }
        state.enabled = true;
        state.stopping = false;
        state.failed = None;
        state.busy = false;
        state.generation += 1;
        let epoch = state.generation;
        state.resident = true;
        let owner = self.shared.clone();
        let factory = self.factory.clone();
        let spawned = thread::Builder::new()
            .name("heimdall-verify".into())
            .spawn(move || {
                let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    background()?;
                    let mut model = pool(|| (factory.lock().unwrap())())?;
                    {
                        let mut state = owner.state.lock().unwrap();
                        if !state.enabled || state.generation != epoch {
                            return Ok::<(), anyhow::Error>(());
                        }
                        state.ready = true;
                        owner.changed.notify_all();
                    }
                    loop {
                        let mut state = owner.state.lock().unwrap();
                        if !state.enabled || state.generation != epoch {
                            break;
                        }
                        let Some(request) = state.request.take() else {
                            drop(owner.changed.wait(state).unwrap());
                            continue;
                        };
                        state.active = Some(request.cancelled.clone());
                        drop(state);
                        let started = Instant::now();
                        let result = if request.cancelled.load(Ordering::Acquire)
                            || started >= request.deadline
                        {
                            Err("verification cancelled or expired".into())
                        } else {
                            pool(|| model.transcribe(&request.samples, 0))
                                .map(|transcript| VerifiedTranscript {
                                    transcript,
                                    queue_ms: started.duration_since(request.queued).as_secs_f64()
                                        * 1000.,
                                    decode_ms: started.elapsed().as_secs_f64() * 1000.,
                                })
                                .map_err(|e| format!("{e:#}"))
                        };
                        state = owner.state.lock().unwrap();
                        state.busy = false;
                        state.active = None;
                        let valid = state.enabled
                            && state.generation == request.generation
                            && !request.cancelled.load(Ordering::Acquire)
                            && Instant::now() < request.deadline;
                        drop(state);
                        let _ = request.reply.try_send(if valid {
                            result
                        } else {
                            Err("verification cancelled or expired".into())
                        });
                    }
                    pool(|| drop(model));
                    Ok(())
                }));
                let mut state = owner
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                state.ready = false;
                state.resident = false;
                state.busy = false;
                state.request.take();
                state.active = None;
                state.enabled = false;
                if let Ok(Err(error)) = run {
                    state.failed = Some(error.to_string());
                } else if run.is_err() {
                    state.failed = Some("verification worker crashed".into());
                }
                state.stopping = true;
                owner.changed.notify_all();
            });
        match spawned {
            Ok(thread) => *slot = Some((Arc::downgrade(&self.shared), thread)),
            Err(error) => {
                state.enabled = false;
                state.resident = false;
                state.failed = Some(error.to_string());
            }
        }
    }
    pub fn client(&self) -> VerificationClient {
        VerificationClient {
            shared: self.shared.clone(),
        }
    }
    pub fn wait_ready(&self, timeout: Duration) -> Result<()> {
        let state = self.shared.state.lock().unwrap();
        let (state, _) = self
            .shared
            .changed
            .wait_timeout_while(state, timeout, |s| {
                !s.ready && s.enabled && s.failed.is_none() && !s.stopping
            })
            .unwrap();
        ensure!(
            state.ready,
            "verification decoder unavailable: {:?}",
            state.failed
        );
        Ok(())
    }
    pub fn wait_unloaded(&self, timeout: Duration) -> bool {
        let started = Instant::now();
        loop {
            if self
                .slot
                .0
                .lock()
                .unwrap()
                .as_ref()
                .is_none_or(|(_, thread)| thread.is_finished())
            {
                return true;
            }
            if started.elapsed() >= timeout {
                return false;
            }
            let state = self.shared.state.lock().unwrap();
            drop(
                self.shared
                    .changed
                    .wait_timeout(
                        state,
                        Duration::from_millis(5).min(timeout.saturating_sub(started.elapsed())),
                    )
                    .unwrap(),
            );
        }
    }
}
impl Drop for VerificationWorker {
    fn drop(&mut self) {
        self.configure(false);
    }
}
impl VerificationClient {
    pub fn transcribe(
        &self,
        samples: Vec<f32>,
        timeout: Duration,
        cancel: &dyn Fn() -> bool,
    ) -> Result<VerifiedTranscript> {
        ensure!(
            !samples.is_empty()
                && samples.len() <= 240_000
                && samples.iter().all(|x| x.is_finite()),
            "verification audio must be finite mono 16 kHz and at most 15 seconds"
        );
        ensure!(!cancel(), "speech interrupted");
        let queued = Instant::now();
        let token = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::sync_channel(1);
        struct Cancel(Arc<AtomicBool>, Arc<Shared>);
        impl Drop for Cancel {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
                let mut state = self.1.state.lock().unwrap();
                if state
                    .request
                    .as_ref()
                    .is_some_and(|r| Arc::ptr_eq(&r.cancelled, &self.0))
                {
                    state.request.take();
                    state.busy = state.active.is_some();
                }
            }
        }
        let _cancel = Cancel(token.clone(), self.shared.clone());
        let generation = {
            let mut state = self.shared.state.lock().unwrap();
            ensure!(
                state.enabled && !state.stopping && state.failed.is_none(),
                "verification decoder unavailable"
            );
            ensure!(!state.busy, "verification queue full");
            state.busy = true;
            let generation = state.generation;
            state.request = Some(Request {
                samples,
                deadline: queued + timeout,
                cancelled: token,
                reply: tx,
                queued,
                generation,
            });
            self.shared.changed.notify_all();
            generation
        };
        loop {
            ensure!(!cancel(), "speech interrupted");
            ensure!(queued.elapsed() < timeout, "verification deadline");
            {
                let state = self.shared.state.lock().unwrap();
                ensure!(
                    state.enabled && state.generation == generation && !state.stopping,
                    "verification decoder unloaded"
                );
            }
            match rx.recv_timeout(Duration::from_millis(5)) {
                Ok(result) => {
                    ensure!(
                        !cancel() && queued.elapsed() < timeout,
                        "verification cancelled or expired"
                    );
                    return result.map_err(anyhow::Error::msg);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(_) => anyhow::bail!("verification worker stopped"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        AudioChunk, ListenEngine, ListenEvent, ListenWorker, SpeechDetector, TurnDetector,
        WorkerEvent,
    };
    struct Silent;
    impl SpeechDetector for Silent {
        fn probability(&mut self, _: &[f32; 512], _: &mut crate::onnx::VadState) -> Result<f32> {
            Ok(1.)
        }
    }
    impl TurnDetector for Silent {
        fn probability(&mut self, _: &[f32]) -> Result<f32> {
            Ok(1.)
        }
    }
    struct Live;
    impl Recognizer for Live {
        fn transcribe(&mut self, samples: &[f32], ns: u64) -> Result<Transcript> {
            Ok(Transcript::from_words(vec![crate::Word {
                text: "heard".into(),
                start_ns: ns,
                end_ns: ns + samples.len() as u64 * 62500,
                confidence: 1.,
            }]))
        }
    }
    struct Blocked {
        at: mpsc::Sender<()>,
        gate: mpsc::Receiver<()>,
    }
    impl Recognizer for Blocked {
        fn transcribe(&mut self, _: &[f32], _: u64) -> Result<Transcript> {
            self.at.send(()).unwrap();
            self.gate.recv().unwrap();
            Ok(Transcript {
                text: "verified".into(),
                words: vec![],
            })
        }
    }
    #[test]
    fn mic_pcm_and_barge_in_are_processed_while_verification_is_still_blocked() {
        let (at, reached) = mpsc::channel();
        let (go, wait) = mpsc::channel();
        let mut gate = Some(wait);
        let verification = VerificationWorker::spawn(move || {
            Ok(Blocked {
                at: at.clone(),
                gate: gate.take().unwrap(),
            })
        })
        .unwrap();
        verification.configure(true);
        verification.wait_ready(Duration::from_secs(2)).unwrap();
        let client = verification.client();
        let check = thread::spawn(move || {
            client.transcribe(vec![0.1; 16000], Duration::from_millis(750), &|| false)
        });
        reached.recv_timeout(Duration::from_secs(2)).unwrap();
        let mut live = ListenWorker::spawn(Default::default(), || {
            ListenEngine::new(Live, Silent, Silent, Default::default())
        })
        .unwrap();
        live.wait_ready().unwrap();
        // The decoder cannot complete before go. This is the review reproduction,
        // rather than a timing comparison against a naturally short inference.
        live.push(AudioChunk {
            source: "mic",
            generation: 1,
            host_time_ns: 0,
            samples: &[0.1; 512],
        })
        .unwrap();
        live.flush_source("mic").unwrap();
        let (tx, rx) = mpsc::channel();
        let shared = Arc::new(live);
        let reader = shared.clone();
        let events = thread::spawn(move || {
            let mut speech = false;
            while let Some(event) = reader.recv_event() {
                match event {
                    WorkerEvent::Listen(ListenEvent::SpeechStart { .. }) => {
                        speech = true;
                        tx.send("barge-in").unwrap();
                    }
                    WorkerEvent::Listen(ListenEvent::FinalUtterance { .. }) => {
                        assert!(speech);
                        tx.send("transcript").unwrap();
                        return;
                    }
                    _ => {}
                }
            }
        });
        assert_eq!(rx.recv_timeout(Duration::from_secs(2)).unwrap(), "barge-in");
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            "transcript"
        );
        // The caller deadline fails; even a later valid decode is never accepted.
        assert!(check.join().unwrap().is_err());
        go.send(()).unwrap();
        verification.configure(false);
        assert!(verification.wait_unloaded(Duration::from_secs(2)));
        events.join().unwrap();
        live = Arc::try_unwrap(shared).ok().unwrap();
        live.shutdown();
    }
    #[test]
    fn a_request_that_expires_during_load_is_never_decoded() {
        use std::sync::atomic::AtomicUsize;
        struct Counted(Arc<AtomicUsize>);
        impl Recognizer for Counted {
            fn transcribe(&mut self, _: &[f32], _: u64) -> Result<Transcript> {
                self.0.fetch_add(1, Ordering::SeqCst);
                unreachable!("expired request reached decoder")
            }
        }
        let (at, reached) = mpsc::channel();
        let (go, wait) = mpsc::channel();
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let worker = VerificationWorker::spawn(move || {
            at.send(()).unwrap();
            wait.recv().unwrap();
            Ok(Counted(count.clone()))
        })
        .unwrap();
        worker.configure(true);
        reached.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(worker
            .client()
            .transcribe(vec![0.1; 512], Duration::from_millis(20), &|| false)
            .unwrap_err()
            .to_string()
            .contains("deadline"));
        go.send(()).unwrap();
        worker.wait_ready(Duration::from_secs(2)).unwrap();
        worker.configure(false);
        assert!(worker.wait_unloaded(Duration::from_secs(2)));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
    #[test]
    fn hung_unloaded_owner_keeps_reserve_and_prevents_a_second_thread_even_after_drop() {
        use std::sync::atomic::AtomicUsize;
        let slot = Arc::new(ThreadSlot::default());
        let (at, reached) = mpsc::channel();
        let (go, wait) = mpsc::channel();
        let mut gate = Some(wait);
        let first = VerificationWorker::with_slot(
            move || {
                Ok(Box::new(Blocked {
                    at: at.clone(),
                    gate: gate.take().unwrap(),
                }))
            },
            slot.clone(),
        )
        .unwrap();
        first.configure(true);
        first.wait_ready(Duration::from_secs(2)).unwrap();
        let client = first.client();
        let request = thread::spawn(move || {
            client.transcribe(vec![0.1; 512], Duration::from_secs(2), &|| false)
        });
        reached.recv_timeout(Duration::from_secs(2)).unwrap();
        first.configure(false);
        assert_eq!(first.retained_bytes(), 2_000_000_000);
        assert!(!first.wait_unloaded(Duration::from_millis(10)));
        assert!(request.join().unwrap().is_err());
        drop(first); // Dropping the controller must not surrender the reservation.
        let loads = Arc::new(AtomicUsize::new(0));
        let count = loads.clone();
        let second = VerificationWorker::with_slot(
            move || {
                count.fetch_add(1, Ordering::SeqCst);
                Ok(Box::new(Live))
            },
            slot,
        )
        .unwrap();
        second.configure(true);
        assert!(second.wait_ready(Duration::from_millis(10)).is_err());
        assert!(second
            .client()
            .transcribe(vec![0.1; 512], Duration::from_millis(10), &|| false)
            .is_err());
        assert_eq!(second.retained_bytes(), 2_000_000_000);
        assert_eq!(loads.load(Ordering::SeqCst), 0);
        go.send(()).unwrap();
        assert!(second.wait_unloaded(Duration::from_secs(2)));
        assert_eq!(second.retained_bytes(), 0);
        second.configure(true);
        second.wait_ready(Duration::from_secs(2)).unwrap();
        assert_eq!(loads.load(Ordering::SeqCst), 1);
        second.configure(false);
        assert!(second.wait_unloaded(Duration::from_secs(2)));
    }
    #[test]
    fn residency_does_not_load_while_off_and_drops_before_reloading() {
        use std::sync::atomic::AtomicUsize;
        struct Counted(Arc<AtomicUsize>);
        impl Drop for Counted {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::SeqCst);
            }
        }
        impl Recognizer for Counted {
            fn transcribe(&mut self, _: &[f32], _: u64) -> Result<Transcript> {
                Ok(Transcript {
                    text: "okay".into(),
                    words: vec![],
                })
            }
        }
        let count = Arc::new(AtomicUsize::new(0));
        let current = count.clone();
        let worker = VerificationWorker::spawn(move || {
            assert_eq!(current.fetch_add(1, Ordering::SeqCst), 0);
            Ok(Counted(current.clone()))
        })
        .unwrap();
        assert!(worker.wait_unloaded(Duration::from_millis(10)));
        assert_eq!(count.load(Ordering::SeqCst), 0);
        for _ in 0..2 {
            worker.configure(true);
            worker.wait_ready(Duration::from_secs(2)).unwrap();
            assert_eq!(count.load(Ordering::SeqCst), 1);
            worker.configure(false);
            assert!(worker.wait_unloaded(Duration::from_secs(2)));
            assert_eq!(count.load(Ordering::SeqCst), 0);
        }
    }
    #[test]
    fn cancellation_unload_and_full_queue_fail_without_waiting_for_decode() {
        let (at, reached) = mpsc::channel();
        let (go, wait) = mpsc::channel();
        let mut gate = Some(wait);
        let worker = VerificationWorker::spawn(move || {
            Ok(Blocked {
                at: at.clone(),
                gate: gate.take().unwrap(),
            })
        })
        .unwrap();
        worker.configure(true);
        worker.wait_ready(Duration::from_secs(2)).unwrap();
        let client = worker.client();
        let cancelled = Arc::new(AtomicBool::new(false));
        let flag = cancelled.clone();
        let checking = thread::spawn(move || {
            client.transcribe(vec![0.1; 512], Duration::from_secs(2), &|| {
                flag.load(Ordering::Acquire)
            })
        });
        reached.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(worker
            .client()
            .transcribe(vec![0.1; 512], Duration::from_secs(1), &|| false)
            .unwrap_err()
            .to_string()
            .contains("queue full"));
        cancelled.store(true, Ordering::Release);
        assert!(checking.join().unwrap().is_err());
        worker.configure(false);
        assert!(worker
            .client()
            .transcribe(vec![0.1; 512], Duration::from_secs(1), &|| false)
            .is_err());
        go.send(()).unwrap();
        assert!(worker.wait_unloaded(Duration::from_secs(2)));
    }
}
