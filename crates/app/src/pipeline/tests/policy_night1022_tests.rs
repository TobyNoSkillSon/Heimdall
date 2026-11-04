//! Night pass 1022, policy part, end to end through the real `Pipeline` with stubbed models: d1's own send never wakes
//! the agent on a call participant's line or on the user's lines during a call unless he names Heimdall (bug note 21 Oct
//! 15:40:05); a near-tie of send with not_for_heimdall outside a call sends (bug note 21 Oct 00:07:17); through
//! speakers Heimdall's own voice on the microphone no longer stops it, and the user still can (bug note 21 Oct 17:44:10).
use super::plumbing_tests::{rig, turn, Detect};
use super::*;

const S: u64 = 1_000_000_000;

fn timed(text: &str, start: u64) -> Vec<Word> {
    text.split_whitespace()
        .enumerate()
        .map(|(i, w)| Word { text: w.into(), start_ns: start + i as u64 * 300_000_000, end_ns: start + i as u64 * 300_000_000 + 250_000_000, confidence: 1. })
        .collect()
}
fn decisions(a: &Pipeline) -> Vec<serde_json::Value> {
    a.index.evidence.records().unwrap().into_iter().filter(|r| r.kind == "send_decision").map(|r| r.payload).collect()
}
fn discord(a: &Pipeline) -> String {
    a.sources.iter().find(|(_, s)| crate::context_policy::is_call(s)).map(|(id, _)| id.clone()).expect("the rig's Discord source")
}
/// A line from a named participant on the call: heard on the call source, then labelled (`sources.rs`: a label
/// gives the waiting final to the agent with the participant's name), with d1 saying `send` for it.
fn named_call_line(a: &mut Pipeline, source: &str, speaker: &str, text: &str, at: u64) {
    let words = timed(text, at);
    let end = words.last().unwrap().end_ns;
    a.audio.insert(source.into(), AudioRing { start: at.saturating_sub(S), samples: vec![0.01; ((end + S - at.saturating_sub(S)) / 62500) as usize].into() });
    a.listen(ListenEvent::SpeechStart { source: source.into(), host_time_ns: at }).unwrap();
    a.listen(ListenEvent::FinalUtterance { source: source.into(), text: text.into(), words, start_ns: at, end_ns: end, forced: false, end_of_turn_probability: 0.9 }).unwrap();
    let id = *a.awaiting_speaker.iter().find(|(_, f)| f.observation.text.starts_with(text)).map(|(id, _)| id).expect("waiting for its speaker");
    let mut final_ = a.awaiting_speaker.remove(&id).unwrap();
    final_.observation.speaker = speaker.into();
    final_.turn.role = decide::Role::Other(speaker.into());
    a.hooks.send_choices.push_back(decide::send::Choice::Send);
    a.give_to_agent(final_).unwrap();
}
fn outcome(d: &serde_json::Value) -> (String, Option<String>, Option<String>) {
    (d["outcome"].as_str().unwrap_or("").into(), d["trigger"].as_str().map(str::to_string), d["d1_gate"]["gate"].as_str().map(str::to_string))
}
fn pending(a: &Pipeline) -> String {
    a.tools.send_context.lock().unwrap().pending_text()
}

/// Bug note 21 Oct 15:40 (the Discord call 15:38–15:40, seven wakes, all NO_REPLY): with d1 saying `send` every time,
/// Atta's lines and the user's chat with them stay context and wake nothing; the user addressing Heimdall by name wakes, and
/// that wake carries them. After the call, d1's send wakes again.
#[test]
fn during_a_call_d1_alone_never_wakes_the_agent() {
    let (_t, mut a, mic) = rig(false);
    let call = discord(&a);
    named_call_line(&mut a, &call, "Atta", "No, I don't pull bro.", 5 * S);
    a.hooks.send_choices.push_back(decide::send::Choice::Send);
    turn(&mut a, &mic, 10 * S, "Yeah but like the whole thing is automated now.", Detect::Final, 0, None, 3 * S, |_| {});
    named_call_line(&mut a, &call, "Atta", "Oh, it's insane.", 20 * S);
    let context = pending(&a);
    assert!(context.contains("No, I don't pull bro.") && context.contains("the whole thing is automated now") && context.contains("Oh, it's insane."), "they stay in context: {context}");
    // The user names Heimdall: it wakes at once, before d1.
    turn(&mut a, &mic, 30 * S, "Heimdall, what did Atta just say about pulling?", Detect::Final, 0, Some(0.), 3 * S, |_| {});
    // Three minutes after the last call line: d1's send wakes again.
    a.hooks.send_choices.push_back(decide::send::Choice::Send);
    turn(&mut a, &mic, 300 * S, "Can you open the build log?", Detect::Final, 0, None, 3 * S, |_| {});
    a.agent.as_ref().unwrap().barrier().unwrap();
    let all: Vec<_> = decisions(&a).iter().map(outcome).collect();
    assert_eq!(all, [
        ("held_not_owner".into(), None, Some("not_owner".into())),
        ("held_in_call".into(), None, Some("in_call".into())),
        ("held_not_owner".into(), None, Some("not_owner".into())),
        ("send".into(), Some("name".into()), None),
        ("send".into(), Some("d1".into()), None),
    ], "{all:#?}");
}

/// Bug note 21 Oct 00:07 (night pass 3 finding): "…what I would want is I would want them to be always in front…"
/// scored not_for_heimdall 0.47499 against send 0.47497, agent idle, no call, and never woke the agent. A near-tie
/// like that sends now; a clear not_for_heimdall does not, and inside a call the same near-tie stays held.
#[test]
fn a_near_tie_outside_a_call_sends() {
    let (_t, mut a, mic) = rig(false);
    let request = "Okay so for the floating islands what I would want is I would want them to be always in front of every window.";
    a.hooks.send_probs.push_back((0.47497, 0.05004, 0.47499));
    turn(&mut a, &mic, 10 * S, request, Detect::Final, 0, None, 3 * S, |_| {});
    a.hooks.send_probs.push_back((0.30, 0.05, 0.65));
    turn(&mut a, &mic, 100 * S, "Yeah that was a funny video.", Detect::Final, 0, None, 3 * S, |_| {});
    let call = discord(&a);
    named_call_line(&mut a, &call, "Angel", "What are you doing tonight?", 200 * S);
    a.hooks.send_probs.push_back((0.430, 0.130, 0.440));
    turn(&mut a, &mic, 205 * S, "Probably only math under the lecture.", Detect::Final, 0, None, 3 * S, |_| {});
    a.agent.as_ref().unwrap().barrier().unwrap();
    let owner: Vec<_> = decisions(&a).iter().filter(|d| d["audio"]["source_id"] == mic.as_str()).map(outcome).collect();
    assert_eq!(owner, [
        ("send".into(), Some("d1".into()), Some("near_tie".into())),
        ("not_for_heimdall".into(), None, None),
        ("not_for_heimdall".into(), None, Some("in_call".into())),
    ], "{owner:#?}");
}

// Barge-in through speakers (bug note 21 Oct 17:44:10) is judged by voice now: heimdall_voice_tests.rs.
