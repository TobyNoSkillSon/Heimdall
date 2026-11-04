//! Ground-truth math for the screen-phrase benchmark (`heimdall-eval screen-phrase-bench`):
//! speech trimming and splicing with exact phrase onsets, noise at a measured SNR, the
//! synthetic screen's frame pattern and the frame on screen at a given host time.
//! Pure functions only; the runner (models, archive, ASR) lives in the binary.

pub const RATE: usize = 16_000;
pub const NS_PER_SAMPLE: u64 = 62_500;
/// Energy frames for trimming: 5 ms.
pub const TRIM_FRAME: usize = 80;
/// A frame is speech when its RMS is within this many dB of the loudest frame.
pub const TRIM_REL_DB: f32 = -40.0;
/// Kept before the onset / after the offset, faded, so splices do not click. Below threshold by construction.
pub const TRIM_PAD: usize = 160;

/// Deterministic generator (SplitMix64) so every case is reproducible from its seed.
#[derive(Clone)]
pub struct Rng(pub u64);
impl Rng {
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    /// Uniform in [0, 1).
    pub fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
    pub fn range(&mut self, lo: usize, hi: usize) -> usize {
        lo + (self.next_u64() % (hi - lo + 1) as u64) as usize
    }
    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[(self.next_u64() % items.len() as u64) as usize]
    }
    /// Standard normal (Box-Muller).
    pub fn normal(&mut self) -> f64 {
        let u = self.unit().max(1e-12);
        let v = self.unit();
        (-2.0 * u.ln()).sqrt() * (2.0 * std::f64::consts::PI * v).cos()
    }
}

fn rms(x: &[f32]) -> f32 {
    if x.is_empty() {
        return 0.0;
    }
    (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt()
}

/// First and one-past-last sample of speech: the first/last 5 ms frame whose RMS is within `TRIM_REL_DB`
/// of the loudest frame (and above an absolute floor), refined to the first/last sample in it whose
/// magnitude passes the same threshold. `None` for silence.
pub fn speech_bounds(x: &[f32]) -> Option<(usize, usize)> {
    let frames: Vec<f32> = x.chunks(TRIM_FRAME).map(rms).collect();
    let peak = frames.iter().cloned().fold(0.0, f32::max);
    if peak < 1e-4 {
        return None;
    }
    let threshold = (peak * 10f32.powf(TRIM_REL_DB / 20.0)).max(1e-4);
    let first = frames.iter().position(|&r| r > threshold)?;
    let last = frames.iter().rposition(|&r| r > threshold)?;
    // Refine to the sample: the first/last sample inside those frames whose magnitude passes the threshold.
    let frame = |i: usize| i * TRIM_FRAME..((i + 1) * TRIM_FRAME).min(x.len());
    let start = frame(first).find(|&i| x[i].abs() > threshold).unwrap_or(first * TRIM_FRAME);
    let end = frame(last).rev().find(|&i| x[i].abs() > threshold).map_or(frame(last).end, |i| i + 1);
    Some((start, end))
}

/// A trimmed segment: `audio[onset]` is the first speech sample.
#[derive(Clone, Debug)]
pub struct Trimmed {
    pub audio: Vec<f32>,
    pub onset: usize,
    /// One past the last speech sample.
    pub offset: usize,
}

/// Cut a TTS render to its speech, keeping up to `TRIM_PAD` faded samples on each side.
pub fn trim(x: &[f32]) -> Option<Trimmed> {
    let (start, end) = speech_bounds(x)?;
    let from = start.saturating_sub(TRIM_PAD);
    let to = (end + TRIM_PAD).min(x.len());
    let mut audio = x[from..to].to_vec();
    let onset = start - from;
    let offset = end - from;
    for i in 0..onset {
        audio[i] *= i as f32 / onset as f32;
    }
    let tail = audio.len() - offset;
    for i in 0..tail {
        audio[offset + i] *= 1.0 - (i + 1) as f32 / tail as f32;
    }
    Some(Trimmed { audio, onset, offset })
}

/// Utterance built from segments: sample positions of the target segment's first speech sample
/// and of the utterance's speech span.
#[derive(Clone, Debug, PartialEq)]
pub struct Spliced {
    pub audio: Vec<f32>,
    pub target_onset: usize,
    pub speech_start: usize,
    pub speech_end: usize,
}

/// Concatenate trimmed segments. `gaps[i]` is the silence between segment i's last speech sample and
/// segment i+1's first speech sample (pads overlap into the gap; a shorter gap than both pads is
/// widened to them). `target` selects the segment whose onset is the ground truth.
pub fn splice(parts: &[Trimmed], gaps: &[usize], target: usize) -> Spliced {
    assert!(!parts.is_empty() && gaps.len() + 1 == parts.len() && target < parts.len());
    let mut audio: Vec<f32> = vec![];
    let mut target_onset = 0;
    let mut speech_end = 0;
    let mut speech_start = 0;
    for (i, p) in parts.iter().enumerate() {
        // Where this part's buffer begins in the output.
        let begin = if i == 0 {
            0
        } else {
            let tail_pad = parts[i - 1].audio.len() - parts[i - 1].offset;
            let gap = gaps[i - 1].max(tail_pad + p.onset);
            speech_end + gap - p.onset
        };
        if audio.len() < begin + p.audio.len() {
            audio.resize(begin + p.audio.len(), 0.0);
        }
        for (j, v) in p.audio.iter().enumerate() {
            audio[begin + j] += v;
        }
        if i == 0 {
            speech_start = begin + p.onset;
        }
        if i == target {
            target_onset = begin + p.onset;
        }
        speech_end = begin + p.offset;
    }
    Spliced { audio, target_onset, speech_start, speech_end }
}

/// Mean power of `speech` over its speech span.
pub fn speech_power(speech: &[f32], span: (usize, usize)) -> f64 {
    let s = &speech[span.0..span.1];
    s.iter().map(|v| (*v as f64).powi(2)).sum::<f64>() / s.len().max(1) as f64
}

/// Scale `noise` so speech power over `span` / noise power over the whole noise = `snr_db`; return the gain.
pub fn noise_gain(speech: &[f32], span: (usize, usize), noise: &[f32], snr_db: f64) -> f64 {
    let ps = speech_power(speech, span);
    let pn = noise.iter().map(|v| (*v as f64).powi(2)).sum::<f64>() / noise.len().max(1) as f64;
    if pn <= 0.0 {
        return 0.0;
    }
    (ps / pn / 10f64.powf(snr_db / 10.0)).sqrt()
}

pub fn white(rng: &mut Rng, n: usize) -> Vec<f32> {
    (0..n).map(|_| rng.normal() as f32).collect()
}

/// Pink noise (Paul Kellet's refined filter over white noise).
pub fn pink(rng: &mut Rng, n: usize) -> Vec<f32> {
    let mut b = [0f64; 7];
    (0..n)
        .map(|_| {
            let w = rng.normal();
            b[0] = 0.99886 * b[0] + w * 0.0555179;
            b[1] = 0.99332 * b[1] + w * 0.0750759;
            b[2] = 0.96900 * b[2] + w * 0.1538520;
            b[3] = 0.86650 * b[3] + w * 0.3104856;
            b[4] = 0.55000 * b[4] + w * 0.5329522;
            b[5] = -0.7616 * b[5] - w * 0.0168980;
            let out = b.iter().sum::<f64>() + w * 0.5362;
            b[6] = w * 0.115926;
            out as f32
        })
        .collect()
}

/// A synthetic music bed: chord changes every 0.5 s (four voices with harmonics, soft attack) and a
/// kick-like thump on each beat. Tonal, rhythmic, speech-band energy, no words.
pub fn music(rng: &mut Rng, n: usize) -> Vec<f32> {
    let roots = [220.0, 246.94, 196.0, 174.61, 261.63, 293.66];
    let beat = RATE / 2;
    let mut out = vec![0f32; n];
    let mut start = 0;
    while start < n {
        let root = *rng.pick(&roots);
        let chord = [root, root * 1.26, root * 1.498, root * 2.0];
        for (k, s) in out[start..(start + beat).min(n)].iter_mut().enumerate() {
            let t = k as f64 / RATE as f64;
            let env = (t / 0.02).min(1.0) * (-t * 3.0).exp();
            let mut v = 0.0;
            for f in chord {
                for h in 1..=3 {
                    v += (2.0 * std::f64::consts::PI * f * h as f64 * t).sin() / h as f64;
                }
            }
            let kick = (2.0 * std::f64::consts::PI * 60.0 * t).sin() * (-t * 30.0).exp() * 2.0;
            *s = (v * env * 0.25 + kick) as f32;
        }
        start += beat;
    }
    out
}

/// Babble: several unrelated talkers summed, each looped from a random phase.
pub fn babble(rng: &mut Rng, talkers: &[&[f32]], n: usize) -> Vec<f32> {
    let mut out = vec![0f32; n];
    for t in talkers.iter().filter(|t| !t.is_empty()) {
        let phase = rng.range(0, t.len() - 1);
        let level = 0.7 + 0.6 * rng.unit() as f32;
        for (i, o) in out.iter_mut().enumerate() {
            *o += t[(phase + i) % t.len()] * level;
        }
    }
    out
}

/// Host time of an utterance sample.
pub fn sample_ns(utterance_start_ns: u64, sample: usize) -> u64 {
    utterance_start_ns + sample as u64 * NS_PER_SAMPLE
}

/// Index of the frame on screen at `t_ns`: the newest frame pushed at or before it.
pub fn frame_at(t_ns: u64, screen_start_ns: u64, period_ns: u64) -> Option<u64> {
    (t_ns >= screen_start_ns).then(|| (t_ns - screen_start_ns) / period_ns)
}

/// Synthetic screen: an 8x4 grid of cells; the top two rows carry the frame index's 16 bits
/// (MSB first), the bottom two rows their complement, so a decoded frame validates itself.
pub const GRID_COLS: u32 = 8;
pub const GRID_ROWS: u32 = 4;
pub fn frame_rgb(index: u64, width: u32, height: u32) -> Vec<u8> {
    let mut rgb = vec![0u8; (width * height * 3) as usize];
    let (cw, ch) = (width / GRID_COLS, height / GRID_ROWS);
    // A per-frame tint in the margins and cell borders keeps every frame a real change.
    let tint = [(index * 37 % 200) as u8 + 30, (index * 91 % 200) as u8 + 30, (index * 53 % 200) as u8 + 30];
    for y in 0..height {
        for x in 0..width {
            let (col, row) = (x / cw, y / ch);
            let inner = col < GRID_COLS
                && row < GRID_ROWS
                && x % cw > cw / 10
                && x % cw < cw * 9 / 10
                && y % ch > ch / 10
                && y % ch < ch * 9 / 10;
            let px = if inner {
                let v = if cell_bit(index, row * GRID_COLS + col) { 235 } else { 16 };
                [v, v, v]
            } else {
                tint
            };
            let o = ((y * width + x) * 3) as usize;
            rgb[o..o + 3].copy_from_slice(&px);
        }
    }
    rgb
}
fn cell_bit(index: u64, cell: u32) -> bool {
    let bit = cell % 16;
    let value = (index >> (15 - bit)) & 1 == 1;
    if cell < 16 {
        value
    } else {
        !value
    }
}
/// Read the frame index back; `None` when the complement rows disagree (a blended or corrupt frame).
pub fn read_frame_index(rgb: &[u8], width: u32, height: u32) -> Option<u64> {
    let (cw, ch) = (width / GRID_COLS, height / GRID_ROWS);
    let mut bits = [false; 32];
    for cell in 0..32u32 {
        let (col, row) = (cell % GRID_COLS, cell / GRID_COLS);
        let (x0, y0) = (col * cw + cw * 3 / 10, row * ch + ch * 3 / 10);
        let (x1, y1) = (col * cw + cw * 7 / 10, row * ch + ch * 7 / 10);
        let mut sum = 0u64;
        let mut count = 0u64;
        for y in (y0..y1).step_by(4) {
            for x in (x0..x1).step_by(4) {
                let o = ((y * width + x) * 3) as usize;
                sum += rgb[o] as u64 + rgb[o + 1] as u64 + rgb[o + 2] as u64;
                count += 3;
            }
        }
        bits[cell as usize] = sum / count.max(1) > 128;
    }
    let mut index = 0u64;
    for b in 0..16 {
        if bits[b] == bits[b + 16] {
            return None;
        }
        index = (index << 1) | bits[b] as u64;
    }
    Some(index)
}

/// Candidate correction (evaluation only, not app code): move a phrase start that Parakeet placed in
/// the silence before the phrase forward to the first VAD speech block (512 samples, 32 ms) that ends
/// after it, never past `limit_ns` (the first word's end). `probs[i]` covers
/// `[case_start + i*32 ms, +32 ms)`.
pub fn vad_snap(probs: &[f32], case_start_ns: u64, start_ns: u64, limit_ns: u64, threshold: f32) -> u64 {
    const BLOCK_NS: u64 = 32_000_000;
    for (i, p) in probs.iter().enumerate() {
        let begin = case_start_ns + i as u64 * BLOCK_NS;
        if begin + BLOCK_NS <= start_ns {
            continue;
        }
        if begin >= limit_ns {
            break;
        }
        if *p >= threshold {
            return begin.max(start_ns);
        }
    }
    start_ns
}

/// Candidate correction (evaluation only): the first 5 ms frame in the first word's span whose RMS
/// reaches -20 dB of that span's loudest frame.
pub fn energy_snap(audio: &[f32], case_start_ns: u64, start_ns: u64, limit_ns: u64) -> u64 {
    let at = |ns: u64| (ns.saturating_sub(case_start_ns) / NS_PER_SAMPLE) as usize;
    let (a, b) = (at(start_ns).min(audio.len()), at(limit_ns).min(audio.len()));
    if b <= a {
        return start_ns;
    }
    let frames: Vec<f32> = audio[a..b].chunks(TRIM_FRAME).map(rms).collect();
    let peak = frames.iter().cloned().fold(0.0, f32::max);
    frames
        .iter()
        .position(|r| *r >= peak * 0.1)
        .map_or(start_ns, |i| sample_ns(case_start_ns, a + i * TRIM_FRAME).max(start_ns))
}

/// Percentile (nearest rank) of a sorted slice.
pub fn percentile(sorted: &[f64], p: f64) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    let rank = ((p / 100.0) * sorted.len() as f64).ceil().max(1.0) as usize;
    Some(sorted[rank.min(sorted.len()) - 1])
}

#[cfg(test)]
mod tests {
    use super::*;

    // Starts and ends at full amplitude, so the first and last samples are speech.
    fn tone(n: usize, amp: f32) -> Vec<f32> {
        (0..n).map(|i| amp * (i as f32 * std::f32::consts::PI).cos()).collect()
    }

    #[test]
    fn trim_finds_first_speech_sample_and_keeps_faded_pad() {
        let mut x = vec![0.0; 1000];
        x.extend(tone(3200, 0.5));
        x.extend(vec![0.0; 800]);
        let t = trim(&x).unwrap();
        assert_eq!(t.onset, TRIM_PAD);
        assert_eq!(t.offset - t.onset, 3200);
        assert_eq!(t.audio.len(), 3200 + 2 * TRIM_PAD);
        assert_eq!(t.audio[0], 0.0);
        // The kept speech is untouched.
        assert_eq!(t.audio[t.onset + 5], x[1000 + 5]);
        // Sample-exact inside a 5 ms frame (1003 is not a frame boundary).
        let mut y = vec![0.0; 1003];
        y.extend(tone(1601, 0.5));
        y.extend(vec![0.0; 77]);
        assert_eq!(speech_bounds(&y), Some((1003, 2604)));
        assert!(trim(&vec![0.0; 4000]).is_none());
    }

    #[test]
    fn quiet_onset_below_threshold_is_not_speech() {
        // -50 dB breath before a 0 dB word: trimmed away, onset at the word.
        let mut x = tone(800, 0.5 * 10f32.powf(-50.0 / 20.0));
        x.extend(tone(1600, 0.5));
        assert_eq!(speech_bounds(&x).unwrap().0, 800);
    }

    #[test]
    fn splice_places_target_onset_exactly() {
        let a = trim(&[vec![0.0; 500], tone(1600, 0.4), vec![0.0; 500]].concat()).unwrap();
        let b = trim(&[vec![0.0; 300], tone(2400, 0.4), vec![0.0; 300]].concat()).unwrap();
        let c = trim(&[vec![0.0; 100], tone(800, 0.4)].concat()).unwrap();
        let s = splice(&[a.clone(), b.clone(), c.clone()], &[2000, 1000], 1);
        assert_eq!(s.speech_start, a.onset);
        // a's speech is 1600 samples, then a 2000-sample gap.
        assert_eq!(s.target_onset, a.onset + 1600 + 2000);
        assert_eq!(s.speech_end, s.target_onset + 2400 + 1000 + 800);
        // The target onset really is where b's first speech sample is.
        assert_eq!(s.audio[s.target_onset + 7], b.audio[b.onset + 7]);
        // A gap shorter than the pads widens to them instead of overlapping speech.
        let tight = splice(&[a.clone(), b.clone()], &[10], 1);
        assert_eq!(tight.target_onset - (a.offset), (a.audio.len() - a.offset) + b.onset);
        // First segment as target.
        assert_eq!(splice(&[b.clone(), c], &[500], 0).target_onset, b.onset);
    }

    #[test]
    fn noise_gain_gives_requested_snr() {
        let speech = [vec![0.0; 1000], tone(8000, 0.3), vec![0.0; 1000]].concat();
        let mut rng = Rng(5);
        for make in [white as fn(&mut Rng, usize) -> Vec<f32>, pink, music] {
            let noise = make(&mut rng, speech.len());
            for snr in [20.0, 10.0, 5.0] {
                let g = noise_gain(&speech, (1000, 9000), &noise, snr);
                let pn = noise.iter().map(|v| (*v as f64 * g).powi(2)).sum::<f64>() / noise.len() as f64;
                let measured = 10.0 * (speech_power(&speech, (1000, 9000)) / pn).log10();
                assert!((measured - snr).abs() < 1e-6, "{measured} vs {snr}");
            }
        }
    }

    #[test]
    fn frame_on_screen_is_the_newest_at_or_before() {
        let (start, p) = (1_000_000_000, 200_000_000);
        assert_eq!(frame_at(start - 1, start, p), None);
        assert_eq!(frame_at(start, start, p), Some(0));
        assert_eq!(frame_at(start + p - 1, start, p), Some(0));
        assert_eq!(frame_at(start + p, start, p), Some(1));
        // A phrase onset 3.21 s into an utterance placed 10 s after the screen began.
        let onset = sample_ns(start + 10_000_000_000, 51_360);
        assert_eq!(onset, start + 13_210_000_000);
        assert_eq!(frame_at(onset, start, p), Some(66));
    }

    #[test]
    fn frame_pattern_round_trips_and_rejects_blends() {
        let (w, h) = (432, 280);
        for i in [0, 1, 2, 255, 4097, 65535] {
            assert_eq!(read_frame_index(&frame_rgb(i, w, h), w, h), Some(i));
        }
        // A 50/50 blend of two frames that differ in bits fails validation instead of reading as a third index.
        let (a, b) = (frame_rgb(5, w, h), frame_rgb(10, w, h));
        let blend: Vec<u8> = a.iter().zip(&b).map(|(x, y)| ((*x as u16 + *y as u16) / 2 + 1) as u8).collect();
        let read = read_frame_index(&blend, w, h);
        assert!(read != Some(15) && read != Some(0), "{read:?}");
    }

    #[test]
    fn snaps_move_an_early_start_to_speech_and_never_past_the_word() {
        let start = 1_000_000_000;
        // Speech from block 10 (320 ms); Parakeet said 100 ms.
        let probs: Vec<f32> = (0..30).map(|i| if i >= 10 { 0.9 } else { 0.05 }).collect();
        assert_eq!(vad_snap(&probs, start, start + 100_000_000, start + 800_000_000, 0.5), start + 320_000_000);
        // Already inside speech: unchanged.
        assert_eq!(vad_snap(&probs, start, start + 400_000_000, start + 800_000_000, 0.5), start + 400_000_000);
        // No speech before the word ends: unchanged.
        assert_eq!(vad_snap(&probs, start, start + 100_000_000, start + 300_000_000, 0.5), start + 100_000_000);
        let mut audio = vec![0.001f32; 16_000];
        audio[5_000..9_000].iter_mut().enumerate().for_each(|(i, v)| *v = 0.5 * (i as f32).cos());
        // 5000 samples = 312.5 ms; frames are 80 samples, so the frame starting at sample 4960 holds the onset.
        assert_eq!(energy_snap(&audio, start, start + 100_000_000, start + 900_000_000), sample_ns(start, 4960));
        assert_eq!(energy_snap(&audio, start, start + 400_000_000, start + 300_000_000), start + 400_000_000);
    }

    #[test]
    fn percentile_is_nearest_rank() {
        let v: Vec<f64> = (1..=10).map(|x| x as f64).collect();
        assert_eq!(percentile(&v, 50.0), Some(5.0));
        assert_eq!(percentile(&v, 90.0), Some(9.0));
        assert_eq!(percentile(&v, 99.0), Some(10.0));
        assert_eq!(percentile(&[], 50.0), None);
    }
}
