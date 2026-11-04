//! Night review I1 (bug note 2026-10-20T18-41-17, "Oh, Heimdall animation is bugged. I can still see it."): a heard
//! command cue that its verdict drops leaves nothing of itself on the island, in every combination with what else
//! the island shows, and its exit is reported. The real record at 18:39:46: `Heard { kind: LookAtThis }` at .553,
//! the agent's turn starting at .596, `Dropped` at .608, with the "Ornith 122B sources" note (a detail, opened at
//! 18:38:01) still on screen.

use super::*;
use crate::agent_notice::{AgentNotice, Detail, NoticeKind};
use crate::island::{Activity, CommandCue, CommandKind, HealthCue, Part};

fn island() -> Island {
    Island::new(Display::MBP16, Box::new(Approx))
}

/// The heard/pending command glyph pair (command icon + hourglass ear) is in the scene.
fn command_glyph(i: &Island) -> bool {
    format!("{:?}", i.scene()).contains("hourglass")
}

fn note(id: &str, title: &str) -> Event {
    let mut n = AgentNotice::line(id, title, NoticeKind::Info);
    n.detail = Some(Detail::Text("https://huggingface.co/incoai/Ornith-1.5-122B-DFlash2".into()));
    Event::Notice { notice: n, body: Some((344.0, 104.0)) }
}

fn heard(id: u64) -> Event {
    Event::Command(CommandCue::Heard { id, kind: CommandKind::LookAtThis })
}

fn dropped(id: u64) -> Event {
    Event::Command(CommandCue::Dropped { id })
}

/// Time passes as the window host runs it: a tick at every deadline up to `to`, then at `to`.
fn advance(i: &mut Island, to: f64) {
    for _ in 0..1000 {
        match i.deadline() {
            Some(d) if d <= to => i.apply(Event::Tick, d),
            _ => break,
        }
    }
    i.apply(Event::Tick, to);
}

fn lefts(i: &mut Island) -> Vec<(Option<u64>, &'static str)> {
    i.take_outputs()
        .into_iter()
        .filter_map(|o| match o {
            Output::Report(IslandReport::Left { id, why, .. }) => Some((id, why)),
            _ => None,
        })
        .collect()
}

/// Runs `setup` (timed events), then a heard cue at `at` dropped 55 ms later (the 18:39:46 spacing), on one island,
/// and `setup` alone on a control island. After the drop no command glyph shows at any later instant, the drop is
/// reported, and from `settle` on the island is exactly where the control is. The drop shows at the next motion slot
/// (`MOTION_GAP` after the heard cue's grow began: at most three motion starts a second, invariant I13).
fn heard_then_dropped_leaves_no_trace(label: &str, setup: &[(f64, Event)], at: f64, settle: f64) {
    let mut with = island();
    let mut control = island();
    let mut events: Vec<(f64, Event)> = setup.to_vec();
    events.push((at, heard(7)));
    events.push((at + 0.055, dropped(7)));
    events.sort_by(|a, b| a.0.total_cmp(&b.0));
    for (t, e) in &events {
        advance(&mut with, *t);
        with.apply(e.clone(), *t);
    }
    for (t, e) in setup {
        advance(&mut control, *t);
        control.apply(e.clone(), *t);
    }
    assert_eq!(lefts(&mut with), [(Some(7), "dropped")], "{label}: the drop is reported once");
    let mut t = at + engine::MOTION_GAP;
    while t < settle + 5.0 {
        advance(&mut with, t);
        assert!(!command_glyph(&with), "{label}: the command glyph is still drawn at {t:.3} s: {:?}", with.scene());
        if t >= settle {
            advance(&mut control, t);
            assert_eq!(with.class(), control.class(), "{label} at {t:.3} s");
            assert_eq!(with.scene(), control.scene(), "{label} at {t:.3} s");
        }
        t += 0.25;
    }
}

#[test]
fn a_dropped_heard_cue_clears_alone_and_beside_everything_else() {
    let line = |id: &str, text: &str| Event::Notice { notice: AgentNotice::line(id, text, NoticeKind::Info), body: None };
    let jarvis = Event::Health(HealthCue::Notice { part: Part::Speech, label: "Jarvis missed a sentence · lighter voice".into() });
    let cases: Vec<(&str, Vec<(f64, Event)>, f64, f64)> = vec![
        ("alone", vec![], 10.0, 11.0),
        // Heimdall speaking the 18:39:58 reply.
        ("while speaking", vec![(9.0, Event::Speaking(true)), (9.0, Event::SpokenText(Some("The Ornith thread is closed.".into()))), (12.0, Event::Speaking(false))], 10.0, 20.0),
        // The 18:39:46 order: the agent's turn starts between the heard cue and its drop.
        ("while the turn starts", vec![(10.04, Event::Sent), (10.04, Event::Activity(Some(Activity::Thinking))), (14.0, Event::Activity(None))], 10.0, 20.0),
        ("while thinking", vec![(8.0, Event::Sent), (8.0, Event::Activity(Some(Activity::Thinking))), (14.0, Event::Activity(None))], 10.0, 20.0),
        ("while a line reads", vec![(9.5, line("l1", "Ornith thread closed"))], 10.0, 20.0),
        ("while a health notice shows", vec![(9.8, jarvis.clone())], 10.0, 20.0),
        ("while a result shows", vec![(9.9, Event::Command(CommandCue::Done { id: None, kind: CommandKind::Screenshot, label: "Screenshot".into() }))], 10.0, 20.0),
        // The note opened at 18:38:01, the cue at 18:39:46: the note stays (it waits for the user), the glyph does not.
        ("while a note is open", vec![(0.0, note("n1", "Ornith 122B sources"))], 105.0, 110.0),
        ("at the same instant as a line", vec![(10.0, line("l2", "Same instant"))], 10.0, 20.0),
    ];
    for (label, setup, at, settle) in cases {
        heard_then_dropped_leaves_no_trace(label, &setup, at, settle);
    }
}

/// Another command still pending keeps its glyph; once it is dropped too, nothing remains, and both exits are
/// reported. Heard and dropped at the same instant leave nothing either.
#[test]
fn every_pending_cue_drop_clears_and_is_reported() {
    let mut i = island();
    i.apply(heard(1), 9.0);
    i.apply(heard(2), 10.0);
    i.apply(dropped(2), 10.055);
    advance(&mut i, 10.5);
    assert!(command_glyph(&i), "command 1 is still pending: its glyph stays");
    i.apply(dropped(1), 11.0);
    for t in [11.0 + engine::MOTION_GAP, 11.5, 13.0, 60.0] {
        advance(&mut i, t);
        assert!(!command_glyph(&i), "at {t}: {:?}", i.scene());
    }
    assert_eq!(i.class(), Class::Rest);
    assert_eq!(lefts(&mut i), [(Some(2), "dropped"), (Some(1), "dropped")]);

    let mut i = island();
    i.apply(heard(3), 5.0);
    i.apply(dropped(3), 5.0);
    advance(&mut i, 5.0 + engine::MOTION_GAP);
    assert!(!command_glyph(&i), "{:?}", i.scene());
    advance(&mut i, 30.0);
    assert_eq!(i.class(), Class::Rest);
    // A drop for a cue that was never heard (or already decided) reports nothing.
    i.apply(dropped(99), 31.0);
    assert_eq!(lefts(&mut i), [(Some(3), "dropped")]);
}

/// Listening off with a cue pending: the glyph leaves and the exit is reported as such.
#[test]
fn listening_off_clears_a_pending_cue_and_reports_it() {
    let mut i = island();
    i.apply(heard(4), 1.0);
    advance(&mut i, 3.0);
    assert!(command_glyph(&i), "pending glyph at rest");
    i.apply(Event::ListeningOff, 4.0);
    advance(&mut i, 10.0);
    assert!(!command_glyph(&i));
    assert_eq!(i.class(), Class::Rest);
    assert_eq!(lefts(&mut i), [(Some(4), "listening off")]);
}

/// The whole 18:38–18:41 minute as the island received it: the note at 18:38:01, the Jarvis notice at 18:38:32, the
/// barge-in cut at 18:38:37, the heard cue and its drop around the turn's start at 18:39:46, the short spoken reply at
/// 18:39:58, and the NO_REPLY turns until 18:40:13. At 18:41:11, when the user said he could still see it, nothing is
/// on the island: the note's pull-down folded after `scene::NOTE_UNATTENDED` and nothing stays beside the notch (bug
/// note 2026-10-20T22-19-52); the notch alone opens the last turn. No command glyph, no voice, no thinking dots.
#[test]
fn the_18_41_minute_ends_at_rest_with_the_turn_under_the_notch() {
    let mut i = island();
    let at = |hms: f64| hms - 18.0 * 3600.0 - 38.0 * 60.0; // seconds after 18:38:00
    let s = |m: f64, sec: f64| at(18.0 * 3600.0 + m * 60.0 + sec);
    let events: Vec<(f64, Event)> = vec![
        (s(38.0, 1.8), note("sources", "Ornith 122B sources")),
        (s(38.0, 12.4), Event::Speaking(true)),
        (s(38.0, 12.4), Event::SpokenText(Some("The Ornith research is done: it's worth a test, but on oMLX.".into()))),
        (s(38.0, 32.1), Event::Health(HealthCue::Notice { part: Part::Speech, label: "Jarvis missed a sentence · lighter voice".into() })),
        (s(38.0, 37.3), Event::SpeechCut),
        (s(39.0, 10.6), Event::Sent),
        (s(39.0, 10.6), Event::Activity(Some(Activity::Thinking))),
        (s(39.0, 15.0), Event::Activity(None)),
        (s(39.0, 46.553), heard(1_236_208_476_987_292)),
        (s(39.0, 46.596), Event::Sent),
        (s(39.0, 46.596), Event::Activity(Some(Activity::Thinking))),
        (s(39.0, 46.608), dropped(1_236_208_476_987_292)),
        (s(39.0, 58.2), Event::Speaking(true)),
        (s(39.0, 58.2), Event::SpokenText(Some("The Ornith thread is closed.".into()))),
        (s(40.0, 0.4), Event::Speaking(false)),
        (s(40.0, 4.5), Event::Activity(None)),
        (s(40.0, 4.9), Event::Activity(Some(Activity::Thinking))),
        (s(40.0, 8.2), Event::Activity(None)),
        (s(40.0, 8.4), Event::Activity(Some(Activity::Thinking))),
        (s(40.0, 13.9), Event::Activity(None)),
    ];
    for (t, e) in events {
        advance(&mut i, t);
        i.apply(e, t);
    }
    advance(&mut i, s(41.0, 11.0));
    assert!(!command_glyph(&i), "{:?}", i.scene());
    assert_eq!(i.class(), Class::Rest, "{:?}", i.scene());
    assert_eq!(i.targets().iter().map(|(t, _)| t.clone()).collect::<Vec<_>>(), vec![Target::Token(Token::Reply)], "only the notch's own target");
    assert!(lefts(&mut i).contains(&(Some(1_236_208_476_987_292), "dropped")));
}
