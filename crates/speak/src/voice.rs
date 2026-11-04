//! Voices that stream PCM at their own sample rate and have no speed control Heimdall uses: Phonon S (local) and
//! ElevenLabs (API). `stream_line` says a line through one of them sentence by sentence, the way Jarvis's lines reach
//! playback: each sentence announced as it starts (`SpeechPacket::Sentence`, the island's text), its audio resampled
//! to 44.1 kHz (playback takes one rate per stream, from its first chunk) and time-stretched to the user's speed with the
//! same pitch-keeping WSOLA as Jarvis, chunk by chunk as it arrives.
use anyhow::{Result, ensure};

use crate::{Audio, SpeechPacket};

/// The rate every voice reaches playback at (Supertonic and Jarvis are native 44.1 kHz).
pub const PLAYBACK_RATE: u32 = 44_100;

/// A voice that streams one sentence of PCM at natural speed.
pub trait StreamingVoice: Send {
    /// Short name for log lines ("Phonon S", "ElevenLabs").
    fn label(&self) -> &str;
    /// Say `text` (one sentence, plain), calling `emit(samples, sample_rate)` for each chunk as it is made. Returns
    /// early with an error when `cancel` turns true.
    fn stream_sentence(&mut self, text: &str, cancel: &dyn Fn() -> bool, emit: &mut dyn FnMut(&[f32], u32) -> Result<()>) -> Result<()>;
}

/// Streaming linear-interpolation resampler, continuous across chunks (no clicks at chunk edges). Speech from a 24 kHz
/// voice has nothing above 12 kHz to alias; upsampling only needs a smooth line between samples.
pub struct Resampler {
    /// Input samples per output sample.
    step: f64,
    passthrough: bool,
    /// Global input position of the next output sample.
    next: f64,
    /// Input samples received so far, and the last of them.
    seen: u64,
    last: f32,
}
impl Resampler {
    pub fn new(from: u32, to: u32) -> Self {
        Self { step: from.max(1) as f64 / to.max(1) as f64, passthrough: from == to, next: 0., seen: 0, last: 0. }
    }
    pub fn push(&mut self, input: &[f32]) -> Vec<f32> {
        if self.passthrough {
            return input.to_vec();
        }
        if input.is_empty() {
            return vec![];
        }
        // Global index k: `last` is k = seen-1, input[j] is k = seen+j.
        let base = self.seen as f64;
        let x = |k: f64| -> f32 { if k < base { self.last } else { input[(k - base) as usize] } };
        let end = base + (input.len() - 1) as f64;
        let mut out = Vec::with_capacity((input.len() as f64 / self.step) as usize + 2);
        while self.next <= end {
            let i = self.next.floor();
            let frac = (self.next - i) as f32;
            let a = x(i);
            out.push(if frac == 0. { a } else { a + (x(i + 1.) - a) * frac });
            self.next += self.step;
        }
        self.seen += input.len() as u64;
        self.last = input[input.len() - 1];
        out
    }
}

/// Says `written` through `voice`: one sentence at a time (`drama3::display_sentences`, the island's units), each
/// announced, resampled to 44.1 kHz and stretched to `rate` (1.0 natural) as it streams. Returns everything played.
pub fn stream_line(voice: &mut dyn StreamingVoice, written: &str, rate: f32, cancel: &dyn Fn() -> bool,
    emit: &mut dyn FnMut(SpeechPacket) -> Result<()>) -> Result<Audio> {
    let mut all = Audio { samples: vec![], sample_rate: PLAYBACK_RATE, first_audio_ms: 0., synthesis_ms: 0. };
    let started = std::time::Instant::now();
    for sentence in crate::drama3::display_sentences(written) {
        ensure!(!cancel(), "speech interrupted");
        let plain = crate::drama3::normalize(&sentence);
        if plain.is_empty() {
            continue;
        }
        let sentence_start = std::time::Instant::now();
        let audio = stream_sentence(voice, &sentence, &plain, rate, cancel, emit)?;
        if all.samples.is_empty() {
            all.first_audio_ms = audio.first_audio_ms + (sentence_start - started).as_secs_f64() * 1000.;
        }
        all.samples.extend_from_slice(&audio.samples);
        all.synthesis_ms += audio.synthesis_ms;
    }
    Ok(all)
}

/// One sentence: the label packet before its first audio, then its audio as it streams. The sentence's audio as played.
pub fn stream_sentence(voice: &mut dyn StreamingVoice, written: &str, plain: &str, rate: f32, cancel: &dyn Fn() -> bool,
    emit: &mut dyn FnMut(SpeechPacket) -> Result<()>) -> Result<Audio> {
    let started = std::time::Instant::now();
    let mut stretcher = crate::stretch::Wsola::new(rate);
    let mut resampler: Option<(u32, Resampler)> = None;
    let mut announced = false;
    let mut played = Audio { samples: vec![], sample_rate: PLAYBACK_RATE, first_audio_ms: 0., synthesis_ms: 0. };
    let mut send = |samples: Vec<f32>, played: &mut Audio, announced: &mut bool| -> Result<()> {
        if samples.is_empty() || cancel() {
            return Ok(());
        }
        if !*announced {
            emit(SpeechPacket::Sentence(written.to_string()))?;
            *announced = true;
            played.first_audio_ms = started.elapsed().as_secs_f64() * 1000.;
        }
        played.samples.extend_from_slice(&samples);
        emit(SpeechPacket::Audio(Audio { samples, sample_rate: PLAYBACK_RATE, first_audio_ms: 0., synthesis_ms: 0. }))
    };
    voice.stream_sentence(plain, cancel, &mut |chunk, sample_rate| {
        ensure!(!cancel(), "speech interrupted");
        let (rate_in, r) = resampler.get_or_insert_with(|| (sample_rate, Resampler::new(sample_rate, PLAYBACK_RATE)));
        ensure!(*rate_in == sample_rate, "voice changed sample rate mid-sentence");
        let up = r.push(chunk);
        let out = stretcher.push(&up);
        send(out, &mut played, &mut announced)
    })?;
    ensure!(!cancel(), "speech interrupted");
    send(stretcher.finish(), &mut played, &mut announced)?;
    played.synthesis_ms = started.elapsed().as_secs_f64() * 1000.;
    Ok(played)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A voice that streams a fixed tone in 3 chunks per sentence at 24 kHz, recording what it was asked.
    struct Fake { said: Vec<String>, rate: u32 }
    impl StreamingVoice for Fake {
        fn label(&self) -> &str { "fake" }
        fn stream_sentence(&mut self, text: &str, cancel: &dyn Fn() -> bool, emit: &mut dyn FnMut(&[f32], u32) -> Result<()>) -> Result<()> {
            self.said.push(text.into());
            for _ in 0..3 {
                ensure!(!cancel(), "cancelled");
                emit(&vec![0.25; self.rate as usize / 10], self.rate)?;
            }
            Ok(())
        }
    }

    #[test]
    fn resampler_is_continuous_and_keeps_duration() {
        let mut r = Resampler::new(24_000, 44_100);
        let ramp: Vec<f32> = (0..24_000).map(|i| i as f32 / 24_000.).collect();
        let mut out = vec![];
        for part in ramp.chunks(997) { out.extend(r.push(part)); }
        let expected = 44_100.;
        assert!((out.len() as f64 - expected).abs() < 3., "{}", out.len());
        // A ramp stays a ramp: no step bigger than one input step at a chunk edge.
        let max_step = out.windows(2).map(|w| w[1] - w[0]).fold(0f32, f32::max);
        assert!(max_step <= 1. / 24_000. + 1e-6, "{max_step}");
        assert!(out.windows(2).all(|w| w[1] >= w[0] - 1e-7));
        assert_eq!(Resampler::new(44_100, 44_100).push(&[0.1, 0.2]), vec![0.1, 0.2]);
    }

    #[test]
    fn a_line_streams_sentence_by_sentence_at_44k_and_speed() {
        let mut voice = Fake { said: vec![], rate: 24_000 };
        let mut packets = vec![];
        let audio = stream_line(&mut voice, "Done. I moved 3 files.", 1.6, &|| false, &mut |p| { packets.push(p); Ok(()) }).unwrap();
        assert_eq!(voice.said, vec!["Done.", "I moved 3 files."].iter().map(|s| crate::drama3::normalize(s)).collect::<Vec<_>>());
        let labels: Vec<_> = packets.iter().filter_map(|p| match p { SpeechPacket::Sentence(s) => Some(s.clone()), _ => None }).collect();
        assert_eq!(labels, ["Done.", "I moved 3 files."]);
        assert!(packets.iter().all(|p| !matches!(p, SpeechPacket::Audio(a) if a.sample_rate != PLAYBACK_RATE)));
        // 0.3 s of voice per sentence at 160%: about 0.1875 s each at 44.1 kHz.
        let seconds = audio.samples.len() as f64 / 44_100.;
        assert!((seconds - 2. * 0.3 / 1.6).abs() < 0.03, "{seconds}");
        // The label comes before that sentence's first audio.
        assert!(matches!(packets[0], SpeechPacket::Sentence(_)));
    }

    #[test]
    fn cancel_stops_the_line() {
        let mut voice = Fake { said: vec![], rate: 24_000 };
        let flag = std::sync::atomic::AtomicBool::new(false);
        let result = stream_line(&mut voice, "One. Two. Three.", 1., &|| flag.load(std::sync::atomic::Ordering::Relaxed),
            &mut |p| { if matches!(p, SpeechPacket::Audio(_)) { flag.store(true, std::sync::atomic::Ordering::Relaxed); } Ok(()) });
        assert!(result.is_err());
        assert_eq!(voice.said.len(), 1);
    }
}
