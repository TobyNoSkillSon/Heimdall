//! Echo mux (23 Oct night, echo guard on speakers). Through the MacBook speakers the microphone hears Heimdall's own
//! replies. While Heimdall speaks on speakers the app asks for Apple voice processing (`macos/vpio.rs`); its cleaned
//! mono audio is substituted, frame for frame, into what the primary microphone's own plain IOProc hands listen; the
//! archive keeps the raw samples (`pipeline::Packet::clean`). Timestamps and source id stay the IOProc's own, so
//! ClockMapper, the archive and listen never see a restart or a jump.
//!
//! Platform-neutral and pure so it is unit-tested without a device: `EchoRing` is the lock-free single-producer
//! (voice-processing input callback) / single-consumer (mic IOProc) ring, `EchoMux` the consumer's state machine.
//! Neither side allocates, locks, logs or makes a syscall.
use std::{
    cell::UnsafeCell,
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    thread::Thread,
};

/// What echo cancellation is doing now (`Capture::echo_cancellation`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EchoCancel {
    /// Not asked for, or released.
    Off,
    /// Asked for: gates, unit creation (1–3 s cold) or the ring's prefill.
    Starting,
    /// The microphone source is carrying cleaned audio now.
    Active,
    /// A safety gate said no (AirPods/headphones/headset route, Bluetooth default input, built-in mic, …).
    Refused(String),
    /// The unit failed (OSStatus), stalled, or hit the hard session limit.
    Failed(String),
}

/// Cleaned mono samples from the voice-processing unit to the mic IOProc. Capacity is a power of two; indices are
/// monotonic totals, so `write - read` is the fill.
pub(crate) struct EchoRing {
    buf: Box<[UnsafeCell<f32>]>,
    mask: usize,
    write: AtomicUsize,
    read: AtomicUsize,
    /// Set by the session while its unit runs; the mux substitutes only while this is true.
    pub live: AtomicBool,
    /// Set by the mux while any frame it delivers carries cleaned audio (`EchoCancel::Active`).
    pub substituting: AtomicBool,
    /// Producer samples dropped because the ring was full (the IOProc stopped consuming).
    pub dropped: AtomicU64,
    /// Buffers in which the ring ran dry and raw audio filled in.
    pub underruns: AtomicU64,
    /// Times the fill passed `max_fill` and the reader skipped ahead.
    pub skips: AtomicU64,
}
// SAFETY: slot `i` is written only by the single producer while `i` lies in [write, read + capacity) and read only by
// the single consumer while it lies in [read, write); the Release store of `write`/`read` after each side's access and
// the matching Acquire load on the other side order every slot access. The voice-processing session is the only
// producer (sessions are serialized by its worker) and the mic IOProc, guarded against reentry, the only consumer.
unsafe impl Sync for EchoRing {}
impl EchoRing {
    pub fn new(min_capacity: usize) -> Arc<Self> {
        let capacity = min_capacity.max(64).next_power_of_two();
        Arc::new(Self {
            buf: (0..capacity).map(|_| UnsafeCell::new(0.)).collect(),
            mask: capacity - 1,
            write: AtomicUsize::new(0),
            read: AtomicUsize::new(0),
            live: AtomicBool::new(false),
            substituting: AtomicBool::new(false),
            dropped: AtomicU64::new(0),
            underruns: AtomicU64::new(0),
            skips: AtomicU64::new(0),
        })
    }
    /// Half a second at the microphone's rate.
    pub fn for_rate(rate: u32) -> Arc<Self> {
        Self::new(rate as usize / 2)
    }
    /// Producer only (the voice-processing input callback). A full ring drops the newest samples and counts them.
    pub fn push_clean(&self, samples: &[f32]) {
        let w = self.write.load(Ordering::Relaxed);
        let r = self.read.load(Ordering::Acquire);
        let free = self.buf.len() - w.wrapping_sub(r);
        let n = samples.len().min(free);
        for (i, s) in samples[..n].iter().enumerate() {
            // SAFETY: slot (w + i) is in [write, read + capacity): owned by the producer (see the Sync note).
            unsafe { *self.buf[w.wrapping_add(i) & self.mask].get() = *s };
        }
        self.write.store(w.wrapping_add(n), Ordering::Release);
        if n < samples.len() {
            self.dropped.fetch_add((samples.len() - n) as u64, Ordering::Relaxed);
        }
    }
    #[cfg(test)]
    fn fill(&self) -> usize {
        self.write
            .load(Ordering::Acquire)
            .wrapping_sub(self.read.load(Ordering::Relaxed))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// Raw audio. Waits for `live` and a prefill before fading in.
    Raw,
    /// Fading raw → clean; the count is frames done.
    In(usize),
    /// Cleaned audio only.
    Clean,
    /// Fading clean → raw; the count is frames left.
    Out(usize),
}

/// The mic IOProc's side: one per microphone source, owned by its IOProc block.
pub(crate) struct EchoMux {
    ring: Arc<EchoRing>,
    phase: Phase,
    /// 10 ms in frames: the base prefill and the crossfade length.
    base: usize,
    /// Fill required (beyond the buffer about to be read) before substitution starts. Grows by 10 ms per underrun up
    /// to 40 ms, for a unit that delivers in larger slices than the IOProc reads; shrinks by 10 ms after each 2 s of
    /// cleaned frames without an underrun, back to 10 ms, so one hiccup does not add latency for the whole reply.
    prefill: usize,
    /// Cleaned frames delivered since the last underrun or prefill decay.
    steady: usize,
    /// Past this fill (100 ms) the reader skips ahead, so cleaned audio never lags the raw timestamps by more.
    max_fill: usize,
    /// What a skip leaves behind beyond the buffer being read (20 ms, or the prefill if larger).
    skip_to: usize,
    clean: Box<[f32]>,
    weight: Box<[f32]>,
}
/// Largest IOProc buffer the mux plans; a larger one stays raw (Core Audio's default ceiling is 4096 frames).
const MAX_PLAN_FRAMES: usize = 8192;
impl EchoMux {
    pub fn new(ring: Arc<EchoRing>, rate: u32) -> Self {
        let base = (rate as usize / 100).max(1);
        Self {
            ring,
            phase: Phase::Raw,
            base,
            prefill: base,
            steady: 0,
            max_fill: base * 10,
            skip_to: base * 2,
            clean: vec![0.; MAX_PLAN_FRAMES].into_boxed_slice(),
            weight: vec![0.; MAX_PLAN_FRAMES].into_boxed_slice(),
        }
    }
    /// Plans one IOProc buffer of `frames` frames, consuming cleaned samples from the ring. True when any frame of
    /// the buffer carries cleaned audio; then `blend` gives each sample. Called once per callback, before the samples.
    pub fn plan(&mut self, frames: usize) -> bool {
        let ring = &*self.ring;
        let live = ring.live.load(Ordering::Acquire);
        let w = ring.write.load(Ordering::Acquire);
        let mut r = ring.read.load(Ordering::Relaxed);
        if frames > self.clean.len() {
            self.phase = Phase::Raw;
            ring.read.store(w, Ordering::Release);
            ring.substituting.store(false, Ordering::Release);
            return false;
        }
        if self.phase == Phase::Raw {
            if !live {
                // Stale samples from a finished session must not be played into the next one.
                ring.read.store(w, Ordering::Release);
                ring.substituting.store(false, Ordering::Release);
                return false;
            }
            if w.wrapping_sub(r) < frames + self.prefill {
                ring.substituting.store(false, Ordering::Release);
                return false;
            }
            self.phase = Phase::In(0);
        }
        if !live {
            self.phase = match self.phase {
                Phase::In(done) => Phase::Out(done + 1),
                Phase::Clean => Phase::Out(self.base),
                other => other,
            };
        }
        let fill = w.wrapping_sub(r);
        if fill > self.max_fill {
            let keep = (self.skip_to.max(self.prefill) + frames).min(fill);
            r = w.wrapping_sub(keep);
            ring.skips.fetch_add(1, Ordering::Relaxed);
        }
        let mut any = false;
        for f in 0..frames {
            let weight = match self.phase {
                Phase::Raw => {
                    self.weight[f] = 0.;
                    continue;
                }
                Phase::In(done) => (done + 1) as f32 / self.base as f32,
                Phase::Clean => 1.,
                Phase::Out(left) => (left - 1) as f32 / self.base as f32,
            };
            if r == w {
                // Underrun: raw for the missing frames; prefill again (a little deeper) before the next fade-in.
                ring.underruns.fetch_add(1, Ordering::Relaxed);
                if live {
                    self.prefill = (self.prefill + self.base).min(self.base * 4);
                }
                self.steady = 0;
                self.phase = Phase::Raw;
                self.weight[f..frames].fill(0.);
                break;
            }
            // SAFETY: slot r is in [read, write): owned by the consumer (see EchoRing's Sync note).
            self.clean[f] = unsafe { *ring.buf[r & ring.mask].get() };
            r = r.wrapping_add(1);
            self.steady += 1;
            if self.steady >= self.base * 200 {
                self.steady = 0;
                self.prefill = self.prefill.saturating_sub(self.base).max(self.base);
            }
            self.weight[f] = weight;
            any |= weight > 0.;
            self.phase = match self.phase {
                Phase::In(done) if done + 1 >= self.base => Phase::Clean,
                Phase::In(done) => Phase::In(done + 1),
                Phase::Out(left) if left <= 1 => Phase::Raw,
                Phase::Out(left) => Phase::Out(left - 1),
                other => other,
            };
        }
        ring.read.store(r, Ordering::Release);
        ring.substituting
            .store(any || self.phase != Phase::Raw, Ordering::Release);
        any
    }
    /// The sample to deliver for `frame` of the planned buffer, given the raw one (any channel: the cleaned audio is
    /// mono and goes to every channel).
    #[inline]
    pub fn blend(&self, frame: usize, raw: f32) -> f32 {
        let w = self.weight[frame];
        if w <= 0. {
            raw
        } else if w >= 1. {
            self.clean[frame]
        } else {
            self.clean[frame] * w + raw * (1. - w)
        }
    }
    /// `plan` + `blend` into an interleaved buffer.
    #[cfg(test)]
    pub fn mix_into(
        &mut self,
        frames: usize,
        channels: usize,
        raw: impl Fn(usize, usize) -> f32,
        out: &mut [f32],
    ) -> bool {
        let planned = self.plan(frames);
        for f in 0..frames {
            for c in 0..channels {
                let r = raw(f, c);
                out[f * channels + c] = if planned { self.blend(f, r) } else { r };
            }
        }
        planned
    }
}

/// The primary microphone source a session may clean: published by the HAL owner thread on every pass.
#[derive(Clone)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) struct EchoTarget {
    pub device: u32,
    pub name: String,
    pub built_in: bool,
    pub rate: u32,
    pub ring: Arc<EchoRing>,
}
impl EchoTarget {
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub fn same(&self, other: &EchoTarget) -> bool {
        Arc::ptr_eq(&self.ring, &other.ring) && self.device == other.device
    }
}

/// What the session worker last decided; `Running` reads as Active once the mux substitutes.
#[derive(Clone)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) enum State {
    Off,
    Starting,
    Running(Arc<EchoRing>),
    Refused(String),
    Failed(String),
}

/// Shared between `Capture`, every `EchoControl` clone, the HAL owner and the session worker.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) struct Shared {
    pub wanted: AtomicBool,
    /// Incremented on each off → on request: one start attempt (or refusal) per request.
    pub requests: AtomicU64,
    /// Capture is shutting down: the worker tears down and exits; later calls are no-ops.
    pub closed: AtomicBool,
    pub state: Mutex<State>,
    pub target: Mutex<Option<EchoTarget>>,
    pub worker: Mutex<Option<Thread>>,
}
impl Shared {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            wanted: AtomicBool::new(false),
            requests: AtomicU64::new(0),
            closed: AtomicBool::new(false),
            state: Mutex::new(State::Off),
            target: Mutex::new(None),
            worker: Mutex::new(None),
        })
    }
    pub fn set_state(&self, state: State) {
        if let Ok(mut s) = self.state.lock() {
            *s = state;
        }
    }
    pub fn wake(&self) {
        if let Ok(w) = self.worker.lock() {
            if let Some(t) = w.as_ref() {
                t.unpark();
            }
        }
    }
}

/// A cheap handle on echo cancellation for any thread (`Capture::echo_control`). Calls after the `Capture` is
/// dropped do nothing and read `Off`.
#[derive(Clone)]
pub struct EchoControl(pub(crate) Arc<Shared>);
impl EchoControl {
    /// Ask for (or release) echo cancellation. Idempotent and cheap: two atomics, and a thread unpark only when the
    /// wish changes, so the app may call it on every tick.
    pub fn set_echo_cancellation(&self, wanted: bool) {
        let s = &self.0;
        if s.closed.load(Ordering::Acquire) {
            return;
        }
        if s.wanted.swap(wanted, Ordering::AcqRel) == wanted {
            return;
        }
        if wanted {
            s.requests.fetch_add(1, Ordering::AcqRel);
        }
        s.wake();
    }
    pub fn echo_cancellation(&self) -> EchoCancel {
        let s = &self.0;
        if s.closed.load(Ordering::Acquire) {
            return EchoCancel::Off;
        }
        match s.state.lock().map(|g| g.clone()) {
            Ok(State::Off) | Err(_) => EchoCancel::Off,
            Ok(State::Starting) => EchoCancel::Starting,
            Ok(State::Running(ring)) => {
                if ring.substituting.load(Ordering::Acquire) {
                    EchoCancel::Active
                } else {
                    EchoCancel::Starting
                }
            }
            Ok(State::Refused(r)) => EchoCancel::Refused(r),
            Ok(State::Failed(f)) => EchoCancel::Failed(f),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const RATE: u32 = 48_000; // 10 ms = 480 frames
    fn mux() -> (Arc<EchoRing>, EchoMux) {
        let ring = EchoRing::for_rate(RATE);
        (ring.clone(), EchoMux::new(ring, RATE))
    }
    fn run(m: &mut EchoMux, frames: usize, channels: usize, raw: f32) -> Vec<f32> {
        let mut out = vec![0.; frames * channels];
        m.mix_into(frames, channels, |_, _| raw, &mut out);
        out
    }

    #[test]
    fn nothing_is_substituted_until_live_and_prefilled() {
        let (ring, mut m) = mux();
        // Not live: samples already in the ring are discarded, raw passes untouched.
        ring.push_clean(&[0.5; 2000]);
        assert_eq!(run(&mut m, 512, 2, -0.25), vec![-0.25; 1024]);
        assert_eq!(ring.fill(), 0, "stale cleaned audio is dropped while not live");
        ring.live.store(true, Ordering::Release);
        // Live but below one buffer + 10 ms prefill (512 + 480): still raw, and nothing consumed.
        ring.push_clean(&[0.5; 900]);
        assert_eq!(run(&mut m, 512, 2, -0.25), vec![-0.25; 1024]);
        assert_eq!(ring.fill(), 900);
        assert!(!ring.substituting.load(Ordering::Acquire));
        ring.push_clean(&[0.5; 200]);
        let out = run(&mut m, 512, 2, -0.25);
        assert!(ring.substituting.load(Ordering::Acquire));
        assert_eq!(ring.fill(), 1100 - 512, "frame for frame: one cleaned sample per IOProc frame");
        // Mono goes to every channel.
        assert!(out.chunks(2).all(|f| f[0] == f[1]));
    }

    #[test]
    fn handover_crossfades_over_ten_ms_both_ways() {
        let (ring, mut m) = mux();
        ring.live.store(true, Ordering::Release);
        ring.push_clean(&[1.0; 4000]);
        let out = run(&mut m, 960, 1, 0.0);
        // Linear 0 → 1 over 480 frames, no step larger than one fade step.
        assert!((out[0] - 1. / 480.).abs() < 1e-6);
        assert!((out[239] - 0.5).abs() < 1e-6);
        assert_eq!(out[479], 1.0);
        assert!(out[480..].iter().all(|&v| v == 1.0));
        assert!(out.windows(2).all(|w| (w[1] - w[0]).abs() <= 1. / 480. + 1e-6));
        // Release: the next buffer fades back to raw using the cleaned samples still in the ring.
        ring.live.store(false, Ordering::Release);
        let out = run(&mut m, 960, 1, 0.0);
        assert!((out[0] - 479. / 480.).abs() < 1e-6);
        assert!(out[480..].iter().all(|&v| v == 0.0));
        assert!(out.windows(2).all(|w| (w[1] - w[0]).abs() <= 1. / 480. + 1e-6));
        // Faded out: the rest of the ring is discarded on the next buffer, raw only.
        run(&mut m, 512, 1, 0.0);
        assert_eq!(ring.fill(), 0);
        assert!(!ring.substituting.load(Ordering::Acquire));
    }

    #[test]
    fn underrun_falls_back_to_raw_for_the_missing_frames() {
        let (ring, mut m) = mux();
        ring.live.store(true, Ordering::Release);
        ring.push_clean(&[1.0; 992]); // exactly one buffer + prefill
        run(&mut m, 512, 1, 0.0);
        // 480 left; ask for 512: 480 cleaned, the last 32 raw.
        let out = run(&mut m, 512, 1, -1.0);
        assert!(out[..480].iter().all(|&v| v == 1.0), "{:?}", &out[470..490]);
        assert!(out[480..].iter().all(|&v| v == -1.0));
        assert_eq!(ring.underruns.load(Ordering::Relaxed), 1);
        // Waits for a deeper prefill (20 ms) before fading in again.
        ring.push_clean(&[1.0; 1000]);
        assert_eq!(run(&mut m, 512, 1, -1.0), vec![-1.0; 512]);
        ring.push_clean(&[1.0; 600]);
        assert_ne!(run(&mut m, 512, 1, -1.0)[0], -1.0);
    }

    #[test]
    fn a_grown_prefill_decays_after_two_seconds_without_underruns() {
        let (ring, mut m) = mux();
        ring.live.store(true, Ordering::Release);
        ring.push_clean(&[1.0; 992]);
        run(&mut m, 512, 1, 0.0);
        run(&mut m, 512, 1, 0.0); // underrun
        assert_eq!(m.prefill, 960);
        // 2 s of steady cleaned audio (the ring kept 20 ms ahead), then back to 10 ms.
        ring.push_clean(&[1.0; 1472]);
        for _ in 0..(96_000 / 480) {
            ring.push_clean(&[1.0; 480]);
            run(&mut m, 480, 1, 0.0);
        }
        assert_eq!(ring.underruns.load(Ordering::Relaxed), 1);
        assert_eq!(m.prefill, 480);
    }

    #[test]
    fn overfill_skips_ahead_to_twenty_ms() {
        let (ring, mut m) = mux();
        ring.live.store(true, Ordering::Release);
        let ramp: Vec<f32> = (0..6000).map(|i| i as f32).collect();
        ring.push_clean(&ramp); // 125 ms > 100 ms
        let out = run(&mut m, 512, 1, 0.0);
        assert_eq!(ring.skips.load(Ordering::Relaxed), 1);
        assert_eq!(ring.fill(), 960, "20 ms left after this buffer");
        // The newest audio is played, the oldest dropped.
        assert_eq!(out[511], 6000. - 960. - 1.);
    }

    #[test]
    fn producer_overflow_is_bounded_and_counted() {
        let ring = EchoRing::new(1000); // 1024
        ring.push_clean(&[0.1; 1000]);
        ring.push_clean(&[0.1; 100]);
        assert_eq!(ring.fill(), 1024);
        assert_eq!(ring.dropped.load(Ordering::Relaxed), 76);
    }

    #[test]
    fn handle_is_idempotent_and_inert_after_close() {
        let shared = Shared::new();
        let c = EchoControl(shared.clone());
        c.set_echo_cancellation(true);
        c.set_echo_cancellation(true);
        assert_eq!(shared.requests.load(Ordering::Acquire), 1, "a repeated wish is not a new request");
        c.set_echo_cancellation(false);
        c.set_echo_cancellation(true);
        assert_eq!(shared.requests.load(Ordering::Acquire), 2);
        let ring = EchoRing::new(64);
        shared.set_state(State::Running(ring.clone()));
        assert_eq!(c.echo_cancellation(), EchoCancel::Starting);
        ring.substituting.store(true, Ordering::Release);
        assert_eq!(c.echo_cancellation(), EchoCancel::Active);
        shared.closed.store(true, Ordering::Release);
        c.set_echo_cancellation(false);
        assert!(shared.wanted.load(Ordering::Acquire), "no-op after the Capture is gone");
        assert_eq!(c.echo_cancellation(), EchoCancel::Off);
    }
}
