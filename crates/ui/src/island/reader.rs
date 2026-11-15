//! Fast reading (RSVP): the agent's `fast_read` text, one word at a time at a fixed point under the notch, the
//! recognition letter in red. This file holds what crates/app and the island exchange about a message (`FastRead`,
//! `FastReadUpdate`, `ReadResult`), the dial's grid, and `words`, which crates/app uses to measure a message. The
//! island plays crates/rsvp plans (`object/reader.rs`), whose timing replaced the per-word engine that lived here
//! (audit ISL-7, 14 Nov).

/// The slow default (the research's 240 wpm: ordinary silent reading, at or below where RSVP starts losing
/// comprehension).
pub use rsvp::DEFAULT_WPM;
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

#[derive(Clone, Debug, PartialEq)]
pub struct Word {
    pub text: String,
    /// Letters and digits only (punctuation is not counted in the length).
    pub letters: usize,
    pub pause: Pause,
    /// Which sentence the word belongs to (0-based).
    pub sentence: usize,
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

/// The nearest dial position (`rsvp::dial`: a prediction or a measured mean lands on a value the buttons can reach).
pub fn on_grid(wpm: f64) -> u32 {
    rsvp::dial().into_iter().min_by(|a, b| (*a as f64 - wpm).abs().total_cmp(&(*b as f64 - wpm).abs())).unwrap_or(DEFAULT_WPM)
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn speeds_land_on_the_dial() {
        assert_eq!(on_grid(263.0), 265);
        assert_eq!(on_grid(240.0), 240);
        assert_eq!(on_grid(90.0), 150);
        assert_eq!(on_grid(900.0), 450);
    }
}
