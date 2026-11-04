use crate::{Audio, AudioOutput};
use anyhow::{Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::{
    Arc, Mutex, RwLock,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
pub type LevelHook = Arc<dyn Fn(f32) + Send + Sync>;
static LEVEL_HOOK: RwLock<Option<LevelHook>> = RwLock::new(None);
/// Receives the loudness of what the speaker plays: RMS of each ~30 ms (0 to 1 of full scale) while a stream
/// plays, then 0 when it ends. Called from the output's own waiting thread, never from the render callback; nothing
/// runs while no audio plays.
pub fn set_level_hook(hook: Option<LevelHook>) {
    *LEVEL_HOOK.write().unwrap() = hook;
}
pub type SpokenTextHook = Arc<dyn Fn(Option<&str>) + Send + Sync>;
static SPOKEN_HOOK: RwLock<Option<SpokenTextHook>> = RwLock::new(None);
/// Receives each sentence as the speaker starts playing it (`PlaybackChunk::label`), then `None` when the stream
/// ends, is interrupted or fails. Called from the output's waiting thread, never from the render callback.
pub fn set_spoken_text_hook(hook: Option<SpokenTextHook>) {
    *SPOKEN_HOOK.write().unwrap() = hook;
}
/// Receives `true` when Heimdall's voice becomes audible and `false` when the last sample of the last playing stream
/// has passed the speaker (or playback is cancelled). One flag for every stream: agent speech and the Speed sample
/// count together, so an overlapping stream never ends it early. Called from the output's waiting thread.
pub type SpeakingHook = Arc<dyn Fn(bool) + Send + Sync>;
static SPEAKING_HOOK: RwLock<Option<SpeakingHook>> = RwLock::new(None);
pub fn set_speaking_hook(hook: Option<SpeakingHook>) {
    *SPEAKING_HOOK.write().unwrap() = hook;
}
fn emit_speaking(on: bool) {
    let hook = SPEAKING_HOOK.read().unwrap().clone();
    if let Some(hook) = hook {
        hook(on);
    }
}
/// How long the speaking flag outlives the last sample of a reply's last job.
pub(crate) const SPEAKING_LINGER: std::time::Duration = std::time::Duration::from_millis(150);
/// While jobs are still synthesizing, the claim is released once nothing has played for this long (the island falls
/// back to its thinking look); the next sample that plays claims it again.
pub(crate) const SPEAKING_SILENCE_CAP: std::time::Duration = std::time::Duration::from_millis(1500);
/// The count of claims on the speaking flag and what it emits: `true` when the first claim is taken, `false` when the
/// last is dropped. Transitions are taken under the count's lock, so they reach the hook in order.
pub(crate) struct Speaking {
    streams: Mutex<usize>,
    emit: Box<dyn Fn(bool) + Send + Sync>,
}
impl Speaking {
    pub(crate) fn new(emit: Box<dyn Fn(bool) + Send + Sync>) -> Arc<Self> {
        Arc::new(Self { streams: Mutex::new(0), emit })
    }
}
/// The app-wide flag: every queue and every device stream counts here.
pub(crate) fn global_speaking() -> Arc<Speaking> {
    static OWNER: std::sync::OnceLock<Arc<Speaking>> = std::sync::OnceLock::new();
    OWNER.get_or_init(|| Speaking::new(Box::new(emit_speaking))).clone()
}
/// One claim on the flag, held while its audio is audible (or, for a queue, while its reply is).
struct SpeakingClaim(Arc<Speaking>);
impl SpeakingClaim {
    fn new(owner: Arc<Speaking>) -> Self {
        {
            let mut count = owner.streams.lock().unwrap();
            *count += 1;
            if *count == 1 {
                (owner.emit)(true);
            }
        }
        Self(owner)
    }
}
impl Drop for SpeakingClaim {
    fn drop(&mut self) {
        let mut count = self.0.streams.lock().unwrap();
        *count -= 1;
        if *count == 0 {
            (self.0.emit)(false);
        }
    }
}
/// One queue's claim across the jobs it plays back to back. It is taken when audio reaches the speaker, kept while
/// later jobs are queued (their synthesis gaps included), released `linger` after the last sample of the last job,
/// and released at once by `release` (interrupt, cancel).
pub(crate) struct SpeakingHold {
    owner: Arc<Speaking>,
    linger: std::time::Duration,
    state: Mutex<HoldState>,
}
#[derive(Default)]
struct HoldState {
    claim: Option<SpeakingClaim>,
    last_sample: Option<std::time::Instant>,
}
impl SpeakingHold {
    pub(crate) fn new(owner: Arc<Speaking>, linger: std::time::Duration) -> Arc<Self> {
        Arc::new(Self { owner, linger, state: Mutex::new(HoldState::default()) })
    }
    /// Audio reached the speaker: take the claim unless this queue holds it already.
    pub(crate) fn start(&self) {
        let mut state = self.state.lock().unwrap();
        if state.claim.is_none() {
            state.claim = Some(SpeakingClaim::new(self.owner.clone()));
        }
        state.last_sample = None;
    }
    /// A job's last real sample passed the speaker at `at`: the release, if no further audio comes, is due `linger`
    /// after it.
    pub(crate) fn last_sample(&self, at: std::time::Instant) {
        let mut state = self.state.lock().unwrap();
        if state.claim.is_some() {
            state.last_sample = Some(at);
        }
    }
    /// The instant the held claim ends if the queue stays quiet.
    pub(crate) fn deadline(&self) -> Option<std::time::Instant> {
        let state = self.state.lock().unwrap();
        if state.claim.is_some() { state.last_sample.map(|at| at + self.linger) } else { None }
    }
    /// Ends the claim if the reply has gone quiet for the linger at `now`; returns whether it did.
    pub(crate) fn release_if_due(&self, now: std::time::Instant) -> bool {
        self.release_after(now, self.linger)
    }
    /// Ends the claim if nothing has played for `SPEAKING_SILENCE_CAP` at `now` while the queue waits for more audio;
    /// returns whether it did. Playing audio clears the silence, so this never cuts a sample that is playing.
    pub(crate) fn release_if_silent(&self, now: std::time::Instant) -> bool {
        self.release_after(now, SPEAKING_SILENCE_CAP)
    }
    fn release_after(&self, now: std::time::Instant, after: std::time::Duration) -> bool {
        let mut state = self.state.lock().unwrap();
        let due = state.claim.is_some() && state.last_sample.is_some_and(|at| now >= at + after);
        if due {
            state.claim = None;
            state.last_sample = None;
        }
        due
    }
    /// Ends the claim now: an interrupt or a cancelled reply.
    pub(crate) fn release(&self) {
        let mut state = self.state.lock().unwrap();
        state.claim = None;
        state.last_sample = None;
    }
}
/// What one output stream contributes to the flag: its share of its queue's hold, or its own claim when it plays
/// outside a queue. Every output follows these rules, so the device and the test device behave alike.
pub(crate) struct Audibility {
    hold: Option<Arc<SpeakingHold>>,
    own: Option<SpeakingClaim>,
    audible: bool,
}
impl Audibility {
    pub(crate) fn new(hold: Option<Arc<SpeakingHold>>) -> Self {
        Self { hold, own: None, audible: false }
    }
    /// Called while the stream waits for audio: a reply that has been silent too long lets go of the flag.
    pub(crate) fn poll(&mut self, now: std::time::Instant) {
        if let (false, Some(hold)) = (self.audible, &self.hold) {
            hold.release_if_silent(now);
        }
    }
    /// The first real sample is at the speaker.
    pub(crate) fn audible(&mut self) {
        if self.audible {
            return;
        }
        self.audible = true;
        match &self.hold {
            Some(hold) => hold.start(),
            None => self.own = Some(SpeakingClaim::new(global_speaking())),
        }
    }
    /// The stream ended normally: its last real sample passed the speaker at `last_sample_at`.
    pub(crate) fn ended(self, last_sample_at: std::time::Instant) {
        if let Some(hold) = &self.hold {
            hold.last_sample(last_sample_at);
        }
    }
    /// The stream failed: its audio stops now, and the queue's linger starts now.
    pub(crate) fn failed(self) {
        if let Some(hold) = &self.hold {
            hold.last_sample(std::time::Instant::now());
        }
    }
    /// The stream was cancelled (interrupt, barge-in, delivery ended): the flag falls now.
    pub(crate) fn cancelled(self) {
        if let Some(hold) = &self.hold {
            hold.release();
        }
    }
}
pub const LEVEL_PERIOD: std::time::Duration = std::time::Duration::from_millis(30);
/// Sum of squares (fixed point) and sample count, written by the render callback.
#[derive(Clone, Default)]
struct Meter {
    energy: Arc<AtomicU64>,
    samples: Arc<AtomicU64>,
}
const METER_SCALE: f64 = 1e9;
impl Meter {
    fn add(&self, energy: f32, samples: usize) {
        self.energy.fetch_add((energy as f64 * METER_SCALE) as u64, Ordering::Relaxed);
        self.samples.fetch_add(samples as u64, Ordering::Relaxed);
    }
    fn take(&self) -> f32 {
        let energy = self.energy.swap(0, Ordering::Relaxed) as f64 / METER_SCALE;
        let samples = self.samples.swap(0, Ordering::Relaxed);
        if samples == 0 { 0. } else { (energy / samples as f64).sqrt().min(1.) as f32 }
    }
}
/// Device-frame accounting for one stream, shared by the render callback and the waiting thread. The callback counts
/// every frame it renders. The device plays a frame `latency` frames after the callback renders it, so `played()` is
/// the frame now at the speaker. The first real frame and the frame just past the last real one are recorded when
/// the cursor reaches them.
pub(crate) struct Timeline {
    rate: f64,
    rendered: AtomicU64,
    latency: AtomicU64,
    first: AtomicU64,
    end: AtomicU64,
}
const UNKNOWN: u64 = u64::MAX;
/// How long after the last real frame the flag stays on and the device keeps running, so a device buffer still
/// playing never clips the final phoneme (about two 48 kHz callback buffers).
pub(crate) const TAIL_SECONDS: f64 = 0.02;
/// The longest output latency believed. A larger reading is a bad timestamp, not a second of pipeline.
const MAX_LATENCY_SECONDS: f64 = 1.0;
impl Timeline {
    pub(crate) fn new(rate: u32) -> Self {
        Self {
            rate: rate as f64,
            rendered: AtomicU64::new(0),
            latency: AtomicU64::new(0),
            first: AtomicU64::new(UNKNOWN),
            end: AtomicU64::new(UNKNOWN),
        }
    }
    fn frames_in(&self, seconds: f64) -> u64 {
        (seconds * self.rate).round() as u64
    }
    /// The latency the callback reports for the buffer it is filling: playback instant minus callback instant.
    fn set_latency_from(&self, timestamp: cpal::OutputStreamTimestamp) {
        let seconds = timestamp
            .playback
            .duration_since(&timestamp.callback)
            .unwrap_or_default()
            .as_secs_f64()
            .min(MAX_LATENCY_SECONDS);
        self.latency.store(self.frames_in(seconds), Ordering::Release);
    }
    /// Index of the frame about to be rendered.
    fn reserve(&self) -> u64 {
        self.rendered.fetch_add(1, Ordering::AcqRel)
    }
    /// Frames that have passed the speaker.
    pub(crate) fn played(&self) -> u64 {
        self.rendered.load(Ordering::Acquire).saturating_sub(self.latency.load(Ordering::Acquire))
    }
    /// Frames the render callback has produced (advances while the device calls back).
    pub(crate) fn rendered(&self) -> u64 {
        self.rendered.load(Ordering::Acquire)
    }
    /// The first real frame is at the speaker.
    pub(crate) fn started(&self) -> bool {
        let first = self.first.load(Ordering::Acquire);
        first != UNKNOWN && self.played() > first
    }
    /// The stream has ended and its last real frame, plus the tail, has passed the speaker. The device may stop.
    pub(crate) fn finished(&self) -> bool {
        let end = self.end.load(Ordering::Acquire);
        end != UNKNOWN && self.played() >= end + self.frames_in(TAIL_SECONDS)
    }
    fn mark_first(&self, index: u64) {
        let _ = self.first.compare_exchange(UNKNOWN, index, Ordering::AcqRel, Ordering::Acquire);
    }
    fn mark_end(&self, index: u64) {
        let _ = self.end.compare_exchange(UNKNOWN, index, Ordering::AcqRel, Ordering::Acquire);
    }
    /// Seconds since the last real frame passed the speaker (0 until the stream's end is known).
    pub(crate) fn seconds_past_end(&self) -> f64 {
        let end = self.end.load(Ordering::Acquire);
        if end == UNKNOWN { 0. } else { self.played().saturating_sub(end) as f64 / self.rate }
    }
    #[cfg(test)]
    fn set_latency_frames(&self, frames: u64) {
        self.latency.store(frames, Ordering::Release);
    }
    #[cfg(test)]
    fn end_frame(&self) -> Option<u64> {
        let end = self.end.load(Ordering::Acquire);
        (end != UNKNOWN).then_some(end)
    }
}
fn start_if_current(
    cancel: &dyn Fn() -> bool,
    signal: &crate::Cancellation,
    play: impl FnOnce() -> Result<()>,
) -> Result<bool> {
    if cancel() || signal.cancelled() {
        return Ok(false);
    }
    play()?;
    Ok(true)
}
// voice-underrun (night 1022): the first sentence streams while Drama 3 generates it, and at the user's 160% speed the
// speaker consumed it faster than it arrived: a 150 ms digital-zero hole after the first word (22 Oct, 01:59:41).
// Playback now waits for enough of the sentence before the device opens, adapting to the speed and to how fast
// generation has been running; a stream that still runs dry pauses cleanly (5 ms fades, a short re-buffer) and the
// incident is logged.

/// Jarvis's natural speaking rate for the length estimate of a sentence not yet generated, in characters per second
/// of audio. Typical English speech is about 15; a lower figure only makes the pre-buffer longer (it is capped).
const NATURAL_CHARS_PER_SECOND: f64 = 15.0;
/// The most the first sample waits for the pre-buffer, past the first chunk's arrival.
pub(crate) const PREBUFFER_CAP: std::time::Duration = std::time::Duration::from_millis(1200);
/// The buffer covers the expected shortfall times this, plus `PREBUFFER_SLACK` seconds (chunk granularity: a chunk is
/// 0.37 s of natural speech, 0.23 s at 160%).
const PREBUFFER_SAFETY: f64 = 1.15;
const PREBUFFER_SLACK: f64 = 0.12;
/// How much a stream's own measured arrivals outweigh the remembered rate: seconds of observation equal to the prior.
const RATE_PRIOR_WEIGHT: f64 = 0.4;
/// After running dry mid-sentence, playback resumes once this much audio is queued (or the sentence ends).
pub(crate) const REBUFFER_SECONDS: f64 = 0.35;
/// Fades at a hole's edges, so a pause is a pause and not a click.
const FADE_SECONDS: f64 = 0.005;
/// A stream with nothing to play for this long stops and releases its device (Bluetooth headphones are not held by a
/// silent stream), and starts it again when audio arrives.
pub(crate) const IDLE_RELEASE: std::time::Duration = std::time::Duration::from_secs(2);
/// A gap inside a reply this long is logged when audio resumes (a gap between verified sentences is up to ~2.3 s).
const SLOW_GAP: std::time::Duration = std::time::Duration::from_secs(5);
/// A running stream whose device has rendered nothing for this long (a Bluetooth route wakes in about 1-2 s) has stopped: the device went away or its route
/// changed under it (Bluetooth output, 31 Oct 23:34), and CoreAudio sends neither callbacks nor an error. The stream
/// fails, which drops the speaking flag, instead of waiting for frames that never come (bug note 2026-10-31T23-37-26:
/// the reply stayed on the notch and the next reply queued behind it).
pub(crate) const OUTPUT_STALL: std::time::Duration = std::time::Duration::from_secs(4);
/// The longer wait for a device's first callback (review 1 Nov: a waking Bluetooth route).
pub(crate) const OUTPUT_FIRST_CALLBACK: std::time::Duration = std::time::Duration::from_secs(8);
/// Watches a running stream's rendered-frame count for `OUTPUT_STALL`. Paused (idle-released) time never counts.
pub(crate) struct StallWatch {
    rendered: u64,
    since: std::time::Instant,
    stall: std::time::Duration,
    first_callback: std::time::Duration,
}
impl StallWatch {
    #[cfg(test)]
    pub(crate) fn new(now: std::time::Instant) -> Self {
        Self::with_limits(now, &Limits::DEVICE)
    }
    pub(crate) fn with_limits(now: std::time::Instant, limits: &Limits) -> Self {
        Self { rendered: 0, since: now, stall: limits.stall, first_callback: limits.first_callback }
    }
    /// Whether the device has stalled at `now`, given its rendered-frame count and whether the stream is running.
    pub(crate) fn stalled(&mut self, rendered: u64, running: bool, now: std::time::Instant) -> bool {
        if !running || rendered != self.rendered {
            self.rendered = rendered;
            self.since = now;
            return false;
        }
        // Before its first callback a device may still be waking (a Bluetooth route, a profile switch): longer.
        let limit = if rendered == 0 { self.first_callback } else { self.stall };
        now.saturating_duration_since(self.since) >= limit
    }
}
/// A device lost mid-reply (it stalled, failed, or the default output moved away from it) is reopened on the current
/// default output at most this many times per stream; the next loss fails the stream as before (2 Nov 14:46: the
/// AirPods stopped calling back and the reply was lost; 15 output switches in 3 days).
pub(crate) const MAX_REOPENS: u32 = 2;
/// The times playback waits on a device. `DEVICE` in the app; the recovery tests shorten them.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Limits {
    pub(crate) stall: std::time::Duration,
    pub(crate) first_callback: std::time::Duration,
    /// How often the default output is read while a stream plays.
    pub(crate) route_poll: std::time::Duration,
    /// How long a new default output must hold before a reply moves to it (a connecting headset can flip the
    /// default more than once).
    pub(crate) route_settle: std::time::Duration,
    /// The wait before trying again when a device would not open.
    pub(crate) retry: std::time::Duration,
    /// `IDLE_RELEASE`.
    pub(crate) idle_release: std::time::Duration,
}
impl Limits {
    pub(crate) const DEVICE: Self = Self {
        stall: OUTPUT_STALL,
        first_callback: OUTPUT_FIRST_CALLBACK,
        route_poll: std::time::Duration::from_millis(100),
        route_settle: std::time::Duration::from_millis(250),
        retry: std::time::Duration::from_millis(250),
        idle_release: IDLE_RELEASE,
    };
}
/// Follows the default output while a stream plays on device `opened`: it has moved once a different default has
/// held for `route_settle`. An unknown device, or no default at all (mid-switch), never moves it; a dead device
/// with no new default is the stall watch's.
pub(crate) struct RouteWatch {
    opened: Option<u32>,
    polled: Option<std::time::Instant>,
    candidate: Option<(u32, std::time::Instant)>,
    poll: std::time::Duration,
    settle: std::time::Duration,
}
impl RouteWatch {
    pub(crate) fn new(opened: Option<u32>, limits: &Limits) -> Self {
        Self { opened, polled: None, candidate: None, poll: limits.route_poll, settle: limits.route_settle }
    }
    /// Whether the reply should move at `now`, reading the default (`current`) at most once per poll period.
    pub(crate) fn moved(&mut self, now: std::time::Instant, current: impl FnOnce() -> Option<u32>) -> bool {
        let Some(opened) = self.opened else { return false };
        if self.polled.map_or(true, |at| now.saturating_duration_since(at) >= self.poll) {
            self.polled = Some(now);
            match current() {
                Some(device) if device != opened => {
                    if self.candidate.map(|(candidate, _)| candidate) != Some(device) {
                        self.candidate = Some((device, now));
                    }
                }
                _ => self.candidate = None,
            }
        }
        self.candidate.is_some_and(|(_, since)| now.saturating_duration_since(since) >= self.settle)
    }
}
/// A reply still without its first audio this long after playback began waiting is logged once.
const SLOW_FIRST_AUDIO: std::time::Duration = std::time::Duration::from_secs(10);
/// At most one underrun line per this period; the rest are counted into the next line.
const UNDERRUN_LOG_PERIOD: std::time::Duration = std::time::Duration::from_secs(10);

/// The generation rate remembered between streams, in seconds of natural-speed audio per second of wall time (f64
/// bits). 1.0 until measured.
static GENERATION_RATE: AtomicU64 = AtomicU64::new(0x3FF0_0000_0000_0000);
fn generation_rate() -> f64 {
    f64::from_bits(GENERATION_RATE.load(Ordering::Acquire))
}
fn set_generation_rate(rate: f64) {
    GENERATION_RATE.store(rate.clamp(0.2, 8.0).to_bits(), Ordering::Release);
}

/// When to start the first sentence of a stream. Seconds are of audio as played (already at the user's speed); the
/// speaker consumes one per second. With generation delivering `r` played seconds per second (`r < 1`), playback of a
/// sentence lasting `D` seconds never catches up with generation iff it starts with `D·(1−r)` buffered.
pub(crate) struct Prebuffer {
    started: std::time::Instant,
    /// The first sentence's expected played length, from its text (`None`: no text).
    expected: Option<f64>,
    buffered: f64,
    /// Seconds that arrived after the first chunk (the measured part of the rate).
    arrived: f64,
    /// Remembered generation rate, natural seconds per second.
    prior: f64,
    pace: f64,
    /// A later sentence or the producer's end has arrived: the first sentence is all here.
    complete: bool,
}
impl Prebuffer {
    pub(crate) fn new(now: std::time::Instant, first_seconds: f64, label: Option<&str>, pace: f32, prior: f64) -> Self {
        let pace = (pace as f64).clamp(0.5, 3.0);
        let expected = label
            .map(|text| text.trim().chars().count() as f64)
            .filter(|chars| *chars > 0.)
            .map(|chars| chars / (NATURAL_CHARS_PER_SECOND * pace));
        Self { started: now, expected, buffered: first_seconds, arrived: 0., prior, pace, complete: false }
    }
    /// More audio arrived. `starts_sentence`: it begins a later sentence, so the first one is complete.
    pub(crate) fn arrived(&mut self, seconds: f64, starts_sentence: bool) {
        if starts_sentence {
            self.complete = true;
        } else if !self.complete {
            self.arrived += seconds;
        }
        self.buffered += seconds;
    }
    /// The producer finished: everything there is to play has arrived.
    pub(crate) fn finished(&mut self) {
        self.complete = true;
    }
    /// The generation rate in played seconds per second: the remembered one, then this stream's own arrivals.
    pub(crate) fn rate(&self, now: std::time::Instant) -> f64 {
        let prior = self.prior / self.pace;
        let elapsed = now.saturating_duration_since(self.started).as_secs_f64();
        if elapsed <= 0. {
            return prior;
        }
        let measured = self.arrived / elapsed;
        (prior * RATE_PRIOR_WEIGHT + measured * elapsed) / (RATE_PRIOR_WEIGHT + elapsed)
    }
    /// Seconds to hold before starting at `now`.
    pub(crate) fn needed(&self, now: std::time::Instant) -> f64 {
        let length = self.expected.unwrap_or(2.0);
        let shortfall = (1. - self.rate(now).min(1.)).max(0.);
        PREBUFFER_SAFETY * length * shortfall + PREBUFFER_SLACK
    }
    pub(crate) fn ready(&self, now: std::time::Instant) -> bool {
        self.complete
            || self.buffered >= self.needed(now)
            || now.saturating_duration_since(self.started) >= PREBUFFER_CAP
    }
    /// This stream's measured generation rate in natural seconds per second, once it is worth remembering.
    pub(crate) fn measured(&self, now: std::time::Instant) -> Option<f64> {
        let elapsed = now.saturating_duration_since(self.started).as_secs_f64();
        (elapsed >= 0.25 && self.arrived > 0.).then(|| self.arrived / elapsed * self.pace)
    }
}
/// Remember a measured rate (half old, half new); an underrun lowers it.
fn remember_rate(measured: f64) {
    set_generation_rate(0.5 * generation_rate() + 0.5 * measured);
}
fn rate_after_underrun() {
    set_generation_rate(generation_rate() * 0.8);
}

/// One stretch of silence inside a sentence because generation fell behind.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Underrun {
    /// Seconds of the stream's real audio played before the hole.
    pub(crate) at: f64,
    /// Seconds of silence.
    pub(crate) silent: f64,
    pub(crate) sentence: Option<Arc<str>>,
}
/// Incidents from the render callback to the waiting thread, never blocking the callback (`try_lock`).
#[derive(Default)]
pub(crate) struct Incidents {
    list: Mutex<Vec<Underrun>>,
    lost: AtomicU64,
}
impl Incidents {
    fn push(&self, underrun: Underrun) {
        match self.list.try_lock() {
            Ok(mut list) => list.push(underrun),
            Err(_) => {
                self.lost.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    pub(crate) fn take(&self) -> (Vec<Underrun>, u64) {
        let list = std::mem::take(&mut *self.list.lock().unwrap());
        (list, self.lost.swap(0, Ordering::Relaxed))
    }
}
/// One line per `UNDERRUN_LOG_PERIOD`; incidents in between are counted into the next line.
pub(crate) struct UnderrunLimiter {
    last: Option<std::time::Instant>,
    held: u64,
}
impl UnderrunLimiter {
    pub(crate) const fn new() -> Self {
        Self { last: None, held: 0 }
    }
    /// The line for `underrun` at `now`, or `None` while rate-limited.
    pub(crate) fn line(&mut self, underrun: &Underrun, pace: f32, now: std::time::Instant) -> Option<String> {
        if self.last.is_some_and(|last| now.saturating_duration_since(last) < UNDERRUN_LOG_PERIOD) {
            self.held += 1;
            return None;
        }
        self.last = Some(now);
        let more = std::mem::take(&mut self.held);
        let sentence = underrun
            .sentence
            .as_deref()
            .map(|s| {
                let short: String = s.chars().take(60).collect();
                format!(" in \"{short}{}\"", if s.chars().count() > 60 { "…" } else { "" })
            })
            .unwrap_or_default();
        Some(format!(
            "voice: playback underrun: {:.0} ms of silence {:.2} s into the reply{sentence}; generation fell behind playback at {:.0}% speed{}",
            underrun.silent * 1000.,
            underrun.at,
            pace * 100.,
            if more > 0 { format!(" (and {more} more since the last line)") } else { String::new() }
        ))
    }
}
static UNDERRUN_LOG: Mutex<UnderrunLimiter> = Mutex::new(UnderrunLimiter::new());
fn report_underruns(incidents: &Incidents, pace: f32) {
    let (list, lost) = incidents.take();
    for underrun in &list {
        rate_after_underrun();
        if let Some(line) = UNDERRUN_LOG.lock().unwrap().line(underrun, pace, std::time::Instant::now()) {
            eprintln!("{line}");
        }
    }
    if lost > 0 {
        rate_after_underrun();
        UNDERRUN_LOG.lock().unwrap().held += lost;
    }
}
/// The output route last logged, so a line is written only when it changes.
static LOGGED_ROUTE: Mutex<Option<String>> = Mutex::new(None);
fn route_label(name: &str, transport: crate::route::Transport, rate: u32, channels: u16) -> String {
    format!("{name} ({}, {rate} Hz, {channels} ch)", transport.label())
}
/// "voice: output is now …" when the device or its format differs from the last stream's (21 Oct: a quarter hour of
/// replies played into a device the user could not hear, and nothing said which).
fn log_route(route: &str) {
    if note_route(route) {
        eprintln!("voice: output is now {route}");
    }
}
/// Records the route without a line (a recovery's own line names it); returns whether it changed.
fn note_route(route: &str) -> bool {
    let mut logged = LOGGED_ROUTE.lock().unwrap();
    let changed = logged.as_deref() != Some(route);
    if changed {
        *logged = Some(route.to_string());
    }
    changed
}

/// Where the cursor is: playing, or run dry and waiting for enough audio to go on.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Flow {
    Playing,
    /// Run dry: frames of silence so far, and whether the hole is inside a sentence (it had real audio before).
    Starved { frames: u64 },
}
/// The render callback owns the current chunk. A restart generation invalidates
/// both its remaining samples and stale chunks already queued behind it.
struct PlaybackCursor {
    stream: crate::PlaybackStream,
    current: Vec<f32>,
    generation: u64,
    offset: f64,
    step: f64,
    done: Arc<AtomicBool>,
    /// The label of the chunk now starting, until the waiting thread's slot takes it (never blocks).
    unannounced: Option<Arc<str>>,
    announced: Arc<std::sync::Mutex<Option<Arc<str>>>>,
    /// Real samples produced so far (silence while waiting or cancelled is not counted).
    produced: u64,
    /// Chunks taken off the producer, waiting to play.
    queue: std::collections::VecDeque<crate::PlaybackChunk>,
    /// The producer has finished: nothing beyond `queue` will come.
    closed: bool,
    flow: Flow,
    /// Device frames per second (the chunk's rate over `step`).
    rate: f64,
    /// The sentence now playing (for the underrun line).
    sentence: Option<Arc<str>>,
    /// Gain of the last sample (below 1 while fading out at a hole), and frames left of a fade-in.
    gain: f32,
    fade_in: u32,
    /// The cursor has run dry (read by the waiting thread to release an idle device).
    starving: Arc<AtomicBool>,
    incidents: Arc<Incidents>,
    /// A restart cut the audio: what follows is a new beginning, never a hole.
    restarted: bool,
    /// The last `KEPT_SENTENCES` sentences as rendered, oldest first; the last is the one at the cursor. A device
    /// that dies or a route that moves mid-reply starts again from the sentence the speaker was in (`resume`).
    said: std::collections::VecDeque<Said>,
    /// The producer's sample rate (the chunks' own, before `step` converts it to the device's).
    source_rate: u32,
    /// Timeline frame of the last real sample rendered on the current device.
    last_frame: Option<u64>,
}
/// Sentences kept for a rewind: the one at the cursor and the two before it. A short sentence ("Yes.") can be shorter
/// than a Bluetooth route's latency, so the speaker may still be two sentences behind the cursor.
const KEPT_SENTENCES: usize = 3;
/// One sentence's audio as the cursor rendered it, kept so a reopened device can say it again from its start.
struct Said {
    /// 1-based position in the stream (the recovery log line names it).
    number: u32,
    label: Option<Arc<str>>,
    /// Its finished chunks in order; the chunk now rendering is `PlaybackCursor::current`.
    chunks: Vec<Vec<f32>>,
    /// Timeline frame of its first real sample on the current device.
    frame: Option<u64>,
    /// `PlaybackCursor::produced` when it began.
    produced: u64,
}
impl PlaybackCursor {
    fn new(stream: crate::PlaybackStream, first: crate::PlaybackChunk, step: f64) -> Self {
        let rate = first.audio.sample_rate as f64 / step;
        let source_rate = first.audio.sample_rate;
        let said = Said { number: 1, label: first.label.clone(), chunks: vec![], frame: None, produced: 0 };
        Self {
            stream,
            current: first.audio.samples,
            generation: first.generation,
            offset: 0.,
            step,
            done: Arc::new(AtomicBool::new(false)),
            sentence: first.label.clone(),
            unannounced: first.label,
            announced: Default::default(),
            produced: 0,
            queue: Default::default(),
            closed: false,
            flow: Flow::Playing,
            rate,
            gain: 1.,
            fade_in: 0,
            starving: Arc::new(AtomicBool::new(false)),
            incidents: Default::default(),
            restarted: false,
            said: [said].into(),
            source_rate,
            last_frame: None,
        }
    }
    /// Plays on a device running at `rate` frames per second from the next sample on.
    fn retarget(&mut self, rate: u32) {
        self.step = self.source_rate as f64 / rate.max(1) as f64;
        self.rate = rate.max(1) as f64;
    }
    /// A labelled chunk starts the next sentence.
    fn begin_sentence(&mut self, label: Option<Arc<str>>) {
        let number = self.said.back().map_or(1, |said| said.number + 1);
        self.said.push_back(Said { number, label, chunks: vec![], frame: None, produced: self.produced });
        while self.said.len() > KEPT_SENTENCES {
            self.said.pop_front();
        }
    }
    /// The render callback produced a real sample at timeline frame `index`.
    fn rendered_real(&mut self, index: u64) {
        self.last_frame = Some(index);
        if let Some(said) = self.said.back_mut() {
            said.frame.get_or_insert(index);
        }
    }
    /// What a new device plays after this one was lost with `played` frames of its timeline past the speaker. When
    /// everything rendered was heard, this cursor goes on where it was. Otherwise a new cursor starts again at the
    /// sentence the speaker was in (the last kept one whose first sample had passed it), followed by everything still
    /// queued; the sentence is announced again when it plays. Returns the sentence number it starts again from.
    fn resume(mut self, played: u64) -> (Self, Option<u32>) {
        if self.last_frame.map_or(true, |last| played > last) {
            // Every kept frame counted on the lost device's timeline and was heard: only the sentence in progress stays,
            // unmarked, so a loss on the next device before it plays replays nothing heard (review 4 Nov: it replayed up
            // to three heard sentences).
            self.last_frame = None;
            while self.said.len() > 1 {
                self.said.pop_front();
            }
            if let Some(said) = self.said.back_mut() {
                said.frame = None;
            }
            return (self, None);
        }
        let at = self.said.iter().rposition(|said| said.frame.is_some_and(|frame| played > frame)).unwrap_or(0);
        let mut kept: Vec<Said> = self.said.drain(at..).collect();
        let current = std::mem::take(&mut self.current);
        if let Some(last) = kept.last_mut() {
            if !current.is_empty() {
                last.chunks.push(current);
            }
        }
        let (number, produced) = kept.first().map_or((1, 0), |said| (said.number, said.produced));
        let mut replay = std::collections::VecDeque::new();
        for said in kept {
            for (i, samples) in said.chunks.into_iter().enumerate() {
                replay.push_back(crate::PlaybackChunk {
                    audio: Audio { samples, sample_rate: self.source_rate, first_audio_ms: 0., synthesis_ms: 0. },
                    generation: self.generation,
                    label: if i == 0 { said.label.clone() } else { None },
                });
            }
        }
        let first = replay.pop_front().unwrap_or_else(|| crate::PlaybackChunk {
            audio: Audio { samples: vec![], sample_rate: self.source_rate, first_audio_ms: 0., synthesis_ms: 0. },
            generation: self.generation,
            label: None,
        });
        let mut next = PlaybackCursor::new(self.stream, first, self.step);
        next.rate = self.rate;
        next.source_rate = self.source_rate;
        next.produced = produced;
        if let Some(said) = next.said.back_mut() {
            said.number = number;
            said.produced = produced;
        }
        next.queue = replay;
        next.queue.extend(self.queue);
        next.closed = self.closed;
        next.incidents = self.incidents;
        // Anything not yet announced is said again from the replay.
        *self.announced.lock().unwrap() = None;
        next.announced = self.announced;
        (next, Some(number))
    }
    fn fade_frames(&self) -> f64 {
        (FADE_SECONDS * self.rate).max(1.)
    }
    /// Moves everything the producer has sent so far into `queue`.
    fn drain(&mut self) {
        loop {
            match self.stream.try_next() {
                Ok(chunk) => self.queue.push_back(chunk),
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.closed = true;
                    break;
                }
            }
        }
    }
    /// Seconds of current-generation audio queued, and whether a later sentence starts among it.
    fn queued(&self) -> (f64, bool) {
        let generation = self.stream.generation.load(Ordering::Acquire);
        let mut seconds = 0.;
        let mut sentence = false;
        for chunk in self.queue.iter().filter(|c| c.generation == generation) {
            seconds += chunk.audio.samples.len() as f64 / chunk.audio.sample_rate.max(1) as f64;
            sentence |= chunk.label.is_some();
        }
        (seconds, sentence)
    }
    /// Run dry: may playback go on? (Enough is queued, a new sentence is here, or the producer has finished.)
    fn may_resume(&mut self) -> bool {
        self.drain();
        let (seconds, sentence) = self.queued();
        self.closed || sentence || seconds >= REBUFFER_SECONDS
    }
    /// The next current-generation chunk from the queue, if any.
    fn pop(&mut self) -> Option<crate::PlaybackChunk> {
        let generation = self.stream.generation.load(Ordering::Acquire);
        while let Some(chunk) = self.queue.pop_front() {
            if chunk.generation == generation {
                return Some(chunk);
            }
        }
        None
    }
    fn next(&mut self) -> f32 {
        let generation = self.stream.generation.load(Ordering::Acquire);
        if generation != self.generation {
            self.current.clear();
            self.offset = 0.;
            self.generation = generation;
            // A restarted first sentence is a new beginning, not a hole.
            self.flow = Flow::Playing;
            self.starving.store(false, Ordering::Release);
            self.gain = 1.;
            self.restarted = true;
            // The restarted first sentence replaces what was kept of it.
            let keep = self.said.len().saturating_sub(1);
            self.said.drain(..keep);
            if let Some(said) = self.said.back_mut() {
                said.chunks.clear();
                said.frame = None;
                said.produced = self.produced;
            }
        }
        if let Flow::Starved { frames } = self.flow {
            if !self.may_resume() {
                self.flow = Flow::Starved { frames: frames + 1 };
                return 0.;
            }
        }
        while self.offset as usize >= self.current.len() {
            if self.queue.is_empty() {
                self.drain();
            }
            match self.pop() {
                Some(chunk) => {
                    if let Flow::Starved { frames } = self.flow {
                        // Inside a sentence (the next chunk continues it): a real underrun.
                        if chunk.label.is_none() && self.produced > 0 {
                            self.incidents.push(Underrun {
                                at: self.produced as f64 / self.rate,
                                silent: frames as f64 / self.rate,
                                sentence: self.sentence.clone(),
                            });
                        }
                        self.flow = Flow::Playing;
                        self.starving.store(false, Ordering::Release);
                    }
                    if self.gain < 1. {
                        self.fade_in = self.fade_frames() as u32;
                    }
                    self.restarted = false;
                    self.offset -= self.current.len() as f64;
                    self.offset = self.offset.max(0.);
                    let finished = std::mem::replace(&mut self.current, chunk.audio.samples);
                    if !finished.is_empty() {
                        if let Some(said) = self.said.back_mut() {
                            said.chunks.push(finished);
                        }
                    }
                    self.generation = chunk.generation;
                    if chunk.label.is_some() {
                        self.begin_sentence(chunk.label.clone());
                        self.sentence = chunk.label.clone();
                        self.unannounced = chunk.label;
                    }
                }
                None if self.closed => {
                    self.done.store(true, Ordering::Release);
                    return 0.;
                }
                None => {
                    if self.produced > 0 && !self.restarted {
                        self.flow = Flow::Starved { frames: 1 };
                        self.starving.store(true, Ordering::Release);
                        self.gain = 0.;
                    }
                    self.offset = self.current.len() as f64;
                    return 0.;
                }
            }
        }
        if self.generation != self.stream.generation.load(Ordering::Acquire) {
            return 0.;
        }
        self.stream.playback_receipt().store(true, Ordering::Release);
        if self.unannounced.is_some() {
            if let Ok(mut slot) = self.announced.try_lock() {
                *slot = self.unannounced.take();
            }
        }
        let index = self.offset as usize;
        let fraction = (self.offset - index as f64) as f32;
        let x = self.current[index];
        let y = self.current.get(index + 1).copied().unwrap_or(x);
        self.offset += self.step;
        self.produced += 1;
        // Fade out over the last few milliseconds before a hole: the chunk ends and nothing follows it yet.
        let left = (self.current.len() as f64 - self.offset) / self.step;
        let fade = self.fade_frames();
        let mut gain = 1.;
        // Only real chunks fade (a chunk is at least 0.1 s); a few-sample chunk plays as it is.
        let long = self.current.len() as f64 / self.step >= 2. * fade;
        if long && left < fade {
            if self.queue.is_empty() {
                self.drain();
            }
            if self.queue.is_empty() && !self.closed {
                gain = (left.max(0.) / fade) as f32;
            }
        }
        if self.fade_in > 0 {
            let total = self.fade_frames() as u32;
            gain *= 1. - self.fade_in as f32 / total.max(1) as f32;
            self.fade_in -= 1;
        }
        self.gain = gain;
        (x + (y - x) * fraction) * gain
    }
}
/// Fills one device buffer from the cursor: one sample per frame, silence once cancelled (the cursor does not
/// advance), and the frame indices where the real audio starts and ends. The CoreAudio callbacks and the virtual-time
/// device model run this same loop. Returns the buffer's energy (sum of squares) for the level meter.
fn fill<T: Copy>(
    data: &mut [T],
    channels: usize,
    cursor: &mut PlaybackCursor,
    timeline: &Timeline,
    cancel: &crate::Cancellation,
    convert: impl Fn(f32) -> T,
) -> f32 {
    let mut energy = 0.;
    for frame in data.chunks_mut(channels) {
        let index = timeline.reserve();
        let (ended, produced) = (cursor.done.load(Ordering::Acquire), cursor.produced);
        let v = cancel.sample(&mut || cursor.next()).clamp(-1., 1.);
        if cursor.produced > produced {
            // The first real sample on this device (a reopened device's timeline starts again).
            timeline.mark_first(index);
            cursor.rendered_real(index);
        }
        if !ended && cursor.done.load(Ordering::Acquire) {
            timeline.mark_end(index);
        }
        energy += v * v;
        frame.fill(convert(v));
    }
    energy
}
/// What the pre-buffer wait ended with.
enum Gathered {
    /// Start playing: the chunks to play first, in order (the first one non-empty).
    Play(Vec<crate::PlaybackChunk>),
    /// Cancelled while waiting.
    Cancelled,
    /// The producer finished without any current audio.
    Empty,
    /// The stream's start check said never (`MayStart::Skip`): fail the job unheard.
    Skipped(anyhow::Error),
}
/// Holds the first sentence until the pre-buffer says it may start (`Prebuffer`), taking what arrives off the stream.
/// Nothing is opened meanwhile. A restart drops the cut audio and waits again.
fn gather(
    chunks: &mut crate::PlaybackStream,
    first: crate::PlaybackChunk,
    cancel: &dyn Fn() -> bool,
    signal: &crate::Cancellation,
    audibility: &mut Audibility,
) -> Gathered {
    let seconds = |chunk: &crate::PlaybackChunk| chunk.audio.samples.len() as f64 / chunk.audio.sample_rate.max(1) as f64;
    let mut held = vec![first];
    let mut policy = Prebuffer::new(std::time::Instant::now(), seconds(&held[0]), held[0].label.as_deref(), chunks.pace, generation_rate());
    let mut ready_at = None;
    loop {
        if cancel() || signal.cancelled() {
            return Gathered::Cancelled;
        }
        let current = chunks.generation.load(Ordering::Acquire);
        if held.iter().any(|c| c.generation != current) {
            held.retain(|c| c.generation == current);
            match held.first() {
                Some(first) => {
                    let total: f64 = held.iter().map(seconds).sum();
                    policy = Prebuffer::new(std::time::Instant::now(), total, first.label.as_deref(), chunks.pace, generation_rate());
                    ready_at = None;
                }
                None => match chunks.recv(std::time::Duration::from_millis(5)) {
                    Ok(chunk) => {
                        policy = Prebuffer::new(std::time::Instant::now(), seconds(&chunk), chunk.label.as_deref(), chunks.pace, generation_rate());
                        ready_at = None;
                        held.push(chunk);
                        continue;
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return Gathered::Empty,
                },
            }
        }
        let now = std::time::Instant::now();
        if policy.ready(now) {
            // The rate is measured up to the moment the pre-buffer was full, not over a wait for the start check.
            let full = *ready_at.get_or_insert(now);
            // Right before the first sample: the hold looks at the user again (he may have started during the pre-buffer).
            match chunks.may_start() {
                crate::MayStart::Start => {
                    if let Some(rate) = policy.measured(full) {
                        remember_rate(rate);
                    }
                    chunks.started();
                    return Gathered::Play(held);
                }
                crate::MayStart::Skip(error) => return Gathered::Skipped(error),
                crate::MayStart::Drop => return Gathered::Cancelled,
                crate::MayStart::Wait => {}
            }
        }
        audibility.poll(now);
        match chunks.recv(std::time::Duration::from_millis(5)) {
            Ok(chunk) => {
                policy.arrived(seconds(&chunk), chunk.label.is_some());
                held.push(chunk);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => policy.finished(),
        }
    }
}
/// The parts of playback that touch the platform: which device is the default output, and a stream on it. CoreAudio
/// in the app (`CoreAudio`); a scripted device in the tests, so a dying or moving route is tested without hardware.
pub(crate) trait Outputs {
    type Stream: OutputStream;
    /// The current default output device (CoreAudio's device ID), `None` when there is none or it cannot be read.
    fn current(&mut self) -> Option<u32>;
    /// Opens a stream on the current default output. `bind` is called with the device's frame rate before the stream
    /// is built and returns what its callback renders from: the stream may call back at once (cpal 0.17 starts the
    /// unit while building it).
    fn open(&mut self, bind: &mut dyn FnMut(u32) -> Render) -> Result<Opened<Self::Stream>>;
}
pub(crate) trait OutputStream {
    fn play(&self) -> Result<()>;
    fn pause(&self) -> Result<()>;
}
pub(crate) struct Opened<S> {
    pub(crate) stream: S,
    /// The device it opened on (as `Outputs::current` names it); `None`: unknown, and a default change is not followed.
    pub(crate) device: Option<u32>,
    /// "AirPods Pro (Bluetooth, 48000 Hz, 2 ch)", for the log.
    pub(crate) route: String,
}
/// What one device stream's render callback works from, shared with the waiting thread. Each stream gets its own:
/// when a route is lost the cursor moves on to the next stream's, and a callback of the old one that still runs finds
/// none and plays silence.
#[derive(Clone)]
pub(crate) struct Render {
    cursor: Arc<Mutex<Option<PlaybackCursor>>>,
    timeline: Arc<Timeline>,
    cancel: crate::Cancellation,
    meter: Meter,
    error: Arc<Mutex<Option<String>>>,
}
impl Render {
    /// Fills one device buffer of `channels`-interleaved frames. The waiting thread locks the cursor only briefly
    /// (an idle release, a lost route); the callback plays silence rather than wait for it.
    pub(crate) fn buffer<T: Copy>(&self, data: &mut [T], channels: usize, convert: impl Fn(f32) -> T) {
        let frames = data.len() / channels.max(1);
        let energy = match self.cursor.try_lock() {
            Ok(mut slot) => slot.as_mut().map(|cursor| fill(data, channels, cursor, &self.timeline, &self.cancel, &convert)),
            Err(_) => None,
        };
        match energy {
            Some(energy) => self.meter.add(energy, frames),
            None => {
                data.fill(convert(0.));
                self.meter.add(0., frames);
            }
        }
    }
    /// The stream reported an error; the first one is kept.
    pub(crate) fn fail(&self, error: String) {
        if let Ok(mut slot) = self.error.lock() {
            slot.get_or_insert(error);
        }
    }
}
/// The hooks a stream reports to: the installed ones in the app, the tests' own in the tests.
#[derive(Clone, Default)]
pub(crate) struct Hooks {
    pub(crate) level: Option<LevelHook>,
    pub(crate) spoken: Option<SpokenTextHook>,
}
impl Hooks {
    fn installed() -> Self {
        Self { level: LEVEL_HOOK.read().unwrap().clone(), spoken: SPOKEN_HOOK.read().unwrap().clone() }
    }
}
/// How a device was lost mid-reply.
#[derive(Debug)]
enum Loss {
    /// It stopped calling back (cpal pauses a default-output stream whose device dies and reports nothing).
    Stalled,
    /// Its stream reported an error, or would not start or stop.
    Failed(String),
    /// The default output moved to another device.
    Moved,
}
impl Loss {
    /// The failure as the stream's error states it when it gives up.
    fn message(&self, route: &str, limits: &Limits) -> String {
        match self {
            Loss::Stalled => format!("CoreAudio output stopped calling back for {:.0} s on {route}", limits.stall.as_secs_f64()),
            Loss::Failed(e) => format!("CoreAudio output stream failed on {route}: {e}"),
            Loss::Moved => format!("the default output moved away from {route}"),
        }
    }
}
/// A loss being recovered from: what was lost, on which route, and the sentence the reply starts again from.
struct Lost {
    loss: Loss,
    route: String,
    resumed: Option<u32>,
}
impl Lost {
    /// The one log line of a recovery, once the stream is open on `to`.
    fn line(&self, to: &str, limits: &Limits) -> String {
        let resumed = match self.resumed {
            Some(number) => format!("resumed from sentence {number}"),
            None => "resumed where it stopped".to_string(),
        };
        match &self.loss {
            Loss::Moved => format!("voice: output moved to {to} mid-reply; {resumed}"),
            loss => format!("voice: {}; reopened on {to}, {resumed}", loss.message(&self.route, limits)),
        }
    }
}
/// How one device stream ended.
enum Ended {
    /// The last real sample passed the speaker at this instant.
    Finished(std::time::Instant),
    Cancelled,
    Lost(Loss),
}
/// Waits `wait` unless cancelled first; returns whether it was.
fn wait_unless(cancel: &dyn Fn() -> bool, signal: &crate::Cancellation, wait: std::time::Duration) -> bool {
    let until = std::time::Instant::now() + wait;
    while std::time::Instant::now() < until {
        if cancel() || signal.cancelled() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    cancel() || signal.cancelled()
}
/// Plays one stream on `outputs`' default device. A device lost mid-reply (it stops calling back, its stream fails,
/// or the default output moves to another device) is closed and the reply reopened on the current default, from the
/// start of the sentence the speaker was in, at most `MAX_REOPENS` times; the next loss fails the stream as before.
/// Across a reopen the speaking claim is kept (one claim, released once), the spoken-text hook announces the
/// replayed sentence again (at most once more), underruns keep their count, and a cancel stops everything.
pub(crate) fn play_reply<O: Outputs>(
    outputs: &mut O,
    mut chunks: crate::PlaybackStream,
    cancel: &dyn Fn() -> bool,
    signal: crate::Cancellation,
    hooks: &Hooks,
    limits: &Limits,
) -> Result<()> {
    // The stream's share of the speaking flag, from the start: while the job waits for its first sample (its
    // synthesis gap), the queue's silence cap is polled here.
    let mut audibility = Audibility::new(chunks.speaking());
    let waiting = std::time::Instant::now();
    let mut slow_logged = false;
    let first = loop {
        if cancel() || signal.cancelled() {
            audibility.cancelled();
            return Ok(());
        }
        audibility.poll(std::time::Instant::now());
        if !slow_logged && waiting.elapsed() >= SLOW_FIRST_AUDIO {
            slow_logged = true;
            eprintln!(
                "voice: no audio {} s into a reply; generation is slow or stuck",
                SLOW_FIRST_AUDIO.as_secs()
            );
        }
        match chunks.recv(std::time::Duration::from_millis(5)) {
            Ok(a) => break a,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                audibility.ended(std::time::Instant::now());
                return Ok(());
            }
        }
    };
    if slow_logged {
        eprintln!("voice: first audio arrived after {:.1} s", waiting.elapsed().as_secs_f64());
    }
    let mut held = match gather(&mut chunks, first, cancel, &signal, &mut audibility) {
        Gathered::Play(held) => held,
        Gathered::Cancelled => {
            audibility.cancelled();
            return Ok(());
        }
        Gathered::Empty => {
            audibility.ended(std::time::Instant::now());
            return Ok(());
        }
        Gathered::Skipped(error) => {
            audibility.cancelled();
            return Err(error);
        }
    };
    let first = held.remove(0);
    let pace = chunks.pace;
    let meter = Meter::default();
    // The step is set for each device as it opens (`retarget`).
    let mut cursor = PlaybackCursor::new(chunks, first, 1.);
    cursor.queue.extend(held);
    // Both outlive every reopen: `resume` hands them to the next cursor.
    let announced = cursor.announced.clone();
    let incidents = cursor.incidents.clone();
    let mut reported = std::time::Instant::now();
    let mut report = |force: bool| {
        if let Some(hook) = &hooks.level {
            if force || reported.elapsed() >= LEVEL_PERIOD {
                reported = std::time::Instant::now();
                hook(meter.take());
            }
        }
        if let Some(spoken) = &hooks.spoken {
            let started = announced.lock().unwrap().take();
            if let Some(text) = started {
                spoken(Some(&text));
            }
        }
    };
    // The cursor waiting for a device, the reopens spent, and why the next open is a reopen.
    let mut next = Some(cursor);
    let mut reopens = 0;
    let mut lost: Option<Lost> = None;
    let outcome: Result<Option<std::time::Instant>> = loop {
        let mut bound: Option<(Render, Arc<AtomicBool>)> = None;
        let opened = outputs.open(&mut |rate| {
            let mut cursor = next.take().expect("one cursor per open");
            cursor.retarget(rate);
            let starving = cursor.starving.clone();
            let render = Render {
                cursor: Arc::new(Mutex::new(Some(cursor))),
                timeline: Arc::new(Timeline::new(rate)),
                cancel: signal.clone(),
                meter: meter.clone(),
                error: Default::default(),
            };
            bound = Some((render.clone(), starving));
            render
        });
        let (opened, render, starving) = match (opened, bound) {
            (Ok(opened), Some((render, starving))) => (opened, render, starving),
            (result, bound) => {
                // The device would not open: the cursor comes back for the next try.
                if let Some((render, _)) = bound {
                    next = render.cursor.lock().unwrap().take();
                }
                let error = match result {
                    Err(error) => error,
                    Ok(_) => anyhow::anyhow!("the output opened without its render state"),
                };
                if next.is_none() {
                    break Err(error);
                }
                if cancel() || signal.cancelled() {
                    break Ok(None);
                }
                if reopens >= MAX_REOPENS {
                    break Err(match &lost {
                        Some(lost) => anyhow::anyhow!("{}; reopening failed: {error:#}", lost.loss.message(&lost.route, limits)),
                        None => error,
                    });
                }
                reopens += 1;
                eprintln!("voice: could not open the output ({error:#}); trying again");
                if wait_unless(cancel, &signal, limits.retry) {
                    break Ok(None);
                }
                continue;
            }
        };
        match lost.take() {
            Some(lost) => {
                note_route(&opened.route);
                eprintln!("{}", lost.line(&opened.route, limits));
            }
            None => log_route(&opened.route),
        }
        let stream = opened.stream;
        let ended = 'session: {
            match start_if_current(cancel, &signal, || stream.play()) {
                Ok(true) => {}
                Ok(false) => break 'session Ended::Cancelled,
                Err(e) => break 'session Ended::Lost(Loss::Failed(format!("{e:#}"))),
            }
            // The flag is claimed when the first real sample reaches the speaker. A reply's claim outlives this
            // stream (the queue holds it); the end of the last real sample is what the linger counts from.
            let mut dry_since: Option<std::time::Instant> = None;
            let mut released: Option<std::time::Instant> = None;
            let mut stall = StallWatch::with_limits(std::time::Instant::now(), limits);
            // A reopened device gets the plain stall limit for its first callback, not the longer wake allowance: a
            // dead default would otherwise hold the speaking flag 4 + 8 + 8 s before the reply fails (review 4 Nov).
            if reopens > 0 {
                stall.first_callback = stall.first_callback.min(limits.stall);
            }
            let mut route = RouteWatch::new(opened.device, limits);
            loop {
                if cancel() || signal.cancelled() {
                    break 'session Ended::Cancelled;
                }
                if let Some(e) = render.error.lock().unwrap().clone() {
                    break 'session Ended::Lost(Loss::Failed(e));
                }
                report_underruns(&incidents, pace);
                let now = std::time::Instant::now();
                if stall.stalled(render.timeline.rendered(), released.is_none(), now) {
                    break 'session Ended::Lost(Loss::Stalled);
                }
                // With no reopen left, a moved default is not followed: the old device plays on while it can.
                let moved = reopens < MAX_REOPENS && route.moved(now, || outputs.current());
                match released {
                    // Stopped while idle: start again once there is audio to play (or the stream has ended), on
                    // the new default if the route moved meanwhile.
                    Some(at) => {
                        let resume = render.cursor.lock().unwrap().as_mut().is_some_and(|cursor| cursor.may_resume());
                        if resume {
                            let gap = at.elapsed() + limits.idle_release;
                            if gap >= SLOW_GAP {
                                eprintln!("voice: {:.1} s without audio mid-reply; generation was slow or stuck", gap.as_secs_f64());
                            }
                            if moved {
                                break 'session Ended::Lost(Loss::Moved);
                            }
                            if let Err(e) = stream.play() {
                                break 'session Ended::Lost(Loss::Failed(format!("{e:#}")));
                            }
                            released = None;
                            dry_since = None;
                        }
                    }
                    None if moved => break 'session Ended::Lost(Loss::Moved),
                    None if starving.load(Ordering::Acquire) => {
                        let since = *dry_since.get_or_insert_with(std::time::Instant::now);
                        if since.elapsed() >= limits.idle_release {
                            if let Err(e) = stream.pause() {
                                break 'session Ended::Lost(Loss::Failed(format!("{e:#}")));
                            }
                            released = Some(std::time::Instant::now());
                        }
                    }
                    None => dry_since = None,
                }
                if render.timeline.started() {
                    audibility.audible();
                }
                if render.timeline.finished() {
                    break 'session Ended::Finished(
                        std::time::Instant::now() - std::time::Duration::from_secs_f64(render.timeline.seconds_past_end()),
                    );
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
                report(false);
            }
        };
        // This stream is done, however it ended: its callback finds no cursor from here on, then it stops.
        let cursor = render.cursor.lock().unwrap().take();
        let _ = stream.pause();
        drop(stream);
        let loss = match ended {
            Ended::Finished(at) => break Ok(Some(at)),
            Ended::Cancelled => break Ok(None),
            Ended::Lost(loss) => loss,
        };
        // An interrupt during the loss or the reopen stops everything.
        if cancel() || signal.cancelled() {
            break Ok(None);
        }
        let message = loss.message(&opened.route, limits);
        let Some(cursor) = cursor else { break Err(anyhow::anyhow!("{message}")) };
        let played = render.timeline.played();
        // Everything already passed the speaker and nothing more is coming: the reply is complete.
        if cursor.done.load(Ordering::Acquire) && cursor.last_frame.map_or(true, |last| played > last) {
            break Ok(Some(std::time::Instant::now()));
        }
        if reopens >= MAX_REOPENS {
            break Err(anyhow::anyhow!("{message} (after {reopens} reopens)"));
        }
        reopens += 1;
        let (cursor, resumed) = cursor.resume(played);
        next = Some(cursor);
        lost = Some(Lost { loss, route: opened.route, resumed });
    };
    report_underruns(&incidents, pace);
    match &outcome {
        Ok(Some(last_sample_at)) => audibility.ended(*last_sample_at),
        Ok(None) => audibility.cancelled(),
        Err(_) => audibility.failed(),
    }
    if let Some(hook) = &hooks.level {
        hook(0.);
    }
    if let Some(spoken) = &hooks.spoken {
        spoken(None);
    }
    outcome.map(|_| ())
}
/// The app's outputs: CoreAudio through CPAL, on the default output device.
struct CoreAudio;
impl OutputStream for cpal::Stream {
    fn play(&self) -> Result<()> {
        StreamTrait::play(self).map_err(Into::into)
    }
    fn pause(&self) -> Result<()> {
        StreamTrait::pause(self).map_err(Into::into)
    }
}
impl Outputs for CoreAudio {
    type Stream = cpal::Stream;
    fn current(&mut self) -> Option<u32> {
        crate::route::default_output_id()
    }
    fn open(&mut self, bind: &mut dyn FnMut(u32) -> Render) -> Result<Opened<cpal::Stream>> {
        let id = crate::route::default_output_id();
        let device = cpal::default_host()
            .default_output_device()
            .context("no audio output device")?;
        let supported = device.default_output_config()?;
        let config = supported.config();
        let channels = config.channels as usize;
        let route = crate::route::default_output();
        let label = route_label(
            route.as_ref().map(|r| r.name.as_str()).unwrap_or("default output"),
            route.as_ref().map(|r| r.transport).unwrap_or(crate::route::Transport::Unknown),
            config.sample_rate,
            config.channels,
        );
        let render = bind(config.sample_rate);
        macro_rules! build {
            ($t:ty, $convert:expr) => {{
                let callback = render.clone();
                let errors = render.clone();
                device.build_output_stream(
                    &config,
                    move |data: &mut [$t], info: &cpal::OutputCallbackInfo| {
                        callback.timeline.set_latency_from(info.timestamp());
                        callback.buffer(data, channels, $convert);
                    },
                    move |e: cpal::StreamError| errors.fail(e.to_string()),
                    None,
                )?
            }};
        }
        let stream = match supported.sample_format() {
            cpal::SampleFormat::F32 => build!(f32, |v: f32| v),
            cpal::SampleFormat::I16 => build!(i16, |v: f32| (v * 32767.) as i16),
            cpal::SampleFormat::U16 => build!(u16, |v: f32| ((v + 1.) * 32767.5) as u16),
            format => anyhow::bail!("unsupported output format {format:?}"),
        };
        Ok(Opened { stream, device: id, route: label })
    }
}
/// CPAL uses Apple's CoreAudio backend on macOS. Constructed lazily on first output.
pub struct CoreAudioOutput;
impl AudioOutput for CoreAudioOutput {
    fn output_stream(
        &mut self,
        chunks: crate::PlaybackStream,
        cancel: &dyn Fn() -> bool,
        signal: crate::Cancellation,
    ) -> Result<()> {
        play_reply(&mut CoreAudio, chunks, cancel, signal, &Hooks::installed(), &Limits::DEVICE)
    }

    fn output(&mut self, a: &Audio, cancel: &dyn Fn() -> bool) -> Result<()> {
        self.output_cancellable(a, cancel, crate::Cancellation::new(|| false))
    }
    fn output_cancellable(
        &mut self,
        a: &Audio,
        cancel: &dyn Fn() -> bool,
        signal: crate::Cancellation,
    ) -> Result<()> {
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(crate::PlaybackChunk {
            audio: a.clone(),
            generation: 0,
            label: None,
        })?;
        drop(tx);
        self.output_stream(
            crate::PlaybackStream::new(rx, Arc::new(std::sync::atomic::AtomicU64::new(0))),
            cancel,
            signal,
        )
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::Cancellation;
    use std::sync::atomic::AtomicUsize;
    #[test]
    fn restart_cuts_current_prefix_and_queued_tail_before_retry_or_fallback() {
        let generation = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let (tx, rx) = std::sync::mpsc::channel();
        let audio = |sample| Audio {
            samples: vec![sample; 4],
            sample_rate: 44100,
            first_audio_ms: 0.,
            synthesis_ms: 0.,
        };
        tx.send(crate::PlaybackChunk {
            audio: audio(0.2),
            generation: 0,
            label: None,
        })
        .unwrap();
        let mut cursor = PlaybackCursor::new(
            crate::PlaybackStream::new(rx, generation.clone()),
            crate::PlaybackChunk { audio: audio(0.1), generation: 0, label: None },
            1.,
        );
        assert_eq!(cursor.next(), 0.1); // The listener heard the prefix begin.
        generation.fetch_add(1, Ordering::AcqRel);
        assert_eq!(cursor.next(), 0.); // Current prefix AND queued old tail were cut.
        tx.send(crate::PlaybackChunk {
            audio: audio(0.8),
            generation: 1,
            label: None,
        })
        .unwrap();
        for _ in 0..4 {
            assert_eq!(cursor.next(), 0.8);
        }
        assert_eq!(cursor.next(), 0.);
    }
    #[test]
    fn callback_silences_an_invalidated_generation_without_consuming_audio() {
        let generation = Arc::new(AtomicUsize::new(0));
        let callback = generation.clone();
        let signal = Cancellation::new(move || callback.load(Ordering::Acquire) != 0);
        let mut frames = 0;
        let mut next = || {
            frames += 1;
            0.5
        };
        assert_eq!(signal.sample(&mut next), 0.5);
        generation.fetch_add(1, Ordering::AcqRel);
        for _ in 0..512 {
            assert_eq!(signal.sample(&mut next), 0.);
        }
        assert_eq!(frames, 1);
    }
    #[test]
    fn a_sentence_is_announced_when_its_first_sample_plays_not_when_queued() {
        let (tx, rx) = std::sync::mpsc::channel();
        let audio = |v| Audio { samples: vec![v; 2], sample_rate: 44100, first_audio_ms: 0., synthesis_ms: 0. };
        let announced = Arc::new(std::sync::Mutex::new(None));
        let mut cursor = PlaybackCursor::new(
            crate::PlaybackStream::new(rx, Arc::new(std::sync::atomic::AtomicU64::new(0))),
            crate::PlaybackChunk { audio: audio(0.1), generation: 0, label: Some("First.".into()) },
            1.,
        );
        cursor.announced = announced.clone();
        tx.send(crate::PlaybackChunk { audio: audio(0.2), generation: 0, label: Some("Second, with a comma.".into()) }).unwrap();
        assert_eq!(cursor.next(), 0.1);
        assert_eq!(announced.lock().unwrap().take().as_deref(), Some("First."));
        assert_eq!(cursor.next(), 0.1);
        assert!(announced.lock().unwrap().is_none(), "the next sentence is queued, not playing");
        assert_eq!(cursor.next(), 0.2);
        assert_eq!(announced.lock().unwrap().take().as_deref(), Some("Second, with a comma."));
    }
    /// Bug note 2026-10-31T23-37-26: a device that stops calling back fails the stream after `OUTPUT_STALL`; a
    /// running device, and any time paused while idle, never do.
    #[test]
    fn a_device_that_stops_calling_back_is_a_stalled_stream() {
        let t0 = std::time::Instant::now();
        let at = |s: f64| t0 + std::time::Duration::from_secs_f64(s);
        let mut w = StallWatch::new(t0);
        // Rendering: never stalled.
        for i in 1..100u64 {
            assert!(!w.stalled(i * 480, true, at(i as f64 * 0.1)));
        }
        // Frames stop advancing: stalled only after the threshold.
        let last = 99 * 480;
        assert!(!w.stalled(last, true, at(9.9 + OUTPUT_STALL.as_secs_f64() - 0.1)));
        assert!(w.stalled(last, true, at(9.9 + OUTPUT_STALL.as_secs_f64() + 0.1)));
        // Paused while idle: no count, however long, and resuming starts the clock again.
        let mut w = StallWatch::new(t0);
        assert!(!w.stalled(0, false, at(30.)));
        assert!(!w.stalled(0, true, at(30.1)));
        // Never called back yet: the longer first-callback budget.
        assert!(!w.stalled(0, true, at(30. + OUTPUT_FIRST_CALLBACK.as_secs_f64() - 0.1)));
        assert!(w.stalled(0, true, at(30.2 + OUTPUT_FIRST_CALLBACK.as_secs_f64())));
    }
    #[test]
    fn meter_reports_rms_per_period_and_zero_when_nothing_played() {
        let meter = Meter::default();
        assert_eq!(meter.take(), 0.);
        // 0.5 amplitude square wave over two callbacks: RMS 0.5.
        meter.add(0.25 * 512., 512);
        meter.add(0.25 * 1024., 1024);
        assert!((meter.take() - 0.5).abs() < 1e-4);
        assert_eq!(meter.take(), 0.);
    }
    #[test]
    fn cancelled_setup_never_starts_the_device() {
        let generation = Arc::new(AtomicUsize::new(0));
        let callback = generation.clone();
        let signal = Cancellation::new(move || callback.load(Ordering::Acquire) != 0);
        generation.fetch_add(1, Ordering::AcqRel); // Cancellation during device setup.
        let mut played = false;
        start_if_current(&|| false, &signal, || {
            played = true;
            Ok(())
        })
        .unwrap();
        assert!(!played);
    }
    /// The offline device the CoreAudio callback is modelled by: the same `fill` loop, one buffer per step at the
    /// device rate, with a fixed output latency. Virtual time; no hardware, no sound.
    struct Device {
        timeline: Timeline,
        cursor: PlaybackCursor,
        cancel: Cancellation,
        /// Every frame the callback has handed to the device, in order.
        handed: Vec<f32>,
    }
    impl Device {
        fn new(rate: u32, samples: Vec<f32>, latency: u64) -> Self {
            let (tx, rx) = std::sync::mpsc::channel();
            let audio = Audio { samples, sample_rate: rate, first_audio_ms: 0., synthesis_ms: 0. };
            tx.send(crate::PlaybackChunk { audio, generation: 0, label: None }).unwrap();
            // The producer finishes at once: the stream's end is known long before the speaker reaches it.
            drop(tx);
            let timeline = Timeline::new(rate);
            timeline.set_latency_frames(latency);
            Self {
                timeline,
                cursor: PlaybackCursor::new(
                    crate::PlaybackStream::new(rx, Arc::new(AtomicU64::new(0))),
                    crate::PlaybackChunk { audio: Audio { samples: vec![], sample_rate: rate, first_audio_ms: 0., synthesis_ms: 0. }, generation: 0, label: None },
                    1.,
                ),
                cancel: Cancellation::new(|| false),
                handed: vec![],
            }
        }
        /// One device buffer: what the callback fills, and what the waiting thread then sees.
        fn buffer(&mut self, frames: usize) {
            let mut data = vec![0f32; frames];
            fill(&mut data, 1, &mut self.cursor, &self.timeline, &self.cancel, |v| v);
            self.handed.extend_from_slice(&data);
        }
    }
    /// A signal with no silence in it, so a clipped tail shows as zeros.
    fn ramp(frames: usize) -> Vec<f32> {
        (0..frames).map(|i| 0.5 + 0.4 * ((i as f32) * 0.01).sin()).collect()
    }
    // voice-underrun (night 1022): the streamed first sentence against a producer slower than playback.
    const SIM_RATE: u32 = 44_100;
    /// Drama 3's streamed chunk: 0.37 s of natural speech.
    const SIM_CHUNK: f64 = 0.37;
    /// Virtual-time model of one streamed first sentence (3 s of natural speech): chunks arrive at `generation` natural
    /// seconds per second, the first 450 ms after the request, each stretched to `pace`. Playback starts when the
    /// pre-buffer says so (or at once, the old behaviour), then the device pulls 10 ms buffers through the real cursor
    /// and `fill`. Returns every frame handed to the device, the underruns, and the start delay after the first chunk.
    fn stream_sim(pace: f32, generation: f64, prebuffer: bool) -> (Vec<f32>, Vec<Underrun>, f64) {
        use std::time::{Duration, Instant};
        let natural = 3.0;
        let label: Arc<str> = "a".repeat((natural * NATURAL_CHARS_PER_SECOND) as usize).into();
        let total = (natural / pace as f64 * SIM_RATE as f64).round() as usize;
        let per_chunk = (SIM_CHUNK / pace as f64 * SIM_RATE as f64).round() as usize;
        let mut chunks = vec![];
        let mut index = 0;
        while index < total {
            let end = (index + per_chunk).min(total);
            let arrival = 0.45 + chunks.len() as f64 * SIM_CHUNK / generation;
            let samples = (index..end).map(|i| 0.5 + 0.4 * ((i as f32) * 0.01).sin()).collect::<Vec<_>>();
            chunks.push((arrival, samples));
            index = end;
        }
        let base = Instant::now();
        let at = |t: f64| base + Duration::from_secs_f64(t);
        let (tx, rx) = std::sync::mpsc::channel();
        let mut tx = Some(tx);
        let mut rx = Some(rx);
        let mut held: Vec<crate::PlaybackChunk> = vec![];
        let mut policy: Option<Prebuffer> = None;
        let mut cursor: Option<PlaybackCursor> = None;
        let timeline = Timeline::new(SIM_RATE);
        let cancel = Cancellation::new(|| false);
        let mut handed = vec![];
        let mut next = 0;
        let mut start_delay = 0.;
        let buffer = SIM_RATE as usize / 100;
        let mut t = 0.;
        loop {
            while next < chunks.len() && chunks[next].0 <= t {
                let samples = chunks[next].1.clone();
                let seconds = samples.len() as f64 / SIM_RATE as f64;
                let chunk = crate::PlaybackChunk {
                    audio: Audio { samples, sample_rate: SIM_RATE, first_audio_ms: 0., synthesis_ms: 0. },
                    generation: 0,
                    label: (next == 0).then(|| label.clone()),
                };
                match (&cursor, &mut policy) {
                    (Some(_), _) => tx.as_ref().unwrap().send(chunk).unwrap(),
                    (None, Some(policy)) => {
                        policy.arrived(seconds, false);
                        held.push(chunk);
                    }
                    (None, None) => {
                        policy = Some(Prebuffer::new(at(t), seconds, Some(&label), pace, 1.0));
                        held.push(chunk);
                    }
                }
                next += 1;
                if next == chunks.len() {
                    if let Some(policy) = &mut policy {
                        policy.finished();
                    }
                }
            }
            if cursor.is_none() && policy.as_ref().is_some_and(|p| !prebuffer || p.ready(at(t))) {
                start_delay = t - chunks[0].0;
                let mut c = PlaybackCursor::new(
                    crate::PlaybackStream::new(rx.take().unwrap(), Arc::new(AtomicU64::new(0))),
                    held.remove(0),
                    1.,
                );
                c.queue.extend(held.drain(..));
                cursor = Some(c);
            }
            if cursor.is_some() && next == chunks.len() {
                tx = None;
            }
            if let Some(c) = &mut cursor {
                let mut data = vec![0f32; buffer];
                fill(&mut data, 1, c, &timeline, &cancel, |v| v);
                handed.extend_from_slice(&data);
                if c.done.load(Ordering::Acquire) {
                    let (incidents, lost) = c.incidents.take();
                    assert_eq!(lost, 0);
                    return (handed, incidents, start_delay);
                }
            }
            t += buffer as f64 / SIM_RATE as f64;
            assert!(t < 30., "the stream never finished");
        }
    }
    /// Frames of exact digital zero between the first and the last real sample: holes inside the speech.
    fn zeros_inside(frames: &[f32]) -> usize {
        let first = frames.iter().position(|v| *v != 0.).expect("no speech");
        let last = frames.iter().rposition(|v| *v != 0.).unwrap();
        frames[first..=last].iter().filter(|v| **v == 0.).count()
    }
    #[test]
    fn the_old_start_left_a_hole_at_160_percent_and_logged_nothing_now_it_is_an_incident() {
        // Generation at 1.3x natural speed: faster than real time, slower than the user's 160% playback.
        let (frames, incidents, delay) = stream_sim(1.6, 1.3, false);
        assert!(delay < 0.011, "started at once ({delay} s)");
        assert!(zeros_inside(&frames) > (0.03 * SIM_RATE as f64) as usize, "the regression case: a hole after the first chunk");
        let first = incidents.first().expect("the underrun is recorded");
        assert!((first.at - SIM_CHUNK / 1.6).abs() < 0.02, "the hole is after the first chunk: {first:?}");
        assert!(first.silent >= REBUFFER_SECONDS - 0.02, "it re-buffers rather than stutter: {first:?}");
        assert!(first.sentence.is_some());
    }
    #[test]
    fn prebuffered_first_sentence_has_no_hole_at_100_or_160_percent_within_the_latency_cap() {
        for (pace, generation) in [(1.0, 1.3), (1.6, 1.3), (1.6, 1.0), (1.15, 0.9)] {
            let (frames, incidents, delay) = stream_sim(pace, generation, true);
            assert_eq!(zeros_inside(&frames), 0, "a hole at {pace}x with generation {generation}x");
            assert!(incidents.is_empty(), "{incidents:?} at {pace}x");
            assert!(delay <= PREBUFFER_CAP.as_secs_f64() + 0.011, "start waited {delay} s at {pace}x");
            if pace == 1.0 {
                assert!(delay < 0.02, "faster-than-playback generation starts at once ({delay} s)");
            } else {
                assert!(delay > 0.1, "slower-than-playback generation waits ({delay} s at {pace}x)");
            }
        }
    }
    #[test]
    fn a_producer_far_too_slow_still_starts_by_the_cap_and_its_holes_are_recorded() {
        let (frames, incidents, delay) = stream_sim(1.6, 0.5, true);
        assert!((delay - PREBUFFER_CAP.as_secs_f64()).abs() < 0.011, "waited {delay} s");
        assert!(!incidents.is_empty() && zeros_inside(&frames) > 0, "the remaining holes are pauses, and logged");
    }
    #[test]
    fn prebuffer_estimates_and_measures_the_rate() {
        use std::time::{Duration, Instant};
        let now = Instant::now();
        let ms = |n: u64| now + Duration::from_millis(n);
        // 30 characters at 160%: 1.25 s expected; remembered rate 1.0 natural = 0.625 played seconds per second.
        let mut p = Prebuffer::new(now, 0.23, Some("Thirty characters, near enough"), 1.6, 1.0);
        assert!((p.rate(now) - 0.625).abs() < 1e-6);
        assert!(!p.ready(now), "0.23 s does not cover a 1.25 s sentence at 0.625");
        // Arrivals at 0.81 played seconds per second move the estimate toward it.
        for i in 1..=4 {
            p.arrived(0.231, false);
            let r = p.rate(ms(285 * i));
            assert!(r > 0.625 && r < 0.81, "{r}");
        }
        assert!(p.ready(ms(1140)));
        let measured = p.measured(ms(1140)).unwrap();
        assert!((measured - 0.924 / 1.14 * 1.6).abs() < 1e-5, "remembered at natural speed: {measured}");
        // A later sentence arriving means the first is complete: start.
        let mut q = Prebuffer::new(now, 0.1, Some(&"x".repeat(200)), 1.6, 0.3);
        assert!(!q.ready(now));
        q.arrived(2.0, true);
        assert!(q.ready(now));
        // The cap holds however slow.
        let r = Prebuffer::new(now, 0.01, Some(&"x".repeat(500)), 1.6, 0.2);
        assert!(!r.ready(now + PREBUFFER_CAP - Duration::from_millis(1)));
        assert!(r.ready(now + PREBUFFER_CAP));
    }
    #[test]
    fn gather_holds_a_slow_first_sentence_and_lets_a_fast_one_go() {
        use std::time::{Duration, Instant};
        let chunk = |seconds: f64, label: Option<&str>| crate::PlaybackChunk {
            audio: Audio { samples: vec![0.3; (seconds * 44_100.) as usize], sample_rate: 44_100, first_audio_ms: 0., synthesis_ms: 0. },
            generation: 0,
            label: label.map(Into::into),
        };
        // Slow: 0.1 s of played audio every 150 ms (0.67x) at 160%, a 20-character sentence (0.83 s expected).
        let (tx, rx) = std::sync::mpsc::sync_channel(64);
        let producer = std::thread::spawn(move || {
            for _ in 0..12 {
                std::thread::sleep(Duration::from_millis(150));
                if tx.send(chunk(0.1, None)).is_err() { return; }
            }
        });
        let mut stream = crate::PlaybackStream::new(rx, Arc::new(AtomicU64::new(0))).with_pace(1.6);
        let mut audibility = Audibility::new(None);
        let start = Instant::now();
        let first = chunk(0.1, Some("Twenty characters ok"));
        let Gathered::Play(held) = gather(&mut stream, first, &|| false, &Cancellation::new(|| false), &mut audibility) else { panic!() };
        let waited = start.elapsed();
        assert!(held.len() >= 3, "held {} chunks", held.len());
        assert!(waited >= Duration::from_millis(250) && waited <= PREBUFFER_CAP + Duration::from_millis(50), "{waited:?}");
        drop(stream);
        producer.join().unwrap();
        // Fast: the whole sentence is already here and the producer is done.
        let (tx, rx) = std::sync::mpsc::sync_channel(4);
        drop(tx);
        let mut stream = crate::PlaybackStream::new(rx, Arc::new(AtomicU64::new(0))).with_pace(1.6);
        let start = Instant::now();
        let Gathered::Play(held) = gather(&mut stream, chunk(1.0, Some("Twenty characters ok")), &|| false, &Cancellation::new(|| false), &mut audibility) else { panic!() };
        assert_eq!(held.len(), 1);
        assert!(start.elapsed() < Duration::from_millis(50));
    }
    /// Night 1022 review (2): the pre-buffer runs after the app's hold let the line start; the stream's start check
    /// is asked again right before the first sample. Skip fails the job unplayed, Drop ends it unplayed, Wait keeps
    /// taking audio until it says Start.
    #[test]
    fn gather_asks_the_start_check_right_before_the_first_sample() {
        use std::sync::atomic::AtomicUsize;
        use std::time::Duration;
        let chunk = |label: Option<&str>| crate::PlaybackChunk {
            audio: Audio { samples: vec![0.3; 4_410], sample_rate: 44_100, first_audio_ms: 0., synthesis_ms: 0. },
            generation: 0,
            label: label.map(Into::into),
        };
        let done = || { let (tx, rx) = std::sync::mpsc::sync_channel(4); drop(tx); rx };
        let mut audibility = Audibility::new(None);
        let no = || false;
        let never = Cancellation::new(|| false);
        // Skip: never plays, the error is the check's.
        let mut stream = crate::PlaybackStream::new(done(), Arc::new(AtomicU64::new(0)))
            .with_start_check(|| crate::MayStart::Skip(anyhow::anyhow!("he is talking")));
        let Gathered::Skipped(e) = gather(&mut stream, chunk(Some("Hi.")), &no, &never, &mut audibility) else { panic!("not skipped") };
        assert_eq!(e.to_string(), "he is talking");
        // Drop: ends unplayed.
        let mut stream = crate::PlaybackStream::new(done(), Arc::new(AtomicU64::new(0))).with_start_check(|| crate::MayStart::Drop);
        assert!(matches!(gather(&mut stream, chunk(Some("Hi.")), &no, &never, &mut audibility), Gathered::Cancelled));
        // Wait twice, then start: asked only once the pre-buffer is full, and audio keeps arriving meanwhile.
        let asked = Arc::new(AtomicUsize::new(0));
        let (tx, rx) = std::sync::mpsc::sync_channel(8);
        tx.send(chunk(None)).unwrap();
        let a = asked.clone();
        let mut stream = crate::PlaybackStream::new(rx, Arc::new(AtomicU64::new(0))).with_start_check(move || {
            if a.fetch_add(1, Ordering::SeqCst) < 2 { crate::MayStart::Wait } else { crate::MayStart::Start }
        });
        let feeder = std::thread::spawn(move || { std::thread::sleep(Duration::from_millis(20)); let _ = tx.send(chunk(Some("Next."))); });
        let Gathered::Play(held) = gather(&mut stream, chunk(Some("Hi.")), &no, &never, &mut audibility) else { panic!("not played") };
        feeder.join().unwrap();
        assert_eq!(asked.load(Ordering::SeqCst), 3);
        assert!(held.len() >= 2, "kept taking audio while waiting: {}", held.len());
        assert!(matches!(stream.may_start(), crate::MayStart::Start), "the check is done with once playing");
    }
    /// A cursor fed by hand: (cursor, sender).
    fn fed(first: Vec<f32>, label: Option<&str>) -> (PlaybackCursor, std::sync::mpsc::Sender<crate::PlaybackChunk>) {
        let (tx, rx) = std::sync::mpsc::channel();
        let chunk = crate::PlaybackChunk {
            audio: Audio { samples: first, sample_rate: SIM_RATE, first_audio_ms: 0., synthesis_ms: 0. },
            generation: 0,
            label: label.map(Into::into),
        };
        (PlaybackCursor::new(crate::PlaybackStream::new(rx, Arc::new(AtomicU64::new(0))), chunk, 1.), tx)
    }
    fn audio(frames: usize, label: Option<&str>) -> crate::PlaybackChunk {
        crate::PlaybackChunk { audio: Audio { samples: vec![0.5; frames], sample_rate: SIM_RATE, first_audio_ms: 0., synthesis_ms: 0. }, generation: 0, label: label.map(Into::into) }
    }
    #[test]
    fn a_mid_sentence_underrun_fades_out_rebuffers_fades_in_and_is_recorded() {
        let rate = SIM_RATE as f64;
        let (mut cursor, tx) = fed(vec![0.5; (0.3 * rate) as usize], Some("Not that hard."));
        let mut out: Vec<f32> = (0..(0.3 * rate) as usize).map(|_| cursor.next()).collect();
        // The last 5 ms fade to silence: nothing follows yet.
        let fade = (FADE_SECONDS * rate) as usize;
        assert!(out[out.len() - fade - 1] > 0.49 && out[out.len() - 1] < 0.01, "faded out");
        assert!(out.windows(2).all(|w| (w[1] - w[0]).abs() < 0.01), "no step at the edge");
        // Run dry: silence, flagged for the idle release.
        out.extend((0..441).map(|_| cursor.next()));
        assert!(cursor.starving.load(Ordering::Acquire));
        // 0.2 s arrives: below the re-buffer, still silent; another 0.2 s: it goes on, fading in.
        tx.send(audio((0.2 * rate) as usize, None)).unwrap();
        out.extend((0..441).map(|_| cursor.next()));
        assert!(out[out.len() - 441..].iter().all(|v| *v == 0.), "waits for the re-buffer");
        tx.send(audio((0.2 * rate) as usize, None)).unwrap();
        let resumed: Vec<f32> = (0..441).map(|_| cursor.next()).collect();
        assert!(resumed[0] < 0.01 && resumed[fade + 1] > 0.49, "faded in");
        assert!(resumed.windows(2).all(|w| (w[1] - w[0]).abs() < 0.01), "no step at the edge");
        assert!(!cursor.starving.load(Ordering::Acquire));
        let (incidents, _) = cursor.incidents.take();
        assert_eq!(incidents.len(), 1);
        assert!((incidents[0].at - 0.3).abs() < 0.001, "{:?}", incidents[0]);
        assert!((incidents[0].silent - 0.02).abs() < 0.001, "{:?}", incidents[0]);
        assert_eq!(incidents[0].sentence.as_deref(), Some("Not that hard."));
    }
    #[test]
    fn a_gap_between_sentences_and_the_end_of_a_stream_are_not_underruns() {
        let (mut cursor, tx) = fed(vec![0.5; 4410], Some("First."));
        for _ in 0..4410 + 2000 {
            cursor.next();
        }
        // The next sentence arrives whole (verified): it plays at once, and the pause was not an underrun.
        tx.send(audio(4410, Some("Second."))).unwrap();
        assert!(cursor.next() >= 0.);
        for _ in 0..4410 + 100 {
            cursor.next();
        }
        drop(tx);
        for _ in 0..10 {
            cursor.next();
        }
        assert!(cursor.done.load(Ordering::Acquire));
        assert!(cursor.incidents.take().0.is_empty());
    }
    #[test]
    fn underrun_lines_are_rate_limited_and_count_what_they_held_back() {
        use std::time::{Duration, Instant};
        let mut limiter = UnderrunLimiter::new();
        let now = Instant::now();
        let u = Underrun { at: 0.102, silent: 0.15, sentence: Some("Not that hard, and it suits a small model well.".into()) };
        let line = limiter.line(&u, 1.6, now).unwrap();
        assert_eq!(line, "voice: playback underrun: 150 ms of silence 0.10 s into the reply in \"Not that hard, and it suits a small model well.\"; generation fell behind playback at 160% speed");
        assert!(limiter.line(&u, 1.6, now + Duration::from_secs(3)).is_none());
        assert!(limiter.line(&u, 1.6, now + Duration::from_secs(9)).is_none());
        let next = limiter.line(&u, 1.6, now + UNDERRUN_LOG_PERIOD).unwrap();
        assert!(next.ends_with("(and 2 more since the last line)"), "{next}");
    }
    #[test]
    fn speaking_flag_ends_when_the_last_sample_passes_the_speaker_not_when_the_stream_ends() {
        const RATE: u32 = 48_000;
        const LATENCY: u64 = 5_760; // 120 ms of output latency
        const BUFFER: usize = 512;
        let samples = ramp(RATE as usize); // one second of speech
        let end = samples.len() as u64;
        let mut device = Device::new(RATE, samples, LATENCY);
        let (mut on_at, mut off_at, mut steps) = (None, None, 0);
        while off_at.is_none() {
            device.buffer(BUFFER);
            steps += 1;
            if on_at.is_none() && device.timeline.started() {
                on_at = Some((device.timeline.played(), device.handed.len() as u64));
            }
            if device.timeline.finished() {
                off_at = Some(device.timeline.played());
            }
            assert!(steps < 10_000, "the device never finished");
        }
        let ((on_played, on_rendered), off_at) = (on_at.unwrap(), off_at.unwrap());
        // On when the first sample is audible: the callback had rendered a whole latency before it (the first sample
        // is now at the speaker), and the flag came up within one buffer of that.
        assert!(on_rendered > LATENCY && on_played < BUFFER as u64, "on at {on_played} (rendered {on_rendered})");
        // Off once the last real sample has passed the speaker, plus the tail, checked once per buffer.
        let tail = (TAIL_SECONDS * RATE as f64).round() as u64;
        assert!(off_at >= end + tail && off_at < end + tail + BUFFER as u64, "off at {off_at}, end {end}");
        // The callback knew the stream had ended a whole latency earlier; the flag did not follow that.
        assert_eq!(device.timeline.end_frame(), Some(end));
        assert!(off_at - end <= (0.150 * RATE as f64) as u64, "the flag stays on at most 150 ms after the last sample");
    }
    #[test]
    fn tail_drains_the_final_150_ms_to_the_device_before_it_stops() {
        const RATE: u32 = 44_100;
        const LATENCY: u64 = 5_292; // 120 ms
        const BUFFER: usize = 512;
        let samples = ramp(RATE as usize / 2);
        let end = samples.len();
        let mut device = Device::new(RATE, samples.clone(), LATENCY);
        while !device.timeline.finished() {
            device.buffer(BUFFER);
            assert!(device.handed.len() < 10 * RATE as usize, "the device never finished");
        }
        // Every real frame reached the device, in order, with the final 150 ms among them.
        let tail_frames = (0.150 * RATE as f64) as usize;
        assert_eq!(&device.handed[end - tail_frames..end], &samples[end - tail_frames..], "final 150 ms reached the device");
        assert!(device.handed[end - 1].abs() > 0.01, "the last sample is not silence");
        // The device stops only after the last sample has passed the speaker: the frames it still holds are the
        // latency plus the tail, so stopping at the old two-buffer drain would have cut the last 100 ms.
        assert!(device.timeline.played() >= end as u64, "stopped before the last sample was audible");
        let two_buffer_drain = 2 * BUFFER as u64;
        assert!(LATENCY > two_buffer_drain, "the regression case: the latency outlasts the old drain");
        assert!(device.handed.len() as u64 >= end as u64 + LATENCY, "every frame was rendered before the device stopped");
    }
    /// A flag that records each transition with the instant it was emitted.
    fn recorder() -> (Arc<Speaking>, Arc<Mutex<Vec<bool>>>) {
        let log = Arc::new(Mutex::new(Vec::new()));
        let sink = log.clone();
        (Speaking::new(Box::new(move |on| sink.lock().unwrap().push(on))), log)
    }
    #[test]
    fn two_overlapping_streams_keep_one_speaking_flag() {
        let (owner, log) = recorder();
        let agent = SpeakingClaim::new(owner.clone());
        let sample = SpeakingClaim::new(owner.clone()); // the Speed sample starts over the agent's speech
        drop(agent); // the agent's stream ends first: the sample is still audible
        assert_eq!(*log.lock().unwrap(), [true], "no false while the sample plays");
        drop(sample);
        assert_eq!(*log.lock().unwrap(), [true, false]);
        // A later stream starts a fresh on/off pair; the count never goes negative.
        drop(SpeakingClaim::new(owner.clone()));
        assert_eq!(*log.lock().unwrap(), [true, false, true, false]);
    }
    /// Three sentences queued back to back, each 300 ms of synthesis after the previous one ends, on a virtual clock
    /// (offsets from one base instant). The claim is held through every gap and ends 150 ms after the last sample.
    #[test]
    fn a_hold_spans_queued_sentences_and_ends_150_ms_after_the_last_sample() {
        use std::time::{Duration, Instant};
        let (owner, log) = recorder();
        let hold = SpeakingHold::new(owner, Duration::from_millis(150));
        let base = Instant::now();
        let at = |ms: u64| base + Duration::from_millis(ms);
        // Sentence 1 plays 0..500 ms; sentences 2 and 3 are synthesized in the gaps (500..800, 1300..1600).
        hold.start();
        hold.last_sample(at(500));
        assert_eq!(hold.deadline(), Some(at(650)));
        hold.start(); // sentence 2 is audible at 800 ms
        hold.last_sample(at(1300));
        hold.start(); // sentence 3 is audible at 1600 ms
        hold.last_sample(at(2100));
        assert_eq!(*log.lock().unwrap(), [true], "one continuous span through the gaps");
        // The worker waits for the next job only until the linger runs out.
        assert!(!hold.release_if_due(at(2249)));
        assert!(hold.release_if_due(at(2250)));
        assert_eq!(*log.lock().unwrap(), [true, false]);
        assert_eq!(hold.deadline(), None);
    }
    /// A reply whose next sentence is still synthesizing: after 1.5 s of silence the claim falls (the island goes back
    /// to its thinking look), and the next sentence's first sample claims it again. Virtual clock, offsets in ms.
    #[test]
    fn a_silent_gap_past_the_cap_releases_the_claim_and_the_next_sample_reclaims_it() {
        use std::time::{Duration, Instant};
        let (owner, log) = recorder();
        let hold = SpeakingHold::new(owner, Duration::from_millis(150));
        let base = Instant::now();
        let at = |ms: u64| base + Duration::from_millis(ms);
        // Sentence 1 plays 0..500 ms. Sentence 2 synthesizes for 3 s: nothing plays from 500 ms.
        hold.start();
        hold.last_sample(at(500));
        assert!(!hold.release_if_silent(at(1999)), "1.499 s of silence: still held");
        assert!(hold.release_if_silent(at(2000)), "1.5 s of silence: released");
        assert_eq!(*log.lock().unwrap(), [true, false]);
        assert_eq!(hold.deadline(), None);
        assert!(!hold.release_if_silent(at(5000)), "nothing left to release");
        // Sentence 2's first sample plays at 3500 ms: the claim is taken again, once.
        hold.start();
        assert_eq!(*log.lock().unwrap(), [true, false, true]);
        hold.start();
        assert_eq!(*log.lock().unwrap(), [true, false, true], "a second sample does not re-claim");
        // Playing audio is never silent: the cap cannot release it, however long it lasts.
        assert!(!hold.release_if_silent(at(9000)));
        // Sentence 2 ends at 4000 ms; sentence 3 synthesizes; the cap counts from that end.
        hold.last_sample(at(4000));
        assert!(!hold.release_if_silent(at(5499)));
        assert!(hold.release_if_silent(at(5500)));
        assert_eq!(*log.lock().unwrap(), [true, false, true, false]);
    }
    #[test]
    fn release_ends_a_held_span_at_once_and_only_once() {
        use std::time::{Duration, Instant};
        let (owner, log) = recorder();
        let hold = SpeakingHold::new(owner, Duration::from_secs(10));
        let base = Instant::now();
        hold.start();
        hold.last_sample(base);
        hold.release(); // an interrupt, long before the linger would end it
        assert_eq!(*log.lock().unwrap(), [true, false]);
        assert!(!hold.release_if_due(base + Duration::from_secs(60)), "nothing left to release");
        hold.release();
        assert_eq!(*log.lock().unwrap(), [true, false]);
    }
    // Route recovery (2 Nov 14:46: the AirPods stopped calling back and the reply was lost). A scripted output stands
    // in for CoreAudio: each open follows a plan, on its own thread, through the same `Render::buffer` the CoreAudio
    // callback runs. Virtual devices; no hardware, no sound.
    #[derive(Clone, Default)]
    struct Plan {
        /// The device will not open.
        refuse: bool,
        /// Frames it renders before it stops calling back (`None`: it never stops).
        frames: Option<usize>,
        /// After this many frames, the default output becomes this device (the old one keeps playing).
        then_default: Option<(usize, u32)>,
        /// The user interrupts as this device opens.
        interrupt: bool,
    }
    struct Fake {
        default: Arc<std::sync::atomic::AtomicU32>,
        plans: std::collections::VecDeque<Plan>,
        interrupt: Arc<AtomicBool>,
        /// Every stream opened: its device and every frame its callback handed over.
        opened: Vec<(u32, Arc<Mutex<Vec<f32>>>)>,
        refused: usize,
        /// Pauses across every stream (an idle release, and each stream's stop).
        pauses: Arc<std::sync::atomic::AtomicUsize>,
    }
    const FAKE_BUFFER: usize = 441;
    const FAKE_LATENCY: u64 = 882;
    impl Fake {
        fn new(plans: Vec<Plan>) -> Self {
            Self {
                default: Arc::new(std::sync::atomic::AtomicU32::new(1)),
                plans: plans.into(),
                interrupt: Default::default(),
                opened: vec![],
                refused: 0,
                pauses: Default::default(),
            }
        }
        /// The real (non-silent) samples the `n`th opened stream handed over.
        fn real(&self, n: usize) -> Vec<f32> {
            self.opened[n].1.lock().unwrap().iter().copied().filter(|v| *v != 0.).collect()
        }
    }
    struct FakeStream {
        stop: Arc<AtomicBool>,
        paused: Arc<AtomicBool>,
        pauses: Arc<std::sync::atomic::AtomicUsize>,
        thread: Option<std::thread::JoinHandle<()>>,
    }
    impl OutputStream for FakeStream {
        fn play(&self) -> Result<()> {
            self.paused.store(false, Ordering::Release);
            Ok(())
        }
        fn pause(&self) -> Result<()> {
            self.pauses.fetch_add(1, Ordering::AcqRel);
            self.paused.store(true, Ordering::Release);
            Ok(())
        }
    }
    impl Drop for FakeStream {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            if let Some(thread) = self.thread.take() {
                thread.join().unwrap();
            }
        }
    }
    impl Outputs for Fake {
        type Stream = FakeStream;
        fn current(&mut self) -> Option<u32> {
            Some(self.default.load(Ordering::Acquire))
        }
        fn open(&mut self, bind: &mut dyn FnMut(u32) -> Render) -> Result<Opened<FakeStream>> {
            let plan = self.plans.pop_front().expect("an open beyond the script");
            let device = self.default.load(Ordering::Acquire);
            if plan.interrupt {
                self.interrupt.store(true, Ordering::Release);
            }
            if plan.refuse {
                self.refused += 1;
                anyhow::bail!("device {device} would not open");
            }
            let render = bind(SIM_RATE);
            render.timeline.set_latency_frames(FAKE_LATENCY);
            let handed = Arc::new(Mutex::new(Vec::new()));
            self.opened.push((device, handed.clone()));
            let (stop, paused) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
            let (s, p, default) = (stop.clone(), paused.clone(), self.default.clone());
            // Like cpal 0.17, the device calls back as soon as it is built.
            let thread = std::thread::spawn(move || {
                let mut left = plan.frames.unwrap_or(usize::MAX);
                let mut rendered = 0;
                while !s.load(Ordering::Acquire) {
                    if p.load(Ordering::Acquire) || left == 0 {
                        std::thread::sleep(std::time::Duration::from_millis(1));
                        continue;
                    }
                    let mut data = vec![0f32; FAKE_BUFFER.min(left)];
                    render.buffer(&mut data, 1, |v| v);
                    left -= data.len();
                    rendered += data.len();
                    handed.lock().unwrap().extend_from_slice(&data);
                    if let Some((at, to)) = plan.then_default {
                        if rendered >= at {
                            default.store(to, Ordering::Release);
                        }
                    }
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
            });
            Ok(Opened {
                stream: FakeStream { stop, paused, pauses: self.pauses.clone(), thread: Some(thread) },
                device: Some(device),
                route: format!("Device {device} (test, 44100 Hz, 1 ch)"),
            })
        }
    }
    const TEST_LIMITS: Limits = Limits {
        stall: std::time::Duration::from_millis(150),
        first_callback: std::time::Duration::from_millis(400),
        route_poll: std::time::Duration::from_millis(5),
        route_settle: std::time::Duration::from_millis(20),
        retry: std::time::Duration::from_millis(10),
        idle_release: std::time::Duration::from_millis(100),
    };
    /// Frames per test sentence (0.5 s).
    const SENTENCE: usize = 22_050;
    /// A three-sentence reply, all generated (the producer has finished), in 0.1 s chunks, the first of each sentence
    /// labelled "Sentence k.". Every sample is distinct and non-zero (sentence k, frame i: 0.1·k + i·1e-6). Returns
    /// the stream with its speaking hold, the hold's flag log, and each sentence's samples.
    fn reply() -> (crate::PlaybackStream, Arc<SpeakingHold>, Arc<Mutex<Vec<bool>>>, Vec<Vec<f32>>) {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut said = vec![];
        for k in 1..=3 {
            let samples: Vec<f32> = (0..SENTENCE).map(|i| 0.1 * k as f32 + i as f32 * 1e-6).collect();
            for (j, part) in samples.chunks(4_410).enumerate() {
                let audio = Audio { samples: part.to_vec(), sample_rate: SIM_RATE, first_audio_ms: 0., synthesis_ms: 0. };
                let label = (j == 0).then(|| format!("Sentence {k}.").into());
                tx.send(crate::PlaybackChunk { audio, generation: 0, label }).unwrap();
            }
            said.push(samples);
        }
        drop(tx);
        let (owner, flag) = recorder();
        let hold = SpeakingHold::new(owner, std::time::Duration::from_millis(150));
        let stream = crate::PlaybackStream::new(rx, Arc::new(AtomicU64::new(0))).with_speaking(hold.clone());
        (stream, hold, flag, said)
    }
    /// Plays the stream on `fake` with test hooks; returns the outcome and every spoken-text call.
    fn run(fake: &mut Fake, stream: crate::PlaybackStream) -> (Result<()>, Vec<Option<String>>) {
        let spoken = Arc::new(Mutex::new(vec![]));
        let sink = spoken.clone();
        let hooks = Hooks {
            level: None,
            spoken: Some(Arc::new(move |text: Option<&str>| sink.lock().unwrap().push(text.map(str::to_string)))),
        };
        let interrupt = fake.interrupt.clone();
        let signal = Cancellation::new(move || interrupt.load(Ordering::Acquire));
        let result = play_reply(fake, stream, &|| false, signal, &hooks, &TEST_LIMITS);
        let spoken = spoken.lock().unwrap().clone();
        (result, spoken)
    }
    fn said(names: &[Option<&str>]) -> Vec<Option<String>> {
        names.iter().map(|n| n.map(str::to_string)).collect()
    }
    /// The flag went on once and is still held by the queue; its linger then ends it once.
    fn held_once_then_released(hold: &SpeakingHold, flag: &Mutex<Vec<bool>>) {
        assert_eq!(*flag.lock().unwrap(), [true], "one claim across every device");
        assert!(hold.release_if_due(std::time::Instant::now() + std::time::Duration::from_secs(5)));
        assert_eq!(*flag.lock().unwrap(), [true, false]);
    }
    #[test]
    fn a_device_that_stops_calling_back_mid_reply_is_reopened_and_says_the_sentence_again() {
        let (stream, hold, flag, s) = reply();
        // The device renders sentence 1 and 15000 frames of sentence 2, then stops calling back (the AirPods).
        let mut fake = Fake::new(vec![Plan { frames: Some(SENTENCE + 15_000), ..Plan::default() }, Plan::default()]);
        let (result, spoken) = run(&mut fake, stream);
        result.unwrap();
        assert_eq!(fake.opened.iter().map(|(d, _)| *d).collect::<Vec<_>>(), [1, 1], "reopened on the current default");
        assert_eq!(fake.real(0), [&s[0][..], &s[1][..15_000]].concat());
        // The new stream starts again at sentence 2 and plays the rest of the reply, every sample once, in order.
        assert_eq!(fake.real(1), [&s[1][..], &s[2][..]].concat());
        // Sentence 2 is announced once more, nothing is skipped, and the stream ends once.
        assert_eq!(spoken, said(&[Some("Sentence 1."), Some("Sentence 2."), Some("Sentence 2."), Some("Sentence 3."), None]));
        held_once_then_released(&hold, &flag);
    }
    #[test]
    fn a_new_default_output_mid_reply_moves_the_reply_from_the_sentence_at_the_speaker() {
        let (stream, hold, flag, s) = reply();
        // Device 2 becomes the default 5000 frames into sentence 2; device 1 would have played on.
        let mut fake = Fake::new(vec![Plan { then_default: Some((SENTENCE + 5_000, 2)), ..Plan::default() }, Plan::default()]);
        let (result, spoken) = run(&mut fake, stream);
        result.unwrap();
        assert_eq!(fake.opened.iter().map(|(d, _)| *d).collect::<Vec<_>>(), [1, 2], "moved to the new default");
        // The sentence the speaker was in when device 1 closed: frames rendered minus the device latency.
        let heard = fake.real(0).len() as u64 - FAKE_LATENCY;
        let k = ((heard - 1) / SENTENCE as u64) as usize;
        assert!(k >= 1, "it moved after sentence 2 began");
        assert_eq!(fake.real(1), s[k..].concat(), "device 2 says sentence {} again and the rest", k + 1);
        let again = format!("Sentence {}.", k + 1);
        assert_eq!(spoken.iter().filter(|t| t.as_deref() == Some(again.as_str())).count(), 2, "{spoken:?}");
        assert_eq!(spoken.last(), Some(&None));
        assert_eq!(spoken.iter().filter(|t| t.is_none()).count(), 1);
        for k in 1..=3 {
            assert!(spoken.contains(&Some(format!("Sentence {k}."))), "sentence {k} announced: {spoken:?}");
        }
        held_once_then_released(&hold, &flag);
    }
    #[test]
    fn a_third_lost_device_fails_the_reply_as_before() {
        let (stream, hold, flag, s) = reply();
        let mut fake = Fake::new(vec![
            Plan { frames: Some(SENTENCE + 8_000), ..Plan::default() },
            Plan { frames: Some(5_000), ..Plan::default() },
            Plan { frames: Some(5_000), ..Plan::default() },
        ]);
        let (result, spoken) = run(&mut fake, stream);
        let error = result.unwrap_err().to_string();
        assert!(error.contains("stopped calling back") && error.contains("after 2 reopens"), "{error}");
        assert_eq!(fake.opened.len(), 3, "two reopens, no more");
        assert_eq!(fake.real(1), s[1][..5_000], "each reopen starts sentence 2 again");
        assert_eq!(fake.real(2), s[1][..5_000]);
        assert_eq!(spoken.last(), Some(&None));
        assert_eq!(spoken.iter().filter(|t| t.is_none()).count(), 1, "the stream ends once: {spoken:?}");
        held_once_then_released(&hold, &flag);
    }
    #[test]
    fn an_interrupt_during_a_reopen_stops_everything() {
        let (stream, _hold, flag, _) = reply();
        let mut fake = Fake::new(vec![
            Plan { frames: Some(SENTENCE + 8_000), ..Plan::default() },
            Plan { interrupt: true, ..Plan::default() },
        ]);
        let (result, spoken) = run(&mut fake, stream);
        result.unwrap();
        assert!(fake.real(1).is_empty(), "nothing plays on the reopened device");
        assert_eq!(*flag.lock().unwrap(), [true, false], "the flag falls at once");
        assert_eq!(spoken.last(), Some(&None));
        assert_eq!(spoken.iter().filter(|t| t.is_none()).count(), 1);
    }
    #[test]
    fn a_device_that_will_not_open_spends_a_reopen() {
        // Lost, refused once, then open: the reply still completes.
        let (stream, hold, flag, s) = reply();
        let mut fake = Fake::new(vec![
            Plan { frames: Some(SENTENCE + 8_000), ..Plan::default() },
            Plan { refuse: true, ..Plan::default() },
            Plan::default(),
        ]);
        let (result, _) = run(&mut fake, stream);
        result.unwrap();
        assert_eq!(fake.refused, 1);
        assert_eq!(fake.real(1), [&s[1][..], &s[2][..]].concat());
        held_once_then_released(&hold, &flag);
        // Lost, then refused twice: the budget is spent and the reply fails, naming both.
        let (stream, hold, flag, _) = reply();
        let mut fake = Fake::new(vec![
            Plan { frames: Some(SENTENCE + 8_000), ..Plan::default() },
            Plan { refuse: true, ..Plan::default() },
            Plan { refuse: true, ..Plan::default() },
        ]);
        let error = run(&mut fake, stream).0.unwrap_err().to_string();
        assert!(error.contains("stopped calling back") && error.contains("reopening failed: device 1 would not open"), "{error}");
        held_once_then_released(&hold, &flag);
    }
    /// The device was released while the reply waited for its next sentence, and the route moved meanwhile: the next
    /// sentence plays on the new default, and nothing heard is said again.
    #[test]
    fn a_route_that_moves_while_the_device_is_released_takes_the_next_sentence() {
        let sentence = |k: usize| (0..SENTENCE).map(|i| 0.1 * k as f32 + i as f32 * 1e-6).collect::<Vec<f32>>();
        let (s1, s2) = (sentence(1), sentence(2));
        let (tx, rx) = std::sync::mpsc::channel();
        let chunk = |samples: &[f32], label: Option<&str>| crate::PlaybackChunk {
            audio: Audio { samples: samples.to_vec(), sample_rate: SIM_RATE, first_audio_ms: 0., synthesis_ms: 0. },
            generation: 0,
            label: label.map(Into::into),
        };
        tx.send(chunk(&s1, Some("Sentence 1."))).unwrap();
        // Sentence 2 is the start of a later sentence, so sentence 1 may start; it arrives only later.
        let (owner, flag) = recorder();
        let hold = SpeakingHold::new(owner, std::time::Duration::from_millis(150));
        let stream = crate::PlaybackStream::new(rx, Arc::new(AtomicU64::new(0))).with_speaking(hold.clone());
        let mut fake = Fake::new(vec![Plan::default(), Plan::default()]);
        let default = fake.default.clone();
        let producer = {
            let s2 = s2.clone();
            std::thread::spawn(move || {
                // Sentence 1 plays (~0.1 s at the fake's pace), the device runs dry and is released 0.1 s later.
                std::thread::sleep(std::time::Duration::from_millis(500));
                default.store(2, Ordering::Release);
                std::thread::sleep(std::time::Duration::from_millis(60));
                tx.send(chunk(&s2, Some("Sentence 2."))).unwrap();
            })
        };
        let (result, spoken) = run(&mut fake, stream);
        producer.join().unwrap();
        result.unwrap();
        assert_eq!(fake.opened.iter().map(|(d, _)| *d).collect::<Vec<_>>(), [1, 2]);
        assert_eq!(fake.pauses.load(Ordering::Acquire), 3, "device 1 was released while idle, then each stream stopped");
        // Sentence 1 fades out at the hole and sentence 2 fades in after it (5 ms each, existing behaviour); every
        // other sample is exact.
        let fade = (FADE_SECONDS * SIM_RATE as f64) as usize + 1;
        let (one, two) = (fake.real(0), fake.real(1));
        assert!(one.len() >= SENTENCE - fade && one[..SENTENCE - fade] == s1[..SENTENCE - fade], "sentence 1 on device 1");
        assert!(two.len() <= SENTENCE && two.len() >= SENTENCE - fade, "{} samples on device 2", two.len());
        assert_eq!(two[two.len() - (SENTENCE - fade)..], s2[fade..], "sentence 2 on the new route; sentence 1, already heard, is not said again");
        assert_eq!(spoken, said(&[Some("Sentence 1."), Some("Sentence 2."), None]));
        // The quiet gap released the flag (the silence cap); sentence 2 took it again, once.
        let log = flag.lock().unwrap().clone();
        assert!(log == [true] || log == [true, false, true], "{log:?}");
        assert!(hold.release_if_due(std::time::Instant::now() + std::time::Duration::from_secs(5)));
        assert_eq!(flag.lock().unwrap().last(), Some(&false));
    }
    #[test]
    fn a_reply_moves_only_once_a_new_default_has_held() {
        use std::time::{Duration, Instant};
        let t0 = Instant::now();
        let ms = |n: u64| t0 + Duration::from_millis(n);
        let mut watch = RouteWatch::new(Some(1), &Limits::DEVICE);
        assert!(!watch.moved(ms(0), || Some(1)));
        // A headset flips the default and back within the settle time: no move.
        assert!(!watch.moved(ms(100), || Some(2)));
        assert!(!watch.moved(ms(200), || Some(1)));
        // No default at all (mid-switch) is not a move either.
        assert!(!watch.moved(ms(300), || None));
        // A new default that holds for the settle time is.
        assert!(!watch.moved(ms(400), || Some(3)));
        assert!(!watch.moved(ms(500), || Some(3)));
        assert!(watch.moved(ms(650), || Some(3)));
        // Read at most once per poll period.
        let mut reads = 0;
        let mut watch = RouteWatch::new(Some(1), &Limits::DEVICE);
        for n in 0..20 {
            watch.moved(ms(n * 5), || {
                reads += 1;
                Some(1)
            });
        }
        assert_eq!(reads, 1);
        // An unknown device is never moved off.
        assert!(!RouteWatch::new(None, &Limits::DEVICE).moved(ms(0), || Some(2)));
    }
    #[test]
    fn resume_starts_at_the_sentence_the_speaker_was_in_even_behind_a_short_one() {
        let samples = |k: usize, n: usize| (0..n).map(|i| 0.1 * k as f32 + i as f32 * 1e-6).collect::<Vec<f32>>();
        let (s1, s2, s3) = (samples(1, 4_410), samples(2, 300), samples(3, 4_410));
        let (mut cursor, tx) = fed(s1.clone(), Some("One."));
        for (s, label) in [(&s2, "Yes."), (&s3, "Three.")] {
            let audio = Audio { samples: s.clone(), sample_rate: SIM_RATE, first_audio_ms: 0., synthesis_ms: 0. };
            tx.send(crate::PlaybackChunk { audio, generation: 0, label: Some(label.into()) }).unwrap();
        }
        drop(tx);
        // The cursor is 100 frames into "Three." but, 882 frames of latency behind it, the speaker is still in "One.".
        let timeline = Timeline::new(SIM_RATE);
        timeline.set_latency_frames(FAKE_LATENCY);
        let never = Cancellation::new(|| false);
        let mut data = vec![0f32; 4_410 + 300 + 100];
        fill(&mut data, 1, &mut cursor, &timeline, &never, |v| v);
        let (mut next, from) = cursor.resume(timeline.played());
        assert_eq!(from, Some(1));
        let timeline = Timeline::new(SIM_RATE);
        let mut again = vec![0f32; 9_500];
        fill(&mut again, 1, &mut next, &timeline, &never, |v| v);
        let real: Vec<f32> = again.into_iter().filter(|v| *v != 0.).collect();
        assert_eq!(real, [&s1[..], &s2[..], &s3[..]].concat(), "every sample from the start of \"One.\", once");
        assert!(next.done.load(Ordering::Acquire));
        assert!(next.incidents.take().0.is_empty(), "a replay is not an underrun");
        // Everything rendered has passed the speaker: nothing is said again.
        let played = timeline.played() + FAKE_LATENCY;
        let (next, from) = next.resume(played);
        assert_eq!(from, None);
        // The next device is lost too before it plays a sample: still nothing heard is said again.
        let (_, from) = next.resume(0);
        assert_eq!(from, None, "a loss after a lossless recovery replays nothing heard");
    }
    #[test]
    fn a_recovery_is_one_line_naming_both_routes_and_the_sentence() {
        let lost = Lost { loss: Loss::Moved, route: "AirPods Pro (Bluetooth, 48000 Hz, 2 ch)".into(), resumed: Some(3) };
        assert_eq!(
            lost.line("MacBook Pro Speakers (built-in, 48000 Hz, 2 ch)", &Limits::DEVICE),
            "voice: output moved to MacBook Pro Speakers (built-in, 48000 Hz, 2 ch) mid-reply; resumed from sentence 3"
        );
        let lost = Lost { loss: Loss::Stalled, route: "AirPods Pro (Bluetooth, 48000 Hz, 2 ch)".into(), resumed: Some(2) };
        assert_eq!(
            lost.line("MacBook Pro Speakers (built-in, 48000 Hz, 2 ch)", &Limits::DEVICE),
            "voice: CoreAudio output stopped calling back for 4 s on AirPods Pro (Bluetooth, 48000 Hz, 2 ch); reopened on MacBook Pro Speakers (built-in, 48000 Hz, 2 ch), resumed from sentence 2"
        );
    }
}
