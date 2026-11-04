//! Live intake: bounded per-source PCM and event queues, one inference owner.
//! No inference or channel send waits on the capture caller. Dropped audio and
//! undelivered transcript events are explicit gaps, never silent concatenation.
use crate::{
    AudioChunk, ListenEngine, ListenEvent, PartialPolicy, Recognizer, SpeechDetector, TurnDetector,
};
use anyhow::{ensure, Result};
use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError},
    thread::{self, JoinHandle},
};
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum GapReason {
    InputOverflow,
    OutputOverflow,
}
#[derive(Debug, Clone)]
pub enum WorkerEvent {
    /// The models loaded; from here on queued audio is being transcribed. Sent once, first.
    Ready,
    ModelsReloaded { error: Option<String> },
    SourceClosed {
        source: String,
    },
    Listen(ListenEvent),
    Gap {
        source: String,
        start_ns: u64,
        end_ns: u64,
        reason: GapReason,
    },
    Error {
        source: Option<String>,
        message: String,
    },
    /// A side transcript requested with `transcribe_side`. Never part of any source's stream.
    SideTranscript {
        id: u64,
        result: Result<crate::Transcript, String>,
        decode_ms: f64,
        /// Process CPU delta during this decode; may include concurrent host work.
        cpu_seconds: f64,
    },
}
#[derive(Debug, Clone, Copy)]
pub struct QueueConfig {
    pub maximum_sources: usize,
    pub chunks_per_source: usize,
    pub maximum_chunk_samples: usize,
    pub events_per_source: usize,
}
impl Default for QueueConfig {
    fn default() -> Self {
        Self {
            maximum_sources: 32,
            chunks_per_source: 16,
            maximum_chunk_samples: 1600,
            events_per_source: 32,
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushOutcome {
    Queued,
    DroppedOldest { start_ns: u64, end_ns: u64 },
}
struct Chunk {
    generation: u64,
    discontinuity: bool,
    ns: u64,
    samples: Vec<f32>,
}
#[derive(Default)]
struct Slot {
    audio: VecDeque<Chunk>,
    input_gap: Option<(u64, u64)>,
    events: VecDeque<WorkerEvent>,
    flush: bool,
    closing: bool,
    closed: bool,
    policy: Option<PartialPolicy>,
    discard: bool,
    output_gap: Option<(u64, u64)>,
    last_ns: u64,
}
fn process_cpu_seconds()->f64 {
    let mut usage=unsafe {std::mem::zeroed::<libc::rusage>()};
    unsafe {libc::getrusage(libc::RUSAGE_SELF,&mut usage)};
    usage.ru_utime.tv_sec as f64+usage.ru_utime.tv_usec as f64/1e6+usage.ru_stime.tv_sec as f64+usage.ru_stime.tv_usec as f64/1e6
}
struct SideRequest {
    id: u64,
    start_ns: u64,
    samples: Vec<f32>,
    offset: usize,
    words: Vec<crate::Word>,
    decode_ms: f64,
    cpu_seconds: f64,
}
// Half-second ownership with 250 ms context per side: at most one second
// reaches the recognizer before returning to command/live scheduling.
pub const CORRECTION_SLICE_SAMPLES: usize = 8000;
const CORRECTION_CONTEXT_SAMPLES: usize = 4000;
/// Side transcripts waiting at most; spoken commands are rare, so a full queue means something is stuck.
const MAX_SIDE_REQUESTS: usize = 8;
/// Most audio one external request may carry: ten minutes at 16 kHz (docs/vella.md).
pub const EXTERNAL_MAX_SAMPLES: usize = 9_600_000;
/// External requests held at once: one decoding and one waiting.
pub const EXTERNAL_SLOTS: usize = 2;
/// A finished external request: another app's audio decoded on this model, never part of any source's stream.
#[derive(Debug, Clone)]
pub struct ExternalTranscript {
    pub transcript: crate::Transcript,
    /// Admission to the first window's decode.
    pub queue_ms: f64,
    /// First window's decode to the result, including the live turns interleaved between windows.
    pub decode_ms: f64,
    /// Model time alone, summed over windows.
    pub model_ms: f64,
    pub windows: usize,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExternalRefusal {
    /// One request decodes and one waits already.
    Busy,
    /// The models are not loaded (startup, failure) or the worker is stopping.
    Unavailable,
    /// Too long or non-finite audio.
    Invalid(String),
}
pub type ExternalReply = std::sync::mpsc::Receiver<Result<ExternalTranscript, String>>;
struct ExternalJob {
    long: crate::long::LongTranscription,
    reply: std::sync::mpsc::Sender<Result<ExternalTranscript, String>>,
    admitted: std::time::Instant,
    started: Option<std::time::Instant>,
    model: std::time::Duration,
}
/// Live work queued for the inference owner: what a live batch would take next.
fn live_pending(slot: &Slot) -> bool {
    slot.discard || slot.policy.is_some() || slot.input_gap.is_some() || !slot.audio.is_empty() || slot.flush
}
/// Live batches it takes to drain what is queued now (one chunk per source per batch).
fn live_backlog(s: &State) -> usize {
    s.slots.values().map(|slot| slot.audio.len()
        + usize::from(slot.flush || slot.discard || slot.policy.is_some() || slot.input_gap.is_some())).max().unwrap_or(0)
}
fn refuse_external(s: &mut State, reason: &str) {
    for job in s.external.drain(..) {
        let _ = job.reply.send(Err(reason.into()));
    }
}
struct State {
    /// External (Vella) requests; the front one is decoding, one window per turn.
    external: VecDeque<ExternalJob>,
    /// The front external request is out of the queue for its current window.
    external_running: bool,
    side: VecDeque<SideRequest>,
    correction: VecDeque<SideRequest>,
    correction_started: Option<std::time::Instant>,
    reload: bool,
    controls: VecDeque<WorkerEvent>,
    slots: HashMap<String, Slot>,
    order: VecDeque<String>,
    stopping: bool,
    finished: bool,
    ready: bool,
    fatal: Option<String>,
    /// Units of work finished (a source's chunk batch, a side decode, a reload): the hang check's progress.
    done: u64,
}
/// What the worker has finished and what waits for it. A hang is work waiting while `done` stands still; a quiet
/// worker has nothing waiting.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Progress {
    pub done: u64,
    pub waiting: usize,
    pub ready: bool,
    pub correction_started: Option<std::time::Instant>,
    pub finished: bool,
}
struct Shared {
    state: Mutex<State>,
    signal: Condvar,
    event_signal: Condvar,
    config: QueueConfig,
}
impl Shared {
    /// The state stays readable after a panic on any thread: the end marker and the reason still reach readers.
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}
pub struct ListenWorker {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}
impl ListenWorker {
    /// Factory executes on the inference thread, so Core ML objects never cross
    /// threads and need no unsafe Send implementation. Startup queues are bounded.
    pub fn spawn<R, V, T, F>(config: QueueConfig, factory: F) -> Result<Self>
    where
        R: Recognizer + 'static, V: SpeechDetector + 'static, T: TurnDetector + 'static,
        F: FnOnce() -> Result<ListenEngine<R,V,T>> + Send + 'static,
    {
        let mut factory = Some(factory);
        Self::spawn_reloadable(config, move || {
            factory.take().ok_or_else(|| anyhow::anyhow!("worker has no reload factory"))?()
        })
    }
    pub fn spawn_reloadable<R, V, T, F>(config: QueueConfig, factory: F) -> Result<Self>
    where
        R: Recognizer + 'static,
        V: SpeechDetector + 'static,
        T: TurnDetector + 'static,
        F: FnMut() -> Result<ListenEngine<R, V, T>> + Send + 'static,
    {
        let factory=Arc::new(Mutex::new(factory));
        let initial=factory.clone();
        Self::spawn_configurable(config,move ||initial.lock().unwrap()(),move |engine|{
            let next=factory.lock().unwrap()()?;
            engine.replace_models(next);Ok(())
        })
    }
    pub fn spawn_configurable<R,V,T,F,G>(config:QueueConfig, factory:F, mut reconfigure:G)->Result<Self>
    where
        R:Recognizer+'static,V:SpeechDetector+'static,T:TurnDetector+'static,
        F:FnOnce()->Result<ListenEngine<R,V,T>>+Send+'static,
        G:FnMut(&mut ListenEngine<R,V,T>)->Result<()>+Send+'static,
    {
        ensure!(
            config.maximum_sources > 0
                && config.maximum_sources <= 32
                && config.chunks_per_source > 0
                && config.events_per_source >= 2
                && config.maximum_chunk_samples > 0
                && config.maximum_chunk_samples <= 16000,
            "Invalid queue bounds"
        );
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                external: VecDeque::new(),
                external_running: false,
                side: VecDeque::new(),
                correction: VecDeque::new(),
                correction_started: None,
                reload: false,
                controls: VecDeque::new(),
                slots: HashMap::new(),
                order: VecDeque::new(),
                stopping: false,
                finished: false,
                ready: false,
                fatal: None,
                done: 0,
            }),
            signal: Condvar::new(),
            event_signal: Condvar::new(),
            config,
        });
        let owner = shared.clone();
        let thread = thread::Builder::new()
            .name("heimdall-listen".into())
            .spawn(move || {
                // A panic anywhere in the model code or the loop below ends the worker like any other fatal
                // error: readers get the reason, then the end marker, instead of waiting forever on a dead thread.
                let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let mut engine = match factory() {
                    Ok(e) => e,
                    Err(e) => {
                        owner.lock().fatal = Some(e.to_string());
                        return;
                    }
                };
                {
                    let mut s = owner.lock();
                    s.ready = true;
                    s.controls.push_front(WorkerEvent::Ready);
                    owner.event_signal.notify_all();
                }
                // Live batches owed before the next external window: the backlog queued while the last window
                // decoded. Live therefore waits at most one window, and an external request at most one drain.
                let mut live_owed = 0usize;
                loop {
                    let reload = {
                        let mut s = owner.lock();
                        std::mem::take(&mut s.reload) && !s.stopping
                    };
                    if reload {
                        let error = reconfigure(&mut engine).err().map(|e|format!("{e:#}"));
                        let mut s = owner.lock();
                        // Only the newest reload result matters; an unread Ready stays.
                        s.controls.retain(|e| !matches!(e, WorkerEvent::ModelsReloaded { .. }));
                        s.controls.push_back(WorkerEvent::ModelsReloaded { error });
                        s.done += 1;
                        owner.event_signal.notify_all();
                    }
                    // Side transcripts go first: a spoken command waits on one. Each is one decode on
                    // this same model, between source chunks; no source state is read or changed.
                    let side = {
                        let mut s = owner.lock();
                        // Commands win; corrections run only between fully drained live batches.
                        if let Some(command) = s.side.pop_front() { Some((command, false)) }
                        else if !s.stopping && !s.reload && s.external.is_empty() && s.slots.values().all(|slot|
                            slot.audio.is_empty() && slot.input_gap.is_none() && !slot.flush && !slot.discard && slot.policy.is_none()) {
                            s.correction.pop_front().map(|r| {
                                // Keep execution visible atomically with removing queued ownership.
                                s.correction_started = Some(std::time::Instant::now());
                                (r, true)
                            })
                        } else { None }
                    };
                    if let Some((mut r, correction)) = side {
                        let started = std::time::Instant::now();
                        let cpu_started = process_cpu_seconds();
                        let end = if correction { (r.offset + CORRECTION_SLICE_SAMPLES).min(r.samples.len()) } else { r.samples.len() };
                        let left = if correction { r.offset.saturating_sub(CORRECTION_CONTEXT_SAMPLES) } else { 0 };
                        let right = if correction { (end + CORRECTION_CONTEXT_SAMPLES).min(r.samples.len()) } else { end };
                        let result = engine.transcribe_side(&r.samples[left..right], r.start_ns + left as u64 * 62500)
                            .map_err(|e| format!("{e:#}"));
                        r.decode_ms += started.elapsed().as_secs_f64() * 1000.;
                        r.cpu_seconds += process_cpu_seconds() - cpu_started;
                        let mut s = owner.lock();
                        s.correction_started = None;
                        if correction {
                            if let Ok(transcript) = &result {
                                let a = r.start_ns + r.offset as u64 * 62500;
                                let b = r.start_ns + end as u64 * 62500;
                                r.words.extend(transcript.words.iter().filter(|w| {
                                    let mid = w.start_ns + w.end_ns.saturating_sub(w.start_ns) / 2;
                                    mid >= a && mid < b
                                }).cloned());
                            }
                        }
                        if correction && result.is_ok() && end < r.samples.len() && !s.stopping {
                            r.offset = end;
                            // The next loop checks commands and live intake before this continuation.
                            s.correction.push_front(r);
                        } else {
                            let result = if correction { result.map(|_| crate::Transcript::from_words(r.words)) } else { result };
                            s.controls.push_back(WorkerEvent::SideTranscript { id: r.id, result, decode_ms: r.decode_ms, cpu_seconds: r.cpu_seconds });
                            owner.event_signal.notify_all();
                        }
                        s.done += 1;
                        continue;
                    }
                    // One external window per turn, alternating with live drains (docs/vella.md, Starvation). No source
                    // state is read or changed; no VAD, turn detection or evidence hook sees this audio.
                    let external = {
                        let mut s = owner.lock();
                        if s.external.is_empty() { None }
                        else if s.stopping { refuse_external(&mut s, "listen worker stopping"); None }
                        else if live_owed == 0 || !s.slots.values().any(live_pending) {
                            s.external_running = true;
                            s.external.pop_front()
                        } else { None }
                    };
                    if let Some(mut job) = external {
                        let started = std::time::Instant::now();
                        job.started.get_or_insert(started);
                        let result = {
                            let (audio, ns) = job.long.window();
                            engine.transcribe_side(audio, ns)
                        };
                        job.model += started.elapsed();
                        let mut s = owner.lock();
                        s.external_running = false;
                        s.done += 1;
                        match result {
                            Err(e) => { let _ = job.reply.send(Err(format!("{e:#}"))); }
                            Ok(transcript) => {
                                job.long.apply(transcript);
                                if job.long.is_done() {
                                    let first = job.started.unwrap_or(started);
                                    let windows = job.long.windows();
                                    let done = ExternalTranscript {
                                        queue_ms: first.duration_since(job.admitted).as_secs_f64() * 1000.,
                                        decode_ms: first.elapsed().as_secs_f64() * 1000.,
                                        model_ms: job.model.as_secs_f64() * 1000.,
                                        windows,
                                        transcript: job.long.finish(),
                                    };
                                    let _ = job.reply.send(Ok(done));
                                } else {
                                    s.external.push_front(job);
                                }
                            }
                        }
                        live_owed = live_backlog(&s);
                        continue;
                    }
                    let work = {
                        let mut s = owner.lock();
                        loop {
                            if s.reload && !s.stopping { break Some(vec![]); }
                            if !s.side.is_empty() { break Some(vec![]); }
                            let mut found = vec![];
                            for _ in 0..s.order.len() {
                                let id = s.order.pop_front().unwrap();
                                s.order.push_back(id.clone());
                                let slot = s.slots.get_mut(&id).unwrap();
                                if slot.discard
                                    || slot.policy.is_some()
                                    || slot.input_gap.is_some()
                                    || !slot.audio.is_empty()
                                    || slot.flush
                                {
                                    let discard = std::mem::take(&mut slot.discard);
                                    let policy = slot.policy.take();
                                    let gap = slot.input_gap.take();
                                    let chunk = slot.audio.pop_front();
                                    let flush = slot.flush && slot.audio.is_empty();
                                    if flush {
                                        slot.flush = false;
                                    }
                                    found.push((id, discard, policy, gap, chunk, flush));
                                }
                            }
                            if !found.is_empty() {
                                break Some(found);
                            }
                            if !s.external.is_empty() { break Some(vec![]); }
                            if !s.correction.is_empty() && !s.stopping { break Some(vec![]); }
                            if s.stopping {
                                s.correction.clear();
                                break None;
                            }
                            s = owner.signal.wait(s).unwrap_or_else(PoisonError::into_inner);
                        }
                    };
                    let Some(works) = work else {
                        break;
                    };
                    if !works.is_empty() {
                        live_owed = live_owed.saturating_sub(1);
                    }
                    let mut batched_events = vec![vec![]; works.len()];
                    for (i, (source, discard, policy, gap, chunk, _)) in works.iter().enumerate() {
                        if *discard {
                            engine.discard_source(source);
                        }
                        if let Some(policy) = policy {
                            if let Err(e) = engine.set_source_policy(source, *policy) {
                                batched_events[i].push(WorkerEvent::Error {
                                    source: Some(source.clone()),
                                    message: e.to_string(),
                                });
                            }
                        }
                        if let Some((start_ns, end_ns)) = gap {
                            match engine.flush_source(source) {
                                Ok(es) => batched_events[i].extend(es.into_iter().map(WorkerEvent::Listen)),
                                Err(e) => batched_events[i].push(WorkerEvent::Error { source: Some(source.clone()), message: e.to_string() }),
                            }
                            batched_events[i].push(WorkerEvent::Gap {
                                source: source.clone(),
                                start_ns: *start_ns,
                                end_ns: *end_ns,
                                reason: GapReason::InputOverflow,
                            });
                        }
                        if let Some(c) = chunk.as_ref().filter(|c| c.discontinuity) {
                            match engine.discontinue_source(source, c.ns) {
                                Ok(es) => batched_events[i].extend(es.into_iter().map(WorkerEvent::Listen)),
                                Err(e) => batched_events[i].push(WorkerEvent::Error { source: Some(source.clone()), message: e.to_string() }),
                            }
                        }
                    }
                    let positions: Vec<usize> = works
                        .iter()
                        .enumerate()
                        .filter_map(|(i, w)| w.4.as_ref().map(|_| i))
                        .collect();
                    let chunks: Vec<_> = positions
                        .iter()
                        .map(|i| {
                            let (source, _, _, _, chunk, _) = &works[*i];
                            let c = chunk.as_ref().unwrap();
                            AudioChunk {
                                source,
                                generation: c.generation,
                                host_time_ns: c.ns,
                                samples: &c.samples,
                            }
                        })
                        .collect();
                    for (i, result) in positions.into_iter().zip(engine.push_batch(&chunks)) {
                        match result {
                            Ok(es) => {
                                batched_events[i].extend(es.into_iter().map(WorkerEvent::Listen))
                            }
                            Err(e) => batched_events[i].push(WorkerEvent::Error {
                                source: Some(works[i].0.clone()),
                                message: e.to_string(),
                            }),
                        }
                    }
                    for ((source, _, _, _, chunk, flush), mut events) in
                        works.into_iter().zip(batched_events)
                    {
                        let processed_ns = chunk
                            .as_ref()
                            .map(|c| c.ns + c.samples.len() as u64 * 62500);
                        if flush {
                            match engine.remove_source(&source) {
                                Ok(es) => events.extend(es.into_iter().map(WorkerEvent::Listen)),
                                Err(e) => events.push(WorkerEvent::Error {
                                    source: Some(source.clone()),
                                    message: e.to_string(),
                                }),
                            }
                        }
                        let mut s = owner.lock();
                        s.done += 1;
                        // Only a closed slot whose events were all read is retired, and a closed slot gets no work.
                        let Some(slot) = s.slots.get_mut(&source) else { continue };
                        if let Some(ns) = processed_ns {
                            slot.last_ns = ns;
                        }
                        if flush {
                            events.push(WorkerEvent::SourceClosed {
                                source: source.clone(),
                            });
                            slot.closed = true;
                        }
                        for event in events {
                            // Replace only consecutive partials; segment/final ordering
                            // cannot be coalesced without losing transcript words.
                            if matches!(
                                event,
                                WorkerEvent::Listen(ListenEvent::PartialTranscript { .. })
                            ) && matches!(
                                slot.events.back(),
                                Some(WorkerEvent::Listen(ListenEvent::PartialTranscript { .. }))
                            ) {
                                slot.events.pop_back();
                            }
                            if slot.events.len() >= owner.config.events_per_source {
                                let start = slot
                                    .events
                                    .iter()
                                    .filter_map(event_start)
                                    .min()
                                    .unwrap_or(slot.last_ns);
                                let end = slot.last_ns;
                                slot.events.clear();
                                slot.output_gap = Some(
                                    slot.output_gap
                                        .map_or((start, end), |(a, b)| (a.min(start), b.max(end))),
                                );
                                // We lost earlier deliveries; abandon their acoustic
                                // tail so a subsequent final cannot claim completeness.
                                engine.discard_source(&source);
                            }
                            slot.events.push_back(event);
                        }
                        owner.event_signal.notify_all();
                    }
                }
                }));
                let mut s = owner.lock();
                refuse_external(&mut s, "listen worker ended");
                if let Err(panic) = run {
                    let reason = panic.downcast_ref::<&str>().map(|m| m.to_string())
                        .or_else(|| panic.downcast_ref::<String>().cloned()).unwrap_or_else(|| "unknown panic".into());
                    s.fatal = Some(format!("listen worker crashed: {reason}"));
                }
                s.finished = true;
                owner.event_signal.notify_all();
            })?;
        Ok(Self {
            shared,
            thread: Some(thread),
        })
    }
    fn slot<'a>(&self, s: &'a mut State, source: &str) -> Result<&'a mut Slot> {
        ensure!(source.len() <= 256, "Source ID exceeds 256 bytes");
        ensure!(
            !s.stopping && !s.finished && s.fatal.is_none(),
            "Listen worker stopped or failed"
        );
        if !s.slots.contains_key(source) {
            ensure!(
                s.slots.len() < self.shared.config.maximum_sources,
                "Source limit"
            );
            s.slots.insert(source.into(), Slot::default());
            s.order.push_back(source.into());
        }
        let slot = s.slots.get_mut(source).unwrap();
        ensure!(
            !slot.closing && !slot.closed,
            "Source is closing; drain its events before reuse"
        );
        Ok(slot)
    }
    pub fn push(&self, chunk: AudioChunk<'_>) -> Result<PushOutcome> {
        self.push_with_discontinuity(chunk, false)
    }
    /// A capture boundary travels with its PCM, after every earlier queued chunk
    /// and transcript. It never clears a valid final waiting for delivery.
    pub fn push_with_discontinuity(&self, chunk: AudioChunk<'_>, discontinuity: bool) -> Result<PushOutcome> {
        ensure!(
            chunk.samples.len() <= self.shared.config.maximum_chunk_samples
                && chunk.samples.iter().all(|x| x.is_finite()),
            "Invalid/oversize audio chunk"
        );
        let mut s = self.shared.lock();
        let slot = self.slot(&mut s, chunk.source)?;
        let mut outcome = PushOutcome::Queued;
        if slot.audio.len() >= self.shared.config.chunks_per_source {
            let old = slot.audio.pop_front().unwrap();
            let end = old.ns + old.samples.len() as u64 * 62500;
            slot.input_gap = Some(
                slot.input_gap
                    .map_or((old.ns, end), |(a, b)| (a.min(old.ns), b.max(end))),
            );
            outcome = PushOutcome::DroppedOldest {
                start_ns: old.ns,
                end_ns: end,
            };
        }
        slot.audio.push_back(Chunk {
            generation: chunk.generation,
            discontinuity,
            ns: chunk.host_time_ns,
            samples: chunk.samples.to_vec(),
        });
        self.shared.signal.notify_one();
        Ok(outcome)
    }
    /// Queue a one-off decode of `samples` (16 kHz, at most 15 s); the result arrives as
    /// `WorkerEvent::SideTranscript { id, .. }`. Never waits on inference.
    /// Bounded background lane on the SAME recognizer. Intake and command requests win.
    pub fn transcribe_correction(&self, id: u64, start_ns: u64, samples: Vec<f32>) -> Result<()> {
        ensure!(!samples.is_empty() && samples.len() <= 128000 && samples.iter().all(|v| v.is_finite()), "invalid correction PCM");
        let mut s = self.shared.lock();
        ensure!(!s.stopping && !s.finished && s.fatal.is_none(), "listen worker stopped");
        ensure!(s.correction.len() < 2, "correction lane full");
        s.correction.push_back(SideRequest { id, start_ns, samples, offset: 0, words: vec![], decode_ms: 0., cpu_seconds: 0. });
        self.shared.signal.notify_one();
        Ok(())
    }
    pub fn transcribe_side(&self, id: u64, start_ns: u64, samples: Vec<f32>) -> Result<()> {
        ensure!(!samples.is_empty() && samples.len() <= 240_000, "side transcript audio must be 1..240000 samples");
        let mut s = self.shared.lock();
        ensure!(!s.stopping && !s.finished && s.fatal.is_none(), "Listen worker stopped or failed");
        ensure!(s.side.len() < MAX_SIDE_REQUESTS, "side transcript queue full");
        s.side.push_back(SideRequest { id, start_ns, samples, offset: 0, words: vec![], decode_ms: 0., cpu_seconds: 0. });
        self.shared.signal.notify_one();
        Ok(())
    }
    /// Queue another app's finished audio (16 kHz mono, at most ten minutes) for a whole-request decode on this same
    /// model. It is cut into the live path's 15-s windows, decoded one window per scheduling turn between live
    /// drains, and never touches a source's stream. Never waits on inference; the reply arrives on the receiver.
    pub fn transcribe_external(&self, samples: Vec<f32>) -> std::result::Result<ExternalReply, ExternalRefusal> {
        if samples.len() > EXTERNAL_MAX_SAMPLES {
            return Err(ExternalRefusal::Invalid(format!("at most {EXTERNAL_MAX_SAMPLES} samples")));
        }
        if !samples.iter().all(|v| v.is_finite()) {
            return Err(ExternalRefusal::Invalid("non-finite audio".into()));
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let mut s = self.shared.lock();
        if !s.ready || s.stopping || s.finished || s.fatal.is_some() {
            return Err(ExternalRefusal::Unavailable);
        }
        if s.external.len() + usize::from(s.external_running) >= EXTERNAL_SLOTS {
            return Err(ExternalRefusal::Busy);
        }
        let admitted = std::time::Instant::now();
        if samples.is_empty() {
            let _ = tx.send(Ok(ExternalTranscript { transcript: crate::Transcript::from_words(vec![]), queue_ms: 0., decode_ms: 0., model_ms: 0., windows: 0 }));
            return Ok(rx);
        }
        s.external.push_back(ExternalJob {
            long: crate::long::LongTranscription::new(samples, 0),
            reply: tx,
            admitted,
            started: None,
            model: std::time::Duration::ZERO,
        });
        self.shared.signal.notify_one();
        Ok(rx)
    }
    /// Whether `transcribe_external` would be admitted now, as far as the model goes: loaded and not stopping.
    pub fn external_available(&self) -> bool {
        let s = self.shared.lock();
        s.ready && !s.stopping && !s.finished && s.fatal.is_none()
    }
    /// Both external slots are taken.
    pub fn external_full(&self) -> bool {
        let s = self.shared.lock();
        s.external.len() + usize::from(s.external_running) >= EXTERNAL_SLOTS
    }
    pub fn set_source_policy(&self, source: &str, policy: PartialPolicy) -> Result<()> {
        ensure!(
            !matches!(policy,PartialPolicy::Slow{interval_samples} if interval_samples<512),
            "Invalid cadence"
        );
        let mut s = self.shared.lock();
        self.slot(&mut s, source)?.policy = Some(policy);
        self.shared.signal.notify_one();
        Ok(())
    }
    /// Abandon inference state after a disabled source or capture discontinuity.
    /// Retain lifecycle metadata; future audio starts a fresh utterance.
    pub fn discard_source(&self, source: &str) -> Result<()> {
        let mut state = self.shared.lock();
        let slot = self.slot(&mut state, source)?;
        slot.audio.clear();
        slot.events.clear();
        slot.input_gap = None;
        slot.output_gap = None;
        slot.discard = true;
        self.shared.signal.notify_one();
        Ok(())
    }
    pub fn flush_source(&self, source: &str) -> Result<()> {
        let mut s = self.shared.lock();
        let slot = self.slot(&mut s, source)?;
        slot.flush = true;
        slot.closing = true;
        self.shared.signal.notify_one();
        Ok(())
    }
    /// Nonblocking, round-robin event drain. Gaps precede retained events.
    pub fn pop_event(&self) -> Option<WorkerEvent> {
        let mut s = self.shared.lock();
        if let Some(event) = s.controls.pop_front() { return Some(event); }
        if let Some(message) = s.fatal.take() {
            return Some(WorkerEvent::Error {
                source: None,
                message,
            });
        }
        for _ in 0..s.order.len() {
            let id = s.order.pop_front().unwrap();
            s.order.push_back(id.clone());
            let slot = s.slots.get_mut(&id).unwrap();
            let event = slot
                .output_gap
                .take()
                .map(|(start_ns, end_ns)| WorkerEvent::Gap {
                    source: id.clone(),
                    start_ns,
                    end_ns,
                    reason: GapReason::OutputOverflow,
                })
                .or_else(|| slot.events.pop_front());
            if slot.closed && slot.events.is_empty() && slot.output_gap.is_none() {
                s.slots.remove(&id);
                s.order.retain(|x| x != &id);
            }
            if event.is_some() {
                return event;
            }
        }
        None
    }
    /// Block on the existing output signal, without polling. A closed worker
    /// returns None only after retained events have been drained.
    pub fn recv_event(&self) -> Option<WorkerEvent> {
        loop {
            if let Some(event) = self.pop_event() {
                return Some(event);
            }
            let mut state = self.shared.lock();
            if state.finished {
                return None;
            }
            while !state.finished
                && state.controls.is_empty()
                && state.fatal.is_none()
                && state
                    .slots
                    .values()
                    .all(|slot| slot.events.is_empty() && slot.output_gap.is_none())
            {
                state = self.shared.event_signal.wait(state).unwrap_or_else(PoisonError::into_inner);
            }
        }
    }
    /// Preparation barrier for offline real-time replay; live admission need not wait.
    pub fn wait_ready(&self) -> Result<()> {
        let mut state = self.shared.lock();
        while !state.ready && !state.finished {
            state = self.shared.event_signal.wait(state).unwrap_or_else(PoisonError::into_inner);
        }
        ensure!(
            state.ready,
            "listen initialization failed: {}",
            state.fatal.as_deref().unwrap_or("worker stopped")
        );
        Ok(())
    }
    pub fn is_ready(&self) -> bool {
        self.shared.lock().ready
    }
    pub fn is_finished(&self) -> bool {
        self.shared.lock().finished
    }
    /// Finished and waiting work, for the app's hang check. Never waits on inference (the lock is not held there).
    pub fn progress(&self) -> Progress {
        let s = self.shared.lock();
        let waiting = s.side.len() + s.correction.len() + s.external.len() + usize::from(s.external_running)
            + usize::from(s.correction_started.is_some())
            + usize::from(s.reload)
            + s.slots.values().map(|slot| slot.audio.len() + usize::from(slot.flush || slot.discard || slot.policy.is_some())).sum::<usize>();
        Progress { done: s.done, waiting, ready: s.ready, finished: s.finished, correction_started: s.correction_started }
    }
    pub fn reload_models(&self) -> Result<()> {
        let mut s = self.shared.lock();
        ensure!(!s.stopping && !s.finished, "listen worker stopped");
        s.reload = true;
        self.shared.signal.notify_one();
        Ok(())
    }
    /// Close every source and keep the models: each open source is flushed once (its final follows) and closed.
    /// Returns every source still open or closing; each ends with `SourceClosed`. Listening off with the worker
    /// kept loaded, so the next Listening on needs no model load.
    pub fn close_all_sources(&self) -> Vec<String> {
        let mut s = self.shared.lock();
        let mut open = vec![];
        for (id, slot) in s.slots.iter_mut() {
            if slot.closed { continue; }
            if !slot.closing {
                slot.flush = true;
                slot.closing = true;
            }
            open.push(id.clone());
        }
        self.shared.signal.notify_all();
        open
    }
    /// Signal shutdown without waiting; callers can keep draining output.
    pub fn request_shutdown(&self) {
        stop_admission(&mut self.shared.lock());
        self.shared.signal.notify_all();
    }
    /// Stop admission, process retained chunks, then flush. Final events remain
    /// available to pop_event. Shutdown alone may wait for current inference.
    pub fn shutdown(&mut self) {
        {
            stop_admission(&mut self.shared.lock());
            self.shared.signal.notify_all();
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
/// Every source not already closing gets its one flush. A source whose flush is under way is closing but not yet
/// closed; flushing it again let the reader retire its slot between the two flushes (13 Oct 11:16, see
/// `shutdown_during_a_source_flush_ends_the_worker_cleanly`).
fn stop_admission(s: &mut State) {
    s.stopping = true;
    for slot in s.slots.values_mut() {
        if !slot.closing && !slot.closed {
            slot.flush = true;
            slot.closing = true;
        }
    }
}
impl Drop for ListenWorker {
    fn drop(&mut self) {
        self.shutdown();
    }
}
fn event_start(e: &WorkerEvent) -> Option<u64> {
    match e {
        WorkerEvent::Listen(
            ListenEvent::SpeechStart { host_time_ns, .. }
            | ListenEvent::SpeechEnd { host_time_ns, .. }
            | ListenEvent::Discontinuity { host_time_ns, .. },
        ) => Some(*host_time_ns),
        WorkerEvent::Listen(
            ListenEvent::FinalUtterance { start_ns, .. }
            | ListenEvent::TranscriptSegment { start_ns, .. },
        ) => Some(*start_ns),
        WorkerEvent::Listen(ListenEvent::PartialTranscript { words, .. }) => {
            words.first().map(|w| w.start_ns)
        }
        WorkerEvent::Gap { start_ns, .. } => Some(*start_ns),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{onnx::VadState, ListenConfig, Transcript, Word};
    use std::sync::mpsc;
    struct Vad;
    impl SpeechDetector for Vad {
        fn probability(&mut self, _: &[f32; 512], _: &mut VadState) -> Result<f32> {
            Ok(1.0)
        }
    }
    struct Turn;
    impl TurnDetector for Turn {
        fn probability(&mut self, _: &[f32]) -> Result<f32> {
            Ok(0.1)
        }
    }
    struct Asr;
    impl Recognizer for Asr {
        fn transcribe(&mut self, a: &[f32], ns: u64) -> Result<Transcript> {
            Ok(Transcript::from_words(vec![Word {
                text: "hello".into(),
                start_ns: ns,
                end_ns: ns + a.len() as u64 * 62500,
                confidence: 1.0,
            }]))
        }
    }
    fn engine() -> Result<ListenEngine<Asr, Vad, Turn>> {
        ListenEngine::new(
            Asr,
            Vad,
            Turn,
            ListenConfig {
                partial_interval_samples: 512,
                first_partial_samples: 512,
                maximum_partial_interval_samples: 512,
                ..ListenConfig::default()
            },
        )
    }
    /// What the inference owner did, in order: `V` an external window, `L` a live VAD block.
    #[derive(Clone, Default)]
    struct Order(Arc<Mutex<Vec<char>>>);
    impl Order {
        fn log(&self) -> String { self.0.lock().unwrap().iter().collect() }
    }
    struct SilentVad(Order);
    impl SpeechDetector for SilentVad {
        fn probability(&mut self, _: &[f32; 512], _: &mut VadState) -> Result<f32> {
            self.0 .0.lock().unwrap().push('L');
            Ok(0.)
        }
    }
    /// Every recognizer call here is an external window (the live source stays silent); each takes `decode`.
    struct WindowAsr(Order, std::time::Duration);
    impl Recognizer for WindowAsr {
        fn transcribe(&mut self, a: &[f32], ns: u64) -> Result<Transcript> {
            assert!(a.len() <= crate::long::WINDOW_SAMPLES);
            self.0 .0.lock().unwrap().push('V');
            thread::sleep(self.1);
            Asr.transcribe(a, ns)
        }
    }
    #[test]
    fn external_windows_alternate_with_live_drains() {
        let order = Order::default();
        let (vad, asr) = (SilentVad(order.clone()), WindowAsr(order.clone(), std::time::Duration::ZERO));
        let (go, wait) = mpsc::channel::<()>();
        let worker = ListenWorker::spawn(QueueConfig::default(), move || {
            wait.recv().unwrap();
            ListenEngine::new(asr, vad, Turn, ListenConfig::default())
        }).unwrap();
        assert_eq!(worker.transcribe_external(vec![0.1; 16_000]).err(), Some(ExternalRefusal::Unavailable), "models still loading");
        // Eight 512-sample chunks queued live, then a three-window request.
        for n in 0..8u64 {
            worker.push(AudioChunk { source: "mic", generation: 0, host_time_ns: n * 32_000_000, samples: &[0.; 512] }).unwrap();
        }
        go.send(()).unwrap();
        worker.wait_ready().unwrap();
        let reply = worker.transcribe_external(vec![0.1; 500_000]).unwrap();
        let done = reply.recv_timeout(std::time::Duration::from_secs(5)).unwrap().unwrap();
        assert_eq!(done.windows, 3);
        let log = order.log();
        // The live backlog drains before the first window (it was queued first) or within one turn after it; every
        // later window has at most one live drain before it.
        let first_v = log.find('V').unwrap();
        assert!(log[first_v..].matches('L').count() <= 8, "{log}");
        assert_eq!(log.matches('L').count(), 8, "{log}");
        assert_eq!(log.matches('V').count(), 3, "{log}");
    }
    #[test]
    fn ten_minute_external_request_never_overflows_a_live_source() {
        use std::time::{Duration, Instant};
        // Scaled time: a window decodes in 10 ms while a 100-ms live chunk arrives every 7 ms, so 1.4 chunks arrive per
        // window. Decoding the whole request at once (47 windows) would queue 67 chunks; alternating one window with
        // one live chunk would gain 0.4 a window, 19 over the request. Both overflow the 16-chunk queue.
        let order = Order::default();
        let (vad, asr) = (SilentVad(order.clone()), WindowAsr(order.clone(), Duration::from_millis(10)));
        let worker = Arc::new(ListenWorker::spawn(QueueConfig::default(), move || {
            ListenEngine::new(asr, vad, Turn, ListenConfig::default())
        }).unwrap());
        worker.wait_ready().unwrap();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (live, halt) = (worker.clone(), stop.clone());
        let pusher = thread::spawn(move || {
            let (mut dropped, mut pushed, mut most_waiting) = (0, 0u64, 0);
            let start = Instant::now();
            while !halt.load(std::sync::atomic::Ordering::Acquire) {
                most_waiting = most_waiting.max(live.progress().waiting);
                if live.push(AudioChunk { source: "mic", generation: 0, host_time_ns: pushed * 100_000_000, samples: &[0.; 1600] }).unwrap() != PushOutcome::Queued {
                    dropped += 1;
                }
                pushed += 1;
                let next = start + Duration::from_millis(7 * pushed);
                thread::sleep(next.saturating_duration_since(Instant::now()));
            }
            (dropped, pushed, most_waiting)
        });
        thread::sleep(Duration::from_millis(30));
        let at = Instant::now();
        let reply = worker.transcribe_external(vec![0.1; EXTERNAL_MAX_SAMPLES]).unwrap();
        let done = reply.recv_timeout(Duration::from_secs(20)).unwrap().unwrap();
        let wall = at.elapsed();
        thread::sleep(Duration::from_millis(30));
        stop.store(true, std::sync::atomic::Ordering::Release);
        let (dropped, pushed, most_waiting) = pusher.join().unwrap();
        assert_eq!(done.windows, 47);
        assert_eq!(dropped, 0, "live input overflowed while the request decoded");
        let mut gaps = 0;
        while let Some(e) = worker.pop_event() { if matches!(e, WorkerEvent::Gap { .. }) { gaps += 1; } }
        assert_eq!(gaps, 0);
        // Live kept moving between windows: every pair of consecutive windows has a live drain between them.
        let log = order.log();
        let inner = &log[log.find('V').unwrap()..=log.rfind('V').unwrap()];
        let parts: Vec<_> = inner.split('V').skip(1).collect();
        let starved = parts[..parts.len() - 1].iter().filter(|p| p.is_empty()).count();
        assert!(starved <= 2, "windows ran back to back {starved} times: {inner}");
        eprintln!("10-min request: {} windows in {:.0} ms wall (model {:.0} ms), {pushed} live chunks, most waiting {most_waiting}",
            done.windows, wall.as_secs_f64() * 1000., done.model_ms);
        // One decoding request and at most the chunks of about one window (plus scheduling slack) wait.
        assert!(most_waiting <= 1 + 8, "most waiting {most_waiting}");
    }
    #[test]
    fn external_admission_refuses_busy_too_long_and_stopped() {
        use std::time::Duration;
        struct Held(mpsc::Sender<()>, mpsc::Receiver<()>);
        impl Recognizer for Held {
            fn transcribe(&mut self, a: &[f32], ns: u64) -> Result<Transcript> {
                let _ = self.0.send(());
                self.1.recv_timeout(Duration::from_secs(5)).unwrap();
                Asr.transcribe(a, ns)
            }
        }
        let (entered, inside) = mpsc::channel();
        let (release, held) = mpsc::channel();
        let mut worker = ListenWorker::spawn(QueueConfig::default(), move || ListenEngine::new(Held(entered, held), Vad, Turn, ListenConfig::default())).unwrap();
        worker.wait_ready().unwrap();
        assert!(matches!(worker.transcribe_external(vec![0.; EXTERNAL_MAX_SAMPLES + 1]), Err(ExternalRefusal::Invalid(_))));
        assert!(matches!(worker.transcribe_external(vec![f32::NAN; 10]), Err(ExternalRefusal::Invalid(_))));
        let empty = worker.transcribe_external(vec![]).unwrap().recv().unwrap().unwrap();
        assert_eq!((empty.transcript.text.as_str(), empty.windows), ("", 0));
        let first = worker.transcribe_external(vec![0.1; 16_000]).unwrap();
        inside.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(!worker.external_full());
        let second = worker.transcribe_external(vec![0.1; 16_000]).unwrap();
        assert!(worker.external_full());
        assert_eq!(worker.transcribe_external(vec![0.1; 16_000]).err(), Some(ExternalRefusal::Busy), "one decoding, one waiting");
        assert_eq!(worker.progress().waiting, 2, "the hang check sees both");
        release.send(()).unwrap();
        assert_eq!(first.recv_timeout(Duration::from_secs(5)).unwrap().unwrap().transcript.text, "hello");
        inside.recv_timeout(Duration::from_secs(5)).unwrap();
        release.send(()).unwrap();
        assert!(second.recv_timeout(Duration::from_secs(5)).unwrap().is_ok());
        worker.shutdown();
        assert!(!worker.external_available());
        assert_eq!(worker.transcribe_external(vec![0.1; 16]).err(), Some(ExternalRefusal::Unavailable));
    }
    #[test]
    fn a_queued_external_request_is_refused_at_shutdown_not_left_waiting() {
        use std::time::Duration;
        struct Slow;
        impl Recognizer for Slow {
            fn transcribe(&mut self, a: &[f32], ns: u64) -> Result<Transcript> { thread::sleep(Duration::from_millis(20)); Asr.transcribe(a, ns) }
        }
        let mut worker = ListenWorker::spawn(QueueConfig::default(), || ListenEngine::new(Slow, Vad, Turn, ListenConfig::default())).unwrap();
        worker.wait_ready().unwrap();
        let reply = worker.transcribe_external(vec![0.1; EXTERNAL_MAX_SAMPLES]).unwrap();
        thread::sleep(Duration::from_millis(30));
        worker.shutdown();
        assert!(reply.recv_timeout(Duration::from_secs(2)).unwrap().is_err());
    }
    #[test]
    fn correction_lane_yields_to_command_and_live_final() {
        struct OrderAsr(Arc<Mutex<Vec<u64>>>);
        impl Recognizer for OrderAsr {
            fn transcribe(&mut self,a:&[f32],ns:u64)->Result<Transcript> {self.0.lock().unwrap().push(ns);Asr.transcribe(a,ns)}
        }
        let order=Arc::new(Mutex::new(vec![]));let log=order.clone();
        let (go,wait)=mpsc::channel();
        let worker=ListenWorker::spawn(QueueConfig {chunks_per_source:64,events_per_source:64,..Default::default()},move || {
            wait.recv().unwrap();ListenEngine::new(OrderAsr(log),Vad,Turn,ListenConfig::default())
        }).unwrap();
        worker.transcribe_correction(88,88_000_000_000,vec![1.;3200]).unwrap();
        for n in 0..32 {worker.push(AudioChunk {source:"mic",generation:0,host_time_ns:n*32_000_000,samples:&[1.;512]}).unwrap();}
        worker.flush_source("mic").unwrap();
        worker.transcribe_side(99,99_000_000_000,vec![1.;3200]).unwrap();
        go.send(()).unwrap();
        let mut got=vec![];
        while !got.contains(&88) {if let Some(WorkerEvent::SideTranscript {id,..})=worker.recv_event(){got.push(id);}}
        assert_eq!(got,[99,88]);
        let log=order.lock().unwrap();
        assert_eq!(log[0],99_000_000_000);assert_eq!(*log.last().unwrap(),88_000_000_000);
        assert!(log[1..log.len()-1].contains(&0),"live final precedes correction: {log:?}");
    }
    #[test]
    fn executing_correction_remains_outstanding_without_capture() {
        use std::time::Duration;
        struct Blocking(mpsc::Sender<()>, mpsc::Receiver<()>);
        impl Recognizer for Blocking {
            fn transcribe(&mut self, a: &[f32], ns: u64) -> Result<Transcript> {
                self.0.send(()).unwrap();
                self.1.recv_timeout(Duration::from_secs(2)).unwrap();
                Asr.transcribe(a, ns)
            }
        }
        let (entered, inside) = mpsc::channel();
        let (release, blocked) = mpsc::channel();
        let worker = ListenWorker::spawn(QueueConfig::default(), move || {
            ListenEngine::new(Blocking(entered, blocked), Vad, Turn, ListenConfig::default())
        }).unwrap();
        worker.wait_ready().unwrap();
        worker.transcribe_correction(1, 0, vec![1.; 3200]).unwrap();
        inside.recv_timeout(Duration::from_secs(2)).unwrap();
        let progress = worker.progress();
        // Release before asserting so a regression cannot leave a blocked decoder.
        release.send(()).unwrap();
        assert!(progress.correction_started.is_some());
        assert_eq!(progress.waiting, 1, "executing decode must remain visible to the hang check");
        assert_eq!(progress.done, 0);
        while !matches!(worker.recv_event(), Some(WorkerEvent::SideTranscript { id: 1, .. })) {}
        assert_eq!(worker.progress().waiting, 0);
    }
    #[test]
    fn command_arriving_inside_blocking_correction_precedes_remaining_slices() {
        use std::time::{Duration, Instant};
        struct Blocking { entered: mpsc::Sender<()>, release: mpsc::Receiver<()>, calls: Arc<Mutex<Vec<usize>>> }
        impl Recognizer for Blocking {
            fn transcribe(&mut self, a: &[f32], ns: u64) -> Result<Transcript> {
                if ns < 10_000_000_000 {
                    self.calls.lock().unwrap().push(a.len());
                    if ns == 0 {
                        self.entered.send(()).unwrap();
                        self.release.recv_timeout(Duration::from_secs(2)).unwrap();
                        // A genuinely blocking decoder consumes the rest of its bounded slice.
                        thread::sleep(Duration::from_millis(60));
                    }
                }
                Asr.transcribe(a,ns)
            }
        }
        let (entered, inside) = mpsc::channel();
        let (release, blocked) = mpsc::channel();
        let calls = Arc::new(Mutex::new(vec![])); let seen = calls.clone();
        let worker = ListenWorker::spawn(QueueConfig::default(), move || ListenEngine::new(Blocking {entered,release:blocked,calls:seen},Vad,Turn,ListenConfig::default())).unwrap();
        worker.wait_ready().unwrap();
        worker.transcribe_correction(1,0,vec![1.;128000]).unwrap();
        inside.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(worker.progress().correction_started.is_some());
        let at = Instant::now();
        worker.transcribe_side(2,20_000_000_000,vec![1.;3200]).unwrap();
        release.send(()).unwrap();
        loop {
            if let Some(WorkerEvent::SideTranscript {id,..}) = worker.recv_event() {
                assert_eq!(id,2,"command must arrive before the eight-second correction completes"); break;
            }
        }
        let wait=at.elapsed();
        eprintln!("command queued AFTER correction began: {:.3} ms",wait.as_secs_f64()*1000.);
        assert!(wait < Duration::from_millis(150),"{wait:?}");
        while !matches!(worker.recv_event(),Some(WorkerEvent::SideTranscript {id:1,result:Ok(_),..})) {}
        let calls=calls.lock().unwrap();
        assert_eq!(calls.len(),16);
        assert!(calls.iter().all(|n| *n <= CORRECTION_SLICE_SAMPLES + 2*CORRECTION_CONTEXT_SAMPLES));
    }
    #[test]
    fn capture_boundary_finishes_queued_pcm_before_gap_and_keeps_other_sources() {
        for boundary_ns in [128_000_000, 1_000_000_000] {
            let (go, wait) = mpsc::channel();
            let mut worker = ListenWorker::spawn(QueueConfig { chunks_per_source: 64, events_per_source: 64, ..Default::default() }, move || {
                wait.recv().unwrap(); engine()
            }).unwrap();
            for i in 0..4u64 {
                for source in ["mic", "discord"] {
                    worker.push(AudioChunk { source, generation: 1, host_time_ns: i * 32_000_000, samples: &[1.;512] }).unwrap();
                }
            }
            worker.push_with_discontinuity(AudioChunk { source: "mic", generation: 1, host_time_ns: boundary_ns, samples: &[1.;512] }, true).unwrap();
            worker.push(AudioChunk { source: "discord", generation: 1, host_time_ns: 128_000_000, samples: &[1.;512] }).unwrap();
            go.send(()).unwrap(); worker.shutdown();
            let events: Vec<_> = std::iter::from_fn(|| worker.pop_event()).collect();
            let finals: Vec<_> = events.iter().filter_map(|e| match e {
                WorkerEvent::Listen(ListenEvent::FinalUtterance { source, start_ns, end_ns, .. }) => Some((source.as_str(),*start_ns,*end_ns)), _ => None,
            }).collect();
            assert_eq!(finals.iter().filter(|f| f.0 == "mic").count(), 2);
            assert!(finals.contains(&("mic",0,128_000_000))); assert!(finals.contains(&("mic",boundary_ns,boundary_ns+32_000_000)));
            assert!(finals.contains(&("discord",0,160_000_000)));
            let old_final = events.iter().position(|e| matches!(e,WorkerEvent::Listen(ListenEvent::FinalUtterance { source,end_ns:128_000_000,.. }) if source=="mic")).unwrap();
            let boundary = events.iter().position(|e| matches!(e,WorkerEvent::Listen(ListenEvent::Discontinuity { source,host_time_ns }) if source=="mic" && *host_time_ns==boundary_ns)).unwrap();
            assert!(old_final < boundary);
            assert!(!events.iter().any(|e| matches!(e,WorkerEvent::Gap {..}|WorkerEvent::Error {..})));
        }
    }
    #[test]
    fn input_overflow_keeps_pre_loss_speech_before_the_gap() {
        struct BlockOnce(Option<(mpsc::Sender<()>,mpsc::Receiver<()>)>);
        impl Recognizer for BlockOnce {
            fn transcribe(&mut self,a:&[f32],ns:u64)->Result<Transcript> {
                if let Some((reached,go))=self.0.take() { reached.send(()).unwrap();go.recv().unwrap(); }
                Asr.transcribe(a,ns)
            }
        }
        let (reached,at_decode)=mpsc::channel();let (go,wait)=mpsc::channel();
        let mut worker=ListenWorker::spawn(QueueConfig {chunks_per_source:2,events_per_source:64,..Default::default()},move || {
            ListenEngine::new(BlockOnce(Some((reached,wait))),Vad,Turn,ListenConfig::default())
        }).unwrap();
        worker.wait_ready().unwrap();
        worker.push(AudioChunk {source:"mic",generation:1,host_time_ns:0,samples:&[1.;512]}).unwrap();
        while !matches!(worker.recv_event(),Some(WorkerEvent::Listen(ListenEvent::SpeechStart {..}))) {}
        worker.transcribe_side(1,0,vec![1.;512]).unwrap();
        at_decode.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        for tick in 1..5u64 { worker.push(AudioChunk {source:"mic",generation:1,host_time_ns:tick*32_000_000,samples:&[1.;512]}).unwrap(); }
        go.send(()).unwrap();worker.shutdown();
        let events:Vec<_>=std::iter::from_fn(||worker.pop_event()).collect();
        let first=events.iter().position(|e|matches!(e,WorkerEvent::Listen(ListenEvent::FinalUtterance {start_ns:0,end_ns:32_000_000,..}))).unwrap();
        let gap=events.iter().position(|e|matches!(e,WorkerEvent::Gap {start_ns:32_000_000,end_ns:96_000_000,reason:GapReason::InputOverflow,..})).unwrap();
        let second=events.iter().position(|e|matches!(e,WorkerEvent::Listen(ListenEvent::FinalUtterance {start_ns:96_000_000,end_ns:160_000_000,..}))).unwrap();
        assert!(first<gap && gap<second);
    }
    #[test]
    fn blocking_output_drains_final_and_closure_without_polling() {
        let worker = Arc::new(ListenWorker::spawn(QueueConfig::default(), engine).unwrap());
        worker.wait_ready().unwrap();
        worker
            .push(AudioChunk {
                source: "mic",
                generation: 0,
                host_time_ns: 0,
                samples: &[1.; 512],
            })
            .unwrap();
        worker.flush_source("mic").unwrap();
        let output = worker.clone();
        let (tx, rx) = mpsc::channel();
        let reader = thread::spawn(move || {
            let mut final_seen = false;
            while let Some(event) = output.recv_event() {
                match event {
                    WorkerEvent::Listen(ListenEvent::FinalUtterance { .. }) => final_seen = true,
                    WorkerEvent::SourceClosed { .. } => {
                        tx.send(final_seen).unwrap();
                    }
                    _ => {}
                }
            }
        });
        assert!(rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap());
        worker.request_shutdown();
        reader.join().unwrap();
    }
    #[test]
    fn side_transcript_is_its_own_event_and_leaves_the_source_stream_whole() {
        let worker = ListenWorker::spawn(QueueConfig::default(), engine).unwrap();
        worker.wait_ready().unwrap();
        for i in 0..4u64 {
            worker.push(AudioChunk { source: "mic", generation: 0, host_time_ns: i * 32_000_000, samples: &[1.; 512] }).unwrap();
        }
        worker.transcribe_side(7, 5_000_000_000, vec![0.5; 3200]).unwrap();
        assert!(worker.transcribe_side(8, 0, vec![]).is_err(), "empty side audio is refused");
        assert!(worker.transcribe_side(8, 0, vec![0.; 240_001]).is_err(), "over one 15-s window is refused");
        worker.flush_source("mic").unwrap();
        let (mut side, mut source_words) = (vec![], vec![]);
        while let Some(event) = worker.recv_event() {
            match event {
                WorkerEvent::SideTranscript { id, result, .. } => side.push((id, result.unwrap().words[0].start_ns)),
                WorkerEvent::Listen(ListenEvent::FinalUtterance { words, .. }) => source_words.extend(words),
                WorkerEvent::SourceClosed { .. } => break,
                _ => {}
            }
        }
        assert_eq!(side, [(7, 5_000_000_000)]);
        // The source's own final starts at its own audio, untouched by the side decode.
        assert!(!source_words.is_empty() && source_words.iter().all(|w| w.start_ns < 1_000_000_000));
    }
    #[test]
    fn discarded_queued_audio_never_becomes_a_final() {
        let (go, wait) = mpsc::channel();
        let worker = ListenWorker::spawn(QueueConfig::default(), move || {
            wait.recv().unwrap();
            engine()
        })
        .unwrap();
        worker
            .push(AudioChunk {
                source: "mic",
                generation: 0,
                host_time_ns: 0,
                samples: &[1.; 512],
            })
            .unwrap();
        worker.discard_source("mic").unwrap();
        worker.flush_source("mic").unwrap();
        go.send(()).unwrap();
        while let Some(event) = worker.recv_event() {
            assert!(!matches!(
                event,
                WorkerEvent::Listen(ListenEvent::FinalUtterance { .. })
            ));
            if matches!(event, WorkerEvent::SourceClosed { .. }) {
                worker.request_shutdown();
            }
        }
    }
    #[test]
    fn stalled_inference_does_not_block_push_and_reports_exact_overflow() {
        let (go, wait) = mpsc::channel();
        let mut worker = ListenWorker::spawn(
            QueueConfig {
                chunks_per_source: 2,
                ..QueueConfig::default()
            },
            move || {
                wait.recv().unwrap();
                engine()
            },
        )
        .unwrap();
        let start = std::time::Instant::now();
        for i in 0..100 {
            let outcome = worker
                .push(AudioChunk {
                    source: "mic",
                    generation: 0,
                    host_time_ns: i * 32_000_000,
                    samples: &[1.0; 512],
                })
                .unwrap();
            assert_eq!(matches!(outcome, PushOutcome::Queued), i < 2);
        }
        assert!(
            start.elapsed().as_secs_f64() < 1.0,
            "intake waited on stalled initialization"
        );
        {
            let s = worker.shared.state.lock().unwrap();
            let slot = &s.slots["mic"];
            assert_eq!(slot.audio.len(), 2);
            assert_eq!(slot.input_gap, Some((0, 98 * 32_000_000)));
        }
        go.send(()).unwrap();
        worker.shutdown();
        let mut events = vec![];
        while let Some(e) = worker.pop_event() {
            events.push(e);
        }
        assert!(matches!(events.remove(0), WorkerEvent::Ready));
        assert!(
            matches!(&events[0],WorkerEvent::Gap{start_ns:0,end_ns,reason:GapReason::InputOverflow,..} if *end_ns==98*32_000_000)
        );
        assert!(events.iter().any(|e|matches!(e,WorkerEvent::Listen(ListenEvent::FinalUtterance{start_ns,..}) if *start_ns>=98*32_000_000)));
        assert!(worker.is_finished());
    }
    #[test]
    fn undrained_output_is_bounded_and_explicit() {
        let mut worker = ListenWorker::spawn(
            QueueConfig {
                events_per_source: 2,
                chunks_per_source: 64,
                ..QueueConfig::default()
            },
            engine,
        )
        .unwrap();
        // Lifecycle events + final exceed two even with partial coalescing.
        for source in ["mic:1", "mic:2", "mic:3"] {
            for i in 0..40 {
                worker
                    .push(AudioChunk {
                        source,
                        generation: i,
                        host_time_ns: i * 32_000_000,
                        samples: &[1.0; 512],
                    })
                    .unwrap();
            }
        }
        worker.shutdown();
        {
            let s = worker.shared.state.lock().unwrap();
            assert_eq!(s.slots.len(), 3);
            for slot in s.slots.values() {
                assert!(slot.events.len() <= 2);
                assert!(slot.audio.is_empty());
            }
        }
        let mut gaps = 0;
        while let Some(e) = worker.pop_event() {
            if matches!(
                e,
                WorkerEvent::Gap {
                    reason: GapReason::OutputOverflow,
                    ..
                }
            ) {
                gaps += 1;
            }
        }
        assert_eq!(gaps, 3);
        assert!(worker.shared.state.lock().unwrap().slots.is_empty());
    }
    #[test]
    fn five_minutes_three_sources_stalled_intake_has_constant_storage() {
        let (go, wait) = mpsc::channel();
        let mut worker = ListenWorker::spawn(QueueConfig::default(), move || {
            wait.recv().unwrap();
            engine()
        })
        .unwrap();
        for i in 0..9375 {
            for source in ["mic:owner", "process:discord", "process:browser"] {
                worker
                    .push(AudioChunk {
                        source,
                        generation: 0,
                        host_time_ns: i * 32_000_000,
                        samples: &[1.0; 512],
                    })
                    .unwrap();
            }
            let s = worker.shared.state.lock().unwrap();
            assert_eq!(s.slots.len(), 3);
            let queued: usize = s
                .slots
                .values()
                .map(|slot| slot.audio.iter().map(|c| c.samples.len()).sum::<usize>())
                .sum();
            assert!(queued <= 3 * 16 * 512);
        }
        {
            let s = worker.shared.state.lock().unwrap();
            for slot in s.slots.values() {
                assert_eq!(slot.audio.len(), 16);
                assert_eq!(slot.input_gap, Some((0, (9375 - 16) * 32_000_000)));
            }
        }
        go.send(()).unwrap();
        worker.shutdown();
        let mut gaps = 0;
        while let Some(event) = worker.pop_event() {
            if let WorkerEvent::Gap {
                reason: GapReason::InputOverflow,
                start_ns: 0,
                end_ns,
                ..
            } = event
            {
                assert_eq!(end_ns, (9375 - 16) * 32_000_000);
                gaps += 1;
            }
        }
        assert_eq!(gaps, 3);
    }
    struct BlockAsr {
        started: Option<mpsc::Sender<()>>,
        resume: mpsc::Receiver<()>,
    }
    impl Recognizer for BlockAsr {
        fn transcribe(&mut self, a: &[f32], ns: u64) -> Result<Transcript> {
            if let Some(started) = self.started.take() {
                started.send(()).unwrap();
                self.resume.recv().unwrap();
            }
            Asr.transcribe(a, ns)
        }
    }
    /// The app's hang check: a worker stuck in inference shows work waiting and no work finishing; once it moves,
    /// `done` grows and the queue empties. A quiet worker has nothing waiting.
    #[test]
    fn progress_tells_a_stuck_worker_from_a_quiet_one() {
        let (started, signal) = mpsc::channel();
        let (resume, wait) = mpsc::channel();
        let worker = ListenWorker::spawn(QueueConfig::default(), move || {
            ListenEngine::new(
                BlockAsr { started: Some(started), resume: wait },
                Vad,
                Turn,
                ListenConfig {
                    source_policies: HashMap::from([("mic".into(), PartialPolicy::Responsive)]),
                    partial_interval_samples: 512,
                    first_partial_samples: 512,
                    maximum_partial_interval_samples: 512,
                    ..ListenConfig::default()
                },
            )
        })
        .unwrap();
        worker.wait_ready().unwrap();
        let quiet = worker.progress();
        assert_eq!((quiet.waiting, quiet.ready, quiet.finished), (0, true, false));
        let push = |i: u64| worker.push(AudioChunk { source: "mic", generation: 0, host_time_ns: i * 32_000_000, samples: &[1.0; 512] }).unwrap();
        push(0);
        signal.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
        for i in 1..5 {
            push(i);
        }
        let stuck = worker.progress();
        std::thread::sleep(std::time::Duration::from_millis(50));
        let still = worker.progress();
        assert!(stuck.waiting >= 4 && still.done == stuck.done, "{stuck:?} {still:?}");
        resume.send(()).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while worker.progress().waiting > 0 {
            assert!(std::time::Instant::now() < deadline, "the worker never drained");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(worker.progress().done > still.done);
    }
    #[test]
    fn blocked_asr_allows_intake_and_flush_closes_admission() {
        let (started, signal) = mpsc::channel();
        let (resume, wait) = mpsc::channel();
        let mut worker = ListenWorker::spawn(QueueConfig::default(), move || {
            ListenEngine::new(
                BlockAsr {
                    started: Some(started),
                    resume: wait,
                },
                Vad,
                Turn,
                ListenConfig {
                    source_policies: HashMap::from([("mic".into(), PartialPolicy::Responsive)]),
                    partial_interval_samples: 512,
                    first_partial_samples: 512,
                    maximum_partial_interval_samples: 512,
                    ..ListenConfig::default()
                },
            )
        })
        .unwrap();
        worker
            .push(AudioChunk {
                source: "mic",
                generation: 0,
                host_time_ns: 0,
                samples: &[1.0; 512],
            })
            .unwrap();
        signal
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        let start = std::time::Instant::now();
        for i in 1..20 {
            worker
                .push(AudioChunk {
                    source: "mic",
                    generation: 0,
                    host_time_ns: i * 32_000_000,
                    samples: &[1.0; 512],
                })
                .unwrap();
        }
        assert!(start.elapsed().as_secs_f64() < 1.0, "intake waited on ASR");
        worker.flush_source("mic").unwrap();
        assert!(worker
            .push(AudioChunk {
                source: "mic",
                generation: 1,
                host_time_ns: 1_000_000_000,
                samples: &[1.0; 512]
            })
            .is_err());
        resume.send(()).unwrap();
        worker.shutdown();
        let mut finalized = false;
        while let Some(e) = worker.pop_event() {
            finalized |= matches!(e, WorkerEvent::Listen(ListenEvent::FinalUtterance { .. }));
        }
        assert!(finalized);
        assert!(worker.is_finished());
    }
    #[test]
    fn initialization_failure_is_visible_and_intake_rejected() {
        let mut worker = ListenWorker::spawn::<Asr, Vad, Turn, _>(QueueConfig::default(), || {
            anyhow::bail!("missing model")
        })
        .unwrap();
        worker.shutdown();
        assert!(
            matches!(worker.pop_event(),Some(WorkerEvent::Error{message,..}) if message=="missing model")
        );
        assert!(worker
            .push(AudioChunk {
                source: "mic",
                generation: 0,
                host_time_ns: 0,
                samples: &[]
            })
            .is_err());
    }
    #[test]
    fn reload_failure_retains_the_engine_and_retry_keeps_both_streams() {
        let attempt=Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter=attempt.clone();
        let mut worker=ListenWorker::spawn_reloadable(QueueConfig {chunks_per_source:512,..Default::default()},move || {
            if counter.fetch_add(1,std::sync::atomic::Ordering::SeqCst)==1 {anyhow::bail!("unavailable candidate");}
            ListenEngine::new(Asr,Vad,Turn,ListenConfig::default())
        }).unwrap();
        worker.wait_ready().unwrap();
        for tick in 0..30 {
            for source in ["mic", "app"] {worker.push(AudioChunk {source,generation:1,host_time_ns:tick*32_000_000,samples:&[1.;512]}).unwrap();}
        }
        worker.reload_models().unwrap();
        let deadline=std::time::Instant::now()+std::time::Duration::from_secs(3);
        let mut failure=false;
        while std::time::Instant::now()<deadline&&!failure {
            if let Some(WorkerEvent::ModelsReloaded {error})=worker.pop_event(){assert!(error.unwrap().contains("unavailable candidate"));failure=true;}
            std::thread::yield_now();
        }
        assert!(failure);
        worker.reload_models().unwrap();
        for tick in 30..60 {for source in ["mic", "app"] {worker.push(AudioChunk {source,generation:1,host_time_ns:tick*32_000_000,samples:&[1.;512]}).unwrap();}}
        let deadline=std::time::Instant::now()+std::time::Duration::from_secs(3);
        let mut success=false;
        while std::time::Instant::now()<deadline&&!success {
            if let Some(WorkerEvent::ModelsReloaded {error})=worker.pop_event(){assert!(error.is_none());success=true;}
            std::thread::yield_now();
        }
        assert!(success);
        worker.flush_source("mic").unwrap();worker.flush_source("app").unwrap();worker.request_shutdown();
        let mut finals=std::collections::HashSet::new();
        while let Some(event)=worker.recv_event(){match event {
            WorkerEvent::ModelsReloaded {error}=>{assert!(error.is_none());success=true;},
            WorkerEvent::Listen(ListenEvent::FinalUtterance {source,..})=>{finals.insert(source);},
            WorkerEvent::Gap {..}|WorkerEvent::Error {..}=>panic!("unexpected loss: {event:?}"),
            _=>{},
        }}
        worker.shutdown();assert!(success);assert_eq!(finals.len(),2);
    }
    /// While closed, ASR (0) and VAD (1) calls each wait for a permit; `waiting` says where the worker is parked.
    #[derive(Default)]
    struct GateState { closed: bool, permits: usize, waiting: [usize; 2] }
    #[derive(Clone, Default)]
    struct Gate(Arc<(Mutex<GateState>, Condvar)>);
    impl Gate {
        fn set_closed(&self, closed: bool) { self.0.0.lock().unwrap().closed = closed; self.0.1.notify_all(); }
        /// One permit to a call parked at `at`, never a spare one for the next call.
        fn release(&self, at: usize) {
            let mut g = self.0.0.lock().unwrap();
            if g.waiting[at] > 0 && g.permits == 0 { g.permits = 1; self.0.1.notify_all(); }
        }
        fn parked(&self, at: usize) -> bool { self.0.0.lock().unwrap().waiting[at] > 0 }
        fn wait_parked(&self, at: usize) {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while !self.parked(at) { assert!(std::time::Instant::now() < deadline, "worker never parked at {at}"); thread::yield_now(); }
        }
        fn pass(&self, at: usize) {
            let (lock, signal) = &*self.0;
            let mut g = lock.lock().unwrap();
            while g.closed && g.permits == 0 { g.waiting[at] += 1; g = signal.wait(g).unwrap(); g.waiting[at] -= 1; }
            if g.closed { g.permits -= 1; }
        }
    }
    struct GatedAsr(Gate);
    impl Recognizer for GatedAsr {
        fn transcribe(&mut self, a: &[f32], ns: u64) -> Result<Transcript> {
            self.0.pass(0);
            Asr.transcribe(a, ns)
        }
    }
    struct GatedVad(Gate);
    impl SpeechDetector for GatedVad {
        fn probability(&mut self, frame: &[f32; 512], state: &mut VadState) -> Result<f32> {
            self.0.pass(1);
            Vad.probability(frame, state)
        }
    }
    fn slot_state(worker: &ListenWorker, source: &str) -> Option<(bool, bool)> {
        worker.shared.state.lock().unwrap().slots.get(source).map(|s| (s.closing, s.closed))
    }
    #[test]
    fn shutdown_during_a_source_flush_ends_the_worker_cleanly() {
        // 13 Oct 11:16: Listening off removed Heimdall's own-audio source (a flush) and asked the worker to stop while
        // that flush was still decoding. Shutdown flushed the closing source a second time; the reader retired the
        // closed slot in between, the worker's slot lookup panicked, and the thread died without its end marker.
        let gate = Gate::default();
        let (asr, vad) = (gate.clone(), gate.clone());
        let worker = Arc::new(ListenWorker::spawn(QueueConfig::default(), move || {
            ListenEngine::new(GatedAsr(asr), GatedVad(vad), Turn, ListenConfig {
                partial_interval_samples: 512, first_partial_samples: 512, maximum_partial_interval_samples: 512,
                ..ListenConfig::default()
            })
        }).unwrap());
        worker.wait_ready().unwrap();
        for tick in 0..32u64 {
            for source in ["app", "mic"] {
                worker.push(AudioChunk { source, generation: 0, host_time_ns: tick * 32_000_000, samples: &[1.; 512] }).unwrap();
            }
        }
        let idle = |w: &ListenWorker| w.shared.state.lock().unwrap().slots.values().all(|s| s.audio.is_empty());
        while !idle(&worker) { thread::yield_now(); }
        std::thread::sleep(std::time::Duration::from_millis(50));
        gate.set_closed(true);
        // The capture drain removes the own-audio source: its final decode parks in ASR.
        worker.flush_source("app").unwrap();
        gate.wait_parked(0);
        worker.push(AudioChunk { source: "mic", generation: 0, host_time_ns: 32 * 32_000_000, samples: &[1.; 512] }).unwrap();
        // Listening off, while that flush is still running.
        worker.request_shutdown();
        // Let the app flush finish; the worker's next batch parks in the mic chunk's speech detection.
        while slot_state(&worker, "app") != Some((true, true)) { gate.release(0); thread::yield_now(); }
        gate.wait_parked(1);
        // The live loop's reader takes the closed source's last events, which retires its slot.
        let mut closed = false;
        while !closed {
            match worker.pop_event() {
                Some(WorkerEvent::SourceClosed { source }) => closed = source == "app",
                Some(_) => {}
                None => thread::yield_now(),
            }
        }
        assert_eq!(slot_state(&worker, "app"), None);
        gate.set_closed(false);
        let reader = worker.clone();
        let (tx, rx) = mpsc::channel();
        let drained = thread::spawn(move || {
            let mut events = vec![];
            while let Some(event) = reader.recv_event() { events.push(event); }
            tx.send(()).unwrap();
            events
        });
        rx.recv_timeout(std::time::Duration::from_secs(5)).expect("the worker never sent its end marker");
        let events = drained.join().expect("reader panicked on a poisoned worker");
        assert!(!events.iter().any(|e| matches!(e, WorkerEvent::Error { .. })), "{events:?}");
        assert!(events.iter().any(|e| matches!(e, WorkerEvent::SourceClosed { source } if source == "mic")));
        assert!(worker.is_finished());
    }
    struct PanicAsr;
    impl Recognizer for PanicAsr {
        fn transcribe(&mut self, _: &[f32], _: u64) -> Result<Transcript> { panic!("decoder bug") }
    }
    #[test]
    fn a_panic_in_the_models_ends_the_worker_with_its_reason_and_end_marker() {
        let worker = ListenWorker::spawn(QueueConfig::default(), || {
            ListenEngine::new(PanicAsr, Vad, Turn, ListenConfig {
                partial_interval_samples: 512, first_partial_samples: 512, maximum_partial_interval_samples: 512,
                ..ListenConfig::default()
            })
        }).unwrap();
        worker.wait_ready().unwrap();
        for tick in 0..32u64 {
            if worker.push(AudioChunk { source: "mic", generation: 0, host_time_ns: tick * 32_000_000, samples: &[1.; 512] }).is_err() { break; }
        }
        // The final decode reaches the recognizer; the stop request bounds the test should it not.
        let _ = worker.flush_source("mic");
        worker.request_shutdown();
        let mut reason = None;
        while let Some(event) = worker.recv_event() {
            if let WorkerEvent::Error { source: None, message } = event { reason = Some(message); }
        }
        assert_eq!(reason.as_deref(), Some("listen worker crashed: decoder bug"));
        assert!(worker.is_finished());
        assert!(worker.push(AudioChunk { source: "mic", generation: 0, host_time_ns: 0, samples: &[0.; 512] }).is_err());
    }
    #[test]
    fn closing_all_sources_keeps_the_models_and_the_same_source_can_return() {
        let worker = ListenWorker::spawn(QueueConfig::default(), engine).unwrap();
        let mut first = None;
        for round in 0..2u64 {
            for tick in 0..16u64 {
                worker.push(AudioChunk { source: "mic", generation: round, host_time_ns: (round * 100 + tick) * 32_000_000, samples: &[1.; 512] }).unwrap();
            }
            assert_eq!(worker.close_all_sources(), vec!["mic".to_string()]);
            let (mut finals, mut closed) = (0, false);
            while !closed {
                match worker.recv_event().expect("the worker stays up") {
                    e @ WorkerEvent::Ready => { first.get_or_insert(e); }
                    WorkerEvent::Listen(ListenEvent::FinalUtterance { .. }) => finals += 1,
                    WorkerEvent::SourceClosed { source } => closed = source == "mic",
                    e => { first.get_or_insert(e); }
                }
            }
            assert_eq!(finals, 1, "round {round}");
        }
        assert!(matches!(first, Some(WorkerEvent::Ready)), "{first:?}");
        assert!(worker.close_all_sources().is_empty());
        assert!(!worker.is_finished());
    }
}
