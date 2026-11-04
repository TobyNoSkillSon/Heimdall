pub mod screen_bench;
use anyhow::{ensure, Context, Result};
use listen::onnx::{Silero, VadState};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{fs, path::Path};

#[derive(Debug, Deserialize)]
pub struct Segment {
    pub path: String,
    pub frames: usize,
    pub overlap_frames: usize,
}
#[derive(Debug, Deserialize)]
pub struct Recording {
    pub id: String,
    pub audio_path: String,
    pub duration: f64,
    pub segments: Vec<Segment>,
    pub vella_text: String,
    pub matched_message: Option<Value>,
    pub has_image: Option<bool>,
    pub labels: Value,
}
/// Lossless reconstruction. request.wav caches only the last ASR request.
pub fn read_recording(r: &Recording) -> Result<Vec<f32>> {
    ensure!(
        r.duration.is_finite() && r.duration > 0.0 && r.duration <= 3600.0,
        "Invalid duration or one-hour memory bound exceeded"
    );
    let mut samples = Vec::new();
    for s in &r.segments {
        ensure!(s.overlap_frames <= s.frames, "Invalid overlap");
        let bytes = fs::read(&s.path).with_context(|| format!("Read segment {}", s.path))?;
        ensure!(
            bytes.len() == s.frames * 4,
            "Source length changed since manifest: {}",
            s.path
        );
        for b in bytes[s.overlap_frames * 4..].as_chunks::<4>().0 {
            let x = f32::from_le_bytes(*b);
            ensure!(x.is_finite(), "Nonfinite source sample");
            samples.push(x);
        }
    }
    ensure!(
        (samples.len() as f64 / 16000.0 - r.duration).abs() < 0.0001,
        "Duration mismatch"
    );
    Ok(samples)
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pause {
    pub start_ms: f64,
    pub end_ms: f64,
    pub done: bool,
}
/// Same hysteresis as listen; only quiet spans between speech are negatives.
pub fn pauses(vad: &mut Silero, audio: &[f32]) -> Result<Vec<Pause>> {
    let mut state = VadState::default();
    let mut voiced = false;
    let mut ever = false;
    let mut silence = None;
    let mut out = Vec::new();
    for (i, block) in audio.chunks(512).enumerate() {
        let mut padded = [0.0; 512];
        padded[..block.len()].copy_from_slice(block);
        let speech = vad.probability(&padded, &mut state)? >= if voiced { 0.35 } else { 0.5 };
        let start = i as f64 * 32.0;
        if speech {
            if let Some(from) = silence.take() {
                if start - from >= 300.0 {
                    out.push(Pause {
                        start_ms: from,
                        end_ms: start,
                        done: false,
                    });
                }
            }
            ever = true;
        } else if ever && silence.is_none() {
            silence = Some(start);
        }
        voiced = speech;
    }
    Ok(out)
}
pub fn words(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_owned)
        .collect()
}
/// Aggregate word-level Levenshtein edits/reference words, not per-file mean.
pub fn word_edits(reference: &str, hypothesis: &str) -> (usize, usize) {
    let a = words(reference);
    let b = words(hypothesis);
    let mut previous: Vec<usize> = (0..=b.len()).collect();
    for (i, x) in a.iter().enumerate() {
        let mut current = vec![i + 1; b.len() + 1];
        for (j, y) in b.iter().enumerate() {
            current[j + 1] = (previous[j] + usize::from(x != y))
                .min(previous[j + 1] + 1)
                .min(current[j] + 1);
        }
        previous = current;
    }
    (previous[b.len()], a.len())
}
#[derive(Default, Debug, Serialize)]
pub struct Confusion {
    pub true_positive: usize,
    pub false_positive: usize,
    pub false_negative: usize,
    pub true_negative: usize,
    pub unknown_label: usize,
    pub unavailable_prediction: usize,
}
impl Confusion {
    pub fn add(&mut self, label: Option<bool>, prediction: Option<bool>) {
        match (label, prediction) {
            (None, _) => self.unknown_label += 1,
            (Some(_), None) => self.unavailable_prediction += 1,
            (Some(true), Some(true)) => self.true_positive += 1,
            (Some(false), Some(true)) => self.false_positive += 1,
            (Some(true), Some(false)) => self.false_negative += 1,
            (Some(false), Some(false)) => self.true_negative += 1,
        }
    }
    pub fn precision(&self) -> Option<f64> {
        ratio(self.true_positive, self.true_positive + self.false_positive)
    }
    pub fn recall(&self) -> Option<f64> {
        ratio(self.true_positive, self.true_positive + self.false_negative)
    }
}
pub fn ratio(a: usize, b: usize) -> Option<f64> {
    if b == 0 {
        None
    } else {
        Some(a as f64 / b as f64)
    }
}
pub fn load_manifest(path: &Path) -> Result<Vec<Value>> {
    fs::read_to_string(path)?
        .lines()
        .filter(|s| !s.trim().is_empty())
        .map(|s| serde_json::from_str(s).map_err(Into::into))
        .collect()
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn wer_unicode_insertions_and_empty_reference() {
        assert_eq!(
            word_edits("Zobacz TO! Łódź 12", "zobacz to łódź twelve now"),
            (2, 4)
        );
        assert_eq!(word_edits("", "hello"), (1, 0));
        assert_eq!(word_edits("one two three", "one three"), (1, 3));
    }
    #[test]
    fn unknown_is_not_negative() {
        let mut c = Confusion::default();
        c.add(None, Some(true));
        c.add(Some(true), None);
        assert_eq!(c.false_positive, 0);
        assert_eq!(c.unknown_label, 1);
        assert_eq!(c.unavailable_prediction, 1);
        assert!(c.precision().is_none());
        c.add(Some(true), Some(true));
        c.add(Some(false), Some(true));
        c.add(Some(true), Some(false));
        assert_eq!(c.precision(), Some(0.5));
        assert_eq!(c.recall(), Some(0.5));
    }
    #[test]
    fn reconstruct_overlapping_float_pcm() {
        let dir = std::env::temp_dir().join(format!("heimdall-eval-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("segment.pcm");
        fs::write(
            &p,
            [1.0_f32, 2.0, 3.0]
                .iter()
                .flat_map(|x| x.to_le_bytes())
                .collect::<Vec<_>>(),
        )
        .unwrap();
        let r = Recording {
            id: "x".into(),
            audio_path: dir.to_string_lossy().into(),
            duration: 2.0 / 16000.0,
            segments: vec![Segment {
                path: p.to_string_lossy().into(),
                frames: 3,
                overlap_frames: 1,
            }],
            vella_text: "".into(),
            matched_message: None,
            has_image: None,
            labels: Value::Null,
        };
        assert_eq!(read_recording(&r).unwrap(), vec![2.0, 3.0]);
        assert_eq!(fs::read(&p).unwrap().len(), 12);
        fs::remove_dir_all(dir).unwrap();
    }
}

/// All private replay/label output stays below lab/eval/data. Check the existing
/// ancestor before creating directories, so even an output symlink cannot write
/// into Vella's immutable store or any other project/profile.
pub fn prepare_output(root: &Path, output: &Path) -> Result<std::path::PathBuf> {
    let base = root.join("lab/eval/data");
    ensure!(
        !fs::symlink_metadata(&base)?.file_type().is_symlink(),
        "lab/eval/data must not be a symlink"
    );
    let base = fs::canonicalize(base)?;
    let absolute = if output.is_absolute() {
        output.to_owned()
    } else {
        std::env::current_dir()?.join(output)
    };
    let ancestor = absolute
        .ancestors()
        .find(|p| p.exists())
        .context("No existing output ancestor")?;
    ensure!(
        fs::canonicalize(ancestor)?.starts_with(&base),
        "Private outputs must stay under lab/eval/data"
    );
    fs::create_dir_all(&absolute)?;
    let canonical = fs::canonicalize(absolute)?;
    ensure!(
        canonical.starts_with(base),
        "Output resolves outside lab/eval/data"
    );
    Ok(canonical)
}
#[cfg(test)]
mod output_tests {
    use super::*;
    #[test]
    fn reject_outside_output_before_any_write() {
        let root =
            std::env::temp_dir().join(format!("heimdall-eval-output-{}", std::process::id()));
        fs::create_dir_all(root.join("lab/eval/data")).unwrap();
        let forbidden = root.join("Recordings/new");
        assert!(prepare_output(&root, &forbidden).is_err());
        assert!(!forbidden.exists());
        let allowed = root.join("lab/eval/data/new");
        assert_eq!(
            prepare_output(&root, &allowed).unwrap(),
            fs::canonicalize(&allowed).unwrap()
        );
        #[cfg(unix)]
        {
            let link = root.join("lab/eval/data/escape");
            std::os::unix::fs::symlink(&root, &link).unwrap();
            assert!(prepare_output(&root, &link.join("forbidden")).is_err());
            assert!(!root.join("forbidden").exists());
        }
        fs::remove_dir_all(root).unwrap();
    }
}
