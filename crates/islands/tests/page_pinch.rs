//! A page's own pinch in the viewer (`model::page_pinch`): who gets each magnify event.

use islands::model::page_pinch::{factor, page_pinch, Call, Phase, Router, Step, MAX_HELD, MIN_FACTOR};
use islands::model::viewer_size::page_ask;

const META: &str = r#"<meta name="heimdall-viewer" content="width=1500, height=1000, pinch=page">"#;

fn call(phase: &'static str, scale: f64, x: f64, y: f64) -> Call {
    Call { phase, scale, x, y }
}

#[test]
fn a_page_says_it_takes_its_pinch_in_its_viewer_meta() {
    assert!(page_pinch(&format!("<html><head>{META}</head>")));
    assert!(page_pinch("<meta name='heimdall-viewer' content='pinch = PAGE'>"), "spaces and case");
    assert!(page_pinch("<meta name=heimdall-viewer content=pinch=page/>"), "unquoted, self-closed");
    assert!(!page_pinch("<meta name='heimdall-viewer' content='width=1500, height=1000'>"), "a size alone");
    assert!(!page_pinch("<meta name='heimdall-viewer' content='pinch=view'>"));
    assert!(!page_pinch("<meta name='viewport' content='pinch=page'>"), "another meta");
    assert!(!page_pinch("<p>pinch=page</p>"));
    assert!(!page_pinch(&format!("{}{META}", " ".repeat(70 * 1024))), "past the first 64 KB");
    // The size it asks is read as before, the pinch key beside it changes nothing.
    let a = page_ask(META).expect("asks");
    assert_eq!((a.w, a.h), (Some(1500.0), Some(1000.0)));
    assert_eq!(page_ask("<meta name='heimdall-viewer' content='pinch=page'>"), None, "pinch alone asks no size");
}

#[test]
fn a_page_that_says_nothing_is_never_asked() {
    let mut r = Router::new(false);
    for (p, m) in [(Phase::Begin, 0.0), (Phase::Change, 0.1), (Phase::Change, -0.05), (Phase::End, 0.0)] {
        assert_eq!(r.event(p, m, 10.0, 10.0), vec![Step::View], "{p:?}: WebKit's own magnify, as today");
    }
    assert_eq!(r.double_tap(10.0, 10.0), None, "its double-tap is WebKit's smart zoom");
    assert!(!r.page_has_it());
}

#[test]
fn the_begin_asks_the_page_and_holds_the_gesture_until_it_answers() {
    let mut r = Router::new(true);
    assert_eq!(r.event(Phase::Begin, 0.0, 100.0, 50.0), vec![Step::Ask(call("begin", 1.0, 100.0, 50.0), 1)]);
    assert_eq!(r.event(Phase::Change, 0.1, 101.0, 50.0), vec![Step::Hold]);
    assert_eq!(r.event(Phase::Change, 0.2, 102.0, 51.0), vec![Step::Hold]);
    // Taken: the held changes reach the page as one change of their product at the last point, then it has them all.
    let s = r.answer(1, true);
    assert_eq!(s.len(), 2);
    assert_eq!(s[0], Step::DropHeld);
    let Step::Page(c) = s[1] else { panic!("{s:?}") };
    assert_eq!((c.phase, c.x, c.y), ("change", 102.0, 51.0));
    assert!((c.scale - 1.1 * 1.2).abs() < 1e-12, "no step lost or doubled: {}", c.scale);
    assert!(r.page_has_it());
    assert_eq!(r.event(Phase::Change, -0.05, 103.0, 52.0), vec![Step::Page(call("change", 0.95, 103.0, 52.0))]);
    assert_eq!(r.event(Phase::End, 0.0, 103.0, 52.0), vec![Step::Page(call("end", 1.0, 103.0, 52.0))]);
    assert!(!r.page_has_it());
}

#[test]
fn an_answer_with_nothing_held_sends_nothing_more() {
    let mut r = Router::new(true);
    r.event(Phase::Begin, 0.0, 1.0, 1.0);
    assert_eq!(r.answer(1, true), vec![Step::DropHeld], "no empty change (no jump at the start)");
    assert_eq!(r.event(Phase::Change, 0.02, 1.0, 1.0), vec![Step::Page(call("change", 1.02, 1.0, 1.0))]);
}

#[test]
fn a_page_that_declines_throws_or_has_no_hook_gets_the_whole_page_magnified() {
    // The hook's script answers false for all three (`Call::script`); the viewer then replays what it held to WebKit.
    let mut r = Router::new(true);
    r.event(Phase::Begin, 0.0, 5.0, 5.0);
    r.event(Phase::Change, 0.1, 5.0, 5.0);
    assert_eq!(r.answer(1, false), vec![Step::ReplayHeld]);
    assert_eq!(r.event(Phase::Change, 0.1, 5.0, 5.0), vec![Step::View]);
    assert_eq!(r.event(Phase::End, 0.0, 5.0, 5.0), vec![Step::View]);
    // The next gesture asks again (the pointer may be over the CV now).
    assert!(matches!(r.event(Phase::Begin, 0.0, 5.0, 5.0)[..], [Step::Ask(_, 2)]));
}

#[test]
fn the_script_answers_true_only_for_a_hook_that_returns_true() {
    let js = call("begin", 1.25, 12.5, 40.0).script();
    assert!(js.contains("typeof f!=='function')return false"), "no hook: false");
    assert!(js.contains("catch(e){return false}"), "a hook that throws: false");
    assert!(js.contains("===true"), "only exactly true takes it");
    assert!(js.contains("phase:'begin',scale:1.25"));
    assert!(js.contains("+12.5/s") && js.contains("+40/s"), "the pointer through the visual viewport: {js}");
    assert!(!call("change", f64::NAN, f64::INFINITY, 0.0).script().contains("NaN"), "never NaN into the page");
}

#[test]
fn a_gesture_that_ends_before_the_answer_finishes_on_the_side_that_gets_it() {
    let mut r = Router::new(true);
    r.event(Phase::Begin, 0.0, 1.0, 2.0);
    r.event(Phase::Change, 0.5, 1.0, 2.0);
    assert_eq!(r.event(Phase::End, 0.0, 1.0, 2.0), vec![Step::Hold]);
    assert_eq!(r.answer(1, true), vec![Step::DropHeld, Step::Page(call("change", 1.5, 1.0, 2.0)), Step::Page(call("end", 1.0, 1.0, 2.0))]);
    assert!(!r.page_has_it());
    r.event(Phase::Begin, 0.0, 1.0, 2.0);
    r.event(Phase::End, 0.0, 1.0, 2.0);
    assert_eq!(r.answer(2, false), vec![Step::ReplayHeld], "begin and end replayed to WebKit");
    assert!(matches!(r.event(Phase::Begin, 0.0, 1.0, 2.0)[..], [Step::Ask(_, 3)]), "idle again");
}

#[test]
fn a_late_answer_for_an_older_gesture_is_ignored() {
    let mut r = Router::new(true);
    r.event(Phase::Begin, 0.0, 0.0, 0.0);
    r.event(Phase::End, 0.0, 0.0, 0.0);
    r.event(Phase::Begin, 0.0, 0.0, 0.0);
    assert_eq!(r.answer(1, true), vec![], "gesture 1's answer while 2 waits");
    assert_eq!(r.answer(2, true), vec![Step::DropHeld]);
    assert_eq!(r.answer(2, false), vec![], "answered already");
}

#[test]
fn a_change_with_no_begin_starts_a_gesture() {
    let mut r = Router::new(true);
    assert_eq!(r.event(Phase::Change, 0.1, 3.0, 4.0), vec![Step::Ask(call("begin", 1.1, 3.0, 4.0), 1)]);
}

#[test]
fn each_event_hands_the_page_its_own_factor_as_chromium_does() {
    // Chromium gives a page exp(-deltaY/100) per pinch update with deltaY = -100·ln(1 + m): the same 1 + m.
    for m in [0.0, 0.01, 0.12, -0.05, -0.3] {
        assert!((factor(m) - (-(-100.0 * (1.0f64 + m).ln()) / 100.0).exp()).abs() < 1e-12, "{m}");
    }
    assert_eq!(factor(-1.5), MIN_FACTOR, "never zero or negative");
    assert_eq!(factor(f64::NAN), 1.0);
}

#[test]
fn phases_come_from_appkit_bits() {
    assert_eq!(Phase::from_bits(1), Phase::Begin);
    assert_eq!(Phase::from_bits(4), Phase::Change);
    assert_eq!(Phase::from_bits(2), Phase::Change, "stationary");
    assert_eq!(Phase::from_bits(0), Phase::Change, "no phase");
    assert_eq!(Phase::from_bits(8), Phase::End);
    assert_eq!(Phase::from_bits(16), Phase::End, "cancelled");
}

#[test]
fn the_double_tap_is_asked_of_a_page_that_takes_its_pinch() {
    let r = Router::new(true);
    assert_eq!(r.double_tap(7.0, 8.0), Some(call("double-tap", 1.0, 7.0, 8.0)));
}

#[test]
fn a_pinch_begun_before_the_last_one_was_answered_drops_the_old_rest_and_asks_anew() {
    let mut r = Router::new(true);
    r.event(Phase::Begin, 0.0, 0.0, 0.0);
    r.event(Phase::Change, 0.3, 0.0, 0.0);
    r.event(Phase::End, 0.0, 0.0, 0.0);
    assert_eq!(r.event(Phase::Begin, 0.0, 0.0, 0.0), vec![Step::DropHeld, Step::Ask(call("begin", 1.0, 0.0, 0.0), 2)]);
    assert_eq!(r.answer(1, false), vec![], "the old gesture's late answer replays nothing");
    r.event(Phase::Change, 0.1, 0.0, 0.0);
    let s = r.answer(2, true);
    assert_eq!(s, vec![Step::DropHeld, Step::Page(call("change", 1.1, 0.0, 0.0))], "only the new gesture's own step");
}

#[test]
fn a_page_that_never_answers_lets_go_of_the_gesture_after_a_second() {
    let mut r = Router::new(true);
    r.event(Phase::Begin, 0.0, 0.0, 0.0);
    for _ in 0..MAX_HELD {
        assert_eq!(r.event(Phase::Change, 0.001, 0.0, 0.0), vec![Step::Hold]);
    }
    assert_eq!(r.event(Phase::Change, 0.001, 0.0, 0.0), vec![Step::ReplayHeld, Step::View], "WebKit gets it all, in order");
    assert_eq!(r.event(Phase::Change, 0.001, 0.0, 0.0), vec![Step::View]);
    assert_eq!(r.answer(1, true), vec![], "too late");
}

#[test]
fn a_rebuilt_page_goes_on_numbering_so_the_old_pages_late_answer_is_ignored() {
    let mut old = Router::new(true);
    old.event(Phase::Begin, 0.0, 0.0, 0.0);
    let mut new = old.renewed(true);
    assert!(matches!(new.event(Phase::Begin, 0.0, 0.0, 0.0)[..], [Step::Ask(_, 2)]));
    assert_eq!(new.answer(1, false), vec![], "the old page's answer to its gesture 1");
}

#[test]
fn an_end_with_its_own_factor_is_one_last_change_on_both_paths() {
    let mut r = Router::new(true);
    r.event(Phase::Begin, 0.0, 0.0, 0.0);
    r.answer(1, true);
    assert_eq!(r.event(Phase::End, 0.05, 0.0, 0.0), vec![Step::Page(call("change", 1.05, 0.0, 0.0)), Step::Page(call("end", 1.0, 0.0, 0.0))]);
}
