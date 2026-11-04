//! Island object tests: the recon bugs as scenarios (each red before its fix), the user's flows, and the simulator's
//! generators (grid pairs, a triple slice, seeded random sequences, adversarial text widths) under every invariant.

use super::sim::{self, Config, Scenario, Step, Text};
use super::*;
use crate::agent_notice::{AgentNotice, NoticeAction, NoticeKind};
use crate::island::{Activity, CommandCue, CommandKind, HealthControl, HealthCue, Part};
use crate::reader::{FastRead, FastReadUpdate, ReadOutcome};

fn island() -> Island {
    Island::new(Display::MBP16, Box::new(Approx))
}

fn ev(t: f64, e: Event) -> (f64, Step) {
    (t, Step::Ev(e))
}

fn line(id: &str, text: &str, kind: NoticeKind) -> Event {
    Event::Notice { notice: AgentNotice::line(id, text, kind), body: None }
}

fn question(id: &str, text: &str, options: &[&str], expires_ms: u64) -> Event {
    let mut n = AgentNotice::line(id, text, NoticeKind::Info);
    n.action = Some(NoticeAction::single(id, text, options, expires_ms));
    Event::Notice { notice: n, body: None }
}

fn assert_clean(s: &Scenario) -> sim::Report {
    let r = sim::run(s, &Config::default());
    assert!(r.ok(), "{}: {:#?}", s.id, r.findings);
    let r60 = sim::run(s, &Config { fps: 60.0, ..Config::default() });
    assert!(r60.ok(), "{} at 60 Hz: {:#?}", s.id, r60.findings);
    let rj = sim::run(s, &Config { jitter: Some(7), ..Config::default() });
    assert!(rj.ok(), "{} jittered: {:#?}", s.id, rj.findings);
    r
}

fn settle(i: &mut Island, t: f64) {
    i.apply(Event::Tick, t);
}

fn voice_slots(f: &FrameModel) -> usize {
    f.slots.iter().filter(|s| (&*s.id == "voice.helmet" || &*s.id == "voice.wave") && s.opacity > 0.05).count()
}

// ---------------------------------------------------------------- recon bugs (18 Oct evening)

/// Recon 17:54 crash: `light.rs` indexed `track.paths.len() - 1` on an empty track while the user drew. The track is a
/// non-empty type and every frame of drawing mode's open, Send and close has one.
#[test]
fn drawing_opens_on_an_empty_track() {
    let s = Scenario::new(
        "drawing-opens-on-an-empty-track",
        "Drawing mode opens while the island has no motion and no look; Send at once; close.",
        vec![ev(0.0, Event::DrawOpen), ev(0.0, Event::Ink(0)), (0.4, Step::Ev(Event::Click(Target::DrawSend))), ev(0.41, Event::DrawClose)],
    );
    let r = assert_clean(&s);
    assert!(r.outputs.contains(&Output::DrawSend));
    let mut i = island();
    assert!(i.track(0.0).len() >= 1, "a fresh island has a track");
    i.apply(Event::DrawOpen, 0.0);
    for k in 0..200 {
        let t = k as f64 / 120.0;
        let tr = i.track(t);
        assert!(tr.len() >= 1);
        let _ = tr.at(t);
        let _ = tr.get(tr.len() + 10);
    }
}

/// Recon 17:47 / 17:54: the speaking animation kept playing after the voice stopped. The bars flatten the frame the
/// audio ends, and the look retracts after the grace.
#[test]
fn speaking_ends_when_the_audio_ends() {
    let mut i = island();
    i.apply(Event::Speaking(true), 0.0);
    i.apply(Event::SpokenText(Some("The build is green.".into())), 0.05);
    for k in 1..60 {
        i.apply(Event::SpeechLevel(0.4 + 0.2 * (k as f32 * 0.7).sin()), 0.05 + k as f64 * 0.03);
    }
    assert!(voice_slots(&i.frame(1.9)) == 2);
    i.apply(Event::Speaking(false), 2.0);
    let f = i.frame(2.0);
    let wave = f.slots.iter().find(|s| &*s.id == "voice.wave").expect("waveform");
    assert!(matches!(&*wave.content, Content::Waveform { flat: true, level: 0 }), "flat the frame the audio stops: {:?}", wave.content);
    // Gone after the grace and one retraction.
    let mut t = 2.0;
    while t < 2.0 + scene::GRACE + 0.7 {
        if let Some(d) = i.deadline().filter(|d| *d <= t) {
            i.apply(Event::Tick, d);
        }
        t += 1.0 / 120.0;
    }
    assert_eq!(voice_slots(&i.frame(t)), 0);
    assert_eq!(i.class(), Class::Rest);
    assert_eq!(i.needs_frames(t + 1.0), FrameNeed::None);
}

/// A lost end signal (no `Speaking(false)`) never leaves the waveform running.
#[test]
fn a_voice_without_an_end_signal_stops_after_its_stall() {
    let s = Scenario::new(
        "voice-without-end",
        "Speaking on, levels for a second, then nothing: the look ends by itself.",
        (0..30).map(|k| ev(k as f64 * 0.033, if k == 0 { Event::Speaking(true) } else { Event::SpeechLevel(0.5) })).collect(),
    );
    assert_clean(&s);
    let mut i = island();
    i.apply(Event::Speaking(true), 0.0);
    i.apply(Event::SpokenText(Some("Hello.".into())), 0.1);
    let mut t = 0.1;
    while let Some(d) = i.deadline() {
        assert!(d > t, "deadlines move on");
        t = d;
        i.apply(Event::Tick, t);
        if i.class() == Class::Rest {
            break;
        }
    }
    assert_eq!(i.class(), Class::Rest);
    assert!((t - (0.1 + scene::VOICE_STALL + scene::GRACE)).abs() < 0.01, "ended at {t}");
}

/// The voice and the speed sample may toggle the flag in any order; an off without an on and an on twice are
/// harmless.
#[test]
fn speaking_off_without_on_and_on_twice() {
    let s = Scenario::new(
        "speaking-off-on",
        "off without on, on twice, off once",
        vec![ev(0.0, Event::Speaking(false)), ev(0.1, Event::Speaking(true)), ev(0.2, Event::Speaking(true)), ev(0.3, Event::SpokenText(Some("Two.".into()))), ev(1.0, Event::Speaking(false))],
    );
    assert_clean(&s);
}

/// Stop and an interrupt's audio cut end the speaking look at once (post-install E3.1: within 100 ms). The natural
/// end holds it for `GRACE` between sentences, and a cut is not followed by a grace.
#[test]
fn a_cut_ends_the_speaking_look_at_once_and_a_natural_end_keeps_its_grace() {
    for cut in [Event::SpeechCut, Event::Stop] {
        let mut i = island();
        i.apply(Event::Speaking(true), 0.0);
        i.apply(Event::SpokenText(Some("Step by step.".into())), 0.05);
        i.apply(Event::SpeechLevel(0.5), 0.1);
        settle(&mut i, 1.0);
        assert!(i.speaking(1.0), "{cut:?}: the look is up while the audio plays");
        i.apply(cut.clone(), 2.0);
        assert!(!i.speaking(2.0), "{cut:?}: the look ends the moment the audio is cut");
        assert!(!i.voice_on());
        // The audio's own end, arriving after the cut, neither holds the look nor brings it back.
        i.apply(Event::Speaking(false), 2.01);
        assert!(!i.speaking(2.01), "{cut:?}: a late Speaking(false) after the cut does not hold the look");
    }
    // A natural end: the look holds for the grace, then goes.
    let mut i = island();
    i.apply(Event::Speaking(true), 0.0);
    i.apply(Event::SpokenText(Some("Step by step.".into())), 0.05);
    settle(&mut i, 1.0);
    i.apply(Event::Speaking(false), 2.0);
    assert!(i.speaking(2.0 + scene::GRACE * 0.5), "a natural end keeps the look for the grace");
    assert!(!i.speaking(2.0 + scene::GRACE + 0.01));
}

/// A cut silences the voice; the turn it belongs to is not ended by it (only Stop ends a turn).
#[test]
fn a_speech_cut_leaves_the_turn_running() {
    let mut i = island();
    i.apply(Event::Sent, 0.0);
    i.apply(Event::Activity(Some(Activity::Thinking)), 0.1);
    i.apply(Event::Speaking(true), 0.5);
    i.apply(Event::SpokenText(Some("Working on it.".into())), 0.55);
    settle(&mut i, 1.0);
    i.apply(Event::SpeechCut, 2.0);
    assert!(!i.speaking(2.0));
    assert_ne!(i.class(), Class::Rest, "the thinking turn stays on the island");
}

/// Recon: "Island test complete and it's still there", and the flood of lines during the icon test. Lines read for
/// their kind's time (1.8–3.4 s, a little longer for long text) and retract; a flood shows the newest and ends empty.
#[test]
fn lines_clear_themselves_and_a_flood_ends_empty() {
    let mut i = island();
    i.apply(line("n1", "Island test complete", NoticeKind::Info), 0.0);
    assert_eq!(i.class(), Class::Stack);
    let read = read_seconds(NoticeKind::Info, "Island test complete");
    assert!((read - (2.6 + 0.05 * 8.0)).abs() < 1e-9);
    assert_eq!(i.deadline(), Some(read));
    i.apply(Event::Tick, read);
    assert_eq!(i.class(), Class::Rest);
    // A flood.
    let steps: Vec<(f64, Step)> = (0..50).map(|k| ev(k as f64 * 0.02, line(&format!("f{k}"), &format!("Icon test {k}"), NoticeKind::Info))).collect();
    let r = assert_clean(&Scenario::new("notice-flood", "50 lines in a second", steps));
    assert!(r.motions <= 12, "{} motions", r.motions);
    // The spec's read times by kind, and the cap.
    assert_eq!(read_seconds(NoticeKind::Working, "On it"), 1.8);
    assert_eq!(read_seconds(NoticeKind::Success, "Done"), 2.0);
    assert_eq!(read_seconds(NoticeKind::Warning, "Low disk"), 3.0);
    assert_eq!(read_seconds(NoticeKind::Error, "Build failed"), 3.4);
    assert_eq!(read_seconds(NoticeKind::Info, &"x".repeat(1000)), 8.0);
}

/// Review (fix first): an error reopened from its token came back as a warning, and errors waited as tokens while
/// approved frames retracted. An error now reads with its own red x the whole time and retracts to rest like every
/// line; only health problems rest as a token.
#[test]
fn errors_keep_their_kind_and_retract_like_lines() {
    let mut i = island();
    i.apply(line("e1", "Build failed", NoticeKind::Error), 0.0);
    let read = read_seconds(NoticeKind::Error, "Build failed");
    let icon = |i: &Island| match i.scene() {
        Scene::Stack { icon, .. } => Some(icon.name.to_string()),
        _ => None,
    };
    let first = icon(&i).expect("an error shows as a stack");
    assert_eq!(first, "xmark.circle.fill");
    for k in 1..20 {
        let t = read * k as f64 / 20.0;
        i.apply(Event::Tick, t);
        assert_eq!(icon(&i).as_deref(), Some(first.as_str()), "the error keeps its icon at {t:.2}");
    }
    i.apply(Event::Tick, read);
    assert_eq!(i.class(), Class::Rest, "{:?}", i.scene());
    assert!(i.targets().iter().all(|(t, _)| !matches!(t, Target::Token(_))), "no token left behind");
    assert_eq!(i.deadline(), None);
    // Cleared by id while showing.
    i.apply(line("e2", "Tests failed", NoticeKind::Error), 100.0);
    assert_eq!(i.class(), Class::Stack);
    i.apply(Event::NoticeClear("e2".into()), 100.5);
    assert_eq!(i.class(), Class::Rest);
    // And the simulator's I18 watches every reopen.
    assert_clean(&Scenario::new("error-retracts", "An error, a second error, a clear", vec![ev(0.0, line("e1", "Build failed", NoticeKind::Error)), ev(1.0, line("e1", "Build failed", NoticeKind::Error)), ev(2.0, Event::NoticeClear("e1".into()))]));
}

/// Recon 17:5x: "I cannot click send with my finger". Send and Close are 112 × 34 hit targets; a tap lands on Send at
/// rest and while the ears change (a screenshot while drawing).
#[test]
fn send_and_close_are_tappable_and_stay_tappable_while_the_ears_change() {
    let mut i = island();
    i.apply(Event::DrawOpen, 0.0);
    let t = 1.0;
    settle(&mut i, t);
    let send = i.targets().iter().find(|(t, _)| *t == Target::DrawSend).expect("Send").1;
    let close = i.targets().iter().find(|(t, _)| *t == Target::DrawClose).expect("Close").1;
    assert_eq!((send.w, send.h), (112.0, 34.0));
    assert_eq!((close.w, close.h), (112.0, 34.0));
    assert!((send.mid_x() + close.mid_x()).abs() < 1e-9, "mirrored");
    assert_eq!(i.hit(t, send.center()), Some(Target::DrawSend));
    // Any point of the capsule, corners included.
    for (dx, dy) in [(-50.0, -15.0), (50.0, 15.0), (0.0, 0.0), (-55.0, 0.0)] {
        assert_eq!(i.hit(t, Point::new(send.mid_x() + dx, send.mid_y() + dy)), Some(Target::DrawSend), "{dx} {dy}");
    }
    // A screenshot while drawing: the ears change, Send stays where it is and keeps taking taps mid-motion.
    i.apply(Event::Command(CommandCue::Heard { id: 1, kind: CommandKind::LookAtThis }), 2.0);
    i.apply(Event::Command(CommandCue::Done { id: Some(1), kind: CommandKind::LookAtThis, label: "Screenshot".into() }), 2.4);
    assert!(i.needs_frames(2.45) == FrameNeed::Moving);
    assert_eq!(i.hit(2.45, send.center()), Some(Target::DrawSend));
    i.apply(Event::Click(Target::DrawSend), 2.45);
    assert!(i.take_outputs().contains(&Output::DrawSend));
    // The flash is the capture's confirmation: the keys simply retract (no "Attached" stage after it).
    settle(&mut i, 3.2);
    assert_eq!(i.class(), Class::Rest, "{:?}", i.scene());
}

/// GPU trace 28 Oct 20:05: Heimdall committed its island 30 times a second while the agent worked. The thinking dots
/// and the loading spinner loop as their layers' own animation, so once the arrival's motion settles a live window
/// runs no clock for them (`needs_ticks`), while the model still declares the loop and draws each phase (`frame`).
#[test]
fn a_loop_of_dots_or_spinner_needs_no_ticks() {
    for (name, events) in [
        ("thinking dots", vec![Event::Sent, Event::Activity(Some(Activity::Thinking))]),
        ("loading spinner", vec![Event::Command(CommandCue::Heard { id: 1, kind: CommandKind::Listening })]),
    ] {
        let mut i = island();
        for e in events {
            i.apply(e, 1.0);
        }
        let mut t = 1.0;
        while t < 2.5 {
            if let Some(d) = i.deadline().filter(|d| *d <= t) {
                i.apply(Event::Tick, d);
            }
            t += 0.01;
        }
        assert!(i.frame(t).slots.iter().any(|s| s.content.animated()), "{name}: the loop shows ({:?})", i.scene());
        assert!(matches!(i.needs_frames(t), FrameNeed::Loop { .. }), "{name}: the model declares its loop");
        assert_eq!(i.needs_ticks(t), FrameNeed::None, "{name}: a live window runs no clock for it");
    }
    // The drawing light's breathing still needs its 20 fps.
    let mut i = island();
    i.apply(Event::DrawOpen, 1.0);
    assert!(matches!(i.needs_ticks(3.0), FrameNeed::Loop { fps, .. } if fps <= 20.0));
}

/// Drawing mode's light runs from the island's flares on the island's clock and runs back on close.
#[test]
fn the_drawing_light_runs_on_the_islands_clock() {
    let mut i = island();
    i.apply(Event::DrawOpen, 1.0);
    let f0 = i.frame(1.0);
    assert_eq!(f0.light.run, 0.0);
    let f = i.frame(1.3);
    assert!(f.light.run > 0.3 && f.light.run < 1.0, "{:?}", f.light);
    assert!(f.glow > 0.0);
    let f = i.frame(3.0);
    assert!((f.light.run - 1.0).abs() < 1e-9);
    assert!(matches!(i.needs_frames(3.0), FrameNeed::Loop { fps, .. } if fps <= 20.0), "breathing at ≤ 20 fps");
    i.apply(Event::DrawClose, 4.0);
    assert!(i.frame(4.2).light.run < 1.0);
    let mut t = 4.0;
    while t < 6.0 {
        if let Some(d) = i.deadline().filter(|d| *d <= t) {
            i.apply(Event::Tick, d);
        }
        t += 0.01;
    }
    assert_eq!(i.frame(6.0).light.run, 0.0);
    assert_eq!(i.needs_frames(6.0), FrameNeed::None);
}

// ---------------------------------------------------------------- the user's flows

/// ("That is an animation, but also speech"): the send arrow and the thinking dots wait for the
/// spoken ack's moment (`Event::Hold`), held longer while he talks; then the arrow gets its full time and the dots
/// follow. Compacting is not his speech's and is never held.
#[test]
fn the_turns_lip_waits_for_the_acks_moment() {
    let mut i = island();
    // The wake leaves 1.6 s after his last voice; the ack sounds at 2 s: the island waits 0.4 s.
    i.apply(Event::Hold(0.4), 0.0);
    i.apply(Event::Sent, 0.0);
    i.apply(Event::Activity(Some(Activity::Thinking)), 0.1);
    settle(&mut i, 0.39);
    assert_eq!(i.class(), Class::Rest, "nothing before the ack's moment");
    assert!(i.deadline().is_some_and(|d| d <= 0.4), "the island wakes itself by that moment");
    settle(&mut i, 0.4);
    assert_eq!(i.scene(), &Scene::Lip(Lip::Sent), "the arrow at the ack's moment");
    settle(&mut i, 0.4 + scene::SENT_SECONDS - 0.01);
    assert_eq!(i.scene(), &Scene::Lip(Lip::Sent), "for its full time");
    settle(&mut i, 0.4 + scene::SENT_SECONDS);
    assert_eq!(i.scene(), &Scene::Lip(Lip::Dots), "then the dots");
    // He talks again before the moment: held while he talks, then until the same quiet.
    let mut j = island();
    j.apply(Event::Hold(0.4), 0.0);
    j.apply(Event::Activity(Some(Activity::Thinking)), 0.1);
    j.apply(Event::Hold(f64::INFINITY), 0.2);
    settle(&mut j, 5.0);
    assert_eq!(j.class(), Class::Rest, "never while he talks");
    j.apply(Event::Hold(1.5), 5.0);
    settle(&mut j, 6.49);
    assert_eq!(j.class(), Class::Rest);
    settle(&mut j, 6.5);
    assert_eq!(j.scene(), &Scene::Lip(Lip::Dots));
    // Compacting shows through a hold.
    let mut k = island();
    k.apply(Event::Hold(0.4), 0.0);
    k.apply(Event::Activity(Some(Activity::Compacting)), 0.1);
    settle(&mut k, 0.2);
    assert_ne!(k.class(), Class::Rest, "compacting shows");
}

/// A whole turn: sent lip, thinking dots and working words at the notch's own width (each sized to its content:
///), speaking with words at the ears' width, then it simply retracts: no success mark after speech.
#[test]
fn a_whole_turn_morphs_vertically_and_ends_by_retracting() {
    let mut i = island();
    i.apply(Event::Sent, 0.0);
    assert_eq!(i.scene(), &Scene::Lip(Lip::Sent));
    assert_eq!(i.target_size(), (185.0, 56.0));
    i.apply(Event::Activity(Some(Activity::Thinking)), 0.2);
    settle(&mut i, 1.0);
    assert_eq!(i.scene(), &Scene::Lip(Lip::Dots));
    i.apply(line("w", "On it", NoticeKind::Working), 1.2);
    settle(&mut i, 1.6);
    assert_eq!(i.class(), Class::Working);
    assert_eq!(i.target_size(), (185.0, 94.0), "dots over \"On it\": the notch's width, only deeper");
    i.apply(Event::Speaking(true), 5.0);
    i.apply(Event::SpokenText(Some("The build is green.".into())), 5.1);
    settle(&mut i, 5.5);
    assert_eq!(i.class(), Class::Row);
    assert_eq!(i.target_size(), (285.0, 73.0));
    i.apply(Event::Activity(None), 6.0);
    i.apply(Event::Speaking(false), 7.0);
    settle(&mut i, 7.0 + scene::GRACE);
    assert_eq!(i.class(), Class::Rest, "straight to rest after the voice's grace: {:?}", i.scene());
    let s = Scenario::new(
        "whole-turn",
        "sent, thinking, working words, speaking, done, rest",
        vec![
            ev(0.0, Event::Sent),
            ev(0.2, Event::Activity(Some(Activity::Thinking))),
            ev(1.2, line("w", "On it", NoticeKind::Working)),
            ev(5.0, Event::Speaking(true)),
            ev(5.1, Event::SpokenText(Some("Dave pushed the audio fix an hour ago, and I kept the old build as the rollback.".into()))),
            ev(6.0, Event::Activity(None)),
            ev(9.0, Event::Speaking(false)),
        ],
    );
    assert_clean(&s);
}

/// The voice starting before its words: the shape goes straight to the speaking row (no dip from the lip to the
/// ears and back down); the words then only fade in (gallery judge, agent_turn: the width reversed 4 times).
#[test]
fn speaking_before_its_words_never_dips() {
    let mut i = island();
    i.apply(Event::Activity(Some(Activity::Thinking)), 0.0);
    settle(&mut i, 1.0);
    assert_eq!(i.target_size(), (185.0, 56.0));
    i.apply(Event::Speaking(true), 1.0);
    assert_eq!(i.target_size(), (185.0, 56.0), "the lip holds while the words are on their way (no empty row)");
    i.apply(Event::SpokenText(Some("The build is green.".into())), 1.15);
    assert_eq!(i.target_size(), (285.0, 73.0), "the row opens with its words");
    settle(&mut i, 1.6);
    assert_eq!(i.target_size(), (285.0, 73.0));
    let mut last_h = 0.0;
    let mut grew = true;
    for k in 0..120 {
        let h = i.frame(1.0 + k as f64 / 120.0).shape.h;
        if h + 0.5 < last_h && h < 70.0 {
            grew = false;
        }
        last_h = last_h.max(h);
    }
    assert!(grew, "the height never dips on the way to the row");
}

/// A line while speaking rides as a status row above the sentences; the ears and the words stay.
#[test]
fn a_line_while_speaking_rides_above_the_words() {
    let mut i = island();
    i.apply(Event::Speaking(true), 0.0);
    i.apply(Event::SpokenText(Some("The build is green.".into())), 0.05);
    i.apply(line("n", "Tests passed", NoticeKind::Success), 1.0);
    settle(&mut i, 1.5);
    assert!(matches!(i.scene(), Scene::Speaking { status: Some(_), sentences, .. } if sentences.len() == 1), "{:?}", i.scene());
    let f = i.frame(2.0);
    assert_eq!(voice_slots(&f), 2);
    assert!(f.slots.iter().any(|s| &*s.id == "voice.s0.0" && s.opacity > 0.99), "the sentence stays under the line");
}

/// Questions: Yes / No, answered by a click (white capsules; it retracts at once, no chosen-mark stage); answered by
/// voice; expired.
#[test]
fn questions_answer_retract_and_expire() {
    let mut i = island();
    i.apply(question("q1", "Install the new build?", &["Yes", "No"], 600_000), 0.0);
    settle(&mut i, 1.0);
    assert_eq!(i.class(), Class::Question);
    let yes = i.targets().iter().find(|(t, _)| matches!(t, Target::AskPick { option: 0, .. })).cloned().expect("Yes");
    assert!(yes.1.w >= 84.0 && yes.1.h >= 32.0);
    assert_eq!(i.hit(1.0, yes.1.center()), Some(yes.0.clone()));
    i.apply(Event::Click(yes.0), 1.0);
    let out = i.take_outputs();
    assert!(out.iter().any(|o| matches!(o, Output::Notice(crate::agent_notice::AgentNoticeAction::Answer { answers, .. }) if answers.first().is_some_and(|a| a.labels == ["Yes"]))), "{out:?}");
    assert_eq!(i.class(), Class::Rest, "answered: it retracts at once ({:?})", i.scene());
    // Expired: answers go, one Dismiss.
    i.apply(question("q2", "Run the full test suite now?", &["Yes", "No"], 12_000), 10.0);
    settle(&mut i, 12.0);
    assert!(matches!(i.scene(), Scene::Question { ref card, .. } if card.look == QLook::Expired), "{:?}", i.scene());
    assert!(i.targets().iter().any(|(t, _)| matches!(t, Target::Dismiss(_))));
    settle(&mut i, 12.0 + scene::EXPIRED_HOLD);
    assert_eq!(i.class(), Class::Rest);
    // By voice.
    i.apply(question("q3", "Push it?", &["Yes", "No"], 600_000), 50.0);
    i.apply(Event::CloseQuestion { id: "q3".into(), answer: "no.".into() }, 51.0);
    assert_eq!(i.class(), Class::Rest, "{:?}", i.scene());
    let s = Scenario::new(
        "question-under-status",
        "a question with a restart above it, answered",
        vec![
            ev(0.0, question("q1", "Install the new build?", &["Yes", "No"], 600_000)),
            ev(0.5, Event::Health(HealthCue::Restarting { key: "agent".into(), part: Part::Agent, label: "Restarting agent".into() })),
            (1.5, Step::ClickAny(0)),
            ev(4.0, Event::Health(HealthCue::Recovered { key: "agent".into(), part: Part::Agent, label: "Agent back".into() })),
        ],
    );
    assert_clean(&s);
}

/// A note opens the last turn's pull-down; its lip pulls it up to nothing beside the notch (bug note
/// 2026-10-20T22-19-52); the notch opens it again.
#[test]
fn a_note_opens_the_turn_and_the_notch_opens_it_again() {
    let mut i = island();
    i.apply(Event::Notice { notice: agent_note("d1"), body: Some((344.0, 104.0)) }, 0.0);
    assert_eq!(i.class(), Class::Reply);
    settle(&mut i, 1.0);
    let band = i.targets().iter().find(|(t, _)| *t == Target::ReplyBand).cloned().expect("band");
    assert!(band.1.w >= 340.0 && band.1.h >= 24.0, "{:?}", band.1);
    i.apply(Event::Click(Target::ReplyBand), 5.0);
    assert_eq!(i.class(), Class::Rest, "nothing on the left and right: {:?}", i.scene());
    assert_eq!(i.targets().iter().map(|(t, _)| t.clone()).collect::<Vec<_>>(), vec![Target::Token(Token::Reply)], "the notch alone");
    i.apply(Event::Click(Target::Token(Token::Reply)), 6.0);
    assert_eq!(i.class(), Class::Reply);
    assert!(matches!(i.scene(), Scene::Reply { notes, .. } if notes.len() == 1));
}

// ---------------------------------------------------------------- an agent's note left open (bug note 2026-10-20T18-41-17)

fn agent_note(id: &str) -> AgentNotice {
    let mut n = AgentNotice::line(id, "Ornith 122B sources", NoticeKind::Info);
    n.detail = Some(crate::agent_notice::Detail::Text("One source.\nAnother source.".into()));
    n
}

/// Runs deadlines up to `t` (as the host's one-shot timer does).
fn run_to(i: &mut Island, t: f64) {
    while let Some(d) = i.deadline().filter(|d| *d <= t) {
        i.apply(Event::Tick, d);
    }
    settle(i, t);
}

/// 18:38: the agent's `island_notice` with a detail opened as a note and stayed open, which the user took for a stuck
/// island. The pull-down an agent's note opened pulls itself up after `NOTE_UNATTENDED` without the pointer over it
/// and leaves nothing beside the notch; the notch opens it again, and then it is the user's.
#[test]
fn an_agents_note_pulls_itself_up_when_nobody_is_at_it() {
    let mut i = island();
    i.apply(Event::Notice { notice: agent_note("d1"), body: Some((344.0, 104.0)) }, 1.0);
    assert_eq!(i.class(), Class::Reply);
    assert_eq!(i.deadline(), Some(1.0 + scene::NOTE_UNATTENDED), "the pull-down's own deadline, not open for good");
    run_to(&mut i, 1.0 + scene::NOTE_UNATTENDED - 0.01);
    assert_eq!(i.class(), Class::Reply, "it reads for the whole time");
    run_to(&mut i, 1.0 + scene::NOTE_UNATTENDED);
    assert_eq!(i.class(), Class::Rest, "pulled up, nothing left beside the notch: {:?}", i.scene());
    assert!(i.targets().iter().any(|(t, _)| *t == Target::Token(Token::Reply)), "the notch opens it again");
    // Opened again by the user from the notch: his, it stays.
    i.apply(Event::Click(Target::Token(Token::Reply)), 20.0);
    assert_eq!(i.class(), Class::Reply);
    run_to(&mut i, 20.0 + 3.0 * scene::NOTE_UNATTENDED);
    assert_eq!(i.class(), Class::Reply, "a pull-down User opened waits for him");
    // The agent updates the note he has open: still his.
    i.apply(Event::Notice { notice: agent_note("d1"), body: Some((344.0, 140.0)) }, 70.0);
    run_to(&mut i, 70.0 + 2.0 * scene::NOTE_UNATTENDED);
    assert_eq!(i.class(), Class::Reply, "an update keeps User's pull-down open");
    // He pulls it up; a new note from the agent is the agent's again.
    i.apply(Event::Click(Target::ReplyBand), 100.0);
    assert_eq!(i.class(), Class::Rest);
    i.apply(Event::Notice { notice: agent_note("d2"), body: Some((344.0, 104.0)) }, 101.0);
    assert_eq!(i.class(), Class::Reply);
    run_to(&mut i, 101.0 + scene::NOTE_UNATTENDED);
    assert_eq!(i.class(), Class::Rest);
}

/// The pointer over the pull-down (anywhere on it, or its band) holds it open; leaving starts the time over.
/// Scrolling is reading: it starts the time over too.
#[test]
fn the_pointer_over_an_agents_note_holds_it() {
    let mut i = island();
    i.apply(Event::Notice { notice: agent_note("d1"), body: Some((344.0, 3000.0)) }, 0.0);
    i.apply(Event::Pointer(true), 5.0);
    assert_eq!(i.deadline().filter(|d| *d < 1e6), None, "nothing to wait for while he is at it: {:?}", i.deadline());
    run_to(&mut i, 300.0);
    assert_eq!(i.class(), Class::Reply, "held while the pointer is over it");
    i.apply(Event::Pointer(false), 300.0);
    run_to(&mut i, 300.0 + scene::NOTE_UNATTENDED - 0.5);
    assert_eq!(i.class(), Class::Reply, "the time starts when the pointer leaves");
    i.apply(Event::Scroll(40.0), 311.0);
    run_to(&mut i, 311.0 + scene::NOTE_UNATTENDED - 0.5);
    assert_eq!(i.class(), Class::Reply, "a scroll starts it over");
    run_to(&mut i, 311.0 + scene::NOTE_UNATTENDED);
    assert_eq!(i.class(), Class::Rest);
    // The band hot (the pointer over the lip) holds it as well.
    let mut j = island();
    j.apply(Event::Notice { notice: agent_note("d1"), body: Some((344.0, 104.0)) }, 0.0);
    j.apply(Event::Hover(Some(Target::ReplyBand)), 1.0);
    run_to(&mut j, 60.0);
    assert_eq!(j.class(), Class::Reply);
    j.apply(Event::Hover(None), 60.0);
    run_to(&mut j, 60.0 + scene::NOTE_UNATTENDED);
    assert_eq!(j.class(), Class::Rest);
}

/// Hidden under a question, its time does not run: once the question is answered the note reads in full again.
#[test]
fn an_agents_note_under_a_question_reads_in_full_after_it() {
    let mut i = island();
    i.apply(Event::Notice { notice: agent_note("d1"), body: Some((344.0, 104.0)) }, 0.0);
    i.apply(question("q1", "Install the new build?", &["Yes", "No"], 600_000), 2.0);
    assert_eq!(i.class(), Class::Question);
    run_to(&mut i, 60.0);
    i.apply(Event::Click(Target::AskPick { id: "q1".into(), question: 0, option: 0 }), 60.0);
    run_to(&mut i, 60.5);
    assert_eq!(i.class(), Class::Reply, "{:?}", i.scene());
    run_to(&mut i, 60.0 + scene::NOTE_UNATTENDED - 0.5);
    assert_eq!(i.class(), Class::Reply);
    run_to(&mut i, 61.0 + scene::NOTE_UNATTENDED);
    assert_eq!(i.class(), Class::Rest);
}

/// The whole flow under every invariant (I22 included): the agent's note after a spoken reply pulls itself up, the user
/// opens the turn from the notch and it stays, the pointer holds the next one.
#[test]
fn agent_and_owner_notes_under_the_simulator() {
    assert_clean(&Scenario::new(
        "note-unattended",
        "the agent's note pulls itself up; the pointer holds it; User's stays",
        vec![
            (0.2, Step::Ev(Event::Speaking(true))),
            (0.3, Step::Ev(Event::SpokenText(Some("The Ornith thread is closed.".into())))),
            (0.5, Step::Ev(Event::Notice { notice: agent_note("d1"), body: Some((344.0, 104.0)) })),
            (2.0, Step::Ev(Event::Speaking(false))),
            (20.0, Step::Ev(Event::Click(Target::Token(Token::Reply)))),
            (21.0, Step::Ev(Event::Pointer(true))),
            (40.0, Step::Ev(Event::Pointer(false))),
            (80.0, Step::Ev(Event::Click(Target::ReplyBand))),
            (81.0, Step::Ev(Event::Notice { notice: agent_note("d2"), body: Some((344.0, 104.0)) })),
            (83.0, Step::Ev(Event::Pointer(true))),
            (90.0, Step::Ev(Event::Pointer(false))),
        ],
    ));
}

// ---------------------------------------------------------------- the last turn (bug notes 2026-10-20T22-19-52, 23-34-14)

fn jobs(id: &str) -> Event {
    let mut n = gallery::jobs_note();
    n.id = id.into();
    Event::Notice { notice: n, body: Some((520.0, 100.0)) }
}

fn turn_of(i: &Island) -> (Vec<String>, Vec<String>) {
    match i.scene() {
        Scene::Reply { paragraphs, notes, .. } => (paragraphs.clone(), notes.iter().map(|n| n.id.clone()).collect()),
        other => panic!("not the pull-down: {other:?}"),
    }
}

/// 23:11–23:33: the jobs table stayed for 20 minutes and a click on the island opened it, not the last reply. The
/// table belongs to its turn: the pull-down shows that turn's reply with it; the next turn's reply retires it, and
/// the notch then opens the newest reply alone.
#[test]
fn a_later_turn_retires_the_last_turns_table() {
    let mut i = island();
    // Turn 1: the table, then its reply as written.
    i.apply(jobs("notice-dd8e"), 0.0);
    i.apply(Event::LastReply("Three local AI jobs in Warsaw.".into()), 0.5);
    settle(&mut i, 1.0);
    assert_eq!(turn_of(&i), (vec!["Three local AI jobs in Warsaw.".to_string()], vec!["notice-dd8e".to_string()]), "the reply with its table");
    i.apply(Event::Click(Target::ReplyBand), 2.0);
    // Turn 2 (minutes later): a reply without a note.
    i.apply(Event::LastReply("The CV is in Downloads.".into()), 120.0);
    assert!(i.note_ids().is_empty(), "the table retired with its turn");
    i.apply(Event::Click(Target::Token(Token::Reply)), 1320.0);
    assert_eq!(turn_of(&i), (vec!["The CV is in Downloads.".to_string()], vec![]), "the notch opens the last reply, not the old table");
    // Nothing on either side of the notch at rest, ever.
    i.apply(Event::Click(Target::ReplyBand), 1321.0);
    run_to(&mut i, 2000.0);
    assert_eq!(i.class(), Class::Rest);
}

/// A spoken reply of a later turn (no written reply yet) retires the table too; the voice of the turn's own final
/// reply does not.
#[test]
fn a_later_spoken_reply_retires_the_table_but_its_own_voice_does_not() {
    let mut i = island();
    i.apply(jobs("t1"), 0.0);
    i.apply(Event::LastReply("Three jobs.".into()), 0.5);
    // The final reply is said right after it is written: the same turn.
    i.apply(Event::Speaking(true), 1.0);
    i.apply(Event::SpokenText(Some("Three jobs.".into())), 1.05);
    i.apply(Event::Speaking(false), 2.0);
    assert_eq!(i.note_ids(), vec!["t1".to_string()], "its own voice keeps the table");
    // Next turn (a new run): "On it" by `speak` before anything is written.
    i.apply(Event::Activity(Some(Activity::Thinking)), 59.0);
    i.apply(Event::Speaking(true), 60.0);
    i.apply(Event::SpokenText(Some("On it.".into())), 60.05);
    i.apply(Event::Speaking(false), 61.0);
    assert!(i.note_ids().is_empty(), "the next turn's words retire it");
    settle(&mut i, 62.0);
    i.apply(Event::Click(Target::Token(Token::Reply)), 63.0);
    assert_eq!(turn_of(&i), (vec!["On it.".to_string()], vec![]));
    // A slow first sentence (its synthesis waited 20 s behind other speech) is still the written reply's own turn.
    let mut j = island();
    j.apply(jobs("slow"), 0.0);
    j.apply(Event::LastReply("Three jobs.".into()), 0.5);
    j.apply(Event::Speaking(true), 20.5);
    j.apply(Event::SpokenText(Some("Three jobs.".into())), 20.55);
    assert_eq!(j.note_ids(), vec!["slow".to_string()], "the table stays with its own reply's late voice");
}

/// Review of 063424a, finding 1: a run that shows a table and ends NO_REPLY sends no `LastReply`. Its table must not
/// attach to the reply of a run 20 minutes later, nor stack with the next run's notes. A run starts when the agent
/// goes from idle to thinking.
#[test]
fn a_silent_runs_table_retires_when_the_next_run_starts() {
    let thinking = Event::Activity(Some(Activity::Thinking));
    let mut i = island();
    i.apply(thinking.clone(), 0.0);
    i.apply(jobs("a"), 1.0);
    i.apply(Event::Activity(None), 2.0); // NO_REPLY: the run ends with no written reply
    i.apply(thinking.clone(), 1200.0);
    i.apply(Event::LastReply("The CV is in Downloads.".into()), 1210.0);
    assert!(i.note_ids().is_empty(), "the silent run's table retired with its run: {:?}", i.note_ids());
    i.apply(Event::Click(Target::Token(Token::Reply)), 1211.0);
    assert_eq!(turn_of(&i), (vec!["The CV is in Downloads.".to_string()], vec![]));
    // Two silent runs never stack their notes.
    let mut j = island();
    j.apply(thinking.clone(), 0.0);
    j.apply(jobs("a"), 1.0);
    j.apply(Event::Activity(None), 2.0);
    j.apply(thinking.clone(), 60.0);
    j.apply(jobs("b"), 61.0);
    j.apply(Event::Activity(None), 62.0);
    assert_eq!(j.note_ids(), vec!["b".to_string()], "only the newest run's note");
    // Compacting back to thinking is the same run.
    let mut k = island();
    k.apply(thinking.clone(), 0.0);
    k.apply(jobs("c"), 1.0);
    k.apply(Event::Activity(Some(Activity::Compacting)), 2.0);
    k.apply(thinking.clone(), 5.0);
    k.apply(Event::LastReply("Done.".into()), 6.0);
    assert_eq!(k.note_ids(), vec!["c".to_string()], "compaction inside a run keeps its table");
}

/// The run's end (Ready) and its reply as written come on different channels, in either order: the run's own table
/// stays with its reply both ways.
#[test]
fn ready_before_or_after_the_reply_keeps_the_turns_table() {
    for ready_first in [true, false] {
        let mut i = island();
        i.apply(Event::Activity(Some(Activity::Thinking)), 0.0);
        i.apply(jobs("t"), 1.0);
        if ready_first {
            i.apply(Event::Activity(None), 2.0);
            i.apply(Event::LastReply("Three jobs.".into()), 2.0);
        } else {
            i.apply(Event::LastReply("Three jobs.".into()), 2.0);
            i.apply(Event::Activity(None), 2.0);
        }
        settle(&mut i, 3.0);
        i.apply(Event::Click(Target::ReplyBand), 3.0);
        i.apply(Event::Click(Target::Token(Token::Reply)), 4.0);
        assert_eq!(turn_of(&i), (vec!["Three jobs.".to_string()], vec!["t".to_string()]), "ready first: {ready_first}");
        // The next run's reply retires it.
        i.apply(Event::Activity(Some(Activity::Thinking)), 30.0);
        i.apply(Event::LastReply("Next.".into()), 35.0);
        assert!(i.note_ids().is_empty(), "ready first: {ready_first}");
    }
}

/// Finding 2: a pause of more than 3 s inside the turn's own spoken reply (a voice retry took 6 s, heimdall.log
/// 01:28:57–01:29:03) keeps its table and its written words.
#[test]
fn a_pause_inside_the_turns_own_voice_keeps_its_table() {
    let mut i = island();
    i.apply(Event::Activity(Some(Activity::Thinking)), 0.0);
    i.apply(jobs("t"), 0.2);
    i.apply(Event::LastReply("Three jobs. BlockWise fits best.".into()), 0.5);
    i.apply(Event::Activity(None), 0.5);
    i.apply(Event::Speaking(true), 1.0);
    i.apply(Event::SpokenText(Some("Three jobs.".into())), 1.05);
    i.apply(Event::Speaking(false), 3.0);
    i.apply(Event::Speaking(true), 6.5);
    i.apply(Event::SpokenText(Some("BlockWise fits best.".into())), 6.55);
    i.apply(Event::Speaking(false), 8.0);
    assert_eq!(i.note_ids(), vec!["t".to_string()], "the table stays through the pause");
    settle(&mut i, 9.0);
    i.apply(Event::Click(Target::Token(Token::Reply)), 9.0);
    assert_eq!(turn_of(&i), (vec!["Three jobs. BlockWise fits best.".to_string()], vec!["t".to_string()]));
}

/// Within one turn a note shown before the turn's reply stays with it (`speak` lines in between included), and a
/// note shown in a new turn before its reply never sits under the older turn's words.
#[test]
fn a_turns_notes_stay_with_its_own_reply() {
    let mut i = island();
    i.apply(Event::LastReply("Earlier answer.".into()), 0.0);
    // New turn: a spoken update, a table, more speech, then the written reply.
    i.apply(jobs("t2"), 30.0);
    settle(&mut i, 31.0);
    assert_eq!(turn_of(&i), (vec![], vec!["t2".to_string()]), "the earlier turn's words are not this turn's");
    i.apply(Event::Speaking(true), 32.0);
    i.apply(Event::SpokenText(Some("Here are three.".into())), 32.05);
    i.apply(Event::Speaking(false), 33.0);
    assert_eq!(i.note_ids(), vec!["t2".to_string()], "speech within the turn keeps its table");
    i.apply(Event::LastReply("Here are three. BlockWise fits best.".into()), 34.0);
    settle(&mut i, 35.0);
    assert_eq!(turn_of(&i), (vec!["Here are three. BlockWise fits best.".to_string()], vec!["t2".to_string()]));
    // A second note in the same turn joins it; the same id again replaces in place.
    let mut j = island();
    j.apply(jobs("a"), 0.0);
    j.apply(jobs("b"), 1.0);
    j.apply(jobs("a"), 2.0);
    assert_eq!(j.note_ids(), vec!["a".to_string(), "b".to_string()]);
}

/// Each note has a small X: closing it removes it for good (an update of the same id never brings it back); the
/// reply stays; a pull-down with nothing left closes.
#[test]
fn a_notes_x_closes_it_for_good() {
    let mut i = island();
    i.apply(jobs("t1"), 0.0);
    i.apply(Event::LastReply("Three jobs.".into()), 0.1);
    settle(&mut i, 1.0);
    let x = i.targets().iter().find(|(t, _)| *t == Target::NoteClose("t1".into())).cloned().expect("the note's X");
    assert!(x.1.w >= 24.0 && x.1.h >= 24.0, "a trackpad tap's size: {:?}", x.1);
    let body = i.items().find(|(id, _, _)| &***id == "reply.notes").map(|(_, _, r)| r).expect("the notes' body");
    assert!(x.1.max_x() <= body.max_x() + 6.0 && x.1.y >= body.y, "at the header's right end, inside the body: {x:?} {body:?}");
    // The X is what a click there lands on.
    assert_eq!(i.hit(1.0, Point::new(x.1.mid_x(), x.1.mid_y())), Some(Target::NoteClose("t1".into())));
    i.apply(Event::Click(Target::NoteClose("t1".into())), 1.5);
    assert!(i.note_ids().is_empty());
    assert_eq!(i.class(), Class::Reply, "the reply stays open");
    i.apply(jobs("t1"), 2.0);
    assert!(i.note_ids().is_empty(), "closed for good");
    // Only a note in the turn: closing it closes the pull-down.
    let mut j = island();
    j.apply(jobs("n"), 0.0);
    settle(&mut j, 1.0);
    j.apply(Event::Click(Target::NoteClose("n".into())), 1.0);
    settle(&mut j, 2.0);
    assert_eq!(j.class(), Class::Rest);
    assert!(j.targets().is_empty(), "nothing left to open");
}

/// A wide table widens the pull-down past the reply's 480 (its columns whole), up to the screen-bound maximum, and
/// a long one scrolls; the X scrolls with its header and the targets never start a motion while scrolling.
#[test]
fn a_wide_table_widens_the_pull_down_and_scrolls_in_place() {
    let mut i = island();
    i.apply(jobs("w"), 0.0);
    i.apply(Event::LastReply("Three jobs.".into()), 0.1);
    settle(&mut i, 1.0);
    let (w, _) = i.target_size();
    assert!(w > 480.0 && w <= layout::reply_max_w(&Display::MBP16), "{w}");
    // A long table: taller than half the screen, it scrolls.
    let mut tall = gallery::jobs_note();
    tall.id = "tall".into();
    let mut k = island();
    k.apply(Event::Notice { notice: tall, body: Some((520.0, 2000.0)) }, 0.0);
    settle(&mut k, 1.0);
    let (_, h) = k.target_size();
    assert!(h <= Display::MBP16.height / 2.0 + 1.0, "{h}");
    let before = k.motion_starts().len();
    let x0 = k.targets().iter().find(|(t, _)| matches!(t, Target::NoteClose(_))).map(|(_, r)| *r).expect("x");
    for n in 0..20 {
        k.apply(Event::Scroll(15.0), 1.0 + n as f64 * 0.016);
    }
    assert_eq!(k.motion_starts().len(), before, "scrolling started motions");
    let x1 = k.targets().iter().find(|(t, _)| matches!(t, Target::NoteClose(_))).map(|(_, r)| *r);
    assert!(x1.is_none(), "the header scrolled out: its X takes no clicks ({x0:?} → {x1:?})");
    assert!(matches!(k.scene(), Scene::Reply { scroll, .. } if *scroll > 200.0));
    let f = k.frame(1.5);
    assert!(f.slots.iter().all(|s| !s.id.contains('~')), "nothing leaves while scrolling");
    // Scrolled to the end and no further.
    k.apply(Event::Scroll(1e9), 2.0);
    let (full, shown) = k.items().find(|(id, _, _)| &***id == "reply.notes").map(|(_, c, _)| (c.body_full(), (**c).clone())).expect("body");
    assert!(matches!(shown, Content::Body { scroll, h, .. } if h > 0.0 && (scroll + h - full).abs() < 1e-6), "its end shows: {shown:?} of {full}");
    // The wheel's position was held at the end (not 1e9 points down): one step back shows the rows above the end.
    k.apply(Event::Scroll(-30.0), 2.1);
    let back = k.items().find(|(id, _, _)| &***id == "reply.notes").map(|(_, c, _)| (**c).clone()).expect("body");
    assert!(matches!(back, Content::Body { scroll, h, .. } if (scroll + h + 30.0 - full).abs() < 1e-6), "one step back from the end: {back:?}");
}

/// Finding 3: three 200-row tables and a reply passed the 16,384 px bitmap limit at 2× and the body drew nothing (a
/// black box). Every body bitmap fits; the oldest notes give way, the newest stays.
#[test]
fn the_pull_down_never_passes_the_bitmap_limit() {
    let d = Display::MBP16;
    let notes: Vec<NoteView> = ["old", "mid", "new"].iter().map(|id| NoteView { id: id.to_string(), rev: 1, icon: Glyph::new("tablecells"), title: "Jobs".into(), body: (600.0, 3000.0) }).collect();
    let paragraphs = vec!["Sentence after sentence. ".repeat(60); 30];
    let l = layout::layout(&Scene::Reply { copied: false, paragraphs, notes, chat: Default::default(), follow: false, tail: false, ask: None, status: None, level: 0, flat: true, scroll: 0.0, hot: false, composer_empty: true, attached: vec![], working: false }, &d, &Approx);
    let mut kept = vec![];
    for (_, c) in l.items.iter().filter(|i| i.id.starts_with("reply.")).map(|i| (i.id.clone(), (*i.content).clone())) {
        if let Content::Body { notes, .. } = &c {
            kept.extend(notes.iter().map(|n| n.id.clone()));
            assert!(c.body_full() * d.scale <= layout::MAX_RASTER_PX, "a body of {} pt at 2x", c.body_full());
        }
    }
    assert!(kept.contains(&"new".to_string()), "the newest note stays: {kept:?}");
    assert!(!kept.contains(&"old".to_string()), "the oldest gives way: {kept:?}");
}

/// Finding 5: closing a note left the reply fading out and back in, jumping from the left to the centre with a faint
/// double image. The reply's words are the same element: they stay fully drawn and glide; only the note leaves.
#[test]
fn closing_a_note_keeps_the_reply_still() {
    let mut i = island();
    i.apply(jobs("t"), 0.0);
    i.apply(Event::LastReply("Three local AI jobs in Warsaw.".into()), 0.1);
    i.apply(Event::Pointer(true), 0.2);
    settle(&mut i, 2.0);
    i.apply(Event::Click(Target::NoteClose("t".into())), 2.0);
    let mut t = 2.0;
    while t < 3.0 {
        let f = i.frame(t);
        assert!(f.slots.iter().all(|s| !(s.id.starts_with("reply.body~") && s.opacity > 0.02)), "the reply's words never leave as a ghost at {t:.3}");
        let words = f.slots.iter().find(|s| &*s.id == "reply.body").map(|s| s.opacity);
        assert!(words.is_some_and(|o| o > 0.98), "the reply's words stay drawn at {t:.3}: {words:?}");
        t += 1.0 / 120.0;
    }
}

/// Finding 6: retracting, the table's bottom row was sliced flat by the shape's edge at near full opacity. Content
/// the shrinking shape cuts has faded most of the way.
#[test]
fn a_retracting_pull_down_fades_what_the_edge_cuts() {
    let mut i = island();
    i.apply(Event::LastReply("The tests are faster now.".into()), 0.0);
    i.apply(Event::Click(Target::Token(Token::Reply)), 0.1);
    i.apply(jobs("t"), 0.2);
    settle(&mut i, 2.0);
    i.apply(Event::Click(Target::ReplyBand), 2.0);
    let mut t = 2.0;
    while t < 3.0 {
        let f = i.frame(t);
        for s in f.slots.iter().filter(|s| s.id.starts_with("reply.") && matches!(*s.content, Content::Body { .. })) {
            if s.rect.max_y() > f.shape.h + 2.0 && s.rect.h > 0.0 {
                assert!(s.opacity <= 0.35, "{} cut by the edge at {:.0}% opacity, {t:.3} s ({:?} in {:?})", s.id, s.opacity * 100.0, s.rect, f.shape);
            }
        }
        t += 1.0 / 120.0;
    }
}

/// Finding 7: a long table gave no sign that it scrolls. While more is below, the body's bottom fades out; at its end
/// the fade goes. A body that fits has none.
#[test]
fn a_scrolling_pull_down_fades_out_while_more_is_below() {
    let fade = |i: &Island| i.items().find(|(id, _, _)| &***id == "reply.fade").map(|(_, c, r)| ((**c).clone(), r));
    let mut tall = gallery::jobs_note();
    tall.id = "tall".into();
    let mut i = island();
    i.apply(Event::Notice { notice: tall, body: Some((520.0, 2000.0)) }, 0.0);
    settle(&mut i, 1.0);
    let (c, r) = fade(&i).expect("a fade while more is below");
    assert_eq!(c, Content::Fade { on: true });
    let body = i.items().find(|(id, _, _)| &***id == "reply.notes").map(|(_, _, r)| r).expect("body");
    assert!((r.max_y() - body.max_y()).abs() < 0.5 && r.w >= body.w - 0.5, "over the body's bottom: {r:?} {body:?}");
    let before = i.motion_starts().len();
    i.apply(Event::Scroll(1e9), 1.5);
    assert_eq!(fade(&i).map(|(c, _)| c), Some(Content::Fade { on: false }), "at the end nothing is below");
    assert_eq!(i.motion_starts().len(), before, "the fade changes in place");
    let mut j = island();
    j.apply(jobs("short"), 0.0);
    settle(&mut j, 1.0);
    assert!(fade(&j).is_none(), "a body that fits has no fade");
}

/// The table, the X, the turn and the notch under every invariant.
#[test]
fn the_last_turn_under_the_simulator() {
    assert_clean(&Scenario::new(
        "last-turn",
        "a table with its reply, closed by its X, retired by the next turn, the notch opening the newest reply",
        vec![
            (0.1, Step::Ev(jobs("t1"))),
            (0.4, Step::Ev(Event::LastReply("Three local AI jobs in Warsaw.".into()))),
            (0.6, Step::Ev(Event::Speaking(true))),
            (0.7, Step::Ev(Event::SpokenText(Some("Three local AI jobs in Warsaw.".into())))),
            (2.0, Step::Ev(Event::Speaking(false))),
            (3.0, Step::Ev(Event::Scroll(30.0))),
            (4.0, Step::Ev(Event::Click(Target::NoteClose("t1".into())))),
            (5.0, Step::Ev(Event::Click(Target::ReplyBand))),
            (8.0, Step::Ev(jobs("t2"))),
            (8.2, Step::Ev(Event::LastReply("Another table.".into()))),
            (30.0, Step::Ev(Event::LastReply("Done.".into()))),
            (31.0, Step::Ev(Event::Click(Target::Token(Token::Reply)))),
            (33.0, Step::Ev(Event::Click(Target::ReplyBand))),
        ],
    ));
}

/// The fast reader plays crates/rsvp units for their own durations, the focus letter on the centre line, and
/// closes itself 1.2 s after the end with the Face ID face.
#[test]
fn the_reader_plays_rsvp_units_and_closes_itself() {
    let mut i = island();
    let text = "First, the limit is two minutes, and the tests need longer. Second, the slow part is loading the embedding model.";
    i.apply(Event::Reader(FastReadUpdate::Show(FastRead { id: "r1".into(), text: text.into(), title: Some("Build report".into()), wpm: 240 })), 0.0);
    assert_eq!(i.class(), Class::Reader);
    assert_eq!(i.target_size(), (424.0, 182.0));
    assert_eq!(i.deadline(), None);
    i.apply(Event::Click(Target::ReaderPlay), 1.0);
    let mut t = 1.0;
    let mut units = vec![];
    let mut changes = vec![];
    while t < 60.0 && i.class() != Class::Rest {
        let Some(d) = i.deadline() else { break };
        t = d;
        i.apply(Event::Tick, t);
        if let Scene::Reader { face: ReaderFace::Unit { text, focus }, .. } = i.scene() {
            if units.last() != Some(text) {
                units.push(text.clone());
                changes.push(t);
                // The focus letter's slot rect is anchored on the centre line.
                let f = i.frame(t);
                let unit = f.slots.iter().find(|s| &*s.id == "reader.face").expect("face");
                assert!(unit.rect.x <= 0.0 && unit.rect.max_x() >= 0.0, "{text} {focus} {:?}", unit.rect);
            }
        }
        if let Scene::Reader { face: ReaderFace::End, .. } = i.scene() {
            let f = i.frame(t + 0.05);
            assert!(f.slots.iter().any(|s| matches!(&*s.content, Content::Symbol { name, size, .. } if name == "faceid" && *size == 40.0)));
        }
    }
    assert!(units.len() > 8, "{units:?}");
    let gaps: Vec<i64> = changes.windows(2).map(|w| ((w[1] - w[0]) * 1000.0).round() as i64).collect();
    let distinct: std::collections::BTreeSet<i64> = gaps.iter().copied().collect();
    assert!(distinct.len() > 3, "units show for their own durations, not one interval: {gaps:?}");
    assert_eq!(i.class(), Class::Rest, "closed by itself");
    let out = i.take_outputs();
    assert!(out.iter().any(|o| matches!(o, Output::Read(r) if r.outcome == ReadOutcome::Read)), "{out:?}");
}

/// The reader's ear numerals clear before the shape that holds them retracts: the shape's clip would cut them mid-fade
/// (the 450 and 0:00 in the lab's fast-read frames). Gone within the clock's 0.12 s, while the shape is still up.
#[test]
fn the_reader_ear_numerals_clear_before_the_shape_retracts() {
    let mut i = island();
    let text = "First, the limit is two minutes, and the tests need longer. Second, the slow part is loading the model.";
    i.apply(Event::Reader(FastReadUpdate::Show(FastRead { id: "r1".into(), text: text.into(), title: Some("Build report".into()), wpm: 240 })), 0.0);
    settle(&mut i, 1.0);
    let numeral = |f: &FrameModel| f.slots.iter().filter(|s| s.id.ends_with(".num")).map(|s| s.opacity).fold(0.0, f64::max);
    assert!(numeral(&i.frame(1.0)) > 0.9, "the ears show their numerals while the reader plays");
    i.apply(Event::Reader(FastReadUpdate::Withdraw("r1".into())), 2.0);
    let f = i.frame(2.0 + 0.12);
    assert!(f.shape.h > 40.0, "the shape is still up {:?}", f.shape);
    assert!(numeral(&f) < 0.02, "the numerals are gone by the clock's 0.12 s: {}", numeral(&f));
}

/// Pressing play: the focus dot never inherits the title's rect (live content cuts; the gallery showed the dot
/// stretched into a red blob for a quarter second).
#[test]
fn the_readers_focus_dot_never_stretches() {
    let mut i = island();
    i.apply(Event::Reader(FastReadUpdate::Show(FastRead { id: "r".into(), text: "One two three.".into(), title: Some("Build report".into()), wpm: 240 })), 0.0);
    settle(&mut i, 1.0);
    i.apply(Event::Click(Target::ReaderPlay), 1.0);
    for k in 0..60 {
        let f = i.frame(1.0 + k as f64 / 120.0);
        if let Some(face) = f.slots.iter().find(|s| &*s.id == "reader.face") {
            if matches!(&*face.content, Content::Dot { .. }) {
                assert!(face.rect.w <= 9.0 && face.rect.h <= 9.0, "{:?}", face.rect);
            }
        }
    }
}

/// A problem: stack + white button for its read time, then a token; the token opens it again; the button runs the
/// control.
#[test]
fn a_problem_reads_then_rests_as_a_token_whose_click_reopens_it() {
    let mut i = island();
    let control = HealthControl::OpenSettings(crate::model::Permission::ScreenRecording);
    i.apply(Event::Health(HealthCue::Problem { key: "screen".into(), part: Part::Screen, label: "Screen Recording off".into(), action: "Open Settings".into(), control }), 0.0);
    assert_eq!(i.class(), Class::StackButton);
    let mut t = 0.0;
    while let Some(d) = i.deadline() {
        if d > 30.0 {
            break;
        }
        t = d;
        i.apply(Event::Tick, t);
    }
    assert_eq!(i.class(), Class::Ears, "{:?}", i.scene());
    let tok = i.targets()[0].clone();
    i.apply(Event::Click(tok.0), t + 1.0);
    assert_eq!(i.class(), Class::StackButton);
    i.apply(Event::Click(Target::HealthButton), t + 1.5);
    assert!(i.take_outputs().iter().any(|o| matches!(o, Output::Health { key, .. } if key == "screen")));
}

/// Rest costs nothing: no frames, no deadline.
#[test]
fn rest_runs_nothing() {
    let mut i = island();
    assert_eq!(i.needs_frames(0.0), FrameNeed::None);
    assert_eq!(i.deadline(), None);
    assert!(!i.frame(0.0).visible);
    i.apply(line("n", "Dave's fix landed", NoticeKind::Info), 1.0);
    let mut t = 1.0;
    while let Some(d) = i.deadline() {
        t = d;
        i.apply(Event::Tick, t);
    }
    assert_eq!(i.needs_frames(t + 1.0), FrameNeed::None);
    assert!(!i.frame(t + 1.0).visible);
}

/// The shape is the notch at rest and grows out of it without a seam; no notch: a zero-height band.
#[test]
fn growth_starts_exactly_at_the_notch() {
    let mut i = island();
    i.apply(Event::Sent, 0.0);
    let f = i.frame(0.0);
    assert_eq!((f.shape.w, f.shape.h, f.shape.flare), (185.0, 32.0, 5.0));
    let mut j = Island::new(Display::PLAIN, Box::new(Approx));
    j.apply(Event::Sent, 0.0);
    assert_eq!(j.frame(0.0).shape.h, 0.0);
    assert_clean(&Scenario::new("no-notch-turn", "a turn without a notch", vec![ev(0.0, Event::Sent), ev(0.3, Event::Activity(Some(Activity::Thinking))), ev(3.0, Event::Activity(None))]).on(Display::PLAIN));
}

/// Every DESIGN.md class laid out is symmetric and its content fits, with real-ish and adversarial widths.
#[test]
fn every_gallery_scenario_passes_every_invariant() {
    for s in gallery::all() {
        assert_clean(&s.scenario);
        for w in [FuzzWidth::Zero, FuzzWidth::One, FuzzWidth::Max, FuzzWidth::MaxPlusOne, FuzzWidth::TenTimesMax] {
            let r = sim::run(&s.scenario, &Config { text: Text::Fuzz(w), ..Config::default() });
            assert!(r.ok(), "{} with {w:?} widths: {:#?}", s.scenario.id, r.findings);
        }
    }
}

// ---------------------------------------------------------------- generators

fn run_many(scenarios: impl Iterator<Item = Scenario>) -> Vec<sim::Report> {
    let mut out = vec![];
    for (n, s) in scenarios.enumerate() {
        let cfg = Config { fps: if n % 2 == 0 { 120.0 } else { 60.0 }, jitter: (n % 5 == 0).then_some(n as u64 + 1), ..Config::default() };
        let r = std::panic::catch_unwind(|| sim::run(&s, &cfg)).unwrap_or_else(|_| panic!("{} panicked: {:?}", s.id, s.steps));
        assert!(r.ok(), "{}: {:#?}\nsteps: {:#?}", s.id, r.findings, s.steps);
        if n % 100 == 0 {
            let again = sim::run(&s, &cfg);
            assert_eq!(r.hash, again.hash, "{} is not deterministic", s.id);
        }
        out.push(r);
    }
    out
}

/// Every ordered pair of events at every phase of the running motion (TOOLING.md §1.4, generator 2).
fn grid(phase: usize) {
    let n = sim::VOCABULARY;
    run_many((0..n).flat_map(move |a| (0..n).map(move |b| sim::pair(a, b, phase, (a * 31 + b * 7 + phase) as u64 * 0x9E37_79B9 + 1))));
}

#[test]
fn grid_pairs_same_instant() {
    grid(0);
}
#[test]
fn grid_pairs_one_frame_later() {
    grid(1);
}
#[test]
fn grid_pairs_at_a_quarter() {
    grid(2);
}
#[test]
fn grid_pairs_at_half() {
    grid(3);
}
#[test]
fn grid_pairs_at_ninety_percent() {
    grid(4);
}
#[test]
fn grid_pairs_settled() {
    grid(5);
}
#[test]
fn grid_pairs_at_rest() {
    grid(6);
}
#[test]
fn grid_pairs_during_the_exit() {
    grid(7);
}

/// A fixed stratified slice of the triples (the full million runs in the extended sweep).
#[test]
fn grid_triples_slice() {
    let n = sim::VOCABULARY;
    let mut k = 0u64;
    run_many((0..n).flat_map(move |a| {
        (0..n).step_by(5).map(move |b| {
            k = k.wrapping_add(1);
            let c = (a * 7 + b * 13) % n;
            sim::triple(a, b, c, (a + b) % sim::PHASES.len(), (a + c) % sim::PHASES.len(), k.wrapping_mul(0x2545_F491) + 3)
        })
    }));
}

fn random_range(range: std::ops::Range<u64>) {
    let extra: u64 = std::env::var("HEIMDALL_SIM_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
    let scale = (extra / 2000).max(1);
    let reports = run_many(range.flat_map(move |s| (0..scale).map(move |k| sim::random(s + k * 1_000_003))));
    assert!(!reports.is_empty());
}

#[test]
fn random_sequences_a() {
    random_range(0..500);
}
#[test]
fn random_sequences_b() {
    random_range(500..1000);
}
#[test]
fn random_sequences_c() {
    random_range(1000..1500);
}
#[test]
fn random_sequences_d() {
    random_range(1500..2000);
}

/// Every size class is reached across the corpus (I7's coverage half).
#[test]
fn every_class_is_reached() {
    let reports: Vec<sim::Report> = gallery::all().iter().map(|g| sim::run(&g.scenario, &Config::default())).collect();
    let cov = sim::coverage(&reports);
    for c in [Class::Rest, Class::Ears, Class::Lip, Class::Working, Class::Stack, Class::StackButton, Class::Row, Class::Lines, Class::Question, Class::Reply, Class::Reader, Class::Drawing] {
        assert!(cov.get(&c).copied().unwrap_or(0) > 0, "{c:?} never reached: {cov:?}");
    }
}

/// Speed: the model tier runs well over a thousand average sequences a second on one core (TOOLING.md §1.6).
#[test]
fn the_simulator_is_fast() {
    let started = std::time::Instant::now();
    let mut frames = 0;
    let n = 300;
    for s in 0..n {
        frames += sim::run(&sim::random(90_000 + s), &Config::default()).frames;
    }
    let per = started.elapsed().as_secs_f64() / n as f64;
    eprintln!("simulator: {:.3} ms per random sequence, {} frames each on average", per * 1000.0, frames / n as usize);
    assert!(per < 0.02, "{per} s per sequence");
}

use super::gallery;

/// Prints a frame listing of the grid cell named in `ISLAND_TRACE` ("a,b,phase", optional ",from,to"). Run with
/// `--ignored --nocapture` while diagnosing a finding.
#[test]
#[ignore]
fn trace_a_grid_cell() {
    let spec = std::env::var("ISLAND_TRACE").unwrap_or_else(|_| "30,2,0".into());
    let v: Vec<f64> = spec.split(',').filter_map(|x| x.parse().ok()).collect();
    let (a, b, p) = (v[0] as usize, v[1] as usize, v[2] as usize);
    let bits = (a * 31 + b * 7 + p) as u64 * 0x9E37_79B9 + 1;
    let s = sim::pair(a, b, p, bits);
    let (from, to) = (v.get(3).copied().unwrap_or(0.0), v.get(4).copied().unwrap_or(2.0));
    eprintln!("{:?}\n{}", s.steps, sim::trace(&s, 120.0, from, to));
}

/// Prints a frame listing of random sequence `ISLAND_SEED` from `ISLAND_FROM` to `ISLAND_TO`.
#[test]
#[ignore]
fn trace_a_random_sequence() {
    let seed: u64 = std::env::var("ISLAND_SEED").ok().and_then(|v| v.parse().ok()).unwrap_or(1);
    let from: f64 = std::env::var("ISLAND_FROM").ok().and_then(|v| v.parse().ok()).unwrap_or(0.0);
    let to: f64 = std::env::var("ISLAND_TO").ok().and_then(|v| v.parse().ok()).unwrap_or(2.0);
    let s = sim::random(seed);
    eprintln!("{}", sim::trace(&s, 120.0, from, to));
}

/// Prints a frame listing of the triple in `ISLAND_TRIPLE` ("a,b,c,pb,pc,bits,from,to").
#[test]
#[ignore]
fn trace_a_triple() {
    let v: Vec<f64> = std::env::var("ISLAND_TRIPLE").unwrap_or_default().split(',').filter_map(|x| x.parse().ok()).collect();
    let s = sim::triple(v[0] as usize, v[1] as usize, v[2] as usize, v[3] as usize, v[4] as usize, v[5] as u64);
    eprintln!("{:?}\n{}", s.steps, sim::trace(&s, 120.0, v[6], v[7]));
}

// ---------------------------------------------------------------- review fixes (18 Oct, REVIEW-impl.md)

/// Review MUST 1: speaking opened as an empty box and grew again when its words came. The look waits for the words
/// (up to 0.25 s) and opens once with them; a change that comes while the shape is still growing redirects that
/// growth instead of waiting for the next motion slot.
#[test]
fn speaking_opens_once_with_its_words() {
    let mut i = island();
    i.apply(Event::Speaking(true), 0.0);
    assert_eq!(i.class(), Class::Rest, "no box before the words");
    i.apply(Event::SpokenText(Some("The build is green.".into())), 0.12);
    assert_eq!(i.class(), Class::Row);
    assert_eq!(i.motion_starts().len(), 1, "one motion: {:?}", i.motion_starts());
    assert!(matches!(i.scene(), Scene::Speaking { sentences, .. } if !sentences.is_empty()), "{:?}", i.scene());
    // Words that never come: after the wait, the ears alone (never an empty row).
    let mut j = island();
    j.apply(Event::Speaking(true), 0.0);
    assert_eq!(j.deadline(), Some(super::scene::WORDS_WAIT));
    j.apply(Event::Tick, super::scene::WORDS_WAIT);
    assert_eq!(j.class(), Class::Ears);
    // A change while the shape is still growing redirects the growth at once.
    let mut k = island();
    k.apply(Event::Activity(Some(Activity::Thinking)), 0.0);
    k.apply(Event::Speaking(true), 0.08);
    k.apply(Event::SpokenText(Some("Done.".into())), 0.1);
    assert_eq!(k.target_size().1, 73.0, "redirected at 0.1 s, not at the next slot");
    assert_eq!(k.motion_starts().len(), 1, "a redirect is the same arrival");
    // The speaking cards never stand empty at full size (I15) nor show text over text (I17).
    for name in ["speaking", "speaking_with_line", "agent_turn"] {
        if let Some(c) = gallery::all().into_iter().find(|c| c.name == name) {
            assert_clean(&c.scenario);
        }
    }
}

/// Review MUST 2: settling into the ears, the shape undershot to 30–31 pt and the notch showed beneath it. While the
/// shape is at least as wide as the notch it is never shorter than the notch (I16 in the simulator).
#[test]
fn the_notch_never_shows_below_the_shape() {
    let mut i = island();
    i.apply(line("n1", "Island test complete", NoticeKind::Info), 0.0);
    i.apply(Event::Command(CommandCue::Heard { id: 1, kind: CommandKind::LookAtThis }), 1.0);
    let (nw, nh) = Display::MBP16.notch.unwrap();
    for k in 0..600 {
        let f = i.frame(k as f64 / 120.0);
        if f.visible && f.shape.w >= nw {
            assert!(f.shape.h >= nh - 1e-9, "{:.1}×{:.2} at {:.3}", f.shape.w, f.shape.h, f.t);
        }
    }
}

/// Review MUST 3: text over text (the status row over the question, "Tests passed" twice, "On it" half-blended
/// with "Checking build"). One content layer per place: what leaves clears before what arrives there (I17).
#[test]
fn no_text_is_drawn_over_text() {
    for name in ["question_under_status", "speaking_with_line", "agent_working", "question_one_at_a_time"] {
        let c = gallery::all().into_iter().find(|c| c.name == name).unwrap_or_else(|| panic!("card {name}"));
        assert_clean(&c.scenario);
    }
}

/// Review polish: the paused reader's play button turns into pause in place (the system's replace transition),
/// never a motion of the whole island.
#[test]
fn play_and_pause_swap_in_place() {
    let mut i = island();
    i.apply(Event::Reader(FastReadUpdate::Show(FastRead { id: "r".into(), text: "One two three four five six.".into(), title: Some("Build report".into()), wpm: 240 })), 0.0);
    settle(&mut i, 1.0);
    let before = i.motion_starts().len();
    i.apply(Event::Click(Target::ReaderPlay), 1.0);
    settle(&mut i, 1.5);
    i.apply(Event::Click(Target::ReaderPlay), 1.5);
    let icon = i.frame(1.6).slots.iter().find(|s| &*s.id == "reader.play.icon").map(|s| s.content.clone());
    assert!(matches!(icon.as_deref(), Some(Content::Symbol { name, .. }) if name == "play.fill"), "{icon:?}");
    assert!(i.motion_starts().len() - before <= 2, "play/pause are not motions: {:?}", i.motion_starts());
}

// ---------------------------------------------------------------- timing table (diagnostic)

/// Prints, per gallery card and per motion: when it starts, its target size, when the shape is within a point of
/// its target, when its content is readable (every target item at 90 % of its own opacity), and when the arrival
/// effects start. `cargo test -p ui --lib timing_table -- --ignored --nocapture`.
#[test]
#[ignore]
fn timing_table() {
    for c in gallery::all() {
        let frames = sim::frames(&c.scenario, 240.0, c.end, Box::new(Approx));
        println!("== {} ({:.2} s)", c.name, c.end);
        let mut i = 0;
        while i < frames.len() {
            let m = frames[i].motion;
            let mut j = i;
            while j + 1 < frames.len() && frames[j + 1].motion == m {
                j += 1;
            }
            let f0 = &frames[i];
            let target = f0.target;
            let near = frames[i..=j].iter().find(|f| (f.shape.w - target.0).abs() < 1.0 && (f.shape.h - target.1).abs() < 1.0).map(|f| f.t - m.1);
            let fx = frames[i..=j].iter().find(|f| f.slots.iter().any(|s| s.fx != Fx::None && s.arrived)).map(|f| f.t - m.1);
            let last = &frames[j];
            let ids: Vec<&Arc<str>> = last.slots.iter().filter(|s| !s.id.contains('~')).map(|s| &s.id).collect();
            let readable = frames[i..=j]
                .iter()
                .find(|f| ids.iter().all(|id| f.slots.iter().any(|s| &s.id == *id && s.opacity >= 0.9 * last.slots.iter().find(|x| &x.id == *id).map_or(1.0, |x| x.opacity))))
                .map(|f| f.t - m.1);
            let settled = frames[i..=j].iter().find(|f| !f.moving).map(|f| f.t - m.1);
            println!(
                "  m{:<3} @{:>6.3}  {:>3.0}×{:<3.0} → {:>3.0}×{:<3.0}  shape±1pt {:>6}  readable {:>6}  fx {:>6}  settled {:>6}  ids {:?}",
                m.0,
                m.1,
                f0.shape.w,
                f0.shape.h,
                target.0,
                target.1,
                near.map_or("-".into(), |x| format!("{x:.3}")),
                readable.map_or("-".into(), |x| format!("{x:.3}")),
                fx.map_or("-".into(), |x| format!("{x:.3}")),
                settled.map_or("-".into(), |x| format!("{x:.3}")),
                ids.iter().map(|s| s.to_string()).collect::<Vec<_>>()
            );
            i = j + 1;
        }
    }
}

#[test]
#[ignore]
fn debug_random_steps() {
    let seed: u64 = std::env::var("ISLAND_SEED").ok().and_then(|v| v.parse().ok()).unwrap_or(1);
    let from: f64 = std::env::var("ISLAND_FROM").ok().and_then(|v| v.parse().ok()).unwrap_or(0.0);
    let to: f64 = std::env::var("ISLAND_TO").ok().and_then(|v| v.parse().ok()).unwrap_or(2.0);
    let s = sim::random(seed);
    for (t, st) in &s.steps {
        if *t >= from - 5.0 && *t <= to {
            eprintln!("{t:.4} {st:?}");
        }
    }
    let fps: f64 = std::env::var("ISLAND_FPS").ok().and_then(|v| v.parse().ok()).unwrap_or(120.0);
    let jitter: Option<u64> = std::env::var("ISLAND_JITTER").ok().and_then(|v| v.parse().ok());
    let r = sim::run(&s, &Config { fps, jitter, ..Config::default() });
    eprintln!("{:#?}", r.findings);
}

#[test]
#[ignore]
fn trace_a_card() {
    let name = std::env::var("ISLAND_CARD").unwrap_or_default();
    let from: f64 = std::env::var("ISLAND_FROM").ok().and_then(|v| v.parse().ok()).unwrap_or(0.0);
    let to: f64 = std::env::var("ISLAND_TO").ok().and_then(|v| v.parse().ok()).unwrap_or(2.0);
    let c = gallery::all().into_iter().find(|c| c.name == name).expect("card");
    eprintln!("{}", sim::trace(&c.scenario, 120.0, from, to));
}

// ---------------------------------------------------------------- fix round 19 Oct (the user: simpler, faster animations; the
// reply's words stay and add up; the last reply pulls down)

/// Plays `steps` and returns every frame at 120 Hz up to `end`.
fn frames_of(steps: Vec<(f64, Step)>, end: f64) -> Vec<FrameModel> {
    sim::frames(&Scenario::new("t", "t", steps), 120.0, end, Box::new(Approx))
}

fn is_check(s: &Slot) -> bool {
    matches!(&*s.content, Content::Symbol { name, .. } if name.contains("checkmark"))
}

/// "after speaking, the success animation is not needed". A spoken turn ends by retracting: no check
/// mark after the voice, one motion from the speaking row to rest, at rest within the grace and one retraction. Both
/// orders of the turn's end and the audio's end.
#[test]
fn no_success_after_speech() {
    for turn_ends_first in [true, false] {
        let mut steps = vec![ev(0.0, Event::Sent), ev(0.2, Event::Activity(Some(Activity::Thinking)))];
        steps.extend(gallery_speech(1.0, &[("The build is green.", 1.0), ("I kept the old build as the rollback.", 1.6)]));
        let voice_end = 1.0 + 0.12 + 2.6;
        steps.push(ev(if turn_ends_first { 2.0 } else { voice_end + 0.1 }, Event::Activity(None)));
        let frames = frames_of(steps.clone(), voice_end + 3.0);
        let after: Vec<&FrameModel> = frames.iter().filter(|f| f.t >= voice_end).collect();
        for f in &after {
            assert!(!f.slots.iter().any(|s| is_check(s) && s.opacity > 0.05), "a check mark after the voice at {:.3}: {:?}", f.t, f.slots);
        }
        // One motion after the voice's end: the retraction (a turn still running holds the lip first).
        let motions: std::collections::BTreeSet<u64> = after.iter().map(|f| f.motion.0).collect();
        assert!(motions.len() <= 2, "{turn_ends_first}: motions after the voice {motions:?}");
        let rest = after.iter().find(|f| !f.visible).map(|f| f.t - voice_end).expect("rests");
        let limit = scene::GRACE + 0.6 + if turn_ends_first { 0.0 } else { 0.2 };
        assert!(rest <= limit, "{turn_ends_first}: at rest {rest:.3} s after the voice (limit {limit:.2})");
        assert_clean(&Scenario::new("no-success-after-speech", "a spoken turn ends by retracting", steps));
    }
}

/// The reply's speech as playback sends it (the gallery's helper, for tests).
fn gallery_speech(t: f64, sentences: &[(&str, f64)]) -> Vec<(f64, Step)> {
    let mut v = vec![ev(t, Event::Speaking(true))];
    let mut at = t + 0.12;
    for (text, seconds) in sentences {
        v.push(ev(at, Event::SpokenText(Some(text.to_string()))));
        at += seconds;
    }
    for k in 1..((at - t) / 0.05) as usize {
        v.push(ev(t + k as f64 * 0.05, Event::SpeechLevel(0.5)));
    }
    v.push(ev(at, Event::Speaking(false)));
    v
}

/// "it kind of changes the text from sentence to sentence … we don't disappear them until the speech is
/// done … we just add them". Each sentence stays where it appeared (never moves sideways, never fades) while later
/// ones are added below; a repeat of the last sentence is one; a gap inside the reply keeps them; a cut or a new reply
/// starts over; words a moment before their audio belong to it.
#[test]
fn sentences_accumulate_and_never_shrink_mid_speech() {
    let s = ["The build is green.", "Dave pushed the audio fix an hour ago, and I kept the old build as the rollback.", "Say the word."];
    let mut i = island();
    i.apply(Event::Speaking(true), 0.0);
    i.apply(Event::SpokenText(Some(s[0].into())), 0.1);
    i.apply(Event::SpokenText(Some(s[0].into())), 0.2);
    let mut seen: std::collections::HashMap<String, (f64, f64)> = Default::default();
    let mut t = 0.1;
    let plan = [(1.0, s[1]), (3.0, s[2])];
    let mut next = 0;
    while t < 5.0 {
        if next < plan.len() && t >= plan[next].0 {
            i.apply(Event::SpokenText(Some(plan[next].1.into())), plan[next].0);
            next += 1;
        }
        i.apply(Event::SpeechLevel(0.5), t);
        let f = i.frame(t);
        for sl in f.slots.iter().filter(|x| x.id.starts_with("voice.s") && !x.id.contains('~')) {
            if let Some((x, op)) = seen.get(&sl.id.to_string()) {
                if *op > 0.99 {
                    assert!(sl.opacity > 0.99, "{} faded mid-speech at {t:.3}", sl.id);
                    assert!((sl.rect.mid_x() - x).abs() < 0.5, "{} moved sideways at {t:.3}", sl.id);
                }
            }
            seen.insert(sl.id.to_string(), (sl.rect.mid_x(), sl.opacity));
        }
        assert!(!f.slots.iter().any(|x| x.id.starts_with("voice.s") && x.id.contains('~') && x.opacity > 0.05), "a sentence left mid-speech at {t:.3}");
        t += 1.0 / 120.0;
    }
    assert_eq!(i.spoken(), &s.map(String::from), "added, never replaced; the repeat is one");
    assert!(matches!(i.scene(), Scene::Speaking { sentences, .. } if sentences.len() == 3));
    // A gap inside the reply (the audio ended, the next job starts 1.5 s later): the words stay, the next is added.
    i.apply(Event::Speaking(false), 5.0);
    i.apply(Event::Speaking(true), 6.5);
    i.apply(Event::SpokenText(Some("One more thing.".into())), 6.6);
    assert_eq!(i.spoken().len(), 4);
    // A new reply later starts over; so does one after a cut.
    i.apply(Event::Speaking(false), 7.0);
    i.apply(Event::Speaking(true), 7.0 + scene::REPLY_JOIN + 0.1);
    i.apply(Event::SpokenText(Some("Next reply.".into())), 7.2 + scene::REPLY_JOIN);
    assert_eq!(i.spoken(), &["Next reply.".to_string()]);
    i.apply(Event::SpeechCut, 12.0);
    assert!(i.spoken().is_empty());
    // Words a moment before their audio belong to it.
    i.apply(Event::SpokenText(Some("Early.".into())), 30.0);
    i.apply(Event::Speaking(true), 30.2);
    assert_eq!(i.spoken(), &["Early.".to_string()]);
    // Under every invariant, including I20 (words only grow while spoken).
    let mut steps = gallery_speech(0.1, &[(s[0], 1.0), (s[0], 0.2), (s[1], 3.0), (s[2], 1.0)]);
    steps.push(ev(2.0, line("n", "Tests passed", NoticeKind::Success)));
    assert_clean(&Scenario::new("accumulate", "sentences add up, a line rides above them", steps));
}

/// A reply longer than the row: the row keeps the newest sentences within its lines (the oldest leaves at the top,
/// the rest glide up, nothing slides under anything), its height stays bounded, and the pull-down has every sentence.
#[test]
fn a_long_reply_keeps_the_newest_lines_and_the_pull_down_has_all() {
    let steps = gallery_speech(0.1, &gallery::LONG_REPLY);
    let r = assert_clean(&Scenario::new("long-reply", "eight sentences", steps.clone()));
    assert!(r.ok());
    let frames = frames_of(steps, 14.0);
    // A full row scrolls in one motion: the oldest line leaves with the column while the new one rides in from
    // below the edge at once (no second stage waiting for the glide).
    let mut scrolls = 0;
    for w in frames.windows(2) {
        let (a, b) = (&w[0], &w[1]);
        if a.motion != b.motion && b.slots.iter().any(|s| s.id.starts_with("voice.s") && s.id.contains('~')) {
            scrolls += 1;
            let at = b.motion.1 + 0.16;
            let f = frames.iter().find(|f| f.t >= at).expect("a frame");
            let newest = f.slots.iter().filter(|s| s.id.starts_with("voice.s") && !s.id.contains('~')).max_by(|x, y| x.rect.y.total_cmp(&y.rect.y)).expect("lines");
            assert!(newest.opacity > 0.4 && newest.clip, "{} at {:.3}: opacity {:.2}, clip {}", newest.id, f.t, newest.opacity, newest.clip);
        }
    }
    assert!(scrolls >= 2, "the long reply scrolls ({scrolls})");
    let max_h = frames.iter().map(|f| f.target.1).fold(0.0, f64::max);
    assert!(max_h <= 73.0 + 20.0 * (scene::SPEAK_LINES as f64 - 1.0) + 0.5, "the row grows to {max_h}");
    let mut i = island();
    for (t, st) in gallery_speech(0.1, &gallery::LONG_REPLY) {
        if let Step::Ev(e) = st {
            i.apply(e, t);
        }
    }
    settle(&mut i, 30.0);
    assert_eq!(i.class(), Class::Rest);
    let lines = match i.scene() {
        Scene::Rest => i.reply(),
        other => panic!("{other:?}"),
    };
    assert_eq!(lines.len(), 1, "the spoken reply as one paragraph");
    for (s, _) in gallery::LONG_REPLY {
        assert!(lines[0].contains(s), "{s}");
    }
}

/// "we should be also able to pull the dynamic island down and that should show the last response".
/// The notch at rest is the target (only under a real notch); the reply as written wins over the spoken sentences
/// and keeps its paragraphs; a long one scrolls inside half the screen and its scroll is clamped both ways; the lip
/// band pulls it up; while the voice plays the pulled-down reply's bars move, and they flatten when it ends.
#[test]
fn pull_down_shows_the_last_reply_whole_and_scrolls() {
    let mut i = island();
    assert!(i.targets().is_empty(), "no reply yet: the notch is not a target");
    let para: Vec<String> = (0..60).map(|k| if k % 4 == 3 { String::new() } else { format!("Paragraph {k} says something worth reading again later.") }).collect();
    i.apply(Event::SpokenText(Some("Spoken words.".into())), 0.0);
    i.apply(Event::LastReply(para.join("\n")), 0.1);
    settle(&mut i, 1.0);
    assert_eq!(i.class(), Class::Rest);
    let (target, rect) = i.targets().first().cloned().expect("the notch is the target at rest");
    assert_eq!(target, Target::Token(Token::Reply));
    assert_eq!(i.hit(1.0, rect.center()), Some(Target::Token(Token::Reply)));
    assert!(i.frame(1.0).slots.is_empty() && !i.frame(1.0).visible, "the target draws nothing");
    assert_eq!(i.needs_frames(1.0), FrameNeed::None);
    i.apply(Event::Click(Target::Token(Token::Reply)), 1.0);
    settle(&mut i, 2.0);
    assert_eq!(i.class(), Class::Reply);
    let written = para.join("\n").trim().split('\n').count();
    assert!(matches!(i.scene(), Scene::Reply { paragraphs, .. } if paragraphs.len() == written && paragraphs[0] == para[0]), "as written, not the spoken words: {:?}", i.scene());
    let (w, h) = i.target_size();
    assert!(h <= Display::MBP16.height / 2.0 + 1.0, "{h}");
    assert!(w <= 480.0 && w >= 285.0);
    let body = |i: &Island| {
        i.frame(10.0).slots.iter().find_map(|s| match &*s.content {
            c @ Content::Body { h, scroll, .. } => Some((c.body_full(), *h, *scroll)),
            _ => None,
        })
    };
    let (full, visible, scroll) = body(&i).expect("the reply's body");
    assert!(visible < full && scroll == 0.0, "{full} {visible} {scroll}");
    i.apply(Event::Scroll(1e9), 3.0);
    let (_, _, scroll) = body(&i).expect("body");
    assert!((scroll - (full - visible)).abs() < 1e-6, "scrolled to its end: {scroll}");
    i.apply(Event::Scroll(-40.0), 3.1);
    let (_, _, back) = body(&i).expect("body");
    assert!((back - (full - visible - 40.0)).abs() < 1e-6, "no dead zone past the end: {back}");
    i.apply(Event::Scroll(-1e9), 3.2);
    assert_eq!(body(&i).expect("body").2, 0.0);
    assert_eq!(i.motion_starts().len(), 1, "scrolling is not a motion: {:?}", i.motion_starts());
    let band = i.targets().iter().find(|(t, _)| *t == Target::ReplyBand).cloned().expect("band");
    assert!(band.1.w >= 28.0 && band.1.h >= 24.0);
    i.apply(Event::Click(Target::ReplyBand), 4.0);
    settle(&mut i, 5.0);
    assert_eq!(i.class(), Class::Rest);
    // Open while the voice plays: the bars move; they flatten when it ends and the reply stays until pulled up.
    i.apply(Event::Click(Target::Token(Token::Reply)), 6.0);
    i.apply(Event::Speaking(true), 6.1);
    i.apply(Event::SpokenText(Some("Live.".into())), 6.2);
    i.apply(Event::SpeechLevel(0.7), 6.3);
    assert!(matches!(i.scene(), Scene::Reply { flat: false, .. }), "{:?}", i.scene());
    i.apply(Event::Speaking(false), 7.0);
    settle(&mut i, 9.0);
    assert!(matches!(i.scene(), Scene::Reply { flat: true, .. }), "{:?}", i.scene());
    // No notch: the menu bar's middle is the apps', never a target.
    let mut j = Island::new(Display::PLAIN, Box::new(Approx));
    j.apply(Event::LastReply("Hello.".into()), 0.0);
    settle(&mut j, 1.0);
    assert!(j.targets().is_empty());
    // The whole flow under every invariant.
    let c = gallery::all().into_iter().find(|c| c.name == "reply_pull_down_long").expect("card");
    assert_clean(&c.scenario);
}

/// Review 19 Oct: the pull-down showed the previous turn's written reply while a new spoken reply played. A new
/// spoken reply drops a written reply sent before the previous one started (or long before); the final reply's own
/// written text, sent a moment before its words, stays.
#[test]
fn a_new_spoken_reply_never_shows_the_last_turns_written_reply() {
    let mut i = island();
    i.apply(Event::LastReply("The build is green.\nAll 222 tests passed.".into()), 0.0);
    i.apply(Event::Speaking(true), 0.8);
    i.apply(Event::SpokenText(Some("The build is green.".into())), 0.9);
    assert_eq!(i.reply(), ["The build is green.", "All 222 tests passed."], "the reply as written while it is said");
    i.apply(Event::Speaking(false), 4.0);
    settle(&mut i, 10.0);
    assert_eq!(i.reply(), ["The build is green.", "All 222 tests passed."], "kept after it is said");
    // The next turn (a new run: idle to thinking) speaks an update (no written reply yet): the pull-down follows the
    // voice.
    i.apply(Event::Activity(Some(Activity::Thinking)), 39.0);
    i.apply(Event::Speaking(true), 40.0);
    i.apply(Event::SpokenText(Some("Checking the logs.".into())), 40.1);
    assert_eq!(i.reply(), ["Checking the logs."]);
    i.apply(Event::Speaking(false), 41.0);
    // Its final reply: written first, its words a moment later (a new reply after the join gap).
    i.apply(Event::LastReply("Checking the logs.\nThe speak tool failed twice.".into()), 50.0);
    i.apply(Event::SpokenText(Some("The speak tool failed twice.".into())), 50.6);
    i.apply(Event::Speaking(true), 50.7);
    assert_eq!(i.reply(), ["Checking the logs.", "The speak tool failed twice."]);
    i.apply(Event::Speaking(false), 53.0);
    // A reply cut by the user, then a new update a few seconds later: the cut reply's written text goes too.
    i.apply(Event::LastReply("A long answer.".into()), 60.0);
    i.apply(Event::Speaking(true), 60.5);
    i.apply(Event::SpokenText(Some("A long answer.".into())), 60.6);
    i.apply(Event::SpeechCut, 61.0);
    i.apply(Event::Activity(None), 61.0);
    i.apply(Event::Activity(Some(Activity::Thinking)), 65.0);
    i.apply(Event::Speaking(true), 66.0);
    i.apply(Event::SpokenText(Some("Stopped.".into())), 66.1);
    assert_eq!(i.reply(), ["Stopped."]);
    // Speaking off: a written reply with no voice stays until a voice starts.
    let mut j = island();
    j.apply(Event::LastReply("Read me.".into()), 0.0);
    settle(&mut j, 100.0);
    assert_eq!(j.reply(), ["Read me."]);
}

/// Review 19 Oct: a very long reply's pull-down body passed the 16,384 px bitmap limit and drew nothing. It holds
/// what one bitmap can and ends with a pointer; scrolling reaches that last line.
#[test]
fn a_very_long_reply_pulls_down_within_one_bitmap() {
    let mut i = island();
    let para: Vec<String> = (0..1500).map(|k| format!("Line {k} of a very long report that keeps going.")).collect();
    i.apply(Event::LastReply(para.join("\n")), 0.0);
    settle(&mut i, 1.0);
    i.apply(Event::Click(Target::Token(Token::Reply)), 1.0);
    settle(&mut i, 2.0);
    i.apply(Event::Scroll(1e9), 2.0);
    settle(&mut i, 3.0);
    let body = i.frame(3.0).slots.iter().find_map(|s| match &*s.content {
        Content::Body { lines, line_h, h, scroll, .. } => Some((lines.clone(), *line_h, *h, *scroll)),
        _ => None,
    }).expect("the body shows");
    let (lines, line_h, h, scroll) = body;
    let scale = Display::MBP16.scale;
    assert_eq!(lines.len(), crate::island::object::layout::reply_max_lines(scale));
    assert!(lines.len() as f64 * line_h * scale <= crate::island::object::layout::MAX_RASTER_PX, "{} lines", lines.len());
    assert_eq!(lines[0].text, para[0]);
    assert_eq!(lines.last().map(|r| r.text.as_str()), Some(crate::island::object::layout::REPLY_CUT));
    assert!((scroll - (lines.last().expect("rows").y + line_h - h)).abs() < 1e-6, "scrolled to the end: {scroll}");
    // A reply that fits keeps every line, no pointer.
    let mut j = island();
    j.apply(Event::LastReply("Short.\nTwo lines.".into()), 0.0);
    j.apply(Event::Click(Target::Token(Token::Reply)), 0.5);
    settle(&mut j, 2.0);
    assert!(j.frame(2.0).slots.iter().any(|s| matches!(&*s.content, Content::Body { lines, .. } if lines.len() == 2)));
}

/// Apple's own springs measured on this Mac (macOS 27.0.1, SwiftUI `Spring`): smooth 0.5 s response, critically
/// damped, 90 % at 0.310 s; snappy 0.5 s / 0.85, 90 % at 0.254 s; bouncy 0.5 s / 0.7, 90 % at 0.210 s. The island's
/// grow spring reaches 90 % at 0.220 s and its retraction at 0.211 s; what made it feel slow was everything around
/// them. Budgets, every gallery card: an arrival's bounce starts with the landing (≤ 0.30 s after its motion, was
/// 0.48 s), a swap in place is readable by 0.20 s (was 0.27 s), a retraction stops asking for frames by 0.6 s
/// (was 0.78 s) and a growth by 0.7 s (was 0.82–0.91 s).
#[test]
fn every_motion_keeps_apples_pace() {
    assert!(spring::GROW.time_to(0.9) <= 0.255, "{}", spring::GROW.time_to(0.9));
    assert!(spring::CALM.time_to(0.9) <= 0.255);
    for c in gallery::all() {
        let frames = sim::frames(&c.scenario, 240.0, c.end, Box::new(Approx));
        let light = c.name.starts_with("drawing") || c.name.starts_with("pointing");
        let mut k = 0;
        while k < frames.len() {
            let m = frames[k].motion;
            let mut j = k;
            while j + 1 < frames.len() && frames[j + 1].motion == m {
                j += 1;
            }
            let run = &frames[k..=j];
            if m.1.is_finite() {
                let to_rest = run.last().is_some_and(|f| f.target == (185.0, 32.0) || f.target.1 == 0.0);
                if let Some(fx) = run.iter().find(|f| f.slots.iter().any(|s| s.fx != Fx::None && s.arrived)) {
                    let first = run.iter().find(|f| f.slots.iter().any(|s| s.fx != Fx::None)).map_or(m.1, |f| f.t);
                    assert!(fx.t - first.max(m.1) <= 0.30 + 1e-9, "{}: m{} bounce {:.3} s after its motion", c.name, m.0, fx.t - m.1);
                }
                if let Some(settled) = run.iter().find(|f| !f.moving) {
                    let limit = if to_rest { 0.6 } else { 0.7 };
                    if !light {
                        assert!(settled.t - m.1 <= limit + 1e-9, "{}: m{} asks for frames {:.3} s", c.name, m.0, settled.t - m.1);
                    }
                }
            }
            k = j + 1;
        }
    }
}

/// The pull-down's body scrolls in place: before, every wheel step started a motion and the body cross-faded into
/// itself (a flicker per scroll). A note arriving while the user has the reply open joins it, and the wheel scrolls both.
#[test]
fn a_note_scrolls_in_place() {
    let mut i = island();
    i.apply(Event::Notice { notice: gallery::table_note(), body: Some((344.0, 3000.0)) }, 0.0);
    settle(&mut i, 1.0);
    assert_eq!(i.class(), Class::Reply);
    let before = i.motion_starts().len();
    for k in 0..20 {
        i.apply(Event::Scroll(15.0), 1.0 + k as f64 * 0.016);
    }
    assert_eq!(i.motion_starts().len(), before, "scrolling started motions: {:?}", i.motion_starts());
    let f = i.frame(1.5);
    assert!(f.slots.iter().all(|s| !s.id.contains('~')), "nothing leaves while scrolling");
    assert!(matches!(i.scene(), Scene::Reply { scroll, .. } if *scroll > 200.0));
    let mut j = island();
    j.apply(Event::LastReply("A reply.".into()), 0.0);
    j.apply(Event::Click(Target::Token(Token::Reply)), 0.5);
    j.apply(Event::Notice { notice: gallery::table_note(), body: Some((344.0, 3000.0)) }, 1.0);
    settle(&mut j, 2.0);
    j.apply(Event::Scroll(30.0), 2.0);
    assert!(matches!(j.scene(), Scene::Reply { scroll, notes, .. } if *scroll == 30.0 && notes.len() == 1), "{:?}", j.scene());
}

/// A sentence being added shows line by line as the growing edge passes it: the shape never grows an empty band
/// below the words (the first render showed 0.1 s of a box with nothing in its new space).
#[test]
fn an_added_sentence_never_leaves_an_empty_band() {
    let c = gallery::all().into_iter().find(|c| c.name == "speaking_reply").expect("card");
    let frames = sim::frames(&c.scenario, 240.0, 5.7, Box::new(Approx));
    let mut worst: f64 = 0.0;
    for f in frames.iter().filter(|f| f.visible && f.t > 0.8) {
        let bottom = f.slots.iter().filter(|s| s.id.starts_with("voice.s") && s.opacity > 0.3).map(|s| s.rect.max_y()).fold(0.0, f64::max);
        if bottom > 0.0 {
            worst = worst.max(f.shape.h - bottom);
        }
    }
    // At rest the last line's bottom is 13 pt above the edge; while a sentence comes in, at most a line more.
    assert!(worst <= 13.0 + 20.0 + 1.0, "an empty band of {worst:.1} pt below the words");
}

// ---------------------------------------------------------------- sized to its content (bug note 2026-10-19T18-46-34)

use super::layout;

/// Every settled state of every gallery card: (card, class, target size, content width, content bottom).
fn gallery_states() -> Vec<(&'static str, Class, (f64, f64), f64, f64)> {
    let mut out = vec![];
    for card in gallery::all() {
        let s = &card.scenario;
        let mut i = Island::new(s.display.sanitized(), Box::new(Approx));
        i.set_wall_origin(0.0);
        let mut steps = s.steps.clone();
        steps.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut record = |i: &Island| {
            if i.class() == Class::Rest {
                return;
            }
            let (mut reach, mut bottom) = (0.0f64, 0.0f64);
            for (_, _, r) in i.items() {
                if r.w > 0.0 && r.h > 0.0 {
                    reach = reach.max(r.x.abs()).max(r.max_x().abs());
                    bottom = bottom.max(r.max_y());
                }
            }
            let row = (card.name, i.class(), i.target_size(), 2.0 * reach, bottom);
            if !out.contains(&row) {
                out.push(row);
            }
        };
        let mut k = 0;
        for (t, st) in &steps {
            while let Some(d) = i.deadline().filter(|d| d <= t) {
                i.apply(Event::Tick, d);
                record(&i);
            }
            match st {
                Step::Ev(e) => i.apply(e.clone(), *t),
                Step::ClickAny(n) => {
                    let ts = i.targets().to_vec();
                    if !ts.is_empty() {
                        i.apply(Event::Click(ts[n % ts.len()].0.clone()), *t);
                    }
                }
                Step::Crash => i = Island::new(s.display.sanitized(), Box::new(Approx)),
            }
            record(&i);
        }
        while let Some(d) = i.deadline().filter(|d| *d <= card.end + 5.0) {
            i.apply(Event::Tick, d);
            record(&i);
            k += 1;
            if k > 200 {
                break;
            }
        }
    }
    out
}

/// "the animation for you thinking is literally three dots. It doesn't need to make the dynamic
/// island thicker." No state of any gallery card is bigger than its content + padding (I21), and the minimal states
/// are the notch's own width: the dots, the sent arrow, a short line, working words.
#[test]
fn every_state_is_sized_to_its_content() {
    let d = Display::MBP16;
    for (name, class, (w, h), cw, bottom) in gallery_states() {
        let (nw, nh) = d.notch_size();
        if class != Class::Reader {
            assert!(w <= (cw + 2.0 * layout::PAD_MAX).ceil().max(nw) + 0.5, "{name} {class:?}: {w:.0} wide for {cw:.0} of content");
        }
        assert!(h <= (bottom + layout::PAD_BOTTOM_MAX).ceil().max(nh) + 0.5, "{name} {class:?}: {h:.0} deep for content down to {bottom:.0}");
        // Content that fits under the notch keeps the notch's width: nothing grows sideways for it.
        if matches!(class, Class::Lip | Class::Working | Class::Stack | Class::StackButton | Class::Question | Class::Lines) && cw + 2.0 * layout::PAD <= nw {
            assert_eq!(w, nw, "{name} {class:?}: {cw:.0} of content in a {w:.0} wide shape");
        }
    }
    let mut i = island();
    i.apply(Event::Activity(Some(Activity::Thinking)), 0.0);
    assert_eq!((i.scene().clone(), i.target_size()), (Scene::Lip(Lip::Dots), (185.0, 56.0)), "thinking: three dots under the notch");
    i.apply(line("l", "Looking", NoticeKind::Info), 0.5);
    settle(&mut i, 1.0);
    assert_eq!(i.target_size().0, 185.0, "a one-word line keeps the notch's width: {:?}", i.scene());
    // The same on a display without a notch (the band stands in for it).
    let mut p = Island::new(Display::PLAIN, Box::new(Approx));
    p.apply(Event::Activity(Some(Activity::Thinking)), 0.0);
    assert_eq!(p.target_size(), (185.0, 56.0));
}

/// Text grows the shape only as far as the text needs, and an ear pair still needs the ears' width.
#[test]
fn text_and_ears_size_the_shape() {
    let d = Display::MBP16;
    let m = Approx;
    let short = layout::layout(&Scene::Stack { icon: Glyph::new("eye"), words: "Looking".into(), button: None }, &d, &m);
    let long = layout::layout(&Scene::Stack { icon: Glyph::new("eye"), words: "Screen Recording is off for Heimdall".into(), button: None }, &d, &m);
    assert_eq!(short.w, 185.0);
    let words = m.width("Screen Recording is off for Heimdall", Font::Label).min(360.0);
    assert_eq!(long.w, (words + 2.0 * layout::PAD).ceil().max(185.0), "longer words: their width + 22 either side");
    assert!(long.w > short.w);
    let ears = layout::layout(&Scene::Ears { left: Ear::Glyph(Glyph::new("eye")), right: Ear::Glyph(Glyph::new("hourglass")), target: None }, &d, &m);
    assert_eq!((ears.w, ears.h), (285.0, 32.0), "the ear pair: the notch and two ears, inside the menu bar");
    let reader = layout::layout(&Scene::Reader { speed: 240, left: "0:42".into(), face: ReaderFace::Focus, playing: false, started: false, hot: false, status: None }, &d, &m);
    assert_eq!(reader.w, 2.0 * CHUNK_SIDE + 2.0 * layout::PAD, "the reader: its widest unit and the padding");
    // The pull-down with a note: as wide as its body or title needs (a narrow picture, a short text), a wide table
    // up to the screen-bound maximum, never past it.
    let turn = |bw: f64, title: &str| {
        let notes = vec![NoteView { id: "n".into(), rev: 1, icon: Glyph::new("photo"), title: title.into(), body: (bw, 80.0) }];
        layout::layout(&Scene::Reply { copied: false, paragraphs: vec![], notes, chat: Default::default(), follow: false, tail: false, ask: None, status: None, level: 0, flat: true, scroll: 0.0, hot: false, composer_empty: true, attached: vec![], working: false }, &d, &m)
    };
    assert_eq!(turn(344.0, "Disk").w, 344.0 + 2.0 * layout::PAD_MAX);
    assert!(turn(120.0, "Disk").w < 344.0 && turn(120.0, "Disk").w >= 285.0, "{}", turn(120.0, "Disk").w);
    assert_eq!(turn(f64::NAN, "Disk").w, 285.0, "an unknown body width never widens it past the ears");
    assert_eq!(turn(620.0, "Jobs").w, 620.0 + 2.0 * layout::PAD_MAX);
    assert_eq!(turn(5000.0, "Jobs").w, layout::reply_max_w(&d), "never wider than the screen allows");
    assert_eq!(layout::reply_max_w(&Display { width: 1280.0, height: 800.0, scale: 2.0, notch: Some((185.0, 32.0)) }), 700.0);
    assert_eq!(layout::reply_max_w(&Display { width: 760.0, height: 600.0, scale: 1.0, notch: None }), layout::MAX_W, "a small screen keeps the usual width");
    for w in [344.0, 120.0, 620.0, 5000.0] {
        assert_eq!(layout::oversize(&turn(w, "Disk"), &d), None, "{w}");
    }
    for l in [&short, &long, &ears, &reader] {
        assert_eq!(layout::oversize(l, &d), None, "{l:?}");
    }
}

/// I21 catches the bug as it was: the thinking dots in a 285 wide lip, a one-word line in a 285 wide stack.
#[test]
fn oversize_flags_the_old_wide_lip() {
    let d = Display::MBP16;
    let mut lip = layout::layout(&Scene::Lip(Lip::Dots), &d, &Approx);
    assert_eq!(layout::oversize(&lip, &d), None);
    lip.w = 285.0;
    assert!(layout::oversize(&lip, &d).is_some_and(|s| s.contains("285 wide")), "{:?}", layout::oversize(&lip, &d));
    let mut deep = layout::layout(&Scene::Lip(Lip::Dots), &d, &Approx);
    deep.h = 94.0;
    assert!(layout::oversize(&deep, &d).is_some(), "the dots in a 94 pt box");
    let mut stack = layout::layout(&Scene::Stack { icon: Glyph::new("eye"), words: "Looking".into(), button: None }, &d, &Approx);
    assert_eq!(layout::oversize(&stack, &d), None);
    stack.w = 285.0;
    assert!(layout::oversize(&stack, &d).is_some(), "a one-word line in a 285 wide stack");
}

/// Prints the size table (the report's before/after): `cargo test -p ui --lib size_table -- --ignored --nocapture`.
#[test]
#[ignore]
fn size_table() {
    for (name, class, (w, h), cw, bottom) in gallery_states() {
        println!("{name:<28} {:<12} {w:>5.0} x {h:>4.0}   content {cw:>5.0} x {bottom:>4.0}", format!("{class:?}"));
    }
}

// ---------------------------------------------------------------- night pass 1022

/// Bug note 2026-10-26T19-14-48 (reversing 2026-10-21T18-50-16): the conversation the user pulled down stays open through
/// his sends, the thinking and the next send; only a pull-down an agent's note opened pulls itself up on a send.
#[test]
fn a_send_keeps_owners_chat_open() {
    let mut i = island();
    i.apply(Event::LastReply("The build is green. All tests passed.".into()), 0.0);
    settle(&mut i, 1.0);
    i.apply(Event::Click(Target::Token(Token::Reply)), 1.0);
    settle(&mut i, 2.0);
    assert_eq!(i.class(), Class::Reply);
    i.apply(Event::Sent, 2.0);
    settle(&mut i, 2.5);
    assert!(i.reply_open(), "open through the send");
    i.apply(Event::Activity(Some(Activity::Thinking)), 2.0 + scene::SENT_SECONDS);
    settle(&mut i, 2.0 + scene::SENT_SECONDS + 1.0);
    assert!(i.reply_open() && i.class() == Class::Reply, "and through the thinking");
    i.apply(Event::Sent, 4.0);
    i.apply(Event::Activity(Some(Activity::Thinking)), 4.2);
    settle(&mut i, 5.5);
    assert!(i.reply_open() && i.class() == Class::Reply, "and a second send");
    // A note the agent opened: pulled up by a send.
    let mut j = island();
    j.apply(Event::Notice { notice: agent_note("d1"), body: Some((344.0, 104.0)) }, 0.0);
    settle(&mut j, 1.0);
    assert_eq!(j.class(), Class::Reply);
    j.apply(Event::Sent, 1.5);
    assert_eq!(j.scene(), &Scene::Lip(Lip::Sent));
    // The notch still opens the last turn (the run that answers starts the next turn).
    settle(&mut j, 3.0);
    j.apply(Event::Click(Target::Token(Token::Reply)), 3.0);
    settle(&mut j, 3.5);
    assert_eq!(j.class(), Class::Reply);
    // The whole flow under every invariant.
    let s = Scenario::new(
        "send_keeps_the_chat_open",
        "User's pulled-down turn stays open when his words are sent and while the agent thinks",
        vec![
            ev(0.0, Event::LastReply("The build is green.\n\nAll 222 tests passed.\n- one\n- two".into())),
            ev(0.5, Event::Click(Target::Token(Token::Reply))),
            ev(1.4, Event::Sent),
            ev(2.3, Event::Activity(Some(Activity::Thinking))),
            ev(4.0, Event::Activity(None)),
            ev(4.5, Event::LastReply("Done: the build is out.".into())),
        ],
    );
    assert_clean(&s);
}

/// Bug note 2026-10-25T23-17-01: beside a wide table the reply's lines stayed 420 pt wide in a wider column.
#[test]
fn a_reply_beside_a_wide_table_wraps_to_the_column() {
    let mut i = island();
    let mut n = gallery::jobs_note();
    n.id = "wide".into();
    i.apply(Event::Notice { notice: n, body: Some((700.0, 100.0)) }, 0.0);
    let long = "Grok models compared by price and context: the cheaper one is fine for summaries, the larger one for long documents and code, and above 200k tokens of context the price doubles, so keep prompts under that.";
    i.apply(Event::LastReply(long.into()), 0.1);
    settle(&mut i, 1.5);
    let lines = i.items().find(|(id, _, _)| &***id == "reply.body").map(|(_, c, _)| match &**c {
        Content::Body { lines, .. } => lines.clone(),
        _ => vec![],
    });
    let lines = lines.expect("the reply's body");
    let widest = lines.iter().map(|r| r.x + Approx.width(&r.text, r.font())).fold(0.0, f64::max);
    assert!(widest > layout::SPEAK_W + 20.0, "the reply wraps to the table's column: widest line {widest:.0} pt ({} lines)", lines.len());
    assert!(widest <= layout::reply_max_w(&Display::MBP16));
}

fn rows_of(i: &Island, t: f64, prefix: &str) -> Vec<(String, Rect, Font)> {
    let mut rows: Vec<(String, Rect, Font)> = i
        .frame(t)
        .slots
        .iter()
        .filter(|s| s.id.starts_with(prefix) && !s.id.contains('~'))
        .filter_map(|s| match &*s.content {
            Content::Text { lines, font, .. } => Some((lines.join(" "), s.rect, *font)),
            _ => None,
        })
        .collect();
    rows.sort_by(|a, b| a.1.y.total_cmp(&b.1.y));
    rows
}

/// Bug note 2026-10-21T19-21-01: a reply shown as island lines kept none of its paragraphs. The collapsed lines keep
/// its line breaks, a gap between paragraphs and its list items, left-aligned as a page, the answer bold; one
/// short paragraph stays a centred caption.
#[test]
fn island_lines_keep_paragraphs_and_lists() {
    let mut i = island();
    let text = "The build is green. Two tests were slow.\n\nWhat changed:\n- the island\n- the speakers store";
    i.apply(line("l1", text, NoticeKind::Info), 0.0);
    settle(&mut i, 1.0);
    assert_eq!(i.class(), Class::Lines);
    let rows = rows_of(&i, 1.0, "lines.");
    let texts: Vec<&str> = rows.iter().map(|r| r.0.as_str()).collect();
    assert_eq!(texts, ["The build is green.", "Two tests were slow.", "What changed:", "•\u{2002}the island", "•\u{2002}the speakers store"], "{rows:#?}");
    assert_eq!(rows[0].2, Font::Strong, "the answer is bold");
    assert!(rows[1..].iter().all(|r| r.2 == Font::Spoken));
    assert!((rows[1].1.y - rows[0].1.y - layout::LINE).abs() < 1e-6, "the same paragraph: one line apart");
    assert!((rows[2].1.y - rows[1].1.y - layout::LINE - prose::PARA_GAP).abs() < 1e-6, "a paragraph gap");
    assert!((rows[3].1.y - rows[2].1.y - layout::LINE - prose::ITEM_GAP).abs() < 1e-6, "a list starts a little apart");
    let left = rows[0].1.x;
    assert!(rows.iter().all(|r| (r.1.x - left).abs() < 1e-6), "a page: one left edge");
    // One sentence: a centred caption, no semibold.
    let mut j = island();
    j.apply(line("l2", "Your notes went to the iPhone worker and it will reply soon.", NoticeKind::Info), 0.0);
    settle(&mut j, 1.0);
    let rows = rows_of(&j, 1.0, "lines.");
    assert!(!rows.is_empty());
    assert!(rows.iter().all(|r| r.2 == Font::Spoken && (r.1.x + r.1.max_x()).abs() < 1.0), "{rows:#?}");
    // Long: kept to the row's lines, the last saying it goes on.
    let long: Vec<String> = (0..12).map(|k| format!("- item {k} that is long enough to read")).collect();
    let mut k = island();
    k.apply(line("l3", &long.join("\n"), NoticeKind::Info), 0.0);
    settle(&mut k, 1.0);
    let rows = rows_of(&k, 1.0, "lines.");
    assert_eq!(rows.len(), scene::SPEAK_LINES);
    assert!(rows.last().expect("rows").0.ends_with('…'));
    for (name, text) in [("lines_page", text), ("lines_list", &long.join("\n") as &str)] {
        assert_clean(&Scenario::new(name, "island lines as a page", vec![ev(0.0, line("l", text, NoticeKind::Info))]));
    }
}

/// The pulled-down reply: paragraph gaps (not a whole blank line), list items hung under their text, the answer
/// semibold, Markdown marks dropped.
#[test]
fn the_pulled_down_reply_keeps_paragraphs_and_lists() {
    let mut i = island();
    let long_item = "a list item long enough that it has to wrap onto a second line in the island's column of text";
    i.apply(Event::LastReply(format!("**Yes**, it is fixed. The lip shows now.\n\n## Changes\n1. {long_item}\n2. `short`\n\nThat is all.")), 0.0);
    i.apply(Event::Click(Target::Token(Token::Reply)), 0.5);
    settle(&mut i, 2.0);
    let rows = i.frame(2.0).slots.iter().find_map(|s| match &*s.content {
        Content::Body { lines, .. } if !lines.is_empty() => Some(lines.clone()),
        _ => None,
    }).expect("the reply's body");
    assert_eq!(rows[0].text, "Yes, it is fixed.");
    assert!(rows[0].strong);
    assert_eq!(rows[1].text, "The lip shows now.");
    assert!(!rows[1].strong);
    let heading = rows.iter().find(|r| r.text == "Changes").expect("heading");
    assert!(heading.strong);
    assert!((heading.y - rows[1].y - layout::LINE - prose::PARA_GAP).abs() < 1e-6);
    let first = rows.iter().position(|r| r.text.starts_with("1.\u{2002}")).expect("item 1");
    assert!(rows[first + 1].x > 0.0 && !rows[first + 1].text.starts_with("2."), "its second line hangs under its text: {rows:#?}");
    assert!(rows.iter().any(|r| r.text == "2.\u{2002}short"));
    let last = rows.last().expect("rows");
    assert_eq!(last.text, "That is all.");
}

/// The prose rules themselves (`prose::lay`), on the simulator's widths.
#[test]
fn prose_rules() {
    let lay = |s: &str| prose::lay(&Approx, &s.split('\n').map(str::to_string).collect::<Vec<_>>(), layout::SPEAK_W, layout::LINE);
    // One short sentence: nothing semibold, not a page.
    let p = lay("Done.");
    assert_eq!(p.rows.len(), 1);
    assert!(!p.rows[0].strong && !p.page);
    // An abbreviation or a decimal never ends the answer.
    let p = lay("Use a model, e.g. Haiku 5.5 for this. Then check.");
    assert_eq!(p.rows[0].text, "Use a model, e.g. Haiku 5.5 for this.");
    // A first line that is a whole sentence, with more after it, is the answer.
    let p = lay("It works.\n- one\n- two");
    assert!(p.rows[0].strong && p.page);
    // Runs of blank lines are one gap; leading and trailing blanks none.
    let p = lay("\n\nA b.\n\n\n\nC d.\n\n");
    assert_eq!(p.rows.len(), 2);
    assert_eq!(p.rows[0].y, 0.0);
    assert!((p.rows[1].y - layout::LINE - prose::PARA_GAP).abs() < 1e-6);
    assert!((p.height - (2.0 * layout::LINE + prose::PARA_GAP)).abs() < 1e-6);
    // "-5 degrees" and "2024." are not list items; "3) x" is.
    let p = lay("-5 degrees\n2024. Was a year\n3) x");
    assert_eq!(p.rows.iter().map(|r| r.text.as_str()).collect::<Vec<_>>(), ["-5 degrees", "2024. Was a year", "3.\u{2002}x"]);
    // Kept to n rows: the last ends with an ellipsis.
    let p = prose::keep(&Approx, lay("A b c.\nD e f.\nG h i."), 2, layout::SPEAK_W, layout::LINE);
    assert_eq!(p.rows.len(), 2);
    assert!(p.rows[1].text.ends_with('…'));
    // Never panics on odd text.
    for s in ["", "\n", "-", "- ", "1.", "#", "# ", "**", "😀. 😀 😀. x", ". . .", "a.b. c"] {
        let _ = lay(s);
    }
}

/// Bug note 2026-10-20T22-43-17 (option a): the lip was hidden behind the notch. Nothing draws at rest and only the
/// notch takes clicks; when the pointer reaches the notch (with a last turn to open) a lip shows just below its bottom
/// edge, and the notch and the lip open the last turn; when the pointer leaves, the lip goes and the area under the
/// notch takes no clicks again. With no last turn, the notch is not a target and nothing shows.
#[test]
fn the_peek_lip_shows_only_while_the_pointer_is_at_the_notch() {
    let (nw, nh) = Display::MBP16.notch_size();
    let below = Point::new(0.0, nh + layout::PEEK_BELOW / 2.0);
    let mut i = island();
    // No last turn: nothing to hover.
    assert!(i.targets().is_empty());
    i.apply(Event::LastReply("The build is green. All tests passed.".into()), 0.0);
    settle(&mut i, 1.0);
    assert_eq!(i.class(), Class::Rest);
    assert!(!i.frame(1.0).visible && i.frame(1.0).slots.is_empty(), "nothing draws at rest");
    assert_eq!(i.hit(1.0, below), None, "no invisible strip under the notch at rest");
    assert_eq!(i.hit(1.0, Point::new(0.0, nh / 2.0)), Some(Target::Token(Token::Reply)));
    // The pointer reaches the notch.
    i.apply(Event::Hover(Some(Target::Token(Token::Reply))), 1.0);
    settle(&mut i, 1.05);
    assert_eq!(i.hit(1.08, Point::new(0.0, nh + 2.0)), Some(Target::Token(Token::Reply)), "the lip takes the pointer while it grows");
    settle(&mut i, 2.0);
    assert_eq!(i.scene(), &Scene::Lip(Lip::Peek));
    let f = i.frame(2.0);
    assert!(f.visible && f.shape.h >= nh + layout::PEEK_BELOW - 0.5 && (f.shape.w - nw).abs() < 1.0, "a notch-wide lip below the notch: {:?}", f.shape);
    assert!(f.slots.iter().any(|s| matches!(*s.content, Content::Grabber { .. }) && s.rect.y >= nh), "its grabber sits below the notch's edge");
    assert_eq!(i.hit(2.0, below), Some(Target::Token(Token::Reply)), "the lip under the notch opens the turn");
    assert_eq!(i.hit(2.0, Point::new(0.0, nh / 2.0)), Some(Target::Token(Token::Reply)), "and the notch");
    assert_eq!(i.hit(2.0, Point::new(nw / 2.0 + 20.0, nh / 2.0)), None, "nothing beside it");
    // Every point the notch took at rest still opens the turn with the lip shown (no show-hide flicker at its edge).
    for x in [-nw / 2.0 + 8.5, nw / 2.0 - 8.5, 0.0] {
        for y in [0.5, nh - 4.5] {
            assert_eq!(i.hit(2.0, Point::new(x, y)), Some(Target::Token(Token::Reply)), "({x}, {y})");
        }
    }
    assert_eq!(i.needs_frames(2.0), FrameNeed::None, "no frames while it waits");
    // It leaves: the lip goes, no strip stays.
    i.apply(Event::Hover(None), 2.5);
    settle(&mut i, 4.0);
    assert_eq!(i.class(), Class::Rest);
    assert!(!i.frame(4.0).visible);
    assert_eq!(i.hit(4.0, below), None);
    // Back, then a click on the lip: the last turn opens.
    i.apply(Event::Hover(Some(Target::Token(Token::Reply))), 5.0);
    settle(&mut i, 6.0);
    i.apply(Event::Click(Target::Token(Token::Reply)), 6.0);
    settle(&mut i, 7.0);
    assert_eq!(i.class(), Class::Reply);
    // Pulled up: no lip left behind even though the last hover was the notch.
    i.apply(Event::Click(Target::ReplyBand), 8.0);
    settle(&mut i, 9.5);
    assert_eq!(i.class(), Class::Rest, "{:?}", i.scene());
    // Something else to say wins over the lip, and the lip comes back after it only while hovered.
    i.apply(Event::Hover(Some(Target::Token(Token::Reply))), 10.0);
    i.apply(Event::Sent, 10.1);
    settle(&mut i, 10.2);
    assert_eq!(i.scene(), &Scene::Lip(Lip::Sent));
    // No notch: never a lip.
    let mut j = Island::new(Display::PLAIN, Box::new(Approx));
    j.apply(Event::LastReply("Hello there. More.".into()), 0.0);
    j.apply(Event::Hover(Some(Target::Token(Token::Reply))), 0.5);
    settle(&mut j, 1.0);
    assert_eq!(j.class(), Class::Rest);
    // Under every invariant, with the pointer coming and going.
    assert_clean(&Scenario::new(
        "peek_lip",
        "the pointer reaches the notch, leaves, comes back and opens the last turn",
        vec![
            ev(0.0, Event::LastReply("Done. Two things changed.".into())),
            ev(1.0, Event::Hover(Some(Target::Token(Token::Reply)))),
            ev(1.6, Event::Hover(None)),
            ev(1.7, Event::Hover(Some(Target::Token(Token::Reply)))),
            ev(2.6, Event::Click(Target::Token(Token::Reply))),
            ev(4.0, Event::Click(Target::ReplyBand)),
        ],
    ));
}

/// Night 1022 review (5): the pointer left the lip without an exit event for the notch target; the host's catch-up
/// says `Pointer(false)`, and the lip goes with it instead of staying drawn at rest. The host then tells the target
/// under the pointer again, which brings it back while the pointer is still at the notch.
#[test]
fn the_peek_lip_goes_when_the_pointer_leaves_without_an_exit_event() {
    let mut i = island();
    i.apply(Event::LastReply("The build is green.".into()), 0.0);
    settle(&mut i, 1.0);
    i.apply(Event::Hover(Some(Target::Token(Token::Reply))), 1.0);
    i.apply(Event::Pointer(true), 1.1);
    settle(&mut i, 2.0);
    assert_eq!(i.scene(), &Scene::Lip(Lip::Peek));
    i.apply(Event::Pointer(false), 2.5);
    settle(&mut i, 4.0);
    assert_eq!(i.class(), Class::Rest, "{:?}", i.scene());
    assert!(!i.frame(4.0).visible, "nothing drawn at rest");
    i.apply(Event::Hover(Some(Target::Token(Token::Reply))), 5.0);
    settle(&mut i, 6.0);
    assert_eq!(i.scene(), &Scene::Lip(Lip::Peek));
}

/// Night 1022 review (4): a final reply skipped at the speaker (the user was talking) leaves the pull-down: no peek lip,
/// nothing to open. A withdrawal naming an older reply leaves a later one alone.
#[test]
fn a_withdrawn_reply_leaves_the_pull_down() {
    let mut i = island();
    i.apply(Event::LastReply("Want me to merge it?".into()), 0.0);
    settle(&mut i, 1.0);
    assert!(i.reply_available());
    i.apply(Event::WithdrawReply("Want me to merge it?\n".into()), 1.0);
    settle(&mut i, 2.0);
    assert!(!i.reply_available(), "neither said nor shown");
    i.apply(Event::Hover(Some(Target::Token(Token::Reply))), 2.0);
    settle(&mut i, 3.0);
    assert_eq!(i.class(), Class::Rest, "no peek lip: {:?}", i.scene());
    i.apply(Event::Hover(None), 3.0);
    i.apply(Event::LastReply("Merged.".into()), 4.0);
    i.apply(Event::WithdrawReply("Want me to merge it?".into()), 5.0);
    settle(&mut i, 6.0);
    assert!(i.reply_available(), "a later reply stays");
}

/// Bug note 22 Oct 15:13 ("you've responded twice and I cannot see your last response"): with Speaking off two final
/// replies came as island lines 7.5 s apart, and the second took the island while the first was still being read. A
/// line to read now keeps the island for its read time and the next waits its turn; the app sends the pull-down both
/// replies, newest last (`bridge::unread`). Seconds apart, 7.5 s apart and a minute apart.
#[test]
fn a_second_reply_waits_for_the_first_to_be_read() {
    const MISTRAL: &str = "I found it: it's the Mistral voice agent demo, with two people talking to Charlotte. I've started research on how it works. The worker will bring back the top ideas we could adopt.";
    const WEB: &str = "Right. I can fetch a page through the shell, but I have no proper search tool, so research always goes to a worker. Do you have a search key?";
    let lines = |i: &Island| match i.scene() {
        Scene::Lines { text } => Some(text.clone()),
        _ => None,
    };
    let first_read = read_seconds(NoticeKind::Info, MISTRAL);
    let second_read = read_seconds(NoticeKind::Info, WEB);
    for gap in [1.5, 7.5, 60.0] {
        let mut i = island();
        i.apply(line("final-reply:1:island-line", MISTRAL, NoticeKind::Info), 0.0);
        i.apply(Event::LastReply(MISTRAL.into()), 0.0);
        settle(&mut i, 0.5);
        assert_eq!(lines(&i).as_deref(), Some(MISTRAL), "gap {gap}: {:?}", i.scene());
        i.apply(line("final-reply:2:island-line", WEB, NoticeKind::Info), gap);
        i.apply(Event::LastReply(format!("{MISTRAL}\n\n{WEB}")), gap);
        settle(&mut i, gap + 0.3);
        if gap < first_read {
            assert_eq!(lines(&i).as_deref(), Some(MISTRAL), "gap {gap}: the first stays until it has been read");
            assert_eq!(i.deadline(), Some(first_read), "gap {gap}: it hands over at its read time");
            settle(&mut i, first_read);
            assert_eq!(lines(&i).as_deref(), Some(WEB), "gap {gap}: then the second, for its own read time");
            settle(&mut i, first_read + second_read - 0.05);
            assert_eq!(lines(&i).as_deref(), Some(WEB), "gap {gap}");
        } else {
            assert_eq!(lines(&i).as_deref(), Some(WEB), "gap {gap}: the first was read long ago");
        }
        let reply = i.reply().join("\n");
        assert!(reply.starts_with("I found it") && reply.ends_with("search key?"), "gap {gap}: both in the pull-down, newest last: {reply}");
        settle(&mut i, gap.max(first_read) + second_read + 1.0);
        assert_eq!(i.class(), Class::Rest, "gap {gap}: lines still clear themselves");
    }
    // A short status line still gives way after `MIN_LINE` (a flood shows the newest).
    let mut i = island();
    i.apply(line("s1", "Tests passed", NoticeKind::Success), 0.0);
    settle(&mut i, 0.2);
    i.apply(line("s2", "Pushed", NoticeKind::Success), 0.3);
    settle(&mut i, super::scene::MIN_LINE + 0.01);
    assert_eq!(i.line_ids(), ["s2"]);
    // Under the simulator's invariants, the replies a few seconds apart.
    let s = Scenario::new(
        "two-replies-in-quick-succession",
        "Two final replies as island lines 1.5 s apart, then a third right after (bug note 22 Oct 15:13).",
        vec![
            ev(0.1, line("r1", MISTRAL, NoticeKind::Info)),
            ev(0.1, Event::LastReply(MISTRAL.into())),
            ev(1.6, line("r2", WEB, NoticeKind::Info)),
            ev(1.6, Event::LastReply(format!("{MISTRAL}\n\n{WEB}"))),
            ev(2.0, line("r3", "Done. The research worker is running and reports back when it has the comparison.", NoticeKind::Info)),
        ],
    );
    assert_clean(&s);
}

/// Review of the 15:13 fix: a status line arriving while a reply waits its turn does not push that reply out (it would
/// never show), and a warning or error never waits behind a reply being read.
#[test]
fn a_waiting_reply_survives_a_status_line_and_an_error_does_not_wait_behind_it() {
    const FIRST: &str = "I found it: it's the Mistral voice agent demo, with two people talking to Charlotte. I've started research on how it works.";
    const SECOND: &str = "Right. I can fetch a page through the shell, but I have no proper search tool. Do you have a search key?";
    let first_read = read_seconds(NoticeKind::Info, FIRST);
    let mut i = island();
    i.apply(line("r1", FIRST, NoticeKind::Info), 0.0);
    settle(&mut i, 0.5);
    i.apply(line("r2", SECOND, NoticeKind::Info), 1.0);
    i.apply(line("t3", "Looking", NoticeKind::Info), 1.5);
    settle(&mut i, first_read + 0.01);
    assert_eq!(i.line_ids(), ["r2"], "the waiting reply shows next");

    let mut i = island();
    i.apply(line("r1", FIRST, NoticeKind::Info), 0.0);
    settle(&mut i, 0.5);
    i.apply(line("e1", "Microphone stopped", NoticeKind::Error), 0.6);
    settle(&mut i, super::scene::MIN_LINE + 0.01);
    assert_eq!(i.line_ids(), ["e1"], "an error shows after MIN_LINE, not the reply's read time");
}


