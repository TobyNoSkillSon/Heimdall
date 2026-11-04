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

/// Speech that ends before the first check (a cough, "mhm") is not judged; a new onset starts over.
#[test]
fn a_short_sound_is_not_judged() {
    let (_t, mut a, mic, speech) = speaking(SAID, 2 * S, false);
    a.listen(ListenEvent::SpeechStart { source: mic.clone(), host_time_ns: 2 * S }).unwrap();
    scores(&mut a, &[(0.9, 0.1)]);
    hear(&mut a, &mic, 2 * S + 400 * MS);
    a.listen(ListenEvent::SpeechEnd { source: mic.clone(), host_time_ns: 2 * S + 400 * MS }).unwrap();
    hear(&mut a, &mic, 4 * S);
    speech.still_speaking();
    assert_eq!(reasons(&a), ["too_short"]);
    a.listen(ListenEvent::SpeechStart { source: mic.clone(), host_time_ns: 4 * S }).unwrap();
    scores(&mut a, &[(0.9, 0.1)]);
    hear(&mut a, &mic, 5 * S + 800 * MS);
    speech.cancelled(SAID);
    assert_eq!(reasons(&a), ["too_short", "user_voice_once", "user_voice"]);
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
/// later sample joins only if it matches at least half of those kept (one stray voice can never admit another), at
/// most `USER_MAX_SAMPLES` with the oldest leaving; misfits that disagree with each other never replace it, five that
/// agree do (his mic or voice changed).
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
    // Guests and the room: misfits that do not agree with each other never replace him.
    for (n, i) in [0usize, 10, 20, 30, 40, 60, 70].into_iter().enumerate() {
        assert!(!p.add(unit(i), 100 + n as u64));
    }
    assert!(p.embeddings.iter().all(|e| e.cosine(&unit(50)) > 0.99));
    // Five that agree: his voice changed.
    for n in 0..heimdall_voice::USER_RESTART - 1 {
        assert!(!p.add(unit(90), 200 + n as u64));
    }
    assert!(p.add(unit(90), 300));
    assert_eq!(p.embeddings.len(), heimdall_voice::USER_RESTART);
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
    a.last_call_line_ns = Some(40 * S);
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
