//! Barge-in by voice (decision 31): while Heimdall's voice plays, speech that speaker ID confidently
//! matches to the user stops it at once; other voices and Heimdall's own echo never do. No side transcript, no decider.
//! End to end through the real `Pipeline` with the encoder's scores injected (pipeline/heimdall_voice.rs `Scores`).
use super::plumbing_tests::{rig, Speech};
use super::*;

const S: u64 = 1_000_000_000;
const MS: u64 = 1_000_000;
/// The 13:35:33 reply (bug note 24 Oct 13:35); the fake synthesizer gets one clause at a time.
const SAID: &str = "hold Yes, it worked. The Shure heard my whole reply through the speakers, and it was correctly labelled as me.";

fn speaking(saying: &str, from: u64, speakers: bool) -> (tempfile::TempDir, Pipeline, String, Speech) {
    let (t, mut a, mic) = rig(false);
    a.tools.set_agent_enabled(true);
    a.hooks.speakers = speakers;
    a.hooks.voice_enrolled = true;
    let speech = Speech::install(&a);
    speech.say(&a, saying);
    a.audio.insert(mic.clone(), AudioRing { start: 0, samples: vec![0.01; (from / 62500) as usize].into() });
    (t, a, mic, speech)
}
fn hear(a: &mut Pipeline, mic: &str, until: u64) {
    while a.audio[mic].end() < until {
        let at = a.audio[mic].end();
        a.audio.get_mut(mic).unwrap().push(at, &[0.01; 320]);
        a.barge_audio(mic).unwrap();
    }
}
fn barges(a: &Pipeline) -> Vec<serde_json::Value> {
    a.index.evidence.records().unwrap().into_iter().filter(|r| r.kind == "barge_in").map(|r| r.payload).collect()
}
fn reasons(a: &Pipeline) -> Vec<String> {
    barges(a).iter().map(|b| b["reason"].as_str().unwrap_or(b["outcome"].as_str().unwrap_or("")).to_string()).collect()
}
/// Queue one window's scores: (user, heimdall).
fn scores(a: &mut Pipeline, windows: &[(f32, f32)]) {
    for (u, h) in windows {
        a.hooks.user_voice.push_back(Some(*u));
        a.hooks.voice.push_back(Some(*h));
    }
}

/// His voice in two windows in a row stops Heimdall: the first check 1.2 s after the onset (a 1.5 s window, the 300 ms
/// before the onset included), the stop at the second, half a second later. Nothing is transcribed and no decider is
/// asked: the records hold only the scores.
#[test]
fn the_users_voice_stops_heimdall_at_the_second_window_in_a_row() {
    for speakers in [false, true] {
        let (_t, mut a, mic, speech) = speaking(SAID, 2 * S, speakers);
        a.listen(ListenEvent::SpeechStart { source: mic.clone(), host_time_ns: 2 * S }).unwrap();
        scores(&mut a, &[(0.72, 0.12), (0.75, 0.10)]);
        hear(&mut a, &mic, 3 * S + 100 * MS);
        assert!(barges(&a).is_empty(), "not before the window is full");
        hear(&mut a, &mic, 3 * S + 220 * MS);
        speech.still_speaking();
        assert_eq!(reasons(&a), ["user_voice_once"]);
        hear(&mut a, &mic, 3 * S + 720 * MS);
        speech.cancelled(SAID);
        let b = barges(&a);
        assert_eq!(reasons(&a), ["user_voice_once", "user_voice"]);
        assert_eq!((b[1]["outcome"].as_str(), b[1]["voice"]["verdict"].as_str(), b[1]["streak"].as_u64()), (Some("stopped"), Some("user"), Some(2)));
        assert_eq!((b[0]["voice"]["window"]["start_ns"].as_u64(), b[0]["voice"]["window"]["end_ns"].as_u64()), (Some(2 * S - 300 * MS), Some(3 * S + 200 * MS)));
        assert_eq!(b[1]["voice"]["window"]["end_ns"].as_u64(), Some(3 * S + 700 * MS));
        assert_eq!(b[1]["output"].as_str(), Some(if speakers { "speakers" } else { "headphones" }));
        assert!(b[1]["side_transcript"].is_null() && b[1]["input"].is_null() && b[1]["policy"] == barge_in::POLICY);
        assert!(a.barge.is_none());
    }
}

/// A window too quiet to be speech is never scored (near-silent windows on his mic score like him), and it breaks the
/// streak: one window of his voice, a quiet one, one more: Heimdall talks on.
#[test]
fn a_quiet_window_is_not_scored_and_breaks_the_streak() {
    let (_t, mut a, mic, speech) = speaking(SAID, 2 * S, true);
    a.listen(ListenEvent::SpeechStart { source: mic.clone(), host_time_ns: 2 * S }).unwrap();
    scores(&mut a, &[(0.8, 0.1), (0.8, 0.1)]);
    hear(&mut a, &mic, 3 * S + 220 * MS);
    // 1.5 s of near silence.
    while a.audio[&mic].end() < 5 * S {
        let at = a.audio[&mic].end();
        a.audio.get_mut(&mic).unwrap().push(at, &[0.002; 320]);
        a.barge_audio(&mic).unwrap();
    }
    speech.still_speaking();
    let r = reasons(&a);
    assert_eq!(r[0], "user_voice_once");
    assert!(r[1..].iter().all(|x| x == "quiet"), "{r:?}");
    assert_eq!(a.hooks.user_voice.len(), 1, "quiet windows were never scored");
    a.tools.interrupt();
    speech.cancelled(SAID);
}

/// Through speakers: the echo alone never stops Heimdall (the 13:35:37 cut, now scored), however long; then the user over
/// the echo does, at the first window where his voice is confident and beats Heimdall's.
#[test]
fn echo_never_stops_heimdall_and_owner_over_it_does() {
    let (_t, mut a, mic, speech) = speaking(SAID, 2 * S, true);
    a.listen(ListenEvent::SpeechStart { source: mic.clone(), host_time_ns: 2 * S }).unwrap();
    // Echo: Heimdall's voice, or unsure; his score low or not beating Heimdall's by the margin.
    scores(&mut a, &[(0.10, 0.71), (0.30, 0.58), (0.55, 0.50), (0.48, 0.42), (0.20, 0.30)]);
    hear(&mut a, &mic, 5 * S + 300 * MS);
    speech.still_speaking();
    assert_eq!(reasons(&a), ["voice_heimdall", "voice_heimdall", "voice_heimdall", "voice_unsure", "voice_other"]);
    assert_eq!(barges(&a)[1]["voice"]["window"]["end_ns"].as_u64(), Some(3 * S + 700 * MS), "every half second");
    // He talks over it.
    scores(&mut a, &[(0.61, 0.33), (0.66, 0.30)]);
    hear(&mut a, &mic, 6 * S + 300 * MS);
    speech.cancelled(SAID);
    assert_eq!(reasons(&a).last().map(String::as_str), Some("user_voice"));
}

/// Another person's voice (a guest in the room, a video) never stops Heimdall, and keeps being checked; when the user
/// starts talking over it, his voice stops Heimdall.
#[test]
fn another_voice_never_stops_heimdall_but_owner_starting_does() {
    let (_t, mut a, mic, speech) = speaking(SAID, 2 * S, false);
    a.hooks.voice_enrolled = false;
    a.listen(ListenEvent::SpeechStart { source: mic.clone(), host_time_ns: 2 * S }).unwrap();
    for _ in 0..6 {
        a.hooks.user_voice.push_back(Some(0.15));
    }
    hear(&mut a, &mic, 5 * S + 800 * MS);
    speech.still_speaking();
    assert_eq!(reasons(&a), ["voice_unsure"; 6]);
    a.hooks.user_voice.extend([Some(0.66), Some(0.70)]);
    hear(&mut a, &mic, 6 * S + 800 * MS);
    speech.cancelled(SAID);
    assert_eq!(reasons(&a).last().map(String::as_str), Some("user_voice"));
}

/// Before his voice is known (his first wakes): a window confidently not Heimdall's voice counts as his, so he can
/// always stop it; Heimdall's own voice still never does, and with no profile at all nothing is judged.
#[test]
fn before_his_voice_is_known_a_voice_not_heimdalls_stops_it() {
    let (_t, mut a, mic, speech) = speaking(SAID, 2 * S, true);
    a.listen(ListenEvent::SpeechStart { source: mic.clone(), host_time_ns: 2 * S }).unwrap();
    a.hooks.voice.extend([Some(0.66), Some(0.42), Some(0.12), Some(0.15)]);
    hear(&mut a, &mic, 4 * S + 800 * MS);
    speech.cancelled(SAID);
    assert_eq!(reasons(&a), ["voice_heimdall", "voice_unsure", "user_voice_once", "user_voice"]);
    assert!(barges(&a)[3]["voice"]["user_cosine"].is_null());

    let (_t, mut a, mic, speech) = speaking(SAID, 2 * S, false);
    a.hooks.voice_enrolled = false;
    a.listen(ListenEvent::SpeechStart { source: mic.clone(), host_time_ns: 2 * S }).unwrap();
    hear(&mut a, &mic, 4 * S);
    speech.still_speaking();
    assert!(reasons(&a).iter().all(|r| r == "voice_check_failed"), "{:?}", reasons(&a));
    assert_eq!(barges(&a)[0]["voice"]["error"], "no profile or encoder");
    a.tools.interrupt();
    speech.cancelled(SAID);
}

/// A sound too short for two checks in a row (a 100 ms click) never stops Heimdall, even in his voice: its one check
/// runs in the tail after it ends, and the barge-in is over 1.5 s after his last voice. A new onset starts over.
#[test]
fn a_short_sound_never_stops_heimdall() {
    let (_t, mut a, mic, speech) = speaking(SAID, 2 * S, false);
    a.listen(ListenEvent::SpeechStart { source: mic.clone(), host_time_ns: 2 * S }).unwrap();
    scores(&mut a, &[(0.9, 0.1)]);
    hear(&mut a, &mic, 2 * S + 100 * MS);
    a.listen(ListenEvent::SpeechEnd { source: mic.clone(), host_time_ns: 2 * S + 100 * MS }).unwrap();
    hear(&mut a, &mic, 4 * S);
    speech.still_speaking();
    assert_eq!(reasons(&a), ["user_voice_once"]);
    assert!(a.barge.is_none(), "over at the tail's end");
    a.listen(ListenEvent::SpeechStart { source: mic.clone(), host_time_ns: 4 * S }).unwrap();
    scores(&mut a, &[(0.9, 0.1), (0.9, 0.1)]);
    hear(&mut a, &mic, 5 * S + 800 * MS);
    speech.cancelled(SAID);
    assert_eq!(reasons(&a), ["user_voice_once", "user_voice_once", "user_voice"]);
}

/// A short "stop" (0.4 s) whose end the speech detector reports 300 ms after his last voice (Smart Turn ends his turns
/// early since 11 Nov) still stops Heimdall, as it did when the end came 1.5 s late: the checks at 1.2 s and 1.7 s
/// after the onset run in the tail. Without the tail it was "too short" and Heimdall talked on.
#[test]
fn a_short_stop_ending_early_still_stops_heimdall() {
    for speakers in [false, true] {
        let (_t, mut a, mic, speech) = speaking(SAID, 2 * S, speakers);
        a.listen(ListenEvent::SpeechStart { source: mic.clone(), host_time_ns: 2 * S }).unwrap();
        scores(&mut a, &[(0.72, 0.12), (0.75, 0.10)]);
        hear(&mut a, &mic, 2 * S + 700 * MS);
        a.listen(ListenEvent::SpeechEnd { source: mic.clone(), host_time_ns: 2 * S + 400 * MS }).unwrap();
        hear(&mut a, &mic, 3 * S + 720 * MS);
        speech.cancelled(SAID);
        assert_eq!(reasons(&a), ["user_voice_once", "user_voice"]);
    }
}

/// Speech starting again within the tail is the same barge-in: its streak goes on.
#[test]
fn speech_resuming_in_the_tail_keeps_the_barge_in() {
    let (_t, mut a, mic, speech) = speaking(SAID, 2 * S, false);
    a.listen(ListenEvent::SpeechStart { source: mic.clone(), host_time_ns: 2 * S }).unwrap();
    scores(&mut a, &[(0.1, 0.1), (0.8, 0.1), (0.8, 0.1)]);
    hear(&mut a, &mic, 2 * S + 600 * MS);
    a.listen(ListenEvent::SpeechEnd { source: mic.clone(), host_time_ns: 2 * S + 600 * MS }).unwrap();
    hear(&mut a, &mic, 3 * S + 600 * MS);
    a.listen(ListenEvent::SpeechStart { source: mic.clone(), host_time_ns: 3 * S + 600 * MS }).unwrap();
    hear(&mut a, &mic, 5 * S);
    speech.cancelled(SAID);
    assert_eq!(reasons(&a), ["voice_other", "user_voice_once", "user_voice"]);
}

/// A failed or missing check is no confident speaker: Heimdall goes on and the next window is checked.
#[test]
fn a_failed_check_never_stops_heimdall() {
    let (_t, mut a, mic, speech) = speaking(SAID, 2 * S, true);
    a.listen(ListenEvent::SpeechStart { source: mic.clone(), host_time_ns: 2 * S }).unwrap();
    a.hooks.voice.extend([None, Some(f32::NAN)]);
    a.hooks.user_voice.extend([Some(0.9), Some(f32::NAN)]);
    hear(&mut a, &mic, 3 * S + 750 * MS);
    speech.still_speaking();
    assert_eq!(reasons(&a), ["voice_check_failed", "voice_unsure"]);
    a.tools.interrupt();
    speech.cancelled(SAID);
}

/// An answer that never comes (the encoder's thread gone) does not leave the barge-in waiting: three seconds of audio
/// later the window counts as unchecked and the newest window is asked; the user can still stop Heimdall.
#[test]
fn a_lost_answer_never_blocks_later_checks() {
    let (_t, mut a, mic, speech) = speaking(SAID, 2 * S, true);
    a.listen(ListenEvent::SpeechStart { source: mic.clone(), host_time_ns: 2 * S }).unwrap();
    scores(&mut a, &[(0.1, 0.7), (0.7, 0.1), (0.7, 0.1)]);
    a.hooks.lose_voice_answers = 1;
    hear(&mut a, &mic, 6 * S);
    speech.still_speaking();
    assert!(barges(&a).is_empty(), "waiting for the answer (3.2 s window, 3 s allowance)");
    hear(&mut a, &mic, 6 * S + 800 * MS);
    speech.cancelled(SAID);
    assert_eq!(reasons(&a), ["voice_check_failed", "user_voice_once", "user_voice"]);
    assert_eq!(barges(&a)[0]["voice"]["error"], "no answer");
    assert_eq!(barges(&a)[1]["voice"]["window"]["end_ns"].as_u64(), Some(6 * S + 200 * MS));
}

/// An answer from before a newer reply never acts on it.
#[test]
fn an_answer_from_an_older_reply_is_dropped() {
    let (_t, mut a, mic, speech) = speaking(SAID, 2 * S, true);
    a.listen(ListenEvent::SpeechStart { source: mic.clone(), host_time_ns: 2 * S }).unwrap();
    scores(&mut a, &[(0.9, 0.1)]);
    a.hooks.lose_voice_answers = 1;
    hear(&mut a, &mic, 3 * S + 300 * MS);
    let id = a.next_side_id - 1;
    assert!(a.barge.is_some(), "waiting for the answer");
    a.tools.interrupt();
    speech.cancelled(SAID);
    a.barge_voice(id, Ok(heimdall_voice::Scores { user: Some(0.9), heimdall: None })).unwrap();
    assert!(a.barge.is_none() && barges(&a).is_empty(), "nothing judged");
}

/// The verdict rule on its own: his voice at the bar and beating Heimdall's by the margin; Heimdall's own voice never.
#[test]
fn the_user_verdict_rule() {
    use heimdall_voice::{user_verdict, Scores, Who};
    let s = |u: Option<f32>, h: Option<f32>| Scores { user: u, heimdall: h.map(|c| ("v".to_string(), c)) };
    assert_eq!(user_verdict(&s(Some(heimdall_voice::USER_MIN), None)), Who::User);
    assert_eq!(user_verdict(&s(Some(heimdall_voice::USER_MIN - 0.01), None)), Who::Unsure);
    assert_eq!(user_verdict(&s(Some(0.9), Some(heimdall_voice::HEIMDALL_MIN))), Who::Heimdall, "its own voice never stops it");
    assert_eq!(user_verdict(&s(Some(0.50), Some(0.41))), Who::Unsure, "not beating Heimdall's score by the margin");
    assert_eq!(user_verdict(&s(Some(0.6), Some(0.2))), Who::User);
    assert_eq!(user_verdict(&s(Some(0.1), Some(0.1))), Who::Other);
    assert_eq!(user_verdict(&s(Some(f32::NAN), None)), Who::Unsure);
    assert_eq!(user_verdict(&s(None, Some(0.1))), Who::User, "no profile yet: not Heimdall's voice");
    assert_eq!(user_verdict(&s(None, Some(0.7))), Who::Heimdall);
    assert_eq!(user_verdict(&s(None, None)), Who::NoProfile);
}

/// His profile is learned from his wakes: the first three must agree with each other (a misfit starts it again), a
/// later sample joins only if it matches at least half of those kept and his first ones (one stray voice can never
/// admit another), at most `USER_MAX_SAMPLES` with the oldest leaving; misfits never replace it, whether they agree
/// with each other or not (12 Nov: a microphone that changed gets a set of its own, from supported lines only).
#[test]
fn the_users_profile_learns_only_his_voice() {
    let unit = |i: usize| { let mut v = vec![0.; 192]; v[i] = 1.; v[i + 1] = 0.2; speakers::embedding::Embedding::new(v).unwrap() };
    let mut p = heimdall_voice::UserVoice::default();
    assert!(p.add(unit(0), 1));
    assert!(p.add(unit(50), 2), "a misfit before the profile is ready starts it again");
    assert_eq!(p.embeddings.len(), 1);
    for n in 0..2 {
        assert!(p.add(unit(50), 3 + n));
    }
    assert!(p.ready());
    assert!(!p.add(unit(0), 6), "another voice does not join");
    for n in 0..30 {
        p.add(unit(50), 10 + n);
    }
    assert_eq!(p.embeddings.len(), heimdall_voice::USER_MAX_SAMPLES);
    // Guests and the room: misfits never replace him.
    for (n, i) in [0usize, 10, 20, 30, 40, 60, 70].into_iter().enumerate() {
        assert!(!p.add(unit(i), 100 + n as u64));
    }
    for n in 0..2 * heimdall_voice::USER_RESTART {
        assert!(!p.add(unit(90), 200 + n as u64), "five that agree, with nothing but the voice, change nothing");
    }
    assert!(p.embeddings.iter().all(|e| e.cosine(&unit(50)) > 0.99));
    assert!(p.devices.is_empty());
}

/// A turn of his that woke Heimdall is a sample of his voice (saved, used at once); one in a call or with Heimdall
/// speaking over it is not.
#[test]
fn his_wakes_teach_the_profile_and_calls_do_not() {
    let (_t, mut a, mic) = rig(false);
    let unit = |i: usize| { let mut v = vec![0.; 192]; v[i] = 1.; speakers::embedding::Embedding::new(v).unwrap() };
    for n in 0..3u64 {
        a.hooks.user_samples.push_back(unit(0));
        super::plumbing_tests::turn(&mut a, &mic, (2 + 10 * n) * S, "Heimdall can you check the build for me please", super::plumbing_tests::Detect::Final, 0, Some(1.), S / 2, |_| {});
    }
    assert!(a.heimdall_voice.user.ready(), "three wakes: {} samples", a.heimdall_voice.user.embeddings.len());
    let saved = heimdall_voice::UserVoice::load(&a.data.join(heimdall_voice::USER_FILE));
    assert_eq!(saved.embeddings.len(), 3);
    // A call is going on: his wake is not a clean sample of his voice.
    a.tools.speech_policy.call_line(40 * S, true);
    a.hooks.user_samples.push_back(unit(0));
    super::plumbing_tests::turn(&mut a, &mic, 42 * S, "Heimdall can you check the build for me please", super::plumbing_tests::Detect::Final, 0, Some(1.), S / 2, |_| {});
    assert_eq!(a.heimdall_voice.user.embeddings.len(), 3);
    assert_eq!(a.hooks.user_samples.len(), 1, "not asked");
}

/// An enrolment is saved beside the voices already in `speakers.json` (named people kept), used at once, and a newer
/// recipe or voice replaces only its own entry.
#[test]
fn an_enrolled_voice_is_saved_with_the_others_and_used_at_once() {
    let (_t, mut a, _mic) = rig(false);
    let path = a.data.join("speakers.json");
    let unit = |i: usize| { let mut v = vec![0.; 192]; v[i] = 1.; speakers::embedding::Embedding::new(v).unwrap() };
    let mut store = speakers::identity::EnrollmentStore::default();
    store.enroll("Vim", unit(3)).unwrap();
    store.save(&path).unwrap();
    let voice = |i| speakers::identity::HeimdallVoice { embeddings: vec![unit(i)], recipe: heimdall_voice::RECIPE.into(), lines: 7, enrolled_ms: 1 };
    a.heimdall_voice_enrolled("drama3".into(), Ok(voice(0))).unwrap();
    a.heimdall_voice_enrolled("supertonic".into(), Ok(voice(1))).unwrap();
    let saved = speakers::identity::EnrollmentStore::load(&path).unwrap();
    assert_eq!(saved.names.keys().collect::<Vec<_>>(), ["Vim"]);
    assert_eq!(saved.heimdall.keys().collect::<Vec<_>>(), ["drama3", "supertonic"]);
    assert_eq!(a.heimdall_voice.similarity(&unit(1)), Some(("supertonic".into(), 1.)));
    assert!(matches!(saved.identify(&unit(0)), speakers::identity::Identity::Known { ref name, .. } if name == "Heimdall"), "calls name it Heimdall");
    // A failed enrolment saves nothing and waits before trying that voice again.
    a.heimdall_voice_enrolled("phonon-Edwin".into(), Err(anyhow::anyhow!("Phonon S is not resident"))).unwrap();
    assert!(a.heimdall_voice.retry.contains_key("phonon-Edwin") && !speakers::identity::EnrollmentStore::load(&path).unwrap().heimdall.contains_key("phonon-Edwin"));
    assert!(a.heimdall_voice.enrolling.is_none());
}

/// Bug note 2026-11-07T04-31-47 (04:30:56 on 7 Nov): a 1.2-second onset on the Shure that never held a word skipped a
/// reply the user wanted to hear. Now a reply that comes up over speech on his microphone waits for the speaker check: a
/// voice that is not his lets it play while that speech goes on; his voice skips it, as before. Through the real
/// pipeline and hold, with the encoder's scores injected.
#[test]
fn a_reply_waits_for_the_speaker_check_and_only_his_voice_skips_it() {
    use std::time::{Duration, Instant};
    struct Quick;
    impl speak::Synthesizer for Quick {
        fn synthesize(&mut self, _: &str, _: &str, _: &dyn Fn() -> bool) -> Result<speak::Audio> {
            Ok(speak::Audio { samples: vec![0.1; 160], sample_rate: 16000, first_audio_ms: 0., synthesis_ms: 1. })
        }
    }
    struct Count(Arc<std::sync::atomic::AtomicUsize>);
    impl speak::AudioOutput for Count {
        fn output(&mut self, _: &speak::Audio, _: &dyn Fn() -> bool) -> Result<()> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
    }
    // (user, Heimdall) scores of the first window, and whether the reply is skipped.
    for (user, heimdall, skipped) in [(0.15, 0.12, false), (0.30, 0.71, false), (0.72, 0.12, true)] {
        let (_t, mut a, mic) = rig(false);
        a.tools.set_agent_enabled(true);
        a.hooks.voice_enrolled = true;
        let played = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        a.tools.test_held_speech_queue(Quick, Count(played.clone()));
        a.audio.insert(mic.clone(), AudioRing { start: 0, samples: vec![0.01; (2 * S / 62500) as usize].into() });
        // The listen worker reports the onset before the pipeline does (sources.rs); the live tick has already told
        // the hold this microphone is checked, so even that first report is unverified (review, 7 Nov).
        a.hold_verify_tick();
        a.tools.voice(&mic, true);
        assert!(a.tools.owner.unverified(&mic), "{user}: the worker's onset waits for the check");
        a.listen(ListenEvent::SpeechStart { source: mic.clone(), host_time_ns: 2 * S }).unwrap();
        agent::mcp::ToolHost::speak(&*a.tools, "The zoom view is rebuilt.", "en", &a.tools.agent_enabled.permit().unwrap()).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(played.load(std::sync::atomic::Ordering::SeqCst), 0, "{user}: not over the onset");
        assert!(!a.tools.owner.skipping(), "{user}: an onset alone skips nothing");
        scores(&mut a, &[(user, heimdall)]);
        hear(&mut a, &mic, 3 * S + 300 * MS);
        let b = barges(&a);
        assert_eq!((b[0]["armed_by"].as_str(), b[0]["outcome"].as_str()), (Some("hold_check"), Some("hold_check")), "{user}: {b:?}");
        let began = Instant::now();
        while played.load(std::sync::atomic::Ordering::SeqCst) == 0 && !a.tools.owner.skipping() && began.elapsed() < Duration::from_secs(2) {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(a.tools.owner.skipping(), skipped, "{user}");
        assert_eq!(played.load(std::sync::atomic::Ordering::SeqCst) > 0, !skipped, "{user}");
        a.tools.interrupt();
    }
}

// ---- his own headset: bug note 2026-11-13T13-23-33 (AirPods talk-over did not stop Heimdall) ----

/// Talk-overs replayed from the archive (window scores only; lab/airpods-barge-in/make_fixture.py).
fn talk_overs() -> Vec<serde_json::Value> {
    let v: serde_json::Value = serde_json::from_str(include_str!("barge_fixtures/talk-overs-2026-11-13.json")).unwrap();
    v["talk_overs"].as_array().unwrap().clone()
}
fn windows(t: &serde_json::Value) -> Vec<(f32, f32)> {
    t["windows"].as_array().unwrap().iter().map(|w| (w[0].as_f64().unwrap() as f32, w[1].as_f64().unwrap() as f32)).collect()
}
/// The 13:22:53 talk-over on his AirPods (Heimdall in the headphones): ten windows, none two in a row at 0.60.
fn the_13_22_talk_over() -> Vec<(f32, f32)> {
    let t = talk_overs();
    let t = t.iter().find(|t| t["mic"] == "AirPods Pro" && t["windows"].as_array().unwrap().len() == 10).expect("in the fixture");
    windows(t)
}
const AIRPODS: &str = "AirPods Pro [FC-B2-14-F4-23-C9:input]";
/// Replays `w` as the encoder's answers to a talk-over on a microphone named `label`; returns the reasons.
fn replay_talk_over(label: &str, speakers: bool, w: &[(f32, f32)]) -> (Vec<String>, Vec<String>, bool) {
    let (_t, mut a, mic, speech) = speaking(SAID, 2 * S, speakers);
    a.sources.get_mut(&mic).unwrap().label = label.into();
    a.listen(ListenEvent::SpeechStart { source: mic.clone(), host_time_ns: 2 * S }).unwrap();
    scores(&mut a, w);
    // Every window answered (1.2 s after the onset, then every 0.5 s), or the stop. The audio here comes far faster
    // than real time; live, a stop's cancel ends the speech long before its second of rest (`REST_NS`) is over. So at
    // the stop the replay waits for the speech to end, as real time would (14 Nov: under load the rest ran out first
    // and a new check armed on the still-ending speech, recording `heimdall_finished`; a cancel held back 5 ms in the
    // fake synthesizer failed the old replay every time).
    let until = 3 * S + 220 * MS + 500 * MS * (w.len() as u64 - 1);
    let mut cancelled = false;
    while a.audio[&mic].end() < until {
        let next = a.audio[&mic].end() + 1;
        hear(&mut a, &mic, next);
        if !cancelled && a.barge_rest_until > 0 {
            speech.cancelled(SAID);
            cancelled = true;
            let began = std::time::Instant::now();
            while a.tools.agent_speech_busy() || a.tools.heimdall_speaking() {
                assert!(began.elapsed() < std::time::Duration::from_secs(5), "the cancelled speech never ended");
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        }
    }
    let stopped = !a.tools.heimdall_speaking();
    if !stopped { a.tools.interrupt(); }
    if !cancelled { speech.cancelled(SAID); }
    (reasons(&a), a.hooks.voice_devices.clone(), stopped)
}

/// Bug note 2026-11-13T13-23-33: the 13:22 AirPods talk-over, replayed, now stops Heimdall at its fourth window
/// (2.7 s after his first word; on the Shure his stops come at the second to fourth); each check named the AirPods, so
/// his score used their own set when he has one.
#[test]
fn his_airpods_talk_over_stops_heimdall() {
    let (r, devices, stopped) = replay_talk_over(AIRPODS, false, &the_13_22_talk_over());
    assert!(stopped, "{r:?}");
    assert_eq!(r, ["voice_other", "voice_other", "user_voice_once", "user_voice"]);
    assert!(!devices.is_empty() && devices.iter().all(|d| d == AIRPODS), "{devices:?}");
}

/// The same windows keep today's bar elsewhere: on the Shure (a room microphone) and on the AirPods while Heimdall
/// plays through the Mac's speakers (his voice may reach them), two windows in a row at 0.60 never came.
#[test]
fn the_headset_bar_is_only_for_his_headset_in_the_headphones() {
    let w = the_13_22_talk_over();
    for (label, speakers) in [("Shure MV7i", false), (AIRPODS, true), ("iPhone Microphone", false)] {
        let (r, _, stopped) = replay_talk_over(label, speakers, &w);
        assert!(!stopped, "{label} {speakers}: {r:?}");
        assert!(r.iter().all(|x| x != "user_voice"), "{r:?}");
    }
}

/// Every archived talk-over (12-13 Nov) through the window verdict: on his headset in the headphones no talk-over
/// stops later than before, and no talk-over that was not his (every window under 0.40) stops at either bar.
#[test]
fn archived_talk_overs_stop_no_later_and_others_never() {
    use heimdall_voice::{user_verdict_at, Scores, Who, HEADSET_USER_MIN, USER_MIN, USER_WINDOWS};
    let stop_at = |w: &[(f32, f32)], min: f32| {
        let mut streak = 0;
        w.iter().position(|(u, h)| {
            let s = Scores { user: Some(*u), heimdall: Some(("v".into(), *h)) };
            streak = if user_verdict_at(&s, min) == Who::User { streak + 1 } else { 0 };
            streak >= USER_WINDOWS
        })
    };
    let all = talk_overs();
    assert!(all.len() >= 30, "{}", all.len());
    for t in &all {
        let w = windows(t);
        let (before, now) = (stop_at(&w, USER_MIN), stop_at(&w, HEADSET_USER_MIN));
        if let Some(b) = before { assert!(now.is_some_and(|n| n <= b), "{t}"); }
        if w.iter().all(|(u, _)| *u < 0.40) { assert_eq!(now, None, "{t}"); }
        // What happened live is what the old bar says (the fixture is the live record).
        if t["reason"] == "user_voice" { assert!(before.is_some(), "{t}"); }
    }
}
