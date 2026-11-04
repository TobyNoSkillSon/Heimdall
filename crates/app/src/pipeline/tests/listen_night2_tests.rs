//! Night review 20→21 Oct, listen part, end to end through the real `Pipeline` with stubbed models: words after a
//! capture gap reach the agent (L1), quick-command phrases inside statements raise no cue (D4), a heard cue always
//! leaves the island (I1), and triggers during a call wait for d1 (D3). Lines are the user's own from the 20 Oct archive.
use super::plumbing_tests::{rig, turn, Detect};
use super::*;
use std::time::Duration;

const S: u64 = 1_000_000_000;

fn timed(text: &str, start: u64) -> Vec<Word> {
    text.split_whitespace()
        .enumerate()
        .map(|(i, w)| Word { text: w.into(), start_ns: start + i as u64 * 300_000_000, end_ns: start + i as u64 * 300_000_000 + 250_000_000, confidence: 1. })
        .collect()
}
fn records(a: &Pipeline) -> Vec<heimdall_archive::Record> {
    a.index.evidence.records().unwrap()
}
fn decisions(a: &Pipeline) -> Vec<serde_json::Value> {
    records(a).into_iter().filter(|r| r.kind == "send_decision").map(|r| r.payload).collect()
}
fn cues(a: &Pipeline) -> Vec<ui::CommandCue> {
    a.hooks.ui.lock().unwrap().iter().filter_map(|u| match u { ui::UiUpdate::Command(c) => Some(c.clone()), _ => None }).collect()
}
fn discord(a: &Pipeline) -> String {
    a.sources.iter().find(|(_, s)| crate::context_policy::is_call(s)).map(|(id, _)| id.clone()).expect("the rig's Discord source")
}
/// A line from another participant on the call, ending at about `at` + its words.
fn call_line(a: &mut Pipeline, source: &str, text: &str, at: u64) {
    let words = timed(text, at);
    let end = words.last().unwrap().end_ns;
    a.audio.insert(source.into(), AudioRing { start: at.saturating_sub(S), samples: vec![0.01; ((end + S - at.saturating_sub(S)) / 62500) as usize].into() });
    a.listen(ListenEvent::SpeechStart { source: source.into(), host_time_ns: at }).unwrap();
    a.listen(ListenEvent::FinalUtterance { source: source.into(), text: text.into(), words, start_ns: at, end_ns: end, forced: false, end_of_turn_probability: 0.9 }).unwrap();
}

/// L1 (20 Oct 18:28:19): a capture clock gap lost the waveform of "Uh whether we can run on", the continuation of
/// "…I want you to figure out what's". It was archived with "utterance waveform unavailable after capture gap" and
/// never given to the agent, which answered "Your sentence cut off after 'figure out what's'". The decider reads text
/// only, so the words go on as text, with the gap noted.
#[test]
fn words_after_a_capture_gap_reach_the_agent_as_text() {
    let (_t, mut a, mic) = rig(false);
    turn(&mut a, &mic, 10 * S, "Uh I want you to spawn a thread uh in T3 Code and I want you to figure out what's", Detect::Final, 0, Some(0.), 0, |_| {});
    // The capture clock jumped: the ring restarted after these words were spoken.
    let words = timed("Uh whether we can run on", 40 * S);
    let end = words.last().unwrap().end_ns;
    a.audio.insert(mic.clone(), AudioRing { start: end + S, samples: vec![0.01; 16000].into() });
    a.listen(ListenEvent::SpeechStart { source: mic.clone(), host_time_ns: 40 * S }).unwrap();
    a.hooks.send.push_back(0.);
    a.listen(ListenEvent::FinalUtterance { source: mic.clone(), text: "Uh whether we can run on".into(), words, start_ns: 40 * S, end_ns: end, forced: false, end_of_turn_probability: 0.9 }).unwrap();
    a.agent.as_ref().unwrap().barrier().unwrap();
    let d = decisions(&a);
    assert_eq!(d.len(), 2, "both lines reached the decider");
    assert!(d[1]["audio"]["unavailable"].as_str().is_some_and(|e| e.contains("capture gap")), "{}", d[1]["audio"]);
    assert!(a.pending_text.iter().any(|t| t.contains("whether we can run on")), "{:?}", a.pending_text);
    assert!(records(&a).iter().any(|r| r.kind == "agent_context_routing" && r.payload["given_to_agent"] == true
        && r.payload["observation"]["text"].as_str().is_some_and(|t| t.contains("whether we can run on"))));
    let rows = a.index.utterances_between(0, 100 * S).unwrap();
    let gap = rows.iter().find(|r| r["text"].as_str().is_some_and(|t| t.contains("whether we can run on"))).unwrap();
    assert_ne!(gap["decision"]["status"], "unavailable_audio");
    assert_eq!(gap["decision"]["given_to_agent"], true);
}

/// D4: the 18:39:46 line to Morgan raised a "look at this" cue; the 19:14:43 line a "take a screenshot" cue. Inside a
/// statement neither phrase is a command now: no candidate, no heard glyph. The user's own command still is.
#[test]
fn command_phrases_inside_statements_raise_no_heard_cue() {
    let (_t, mut a, mic) = rig(false);
    for (i, text) in [
        "So yeah, I wouldn't even look at this. Yeah, Heimdal, we are not doing this.",
        "Uh, especially when we uh like when I take a screenshot for him to decide whether to send it or not?",
    ]
    .into_iter()
    .enumerate()
    {
        turn(&mut a, &mic, (10 + 40 * i as u64) * S, text, Detect::Final, 0, Some(0.), 3 * S, |_| {});
    }
    assert!(cues(&a).is_empty(), "{:?}", cues(&a));
    assert!(!records(&a).iter().any(|r| r.kind == "command_decision"));
    turn(&mut a, &mic, 100 * S, "Yo, Heimdall, look at this.", Detect::Final, 0, Some(0.), 3 * S, |_| {});
    assert!(matches!(cues(&a).first(), Some(ui::CommandCue::Heard { kind: ui::CommandKind::LookAtThis, .. })), "{:?}", cues(&a));
}

/// I1: a command whose verdict arrives after its turn is gone (a capture gap ended the utterance while the decider
/// worked — 18:28:19 had a gap during a 3.7 s command decision) drops its heard cue, and the record says why. Before,
/// the verdict returned silently and the glyph rested at the notch until something else cleared it.
#[test]
fn a_verdict_after_its_turn_ended_drops_the_heard_cue() {
    let (_t, mut a, mic) = rig(false);
    // No injected command answer: the verdict goes through the real inference owner (models missing: it fails late).
    turn(&mut a, &mic, 10 * S, "Okay, look at this.", Detect::Final, 0, Some(0.), 3 * S, |_| {});
    let heard = cues(&a);
    let id = match heard.first() { Some(ui::CommandCue::Heard { id, .. }) => *id, other => panic!("{other:?}") };
    assert_eq!(a.command_inflight.get(&mic).copied(), Some(1), "the verdict is still out");
    // A capture gap ends the utterance before the verdict arrives.
    a.gap(&mic, 20 * S, "fixture gap");
    assert!(!a.active.contains_key(&mic));
    for _ in 0..50 {
        if a.command_inflight.get(&mic).copied().unwrap_or(0) == 0 {
            break;
        }
        let e = a.internal.1.recv_timeout(Duration::from_secs(5)).expect("the verdict arrives");
        a.internal(e).unwrap();
    }
    assert_eq!(a.command_inflight.get(&mic).copied().unwrap_or(0), 0);
    let after: Vec<_> = cues(&a).into_iter().skip(1).collect();
    assert_eq!(after, [ui::CommandCue::Dropped { id }], "the heard cue's one exit");
    assert!(records(&a).iter().any(|r| r.kind == "command_decision" && r.payload["reason"] == "turn_ended_before_verdict"));
}

/// D3 (20 Oct, Discord call with Morgan): while a call is going on, a mention of the name and the follow-up window wait
/// for d1, which holds them back at not_for_heimdall >= 0.5; the user addressing Heimdall by name still wakes at once.
/// After the call (no call line for two minutes) the name wakes as before.
#[test]
fn during_a_call_mentions_wait_for_d1_and_an_address_still_wakes() {
    let (_t, mut a, mic) = rig(false);
    let call = discord(&a);
    let mention = "Oh my god dude. I so I'm running like like remember Heimdall? Like the the thing I was doing?";
    call_line(&mut a, &call, "So I can upgrade.", 5 * S);
    // 1. A mention, d1 strongly not for Heimdall: no wake.
    a.hooks.send_choices.push_back(decide::send::Choice::NotForHeimdall);
    turn(&mut a, &mic, 10 * S, mention, Detect::Final, 0, None, 3 * S, |_| {});
    // 2. The same mention, d1 not against it: it wakes on the name.
    call_line(&mut a, &call, "Move it, move it.", 40 * S);
    turn(&mut a, &mic, 45 * S, mention, Detect::Final, 0, Some(0.), 3 * S, |_| {});
    // 3. An address, d1 strongly not for Heimdall (18:39:46 scored 0.77): it wakes at once on the name.
    call_line(&mut a, &call, "Yeah.", 80 * S);
    a.hooks.send_choices.push_back(decide::send::Choice::NotForHeimdall);
    turn(&mut a, &mic, 85 * S, "So yeah, I wouldn't even look at this. Yeah, Heimdal, we are not doing this.", Detect::Final, 0, None, 3 * S, |_| {});
    // 4. Three minutes after the last call line: the call rule is off, the mention wakes as before.
    a.hooks.send_choices.push_back(decide::send::Choice::NotForHeimdall);
    turn(&mut a, &mic, 300 * S, mention, Detect::Final, 0, None, 3 * S, |_| {});
    a.agent.as_ref().unwrap().barrier().unwrap();
    let all = decisions(&a);
    let outcome = |d: &serde_json::Value| (d["outcome"].as_str().unwrap_or("").to_string(), d["trigger"].as_str().map(str::to_string), d["call_gate"]["wakes"].as_bool());
    let owner: Vec<_> = all.iter().filter(|d| d["audio"]["source_id"] == mic.as_str()).map(outcome).collect();
    assert_eq!(owner, [
        ("not_for_heimdall".into(), None, Some(false)),
        ("send".into(), Some("name".into()), Some(true)),
        ("send".into(), Some("name".into()), None),
        ("send".into(), Some("name".into()), None),
    ], "{owner:#?}");
}
