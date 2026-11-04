//! The engine client against the fake engine and small shell children: routing, cancellation, masks, status,
//! failure fan-out, limits and deadlines. No model, no real engine binary.
use splash::engine::fake::FakeEngine;
use splash::engine::{
    wire_deadline, Engine, EngineError, EngineOptions, FailureKind, MaskProvider, RequestEvent, Submission,
};
use splash::protocol::{EngineEvent, FailureClass, FinishReason, TokensEvent};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const WAIT: Duration = Duration::from_secs(5);

fn submission(prompt: Vec<u32>, max_output_tokens: u32) -> Submission {
    Submission { prompt_tokens: prompt, max_output_tokens, ..Default::default() }
}

fn ready(engine: &Engine) {
    engine.wait_ready(Some(WAIT)).expect("ready");
}

fn done(end: &RequestEvent) -> &splash::protocol::DoneEvent {
    match end {
        RequestEvent::Done(done) => done,
        other => panic!("expected Done, got {other:?}"),
    }
}

fn started(request: &splash::engine::Request) {
    match request.events.recv_timeout(WAIT).unwrap() {
        RequestEvent::Started { .. } => {}
        other => panic!("expected Started, got {other:?}"),
    }
}

fn failed(end: &RequestEvent) -> (&str, bool, FailureKind) {
    match end {
        RequestEvent::Failed { message, retryable, kind, .. } => (message.as_str(), *retryable, *kind),
        other => panic!("expected Failed, got {other:?}"),
    }
}

#[test]
fn interleaved_requests_route_to_their_own_channels() {
    let (engine, _fake) = FakeEngine::builder().batch(1).script(|prompt| vec![prompt[0]; 6]).build();
    ready(&engine);
    let requests: Vec<_> = (1..=4).map(|n| engine.submit(submission(vec![n * 100, 1], 64)).unwrap()).collect();
    assert_eq!(requests.iter().map(|r| r.id).collect::<Vec<_>>(), vec![1, 2, 3, 4]);
    for (n, request) in (1..=4).zip(&requests) {
        let collected = request.collect(WAIT).unwrap();
        assert_eq!(collected.tokens, vec![n * 100; 6]);
        let done = done(&collected.end);
        assert_eq!((done.reason, done.prompt_tokens, done.completion_tokens), (FinishReason::Stop, 2, 6));
        assert!(request.is_finished());
    }
    assert_eq!(engine.pending_count(), 0);
}

#[test]
fn output_limit_and_stop_token_end_requests() {
    let (engine, fake) = FakeEngine::builder().script(|_| vec![4, 5, 6, 2, 7]).stop_token(2).build();
    ready(&engine);
    let length = engine.submit(submission(vec![1], 3)).unwrap().collect(WAIT).unwrap();
    assert_eq!((length.tokens, done(&length.end).reason), (vec![4, 5, 6], FinishReason::Length));
    let stop = engine.submit(submission(vec![1], 10)).unwrap().collect(WAIT).unwrap();
    assert_eq!((stop.tokens, done(&stop.end).reason), (vec![4, 5, 6, 2], FinishReason::Stop));
    let ignore = Submission { ignore_eos: true, ..submission(vec![1], 10) };
    let ignored = engine.submit(ignore).unwrap().collect(WAIT).unwrap();
    assert_eq!(ignored.tokens, vec![4, 5, 6, 2, 7]);
    assert!(fake.log().requests[2].flags.contains(splash::protocol::RequestFlags::IGNORE_END_OF_SEQUENCE));
}

#[test]
fn progress_and_score_requests() {
    let (engine, _fake) = FakeEngine::builder().build();
    ready(&engine);
    let request = engine.submit(Submission { return_progress: true, ..submission((1..=9).collect(), 2) }).unwrap();
    started(&request);
    let mut progress = vec![];
    let mut tokens = vec![];
    loop {
        match request.events.recv_timeout(WAIT).unwrap() {
            RequestEvent::Progress { processed_tokens, .. } => {
                assert!(tokens.is_empty(), "prefill progress comes before tokens");
                progress.push(processed_tokens);
            }
            RequestEvent::Tokens(batch) => tokens.extend(batch),
            RequestEvent::Done(_) => break,
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!((progress, tokens), (vec![3, 6, 9], vec![1, 2]));
    let score = Submission { score_tokens: vec![10, 20, 30], ..submission(vec![1, 2], 0) };
    let scored = engine.submit(score).unwrap().collect(WAIT).unwrap();
    assert_eq!(done(&scored.end).option_logits, vec![0.0, -1.0, -2.0]);
}

#[test]
fn cancel_ends_with_cancelled_and_drop_cancels() {
    let (engine, fake) = FakeEngine::builder().build();
    ready(&engine);
    fake.pause();
    let request = engine.submit(submission(vec![1, 2, 3], 16)).unwrap();
    started(&request);
    assert!(request.cancel());
    assert!(!request.cancel(), "a second cancel sends nothing");
    let collected = request.collect(WAIT).unwrap();
    assert_eq!((collected.tokens.len(), done(&collected.end).reason), (0, FinishReason::Cancelled));
    assert!(!request.cancel(), "a finished request cannot be cancelled");

    let dropped = engine.submit(submission(vec![4], 16)).unwrap();
    let id = dropped.id;
    started(&dropped);
    drop(dropped);
    assert!(fake.wait_for(WAIT, |log| log.cancels == vec![1, id]));
    let deadline = Instant::now() + WAIT;
    while engine.pending_count() != 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(engine.pending_count(), 0);
    assert_eq!(engine.failure(), None);
}

/// Allows a fixed token set; records what the engine client asks of it.
#[derive(Default)]
struct Allow {
    allowed: Vec<u32>,
    commits: Mutex<Vec<u32>>,
    masks: AtomicUsize,
    finishes: AtomicUsize,
    fail_masks: bool,
    short_mask: bool,
    fail_finish: bool,
}

impl MaskProvider for Allow {
    fn commit(&self, tokens: &[u32]) -> anyhow::Result<()> {
        self.commits.lock().unwrap().extend(tokens);
        Ok(())
    }

    fn masks(&self, simulation: &[u32], words_per_mask: u32) -> anyhow::Result<Vec<u32>> {
        self.masks.fetch_add(1, Ordering::SeqCst);
        if self.fail_masks {
            anyhow::bail!("grammar exhausted");
        }
        let words = words_per_mask as usize;
        let mut mask = vec![0u32; words * (simulation.len() + 1)];
        for row in 0..=simulation.len() {
            for &t in &self.allowed {
                mask[row * words + t as usize / 32] |= 1 << (t % 32);
            }
        }
        if self.short_mask {
            mask.pop();
        }
        Ok(mask)
    }

    fn finish(&self) -> anyhow::Result<()> {
        self.finishes.fetch_add(1, Ordering::SeqCst);
        if self.fail_finish {
            anyhow::bail!("incomplete JSON");
        }
        Ok(())
    }
}

fn constrained(provider: &Arc<Allow>) -> Submission {
    Submission { constraint: Some(provider.clone()), ..submission(vec![1, 2], 8) }
}

#[test]
fn masks_round_trip_and_steer_tokens() {
    let (engine, fake) = FakeEngine::builder().vocab(64).script(|_| vec![10, 33, 12]).build();
    ready(&engine);
    let provider = Arc::new(Allow { allowed: vec![10, 12, 40], ..Default::default() });
    let collected = engine.submit(constrained(&provider)).unwrap().collect(WAIT).unwrap();
    // The anchor 10 is verified with drafts [33, 12]; 33 is refused and replaced by the lowest allowed token.
    assert_eq!(collected.tokens, vec![10, 10, 12]);
    let first = done(&collected.end);
    assert_eq!(first.reason, FinishReason::Stop);
    assert_eq!((first.drafted_tokens, first.accepted_draft_tokens, first.draft_cycles), (2, 0, 1));
    assert_eq!(*provider.commits.lock().unwrap(), vec![10, 10, 12]);
    assert_eq!(provider.masks.load(Ordering::SeqCst), 3);
    assert_eq!(provider.finishes.load(Ordering::SeqCst), 1);
    let log = fake.log();
    assert_eq!(log.requests[0].constraint, splash::protocol::ConstraintMode::TokenMask);
    let simulations: Vec<Vec<u32>> = log.mask_requests.iter().map(|(_, sim)| sim.clone()).collect();
    assert_eq!(simulations, vec![vec![], vec![10, 33, 12], vec![]]);
    let sizes: Vec<usize> = log.mask_responses.iter().map(|(_, _, words)| words.len()).collect();
    assert_eq!(sizes, vec![2, 8, 2], "words_per_mask x rows");

    // All drafts accepted: the last row adds a bonus token.
    let (engine, fake) = FakeEngine::builder().vocab(64).script(|_| vec![10, 12, 40, 12]).build();
    ready(&engine);
    let collected = engine.submit(constrained(&provider)).unwrap().collect(WAIT).unwrap();
    assert_eq!(collected.tokens, vec![10, 12, 40, 12]);
    assert_eq!(fake.log().mask_requests[1].1, vec![10, 12, 40]);
    assert_eq!(done(&collected.end).accepted_draft_tokens, 2);
}

#[test]
fn mask_failures_end_only_their_request() {
    let (engine, fake) = FakeEngine::builder().vocab(64).script(|_| vec![10]).build();
    ready(&engine);
    let failing = Arc::new(Allow { allowed: vec![10], fail_masks: true, ..Default::default() });
    let end = engine.submit(constrained(&failing)).unwrap().collect(WAIT).unwrap().end;
    assert_eq!(failed(&end), ("mask computation failed: grammar exhausted", false, FailureKind::Constraint));
    assert_eq!(failing.finishes.load(Ordering::SeqCst), 0);

    let short = Arc::new(Allow { allowed: vec![10], short_mask: true, ..Default::default() });
    let end = engine.submit(constrained(&short)).unwrap().collect(WAIT).unwrap().end;
    assert_eq!(failed(&end), ("mask provider returned 1 words; expected 2", false, FailureKind::Constraint));

    let unfinished = Arc::new(Allow { allowed: vec![10], fail_finish: true, ..Default::default() });
    let end = engine.submit(constrained(&unfinished)).unwrap().collect(WAIT).unwrap().end;
    assert_eq!(failed(&end), ("incomplete JSON", false, FailureKind::Constraint));

    // The mask failures cancelled their requests; the engine still serves.
    assert!(fake.wait_for(WAIT, |log| log.cancels == vec![1, 2]));
    assert_eq!(engine.failure(), None);
    let ok = engine.submit(submission(vec![3], 2)).unwrap().collect(WAIT).unwrap();
    assert_eq!(ok.tokens, vec![10]);
}

#[test]
fn status_and_weight_release() {
    let (engine, fake) = FakeEngine::builder().disk(1024, 4096).build();
    ready(&engine);
    let status = engine.status(WAIT).unwrap();
    assert_eq!(status["schema_version"], 6);
    for section in ["admission", "weights", "disk", "scheduler", "requests", "metrics", "memory_governor", "kv", "cache"] {
        assert!(status[section].is_object(), "status has {section}");
    }
    assert_eq!(status["weights"]["released"], false);
    assert_eq!((status["disk"]["used_bytes"].as_u64(), status["disk"]["capacity_bytes"].as_u64()), (Some(1024), Some(4096)));
    assert_eq!(engine.release_weights(WAIT).unwrap()["weights"]["released"], true);
    engine.submit(submission(vec![1], 1)).unwrap().collect(WAIT).unwrap();
    assert_eq!(engine.status(WAIT).unwrap()["weights"]["released"], false);
    let log = fake.log();
    assert_eq!((log.status_requests, log.release_requests), (vec![1, 3], vec![2]));

    fake.mute_status(true);
    let error = engine.status(Duration::from_millis(50)).unwrap_err();
    assert_eq!(error, EngineError::Timeout("native status response timed out".into()));
    // Its late answer is not a protocol break.
    fake.mute_status(false);
    assert!(engine.status(WAIT).is_ok());
}

#[test]
fn engine_error_fails_every_pending_request_and_later_calls() {
    let (engine, fake) = FakeEngine::builder().build();
    ready(&engine);
    fake.pause();
    let a = engine.submit(submission(vec![1], 4)).unwrap();
    let b = engine.submit(submission(vec![2], 4)).unwrap();
    started(&a);
    started(&b);
    fake.send_error(FailureClass::EngineUnhealthy, 0, "metal_failure", "GPU fault");
    for request in [&a, &b] {
        let end = request.collect(WAIT).unwrap().end;
        assert_eq!(failed(&end), ("GPU fault", true, FailureKind::Engine));
    }
    assert_eq!(engine.failure(), Some(EngineError::Unhealthy("GPU fault".into())));
    assert_eq!(engine.ready(), None);
    assert_eq!(engine.submit(submission(vec![1], 4)).unwrap_err(), EngineError::Unhealthy("GPU fault".into()));
    assert_eq!(engine.status(WAIT).unwrap_err(), EngineError::Unhealthy("GPU fault".into()));
}

#[test]
fn eof_and_protocol_breaks_fail_the_engine() {
    let (engine, fake) = FakeEngine::builder().build();
    ready(&engine);
    fake.pause();
    let request = engine.submit(submission(vec![1], 4)).unwrap();
    started(&request);
    fake.crash();
    assert_eq!(failed(&request.collect(WAIT).unwrap().end).2, FailureKind::Engine);
    assert_eq!(engine.failure(), Some(EngineError::Unhealthy("native protocol reached EOF".into())));

    let (engine, fake) = FakeEngine::builder().build();
    ready(&engine);
    fake.send_event(EngineEvent::Tokens(TokensEvent { request_id: 99, sequence_offset: 0, tokens: vec![1] }));
    assert_eq!(
        engine.failure_wait(),
        Some(EngineError::ProtocolFatal("native event references unknown request 99".into()))
    );

    let (engine, fake) = FakeEngine::builder().without_ready().build();
    fake.send_raw(b"XXXXXXXXXXXXXXXXXXXX".to_vec());
    assert_eq!(
        engine.wait_ready(Some(WAIT)).unwrap_err(),
        EngineError::ProtocolFatal("protocol_fatal:bad_magic: frame magic is not SPLH".into())
    );
}

trait FailureWait {
    fn failure_wait(&self) -> Option<EngineError>;
}

impl FailureWait for Engine {
    fn failure_wait(&self) -> Option<EngineError> {
        let deadline = Instant::now() + WAIT;
        while self.failure().is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
        }
        self.failure()
    }
}

#[test]
fn request_errors_from_the_engine_and_from_validation() {
    let (engine, fake) = FakeEngine::builder().build();
    ready(&engine);
    fake.pause();
    let request = engine.submit(submission(vec![1], 4)).unwrap();
    started(&request);
    fake.send_event(EngineEvent::Error(splash::protocol::ErrorEvent {
        failure_class: FailureClass::RequestError,
        request_id: request.id,
        retryable: true,
        code: b"kv_disk_quota".to_vec(),
        message: b"the SSD cache is full".to_vec(),
        retry_after_seconds: 7,
    }));
    match request.collect(WAIT).unwrap().end {
        RequestEvent::Failed { message, retryable, retry_after_seconds, error, kind } => {
            assert_eq!((message.as_str(), retryable, retry_after_seconds, kind), ("the SSD cache is full", true, 7, FailureKind::Request));
            assert_eq!(error.unwrap().code_text(), "kv_disk_quota");
        }
        other => panic!("expected Failed, got {other:?}"),
    }
    let invalid = engine.submit(submission(vec![1], 0)).unwrap_err();
    assert_eq!(
        invalid,
        EngineError::Request { code: "limit_exceeded".into(), message: "logical max output token count exceeds its limit".into() }
    );
    assert_eq!(engine.failure(), None);
}

#[test]
fn pending_limit_refuses_past_the_limit_and_frees_slots() {
    let options = EngineOptions { pending_limit: 2, mask_workers: 1, ..Default::default() };
    let (engine, fake) = FakeEngine::builder().options(options).build();
    ready(&engine);
    fake.pause();
    let a = engine.submit(submission(vec![1], 4)).unwrap();
    let _b = engine.submit(submission(vec![2], 4)).unwrap();
    assert_eq!(engine.submit(submission(vec![3], 4)).unwrap_err(), EngineError::PendingLimit(2));
    a.cancel();
    a.collect(WAIT).unwrap();
    // A refused submission (here an invalid one) gives its slot back.
    assert!(matches!(engine.submit(submission(vec![], 4)), Err(EngineError::Request { .. })));
    let c = engine.submit(submission(vec![3], 4)).unwrap();
    assert_eq!(c.id, 5, "ids are never reused, refused ones included");
    assert_eq!(engine.submit(submission(vec![3], 4)).unwrap_err(), EngineError::PendingLimit(2));
    fake.resume();
    assert_eq!(c.collect(WAIT).unwrap().tokens, vec![3]);
}

#[test]
fn deadlines_are_stamped_like_python() {
    assert_eq!(wire_deadline(None, 1_000), (u64::MAX, u64::MAX - 1_000));
    assert_eq!(wire_deadline(Some(Duration::from_secs(2)), 1_000), (2_001_000, 2_000_000));
    assert_eq!(wire_deadline(Some(Duration::from_nanos(10)), 1_000), (1_001, 1));
    assert_eq!(wire_deadline(Some(Duration::MAX), 1_000), (u64::MAX, u64::MAX - 1_000));

    let (engine, fake) = FakeEngine::builder().build();
    ready(&engine);
    let now = || SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_micros() as u64;
    let before = now();
    engine.submit(Submission { timeout: Some(Duration::from_secs(30)), ..submission(vec![1], 1) }).unwrap().collect(WAIT);
    let after = now();
    engine.submit(submission(vec![1], 1)).unwrap().collect(WAIT);
    let log = fake.log();
    let timed = &log.requests[0];
    assert_eq!(timed.remaining_deadline_micros, 30_000_000);
    assert!((before + 30_000_000..=after + 30_000_000).contains(&timed.absolute_deadline_unix_micros));
    let open = &log.requests[1];
    assert_eq!(open.absolute_deadline_unix_micros, u64::MAX);
    assert!(open.remaining_deadline_micros > u64::MAX - after - 1_000_000);
    let expired = engine.submit(Submission { timeout: Some(Duration::ZERO), ..submission(vec![1], 1) });
    assert_eq!(expired.unwrap_err(), EngineError::Timeout("native operation timed out".into()));
}

#[test]
fn cancel_after_tokens_streams_tokens_in_flight_first() {
    let (engine, fake) = FakeEngine::builder().batch(1).token_delay(Duration::from_millis(20)).script(|_| (10..60).collect()).build();
    ready(&engine);
    let request = engine.submit(submission(vec![1], 64)).unwrap();
    started(&request);
    assert!(matches!(request.events.recv_timeout(WAIT).unwrap(), RequestEvent::Tokens(_)));
    request.cancel();
    let collected = request.collect(WAIT).unwrap();
    let done = done(&collected.end);
    assert_eq!(done.reason, FinishReason::Cancelled);
    assert!(!collected.tokens.is_empty(), "a batch in flight arrives after the cancel");
    assert_eq!(done.completion_tokens as usize, collected.tokens.len() + 1);
    assert_eq!(fake.log().cancels, vec![1]);
}

/// Panics in the chosen grammar call.
struct Panicking(&'static str);

impl MaskProvider for Panicking {
    fn commit(&self, _: &[u32]) -> anyhow::Result<()> {
        if self.0 == "commit" {
            panic!("boom");
        }
        Ok(())
    }
    fn masks(&self, simulation: &[u32], words_per_mask: u32) -> anyhow::Result<Vec<u32>> {
        if self.0 == "masks" {
            panic!("boom");
        }
        Ok(vec![u32::MAX; words_per_mask as usize * (simulation.len() + 1)])
    }
    fn finish(&self) -> anyhow::Result<()> {
        if self.0 == "finish" {
            panic!("{}", String::from("boom"));
        }
        Ok(())
    }
}

#[test]
fn grammar_panics_fail_their_request_and_free_mask_slots() {
    // Two mask slots: a slot a panic leaked would fill the queue by the third request.
    let options = EngineOptions { pending_limit: 2, mask_workers: 1, ..Default::default() };
    let (engine, _fake) = FakeEngine::builder().vocab(64).script(|_| vec![5, 6]).options(options).build();
    ready(&engine);
    let run = |provider: Panicking| {
        let submission = Submission { constraint: Some(Arc::new(provider)), ..submission(vec![1], 4) };
        engine.submit(submission).unwrap().collect(WAIT).unwrap()
    };
    for _ in 0..3 {
        let end = run(Panicking("masks")).end;
        assert_eq!(failed(&end), ("mask computation failed: constraint panicked: boom", false, FailureKind::Constraint));
    }
    assert_eq!(failed(&run(Panicking("commit")).end), ("constraint panicked: boom", false, FailureKind::Constraint));
    assert_eq!(failed(&run(Panicking("finish")).end), ("constraint panicked: boom", false, FailureKind::Constraint));
    let ok = run(Panicking("none"));
    assert_eq!((ok.tokens, done(&ok.end).reason), (vec![5, 6], FinishReason::Stop));
    assert_eq!(engine.failure(), None);
}

/// `finish` waits for the test's signal.
struct SlowFinish(Mutex<crossbeam_channel::Receiver<()>>);

impl MaskProvider for SlowFinish {
    fn commit(&self, _: &[u32]) -> anyhow::Result<()> {
        Ok(())
    }
    fn masks(&self, simulation: &[u32], words_per_mask: u32) -> anyhow::Result<Vec<u32>> {
        Ok(vec![u32::MAX; words_per_mask as usize * (simulation.len() + 1)])
    }
    fn finish(&self) -> anyhow::Result<()> {
        let _ = self.0.lock().unwrap().recv();
        Ok(())
    }
}

#[test]
fn finish_runs_off_the_reader_thread() {
    let (engine, _fake) = FakeEngine::builder().vocab(64).script(|_| vec![5, 6]).build();
    ready(&engine);
    let (release, wait) = crossbeam_channel::bounded(0);
    let slow = Submission { constraint: Some(Arc::new(SlowFinish(Mutex::new(wait)))), ..submission(vec![1], 4) };
    let slow = engine.submit(slow).unwrap();
    // While its finish blocks, the reader still serves other requests.
    let other = engine.submit(submission(vec![7, 8], 4)).unwrap().collect(WAIT).unwrap();
    assert_eq!(done(&other.end).reason, FinishReason::Stop);
    std::thread::sleep(Duration::from_millis(50));
    let mut tokens = vec![];
    for event in slow.events.try_iter() {
        match event {
            RequestEvent::Tokens(batch) => tokens.extend(batch),
            RequestEvent::Started { .. } => {}
            other => panic!("Done must wait for finish, got {other:?}"),
        }
    }
    assert_eq!(tokens, vec![5, 6], "every token arrives before Done");
    release.send(()).unwrap();
    assert_eq!(done(&slow.events.recv_timeout(WAIT).unwrap()).reason, FinishReason::Stop);
}

#[test]
fn startup_and_liveness_timeouts_fail_the_engine() {
    let options = EngineOptions { startup_timeout: Duration::from_millis(100), mask_workers: 1, ..Default::default() };
    let (engine, _fake) = FakeEngine::builder().without_ready().options(options).build();
    assert_eq!(engine.wait_ready(Some(WAIT)).unwrap_err(), EngineError::Unhealthy("native ReadyEvent timed out".into()));

    let options = EngineOptions {
        liveness_interval: Duration::from_millis(30),
        status_answer_limit: Duration::from_millis(100),
        mask_workers: 1,
        ..Default::default()
    };
    let (engine, fake) = FakeEngine::builder().options(options.clone()).build();
    ready(&engine);
    fake.pause();
    let request = engine.submit(submission(vec![1], 4)).unwrap();
    // An answered probe keeps the engine; a stuck loop fails it.
    assert!(fake.wait_for(WAIT, |log| log.status_requests.len() >= 2));
    assert_eq!(engine.failure(), None);
    fake.mute_status(true);
    let end = request.collect(WAIT).unwrap().end;
    assert_eq!(failed(&end).0, "native loop did not answer status within 0.1 s");

    // Idle too: with nothing pending the probe still runs, and `failure()` reports a loop that stopped answering.
    let (engine, fake) = FakeEngine::builder().options(options).build();
    ready(&engine);
    assert!(fake.wait_for(WAIT, |log| log.status_requests.len() >= 2));
    assert_eq!(engine.failure(), None);
    fake.mute_status(true);
    assert_eq!(
        engine.failure_wait(),
        Some(EngineError::Unhealthy("native loop did not answer status within 0.1 s".into()))
    );
}

#[test]
fn shutdown_fails_pending_requests_as_closed() {
    let (engine, fake) = FakeEngine::builder().build();
    ready(&engine);
    fake.pause();
    let request = engine.submit(submission(vec![1], 4)).unwrap();
    started(&request);
    engine.shutdown(Duration::from_secs(1));
    assert_eq!(failed(&request.collect(WAIT).unwrap().end), ("runtime is closed", false, FailureKind::Engine));
    assert_eq!(engine.failure(), Some(EngineError::Closed));
    assert_eq!(engine.submit(submission(vec![1], 4)).unwrap_err(), EngineError::Closed);
}

/// A writer that accepts nothing until the engine's kill hook lets it fail.
struct Stuck(crossbeam_channel::Receiver<()>);

impl std::io::Write for Stuck {
    fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
        let _ = self.0.recv();
        Err(std::io::ErrorKind::BrokenPipe.into())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn stalled_writes_abandon_unstarted_frames_and_fail_started_ones() {
    let ready_frame = splash::protocol::encode_event(&EngineEvent::Ready(splash::protocol::ReadyEvent {
        max_concurrent_requests: 2,
        max_context_tokens: 4096,
        vision: false,
    }));
    let (release, stuck) = crossbeam_channel::bounded::<()>(0);
    let (reader_tx, reader_rx) = crossbeam_channel::bounded::<()>(0);
    let reader = std::io::Read::chain(std::io::Cursor::new(ready_frame), Blocking(reader_rx));
    let options = EngineOptions { io_timeout: Duration::from_millis(300), mask_workers: 1, ..Default::default() };
    let engine = Arc::new(Engine::from_streams(reader, Stuck(stuck), move || drop((release, reader_tx)), options));
    ready(&engine);
    let first = {
        let engine = engine.clone();
        std::thread::spawn(move || engine.submit(submission(vec![1], 4)).map(|r| r.id))
    };
    std::thread::sleep(Duration::from_millis(50));
    let queued = engine.submit(Submission { timeout: Some(Duration::from_millis(50)), ..submission(vec![2], 4) });
    assert_eq!(queued.unwrap_err(), EngineError::Timeout("native write lock timed out".into()));
    let stalled = EngineError::Unhealthy("native frame write timed out after 0 bytes".into());
    assert_eq!(first.join().unwrap().unwrap_err(), stalled);
    assert_eq!(engine.failure(), Some(stalled));
}

/// A reader that blocks until its sender is dropped, then reports EOF.
struct Blocking(crossbeam_channel::Receiver<()>);

impl std::io::Read for Blocking {
    fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
        let _ = self.0.recv();
        Ok(0)
    }
}

/// `printf` arguments for a READY frame (octal escapes, which POSIX printf takes).
fn ready_printf() -> String {
    let frame = splash::protocol::encode_event(&EngineEvent::Ready(splash::protocol::ReadyEvent {
        max_concurrent_requests: 3,
        max_context_tokens: 8192,
        vision: true,
    }));
    frame.iter().map(|b| format!("\\{b:03o}")).collect()
}

fn sh(script: String) -> Vec<std::ffi::OsString> {
    vec!["/bin/sh".into(), "-c".into(), script.into()]
}

#[test]
fn spawned_children_are_read_and_stopped() {
    let missing = Engine::spawn(vec!["/nonexistent/splash".into()], WAIT).err().unwrap();
    assert!(missing.to_string().starts_with("native engine executable is missing"));

    // Exits when its stdin closes.
    let engine = Engine::spawn(sh(format!("printf '{}'; exec cat >/dev/null", ready_printf())), WAIT).unwrap();
    let ready = engine.wait_ready(Some(WAIT)).unwrap();
    assert_eq!((ready.max_concurrent_requests, ready.max_context_tokens, ready.vision), (3, 8192, true));
    let started = Instant::now();
    engine.shutdown(Duration::from_secs(5));
    assert!(started.elapsed() < Duration::from_secs(2));

    // Ignores stdin: SIGTERM stops it.
    let engine = Engine::spawn(sh(format!("printf '{}'; exec sleep 30", ready_printf())), WAIT).unwrap();
    engine.wait_ready(Some(WAIT)).unwrap();
    let started = Instant::now();
    engine.shutdown(Duration::from_secs(5));
    assert!(started.elapsed() < Duration::from_secs(2));

    // Dropped without shutdown: the child still exits and is reaped (no zombie: kill(pid, 0) fails only once reaped).
    let dir = tempfile::tempdir().unwrap();
    let pid_file = dir.path().join("pid");
    let script = format!("echo $$ > '{}'; printf '{}'; exec sleep 30", pid_file.display(), ready_printf());
    let engine = Engine::spawn(sh(script), WAIT).unwrap();
    engine.wait_ready(Some(WAIT)).unwrap();
    let pid: libc::pid_t = std::fs::read_to_string(&pid_file).unwrap().trim().parse().unwrap();
    drop(engine);
    let deadline = Instant::now() + WAIT;
    // SAFETY: signal 0 only probes the pid.
    while unsafe { libc::kill(pid, 0) } == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_ne!(unsafe { libc::kill(pid, 0) }, 0, "child {pid} still exists (running or a zombie)");

    // A protocol break fails the engine and terminates the child.
    let engine = Engine::spawn(sh("printf 'XXXXXXXXXXXXXXXX'; exec sleep 30".into()), WAIT).unwrap();
    assert!(matches!(engine.wait_ready(Some(WAIT)), Err(EngineError::ProtocolFatal(m)) if m.contains("bad_magic")));
    let started = Instant::now();
    engine.shutdown(Duration::from_secs(5));
    assert!(started.elapsed() < Duration::from_secs(2));
}
