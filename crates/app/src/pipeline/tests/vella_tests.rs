//! Dictating to Vella (docs/vella.md, Channel 1) through the real `Pipeline` with stubbed models, the dry-run agent and
//! Vella's notifications injected as the observer delivers them. While the user dictates, his words are archived and given
//! to the agent marked `[dictated to Vella]`, but never wake it; afterwards his name wakes it as before.
use super::plumbing_tests::{barges, hear, rig, speaking, turn, Detect};
use super::*;

const S: u64 = 1_000_000_000;
/// Vella's wall clock at host time 0, in Unix seconds.
const EPOCH: f64 = 1_800_000_000.0;

fn records(a: &Pipeline) -> Vec<heimdall_archive::Record> {
    a.index.evidence.records().unwrap()
}
fn notice(state: &str, reason: &str, label: &str) -> serde_json::Value {
    serde_json::json!({"v":1,"state":state,"reason":reason,"session":"S1","pid":4321,"mode":"dictation",
        "shortcut":"{\"key\":\"n\",\"modifiers\":[\"control\",\"command\"],\"behavior\":\"toggle\"}","shortcutLabel":label})
}
/// Vella posts `info` at host time `at`; Heimdall receives it 20 ms later, with its clock reading then.
fn post(a: &mut Pipeline, mut info: serde_json::Value, at: u64) {
    info["sent"] = serde_json::json!(EPOCH + at as f64 / 1e9);
    let received = at + 20_000_000;
    a.hooks.vella_now = Some(received);
    a.vella_notice(crate::vella::Heard { info, at_ns: received, at_unix: EPOCH + received as f64 / 1e9 });
}
fn vella(a: &mut Pipeline, state: &str, reason: &str, at: u64) {
    post(a, notice(state, reason, "⌃⌘N · Toggle"), at);
}
/// A rig whose Vella process counts as alive.
fn app() -> (tempfile::TempDir, Pipeline, String) {
    let (t, mut a, mic) = rig(false);
    a.vella.alive = |_| true;
    (t, a, mic)
}
/// The island's Vella updates so far.
fn island(a: &Pipeline) -> Vec<(bool, Option<String>)> {
    a.hooks.ui.lock().unwrap().iter().filter_map(|u| match u { ui::UiUpdate::Vella { on, keys } => Some((*on, keys.clone())), _ => None }).collect()
}
fn row(a: &Pipeline, text: &str) -> serde_json::Value {
    a.index.utterances_between(0, 1000 * S).unwrap().into_iter().find(|r| r["text"].as_str().is_some_and(|t| t.contains(text))).unwrap_or_else(|| panic!("no utterance with {text}"))
}
/// The d1 send decisions made, with their triggers (a wake by name has `trigger` "name").
fn decisions(a: &Pipeline) -> Vec<serde_json::Value> {
    records(a).into_iter().filter(|r| r.kind == "send_decision").map(|r| r.payload).collect()
}
fn dictated_status(a: &Pipeline, text: &str) {
    let r = row(a, text);
    assert_eq!(r["decision"], serde_json::json!({"status":"dictated_to_vella","given_to_agent":true,"would_wake":false}), "{r:#}");
    assert!(!r["text"].as_str().unwrap().contains("[dictated"), "the archive keeps his words as said: {r:#}");
}
/// He said `text` and it woke the agent by name (through d1's record, as every final does).
fn woke(a: &Pipeline, text: &str) {
    let id = row(a, text)["id"].as_str().unwrap().to_owned();
    let r = records(a);
    let woke = r.iter().any(|r| r.kind == "send_decision" && r.payload["trigger"] == "name" && r.utterance_ids.iter().any(|u| *u == id || u.ends_with(&id) || id.ends_with(u.as_str())));
    assert!(woke, "{text} did not wake: {:#?}", decisions(a));
    assert_eq!(row(a, text)["decision"]["would_wake"], true);
}

/// Toggle on, a dictated utterance (naming Heimdall, a screen phrase seen in a partial, "send it"), toggle off, then his
/// name wakes as before, with the dictated line already in the conversation the wake carries.
#[test]
fn dictation_on_then_off_holds_every_wake_and_keeps_the_words_as_context() {
    let (_t, mut a, mic) = app();
    vella(&mut a, "on", "start", S);
    assert_eq!(island(&a), vec![(true, Some("⌃⌘N".into()))]);
    a.hooks.vella_now = Some(9 * S);
    turn(&mut a, &mic, 2 * S, "Heimdall look at this and send it", Detect::Partial, 4, None, S, |_| {});
    dictated_status(&a, "Heimdall look at this and send it");
    let r = records(&a);
    assert!(!r.iter().any(|r| r.kind == "command_decision" || r.kind == "screen_command"), "never a spoken command");
    let heard = a.hooks.ui.lock().unwrap().iter().filter(|u| matches!(u, ui::UiUpdate::Command(ui::CommandCue::Heard { .. }))).count();
    assert_eq!(heard, 0, "never a heard cue while dictating");
    assert!(decisions(&a).is_empty(), "never decided, never a wake");
    assert!(a.early_sends.is_empty(), "never sent early");
    assert!(a.command_candidates.is_empty() && a.hooks.send.is_empty());
    let routed: Vec<_> = r.iter().filter(|r| r.kind == "agent_context_routing").collect();
    assert_eq!(routed.len(), 1);
    assert_eq!(routed[0].payload["policy"], "dictated_to_vella");
    assert_eq!(routed[0].payload["observation"]["text"], "[dictated to Vella] Heimdall look at this and send it");
    assert!(a.pending_text.iter().any(|l| l.contains("[dictated to Vella] Heimdall look at this and send it")), "{:?}", a.pending_text);

    vella(&mut a, "off", "stop", 10 * S);
    assert_eq!(island(&a).last(), Some(&(false, Some("⌃⌘N".into()))));
    a.hooks.vella_now = Some(20 * S);
    turn(&mut a, &mic, 12 * S, "Heimdall what did I just dictate", Detect::Final, 0, Some(0.), S, |_| {});
    woke(&a, "what did I just dictate");
    let d = decisions(&a);
    assert_eq!(d.len(), 1, "{d:#?}");
    assert!(d[0]["prompt"]["state"].as_str().unwrap().contains("[dictated to Vella] Heimdall look at this"), "the wake carries the dictated line");
    assert_eq!(row(&a, "what did I just dictate")["decision"]["given_to_agent"], true);
}

/// A missed stop: with no heartbeat, dictation ends 45 s after the start was heard, at that time; the settings tick
/// tells the island; heartbeats keep a long one on.
#[test]
fn a_missed_stop_ends_after_45_seconds_and_heartbeats_keep_it_on() {
    let (_t, mut a, mic) = app();
    vella(&mut a, "on", "start", S);
    a.hooks.vella_now = Some(30 * S);
    turn(&mut a, &mic, 20 * S, "Heimdall first draft of the email", Detect::Final, 0, None, S, |_| {});
    dictated_status(&a, "first draft of the email");
    a.hooks.vella_now = Some(S + 20_000_000 + crate::vella::TIMEOUT_NS + 1);
    a.vella_tick();
    assert_eq!(island(&a).last().map(|u| u.0), Some(false), "the token goes at the timeout");
    assert!(records(&a).iter().any(|r| r.kind == "vella_dictation" && r.payload["expired"] == "timeout"));
    a.hooks.vella_now = Some(60 * S);
    turn(&mut a, &mic, 50 * S, "Heimdall are you back", Detect::Final, 0, Some(0.), S, |_| {});
    woke(&a, "are you back");

    // Heartbeats every 15 s keep a long dictation on well past 45 s.
    vella(&mut a, "on", "start", 100 * S);
    for k in 1..=5 {
        vella(&mut a, "on", "heartbeat", 100 * S + k * 15 * S);
    }
    a.hooks.vella_now = Some(180 * S);
    a.vella_tick();
    turn(&mut a, &mic, 170 * S, "Heimdall this is still the email", Detect::Final, 0, None, S, |_| {});
    dictated_status(&a, "still the email");
    assert_eq!(decisions(&a).len(), 1);
}

/// Vella's process died without a stop: dictation ended when it was last heard, and his name wakes at once.
#[test]
fn a_dead_vella_restores_waking_at_once() {
    let (_t, mut a, mic) = app();
    vella(&mut a, "on", "start", S);
    a.vella.alive = |_| false;
    a.hooks.vella_now = Some(5 * S);
    turn(&mut a, &mic, 3 * S, "Heimdall did Vella crash", Detect::Final, 0, Some(0.), S, |_| {});
    woke(&a, "did Vella crash");
    assert_eq!(island(&a).last().map(|u| u.0), Some(false));
    assert!(records(&a).iter().any(|r| r.kind == "vella_dictation" && r.payload["expired"] == "pid_gone"));
}

/// A keybind change in Vella arrives in a hello: Heimdall's stored shortcut follows, and the island's token shows the
/// new keys while dictating (a long label falls back to the keyboard symbol in the scene).
#[test]
fn a_keybind_change_in_a_hello_updates_the_stored_shortcut() {
    let (_t, mut a, _mic) = app();
    post(&mut a, notice("off", "hello", "⌃⌘N · Toggle"), S);
    assert_eq!(a.vella.keys().as_deref(), Some("⌃⌘N"));
    assert!(island(&a).is_empty(), "nothing shows while off");
    let mut changed = notice("off", "hello", "⌥D · Hold");
    changed["shortcut"] = serde_json::json!("{\"key\":\"d\",\"modifiers\":[\"option\"],\"behavior\":\"hold\"}");
    post(&mut a, changed.clone(), 2 * S);
    assert_eq!((a.vella.keys().as_deref(), a.vella.shortcut_label.as_str()), (Some("⌥D"), "⌥D · Hold"));
    assert!(a.vella.shortcut.contains("option"));
    changed["state"] = serde_json::json!("on");
    changed["reason"] = serde_json::json!("start");
    post(&mut a, changed, 3 * S);
    assert_eq!(island(&a), vec![(true, Some("⌥D".into()))]);
    // Changed again while dictating: the token follows.
    let mut again = notice("on", "hello", "⌃⌥Space · Toggle");
    again["shortcut"] = serde_json::json!("{\"key\":\"space\"}");
    post(&mut a, again, 4 * S);
    assert_eq!(island(&a).last(), Some(&(true, Some("⌃⌥Space".into()))));
    let journal: Vec<_> = records(&a).into_iter().filter(|r| r.kind == "vella_dictation").collect();
    assert!(journal.iter().any(|r| r.payload["link"]["shortcut_label"] == "⌃⌥Space · Toggle"));
}

/// Vella absent: no notification ever arrives, and everything is as it was: his name wakes, nothing shows.
#[test]
fn without_vella_nothing_changes() {
    let (_t, mut a, mic) = rig(false);
    turn(&mut a, &mic, 2 * S, "Heimdall how is the build", Detect::Final, 0, Some(0.), S, |_| {});
    woke(&a, "how is the build");
    assert!(island(&a).is_empty());
    assert!(!records(&a).iter().any(|r| r.kind == "vella_dictation"));
    assert!(!a.pending_text.iter().any(|l| l.contains("[dictated")));
}

/// Malformed userInfo or another protocol version is ignored (journalled), and waking is unchanged.
#[test]
fn malformed_notifications_are_ignored() {
    let (_t, mut a, mic) = app();
    let mut v2 = notice("on", "start", "⌃⌘N · Toggle");
    v2["v"] = serde_json::json!(2);
    post(&mut a, v2, S);
    let mut no_pid = notice("on", "start", "⌃⌘N · Toggle");
    no_pid.as_object_mut().unwrap().remove("pid");
    post(&mut a, no_pid, S);
    post(&mut a, serde_json::json!({"v":1,"state":"recording","reason":"start","pid":4321}), S);
    a.vella_notice(crate::vella::Heard { info: serde_json::Value::Null, at_ns: S, at_unix: EPOCH + 1.0 });
    assert!(!a.vella.on() && island(&a).is_empty());
    assert_eq!(records(&a).iter().filter(|r| r.kind == "vella_dictation" && r.payload["ignored"].is_string()).count(), 4);
    a.hooks.vella_now = Some(5 * S);
    turn(&mut a, &mic, 2 * S, "Heimdall are you listening", Detect::Final, 0, Some(0.), S, |_| {});
    woke(&a, "are you listening");
}

/// An utterance half inside: the line is dictated when dictation covers at least half of its words' span, and goes the
/// ordinary way (d1 judges it) when less. Ten words from 2.0 s end at 4.95 s (2.95 s): a stop stamped at 3.6 s covers
/// 54 %, at 3.2 s 41 %. Its words inside dictation trigger nothing either way (review 27 Oct).
#[test]
fn an_utterance_half_inside_is_dictated_only_from_half() {
    const TEXT: &str = "one two three four five six seven eight nine ten";
    for (stop, dictated) in [(3_600_000_000u64, true), (3_200_000_000, false)] {
        let (_t, mut a, mic) = app();
        vella(&mut a, "on", "start", S);
        // The stop is stamped at its posting time (received 20 ms later, less the 20 ms delay).
        vella(&mut a, "off", "stop", stop);
        a.hooks.vella_now = Some(10 * S);
        turn(&mut a, &mic, 2 * S, TEXT, Detect::Final, 0, if dictated { None } else { Some(0.) }, S, |_| {});
        if dictated {
            dictated_status(&a, TEXT);
            assert!(decisions(&a).is_empty());
        } else {
            let d = decisions(&a);
            assert_eq!(d.len(), 1, "d1 judged it: {d:#?}");
            assert!(d[0]["prompt"]["state"].as_str().unwrap().contains("[dictated to Vella: one two three four]"), "{:#}", d[0]["prompt"]["state"]);
            assert_ne!(row(&a, TEXT)["decision"]["status"], "dictated_to_vella");
        }
    }
}

/// While he dictates, talking over Heimdall's speech is not a barge-in: Heimdall talks on, his voice not even checked.
#[test]
fn dictating_over_heimdalls_voice_is_never_a_barge_in() {
    let (_t, mut a, mic, speech) = speaking("hold the build failed on main", 2 * S);
    a.vella.alive = |_| true;
    vella(&mut a, "on", "start", S);
    a.hooks.vella_now = Some(2 * S);
    a.listen(ListenEvent::SpeechStart { source: mic.clone(), host_time_ns: 2 * S }).unwrap();
    a.hooks.user_voice.push_back(Some(0.9));
    hear(&mut a, &mic, 4 * S);
    speech.still_speaking();
    assert!(barges(&a).is_empty(), "{:?}", barges(&a));
    assert!(a.barge.is_none());
    assert_eq!(a.hooks.user_voice.len(), 1, "his voice was not even checked");
}

/// Review 27 Oct (2): a command is judged by its own phrase's span. "take a screenshot" said at 2 s, Vella started at
/// 3.5 s, the window decode and its verdict at 4 s: the phrase was not dictated, so it runs.
#[test]
fn a_command_said_just_before_dictation_still_runs() {
    let (_t, mut a, mic) = app();
    a.command_test_answers.push_back((decide::CommandIntent::Yes, decide::CommandSelection::TakeScreenshot));
    let words = super::plumbing_tests::timed("take a screenshot of the error", 2 * S);
    a.audio.insert(mic.clone(), AudioRing { start: 0, samples: vec![0.01; (6 * S / 62500) as usize].into() });
    a.listen(ListenEvent::SpeechStart { source: mic.clone(), host_time_ns: 2 * S }).unwrap();
    vella(&mut a, "on", "start", 3_500_000_000);
    a.hooks.vella_now = Some(4 * S);
    let segment = words[..3].to_vec();
    a.listen(ListenEvent::TranscriptSegment { source: mic.clone(), text: listen::Transcript::from_words(segment.clone()).text, start_ns: segment[0].start_ns, end_ns: segment.last().unwrap().end_ns, words: segment }).unwrap();
    let r = records(&a);
    assert!(r.iter().any(|r| r.kind == "command_action"), "the command ran");
    assert!(!r.iter().any(|r| r.payload["reason"] == "dictated_to_vella"));
}

/// Review 27 Oct (2): a phrase proposed from words that turn out dictated (Vella's start arrived late, stamped back to
/// the press) never runs when its verdict comes, even though dictation stopped before it.
#[test]
fn a_command_from_dictated_words_never_runs_even_after_dictation_stopped() {
    let (_t, mut a, mic) = app();
    a.audio.insert(mic.clone(), AudioRing { start: 0, samples: vec![0.01; (8 * S / 62500) as usize].into() });
    a.listen(ListenEvent::SpeechStart { source: mic.clone(), host_time_ns: 3 * S }).unwrap();
    let words = super::plumbing_tests::timed("take a screenshot", 3 * S);
    a.hooks.vella_now = Some(4 * S);
    a.listen(ListenEvent::PartialTranscript { source: mic.clone(), text: "take a screenshot".into(), words: words.clone(), stable_words: 0 }).unwrap();
    assert_eq!(a.command_candidates.len(), 1, "proposed before Vella was heard from");
    // Vella's start, posted at 2.9 s, arrives at 4.0 s (stamped 2.9 s); its stop at 5 s.
    let mut start = notice("on", "start", "⌃⌘N · Toggle");
    start["sent"] = serde_json::json!(EPOCH + 2.9);
    a.vella_notice(crate::vella::Heard { info: start, at_ns: 4 * S, at_unix: EPOCH + 4.0 });
    vella(&mut a, "off", "stop", 5 * S);
    a.hooks.vella_now = Some(6 * S);
    a.command_test_answers.push_back((decide::CommandIntent::Yes, decide::CommandSelection::TakeScreenshot));
    let c = a.command_candidates.pop_front().unwrap();
    a.decide_command(c, &words, decide::CommandStage::Final, serde_json::json!({"origin":"test"})).unwrap();
    let r = records(&a);
    assert!(!r.iter().any(|r| r.kind == "command_action" || r.kind == "screen_command"), "nothing ran");
    assert!(r.iter().any(|r| r.kind == "command_decision" && r.payload["reason"] == "dictated_to_vella"));
    let cues: Vec<_> = a.hooks.ui.lock().unwrap().iter().filter_map(|u| match u { ui::UiUpdate::Command(c) => Some(c.clone()), _ => None }).collect();
    assert!(matches!(cues.last(), Some(ui::CommandCue::Dropped { .. })), "the heard cue left: {cues:?}");
}

/// Review 27 Oct (1): 4 s of ordinary talk, then the key, then 2 s dictated "tell Heimdall to send it tomorrow". Under
/// half dictated, the line goes the ordinary way (d1 judges it), but the dictated words name nobody and send nothing:
/// no name or "send it" wake, no early send. The agent and d1 read the dictated run marked; the archive keeps his words.
#[test]
fn dictated_words_in_a_mostly_spoken_line_never_trigger() {
    const SPOKEN: &str = "so I was thinking about the plan for the launch next week";
    const DICTATED: &str = "tell Heimdall to send it tomorrow";
    let (_t, mut a, mic) = app();
    // His spoken words run 2.0 to 5.55 s; Vella starts at 5.8 s; the dictated words run 5.9 to 7.65 s.
    let spoken = super::plumbing_tests::timed(SPOKEN, 2 * S);
    let dictated = super::plumbing_tests::timed(DICTATED, 5_900_000_000);
    let mut words = spoken.clone();
    words.extend(dictated);
    let end = words.last().unwrap().end_ns;
    a.audio.insert(mic.clone(), AudioRing { start: 0, samples: vec![0.01; (10 * S / 62500) as usize].into() });
    a.listen(ListenEvent::SpeechStart { source: mic.clone(), host_time_ns: 2 * S }).unwrap();
    vella(&mut a, "on", "start", 5_800_000_000);
    a.hooks.vella_now = Some(9 * S);
    a.hooks.send.push_back(0.);
    a.listen(ListenEvent::FinalUtterance { source: mic.clone(), text: listen::Transcript::from_words(words.clone()).text, words, start_ns: 2 * S, end_ns: end, forced: false, end_of_turn_probability: 0.9 }).unwrap();
    let d = decisions(&a);
    assert_eq!(d.len(), 1, "d1 judged the line: {d:#?}");
    assert!(d[0]["trigger"].is_null(), "no name or send-it wake from dictated words: {:#}", d[0]["trigger"]);
    assert!(a.early_sends.is_empty(), "never sent early");
    let marked = format!("{SPOKEN} [dictated to Vella: {DICTATED}]");
    assert!(d[0]["prompt"]["state"].as_str().unwrap().contains(&marked), "d1 reads the dictated run marked");
    let routed = records(&a).into_iter().find(|r| r.kind == "agent_context_routing").unwrap();
    assert_eq!(routed.payload["observation"]["text"], marked.as_str());
    let r = row(&a, "launch next week");
    assert_eq!(r["text"], format!("{SPOKEN} {DICTATED}"), "the archive keeps his words");
    assert_eq!(r["decision"]["would_wake"], false);
}

/// Review 27 Oct (1): the dictated "take a screenshot" inside a mostly spoken line is never a command.
#[test]
fn a_dictated_command_phrase_in_a_mostly_spoken_line_never_runs() {
    let (_t, mut a, mic) = app();
    a.command_test_answers.push_back((decide::CommandIntent::Yes, decide::CommandSelection::TakeScreenshot));
    vella(&mut a, "on", "start", 5_800_000_000);
    a.hooks.vella_now = Some(9 * S);
    let mut words = super::plumbing_tests::timed("so I was thinking about the plan for the launch next week", 2 * S);
    words.extend(super::plumbing_tests::timed("take a screenshot", 5_900_000_000));
    let end = words.last().unwrap().end_ns;
    a.audio.insert(mic.clone(), AudioRing { start: 0, samples: vec![0.01; (10 * S / 62500) as usize].into() });
    a.listen(ListenEvent::SpeechStart { source: mic.clone(), host_time_ns: 2 * S }).unwrap();
    a.listen(ListenEvent::PartialTranscript { source: mic.clone(), text: String::new(), words: words.clone(), stable_words: 0 }).unwrap();
    a.hooks.send.push_back(0.);
    a.listen(ListenEvent::FinalUtterance { source: mic.clone(), text: listen::Transcript::from_words(words.clone()).text, words, start_ns: 2 * S, end_ns: end, forced: false, end_of_turn_probability: 0.9 }).unwrap();
    let r = records(&a);
    assert!(!r.iter().any(|r| r.kind == "command_decision" || r.kind == "screen_command" || r.kind == "command_action"), "never a command");
    assert_eq!(a.hooks.ui.lock().unwrap().iter().filter(|u| matches!(u, ui::UiUpdate::Command(ui::CommandCue::Heard { .. }))).count(), 0);
}

/// Review 27 Oct (1), the reverse: a line mostly dictated whose last words come after he stopped. Those words were said
/// to Heimdall, not Vella: "Heimdall, what do you think" wakes it by name, carrying the dictated run marked.
#[test]
fn words_after_the_stop_in_a_mostly_dictated_line_wake() {
    const DICTATED: &str = "dear team the release moves to friday because the audio fix needs another day of testing";
    let (_t, mut a, mic) = app();
    vella(&mut a, "on", "start", 1_900_000_000);
    let mut words = super::plumbing_tests::timed(DICTATED, 2 * S);
    let stop = words.last().unwrap().end_ns + 100_000_000;
    vella(&mut a, "off", "stop", stop);
    words.extend(super::plumbing_tests::timed("Heimdall what do you think", stop + 200_000_000));
    let end = words.last().unwrap().end_ns;
    a.audio.insert(mic.clone(), AudioRing { start: 0, samples: vec![0.01; (end / 62500 + 16000) as usize].into() });
    a.listen(ListenEvent::SpeechStart { source: mic.clone(), host_time_ns: 2 * S }).unwrap();
    a.hooks.vella_now = Some(end + S);
    a.hooks.send.push_back(0.);
    a.listen(ListenEvent::FinalUtterance { source: mic.clone(), text: listen::Transcript::from_words(words.clone()).text, words, start_ns: 2 * S, end_ns: end, forced: false, end_of_turn_probability: 0.9 }).unwrap();
    woke(&a, "what do you think");
    let routed = records(&a).into_iter().find(|r| r.kind == "agent_context_routing").unwrap();
    assert_eq!(routed.payload["observation"]["text"], format!("[dictated to Vella: {DICTATED}] Heimdall what do you think"));
    // Without the words after the stop, the same line is dictated and wakes nothing.
    let (_t, mut b, mic) = app();
    vella(&mut b, "on", "start", 1_900_000_000);
    b.hooks.vella_now = Some(10 * S);
    turn(&mut b, &mic, 2 * S, DICTATED, Detect::Final, 0, None, S, |_| {});
    dictated_status(&b, "release moves to friday");
}

/// Review 27 Oct (4): the live loop's `select!` picks among ready channels at random, so Vella's start queued beside
/// a final could be applied after it. The loop drains queued notifications before a listen event and before the
/// timeout check (`live_loop::drain_vella`): the final then counts as dictated, and a queued heartbeat keeps a long
/// dictation from timing out.
#[test]
fn queued_vella_notices_are_applied_before_a_final_and_before_the_timeout() {
    let (_t, mut a, mic) = app();
    let (tx, rx) = crossbeam_channel::bounded(8);
    let heard = |state: &str, reason: &str, at: u64| {
        let mut info = notice(state, reason, "⌃⌘N · Toggle");
        info["sent"] = serde_json::json!(EPOCH + at as f64 / 1e9);
        crate::vella::Heard { info, at_ns: at, at_unix: EPOCH + at as f64 / 1e9 }
    };
    tx.send(heard("on", "start", S)).unwrap();
    crate::live_loop::drain_vella(&mut a, &rx);
    a.hooks.vella_now = Some(10 * S);
    turn(&mut a, &mic, 2 * S, "Heimdall this goes to Vella", Detect::Final, 0, None, S, |_| {});
    dictated_status(&a, "this goes to Vella");
    // A heartbeat queued at 40 s, drained before the 50 s tick: still dictating.
    tx.send(heard("on", "heartbeat", 40 * S)).unwrap();
    a.hooks.vella_now = Some(50 * S);
    crate::live_loop::drain_vella(&mut a, &rx);
    a.vella_tick();
    assert!(a.vella.on(), "the queued heartbeat counted before the timeout check");
    assert!(rx.is_empty());
}
