//! The user's own words, checked before the computer tool goes on or presses something outward (review C2 and C3, night
//! 10-11 Nov). A quote (`owner_asked`) counts only when it is in one of his recent lines, word for word, and that line
//! asks for the thing in so many words: a verb and its object ("turn on computer control", "you can control my apps",
//! "go ahead and click it for me"), not a word that merely appears ("my computer is slow", "remote control", "the
//! controller"). A negation or an "if" just before the ask voids it ("don't turn on computer control").
//!
//! Matching is on whole words, lower-cased, with apostrophes dropped ("don't" is "dont"). English only.

/// Lower-case words; apostrophes are dropped so "don't" reads "dont".
pub fn words(text: &str) -> Vec<String> {
    text.to_lowercase()
        .replace(['\'', '’', '‘'], "")
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Words that void an ask when they come within [`NEGATION_REACH`] words before it.
const NEGATIONS: &[&str] = &[
    "not", "dont", "never", "no", "stop", "without", "cant", "cannot", "shouldnt", "wont", "didnt", "doesnt", "nor",
    "neither", "if", "whether", "unless", "before", "nobody",
];
const NEGATION_REACH: usize = 4;

/// Where `quote`'s words sit in one of `recent`'s lines: (that line's words, the quote's first word's index). The
/// latest such line wins.
fn locate(quote: &str, recent: &[String]) -> Option<(Vec<String>, usize, usize)> {
    let q = words(quote);
    if q.is_empty() {
        return None;
    }
    recent.iter().rev().find_map(|line| {
        let w = words(line);
        let at = w.windows(q.len()).position(|win| win == q.as_slice())?;
        Some((w, at, q.len()))
    })
}

fn negated(line: &[String], at: usize) -> bool {
    line[at.saturating_sub(NEGATION_REACH)..at].iter().any(|w| NEGATIONS.contains(&w.as_str()))
}

/// One slot of a pattern.
#[derive(Clone, Copy)]
enum Slot {
    /// Any of these words.
    W(&'static [&'static str]),
    /// Any of these words, or nothing.
    Opt(&'static [&'static str]),
    /// Up to [`GAP_WORDS`] free words ("click it for me", "press Mute on morgan for me").
    Gap,
    /// The computer tool by any of its names ([`CONTROL_OBJ`]).
    Obj,
}
use Slot::{Gap, Obj, Opt, W};
const GAP_WORDS: usize = 4;

/// What "control" means here: the computer tool itself, by any of its names.
const CONTROL_OBJ: &[&[Slot]] = &[
    &[W(&["computer", "app", "apps", "mac", "application", "applications"]), W(&["control", "controls"])],
    &[W(&["control", "controls"]), Opt(&["of", "over"]), Opt(&["my", "the"]), W(&["apps", "app", "applications", "computer", "mac"])],
    &[W(&["accessibility"]), Opt(&["control", "access"])],
    &[W(&["control"]), Opt(&["mode"])],
];
const DET: &[&str] = &["the", "my", "your"];
const THING: &[&str] = &["apps", "app", "applications", "computer", "mac", "screen", "desktop"];
/// The asks that turn Control Apps on.
const CONTROL_ASKS: &[&[Slot]] = &[
    // turn on (the) computer control; turn (the) computer control on; enable (the) computer control
    &[W(&["turn", "switch", "flip", "put"]), W(&["on"]), Opt(DET), Obj],
    &[W(&["turn", "switch", "flip", "put"]), Opt(DET), Obj, W(&["on"])],
    &[W(&["enable", "activate", "allow", "unlock"]), Opt(DET), Obj],
    // you can control my apps; take control of my Mac
    &[W(&["you"]), Opt(&["can", "may", "could", "should", "are"]), Opt(&["now", "allowed", "free"]), Opt(&["to"]), W(&["control", "drive", "operate"]), W(&["my", "the"]), W(THING)],
    &[W(&["take"]), W(&["control"]), W(&["of", "over"]), W(&["my", "the"]), W(THING)],
    // go ahead and click it; click it for me
    &[W(&["go"]), W(&["ahead"]), Opt(&["and"]), W(&["click", "press", "tap"])],
    &[W(&["click", "press", "tap"]), Gap, W(&["for"]), W(&["me"])],
];

/// The end of a match of `p` at `i` in `line`, within `to`.
fn go(p: &[Slot], line: &[String], i: usize, to: usize) -> Option<usize> {
    let Some((first, rest)) = p.split_first() else { return Some(i) };
    let word = (i < to).then(|| line[i].as_str());
    match *first {
        W(any) => word.filter(|w| any.contains(w)).and_then(|_| go(rest, line, i + 1, to)),
        Opt(any) => word.filter(|w| any.contains(w)).and_then(|_| go(rest, line, i + 1, to)).or_else(|| go(rest, line, i, to)),
        Gap => (0..=GAP_WORDS).filter(|k| i + k <= to).find_map(|k| go(rest, line, i + k, to)),
        Obj => CONTROL_OBJ.iter().find_map(|obj| go(obj, line, i, to).and_then(|j| go(rest, line, j, to))),
    }
}

/// The first word of the first ask in `line[from..to]`.
fn first_ask(line: &[String], from: usize, to: usize, asks: &[&[Slot]]) -> Option<usize> {
    (from..to).find(|&i| asks.iter().any(|p| go(p, line, i, to).is_some_and(|end| end > i)))
}

/// `quote` is the user's: in one of his `recent` lines word for word, asking to turn on control of apps, with no negation
/// just before the ask. Err says which part failed, for the agent.
pub fn asks_for_control(quote: &str, recent: &[String]) -> Result<(), &'static str> {
    let Some((line, at, len)) = locate(quote, recent) else {
        return Err("owner_asked must quote {user}'s own recent words, as the transcript has them; these are not in their recent lines");
    };
    let Some(i) = first_ask(&line, at, at + len, CONTROL_ASKS) else {
        return Err("owner_asked must be their words asking for it in so many words (\"turn on computer control\", \"you can control my apps\", \"go ahead and click it for me\"), not other words of theirs");
    };
    if negated(&line, i) {
        return Err("their words around it say not to (or only if); ask them");
    }
    Ok(())
}

/// `quote` is the user's (in a recent line of his, word for word) and names `label` itself, whole, with no negation just
/// before it: "press Send" names “Send”, "send it" does not name “Send Message”.
pub fn names_label(quote: &str, recent: &[String], label: &str) -> bool {
    let want = words(label);
    let Some((line, at, len)) = locate(quote, recent) else { return false };
    if want.is_empty() || want.len() > len {
        return false;
    }
    (at..=at + len - want.len()).any(|i| line[i..i + want.len()] == want[..] && !negated(&line, i))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn said(lines: &[&str]) -> Vec<String> {
        lines.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn asks_that_turn_control_on() {
        for (line, quote) in [
            ("Okay, turn on computer control please", "turn on computer control"),
            ("You can control my apps now", "You can control my apps"),
            ("go ahead and click it for me", "go ahead and click it for me"),
            ("Heimdall, turn on control apps so you can mute people", "turn on control apps"),
            ("Enable accessibility control for this", "Enable accessibility control"),
            ("turn the computer control on", "turn the computer control on"),
            ("Just take control of my Mac and do it", "take control of my Mac"),
            ("click Mute for me", "click Mute for me"),
        ] {
            assert_eq!(asks_for_control(quote, &said(&[line])), Ok(()), "{line}");
        }
    }

    #[test]
    fn words_that_only_mention_it_are_refused() {
        for line in [
            "my computer is slow",
            "My computer is slow, click on that link",
            "where is the remote control",
            "the controller is broken",
            "turn on the remote control",
            "I control my own schedule",
            "mute morgan in Discord",
        ] {
            assert!(asks_for_control(line, &said(&[line])).is_err(), "{line}");
        }
    }

    #[test]
    fn a_negation_or_an_if_voids_the_ask() {
        for (line, quote) in [
            ("Don't turn on computer control", "turn on computer control"),
            ("never enable computer control", "enable computer control"),
            ("what happens if you turn on computer control", "turn on computer control"),
        ] {
            assert!(asks_for_control(quote, &said(&[line])).is_err(), "{line}");
        }
    }

    #[test]
    fn the_quote_must_be_his_words() {
        // The agent's paraphrase, or an ask he never made.
        assert!(asks_for_control("turn on computer control", &said(&["Can you mute morgan?"])).is_err());
        assert!(asks_for_control("turn on computer control", &[]).is_err());
    }

    #[test]
    fn a_label_counts_only_when_his_words_name_it_whole() {
        let recent = said(&["Yes, press Send on that draft", "don't delete anything", "delete message from morgan please"]);
        assert!(names_label("press Send", &recent, "Send"));
        assert!(!names_label("press Send", &recent, "Send Message"), "the whole label");
        assert!(!names_label("delete anything", &recent, "Delete"), "negated");
        assert!(names_label("delete message from morgan", &recent, "Delete Message"));
        assert!(!names_label("send it now", &recent, "Send"), "not his words");
        assert!(!names_label("press Send", &recent, ""), "an unnamed element is never named");
    }
}
