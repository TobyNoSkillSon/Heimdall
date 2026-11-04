//! Tokenisation: text → words with their kind, role, length, frequency and the pause after them.
//!
//! Splits at whitespace only, so numbers ("1,000.50", "10:30", "2026-10-18", "$5", "40%"), hyphenated words,
//! contractions, URLs, paths and file names stay whole. Em dashes and ellipses glued between two words split
//! after the mark; a lone dash or ellipsis joins the word before it as a pause. Light markdown (backticks, bold,
//! headings, bullets) is removed rather than shown.

use crate::lexicon::{self, Role};

/// Blank time after a word, weakest to strongest.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Pause {
    None,
    /// Comma, semicolon, colon, dash, closing parenthesis.
    Clause,
    /// Full stop, question mark, exclamation mark, ellipsis, a line break before a new line.
    Sentence,
    /// Blank line (and the end of the text).
    Paragraph,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Word,
    /// Capitalised and not an ordinary sentence-initial word ("Heimdall", "the user's").
    Name,
    /// All capitals ("GPU", "CI", "U.S.", "APIs").
    Acronym,
    /// Contains a digit ("240", "3.14", "v2", "M4", "40%").
    Number,
    /// URL, e-mail, path, file or code name ("https://…", "~/Work", "fast_read.rs", "Plan::retime").
    Literal,
    /// A standalone symbol ("&", "+", "→").
    Symbol,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Token {
    /// What is shown, punctuation included.
    pub text: String,
    pub kind: Kind,
    pub role: Role,
    /// Letters and digits of the word itself (punctuation around it is not counted).
    pub letters: usize,
    pub syllables: usize,
    /// Frequency rank (1 = most frequent); none when the word is in neither list.
    pub rank: Option<u32>,
    pub pause: Pause,
    pub sentence: usize,
    pub paragraph: usize,
}

const OPENERS: &[char] = &['(', '[', '{', '"', '“', '‘', '«', '„', '\'', '¿', '¡'];
const CLOSERS: &[char] = &[')', ']', '}', '"', '”', '’', '»', '\''];
const TERMINAL: &[char] = &['.', '!', '?', '…'];
const CLAUSE: &[char] = &[',', ';', ':'];
const DASHES: &[&str] = &["-", "–", "—", "--", "---", "―"];
const BULLETS: &[&str] = &["-", "*", "•", "·", "–", "—", "+", "‣", "◦", ">"];
/// Abbreviations whose full stop never ends a sentence (lowercase, dots removed).
const ABBREVIATIONS: &[&str] = &[
    "mr", "mrs", "ms", "dr", "prof", "sr", "jr", "st", "vs", "eg", "ie", "cf", "approx", "fig", "inc", "ltd", "co", "al", "dept",
    "est", "min", "max", "np", "tj", "tzn", "wg", "ul", "godz", "ok", "pl", "nr", "ds", "mgr", "inż", "dot", "am", "pm",
];
/// Abbreviations that often end a list, and so a sentence: a stop only when the next word starts one.
const LIST_ENDS: &[&str] = &["etc", "itd", "itp", "itede"];

/// A whitespace piece before classification.
struct Piece {
    text: String,
    line_start: bool,
    /// A numbered list marker ("1.", "2)", "a)") at the start of a line.
    marker: bool,
    /// The first word after a bullet ("- ", "• ").
    bullet: bool,
}

fn strip_markdown(piece: &str) -> String {
    if !piece.contains(['`', '*', '_']) {
        return piece.to_string();
    }
    let mut s = piece.replace('`', "").replace("**", "").replace("__", "");
    // *emphasis* and _emphasis_ wrappers.
    for wrap in ['*', '_'] {
        let trimmed_start = s.trim_start_matches(OPENERS);
        let lead = s.len() - trimmed_start.len();
        if trimmed_start.starts_with(wrap) {
            s.replace_range(lead..lead + wrap.len_utf8(), "");
        }
        let core_end = s.trim_end_matches(|c: char| CLOSERS.contains(&c) || TERMINAL.contains(&c) || CLAUSE.contains(&c)).len();
        if s[..core_end].ends_with(wrap) && core_end > wrap.len_utf8() {
            s.replace_range(core_end - wrap.len_utf8()..core_end, "");
        }
    }
    s
}

/// Splits "slow—really" into "slow—" and "really", and "wait...what" into "wait..." and "what".
fn split_inner(piece: &str, out: &mut Vec<String>) {
    if !piece.contains(['—', '–', '…']) && !piece.contains("--") && !piece.contains("...") {
        out.push(piece.to_string());
        return;
    }
    let chars: Vec<char> = piece.chars().collect();
    let mut start = 0;
    let mut i = 0;
    while i < chars.len() {
        let mark_len = if chars[i] == '—' || chars[i] == '–' || chars[i] == '…' {
            1
        } else if chars[i] == '-' && chars.get(i + 1) == Some(&'-') {
            2
        } else if chars[i] == '.' && chars.get(i + 1) == Some(&'.') && chars.get(i + 2) == Some(&'.') {
            3
        } else {
            0
        };
        if mark_len > 0 {
            let before = i > start && chars[i - 1].is_alphanumeric();
            let after = chars.get(i + mark_len).is_some_and(|c| c.is_alphanumeric() || OPENERS.contains(c));
            // An en dash between digits is a range ("10–12"): keep it whole.
            let range = chars[i] == '–' && before && chars[i - 1].is_ascii_digit() && chars.get(i + 1).is_some_and(|c| c.is_ascii_digit());
            if before && after && !range {
                out.push(chars[start..i + mark_len].iter().collect());
                start = i + mark_len;
            }
            i += mark_len;
        } else {
            i += 1;
        }
    }
    if start < chars.len() {
        out.push(chars[start..].iter().collect());
    }
}

fn is_marker(piece: &str) -> bool {
    let core = piece.trim_end_matches(['.', ')']);
    piece.len() > core.len()
        && !core.is_empty()
        && (core.chars().all(|c| c.is_ascii_digit()) && core.len() <= 3 || core.chars().count() == 1 && core.chars().all(|c| c.is_ascii_lowercase()))
}

fn pieces(paragraph: &[&str]) -> Vec<Piece> {
    let mut out = Vec::new();
    for line in paragraph {
        let mut first = true;
        let mut bullet = false;
        let raw: Vec<&str> = line.split_whitespace().collect();
        for (i, token) in raw.iter().enumerate() {
            if first && token.chars().all(|c| c == '#') {
                continue; // heading marker
            }
            if first && BULLETS.contains(token) && i + 1 < raw.len() {
                bullet = true;
                continue; // list bullet
            }
            let cleaned = strip_markdown(token);
            if cleaned.is_empty() {
                continue;
            }
            let mut split = Vec::new();
            split_inner(&cleaned, &mut split);
            for s in split {
                let marker = first && is_marker(&s);
                out.push(Piece { text: s, line_start: first, marker, bullet: first && bullet });
                first = false;
            }
        }
    }
    out
}

fn has_alnum(s: &str) -> bool {
    s.chars().any(char::is_alphanumeric)
}

/// The word without the punctuation around it: (leading, core, trailing).
fn split_core(text: &str) -> (&str, &str, &str) {
    let start = text.find(|c: char| c.is_alphanumeric() || matches!(c, '$' | '€' | '£' | '~' | '/' | '#' | '@' | '+')).unwrap_or(0);
    let end = text.rfind(|c: char| c.is_alphanumeric() || matches!(c, '%' | '/')).map_or(text.len(), |i| i + text[i..].chars().next().map_or(1, char::len_utf8));
    let end = end.max(start);
    (&text[..start], &text[start..end], &text[end..])
}

fn is_dotted_acronym(core: &str) -> bool {
    // "U.S", "e.g", "p.m": single letters separated by dots.
    let parts: Vec<&str> = core.split('.').collect();
    parts.len() >= 2 && parts.iter().all(|p| p.chars().count() == 1 && p.chars().all(char::is_alphabetic))
}

fn is_literal(core: &str) -> bool {
    if core.contains("://") || core.starts_with("www.") || core.starts_with('/') || core.starts_with("~/") || core.starts_with("./") {
        return true;
    }
    if let Some(at) = core.find('@') {
        if at > 0 && core[at..].contains('.') {
            return true;
        }
    }
    if core.contains("::") || core.contains("()") || core.contains('\\') {
        return true;
    }
    let chars: Vec<char> = if core.contains(['_', '/']) { core.chars().collect() } else { Vec::new() };
    for i in 1..chars.len().saturating_sub(1) {
        let (a, b) = (chars[i - 1], chars[i + 1]);
        if chars[i] == '_' && a.is_alphanumeric() && b.is_alphanumeric() {
            return true;
        }
        if chars[i] == '/' && a.is_alphabetic() && b.is_alphabetic() && chars.len() > 8 {
            return true; // a relative path ("crates/rsvp"); short "and/or" stays a word
        }
    }
    // A file or domain name: "fast_read.rs", "README.md", "example.com" (not "e.g", "3.14", "U.S").
    if !is_dotted_acronym(core) && !core.chars().any(|c| c.is_ascii_digit()) {
        if let Some((head, ext)) = core.rsplit_once('.') {
            return head.chars().count() >= 2 && (1..=5).contains(&ext.chars().count()) && ext.chars().all(char::is_alphanumeric) && head.chars().last().is_some_and(char::is_alphanumeric);
        }
    }
    false
}

fn classify(core: &str, sentence_initial: bool) -> (Kind, Option<u32>, Role) {
    let lower = core.to_lowercase();
    if !has_alnum(core) {
        let role = if core == "&" { Role::Function } else { Role::Content };
        return (Kind::Symbol, None, role);
    }
    if is_literal(core) {
        return (Kind::Literal, None, Role::Content);
    }
    if core.chars().any(|c| c.is_ascii_digit()) {
        return (Kind::Number, None, Role::Leader);
    }
    let alpha: Vec<char> = core.chars().filter(|c| c.is_alphabetic()).collect();
    let role = lexicon::role(&lower);
    let rank = lexicon::rank(&lower);
    let base = core.trim_end_matches('s');
    let upper = |s: &str| s.chars().filter(|c| c.is_alphabetic()).count() >= 2 && s.chars().filter(|c| c.is_alphabetic()).all(char::is_uppercase);
    if (upper(core) || upper(base) && core.ends_with('s')) && lower != "i" || is_dotted_acronym(core) && alpha.iter().all(|c| c.is_uppercase()) {
        return (Kind::Acronym, rank, Role::Content);
    }
    let capitalised = alpha.first().is_some_and(|c| c.is_uppercase());
    let pronoun_i = lower == "i" || lower.starts_with("i'") || lower.starts_with("i’");
    if capitalised && !pronoun_i && (!sentence_initial || rank.is_none()) {
        return (Kind::Name, rank, Role::Content);
    }
    (Kind::Word, rank, role)
}

fn trailing_pause(trailing: &str) -> (bool, Pause) {
    let terminal = trailing.contains(TERMINAL) || trailing.contains("...");
    let pause = if terminal {
        Pause::Sentence
    } else if trailing.contains(CLAUSE) || trailing.contains([')', ']', '}']) || DASHES.iter().any(|d| trailing.ends_with(d)) {
        Pause::Clause
    } else {
        Pause::None
    };
    (terminal, pause)
}

/// Whether `next` opens a new sentence: it starts with a capital and is a listed word, so a stop before it is a
/// sentence end rather than an abbreviation followed by a name.
fn opens_sentence(next: &str) -> bool {
    let (_, core, _) = split_core(next);
    let Some(first) = core.chars().next() else { return false };
    if !first.is_uppercase() {
        return false;
    }
    let lower = core.to_lowercase();
    lexicon::role(&lower) != Role::Content || lexicon::rank(&lower).is_some()
}

/// Tokens of `text`, with sentence and paragraph indices and the pause after each.
pub fn tokenize(text: &str) -> Vec<Token> {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut paragraphs: Vec<Vec<&str>> = vec![Vec::new()];
    for line in normalized.split('\n') {
        if line.trim().is_empty() {
            if !paragraphs.last().unwrap().is_empty() {
                paragraphs.push(Vec::new());
            }
        } else {
            paragraphs.last_mut().unwrap().push(line);
        }
    }

    let mut out: Vec<Token> = Vec::new();
    let mut sentence = 0usize;
    let mut paragraph = 0usize;
    for lines in paragraphs.iter().filter(|p| !p.is_empty()) {
        let pieces = pieces(lines);
        let start = out.len();
        let mut prefix = String::new();
        for (i, piece) in pieces.iter().enumerate() {
            let text = &piece.text;
            if !has_alnum(text) && !matches!(text.as_str(), "&" | "+" | "=" | "→" | "/" | "%" | "$" | "€" | "@" | "#" | "×") {
                if text.chars().all(|c| OPENERS.contains(&c)) && i + 1 < pieces.len() {
                    prefix.push_str(text);
                    continue;
                }
                // A lone dash, ellipsis or stray punctuation: a pause after the previous word.
                if let Some(last) = out[start..].last_mut() {
                    let dash = DASHES.contains(&text.as_str());
                    if dash || text.contains('…') || text.contains("...") {
                        last.text.push(' ');
                    }
                    last.text.push_str(text);
                    let (_, mut p) = trailing_pause(text);
                    if dash {
                        p = p.max(Pause::Clause);
                    }
                    last.pause = last.pause.max(p);
                }
                continue;
            }
            // A line break inside a paragraph ends the previous line's sentence when the new line looks like a new
            // item: a bullet or number, or a capital letter. Hard-wrapped prose (lowercase continuation) flows on.
            if piece.line_start && out.len() > start {
                let first = text.trim_start_matches(OPENERS).chars().next();
                let item = piece.marker || first.is_some_and(|c| c.is_uppercase() || c.is_ascii_digit()) || piece.bullet;
                if item {
                    let last = out.last_mut().unwrap();
                    last.pause = last.pause.max(Pause::Sentence);
                }
            }
            if let Some(last) = out[start..].last() {
                if last.pause >= Pause::Sentence {
                    sentence += 1;
                }
            }
            let full = format!("{}{}", std::mem::take(&mut prefix), text);
            let (_, core, trailing) = split_core(&full);
            let sentence_initial = out[start..].last().is_none_or(|t| t.pause >= Pause::Sentence) || piece.line_start;
            let (kind, rank, role) = classify(core, sentence_initial);
            let (terminal, mut pause) = trailing_pause(trailing);
            // An opening parenthesis or quote before this word: a clause pause before it.
            if full.starts_with(['(', '[']) {
                if let Some(last) = out[start..].last_mut() {
                    last.pause = last.pause.max(Pause::Clause);
                }
            }
            if terminal {
                let next = pieces.get(i + 1).map(|p| p.text.as_str());
                let lower: String = core.to_lowercase().chars().filter(|c| *c != '.').collect();
                let alpha = core.chars().filter(|c| c.is_alphabetic()).count();
                let ellipsis = trailing.contains('…') || trailing.contains("...");
                let question = trailing.contains(['!', '?']);
                if piece.marker {
                    pause = Pause::Clause;
                } else if let Some(next) = next {
                    let next_lower_start = split_core(next).1.chars().next().is_some_and(char::is_lowercase);
                    let abbreviation = !question && !ellipsis && (ABBREVIATIONS.contains(&lower.as_str()) || alpha == 1 && core.chars().all(|c| c.is_alphabetic() && c.is_uppercase()) || lower == "no" && split_core(next).1.starts_with(|c: char| c.is_ascii_digit()));
                    let list_end = !question && !ellipsis && (LIST_ENDS.contains(&lower.as_str()) || is_dotted_acronym(core));
                    if abbreviation {
                        pause = Pause::None;
                    } else if list_end {
                        pause = if opens_sentence(next) { Pause::Sentence } else { Pause::None };
                    } else if next_lower_start && !question {
                        // "approx. five", "and then... it worked": a pause, not a sentence end.
                        pause = Pause::Clause;
                    }
                }
            }
            let letters = core.chars().filter(|c| c.is_alphanumeric()).count();
            let lower = core.to_lowercase();
            let syllables = match kind {
                Kind::Word | Kind::Name => lexicon::syllables(&lower),
                Kind::Acronym => letters,
                Kind::Number => core.chars().filter(|c| c.is_ascii_digit()).count().min(10) + core.chars().filter(|c| c.is_alphabetic()).count() / 2,
                Kind::Literal => (letters / 3).max(1),
                Kind::Symbol => 1,
            }
            .max(1);
            out.push(Token { text: full, kind, role, letters: letters.max(1), syllables, rank, pause, sentence, paragraph });
        }
        if out.len() > start {
            out.last_mut().unwrap().pause = Pause::Paragraph;
            sentence += 1;
            paragraph += 1;
        }
    }
    // A sentence-initial first name: a capitalised, less common word right before a name ("Ada Lovelace").
    for i in 0..out.len().saturating_sub(1) {
        let t = &out[i];
        let capitalised = t.text.chars().find(|c| c.is_alphabetic()).is_some_and(char::is_uppercase);
        if t.kind == Kind::Word && capitalised && t.role == Role::Content && t.rank.is_none_or(|r| r > 2000) && t.pause == Pause::None && out[i + 1].kind == Kind::Name {
            out[i].kind = Kind::Name;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(s: &str) -> Vec<String> {
        tokenize(s).into_iter().map(|t| t.text).collect()
    }

    fn kinds(s: &str) -> Vec<Kind> {
        tokenize(s).into_iter().map(|t| t.kind).collect()
    }

    #[test]
    fn keeps_numbers_urls_hyphens_and_contractions_whole() {
        let t = texts("It's 3.14, $5, 40% and 1,000.50 at 10:30 on 2026-10-18 — see https://example.com/a?b=c. State-of-the-art don't User's.");
        assert_eq!(t, vec!["It's", "3.14,", "$5,", "40%", "and", "1,000.50", "at", "10:30", "on", "2026-10-18 —", "see", "https://example.com/a?b=c.", "State-of-the-art", "don't", "User's."]);
        let k = kinds("It's 3.14 https://example.com fast_read.rs ~/Work GPU Heimdall said hello");
        assert_eq!(k, vec![Kind::Word, Kind::Number, Kind::Literal, Kind::Literal, Kind::Literal, Kind::Acronym, Kind::Name, Kind::Word, Kind::Word]);
    }

    #[test]
    fn sentences_and_pauses() {
        let t = tokenize("The build failed. Tests timed out, again!\n\nNext paragraph");
        let p: Vec<Pause> = t.iter().map(|t| t.pause).collect();
        assert_eq!(p, vec![Pause::None, Pause::None, Pause::Sentence, Pause::None, Pause::None, Pause::Clause, Pause::Paragraph, Pause::None, Pause::Paragraph]);
        let s: Vec<usize> = t.iter().map(|t| t.sentence).collect();
        assert_eq!(s, vec![0, 0, 0, 1, 1, 1, 1, 2, 2]);
        assert_eq!(t[8].paragraph, 1);
    }

    #[test]
    fn abbreviations_and_initials_do_not_end_sentences() {
        let t = tokenize("Dr. Smith met J. R. Tolkien, e.g. at 3 p.m. today. Then apples, pears etc. Done. Jabłka itd. Dobrze.");
        let ends: Vec<&str> = t.iter().filter(|t| t.pause >= Pause::Sentence).map(|t| t.text.as_str()).collect();
        assert_eq!(ends, vec!["today.", "etc.", "Done.", "itd.", "Dobrze."]);
        assert_eq!(t[1].kind, Kind::Name);
    }

    #[test]
    fn dashes_ellipses_and_brackets() {
        assert_eq!(texts("slow—really slow"), vec!["slow—", "really", "slow"]);
        assert_eq!(texts("wait...what"), vec!["wait...", "what"]);
        assert_eq!(texts("pages 10–12"), vec!["pages", "10–12"]);
        let t = tokenize("one - two ( three ) four");
        assert_eq!(t[0].text, "one -");
        assert_eq!(t[0].pause, Pause::Clause);
        assert_eq!(t[1].pause, Pause::Clause, "pause before an opening parenthesis");
        assert_eq!(t[2].text, "(three)");
        assert_eq!(t[2].pause, Pause::Clause);
    }

    #[test]
    fn markdown_and_lists() {
        let t = tokenize("# Status\n- **Build** is `green`\n- Tests pass\n1. First item");
        let words: Vec<&str> = t.iter().map(|t| t.text.as_str()).collect();
        assert_eq!(words, vec!["Status", "Build", "is", "green", "Tests", "pass", "1.", "First", "item"]);
        let ends: Vec<&str> = t.iter().filter(|t| t.pause >= Pause::Sentence).map(|t| t.text.as_str()).collect();
        assert_eq!(ends, vec!["Status", "green", "pass", "item"]);
        assert_eq!(t[6].pause, Pause::Clause);
    }

    #[test]
    fn hard_wrapped_prose_flows_on() {
        let t = tokenize("the tests need\nlonger than two minutes.");
        assert_eq!(t[2].pause, Pause::None);
    }

    #[test]
    fn names_versus_sentence_initial_words() {
        let t = tokenize("Second, Heimdall asked User. Polska is here.");
        assert_eq!(t.iter().map(|t| t.kind).collect::<Vec<_>>(), vec![Kind::Word, Kind::Name, Kind::Word, Kind::Name, Kind::Name, Kind::Word, Kind::Word]);
    }

    #[test]
    fn empty_and_punctuation_only() {
        assert!(tokenize("").is_empty());
        assert!(tokenize("  \n\n — ... ").is_empty());
    }
}
