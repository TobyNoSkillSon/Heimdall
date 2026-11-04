//! Switches at the notch ("a little animation … when we stop listening and stop seeing, and when
//! we start"): the glyph shows in its ear and turns in place (the same slot, so the image view's magic replace draws
//! the slash), Listening and Seeing off rest as a dim token, on again returns to rest, Islands is a moment only.

use super::sim::{self, Config, Scenario, Step};
use super::*;

fn island() -> Island {
    Island::new(Display::MBP16, Box::new(Approx))
}

fn run_to(i: &mut Island, until: f64) {
    while let Some(d) = i.deadline().filter(|d| *d <= until) {
        i.apply(Event::Tick, d);
    }
    i.apply(Event::Tick, until);
}

fn ear<'a>(f: &'a FrameModel, id: &str) -> Option<(&'a str, f64)> {
    f.slots.iter().find(|s| &*s.id == id).and_then(|s| match &*s.content {
        Content::Symbol { name, .. } => Some((name.as_str(), s.opacity)),
        _ => None,
    })
}

fn switch(switch: Switch, on: bool) -> Event {
    Event::Switch { switch, on, moment: true }
}

#[test]
fn listening_off_draws_the_slash_in_place_then_rests_dim() {
    let mut i = island();
    i.apply(switch(Switch::Listening, false), 1.0);
    assert!(matches!(i.scene(), Scene::Ears { .. }));
    // Landed: the mic, whole and at full strength, in the left ear; Seeing (on) dim on the right.
    run_to(&mut i, 1.0 + scene::SWITCH_TURN - 0.05);
    let f = i.frame(1.0 + scene::SWITCH_TURN - 0.05);
    let (name, op) = ear(&f, "ear.l").expect("the left ear");
    assert_eq!(name, "mic");
    assert!(op > 0.9, "full strength: {op}");
    assert_eq!(ear(&f, "ear.r").map(|e| e.0), Some("eye"));
    // The turn: the same slot now shows the slashed mic (the painter's replace runs on the same image view).
    run_to(&mut i, 1.0 + scene::SWITCH_TURN + 0.05);
    let f = i.frame(1.0 + scene::SWITCH_TURN + 0.05);
    assert_eq!(ear(&f, "ear.l").map(|e| e.0), Some("mic.slash"));
    assert!(f.slots.iter().filter(|s| s.id.starts_with("ear.l")).count() == 1, "one left ear, no cross-fade twin: {:?}", f.slots.iter().map(|s| &s.id).collect::<Vec<_>>());
    // Afterwards: the token, dim, still, asking for no frames.
    let t = 1.0 + scene::SWITCH_SECONDS + 1.5;
    run_to(&mut i, t);
    let f = i.frame(t);
    assert!(f.visible && !f.moving);
    assert_eq!(i.needs_frames(t), FrameNeed::None);
    let (name, op) = ear(&f, "ear.l").expect("the token's left ear");
    assert_eq!(name, "mic.slash");
    assert!((op - scene::SWITCH_TOKEN_ALPHA).abs() < 0.05, "dim: {op}");
    let (name, op) = ear(&f, "ear.r").expect("the token's right ear");
    assert_eq!(name, "eye");
    assert!((op - scene::SWITCH_TOKEN_ALPHA).abs() < 0.05, "as dim, a pair: {op}");
    assert_eq!(i.deadline(), None, "nothing else is due");
}

#[test]
fn on_again_lifts_the_slash_and_returns_to_rest() {
    let mut i = island();
    i.apply(Event::Switch { switch: Switch::Seeing, on: false, moment: false }, 0.5);
    run_to(&mut i, 2.0);
    assert_eq!(ear(&i.frame(2.0), "ear.r").map(|e| e.0), Some("eye.slash"), "told without a moment: the token only");
    i.apply(switch(Switch::Seeing, true), 3.0);
    run_to(&mut i, 3.0 + scene::SWITCH_TURN - 0.05);
    let f = i.frame(3.0 + scene::SWITCH_TURN - 0.05);
    let (name, op) = ear(&f, "ear.r").expect("the right ear");
    assert_eq!(name, "eye.slash");
    assert!(op > 0.9, "brightened: {op}");
    run_to(&mut i, 3.0 + scene::SWITCH_TURN + 0.05);
    assert_eq!(ear(&i.frame(3.0 + scene::SWITCH_TURN + 0.05), "ear.r").map(|e| e.0), Some("eye"));
    let t = 3.0 + scene::SWITCH_SECONDS + 1.5;
    run_to(&mut i, t);
    let f = i.frame(t);
    assert!(!f.visible && !f.moving, "back to rest");
    assert_eq!(i.needs_frames(t), FrameNeed::None);
}

#[test]
fn islands_is_a_moment_with_a_check_and_no_token() {
    let mut i = island();
    i.apply(switch(Switch::Islands, false), 1.0);
    run_to(&mut i, 1.0 + scene::SWITCH_TURN + 0.1);
    let f = i.frame(1.0 + scene::SWITCH_TURN + 0.1);
    assert_eq!(ear(&f, "ear.l").map(|e| e.0), Some("rectangle.on.rectangle.slash"));
    assert_eq!(ear(&f, "ear.r").map(|e| e.0), Some("checkmark"));
    let t = 1.0 + scene::SWITCH_SECONDS + 1.5;
    run_to(&mut i, t);
    assert!(!i.frame(t).visible, "no token for the islands");
}

#[test]
fn the_token_yields_to_the_voice_and_opens_the_last_turn() {
    let mut i = island();
    i.apply(Event::Switch { switch: Switch::Listening, on: false, moment: false }, 0.5);
    i.apply(Event::LastReply("The build is green.".into()), 0.6);
    run_to(&mut i, 2.0);
    match i.scene() {
        Scene::Ears { target, .. } => assert_eq!(target, &Some(Target::Token(Token::Reply)), "a click on the token opens the last turn"),
        other => panic!("the token: {other:?}"),
    }
    i.apply(Event::Speaking(true), 2.5);
    i.apply(Event::SpokenText(Some("Hello.".into())), 2.5);
    assert!(matches!(i.scene(), Scene::Speaking { .. }), "the voice comes first: {:?}", i.scene());
}

#[test]
fn a_second_press_turns_back_from_where_the_glyph_is() {
    let mut i = island();
    i.apply(switch(Switch::Listening, false), 1.0);
    run_to(&mut i, 1.2);
    i.apply(switch(Switch::Listening, true), 1.2);
    run_to(&mut i, 1.25);
    assert_eq!(ear(&i.frame(1.25), "ear.l").map(|e| e.0), Some("mic.slash"), "the newest press starts from off");
    run_to(&mut i, 1.2 + scene::SWITCH_SECONDS + 1.0);
    assert!(!i.frame(1.2 + scene::SWITCH_SECONDS + 1.0).visible);
}

#[test]
fn switch_scenarios_hold_every_invariant() {
    let ev = |t: f64, e: Event| (t, Step::Ev(e));
    let steps = vec![
        ev(0.1, switch(Switch::Listening, false)),
        ev(0.5, switch(Switch::Seeing, false)),
        ev(1.0, Event::Sent),
        ev(1.4, switch(Switch::Islands, false)),
        ev(3.5, Event::DrawOpen),
        ev(3.8, switch(Switch::Listening, true)),
        ev(4.5, Event::DrawClose),
        ev(6.0, switch(Switch::Seeing, true)),
    ];
    for cfg in [Config::default(), Config { fps: 60.0, ..Config::default() }, Config { jitter: Some(5), ..Config::default() }] {
        let r = sim::run(&Scenario::new("switches", "switches mixed with a send and drawing mode", steps.clone()), &cfg);
        assert!(r.ok(), "{:#?}", r.findings);
    }
}

/// Simulator seed 1884 (a lone ear for 0.1 s): the token's ears were dimmed unequally, so the dimmer one dropped out
/// of the next motion while its partner carried on. Both ears now dim alike.
#[test]
fn the_token_after_a_line_swaps_as_a_pair() {
    let r = sim::run(&sim::random(1884), &Config::default());
    assert!(r.ok(), "{:#?}", &r.findings[..r.findings.len().min(3)]);
}

/// The random sequences reach switches, with and without a moment, so every invariant runs over them.
#[test]
fn the_random_sequences_turn_switches() {
    let (mut moment, mut silent) = (0, 0);
    for seed in 0..2000 {
        for (_, step) in sim::random(seed).steps {
            if let Step::Ev(Event::Switch { moment: m, .. }) = step {
                if m { moment += 1 } else { silent += 1 }
            }
        }
    }
    assert!(moment > 50 && silent > 10, "moments {moment}, silent {silent}");
}

/// Review 24 Oct: a command or health moment is not cut short by a switch (its time runs from its arrival); the
/// switch shows after it if its own time is left.
#[test]
fn a_command_moment_comes_before_a_switch() {
    use crate::island::{CommandCue, CommandKind};
    let mut i = island();
    i.apply(Event::Command(CommandCue::Heard { id: 1, kind: CommandKind::Screenshot }), 1.0);
    i.apply(switch(Switch::Listening, false), 1.05);
    match i.scene() {
        Scene::Ears { left: Ear::Glyph(g), .. } => assert_ne!(g.name, "mic", "the heard command keeps the island"),
        other => panic!("{other:?}"),
    }
}

/// Review 24 Oct: in drawing mode the switch shows in drawing mode's ears, and the keys stay.
#[test]
fn drawing_mode_shows_a_switch_in_its_ears() {
    let mut i = island();
    i.apply(Event::DrawOpen, 1.0);
    i.apply(switch(Switch::Seeing, false), 2.0);
    run_to(&mut i, 2.0 + scene::SWITCH_TURN + 0.05);
    match i.scene() {
        Scene::Drawing { ears: Some((l, r)), .. } => assert_eq!((l.name.as_str(), r.name.as_str()), ("mic", "eye.slash")),
        other => panic!("{other:?}"),
    }
    run_to(&mut i, 2.0 + scene::SWITCH_SECONDS + 0.5);
    assert!(matches!(i.scene(), Scene::Drawing { ears: None, .. }), "{:?}", i.scene());
}
