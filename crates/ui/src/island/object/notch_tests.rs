//! Night 1022, floating islands and the notch: the nudge (a floating island newly needs the user) and pulling the last
//! reply out of the pulled-down notch. State over time, back to rest, nothing moving at rest, and the simulator's
//! invariants on both.

use super::sim::{self, Config, Scenario, Step};
use super::*;

fn island() -> Island {
    Island::new(Display::MBP16, Box::new(Approx))
}

/// Applies every deadline up to `until` (what the host's one-shot timer does), then a tick at `until`.
fn run_to(i: &mut Island, until: f64) {
    while let Some(d) = i.deadline().filter(|d| *d <= until) {
        i.apply(Event::Tick, d);
    }
    i.apply(Event::Tick, until);
}

fn nudge_slot(f: &FrameModel) -> Option<&Slot> {
    f.slots.iter().find(|s| &*s.id == "lip.nudge")
}

fn is_nudge(i: &Island) -> bool {
    matches!(i.scene(), Scene::Lip(Lip::Nudge { .. }))
}

fn at_rest(i: &Island, t: f64) {
    let f = i.frame(t);
    assert!(!f.visible, "at rest at {t}: nothing drawn");
    assert!(!f.moving, "at rest at {t}: nothing moving");
    assert_eq!(i.needs_frames(t), FrameNeed::None, "at rest at {t}: no frames");
}

/// Counts how many times the nudge's scene starts between `from` and `to`, sampled every 10 ms.
fn nudges_seen(i: &mut Island, from: f64, to: f64) -> usize {
    let (mut n, mut was) = (0, false);
    let mut t = from;
    while t < to {
        run_to(i, t);
        let now = is_nudge(i);
        if now && !was {
            n += 1;
        }
        was = now;
        t += 0.01;
    }
    n
}

fn open_reply(i: &mut Island, t: f64, text: &str) {
    i.apply(Event::LastReply(text.into()), t);
    run_to(i, t + 1.0);
    i.apply(Event::Click(Target::Token(Token::Reply)), t + 1.0);
    run_to(i, t + 2.0);
    assert!(i.reply_open() && i.class() == Class::Reply, "the reply is pulled down");
}

#[test]
fn a_nudge_rings_the_bell_jiggles_and_returns_to_rest() {
    let mut i = island();
    at_rest(&i, 0.5);
    i.apply(Event::Nudge { question: false }, 1.0);
    assert!(is_nudge(&i));
    assert_eq!(i.needs_frames(1.0), FrameNeed::Moving);
    // The bell, orange, ringing (the system's wiggle) as the shape lands.
    run_to(&mut i, 1.3);
    let f = i.frame(1.3);
    let s = nudge_slot(&f).expect("the bell shows");
    assert!(f.visible && s.opacity > 0.9);
    assert_eq!(s.fx, Fx::Wiggle);
    match &*s.content {
        Content::Symbol { name, palette, .. } => {
            assert_eq!(name, "bell.fill");
            assert_eq!(palette, &vec![Rgba::ORANGE]);
        }
        other => panic!("not a symbol: {other:?}"),
    }
    // The jiggle: the pill swells past its target and comes back, smoothly, only for `NUDGE_WOBBLE`.
    let (tw, _) = i.target_size();
    let widest = (0..200).map(|k| 1.0 + k as f64 * 0.005).map(|t| i.frame(t).shape.w).fold(0.0, f64::max);
    assert!(widest > tw * 1.03, "it jiggles: widest {widest} vs target {tw}");
    let mut last = i.frame(1.0).shape.w;
    for k in 1..=120 {
        let w = i.frame(1.0 + k as f64 / 120.0).shape.w;
        assert!((w - last).abs() < 6.0, "smooth at frame {k}: {last} → {w}");
        last = w;
    }
    let held = i.frame(1.0 + NUDGE_SECONDS - 0.05);
    assert!((held.shape.w - tw).abs() < 0.2 && !held.moving, "still once jiggled, before it retracts");
    // Then it retracts and nothing draws, moves or waits.
    run_to(&mut i, 1.0 + NUDGE_SECONDS + 1.0);
    at_rest(&i, 1.0 + NUDGE_SECONDS + 1.0);
    assert_eq!(i.deadline(), None);
    assert!(!i.nudge_pending());
    for k in 0..50 {
        at_rest(&i, 3.5 + k as f64 * 0.37);
    }
}

#[test]
fn a_question_nudge_shows_the_blue_question_mark() {
    let mut i = island();
    i.apply(Event::Nudge { question: true }, 1.0);
    run_to(&mut i, 1.3);
    let f = i.frame(1.3);
    let s = nudge_slot(&f).expect("the question mark shows");
    assert_eq!(s.fx, Fx::Bounce);
    match &*s.content {
        Content::Symbol { name, palette, .. } => {
            assert_eq!(name, "questionmark.circle.fill");
            assert_eq!(palette, &vec![Rgba::WHITE, Rgba::BLUE]);
        }
        other => panic!("not a symbol: {other:?}"),
    }
    run_to(&mut i, 4.0);
    at_rest(&i, 4.0);
}

#[test]
fn a_burst_of_nudges_is_one_and_a_later_one_plays_again() {
    let mut i = island();
    for k in 0..6 {
        i.apply(Event::Nudge { question: k == 3 }, 1.0 + k as f64 * 0.15);
    }
    assert_eq!(nudges_seen(&mut i, 1.0, 1.0 + NUDGE_SECONDS + NUDGE_COALESCE), 1, "one nudge for the burst");
    // A burst arriving right after the nudge ended is still the same burst.
    let end = 1.0 + NUDGE_SECONDS;
    i.apply(Event::Nudge { question: false }, end + 0.5);
    assert!(!i.nudge_pending(), "within the coalescing window: dropped");
    let later = end + NUDGE_COALESCE + 0.5;
    i.apply(Event::Nudge { question: false }, later);
    assert_eq!(nudges_seen(&mut i, later, later + 3.0), 1, "a later island plays its own nudge");
    run_to(&mut i, later + 4.0);
    at_rest(&i, later + 4.0);
}

#[test]
fn a_nudge_waits_while_the_reply_is_pulled_down_and_plays_once_it_goes_up() {
    let mut i = island();
    open_reply(&mut i, 0.0, "The build is green.\n\nKuba pushed the audio fix.");
    i.apply(Event::Nudge { question: false }, 2.5);
    i.apply(Event::Nudge { question: true }, 2.6);
    run_to(&mut i, 6.0);
    assert_eq!(i.class(), Class::Reply, "the pull-down is never interrupted");
    assert!(i.nudge_pending(), "one waits");
    i.apply(Event::Click(Target::ReplyBand), 6.0);
    assert!(is_nudge(&i), "it plays as soon as the pull-down goes up");
    assert!(matches!(i.scene(), Scene::Lip(Lip::Nudge { question: true })), "a question in the burst wins");
    run_to(&mut i, 9.0);
    at_rest(&i, 9.0);
}

#[test]
fn a_nudge_waits_for_the_voice_and_one_that_waited_too_long_is_dropped() {
    let mut i = island();
    i.apply(Event::Speaking(true), 0.0);
    i.apply(Event::SpokenText(Some("Here is what happened overnight.".into())), 0.05);
    i.apply(Event::Nudge { question: false }, 0.5);
    run_to(&mut i, 1.5);
    assert!(matches!(i.scene(), Scene::Speaking { .. }), "the voice keeps the notch");
    i.apply(Event::Speaking(false), 2.0);
    let seen = nudges_seen(&mut i, 2.0, 5.0);
    assert_eq!(seen, 1, "it plays after the voice's grace");
    run_to(&mut i, 6.0);
    at_rest(&i, 6.0);

    // Held behind the pull-down for longer than `NUDGE_QUEUE`: stale, dropped.
    let mut i = island();
    open_reply(&mut i, 0.0, "Done.");
    i.apply(Event::Nudge { question: false }, 2.0);
    run_to(&mut i, 2.0 + NUDGE_QUEUE + 1.0);
    i.apply(Event::Click(Target::ReplyBand), 2.0 + NUDGE_QUEUE + 1.0);
    assert!(!is_nudge(&i));
    assert!(!i.nudge_pending());
}

#[test]
fn dragging_the_reply_down_stretches_it_and_springs_back_short_of_the_threshold() {
    let mut i = island();
    open_reply(&mut i, 0.0, "The build is green.");
    let (_, th) = i.target_size();
    // Held: a rubber band (1:1 at first, ever less), live with the pointer.
    i.apply(Event::Pull(Some(20.0)), 3.0);
    assert!(i.frame(3.0).shape.h - th < 0.01, "eased, never a step");
    // The follow is critically damped (scene.rs `stretch`), not exponential: it starts at rest, so the first frames
    // move a few points, never the exponential's 23 pt step (I4, random-1965). It is about half-way after 0.08 s,
    // within a tenth of the reach after 0.2 s, and it never passes the reach on the way.
    let reach = rubber(20.0);
    assert!(i.frame(3.08).shape.h - th > 0.3 * reach && i.frame(3.08).shape.h - th < reach, "follows the pointer quickly");
    assert!((i.frame(3.2).shape.h - th - reach).abs() < 0.1 * reach, "within a tenth of the reach after 0.2 s");
    let mut last = 0.0;
    for k in 0..=24 {
        let v = i.frame(3.0 + k as f64 / 120.0).shape.h - th;
        assert!(v >= last - 1e-9 && v <= reach + 1e-9, "rises to the reach without overshoot: {v} at step {k}");
        last = v;
    }
    assert!(reach > 9.0 && reach < 20.0);
    i.apply(Event::Pull(Some(400.0)), 3.2);
    assert!(rubber(400.0) < 160.0 && rubber(400.0) > rubber(PULL_OUT));
    assert_eq!(i.needs_frames(3.2), FrameNeed::Moving);
    // Dragged back up short of the threshold and let go: no pull-out, it springs back and stays open.
    i.apply(Event::Pull(Some(PULL_OUT - 5.0)), 3.2);
    i.apply(Event::Pull(None), 3.3);
    assert!(i.take_outputs().iter().all(|o| !matches!(o, Output::PullOut { .. })));
    assert!(i.reply_open());
    let mut last = i.frame(3.3).shape.h;
    for k in 1..60 {
        let h = i.frame(3.3 + k as f64 / 120.0).shape.h;
        assert!(h <= last + 1e-9, "springs back without overshoot");
        last = h;
    }
    run_to(&mut i, 4.5);
    assert!(i.stretch(4.5) == 0.0 && (i.frame(4.5).shape.h - th).abs() < 0.01);
    assert_eq!(i.needs_frames(4.5), FrameNeed::None, "still again once it has sprung back");
}

#[test]
fn dragging_the_reply_past_the_threshold_pulls_it_out_and_the_panel_retracts() {
    let mut i = island();
    let text = "**The build is green.**\n\n- Dave pushed the audio fix\n- tests pass";
    open_reply(&mut i, 0.0, text);
    i.apply(Event::Pull(Some(PULL_OUT + 1.0)), 3.0);
    let stretched = i.frame(3.05).shape.h;
    // One time constant in (scene.rs `stretch`, critically damped, 1 - (1 + x)e^-x): about a quarter of the way to the
    // reach, and the exponential it replaced was about two thirds (its first slope is what stepped I4).
    let to = rubber(PULL_OUT + 1.0);
    let pulled = stretched - i.target_size().1;
    assert!(pulled > 0.2 * to && pulled < to, "stretched toward its reach while held: {pulled} of {to}");
    i.apply(Event::Pull(None), 3.05);
    assert_eq!(i.take_outputs(), vec![Output::PullOut { text: text.into(), notes: vec![] }]);
    assert!(!i.reply_open(), "the panel retracts");
    // The retraction starts from the stretched shape (no jump), then nothing draws.
    let after = i.frame(3.05).shape.h;
    assert!((after - stretched).abs() < 2.0, "the retraction starts where the stretch was: {stretched} then {after}");
    run_to(&mut i, 5.0);
    at_rest(&i, 5.0);
    // The notch still opens the last turn afterwards.
    assert!(i.reply_available());
}

#[test]
fn the_pin_pulls_the_reply_out_and_drags_elsewhere_do_nothing() {
    let mut i = island();
    // At rest a drag is not a pull.
    i.apply(Event::Pull(Some(200.0)), 0.1);
    i.apply(Event::Pull(None), 0.2);
    assert!(i.take_outputs().is_empty());
    assert_eq!(i.stretch(0.2), 0.0);
    open_reply(&mut i, 1.0, "Kept for later.");
    let pin = i.targets().iter().find(|(t, _)| *t == Target::ReplyPin).cloned().expect("the pin, pulled down");
    let band = i.targets().iter().find(|(t, _)| *t == Target::ReplyBand).cloned().expect("the band");
    // The pin sits in the band's right corner, inside the shape, ahead of the band for clicks.
    assert!(pin.1.x > 0.0 && pin.1.x + pin.1.w <= i.target_size().0 / 2.0);
    let c = Point::new(pin.1.x + pin.1.w / 2.0, pin.1.y + pin.1.h / 2.0);
    assert!(band.1.contains(c));
    assert_eq!(i.hit(4.0, c), Some(Target::ReplyPin));
    i.apply(Event::Click(Target::ReplyPin), 4.0);
    assert_eq!(i.take_outputs(), vec![Output::PullOut { text: "Kept for later.".into(), notes: vec![] }]);
    assert!(!i.reply_open());
    run_to(&mut i, 6.0);
    at_rest(&i, 6.0);
    // Only in the pulled-down state.
    assert!(i.targets().iter().all(|(t, _)| *t != Target::ReplyPin));
}

#[test]
fn a_send_while_dragging_lets_go_without_pulling_out() {
    let mut i = island();
    open_reply(&mut i, 0.0, "The build is green.");
    i.apply(Event::Pull(Some(PULL_OUT + 30.0)), 3.0);
    i.apply(Event::Sent, 3.1);
    i.apply(Event::Pull(None), 3.2);
    assert!(i.take_outputs().iter().all(|o| !matches!(o, Output::PullOut { .. })));
    run_to(&mut i, 6.0);
    // The user's pull-down stays open through his send (bug note 2026-10-26T19-14-48), its stretch sprung back.
    assert!(i.reply_open() && i.class() == Class::Reply);
    assert_eq!(i.stretch(6.0), 0.0);
}

/// A drag as the pointer delivers it: 4 points every 1/120 s down to `to`, then the release.
fn drag(t0: f64, to: f64) -> Vec<(f64, Step)> {
    let n = (to / 4.0).ceil() as usize;
    let mut v: Vec<(f64, Step)> = (1..=n).map(|k| (t0 + k as f64 / 120.0, Step::Ev(Event::Pull(Some((k as f64 * 4.0).min(to)))))).collect();
    v.push((t0 + (n + 1) as f64 / 120.0, Step::Ev(Event::Pull(None))));
    v
}

#[test]
fn nudges_and_pulls_hold_every_invariant() {
    let reply = "The build is green.\n\nKuba pushed the audio fix an hour ago.";
    let scenarios = [
        Scenario::new("nudge-alone", "A bell nudge at rest.", vec![(0.5, Step::Ev(Event::Nudge { question: false }))]),
        Scenario::new(
            "nudge-burst",
            "A burst of nudges, one a question, then a later one.",
            vec![
                (0.5, Step::Ev(Event::Nudge { question: false })),
                (0.6, Step::Ev(Event::Nudge { question: true })),
                (0.9, Step::Ev(Event::Nudge { question: false })),
                (6.0, Step::Ev(Event::Nudge { question: true })),
            ],
        ),
        Scenario::new(
            "nudge-interrupted",
            "A nudge mid-jiggle, then the voice and a line arrive.",
            vec![
                (0.5, Step::Ev(Event::Nudge { question: false })),
                (0.75, Step::Ev(Event::Speaking(true))),
                (0.8, Step::Ev(Event::SpokenText(Some("One thing first.".into())))),
                (1.5, Step::Ev(Event::Speaking(false))),
            ],
        ),
        Scenario::new(
            "pull-out",
            "The reply pulled down, dragged, sprung back, dragged past the threshold.",
            vec![
                (0.2, Step::Ev(Event::LastReply(reply.into()))),
                (1.0, Step::Ev(Event::Click(Target::Token(Token::Reply)))),
            ]
            .into_iter()
            .chain(drag(2.0, 40.0))
            .chain([(2.4, Step::Ev(Event::Nudge { question: true }))])
            .chain(drag(3.0, 90.0))
            .collect(),
        ),
    ];
    for s in &scenarios {
        for cfg in [Config::default(), Config { fps: 60.0, ..Config::default() }, Config { jitter: Some(7), ..Config::default() }] {
            let r = sim::run(s, &cfg);
            assert!(r.ok(), "{}: {:#?}", s.id, r.findings);
        }
    }
    let r = sim::run(&scenarios[3], &Config::default());
    assert_eq!(r.outputs.iter().filter(|o| matches!(o, Output::PullOut { .. })).count(), 1);
}

// ---------------------------------------------------------------- 23 Oct: notes pull out, the conversation

fn table_note(id: &str, title: &str) -> Event {
    let mut n = crate::agent_notice::AgentNotice::line(id, title, crate::agent_notice::NoticeKind::Info);
    n.detail = Some(crate::agent_notice::Detail::Table { headers: vec!["A".into()], rows: vec![vec!["1".into()]] });
    Event::Notice { notice: n, body: Some((200.0, 48.0)) }
}

fn tap(i: &mut Island, target: &Target, t: f64) {
    let r = i.targets().iter().find(|(x, _)| x == target).map(|(_, r)| *r).unwrap_or_else(|| panic!("{target:?} among {:?}", i.targets()));
    assert_eq!(i.hit(t, r.center()).as_ref(), Some(target), "hit-testing finds {target:?}");
    i.apply(Event::Click(target.clone()), t);
}

/// "when you're giving me a table inside of it, I cannot actually pull the table out … even when I
/// click the pin on the bottom right … I can only get your last reply".
#[test]
fn the_pin_takes_the_turns_table_with_its_reply_and_a_notes_own_pin_takes_it_alone() {
    let mut i = island();
    i.apply(table_note("notice-t", "Memory"), 0.1);
    open_reply(&mut i, 0.2, "About 61 GB in use.");
    let _ = i.take_outputs();
    tap(&mut i, &Target::ReplyPin, 3.0);
    assert_eq!(i.take_outputs(), vec![Output::PullOut { text: "About 61 GB in use.".into(), notes: vec!["notice-t".into()] }]);
    assert!(!i.reply_open());
    // A note's own pin, beside its close X: that note alone.
    run_to(&mut i, 4.0);
    i.apply(Event::Click(Target::Token(Token::Reply)), 4.0);
    run_to(&mut i, 5.0);
    let pin = i.targets().iter().find(|(t, _)| *t == Target::NotePin("notice-t".into())).map(|(_, r)| *r).expect("the note's pin");
    let close = i.targets().iter().find(|(t, _)| *t == Target::NoteClose("notice-t".into())).map(|(_, r)| *r).expect("its close X");
    assert!(pin.max_x() <= close.x + 1e-9 && (pin.center().y - close.center().y).abs() < 1e-9, "left of the X, on its row");
    tap(&mut i, &Target::NotePin("notice-t".into()), 5.0);
    assert_eq!(i.take_outputs(), vec![Output::PullOut { text: String::new(), notes: vec!["notice-t".into()] }]);
}

#[test]
fn a_turn_of_a_table_alone_can_be_pinned_and_dragged_out() {
    let mut i = island();
    i.apply(table_note("notice-t", "Memory"), 0.1);
    run_to(&mut i, 1.0);
    assert!(i.targets().iter().any(|(t, _)| *t == Target::ReplyPin), "the pin shows for a note without words");
    i.apply(Event::Pull(Some(PULL_OUT + 5.0)), 1.0);
    i.apply(Event::Pull(None), 1.1);
    assert_eq!(i.take_outputs(), vec![Output::PullOut { text: String::new(), notes: vec!["notice-t".into()] }]);
}

fn day() -> Vec<crate::chat::ChatEntry> {
    super::gallery::chat_day()
}

#[test]
fn owner_pulls_down_the_conversation_opening_on_his_last_words_and_scrolls_up_to_earlier_turns() {
    let mut i = island();
    i.apply(Event::Chat(day()), 0.0);
    // The conversation alone never shows anything: the notch rests until the user pulls it.
    run_to(&mut i, 0.5);
    at_rest(&i, 0.5);
    i.apply(Event::Notice { notice: super::gallery::memory_note(), body: Some((300.0, 126.0)) }, 0.55);
    i.apply(Event::LastReply(super::gallery::CHAT_REPLY.into()), 0.6);
    i.apply(Event::Click(Target::ReplyBand), 0.8);
    run_to(&mut i, 1.0);
    i.apply(Event::Click(Target::Token(Token::Reply)), 1.0);
    run_to(&mut i, 2.0);
    let Scene::Reply { chat, follow, .. } = i.scene().clone() else { panic!("{:?}", i.scene()) };
    assert!(follow);
    assert_eq!(chat.before.last(), Some(&crate::chat::ChatKind::Owner("How much memory are the jobs using right now?".into())));
    assert!(chat.before.iter().all(|k| !matches!(k, crate::chat::ChatKind::Heimdall { text, .. } if text == super::gallery::CHAT_REPLY)), "the live reply shows once");
    let l = i.layout().clone();
    assert!(l.scroll_max > 0.0 && l.scroll_at > 0.0 && l.scroll_at <= l.scroll_max, "opened down at the latest exchange: {} of {}", l.scroll_at, l.scroll_max);
    assert!(l.h <= Display::MBP16.height / 2.0 + 1.0, "never past half the screen");
    // His first scroll starts from what he sees.
    i.apply(Event::Scroll(-50.0), 2.0);
    run_to(&mut i, 2.5);
    assert!((i.layout().scroll_at - (l.scroll_at - 50.0)).abs() < 1e-6);
    // At the top, the earlier notice's row pulls that notice out.
    i.apply(Event::Scroll(-1e6), 2.5);
    run_to(&mut i, 3.0);
    assert_eq!(i.layout().scroll_at, 0.0);
    let _ = i.take_outputs();
    tap(&mut i, &Target::ChatNote("notice-acks".into()), 3.0);
    assert_eq!(i.take_outputs(), vec![Output::PullOut { text: String::new(), notes: vec!["notice-acks".into()] }]);
}

/// Bug note 2026-10-26T19-14-21-3: "whenever I send a message and there is a new message, the island should
/// automatically scroll all the way down", unless he scrolled up on purpose.
#[test]
fn a_send_and_a_new_reply_scroll_to_the_end() {
    let mut i = island();
    let mut rows = day();
    i.apply(Event::Chat(rows.clone()), 0.0);
    i.apply(Event::LastReply(super::gallery::CHAT_REPLY.into()), 0.1);
    i.apply(Event::OpenChat, 0.5);
    run_to(&mut i, 1.5);
    assert!(i.layout().scroll_max > 100.0);
    // He scrolls up, then sends: all the way down.
    i.apply(Event::Scroll(-120.0), 1.5);
    run_to(&mut i, 2.0);
    i.apply(Event::ComposerSent("And the GPU?".into()), 2.0);
    i.apply(Event::Sent, 2.05);
    let at = rows.last().map_or(0, |e| e.at_ms) + 1000;
    rows.push(crate::chat::ChatEntry { at_ms: at, kind: crate::chat::ChatKind::Owner("And the GPU?".into()) });
    i.apply(Event::Chat(rows.clone()), 2.1);
    run_to(&mut i, 3.0);
    assert!(i.reply_open());
    let l = i.layout().clone();
    assert!(l.scroll_max > 0.0 && (l.scroll_at - l.scroll_max).abs() < 1e-6, "a send scrolls to the end: {} of {}", l.scroll_at, l.scroll_max);
    // A new reply: still at the end.
    let long = (1..=14).map(|k| format!("{k}. The GPU runs the decider and the voice: about 3 GB each, idle between calls.")).collect::<Vec<_>>().join("\n");
    i.apply(Event::LastReply(long), 3.0);
    run_to(&mut i, 4.0);
    let l = i.layout().clone();
    assert!((l.scroll_at - l.scroll_max).abs() < 1e-6, "a new reply follows: {} of {}", l.scroll_at, l.scroll_max);
    // Scrolled up on purpose, a new message leaves him where he is.
    i.apply(Event::Scroll(-200.0), 4.0);
    run_to(&mut i, 4.5);
    let held = i.layout().scroll_at;
    rows.push(crate::chat::ChatEntry { at_ms: at + 1000, kind: crate::chat::ChatKind::Action { kind: crate::chat::ActionKind::Started, text: "Checking the GPU".into(), answer: None } });
    i.apply(Event::Chat(rows.clone()), 4.5);
    run_to(&mut i, 5.5);
    assert!((i.layout().scroll_at - held).abs() < 1e-6, "scrolled up on purpose: stays ({held} → {})", i.layout().scroll_at);
}

#[test]
fn a_pull_down_a_note_opened_stays_the_notes_and_the_conversation_alone_still_opens_from_the_notch() {
    let mut i = island();
    i.apply(Event::Chat(day()), 0.0);
    i.apply(table_note("notice-t", "Memory"), 0.1);
    run_to(&mut i, 1.0);
    assert!(matches!(i.scene(), Scene::Reply { chat, .. } if chat.before.is_empty() && chat.after.is_empty()), "the agent's note opens compact");
    // After a restart: no turn, only the conversation; the notch at rest is its target and opens it at its end.
    let mut j = island();
    j.apply(Event::Chat(day()), 0.0);
    run_to(&mut j, 0.5);
    assert!(j.reply_available());
    assert!(j.targets().iter().any(|(t, _)| *t == Target::Token(Token::Reply)));
    j.apply(Event::Click(Target::Token(Token::Reply)), 0.5);
    run_to(&mut j, 1.5);
    assert_eq!(j.class(), Class::Reply);
    assert_eq!(j.layout().scroll_at, j.layout().scroll_max, "newest at the bottom");
    assert!(j.targets().iter().all(|(t, _)| *t != Target::ReplyPin), "nothing of a turn to pin");
    // An empty conversation closes it.
    j.apply(Event::Chat(vec![]), 2.0);
    run_to(&mut j, 3.0);
    assert!(!j.reply_open());
}

#[test]
fn the_conversation_holds_every_invariant() {
    let s = Scenario::new(
        "chat",
        "The conversation pulled down, scrolled, a note and a reply arriving, a send.",
        vec![
            (0.05, Step::Ev(Event::Chat(day()))),
            (0.3, Step::Ev(Event::Click(Target::Token(Token::Reply)))),
            (1.2, Step::Ev(Event::Scroll(-120.0))),
            (1.6, Step::Ev(table_note("notice-t", "Memory"))),
            (2.0, Step::Ev(Event::LastReply("Here it is.".into()))),
            (2.4, Step::ClickAny(3)),
            (3.0, Step::Ev(Event::Click(Target::Token(Token::Reply)))),
            (3.5, Step::Ev(Event::Scroll(400.0))),
            (4.0, Step::Ev(Event::Sent)),
        ],
    );
    for cfg in [Config::default(), Config { fps: 60.0, ..Config::default() }, Config { jitter: Some(5), ..Config::default() }] {
        let r = sim::run(&s, &cfg);
        assert!(r.ok(), "{:#?}", r.findings);
    }
}

// ---------------------------------------------------------------- 23 Oct 22:07: a question while Heimdall speaks

fn ask(id: &str, text: &str) -> Event {
    let mut n = crate::agent_notice::AgentNotice::line(id, text, crate::agent_notice::NoticeKind::Info);
    n.action = Some(crate::agent_notice::NoticeAction::single(id, text, &["Publish", "Hold draft"], u64::MAX / 4));
    Event::Notice { notice: n, body: None }
}

/// "Whenever you're asking me a question and then you start talking, the only thing I see is the
/// question … I should see you talking, and the decision goes down".
#[test]
fn a_question_docks_under_the_voices_words_and_never_replaces_them() {
    let mut i = island();
    i.apply(ask("q1", "Publish Vella 2.1.1 now?"), 0.1);
    run_to(&mut i, 0.5);
    assert!(matches!(i.scene(), Scene::Question { speech: None, .. }), "alone, the card as before");
    i.apply(Event::Speaking(true), 0.5);
    i.apply(Event::SpokenText(Some("The signed draft is ready.".into())), 0.55);
    run_to(&mut i, 1.5);
    let Scene::Question { speech: Some(sp), .. } = i.scene().clone() else { panic!("{:?}", i.scene()) };
    assert_eq!(sp.sentences, vec!["The signed draft is ready.".to_string()]);
    let f = i.frame(1.5);
    let words = f.slots.iter().find(|s| s.id.starts_with("voice.s")).expect("his words").rect;
    let q = f.slots.iter().find(|s| &*s.id == "q.under.q").expect("the question").rect;
    assert!(words.max_y() <= q.y, "the words on top, the question below: {words:?} {q:?}");
    // Live text: the next sentence is added; the card stays clickable throughout.
    i.apply(Event::SpokenText(Some("Say the word and it goes out.".into())), 1.6);
    run_to(&mut i, 2.5);
    assert!(matches!(i.scene(), Scene::Question { speech: Some(sp), .. } if sp.sentences.len() == 2));
    let _ = i.take_outputs();
    tap(&mut i, &Target::AskPick { id: "q1".into(), question: 0, option: 1 }, 2.5);
    assert!(i.take_outputs().iter().any(|o| matches!(o, Output::Notice(crate::agent_notice::AgentNoticeAction::Answer { answers, .. }) if answers.first().is_some_and(|a| a.labels == ["Hold draft"]))));
    // Answered, the voice goes on alone.
    run_to(&mut i, 3.0);
    assert!(matches!(i.scene(), Scene::Speaking { .. }), "{:?}", i.scene());
}

#[test]
fn when_the_voice_ends_the_card_stays_alone_and_a_question_in_the_conversation_docks_at_its_bottom() {
    let mut i = island();
    i.apply(ask("q1", "Publish?"), 0.1);
    i.apply(Event::Speaking(true), 0.2);
    i.apply(Event::SpokenText(Some("Ready when you are.".into())), 0.25);
    run_to(&mut i, 1.0);
    i.apply(Event::Speaking(false), 1.0);
    run_to(&mut i, 3.0);
    assert!(matches!(i.scene(), Scene::Question { speech: None, .. }), "{:?}", i.scene());
    // The conversation pulled down, then a question: it stays, the question below it, outside its scroll.
    let mut j = island();
    j.apply(Event::Chat(day()), 0.0);
    j.apply(Event::Click(Target::Token(Token::Reply)), 0.2);
    run_to(&mut j, 1.0);
    j.apply(ask("q2", "Pause the retrain?"), 1.0);
    run_to(&mut j, 2.0);
    assert!(matches!(j.scene(), Scene::Reply { ask: Some(a), .. } if a.id == "q2"), "{:?}", j.scene());
    assert!(j.layout().h <= Display::MBP16.height / 2.0 + 1.0, "still never past half the screen");
    let answer = Target::AskPick { id: "q2".into(), question: 0, option: 0 };
    let before = j.targets().iter().find(|(t, _)| *t == answer).map(|(_, r)| *r).expect("its answers");
    j.apply(Event::Scroll(-300.0), 2.0);
    run_to(&mut j, 2.5);
    assert_eq!(j.targets().iter().find(|(t, _)| *t == answer).map(|(_, r)| *r), Some(before), "scrolling never moves it");
    let _ = j.take_outputs();
    tap(&mut j, &answer, 2.5);
    assert!(j.take_outputs().iter().any(|o| matches!(o, Output::Notice(_))));
    run_to(&mut j, 3.5);
    assert!(matches!(j.scene(), Scene::Reply { ask: None, .. }), "answered, the conversation stays open");
}

/// Bug note 2026-10-26T19-14-21 (and its correction 19-15-18): the conversation scrolled up went into a hard black cut
/// under the top bar. It goes on under the bar as a veil (the same body, blurred by the app, fading out toward the
/// screen's edge) and reaches a little into the window where the sharp lines fade in; at the top there is none, and
/// scrolling stays a change in place.
#[test]
fn the_conversation_goes_on_under_the_bar_as_a_veil() {
    let mut i = island();
    i.apply(Event::Chat(day()), 0.0);
    i.apply(Event::LastReply(super::gallery::CHAT_REPLY.into()), 0.1);
    i.apply(Event::OpenChat, 0.5);
    run_to(&mut i, 1.5);
    let l = i.layout().clone();
    assert!(l.scroll_max > 100.0 && l.scroll_at > 100.0, "opened scrolled: {} of {}", l.scroll_at, l.scroll_max);
    let veils = |i: &Island| -> Vec<(String, Rect, f64)> {
        i.layout().items.iter().filter_map(|it| match &*it.content {
            Content::Body { veil, .. } if *veil > 0.0 => Some((it.id.to_string(), it.rect, *veil)),
            _ => None,
        }).collect()
    };
    let shown: Vec<_> = veils(&i).into_iter().filter(|(_, r, _)| r.h > 0.0).collect();
    assert!(!shown.is_empty(), "a veil under the bar: {:?}", veils(&i));
    let band = shown[0].2;
    let top = shown.iter().map(|(_, r, _)| r.y).fold(f64::INFINITY, f64::min);
    let bottom = shown.iter().map(|(_, r, _)| r.max_y()).fold(0.0, f64::max);
    assert!(top < 1.0, "from the shape's top: {top}");
    assert!((bottom - band).abs() < 1e-6 && band > layout::VEIL_INTO, "into the window's top {} pt: {bottom} of {band}", layout::VEIL_INTO);
    // Every sharp body keeps a veil twin with the same marks (what it blurs is what scrolled up).
    for (id, r, _) in &shown {
        let sharp = id.trim_end_matches(".veil");
        assert!(i.layout().items.iter().any(|it| &*it.id == sharp), "{id} has its body");
        assert!(r.h > 0.0);
    }
    // Scrolling moves it in place.
    let before = i.motion_starts().len();
    for k in 0..30 {
        i.apply(Event::Scroll(-20.0), 1.5 + k as f64 * 0.016);
    }
    assert_eq!(i.motion_starts().len(), before, "scrolling started motions");
    // At the top: nothing above, no veil.
    i.apply(Event::Scroll(-1e9), 2.2);
    run_to(&mut i, 2.5);
    assert_eq!(i.layout().scroll_at, 0.0);
    assert!(veils(&i).iter().all(|(_, r, _)| r.h == 0.0), "no veil at the top: {:?}", veils(&i));
}

/// Bug note 2026-10-30T22-16-36, the rest of it: a right-click on a message of the pulled-down conversation copies that
/// message as written (the user's or Heimdall's), one on the reply copies the reply, and ⌘C with nothing selected copies the
/// reply. Off the words, at rest or mid-motion there is nothing to copy.
#[test]
fn a_message_under_the_pointer_and_the_reply_copy_as_written() {
    let mut i = island();
    assert_eq!(i.copy_shown(), None, "at rest");
    i.apply(Event::Chat(day()), 0.0);
    i.apply(Event::LastReply(super::gallery::CHAT_REPLY.into()), 0.1);
    i.apply(Event::OpenChat, 0.5);
    run_to(&mut i, 1.5);
    assert!(i.class() == Class::Reply);
    let copies = i.layout().copies.clone();
    let owner = "How much memory are the jobs using right now?";
    let (r, _) = copies.iter().find(|(_, t)| t == owner).expect("his last words can be copied");
    assert_eq!(i.copy_at(1.5, r.center()).as_deref(), Some(owner));
    let (r, _) = copies.iter().find(|(_, t)| t == super::gallery::CHAT_REPLY).expect("the reply can be copied");
    assert_eq!(i.copy_at(1.5, r.center()).as_deref(), Some(super::gallery::CHAT_REPLY));
    // The region is where the reply is drawn: the reply body's top and left edge, as far down as it shows.
    let body = i.layout().items.iter().find(|it| &*it.id == "reply.body").map(|it| it.rect).expect("the reply is drawn");
    assert!((r.y - body.y).abs() < 0.5 && (r.x - body.x).abs() < 0.5 && (r.max_y() - body.max_y()).abs() < 0.5, "{r:?} vs {body:?}");
    assert_eq!(i.copy_shown().as_deref(), Some(super::gallery::CHAT_REPLY));
    // Each region is in view and on the shape; a point off the words takes nothing.
    for (r, t) in &copies {
        assert!(r.h > 0.0 && !t.is_empty());
    }
    assert_eq!(i.copy_at(1.5, Point { x: 0.0, y: 5.0 }), None, "the band under the menu bar holds no words");
    assert_eq!(i.copy_at(1.5, Point { x: 5000.0, y: 300.0 }), None, "off the island");
}

/// Bug note 2026-10-30T22-16-36: "he asked for a cleaned-up quote, it showed on the island, and he couldn't select or
/// copy it". The pulled-down reply has a copy glyph: a click puts the reply as written on the clipboard, once per
/// click, and the glyph becomes a check.
#[test]
fn the_pulled_down_reply_copies_its_text() {
    let mut i = island();
    let quote = "\u{201c}The best way to predict the future is to invent it.\u{201d} \u{2014} Alan Kay";
    open_reply(&mut i, 0.2, quote);
    let _ = i.take_outputs();
    tap(&mut i, &Target::ReplyCopy, 3.0);
    let copies: Vec<String> = i.take_outputs().into_iter().filter_map(|o| match o { Output::Copy(t) => Some(t), _ => None }).collect();
    assert_eq!(copies, vec![quote.to_string()]);
    assert!(matches!(i.scene(), Scene::Reply { copied: true, .. }), "{:?}", i.scene());
    assert!(i.reply_open(), "copying leaves the reply open");
}
