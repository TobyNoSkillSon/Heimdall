//! Voice answers to `ask` cards (night-ask, 20 Oct; review fixes 20 Oct). "If I'm talking to you
//! while a question is pending, me talking should cancel the question" — card answers "should not be queued … or they
//! should just be overwritten by the voice". Only speech that is for Heimdall counts: fillers, asides to other people,
//! TV and call audio neither wake the agent past the decider nor close or override a card.
//!
//! Which spoken lines count: the user's utterances the decider (d1) sent to Heimdall, or his name or "send it" did
//! (`Tools::spoke_to_heimdall`). d1 stays in the loop while a card is up; it sees the question as Heimdall's last turn
//! (`Tools::question_asked`). For an `ANSWER_WINDOW_NS` after the card appears his speech is sent at a lower d1 score
//! (`OPEN_CARD_SEND`, `card_verdict`): 19 Oct 18:45:00 "Yes, but let's keep it as a separate project", his answer to the
//! card, scored send 0.41 against not_for_heimdall 0.51. The 30-second follow-up window does not bypass d1 while a card
//! is answering (`card_trigger`). A filler ("um", "hmm") never wakes the agent while a card is up and never closes
//! or overrides one (`filler_only`).
//!
//! Talk never closes a card ("that question should still be there"). His words are a candidate answer
//! only: a spoken letter that matches an option answers the card (ask_voice.rs); any other talk leaves it open, and the
//! agent is told it stays open. It closes on his click, Option and a digit, typed Other, or the agent's `ask_close`
//! once his words answer it.
//!
//! - One card per spoken turn: the oldest card still shown, which is the one the island shows. A card the agent
//!   closed, or that was clicked or cancelled, is skipped (`Tools::open_cards`).
//! - Speech that ended before the card appeared is not an answer to it (18:44:25 on 19 Oct ended before the 18:44:33
//!   question).
//! - A guarded T3 confirmation stays open (`Actions::needs_click`): only his click approves it.
//! - A click is the card's answer whenever it lands while the card is open, including while he is talking.

/// A card on the island.
#[derive(Clone, Debug)]
pub(crate) struct OpenCard {
    pub id: String,
    pub question: String,
    /// Host clock, when the card was shown.
    pub shown_ns: u64,
    /// A guarded confirmation: only a click answers it.
    pub click_only: bool,
}
/// The user's click that answered a card.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CardClick {
    pub id: String,
    pub question: String,
    pub choice: String,
    pub shown_ns: u64,
    /// Host clock, when the click reached the app.
    pub at_ns: u64,
}
/// One of the user's spoken lines in the wake (host clock).
#[derive(Clone, Debug)]
pub(crate) struct Spoken {
    pub start_ns: u64,
    pub end_ns: u64,
    pub text: String,
}
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Plan {
    /// The card his words close, with the words it shows.
    pub close: Option<(String, String)>,
    /// Each of his utterances that closes it, in order: a wake's choices are parsed one by one (R9, ask_voice.rs).
    pub close_lines: Vec<String>,
    /// A click-only card he spoke to: it stays.
    pub kept: Option<String>,
}
/// How long after a card appears the user's talk is sent to the agent at the lower d1 score (`OPEN_CARD_SEND`): the next
/// thing he says to an immediate question (he answered after 27 s on 19 Oct). Later talk is judged by d1 as usual,
/// which still sees the question as Heimdall's last turn.
pub(crate) const ANSWER_WINDOW_NS: u64 = 60_000_000_000;
/// d1's send probability that sends the user's speech while a card is answering, instead of d1's top choice. Measured on
/// 710 d1 decisions of 14–19 Oct: every line of the user's that d1 did not send at send ≥ 0.35 was addressed to
/// Heimdall (the 19 Oct card answer scored 0.41); asides and other speakers sat below.
pub(crate) const OPEN_CARD_SEND: f32 = 0.35;
/// Whether a mic utterance is talk while the card shown at `shown_ns` was up, inside the answer window.
pub(crate) fn answers(shown_ns: u64, start_ns: u64, end_ns: u64) -> bool {
    end_ns > shown_ns && start_ns <= shown_ns + ANSWER_WINDOW_NS
}
/// The cards on screen as the user's utterance sees them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CardState {
    /// No card is up.
    None,
    /// A card is up, but this utterance is outside its answer window.
    Open,
    /// The utterance falls in a shown card's answer window.
    Answering,
}
pub(crate) fn card_state(shown: impl IntoIterator<Item = u64>, start_ns: u64, end_ns: u64) -> CardState {
    let mut state = CardState::None;
    for shown_ns in shown {
        if answers(shown_ns, start_ns, end_ns) { return CardState::Answering; }
        state = CardState::Open;
    }
    state
}
/// Hesitation sounds and filler words with nothing else: never an answer, a wake or an override while a card is up.
/// "mm-hmm", "uh-huh", "okay" and "yeah" are answers, not fillers.
pub(crate) fn filler_only(text: &str) -> bool {
    const FILLERS: &[&str] = &[
        "um", "umm", "ummm", "uh", "uhh", "uhhh", "er", "err", "erm", "ehm", "em", "hm", "hmm", "hmmm", "mm", "mmm", "ah", "ahh",
        "eh", "ehh", "oh", "huh", "yyy", "eee", "so", "well", "like", "you", "know", "i", "mean",
    ];
    let lower = text.to_lowercase();
    if ["mm-hmm", "mhm", "uh-huh", "uh huh", "mm hmm"].iter().any(|yes| lower.contains(yes)) { return false; }
    let mut words = lower.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).peekable();
    words.peek().is_some() && words.all(|w| FILLERS.contains(&w))
}
/// d1's verdict on the user's speech, given the cards on screen. `argmax_send`: d1's top choice was send.
pub(crate) fn card_verdict(card: CardState, text: &str, argmax_send: bool, p_send: f32) -> Option<decide::send::Trigger> {
    use decide::send::Trigger;
    if card != CardState::None && filler_only(text) { return None; }
    if argmax_send { return Some(Trigger::D1); }
    (card == CardState::Answering && p_send >= OPEN_CARD_SEND).then_some(Trigger::OpenQuestion)
}
/// The explicit cue for the user's speech, given the cards on screen: inside a card's answer window the 30-second
/// follow-up window does not bypass d1 (fillers and asides right after a spoken question would wake the agent and
/// close the card). His name and "send it" stay explicit.
pub(crate) fn card_trigger(explicit: Option<decide::send::Trigger>, card: CardState) -> Option<decide::send::Trigger> {
    match explicit {
        Some(decide::send::Trigger::FollowUp) if card == CardState::Answering => None,
        other => other,
    }
}
/// Whether a wake trigger means the speech was for Heimdall (a counted line, module docs).
pub(crate) fn for_heimdall(trigger: decide::send::Trigger) -> bool {
    use decide::send::Trigger;
    matches!(trigger, Trigger::D1 | Trigger::Name | Trigger::SendIt | Trigger::OpenQuestion | Trigger::Typed | Trigger::Dropped)
}
/// Clicks remembered for the override check: a decision and a wake take well under this.
pub(crate) const CLICK_MEMORY_NS: u64 = 120_000_000_000;

pub(crate) fn plan(open: &[OpenCard], spoken: &[Spoken]) -> Plan {
    let mut plan = Plan::default();
    let Some(last_end) = spoken.iter().map(|s| s.end_ns).max() else { return plan };
    let mut cards: Vec<&OpenCard> = open.iter().filter(|c| c.shown_ns < last_end).collect();
    cards.sort_by_key(|c| c.shown_ns);
    if let Some(card) = cards.first() {
        if card.click_only {
            plan.kept = Some(card.id.clone());
        } else {
            let lines: Vec<&str> = spoken.iter().filter(|s| s.end_ns > card.shown_ns).map(|s| s.text.trim()).filter(|t| !t.is_empty()).collect();
            plan.close = Some((card.id.clone(), lines.join(" ")));
            plan.close_lines = lines.iter().map(|l| l.to_string()).collect();
        }
    }
    // He began speaking after the card appeared and before he clicked it: the words came first.
    plan
}
/// The lines the wake carries for the agent. `answered`: his spoken choice answered the card (it retracted).
pub(crate) fn notes(plan: &Plan, open: &[OpenCard], answered: bool) -> Vec<String> {
    let question = |id: &str| open.iter().find(|c| c.id == id).map(|c| c.question.clone()).unwrap_or_default();
    let mut lines = Vec::new();
    if let (Some((id, _)), false) = (&plan.close, answered) {
        lines.push(format!("Your question {:?} ({id}) stays open: User spoke, and only his click, Option and a digit, typed Other, or your ask_close answers it.", question(id)));
    }
    if let Some(id) = &plan.kept {
        lines.push(format!("Your question {:?} ({id}) stays open: only his click approves it.", question(id)));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    const S: u64 = 1_000_000_000;
    fn card(id: &str, shown: u64) -> OpenCard {
        OpenCard { id: id.into(), question: format!("{id}?"), shown_ns: shown * S, click_only: false }
    }
    fn said(start: u64, end: u64, text: &str) -> Spoken {
        Spoken { start_ns: start * S, end_ns: end * S, text: text.into() }
    }
    fn click(id: &str, shown: u64, at: u64, choice: &str) -> CardClick {
        CardClick { id: id.into(), question: format!("{id}?"), choice: choice.into(), shown_ns: shown * S, at_ns: at * S }
    }

    /// 19 Oct 18:44:33 "Start a thread on one test desktop widget?"; 18:45:00 "Yes, but let's keep it as a separate
    /// project": a candidate answer only. Talk leaves the card open (25 Oct); the agent's ask_close answers it.
    #[test]
    fn a_spoken_turn_is_a_candidate_answer_and_the_card_stays_open() {
        let open = [card("ask-widget", 100)];
        let p = plan(&open, &[said(127, 133, "Yes, but let's keep it as a separate project for Heimdall, okay?")]);
        assert_eq!(p.close, Some(("ask-widget".into(), "Yes, but let's keep it as a separate project for Heimdall, okay?".into())));
        let lines = notes(&p, &open, false);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].starts_with("Your question \"ask-widget?\" (ask-widget) stays open: User spoke"), "{}", lines[0]);
    }

    /// 18:44:25 "That would be really cool actually" ended before the 18:44:33 card: not an answer to it.
    #[test]
    fn speech_that_ended_before_the_card_appeared_leaves_it_open() {
        let open = [card("ask-widget", 100)];
        assert_eq!(plan(&open, &[said(92, 94, "That would be really cool actually.")]), Plan::default());
        // Still speaking when it appeared: he was talking while it was pending.
        assert!(plan(&open, &[said(98, 101, "yeah do it")]).close.is_some());
        // A wake with no words of the user's (another speaker's line, a screenshot) closes nothing.
        assert_eq!(plan(&open, &[]), Plan::default());
    }

    /// Two cards: one spoken turn answers the one on screen (the oldest); the next turn answers the next.
    #[test]
    fn two_cards_take_each_spoken_turn_oldest_first() {
        let open = [card("ask-b", 110), card("ask-a", 100)];
        assert_eq!(plan(&open, &[said(120, 122, "later")]).close, Some(("ask-a".into(), "later".into())));
        let open = [card("ask-b", 110)];
        assert_eq!(plan(&open, &[said(130, 131, "no")]).close, Some(("ask-b".into(), "no".into())));
        // Only the words said while the card was up are shown on it.
        let open = [card("ask-c", 200)];
        let p = plan(&open, &[said(150, 152, "an aside before"), said(201, 203, "Yes."), said(204, 206, "and the island too")]);
        assert_eq!(p.close, Some(("ask-c".into(), "Yes. and the island too".into())));
    }

    /// A guarded T3 confirmation: his voice never approves it, so the card stays and nothing behind it closes.
    #[test]
    fn a_click_only_confirmation_stays_open_and_shields_the_cards_behind_it() {
        let open = [OpenCard { click_only: true, ..card("ask-guard", 100) }, card("ask-other", 105)];
        let p = plan(&open, &[said(110, 112, "yes delete it")]);
        assert_eq!((p.close, p.kept.as_deref()), (None, Some("ask-guard")));
        assert_eq!(notes(&plan(&open, &[said(110, 112, "yes")]), &open, false), vec!["Your question \"ask-guard?\" (ask-guard) stays open: only his click approves it.".to_string()]);
    }

    /// D2 (19 Oct 18:45:00): talk while a card is up, inside its minute, gets the card's lower d1 bar; talk that
    /// ended before it appeared, or began after the window, is d1's as usual.
    #[test]
    fn talk_inside_the_answer_window_is_answering_and_outside_it_the_card_is_only_open() {
        assert!(answers(100 * S, 127 * S, 133 * S));
        assert!(answers(100 * S, 98 * S, 101 * S), "still talking when it appeared");
        assert!(!answers(100 * S, 92 * S, 94 * S), "ended before it appeared");
        assert!(answers(100 * S, 160 * S, 163 * S), "began at the edge of the window");
        assert!(!answers(100 * S, 161 * S, 164 * S), "began after the window");
        assert_eq!(card_state([], 10 * S, 11 * S), CardState::None);
        assert_eq!(card_state([100 * S], 161 * S, 164 * S), CardState::Open);
        assert_eq!(card_state([100 * S, 150 * S], 161 * S, 164 * S), CardState::Answering, "the newer card's window");
    }

    /// Fillers carry nothing: never a wake, a close or an override while a card is up. Short answers are answers.
    #[test]
    fn fillers_are_hesitation_alone_and_short_answers_are_not_fillers() {
        for f in ["Um.", "uh", "Hmm...", "Uh, um.", "erm", "Mm.", "Oh.", "So, like, you know.", "I mean, uh", "Yyy", "  Ah  "] {
            assert!(filler_only(f), "{f:?} is a filler");
        }
        for a in ["Yes.", "No.", "Yeah.", "Okay.", "Later.", "Mm-hmm.", "mhm", "Uh-huh.", "Uh, yes.", "Um, no, wait.", "Wait.",
            "Oh no.", "Hmm, do it.", "So yeah.", "", "   ", "..."] {
            assert!(!filler_only(a), "{a:?} is not a filler");
        }
    }

    /// d1 stays in the loop while a card is up. Measured on the 19 Oct D2 line: send 0.41 against not_for_heimdall 0.51.
    #[test]
    fn a_card_lowers_the_d1_bar_inside_its_window_and_fillers_never_wake() {
        use decide::send::Trigger;
        let answer = "Yes, but let's keep it as a separate project";
        // No card: d1's top choice, as before.
        assert_eq!(card_verdict(CardState::None, answer, false, 0.41), None);
        assert_eq!(card_verdict(CardState::None, "Um.", true, 0.72), Some(Trigger::D1), "no card: a filler is d1's call");
        // Answering: the lower bar.
        assert_eq!(card_verdict(CardState::Answering, answer, false, 0.41), Some(Trigger::OpenQuestion));
        assert_eq!(card_verdict(CardState::Answering, answer, false, OPEN_CARD_SEND), Some(Trigger::OpenQuestion), "inclusive");
        assert_eq!(card_verdict(CardState::Answering, "Alex are you coming", false, 0.34), None, "an aside below the bar");
        assert_eq!(card_verdict(CardState::Answering, "Yes.", true, 0.7), Some(Trigger::D1));
        // Card up but outside the window: d1's top choice.
        assert_eq!(card_verdict(CardState::Open, answer, false, 0.41), None);
        assert_eq!(card_verdict(CardState::Open, "Yes.", true, 0.6), Some(Trigger::D1));
        // Fillers with a card up never wake, whatever d1 says (d1 sent "Um." at 0.72 right after a question).
        for card in [CardState::Answering, CardState::Open] {
            assert_eq!(card_verdict(card, "Um.", true, 0.72), None);
            assert_eq!(card_verdict(card, "Hmm", false, 0.9), None);
        }
    }

    /// The follow-up window (30 s after Heimdall's voice) does not bypass d1 while a card is answering; his name and
    /// "send it" stay explicit. Only speech for Heimdall counts for a card: never the follow-up window alone.
    #[test]
    fn inside_a_cards_window_only_his_name_or_send_it_skip_d1() {
        use decide::send::Trigger;
        assert_eq!(card_trigger(Some(Trigger::FollowUp), CardState::Answering), None);
        assert_eq!(card_trigger(Some(Trigger::FollowUp), CardState::Open), Some(Trigger::FollowUp));
        assert_eq!(card_trigger(Some(Trigger::FollowUp), CardState::None), Some(Trigger::FollowUp));
        for t in [Trigger::Name, Trigger::SendIt] {
            assert_eq!(card_trigger(Some(t), CardState::Answering), Some(t));
        }
        assert_eq!(card_trigger(None, CardState::Answering), None);
        for t in [Trigger::D1, Trigger::Name, Trigger::SendIt, Trigger::OpenQuestion, Trigger::Typed] { assert!(for_heimdall(t)); }
        assert!(!for_heimdall(Trigger::FollowUp));
    }

    /// Talk never closes a card: a click made while he is talking, or after his words, still answers it.
    #[test]
    fn a_click_during_or_after_his_words_stands() {
        let open = [card("ask-w", 100)];
        let p = plan(&open, &[said(120, 130, "yes but separately")]);
        assert_eq!(p.close, Some(("ask-w".into(), "yes but separately".into())));
        let lines = notes(&p, &open, false);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("stays open"), "{}", lines[0]);
    }
}
