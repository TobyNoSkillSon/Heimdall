//! Heimdall's voice-bank runtime: the clip store under the archive, playing a chosen bank line through the
//! same single voice as agent speech, the background fill job, and recording every synthesized clip.
//!
//! One voice at a time. Every audible Heimdall output (agent speech chunk or bank line) holds `playback`.
//! - A bank line is skipped when agent speech is queued, synthesizing or playing, when the user is talking, when
//!   Speaking or Heimdall is off, or when anything interrupted speech since the turn's decision began.
//! - A bank line that has started plays to its end unless interrupted; agent speech that becomes ready in
//!   the meantime waits for it (lines are one or two seconds) instead of overlapping.
//! - A barge-in the decider judged, Speaking off and Listening off interrupt a line exactly like agent
//!   speech: they all go through `Tools::interrupt`/`Tools::shutdown`, which advance `epoch`. The user
//!   starting to talk does not stop a line by itself (pipeline/barge_in.rs).
use crate::bridge::Tools;
use anyhow::Result;
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc, Arc, Mutex, Weak,
    },
    time::{Duration, Instant},
};
use voicebank::{ClipMeta, Store};

/// The editable line list beside the archive: `<data>/voice-bank-lines.json`. Clips live in the archive store.
pub const LINES_FILE: &str = "voice-bank-lines.json";
/// The speech model Heimdall speaks with. Bank takes, offered lines and recorded clips are keyed by it;
/// a new model changes this one value (and `Tools::fill_generator`).
pub const GENERATOR: &str = voicebank::SUPERTONIC_ID;

pub type OutputFactory = Box<dyn Fn() -> Box<dyn speak::AudioOutput> + Send + Sync>;

pub struct Bank {
    root: PathBuf,
    store: Mutex<Option<Arc<Store>>>,
    pub lines: voicebank::LineSet,
    epoch: AtomicU64,
    playback: Arc<Mutex<()>>,
    line_playing: Arc<AtomicBool>,
    /// The line playing is an instant acknowledgement (pipeline/ack.rs).
    ack_playing: AtomicBool,
    /// Stops the playing acknowledgement (and only it).
    ack_stop: Arc<AtomicBool>,
    /// An acknowledgement is waiting out the user's quiet before it plays (`play_ack_after`).
    ack_waiting: Arc<AtomicBool>,
    /// The acknowledgements said most recently, oldest first (the rule avoids repeating them).
    pub(crate) recent_acks: Mutex<std::collections::VecDeque<String>>,
    // The end-of-turn reply line left the decider (29 Oct, Decider V5); bank lines stay playable and tested.
    #[cfg_attr(not(test), allow(dead_code))]
    rotation: Mutex<HashMap<(String, String, String), usize>>,
    recorder: Mutex<Option<mpsc::SyncSender<(ClipMeta, speak::Audio)>>>,
    fill_started: AtomicBool,
    output: Mutex<OutputFactory>,
    /// Lines handed to the output (tests count them).
    pub lines_played: AtomicU64,
}
impl Bank {
    pub fn new(archive_root: &Path, _decide_models: &Path) -> Self {
        let data = archive_root.parent().map(Path::to_path_buf).unwrap_or_default();
        Self {
            lines: voicebank::LineSet::new(data.join(LINES_FILE)),
            root: archive_root.into(),
            store: Mutex::new(None),
            epoch: AtomicU64::new(0),
            playback: Arc::new(Mutex::new(())),
            line_playing: Arc::new(AtomicBool::new(false)),
            ack_playing: AtomicBool::new(false),
            ack_stop: Arc::new(AtomicBool::new(false)),
            ack_waiting: Arc::new(AtomicBool::new(false)),
            recent_acks: Mutex::new(Default::default()),
            rotation: Mutex::new(HashMap::new()),
            recorder: Mutex::new(None),
            fill_started: AtomicBool::new(false),
            // Tests never touch a speaker device.
            output: Mutex::new(if cfg!(test) { Box::new(|| Box::new(Silent) as Box<dyn speak::AudioOutput>) } else {
                Box::new(|| Box::new(speak::CoreAudioOutput) as Box<dyn speak::AudioOutput>)
            }),
            lines_played: AtomicU64::new(0),
        }
    }
    /// The archive store holding the bank.
    // The end-of-turn reply line left the decider (29 Oct, Decider V5); bank lines stay playable and tested.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn root(&self) -> &Path {
        &self.root
    }
    /// Opened on first use: a primary-mic turn with Speaking on, a spoken clip, or the fill job.
    pub fn store(&self) -> Result<Arc<Store>> {
        let mut s = self.store.lock().unwrap();
        if s.is_none() {
            *s = Some(Arc::new(Store::open(&self.root)?));
        }
        Ok(s.as_ref().unwrap().clone())
    }
    /// Whether anything has opened the bank yet.
    #[cfg(test)]
    pub(crate) fn is_open(&self) -> bool {
        self.store.lock().unwrap().is_some()
    }
    pub fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::Acquire)
    }
    /// Barge-in, Speaking off, Listening off: stops a playing line and voids pending ones.
    pub fn interrupt(&self) {
        self.epoch.fetch_add(1, Ordering::AcqRel);
    }
    pub fn line_playing(&self) -> bool {
        self.line_playing.load(Ordering::Acquire)
    }
    /// An instant acknowledgement is playing.
    pub fn ack_playing(&self) -> bool {
        self.ack_playing.load(Ordering::Acquire)
    }
    /// An instant acknowledgement is waiting to play (`play_ack_after`).
    pub fn ack_waiting(&self) -> bool {
        self.ack_waiting.load(Ordering::Acquire)
    }
    /// Stops the acknowledgement playing now, if any; a reply line or agent speech is not affected.
    pub fn stop_ack(&self) -> bool {
        let playing = self.ack_playing();
        if playing {
            self.ack_stop.store(true, Ordering::Release);
        }
        playing
    }
    /// The output every audible Heimdall voice must hold while it plays.
    pub fn exclusive<O: speak::AudioOutput>(&self, inner: O) -> Exclusive<O> {
        Exclusive { inner, lock: self.playback.clone() }
    }
    #[cfg(test)]
    pub fn set_output(&self, factory: OutputFactory) {
        *self.output.lock().unwrap() = factory;
    }
    /// Keeps a synthesized clip (agent speech) as training data, off the speech thread.
    pub fn record(&self, meta: ClipMeta, audio: speak::Audio) {
        let mut recorder = self.recorder.lock().unwrap();
        if recorder.is_none() {
            let store = match self.store() {
                Ok(s) => s,
                Err(e) => return eprintln!("Voice bank unavailable; clip not kept: {e:#}"),
            };
            let (tx, rx) = mpsc::sync_channel::<(ClipMeta, speak::Audio)>(64);
            let spawned = std::thread::Builder::new().name("heimdall-voice-bank-writer".into()).spawn(move || {
                while let Ok((meta, audio)) = rx.recv() {
                    if let Err(e) = store.insert(&meta, &audio) {
                        eprintln!("Voice bank write failed: {e:#}");
                    }
                }
            });
            if let Err(e) = spawned {
                return eprintln!("Voice bank writer unavailable: {e}");
            }
            *recorder = Some(tx);
        }
        if recorder.as_ref().unwrap().try_send((meta, audio)).is_err() {
            eprintln!("Voice bank writer backlog full; clip not kept");
        }
    }
    /// Next take of a line, rotating so repeats do not sound identical.
    // The end-of-turn reply line left the decider (29 Oct, Decider V5); bank lines stay playable and tested.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn pick_take(&self, voice: &str, generator: &str, line: &str, takes: &[i64]) -> Option<i64> {
        if takes.is_empty() {
            return None;
        }
        let mut r = self.rotation.lock().unwrap();
        let n = r.entry((voice.into(), generator.into(), line.into())).or_insert(0);
        let take = takes[*n % takes.len()];
        *n += 1;
        Some(take)
    }
}

/// Holds the playback lock for the duration of one output. Agent speech uses it so it never overlaps a line.
pub struct Exclusive<O> {
    inner: O,
    lock: Arc<Mutex<()>>,
}
impl<O: speak::AudioOutput> speak::AudioOutput for Exclusive<O> {
    fn output(&mut self, audio: &speak::Audio, cancel: &dyn Fn() -> bool) -> Result<()> {
        let _voice = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        self.inner.output(audio, cancel)
    }
    fn output_stream(&mut self,chunks:speak::PlaybackStream,cancel:&dyn Fn()->bool,signal:speak::Cancellation)->Result<()> {
        // Take the playback lock for the entire utterance, not each codec chunk.
        let _guard=self.lock.lock().unwrap();
        self.inner.output_stream(chunks,cancel,signal)
    }

}
struct Silent;
impl speak::AudioOutput for Silent {
    fn output(&mut self, _: &speak::Audio, _: &dyn Fn() -> bool) -> Result<()> {
        Ok(())
    }
}

/// Why a chosen line is not played right now, checked at decision time and again on the player thread.
pub fn blocked(tools: &Tools, epoch: u64) -> Option<&'static str> {
    if tools.voice_bank.epoch() != epoch {
        Some("interrupted")
    } else if !tools.enabled.load(Ordering::Acquire) {
        Some("speaking_off")
    } else if tools.is_speaking() {
        Some("owner_speaking")
    } else if tools.agent_speech_busy() {
        Some("agent_speech")
    } else if tools.voice_bank.line_playing() {
        Some("line_playing")
    } else if !tools.speech_voices.bank_matches() {
        // Bank lines are Jarvis's recordings; another chosen voice speaks for itself (night 1022 menu).
        Some("other_voice")
    } else {
        None
    }
}

/// Plays a stored clip on its own thread; the pipeline never waits. `Err` is an immediate skip reason. The
/// outcome (finished, interrupted, skipped, failed) is journalled as `voice_reply_playback`.
// The end-of-turn reply line left the decider (29 Oct, Decider V5); bank lines stay playable and tested.
#[cfg_attr(not(test), allow(dead_code))]
pub fn play(tools: &Arc<Tools>, clip: i64, epoch: u64, utterance: &str) -> std::result::Result<(), &'static str> {
    play_line(tools, clip, epoch, utterance, None)
}
/// How an instant acknowledgement plays (pipeline/ack.rs).
struct AckPlay {
    /// `Bank::stop_ack`.
    stop: Arc<AtomicBool>,
    /// Through speakers its echo reaches the microphone: its span is Heimdall's own sound. Through headphones it is not,
    /// so the user's words over it stay his.
    speakers: bool,
    /// true once its first sample is handed to the speaker; false when it was skipped or failed before.
    started: mpsc::Sender<bool>,
}
/// How long the wake waits for an acknowledgement to start (its decode and stretch: tens of ms) before going without it.
pub const ACK_START_WAIT: Duration = Duration::from_millis(300);
/// An instant acknowledgement (pipeline/ack.rs): a bank clip like any line, which also stops when `Bank::stop_ack`
/// is called (the user talking over it where his microphone cannot hear Heimdall), journalled as `voice_ack_playback`.
/// While it sounds (and `ACK_TAIL` after), a microphone utterance lying wholly inside it is Heimdall's own voice.
/// Ok(true) once it is playing; Ok(false) when it was skipped or failed before its first sample, or did not start
/// within `ACK_START_WAIT` (then the agent is not told about it).
pub fn play_ack(tools: &Arc<Tools>, clip: i64, utterance: &str, speakers: bool) -> std::result::Result<bool, &'static str> {
    let (started, receipt) = mpsc::channel();
    let ack = AckPlay { stop: tools.voice_bank.ack_stop.clone(), speakers, started };
    play_line(tools, clip, tools.voice_bank.epoch(), utterance, Some(ack))?;
    Ok(receipt.recv_timeout(ACK_START_WAIT).unwrap_or(false))
}
/// An instant acknowledgement that waits `delay` for the user's quiet before it plays (pipeline/ack.rs: the wake goes to
/// the agent now; the ack sounds 2.5 s after his last word). With no delay it is `play_ack`. Otherwise it waits on its
/// own thread and plays only if, at its moment, nothing blocks it (`blocked`: the user talking again, agent speech already
/// going, an interruption, Speaking off); a skipped one is journalled as `voice_ack_playback` and `done` is called
/// with the reason, so the wake's line is withdrawn while it is still untold and the wake sound can stand in. Once it
/// is playing, `done(Ok)` (decider v3's late ack tells the agent then). Ok(true) once it is playing or waiting.
pub fn play_ack_after(tools: &Arc<Tools>, clip: i64, utterance: &str, speakers: bool, delay: Duration,
    done: impl FnOnce(std::result::Result<(), &'static str>) + Send + 'static) -> std::result::Result<bool, &'static str> {
    if delay.is_zero() {
        return play_ack(tools, clip, utterance, speakers);
    }
    let epoch = tools.voice_bank.epoch();
    if let Some(reason) = blocked(tools, epoch) {
        return Err(reason);
    }
    let bank = &tools.voice_bank;
    if bank.ack_waiting.swap(true, Ordering::AcqRel) {
        return Err("ack_waiting");
    }
    let worker = tools.clone();
    let utterance = utterance.to_string();
    let waiting = bank.ack_waiting.clone();
    let spawned = std::thread::Builder::new().name("heimdall-ack-wait".into()).spawn(move || {
        let tools = worker;
        let until = Instant::now() + delay;
        let mut reason = loop {
            if let Some(r) = blocked(&tools, epoch) { break Some(r); }
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() { break None; }
            std::thread::sleep(left.min(Duration::from_millis(10)));
        };
        if reason.is_none() {
            let (started, receipt) = mpsc::channel();
            let ack = AckPlay { stop: tools.voice_bank.ack_stop.clone(), speakers, started };
            reason = play_line(&tools, clip, epoch, &utterance, Some(ack)).err();
            waiting.store(false, Ordering::Release);
            // Skipped or failed on the player thread before its first sample (review 23 Oct): withdrawn too. The
            // player journals that outcome itself.
            if reason.is_none() && !receipt.recv_timeout(ACK_START_WAIT).unwrap_or(false) {
                done(Err("not_started"));
                return;
            }
        }
        waiting.store(false, Ordering::Release);
        let Some(reason) = reason else { return done(Ok(())) };
        done(Err(reason));
        let session = tools.archive_session.lock().unwrap().clone();
        if let Some(session) = session {
            let payload = json!({"schema":1,"utterance_id":utterance,"clip_id":clip,"outcome":"skipped","reason":reason,
                "error":null,"elapsed_ms":delay.as_secs_f64()*1000.});
            if let Err(e) = heimdall_archive::Journal::open(&session)
                .and_then(|j| j.event("voice_ack_playback", "heimdall:voice-bank", std::slice::from_ref(&utterance), capture::host_now_ns(), payload)) {
                eprintln!("Voice line evidence failed: {e:#}");
            }
        }
    });
    if spawned.is_err() {
        bank.ack_waiting.store(false, Ordering::Release);
        return Err("player_unavailable");
    }
    Ok(true)
}
/// How early an acknowledgement's span opens: Parakeet may date the echo's first word up to a block before it
/// arrives. The user has been quiet 2.5 s by then, so it never reaches back into his words.
pub const ACK_LEAD: Duration = Duration::from_millis(300);
/// How long after an ack's last sample the microphone may still carry it (output and input latency, the room), for
/// words that began while it sounded. Echo guard (23 Oct): 600 ms missed the 23:40:01 echo by 70 ms and the 23:40:23
/// one ("La dice." for "Let me check.", too misheard for the text guard) by 480 ms: Parakeet stretches a last word
/// over the silence after it. So the same tail as the guard's (pipeline/echo_guard.rs `TAIL_NS`). Words that begin
/// after the last sample never fall in it (`SpeechPolicy::own_sound_until`): the user's quick reply stays his.
pub const ACK_TAIL: Duration = Duration::from_nanos(crate::pipeline::echo_guard::TAIL_NS);
fn play_line(tools: &Arc<Tools>, clip: i64, epoch: u64, utterance: &str, ack: Option<AckPlay>) -> std::result::Result<(), &'static str> {
    if let Some(reason) = blocked(tools, epoch) {
        return Err(reason);
    }
    let bank = &tools.voice_bank;
    if bank.line_playing.swap(true, Ordering::AcqRel) {
        return Err("line_playing");
    }
    // Its level and patience from the call state now (in a call the ack is quieter, `Tools::apply_call_level`).
    tools.apply_call_level();
    let is_ack = ack.is_some();
    // A new ack's stop flag is cleared only once it owns the voice, so a stop for one playing now is never lost.
    if let Some(a) = &ack { a.stop.store(false, Ordering::Release); }
    bank.ack_playing.store(is_ack, Ordering::Release);
    let worker = tools.clone();
    let utterance = utterance.to_string();
    let spawned = std::thread::Builder::new().name("heimdall-voice-line".into()).spawn(move || {
        let ack_stopped = || ack.as_ref().is_some_and(|a| a.stop.load(Ordering::Acquire));
        let tools = worker;
        let bank = &tools.voice_bank;
        let started = Instant::now();
        let mut reached_output = false;
        let outcome = (|| -> Result<(&'static str, Option<&'static str>)> {
            let mut audio = bank.store()?.audio(clip)?;
            // Lines are kept at natural speed; they play at the user's voice speed like every other sentence.
            audio.samples = speak::stretch::stretch(&audio.samples, tools.voice_rate());
            let Ok(_voice) = bank.playback.try_lock() else { return Ok(("skipped", Some("agent_speech"))) };
            if let Some(reason) = blocked_except_line(&tools, epoch) {
                return Ok(("skipped", Some(reason)));
            }
            let text=bank.store()?.describe(clip)?["text"].as_str().unwrap_or("").to_owned();
            bank.lines_played.fetch_add(1, Ordering::AcqRel);
            // Agent speech arriving now waits on the lock; only an interruption stops a started line.
            let cancel = || stopped(&tools, epoch).is_some() || ack_stopped();
            let mut output = (bank.output.lock().unwrap())();
            let callback_tools=tools.clone();
            let callback_ack=ack.as_ref().map(|a| a.stop.clone());
            let signal=speak::Cancellation::new(move||stopped(&callback_tools,epoch).is_some()
                || callback_ack.as_ref().is_some_and(|s| s.load(Ordering::Acquire)));
            if let Some(a) = &ack {
                // Its words are Heimdall's from here (the barge-in's own-voice check), and only if it plays.
                tools.note_spoken(&text);
                if a.speakers {
                    // Heimdall's own voice on the microphone, its end and tail (bridge/speech_policy.rs).
                    let start = capture::host_now_ns();
                    let length = audio.samples.len() as u64 * 1_000_000_000 / audio.sample_rate.max(1) as u64;
                    tools.speech_policy.own_sound_until(start.saturating_sub(ACK_LEAD.as_nanos() as u64),
                        start + length, start + length + ACK_TAIL.as_nanos() as u64);
                }
            }
            let (tx,rx)=std::sync::mpsc::channel();
            tx.send(speak::PlaybackChunk {audio,generation:0,label:(!text.trim().is_empty()).then(|| text.as_str().into())})?;drop(tx);
            let stream=speak::PlaybackStream::new(rx,Arc::new(AtomicU64::new(0)));
            let played=stream.playback_receipt();
            reached_output = true;
            if let Some(a) = &ack { let _ = a.started.send(true); }
            let result=output.output_stream(stream, &cancel,signal);
            if played.load(Ordering::Acquire) {
                let at=capture::host_now_ns();
                // An acknowledgement is not Heimdall's answer: it opens no follow-up window (the agent's reply does).
                if !is_ack {tools.playback_ended(at);}
                if !text.trim().is_empty() {tools.spoken_line(&text,at);}
            }
            result?;
            Ok(match stopped(&tools, epoch) {
                Some(reason) => ("interrupted", Some(reason)),
                None if ack_stopped() => ("interrupted", Some("owner_talked_over")),
                None => ("finished", None),
            })
        })();
        bank.ack_playing.store(false, Ordering::Release);
        bank.line_playing.store(false, Ordering::Release);
        // An ack that never reached the speaker was not said: the wake goes without its line (and with its sound).
        if let (Some(a), false) = (&ack, reached_output) { let _ = a.started.send(false); }
        let (outcome, reason, error) = match outcome {
            Ok((o, r)) => (o, r, None),
            Err(e) => ("failed", None, Some(format!("{e:#}"))),
        };
        let payload = json!({"schema":1,"utterance_id":utterance,"clip_id":clip,"outcome":outcome,"reason":reason,"error":error,"elapsed_ms":started.elapsed().as_secs_f64()*1000.});
        let session = tools.archive_session.lock().unwrap().clone();
        if let Some(session) = session {
            let kind = if is_ack { "voice_ack_playback" } else { "voice_reply_playback" };
            let journal = heimdall_archive::Journal::open(&session)
                .and_then(|j| j.event(kind, "heimdall:voice-bank", std::slice::from_ref(&utterance), capture::host_now_ns(), payload));
            if let Err(e) = journal {
                eprintln!("Voice line evidence failed: {e:#}");
            }
        }
    });
    if spawned.is_err() {
        tools.voice_bank.ack_playing.store(false, Ordering::Release);
        tools.voice_bank.line_playing.store(false, Ordering::Release);
        return Err("player_unavailable");
    }
    Ok(())
}
fn blocked_except_line(tools: &Tools, epoch: u64) -> Option<&'static str> {
    blocked(tools, epoch).filter(|r| *r != "line_playing")
}
/// What stops a line that is already playing. The user talking does not, by itself: the barge-in decision
/// interrupts it when he is talking to Heimdall (pipeline/barge_in.rs).
fn stopped(tools: &Tools, epoch: u64) -> Option<&'static str> {
    blocked(tools, epoch).filter(|r| matches!(*r, "interrupted" | "speaking_off"))
}

/// The app's Supertonic as a bank generator: shares the loaded model with agent speech (no second copy).
pub struct AppSupertonic {
    pub synth: crate::bridge::LazySynth,
    pub backend: String,
}
impl voicebank::Generator for AppSupertonic {
    fn id(&self) -> String {
        voicebank::SUPERTONIC_ID.into()
    }
    fn settings(&self) -> Value {
        voicebank::supertonic_settings(&self.backend)
    }
    fn synthesize(&mut self, text: &str, voice: &str, cancel: &dyn Fn() -> bool) -> Result<speak::Audio> {
        anyhow::ensure!(voice == speak::VOICE, "Supertonic speaks only {}", speak::VOICE);
        speak::Synthesizer::synthesize(&mut self.synth, text, "en", cancel)
    }
}

/// While the gate is closed the job checks every 30 s; it waits a minute after finishing or failing.
const CLOSED_WAIT: Duration = Duration::from_secs(30);
const IDLE_WAIT: Duration = Duration::from_secs(60);
const NEXT_CLIP: Duration = Duration::from_millis(250);

/// Starts the background fill once per `Tools`. It holds only a weak reference and ends with the app.
pub fn start_fill(tools: &Arc<Tools>) {
    if cfg!(test) {
        return; // Tests start it explicitly with an injected power/idle probe.
    }
    if crate::edition::PUBLIC {
        return; // Bank lines are Jarvis's and its generator is Supertonic: neither is in the public edition.
    }
    start_fill_with(tools, || heimdall_night::gate::state().ok(), CLOSED_WAIT);
}
pub(crate) fn start_fill_with(
    tools: &Arc<Tools>,
    power: impl Fn() -> Option<heimdall_night::gate::State> + Send + 'static,
    closed_wait: Duration,
) {
    if tools.voice_bank.fill_started.swap(true, Ordering::AcqRel) {
        return;
    }
    let weak: Weak<Tools> = Arc::downgrade(tools);
    let spawned = std::thread::Builder::new().name("heimdall-voice-bank-fill".into()).spawn(move || {
        if let Err(e) = heimdall_night::gate::background_thread() {
            return eprintln!("Voice bank fill unavailable: {e:#}");
        }
        let mut exhausted = std::collections::BTreeSet::new();
        loop {
            let Some(tools) = weak.upgrade() else { return };
            let state = power();
            let conditions = voicebank::Conditions {
                permit: tools.enabled.load(Ordering::Acquire),
                on_ac: state.map(|s| s.on_ac),
                user_idle_seconds: state.map_or(f64::NAN, |s| s.idle_seconds),
                speech_quiet_seconds: tools.speech_quiet_seconds(),
                heimdall_speaking: tools.agent_speech_busy() || tools.voice_bank.line_playing(),
            };
            let tick = if let Some(reason) = conditions.closed_by() {
                voicebank::Tick::Closed(reason)
            } else {
                fill_once(&tools, conditions, &mut exhausted)
            };
            let wait = match &tick {
                voicebank::Tick::Closed(_) => closed_wait,
                voicebank::Tick::Filled { .. } | voicebank::Tick::Duplicate { .. } => NEXT_CLIP,
                voicebank::Tick::Complete => IDLE_WAIT,
                voicebank::Tick::Failed(e) => {
                    eprintln!("Voice bank fill: {e}");
                    IDLE_WAIT
                }
            };
            drop(tools);
            std::thread::sleep(wait);
        }
    });
    if let Err(e) = spawned {
        eprintln!("Voice bank fill thread failed: {e}");
    }
}
fn fill_once(
    tools: &Arc<Tools>,
    conditions: voicebank::Conditions,
    exhausted: &mut std::collections::BTreeSet<(String, String, String)>,
) -> voicebank::Tick {
    // Drama 3 bank takes are produced by the offline tool with an ASR gate.
    // Do not silently start a full bank run on a daytime idle interval.
    if tools.drama3.ready() { return voicebank::Tick::Closed("drama3_offline_asr_fill"); }
    let bank = &tools.voice_bank;
    let store = match bank.store() {
        Ok(s) => s,
        Err(e) => return voicebank::Tick::Failed(format!("{e:#}")),
    };
    let (lines, error) = bank.lines.current();
    if let Some(e) = error {
        eprintln!("Voice bank lines: {e}");
    }
    let mut generator = match tools.fill_generator() {
        Ok(g) => g,
        Err(e) => return voicebank::Tick::Failed(format!("{e:#}")),
    };
    let epoch = bank.epoch();
    let cancel = || {
        bank.epoch() != epoch
            || !tools.enabled.load(Ordering::Acquire)
            || tools.is_speaking()
            || tools.agent_speech_busy()
    };
    voicebank::tick(conditions, &store, &lines, speak::VOICE, &mut generator, voicebank::TAKES, exhausted, &cancel)
}

/// Lines with at least one take for this voice and generator, in list order, with their takes.
/// A line offered to the decider with its stored takes.
// The end-of-turn reply line left the decider (29 Oct, Decider V5); bank lines stay playable and tested.
#[cfg_attr(not(test), allow(dead_code))]
pub type Offered = (voicebank::Line, Vec<i64>);
/// Prefer accepted Jarvis takes; an empty deferred bank keeps immediate replies
/// usable through the Supertonic stand-in.
// The end-of-turn reply line left the decider (29 Oct, Decider V5); bank lines stay playable and tested.
#[cfg_attr(not(test), allow(dead_code))]
pub fn offer_with_fallback(bank:&Bank,preferred:(String,&'static str),fallback_voice:&str)
    -> Result<(String,&'static str,Vec<Offered>,Option<String>)> {
    let (voice,generator)=preferred;
    let (offered,error)=offer(bank,&voice,generator)?;
    if !offered.is_empty() || generator!=speak::drama3::GENERATOR {
        return Ok((voice,generator,offered,error));
    }
    let (offered,fallback_error)=offer(bank,fallback_voice,voicebank::SUPERTONIC_ID)?;
    Ok((fallback_voice.into(),voicebank::SUPERTONIC_ID,offered,fallback_error.or(error)))
}
// The end-of-turn reply line left the decider (29 Oct, Decider V5); bank lines stay playable and tested.
#[cfg_attr(not(test), allow(dead_code))]
pub fn offer(bank: &Bank, voice: &str, generator: &str) -> Result<(Vec<Offered>, Option<String>)> {
    let (lines, error) = bank.lines.current();
    let mut takes: BTreeMap<String, Vec<i64>> = bank.store()?.takes(voice, generator)?;
    let offered = lines.iter().filter_map(|l| takes.remove(&l.id).map(|t| (l.clone(), t))).collect();
    Ok((offered, error))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use voicebank::Origin;
    pub(crate) fn fake_drama3(assets: &Path) -> Result<Arc<speak::drama3::Service>> {
        std::fs::create_dir_all(assets.join("runtime/bin"))?;
        std::os::unix::fs::symlink("/usr/bin/python3",assets.join("runtime/bin/python3"))?;
        std::fs::write(assets.join("sidecar.py"),
            "import json,sys\nprint(json.dumps({'event':'ready'}),flush=True)\nsys.stdin.read()\n")?;
        Ok(speak::drama3::Service::new(assets.into()))
    }
    #[test]
    fn imported_jarvis_takes_are_offered_by_the_live_reply_path_without_resident_drama() -> Result<()> {
        assert_eq!(crate::settings::Settings::default().free_space_floor_bytes,
            voicebank::import::DEFAULT_FREE_SPACE_FLOOR_BYTES);
        let temp=tempfile::tempdir()?;
        let data=temp.path().join("data");
        let assets=temp.path().join("fake-sidecar");
        let mut tools=Tools::new(&temp.path().join("models"),&data);
        tools.drama3=fake_drama3(&assets)?;
        let source=temp.path().join("source");
        let source_bank=Store::open(&source)?;
        let mut settings=speak::drama3::settings();
        settings["asr"]=serde_json::json!({"passed":true,"expected":"Sure.","text":"Sure."});
        let pcm=speak::Audio {samples:vec![0.15;441],sample_rate:44100,first_audio_ms:0.,synthesis_ms:0.};
        source_bank.insert(&voicebank::ClipMeta {origin:Origin::Bank,line_id:Some("sure".into()),text:"Sure.".into(),
            language:"en".into(),voice:speak::drama3::VOICE.into(),generator:speak::drama3::GENERATOR.into(),settings},&pcm)?;
        // Open the app's bank before import, so this tests fresh visibility on
        // its already-open SQLite connection rather than constructing a new app.
        assert!(offer(&tools.voice_bank,speak::drama3::VOICE,speak::drama3::GENERATOR)?.0.is_empty());
        let report=voicebank::import::run(&source,&data,true)?;
        assert_eq!(report.inserted,1);
        assert!(!tools.drama3.ready());
        let (voice,generator,offered,error)=offer_with_fallback(&tools.voice_bank,tools.bank_identity(),speak::VOICE)?;
        tools.drama3.stop();
        assert_eq!(voice,"jarvis");assert_eq!(generator,speak::drama3::GENERATOR);assert!(error.is_none());
        assert_eq!(offered.len(),1);assert_eq!(offered[0].0.id,"sure");assert_eq!(offered[0].1.len(),1);
        let picked=tools.voice_bank.pick_take(&voice,generator,&offered[0].0.id,&offered[0].1).unwrap();
        assert_eq!(picked,offered[0].1[0]);
        let back=tools.voice_bank.store()?.audio(picked)?;
        assert_eq!(back.samples,pcm.samples);
        Ok(())
    }
    #[test]
    fn empty_drama_bank_uses_supertonic_takes_and_accepted_drama_takes_win() {
        let (_temp,tools)=tools();let bank=&tools.voice_bank;
        let fallback=add_clip(&tools,"sure",100);
        let preference=||(speak::drama3::VOICE.into(),speak::drama3::GENERATOR);
        let (voice,generator,offered,_)=offer_with_fallback(bank,preference(),speak::VOICE).unwrap();
        assert_eq!(voice,speak::VOICE);assert_eq!(generator,voicebank::SUPERTONIC_ID);
        assert!(offered.iter().any(|(_,takes)|takes.contains(&fallback)));
        let audio=speak::Audio {samples:vec![0.2;100],sample_rate:44100,first_audio_ms:0.,synthesis_ms:0.};
        bank.store().unwrap().insert(&voicebank::ClipMeta {origin:Origin::Bank,line_id:Some("sure".into()),
            text:"Sure.".into(),language:"en".into(),voice:speak::drama3::VOICE.into(),
            generator:speak::drama3::GENERATOR.into(),settings:speak::drama3::settings()},&audio).unwrap();
        let (voice,generator,offered,_)=offer_with_fallback(bank,preference(),speak::VOICE).unwrap();
        assert_eq!(voice,speak::drama3::VOICE);assert_eq!(generator,speak::drama3::GENERATOR);assert!(!offered.is_empty());
    }
    /// Plays until cancelled or `hold` passes; records what it was given.
    pub(crate) struct Probe {
        pub log: Arc<Mutex<Vec<(usize, bool)>>>,
        pub hold: Duration,
    }
    impl speak::AudioOutput for Probe {
        fn output(&mut self, audio: &speak::Audio, cancel: &dyn Fn() -> bool) -> Result<()> {
            let start = Instant::now();
            while start.elapsed() < self.hold && !cancel() {
                std::thread::sleep(Duration::from_millis(2));
            }
            self.log.lock().unwrap().push((audio.samples.len(), cancel()));
            Ok(())
        }
    }
    pub(crate) fn probe(tools: &Tools, hold: Duration) -> Arc<Mutex<Vec<(usize, bool)>>> {
        let log = Arc::new(Mutex::new(vec![]));
        let l = log.clone();
        tools.voice_bank.set_output(Box::new(move || Box::new(Probe { log: l.clone(), hold }) as Box<dyn speak::AudioOutput>));
        log
    }
    pub(crate) fn tools() -> (tempfile::TempDir, Arc<Tools>) {
        let t = tempfile::tempdir().unwrap();
        let tools = Arc::new(Tools::new(&t.path().join("models"), &t.path().join("data")));
        let session = t.path().join("data/archive/session-test");
        *tools.archive_session.lock().unwrap() = Some(session);
        tools.set_agent_enabled(true);
        (t, tools)
    }
    pub(crate) fn add_clip(tools: &Tools, line: &str, samples: usize) -> i64 {
        let meta = ClipMeta {
            origin: Origin::Bank,
            line_id: Some(line.into()),
            text: line.into(),
            language: "en".into(),
            voice: speak::VOICE.into(),
            generator: GENERATOR.into(),
            settings: json!({}),
        };
        let audio = speak::Audio { samples: (0..samples).map(|i| (i as f32 * 0.01).sin() * 0.1).collect(), sample_rate: 16_000, first_audio_ms: 0., synthesis_ms: 0. };
        tools.voice_bank.store().unwrap().insert(&meta, &audio).unwrap().clip_id
    }
    pub(crate) fn wait_idle(tools: &Tools) {
        let start = Instant::now();
        while tools.voice_bank.line_playing() || tools.voice_bank.ack_waiting() {
            assert!(start.elapsed() < Duration::from_secs(5), "line never settled");
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    fn playback_records(tools: &Tools) -> Vec<Value> {
        let session = tools.archive_session.lock().unwrap().clone().unwrap();
        heimdall_archive::Journal::open(&session).unwrap().records().unwrap().into_iter().filter(|r| r.kind == "voice_reply_playback").map(|r| r.payload).collect()
    }
    /// The line has ended and its outcome is journalled (the record lands just after the voice frees).
    fn settle(tools: &Tools, records: usize) {
        wait_idle(tools);
        let start = Instant::now();
        while playback_records(tools).len() < records {
            assert!(start.elapsed() < Duration::from_secs(5), "outcome never journalled");
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    struct Gate(Arc<AtomicBool>);
    impl speak::Synthesizer for Gate {
        fn synthesize(&mut self, _: &str, _: &str, cancel: &dyn Fn() -> bool) -> Result<speak::Audio> {
            while !self.0.load(Ordering::Acquire) && !cancel() {
                std::thread::sleep(Duration::from_millis(2));
            }
            Ok(speak::Audio { samples: vec![0.; 32], sample_rate: 16_000, first_audio_ms: 0., synthesis_ms: 0. })
        }
    }

    #[test]
    fn line_plays_and_each_interruption_stops_it() {
        let (_t, tools) = tools();
        let clip = add_clip(&tools, "sure", 1600);
        let log = probe(&tools, Duration::from_millis(20));
        play(&tools, clip, tools.voice_bank.epoch(), "u1").unwrap();
        settle(&tools, 1);
        // The take plays once, at the user's voice speed (default 1.15x: 1600 samples stretched to 1391).
        assert_eq!(*log.lock().unwrap(), [(1391, false)]);
        let snapshot=tools.send_context.lock().unwrap().snapshot();
        assert_eq!(snapshot.turns.len(),1);
        assert_eq!((&*snapshot.turns[0].speaker,&*snapshot.turns[0].text),("Heimdall","sure"));
        let mut previous=tools.send_context.lock().unwrap().finished_ns.expect("bank playback did not arm follow-up");
        // No follow-up window (decision 33): a reply right after the line is the decider's, never a bypass.
        assert_eq!(decide::send::explicit_trigger("yes",true,previous+1,Some(previous)),None);
        type Interrupt = fn(&Tools);
        let cases: [(Interrupt, &str); 3] = [
            (|t| { t.voice("mic:1", true); t.interrupt(); }, "barge-in"),
            (|t| { t.enabled.store(false, Ordering::Release); t.interrupt(); }, "speaking off"),
            (|t| { t.enabled.store(false, Ordering::Release); t.shutdown(); }, "listening off"),
        ];
        for (n, (interrupt, label)) in cases.into_iter().enumerate() {
            tools.enabled.store(true, Ordering::Release);
            tools.voice("mic:1", false);
            let log = probe(&tools, Duration::from_secs(10));
            play(&tools, clip, tools.voice_bank.epoch(), label).unwrap();
            let start = Instant::now();
            while log.lock().unwrap().is_empty() && !tools.voice_bank.line_playing() {
                assert!(start.elapsed() < Duration::from_secs(2));
            }
            std::thread::sleep(Duration::from_millis(30));
            interrupt(&tools);
            settle(&tools, n + 2);
            let ended=tools.send_context.lock().unwrap().finished_ns.expect("interrupted bank playback must arm follow-up");
            assert!(ended>previous);previous=ended;
            assert_eq!(decide::send::explicit_trigger("actually change that",true,ended+1,Some(ended)),None);
            assert!(start.elapsed() < Duration::from_secs(5), "{label}: line kept playing");
            assert_eq!(*log.lock().unwrap(), [(1391, true)], "{label}: the take at the default 1.15x");
        }
        let outcomes: Vec<_> = playback_records(&tools).iter().map(|r| r["outcome"].as_str().unwrap().to_string()).collect();
        assert_eq!(outcomes, ["finished", "interrupted", "interrupted", "interrupted"]);
        // A stale decision (interrupted before playing) never starts.
        tools.enabled.store(true, Ordering::Release);
        tools.voice("mic:1", false);
        let epoch = tools.voice_bank.epoch();
        tools.interrupt();
        assert_eq!(play(&tools, clip, epoch, "stale"), Err("interrupted"));
        tools.voice("mic:1", true);
        assert_eq!(play(&tools, clip, tools.voice_bank.epoch(), "talking"), Err("owner_speaking"));
        // The user starting to talk over a line that is playing does not stop it by itself: only the
        // barge-in decision's interrupt does.
        tools.voice("mic:1", false);
        let log = probe(&tools, Duration::from_millis(150));
        let played = tools.voice_bank.lines_played.load(Ordering::Acquire);
        play(&tools, clip, tools.voice_bank.epoch(), "talked over").unwrap();
        let start = Instant::now();
        while tools.voice_bank.lines_played.load(Ordering::Acquire) == played {
            assert!(start.elapsed() < Duration::from_secs(2), "line never started");
            std::thread::sleep(Duration::from_millis(1));
        }
        tools.voice("mic:1", true);
        settle(&tools, 5);
        assert_eq!(*log.lock().unwrap(), [(1391, false)], "the line finished (at the default 1.15x)");
        assert_eq!(playback_records(&tools).last().unwrap()["outcome"], "finished");
    }

    #[test]
    fn never_overlaps_agent_speech() {
        let (t, tools) = tools();
        let clip = add_clip(&tools, "sure", 800);
        // Agent speech queued or synthesizing: the line is skipped.
        let release = Arc::new(AtomicBool::new(false));
        let agent_log = Arc::new(Mutex::new(vec![]));
        let out = Probe { log: agent_log.clone(), hold: Duration::from_millis(1) };
        tools.test_guarded_speech_queue(Gate(release.clone()), tools.voice_bank.exclusive(out));
        tools.test_speak("agent reply").unwrap();
        assert_eq!(play(&tools, clip, tools.voice_bank.epoch(), "u1"), Err("agent_speech"));
        release.store(true, Ordering::Release);
        let start = Instant::now();
        while tools.agent_speech_busy() {
            assert!(start.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(2));
        }
        // A line already playing holds the voice: agent speech that becomes ready waits for it.
        let line_log = probe(&tools, Duration::from_millis(150));
        play(&tools, clip, tools.voice_bank.epoch(), "u2").unwrap();
        let start = Instant::now();
        while tools.voice_bank.lines_played.load(Ordering::Acquire) < 1 {
            assert!(start.elapsed() < Duration::from_secs(5), "line never started");
            std::thread::sleep(Duration::from_millis(1));
        }
        let asked = Instant::now();
        tools.test_speak("second reply").unwrap();
        let start = Instant::now();
        while agent_log.lock().unwrap().len() < 2 {
            assert!(start.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(line_log.lock().unwrap().len(), 1, "the line finished before the agent spoke");
        assert!(asked.elapsed() >= Duration::from_millis(100), "agent output waited for the line");
        drop(t);
    }

    #[test]
    fn agent_speech_is_kept_in_the_bank() {
        let (_t, tools) = tools();
        let bank = &tools.voice_bank;
        let audio = speak::Audio { samples: vec![0.25; 400], sample_rate: 44_100, first_audio_ms: 0., synthesis_ms: 0. };
        let meta = |origin| ClipMeta {
            origin,
            line_id: None,
            text: "The build passed.".into(),
            language: "en".into(),
            voice: speak::VOICE.into(),
            generator: GENERATOR.into(),
            settings: voicebank::supertonic_settings("auto"),
        };
        bank.record(meta(Origin::Agent), audio);
        let start = Instant::now();
        loop {
            let counts = bank.store().unwrap().counts().unwrap();
            if counts.get("agent") == Some(&1) {
                break;
            }
            assert!(start.elapsed() < Duration::from_secs(5), "{counts:?}");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(bank.root(), tools.archive_session.lock().unwrap().as_ref().unwrap().parent().unwrap());
    }

    #[test]
    fn fill_job_runs_only_on_ac_while_idle_and_quiet() {
        let (_t, tools) = tools();
        let power = Arc::new(Mutex::new(heimdall_night::gate::State { on_ac: false, idle_seconds: 900. }));
        let p = power.clone();
        tools.test_quiet_since(Instant::now() - Duration::from_secs(600));
        // No supertonic models in this lab dir: an open gate reaches synthesis and fails there, which proves
        // the gate opened; the store stays empty.
        tools.voice_bank.lines_played.store(0, Ordering::Release);
        start_fill_with(&tools, move || Some(*p.lock().unwrap()), Duration::from_millis(5));
        std::thread::sleep(Duration::from_millis(60));
        assert!(!tools.voice_bank.is_open(), "battery: the bank was never opened");
        *power.lock().unwrap() = heimdall_night::gate::State { on_ac: true, idle_seconds: 10. };
        std::thread::sleep(Duration::from_millis(60));
        assert!(!tools.voice_bank.is_open(), "user active: still closed");
        *power.lock().unwrap() = heimdall_night::gate::State { on_ac: true, idle_seconds: 900. };
        let start = Instant::now();
        while !tools.voice_bank.is_open() {
            assert!(start.elapsed() < Duration::from_secs(5), "AC + idle + quiet opened the gate");
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}
