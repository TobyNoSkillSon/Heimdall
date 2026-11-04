//! The reply path ("your final response will be actually the speaking response … the final response
//! will be also read … we will also have to have like a very strict guidelines how do you write the final response").
//!
//! A settled turn's final text is what the user hears (Speaking on, Heimdall's voice) or reads on the island (off): the
//! app routes it through the same path a `speak` call takes. `speak` stays for updates during longer work. The rules,
//! all pure and decided here so the app and the harness simulator run the same code:
//!
//! - Silent: a final text that is empty, or whose first or last word is [`SILENT`] (`NO_REPLY`), or that begins
//!   with the earlier convention "No request" followed by punctuation, says nothing and shows nothing.
//! - No double-speaking: within one run (Pi's run, from one settlement to the next, steered input included), a
//!   sentence `speak` or an earlier reply of the run already said is not said again by a final reply; only its
//!   other sentences are said. A sentence counts as said when its words appear in that order inside one spoken
//!   line (re-split or trimmed: "On it, the tests pass." covers "The tests pass."). A final reply made entirely of
//!   sentences already spoken says nothing more.
//! - Every finished reply of a run is said: Pi keeps one run across steering, so the user's second question while the
//!   first answer streams makes two replies, each reported when its model turn ends (`crate::harness`).
//! - Only a reply that finished (`stop`, or `length` when cut at the output limit) is said. An aborted turn (the user's
//!   Stop) or a provider error says nothing; the harness reports those itself.
//! - The island's pull-down keeps the last reply as written (`Final::shown`): the final text, else the turn's
//!   `speak` lines when the turn ended silent after speaking.

/// The final reply that means "nothing for the user": the harness swallows it and the turn stays silent.
pub const SILENT: &str = "NO_REPLY";

/// What a settled turn's final text becomes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Final {
    /// Said now through the speak path (voice, or island line with Speaking off): the final text without the
    /// sentences `speak` already said this turn. `None`: nothing more to say.
    pub say: Option<String>,
    /// The last reply as written, for the island's pull-down. `None`: the pull-down keeps the previous reply.
    pub shown: Option<String>,
}

/// One turn's reply state: what `speak` said since the last final reply, and what the run said before it.
#[derive(Clone, Debug, Default)]
pub struct Replies {
    said: Vec<String>,
    /// Lines said earlier in this run (finished replies and their `speak` lines), cleared when the run settles.
    run: Vec<String>,
}
impl Replies {
    /// A `speak` call (or an `ask` question) reached the user during the current turn.
    pub fn said(&mut self, text: &str) {
        if !text.trim().is_empty() {
            self.said.push(text.trim().to_owned());
        }
    }
    /// What this turn said so far, oldest first.
    pub fn spoken(&self) -> &[String] {
        &self.said
    }
    /// The user's Stop or a new process: the turn is over without a reply.
    pub fn reset(&mut self) {
        self.said.clear();
        self.run.clear();
    }
    /// Pi's run settled (a delivery completed): the next reply may say anything again.
    pub fn settled(&mut self) {
        self.run.clear();
    }
    /// A model turn ended with this final text and Pi's `stopReason`; the next turn starts with nothing `speak`-said,
    /// and the run remembers what this one said until it settles.
    pub fn finish(&mut self, text: &str, stop_reason: &str) -> Final {
        let said = std::mem::take(&mut self.said);
        let spoken_lines = (!said.is_empty()).then(|| said.join("\n"));
        if !finished(stop_reason) || is_silent(text) {
            self.run.extend(said);
            return Final { say: None, shown: spoken_lines };
        }
        let known: Vec<String> = self.run.iter().chain(&said).cloned().collect();
        let say = unsaid(text, &known);
        self.run.extend(said);
        self.run.push(text.trim().to_owned());
        Final { say, shown: Some(text.trim().to_owned()) }
    }
}

/// A reply that ended on its own (`stop`) or at the output limit (`length`). An event without a stop reason (an
/// older fixture) counts as finished.
pub fn finished(stop_reason: &str) -> bool {
    matches!(stop_reason, "stop" | "length" | "")
}

/// Whether a final text is the silent reply (see the module rules).
pub fn is_silent(text: &str) -> bool {
    let words: Vec<&str> = text
        .split_whitespace()
        .map(|w| w.trim_matches(|c: char| !(c.is_alphanumeric() || c == '_')))
        .filter(|w| !w.is_empty())
        .collect();
    let (Some(first), Some(last)) = (words.first(), words.last()) else { return true };
    if *first == SILENT || *last == SILENT {
        return true;
    }
    // Earlier sessions ended false wakes with "No request." (and sometimes a reason after it); the conversation
    // history still teaches that habit.
    text.trim_start()
        .strip_prefix("No request")
        .is_some_and(|rest| rest.is_empty() || rest.starts_with(['.', ';', ',', ':', '-', '\u{2014}', '\u{2013}']))
}

/// `text` without the sentences already said this turn, keeping its lines and paragraphs. `None` when nothing is
/// left to say. A sentence was said when its words, in order and adjacent, appear inside one said line, whatever
/// the sentence breaks and marks there.
pub fn unsaid(text: &str, said: &[String]) -> Option<String> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    if said.is_empty() {
        return Some(text.to_owned());
    }
    // Each said line as its words, padded so a containment test matches whole words only.
    let known: Vec<String> = said.iter().map(|s| format!(" {} ", s.lines().map(key).collect::<Vec<_>>().join(" "))).collect();
    let mut out: Vec<String> = vec![];
    for line in text.lines() {
        let (marker, body) = split_marker(line);
        let kept: Vec<String> = sentences(body).into_iter().filter(|s| {
            let k = key(s);
            !k.is_empty() && !known.iter().any(|line| line.contains(&format!(" {k} ")))
        }).collect();
        if !kept.is_empty() {
            out.push(format!("{marker}{}", kept.join(" ")));
        } else if line.trim().is_empty() && out.last().is_some_and(|l| !l.is_empty()) {
            out.push(String::new());
        }
    }
    while out.last().is_some_and(String::is_empty) {
        out.pop();
    }
    (!out.is_empty()).then(|| out.join("\n"))
}

/// The reply as the voice should say it, on one line: list markers and markdown dropped, and every line but the last
/// closed with `.`, `?` or `!` so list items and short lines are said as separate sentences ("- Latency map" and
/// "- Island polish" become "Latency map. Island polish"). A plain one-line reply passes through unchanged.
pub fn for_speech(text: &str) -> String {
    let mut out: Vec<String> = vec![];
    for line in text.lines() {
        let plain = strip_marker(line).replace("**", "").replace("__", "").replace('`', "");
        let plain = plain.trim();
        if plain.is_empty() || plain.chars().all(|c| "|-:=+ ".contains(c)) {
            continue;
        }
        if let Some(last) = out.last_mut() {
            close(last);
        }
        out.push(plain.to_owned());
    }
    if let Some(last) = out.last_mut().filter(|l| l.ends_with([':', ';', ','])) {
        close(last);
    }
    out.join(" ")
}
/// Ends a line with a sentence mark (a trailing `:`, `;` or `,` becomes `.`).
fn close(line: &mut String) {
    match line.chars().last() {
        Some('.' | '?' | '!') => {}
        Some(':' | ';' | ',') => {
            line.pop();
            line.push('.');
        }
        _ => line.push('.'),
    }
}

/// `pl` when the text carries Polish letters, else `en` (the `speak` tool's two languages; the final reply has no
/// language field).
pub fn language(text: &str) -> &'static str {
    if text.chars().any(|c| "ąćęłńśźżĄĆĘŁŃŚŹŻ".contains(c)) { "pl" } else { "en" }
}

/// A line without its list, heading or quote marker.
fn strip_marker(line: &str) -> &str {
    split_marker(line).1
}
/// A line split into its list, heading or quote marker ("- ", "1. ", "## ", with the indent) and the rest.
fn split_marker(line: &str) -> (&str, &str) {
    let start = line.len() - line.trim_start().len();
    let t = &line[start..];
    let t = t.trim_start_matches(['#', '>']).trim_start();
    let at = |rest: &str| line.len() - rest.len();
    for bullet in ["- ", "* ", "+ ", "\u{2022} "] {
        if let Some(rest) = t.strip_prefix(bullet) {
            let rest = rest.trim_start();
            return (&line[..at(rest)], rest);
        }
    }
    // "1. ", "12) ", "a) "
    let digits = t.chars().take_while(char::is_ascii_digit).count();
    let end = if digits > 0 { Some(digits) } else { t.chars().next().filter(char::is_ascii_lowercase).map(|_| 1).filter(|_| t[1..].starts_with(") ")) };
    if let Some(end) = end {
        if let Some(rest) = t[end..].strip_prefix(". ").or_else(|| t[end..].strip_prefix(") ")) {
            let rest = rest.trim_start();
            return (&line[..at(rest)], rest);
        }
    }
    (&line[..at(t)], t)
}

/// A line's sentences, each ending at `.`, `?` or `!` followed by a space or the line's end.
fn sentences(line: &str) -> Vec<String> {
    let mut out = vec![];
    let mut part = String::new();
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        part.push(c);
        if ".?!".contains(c) && chars.peek().is_none_or(|n| n.is_whitespace()) {
            if !part.trim().is_empty() {
                out.push(part.trim().to_owned());
            }
            part.clear();
        }
    }
    if !part.trim().is_empty() {
        out.push(part.trim().to_owned());
    }
    out
}

/// What two sentences must share to be the same thing said: their words, lowercased, without marks or list markers.
fn key(sentence: &str) -> String {
    strip_marker(sentence)
        .chars()
        .map(|c| if c.is_alphanumeric() { c.to_lowercase().next().unwrap_or(c) } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_silent_reply_is_the_token_or_nothing_and_never_a_real_answer() {
        for silent in [
            "", "   \n", "NO_REPLY", "`NO_REPLY`", "**NO_REPLY**", "NO_REPLY.", "NO_REPLY — User is on a call with Dave.",
            "That was my own voice on the mic. NO_REPLY", "No request.", "No request; waiting for the rest of the sentence.",
            "No request. \"Yes, I can hear you.\" is word for word what I just said.", "No request",
        ] {
            assert!(is_silent(silent), "{silent:?} should stay silent");
        }
        for said in [
            "Yes, I can hear you.", "No request went to the worker yet; I can start one now.", "No.",
            "The token NO_REPLY_X is not ours.", "Nothing new on the build, it is still green.",
        ] {
            assert!(!is_silent(said), "{said:?} must be said");
        }
    }

    #[test]
    fn a_finished_reply_is_said_and_shown_whole() {
        let mut r = Replies::default();
        let f = r.finish("The build is green.\nAll 222 tests passed.", "stop");
        assert_eq!(f.say.as_deref(), Some("The build is green.\nAll 222 tests passed."));
        assert_eq!(f.shown.as_deref(), Some("The build is green.\nAll 222 tests passed."));
        // Cut at the output limit: still the reply.
        assert!(r.finish("A long answer that ran out of", "length").say.is_some());
    }

    #[test]
    fn aborted_or_failed_turns_say_nothing() {
        let mut r = Replies::default();
        for reason in ["aborted", "error"] {
            assert_eq!(r.finish("Half an answer", reason), Final::default(), "{reason}");
        }
        r.said("On it.");
        let f = r.finish("Half an answer", "aborted");
        assert_eq!(f.say, None);
        assert_eq!(f.shown.as_deref(), Some("On it."), "what was said stays readable");
        assert!(r.spoken().is_empty(), "the next turn starts empty");
    }

    #[test]
    fn speak_then_final_never_says_a_sentence_twice() {
        let mut r = Replies::default();
        // The whole answer was spoken: the final repeat says nothing more, but the pull-down shows the reply.
        r.said("Started a Haiku thread. I'll tell you when it replies.");
        let f = r.finish("Started a Haiku thread. I'll tell you when it replies.", "stop");
        assert_eq!(f.say, None);
        assert_eq!(f.shown.as_deref(), Some("Started a Haiku thread. I'll tell you when it replies."));
        // Part of it was spoken: only the new sentences, regardless of case, marks or a list marker.
        r.said("On it.");
        r.said("The tests are running");
        let f = r.finish("On it! The tests are running.\n\n- All 40 passed.\n- the tests are RUNNING.", "stop");
        assert_eq!(f.say.as_deref(), Some("- All 40 passed."), "the list item keeps its marker; the voice drops it");
        // Different words are a different sentence.
        r.said("Checking the logs.");
        assert_eq!(r.finish("I checked the logs: the speak tool failed twice.", "stop").say.as_deref(), Some("I checked the logs: the speak tool failed twice."));
    }

    #[test]
    fn a_sentence_said_inside_a_longer_or_re_split_line_is_not_said_again() {
        // Re-split: the voice said one sentence, the reply splits it in two (and the reverse).
        let said = vec!["On it, the tests pass. All 40 of them.".to_owned()];
        assert_eq!(unsaid("The tests pass! All 40 of them.\nThe build is green.", &said).as_deref(), Some("The build is green."));
        let said = vec!["The tests pass.".to_owned(), "All 40.".to_owned()];
        assert_eq!(unsaid("The tests pass, all 40.", &said).as_deref(), Some("The tests pass, all 40."), "words split across two lines are two lines");
        // Trimmed: a final sentence that is a run of words inside a spoken sentence.
        let said = vec!["The build is green and all 222 tests passed.".to_owned()];
        assert_eq!(unsaid("All 222 tests passed. Shall I push?", &said).as_deref(), Some("Shall I push?"));
        // Whole words only, in order, adjacent.
        let said = vec!["Retesting the build.".to_owned()];
        assert_eq!(unsaid("Testing the build.", &said).as_deref(), Some("Testing the build."));
        let said = vec!["The tests passed on the build.".to_owned()];
        assert_eq!(unsaid("The build passed.", &said).as_deref(), Some("The build passed."));
        assert_eq!(unsaid("Passed the tests.", &said).as_deref(), Some("Passed the tests."));
    }

    #[test]
    fn a_run_says_each_reply_once_until_it_settles() {
        let mut r = Replies::default();
        // The user asks A; while A's reply streams he adds B. Both replies are said, B without A's repeated sentence.
        r.said("Running the tests.");
        let a = r.finish("All 222 tests passed.", "stop");
        assert_eq!(a.say.as_deref(), Some("All 222 tests passed."));
        let b = r.finish("All 222 tests passed. It is 14:05.", "stop");
        assert_eq!(b.say.as_deref(), Some("It is 14:05."));
        assert_eq!(b.shown.as_deref(), Some("All 222 tests passed. It is 14:05."));
        assert_eq!(r.finish("Running the tests.", "stop").say, None, "speak lines of the run count too");
        // The run settles: the next run may say the same words.
        r.settled();
        assert_eq!(r.finish("All 222 tests passed.", "stop").say.as_deref(), Some("All 222 tests passed."));
        r.reset();
        assert_eq!(r.finish("All 222 tests passed.", "stop").say.as_deref(), Some("All 222 tests passed."));
    }

    #[test]
    fn a_silent_end_after_speaking_says_nothing_more_and_shows_the_spoken_lines() {
        let mut r = Replies::default();
        r.said("Noted. The animations are slow.");
        let f = r.finish("NO_REPLY", "stop");
        assert_eq!(f.say, None);
        assert_eq!(f.shown.as_deref(), Some("Noted. The animations are slow."));
        // A silent turn with nothing said leaves the pull-down alone.
        assert_eq!(r.finish("NO_REPLY", "stop"), Final::default());
    }

    #[test]
    fn each_turn_starts_with_nothing_said() {
        let mut r = Replies::default();
        r.said("Checking.");
        r.finish("Checking. Done.", "stop");
        r.settled();
        assert_eq!(r.finish("Checking.", "stop").say.as_deref(), Some("Checking."), "a new run may say it again");
        r.said("Checking.");
        r.reset();
        assert_eq!(r.finish("Checking.", "stop").say.as_deref(), Some("Checking."));
    }

    #[test]
    fn unsaid_keeps_lines_and_paragraphs() {
        let said = vec!["First.".to_owned()];
        assert_eq!(unsaid("First. Second.\n\nThird.\nFourth.", &said).as_deref(), Some("Second.\n\nThird.\nFourth."));
        assert_eq!(unsaid("First.\n\n\nSecond.", &said).as_deref(), Some("Second."));
        assert_eq!(unsaid("  ", &[]), None);
        // A list item keeps its marker; a spoken item goes, marker and all.
        let said = vec!["Bug note tool.".to_owned()];
        assert_eq!(unsaid("Next:\n1. Bug note tool.\n2. Latency map.", &said).as_deref(), Some("Next:\n2. Latency map."));
        // Decimals and versions are not sentence ends.
        assert_eq!(sentences("Pi 1.2.0 runs Opus 5.5 now. Good."), ["Pi 1.2.0 runs Opus 5.5 now.", "Good."]);
    }

    #[test]
    fn speech_form_closes_every_line_and_drops_markup() {
        assert_eq!(for_speech("Two fixes first:\n- Latency map\n- **Island** polish\n1. Bug note tool\n2) `vault` JSON"),
            "Two fixes first. Latency map. Island polish. Bug note tool. vault JSON");
        assert_eq!(for_speech("## Result\nDone!\n| a | b |\n|---|---|\nIs that all?"), "Result. Done! | a | b |. Is that all?");
        assert_eq!(for_speech("Plain sentence."), "Plain sentence.");
        assert_eq!(for_speech("fixture reply"), "fixture reply", "one plain line passes through");
        assert_eq!(for_speech("Pi 1.2.0 is pinned\n"), "Pi 1.2.0 is pinned");
        assert_eq!(for_speech("Next steps:"), "Next steps.");
    }

    #[test]
    fn polish_letters_choose_the_polish_voice() {
        assert_eq!(language("Zbudowałem nową wersję."), "pl");
        assert_eq!(language("The build is green."), "en");
        assert_eq!(language("Ok, gotowe"), "en", "no Polish letters: English");
    }
}
