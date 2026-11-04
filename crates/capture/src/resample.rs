use anyhow::Result;
use rubato::{FftFixedIn, Resampler};
/// Anti-aliased FFT resampling with fixed blocks. Preserve input remainder across
/// callbacks; output_delay is compensated in the pipeline's timestamp origin.
pub(crate) struct MonoResampler {
    dsp: FftFixedIn<f32>,
    input: Vec<f32>,
    consumed: usize,
    block: usize,
    pub delay: usize,
}
impl MonoResampler {
    pub fn new(rate: u32) -> Result<Self> {
        let block = (rate as usize / 50).max(160);
        let dsp = FftFixedIn::<f32>::new(rate as usize, 16000, block, 1, 1)?;
        let delay = dsp.output_delay();
        Ok(Self {
            dsp,
            input: Vec::new(),
            consumed: 0,
            block,
            delay,
        })
    }
    pub fn push(&mut self, interleaved: &[f32], channels: usize) -> Result<Vec<f32>> {
        anyhow::ensure!(
            channels > 0 && interleaved.len().is_multiple_of(channels),
            "invalid channel layout"
        );
        self.input.extend(
            interleaved
                .chunks_exact(channels)
                .map(|f| f.iter().sum::<f32>() / channels as f32),
        );
        let mut out = Vec::new();
        while self.input.len() - self.consumed >= self.block {
            let samples = &self.input[self.consumed..self.consumed + self.block];
            let output = self.dsp.process(&[samples], None)?;
            out.extend_from_slice(&output[0]);
            self.consumed += self.block;
        }
        if self.consumed > 0 {
            self.input.drain(..self.consumed);
            self.consumed = 0;
        }
        Ok(out)
    }
    pub fn finish(&mut self, needed: usize) -> Result<Vec<f32>> {
        let mut padded = vec![0.; self.block];
        padded[..self.input.len()].copy_from_slice(&self.input);
        let mut out = self.dsp.process(&[padded], None)?[0].clone();
        while out.len() < needed {
            out.extend_from_slice(&self.dsp.process(&[vec![0.; self.block]], None)?[0]);
        }
        self.input.clear();
        Ok(out)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn tone(rate: u32, hz: f32) -> Vec<f32> {
        (0..rate)
            .map(|i| (i as f32 * hz * std::f32::consts::TAU / rate as f32).sin())
            .collect()
    }
    #[test]
    fn arbitrary_callback_boundaries_and_downmix() {
        let mono = tone(44100, 1000.);
        let stereo: Vec<_> = mono.iter().flat_map(|s| [*s, *s]).collect();
        let mut a = MonoResampler::new(44100).unwrap();
        let mut b = MonoResampler::new(44100).unwrap();
        let expected = a.push(&stereo, 2).unwrap();
        let mut actual = vec![];
        for c in stereo.chunks(254) {
            actual.extend(b.push(c, 2).unwrap());
        }
        assert_eq!(expected, actual);
        assert!((actual.len() as i32 - 16000).abs() < 1000);
        let rms = (actual
            .iter()
            .skip(a.delay + 160)
            .map(|v| v * v)
            .sum::<f32>()
            / (actual.len() - a.delay - 160) as f32)
            .sqrt();
        assert!((rms - 0.707).abs() < 0.02);
    }
    #[test]
    fn alias_is_suppressed_and_silence_stays_silent() {
        let mut r = MonoResampler::new(48000).unwrap();
        let high = r.push(&tone(48000, 12000.), 1).unwrap();
        assert!(
            high.iter().skip(r.delay + 320).map(|v| v * v).sum::<f32>() / (high.len() as f32)
                < 1e-5
        );
        let mut r = MonoResampler::new(16000).unwrap();
        assert!(r.push(&vec![0.; 3200], 1).unwrap().iter().all(|v| *v == 0.));
    }
}
