//! Causal embedding windows and bounded online clusters. No reference boundaries at inference.
use crate::{
    diar::{DiarStream, SpeakerSegment},
    embedding::{Embedding, ReDimNet},
    identity::EnrollmentStore,
    pipeline::Diarization,
};
use anyhow::{ensure, Result};
use ort::{session::Session, value::Tensor};
use serde::{Deserialize, Serialize};
use std::{collections::VecDeque, path::Path};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OnlineConfig {
    pub window_seconds: f64,
    pub hop_seconds: f64,
    pub cosine_threshold: f32,
    pub max_speakers: usize,
}
impl Default for OnlineConfig {
    fn default() -> Self {
        Self {
            window_seconds: 3.,
            hop_seconds: 0.5,
            cosine_threshold: 0.43815252,
            max_speakers: 8,
        }
    }
}
impl OnlineConfig {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.window_seconds.is_finite() && (1.5..=3.).contains(&self.window_seconds),
            "window must be 1.5–3 seconds"
        );
        ensure!(
            self.hop_seconds.is_finite()
                && self.hop_seconds >= 0.1
                && self.hop_seconds <= self.window_seconds,
            "invalid hop"
        );
        ensure!(
            self.cosine_threshold.is_finite() && (-1.0..=1.0).contains(&self.cosine_threshold),
            "invalid cosine threshold"
        );
        ensure!((2..=8).contains(&self.max_speakers), "capacity must be 2–8");
        Ok(())
    }
}
struct Vad {
    session: Session,
    hidden: Vec<f32>,
    context: [f32; 64],
}
impl Vad {
    fn load(path: &Path) -> Result<Self> {
        Ok(Self {
            session: Session::builder()?
                .with_intra_threads(1)
                .map_err(|e| anyhow::anyhow!(e.to_string()))?
                .with_inter_threads(1)
                .map_err(|e| anyhow::anyhow!(e.to_string()))?
                .with_intra_op_spinning(false)
                .map_err(|e| anyhow::anyhow!(e.to_string()))?
                .with_inter_op_spinning(false)
                .map_err(|e| anyhow::anyhow!(e.to_string()))?
                .with_no_environment_execution_providers()
                .map_err(|e| anyhow::anyhow!(e.to_string()))?
                .commit_from_file(path)?,
            hidden: vec![0.; 256],
            context: [0.; 64],
        })
    }
    fn probability(&mut self, audio: &[f32]) -> Result<f32> {
        ensure!(audio.len() == 512, "VAD frame must have 512 samples");
        let mut input = self.context.to_vec();
        input.extend_from_slice(audio);
        let output=self.session.run(ort::inputs!["input"=>Tensor::from_array(([1,576],input))?, "state"=>Tensor::from_array(([2,1,128],self.hidden.clone()))?, "sr"=>Tensor::from_array((Vec::<usize>::new(),vec![16000_i64]))?])?;
        let (_, p) = output["output"].try_extract_tensor::<f32>()?;
        let (_, h) = output["stateN"].try_extract_tensor::<f32>()?;
        ensure!(
            p.len() == 1 && h.len() == 256 && p[0].is_finite(),
            "invalid VAD output"
        );
        self.hidden.copy_from_slice(h);
        self.context.copy_from_slice(&audio[448..]);
        Ok(p[0])
    }
}
#[derive(Default)]
pub struct OnlineClusters {
    exemplars: Vec<Vec<Embedding>>,
    pinned: usize,
}
impl OnlineClusters {
    pub fn from_enrollments(
        store: &EnrollmentStore,
        capacity: usize,
    ) -> Result<(Self, Vec<String>)> {
        ensure!(
            store.names.len() <= capacity,
            "enrolled people exceed live capacity"
        );
        let names: Vec<_> = store.names.keys().cloned().collect();
        Ok((
            Self {
                exemplars: store.names.values().map(|e| e.embeddings.clone()).collect(),
                pinned: names.len(),
            },
            names,
        ))
    }
    pub fn assign(&mut self, e: Embedding, threshold: f32, capacity: usize) -> Option<i32> {
        let best = self
            .exemplars
            .iter()
            .enumerate()
            .map(|(i, es)| (i, es.iter().map(|x| x.cosine(&e)).fold(-1_f32, f32::max)))
            .max_by(|a, b| a.1.total_cmp(&b.1));
        if let Some((i, score)) = best.filter(|(_, s)| *s >= threshold) {
            // Enrolled identities never learn from unchecked mixture windows. Anonymous
            // clusters keep a bounded bank and update only on strong matches.
            if i >= self.pinned && score >= 0.7 && self.exemplars[i].len() < 4 {
                self.exemplars[i].push(e);
            }
            return Some(i as i32 + 1);
        }
        if self.exemplars.len() < capacity {
            self.exemplars.push(vec![e]);
            Some(self.exemplars.len() as i32)
        } else {
            None
        }
    }
}
pub struct OnlineDiarizer {
    config: OnlineConfig,
    encoder: ReDimNet,
    vad: Vad,
    native: Option<DiarStream>,
    audio: VecDeque<f32>,
    start: usize,
    total: usize,
    vad_next: usize,
    masks: VecDeque<bool>,
    mask_start: usize,
    next_end: usize,
    committed: usize,
    clusters: OnlineClusters,
    segments: Vec<SpeakerSegment>,
    finished: bool,
}
impl OnlineDiarizer {
    pub fn load(models: &Path, config: OnlineConfig, native: Option<DiarStream>) -> Result<Self> {
        config.validate()?;
        let window = (config.window_seconds * 16000.).round() as usize;
        Ok(Self {
            config,
            encoder: ReDimNet::load(&models.join("ReDimNet2B6.onnx"))?,
            vad: Vad::load(&models.join("../silero/silero_vad.onnx"))?,
            native,
            audio: VecDeque::new(),
            start: 0,
            total: 0,
            vad_next: 0,
            masks: VecDeque::new(),
            mask_start: 0,
            next_end: window,
            committed: 0,
            clusters: OnlineClusters::default(),
            segments: vec![],
            finished: false,
        })
    }
    pub fn enroll(&mut self, store: &EnrollmentStore) -> Result<Vec<String>> {
        ensure!(self.total == 0, "enroll before feeding audio");
        let (c, n) = OnlineClusters::from_enrollments(store, self.config.max_speakers)?;
        self.clusters = c;
        Ok(n)
    }
    fn samples(&self, a: usize, b: usize) -> Vec<f32> {
        (a..b).map(|i| self.audio[i - self.start]).collect()
    }
    fn process(&mut self, flush: bool) -> Result<()> {
        while self.vad_next + 512 <= self.total {
            let x = self.samples(self.vad_next, self.vad_next + 512);
            self.masks.push_back(self.vad.probability(&x)? >= 0.5);
            self.vad_next += 512;
        }
        let window = (self.config.window_seconds * 16000.).round() as usize;
        let hop = (self.config.hop_seconds * 16000.).round() as usize;
        while self.next_end <= self.vad_next || (flush && self.committed < self.total) {
            let end = self.next_end.min(self.total);
            let begin = end.saturating_sub(window);
            let target_end = if flush && self.next_end > self.total {
                self.total
            } else {
                end.saturating_sub((window - hop) / 2)
            };
            if target_end <= self.committed {
                break;
            }
            let native = self.native.as_ref().map(|s| s.segments()).transpose()?;
            let speech = |i: usize| -> bool {
                if let Some(s) = &native {
                    let t = i as f64 / 16000.;
                    s.iter()
                        .filter(|s| s.start_time <= t && t < s.end_time)
                        .count()
                        == 1
                } else {
                    let frame = i / 512;
                    frame >= self.mask_start
                        && self
                            .masks
                            .get(frame - self.mask_start)
                            .copied()
                            .unwrap_or(false)
                }
            };
            let clean: Vec<_> = (begin..end)
                .filter(|i| speech(*i))
                .map(|i| self.audio[i - self.start])
                .collect();
            let who = if clean.len() >= 8000 {
                let e = self.encoder.embed_short(&clean)?;
                self.clusters
                    .assign(e, self.config.cosine_threshold, self.config.max_speakers)
            } else {
                None
            };
            // VAD supplies boundaries; assign only the central hop (first window also
            // covers its leading edge). Future windows cannot revise committed labels.
            for a in (self.committed..target_end).step_by(160) {
                let b = (a + 160).min(target_end);
                if speech((a + b) / 2) {
                    if let Some(s) = who {
                        append_segment(&mut self.segments, a as f64 / 16000., b as f64 / 16000., s);
                    }
                }
            }
            self.committed = target_end;
            self.next_end += hop;
            if self.next_end > self.total && !flush {
                break;
            }
        }
        let keep = self
            .committed
            .saturating_sub(window + 512)
            .min(self.vad_next);
        if keep > self.start {
            self.audio.drain(..keep - self.start);
            self.start = keep;
        }
        let frames = keep / 512;
        if frames > self.mask_start {
            self.masks
                .drain(..(frames - self.mask_start).min(self.masks.len()));
            self.mask_start = frames;
        }
        Ok(())
    }
}
pub(crate) fn append_segment(out: &mut Vec<SpeakerSegment>, a: f64, b: f64, s: i32) {
    if let Some(last) = out
        .iter_mut()
        .rev()
        .find(|x| x.speaker == s)
        .filter(|x| (x.end_time - a).abs() < 1e-7)
    {
        last.end_time = b;
    } else {
        out.push(SpeakerSegment {
            start_time: a,
            end_time: b,
            speaker: s,
        });
    }
}
impl Diarization for OnlineDiarizer {
    fn sample_rate(&self) -> u32 {
        16000
    }
    fn speakers(&self) -> usize {
        self.config.max_speakers
    }
    fn seconds_per_frame(&self) -> f64 {
        0.01
    }
    fn frame_count(&self) -> i64 {
        (self.committed / 160) as i64
    }
    fn push_f32(&mut self, a: &[f32]) -> Result<()> {
        ensure!(
            !self.finished && a.iter().all(|x| x.is_finite()),
            "finished stream or nonfinite PCM"
        );
        for chunk in a.chunks(512) {
            if let Some(s) = &mut self.native {
                s.push_f32(chunk)?;
            }
            self.audio.extend(chunk.iter().copied());
            self.total += chunk.len();
            self.process(false)?;
        }
        Ok(())
    }
    fn finish(&mut self) -> Result<()> {
        if !self.finished {
            if let Some(s) = &mut self.native {
                s.finish()?;
            }
            self.process(true)?;
            self.finished = true;
        }
        Ok(())
    }
    fn segments(&self) -> Result<Vec<SpeakerSegment>> {
        Ok(self.segments.clone())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn e(i: usize) -> Embedding {
        let mut v = vec![0.; 192];
        v[i] = 1.;
        Embedding::new(v).unwrap()
    }
    #[test]
    fn unknown_capacity_and_strong_updates() {
        let mut c = OnlineClusters::default();
        assert_eq!(c.assign(e(0), 0.5, 2), Some(1));
        assert_eq!(c.assign(e(0), 0.5, 2), Some(1));
        assert_eq!(c.assign(e(1), 0.5, 2), Some(2));
        assert_eq!(c.assign(e(2), 0.5, 2), None);
    }
    #[test]
    fn config_rejects_nan_and_invalid_window() {
        let mut c = OnlineConfig::default();
        assert!(c.validate().is_ok());
        c.window_seconds = f64::NAN;
        assert!(c.validate().is_err());
    }
}
