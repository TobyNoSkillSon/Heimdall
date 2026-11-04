//! One bounded speaker actor per source. Native models stay in its owned helper;
//! neither initial loading nor a diarization/embedding request blocks intake.
use crate::metrics::SpeakerCpu;
use anyhow::{Context, Result};
use speakers::{
    call::{CallScopedSpeakers, QUIET_TIMEOUT},
    diar::SpeakerSegment,
    identity::EnrollmentStore,
    naming::{NamingFlow, SpeakerEvent, TrackId},
    pipeline::Diarization,
};
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
/// Five failures open a five-minute circuit; a healthy minute resets it.
#[derive(Default)]
struct HelperRecovery {
    failures: u32,
    retry: Option<Instant>,
    healthy_since: Option<Instant>,
}
impl HelperRecovery {
    fn ready(&mut self, now: Instant) -> bool {
        if self.retry.is_some_and(|at| now < at) { return false; }
        if self.failures >= 5 { self.failures = 0; }
        self.retry = None;
        true
    }
    fn failed(&mut self, now: Instant) -> Duration {
        if self.healthy_since.take().is_some_and(|at| now.saturating_duration_since(at) >= Duration::from_secs(60)) { self.failures = 0; }
        self.failures += 1;
        let delay = Duration::from_secs(if self.failures >= 5 { 300 } else { 1 << (self.failures - 1) });
        self.retry = Some(now + delay);
        delay
    }
}
pub enum Work {
    Backend(String),
    Audio(Vec<f32>, bool, Instant),
    Ax(Vec<speakers::ax::AxSnapshot>),
    Activate(u64, Instant),
    Observe(TrackId, f64, String, Vec<speakers::ax::AxSnapshot>),
    Finish,
    Store(EnrollmentStore),
    Forget(String),
    Rename(String, String),
}
pub enum Update {
    Stable { origin:u64, until:u64, segments:Vec<SpeakerSegment>, finalized:bool },
    Started {
        origin: u64,
    },
    Snapshot {
        origin: u64,
        segments: Vec<SpeakerSegment>,
        events: Vec<SpeakerEvent>,
    },
    Closed {
        origin: u64,
        segments: Vec<SpeakerSegment>,
        events: Vec<SpeakerEvent>,
    },
    Expired {
        origin: u64,
        segments: Vec<SpeakerSegment>,
        events: Vec<SpeakerEvent>,
    },
    GpuStatus(Option<String>),
    Error(String),
}
pub struct Event {
    pub source: String,
    pub generation: u64,
    pub update: Update,
}
pub struct Actor {
    tx: crossbeam_channel::Sender<Work>,
    cancel: Arc<AtomicBool>,
    pid: Arc<AtomicU32>,
    cpu: Arc<SpeakerCpu>,
    latest_voice: Arc<Mutex<Instant>>,
}
impl Actor {
    #[allow(clippy::too_many_arguments)] // Source clock, model/archive roots and actor resources travel together.
    pub fn start(
        source: String,
        origin: u64,
        models: PathBuf,
        archive: PathBuf,
        mut naming: NamingFlow,
        cpu: Arc<SpeakerCpu>,
        output: crossbeam_channel::Sender<Event>,
        backend: String,
    ) -> Result<Self> {
        naming.enable_discord_ax(source.clone());
        let helper = std::env::current_exe()?.with_file_name("heimdall-speakers-worker");
        let (tx, rx) = crossbeam_channel::bounded(4096); // 81.92 s / 5 MiB at 320-sample packets.
        let cancel = Arc::new(AtomicBool::new(false));
        let pid = Arc::new(AtomicU32::new(0));
        let cancelled = cancel.clone();
        let child = pid.clone();
        let scope = cpu.clone();
        let latest_voice = Arc::new(Mutex::new(Instant::now()));
        let observed_voice = latest_voice.clone();
        std::thread::spawn(move || {
            let mut call = CallScopedSpeakers::new(helper, models, 0, naming);
            if let Err(e) = call.set_metal_backend(&backend) { eprintln!("Speaker backend: {e:#}"); return; }
            let mut current_origin = origin;
            let mut last_frame = -1;
            let archive_evidence = heimdall_archive::Journal::open(&archive);
            let archive_segments = std::cell::RefCell::new((0, Vec::new()));
            let emit = |update| {
                if matches!(
                    &update,
                    Update::Snapshot { .. } | Update::Closed { .. } | Update::Expired { .. }
                ) {
                    let journal = archive_evidence
                        .as_ref()
                        .map_err(|e| anyhow::anyhow!("speaker archive: {e:#}"))?;
                    archive_snapshot(
                        journal,
                        &source,
                        &mut archive_segments.borrow_mut(),
                        &update,
                    )?;
                }
                output
                    .send(Event {
                        source: source.clone(),
                        generation: origin,
                        update,
                    })
                    .map_err(anyhow::Error::from)
            };
            let activate = |call: &mut CallScopedSpeakers, at: u64, now: Instant| -> Result<()> {
                call.activate_observed(source.clone(), at, at as f64 / 1e9, now, |p| {
                    child.store(p, Ordering::Release);
                    scope.register(p);
                    if cancelled.load(Ordering::Acquire) {
                        scope.retire(p);
                        unsafe {
                            libc::kill(p as i32, libc::SIGTERM);
                        }
                    }
                })?;
                let writer = heimdall_archive::records::Writer::open(&archive)?;
                let id = source.clone();
                let correction_output=output.clone();
                let generation=origin;
                if let Some(pipeline) = call.pipeline_mut() {
                    let vectors = heimdall_archive::Journal::open(&archive)?;
                    pipeline.set_evidence_hook(Box::new(move |mut payload| {
                        if payload["kind"]=="speaker_segments_finalized" {
                            let segments=serde_json::from_value::<Vec<SpeakerSegment>>(payload["segments"].clone())?;
                            let until=segments.iter().map(|s|at+(s.end_time*1e9).round() as u64).max().unwrap_or(at);
                            correction_output.send(Event {source:id.clone(),generation,update:Update::Stable {origin:at,until,segments,finalized:true}})?;
                        }
                        vectors.reference_vectors(&mut payload)?;
                        vectors.reference_tensors(&mut payload)?;
                        let ns = payload["input_spans_seconds"]
                            .as_array()
                            .and_then(|v| v.first())
                            .and_then(|v| v[0].as_f64())
                            .map(|v| (v * 1e9) as u64)
                            .unwrap_or(at);
                        writer.event(payload["kind"].as_str().unwrap_or("speaker_model_output"), &id, &[], ns, payload.clone())
                    }));
                }
                Ok(())
            };
            let snapshot = |call: &mut CallScopedSpeakers| -> Result<Vec<SpeakerSegment>> {
                if let Some(p)=call.pipeline_mut() {
                    let segments=p.segments()?;
                    let until=(p.origin_seconds()+p.stream_ref().stable_frames() as f64*p.stream_ref().seconds_per_frame())*1e9;
                    output.send(Event {source:source.clone(),generation:origin,update:Update::Stable {origin:(p.origin_seconds()*1e9).round() as u64,until:until.round() as u64,segments:segments.clone(),finalized:false}})?;
                    Ok(segments)
                } else {Ok(vec![])}
            };
            let mut last_gpu_status = None;
            let run = (|| -> Result<()> {
                let mut recovery = HelperRecovery::default();
                let mut first = Some(Work::Activate(origin, *observed_voice.lock().unwrap()));
                let mut audio_clock = origin;
                loop {
                    if cancelled.load(Ordering::Acquire) {
                        break;
                    }
                    let status = call.gpu_status();
                    if status != last_gpu_status {
                        emit(Update::GpuStatus(status.clone()))?;
                        last_gpu_status = status;
                    }
                    let deadline = *observed_voice.lock().unwrap() + QUIET_TIMEOUT;
                    let work = if let Some(work) = first.take() { Some(work) } else if call.is_active() {
                        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                            Ok(work) => Some(work),
                            Err(crossbeam_channel::RecvTimeoutError::Timeout) => None,
                            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                        }
                    } else {
                        match rx.recv() {
                            Ok(work) => Some(work),
                            Err(_) => break,
                        }
                    };
                    let finishing = matches!(&work, Some(Work::Finish));
                    let operation = (|| -> Result<bool> {
                    // A queued silent packet must not postpone the wall-clock deadline.
                    // Intake records recent VAD voice even if inference has a backlog.
                    let observed_quiet =
                        Instant::now().saturating_duration_since(*observed_voice.lock().unwrap());
                    if call.is_active() && observed_quiet >= QUIET_TIMEOUT {
                        let events = call
                            .pipeline_mut()
                            .map(|p| p.finish())
                            .transpose()?
                            .unwrap_or_default();
                        let segments = snapshot(&mut call)?;
                        if let Some(p) = call.worker_pid() {
                            scope.retire(p);
                        }
                        let _ = call.expire(Instant::now())?;
                        child.store(0, Ordering::Release);
                        last_frame = -1;
                        emit(Update::Expired {
                            origin: current_origin,
                            segments,
                            events,
                        })?;
                    }
                    let Some(work) = work else { return Ok(false); };
                    match work {
                        Work::Backend(backend) => {
                            let active = call.is_active();
                            if active {
                                let events = call.pipeline_mut().unwrap().finish()?;
                                let segments = snapshot(&mut call)?;
                                if let Some(p) = call.worker_pid() { scope.retire(p); }
                                call.release(); child.store(0, Ordering::Release);
                                emit(Update::Expired { origin:current_origin, segments, events })?;
                            }
                            call.set_metal_backend(&backend)?;
                            if active {
                                current_origin=audio_clock;last_frame = -1;
                                activate(&mut call,audio_clock,Instant::now())?;
                                emit(Update::Started {origin:current_origin})?;
                            }
                        },
                        Work::Activate(at, now) => {
                            if !call.is_active() && recovery.ready(Instant::now()) {
                                current_origin = at;
                                audio_clock = at;
                                last_frame = -1;
                                activate(&mut call, at, now)?;
                                recovery.healthy_since = Some(Instant::now());
                                emit(Update::Started {
                                    origin: current_origin,
                                })?;
                            }
                        }
                        Work::Audio(audio, speech, at) => {
                            let start = audio_clock;
                            audio_clock = audio_clock.saturating_add(audio.len() as u64 * 62500);
                            if !call.is_active() {
                                if !speech || !recovery.ready(Instant::now()) { return Ok(false); }
                                current_origin = start;
                                last_frame = -1;
                                activate(&mut call, start, at)?;
                                recovery.healthy_since = Some(Instant::now());
                                emit(Update::Started { origin: current_origin })?;
                            }
                            let events = call.push(&audio, speech, at)?;
                            let frame = call.pipeline_mut().unwrap().stream_ref().frame_count();
                            if frame != last_frame || !events.is_empty() {
                                last_frame = frame;
                                emit(Update::Snapshot {
                                    origin: current_origin,
                                    segments: snapshot(&mut call)?,
                                    events,
                                })?;
                            }
                        }
                        Work::Ax(items) => {
                            for item in items {
                                call.naming_mut().observe_ax(item);
                            }
                        }
                        Work::Observe(track, end, text, ax) => {
                            for item in ax {
                                call.naming_mut().observe_ax(item);
                            }
                            let events = if let Some(p) = call.pipeline_mut() {
                                p.observe_transcript(track.speaker, end, &text)
                            } else {
                                call.naming_mut().observe_transcript(track, end, &text)
                            };
                            if !events.is_empty() {
                                emit(Update::Snapshot {
                                    origin: current_origin,
                                    segments: snapshot(&mut call)?,
                                    events,
                                })?;
                            }
                        }
                        Work::Finish => {
                            let events = call
                                .pipeline_mut()
                                .map(|p| p.finish())
                                .transpose()?
                                .unwrap_or_default();
                            emit(Update::Closed {
                                origin: current_origin,
                                segments: snapshot(&mut call)?,
                                events,
                            })?;
                            return Ok(true);
                        }
                        Work::Store(store) => call.naming_mut().set_store(store),
                        Work::Rename(from, to) => {
                            let events = call.naming_mut().rename_name(&from, &to);
                            if !events.is_empty() {
                                emit(Update::Snapshot {
                                    origin: current_origin,
                                    segments: snapshot(&mut call)?,
                                    events,
                                })?;
                            }
                        }
                        Work::Forget(name) => {
                            let events = call.naming_mut().clear_name(&name);
                            if !events.is_empty() {
                                emit(Update::Snapshot {
                                    origin: current_origin,
                                    segments: snapshot(&mut call)?,
                                    events,
                                })?;
                            }
                        }
                    }
                    Ok(false)
                    })();
                    match operation {
                        Ok(true) => break,
                        Ok(false) => {},
                        Err(e) => {
                            let p = child.swap(0, Ordering::AcqRel);
                            if p != 0 { scope.retire(p); }
                            call.release();
                            last_frame = -1;
                            let delay = recovery.failed(Instant::now());
                            let loop_status = if recovery.failures >= 5 { "helper crash loop" } else { "retrying helper" };
                            emit(Update::Error(format!("Speaker ? · {loop_status} in {} s: {e:#}", delay.as_secs())))?;
                            if finishing {
                                emit(Update::Closed { origin: current_origin, segments: vec![], events: vec![] })?;
                                break;
                            }
                        }
                    }
                }
                Ok(())
            })();
            if let Err(e) = run {
                let _ = emit(Update::Error(format!("{e:#}")));
            }
            if let Some(p) = call.worker_pid() {
                scope.retire(p);
            }
            drop(call);
            child.store(0, Ordering::Release);
        });
        Ok(Self {
            tx,
            cancel,
            pid,
            cpu,
            latest_voice,
        })
    }
    /// An already-expired actor for the isolated replay. Real expiry releases its
    /// native helper before delivering Update::Expired; do not load another one.
    pub(crate) fn expired_replay_fixture(cpu: Arc<SpeakerCpu>) -> Self {
        let (tx, _rx) = crossbeam_channel::bounded(1);
        Self {tx, cancel: Arc::new(AtomicBool::new(false)), pid: Arc::new(AtomicU32::new(0)), cpu, latest_voice: Arc::new(Mutex::new(Instant::now()))}
    }
    #[cfg(test)]
    pub(crate) fn finishing_fixture(source: String, origin: u64, archive: PathBuf, output: crossbeam_channel::Sender<Event>, cpu: Arc<SpeakerCpu>) -> (Self, std::thread::JoinHandle<()>) {
        let (tx, rx) = crossbeam_channel::bounded(4096);
        let actor = Self {tx, cancel: Arc::new(AtomicBool::new(false)), pid: Arc::new(AtomicU32::new(0)), cpu, latest_voice: Arc::new(Mutex::new(Instant::now()))};
        let thread = std::thread::spawn(move || {
            while let Ok(work) = rx.recv() {
                if matches!(work, Work::Finish) {
                    let update = Update::Closed {origin, segments: vec![SpeakerSegment {start_time:0.,end_time:1.,speaker:1}], events: vec![]};
                    archive_snapshot(&heimdall_archive::Journal::open(&archive).unwrap(), &source, &mut (0,vec![]), &update).unwrap();
                    output.send(Event {source,generation:origin,update}).unwrap();
                    break;
                }
            }
        });
        (actor, thread)
    }
    pub fn send(&self, work: Work) -> Result<()> {
        let voice = match &work {
            Work::Audio(_, true, at) | Work::Activate(_, at) => Some(*at),
            _ => None,
        };
        // Publish before admission, so the worker cannot expire a recently resumed call.
        if let Some(at) = voice {
            let mut latest = self.latest_voice.lock().unwrap();
            *latest = (*latest).max(at);
        }
        self.tx
            .try_send(work)
            .context("speaker actor queue unavailable (81.92-second bound)")
    }
}
impl Drop for Actor {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
        let pid = self.pid.load(Ordering::Acquire);
        if pid != 0 {
            self.cpu.retire(pid);
            unsafe {
                libc::kill(pid as i32, libc::SIGTERM);
            }
        }
    }
}

fn archive_snapshot(
    journal: &heimdall_archive::Journal,
    source: &str,
    previous: &mut (u64, Vec<SpeakerSegment>),
    update: &Update,
) -> Result<()> {
    let (origin, segments, events) = match update {
        Update::Snapshot {
            origin,
            segments,
            events,
        }
        | Update::Closed {
            origin,
            segments,
            events,
        }
        | Update::Expired {
            origin,
            segments,
            events,
        } => (*origin, segments, events),
        _ => return Ok(()),
    };
    if previous.0 != origin {
        previous.1.clear();
        previous.0 = origin;
    }
    let changed: Vec<_> = segments
        .iter()
        .enumerate()
        .filter(|(i, s)| previous.1.get(*i).is_none_or(|p| p != *s))
        .map(|(i, s)| serde_json::json!({"index":i,"segment":s}))
        .collect();
    if !changed.is_empty() || !events.is_empty() || previous.1.len() != segments.len() {
        journal.event("speaker_segments_delta",source,&[],origin,serde_json::json!({"origin_ns":origin,"segments":changed,"total_segments":segments.len(),"events":events}))?;
    }
    previous.1 = segments.clone();
    Ok(())
}

#[cfg(test)]
mod archive_tests {
    use super::*;
    #[test]
    fn worker_archives_final_segments_before_ui_generation_filtering() {
        let t = tempfile::tempdir().unwrap();
        let journal = heimdall_archive::Journal::open(&t.path().join("session-speakers")).unwrap();
        let mut previous = (0, vec![]);
        let segment = SpeakerSegment {
            start_time: 0.,
            end_time: 1.,
            speaker: 1,
        };
        let update = Update::Snapshot {
            origin: 1_000_000_000,
            segments: vec![segment],
            events: vec![],
        };
        archive_snapshot(&journal, "old-source-generation", &mut previous, &update).unwrap();
        archive_snapshot(&journal, "old-source-generation", &mut previous, &update).unwrap();
        let final_segment = SpeakerSegment {
            end_time: 2.,
            ..segment
        };
        archive_snapshot(
            &journal,
            "old-source-generation",
            &mut previous,
            &Update::Closed {
                origin: 1_000_000_000,
                segments: vec![final_segment],
                events: vec![],
            },
        )
        .unwrap();
        let records = journal.records().unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[1].source, "old-source-generation");
        assert_eq!(records[1].payload["segments"][0]["segment"]["end_time"], 2.);
    }
}

#[cfg(test)]
mod recovery_tests {
    use super::*;
    #[test]
    fn repeated_crashes_back_off_and_open_circuit_then_recover() {
        let mut state = HelperRecovery::default();
        let mut now = Instant::now();
        for seconds in [1,2,4,8,300] {
            assert!(state.ready(now));
            assert_eq!(state.failed(now), Duration::from_secs(seconds));
            assert!(!state.ready(now + Duration::from_millis(999)));
            now += Duration::from_secs(seconds);
        }
        assert!(state.ready(now));
        assert_eq!(state.failures,0);
        state.healthy_since=Some(now);
        assert_eq!(state.failed(now+Duration::from_secs(60)),Duration::from_secs(1));
    }
}
