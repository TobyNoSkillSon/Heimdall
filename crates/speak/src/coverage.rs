//! Word alignment with protected meaning and repeated-phrase rejection.
use serde_json::{json, Value};
use std::collections::HashSet;
use unicode_normalization::UnicodeNormalization;
/// Calibrated on retained complete takes and ASR-confirmed damaged variants.
/// See docs/drama3-integration.md for the measured separation and false retry.
pub const MAX_WORD_ERROR_RATE: f64 = 0.05;
fn number(word: &str) -> Option<u32> {
    let value = match word {
        "zero" | "zerowy" => 0,
        "one" | "jeden" | "jedna" | "jedno" | "pierwszego" => 1,
        "two" | "dwa" | "dwie" | "drugiego" => 2,
        "three" | "trzy" | "trzeciego" => 3,
        "four" | "cztery" | "czwartego" => 4,
        "five" | "pięć" | "piątego" => 5,
        "six" | "sześć" | "szóstego" => 6,
        "seven" | "siedem" | "siódmego" => 7,
        "eight" | "osiem" | "ósmego" => 8,
        "nine" | "dziewięć" | "dziewiątego" => 9,
        "ten" | "dziesięć" | "dziesiątego" => 10,
        "eleven" | "jedenaście" | "jedenastego" => 11,
        "twelve" | "dwanaście" | "dwunastego" => 12,
        "thirteen" | "trzynaście" | "trzynastego" => 13,
        "fourteen" | "czternaście" | "czternastego" => 14,
        "fifteen" | "piętnaście" | "piętnastego" => 15,
        "sixteen" | "szesnaście" | "szesnastego" => 16,
        "seventeen" | "siedemnaście" | "siedemnastego" => 17,
        "eighteen" | "osiemnaście" | "osiemnastego" => 18,
        "nineteen" | "dziewiętnaście" | "dziewiętnastego" => 19,
        "twenty" | "dwadzieścia" | "dwudziestego" => 20,
        "thirty" | "trzydzieści" | "trzydziestego" => 30,
        "forty" | "czterdzieści" => 40,
        "fifty" | "pięćdziesiąt" => 50,
        "sixty" | "sześćdziesiąt" => 60,
        "seventy" | "siedemdziesiąt" => 70,
        "eighty" | "osiemdziesiąt" => 80,
        "ninety" | "dziewięćdziesiąt" => 90,
        _ => return word.parse().ok(),
    };
    Some(value)
}
/// Polish hundreds, each one word ("sześćset").
fn hundreds(word: &str) -> Option<u64> {
    Some(match word {
        "sto" => 100,
        "dwieście" => 200,
        "trzysta" => 300,
        "czterysta" => 400,
        "pięćset" => 500,
        "sześćset" => 600,
        "siedemset" => 700,
        "osiemset" => 800,
        "dziewięćset" => 900,
        _ => return None,
    })
}
fn digits(word: &str) -> bool {
    !word.is_empty() && word.chars().all(|c| c.is_ascii_digit())
}
/// A thousand, spoken or abbreviated after a number ("20k").
fn thousand(word: &str) -> bool {
    matches!(word, "thousand" | "tysiąc" | "tysiące" | "tysięcy" | "tysiąca")
}
/// A number spelled in words at the start of `tokens`, below a million: its value and how many words it took ("six
/// hundred" 600, "a hundred and five" 105, "twenty one thousand" 21000, "sześćset" 600). Millions and billions stay
/// words, as the recognizer writes them ("600 million").
fn spelled(tokens: &[&str]) -> Option<(u64, usize)> {
    // Below a hundred: a tens word may take a unit ("twenty one").
    let below_hundred = |at: usize| -> Option<(u64, usize)> {
        let word = *tokens.get(at)?;
        if digits(word) {
            return None;
        }
        let mut n = number(word)? as u64;
        let mut used = 1;
        if (20..100).contains(&n) && n % 10 == 0 {
            if let Some(unit) = tokens.get(at + 1).filter(|w| !digits(w)).and_then(|w| number(w)).filter(|x| (1..10).contains(x)) {
                n += unit as u64;
                used = 2;
            }
        }
        Some((n, used))
    };
    let below_thousand = |at: usize| -> Option<(u64, usize)> {
        let mut used = 0;
        let mut value = None;
        if let Some(h) = tokens.get(at).and_then(|w| hundreds(w)) {
            value = Some(h);
            used = 1;
        } else if tokens.get(at + 1) == Some(&"hundred") {
            let leading = if tokens[at] == "a" { Some((1, 1)) } else { below_hundred(at).filter(|(n, used)| *used == 1 && (1..10).contains(n)) };
            if let Some((n, _)) = leading {
                value = Some(n * 100);
                used = 2;
            }
        } else if tokens.get(at) == Some(&"hundred") {
            value = Some(100);
            used = 1;
        }
        match value {
            Some(h) => {
                let and = usize::from(tokens.get(at + used) == Some(&"and") && below_hundred(at + used + 1).is_some());
                match below_hundred(at + used + and) {
                    Some((n, more)) if n < 100 => Some((h + n, used + and + more)),
                    _ => Some((h, used)),
                }
            }
            None => below_hundred(at),
        }
    };
    let (mut value, mut used) = match below_thousand(0) {
        Some(found) => found,
        None if tokens.first() == Some(&"a") && tokens.get(1).is_some_and(|w| thousand(w)) => (1, 1),
        None if tokens.first().is_some_and(|w| thousand(w)) => (1, 0),
        None => return None,
    };
    if tokens.get(used).is_some_and(|w| thousand(w)) {
        value *= 1000;
        used += 1;
        if let Some((n, more)) = below_thousand(used) {
            value += n;
            used += more;
        }
    }
    Some((value, used))
}
/// An English ordinal word as a number and its written suffix ("third" is 3 and "rd").
fn ordinal(word: &str) -> Option<(u32, &'static str)> {
    Some(match word {
        "first" => (1, "st"),
        "second" => (2, "nd"),
        "third" => (3, "rd"),
        "fourth" => (4, "th"),
        "fifth" => (5, "th"),
        "sixth" => (6, "th"),
        "seventh" => (7, "th"),
        "eighth" => (8, "th"),
        "ninth" => (9, "th"),
        "tenth" => (10, "th"),
        "eleventh" => (11, "th"),
        "twelfth" => (12, "th"),
        "thirteenth" => (13, "th"),
        "fourteenth" => (14, "th"),
        "fifteenth" => (15, "th"),
        "sixteenth" => (16, "th"),
        "seventeenth" => (17, "th"),
        "eighteenth" => (18, "th"),
        "nineteenth" => (19, "th"),
        "twentieth" => (20, "th"),
        "thirtieth" => (30, "th"),
        "fortieth" => (40, "th"),
        "fiftieth" => (50, "th"),
        "sixtieth" => (60, "th"),
        "seventieth" => (70, "th"),
        "eightieth" => (80, "th"),
        "ninetieth" => (90, "th"),
        _ => return None,
    })
}
/// A unit or currency the recognizer writes as a symbol or abbreviation, in one spelling for both sides ("GB" and
/// "gigabytes" are "gigabyte", "$" and "dollars" are "dollar"). Abbreviations count only after a number.
fn unit(word: &str, after_number: bool) -> Option<&'static str> {
    Some(match word {
        "$" | "dollar" | "dollars" | "usd" | "dolar" | "dolary" | "dolarów" | "dolara" => "dollar",
        "€" | "euro" | "euros" | "eur" => "euro",
        "£" | "pound" | "pounds" | "funt" | "funty" | "funtów" | "funta" => "pound",
        "degree" | "degrees" | "stopień" | "stopnie" | "stopni" | "stopnia" => "degrees",
        "celsjusza" => "celsius",
        "fahrenheita" => "fahrenheit",
        "%" | "percent" | "procent" => "percent",
        "gigabyte" | "gigabytes" => "gigabyte",
        "terabyte" | "terabytes" => "terabyte",
        "megabyte" | "megabytes" => "megabyte",
        "kilobyte" | "kilobytes" => "kilobyte",
        "millisecond" | "milliseconds" => "millisecond",
        "million" | "millions" | "milion" | "miliony" | "milionów" | "miliona" => "million",
        "billion" | "billions" | "miliard" | "miliardy" | "miliardów" | "miliarda" => "billion",
        "złoty" | "złote" | "złotych" | "złotego" | "złotówek" | "pln" => "złoty",
        "gb" | "gig" | "gigs" if after_number => "gigabyte",
        "tb" if after_number => "terabyte",
        "mb" if after_number => "megabyte",
        "kb" if after_number => "kilobyte",
        "ms" if after_number => "millisecond",
        "b" | "bn" if after_number => "billion",
        "mln" if after_number => "million",
        "zł" | "zl" if after_number => "złoty",
        // "6 ch" is said "6 channel" (`drama3::spell_symbols`; 25-26 Oct: 12 takes rejected for "ch").
        "ch" if after_number => "channel",
        _ => return None,
    })
}
/// A unit abbreviation written straight after its number ("73GB", "600m", "20k"): split so the number and the unit
/// are compared apart. Other letter-digit words ("DFlash2", "M5", "21st") stay whole.
fn suffixed(token: &str) -> Option<(&str, &str)> {
    let at = token.find(|c: char| !c.is_ascii_digit())?;
    let (n, suffix) = token.split_at(at);
    (at > 0 && (unit(suffix, true).is_some() || matches!(suffix, "m" | "k" | "x"))).then_some((n, suffix))
}
/// A web address's top-level name straight after its dot ("com" in "X.com"), ending there.
fn domain(after: &[char]) -> bool {
    const TOP: [&str; 15] = ["com", "org", "net", "io", "ai", "dev", "app", "co", "pl", "uk", "de", "eu", "gov", "edu", "tv"];
    let name: String = after.iter().take_while(|c| c.is_alphanumeric()).collect();
    TOP.contains(&name.as_str())
}
/// The words of `text` as they are spoken: lower case, punctuation dropped, numbers as digits ("twenty one" and
/// "21", "six hundred" and "600", "1,000" and "1000"), a currency symbol after its amount ("$20" is "20 dollar") and
/// units in one spelling (`unit`).
pub fn words(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.nfkc().flat_map(char::to_lowercase).collect();
    // A thousands separator is part of the number: the recognizer writes "1,000" for a spoken "thousand" (19 Oct:
    // read as "1 0" and the take was rejected).
    // So is a space between digit groups, as the agent writes "86 000" (22 Oct 21:31: heard "86,000", read as "86 0",
    // and all three Jarvis takes were rejected): only after a group of one to three digits, so "2026 100" stays two.
    let separator = |k: usize| {
        let group = chars[..k].iter().rev().take_while(|c| c.is_ascii_digit()).count();
        (chars[k] == ',' || (chars[k].is_whitespace() && (1..=3).contains(&group)))
            && group > 0
            && chars.len() >= k + 4
            && chars[k + 1..k + 4].iter().all(char::is_ascii_digit)
            && !chars.get(k + 4).is_some_and(char::is_ascii_digit)
    };
    let mut text = String::new();
    for k in (0..chars.len()).filter(|&k| !separator(k)) {
        match chars[k] {
            '$' | '€' | '£' | '%' => text.extend([' ', chars[k], ' ']),
            // A decimal point ("5.5", "v1.2") is its own token, so "$1.5 billion" keeps its decimals together.
            '.' if k > 0 && chars[k - 1].is_ascii_digit() && chars.get(k + 1).is_some_and(char::is_ascii_digit) => {
                text.push_str(" . ")
            }
            // The dot of a web address ("X.com") is said "dot", which the recognizer writes either way (25 Oct: "X.com"
            // heard "X dot com", rejected for the added "dot").
            '.' if k > 0 && chars[k - 1].is_alphanumeric() && domain(&chars[k + 1..]) => text.push_str(" dot "),
            // A dot that starts a word is the spoken "dot" of an address (".ai"), which the recognizer writes bare.
            '.' if chars.get(k + 1).is_some_and(|c| c.is_alphabetic()) && (k == 0 || chars[k - 1].is_whitespace()) => {
                text.push_str(" dot ")
            }
            // Said as words (`drama3::spell_symbols`), however the recognizer writes them.
            '&' => text.push_str(" and "),
            '+' => text.push_str(" plus "),
            '@' => text.push_str(" at "),
            '°' => text.push_str(" degrees "),
            '×' => text.push_str(" x "),
            // "1920x1080" is the two numbers apart, on both sides.
            'x' if k > 0 && chars[k - 1].is_ascii_digit() && chars.get(k + 1).is_some_and(char::is_ascii_digit) => text.push_str(" x "),
            '#' if chars.get(k + 1).is_some_and(char::is_ascii_digit) => text.push_str(" number "),
            // A dash between numbers ("4-1", "9–5") is a range or score: `RANGE`, never joined across.
            '-' | '–' | '—' if k > 0 && chars[k - 1].is_ascii_digit() && chars.get(k + 1).is_some_and(char::is_ascii_digit) => {
                text.push_str(" ~ ")
            }
            '’' => text.push('\''),
            c if c.is_alphanumeric() || c == '\'' => text.push(c),
            _ => text.push(' '),
        }
    }
    let mut tokens = Vec::new();
    for token in text.split_whitespace().map(|w| w.trim_matches('\'')).filter(|w| !w.is_empty()) {
        match suffixed(token) {
            Some((n, suffix)) => tokens.extend([n, suffix]),
            None => tokens.push(token),
        }
    }
    let mut result: Vec<String> = Vec::new();
    // A number at `at`, spelled or in digits, with a following thousand ("20 thousand", "20k").
    let amount = |at: usize| -> Option<(u64, usize)> {
        if let Some(found) = spelled(&tokens[at..]) {
            return Some(found);
        }
        // A leading zero is part of what was said ("00 40" for a time, heard "0040"; 24 Oct 01:16): such a token stays
        // as written, on both sides.
        if tokens[at].len() > 1 && tokens[at].starts_with('0') {
            return None;
        }
        let n: u64 = tokens[at].parse().ok().filter(|_| digits(tokens[at]))?;
        match tokens.get(at + 1) {
            Some(w) if thousand(w) || *w == "k" => Some((n.saturating_mul(1000), 2)),
            _ => Some((n, 1)),
        }
    };
    let mut i = 0;
    while i < tokens.len() {
        let after_number = result.last().is_some_and(|w| digits(w));
        // An abbreviated unit after a spelled magnitude: "2 mln zł" is "2 million złoty".
        let after_amount = after_number || result.last().is_some_and(|w| matches!(w.as_str(), "million" | "billion"));
        if tokens[i] == "." {
            // Spoken as "point", which is dropped between numbers too: both are "5 5".
            i += 1;
        } else if let Some((n, used)) = amount(i) {
            result.push(n.to_string());
            i += used;
        } else if tokens[i] == "point" && after_number && i + 1 < tokens.len() && amount(i + 1).is_some() {
            // "five point five" is "5.5", whose point the punctuation pass drops.
            i += 1;
        } else if after_number
            && tokens[i] == "and"
            && amount(i + 1).is_some_and(|(_, used)| tokens.get(i + 1 + used).is_some_and(|w| fraction(w)))
        {
            // The "and" of a mixed number is not written: "three and three quarter hours" heard "3 3 quarter hours" (2 Nov
            // 23:47, all three takes rejected and the sentence went to the lighter voice).
            i += 1;
        } else if after_number && tokens[i] == "and" && tokens.get(i + 1) == Some(&"a") && tokens.get(i + 2) == Some(&"half") {
            if tokens.get(i + 3).is_some_and(|w| thousand(w)) {
                // "two and a half thousand" is 2500, which the recognizer writes "2,500" or "2.500" (25-26 Oct: six
                // takes rejected as "2 5 1000").
                let n: u64 = result.last().and_then(|w| w.parse().ok()).unwrap_or(0);
                *result.last_mut().unwrap() = n.saturating_mul(1000).saturating_add(500).to_string();
                i += 4;
            } else {
                // "three and a half" is 3.5, which the recognizer writes "3.5".
                result.push("5".to_owned());
                i += 3;
            }
        } else if let (Some(currency), Some((n, used))) = (
            matches!(tokens[i], "$" | "€" | "£").then(|| unit(tokens[i], false)).flatten(),
            (i + 1 < tokens.len()).then(|| amount(i + 1)).flatten(),
        ) {
            // "$20" is said "20 dollars", "$1.5 billion" "1.5 billion dollars".
            result.push(n.to_string());
            i += 1 + used;
            while i < tokens.len() {
                if matches!(tokens[i], "." | "point") && tokens.get(i + 1).is_some_and(|w| digits(w)) {
                    result.push(tokens[i + 1].to_owned());
                    i += 2;
                } else if let Some(magnitude) = unit(tokens[i], true).filter(|u| matches!(*u, "million" | "billion")) {
                    result.push(magnitude.to_owned());
                    i += 1;
                } else if tokens[i] == "m" {
                    result.push("million".to_owned());
                    i += 1;
                } else {
                    break;
                }
            }
            result.push(currency.to_owned());
        } else if let Some((n, suffix)) = ordinal(tokens[i]) {
            // An ordinal as the recognizer writes it ("twentieth" is "20th", "twenty first" "21st"; 24 Oct 03:03: all
            // three takes of "the twentieth century" heard "20th" and the sentence went to the lighter voice).
            match result.last().and_then(|w| w.parse::<u32>().ok()).filter(|t| after_number && n < 10 && (20..100).contains(t) && t % 10 == 0) {
                Some(tens) => *result.last_mut().unwrap() = format!("{}{}", tens + n, suffix),
                None => result.push(format!("{n}{suffix}")),
            }
            i += 1;
        } else if after_number && matches!(tokens[i], "x" | "na" | "razy") {
            // "1920x1080" is said "1920 by 1080", "4x faster" "4 times faster".
            let between = tokens.get(i + 1).is_some_and(|w| w.starts_with(|c: char| c.is_ascii_digit()) || spelled(&tokens[i + 1..]).is_some());
            // Polish "2 na 3", "4 razy".
            let by = match tokens[i] { "na" => true, "razy" => false, _ => between };
            result.push(if by { "by" } else { "times" }.to_owned());
            i += 1;
        } else if result.last().is_some_and(|w| w == "degrees") && matches!(tokens[i], "c" | "f") {
            result.push(if tokens[i] == "c" { "celsius" } else { "fahrenheit" }.to_owned());
            i += 1;
        } else if let Some(word) = unit(tokens[i], after_amount) {
            result.push(word.to_owned());
            i += 1;
        } else {
            match tokens[i] {
                "don't" => result.extend(["do".into(), "not".into()]),
                "can't" | "cannot" => result.extend(["can".into(), "not".into()]),
                "won't" => result.extend(["will".into(), "not".into()]),
                word => result.push(decade(word).unwrap_or_else(|| word.to_owned())),
            }
            i += 1;
        }
    }
    // "four to one" is written "4-1" (a score or a range): the "to" between two numbers is `RANGE` on both sides (26 Oct:
    // three takes of "four to one" rejected for "-to"). It stays a token, so "4 to 1" heard "41" is no joined number.
    // A unit or scale said once for a range is written after both numbers: "a hundred to three hundred dollars" heard
    // "$100 to $300", "19 to 42 million" heard "19 million to 42 million" (4 Nov 02:15 and 02:29: every take rejected for
    // "+to" and the sentence went to the lighter voice). The run after the first number is dropped on both sides.
    let shared = |w: &str| matches!(w, "dollar" | "euro" | "pound" | "złoty" | "percent" | "million" | "billion" | "gigabyte" | "terabyte" | "megabyte" | "kilobyte" | "millisecond");
    let mut k = 1;
    while k < result.len() {
        let run = result[k..].iter().take_while(|w| shared(w)).count();
        let to = k + run;
        if run > 0
            && digits(&result[k - 1])
            && result.get(to).is_some_and(|w| w == "to")
            && result.get(to + 1).is_some_and(|w| digits(w))
            && result.get(to + 2..to + 2 + run).is_some_and(|r| r == &result[k..to])
        {
            result.drain(k..to);
        }
        k += 1;
    }
    for k in 1..result.len().saturating_sub(1) {
        if result[k] == "to" && digits(&result[k - 1]) && digits(&result[k + 1]) {
            result[k] = RANGE.to_owned();
        }
    }
    // A thousand said once for a range ("19 to 42 thousand dollars", heard "$19,000 to $42,000"; 2 Nov 15:58, three
    // takes): the first number is in thousands too, on both sides.
    for k in 1..result.len().saturating_sub(1) {
        if result[k] == RANGE {
            if let (Ok(a), Ok(b)) = (result[k - 1].parse::<u64>(), result[k + 1].parse::<u64>()) {
                if (1..1000).contains(&a) && b >= 1000 && b % 1000 == 0 && a * 1000 <= b && b < a * 10_000 {
                    result[k - 1] = (a * 1000).to_string();
                }
            }
        }
    }
    // "one dollar thirty six" is written "$1.36", whose cents come before the unit.
    let mut k = 0;
    while k + 2 < result.len() {
        if digits(&result[k]) && result[k + 1] == "dollar" && digits(&result[k + 2]) && result[k + 2].len() <= 2 {
            result.swap(k + 1, k + 2);
        }
        k += 1;
    }
    result
}
/// The fraction of a mixed number ("three and three quarters").
fn fraction(word: &str) -> bool {
    matches!(word, "quarter" | "quarters" | "third" | "thirds" | "half" | "halves" | "fifths" | "eighths" | "3rd" | "5th" | "8th")
}
/// A decade as the recognizer writes it: "forties" is "40s", and so is "40's" (3 Nov 19:10: "mid-forties" heard "mid
/// 40s" on all three takes, and the sentence went to the lighter voice).
fn decade(word: &str) -> Option<String> {
    const TENS: [&str; 8] = ["twenties", "thirties", "forties", "fifties", "sixties", "seventies", "eighties", "nineties"];
    if let Some(k) = TENS.iter().position(|t| *t == word) {
        return Some(format!("{}0s", k + 2));
    }
    let n = word.strip_suffix("'s")?;
    (digits(n) && n.ends_with('0')).then(|| format!("{n}s"))
}
/// The token for a "to" or dash between two numbers (`words`): free to drop on either side ("4 to 1" heard "4 1"), but
/// a compound never joins across it ("4 to 1" heard "41", "9 to 5" heard "95": another number; review, night 1027).
const RANGE: &str = "~";
fn protected(word: &str) -> bool {
    matches!(
        word,
        "not"
            | "no"
            | "never"
            | "nie"
            | "nigdy"
            | "hundred"
            | "thousand"
            | "million"
            | "billion"
            | "sto"
            | "tysiąc"
            | "tysiące"
            | "milion"
            | "first"
            | "second"
            | "third"
            | "fourth"
            | "fifth"
            | "sixth"
            | "seventh"
            | "eighth"
            | "ninth"
            | "tenth"
            | "eleventh"
            | "twelfth"
            | "thirteenth"
            | "fourteenth"
            | "fifteenth"
            | "sixteenth"
            | "seventeenth"
            | "eighteenth"
            | "nineteenth"
            | "twentieth"
            | "thirtieth"
            | "fortieth"
            | "fiftieth"
            | "sixtieth"
            | "seventieth"
            | "eightieth"
            | "ninetieth"
            | "pierwszy"
            | "drugi"
            | "trzeci"
            | "czwarty"
            | "piąty"
            | "szósty"
            | "siódmy"
            | "ósmy"
            | "dziewiąty"
            | "dziesiąty"
            | "dwieście"
            | "trzysta"
            | "czterysta"
            | "pięćset"
            | "sześćset"
            | "siedemset"
            | "osiemset"
            | "dziewięćset"
            // Units and currencies (`unit`): "20 dollars" is not "20 euros", and a dropped unit changes the amount.
            | "dollar"
            | "euro"
            | "pound"
            | "percent"
            | "złoty"
            | "gigabyte"
            | "terabyte"
            | "megabyte"
            | "kilobyte"
            | "millisecond"
            | "degrees"
            | "celsius"
            | "fahrenheit"
    ) || word.ends_with("n't")
        || word.chars().any(char::is_numeric)
}
fn noise(word: &str) -> bool {
    matches!(
        word,
        "a" | "an" | "the" | "uh" | "um" | "erm" | "hmm" | "yyy" | "eee"
    )
}
fn repeated_run(expected: &[String], actual: &[String]) -> bool {
    use std::collections::HashMap;
    // Every newly repeated phrase is checked, including non-adjacent repetitions.
    for width in 2..=actual.len() / 2 {
        let mut counts = HashMap::new();
        for run in actual.windows(width) {
            *counts.entry(run).or_insert(0usize) += 1;
        }
        for (run, count) in counts {
            if count >= 2
                && count
                    > expected
                        .windows(width)
                        .filter(|candidate| *candidate == run)
                        .count()
            {
                return true;
            }
        }
    }
    false
}
/// The value of a Roman numeral the recognizer may write for a spoken number ("Opus five" heard as "Opus V").
fn roman(word: &str) -> Option<u32> {
    if word.is_empty() || word.len() > 8 || !word.chars().all(|c| "ivxlc".contains(c)) {
        return None;
    }
    let value = |c| match c { 'i' => 1, 'v' => 5, 'x' => 10, 'l' => 50, _ => 100 };
    let digits: Vec<u32> = word.chars().map(value).collect();
    let total = digits.iter().enumerate().map(|(k, d)| {
        if digits.get(k + 1).is_some_and(|next| next > d) { -(*d as i64) } else { *d as i64 }
    }).sum::<i64>();
    // Only the canonical spelling counts ("iv", never "iiii" or "vx").
    let canonical = |mut n: u32| {
        let mut out = String::new();
        for (v, s) in [(100, "c"), (90, "xc"), (50, "l"), (40, "xl"), (10, "x"), (9, "ix"), (5, "v"), (4, "iv"), (1, "i")] {
            while n >= v { out.push_str(s); n -= v; }
        }
        out
    };
    (1..=100).contains(&total).then_some(total as u32).filter(|n| canonical(*n) == word)
}
/// The words a lone "m" after a number stands for: the recognizer writes "600m" for "600 million", and "m" is also
/// the usual abbreviation of these.
fn m(word: &str) -> bool {
    matches!(word, "million" | "meter" | "meters" | "metre" | "metres" | "minute" | "minutes" | "mile" | "miles")
}
/// One spoken word written two ways: the same token, a numeral and its Roman form, or "m" and a word it abbreviates.
fn same(expected: &str, actual: &str) -> bool {
    expected == actual
        || (expected == "m" && m(actual))
        || (actual == "m" && m(expected))
        || matches!((expected.parse::<u32>().ok(), roman(actual)), (Some(x), Some(y)) if x == y)
        || matches!((roman(expected), actual.parse::<u32>().ok()), (Some(x), Some(y)) if x == y)
}
/// The recognizer's spelling of the same spoken word: an inflection or a dropped or doubled letter at the word's end
/// ("processes" heard as "processors", "Heimdall" as "Heimdal", "notices" as "notice"). Never a protected word, a
/// short word or a different stem; as the final word it may not be shorter (a cut tail is what the check catches).
fn near(expected: &str, actual: &str) -> bool {
    if protected(expected) || protected(actual) {
        return false;
    }
    let (a, b): (Vec<char>, Vec<char>) = (expected.chars().collect(), actual.chars().collect());
    if a.len() < 5 || b.len() < 5 {
        return false;
    }
    let prefix = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    if prefix < 4 || prefix + 3 < a.len().max(b.len()) {
        return false;
    }
    // Edit distance of the differing tails (at most three characters each).
    let (x, y) = (&a[prefix..], &b[prefix..]);
    let mut row: Vec<usize> = (0..=y.len()).collect();
    for (i, cx) in x.iter().enumerate() {
        let mut previous = row[0];
        row[0] = i + 1;
        for (j, cy) in y.iter().enumerate() {
            let next = (previous + usize::from(cx != cy)).min(row[j] + 1).min(row[j + 1] + 1);
            previous = row[j + 1];
            row[j + 1] = next;
        }
    }
    row[y.len()] <= 2
}
/// How an English word sounds, as far as its spelling shows: the consonants by sound ("c" before e/i/y is "s", "x" is
/// "ks", "ch"/"sh" one sound, "ph" is "f", a silent final "e" or "gh" dropped, doubled letters once) and the vowels,
/// the first exactly and later (unstressed) ones as one vowel. `None` for a word that is protected, short or not
/// plain letters: those are never compared by sound.
fn sound(word: &str) -> Option<String> {
    let word: String = word.chars().filter(|c| *c != '\'').collect();
    if protected(&word) || word.chars().count() < 3 || !word.chars().all(|c| c.is_ascii_lowercase()) {
        return None;
    }
    let vowel = |c: u8| b"aeiouy".contains(&c);
    // A doubled consonant is one sound, "xx" too ("benchmaxxing"/"benchmaxing", 20 Oct).
    let mut s = word.into_bytes();
    s.dedup_by(|next, previous| next == previous && !vowel(*next));
    // A silent final "e" after a consonant is dropped once the consonant's sound is known ("licence": the "c" before
    // it is still an "s", 20 Oct "licence"/"license").
    let silent_e = s.len() > 3 && s.ends_with(b"e") && !vowel(s[s.len() - 2]);
    let mut out = Vec::new();
    let mut k = 0;
    while k < s.len() {
        let next = s.get(k + 1).copied();
        let (code, used): (&[u8], usize) = match (s[k], next) {
            (b't', Some(b'c')) if s.get(k + 2) == Some(&b'h') => (b"X", 3),
            (b'c' | b's', Some(b'h')) => (b"X", 2),
            (b'p', Some(b'h')) => (b"f", 2),
            (b'g', Some(b'h')) if k > 0 => (b"", 2),
            (b'w', Some(b'h')) => (b"w", 2),
            (b'w', Some(b'r')) | (b'k', Some(b'n')) if k == 0 => (b"", 1),
            (b'c', Some(b'k')) => (b"k", 2),
            (b'q', Some(b'u')) => (b"kw", 2),
            (b'q', _) => (b"k", 1),
            (b'x', _) => (b"ks", 1),
            (b'z', _) => (b"s", 1),
            (b'c', Some(b'e' | b'i' | b'y')) => (b"s", 1),
            (b'c', _) => (b"k", 1),
            _ => (std::slice::from_ref(&s[k]), 1),
        };
        for &c in code {
            if out.last() != Some(&c) {
                out.push(c);
            }
        }
        k += used;
    }
    if silent_e && out.last() == Some(&b'e') {
        out.pop();
    }
    // A final "r" after an unstressed later vowel is not said in Jarvis's British speech: "adder" sounds as "add a"
    // (2 Nov 05:49: "the add a model kit" heard "the Adder model kit" on all three takes).
    let groups = out.iter().enumerate().filter(|&(k, c)| vowel(*c) && !(k == 0 && *c == b'y') && (k == 0 || !vowel(out[k - 1]))).count();
    if out.len() >= 3 && out.last() == Some(&b'r') && vowel(out[out.len() - 2]) && groups >= 2 {
        out.pop();
    }
    // The first vowel group as written; every later one is a single unstressed vowel.
    let mut key = String::new();
    let mut seen_vowel = false;
    let mut k = 0;
    while k < out.len() {
        if vowel(out[k]) && !(k == 0 && out[k] == b'y') {
            let start = k;
            while k < out.len() && vowel(out[k]) {
                k += 1;
            }
            if seen_vowel {
                key.push('a');
            } else {
                key.push_str(std::str::from_utf8(&out[start..k]).ok()?);
            }
            seen_vowel = true;
        } else {
            key.push(out[k] as char);
            k += 1;
        }
    }
    Some(key)
}
/// Two spellings of the same sound ("cache"/"cash", "Mac's"/"max", "Vercel"/"versal"): the recognizer cannot tell
/// them apart, so the take said the word (19 Oct: each rejected all three correct Jarvis takes of a sentence).
fn sounds_alike(expected: &str, actual: &str) -> bool {
    expected != actual && matches!((sound(expected), sound(actual)), (Some(x), Some(y)) if x == y)
}
/// The sounds of a word in a rough common spelling, for comparing a foreign name with the English recognizer's
/// spelling of it: Polish letters by their sound ("ś"/"sz"/"sch"/"sh" one sound, "w" and "v" one sound, "ł" a "w",
/// "cz"/"ć"/"ch" one sound in English), other accents dropped, each vowel group one mark, doubled sounds once.
fn rough(word: &str) -> String {
    let polish = word.chars().any(|c| "ąćęłńóśźż".contains(c));
    let s: Vec<char> = word.chars().filter(|c| *c != '\'').collect();
    let mut out: Vec<char> = Vec::new();
    let mut k = 0;
    while k < s.len() {
        let next = s.get(k + 1).copied();
        let (code, used): (&str, usize) = match (s[k], next) {
            ('s', Some('c')) if s.get(k + 2) == Some(&'h') => ("X", 3),
            ('s', Some('h' | 'z')) | ('ś', _) => ("X", if s[k] == 'ś' { 1 } else { 2 }),
            ('t', Some('c')) if s.get(k + 2) == Some(&'h') => ("C", 3),
            ('c', Some('h')) if polish => ("h", 2),
            ('c', Some('h' | 'z')) | ('ć', _) => ("C", if s[k] == 'ć' { 1 } else { 2 }),
            ('r', Some('z')) if polish => ("Z", 2),
            ('ż' | 'ź', _) => ("Z", 1),
            ('z', Some('h')) => ("Z", 2),
            ('p', Some('h')) => ("f", 2),
            ('c', Some('k')) => ("k", 2),
            ('c', Some('e' | 'i' | 'y')) if !polish => ("s", 1),
            ('c', _) if polish => ("ts", 1),
            ('c' | 'q', _) => ("k", 1),
            ('x', _) => ("ks", 1),
            ('w', _) if !polish && next.is_none_or(|c| !"aeiouy".contains(c)) => ("", 1),
            ('w' | 'v', _) => ("v", 1),
            ('ł', _) => ("w", 1),
            ('j', _) if polish => ("y", 1),
            ('ą', _) => ("on", 1),
            ('ę', _) => ("en", 1),
            ('ó', _) => ("u", 1),
            ('ń', _) => ("n", 1),
            (c, _) => {
                let plain: String = c.to_string().nfkd().filter(char::is_ascii_alphanumeric).collect();
                k += 1;
                for c in plain.chars() {
                    let c = if "aeiouy".contains(c) { 'a' } else { c };
                    if out.last() != Some(&c) {
                        out.push(c);
                    }
                }
                continue;
            }
        };
        for c in code.chars() {
            let c = if "aeiouy".contains(c) { 'a' } else { c };
            if out.last() != Some(&c) {
                out.push(c);
            }
        }
        k += used;
    }
    out.into_iter().collect()
}
/// A foreign name heard by the English recognizer in its own spelling ("Światowid" as "Schwiatovid", 20 Oct: all
/// three correct takes rejected): the expected word has letters outside English, the heard one is plain letters, and
/// their rough sounds (`rough`, five or more) are at most one sound apart. Never a protected word.
fn foreign_alike(expected: &str, actual: &str) -> bool {
    if expected.is_ascii() || !actual.chars().all(|c| c.is_ascii_lowercase()) || protected(actual) {
        return false;
    }
    let (x, y): (Vec<char>, Vec<char>) = (rough(expected).chars().collect(), rough(actual).chars().collect());
    if x.len() < 5 || y.len() < 5 {
        return false;
    }
    let mut row: Vec<usize> = (0..=y.len()).collect();
    for (i, cx) in x.iter().enumerate() {
        let mut previous = row[0];
        row[0] = i + 1;
        for (j, cy) in y.iter().enumerate() {
            let next = (previous + usize::from(cx != cy)).min(row[j] + 1).min(row[j + 1] + 1);
            previous = row[j + 1];
            row[j + 1] = next;
        }
    }
    row[y.len()] <= 1
}
/// `parts` (two to `MAX_PARTS` words) heard as the single word `whole`, or the reverse: a compound written as one word
/// or several ("pull down"/"pulldown", "13 51"/"1351", "AllForOne"/"all for one", "DFlash2"/"D Flash 2"). Number words
/// were turned into digits by `words`, so a digit part is also tried spelled out ("every one" is "every 1" here).
/// Without a number or negation, the joined spelling may also sound alike ("a Vercel" heard as "Aversel").
fn joined(parts: &[String], whole: &str) -> bool {
    fn spelled(word: &str) -> &str {
        const WORDS: [&str; 13] = ["zero", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten", "eleven", "twelve"];
        word.parse::<usize>().ok().and_then(|n| WORDS.get(n)).copied().unwrap_or(word)
    }
    fn concatenates(parts: &[String], rest: &str) -> bool {
        match parts.split_first() {
            None => rest.is_empty(),
            // A letter "o" said as zero ("GPT-4o" heard "GPT-4.0", 26 Oct, three takes).
            // "too" said after a number is heard as its digit ("aimed at 8.5 too" heard "8.52", 3 Nov 03:51, three takes).
            Some((first, more)) => [first.as_str(), spelled(first), if first == "0" { "o" } else { first.as_str() }, if first == "too" { "2" } else { first.as_str() }]
                .iter()
                .any(|form| rest.strip_prefix(*form).is_some_and(|tail| concatenates(more, tail))),
        }
    }
    if !(2..=MAX_PARTS).contains(&parts.len()) {
        return false;
    }
    if contraction(whole).is_some_and(|(stem, clitic)| parts[0] == stem && clitic_words(clitic).contains(&parts[1].as_str())) {
        return true;
    }
    concatenates(parts, whole)
        || (!protected(whole) && !parts.iter().any(|p| protected(p)) && sounds_alike(&parts.concat(), whole))
        || compound_alike(&parts.concat(), whole)
}
/// A name of letters and digits heard in another spelling of its letters, the digits exactly the same: "DINOv3" heard
/// "Dyno V3" (2 Nov, five sentences, three of them sent to the lighter voice). The letter runs sound alike (a "y" as
/// a vowel is an "i"), the digit runs are equal and in the same places.
fn compound_alike(expected: &str, actual: &str) -> bool {
    let shape = |w: &str| -> Option<(Vec<bool>, String, String)> {
        let mut kinds = Vec::new();
        let (mut numbers, mut letters) = (String::new(), String::new());
        for (k, c) in w.chars().enumerate() {
            if c.is_ascii_digit() {
                numbers.push(c);
            } else if c.is_ascii_lowercase() {
                letters.push(if c == 'y' && k > 0 { 'i' } else { c });
            } else {
                return None;
            }
            let digit = c.is_ascii_digit();
            if kinds.last() != Some(&digit) {
                kinds.push(digit);
                if digit { numbers.push('|') }
            }
        }
        Some((kinds, numbers, letters))
    };
    match (shape(expected), shape(actual)) {
        (Some((k1, n1, l1)), Some((k2, n2, l2))) => {
            expected != actual && !n1.is_empty() && k1 == k2 && n1 == n2 && l1.len() >= 3
                && matches!((sound(&l1), sound(&l2)), (Some(x), Some(y)) if x == y)
        }
        _ => false,
    }
}
/// Two plain words heard as one word of about their joined sound ("main Heimdall" as
/// "Mainheimdahl", 25 Oct): a mishearing (`misheard`), counted as one.
/// `heard`: the parts are what was heard (one expected word heard as two), else the expected words.
fn fused(parts: &[String], whole: &str, names: &HashSet<String>, heard: bool) -> bool {
    // Each part a word of three letters or more: "my workers finish" heard "my work is finished" changed the words.
    parts.len() == 2 && !parts.iter().any(|p| protected(p) || contrast(p) || p.chars().count() < 3)
        && if heard { misheard(whole, &parts.concat(), names) } else { misheard(&parts.concat(), whole, names) }
}
/// The most words one compound may be heard as ("all for one").
const MAX_PARTS: usize = 4;
/// Words the recognizer writes for one another: homophones, and a word said in full that it writes short.
const SAME_SPEECH: [(&str, &str); 25] = [
    // Too short to sound out: "a full model add" heard "model ad" (2 Nov 02:49, all three takes).
    ("add", "ad"),
    ("i", "eye"),
    ("you", "u"),
    ("to", "too"),
    // "two" is a digit by now (`words`): "hide those too" heard "those two" (24 Oct 01:47, all three takes).
    ("to", "2"),
    ("too", "2"),
    // Written short by the recognizer (24 Oct: "Control Command M" heard "Ctrl Command M" on every take).
    ("control", "ctrl"),
    // The letter X said as a word ("X Premium" heard "ex premium", 24 Oct 00:42).
    ("x", "ex"),
    ("there", "their"),
    ("see", "sea"),
    ("be", "bee"),
    ("right", "write"),
    ("hear", "here"),
    ("versus", "vs"),
    // 24 Oct 12:19 and 12:20: each heard alike on two takes before the take was kept.
    ("passed", "past"),
    ("plain", "plane"),
    // 26 Oct: "won one and one was a tie" heard "1-1 and 1" on three takes; "your OK" heard "okay" (twice).
    ("won", "1"),
    ("ok", "okay"),
    // 25-26 Oct, 7 takes: one sound, two spellings.
    ("queued", "cued"),
    ("queue", "cue"),
    ("route", "root"),
    ("male", "mail"),
    // 31 Oct to 3 Nov, each rejected on two takes before the third was kept: "the worst wait" heard "weight", "it
    // heard back" heard "herd", and in Jarvis's British speech (no "r" before a consonant) "the cause" heard "cores".
    ("wait", "weight"),
    ("heard", "herd"),
    ("cause", "cores"),
];
fn listed(expected: &str, actual: &str) -> bool {
    SAME_SPEECH.iter().any(|&(x, y)| (expected == x && actual == y) || (expected == y && actual == x))
}
/// The stem and clitic of a contraction: "it'll" is "it" and "ll". Negations ("isn't") are `protected`, not here.
fn contraction(word: &str) -> Option<(&str, &str)> {
    let (stem, clitic) = word.split_once('\'')?;
    (!stem.is_empty() && !clitic.is_empty() && !clitic.contains('\'')).then_some((stem, clitic))
}
/// What a clitic is said as in full ("'ll" is "will", "'s" is "is" or "has").
fn clitic_words(clitic: &str) -> &'static [&'static str] {
    match clitic {
        "ll" => &["will"],
        "ve" => &["have"],
        "d" => &["would", "had"],
        "re" => &["are"],
        "m" => &["am"],
        "s" => &["is", "has"],
        _ => &[],
    }
}
/// A contraction and its stem or clitic written apart: "I've" heard "I", "it's" heard "it" or "its", "it'll" heard
/// "will". The clitic "ll" is never dropped from its stem ("I'll" heard "I" would lose the future).
fn contraction_alike(expected: &str, actual: &str) -> bool {
    let stripped = |word: &str| word.replace('\'', "");
    if (expected.contains('\'') || actual.contains('\'')) && stripped(expected) == stripped(actual) {
        return true;
    }
    [(expected, actual), (actual, expected)].iter().any(|&(whole, part)| {
        contraction(whole).is_some_and(|(stem, clitic)| {
            (part == stem && clitic != "ll") || (clitic == "ll" && part == "will")
        })
    })
}
/// A word in British spelling written the American way, or back: "judgement" heard "judgment" (23 and 24 Oct, on
/// nearly every take: two retries before the take was kept), "centre", "offence", "colour", "organise", "travelled".
fn regional(expected: &str, actual: &str) -> bool {
    fn american(word: &str) -> String {
        let mut w = word.replace("dgement", "dgment");
        for (british, us) in [("isation", "ization"), ("ising", "izing"), ("ised", "ized"), ("ise", "ize"), ("ises", "izes"), ("yse", "yze"), ("ysed", "yzed"), ("ence", "ense"), ("tre", "ter"), ("tres", "ters"), ("ogue", "og"), ("lled", "led"), ("lling", "ling")] {
            // A doubled "l" only after an unstressed "e" ("travelled", "cancelling"), never "stalled" as "staled".
            let doubled = british.starts_with("ll");
            if let Some(stem) = w.strip_suffix(british).filter(|stem| stem.len() >= 3 && (!doubled || (stem.len() >= 4 && stem.ends_with('e')))) {
                w = format!("{stem}{us}");
                break;
            }
        }
        for ending in ["our", "ours", "oured", "ouring"] {
            if let Some(stem) = w.strip_suffix(ending).filter(|stem| stem.len() >= 3) {
                return format!("{stem}{}", ending.replacen("our", "or", 1));
            }
        }
        w
    }
    let plain = |w: &str| w.chars().count() >= 5 && w.chars().all(char::is_alphabetic) && !protected(w);
    expected != actual && plain(expected) && plain(actual) && american(expected) == american(actual)
}
/// A plural written with its singular stem: "copy" heard "copies".
fn inflection(expected: &str, actual: &str) -> bool {
    let plural = |singular: &str, other: &str| {
        singular.len() >= 3 && singular.ends_with('y') && other == format!("{}ies", &singular[..singular.len() - 1])
    };
    plural(expected, actual) || plural(actual, expected)
}
/// The names of the expected text, lower case: a capitalised word that is not the first of its sentence, or a
/// mixed-case word such as "vLLM". A capitalised first word is kept as "^word": it may be a name, so it gets the
/// stricter test in `name_alike`. The recognizer cannot know names, so it spells them by sound.
fn proper_nouns(text: &str) -> HashSet<String> {
    let mut names = HashSet::new();
    let mut sentence_start = true;
    for token in text.split_whitespace() {
        let word: String = token.chars().filter(|c| c.is_alphanumeric() || *c == '\'').collect();
        let capital = word.chars().next().is_some_and(char::is_uppercase);
        let inside = word.chars().skip(1).any(char::is_uppercase);
        if inside || (capital && !sentence_start) {
            names.insert(word.to_lowercase());
        } else if capital {
            names.insert(format!("^{}", word.to_lowercase()));
        }
        sentence_start = token.ends_with(['.', '?', '!']);
    }
    names
}
/// A name heard as another spelling of its sound: the same first letter and at most one sound apart, or, for a name
/// that is not the first word of its sentence, the same first two letters and at most two sounds apart ("Shure"
/// heard "Shaw", "Franquen" heard "Franken"). "Quim" for "Qwen" is not the same name.
fn name_alike(expected: &str, actual: &str, names: &HashSet<String>) -> bool {
    let inside = names.contains(expected);
    let first = names.contains(&format!("^{expected}"));
    if !(inside || first) || expected.chars().count() < 4 || actual.chars().count() < 4 {
        return false;
    }
    // A soft "g" is a "j" ("Jina" heard "Gina" and "Gena", 3 Nov 03:11: two sentences, three takes).
    let soft = |w: &str| match w.strip_prefix('g') {
        Some(rest) if rest.starts_with(['e', 'i', 'y']) => format!("j{rest}"),
        _ => w.to_owned(),
    };
    let (expected, actual) = (&soft(expected)[..], &soft(actual)[..]);
    if expected.chars().next() != actual.chars().next() {
        return false;
    }
    let (Some(x), Some(y)) = (sound(expected), sound(actual)) else {
        return false;
    };
    let distance = edit_distance(&x, &y);
    distance <= 1 || (inside && distance <= 2 && expected.chars().take(2).eq(actual.chars().take(2)))
}
fn edit_distance(a: &str, b: &str) -> usize {
    let (x, y): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut row: Vec<usize> = (0..=y.len()).collect();
    for (i, cx) in x.iter().enumerate() {
        let mut previous = row[0];
        row[0] = i + 1;
        for (j, cy) in y.iter().enumerate() {
            let next = (previous + usize::from(cx != cy)).min(row[j] + 1).min(row[j + 1] + 1);
            previous = row[j + 1];
            row[j + 1] = next;
        }
    }
    row[y.len()]
}
/// The same letters split into words differently: "Qwen3 TTS" heard "Qwen 3TTS". Numbers and negations are never
/// respaced; the words must hold at least four letters.
fn respaced(expected: &[String], actual: &[String]) -> bool {
    let (left, right) = (expected.concat(), actual.concat());
    left == right
        && left.chars().filter(|c| c.is_alphabetic()).count() >= 4
        && expected.iter().chain(actual).all(|w| !digits(w) && !(protected(w) && w.chars().all(char::is_alphabetic)))
}
/// A currency word said after its amount: the recognizer writes "$20" for "twenty" in a list where the unit is said once
/// ("twelve and twenty-five dollars" heard "$12 and $25"). The amount is the same, so the unit is free.
fn unit_after_number(words: &[String], at: usize) -> bool {
    // Also after a spelled magnitude: "70 billion a month" heard "$70 billion a month" (2 Nov 15:46, two takes).
    let amount = at >= 1
        && (digits(&words[at - 1]) || (at >= 2 && digits(&words[at - 2]) && matches!(words[at - 1].as_str(), "million" | "billion")));
    amount && matches!(words[at].as_str(), "dollar" | "euro" | "pound" | "złoty")
}
/// The spelling the recognizer may use for the same spoken word, if any.
fn spelling_alike(expected: &str, actual: &str, names: &HashSet<String>) -> Option<&'static str> {
    if listed(expected, actual) {
        Some("homophone")
    } else if contraction_alike(expected, actual) {
        Some("contraction")
    } else if inflection(expected, actual) {
        Some("inflection")
    } else if regional(expected, actual) {
        Some("regional")
    } else if name_alike(expected, actual, names) {
        Some("name")
    } else {
        None
    }
}
pub fn check(expected: &str, transcript: &str) -> Value {
    let a = words(expected);
    let b = words(transcript);
    if a.is_empty() || a.len() > 256 || b.len() > 256 {
        return json!({"accepted":false,"reason":"empty or oversized word sequence"});
    }
    // A currency word after its amount is free in the alignment (`unit_after_number`), so it is no repeat either: "650
    // dollars a day or nearly 20,000 a month" heard "$650 a day or nearly $20,000 a month" has "dollar a" twice (24 Oct
    // 03:23 and 23 Oct 01:25: all three takes rejected as repeated).
    let spoken = |w: &[String]| (0..w.len()).filter(|&k| !unit_after_number(w, k) && w[k] != RANGE).map(|k| w[k].clone()).collect::<Vec<_>>();
    // Edit distance in words, where the recognizer's other spellings of what was said cost nothing: a compound
    // written as one word or up to four ("pull-down"/"pulldown", "13 51"/"1351", "AllForOne"/"all for one"), a Roman
    // numeral, an inflected ending (`near`) and a word that sounds the same (`sounds_alike`: "cache"/"cash"). Measured
    // 18–19 Oct: each of these rejected all three Jarvis takes of a correctly spoken sentence and switched the user's
    // reply to the lighter voice.
    let names = proper_nouns(expected);
    let mut costs = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    // A `RANGE` is free here too: "4-1, what a game" heard "1, what a game" dropped one word, not two.
    for i in 1..=a.len() {
        costs[i][0] = costs[i - 1][0] + usize::from(a[i - 1] != RANGE);
    }
    for j in 1..=b.len() {
        costs[0][j] = costs[0][j - 1] + usize::from(b[j - 1] != RANGE);
    }
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            let free = same(&a[i - 1], &b[j - 1])
                || near(&a[i - 1], &b[j - 1])
                || sounds_alike(&a[i - 1], &b[j - 1])
                || foreign_alike(&a[i - 1], &b[j - 1])
                || spelling_alike(&a[i - 1], &b[j - 1], &names).is_some();
            let mut cost = (costs[i - 1][j - 1] + usize::from(!free))
                .min(costs[i - 1][j] + 1)
                .min(costs[i][j - 1] + 1);
            for k in 2..=MAX_PARTS {
                if i >= k && (joined(&a[i - k..i], &b[j - 1]) || fused(&a[i - k..i], &b[j - 1], &names, false)) {
                    cost = cost.min(costs[i - k][j - 1]);
                }
                if j >= k && (joined(&b[j - k..j], &a[i - 1]) || fused(&b[j - k..j], &a[i - 1], &names, true)) {
                    cost = cost.min(costs[i - 1][j - k]);
                }
                for m in 2..=MAX_PARTS {
                    if i >= k && j >= m && respaced(&a[i - k..i], &b[j - m..j]) {
                        cost = cost.min(costs[i - k][j - m]);
                    }
                }
            }
            if unit_after_number(&b, j - 1) || b[j - 1] == RANGE {
                cost = cost.min(costs[i][j - 1]);
            }
            if a[i - 1] == RANGE {
                cost = cost.min(costs[i - 1][j]);
            }
            costs[i][j] = cost;
        }
    }
    let (mut i, mut j) = (a.len(), b.len());
    let (mut insertions, mut deletions, mut substitutions) = (0, 0, 0);
    let mut critical = false;
    let mut harmless = true;
    let mut edits = Vec::new();
    let mut spellings = Vec::new();
    let mut positions = vec![None; b.len()];
    // The final expected word is the final word heard: exactly, as a compound or a numeral, or as an ending that is
    // not shorter than the expected one.
    let mut last_word = None;
    // The final word was heard as another word (not cut): `keepable` may still keep such a take.
    let mut last_misheard = false;
    // Words heard as another spelling of their sound (`misheard`): a few pass.
    let mut misheard_words = 0;
    // Of them, substitutions (counted in `substitutions` too).
    let mut misheard_subs = 0;
    while i > 0 || j > 0 {
        let here = costs[i][j];
        let step = if i > 0 && j > 0 && same(&a[i - 1], &b[j - 1]) && here == costs[i - 1][j - 1] {
            if a[i - 1] != b[j - 1] {
                let kind = if a[i - 1] == "m" || b[j - 1] == "m" { "abbreviation" } else { "numeral" };
                spellings.push(json!({"kind":kind,"expected":a[i-1],"actual":b[j-1]}));
            }
            positions[j - 1] = Some(i - 1);
            i -= 1;
            j -= 1;
            true
        } else if i > 0 && j > 0 && spelling_alike(&a[i - 1], &b[j - 1], &names).is_some() && here == costs[i - 1][j - 1] {
            let kind = spelling_alike(&a[i - 1], &b[j - 1], &names).unwrap_or("spelling");
            spellings.push(json!({"kind":kind,"expected":a[i-1],"actual":b[j-1]}));
            positions[j - 1] = Some(i - 1);
            i -= 1;
            j -= 1;
            true
        } else if i > 0 && a[i - 1] == RANGE && here == costs[i - 1][j] {
            i -= 1;
            true
        } else if j > 0 && b[j - 1] == RANGE && here == costs[i][j - 1] {
            j -= 1;
            true
        } else if j > 0 && unit_after_number(&b, j - 1) && here == costs[i][j - 1] {
            spellings.push(json!({"kind":"unit","actual":b[j-1]}));
            j -= 1;
            true
        } else if let Some(k) = (2..=MAX_PARTS).find(|&k| i >= k && j > 0 && (joined(&a[i - k..i], &b[j - 1]) || fused(&a[i - k..i], &b[j - 1], &names, false)) && here == costs[i - k][j - 1]) {
            let exact = joined(&a[i - k..i], &b[j - 1]);
            misheard_words += usize::from(!exact);
            spellings.push(json!({"kind":if exact { "joined" } else { "fused" },"expected":a[i-k..i].join(" "),"actual":b[j-1]}));
            positions[j - 1] = Some(i - 1);
            i -= k;
            j -= 1;
            true
        } else if let Some(k) = (2..=MAX_PARTS).find(|&k| i > 0 && j >= k && (joined(&b[j - k..j], &a[i - 1]) || fused(&b[j - k..j], &a[i - 1], &names, true)) && here == costs[i - 1][j - k]) {
            let exact = joined(&b[j - k..j], &a[i - 1]);
            misheard_words += usize::from(!exact);
            spellings.push(json!({"kind":if exact { "split" } else { "fused" },"expected":a[i-1],"actual":b[j-k..j].join(" ")}));
            for position in &mut positions[j - k..j] {
                *position = Some(i - 1);
            }
            i -= 1;
            j -= k;
            true
        } else if let Some((k, m)) = (2..=MAX_PARTS)
            .flat_map(|k| (2..=MAX_PARTS).map(move |m| (k, m)))
            .find(|&(k, m)| i >= k && j >= m && respaced(&a[i - k..i], &b[j - m..j]) && here == costs[i - k][j - m])
        {
            spellings.push(json!({"kind":"respaced","expected":a[i-k..i].join(" "),"actual":b[j-m..j].join(" ")}));
            for (t, position) in positions[j - m..j].iter_mut().enumerate() {
                *position = Some(i - k + t.min(k - 1));
            }
            i -= k;
            j -= m;
            true
        } else if i > 0 && j > 0 && sounds_alike(&a[i - 1], &b[j - 1]) && here == costs[i - 1][j - 1] {
            // The same sound spelled another way: the whole word was said.
            spellings.push(json!({"kind":"sound","expected":a[i-1],"actual":b[j-1]}));
            positions[j - 1] = Some(i - 1);
            i -= 1;
            j -= 1;
            true
        } else if i > 0 && j > 0 && foreign_alike(&a[i - 1], &b[j - 1]) && here == costs[i - 1][j - 1] {
            spellings.push(json!({"kind":"foreign","expected":a[i-1],"actual":b[j-1]}));
            positions[j - 1] = Some(i - 1);
            i -= 1;
            j -= 1;
            true
        } else if i > 0 && j > 0 && near(&a[i - 1], &b[j - 1]) && here == costs[i - 1][j - 1] {
            spellings.push(json!({"kind":"ending","expected":a[i-1],"actual":b[j-1]}));
            // As the final word, a shorter spelling is a cut tail ("processes" heard as "process").
            let complete = b[j - 1].chars().count() >= a[i - 1].chars().count();
            positions[j - 1] = Some(i - 1);
            i -= 1;
            j -= 1;
            complete
        } else if i > 0 && j > 0 && here == costs[i - 1][j - 1] + 1 {
            // Heard as a whole word of about its length, never a noise word or a stub: "…press done" cut to
            // "…press the" is a cut, not a mishearing.
            // As the final word, never shorter: "models" heard "model" is a cut tail (review, night 1027).
            let whole = !noise(&b[j - 1]) && b[j - 1].chars().count() >= a[i - 1].chars().count();
            last_misheard |= last_word.is_none() && whole;
            let alike = misheard(&a[i - 1], &b[j - 1], &names);
            misheard_words += usize::from(alike);
            misheard_subs += usize::from(alike);
            substitutions += 1;
            critical |= protected(&a[i - 1]) || protected(&b[j - 1]);
            harmless &= (noise(&a[i - 1]) && noise(&b[j - 1])) || alike;
            edits.push(json!({"kind":"substitution","expected":a[i-1],"actual":b[j-1],"misheard":alike}));
            // Aligned, for the repeat check: "Claude Code … Claude Code" heard "clawed code" twice is no repeat.
            positions[j - 1] = Some(i - 1);
            i -= 1;
            j -= 1;
            // The final word heard as another spelling of its sound, not shorter: said.
            alike && whole
        } else if i > 0 && here == costs[i - 1][j] + 1 {
            deletions += 1;
            critical |= protected(&a[i - 1]);
            harmless &= noise(&a[i - 1]);
            edits.push(json!({"kind":"deletion","expected":a[i-1]}));
            i -= 1;
            false
        } else {
            insertions += 1;
            critical |= protected(&b[j - 1]);
            harmless &= noise(&b[j - 1]);
            edits.push(json!({"kind":"insertion","actual":b[j-1]}));
            j -= 1;
            false
        };
        last_word.get_or_insert(step);
    }
    let last_word = last_word.unwrap_or(false);
    // Repeats are judged on what was heard where it aligns: each heard word stands for the expected word it was heard
    // as (one expected word heard as several counts once), so only words with no place in the sentence can repeat.
    // Comparing raw heard words rejected "Claude Code … Claude Code" heard "clawed code" twice (25 Oct, 7 takes).
    let mut mapped: Vec<String> = Vec::new();
    for (k, w) in b.iter().enumerate() {
        match positions[k] {
            Some(at) if k > 0 && positions[k - 1] == Some(at) => {}
            Some(at) => mapped.push(a[at].clone()),
            None => mapped.push(w.clone()),
        }
    }
    let repeat = repeated_run(&spoken(&a), &spoken(&mapped));
    let immediate_repeat = b.windows(2).enumerate().any(|(j, pair)| {
        pair[0] == pair[1]
            && !matches!((positions[j],positions[j+1]),(Some(left),Some(right)) if right==left+1 || right==left || (right==left+2 && a[left+1]==RANGE))
    });
    edits.reverse();
    spellings.reverse();
    let errors = insertions + deletions + substitutions;
    let said = a.iter().filter(|w| *w != RANGE).count().max(1);
    let wer = errors as f64 / said as f64;
    // One dropped or added "a"/"the" never fails a sentence, however short (20 Oct: "The backup is …" heard as
    // "Backup is …" rejected all three takes at 1 in 11 words). Words heard as another spelling of their sound count
    // apart: at most `misheard_limit` of them.
    let slips = errors - misheard_subs;
    let few = (slips as f64 / said as f64 <= MAX_WORD_ERROR_RATE || slips <= 1) && misheard_words <= misheard_limit(said);
    json!({"accepted":last_word && !repeat && !immediate_repeat && !critical && harmless && few,"last_word":last_word,"insertions":insertions,"deletions":deletions,"substitutions":substitutions,"wer":wer,"max_wer":MAX_WORD_ERROR_RATE,"unexpected_repeat":repeat || immediate_repeat,"immediate_repeat":immediate_repeat,"protected_edit":critical,"harmless_edits":harmless,"edits":edits,"misheard_words":misheard_words,"spellings":spellings,"expected_words":said,"last_misheard":last_misheard,"transcript":transcript})
}
/// How many words a take may have heard as another spelling of their sound and still pass: one, two from eight words,
/// three from sixteen.
fn misheard_limit(words: usize) -> usize {
    match words { 0..8 => 1, 8..16 => 2, _ => 3 }
}
/// A plain word the recognizer heard as another spelling of about the same sound: "billed"/"built", "abliterated"/
/// "obliterated", "Sol"/"soul", "fix"/"fixed" (night 1027: of 602 logged rejections, 20-27 Oct, none of the 16 real voice faults was a
/// plain word heard as another; they were dropped words and wrong numbers). The same first letter or first sound (or
/// both a vowel), never a cut start of the word ("centre"/"cent"), and close sounds: a word of four letters or fewer
/// only with the same sound (a name: the same rough sound), a longer one at most one sound apart, two from seven, three
/// from nine. Never a negation, number, unit, ordinal, noise or `contrast` word: those change what was said.
fn misheard(expected: &str, actual: &str, names: &HashSet<String>) -> bool {
    let plain = |w: &str| w.chars().count() >= 2 && w.chars().all(|c| c.is_alphabetic() || c == '\'') && !protected(w) && !contrast(w) && !noise(w);
    if !plain(expected) || !plain(actual) || expected == actual {
        return false;
    }
    // A short stem with its plural or past ending ("fix"/"fixed", "test"/"tests", "bit"/"bits": 39 takes rejected,
    // 20-26 Oct). As the final word the shorter spelling is still a cut (`check`).
    let ending = |stem: &str, longer: &str| stem.chars().count() >= 3 && longer.strip_prefix(stem).is_some_and(|e| matches!(e, "s" | "es" | "ed" | "d"));
    if ending(expected, actual) || ending(actual, expected) {
        return true;
    }
    // A word inside the other at its end, or with a negating start: "available"/"unavailable", "active"/"inactive"
    // say the opposite (review, night 1027: both passed by the vowel start and two sounds apart).
    let (short, long) = if expected.chars().count() <= actual.chars().count() { (expected, actual) } else { (actual, expected) };
    let negated = ["un", "in", "im", "il", "ir", "non", "dis"]
        .iter()
        .filter_map(|p| long.strip_prefix(p))
        .any(|rest| rest == short || (short.chars().count() >= 4 && edit_distance(rest, short) <= 1));
    if long.ends_with(short) || negated {
        return false;
    }
    if expected.starts_with(actual) && actual.chars().count() + 2 <= expected.chars().count() {
        return false;
    }
    let name = names.contains(expected) || names.contains(&format!("^{expected}"));
    if expected.chars().count() <= 4 || actual.chars().count() <= 4 {
        return matches!((sound(expected), sound(actual)), (Some(x), Some(y)) if x == y) || (name && rough(expected) == rough(actual));
    }
    let (x, y) = match (sound(expected), sound(actual)) {
        (Some(x), Some(y)) => (x, y),
        // Letters outside English: their rough sounds.
        _ => (rough(expected), rough(actual)),
    };
    let vowel = |c: Option<char>| c.is_some_and(|c| "aeiouyąęó".contains(c));
    let first = |w: &str| w.chars().next();
    let starts = first(expected) == first(actual) || x.chars().next() == y.chars().next() || (vowel(first(expected)) && vowel(first(actual)));
    let longest = x.chars().count().max(y.chars().count());
    let apart = edit_distance(&x, &y);
    starts && (apart <= 1 || (apart <= 2 && longest >= 7) || (apart <= 3 && longest >= 9))
}
/// The one word a rejected take got wrong, when that was its only fault: a single substitution of an unprotected
/// plain word, the final word right, nothing cut or repeated. `(expected, heard)`.
pub fn lone_substitution(evidence: &Value) -> Option<(String, String)> {
    let clean = evidence["accepted"] == false
        && evidence["last_word"] == true
        && evidence["unexpected_repeat"] == false
        && evidence["protected_edit"] == false
        && evidence["insertions"] == 0
        && evidence["deletions"] == 0
        && evidence["substitutions"] == 1;
    let edit = evidence["edits"].as_array().filter(|e| clean && e.len() == 1)?.first()?;
    let (expected, heard) = (edit["expected"].as_str()?, edit["actual"].as_str()?);
    let plain = |w: &str| !w.is_empty() && w.chars().all(|c| c.is_alphabetic() || c == '\'');
    if !plain(expected) || !plain(heard) || contrast(expected) || contrast(heard) {
        return None;
    }
    // A mishearing of that word, never an unrelated one: the same first letter or first sound ("queued"/"cued"), at
    // most two sounds (or letters, for a word too short to sound out) apart, and about the same length.
    let first = |w: &str| sound(w).and_then(|s| s.chars().next());
    let starts = expected.chars().next() == heard.chars().next() || matches!((first(expected), first(heard)), (Some(x), Some(y)) if x == y);
    let apart = match (sound(expected), sound(heard)) {
        (Some(x), Some(y)) => edit_distance(&x, &y),
        _ => edit_distance(expected, heard),
    };
    let lengths = expected.chars().count().abs_diff(heard.chars().count());
    (starts && apart <= 2 && lengths <= 2).then(|| (expected.to_string(), heard.to_string()))
}
/// Words whose sound-alike is often their opposite or another value ("start"/"stop", "June"/"July"): a take that
/// changes one is never kept by `accept_repeated_lone_word`.
fn contrast(word: &str) -> bool {
    matches!(
        word,
        "on" | "off" | "in" | "out" | "up" | "down" | "yes" | "start" | "stop" | "open" | "close" | "closed" | "more" | "less"
            | "before" | "after" | "with" | "without" | "can" | "will" | "won't" | "now" | "later" | "add" | "added"
            | "monday" | "tuesday" | "wednesday" | "thursday" | "friday" | "saturday" | "sunday"
            | "january" | "february" | "march" | "april" | "may" | "june" | "july" | "august" | "september"
            | "october" | "november" | "december"
    )
}
/// A take rejected only for the same lone word (`lone_substitution`) as an earlier take of the same sentence: two
/// takes with different seeds failed on that word alike, so the recognizer cannot spell it ("ack" heard "act", "at", "AC"
/// on every take, 22 Oct: each switched a correct sentence to the lighter voice). The take is accepted and its
/// evidence names the word. `earlier` holds the lone words of the sentence's rejected takes so far; a new one is added.
pub fn accept_repeated_lone_word(evidence: &mut Value, earlier: &mut Vec<String>) -> bool {
    let Some((expected, heard)) = lone_substitution(evidence) else { return false };
    if !earlier.contains(&expected) {
        earlier.push(expected);
        return false;
    }
    evidence["accepted"] = true.into();
    if let Some(spellings) = evidence["spellings"].as_array_mut() {
        spellings.push(json!({"kind":"repeated","expected":expected,"actual":heard}));
    }
    true
}
/// Words a rejected take may have wrong and still be kept after Jarvis's last retry: one, or two in a sentence of
/// eight words or more.
fn keep_limit(words: usize) -> usize {
    if words >= 8 { 2 } else { 1 }
}
/// Whether Jarvis's rejected take of a sentence is still his to say once every retry is spent, and if so how many
/// words it got wrong (fewer is better). "It changed your voice to the lighter version": every
/// one of that night's switches was a take the recognizer misheard (a name, "Ctrl", "20th", "too" as "two"), never a
/// broken one. A kept take is complete (its final word heard, or heard as another whole word of about its length),
/// repeats nothing, and only has words heard as other words: at most `keep_limit` of them, none of them a negation,
/// number, unit, ordinal or `contrast` word. A dropped or added word is never kept: skipping words is how the voice
/// fails, and the check exists to catch it (review 24 Oct). `_earlier` is the sentence's earlier rejected takes.
pub fn keepable(evidence: &Value, _earlier: &[Value]) -> Option<usize> {
    let edits = evidence["edits"].as_array()?;
    let words = evidence["expected_words"].as_u64()? as usize;
    let complete = evidence["last_word"] == true || evidence["last_misheard"] == true;
    if evidence["unexpected_repeat"] != false || !complete || edits.is_empty() || edits.len() > keep_limit(words) {
        return None;
    }
    edits
        .iter()
        .all(|e| e["kind"] == "substitution" && !meaning_changed(&e["expected"], &e["actual"]))
        .then_some(edits.len())
}
/// Whether a word heard as another may have changed what was said: a negation, number, unit or ordinal on either side,
/// or one `contrast` word heard as another ("start"/"stop", "in"/"on"). A contrast word heard as a plain word is a
/// mishearing ("in Downloads" heard "and downloads" on all three takes, 4 Nov 01:41: the sentence went to the lighter
/// voice).
fn meaning_changed(expected: &Value, actual: &Value) -> bool {
    match (expected.as_str(), actual.as_str()) {
        (Some(x), Some(y)) => protected(x) || protected(y) || (contrast(x) && contrast(y)),
        _ => true,
    }
}
/// Whether the recognizer could hear one word as the other: the same first letter or sound (or both a vowel), at most
/// two sounds apart, and neither the other with a negating start or end cut off ("lock"/"unlock", "abled"/"disabled").
/// "passed" heard "failed" on two takes was said "failed" (review 4 Nov).
fn heard_alike(expected: &str, actual: &str) -> bool {
    let (short, long) = if expected.chars().count() <= actual.chars().count() { (expected, actual) } else { (actual, expected) };
    let negated = ["un", "in", "im", "il", "ir", "non", "dis", "en"].iter().filter_map(|p| long.strip_prefix(p)).any(|rest| rest == short);
    if negated || (long != short && long.ends_with(short) && short.chars().count() >= 3) {
        return false;
    }
    let vowel = |c: Option<char>| c.is_some_and(|c| "aeiouy".contains(c));
    let first = |w: &str| w.chars().next();
    let sounds = (sound(expected), sound(actual));
    // Letters a name may start with by one sound: "Jarovit"/"Yarovit", "Jina"/"Gina", "Vela"/"Wela".
    let class = |c: Option<char>| c.map(|c| match c { 'j' | 'y' | 'g' => 'j', 'v' | 'w' => 'v', 'c' | 'k' | 'q' => 'k', c => c });
    let starts = class(first(expected)) == class(first(actual))
        || matches!(&sounds, (Some(x), Some(y)) if x.chars().next() == y.chars().next())
        || (vowel(first(expected)) && vowel(first(actual)));
    let apart = match sounds {
        (Some(x), Some(y)) => edit_distance(&x, &y),
        _ => edit_distance(expected, actual),
    };
    starts && apart <= 2
}
/// The edits a take was rejected for, without the check's own notes: what two takes must share to agree.
fn edit_set(evidence: &Value) -> Option<Vec<(String, String, String)>> {
    let field = |e: &Value, k: &str| e[k].as_str().unwrap_or("").to_owned();
    Some(evidence["edits"].as_array()?.iter().map(|e| (field(e, "kind"), field(e, "expected"), field(e, "actual"))).collect())
}
/// A take rejected for exactly the same wrong words as an earlier take of the same sentence. Takes are made with
/// different seeds, so the voice's own faults (cut, skipped, repeated words) differ between them; the same words heard
/// the same way twice are how the recognizer writes what was said. 31 Oct to 4 Nov: of 12 sentences sent to the lighter
/// voice, every one was heard alike on all three takes. The take is accepted at once (no third take, no switch) when it
/// is complete, repeats nothing, and its edits are at most `keep_limit` words heard as other words or added, none a
/// negation, number, unit, ordinal or opposite (`meaning_changed`). A dropped word is never agreed: skipping words is
/// how the voice fails. `earlier` holds the sentence's rejected takes so far.
pub fn accept_agreed_take(evidence: &mut Value, earlier: &[Value]) -> bool {
    let complete = evidence["last_word"] == true || evidence["last_misheard"] == true;
    let words = evidence["expected_words"].as_u64().unwrap_or(0) as usize;
    let Some(edits) = edit_set(evidence) else { return false };
    let plain = |w: &str| w.is_empty() || w.chars().all(|c| c.is_alphabetic() || c == '\'');
    let clean = evidence["accepted"] == false
        && complete
        && evidence["unexpected_repeat"] == false
        && evidence["protected_edit"] == false
        && !edits.is_empty()
        && edits.len() <= keep_limit(words)
        && edits.iter().all(|(kind, expected, actual)| {
            plain(expected)
                && plain(actual)
                && match kind.as_str() {
                    "substitution" => !meaning_changed(&json!(expected), &json!(actual)) && heard_alike(expected, actual),
                    // Only a short function word heard twice: an added content word was said.
                    "insertion" => matches!(actual.as_str(), "to" | "a" | "an" | "the" | "and" | "of" | "in" | "it" | "is" | "so" | "as"),
                    _ => false,
                }
        });
    if !clean || !earlier.iter().any(|e| edit_set(e).as_ref() == Some(&edits)) {
        return false;
    }
    evidence["accepted"] = true.into();
    let agreed = json!({"kind":"agreed","edits":evidence["edits"].clone()});
    if let Some(spellings) = evidence["spellings"].as_array_mut() {
        spellings.push(agreed);
    }
    true
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_lone_word_misheard_on_two_takes_is_the_recognizer() {
        let mut earlier = Vec::new();
        let mut first = check("My guess is the new ack playback path.", "My guess is the new AC playback path.");
        assert_eq!(first["accepted"], false);
        assert!(!accept_repeated_lone_word(&mut first, &mut earlier), "one take is not enough");
        let mut second = check("My guess is the new ack playback path.", "My guess is the new act playback path.");
        assert!(accept_repeated_lone_word(&mut second, &mut earlier));
        assert_eq!(second["accepted"], true);
        assert!(second["spellings"].as_array().unwrap().iter().any(|s| s["kind"] == "repeated"), "{second}");
        // "queued" heard "cued" is one sound (`SAME_SPEECH`, night 1027): the first take passes.
        assert_eq!(check("Your note is queued for tonight.", "Your note is cued for tonight.")["accepted"], true);
    }
    #[test]
    fn repeated_lone_word_never_hides_other_faults() {
        let repeated = |expected: &str, heard: &str| {
            let mut earlier = Vec::new();
            let mut v = check(expected, heard);
            accept_repeated_lone_word(&mut v, &mut earlier);
            let mut v = check(expected, heard);
            accept_repeated_lone_word(&mut v, &mut earlier)
        };
        // Two faults, a dropped word, a changed negation or number, a cut last word, an unrelated word.
        assert!(!repeated("The ack clips still wait for tonight.", "The act clip still waits for tonight."));
        assert!(!repeated("The ack clips still wait for tonight.", "The clips still wait for tonight."));
        assert!(!repeated("Qwen3 TTS is ready.", "Qwen3 TTS is not ready."));
        assert!(!repeated("It costs 20 dollars.", "It costs 30 dollars."));
        assert!(!repeated("Mistakes stay visible like calm delay for comm delay.", "Mistakes stay visible like calm delay for comm day."));
        assert!(!repeated("The plan is ready for you.", "The plan is ready the you."));
        // Sound-alikes that change the meaning (review, 22 Oct).
        assert!(!repeated("Start the worker now please.", "Stop the worker now please."));
        assert!(!repeated("It goes with the build tonight.", "It goes without the build tonight."));
        assert!(!repeated("The review is on Tuesday this week.", "The review is on Thursday this week."));
        assert!(!repeated("Leave the light on for me.", "Leave the light off for me."));
        // Tonight's misses are still kept.
        for (expected, heard) in [("Ack say something to me and stop.", "Axe say something to me and stop."),
            ("The ack clips still wait for tonight.", "The at clips still wait for tonight.")] {
            assert!(repeated(expected, heard), "{expected} / {heard}");
        }
        // "wait" heard "weight" is a listed homophone since 4 Nov: accepted on the first take, no repeat needed.
        let v = check("Letting the short acknowledgement skip that wait would help.", "Letting the short acknowledgement skip that weight would help.");
        assert_eq!(v["accepted"], true, "{v}");
        // Different words on the two takes are not a repeat.
        let mut earlier = Vec::new();
        let mut v = check("The worker finished the task.", "The walker finished the task.");
        accept_repeated_lone_word(&mut v, &mut earlier);
        let mut v = check("The worker finished the task.", "The worker finished the tusk.");
        assert!(!accept_repeated_lone_word(&mut v, &mut earlier));
    }
    #[test]
    fn final_word_is_required_even_when_most_words_match() {
        let text = "The worker finished the archive checks and all of the new transcript samples matched the expected output.";
        assert_eq!(
            check(text, &text.replace(" output.", "."))["accepted"],
            false
        );
        assert_eq!(check(text, text)["accepted"], true);
    }
    #[test]
    fn enough_ordered_words_and_duplicate_counts_are_required() {
        assert_eq!(
            check(
                "The worker checked the archive and saved the notes.",
                "notes"
            )["accepted"],
            false
        );
        // One dropped "the" is not a missing word (20 Oct: "The backup is …" heard as "Backup is …"); a dropped
        // content word or a second dropped article is.
        assert_eq!(
            check(
                "The worker checked the archive and saved the notes.",
                "The worker checked the archive and saved notes."
            )["accepted"],
            true
        );
        for heard in ["The worker checked the archive and the notes.", "Worker checked the archive and saved notes."] {
            assert_eq!(check("The worker checked the archive and saved the notes.", heard)["accepted"], false, "{heard}");
        }
        assert_eq!(check("go go go go home", "go home")["accepted"], false);
    }
    #[test]
    fn rejects_new_repeated_runs_but_preserves_repetitions_in_the_text() {
        let r = check("The archive is ready.", "The archive the archive is ready.");
        assert_eq!(r["accepted"], false);
        assert_eq!(r["unexpected_repeat"], true);
        assert_eq!(r["insertions"], 2);
        assert_eq!(
            check(
                "The archive the archive is ready.",
                "The archive the archive is ready."
            )["accepted"],
            true
        );
    }
    #[test]
    fn immediate_word_repeats_require_the_same_aligned_duplicate_in_the_text() {
        let text="The worker will carefully check every part of this recording before moving all of the completed notes into the archive.";
        let r = check(text, &text.replacen("The ", "The the ", 1));
        assert_eq!(r["wer"], 0.05);
        assert_eq!(r["immediate_repeat"], true);
        assert_eq!(r["accepted"], false);
        assert_eq!(
            check("I had had enough.", "I had had enough.")["accepted"],
            true
        );
        assert_eq!(
            check("The the worker is ready.", "The the worker is ready.")["accepted"],
            true
        );
        let r = check(
            "The the worker will check the archive.",
            "The worker will check the the archive.",
        );
        assert_eq!(r["immediate_repeat"], true);
        assert_eq!(r["accepted"], false);
    }
    #[test]
    fn negations_and_numbers_are_protected_even_at_low_word_error_rate() {
        let text = "I will not delete the archive until you approve this change.";
        let r = check(
            text,
            "I will delete the archive until you approve this change.",
        );
        assert_eq!(r["accepted"], false);
        assert_eq!(r["protected_edit"], true);
        assert_eq!(r["deletions"], 1);
        for word in [
            "not",
            "no",
            "never",
            "don't",
            "can't",
            "won't",
            "nie",
            "nigdy",
            "13",
            "thirteen",
            "first",
            "dwieście",
        ] {
            let text=format!("The worker will {word} carefully check every part of this recording before moving all of the completed notes into the archive.");
            for actual in [
                text.replace(&format!(" {word} "), " "),
                text.replace(&format!(" {word} "), " maybe "),
            ] {
                let r = check(&text, &actual);
                assert_eq!(r["accepted"], false, "{word}: {r}");
                assert_eq!(r["protected_edit"], true, "{word}: {r}");
            }
        }
        assert_eq!(
            check("I don't delete files.", "I do not delete files.")["accepted"],
            true
        );
        assert_eq!(
            check("I can't delete files.", "I cannot delete files.")["accepted"],
            true
        );
    }
    #[test]
    fn small_noise_tolerance_never_allows_meaningful_omissions_or_substitutions() {
        let text="The worker will carefully check every part of this recording before moving all of the completed notes into the archive.";
        let harmless = text.replacen("The ", "", 1);
        let r = check(text, &harmless);
        assert_eq!(r["accepted"], true);
        assert_eq!(r["deletions"], 1);
        for actual in [
            text.replace("check ", ""),
            text.replace("check ", "delete "),
            text.replace("check ", "check not "),
        ] {
            assert_eq!(check(text, &actual)["accepted"], false);
        }
    }
    /// The recognizer's transcripts of correctly spoken Jarvis takes that rejected all three attempts on 18–19 Oct and
    /// switched the user's reply to the lighter voice (archive `drama3` sentence_coverage records), plus one that cost a
    /// retry. Each is the same speech written another way.
    #[test]
    fn other_spellings_of_the_same_speech_are_accepted() {
        for (expected, heard) in [
            ("Worth retrying after the rebuild opening an app then circling something a question card a pull down and fast reading.",
             "Worth retrying after the rebuild. Opening an app, then circling something, a question card, a pulldown, and fast reading."),
            ("A circle around Opus 5.", "A circle around Opus V."),
            ("I circled the failed turn at 13 51.", "I circled the failed turn at 1351."),
            ("Mobius found Heimdall using about 38 cores mostly ten speaker comparison processes.",
             "Mobius found Heimdall using about 38 cores, mostly 10 speaker comparison processors."),
            ("Mobius found Heimdall using about 38 cores mostly ten speaker comparison processes.",
             "Mobius found Heimdal using about 38 cores, mostly 10-speaker comparison processors."),
            ("Things I'd file today the repeated Mobius notices bulky JSON results and replies missing from completion notices.",
             "Things I'd file today, the repeated Mobius notice, bulky JSON results and replies missing from completion notices."),
            ("Each icon is one call and the Agent window lists every one.", "Each icon is one call, and the agent window lists everyone."),
            ("Open the pulldown menu.", "Open the pull down menu."),
            ("A circle around Opus 5.5 in my window.", "A circle around Opus 5.5 in my window."),
        ] {
            let r = check(expected, heard);
            assert_eq!(r["accepted"], true, "{expected} / {heard}: {r}");
            assert_eq!(r["wer"], 0.0, "{r}");
            assert!(!r["spellings"].as_array().unwrap().is_empty() || words(expected) == words(heard), "{r}");
        }
    }
    #[test]
    fn other_spellings_never_hide_a_missing_changed_or_cut_word() {
        for (expected, heard) in [
            // A half of a compound is missing, not joined.
            ("Open the pull down menu.", "Open the pull menu."),
            // The final word is cut: a near spelling of the last word is a truncation, not a spelling.
            ("Mobius found ten speaker comparison processes.", "Mobius found ten speaker comparison process."),
            // A different word with the same start.
            ("The worker will check every part of the archive before moving the notes.", "The worker will chop every part of the archive before moving the notes."),
            // Numbers stay exact: another numeral, a Roman numeral of another value, a joined number that differs.
            ("A circle around Opus 5.", "A circle around Opus X."),
            ("A circle around Opus 5.", "A circle around Opus 6."),
            ("I circled the failed turn at 13 51.", "I circled the failed turn at 1352."),
            // Negations stay exact.
            ("I will not delete the archive until you approve this change.", "I will delete the archive until you approve this change."),
            // An inflected protected word is not a spelling.
            ("This is the thirteenth archive check of the night.", "This is the thirteen archive check of the night."),
        ] {
            let r = check(expected, heard);
            assert_eq!(r["accepted"], false, "{expected} / {heard}: {r}");
        }
        assert!(!near("check", "chop") && !near("is", "in") && !near("notes", "note") && near("notices", "notice"));
        assert!(!near("first", "firsts") && !near("13th", "13"));
        assert_eq!((roman("v"), roman("iv"), roman("xii"), roman("iiii"), roman("vx"), roman("mix")), (Some(5), Some(4), Some(12), None, None, None));
    }
    /// 19 Oct 18:14–18:42 on the installed 80ca44e (archive `drama3` sentence_coverage records): each pair rejected all
    /// three correct Jarvis takes and the lighter voice said the sentence; "1,000" cost a retry.
    #[test]
    fn sound_alike_words_and_long_compounds_of_19_oct_are_accepted() {
        for (expected, heard) in [
            ("Splash lacks MTP for this model and vllm metal turns the cache off while drafting.",
             "Splash lacks MTP for this model, and VLLM Metal turns the cash off while drafting."),
            ("Any engine that can't keep the cache on while drafting is marked as failing.",
             "Any engine that can't keep the cash on while drafting is marked as failing."),
            ("The top bar shows the Mac's GPU and memory load plus tokens used today.",
             "The top bar shows the max GPU and memory load plus tokens used today."),
            ("It also has to confirm or refute your bet on DFlash2 and keep the reply short.",
             "It also has to confirm or refute your bet on D Flash 2 and keep the reply short."),
            ("The one thing I held back is deleting AllForOne since that can't be undone.",
             "The one thing I held back is deleting all for one, since that can't be undone."),
            ("It will delete AllForOne's weights build files and caches and archive the code notes and results.",
             "It will delete all for one's weights build files and caches and archive the code notes and results."),
            ("The ones that look good are built like Linear a Vercel dashboard or Apple's widget grid.",
             "The ones that look good are built like Linear Aversel dashboard or Apple's widget grid."),
            ("The ones that look good are built like Linear a Vercel dashboard or Apple's widget grid.",
             "The ones that look good are built like linear avercell dashboard, or Apple's widget grid."),
            ("It will test giving the decider the last 200 400 800 and 1000 characters to see which length wakes me correctly.",
             "It will test giving the decider the last 200, 400, 800 and 1,000 characters to see which length wakes me correctly."),
            ("I checked the site.", "I checked the sight."),
        ] {
            let r = check(expected, heard);
            assert_eq!(r["accepted"], true, "{expected} / {heard}: {r}");
            assert_eq!(r["wer"], 0.0, "{r}");
        }
        assert_eq!(sound("cache"), sound("cash"));
        assert_eq!(sound("mac's"), sound("max"));
        assert_eq!(sound("vercel"), sound("versal"));
        assert_eq!(words("1,000 and 12,500,000 but 1,00 and 1,0000"), ["1000", "and", "12500000", "but", "1", "00", "and", "1", "0000"]);
    }
    /// Sounding alike never hides a different word, a number or a negation, and a compound must be the same letters.
    #[test]
    fn sound_alike_and_compounds_never_hide_a_changed_word() {
        for (expected, heard) in [
            // A different vowel is a different word.
            ("The card is on the table now.", "The cord is on the table now."),
            ("I will accept the change for you.", "I will except the change for you."),
            ("The worker will check every part of the archive before moving the notes.", "The worker will chalk every part of the archive before moving the notes."),
            // Kept strict on 19 Oct: the recognizer heard other words, not other spellings.
            ("I'll hear back on my own when my workers finish or when any thread is waiting on a question or approval.",
             "I'll hear back on my own when my work is finished or when any thread is waiting on a question or approval."),
            ("Want me to have a thread build it in this style?", "Want me to have a thread builder in this style."),
            // Numbers and negations are never compared by sound or joined into another value.
            ("It also has to confirm or refute your bet on DFlash2 and keep the reply short.",
             "It also has to confirm or refute your bet on D Flash 3 and keep the reply short."),
            ("There is no reason to wait for it now.", "There is know reason to wait for it now."),
            ("I will not ever delete the archive for you.", "I will never delete the archive for you."),
            ("It will test the last 1000 characters to see which length wakes me.", "It will test the last 1,001 characters to see which length wakes me."),
            // A compound with a part missing is not the compound.
            ("The one thing I held back is deleting AllForOne today.", "The one thing I held back is deleting all for today."),
        ] {
            let r = check(expected, heard);
            assert_eq!(r["accepted"], false, "{expected} / {heard}: {r}");
        }
        for (x, y) in [("no", "know"), ("not", "knot"), ("13", "thirteen"), ("bat", "but"), ("the", "them"), ("free", "freed")] {
            assert!(!sounds_alike(x, y), "{x}/{y}");
        }
        assert!(!joined(&["all".into(), "for".into(), "1".into(), "x".into(), "y".into()], "allfor1xy"), "at most four parts");
    }
    /// 20 Oct 18:35–19:20 on the installed 4b9117a (archive `drama3` sentence_coverage records): each heard take
    /// rejected all three Jarvis takes of a correctly spoken sentence (or cost retries) and moved it to the lighter
    /// voice. Each is the same speech written another way: a number in digits, a unit or currency symbol, a doubled
    /// letter, a Polish name in English spelling, a dropped "the".
    #[test]
    fn numbers_units_currency_and_names_of_20_oct_are_accepted() {
        for (expected, heard) in [
            ("The best fit is a 73 gigabyte quant on oMLX with its own multi token prediction head.",
             "The best fit is a 73GB Quanton oMLX with its own multi-token prediction head."),
            ("The best fit is a 73 gigabyte quant on oMLX with its own multi token prediction head.",
             "The best fit is a 73GB quant on oMLX with its own multi-token prediction head."),
            ("Say twenty dollars a month hosted models and agent runs included so no API keys.",
             "Say $20 a month, hosted models and agent runs included, so no API keys."),
            ("The pitch is moments like today when it researched Ornith in the background and caught the benchmaxxing on its own.",
             "The pitch is moments like today, when it researched Ornith in the background and caught the benchmaxing on its own."),
            ("Finally it builds a list of names that work as a spoken wake word with trademark and domain checks including Heimdall and Światowid.",
             "Finally, it builds a list of names that work as a spoken wake word with trademark and domain checks, including Heimdall and Schwiatovid."),
            ("The model is a small six hundred million parameter omni model fine tuned for Heimdall so it could take audio and images.",
             "The model is a small 600 million parameter Omni model, fine-tuned for Heimdal, so it could take audio and images."),
            ("The model is a small six hundred million parameter omni model fine tuned for Heimdall so it could take audio and images.",
             "The model is a small 600m parameter Omni model fine-tuned for Heimdall, so it could take audio and images."),
            ("The backup is a small Qwen model with a cleaner Apache licence.", "Backup is a small Qwen model with a cleaner Apache license."),
            // The same forms the other way round and in other amounts.
            ("It costs $20 a month.", "It costs twenty dollars a month."),
            ("Raise it to $1.5 billion.", "Raise it to one point five billion dollars."),
            ("About $20k a year.", "About twenty thousand dollars a year."),
            ("A hundred and five tests passed.", "105 tests passed."),
            ("Two hundred twenty one thousand rows.", "221,000 rows."),
            ("Opus 5.5 is faster.", "Opus five point five is faster."),
            ("Only ten percent of it.", "Only 10% of it."),
            ("Ten milliseconds is the budget.", "10 ms is the budget."),
            ("The run took 600 meters of cable.", "The run took 600m of cable."),
            ("Faktura na sześćset złotych.", "Faktura na 600 zł."),
            ("Ship v1.2 of the widget today.", "Ship v 1.2 of the widget today."),
            // 22 Oct 21:31: digit groups split by a space, as the agent writes them.
            ("That group draws about 86 000 kroner a year in benefits against 59 000 for Danes.",
             "That group draws about 86,000 krona a year in benefits against 59,000 for Danes."),
            ("Kosztuje 1 000 000 złotych.", "Kosztuje 1,000,000 złotych."),
        ] {
            let r = check(expected, heard);
            assert_eq!(r["accepted"], true, "{expected} / {heard}: {r}");
        }
        assert_eq!(words("$20"), ["20", "dollar"]);
        assert_eq!(words("$1.5 billion"), ["1", "5", "billion", "dollar"]);
        assert_eq!(words("six hundred million"), ["600", "million"]);
        assert_eq!(words("600m 73GB 20k 122B"), ["600", "m", "73", "gigabyte", "20000", "122", "billion"]);
        assert_eq!(words("a hundred and five, twenty one thousand"), ["105", "21000"]);
        assert_eq!(words("v1.2 and 1.2.3"), ["v1", "2", "and", "1", "2", "3"]);
        assert_eq!(words("86 000, 86\u{a0}000, 86\u{202f}000 and 1 000 000"), ["86000", "86000", "86000", "and", "1000000"]);
        assert_eq!(words("In 2026 100 people came, 5 0001 and 12 34."), ["in", "2026", "100", "people", "came", "5", "0001", "and", "12", "34"]);
        // Letter-digit names stay whole, and abbreviations count only after a number.
        assert_eq!(words("M5 DFlash2 the 21st plan B ms"), ["m5", "dflash2", "the", "21st", "plan", "b", "ms"]);
        assert_eq!(sound("benchmaxxing"), sound("benchmaxing"));
        assert_eq!(sound("licence"), sound("license"));
    }
    /// The new spellings never hide a changed amount, a dropped or swapped unit, a different word or a cut name.
    #[test]
    fn numbers_units_currency_and_names_still_catch_real_failures() {
        for (expected, heard) in [
            ("Say twenty dollars a month for hosted models.", "Say $30 a month for hosted models."),
            ("Say twenty dollars a month for hosted models.", "Say twenty a month for hosted models."),
            ("Say twenty dollars a month for hosted models.", "Say €20 a month for hosted models."),
            ("The best fit is a 73 gigabyte quant on oMLX.", "The best fit is a 73MB quant on oMLX."),
            ("The best fit is a 73 gigabyte quant on oMLX.", "The best fit is a 37GB quant on oMLX."),
            ("The best fit is a 73 gigabyte quant on oMLX.", "The best fit is a 73 quant on oMLX."),
            ("The model is a small six hundred million parameter model.", "The model is a small 600 billion parameter model."),
            ("The model is a small six hundred million parameter model.", "The model is a small 60 million parameter model."),
            ("The model is a small six hundred million parameter model.", "The model is a small 6 million parameter model."),
            ("Two hundred rows are left in the table.", "200,000 rows are left in the table."),
            ("They draw about 86 000 kroner a year.", "They draw about 86 kroner a year."),
            ("They draw about 86 000 kroner a year.", "They draw about 68,000 kroner a year."),
            ("Opus 5.5 is faster than the last one.", "Opus 5 is faster than the last one."),
            ("Only ten percent of it was used.", "Only 10 of it was used."),
            // A cut or different name.
            ("The names include Heimdall and Światowid.", "The names include Heimdall and Schwiat."),
            ("The names include Heimdall and Światowid.", "The names include Heimdall and Sweden."),
            // One content word missing, or two articles, is not a dropped "the".
            ("The backup is a small Qwen model with a cleaner Apache licence.", "The backup is a Qwen model with a cleaner Apache licence."),
            ("The backup is a small Qwen model with a cleaner Apache licence.", "Backup is small Qwen model with a cleaner Apache licence."),
        ] {
            let r = check(expected, heard);
            assert_eq!(r["accepted"], false, "{expected} / {heard}: {r}");
        }
        // A foreign match needs a letter outside English in the expected word: plain English stays exact.
        assert!(foreign_alike("światowid", "schwiatovid") && !foreign_alike("received", "relieved") && !foreign_alike("światowid", "swiat"));
    }
    #[test]
    fn punctuation_case_apostrophes_and_numbers_are_shared() {
        assert_eq!(
            check("It's ready User.", "IT’S READY, USER!")["accepted"],
            true
        );
        assert_eq!(words("twenty eight"), words("28"));
        let expected =
            "Faktura jest do dwudziestego ósmego października czyli masz jeszcze trzynaście dni.";
        assert_eq!(
            check(
                expected,
                "Faktura jest do 28 października, czyli masz jeszcze 13 dni."
            )["accepted"],
            true
        );
        assert_eq!(check("Around eleven.", "Around 11.")["accepted"], true);
        assert_eq!(check("Wait a second.", "Wait a two.")["accepted"], false);
    }
}

/// Rejections from `heimdall.log` ("Jarvis take rejected by the word check", 20–21 Oct), each as the expected sentence
/// and the transcript the recognizer heard. The log keeps only the differing words, so the expected sentence is
/// rebuilt from those differences and the heard text; where the log showed a single heard line it is that line.
#[cfg(test)]
mod night_log_tests {
    use super::*;

    /// Same speech written another way: the recognizer's choice, not Jarvis's. Each must pass.
    const FALSE_ALARMS: &[(&str, &str)] = &[
        // Homophones: "U" for "you", "eye" for "I", "it's" and "shore" and "Shaw" for names and words.
        ("So Heimdall could ship Drama 3 as an optional download, with a voice you design.",
         "So Heimdall could ship Drama 3 as an optional download, with a voice U design."),
        ("Dragging no longer opens or closes them, and only a click or I can open one and it stays open.",
         "Dragging no longer opens or closes them, and only a click or eye can open one and it stays open."),
        ("It will not be a problem, I think.", "It will not be a problem, eye think."),
        ("Ask the team to see the sea.", "Ask the team to sea the see."),
        // Contractions: a dropped or added clitic, and "I've" heard "I have" or "I".
        ("We could try designing that voice tonight, and you'd judge it by ear.",
         "We could try designing that voice tonight, and you judge it by ear."),
        ("I've added this to the prototype and it'll log why it switched each time.",
         "I've added this to the prototype and it'll log why it's switched each time."),
        ("I'd combine that with simpler clues.", "I combine that with simpler clues."),
        ("It'll try the most natural-sounding ones like Qwen 3 TTS, Orpheus, Sesame Dia, Vibe Voice, and Index TTS2, each with its best voice.",
         "It will try the most natural-sounding ones like Qwen 3 TTS, Orpheus, Sesame Dia, Vibe Voice, and Index TTS2, each with its best voice."),
        ("I've also had the rule removed from my instructions.", "I have also had the rule removed from my instructions."),
        ("I've filed a bug note about it.", "I filed a bug note about it."),
        ("I've told the prototype thread to keep that possible for later.",
         "I have told the prototype thread to keep that possible for later."),
        // Names heard in another spelling: "Shure" as "Shaw" or "shore", "Franquen" as "Franken", "vLLM" as "Valm",
        // "Aiden" as "Aden".
        ("The Shure MV7i was switched off, and AirPods Pro are now your main mic.",
         "The Shaw MV7i was switched off, and AirPods Pro are now your main mic."),
        ("It picked the Shure as the main mic and dropped the AirPods mic by itself, so your line was written only once.",
         "It picked the shore as the main mic and dropped the AirPods mic by itself, so your line was written only once."),
        ("Franquen and Virio fit the local AI story best.", "Franken and Virio fit the local AI story best."),
        ("Franquen was never on GitHub.", "Franken was never on GitHub."),
        ("One leftover the Quendrafter Franquen used is still in your model cache, about three and a half gigabytes.",
         "One leftover the Quendrafter Franken used is still in your model cache about 3.5 gigabytes."),
        ("oMLX came second, and vLLM Metal was far behind because it refuses the drafter on this model.",
         "oMLX came second, and Valm Metal was far behind because it refuses the drafter on this model."),
        ("The top one is Qwen3 TTS 1.7B. Try the Aiden voice and the designed voice.",
         "The top one is Qwen 3TTS 1.7B. Try the Aden voice and the designed voice."),
        ("Finally it builds a list of names that work as a spoken wake word with trademark and domain checks, including Heimdall and Światowid.",
         "Finally, it builds a list of names that work as a spoken wake word with trademark and domain checks, including Heimdall and Schwiatovich."),
        // Foreign currency in English spelling.
        ("It pays the best 20 to 28,000 złoty a month, and it's remote or hybrid.",
         "It pays the best 20 to 28,000 zwati a month, and it's remote or hybrid."),
        // Web addresses: "dot" said, written as a bare dot.
        ("Its dot ai and dot app domains are free, and the search found no conflicts.",
         "Its .ai and .app domains are free, and the search found no conflicts."),
        // Duplicated or merged words: "DFlash" heard as "D Flash", a plural written singular, a unit said once for a list.
        ("DFlash 2 versus MTP never got tested because these weights have no MTP head.",
         "Dflash 2 vs MTP never got tested because these weights have no MTP head."),
        ("Clipboard copy something, and a pill offers to translate, summarize or save it.",
         "Clipboard copies something, and a pill offers to translate, summarize or save it."),
        ("It suggests a free local version, plus paid plans at twelve and twenty five dollars a month.",
         "It suggests a free local version, plus paid plans at $12 and $25 a month."),
        ("The spend is nineteen dollars sixty five, which matches your twenty.",
         "The spend is $19.65, which matches your $20."),
        ("False wakes cost only one dollar thirty six, about seven percent.",
         "False wakes cost only $1.36, about 7%."),
        // Words the earlier nights already covered (kept as regressions).
        ("The best fit is a 73 gigabyte quant on oMLX with its own multi token prediction head.",
         "The best fit is a 73GB Quanton oMLX with its own multi-token prediction head."),
        ("Say twenty dollars a month hosted models and agent runs included so no API keys.",
         "Say $20 a month, hosted models and agent runs included, so no API keys."),
        ("The pitch is moments like today when it researched Ornith in the background and caught the benchmaxxing on its own.",
         "The pitch is moments like today, when it researched Ornith in the background and caught the benchmaxing on its own."),
        ("The model is a small six hundred million parameter Omni model fine tuned for Heimdall so it could take audio and images.",
         "The model is a small 600 million parameter Omni model, fine-tuned for Heimdal, so it could take audio and images."),
        ("The backup is a small Qwen model with a cleaner Apache licence.", "Backup is a small Qwen model with a cleaner Apache license."),
        ("Everything else on the page is real output from your app Flux Corgi with its cutout and depth map, a voiceline, music and translation.",
         "Everything else on the page is real output from your app Flux Corgi with its cutout and depth map, voiceline, music and translation."),
    ];

    /// Real garbling the check must keep rejecting: a wrong word, a dropped or cut word, a changed negation, extra speech,
    /// an empty transcript.
    const REAL_FAILURES: &[(&str, &str)] = &[
        ("Ornith is a fine-tune of Qwen 3.5 122B, with about 10 billion parameters active per token.",
         "Ornith is a fine-tune of Quim 3.5122b, with about 10 billion parameters active per token."),
        ("All the benchmarks so far are the maker own.", "All the benchmarks so far are the maker zone."),
        ("The work itself as the output.", "The work itself as the outcome."),
        ("Should I start a thread to find Mobius name candidates with trademark and domain checks?",
         "Should I start a thread to find Mobius name candidates with trademark and domain check?"),
        ("I combine that with simpler clues.",
         "I combine that with simpler clues. Is the Mac locked or idle? Which device your AirPods are connected to, and whether the phone is on the same Wi-Fi."),
        ("Dragging no longer opens or closes them, and only a click can open one.",
         "Dragging opens or closes them, and only a click can open one."),
        ("The top one is Qwen3 TTS 1.7B.", "The top one is Qwen 3TTS 1.7B. Try the Aden voice and the designed voice."),
        ("The top one is Qwen3 TTS 1.7B.", "The top one is QA drafter 1.7B."),
        ("Franquen and Virio fit the local AI story best.", "Franquen and Virio fit the local AI story best. It is none of your business."),
        ("Franquen and Virio fit the local AI story best.", ""),
        ("Say twenty dollars a month for hosted models.", "Say twenty a month for hosted models."),
        ("Say twenty dollars a month for hosted models.", "Say €20 a month for hosted models."),
        ("I'll delete the archive.", "I delete the archive."),
        ("I'll delete the archive.", "I'll keep the archive."),
        ("It's a plan.", "It is no plan."),
        ("The card is on the table now.", "The cord is on the table now."),
        ("Thank you for the help.", "Thank your for the help."),
        ("Qwen3 TTS is ready.", "Qwen3 TTS is not ready."),
    ];

    fn verdict(expected: &str, heard: &str) -> Value {
        check(expected, heard)
    }

    #[test]
    fn log_false_alarms_now_pass() {
        let failing: Vec<String> = FALSE_ALARMS
            .iter()
            .filter(|(expected, heard)| verdict(expected, heard)["accepted"] != true)
            .map(|(expected, heard)| format!("{expected} / {heard}: {}", verdict(expected, heard)))
            .collect();
        assert!(failing.is_empty(), "still rejected:\n{}", failing.join("\n"));
    }

    #[test]
    fn log_real_garbling_still_fails() {
        let passing: Vec<String> = REAL_FAILURES
            .iter()
            .filter(|(expected, heard)| verdict(expected, heard)["accepted"] != false)
            .map(|(expected, heard)| format!("{expected} / {heard}"))
            .collect();
        assert!(passing.is_empty(), "accepted garbling:\n{}", passing.join("\n"));
    }

    #[test]
    fn spellings_are_named_in_the_evidence() {
        let r = verdict("I've told the Shure team.", "I have told the Shaw team.");
        let kinds: Vec<&str> = r["spellings"].as_array().unwrap().iter().map(|s| s["kind"].as_str().unwrap()).collect();
        assert!(kinds.contains(&"split") && kinds.contains(&"name"), "{r}");
    }
    /// 23–24 Oct night (`heimdall.log`): each was heard alike on every take and switched the sentence to the lighter
    /// voice.
    #[test]
    fn the_24_oct_recognizer_spellings_pass() {
        for (expected, heard) in [
            ("Scores rose about three points a decade during the twentieth century, so whoever renorms the test decides who's in.",
                "Scores rose about three points a decade during the 20th century, so whoever renorms the test decides who's in."),
            ("It was the twenty first time.", "It was the 21st time."),
            ("The worker will also find out why Control Command M often fails.", "The worker will also find out why Ctrl Command-M often fails."),
            ("Pick on the card whether the switch should hide those too when it's off.", "Pick on the card whether the switch should hide those two when it's off."),
            ("GitHub Sponsors and X subscriptions also pay into Stripe.", "GitHub sponsors and ex-subscriptions also pay into Stripe."),
            ("The Clip tool gets a real test on the music video you watched at 00 40, so we'll know it works.",
                "The Clip tool gets a real test on the music video you watched at 0040, so we'll know it works."),
        ] {
            assert_eq!(check(expected, heard)["accepted"], true, "{expected} / {heard}: {}", check(expected, heard));
        }
    }
    #[test]
    fn the_24_oct_spellings_still_catch_real_failures() {
        for (expected, heard) in [
            ("During the twentieth century scores rose.", "During the 19th century scores rose."),
            ("It was the twenty first time.", "It was the 20th time."),
            ("Hide those too when it's off.", "Hide those three when it's off."),
            ("Watched at 00 40 today.", "Watched at 40 today."),
            ("Press Control Command M now.", "Press Command M now."),
        ] {
            assert_eq!(check(expected, heard)["accepted"], false, "{expected} / {heard}");
        }
    }
    /// The check as the app runs it: on the sentence as the voice is given it (`drama3::normalize`).
    fn spoken_check(written: &str, heard: &str) -> Value {
        let said = crate::drama3::normalize(written);
        check(&said, heard)
    }
    /// 23–24 Oct (`heimdall.log`): symbols, units and spellings that rejected correct takes. Each must pass.
    #[test]
    fn the_24_oct_symbol_takes_pass() {
        for (written, heard) in [
            // 12:19: "40%" was stripped before the check, so the heard "percent" was an added word on all three takes.
            ("That covers the freeze fix, the shortcuts, dragging files onto the notch, the voice fallback, the taste ledger, the new acknowledgement lines, and background jobs that now cost about 40% less.",
                "That covers the freeze fix, the shortcuts dragging files onto the notch, the voice fallback, the taste ledger, the new acknowledgement lines and background jobs that now cost about 40 percent less."),
            ("Background jobs now cost about 40% less.", "Background jobs now cost about 40% less."),
            ("It's right 85% of the time against 81%, it catches 76% of your real requests against 68%, and it doesn't add any false sends.",
                "It's right, 85% of the time, against 81%, it catches 76% of your real requests against 68%, and it doesn't add any false sends."),
            // 03:23 and 23 Oct 01:25: the recognizer's "$" on an amount said once ("dollar a" twice) read as a repeat.
            ("That's about 650 dollars a day, or nearly 20,000 a month.", "That's about $650 a day, or nearly $20,000 a month."),
            ("Opus now about 11 dollars 50 a day, roughly 345 a month.", "Opus now about $11.50 a day, roughly $345 a month."),
            // 03:17 and 03:37: the thousands now reach the voice as one number.
            ("Your 200-dollar plan covers what would cost about 18,500 dollars a month at Anthropic's list prices.",
                "Your $200 plan covers what would cost about $18,500 a month at Anthropics list prices."),
            ("The answer: keep me on Opus, and you'd pay about 3,100 dollars a month.", "The answer, keep me on Opus, and you'd pay about $3,100 a month."),
            // 03:30: "twentieth" heard "20th" (since fixed), kept as a regression.
            ("The actual fix is also in: correct takes were being rejected over spellings like twentieth and 20th.",
                "The actual fix is also in. Correct takes were being rejected over spellings like 20th and 20th."),
            // British spelling heard American, and homophones heard alike on two takes.
            ("Three times today Jarvis kept his own take instead of switching, on words like past and judgement.",
                "Three times today Jarvis kept his own take instead of switching on words like past and judgment."),
            ("In a blind judgement Haiku came first with the right move on 95% of steps against 75% for Opus.",
                "In a blind judgment haiku came first with the right move on 95% of steps against 75% for Opus."),
            ("A worker is rewriting the acknowledgements, crisp and decisive.", "A worker is rewriting the acknowledgments, crisp and decisive."),
            ("The centre of the colour wheel.", "The center of the color wheel."),
            ("Everything from yesterday merged, passed the full test suite and installed cleanly at eight.",
                "Everything from yesterday merged past the full test suite and installed cleanly at 8."),
            ("On the plain yes or no, it's better.", "On the plane yes or no, it's better."),
            // The other symbols, as the recognizer writes them or in words.
            ("It runs at 1920x1080 and 4x faster.", "It runs at 1920 by 1080 and four times faster."),
            ("It runs at 1920x1080 and 4x faster.", "It runs at 1920x1080 and 4x faster."),
            ("Salt & pepper, at 20°C.", "Salt and pepper at 20 degrees Celsius."),
            ("Salt & pepper, at 20°C.", "Salt & pepper, at 20°C."),
            ("It holds 20k rows for €5 and costs 20 zł.", "It holds 20,000 rows for 5 euros and costs 20 złoty."),
            ("It holds 20k rows for €5.", "It holds 20K rows for €5."),
            ("C++ is #1.", "C plus plus is number one."),
            // Review 24 Oct: a word after "°", one degree, magnitudes, a symbol after its amount, Polish symbols.
            ("Turn 90° to the left.", "Turn 90 degrees to the left."),
            ("It is 1°C outside.", "It is one degree Celsius outside."),
            ("It is 1°C outside.", "It is 1°C outside."),
            ("It raised $5B and $5m.", "It raised 5 billion dollars and $5 million."),
            ("It costs 20€ a month.", "It costs 20 euros a month."),
            ("Kosztuje 40% mniej, czyli 22 $ i 5 zł.", "Kosztuje 40% mniej, czyli 22 dolary i 5 złotych."),
            ("Kosztuje 40% mniej, czyli 22 $ i 5 zł.", "Kosztuje 40 procent mniej, czyli $22 i 5 zł."),
            ("Ekran 2 x 3 jest 4x szybszy.", "Ekran 2 na 3 jest 4 razy szybszy."),
            ("We travelled and cancelled it.", "We traveled and canceled it."),
            ("Zebrał $2k i 2M zł.", "Zebrał 2 tysiące dolarów i 2 miliony złotych."),
            ("Zebrał $2k i 2M zł.", "Zebrał $2,000 i 2 mln zł."),
            ("Flights to Kraków are 40% cheaper.", "Flights to Kraków are 40 percent cheaper."),
        ] {
            let v = spoken_check(written, heard);
            assert_eq!(v["accepted"], true, "{written} / {heard}: {v}");
        }
    }
    /// The same sentences garbled: a dropped or added word, a changed number or unit, a flipped negation.
    #[test]
    fn the_24_oct_symbol_takes_still_catch_real_failures() {
        for (written, heard) in [
            ("Background jobs now cost about 40% less.", "Background jobs now cost about 40 less."),
            ("Background jobs now cost about 40% less.", "Background jobs now cost about 14% less."),
            ("Background jobs now cost about 40% less.", "Background jobs now cost about 40 percent percent less."),
            ("Background jobs now cost about 40% less.", "Background jobs now cost about $40 less."),
            ("Jobs do not cost 40% less.", "Jobs do cost 40% less."),
            // 12:20, first take: two of the four percents unsaid.
            ("It's right 85% of the time against 81%, it catches 76% of your real requests against 68%, and it doesn't add any false sends.",
                "It's right, 85 of the time, against 81, it catches 76% of your real requests against 68%, and it doesn't add any false sends."),
            // 03:17 and 03:37 as heard: a different amount.
            ("Your 200-dollar plan covers what would cost about 18,500 dollars a month at Anthropic's list prices.",
                "Your $200 plan covers what would cost about $1,8,500 a month at Anthropics list prices."),
            ("The answer: keep me on Opus, and you'd pay about 3,100 dollars a month.", "The answer, keep me on Opus, and you'd pay about $300-100 a month."),
            ("That's about 650 dollars a day, or nearly 20,000 a month.", "That's about $650 a day, or nearly $20,000 a month a month."),
            ("That's about 650 dollars a day, or nearly 20,000 a month.", "That's about 650 euros a day, or nearly 20,000 a month."),
            ("Salt & pepper, at 20°C.", "Salt and pepper at 20 degrees Fahrenheit."),
            ("Salt & pepper, at 20°C.", "Salt and pepper at 20."),
            ("It holds 20k rows.", "It holds 20 rows."),
            ("It runs 4x faster now.", "It runs 40 times faster now."),
        ] {
            let v = spoken_check(written, heard);
            assert_eq!(v["accepted"], false, "{written} / {heard}: {v}");
            assert_eq!(keepable(&v, &[]), None, "{written} / {heard}: kept {v}");
        }
        // Another word, not another spelling (`keepable` may still keep one such word after the last retry).
        assert_eq!(spoken_check("A blind judgement came first.", "A blind judge came first.")["accepted"], false);
        assert_eq!(spoken_check("Paint the centre wall.", "Paint the cent wall.")["accepted"], false);
        // The doubled "l" rule needs an unstressed "e" ("stalled" heard "staled" is matched by sound, `sounds_alike`).
        assert!(!regional("stalled", "staled") && regional("travelled", "traveled") && regional("judgement", "judgment"));
        // A Polish decimal comma stays a decimal: "0,125" is never 125.
        assert_eq!(spoken_check("Wynik to 0,125 sekundy.", "Wynik to 125 sekundy.")["accepted"], false);
        assert_eq!(spoken_check("It raised $5B.", "It raised 5 million dollars.")["accepted"], false);
        assert_eq!(spoken_check("Kosztuje 22 $ dzisiaj.", "Kosztuje 22 euro dzisiaj.")["accepted"], false);
    }
    #[test]
    fn a_take_misheard_but_complete_is_kept_after_the_last_retry() {
        let keep = |expected: &str, heard: &str| keepable(&check(expected, heard), &[]);
        // 23 Oct: a name, a Polish phrase, a name heard another way.
        assert_eq!(keep("The Baltic Slavs also had a dedicated war god Jarovit.", "The Baltic Slavs also had a dedicated war god Yarovit."), Some(1));
        assert!(keep("He's also very Polish the four faced Zbruch idol in Kraków is popularly called Światowid.",
            "He's also very Polish. The four faced Zabruk idol in Krakov is popularly called Shwiatovid.").is_some());
        assert!(keep("Listen through and tell me which lines and deliveries you like and whether to drop Robi się.",
            "Listen through and tell me which lines and deliveries you like and whether to drop Roby's share.").is_some());
        // Never kept: a cut end, a repeat, too many wrong words, a dropped negation, a changed number or opposite.
        assert_eq!(keep("It's crowded and there's a metal band called Týr.", "It's crowded and there's a metal band called"), None);
        assert_eq!(keep("The plan is ready for you now.", "The plan is ready ready for you now."), None);
        assert_eq!(keep("Nothing gets deleted until that review passes and you press done.", "Nothing gets deleted until that review passes."), None);
        assert_eq!(keep("Videos and folders I get the path.", "Videos and folders."), None);
        assert_eq!(keep("The build is not ready for tonight.", "The build is ready for tonight."), None);
        assert_eq!(keep("It costs twenty dollars today.", "It costs thirty dollars today."), None);
        assert_eq!(keep("Start the worker now please.", "Stop the worker now please."), None);
        assert_eq!(keep("Big red words here.", "Pig red birds here."), None, "short sentences keep one wrong word");
    }
    #[test]
    fn a_meaning_word_a_dropped_word_or_a_cut_end_is_never_kept() {
        let keep = |expected: &str, heard: &str| keepable(&check(expected, heard), &[check(expected, heard)]);
        // A changed number, negation or opposite, even heard alike on every take (review 24 Oct: the voice misreads a
        // number the same way each time).
        assert_eq!(keep("It costs fifteen dollars today.", "It costs fifty dollars today."), None);
        assert_eq!(keep("The build is not ready for tonight.", "The build is now ready for tonight."), None);
        assert_eq!(keep("Start the worker now please.", "Stop the worker now please."), None);
        // A dropped or added plain word.
        assert_eq!(keep("Delete the cache files now.", "Delete the files now."), None);
        assert_eq!(keep("Delete the old cache files now and then.", "Delete the old cache files now and again then."), None);
        // A cut last word heard as a stub or a noise word.
        assert_eq!(keep("Pick one and press done.", "Pick one and press the."), None);
        assert_eq!(keep("The war god was called Jarovit.", "The war god was called Ja."), None);
        // A verifier error carries no edits and is never kept.
        assert_eq!(keepable(&json!({"accepted":false,"reason":"live recognizer unavailable"}), &[]), None);
    }
}
#[cfg(test)]
mod night_1027_tests {
    use super::*;
    /// The written sentence through the speech contract (`drama3::normalize`), as the live check gets it.
    fn spoken(written: &str, heard: &str) -> Value {
        check(&crate::drama3::normalize(written), heard)
    }

    /// Takes rejected 25-26 Oct (`lab/word-check-1027/takes.jsonl`) that were the recognizer's spelling: each must pass.
    #[test]
    fn logged_false_rejections_pass() {
        let failing: Vec<String> = [
            // Repeat check on raw heard words: "Claude Code" heard "clawed code" twice (all three takes).
            ("Claude Code with a proxy model is the riskiest option, because Anthropic's own docs say they don't support Claude Code with other models.",
             "Clawed code with a proxy model is the riskiest option because Anthropic's own docs say they don't support clawed code with other models."),
            // Numbers.
            ("It's two and a half thousand 3D objects, things like server racks, robot arms and forklifts.", "It's 2,500 3D objects, things like server racks, robot arms and forklifts."),
            ("Haiku rewriting a two-and-a-half-thousand-word skill just reshuffles words.", "Haiku rewriting a 2.500-word skill just reshuffles words."),
            ("Your new picks favoured four-bit four to one, with one tie.", "Your new picks favoured 4-bit 4-1 with one tie."),
            ("Full precision won one and one was a tie.", "Full precision 1-1 and 1 was a tie."),
            ("The Omni models are full multimodal models, closer to GPT-4o.", "The Omni models are full multimodal models, closer to GPT-4.0."),
            // Short stems with an ending, mid-sentence.
            ("Arsenal: the Launch at Login fix, the twelve dropped models and the install.", "Arsenal the launch at login fixed the 12 dropped models and the install."),
            ("You can filter by kind, such as speech, screen or text, and open any hit in full.", "You can filter by kinds such as speech screen or text and open any hit in full."),
            ("That's a big part of why it hit swap.", "That's a big part of why it hits swap."),
            // Names and jargon by sound.
            ("The research now also covers which abliterated versions are trustworthy.", "The research now also covers which obliterated versions are trustworthy."),
            ("That judge ran as Luna, not Sol, because your Codex doesn't offer Sol yet.", "That judge ran as lunar not soul because your Codex doesn't offer soul yet."),
            ("Both labs are on it as whole labs, Qwen and Nanbeige, so we check their pages.", "Both labs are on it as whole labs Qwen and Nambège, so we check their pages."),
            ("It caught the benchmaxxing on its own today.", "It caught the benchmarking on its own today."),
            // Sound-alikes and homophones.
            ("I've queued it for tonight's night pass.", "I've cued it for tonight's night pass."),
            ("It also doesn't speak Polish, and there's no Mac route; it's built for CUDA servers.", "It also doesn't speak Polish, and there's no Mac root, it's built for CUDA servers."),
            ("Arsenal needs your OK before it runs the model's own code.", "Arsenal needs your okay before it runs the model's own code."),
            // Two words heard as one.
            ("I kept two that are in use: the main Heimdall build from this afternoon.", "I kept two that are in use, the Mainheimdahl build from this afternoon."),
            // "6 ch" is spoken "6 channel"; a web address keeps its "dot".
            ("That's your call, and the rule is simple: a 5.1 or \"6 ch\" track, or your Blu-ray.", "That's your call, and the rule is simple, a 5.1 or 6 channel track or your Blu-ray."),
            ("Usage from X.com counts against the same pool.", "Usage from X dot com counts against the same pool."),
            ("Usage from X.com counts against the same pool.", "Usage from X.com counts against the same pool."),
        ]
        .iter()
        .filter(|(w, h)| spoken(w, h)["accepted"] != true)
        .map(|(w, h)| format!("{w} / {h}: {}", spoken(w, h)))
        .collect();
        assert!(failing.is_empty(), "still rejected:\n{}", failing.join("\n"));
    }

    /// The real voice faults of 20-27 Oct (dropped tails and quoted questions, wrong numbers) and what the new rules
    /// must not hide: each stays rejected.
    #[test]
    fn logged_real_faults_and_near_misses_stay_rejected() {
        let passing: Vec<String> = [
            ("Delegated work now defaults to your Anthropic subscription; I'm still on Codex.", "Delegated work now defaults to your Anthropic subscription."),
            ("If you'd like to try it, open the settings and you press Done.", "If you'd like to try it, open the settings."),
            ("Your 200-dollar plan covers what would cost about 18,500 dollars a month.", "Your $200 plan covers what would cost about $1,8,500 a month."),
            ("The worker had decided it couldn't be done.", "The worker decided it couldn't be done."),
            // A cut word, a changed short word, a changed number, a negation, a real repeat.
            ("Paint the centre wall.", "Paint the cent wall."),
            ("The card is on the table now.", "The cord is on the table now."),
            ("Thank you for the help.", "Thank your for the help."),
            ("Your new picks favoured four-bit four to one.", "Your new picks favoured 4-bit 4-2."),
            ("It's two and a half thousand objects.", "It's 2,000 objects."),
            ("A lint script fails it if there is no alt text.", "A lint script fails it if there is know alt text."),
            ("Claude Code is the riskiest option.", "Clawed code is the riskiest Clawed code is the riskiest option."),
            // The last word heard shorter is a cut tail, even with an ending rule.
            ("I've sent the breakdown to the fix task and it is fixed.", "I've sent the breakdown to the fix task and it is fix."),
            // Two words heard as one never hides a dropped content word.
            ("The free fix is to turn so no window is behind you.", "The fix is to turn so no window is behind you."),
            // At most `misheard_limit` misheard words: four in a short sentence is garbling.
            ("Queue the leaner build route.", "Cue the liener bill root."),
        ]
        .iter()
        .filter(|(w, h)| spoken(w, h)["accepted"] != false)
        .map(|(w, h)| format!("{w} / {h}: {}", spoken(w, h)))
        .collect();
        assert!(passing.is_empty(), "accepted:\n{}", passing.join("\n"));
    }

    /// Review of night 1027: an opposite, a number joined across "to" and a final word one letter short each passed.
    #[test]
    fn opposites_ranges_and_short_final_words_stay_rejected() {
        let names = HashSet::new();
        for (x, y) in [("available", "unavailable"), ("important", "unimportant"), ("expected", "unexpected"), ("active", "inactive"), ("regular", "irregular"), ("possible", "impossible")] {
            assert!(!misheard(x, y, &names), "{x}/{y}");
            assert!(!misheard(y, x, &names), "{y}/{x}");
        }
        assert!(misheard("abliterated", "obliterated", &names), "a vowel start still passes");
        let passing: Vec<String> = [
            ("The build is available on the Mac now.", "The build is unavailable on the Mac now."),
            ("That was an important change for us.", "That was an unimportant change for us."),
            ("The new worker is active on the queue.", "The new worker is inactive on the queue."),
            ("Your new picks favoured four to one.", "Your new picks favoured 41."),
            ("The shop opens nine to five today.", "The shop opens 95 today."),
            ("I've moved the two smaller models.", "I've moved the two smaller model."),
            ("I fixed the two failing tests.", "I fixed the two failing test."),
            ("The worker flipped both bits.", "The worker flipped both bit."),
        ]
        .iter()
        .filter(|(w, h)| spoken(w, h)["accepted"] != false)
        .map(|(w, h)| format!("{w} / {h}: {}", spoken(w, h)))
        .collect();
        assert!(passing.is_empty(), "accepted:\n{}", passing.join("\n"));
        // A range still passes however it is written.
        for heard in ["Your new picks favoured 4-1.", "Your new picks favoured 4 to 1.", "Your new picks favoured 4–1.", "Your new picks favoured 4 1."] {
            assert_eq!(spoken("Your new picks favoured four to one.", heard)["accepted"], true, "{heard}");
        }
        assert_eq!(spoken("The score was 4-1 at the end.", "The score was four to one at the end.")["accepted"], true);
        // A tie heard without its "to" is no repeat, and a dropped number around a range is one deletion.
        assert_eq!(spoken("It ended two to two.", "It ended 2 2.")["accepted"], true);
        assert_eq!(spoken("It ended two to two.", "It ended 2-2.")["accepted"], true);
        assert_eq!(check("4-1, what a game.", "1, what a game.")["deletions"], 1);
        assert_eq!(spoken("I've moved the two smaller models.", "I've moved the two smaller modules.")["accepted"], true, "not shorter");
        // A huge number with "thousand" or "and a half thousand" saturates instead of overflowing.
        words("99999999999999999999 thousand");
        words("18446744073709551615 and a half thousand");
        words("18446744073709551615 thousand");
    }

    #[test]
    fn a_web_address_and_a_channel_count_are_one_sentence_and_spoken_whole() {
        assert_eq!(crate::drama3::sentences("Usage from X.com counts. It is 5 a.m. now."), ["Usage from X.com counts.", "It is 5 a.m.", "now."]);
        assert_eq!(crate::drama3::sentences("Version 1.2.3 shipped. Wait.5 is odd."), ["Version 1.2.3 shipped.", "Wait.", "5 is odd."]);
        assert_eq!(crate::drama3::normalize("Look for \"6 ch\" or AAC 2ch."), "Look for 6 channel or AAC 2 channel.");
        assert_eq!(crate::drama3::normalize("Szukaj ścieżki 6 ch."), "Szukaj ścieżki 6 ch.", "Polish text keeps its letters");
    }
}
/// 31 Oct to 4 Nov (`heimdall.log`): the 12 sentences the word check sent to the lighter voice, each heard alike on
/// every take. Expected text is the plain text the voices were given (from the fallback line).
#[cfg(test)]
mod night_1104_tests {
    use super::*;

    #[test]
    fn the_logged_fallbacks_now_pass() {
        for (expected, heard) in [
            ("Save As and a full model add haven't been tested yet.", "Save as and a full model ad haven't been tested yet."),
            ("The public list is now about 77 models and the add a model kit ships inside the app.",
                "The public list is now about 77 models and the Adder model kit ships inside the app."),
            ("Meta's DINOv3 encoder comes under Meta's own licence which bars military and warfare uses and has to be passed along with any distribution.",
                "Meta's Dyno V3 encoder comes under Meta's own license which bars military and warfare uses and has to be passed along with any distribution."),
            ("One thing you can do is request access for SAM 3.1 SAM Audio and the DINOv3 model behind TRELLIS.",
                "One thing you can do is request access for SAM 3.1 SAM audio and the Dyno V3 model behind Trellis."),
            ("At list prices it's roughly 19 to 42 thousand dollars a month.", "At list prices it's roughly $19,000 to $42,000 a month."),
            ("The battery is at 86 percent with about three and three quarter hours left.", "The battery is at 86% with about 3 3 quarter hours left."),
            ("When it's done it gets a beauty climb aimed at 8.5 too.", "When it's done, it gets a beauty climb aimed at 8.52."),
            ("Something like a calm dry refined British butler voice mid forties.", "Something like a calm, dry, refined British butler voice, mid 40s."),
            ("Build one of them a simple tool a parts lookup a quote helper or an inbox auto reply and charge a hundred to three hundred dollars.",
                "Build one of them a simple tool, a parts lookup, a quote helper, or an inbox auto-reply, and charge $100 to $300."),
            ("I wrote a hundred to three hundred dollars and it heard back the first half.", "I wrote $100 to $300 and it heard back the first half."),
            ("It will place your 70 billion a month on a scale.", "It will place your $70 billion a month on a scale."),
            ("It costs 19 to 42 million dollars.", "It costs $19 million to $42 million."),
        ] {
            assert_eq!(check(expected, heard)["accepted"], true, "{expected} / {heard}: {}", check(expected, heard));
        }
    }

    /// The exact 4 Nov fallbacks the night brief named: 02:15:52 ("+to" on all three takes) and 02:29:49.
    #[test]
    fn the_exact_plus_to_fallbacks_of_4_nov_pass() {
        for (expected, heard) in [
            ("Build one of them a simple tool a parts lookup a quote helper or an inbox auto reply and charge a hundred to three hundred dollars.",
                "Build one of them a simple tool, a parts lookup, a quote helper, or an inbox auto reply, and charge $100 to $300."),
            ("I wrote a hundred to three hundred dollars it heard back 100 dollars to decided a word was missing and rejected the take three times.",
                "I wrote $100 to $300, it heard back $100 to decided a word was missing, and rejected the take three times."),
            ("I wrote a hundred to three hundred dollars it heard back 100 dollars to decided a word was missing and rejected the take three times.",
                "I wrote $100 to $300, it heard back $100, to decided a word was missing, and rejected the take three times."),
            ("I wrote a hundred to three hundred dollars it heard back 100 dollars to decided a word was missing and rejected the take three times.",
                "I wrote $100 to $300, it herd back $100 to decided a word was missing and rejected the take three times."),
        ] {
            assert_eq!(check(expected, heard)["accepted"], true, "{expected} / {heard}: {}", check(expected, heard));
        }
    }

    /// 31 Oct to 4 Nov, the other rejections (each cost Jarvis one or two retries before a take was kept), with the
    /// expected text rebuilt from what was heard and the wrong words the log names.
    #[test]
    fn the_logged_retries_of_homophones_and_soft_names_pass() {
        for (expected, heard) in [
            ("In a test on a copy grouping those writes cut the run from 23.7 seconds to 2.9 and the worst wait from over 4 seconds to 41 milliseconds.",
                "In a test on a copy grouping those writes cut the run from 23.7 seconds to 2.9, and the worst weight from over 4 seconds to 41 milliseconds."),
            ("I've filed it again with the cause which looks like the subthread hanging one level deeper than the fix expected.",
                "I've filed it again with the cores, which looks like the subthread hanging one level deeper than the fix expected."),
            ("The private alternative is Tavily for search plus Jina for reading with two accounts.",
                "The private alternative is Tavali for search plus Gina for reading with two accounts."),
            ("If privacy is what's bothering you the cleaner option is Tavily plus Jina or Brave for search which says it doesn't train on your queries.",
                "If privacy is what's bothering you, the cleaner option is Tavali plus Gena or Brave for search, which says it doesn't train on your queries."),
        ] {
            assert_eq!(check(expected, heard)["accepted"], true, "{expected} / {heard}: {}", check(expected, heard));
        }
        // Still caught: a hard "g" in a name, a soft "g" on a plain word (names only), words that only look alike.
        for (expected, heard) in [
            ("Ask Gary for the reading.", "Ask Jary for the reading."),
            ("Keep the gentle tone for the reply.", "Keep the jentle tone for the reply."),
            ("Wait for the build.", "Weigh for the build."),
            ("The cause is known.", "The course is known."),
            ("It heard nothing back.", "It had nothing back."),
        ] {
            assert_eq!(check(expected, heard)["accepted"], false, "{expected} / {heard}: {}", check(expected, heard));
        }
    }

    #[test]
    fn the_new_spellings_still_catch_wrong_numbers_and_names() {
        for (expected, heard) in [
            ("It's roughly 19 to 42 thousand dollars a month.", "It's roughly $19,000 to $45,000 a month."),
            ("Charge a hundred to three hundred dollars.", "Charge $100 to $400."),
            ("Charge a hundred to three hundred dollars.", "Charge $100 to 300 euros."),
            ("Aimed at 8.5 too.", "Aimed at 8.53."),
            ("A voice in its mid forties.", "A voice in its mid 50s."),
            ("Meta's DINOv3 encoder is ready.", "Meta's Dyno V2 encoder is ready."),
            ("Meta's DINOv3 encoder is ready.", "Meta's Rhino V3 encoder is ready."),
            ("About three and three quarter hours left.", "About 3 4 quarter hours left."),
            ("A full model add is next.", "A full model had is next."),
        ] {
            assert_eq!(check(expected, heard)["accepted"], false, "{expected} / {heard}");
        }
    }

    fn rejected(expected: &str, heard: &str) -> Value {
        let r = check(expected, heard);
        assert_eq!(r["accepted"], false, "{expected} / {heard}");
        r
    }

    #[test]
    fn a_take_heard_alike_twice_is_accepted_at_the_second_take() {
        // 4 Nov 01:41: "in Downloads" heard "and downloads" on all three takes.
        let (expected, heard) = ("The page and Angel's copy in Downloads get updated when it finishes.",
            "The page and Angel's copy and downloads get updated when it finishes.");
        let first = rejected(expected, heard);
        let mut second = rejected(expected, heard);
        assert!(!accept_agreed_take(&mut first.clone(), &[]), "one take alone is not agreement");
        assert!(accept_agreed_take(&mut second, &[first.clone()]));
        assert_eq!(second["accepted"], true);
        assert!(second["spellings"].as_array().unwrap().iter().any(|s| s["kind"] == "agreed"), "{second}");
        // Heard differently on the two takes: no agreement.
        let other = rejected(expected, "The page and Angel's copy on downloads get updated when it finishes.");
        assert!(!accept_agreed_take(&mut other.clone(), &[first]));
    }

    #[test]
    fn agreement_never_keeps_a_dropped_word_a_changed_number_or_an_opposite() {
        for (expected, heard) in [
            ("Delete the old cache files now.", "Delete the old files now."),
            ("It costs fifteen dollars today.", "It costs fifty dollars today."),
            ("The build is not ready for tonight.", "The build is now ready for tonight."),
            ("Start the worker now please.", "Stop the worker now please."),
            ("Put it in the folder please.", "Put it on the folder please."),
            ("The plan is ready for you now.", "The plan is ready ready for you now."),
            ("Pick one and press done.", "Pick one and press the."),
            // Review 4 Nov: words that do not sound alike were said that way.
            ("The tests passed on the first run.", "The tests failed on the first run."),
            ("The switch is disabled for now.", "The switch is enabled for now."),
            ("Unlock the screen when you are back.", "Lock the screen when you are back."),
            ("Run it before the build tonight.", "Run it for the build tonight."),
            ("Delete the old cache files now.", "Delete the old cache backup files now."),
        ] {
            let first = check(expected, heard);
            let mut second = first.clone();
            assert!(!accept_agreed_take(&mut second, &[first]), "{expected} / {heard}");
        }
    }

    #[test]
    fn too_is_a_digit_only_after_a_number() {
        assert_eq!(check("Set the timer to 15 minutes.", "Set the timer 215 minutes.")["accepted"], false);
        assert_eq!(check("It costs 25 dollars a month.", "It costs to 5 dollars a month.")["accepted"], false);
    }
    #[test]
    fn a_contrast_word_heard_as_a_plain_word_may_be_kept_after_the_retries() {
        let r = check("The page and Angel's copy in Downloads get updated when it finishes.", "The page and Angel's copy and downloads get updated when it finishes.");
        assert_eq!(keepable(&r, &[]), Some(1));
        assert_eq!(keepable(&check("Put it in the folder please.", "Put it on the folder please."), &[]), None);
    }
}
