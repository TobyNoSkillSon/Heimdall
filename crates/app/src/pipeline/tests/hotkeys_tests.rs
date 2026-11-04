//! 29 Oct 22:27-22:32, the user's first live evening on Decider V5, end to end through the real `Pipeline` with stubbed
//! models: after a goodbye in the Discord call his lines to Heimdall wake again (they were held as call chatter for two
//! minutes), and his two new keys wake Heimdall (⌃⌘\) or ask for a suggestion (⌃⌘') with a label for the decider.
use super::plumbing_tests::{rig, turn, Detect};
use super::*;

const S: u64 = 1_000_000_000;

fn decisions(a: &Pipeline) -> Vec<serde_json::Value> {
    a.index.evidence.records().unwrap().into_iter().filter(|r| r.kind == "send_decision").map(|r| r.payload).collect()
}
fn labels(a: &Pipeline) -> Vec<serde_json::Value> {
    std::fs::read_to_string(a.labels_path()).unwrap_or_default().lines().map(|l| serde_json::from_str(l).unwrap()).collect()
}
fn discord(a: &Pipeline) -> String {
    a.sources.iter().find(|(_, s)| crate::context_policy::is_call(s)).map(|(id, _)| id.clone()).expect("the rig's Discord source")
}
/// A named participant's line on the call (as `policy_night1022_tests`), returning its end.
fn call_line(a: &mut Pipeline, source: &str, speaker: &str, text: &str, at: u64) -> u64 {
    let words = super::plumbing_tests::timed(text, at);
    let end = words.last().unwrap().end_ns;
    a.audio.insert(source.into(), AudioRing { start: at.saturating_sub(S), samples: vec![0.01; ((end + S - at.saturating_sub(S)) / 62500) as usize].into() });
    a.listen(ListenEvent::SpeechStart { source: source.into(), host_time_ns: at }).unwrap();
    a.listen(ListenEvent::FinalUtterance { source: source.into(), text: text.into(), words, start_ns: at, end_ns: end, forced: false, end_of_turn_probability: 0.9 }).unwrap();
    let id = *a.awaiting_speaker.iter().find(|(_, f)| f.observation.text.starts_with(text)).map(|(id, _)| id).expect("waiting for its speaker");
    let mut final_ = a.awaiting_speaker.remove(&id).unwrap();
    final_.observation.speaker = speaker.into();
    final_.turn.role = decide::Role::Other(speaker.into());
    a.hooks.send_probs.push_back((0.01, 0.0, 0.99));
    a.give_to_agent(final_).unwrap();
    end
}

/// 22:27:49-22:29:24: Skyline "All right, man.", the user "… See ya, sir.", Skyline "See ya.", then the user to Heimdall 19 s
/// later with V5 at send 0.93. Before the fix it was `held_in_call` (the call window ran two more minutes); now the
/// goodbye ended the call, so V5's send wakes and replies are spoken again.
#[test]
fn after_a_goodbye_his_lines_to_heimdall_wake() {
    let (_t, mut a, mic) = rig(false);
    let call = discord(&a);
    call_line(&mut a, &call, "Skyline", "All right, man.", 5 * S);
    a.hooks.send_probs.push_back((0.01, 0.0, 0.99));
    turn(&mut a, &mic, 8 * S, "Anyways, I am going to the shop now. I need to buy it. See ya, sir.", Detect::Final, 0, None, 3 * S, |_| {});
    let bye = call_line(&mut a, &call, "Skyline", "See ya.", 14 * S);
    assert!(!a.tools.speech_policy.in_conversation(bye + 19 * S), "replies are spoken again after the goodbye");
    a.hooks.send_probs.push_back((0.93, 0.0, 0.06));
    turn(&mut a, &mic, bye + 17 * S, "Oh, you can talk to me. How much? Hello.", Detect::Final, 0, None, 3 * S, |_| {});
    a.agent.as_ref().unwrap().barrier().unwrap();
    let last = decisions(&a).last().cloned().unwrap();
    assert_eq!((last["outcome"].as_str(), last["trigger"].as_str()), (Some("send"), Some("d1")), "{last:#}");
    // Someone speaks in the call again: it is a call again, and his chat is held as before.
    call_line(&mut a, &call, "Skyline", "Wait, are you still there?", bye + 40 * S);
    a.hooks.send_probs.push_back((0.9, 0.0, 0.1));
    turn(&mut a, &mic, bye + 45 * S, "Yeah I'm here, what's up?", Detect::Final, 0, None, 3 * S, |_| {});
    let last = decisions(&a).last().cloned().unwrap();
    assert_eq!(last["outcome"], "held_in_call", "{last:#}");
}

/// ⌃⌘\ on a line V5 kept (22:28:47 "I feel like you have a lot of problems…", here not_for_heimdall): it wakes at once
/// with what was said since the last wake, and the line is labelled `send` with V5's exact input and its own answer.
#[test]
fn the_wake_key_sends_unsent_lines_and_labels_them() {
    let (_t, mut a, mic) = rig(false);
    a.hooks.send_probs.push_back((0.2, 0.05, 0.75));
    let text = "Okay, hello. I feel like you have a lot of problems with understanding when you should wake up.";
    turn(&mut a, &mic, 10 * S, text, Detect::Final, 0, None, 3 * S, |_| {});
    let d = decisions(&a).last().cloned().unwrap();
    assert_eq!(d["outcome"], "not_for_heimdall");
    assert_eq!(a.dispatched_through, 0, "V5 kept it");
    a.action(ui::UiAction::ForceWake).unwrap();
    assert!(a.dispatched_through > 0, "the key woke the agent");
    let l = labels(&a);
    assert_eq!(l.len(), 1, "{l:#?}");
    assert_eq!((l[0]["task"].as_str(), l[0]["label"].as_str(), l[0]["source"].as_str()), (Some("send"), Some("send"), Some("owner-hotkey")));
    assert_eq!(l[0]["text"], text);
    assert_eq!(l[0]["decider_input"], d["prompt"], "exactly what V5 saw");
    assert_eq!(l[0]["v5"]["choice"], "not_for_heimdall");
    assert_eq!(l[0]["v5"]["probabilities"], d["probabilities"]);
    assert_eq!(l[0]["woke"], true);
    assert!(a.labels_path().starts_with(crate::archive::store_root(&a.session)), "in the archive");
    // Pressed again with nothing new: no wake, no second label.
    a.action(ui::UiAction::ForceWake).unwrap();
    assert_eq!(labels(&a).len(), 1);
}

/// ⌃⌘' labels the current line `suggest` with V5's suggest input and answer (asked on the snapshot V5 saw), and asks
/// for a pass now.
#[test]
fn the_suggest_key_labels_the_current_line() {
    let (_t, mut a, mic) = rig(false);
    a.hooks.send_probs.push_back((0.05, 0.0, 0.95));
    turn(&mut a, &mic, 10 * S, "I wonder which codec keeps the voice tracks separate.", Detect::Final, 0, None, 3 * S, |_| {});
    a.hooks.suggest.push_back(0.12);
    a.action(ui::UiAction::ForceSuggest).unwrap();
    let l = labels(&a);
    assert_eq!(l.len(), 1, "{l:#?}");
    assert_eq!((l[0]["task"].as_str(), l[0]["label"].as_str(), l[0]["source"].as_str()), (Some("suggest"), Some("yes"), Some("owner-hotkey")));
    assert_eq!(l[0]["text"], "I wonder which codec keeps the voice tracks separate.");
    assert_eq!(l[0]["v5"]["yes"].as_f64().map(|y| (y * 100.).round()), Some(12.));
    assert!(!l[0]["decider_input"].is_null());
}

/// Live 29 Oct 23:00: the first press after a relaunch carried a line heard before it, whose decision was not in
/// memory; it woke but wrote no label. Now it is labelled at once, marked without V5's input.
#[test]
fn a_line_from_before_a_relaunch_is_labelled_without_its_decision() {
    let (_t, mut a, mic) = rig(false);
    a.hooks.send_probs.push_back((0.2, 0.05, 0.75));
    turn(&mut a, &mic, 10 * S, "Are you there?", Detect::Final, 0, None, 3 * S, |_| {});
    a.hotkeys = Default::default(); // the relaunch: the decision is only in the archive
    a.action(ui::UiAction::ForceWake).unwrap();
    let l = labels(&a);
    assert_eq!(l.len(), 1, "{l:#?}");
    assert_eq!((l[0]["text"].as_str(), l[0]["speaker"].as_str(), l[0]["woke"].as_bool()), (Some("Are you there?"), Some("User"), Some(true)));
    assert!(l[0]["decider_input"].is_null() && l[0]["decision"].as_str().is_some_and(|d| d.starts_with("unavailable")));
}
