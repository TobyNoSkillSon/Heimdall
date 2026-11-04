//! Opening the conversation: ⌃⌘H (`Event::OpenChat`) and a click on the notch while Heimdall thinks, sends or speaks.

use super::{Approx, Event, Island, Target, Token};
use crate::island::chat::{ChatEntry, ChatKind, Heard};
use crate::island::object::{Class, Content, Display};
use crate::island::Activity;

fn island() -> Island {
    Island::new(Display::MBP16, Box::new(Approx))
}

fn history() -> Event {
    Event::Chat(vec![
        ChatEntry { at_ms: 1, kind: ChatKind::Owner("what's on my calendar".into()) },
        ChatEntry { at_ms: 2, kind: ChatKind::Heimdall { text: "Two meetings, the first at ten.".into(), heard: Heard::Whole } },
    ])
}

fn composer(i: &Island, t: f64) -> bool {
    i.frame(t).slots.iter().any(|s| matches!(&*s.content, Content::Field { owner: Target::Composer, .. }))
}

/// Bug note 2026-10-25T21-28-46: while Heimdall thinks, a click on the island opens the conversation.
#[test]
fn the_thinking_lip_opens_the_conversation_on_a_click() {
    let mut i = island();
    i.apply(history(), 0.0);
    i.apply(Event::Sent, 0.1);
    i.apply(Event::Activity(Some(Activity::Thinking)), 0.5);
    i.apply(Event::Tick, 2.0);
    assert_eq!(i.class(), Class::Lip);
    let target = i.targets().iter().find(|(t, _)| *t == Target::Token(Token::Reply)).cloned().expect("the thinking lip is a target");
    assert!(target.1.w >= 100.0, "the whole lip takes the click: {:?}", target.1);
    i.apply(Event::Click(target.0), 2.1);
    // Its motion waits out the lip's last one (at most three a second), then grows.
    for t in [2.5, 3.0, 3.5, 4.0] {
        i.apply(Event::Tick, t);
    }
    assert_eq!(i.class(), Class::Reply);
    assert!(composer(&i, 4.0));
}

/// The working words take the click too; with nothing to show the click opens nothing. The voice's click puts its
/// words away instead (bug note 2026-10-31T23-37-26, `stuck_reply_tests`).
#[test]
fn speaking_and_working_take_the_click_and_nothing_to_show_opens_nothing() {
    let mut i = island();
    i.apply(Event::Sent, 0.0);
    i.apply(Event::Activity(Some(Activity::Thinking)), 0.2);
    i.apply(Event::Tick, 2.0);
    assert!(i.targets().iter().any(|(t, _)| *t == Target::Token(Token::Reply)));
    i.apply(Event::Click(Target::Token(Token::Reply)), 2.1);
    i.apply(Event::Tick, 3.0);
    assert_eq!(i.class(), Class::Lip, "no conversation and no reply yet: the lip stays");
    i.apply(Event::Speaking(true), 3.5);
    i.apply(Event::SpokenText(Some("Two meetings.".into())), 3.5);
    i.apply(Event::Tick, 5.0);
    assert!(i.targets().iter().any(|(t, _)| *t == Target::Hush), "the voice is a target: {:?}", i.targets());
}

/// ⌃⌘H: the conversation pulled down as the user's with its composer, even when there is nothing yet; a second press is
/// the same open conversation.
#[test]
fn open_chat_pulls_the_conversation_down_with_its_composer_even_when_empty() {
    let mut i = island();
    assert_eq!(i.class(), Class::Rest);
    i.apply(Event::OpenChat, 0.0);
    i.apply(Event::Tick, 1.0);
    assert_eq!(i.class(), Class::Reply);
    assert!(composer(&i, 1.0), "the composer is there to type in");
    // Nothing but the composer: a real sheet, the composer whole inside it.
    let f = i.frame(1.0);
    let field = f.slots.iter().find(|s| matches!(&*s.content, Content::Field { owner: Target::Composer, .. })).map(|s| s.rect).unwrap();
    assert!(f.shape.w >= 300.0 && f.shape.h >= field.y + field.h + 8.0, "{:?} holds {:?}", f.shape, field);
    assert!(field.w >= 200.0, "{field:?}");
    i.apply(Event::OpenChat, 1.1);
    i.apply(Event::Tick, 2.0);
    assert_eq!(i.class(), Class::Reply);
    // It stays as the user's (no unattended pull-up), and its band pulls it up.
    i.apply(Event::Tick, 30.0);
    assert_eq!(i.class(), Class::Reply);
    i.apply(Event::Click(Target::ReplyBand), 30.1);
    i.apply(Event::Tick, 31.0);
    assert_eq!(i.class(), Class::Rest);
    // With a conversation, it opens on it while Heimdall thinks.
    i.apply(history(), 32.0);
    i.apply(Event::Activity(Some(Activity::Thinking)), 32.1);
    i.apply(Event::OpenChat, 33.0);
    i.apply(Event::Tick, 34.0);
    assert_eq!(i.class(), Class::Reply);
}

/// Bug note 2026-10-25T23-12-07: the composer's placeholder has one source, the island's drawn text (from
/// `composer_placeholder`); it goes with the first typed character and comes back when the composer is empty again.
#[test]
fn the_composer_placeholder_is_drawn_once_and_follows_the_text() {
    let mut i = island();
    i.apply(Event::OpenChat, 0.0);
    i.apply(Event::Tick, 1.0);
    let drawn = |i: &Island, t: f64| {
        i.frame(t).slots.iter().filter(|s| matches!(&*s.content, Content::Text { lines, .. } if lines.iter().any(|l| *l == super::composer_placeholder()))).count()
    };
    assert_eq!(drawn(&i, 1.0), 1, "one drawn placeholder while empty");
    i.apply(Event::ComposerEmpty(false), 1.5);
    i.apply(Event::Tick, 2.5);
    assert_eq!(drawn(&i, 2.5), 0, "typed: no placeholder");
    assert!(composer(&i, 2.5), "the field stays");
    i.apply(Event::ComposerEmpty(true), 3.0);
    i.apply(Event::Tick, 4.0);
    assert_eq!(drawn(&i, 4.0), 1, "empty again: the placeholder is back");
}

/// Audit 27 Oct: an apply that takes long (a long day laid out cold) ate the opening spring: the first frame painted
/// after it showed the conversation already open. The host delays the motion by the apply's time
/// (`Island::delay_motion`): the frame painted then is the motion's first, and the whole opening plays after it.
#[test]
fn a_late_first_frame_starts_the_opening_motion_instead_of_skipping_it() {
    let (mut on_time, mut late) = (island(), island());
    for i in [&mut on_time, &mut late] {
        i.apply(history(), 0.0);
        i.apply(Event::Tick, 1.0);
        i.apply(Event::OpenChat, 2.0);
    }
    let (id, start) = late.motion();
    assert_eq!(start, 2.0);
    let delay = 0.66;
    late.delay_motion(id, delay);
    assert_eq!(late.motion(), (id, 2.0 + delay));
    let rest = on_time.frame(2.0).shape;
    let first = late.frame(2.0 + delay).shape;
    assert!((first.h - rest.h).abs() < 0.5 && (first.w - rest.w).abs() < 0.5, "the first frame painted is where the motion starts: {first:?} vs {rest:?}");
    for dt in [0.016, 0.05, 0.1, 0.2, 0.4, 1.0] {
        let (a, b) = (on_time.frame(2.0 + dt), late.frame(2.0 + delay + dt));
        assert!((a.shape.h - b.shape.h).abs() < 1e-6 && (a.shape.w - b.shape.w).abs() < 1e-6, "the same motion, later: {dt}");
        assert_eq!(a.slots.len(), b.slots.len(), "{dt}");
    }
    let open = on_time.frame(5.0).shape;
    assert!(open.h > rest.h + 100.0, "it opens: {open:?}");
    // Only the motion this apply started: an old id changes nothing.
    late.delay_motion(id - 1, 1.0);
    assert_eq!(late.motion(), (id, 2.0 + delay));
}

/// Bug note 2026-10-26T19-14-21-2: while Heimdall speaks, ⌃⌘H opens the conversation and its band closes it, and the
/// voice goes on.
#[test]
fn the_conversation_opens_and_closes_while_heimdall_speaks() {
    let mut i = island();
    i.apply(history(), 0.0);
    i.apply(Event::Speaking(true), 1.0);
    i.apply(Event::SpokenText(Some("Two meetings today, the first at ten and the second after lunch.".into())), 1.0);
    i.apply(Event::Tick, 2.0);
    assert!(i.speaking(2.0));
    i.apply(Event::OpenChat, 2.0);
    for t in [2.4, 2.8, 3.2] {
        i.apply(Event::SpeechLevel(0.5), t);
        i.apply(Event::Tick, t);
    }
    assert_eq!(i.class(), Class::Reply, "opened while speaking");
    assert!(composer(&i, 3.2));
    i.apply(Event::Click(Target::ReplyBand), 3.3);
    for t in [3.7, 4.1, 4.5] {
        i.apply(Event::SpeechLevel(0.4), t);
        i.apply(Event::Tick, t);
    }
    assert!(!i.reply_open() && i.class() != Class::Reply, "closed while speaking");
    assert!(i.speaking(4.5), "the voice goes on");
}

/// Bug note 2026-10-26T19-14-48: "when I finish talking and the window is open it will actually close itself". A
/// whole voice turn with the conversation the user clicked open: he talks (the lip held), his words are sent, the agent
/// thinks and works, a reply that would have played over him is withdrawn, the next one is spoken and written, the
/// conversation grows, the voice and the turn end. It stays open through all of it; closed by its band while the
/// agent thinks, a click on the lip opens it again.
#[test]
fn a_voice_turn_never_closes_the_conversation() {
    let mut i = island();
    i.apply(history(), 0.0);
    i.apply(Event::LastReply("Two meetings, the first at ten.".into()), 0.1);
    i.apply(Event::Click(Target::Token(Token::Reply)), 0.5);
    let open = |i: &mut Island, t: f64, what: &str| {
        i.apply(Event::Tick, t);
        assert!(i.reply_open() && i.class() == Class::Reply, "open {what} (t {t})");
    };
    open(&mut i, 1.5, "before the turn");
    // He talks: the turn's lip waits while he does, then for the ack's moment.
    i.apply(Event::Hold(f64::INFINITY), 2.0);
    open(&mut i, 3.0, "while he talks");
    i.apply(Event::Hold(0.4), 4.0);
    i.apply(Event::Sent, 4.0);
    open(&mut i, 4.6, "after his words are sent");
    i.apply(Event::Activity(Some(Activity::Thinking)), 5.0);
    i.apply(Event::Notice { notice: crate::agent_notice::AgentNotice::line("w1", "Checking the calendar", crate::agent_notice::NoticeKind::Working), body: None }, 5.5);
    open(&mut i, 6.5, "while the agent thinks and works");
    // A reply skipped because he was talking (Hold: "skipped unheard and unshown") leaves the pull-down.
    i.apply(Event::LastReply("Never heard.".into()), 7.0);
    i.apply(Event::WithdrawReply("Never heard.".into()), 7.1);
    open(&mut i, 7.5, "after a withdrawn reply");
    // The reply: written, then spoken; the conversation grows.
    i.apply(Event::LastReply("Three meetings now: ten, two and four.".into()), 8.0);
    i.apply(Event::Speaking(true), 8.2);
    i.apply(Event::SpokenText(Some("Three meetings now: ten, two and four.".into())), 8.2);
    for k in 0..30 {
        i.apply(Event::SpeechLevel(0.5), 8.3 + k as f64 * 0.033);
    }
    open(&mut i, 9.5, "while Heimdall speaks");
    i.apply(Event::Chat(vec![
        ChatEntry { at_ms: 1, kind: ChatKind::Owner("what's on my calendar".into()) },
        ChatEntry { at_ms: 2, kind: ChatKind::Heimdall { text: "Two meetings, the first at ten.".into(), heard: Heard::Whole } },
        ChatEntry { at_ms: 3, kind: ChatKind::Owner("and tomorrow".into()) },
    ]), 9.6);
    i.apply(Event::Speaking(false), 10.0);
    i.apply(Event::SpokenText(None), 10.0);
    i.apply(Event::Activity(None), 10.2);
    open(&mut i, 12.0, "after the voice and the turn end");
    open(&mut i, 40.0, "long after (no timer closes it)");
    // Closed by its band while the agent thinks, the lip opens it again.
    i.apply(Event::Sent, 41.0);
    i.apply(Event::Activity(Some(Activity::Thinking)), 41.2);
    i.apply(Event::Click(Target::ReplyBand), 41.5);
    for t in [42.0, 42.5, 43.0] {
        i.apply(Event::Tick, t);
    }
    assert!(!i.reply_open());
    assert!(i.targets().iter().any(|(t, _)| *t == Target::Token(Token::Reply)), "the thinking lip is a target");
    i.apply(Event::Click(Target::Token(Token::Reply)), 43.1);
    for t in [43.5, 44.0, 44.5] {
        i.apply(Event::Tick, t);
    }
    assert!(i.reply_open() && i.class() == Class::Reply, "reopened while the agent thinks");
}
