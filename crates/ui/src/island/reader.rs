//! Fast reading (RSVP): the agent's `fast_read` text, one word at a time at a fixed point under the notch, the
//! recognition letter in red. This file is the platform-independent part: words, per-word timing, the recognition
//! letter, and the playback state the island's reader draws (`mac/rsvp.rs`).
//!
//! Timing (docs/research/rsvp-fast-reading.md): `duration = base × lengthFactor`, plus blank time after clause,
//! sentence and paragraph ends, all scaled so the whole message takes exactly `words / wpm` minutes. The dial is the
//! true average speed, pauses included.

/// The slow default (the research's 240 wpm: ordinary silent reading, at or below where RSVP starts losing
/// comprehension).
pub const DEFAULT_WPM: u32 = 240;
pub const MIN_WPM: u32 = 150;
pub const MAX_WPM: u32 = 450;
pub const STEP_WPM: u32 = 25;
/// The red focus point alone before the first word, so the eye settles where the words will appear.
pub const LEAD_IN_MS: f64 = 600.0;

/// A message the agent asked the user to fast-read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FastRead {
    pub id: String,
    pub text: String,
    pub title: Option<String>,
    /// The predicted starting speed (crates/app's speed memory), already on the 25-wpm grid.
    pub wpm: u32,
}

/// crates/app → the reader.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FastReadUpdate {
    /// A message is ready: the island shows the ready pill until the user clicks it. A message already showing is
    /// replaced and reported dismissed.
    Show(FastRead),
    /// A better starting speed arrived (the message's embedding) before the user started; ignored once he has.
    StartWpm { id: String, wpm: u32 },
    /// The app withdrew the message; no result is reported.
    Withdraw(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadOutcome {
    /// "Read it" after reaching the end (or its last tenth).
    Read,
    /// Closed before the end, or replaced by a newer message.
    Dismissed,
}

/// The reader → crates/app, once per message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadResult {
    pub id: String,
    pub outcome: ReadOutcome,
    pub start_wpm: u32,
    /// The speed the user settled on: the dial's time-weighted mean over the last two thirds of the message, or the
    /// dial when he left before reaching them.
    pub settled_wpm: u32,
    /// Back and repeat presses.
    pub rewinds: u32,
    /// The furthest word reached, in per cent of the message.
    pub progress_pct: u8,
}

/// Blank time after a word, in multiples of the base duration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pause {
    None,
    Clause,
    Sentence,
    Paragraph,
}

impl Pause {
    fn factor(self) -> f64 {
        match self {
            Pause::None => 0.0,
            Pause::Clause => 1.0,
            Pause::Sentence => 2.0,
            Pause::Paragraph => 3.5,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Word {
    pub text: String,
    /// Letters and digits only (punctuation is not counted in the length).
    pub letters: usize,
    pub pause: Pause,
    /// Which sentence the word belongs to (0-based).
    pub sentence: usize,
}

/// Length factor: short words faster, long words slower (Spritz patent baseline, research table).
pub fn length_factor(letters: usize) -> f64 {
    match letters {
        0..=4 => 0.85,
        5..=7 => 1.0,
        8..=13 => 1.3,
        _ => 1.6,
    }
}

const CLOSERS: &[char] = &['"', '\'', ')', ']', '}', '”', '’', '»'];

fn pause_after(token: &str) -> Pause {
    let core = token.trim_end_matches(CLOSERS);
    match core.chars().last() {
        Some('.' | '!' | '?' | '…') => Pause::Sentence,
        Some(',' | ';' | ':' | '—' | '–') => Pause::Clause,
        _ => Pause::None,
    }
}

/// Words of `text`: paragraphs split at blank lines, words at whitespace. A token with no letters (a lone dash)
/// joins the word before it rather than flashing on its own.
pub fn words(text: &str) -> Vec<Word> {
    let mut out: Vec<Word> = Vec::new();
    let mut sentence = 0;
    let normalized = text.replace("\r\n", "\n");
    let paragraphs: Vec<&str> = normalized.split("\n\n").map(str::trim).filter(|p| !p.is_empty()).collect();
    for paragraph in paragraphs {
        let start = out.len();
        for token in paragraph.split_whitespace() {
            let letters = token.chars().filter(|c| c.is_alphanumeric()).count();
            if letters == 0 && out.len() > start {
                if let Some(last) = out.last_mut() {
                    last.text.push(' ');
                    last.text.push_str(token);
                    last.pause = last.pause.max_with(pause_after(token).max_with(Pause::Clause));
                    continue;
                }
            }
            if out.len() > start && matches!(out.last().map(|w| w.pause), Some(Pause::Sentence)) {
                sentence += 1;
            }
            out.push(Word { text: token.to_string(), letters: letters.max(1), pause: pause_after(token), sentence });
        }
        if out.len() > start {
            let last = out.last_mut().unwrap();
            last.pause = Pause::Paragraph;
            sentence += 1;
        }
    }
    out
}

impl Pause {
    fn rank(self) -> u8 {
        match self {
            Pause::None => 0,
            Pause::Clause => 1,
            Pause::Sentence => 2,
            Pause::Paragraph => 3,
        }
    }
    fn max_with(self, other: Pause) -> Pause {
        if other.rank() > self.rank() { other } else { self }
    }
}

/// The recognition letter: the character index (into `word.chars()`) about a third into the word's letters,
/// leading punctuation skipped.
pub fn focus_index(word: &str) -> usize {
    let chars: Vec<char> = word.chars().collect();
    let letters = chars.iter().filter(|c| c.is_alphanumeric()).count();
    let nth = match letters {
        0 | 1 => 0,
        2..=5 => 1,
        6..=9 => 2,
        10..=13 => 3,
        _ => 4,
    };
    chars.iter().enumerate().filter(|(_, c)| c.is_alphanumeric()).nth(nth).map_or(0, |(i, _)| i)
}

/// The dial's positions: 25-wpm steps through the 240 default (165, 190, … 440), plus the range's ends.
pub fn grid() -> Vec<u32> {
    let mut g = vec![MIN_WPM];
    let mut w = DEFAULT_WPM - ((DEFAULT_WPM - MIN_WPM) / STEP_WPM) * STEP_WPM;
    while w <= MAX_WPM {
        if w > MIN_WPM && w < MAX_WPM {
            g.push(w);
        }
        w += STEP_WPM;
    }
    g.push(MAX_WPM);
    g
}

/// The nearest dial position (a prediction or a measured mean lands on a value the buttons can reach).
pub fn on_grid(wpm: f64) -> u32 {
    grid().into_iter().min_by(|a, b| (*a as f64 - wpm).abs().total_cmp(&(*b as f64 - wpm).abs())).unwrap_or(DEFAULT_WPM)
}

/// Per-word display and blank times (ms) for a message, scaled so they average exactly to the dial.
#[derive(Clone, Debug)]
pub struct Timing {
    factors: Vec<(f64, f64)>,
    /// Normalisation: words / Σ(length + pause factors).
    scale: f64,
}

impl Timing {
    pub fn new(words: &[Word]) -> Timing {
        let factors: Vec<(f64, f64)> = words.iter().map(|w| (length_factor(w.letters), w.pause.factor())).collect();
        let total: f64 = factors.iter().map(|(l, p)| l + p).sum();
        let scale = if total > 0.0 { words.len() as f64 / total } else { 1.0 };
        Timing { factors, scale }
    }

    /// How long word `i` shows, then how long the blank after it lasts, at `wpm`.
    pub fn of(&self, i: usize, wpm: u32) -> (f64, f64) {
        let base = 60_000.0 / wpm as f64;
        let (l, p) = self.factors[i];
        (l * self.scale * base, p * self.scale * base)
    }

    /// The whole message at `wpm`, lead-in excluded (ms).
    pub fn total_ms(&self, wpm: u32) -> f64 {
        (0..self.factors.len()).map(|i| { let (s, b) = self.of(i, wpm); s + b }).sum()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Open below the notch, waiting for the user to click play.
    Ready,
    /// The red focus point alone, before the first word.
    LeadIn,
    /// A word shows (or, with `blank`, the pause after it).
    Playing,
    /// Stopped on a word; the reader shows its whole sentence.
    Paused,
    /// Past the last word: repeat or "read it".
    Ended,
}

/// One message's playback. Pure state: the view asks `delay_ms` for its one-shot timer and calls `advance` when it
/// fires.
#[derive(Clone, Debug)]
pub struct Playback {
    pub message: FastRead,
    pub words: Vec<Word>,
    pub timing: Timing,
    pub wpm: u32,
    pub start_wpm: u32,
    pub phase: Phase,
    /// The current word.
    pub index: usize,
    /// The blank after the current word is showing.
    pub blank: bool,
    pub rewinds: u32,
    /// The furthest word shown.
    pub reached: usize,
    /// Σ wpm × time and Σ time over words in the last two thirds (for the settled speed).
    weighted: f64,
    weight: f64,
}

impl Playback {
    pub fn new(message: FastRead) -> Playback {
        let words = words(&message.text);
        let timing = Timing::new(&words);
        let wpm = on_grid(message.wpm as f64);
        Playback { message, words, timing, wpm, start_wpm: wpm, phase: Phase::Ready, index: 0, blank: false, rewinds: 0, reached: 0, weighted: 0.0, weight: 0.0 }
    }

    pub fn word(&self) -> Option<&Word> {
        self.words.get(self.index)
    }

    /// The ready pill's estimate of the reading time at the current speed (ms).
    pub fn estimate_ms(&self) -> f64 {
        self.timing.total_ms(self.wpm)
    }

    /// The next timer's delay, or none (ready, paused, ended: nothing runs).
    pub fn delay_ms(&self) -> Option<f64> {
        match self.phase {
            Phase::LeadIn => Some(LEAD_IN_MS),
            Phase::Playing if !self.words.is_empty() => {
                let (show, blank) = self.timing.of(self.index, self.wpm);
                Some(if self.blank { blank } else { show })
            }
            _ => None,
        }
    }

    /// The user clicked the ready pill.
    pub fn start(&mut self) {
        if self.phase == Phase::Ready {
            self.phase = if self.words.is_empty() { Phase::Ended } else { Phase::LeadIn };
        }
    }

    /// The timer fired: the next word, the blank after this one, or the end.
    pub fn advance(&mut self) {
        match self.phase {
            Phase::LeadIn => {
                self.phase = Phase::Playing;
                self.blank = false;
            }
            Phase::Playing => {
                let (show, blank) = self.timing.of(self.index, self.wpm);
                let spent = if self.blank { blank } else { show };
                if 3 * self.index >= self.words.len() {
                    self.weighted += spent * self.wpm as f64;
                    self.weight += spent;
                }
                if !self.blank && blank > 0.0 {
                    self.blank = true;
                    return;
                }
                self.blank = false;
                if self.index + 1 >= self.words.len() {
                    self.phase = Phase::Ended;
                } else {
                    self.index += 1;
                    self.reached = self.reached.max(self.index);
                }
            }
            _ => {}
        }
    }

    /// Click on the words: pause (showing the sentence) or resume from the current word.
    pub fn toggle_pause(&mut self) {
        match self.phase {
            Phase::LeadIn | Phase::Playing => {
                self.phase = Phase::Paused;
                self.blank = false;
            }
            Phase::Paused => self.phase = Phase::Playing,
            Phase::Ended => self.repeat(),
            Phase::Ready => self.start(),
        }
    }

    /// Back: replay from the start of the current sentence (the previous one when already at its first word),
    /// playing.
    pub fn back(&mut self) {
        if self.words.is_empty() {
            return;
        }
        if self.phase == Phase::Ended {
            self.index = self.words.len() - 1;
        }
        let start = self.sentence_start(self.index);
        self.index = if start == self.index && start > 0 && self.phase != Phase::Ended { self.sentence_start(start - 1) } else { start };
        self.blank = false;
        self.rewinds += 1;
        self.phase = Phase::Playing;
    }

    /// Repeat: the whole message again, from the lead-in.
    pub fn repeat(&mut self) {
        if self.words.is_empty() {
            return;
        }
        self.index = 0;
        self.blank = false;
        self.rewinds += 1;
        self.phase = Phase::LeadIn;
    }

    /// Slower or faster by one step; whether the dial moved.
    pub fn nudge(&mut self, faster: bool) -> bool {
        let g = grid();
        let next = if faster { g.iter().copied().find(|w| *w > self.wpm) } else { g.iter().rev().copied().find(|w| *w < self.wpm) }.unwrap_or(self.wpm);
        let moved = next != self.wpm;
        self.wpm = next;
        moved
    }

    fn sentence_start(&self, i: usize) -> usize {
        let s = self.words[i].sentence;
        let mut j = i;
        while j > 0 && self.words[j - 1].sentence == s {
            j -= 1;
        }
        j
    }

    /// The current word's sentence as a word range.
    pub fn sentence(&self) -> std::ops::Range<usize> {
        if self.words.is_empty() {
            return 0..0;
        }
        let i = self.index.min(self.words.len() - 1);
        let start = self.sentence_start(i);
        let s = self.words[i].sentence;
        let mut end = i + 1;
        while end < self.words.len() && self.words[end].sentence == s {
            end += 1;
        }
        start..end
    }

    /// How far through the message (0–1) the current word is, for the progress line.
    pub fn progress(&self) -> f64 {
        if self.words.is_empty() || self.phase == Phase::Ended {
            return 1.0;
        }
        if self.phase == Phase::Ready || self.phase == Phase::LeadIn {
            return 0.0;
        }
        (self.index as f64 + if self.blank { 1.0 } else { 0.5 }) / self.words.len() as f64
    }

    fn progress_pct(&self) -> u8 {
        if self.words.is_empty() || self.phase == Phase::Ended {
            return if self.phase == Phase::Ready { 0 } else { 100 };
        }
        if self.phase == Phase::Ready {
            return 0;
        }
        (((self.reached + 1) as f64 / self.words.len() as f64) * 100.0).round().min(100.0) as u8
    }

    /// The speed he settled on (see `ReadResult::settled_wpm`).
    pub fn settled_wpm(&self) -> u32 {
        if self.weight > 0.0 { on_grid(self.weighted / self.weight) } else { self.wpm }
    }

    /// "Read it": read when he reached the end or its last tenth, dismissed otherwise.
    pub fn finish(&self) -> ReadResult {
        let pct = self.progress_pct();
        self.result(if pct >= 90 { ReadOutcome::Read } else { ReadOutcome::Dismissed })
    }

    pub fn result(&self, outcome: ReadOutcome) -> ReadResult {
        ReadResult {
            id: self.message.id.clone(),
            outcome,
            start_wpm: self.start_wpm,
            settled_wpm: self.settled_wpm(),
            rewinds: self.rewinds,
            progress_pct: self.progress_pct(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEXT: &str = "The build failed because tests timed out. Three things matter. First, the limit is two minutes, and the tests need longer. Second, the slow part is loading the embedding model, which happens one target at a time.\n\nThird, loading all three targets together would fix it. A longer limit would also work, but it hides the problem.";

    fn message(wpm: u32) -> FastRead {
        FastRead { id: "fastread-1".into(), text: TEXT.into(), title: Some("Build".into()), wpm }
    }

    /// The dial is the true average: the per-word show and blank times sum to words / wpm, at every speed.
    #[test]
    fn durations_sum_to_words_over_wpm_at_every_speed() {
        let w = words(TEXT);
        let timing = Timing::new(&w);
        for wpm in grid() {
            let expected = w.len() as f64 / wpm as f64 * 60_000.0;
            let total = timing.total_ms(wpm);
            assert!((total - expected).abs() < 1e-6, "{wpm} wpm: {total} ms, expected {expected} ms");
        }
        // Long words and sentence ends really are longer than short words.
        let (short, _) = timing.of(0, 240); // "The"
        let (long, _) = timing.of(w.iter().position(|x| x.text == "embedding").unwrap(), 240);
        assert!(long > short * 1.4, "{long} vs {short}");
        let (_, end) = timing.of(w.iter().position(|x| x.text == "out.").unwrap(), 240);
        let (_, comma) = timing.of(w.iter().position(|x| x.text == "First,").unwrap(), 240);
        let (_, none) = timing.of(1, 240);
        assert!(end > comma && comma > none && none == 0.0);
        let para = w.iter().position(|x| x.text == "time.").unwrap();
        assert_eq!(w[para].pause, Pause::Paragraph);
        assert!(timing.of(para, 240).1 > end);
    }

    /// Playing a message through its timer at a fixed speed takes exactly words / wpm (plus the lead-in).
    #[test]
    fn playback_through_the_timer_takes_words_over_wpm() {
        for wpm in [150, 240, 450] {
            let mut p = Playback::new(message(wpm));
            p.start();
            let mut elapsed = 0.0;
            let mut fires = 0;
            while let Some(d) = p.delay_ms() {
                elapsed += d;
                p.advance();
                fires += 1;
                assert!(fires < 10_000);
            }
            assert_eq!(p.phase, Phase::Ended);
            let expected = LEAD_IN_MS + p.words.len() as f64 / wpm as f64 * 60_000.0;
            assert!((elapsed - expected).abs() < 1e-6, "{elapsed} vs {expected}");
            let r = p.finish();
            assert_eq!((r.outcome, r.settled_wpm, r.progress_pct, r.rewinds), (ReadOutcome::Read, wpm, 100, 0));
        }
    }

    #[test]
    fn words_split_sentences_paragraphs_and_lone_dashes() {
        let w = words("Point first — then detail.\n\nNew paragraph here.");
        let texts: Vec<_> = w.iter().map(|w| w.text.as_str()).collect();
        assert_eq!(texts, ["Point", "first —", "then", "detail.", "New", "paragraph", "here."]);
        assert_eq!(w[1].pause, Pause::Clause);
        assert_eq!(w[3].pause, Pause::Paragraph);
        assert_eq!(w.iter().map(|w| w.sentence).collect::<Vec<_>>(), [0, 0, 0, 0, 1, 1, 1]);
        assert_eq!(words("  \n\n ").len(), 0);
        assert_eq!(pause_after("(this)."), Pause::Sentence);
        assert_eq!(pause_after("said,\""), Pause::Clause);
    }

    #[test]
    fn recognition_letter_sits_about_a_third_in() {
        assert_eq!(focus_index("a"), 0);
        assert_eq!(focus_index("the"), 1);
        assert_eq!(focus_index("reading"), 2);
        assert_eq!(focus_index("comprehension"), 3);
        assert_eq!(focus_index("\"quoted"), 3);
        assert_eq!(focus_index("internationalization"), 4);
    }

    #[test]
    fn back_replays_the_sentence_and_repeat_the_message() {
        let mut p = Playback::new(message(240));
        p.start();
        p.advance(); // lead-in done
        while p.word().unwrap().text != "matter." {
            p.advance();
        }
        p.toggle_pause();
        assert_eq!(p.phase, Phase::Paused);
        let s = p.sentence();
        let shown: Vec<_> = p.words[s].iter().map(|w| w.text.as_str()).collect();
        assert_eq!(shown, ["Three", "things", "matter."]);
        p.back();
        assert_eq!((p.phase, p.word().unwrap().text.as_str(), p.rewinds), (Phase::Playing, "Three", 1));
        // Already at the sentence's first word: back goes to the previous sentence.
        p.back();
        assert_eq!(p.word().unwrap().text, "The");
        p.repeat();
        assert_eq!((p.phase, p.index, p.rewinds), (Phase::LeadIn, 0, 3));
    }

    #[test]
    fn speed_steps_clamp_and_settle_on_the_late_dial() {
        let mut p = Playback::new(message(440));
        assert!(p.nudge(true));
        assert_eq!(p.wpm, 450);
        assert!(!p.nudge(true));
        p.wpm = 160;
        assert!(p.nudge(false) && p.wpm == 150 && !p.nudge(false));
        // Read the first third at 240, then two steps up for the rest: settles at 290, not the start.
        let mut p = Playback::new(message(240));
        p.start();
        p.advance();
        while p.phase == Phase::Playing {
            if 3 * p.index >= p.words.len() && p.wpm == 240 {
                assert!(p.nudge(true) && p.nudge(true));
            }
            p.advance();
        }
        let r = p.finish();
        assert_eq!((r.start_wpm, r.settled_wpm, r.outcome), (240, 290, ReadOutcome::Read));
    }

    #[test]
    fn leaving_early_is_dismissed_with_its_progress() {
        let mut p = Playback::new(message(240));
        assert_eq!(p.finish().progress_pct, 0);
        assert_eq!(p.finish().outcome, ReadOutcome::Dismissed);
        p.start();
        p.advance();
        for _ in 0..10 {
            p.advance();
        }
        let r = p.finish();
        assert_eq!(r.outcome, ReadOutcome::Dismissed);
        assert!(r.progress_pct > 0 && r.progress_pct < 50, "{}", r.progress_pct);
        assert_eq!(r.settled_wpm, 240);
        assert_eq!(on_grid(263.0), 265);
        assert_eq!(on_grid(240.0), 240);
        assert_eq!(grid().first(), Some(&150));
        assert_eq!(grid()[1], 165);
        assert_eq!(grid().last(), Some(&450));
        assert!(grid().windows(2).all(|w| w[1] - w[0] <= STEP_WPM && w[1] > w[0]));
        assert_eq!(on_grid(90.0), 150);
        assert_eq!(on_grid(900.0), 450);
    }
}
