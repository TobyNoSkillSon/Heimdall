//! FluidInference Sortformer v2.1 FP16 Core ML; causal mel, 0.48s chunks + 0.56s right context.
//! State/cache compression follows FluidAudio e560c6445 (Apache-2.0), itself NeMo-derived.
use crate::{
    coreml::{Array, Model},
    diar::SpeakerSegment,
    online::append_segment,
    pipeline::Diarization,
};
use anyhow::{ensure, Result};
use objc2_core_ml::{MLComputeUnits, MLMultiArrayDataType};
use rustfft::{num_complex::Complex, Fft, FftPlanner};
use std::{collections::VecDeque, path::Path, sync::Arc};

struct Mel {
    fft: Arc<dyn Fft<f32>>,
    window: Vec<f32>,
    filters: Vec<Vec<f32>>,
    samples: VecDeque<f32>,
    start: usize,
    total: usize,
    next: usize,
}
impl Mel {
    fn new() -> Self {
        fn hz_mel(h: f32) -> f32 {
            if h < 1000. {
                h / (200. / 3.)
            } else {
                15. + (h / 1000.).ln() / (6.4_f32.ln() / 27.)
            }
        }
        fn mel_hz(m: f32) -> f32 {
            if m < 15. {
                m * (200. / 3.)
            } else {
                1000. * ((m - 15.) * (6.4_f32.ln() / 27.)).exp()
            }
        }
        let points: Vec<_> = (0..130)
            .map(|i| mel_hz(hz_mel(8000.) * i as f32 / 129.))
            .collect();
        let filters = (0..128)
            .map(|m| {
                (0..257)
                    .map(|k| {
                        let hz = k as f32 * 16000. / 512.;
                        let (a, b, c) = (points[m], points[m + 1], points[m + 2]);
                        ((hz - a) / (b - a)).min((c - hz) / (c - b)).max(0.) * 2. / (c - a)
                    })
                    .collect()
            })
            .collect();
        Self {
            fft: FftPlanner::new().plan_fft_forward(512),
            window: (0..400)
                .map(|i| 0.5 - 0.5 * (2. * std::f32::consts::PI * i as f32 / 399.).cos())
                .collect(),
            filters,
            samples: VecDeque::new(),
            start: 0,
            total: 0,
            next: 0,
        }
    }
    fn push(&mut self, a: &[f32], flush: bool) -> Vec<Vec<f32>> {
        self.samples.extend(a.iter().copied());
        self.total += a.len();
        let mut result = vec![];
        // Centered 400-sample Hann inside a 512-point FFT. Never inspect future audio.
        while if flush {
            self.next < self.total.div_ceil(160)
        } else {
            self.next * 160 + 200 <= self.total
        } {
            let mut x = vec![Complex::new(0., 0.); 512];
            for i in 0..400 {
                let n = self.next as i64 * 160 + i as i64 - 200;
                if n >= 0 && (n as usize) < self.total {
                    let n = n as usize;
                    let current = self.samples[n - self.start];
                    let previous = if n == 0 {
                        0.
                    } else {
                        self.samples[n - 1 - self.start]
                    };
                    x[i + 56].re = (current - 0.97 * previous) * self.window[i];
                }
            }
            self.fft.process(&mut x);
            let power: Vec<_> = x[..257].iter().map(|x| x.norm_sqr()).collect();
            result.push(
                self.filters
                    .iter()
                    .map(|f| {
                        (f.iter().zip(&power).map(|(a, b)| a * b).sum::<f32>() + 2_f32.powi(-24))
                            .ln()
                    })
                    .collect(),
            );
            self.next += 1;
        }
        let keep = (self.next * 160).saturating_sub(201).min(self.total);
        if keep > self.start {
            self.samples.drain(..keep - self.start);
            self.start = keep;
        }
        result
    }
}
#[derive(Default)]
struct State {
    cache: Vec<f32>,
    fifo: Vec<f32>,
    cache_preds: Option<Vec<f32>>,
    silence: Vec<f32>,
    sil_count: usize,
}
impl State {
    fn update(&mut self, emb: &[f32], preds: &[f32], lc: usize, rc: usize) -> Result<Vec<f32>> {
        let cache_len = self.cache.len() / 512;
        let fifo_len = self.fifo.len() / 512;
        ensure!(
            emb.len() / 512 >= lc + rc,
            "invalid Sortformer context length"
        );
        let n = emb.len() / 512 - lc - rc;
        let chunk_start = cache_len + fifo_len + lc;
        ensure!(
            preds.len() >= (chunk_start + n + rc) * 4,
            "invalid Sortformer predictions"
        );
        let core = preds[chunk_start * 4..(chunk_start + n) * 4].to_vec();
        let mut fp = preds[cache_len * 4..(cache_len + fifo_len) * 4].to_vec();
        fp.extend(&core);
        self.fifo.extend_from_slice(&emb[lc * 512..(lc + n) * 512]);
        let context = fifo_len + n;
        if context > 40 {
            let pop = 31.max(context - 40).min(context);
            if self.silence.is_empty() {
                self.silence = vec![0.; 512];
            }
            for i in 0..pop {
                if fp[i * 4..i * 4 + 4].iter().sum::<f32>() < 0.2 {
                    for d in 0..512 {
                        self.silence[d] = (self.silence[d] * self.sil_count as f32
                            + self.fifo[i * 512 + d])
                            / (self.sil_count + 1) as f32;
                    }
                    self.sil_count += 1;
                }
            }
            self.cache.extend(self.fifo.drain(..pop * 512));
            if let Some(cp) = &mut self.cache_preds {
                cp.extend_from_slice(&fp[..pop * 4]);
            }
            if self.cache.len() / 512 > 188 {
                if self.cache_preds.is_none() {
                    let mut cp = preds[..cache_len * 4].to_vec();
                    cp.extend_from_slice(&fp[..pop * 4]);
                    self.cache_preds = Some(cp);
                }
                self.compress();
            }
        }
        Ok(core)
    }
    fn compress(&mut self) {
        let cp = self.cache_preds.as_ref().unwrap();
        let n = self.cache.len() / 512;
        let mut scores = vec![f32::NEG_INFINITY; n * 4];
        for i in 0..n {
            let log1: Vec<_> = cp[i * 4..i * 4 + 4]
                .iter()
                .map(|p| (-p.min(0.75)).ln_1p())
                .collect();
            for s in 0..4 {
                let p = cp[i * 4 + s];
                scores[i * 4 + s] =
                    p.max(0.25).ln() - log1[s] + log1.iter().sum::<f32>() + 2_f32.ln();
            }
        }
        for s in 0..4 {
            let positive = (0..n)
                .filter(|i| cp[i * 4 + s] > 0.5 && scores[i * 4 + s] > 0.)
                .count();
            for i in 0..n {
                if cp[i * 4 + s] <= 0.5 || (scores[i * 4 + s] <= 0. && positive >= 22) {
                    scores[i * 4 + s] = f32::NEG_INFINITY;
                }
                if i >= 188 {
                    scores[i * 4 + s] += 0.05;
                }
            }
            for (k, scale) in [(33, 2.), (66, 1.)] {
                let mut ids: Vec<_> = (0..n).collect();
                ids.sort_by(|a, b| {
                    scores[b * 4 + s]
                        .total_cmp(&scores[a * 4 + s])
                        .then(a.cmp(b))
                });
                for i in ids.into_iter().take(k) {
                    scores[i * 4 + s] += scale * 2_f32.ln();
                }
            }
        }
        // Top-k in speaker-major order; duplicate frame indices are intentional.
        let mut entries: Vec<_> = (0..4)
            .flat_map(|s| {
                let scores = &scores;
                (0..n + 3).map(move |i| {
                    (
                        s * (n + 3) + i,
                        if i >= n {
                            f32::INFINITY
                        } else {
                            scores[i * 4 + s]
                        },
                    )
                })
            })
            .collect();
        entries.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        entries.truncate(188);
        for e in &mut entries {
            if e.1 == f32::NEG_INFINITY {
                e.0 = 99999;
            }
        }
        entries.sort_by_key(|e| e.0);
        let mut cache = Vec::with_capacity(188 * 512);
        let mut p = Vec::with_capacity(188 * 4);
        for (id, _) in entries {
            let i = id % (n + 3);
            if id == 99999 || i >= n {
                cache.extend(&self.silence);
                p.extend([0.; 4]);
            } else {
                cache.extend_from_slice(&self.cache[i * 512..i * 512 + 512]);
                p.extend_from_slice(&cp[i * 4..i * 4 + 4]);
            }
        }
        self.cache = cache;
        self.cache_preds = Some(p);
    }
}
pub struct Sortformer {
    model: Model,
    mel: Mel,
    state: State,
    features: VecDeque<Vec<f32>>,
    feature_start: usize,
    next: usize,
    chunk: Array,
    chunk_len: Array,
    cache: Array,
    cache_len: Array,
    fifo: Array,
    fifo_len: Array,
    frames: usize,
    segments: Vec<SpeakerSegment>,
    total: usize,
    finished: bool,
}
#[allow(deprecated)]
fn flat(a: &Array) -> Vec<f32> {
    let shape = a.shape();
    let strides = a.strides();
    let mut result = Vec::with_capacity(a.count());
    for mut i in 0..a.count() {
        let mut off = 0;
        for axis in (0..shape.len()).rev() {
            off += (i % shape[axis]) * strides[axis];
            i /= shape[axis];
        }
        let v = unsafe {
            match a.inner.dataType() {
                MLMultiArrayDataType::Float32 => {
                    *a.inner.dataPointer().as_ptr().cast::<f32>().add(off)
                }
                MLMultiArrayDataType::Int32 => {
                    *a.inner.dataPointer().as_ptr().cast::<i32>().add(off) as f32
                }
                _ => unreachable!(),
            }
        };
        result.push(v);
    }
    result
}
impl Sortformer {
    pub fn load(path: &Path) -> Result<Self> {
        // Every compiled file is pinned by the fetched model manifest, verified before inference.
        let manifest: Vec<crate::artifacts::Asset> =
            serde_json::from_str(include_str!("../scripts/sortformer-manifest.json"))?;
        for a in manifest {
            crate::artifacts::verify(&path.parent().unwrap().join(&a.path), a.bytes, &a.sha256)?;
        }
        let f = MLMultiArrayDataType::Float32;
        let i = MLMultiArrayDataType::Int32;
        Ok(Self {
            model: Model::load(path, MLComputeUnits::CPUAndGPU)?,
            mel: Mel::new(),
            state: State::default(),
            features: VecDeque::new(),
            feature_start: 0,
            next: 0,
            chunk: Array::zeros(&[1, 112, 128], f)?,
            chunk_len: Array::zeros(&[1], i)?,
            cache: Array::zeros(&[1, 188, 512], f)?,
            cache_len: Array::zeros(&[1], i)?,
            fifo: Array::zeros(&[1, 40, 512], f)?,
            fifo_len: Array::zeros(&[1], i)?,
            frames: 0,
            segments: vec![],
            total: 0,
            finished: false,
        })
    }
    fn process(&mut self, flush: bool) -> Result<()> {
        let available = self.feature_start + self.features.len();
        while self.next + 104 <= available || (flush && self.next < available) {
            let lc = usize::from(self.next > 0);
            let begin = self.next - lc * 8;
            let end = (self.next + 104).min(available);
            let actual = end - begin;
            let core = (available - self.next).min(48).div_ceil(8);
            let encoded = actual.div_ceil(8);
            let rc = encoded.saturating_sub(lc + core);
            let mut input = vec![0.; 112 * 128];
            for t in begin..end {
                input[(t - begin) * 128..(t - begin + 1) * 128]
                    .copy_from_slice(&self.features[t - self.feature_start]);
            }
            self.chunk.copy_f32(&input)?;
            self.chunk_len.set(0, actual as f32);
            let mut c = vec![0.; 188 * 512];
            c[..self.state.cache.len()].copy_from_slice(&self.state.cache);
            self.cache.copy_f32(&c)?;
            self.cache_len.set(0, (self.state.cache.len() / 512) as f32);
            let mut f = vec![0.; 40 * 512];
            f[..self.state.fifo.len()].copy_from_slice(&self.state.fifo);
            self.fifo.copy_f32(&f)?;
            self.fifo_len.set(0, (self.state.fifo.len() / 512) as f32);
            let probabilities = objc2::rc::autoreleasepool(|_| -> Result<Vec<f32>> {
                let out = self.model.predict(&[
                    ("chunk", &self.chunk),
                    ("chunk_lengths", &self.chunk_len),
                    ("spkcache", &self.cache),
                    ("spkcache_lengths", &self.cache_len),
                    ("fifo", &self.fifo),
                    ("fifo_lengths", &self.fifo_len),
                ])?;
                let p = flat(&out.array("speaker_preds_out")?);
                let emb = flat(&out.array("chunk_pre_encoder_embs_out")?);
                let len = out.array("chunk_pre_encoder_lengths_out")?.get(0) as usize;
                ensure!(
                    len * 512 <= emb.len(),
                    "invalid Sortformer embedding length"
                );
                self.state.update(&emb[..len * 512], &p, lc, rc)
            })?;
            // Store immutable confirmed core only. Never expose right-context predictions as stable.
            for row in probabilities.as_chunks::<4>().0 {
                let a = self.frames as f64 * 0.08;
                let b = ((self.frames + 1) as f64 * 0.08).min(self.total as f64 / 16000.);
                for (s, p) in row.iter().enumerate() {
                    if *p >= 0.5 && b > a {
                        append_segment(&mut self.segments, a, b, s as i32 + 1);
                    }
                }
                self.frames += 1;
            }
            self.next += core * 8;
            let keep = self.next.saturating_sub(8);
            if keep > self.feature_start {
                self.features
                    .drain(..(keep - self.feature_start).min(self.features.len()));
                self.feature_start = keep;
            }
        }
        Ok(())
    }
}
impl Diarization for Sortformer {
    fn sample_rate(&self) -> u32 {
        16000
    }
    fn speakers(&self) -> usize {
        4
    }
    fn seconds_per_frame(&self) -> f64 {
        0.08
    }
    fn frame_count(&self) -> i64 {
        self.frames as i64
    }
    fn push_f32(&mut self, a: &[f32]) -> Result<()> {
        ensure!(
            !self.finished && a.iter().all(|x| x.is_finite()),
            "finished stream or nonfinite PCM"
        );
        for chunk in a.chunks(1600) {
            self.total += chunk.len();
            self.features.extend(self.mel.push(chunk, false));
            self.process(false)?;
        }
        Ok(())
    }
    fn finish(&mut self) -> Result<()> {
        if !self.finished {
            self.features.extend(self.mel.push(&[], true));
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
    #[test]
    fn frontend_is_partition_invariant() {
        let audio: Vec<_> = (0..16000).map(|i| (i as f32 * 0.02).sin()).collect();
        let mut a = Mel::new();
        let mut x = a.push(&audio, false);
        x.extend(a.push(&[], true));
        let mut b = Mel::new();
        let mut y = vec![];
        for c in audio.chunks(317) {
            y.extend(b.push(c, false));
        }
        y.extend(b.push(&[], true));
        assert_eq!(x, y);
        assert_eq!(x.len(), 100);
    }
}
#[cfg(test)]
mod parity_tests {
    use super::*;
    #[test]
    fn mel_matches_independent_pytorch_stft() {
        let audio: Vec<_> = (0..16000).map(|i| (i as f32 * 0.02).sin()).collect();
        let mut mel = Mel::new();
        let mut frames = mel.push(&audio, false);
        frames.extend(mel.push(&[], true));
        let rows: Vec<serde_json::Value> =
            serde_json::from_str(include_str!("../tests/fixtures/sortformer-mel.json")).unwrap();
        let mut max = 0_f32;
        for row in rows {
            let i = row["frame"].as_u64().unwrap() as usize;
            let expected: Vec<f32> = serde_json::from_value(row["values"].clone()).unwrap();
            for (a, b) in frames[i].iter().zip(expected) {
                max = max.max((a - b).abs());
            }
        }
        // Log-floor bins amplify roundoff from independent sin/FFT implementations.
        assert!(max < 0.003, "frontend max error {max}");
    }
    #[test]
    fn cache_compression_matches_pinned_fluid_audio() {
        let mut state = State::default();
        for step in 0..80 {
            let lc = usize::from(step > 0);
            let count = lc + 13;
            let mut emb = vec![];
            for i in 0..count {
                emb.extend(vec![((step * 6 + i) as f32 * 0.07).sin(); 512]);
            }
            let mut preds = vec![];
            for i in 0..state.cache.len() / 512 + state.fifo.len() / 512 + count {
                for s in 0..4 {
                    preds.push(if i % 11 == 0 {
                        0.01
                    } else if s == (i / 15 + step / 20) % 4 {
                        0.7 + 0.299 * ((i * 17 + step * 19) % 997) as f32 / 997.
                    } else {
                        0.003 + 0.03 * ((i * 13 + step * 23 + s * 11) % 991) as f32 / 991.
                    });
                }
            }
            state.update(&emb, &preds, lc, 7).unwrap();
        }
        let golden: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/sortformer-state.json")).unwrap();
        for (name, values) in [
            (
                "cache",
                state.cache.chunks(512).map(|r| r[0]).collect::<Vec<_>>(),
            ),
            ("fifo", state.fifo.chunks(512).map(|r| r[0]).collect()),
            ("cache_preds", state.cache_preds.unwrap()),
        ] {
            let expected: Vec<f32> = serde_json::from_value(golden[name].clone()).unwrap();
            assert_eq!(values.len(), expected.len());
            let max = values
                .iter()
                .zip(expected)
                .map(|(a, b)| (a - b).abs())
                .fold(0_f32, f32::max);
            assert!(max < 1e-6, "{name} max error {max}");
        }
        assert_eq!(
            state.sil_count,
            golden["silence_count"].as_u64().unwrap() as usize
        );
        assert!((state.silence[0] - golden["silence"].as_f64().unwrap() as f32).abs() < 1e-6);
    }
}
