//! Explicit live strategy selection. No silent fallback or completed-file model in a live slot.
use crate::{
    diar::Diarizer,
    online::{OnlineConfig, OnlineDiarizer},
    pipeline::Diarization,
};
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LiveStrategy {
    #[default]
    Nemotron,
    Sortformer,
    Embedding,
    Hybrid,
}
impl LiveStrategy {
    /// Best measured alternative for calls with at most four participants.
    /// Validate the participant count before selecting it; no automatic fallback.
    pub const RUNNER_UP: Self = Self::Sortformer;

    pub fn capacity(self) -> usize {
        match self {
            Self::Sortformer => 4,
            _ => 8,
        }
    }
    pub fn validate_participants(self, participants: usize) -> Result<()> {
        anyhow::ensure!(
            (2..=self.capacity()).contains(&participants),
            "{self:?} supports 2–{} participants; requested {participants}",
            self.capacity()
        );
        Ok(())
    }
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "nemotron" => Ok(Self::Nemotron),
            "sortformer" => Ok(Self::Sortformer),
            "embedding" => Ok(Self::Embedding),
            "hybrid" => Ok(Self::Hybrid),
            _ => bail!("unknown live diarization strategy: {s}"),
        }
    }
    /// `models` is the speakers model directory; models are loaded on this thread.
    pub fn load(
        self,
        models: &Path,
        gpu: i32,
        config: OnlineConfig,
    ) -> Result<Box<dyn Diarization>> {
        self.load_enrolled(models, gpu, config, None)
    }
    /// Supply confirmed voiceprints before the call. Anonymous cluster IDs start after
    /// enrolled names in BTreeMap order; unknown people retain separate new-speaker IDs.
    pub fn load_enrolled(
        self,
        models: &Path,
        gpu: i32,
        config: OnlineConfig,
        store: Option<&crate::identity::EnrollmentStore>,
    ) -> Result<Box<dyn Diarization>> {
        match self {
            Self::Nemotron => Ok(Box::new(
                Diarizer::load(&models.join("Nemotron-3-Diarization.q8_0.gguf"), gpu)?
                    .stream(16000)?,
            )),
            Self::Embedding => {
                let mut stream = OnlineDiarizer::load(models, config, None)?;
                if let Some(store) = store {
                    stream.enroll(store)?;
                }
                Ok(Box::new(stream))
            }
            Self::Hybrid => {
                let stream = Diarizer::load(&models.join("Nemotron-3-Diarization.q8_0.gguf"), gpu)?
                    .stream(16000)?;
                let mut stream = OnlineDiarizer::load(models, config, Some(stream))?;
                if let Some(store) = store {
                    stream.enroll(store)?;
                }
                Ok(Box::new(stream))
            }
            Self::Sortformer => {
                #[cfg(target_os = "macos")]
                {
                    Ok(Box::new(crate::sortformer::Sortformer::load(
                        &models.join("Sortformer_v2.1.mlmodelc"),
                    )?))
                }
                #[cfg(not(target_os = "macos"))]
                {
                    bail!("Sortformer Core ML requires macOS")
                }
            }
        }
    }
}
impl<T: Diarization + ?Sized> Diarization for Box<T> {
    fn take_finalized_segments(&self) -> Result<Vec<crate::diar::SpeakerSegment>> { (**self).take_finalized_segments() }
    fn frame_probabilities(&self,since:i64) -> Result<Option<crate::diar::FrameProbabilities>> { (**self).frame_probabilities(since) }
    fn output_revision(&self) -> i64 { (**self).output_revision() }
    fn stable_frames(&self) -> i64 { (**self).stable_frames() }
    fn sample_rate(&self) -> u32 {
        (**self).sample_rate()
    }
    fn speakers(&self) -> usize {
        (**self).speakers()
    }
    fn seconds_per_frame(&self) -> f64 {
        (**self).seconds_per_frame()
    }
    fn frame_count(&self) -> i64 {
        (**self).frame_count()
    }
    fn push_f32(&mut self, a: &[f32]) -> Result<()> {
        (**self).push_f32(a)
    }
    fn finish(&mut self) -> Result<()> {
        (**self).finish()
    }
    fn segments(&self) -> Result<Vec<crate::diar::SpeakerSegment>> {
        (**self).segments()
    }
}
