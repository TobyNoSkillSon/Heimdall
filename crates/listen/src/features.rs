//! Smart Turn's exact eight-second Whisper frontend. No ASR frontend reuse.
use rustfft::{num_complex::Complex32, Fft, FftPlanner};
use std::sync::Arc;

pub struct WhisperFeatures {
    fft: Arc<dyn Fft<f32>>,
    window: Vec<f32>,
    // A triangular Slaney band has one contiguous nonzero frequency span.
    // Retain its original ascending-bin reduction order and f64 arithmetic.
    filters: Vec<(usize, Vec<f32>)>,
}
fn hz_mel(hz: f64) -> f64 {
    if hz < 1000.0 {
        hz / (200.0 / 3.0)
    } else {
        15.0 + (hz / 1000.0).ln() / (6.4_f64.ln() / 27.0)
    }
}
fn mel_hz(mel: f64) -> f64 {
    if mel < 15.0 {
        mel * (200.0 / 3.0)
    } else {
        1000.0 * ((mel - 15.0) * (6.4_f64.ln() / 27.0)).exp()
    }
}
impl Default for WhisperFeatures {
    fn default() -> Self {
        let mut planner = FftPlanner::new();
        let points: Vec<f64> = (0..82)
            .map(|i| mel_hz(hz_mel(8000.0) * i as f64 / 81.0))
            .collect();
        let mut filters = vec![0.0; 80 * 201];
        for m in 0..80 {
            for k in 0..201 {
                let hz = k as f64 * 40.0;
                let weight = ((hz - points[m]) / (points[m + 1] - points[m]))
                    .min((points[m + 2] - hz) / (points[m + 2] - points[m + 1]))
                    .max(0.0);
                filters[m * 201 + k] = (weight * 2.0 / (points[m + 2] - points[m])) as f32;
            }
        }
        Self {
            fft: planner.plan_fft_forward(400),
            window: (0..400)
                .map(|i| (0.5 - 0.5 * (std::f64::consts::TAU * i as f64 / 400.0).cos()) as f32)
                .collect(),
            filters: filters
                .as_chunks::<201>()
                .0
                .iter()
                .map(|band| {
                    let start = band.iter().position(|v| *v != 0.).unwrap();
                    let end = band.iter().rposition(|v| *v != 0.).unwrap() + 1;
                    (start, band[start..end].to_vec())
                })
                .collect(),
        }
    }
}
impl WhisperFeatures {
    /// Last 128000 samples, left-pad *before* normalization; [80,800] row major.
    pub fn extract(&self, audio: &[f32]) -> Vec<f32> {
        let tail = &audio[audio.len().saturating_sub(128000)..];
        let mut x = vec![0_f32; 128000];
        x[128000 - tail.len()..].copy_from_slice(tail);
        // numpy performs float32 mean/variance; f64 accumulation minimizes reduction error.
        let mean = (x.iter().map(|&v| v as f64).sum::<f64>() / x.len() as f64) as f32;
        let var = (x
            .iter()
            .map(|&v| ((v - mean) * (v - mean)) as f64)
            .sum::<f64>()
            / x.len() as f64) as f32;
        let scale = (var + 1e-7).sqrt();
        for v in &mut x {
            *v = (*v - mean) / scale;
        }
        let mut out = vec![0.0; 80 * 800];
        let mut frame = vec![Complex32::default(); 400];
        let mut scratch = vec![Complex32::default(); self.fft.get_inplace_scratch_len()];
        let mut power = vec![0_f32; 201];
        for t in 0..800 {
            for (i, v) in frame.iter_mut().enumerate() {
                let pos = t as isize * 160 + i as isize - 200;
                let index = if pos < 0 {
                    -pos
                } else if pos >= 128000 {
                    255998 - pos
                } else {
                    pos
                } as usize;
                *v = Complex32::new(x[index] * self.window[i], 0.0);
            }
            self.fft.process_with_scratch(&mut frame, &mut scratch);
            for k in 0..201 {
                power[k] = frame[k].norm_sqr();
            }
            for m in 0..80 {
                let (start, weights) = &self.filters[m];
                let sum: f64 = power[*start..*start + weights.len()]
                    .iter()
                    .zip(weights)
                    .map(|(&p, &weight)| p as f64 * weight as f64)
                    .sum();
                out[m * 800 + t] = sum.max(1e-10).log10() as f32;
            }
        }
        let max = out.iter().copied().fold(f32::NEG_INFINITY, f32::max);
        for v in &mut out {
            *v = (v.max(max - 8.0) + 4.0) / 4.0;
        }
        out
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "needs the reference tensor fixtures (.bin) that are not in the public repository"]
    fn reference_tensors() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
        let read = |p: std::path::PathBuf| {
            let bytes = std::fs::read(p).unwrap();
            assert_eq!(bytes.len() % 4, 0, "Truncated f32 fixture");
            bytes
                .as_chunks::<4>()
                .0
                .iter()
                .map(|b| f32::from_le_bytes(*b))
                .collect::<Vec<_>>()
        };
        let frontend = WhisperFeatures::default();
        for name in ["silence", "sine", "boundary", "speech"] {
            let audio = read(root.join(format!("features-{name}-audio.bin")));
            let reference = read(root.join(format!("features-{name}-expected.bin")));
            let actual = frontend.extract(&audio);
            assert_eq!(actual.len(), 80 * 800);
            assert_eq!(
                reference.len(),
                actual.len(),
                "Incomplete reference: {name}"
            );
            let max = actual
                .iter()
                .zip(&reference)
                .map(|(a, b)| (a - b).abs())
                .fold(0_f32, f32::max);
            let rms = (actual
                .iter()
                .zip(&reference)
                .map(|(a, b)| (a - b).powi(2) as f64)
                .sum::<f64>()
                / actual.len() as f64)
                .sqrt();
            println!("Whisper {name}: max_abs={max}, rms={rms}");
            assert!(
                max < 0.0001 && rms < 0.00001,
                "Whisper frontend reference mismatch: {name}"
            );
        }
    }
    #[test]
    fn silence_and_window() {
        let f = WhisperFeatures::default();
        let s = f.extract(&[]);
        assert_eq!(s.len(), 64000);
        assert!(s.iter().all(|v| (*v + 1.5).abs() < 1e-6));
        let x: Vec<_> = (0..128001).map(|i| (i as f32 * 0.1).sin()).collect();
        assert_eq!(f.extract(&x), f.extract(&x[1..]));
    }
}
