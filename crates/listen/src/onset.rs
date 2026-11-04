//! Word onsets snapped to speech. Parakeet stamps a word at its first token's 80 ms encoder frame, and a word that
//! follows silence comes out early: about 0.43 s at the start of an utterance, 140–180 ms after a pause
//! (lab/screen-phrase-bench/REPORT.md). The engine already scores every 512-sample (32 ms) block with Silero, so each
//! decode's word starts move forward to the first voiced block. A start already inside speech never moves, a word is
//! never reordered past the next word, and the decoder's raw output stays in the evidence log.
use crate::Word;
use std::collections::VecDeque;

/// One Silero block: 512 samples at 16 kHz.
pub const BLOCK_NS: u64 = 32_000_000;
/// How far past its own end a word that lies wholly in non-speech may move to reach the speech after it. Parakeet's
/// first word after silence can sit entirely in the silence and end up to ~106 ms before the true onset (measured);
/// Silero can call that onset a block late. A longer gap is a real pause, so the word stays where it is.
pub const MAX_PAST_END_NS: u64 = 160_000_000;
/// Blocks kept per source: more than one 15-second decode window plus its one-second pre-roll.
pub const HISTORY_BLOCKS: usize = 640;

/// A source's recent VAD verdicts: `voiced[i]` covers `[origin_ns + i·32 ms, +32 ms)`, contiguous.
#[derive(Debug, Clone, Default)]
pub struct VoicedHistory {
    origin_ns: u64,
    voiced: VecDeque<bool>,
}
impl VoicedHistory {
    /// Record the block starting at `ns`. A block that does not follow the previous one restarts the history.
    pub fn push(&mut self, ns: u64, voiced: bool) {
        if self.voiced.is_empty() || self.origin_ns + self.voiced.len() as u64 * BLOCK_NS != ns {
            self.voiced.clear();
            self.origin_ns = ns;
        }
        self.voiced.push_back(voiced);
        while self.voiced.len() > HISTORY_BLOCKS {
            self.voiced.pop_front();
            self.origin_ns += BLOCK_NS;
        }
    }
    /// Start of the first voiced block that ends after `t` and begins at or before `limit`, never earlier than `t`. `None`
    /// when `t` lies before the history (its speech is unknown) or no such block is recorded.
    fn first_voiced(&self, t: u64, limit: u64) -> Option<u64> {
        if t < self.origin_ns {
            return None;
        }
        let first = ((t - self.origin_ns) / BLOCK_NS) as usize;
        (first..self.voiced.len())
            .map(|i| (i, self.origin_ns + i as u64 * BLOCK_NS))
            .take_while(|(_, begin)| *begin <= limit)
            .find(|(i, _)| self.voiced[*i])
            .map(|(_, begin)| begin.max(t))
    }
    /// Move each word's start forward to the first voiced block at or after it, within the word. Words are in time
    /// order and stay in order: a word never starts after the next one.
    ///
    /// A word that lies wholly in non-speech just before the next word's speech (within `MAX_PAST_END_NS` of its end)
    /// is the decoder's alignment running one word early, typically the first word of an utterance ("Take" in the
    /// pre-roll, "a" holding the onset). It moves to that onset: when the onset falls in the gap before the next word it
    /// fills the gap, otherwise it shares the next word's span with it, half each. A next word's start never moves
    /// earlier, and no word becomes zero-length (consumers drop or disown those: an `end > from` window filter, speaker ownership).
    /// The last word never moves past its own end.
    pub fn snap(&self, words: &mut [Word]) {
        for i in (0..words.len()).rev() {
            let next = words.get(i + 1).map(|n| (n.start_ns, n.end_ns));
            let w = &words[i];
            let limit = match next {
                Some((start, _)) => w.end_ns.saturating_add(MAX_PAST_END_NS).min(start).max(w.end_ns),
                // Strictly inside the word, so it keeps a span.
                None => w.end_ns.saturating_sub(1),
            };
            let Some(onset) = self.first_voiced(w.start_ns, limit) else { continue };
            match next {
                // Speech starts in the gap before the next word: the word fills that gap.
                Some((next_start, _)) if onset >= w.end_ns && onset < next_start => {
                    words[i].start_ns = onset;
                    words[i].end_ns = next_start;
                }
                // Speech starts at or inside the next word: the two share its span (positive only while the next
                // word still ends after the onset).
                Some((_, next_end)) if onset >= w.end_ns => {
                    // Never past the start of the word after next (decoder words can overlap).
                    let cap = next_end.min(words.get(i + 2).map_or(u64::MAX, |w| w.start_ns));
                    let half = onset + cap.saturating_sub(onset) / 2;
                    if half > onset {
                        words[i].start_ns = onset;
                        words[i].end_ns = half;
                        words[i + 1].start_ns = half;
                    }
                }
                _ => words[i].start_ns = onset.min(next.map_or(u64::MAX, |n| n.0)).max(w.start_ns),
            }
        }
    }
}

/// Energy frame for onset refinement: 80 samples (5 ms at 16 kHz), laid out from each word's own start.
const FRAME: usize = 80;
const FRAME_NS: i64 = 5_000_000;
const NS_PER_SAMPLE: u64 = 62_500;
/// Audio before a word that sets its noise floor.
pub const FLOOR_LOOKBACK_NS: u64 = 600_000_000;
/// How far before its (VAD-snapped) start a word may move: two Silero blocks, which can call an onset late.
pub const MAX_BACK_NS: u64 = 64_000_000;
/// The onset is where the energy, walked back from the word's loudest point, first falls to this far above the floor
/// (or this far below the peak in clean audio).
const ABOVE_FLOOR_DB: f32 = 3.0;
const BELOW_PEAK_DB: f32 = 35.0;
/// A forward move must skip audio that is quieter than the peak: its 90th-percentile frame at least this far below. A
/// stop closure inside a long word ("zerk|nij") would otherwise pass for the onset and jump the start past real speech.
const SKIPPED_BELOW_PEAK_DB: f32 = 3.0;

/// Refine word starts with the decoded audio (`audio[0]` at `audio_start_ns`), after `VoicedHistory::snap`. Silero
/// calls babble and a short pause speech, so a start there stays early (lab/screen-phrase-bench/REPORT.md, 23 Oct).
/// From the loudest 5 ms frame of each word, walk back while the smoothed energy stays above
/// `max(peak − 35 dB, floor + 3 dB)` (floor: 10th percentile over the 600 ms before the word and the word itself).
/// Where the walk stops is the onset. The start moves only to an onset found between `MAX_BACK_NS` before it and the
/// peak, never before the previous word's start; continuous speech with no such dip leaves the word alone, and so does
/// a forward move over audio nearly as loud as the peak.
pub fn refine_with_energy(words: &mut [Word], audio: &[f32], audio_start_ns: u64) {
    let audio_end_ns = audio_start_ns + audio.len() as u64 * NS_PER_SAMPLE;
    let mut prev_start: Option<u64> = None;
    for w in words.iter_mut() {
        let (s0, e1) = (w.start_ns, w.end_ns);
        let prev = prev_start.replace(s0);
        if s0 < audio_start_ns || e1 > audio_end_ns || e1 <= s0 {
            continue;
        }
        let rel = (s0 - audio_start_ns) as i64; // ns from the audio start
        let back = rel.min(FLOOR_LOOKBACK_NS as i64) / FRAME_NS; // whole frames available before the word
        let kend = (e1 - s0) as i64 / FRAME_NS; // frames starting inside the word that fit in it
        if kend < 2 {
            continue;
        }
        let db: Vec<f32> = (-back..kend)
            .map(|k| {
                let a = ((rel + k * FRAME_NS) as u64 / NS_PER_SAMPLE) as usize;
                let f = &audio[a..(a + FRAME).min(audio.len())];
                10.0 * (f.iter().map(|v| v * v).sum::<f32>() / f.len().max(1) as f32 + 1e-10).log10()
            })
            .collect();
        // Centred 3-frame mean, edges over what exists.
        let sm: Vec<f32> = (0..db.len())
            .map(|i| {
                let n = &db[i.saturating_sub(1)..(i + 2).min(db.len())];
                n.iter().sum::<f32>() / n.len() as f32
            })
            .collect();
        let at = |k: i64| (k + back) as usize;
        let p = (0..kend).fold(0, |b, k| if sm[at(k)] > sm[at(b)] { k } else { b });
        let mut sorted = sm.clone();
        sorted.sort_by(f32::total_cmp);
        let floor = sorted[(sorted.len() - 1) / 10];
        let level = (sm[at(p)] - BELOW_PEAK_DB).max(floor + ABOVE_FLOOR_DB);
        let mut lowest = -back.min(MAX_BACK_NS as i64 / FRAME_NS);
        if let Some(prev) = prev {
            // Never at or before the previous word's start.
            lowest = lowest.max((prev as i64 - s0 as i64).div_euclid(FRAME_NS) + 1);
        }
        let mut k = p;
        while k > lowest && sm[at(k - 1)] >= level {
            k -= 1;
        }
        if k > 0 {
            let mut skipped = sm[at(0)..at(k)].to_vec();
            skipped.sort_by(f32::total_cmp);
            if skipped[(skipped.len() - 1) * 9 / 10] > sm[at(p)] - SKIPPED_BELOW_PEAK_DB {
                continue;
            }
        }
        if k > lowest {
            let start = (s0 as i64 + k * FRAME_NS) as u64;
            w.start_ns = start;
            prev_start = Some(start);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const MS: u64 = 1_000_000;

    fn word(text: &str, start_ms: u64, end_ms: u64) -> Word {
        Word { text: text.into(), start_ns: start_ms * MS, end_ns: end_ms * MS, confidence: 1.0 }
    }
    /// History from `origin_ms` with speech in blocks `[from, to)`.
    fn history(origin_ms: u64, blocks: usize, speech: &[(usize, usize)]) -> VoicedHistory {
        let mut h = VoicedHistory::default();
        for i in 0..blocks {
            h.push(origin_ms * MS + i as u64 * BLOCK_NS, speech.iter().any(|(a, b)| (*a..*b).contains(&i)));
        }
        h
    }
    fn starts(words: &[Word]) -> Vec<u64> {
        words.iter().map(|w| w.start_ns / MS).collect()
    }

    #[test]
    fn early_start_moves_to_the_first_voiced_block_inside_the_word() {
        // Speech from block 10 (320 ms); Parakeet put "look" at 100 ms, ending at 500 ms.
        let h = history(0, 40, &[(10, 30)]);
        let mut w = vec![word("Look", 100, 500), word("at", 500, 600), word("this.", 600, 900)];
        h.snap(&mut w);
        assert_eq!(starts(&w), [320, 500, 600]);
        assert_eq!(w[0].end_ns, 500 * MS);
    }

    #[test]
    fn start_inside_speech_never_moves() {
        let h = history(0, 40, &[(0, 40)]);
        let mut w = vec![word("Heimdall,", 37, 410), word("take", 455, 600)];
        let before = w.clone();
        h.snap(&mut w);
        assert_eq!(w, before);
        // Inside a voiced block, not at its edge: unchanged too.
        let h = history(0, 40, &[(5, 40)]);
        let mut w = vec![word("take", 171, 300)];
        h.snap(&mut w);
        assert_eq!(starts(&w), [171]);
    }

    #[test]
    fn pause_absorbed_into_the_next_word_is_removed() {
        // "...again. Take a look": speech 0–992 ms, a 416 ms pause, speech from 1.408 s (block 44). Parakeet starts "Take"
        // where "again." ended.
        let h = history(0, 80, &[(0, 31), (44, 70)]);
        let mut w = vec![word("again.", 700, 1000), word("Take", 1000, 1600), word("a", 1600, 1700), word("look.", 1700, 2100)];
        h.snap(&mut w);
        assert_eq!(starts(&w), [700, 1408, 1600, 1700]);
        assert_eq!(w[0].end_ns, 1000 * MS, "the previous word keeps its end");
    }

    #[test]
    fn word_wholly_in_preroll_moves_to_the_onset_just_after_it() {
        // Utterance start: Parakeet put all of "Take" in the silence (40–380 ms); speech starts at block 13 (416 ms).
        let h = history(0, 60, &[(13, 50)]);
        let mut w = vec![word("Take", 40, 380), word("a", 380, 500), word("screenshot.", 500, 1200)];
        h.snap(&mut w);
        // "a" holds the onset (416–500 ms); "Take" moves there and takes the first half of it.
        assert_eq!(starts(&w), [416, 458, 500]);
        assert_eq!((w[0].end_ns, w[1].end_ns), (458 * MS, 500 * MS));
        assert!(w.windows(2).all(|p| p[0].start_ns <= p[1].start_ns && p[0].end_ns <= p[1].start_ns));
        assert!(w.iter().all(|w| w.start_ns < w.end_ns), "no zero-length word");
        // Two leading words in the silence: both move, each keeps a span.
        let mut w = vec![word("Look", 40, 300), word("at", 300, 400), word("this.", 400, 900)];
        h.snap(&mut w);
        assert_eq!(starts(&w), [416, 537, 658]);
        assert!(w.iter().all(|w| w.start_ns < w.end_ns) && w[2].end_ns == 900 * MS);
    }

    #[test]
    fn unvoiced_word_before_a_real_pause_stays() {
        // A quiet "this." that Silero missed, then 600 ms of silence before the next sentence: no move.
        let h = history(0, 80, &[(0, 20), (40, 70)]);
        // "this." spans 680–740 ms, all unvoiced (blocks 21–23); the next speech is at 1280 ms.
        let mut w = vec![word("at", 500, 640), word("this.", 680, 740), word("Then", 1280, 1500)];
        h.snap(&mut w);
        assert_eq!(starts(&w), [500, 680, 1280]);
    }

    #[test]
    fn starts_stay_ordered_when_words_overlap() {
        // Decoder overlap: "at" ends after "this" starts. Speech only from 700 ms.
        let h = history(0, 40, &[(22, 40)]);
        let mut w = vec![word("at", 300, 800), word("this", 600, 900)];
        h.snap(&mut w);
        // "this" moves to 704; "at" may not pass it.
        assert_eq!(starts(&w), [704, 704]);
        assert!(w[0].end_ns >= w[0].start_ns);
        // A split of an overlapping next word never passes the word after it (review: "Take a look" with "a" 380–600
        // and "look" 500–900 must keep a ≤ look).
        let h = history(0, 40, &[(13, 40)]);
        let mut w = vec![word("Take", 40, 380), word("a", 380, 600), word("look", 500, 900)];
        h.snap(&mut w);
        assert!(w.windows(2).all(|p| p[0].start_ns <= p[1].start_ns), "{:?}", starts(&w));
        assert_eq!(starts(&w), [416, 458, 500]);
        assert!(w.iter().all(|w| w.start_ns < w.end_ns));
    }

    #[test]
    fn unknown_speech_leaves_words_alone() {
        // Before the history, after it, or with no voiced block before the limit.
        let h = history(1_000, 20, &[(5, 20)]);
        let mut w = vec![word("early", 900, 1_400)];
        h.snap(&mut w);
        assert_eq!(starts(&w), [900]);
        let mut w = vec![word("late", 1_700, 1_800)];
        h.snap(&mut w);
        assert_eq!(starts(&w), [1_700]);
        let silent = history(0, 40, &[]);
        let mut w = vec![word("hm", 100, 300)];
        silent.snap(&mut w);
        assert_eq!(starts(&w), [100]);
        // A word moves past its end only within MAX_PAST_END_NS, and the last word never does.
        let h = history(0, 40, &[(20, 40)]);
        let mut w = vec![word("uh", 100, 400), word("so", 640, 800)];
        h.snap(&mut w);
        assert_eq!(starts(&w), [100, 640], "speech at 640 ms is 240 ms after the word");
        let mut w = vec![word("uh", 100, 500), word("so", 640, 800)];
        h.snap(&mut w);
        assert_eq!(starts(&w), [640, 720]);
        // Speech in the gap before the next word: the word fills the gap and the next word keeps its start
        // (bench case "Okay so, capture…": "so," must not move earlier).
        let mut w = vec![word("Okay", 100, 500), word("so,", 700, 900)];
        h.snap(&mut w);
        assert_eq!(starts(&w), [640, 700]);
        assert_eq!(w[0].end_ns, 700 * MS);
        let mut w = vec![word("uh", 100, 500)];
        h.snap(&mut w);
        assert_eq!(starts(&w), [100]);
        // Speech beginning exactly at the last word's end leaves it alone rather than zero-length.
        let mut w = vec![word("uh", 100, 640)];
        h.snap(&mut w);
        assert_eq!(starts(&w), [100]);
    }


    /// 16 kHz audio from 0 ns: `level` (linear amplitude) per `[from_ms, to_ms)` span, `noise` elsewhere.
    fn audio(ms: u64, noise: f32, spans: &[(u64, u64, f32)]) -> Vec<f32> {
        (0..ms * 16)
            .map(|i| {
                let t = i / 16;
                let a = spans.iter().find(|(a, b, _)| (*a..*b).contains(&t)).map_or(noise, |s| s.2);
                // Alternating sign keeps the RMS at `a`.
                if i % 2 == 0 { a } else { -a }
            })
            .collect()
    }

    /// Starts within one 5 ms frame of `want` (the 3-frame smoothing can put an onset one frame early).
    fn near(words: &[Word], want: &[u64]) {
        let got = starts(words);
        assert!(got.len() == want.len() && got.iter().zip(want).all(|(g, w)| g <= w && w - g <= 5), "{got:?} vs {want:?}");
    }

    #[test]
    fn energy_moves_a_start_left_in_a_short_pause_to_the_onset() {
        // "Heimdall, | take a look": Silero held through the 200 ms pause, so "take" starts at the name's end (600 ms).
        let a = audio(2_000, 0.0, &[(100, 600, 0.3), (800, 1_400, 0.3)]);
        let mut w = vec![word("Heimdall,", 100, 600), word("take", 600, 1_000), word("a", 1_000, 1_100)];
        refine_with_energy(&mut w, &a, 0);
        near(&w, &[100, 800, 1_000]);
        // Babble 10 dB under the speech fills the pause: the same onset.
        let a = audio(2_000, 0.03, &[(100, 600, 0.3), (800, 1_400, 0.3)]);
        let mut w = vec![word("Heimdall,", 100, 600), word("take", 600, 1_000)];
        refine_with_energy(&mut w, &a, 0);
        near(&w, &[100, 800]);
    }

    #[test]
    fn energy_moves_a_late_start_back_at_most_two_blocks() {
        // VAD called the onset at 832 ms; speech began at 800 ms after silence.
        let a = audio(2_000, 0.0, &[(800, 1_400, 0.3)]);
        let mut w = vec![word("take", 832, 1_200)];
        refine_with_energy(&mut w, &a, 0);
        near(&w, &[800]);
        // An onset 200 ms back is out of reach: the start stays.
        let mut w = vec![word("take", 1_000, 1_200)];
        let a = audio(2_000, 0.0, &[(800, 1_400, 0.3)]);
        refine_with_energy(&mut w, &a, 0);
        assert_eq!(starts(&w), [1_000]);
    }

    #[test]
    fn energy_leaves_continuous_speech_unknown_audio_and_order_alone() {
        // No dip anywhere near the word: unchanged.
        let a = audio(2_000, 0.0, &[(0, 2_000, 0.3)]);
        let mut w = vec![word("a", 500, 700), word("b", 700, 900)];
        let before = w.clone();
        refine_with_energy(&mut w, &a, 0);
        assert_eq!(w, before);
        // A sharp onset exactly at the start stays exact.
        let a = audio(2_000, 0.0, &[(1_024, 1_800, 0.3)]);
        let mut w = vec![word("Take", 1_024, 1_400)];
        refine_with_energy(&mut w, &a, 0);
        assert_eq!(starts(&w), [1_024]);
        // A stop closure inside a long word ("zerk | nij", the second part louder) is not an onset.
        let a = audio(2_000, 0.0, &[(500, 760, 0.25), (840, 1_100, 0.3)]);
        let mut w = vec![word("zerknij", 500, 1_100)];
        refine_with_energy(&mut w, &a, 0);
        assert_eq!(starts(&w), [500]);
        // Words outside the audio, or shorter than two frames, are skipped.
        // (Audio here spans 1–3 s.)
        let mut w = vec![word("y", 500, 1_200), word("x", 2_900, 3_100), word("z", 2_000, 2_007)];
        refine_with_energy(&mut w, &a, 1_000 * MS);
        assert_eq!(starts(&w), [500, 2_900, 2_000]);
        // Moving back never reaches the previous word's start.
        let a = audio(2_000, 0.0, &[(800, 1_400, 0.3)]);
        let mut w = vec![word("uh", 820, 832), word("take", 832, 1_200)];
        refine_with_energy(&mut w, &a, 0);
        // "uh" takes the onset itself; "take" then has no onset in reach above it and stays.
        assert_eq!(starts(&w), [800, 832]);
        let mut w = vec![word("uh", 815, 832), word("take", 832, 1_200)];
        let a = audio(2_000, 0.0, &[(700, 1_400, 0.3)]);
        refine_with_energy(&mut w, &a, 0);
        assert_eq!(starts(&w), [815, 832], "continuous speech before both: no onset in reach");
    }

    #[test]
    fn history_is_bounded_and_restarts_on_a_gap() {
        let mut h = VoicedHistory::default();
        for i in 0..(HISTORY_BLOCKS as u64 + 10) {
            h.push(i * BLOCK_NS, true);
        }
        assert_eq!(h.voiced.len(), HISTORY_BLOCKS);
        assert_eq!(h.origin_ns, 10 * BLOCK_NS);
        // A discontinuous block starts over.
        h.push(10_000 * BLOCK_NS + 5, false);
        assert_eq!((h.voiced.len(), h.origin_ns), (1, 10_000 * BLOCK_NS + 5));
    }
}
