//! A question card's countdown (the quota recovery card counts down about a minute, then Heimdall
//! searches on its own). The card shows m:ss and a ring once a second while it is on screen; at its expiry it shows
//! 0:00 and waits for the controller; nothing wakes for it once it is gone.

use super::{Approx, Content, Display, Event, FrameNeed, Island, Scene};
use crate::agent_notice::{countdown_text, AgentNotice, AskOption, AskQuestion, NoticeAction, NoticeKind};

const OPTIONS: [&str; 4] = ["Claude Opus · account 2", "Codex · gpt-6", "Local model · Qwen3.8 27B", "Wait for Claude account 1"];

fn island() -> Island {
    Island::new(Display::MBP16, Box::new(Approx))
}

/// A one-question card `id` expiring at wall-clock `expires_ms` (the island's wall origin is 0: t seconds is t·1000 ms).
fn card(id: &str, expires_ms: u64, countdown: bool) -> Event {
    let q = AskQuestion { id: "q1".into(), header: None, text: "Claude is out of quota. Continue with".into(), multi_select: false, allow_other: false, field: None, options: OPTIONS.iter().map(|l| AskOption::label(l)).collect() };
    let mut n = AgentNotice::line(id, "Claude is out of quota", NoticeKind::Warning);
    n.action = Some(NoticeAction::Ask { request_id: id.into(), questions: vec![q], expires_ms, countdown });
    Event::Notice { notice: n, body: None }
}

/// Runs every deadline up to `until`, then ticks there; returns how many deadlines fired.
fn run_to(i: &mut Island, until: f64) -> usize {
    let mut fired = 0;
    while let Some(d) = i.deadline().filter(|d| *d <= until) {
        i.apply(Event::Tick, d);
        fired += 1;
        assert!(fired < 10_000, "a deadline that never moves on");
    }
    i.apply(Event::Tick, until);
    fired
}

/// The countdown's numerals in the layout, and whether its ring is there.
fn shown(i: &Island) -> (Option<String>, bool) {
    let items = &i.layout().items;
    let text = items.iter().find(|it| &*it.id == "q.countdown").and_then(|it| match &*it.content {
        Content::Text { lines, .. } => lines.first().cloned(),
        _ => None,
    });
    (text, items.iter().any(|it| &*it.id == "q.countdown.ring" && matches!(*it.content, Content::Ring { .. })))
}

#[test]
fn countdown_text_is_m_ss_rounded_up_and_never_negative() {
    assert_eq!(countdown_text(60_000), "1:00");
    assert_eq!(countdown_text(59_000), "0:59");
    assert_eq!(countdown_text(47_000), "0:47");
    assert_eq!(countdown_text(46_001), "0:47", "rounded up: 0:00 only once time is out");
    assert_eq!(countdown_text(1), "0:01");
    assert_eq!(countdown_text(0), "0:00");
    assert_eq!(countdown_text(125_000), "2:05");
}

/// 0:59 down to 0:00, one change a second, then still: the card stays open (no Dismiss) for the controller to close.
#[test]
fn the_card_counts_down_once_a_second_to_zero_and_stays_open() {
    let mut i = island();
    i.apply(card("r", 60_000, true), 0.0);
    run_to(&mut i, 0.5);
    assert_eq!(shown(&i), (Some("1:00".into()), true), "on arrival: the full minute and the ring");
    run_to(&mut i, 1.0);
    assert_eq!(shown(&i).0.as_deref(), Some("0:59"));
    let mut seen = vec![];
    for k in 2..=64 {
        run_to(&mut i, k as f64);
        let (text, ring) = shown(&i);
        let text = text.expect("the numerals stay while the card shows");
        assert!(ring && !text.starts_with('-'), "{text}");
        seen.push(text);
    }
    assert_eq!(seen[..3], ["0:58", "0:57", "0:56"]);
    assert_eq!(seen[57], "0:01", "at 59 s");
    assert!(seen[58..].iter().all(|s| s == "0:00"), "0:00 from 60 s on: {:?}", &seen[58..]);
    assert!(matches!(i.scene(), Scene::Question { card, .. } if card.look == super::layout::QLook::Open), "the UI does not expire it: {:?}", i.scene());
    // Out of time, nothing more to draw: no deadline and no frames for the countdown.
    assert_eq!(i.deadline(), None, "nothing left to count");
    assert_eq!(i.needs_frames(64.0), FrameNeed::None);
}

/// Each second's change is a cut in place: it starts no motion.
#[test]
fn a_second_ticking_over_starts_no_motion() {
    let mut i = island();
    i.apply(card("r", 60_000, true), 0.0);
    run_to(&mut i, 5.0);
    assert_eq!(i.needs_frames(5.0), FrameNeed::None, "settled");
    let d = i.deadline().expect("the next second");
    assert!(d > 5.0 && d <= 6.001, "{d}");
    i.apply(Event::Tick, d);
    assert_eq!(i.needs_frames(d), FrameNeed::None, "the numerals and ring cut; the shape stays still");
    assert!(!i.frame(d).moving);
}

/// A card without a countdown looks as before: no numerals, no ring, and the clock wakes only at its expiry.
#[test]
fn a_card_without_countdown_shows_none() {
    let mut i = island();
    i.apply(card("p", 60_000, false), 0.0);
    let fired = run_to(&mut i, 30.0);
    assert_eq!(shown(&i), (None, false));
    assert!(fired < 10, "no per-second ticks: {fired}");
    assert_eq!(i.deadline(), Some(60.0), "only the expiry");
}

/// The per-second schedule ends with the card: closed by the controller, nothing wakes again.
#[test]
fn the_ticks_stop_when_the_card_closes() {
    let mut i = island();
    i.apply(card("r", 60_000, true), 0.0);
    let fired = run_to(&mut i, 10.0);
    assert!((9..=14).contains(&fired), "about one a second: {fired}");
    i.apply(Event::NoticeClear("r".into()), 10.2);
    run_to(&mut i, 12.0);
    assert_eq!(shown(&i), (None, false));
    assert_eq!(i.deadline(), None, "no countdown deadline once the card is gone");
    assert_eq!(i.needs_frames(12.0), FrameNeed::None);
    assert_eq!(run_to(&mut i, 70.0), 0, "nothing fires up to and past its old expiry");
}
