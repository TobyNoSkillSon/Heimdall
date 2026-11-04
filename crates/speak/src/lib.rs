//! Local English/Polish TTS. Submission never waits for inference or playback.
pub mod coverage;
pub mod gpu_recovery;
pub mod drama3;
// Jarvis gives way to Phonon S when memory runs short (bug note 2026-11-03T22-38-44).
pub mod memory_line;
mod graph_patch;
#[cfg(target_os = "macos")]
mod metal;
mod onnx;
mod playback;
pub mod stretch;
pub mod route;
// Phonon S and the API voices (night 1022 menu): voices that stream at their own rate, stretched to the user's speed.
pub mod voice;
pub mod phonon;
use anyhow::{Context, Result, ensure};
#[doc(hidden)]
pub use onnx::shader_module_probe;
pub use onnx::{chunk_text, preprocess_text, write_wav_file};
pub use playback::{CoreAudioOutput, LevelHook, SpeakingHook, SpokenTextHook, set_level_hook, set_speaking_hook, set_spoken_text_hook};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::Instant,
};

#[derive(Debug, Clone)]
pub struct Audio {
    pub samples: Vec<f32>,
    pub sample_rate: u32,
    pub first_audio_ms: f64,
    pub synthesis_ms: f64,
}
impl Audio {
    pub fn duration_s(&self) -> f64 {
        self.samples.len() as f64 / self.sample_rate as f64
    }
    pub fn real_time_factor(&self) -> f64 {
        self.synthesis_ms / 1000.0 / self.duration_s()
    }
    pub fn write_wav(&self, path: impl AsRef<Path>) -> Result<()> {
        write_wav_file(path, &self.samples, self.sample_rate as i32)
    }
}
pub type EvidenceHook = Box<dyn FnMut(serde_json::Value) -> Result<()> + Send>;

/// Supertonic's one style, the silent stand-in for Jarvis (Drama 3) when Jarvis is not loaded or a sentence fails
/// verification. M3 is upstream's "polished, authoritative male voice" (supertonic-py `docs/voices.md`), the
/// preset closest to Jarvis's calm, refined, measured delivery.
pub const VOICE: &str = "M3";
/// Fixed render settings: vector-estimator denoising steps and the natural speaking speed. The user's voice speed is
/// applied after synthesis by `stretch` (WSOLA), as for Jarvis: on 18 Oct the model's own duration control lost
/// words at 1.15x and above (recognizer 17/20 against 20/20 stretched, rates 0.8 to 1.6).
pub const DENOISING_STEPS: usize = 5;
pub const SPEED: f32 = 1.05;

fn voice_style_path(models: &Path, id: &str) -> PathBuf {
    models.join("voice_styles").join(format!("{id}.json"))
}

pub struct Supertonic {
    evidence: Option<EvidenceHook>,
    tts: gpu_recovery::Recovery<onnx::TextToSpeech>,
    models: PathBuf,
    style: onnx::Style,
}
/// Explicit backend for agent-operated parity checks. Ordinary callers use `load`.
#[derive(Clone, Copy, Debug)]
pub enum TtsBackend {
    Cpu,
    Metal,
}
impl Supertonic {
    /// Archive every component invocation before its result is transformed or played.
    pub fn set_evidence_hook(&mut self, hook: EvidenceHook) { self.evidence = Some(hook); }
    pub fn load(models: impl AsRef<Path>) -> Result<Self> {
        let backend = match std::env::var("HEIMDALL_TTS_BACKEND").as_deref() {
            Ok("metal") => TtsBackend::Metal,
            Ok("cpu") => TtsBackend::Cpu,
            Err(_) => {
                #[cfg(target_os = "macos")]
                if metal::Gpu::qualified_device() {
                    TtsBackend::Metal
                } else {
                    TtsBackend::Cpu
                }
                #[cfg(not(target_os = "macos"))]
                {
                    TtsBackend::Cpu
                }
            }
            Ok(value) => anyhow::bail!("unknown HEIMDALL_TTS_BACKEND {value}"),
        };
        Self::load_with_backend(models, backend)
    }
    pub fn load_with_backend(models: impl AsRef<Path>, backend: TtsBackend) -> Result<Self> {
        let p = models.as_ref();
        Ok(Self {
            evidence: None,
            models: p.to_owned(),
            tts: gpu_recovery::Recovery::new(
                onnx::load_text_to_speech(
                    p.join("onnx")
                        .to_str()
                        .ok_or_else(|| anyhow::anyhow!("non-UTF8 model path"))?,
                    matches!(backend, TtsBackend::Metal),
                )?,
                matches!(backend, TtsBackend::Metal),
            ),
            style: onnx::load_voice_style(
                &[voice_style_path(p, VOICE).to_string_lossy().into()],
                false,
            )?,
        })
    }
    /// The style in use.
    pub fn voice(&self) -> &str {
        VOICE
    }

    pub fn render(&mut self, text: &str, lang: &str) -> Result<Audio> {
        self.render_cancel(text, lang, || false)
    }
    fn render_cancel(
        &mut self,
        text: &str,
        lang: &str,
        cancel: impl Fn() -> bool,
    ) -> Result<Audio> {
        ensure!(matches!(lang, "en" | "pl"), "Heimdall supports en/pl");
        ensure!(
            !text.trim().is_empty() && text.len() <= 32_768,
            "empty or excessive speech"
        );
        let start = Instant::now();
        let mut samples = Vec::new();
        let mut first = None;
        for chunk in chunk_text(text, Some(120)) {
            ensure!(!cancel(), "speech interrupted");
            let (wav, durations) = self.tts.run(
                |gpu| {
                    onnx::load_text_to_speech(
                        self.models
                            .join("onnx")
                            .to_str()
                            .context("non-UTF8 model path")?,
                        gpu,
                    )
                },
                |tts| {
                    tts._infer(
                        std::slice::from_ref(&chunk),
                        &[lang.into()],
                        &self.style,
                        DENOISING_STEPS,
                        SPEED,
                        &cancel,
                        &mut self.evidence,
                    )
                },
            )?;
            ensure!(
                durations[0].is_finite() && durations[0] > 0. && durations[0] < 60.,
                "invalid speech duration"
            );
            ensure!(!cancel(), "speech interrupted");
            let n = ((durations[0] * self.tts.model.as_ref().unwrap().sample_rate as f32) as usize)
                .min(wav.len());
            first.get_or_insert(start.elapsed().as_secs_f64() * 1000.);
            samples.extend_from_slice(&wav[..n]);
        }
        ensure!(samples.iter().all(|x| x.is_finite()), "nonfinite audio");
        Ok(Audio {
            samples,
            sample_rate: self.tts.model.as_ref().unwrap().sample_rate as u32,
            first_audio_ms: first.unwrap_or(0.),
            synthesis_ms: start.elapsed().as_secs_f64() * 1000.,
        })
    }
}
/// Shared invalidation reaches the render callback without waiting for a worker poll.
#[derive(Clone)]
pub struct Cancellation(Arc<dyn Fn() -> bool + Send + Sync>);
impl Cancellation {
    pub fn new(check:impl Fn()->bool + Send + Sync + 'static)->Self {Self(Arc::new(check))}
    pub fn cancelled(&self)->bool {(self.0)()}
    pub fn or(self,check:impl Fn()->bool + Send + Sync + 'static)->Self {
        Self::new(move||self.cancelled() || check())
    }
    pub fn sample(&self,next:&mut impl FnMut()->f32)->f32 {if self.cancelled() {0.} else {next()}}
}
/// A restart invalidates queued and currently rendering chunks immediately.
#[derive(Debug)]
/// `Sentence`: the next audio starts saying this text (shown on the island as it starts playing).
pub enum SpeechPacket { Audio(Audio), RestartFirst, Sentence(String) }
/// `label`: the sentence this chunk starts, announced when the speaker reaches it.
pub struct PlaybackChunk { pub audio:Audio, pub generation:u64, pub label:Option<Arc<str>> }
/// What a stream's start check says right before its first sample (`PlaybackStream::with_start_check`).
pub enum MayStart {
    /// Start now.
    Start,
    /// Not yet: keep taking audio and look again.
    Wait,
    /// Never: the job fails with this error, unheard.
    Skip(anyhow::Error),
    /// Never: the job ends unplayed (its interrupt follows).
    Drop,
}
pub struct PlaybackStream {
    pub(crate) chunks:mpsc::Receiver<PlaybackChunk>,
    pub(crate) generation:Arc<AtomicU64>,
    /// Chunks taken off the producer before playback started (`take_ready`); they play first.
    early:std::collections::VecDeque<PlaybackChunk>,
    played:Arc<std::sync::atomic::AtomicBool>,
    /// The queue's claim on the speaking flag, which this stream's audio is part of.
    speaking:Option<Arc<playback::SpeakingHold>>,
    /// The user's voice speed the audio was stretched to (1.0 = natural): playback's pre-buffer sizes itself by it.
    pub(crate) pace:f32,
    /// Consulted right before the first sample, after any pre-buffer (night 1022 review: the app's hold decided the
    /// line may start, then up to 1.2 s of pre-buffer passed with nobody looking at the user again).
    start_check:Option<Box<dyn FnMut()->MayStart + Send>>,
}
impl PlaybackStream {
    pub fn new(chunks:mpsc::Receiver<PlaybackChunk>,generation:Arc<AtomicU64>)->Self {Self {chunks,generation,early:Default::default(),played:Arc::new(std::sync::atomic::AtomicBool::new(false)),speaking:None,pace:1.,start_check:None}}
    /// The output asks this right before its first sample (`may_start`); without one it starts.
    pub fn with_start_check(mut self,check:impl FnMut()->MayStart + Send + 'static)->Self {self.start_check=Some(Box::new(check));self}
    /// Whether the first sample may play now. Outputs with a pre-buffer call it once the pre-buffer is full, and
    /// again while it says `Wait`.
    pub fn may_start(&mut self)->MayStart {self.start_check.as_mut().map_or(MayStart::Start,|check|check())}
    /// Playback started: the check is done with (and lets go of whatever it holds).
    pub(crate) fn started(&mut self) {self.start_check=None;}
    /// The voice speed this stream's audio was stretched to (`Synthesizer::pace`).
    pub fn with_pace(mut self,pace:f32)->Self {self.pace=if pace.is_finite() && pace>0. {pace} else {1.};self}
    pub(crate) fn with_speaking(mut self,hold:Arc<playback::SpeakingHold>)->Self {self.speaking=Some(hold);self}
    pub(crate) fn speaking(&self)->Option<Arc<playback::SpeakingHold>> {self.speaking.clone()}
    /// Receipt set only by the output after it consumes audio.
    pub fn playback_receipt(&self)->Arc<std::sync::atomic::AtomicBool> {self.played.clone()}
    /// Moves what the producer has made so far off its bounded channel into this stream's own buffer, waiting up to
    /// `wait` for something new, for an output that may not start playing yet (the app's hold while the user talks). The
    /// producer then never blocks on that output: Drama 3 answers its sentence checks only between packets, and a
    /// check past its deadline hands the sentence to the lighter voice (night review 20 Oct). Audio a restarted first
    /// sentence replaced is dropped. Nothing of this stream is audible meanwhile, so a reply that has been silent too
    /// long lets go of the speaking flag, as it does while a device waits for its first sample. False once the
    /// producer has finished.
    pub fn take_ready(&mut self,wait:std::time::Duration)->bool {
        if let Some(hold)=&self.speaking { hold.release_if_silent(Instant::now()); }
        let open=match self.chunks.recv_timeout(wait) {
            Ok(chunk)=>{
                self.early.push_back(chunk);
                loop {
                    match self.chunks.try_recv() {
                        Ok(chunk)=>self.early.push_back(chunk),
                        Err(mpsc::TryRecvError::Empty)=>break true,
                        Err(mpsc::TryRecvError::Disconnected)=>break false,
                    }
                }
            },
            Err(mpsc::RecvTimeoutError::Timeout)=>true,
            Err(mpsc::RecvTimeoutError::Disconnected)=>false,
        };
        let current=self.generation.load(Ordering::Acquire);
        self.early.retain(|chunk|chunk.generation==current);
        open
    }
    /// Audio taken early (`take_ready`) is waiting to play.
    pub fn has_audio(&self)->bool {
        let current=self.generation.load(Ordering::Acquire);
        self.early.iter().any(|chunk|chunk.generation==current)
    }
    pub(crate) fn recv(&mut self,timeout:std::time::Duration)->std::result::Result<PlaybackChunk,mpsc::RecvTimeoutError> {
        while let Some(chunk)=self.early.pop_front() {
            if chunk.generation==self.generation.load(Ordering::Acquire){return Ok(chunk);}
        }
        let deadline=Instant::now()+timeout;
        loop {
            let chunk=self.chunks.recv_timeout(deadline.saturating_duration_since(Instant::now()))?;
            if chunk.generation==self.generation.load(Ordering::Acquire){return Ok(chunk);}
        }
    }
    /// The next chunk without waiting (the render callback): chunks taken early first. Stale generations are the
    /// caller's to skip.
    pub(crate) fn try_next(&mut self)->std::result::Result<PlaybackChunk,mpsc::TryRecvError> {
        match self.early.pop_front() {
            Some(chunk)=>Ok(chunk),
            None=>self.chunks.try_recv(),
        }
    }
}
pub trait AudioOutput: Send + 'static {
    fn output(&mut self, audio: &Audio, cancel: &dyn Fn() -> bool) -> Result<()>;
    fn output_cancellable(&mut self,audio:&Audio,cancel:&dyn Fn()->bool,signal:Cancellation)->Result<()> {
        self.output(audio,&||cancel() || signal.cancelled())
    }
    fn output_stream(&mut self, mut chunks: PlaybackStream, cancel:&dyn Fn()->bool, signal:Cancellation) -> Result<()> {
        loop {
            if cancel() || signal.cancelled() { return Ok(()); }
            match chunks.recv(std::time::Duration::from_millis(5)) {
                Ok(chunk)=>{
                    let epoch=chunks.generation.clone();let expected=chunk.generation;
                    let playing=!chunk.audio.samples.is_empty() && !cancel() && !signal.cancelled();
                    self.output_cancellable(&chunk.audio,cancel,signal.clone().or(move||epoch.load(Ordering::Acquire)!=expected))?;
                    if playing {chunks.played.store(true,Ordering::Release);}
                },
                Err(mpsc::RecvTimeoutError::Timeout)=>{},
                Err(mpsc::RecvTimeoutError::Disconnected)=>return Ok(()),
            }
        }
    }
}
/// Offline sink for tests/daytime validation. Never initializes a speaker device.
pub struct WavOutput {
    path: PathBuf,
    writer: Option<hound::WavWriter<std::io::BufWriter<std::fs::File>>>,
    sample_rate: Option<u32>,
}
impl WavOutput {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            writer: None,
            sample_rate: None,
        }
    }
}
impl AudioOutput for WavOutput {
    fn output(&mut self, audio: &Audio, cancel: &dyn Fn() -> bool) -> Result<()> {
        if !cancel() {
            if self.writer.is_none() {
                self.writer = Some(hound::WavWriter::create(
                    &self.path,
                    hound::WavSpec {
                        channels: 1,
                        sample_rate: audio.sample_rate,
                        bits_per_sample: 16,
                        sample_format: hound::SampleFormat::Int,
                    },
                )?);
                self.sample_rate = Some(audio.sample_rate);
            }
            ensure!(
                self.sample_rate == Some(audio.sample_rate),
                "WAV stream sample rate changed"
            );
            let writer = self.writer.as_mut().unwrap();
            for &s in &audio.samples {
                writer.write_sample((s.clamp(-1., 1.) * 32767.) as i16)?;
            }
            writer.flush()?;
        }
        Ok(())
    }
}
pub trait Synthesizer: Send + 'static {
    fn synthesize(&mut self, text: &str, lang: &str, cancel: &dyn Fn() -> bool) -> Result<Audio>;
    /// The speed its audio is played at relative to natural speech (Speaking ▸ Speed), read at each job.
    fn pace(&self) -> f32 { 1. }
    fn stream(&mut self, text: &str, lang: &str, cancel: &dyn Fn() -> bool,
              emit: &mut dyn FnMut(&Audio) -> Result<()>) -> Result<Audio> {
        let mut all=Audio{samples:vec![],sample_rate:44100,first_audio_ms:0.,synthesis_ms:0.};
        for part in chunk_text(text,Some(120)) {
            ensure!(!cancel(),"speech interrupted");
            let audio=self.synthesize(&part,lang,cancel)?;
            if !cancel(){emit(&audio)?;}
            if all.samples.is_empty(){all.sample_rate=audio.sample_rate;all.first_audio_ms=audio.first_audio_ms;}
            all.samples.extend_from_slice(&audio.samples);all.synthesis_ms+=audio.synthesis_ms;
        }
        Ok(all)
    }
    fn stream_packets(&mut self,text:&str,lang:&str,cancel:&dyn Fn()->bool,
        emit:&mut dyn FnMut(SpeechPacket)->Result<()>)->Result<Audio> {
        emit(SpeechPacket::Sentence(text.into()))?;
        self.stream(text,lang,cancel,&mut |audio|emit(SpeechPacket::Audio(audio.clone())))
    }

}
impl Synthesizer for Supertonic {
    fn synthesize(&mut self, t: &str, l: &str, c: &dyn Fn() -> bool) -> Result<Audio> {
        self.render_cancel(t, l, c)
    }
}
#[derive(Debug)]
pub enum SpeechEvent {
    /// Audio reached the output; the stream ended, including an interruption or later failure.
    PlaybackEnded { completed_at: Instant },
    /// The job was cancelled; `heard` when some of its audio had reached the output (the user heard its start).
    Cancelled { job: u64, heard: bool },
    Complete {
        job: u64,
        completed_at: Instant,
        first_audio_ms: f64,
        real_time_factor: f64,
    },
    Failed {
        job: u64,
        error: String,
    },
}
struct Job {
    id: u64,
    epoch: u64,
    text: String,
    lang: String,
}
pub struct SpeechQueue {
    /// The speaking claim held across this queue's jobs (released by `interrupt`).
    speaking: Arc<playback::SpeakingHold>,
    sender: Option<mpsc::SyncSender<Job>>,
    epoch: Arc<AtomicU64>,
    ids: AtomicU64,
    /// Jobs accepted and not yet finished (queued, synthesizing or playing), cancelled ones included.
    pending: Arc<AtomicU64>,
    worker: Option<JoinHandle<()>>,
    pub events: mpsc::Receiver<SpeechEvent>,
}
impl SpeechQueue {
    pub fn new(synth: impl Synthesizer, output: impl AudioOutput) -> Self {
        Self::new_speaking(synth, output, playback::global_speaking(), playback::SPEAKING_LINGER)
    }
    /// The queue with its speaking flag and linger given (tests observe their own flag).
    pub(crate) fn new_speaking(mut synth: impl Synthesizer, mut output: impl AudioOutput, owner: Arc<playback::Speaking>, linger: std::time::Duration) -> Self {
        let speaking = playback::SpeakingHold::new(owner, linger);
        let worker_speaking = speaking.clone();
        let (tx, rx) = mpsc::sync_channel::<Job>(32);
        let (et, er) = mpsc::channel();
        let epoch = Arc::new(AtomicU64::new(0));
        let worker_epoch = epoch.clone();
        let pending = Arc::new(AtomicU64::new(0));
        let worker_pending = pending.clone();
        let worker = thread::spawn(move || {
            loop {
                // While the reply's claim is held, wait only until its linger runs out: a job that arrives in time
                // keeps the claim through its synthesis gap.
                let j = match worker_speaking.deadline() {
                    Some(due) => match rx.recv_timeout(due.saturating_duration_since(Instant::now())) {
                        Ok(job) => job,
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            worker_speaking.release_if_due(Instant::now());
                            continue;
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    },
                    None => match rx.recv() {
                        Ok(job) => job,
                        Err(_) => break,
                    },
                };
                // Every exit from this iteration, cancelled or not, settles the job.
                struct Settle<'a>(&'a AtomicU64);
                impl Drop for Settle<'_> {
                    fn drop(&mut self) {
                        self.0.fetch_sub(1, Ordering::AcqRel);
                    }
                }
                let _settle = Settle(&worker_pending);
                let cancel = || worker_epoch.load(Ordering::Acquire) != j.epoch;
                if cancel() {
                    let _ = et.send(SpeechEvent::Cancelled {job:j.id,heard:false});
                    continue;
                }
                let started = Instant::now();
                let mut heard = false;
                let pace = synth.pace();
                let result = std::thread::scope(|scope| {
                    let (tx,rx)=mpsc::sync_channel::<PlaybackChunk>(8);
                    let generation=Arc::new(AtomicU64::new(0));let producer_generation=generation.clone();
                    let synth=&mut synth;
                    let cancel=&cancel;
                    let producer=scope.spawn(move || -> Result<(f64,f64)> {
                        let mut first_audio=None;
                        let mut synthesis_ms=0.;
                        let mut audio_seconds=0.;
                        if cancel() {anyhow::bail!("speech interrupted");}
                        let mut label:Option<Arc<str>>=None;
                        let audio=synth.stream_packets(&j.text,&j.lang,cancel,&mut |packet| {
                            if cancel() {anyhow::bail!("speech interrupted");}
                            match packet {
                                SpeechPacket::RestartFirst=>{label=None;producer_generation.fetch_add(1,Ordering::AcqRel);Ok(())},
                                SpeechPacket::Sentence(text)=>{label=Some(text.into());Ok(())},
                                SpeechPacket::Audio(audio)=>{
                                    first_audio.get_or_insert(started.elapsed().as_secs_f64()*1000.);
                                    tx.send(PlaybackChunk {audio,generation:producer_generation.load(Ordering::Acquire),label:label.take()})
                                        .map_err(|_|anyhow::anyhow!("speech output closed"))
                                },
                            }
                        })?;
                        synthesis_ms+=audio.synthesis_ms;
                        audio_seconds+=audio.duration_s();
                        Ok((first_audio.unwrap_or(0.),synthesis_ms/1000./audio_seconds))
                    });
                    // One device stream for the whole utterance. The producer runs
                    // ahead by at most eight codec chunks, with bounded host memory.
                    let epoch=worker_epoch.clone();let expected=j.epoch;
                    let signal=Cancellation::new(move||epoch.load(Ordering::Acquire)!=expected);
                    let stream=PlaybackStream::new(rx,generation).with_speaking(worker_speaking.clone()).with_pace(pace);
                    let played=stream.playback_receipt();
                    let playback=output.output_stream(stream,cancel,signal);
                    heard=played.load(Ordering::Acquire);
                    if heard {
                        let _ = et.send(SpeechEvent::PlaybackEnded {completed_at:Instant::now()});
                    }
                    let generated=producer.join().map_err(|_|anyhow::anyhow!("speech producer panicked"))?;
                    playback?;
                    generated
                });
                if cancel() {
                    let _ = et.send(SpeechEvent::Cancelled {job:j.id,heard});
                    continue;
                }
                if result.is_ok() && !heard {
                    // voice-silent (night 1022): a reply that ends without one sample reaching the speaker is said.
                    eprintln!("voice: a reply finished without any audio reaching the speaker");
                }
                let event = match result {
                    Ok((first_audio_ms, real_time_factor)) => SpeechEvent::Complete {
                        job: j.id,
                        completed_at: Instant::now(),
                        first_audio_ms,
                        real_time_factor,
                    },
                    Err(e) => SpeechEvent::Failed {
                        job: j.id,
                        error: e.to_string(),
                    },
                };
                let _ = et.send(event);
            }
        });
        Self {
            speaking,
            sender: Some(tx),
            epoch,
            ids: AtomicU64::new(1),
            pending,
            worker: Some(worker),
            events: er,
        }
    }
    pub fn speak(&self, text: impl Into<String>, lang: &str) -> Result<u64> {
        ensure!(matches!(lang, "en" | "pl"), "unsupported language");
        let text = text.into();
        ensure!(
            !text.trim().is_empty() && text.len() < 32_768,
            "empty or excessive speech"
        );
        // Strip whole tags before punctuation-based chunking can split a tag. The rest of the punctuation stays
        // for the island's text; each voice normalizes what it says.
        let text = drama3::strip_tags(&text);
        ensure!(!drama3::normalize(&text).is_empty(), "no spoken text");
        let id = self.ids.fetch_add(1, Ordering::Relaxed);
        self.pending.fetch_add(1, Ordering::AcqRel);
        let sent = self.sender.as_ref().unwrap().try_send(Job {
            id,
            epoch: self.epoch.load(Ordering::Acquire),
            text,
            lang: lang.into(),
        });
        if let Err(e) = sent {
            self.pending.fetch_sub(1, Ordering::AcqRel);
            anyhow::bail!("speech queue unavailable: {e}");
        }
        Ok(id)
    }
    /// True while any accepted job is queued, synthesizing or playing.
    pub fn busy(&self) -> bool {
        self.pending.load(Ordering::Acquire) > 0
    }
    /// Call at the user's speech onset. Cancels playback and all previously queued jobs.
    pub fn interrupt(&self) {
        self.epoch.fetch_add(1, Ordering::AcqRel);
        self.speaking.release();
    }
}
impl Drop for SpeechQueue {
    fn drop(&mut self) {
        self.interrupt();
        self.sender.take();
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}

#[cfg(all(test, target_os = "macos"))]
mod fault_model_tests {
    use super::*;
    #[test]
    #[ignore = "requires pinned offline TTS assets"]
    fn gpu_fault_tts_recreates_falls_back_and_recovers() -> Result<()> {
        let mut model = Supertonic::load_with_backend(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../models/supertonic-3"),
            TtsBackend::Metal,
        )?;
        gpu_recovery::inject(1);
        assert!(!model.render("Hello.", "en")?.samples.is_empty());
        assert!(model.tts.is_gpu());
        gpu_recovery::inject(2);
        assert!(!model.render("Hello.", "en")?.samples.is_empty());
        assert!(!model.tts.is_gpu());
        assert_eq!(
            gpu_recovery::gpu_status(),
            Some("Voice running on CPU after a GPU error")
        );
        model.tts.retry_now();
        assert!(!model.render("Hello.", "en")?.samples.is_empty());
        assert!(model.tts.is_gpu());
        Ok(())
    }
}

#[cfg(test)]
mod queue_tests {
    use super::*;
    struct Slow;
    impl Synthesizer for Slow {
        fn synthesize(&mut self, _: &str, _: &str, cancel: &dyn Fn() -> bool) -> Result<Audio> {
            for _ in 0..200 {
                ensure!(!cancel(), "speech interrupted");
                thread::sleep(std::time::Duration::from_millis(1));
            }
            Ok(Audio { samples: vec![0.; 16], sample_rate: 16_000, first_audio_ms: 0., synthesis_ms: 1. })
        }
    }
    struct Null;
    impl AudioOutput for Null {
        fn output(&mut self, _: &Audio, _: &dyn Fn() -> bool) -> Result<()> {
            Ok(())
        }
    }
    fn settle(q: &SpeechQueue) {
        let start = Instant::now();
        while q.busy() {
            assert!(start.elapsed().as_secs() < 5, "queue never settled");
            thread::sleep(std::time::Duration::from_millis(2));
        }
    }
    #[test]
    fn failed_synthesis_without_output_never_emits_playback_ended() -> Result<()> {
        struct Fails;
        impl Synthesizer for Fails {fn synthesize(&mut self,_:&str,_:&str,_:&dyn Fn()->bool)->Result<Audio> {anyhow::bail!("injected synthesis failure")}}
        let q=SpeechQueue::new(Fails,Null);q.speak("No speech was produced.","en")?;settle(&q);
        let events:Vec<_>=q.events.try_iter().collect();
        assert!(events.iter().all(|e|!matches!(e,SpeechEvent::PlaybackEnded {..})),"{events:?}");
        assert!(matches!(events.as_slice(),[SpeechEvent::Failed {..}]));Ok(())
    }
    #[test]
    fn output_failure_before_first_sample_never_emits_playback_ended() -> Result<()> {
        struct One;
        impl Synthesizer for One {fn synthesize(&mut self,_:&str,_:&str,_:&dyn Fn()->bool)->Result<Audio> {Ok(Audio {samples:vec![0.1;16],sample_rate:16000,first_audio_ms:0.,synthesis_ms:0.})}}
        struct Fails;
        impl AudioOutput for Fails {fn output(&mut self,_:&Audio,_:&dyn Fn()->bool)->Result<()> {anyhow::bail!("output device unavailable")}}
        let q=SpeechQueue::new(One,Fails);q.speak("Not played.","en")?;settle(&q);
        assert!(q.events.try_iter().all(|e|!matches!(e,SpeechEvent::PlaybackEnded {..})));Ok(())
    }
    /// Queue tests run on real time at half the production scale (linger 30 ms, sentences 60 ms of synthesis apart,
    /// the same 2:1 ratio as 300 ms and 150 ms), so each finishes in a fraction of a second.
    const SCALED_LINGER: std::time::Duration = std::time::Duration::from_millis(30);
    const SCALED_GAP: std::time::Duration = std::time::Duration::from_millis(60);
    const SCALED_CHUNK: std::time::Duration = std::time::Duration::from_millis(40);
    /// Synthesis takes `SCALED_GAP` per sentence; one sentence is `SCALED_CHUNK` long.
    struct Gap;
    impl Synthesizer for Gap {
        fn synthesize(&mut self, _: &str, _: &str, cancel: &dyn Fn() -> bool) -> Result<Audio> {
            let start = Instant::now();
            while start.elapsed() < SCALED_GAP {
                ensure!(!cancel(), "speech interrupted");
                thread::sleep(std::time::Duration::from_millis(1));
            }
            Ok(Audio { samples: vec![0.5; 640], sample_rate: 16_000, first_audio_ms: 0., synthesis_ms: 0. })
        }
    }
    /// An output that plays each chunk for `SCALED_CHUNK` and claims the flag the way the device does.
    struct Timed {
        last_end: Arc<std::sync::Mutex<Option<Instant>>>,
    }
    impl AudioOutput for Timed {
        fn output(&mut self, _: &Audio, _: &dyn Fn() -> bool) -> Result<()> {
            Ok(())
        }
        fn output_stream(&mut self, mut chunks: PlaybackStream, cancel: &dyn Fn() -> bool, signal: Cancellation) -> Result<()> {
            let mut audibility = playback::Audibility::new(chunks.speaking());
            loop {
                if cancel() || signal.cancelled() {
                    audibility.cancelled();
                    return Ok(());
                }
                audibility.poll(Instant::now());
                match chunks.recv(std::time::Duration::from_millis(2)) {
                    Ok(_) => {
                        audibility.audible();
                        thread::sleep(SCALED_CHUNK);
                        *self.last_end.lock().unwrap() = Some(Instant::now());
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        let at = self.last_end.lock().unwrap().unwrap_or_else(Instant::now);
                        audibility.ended(at);
                        return Ok(());
                    }
                }
            }
        }
    }
    /// The flag's transitions with the instant each was emitted.
    fn flag_log() -> (Arc<playback::Speaking>, Arc<std::sync::Mutex<Vec<(bool, Instant)>>>) {
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = log.clone();
        (playback::Speaking::new(Box::new(move |on| sink.lock().unwrap().push((on, Instant::now())))), log)
    }
    fn wait_for(limit: std::time::Duration, mut done: impl FnMut() -> bool) {
        let start = Instant::now();
        while !done() {
            assert!(start.elapsed() < limit, "timed out");
            thread::sleep(std::time::Duration::from_millis(1));
        }
    }
    /// Three sentences queued back to back: one continuous speaking span through the synthesis gaps, which end
    /// `SCALED_LINGER` after the last sample of the last one.
    #[test]
    fn a_reply_of_queued_sentences_is_one_speaking_span() {
        let (owner, log) = flag_log();
        let last_end = Arc::new(std::sync::Mutex::new(None));
        let q = SpeechQueue::new_speaking(Gap, Timed { last_end: last_end.clone() }, owner, SCALED_LINGER);
        for sentence in ["First sentence.", "Second sentence.", "Third sentence."] {
            q.speak(sentence, "en").unwrap();
        }
        wait_for(std::time::Duration::from_secs(5), || log.lock().unwrap().iter().any(|(on, _)| !on));
        let log = log.lock().unwrap().clone();
        let values: Vec<bool> = log.iter().map(|(on, _)| *on).collect();
        assert_eq!(values, [true, false], "one span: no drop and regrow between sentences");
        let end = last_end.lock().unwrap().unwrap();
        let off = log[1].1;
        assert!(off >= end + SCALED_LINGER - std::time::Duration::from_millis(5), "released before the linger ran out");
        assert!(off < end + SCALED_LINGER + std::time::Duration::from_millis(100), "released late");
    }
    /// Interrupt ends a held span at once, not after the linger: the linger here is 10 s.
    #[test]
    fn interrupt_ends_a_held_span_at_once() {
        let (owner, log) = flag_log();
        let q = SpeechQueue::new_speaking(Gap, Timed { last_end: Default::default() }, owner, std::time::Duration::from_secs(10));
        for sentence in ["First sentence.", "Second sentence.", "Third sentence."] {
            q.speak(sentence, "en").unwrap();
        }
        wait_for(std::time::Duration::from_secs(5), || log.lock().unwrap().iter().any(|(on, _)| *on));
        thread::sleep(std::time::Duration::from_millis(80));
        let interrupted = Instant::now();
        q.interrupt();
        wait_for(std::time::Duration::from_secs(1), || log.lock().unwrap().iter().any(|(on, _)| !on));
        let log = log.lock().unwrap().clone();
        assert_eq!(log.iter().map(|(on, _)| *on).collect::<Vec<_>>(), [true, false]);
        assert!(log[1].1 - interrupted < std::time::Duration::from_millis(100), "the flag fell at once");
        settle(&q);
    }
    /// Two Speed releases, the first sample still waiting for Jarvis, and both played. The second release
    /// (`interrupt` then `speak`, as the app's sample does) cancels the waiting one; only the last sample plays.
    #[test]
    fn a_newer_release_cancels_a_sample_still_waiting_for_its_voice() -> Result<()> {
        struct Waits(Arc<std::sync::atomic::AtomicBool>);
        impl Synthesizer for Waits {
            fn synthesize(&mut self, t: &str, _: &str, cancel: &dyn Fn() -> bool) -> Result<Audio> {
                let start = Instant::now();
                while !self.0.load(Ordering::Acquire) {
                    ensure!(!cancel(), "speech interrupted");
                    ensure!(start.elapsed().as_secs() < 5, "voice never loaded");
                    thread::sleep(std::time::Duration::from_millis(1));
                }
                let marker = if t.starts_with("first") { 0.1 } else { 0.9 };
                Ok(Audio { samples: vec![marker; 16], sample_rate: 16_000, first_audio_ms: 0., synthesis_ms: 0. })
            }
        }
        struct Record(Arc<std::sync::Mutex<Vec<f32>>>);
        impl AudioOutput for Record {
            fn output(&mut self, a: &Audio, cancel: &dyn Fn() -> bool) -> Result<()> {
                if !cancel() {
                    self.0.lock().unwrap().push(a.samples[0]);
                }
                Ok(())
            }
        }
        let gate = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let played = Arc::new(std::sync::Mutex::new(Vec::new()));
        let q = SpeechQueue::new(Waits(gate.clone()), Record(played.clone()));
        q.speak("first speed sample", "en")?;
        thread::sleep(std::time::Duration::from_millis(20)); // the first sample is waiting for its voice
        q.interrupt();
        q.speak("second speed sample", "en")?;
        gate.store(true, Ordering::Release);
        settle(&q);
        assert_eq!(*played.lock().unwrap(), [0.9], "only the last sample plays");
        Ok(())
    }
    #[test]
    fn first_sentence_restart_cuts_playing_and_queued_prefix_before_complete_retry() -> Result<()> {
        use std::sync::atomic::AtomicBool;
        let heard=Arc::new(AtomicBool::new(false));let cut=Arc::new(AtomicBool::new(false));
        let log=Arc::new(std::sync::Mutex::new(Vec::new()));
        struct Restart {heard:Arc<AtomicBool>,cut:Arc<AtomicBool>}
        impl Synthesizer for Restart {
            fn synthesize(&mut self,_:&str,_:&str,_:&dyn Fn()->bool)->Result<Audio>{anyhow::bail!("buffered path")}
            fn stream_packets(&mut self,_:&str,_:&str,_:&dyn Fn()->bool,emit:&mut dyn FnMut(SpeechPacket)->Result<()>)->Result<Audio>{
                let audio=|value|Audio {samples:vec![value;4],sample_rate:44100,first_audio_ms:0.,synthesis_ms:0.};
                emit(SpeechPacket::Audio(audio(0.1)))?;
                let deadline=Instant::now()+std::time::Duration::from_secs(2);
                while !self.heard.load(Ordering::Acquire){ensure!(Instant::now()<deadline,"prefix not playing");thread::sleep(std::time::Duration::from_millis(1));}
                emit(SpeechPacket::Audio(audio(0.2)))?; // Old queued tail must be discarded.
                emit(SpeechPacket::RestartFirst)?;
                while !self.cut.load(Ordering::Acquire){ensure!(Instant::now()<deadline,"prefix did not stop");thread::sleep(std::time::Duration::from_millis(1));}
                let accepted=audio(0.8);emit(SpeechPacket::Audio(accepted.clone()))?;Ok(accepted)
            }
        }
        struct Sink {heard:Arc<AtomicBool>,cut:Arc<AtomicBool>,log:Arc<std::sync::Mutex<Vec<f32>>>}
        impl AudioOutput for Sink {
            fn output(&mut self,a:&Audio,cancel:&dyn Fn()->bool)->Result<()> {
                self.log.lock().unwrap().push(a.samples[0]);
                if a.samples[0]==0.1 {
                    self.heard.store(true,Ordering::Release);let deadline=Instant::now()+std::time::Duration::from_secs(2);
                    while !cancel(){ensure!(Instant::now()<deadline,"playing prefix was not cut");thread::sleep(std::time::Duration::from_millis(1));}
                    self.cut.store(true,Ordering::Release);
                }
                Ok(())
            }
        }
        let q=SpeechQueue::new(Restart {heard:heard.clone(),cut:cut.clone()},Sink {heard,cut,log:log.clone()});
        q.speak("First. Next.","en")?;settle(&q);
        assert_eq!(*log.lock().unwrap(),[0.1,0.8]);
        assert!(matches!(q.events.try_recv()?,SpeechEvent::PlaybackEnded {..}));
        assert!(matches!(q.events.try_recv()?,SpeechEvent::Complete {..}));
        Ok(())
    }
    #[test]
    fn streaming_reaches_output_before_synthesis_completes() -> Result<()> {
        use std::sync::atomic::AtomicBool;
        struct Streaming(Arc<AtomicBool>);
        impl Synthesizer for Streaming {
            fn synthesize(&mut self,_:&str,_:&str,_:&dyn Fn()->bool)->Result<Audio> {anyhow::bail!("buffered path was used")}
            fn stream(&mut self,text:&str,_:&str,c:&dyn Fn()->bool,emit:&mut dyn FnMut(&Audio)->Result<()>)->Result<Audio> {
                assert_eq!(text,"Okay.");
                let audio=Audio {samples:vec![0.1;100],sample_rate:16000,first_audio_ms:1.,synthesis_ms:2.};
                emit(&audio)?;
                let deadline=Instant::now()+std::time::Duration::from_secs(2);
                while !self.0.load(Ordering::Acquire) {ensure!(!c() && Instant::now()<deadline,"first chunk was buffered");thread::sleep(std::time::Duration::from_millis(1));}
                emit(&audio)?;Ok(audio)
            }
        }
        struct Sink(Arc<AtomicBool>);
        impl AudioOutput for Sink {
            fn output(&mut self,_:&Audio,_:&dyn Fn()->bool)->Result<()> {self.0.store(true,Ordering::Release);Ok(())}
        }
        let heard=Arc::new(AtomicBool::new(false));let q=SpeechQueue::new(Streaming(heard.clone()),Sink(heard.clone()));
        q.speak("[calm, slow] Okay.","en")?;settle(&q);assert!(heard.load(Ordering::Acquire));
        assert!(matches!(q.events.try_recv()?,SpeechEvent::PlaybackEnded {..}));
        assert!(matches!(q.events.try_recv()?,SpeechEvent::Complete {..}));Ok(())
    }
    #[test]
    fn busy_covers_queued_and_running_jobs_and_clears_when_finished_or_interrupted() -> Result<()> {
        let q = SpeechQueue::new(Slow, Null);
        assert!(!q.busy());
        q.speak("one", "en")?;
        q.speak("two", "en")?;
        assert!(q.busy());
        settle(&q);
        q.speak("three", "en")?;
        q.interrupt();
        settle(&q);
        Ok(())
    }
    /// Night review 20 Oct: an output that may not start yet (the app's hold while the user talks) takes the producer's
    /// chunks early, so a producer far ahead of the bounded channel never blocks; the early chunks play first, in
    /// order, and those a restarted first sentence replaced never play.
    #[test]
    fn chunks_taken_early_never_block_the_producer_and_play_first_in_order() -> Result<()> {
        let (tx,rx)=mpsc::sync_channel::<PlaybackChunk>(8);
        let generation=Arc::new(AtomicU64::new(0));
        let chunk=|n:usize,generation:u64|PlaybackChunk{audio:Audio{samples:vec![n as f32;4],sample_rate:44_100,first_audio_ms:0.,synthesis_ms:0.},generation,label:None};
        let producer_generation=generation.clone();
        let producer=thread::spawn(move || -> Result<std::time::Duration> {
            let began=Instant::now();
            for n in 0..40 { tx.send(chunk(n,0)).map_err(|_|anyhow::anyhow!("closed"))?; }
            // A restarted first sentence: what was sent so far is stale.
            producer_generation.fetch_add(1,Ordering::AcqRel);
            for n in 100..140 { tx.send(chunk(n,1)).map_err(|_|anyhow::anyhow!("closed"))?; }
            Ok(began.elapsed())
        });
        let mut stream=PlaybackStream::new(rx,generation);
        let began=Instant::now();
        while stream.take_ready(std::time::Duration::from_millis(5)) { ensure!(began.elapsed()<std::time::Duration::from_secs(2),"producer never finished"); }
        let sent=producer.join().unwrap()?;
        assert!(sent<std::time::Duration::from_millis(500),"the producer waited on the held output: {sent:?}");
        assert!(stream.has_audio());
        let mut heard=vec![];
        while let Ok(chunk)=stream.recv(std::time::Duration::from_millis(5)) { heard.push(chunk.audio.samples[0] as usize); }
        assert_eq!(heard,(100..140).collect::<Vec<_>>(),"early chunks first, in order, the replaced ones dropped");
        assert!(!stream.has_audio());
        Ok(())
    }
}
