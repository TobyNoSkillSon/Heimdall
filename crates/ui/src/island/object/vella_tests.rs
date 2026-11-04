//! Dictating to Vella at the notch (docs/vella.md): Vella's mark over a few words as it starts, then the mark resting in
//! the left ear beside Vella's shortcut, drawn once (no frames, no timer); off, the token goes in one motion.

use super::layout::VELLA;
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

fn vella(on: bool, keys: Option<&str>) -> Event {
    Event::Vella { on, keys: keys.map(str::to_owned) }
}

#[test]
fn the_mark_and_its_words_then_the_mark_rests_in_an_ear_beside_the_keys() {
    let mut i = island();
    i.apply(vella(true, Some("⌃⌘N")), 1.0);
    match i.scene() {
        Scene::Stack { icon, words, button: None } => assert_eq!((icon.name.as_str(), words.as_str()), (VELLA, scene::VELLA_WORDS)),
        other => panic!("the moment: {other:?}"),
    }
    let f = i.frame(1.3);
    assert!(f.slots.iter().any(|s| matches!(&*s.content, Content::Vella { .. })), "Vella's own mark draws");
    let t = 1.0 + scene::VELLA_SECONDS + 1.5;
    run_to(&mut i, t);
    match i.scene() {
        Scene::Ears { left: Ear::Glyph(g), right: Ear::Num(keys), .. } => assert_eq!((g.name.as_str(), keys.as_str()), (VELLA, "⌃⌘N")),
        other => panic!("the token: {other:?}"),
    }
    let f = i.frame(t);
    assert!(f.visible && !f.moving);
    assert_eq!(i.needs_frames(t), FrameNeed::None, "nothing draws at rest");
    assert_eq!(i.deadline(), None, "no timer at rest");
    // A keybind change while on changes the token, without another moment.
    i.apply(vella(true, Some("⌃⌥Space")), t + 1.0);
    match i.scene() {
        Scene::Ears { right: Ear::Glyph(g), .. } => assert_eq!(g.name, "keyboard", "keys too long for an ear"),
        other => panic!("{other:?}"),
    }
    i.apply(vella(false, None), t + 2.0);
    run_to(&mut i, t + 4.0);
    assert!(!i.frame(t + 4.0).visible, "off: back into the notch");
}

#[test]
fn listening_off_shows_its_own_token_and_the_voice_comes_first() {
    let mut i = island();
    i.apply(vella(true, None), 1.0);
    i.apply(Event::Switch { switch: Switch::Listening, on: false, moment: false }, 1.0);
    run_to(&mut i, 4.0);
    match i.scene() {
        Scene::Ears { left: Ear::Glyph(g), .. } => assert_eq!(g.name, "mic.slash", "nothing is heard: the Listening token"),
        other => panic!("{other:?}"),
    }
    i.apply(Event::Switch { switch: Switch::Listening, on: true, moment: false }, 4.5);
    run_to(&mut i, 6.0);
    match i.scene() {
        Scene::Ears { left: Ear::Glyph(g), right: Ear::Glyph(r), .. } => assert_eq!((g.name.as_str(), r.name.as_str()), (VELLA, "keyboard")),
        other => panic!("{other:?}"),
    }
    i.apply(Event::Speaking(true), 6.5);
    i.apply(Event::SpokenText(Some("Hello.".into())), 6.5);
    assert!(matches!(i.scene(), Scene::Speaking { .. }), "{:?}", i.scene());
}

#[test]
fn vella_scenarios_hold_every_invariant() {
    let ev = |t: f64, e: Event| (t, Step::Ev(e));
    let steps = vec![
        ev(0.1, vella(true, Some("⌃⌘N"))),
        ev(0.6, Event::Sent),
        ev(2.5, Event::Switch { switch: Switch::Seeing, on: false, moment: true }),
        ev(5.0, vella(true, Some("⌃⌥Space"))),
        ev(6.0, Event::Switch { switch: Switch::Seeing, on: true, moment: true }),
        ev(8.0, vella(false, None)),
        ev(8.2, vella(true, Some("⌃⌘N"))),
        ev(8.3, vella(false, None)),
    ];
    for cfg in [Config::default(), Config { fps: 60.0, ..Config::default() }, Config { jitter: Some(5), ..Config::default() }] {
        let r = sim::run(&Scenario::new("vella", "dictation mixed with a send and switches", steps.clone()), &cfg);
        assert!(r.ok(), "{:#?}", r.findings);
    }
}

/// The random sequences reach Vella starting and stopping, so every invariant runs over them.
#[test]
fn the_random_sequences_dictate_to_vella() {
    let (mut on, mut off) = (0, 0);
    for seed in 0..2000 {
        for (_, step) in sim::random(seed).steps {
            if let Step::Ev(Event::Vella { on: o, .. }) = step {
                if o { on += 1 } else { off += 1 }
            }
        }
    }
    assert!(on > 30 && off > 10, "on {on}, off {off}");
}
