//! Catching up at the notch (31 Oct: the archive was locked for about four minutes and speech and listening stalled
//! with no visible sign). `HealthCue::Busy`: the stack and its words for `BUSY_READ`, then the stack and a dim
//! hourglass in the ears until it ends; resends change only the words; it yields to a restart and clears by itself.

use super::*;
use crate::island::{HealthCue, Part, BUSY_SYMBOL};

fn island() -> Island {
    Island::new(Display::MBP16, Box::new(Approx))
}

fn run_to(i: &mut Island, until: f64) {
    while let Some(d) = i.deadline().filter(|d| *d <= until) {
        i.apply(Event::Tick, d);
    }
    i.apply(Event::Tick, until);
}

fn busy(label: &str) -> Event {
    Event::Health(HealthCue::Busy { key: "archive".into(), part: Part::Agent, label: label.into() })
}

fn stack_words(i: &Island) -> Option<String> {
    match i.scene() {
        Scene::Stack { icon, words, button: None } if icon.name == BUSY_SYMBOL && icon.fx == Fx::Variable => Some(words.clone()),
        _ => None,
    }
}

fn busy_token(i: &Island) -> bool {
    matches!(i.scene(), Scene::Ears { left: Ear::Glyph(l), right: Ear::Glyph(r), target: None } if l.name == BUSY_SYMBOL && l.fx == Fx::Variable && r.name == "hourglass")
}

/// Arrives as the stack with its words; a resend changes the words in the same icon slot (nothing arrives again);
/// after its read it rests as a token that a resend never opens; it costs no ticks at rest; back, the island rests.
#[test]
fn catching_up_reads_then_rests_as_a_token_and_clears_by_itself() {
    let mut i = island();
    i.apply(busy("Catching up · 6,489 notes to save"), 1.0);
    assert_eq!(stack_words(&i).as_deref(), Some("Catching up · 6,489 notes to save"), "{:?}", i.scene());
    run_to(&mut i, 2.0);
    let icon = |i: &Island, t: f64| i.frame(t).slots.iter().find(|s| s.id.as_ref() == "stack.icon").map(|s| (s.key, s.since));
    let before = icon(&i, 2.0).expect("the stack's icon");
    // The app resends it with a lower count: the words change, the icon slot is the same content from the same time.
    i.apply(busy("Catching up · 2,310 notes to save"), 3.0);
    run_to(&mut i, 3.5);
    assert_eq!(stack_words(&i).as_deref(), Some("Catching up · 2,310 notes to save"), "{:?}", i.scene());
    assert_eq!(icon(&i, 3.5), Some(before), "the icon did not arrive again");
    assert!(!i.alerting());
    // Read from its arrival, not from its last words.
    run_to(&mut i, 1.0 + scene::BUSY_READ + 0.05);
    assert!(busy_token(&i), "{:?}", i.scene());
    // Resting: a resend never opens it again, and the live window runs no clock (the loop is the render server's).
    i.apply(busy("Catching up · 940 notes to save"), 7.0);
    run_to(&mut i, 9.0);
    assert!(busy_token(&i), "{:?}", i.scene());
    assert_eq!(i.needs_ticks(9.0), FrameNeed::None);
    assert_eq!(i.deadline(), None);
    // Back: it leaves with no success mark.
    i.apply(Event::Health(HealthCue::Recovered { key: "archive".into(), part: Part::Agent, label: "Caught up".into() }), 10.0);
    run_to(&mut i, 12.0);
    assert_eq!(i.class(), Class::Rest, "{:?}", i.scene());
    assert!(!format!("{:?}", i.scene()).contains("checkmark"));
}

/// A restart over it wins (orange stack, then its token); once the restart is back, catching up's token returns
/// without opening again (its read was spent).
#[test]
fn a_restart_holds_over_catching_up() {
    let mut i = island();
    i.apply(busy("Catching up · 6,489 notes to save"), 1.0);
    run_to(&mut i, 6.0);
    assert!(busy_token(&i));
    i.apply(Event::Health(HealthCue::Restarting { key: "listen".into(), part: Part::Transcription, label: "Restarting transcription".into() }), 6.5);
    run_to(&mut i, 7.0);
    assert!(matches!(i.scene(), Scene::Stack { icon, .. } if icon.name == "arrow.clockwise"), "{:?}", i.scene());
    i.apply(busy("Catching up · 3,100 notes to save"), 7.5);
    run_to(&mut i, 12.0);
    assert!(matches!(i.scene(), Scene::Ears { left: Ear::Glyph(g), .. } if g.name == "arrow.clockwise"), "{:?}", i.scene());
    i.apply(Event::Health(HealthCue::Recovered { key: "listen".into(), part: Part::Transcription, label: "Transcription back".into() }), 13.0);
    run_to(&mut i, 13.2);
    assert!(busy_token(&i), "{:?}", i.scene());
    i.apply(Event::Health(HealthCue::Clear { key: "archive".into() }), 14.0);
    run_to(&mut i, 16.0);
    assert_eq!(i.class(), Class::Rest);
}

/// The simulator's invariants hold across it, its updates and a restart over it.
#[test]
fn catching_up_passes_the_invariants() {
    let ev = |t: f64, e: Event| (t, sim::Step::Ev(e));
    let s = sim::Scenario::new(
        "catching-up",
        "catching up, its count falling, a restart over it, then back",
        vec![
            ev(0.1, busy("Catching up · 6,489 notes to save")),
            ev(2.1, busy("Catching up · 4,920 notes to save")),
            ev(4.1, busy("Catching up · 3,310 notes to save")),
            ev(5.0, Event::Health(HealthCue::Restarting { key: "listen".into(), part: Part::Transcription, label: "Restarting transcription".into() })),
            ev(6.1, busy("Catching up · 1,204 notes to save")),
            ev(8.0, Event::Health(HealthCue::Recovered { key: "listen".into(), part: Part::Transcription, label: "Transcription back".into() })),
            ev(9.0, Event::Health(HealthCue::Recovered { key: "archive".into(), part: Part::Agent, label: "Caught up".into() })),
        ],
    );
    for cfg in [sim::Config::default(), sim::Config { fps: 60.0, ..sim::Config::default() }, sim::Config { jitter: Some(7), ..sim::Config::default() }] {
        let r = sim::run(&s, &cfg);
        assert!(r.ok(), "{:#?}", r.findings);
    }
}
