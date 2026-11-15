use crate::{
    asr::{reconcile, Recognizer, Transcript, Word},
    onnx::{Silero, SmartTurn, VadState},
    onset::VoicedHistory,
};
use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, VecDeque},
    path::Path,
};
/// Nanoseconds in the capture host-clock domain, *not* UNIX time. Generation
/// changes on source restart. Samples are consecutive mono PCM at exactly 16 kHz.
pub struct AudioChunk<'a> {
    pub source: &'a str,
    pub generation: u64,
    pub host_time_ns: u64,
    pub samples: &'a [f32],
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum ListenEvent {
    SpeechStart {
        source: String,
        host_time_ns: u64,
    },
    SpeechEnd {
        source: String,
        host_time_ns: u64,
    },
    PartialTranscript {
        source: String,
        text: String,
        words: Vec<Word>,
        stable_words: usize,
    },
    FinalUtterance {
        source: String,
        text: String,
        words: Vec<Word>,
        start_ns: u64,
        end_ns: u64,
        end_of_turn_probability: f32,
        forced: bool,
    },
    /// Earlier, non-overlapping words from an ongoing turn. Append once; the
    /// partial/final payload now contains only the bounded tail. Not a turn end.
    TranscriptSegment {
        source: String,
        text: String,
        words: Vec<Word>,
        start_ns: u64,
        end_ns: u64,
    },
    Discontinuity {
        source: String,
        host_time_ns: u64,
    },
}
/// Policies are keyed by capture source ID. Capture's `mic:` sources (and the
/// offline `mic` source) default final-only, like app audio. Responsive is an explicit lab override.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartialPolicy {
    Responsive,
    FinalOnly,
}
#[derive(Debug, Clone)]
pub struct ListenConfig {
    pub vad_start: f32,
    pub vad_continue: f32,
    pub pause_samples: usize,
    pub maximum_silence_samples: usize,
    pub partial_interval_samples: usize,
    pub complete_threshold: f32,
    pub first_partial_samples: usize,
    pub maximum_partial_interval_samples: usize,
    pub source_policies: HashMap<String, PartialPolicy>,
    pub maximum_sources: usize,
    pub maximum_chunk_samples: usize,
    /// Silero probability at which a block counts as speech for word onsets (`onset::VoicedHistory`). Every decode's
    /// word starts move forward to speech; 0.5 is the value measured in lab/screen-phrase-bench.
    pub onset_threshold: f32,
    /// The user's microphones (sources starting with `mic`, the final-only convention of `policy`, and the phone's
    /// `phone:` microphone): the Smart Turn
    /// checks of one pause, as (silence samples, threshold), ascending. Each asks once the pause reaches its length
    /// and ends the turn when the probability exceeds its threshold; the first also reports `SpeechEnd`.
    /// `maximum_silence_samples` stays the ceiling. Empty: these sources use the one check below like every other.
    pub mic_turn_checks: Vec<(usize, f32)>,
}
/// The user's tuned turn model (`models/smart-turn/owner-turn.onnx`) on his microphones: when Smart Turn is asked during a
/// pause, and the bar it must clear, as (silence ms, threshold); the last bar is the policy's own `threshold` (.5).
/// Before 11 Nov one check at `minimum_silence_ms` (1500) was the only one, a fixed 1.5 s wait before every turn could
/// end. The steps come from the model's validation sweep (`lab/eval/data/tune/turn-training.json`, 382 recordings,
/// pauses under 1 s the hard "not done" examples): at 300 ms over .95 ended no short pause and 9% of finished turns; at
/// 1 s over .95 ends 33% (the sweep has no hard examples of 1–1.5 s pauses, and at 600 ms .9 still ended 48 of 382
/// mid-sentence pauses, so the bar stays at the one that ended none); 1.5 s over .5 is the old check. A pause that clears no bar ends at the ceiling (`long_silence_ms`, 2.5 s), as before.
pub const TUNED_TURN_CHECKS: [(u64, f32); 2] = [(300, 0.95), (1000, 0.95)];
impl ListenConfig {
    /// The user's turn policy (`owner-policy.json`): every source keeps its one check at `minimum_silence_ms` over
    /// `threshold`, ceiling `long_silence_ms`; his microphones are asked earlier too (`TUNED_TURN_CHECKS`).
    pub fn apply_tuned_policy(&mut self, p: &serde_json::Value) -> Result<()> {
        let ms = |key: &str| p[key].as_u64().map(|v| v as usize * 16).ok_or_else(|| anyhow::anyhow!("missing {key}"));
        self.pause_samples = ms("minimum_silence_ms")?;
        self.maximum_silence_samples = ms("long_silence_ms").unwrap_or(40000);
        self.complete_threshold = p["threshold"].as_f64().ok_or_else(|| anyhow::anyhow!("missing turn threshold"))? as f32;
        self.mic_turn_checks = TUNED_TURN_CHECKS.iter().map(|&(ms, bar)| (ms as usize * 16, bar)).filter(|&(n, _)| n < self.pause_samples).collect();
        self.mic_turn_checks.push((self.pause_samples, self.complete_threshold));
        Ok(())
    }
    /// The `n`th Smart Turn check of one pause on `source`, if there is one.
    fn turn_check(&self, source: &str, n: usize) -> Option<(usize, f32)> {
        // His phone's microphone (`phone:<kind>:<generation>`, the app's live phone mic) is his voice too.
        if (source.starts_with("mic") || source.starts_with("phone:")) && !self.mic_turn_checks.is_empty() {
            self.mic_turn_checks.get(n).copied()
        } else {
            (n == 0).then_some((self.pause_samples, self.complete_threshold))
        }
    }
}
impl Default for ListenConfig {
    fn default() -> Self {
        Self {
            vad_start: 0.4,
            vad_continue: 0.35,
            pause_samples: 4800,
            maximum_silence_samples: 24000,
            partial_interval_samples: 40000,
            complete_threshold: 0.5,
            first_partial_samples: 6144,
            maximum_partial_interval_samples: 48000,
            source_policies: HashMap::new(),
            maximum_sources: 32,
            maximum_chunk_samples: 16000,
            onset_threshold: 0.5,
            mic_turn_checks: vec![],
        }
    }
}
pub trait SpeechDetector {
    fn probability(&mut self, audio: &[f32; 512], state: &mut VadState) -> Result<f32>;
}
impl SpeechDetector for Silero {
    fn probability(&mut self, a: &[f32; 512], s: &mut VadState) -> Result<f32> {
        self.probability(a, s)
    }
}
pub trait TurnDetector {
    fn probability(&mut self, audio: &[f32]) -> Result<f32>;
}
impl TurnDetector for SmartTurn {
    fn probability(&mut self, a: &[f32]) -> Result<f32> {
        self.probability(a)
    }
}
impl<V: SpeechDetector + ?Sized> SpeechDetector for &mut V {
    fn probability(&mut self, a: &[f32; 512], s: &mut VadState) -> Result<f32> {
        (**self).probability(a, s)
    }
}
impl<T: TurnDetector + ?Sized> TurnDetector for &mut T {
    fn probability(&mut self, a: &[f32]) -> Result<f32> {
        (**self).probability(a)
    }
}
struct Source {
    faulted: bool,
    generation: u64,
    next_ns: u64,
    pending_ns: u64,
    pending: VecDeque<f32>,
    vad: VadState,
    pre: VecDeque<f32>,
    active: bool,
    voicing: bool,
    turn_audio: VecDeque<f32>,
    audio: Vec<f32>,
    window_ns: u64,
    start_ns: u64,
    last_voice_ns: u64,
    silence: usize,
    since_partial: usize,
    partial_count: usize,
    decode_revision: u64,
    /// Smart Turn checks already made in this pause (`ListenConfig::turn_check`).
    checks_done: usize,
    probability: f32,
    words: Vec<Word>,
    agreement: Vec<usize>,
    stable: usize,
    /// Every VAD block of this source and generation, for word onsets.
    voiced: VoicedHistory,
    /// A window roll sent words of this utterance as a `TranscriptSegment`: its final must be emitted even with no
    /// words left, or the consumer keeps those words open until the next utterance (bug note 2026-11-14T11-58-47).
    segmented: bool,
}
impl Source {
    fn new(generation: u64, ns: u64) -> Self {
        Self {
            faulted: false,
            generation,
            next_ns: ns,
            pending_ns: ns,
            pending: VecDeque::new(),
            vad: VadState::default(),
            pre: VecDeque::new(),
            active: false,
            voicing: false,
            turn_audio: VecDeque::new(),
            audio: vec![],
            window_ns: ns,
            start_ns: ns,
            last_voice_ns: ns,
            silence: 0,
            since_partial: 0,
            partial_count: 0,
            decode_revision: 0,
            checks_done: 0,
            probability: 0.0,
            words: vec![],
            agreement: vec![],
            stable: 0,
            voiced: VoicedHistory::default(),
            segmented: false,
        }
    }
}
pub type EvidenceHook = Box<dyn FnMut(&str, u64, serde_json::Value) -> Result<()> + Send>;
pub struct ListenEngine<R, V = Silero, T = SmartTurn> {
    evidence: Option<EvidenceHook>,
    asr: R,
    vad: V,
    turn: T,
    config: ListenConfig,
    sources: HashMap<String, Source>,
    batch_partial_sources: std::collections::HashSet<String>,
    deferred_partials: Vec<(String, Option<bool>)>,
}
/// The turn model under `models` and the listening config that goes with it: the user's tuned model and its policy
/// (`smart-turn/owner-policy.json`) when the policy is there, else the stock CPU model and the defaults.
pub fn turn_model(models: &Path) -> Result<(std::path::PathBuf, ListenConfig)> {
    let mut config = ListenConfig::default();
    let policy = models.join("smart-turn/owner-policy.json");
    if !policy.exists() {
        return Ok((models.join("smart-turn/smart-turn-v3.2-cpu.onnx"), config));
    }
    let p: serde_json::Value = serde_json::from_slice(&std::fs::read(policy)?)?;
    config.apply_tuned_policy(&p)?;
    Ok((models.join("smart-turn/owner-turn.onnx"), config))
}
impl<R: Recognizer> ListenEngine<R> {
    pub fn load(models: &Path, asr: R) -> Result<Self> {
        Self::load_with_turn_backend(models, asr, None)
    }
    pub fn load_with_turn_backend(models: &Path, asr: R, backend: Option<&str>) -> Result<Self> {
        let (turn_path, config) = turn_model(models)?;
        Self::new(
            asr,
            Silero::load(&models.join("silero/silero_vad.onnx"))?,
            match backend {
                // A model without its Metal data (the stock CPU model, a fresh install) listens on CPU.
                Some("metal") => SmartTurn::load_backend(&turn_path, "metal").or_else(|e| {
                    eprintln!("Smart Turn: {e:#}; using CPU");
                    SmartTurn::load_backend(&turn_path, "cpu")
                })?,
                Some(b) => SmartTurn::load_backend(&turn_path, b)?,
                None => SmartTurn::load(&turn_path)?,
            },
            config,
        )
    }
}
impl<R: Recognizer, V: SpeechDetector, T: TurnDetector> ListenEngine<R, V, T> {
    /// Retain every source's PCM, VAD state, transcript and timestamps. Failed construction
    /// never reaches this method, leaving the current instance usable.
    pub fn replace_models(&mut self, next: Self) {
        self.asr = next.asr;
        self.vad = next.vad;
        self.turn = next.turn;
    }
    pub fn replace_recognizer(&mut self, next:R) { self.asr=next; }
    /// A one-off decode of `audio` (at most one 15-s window) that touches no source state: the
    /// spoken-command side transcript. Nothing it returns enters the transcript stream.
    pub fn transcribe_side(&mut self, audio: &[f32], start_ns: u64) -> Result<Transcript> {
        anyhow::ensure!(!audio.is_empty() && audio.len() <= 240_000, "side transcript audio must be 1..240000 samples");
        self.asr.transcribe(audio, start_ns)
    }
    pub fn replace_turn_detector(&mut self, next:T) { self.turn=next; }
    pub fn new(asr: R, vad: V, turn: T, config: ListenConfig) -> Result<Self> {
        ensure!(
            (0.0..=1.0).contains(&config.vad_start)
                && (0.0..=1.0).contains(&config.vad_continue)
                && (0.0..=1.0).contains(&config.complete_threshold)
                && (0.0..=1.0).contains(&config.onset_threshold)
                && config.mic_turn_checks.iter().all(|(_, t)| (0.0..=1.0).contains(t)),
            "Invalid threshold"
        );
        ensure!(
            config.mic_turn_checks.iter().try_fold(0, |at, &(n, _)| (n > at).then_some(n)).is_some()
                && config.mic_turn_checks.last().is_none_or(|&(n, _)| n <= config.maximum_silence_samples),
            "Invalid turn checks"
        );
        ensure!(
            config.pause_samples > 0
                && config.maximum_silence_samples >= config.pause_samples
                && config.partial_interval_samples >= 512
                && config.first_partial_samples >= 512
                && config.maximum_partial_interval_samples >= config.partial_interval_samples
                && config.maximum_sources > 0
                && config.maximum_chunk_samples > 0
                && config.source_policies.len() <= config.maximum_sources
                && config.source_policies.keys().all(|id| id.len() <= 256),
            "Invalid endpoint/cadence"
        );
        Ok(Self {
            evidence: None,
            asr,
            vad,
            turn,
            config,
            sources: HashMap::new(),
            batch_partial_sources: Default::default(),
            deferred_partials: vec![],
        })
    }
    /// Persist model results independently of UI event coalescing.
    pub fn set_evidence_hook(&mut self, hook: EvidenceHook) {
        self.evidence = Some(hook);
    }
    fn evidence(&mut self, source: &str, ns: u64, payload: serde_json::Value) -> Result<()> {
        if let Some(hook) = &mut self.evidence {
            hook(source, ns, payload)?;
        }
        Ok(())
    }
    fn turn_probability(&mut self, source: &str, s: &mut Source) -> Result<f32> {
        let n = s.turn_audio.len();
        let p = self.turn.probability(s.turn_audio.make_contiguous())?;
        self.evidence(source,s.pending_ns,serde_json::json!({"model":"smart_turn","input":{"source":source,"start_ns":s.pending_ns.saturating_sub(n as u64*62500),"end_ns":s.pending_ns,"rate":16000,"samples":n},"probability":p,"utterance_start_ns":s.start_ns}))?;
        Ok(p)
    }
    pub fn set_source_policy(&mut self, source: &str, policy: PartialPolicy) -> Result<()> {
        ensure!(source.len() <= 256, "Source ID exceeds 256 bytes");
        ensure!(
            self.config.source_policies.contains_key(source)
                || self.config.source_policies.len() < self.config.maximum_sources,
            "Source policy limit"
        );
        self.config.source_policies.insert(source.into(), policy);
        Ok(())
    }
    fn policy(&self, source: &str) -> PartialPolicy {
        self.config
            .source_policies
            .get(source)
            .copied()
            .unwrap_or(PartialPolicy::FinalOnly)
    }
    /// Synchronous offline/core inference. Live capture uses ListenWorker's
    /// bounded nonblocking push; never call this on an audio callback.
    pub fn push(&mut self, chunk: AudioChunk<'_>) -> Result<Vec<ListenEvent>> {
        ensure!(chunk.source.len() <= 256, "Source ID exceeds 256 bytes");
        ensure!(
            chunk.samples.len() <= self.config.maximum_chunk_samples,
            "Audio chunk exceeds configured bound"
        );
        ensure!(
            self.sources.contains_key(chunk.source)
                || self.sources.len() < self.config.maximum_sources,
            "Source limit"
        );
        ensure!(
            chunk.samples.iter().all(|v| v.is_finite()),
            "Non-finite audio samples"
        );
        let mut s = self
            .sources
            .remove(chunk.source)
            .unwrap_or_else(|| Source::new(chunk.generation, chunk.host_time_ns));
        let result = self.push_source(&chunk, &mut s);
        // Inference may already have consumed samples/state. Never silently
        // concatenate the next arrival with that partially processed utterance.
        s.faulted |= result.is_err();
        self.sources.insert(chunk.source.into(), s);
        result
    }
    /// Independent arrivals of several streams, each pushed in order. Single-frame, unique-source arrivals can
    /// share ready partial/final decodes; seams and multi-frame intake keep their immediate path.
    pub fn push_batch(&mut self, chunks: &[AudioChunk<'_>]) -> Vec<Result<Vec<ListenEvent>>> {
        let mut ids = HashMap::new();
        for c in chunks {
            *ids.entry(c.source).or_insert(0) += 1;
        }
        let mut indices = vec![];
        for (i, c) in chunks.iter().enumerate() {
            if ids[c.source] != 1
                || c.source.len() > 256
                || c.samples.len() > self.config.maximum_chunk_samples
                || !c.samples.iter().all(|v| v.is_finite())
            {
                continue;
            }
            let source = self.sources.get(c.source);
            if source.is_some_and(|s| {
                s.faulted
                    || s.generation != c.generation
                    || s.next_ns.abs_diff(c.host_time_ns) > 125000
            }) {
                continue;
            }
            let pending = source.map_or(0, |s| s.pending.len());
            let count = pending + c.samples.len();
            if !(512..1024).contains(&count) {
                continue;
            }
            indices.push(i);
        }
        self.batch_partial_sources = indices
            .iter()
            .map(|i| chunks[*i].source.to_string())
            .collect();
        let mut results: Vec<_> = chunks
            .iter()
            .map(|c| {
                self.push(AudioChunk {
                    source: c.source,
                    generation: c.generation,
                    host_time_ns: c.host_time_ns,
                    samples: c.samples,
                })
            })
            .collect();
        self.batch_partial_sources.clear();
        let names = std::mem::take(&mut self.deferred_partials);
        let mut states: Vec<_> = names
            .into_iter()
            .filter_map(|(id, final_)| self.sources.remove(&id).map(|s| (id, s, final_)))
            .collect();
        let inputs: Vec<_> = states
            .iter()
            .map(|(_, s, _)| (s.audio.as_slice(), s.window_ns))
            .collect();
        let mut transcripts = self.asr.transcribe_batch(&inputs);
        if transcripts.len() != states.len() {
            transcripts = states
                .iter()
                .map(|_| Err(anyhow::anyhow!("Recognizer batch result count")))
                .collect();
        }
        for ((id, mut s, final_), result) in states.drain(..).zip(transcripts) {
            let index = chunks.iter().position(|c| c.source == id).unwrap();
            let applied = match (&mut results[index], result) {
                (Ok(events), Ok(t)) => self
                    .apply_decode(&id, &mut s, final_.is_some(), events, t)
                    .map(|()| {
                        if let Some(forced) = final_ {
                            self.complete_finish(&id, &mut s, forced, events, true);
                        }
                    }),
                (_, Err(e)) => Err(e),
                (Err(_), _) => Err(anyhow::anyhow!("Source failed before batched partial")),
            };
            if let Err(e) = applied {
                s.faulted = true;
                results[index] = Err(e);
            }
            self.sources.insert(id, s);
        }
        results
    }
    fn push_source(
        &mut self,
        chunk: &AudioChunk<'_>,
        s: &mut Source,
    ) -> Result<Vec<ListenEvent>> {
        let mut events = vec![];
        if s.faulted {
            *s = Source::new(chunk.generation, chunk.host_time_ns);
            events.push(ListenEvent::Discontinuity {
                source: chunk.source.into(),
                host_time_ns: chunk.host_time_ns,
            });
        }
        // Callback jitter is not tolerated as a time gap: adapters must timestamp
        // the first sample from the stream clock, not callback delivery time.
        if s.generation != chunk.generation || s.next_ns.abs_diff(chunk.host_time_ns) > 125000 {
            if s.active {
                s.probability = self.turn_probability(chunk.source, s)?;
                self.finish(chunk.source, s, true, &mut events)?;
            }
            *s = Source::new(chunk.generation, chunk.host_time_ns);
            events.push(ListenEvent::Discontinuity {
                source: chunk.source.into(),
                host_time_ns: chunk.host_time_ns,
            });
        }
        s.next_ns = chunk.host_time_ns + chunk.samples.len() as u64 * 62500;
        s.pending.extend(chunk.samples);
        while s.pending.len() >= 512 {
            let block: Vec<_> = s.pending.drain(..512).collect();
            let block: [f32; 512] = block.try_into().unwrap();
            let ns = s.pending_ns;
            s.pending_ns += 32_000_000;
            let p = self.vad.probability(&block, &mut s.vad)?;
            self.evidence(chunk.source, ns, serde_json::json!({"model":"silero","input":{"source":chunk.source,"start_ns":ns,"end_ns":ns+32_000_000,"rate":16000,"samples":512,"context_samples":64,"state":"previous source/generation Silero state; zero on reset"},"generation":s.generation,"probability":p,"utterance_start_ns":s.active.then_some(s.start_ns)}))?;
            s.voiced.push(ns, p >= self.config.onset_threshold);
            // Within the policy's pause (1.5 s), speech going on again needs only the continue level, as when this
            // pause was reported at 1.5 s: an early `SpeechEnd` must not make his soft words count as silence.
            let speech = p
                >= if s.voicing || (s.active && s.silence < self.config.pause_samples) {
                    self.config.vad_continue
                } else {
                    self.config.vad_start
                };
            if speech && !s.active {
                s.active = true;
                s.voicing = true;
                s.start_ns = ns.saturating_sub(s.pre.len() as u64 * 62500);
                s.window_ns = s.start_ns;
                s.audio = s.pre.iter().copied().collect();
                s.turn_audio = s.pre.clone();
                s.words.clear();
                s.agreement.clear();
                s.stable = 0;
                s.since_partial = 0;
                s.partial_count = 0;
                s.probability = 0.0;
                events.push(ListenEvent::SpeechStart {
                    source: chunk.source.into(),
                    host_time_ns: s.start_ns,
                });
            } else if speech && !s.voicing {
                s.voicing = true;
                events.push(ListenEvent::SpeechStart {
                    source: chunk.source.into(),
                    host_time_ns: ns,
                });
            }
            if s.active {
                if s.audio.len() + 512 > crate::long::WINDOW_SAMPLES {
                    self.decode(chunk.source, s, false, &mut events)?;
                }
                s.audio.extend_from_slice(&block);
                s.turn_audio.extend(block);
                while s.turn_audio.len() > 128000 {
                    s.turn_audio.pop_front();
                }
                s.since_partial += 512;
                if speech {
                    s.last_voice_ns = ns + 32_000_000;
                    s.silence = 0;
                    s.checks_done = 0;
                } else {
                    s.silence += 512;
                }
                // The candidate pause, then each later check of the same pause: Smart Turn decides, the ceiling
                // below only catches a pause it never calls finished.
                let due = self.config.turn_check(chunk.source, s.checks_done);
                if let Some((_, bar)) = due.filter(|&(at, _)| s.silence >= at) {
                    if s.checks_done == 0 {
                        s.voicing = false;
                        events.push(ListenEvent::SpeechEnd {
                            source: chunk.source.into(),
                            host_time_ns: s.last_voice_ns,
                        });
                    }
                    s.checks_done += 1;
                    crate::timing::endpoint(chunk.source, s.last_voice_ns);
                    s.probability = self.turn_probability(chunk.source, s)?;
                    crate::timing::turn(chunk.source);
                    if s.probability > bar {
                        self.finish(chunk.source, s, false, &mut events)?;
                    }
                }
                if s.active && s.silence >= self.config.maximum_silence_samples {
                    self.finish(chunk.source, s, true, &mut events)?;
                }
                let interval = match self.policy(chunk.source) {
                    PartialPolicy::FinalOnly => usize::MAX,
                    PartialPolicy::Responsive if s.partial_count == 0 => self
                        .config
                        .first_partial_samples
                        .min(self.config.partial_interval_samples),
                    PartialPolicy::Responsive => self
                        .config
                        .partial_interval_samples
                        .saturating_mul(s.partial_count)
                        .min(self.config.maximum_partial_interval_samples),
                };
                if s.active && s.voicing && s.since_partial >= interval {
                    if self.batch_partial_sources.contains(chunk.source) {
                        self.deferred_partials.push((chunk.source.into(), None));
                    } else {
                        self.decode(chunk.source, s, false, &mut events)?;
                    }
                }
            }
            s.pre.extend(block);
            while s.pre.len() > 16000 {
                s.pre.pop_front();
            }
        }
        Ok(events)
    }
    fn decode(
        &mut self,
        source: &str,
        s: &mut Source,
        final_: bool,
        events: &mut Vec<ListenEvent>,
    ) -> Result<()> {
        // A window never exceeds 15 seconds. Roll with two seconds of acoustic
        // context; all previous words outside the revisable tail are retained.
        let result = self.asr.transcribe(&s.audio, s.window_ns)?;
        self.apply_decode(source, s, final_, events, result)
    }
    fn apply_decode(
        &mut self,
        source: &str,
        s: &mut Source,
        final_: bool,
        events: &mut Vec<ListenEvent>,
        result: Transcript,
    ) -> Result<()> {
        s.decode_revision = s.decode_revision.saturating_add(1);
        // Raw model output precedes reconciliation, rolling and UI coalescing.
        self.evidence(source,s.window_ns,serde_json::json!({"model":"parakeet","is_final":final_,"revision":s.decode_revision,"generation":s.generation,"input":{"audio_id":source,"source":source,"start_ns":s.window_ns,"end_ns":s.window_ns+s.audio.len() as u64*62500,"rate":16000,"samples":s.audio.len(),"transform":"capture mono/resample 16k"},"utterance_start_ns":s.start_ns,"transcript":result}))?;
        ensure!(
            result.words.len() <= 2048 && result.words.iter().all(|w| w.text.len() <= 1024),
            "Recognizer exceeded transcript bounds"
        );
        // Parakeet starts a word that follows silence early; move it to speech. Partials, window rolls and finals all
        // pass here, so every decode of the same audio snaps the same way. The evidence above keeps the raw times.
        let mut result = result;
        s.voiced.snap(&mut result.words);
        // Then to the energy onset, which also finds speech after a pause Silero held through and under babble.
        crate::onset::refine_with_energy(&mut result.words, &s.audio, s.window_ns);
        let merged = reconcile(&s.words, &result.words, s.window_ns);
        // Confirmation is a consensus hint, not an irrevocable commit. Even
        // three agreeing decodes can be wrong (Polish kościela -> kościelna).
        // The newest decode owns all words still in the acoustic window.
        let common = s
            .words
            .iter()
            .zip(&merged)
            .take_while(|(a, b)| a.text == b.text && a.start_ns.abs_diff(b.start_ns) <= 320_000_000)
            .count();
        let cutoff = s.window_ns + s.audio.len() as u64 * 62500;
        let agreement: Vec<_> = (0..merged.len())
            .map(|i| {
                if i < common {
                    s.agreement.get(i).copied().unwrap_or(1).saturating_add(1)
                } else {
                    1
                }
            })
            .collect();
        let confirmed = agreement.iter().take_while(|&&n| n >= 3).count();
        s.stable = confirmed.min(merged.partition_point(|w| w.end_ns + 3_000_000_000 <= cutoff));
        s.agreement = agreement;
        s.words = merged;
        s.since_partial = 0;
        if !final_ {
            s.partial_count = s.partial_count.saturating_add(1);
            // Recompute only tail after stable prefix, retaining >=2 s overlap.
            // The fixed-shape encoder does the same arithmetic for a 3-s or
            // 15-s padded input. Retain context until the 15-s window is full;
            // trimming earlier worsens final quality without saving encoder work.
            // The roll is aligned to the encoder's 80-ms time grid (shared with `long.rs`).
            let trim = if s.audio.len() + 512 > crate::long::WINDOW_SAMPLES {
                crate::long::ROLL_SAMPLES
            } else {
                0
            };
            if trim > 0 {
                s.audio.drain(..trim);
                s.window_ns += trim as u64 * 62500;
                // Send only words outside the retained overlap. Consumers
                // append these segments and replace the bounded live tail.
                let count = crate::long::committed_before(&s.words, s.window_ns);
                if count > 0 {
                    let words: Vec<_> = s.words.drain(..count).collect();
                    let transcript = Transcript::from_words(words);
                    events.push(ListenEvent::TranscriptSegment {
                        source: source.into(),
                        text: transcript.text,
                        start_ns: s.start_ns,
                        end_ns: transcript.words.last().unwrap().end_ns,
                        words: transcript.words,
                    });
                    s.agreement.drain(..count);
                    s.segmented = true;
                    s.stable = s.stable.saturating_sub(count);
                    s.start_ns = s.words.first().map_or(s.window_ns, |w| w.start_ns);
                }
            }
            if self.policy(source) != PartialPolicy::FinalOnly {
                events.push(ListenEvent::PartialTranscript {
                    source: source.into(),
                    text: Transcript::from_words(s.words.clone()).text,
                    words: s.words.clone(),
                    stable_words: s.stable,
                });
            }
        }
        Ok(())
    }
    fn finish(
        &mut self,
        source: &str,
        s: &mut Source,
        forced: bool,
        events: &mut Vec<ListenEvent>,
    ) -> Result<()> {
        if forced {
            crate::timing::endpoint(source, s.last_voice_ns);
        }
        if s.voicing {
            events.push(ListenEvent::SpeechEnd {
                source: source.into(),
                host_time_ns: s.last_voice_ns,
            });
        }
        if self.batch_partial_sources.contains(source) {
            // This source has no second frame in this tick. Retain its acoustic
            // input for the shared encoder while preventing another endpoint.
            s.active = false;
            s.voicing = false;
            s.pre.clear();
            self.deferred_partials.push((source.into(), Some(forced)));
            return Ok(());
        }
        self.decode(source, s, true, events)?;
        self.complete_finish(source, s, forced, events, false);
        Ok(())
    }
    fn complete_finish(
        &mut self,
        source: &str,
        s: &mut Source,
        forced: bool,
        events: &mut Vec<ListenEvent>,
        preserve_post_frame_pre: bool,
    ) {
        let transcript = Transcript::from_words(std::mem::take(&mut s.words));
        // VAD and acoustic alignment can disagree by a few frames. The final
        // utterance interval must contain every word we expose to consumers.
        let end_ns = transcript
            .words
            .iter()
            .map(|word| word.end_ns)
            .max()
            .unwrap_or(s.last_voice_ns)
            .max(s.last_voice_ns);
        // Bug note 2026-11-14T11-58-47 (11:56:55): a 15-s window roll sent every word of the turn as a segment, the
        // final then had none, and no final was emitted: the consumer kept his plea open for 103 s, until he spoke
        // again (20 of his finals since 10 Nov). A rolled utterance always ends with a final, empty or not.
        if !transcript.words.is_empty() || s.segmented {
            crate::timing::decoded(source, end_ns);
            events.push(ListenEvent::FinalUtterance {
                source: source.into(),
                text: transcript.text,
                words: transcript.words,
                start_ns: s.start_ns,
                end_ns,
                end_of_turn_probability: s.probability,
                forced,
            });
        }
        s.active = false;
        s.voicing = false;
        s.audio.clear();
        s.turn_audio.clear();
        // A new utterance may begin before the one-second preroll expires.
        // Do not decode speech already emitted in the previous final again.
        if !preserve_post_frame_pre {
            s.pre.clear();
        }
        s.silence = 0;
        s.checks_done = 0;
        s.stable = 0;
        s.segmented = false;
        s.agreement.clear();
    }
    /// End-of-file/source: include the exact pending remainder, no per-callback
    /// padding. An EOF final is forced; its probability is still measured.
    pub fn flush_source(&mut self, source: &str) -> Result<Vec<ListenEvent>> {
        let Some(mut s) = self.sources.remove(source) else {
            return Ok(vec![]);
        };
        let result = self.flush_state(source, &mut s);
        if result.is_err() {
            s.faulted = true;
            self.sources.insert(source.into(), s);
        }
        result
    }
    fn flush_state(&mut self, source: &str, s: &mut Source) -> Result<Vec<ListenEvent>> {
        let mut events = vec![];
        if s.active && !s.faulted {
            let remainder: Vec<_> = s.pending.drain(..).collect();
            s.pending_ns += remainder.len() as u64 * 62500;
            if s.audio.len() + remainder.len() > 240000 {
                self.decode(source, s, false, &mut events)?;
            }
            s.audio.extend_from_slice(&remainder);
            s.turn_audio.extend(remainder);
            while s.turn_audio.len() > 128000 {
                s.turn_audio.pop_front();
            }
            s.probability = self.turn_probability(source, s)?;
            self.finish(source, s, true, &mut events)?;
        }
        Ok(events)
    }
    /// Finish only captured PCM before announcing a loss boundary. Keep policy
    /// and reset acoustic state; post-gap samples start a separate utterance.
    pub fn discontinue_source(&mut self, source: &str, host_time_ns: u64) -> Result<Vec<ListenEvent>> {
        let mut events = self.flush_source(source)?;
        events.push(ListenEvent::Discontinuity { source: source.into(), host_time_ns });
        Ok(events)
    }
    pub fn remove_source(&mut self, source: &str) -> Result<Vec<ListenEvent>> {
        self.config.source_policies.remove(source);
        self.flush_source(source)
    }
    /// Forget a broken stream without pretending the lost tail was a final.
    pub fn discard_source(&mut self, source: &str) {
        self.sources.remove(source);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct FakeAsr {
        calls: usize,
    }
    impl Recognizer for FakeAsr {
        fn transcribe(&mut self, a: &[f32], ns: u64) -> Result<Transcript> {
            self.calls += 1;
            Ok(Transcript::from_words(vec![Word {
                text: "hello".into(),
                start_ns: ns,
                end_ns: ns + (a.len().min(1600) as u64 * 62500),
                confidence: 1.0,
            }]))
        }
    }
    /// Words at fixed host times, returned once the window has heard all of them (like a decoder, which only emits
    /// heard words). Several start in silence, the way Parakeet starts a word after a pause.
    struct EarlyAsr(Vec<(&'static str, u64, u64)>);
    impl Recognizer for EarlyAsr {
        fn transcribe(&mut self, a: &[f32], ns: u64) -> Result<Transcript> {
            let end = ns + a.len() as u64 * 62500;
            Ok(Transcript::from_words(
                self.0
                    .iter()
                    .filter(|(_, s, e)| *s >= ns && *e <= end)
                    .map(|(t, s, e)| Word { text: (*t).into(), start_ns: *s, end_ns: *e, confidence: 1.0 })
                    .collect(),
            ))
        }
    }
    #[test]
    fn word_onsets_snap_to_speech_identically_in_partials_window_segments_and_finals() {
        const MS: u64 = 1_000_000;
        // Speech (FakeVad: amplitude 1.0) in 1.024–1.92 s, 2.4–16.0 s and 16.416–18.0 s; silence around it.
        let voiced = |ns: u64| (1_024 * MS..1_920 * MS).contains(&ns) || (2_400 * MS..16_000 * MS).contains(&ns)
            || (16_416 * MS..18_000 * MS).contains(&ns);
        let mut words = vec![
            ("Take", 620 * MS, 1_000 * MS), // wholly in the silence before the onset
            ("a", 1_000 * MS, 1_200 * MS),
            ("look.", 1_200 * MS, 1_900 * MS),
            ("again", 1_920 * MS, 2_800 * MS), // absorbs the 480 ms pause
        ];
        let fillers = ["w06", "w07", "w08", "w09", "w10", "w11", "w12", "w13", "w14", "w15", "w16", "w17", "w18", "w19",
            "w20", "w21", "w22", "w23", "w24", "w25", "w26", "w27", "w28", "w29", "w30", "w31"];
        for (k, t) in (6..=31).zip(fillers) {
            words.push((t, k * 500 * MS, k * 500 * MS + 450 * MS));
        }
        words.push(("late", 16_000 * MS, 16_900 * MS)); // absorbs the 416 ms pause
        words.push(("w34", 17_000 * MS, 17_450 * MS));
        words.push(("w35.", 17_500 * MS, 17_950 * MS));
        let mut e = ListenEngine::new(
            EarlyAsr(words.clone()),
            FakeVad,
            FakeTurn(0.9),
            ListenConfig {
                source_policies: HashMap::from([("mic".into(), PartialPolicy::Responsive)]),
                pause_samples: 9600,
                maximum_silence_samples: 16000,
                partial_interval_samples: 16000,
                ..ListenConfig::default()
            },
        )
        .unwrap();
        let mut seen: HashMap<String, std::collections::BTreeSet<u64>> = HashMap::new();
        let mut kinds = std::collections::BTreeSet::new();
        for i in 0..(19_000 * MS / BLOCK) {
            let ns = i * BLOCK;
            let block = [if voiced(ns) { 1.0 } else { 0.0 }; 512];
            for event in e.push(AudioChunk { source: "mic", generation: 1, host_time_ns: ns, samples: &block }).unwrap() {
                let (kind, words) = match event {
                    ListenEvent::PartialTranscript { words, .. } => ("partial", words),
                    ListenEvent::TranscriptSegment { words, .. } => ("segment", words),
                    ListenEvent::FinalUtterance { words, .. } => ("final", words),
                    _ => continue,
                };
                assert!(words.windows(2).all(|p| p[0].start_ns <= p[1].start_ns));
                assert!(words.iter().all(|w| w.start_ns <= w.end_ns));
                for w in words {
                    kinds.insert((kind, w.text.clone()));
                    seen.entry(w.text).or_default().insert(w.start_ns);
                }
            }
        }
        const BLOCK: u64 = 32 * MS;
        // Every decode of a word reports one start.
        for (text, starts) in &seen {
            assert_eq!(starts.len(), 1, "{text}: {starts:?}");
        }
        let start = |t: &str| *seen[t].first().unwrap() / MS;
        assert_eq!(start("Take"), 1_024, "moved out of the pre-roll to the onset");
        assert_eq!(start("a"), 1_112, "shares its span with the moved word");
        assert_eq!(start("look."), 1_200, "already in speech: unchanged");
        assert_eq!(start("again"), 2_400);
        assert_eq!(start("w06"), 3_000);
        assert_eq!(start("late"), 16_416);
        // The paths each carried snapped words: partials, the 15-second window roll's segment, and the final.
        for (kind, text) in [("partial", "Take"), ("segment", "Take"), ("segment", "again"), ("partial", "late"), ("final", "late")] {
            assert!(kinds.contains(&(kind, text.to_string())), "{kind} {text}: {kinds:?}");
        }
    }
    /// Bug note 2026-11-14T11-58-47: his 11:56:55–11:57:07 plea, then 85 s of silence with the turn model unsure. The
    /// 15-s window roll sent all its words as a segment; the final had no words left and was never emitted, so the
    /// utterance stayed open until he spoke again. Now the turn still ends with a final (empty), at the silence ceiling.
    #[test]
    fn a_turn_whose_words_all_went_out_in_a_window_roll_still_ends_with_a_final() {
        const MS: u64 = 1_000_000;
        const BLOCK: u64 = 32 * MS;
        let voiced = |ns: u64| (1_000 * MS..7_000 * MS).contains(&ns);
        let words = vec![("Did", 1_100 * MS, 1_500 * MS), ("you", 1_600 * MS, 2_000 * MS), ("hear", 2_100 * MS, 2_600 * MS),
            ("what", 3_000 * MS, 3_500 * MS), ("I", 3_600 * MS, 3_800 * MS), ("said?", 4_000 * MS, 6_900 * MS)];
        let mut e = ListenEngine::new(
            EarlyAsr(words),
            FakeVad,
            FakeTurn(0.1), // never sure the turn is over: only the silence ceiling ends it
            ListenConfig {
                source_policies: HashMap::from([("mic".into(), PartialPolicy::Responsive)]),
                pause_samples: 9600,
                maximum_silence_samples: 16000 * 14,
                partial_interval_samples: 16000,
                ..ListenConfig::default()
            },
        )
        .unwrap();
        let (mut segment, mut finals) = (vec![], vec![]);
        for i in 0..(30_000 * MS / BLOCK) {
            let ns = i * BLOCK;
            let block = [if voiced(ns) { 1.0 } else { 0.0 }; 512];
            for event in e.push(AudioChunk { source: "mic", generation: 1, host_time_ns: ns, samples: &block }).unwrap() {
                match event {
                    ListenEvent::TranscriptSegment { words, .. } => segment.extend(words.into_iter().map(|w| w.text)),
                    ListenEvent::FinalUtterance { words, end_ns, forced, .. } => finals.push((words.len(), end_ns, forced, ns)),
                    _ => {}
                }
            }
        }
        assert_eq!(segment, ["Did", "you", "hear", "what", "I", "said?"], "the roll sent every word");
        assert_eq!(finals.len(), 1, "{finals:?}");
        let (n, end_ns, forced, at) = finals[0];
        assert_eq!(n, 0, "no word twice: the consumer joins the segment as the prefix");
        assert!(forced);
        assert!((6_900 * MS..7_100 * MS).contains(&end_ns), "ends at the last voice: {end_ns}");
        assert!(at < 7_000 * MS + 14_500 * MS, "at the silence ceiling, not at the next utterance: {at}");
    }
    #[test]
    fn every_decode_keeps_raw_words_confidence_clock_revision_and_final_flag() {
        let mut e = engine(1.);
        let records = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let saved = records.clone();
        e.set_evidence_hook(Box::new(move |source,ns,payload| {
            saved.lock().unwrap().push((source.to_owned(),ns,payload)); Ok(())
        }));
        let mut state = Source::new(7,1_000_000_000);
        state.audio = vec![0.8;16000];
        let mut events = vec![];
        for (text, final_) in [("wrong",false),("correct",false),("final",true)] {
            e.apply_decode("mic",&mut state,final_,&mut events,Transcript::from_words(vec![Word {
                text:text.into(), start_ns:1_000_000_000, end_ns:1_500_000_000,confidence:0.875,
            }])).unwrap();
        }
        let records = records.lock().unwrap();
        assert_eq!(records.len(),3);
        for (i,(source,ns,payload)) in records.iter().enumerate() {
            assert_eq!(source,"mic"); assert_eq!(*ns,1_000_000_000);
            assert_eq!(payload["revision"],i+1);
            assert_eq!(payload["is_final"],i==2);
            assert_eq!(payload["input"]["audio_id"],"mic");
            assert_eq!(payload["input"]["end_ns"],2_000_000_000_u64);
            assert_eq!(payload["transcript"]["words"][0]["confidence"],0.875);
        }
        assert_eq!(records[0].2["transcript"]["text"],"wrong");
        assert_eq!(records[1].2["transcript"]["text"],"correct");
        assert_eq!(events.len(),2);
    }
    struct FakeVad;
    impl SpeechDetector for FakeVad {
        fn probability(&mut self, a: &[f32; 512], s: &mut VadState) -> Result<f32> {
            s.hidden[0] += 1.0;
            Ok(if a.iter().any(|v| *v > 0.5) { 1.0 } else { 0.0 })
        }
    }
    struct FakeTurn(f32);
    impl TurnDetector for FakeTurn {
        fn probability(&mut self, _: &[f32]) -> Result<f32> {
            Ok(self.0)
        }
    }
    fn engine(p: f32) -> ListenEngine<FakeAsr, FakeVad, FakeTurn> {
        ListenEngine::new(
            FakeAsr { calls: 0 },
            FakeVad,
            FakeTurn(p),
            ListenConfig {
                source_policies: HashMap::from([("mic".into(), PartialPolicy::Responsive)]),
                pause_samples: 1024,
                maximum_silence_samples: 2048,
                partial_interval_samples: 1024,
                ..ListenConfig::default()
            },
        )
        .unwrap()
    }

    #[test]
    fn ready_partials_use_one_recognizer_batch_and_failures_fault_each_source() {
        struct BatchAsr {
            sizes: Vec<usize>,
            fail: bool,
        }
        impl Recognizer for BatchAsr {
            fn transcribe(&mut self, a: &[f32], ns: u64) -> Result<Transcript> {
                FakeAsr { calls: 0 }.transcribe(a, ns)
            }
            fn transcribe_batch(&mut self, a: &[(&[f32], u64)]) -> Vec<Result<Transcript>> {
                if !a.is_empty() {
                    self.sizes.push(a.len());
                }
                a.iter()
                    .map(|(a, ns)| {
                        if self.fail {
                            Err(anyhow::anyhow!("batch failed"))
                        } else {
                            self.transcribe(a, *ns)
                        }
                    })
                    .collect()
            }
        }
        for fail in [false, true] {
            let mut e = ListenEngine::new(
                BatchAsr {
                    sizes: vec![],
                    fail,
                },
                FakeVad,
                FakeTurn(0.),
                ListenConfig {
                    first_partial_samples: 512,
                    partial_interval_samples: 512,
                    ..ListenConfig::default()
                },
            )
            .unwrap();
            for source in ["mic:a", "mic:b", "mic:c"] { e.set_source_policy(source, PartialPolicy::Responsive).unwrap(); }
            let a = [1.; 512];
            let chunks: Vec<_> = ["mic:a", "mic:b", "mic:c"]
                .into_iter()
                .map(|source| AudioChunk {
                    source,
                    generation: 0,
                    host_time_ns: 0,
                    samples: &a,
                })
                .collect();
            let results = e.push_batch(&chunks);
            assert_eq!(e.asr.sizes, [3]);
            assert!(results.iter().all(|r| r.is_err() == fail));
            assert!(e.sources.values().all(|s| s.faulted == fail));
            if !fail {
                assert!(results.iter().all(|r| r
                    .as_ref()
                    .unwrap()
                    .iter()
                    .any(|e| matches!(e, ListenEvent::PartialTranscript { .. }))));
            }
        }
    }
    #[test]
    fn ready_finals_batch_without_changing_events_or_next_turn_preroll() {
        struct BatchAsr(Vec<usize>);
        impl Recognizer for BatchAsr {
            fn transcribe(&mut self, a: &[f32], ns: u64) -> Result<Transcript> {
                FakeAsr { calls: 0 }.transcribe(a, ns)
            }
            fn transcribe_batch(&mut self, a: &[(&[f32], u64)]) -> Vec<Result<Transcript>> {
                if !a.is_empty() {
                    self.0.push(a.len());
                }
                a.iter().map(|(a, ns)| self.transcribe(a, *ns)).collect()
            }
        }
        let mut serial = engine(0.9);
        let mut batched = ListenEngine::new(
            BatchAsr(vec![]),
            FakeVad,
            FakeTurn(0.9),
            serial.config.clone(),
        )
        .unwrap();
        for tick in 0..40 {
            let audio = if tick < 6 || (15..22).contains(&tick) {
                [1.; 512]
            } else {
                [0.; 512]
            };
            let chunks: Vec<_> = ["app:a", "app:b", "app:c"]
                .into_iter()
                .map(|source| AudioChunk {
                    source,
                    generation: 0,
                    host_time_ns: tick * 32_000_000,
                    samples: &audio,
                })
                .collect();
            let results = batched.push_batch(&chunks);
            for (chunk, actual) in chunks.into_iter().zip(results) {
                let id = chunk.source;
                assert_eq!(
                    serde_json::to_value(actual.unwrap()).unwrap(),
                    serde_json::to_value(serial.push(chunk).unwrap()).unwrap()
                );
                assert_eq!(batched.sources[id].pre, serial.sources[id].pre);
                assert_eq!(batched.sources[id].audio, serial.sources[id].audio);
            }
        }
        assert_eq!(batched.asr.0, [3, 3]);
    }
    #[test]
    fn batched_intake_preserves_events_and_discontinuity_resets() {
        let mut serial = engine(0.9);
        let mut batched = engine(0.9);
        for tick in 0..12 {
            let audio_a = if tick < 5 { [1.; 320] } else { [0.; 320] };
            let audio_b = if tick % 3 == 0 { [1.; 320] } else { [0.; 320] };
            let generation = if tick >= 7 { 2 } else { 1 };
            let chunks = [
                AudioChunk {
                    source: "mic:a",
                    generation: 1,
                    host_time_ns: tick * 20_000_000,
                    samples: &audio_a,
                },
                AudioChunk {
                    source: "app:b",
                    generation,
                    host_time_ns: tick * 20_000_000,
                    samples: &audio_b,
                },
            ];
            let results = batched.push_batch(&chunks);
            for (chunk, result) in chunks.into_iter().zip(results) {
                let expected = serial.push(chunk).unwrap();
                let actual = result.unwrap();
                assert_eq!(
                    serde_json::to_value(actual).unwrap(),
                    serde_json::to_value(expected).unwrap()
                );
            }
        }
        for id in ["mic:a", "app:b"] {
            assert_eq!(
                batched.sources[id].vad.hidden,
                serial.sources[id].vad.hidden
            );
            assert_eq!(
                serde_json::to_value(batched.flush_source(id).unwrap()).unwrap(),
                serde_json::to_value(serial.flush_source(id).unwrap()).unwrap()
            );
        }
    }
    struct FailOnceAsr(bool);
    impl Recognizer for FailOnceAsr {
        fn transcribe(&mut self, a: &[f32], ns: u64) -> Result<Transcript> {
            if std::mem::take(&mut self.0) {
                anyhow::bail!("injected inference failure");
            }
            FakeAsr { calls: 0 }.transcribe(a, ns)
        }
    }
    #[test]
    fn inference_failure_reports_discontinuity_on_recovery() {
        for at_eof in [false, true] {
            let mut e = ListenEngine::new(
                FailOnceAsr(true),
                FakeVad,
                FakeTurn(0.9),
                ListenConfig {
                    partial_interval_samples: 512,
                    ..ListenConfig::default()
                },
            )
            .unwrap();
            e.set_source_policy("mic", PartialPolicy::Responsive).unwrap();
            if at_eof {
                e.config.partial_interval_samples = 16000;
            }
            let result = e.push(AudioChunk {
                source: "mic",
                generation: 1,
                host_time_ns: 0,
                samples: &[1.0; 512],
            });
            if at_eof {
                result.unwrap();
                assert!(e.flush_source("mic").is_err());
            } else {
                assert!(result.is_err());
            }
            let events = e
                .push(AudioChunk {
                    source: "mic",
                    generation: 1,
                    host_time_ns: 32_000_000,
                    samples: &[1.0; 512],
                })
                .unwrap();
            assert!(
                matches!(&events[0], ListenEvent::Discontinuity { source, .. } if source == "mic")
            );
            assert_eq!(e.sources["mic"].vad.hidden[0], 1.0);
        }
    }
    #[test]
    fn accumulator_and_per_source_state() {
        let mut e = engine(0.9);
        let x = [1.0; 320];
        for source in ["mic", "app"] {
            assert!(e
                .push(AudioChunk {
                    source,
                    generation: 1,
                    host_time_ns: 0,
                    samples: &x
                })
                .unwrap()
                .is_empty());
        }
        assert_eq!(e.asr.calls, 0);
        for source in ["mic", "app"] {
            let events = e
                .push(AudioChunk {
                    source,
                    generation: 1,
                    host_time_ns: 20_000_000,
                    samples: &x,
                })
                .unwrap();
            assert!(matches!(&events[0],ListenEvent::SpeechStart{source:s,..} if s==source));
            assert_eq!(e.sources[source].vad.hidden[0], 1.0);
            assert_eq!(e.sources[source].pending.len(), 128);
        }
    }
    /// The user's turn timing on audio timelines (11 Nov, "we need to get rid of the delay"): Smart Turn, not a fixed
    /// 1.5 s silence, closes his turn; a mid-sentence pause does not; resumed speech stays one turn.
    mod turn_timing {
        use super::*;
        use std::{cell::RefCell, rc::Rc};
        /// Smart Turn as a function of the pause so far (ms of trailing silence in its audio); every call is logged.
        #[derive(Clone)]
        struct Script(Rc<RefCell<(fn(u64) -> f32, Vec<u64>)>>);
        impl TurnDetector for Script {
            fn probability(&mut self, audio: &[f32]) -> Result<f32> {
                let quiet = audio.iter().rev().take_while(|v| **v < 0.5).count() as u64 / 16;
                let mut s = self.0.borrow_mut();
                s.1.push(quiet);
                Ok((s.0)(quiet))
            }
        }
        const POLICY: &str = r#"{"threshold": 0.5, "minimum_silence_ms": 1500, "long_silence_ms": 2500}"#;
        fn owner() -> ListenConfig {
            let mut c = ListenConfig::default();
            c.apply_tuned_policy(&serde_json::from_str(POLICY).unwrap()).unwrap();
            c
        }
        /// Before 11 Nov: one check after 1.5 s of silence, the same ceiling, on every source.
        fn before() -> ListenConfig {
            ListenConfig { pause_samples: 24000, maximum_silence_samples: 40000, complete_threshold: 0.5, ..ListenConfig::default() }
        }
        const BLOCK_NS: u64 = 32_000_000;
        /// What happened on one timeline.
        struct Heard {
            /// Each final: ms from the end of the last voiced block to the block that emitted it, and forced.
            finals: Vec<(u64, bool)>,
            events: Vec<ListenEvent>,
            /// The pause (ms) at each Smart Turn call.
            asked: Vec<u64>,
        }
        /// Plays `(speech?, ms, Smart Turn meanwhile)` on `source` in 32 ms blocks (lengths round up to whole blocks).
        fn play(config: ListenConfig, source: &str, timeline: &[(bool, u64, fn(u64) -> f32)]) -> Heard {
            let turn = Script(Rc::new(RefCell::new((|_| 0.0, vec![]))));
            let mut e = ListenEngine::new(FakeAsr { calls: 0 }, FakeVad, turn.clone(), config).unwrap();
            let (mut t, mut last_voice_end, mut finals, mut events) = (0u64, 0u64, vec![], vec![]);
            for &(speech, ms, p) in timeline {
                turn.0.borrow_mut().0 = p;
                for _ in 0..(ms * 1_000_000).div_ceil(BLOCK_NS) {
                    let block = [if speech { 1.0 } else { 0.0 }; 512];
                    let out = e.push(AudioChunk { source, generation: 1, host_time_ns: t, samples: &block }).unwrap();
                    t += BLOCK_NS;
                    if speech {
                        last_voice_end = t;
                    }
                    for ev in &out {
                        if let ListenEvent::FinalUtterance { forced, .. } = ev {
                            finals.push(((t - last_voice_end) / 1_000_000, *forced));
                        }
                    }
                    events.extend(out);
                }
            }
            let asked = turn.0.borrow().1.clone();
            Heard { finals, events, asked }
        }
        fn talk(_: u64) -> f32 {
            0.0
        }
        fn sure(_: u64) -> f32 {
            0.97
        }
        /// Finished, but Smart Turn only grows sure as the pause lengthens.
        fn slowly_sure(quiet: u64) -> f32 {
            if quiet < 900 { 0.6 } else { 0.96 }
        }
        /// Mid-sentence: Smart Turn leans "done" more the longer he pauses (the sweep's 600 ms row: .9 ended 48 of
        /// 382 such pauses), yet stays under the .95 bar of his checks at 300 ms and 1 s.
        fn leaning(quiet: u64) -> f32 {
            if quiet < 900 { 0.9 } else { 0.94 }
        }
        /// Mid-sentence, a dangling word: Smart Turn stays under the 1.5 s bar.
        fn unfinished(_: u64) -> f32 {
            0.3
        }
        #[test]
        fn a_finished_sentence_reaches_the_final_without_the_fixed_wait() {
            let said: [(bool, u64, fn(u64) -> f32); 2] = [(true, 2000, talk), (false, 3000, sure)];
            let old = play(before(), "mic:BuiltInMicrophoneDevice:1", &said).finals;
            assert!(old.len() == 1 && old[0].0 >= 1500, "before: the 1.5 s wait ({old:?})");
            let new = play(owner(), "mic:BuiltInMicrophoneDevice:1", &said).finals;
            assert!(new.len() == 1 && !new[0].1 && new[0].0 <= 350, "a sure finish ends at the 300 ms check ({new:?})");
            let new = play(owner(), "mic:BuiltInMicrophoneDevice:1", &[(true, 2000, talk), (false, 3000, slowly_sure)]).finals;
            assert!(new.len() == 1 && !new[0].1 && (1000..1100).contains(&new[0].0), "sure from 1 s: ends there ({new:?})");
            // His phone's live microphone is his voice: the same early checks.
            let phone = play(owner(), "phone:airpods:3", &said).finals;
            assert!(phone.len() == 1 && !phone[0].1 && phone[0].0 <= 350, "the phone waits like an app ({phone:?})");
        }
        #[test]
        fn mid_sentence_pauses_of_half_one_and_two_seconds_stay_one_turn() {
            for (pause, p) in [(500, leaning as fn(u64) -> f32), (1000, leaning), (1400, leaning), (500, unfinished), (1000, unfinished), (2000, unfinished)] {
                let h = play(owner(), "mic:BuiltInMicrophoneDevice:1", &[(true, 1500, talk), (false, pause, p), (true, 1500, talk), (false, 3000, sure)]);
                assert_eq!(h.finals.len(), 1, "a {pause} ms pause split the turn: {:?}", h.finals);
                assert!(!h.finals[0].1 && h.finals[0].0 <= 350, "{pause} ms: {:?}", h.finals);
                // Asked in the pause at each check its length reaches, then once at the end.
                let in_pause: Vec<u64> = h.asked[..h.asked.len() - 1].iter().map(|q| q / 100).collect();
                let due: Vec<u64> = [3, 10, 15].into_iter().filter(|at| at * 100 <= pause).collect();
                assert_eq!(in_pause, due, "{pause} ms: asked at {:?}", h.asked);
                let starts = h.events.iter().filter(|e| matches!(e, ListenEvent::SpeechStart { .. })).count();
                assert_eq!(starts, 2, "the resumed speech is the same utterance's voice again");
            }
        }
        #[test]
        fn resumed_speech_starts_the_checks_over_and_nothing_is_emitted_twice() {
            let h = play(
                owner(),
                "mic:BuiltInMicrophoneDevice:1",
                &[(true, 1000, talk), (false, 400, leaning), (true, 800, talk), (false, 1200, leaning), (true, 800, talk), (false, 3000, sure)],
            );
            assert_eq!(h.finals.len(), 1, "{:?}", h.finals);
            // Each pause is asked from its own start: 300 ms; 300 ms and 1 s; then the end at 300 ms.
            assert_eq!(h.asked.iter().map(|q| q / 100).collect::<Vec<_>>(), [3, 3, 10, 3], "{:?}", h.asked);
            let final_start = h.events.iter().find_map(|e| match e { ListenEvent::FinalUtterance { start_ns, .. } => Some(*start_ns), _ => None });
            let first_start = h.events.iter().find_map(|e| match e { ListenEvent::SpeechStart { host_time_ns, .. } => Some(*host_time_ns), _ => None });
            assert_eq!(final_start, first_start, "the final covers the turn from its first word");
            let early = h.events.iter().filter(|e| matches!(e, ListenEvent::PartialTranscript { .. } | ListenEvent::TranscriptSegment { .. })).count();
            assert_eq!(early, 0, "a pause decodes nothing early on the final-only microphone");
        }
        /// Silero as the block's level: .37 is soft speech, over the continue level (.35), under the start level (.4).
        struct LevelVad;
        impl SpeechDetector for LevelVad {
            fn probability(&mut self, a: &[f32; 512], _: &mut VadState) -> Result<f32> {
                Ok(a.iter().copied().fold(0.0, f32::max))
            }
        }
        #[test]
        fn soft_words_after_an_early_speech_end_still_count_as_speech() {
            // He talks, pauses 400 ms (the 300 ms check reports SpeechEnd), then goes on softly for 1.5 s. Before 11
            // Nov that pause was never reported, so the continue level held; it must still hold, or the soft words
            // count as silence and the 1.5 s check or the ceiling ends his turn while he talks.
            let mut e = ListenEngine::new(FakeAsr { calls: 0 }, LevelVad, FakeTurn(0.3), owner()).unwrap();
            let (mut t, mut finals, mut starts) = (0u64, vec![], 0);
            for (level, ms) in [(1.0, 1000), (0.0, 400), (0.37, 1500)] {
                for _ in 0..(ms * 1_000_000u64).div_ceil(BLOCK_NS) {
                    let out = e.push(AudioChunk { source: "mic:BuiltInMicrophoneDevice:1", generation: 1, host_time_ns: t, samples: &[level; 512] }).unwrap();
                    t += BLOCK_NS;
                    starts += out.iter().filter(|e| matches!(e, ListenEvent::SpeechStart { .. })).count();
                    finals.extend(out.into_iter().filter(|e| matches!(e, ListenEvent::FinalUtterance { .. })));
                }
            }
            assert!(finals.is_empty(), "ended while he spoke softly: {finals:?}");
            assert_eq!(starts, 2, "the soft words are his voice again after the early speech end");
        }
        #[test]
        fn a_pause_smart_turn_never_calls_finished_ends_at_the_ceiling() {
            let h = play(owner(), "mic:BuiltInMicrophoneDevice:1", &[(true, 2000, talk), (false, 3500, unfinished)]);
            assert!(h.finals.len() == 1 && h.finals[0].1 && (2500..2600).contains(&h.finals[0].0), "{:?}", h.finals);
            assert_eq!(h.asked.iter().map(|q| q / 100).collect::<Vec<_>>(), [3, 10, 15], "then the ceiling");
        }
        #[test]
        fn other_voices_keep_the_single_late_check() {
            // App audio and call guests are not his voice: the tuned model's early bars are not theirs.
            for source in ["process:1234", "process:5678:discord"] {
                let h = play(owner(), source, &[(true, 2000, talk), (false, 3000, sure)]);
                assert!(h.finals.len() == 1 && h.finals[0].0 >= 1500, "{source}: {:?}", h.finals);
                assert_eq!(h.asked.len(), 1, "{source}");
            }
        }
        #[test]
        fn the_policy_steps_are_ordered_and_bounded() {
            let c = owner();
            assert_eq!((c.pause_samples, c.complete_threshold, c.maximum_silence_samples), (24000, 0.5, 40000));
            assert_eq!(c.mic_turn_checks, vec![(4800, 0.95), (16000, 0.95), (24000, 0.5)]);
            // A policy whose own check comes earlier drops the steps after it.
            let mut short = ListenConfig::default();
            short.apply_tuned_policy(&serde_json::json!({"threshold": 0.6, "minimum_silence_ms": 800, "long_silence_ms": 2500})).unwrap();
            assert_eq!(short.mic_turn_checks, vec![(4800, 0.95), (12800, 0.6)]);
            assert!(ListenEngine::new(FakeAsr { calls: 0 }, FakeVad, FakeTurn(0.1), short).is_ok());
            let bad = ListenConfig { mic_turn_checks: vec![(9000, 0.5), (4800, 0.5)], ..ListenConfig::default() };
            assert!(ListenEngine::new(FakeAsr { calls: 0 }, FakeVad, FakeTurn(0.1), bad).is_err(), "out of order");
            let bad = ListenConfig { mic_turn_checks: vec![(30000, 0.5)], ..ListenConfig::default() };
            assert!(ListenEngine::new(FakeAsr { calls: 0 }, FakeVad, FakeTurn(0.1), bad).is_err(), "past the ceiling");
        }
    }
    #[test]
    fn endpoint_fallback_and_source_restart() {
        let mut e = engine(0.1);
        let speech = [1.0; 512];
        let quiet = [0.0; 2048];
        e.push(AudioChunk {
            source: "mic",
            generation: 1,
            host_time_ns: 0,
            samples: &speech,
        })
        .unwrap();
        let events = e
            .push(AudioChunk {
                source: "mic",
                generation: 1,
                host_time_ns: 32_000_000,
                samples: &quiet,
            })
            .unwrap();
        assert!(events.iter().any(|v|matches!(v,ListenEvent::FinalUtterance{forced:true,end_of_turn_probability:p,..} if *p==0.1)));
        let events = e
            .push(AudioChunk {
                source: "mic",
                generation: 2,
                host_time_ns: 500_000_000,
                samples: &speech,
            })
            .unwrap();
        assert!(matches!(&events[0], ListenEvent::Discontinuity { .. }));
        assert_eq!(e.sources["mic"].vad.hidden[0], 1.0);
    }
    #[test]
    fn turn_window_stays_eight_seconds_after_asr_roll() {
        let mut e = engine(0.1);
        let speech = [1.0; 512];
        for i in 0..600 {
            e.push(AudioChunk {
                source: "mic",
                generation: 1,
                host_time_ns: i * 32_000_000,
                samples: &speech,
            })
            .unwrap();
        }
        let s = &e.sources["mic"];
        assert_eq!(s.turn_audio.len(), 128000);
        assert!(s.audio.len() <= 240000 + 512);
    }
    #[test]
    fn complete_pause_and_eof() {
        let mut e = engine(0.9);
        e.push(AudioChunk {
            source: "a",
            generation: 0,
            host_time_ns: 0,
            samples: &[1.0; 512],
        })
        .unwrap();
        let ev = e
            .push(AudioChunk {
                source: "a",
                generation: 0,
                host_time_ns: 32_000_000,
                samples: &[0.0; 1024],
            })
            .unwrap();
        assert!(ev
            .iter()
            .any(|v| matches!(v, ListenEvent::FinalUtterance { forced: false, .. })));
        assert!(e.flush_source("a").unwrap().is_empty());
        e.push(AudioChunk {
            source: "b",
            generation: 0,
            host_time_ns: 0,
            samples: &[1.0; 600],
        })
        .unwrap();
        assert!(e
            .flush_source("b")
            .unwrap()
            .iter()
            .any(|v| matches!(v, ListenEvent::FinalUtterance { forced: true, .. })));
    }
    #[test]
    fn completed_speech_is_not_reused_as_preroll() {
        let mut e = engine(0.9);
        e.push(AudioChunk {
            source: "a",
            generation: 0,
            host_time_ns: 0,
            samples: &[1.0; 512],
        })
        .unwrap();
        e.push(AudioChunk {
            source: "a",
            generation: 0,
            host_time_ns: 32_000_000,
            samples: &[0.0; 1024],
        })
        .unwrap();
        let ev = e
            .push(AudioChunk {
                source: "a",
                generation: 0,
                host_time_ns: 96_000_000,
                samples: &[1.0; 512],
            })
            .unwrap();
        assert!(ev.iter().any(
            |v| matches!(v,ListenEvent::SpeechStart{host_time_ns,..} if *host_time_ns>=32_000_000)
        ));
    }
}

#[cfg(test)]
mod efficiency_tests {
    use super::*;
    struct Vad;
    impl SpeechDetector for Vad {
        fn probability(&mut self, _: &[f32; 512], s: &mut VadState) -> Result<f32> {
            s.hidden[0] += 1.0;
            Ok(1.0)
        }
    }
    struct Turn;
    impl TurnDetector for Turn {
        fn probability(&mut self, _: &[f32]) -> Result<f32> {
            Ok(0.1)
        }
    }
    struct DenseAsr;
    impl Recognizer for DenseAsr {
        fn transcribe(&mut self, a: &[f32], ns: u64) -> Result<Transcript> {
            assert!(a.len() <= 240000);
            Ok(Transcript::from_words(
                (0..a.len() / 1280)
                    .map(|i| Word {
                        text: format!("w{}", ns / 80_000_000 + i as u64),
                        start_ns: ns + i as u64 * 80_000_000,
                        end_ns: ns + (i + 1) as u64 * 80_000_000,
                        confidence: 1.0,
                    })
                    .collect(),
            ))
        }
    }
    #[test]
    fn three_continuous_interleaved_sources_five_minutes_stay_bounded() {
        let mut e = ListenEngine::new(DenseAsr, Vad, Turn, ListenConfig::default()).unwrap();
        let mut delivered = HashMap::<String, Vec<Word>>::new();
        let mut maximum_words = 0;
        for i in 0..9375 {
            for source in ["mic:owner:0", "process:discord", "process:browser"] {
                let events = e
                    .push(AudioChunk {
                        source,
                        generation: 0,
                        host_time_ns: i * 32_000_000,
                        samples: &[1.0; 512],
                    })
                    .unwrap();
                for event in events {
                    if let ListenEvent::TranscriptSegment { source, words, .. } = event {
                        delivered.entry(source).or_default().extend(words);
                    }
                }
                let s = &e.sources[source];
                assert!(s.audio.len() <= 240000);
                assert!(s.words.len() <= 188);
                assert!(s.agreement.len() <= 188);
                assert!(s.pending.len() < 512);
                assert!(s.pre.len() <= 16000);
                assert!(s.turn_audio.len() <= 128000);
                maximum_words = maximum_words.max(s.words.len());
            }
        }
        for source in ["mic:owner:0", "process:discord", "process:browser"] {
            for event in e.flush_source(source).unwrap() {
                if let ListenEvent::FinalUtterance { source, words, .. } = event {
                    delivered.entry(source).or_default().extend(words);
                }
            }
            let words = &delivered[source];
            assert_eq!(words.len(), 3750, "lost/duplicated words: {source}");
            for (i, w) in words.iter().enumerate() {
                assert_eq!(w.text, format!("w{i}"));
            }
        }
        assert!(maximum_words <= 188);
        assert!(e.sources.is_empty());
    }
    struct CorrectionAsr;
    impl Recognizer for CorrectionAsr {
        fn transcribe(&mut self, a: &[f32], ns: u64) -> Result<Transcript> {
            Ok(Transcript::from_words(vec![Word {
                text: if a.len() < 96000 {
                    "kościela"
                } else {
                    "kościelna"
                }
                .into(),
                start_ns: ns,
                end_ns: ns + 80_000_000,
                confidence: 1.0,
            }]))
        }
    }
    #[test]
    fn consensus_and_age_do_not_freeze_later_polish_correction() {
        let mut e = ListenEngine::new(
            CorrectionAsr,
            Vad,
            Turn,
            ListenConfig {
                partial_interval_samples: 16000,
                maximum_partial_interval_samples: 16000,
                first_partial_samples: 16000,
                ..ListenConfig::default()
            },
        )
        .unwrap();
        e.set_source_policy("mic", PartialPolicy::Responsive).unwrap();
        let mut confirmed_wrong = false;
        let mut corrected = false;
        for i in 0..200 {
            for event in e
                .push(AudioChunk {
                    source: "mic",
                    generation: 0,
                    host_time_ns: i * 32_000_000,
                    samples: &[1.0; 512],
                })
                .unwrap()
            {
                if let ListenEvent::PartialTranscript {
                    text, stable_words, ..
                } = event
                {
                    confirmed_wrong |= text == "kościela" && stable_words == 1;
                    corrected |= text == "kościelna";
                }
            }
        }
        assert!(confirmed_wrong && corrected);
        assert!(e
            .flush_source("mic")
            .unwrap()
            .iter()
            .any(|e| matches!(e,ListenEvent::FinalUtterance{text,..} if text=="kościelna")));
    }
    #[test]
    fn default_source_policy_and_overrides() {
        let mut e = ListenEngine::new(DenseAsr, Vad, Turn, ListenConfig::default()).unwrap();
        assert_eq!(e.policy("mic:built-in:0"), PartialPolicy::FinalOnly);
        assert_eq!(e.policy("process:discord"), PartialPolicy::FinalOnly);
        for source in ["mic", "process:discord"] {
            let mut partial = false;
            for i in 0..50 {
                partial |= e
                    .push(AudioChunk {
                        source,
                        generation: 0,
                        host_time_ns: i * 32_000_000,
                        samples: &[1.0; 512],
                    })
                    .unwrap()
                    .iter()
                    .any(|e| matches!(e, ListenEvent::PartialTranscript { .. }));
            }
            assert!(!partial);
        }
        e.set_source_policy("process:browser", PartialPolicy::Responsive).unwrap();
        assert_eq!(e.policy("process:browser"), PartialPolicy::Responsive);
        assert!(e.set_source_policy(&"x".repeat(257), PartialPolicy::Responsive).is_err());
    }
    #[test]
    fn model_swaps_preserve_every_active_source_and_final_only_decodes() {
        let mut e=ListenEngine::new(DenseAsr,Vad,Turn,ListenConfig::default()).unwrap();
        for tick in 0..938u64 {
            for source in ["mic", "mic:extra", "app:audio"] {
                let events=e.push(AudioChunk {source,generation:1,host_time_ns:tick*32_000_000,samples:&[1.;512]}).unwrap();
                assert!(!events.iter().any(|e| matches!(e,ListenEvent::PartialTranscript{..}|ListenEvent::Discontinuity{..})));
            }
            if tick%50==0 {
                let before: Vec<_>=e.sources.iter().map(|(id,s)|(id.clone(),s.start_ns,s.audio.len(),s.vad.hidden.clone())).collect();
                e.replace_models(ListenEngine::new(DenseAsr,Vad,Turn,ListenConfig::default()).unwrap());
                for (id,start,len,vad) in before {assert_eq!((e.sources[&id].start_ns,e.sources[&id].audio.len()),(start,len));assert_eq!(e.sources[&id].vad.hidden,vad);}
            }
        }
        for source in ["mic", "mic:extra", "app:audio"] { assert!(e.flush_source(source).unwrap().iter().any(|e| matches!(e,ListenEvent::FinalUtterance{..}))); }
        assert!(e.sources.is_empty());
    }
    #[test]
    fn eof_remainder_near_full_window_rolls_before_decode() {
        let mut e = ListenEngine::new(DenseAsr, Vad, Turn, ListenConfig::default()).unwrap();
        let samples = vec![1.0; 240127];
        let mut offset = 0;
        for chunk in samples.chunks(1000) {
            e.push(AudioChunk {
                source: "process:test",
                generation: 0,
                host_time_ns: offset * 62500,
                samples: chunk,
            })
            .unwrap();
            offset += chunk.len() as u64;
        }
        let events = e.flush_source("process:test").unwrap();
        assert!(events
            .iter()
            .any(|e| matches!(e, ListenEvent::TranscriptSegment { .. })));
        assert!(events
            .iter()
            .any(|e| matches!(e, ListenEvent::FinalUtterance { .. })));
    }
    #[test]
    fn oversize_audio_and_excess_sources_fail_without_allocating_state() {
        let mut e = ListenEngine::new(
            DenseAsr,
            Vad,
            Turn,
            ListenConfig {
                maximum_sources: 1,
                ..ListenConfig::default()
            },
        )
        .unwrap();
        assert!(e
            .push(AudioChunk {
                source: "mic",
                generation: 0,
                host_time_ns: 0,
                samples: &[1.0; 16001]
            })
            .is_err());
        assert!(e.sources.is_empty());
        e.push(AudioChunk {
            source: "mic",
            generation: 0,
            host_time_ns: 0,
            samples: &[1.0; 512],
        })
        .unwrap();
        assert!(e
            .push(AudioChunk {
                source: "other",
                generation: 0,
                host_time_ns: 0,
                samples: &[1.0; 512]
            })
            .is_err());
        assert_eq!(e.sources.len(), 1);
    }
}
