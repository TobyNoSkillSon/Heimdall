//! Agent-operated resident A/B probe. JSON lines in/out; private audio stays in place.
use anyhow::{Context, Result};
use listen::{AudioChunk, ListenConfig, ListenEngine, ListenEvent, Recognizer, Transcript};
use serde::Deserialize;
use std::{
    cell::RefCell,
    io::{self, BufRead, Write},
    path::PathBuf,
    rc::Rc,
    time::{Duration, Instant},
};
#[derive(Default)]
struct Stats {
    calls: usize,
    seconds: f64,
    speech_samples: usize,
    vad_max: f32,
}
struct Counted<R> {
    inner: R,
    stats: Rc<RefCell<Stats>>,
}
impl Recognizer for Counted<listen::asr::Parakeet> {
    fn transcribe_batch(&mut self, inputs: &[(&[f32], u64)]) -> Vec<Result<Transcript>> {
        let calls = self.inner.encoder_calls();
        let start = Instant::now();
        let result = self.inner.transcribe_batch(inputs);
        let mut stats = self.stats.borrow_mut();
        stats.calls += (self.inner.encoder_calls() - calls) as usize;
        stats.seconds += start.elapsed().as_secs_f64();
        result
    }
    fn transcribe(&mut self, a: &[f32], ns: u64) -> Result<Transcript> {
        let calls = self.inner.encoder_calls();
        let start = Instant::now();
        let result = self.inner.transcribe(a, ns);
        let mut stats = self.stats.borrow_mut();
        stats.calls += (self.inner.encoder_calls() - calls) as usize;
        stats.seconds += start.elapsed().as_secs_f64();
        result
    }
}
impl<V: listen::SpeechDetector> listen::SpeechDetector for Counted<V> {
    fn probability(&mut self, a: &[f32; 512], state: &mut listen::onnx::VadState) -> Result<f32> {
        let p = self.inner.probability(a, state)?;
        let mut s = self.stats.borrow_mut();
        if p >= 0.35 {
            s.speech_samples += 512;
        }
        s.vad_max = s.vad_max.max(p);
        Ok(p)
    }
}
#[derive(Deserialize)]
struct Request {
    #[serde(default = "one")]
    count: usize,
    #[serde(default)]
    batch_paths: Vec<PathBuf>,
    path: Option<PathBuf>,
    #[serde(default)]
    pcm_paths: Vec<PathBuf>,
    #[serde(default)]
    whole: bool,
    #[serde(default)]
    responsive: bool,
    #[serde(default)]
    realtime: bool,
    #[serde(default)]
    seconds: f64,
    #[serde(default = "mic")]
    source: String,
}
fn one() -> usize {
    1
}
fn mic() -> String {
    "mic".into()
}
#[cfg(target_os = "macos")]
fn main() -> Result<()> {
    fn cpu() -> f64 {
        let mut r: libc::rusage = unsafe { std::mem::zeroed() };
        unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut r); }
        r.ru_utime.tv_sec as f64 + r.ru_utime.tv_usec as f64/1e6 + r.ru_stime.tv_sec as f64 + r.ru_stime.tv_usec as f64/1e6
    }
    let models = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../models");
    let stats = Rc::new(RefCell::new(Stats::default()));
    let mut asr = Counted {
        inner: listen::asr::Parakeet::load(&models.join("parakeet-ultra"))?,
        stats: stats.clone(),
    };
    let mut vad = Counted {
        inner: listen::onnx::Silero::load(&models.join("silero/silero_vad.onnx"))?,
        stats: stats.clone(),
    };
    let policy = models.join("smart-turn/owner-policy.json");
    let mut config=ListenConfig::default();
    let turn_path=if policy.exists(){
        let p:serde_json::Value=serde_json::from_slice(&std::fs::read(policy)?)?;
        config.pause_samples=p["minimum_silence_ms"].as_u64().context("turn silence")? as usize*16;
        config.maximum_silence_samples=40000;
        config.complete_threshold=p["threshold"].as_f64().context("turn threshold")? as f32;
        models.join("smart-turn/owner-turn.onnx")
    }else{models.join("smart-turn/smart-turn-v3.2-cpu.onnx")};
    let mut turn=listen::onnx::SmartTurn::load(&turn_path)?;
    println!("{{\"ready\":true}}");
    io::stdout().flush()?;
    for line in io::stdin().lock().lines() {
        let req: Request = serde_json::from_str(&line?)?;
        if !req.batch_paths.is_empty() {
            let audio = req
                .batch_paths
                .iter()
                .map(|p| listen::file::read_wav(p))
                .collect::<Result<Vec<_>>>()?;
            let inputs: Vec<_> = audio.iter().map(|a| (a.as_slice(), 0)).collect();
            let start = Instant::now();
            let results = asr
                .transcribe_batch(&inputs)
                .into_iter()
                .collect::<Result<Vec<_>>>()?;
            println!(
                "{}",
                serde_json::json!({"backend":asr.inner.backend(),"wall_seconds":start.elapsed().as_secs_f64(),"streams":results.len(),"transcripts":results})
            );
            io::stdout().flush()?;
            continue;
        }
        if req.path.is_none() && req.pcm_paths.is_empty() {
            std::thread::sleep(Duration::from_secs_f64(req.seconds));
            println!("{{\"idle\":true}}");
            io::stdout().flush()?;
            continue;
        }
        let mut audio = if let Some(path) = req.path {
            listen::file::read_wav(&path)?
        } else {
            let mut audio = vec![];
            for path in req.pcm_paths {
                let b = std::fs::read(path)?;
                anyhow::ensure!(b.len() % 4 == 0, "PCM alignment");
                audio.extend(b.as_chunks::<4>().0.iter().map(|x| f32::from_le_bytes(*x)));
            }
            audio
        };
        if req.seconds > 0.0 {
            audio.truncate((req.seconds * 16000.0) as usize);
        }
        *stats.borrow_mut() = Stats::default();
        let cpu_start = cpu();
        let start = Instant::now();
        let mut first = None;
        let mut onset = None;
        let mut texts = vec![];
        let mut partials = 0;
        let mut transcript_words = vec![];
        if req.whole {
            anyhow::ensure!((1..=128).contains(&req.count), "probe repeat count");
            for _ in 0..req.count {
                let result = asr.transcribe(&audio, 0)?;
                if texts.is_empty() {
                    texts.push(result.text);
                    transcript_words = result.words;
                }
            }
        } else {
            let mut engine =
                ListenEngine::new(&mut asr, &mut vad, &mut turn, config.clone())?;
            if req.responsive { engine.set_source_policy(&req.source, listen::PartialPolicy::Responsive)?; }
            for (i, chunk) in audio.chunks(320).enumerate() {
                if req.realtime {
                    let target = Duration::from_secs_f64(i as f64 * 0.020);
                    if target > start.elapsed() {
                        std::thread::sleep(target - start.elapsed());
                    }
                }
                let events = engine.push(AudioChunk {
                    source: &req.source,
                    generation: 0,
                    host_time_ns: i as u64 * 20_000_000,
                    samples: chunk,
                })?;
                for e in events {
                    match e {
                        ListenEvent::SpeechStart { .. } => {
                            onset.get_or_insert((i + 1) as f64 * 0.020);
                        }
                        ListenEvent::PartialTranscript { text, .. } => {
                            partials += 1;
                            if !text.is_empty() {
                                first.get_or_insert((
                                    start.elapsed().as_secs_f64(),
                                    (i + 1) as f64 * 0.020,
                                ));
                            }
                        }
                        ListenEvent::FinalUtterance { text, .. }
                        | ListenEvent::TranscriptSegment { text, .. } => texts.push(text),
                        _ => {}
                    }
                }
            }
            for e in engine.flush_source(&req.source)? {
                if let ListenEvent::FinalUtterance { text, .. }
                | ListenEvent::TranscriptSegment { text, .. } = e
                {
                    texts.push(text);
                }
            }
        }
        let wall = start.elapsed().as_secs_f64();
        let s = stats.borrow();
        println!(
            "{}",
            serde_json::json!({"cpu_seconds":cpu()-cpu_start,"policy":if req.responsive{"responsive"}else{"final_only"},"backend":asr.inner.backend(),"count":if req.whole {req.count}else{1},"audio_seconds":audio.len() as f64/16000.0,"wall_seconds":wall,"asr_seconds":s.seconds,"encoder_calls":s.calls,"asr_share":s.seconds/wall,"first_partial_wall":first.map(|v|v.0),"first_partial_audio":first.map(|v|v.1),"onset_audio":onset,"speech_seconds":s.speech_samples as f64/16000.0,"vad_max":s.vad_max,"partials":partials,"text":texts.join(" "),"words":transcript_words})
        );
        io::stdout().flush().context("probe stdout")?;
    }
    Ok(())
}
#[cfg(not(target_os = "macos"))]
fn main() -> Result<()> {
    anyhow::bail!("Core ML probe requires macOS")
}
