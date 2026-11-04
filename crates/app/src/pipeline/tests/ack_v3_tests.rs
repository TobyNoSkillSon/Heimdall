//! Decider v3 picks the instant acknowledgement (night 23 Oct; pipeline/ack.rs `AckPlan`), end to end through the real
//! `Pipeline` with stubbed models and a fake Pi. Live evidence 22 Oct 20:14–23 Oct 02:11: 86 acks for 95 spoken wakes,
//! because the rule acked every wake (lab/decider-v3/ACK-RATE.md). With v3 only the decider's own line plays: its
//! plain send says nothing. Wakes sent before it answers (name, "send it", follow-up) take its verdict on the same
//! words when it comes ("sometimes you acknowledge, sometimes you don't"); v2 keeps the rule.
use super::plumbing_tests::{rig, turn, Detect};
use super::*;
use std::sync::atomic::AtomicUsize;

const S: u64 = 1_000_000_000;

fn records(a: &Pipeline, kind: &str) -> Vec<serde_json::Value> {
    a.index.evidence.records().unwrap().into_iter().filter(|r| r.kind == kind).map(|r| r.payload).collect()
}
fn take(a: &Pipeline, line: &str, text: &str, lang: &str) {
    let mut settings = speak::drama3::settings();
    settings["asr"] = serde_json::json!({"passed":true});
    a.tools.voice_bank.store().unwrap().insert(&voicebank::ClipMeta { origin: voicebank::Origin::Bank, line_id: Some(line.into()),
        text: text.into(), language: lang.into(), voice: speak::drama3::VOICE.into(), generator: speak::drama3::GENERATOR.into(), settings },
        &speak::Audio { samples: vec![0.1; 20_000], sample_rate: 44100, first_audio_ms: 0., synthesis_ms: 0. }).unwrap();
}
fn cues(a: &Pipeline) -> Arc<AtomicUsize> {
    let played = Arc::new(AtomicUsize::new(0));
    let count = played.clone();
    a.tools.speech_policy.set_player(Arc::new(move || { count.fetch_add(1, Ordering::SeqCst); }));
    played
}
/// Agent on, speaking on, takes for "On it.", "Will do." and last night's "Sprawdzam.", decider v3 (or v2).
fn v3_rig(v3: bool) -> (tempfile::TempDir, Pipeline, String, Arc<AtomicUsize>, Arc<std::sync::Mutex<Vec<(usize, bool)>>>) {
    let (t, mut a, mic) = rig(true);
    a.settings.speak_replies = true;
    a.hooks.decider_v3 = Some(v3);
    take(&a, "en_on_it", "On it.", "en");
    take(&a, "en_will_do", "Will do.", "en");
    take(&a, "pl_sprawdzam", "Sprawdzam.", "pl");
    let cue = cues(&a);
    let log = crate::voice_bank::tests::probe(&a.tools, std::time::Duration::from_millis(1));
    (t, a, mic, cue, log)
}
/// The wake went out, its ack is over, and the fake Pi's spoken reply has left the speech queue: the next turn starts
/// with Heimdall quiet. The fake Pi is its own process and answers on its own time, so the barrier alone can return
/// before its reply exists; under a full parallel run that reply then reached the speech queue during the second
/// turn, and that ack was rightly skipped as `agent_speech` (an ack never plays over or after a reply). So wait for
/// every wake's completion first: the supervisor queues a reply's speech (`heimdall_final_answer`) before it reads,
/// and records, that turn's completion.
fn settle(a: &Pipeline) {
    a.agent.as_ref().unwrap().barrier().unwrap();
    let began = std::time::Instant::now();
    let waited = |what: &str| assert!(began.elapsed() < std::time::Duration::from_secs(30), "{what}");
    loop {
        let r = a.index.evidence.records().unwrap();
        let wakes = r.iter().filter(|r| r.kind == "agent_wake").count();
        let done = r.iter().filter(|r| r.kind == "agent_reply" && r.payload["event"]["kind"] == "complete" && r.payload["event"]["query"] == true).count();
        if done >= wakes { break; }
        waited("a wake's turn never completed");
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    crate::voice_bank::tests::wait_idle(&a.tools);
    while a.tools.agent_speech_busy() {
        waited("the reply never left the speech queue");
        a.tools.speech_status();
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

#[test]
fn the_plan_follows_the_decider_generation() {
    let (_t, mut a, _mic) = rig(false);
    a.hooks.decider_v3 = Some(false);
    assert_eq!(a.ack_plan(Some(Some("en_on_it"))), ack::AckPlan::Rule, "v2: the rule, whatever the verdict");
    assert_eq!(a.ack_plan(None), ack::AckPlan::Rule);
    a.hooks.decider_v3 = Some(true);
    assert_eq!(a.ack_plan(Some(Some("en_on_it"))), ack::AckPlan::Line("en_on_it".into()));
    assert_eq!(a.ack_plan(Some(None)), ack::AckPlan::Silent("decider_plain_send"));
    assert_eq!(a.ack_plan(None), ack::AckPlan::Later);
    a.hooks.decider_v3 = None;
    assert!(a.decider_picks_acks(), "the shipped decider picks acks (d1::DEFAULT_MODEL: V6 since 4 Nov, V5's questions)");
}

/// d1 v3 sends with "On it.": that line plays (not the rule's pick), instead of the wake sound, and the wake says so.
#[test]
fn v3_plays_the_line_the_decider_picked() {
    let (_t, mut a, mic, cue, log) = v3_rig(true);
    a.hooks.send_v3.push_back(Some("en_will_do".into()));
    turn(&mut a, &mic, 10 * S, "rename the island file please", Detect::Final, 0, None, 0, |_| {});
    settle(&a);
    let ack = records(&a, "voice_ack");
    assert_eq!(ack.len(), 1, "{ack:#?}");
    assert_eq!((ack[0]["outcome"].as_str(), ack[0]["text"].as_str(), ack[0]["decider_line"].as_str()),
        (Some("playing"), Some("Will do."), Some("en_will_do")), "{:#}", ack[0]);
    assert_eq!(records(&a, "send_decision")[0]["ack_line"], "en_will_do");
    assert_eq!(log.lock().unwrap().len(), 1);
    assert_eq!(cue.load(Ordering::SeqCst), 0, "no wake sound on top");
    assert_eq!(records(&a, "agent_wake").len(), 1);
    // Polish: the decider's Polish line no longer exists (English-only acks), so nothing plays, though the
    // store still has last night's Polish take.
    a.hooks.send_v3.push_back(Some("pl_sprawdzam".into()));
    turn(&mut a, &mic, 40 * S, "sprawdź czy build już przeszedł", Detect::Final, 0, None, 0, |_| {});
    settle(&a);
    let ack = records(&a, "voice_ack");
    // Day 26: the ack gate may stop it first (no_request: the gate reads English requests only); either way nothing plays.
    assert_eq!(ack[1]["outcome"].as_str(), Some("nothing"), "{:#}", ack[1]);
    assert!(matches!(ack[1]["reason"].as_str(), Some("unknown_line" | "no_request")), "{:#}", ack[1]);
    assert_eq!(log.lock().unwrap().len(), 1, "no Polish take played");
    assert_eq!(records(&a, "agent_wake").len(), 2, "the wake still goes");
}

/// Plain send, a line without a take, an unknown id: nothing is said, the wake sound plays, the wake still goes.
#[test]
fn v3_plain_send_or_a_line_without_a_clip_says_nothing() {
    let (_t, mut a, mic, cue, log) = v3_rig(true);
    a.hooks.send_v3.push_back(None);
    turn(&mut a, &mic, 10 * S, "what time is it in Tokyo", Detect::Final, 0, None, 0, |_| {});
    settle(&a);
    a.hooks.send_v3.push_back(Some("en_noted".into()));
    turn(&mut a, &mic, 40 * S, "note that the build is slow", Detect::Final, 0, None, 0, |_| {});
    settle(&a);
    a.hooks.send_v3.push_back(Some("en_dance".into()));
    turn(&mut a, &mic, 70 * S, "open the build log", Detect::Final, 0, None, 0, |_| {});
    settle(&a);
    let reasons: Vec<_> = records(&a, "voice_ack").iter().map(|r| (r["outcome"].as_str().unwrap().to_owned(), r["reason"].as_str().unwrap_or("").to_owned())).collect();
    assert_eq!(reasons, [("nothing".into(), "decider_plain_send".into()), ("nothing".into(), "no_clip_for_language".into()), ("nothing".into(), "unknown_line".into())]);
    assert!(log.lock().unwrap().is_empty(), "nothing played");
    assert_eq!(records(&a, "agent_wake").len(), 3, "every send still wakes");
    assert_eq!(cue.load(Ordering::SeqCst), 3, "with the wake sound");
    assert!(a.tools.take_ack_line(Some(u64::MAX)).is_none(), "and no line is claimed to the agent");
}

/// The context notes told to the agent from now on (the wake itself goes through the agent's own handle, started
/// here as the first final would).
fn notes(a: &mut Pipeline) -> tokio::sync::mpsc::UnboundedReceiver<crate::bridge::Command> {
    if a.agent.is_none() {
        a.agent = Some(AgentRuntime::start(a.settings.clone(), a.data.clone(), a.session.clone(), a.tools.clone(), a.ui).unwrap());
    }
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    *a.tools.agent_commands.lock().unwrap() = Some(tx);
    rx
}
fn said(rx: &mut tokio::sync::mpsc::UnboundedReceiver<crate::bridge::Command>, within: std::time::Duration) -> Vec<(String, String)> {
    let deadline = std::time::Instant::now() + within;
    let mut out = vec![];
    loop {
        while let Ok(command) = rx.try_recv() {
            if let crate::bridge::Command::Notice { id, text, .. } = command { out.push((id, text)); }
        }
        if !out.is_empty() || std::time::Instant::now() >= deadline { return out; }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}
/// The clock the ack reads: `after` past the last word of `text` spoken from `at`.
fn after_words(a: &mut Pipeline, text: &str, at: u64, after: u64) {
    a.hooks.ack_now = Some(super::plumbing_tests::timed(text, at).last().unwrap().end_ns + after);
}

/// His name wakes at once, before d1 answers; d1's line on the same words then plays (here he has been
/// quiet long enough already; the wait for 2.5 s of quiet is `pipeline::ack` `a_late_line_waits_out_the_quiet...`),
/// instead of the wake sound, and the agent is told by a context note once it plays (the wake had already gone
/// without it). One wake, one decision.
#[test]
fn v3_name_wake_plays_the_line_d1_picks_after_quiet_and_the_agent_learns_it() {
    let (_t, mut a, mic, cue, log) = v3_rig(true);
    let mut rx = notes(&mut a);
    // The wake goes before d1 answers, so a quick fake Pi could have its reply queued first, and then the line is
    // rightly not said (an ack never plays after a reply): under a full parallel run it sometimes was. Its reply
    // waits until the line plays.
    let hold = a.data.join("hold-reply");
    std::fs::write(&hold, b"hold").unwrap();
    a.hooks.send_v3.push_back(Some("en_will_do".into()));
    turn(&mut a, &mic, 10 * S, "Heimdall, open the build log.", Detect::Final, 0, None, 0, |_| {});
    let began = std::time::Instant::now();
    while log.lock().unwrap().is_empty() && began.elapsed() < std::time::Duration::from_secs(10) {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    std::fs::remove_file(&hold).unwrap();
    settle(&a);
    let ack = records(&a, "voice_ack");
    assert_eq!(log.lock().unwrap().len(), 1, "played once: {ack:#?}");
    assert_eq!(ack.len(), 2, "{ack:#?}");
    assert_eq!((ack[0]["outcome"].as_str(), ack[0]["reason"].as_str()), (Some("nothing"), Some("awaiting_decider")), "{:#}", ack[0]);
    assert_eq!((ack[1]["outcome"].as_str(), ack[1]["text"].as_str(), ack[1]["late"].as_bool()), (Some("playing"), Some("Will do."), Some(true)), "{:#}", ack[1]);
    let decisions = records(&a, "send_decision");
    assert_eq!((decisions.len(), decisions[0]["trigger"].as_str()), (1, Some("name")));
    assert_eq!(records(&a, "agent_wake").len(), 1, "one wake");
    let told = said(&mut rx, std::time::Duration::from_secs(2));
    assert_eq!(told.len(), 1, "{told:?}");
    assert!(told[0].0.starts_with("ack-said:") && told[0].1.starts_with("[Heimdall already said: \"Will do.\" at "), "{told:?}");
    assert_eq!(agent::notice::NoticeKind::classify(&told[0].0, &told[0].1), agent::notice::NoticeKind::Context, "a note, not a turn");
    assert!(a.tools.take_ack_line(Some(u64::MAX)).is_none(), "nothing left for a later wake");
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert_eq!(cue.load(Ordering::SeqCst), 0, "no wake sound on top");
}

/// His name, then d1's plain send: nothing said, the wake sound plays (held for the verdict), and nothing is told.
#[test]
fn v3_name_wake_with_a_plain_send_says_nothing() {
    let (_t, mut a, mic, cue, log) = v3_rig(true);
    let mut rx = notes(&mut a);
    let text = "Heimdall, what time is it in Tokyo?";
    after_words(&mut a, text, 10 * S, 1_600_000_000);
    a.hooks.send_v3.push_back(None);
    turn(&mut a, &mic, 10 * S, text, Detect::Final, 0, None, 0, |_| {});
    settle(&a);
    let ack = records(&a, "voice_ack");
    assert_eq!((ack[1]["outcome"].as_str(), ack[1]["reason"].as_str()), (Some("nothing"), Some("decider_no_line")), "{ack:#?}");
    assert!(log.lock().unwrap().is_empty(), "nothing played");
    assert_eq!(cue.load(Ordering::SeqCst), 1, "the wake sound, once");
    std::thread::sleep(std::time::Duration::from_millis(1100));
    assert_eq!(cue.load(Ordering::SeqCst), 1, "its deadline does not sound it again");
    assert!(said(&mut rx, std::time::Duration::from_millis(50)).is_empty());
    assert_eq!(records(&a, "agent_wake").len(), 1);
}

/// His name wakes right after Heimdall spoke; the user talks again before d1's line arrives: the line is stale, nothing
/// plays, no sound over him, nothing told. (Until 29 Oct the follow-up window woke here; it is gone, decision 33.)
#[test]
fn v3_early_wake_with_a_line_after_owner_talks_again_says_nothing() {
    let (_t, mut a, mic, cue, log) = v3_rig(true);
    let mut rx = notes(&mut a);
    a.tools.playback_ended(40 * S);
    let text = "Heimdall, and the build folder too";
    after_words(&mut a, text, 42 * S, 1_600_000_000);
    a.hooks.send_v3.push_back(Some("en_on_it".into()));
    a.hooks.talk_before_verdict = true;
    turn(&mut a, &mic, 42 * S, text, Detect::Final, 0, None, 0, |_| {});
    settle(&a);
    assert_eq!(records(&a, "send_decision")[0]["trigger"], "name");
    let ack = records(&a, "voice_ack");
    assert_eq!((ack[1]["outcome"].as_str(), ack[1]["reason"].as_str()), (Some("nothing"), Some("owner_talked_again")), "{ack:#?}");
    std::thread::sleep(std::time::Duration::from_millis(1100));
    assert!(log.lock().unwrap().is_empty(), "nothing played");
    assert_eq!(cue.load(Ordering::SeqCst), 0, "no sound while he talks");
    assert!(said(&mut rx, std::time::Duration::from_millis(50)).is_empty());
    assert_eq!(records(&a, "agent_wake").len(), 1);
}

/// d1 answers after the ack's moment: the wake sound has stood in, and its line no longer plays.
#[test]
fn v3_a_verdict_after_the_acks_moment_says_nothing_and_the_sound_stood_in() {
    let (_t, mut a, mic, cue, log) = v3_rig(true);
    a.quiet_since.insert(mic.clone(), 10 * S);
    a.hooks.ack_now = Some(13 * S);
    assert!(a.defer_ack(&mic, 7, "s:7", "Heimdall, build it."));
    std::thread::sleep(std::time::Duration::from_millis(800));
    assert_eq!(cue.load(Ordering::SeqCst), 1, "the sound at the deadline");
    a.late_ack(7, Some(Some("en_on_it")));
    crate::voice_bank::tests::wait_idle(&a.tools);
    assert_eq!(records(&a, "voice_ack")[0]["reason"], "decider_too_late");
    assert!(log.lock().unwrap().is_empty());
    assert_eq!(cue.load(Ordering::SeqCst), 1);
}

/// v2 keeps the rule on wakes: his name and d1's own send ack at once, as before. A reply right after Heimdall spoke is
/// d1's to send (no follow-up window, decision 33).
#[test]
fn v2_keeps_the_rule_on_wakes_the_decider_did_not_decide() {
    let (_t, mut a, mic, cue, log) = v3_rig(false);
    a.hooks.send_v3.push_back(Some("en_will_do".into()));
    turn(&mut a, &mic, 10 * S, "Heimdall, open the build log.", Detect::Final, 0, None, 0, |_| {});
    settle(&a);
    a.tools.playback_ended(40 * S);
    a.hooks.send_v3.push_back(None);
    turn(&mut a, &mic, 42 * S, "and the build folder too", Detect::Final, 0, None, 0, |_| {});
    settle(&a);
    assert_eq!(records(&a, "agent_wake").len(), 2);
    let ack = records(&a, "voice_ack");
    assert_eq!(ack.len(), 2, "one per wake, none late: {ack:#?}");
    for r in &ack {
        assert_eq!(r["outcome"], "playing", "v2: as before: {r:#}");
        assert!(r.get("decider_line").is_none() && r.get("late").is_none(), "{r:#}");
    }
    assert_eq!(log.lock().unwrap().len(), 2);
    assert_eq!(cue.load(Ordering::SeqCst), 0, "the ack stands for the sound");
    let triggers: Vec<_> = records(&a, "send_decision").iter().map(|r| r["trigger"].as_str().unwrap().to_owned()).collect();
    assert_eq!(triggers, ["name", "d1"]);
}
