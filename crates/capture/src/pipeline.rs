use crate::resample::MonoResampler;
use crate::{Archive, ArchiveSink, ClockMapper, SourceId, SourceInfo};
use anyhow::Result;
use crossbeam_channel::{Receiver, Sender};
use std::{
    cell::UnsafeCell,
    collections::{HashMap, VecDeque},
    ops::Deref,
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::Duration,
};
#[derive(Clone, Debug)]
pub struct AudioChunk {
    pub source_id: SourceId,
    pub label: String,
    pub host_ns: u64,
    pub samples: Vec<f32>,
    pub discontinuity: bool,
}
#[derive(Clone, Debug)]
pub enum CaptureEvent {
    SourceAdded(SourceInfo),
    SourceRemoved {
        source_id: SourceId,
        host_ns: u64,
        reason: String,
    },
    Audio(AudioChunk),
    /// Discovery/recovery information; no delivered stream lost samples.
    Diagnostic { host_ns: u64, detail: String },
    Gap {
        source_id: Option<SourceId>,
        host_ns: u64,
        detail: String,
    },
}
/// Blocking/channel API: pass to listen's capture adapter or use recv_timeout.
/// Audio has a 512-chunk memory bound; metadata and explicit gaps are retained.
/// Slow readers lose only listen PCM, never silently grow RAM or stop archival.
pub struct CaptureReceiver {
    rx: Receiver<CaptureEvent>,
    audio_inflight: Arc<AtomicUsize>,
}
impl CaptureReceiver {
    fn received(&self, e: &CaptureEvent) {
        if matches!(e, CaptureEvent::Audio(_)) {
            self.audio_inflight.fetch_sub(1, Ordering::Relaxed);
        }
    }
    pub fn recv(&self) -> std::result::Result<CaptureEvent, crossbeam_channel::RecvError> {
        let e = self.rx.recv()?;
        self.received(&e);
        Ok(e)
    }
    pub fn recv_timeout(
        &self,
        d: Duration,
    ) -> std::result::Result<CaptureEvent, crossbeam_channel::RecvTimeoutError> {
        let e = self.rx.recv_timeout(d)?;
        self.received(&e);
        Ok(e)
    }
    pub fn try_recv(&self) -> std::result::Result<CaptureEvent, crossbeam_channel::TryRecvError> {
        let e = self.rx.try_recv()?;
        self.received(&e);
        Ok(e)
    }
}
const MAX_SAMPLES: usize = 8192;
pub(crate) struct Packet {
    pub data: [f32; MAX_SAMPLES],
    pub len: usize,
    pub ns: u64,
    pub dropped: u64,
    /// Echo cancellation (the user's decision, 23 Oct: archive the raw microphone): while the echo mux substitutes,
    /// `data` stays the raw samples for the archive and this carries the cleaned mono frames for listen. Empty on
    /// sources that are never cleaned.
    pub clean: Box<[f32]>,
    pub cleaned: bool,
}
// A commit stall today lost up to 44,544 frames (928 ms) beyond the old
// 16-packet / 171-ms queue. Keep 2.73 s at 512 frames / 48 kHz:
// 256 preallocated packets cost 8 MiB/source, with no callback allocation.
const PACKETS: usize = 256;
/// The producer publishes a complete slot; the drain pump releases it once copied
/// to the heap stage. Unlike ArrayQueue, neither side spins waiting for the
/// other side to publish a slot. A preempted pump causes overflow.
struct PacketRing {
    slots: Vec<UnsafeCell<Packet>>,
    read: AtomicUsize,
    write: AtomicUsize,
    reading: AtomicBool,
}
// Slots are accessed only after the acquire/release handoff below. RawQueue's
// producer gate and the consumer's nonwaiting gate prevent concurrent owners.
unsafe impl Sync for PacketRing {}
impl PacketRing {
    /// `clean_frames`: room per packet for cleaned mono frames (0: none).
    fn new(clean_frames: usize) -> Self {
        Self {
            slots: (0..PACKETS)
                .map(|_| {
                    UnsafeCell::new(Packet {
                        data: [0.; MAX_SAMPLES],
                        len: 0,
                        ns: 0,
                        dropped: 0,
                        clean: vec![0.; clean_frames].into_boxed_slice(),
                        cleaned: false,
                    })
                })
                .collect(),
            read: AtomicUsize::new(0),
            write: AtomicUsize::new(0),
            reading: AtomicBool::new(false),
        }
    }
    /// Caller holds RawQueue's producer gate for the entire write/publication.
    unsafe fn push(&self, fill: impl FnOnce(&mut Packet)) -> bool {
        let write = self.write.load(Ordering::Relaxed);
        if write.wrapping_sub(self.read.load(Ordering::Acquire)) == PACKETS {
            return false;
        }
        fill(&mut *self.slots[write % PACKETS].get());
        self.write.store(write.wrapping_add(1), Ordering::Release);
        true
    }
    fn pop(&self) -> Option<PacketRead<'_>> {
        if self
            .reading
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return None;
        }
        let read = self.read.load(Ordering::Relaxed);
        if read == self.write.load(Ordering::Acquire) {
            self.reading.store(false, Ordering::Release);
            return None;
        }
        Some(PacketRead {
            ring: self,
            index: read,
        })
    }
    #[cfg(test)]
    fn len(&self) -> usize {
        self.write
            .load(Ordering::Acquire)
            .wrapping_sub(self.read.load(Ordering::Acquire))
    }
}
struct PacketRead<'a> {
    ring: &'a PacketRing,
    index: usize,
}
impl Deref for PacketRead<'_> {
    type Target = Packet;
    fn deref(&self) -> &Packet {
        unsafe { &*self.ring.slots[self.index % PACKETS].get() }
    }
}
impl Drop for PacketRead<'_> {
    fn drop(&mut self) {
        self.ring
            .read
            .store(self.index.wrapping_add(1), Ordering::Release);
        self.ring.reading.store(false, Ordering::Release);
    }
}
/// A native packet copied off the ring by the drain pump, awaiting the archive thread.
pub(crate) struct Staged {
    pub samples: Vec<f32>,
    pub ns: u64,
    pub dropped: u64,
    /// Cleaned mono frames for listen, when echo cancellation ran (`Packet::clean`).
    pub clean: Option<Vec<f32>>,
}
#[derive(Default)]
struct StagedQueue {
    packets: VecDeque<Staged>,
    samples: usize,
}
// The archive thread's once-a-second commit takes the shared index's write lock
// (60-s busy timeout) and fullfsyncs. Another Heimdall writer holding that lock
// stalled it ~7.6 s on 22 Oct (235,520 frames lost past the 2.73-s ring), on the
// mic and a tap at once. The pump keeps draining the ring meanwhile into this
// heap stage: at most 30 s of native PCM per source (11 MiB at 48 kHz stereo),
// and never more than 16 MiB for many-channel devices. Each packet is copied
// once; the stage holds only what the archive has not consumed yet.
const STAGE_SECONDS: usize = 30;
const STAGE_MAX_SAMPLES: usize = 4 << 20;
struct ProducerGate<'a>(&'a AtomicBool);
impl Drop for ProducerGate<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}
/// Preallocated bounded pool. No allocate/compress/DB/log in native callbacks.
pub(crate) struct RawQueue {
    pub info: SourceInfo,
    ready: PacketRing,
    staged: Mutex<StagedQueue>,
    producing: AtomicBool,
    pub last_callback: AtomicU64,
    pub last_nonzero: AtomicU64,
    lost: AtomicU64,
    pub stopped: AtomicBool,
    wake: thread::Thread,
}
impl RawQueue {
    pub fn new(info: SourceInfo, wake: thread::Thread) -> Arc<Self> {
        Self::with_clean(info, wake, 0)
    }
    /// A microphone that echo cancellation may clean: each packet also holds its frames' cleaned mono copy
    /// (`Packet::clean`; 8 MiB / channels more per source).
    pub fn with_clean_overlay(info: SourceInfo, wake: thread::Thread) -> Arc<Self> {
        let frames = MAX_SAMPLES / info.format.channels.max(1) as usize;
        Self::with_clean(info, wake, frames)
    }
    fn with_clean(info: SourceInfo, wake: thread::Thread, clean_frames: usize) -> Arc<Self> {
        Arc::new(Self {
            info,
            ready: PacketRing::new(clean_frames),
            staged: Mutex::new(StagedQueue::default()),
            producing: AtomicBool::new(false),
            last_callback: AtomicU64::new(0),
            last_nonzero: AtomicU64::new(0),
            lost: AtomicU64::new(0),
            stopped: AtomicBool::new(false),
            wake,
        })
    }
    #[cfg(test)]
    pub fn push_interleaved(&self, samples: &[f32], ns: u64) {
        self.push_frames(
            samples.len() / self.info.format.channels as usize,
            ns,
            |f, c| samples[f * self.info.format.channels as usize + c],
        );
    }
    pub fn push_frames(&self, frames: usize, ns: u64, read: impl FnMut(usize, usize) -> f32) {
        self.push(frames, ns, read, None::<fn(usize) -> f32>);
    }
    /// Raw frames for the archive plus their cleaned mono copy for listen (`clean(frame)`). A source built without the
    /// overlay keeps only the raw frames: the archive is never given cleaned audio.
    pub fn push_frames_cleaned(&self, frames: usize, ns: u64, read: impl FnMut(usize, usize) -> f32, clean: impl FnMut(usize) -> f32) {
        self.push(frames, ns, read, Some(clean));
    }
    fn push(&self, frames: usize, ns: u64, mut read: impl FnMut(usize, usize) -> f32, mut clean: Option<impl FnMut(usize) -> f32>) {
        if self.stopped.load(Ordering::Acquire) {
            return;
        }
        if self
            .producing
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            self.reject_frames(frames as u64);
            return;
        }
        let _gate = ProducerGate(&self.producing);
        self.last_callback.store(ns, Ordering::Release);
        let ch = self.info.format.channels as usize;
        let block = MAX_SAMPLES / ch;
        for offset in (0..frames).step_by(block) {
            let n = (frames - offset).min(block);
            // SAFETY: this callback owns the nonwaiting producer gate.
            let pushed = unsafe {
                self.ready.push(|p| {
                    p.len = n * ch;
                    p.ns =
                        ns + (offset as u64 * 1_000_000_000 / self.info.format.sample_rate as u64);
                    p.dropped = self.lost.swap(0, Ordering::AcqRel);
                    let mut nonzero = false;
                    for f in 0..n {
                        for c in 0..ch {
                            let v = read(offset + f, c);
                            p.data[f * ch + c] = v;
                            nonzero |= v != 0.;
                        }
                    }
                    if nonzero {
                        self.last_nonzero.store(p.ns, Ordering::Release);
                    }
                    p.cleaned = false;
                    if let Some(clean) = clean.as_mut().filter(|_| p.clean.len() >= n) {
                        for f in 0..n {
                            p.clean[f] = clean(offset + f);
                        }
                        p.cleaned = true;
                    }
                })
            };
            if !pushed {
                self.lost.fetch_add(n as u64, Ordering::Relaxed);
            }
            self.wake.unpark();
        }
    }
    /// Every queued packet as (host ns, raw samples, cleaned mono frames).
    #[cfg(test)]
    pub fn take_packets_with_clean(&self) -> Vec<(u64, Vec<f32>, Option<Vec<f32>>)> {
        let mut out = vec![];
        let ch = self.info.format.channels.max(1) as usize;
        while let Some(p) = self.ready.pop() {
            out.push((p.ns, p.data[..p.len].to_vec(), p.cleaned.then(|| p.clean[..p.len / ch].to_vec())));
        }
        out
    }
    pub fn reject_frames(&self, frames: u64) {
        self.lost.fetch_add(frames.max(1), Ordering::Relaxed);
        self.wake.unpark();
    }
    fn staged_queue(&self) -> std::sync::MutexGuard<'_, StagedQueue> {
        self.staged.lock().unwrap_or_else(|e| e.into_inner())
    }
    /// Moves published packets from the native ring to the bounded heap stage.
    /// Never touches the archive. Ring pop and stage push happen under one lock,
    /// so the pump and the archive thread (at shutdown) keep packet order. When
    /// the stage is full the ring is left to fill and overflow with exact loss.
    fn stage(&self) -> usize {
        let cap = (self.info.format.sample_rate as usize * self.info.format.channels as usize * STAGE_SECONDS)
            .min(STAGE_MAX_SAMPLES);
        let mut staged = self.staged_queue();
        let mut moved = 0;
        while staged.samples < cap {
            let Some(p) = self.ready.pop() else { break };
            staged.samples += p.len;
            let frames = p.len / (self.info.format.channels.max(1) as usize);
            let clean = p.cleaned.then(|| p.clean[..frames].to_vec());
            staged.packets.push_back(Staged { samples: p.data[..p.len].to_vec(), ns: p.ns, dropped: p.dropped, clean });
            moved += 1;
        }
        moved
    }
    fn next_staged(&self) -> Option<Staged> {
        let mut staged = self.staged_queue();
        let p = staged.packets.pop_front()?;
        staged.samples -= p.samples.len();
        Some(p)
    }
    fn drained(&self) -> bool {
        self.ready.read.load(Ordering::Acquire) == self.ready.write.load(Ordering::Acquire)
    }
}
pub(crate) enum Command {
    Add(Arc<RawQueue>),
    Remove(SourceId, String),
    Gap(Option<SourceId>, String),
    Diagnostic(String),
    Stop,
}
pub(crate) struct Pipeline {
    pub tx: Sender<Command>,
    /// The drain pump: native callbacks and commands wake it, never the archive thread.
    pub wake: thread::Thread,
    pump: Option<JoinHandle<()>>,
    join: Option<JoinHandle<()>>,
    pub alive: Arc<AtomicBool>,
    pub control_wake: Arc<std::sync::Mutex<Option<thread::Thread>>>,
}
/// The longest a capture stop waits for the archive thread's last commit before handing it off.
pub(crate) const CLOSE_WAIT: Duration = Duration::from_secs(2);
/// Archive threads still committing after their capture stopped (`Pipeline::close`).
fn finishing() -> &'static Mutex<Vec<JoinHandle<()>>> {
    static FINISHING: std::sync::OnceLock<Mutex<Vec<JoinHandle<()>>>> = std::sync::OnceLock::new();
    FINISHING.get_or_init(|| Mutex::new(vec![]))
}
/// Waits up to `timeout` for archive threads a stop handed off. True when none is left. Quit calls this so the last
/// second of audio still commits; Listening off never waits for it.
pub fn settle_stopped_archives(timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let mut left = finishing().lock().unwrap();
        let (done, running): (Vec<_>, Vec<_>) = left.drain(..).partition(|j| j.is_finished());
        *left = running;
        let empty = left.is_empty();
        drop(left);
        for j in done {
            let _ = j.join();
        }
        if empty {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(10));
    }
}
impl Pipeline {
    pub fn close(&mut self) {
        self.close_within(CLOSE_WAIT)
    }
    /// Stops intake and waits up to `wait` for the archive thread's last commit. A commit waits for the archive's
    /// write lock (up to `store::BUSY`, 60 s, behind a compaction step), and a stop runs on the live loop: past
    /// `wait` the thread finishes on its own and Quit settles it (24 Oct 02:30 freeze). Nothing is dropped.
    pub(crate) fn close_within(&mut self, wait: Duration) {
        if let Some(join) = self.join.take() {
            let _ = self.tx.send(Command::Stop);
            self.wake.unpark();
            if let Some(pump) = self.pump.take() {
                let _ = pump.join();
            }
            let deadline = std::time::Instant::now() + wait;
            while !join.is_finished() && std::time::Instant::now() < deadline {
                thread::sleep(Duration::from_millis(5));
            }
            if join.is_finished() {
                let _ = join.join();
            } else {
                eprintln!("Capture stopped; its archive is still committing (lock held) and finishes in the background");
                finishing().lock().unwrap().push(join);
            }
        }
    }
}
impl Drop for Pipeline {
    fn drop(&mut self) {
        self.close();
    }
}
struct State {
    raw: Arc<RawQueue>,
    clock: ClockMapper,
    resampler: MonoResampler,
    origin: Option<u64>,
    output: Vec<f32>,
    emitted: usize,
    skip: usize,
    listen_gap: bool,
    receiver_lag: bool,
    total_frames: usize,
    anchor_ns: u64,
    anchor_frame: usize,
    last_output_ns: Option<u64>,
}
impl State {
    fn new(raw: Arc<RawQueue>) -> Result<Self> {
        let resampler = MonoResampler::new(raw.info.format.sample_rate)?;
        let skip = resampler.delay;
        Ok(Self {
            raw,
            clock: ClockMapper::default(),
            resampler,
            origin: None,
            output: vec![],
            emitted: 0,
            skip,
            listen_gap: false,
            receiver_lag: false,
            total_frames: 0,
            anchor_ns: 0,
            anchor_frame: 0,
            last_output_ns: None,
        })
    }
    fn reset(&mut self, ns: u64, discontinuity: bool) -> Result<()> {
        self.resampler = MonoResampler::new(self.raw.info.format.sample_rate)?;
        self.skip = self.resampler.delay;
        self.origin = Some(ns);
        self.output.clear();
        self.emitted = 0;
        self.total_frames = 0;
        self.anchor_ns = ns;
        self.anchor_frame = 0;
        // Establishing the first clock/resampler origin loses no audio.
        // A later clock break or queue loss must still reset listen explicitly.
        self.listen_gap |= discontinuity;
        Ok(())
    }
}
pub(crate) fn start_pipeline(config: &crate::CaptureConfig) -> Result<(Pipeline, CaptureReceiver)> {
    start_pipeline_with_archive(Box::new(Archive::create(&config.archive_dir)?))
}
pub(crate) fn start_pipeline_with_archive(archive: Box<dyn ArchiveSink>) -> Result<(Pipeline, CaptureReceiver)> {
    // Listen never waits for an archive commit (bug note 2026-10-27T16-57-38; commit.rs).
    let archive: Box<dyn ArchiveSink> = Box::new(crate::commit::Committer::start(archive)?);
    let (tx, rx) = crossbeam_channel::unbounded();
    let (forward, forwarded) = crossbeam_channel::unbounded();
    let (events, erx) = crossbeam_channel::unbounded();
    let audio_inflight = Arc::new(AtomicUsize::new(0));
    let inflight = audio_inflight.clone();
    let control_wake = Arc::new(std::sync::Mutex::new(None::<thread::Thread>));
    let worker_wake = control_wake.clone();
    let alive = Arc::new(AtomicBool::new(true));
    let worker_alive = alive.clone();
    let join = thread::Builder::new()
        .name("heimdall-archive".into())
        .spawn(move || {
            if let Err(e) = run(archive, forwarded, &events, &inflight) {
                let _ = events.send(CaptureEvent::Gap {
                    source_id: None,
                    host_ns: crate::host_now_ns(),
                    detail: format!("archive pipeline stopped: {e:#}"),
                });
            }
            worker_alive.store(false, Ordering::Release);
            if let Ok(wake) = worker_wake.lock() {
                if let Some(w) = wake.as_ref() {
                    w.unpark();
                }
            }
        })?;
    let archive_wake = join.thread().clone();
    let pump_alive = alive.clone();
    let stop_archive = forward.clone();
    let pump = match thread::Builder::new()
        .name("heimdall-capture-drain".into())
        .spawn(move || pump(rx, forward, archive_wake, &pump_alive))
    {
        Ok(pump) => pump,
        Err(e) => {
            let _ = stop_archive.send(Command::Stop);
            join.thread().unpark();
            let _ = join.join();
            return Err(e.into());
        }
    };
    drop(stop_archive);
    let wake = pump.thread().clone();
    Ok((
        Pipeline {
            tx,
            wake,
            pump: Some(pump),
            join: Some(join),
            alive,
            control_wake,
        },
        CaptureReceiver {
            rx: erx,
            audio_inflight,
        },
    ))
}
fn output_timestamp(state: &mut State, output_frame: usize) -> u64 {
    let rate = state.raw.info.format.sample_rate as i128;
    let projected = (state.anchor_ns as i128 + output_frame as i128 * 1_000_000_000 / 16000
        - state.anchor_frame as i128 * 1_000_000_000 / rate)
        .max(0) as u64;
    let ns = state
        .last_output_ns
        .map(|previous| projected.max(previous.saturating_add(1)))
        .unwrap_or(projected);
    if ns != projected {
        state.listen_gap = true;
    }
    state.last_output_ns = Some(ns);
    ns
}
fn emit_audio(
    state: &mut State,
    samples: Vec<f32>,
    events: &Sender<CaptureEvent>,
    inflight: &AtomicUsize,
) -> Result<()> {
    let skip = state.skip.min(samples.len());
    state.skip -= skip;
    state.output.extend_from_slice(&samples[skip..]);
    let mut used = 0;
    while state.output.len() - used >= 320 {
        let ns = output_timestamp(state, state.emitted);
        if inflight.load(Ordering::Acquire) < 512 {
            inflight.fetch_add(1, Ordering::Relaxed);
            if events
                .send(CaptureEvent::Audio(AudioChunk {
                    source_id: state.raw.info.id.clone(),
                    label: state.raw.info.label.clone(),
                    host_ns: ns,
                    samples: state.output[used..used + 320].to_vec(),
                    discontinuity: state.listen_gap,
                }))
                .is_err()
            {
                inflight.fetch_sub(1, Ordering::Relaxed);
            }
            state.listen_gap = false;
            state.receiver_lag = false;
        } else if !state.receiver_lag {
            let _ = events.send(CaptureEvent::Gap {
                source_id: Some(state.raw.info.id.clone()),
                host_ns: ns,
                detail: "listen receiver lagged; raw archive retained".into(),
            });
            state.listen_gap = true;
            state.receiver_lag = true;
        }
        state.emitted += 320;
        used += 320;
    }
    state.output.drain(..used);
    Ok(())
}
fn drain(
    state: &mut State,
    archive: &mut dyn ArchiveSink,
    events: &Sender<CaptureEvent>,
    inflight: &AtomicUsize,
) -> Result<bool> {
    let mut any = false;
    let raw = state.raw.clone();
    loop {
        let Some(p) = raw.next_staged() else {
            // Without a pump (direct tests) or at shutdown, stage here: same lock, same order.
            if raw.stage() == 0 {
                break;
            }
            continue;
        };
        any = true;
        let f = state.raw.info.format;
        let len = p.samples.len();
        let (ns, jump) = state
            .clock
            .stamp(p.ns, len / f.channels as usize, f.sample_rate);
        if jump && p.dropped == 0 {
            archive.gap(Some(&state.raw.info.id), ns, "native clock discontinuity without queue loss")?;
            let _ = events.send(CaptureEvent::Gap {
                source_id: Some(state.raw.info.id.clone()), host_ns: ns,
                detail: "native clock discontinuity without queue loss".into(),
            });
        }
        if p.dropped > 0 {
            archive.gap(
                Some(&state.raw.info.id),
                ns,
                &format!("native queue overflow: {} frames lost", p.dropped),
            )?;
            let _ = events.send(CaptureEvent::Gap {
                source_id: Some(state.raw.info.id.clone()),
                host_ns: ns,
                detail: format!("native overflow: {} frames", p.dropped),
            });
        }
        if state.origin.is_some() && (jump || p.dropped > 0) {
            // Flush the captured epoch's filter/pending tail before resetting.
            // Resetting first silently dropped up to a resampler block of PCM.
            finish_audio(state, events, inflight)?;
        }
        archive.append(
            &state.raw.info.id,
            p.ns,
            f,
            &p.samples,
            jump || p.dropped > 0,
        )?;
        if state.origin.is_none() || jump || p.dropped > 0 {
            state.reset(ns, jump || p.dropped > 0)?;
        }
        state.anchor_ns = ns;
        state.anchor_frame = state.total_frames;
        state.total_frames += len / f.channels as usize;
        // The archive above kept the raw samples; listen hears the cleaned ones while echo cancellation runs. The
        // resampler averages channels, so the mono copy (the mux's blend averaged over channels) is the same signal.
        let output = match &p.clean {
            Some(clean) => state.resampler.push(clean, 1)?,
            None => state.resampler.push(&p.samples, f.channels as usize)?,
        };
        emit_audio(state, output, events, inflight)?;
    }
    Ok(any)
}
/// Drains every native ring into its heap stage and forwards commands to the
/// archive thread. It never waits on SQLite, fsync or the app, so an archive
/// commit stall no longer overflows every source's ring at the same instant.
fn pump(commands: Receiver<Command>, forward: Sender<Command>, archive: thread::Thread, alive: &AtomicBool) {
    // If the pump ends (stop or panic), report intake as stopped so the owner
    // never keeps native sources running with nothing draining them.
    struct Dead<'a>(&'a AtomicBool, thread::Thread);
    impl Drop for Dead<'_> {
        fn drop(&mut self) {
            self.0.store(false, Ordering::Release);
            self.1.unpark();
        }
    }
    let _dead = Dead(alive, archive.clone());
    let mut queues: Vec<Arc<RawQueue>> = vec![];
    loop {
        let mut stop = false;
        for command in commands.try_iter() {
            match &command {
                Command::Add(raw) => queues.push(raw.clone()),
                Command::Stop => stop = true,
                _ => {}
            }
            // Commands queued after Stop still reach the archive, as before the pump.
            let _ = forward.send(command);
        }
        for q in &queues {
            q.stage();
        }
        // A stopped source rejects new callbacks; the archive's finish stages any
        // packet published after this check.
        queues.retain(|q| !(q.stopped.load(Ordering::Acquire) && q.drained()));
        archive.unpark();
        if stop || !alive.load(Ordering::Acquire) {
            return;
        }
        thread::park();
    }
}
/// Emit exactly the current epoch's captured duration, including a partial
/// 20-ms packet. Padding needed by the resampling filter never reaches listen.
fn finish_audio(state: &mut State, events: &Sender<CaptureEvent>, inflight: &AtomicUsize) -> Result<()> {
    // Final padding is excluded from listen; archive already contains exact frames.
    let max = state.total_frames * 16000 / state.raw.info.format.sample_rate as usize;
    let remaining = max.saturating_sub(state.emitted + state.output.len());
    let tail = state.resampler.finish(remaining + state.skip)?;
    let take = (remaining + state.skip).min(tail.len());
    emit_audio(state, tail[..take].to_vec(), events, inflight)?;
    if !state.output.is_empty() {
        let index = state.emitted;
        let ns = output_timestamp(state, index);
        let samples = std::mem::take(&mut state.output);
        if inflight.load(Ordering::Acquire) < 512 {
            inflight.fetch_add(1, Ordering::Relaxed);
            if events
                .send(CaptureEvent::Audio(AudioChunk {
                    source_id: state.raw.info.id.clone(),
                    label: state.raw.info.label.clone(),
                    host_ns: ns,
                    samples,
                    discontinuity: state.listen_gap,
                }))
                .is_err()
            {
                inflight.fetch_sub(1, Ordering::Relaxed);
            }
            state.receiver_lag = false;
        } else if !state.receiver_lag {
            let _ = events.send(CaptureEvent::Gap {
                source_id: Some(state.raw.info.id.clone()), host_ns: ns,
                detail: format!("listen receiver lagged; raw archive retained; tail {} samples", samples.len()),
            });
            state.receiver_lag = true;
            state.listen_gap = true;
        }
    }
    Ok(())
}
fn finish(
    mut state: State,
    archive: &mut dyn ArchiveSink,
    events: &Sender<CaptureEvent>,
    inflight: &AtomicUsize,
    reason: String,
) -> Result<()> {
    state.raw.stopped.store(true, Ordering::Release);
    drain(&mut state, archive, events, inflight)?;
    let lost = state.raw.lost.swap(0, Ordering::AcqRel);
    if lost > 0 {
        archive.gap(
            Some(&state.raw.info.id),
            crate::host_now_ns(),
            &format!("native shutdown loss: {lost} frames"),
        )?;
    }

    let native_end = state.anchor_ns
        + (state.total_frames.saturating_sub(state.anchor_frame) as u64 * 1_000_000_000
            / state.raw.info.format.sample_rate as u64);
    let end = crate::host_now_ns().max(native_end);
    finish_audio(&mut state, events, inflight)?;
    archive.remove_source(&state.raw.info.id, end)?;
    let _ = events.send(CaptureEvent::SourceRemoved {
        source_id: state.raw.info.id.clone(),
        host_ns: end,
        reason,
    });
    Ok(())
}
fn run(
    mut archive: Box<dyn ArchiveSink>,
    commands: Receiver<Command>,
    events: &Sender<CaptureEvent>,
    inflight: &AtomicUsize,
) -> Result<()> {
    let mut states: HashMap<SourceId, State> = HashMap::new();
    let mut stop = false;
    loop {
        loop {
            let command = match commands.try_recv() {
                Ok(command) => command,
                // The drain thread is gone (it panicked): nothing will send Stop, so stop here rather than park
                // forever and hang `close()` (review, 23 Oct).
                Err(e) => { if e.is_disconnected() { stop = true; } break; }
            };
            match command {
                Command::Add(raw) => {
                    archive.add_source(&raw.info, crate::host_now_ns())?;
                    let _ = events.send(CaptureEvent::SourceAdded(raw.info.clone()));
                    states.insert(raw.info.id.clone(), State::new(raw)?);
                }
                Command::Remove(id, reason) => {
                    if let Some(s) = states.remove(&id) {
                        finish(s, archive.as_mut(), events, inflight, reason)?;
                    }
                    archive.flush()?;
                }
                Command::Diagnostic(detail) => {
                    let host_ns = crate::host_now_ns();
                    archive.gap(None, host_ns, &format!("diagnostic: {detail}"))?;
                    let _ = events.send(CaptureEvent::Diagnostic { host_ns, detail });
                }
                Command::Gap(id, detail) => {
                    let ns = crate::host_now_ns();
                    archive.gap(id.as_ref(), ns, &detail)?;
                    let _ = events.send(CaptureEvent::Gap {
                        source_id: id,
                        host_ns: ns,
                        detail,
                    });
                }
                Command::Stop => stop = true,
            }
        }
        let mut any = false;
        for state in states.values_mut() {
            any |= drain(state, archive.as_mut(), events, inflight)?;
        }
        if stop {
            for (_, state) in states.drain() {
                finish(
                    state,
                    archive.as_mut(),
                    events,
                    inflight,
                    "capture stopped".into(),
                )?;
            }
            archive.close()?;
            return Ok(());
        }
        if !any {
            if let Some(wait) = archive.commit_wait() {
                if wait.is_zero() {
                    archive.flush()?;
                } else {
                    thread::park_timeout(wait);
                }
            } else {
                // Bounded: a drain pump that panics wakes this thread before its sender drops (review 27 Oct), and
                // nothing wakes it after the disconnect; the next look finds it and stops.
                thread::park_timeout(Duration::from_secs(1));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CaptureConfig, NativeFormat, SourceKind};
    fn test_queue(uid: &str) -> Arc<RawQueue> {
        RawQueue::new(
            SourceInfo::new(
                SourceKind::InputDevice {
                    name: "Test".into(),
                    uid: uid.into(),
                },
                NativeFormat {
                    sample_rate: 16000,
                    channels: 1,
                },
                1,
            ),
            thread::current(),
        )
    }
    #[test]
    fn preempted_reader_never_blocks_or_overwrites_callback() {
        let q = test_queue("held-reader");
        for i in 0..PACKETS {
            q.push_interleaved(&[i as f32; 32], i as u64 * 2_000_000);
        }
        let held = q.ready.pop().unwrap();
        // Holding the first packet keeps its slot occupied. The callback returns
        // with a loss count rather than waiting for the consumer to release it.
        q.push_interleaved(&[99.; 32], PACKETS as u64 * 2_000_000);
        assert_eq!(held.data[0], 0.);
        assert_eq!(q.lost.load(Ordering::Acquire), 32);
        drop(held);
        q.push_interleaved(&[100.; 32], (PACKETS as u64 + 1) * 2_000_000);
        for i in 1..PACKETS {
            assert_eq!(q.ready.pop().unwrap().data[0], i as f32);
        }
        let last = q.ready.pop().unwrap();
        assert_eq!(last.data[0], 100.);
        assert_eq!(last.dropped, 32);
    }
    #[test]
    fn concurrent_handoff_preserves_data_and_accounts_for_every_frame() {
        let q = test_queue("concurrent");
        let producer = q.clone();
        let done = Arc::new(AtomicBool::new(false));
        let finished = done.clone();
        let worker = thread::spawn(move || {
            for i in 0..20_000 {
                producer.push_interleaved(&[i as f32; 32], i * 2_000_000);
            }
            finished.store(true, Ordering::Release);
        });
        let mut accounted = 0u64;
        loop {
            // Acquire completion before reading the ring, so the final published
            // packet cannot be missed after observing producer completion.
            let complete = done.load(Ordering::Acquire);
            if let Some(p) = q.ready.pop() {
                accounted += p.dropped;
                assert_eq!(p.ns, accounted / 32 * 2_000_000);
                assert!(p.data[..p.len]
                    .iter()
                    .all(|v| *v == (p.ns / 2_000_000) as f32));
                accounted += p.len as u64;
            } else if complete {
                break;
            } else {
                thread::yield_now();
            }
        }
        worker.join().unwrap();
        accounted += q.lost.load(Ordering::Acquire);
        assert_eq!(accounted, 20_000 * 32);
    }
    #[test]
    fn archive_precedes_listen_and_shutdown_has_no_synthetic_tail() {
        let temp = tempfile::tempdir().unwrap();
        let config = CaptureConfig {
            archive_dir: temp.path().join("session-pipeline-test"),
            ..Default::default()
        };
        let (mut pipeline, receiver) = start_pipeline(&config).unwrap();
        let info = SourceInfo::new(
            SourceKind::InputDevice {
                name: "Test".into(),
                uid: "synthetic".into(),
            },
            NativeFormat {
                sample_rate: 48000,
                channels: 2,
            },
            1,
        );
        let queue = RawQueue::new(info.clone(), pipeline.wake.clone());
        pipeline.tx.send(Command::Add(queue.clone())).unwrap();
        pipeline.wake.unpark();
        assert!(matches!(
            receiver.recv_timeout(Duration::from_secs(2)).unwrap(),
            CaptureEvent::SourceAdded(_)
        ));
        let origin = 10_000_000_000;
        let pcm: Vec<f32> = (0..9600)
            .map(|n| if n % 2 == 0 { 0.1 } else { -0.05 })
            .collect();
        for (i, block) in pcm.chunks(960).enumerate() {
            queue.push_interleaved(block, origin + i as u64 * 10_000_000);
        }
        queue.stopped.store(true, Ordering::Release);
        pipeline
            .tx
            .send(Command::Remove(info.id.clone(), "test finished".into()))
            .unwrap();
        pipeline.tx.send(Command::Stop).unwrap();
        pipeline.wake.unpark();
        pipeline.close();
        let events: Vec<_> = receiver.rx.try_iter().collect();
        let audio: Vec<_> = events
            .iter()
            .filter_map(|e| {
                if let CaptureEvent::Audio(c) = e {
                    Some(c)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(audio.iter().map(|c| c.samples.len()).sum::<usize>(), 1600);
        assert_eq!(audio[0].host_ns, origin);
        assert!(audio.iter().all(|c| !c.discontinuity), "fresh continuous source is not a gap");
        for pair in audio.windows(2) {
            assert!(pair[1].host_ns > pair[0].host_ns);
        }
        assert!(events
            .iter()
            .any(|e| matches!(e, CaptureEvent::SourceRemoved { .. })));
        let db = rusqlite::Connection::open(temp.path().join(heimdall_archive::store::INDEX)).unwrap();
        assert_eq!(
            db.query_row("SELECT sum(frames) FROM blocks", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            4800
        );
    }
    #[test]
    fn first_origin_is_continuous_but_clock_breaks_and_losses_are_not() {
        let q = test_queue("onset-gap");
        let mut state = State::new(q.clone()).unwrap();
        let temp = tempfile::tempdir().unwrap();
        let mut archive = Archive::create(&temp.path().join("session-onset-gap")).unwrap();
        archive.add_source(&q.info, 10_000_000_000).unwrap();
        let (events, rx) = crossbeam_channel::unbounded();
        let inflight = AtomicUsize::new(0);
        q.push_interleaved(&[0.1; 960],10_000_000_000);
        drain(&mut state,&mut archive,&events,&inflight).unwrap();
        let initial: Vec<_> = rx.try_iter().collect();
        assert!(initial.iter().any(|e|matches!(e,CaptureEvent::Audio(_))));
        assert!(initial.iter().all(|e|!matches!(e,CaptureEvent::Audio(c) if c.discontinuity)));
        q.push_interleaved(&[0.1; 960],11_000_000_000);
        drain(&mut state,&mut archive,&events,&inflight).unwrap();
        assert!(rx.try_iter().any(|e|matches!(e,CaptureEvent::Audio(c) if c.discontinuity)));
        q.reject_frames(320);
        q.push_interleaved(&[0.1; 960],11_060_000_000);
        drain(&mut state,&mut archive,&events,&inflight).unwrap();
        let loss: Vec<_> = rx.try_iter().collect();
        assert!(loss.iter().any(|e|matches!(e,CaptureEvent::Gap {..})));
        assert!(loss.iter().any(|e|matches!(e,CaptureEvent::Audio(c) if c.discontinuity)));
    }
    #[test]
    fn receiver_loss_is_recorded_for_a_partial_tail_and_after_a_clock_gap() {
        for tail in [false, true] {
            let q = test_queue("receiver-loss");
            let mut state = State::new(q.clone()).unwrap();
            state.reset(10_000_000_000, true).unwrap();
            let (events, rx) = crossbeam_channel::unbounded();
            let inflight = AtomicUsize::new(512);
            if tail {
                state.total_frames = 100;
                let output=state.resampler.push(&[0.1;100],1).unwrap();
                emit_audio(&mut state,output,&events,&inflight).unwrap();
                finish_audio(&mut state,&events,&inflight).unwrap();
            } else {
                state.skip=0;
                emit_audio(&mut state,vec![0.1;640],&events,&inflight).unwrap();
            }
            let lost: Vec<_> = rx.try_iter().collect();
            assert_eq!(lost.len(),1);
            assert!(matches!(&lost[0],CaptureEvent::Gap {source_id:Some(id),detail,..} if *id==q.info.id && detail.starts_with("listen receiver lagged;")));
            assert_eq!(inflight.load(Ordering::Acquire),512);
        }
    }
    #[test]
    fn capture_gap_flushes_exact_resampler_tail_without_padding() {
        let info = SourceInfo::new(SourceKind::InputDevice { name: "Tail".into(), uid: "gap-tail".into() },
            NativeFormat { sample_rate: 48000, channels: 2 }, 1);
        let q = RawQueue::new(info, thread::current());
        let mut state = State::new(q.clone()).unwrap();
        let temp = tempfile::tempdir().unwrap();
        let mut archive = Archive::create(&temp.path().join("session-tail")).unwrap();
        archive.add_source(&q.info, 10_000_000_000).unwrap();
        let (events, rx) = crossbeam_channel::unbounded();
        let inflight = AtomicUsize::new(0);
        for epoch in 0..2u64 {
            for tick in 0..5u64 {
                q.push_interleaved(&[0.1;1024], 10_000_000_000 + epoch * 1_000_000_000 + tick * 512_000_000_000 / 48000);
            }
            drain(&mut state, &mut archive, &events, &inflight).unwrap();
        }
        finish(state, &mut archive, &events, &inflight, "test".into()).unwrap();
        archive.flush().unwrap();
        let chunks: Vec<_> = rx.try_iter().filter_map(|e| match e {CaptureEvent::Audio(c) => Some(c), _=>None}).collect();
        for epoch in 0..2u64 {
            let start=10_000_000_000 + epoch * 1_000_000_000;
            let group: Vec<_> = chunks.iter().filter(|c|c.host_ns >= start && c.host_ns < start + 1_000_000_000).collect();
            assert_eq!(group.iter().map(|c|c.samples.len()).sum::<usize>(),2560*16000/48000);
            assert_eq!(group[0].discontinuity,epoch==1);
            for pair in group.windows(2) { assert!(pair[0].host_ns.saturating_add(pair[0].samples.len() as u64*62500).abs_diff(pair[1].host_ns)<=125000); }
        }
        let db=rusqlite::Connection::open(temp.path().join(heimdall_archive::store::INDEX)).unwrap();
        assert_eq!(db.query_row("SELECT sum(frames) FROM blocks",[],|r|r.get::<_,i64>(0)).unwrap(),5120);
        assert_eq!(db.query_row("SELECT detail FROM gaps",[],|r|r.get::<_,String>(0)).unwrap(),"native clock discontinuity without queue loss");
    }
    #[test]
    fn native_commit_stall_keeps_three_sources_and_exact_frame_order() {
        // Replay 1.28 s of native 48-kHz / 512-frame arrivals while the archive
        // consumer cannot run. The recorded losses plus the old queue imply a ~1.10-s stall.
        let queues: Vec<_> = (0..3).map(|i| {
            RawQueue::new(SourceInfo::new(SourceKind::InputDevice {
                name: "Stall replay".into(), uid: format!("stall-{i}"),
            }, NativeFormat { sample_rate: 48000, channels: 2 }, 1), thread::current())
        }).collect();
        for tick in 0..120u64 {
            for q in &queues { q.push_interleaved(&vec![tick as f32 + 1.; 1024], 10_000_000_000 + tick * 512_000_000_000 / 48000); }
        }
        for q in queues {
            assert_eq!(q.lost.load(Ordering::Acquire), 0);
            for tick in 0..120u64 {
                let p = q.ready.pop().unwrap();
                assert_eq!(p.len, 1024); assert_eq!(p.dropped, 0);
                assert!(p.data[..p.len].iter().all(|v| *v == tick as f32 + 1.));
                assert_eq!(p.ns, 10_000_000_000 + tick * 512_000_000_000 / 48000);
            }
            assert!(q.ready.pop().is_none());
        }
    }
    /// An archive sink whose appends block like a commit waiting on another
    /// writer's index lock, until the test opens the gate.
    struct LockedArchive {
        open: Arc<(Mutex<bool>, std::sync::Condvar)>,
        frames: Arc<AtomicU64>,
        gaps: Arc<Mutex<Vec<String>>>,
    }
    impl ArchiveSink for LockedArchive {
        fn add_source(&mut self, _: &SourceInfo, _: u64) -> Result<()> { Ok(()) }
        fn remove_source(&mut self, _: &SourceId, _: u64) -> Result<()> { Ok(()) }
        fn gap(&mut self, _: Option<&SourceId>, _: u64, detail: &str) -> Result<()> {
            self.gaps.lock().unwrap().push(detail.into());
            Ok(())
        }
        fn append(&mut self, _: &SourceId, _: u64, f: NativeFormat, samples: &[f32], _: bool) -> Result<()> {
            let (lock, opened) = &*self.open;
            let mut open = lock.lock().unwrap();
            while !*open {
                open = opened.wait(open).unwrap();
            }
            self.frames.fetch_add((samples.len() / f.channels as usize) as u64, Ordering::Relaxed);
            Ok(())
        }
        fn flush(&mut self) -> Result<()> { Ok(()) }
        fn commit_wait(&self) -> Option<Duration> { None }
    }
    /// Every appended sample, in order.
    struct RecordingArchive(Arc<Mutex<Vec<f32>>>);
    impl ArchiveSink for RecordingArchive {
        fn add_source(&mut self, _: &SourceInfo, _: u64) -> Result<()> { Ok(()) }
        fn remove_source(&mut self, _: &SourceId, _: u64) -> Result<()> { Ok(()) }
        fn gap(&mut self, _: Option<&SourceId>, _: u64, _: &str) -> Result<()> { Ok(()) }
        fn append(&mut self, _: &SourceId, _: u64, _: NativeFormat, samples: &[f32], _: bool) -> Result<()> {
            self.0.lock().unwrap().extend_from_slice(samples);
            Ok(())
        }
        fn flush(&mut self) -> Result<()> { Ok(()) }
        fn commit_wait(&self) -> Option<Duration> { None }
    }
    /// The user's decision (23 Oct): the archive holds the raw microphone, never the echo-cancelled signal, even while echo
    /// cancellation runs; listen (transcription, the decider's clip) hears the cleaned one.
    #[test]
    fn while_echo_cancellation_runs_the_archive_keeps_the_raw_mic_and_listen_hears_the_cleaned_one() {
        let archived = Arc::new(Mutex::new(vec![]));
        let (mut pipeline, receiver) = start_pipeline_with_archive(Box::new(RecordingArchive(archived.clone()))).unwrap();
        let info = SourceInfo::new(SourceKind::InputDevice { name: "Shure MV7i".into(), uid: "shure".into() },
            NativeFormat { sample_rate: 48000, channels: 2 }, 1);
        let q = RawQueue::with_clean_overlay(info, pipeline.wake.clone());
        pipeline.tx.send(Command::Add(q.clone())).unwrap();
        pipeline.wake.unpark();
        assert!(matches!(receiver.recv_timeout(Duration::from_secs(5)).unwrap(), CaptureEvent::SourceAdded(_)));
        // One second of 512-frame callbacks: raw 0.5 on both channels, cleaned mono 0.1 (the echo removed).
        for tick in 0..94u64 {
            q.push_frames_cleaned(512, 10_000_000_000 + tick * 512_000_000_000 / 48000, |_, _| 0.5, |_| 0.1);
        }
        q.stopped.store(true, Ordering::Release);
        pipeline.tx.send(Command::Remove(q.info.id.clone(), "test".into())).unwrap();
        pipeline.close();
        let archived = archived.lock().unwrap();
        assert_eq!(archived.len(), 94 * 512 * 2);
        assert!(archived.iter().all(|&v| v == 0.5), "cleaned audio reached the archive");
        let heard: Vec<f32> = receiver.rx.try_iter().filter_map(|e| match e { CaptureEvent::Audio(c) => Some(c.samples), _ => None }).flatten().collect();
        assert!(heard.len() > 12_000, "{}", heard.len());
        // Past the resampler's edges, listen hears the cleaned level.
        let middle = &heard[2_000..heard.len() - 2_000];
        assert!(middle.iter().all(|v| (v - 0.1).abs() < 0.01), "listen did not hear the cleaned audio: {:?}", &middle[..4]);
        // Without the overlay (a process tap), a cleaned push keeps only the raw frames.
        let tap = test_queue("tap");
        tap.push_frames_cleaned(32, 0, |_, _| 0.5, |_| 0.1);
        let packets = tap.take_packets_with_clean();
        assert_eq!((packets[0].1[0], packets[0].2.is_none()), (0.5, true));
    }
    #[test]
    fn archive_stall_longer_than_the_ring_loses_no_native_audio() {
        let open = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let frames = Arc::new(AtomicU64::new(0));
        let gaps = Arc::new(Mutex::new(vec![]));
        let sink = LockedArchive { open: open.clone(), frames: frames.clone(), gaps: gaps.clone() };
        let (mut pipeline, receiver) = start_pipeline_with_archive(Box::new(sink)).unwrap();
        // The 22-Oct pattern: a mic and a process tap, both 48-kHz stereo.
        let queues: Vec<_> = ["mic", "tap"].iter().map(|uid| {
            let info = SourceInfo::new(SourceKind::InputDevice { name: "Stall".into(), uid: (*uid).into() },
                NativeFormat { sample_rate: 48000, channels: 2 }, 1);
            let q = RawQueue::new(info, pipeline.wake.clone());
            pipeline.tx.send(Command::Add(q.clone())).unwrap();
            q
        }).collect();
        pipeline.wake.unpark();
        for _ in 0..2 {
            assert!(matches!(receiver.recv_timeout(Duration::from_secs(5)).unwrap(), CaptureEvent::SourceAdded(_)));
        }
        // 3 ring capacities (8.2 s) of 512-frame callbacks while the archive is blocked.
        // Old design: the archive thread drained the ring, so this lost ~5.5 s per source.
        let ticks = PACKETS as u64 * 3;
        for tick in 0..ticks {
            for q in &queues {
                q.push_interleaved(&[0.1; 1024], 10_000_000_000 + tick * 512_000_000_000 / 48000);
            }
            if tick % 64 == 63 {
                // Real callbacks arrive every 10.7 ms; let the pump catch up instead of racing it.
                let deadline = std::time::Instant::now() + Duration::from_secs(5);
                while !queues.iter().all(|q| q.drained()) {
                    assert!(std::time::Instant::now() < deadline, "pump stopped draining while the archive was stalled");
                    thread::sleep(Duration::from_millis(1));
                }
            }
        }
        for q in &queues {
            assert_eq!(q.lost.load(Ordering::Acquire), 0, "native ring overflowed during an archive stall");
        }
        assert_eq!(frames.load(Ordering::Relaxed), 0, "archive must still be stalled");
        *open.0.lock().unwrap() = true;
        open.1.notify_all();
        for q in &queues {
            q.stopped.store(true, Ordering::Release);
            pipeline.tx.send(Command::Remove(q.info.id.clone(), "test".into())).unwrap();
        }
        pipeline.close();
        // A slow machine may hand the last commits off; they still land.
        assert!(settle_stopped_archives(Duration::from_secs(60)));
        assert_eq!(frames.load(Ordering::Relaxed), 2 * ticks * 512);
        assert!(gaps.lock().unwrap().iter().all(|g| !g.contains("overflow")));
        // This test never reads listen PCM, so the 512-chunk listen cap may report
        // receiver lag; no native loss may appear.
        let details: Vec<_> = receiver.rx.try_iter().filter_map(|e| match e { CaptureEvent::Gap { detail, .. } => Some(detail), _ => None }).collect();
        assert!(details.iter().all(|d| d.starts_with("listen receiver lagged;")), "{details:?}");
    }
    /// 24 Oct 02:30: a capture stop on the live loop waited for an archive commit stuck behind a compaction step.
    /// The stop returns after `CLOSE_WAIT`; the held audio still commits once the lock frees, and Quit settles it.
    #[test]
    fn a_stop_under_a_held_archive_lock_returns_and_the_audio_still_commits() {
        let open = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let frames = Arc::new(AtomicU64::new(0));
        let sink = LockedArchive { open: open.clone(), frames: frames.clone(), gaps: Arc::new(Mutex::new(vec![])) };
        let (mut pipeline, receiver) = start_pipeline_with_archive(Box::new(sink)).unwrap();
        let info = SourceInfo::new(SourceKind::InputDevice { name: "Held".into(), uid: "held".into() },
            NativeFormat { sample_rate: 48000, channels: 2 }, 1);
        let q = RawQueue::new(info, pipeline.wake.clone());
        pipeline.tx.send(Command::Add(q.clone())).unwrap();
        pipeline.wake.unpark();
        assert!(matches!(receiver.recv_timeout(Duration::from_secs(5)).unwrap(), CaptureEvent::SourceAdded(_)));
        for tick in 0..8u64 {
            q.push_interleaved(&[0.1; 1024], 10_000_000_000 + tick * 512_000_000_000 / 48000);
        }
        q.stopped.store(true, Ordering::Release);
        pipeline.tx.send(Command::Remove(q.info.id.clone(), "test".into())).unwrap();
        let started = std::time::Instant::now();
        pipeline.close_within(Duration::from_millis(200));
        assert!(started.elapsed() < Duration::from_secs(5), "stop waited {:?} for a held lock", started.elapsed());
        assert_eq!(frames.load(Ordering::Relaxed), 0, "the archive must still be held");
        assert!(!settle_stopped_archives(Duration::from_millis(50)), "the handed-off commit is still waiting");
        *open.0.lock().unwrap() = true;
        open.1.notify_all();
        assert!(settle_stopped_archives(Duration::from_secs(30)));
        assert_eq!(frames.load(Ordering::Relaxed), 8 * 512, "every held frame committed after the stop");
    }
    /// Bug note 2026-10-27T16-57-38: a compaction step held the archive's write lock, the intake thread waited in its
    /// commit, and listen heard nothing from the Shure for 16 s (four stalls, then a relaunch). Listen keeps hearing a
    /// microphone while its commit waits, and the archive still gets every frame once the lock frees.
    #[test]
    fn listen_keeps_hearing_the_mic_while_the_archive_lock_is_held() {
        let open = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let frames = Arc::new(AtomicU64::new(0));
        let sink = LockedArchive { open: open.clone(), frames: frames.clone(), gaps: Arc::new(Mutex::new(vec![])) };
        let (mut pipeline, receiver) = start_pipeline_with_archive(Box::new(sink)).unwrap();
        let info = SourceInfo::new(SourceKind::InputDevice { name: "Shure MV7i".into(), uid: "shure".into() },
            NativeFormat { sample_rate: 48000, channels: 2 }, 1);
        let q = RawQueue::new(info, pipeline.wake.clone());
        pipeline.tx.send(Command::Add(q.clone())).unwrap();
        pipeline.wake.unpark();
        assert!(matches!(receiver.recv_timeout(Duration::from_secs(5)).unwrap(), CaptureEvent::SourceAdded(_)));
        // One second of 512-frame callbacks while every archive call blocks.
        let ticks = 94u64;
        for tick in 0..ticks {
            q.push_interleaved(&[0.1; 1024], 10_000_000_000 + tick * 512_000_000_000 / 48000);
        }
        pipeline.wake.unpark();
        let mut heard = 0usize;
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while heard < 12_000 {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            match receiver.recv_timeout(left) {
                Ok(CaptureEvent::Audio(c)) => heard += c.samples.len(),
                Ok(_) => {}
                Err(_) => panic!("listen heard {heard} samples while the archive lock was held"),
            }
        }
        assert_eq!(frames.load(Ordering::Relaxed), 0, "the archive must still be held");
        *open.0.lock().unwrap() = true;
        open.1.notify_all();
        q.stopped.store(true, Ordering::Release);
        pipeline.tx.send(Command::Remove(q.info.id.clone(), "test".into())).unwrap();
        pipeline.close();
        assert!(settle_stopped_archives(Duration::from_secs(30)));
        assert_eq!(frames.load(Ordering::Relaxed), ticks * 512, "every frame committed once the lock freed");
    }
    #[test]
    fn stage_is_bounded_and_ring_loss_past_it_stays_exact() {
        let q = test_queue("stage-bound");
        let cap = 16000 * STAGE_SECONDS;
        let pushed = cap / 320 + PACKETS + 10;
        for i in 0..pushed {
            q.push_interleaved(&[0.1; 320], i as u64 * 20_000_000);
            q.stage();
        }
        assert!(q.staged_queue().samples <= cap);
        assert_eq!(q.ready.len(), PACKETS);
        assert_eq!(q.lost.load(Ordering::Acquire), 10 * 320);
        // Draining resumes in order and the next packet after the hole carries the loss.
        let mut seen = 0;
        let mut dropped = 0;
        let mut last_ns = None;
        loop {
            let p = match q.next_staged() { Some(p) => p, None => { if q.stage() == 0 { break } continue } };
            assert!(last_ns.is_none_or(|n| p.ns > n));
            last_ns = Some(p.ns);
            seen += p.samples.len();
            dropped += p.dropped;
        }
        q.push_interleaved(&[0.1; 320], pushed as u64 * 20_000_000);
        q.stage();
        dropped += q.next_staged().unwrap().dropped;
        assert_eq!(seen, cap + PACKETS * 320);
        assert_eq!(dropped, 10 * 320);
    }
    /// Review, 23 Oct: if the drain thread dies its command sender drops and Stop can never come; the archive
    /// thread must end (flushing) instead of parking forever and hanging `close()`.
    #[test]
    fn the_archive_thread_ends_when_the_drain_thread_is_gone() {
        let (tx, commands) = crossbeam_channel::unbounded::<Command>();
        let (events, _rx) = crossbeam_channel::unbounded();
        let archive = LockedArchive {
            open: Arc::new((Mutex::new(true), std::sync::Condvar::new())),
            frames: Arc::new(AtomicU64::new(0)),
            gaps: Arc::new(Mutex::new(vec![])),
        };
        drop(tx);
        let (done, ended) = crossbeam_channel::bounded(1);
        std::thread::spawn(move || {
            let inflight = AtomicUsize::new(0);
            let _ = done.send(run(Box::new(archive), commands, &events, &inflight).is_ok());
        });
        assert_eq!(ended.recv_timeout(Duration::from_secs(5)), Ok(true));
    }
    /// Review 27 Oct: the drain pump's death wakes the archive thread before its sender drops. Woken while still
    /// connected, the thread parked again and no wake followed the disconnect: it leaked and Quit waited for it.
    #[test]
    fn the_archive_thread_ends_when_the_drain_thread_goes_after_its_last_wake() {
        let (tx, commands) = crossbeam_channel::unbounded::<Command>();
        let (events, _rx) = crossbeam_channel::unbounded();
        let archive = LockedArchive {
            open: Arc::new((Mutex::new(true), std::sync::Condvar::new())),
            frames: Arc::new(AtomicU64::new(0)),
            gaps: Arc::new(Mutex::new(vec![])),
        };
        let (done, ended) = crossbeam_channel::bounded(1);
        let worker = std::thread::spawn(move || {
            let inflight = AtomicUsize::new(0);
            let _ = done.send(run(Box::new(archive), commands, &events, &inflight).is_ok());
        });
        thread::sleep(Duration::from_millis(100));
        worker.thread().unpark();
        thread::sleep(Duration::from_millis(100));
        drop(tx);
        assert_eq!(ended.recv_timeout(Duration::from_secs(5)), Ok(true));
    }
    #[test]
    fn diagnostic_is_durable_without_an_audio_gap_or_clock_reset() {
        let temp = tempfile::tempdir().unwrap();
        let config = CaptureConfig { archive_dir: temp.path().join("session-diagnostic"), ..Default::default() };
        let (mut pipeline, rx) = start_pipeline(&config).unwrap();
        pipeline.tx.send(Command::Diagnostic("PID vanished during discovery".into())).unwrap();
        pipeline.wake.unpark();
        assert!(matches!(rx.recv_timeout(Duration::from_secs(2)).unwrap(), CaptureEvent::Diagnostic { .. }));
        pipeline.close();
        let db = rusqlite::Connection::open(temp.path().join(heimdall_archive::store::INDEX)).unwrap();
        let detail: String = db.query_row("SELECT detail FROM gaps", [], |r| r.get(0)).unwrap();
        assert_eq!(detail, "diagnostic: PID vanished during discovery");
        assert!(rx.rx.try_iter().all(|e| !matches!(e, CaptureEvent::Gap { .. })));
    }
    #[test]
    fn ring_overflow_is_bounded_and_accounted() {
        let info = SourceInfo::new(
            SourceKind::InputDevice {
                name: "Test".into(),
                uid: "bounded".into(),
            },
            NativeFormat {
                sample_rate: 16000,
                channels: 1,
            },
            1,
        );
        let q = RawQueue::new(info, thread::current());
        for i in 0..PACKETS + 4 {
            q.push_interleaved(&[0.1; 320], i as u64 * 20_000_000);
        }
        assert_eq!(q.ready.len(), PACKETS);
        assert_eq!(q.lost.load(Ordering::Acquire), 1280);
    }
}

#[cfg(test)]
mod timeline_tests {
    use super::*;
    use crate::{NativeFormat, SourceKind};
    #[test]
    fn output_uses_native_clock_anchor_instead_of_accumulating_rate_drift() {
        let info = SourceInfo::new(
            SourceKind::InputDevice {
                name: "Clock".into(),
                uid: "drifting".into(),
            },
            NativeFormat {
                sample_rate: 48000,
                channels: 1,
            },
            1,
        );
        let mut s = State::new(RawQueue::new(info, thread::current())).unwrap();
        s.reset(10_000_000_000, false).unwrap();
        s.anchor_frame = 4800;
        s.anchor_ns = 10_100_100_000;
        assert_eq!(output_timestamp(&mut s, 1600), 10_100_100_000);
        s.anchor_ns = 10_090_000_000;
        assert_eq!(output_timestamp(&mut s, 1600), 10_100_100_001);
        assert!(s.listen_gap);
    }
}
