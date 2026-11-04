//! Spoken choices for an `ask` card's options (25 Oct, the user: "ABC is useless": the options are numbered 1 to 4). One
//! utterance, read against the card's current item: a number ("2", "two", "the first one"), a list for multi-select
//! ("1 and 3", "one and three", "both", "all"), an ordinal ("the second one") or the option's exact label. A letter
//! ("B", "option B") is still taken silently for older habits. It returns the option indices it names, or nothing.
//!
//! Nothing is guessed: any word the parser does not know, a letter past the last option, two picks on a single
//! select, "both" or "none" outside the cases below, makes the utterance ordinary talk, and ordinary talk closes the
//! card with his words as before (questions.rs). "none" is never a pick: it is talk, and the agent reads it as no.

/// The letters older habits still say (a silent fallback; the card draws numbers): at most four options.
const LETTERS: [&str; 4] = ["a", "b", "c", "d"];
const ORDINALS: [&str; 4] = ["first", "second", "third", "fourth"];
/// Words that may lead a choice ("option B", "the second one"); only these are dropped from the front.
const LEAD: [&str; 5] = ["the", "option", "letter", "choice", "number"];
/// Words that may follow an ordinal ("the second one").
const TAIL: [&str; 4] = ["one", "option", "choice", "answer"];
/// A choice is short: a long sentence is talk even when it contains letters.
const MAX_TOKENS: usize = 8;

/// The option indices the user's utterance `text` picks from `labels`, in option order, without repeats. `multi` is
/// whether the item is multi-select. `None` when the utterance is not a choice.
pub(crate) fn parse(text: &str, labels: &[String], multi: bool) -> Option<Vec<usize>> {
    let said = clean(text);
    if said.is_empty() || labels.is_empty() {
        return None;
    }
    // His words are exactly one option's label: that option, as the card has always taken them.
    let named: Vec<usize> = labels.iter().enumerate().filter(|(_, l)| clean(l) == said).map(|(i, _)| i).collect();
    if named.len() == 1 {
        return Some(named);
    }
    if !named.is_empty() {
        return None;
    }
    // A bare "a" is the article, not option A (R5): "option a" and "A." still name the letter.
    if text.trim().eq_ignore_ascii_case("a") {
        return None;
    }
    let mut tokens: Vec<String> = said
        .chars()
        .map(|c| if matches!(c, ',' | ';' | '&' | '/') { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .filter(|w| *w != "and")
        .map(|w| w.trim_matches(['(', ')', '.', '\'', '"']).to_owned())
        .filter(|w| !w.is_empty())
        .collect();
    while tokens.len() > 1 && LEAD.contains(&tokens[0].as_str()) {
        tokens.remove(0);
    }
    if tokens.is_empty() || tokens.len() > MAX_TOKENS {
        return None;
    }
    let picks = match tokens.as_slice() {
        [word] if word == "both" => return (multi && labels.len() == 2).then(|| vec![0, 1]),
        [word] if word == "all" => return multi.then(|| (0..labels.len()).collect()),
        [word] if word == "none" || word == "nothing" => return None,
        [word, rest @ ..] if rest.is_empty() || (rest.len() == 1 && TAIL.contains(&rest[0].as_str())) => {
            if let Some(i) = ORDINALS.iter().position(|o| *o == word.as_str()) {
                return (i < labels.len()).then(|| vec![i]);
            }
            tokens.iter().map(|t| index_of(t, labels.len())).collect::<Option<Vec<usize>>>()?
        }
        _ => tokens.iter().map(|t| index_of(t, labels.len())).collect::<Option<Vec<usize>>>()?,
    };
    let mut picks = picks;
    picks.sort_unstable();
    picks.dedup();
    (multi || picks.len() == 1).then_some(picks)
}

/// The option index a number token ("2", "two") or a letter names, when the card has that option.
fn index_of(token: &str, options: usize) -> Option<usize> {
    let i = match token {
        "1" | "one" => 0,
        "2" | "two" => 1,
        "3" | "three" => 2,
        "4" | "four" => 3,
        _ => LETTERS.iter().position(|l| *l == token)?,
    };
    (i < options).then_some(i)
}

/// Lower case, single spaces, the ends trimmed of sentence punctuation and quotes.
fn clean(text: &str) -> String {
    let lower = text.to_lowercase().replace(['’', '‘'], "'");
    let collapsed = lower.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed.trim_matches(|c: char| matches!(c, '.' | '!' | '?' | ' ' | '"' | '“' | '”')).to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }
    fn branches() -> Vec<String> {
        labels(&["resampler-old", "resampler-v2", "meter-fix", "stash-2024"])
    }

    #[test]
    fn a_letter_names_its_option_however_it_is_said() {
        let four = branches();
        assert_eq!(parse("B", &four, false), Some(vec![1]));
        assert_eq!(parse("b.", &four, false), Some(vec![1]));
        assert_eq!(parse("Option B", &four, false), Some(vec![1]));
        assert_eq!(parse("letter c!", &four, false), Some(vec![2]));
        assert_eq!(parse("the D", &four, false), Some(vec![3]));
        assert_eq!(parse("2", &four, false), Some(vec![1]), "a digit names the option at that place");
    }

    #[test]
    fn numbers_name_their_options_in_words_or_digits() {
        let four = branches();
        assert_eq!(parse("one", &four, false), Some(vec![0]));
        assert_eq!(parse("Two.", &four, false), Some(vec![1]));
        assert_eq!(parse("the first one", &four, false), Some(vec![0]));
        assert_eq!(parse("the three", &four, false), Some(vec![2]));
        assert_eq!(parse("1 and 3", &four, true), Some(vec![0, 2]));
        assert_eq!(parse("one and three", &four, true), Some(vec![0, 2]));
        assert_eq!(parse("two, four", &four, true), Some(vec![1, 3]));
        assert_eq!(parse("one and three", &four, false), None, "two picks on a single select are talk");
        assert_eq!(parse("five", &four, true), None, "no option five");
        assert_eq!(parse("both", &labels(&["Yes", "No"]), true), Some(vec![0, 1]));
        assert_eq!(parse("all", &four, true), Some(vec![0, 1, 2, 3]));
        assert_eq!(parse("B", &four, false), Some(vec![1]), "a letter is still taken, silently");
    }

    #[test]
    fn a_bare_article_is_talk_and_a_marked_a_is_the_option() {
        let two = labels(&["Yes", "No"]);
        assert_eq!(parse("a", &two, false), None, "the article, said alone");
        assert_eq!(parse("A", &two, false), None, "the same word, capitalised");
        assert_eq!(parse("a", &branches(), true), None, "not even in a multi-select");
        assert_eq!(parse("option a", &two, false), Some(vec![0]));
        assert_eq!(parse("A.", &two, false), Some(vec![0]));
        assert_eq!(parse("letter a!", &two, false), Some(vec![0]));
        assert_eq!(parse("a and b", &branches(), true), Some(vec![0, 1]), "in a list it is a letter");
        assert_eq!(parse("B", &branches(), false), Some(vec![1]), "other letters stay bare choices");
        assert_eq!(parse("b.", &branches(), false), Some(vec![1]));
    }

    #[test]
    fn a_letter_past_the_last_option_is_talk() {
        let two = labels(&["Yes", "No"]);
        assert_eq!(parse("D", &two, false), None);
        assert_eq!(parse("3", &two, false), None);
    }

    #[test]
    fn ordinals_name_their_place() {
        let four = branches();
        assert_eq!(parse("the first one", &four, false), Some(vec![0]));
        assert_eq!(parse("the second one", &four, false), Some(vec![1]));
        assert_eq!(parse("third", &four, false), Some(vec![2]));
        assert_eq!(parse("the fourth option", &four, false), Some(vec![3]));
        assert_eq!(parse("the third one", &labels(&["Yes", "No"]), false), None, "no third place on two options");
        assert_eq!(parse("the second one", &labels(&["Yes", "No"]), false), Some(vec![1]));
    }

    #[test]
    fn an_exact_label_is_its_option_as_before() {
        let two = labels(&["Yes", "No"]);
        assert_eq!(parse("Yes", &two, false), Some(vec![0]));
        assert_eq!(parse("yes.", &two, false), Some(vec![0]));
        assert_eq!(parse("resampler-v2", &branches(), true), Some(vec![1]), "a multi-select label is one pick");
        assert_eq!(parse("Yes, but let's keep it as a separate project", &two, false), None, "talk, not a choice");
    }

    #[test]
    fn several_letters_pick_several_options_on_a_multi_select() {
        let four = branches();
        assert_eq!(parse("A and C", &four, true), Some(vec![0, 2]));
        assert_eq!(parse("A, C", &four, true), Some(vec![0, 2]));
        assert_eq!(parse("a & c", &four, true), Some(vec![0, 2]));
        assert_eq!(parse("C, A and B", &four, true), Some(vec![0, 1, 2]), "sorted into option order");
        assert_eq!(parse("A and A", &four, true), Some(vec![0]), "a repeat is one pick");
        assert_eq!(parse("the first and the second", &four, true), None, "ordinal lists are not taken");
    }

    #[test]
    fn two_picks_on_a_single_select_are_talk() {
        assert_eq!(parse("A and C", &branches(), false), None);
    }

    #[test]
    fn both_and_all_cover_the_options_only_where_that_is_plain() {
        let two = labels(&["Yes", "No"]);
        assert_eq!(parse("both", &two, true), Some(vec![0, 1]));
        assert_eq!(parse("both", &two, false), None, "both of one choice is nonsense");
        assert_eq!(parse("both", &branches(), true), None, "four options: which two?");
        assert_eq!(parse("all", &branches(), true), Some(vec![0, 1, 2, 3]));
        assert_eq!(parse("all", &branches(), false), None);
    }

    #[test]
    fn none_is_talk_not_a_pick() {
        assert_eq!(parse("none", &branches(), true), None);
        assert_eq!(parse("nothing", &branches(), true), None);
    }

    #[test]
    fn words_the_parser_does_not_know_are_never_guessed() {
        let four = branches();
        for talk in ["", "   ", "um", "okay", "A or C", "I think B", "let's do the meter one", "B is fine with me", "Ba", "bee", "yes", "sure"] {
            assert_eq!(parse(talk, &four, true), None, "{talk:?} is not a choice");
        }
        assert_eq!(parse("sure", &labels(&["Yes", "No"]), false), None, "a synonym is not a label");
    }

    #[test]
    fn a_long_sentence_with_letters_in_it_is_talk() {
        assert_eq!(parse("a b c d a b c d a b", &branches(), true), None);
    }
}
