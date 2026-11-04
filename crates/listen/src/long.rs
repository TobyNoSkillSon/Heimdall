//! Finished audio of any length through the live path's 15-s windows and seams, one window per call, so the
//! inference owner can interleave other work between windows (Vella's borrowed dictation, docs/vella.md).
//! The window/roll/margin arithmetic here is the one the live engine uses (`engine.rs`, `apply_decode`).
use crate::asr::{reconcile, Transcript, Word};

/// A decode window never exceeds 15 seconds at 16 kHz.
pub const WINDOW_SAMPLES: usize = 240_000;
/// A full window rolls forward by 13 s on the encoder's 80-ms grid, keeping about 2 s of acoustic overlap.
pub const ROLL_SAMPLES: usize = 208_000 / 1280 * 1280;
/// Words ending within this of the new window origin stay revisable: the next decode owns them.
pub const SEAM_MARGIN_NS: u64 = 640_000_000;
const NS_PER_SAMPLE: u64 = 62_500;

/// How many leading words a roll to `window_ns` commits: those ending well before the new origin.
pub(crate) fn committed_before(words: &[Word], window_ns: u64) -> usize {
    words.partition_point(|w| w.end_ns.saturating_add(SEAM_MARGIN_NS) < window_ns)
}

/// One request's progress through its windows. No VAD, no turn detection, no source state.
pub struct LongTranscription {
    audio: Vec<f32>,
    start_ns: u64,
    offset: usize,
    committed: Vec<Word>,
    tail: Vec<Word>,
    windows: usize,
    done: bool,
}
impl LongTranscription {
    pub fn new(audio: Vec<f32>, start_ns: u64) -> Self {
        let done = audio.is_empty();
        Self { audio, start_ns, offset: 0, committed: vec![], tail: vec![], windows: 0, done }
    }
    /// Windows a request of `samples` takes: 1 up to 15 s, then one more per 12.96 s.
    pub fn window_count(samples: usize) -> usize {
        match samples {
            0 => 0,
            n if n <= WINDOW_SAMPLES => 1,
            n => 1 + (n - WINDOW_SAMPLES).div_ceil(ROLL_SAMPLES),
        }
    }
    pub fn samples(&self) -> usize {
        self.audio.len()
    }
    pub fn is_done(&self) -> bool {
        self.done
    }
    /// Windows decoded so far.
    pub fn windows(&self) -> usize {
        self.windows
    }
    /// The next window's audio and its host time. Only meaningful while not done.
    pub fn window(&self) -> (&[f32], u64) {
        let end = (self.offset + WINDOW_SAMPLES).min(self.audio.len());
        (&self.audio[self.offset..end], self.start_ns + self.offset as u64 * NS_PER_SAMPLE)
    }
    /// Take the decode of `window()`: refine onsets on its audio, merge at the seam as the live roll does, then roll.
    pub fn apply(&mut self, mut transcript: Transcript) {
        if self.done {
            return;
        }
        {
            let (audio, ns) = self.window();
            crate::onset::refine_with_energy(&mut transcript.words, audio, ns);
            self.tail = reconcile(&self.tail, &transcript.words, ns);
        }
        self.windows += 1;
        if self.offset + WINDOW_SAMPLES >= self.audio.len() {
            self.done = true;
            return;
        }
        self.offset += ROLL_SAMPLES;
        let window_ns = self.start_ns + self.offset as u64 * NS_PER_SAMPLE;
        let count = committed_before(&self.tail, window_ns);
        self.committed.extend(self.tail.drain(..count));
    }
    /// Every word once, in order.
    pub fn finish(mut self) -> Transcript {
        self.committed.append(&mut self.tail);
        Transcript::from_words(self.committed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Recognizer;
    const MS: u64 = 1_000_000;
    /// Words at fixed times, returned once a window has heard all of them, like a decoder.
    struct Fixed(Vec<Word>, usize);
    impl Recognizer for Fixed {
        fn transcribe(&mut self, a: &[f32], ns: u64) -> anyhow::Result<Transcript> {
            assert!(a.len() <= WINDOW_SAMPLES, "a window never exceeds 15 s");
            self.1 += 1;
            let end = ns + a.len() as u64 * NS_PER_SAMPLE;
            // A word cut by the window origin is still heard, from the origin on.
            Ok(Transcript::from_words(self.0.iter().filter(|w| w.end_ns > ns && w.end_ns <= end)
                .map(|w| Word { start_ns: w.start_ns.max(ns), ..w.clone() }).collect()))
        }
    }
    fn run(samples: usize, words: Vec<Word>) -> (Transcript, usize) {
        let mut asr = Fixed(words, 0);
        let mut long = LongTranscription::new(vec![0.25; samples], 0);
        while !long.is_done() {
            let (audio, ns) = long.window();
            let t = asr.transcribe(audio, ns).unwrap();
            long.apply(t);
        }
        assert_eq!(long.windows(), LongTranscription::window_count(samples));
        (long.finish(), asr.1)
    }
    #[test]
    fn window_count_matches_the_live_roll() {
        assert_eq!(ROLL_SAMPLES, 207_360, "13 s on the 80-ms grid, as the live roll trims");
        assert_eq!(LongTranscription::window_count(0), 0);
        assert_eq!(LongTranscription::window_count(1), 1);
        assert_eq!(LongTranscription::window_count(240_000), 1);
        assert_eq!(LongTranscription::window_count(240_001), 2);
        // Ten minutes, Vella's limit: 47 windows.
        assert_eq!(LongTranscription::window_count(9_600_000), 47);
    }
    #[test]
    fn every_word_appears_once_across_seams() {
        // A word every 450 ms for 100 s, some straddling every seam.
        let words: Vec<Word> = (0..222u64)
            .map(|k| Word { text: format!("w{k}"), start_ns: k * 450 * MS, end_ns: k * 450 * MS + 400 * MS, confidence: 1. })
            .collect();
        let (t, calls) = run(1_600_000, words.clone());
        assert_eq!(calls, LongTranscription::window_count(1_600_000));
        assert!(t.words.windows(2).all(|p| p[0].start_ns < p[1].start_ns));
        let texts: Vec<_> = t.words.iter().map(|w| w.text.as_str()).collect();
        let expected: Vec<_> = words.iter().map(|w| w.text.as_str()).collect();
        assert_eq!(texts, expected);
    }
    #[test]
    fn short_audio_is_one_plain_decode_and_empty_audio_none() {
        let words = vec![Word { text: "hi".into(), start_ns: 100 * MS, end_ns: 300 * MS, confidence: 1. }];
        let (t, calls) = run(8_000, words);
        assert_eq!((t.text.as_str(), calls), ("hi", 1));
        let long = LongTranscription::new(vec![], 0);
        assert!(long.is_done());
        assert_eq!(long.finish().text, "");
    }
}
