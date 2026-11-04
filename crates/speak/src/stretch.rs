//! Pitch-preserving speed change for voices without a native rate control (Jarvis, Drama 3).
//!
//! WSOLA (waveform-similarity overlap-add, Verhelst & Roelands 1993): 1024-sample Hann frames (23 ms at 44.1 kHz)
//! are overlap-added at a fixed 512-sample output hop while the input advances `512 × rate`. Each frame's input
//! position may move up to ±384 samples from its ideal place, to wherever its overlap best matches the natural
//! continuation of the previous frame, so pitch periods line up and the voice keeps its pitch and timbre. Rate 1.0
//! is an exact pass-through. The output depends only on the input samples, never on how they were chunked.
pub const FRAME: usize = 1024;
pub const HOP: usize = FRAME / 2;
pub const SEEK: usize = 384;
/// The menu's range, in percent of natural speed.
pub const MIN_PERCENT: u16 = 80;
pub const MAX_PERCENT: u16 = 160;

pub fn clamp_rate(rate: f32) -> f32 {
    if rate.is_finite() {
        rate.clamp(MIN_PERCENT as f32 / 100., MAX_PERCENT as f32 / 100.)
    } else {
        1.
    }
}

pub struct Wsola {
    rate: f64,
    window: Vec<f32>,
    /// Input samples from absolute index `base`, with HOP zeros of virtual lead-in before sample 0.
    input: Vec<f32>,
    base: usize,
    /// Absolute (padded) input position of the previous frame, None before the first frame.
    previous: Option<usize>,
    frames: usize,
    /// Overlap-add tail: the second half of the last frame, not yet complete.
    tail: Vec<f32>,
    /// Output samples still to drop: the lead-in half frame.
    skip: usize,
    received: usize,
    emitted: usize,
}

impl Wsola {
    pub fn new(rate: f32) -> Self {
        let rate = clamp_rate(rate) as f64;
        let window = (0..FRAME)
            .map(|i| (0.5 - 0.5 * (std::f64::consts::TAU * i as f64 / FRAME as f64).cos()) as f32)
            .collect();
        Self { rate, window, input: vec![0.; HOP], base: 0, previous: None, frames: 0, tail: vec![0.; HOP], skip: HOP, received: 0, emitted: 0 }
    }
    pub fn passthrough(&self) -> bool {
        self.rate == 1.
    }
    /// Feed input; returns every output sample that is final.
    pub fn push(&mut self, samples: &[f32]) -> Vec<f32> {
        self.received += samples.len();
        if self.passthrough() {
            self.emitted += samples.len();
            return samples.to_vec();
        }
        self.input.extend_from_slice(samples);
        let mut out = Vec::new();
        while self.ready() {
            self.frame(&mut out);
        }
        self.compact();
        self.trim(out)
    }
    /// Flush at the end of the utterance: the remaining input, padded with silence, then the overlap tail.
    pub fn finish(&mut self) -> Vec<f32> {
        if self.passthrough() {
            return vec![];
        }
        let target = (self.received as f64 / self.rate).round() as usize;
        let mut out = Vec::new();
        // Enough silence for the frame that reaches the last real sample, wherever its search lands.
        self.input.extend(std::iter::repeat(0.).take(2 * FRAME + 2 * SEEK));
        while self.emitted + out.len().saturating_sub(self.skip) < target && self.ready() {
            self.frame(&mut out);
        }
        out.extend_from_slice(&self.tail);
        self.tail.iter_mut().for_each(|x| *x = 0.);
        let before = self.emitted;
        let mut out = self.trim(out);
        out.truncate(target.saturating_sub(before));
        self.emitted = before + out.len();
        out
    }
    fn ideal(&self) -> usize {
        (self.frames as f64 * HOP as f64 * self.rate).round() as usize
    }
    fn end(&self) -> usize {
        self.base + self.input.len()
    }
    fn ready(&self) -> bool {
        let ideal = self.ideal();
        let candidate_end = ideal + SEEK + FRAME;
        let natural_end = self.previous.map_or(0, |p| p + HOP + FRAME);
        candidate_end.max(natural_end) <= self.end()
    }
    fn at(&self, absolute: usize) -> &[f32] {
        &self.input[absolute - self.base..]
    }
    fn frame(&mut self, out: &mut Vec<f32>) {
        let ideal = self.ideal();
        let position = match self.previous {
            None => ideal,
            Some(previous) => {
                // The overlap region: the first half of the new frame against the samples that would have followed
                // the previous frame's overlap (its natural continuation).
                let natural = &self.at(previous + HOP)[..HOP];
                let lo = ideal.saturating_sub(SEEK).max(self.base);
                let hi = ideal + SEEK;
                let mut best = (f32::NEG_INFINITY, ideal);
                for start in lo..=hi {
                    let candidate = &self.at(start)[..HOP];
                    let score: f32 = candidate.iter().zip(natural).map(|(a, b)| a * b).sum();
                    if score > best.0 {
                        best = (score, start);
                    }
                }
                best.1
            }
        };
        let segment = &self.input[position - self.base..position - self.base + FRAME];
        for i in 0..HOP {
            out.push(self.tail[i] + segment[i] * self.window[i]);
        }
        for i in 0..HOP {
            self.tail[i] = segment[HOP + i] * self.window[HOP + i];
        }
        self.previous = Some(position);
        self.frames += 1;
    }
    fn compact(&mut self) {
        // Keep everything the next frame's search or natural continuation can still read.
        let keep_from = self.ideal().saturating_sub(SEEK).min(self.previous.map_or(usize::MAX, |p| p + HOP));
        if keep_from > self.base + 8192 {
            let drop = keep_from - self.base;
            self.input.drain(..drop);
            self.base += drop;
        }
    }
    fn trim(&mut self, mut out: Vec<f32>) -> Vec<f32> {
        let skip = self.skip.min(out.len());
        out.drain(..skip);
        self.skip -= skip;
        self.emitted += out.len();
        out
    }
}

/// Stretch a complete clip (bank lines, samples).
pub fn stretch(samples: &[f32], rate: f32) -> Vec<f32> {
    let mut w = Wsola::new(rate);
    let mut out = w.push(samples);
    out.extend(w.finish());
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    fn tone(freq: f32, seconds: f32) -> Vec<f32> {
        (0..(44100. * seconds) as usize).map(|i| (std::f32::consts::TAU * freq * i as f32 / 44100.).sin() * 0.5).collect()
    }
    fn crossings(x: &[f32]) -> usize {
        x.windows(2).filter(|w| w[0] < 0. && w[1] >= 0.).count()
    }
    #[test]
    fn unit_rate_is_an_exact_passthrough() {
        let x = tone(220., 0.5);
        assert_eq!(stretch(&x, 1.), x);
    }
    #[test]
    fn faster_speech_keeps_pitch_and_shortens_duration() {
        let x = tone(220., 2.);
        for rate in [0.8_f32, 1.25, 1.6] {
            let y = stretch(&x, rate);
            let expected = (x.len() as f32 / rate).round() as usize;
            assert_eq!(y.len(), expected, "rate {rate}");
            // Pitch: crossings per second unchanged within 1%.
            let fx = crossings(&x) as f32 / (x.len() as f32 / 44100.);
            let inner = &y[2048..y.len() - 2048];
            let fy = crossings(inner) as f32 / (inner.len() as f32 / 44100.);
            assert!((fy - fx).abs() / fx < 0.01, "rate {rate}: {fx} Hz vs {fy} Hz");
            // Level: steady-state RMS within 10% (no comb cancellation, no fade).
            let rms = |s: &[f32]| (s.iter().map(|v| v * v).sum::<f32>() / s.len() as f32).sqrt();
            assert!((rms(inner) - rms(&x)).abs() / rms(&x) < 0.1, "rate {rate}");
            // The onset is not faded in: the first 5 ms carry the tone already.
            assert!(rms(&y[..220]) > 0.25, "rate {rate}: onset {}", rms(&y[..220]));
        }
    }
    #[test]
    fn output_is_independent_of_chunking() {
        let x = tone(180., 1.3);
        let whole = stretch(&x, 1.3);
        let mut w = Wsola::new(1.3);
        let mut chunked = Vec::new();
        for chunk in x.chunks(777) {
            chunked.extend(w.push(chunk));
        }
        chunked.extend(w.finish());
        assert_eq!(whole, chunked);
    }
    #[test]
    fn short_clips_and_silence_survive() {
        assert_eq!(stretch(&[], 1.4).len(), 0);
        let y = stretch(&vec![0.; 300], 1.5);
        assert_eq!(y.len(), 200);
        assert!(y.iter().all(|v| *v == 0.));
        assert_eq!(clamp_rate(f32::NAN), 1.);
        assert_eq!(clamp_rate(9.), 1.6);
    }
}
