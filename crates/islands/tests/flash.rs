//! Hidden islands come back for news (`model/flash.rs`): what counts as news, and the reveal's state machine.

use islands::jobj;
use islands::model::flash::{news, Cmd, Flash, News, FADE_OUT, HOLD, HOVER_CHECK, LINGER, PULSE_GAP};
use islands::model::Island;

fn i(id: &str, raw: serde_json::Map<String, serde_json::Value>) -> Island {
    Island::new(id, raw)
}
fn s(id: &str) -> String {
    id.to_owned()
}

#[test]
fn a_result_a_wait_an_ask_an_arrival_and_a_reply_are_news() {
    let run = i("t", jobj! {"title": "Tests", "state": "running", "stage": "Building"});
    assert_eq!(news(None, &run), Some(News::Arrived), "a new island: a notice, a job, a pulled-out reply");
    assert_eq!(news(Some(&run), &i("t", jobj! {"title": "Tests", "state": "done"})), Some(News::Result));
    assert_eq!(news(Some(&run), &i("t", jobj! {"title": "Tests", "state": "failed"})), Some(News::Result));
    assert_eq!(news(Some(&run), &i("t", jobj! {"title": "Tests", "state": "waiting"})), Some(News::Waiting));
    let q = i("t", jobj! {"title": "Tests", "state": "running", "attention": "question", "parts": [{"type": "button", "label": "Warm", "action": "t3q:r1:0"}]});
    assert_eq!(news(Some(&run), &q), Some(News::Ask));
    let q2 = i("t", jobj! {"title": "Tests", "state": "running", "attention": "question", "parts": [{"type": "button", "label": "Next", "action": "t3q:r2:0"}]});
    assert_eq!(news(Some(&q), &q2), Some(News::Ask), "a new question on the same island");
    let v1 = i("view-a", jobj! {"preset": "viewer", "path": "/a.md", "updated": 1.0});
    let v2 = i("view-a", jobj! {"preset": "viewer", "path": "/a.md", "updated": 2.0});
    assert_eq!(news(Some(&v1), &v2), Some(News::Reply), "Heimdall showed it again");
    let n1 = i("n", jobj! {"preset": "note", "summary": "One"});
    assert_eq!(news(Some(&n1), &i("n", jobj! {"preset": "note", "summary": "Two"})), Some(News::Reply));
}

#[test]
fn progress_and_stage_updates_are_not_news() {
    let a = i("t", jobj! {"title": "Tests", "state": "running", "stage": "Building", "progress": 0.2, "updated": 1.0});
    let b = i("t", jobj! {"title": "Tests", "state": "running", "stage": "Linking", "progress": 0.6, "updated": 2.0, "summary": "Half"});
    assert_eq!(news(Some(&a), &b), None);
    let done = i("t", jobj! {"title": "Tests", "state": "done", "value": "3 / 3"});
    assert_eq!(news(Some(&done), &i("t", jobj! {"title": "Tests", "state": "done", "value": "3 / 3", "expanded": true})), None, "still done");
    assert_eq!(news(Some(&done), &run_again()), None, "running again is not news");
    let q = i("t", jobj! {"state": "waiting", "attention": "question", "parts": [{"type": "button", "action": "x"}]});
    let answered = i("t", jobj! {"state": "waiting", "attention": "question", "parts": []});
    assert_eq!(news(Some(&q), &answered), None, "a question going away is not news");
}

fn run_again() -> Island {
    i("t", jobj! {"title": "Tests", "state": "running"})
}

#[test]
fn plan_lists_are_news_only_for_a_new_waiting_item() {
    let todo = |items: serde_json::Value| i("todo", jobj! {"preset": "todo", "list": "todo", "items": items});
    let a = todo(serde_json::json!([{"id": "a", "title": "Port", "group": "running"}]));
    let b = todo(serde_json::json!([{"id": "a", "title": "Port", "group": "running", "detail": "More"}, {"id": "b", "title": "Ship", "group": "running"}]));
    assert_eq!(news(Some(&a), &b), None, "the refresher's rewrites and new running items");
    let c = todo(serde_json::json!([{"id": "a", "title": "Port", "group": "running"}, {"id": "w", "title": "Pick a voice", "group": "waiting"}]));
    assert_eq!(news(Some(&b), &c), Some(News::Ask));
    let d = todo(serde_json::json!([{"id": "w", "title": "Pick a voice", "group": "waiting"}, {"id": "w2", "title": "Approve", "group": "waiting"}]));
    assert_eq!(news(Some(&c), &d), Some(News::Ask), "a second waiting item");
    let e = todo(serde_json::json!([{"id": "w2", "title": "Approve", "group": "waiting"}]));
    assert_eq!(news(Some(&d), &e), None, "one answered");
}

#[test]
fn news_reveals_once_pulses_and_fades_after_the_hold() {
    let mut f = Flash::new();
    assert_eq!(f.news("t", 10.0, false), vec![Cmd::Reveal(s("t")), Cmd::Pulse(s("t"))]);
    assert!(f.is_up("t"));
    assert_eq!(f.next_wake(10.0), Some(10.0 + HOLD));
    assert!(f.tick(10.0 + HOLD - 0.01).is_empty());
    assert_eq!(f.tick(10.0 + HOLD), vec![Cmd::Conceal(s("t"))]);
    assert!(f.is_up("t"), "still on screen while it fades");
    assert_eq!(f.next_wake(10.0 + HOLD), Some(10.0 + HOLD + FADE_OUT));
    assert_eq!(f.tick(10.0 + HOLD + FADE_OUT), vec![Cmd::Gone(s("t"))]);
    assert!(!f.is_up("t"));
    assert_eq!(f.next_wake(20.0), None, "nothing left: no timer");
}

#[test]
fn news_in_a_row_extends_without_flicker() {
    let mut f = Flash::new();
    f.news("t", 0.0, false);
    assert!(f.news("t", 0.5, false).is_empty(), "already up, pulsed just now: nothing");
    assert_eq!(f.news("t", PULSE_GAP + 0.1, false), vec![Cmd::Pulse(s("t"))], "up: one more pulse, no second reveal");
    assert!(f.tick(HOLD + 0.5).is_empty(), "the hold counts from the latest news");
    assert_eq!(f.tick(PULSE_GAP + 0.1 + HOLD), vec![Cmd::Conceal(s("t"))]);
}

#[test]
fn several_islands_leave_together() {
    let mut f = Flash::new();
    f.news("a", 0.0, false);
    f.news("b", 3.0, false);
    assert!(f.tick(HOLD).is_empty(), "a waits for b");
    assert_eq!(f.next_wake(HOLD), Some(3.0 + HOLD));
    assert_eq!(f.tick(3.0 + HOLD), vec![Cmd::Conceal(s("a")), Cmd::Conceal(s("b"))]);
}

#[test]
fn news_while_fading_out_brings_it_back() {
    let mut f = Flash::new();
    f.news("t", 0.0, false);
    f.tick(HOLD);
    assert_eq!(f.news("t", HOLD + 0.2, false), vec![Cmd::Reveal(s("t")), Cmd::Pulse(s("t"))]);
    assert!(f.tick(HOLD + FADE_OUT).is_empty(), "the old fade's end does not take it off screen");
    assert_eq!(f.tick(HOLD + 0.2 + HOLD), vec![Cmd::Conceal(s("t"))]);
}

#[test]
fn hover_keeps_it_and_leaving_lingers() {
    let mut f = Flash::new();
    f.news("t", 0.0, false);
    f.set_hover("t", true, HOLD);
    assert!(f.tick(HOLD).is_empty());
    assert_eq!(f.next_wake(HOLD), Some(HOLD + HOVER_CHECK), "the pointer is looked at again while it is there");
    assert!(f.tick(60.0).is_empty(), "held as long as the pointer stays");
    f.set_hover("t", false, 60.0);
    assert!(f.tick(60.0 + LINGER - 0.01).is_empty());
    assert_eq!(f.tick(60.0 + LINGER), vec![Cmd::Conceal(s("t"))]);
}

#[test]
fn an_open_island_stays_until_it_closes() {
    let mut f = Flash::new();
    f.news("t", 0.0, false);
    f.set_open("t", true, 1.0);
    assert!(f.tick(100.0).is_empty());
    assert_eq!(f.next_wake(100.0), None, "held by openness: no timer until it closes");
    f.set_open("t", false, 100.0);
    assert_eq!(f.tick(100.0 + LINGER), vec![Cmd::Conceal(s("t"))]);
}

#[test]
fn a_question_stays_until_answered_or_dismissed() {
    let mut f = Flash::new();
    f.news("q", 0.0, true);
    f.news("r", 0.0, false);
    assert_eq!(f.tick(HOLD), vec![Cmd::Conceal(s("r"))], "the result leaves; the question does not hold it up");
    assert_eq!(f.tick(HOLD + FADE_OUT), vec![Cmd::Gone(s("r"))]);
    assert!(f.tick(3600.0).is_empty(), "an hour later the question is still there");
    assert!(f.is_up("q"));
    f.set_sticky("q", false, 3600.0);
    assert_eq!(f.tick(3600.0 + LINGER), vec![Cmd::Conceal(s("q"))], "answered: it lingers, then goes");
    let mut g = Flash::new();
    g.news("q", 0.0, true);
    g.removed("q");
    assert!(!g.is_up("q"), "dismissed (closed for good)");
    assert!(g.tick(10.0).is_empty());
}

#[test]
fn showing_or_hiding_all_ends_every_reveal() {
    let mut f = Flash::new();
    f.news("a", 0.0, false);
    f.news("b", 0.0, true);
    assert_eq!(f.clear(), vec![s("a"), s("b")]);
    assert!(f.up().is_empty());
    assert_eq!(f.next_wake(0.0), None);
}

/// Review 24 Oct: clicked or pointed at in the last 0.6 s, it used to be ordered out while open.
#[test]
fn held_while_fading_out_comes_back() {
    let mut f = Flash::new();
    f.news("t", 0.0, false);
    assert_eq!(f.tick(HOLD), vec![Cmd::Conceal(s("t"))]);
    assert_eq!(f.set_open("t", true, HOLD + 0.3), vec![Cmd::Reveal(s("t"))]);
    assert!(f.tick(HOLD + FADE_OUT).is_empty(), "no Gone for an open island");
    assert!(f.tick(100.0).is_empty());
    f.set_open("t", false, 100.0);
    assert_eq!(f.tick(100.0 + LINGER), vec![Cmd::Conceal(s("t"))]);
    let mut g = Flash::new();
    g.news("t", 0.0, false);
    g.tick(HOLD);
    assert_eq!(g.set_hover("t", true, HOLD + 0.5), vec![Cmd::Reveal(s("t"))]);
    assert!(g.tick(HOLD + FADE_OUT).is_empty());
    assert!(g.set_hover("t", true, HOLD + 1.0).is_empty(), "already back: no second reveal");
}

#[test]
fn news_for_an_island_already_open_holds_it() {
    let mut f = Flash::new();
    f.news("v", 0.0, false);
    f.set_open("v", true, 0.0);
    assert!(f.news("v", 10.0, false).iter().all(|c| matches!(c, Cmd::Pulse(_))));
    assert!(f.tick(60.0).is_empty(), "open: stays until closed");
}
