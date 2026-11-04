//! Fast-reading (RSVP) planner: text in, a timed sequence of display units out.
//!
//! Pure functions, no I/O, no dependencies. `plan(text, &Config)` tokenises the text, groups short function words
//! with their neighbours into 1–3-word units ("of the", "is two minutes,", "timed out."), gives every unit its own
//! display time from length, syllables, word frequency and kind (numbers, names, acronyms, URLs are slower),
//! adds blank time after clause, sentence and paragraph ends, lengthens the first unit after a pause, and scales
//! everything so the whole message takes exactly `words / wpm` minutes. Each unit carries its recognition letter
//! (`focus_index`). See README.md for the model and its sources.

mod lexicon;
mod token;

pub use lexicon::{rank as frequency_rank, warm, Role, UNKNOWN_RANK};
pub use token::{tokenize, Kind, Pause, Token};

use std::ops::Range;

/// The research default: ordinary silent reading, at or below where RSVP starts losing comprehension.
pub const DEFAULT_WPM: u32 = 240;
pub const MIN_WPM: u32 = 150;
pub const MAX_WPM: u32 = 450;
pub const STEP_WPM: u32 = 25;

/// The dial's positions: 25-wpm steps through the 240 default (165, 190, … 440) plus the range's ends; the same
/// grid the island reader uses.
pub fn dial() -> Vec<u32> {
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

/// Planner parameters. Factors are multiples of the base time `60000 / wpm` ms before normalisation.
#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    /// The dial: average words per minute over the whole message, pauses included.
    pub wpm: u32,
    /// Most words in one unit; 1 shows single words.
    pub max_chunk_words: usize,
    /// Widest unit in characters, spaces and punctuation included.
    pub max_chunk_chars: usize,
    /// Words with at least this many letters (and every URL or code name) show alone, except that one function
    /// word of up to three letters may precede a long word ("the onboarding"; never a URL).
    pub long_word_letters: usize,
    /// Shortest and longest time a unit is on screen (the blank after it is not limited).
    pub min_unit_ms: u32,
    pub max_unit_ms: u32,
    /// Blank time after a comma, semicolon, colon, dash or closing bracket.
    pub clause_pause: f64,
    /// Blank time after a sentence of 8–22 words; shorter sentences get 0.8 of it, longer ones 1.25.
    pub sentence_pause: f64,
    /// Blank time after a paragraph.
    pub paragraph_pause: f64,
    /// The first unit of the message and the first after a sentence or paragraph pause show this much longer;
    /// after a clause pause, half the extra.
    pub restart_factor: f64,
    /// Weight of a function word inside a multi-word unit (it is read in the same glance as its neighbour).
    pub light_word_weight: f64,
    /// Extra time per tenfold drop in word frequency, as a fraction of the word's time.
    pub frequency_effect: f64,
    pub number_factor: f64,
    pub name_factor: f64,
    pub acronym_factor: f64,
    pub literal_factor: f64,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            wpm: DEFAULT_WPM,
            max_chunk_words: 3,
            max_chunk_chars: 16,
            long_word_letters: 10,
            min_unit_ms: 100,
            max_unit_ms: 2000,
            clause_pause: 1.0,
            sentence_pause: 2.0,
            paragraph_pause: 3.5,
            restart_factor: 1.2,
            light_word_weight: 0.5,
            frequency_effect: 0.06,
            number_factor: 1.35,
            name_factor: 1.2,
            acronym_factor: 1.2,
            literal_factor: 1.3,
        }
    }
}

/// One display step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unit {
    /// The words shown together, separated by single spaces, punctuation included.
    pub text: String,
    /// The recognition letter: a char index into `text.chars()` (not a byte index). In a multi-word unit it sits
    /// in the main word, so the reader's eye lands on the content word and takes the short words around it in.
    pub focus_index: usize,
    /// How long the unit shows.
    pub duration_ms: u32,
    /// The blank after it (0 for the last unit: the reader decides how to end).
    pub pause_after_ms: u32,
    pub sentence_index: usize,
    pub word_count: usize,
}

/// Where a moment of playback falls.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Position {
    pub unit: usize,
    /// True while the unit shows; false in the blank after it.
    pub showing: bool,
    /// Milliseconds since the unit appeared.
    pub offset_ms: u32,
}

/// A timed message.
#[derive(Clone, Debug, PartialEq)]
pub struct Plan {
    pub units: Vec<Unit>,
    /// Sum of every duration and pause.
    pub total_ms: u32,
    /// Words in the message (what `wpm` counts).
    pub words: usize,
    pub wpm: u32,
    /// Per unit: display and pause weights in base units, before normalisation (kept for `retime`).
    weights: Vec<(f64, f64)>,
    starts: Vec<u32>,
    words_before: Vec<usize>,
    /// First unit of each sentence.
    sentence_starts: Vec<usize>,
    min_unit_ms: u32,
    max_unit_ms: u32,
}

/// The recognition letter of one word: a char index about a third into its letters (slightly left of centre,
/// further left for long words), leading punctuation skipped.
pub fn focus_index(word: &str) -> usize {
    let letters = word.chars().filter(|c| c.is_alphanumeric()).count();
    let nth = match letters {
        0 | 1 => 0,
        2..=5 => 1,
        6..=9 => 2,
        10..=13 => 3,
        _ => 4,
    };
    word.chars().enumerate().filter(|(_, c)| c.is_alphanumeric()).nth(nth).map_or(0, |(i, _)| i)
}

fn chars(s: &str) -> usize {
    s.chars().count()
}

/// Groups tokens into display units: a function word leads the next word into its unit ("of the build"), a
/// numeral or "very" leads a content word ("three things", "40 percent"), consecutive names stay together, and a
/// short particle closing a clause joins the word before it ("timed out."). Never across a pause, never past
/// `max_chunk_words` or `max_chunk_chars`; a long word shares its unit only with one short function word in front
/// of it ("the onboarding").
fn chunk(tokens: &[Token], c: &Config) -> Vec<Range<usize>> {
    let long = |t: &Token| t.letters >= c.long_word_letters || t.kind == Kind::Literal;
    // A one-to-three-letter function word ("a", "the", "of", "w") may sit in front of a long word: it costs the
    // eye almost nothing and would otherwise flash alone.
    let article = |t: &Token| t.role == Role::Function && t.letters <= 3 && t.kind == Kind::Word;
    let mut out = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        let mut end = i + 1;
        let mut width = chars(&tokens[i].text);
        while end < tokens.len() {
            if end - i >= c.max_chunk_words.max(1) {
                break;
            }
            let (last, next) = (&tokens[end - 1], &tokens[end]);
            if last.pause != Pause::None || long(last) || long(next) && !(end - i == 1 && article(last) && next.kind != Kind::Literal) {
                break;
            }
            let w = width + 1 + chars(&next.text);
            if w > c.max_chunk_chars {
                break;
            }
            let leads = match last.role {
                Role::Function => true,
                Role::Leader => next.role != Role::Function,
                Role::Content => false,
            };
            let names = last.kind == Kind::Name && next.kind == Kind::Name;
            let particle = next.role == Role::Function && next.kind == Kind::Word && next.letters <= 3 && next.pause >= Pause::Clause;
            if !(leads || names || particle) {
                break;
            }
            width = w;
            end += 1;
            if particle && !leads {
                break;
            }
        }
        // Stopped (by word count or width) with a function word or numeral last, before the word it introduces
        // ("it is a" | "test", "is two" | "minutes,"): hand it to the next unit ("it is" | "a test", "is" | "two
        // minutes,") when the two fit together.
        if end < tokens.len() && end - i > 1 {
            let (last, next) = (&tokens[end - 1], &tokens[end]);
            let fits = chars(&last.text) + 1 + chars(&next.text) <= c.max_chunk_chars;
            if last.role != Role::Content && last.pause == Pause::None && (!long(next) || article(last) && next.kind != Kind::Literal) && fits {
                end -= 1;
            }
        }
        out.push(i..end);
        i = end;
    }
    // An auxiliary, object pronoun or particle left alone (its next word was long, or taken by a better pair)
    // joins the unit before it when nothing separates them: "the limit is" | "two minutes," rather than a lone
    // "is". Articles and prepositions stay with what follows, even alone ("with" | "one decision:").
    let mut merged: Vec<Range<usize>> = Vec::with_capacity(out.len());
    for r in out {
        let lone = r.len() == 1 && tokens[r.start].role == Role::Function && tokens[r.start].kind == Kind::Word && lexicon::trails(tokens[r.start].text.to_lowercase().trim_matches(|c: char| !c.is_alphanumeric() && c != '\'' && c != '’'));
        if let Some(prev) = merged.last_mut() {
            let before = &tokens[prev.end - 1];
            let width: usize = tokens[prev.clone()].iter().map(|t| chars(&t.text) + 1).sum::<usize>() + chars(&tokens[r.start].text);
            if lone && prev.end == r.start && before.pause == Pause::None && !long(before) && prev.len() < c.max_chunk_words && width <= c.max_chunk_chars {
                prev.end = r.end;
                continue;
            }
        }
        merged.push(r);
    }
    merged
}

fn word_weight(t: &Token, c: &Config) -> f64 {
    // 0.55 + 0.05 per letter + 0.08 per syllable: a continuous version of the Spritz patent's bands (about
    // 0.85 for four letters, 1.0 for six, 1.3 for ten, 1.6 for fourteen).
    // Letters count up to 20 (40 for URLs and paths, which are read character by character).
    let cap = if t.kind == Kind::Literal { 40 } else { 20 };
    let length = 0.55 + 0.05 * t.letters.min(cap) as f64 + 0.08 * t.syllables.min(8) as f64;
    let kind = match t.kind {
        Kind::Word => {
            let rank = t.rank.unwrap_or(UNKNOWN_RANK).max(1) as f64;
            let min = 1.0 - c.frequency_effect * 1.33;
            (min + c.frequency_effect * rank.log10()).clamp(min, min + c.frequency_effect * (UNKNOWN_RANK as f64).log10())
        }
        Kind::Name => c.name_factor,
        Kind::Acronym => c.acronym_factor,
        Kind::Number => c.number_factor,
        Kind::Literal => c.literal_factor,
        Kind::Symbol => 1.0,
    };
    length * kind
}

impl Plan {
    fn build(tokens: &[Token], c: &Config) -> Plan {
        let ranges = chunk(tokens, c);
        let mut units = Vec::with_capacity(ranges.len());
        let mut weights = Vec::with_capacity(ranges.len());
        let mut words_before = Vec::with_capacity(ranges.len());
        let mut sentence_starts = Vec::new();
        // Words per sentence, for the sentence pause.
        let sentences = tokens.last().map_or(0, |t| t.sentence + 1);
        let mut sentence_words = vec![0usize; sentences];
        for t in tokens {
            sentence_words[t.sentence] += 1;
        }
        let mut words = 0;
        let mut previous = Pause::Sentence;
        for (u, range) in ranges.iter().enumerate() {
            let group = &tokens[range.clone()];
            let multi = group.len() > 1;
            let mut display: f64 = group.iter().map(|t| word_weight(t, c) * if multi && t.role == Role::Function { c.light_word_weight } else { 1.0 }).sum();
            display *= match previous {
                Pause::Sentence | Pause::Paragraph => c.restart_factor,
                Pause::Clause => 1.0 + (c.restart_factor - 1.0) / 2.0,
                Pause::None => 1.0,
            };
            let last = group.last().unwrap();
            let last_unit = u + 1 == ranges.len();
            let pause = if last_unit {
                0.0
            } else {
                match last.pause {
                    Pause::None => 0.0,
                    Pause::Clause => c.clause_pause,
                    Pause::Sentence => {
                        let n = sentence_words[last.sentence];
                        c.sentence_pause * if n <= 7 { 0.8 } else if n > 22 { 1.25 } else { 1.0 }
                    }
                    Pause::Paragraph => c.paragraph_pause,
                }
            };
            previous = last.pause;
            // Focus on the main word: the last that is not a function word (or the longest, if all are).
            let head = group.iter().rposition(|t| t.role != Role::Function && t.kind != Kind::Symbol).unwrap_or_else(|| {
                group.iter().enumerate().max_by_key(|(i, t)| (t.letters, usize::MAX - i)).map_or(0, |(i, _)| i)
            });
            let offset: usize = group[..head].iter().map(|t| chars(&t.text) + 1).sum();
            let text = group.iter().map(|t| t.text.as_str()).collect::<Vec<_>>().join(" ");
            let sentence = group[0].sentence;
            if sentence_starts.len() <= sentence {
                sentence_starts.resize(sentence + 1, u);
            }
            words_before.push(words);
            words += group.len();
            units.push(Unit { text, focus_index: offset + focus_index(&group[head].text), duration_ms: 0, pause_after_ms: 0, sentence_index: sentence, word_count: group.len() });
            weights.push((display, pause));
        }
        let mut plan = Plan { units, total_ms: 0, words, wpm: c.wpm, weights, starts: Vec::new(), words_before, sentence_starts, min_unit_ms: c.min_unit_ms, max_unit_ms: c.max_unit_ms };
        plan.retime(c.wpm);
        plan
    }

    /// Re-times the same units for a new dial speed (unit indices stay valid, so playback keeps its place).
    pub fn retime(&mut self, wpm: u32) {
        let wpm = wpm.max(1);
        self.wpm = wpm;
        let base = 60_000.0 / wpm as f64;
        let target = self.words as f64 * base;
        let (lo_ms, hi_ms) = (self.min_unit_ms as f64, (self.max_unit_ms.max(self.min_unit_ms)) as f64);
        let pause_sum: f64 = self.weights.iter().map(|w| w.1).sum();
        let total = |s: f64| -> f64 { self.weights.iter().map(|(d, _)| (s * d * base).clamp(lo_ms, hi_ms)).sum::<f64>() + s * pause_sum * base };
        // Find the scale s at which clamped display times plus scaled pauses add up to the target. The sum is
        // non-decreasing in s, so bisection converges; when even the limits cannot reach it, the nearest wins.
        let unclamped: f64 = self.weights.iter().map(|w| w.0 + w.1).sum();
        let mut hi = if unclamped > 0.0 { self.words as f64 / unclamped } else { 1.0 };
        let mut lo = 0.0;
        let mut grow = 0;
        while total(hi) < target && grow < 60 {
            lo = hi;
            hi *= 2.0;
            grow += 1;
        }
        for _ in 0..64 {
            let mid = 0.5 * (lo + hi);
            if total(mid) < target {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        let s = hi;
        // Cumulative rounding: every value is within a millisecond of its exact time and the sum never drifts.
        let mut exact = 0.0f64;
        let mut rounded = 0u64;
        let mut starts = Vec::with_capacity(self.units.len());
        for (unit, (d, p)) in self.units.iter_mut().zip(&self.weights) {
            starts.push(rounded as u32);
            exact += (s * d * base).clamp(lo_ms, hi_ms);
            let shown = exact.round() as u64 - rounded;
            rounded += shown;
            exact += s * p * base;
            let blank = exact.round() as u64 - rounded;
            rounded += blank;
            unit.duration_ms = shown as u32;
            unit.pause_after_ms = blank as u32;
        }
        self.starts = starts;
        self.total_ms = rounded as u32;
    }

    /// When unit `i` appears (ms from the first unit).
    pub fn start_ms(&self, i: usize) -> u32 {
        self.starts.get(i).copied().unwrap_or(self.total_ms)
    }

    /// The unit at `t_ms` after the first appeared; none past the end.
    pub fn at(&self, t_ms: u32) -> Option<Position> {
        if t_ms >= self.total_ms || self.units.is_empty() {
            return None;
        }
        let unit = self.starts.partition_point(|&s| s <= t_ms) - 1;
        let offset_ms = t_ms - self.starts[unit];
        Some(Position { unit, showing: offset_ms < self.units[unit].duration_ms, offset_ms })
    }

    /// Time left from the start of unit `i`.
    pub fn remaining_ms(&self, i: usize) -> u32 {
        self.total_ms - self.start_ms(i)
    }

    /// Words shown before unit `i` (progress, and the settled-speed bookkeeping).
    pub fn words_before(&self, i: usize) -> usize {
        self.words_before.get(i).copied().unwrap_or(self.words)
    }

    pub fn sentences(&self) -> usize {
        self.sentence_starts.len()
    }

    /// The first unit of unit `i`'s sentence.
    pub fn sentence_start(&self, i: usize) -> usize {
        self.units.get(i).map_or(0, |u| self.sentence_starts[u.sentence_index])
    }

    /// "Back": the start of the current sentence, or of the previous one when `i` already is a sentence start.
    pub fn back(&self, i: usize) -> usize {
        let start = self.sentence_start(i);
        if start == i && i > 0 { self.sentence_start(i - 1) } else { start }
    }

    /// The first unit of the next sentence; none in the last.
    pub fn next_sentence(&self, i: usize) -> Option<usize> {
        let s = self.units.get(i)?.sentence_index;
        self.sentence_starts.get(s + 1).copied()
    }

    /// The units of sentence `s`.
    pub fn sentence_range(&self, s: usize) -> Range<usize> {
        let start = self.sentence_starts.get(s).copied().unwrap_or(self.units.len());
        let end = self.sentence_starts.get(s + 1).copied().unwrap_or(self.units.len());
        start..end
    }

    /// Sentence `s` as one line, for the paused view.
    pub fn sentence_text(&self, s: usize) -> String {
        self.units[self.sentence_range(s)].iter().map(|u| u.text.as_str()).collect::<Vec<_>>().join(" ")
    }
}

/// Plans `text` at `config.wpm`.
pub fn plan(text: &str, config: &Config) -> Plan {
    Plan::build(&tokenize(text), config)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit_texts(text: &str) -> Vec<String> {
        plan(text, &Config::default()).units.into_iter().map(|u| u.text).collect()
    }

    #[test]
    fn chunks_function_words_with_their_neighbours() {
        assert_eq!(unit_texts("The build failed because tests timed out."), vec!["The build", "failed", "because tests", "timed out."]);
        assert_eq!(unit_texts("First, the limit is two minutes, and the tests need longer."), vec!["First,", "the limit", "is two minutes,", "and the tests", "need", "longer."]);
        assert_eq!(unit_texts("It is a test of the system."), vec!["It is", "a test", "of the system."]);
        assert_eq!(unit_texts("The limit is comprehensive."), vec!["The limit is", "comprehensive."]);
        assert_eq!(unit_texts("They ended with comprehensive notes."), vec!["They ended", "with", "comprehensive", "notes."]);
        assert_eq!(unit_texts("Read the implementation notes."), vec!["Read", "the", "implementation", "notes."]);
        assert_eq!(unit_texts("It says a dependency changed."), vec!["It says", "a dependency", "changed."]);
        assert_eq!(unit_texts("Ada Lovelace wrote 40 percent of it."), vec!["Ada Lovelace", "wrote", "40 percent", "of it."]);
    }

    #[test]
    fn single_word_mode() {
        let c = Config { max_chunk_words: 1, ..Config::default() };
        assert!(plan("of the build in a minute", &c).units.iter().all(|u| u.word_count == 1));
    }

    #[test]
    fn focus_lands_in_the_main_word() {
        let p = plan("of the build", &Config::default());
        let u = &p.units[0];
        assert_eq!(u.text, "of the build");
        assert_eq!(u.text.chars().nth(u.focus_index), Some('u'));
        assert_eq!(focus_index("implementation"), 4);
        assert_eq!(focus_index("\"Hello"), 2);
        assert_eq!(focus_index("a"), 0);
    }

    #[test]
    fn seek_helpers() {
        let p = plan("One two three. Four five six. Seven.", &Config::default());
        assert_eq!(p.at(0).unwrap().unit, 0);
        assert!(p.at(p.total_ms).is_none());
        let last = p.units.len() - 1;
        assert_eq!(p.at(p.total_ms - 1).unwrap().unit, last);
        for i in 0..p.units.len() {
            let pos = p.at(p.start_ms(i)).unwrap();
            assert_eq!((pos.unit, pos.showing, pos.offset_ms), (i, true, 0));
        }
        let s1 = p.sentence_range(1).start;
        assert_eq!(p.back(s1 + 1), s1);
        assert_eq!(p.back(s1), 0);
        assert_eq!(p.back(0), 0);
        assert_eq!(p.next_sentence(0), Some(s1));
        assert_eq!(p.next_sentence(last), None);
        assert_eq!(p.sentence_text(1), "Four five six.");
        assert_eq!(p.words_before(s1), 3);
        assert_eq!(p.remaining_ms(0), p.total_ms);
    }

    #[test]
    fn empty_text_is_an_empty_plan() {
        let p = plan("", &Config::default());
        assert!(p.units.is_empty());
        assert_eq!(p.total_ms, 0);
        assert!(p.at(0).is_none());
    }
}
