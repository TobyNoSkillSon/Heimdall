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

/// Speech that plays until cut, for the Stop tests: each line reports its first sample on the channel.
struct Synth;
impl speak::Synthesizer for Synth {
    fn synthesize(&mut self, _: &str, _: &str, _: &dyn Fn() -> bool) -> Result<speak::Audio> {
        Ok(speak::Audio { samples: vec![0.1; 160], sample_rate: 16_000, first_audio_ms: 0., synthesis_ms: 0. })
    }
}
struct Blocking(std::sync::mpsc::Sender<()>);
impl speak::AudioOutput for Blocking {
    fn output(&mut self, _: &speak::Audio, cancel: &dyn Fn() -> bool) -> Result<()> {
        let _ = self.0.send(());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while !cancel() && std::time::Instant::now() < deadline { std::thread::sleep(std::time::Duration::from_millis(2)); }
        Ok(())
    }
}
/// The agent's `speak` tool: the path a run's reply takes (`mcp_actions::Host::speak`).
fn say(a: &Pipeline, text: &str, op: &str) -> String {
    a.tools.mcp_actions.speak(&a.tools, text, "en", op, &a.tools.agent_enabled.permit().unwrap()).unwrap()
}
/// The agent on, with the fake Pi's reply held until `release` (so the run is still running while it speaks), and a
/// speech queue that plays until cut. The user's `text` was sent by V5 and woke the run.
fn speaking_rig(text: &str, probs: (f32, f32, f32)) -> (tempfile::TempDir, Pipeline, std::sync::mpsc::Receiver<()>, PathBuf) {
    let (t, mut a, mic) = rig(true);
    // Started first: a new agent process begins with no run (`run_settled`), which would forget the wake's line.
    a.ensure_agent().unwrap();
    a.agent.as_ref().unwrap().barrier().unwrap();
    let hold = a.data.join("hold-reply");
    std::fs::write(&hold, b"hold").unwrap();
    a.hooks.send_probs.push_back(probs);
    turn(&mut a, &mic, 10 * S, text, Detect::Final, 0, None, 3 * S, |_| {});
    if let Some(agent) = a.agent.as_ref() { agent.barrier().unwrap(); }
    let (started, playing) = std::sync::mpsc::channel();
    a.tools.test_guarded_speech_queue(Synth, Blocking(started));
    (t, a, playing, hold)
}
/// Releases the fake Pi's held reply and waits for every wake's turn to complete.
fn release(a: &Pipeline, hold: &std::path::Path) {
    let _ = std::fs::remove_file(hold);
    a.agent.as_ref().unwrap().barrier().unwrap();
    let began = std::time::Instant::now();
    loop {
        let r = a.index.evidence.records().unwrap();
        let wakes = r.iter().filter(|r| r.kind == "agent_wake").count();
        let done = r.iter().filter(|r| r.kind == "agent_reply" && r.payload["event"]["kind"] == "complete" && r.payload["event"]["query"] == true).count();
        if done >= wakes { break; }
        assert!(began.elapsed() < std::time::Duration::from_secs(30), "a wake's turn never completed");
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}
fn stop_records(a: &Pipeline) -> Vec<serde_json::Value> {
    a.index.evidence.records().unwrap().into_iter().filter(|r| r.kind == "hotkey" && r.payload["key"] == "stop").map(|r| r.payload).collect()
}

/// Bug notes 2026-11-05T00-10-37 and 2026-11-05T02-32-17 (item 4): the island's Stop while Heimdall speaks stops the
/// voice and labels the line V5 woke the speaking run for `not_for_heimdall` ("this moment wasn't for Heimdall").
/// Review 5 Nov: only while speech plays (a press with nothing playing labels nothing), and once per line: a second
/// press on the same run, even with its speech playing again after the user's typed words, adds no label.
#[test]
fn the_stop_button_labels_the_line_that_woke_the_agent_not_for_heimdall() {
    let text = "I think that makes quite a lot of sense.";
    let (_t, mut a, playing, hold) = speaking_rig(text, (0.95, 0.0, 0.05));
    let d = decisions(&a).last().cloned().unwrap();
    assert_eq!(d["outcome"], "send", "{d:#}");
    a.action(ui::UiAction::StopSpeaking).unwrap();
    assert!(labels(&a).is_empty(), "nothing playing: nothing labelled");
    assert_eq!(stop_records(&a).last().unwrap()["labelled"], false);
    say(&a, "Here is the long answer.", "stop-1");
    playing.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
    a.action(ui::UiAction::StopSpeaking).unwrap();
    a.tools.test_finish_speech().unwrap();
    let l = labels(&a);
    assert_eq!(l.len(), 1, "{l:#?}");
    assert_eq!((l[0]["task"].as_str(), l[0]["label"].as_str(), l[0]["source"].as_str()), (Some("send"), Some("not_for_heimdall"), Some("owner-stop")));
    assert_eq!(l[0]["text"], text);
    assert_eq!(l[0]["decider_input"], d["prompt"], "exactly what V5 saw");
    assert_eq!(l[0]["v5"]["choice"], d["choice"]);
    let stop = stop_records(&a).last().cloned().unwrap();
    assert_eq!((stop["speaking"].as_bool(), stop["labelled"].as_bool()), (Some(true), Some(true)));
    // His typed words reach the run (it may speak again); it speaks, he presses Stop again: still one label.
    a.tools.speech_policy.owner_woke("Go on then.", capture::host_now_ns());
    assert!(say(&a, "As I was saying.", "stop-2").starts_with("Speech queued"));
    playing.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
    a.action(ui::UiAction::StopSpeaking).unwrap();
    a.tools.test_finish_speech().unwrap();
    assert_eq!(labels(&a).len(), 1, "once per line");
    assert_eq!(stop_records(&a).last().unwrap()["labelled"], false);
    release(&a, &hold);
}

/// 5 Nov: Stop cuts speech, it does not cancel work. The press goes through `Tools::stop_pressed` (the voice
/// and the queue stop, one `speech_stopped` signal, the agent's turn goes on), with exactly one decider-v6 label.
/// The full run failed here: the turn went on and its final reply was said right after the Stop. Now the rest of
/// the run is shown on the island, unsaid.
#[test]
fn the_stop_button_cuts_the_voice_once_with_one_label_and_one_signal() {
    let (_t, mut a, playing, hold) = speaking_rig("I think that makes quite a lot of sense.", (0.95, 0.0, 0.05));
    say(&a, "Here is the long answer.", "stop-1");
    say(&a, "And more of it.", "stop-2");
    playing.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
    a.action(ui::UiAction::StopSpeaking).unwrap();
    a.tools.test_finish_speech().unwrap();
    a.tools.test_finish_speech().unwrap();
    assert!(!a.tools.agent_speech_busy(), "the voice and the queue stopped");
    // The turn goes on: its final reply ("fixture reply") arrives now, and is shown, not said.
    release(&a, &hold);
    assert!(playing.recv_timeout(std::time::Duration::from_millis(300)).is_err(), "neither the queued line nor the run's final reply plays");
    assert!(!a.tools.agent_speech_busy());
    assert_eq!(labels(&a).len(), 1, "one decider-v6 label for the press");
    let session = a.tools.archive_session.lock().unwrap().clone().unwrap();
    let records = heimdall_archive::Journal::open_read_only(&session).unwrap().records().unwrap();
    let stops: Vec<_> = records.iter().filter(|r| r.kind == "speech_stopped").collect();
    assert_eq!(stops.len(), 1, "one speech_stopped signal");
    assert_eq!(stops[0].payload["reason"], "stop_button");
    assert!(records.iter().any(|r| r.kind == "speech_held" && r.payload["event"].as_str().is_some_and(|e| e.contains("Stop"))), "the final reply was shown unsaid");
    // His next words wake a new run: its answer is said again.
    a.tools.speech_policy.owner_woke("And now?", capture::host_now_ns());
    assert!(say(&a, "Now it is said.", "stop-3").starts_with("Speech queued"));
    playing.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
}

/// A Stop with no line the decider woke for (only a kept line) stops the voice and labels nothing, even while the
/// reply of a run nothing of his woke (a T3 result, a notice) is playing.
#[test]
fn a_stop_with_no_decider_wake_labels_nothing() {
    let (_t, mut a, playing, hold) = speaking_rig("Just talking to myself here.", (0.05, 0.0, 0.95));
    assert_eq!(a.dispatched_through, 0, "V5 kept it");
    say(&a, "The T3 worker finished the build.", "notice-1");
    playing.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
    a.action(ui::UiAction::StopSpeaking).unwrap();
    a.tools.test_finish_speech().unwrap();
    assert!(labels(&a).is_empty());
    assert_eq!(stop_records(&a).last().unwrap()["speaking"], true);
    release(&a, &hold);
}
