//! Bounded SentencePiece prefix automaton for contextual TDT vocabulary.
use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};

pub const TOKEN_COUNT: usize = 8193;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Vocabulary {
    /// Names and phrases ("T3 Code"): letters and digits, words separated by single spaces.
    pub words: Vec<String>,
    /// Additive logit bonus once a word has started. Blank is never boosted.
    pub boost: f32,
    /// Logit penalty on pieces only other languages use: the user speaks English and Polish, and
    /// Parakeet v3 otherwise drifts into Dutch, Russian, Swedish… on accented or short speech.
    /// See `foreign`. Zero disables it. Blank and configured vocabulary paths are never penalized.
    #[serde(default = "default_foreign_penalty")]
    pub foreign_penalty: f32,
}
pub const DEFAULT_FOREIGN_PENALTY: f32 = 6.0;
fn default_foreign_penalty() -> f32 {
    if cfg!(feature = "public") { 0.0 } else { DEFAULT_FOREIGN_PENALTY }
}
impl Default for Vocabulary {
    fn default() -> Self {
        Self {
            words: serde_json::from_str(DEFAULT_WORDS).expect("valid default vocabulary"),
            boost: 4.0,
            foreign_penalty: default_foreign_penalty(),
        }
    }
}
/// The user's words (his projects, models and city), or the public edition's few product words (docs/editions.md).
#[cfg(not(feature = "public"))]
const DEFAULT_WORDS: &str = include_str!("default-vocabulary.json");
#[cfg(feature = "public")]
const DEFAULT_WORDS: &str = include_str!("public-vocabulary.json");
impl Vocabulary {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.words.len() <= 128, "Vocabulary exceeds 128 words");
        ensure!(
            self.boost.is_finite() && (0.0..=8.0).contains(&self.boost),
            "Vocabulary boost must be between 0 and 8"
        );
        ensure!(
            self.foreign_penalty.is_finite() && (0.0..=16.0).contains(&self.foreign_penalty),
            "Foreign-language penalty must be between 0 and 16"
        );
        ensure!(
            self.words.iter().all(|w| !w.is_empty()
                && w.chars().count() <= 64
                && w.chars().any(char::is_alphabetic)
                && w.split(' ').all(|part| !part.is_empty() && part.chars().all(char::is_alphanumeric))),
            "Vocabulary entries must be words of letters and digits (single spaces between words), at most 64 characters"
        );
        Ok(())
    }
    pub(crate) fn from_env() -> Result<Self> {
        let mut value = Self::default();
        if let Ok(words) = std::env::var("HEIMDALL_ASR_VOCABULARY") {
            value.words = serde_json::from_str(&words)?;
        }
        value.add_extra();
        if let Ok(boost) = std::env::var("HEIMDALL_ASR_VOCABULARY_BOOST") {
            value.boost = boost.parse()?;
        }
        if let Ok(penalty) = std::env::var("HEIMDALL_ASR_FOREIGN_PENALTY") {
            value.foreign_penalty = penalty.parse()?;
        }
        value.validate()?;
        Ok(value)
    }
}

/// The host's own names to bias toward (Personalise, 26 Oct: the assistant's name), added to the
/// list at the next model load. Words that would not validate are skipped; the list keeps its 128-word bound.
static EXTRA: std::sync::RwLock<Vec<String>> = std::sync::RwLock::new(Vec::new());
pub fn set_extra_words(words: Vec<String>) {
    *EXTRA.write().unwrap() = words;
}
impl Vocabulary {
    fn add_extra(&mut self) {
        for w in EXTRA.read().unwrap().iter() {
            let w = w.split(|c: char| !c.is_alphanumeric()).filter(|p| !p.is_empty()).collect::<Vec<_>>().join(" ");
            let ok = !w.is_empty() && w.chars().count() <= 64 && w.chars().any(char::is_alphabetic);
            if ok && self.words.len() < 128 && !self.words.iter().any(|x| x.eq_ignore_ascii_case(&w)) {
                self.words.push(w);
            }
        }
    }
}

// Observed acoustic entries, not fuzzy transcript replacements. Each entry replaces
// only a leading spelling; the remaining canonical suffix must still be decoded.
const ENTRIES: &[(&str, &str, &str)] = &[
    ("heimdall", "hi", "hei"),
    ("heimdall", "hem", "heim"),
    ("heimdall", "him", "heim"),
    ("heimdall", "i'm", "heim"),
    ("heimdall", "hi'm", "heim"),
    ("qwen", "qu", "qw"),
    ("qwen", "ku", "qw"),
    ("nemotron", "nimo", "nemo"),
    ("silero", "sile", "siler"),
    ("ultron", "al", "ul"),
    ("mobius", "mop", "mob"),
    ("parakeet", "parakit", "parakeet"),
    // "T3 Code" heard as " T" "if" "ri" "co" (19 Oct 18:24, "Tifrico").
    ("t3code", "tifri", "t3"),
];

/// Lexicon key: lowercase, phrase words joined. SentencePiece paths cross word boundaries
/// (" T" "3" " Code"), so "T3 Code" is matched as "t3code" and emitted as two words.
fn key(word: &str) -> String {
    word.to_lowercase().replace(' ', "")
}

/// Word-initial pieces (vocabulary spelling, leading space) used by Parakeet v3's other
/// languages and by no common English or Polish word; generated by `scripts/foreign_pieces.py`.
const FOREIGN_INITIAL: &str = include_str!("foreign-pieces.json");
fn foreign_initial() -> &'static std::collections::HashSet<String> {
    static SET: std::sync::OnceLock<std::collections::HashSet<String>> = std::sync::OnceLock::new();
    SET.get_or_init(|| serde_json::from_str(FOREIGN_INITIAL).expect("valid foreign piece list"))
}
/// A piece English and Polish speech does not produce: any letter outside the English and
/// Polish alphabets (Cyrillic, Greek, ä/é/ß…), or a listed word-initial piece ("▁daar").
pub(crate) fn foreign(piece: &str) -> bool {
    piece
        .chars()
        .any(|c| c.is_alphabetic() && !c.is_ascii_alphabetic() && !"ąćęłńóśźżĄĆĘŁŃÓŚŹŻ".contains(c))
        || (piece.starts_with([' ', '▁']) && foreign_initial().contains(&piece.replacen('▁', " ", 1)))
}

/// Small shared transition policy for GPU table construction and token formatting.
/// Formatting follows completed decoder paths, including split acoustic entries;
/// it never rewrites a similar-looking word that did not complete such a path.
struct Lexicon {
    prefixes: Vec<String>,
    index: std::collections::HashMap<String, usize>,
    complete: std::collections::HashMap<usize, usize>,
    canonical_prefixes: std::collections::HashSet<String>,
    entry_prefixes: std::collections::HashSet<String>,
    entry_starts: std::collections::HashSet<String>,
}
impl Lexicon {
    fn new(config: &Vocabulary) -> Result<Self> {
        let mut value = Self {
            prefixes: vec![String::new()],
            index: std::collections::HashMap::new(),
            complete: std::collections::HashMap::new(),
            canonical_prefixes: std::collections::HashSet::new(),
            entry_prefixes: std::collections::HashSet::new(),
            entry_starts: std::collections::HashSet::new(),
        };
        for (word_index, canonical) in config.words.iter().enumerate() {
            let word = key(canonical);
            for end in word
                .char_indices()
                .skip(1)
                .map(|(i, _)| i)
                .chain(std::iter::once(word.len()))
            {
                value.canonical_prefixes.insert(word[..end].into());
            }
            let mut spellings = vec![word.clone()];
            for &(name, surface, prefix) in ENTRIES {
                if word == name {
                    spellings.push(format!("{surface}{}", &word[prefix.len()..]));
                    for end in surface
                        .char_indices()
                        .skip(1)
                        .map(|(i, _)| i)
                        .chain(std::iter::once(surface.len()))
                    {
                        value.entry_prefixes.insert(surface[..end].into());
                    }
                    // "al" is an ordinary word in several languages. Admit it
                    // acoustically, but never push it into unrelated speech.
                    if surface != "al" {
                        value.entry_starts.insert(surface.into());
                    }
                }
            }
            if word == "heimdall" {
                // The archive also emits the observed Germanic "dahl" suffix.
                // It still requires the complete name path, not a broad "d..." match.
                let dahl: Vec<_> = spellings
                    .iter()
                    .filter_map(|s| s.strip_suffix("dall").map(|p| format!("{p}dahl")))
                    .collect();
                spellings.extend(dahl);
            }
            for spelling in spellings {
                for end in spelling
                    .char_indices()
                    .skip(1)
                    .map(|(i, _)| i)
                    .chain(std::iter::once(spelling.len()))
                {
                    let prefix = &spelling[..end];
                    let next = value.prefixes.len();
                    let node = *value.index.entry(prefix.into()).or_insert_with(|| {
                        value.prefixes.push(prefix.into());
                        next
                    });
                    if end == spelling.len() {
                        value.complete.entry(node).or_insert(word_index);
                    }
                }
            }
        }
        ensure!(
            value.prefixes.len() <= 1024,
            "Vocabulary exceeds 1024 prefix nodes"
        );
        Ok(value)
    }
    fn transition(&self, node: usize, piece: &str) -> (usize, bool) {
        let boundary = piece.starts_with([' ', '▁']);
        let piece = piece.trim_start_matches([' ', '▁']).to_lowercase();
        if piece.is_empty() {
            return (0, false);
        }
        // A prefix may cross a SentencePiece word boundary (Hi Mdal, Nemo Tron).
        // A complete word can continue only along another configured path, and
        // gets no continuation bonus there (thread/threads, if configured).
        if node != 0 {
            if let Some(&next) = self.index.get(&format!("{}{piece}", self.prefixes[node])) {
                return (next, false);
            }
        }
        if boundary {
            if let Some(&next) = self.index.get(&piece) {
                return (next, true);
            }
        }
        (0, false)
    }
}

/// Each row maps an emitted token to a bounded prefix state. The high bit marks
/// a completion allowed to override blank; short words deliberately lack it.
pub(crate) struct BiasTrie {
    pub next: Vec<u32>,
    pub bonus: Vec<f32>,
}
impl BiasTrie {
    pub fn build(config: &Vocabulary, vocab: &[String]) -> Result<Self> {
        config.validate()?;
        ensure!(vocab.len() == TOKEN_COUNT - 1, "Vocabulary token schema");
        let lexicon = Lexicon::new(config)?;
        let mut next = vec![0; lexicon.prefixes.len() * TOKEN_COUNT];
        let mut bonus = vec![0.; next.len()];
        // Every row, so the prior holds inside and after a vocabulary word as at the root.
        let penalty: Vec<f32> = vocab
            .iter()
            .map(|piece| if config.foreign_penalty > 0. && foreign(piece) { -config.foreign_penalty } else { 0. })
            .collect();
        for node in 0..lexicon.prefixes.len() {
            for (token, piece) in vocab.iter().enumerate() {
                let (target, start) = lexicon.transition(node, piece);
                if target == 0 {
                    bonus[node * TOKEN_COUNT + token] = penalty[token];
                    continue;
                }
                let complete = lexicon
                    .complete
                    .get(&target)
                    .is_some_and(|&i| config.words[i].chars().count() >= 4);
                let offset = node * TOKEN_COUNT + token;
                next[offset] = target as u32 | if complete { 0x8000_0000 } else { 0 };
                let letters = piece.chars().filter(|c| c.is_alphabetic()).count();
                bonus[offset] = if start {
                    if letters >= 2 {
                        if lexicon
                            .canonical_prefixes
                            .contains(&lexicon.prefixes[target])
                        {
                            config.boost * 0.25
                        } else if lexicon.entry_starts.contains(&lexicon.prefixes[target]) {
                            (config.boost * 0.5).min(2.)
                        } else {
                            0.
                        }
                    } else {
                        0.
                    }
                } else if lexicon.complete.contains_key(&node)
                    || (lexicon.entry_prefixes.contains(&lexicon.prefixes[target])
                        && !lexicon
                            .canonical_prefixes
                            .contains(&lexicon.prefixes[target]))
                {
                    0.
                } else {
                    config.boost * if complete { 2. } else { 1. }
                };
            }
        }
        Ok(Self { next, bonus })
    }
    pub fn advance(&self, node: usize, token: usize) -> usize {
        if token == TOKEN_COUNT - 1 {
            node
        } else {
            (self.next[node * TOKEN_COUNT + token] & 0x7fff_ffff) as usize
        }
    }
}

/// Inclusive token spans that completed a configured long word. Match the GPU's
/// eight-frame expiry; retain timestamps/confidence from every consumed token.
pub(crate) fn completed_spans(
    config: &Vocabulary,
    vocab: &[String],
    tokens: &[crate::asr::Token],
) -> Vec<(usize, usize, String)> {
    let Ok(lexicon) = Lexicon::new(config) else {
        return vec![];
    };
    let (mut node, mut first, mut last_frame) = (0, 0, 0);
    let mut spans = vec![];
    for (i, token) in tokens.iter().enumerate() {
        if token.frame > last_frame + 8 {
            node = 0;
        }
        let Some(piece) = vocab.get(token.id) else {
            node = 0;
            continue;
        };
        let (next, start) = lexicon.transition(node, piece);
        if start {
            first = i;
        }
        node = next;
        last_frame = token.frame;
        if let Some(&word) = lexicon.complete.get(&node) {
            if config.words[word].chars().count() >= 4 {
                if spans.last().is_some_and(|(a, _, _)| *a == first) {
                    spans.pop();
                }
                spans.push((first, i, config.words[word].clone()));
            }
        }
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;
    /// Editions (docs/editions.md): the public vocabulary is valid and carries none of the user's own words, and the
    /// public build assumes no languages (no English-and-Polish penalty). Runs in both builds.
    #[test]
    fn edition_vocabulary_public_has_none_of_owners_words() {
        let public: Vec<String> = serde_json::from_str(include_str!("public-vocabulary.json")).unwrap();
        let owner: Vec<String> = serde_json::from_str(include_str!("default-vocabulary.json")).unwrap();
        Vocabulary { words: public.clone(), ..Vocabulary::default() }.validate().unwrap();
        for w in ["Vella", "Mobius", "Ultron", "Astra", "Poznań", "BiRefNet", "oMLX", "Nemotron"] {
            assert!(owner.iter().any(|t| t == w) && !public.iter().any(|p| p == w), "{w}");
        }
        let v = Vocabulary::default();
        assert_eq!(v.words, if cfg!(feature = "public") { public } else { owner });
        assert_eq!(v.foreign_penalty, if cfg!(feature = "public") { 0.0 } else { DEFAULT_FOREIGN_PENALTY });
    }
    fn tokens(pieces: &[&str]) -> Vec<String> {
        let mut vocab = vec![String::new(); TOKEN_COUNT - 1];
        for (i, piece) in pieces.iter().enumerate() {
            vocab[i] = (*piece).into();
        }
        vocab
    }
    #[test]
    fn acoustic_entries_need_a_completed_suffix_and_expire() {
        let vocab = tokens(&[
            " Hi", " M", "dal", "l", " Hem", " dal", " I'm", "dall", " there",
        ]);
        let config = Vocabulary {
            words: vec!["Heimdall".into()],
            ..Vocabulary::default()
        };
        let trie = BiasTrie::build(&config, &vocab).unwrap();
        assert_eq!(trie.bonus[0], 2.);
        let hi = trie.advance(0, 0);
        assert_eq!(trie.advance(hi, 8), 0);
        let mdal = trie.advance(trie.advance(hi, 1), 2);
        assert_ne!(trie.next[mdal * TOKEN_COUNT + 3] & 0x8000_0000, 0);
        assert_eq!(trie.bonus[mdal * TOKEN_COUNT + 3], 8.);
        for ids in [vec![0, 1, 2, 3], vec![4, 5, 3], vec![6, 7]] {
            let mut emitted: Vec<_> = ids
                .iter()
                .enumerate()
                .map(|(i, id)| crate::asr::Token {
                    id: *id,
                    frame: i + 1,
                    duration: 1,
                    probability: 0.8,
                })
                .collect();
            assert_eq!(
                completed_spans(&config, &vocab, &emitted),
                vec![(0, ids.len() - 1, "Heimdall".into())]
            );
            emitted.last_mut().unwrap().frame += 9;
            assert!(completed_spans(&config, &vocab, &emitted).is_empty());
        }
        assert!(completed_spans(
            &Vocabulary {
                words: vec![],
                ..config
            },
            &vocab,
            &[]
        )
        .is_empty());
    }
    #[test]
    fn short_words_never_override_blank_or_create_joined_word_splits() {
        let vocab = tokens(&[" P", "i", "ck", " So", "l", "ve", " Ma", "c"]);
        let config = Vocabulary {
            words: vec!["Pi".into(), "Sol".into(), "Mac".into()],
            ..Vocabulary::default()
        };
        let trie = BiasTrie::build(&config, &vocab).unwrap();
        for (first, last) in [(0, 1), (3, 4), (6, 7)] {
            let node = trie.advance(0, first);
            assert_eq!(trie.next[node * TOKEN_COUNT + last] & 0x8000_0000, 0);
            assert_eq!(trie.bonus[node * TOKEN_COUNT + last], 4.);
        }
    }
    #[test]
    fn incomplete_acoustic_entries_cannot_push_contractions_or_common_fragments() {
        let vocab = tokens(&[
            " I", "'m", " Hai", "m", " Al", "tron", " Hi", " M", "dal", "l", " h", "id", "all",
        ]);
        let config = Vocabulary {
            words: vec!["Heimdall".into(), "Ultron".into()],
            ..Vocabulary::default()
        };
        let trie = BiasTrie::build(&config, &vocab).unwrap();
        assert_eq!(trie.bonus[2], 0.); // The rejected partial "Haim" route is absent.
        assert_eq!(trie.bonus[4], 0.); // common "al" is admitted, never pushed.
        assert_eq!(trie.bonus[trie.advance(0, 0) * TOKEN_COUNT + 1], 0.); // I -> 'm
        assert_eq!(trie.bonus[trie.advance(0, 2) * TOKEN_COUNT + 3], 0.); // Hai -> m
        assert_eq!(trie.advance(trie.advance(0, 10), 11), 0); // rejected h + id + all
        assert_eq!(trie.bonus[6], 2.);
        let hi = trie.advance(0, 6);
        assert_eq!(trie.bonus[hi * TOKEN_COUNT + 7], 0.);
        let him = trie.advance(hi, 7);
        assert_eq!(trie.bonus[him * TOKEN_COUNT + 8], 4.);
    }
    #[test]
    fn defaults_are_bounded_and_override_can_disable_all_paths() {
        let config = Vocabulary::default();
        config.validate().unwrap();
        Lexicon::new(&config).unwrap();
        let trie = BiasTrie::build(
            &Vocabulary {
                words: vec![],
                ..config
            },
            &tokens(&[" Hi", "dall"]),
        )
        .unwrap();
        assert!(trie.next.iter().all(|&n| n == 0));
        assert!(trie.bonus.iter().all(|&n| n == 0.));
    }
    #[test]
    fn boundaries_blank_completion_and_unicode() {
        let mut vocab = vec![String::new(); TOKEN_COUNT - 1];
        for (i, s) in [" He", "im", "dal", "l", " made", "im", " Ż", "an", "eta"]
            .iter()
            .enumerate()
        {
            vocab[i] = (*s).into();
        }
        let config = Vocabulary {
            words: vec!["Heimdall".into(), "Żaneta".into()],
            ..Vocabulary::default()
        };
        let trie = BiasTrie::build(&config, &vocab).unwrap();
        assert_eq!(trie.bonus[0], 1.);
        let he = trie.advance(0, 0);
        assert_ne!(he, 0);
        assert_eq!(trie.advance(he, 8192), he);
        assert_eq!(trie.bonus[he * TOKEN_COUNT + 1], 4.);
        let done = trie.advance(trie.advance(trie.advance(he, 1), 2), 3);
        assert_ne!(done, 0);
        assert_eq!(trie.advance(done, 4), 0);
        assert_eq!(trie.advance(0, 5), 0); // never start inside another word
        assert_ne!(trie.advance(trie.advance(trie.advance(0, 6), 7), 8), 0);
    }
    #[test]
    fn foreign_pieces_are_other_scripts_and_listed_initial_words_only() {
        for piece in [" п", "ου", " é", "ä", "ß", " Ç", " daar", "▁daar", " zijn", " voor"] {
            assert!(foreign(piece), "{piece:?}");
        }
        // Polish letters, digits, punctuation, English, shared words ("je", "het") and
        // non-initial pieces stay free; English tech prefixes are never listed.
        for piece in [" the", " je", " het", " Ż", "ń", "ę", "3", ",", "daar", " var", " seg", " vec", " immer", " Heim", " T"] {
            assert!(!foreign(piece), "{piece:?}");
        }
        assert!(foreign_initial().len() > 400);
        assert!(foreign_initial().iter().all(|p| p.starts_with(' ') && p[1..].chars().all(|c| c.is_ascii_alphabetic() || c == '\'')));
    }
    #[test]
    fn foreign_penalty_holds_in_every_row_but_never_on_blank_or_vocabulary_paths() {
        let vocab = tokens(&[" Hi", "dall", " п", " daar", " the", " J", "äg", "er", "3"]);
        let config = Vocabulary {
            words: vec!["Heimdall".into(), "Jäger".into()],
            ..Vocabulary::default()
        };
        assert_eq!(config.foreign_penalty, DEFAULT_FOREIGN_PENALTY);
        let trie = BiasTrie::build(&config, &vocab).unwrap();
        let rows = trie.bonus.len() / TOKEN_COUNT;
        assert!(rows > 2);
        for row in 0..rows {
            let at = |t: usize| trie.bonus[row * TOKEN_COUNT + t];
            assert_eq!(at(2), -DEFAULT_FOREIGN_PENALTY, "Cyrillic in row {row}");
            assert_eq!(at(3), -DEFAULT_FOREIGN_PENALTY, "listed piece in row {row}");
            assert!(at(4) >= 0., "English piece in row {row}");
            assert_eq!(at(TOKEN_COUNT - 1), 0., "blank in row {row}");
        }
        // "äg" is foreign at the root, but on the configured "Jäger" path it keeps its bonus.
        assert_eq!(trie.bonus[6], -DEFAULT_FOREIGN_PENALTY);
        let j = trie.advance(0, 5);
        assert_ne!(j, 0);
        assert!(trie.bonus[j * TOKEN_COUNT + 6] > 0.);
        let off = BiasTrie::build(&Vocabulary { foreign_penalty: 0., ..config }, &vocab).unwrap();
        assert!(off.bonus.iter().all(|&b| b >= 0.));
    }
    #[test]
    fn phrases_cross_piece_boundaries_and_complete_only_on_the_whole_phrase() {
        let vocab = tokens(&[" T", "3", " Co", "de", "if", "ri", "co", " Code"]);
        let config = Vocabulary {
            words: vec!["T3 Code".into()],
            ..Vocabulary::default()
        };
        let trie = BiasTrie::build(&config, &vocab).unwrap();
        // A one-letter start is admitted, never pushed; the rest of the phrase is boosted.
        assert_eq!(trie.bonus[0], 0.);
        let t = trie.advance(0, 0);
        assert_eq!(trie.bonus[t * TOKEN_COUNT + 1], config.boost);
        let t3 = trie.advance(t, 1);
        let t3co = trie.advance(t3, 2);
        assert_ne!(t3co, 0);
        assert_eq!(trie.next[t3co * TOKEN_COUNT + 3] & 0x8000_0000, 0x8000_0000);
        assert_ne!(trie.next[t3 * TOKEN_COUNT + 7] & 0x8000_0000, 0); // " Code" in one piece
        // The observed "Tifrico" route completes only with the decoded "code" suffix.
        let tifrico = [4, 5, 6].iter().fold(t, |n, &id| trie.advance(n, id));
        assert_ne!(tifrico, 0);
        assert_ne!(trie.next[tifrico * TOKEN_COUNT + 3] & 0x8000_0000, 0);
        // "Code" alone is not the phrase.
        assert_eq!(trie.advance(0, 7), 0);
    }
    #[test]
    fn phrase_and_digit_entries_validate_and_defaults_carry_owner_names() {
        for words in [vec!["T3 Code"], vec!["oMLX"], vec!["Parakeet", "thread"]] {
            let config = Vocabulary { words: words.iter().map(|w| (*w).into()).collect(), ..Vocabulary::default() };
            config.validate().unwrap();
        }
        for bad in ["T3  Code", " T3", "T3 ", "3 4", "a-b", ""] {
            let config = Vocabulary { words: vec![bad.into()], ..Vocabulary::default() };
            assert!(config.validate().is_err(), "{bad:?}");
        }
        assert!(Vocabulary { foreign_penalty: 17., ..Vocabulary::default() }.validate().is_err());
        let defaults = Vocabulary::default();
        // Measured 20 Oct: "thread(s)" rewrote 3/200 ordinary controls ("Mm." -> "Them") and fixed nothing.
        assert!(!defaults.words.iter().any(|w| w.starts_with("thread")));
        for name in ["T3 Code", "Mobius", "Heimdall", "Parakeet", "oMLX"] {
            assert!(defaults.words.iter().any(|w| w == name), "{name}");
        }
        // Settings written before the penalty existed still load, with it on.
        let old: Vocabulary = serde_json::from_str(r#"{"words":["Heimdall"],"boost":4.0}"#).unwrap();
        assert_eq!(old.foreign_penalty, DEFAULT_FOREIGN_PENALTY);
    }
    #[test]
    fn invalid_config_is_rejected() {
        for config in [
            Vocabulary {
                boost: f32::NAN,
                ..Vocabulary::default()
            },
            Vocabulary {
                words: vec!["two-words".into()],
                ..Vocabulary::default()
            },
        ] {
            assert!(config.validate().is_err());
        }
    }
}
