//! The question card as the island holds it: picks, tabs, Send, Other, Option plus a digit, voice picks, expiry, and the
//! composer. The card's own rules are in `agent_notice`; these check the island's events and outputs on top of them.

use super::{Approx, Event, Island, Output, Target};
use crate::agent_notice::{AgentNotice, AgentNoticeAction, AskOption, AskQuestion, NoticeAction, NoticeKind, QuestionAnswer};
use crate::island::object::{Class, Display, Scene};

fn island() -> Island {
    Island::new(Display::MBP16, Box::new(Approx))
}

fn question(id: &str, header: Option<&str>, multi: bool, labels: &[&str]) -> AskQuestion {
    AskQuestion {
        id: id.into(),
        header: header.map(str::to_owned),
        text: format!("{id}: which?"),
        multi_select: multi,
        allow_other: true, field: None,
        options: labels.iter().map(|l| AskOption { label: (*l).into(), description: None, grant: vec![] }).collect(),
    }
}

fn card(id: &str, questions: Vec<AskQuestion>, expires_ms: u64) -> Event {
    let mut n = AgentNotice::line(id, "Pick", NoticeKind::Info);
    n.action = Some(NoticeAction::Ask { request_id: id.into(), questions, expires_ms, countdown: false });
    Event::Notice { notice: n, body: None }
}

fn settle(i: &mut Island, t: f64) {
    i.apply(Event::Tick, t);
}

fn answer(id: &str, labels: &[&str], other: Option<&str>) -> QuestionAnswer {
    QuestionAnswer { question_id: id.into(), labels: labels.iter().map(|l| (*l).into()).collect(), other: other.map(str::to_owned) }
}

/// The digit keys follow the card's current tab (2 options, then 4, then 2 again) and drop to none once it closes.
#[test]
fn digit_keys_follow_the_current_tab_and_close_with_the_card() {
    let mut i = island();
    i.apply(card("k", vec![question("k1", None, false, &["A", "B"]), question("k2", None, false, &["A", "B", "C", "D"])], 600_000), 0.1);
    settle(&mut i, 0.5);
    assert_eq!(i.card_keys(), 2, "a two-option tab: digits 1 and 2");
    i.apply(Event::AskTab { id: "k".into(), question: 1 }, 0.6);
    settle(&mut i, 1.0);
    assert_eq!(i.card_keys(), 4, "a four-option tab: digits 1 to 4");
    i.apply(Event::AskTab { id: "k".into(), question: 0 }, 1.1);
    settle(&mut i, 1.5);
    assert_eq!(i.card_keys(), 2, "back on the two-option tab");
    i.apply(Event::CloseQuestion { id: "k".into(), answer: "no".into() }, 1.6);
    settle(&mut i, 2.5);
    assert_eq!(i.card_keys(), 0, "a closed card registers no digit");
}

/// The text of the native field `id` in the layout (`None` when there is no such field).
fn field_text(i: &Island, id: &str) -> Option<String> {
    i.layout().items.iter().find(|it| &*it.id == id).and_then(|it| match &*it.content {
        super::Content::Field { text, .. } => text.clone(),
        _ => None,
    })
}

/// R7: a single-select pick drops the Other answer, and the field the window shows drops its text with it.
#[test]
fn a_pick_clears_the_other_text_the_field_shows() {
    let mut i = island();
    i.apply(card("c", vec![question("q1", None, false, &["A1", "B1"]), question("q2", None, false, &["A2", "B2"])], 600_000), 0.1);
    settle(&mut i, 0.5);
    i.apply(Event::AskOther { id: "c".into(), question: 0, text: "soft".into() }, 0.6);
    settle(&mut i, 0.9);
    assert_eq!(field_text(&i, "q.other.field"), Some("soft".into()));
    i.apply(Event::AskPick { id: "c".into(), question: 0, option: 1 }, 1.0);
    i.apply(Event::AskTab { id: "c".into(), question: 0 }, 1.1);
    settle(&mut i, 1.6);
    assert_eq!(field_text(&i, "q.other.field"), Some(String::new()), "the pick dropped Other, so the field is empty");
}

fn take_notices(i: &mut Island) -> Vec<AgentNoticeAction> {
    i.take_outputs()
        .into_iter()
        .filter_map(|o| match o {
            Output::Notice(a) => Some(a),
            _ => None,
        })
        .collect()
}

#[test]
fn a_single_question_card_answers_by_its_digit_at_once() {
    let mut i = island();
    i.apply(card("c1", vec![question("q1", None, false, &["Yes", "No", "Later"])], 600_000), 0.0);
    settle(&mut i, 1.0);
    assert_eq!(i.card_keys(), 3, "one digit per option");
    i.apply(Event::AskDigit(2), 1.0);
    assert_eq!(take_notices(&mut i), vec![AgentNoticeAction::Answer { request_id: "c1".into(), answers: vec![answer("q1", &["No"], None)], by_hand: vec![] }]);
    i.apply(Event::AskDigit(1), 1.1);
    assert!(take_notices(&mut i).is_empty(), "answered: a second digit answers nothing");
    assert_eq!(i.card_keys(), 0, "no keys once the card is answered");
}

#[test]
fn digits_answer_the_option_on_the_current_tab() {
    let mut i = island();
    let qs = vec![question("q1", Some("Branches"), false, &["A1", "B1"]), question("q2", Some("Push"), false, &["Yes", "No", "Later", "Never"])];
    i.apply(card("c2", qs, 600_000), 0.0);
    settle(&mut i, 1.0);
    assert_eq!(i.card_keys(), 2, "the first tab's two options");
    i.apply(Event::AskTab { id: "c2".into(), question: 1 }, 1.0);
    assert_eq!(i.card_keys(), 4, "the second tab's four options");
    i.apply(Event::AskDigit(3), 1.1);
    assert_eq!(take_notices(&mut i), vec![AgentNoticeAction::Pick { request_id: "c2".into(), answer: answer("q2", &["Later"], None) }]);
    assert_eq!(i.card_keys(), 2, "the single-select pick moved to the next open question, q1");
    i.apply(Event::AskDigit(1), 1.2);
    let out = take_notices(&mut i);
    assert_eq!(out, vec![AgentNoticeAction::Pick { request_id: "c2".into(), answer: answer("q1", &["A1"], None) }], "digit 1 answers q1 on its tab, not q2");
}

#[test]
fn a_digit_past_the_options_or_with_no_card_does_nothing() {
    let mut i = island();
    i.apply(card("c3", vec![question("q1", None, false, &["Yes", "No"])], 600_000), 0.0);
    settle(&mut i, 1.0);
    i.apply(Event::AskDigit(4), 1.0);
    i.apply(Event::AskDigit(0), 1.0);
    assert!(take_notices(&mut i).is_empty());
    let mut empty = island();
    empty.apply(Event::AskDigit(1), 0.0);
    assert!(take_notices(&mut empty).is_empty());
    assert_eq!(empty.card_keys(), 0);
}

/// Bug note 2026-11-02T16-27-21: the user ticked one question of two and found no way to send. Send works once anything
/// is answered; the question he left goes with an empty answer (the app tells the agent it was left unanswered).
#[test]
fn several_questions_send_once_anything_is_answered_and_the_rest_go_unanswered() {
    let mut i = island();
    let qs = vec![question("q1", Some("IDOL"), false, &["x", "y"]), question("q2", Some("Meta"), true, &["x", "y"])];
    i.apply(card("c4", qs, 600_000), 0.0);
    settle(&mut i, 1.0);
    i.apply(Event::AskSend("c4".into()), 1.0);
    assert!(take_notices(&mut i).is_empty(), "nothing picked: Send does nothing");
    i.apply(Event::AskTab { id: "c4".into(), question: 1 }, 1.05);
    i.apply(Event::AskPick { id: "c4".into(), question: 1, option: 1 }, 1.1);
    assert_eq!(take_notices(&mut i), vec![AgentNoticeAction::Pick { request_id: "c4".into(), answer: answer("q2", &["y"], None) }]);
    i.apply(Event::AskSend("c4".into()), 1.2);
    assert_eq!(take_notices(&mut i), vec![AgentNoticeAction::Answer { request_id: "c4".into(), answers: vec![answer("q1", &[], None), answer("q2", &["y"], None)], by_hand: vec![] }]);
    i.apply(Event::AskSend("c4".into()), 1.3);
    assert!(take_notices(&mut i).is_empty(), "sent once");
}

/// Return in the Other field of a card with a question still open brings that question up instead of sending; once
/// none is left, Return sends.
#[test]
fn return_in_other_moves_to_the_open_question_then_sends() {
    let mut i = island();
    let qs = vec![question("q1", Some("A"), false, &["x", "y"]), question("q2", Some("B"), false, &["x", "y"])];
    i.apply(card("c4r", qs, 600_000), 0.0);
    settle(&mut i, 1.0);
    i.apply(Event::AskOther { id: "c4r".into(), question: 0, text: "neither".into() }, 1.0);
    take_notices(&mut i);
    i.apply(Event::AskReturn("c4r".into()), 1.1);
    assert!(take_notices(&mut i).is_empty(), "q2 is open: Return does not send");
    settle(&mut i, 2.0);
    assert!(matches!(i.scene(), Scene::Question { card, .. } if card.tab == 1), "q2 came up: {:?}", i.scene());
    i.apply(Event::AskPick { id: "c4r".into(), question: 1, option: 0 }, 2.1);
    take_notices(&mut i);
    i.apply(Event::AskReturn("c4r".into()), 2.2);
    assert_eq!(take_notices(&mut i), vec![AgentNoticeAction::Answer { request_id: "c4r".into(), answers: vec![answer("q1", &[], Some("neither")), answer("q2", &["x"], None)], by_hand: vec![] }]);
}

/// The card shows which question still needs a pick: each chip leads with a tick once answered and an accent dot
/// while open, and its chip is the way to it. Send sits after the chips, a target only once something is answered.
#[test]
fn chips_mark_the_open_questions_and_send_sits_after_them() {
    let mut i = island();
    let qs = vec![question("q1", Some("IDOL"), false, &["x", "y"]), question("q2", Some("Meta"), true, &["x", "y"])];
    i.apply(card("c4m", qs, 600_000), 0.0);
    settle(&mut i, 1.0);
    let send = |i: &Island| i.targets().iter().find(|(t, _)| *t == Target::AskSend("c4m".into())).map(|(_, r)| *r);
    assert!(send(&i).is_none(), "nothing answered: Send is drawn dim and takes no click");
    i.apply(Event::AskTab { id: "c4m".into(), question: 1 }, 1.0);
    i.apply(Event::AskPick { id: "c4m".into(), question: 1, option: 0 }, 1.1);
    settle(&mut i, 2.0);
    let items = &i.layout().items;
    let has = |id: &str| items.iter().any(|it| &*it.id == id);
    assert!(has("q.chip.0.open.off") && !has("q.chip.0.tick.off"), "IDOL still needs a pick");
    assert!(has("q.chip.1.tick.tab") && !has("q.chip.1.open.tab"), "Meta, the tab on screen, is answered");
    let rect = |id: &str| items.iter().find(|it| &*it.id == id).map(|it| it.rect).unwrap();
    let (chip1, q, disc) = (rect("q.chip.1"), rect("q.q"), rect("q.send"));
    let send = send(&i).expect("Send takes the click once something is answered");
    assert!(disc.x > chip1.max_x() && (disc.y + disc.h / 2.0 - (chip1.y + chip1.h / 2.0)).abs() < 1.0, "Send follows the chips: {disc:?} {chip1:?}");
    assert!(send.w >= 44.0 && send.max_x() <= q.x + 580.0, "a full-size target inside the card");
    assert!(i.targets().iter().any(|(t, _)| *t == Target::AskTab { id: "c4m".into(), question: 0 }), "IDOL's chip takes him to it");
    assert!(!has("q.send.arrow.dim") && has("q.send.arrow.lit"));
}

/// One multi-select question: its Other row's arrow lights with the first tick, not only once words are typed.
#[test]
fn a_single_multi_select_card_lights_send_with_its_first_tick() {
    let mut i = island();
    i.apply(card("c4s", vec![question("q1", None, true, &["x", "y", "z"])], 600_000), 0.0);
    settle(&mut i, 1.0);
    assert!(i.layout().items.iter().any(|it| &*it.id == "q.send.arrow.dim"));
    i.apply(Event::AskPick { id: "c4s".into(), question: 0, option: 2 }, 1.1);
    settle(&mut i, 2.0);
    assert!(i.layout().items.iter().any(|it| &*it.id == "q.send.arrow.lit"));
    let (send, row) = (
        i.targets().iter().find(|(t, _)| *t == Target::AskSend("c4s".into())).map(|(_, r)| *r).expect("a target"),
        i.layout().items.iter().find(|it| &*it.id == "q.other.row").map(|it| it.rect).unwrap(),
    );
    assert!(send.y < row.max_y() && send.max_y() > row.y, "in the Other row");
}

#[test]
fn multi_select_toggles_and_other_text_is_an_answer() {
    let mut i = island();
    i.apply(card("c5", vec![question("q1", None, true, &["alpha", "beta", "gamma"])], 600_000), 0.0);
    settle(&mut i, 1.0);
    i.apply(Event::AskPick { id: "c5".into(), question: 0, option: 2 }, 1.0);
    i.apply(Event::AskPick { id: "c5".into(), question: 0, option: 0 }, 1.1);
    i.apply(Event::AskPick { id: "c5".into(), question: 0, option: 2 }, 1.2);
    assert_eq!(take_notices(&mut i).last(), Some(&AgentNoticeAction::Pick { request_id: "c5".into(), answer: answer("q1", &["alpha"], None) }), "a second click unticks the option");
    i.apply(Event::AskOther { id: "c5".into(), question: 0, text: "  delta  ".into() }, 1.3);
    assert_eq!(take_notices(&mut i).last(), Some(&AgentNoticeAction::Pick { request_id: "c5".into(), answer: answer("q1", &["alpha"], Some("delta")) }));
    i.apply(Event::AskSend("c5".into()), 1.4);
    assert_eq!(take_notices(&mut i), vec![AgentNoticeAction::Answer { request_id: "c5".into(), answers: vec![answer("q1", &["alpha"], Some("delta"))], by_hand: vec![] }]);
}

#[test]
fn a_voice_pick_sets_its_question_and_moves_on_without_sending() {
    let mut i = island();
    let qs = vec![question("q1", None, false, &["A1", "B1"]), question("q2", None, false, &["Y", "N"])];
    i.apply(card("c6", qs, 600_000), 0.0);
    settle(&mut i, 1.0);
    i.apply(Event::AskSet { id: "c6".into(), answer: answer("q1", &["B1"], None) }, 1.0);
    assert!(take_notices(&mut i).is_empty(), "a voice pick is the app's; the island sends nothing");
    assert_eq!(i.card_keys(), 2, "the card is on its second question");
    i.apply(Event::AskDigit(1), 1.1);
    assert_eq!(take_notices(&mut i), vec![AgentNoticeAction::Pick { request_id: "c6".into(), answer: answer("q2", &["Y"], None) }]);
}

#[test]
fn an_expired_card_takes_no_digit_and_no_send() {
    let mut i = island();
    i.apply(card("c7", vec![question("q1", None, false, &["Yes", "No"])], 2_000), 0.0);
    settle(&mut i, 1.0);
    assert_eq!(i.card_keys(), 2);
    settle(&mut i, 3.0);
    assert_eq!(i.card_keys(), 0, "expired: no keys");
    i.apply(Event::AskDigit(1), 3.1);
    assert!(take_notices(&mut i).is_empty());
    assert!(matches!(i.scene(), Scene::Question { card, .. } if card.look == crate::island::object::QLook::Expired));
}

#[test]
fn the_composer_sends_its_text_as_typed_and_nothing_when_empty() {
    let mut i = island();
    i.apply(Event::ComposerSent("   ".into()), 0.0);
    assert!(i.take_outputs().is_empty());
    i.apply(Event::ComposerSent("  call Dave at four  ".into()), 0.0);
    assert_eq!(i.take_outputs(), vec![Output::Typed("call Dave at four".into())]);
}

#[test]
fn a_question_card_is_a_click_target_per_option_and_the_option_is_the_pick() {
    let mut i = island();
    i.apply(card("c8", vec![question("q1", None, false, &["Yes", "No"])], 600_000), 0.0);
    settle(&mut i, 1.0);
    assert_eq!(i.class(), Class::Question);
    let no = i.targets().iter().find(|(t, _)| matches!(t, Target::AskPick { option: 1, .. })).cloned().expect("No is a target");
    assert!(no.1.w >= 500.0 && no.1.h >= 50.0, "a whole row is the target: {:?}", no.1);
    i.apply(Event::Click(no.0), 1.0);
    assert_eq!(take_notices(&mut i), vec![AgentNoticeAction::Answer { request_id: "c8".into(), answers: vec![answer("q1", &["No"], None)], by_hand: vec![] }]);
}

/// A label is one layer: no two text elements carry the same words within 4 pt of each other. The Other placeholder drew
/// twice (the native field's and the layout's, 2 pt apart), so the card shows ghosted text.
#[test]
fn no_label_is_drawn_twice_on_the_card() {
    let mut i = island();
    i.apply(card("c9", vec![question("q1", None, false, &["Yes", "No"]), question("q2", Some("Second"), false, &["A1", "B1"])], 600_000), 0.0);
    settle(&mut i, 1.0);
    let texts: Vec<(String, crate::island::object::Rect)> = i.layout().items.iter().filter_map(|it| match &*it.content {
        super::Content::Text { lines, .. } => Some((lines.join(" "), it.rect)),
        _ => None,
    }).collect();
    assert!(texts.iter().any(|(t, _)| t == "Type your own answer"), "the card has its Other row");
    for (a, (la, ra)) in texts.iter().enumerate() {
        for (lb, rb) in &texts[a + 1..] {
            assert!(!(la == lb && (ra.x - rb.x).abs() < 4.0 && (ra.y - rb.y).abs() < 4.0), "{la:?} is drawn twice: {ra:?} and {rb:?}");
        }
    }
}

/// The Other row is the card's last row: the height of an option row, below numbered options, with its placeholder. No
/// letters and no row of digits sit on the card; Option plus a digit answers silently.
#[test]
fn the_other_row_is_the_last_row_after_the_numbered_options() {
    let mut i = island();
    i.apply(card("c10", vec![question("q1", None, false, &["Yes", "No"])], 600_000), 0.0);
    settle(&mut i, 1.0);
    let items = &i.layout().items;
    let rect = |id: &str| items.iter().find(|it| &*it.id == id).map(|it| it.rect).unwrap_or_else(|| panic!("{id} is drawn"));
    let (no, other) = (rect("q.o1.row"), rect("q.other.row"));
    assert_eq!(other.h, no.h, "the Other row is as tall as an option row");
    assert!(other.y > no.y + no.h, "the Other row sits below the options");
    let text = |id: &str| items.iter().find_map(|it| match (&*it.id == id, &*it.content) {
        (true, super::Content::Text { lines, .. }) => Some(lines.join(" ")),
        _ => None,
    });
    assert_eq!(text("q.o0.digit").as_deref(), Some("1"));
    assert_eq!(text("q.o1.digit").as_deref(), Some("2"));
    assert!(!items.iter().any(|it| it.id.contains(".letter")), "no letters on the card");
    assert_eq!(text("q.other.placeholder").as_deref(), Some("Type your own answer"));
    assert!(!items.iter().any(|it| it.id.contains(".key.")), "no row of digits under the card");
}

/// Bug note 2026-10-28T19-45-55 ("I should have the lip, not the X"): an open card ends in the pull-down's own lip
/// band (the same grabber, as far from the bottom, the same band target) and has no close X.
#[test]
fn an_open_card_ends_in_the_pull_downs_lip_and_has_no_x() {
    let mut i = island();
    i.apply(card("lip", vec![question("q1", None, false, &["Yes", "No"])], 600_000), 0.0);
    settle(&mut i, 1.0);
    assert_eq!(i.class(), Class::Question);
    let l = i.layout().clone();
    assert!(!l.items.iter().any(|it| it.id.contains("dismiss") || matches!(&*it.content, super::Content::Symbol { name, .. } if name == "xmark")), "no close X on the card");
    assert!(!l.targets.iter().any(|(t, _)| matches!(t, Target::Dismiss(_))), "nothing on an open card closes it");
    let grab = l.items.iter().find(|it| matches!(&*it.content, super::Content::Grabber { .. })).expect("the card has the lip's grabber");
    let tuck = l.targets.iter().find(|(t, _)| *t == Target::AskTuck("lip".into())).map(|(_, r)| *r).expect("the lip band is a target");
    // The pull-down's lip, laid out by the same code: the grabber and the band sit the same way on its bottom edge.
    let mut r = island();
    r.apply(Event::OpenChat, 0.0);
    settle(&mut r, 1.0);
    assert_eq!(r.class(), Class::Reply);
    let rl = r.layout().clone();
    let rgrab = rl.items.iter().find(|it| &*it.id == "reply.grabber").expect("the pull-down's grabber");
    let band = rl.targets.iter().find(|(t, _)| *t == Target::ReplyBand).map(|(_, r)| *r).expect("the pull-down's band");
    assert_eq!((grab.rect.w, grab.rect.h, l.h - grab.rect.y), (rgrab.rect.w, rgrab.rect.h, rl.h - rgrab.rect.y), "the same grabber, as far from the bottom");
    assert!(grab.rect.mid_x().abs() < 1e-9, "centred");
    assert_eq!((tuck.h, l.h - tuck.y), (band.h, rl.h - band.y), "the same band target on the bottom edge");
    assert!(tuck.mid_x().abs() < 1e-9 && tuck.w > l.w / 2.0, "the band spans the card inside its rounded corners ({} of {})", tuck.w, l.w);
    // The band is below every answer: a tap on it never picks one.
    let last_row = l.targets.iter().filter(|(t, _)| matches!(t, Target::AskPick { .. } | Target::AskOther { .. })).map(|(_, r)| r.max_y()).fold(0.0, f64::max);
    assert!(grab.rect.y > last_row, "the grabber sits under the last row");
}

/// Tucking a card with its lip leaves the question pending: nothing goes to the agent, the user's picks stay, the island
/// rests with the notch as its target (the peek lip under the pointer), and the notch brings the same card back,
/// still answerable. A click on the lip, and the host's drag up, are the same `AskTuck` click.
#[test]
fn the_lip_tucks_the_card_away_pending_and_the_notch_brings_it_back() {
    let mut i = island();
    i.apply(card("tuck", vec![question("a", Some("One"), false, &["Yes", "No"]), question("b", Some("Two"), false, &["Now", "Later"])], 600_000), 0.0);
    settle(&mut i, 1.0);
    i.apply(Event::Click(Target::AskPick { id: "tuck".into(), question: 0, option: 1 }), 1.0);
    let _ = i.take_outputs();
    settle(&mut i, 1.5);
    assert_eq!(i.card_keys(), 2);
    i.apply(Event::Hover(Some(Target::AskTuck("tuck".into()))), 1.6);
    assert!(matches!(i.scene(), Scene::Question { hot: true, .. }), "the pointer on the band lights the grabber, as on the pull-down");
    i.apply(Event::Click(Target::AskTuck("tuck".into())), 1.7);
    settle(&mut i, 3.0);
    assert!(take_notices(&mut i).is_empty(), "tucking neither answers nor dismisses");
    assert_eq!(i.class(), Class::Rest, "the card is tucked away");
    assert_eq!(i.card_keys(), 0, "no digit answers a card that is out of sight");
    i.apply(Event::AskDigit(1), 3.1);
    assert!(take_notices(&mut i).is_empty(), "Option plus a digit does not answer it unseen");
    assert_eq!(i.card_ids(), vec!["tuck".to_string()], "the question is still held");
    assert!(i.targets().iter().any(|(t, _)| *t == Target::Token(super::Token::Reply)), "the notch at rest brings it back");
    i.apply(Event::Hover(Some(Target::Token(super::Token::Reply))), 3.2);
    settle(&mut i, 4.0);
    assert!(matches!(i.scene(), Scene::Lip(super::Lip::Peek)), "the pointer at the notch shows the peek lip");
    i.apply(Event::Click(Target::Token(super::Token::Reply)), 4.1);
    settle(&mut i, 5.5);
    assert_eq!(i.class(), Class::Question, "the notch brings the card back");
    match i.scene() {
        Scene::Question { id, card, .. } => {
            assert_eq!(id, "tuck");
            assert_eq!(card.picks[0].labels, vec!["No".to_string()], "his pick is kept");
        }
        other => panic!("not the card: {other:?}"),
    }
    i.apply(Event::Click(Target::AskPick { id: "tuck".into(), question: 1, option: 0 }), 5.6);
    i.apply(Event::Click(Target::AskSend("tuck".into())), 5.7);
    let sent = take_notices(&mut i);
    assert!(sent.iter().any(|a| matches!(a, AgentNoticeAction::Answer { request_id, .. } if request_id == "tuck")), "still answerable: {sent:?}");
}

/// A tucked question is still the user's to answer: his pulled-down conversation (⌃⌘H) docks it at its bottom.
#[test]
fn a_tucked_card_docks_in_owners_conversation() {
    let mut i = island();
    i.apply(card("dock", vec![question("a", None, false, &["Yes", "No"])], 600_000), 0.0);
    settle(&mut i, 1.0);
    i.apply(Event::Click(Target::AskTuck("dock".into())), 1.0);
    settle(&mut i, 2.0);
    i.apply(Event::OpenChat, 2.1);
    settle(&mut i, 3.5);
    assert!(matches!(i.scene(), Scene::Reply { ask: Some(a), .. } if a.id == "dock"), "docked in the conversation: {:?}", i.scene());
}

/// Esc while the card's field has focus dismisses it; a dismissed card answers nothing else.
#[test]
fn esc_dismisses_the_card_once() {
    let mut i = island();
    i.apply(card("c13", vec![question("q1", None, false, &["Yes", "No"])], 600_000), 0.0);
    settle(&mut i, 1.0);
    i.apply(Event::AskDismiss("c13".into()), 1.0);
    i.apply(Event::AskDismiss("c13".into()), 1.1);
    assert_eq!(take_notices(&mut i), vec![AgentNoticeAction::Dismiss { request_id: "c13".into() }]);
}

/// Review of the lip (bug note 2026-10-28T19-45-55): a question that arrives while another is tucked away shows at
/// once (it never waits unseen behind the tucked one); the tucked one stays behind the notch, and comes back once
/// the new one is answered.
#[test]
fn a_new_question_shows_while_another_is_tucked_and_the_tucked_one_stays_reachable() {
    let mut i = island();
    i.apply(card("a", vec![question("a1", None, false, &["Yes", "No"])], 600_000), 0.0);
    settle(&mut i, 1.0);
    i.apply(Event::Click(Target::AskTuck("a".into())), 1.0);
    settle(&mut i, 2.0);
    assert_eq!(i.class(), Class::Rest);
    i.apply(card("b", vec![question("b1", None, false, &["Now", "Later"])], 600_000), 2.1);
    settle(&mut i, 3.5);
    assert!(matches!(i.scene(), Scene::Question { id, .. } if id == "b"), "the new question shows: {:?}", i.scene());
    assert_eq!(i.card_keys(), 2, "its digits answer it");
    i.apply(Event::AskDigit(1), 3.6);
    let sent = take_notices(&mut i);
    assert!(sent.iter().any(|a| matches!(a, AgentNoticeAction::Answer { request_id, .. } if request_id == "b")), "the digit answers the card on screen: {sent:?}");
    i.apply(Event::CloseQuestion { id: "b".into(), answer: "Now".into() }, 3.7);
    settle(&mut i, 5.5);
    assert_eq!(i.class(), Class::Rest, "the tucked one stays tucked");
    assert!(i.targets().iter().any(|(t, _)| *t == Target::Token(super::Token::Reply)), "the notch holds it");
    i.apply(Event::Click(Target::Token(super::Token::Reply)), 5.6);
    settle(&mut i, 7.0);
    assert!(matches!(i.scene(), Scene::Question { id, .. } if id == "a"), "the notch brings the tucked one back: {:?}", i.scene());
}

/// Review of the lip: on a display without a notch nothing could bring a tucked card back, so the card has no lip
/// and a tuck does nothing; moving to such a display brings a tucked card back.
#[test]
fn without_a_notch_the_card_has_no_lip_and_never_tucks() {
    let mut i = Island::new(Display::PLAIN, Box::new(Approx));
    i.apply(card("p", vec![question("p1", None, false, &["Yes", "No"])], 600_000), 0.0);
    settle(&mut i, 1.0);
    assert_eq!(i.class(), Class::Question);
    assert!(!i.layout().items.iter().any(|it| matches!(&*it.content, super::Content::Grabber { .. })), "no lip without a notch");
    assert!(!i.targets().iter().any(|(t, _)| matches!(t, Target::AskTuck(_))), "no tuck target");
    i.apply(Event::Click(Target::AskTuck("p".into())), 1.1);
    settle(&mut i, 2.5);
    assert_eq!(i.class(), Class::Question, "a tuck does nothing there");
    let mut j = island();
    j.apply(card("m", vec![question("m1", None, false, &["Yes", "No"])], 600_000), 0.0);
    settle(&mut j, 1.0);
    j.apply(Event::Click(Target::AskTuck("m".into())), 1.0);
    settle(&mut j, 2.0);
    assert_eq!(j.class(), Class::Rest);
    j.apply(Event::Display(Display::PLAIN), 2.1);
    settle(&mut j, 3.5);
    assert_eq!(j.class(), Class::Question, "on a display without a notch the tucked card shows again");
}

/// Review of the lip: a tucked question that is answered, closed or expires is no longer tucked; asked again under
/// the same id, it shows.
#[test]
fn a_tucked_question_that_closes_is_no_longer_tucked() {
    let mut i = island();
    i.apply(card("r", vec![question("r1", None, false, &["Yes", "No"])], 600_000), 0.0);
    settle(&mut i, 1.0);
    i.apply(Event::Click(Target::AskTuck("r".into())), 1.0);
    settle(&mut i, 2.0);
    i.apply(Event::CloseQuestion { id: "r".into(), answer: "Yes".into() }, 2.1);
    settle(&mut i, 4.0);
    assert!(!i.targets().iter().any(|(t, _)| *t == Target::Token(super::Token::Reply)), "nothing left behind the notch");
    i.apply(card("r", vec![question("r1", None, false, &["Yes", "No"])], 600_000), 4.1);
    settle(&mut i, 5.5);
    assert_eq!(i.class(), Class::Question, "asked again, it shows");
}

/// The voice card: the quote, the guessed names as one-click chips in one row, the name field with
/// its own placeholder, and Not important last. A chip answers at once; Return sends exactly what he typed (trimmed).
/// The field asks for the keyboard only when the card says so (`focus_until_ms`).
#[test]
fn a_voice_card_is_chips_then_the_name_field_then_not_important() {
    use crate::agent_notice::OtherField;
    let field = |focus: bool| Some(OtherField { placeholder: "Type the name".into(), focus_until_ms: if focus { u64::MAX } else { 0 }, after: 1 });
    let named = |id: &str, labels: &[&str], focus| AskQuestion { field: field(focus), ..question(id, None, false, labels) };
    let mut i = island();
    i.apply(card("v", vec![named("q1", &["Mari", "Ellie", "Not important"], false)], 600_000), 0.1);
    settle(&mut i, 0.8);
    let l = i.layout();
    let rect = |id: &str| l.items.iter().find(|it| &*it.id == id).map(|it| it.rect).unwrap_or_else(|| panic!("{id}"));
    assert_eq!(rect("q.o0.row").y, rect("q.o1.row").y, "the names are chips in one row");
    assert!(rect("q.o1.row").x > rect("q.o0.row").max_x(), "side by side");
    assert!(rect("q.other.row").y > rect("q.o0.row").max_y(), "the field after the names");
    assert!(rect("q.o2.label").y > rect("q.other.row").max_y(), "Not important last");
    assert!(!l.items.iter().any(|it| it.id.contains(".tile") && !it.id.contains("other")), "no numbered rows");
    let text = |id: &str| l.items.iter().find(|it| &*it.id == id).and_then(|it| match &*it.content { super::Content::Text { lines, .. } => lines.first().cloned(), _ => None });
    assert_eq!(text("q.other.placeholder").as_deref(), Some("Type the name"));
    assert_eq!(text("q.o2.label").as_deref(), Some("Not important"));
    assert!(l.items.iter().any(|it| matches!(&*it.content, super::Content::Field { focus_until_ms: 0, .. })), "a card that came on its own never takes the keyboard");
    assert_eq!(i.card_keys(), 0, "no digits drawn, none taken: a stray Option+3 in another app never forgets a voice");
    let half = super::layout::ASK_W / 2.0;
    assert!(l.items.iter().filter(|it| it.id.starts_with("q.")).all(|it| it.rect.x >= -half && it.rect.max_x() <= half), "inside the card");
    // Every answer is one click: a chip, and Not important.
    assert!(l.targets.iter().any(|(t, _)| *t == Target::AskPick { id: "v".into(), question: 0, option: 2 }));
    i.apply(Event::Click(Target::AskPick { id: "v".into(), question: 0, option: 1 }), 0.9);
    let sent = take_notices(&mut i);
    assert!(sent.iter().any(|a| matches!(a, AgentNoticeAction::Answer { request_id, answers, .. } if request_id == "v" && answers[0] == answer("q1", &["Ellie"], None))), "{sent:?}");

    // His own spelling: Return sends it.
    let mut i = island();
    i.apply(card("w", vec![named("q1", &["Mari", "Not important"], true)], 600_000), 0.1);
    settle(&mut i, 0.8);
    assert!(i.layout().items.iter().any(|it| matches!(&*it.content, super::Content::Field { focus_until_ms: u64::MAX, .. })), "asked to, it takes the keyboard");
    i.apply(Event::AskOther { id: "w".into(), question: 0, text: " Marii ".into() }, 0.9);
    i.apply(Event::AskReturn("w".into()), 1.0);
    let sent = take_notices(&mut i);
    assert!(sent.iter().any(|a| matches!(a, AgentNoticeAction::Answer { request_id, answers, .. } if request_id == "w" && answers[0] == answer("q1", &[], Some("Marii")))), "{sent:?}");

    // No real guess: no chip, the field, then Not important.
    let mut i = island();
    i.apply(card("x", vec![named("q1", &["Not important"], false)], 600_000), 0.1);
    settle(&mut i, 0.8);
    let l = i.layout();
    let rect = |id: &str| l.items.iter().find(|it| &*it.id == id).map(|it| it.rect).unwrap_or_else(|| panic!("{id}"));
    let field_y = rect("q.other.row").y;
    assert!(!l.items.iter().any(|it| it.id.starts_with("q.o") && !it.id.starts_with("q.other") && it.rect.y < field_y), "no chips above the field");
    assert!(rect("q.o0.row").y > rect("q.other.row").max_y(), "Not important under it");
    i.apply(Event::AskOther { id: "x".into(), question: 0, text: "dave".into() }, 0.9);
    i.apply(Event::AskReturn("x".into()), 1.0);
    assert!(take_notices(&mut i).iter().any(|a| matches!(a, AgentNoticeAction::Answer { answers, .. } if answers[0].other.as_deref() == Some("dave"))));
}

/// Approval grants (4 Nov evening): only the user's own pointer counts. A pick the host marks as a hardware click
/// (`Event::HardwareClick`, from the HID system's mouse-down and mouse-up) goes as `by_hand`; a plain click (an
/// in-process or posted event), Option plus a digit, a voice pick or Return never does.
#[test]
fn only_a_hardware_click_answers_by_hand() {
    let hand = |id: &str, labels: &[&str]| vec![answer(id, labels, None)];
    let pick = |id: &str, q: usize, o: usize| Target::AskPick { id: id.into(), question: q, option: o };
    // One question: the click that answers is the one that counts.
    let mut i = island();
    i.apply(card("g1", vec![question("q1", None, false, &["Delete nightly", "Keep them"])], 600_000), 0.0);
    settle(&mut i, 1.0);
    i.apply(Event::HardwareClick(pick("g1", 0, 0)), 1.0);
    assert_eq!(take_notices(&mut i), vec![AgentNoticeAction::Answer { request_id: "g1".into(), answers: hand("q1", &["Delete nightly"]), by_hand: hand("q1", &["Delete nightly"]) }]);
    for (n, ev) in [Event::Click(pick("g2", 0, 0)), Event::AskPick { id: "g2".into(), question: 0, option: 0 }, Event::AskDigit(1)].into_iter().enumerate() {
        let mut i = island();
        i.apply(card("g2", vec![question("q1", None, false, &["Delete nightly", "Keep them"])], 600_000), 0.0);
        settle(&mut i, 1.0);
        i.apply(ev, 1.0);
        let sent = take_notices(&mut i);
        assert!(matches!(&sent[..], [AgentNoticeAction::Answer { answers, by_hand, .. }] if answers == &hand("q1", &["Delete nightly"]) && by_hand.is_empty()), "case {n}: {sent:?}");
    }

    // Two questions: picks by hand, then Send by hand.
    let two = || card("g3", vec![question("q1", Some("Delete"), true, &["A", "B", "C"]), question("q2", Some("Login"), false, &["Remove", "Keep"])], 600_000);
    let mut i = island();
    i.apply(two(), 0.0);
    settle(&mut i, 1.0);
    i.apply(Event::HardwareClick(pick("g3", 0, 0)), 1.0);
    i.apply(Event::Click(pick("g3", 0, 1)), 1.1);
    i.apply(Event::HardwareClick(pick("g3", 0, 2)), 1.2);
    i.apply(Event::HardwareClick(pick("g3", 0, 2)), 1.3); // untoggled
    i.apply(Event::HardwareClick(pick("g3", 1, 0)), 1.4);
    take_notices(&mut i);
    i.apply(Event::HardwareClick(Target::AskSend("g3".into())), 1.5);
    let sent = take_notices(&mut i);
    assert!(matches!(&sent[..], [AgentNoticeAction::Answer { answers, by_hand, .. }]
        if answers == &vec![answer("q1", &["A", "B"], None), answer("q2", &["Remove"], None)]
        && by_hand == &vec![answer("q1", &["A"], None), answer("q2", &["Remove"], None)]), "{sent:?}");

    // The same picks sent by a plain click on Send, or by Return: nothing by hand.
    for send in [Event::Click(Target::AskSend("g3".into())), Event::AskSend("g3".into()), Event::AskReturn("g3".into())] {
        let mut i = island();
        i.apply(two(), 0.0);
        settle(&mut i, 1.0);
        i.apply(Event::HardwareClick(pick("g3", 0, 0)), 1.0);
        i.apply(Event::HardwareClick(pick("g3", 1, 0)), 1.1);
        take_notices(&mut i);
        i.apply(send, 1.2);
        let sent = take_notices(&mut i);
        assert!(matches!(&sent[..], [AgentNoticeAction::Answer { by_hand, .. }] if by_hand.is_empty()), "{sent:?}");
    }

    // His voice re-sets a question, or typed Other replaces a single pick: those picks are no longer clicks.
    let mut i = island();
    i.apply(two(), 0.0);
    settle(&mut i, 1.0);
    i.apply(Event::HardwareClick(pick("g3", 0, 0)), 1.0);
    i.apply(Event::HardwareClick(pick("g3", 1, 0)), 1.1);
    i.apply(Event::AskSet { id: "g3".into(), answer: answer("q1", &["A"], None) }, 1.2);
    i.apply(Event::AskOther { id: "g3".into(), question: 1, text: "only the helper".into() }, 1.3);
    take_notices(&mut i);
    i.apply(Event::HardwareClick(Target::AskSend("g3".into())), 1.4);
    let sent = take_notices(&mut i);
    assert!(matches!(&sent[..], [AgentNoticeAction::Answer { answers, by_hand, .. }] if answers[0].labels == ["A"] && by_hand.is_empty()), "{sent:?}");
}

/// Review 4 Nov evening: an option with a grant takes only the user's own pointer. A key, a plain click (made in this
/// process or posted by another), Return or his voice picks nothing there, so the card never answers yes to a grant
/// that was not clicked; and a grant drawn past the window's bottom takes no click at all.
#[test]
fn a_granting_option_takes_only_a_hardware_click() {
    use crate::agent_notice::GrantItem;
    let granting = |id: &str, grant: Vec<GrantItem>| {
        let mut q = question(id, Some("Delete"), false, &["Delete them", "Keep them"]);
        q.options[0].grant = grant;
        q
    };
    let one = || granting("q1", vec![GrantItem::Delete("~/Backups/T3 Code (Nightly).app".into())]);
    let pick = |q: usize, o: usize| Target::AskPick { id: "g".into(), question: q, option: o };
    for ev in [Event::Click(pick(0, 0)), Event::AskPick { id: "g".into(), question: 0, option: 0 }, Event::AskDigit(1)] {
        let mut i = island();
        i.apply(card("g", vec![one()], 600_000), 0.0);
        settle(&mut i, 1.0);
        i.apply(ev, 1.0);
        assert!(take_notices(&mut i).is_empty(), "not by hand: no answer");
        i.apply(Event::Click(pick(0, 1)), 1.1);
        assert!(matches!(&take_notices(&mut i)[..], [AgentNoticeAction::Answer { answers, .. }] if answers[0].labels == ["Keep them"]), "the plain option still answers");
    }
    let mut i = island();
    i.apply(card("g", vec![one()], 600_000), 0.0);
    settle(&mut i, 1.0);
    i.apply(Event::HardwareClick(pick(0, 0)), 1.0);
    assert!(matches!(&take_notices(&mut i)[..], [AgentNoticeAction::Answer { by_hand, .. }] if by_hand[0].labels == ["Delete them"]));

    // Two questions: a hand-picked grant goes only by his own click on Send; his voice cannot pick it.
    let two = || card("g", vec![one(), question("q2", Some("Push"), false, &["Yes", "No"])], 600_000);
    for send in [Event::AskReturn("g".into()), Event::AskSend("g".into()), Event::Click(Target::AskSend("g".into()))] {
        let mut i = island();
        i.apply(two(), 0.0);
        settle(&mut i, 1.0);
        i.apply(Event::HardwareClick(pick(0, 0)), 1.0);
        i.apply(Event::AskPick { id: "g".into(), question: 1, option: 0 }, 1.1);
        take_notices(&mut i);
        i.apply(send, 1.2);
        assert!(take_notices(&mut i).iter().all(|a| !matches!(a, AgentNoticeAction::Answer { .. })), "not sent without his click");
        i.apply(Event::HardwareClick(Target::AskSend("g".into())), 1.3);
        assert!(matches!(&take_notices(&mut i)[..], [AgentNoticeAction::Answer { by_hand, .. }] if by_hand == &vec![answer("q1", &["Delete them"], None)]), "only the hand pick goes by hand");
    }
    let mut i = island();
    i.apply(two(), 0.0);
    settle(&mut i, 1.0);
    i.apply(Event::AskSet { id: "g".into(), answer: answer("q1", &["Delete them"], None) }, 1.0);
    i.apply(Event::AskPick { id: "g".into(), question: 1, option: 0 }, 1.1);
    take_notices(&mut i);
    i.apply(Event::AskSend("g".into()), 1.2);
    assert!(matches!(&take_notices(&mut i)[..], [AgentNoticeAction::Answer { answers, .. }] if answers[0].labels.is_empty() && answers[1].labels == ["Yes"]), "his voice never picks a grant");

    // A grant taller than the window (bypassing the agent's own limit): drawn, but no click target.
    let tall = (0..24).map(|k| GrantItem::Delete(format!("~/Backups/{}-{k}", "a".repeat(70)))).collect();
    let mut i = island();
    i.apply(card("g", vec![granting("q1", tall)], 600_000), 0.0);
    settle(&mut i, 2.0);
    let targets = i.targets();
    assert!(!targets.iter().any(|(t, _)| *t == pick(0, 0)), "a grant cut off takes no click");
    let mut i = island();
    i.apply(card("g", vec![one()], 600_000), 0.0);
    settle(&mut i, 2.0);
    assert!(i.targets().iter().any(|(t, _)| *t == pick(0, 0)), "a grant that fits takes his click");
}
