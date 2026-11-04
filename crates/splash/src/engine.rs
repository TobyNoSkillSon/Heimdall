//! The native engine as a child process (server/runtime.py): spawn `splash serve-native`, multiplex requests over its
//! stdin/stdout frames, route events to each request, answer token-mask requests off the reader thread, status and
//! weight release, cancellation. OWNER: engine worker. The contract items (MaskProvider, Submission, RequestEvent)
//! are what other modules code against.
//!
//! One engine per `Engine`, no relaunch: when it fails (EOF, a protocol break, an engine-level ERROR, a stuck loop),
//! every pending request ends with `Failed`, `failure()` says why and new work is refused; Heimdall's manager restarts
//! the whole server. Threads: a reader (parses and routes events, runs `MaskProvider::commit`), a writer (owns stdin,
//! so no caller ever blocks on a stuck pipe and a frame not yet started can be abandoned at its deadline), a few mask
//! workers (which also run `MaskProvider::finish`), and a monitor that sleeps until the next timeout it guards
//! (startup, write stall, unacknowledged cancel, the liveness probe). Grammar code runs under `catch_unwind`: its panic
//! fails its request, never a thread.
use crate::protocol::{
    self as wire, ClientMessage, ConstraintMode, DoneEvent, EngineEvent, ErrorEvent, FailureClass, FinishReason,
    ImageSpan, MaskRequestEvent, MaskResponseFrame, PromptProgressEvent, ReadyEvent, ReleaseRequestFrame, RequestFlags,
    RequestFrame, RequestPriority, SamplingParameters, StartEvent, StatusRequestFrame, ThinkingBudget,
};
use crossbeam_channel::{Receiver, Sender};
use std::collections::{HashMap, VecDeque};
use std::ffi::OsString;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub mod fake;

/// A grammar's view of one request (constraints.py TokenConstraint). `commit` runs on the reader thread with each
/// generated batch and must not block; `masks` runs on a mask worker thread and returns `rows = simulation.len() + 1`
/// rows of `words_per_mask` little-endian u32 words (bit t of a row allows token t).
pub trait MaskProvider: Send + Sync {
    fn commit(&self, tokens: &[u32]) -> anyhow::Result<()>;
    fn masks(&self, simulation: &[u32], words_per_mask: u32) -> anyhow::Result<Vec<u32>>;
    /// Checks the tokens generated after the last mask, once the request is done. The engine client calls it before
    /// delivering a Done that is not CANCELLED; an error turns that Done into `Failed` (FailureKind::Constraint).
    fn finish(&self) -> anyhow::Result<()>;
}

/// One request as the server submits it (the REQUEST frame without ids and deadlines' wire form).
#[derive(Default)]
pub struct Submission {
    pub prompt_tokens: Vec<u32>,
    pub max_output_tokens: u32,
    pub sampling: SamplingParameters,
    pub seed: u64,
    pub priority: RequestPriority,
    /// None: no deadline (the wire still needs one; the engine client uses a far one).
    pub timeout: Option<Duration>,
    pub constraint: Option<std::sync::Arc<dyn MaskProvider>>,
    pub image_spans: Vec<ImageSpan>,
    pub image_pixels: Vec<u8>,
    pub score_tokens: Vec<u32>,
    pub generation_prompt_tokens: u32,
    pub ignore_eos: bool,
    pub return_progress: bool,
    pub cache_salt: u128,
    pub thinking_budget: Option<ThinkingBudget>,
}

/// What a request hears from the engine, in order; `Done` or `Failed` ends it.
#[derive(Clone, Debug)]
pub enum RequestEvent {
    Started { lane: u32, matched_prompt_tokens: u32 },
    Progress { processed_tokens: u32, elapsed_micros: u64 },
    Tokens(Vec<u32>),
    Done(DoneEvent),
    /// A request error, a mask failure, or the engine's failure; `retryable` as the engine said.
    Failed { message: String, retryable: bool, retry_after_seconds: u32, error: Option<ErrorEvent>, kind: FailureKind },
}

/// Who ended a request with `Failed`, which decides its HTTP answer (backend.py `_api_error`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureKind {
    /// The engine refused or failed this request (`error` holds its ERROR event): status by its code.
    Request,
    /// The grammar: a mask, commit or finish failed (400 constraint_error), or the mask queue was full (retryable,
    /// 503 runtime_busy).
    Constraint,
    /// The engine failed (503 runtime_unavailable; `retryable`, as the server restarts with a new engine) or was
    /// shut down (not retryable); `failure()` says why.
    Engine,
}

/// Why an engine call was refused (runtime.py's EngineRuntimeError family).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum EngineError {
    #[error("runtime is closed")]
    Closed,
    #[error("pending request limit {0} is full")]
    PendingLimit(usize),
    #[error("{0}")]
    Timeout(String),
    /// The request broke a protocol rule before it was sent; `code` is the issue name (e.g. `invalid_sampling`).
    #[error("request failed [{code}]: {message}")]
    Request { code: String, message: String },
    #[error("{0}")]
    Unhealthy(String),
    #[error("{0}")]
    ProtocolFatal(String),
    /// A status answer that is not the current schema's JSON.
    #[error("{0}")]
    InvalidStatus(String),
}

/// runtime.py's timing constants; tests shorten them.
#[derive(Clone, Debug)]
pub struct EngineOptions {
    pub startup_timeout: Duration,
    pub pending_limit: usize,
    /// CPU token-mask workers.
    pub mask_workers: usize,
    /// How long a started frame write may make no progress, and how long a frame may wait to start.
    pub io_timeout: Duration,
    /// Allow the native 120-second command watchdog to finish before fencing an unacknowledged cancel.
    pub cancel_grace: Duration,
    /// How often the engine's loop must answer a status request, busy or idle (backend.py's refresh with
    /// fail_unanswered) ...
    pub liveness_interval: Duration,
    /// ... and how long it may take: far above any legitimate tick.
    pub status_answer_limit: Duration,
    /// SIGTERM to SIGKILL.
    pub shutdown_grace: Duration,
}

impl Default for EngineOptions {
    fn default() -> Self {
        Self {
            startup_timeout: Duration::from_secs(30),
            pending_limit: 64,
            mask_workers: std::thread::available_parallelism().map_or(4, |n| n.get().min(8)),
            io_timeout: Duration::from_secs(5),
            cancel_grace: Duration::from_secs(150),
            liveness_interval: Duration::from_secs(10),
            status_answer_limit: Duration::from_secs(30),
            shutdown_grace: Duration::from_secs(15),
        }
    }
}

/// `(absolute unix µs, remaining µs)` for a deadline `remaining` away; none saturates at the wire's u64 maximum, as
/// Python's `math.inf` does (runtime.py `_wire_deadline`).
pub fn wire_deadline(remaining: Option<Duration>, wall_unix_micros: u64) -> (u64, u64) {
    let limit = u64::MAX - wall_unix_micros;
    let micros = match remaining {
        Some(r) if r.as_micros() < limit as u128 => (r.as_micros() as u64).max(1),
        _ => limit,
    };
    (wall_unix_micros + micros, micros)
}

fn unix_micros() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_micros() as u64)
}

const READ_CHUNK_BYTES: usize = 64 * 1024;
/// Writes go out in pieces so the monitor sees progress on a large image frame.
const WRITE_CHUNK_BYTES: usize = 256 * 1024;

// Ticket states: a frame a caller waits on.
const QUEUED: u8 = 0;
const WRITING: u8 = 1;
const WRITTEN: u8 = 2;
const ABANDONED: u8 = 3;
const FAILED: u8 = 4;

struct Outgoing {
    bytes: Vec<u8>,
    /// Skip the frame once this request is no longer pending (a late cancel or mask response).
    guard: Option<u64>,
    ticket: Option<Arc<AtomicU8>>,
}

struct Writing {
    last_progress: Instant,
    offset: usize,
    ticket: Option<Arc<AtomicU8>>,
}

enum StatusWaiter {
    Caller(Sender<Result<wire::StatusJsonEvent, EngineError>>),
    Probe,
}

/// A pending request: the counts its events are checked against, not the request itself (its pixels must not
/// outlive the written frame).
struct Call {
    events: Sender<RequestEvent>,
    finished: Arc<AtomicBool>,
    constraint: Option<Arc<dyn MaskProvider>>,
    prompt_tokens: u32,
    logical_max: u32,
    score_tokens: u32,
    return_progress: bool,
    start: Option<StartEvent>,
    progress: Option<PromptProgressEvent>,
    next_token_offset: u32,
    /// A mask or commit failure: the request ends with it, whatever Done says.
    constraint_error: Option<(String, bool)>,
    cancel_requested: bool,
    cancel_deadline: Option<Instant>,
}

struct State {
    ready: Option<ReadyEvent>,
    startup_deadline: Instant,
    failure: Option<EngineError>,
    closed: bool,
    calls: HashMap<u64, Call>,
    /// Admitted requests, counted from admission (before serialization, which copies image payloads) to their end.
    slots: usize,
    /// Queued and running mask jobs: a cancel frees a request's slot before its mask job necessarily finishes.
    mask_slots: usize,
    status_waiters: HashMap<u64, StatusWaiter>,
    last_status_id: u64,
    next_request_id: u64,
    next_status_id: u64,
    queue: VecDeque<Outgoing>,
    writing: Option<Writing>,
    next_probe: Option<Instant>,
    probe_deadline: Option<Instant>,
}

enum Process {
    Child(std::process::Child),
    Hook(Option<Box<dyn FnOnce() + Send>>),
}

impl Process {
    fn exited(&mut self) -> bool {
        match self {
            Process::Child(child) => !matches!(child.try_wait(), Ok(None)),
            Process::Hook(_) => true,
        }
    }

    fn terminate(&mut self) {
        match self {
            Process::Child(child) => {
                if matches!(child.try_wait(), Ok(None)) {
                    // SAFETY: plain signal to our own unreaped child.
                    unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
                }
            }
            Process::Hook(hook) => {
                if let Some(hook) = hook.take() {
                    hook()
                }
            }
        }
    }

    fn kill(&mut self) {
        match self {
            Process::Child(child) => {
                let _ = child.kill();
            }
            Process::Hook(_) => self.terminate(),
        }
    }
}

struct Inner {
    options: EngineOptions,
    state: Mutex<State>,
    /// Ready, tickets, failure and monitor wake-ups.
    changed: Condvar,
    writer_wake: Condvar,
    process: Mutex<Process>,
    mask_jobs: Mutex<Option<Sender<Job>>>,
    reader_done: Receiver<()>,
    /// For the SIGKILL fallback thread, which must reach the Child (it knows when it was reaped), not a bare pid.
    me: std::sync::Weak<Inner>,
}

struct MaskJob {
    request_id: u64,
    event: MaskRequestEvent,
    provider: Arc<dyn MaskProvider>,
}

/// Grammar work off the reader thread.
enum Job {
    Mask(MaskJob),
    /// A constrained request's Done waits on `MaskProvider::finish`; nothing follows it for that request.
    Finish { provider: Arc<dyn MaskProvider>, done: DoneEvent, events: Sender<RequestEvent> },
}

/// The multiplexed client of one native engine. Share it behind an `Arc`; dropping it stops the engine.
pub struct Engine {
    inner: Arc<Inner>,
}

/// One submitted request. Events arrive on `events` until `Done` or `Failed`; dropping an unfinished request cancels
/// it.
pub struct Request {
    pub id: u64,
    pub events: Receiver<RequestEvent>,
    finished: Arc<AtomicBool>,
    inner: Arc<Inner>,
}

/// A request's whole output: its tokens and the event that ended it.
#[derive(Clone, Debug)]
pub struct Collected {
    pub tokens: Vec<u32>,
    pub end: RequestEvent,
}

impl Request {
    /// Ask the engine to stop the request; false when it already ended or a cancel was already sent.
    pub fn cancel(&self) -> bool {
        self.inner.cancel(self.id)
    }

    pub fn is_finished(&self) -> bool {
        self.finished.load(Ordering::Acquire)
    }

    /// Read events until the request ends; None if `timeout` passes first.
    pub fn collect(&self, timeout: Duration) -> Option<Collected> {
        let deadline = Instant::now() + timeout;
        let mut tokens = Vec::new();
        loop {
            match self.events.recv_deadline(deadline).ok()? {
                RequestEvent::Tokens(batch) => tokens.extend(batch),
                end @ (RequestEvent::Done(_) | RequestEvent::Failed { .. }) => return Some(Collected { tokens, end }),
                _ => {}
            }
        }
    }
}

impl Drop for Request {
    fn drop(&mut self) {
        if !self.is_finished() {
            self.inner.cancel(self.id);
        }
    }
}

impl std::fmt::Debug for Request {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Request").field("id", &self.id).field("finished", &self.is_finished()).finish()
    }
}

impl Engine {
    /// Start `command` (`splash serve-native ...`) with stdin and stdout piped and stderr inherited. Returns at once;
    /// `wait_ready` waits for its READY, and the engine fails if none arrives within `startup_timeout`.
    pub fn spawn(command: Vec<OsString>, startup_timeout: Duration) -> Result<Engine, EngineError> {
        Self::spawn_with(command, EngineOptions { startup_timeout, ..Default::default() })
    }

    pub fn spawn_with(command: Vec<OsString>, options: EngineOptions) -> Result<Engine, EngineError> {
        let (program, args) = command.split_first().ok_or_else(|| unhealthy("empty engine command"))?;
        // Rust's ChildStdin is unbuffered: each write is a write(2), as Python's bufsize=0.
        let mut child = std::process::Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .map_err(|e| {
                EngineError::Unhealthy(if e.kind() == std::io::ErrorKind::NotFound {
                    "native engine executable is missing; the installation may have been upgraded or removed. Stop \
                     the server and restart Splash from the current installation"
                        .into()
                } else {
                    format!("could not launch native engine: {e}")
                })
            })?;
        let (stdin, stdout) = (child.stdin.take().unwrap(), child.stdout.take().unwrap());
        Ok(Self::start(Box::new(stdout), Box::new(stdin), Process::Child(child), options))
    }

    /// An engine over any byte streams, for the fake engine and tests. `kill` runs once, when the engine fails or
    /// shuts down; it should end both streams.
    pub fn from_streams(
        reader: impl Read + Send + 'static,
        writer: impl Write + Send + 'static,
        kill: impl FnOnce() + Send + 'static,
        options: EngineOptions,
    ) -> Engine {
        Self::start(Box::new(reader), Box::new(writer), Process::Hook(Some(Box::new(kill))), options)
    }

    fn start(reader: Box<dyn Read + Send>, writer: Box<dyn Write + Send>, process: Process, options: EngineOptions) -> Engine {
        assert!(options.pending_limit > 0, "pending_limit must be positive");
        let (jobs_tx, jobs_rx) = crossbeam_channel::unbounded::<Job>();
        let (done_tx, done_rx) = crossbeam_channel::bounded(1);
        let inner = Arc::new_cyclic(|me| Inner {
            state: Mutex::new(State {
                ready: None,
                startup_deadline: Instant::now() + options.startup_timeout,
                failure: None,
                closed: false,
                calls: HashMap::new(),
                slots: 0,
                mask_slots: 0,
                status_waiters: HashMap::new(),
                last_status_id: 0,
                next_request_id: 1,
                next_status_id: 1,
                queue: VecDeque::new(),
                writing: None,
                next_probe: None,
                probe_deadline: None,
            }),
            changed: Condvar::new(),
            writer_wake: Condvar::new(),
            process: Mutex::new(process),
            mask_jobs: Mutex::new(Some(jobs_tx)),
            reader_done: done_rx,
            me: me.clone(),
            options,
        });
        let spawn = |name: String, body: Box<dyn FnOnce() + Send>| {
            std::thread::Builder::new().name(name).spawn(body).expect("spawn engine thread");
        };
        let i = inner.clone();
        spawn("splash-native-reader".into(), Box::new(move || {
            i.reader_loop(reader);
            let _ = done_tx.send(());
        }));
        let i = inner.clone();
        spawn("splash-native-writer".into(), Box::new(move || i.writer_loop(writer)));
        let i = inner.clone();
        spawn("splash-native-monitor".into(), Box::new(move || i.monitor_loop()));
        for n in 0..inner.options.mask_workers.max(1) {
            let (i, jobs) = (inner.clone(), jobs_rx.clone());
            spawn(format!("splash-mask-{n}"), Box::new(move || {
                for job in jobs {
                    match job {
                        Job::Mask(job) => i.run_mask_job(job),
                        Job::Finish { provider, done, events } => finish_request(&*provider, done, &events),
                    }
                }
            }));
        }
        Engine { inner }
    }

    /// Wait for the engine's READY (or its failure, or `timeout`).
    pub fn wait_ready(&self, timeout: Option<Duration>) -> Result<ReadyEvent, EngineError> {
        let deadline = timeout.map(|t| Instant::now() + t);
        let mut st = self.inner.lock();
        loop {
            if let Some(failure) = &st.failure {
                return Err(failure.clone());
            }
            if let Some(ready) = st.ready {
                return Ok(ready);
            }
            st = match deadline {
                None => self.inner.changed.wait(st).unwrap(),
                Some(deadline) => {
                    let now = Instant::now();
                    if now >= deadline {
                        return Err(EngineError::Timeout("native ready wait timed out".into()));
                    }
                    self.inner.changed.wait_timeout(st, deadline - now).unwrap().0
                }
            };
        }
    }

    /// The engine's READY while it serves; None before it and once it failed or shut down.
    pub fn ready(&self) -> Option<ReadyEvent> {
        let st = self.inner.lock();
        st.failure.is_none().then_some(st.ready).flatten()
    }

    /// Why the engine stopped serving: set once, never cleared. The server exits when it is.
    pub fn failure(&self) -> Option<EngineError> {
        self.inner.lock().failure.clone()
    }

    pub fn pending_count(&self) -> usize {
        self.inner.lock().calls.len()
    }

    pub fn pending_limit(&self) -> usize {
        self.inner.options.pending_limit
    }

    /// Admit and write one request (runtime.py `submit`). It returns once the frame is written; a frame that cannot
    /// start within the request's deadline or the I/O timeout is not sent.
    pub fn submit(&self, submission: Submission) -> Result<Request, EngineError> {
        let inner = &self.inner;
        let started = Instant::now();
        let request_id = {
            let mut st = inner.lock();
            let id = st.next_request_id;
            st.next_request_id += 1;
            if st.slots >= inner.options.pending_limit {
                return Err(EngineError::PendingLimit(inner.options.pending_limit));
            }
            st.slots += 1;
            id
        };
        let release_slot = || inner.lock().slots -= 1;
        if submission.timeout == Some(Duration::ZERO) {
            release_slot();
            return Err(EngineError::Timeout("native operation timed out".into()));
        }
        let deadline = submission.timeout.and_then(|t| started.checked_add(t));
        let (absolute, remaining) = wire_deadline(submission.timeout, unix_micros());
        let constraint = submission.constraint;
        let mut flags = RequestFlags::NONE;
        if submission.ignore_eos {
            flags = flags | RequestFlags::IGNORE_END_OF_SEQUENCE;
        }
        if submission.return_progress {
            flags = flags | RequestFlags::RETURN_PROGRESS;
        }
        let frame = RequestFrame {
            request_id,
            priority: submission.priority,
            absolute_deadline_unix_micros: absolute,
            remaining_deadline_micros: remaining,
            logical_max_output_tokens: submission.max_output_tokens,
            prompt_tokens: submission.prompt_tokens,
            sampling: submission.sampling,
            seed: submission.seed,
            constraint: if constraint.is_some() { ConstraintMode::TokenMask } else { ConstraintMode::None },
            image_spans: submission.image_spans,
            image_pixels: submission.image_pixels,
            score_tokens: submission.score_tokens,
            generation_prompt_tokens: submission.generation_prompt_tokens,
            flags,
            cache_salt: submission.cache_salt,
            thinking_budget: submission.thinking_budget,
        };
        // The frame (and its pixels) is consumed here: serialized once, then dropped.
        let encoded = match wire::serialize_message(&ClientMessage::Request(frame)) {
            Ok(bytes) => bytes,
            Err(error) => {
                release_slot();
                return Err(request_protocol_error(error.issue));
            }
        };
        let header_counts = RequestCounts::of(&encoded);
        let (events_tx, events_rx) = crossbeam_channel::unbounded();
        let finished = Arc::new(AtomicBool::new(false));
        let ticket = Arc::new(AtomicU8::new(QUEUED));
        {
            let mut st = inner.lock();
            let refusal = if st.closed {
                Some(EngineError::Closed)
            } else if let Some(failure) = &st.failure {
                Some(failure.clone())
            } else if st.ready.is_none() {
                Some(EngineError::Unhealthy("native process is not ready".into()))
            } else {
                None
            };
            if let Some(refusal) = refusal {
                st.slots -= 1;
                return Err(refusal);
            }
            st.calls.insert(
                request_id,
                Call {
                    events: events_tx,
                    finished: finished.clone(),
                    constraint,
                    prompt_tokens: header_counts.prompt_tokens,
                    logical_max: header_counts.logical_max,
                    score_tokens: header_counts.score_tokens,
                    return_progress: flags.contains(RequestFlags::RETURN_PROGRESS),
                    start: None,
                    progress: None,
                    next_token_offset: 0,
                    constraint_error: None,
                    cancel_requested: false,
                    cancel_deadline: None,
                },
            );
            inner.enqueue(&mut st, Outgoing { bytes: encoded, guard: Some(request_id), ticket: Some(ticket.clone()) });
        }
        let limit = earliest(deadline, Instant::now() + inner.options.io_timeout);
        match inner.wait_written(&ticket, limit) {
            Ok(()) => Ok(Request { id: request_id, events: events_rx, finished, inner: inner.clone() }),
            Err(error) => {
                let mut st = inner.lock();
                if st.calls.remove(&request_id).is_some() {
                    st.slots -= 1;
                }
                Err(error)
            }
        }
    }

    /// The engine's status JSON (schema_version 6), answered between its commands.
    pub fn status(&self, timeout: Duration) -> Result<serde_json::Value, EngineError> {
        self.inner.status(timeout, false)
    }

    /// Ask the engine to give its weights back now. It does so only while it holds no request and restores none; the
    /// answer's `weights.released` (and not `restoring`) says whether it did. The next request restores them.
    pub fn release_weights(&self, timeout: Duration) -> Result<serde_json::Value, EngineError> {
        self.inner.status(timeout, true)
    }

    /// Fail every pending request with `Closed`, close the engine's stdin, SIGTERM it, wait up to `grace` for it to
    /// exit, then SIGKILL it (runtime.py `close` / `_stop_process`).
    pub fn shutdown(&self, grace: Duration) {
        if !self.inner.close() {
            return;
        }
        let mut process = self.inner.process.lock().unwrap();
        process.terminate();
        let deadline = Instant::now() + grace;
        while !process.exited() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        if !process.exited() {
            process.kill();
            let deadline = Instant::now() + Duration::from_secs(1);
            while !process.exited() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        drop(process);
        let _ = self.inner.reader_done.recv_timeout(Duration::from_secs(1));
    }
}

impl Drop for Engine {
    /// Stops without blocking: stdin closes, the engine gets SIGTERM, SIGKILL after the shutdown grace, and is reaped.
    fn drop(&mut self) {
        if self.inner.close() {
            self.inner.terminate_with_fallback();
        }
    }
}

/// The counts the client checks events against, read back from the encoded frame's fixed head.
struct RequestCounts {
    logical_max: u32,
    prompt_tokens: u32,
    score_tokens: u32,
}

impl RequestCounts {
    fn of(frame: &[u8]) -> Self {
        let at = |offset: usize| {
            let o = wire::FRAME_HEADER_BYTES + offset;
            u32::from_le_bytes(frame[o..o + 4].try_into().unwrap())
        };
        Self { logical_max: at(26), prompt_tokens: at(30), score_tokens: at(74) }
    }
}

fn unhealthy(message: &str) -> EngineError {
    EngineError::Unhealthy(message.into())
}

fn earliest(a: Option<Instant>, b: Instant) -> Instant {
    a.map_or(b, |a| a.min(b))
}

/// runtime.py `_request_protocol_error`.
fn request_protocol_error(issue: wire::ProtocolIssue) -> EngineError {
    match issue.failure_class {
        FailureClass::RequestError => EngineError::Request { code: issue.code.name().into(), message: issue.message },
        FailureClass::EngineUnhealthy => EngineError::Unhealthy(issue.describe()),
        FailureClass::ProtocolFatal => EngineError::ProtocolFatal(issue.describe()),
    }
}

/// The parser and the decoder fail the stream, or the engine when an allocation fails; a request's own failure
/// arrives as an ErrorEvent.
fn issue_error(issue: wire::ProtocolIssue) -> EngineError {
    match issue.failure_class {
        FailureClass::EngineUnhealthy => EngineError::Unhealthy(issue.describe()),
        _ => EngineError::ProtocolFatal(issue.describe()),
    }
}

fn fatal(message: impl Into<String>) -> EngineError {
    EngineError::ProtocolFatal(message.into())
}

/// Work the reader does after releasing the state lock: grammar calls must not run under it.
enum Deferred {
    Commit { request_id: u64, provider: Arc<dyn MaskProvider>, tokens: Vec<u32> },
    Finish { provider: Arc<dyn MaskProvider>, done: DoneEvent, events: Sender<RequestEvent> },
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn enqueue(&self, st: &mut State, item: Outgoing) {
        st.queue.push_back(item);
        self.writer_wake.notify_one();
    }

    /// Wait for a queued frame to be written. A frame not started by `limit` is abandoned (and never sent); a started
    /// one must finish, bounded by the monitor's write-stall check.
    fn wait_written(&self, ticket: &AtomicU8, limit: Instant) -> Result<(), EngineError> {
        let mut st = self.lock();
        loop {
            match ticket.load(Ordering::Acquire) {
                WRITTEN => return Ok(()),
                FAILED | ABANDONED => {
                    return Err(st.failure.clone().unwrap_or_else(|| unhealthy("native write failed")))
                }
                QUEUED => {
                    let now = Instant::now();
                    if now >= limit {
                        if ticket.compare_exchange(QUEUED, ABANDONED, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                            return Err(EngineError::Timeout("native write lock timed out".into()));
                        }
                        continue;
                    }
                    st = self.changed.wait_timeout(st, limit - now).unwrap().0;
                }
                _ => st = self.changed.wait(st).unwrap(),
            }
        }
    }

    fn status(&self, timeout: Duration, release: bool) -> Result<serde_json::Value, EngineError> {
        let deadline = Instant::now() + timeout;
        let (tx, rx) = crossbeam_channel::bounded(1);
        let ticket = Arc::new(AtomicU8::new(QUEUED));
        let correlation_id = {
            let mut st = self.lock();
            if st.closed {
                return Err(EngineError::Closed);
            }
            if let Some(failure) = &st.failure {
                return Err(failure.clone());
            }
            if st.ready.is_none() {
                return Err(unhealthy("native process is not ready"));
            }
            let id = st.next_status_id;
            st.next_status_id += 1;
            st.last_status_id = id;
            st.status_waiters.insert(id, StatusWaiter::Caller(tx));
            let message = if release {
                ClientMessage::ReleaseRequest(ReleaseRequestFrame { correlation_id: id })
            } else {
                ClientMessage::StatusRequest(StatusRequestFrame { correlation_id: id })
            };
            let bytes = wire::serialize_message(&message).expect("status frames always serialize");
            self.enqueue(&mut st, Outgoing { bytes, guard: None, ticket: Some(ticket.clone()) });
            id
        };
        let forget = || {
            self.lock().status_waiters.remove(&correlation_id);
        };
        if let Err(error) = self.wait_written(&ticket, deadline.min(Instant::now() + self.options.io_timeout)) {
            forget();
            return Err(error);
        }
        let event = match rx.recv_deadline(deadline) {
            Ok(answer) => answer?,
            Err(_) => {
                forget();
                return Err(EngineError::Timeout("native status response timed out".into()));
            }
        };
        let snapshot: serde_json::Value = serde_json::from_slice(&event.json)
            .map_err(|e| EngineError::InvalidStatus(format!("native status is not JSON: {e}")))?;
        if snapshot.get("schema_version").and_then(|v| v.as_u64()) != Some(wire::STATUS_SCHEMA_VERSION) {
            return Err(EngineError::InvalidStatus("native status does not match the current schema".into()));
        }
        Ok(snapshot)
    }

    /// Mark the request cancelled and queue its CANCEL; the engine answers with Done(CANCELLED).
    fn cancel(&self, request_id: u64) -> bool {
        let mut st = self.lock();
        self.cancel_locked(&mut st, request_id)
    }

    fn cancel_locked(&self, st: &mut State, request_id: u64) -> bool {
        let grace = self.options.cancel_grace;
        let Some(call) = st.calls.get_mut(&request_id) else { return false };
        if call.cancel_requested {
            return false;
        }
        call.cancel_requested = true;
        call.cancel_deadline = Some(Instant::now() + grace);
        let bytes = wire::serialize_message(&ClientMessage::Cancel(wire::CancelFrame { request_id })).unwrap();
        self.enqueue(st, Outgoing { bytes, guard: Some(request_id), ticket: None });
        self.changed.notify_all();
        true
    }

    /// A grammar failure ends the request with it: recorded, then the request is cancelled.
    fn constraint_failed(&self, st: &mut State, request_id: u64, message: String, retryable: bool) {
        if let Some(call) = st.calls.get_mut(&request_id) {
            call.constraint_error.get_or_insert((message, retryable));
            self.cancel_locked(st, request_id);
        }
    }

    /// Mark closed and fail everything with `Closed`; false if it already was.
    fn close(&self) -> bool {
        {
            let mut st = self.lock();
            if st.closed {
                return false;
            }
            st.closed = true;
        }
        self.fail(EngineError::Closed);
        self.mask_jobs.lock().unwrap().take();
        true
    }

    /// The engine has failed (runtime.py `_begin_generation_failure`): every pending request and status waiter hears
    /// why, queued frames are dropped (the writer then exits and closes stdin), and a failed engine gets SIGTERM.
    fn fail(&self, error: EngineError) {
        let (calls, waiters) = {
            let mut st = self.lock();
            if st.failure.is_some() {
                return;
            }
            st.failure = Some(error.clone());
            st.ready = None;
            let calls: Vec<Call> = st.calls.drain().map(|(_, call)| call).collect();
            st.slots -= calls.len();
            let waiters: Vec<StatusWaiter> = st.status_waiters.drain().map(|(_, w)| w).collect();
            for item in st.queue.drain(..) {
                if let Some(ticket) = item.ticket {
                    ticket.store(FAILED, Ordering::Release);
                }
            }
            if let Some(ticket) = st.writing.as_ref().and_then(|w| w.ticket.as_ref()) {
                ticket.store(FAILED, Ordering::Release);
            }
            st.next_probe = None;
            st.probe_deadline = None;
            self.changed.notify_all();
            self.writer_wake.notify_all();
            (calls, waiters)
        };
        let retryable = error != EngineError::Closed;
        for call in calls {
            call.finished.store(true, Ordering::Release);
            let _ = call.events.send(RequestEvent::Failed {
                message: error.to_string(),
                retryable,
                retry_after_seconds: 0,
                error: None,
                kind: FailureKind::Engine,
            });
        }
        for waiter in waiters {
            if let StatusWaiter::Caller(tx) = waiter {
                let _ = tx.send(Err(error.clone()));
            }
        }
        if error != EngineError::Closed {
            self.terminate_with_fallback();
        }
    }

    /// SIGTERM now; a reaper thread then reaps the child, with SIGKILL after the shutdown grace, without blocking the
    /// caller. It holds the engine until the child is reaped, so a dropped Engine leaves no zombie.
    fn terminate_with_fallback(&self) {
        let mut process = self.process.lock().unwrap();
        process.terminate();
        let child = matches!(*process, Process::Child(_));
        drop(process);
        let Some(inner) = self.me.upgrade().filter(|_| child) else { return };
        let grace = self.options.shutdown_grace;
        let _ = std::thread::Builder::new().name("splash-native-reaper".into()).spawn(move || {
            let deadline = Instant::now() + grace;
            loop {
                {
                    let mut process = inner.process.lock().unwrap();
                    if process.exited() {
                        return;
                    }
                    if Instant::now() >= deadline {
                        process.kill();
                        if let Process::Child(child) = &mut *process {
                            let _ = child.wait();
                        }
                        return;
                    }
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        });
    }

    fn writer_loop(&self, mut out: Box<dyn Write + Send>) {
        loop {
            let item = {
                let mut st = self.lock();
                loop {
                    if st.failure.is_some() {
                        // Dropping `out` closes the engine's stdin.
                        return;
                    }
                    if let Some(item) = st.queue.pop_front() {
                        if item.guard.is_some_and(|id| !st.calls.contains_key(&id)) {
                            continue;
                        }
                        if let Some(ticket) = &item.ticket {
                            if ticket.compare_exchange(QUEUED, WRITING, Ordering::AcqRel, Ordering::Acquire).is_err() {
                                continue;
                            }
                        }
                        st.writing = Some(Writing { last_progress: Instant::now(), offset: 0, ticket: item.ticket.clone() });
                        self.changed.notify_all();
                        break item;
                    }
                    st = self.writer_wake.wait(st).unwrap();
                }
            };
            let mut result = Ok(());
            for chunk in item.bytes.chunks(WRITE_CHUNK_BYTES) {
                result = out.write_all(chunk);
                if result.is_err() {
                    break;
                }
                if let Some(writing) = self.lock().writing.as_mut() {
                    writing.offset += chunk.len();
                    writing.last_progress = Instant::now();
                }
            }
            let result = result.and_then(|()| out.flush());
            {
                let mut st = self.lock();
                st.writing = None;
                if let Some(ticket) = &item.ticket {
                    let _ = ticket.compare_exchange(
                        WRITING,
                        if result.is_ok() { WRITTEN } else { FAILED },
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    );
                }
                self.changed.notify_all();
            }
            if let Err(error) = result {
                self.fail(EngineError::Unhealthy(format!("native protocol write failed: {error}")));
                return;
            }
        }
    }

    fn reader_loop(&self, mut input: Box<dyn Read + Send>) {
        let mut parser = wire::FrameParser::new();
        let mut buffer = vec![0u8; READ_CHUNK_BYTES];
        let error = 'read: loop {
            let n = match input.read(&mut buffer) {
                Ok(0) => {
                    break parser.finish().map_or_else(|| unhealthy("native protocol reached EOF"), issue_error)
                }
                Ok(n) => n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => break EngineError::Unhealthy(format!("native reader failed: {e}")),
            };
            let mut offset = 0;
            while offset < n {
                let step = parser.consume(&buffer[offset..n]);
                offset += step.consumed;
                if let Some(found) = step.issue {
                    break 'read issue_error(found);
                }
                if step.consumed == 0 {
                    break 'read fatal("native parser made no progress");
                }
                let Some(frame) = step.frame else { continue };
                let outcome = wire::decode_frame(&frame).map_err(|e| issue_error(e.issue)).and_then(|event| self.dispatch(event));
                match outcome {
                    Ok(Some(deferred)) => self.run_deferred(deferred),
                    Ok(None) => {}
                    Err(error) => break 'read error,
                }
            }
        };
        self.fail(error);
    }

    /// Route one event (runtime.py `_dispatch_message`); an error fails the engine.
    fn dispatch(&self, event: EngineEvent) -> Result<Option<Deferred>, EngineError> {
        let mut st = self.lock();
        if st.failure.is_some() {
            return Ok(None);
        }
        if let EngineEvent::Ready(ready) = event {
            if st.ready.is_some() {
                return Err(fatal("native sent more than one ReadyEvent"));
            }
            if Instant::now() >= st.startup_deadline {
                return Err(unhealthy("native ReadyEvent timed out"));
            }
            st.ready = Some(ready);
            st.next_probe = Some(Instant::now() + self.options.liveness_interval);
            self.changed.notify_all();
            return Ok(None);
        }
        // Before its READY an engine sends nothing but why its start failed.
        let startup_error = matches!(&event, EngineEvent::Error(e) if e.failure_class == FailureClass::EngineUnhealthy);
        if st.ready.is_none() && !startup_error {
            return Err(fatal("native event arrived before ReadyEvent"));
        }
        match event {
            EngineEvent::Ready(_) => unreachable!(),
            EngineEvent::StatusJson(status) => {
                match st.status_waiters.remove(&status.correlation_id) {
                    Some(StatusWaiter::Caller(tx)) => {
                        let _ = tx.send(Ok(status));
                    }
                    Some(StatusWaiter::Probe) => {
                        st.probe_deadline = None;
                        st.next_probe = Some(Instant::now() + self.options.liveness_interval);
                        self.changed.notify_all();
                    }
                    // A late answer to a status request that timed out.
                    None if status.correlation_id > 0 && status.correlation_id <= st.last_status_id => {}
                    None => {
                        return Err(fatal(format!(
                            "native status references unknown correlation {}",
                            status.correlation_id
                        )))
                    }
                }
                Ok(None)
            }
            EngineEvent::Error(error) => match error.failure_class {
                FailureClass::RequestError => {
                    let id = error.request_id;
                    require(&mut st, id)?;
                    let call = self.finish_call(&mut st, id);
                    let _ = call.events.send(RequestEvent::Failed {
                        message: error.message_text(),
                        retryable: error.retryable,
                        retry_after_seconds: error.retry_after_seconds,
                        error: Some(error),
                        kind: FailureKind::Request,
                    });
                    Ok(None)
                }
                FailureClass::EngineUnhealthy => Err(EngineError::Unhealthy(error.message_text())),
                FailureClass::ProtocolFatal => Err(EngineError::ProtocolFatal(error.message_text())),
            },
            EngineEvent::Start(start) => {
                let call = require(&mut st, start.request_id)?;
                if call.start.is_some() {
                    return Err(fatal("duplicate or late StartEvent"));
                }
                call.start = Some(start);
                let _ = call.events.send(RequestEvent::Started {
                    lane: start.lane,
                    matched_prompt_tokens: start.matched_prompt_tokens,
                });
                Ok(None)
            }
            EngineEvent::PromptProgress(progress) => {
                let call = require(&mut st, progress.request_id)?;
                record_progress(call, progress).map_err(fatal)?;
                let _ = call.events.send(RequestEvent::Progress {
                    processed_tokens: progress.processed_tokens,
                    elapsed_micros: progress.elapsed_micros,
                });
                Ok(None)
            }
            EngineEvent::Tokens(tokens) => {
                let call = require(&mut st, tokens.request_id)?;
                record_tokens(call, &tokens).map_err(fatal)?;
                match &call.constraint {
                    // The grammar sees the tokens before the consumer does.
                    Some(provider) => Ok(Some(Deferred::Commit {
                        request_id: tokens.request_id,
                        provider: provider.clone(),
                        tokens: tokens.tokens,
                    })),
                    None => {
                        let _ = call.events.send(RequestEvent::Tokens(tokens.tokens));
                        Ok(None)
                    }
                }
            }
            EngineEvent::MaskRequest(mask) => {
                let id = mask.request_id;
                let call = require(&mut st, id)?;
                let Some(provider) = call.constraint.clone() else {
                    return Err(fatal("native requested a token mask for an unconstrained request"));
                };
                if call.cancel_requested {
                    return Ok(None);
                }
                if st.mask_slots >= self.options.pending_limit {
                    self.constraint_failed(&mut st, id, "token-mask queue is full".into(), true);
                    return Ok(None);
                }
                let jobs = self.mask_jobs.lock().unwrap();
                match jobs.as_ref().map(|jobs| jobs.send(Job::Mask(MaskJob { request_id: id, event: mask, provider }))) {
                    Some(Ok(())) => st.mask_slots += 1,
                    _ => self.constraint_failed(&mut st, id, "token-mask workers are shut down".into(), true),
                }
                Ok(None)
            }
            EngineEvent::Done(done) => {
                let id = done.request_id;
                check_done(require(&mut st, id)?, &done).map_err(fatal)?;
                let call = self.finish_call(&mut st, id);
                if let Some((message, retryable)) = call.constraint_error {
                    let _ = call.events.send(constraint_failure(message, retryable));
                    return Ok(None);
                }
                match call.constraint {
                    // The grammar checks what was generated after the last mask; a cancelled request needs no check.
                    Some(provider) if done.reason != FinishReason::Cancelled => {
                        Ok(Some(Deferred::Finish { provider, done, events: call.events }))
                    }
                    _ => {
                        let _ = call.events.send(RequestEvent::Done(done));
                        Ok(None)
                    }
                }
            }
        }
    }

    /// Remove a call that is ending; its terminal event is the caller's to send.
    fn finish_call(&self, st: &mut State, request_id: u64) -> Call {
        let call = st.calls.remove(&request_id).expect("required call");
        st.slots -= 1;
        call.finished.store(true, Ordering::Release);
        self.changed.notify_all();
        call
    }

    fn run_deferred(&self, deferred: Deferred) {
        match deferred {
            Deferred::Commit { request_id, provider, tokens } => {
                let result = guarded(|| provider.commit(&tokens));
                let mut st = self.lock();
                match result {
                    Ok(()) => {
                        if let Some(call) = st.calls.get(&request_id) {
                            let _ = call.events.send(RequestEvent::Tokens(tokens));
                        }
                    }
                    Err(error) => self.constraint_failed(&mut st, request_id, format!("{error:#}"), false),
                }
            }
            Deferred::Finish { provider, done, events } => {
                let job = Job::Finish { provider, done, events };
                let unsent = match self.mask_jobs.lock().unwrap().as_ref() {
                    Some(jobs) => jobs.send(job).err().map(|e| e.0),
                    None => Some(job),
                };
                // Shut down: no worker is left, so finish here.
                if let Some(Job::Finish { provider, done, events }) = unsent {
                    finish_request(&*provider, done, &events);
                }
            }
        }
    }

    /// runtime.py `_submit_mask`'s job: compute, check the size, answer; any failure ends the request.
    fn run_mask_job(&self, job: MaskJob) {
        let MaskJob { request_id, event, provider } = job;
        let live = |st: &State| st.calls.get(&request_id).is_some_and(|c| !c.cancel_requested);
        let result = if live(&self.lock()) {
            let expected = event.words_per_mask as usize * event.mask_rows();
            guarded(|| provider.masks(&event.simulation_tokens, event.words_per_mask))
                .map_err(|e| format!("mask computation failed: {e:#}"))
                .and_then(|words| {
                    if words.len() != expected {
                        return Err(format!("mask provider returned {} words; expected {expected}", words.len()));
                    }
                    let response = MaskResponseFrame { request_id, mask_request_id: event.mask_request_id, mask_words: words };
                    wire::serialize_message(&ClientMessage::MaskResponse(response)).map_err(|e| e.issue.describe())
                })
                .map(Some)
        } else {
            Ok(None)
        };
        let mut st = self.lock();
        st.mask_slots -= 1;
        match result {
            Ok(Some(bytes)) if live(&st) => self.enqueue(&mut st, Outgoing { bytes, guard: Some(request_id), ticket: None }),
            Ok(_) => {}
            Err(message) => self.constraint_failed(&mut st, request_id, message, false),
        }
    }

    /// Sleeps until the next deadline it guards; fails the engine when one passes.
    fn monitor_loop(&self) {
        let options = &self.options;
        let mut st = self.lock();
        loop {
            if st.failure.is_some() {
                return;
            }
            let now = Instant::now();
            let mut wake: Option<Instant> = None;
            let mut due = |at: Instant| {
                wake = Some(wake.map_or(at, |w| w.min(at)));
                now >= at
            };
            let mut failure = None;
            if st.ready.is_none() && due(st.startup_deadline) {
                failure = Some(unhealthy("native ReadyEvent timed out"));
            }
            if let Some(writing) = &st.writing {
                if due(writing.last_progress + options.io_timeout) {
                    failure = Some(EngineError::Unhealthy(format!(
                        "native frame write timed out after {} bytes",
                        writing.offset
                    )));
                }
            }
            if st.calls.values().filter_map(|c| c.cancel_deadline).any(&mut due) {
                failure = Some(unhealthy("native did not acknowledge cancellation"));
            }
            // The engine's reader drains stdin even while its loop is stuck; only an answer from the loop shows it runs.
            if let Some(deadline) = st.probe_deadline {
                if due(deadline) {
                    failure = Some(EngineError::Unhealthy(format!(
                        "native loop did not answer status within {} s",
                        options.status_answer_limit.as_secs_f64()
                    )));
                }
            } else if let Some(at) = st.next_probe {
                if due(at) {
                    let id = st.next_status_id;
                    st.next_status_id += 1;
                    st.last_status_id = id;
                    st.status_waiters.insert(id, StatusWaiter::Probe);
                    st.next_probe = None;
                    st.probe_deadline = Some(now + options.status_answer_limit);
                    let bytes = wire::serialize_message(&ClientMessage::StatusRequest(StatusRequestFrame { correlation_id: id }))
                        .unwrap();
                    self.enqueue(&mut st, Outgoing { bytes, guard: None, ticket: None });
                    continue;
                }
            }
            if let Some(failure) = failure {
                drop(st);
                self.fail(failure);
                return;
            }
            st = match wake {
                None => self.changed.wait(st).unwrap(),
                Some(at) => self.changed.wait_timeout(st, at.saturating_duration_since(Instant::now())).unwrap().0,
            };
        }
    }
}

fn require(st: &mut State, request_id: u64) -> Result<&mut Call, EngineError> {
    st.calls
        .get_mut(&request_id)
        .ok_or_else(|| fatal(format!("native event references unknown request {request_id}")))
}

/// Grammar code is the server's, not the engine's: a panic in it becomes that request's error.
fn guarded<T>(call: impl FnOnce() -> anyhow::Result<T>) -> anyhow::Result<T> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(call)).unwrap_or_else(|payload| {
        let message = payload
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "unknown panic".into());
        Err(anyhow::anyhow!("constraint panicked: {message}"))
    })
}

/// The grammar checks what was generated after the last mask; its verdict decides Done or Failed.
fn finish_request(provider: &dyn MaskProvider, done: DoneEvent, events: &Sender<RequestEvent>) {
    let _ = events.send(match guarded(|| provider.finish()) {
        Ok(()) => RequestEvent::Done(done),
        Err(error) => constraint_failure(format!("{error:#}"), false),
    });
}

fn constraint_failure(message: String, retryable: bool) -> RequestEvent {
    RequestEvent::Failed { message, retryable, retry_after_seconds: 0, error: None, kind: FailureKind::Constraint }
}

/// runtime.py `RuntimeCall._record_progress`.
fn record_progress(call: &mut Call, event: PromptProgressEvent) -> Result<(), &'static str> {
    if !call.return_progress {
        return Err("unsolicited prompt progress");
    }
    let Some(start) = call.start.filter(|_| call.next_token_offset == 0) else {
        return Err("prompt progress outside prefill");
    };
    if !(start.matched_prompt_tokens <= event.processed_tokens && event.processed_tokens <= call.prompt_tokens) {
        return Err("prompt progress is outside the request's token range");
    }
    if let Some(previous) = call.progress {
        if event.processed_tokens <= previous.processed_tokens || event.elapsed_micros < previous.elapsed_micros {
            return Err("prompt progress moved backwards or repeated");
        }
    }
    call.progress = Some(event);
    Ok(())
}

/// runtime.py `RuntimeCall._record_tokens`.
fn record_tokens(call: &mut Call, event: &wire::TokensEvent) -> Result<(), String> {
    if call.start.is_none() {
        return Err("TokensEvent arrived before StartEvent".into());
    }
    let expected = call.next_token_offset;
    if event.sequence_offset != expected {
        let relation = if event.sequence_offset < expected {
            "overlaps or duplicates prior tokens"
        } else {
            "leaves a gap in the token stream"
        };
        return Err(format!("TokensEvent offset {} {relation}; expected {expected}", event.sequence_offset));
    }
    let next = expected as u64 + event.tokens.len() as u64;
    if next > call.logical_max as u64 {
        return Err(format!("TokensEvent stream length {next} exceeds logical maximum {}", call.logical_max));
    }
    call.next_token_offset = next as u32;
    Ok(())
}

/// runtime.py `RuntimeCall._check_done`.
fn check_done(call: &Call, done: &DoneEvent) -> Result<(), String> {
    let (logical_max, completion) = (call.logical_max, call.next_token_offset);
    if done.prompt_tokens != call.prompt_tokens {
        return Err(format!(
            "DoneEvent prompt count {} does not match request count {}",
            done.prompt_tokens, call.prompt_tokens
        ));
    }
    if done.completion_tokens > logical_max {
        return Err(format!(
            "DoneEvent completion count {} exceeds logical maximum {logical_max}",
            done.completion_tokens
        ));
    }
    if done.completion_tokens != completion {
        return Err(format!(
            "DoneEvent completion count {} does not match streamed count {completion}",
            done.completion_tokens
        ));
    }
    if call.start.is_none() && !(call.cancel_requested && done.reason == FinishReason::Cancelled && completion == 0) {
        return Err("DoneEvent arrived before StartEvent".into());
    }
    // Only a bound of the engine's own ends a request with LENGTH short of its logical maximum.
    if done.reason == FinishReason::Length && (if done.bounded { completion >= logical_max } else { completion != logical_max }) {
        return Err(format!(
            "length-finished DoneEvent has {completion} tokens; expected {}logical maximum {logical_max}",
            if done.bounded { "fewer than " } else { "" }
        ));
    }
    if call.score_tokens != 0 {
        if done.decode_micros != 0 {
            return Err("score DoneEvent reported autoregressive decoding".into());
        }
        match done.reason {
            FinishReason::Stop if done.option_logits.len() != call.score_tokens as usize => {
                return Err(format!(
                    "score DoneEvent returned {} option logits; expected {}",
                    done.option_logits.len(),
                    call.score_tokens
                ))
            }
            FinishReason::Cancelled if !done.option_logits.is_empty() => {
                return Err("unfinished score DoneEvent returned option logits".into())
            }
            FinishReason::Length => return Err("score DoneEvent has an invalid finish reason".into()),
            _ => {}
        }
    } else if !done.option_logits.is_empty() {
        return Err("DoneEvent returned option logits for a generation request".into());
    }
    Ok(())
}
