//! Speaking ▸ Voice: a click on a voice plays a short sample in it ("The sample of the voice should
//! be played"). Each voice's sample is rendered once at natural speed and kept in `<data>/voice-samples/<key>.wav`, so a
//! click plays at once; local voices are rendered ahead at idle (`Tools::prerender_voice_previews`), ElevenLabs voices
//! on their first click (each render spends account credit). Playback is stretched to the user's speed.
//!
//! A sample never plays over the user or a reply: it does not start while he is talking (or in the pause after him), while
//! Heimdall is audible or agent speech is queued, and it stops as soon as one of those begins. A newer click stops the
//! sample before it. The hold's state is only read here (never `OwnerVoice::wait`, whose skip latch belongs to replies).
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crate::speech_voice::Route;

/// What every voice says.
pub(crate) const PREVIEW_TEXT: &str = "Hi, I'm Heimdall. This is how I sound.";

/// The cache file's name for a voice: `drama3`, `supertonic`, `phonon-<preset>`, `elevenlabs-<voice id>`, with
/// anything outside `[A-Za-z0-9_-]` replaced, so a preset or voice name never leaves the folder.
pub(crate) fn key(route: &Route) -> String {
    let clean = |s: &str| s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).take(80).collect::<String>();
    match route {
        Route::Jarvis => "drama3".into(),
        Route::Supertonic => "supertonic".into(),
        Route::Phonon(preset) => format!("phonon-{}", clean(preset)),
        Route::ElevenLabs(id) => format!("elevenlabs-{}", clean(id)),
        // The server's address is part of it: another server's "alloy" is another voice.
        Route::Own(id) => {
            let base = crate::own_voice::saved().map(|e| e.base).unwrap_or_default();
            // FNV-1a: stable across builds, so a kept sample stays found.
            let h = format!("{base}\n{id}").bytes().fold(0x811c_9dc5_u32, |h, b| (h ^ b as u32).wrapping_mul(0x0100_0193));
            format!("own-{}-{h:08x}", clean(id))
        }
    }
}

/// Why a sample does not play now (`None`: it may).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Busy {
    OwnerTalking,
    ReplySpeaking,
}
/// `audible` counts only before the sample starts: once it plays, Heimdall audible is the sample itself.
pub(crate) fn busy(owner_holds: bool, audible: bool, agent_speech: bool, playing: bool) -> Option<Busy> {
    if owner_holds { Some(Busy::OwnerTalking) } else if (audible && !playing) || agent_speech { Some(Busy::ReplySpeaking) } else { None }
}

/// Asked before a sample starts (`false`) and while it plays (`true`).
pub(crate) type BusyCheck = Arc<dyn Fn(bool) -> Option<Busy> + Send + Sync>;

/// Told when the sample reaches the speaker and when it ends (the echo guard's reference, so the microphone hearing it
/// is not taken for the user).
pub(crate) struct Around {
    pub started: Box<dyn Fn() + Send>,
    pub ended: Box<dyn Fn() + Send>,
}
impl Around {
    pub(crate) fn none() -> Self { Self { started: Box::new(|| {}), ended: Box::new(|| {}) } }
}

pub(crate) struct Previews {
    dir: PathBuf,
    /// Bumped by each click: an older sample stops, and a render finished for an older click is kept but not played.
    generation: Arc<AtomicU64>,
}

impl Previews {
    pub(crate) fn new(data: &Path) -> Self {
        Self { dir: data.join("voice-samples"), generation: Arc::default() }
    }
    fn path(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{key}.wav"))
    }
    pub(crate) fn cached(&self, key: &str) -> bool {
        self.path(key).is_file()
    }
    pub(crate) fn load(&self, key: &str) -> Option<speak::Audio> {
        let mut reader = hound::WavReader::open(self.path(key)).ok()?;
        let rate = reader.spec().sample_rate;
        let samples: Vec<f32> = reader.samples::<i16>().filter_map(|s| s.ok()).map(|s| s as f32 / 32768.0).collect();
        (!samples.is_empty()).then_some(speak::Audio { samples, sample_rate: rate, first_audio_ms: 0.0, synthesis_ms: 0.0 })
    }
    /// Written whole to a temporary file and renamed, so a half-written sample is never played.
    pub(crate) fn save(&self, key: &str, audio: &speak::Audio) -> Result<()> {
        anyhow::ensure!(!audio.samples.is_empty() && audio.sample_rate > 0, "empty sample");
        std::fs::create_dir_all(&self.dir)?;
        let tmp = self.dir.join(format!(".{key}.{}.tmp", std::process::id()));
        {
            let spec = hound::WavSpec { channels: 1, sample_rate: audio.sample_rate, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
            let mut w = hound::WavWriter::create(&tmp, spec)?;
            for s in &audio.samples { w.write_sample((s.clamp(-1.0, 1.0) * 32767.0) as i16)?; }
            w.finalize()?;
        }
        std::fs::rename(&tmp, self.path(key)).context("voice sample not saved")
    }
    /// Stops whatever sample is playing (a newer click, a reply, the user).
    pub(crate) fn stop(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
    }
    /// Changes with every click (the ahead-of-time pass yields to one).
    pub(crate) fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }
    /// Plays `key`'s sample: the cached one, else `render`'s (kept for next time). Returns at once; the render and
    /// playback run on their own thread. `busy` is asked before playback and throughout it.
    pub(crate) fn play<O, R>(&self, key: String, render: R, mut output: O, rate: f32, busy: BusyCheck, around: Around) -> std::thread::JoinHandle<Outcome>
    where O: speak::AudioOutput, R: FnOnce(&dyn Fn() -> bool) -> Result<speak::Audio> + Send + 'static {
        let mine = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        let generation = self.generation.clone();
        let cache = Previews { dir: self.dir.clone(), generation: generation.clone() };
        std::thread::Builder::new().name("voice-preview".into()).spawn(move || {
            let current = || generation.load(Ordering::Acquire) == mine;
            if let Some(why) = busy(false) { return Outcome::Busy(why); }
            let audio = match cache.load(&key) {
                Some(a) => a,
                None => {
                    // A newer click, the user or a reply stops the render too (it would compete with the reply).
                    let audio = match render(&|| !current() || busy(false).is_some()) {
                        Ok(a) => a,
                        Err(_) if !current() => return Outcome::Superseded,
                        Err(e) => return Outcome::Failed(format!("{e:#}")),
                    };
                    if let Err(e) = cache.save(&key, &audio) { eprintln!("voice sample for {key} not kept: {e:#}"); }
                    audio
                }
            };
            if !current() { return Outcome::Superseded; }
            if let Some(why) = busy(false) { return Outcome::Busy(why); }
            let audio = if (rate - 1.0).abs() < 0.01 { audio } else { speak::Audio { samples: speak::stretch::stretch(&audio.samples, rate), ..audio } };
            let stop = || !current() || busy(true).is_some();
            (around.started)();
            let played = output.output(&audio, &stop);
            (around.ended)();
            match played {
                Ok(()) if current() && busy(true).is_none() => Outcome::Played,
                Ok(()) => Outcome::Stopped,
                Err(e) => Outcome::Failed(format!("{e:#}")),
            }
        }).expect("voice preview thread")
    }
}

impl super::Tools {
    /// Why a sample may not play now (the user talking or in his pause, Heimdall audible, agent speech queued).
    fn preview_busy(self: &Arc<Self>) -> BusyCheck {
        let tools = Arc::downgrade(self);
        Arc::new(move |playing| {
            let Some(t) = tools.upgrade() else { return Some(Busy::ReplySpeaking) };
            busy(t.owner.holds(), super::hold::audible(), t.agent_speech_busy(), playing)
        })
    }

    /// The echo guard's reference for the sample through speakers (`speech_policy`), without the island's spoken text.
    fn preview_around(self: &Arc<Self>) -> Around {
        let (a, b) = (Arc::downgrade(self), Arc::downgrade(self));
        Around {
            started: Box::new(move || if let Some(t) = a.upgrade() {
                t.speech_policy.sentence_started(PREVIEW_TEXT, capture::host_now_ns(), crate::pipeline::output_route::current() == crate::pipeline::output_route::Route::Speakers);
            }),
            ended: Box::new(move || if let Some(t) = b.upgrade() { t.speech_policy.voice_stopped(capture::host_now_ns()); }),
        }
    }

    /// One voice's sample at natural speed, rendered by that voice only (no fallback to another voice). Drama 3 is
    /// used only once resident: a click on it while it loads waits for the load (the Speed sample's hold keeps it).
    fn render_preview(self: &Arc<Self>, route: &Route, cancel: &dyn Fn() -> bool) -> Result<speak::Audio> {
        self.render_text(route, PREVIEW_TEXT, cancel)
    }
    fn render_text(self: &Arc<Self>, route: &Route, text: &str, cancel: &dyn Fn() -> bool) -> Result<speak::Audio> {
        match route {
            Route::Jarvis => {
                if !self.drama3.ready() {
                    *self.sample_hold.lock().unwrap() = Some(std::time::Instant::now() + super::speech::SAMPLE_LOAD_WAIT + super::speech::SAMPLE_KEEP);
                    self.drama3.configure(true);
                    self.drama3.wait_ready(super::speech::SAMPLE_LOAD_WAIT, cancel);
                }
                anyhow::ensure!(self.drama3.ready(), "Drama 3 is not loaded");
                self.drama3.render(&speak::drama3::normalize(text), cancel)
            }
            Route::Supertonic => {
                let mut synth = super::speech::LazySynth {
                    drama3: None,
                    session: self.archive_session.lock().unwrap().clone().context("speech archive session missing")?,
                    models: self.models.clone(),
                    model: self.tts_model.clone(),
                    backend: self.tts_backend.lock().unwrap().clone(),
                    record: None,
                    speed: Arc::new(std::sync::atomic::AtomicU16::new(100)),
                    sample: None,
                    voices: None,
                };
                speak::Synthesizer::synthesize(&mut synth, text, "en", cancel)
            }
            Route::Phonon(_) | Route::ElevenLabs(_) | Route::Own(_) => {
                let mut samples = vec![];
                let mut rate = 0;
                self.speech_voices.speak_sentence(route, text, &speak::drama3::normalize(text), 1.0, cancel, &mut |packet| {
                    if let speak::SpeechPacket::Audio(a) = packet { rate = a.sample_rate; samples.extend_from_slice(&a.samples); }
                    Ok(())
                })?;
                anyhow::ensure!(!samples.is_empty(), "the voice said nothing");
                Ok(speak::Audio { samples, sample_rate: rate, first_audio_ms: 0.0, synthesis_ms: 0.0 })
            }
        }
    }

    /// Speaking ▸ Voice clicked (the choice already applied): its sample, from the cache or rendered now, at the user's
    /// speed; the sample before it stops. Nothing plays while he talks or a reply speaks.
    pub(crate) fn preview_voice(self: &Arc<Self>) {
        // One sample at a time: the Speed slider's stops for this one.
        if let Some(q) = self.sample_queue.lock().unwrap().as_ref() { q.interrupt(); }
        let route = self.speech_voices.route();
        let me = self.clone();
        let r = route.clone();
        let rate = self.voice_speed.load(Ordering::Acquire) as f32 / 100.0;
        let output = self.voice_bank.exclusive(speak::CoreAudioOutput);
        let handle = self.voice_previews.play(key(&route), move |cancel| me.render_preview(&r, cancel), output, rate, self.preview_busy(), self.preview_around());
        let _ = std::thread::Builder::new().name("voice-preview-result".into()).spawn(move || {
            match handle.join() {
                Ok(Outcome::Failed(why)) => eprintln!("voice sample: {why}"),
                Ok(_) | Err(_) => {}
            }
        });
    }

    /// The local voices' samples, rendered ahead once (Supertonic, every Phonon S preset, Drama 3 while resident), so
    /// a click never waits on a model. A low-priority thread that works only while Heimdall is silent and the user is not
    /// talking, one sample at a time; ElevenLabs voices are left for their first click (each render spends credit).
    pub(crate) fn prerender_voice_previews(self: &Arc<Self>) {
        let tools = Arc::downgrade(self);
        let _ = std::thread::Builder::new().name("voice-preview-prerender".into()).spawn(move || {
            // This thread only: `setpriority(PRIO_PROCESS)` reniced the whole app and every helper it started.
            let _ = heimdall_night::gate::background_thread();
            std::thread::sleep(std::time::Duration::from_secs(90));
            let routes = {
                let Some(t) = tools.upgrade() else { return };
                // The menu's built-in voice (Phonon S, each preset), and Drama 3 while it is the saved voice and resident.
                let mut r: Vec<Route> = t.speech_voices.ui_state().presets.iter().map(|p| Route::Phonon(p.name.clone())).collect();
                r.push(Route::Jarvis);
                r
            };
            for route in routes {
                loop {
                    let Some(t) = tools.upgrade() else { return };
                    if t.voice_previews.cached(&key(&route)) { break; }
                    if (t.preview_busy())(false).is_some() { drop(t); std::thread::sleep(std::time::Duration::from_secs(15)); continue; }
                    // Drama 3 only while already resident: never loaded for a sample nobody asked for.
                    if route == Route::Jarvis && !t.drama3.ready() { break; }
                    let busy = t.preview_busy();
                    let generation = t.voice_previews.generation();
                    // A click (it may need the voice's lock) or a reply ends this render; the voice is tried again later.
                    let result = t.render_inline(&route, &|| busy(false).is_some() || t.voice_previews.generation() != generation);
                    match result {
                        Ok(audio) => { if let Err(e) = t.voice_previews.save(&key(&route), &audio) { eprintln!("voice sample for {} not kept: {e:#}", key(&route)); } }
                        Err(e) => eprintln!("voice sample for {} not rendered ahead: {e:#}", key(&route)),
                    }
                    break;
                }
            }
            // Phonon S (~300 MB) stays loaded only while it is the voice.
            if let Some(t) = tools.upgrade() { t.speech_voices.release_unused(); }
        });
    }

    /// `render_preview` without loading anything (the ahead-of-time pass).
    fn render_inline(self: &Arc<Self>, route: &Route, cancel: &dyn Fn() -> bool) -> Result<speak::Audio> {
        if *route == Route::Jarvis { anyhow::ensure!(self.drama3.ready(), "Drama 3 not resident"); }
        self.render_preview(route, cancel)
    }

    /// Heimdall's own-voice enrolment (pipeline/heimdall_voice.rs): one line in `route`'s voice at natural speed,
    /// rendered silently (never played, not recorded in the voice bank). Nothing is loaded or reloaded into the
    /// app's voices for it, checked before every line: Jarvis and Phonon S only while already loaded; Supertonic the
    /// shared model when resident, else a private CPU instance dropped with the lines (about 400 MB for a few
    /// seconds, only with 8 GB free).
    pub(crate) fn render_voice_lines(self: &Arc<Self>, route: &Route, texts: &[&str], cancel: &dyn Fn() -> bool) -> Result<Vec<speak::Audio>> {
        use speak::Synthesizer;
        let mut private = None;
        if *route == Route::Supertonic && self.tts_model.lock().unwrap().is_none() {
            anyhow::ensure!(!crate::edition::PUBLIC, "no Supertonic in this edition");
            // About 400 MB for a few seconds: only with plenty of memory free.
            let free = crate::diagnostics::available_memory_bytes().unwrap_or(0);
            anyhow::ensure!(free >= 8 << 30, "not enough free memory for a private Supertonic");
            private = Some(speak::Supertonic::load_with_backend(&self.models, speak::TtsBackend::Cpu)?);
        }
        let mut out = vec![];
        for text in texts {
            anyhow::ensure!(!cancel(), "speech interrupted");
            // Checked before every line: a voice unloaded meanwhile is never loaded again for this.
            let audio = match route {
                _ if private.is_some() => private.as_mut().unwrap().synthesize(text, "en", cancel)?,
                Route::Jarvis => {
                    anyhow::ensure!(self.drama3.ready(), "Drama 3 not resident");
                    self.drama3.render(&speak::drama3::normalize(text), cancel)?
                }
                Route::Supertonic => {
                    let mut shared = self.tts_model.lock().unwrap();
                    let model = shared.as_mut().context("Supertonic not resident")?;
                    model.synthesize(text, "en", cancel)?
                }
                Route::Phonon(_) => {
                    anyhow::ensure!(self.speech_voices.phonon_loaded(), "Phonon S not resident");
                    self.render_text(route, text, cancel)?
                }
                Route::Own(_) => self.render_text(route, text, cancel)?,
                Route::ElevenLabs(_) => anyhow::bail!("ElevenLabs is never rendered for this (account credit)"),
            };
            out.push(audio);
        }
        Ok(out)
    }
    /// A line in this voice can be rendered now without touching a resident model. ElevenLabs never: each render
    /// spends account credit.
    pub(crate) fn voice_renderable(&self, route: &Route) -> bool {
        match route {
            Route::Jarvis => self.drama3.ready(),
            Route::Supertonic => !crate::edition::PUBLIC,
            Route::Phonon(_) => self.speech_voices.phonon_loaded(),
            Route::Own(_) => crate::own_voice::saved().is_some(),
            Route::ElevenLabs(_) => false,
        }
    }
    /// The voices that can speak a reply now: the chosen one, and the lighter voice that says a sentence the chosen
    /// one cannot (Supertonic, in the user's edition; the public edition's lighter voice is the chosen Phonon S).
    pub(crate) fn speaking_voices(&self) -> Vec<Route> {
        let route = self.speech_voices.route();
        let mut voices = vec![route.clone()];
        if !crate::edition::PUBLIC && route != Route::Supertonic {
            voices.push(Route::Supertonic);
        }
        voices
    }
    /// A voice's sample key (`drama3`, `supertonic`, `phonon-Edwin`): also the key of its own-voice profile.
    pub(crate) fn voice_sample_key(&self, route: &Route) -> String {
        key(route)
    }
    /// Why a background render must wait now (the user talking or in his pause, Heimdall audible, speech queued).
    pub(crate) fn voice_busy(self: &Arc<Self>) -> BusyCheck {
        self.preview_busy()
    }
}

/// How one sample ended (tests and diagnostics).
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Outcome {
    Played,
    /// Cut by a newer click, the user or a reply while it played.
    Stopped,
    /// A newer click came before it could start.
    Superseded,
    Busy(Busy),
    Failed(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A speaker that "plays" for `ms`, checking cancel as CoreAudio does, and records what it was given.
    struct Speaker { played: Arc<Mutex<Vec<usize>>>, ms: u64 }
    impl speak::AudioOutput for Speaker {
        fn output(&mut self, audio: &speak::Audio, cancel: &dyn Fn() -> bool) -> Result<()> {
            for _ in 0..self.ms / 5 {
                if cancel() { return Ok(()); }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            self.played.lock().unwrap().push(audio.samples.len());
            Ok(())
        }
    }
    fn tone(n: usize) -> speak::Audio {
        speak::Audio { samples: (0..n).map(|i| (i as f32 * 0.01).sin() * 0.5).collect(), sample_rate: 24_000, first_audio_ms: 0.0, synthesis_ms: 0.0 }
    }
    fn free() -> BusyCheck { Arc::new(|_| None) }

    #[test]
    fn keys_stay_inside_the_folder() {
        assert_eq!(key(&Route::Jarvis), "drama3");
        assert_eq!(key(&Route::Phonon("Hadley".into())), "phonon-Hadley");
        assert_eq!(key(&Route::Phonon("../../x y".into())), "phonon-______x_y");
        assert_eq!(key(&Route::ElevenLabs("21m00Tcm4TlvDq8ikWAM".into())), "elevenlabs-21m00Tcm4TlvDq8ikWAM");
    }

    #[test]
    fn talking_and_replies_outrank_a_sample() {
        assert_eq!(busy(false, false, false, false), None);
        assert_eq!(busy(true, true, true, false), Some(Busy::OwnerTalking));
        assert_eq!(busy(false, true, false, false), Some(Busy::ReplySpeaking));
        assert_eq!(busy(false, false, true, true), Some(Busy::ReplySpeaking));
        // Playing, Heimdall audible is the sample itself: it does not stop itself (review 23 Oct).
        assert_eq!(busy(false, true, false, true), None);
    }

    /// Rendered once, kept, then played from the cache with no render; a sample is never played while busy.
    #[test]
    fn renders_once_then_plays_from_the_cache_and_waits_for_nobody() {
        let dir = tempfile::tempdir().unwrap();
        let p = Previews::new(dir.path());
        let played = Arc::new(Mutex::new(vec![]));
        let renders = Arc::new(Mutex::new(0));
        let r = renders.clone();
        let out = p.play("supertonic".into(), move |_| { *r.lock().unwrap() += 1; Ok(tone(2400)) }, Speaker { played: played.clone(), ms: 10 }, 1.0, free(), Around::none()).join().unwrap();
        assert_eq!(out, Outcome::Played);
        assert!(p.cached("supertonic"));
        let back = p.load("supertonic").unwrap();
        assert_eq!((back.samples.len(), back.sample_rate), (2400, 24_000));
        let out = p.play("supertonic".into(), |_| panic!("cached: no render"), Speaker { played: played.clone(), ms: 10 }, 1.0, free(), Around::none()).join().unwrap();
        assert_eq!((out, *renders.lock().unwrap(), played.lock().unwrap().len()), (Outcome::Played, 1, 2));
        // Faster speed: fewer samples reach the speaker.
        p.play("supertonic".into(), |_| unreachable!(), Speaker { played: played.clone(), ms: 10 }, 1.5, free(), Around::none()).join().unwrap();
        assert!(played.lock().unwrap()[2] < 2400);
        // The user talking: nothing plays, and nothing is rendered for it either.
        let out = p.play("drama3".into(), |_| panic!("no render while he talks"), Speaker { played: played.clone(), ms: 10 }, 1.0, Arc::new(|_| Some(Busy::OwnerTalking)), Around::none()).join().unwrap();
        assert_eq!(out, Outcome::Busy(Busy::OwnerTalking));
        assert_eq!(played.lock().unwrap().len(), 3);
    }

    /// Review 23 Oct: the sample's own playback raises "Heimdall audible"; it must not stop itself on that, and the
    /// echo guard hears about it from start to end.
    #[test]
    fn the_sample_does_not_stop_itself_and_the_echo_guard_hears_it() {
        let dir = tempfile::tempdir().unwrap();
        let p = Previews::new(dir.path());
        p.save("a", &tone(100)).unwrap();
        let audible = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (a, b) = (audible.clone(), audible.clone());
        let busy: BusyCheck = Arc::new(move |playing| super::busy(false, a.load(Ordering::Acquire), false, playing));
        let marks = Arc::new(Mutex::new(vec![]));
        let (m1, m2) = (marks.clone(), marks.clone());
        struct Loud { audible: Arc<std::sync::atomic::AtomicBool> }
        impl speak::AudioOutput for Loud {
            fn output(&mut self, _: &speak::Audio, cancel: &dyn Fn() -> bool) -> Result<()> {
                self.audible.store(true, Ordering::Release);
                for _ in 0..10 { anyhow::ensure!(!cancel(), "cut by itself"); std::thread::sleep(std::time::Duration::from_millis(5)); }
                self.audible.store(false, Ordering::Release);
                Ok(())
            }
        }
        let out = p.play("a".into(), |_| unreachable!(), Loud { audible: b }, 1.0, busy,
            Around { started: Box::new(move || m1.lock().unwrap().push("start")), ended: Box::new(move || m2.lock().unwrap().push("end")) }).join().unwrap();
        assert_eq!(out, Outcome::Played);
        assert_eq!(*marks.lock().unwrap(), ["start", "end"]);
    }

    /// A newer click stops the one playing; a reply that starts mid-sample stops it too.
    #[test]
    fn a_newer_click_or_a_reply_stops_the_sample() {
        let dir = tempfile::tempdir().unwrap();
        let p = Previews::new(dir.path());
        p.save("a", &tone(100)).unwrap();
        p.save("b", &tone(200)).unwrap();
        let played = Arc::new(Mutex::new(vec![]));
        let first = p.play("a".into(), |_| unreachable!(), Speaker { played: played.clone(), ms: 2000 }, 1.0, free(), Around::none());
        std::thread::sleep(std::time::Duration::from_millis(50));
        let second = p.play("b".into(), |_| unreachable!(), Speaker { played: played.clone(), ms: 10 }, 1.0, free(), Around::none());
        assert_eq!(first.join().unwrap(), Outcome::Stopped);
        assert_eq!(second.join().unwrap(), Outcome::Played);
        let reply = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let r = reply.clone();
        let busy: BusyCheck = Arc::new(move |_| r.load(Ordering::Acquire).then_some(Busy::ReplySpeaking));
        let third = p.play("a".into(), |_| unreachable!(), Speaker { played: played.clone(), ms: 2000 }, 1.0, busy, Around::none());
        std::thread::sleep(std::time::Duration::from_millis(50));
        reply.store(true, Ordering::Release);
        assert_eq!(third.join().unwrap(), Outcome::Stopped);
        // A failed render plays nothing and keeps nothing.
        let out = p.play("c".into(), |_| anyhow::bail!("Phonon S not loaded"), Speaker { played: played.clone(), ms: 10 }, 1.0, free(), Around::none()).join().unwrap();
        assert_eq!(out, Outcome::Failed("Phonon S not loaded".into()));
        assert!(!p.cached("c"));
        assert_eq!(*played.lock().unwrap(), vec![200]);
    }
}
