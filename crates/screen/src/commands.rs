use crate::{HostTimeNs, ScreenBuffer};
use anyhow::{bail, ensure, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    fs::{self, OpenOptions},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

/// Copy listen::Word fields without coupling the screen crate to ASR runtimes.
#[derive(Clone, Debug)]
pub struct TimedWord {
    pub text: String,
    pub start_ns: u64,
    pub end_ns: u64,
}
/// Completion triggers the action; frame selection uses the first word.
pub const SCREEN_PHRASES: &[&str] = &[
    "look at this",
    "take a look",
    "take a screenshot",
    "capture the screen",
    "screenshot this",
    "show you this",
    "look at my screen",
    "look at the screen",
    "see my screen",
    "zobacz to",
    "spójrz",
    "spójrz na to",
    "zrób zrzut ekranu",
    "zrób screenshot",
    "zrób screenshota",
    "zrób zdjęcie ekranu",
    "popatrz na to",
    "zerknij na to",
    // Pointing (18 Oct): the request arrives with its screenshot, so the agent's whole job is one `point` call.
    // These only attach a screenshot; nothing spoken opens drawing mode.
    "draw me that",
    "show me that on the page",
    "show me where",
];
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PhraseMatch {
    pub source: String,
    pub phrase: String,
    /// Frame timestamp: attention at the beginning of the phrase.
    pub start_ns: HostTimeNs,
    /// Completion timestamp: the final phrase word has been heard.
    pub trigger_ns: HostTimeNs,
}
pub struct PhraseMatcher {
    phrases: &'static [&'static str],
    fired: HashMap<String, HashSet<(String, usize)>>,
}
impl Default for PhraseMatcher {
    /// The screenshot table, `SCREEN_PHRASES`.
    fn default() -> Self {
        Self::new(SCREEN_PHRASES)
    }
}
fn normalize(s: &str) -> String {
    s.trim_matches(|c: char| !c.is_alphanumeric())
        .chars()
        .flat_map(char::to_lowercase)
        // Apostrophes in contractions are not word boundaries. Other internal
        // punctuation remains, so joined/embedded words cannot match an alias.
        .filter(|c| !matches!(c, '\'' | '’'))
        .map(|c| match c {
            'ą' => 'a',
            'ć' => 'c',
            'ę' => 'e',
            'ł' => 'l',
            'ń' => 'n',
            'ó' => 'o',
            'ś' => 's',
            'ź' | 'ż' => 'z',
            _ => c,
        })
        .collect()
}
fn edit_one(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    if a.len().min(b.len()) < 5 {
        return false;
    }
    let a: Vec<_> = a.chars().collect();
    let b: Vec<_> = b.chars().collect();
    if a.len().abs_diff(b.len()) > 1 {
        return false;
    }
    let (mut i, mut j, mut edits) = (0, 0, 0);
    while i < a.len() && j < b.len() {
        if a[i] == b[j] {
            i += 1;
            j += 1;
            continue;
        }
        edits += 1;
        if edits > 1 {
            return false;
        }
        if a.len() >= b.len() {
            i += 1;
        }
        if b.len() >= a.len() {
            j += 1;
        }
    }
    edits + (a.len() - i) + (b.len() - j) <= 1
}
/// Polish imperative verbs must be exact after accent folding. A one-letter
/// edit can change grammatical mood (spójrz -> spojrzą, zobacz -> zobaczy).
fn token_matches(actual: &str, expected: &str) -> bool {
    actual == expected
        || (!matches!(
            expected,
            "spojrz" | "zobacz" | "zrob" | "popatrz" | "zerknij" | "zglos"
        ) && edit_one(actual, expected))
}
fn clause_break(word: &TimedWord) -> bool {
    word.text.ends_with(['.', '!', '?', ';', ':', ','])
}
fn negated(words: &[TimedWord], normalized: &[String], start: usize) -> bool {
    // Scope negation to the preceding clause, including ASR's punctuation-free
    // "don't please take ..." / "nie proszę zrób ...". Explicit contrast resets
    // the scope. Reported speech is intentionally accepted: context is cheap.
    for i in (0..start).rev() {
        if words[i].text.ends_with(['.', '!', '?', ';', ':'])
            || matches!(normalized[i].as_str(), "but" | "instead" | "ale" | "jednak")
            || words[i + 1].start_ns.saturating_sub(words[i].end_ns) > 1_000_000_000
        {
            break;
        }
        if matches!(
            normalized[i].as_str(),
            "dont" | "not" | "never" | "nie"
                // Night review D4 (20 Oct 18:39:46 "So yeah, I wouldn't even look at this.").
                | "wouldnt" | "shouldnt" | "couldnt" | "cant" | "cannot" | "wont" | "didnt" | "doesnt"
        ) {
            return true;
        }
    }
    false
}
/// The configured assistant's name words, lower case (Personalise, 26 Oct; the host sets them): lead words like
/// Heimdall's spellings below ("Ivy, take a screenshot").
static NAME_WORDS: std::sync::RwLock<Vec<String>> = std::sync::RwLock::new(Vec::new());
pub fn set_name_words(words: Vec<String>) {
    *NAME_WORDS.write().unwrap() = words;
}
/// Words that may stand between a clause's start and an English command phrase: interjections, the name, politeness
/// ("Yo, Heimdall, look at this", "Okay look at this", "please just take a screenshot").
fn lead_word(w: &str) -> bool {
    NAME_WORDS.read().unwrap().iter().any(|n| n == w) || matches!(
        w,
        "yo" | "hey" | "hi" | "hello" | "oh" | "ok" | "okay" | "yeah" | "yes" | "yep" | "now" | "then" | "just"
            | "please" | "um" | "uh" | "like" | "look" | "well" | "alright" | "right" | "also" | "here" | "ah"
            | "heimdall" | "heimdal" | "heimda" | "haimdal" | "hemdal" | "himdal" | "hamdal"
    )
}
const FRAMES: &[&[&str]] = &[
    &["can", "you"],
    &["could", "you"],
    &["would", "you"],
    &["will", "you"],
    &["want", "you", "to"],
    &["need", "you", "to"],
    &["like", "you", "to"],
    &["lets"],
    &["go", "ahead", "and"],
];
/// A request frame ending just before position `end` ("can you", "I want you to", "let's").
fn request_frame(normalized: &[String], end: usize) -> bool {
    FRAMES.iter().any(|f| end >= f.len() && normalized[end - f.len()..end].iter().zip(f.iter()).all(|(a, b)| a == b))
}
/// Verbs that open an imperative clause the user joins to a quick command with "and"/"then" ("open Safari and take a
/// screenshot", "scroll down and look at this"). A positive list: a statement's first word ("we", "everything",
/// "when") never joins.
fn imperative_verb(w: &str) -> bool {
    matches!(
        w,
        "open" | "close" | "go" | "come" | "scroll" | "click" | "tap" | "press" | "switch" | "pull" | "bring" | "find"
            | "check" | "move" | "zoom" | "select" | "navigate" | "show" | "run" | "launch" | "start" | "play" | "type"
            | "read" | "search" | "load" | "refresh" | "reload" | "focus" | "put" | "grab"
            | "try" | "pick" | "turn" | "hover" | "expand" | "collapse" | "copy" | "paste" | "drag" | "write" | "draw"
            | "list" | "give" | "wait" | "fix" | "change" | "edit" | "add" | "remove" | "rename" | "save"
            | "send" | "tell" | "record" | "stop" | "pause" | "resume" | "restart" | "build" | "install" | "update"
            | "compare" | "highlight" | "circle" | "point" | "mark" | "fill" | "enter" | "upload" | "download"
            | "attach" | "share" | "jump" | "head" | "minimize" | "maximize" | "resize" | "set" | "hide"
            | "look" | "see" | "watch" | "listen" | "pay" | "hold" | "drop" | "split" | "sort" | "filter" | "test"
    )
}
/// Whether the clause joined by "and"/"then" at `join` is itself a command: its first word after lead words (and an
/// optional request frame, "can you"/"I want you to") is an imperative verb, within ten words of the clause start.
/// Only lead words before the join ("Okay and look at this") count too.
fn commanded_conjunct(words: &[TimedWord], normalized: &[String], join: usize) -> bool {
    let mut s = join;
    while s > 0
        && !clause_break(&words[s - 1])
        && words[s].start_ns.saturating_sub(words[s - 1].end_ns) <= 1_000_000_000
        && !matches!(normalized[s - 1].as_str(), "but" | "so")
    {
        s -= 1;
        if join - s > 10 {
            return false;
        }
    }
    let skip_leads = |mut k: usize| {
        while k < join && (lead_word(&normalized[k]) || matches!(normalized[k].as_str(), "and" | "then")) {
            k += 1;
        }
        k
    };
    let mut k = skip_leads(s);
    if k >= join {
        return true;
    }
    let subject = usize::from(matches!(normalized[k].as_str(), "i" | "id"));
    if let Some(f) = FRAMES.iter().find(|f| {
        let at = k + subject;
        at + f.len() <= join && normalized[at..at + f.len()].iter().zip(f.iter()).all(|(a, b)| a == b)
    }) {
        k = skip_leads(k + subject + f.len());
    }
    k < join && imperative_verb(&normalized[k])
}
/// Night review D4: an English quick command is an imperative. Its phrase opens its clause — the utterance start, a
/// sentence or comma break, a pause over a second, or "but"/"so" — after at most a few lead words, or follows a
/// request frame. Inside a statement it is talk about the thing, not a command: "So yeah, I wouldn't even look at
/// this" (18:39:46), "…when I take a screenshot for him to decide…" (19:14:43), "We should take a look at the …
/// website" (16 Oct), "I said look at this and it attached a screenshot" (14 Oct). A command joined to another
/// command by "and"/"then" stands where that command stands ("open Safari and take a screenshot"; review 21 Oct).
/// Polish phrases keep the old rule (no false triggers observed).
fn commanded(words: &[TimedWord], normalized: &[String], start: usize) -> bool {
    let mut i = start;
    for _ in 0..=4 {
        if i == 0 {
            return true;
        }
        let prev = &words[i - 1];
        if clause_break(prev)
            || words[i].start_ns.saturating_sub(prev.end_ns) > 1_000_000_000
            || matches!(normalized[i - 1].as_str(), "but" | "so")
            || request_frame(normalized, i)
        {
            return true;
        }
        // "take a look at my screen": an alias inside a longer command stands where that command stands.
        if i >= 2 && normalized[i - 2] == "take" && normalized[i - 1] == "a" {
            i -= 2;
            continue;
        }
        // Two commands joined: "open Safari and take a screenshot", "scroll down and then look at this".
        if matches!(normalized[i - 1].as_str(), "and" | "then") && commanded_conjunct(words, normalized, i - 1) {
            return true;
        }
        if !lead_word(&normalized[i - 1]) {
            return false;
        }
        i -= 1;
    }
    false
}
/// Verbs that introduce reported speech, accent-folded: "I said, look at this", "she asked him: take a look",
/// "Powiedziałem mu, zobacz to". Past and third-person forms only: "mówię ci, zobacz to" ("I'm telling you, look at
/// this") is emphasis to the listener, not a report.
fn speech_verb(w: &str) -> bool {
    matches!(
        w,
        "said" | "says" | "told" | "tells" | "asked" | "asks" | "wrote" | "writes" | "replied" | "answered" | "yelled"
            | "shouted" | "whispered" | "texted" | "messaged"
            | "powiedzialem" | "powiedzialam" | "powiedziales" | "powiedzialas" | "powiedzial" | "powiedziala"
            | "powiedzieli" | "powiedzialy" | "powiedzielismy" | "mowilem" | "mowilam" | "mowil" | "mowila" | "mowili"
            | "mowi" | "mowia" | "napisalem" | "napisalam" | "napisal" | "napisala" | "pisal" | "pisala"
            | "zapytalem" | "zapytalam" | "zapytal" | "zapytala" | "spytalem" | "spytalam" | "spytal" | "spytala"
            | "pytal" | "pytala" | "krzyknalem" | "krzyknelam" | "krzyknal" | "krzyknela" | "odpowiedzialem"
            | "odpowiedzialam" | "odpowiedzial" | "odpowiedziala"
    )
}
/// The person told, between a speech verb and its quote ("told him, look at this"). Not "you"/"ci": "I told you, look
/// at this" speaks to the listener.
fn told_object(w: &str) -> bool {
    matches!(
        w,
        "me" | "him" | "her" | "them" | "us" | "mu" | "jej" | "jemu" | "im" | "mi" | "go" | "ich" | "nam" | "niego"
            | "niej" | "nich"
    )
}
/// Polish fillers that may stand between a quote's verb and the quote ("powiedziałem mu wtedy, zobacz to").
fn polish_filler(w: &str) -> bool {
    matches!(w, "no" | "wtedy" | "wiesz" | "tak" | "potem")
}
/// Whether the phrase at `start` is quoted speech: right after a speech verb ("I said, look at this" — Parakeet's comma
/// would otherwise open a command clause), optionally with the person told or a few lead words between ("I said,
/// Heimdall, look at this"), or after "was like". "Like I said, look at this" and "jak mówiłem, zobacz to" are
/// discourse markers before a real command and still fire. 22 Oct screen-phrase benchmark: "I said look at this."
/// fired 9 of 9 times.
fn reported(words: &[TimedWord], normalized: &[String], start: usize) -> bool {
    let mut k = start;
    for _ in 0..4 {
        if k == 0
            || words[k - 1].text.ends_with(['.', '!', '?', ';'])
            || words[k].start_ns.saturating_sub(words[k - 1].end_ns) > 1_000_000_000
        {
            return false;
        }
        // "told him, take a look at my screen": an alias inside a longer phrase stands where that phrase stands.
        if k >= 2 && normalized[k - 2] == "take" && normalized[k - 1] == "a" {
            k -= 2;
            continue;
        }
        let w = normalized[k - 1].as_str();
        let open = |i: usize| !words[i].text.ends_with(['.', '!', '?', ';', ',', ':']);
        let verb = if speech_verb(w) {
            Some(k - 1)
        } else if k >= 2 && told_object(w) && speech_verb(&normalized[k - 2]) && open(k - 2) {
            Some(k - 2)
        } else if k >= 3
            && told_object(w)
            && matches!(normalized[k - 2].as_str(), "to" | "do")
            && speech_verb(&normalized[k - 3])
            && open(k - 3)
            && open(k - 2)
        {
            // "said to him, …", "powiedziałem do niego, …"
            Some(k - 3)
        } else if k >= 2
            && w == "like"
            && open(k - 2)
            && matches!(normalized[k - 2].as_str(), "was" | "were" | "hes" | "shes" | "im" | "theyre")
        {
            Some(k - 1)
        } else {
            None
        };
        if let Some(v) = verb {
            // "Like I said", "as I've already said", "jak mówiłem": a marker within three words before the verb, in
            // the same clause.
            let marker = (1..=3.min(v)).any(|d| {
                matches!(normalized[v - d].as_str(), "as" | "like" | "jak") && (1..d).all(|e| open(v - e)) && open(v - d)
            });
            return !marker;
        }
        if !lead_word(w) && !polish_filler(w) {
            return false;
        }
        k -= 1;
    }
    false
}
/// The command-position rule covers the English screenshot phrases (the false triggers observed); cancel and stop
/// phrases and Polish keep the older rule.
fn positioned(phrase: &str) -> bool {
    SCREEN_PHRASES.contains(&phrase) && !polish(phrase)
}
fn polish(phrase: &str) -> bool {
    phrase
        .split_whitespace()
        .next()
        .is_some_and(|w| matches!(normalize(w).as_str(), "zobacz" | "spojrz" | "zrob" | "popatrz" | "zerknij"))
}
/// Whether a phrase whose words match at `start` may fire there: not negated, and (English) in command position.
fn fires_at(phrase: &str, words: &[TimedWord], normalized: &[String], start: usize) -> bool {
    !negated(words, normalized, start)
        && (!positioned(phrase) || commanded(words, normalized, start))
        && !(SCREEN_PHRASES.contains(&phrase) && reported(words, normalized, start))
}
impl PhraseMatcher {
    /// A matcher over another fixed table (for example the app's cancel/stop phrases).
    pub fn new(phrases: &'static [&'static str]) -> Self {
        Self {
            phrases,
            fired: HashMap::new(),
        }
    }
    /// Compatibility path for a cumulative transcript. Word positions remain
    /// scoped to `source` until reset_source; timestamps never define identity.
    /// Use observe_utterance for distinct utterances and to flush short aliases.
    pub fn observe(&mut self, source: &str, words: &[TimedWord]) -> Vec<PhraseMatch> {
        self.observe_utterance(source, "cumulative", words, false)
    }
    /// IDs must remain stable across partials, finals and re-decodes, and unique
    /// for new utterances. Words are cumulative within that utterance. Retain
    /// occurrence identities until capture discontinuity, without time expiry.
    pub fn observe_utterance(
        &mut self,
        source: &str,
        utterance_id: &str,
        words: &[TimedWord],
        is_final: bool,
    ) -> Vec<PhraseMatch> {
        let fired = self.fired.entry(source.into()).or_default();
        let normalized: Vec<_> = words.iter().map(|w| normalize(&w.text)).collect();
        let mut candidates = vec![];
        let mut pending = vec![];
        for start in 0..words.len() {
            if negated(words, &normalized, start) {
                continue;
            }
            let mut complete = vec![];
            let quoted = reported(words, &normalized, start);
            for phrase in self.phrases {
                if (positioned(phrase) && !commanded(words, &normalized, start))
                    || (quoted && SCREEN_PHRASES.contains(phrase))
                {
                    continue;
                }
                let tokens: Vec<_> = phrase.split_whitespace().map(normalize).collect();
                let mut offset = 0;
                let mut token = 0;
                let mut fillers = 0;
                let mut errors = 0;
                let mut valid = true;
                while token < tokens.len() && start + offset < words.len() {
                    let w = &words[start + offset];
                    if w.end_ns < w.start_ns
                        || (offset > 0
                            && ((clause_break(&words[start + offset - 1])
                                && !(*phrase == "zobacz to"
                                    && words[start + offset - 1].text.ends_with(',')))
                                || w.start_ns < words[start + offset - 1].start_ns
                                || w.start_ns.saturating_sub(words[start + offset - 1].end_ns)
                                    > 1_000_000_000))
                        // The span ends at the last word's start: Parakeet stretches a final word's end over
                        // trailing noise by seconds ("Heimdall, take a screenshot." measured 4.56 s, 22 Oct).
                        || w.start_ns.saturating_sub(words[start].start_ns) > 4_000_000_000
                    {
                        valid = false;
                        break;
                    }
                    let actual = &normalized[start + offset];
                    // Bounded, explicit hesitation vocabulary; never skip content
                    // or negation. Keep the alias canonical for "zobacz, yyy, to".
                    if *phrase == "zobacz to"
                        && token == 1
                        && fillers < 2
                        && matches!(
                            actual.as_str(),
                            "yyy" | "eee" | "um" | "uh" | "no" | "prosze"
                        )
                    {
                        fillers += 1;
                    } else if token_matches(actual, &tokens[token]) {
                        errors += usize::from(*actual != tokens[token]);
                        token += 1;
                    } else {
                        valid = false;
                        break;
                    }
                    offset += 1;
                }
                if !valid || errors > 1 {
                    continue;
                }
                if token < tokens.len() {
                    if !clause_break(words.last().unwrap()) {
                        pending.push((start, start + offset + tokens.len() - token - 1));
                    }
                } else {
                    complete.push((start + offset - 1, *phrase, errors));
                }
            }
            if let Some((end, phrase, _)) = complete
                .into_iter()
                .max_by_key(|(end, _, errors)| (*end, std::cmp::Reverse(*errors)))
            {
                candidates.push((start, end, phrase));
            }
        }
        // Choose the longest complete overlapping alias, even when its first
        // word differs ("take a look at my screen"). A longer partial prefix
        // defers the short alias until disambiguation or the utterance final.
        candidates.sort_by_key(|(start, end, _)| (std::cmp::Reverse(end - start), *start));
        let mut selected = Vec::new();
        for (start, end, phrase) in candidates {
            if (!is_final
                && pending
                    .iter()
                    .any(|(s, e)| *s <= end && *e >= start && e - s > end - start))
                || selected.iter().any(|(s, e, _)| *s <= end && *e >= start)
            {
                continue;
            }
            selected.push((start, end, phrase));
        }
        selected.sort_by_key(|(start, _, _)| *start);
        let mut result = vec![];
        for (start, end, phrase) in selected {
            // Word positions own aliases too, independently of mutable times.
            // Retain the whole span so an overlapping alias on a re-decode
            // cannot manufacture a second occurrence with a different start.
            if (start..=end).any(|position| fired.contains(&(utterance_id.into(), position))) {
                continue;
            }
            for position in start..=end {
                fired.insert((utterance_id.into(), position));
            }
            result.push(PhraseMatch {
                source: source.into(),
                phrase: phrase.into(),
                start_ns: words[start].start_ns,
                trigger_ns: words[end].start_ns,
            });
        }
        result
    }
    /// Reset only when the source's capture generation changes.
    pub fn reset_source(&mut self, source: &str) {
        self.fired.remove(source);
    }
}
/// A phrase that almost fired, for bug triage. Never triggers anything.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NearMiss {
    pub phrase: String,
    /// The words actually heard over the span.
    pub heard: String,
    pub start_ns: HostTimeNs,
    pub end_ns: HostTimeNs,
    /// `negated`: the whole phrase was heard inside a negation. `partial`: the
    /// opening words matched and the rest did not (at least two words, or one
    /// word of five or more letters). `loose`: every word is within two edits,
    /// or a Polish imperative is one edit off, so the strict rules rejected it.
    /// `embedded`: the whole English phrase was heard inside a statement, not
    /// in command position ("I wouldn't even look at this").
    pub reason: String,
}
fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut prev = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cur = row[j + 1];
            row[j + 1] = (prev + usize::from(ca != *cb)).min(row[j] + 1).min(cur + 1);
            prev = cur;
        }
    }
    row[b.len()]
}
/// Offline scan of a finished utterance for phrases from `phrases` that did not
/// fire. Matches the strict matcher's tokenization; one entry per span.
pub fn near_misses(phrases: &[&str], words: &[TimedWord]) -> Vec<NearMiss> {
    let normalized: Vec<_> = words.iter().map(|w| normalize(&w.text)).collect();
    let mut found: Vec<NearMiss> = vec![];
    // Spans where a phrase did fire are not near-misses of a longer alias.
    let mut covered: Vec<(usize, usize)> = vec![];
    for start in 0..words.len() {
        for phrase in phrases {
            if !fires_at(phrase, words, &normalized, start) {
                continue;
            }
            let tokens: Vec<_> = phrase.split_whitespace().map(normalize).collect();
            if start + tokens.len() <= words.len()
                && tokens
                    .iter()
                    .enumerate()
                    .all(|(i, t)| token_matches(&normalized[start + i], t))
            {
                covered.push((start, start + tokens.len() - 1));
            }
        }
    }
    for start in 0..words.len() {
        // (rank, span length, distance of the first unmatched word, phrase, reason)
        let mut best: Option<(u8, usize, usize, &str, &str)> = None;
        for phrase in phrases {
            let tokens: Vec<_> = phrase.split_whitespace().map(normalize).collect();
            let mut strict = 0;
            while strict < tokens.len()
                && start + strict < words.len()
                && token_matches(&normalized[start + strict], &tokens[strict])
            {
                strict += 1;
            }
            let loose = start + tokens.len() <= words.len()
                && tokens.iter().enumerate().all(|(i, t)| {
                    let a = &normalized[start + i];
                    token_matches(a, t) || (t.chars().count() >= 4 && edit_distance(a, t) <= 2)
                });
            let candidate = if strict == tokens.len() {
                if negated(words, &normalized, start) {
                    (3, strict, 0, "negated")
                } else if !fires_at(phrase, words, &normalized, start) {
                    // Inside a statement, not in command position (D4).
                    (3, strict, 0, "embedded")
                } else {
                    continue;
                }
            } else if loose {
                (2, tokens.len(), 0, "loose")
            } else if strict > 0
                && tokens.len() > 1
                && (strict >= 2 || tokens[0].chars().count() >= 5)
            {
                let next = normalized
                    .get(start + strict)
                    .map_or(usize::MAX, |w| edit_distance(w, &tokens[strict]));
                (1, strict, next, "partial")
            } else {
                continue;
            };
            let key = |c: &(u8, usize, usize, &str, &str)| (c.0, c.1, std::cmp::Reverse(c.2));
            let c = (candidate.0, candidate.1, candidate.2, *phrase, candidate.3);
            if best.as_ref().is_none_or(|b| key(&c) > key(b)) {
                best = Some(c);
            }
        }
        let Some((_, len, _, phrase, reason)) = best else {
            continue;
        };
        let end = start + len - 1;
        if covered.iter().any(|(s, e)| *s <= end && *e >= start) {
            continue;
        }
        covered.push((start, end));
        found.push(NearMiss {
            phrase: phrase.into(),
            heard: words[start..=end]
                .iter()
                .map(|w| w.text.as_str())
                .collect::<Vec<_>>()
                .join(" "),
            start_ns: words[start].start_ns,
            end_ns: words[end].end_ns,
            reason: reason.into(),
        });
    }
    found
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShotKind {
    #[default]
    Ring,
    FullResolution,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Shot {
    pub display_id: u32,
    pub frame_ns: u64,
    #[serde(default)]
    pub kind: ShotKind,
    #[serde(default)]
    pub width: u32,
    #[serde(default)]
    pub height: u32,
    /// Request-to-pixels latency for one-shot capture; absent for ring frames.
    #[serde(default)]
    pub grab_latency_ns: Option<u64>,
    pub path: PathBuf,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Action {
    pub source: String,
    pub trigger_ns: u64,
    pub note: String,
    /// First display for simple consumers; attach all shots for all-display context.
    pub image_path: PathBuf,
    pub shots: Vec<Shot>,
    #[serde(default)]
    pub full_resolution_errors: Vec<String>,
    /// Total supplemental capture + PNG time across displays.
    #[serde(default)]
    pub full_resolution_elapsed_ns: Option<u64>,
}
pub fn default_shots_dir() -> Result<PathBuf> {
    Ok(heimdall_home::data_dir().join("shots"))
}
pub(crate) fn wall_label(trigger: u64, host_now: u64) -> String {
    let unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let seconds = unix - (host_now.saturating_sub(trigger) / 1_000_000_000) as i64;
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        let time = seconds as libc::time_t;
        libc::localtime_r(&time, &mut tm);
        format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
    }
}
/// Only this command path persists pixels. Missing displays fail explicitly,
/// never claim a complete attachment when one stream is stale or not ready.
pub fn attach_screenshot(
    buffer: &mut ScreenBuffer,
    matched: &PhraseMatch,
    now_ns: u64,
    dir: &Path,
) -> Result<Action> {
    write_ring_action(buffer.select(matched.start_ns, now_ns), matched, now_ns, dir)
}
/// Write the phrase-time ring frames, live or held, as the command's attachment.
pub(crate) fn write_ring_action(
    selected: crate::FrameSet,
    matched: &PhraseMatch,
    now_ns: u64,
    dir: &Path,
) -> Result<Action> {
    ensure!(selected.errors.is_empty(), "screen frame retrieval failed: {}", selected.errors.join("; "));
    ensure!(
        !selected.frames.is_empty(),
        "no screen frame at phrase start timestamp"
    );
    ensure!(
        selected.missing_displays.is_empty(),
        "screen frames missing for displays {:?}",
        selected.missing_displays
    );
    fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    let mut shots = vec![];
    for f in &selected.frames {
        match write_shot(f, matched.start_ns, ShotKind::Ring, None, dir) {
            Ok(shot) => shots.push(shot),
            Err(e) => {
                for shot in &shots {
                    let _ = fs::remove_file(&shot.path);
                }
                bail!("screenshot write failed: {e}");
            }
        }
    }
    Ok(Action {
        source: matched.source.clone(),
        trigger_ns: matched.trigger_ns,
        note: format!(
            "[screenshot {} attached]",
            wall_label(matched.start_ns, now_ns)
        ),
        image_path: shots[0].path.clone(),
        shots,
        full_resolution_errors: vec![],
        full_resolution_elapsed_ns: None,
    })
}

/// Same private PNG writer for history and native one-shot frames.
pub(crate) fn write_shot(
    f: &crate::Frame,
    command_ns: u64,
    kind: ShotKind,
    grab_latency_ns: Option<u64>,
    dir: &Path,
) -> Result<Shot> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nonce = NEXT.fetch_add(1, Ordering::Relaxed);
    let path = dir.join(format!(
        "{}-{}-{}-{:?}-display-{}.png",
        command_ns,
        std::process::id(),
        nonce,
        kind,
        f.display.id
    ));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(&path)?;
    let result = (|| -> Result<()> {
        let mut encoder = png::Encoder::new(file, f.width, f.height);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header()?;
        writer.write_image_data(&f.rgb)?;
        writer.finish()?;
        // Agent transport bounds apply to each image, including full-resolution PNGs.
        ensure!(
            fs::metadata(&path)?.len() <= 20 * 1024 * 1024,
            "PNG exceeds agent's 20 MiB image limit"
        );
        Ok(())
    })();
    if let Err(e) = result {
        let _ = fs::remove_file(&path);
        return Err(e);
    }
    Ok(Shot {
        display_id: f.display.id,
        frame_ns: f.host_time_ns,
        kind,
        width: f.width,
        height: f.height,
        grab_latency_ns,
        path,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn words(s: &str, start: u64) -> Vec<TimedWord> {
        s.split_whitespace()
            .enumerate()
            .map(|(i, s)| TimedWord {
                text: s.into(),
                start_ns: start + i as u64 * 200_000_000,
                end_ns: start + i as u64 * 200_000_000 + 150_000_000,
            })
            .collect()
    }
    #[test]
    fn bilingual_fuzzy_and_partial_dedupe() {
        let mut m = PhraseMatcher::default();
        assert!(m.observe("mic", &words("look at", 0)).is_empty());
        let w = words("Look at this!", 0);
        let copy = w[2].text.clone();
        assert_eq!(m.observe("mic", &w)[0].trigger_ns, 400_000_000);
        assert_eq!(w[2].text, copy);
        assert!(m
            .observe("mic", &words("look at this please", 50_000_000))
            .is_empty());
        assert_eq!(
            m.observe_utterance(
                "mic",
                "2",
                &words("ZROB zrzut ekranu", 5_000_000_000),
                false
            )
            .len(),
            1
        );
        assert_eq!(
            m.observe_utterance(
                "mic",
                "3",
                &words("take a screenshott", 10_000_000_000),
                false
            )
            .len(),
            1
        );
        assert_eq!(
            m.observe_utterance("mic", "4", &words("spojrz na to", 15_000_000_000), false)
                .len(),
            1
        );
    }
    #[test]
    fn no_short_word_fuzz_and_long_pauses_and_source_isolation() {
        let mut m = PhraseMatcher::default();
        assert!(m.observe("a", &words("look at that", 0)).is_empty());
        let mut w = words("look at this", 0);
        w[2].start_ns += 2_000_000_000;
        w[2].end_ns += 2_000_000_000;
        assert!(m.observe("a", &w).is_empty());
        for source in ["a", "b"] {
            assert_eq!(
                m.observe(source, &words("look at this", 4_000_000_000))
                    .len(),
                1
            );
        }
    }
    #[test]
    fn bilingual_command_table() {
        // Expected canonical alias, first-word index and completion-word index.
        type Expected = Option<(&'static str, usize, usize)>;
        let cases: &[(&str, Expected)] = &[
            ("look at this", Some(("look at this", 0, 2))),
            ("LOOK AT THIS!", Some(("look at this", 0, 2))),
            ("please look at this invoice", Some(("look at this", 1, 3))),
            // Reported speech is a statement, not a command (night review D4, 20 Oct).
            ("he told me to look at this invoice", None),
            ("she said take a screenshot", None),
            ("take a look", Some(("take a look", 0, 2))),
            ("take a screenshot", Some(("take a screenshot", 0, 2))),
            ("take a screenshott", Some(("take a screenshot", 0, 2))),
            ("take a screenhot", Some(("take a screenshot", 0, 2))),
            ("capture the screen", Some(("capture the screen", 0, 2))),
            ("screenshot this", Some(("screenshot this", 0, 1))),
            ("show you this", Some(("show you this", 0, 2))),
            ("look at my screen", Some(("look at my screen", 0, 3))),
            ("look at the screen", Some(("look at the screen", 0, 3))),
            ("okay, look at the screen.", Some(("look at the screen", 1, 4))),
            ("how many times have you actually looked at the screen?", None),
            ("don't look at the screen", None),
            ("nie. okej, spójrz na ekran.", Some(("spójrz", 2, 2))),
            ("see my screen", Some(("see my screen", 0, 2))),
            // Pointing asks carry their screenshot.
            ("draw me that", Some(("draw me that", 0, 2))),
            ("can you show me that on the page", Some(("show me that on the page", 2, 7))),
            ("show me where the save button is", Some(("show me where", 0, 2))),
            ("don't show me where it is", None),
            ("don't take a screenshot", None),
            ("don’t take a screenshot", None),
            ("do not take a screenshot", None),
            ("don't please take a screenshot", None),
            ("do not look at this", None),
            ("never capture the screen", None),
            ("do not show you this", None),
            ("don't screenshot this", None),
            ("do not take a look", None),
            ("don't see my screen", None),
            ("look at that", None),
            ("look into this", None),
            ("take the screenshot", None),
            ("take a screenshotter", None),
            ("take a screennshott", None),
            ("look-at this", None),
            ("take-a screenshot", None),
            ("look atthis", None),
            ("outlook at this", None),
            ("look. at this", None),
            ("do not wait; look at this", Some(("look at this", 3, 5))),
            ("do not wait but look at this", Some(("look at this", 4, 6))),
            ("zobacz to", Some(("zobacz to", 0, 1))),
            ("ZOBACZ TO!", Some(("zobacz to", 0, 1))),
            ("proszę zobacz to", Some(("zobacz to", 1, 2))),
            ("zobacz yyy to", Some(("zobacz to", 0, 2))),
            ("zobacz no to", Some(("zobacz to", 0, 2))),
            ("zobacz proszę to", Some(("zobacz to", 0, 2))),
            ("zobacz eee yyy to", Some(("zobacz to", 0, 3))),
            ("zobacz, yyy, to", Some(("zobacz to", 0, 2))),
            ("spójrz", Some(("spójrz", 0, 0))),
            ("spojrz", Some(("spójrz", 0, 0))),
            ("spójrz na to", Some(("spójrz na to", 0, 2))),
            ("spojrz na to", Some(("spójrz na to", 0, 2))),
            ("teraz spójrz na to zdjęcie", Some(("spójrz na to", 1, 3))),
            ("zrób zrzut ekranu", Some(("zrób zrzut ekranu", 0, 2))),
            ("zrob zrzut ekranu", Some(("zrób zrzut ekranu", 0, 2))),
            ("zrób screenshot", Some(("zrób screenshot", 0, 1))),
            ("zrob screenshota", Some(("zrób screenshota", 0, 1))),
            ("zrób zdjęcie ekranu", Some(("zrób zdjęcie ekranu", 0, 2))),
            ("zrob zdjecie ekranu", Some(("zrób zdjęcie ekranu", 0, 2))),
            ("popatrz na to", Some(("popatrz na to", 0, 2))),
            ("zerknij na to", Some(("zerknij na to", 0, 2))),
            ("powiedział mi żeby zobacz to", Some(("zobacz to", 3, 4))),
            ("nie zobacz to", None),
            ("nie spójrz na to", None),
            ("nie zrób zrzut ekranu", None),
            ("nie zrob screenshot", None),
            ("nie popatrz na to", None),
            ("nie zerknij na to", None),
            ("nie proszę zrób zdjęcie ekranu", None),
            ("zobacz nie to", None),
            ("zobacz jutro to", None),
            ("zobacz yyy eee no to", None),
            ("spojrzą na to", None),
            ("spojrza na to", None),
            ("spojrzy na to", None),
            ("spójrzmy na to", None),
            ("zobaczy to", None),
            ("zrobi zrzut ekranu", None),
            ("popatrzy na to", None),
            ("zerknie na to", None),
            ("spójrz-na-to", None),
            ("nie czekaj ale zobacz to", Some(("zobacz to", 3, 4))),
        ];
        assert!(cases.len() >= 40);
        for (text, expected) in cases {
            let mut matcher = PhraseMatcher::default();
            let w = words(text, 1_000_000_000);
            let matched = matcher.observe_utterance("mic", "table", &w, true);
            if let Some((phrase, start, end)) = expected {
                assert_eq!(matched.len(), 1, "{text}");
                assert_eq!(matched[0].phrase, *phrase, "{text}");
                assert_eq!(matched[0].start_ns, w[*start].start_ns, "{text}");
                assert_eq!(matched[0].trigger_ns, w[*end].start_ns, "{text}");
            } else {
                assert!(
                    matched.is_empty(),
                    "unexpected trigger for {text}: {matched:?}"
                );
            }
        }
    }
    #[test]
    fn bug_note_is_not_a_spoken_command() {
        // Bug notes are Heimdall's `bug_note` tool now: no table matches the old phrases.
        for text in ["bug note", "Bug note.", "bugnote", "bug notes", "But note, it failed", "Heimdall bug", "zgłoś błąd"] {
            assert!(PhraseMatcher::default().observe_utterance("mic", "x", &words(text, 0), true).is_empty(), "{text}");
            assert!(near_misses(SCREEN_PHRASES, &words(text, 0)).is_empty(), "{text}");
        }
    }
    #[test]
    fn near_misses_explain_what_almost_fired() {
        let reasons = |text: &str, phrases: &[&str]| -> Vec<(String, String, String)> {
            near_misses(phrases, &words(text, 0))
                .into_iter()
                .map(|n| (n.phrase, n.heard, n.reason))
                .collect()
        };
        let r = |p: &str, h: &str, why: &str| (p.to_string(), h.to_string(), why.to_string());
        assert_eq!(reasons("take a screen shot", SCREEN_PHRASES), vec![r("take a screenshot", "take a", "partial")]);
        assert_eq!(reasons("don't take a screenshot", SCREEN_PHRASES), vec![r("take a screenshot", "take a screenshot", "negated")]);
        assert_eq!(reasons("look at thiss", SCREEN_PHRASES), vec![r("look at this", "look at thiss", "loose")]);
        assert_eq!(reasons("zrobi zrzut ekranu", SCREEN_PHRASES), vec![r("zrób zrzut ekranu", "zrobi zrzut ekranu", "loose")]);
        assert!(reasons("look at this", SCREEN_PHRASES).is_empty());
        assert!(reasons("what a nice day", SCREEN_PHRASES).is_empty());
        assert_eq!(reasons("when I take a screenshot for him", SCREEN_PHRASES), vec![r("take a screenshot", "take a screenshot", "embedded")]);
    }
    /// Night review D4: the user's own microphone lines from the archive (12–20 Oct). A quick command fires in command
    /// position; the same words inside a statement do not. Each line is verbatim from the transcript, except the two "my screen" variants.
    #[test]
    fn archive_lines_fire_only_as_commands() {
        let fires: &[(&str, &str)] = &[
            ("Look at this.", "look at this"),
            ("Okay, look at this work.", "look at this"),
            ("Take a screenshot.", "take a screenshot"),
            ("Take a look at it.", "take a look"),
            ("Take a screenshot again.", "take a screenshot"),
            ("Yo, look at this.", "look at this"),
            ("Oh shit, yeah, Fosta found it. Look at this.", "look at this"),
            ("Hello, take a screenshot.", "take a screenshot"),
            ("Uh look at this.", "look at this"),
            ("Hey, take a screenshot.", "take a screenshot"),
            ("Hey Heimda, I want you to take a look at this.", "take a look"),
            ("Haim dal, take a look at this.", "take a look"),
            ("Hi Mdal, I'm not speculating. Take a screenshot.", "take a screenshot"),
            ("Yo, Heimdall, look at this. Uh I want you to spawn a thread uh in T3 Code and I want you to figure out what's", "look at this"),
            ("Okay, Heimdall, look at this. Like this is a beautiful design.", "look at this"),
            ("Yo, Heimdall, take a look at the screen right now. I just want you to yeah, take a screenshot.", "look at the screen"),
            ("Okay, so something is definitely not right. Take a screenshot.", "take a screenshot"),
            ("I haven't done this because this just works really well. Like, look at this.", "look at this"),
            ("Yep. That's that's why the streaming models, like look at this, like Nimotron is the best one for uh streaming", "look at this"),
            ("Additionally, it's just so fucking disgusting. Like look at the screen right now.", "look at the screen"),
            ("Next thing. Yeah, look at this. Like this is much better.", "look at this"),
            ("Look at this. Take a screenshot. And it should attach.", "look at this"),
            ("Yeah, so uh interesting interesting interesting there is still like agent ready uh in but take a look at this", "take a look"),
            ("Okay take a look at my screen", "look at my screen"),
        ];
        let silent: &[&str] = &[
            // 20 Oct 18:39:46, to Morgan (the stuck-island bug note's heard cue).
            "9 billion. Yeah, I mean 9 billion are suck anyway. But yeah, like like like he literally figured out that the PP people on the forum, they complain that this entire family is benchmark max, you know? So yeah, I wouldn't even look at this. Yeah, Heimdal, we are not doing this.",
            // 20 Oct 19:14:43.
            "Do we send images to the decider right now? Uh, especially when we uh like when I take a screenshot for him to decide whether to send it or not? I think we do, right?",
            "We should take a look at the Taoino Skilson website that we have.",
            "Which is very interesting and right now I said look at this and it attached a screenshot and it did a screenshot again and is going to add it here.",
            "I wonder when you look at this, um, okay, so it's very sensitive.",
            "So right now I don't want you to actually look at this. Don't look at it.",
            "I don't know, like I just always look at this that's, you know, unless my salary depends on like how",
            "Okay, there is a little bit of a um animation problem, so like when the look at the screen.",
            "on the edges, for example, when I say look at this, or like use some commands that there is like the edges of the screen",
            // 16 Oct, showing Morgan his screen on the call.
            "So you can look at this",
            "We should take a look at my screen later",
        ];
        for (text, phrase) in fires {
            let matched = PhraseMatcher::default().observe_utterance("mic", "a", &words(text, 1_000_000_000), true);
            assert_eq!(matched.first().map(|m| m.phrase.as_str()), Some(*phrase), "{text}");
        }
        for text in silent {
            let matched = PhraseMatcher::default().observe_utterance("mic", "a", &words(text, 1_000_000_000), true);
            assert!(matched.is_empty(), "{text}: {matched:?}");
        }
        // A pause over a second opens a clause.
        let mut w = words("we were done there look at this", 0);
        for x in &mut w[4..] {
            x.start_ns += 1_500_000_000;
            x.end_ns += 1_500_000_000;
        }
        assert_eq!(PhraseMatcher::default().observe_utterance("mic", "a", &w, true).len(), 1);
    }
    /// 22 Oct screen-phrase benchmark: Parakeet writes "I said, look at this." and the comma opened a clause, so reported
    /// speech fired 9 of 9 times. A speech verb's quote is not a command; a real lead still is.
    #[test]
    fn reported_speech_after_a_speech_verb_does_not_fire() {
        let silent: &[&str] = &[
            "I said, look at this.",
            "I said look at this.",
            "I said: take a screenshot.",
            "He told me, look at this.",
            "She asked him, take a look at my screen.",
            "And I said, Heimdall, look at this, and it attached one.",
            "I was like, look at this.",
            "Powiedziałem, zobacz to.",
            "Powiedziałem mu, zobacz to.",
            "A on mówi, spójrz na to.",
            "Zapytałam go, zrób zrzut ekranu.",
            "I said to him, look at this.",
            "Powiedziałem mu wtedy, zobacz to.",
            "Powiedział: no, zobacz to.",
            "Powiedziałem do niego, zobacz to.",
        ];
        for text in silent {
            let w = words(text, 1_000_000_000);
            let matched = PhraseMatcher::default().observe_utterance("mic", "a", &w, true);
            assert!(matched.is_empty(), "{text}: {matched:?}");
            // Near-miss triage still sees the phrase, as embedded.
            assert!(near_misses(SCREEN_PHRASES, &w).iter().any(|m| m.reason == "embedded"), "{text}");
        }
        let fires: &[(&str, &str)] = &[
            ("Okay, look at this.", "look at this"),
            ("Heimdall, look at this.", "look at this"),
            ("Like I said, look at this.", "look at this"),
            ("As I said, take a screenshot.", "take a screenshot"),
            ("Like I already said, look at this.", "look at this"),
            ("As I've already said, take a look.", "take a look"),
            ("I told you, look at this.", "look at this"),
            ("That's what he said. Look at this.", "look at this"),
            ("Jak mówiłem, zobacz to.", "zobacz to"),
            ("Mówię ci, zobacz to.", "zobacz to"),
            ("Heimdall, zobacz to.", "zobacz to"),
        ];
        for (text, phrase) in fires {
            let matched = PhraseMatcher::default().observe_utterance("mic", "a", &words(text, 1_000_000_000), true);
            assert_eq!(matched.first().map(|m| m.phrase.as_str()), Some(*phrase), "{text}");
        }
        // A pause over a second after the verb ends the report.
        let mut w = words("I said, look at this.", 0);
        for x in &mut w[2..] {
            x.start_ns += 1_500_000_000;
            x.end_ns += 1_500_000_000;
        }
        assert_eq!(PhraseMatcher::default().observe_utterance("mic", "a", &w, true).len(), 1);
    }
    /// 22 Oct screen-phrase benchmark: the 4-second phrase limit ran to the last word's end, which Parakeet stretches over
    /// trailing noise ("Heimdall, take a screenshot." spanned 4.56 s and was dropped). It runs to the last word's start.
    #[test]
    fn phrase_span_ends_at_the_last_words_start() {
        let at = |text: &str, start_ms: u64, end_ms: u64| TimedWord {
            text: text.into(),
            start_ns: start_ms * 1_000_000,
            end_ns: end_ms * 1_000_000,
        };
        let stretched = [at("Heimdall,", 500, 900), at("take", 1_000, 1_300), at("a", 1_300, 1_400), at("screenshot.", 1_400, 5_560)];
        let m = PhraseMatcher::default().observe_utterance("mic", "a", &stretched, true);
        assert_eq!(m.len(), 1, "{m:?}");
        assert_eq!((m[0].start_ns, m[0].trigger_ns), (1_000_000_000, 1_400_000_000));
        // Still bounded: the last word starting more than 4 s after the first does not fire.
        let slow = [at("zrób", 1_000, 1_500), at("zrzut", 2_400, 4_000), at("ekranu.", 4_900, 6_100)];
        assert_eq!(PhraseMatcher::default().observe_utterance("mic", "a", &slow, true).len(), 1);
        let slower = [at("zrób", 1_000, 1_500), at("zrzut", 2_400, 4_200), at("ekranu.", 5_100, 6_100)];
        assert!(PhraseMatcher::default().observe_utterance("mic", "a", &slower, true).is_empty());
    }
    /// Review 21 Oct (listen, item 7): a quick command joined to another command by "and"/"then" fires; the same join
    /// inside a statement does not.
    #[test]
    fn a_command_joined_to_another_command_fires() {
        let fires: &[(&str, &str)] = &[
            ("open Safari and take a screenshot", "take a screenshot"),
            ("Open Safari and take a screenshot.", "take a screenshot"),
            ("Okay, open the settings and look at this.", "look at this"),
            ("Scroll down and then look at this", "look at this"),
            ("Can you open Safari and take a screenshot?", "take a screenshot"),
            ("Heimdall, I want you to open Safari and take a look at my screen", "look at my screen"),
            ("okay and look at this", "look at this"),
            ("Go to the website and take a look", "take a look"),
            // 17 Oct, to Heimdall: "Can you focus on a different screen or like bring me somewhere? And then look at the screen…"
            ("Can you bring me somewhere? And then look at the screen and circle it?", "look at the screen"),
        ];
        let silent: &[&str] = &[
            // Night-2 re-review: a yes/no question or design talk opening with a non-command verb.
            "do you guys open Safari and take a screenshot when testing?",
            "use the transcript and take a screenshot of the island later",
            "We should open Safari and take a look at the website",
            "when I open Safari and take a screenshot it breaks",
            "he told me to open it and look at this",
            "so you can open it and look at this",
            "Don't open Safari and take a screenshot.",
            "everything is there and take a look how it looks on the phone",
            "I opened Safari and take a screenshot",
            "we went to the shop and then we look at this thing",
        ];
        for (text, phrase) in fires {
            let matched = PhraseMatcher::default().observe_utterance("mic", "a", &words(text, 1_000_000_000), true);
            assert_eq!(matched.first().map(|m| m.phrase.as_str()), Some(*phrase), "{text}");
        }
        for text in silent {
            let matched = PhraseMatcher::default().observe_utterance("mic", "a", &words(text, 1_000_000_000), true);
            assert!(matched.is_empty(), "{text}: {matched:?}");
        }
        // The near-miss report agrees: a joined command is not "embedded"; a statement's join is.
        let reasons = |text: &str| -> Vec<(String, String, String)> {
            near_misses(SCREEN_PHRASES, &words(text, 0)).into_iter().map(|n| (n.phrase, n.heard, n.reason)).collect()
        };
        let r = |p: &str, h: &str, why: &str| (p.to_string(), h.to_string(), why.to_string());
        assert!(reasons("open Safari and take a screenshot").is_empty());
        assert_eq!(
            reasons("we should open Safari and take a screenshot"),
            vec![r("take a screenshot", "take a screenshot", "embedded")]
        );
    }
    #[test]
    fn timestamp_revisions_and_final_redecode_keep_occurrence_identity() {
        let mut m = PhraseMatcher::default();
        assert_eq!(
            m.observe_utterance("mic", "u1", &words("look at this", 1_000_000_000), false)
                .len(),
            1
        );
        for (start, final_) in [
            (3_000_000_000, false),
            (5_000_000_000, true),
            (70_000_000_000, true),
        ] {
            assert!(m
                .observe_utterance("mic", "u1", &words("look at this please", start), final_)
                .is_empty());
        }
        assert_eq!(
            m.observe_utterance("mic", "u2", &words("look at this", 80_000_000_000), false)
                .len(),
            1
        );
        m.reset_source("mic");
        assert_eq!(
            m.observe_utterance("mic", "u1", &words("look at this", 1_000_000_000), true)
                .len(),
            1
        );
    }
    #[test]
    fn cumulative_transcript_does_not_replay_after_sixty_seconds() {
        let mut m = PhraseMatcher::default();
        let mut w = words("look at this", 1_000_000_000);
        assert_eq!(m.observe("mic", &w).len(), 1);
        w.extend(words("we kept talking.", 70_000_000_000));
        assert!(m.observe("mic", &w).is_empty());
        w.extend(words("look at this", 71_000_000_000));
        let next = m.observe("mic", &w);
        assert_eq!(next.len(), 1);
        assert_eq!(next[0].start_ns, 71_000_000_000);
        assert!(m.observe("mic", &w).is_empty());
    }
    #[test]
    fn longest_alias_waits_for_completion_and_short_alias_flushes() {
        let mut m = PhraseMatcher::default();
        for text in ["spójrz", "spójrz na"] {
            assert!(m
                .observe_utterance("mic", "u1", &words(text, 0), false)
                .is_empty());
        }
        let long = m.observe_utterance("mic", "u1", &words("spójrz na to", 0), false);
        assert_eq!(long.len(), 1);
        assert_eq!(long[0].phrase, "spójrz na to");
        assert_eq!(long[0].start_ns, 0);
        assert_eq!(long[0].trigger_ns, 400_000_000);
        assert!(m
            .observe_utterance("mic", "u1", &words("spójrz na to", 2_000_000_000), true)
            .is_empty());
        assert_eq!(
            m.observe_utterance("mic", "u2", &words("spójrz", 0), true)
                .len(),
            1
        );
        assert_eq!(
            m.observe_utterance("mic", "u3", &words("spójrz proszę", 0), false)
                .len(),
            1
        );
        assert_eq!(
            m.observe_utterance("mic", "u4", &words("spójrz.", 0), false)
                .len(),
            1
        );
    }
    #[test]
    fn longest_alias_resolves_overlaps_at_different_starts() {
        let mut m = PhraseMatcher::default();
        assert!(m
            .observe_utterance("mic", "u1", &words("take a look", 0), false)
            .is_empty());
        let matched =
            m.observe_utterance("mic", "u1", &words("take a look at my screen", 0), false);
        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].phrase, "look at my screen");
        assert_eq!(matched[0].start_ns, 400_000_000);
        assert_eq!(matched[0].trigger_ns, 1_000_000_000);
        assert!(m
            .observe_utterance("mic", "u1", &words("take a look", 2_000_000_000), true)
            .is_empty());
        // Each in command position (D4): the second opens its own clause.
        let twice = m.observe_utterance("mic", "u2", &words("look at this, look at this", 0), true);
        assert_eq!(twice.len(), 2);
    }
    #[test]
    fn disk_only_on_action_and_png_decodes() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("shots");
        let mut b = ScreenBuffer::new(crate::ScreenConfig {fps:1,..Default::default()}).unwrap();
        for id in [1, 2] {
            b.push(
                crate::Frame::new(
                    crate::DisplayInfo {
                        id,
                        x: 0.,
                        y: 0.,
                        width: 2,
                        height: 1,
                    },
                    1_000_000_000,
                    2,
                    1,
                    vec![id as u8; 6],
                )
                .unwrap(),
            )
            .unwrap();
        }
        assert!(!dest.exists());
        let matched = PhraseMatch {
            source: "mic".into(),
            phrase: "look at this".into(),
            start_ns: 1_000_000_000,
            trigger_ns: 1_500_000_000,
        };
        let action = attach_screenshot(&mut b, &matched, 2_000_000_000, &dest).unwrap();
        assert_eq!(action.shots.len(), 2);
        assert!(action.note.starts_with("[screenshot "));
        let decoder = png::Decoder::new(std::io::BufReader::new(
            std::fs::File::open(&action.image_path).unwrap(),
        ));
        let mut reader = decoder.read_info().unwrap();
        let mut rgb = vec![0; reader.output_buffer_size().unwrap()];
        reader.next_frame(&mut rgb).unwrap();
        assert_eq!(rgb, [1; 6]);
        assert!(attach_screenshot(&mut b, &matched, 22_000_000_000, &dest).is_ok(), "recorded history does not expire");
        assert_eq!(std::fs::read_dir(&dest).unwrap().count(), 4);
    }
    #[test]
    fn default_history_keeps_final_only_trigger_until_window_decode() {
        let temp=tempfile::tempdir().unwrap();
        let mut buffer=ScreenBuffer::new(crate::ScreenConfig {fps:1,..Default::default()}).unwrap();
        for second in 0..=18 {
            buffer.push(crate::Frame::new(crate::DisplayInfo {id:1,x:0.,y:0.,width:2,height:1},second*1_000_000_000,2,1,vec![second as u8;6]).unwrap()).unwrap();
        }
        let mut commands=ScreenCommands::new(temp.path().join("shots"));
        let words:Vec<_>=["look","at","this"].into_iter().enumerate().map(|(i,text)|TimedWord {text:text.into(),start_ns:1_000_000_000+i as u64*200_000_000,end_ns:1_200_000_000+i as u64*200_000_000}).collect();
        let events=commands.process_utterance("mic","long-window",&words,true,&mut buffer,18_000_000_000);
        let ScreenActionEvent::Action(action)=&events[0]else{panic!("{events:?}");};
        assert_eq!(action.shots[0].frame_ns,1_000_000_000);
        assert!(commands.process_utterance("mic","long-window",&words,true,&mut buffer,18_000_000_000).is_empty());
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum ScreenActionEvent {
    Action(Action),
    Failed {
        source: String,
        trigger_ns: u64,
        error: String,
    },
}
/// App-facing fixed-command handler. Feed every eligible live partial directly;
/// returned notes are additive transcript entries, never replacement text.
pub struct ScreenCommands {
    matcher: PhraseMatcher,
    shots_dir: PathBuf,
    #[cfg(feature = "listen-adapter")]
    utterances: HashMap<String, ListenUtterance>,
}
impl ScreenCommands {
    pub fn new(shots_dir: PathBuf) -> Self {
        Self {
            matcher: PhraseMatcher::default(),
            shots_dir,
            #[cfg(feature = "listen-adapter")]
            utterances: HashMap::new(),
        }
    }
    pub fn process_partial(
        &mut self,
        source: &str,
        words: &[TimedWord],
        buffer: &mut ScreenBuffer,
        now_ns: u64,
    ) -> Vec<ScreenActionEvent> {
        self.process_utterance(source, "cumulative", words, false, buffer, now_ns)
    }
    /// Explicit identity path for callers that do not use ListenEvent.
    #[allow(clippy::too_many_arguments)]
    pub fn process_utterance(
        &mut self,
        source: &str,
        utterance_id: &str,
        words: &[TimedWord],
        is_final: bool,
        buffer: &mut ScreenBuffer,
        now_ns: u64,
    ) -> Vec<ScreenActionEvent> {
        self.matcher
            .observe_utterance(source, utterance_id, words, is_final)
            .into_iter()
            .map(
                |m| match attach_screenshot(buffer, &m, now_ns, &self.shots_dir) {
                    Ok(action) => ScreenActionEvent::Action(action),
                    Err(e) => ScreenActionEvent::Failed {
                        source: m.source,
                        trigger_ns: m.trigger_ns,
                        error: e.to_string(),
                    },
                },
            )
            .collect()
    }
    /// Live commands release the ring mutex before any native one-shot request.
    #[allow(clippy::too_many_arguments)]
    pub fn process_utterance_live(
        &mut self,
        source: &str,
        utterance_id: &str,
        words: &[TimedWord],
        is_final: bool,
        buffer: &std::sync::Arc<std::sync::Mutex<ScreenBuffer>>,
        now_ns: u64,
    ) -> Vec<ScreenActionEvent> {
        self.matcher
            .observe_utterance(source, utterance_id, words, is_final)
            .into_iter()
            .map(
                |m| match crate::attach_command_screenshot(buffer, &m, now_ns, &self.shots_dir) {
                    Ok(action) => ScreenActionEvent::Action(action),
                    Err(e) => ScreenActionEvent::Failed {
                        source: m.source,
                        trigger_ns: m.trigger_ns,
                        error: e.to_string(),
                    },
                },
            )
            .collect()
    }
    pub fn reset_source(&mut self, source: &str) {
        self.matcher.reset_source(source);
        #[cfg(feature = "listen-adapter")]
        self.utterances.remove(source);
    }
}

#[cfg(feature = "listen-adapter")]
impl From<&listen::Word> for TimedWord {
    fn from(w: &listen::Word) -> Self {
        Self {
            text: w.text.clone(),
            start_ns: w.start_ns,
            end_ns: w.end_ns,
        }
    }
}
#[cfg(feature = "listen-adapter")]
#[derive(Default)]
struct ListenUtterance {
    id: String,
    finalized: bool,
    completed: HashMap<u64, String>,
}
#[cfg(feature = "listen-adapter")]
impl ScreenCommands {
    /// Feed SpeechStart too: its host time supplies the stable utterance ID.
    /// A resumed SpeechStart after an internal pause is still the same utterance.
    /// Routing decides whether this source belongs to the user before this adapter.
    pub fn process_listen_event(
        &mut self,
        event: &listen::ListenEvent,
        buffer: &mut ScreenBuffer,
        now_ns: u64,
    ) -> Vec<ScreenActionEvent> {
        match event {
            listen::ListenEvent::SpeechStart {
                source,
                host_time_ns,
            } => {
                let u = self.utterances.entry(source.clone()).or_default();
                if u.id.is_empty() || u.finalized {
                    u.id = host_time_ns.to_string();
                    u.finalized = false;
                }
                vec![]
            }
            listen::ListenEvent::PartialTranscript { source, words, .. }
            | listen::ListenEvent::FinalUtterance { source, words, .. } => {
                let u = self.utterances.entry(source.clone()).or_default();
                // Legacy callers omitting lifecycle events get one cumulative
                // scope; do not guess identity from mutable ASR timestamps.
                if u.id.is_empty() {
                    u.id = "cumulative".into();
                }
                let is_final = matches!(event, listen::ListenEvent::FinalUtterance { .. });
                let id = if let listen::ListenEvent::FinalUtterance { start_ns, .. } = event {
                    if let Some(id) = u.completed.get(start_ns) {
                        id.clone()
                    } else {
                        u.finalized = true;
                        u.completed.insert(*start_ns, u.id.clone());
                        u.id.clone()
                    }
                } else {
                    u.id.clone()
                };
                let words: Vec<_> = words.iter().map(TimedWord::from).collect();
                self.process_utterance(source, &id, &words, is_final, buffer, now_ns)
            }
            listen::ListenEvent::Discontinuity { source, .. } => {
                self.reset_source(source);
                vec![]
            }
            _ => vec![],
        }
    }
}

#[cfg(test)]
mod handler_tests {
    use super::*;
    #[test]
    fn frame_is_selected_at_phrase_start_not_completion() {
        let dir = tempfile::tempdir().unwrap();
        let mut buffer = ScreenBuffer::new(crate::ScreenConfig {fps:1,..Default::default()}).unwrap();
        for (time, pixel) in [(1_000_000_000, 17), (1_400_000_000, 99)] {
            buffer
                .push(
                    crate::Frame::new(
                        crate::DisplayInfo {
                            id: 1,
                            x: 0.,
                            y: 0.,
                            width: 1,
                            height: 1,
                        },
                        time,
                        1,
                        1,
                        vec![pixel; 3],
                    )
                    .unwrap(),
                )
                .unwrap();
        }
        let mut handler = ScreenCommands::new(dir.path().join("shots"));
        let words = ["spójrz", "na", "to"]
            .iter()
            .enumerate()
            .map(|(i, text)| TimedWord {
                text: (*text).into(),
                start_ns: 1_000_000_000 + i as u64 * 200_000_000,
                end_ns: 1_150_000_000 + i as u64 * 200_000_000,
            })
            .collect::<Vec<_>>();
        let events =
            handler.process_utterance("mic", "u1", &words, false, &mut buffer, 1_600_000_000);
        let ScreenActionEvent::Action(action) = &events[0] else {
            panic!("{events:?}")
        };
        assert_eq!(action.trigger_ns, 1_400_000_000);
        assert_eq!(action.shots[0].frame_ns, 1_000_000_000);
        let decoder = png::Decoder::new(std::io::BufReader::new(
            std::fs::File::open(&action.image_path).unwrap(),
        ));
        let mut reader = decoder.read_info().unwrap();
        let mut rgb = vec![0; reader.output_buffer_size().unwrap()];
        reader.next_frame(&mut rgb).unwrap();
        assert_eq!(rgb, [17; 3]);
    }
    #[cfg(feature = "listen-adapter")]
    #[test]
    fn listen_lifecycle_keeps_partials_finals_and_resumed_speech_in_one_scope() {
        let dir = tempfile::tempdir().unwrap();
        let mut h = ScreenCommands::new(dir.path().join("shots"));
        let mut buffer = ScreenBuffer::new(crate::ScreenConfig {fps:1,..Default::default()}).unwrap();
        let make_words = |time| {
            ["look", "at", "this"]
                .iter()
                .enumerate()
                .map(|(i, w)| listen::Word {
                    text: (*w).into(),
                    start_ns: time + i as u64 * 200_000_000,
                    end_ns: time + i as u64 * 200_000_000 + 150_000_000,
                    confidence: 1.0,
                })
                .collect::<Vec<_>>()
        };
        let start = |time| listen::ListenEvent::SpeechStart {
            source: "mic".into(),
            host_time_ns: time,
        };
        let partial = |time| listen::ListenEvent::PartialTranscript {
            source: "mic".into(),
            text: "look at this".into(),
            words: make_words(time),
            stable_words: 0,
        };
        let final_ = |time, start_ns| listen::ListenEvent::FinalUtterance {
            source: "mic".into(),
            text: "look at this".into(),
            words: make_words(time),
            start_ns,
            end_ns: time + 550_000_000,
            end_of_turn_probability: 1.,
            forced: false,
        };
        h.process_listen_event(&start(1_000_000_000), &mut buffer, 2_000_000_000);
        assert_eq!(
            h.process_listen_event(&partial(1_000_000_000), &mut buffer, 2_000_000_000)
                .len(),
            1
        );
        h.process_listen_event(
            &listen::ListenEvent::SpeechEnd {
                source: "mic".into(),
                host_time_ns: 2_000_000_000,
            },
            &mut buffer,
            2_000_000_000,
        );
        h.process_listen_event(&start(3_000_000_000), &mut buffer, 4_000_000_000);
        assert!(h
            .process_listen_event(&partial(3_000_000_000), &mut buffer, 4_000_000_000)
            .is_empty());
        let f = final_(3_000_000_000, 1_000_000_000);
        assert!(h
            .process_listen_event(&f, &mut buffer, 4_000_000_000)
            .is_empty());
        h.process_listen_event(&start(80_000_000_000), &mut buffer, 81_000_000_000);
        // Replay of an older final still resolves to its completed utterance.
        assert!(h
            .process_listen_event(&f, &mut buffer, 81_000_000_000)
            .is_empty());
        assert_eq!(
            h.process_listen_event(&partial(80_000_000_000), &mut buffer, 81_000_000_000)
                .len(),
            1
        );
        assert!(h
            .process_listen_event(
                &final_(82_000_000_000, 80_000_000_000),
                &mut buffer,
                83_000_000_000
            )
            .is_empty());
        h.process_listen_event(
            &listen::ListenEvent::Discontinuity {
                source: "mic".into(),
                host_time_ns: 84_000_000_000,
            },
            &mut buffer,
            84_000_000_000,
        );
        h.process_listen_event(&start(1_000_000_000), &mut buffer, 85_000_000_000);
        assert_eq!(
            h.process_listen_event(&partial(1_000_000_000), &mut buffer, 85_000_000_000)
                .len(),
            1
        );
    }
    #[test]
    fn live_partial_emits_once_and_failed_capture_is_explicit() {
        let dir = tempfile::tempdir().unwrap();
        let mut handler = ScreenCommands::new(dir.path().join("shots"));
        let words: Vec<_> = ["look", "at", "this"]
            .iter()
            .enumerate()
            .map(|(i, w)| TimedWord {
                text: (*w).into(),
                start_ns: 1_000_000_000 + i as u64 * 100_000_000,
                end_ns: 1_050_000_000 + i as u64 * 100_000_000,
            })
            .collect();
        let original = words.iter().map(|w| w.text.clone()).collect::<Vec<_>>();
        let mut buffer = ScreenBuffer::new(crate::ScreenConfig {fps:1,..Default::default()}).unwrap();
        assert!(matches!(
            &handler.process_partial("mic", &words, &mut buffer, 2_000_000_000)[0],
            ScreenActionEvent::Failed { .. }
        ));
        handler.reset_source("mic");
        buffer
            .push(
                crate::Frame::new(
                    crate::DisplayInfo {
                        id: 1,
                        x: 0.,
                        y: 0.,
                        width: 1,
                        height: 1,
                    },
                    1_000_000_000,
                    1,
                    1,
                    vec![0; 3],
                )
                .unwrap(),
            )
            .unwrap();
        assert!(matches!(
            &handler.process_partial("mic", &words, &mut buffer, 2_000_000_000)[0],
            ScreenActionEvent::Action(_)
        ));
        assert!(handler
            .process_partial("mic", &words, &mut buffer, 2_000_000_000)
            .is_empty());
        assert_eq!(
            words.iter().map(|w| w.text.clone()).collect::<Vec<_>>(),
            original
        );
    }
}
