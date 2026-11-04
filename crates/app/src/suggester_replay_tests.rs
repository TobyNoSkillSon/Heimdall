//! The replay's rules without the network: the archive's lines in, the stub and the pacing through an injected call, the
//! budget, and the page's escaping. A sample line has the shape `heimdall-archive timeline` prints (its fields only).
use super::*;
use serde_json::json;

const NOW: u64 = 1_800_000_000_000;

fn utterance(wall_ms: u64, speaker: &str, text: &str) -> String {
    json!({"time": "10:00:00", "wall_ns": wall_ms * 1_000_000, "id": 1, "session": 2, "kind": "utterance", "source": "Shure MV7i",
           "utterance_ids": [], "host_ns": 0, "payload": {"speaker": speaker, "text": text, "start_ns": 0, "end_ns": 0, "decision": {"choice": "wait"}}})
        .to_string()
}

fn card(topic: &str) -> sg::Verdict {
    let answer = json!({"suggest": true, "topic": topic, "questions": [{"text": "Find it?", "options": ["Find the file", "Not now"], "multi_select": false}], "found": [], "handoff": "h"});
    sg::verify(&answer.to_string(), &[]).unwrap()
}

fn silent() -> sg::Verdict {
    sg::Verdict::Silent("quiet: small talk".into())
}

#[test]
fn an_archive_line_is_a_line_of_talk_and_a_wordless_one_is_not() {
    let l = parse_line(&utterance(NOW, "User", "I remember there was a CAD file")).unwrap();
    assert_eq!(l, sg::Line { at_ms: NOW, speaker: "User".into(), text: "I remember there was a CAD file".into(), owner: true });
    assert!(!parse_line(&utterance(NOW, "Sam", "hi")).unwrap().owner);
    assert!(parse_line(&utterance(NOW, "Sam", "   ")).is_none());
    assert!(parse_line("{\"kind\": \"model_output\"}").is_none());
    assert!(parse_line(&utterance(NOW, "Heimdall", "Yes, I can do that.")).is_none(), "Heimdall's own speech is not talk");
    assert!(parse_line("not json").is_none());
}

#[test]
fn the_talk_is_read_in_time_order() {
    let text = [utterance(NOW + 5_000, "User", "b"), utterance(NOW, "Sam", "a")].join("\n");
    let lines = parse(&text);
    assert_eq!(lines.iter().map(|l| l.text.as_str()).collect::<Vec<_>>(), vec!["a", "b"]);
}

/// The talk that makes the stub fire on the user's line at `at`: Sam spoke just before, the user asks about a file.
fn cue(at: u64) -> Vec<sg::Line> {
    vec![
        sg::Line { at_ms: at - 30_000, speaker: "Sam".into(), text: "what did we build".into(), owner: false },
        sg::Line { at_ms: at, speaker: "User".into(), text: "I remember there was a CAD file like this".into(), owner: true },
    ]
}

#[test]
fn a_silent_answer_costs_its_call_and_shows_nothing() {
    let lines = cue(NOW);
    let mut calls = 0;
    let out = run(
        &lines,
        Some(&mut |_| {
            calls += 1;
            (Ok(silent()), 0.01)
        }),
        &mut Budget::default(),
    );
    assert_eq!(calls, 1);
    assert_eq!(out.len(), 1);
    assert_eq!((out[0].outcome.as_str(), out[0].called), ("silent", true));
    assert_eq!(out[0].reason, "quiet: small talk");
    assert!(out[0].card.is_none() && (out[0].usd - 0.01).abs() < 1e-12);
}

#[test]
fn a_card_holds_the_pacer_for_its_life_and_the_gap_then_allows_the_next() {
    let mut lines = cue(NOW);
    // A second cue inside the card's life: no call (a card is up). After it, another cue on a new topic: shown.
    lines.push(sg::Line { at_ms: NOW + 60_000, speaker: "Sam".into(), text: "ok".into(), owner: false });
    lines.push(sg::Line { at_ms: NOW + 90_000, speaker: "User".into(), text: "who was that about the photos".into(), owner: true });
    lines.push(sg::Line { at_ms: NOW + sg::CARD_MS + 60_000, speaker: "User".into(), text: "I remember there was a photo from the trip".into(), owner: true });
    let mut topics = vec!["CAD file".to_owned(), "Sam's photos".to_owned()].into_iter();
    let out = run(&lines, Some(&mut |_| (Ok(card(&topics.next().unwrap_or_default())), 0.02)), &mut Budget::default());
    let outcomes: Vec<(&str, bool)> = out.iter().map(|d| (d.outcome.as_str(), d.called)).collect();
    assert_eq!(outcomes, vec![("card", true), ("pacing", false), ("card", true)], "{outcomes:?}");
    assert_eq!(out[0].card.as_ref().unwrap().topic, "CAD file");
    assert_eq!(out[2].card.as_ref().unwrap().topic, "Sam's photos");
}

#[test]
fn a_card_on_a_topic_already_offered_is_not_shown_again() {
    let mut lines = cue(NOW);
    // Someone else talks again inside the five minutes before the user's next line (the stub's "someone talked" rule).
    lines.push(sg::Line { at_ms: NOW + sg::CARD_MS, speaker: "Sam".into(), text: "so".into(), owner: false });
    lines.push(sg::Line { at_ms: NOW + sg::CARD_MS + 60_000, speaker: "User".into(), text: "I remember there was the CAD file again".into(), owner: true });
    let out = run(&lines, Some(&mut |_| (Ok(card("CAD file")), 0.02)), &mut Budget::default());
    assert_eq!(out[0].outcome, "card");
    assert_eq!(out[1].outcome, "pacing", "the same topic within two hours: called, not shown");
    assert!(out[1].called && out[1].card.is_none());
}

#[test]
fn spend_stops_the_calls_at_the_budget() {
    // Each call costs $1.00 and the calls are a minute apart: the third would start past the budget.
    let mut lines = vec![];
    for i in 0..6u64 {
        lines.push(sg::Line { at_ms: NOW + i * 120_000, speaker: "Sam".into(), text: "hi".into(), owner: false });
        lines.push(sg::Line { at_ms: NOW + i * 120_000 + 1_000, speaker: "User".into(), text: "I remember there was this".into(), owner: true });
    }
    let mut calls = 0;
    let out = run(
        &lines,
        Some(&mut |_| {
            calls += 1;
            (Ok(silent()), 1.0)
        }),
        &mut Budget::default(),
    );
    assert_eq!(calls, 3, "$1.00 a call against a $2.50 budget: the third starts at $2.00, the fourth would start at $3.00");
    assert_eq!(out.iter().filter(|d| d.outcome == "budget").count(), 3);
    assert!(out.iter().filter(|d| d.outcome == "budget").all(|d| !d.called));
}

#[test]
fn the_call_count_is_capped_too() {
    let mut lines = vec![];
    for i in 0..(MAX_CALLS as u64 + 4) {
        lines.push(sg::Line { at_ms: NOW + i * 120_000, speaker: "Sam".into(), text: "hi".into(), owner: false });
        lines.push(sg::Line { at_ms: NOW + i * 120_000 + 1_000, speaker: "User".into(), text: "I remember there was this".into(), owner: true });
    }
    let mut calls = 0;
    run(
        &lines,
        Some(&mut |_| {
            calls += 1;
            (Ok(silent()), 0.0)
        }),
        &mut Budget::default(),
    );
    assert_eq!(calls, MAX_CALLS);
}

#[test]
fn an_error_is_a_decision_and_the_replay_goes_on() {
    let lines = cue(NOW);
    let out = run(&lines, Some(&mut |_| (Err(anyhow::anyhow!("request failed: boom")), 0.0)), &mut Budget::default());
    assert_eq!(out[0].outcome, "error");
    assert!(out[0].reason.contains("boom"));
}

#[test]
fn the_page_answers_first_and_escapes_the_talk() {
    let conv = vec![(
        "talk <one>".to_owned(),
        run(&cue(NOW), Some(&mut |_| (Ok(card("CAD file")), 0.02)), &mut Budget::default()),
    )];
    let page = html(&conv, 0.02, 1, false);
    assert!(page.contains("1 card(s) would have shown across 1 conversation(s), from 1 model call(s)"), "{page}");
    assert!(page.contains("talk &lt;one&gt;"), "the title is escaped");
    assert!(page.contains("CAD file") && page.contains("Find the file"));
    assert!(!page.contains("<one>"));
    let quiet = html(&[("empty".into(), vec![])], 0.0, 0, false);
    assert!(quiet.contains("never met the stub"));
}

#[test]
fn triggers_only_lists_every_stub_moment_and_makes_no_call_or_pacing() {
    let mut lines = cue(NOW);
    lines.push(sg::Line { at_ms: NOW + 10_000, speaker: "Sam".into(), text: "yes".into(), owner: false });
    lines.push(sg::Line { at_ms: NOW + 20_000, speaker: "User".into(), text: "I remember there was a photo from the trip".into(), owner: true });
    let mut budget = Budget::default();
    let out = run(&lines, None, &mut budget);
    assert_eq!(out.len(), 2, "both cues fire: nothing is paced without a call");
    assert!(out.iter().all(|d| d.outcome == "trigger" && !d.called && d.usd == 0.0));
    assert_eq!(budget, Budget::default(), "no call, no spend");
}
