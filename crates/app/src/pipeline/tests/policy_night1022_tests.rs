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
    // Labelled with a person's name (sources.rs): a live call (night 1115).
    a.tools.speech_policy.call_line(end, true);
    a.give_to_agent(final_).unwrap();
}
fn outcome(d: &serde_json::Value) -> (String, Option<String>, Option<String>) {
    (d["outcome"].as_str().unwrap_or("").into(), d["trigger"].as_str().map(str::to_string), d["d1_gate"]["gate"].as_str().map(str::to_string))
}
fn pending(a: &Pipeline) -> String {
    a.tools.send_context.lock().unwrap().pending_text()
}

/// Bug note 21 Oct 15:40: with d1 saying `send` every time, Atta's lines stay context and wake nothing. Night 1115
/// ("Whenever I'm saying something, it should receive in the same way"): the user's own line gets d1's
/// verdict as off a call, so its send wakes; his name wakes as always.
#[test]
fn during_a_call_other_voices_never_wake_by_d1_and_his_lines_are_judged_as_off_a_call() {
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
        ("held_not_user".into(), None, Some("not_user".into())),
        ("send".into(), Some("d1".into()), None),
        ("held_not_user".into(), None, Some("not_user".into())),
        ("send".into(), Some("name".into()), None),
        ("send".into(), Some("d1".into()), None),
    ], "{all:#?}");
}

/// Bug note 21 Oct 00:07 (night pass 3 finding): "…what I would want is I would want them to be always in front…"
/// scored not_for_heimdall 0.47499 against send 0.47497, agent idle, no call, and never woke the agent. A near-tie
/// like that sends now; a clear not_for_heimdall does not. Night 1115: inside a call the same near-tie sends too.
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
        ("send".into(), Some("d1".into()), Some("near_tie".into())),
    ], "{owner:#?}");
}

// Barge-in through speakers (bug note 21 Oct 17:44:10) is judged by voice now: heimdall_voice_tests.rs.

/// (the VM's background chat "Did you catch the game last night?" woke the agent, d1 send 0.86):
/// until the user's voice is known, d1 alone never wakes the agent, only the wake name does ("Hindor" too, as the
/// ASR hears Heimdall); once it is known (Setup's phrase), d1's send wakes again.
#[test]
fn without_a_known_user_voice_only_the_wake_name_wakes() {
    let (_t, mut a, mic) = rig(false);
    a.hooks.no_user_voice = true;
    a.hooks.send_choices.push_back(decide::send::Choice::Send);
    turn(&mut a, &mic, 5 * S, "Did you catch the game last night?", Detect::Final, 0, None, 3 * S, |_| {});
    turn(&mut a, &mic, 20 * S, "Hindor, what time is it?", Detect::Final, 0, Some(0.), 3 * S, |_| {});
    let e = speakers::embedding::Embedding::new(vec![1.; speakers::embedding::DIMENSION]).unwrap();
    a.heimdall_voice.user.enrol(e, 1);
    a.hooks.send_choices.push_back(decide::send::Choice::Send);
    turn(&mut a, &mic, 300 * S, "Can you open the build log?", Detect::Final, 0, None, 3 * S, |_| {});
    let all: Vec<_> = decisions(&a).iter().map(outcome).collect();
    assert_eq!(all, [
        ("held_no_user_voice".into(), None, Some("no_user_voice".into())),
        ("send".into(), Some("name".into()), None),
        ("send".into(), Some("d1".into()), None),
    ], "{all:#?}");
}

/// Bug note 2026-11-05T02-32-17 items 3 and 5 (night 1111). 9 Nov 16:15:09 in a Discord call: "Can you Heimdall, can
/// you open the skillful Tony comparison?" (the name mid-sentence) waited for d1, which said send 0.98, and woke the
/// agent; its answer was held silent "in a call" and dropped stale 60 s later, as only a name opening a sentence counted
/// as his address. d1's send confirms it: the run may answer aloud (in a gap). A mention d1 does not read as send
/// still wakes past the call rule but stays held.
#[test]
fn in_a_call_his_name_mid_sentence_that_d1_sends_may_be_answered_aloud() {
    let (_t, mut a, mic) = rig(false);
    let call = discord(&a);
    // Host-clock times a little in the past: the call is still live at `now`.
    let t0 = capture::host_now_ns().saturating_sub(40 * S);
    named_call_line(&mut a, &call, "Vim", "Are you gonna make this available for Windows?", t0 + 5 * S);
    a.hooks.send_probs.push_back((0.98, 0.01, 0.01));
    // The dry-run agent's run is held open until the address is read (it settled first 1 in 3, 13 Nov night).
    a.tools.speech_policy.test_hold_settle();
    turn(&mut a, &mic, t0 + 10 * S, "Which is going to be uh oh, I closed it. Can you Heimdall, can you open the skillful Tony comparison?", Detect::Final, 0, None, 3 * S, |_| {});
    let addressed = a.tools.speech_policy.addressed_now();
    let held = a.tools.speech_policy.speech_held(capture::host_now_ns());
    let d = decisions(&a).last().cloned().unwrap();
    assert_eq!(outcome(&d), ("send".into(), Some("name".into()), None), "{d:#}");
    let now = capture::host_now_ns();
    assert!(a.tools.speech_policy.in_conversation(now), "the call is live");
    // The address holds for the whole run: the answer may be said.
    assert!(addressed && !held, "his address by name lets the answer be said");

    // The run settles; a mention of the name wakes at once (night 1115: no wait for d1 in a call), and as his words
    // woke the run, its answer may be said.
    a.tools.speech_policy.test_release_settle();
    a.tools.speech_policy.run_settled();
    named_call_line(&mut a, &call, "Vim", "So what does it do?", t0 + 25 * S);
    a.hooks.send_probs.push_back((0.3, 0.6, 0.1));
    turn(&mut a, &mic, t0 + 30 * S, "It does what Heimdall does but on Windows maybe.", Detect::Final, 0, None, 3 * S, |_| {});
    let d = decisions(&a).last().cloned().unwrap();
    assert_eq!(outcome(&d), ("send".into(), Some("name".into()), None), "{d:#}");
    assert!(a.tools.speech_policy.addressed_now(), "his words woke it: answered aloud as off a call");
}

/// Bug note 2026-11-05T02-32-17 items 3 and 5 (night 1111): 9 Nov 14:16:14 and 14:23:36, the user pressed ⌃⌘\ in a Discord
/// call on a line the call gate held. Night 1115: no call gate; d1's send wakes and the answer may be said aloud.
#[test]
fn in_a_call_his_line_d1_sends_wakes_and_the_answer_may_be_said() {
    let (_t, mut a, mic) = rig(false);
    let call = discord(&a);
    // Host-clock times a little in the past: the call is still live at `now`.
    let t0 = capture::host_now_ns().saturating_sub(40 * S);
    named_call_line(&mut a, &call, "Alex", "Add Elon Musk to it.", t0 + 5 * S);
    a.hooks.send_probs.push_back((0.9, 0.05, 0.05));
    // The dry-run agent's run is held open until the address is read.
    a.tools.speech_policy.test_hold_settle();
    turn(&mut a, &mic, t0 + 10 * S, "Okay, do it. Add Elon Musk and the DeepSeek guy.", Detect::Final, 0, None, 3 * S, |_| {});
    assert_eq!(outcome(decisions(&a).last().unwrap()).0, "send");
    assert!(a.dispatched_through > 0, "d1's send woke the agent");
    let now = capture::host_now_ns();
    assert!(a.tools.speech_policy.in_conversation(now) && !a.tools.speech_policy.speech_held(now), "the answer may be said in the call");
    a.tools.speech_policy.test_release_settle();
}

/// Night 1115, the decider misses of 14 Nov 19:01–19:03: his friend had left at 18:44 and only a foreign-language
/// stream played in Discord ("Speaker Sat 14 Nov 18h #1"). It kept him "in a call", so "Gangdal, hello." (model send
/// 0.97) and his 19:03 complaint were held. Now an unnamed voice he does not talk with is not a call, his misheard
/// name wakes, and his lines get the decider's verdict as off a call (the user 19:03).
#[test]
fn a_stream_in_discord_holds_neither_his_lines_nor_the_answers() {
    let (_t, mut a, mic) = rig(false);
    let call = discord(&a);
    let t0 = capture::host_now_ns().saturating_sub(40 * S);
    turn(&mut a, &call, t0, "No a potom sme išli do mesta a bolo to super.", Detect::Final, 0, None, 3 * S, |_| {});
    turn(&mut a, &call, t0 + 8 * S, "Takže dnes večer to skúsime znova.", Detect::Final, 0, None, 3 * S, |_| {});
    assert!(!a.tools.speech_policy.in_conversation(capture::host_now_ns()), "a stream is not a call");
    a.tools.speech_policy.test_hold_settle();
    a.hooks.send_probs.push_back((0.97, 0.0, 0.03));
    turn(&mut a, &mic, t0 + 15 * S, "Gangdal, hello.", Detect::Final, 0, Some(0.), 3 * S, |_| {});
    let d = decisions(&a).last().cloned().unwrap();
    assert_eq!(outcome(&d), ("send".into(), Some("name".into()), None), "{d:#}");
    assert!(a.tools.speech_policy.addressed_now() && !a.tools.speech_policy.speech_held(capture::host_now_ns()), "answered aloud");
    a.tools.speech_policy.test_release_settle();
    a.tools.speech_policy.run_settled();
    a.hooks.send_probs.push_back((0.9, 0.05, 0.05));
    turn(&mut a, &mic, t0 + 25 * S, "I think the problem is that Heimdall still thinks that I am in the voice call.", Detect::Final, 0, None, 3 * S, |_| {});
    let d = decisions(&a).iter().filter(|d| d["audio"]["source_id"] == mic.as_str()).last().cloned().unwrap();
    assert_eq!(d["outcome"], "send", "{d:#}");
}
