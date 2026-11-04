//! Speech policy (night pass 3, bridge/speech_policy.rs), end to end through the real `Pipeline` with stubbed models
//! and a fake Pi: the wake sound plays on a spoken wake only, and its own window never becomes an utterance, a
//! decision or a wake; `ask` cards are silent; while the user is in a conversation nothing is played, the agent is told in
//! one line, and speech resumes when the conversation ends.
use super::plumbing_tests::{rig, turn, Detect};
use super::*;
use std::sync::atomic::AtomicUsize;

const S: u64 = 1_000_000_000;
const MS: u64 = 1_000_000;

fn records(a: &Pipeline) -> Vec<heimdall_archive::Record> {
    a.index.evidence.records().unwrap()
}
fn count(a: &Pipeline, kind: &str) -> usize {
    records(a).iter().filter(|r| r.kind == kind).count()
}
/// Counts the wake sounds instead of playing them.
fn cue_counter(a: &Pipeline) -> Arc<AtomicUsize> {
    let played = Arc::new(AtomicUsize::new(0));
    let count = played.clone();
    a.tools.speech_policy.set_player(Arc::new(move || { count.fetch_add(1, Ordering::SeqCst); }));
    played
}
fn discord(a: &Pipeline) -> String {
    a.sources.iter().find(|(_, s)| crate::context_policy::is_call(s)).map(|(id, _)| id.clone()).expect("the rig's Discord source")
}

/// "whenever the decider decides to send a message to the agent, there should be a little ring noise".
/// The cue's own sound on the microphone (an "Mm." heard inside its window) is archived as Heimdall's, never a turn, a
/// decision or a wake; speech that runs past the window is the user's and is decided as usual.
#[test]
fn spoken_wake_plays_the_cue_and_its_window_never_becomes_an_utterance_or_a_wake() {
    let (_t, mut a, mic) = rig(true);
    let cues = cue_counter(&a);
    turn(&mut a, &mic, 10 * S, "Heimdall check the build", Detect::Final, 0, Some(0.), 0, |_| {});
    a.agent.as_ref().unwrap().barrier().unwrap();
    assert_eq!(count(&a, "agent_wake"), 1);
    assert_eq!(cues.load(Ordering::SeqCst), 1, "the spoken wake played the cue once");
    let cue = a.tools.speech_policy.cue_at();
    assert!(cue > 0);
    let decided = count(&a, "send_decision");

    // The cue on the microphone, wholly inside its window. d1 would send it if it ever got there.
    turn(&mut a, &mic, cue + 100 * MS, "Mm.", Detect::Final, 0, Some(0.99), 0, |_| {});
    a.agent.as_ref().unwrap().barrier().unwrap();
    assert_eq!(count(&a, "send_decision"), decided, "the cue reached the decider");
    assert_eq!(count(&a, "agent_wake"), 1, "the cue woke the agent");
    assert_eq!(cues.load(Ordering::SeqCst), 1);
    assert_eq!(a.hooks.send.pop_front(), Some(0.99), "d1 was asked about the cue");
    let rows = a.index.utterances_between(cue, cue + S).unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!((rows[0]["speaker"].as_str(), rows[0]["decision"]["status"].as_str(), rows[0]["decision"]["reason"].as_str()),
        (Some("Heimdall"), Some("archive_only"), Some("wake_sound")), "{}", rows[0]);
    let context = a.tools.send_context.lock().unwrap().snapshot();
    assert!(!context.pending.iter().chain(context.turns.iter()).any(|t| t.text.contains("Mm.")), "the cue entered the decider's conversation");

    // Boundary: speech that starts inside the window but runs past it is the user's.
    turn(&mut a, &mic, cue + 600 * MS, "wait one more thing", Detect::Final, 0, Some(0.), 0, |_| {});
    a.agent.as_ref().unwrap().barrier().unwrap();
    assert_eq!(count(&a, "send_decision"), decided + 1, "User's words past the window were dropped");
    assert!(a.pending_text.iter().any(|t| t.contains("wait one more thing")), "{:?}", a.pending_text);
}

/// Speech that starts while the cue sounds is Heimdall audible, as for its voice: the next line does not wait for it
/// as the user's turn (bridge/hold.rs).
#[test]
fn speech_starting_during_the_cue_is_not_owners_turn() {
    let (_t, a, mic) = rig(true);
    a.tools.voice(&mic, true);
    assert!(a.tools.owner.holds(), "without the cue, speech is User's turn");
    a.tools.voice(&mic, false);
    let (_t, a, mic) = rig(true);
    a.tools.speech_policy.wake_sent(capture::host_now_ns());
    a.tools.voice(&mic, true);
    assert!(!a.tools.owner.holds(), "the cue on the microphone held Heimdall's next line");
}

/// Wakes and typed messages sound (bug note 2026-10-30T18-54-31: "the same small ring as a voice wake, so he knows
/// it arrived"); nothing does with Wake Sound off; the menu's switch is a persisted setting the menu shows. A typed
/// message also stops Heimdall's speech (bug note 2026-10-31T02-16-40).
#[test]
fn only_spoken_wakes_sound_and_wake_sound_switches_off_and_on() {
    // Typed: the Agent window's Send.
    let (_t, mut a, mic) = rig(true);
    let cues = cue_counter(&a);
    turn(&mut a, &mic, 10 * S, "An unfinished thought", Detect::Final, 0, Some(0.), 0, |_| {});
    let epoch = a.tools.voice_bank.epoch();
    a.agent.as_ref().unwrap().window_action(ui::AgentWindowAction::Send { text: "Use the current folder".into(), steer: false }).unwrap();
    a.agent.as_ref().unwrap().barrier().unwrap();
    assert_eq!(cues.load(Ordering::SeqCst), 1, "a typed message rings once");
    assert_ne!(a.tools.voice_bank.epoch(), epoch, "a typed message interrupts Heimdall's speech");
    cues.store(0, Ordering::SeqCst);
    // Wake Sound off from the menu: a spoken wake is silent; on again, it sounds.
    a.action(ui::UiAction::SetWakeSound(false)).unwrap();
    assert!(!a.settings.wake_sound && !a.ui_settings_value().wake_sound);
    turn(&mut a, &mic, 20 * S, "Heimdall check the build", Detect::Final, 0, Some(0.), 0, |_| {});
    a.agent.as_ref().unwrap().barrier().unwrap();
    assert!(count(&a, "agent_wake") >= 1);
    assert_eq!(cues.load(Ordering::SeqCst), 0, "Wake Sound off still sounded");
    a.action(ui::UiAction::SetWakeSound(true)).unwrap();
    turn(&mut a, &mic, 30 * S, "Heimdall and run the tests", Detect::Final, 0, Some(0.), 0, |_| {});
    a.agent.as_ref().unwrap().barrier().unwrap();
    assert_eq!(cues.load(Ordering::SeqCst), 1);
    // Agent off: the dry run sends nothing, so nothing sounds.
    let (_t, mut a, mic) = rig(false);
    let cues = cue_counter(&a);
    turn(&mut a, &mic, 10 * S, "Heimdall check the build", Detect::Final, 0, Some(0.), 0, |_| {});
    a.agent.as_ref().unwrap().barrier().unwrap();
    assert_eq!(cues.load(Ordering::SeqCst), 0, "the Agent-off dry run sounded");
    // On by default, and a settings file from before the field loads it on.
    assert!(Settings::default().wake_sound);
    let old: Settings = serde_json::from_str(r#"{"speak_replies":true}"#).unwrap();
    assert!(old.wake_sound);
}

struct Synth(Arc<AtomicUsize>);
impl speak::Synthesizer for Synth {
    fn synthesize(&mut self, _: &str, _: &str, _: &dyn Fn() -> bool) -> Result<speak::Audio> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(speak::Audio { samples: vec![0.; 160], sample_rate: 16000, first_audio_ms: 0., synthesis_ms: 1. })
    }
}
struct Output;
impl speak::AudioOutput for Output {
    fn output(&mut self, _: &speak::Audio, _: &dyn Fn() -> bool) -> Result<()> { Ok(()) }
}
fn notices(a: &Pipeline) -> Vec<String> {
    super::plumbing_tests::island_notices(a)
}
fn heimdall_turns(a: &Pipeline) -> Vec<String> {
    a.tools.send_context.lock().unwrap().snapshot().turns.iter().filter(|t| t.speaker == "Heimdall").map(|t| t.text.clone()).collect()
}

/// "the text to speech by default reads those typed questions, which is kind of annoying" and "when I'm
/// talking to someone I don't want you to talk when a worker updated or whatever, you just ask me a question."
#[test]
fn cards_are_silent_and_nothing_is_spoken_while_owner_is_in_a_conversation() {
    let (_t, mut a, _mic) = rig(true);
    let spoken = Arc::new(AtomicUsize::new(0));
    a.tools.test_guarded_speech_queue(Synth(spoken.clone()), Output);
    a.tools.mcp_actions.attach(ui::Ui::handle(), Arc::new(|_, _, _| Ok(())), Arc::new(|_, _| Ok(())), Arc::new(|_, _| Ok(()))).unwrap();
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let permit = a.tools.agent_enabled.permit().unwrap();
    let say = |a: &Pipeline, text: &str, op: &str| a.tools.mcp_actions.speak(&a.tools, text, "en", op, &a.tools.agent_enabled.permit().unwrap()).unwrap();
    let synthesized = |a: &Pipeline| { while a.tools.agent_speech_busy() { std::thread::sleep(std::time::Duration::from_millis(5)); } a.tools.speech_status(); spoken.load(Ordering::SeqCst) };

    // Speaking on, no conversation: a card is shown and never read aloud; a `speak` line is.
    let asked = rt.block_on(a.tools.mcp_actions.action(&a.tools, "ask", serde_json::json!({"question":"Install the new build?","options":["Yes","No"]}), "ask-1", &permit)).unwrap();
    assert!(asked.starts_with("Question pending"), "{asked}");
    assert_eq!(synthesized(&a), 0, "the card was read aloud");
    assert_eq!(heimdall_turns(&a), ["Install the new build?"], "the card still joins the decider's conversation, once");
    assert!(say(&a, "Build is green.", "speak-1").starts_with("Speech queued"));
    assert_eq!(synthesized(&a), 1);
    // Its playback opened the reply window; held text below must not open it again.
    a.tools.send_context.lock().unwrap().finished_ns = None;

    // Another participant talks on the Discord call: the user is in a conversation.
    let call = discord(&a);
    let at = capture::host_now_ns().saturating_sub(3 * S);
    turn(&mut a, &call, at, "did you see the new build", Detect::Final, 0, None, 0, |_| {});
    assert!(a.tools.speech_policy.in_conversation(capture::host_now_ns()));
    let held = say(&a, "Mobius finished: results are in.", "speak-2");
    assert_eq!(held, crate::bridge::speech_policy::HELD_IN_CONVERSATION);
    assert_eq!(held.lines().count(), 1, "one line for the agent");
    // A final reply is held too, and a card stays silent.
    a.tools.agent_final_answer("Tests passed.", "stop").unwrap();
    rt.block_on(a.tools.mcp_actions.action(&a.tools, "ask", serde_json::json!({"question":"See the results?","options":["Yes","Later"]}), "ask-2", &permit)).unwrap();
    assert_eq!(synthesized(&a), 1, "Heimdall spoke during a conversation");
    let n = notices(&a);
    assert!(n.iter().any(|b| b.contains("Mobius finished: results are in.")) && n.iter().any(|b| b.contains("Tests passed.")), "shown silently on the island: {n:?}");
    assert_eq!(heimdall_turns(&a), ["Install the new build?", "Build is green.", "Mobius finished: results are in.", "Tests passed.", "See the results?"]);
    assert_eq!(a.tools.send_context.lock().unwrap().finished_ns, None, "held text opened the reply window");
    // A replayed call reports what happened the first time, and nothing plays.
    assert_eq!(say(&a, "Mobius finished: results are in.", "speak-2"), "Shown on the island.");

    // The boundary: the last call line ended exactly the conversation window ago, then a moment longer.
    let now = capture::host_now_ns();
    a.tools.speech_policy.test_last_line(now - crate::bridge::speech_policy::CONVERSATION_NS + 5 * S);
    assert_eq!(say(&a, "Still quiet.", "speak-3"), crate::bridge::speech_policy::HELD_IN_CONVERSATION);
    a.tools.speech_policy.test_last_line(now - crate::bridge::speech_policy::CONVERSATION_NS - S);
    assert!(say(&a, "Back again.", "speak-4").starts_with("Speech queued"), "speech did not resume after the conversation");
    assert_eq!(synthesized(&a), 2);
}

/// The exception is the user's own standing rule (prompt, 20 Oct): "Speak only when he clearly addresses you by name with
/// a question that needs a spoken answer, and then briefly." During a call, the run his address by name woke speaks
/// its reply; the next run, woken by a worker's completion notice, is held and shown silently. Through the fake Pi:
/// each run replies "fixture reply" and settles.
#[test]
fn during_a_call_a_run_woken_by_name_speaks_and_a_notice_run_is_held() {
    let (_t, mut a, mic) = rig(true);
    let now = capture::host_now_ns();
    // A line that wakes nothing starts the agent; the speech queue belongs to its delivery generation.
    turn(&mut a, &mic, now - 10 * S, "I was thinking about the build", Detect::Final, 0, Some(0.), 0, |_| {});
    a.agent.as_ref().unwrap().barrier().unwrap();
    let spoken = Arc::new(AtomicUsize::new(0));
    a.tools.test_guarded_speech_queue(Synth(spoken.clone()), Output);
    a.tools.mcp_actions.attach(ui::Ui::handle(), Arc::new(|_, _, _| Ok(())), Arc::new(|_, _| Ok(())), Arc::new(|_, _| Ok(()))).unwrap();
    let tools = a.tools.clone();
    let wait = |what: &str, done: &dyn Fn() -> bool| {
        let began = std::time::Instant::now();
        while !done() {
            // A liveness bound only; no latency is asserted. The full parallel check of 23 Oct 02:29 ran out of 10 s
            // on the notice run's reply, which takes 0.1-0.5 s alone and six copies at once (night 23 Oct fixes).
            assert!(began.elapsed() < std::time::Duration::from_secs(30), "timed out waiting for {what}");
            tools.speech_status();
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    };
    let call = discord(&a);
    turn(&mut a, &call, now - 6 * S, "so what did you think of it", Detect::Final, 0, None, 0, |_| {});
    assert!(a.tools.speech_policy.in_conversation(capture::host_now_ns()));

    // The user addresses Heimdall by name: the wake goes out, and its reply is spoken.
    turn(&mut a, &mic, now - 3 * S, "Heimdall, what time is it in Tokyo?", Detect::Final, 0, Some(0.), 0, |_| {});
    a.agent.as_ref().unwrap().barrier().unwrap();
    wait("the addressed reply to be spoken", &|| spoken.load(Ordering::SeqCst) >= 1);
    assert_eq!(records(&a).iter().filter(|r| r.kind == "send_decision").last().unwrap().payload["trigger"], "name");
    wait("the addressed run to settle", &|| tools.speech_policy.speech_held(capture::host_now_ns()));
    let shown = notices(&a).len();

    // A worker finishes: the run it wakes replies silently on the island.
    a.tools.notice("terminal:t:r:completed", "Worker t run r completed.", &a.tools.agent_enabled.permit().unwrap()).unwrap();
    // The supervisor retires a shown receipt to the archive on its next pass, leaving only a digest in the row (the
    // "archive ids" of the check of 21 Oct, and the timeouts of 24 Oct under load): `notices` reads retired bodies
    // back from the archive.
    wait("the notice run's reply on the island", &|| notices(&a).get(shown..).is_some_and(|n| n.iter().any(|b| b.contains("fixture reply"))));
    std::thread::sleep(std::time::Duration::from_millis(100));
    assert_eq!(synthesized_now(&a, &spoken), 1, "the notice run spoke during a conversation");
    assert!(a.tools.speech_policy.speech_held(capture::host_now_ns()));
}
fn synthesized_now(a: &Pipeline, spoken: &AtomicUsize) -> usize {
    a.tools.speech_status();
    spoken.load(Ordering::SeqCst)
}

/// Instant acknowledgement, end to end: d1 sends the user's line, a Jarvis ack plays instead of the wake
/// sound, the agent's wake says which line was said and when, and the ack's own voice on the microphone is archived as
/// Heimdall's, never a turn, a decision or a wake. Speech running past it is the user's.
#[test]
fn a_sent_turn_is_acknowledged_the_wake_says_so_and_the_acks_echo_is_heimdalls() {
    let (_t, mut a, mic) = rig(true);
    a.settings.speak_replies = true;
    // Through speakers: the microphone hears the ack.
    a.hooks.speakers = true;
    let cues = cue_counter(&a);
    let mut settings = speak::drama3::settings();
    settings["asr"] = serde_json::json!({"passed":true});
    a.tools.voice_bank.store().unwrap().insert(&voicebank::ClipMeta { origin: voicebank::Origin::Bank, line_id: Some("en_on_it".into()),
        text: "On it.".into(), language: "en".into(), voice: speak::drama3::VOICE.into(), generator: speak::drama3::GENERATOR.into(), settings },
        &speak::Audio { samples: vec![0.1; 60_000], sample_rate: 44100, first_audio_ms: 0., synthesis_ms: 0. }).unwrap();
    let log = crate::voice_bank::tests::probe(&a.tools, std::time::Duration::from_millis(1));
    // The shipped decider (v3) sends it and picks "On it." (pipeline/ack.rs `AckPlan::Line`).
    a.hooks.send_v3.push_back(Some("en_on_it".into()));
    turn(&mut a, &mic, 10 * S, "rename the island file please", Detect::Final, 0, None, 0, |_| {});
    a.agent.as_ref().unwrap().barrier().unwrap();
    crate::voice_bank::tests::wait_idle(&a.tools);
    assert_eq!(count(&a, "agent_wake"), 1);
    let ack: Vec<_> = records(&a).into_iter().filter(|r| r.kind == "voice_ack").map(|r| r.payload).collect();
    assert_eq!(ack.len(), 1, "{ack:#?}");
    assert_eq!((ack[0]["outcome"].as_str(), ack[0]["text"].as_str()), (Some("playing"), Some("On it.")), "{:#}", ack[0]);
    assert_eq!(log.lock().unwrap().len(), 1, "the ack played once");
    assert_eq!(cues.load(Ordering::SeqCst), 0, "no wake sound on top of the ack");
    // The supervisor took the line into that wake's text (its wording: pi_supervisor_tests).
    assert!(a.tools.take_ack_line(Some(u64::MAX)).is_none(), "taken by that wake");

    // The ack on the microphone, its words wholly inside its window: Heimdall's. As the speech detector reports it,
    // the onset is dated back by its pre-roll (up to a second, to before the ack began: review 22 Oct). It says a
    // screen phrase, which must not become a screenshot, and d1 would send it if it ever got there.
    let (from, to) = a.tools.speech_policy.last_own_sound().expect("the ack's window");
    let decided = count(&a, "send_decision");
    let words = super::plumbing_tests::timed("Let me take a look.", from + 80 * MS);
    assert!(words.last().unwrap().end_ns < to, "the echo's words fit the window");
    let onset = from - 900 * MS;
    let end = words.last().unwrap().end_ns;
    a.audio.insert(mic.clone(), AudioRing { start: onset - S, samples: vec![0.01; ((end + S - onset) / 62500) as usize].into() });
    a.listen(ListenEvent::SpeechStart { source: mic.clone(), host_time_ns: onset }).unwrap();
    a.listen(ListenEvent::PartialTranscript { source: mic.clone(), text: "Let me take a look.".into(), words: words.clone(), stable_words: words.len() }).unwrap();
    a.listen(ListenEvent::SpeechEnd { source: mic.clone(), host_time_ns: end }).unwrap();
    a.hooks.send.push_back(0.99);
    a.listen(ListenEvent::FinalUtterance { source: mic.clone(), text: "Let me take a look.".into(), start_ns: onset, end_ns: end, words, forced: false, end_of_turn_probability: 0.9 }).unwrap();
    assert!(!records(&a).iter().any(|r| r.kind == "command_decision"), "the ack's own words proposed a screenshot");
    a.agent.as_ref().unwrap().barrier().unwrap();
    assert_eq!(count(&a, "send_decision"), decided, "the ack reached the decider");
    assert_eq!(count(&a, "agent_wake"), 1, "the ack woke the agent");
    assert_eq!(a.hooks.send.pop_front(), Some(0.99));
    let rows = a.index.utterances_between(onset, from + 3 * S).unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!((rows[0]["speaker"].as_str(), rows[0]["decision"]["status"].as_str()), (Some("Heimdall"), Some("archive_only")), "{}", rows[0]);
    assert_eq!(count(&a, "voice_ack"), 1, "Heimdall's own line is never acknowledged");

    // Re-review 22 Oct: the echo merged with the user's next words is his utterance, but the echo's screen phrase is not
    // his command.
    let (from, _) = a.tools.speech_policy.last_own_sound().unwrap();
    let mut words = super::plumbing_tests::timed("Let me take a look.", from + 400 * MS);
    let after = words.last().unwrap().end_ns + 1_500 * MS;
    words.extend(super::plumbing_tests::timed("and the build is red", after));
    let end = words.last().unwrap().end_ns;
    a.listen(ListenEvent::SpeechStart { source: mic.clone(), host_time_ns: from }).unwrap();
    a.listen(ListenEvent::PartialTranscript { source: mic.clone(), text: "Let me take a look. and the build is red".into(), words: words.clone(), stable_words: words.len() }).unwrap();
    a.listen(ListenEvent::SpeechEnd { source: mic.clone(), host_time_ns: end }).unwrap();
    a.hooks.send.push_back(0.);
    a.listen(ListenEvent::FinalUtterance { source: mic.clone(), text: "Let me take a look. and the build is red".into(), start_ns: from, end_ns: end, words, forced: false, end_of_turn_probability: 0.9 }).unwrap();
    assert!(!records(&a).iter().any(|r| r.kind == "command_decision"), "the echo's screen phrase became User's command");
    assert!(a.pending_text.iter().any(|t| t.contains("build is red")), "his words past the ack are his: {:?}", a.pending_text);
}

/// Review 23 Oct, finding 1: echo cancellation ships off (settings `echo_cancel_off`), and the text guard behind it
/// stays whole. A reply sentence played through speakers comes back misheard on the microphone: Heimdall's, archived
/// only, never a decision or a wake; the user's own words after it are his. The unit is never asked for.
#[test]
fn with_echo_cancellation_off_by_default_the_text_guard_still_keeps_heimdalls_echo_out() {
    let (_t, mut a, mic) = rig(true);
    assert!(a.settings.echo_cancel_off && !a.tools.speech_policy.echo_cancel_on(), "ships off");
    let now = capture::host_now_ns();
    // A line that wakes nothing starts the agent.
    turn(&mut a, &mic, now - 30 * S, "I was thinking about the build", Detect::Final, 0, Some(0.), 0, |_| {});
    a.agent.as_ref().unwrap().barrier().unwrap();
    let start = now - 8 * S;
    a.tools.speech_policy.sentence_started("Should I push the new build to the beta testers now?", start, true);
    assert!(!crate::bridge::echo_cancel::heimdall_sounding_soon(&a.tools), "the unit was asked for while off");
    a.tools.speech_policy.voice_stopped(start + 3 * S);
    let decided = count(&a, "send_decision");
    let echo = super::plumbing_tests::timed("should I push the new bill to the beta testers", start + 200 * MS);
    let end = echo.last().unwrap().end_ns;
    a.audio.insert(mic.clone(), AudioRing { start: start - S, samples: vec![0.01; ((end + 4 * S - start) / 62500) as usize].into() });
    a.listen(ListenEvent::SpeechStart { source: mic.clone(), host_time_ns: echo[0].start_ns }).unwrap();
    a.listen(ListenEvent::SpeechEnd { source: mic.clone(), host_time_ns: end }).unwrap();
    a.hooks.send.push_back(0.99);
    a.listen(ListenEvent::FinalUtterance { source: mic.clone(), text: "should I push the new bill to the beta testers".into(), start_ns: echo[0].start_ns,
        end_ns: end, words: echo, forced: false, end_of_turn_probability: 0.9 }).unwrap();
    a.agent.as_ref().unwrap().barrier().unwrap();
    assert_eq!(count(&a, "send_decision"), decided, "the echo reached the decider");
    assert_eq!(a.hooks.send.pop_front(), Some(0.99));
    let rows = a.index.utterances_between(start, end + S).unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!((rows[0]["speaker"].as_str(), rows[0]["decision"]["status"].as_str()), (Some("Heimdall"), Some("archive_only")), "{}", rows[0]);
    assert!(count(&a, "echo_guard") >= 1, "the guard journals what it did");
    // His answer, said well after the sentence ended: his.
    turn(&mut a, &mic, start + 6 * S, "yes push it to them", Detect::Final, 0, Some(0.), 0, |_| {});
    assert!(a.pending_text.iter().any(|t| t.contains("push it to them")), "{:?}", a.pending_text);
}

/// Bug notes 23 Oct 17:34 and 17:36 (build 748cb57, MacBook speakers): an explicit trigger sends a final early, before
/// its command question, and that early send carried the whole utterance, so the echo the guard left out still reached
/// the decider and the agent ahead of the user's own question. The early send now carries only the words the guard kept,
/// the same words as the archive row. (Sentences invented; the follow-up window that triggered it then is gone, so his
/// name does here.)
#[test]
fn an_early_send_carries_only_the_words_the_echo_guard_kept() {
    let (_t, mut a, mic) = rig(true);
    let now = capture::host_now_ns();
    turn(&mut a, &mic, now - 60 * S, "I was thinking about the build", Detect::Final, 0, Some(0.), 0, |_| {});
    a.agent.as_ref().unwrap().barrier().unwrap();
    let start = now - 20 * S;
    a.tools.speech_policy.sentence_started("No restart needed, the app reads the new setting right away.", start, true);
    a.tools.speech_policy.sentence_started("Ask me for a longer answer, and talk over me in the middle.", start + 3 * S, true);
    a.tools.speech_policy.voice_stopped(start + 7 * S);
    // Heimdall's reply just finished; his name sends the final before command inference.
    a.tools.send_context.lock().unwrap().finished_ns = Some(start + 7 * S);
    let mut words = super::plumbing_tests::timed("the new setting right away. Ask me for a longer answer and talk over me in the middle.", start + 1_800 * MS);
    let his = "Heimdall, can you hear your own voice coming back?";
    words.extend(super::plumbing_tests::timed(his, words.last().unwrap().end_ns + 600 * MS));
    let (onset, end) = (words[0].start_ns, words.last().unwrap().end_ns);
    let text = listen::Transcript::from_words(words.clone()).text;
    a.audio.insert(mic.clone(), AudioRing { start: onset - S, samples: vec![0.01; ((end + 2 * S - onset) / 62500) as usize].into() });
    a.listen(ListenEvent::SpeechStart { source: mic.clone(), host_time_ns: onset }).unwrap();
    a.listen(ListenEvent::SpeechEnd { source: mic.clone(), host_time_ns: end }).unwrap();
    a.hooks.send.push_back(0.99);
    a.listen(ListenEvent::FinalUtterance { source: mic.clone(), text, start_ns: onset, end_ns: end, words, forced: false, end_of_turn_probability: 0.9 }).unwrap();
    a.agent.as_ref().unwrap().barrier().unwrap();
    let rows = a.index.utterances_between(onset, end + S).unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["text"].as_str(), Some(his), "{}", rows[0]);
    let decision = &rows[0]["decision"];
    assert_eq!(decision["trigger"].as_str(), Some("name"), "the early path was not taken: {decision}");
    // What the decider read and what the agent was given.
    let state = decision["prompt"]["state"].as_str().unwrap();
    let given: Vec<String> = records(&a).into_iter().filter(|r| r.kind == "agent_context_routing" && r.payload["given_to_agent"] == true)
        .filter_map(|r| r.payload["observation"]["text"].as_str().map(str::to_owned)).collect();
    assert_eq!(given.last().map(String::as_str), Some(his), "{given:?}");
    for delivered in [state, given.join("\n").as_str()] {
        assert!(delivered.contains(his), "{delivered}");
        assert!(!delivered.contains("talk over me") && !delivered.contains("right away"), "the echo reached the delivery: {delivered}");
    }
}

/// Bug note 23 Oct 17:34 (build 748cb57), replayed through the pipeline with the synthetic copy of that test
/// (pipeline/echo_guard_incidents_synthetic.json, invented sentences): a whole reply came back on the microphone
/// during the follow-up window, Heimdall's name inside it heard as two short words. The old guard kept those two words
/// as the user's and the agent woke with the entire reply. All of it is Heimdall's own voice: archived only, no decision,
/// no wake.
#[test]
fn the_17_34_reply_echo_with_heimdalls_name_in_it_wakes_nothing() {
    #[derive(serde::Deserialize)]
    struct Fixture { played: Vec<Span>, mic: Vec<Line> }
    #[derive(serde::Deserialize)]
    struct Span { start_ns: u64, end_ns: u64, text: String }
    #[derive(serde::Deserialize)]
    struct Line { id: String, text: String, words: Vec<W> }
    #[derive(serde::Deserialize)]
    struct W { text: String, start_ns: u64, end_ns: u64 }
    let f: Fixture = serde_json::from_str(include_str!("../echo_guard_incidents_synthetic.json")).unwrap();
    let (_t, mut a, mic) = rig(true);
    let now = capture::host_now_ns();
    turn(&mut a, &mic, now - 90 * S, "I was thinking about the build", Detect::Final, 0, Some(0.), 0, |_| {});
    a.agent.as_ref().unwrap().barrier().unwrap();
    let wakes = count(&a, "agent_wake");
    let decided = count(&a, "send_decision");
    // The fixture's clock moved to a minute ago.
    let line = f.mic.iter().find(|l| l.id == "A").unwrap();
    let (first, last) = (line.words[0].start_ns, line.words.last().unwrap().end_ns);
    let shift = (now - 60 * S) - first;
    let reply: Vec<&Span> = f.played.iter().filter(|p| p.start_ns + S >= first && p.start_ns <= last).collect();
    for p in &reply {
        a.tools.speech_policy.sentence_started(&p.text, p.start_ns + shift, true);
    }
    a.tools.speech_policy.voice_stopped(reply.last().unwrap().end_ns + shift);
    a.tools.send_context.lock().unwrap().finished_ns = Some(reply[0].start_ns + shift);
    let words: Vec<Word> = line.words.iter().map(|w| Word { text: w.text.clone(), start_ns: w.start_ns + shift, end_ns: w.end_ns + shift, confidence: 0.9 }).collect();
    let (onset, end) = (words[0].start_ns, words.last().unwrap().end_ns);
    a.audio.insert(mic.clone(), AudioRing { start: onset - S, samples: vec![0.01; ((end + 2 * S - onset) / 62500) as usize].into() });
    a.listen(ListenEvent::SpeechStart { source: mic.clone(), host_time_ns: onset }).unwrap();
    a.listen(ListenEvent::SpeechEnd { source: mic.clone(), host_time_ns: end }).unwrap();
    a.hooks.send.push_back(0.99);
    a.listen(ListenEvent::FinalUtterance { source: mic.clone(), text: line.text.clone(), start_ns: onset, end_ns: end, words, forced: false, end_of_turn_probability: 0.7 }).unwrap();
    a.agent.as_ref().unwrap().barrier().unwrap();
    assert_eq!((count(&a, "send_decision"), count(&a, "agent_wake")), (decided, wakes), "the echo reached the decider or woke the agent");
    assert_eq!(a.hooks.send.pop_front(), Some(0.99));
    let rows = a.index.utterances_between(onset, end + S).unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!((rows[0]["speaker"].as_str(), rows[0]["decision"]["status"].as_str()), (Some("Heimdall"), Some("archive_only")), "{}", rows[0]);
}

/// One spoken path to the agent, in a fresh rig with Wake Sound `on`: how many cues played and the send's trigger.
fn cue_for(path: &str, on: bool) -> (usize, Option<String>) {
    let (t, mut a, mic) = rig(true);
    a.action(ui::UiAction::SetWakeSound(on)).unwrap();
    let cues = cue_counter(&a);
    match path {
        "name" => turn(&mut a, &mic, 10 * S, "Heimdall check the build", Detect::Final, 0, Some(0.), 0, |_| {}),
        "send_it" => turn(&mut a, &mic, 10 * S, "check the build and send it", Detect::Final, 0, Some(0.), 0, |_| {}),
        "d1" => turn(&mut a, &mic, 10 * S, "check the build for me", Detect::Final, 0, Some(0.99), 0, |_| {}),
        "dropped" => {
            let shot = super::drop_tests::png(t.path());
            turn(&mut a, &mic, 2 * S, "what do you make of these two", Detect::Final, 0, Some(0.), 0, |a| super::drop_tests::drop_on_notch(a, &shot));
        }
        "forced" => {
            // The endpoint forced the final (endless speech), with his name in it.
            let words = super::plumbing_tests::timed("Heimdall check the build", 10 * S);
            let end = words.last().unwrap().end_ns;
            a.audio.insert(mic.clone(), AudioRing { start: 5 * S, samples: vec![0.01; ((end - 5 * S) / 62500) as usize].into() });
            a.listen(ListenEvent::SpeechStart { source: mic.clone(), host_time_ns: 10 * S }).unwrap();
            a.listen(ListenEvent::SpeechEnd { source: mic.clone(), host_time_ns: end }).unwrap();
            a.hooks.send.push_back(0.);
            let text = listen::Transcript::from_words(words.clone()).text;
            a.listen(ListenEvent::FinalUtterance { source: mic.clone(), text, start_ns: 10 * S, end_ns: end, words, forced: true, end_of_turn_probability: 0.2 }).unwrap();
        }
        "typed" => {
            turn(&mut a, &mic, 10 * S, "An unfinished thought", Detect::Final, 0, Some(0.), 0, |_| {});
            a.agent.as_ref().unwrap().window_action(ui::AgentWindowAction::Send { text: "Use the current folder".into(), steer: false }).unwrap();
        }
        other => panic!("no path {other}"),
    }
    a.agent.as_ref().unwrap().barrier().unwrap();
    let trigger = records(&a).iter().filter(|r| r.kind == "send_decision" && r.payload["outcome"] == "send").last()
        .and_then(|r| r.payload["trigger"].as_str().map(str::to_owned));
    let played = cues.load(Ordering::SeqCst);
    a.tools.shutdown();
    (played, trigger)
}

/// "the wake sound doesn't play". Every spoken path that sends to the agent plays the cue exactly
/// once with Wake Sound on and never with it off; a typed message too.
#[test]
fn every_spoken_send_plays_the_cue_once_and_never_with_wake_sound_off() {
    for (path, trigger) in [("name", "name"), ("send_it", "send_it"), ("d1", "d1"), ("dropped", "dropped"), ("forced", "name")] {
        let (played, sent) = cue_for(path, true);
        assert_eq!(sent.as_deref(), Some(trigger), "{path}: the send's trigger");
        assert_eq!(played, 1, "{path}: cues with Wake Sound on");
        assert_eq!(cue_for(path, false), (0, Some(trigger.to_owned())), "{path}: Wake Sound off still sends, silently");
    }
    assert_eq!(cue_for("typed", true).0, 1, "a typed message rings like a wake (bug note 2026-10-30T18-54-31)");
    assert_eq!(cue_for("typed", false).0, 0, "Wake Sound off: a typed message is silent");
}

/// AirPods as the Mac's output (26 Oct 15:49-16:12, every wake): the cue plays while Speaking is on, since the reply
/// claims them anyway; with Speaking off it would be Heimdall's only sound there and could pull them off his iPhone
/// (22 Oct 02:54), so it stays quiet.
#[test]
fn the_cue_plays_on_bluetooth_while_speaking_is_on() {
    for (speaking, expected) in [(true, 1), (false, 0)] {
        let (_t, mut a, mic) = rig(true);
        a.tools.speech_policy.test_set_bluetooth(true);
        a.action(ui::UiAction::SetSpeakReplies(speaking)).unwrap();
        let cues = cue_counter(&a);
        turn(&mut a, &mic, 10 * S, "Heimdall check the build", Detect::Final, 0, Some(0.), 0, |_| {});
        a.agent.as_ref().unwrap().barrier().unwrap();
        assert_eq!(count(&a, "agent_wake"), 1);
        assert_eq!(cues.load(Ordering::SeqCst), expected, "Speaking {speaking}");
        a.tools.shutdown();
    }
}

/// Bug note 2026-10-26T18-35-49: a reply held because the user was in a call left no record, so "not heard" could not be
/// told from "never sent". One record, and one log line.
#[test]
fn a_reply_held_in_a_call_leaves_one_record() {
    let (_t, mut a, _mic) = rig(true);
    let spoken = Arc::new(AtomicUsize::new(0));
    a.tools.test_guarded_speech_queue(Synth(spoken.clone()), Output);
    a.tools.mcp_actions.attach(ui::Ui::handle(), Arc::new(|_, _, _| Ok(())), Arc::new(|_, _| Ok(())), Arc::new(|_, _| Ok(()))).unwrap();
    let call = discord(&a);
    turn(&mut a, &call, capture::host_now_ns().saturating_sub(3 * S), "did you see the new build", Detect::Final, 0, None, 0, |_| {});
    let held = a.tools.mcp_actions.speak(&a.tools, "Mobius finished.", "en", "speak-held", &a.tools.agent_enabled.permit().unwrap()).unwrap();
    assert_eq!(held, crate::bridge::speech_policy::HELD_IN_CONVERSATION);
    let session = a.tools.archive_session.lock().unwrap().clone().unwrap();
    let records: Vec<_> = heimdall_archive::Journal::open_read_only(&session).unwrap().records().unwrap().into_iter().filter(|r| r.kind == "speech_held").collect();
    assert_eq!(records.len(), 1);
    let (line, _) = crate::diagnostics::evidence_line(&records[0]).unwrap();
    assert_eq!(line, "speech: speech_held: event: in a call, shown on the island");
    assert_eq!(spoken.load(Ordering::SeqCst), 0);
}

/// Records what it was asked to say, in order.
struct Said(Arc<Mutex<Vec<String>>>);
impl speak::Synthesizer for Said {
    fn synthesize(&mut self, text: &str, _: &str, _: &dyn Fn() -> bool) -> Result<speak::Audio> {
        self.0.lock().unwrap().push(text.into());
        Ok(speak::Audio { samples: vec![0.; 160], sample_rate: 16000, first_audio_ms: 0., synthesis_ms: 1. })
    }
}
/// A rig with a recording speech queue and the action channel attached, the user in a Discord call 3 s ago.
fn in_a_call() -> (tempfile::TempDir, Pipeline, String, Arc<Mutex<Vec<String>>>) {
    let (t, mut a, mic) = rig(true);
    let said = Arc::new(Mutex::new(vec![]));
    a.tools.test_guarded_speech_queue(Said(said.clone()), Output);
    a.tools.mcp_actions.attach(ui::Ui::handle(), Arc::new(|_, _, _| Ok(())), Arc::new(|_, _| Ok(())), Arc::new(|_, _| Ok(()))).unwrap();
    let call = discord(&a);
    turn(&mut a, &call, capture::host_now_ns().saturating_sub(3 * S), "did you see the new build", Detect::Final, 0, None, 0, |_| {});
    assert!(a.tools.speech_policy.in_conversation(capture::host_now_ns()));
    (t, a, mic, said)
}
fn say_now(a: &Pipeline, text: &str, op: &str) -> String {
    a.tools.mcp_actions.speak(&a.tools, text, "en", op, &a.tools.agent_enabled.permit().unwrap()).unwrap()
}
/// The call is over: its last line ended a moment more than the conversation window ago.
fn call_over(a: &Pipeline) {
    a.tools.speech_policy.test_last_line(capture::host_now_ns() - crate::bridge::speech_policy::CONVERSATION_NS - S);
}
/// What the speech queue has said once it is idle, after a few status checks (each may replay).
fn said_after(a: &Pipeline, said: &Mutex<Vec<String>>) -> Vec<String> {
    for _ in 0..3 {
        a.tools.speech_status();
        while a.tools.agent_speech_busy() { std::thread::sleep(std::time::Duration::from_millis(5)); }
    }
    a.tools.speech_status();
    said.lock().unwrap().clone()
}

/// 4 Nov: 46 replies held in a Discord call were shown on the island and never heard. Once the call is over (and the user
/// is quiet), each held line is said through the normal path, with no lead-in, once; nothing plays while the call
/// lasts or while he talks.
#[test]
fn a_reply_held_in_a_call_is_said_once_the_call_is_over() {
    let (_t, a, mic, said) = in_a_call();
    assert_eq!(say_now(&a, "Mobius finished.", "speak-held"), crate::bridge::speech_policy::HELD_IN_CONVERSATION);
    a.tools.agent_final_answer("Tests passed.", "stop").unwrap();
    assert_eq!(a.tools.speech_policy.test_held().iter().map(|l| l.text.as_str()).collect::<Vec<_>>(), ["Mobius finished.", "Tests passed."]);
    assert!(said_after(&a, &said).is_empty(), "a held line played during the call");
    assert_eq!(a.tools.speech_policy.test_held().len(), 2, "held lines were lost during the call");

    // The call is over but the user is talking: still nothing.
    call_over(&a);
    a.tools.owner.set_quiet(std::time::Duration::ZERO);
    a.tools.voice(&mic, true);
    assert!(said_after(&a, &said).is_empty(), "a held line played over User");
    // Speaking off: nothing either; the lines wait.
    a.tools.voice(&mic, false);
    a.tools.enabled.store(false, Ordering::Release);
    assert!(said_after(&a, &said).is_empty(), "a held line played with Speaking off");
    a.tools.enabled.store(true, Ordering::Release);

    // He is quiet and Speaking is on: both lines, in order, as written, once.
    assert_eq!(said_after(&a, &said), ["Mobius finished.", "Tests passed."]);
    assert!(a.tools.speech_policy.test_held().is_empty());
    assert_eq!(said_after(&a, &said).len(), 2, "a held line was said twice");
}

/// A held line is not said after the call when it is older than ten minutes, or when a newer reply was spoken after
/// it: the conversation moved on. (the user's words reaching the agent after it no longer drop it: bug 2026-11-04T17-48-24,
/// `his_words_to_heimdall_say_the_newest_held_reply_in_a_live_call`.)
#[test]
fn a_held_line_stale_or_superseded_is_dropped_unsaid() {
    let held = crate::bridge::speech_policy::HELD_IN_CONVERSATION;
    // Stale.
    let (_t, a, _mic, said) = in_a_call();
    assert_eq!(say_now(&a, "Old news.", "speak-1"), held);
    a.tools.speech_policy.test_age_held(crate::bridge::speech_policy::HELD_FOR.as_nanos() as u64 + S);
    call_over(&a);
    assert!(said_after(&a, &said).is_empty(), "a stale line was said");
    assert!(a.tools.speech_policy.test_held().is_empty());

    // The user's words reached the agent after two held lines: the older is dropped, the newer said (bug
    // 2026-11-04T17-48-24; before, both were dropped unheard).
    let (_t, a, _mic, said) = in_a_call();
    assert_eq!(say_now(&a, "The build is green.", "speak-1"), held);
    assert_eq!(say_now(&a, "The deploy is queued.", "speak-2"), held);
    a.tools.speech_policy.owner_woke("what about the deploy", capture::host_now_ns());
    call_over(&a);
    assert_eq!(said_after(&a, &said), ["The deploy is queued."]);
    assert!(a.tools.speech_policy.test_held().is_empty());

    // A newer reply was spoken after it (here in the call, to a run he woke by name).
    let (_t, a, _mic, said) = in_a_call();
    assert_eq!(say_now(&a, "The build is green.", "speak-1"), held);
    a.tools.speech_policy.addressed_wake();
    assert!(say_now(&a, "It is five in Tokyo.", "speak-2").starts_with("Speech queued"));
    a.tools.speech_policy.run_settled();
    call_over(&a);
    assert_eq!(said_after(&a, &said), ["It is five in Tokyo."], "a superseded line was said");
    assert!(a.tools.speech_policy.test_held().is_empty());

    // Agent Off and On again since it was held: never said (review 4 Nov).
    let (_t, a, _mic, said) = in_a_call();
    assert_eq!(say_now(&a, "The build is green.", "speak-1"), held);
    a.tools.set_agent_enabled(false);
    a.tools.set_agent_enabled(true);
    call_over(&a);
    assert!(said_after(&a, &said).is_empty(), "a line from before Agent Off was said");
    assert!(a.tools.speech_policy.test_held().is_empty());

    // Stop (or the user talking over Heimdall) drops what was held.
    let (_t, a, _mic, said) = in_a_call();
    assert_eq!(say_now(&a, "The build is green.", "speak-1"), held);
    a.tools.interrupt();
    call_over(&a);
    assert!(said_after(&a, &said).is_empty(), "a line from before Stop was said");

    // At most four wait; the oldest goes first.
    let (_t, a, _mic, said) = in_a_call();
    for i in 1..=5 { assert_eq!(say_now(&a, &format!("Line {i}."), &format!("speak-{i}")), held); }
    call_over(&a);
    assert_eq!(said_after(&a, &said), ["Line 2.", "Line 3.", "Line 4.", "Line 5."]);
}

/// 1 Nov 00:17-00:21: three replies were lost to "database is locked" (the speak_back journal write failed, so the
/// line never queued). Evidence and receipts are bookkeeping: their store failing never costs the user the line.
#[test]
fn a_failing_archive_or_receipt_store_never_costs_the_spoken_line() {
    // The speak_back journal write fails (an address the archive refuses).
    let (t, a, _mic) = rig(true);
    let said = Arc::new(Mutex::new(vec![]));
    a.tools.test_guarded_speech_queue(Said(said.clone()), Output);
    a.tools.mcp_actions.attach(ui::Ui::handle(), Arc::new(|_, _, _| Ok(())), Arc::new(|_, _| Ok(())), Arc::new(|_, _| Ok(()))).unwrap();
    let real = a.tools.archive_session.lock().unwrap().replace(t.path().join("not-a-session"));
    assert!(say_now(&a, "Archive or not, you hear this.", "speak-1").starts_with("Speech queued"));
    assert_eq!(said_after(&a, &said), ["Archive or not, you hear this."]);
    *a.tools.archive_session.lock().unwrap() = real;

    // Another connection holds the receipts' write lock past their busy wait.
    let rival = rusqlite::Connection::open(t.path().join("data/agent/mcp-actions.sqlite")).unwrap();
    rival.execute_batch("BEGIN IMMEDIATE; INSERT INTO outbox(id,text) VALUES('rival','held');").unwrap();
    assert!(say_now(&a, "Locked receipts, still said.", "speak-2").starts_with("Speech queued"));
    assert_eq!(said_after(&a, &said), ["Archive or not, you hear this.", "Locked receipts, still said."]);
    rival.execute_batch("ROLLBACK").unwrap();
}

/// Bug 2026-11-04T17-48-24, 17:46-17:48: Angel's last line on the Discord call ended at 17:46:29 and the user talked to
/// Heimdall from 17:46:50, but the two-minute call window held every reply until 17:48:29. Call lines stop; 60 s later
/// he speaks to Heimdall: he is not in a call, and the reply is spoken.
#[test]
fn a_minute_after_the_last_call_line_he_is_not_in_a_call_and_the_reply_is_spoken() {
    let (_t, mut a, mic) = rig(true);
    let said = Arc::new(Mutex::new(vec![]));
    a.tools.test_guarded_speech_queue(Said(said.clone()), Output);
    a.tools.mcp_actions.attach(ui::Ui::handle(), Arc::new(|_, _, _| Ok(())), Arc::new(|_, _| Ok(())), Arc::new(|_, _| Ok(()))).unwrap();
    let call = discord(&a);
    let now = capture::host_now_ns();
    // Angel's last line (not a goodbye here), then nothing on the call for a minute.
    turn(&mut a, &call, now - 63 * S, "yeah the build looks good to me", Detect::Final, 0, None, 0, |_| {});
    assert!(a.tools.speech_policy.in_conversation(now - 55 * S), "the call was live just after the line");
    a.tools.owner.set_quiet(std::time::Duration::ZERO);
    turn(&mut a, &mic, now - 8 * S, "can you open me the Heimdall website", Detect::Final, 0, Some(0.), 0, |_| {});
    assert!(!a.tools.speech_policy.in_conversation(capture::host_now_ns()), "still in a call a minute after its last line");
    assert!(say_now(&a, "The Heimdall page is open.", "speak-1").starts_with("Speech queued"));
    assert_eq!(said_after(&a, &said), ["The Heimdall page is open."]);
}

/// 17:46: "Sorry, bro, take care. Good luck." ended the call, and so does the user's "you can talk to me now" said while
/// the call is still live: nothing is held after either.
#[test]
fn a_goodbye_mid_line_or_his_off_the_call_ends_the_call_at_once() {
    let (_t, mut a, _mic, said) = in_a_call();
    let call = discord(&a);
    let now = capture::host_now_ns();
    turn(&mut a, &call, now - 6 * S, "Sorry, bro, take care. Good luck.", Detect::Final, 0, None, 0, |_| {});
    assert!(!a.tools.speech_policy.in_conversation(capture::host_now_ns() + S), "the goodbye did not end the call");
    assert!(say_now(&a, "Everything's fine.", "speak-1").starts_with("Speech queued"));

    let (_t, mut a2, mic2, said2) = in_a_call();
    assert_eq!(say_now(&a2, "Held in the call.", "speak-1"), crate::bridge::speech_policy::HELD_IN_CONVERSATION);
    a2.tools.owner.set_quiet(std::time::Duration::ZERO);
    turn(&mut a2, &mic2, capture::host_now_ns() - 6 * S, "Okay, you can talk to me now.", Detect::Final, 0, Some(0.), 0, |_| {});
    assert!(!a2.tools.speech_policy.in_conversation(capture::host_now_ns() + S), "his 'you can talk to me now' did not end the call");
    assert_eq!(said_after(&a2, &said2), ["Held in the call."], "the held line is said once the call is over");
    assert_eq!(said_after(&a, &said), ["Everything's fine."]);
}

/// While a call is really live: a reply is held, then the user addresses Heimdall (his wake reaches the agent): the newest
/// held reply is spoken at once, call or not, and the older one is dropped (both were shown on the island when held).
/// A held reply with only call talk after it, his words never reaching the agent, still waits.
#[test]
fn his_words_to_heimdall_say_the_newest_held_reply_in_a_live_call() {
    let held = crate::bridge::speech_policy::HELD_IN_CONVERSATION;
    let (_t, mut a, mic, said) = in_a_call();
    assert_eq!(say_now(&a, "Everything's fine.", "speak-1"), held);
    assert_eq!(say_now(&a, "Still the 08:14 build.", "speak-2"), held);
    let n = notices(&a);
    assert!(n.iter().any(|b| b.contains("Everything's fine.")) && n.iter().any(|b| b.contains("Still the 08:14 build.")), "held lines are on the island: {n:?}");
    // Call talk and his aside to the call: nothing reaches the agent, nothing is said.
    let call = discord(&a);
    a.tools.owner.set_quiet(std::time::Duration::ZERO);
    turn(&mut a, &call, capture::host_now_ns() - 8 * S, "so are you coming tonight", Detect::Final, 0, None, 0, |_| {});
    turn(&mut a, &mic, capture::host_now_ns() - 5 * S, "yeah I think so", Detect::Final, 0, Some(0.), 0, |_| {});
    assert!(said_after(&a, &said).is_empty(), "an unaddressed line released a held reply");
    assert_eq!(a.tools.speech_policy.test_held().len(), 2);
    // He talks to Heimdall: his wake reaches the agent while the call is still live.
    a.tools.speech_policy.owner_woke("Heimdall, hello?", capture::host_now_ns());
    assert!(a.tools.speech_policy.in_conversation(capture::host_now_ns()), "the call is still live");
    assert_eq!(said_after(&a, &said), ["Still the 08:14 build."]);
    assert!(a.tools.speech_policy.test_held().is_empty());
    assert_eq!(said_after(&a, &said).len(), 1, "said twice");
}

/// A typed message in a live call stops what plays but keeps the held replies, and his words release the newest
/// (before, `typed_arrived` dropped every held line as "speech was stopped").
#[test]
fn a_typed_message_in_a_call_says_the_newest_held_reply() {
    let held = crate::bridge::speech_policy::HELD_IN_CONVERSATION;
    let (_t, a, _mic, said) = in_a_call();
    assert_eq!(say_now(&a, "Everything's fine.", "speak-1"), held);
    a.tools.typed_arrived("is everything ok?");
    assert_eq!(a.tools.speech_policy.test_held().len(), 1, "the typed message dropped the held line");
    a.tools.speech_policy.owner_woke("is everything ok?", capture::host_now_ns());
    assert_eq!(said_after(&a, &said), ["Everything's fine."]);
}

/// The call source stopping playing ends the call a few seconds later, through the real capture event; a stalled tap
/// (the call may go on unheard) leaves only the window.
#[test]
fn the_call_source_stopping_ends_the_call_through_capture() {
    let gone = crate::bridge::speech_policy::CALL_SOURCE_GONE.as_nanos() as u64;
    let (_t, mut a, _mic, _said) = in_a_call();
    let call = discord(&a);
    let now = capture::host_now_ns();
    a.capture_event(capture::CaptureEvent::SourceRemoved { source_id: capture::SourceId(call.clone()), host_ns: now, reason: "no callbacks; reconciling active source".into() }).unwrap();
    assert!(a.tools.speech_policy.in_conversation(now + gone + S), "a stalled tap ended the call");

    let (_t, mut a, _mic, _said) = in_a_call();
    let call = discord(&a);
    let now = capture::host_now_ns();
    a.capture_event(capture::CaptureEvent::SourceRemoved { source_id: capture::SourceId(call), host_ns: now, reason: "source stopped / route or format changed".into() }).unwrap();
    assert!(a.tools.speech_policy.in_conversation(now + gone - S), "a route change returns within ~100 ms");
    assert!(!a.tools.speech_policy.in_conversation(now + gone + S), "the call source stopped");
}
