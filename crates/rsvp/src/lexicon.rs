//! Word knowledge: frequency ranks (English and Polish), function words, syllable counts.

use std::collections::HashMap;
use std::sync::OnceLock;

/// About 10,000 English words, most frequent first (data/gen_en.py: Whisper tokenizer merge order filtered by
/// the system dictionary).
const EN: &str = include_str!("../data/en.txt");
/// About 280 frequent Polish words, hand-ordered from most to least frequent.
const PL: &str = include_str!("../data/pl.txt");

/// The rank a word outside both lists is treated as having (rarer than anything listed).
pub const UNKNOWN_RANK: u32 = 30_000;

fn table() -> &'static HashMap<&'static str, u32> {
    static TABLE: OnceLock<HashMap<&'static str, u32>> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut map = HashMap::with_capacity(10_400);
        for (i, w) in EN.lines().enumerate() {
            map.entry(w).or_insert(i as u32 + 1);
        }
        // Polish ranks are spread over the same scale: the hand-ordered top 280 of Polish roughly covers ranks
        // 1 to 2,000 of a full Polish list, so index i maps to about 7 × i.
        for (i, w) in PL.lines().enumerate() {
            map.entry(w).or_insert(i as u32 * 7 + 1);
        }
        map
    })
}

/// Builds the lookup table now (otherwise the first plan pays about a millisecond for it).
pub fn warm() {
    let _ = table();
    let _ = roles();
}

fn exact(word: &str) -> Option<u32> {
    table().get(word).copied()
}

/// Frequency rank of a lowercase word: 1 is the most frequent. Possessives and contractions use their base
/// word; regular inflections not in the list use their stem, counted as somewhat rarer.
pub fn rank(lower: &str) -> Option<u32> {
    if let Some(r) = exact(lower) {
        return Some(r);
    }
    let word = lower.replace('’', "'");
    for suffix in ["n't", "'s", "'re", "'ll", "'ve", "'d", "'m", "'"] {
        if let Some(base) = word.strip_suffix(suffix) {
            let base = match (suffix, base) {
                ("n't", "ca") => "can",
                ("n't", "wo") => "will",
                ("n't", "sha") => "shall",
                _ => base,
            };
            if let Some(r) = exact(base) {
                return Some(r);
            }
        }
    }
    const STEMS: &[(&str, &str)] =
        &[("ies", "y"), ("ied", "y"), ("es", ""), ("s", ""), ("ed", ""), ("ed", "e"), ("d", ""), ("ing", ""), ("ing", "e"), ("ly", ""), ("er", ""), ("est", "")];
    for (suffix, add) in STEMS {
        if let Some(stem) = word.strip_suffix(suffix) {
            if stem.chars().count() >= 2 {
                let candidate = format!("{stem}{add}");
                if let Some(r) = exact(&candidate) {
                    return Some(r.saturating_mul(2).min(UNKNOWN_RANK));
                }
            }
        }
    }
    None
}

/// How a word behaves in chunking and timing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// Articles, prepositions, auxiliaries, conjunctions, pronouns: lead the next word into a chunk and weigh
    /// less inside one (they are mostly read in the same glance as their neighbour).
    Function,
    /// Determiners that carry meaning (numerals, "very", "no"): lead the next word into a chunk at full weight.
    Leader,
    /// Everything else.
    Content,
}

const EN_FUNCTION: &[&str] = &[
    "a", "an", "the", "of", "to", "in", "on", "at", "by", "for", "from", "with", "into", "onto", "over", "under", "about", "as",
    "than", "then", "and", "or", "but", "nor", "so", "yet", "if", "that", "this", "these", "those", "it", "its", "is", "are",
    "was", "were", "be", "been", "being", "am", "do", "does", "did", "has", "have", "had", "will", "would", "can", "could",
    "shall", "should", "may", "might", "must", "not", "i", "me", "my", "we", "us", "our", "you", "your", "he", "him", "his",
    "she", "her", "they", "them", "their", "who", "whom", "which", "what", "when", "where", "why", "how", "there", "here",
    "up", "out", "off", "via", "per", "vs", "because", "through", "between", "after", "before", "while", "since", "until",
    "unless", "though", "although", "whether", "both", "either", "neither", "just", "also", "too", "each", "every", "any",
    "some", "all", "such", "own", "i'm", "i'll", "i've", "i'd", "it's", "that's", "there's", "we're", "we'll", "we've",
    "you're", "you'll", "they're", "don't", "doesn't", "didn't", "isn't", "aren't", "wasn't", "won't", "can't",
];

const EN_LEADER: &[&str] = &[
    "no", "very", "more", "most", "less", "least", "much", "many", "few", "one", "two", "three", "four", "five", "six",
    "seven", "eight", "nine", "ten", "first", "second", "third", "last", "next", "other", "another", "new", "old", "same",
];

const PL_FUNCTION: &[&str] = &[
    "w", "we", "i", "z", "ze", "na", "do", "o", "od", "po", "za", "u", "a", "ale", "że", "to", "się", "nie", "jak", "czy",
    "już", "tak", "by", "bo", "co", "ten", "ta", "te", "tę", "tym", "tej", "tego", "jest", "są", "był", "była", "było",
    "byli", "będzie", "przez", "dla", "przy", "pod", "nad", "przed", "między", "oraz", "lub", "albo", "ani", "niż", "gdy",
    "kiedy", "jeśli", "żeby", "aby", "też", "także", "jego", "jej", "ich", "mi", "mnie", "go", "mu", "ją", "je", "sobie",
    "który", "która", "które", "którego", "której", "których",
];

const PL_LEADER: &[&str] = &["bardzo", "jeden", "jedna", "jedno", "dwa", "dwie", "trzy", "cztery", "pięć", "każdy", "każda", "nowy", "nowa", "nowe"];

/// Function words that can close the phrase before them (auxiliaries, object pronouns, particles): "the limit
/// is", "zgłosiła się". Articles, prepositions and conjunctions always lead the phrase after them instead.
const TRAILING: &[&str] = &[
    "is", "are", "was", "were", "be", "been", "am", "do", "does", "did", "has", "have", "had", "will", "would", "can", "could",
    "should", "may", "might", "must", "not", "it", "me", "him", "her", "us", "them", "you", "up", "out", "off", "too", "also",
    "here", "there", "się", "jest", "są", "był", "była", "było", "byli", "będzie", "go", "mu", "ją", "je", "mi", "mnie", "nie", "też", "już",
];

/// Whether a function word may join the unit before it when it cannot join the one after.
pub fn trails(lower: &str) -> bool {
    flags(lower) & TRAILS != 0
}

const FUNCTION: u8 = 1;
const LEADER: u8 = 2;
const TRAILS: u8 = 4;

fn roles() -> &'static HashMap<&'static str, u8> {
    static ROLES: OnceLock<HashMap<&'static str, u8>> = OnceLock::new();
    ROLES.get_or_init(|| {
        let mut map = HashMap::new();
        for (list, flag) in [(EN_FUNCTION, FUNCTION), (PL_FUNCTION, FUNCTION), (EN_LEADER, LEADER), (PL_LEADER, LEADER), (TRAILING, TRAILS)] {
            for w in list {
                *map.entry(*w).or_insert(0) |= flag;
            }
        }
        map
    })
}

fn flags(lower: &str) -> u8 {
    if lower.contains('’') {
        roles().get(lower.replace('’', "'").as_str()).copied().unwrap_or(0)
    } else {
        roles().get(lower).copied().unwrap_or(0)
    }
}

pub fn role(lower: &str) -> Role {
    let f = flags(lower);
    if f & FUNCTION != 0 {
        Role::Function
    } else if f & LEADER != 0 {
        Role::Leader
    } else {
        Role::Content
    }
}

fn is_vowel(c: char) -> bool {
    matches!(
        c,
        'a' | 'e' | 'i' | 'o' | 'u' | 'y' | 'ą' | 'ę' | 'ó' | 'á' | 'à' | 'â' | 'ä' | 'é' | 'è' | 'ê' | 'ë' | 'í' | 'ì' | 'î' | 'ï' | 'ò' | 'ô' | 'ö' | 'ú' | 'ù' | 'û' | 'ü' | 'ý'
    )
}

/// Syllables of a lowercase alphabetic word: vowel groups, minus an English silent final "e" ("make", not
/// "table"). At least one. Polish vowel groups ("się", "piękny") count correctly as written.
pub fn syllables(lower: &str) -> usize {
    let chars: Vec<char> = lower.chars().filter(|c| c.is_alphabetic()).collect();
    if chars.is_empty() {
        return 0;
    }
    let mut groups = 0;
    let mut prev = false;
    for &c in &chars {
        let v = is_vowel(c);
        if v && !prev {
            groups += 1;
        }
        prev = v;
    }
    let n = chars.len();
    if groups > 1 && n > 2 && chars[n - 1] == 'e' && !is_vowel(chars[n - 2]) && !(chars[n - 2] == 'l' && n > 3 && !is_vowel(chars[n - 3])) {
        groups -= 1;
    }
    groups.max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranks_order_common_before_rare() {
        assert!(rank("the").unwrap() < 5);
        assert!(rank("because").unwrap() < rank("deadline").unwrap());
        assert!(rank("zyzzogeton").is_none());
        assert_eq!(rank("don't"), rank("do"));
        assert_eq!(rank("problem's"), rank("problem"));
        assert!(rank("się").unwrap() < 100);
        assert!(rank("timeouts").is_none() || rank("timeouts").unwrap() > 1000);
    }

    #[test]
    fn syllable_counts() {
        for (w, n) in [("the", 1), ("make", 1), ("table", 2), ("embedding", 3), ("reading", 2), ("się", 1), ("piękny", 2), ("implementation", 5), ("a", 1)] {
            assert_eq!(syllables(w), n, "{w}");
        }
    }

    #[test]
    fn roles() {
        assert_eq!(role("the"), Role::Function);
        assert_eq!(role("three"), Role::Leader);
        assert_eq!(role("build"), Role::Content);
        assert_eq!(role("się"), Role::Function);
        assert_eq!(role("it’s"), Role::Function);
    }
}
