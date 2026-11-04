//! A scripted engine speaking the real protocol over in-memory pipes, for tests of everything above the engine (the
//! HTTP server end to end included). It behaves as the native engine does where a client can tell:
//! - READY first; for each REQUEST, START, then (when asked) PROMPT_PROGRESS in several prefill steps, then the
//!   script's tokens for that prompt in batches. It ends with STOP at the stop token (streamed) or the script's end,
//!   LENGTH at the output limit, and CANCELLED on CANCEL, after one batch still in flight if the request was decoding.
//! - A TOKEN_MASK request decodes as speculative decoding does: a one-row mask picks the next token (the anchor), then
//!   a verify mask carries `[anchor, drafts...]` and returns one row per position; drafts the rows allow are accepted,
//!   the first one refused is replaced by the lowest allowed token, and a full acceptance adds a bonus token. Where the
//!   scripted token is not allowed, the lowest allowed one is taken.
//! - Status answers carry the schema-6 sections (`weights`, `disk`, `admission`, `scheduler`, ...), with counts kept.
//!
//! Concurrent requests interleave batch by batch.
//!
//! ```ignore
//! let (engine, fake) = FakeEngine::builder().context(32768).script(|prompt| vec![5, 6, 2]).stop_token(2).build();
//! ```
use super::{Engine, EngineOptions};
use crate::protocol::{
    decode_client_frame, encode_event, ClientMessage, ConstraintMode, DoneEvent, EngineEvent, ErrorEvent, FailureClass,
    FinishReason, FrameParser, MaskRequestEvent, PromptProgressEvent, ReadyEvent, RequestFlags, RequestFrame,
    StartEvent, StatusJsonEvent, TokensEvent, MAX_SIMULATION_TOKENS, STATUS_SCHEMA_VERSION,
};
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

type Script = Arc<dyn Fn(&[u32]) -> Vec<u32> + Send + Sync>;

/// Prefill reports this many progress steps (fewer for short prompts).
const PREFILL_STEPS: u32 = 3;

pub struct FakeEngine;

impl FakeEngine {
    pub fn builder() -> FakeEngineBuilder {
        FakeEngineBuilder {
            capacity: 4,
            context: 32768,
            vision: false,
            script: Arc::new(|prompt: &[u32]| prompt.to_vec()),
            batch: 4,
            stop_token: None,
            vocab: 256,
            drafts: 2,
            token_delay: Duration::ZERO,
            send_ready: true,
            disk: (0, 0),
            options: EngineOptions { mask_workers: 2, ..Default::default() },
        }
    }
}

pub struct FakeEngineBuilder {
    capacity: u32,
    context: u32,
    vision: bool,
    script: Script,
    batch: usize,
    stop_token: Option<u32>,
    vocab: u32,
    drafts: usize,
    token_delay: Duration,
    send_ready: bool,
    disk: (u64, u64),
    options: EngineOptions,
}

impl FakeEngineBuilder {
    /// READY's max_concurrent_requests; requests past it wait for a lane.
    pub fn capacity(mut self, requests: u32) -> Self {
        self.capacity = requests;
        self
    }

    pub fn context(mut self, tokens: u32) -> Self {
        self.context = tokens;
        self
    }

    pub fn vision(mut self, vision: bool) -> Self {
        self.vision = vision;
        self
    }

    /// The tokens a prompt generates (default: the prompt echoed back).
    pub fn script(mut self, script: impl Fn(&[u32]) -> Vec<u32> + Send + Sync + 'static) -> Self {
        self.script = Arc::new(script);
        self
    }

    /// Tokens per TOKENS event for unconstrained requests (default 4).
    pub fn batch(mut self, tokens: usize) -> Self {
        self.batch = tokens.max(1);
        self
    }

    /// Generating this token ends the request with STOP (it is streamed, as the engine streams its stop tokens).
    pub fn stop_token(mut self, token: u32) -> Self {
        self.stop_token = Some(token);
        self
    }

    /// The vocabulary masks cover: `words_per_mask = ceil(vocab / 32)` (default 256).
    pub fn vocab(mut self, tokens: u32) -> Self {
        self.vocab = tokens.max(1);
        self
    }

    /// Draft tokens a constrained request verifies per round (default 2); 0 asks one-row masks only.
    pub fn drafts(mut self, tokens: usize) -> Self {
        self.drafts = tokens.min(MAX_SIMULATION_TOKENS - 1);
        self
    }

    /// A pause between generation steps, so tests can act mid-request (default none).
    pub fn token_delay(mut self, delay: Duration) -> Self {
        self.token_delay = delay;
        self
    }

    /// Start without READY; `FakeHandle::send_ready` sends it.
    pub fn without_ready(mut self) -> Self {
        self.send_ready = false;
        self
    }

    /// The status JSON's `disk.used_bytes` and `disk.capacity_bytes`.
    pub fn disk(mut self, used_bytes: u64, capacity_bytes: u64) -> Self {
        self.disk = (used_bytes, capacity_bytes);
        self
    }

    pub fn options(mut self, options: EngineOptions) -> Self {
        self.options = options;
        self
    }

    pub fn build(self) -> (Engine, FakeHandle) {
        let to_engine = Pipe::default();
        let to_server = Pipe::default();
        let (tx, rx) = crossbeam_channel::unbounded();
        let log = Arc::new((Mutex::new(FakeLog::default()), Condvar::new()));
        let (a, b) = (to_engine.clone(), to_server.clone());
        let engine = Engine::from_streams(
            PipeReader(to_server.clone()),
            PipeWriter(to_engine.clone()),
            move || {
                a.close();
                b.close();
            },
            self.options.clone(),
        );
        let input = PipeReader(to_engine.clone());
        let input_tx = tx.clone();
        std::thread::Builder::new().name("fake-engine-input".into()).spawn(move || read_client(input, input_tx)).unwrap();
        let core = Core {
            ready: ReadyEvent { max_concurrent_requests: self.capacity, max_context_tokens: self.context, vision: self.vision },
            words_per_mask: self.vocab.div_ceil(32),
            out: PipeWriter(to_server.clone()),
            log: log.clone(),
            active: Vec::new(),
            waiting: VecDeque::new(),
            paused: false,
            mute_status: false,
            released: false,
            restores: 0,
            counts: Counts::default(),
            next_mask_id: 1,
            started: Instant::now(),
            builder: self,
        };
        let pipes = (to_engine, to_server);
        std::thread::Builder::new().name("fake-engine".into()).spawn(move || core.run(rx)).unwrap();
        (engine, FakeHandle { control: tx, log, pipes })
    }
}

/// What the fake engine received, for assertions.
#[derive(Clone, Debug, Default)]
pub struct FakeLog {
    pub requests: Vec<RequestFrame>,
    pub cancels: Vec<u64>,
    /// (request id, mask request id, words): `words_per_mask * rows` words each.
    pub mask_responses: Vec<(u64, u64, Vec<u32>)>,
    /// Every MASK_REQUEST sent: (request id, simulation tokens).
    pub mask_requests: Vec<(u64, Vec<u32>)>,
    pub status_requests: Vec<u64>,
    pub release_requests: Vec<u64>,
    /// Client frames the engine refused: (request id, issue name).
    pub refused: Vec<(u64, String)>,
}

/// Drives the fake engine from a test. Every method returns at once; the engine acts in order on its own thread.
/// The fake runs as long as its `Engine` does: dropping the handle leaves it serving.
pub struct FakeHandle {
    control: Sender<Message>,
    log: Arc<(Mutex<FakeLog>, Condvar)>,
    pipes: (Pipe, Pipe),
}

impl FakeHandle {
    /// The engine dies: both pipes close, so the server reads EOF.
    pub fn crash(&self) {
        self.pipes.0.close();
        self.pipes.1.close();
        let _ = self.control.send(Message::Stop);
    }

    /// Send an ERROR frame (an engine-level one has request_id 0 and is not a REQUEST_ERROR).
    pub fn send_error(&self, failure_class: FailureClass, request_id: u64, code: &str, message: &str) {
        self.send_event(EngineEvent::Error(ErrorEvent {
            failure_class,
            request_id,
            retryable: false,
            code: code.as_bytes().to_vec(),
            message: message.as_bytes().to_vec(),
            retry_after_seconds: 0,
        }));
    }

    /// Send any event, valid or not.
    pub fn send_event(&self, event: EngineEvent) {
        self.send_raw(encode_event(&event));
    }

    /// Send raw bytes on the engine's stdout.
    pub fn send_raw(&self, bytes: Vec<u8>) {
        let _ = self.control.send(Message::Raw(bytes));
    }

    /// Send READY now (after `without_ready`).
    pub fn send_ready(&self) {
        let _ = self.control.send(Message::SendReady);
    }

    /// Hold generation: requests start but produce no tokens until `resume`.
    pub fn pause(&self) {
        let _ = self.control.send(Message::Pause(true));
    }

    pub fn resume(&self) {
        let _ = self.control.send(Message::Pause(false));
    }

    /// Leave status requests unanswered, as a stuck loop does.
    pub fn mute_status(&self, mute: bool) {
        let _ = self.control.send(Message::MuteStatus(mute));
    }

    pub fn log(&self) -> FakeLog {
        self.log.0.lock().unwrap().clone()
    }

    /// Wait until `condition` holds for the log; false on timeout.
    pub fn wait_for(&self, timeout: Duration, condition: impl Fn(&FakeLog) -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        let mut log = self.log.0.lock().unwrap();
        while !condition(&log) {
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            log = self.log.1.wait_timeout(log, deadline - now).unwrap().0;
        }
        true
    }
}

enum Message {
    Client(ClientMessage),
    Refused(u64, FailureClass, String, String),
    Raw(Vec<u8>),
    SendReady,
    Pause(bool),
    MuteStatus(bool),
    Stop,
}

/// The engine's stdin reader: client frames in, as the engine parses them.
fn read_client(mut input: PipeReader, tx: Sender<Message>) {
    let mut parser = FrameParser::client_frames();
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let n = match input.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        let mut offset = 0;
        while offset < n {
            let step = parser.consume(&buffer[offset..n]);
            offset += step.consumed;
            if let Some(issue) = step.issue {
                let _ = tx.send(Message::Refused(0, issue.failure_class, issue.code.name().into(), issue.message));
                return;
            }
            let Some(frame) = step.frame else { continue };
            let message = match decode_client_frame(&frame) {
                Ok(message) => Message::Client(message),
                Err(e) => {
                    Message::Refused(e.issue.request_id, e.issue.failure_class, e.issue.code.name().into(), e.issue.message)
                }
            };
            if tx.send(message).is_err() {
                return;
            }
        }
    }
    // The engine exits when its stdin closes.
    let _ = tx.send(Message::Stop);
}

struct Active {
    frame: RequestFrame,
    lane: u32,
    plan: VecDeque<u32>,
    /// Prefill progress still to report, in processed tokens.
    progress: VecDeque<u32>,
    emitted: Vec<u32>,
    /// A token picked under a one-row mask, verified with the drafts that follow it.
    anchor: Option<u32>,
    /// The outstanding MASK_REQUEST: its id and simulation tokens.
    awaiting_mask: Option<(u64, Vec<u32>)>,
    drafted: u32,
    accepted: u32,
    cycles: u32,
    started: Instant,
}

#[derive(Default)]
struct Counts {
    submitted: u64,
    completed: u64,
    cancelled: u64,
    failed: u64,
    prompt_tokens: u64,
    output_tokens: u64,
}

struct Core {
    builder: FakeEngineBuilder,
    ready: ReadyEvent,
    words_per_mask: u32,
    out: PipeWriter,
    log: Arc<(Mutex<FakeLog>, Condvar)>,
    active: Vec<Active>,
    waiting: VecDeque<RequestFrame>,
    paused: bool,
    mute_status: bool,
    released: bool,
    restores: u64,
    counts: Counts,
    next_mask_id: u64,
    started: Instant,
}

/// The core stops when the pipe to the server is gone.
struct Gone;

fn allowed(row: &[u32], token: u32) -> bool {
    row.get(token as usize / 32).is_some_and(|w| w >> (token % 32) & 1 == 1)
}

fn lowest_allowed(row: &[u32]) -> Option<u32> {
    (0..row.len() as u32 * 32).find(|&t| allowed(row, t))
}

impl Core {
    fn run(mut self, rx: Receiver<Message>) {
        if self.builder.send_ready && self.send(EngineEvent::Ready(self.ready)).is_err() {
            return;
        }
        let _ = self.serve(rx);
        // Dropping `out` ends the server's stream.
    }

    fn serve(&mut self, rx: Receiver<Message>) -> Result<(), Gone> {
        let delay = self.builder.token_delay;
        loop {
            loop {
                match rx.try_recv() {
                    Ok(message) => self.handle(message)?,
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => return Ok(()),
                }
            }
            let can_step = !self.paused && self.active.iter().any(|a| a.awaiting_mask.is_none());
            if can_step {
                self.step()?;
                if !delay.is_zero() {
                    match rx.recv_timeout(delay) {
                        Ok(message) => self.handle(message)?,
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => return Ok(()),
                    }
                }
            } else {
                match rx.recv() {
                    Ok(message) => self.handle(message)?,
                    Err(_) => return Ok(()),
                }
            }
        }
    }

    fn send(&mut self, event: EngineEvent) -> Result<(), Gone> {
        self.out.write_all(&encode_event(&event)).map_err(|_| Gone)
    }

    fn record(&self, change: impl FnOnce(&mut FakeLog)) {
        change(&mut self.log.0.lock().unwrap());
        self.log.1.notify_all();
    }

    fn handle(&mut self, message: Message) -> Result<(), Gone> {
        match message {
            Message::Client(ClientMessage::Request(frame)) => {
                self.record(|log| log.requests.push(frame.clone()));
                self.counts.submitted += 1;
                // A request restores released weights.
                if std::mem::take(&mut self.released) {
                    self.restores += 1;
                }
                if (self.active.len() as u32) < self.ready.max_concurrent_requests {
                    self.start(frame)?;
                } else {
                    self.waiting.push_back(frame);
                }
            }
            Message::Client(ClientMessage::Cancel(cancel)) => {
                let id = cancel.request_id;
                self.record(|log| log.cancels.push(id));
                if let Some(index) = self.active.iter().position(|a| a.frame.request_id == id) {
                    self.in_flight(index)?;
                    self.finish(index, FinishReason::Cancelled)?;
                } else if let Some(index) = self.waiting.iter().position(|f| f.request_id == id) {
                    let frame = self.waiting.remove(index).unwrap();
                    self.counts.cancelled += 1;
                    self.send(EngineEvent::Done(done(&frame, 0, FinishReason::Cancelled, Instant::now())))?;
                }
            }
            Message::Client(ClientMessage::MaskResponse(response)) => {
                self.record(|log| {
                    log.mask_responses.push((response.request_id, response.mask_request_id, response.mask_words.clone()))
                });
                let Some(index) = self.active.iter().position(|a| {
                    a.frame.request_id == response.request_id
                        && a.awaiting_mask.as_ref().is_some_and(|(id, _)| *id == response.mask_request_id)
                }) else {
                    return Ok(());
                };
                let (_, simulation) = self.active[index].awaiting_mask.take().unwrap();
                self.masked(index, simulation, &response.mask_words)?;
            }
            Message::Client(ClientMessage::StatusRequest(request)) => {
                self.record(|log| log.status_requests.push(request.correlation_id));
                if !self.mute_status {
                    self.send_status(request.correlation_id)?;
                }
            }
            Message::Client(ClientMessage::ReleaseRequest(request)) => {
                self.record(|log| log.release_requests.push(request.correlation_id));
                if self.active.is_empty() && self.waiting.is_empty() {
                    self.released = true;
                }
                self.send_status(request.correlation_id)?;
            }
            Message::Refused(request_id, class, code, message) => {
                self.record(|log| log.refused.push((request_id, code.clone())));
                let request_error = class == FailureClass::RequestError && request_id != 0;
                self.send(EngineEvent::Error(ErrorEvent {
                    failure_class: if request_error { class } else { FailureClass::ProtocolFatal },
                    request_id: if request_error { request_id } else { 0 },
                    retryable: false,
                    code: code.into_bytes(),
                    message: message.into_bytes(),
                    retry_after_seconds: 0,
                }))?;
            }
            Message::Raw(bytes) => self.out.write_all(&bytes).map_err(|_| Gone)?,
            Message::SendReady => self.send(EngineEvent::Ready(self.ready))?,
            Message::Pause(paused) => self.paused = paused,
            Message::MuteStatus(mute) => self.mute_status = mute,
            Message::Stop => return Err(Gone),
        }
        Ok(())
    }

    fn start(&mut self, frame: RequestFrame) -> Result<(), Gone> {
        let lane = (0..).find(|lane| !self.active.iter().any(|a| a.lane == *lane)).unwrap();
        let id = frame.request_id;
        let prompt = frame.prompt_tokens.len() as u32;
        self.counts.prompt_tokens += prompt as u64;
        self.send(EngineEvent::Start(StartEvent { request_id: id, lane, matched_prompt_tokens: 0 }))?;
        if !frame.score_tokens.is_empty() {
            // Score requests: one logit per option, the first highest.
            let mut event = done(&frame, 0, FinishReason::Stop, Instant::now());
            event.option_logits = (0..frame.score_tokens.len()).map(|i| -(i as f32)).collect();
            event.decode_micros = 0;
            self.counts.completed += 1;
            return self.send(EngineEvent::Done(event));
        }
        let mut progress: VecDeque<u32> = VecDeque::new();
        if frame.flags.contains(RequestFlags::RETURN_PROGRESS) {
            for step in 1..=PREFILL_STEPS {
                let processed = (prompt * step).div_ceil(PREFILL_STEPS);
                if progress.back() != Some(&processed) {
                    progress.push_back(processed);
                }
            }
        }
        let plan = (self.builder.script)(&frame.prompt_tokens).into();
        self.active.push(Active {
            frame,
            lane,
            plan,
            progress,
            emitted: Vec::new(),
            anchor: None,
            awaiting_mask: None,
            drafted: 0,
            accepted: 0,
            cycles: 0,
            started: Instant::now(),
        });
        Ok(())
    }

    fn start_waiting(&mut self) -> Result<(), Gone> {
        while (self.active.len() as u32) < self.ready.max_concurrent_requests {
            let Some(frame) = self.waiting.pop_front() else { break };
            self.start(frame)?;
        }
        Ok(())
    }

    /// One step for every request not waiting on a mask: a prefill chunk, a batch, or a mask request.
    fn step(&mut self) -> Result<(), Gone> {
        let ids: Vec<u64> = self.active.iter().filter(|a| a.awaiting_mask.is_none()).map(|a| a.frame.request_id).collect();
        for id in ids {
            let Some(index) = self.active.iter().position(|a| a.frame.request_id == id) else { continue };
            let a = &mut self.active[index];
            if let Some(processed) = a.progress.pop_front() {
                let elapsed_micros = a.started.elapsed().as_micros() as u64 + processed as u64;
                self.send(EngineEvent::PromptProgress(PromptProgressEvent { request_id: id, processed_tokens: processed, elapsed_micros }))?;
                continue;
            }
            if a.frame.constraint == ConstraintMode::TokenMask {
                self.constrained_step(index)?;
                continue;
            }
            let room = (a.frame.logical_max_output_tokens as usize).saturating_sub(a.emitted.len());
            let take = room.min(self.builder.batch).min(a.plan.len());
            let batch: Vec<u32> = a.plan.drain(..take).collect();
            self.emit(index, batch)?;
        }
        Ok(())
    }

    fn constrained_step(&mut self, index: usize) -> Result<(), Gone> {
        let stop = self.builder.stop_token;
        let a = &mut self.active[index];
        let simulation = match a.anchor {
            None => {
                if a.plan.is_empty() && stop.is_none() {
                    return self.finish(index, FinishReason::Stop);
                }
                vec![]
            }
            Some(anchor) => {
                // Room for the anchor, the drafts and a correction or bonus token.
                let room = (a.frame.logical_max_output_tokens as usize).saturating_sub(a.emitted.len());
                let drafts = self.builder.drafts.min(a.plan.len()).min(room.saturating_sub(2));
                if drafts == 0 || Some(anchor) == stop {
                    a.anchor = None;
                    return self.emit(index, vec![anchor]);
                }
                std::iter::once(anchor).chain(a.plan.iter().take(drafts).copied()).collect()
            }
        };
        let mask_request_id = self.next_mask_id;
        self.next_mask_id += 1;
        let id = a.frame.request_id;
        a.awaiting_mask = Some((mask_request_id, simulation.clone()));
        self.record(|log| log.mask_requests.push((id, simulation.clone())));
        let event = MaskRequestEvent { request_id: id, mask_request_id, words_per_mask: self.words_per_mask, simulation_tokens: simulation };
        self.send(EngineEvent::MaskRequest(event))
    }

    /// Apply a mask response: pick the anchor (one row) or verify the drafts (one row per simulated token, plus one).
    fn masked(&mut self, index: usize, simulation: Vec<u32>, words: &[u32]) -> Result<(), Gone> {
        let stop = self.builder.stop_token;
        let width = self.words_per_mask as usize;
        let rows: Vec<&[u32]> = words.chunks(width).collect();
        if words.len() != width * (simulation.len() + 1) {
            return self.refuse(index, "invalid_count", "mask response size does not match the mask request");
        }
        // The scripted token under `row`, else the lowest allowed one.
        let choose = |plan: &mut VecDeque<u32>, row: &[u32]| {
            let scripted = plan.pop_front().or(stop);
            scripted.filter(|&t| allowed(row, t)).or_else(|| lowest_allowed(row))
        };
        let a = &mut self.active[index];
        if simulation.is_empty() {
            let Some(token) = choose(&mut a.plan, rows[0]) else {
                return self.refuse(index, "invalid_constraint", "the token mask allows no token");
            };
            if self.builder.drafts == 0 {
                return self.emit(index, vec![token]);
            }
            a.anchor = Some(token);
            return Ok(());
        }
        a.anchor = None;
        a.cycles += 1;
        a.drafted += simulation.len() as u32 - 1;
        let mut out = vec![simulation[0]];
        let mut all_accepted = true;
        for (i, &draft) in simulation[1..].iter().enumerate() {
            a.plan.pop_front();
            if allowed(rows[i + 1], draft) {
                a.accepted += 1;
                out.push(draft);
                if Some(draft) == stop {
                    all_accepted = false;
                    break;
                }
            } else {
                // The target's own token at the first refused position.
                match lowest_allowed(rows[i + 1]) {
                    Some(token) => out.push(token),
                    None => return self.refuse(index, "invalid_constraint", "the token mask allows no token"),
                }
                all_accepted = false;
                break;
            }
        }
        if all_accepted && (!a.plan.is_empty() || stop.is_some()) {
            // Every draft held: the last row also yields the next token.
            if let Some(token) = choose(&mut a.plan, rows[simulation.len()]) {
                out.push(token);
            }
        }
        self.emit(index, out)
    }

    /// One more batch for a decoding request that is being cancelled: tokens already in flight.
    fn in_flight(&mut self, index: usize) -> Result<(), Gone> {
        let stop = self.builder.stop_token;
        let a = &mut self.active[index];
        if a.emitted.is_empty() || a.frame.constraint == ConstraintMode::TokenMask {
            return Ok(());
        }
        let room = (a.frame.logical_max_output_tokens as usize).saturating_sub(a.emitted.len() + 1);
        let batch: Vec<u32> = a.plan.iter().take(room.min(self.builder.batch)).take_while(|&&t| Some(t) != stop).copied().collect();
        if batch.is_empty() {
            return Ok(());
        }
        a.plan.drain(..batch.len());
        let (id, offset) = (a.frame.request_id, a.emitted.len() as u32);
        a.emitted.extend(&batch);
        self.counts.output_tokens += batch.len() as u64;
        self.send(EngineEvent::Tokens(TokensEvent { request_id: id, sequence_offset: offset, tokens: batch }))
    }

    /// Stream `tokens` (cut at the stop token) and end the request if it is over.
    fn emit(&mut self, index: usize, mut tokens: Vec<u32>) -> Result<(), Gone> {
        let stop = self.builder.stop_token;
        let a = &mut self.active[index];
        let ignore_eos = a.frame.flags.contains(RequestFlags::IGNORE_END_OF_SEQUENCE);
        let stopped = match stop.filter(|_| !ignore_eos).and_then(|s| tokens.iter().position(|&t| t == s)) {
            Some(at) => {
                tokens.truncate(at + 1);
                true
            }
            None => false,
        };
        let id = a.frame.request_id;
        if !tokens.is_empty() {
            let offset = a.emitted.len() as u32;
            a.emitted.extend(&tokens);
            self.counts.output_tokens += tokens.len() as u64;
            self.send(EngineEvent::Tokens(TokensEvent { request_id: id, sequence_offset: offset, tokens }))?;
        }
        let a = &self.active[index];
        if stopped {
            self.finish(index, FinishReason::Stop)
        } else if a.emitted.len() as u32 >= a.frame.logical_max_output_tokens {
            self.finish(index, FinishReason::Length)
        } else if a.plan.is_empty() && a.anchor.is_none() && (a.frame.constraint == ConstraintMode::None || stop.is_none()) {
            // The script ran out: end as a stop token would.
            self.finish(index, FinishReason::Stop)
        } else {
            Ok(())
        }
    }

    /// End a request with a request error, as the engine refuses a request it cannot serve.
    fn refuse(&mut self, index: usize, code: &str, message: &str) -> Result<(), Gone> {
        let a = self.active.remove(index);
        self.counts.failed += 1;
        self.send(EngineEvent::Error(ErrorEvent {
            failure_class: FailureClass::RequestError,
            request_id: a.frame.request_id,
            retryable: false,
            code: code.as_bytes().to_vec(),
            message: message.as_bytes().to_vec(),
            retry_after_seconds: 0,
        }))?;
        self.start_waiting()
    }

    fn finish(&mut self, index: usize, reason: FinishReason) -> Result<(), Gone> {
        let a = self.active.remove(index);
        if reason == FinishReason::Cancelled {
            self.counts.cancelled += 1;
        } else {
            self.counts.completed += 1;
        }
        let mut event = done(&a.frame, a.emitted.len() as u32, reason, a.started);
        (event.drafted_tokens, event.accepted_draft_tokens, event.draft_cycles) = (a.drafted, a.accepted, a.cycles);
        self.send(EngineEvent::Done(event))?;
        self.start_waiting()
    }

    /// A status in the schema-6 shape: the sections the server and Heimdall read (`weights.released`,
    /// `disk.used_bytes`, `disk.capacity_bytes`, `ready`, `schema_version`) and the ones /metrics reads, with the
    /// counts this fake keeps and zeros for what it does not model.
    fn send_status(&mut self, correlation_id: u64) -> Result<(), Gone> {
        let (used, capacity) = self.builder.disk;
        let decoding = self.active.iter().filter(|a| a.progress.is_empty() && a.awaiting_mask.is_none()).count();
        let waiting_mask = self.active.iter().filter(|a| a.awaiting_mask.is_some()).count();
        let active_requests: Vec<serde_json::Value> = self
            .active
            .iter()
            .map(|a| {
                serde_json::json!({
                    "id": a.frame.request_id,
                    "phase": if !a.progress.is_empty() { "prefill" } else if a.awaiting_mask.is_some() { "waiting_mask" } else { "decode" },
                    "prompt_processed": if a.progress.is_empty() { a.frame.prompt_tokens.len() } else { 0 },
                    "generated_tokens": a.emitted.len(),
                    "max_new_tokens": a.frame.logical_max_output_tokens,
                    "kv_mode": "resident",
                    "age_ms": a.started.elapsed().as_millis() as u64,
                })
            })
            .collect();
        let c = &self.counts;
        let json = serde_json::json!({
            "schema_version": STATUS_SCHEMA_VERSION,
            "ready": true,
            "maximum_context_tokens": self.ready.max_context_tokens,
            "memory_pressure": "normal",
            "thermal_state": "nominal",
            "admission": {
                "waiting": self.waiting.len(), "waiting_memory": 0, "waiting_concurrency": self.waiting.len(),
                "held_behind_refusal": 0, "restoring": 0, "suspended": 0, "draining": false, "oldest_wait_ms": 0,
            },
            "loop": { "max_tick_ms": 0.0 },
            "memory_actual": { "allocated_bytes": 0, "current_bytes": 0, "peak_bytes": 0 },
            "memory_governor": { "limit_bytes": 0, "charged_bytes": 0, "headroom_bytes": 0, "growth_allowed": true, "denied_reservations": 0 },
            "weights": { "idle_release_seconds": 0, "released": self.released, "restoring": false, "restores": self.restores, "restore_failures": 0 },
            "kv": { "block_tokens": 16, "pages_allocated": 0, "pages_active": 0, "pages_cache": 0, "pages_free": 0, "allocated_bytes": 0, "reclaimable_bytes": 0 },
            "state": { "entries": 0, "pinned": 0, "in_use": 0, "bytes": 0, "active_lanes": self.active.len() },
            "disk": {
                "capacity_bytes": capacity, "limit_bytes": capacity, "used_bytes": used, "file_bytes": used,
                "volume_available_bytes": capacity.saturating_sub(used), "writes_gated": "no", "write_budget_bytes": 0,
                "read_bytes": 0, "written_bytes": 0, "persistent": false,
            },
            "cache": { "hits": 0, "cold_misses": c.submitted, "hit_rate": 0.0, "reused_tokens": 0, "deduplicated_kv_tokens": 0 },
            "scheduler": {
                "queued": self.waiting.len(), "waiting_resources": 0, "waiting_prefix": 0,
                "prefilling": self.active.len() - decoding - waiting_mask, "decoding": decoding, "waiting_mask": waiting_mask,
                "terminal": 0, "prefill_batches": 0, "prefill_rows": 0, "decode_batches": 0,
                "decode_batches_by_width": { "b1": 0, "b2": 0, "b3": 0, "b4": 0 },
                "active_requests": active_requests,
            },
            "requests": { "submitted": c.submitted, "completed": c.completed, "cancelled": c.cancelled, "failed": c.failed },
            "metrics": {
                "ttft_ms": { "p50": 0.0, "p95": 0.0, "samples": 0 }, "itl_ms": { "p50": 0.0, "p95": 0.0, "samples": 0 },
                "prefill_input_tokens": c.prompt_tokens, "decode_output_tokens": c.output_tokens,
                "drafted_tokens": 0, "accepted_draft_tokens": 0, "capacity_failures": 0, "metal_failures": 0,
            },
            "metal": { "healthy": true, "failure_reason": null },
            "uptime_ms": self.started.elapsed().as_millis() as u64,
        });
        self.send(EngineEvent::StatusJson(StatusJsonEvent { correlation_id, json: json.to_string().into_bytes() }))
    }
}

fn done(frame: &RequestFrame, completion_tokens: u32, reason: FinishReason, started: Instant) -> DoneEvent {
    DoneEvent {
        request_id: frame.request_id,
        reason,
        prompt_tokens: frame.prompt_tokens.len() as u32,
        completion_tokens,
        prefill_micros: 1,
        decode_micros: completion_tokens as u64,
        wall_micros: started.elapsed().as_micros() as u64 + 1,
        option_logits: vec![],
        deduplicated_tokens: 0,
        drafted_tokens: 0,
        accepted_draft_tokens: 0,
        draft_cycles: 0,
        bounded: false,
    }
}

// ---- in-memory pipes ----

#[derive(Default)]
struct PipeState {
    bytes: VecDeque<u8>,
    writer_closed: bool,
    /// Closed from either end by a crash or the engine's kill: reads end at once, writes fail.
    broken: bool,
}

#[derive(Clone, Default)]
struct Pipe(Arc<(Mutex<PipeState>, Condvar)>);

impl Pipe {
    fn close(&self) {
        self.0 .0.lock().unwrap().broken = true;
        self.0 .1.notify_all();
    }
}

struct PipeReader(Pipe);
struct PipeWriter(Pipe);

impl Read for PipeReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let (lock, cv) = &*self.0 .0;
        let mut state = lock.lock().unwrap();
        loop {
            if state.broken {
                return Ok(0);
            }
            if !state.bytes.is_empty() {
                let n = buf.len().min(state.bytes.len());
                let (a, b) = state.bytes.as_slices();
                let first = n.min(a.len());
                buf[..first].copy_from_slice(&a[..first]);
                buf[first..n].copy_from_slice(&b[..n - first]);
                state.bytes.drain(..n);
                return Ok(n);
            }
            if state.writer_closed {
                return Ok(0);
            }
            state = cv.wait(state).unwrap();
        }
    }
}

impl Drop for PipeReader {
    fn drop(&mut self) {
        self.0.close();
    }
}

impl Write for PipeWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let (lock, cv) = &*self.0 .0;
        let mut state = lock.lock().unwrap();
        if state.broken {
            return Err(std::io::ErrorKind::BrokenPipe.into());
        }
        state.bytes.extend(buf);
        cv.notify_all();
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for PipeWriter {
    fn drop(&mut self) {
        self.0 .0 .0.lock().unwrap().writer_closed = true;
        self.0 .0 .1.notify_all();
    }
}
