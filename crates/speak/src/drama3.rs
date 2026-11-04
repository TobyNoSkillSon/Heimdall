//! Supervised serial stdio client. Weights stay in the pinned HF snapshot.
use crate::Audio;
use anyhow::{anyhow, bail, ensure, Context, Result};
use base64::Engine as _;
use serde_json::{json, Value};
use std::{
    collections::VecDeque,
    io::{BufRead, BufReader, Write},
    path::PathBuf,
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicI32, AtomicU64, AtomicUsize, Ordering},
        mpsc, Arc, Mutex,
    },
    time::{Duration, Instant},
};

/// The host supplies its independent verifier. No ASR model lives here.
pub type SentenceVerifier = dyn Fn(&str, &Audio, &dyn Fn() -> bool) -> Result<Value> + Send + Sync;

/// The Jarvis voice's identity in the clip store, not the weights: it stayed when the weights moved to RL
/// iteration-100 (29 Oct) so the user's picked bank and ack takes keep playing. `settings()` records the weights.
pub const GENERATOR: &str = "drama3-cc639e27-exact-jarvis-v1";
pub const VOICE: &str = "jarvis";
/// Removes `[tags]` and `<tags>` only, keeping the punctuation a reader sees.
pub fn strip_tags(text: &str) -> String {
    static TAGS: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let tags = TAGS.get_or_init(|| regex::Regex::new(r"\[[^\]]*\]|<[^>]*>").unwrap());
    let stripped = tags.replace_all(text, " ");
    stripped.split_whitespace().collect::<Vec<_>>().join(" ")
}
/// The sentences as written (commas and colons kept) when they line up with the spoken ones, else the spoken.
pub fn display_sentences(text: &str) -> Vec<String> {
    let spoken = sentences(&normalize(text));
    let written = sentences(&strip_tags(text));
    if written.len() == spoken.len() { written } else { spoken }
}
/// Symbols spelled as the words they are said as, so both voices say them and the word check expects them (24 Oct
/// 12:19: "40%" was stripped to "40", Jarvis said "40 percent", and all three takes were rejected for the added word).
/// English thousands separators are dropped, so "18,500" is one number, not "18 500" (24 Oct 03:17 and 03:37: Jarvis
/// read "3 100 dollars" as "300, 100"). Text with Polish letters is voiced in Polish (`agent::reply::language`), so its
/// symbols are Polish words and its commas stay decimal commas. `text.py` `spell_symbols` is the same.
pub fn spell_symbols(text: &str) -> String {
    use regex::{Captures, Regex};
    use std::sync::OnceLock;
    static RULES: OnceLock<[Regex; 10]> = OnceLock::new();
    let [separator, currency, zloty, after, suffix, percent, dimension, times, degrees, number] = RULES.get_or_init(|| {
        [
            r"\b[1-9]\d{0,2}(?:,\d{3})+\b",
            r"([$€£])\s?(\d+(?:[.,]\d+)?)(?:\s?(k|K|m|M|b|B|bn|thousand|million|billion)\b)?",
            r"(\d+(?:[.,]\d+)?)\s?(?:(k|K|m|M|bn)\s?)?zł\b",
            r"\b(\d+(?:[.,]\d+)?)\s?([$€£])",
            r"\b(\d+(?:\.\d+)?)(k|K|M)\b",
            r"(\d)\s?%",
            r"\b(\d+(?:\.\d+)?)(?:[xX×]| [xX×] )(\d)",
            r"\b(\d+(?:\.\d+)?)[xX×]\b",
            r"(\d+(?:[.,]\d+)?)\s?°(?:\s?([CF])\b)?",
            r"#(\d+)\b",
        ]
        .map(|r| Regex::new(r).unwrap())
    });
    let polish = text.chars().any(|c| "ąćęłńśźżĄĆĘŁŃŚŹŻ".contains(c));
    // A unit after an amount, by its number: "1 dollar", "5 dollars"; in Polish "1 dolar", "2 dolary", "5 dolarów", and
    // "1,5 dolara" or "5 tysięcy dolarów".
    let counted = |amount: &str, magnitude: bool, en: &str, pl: [&str; 4]| -> String {
        if !polish {
            return if amount == "1" && !magnitude { en.to_owned() } else { format!("{en}s") };
        }
        let [one, few, many, part] = pl;
        if magnitude {
            return many.to_owned();
        }
        match amount.parse::<u64>() {
            Err(_) => part,
            Ok(1) => one,
            Ok(n) if (2..=4).contains(&(n % 10)) && !(12..=14).contains(&(n % 100)) => few,
            Ok(_) => many,
        }
        .to_owned()
    };
    // "20 thousand"; in Polish by the amount, "2 tysiące", "5 milionów".
    let magnitude = |m: &str, amount: &str| -> String {
        let (en, pl) = match m {
            "k" | "K" | "thousand" => ("thousand", ["tysiąc", "tysiące", "tysięcy", "tysiąca"]),
            "m" | "M" | "million" => ("million", ["milion", "miliony", "milionów", "miliona"]),
            "b" | "B" | "bn" | "billion" => ("billion", ["miliard", "miliardy", "miliardów", "miliarda"]),
            _ => return String::new(),
        };
        if polish { format!(" {}", counted(amount, false, "", pl)) } else { format!(" {en}") }
    };
    let money = |symbol: &str, amount: &str, magnitude: bool| match symbol {
        "$" => counted(amount, magnitude, "dollar", ["dolar", "dolary", "dolarów", "dolara"]),
        "€" => counted(amount, magnitude, "euro", ["euro", "euro", "euro", "euro"]),
        _ => counted(amount, magnitude, "pound", ["funt", "funty", "funtów", "funta"]),
    };
    // "2,500,000" is one number; "0,125", "1,234,5" and Polish commas are not thousands.
    let text = if polish {
        text.to_owned()
    } else {
        separator
            .replace_all(text, |c: &Captures| {
                let end = c.get(0).unwrap().end();
                let decimal = text[end..].starts_with(',') && text[end + 1..].starts_with(|ch: char| ch.is_ascii_digit());
                if decimal { c[0].to_owned() } else { c[0].replace(',', "") }
            })
            .into_owned()
    };
    let text = ranges(&text, polish);
    let text = currency.replace_all(&text, |c: &Captures| {
        let scale = c.get(3).map_or("", |m| m.as_str());
        format!("{}{} {}", &c[2], magnitude(scale, &c[2]), money(&c[1], &c[2], !scale.is_empty()))
    });
    let text = zloty.replace_all(&text, |c: &Captures| {
        let scale = c.get(2).map_or("", |m| m.as_str());
        let unit = if polish { counted(&c[1], !scale.is_empty(), "", ["złoty", "złote", "złotych", "złotego"]) } else { "złoty".to_owned() };
        format!("{}{} {unit}", &c[1], magnitude(scale, &c[1]))
    });
    let text = after.replace_all(&text, |c: &Captures| format!("{} {}", &c[1], money(&c[2], &c[1], false)));
    let text = shared_unit(&text);
    // "4K" and "5k" (a screen, a run) are said as written.
    let text = suffix.replace_all(&text, |c: &Captures| {
        if c[2].eq_ignore_ascii_case("k") && c[1].len() == 1 { c[0].to_owned() } else { format!("{}{}", &c[1], magnitude(&c[2], &c[1])) }
    });
    let text = percent.replace_all(&text, if polish { "$1 procent" } else { "$1 percent" });
    // "0x1F" is hexadecimal, never "0 by 1F".
    let source = text.into_owned();
    let text = dimension.replace_all(&source, |c: &Captures| {
        let start = c.get(0).unwrap().start();
        let hex = &c[1] == "0" && !source[..start].ends_with(|ch: char| ch.is_alphanumeric());
        if hex { c[0].to_owned() } else { format!("{} {} {}", &c[1], if polish { "na" } else { "by" }, &c[2]) }
    });
    let text = times.replace_all(&text, if polish { "$1 razy" } else { "$1 times" });
    let text = degrees.replace_all(&text, |c: &Captures| {
        let scale = match (c.get(2).map(|m| m.as_str()), polish) {
            (Some("C"), false) => " Celsius",
            (Some("F"), false) => " Fahrenheit",
            (Some("C"), true) => " Celsjusza",
            (Some("F"), true) => " Fahrenheita",
            _ => "",
        };
        format!("{} {}{scale}", &c[1], counted(&c[1], false, "degree", ["stopień", "stopnie", "stopni", "stopnia"]))
    });
    let text = number.replace_all(&text, if polish { "numer $1" } else { "number $1" });
    // "6 ch" is "6 channel": Jarvis read "ch" as "change", "chimps" or "6gh" (25-26 Oct, 12 takes rejected).
    static CHANNEL: OnceLock<Regex> = OnceLock::new();
    let channel = CHANNEL.get_or_init(|| Regex::new(r"\b(\d+)\s?(?:ch|CH|Ch)\b").unwrap());
    let text = if polish { text } else { std::borrow::Cow::Owned(channel.replace_all(&text, "$1 channel").into_owned()) };
    let and = if polish { " i " } else { " and " };
    text.replace('&', and).replace('+', " plus ").replace('@', if polish { " małpa " } else { " at " })
}
/// Ranges and on-the-hour times as they are said (`spell_symbols`, before the currency rule). A dash between amounts
/// is "to" ("$100–$300" was stripped to "100 dollars 300 dollars": no "to" was said), and a currency shared by both ends
/// of a range or pair is said once, after the second ("100 to 300 dollars"; 4 Nov 02:15, the word check rejected three
/// correct takes of "a hundred to three hundred dollars" heard "$100 to $300" and the sentence went to the lighter
/// voice). An English time on the hour is said as a time ("15:00" was "15 00", heard "15,000" on all three takes, 3 Nov
/// 03:28): "3 PM", "9 o'clock", "noon", "midnight"; other times keep their digits ("15 30").
fn ranges(text: &str, polish: bool) -> String {
    use regex::{Captures, Regex};
    use std::sync::OnceLock;
    static RULES: OnceLock<[Regex; 2]> = OnceLock::new();
    let [dash, hour] = RULES.get_or_init(|| {
        [
            // An unspaced en or em dash after an amount, a spaced en dash (a spaced em dash is an aside: "2024 — 3 years
            // ago") (with its %, scale or currency), or a hyphen straight before a currency.
            r"(\d(?:\s?(?:%|[kKmM]|bn|thousand|million|billion)\b|%)?)(?:[–—]|\s–\s|-)([$€£]?\d)",
            r"\b((?i:at|by|from|until|till|to|before|after|around|about|since|the)\s+)([01]?\d|2[0-3]):([0-5]\d)\b(\s*(?:[AaPp]\.?[Mm]\b\.?|:\d))?",
        ]
        .map(|r| Regex::new(r).unwrap())
    });
    let to = if polish { "do" } else { "to" };
    let text = dash.replace_all(text, |c: &Captures| {
        // A plain hyphen only before a currency ("$5-$10"); "Qwen3-8B" and dates keep theirs.
        let hyphen = !c[0].contains(['–', '—']);
        if hyphen && !c[2].starts_with(['$', '€', '£']) && !c[1].ends_with('%') { c[0].to_owned() } else { format!("{} {to} {}", &c[1], &c[2]) }
    });
    if polish {
        return text.into_owned();
    }
    // Only a time of day after its preposition or "the" ("at 15:00", "the 15:00 session"), never a duration ("took 2:00"), "12:00 AM" or "1:00:00".
    hour.replace_all(&text, |c: &Captures| {
        let h: u32 = c[2].parse().unwrap_or(0);
        let time = match (h, &c[3]) {
            _ if c.get(4).is_some() => return c[0].to_owned(),
            (0, "00") => "midnight".to_owned(),
            (12, "00") => "noon".to_owned(),
            (13..=23, "00") => format!("{} PM", h - 12),
            (_, "00") => format!("{h} o'clock"),
            _ => format!("{} {}", &c[2], &c[3]),
        };
        format!("{}{time}", &c[1])
    })
    .into_owned()
}
/// A currency said once for a range, after its second amount: "100 dollars to 300 dollars" is "100 to 300 dollars"
/// (`ranges`; Polish "do" alike when both units are the same word).
fn shared_unit(text: &str) -> String {
    use regex::{Captures, Regex};
    use std::sync::OnceLock;
    static RULE: OnceLock<Regex> = OnceLock::new();
    let amount = r"\d+(?:[.,]\d+)?(?: (?:thousand|million|billion|tysiąc|tysiące|tysięcy|miliony|milionów|miliardy|miliardów))?";
    let unit = r"dollars?|euros?|pounds?|dolar(?:y|ów|a)?|funt(?:y|ów|a)?";
    let rule = RULE.get_or_init(|| Regex::new(&format!(r"({amount}) ({unit}) (to|do) ({amount}) ({unit})\b")).unwrap());
    rule.replace_all(text, |c: &Captures| {
        let stem = |u: &str| u.chars().take(4).collect::<String>();
        if stem(&c[2]) == stem(&c[5]) { format!("{} {} {} {}", &c[1], &c[3], &c[4], &c[5]) } else { c[0].to_owned() }
    })
    .into_owned()
}
/// Applied to both voices before synthesis. The Python worker repeats this at
/// its boundary so direct/offline callers obey the same text contract.
pub fn normalize(text: &str) -> String {
    static TAGS: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let tags = TAGS.get_or_init(|| regex::Regex::new(r"\[[^\]]*\]|<[^>]*>").unwrap());
    let text = text.replace('’', "'");
    let stripped = tags.replace_all(&text, " ");
    let stripped = spell_symbols(&stripped);
    let mapped: String = stripped
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c.is_whitespace() || ".?!'".contains(c) {
                c
            } else {
                ' '
            }
        })
        .collect();
    let mut out = String::new();
    let mut space = false;
    for c in mapped.chars() {
        if c.is_whitespace() {
            space = !out.is_empty();
            continue;
        }
        if space && !".?!".contains(c) {
            out.push(' ');
        }
        out.push(c);
        space = false;
    }
    out
}
/// A delivery direction the worker accepts (`text.py` `direction`): a letter first, then letters, digits, spaces,
/// commas and hyphens, at most 60 characters. No brackets, so it can never close its own cue or open markup.
pub fn valid_direction(direction: &str) -> bool {
    let mut chars = direction.chars();
    chars.next().is_some_and(char::is_alphabetic)
        && direction.chars().count() <= 60
        && chars.all(|c| c.is_alphanumeric() || " ,-".contains(c))
}
/// Sentence boundaries are shared with the worker's punctuation-only contract (`text.py`). A decimal point between
/// digits ("Opus 5.5") is not a boundary: on 19 Oct "Opus 5.5 in my window" was said as "Opus 5." and "5 in my
/// window." and the first half failed its word check. Nor is a dot between a letter or digit and a letter ("X.com",
/// "a.m."): on 25 Oct "from X.com counts" was said as "from X." and "com counts", and both halves failed.
pub fn sentences(text: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut part = String::new();
    let mut ended = false;
    let chars: Vec<char> = text.chars().collect();
    for (k, &c) in chars.iter().enumerate() {
        if ended && !".?!".contains(c) {
            if !part.trim().is_empty() {
                parts.push(part.trim().to_owned());
            }
            part.clear();
            ended = false;
        }
        part.push(c);
        let decimal = c == '.'
            && k > 0
            && chars[k - 1].is_ascii_digit()
            && chars.get(k + 1).is_some_and(char::is_ascii_digit);
        let inside = c == '.'
            && k > 0
            && chars[k - 1].is_alphanumeric()
            && chars.get(k + 1).is_some_and(|n| n.is_alphabetic());
        if ".?!".contains(c) && !decimal && !inside {
            ended = true;
        }
    }
    if !part.trim().is_empty() {
        parts.push(part.trim().to_owned());
    }
    parts
}
#[derive(Debug)]
pub struct SpeechFailure {
    pub remaining: String,
    pub played_text: String,
    pub played: Audio,
    /// The unplayed sentences, index for index: what a voice says (plain) and what the island shows (as written).
    pub remaining_parts: Vec<String>,
    pub remaining_shown: Vec<String>,
    /// Jarvis is still loaded and gave up only on the first remaining sentence, after every retry.
    pub after_retries: bool,
    /// After every retry Jarvis said his best rejected take of a sentence (`coverage::keepable`) instead of switching
    /// voices; the remaining sentences are his to go on with.
    pub kept_take: bool,
    reason: String,
}
impl std::fmt::Display for SpeechFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.reason)
    }
}
impl std::error::Error for SpeechFailure {}
/// Complete the fallback frontier: a failed live first sentence restarts in full;
/// accepted later sentences remain excluded. Tests exercise this exact routing.
pub fn finish_unplayed(
    failure: SpeechFailure,
    fallback: &mut dyn crate::Synthesizer,
    language: &str,
    cancel: &dyn Fn() -> bool,
    emit: &mut dyn FnMut(&Audio) -> Result<()>,
) -> Result<Audio> {
    finish_unplayed_announced(failure, fallback, language, cancel, &mut |_| Ok(()), emit)
}
/// `finish_unplayed`, announcing each sentence before its audio (the island's spoken text).
pub fn finish_unplayed_announced(
    mut failure: SpeechFailure,
    fallback: &mut dyn crate::Synthesizer,
    language: &str,
    cancel: &dyn Fn() -> bool,
    announce: &mut dyn FnMut(&str) -> Result<()>,
    emit: &mut dyn FnMut(&Audio) -> Result<()>,
) -> Result<Audio> {
    for sentence in sentences(&failure.remaining) {
        ensure!(!cancel(), "speech interrupted");
        let audio = fallback.synthesize(&sentence, language, cancel)?;
        ensure!(!cancel(), "speech interrupted");
        announce(&sentence)?;
        emit(&audio)?;
        if failure.played.samples.is_empty() {
            failure.played.sample_rate = audio.sample_rate;
            failure.played.first_audio_ms = audio.first_audio_ms;
        }
        ensure!(
            failure.played.sample_rate == audio.sample_rate,
            "fallback audio rate changed"
        );
        failure.played.samples.extend_from_slice(&audio.samples);
        failure.played.synthesis_ms += audio.synthesis_ms;
    }
    Ok(failure.played)
}

/// Attempts per sentence before Jarvis says his best take or the lighter voice takes over (the worker's `ATTEMPTS`).
pub const ATTEMPTS: u64 = 3;
/// The island's notice for each way a line leaves Jarvis's voice (`voice_switch` events carry it as `notice`).
pub const NOTICE_MISSED: &str = "Jarvis missed a sentence · lighter voice";
pub const NOTICE_STOPPED: &str = "Jarvis stopped · lighter voice";
pub const NOTICE_NOT_LOADED: &str = "Jarvis not loaded · lighter voice";
/// Jarvis gave way to free memory (`memory_line`): shown once per hold; Phonon S says the lines meanwhile.
pub const NOTICE_MEMORY: &str = "Jarvis off for memory · Phonon S";
/// The menu's note while he is held out for memory.
pub const NOTE_MEMORY: &str = "Jarvis off to free memory; Phonon S speaks until memory is back.";
/// Kills after an interruption, in five minutes, that reload at once without counting as failures (`interrupted`).
const INTERRUPTED_RESTARTS: usize = 2;
/// A failed load is tried once more after this pause before the lighter voice takes over (26 Oct 01:38: one
/// sidecar load failure sent Jarvis's lines to the lighter voice for the 30 s back-off).
const LOAD_RETRY_PAUSE: Duration = Duration::from_millis(500);
/// A load stopped because the memory line holds Jarvis out or he is giving way (review 4 Nov: a give-way during a
/// load let it run to the end, ~27 GB for up to 45 s, over the line). Never retried, never a strike or failure.
pub const LOAD_CANCELLED_MEMORY: &str = "load cancelled for memory";

/// The cause class of a switch to the lighter voice, from its reason: the `cause=` the log's `voice: fallback` line
/// carries, so the nightly pass can group every switch by cause (docs/nightly-build.md).
pub fn fallback_cause(reason: &str) -> &'static str {
    let r = reason.to_lowercase();
    let has = |words: &[&str]| words.iter().any(|w| r.contains(w));
    if has(&["too many open files", "os error 24"]) {
        "fd_exhausted"
    } else if has(&["memory", "swap", "exit 73", "pressure"]) {
        "memory"
    } else if has(&["deadline", "timed out", "timeout", " stalled", "stall:"]) {
        "timeout"
    } else if has(&["word coverage", "incomplete sentence", "cut or repeated", "word check"]) || (has(&["after "]) && has(&[" takes"])) {
        "word_check"
    } else if has(&["sidecar load", "runtime missing", "load failed"]) {
        "load_failed"
    } else if has(&["sidecar exited", "sending half is closed", "broken pipe", "os error 32", "invalid sidecar protocol", "exit status"]) {
        "sidecar_crash"
    } else if has(&["paused after repeated failures", "unavailable"]) {
        "load_failed"
    } else if has(&["not loaded", "loading", "unloaded"]) {
        "not_loaded"
    } else {
        "other"
    }
}
/// A load failure worth one more try: not a memory refusal (it waits for memory), a cancelled load, a missing runtime,
/// or a load that already spent its 45 s deadline.
fn retryable_load(reason: &str) -> bool {
    let r = reason.to_lowercase();
    !memory_reason(reason) && !["load cancelled", "runtime missing", "deadline"].iter().any(|w| r.contains(w))
}

/// A memory refusal or unload: its words, or exit 73 itself (`record_exit`'s "Memory unload (exit 73)", an exit
/// status, `settle`). Never a bare "73", which a worker's own line can carry in any number (review 1 Nov: a fatal
/// line with "load_s":1.73 became an endless memory wait).
fn memory_reason(reason: &str) -> bool {
    let r = reason.to_lowercase();
    ["memory", "swap", "pressure", "exit 73", "exit status: 73", "gb available"].iter().any(|w| r.contains(w))
}

/// The two voices of a line. Jarvis streams written sentences through the caller's playback (the app stretches his
/// PCM to the user's speed on the way); the lighter voice says one plain sentence.
pub trait Voices {
    /// Jarvis says `written` (`Service::stream`, or `stream_resumed` when `resumed` after other sentences of the
    /// line), packets through `emit`; also whether playback refused a packet.
    fn jarvis(&mut self, service: &Service, written: &str, resumed: bool, cancel: &dyn Fn() -> bool,
        emit: &mut dyn FnMut(crate::SpeechPacket) -> Result<()>) -> (Result<Audio>, bool);
    /// The lighter voice says one plain sentence.
    fn lighter(&mut self, sentence: &str, cancel: &dyn Fn() -> bool) -> Result<Audio>;
    /// Audio Jarvis said, and its plain text (the app keeps it as training data).
    fn jarvis_said(&mut self, _text: &str, _audio: &Audio) {}
}

/// Says a line in Jarvis's voice. A sentence whose takes the word check rejected is still his when his best take is
/// only misheard (`coverage::keepable`), and he goes on. A sentence Jarvis truly cannot say (every take cut, repeated
/// or stalled) is said by the lighter voice alone, then Jarvis goes on with the rest of the line (and
/// 24 Oct 03:29: "Says one sentence"). The lighter voice says the rest only when Jarvis stops mid-line or is not
/// loaded. Every switch is a `voice_switch` event with the island's notice and reason: never a silent change.
pub fn speak_line(
    service: &Service,
    voices: &mut dyn Voices,
    written: &str,
    cancel: &dyn Fn() -> bool,
    emit: &mut dyn FnMut(crate::SpeechPacket) -> Result<()>,
) -> Result<Audio> {
    fn add(all: &mut Audio, audio: &Audio) -> Result<()> {
        if all.samples.is_empty() {
            all.sample_rate = audio.sample_rate;
            all.first_audio_ms = audio.first_audio_ms;
        }
        ensure!(all.sample_rate == audio.sample_rate, "voice sample rate changed");
        all.samples.extend_from_slice(&audio.samples);
        all.synthesis_ms += audio.synthesis_ms;
        Ok(())
    }
    let _speaking = service.speaking();
    let mut all = Audio { samples: vec![], sample_rate: 44100, first_audio_ms: 0., synthesis_ms: 0. };
    let mut plain = sentences(&normalize(written));
    let mut shown = display_sentences(written);
    ensure!(plain.len() == shown.len(), "sentence split mismatch");
    let mut resumed = false;
    while !plain.is_empty() {
        ensure!(!cancel(), "speech interrupted");
        let resuming = std::mem::replace(&mut resumed, true);
        let lighter = if !service.ready() {
            service.lighter_voice(&plain.join(" "), NOTICE_NOT_LOADED, &service.status().unwrap_or_else(|| "not loaded".into()));
            plain.len()
        } else {
            // After other sentences of this line, his first sentence is made whole before it plays (`buffered`): a
            // cut live first sentence restarts playback, which drops every unplayed packet of the line with it
            // (review 24 Oct). Its chunks still reach playback one at a time between the worker's messages.
            let (result, refused) = voices.jarvis(service, &shown.join(" "), resuming, cancel, emit);
            match result {
                Ok(audio) => {
                    voices.jarvis_said(&plain.join(" "), &audio);
                    add(&mut all, &audio)?;
                    return Ok(all);
                }
                Err(error) if refused || cancel() => return Err(error),
                Err(error) => match error.downcast::<SpeechFailure>() {
                    Ok(failure) => {
                        if !failure.played.samples.is_empty() {
                            voices.jarvis_said(&failure.played_text, &failure.played);
                            add(&mut all, &failure.played)?;
                        }
                        ensure!(failure.remaining_parts.len() == failure.remaining_shown.len(), "sentence split mismatch");
                        plain = failure.remaining_parts;
                        shown = failure.remaining_shown;
                        // A kept take: Jarvis goes on. After every retry the lighter voice says that one sentence and
                        // Jarvis goes on (if he unloaded meanwhile, the next turn logs its own switch for the rest).
                        // A stopped Jarvis leaves it the rest. The request logged the switch either way.
                        if failure.kept_take { 0 } else if failure.after_retries { 1.min(plain.len()) } else { plain.len() }
                    }
                    // Refused before any audio: unloaded between the check and the request.
                    Err(error) => {
                        service.lighter_voice(&plain.join(" "), NOTICE_NOT_LOADED, &format!("{error:#}"));
                        plain.len()
                    }
                },
            }
        };
        for (sentence, written) in plain.drain(..lighter).zip(shown.drain(..lighter)) {
            ensure!(!cancel(), "speech interrupted");
            let audio = voices.lighter(&sentence, cancel)?;
            ensure!(!cancel(), "speech interrupted");
            emit(crate::SpeechPacket::Sentence(written))?;
            emit(crate::SpeechPacket::Audio(audio.clone()))?;
            add(&mut all, &audio)?;
        }
    }
    Ok(all)
}
pub fn settings() -> Value {
    json!({"snapshot":"rl100-78e5ab79e49aa9a25abe8799eff2c4d73a1c3216","voice":VOICE,"runtime":"exact-final","retry":ATTEMPTS-1})
}

/// The fatal or refusal line an exited worker wrote before leaving, if it reached the pipe. The reader thread may
/// deliver it a moment after the exit is seen, so the first read waits briefly.
fn last_words(output: &mpsc::Receiver<Value>) -> Option<Value> {
    let mut said = None;
    let mut next = output.recv_timeout(Duration::from_millis(20)).ok();
    while let Some(v) = next {
        if ["fatal", "memory_refusal"].contains(&v["event"].as_str().unwrap_or("")) {
            said = Some(v);
        }
        next = output.try_recv().ok();
    }
    said
}
struct Process {
    pid: Arc<AtomicI32>,
    child: Child,
    input: ChildStdin,
    output: mpsc::Receiver<Value>,
}
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.pid.compare_exchange(
            self.child.id() as i32,
            0,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        let _ = self.child.kill();
        // Reaping a worker that held a ~31 GB model takes seconds (24 Oct 23:13: Speaking off froze the live loop
        // 2.2 s). An exited child was already reaped; a live one is reaped off this thread.
        if let Ok(None) = self.child.try_wait() {
            reap_detached(self.child.id() as libc::pid_t);
        }
    }
}
/// Waits for the killed child `pid` on a thread of its own, so no zombie is left and the caller never waits.
fn reap_detached(pid: libc::pid_t) {
    let reap = move || unsafe {
        let mut status = 0;
        while libc::waitpid(pid, &mut status, 0) == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {}
    };
    if std::thread::Builder::new().name("drama3-reaper".into()).spawn(reap).is_err() {
        reap();
    }
}

/// A line being spoken (`Service::speaking`): Jarvis returns from a memory hold only between lines.
pub struct Speaking<'a>(&'a Service);
impl Drop for Speaking<'_> {
    fn drop(&mut self) {
        *self.0.last_spoke.lock().unwrap() = Some(Instant::now());
        self.0.speaking.fetch_sub(1, Ordering::AcqRel);
    }
}

pub struct Service {
    assets: PathBuf,
    offline_agent_settings: Option<PathBuf>,
    verifier: Mutex<Option<Arc<SentenceVerifier>>>,
    residency_hook: Mutex<Option<Arc<dyn Fn(bool) + Send + Sync>>>,
    verifier_reserve: Mutex<Option<Arc<dyn Fn() -> u64 + Send + Sync>>>,
    process: Mutex<Option<Process>>,
    ready: AtomicBool,
    starting: AtomicBool,
    wanted: AtomicBool,
    pid: Arc<AtomicI32>,
    ids: AtomicU64,
    failures: Mutex<VecDeque<Instant>>,
    interruptions: Mutex<VecDeque<Instant>>,
    memory_wait: Mutex<Option<Instant>>,
    memory_note: Mutex<Option<String>>,
    events: Mutex<VecDeque<Value>>,
    note: Mutex<Option<String>>,
    /// Why Jarvis last failed to load or stopped: a later "not loaded" switch carries it as its error.
    last_failure: Mutex<Option<String>>,
    /// The Rust engine failed (its own failure at load, or two strikes): the Python worker serves for the rest of
    /// this app run.
    rust_failed: AtomicBool,
    /// Strikes against the Rust engine in the last `STRIKE_WINDOW`: a load that timed out or could not spawn, a crash
    /// while serving.
    rust_strikes: Mutex<VecDeque<Instant>>,
    /// The worker now resident is the Rust engine.
    rust_serving: AtomicBool,
    /// The user's voice speed in percent (the app's `voice_speed`), sent as each request's `rate`: the Rust scheduler
    /// counts its lead in listening time.
    pace: Mutex<Option<Arc<std::sync::atomic::AtomicU16>>>,
    /// The memory line (`memory_line`, bug note 2026-11-03T22-38-44): None in tests and tooling.
    memory_gate: Mutex<Option<crate::memory_line::Gate>>,
    /// Jarvis is giving way: no new line takes him; the line in flight ends, then the tick unloads him.
    giving_way: AtomicBool,
    /// This reload is his return after a memory hold: lines meanwhile do not wait for him (Phonon S says them).
    returning: AtomicBool,
    /// The memory line said he may return; the next load that starts is that return.
    return_pending: AtomicBool,
    /// The memory notice was shown for this hold (the user hears of the switch once).
    memory_announced: AtomicBool,
    /// Lines being spoken now (`speak_line`), and when the last one ended: he reloads only between replies.
    speaking: AtomicUsize,
    last_spoke: Mutex<Option<Instant>>,
}
impl Service {
    pub fn new(assets: PathBuf) -> Arc<Self> {
        Self::new_configured(assets, None)
    }
    /// Offline tooling additionally verifies Agent-off and Splash idle. Production
    /// and offline admission both use current available memory and WARN unload.
    pub fn new_offline_current(assets: PathBuf, agent_settings: PathBuf) -> Arc<Self> {
        Self::new_configured(assets, Some(agent_settings))
    }
    fn new_configured(assets: PathBuf, offline_agent_settings: Option<PathBuf>) -> Arc<Self> {
        Arc::new(Self {
            assets,
            offline_agent_settings,
            verifier: Mutex::new(None),
            residency_hook: Mutex::new(None),
            verifier_reserve: Mutex::new(None),
            process: Mutex::new(None),
            ready: AtomicBool::new(false),
            starting: AtomicBool::new(false),
            wanted: AtomicBool::new(false),
            pid: Arc::new(AtomicI32::new(0)),
            ids: AtomicU64::new(0),
            failures: Mutex::new(VecDeque::new()),
            interruptions: Mutex::new(VecDeque::new()),
            memory_wait: Mutex::new(None),
            memory_note: Mutex::new(None),
            events: Mutex::new(VecDeque::new()),
            note: Mutex::new(None),
            last_failure: Mutex::new(None),
            rust_failed: AtomicBool::new(false),
            rust_strikes: Mutex::new(VecDeque::new()),
            rust_serving: AtomicBool::new(false),
            pace: Mutex::new(None),
            memory_gate: Mutex::new(None),
            giving_way: AtomicBool::new(false),
            returning: AtomicBool::new(false),
            return_pending: AtomicBool::new(false),
            memory_announced: AtomicBool::new(false),
            speaking: AtomicUsize::new(0),
            last_spoke: Mutex::new(None),
        })
    }
    /// The playback speed (percent) each request carries as `rate`.
    pub fn set_pace(&self, percent: Arc<std::sync::atomic::AtomicU16>) {
        *self.pace.lock().unwrap() = Some(percent);
    }
    pub fn set_verifier(&self, verifier: Option<Arc<SentenceVerifier>>) {
        *self.verifier.lock().unwrap() = verifier;
    }
    pub fn set_verifier_reserve(&self, reserve: Arc<dyn Fn() -> u64 + Send + Sync>) {
        *self.verifier_reserve.lock().unwrap() = Some(reserve);
    }
    pub fn record_verifier_unload(&self, retained_bytes: u64) {
        self.event(json!({"event":"verifier_unload_pending","retained_bytes":retained_bytes,"reason":"native verification thread still alive"}));
    }
    pub fn set_residency_hook(&self, hook: Arc<dyn Fn(bool) + Send + Sync>) {
        let mut current = self.residency_hook.lock().unwrap();
        *current = Some(hook);
        current.as_ref().unwrap()(self.ready());
    }
    fn notify_residency(&self, resident: bool) {
        // Serialize notifications with Stop, and re-read the actual state while
        // holding this control lock. A stale ready notification cannot reload ASR.
        if let Some(hook) = self.residency_hook.lock().unwrap().as_ref() {
            hook(resident && self.ready());
        }
    }
    pub fn ready(&self) -> bool {
        self.ready.load(Ordering::Acquire) && self.wanted.load(Ordering::Acquire) && !self.giving_way.load(Ordering::Acquire)
    }
    /// The lighter voice says `text` while Jarvis is wanted: logged as a `voice_switch` event, which the island
    /// shows as `notice`. Nothing is recorded while Jarvis is not wanted (Speaking off, replay, tests).
    pub fn lighter_voice(&self, text: &str, notice: &str, reason: &str) {
        if !self.wanted.load(Ordering::Acquire) {
            return;
        }
        // Held out for memory: the island hears of it once per hold; later lines are logged only.
        if notice == NOTICE_NOT_LOADED {
            if let Some(why) = self.memory_held() {
                if self.memory_announced.swap(true, Ordering::AcqRel) {
                    self.event(json!({"event":"memory_line","text":text,"reason":why}));
                } else {
                    self.event(json!({"event":"voice_switch","text":text,"notice":NOTICE_MEMORY,"reason":why,"cause":"memory","from":"jarvis","to":"phonon","error":why}));
                }
                return;
            }
        }
        self.event(json!({"event":"voice_switch","text":text,"notice":notice,"reason":reason}));
    }
    /// The memory line's gate (the app's: native readings and its needs folder).
    pub fn set_memory_gate(&self, gate: crate::memory_line::Gate) {
        *self.memory_gate.lock().unwrap() = Some(gate);
    }
    /// Why Jarvis is held out for memory now (giving way, held, or reloading after a hold), if he is.
    pub fn memory_held(&self) -> Option<String> {
        let held = self.memory_gate.lock().unwrap().as_ref().and_then(|g| g.policy.held().map(str::to_owned));
        if held.is_some() {
            return held;
        }
        let returning = self.returning.load(Ordering::Acquire) && !self.ready.load(Ordering::Acquire);
        (self.giving_way.load(Ordering::Acquire) || returning).then(|| NOTE_MEMORY.to_owned())
    }
    /// A line is being spoken while the guard lives (`speak_line`).
    pub fn speaking(&self) -> Speaking<'_> {
        self.speaking.fetch_add(1, Ordering::AcqRel);
        Speaking(self)
    }
    /// How long nothing has been spoken; None while a line is.
    fn quiet_for(&self) -> Option<Duration> {
        if self.speaking.load(Ordering::Acquire) > 0 || self.process.try_lock().is_err() {
            return None;
        }
        Some(self.last_spoke.lock().unwrap().map_or(Duration::MAX, |at| at.elapsed()))
    }
    /// The memory line's tick (bug note 2026-11-03T22-38-44): over the 95 GB line, or at WARN/CRITICAL, Jarvis gives
    /// way and Phonon S speaks; once memory is back with a margin and has stayed back, he reloads between replies.
    fn memory_tick(&self) {
        let action = {
            let mut gate = self.memory_gate.lock().unwrap();
            let Some(gate) = gate.as_mut() else { return };
            // His own load counts as resident: the memory it takes is his, never a reason to hold him (review 4 Nov:
            // judged as not loaded, his rising use failed the room check partway and he looped load, give way, return).
            let resident = self.ready.load(Ordering::Acquire) || self.starting.load(Ordering::Acquire);
            let now = gate.now();
            let action = gate.tick(resident, self.quiet_for(), now);
            let sample = gate.last().cloned();
            (action, sample)
        };
        match action {
            (crate::memory_line::Action::GiveWay(reason), sample) => self.give_way(&reason, sample.as_ref()),
            (crate::memory_line::Action::Return, sample) => {
                self.giving_way.store(false, Ordering::Release);
                self.memory_wait.lock().unwrap().take();
                self.memory_note.lock().unwrap().take();
                // The memory notice is shown again only after he is actually back (`start_with`): a return the
                // sidecar refuses, or one that gives way again, is the same hold to the user.
                self.return_pending.store(true, Ordering::Release);
                let (used, available) = sample.as_ref().map_or((0, None), |s| (s.used, s.available));
                let (jarvis, hold_s) = self.memory_gate.lock().unwrap().as_ref()
                    .map_or((crate::memory_line::JARVIS_BYTES, 0.), |g| (g.policy.jarvis_bytes(), g.policy.hold_for().as_secs_f64()));
                self.event(json!({"event":"memory_return","used_bytes":used,"available_bytes":available,
                    "jarvis_bytes":jarvis,"hold_s":hold_s,"line_bytes":crate::memory_line::LINE}));
            }
            // Speaking off and on again clears the note; the hold it describes is still there.
            (crate::memory_line::Action::Hold, _) => {
                if self.status().as_deref() != Some(NOTE_MEMORY) {
                    self.note(Some(NOTE_MEMORY));
                }
            }
            (crate::memory_line::Action::Stay, _) => {}
        }
    }
    /// New lines stop taking Jarvis at once; he is unloaded as soon as no line holds him (a line in flight ends in
    /// his voice, never cut mid-sentence).
    fn give_way(&self, reason: &str, sample: Option<&crate::memory_line::Sample>) {
        if !self.giving_way.swap(true, Ordering::AcqRel) {
            let (used, need, why) = sample.map_or((0, 0, vec![]), |s| (s.used, s.need, s.why.clone()));
            let hold_s = self.memory_gate.lock().unwrap().as_ref().map_or(0., |g| g.policy.hold_for().as_secs_f64());
            self.event(json!({"event":"memory_give_way","reason":reason,"used_bytes":used,"need_bytes":need,"needed_by":why,
                "hold_s":hold_s,"line_bytes":crate::memory_line::LINE}));
            self.note(Some(NOTE_MEMORY));
        }
        // A load holds the worker: it sees `giving_way` within 20 ms, kills its half-loaded worker and finishes the
        // give-way on its own thread (`cancelled_for_memory`).
        let Ok(mut state) = self.process.try_lock() else { return };
        self.ready.store(false, Ordering::Release);
        self.pid.store(0, Ordering::Release);
        // Drop kills the worker and reaps it on its own thread.
        state.take();
        drop(state);
        self.notify_residency(false);
        self.giving_way.store(false, Ordering::Release);
        *self.last_failure.lock().unwrap() = Some(reason.to_owned());
        *self.memory_wait.lock().unwrap() = Some(Instant::now());
        self.note(Some(NOTE_MEMORY));
        self.event(json!({"event":"memory_unload","phase":"give_way","reason":reason}));
    }
    /// The memory line holds Jarvis out, or he is giving way: no load starts, and one under way stops.
    fn memory_blocked(&self) -> bool {
        self.giving_way.load(Ordering::Acquire)
            || self.memory_gate.lock().unwrap().as_ref().is_some_and(|g| g.policy.held().is_some())
    }
    /// A load stopped for memory (`LOAD_CANCELLED_MEMORY`): its worker is already killed; the give-way is done.
    fn cancelled_for_memory(&self) {
        self.ready.store(false, Ordering::Release);
        self.notify_residency(false);
        self.giving_way.store(false, Ordering::Release);
        let why = self.memory_held().filter(|w| w.as_str() != NOTE_MEMORY).unwrap_or_else(|| LOAD_CANCELLED_MEMORY.to_owned());
        *self.last_failure.lock().unwrap() = Some(why.clone());
        *self.memory_wait.lock().unwrap() = Some(Instant::now());
        self.note(Some(NOTE_MEMORY));
        self.event(json!({"event":"memory_unload","phase":"give_way","cancelled":"load","reason":why}));
    }
    /// The app's residency tick wants Jarvis resident (Speaking on with Jarvis chosen).
    pub fn wanted(&self) -> bool {
        self.wanted.load(Ordering::Acquire)
    }
    /// The sidecar is starting for a current request to be resident.
    pub fn loading(&self) -> bool {
        // His return after a memory hold never makes a line wait: Phonon S says it while he loads.
        // Nor one he is giving way during: that load ends in an unload.
        self.starting.load(Ordering::Acquire) && self.wanted.load(Ordering::Acquire) && !self.ready()
            && !self.returning.load(Ordering::Acquire) && self.memory_held().is_none()
    }
    /// A line that arrives while Jarvis is still loading waits for him (5.1 s spawn to ready on a warm Mac,
    /// measured 18 Oct) rather than switching to the lighter voice; a failed or cancelled load returns at once.
    pub fn wait_ready(&self, limit: Duration, cancel: &dyn Fn() -> bool) -> bool {
        let until = Instant::now() + limit;
        while self.loading() && !cancel() && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(20));
        }
        self.ready()
    }
    pub fn status(&self) -> Option<String> {
        self.note.lock().unwrap().clone()
    }
    pub fn events(&self) -> Vec<Value> {
        self.events.lock().unwrap().drain(..).collect()
    }
    /// Loading and memory policy are ordinary residency states, never failures.
    pub fn policy_waiting(&self) -> bool {
        self.starting.load(Ordering::Acquire) || self.memory_wait.lock().unwrap().is_some() || self.memory_held().is_some()
    }
    fn event(&self, mut value: Value) {
        if value["event"] == "voice_switch" && value["cause"].is_null() {
            // One structured fallback: its cause class, the voices, and the error behind it. A "not loaded" switch
            // only knows the status note, so it carries the failure that left Jarvis down.
            let reason = value["reason"].as_str().unwrap_or_default().to_owned();
            let failure = self.last_failure.lock().unwrap().clone();
            let error = match (fallback_cause(&reason), failure) {
                ("not_loaded" | "load_failed", Some(failure)) if value["notice"] == NOTICE_NOT_LOADED => failure,
                _ => reason,
            };
            value["cause"] = fallback_cause(&error).into();
            value["from"] = "jarvis".into();
            value["to"] = "lighter".into();
            value["error"] = error.into();
        }
        if (value["event"] == "memory_refusal" || value["event"] == "fatal")
            && value["reason"].as_str().is_some_and(|reason| reason.starts_with("Not enough free memory")) {
            if let (Some(required),Some(free)) = (value["required_available_bytes"].as_u64(),value["available_bytes"].as_u64()) {
                *self.memory_note.lock().unwrap() = Some(format!(
                    "Not enough free memory for Jarvis right now (needs {:.1} GB, {:.1} GB free)",
                    required as f64/1e9,free as f64/1e9));
            }
        }
        let mut q = self.events.lock().unwrap();
        if q.len() == 64 {
            q.pop_front();
        }
        q.push_back(value);
    }
    fn note(&self, text: Option<&str>) {
        *self.note.lock().unwrap() = text.map(str::to_owned);
    }
    fn failed(&self, reason: String) {
        *self.last_failure.lock().unwrap() = Some(reason.clone());
        self.ready.store(false, Ordering::Release);
        self.notify_residency(false);
        let memory = memory_reason(&reason);
        // A resident Rust engine that crashed: a strike (a load failure was counted by `start`).
        if !memory && self.rust_serving.swap(false, Ordering::AcqRel) {
            self.rust_strike(&reason, false);
        }
        if memory {
            // The memory line holds him out until memory is back and stays back (`memory_line`), not 30 s.
            if let Some(gate) = self.memory_gate.lock().unwrap().as_mut() {
                let now = gate.now();
                gate.policy.hold(reason.clone(), now);
            }
            *self.memory_wait.lock().unwrap() = Some(Instant::now());
            let note = self.memory_note.lock().unwrap().take().unwrap_or_else(||
                "Jarvis unloaded to protect free memory; retrying when memory is available.".into());
            self.note(Some(&note));
            self.event(json!({"event":"memory_wait","reason":reason}));
        } else {
            self.memory_wait.lock().unwrap().take();
            self.failures.lock().unwrap().push_back(Instant::now());
            self.note(Some("Drama 3 unavailable. Using the lighter voice."));
            self.event(json!({"event":"failure","reason":reason}));
        }
    }

    /// The worker was killed because it did not leave a cancelled line in time (the user interrupted Heimdall): not a
    /// fault of Jarvis's voice, so no failure is counted and no 30 s back-off starts; he reloads at once and the next
    /// line waits for him (`resume`). 21 Oct 16:42 and 22 Oct 14:22: each such kill sent the next half minute of
    /// replies to the lighter voice.
    fn interrupted(&self, reason: String) {
        // A worker that keeps missing the deadline is wedged, not interrupted: past `INTERRUPTED_RESTARTS` in five
        // minutes the kill counts as a failure, with its back-off and pause (review 24 Oct).
        let mut recent = self.interruptions.lock().unwrap();
        recent.retain(|at| at.elapsed() < Duration::from_secs(300));
        recent.push_back(Instant::now());
        if recent.len() > INTERRUPTED_RESTARTS {
            drop(recent);
            return self.failed(format!("{reason} (repeated after interruptions)"));
        }
        drop(recent);
        self.ready.store(false, Ordering::Release);
        self.notify_residency(false);
        self.memory_wait.lock().unwrap().take();
        self.note(Some("Jarvis voice loading"));
        self.event(json!({"event":"interrupted_restart","reason":reason}));
    }
    /// Starts Jarvis's reload now, if he is wanted and down, rather than at the next health tick: a line that finds
    /// him reloading after an interruption can then wait for him (`wait_ready`). Back-off, memory waits and the
    /// failure pause still apply (`configure`).
    pub fn resume(self: &Arc<Self>) {
        if self.wanted.load(Ordering::Acquire) && !self.ready() {
            self.start_if_down();
        }
    }
    fn record_exit(&self, process: &mut Process, phase: &str) -> bool {
        // EOF can arrive just before waitpid observes the exit. Keep this bounded;
        // pressure termination must be recorded even if stdout was full or locked.
        let deadline = Instant::now() + Duration::from_millis(20);
        loop {
            if let Ok(Some(status)) = process.child.try_wait() {
                if status.code() == Some(73) {
                    self.event(json!({"event":"memory_unload","phase":phase,"exit_code":73}));
                    return true;
                }
                return false;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    pub fn stop(&self) {
        self.wanted.store(false, Ordering::Release);
        // A memory hold outlives Speaking off (memory is what it is); a pending give-way is done by the stop itself.
        self.giving_way.store(false, Ordering::Release);
        self.ready.store(false, Ordering::Release);
        self.notify_residency(false);
        let pid = self.pid.load(Ordering::Acquire);
        if pid > 0 {
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
        }
        // Never wait for a serving mutex on the pipeline thread.
        if let Ok(mut p) = self.process.try_lock() {
            p.take();
            self.pid.store(0, Ordering::Release);
        }
        self.memory_wait.lock().unwrap().take();
        self.memory_note.lock().unwrap().take();
        self.last_failure.lock().unwrap().take();
        self.note(None);
    }
    /// Called by the app's existing health tick. Loading never blocks it.
    pub fn configure(self: &Arc<Self>, wanted: bool) {
        if !wanted {
            if self.wanted.load(Ordering::Acquire) {
                self.stop();
            }
            return;
        }
        self.wanted.store(true, Ordering::Release);
        self.memory_tick();
        self.start_if_down();
    }
    /// `configure(true)`'s reload, without claiming Jarvis is wanted: `resume` from the speech thread can never undo
    /// a `configure(false)` that lands meanwhile (review 24 Oct); `start` re-checks `wanted` too.
    fn start_if_down(self: &Arc<Self>) {
        if self.ready() {
            if let Ok(mut state) = self.process.try_lock() {
                if let Some(p) = state.as_mut() {
                    if let Ok(Some(code)) = p.child.try_wait() {
                        // Its last protocol line says why it left (the memory guard's reason and reading); unread,
                        // the 19 Oct 18:55 idle unload had no reason anywhere.
                        let said = last_words(&p.output);
                        let why = said.as_ref().and_then(|v| v["reason"].as_str()).unwrap_or("no reason given").to_owned();
                        if code.code() == Some(73) {
                            self.event(json!({"event":"memory_unload","phase":"idle","exit_code":73,"reason":why}));
                        }
                        if let Some(v) = said {
                            self.event(v);
                        }
                        self.pid.store(0, Ordering::Release);
                        state.take();
                        self.failed(format!("sidecar exited {code}: {why}"));
                    }
                }
            }
        }
        // Held out for memory (or giving way): no load until the memory line's tick says he may return.
        if self.memory_blocked() {
            return;
        }
        if self.ready() || self.starting.swap(true, Ordering::AcqRel) {
            // A load already running is not the return the memory line allowed; the next one decides afresh.
            self.return_pending.store(false, Ordering::Release);
            return;
        }
        // Refusals stay retryable without spending the crash budget. The sidecar
        // admission preflight exits before loading any weights while memory is low.
        if self.memory_wait.lock().unwrap().as_ref().is_some_and(|at| at.elapsed() < Duration::from_secs(30)) {
            self.starting.store(false, Ordering::Release);
            self.return_pending.store(false, Ordering::Release);
            return;
        }
        let mut failures = self.failures.lock().unwrap();
        while failures
            .front()
            .is_some_and(|at| at.elapsed() > Duration::from_secs(300))
        {
            failures.pop_front();
        }
        if failures.len() >= 3 {
            self.note(Some(
                "Drama 3 paused after repeated failures. Using the lighter voice.",
            ));
            self.starting.store(false, Ordering::Release);
            self.return_pending.store(false, Ordering::Release);
            return;
        }
        // Avoid spinning under unavailable assets/memory. Three attempts per five minutes.
        if failures
            .back()
            .is_some_and(|at| at.elapsed() < Duration::from_secs(30))
        {
            self.starting.store(false, Ordering::Release);
            self.return_pending.store(false, Ordering::Release);
            return;
        }
        drop(failures);
        if self.return_pending.swap(false, Ordering::AcqRel) {
            self.returning.store(true, Ordering::Release);
        }
        self.note(Some("Jarvis voice loading"));
        let this = self.clone();
        if let Err(error) = std::thread::Builder::new()
            .name("drama3-start".into())
            .spawn(move || {
                let mut result = this.start();
                // One more try before the lighter voice takes over: a transient failure (a full descriptor table, a
                // sidecar that died while loading) passes in the half second; the second failure is the one counted.
                if let Err(error) = &result {
                    let reason = format!("{error:#}");
                    if this.wanted.load(Ordering::Acquire) && retryable_load(&reason) {
                        // A line switched meanwhile names this error, not an older one.
                        *this.last_failure.lock().unwrap() = Some(reason.clone());
                        this.event(json!({"event":"load_retry","reason":reason,"cause":fallback_cause(&reason)}));
                        std::thread::sleep(LOAD_RETRY_PAUSE);
                        // A give-way or hold in the pause stops the retry too (`start_with` checks it again).
                        result = if this.memory_blocked() { Err(anyhow!(LOAD_CANCELLED_MEMORY)) } else { this.start() };
                    }
                }
                if let Err(error) = result {
                    let wanted = this.wanted.load(Ordering::Acquire);
                    if wanted && format!("{error:#}").contains(LOAD_CANCELLED_MEMORY) {
                        this.cancelled_for_memory();
                    } else if wanted {
                        this.failed(format!("{error:#}"));
                    } else {
                        this.event(json!({"event":"unloaded"}));
                        this.note(None);
                    }
                }
                this.starting.store(false, Ordering::Release);
                this.returning.store(false, Ordering::Release);
            })
        {
            self.starting.store(false, Ordering::Release);
            self.returning.store(false, Ordering::Release);
            self.failed(error.to_string());
        }
    }
    /// The Rust engine when bundled and not switched off. When it fails to load by itself (a fatal line, its own
    /// exit), or for the second time this run (a deadline, a spawn error, a crash while serving), the Python worker
    /// loads at once and serves until the app restarts. Memory refusals, cancels and Speaking off are
    /// never the engine's fault (review 1 Nov).
    fn start(&self) -> Result<()> {
        let rust = if self.rust_failed.load(Ordering::Acquire) { None } else { rust_engine(&self.assets) };
        let Some(engine) = rust else { return self.start_with(None) };
        let Err(error) = self.start_with(Some(&engine)) else { return Ok(()) };
        let reason = format!("{error:#}");
        if !self.wanted.load(Ordering::Acquire) || !python_takes_over(&reason) || !sidecar_executable(&self.assets).is_file() {
            return Err(error);
        }
        let own = reason.contains(RUST_EXITED) || reason.contains("\"event\":\"fatal\"");
        if !self.rust_strike(&reason, own) {
            return Err(error);
        }
        self.start_with(None)
    }
    /// One strike against the Rust engine; its own failure, or the second strike, hands Jarvis to the Python worker.
    fn rust_strike(&self, reason: &str, own: bool) -> bool {
        let strikes = {
            let mut recent = self.rust_strikes.lock().unwrap();
            recent.retain(|at| at.elapsed() < STRIKE_WINDOW);
            recent.push_back(Instant::now());
            recent.len()
        };
        if !own && strikes < 2 {
            self.event(json!({"event":"engine_strike","engine":"rust","strikes":strikes,"reason":reason}));
            return false;
        }
        if !self.rust_failed.swap(true, Ordering::AcqRel) {
            self.event(json!({"event":"engine_fallback","from":"rust","to":"python","reason":reason}));
        }
        true
    }
    fn start_with(&self, rust: Option<&std::path::Path>) -> Result<()> {
        let mut state = self.process.lock().unwrap();
        state.take();
        ensure!(self.wanted.load(Ordering::Acquire), "load cancelled");
        // Also the Python worker after a failed Rust engine, and the retry: a hold set meanwhile stops them.
        ensure!(!self.memory_blocked(), LOAD_CANCELLED_MEMORY);
        self.rust_serving.store(false, Ordering::Release);
        let python = sidecar_executable(&self.assets);
        ensure!(
            rust.is_some() || python.is_file(),
            "Drama 3 runtime missing: {}",
            python.display()
        );
        // An orphaned native decode is still real memory. Carry its reserve
        // through this entire sidecar residency; only a later admission after
        // actual thread exit may release it. Never rely on a dropped controller.
        let retained = self
            .verifier_reserve
            .lock()
            .unwrap()
            .as_ref()
            .map_or(0, |reserve| reserve());
        if retained > 0 {
            self.event(json!({"event":"verifier_reserve_carried","retained_bytes":retained}));
        }
        let mut command = match rust {
            Some(engine) => Command::new(engine),
            None => {
                let mut c = Command::new(python);
                c.args(["-B", "-u"]).arg(self.assets.join("sidecar.py"));
                c
            }
        };
        if let Some(settings) = &self.offline_agent_settings {
            command.args(["--splash-reserve", "current", "--offline-agent-settings"]).arg(settings);
        }
        let mut child = command
            .arg("--engine")
            .arg(self.assets.join("engine"))
            .env("PYTHONNOUSERSITE", "1")
            .env(
                "HEIMDALL_DRAMA3_RETAINED_VERIFIER_BYTES",
                retained.to_string(),
            )
            .env("HF_HUB_OFFLINE", "1")
            .env("TOKENIZERS_PARALLELISM", "false")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        self.pid.store(child.id() as i32, Ordering::Release);
        let input = child.stdin.take().unwrap();
        let out = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::sync_channel(32);
        std::thread::spawn(move || {
            for line in BufReader::new(out).lines() {
                let Ok(line) = line else { break };
                let value = serde_json::from_str(&line).unwrap_or_else(
                    |_| json!({"event":"fatal","reason":"invalid sidecar protocol"}),
                );
                if tx.send(value).is_err() {
                    break;
                }
            }
        });
        let mut p = Process {
            pid: self.pid.clone(),
            child,
            input,
            output: rx,
        };
        let loading = Instant::now();
        let deadline = loading + Duration::from_secs(45);
        let mut until_ready = || -> Result<()> { loop {
            ensure!(self.wanted.load(Ordering::Acquire), "load cancelled");
            ensure!(!self.memory_blocked(), LOAD_CANCELLED_MEMORY);
            ensure!(Instant::now() < deadline, "Drama 3 load deadline");
            match p.output.recv_timeout(Duration::from_millis(20)) {
                Ok(v) if v["event"] == "ready" => {
                    let load = &v["load"];
                    self.event(json!({"event":"ready","engine":if rust.is_some() { "rust" } else { "python" },
                        "load_ms":loading.elapsed().as_secs_f64()*1000.,
                        "weights_s":load["load_s"],"voice_s":load["jarvis_cache_s"],"warmup_s":load["generation_warmup_s"]}));
                    break Ok(());
                }
                Ok(v) => {
                    self.event(v.clone());
                    // Exit 73 is a memory refusal whatever its words ("Need 55 GB available before loading"):
                    // retried when memory frees up, never spent from the crash budget (19 Oct 16:47).
                    let memory = self.record_exit(&mut p, "load");
                    bail!("sidecar load: {v}{}", if memory { "; Memory unload (exit 73)" } else { "" });
                }
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(e) => {
                    let memory = self.record_exit(&mut p, "load");
                    bail!(
                        "sidecar load: {e}{}",
                        if memory {
                            "; Memory unload (exit 73)"
                        } else {
                            ""
                        }
                    );
                }
            }
        } };
        if let Err(error) = until_ready() {
            if format!("{error:#}").contains(LOAD_CANCELLED_MEMORY) {
                // Its memory goes now: killed here, reaped off this thread (`Process::drop`).
                let _ = p.child.kill();
                return Err(error);
            }
            // A failed Rust engine is gone, its memory with it, before anything else loads (review 1 Nov: the
            // detached reaper let a 26 GB worker linger while the Python worker started); its exit says whose fault.
            if rust.is_some() {
                return Err(error.context(settle(&mut p.child)));
            }
            return Err(error);
        }
        if !self.wanted.load(Ordering::Acquire) {
            bail!("load cancelled");
        }
        *state = Some(p);
        self.rust_serving.store(rust.is_some(), Ordering::Release);
        self.ready.store(true, Ordering::Release);
        self.notify_residency(true);
        if !self.wanted.load(Ordering::Acquire) {
            self.ready.store(false, Ordering::Release);
            self.notify_residency(false);
            state.take();
            bail!("load cancelled");
        }
        self.memory_wait.lock().unwrap().take();
        self.last_failure.lock().unwrap().take();
        // He is actually back: the next hold is news to the user again.
        self.memory_announced.store(false, Ordering::Release);
        self.note(None);
        Ok(())
    }
    /// Emit materialized chunks directly to the queue's playback callback. Buffered
    /// callers (voice-bank tooling) collect the same PCM via render().
    pub fn stream(
        &self,
        text: &str,
        cancel: &dyn Fn() -> bool,
        emit: &mut dyn FnMut(crate::SpeechPacket) -> Result<()>,
    ) -> Result<Audio> {
        self.request(text, None, cancel, emit, false)
    }
    /// `stream` with the first sentence made whole before any of it plays, so it never restarts playback: for the
    /// rest of a line after other sentences of it (`speak_line`).
    pub fn stream_resumed(
        &self,
        text: &str,
        cancel: &dyn Fn() -> bool,
        emit: &mut dyn FnMut(crate::SpeechPacket) -> Result<()>,
    ) -> Result<Audio> {
        self.request(text, None, cancel, emit, true)
    }
    fn request(
        &self,
        text: &str,
        direction: Option<&str>,
        cancel: &dyn Fn() -> bool,
        emit: &mut dyn FnMut(crate::SpeechPacket) -> Result<()>,
        buffered: bool,
    ) -> Result<Audio> {
        if let Some(d) = direction {
            ensure!(valid_direction(d), "invalid delivery direction {d:?}");
        }
        ensure!(self.ready(), "Drama 3 not ready");
        let mut state = loop {
            ensure!(!cancel(), "speech interrupted");
            if let Ok(lock) = self.process.try_lock() {
                break lock;
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        ensure!(self.ready(), "Drama 3 unloaded");
        let p = state.as_mut().context("Drama 3 process missing")?;
        let id = self.ids.fetch_add(1, Ordering::Relaxed) + 1;
        writeln!(
            p.input,
            "{}",
            {
                // Symbols already spelled (`spell_symbols`): the worker's normalizer is a port that may lag this one
                // (ranges and times), and the word check below expects these words. Its own pass changes them no more.
                let mut request = json!({"op":"speak","id":id,"text":spell_symbols(text),"buffered":buffered});
                if let Some(pace) = self.pace.lock().unwrap().as_ref() {
                    request["rate"] = json!(pace.load(Ordering::Acquire) as f64 / 100.);
                }
                if let Some(d) = direction {
                    request["direction"] = json!(d);
                }
                request
            }
        )?;
        p.input.flush()?;
        let started = Instant::now();
        let mut cancelled = None;
        let mut audio = Audio {
            samples: vec![],
            sample_rate: 44100,
            first_audio_ms: 0.,
            synthesis_ms: 0.,
        };
        let parts = sentences(&normalize(text));
        let shown = display_sentences(text);
        let mut committed = 0usize;
        let mut first_complete = false;
        let mut pending_sentence = None;
        let mut pending = Vec::<Audio>::new();
        let mut pending_samples = 0usize;
        let mut verified_sentence = None;
        // The sentence whose takes the word check rejected for one lone word, and those words (coverage.rs).
        let mut lone_words = (usize::MAX, Vec::<String>::new());
        // The sentence being tried, its rejected takes' evidence, and the best of them Jarvis may still say once
        // every retry is spent (`coverage::keepable`): sentence, attempt, wrong words, audio.
        let mut rejected = (usize::MAX, Vec::<Value>::new());
        let mut best: Option<(usize, u64, usize, Vec<Audio>)> = None;
        let mut kept_take = false;
        let mut fallback_reason = None;
        let mut playback_started = None;
        let mut requested_fallback = None;
        let mut output_error = None;
        // Committed audio waits here and reaches playback one chunk at a time, only while the worker has nothing
        // to say. Playback's bounded channel blocks for up to a chunk; the worker's verdict wait must never sit
        // behind several seconds of already-accepted speech (18 Oct: the short last sentence timed out its check
        // that way and was spoken by the lighter voice).
        let mut outbox = VecDeque::<crate::SpeechPacket>::new();
        let mut last_message = Instant::now();
        let result = (|| -> Result<()> {
            loop {
                if (cancel() || output_error.is_some()) && cancelled.is_none() {
                    writeln!(p.input, "{}", json!({"op":"cancel","id":id}))?;
                    p.input.flush()?;
                    cancelled = Some(Instant::now());
                }
                ensure!(
                    !cancelled.is_some_and(|at| at.elapsed() > Duration::from_millis(500)),
                    "Drama 3 cancellation deadline"
                );
                // A stuck worker, not a long reply: speech of any length streams while messages keep coming.
                ensure!(
                    last_message.elapsed() < Duration::from_secs(45),
                    "Drama 3 request deadline"
                );
                let v = match p.output.try_recv() {
                    Ok(v) => v,
                    Err(mpsc::TryRecvError::Empty) => {
                        if cancelled.is_none() && output_error.is_none() {
                            if let Some(packet) = outbox.pop_front() {
                                if let Err(e) = emit(packet) {
                                    output_error = Some(e);
                                }
                                continue;
                            }
                        }
                        match p.output.recv_timeout(Duration::from_millis(5)) {
                            Ok(v) => v,
                            Err(mpsc::RecvTimeoutError::Timeout) => continue,
                            Err(e) => bail!("Drama 3 disconnected: {e}"),
                        }
                    }
                    Err(e) => bail!("Drama 3 disconnected: {e}"),
                };
                last_message = Instant::now();
                ensure!(
                    !cancelled.is_some_and(|at: Instant| at.elapsed() > Duration::from_millis(500)),
                    "Drama 3 cancellation cleanup deadline"
                );
                if v["event"] == "fatal" {
                    self.event(v.clone());
                    bail!("Drama 3: {v}");
                }
                ensure!(
                    v["id"].as_u64() == Some(id),
                    "Drama 3 mismatched request id"
                );
                match v["event"].as_str().unwrap_or("") {
                    "pcm" if cancelled.is_none() => {
                        let bytes = base64::engine::general_purpose::STANDARD
                            .decode(v["pcm"].as_str().context("missing PCM")?)?;
                        ensure!(
                            bytes.len() % 4 == 0 && bytes.len() <= 4 * 44100 * 2,
                            "invalid PCM size"
                        );
                        let samples: Vec<f32> = bytes
                            .chunks_exact(4)
                            .map(|b| f32::from_le_bytes(b.try_into().unwrap()))
                            .collect();
                        ensure!(samples.iter().all(|x| x.is_finite()), "nonfinite PCM");
                        let rate = v["rate"].as_u64().context("missing rate")? as u32;
                        ensure!(rate == 44100, "unexpected Drama 3 sample rate");
                        let sentence = v["sentence"].as_u64().unwrap_or(0) as usize;
                        ensure!(sentence < parts.len(), "invalid sentence index");
                        if audio.samples.is_empty() {
                            audio.first_audio_ms = started.elapsed().as_secs_f64() * 1000.;
                        }
                        let chunk = Audio {
                            samples,
                            sample_rate: rate,
                            first_audio_ms: audio.first_audio_ms,
                            synthesis_ms: 0.,
                        };
                        if sentence == 0 && !buffered {
                            ensure!(!first_complete, "first sentence PCM after completion");
                            // The Rust worker may say a very short first sentence together with the next ones as one
                            // live unit (`covers`, so playback never waits for sentence two); older workers send 1.
                            let covers = v["covers"].as_u64().unwrap_or(1) as usize;
                            ensure!(covers >= 1 && covers <= parts.len(), "invalid first unit");
                            ensure!(committed == 0 || committed == covers, "first unit changed mid-sentence");
                            if committed == 0 {
                                let text = shown[..covers.min(shown.len())].join(" ");
                                outbox.push_back(crate::SpeechPacket::Sentence(text));
                            }
                            committed = covers;
                            playback_started.get_or_insert_with(Instant::now);
                            audio.samples.extend_from_slice(&chunk.samples);
                            outbox.push_back(crate::SpeechPacket::Audio(chunk));
                        } else {
                            ensure!(
                                sentence == committed && (sentence == 0 || first_complete),
                                "out-of-order sentence PCM"
                            );
                            ensure!(
                                pending_sentence.is_none_or(|index| index == sentence),
                                "interleaved sentence PCM"
                            );
                            pending_sentence = Some(sentence);
                            pending_samples += chunk.samples.len();
                            ensure!(
                                pending_samples <= 44100 * 60,
                                "sentence buffer exceeds frame cap"
                            );
                            pending.push(chunk);
                        }
                    }
                    "stall" | "batch" => self.event(v),
                    "retry" if cancelled.is_none() => {
                        // The worker discards the attempt (including one whose verdict arrived after its deadline);
                        // the next attempt must be checked and played alone, never appended to the old take.
                        let sentence = v["sentence"].as_u64().context("missing retry sentence")? as usize;
                        if pending_sentence == Some(sentence) {
                            pending.clear();
                            pending_samples = 0;
                            pending_sentence = None;
                        }
                        if verified_sentence == Some(sentence) {
                            verified_sentence = None;
                        }
                    }
                    "retry" => {}
                    "restart_first" if cancelled.is_none() => {
                        ensure!(
                            !first_complete,
                            "first sentence restart after later playback"
                        );
                        // Nothing of a later sentence is committed yet: everything waiting is the cut prefix.
                        outbox.clear();
                        if let Err(error) = emit(crate::SpeechPacket::RestartFirst) {
                            output_error = Some(error);
                        }
                        audio.samples.clear();
                        committed = 0;
                        pending.clear();
                        pending_sentence = None;
                        pending_samples = 0;
                        self.event(v);
                    }
                    "restart_first" => {}
                    "sentence_check" if cancelled.is_none() => {
                        let sentence =
                            v["sentence"].as_u64().context("missing check sentence")? as usize;
                        let attempt = v["attempt"].as_u64().context("missing check attempt")?;
                        ensure!(
                            sentence > 0
                                && sentence == committed
                                && sentence < parts.len()
                                && pending_sentence == Some(sentence)
                                && attempt < ATTEMPTS,
                            "invalid sentence check"
                        );
                        let candidate = Audio {
                            samples: pending
                                .iter()
                                .flat_map(|a| a.samples.iter().copied())
                                .collect(),
                            sample_rate: 44100,
                            first_audio_ms: 0.,
                            synthesis_ms: 0.,
                        };
                        let checking = Instant::now();
                        let slack_ms = playback_started.map(|at: Instant| {
                            audio.samples.len() as f64 / 44.1 - at.elapsed().as_secs_f64() * 1000.
                        });
                        let verifier = self.verifier.lock().unwrap().clone();
                        let checked = verifier
                            .context("live recognizer unavailable")
                            .and_then(|verify| verify(&parts[sentence], &candidate, cancel));
                        if cancel() {
                            writeln!(p.input, "{}", json!({"op":"cancel","id":id}))?;
                            p.input.flush()?;
                            cancelled = Some(Instant::now());
                            continue;
                        }
                        let mut evidence = checked.unwrap_or_else(
                            |error| json!({"accepted":false,"reason":error.to_string()}),
                        );
                        if lone_words.0 != sentence {
                            lone_words = (sentence, Vec::new());
                        }
                        crate::coverage::accept_repeated_lone_word(&mut evidence, &mut lone_words.1);
                        if rejected.0 != sentence {
                            rejected = (sentence, Vec::new());
                        }
                        if evidence["accepted"] != true {
                            crate::coverage::accept_agreed_take(&mut evidence, &rejected.1);
                        }
                        if evidence["accepted"] != true {
                            if let Some(errors) = crate::coverage::keepable(&evidence, &rejected.1) {
                                if best.as_ref().is_none_or(|b| b.0 != sentence || errors <= b.2) {
                                    best = Some((sentence, attempt, errors, pending.clone()));
                                }
                            }
                            rejected.1.push(evidence.clone());
                            // His last take is his best: he says it and goes on in this request.
                            if attempt + 1 == ATTEMPTS && best.as_ref().is_some_and(|b| b.0 == sentence && b.1 == attempt) {
                                evidence["accepted"] = true.into();
                                evidence["kept_after_retries"] = true.into();
                            }
                        }
                        let accepted = evidence["accepted"].as_bool().unwrap_or(false);
                        self.event(json!({"event":"sentence_coverage","id":id,"sentence":sentence,"attempt":attempt,"check_ms":checking.elapsed().as_secs_f64()*1000.,"playback_slack_ms":slack_ms,"result":evidence}));
                        if accepted {
                            verified_sentence = Some(sentence);
                        } else {
                            pending.clear();
                            pending_samples = 0;
                            pending_sentence = None;
                            verified_sentence = None;
                        }
                        writeln!(
                            p.input,
                            "{}",
                            json!({"op":"verified","id":id,"sentence":sentence,"attempt":attempt,"accepted":accepted})
                        )?;
                        p.input.flush()?;
                    }
                    "sentence_check" => {}
                    "sentence_done" if cancelled.is_none() => {
                        let sentence = v["sentence"]
                            .as_u64()
                            .context("missing completed sentence index")?
                            as usize;
                        ensure!(sentence < parts.len(), "invalid completed sentence index");
                        if sentence == 0 && !buffered {
                            let covers = v["covers"].as_u64().unwrap_or(1) as usize;
                            ensure!(
                                committed == covers && !first_complete,
                                "invalid first sentence completion"
                            );
                        } else {
                            ensure!(
                                pending_sentence == Some(sentence)
                                    && sentence == committed
                                    && (sentence == 0 || verified_sentence == Some(sentence)),
                                "completion without verified complete buffered PCM"
                            );
                            // The worker may die halfway through sending a sentence.
                            // Nothing becomes audible until its completion marker.
                            outbox.push_back(crate::SpeechPacket::Sentence(shown[sentence].clone()));
                            for chunk in pending.drain(..) {
                                audio.samples.extend_from_slice(&chunk.samples);
                                outbox.push_back(crate::SpeechPacket::Audio(chunk));
                            }
                            pending_sentence = None;
                            pending_samples = 0;
                            verified_sentence = None;
                            playback_started.get_or_insert_with(Instant::now);
                            committed = sentence + 1;
                        }
                        if sentence == 0 {
                            first_complete = true;
                        }
                    }
                    "sentence_done" => {}
                    "fallback" => {
                        let remaining = v["remaining_sentence"]
                            .as_u64()
                            .context("missing fallback frontier")?
                            as usize;
                        ensure!(
                            remaining == committed
                                && remaining <= parts.len()
                                && (remaining == 0 || first_complete),
                            "invalid fallback frontier"
                        );
                        audio.synthesis_ms = v["synthesis_ms"].as_f64().unwrap_or(0.);
                        fallback_reason = v["reason"].as_str().map(str::to_owned);
                        // An earlier rejected take was good enough: Jarvis says it rather than switch voices, and a
                        // new request goes on with the rest.
                        match best.take().filter(|b| b.0 == remaining && remaining > 0) {
                            Some((sentence, attempt, errors, chunks)) => {
                                outbox.push_back(crate::SpeechPacket::Sentence(shown[sentence].clone()));
                                for chunk in chunks {
                                    audio.samples.extend_from_slice(&chunk.samples);
                                    outbox.push_back(crate::SpeechPacket::Audio(chunk));
                                }
                                playback_started.get_or_insert_with(Instant::now);
                                committed = sentence + 1;
                                kept_take = true;
                                self.event(json!({"event":"take_kept","id":id,"sentence":sentence,"attempt":attempt,"errors":errors,
                                    "reason":fallback_reason.clone().unwrap_or_default()}));
                                requested_fallback = Some(committed);
                            }
                            None => requested_fallback = Some(remaining),
                        }
                        break;
                    }
                    "done" => {
                        // A worker without delivery support ignores the field and says the words plain.
                        if let (Some(d), None) = (direction, cancelled.as_ref()) {
                            ensure!(v["direction"].as_str() == Some(d), "the worker did not apply direction {d:?}");
                        }
                        if cancelled.is_none() {
                            ensure!(
                                committed == parts.len() && first_complete && pending.is_empty(),
                                "utterance ended before sentence completion"
                            );
                        }
                        audio.synthesis_ms =
                            v["synthesis_ms"].as_f64().context("missing duration")?;
                        break;
                    }
                    "cancelled" => break,
                    "error" => bail!("Drama 3: {}", v["reason"]),
                    "pcm" => {}
                    _ => bail!("unknown Drama 3 event: {v}"),
                }
            }
            Ok(())
        })();
        if result.is_err() {
            let memory = self.record_exit(p, "speech");
            self.pid.store(0, Ordering::Release);
            state.take();
            // The user interrupted (not a playback error) and the worker left too slowly.
            let slow_cancel = cancelled.is_some() && output_error.is_none() && cancel()
                && result.as_ref().unwrap_err().to_string().starts_with("Drama 3 cancellation");
            if self.wanted.load(Ordering::Acquire) && slow_cancel && !memory {
                self.interrupted(result.as_ref().unwrap_err().to_string());
            } else if self.wanted.load(Ordering::Acquire) {
                self.failed(format!(
                    "{}{}",
                    if memory {
                        "Memory unload (exit 73): "
                    } else {
                        ""
                    },
                    result.as_ref().unwrap_err()
                ));
            } else {
                self.ready.store(false, Ordering::Release);
                self.notify_residency(false);
                self.event(json!({"event":"unloaded"}));
                self.note(None);
            }
        }
        if let Some(error) = output_error {
            return Err(error);
        }
        ensure!(!cancel(), "speech interrupted");
        if result.is_err() && committed >= 1 && !first_complete {
            // A crashed/failed worker must not lose the live first sentence either.
            outbox.clear();
            emit(crate::SpeechPacket::RestartFirst)?;
            audio.samples.clear();
            committed = 0;
        }
        // Accepted Jarvis audio plays out in full before completion or before any fallback sentence.
        for packet in outbox.drain(..) {
            ensure!(!cancel(), "speech interrupted");
            emit(packet)?;
        }
        let retries_reason = || format!("{} after {ATTEMPTS} takes", fallback_reason.as_deref().unwrap_or("incomplete sentence"));
        let kept_take = kept_take && result.is_ok();
        if (result.is_err() || requested_fallback.is_some()) && !(kept_take && committed == parts.len()) {
            let remaining = requested_fallback.unwrap_or(committed);
            // After every retry the lighter voice says only this sentence and Jarvis goes on; a stopped Jarvis leaves
            // it the rest of the line (`speak_line`). Each switch carries its reason. A kept take switches nothing.
            let after_retries = result.is_ok();
            if remaining < parts.len() && !kept_take {
                let (text, notice) = if after_retries {
                    (parts[remaining].clone(), NOTICE_MISSED)
                } else {
                    (parts[remaining..].join(" "), NOTICE_STOPPED)
                };
                self.event(json!({"event":"voice_switch","id":id,"sentence":remaining,"text":text,"notice":notice,
                    "reason":result.as_ref().err().map(|e| e.to_string()).unwrap_or_else(retries_reason)}));
            }
            return Err(SpeechFailure {
                remaining: parts[remaining..].join(" "),
                played_text: parts[..remaining].join(" "),
                played: audio,
                remaining_parts: parts[remaining..].to_vec(),
                remaining_shown: shown[remaining..].to_vec(),
                after_retries,
                kept_take,
                reason: result.err().map(|e| e.to_string()).unwrap_or_else(retries_reason),
            }
            .into());
        }
        ensure!(!audio.samples.is_empty(), "empty Drama 3 audio");
        Ok(audio)
    }
    pub fn render(&self, text: &str, cancel: &dyn Fn() -> bool) -> Result<Audio> {
        self.request(text, None, cancel, &mut |_| Ok(()), true)
    }
    /// `render` with a delivery direction ("crisp and urgent") that the worker puts as a cue before each sentence.
    /// Offline ack takes only: live replies stay plain text (ack deliveries asked for on 23 Oct).
    pub fn render_directed(&self, text: &str, direction: &str, cancel: &dyn Fn() -> bool) -> Result<Audio> {
        self.request(text, Some(direction), cancel, &mut |_| Ok(()), true)
    }
}
impl Drop for Service {
    fn drop(&mut self) {
        self.notify_residency(false);
        if let Ok(state) = self.process.get_mut() {
            state.take();
        }
    }
}

/// The interpreter the sidecar runs on. The bundle carries it a second time as "Heimdall Drama 3" (same binary, same
/// folder, so Python finds its runtime): Activity Monitor names a process after its executable, and Drama 3's 24 GB
/// listed as "python3" is where the user couldn't find Heimdall's memory (25 Oct). Older bundles and tests: python3.
pub(crate) fn sidecar_executable(assets: &std::path::Path) -> std::path::PathBuf {
    let named = assets.join("runtime/bin").join(SIDECAR_PROCESS);
    if named.is_file() { named } else { assets.join("runtime/bin/python3") }
}

/// The Rust Drama engine (the default) whenever the bundle carries it (`rust/` beside the Python runtime, named
/// `SIDECAR_PROCESS` so Activity Monitor and the memory meter still count it as Drama 3). `HEIMDALL_DRAMA3_ENGINE=python`,
/// or a missing binary, keeps the Python worker.
pub(crate) fn rust_engine(assets: &std::path::Path) -> Option<std::path::PathBuf> {
    rust_engine_for(assets, std::env::var("HEIMDALL_DRAMA3_ENGINE").ok().as_deref())
}

pub(crate) fn rust_engine_for(assets: &std::path::Path, switch: Option<&str>) -> Option<std::path::PathBuf> {
    let engine = assets.join("rust").join(SIDECAR_PROCESS);
    (switch != Some("python") && engine.is_file()).then_some(engine)
}

/// A Rust start failure the Python worker can fix: anything but a memory refusal (Python would be refused too) or a
/// cancelled load. The words `failed` counts as memory, and the settled exit code (`settle`, exit 73).
pub(crate) fn python_takes_over(reason: &str) -> bool {
    !memory_reason(reason) && !reason.contains("load cancelled")
}

/// Two strikes within this long hand Jarvis to the Python worker; older ones are forgotten (review 1 Nov: two
/// unrelated speech errors hours apart should not).
const STRIKE_WINDOW: Duration = Duration::from_secs(600);

/// How a settled Rust engine left by itself (`settle`); "killed" when it had to be.
const RUST_EXITED: &str = "Rust engine exited";

/// Waits up to 2 s for a failed worker to leave by itself, else kills it, and reaps it on this (start) thread.
fn settle(child: &mut Child) -> String {
    let until = Instant::now() + Duration::from_secs(2);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return format!("{RUST_EXITED} ({status})"),
            Ok(None) if Instant::now() < until => std::thread::sleep(Duration::from_millis(10)),
            _ => break,
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    "Rust engine killed after its failed load".into()
}

/// The sidecar's process name (crates/ui's `memory::DRAMA3_PROCESS` counts it as Drama 3; scripts/bundle_models.py
/// adds the file).
pub const SIDECAR_PROCESS: &str = "Heimdall Drama 3";

#[cfg(test)]
#[path = "drama3_tests/tests.rs"]
mod tests;
#[cfg(test)]
#[path = "drama3_tests/line_tests.rs"]
mod line_tests;
