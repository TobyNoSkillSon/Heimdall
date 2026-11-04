//! Case/punctuation-normalized word edit distance and two-speaker cpWER.
use serde::{Deserialize, Serialize};
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct Score {
    pub errors: usize,
    pub reference_words: usize,
}
impl Score {
    pub fn rate(self) -> f64 {
        self.errors as f64 / self.reference_words.max(1) as f64
    }
}
fn words(s: &str) -> Vec<String> {
    s.to_lowercase()
        .split_whitespace()
        .filter_map(|w| {
            let w: String = w
                .chars()
                .filter(|c| c.is_alphanumeric() || *c == '\'')
                .collect();
            (!w.is_empty()).then_some(w)
        })
        .collect()
}
pub fn wer(reference: &str, hypothesis: &str) -> Score {
    let a = words(reference);
    let b = words(hypothesis);
    let mut row: Vec<_> = (0..=b.len()).collect();
    for (i, x) in a.iter().enumerate() {
        let mut previous = row[0];
        row[0] = i + 1;
        for (j, y) in b.iter().enumerate() {
            let old = row[j + 1];
            row[j + 1] = (previous + usize::from(x != y))
                .min(row[j] + 1)
                .min(old + 1);
            previous = old;
        }
    }
    Score {
        errors: row[b.len()],
        reference_words: a.len(),
    }
}
pub fn cpwer(reference: [&str; 2], hypothesis: [&str; 2]) -> Score {
    let direct = wer(reference[0], hypothesis[0]).errors + wer(reference[1], hypothesis[1]).errors;
    let swapped = wer(reference[0], hypothesis[1]).errors + wer(reference[1], hypothesis[0]).errors;
    Score {
        errors: direct.min(swapped),
        reference_words: words(reference[0]).len() + words(reference[1]).len(),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn word_and_permutation_errors() {
        assert_eq!(wer("Hello, a cat!", "hello a dog").errors, 1);
        assert_eq!(cpwer(["one two", "three"], ["three", "one two"]).errors, 0);
        assert_eq!(cpwer(["one two", "three"], ["one two", ""]).errors, 1);
        assert_eq!(wer("", "extra").errors, 1);
    }
}
