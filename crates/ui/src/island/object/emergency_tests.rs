//! Emergency mode at the notch (bug note 27 Oct 19:40: "The emergency triangle next to the notch never showed, even
//! though emergency was on from 19:40:05"). The emergency `Notice` is a moment that reads and leaves; the token that
//! stays is `Event::Emergency`: the agent's glyph and the warning triangle, still, until it ends.

use super::*;
use crate::island::{HealthCue, Part};

fn island() -> Island {
    Island::new(Display::MBP16, Box::new(Approx))
}

fn run_to(i: &mut Island, until: f64) {
    while let Some(d) = i.deadline().filter(|d| *d <= until) {
        i.apply(Event::Tick, d);
    }
    i.apply(Event::Tick, until);
}

fn triangle(i: &Island) -> bool {
    matches!(i.scene(), Scene::Ears { right: Ear::Glyph(g), .. } if g.name == EMERGENCY_SYMBOL)
}

const NOTICE: &str = "Agent out of quota till 03:04 · backup agent on it, nothing to do";

/// The cause of the bug: the notice alone leaves nothing beside the notch once it has been read.
#[test]
fn the_emergency_notice_alone_leaves_nothing_at_rest() {
    let mut i = island();
    i.apply(Event::Health(HealthCue::Notice { part: Part::Agent, label: NOTICE.into() }), 1.0);
    run_to(&mut i, 30.0);
    assert!(!triangle(&i) && i.class() == Class::Rest, "{:?}", i.scene());
}

/// Emergency on: the notice reads, then the triangle rests (drawn, still, no frames); off clears it.
#[test]
fn emergency_mode_rests_a_still_triangle_beside_the_notch_until_it_ends() {
    let mut i = island();
    i.apply(Event::Emergency(true), 1.0);
    i.apply(Event::Health(HealthCue::Notice { part: Part::Agent, label: NOTICE.into() }), 1.0);
    run_to(&mut i, 30.0);
    assert!(triangle(&i), "{:?}", i.scene());
    let f = i.frame(30.0);
    assert!(f.visible && !f.moving, "drawn and still");
    assert_eq!(i.needs_frames(30.0), FrameNeed::None, "nothing animates at rest");
    // A line in between takes the island; the token comes back after it.
    i.apply(Event::Notice { notice: crate::agent_notice::AgentNotice::line("l1", "Plan updated", crate::agent_notice::NoticeKind::Info), body: None }, 31.0);
    run_to(&mut i, 32.0);
    assert!(!triangle(&i));
    run_to(&mut i, 60.0);
    assert!(triangle(&i), "back after the line: {:?}", i.scene());
    i.apply(Event::Emergency(false), 61.0);
    run_to(&mut i, 70.0);
    assert!(!triangle(&i) && i.class() == Class::Rest, "{:?}", i.scene());
    assert_eq!(i.needs_frames(70.0), FrameNeed::None);
}

/// The notch still opens the last turn while the triangle rests.
#[test]
fn the_last_turn_still_opens_under_the_triangle() {
    let mut i = island();
    i.apply(Event::LastReply("Done.".into()), 1.0);
    i.apply(Event::Emergency(true), 1.0);
    run_to(&mut i, 20.0);
    assert!(matches!(i.scene(), Scene::Ears { target: Some(_), .. }), "{:?}", i.scene());
    i.apply(Event::Click(Target::Token(Token::Reply)), 20.0);
    run_to(&mut i, 21.0);
    assert!(i.reply_open(), "the pull-down opens");
}
