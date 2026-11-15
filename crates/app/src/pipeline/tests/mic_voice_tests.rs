//! Who speaks on the user's microphone (mic_voice.rs), end to end through the real `Pipeline` with stubbed models: his
//! microphone is his only in his voice, Heimdall's own voice is archived as Heimdall's and never decided or delivered,
//! and a guest the user listens to counts like him. Bug notes 2026-10-30T03-08-53 and 2026-10-30T03-10-22.
use super::plumbing_tests::{rig, timed, turn, Detect};
use super::*;

const S: u64 = 1_000_000_000;

fn records(a: &Pipeline, kind: &str) -> Vec<serde_json::Value> {
    a.index.evidence.records().unwrap().into_iter().filter(|r| r.kind == kind).map(|r| r.payload).collect()
}
fn outcomes(a: &Pipeline) -> Vec<(String, Option<String>)> {
    records(a, "send_decision").iter().map(|d| (d["outcome"].as_str().unwrap_or("").into(), d["trigger"].as_str().map(str::to_string))).collect()
}
/// The archived row of the utterance spoken from `from` (host clock): its speaker and decision status.
fn row(a: &Pipeline, from: u64) -> (String, String) {
    let rows = a.index.utterances_between(from, from + 30 * S).unwrap();
    let r = rows.first().unwrap_or_else(|| panic!("no utterance at {from}"));
    (r["speaker"].as_str().unwrap_or("").into(), r["decision"]["status"].as_str().unwrap_or("").into())
}

/// One utterance on his microphone with the archive's own word timings ("word@start-end", ms after `base`).
fn mic_line(a: &mut Pipeline, mic: &str, base: u64, line: &str, send: f32) -> u64 {
    let words: Vec<Word> = line.split(' ').map(|w| {
        let (text, t) = w.split_once('@').unwrap();
        let (x, y) = t.split_once('-').unwrap();
        Word { text: text.into(), start_ns: base + x.parse::<u64>().unwrap() * 1_000_000, end_ns: base + y.parse::<u64>().unwrap() * 1_000_000, confidence: 0.9 }
    }).collect();
    let (onset, end) = (words[0].start_ns, words.last().unwrap().end_ns);
    a.audio.insert(mic.into(), AudioRing { start: onset - S, samples: vec![0.02; ((end + 2 * S - onset) / 62500) as usize].into() });
    a.listen(ListenEvent::SpeechStart { source: mic.into(), host_time_ns: onset }).unwrap();
    a.listen(ListenEvent::SpeechEnd { source: mic.into(), host_time_ns: end }).unwrap();
    a.hooks.send.push_back(send);
    let text = listen::Transcript::from_words(words.clone()).text;
    a.listen(ListenEvent::FinalUtterance { source: mic.into(), text, start_ns: onset, end_ns: end, words, forced: false, end_of_turn_probability: 0.96 }).unwrap();
    onset
}

const SENTENCE_2: &str = "I'll keep talking for a bit, so interrupt me whenever you like.";
const SENTENCE_3: &str = "If echo cancellation works, my own voice coming out of the speakers won't wake me or count as you, and won't show up as your words in the transcript.";
const HEARD_2: &str = "I'll@3118-3198 keep@3188-3358 talking@3358-3518 for@3518-3598 a@3598-3678 bit,@3673-3758 so@3843-3918 interrupt@3918-4238 me@4243-4318 whenever@4318-4558 you@4558-4878 like.@4878-5518";
const HEARD_3: &str = "If@5846-6166 echo@6518-6566 cancellation@6582-7126 works,@7160-7366 my@7356-7446 own@7501-7606 voice@7606-7846 coming@7846-8086 out@8061-8166 of@8166-8246 the@8246-8326 speakers@8471-8726 won't@8846-8966 wake@8886-9206 me@9241-9366 or@9457-9526 count@9586-9766 as@9766-9846 you,@9871-10086 and@10086-10246 won't@10269-10486 show@10486-10646 up@10646-10726 as@10726-10886 your@10986-11046 words@11116-11286 in@11286-11366 the@11396-11446 transcript.@11446-12886";

/// The 03:08 echo test, replayed with the archive's timings (Jarvis at 160% through the MacBook speakers, each final
/// while its sentence still played, d1 saying send 0.76 as it did live). `voice`: the whole utterance's (user,
/// Heimdall) cosines, or None for no verdict. Returns the outcomes and both rows.
fn replay_03_08(voice: Option<[(Option<f32>, Option<f32>); 2]>) -> (Vec<(String, Option<String>)>, [(String, String); 2], usize) {
    let (_t, mut a, mic) = rig(false);
    a.tools.voice_speed.store(160, std::sync::atomic::Ordering::Release);
    let base = capture::host_now_ns() - 60 * S;
    let ms = |t: u64| base + t * 1_000_000;
    if let Some(v) = voice {
        a.hooks.mic_voice.extend(v);
    }
    a.tools.speech_policy.sentence_started("Okay, let's test it.", ms(1400), true);
    a.tools.speech_policy.sentence_started(SENTENCE_2, ms(2500), true);
    let first = mic_line(&mut a, &mic, base, HEARD_2, 0.76);
    a.tools.speech_policy.sentence_started(SENTENCE_3, ms(5850), true);
    let second = mic_line(&mut a, &mic, base, HEARD_3, 0.76);
    let verdicts = records(&a, "mic_voice").len();
    (outcomes(&a), [row(&a, first), row(&a, second)], verdicts)
}

/// Bug note 2026-10-30T03-08-53: Heimdall's reply came back on the Shure labelled the user and woke the agent. With his
/// voice print and Heimdall's own (the measured whole-utterance scores), both echoes are Heimdall's: archived only,
/// never decided. Without any verdict the text guard's fixed timing catches them too.
#[test]
fn the_03_08_echo_is_heimdalls_and_wakes_nothing() {
    let (decided, rows, verdicts) = replay_03_08(Some([(Some(0.21), Some(0.62)), (Some(0.18), Some(0.66))]));
    assert!(decided.is_empty(), "the echo reached the decider: {decided:?}");
    for r in &rows {
        assert_eq!(r, &("Heimdall".to_string(), "archive_only".to_string()));
    }
    assert_eq!(verdicts, 0, "the text guard found it first; no voice was asked");
    let (decided, rows, _) = replay_03_08(None);
    assert!(decided.is_empty(), "{decided:?}");
    assert!(rows.iter().all(|r| r.0 == "Heimdall"), "{rows:?}");
}

/// The same echo when the text guard misses (the words differ from what played: a misheard echo, or another
/// voice's text): the voice alone decides. Heimdall's voice is archived as Heimdall's; a voice that is neither his
/// nor Heimdall's is "Unknown" and never wakes, even saying the name.
#[test]
fn heimdalls_voice_without_matching_words_is_still_heimdalls() {
    let (_t, mut a, mic) = rig(false);
    let base = capture::host_now_ns() - 60 * S;
    a.hooks.mic_voice.push_back((Some(0.2), Some(0.71)));
    let first = mic_line(&mut a, &mic, base, "Echo@1000-1300 test@1300-1600 result.@1600-1900 It@2000-2100 failed.@2100-2500", 0.9);
    a.hooks.mic_voice.push_back((Some(0.31), Some(0.12)));
    let second = mic_line(&mut a, &mic, base, "Heimdall,@5000-5400 check@5400-5700 the@5700-5800 build@5800-6200 and@6300-6500 the@6500-6600 tests.@6600-7000", 0.0);
    assert_eq!(row(&a, first), ("Heimdall".into(), "archive_only".into()));
    assert_eq!(row(&a, second).0, "Unknown");
    assert_eq!(outcomes(&a), [("held_not_user".to_string(), None)], "the name from another voice is context only");
    let given: Vec<_> = records(&a, "agent_context_routing").into_iter().filter(|r| r["given_to_agent"] == true)
        .map(|r| r["observation"]["speaker"].as_str().unwrap().to_owned()).collect();
    assert_eq!(given, ["Unknown"]);
}

/// His own voice print wakes as always, by name and by d1; no verdict (no print yet, the encoder down) leaves his
/// microphone his.
#[test]
fn his_voice_wakes_and_no_verdict_keeps_the_old_rule() {
    let (_t, mut a, mic) = rig(false);
    a.hooks.mic_voice.push_back((Some(0.78), Some(0.2)));
    turn(&mut a, &mic, 10 * S, "Heimdall check the build", Detect::Final, 0, Some(0.), 2 * S, |_| {});
    a.hooks.mic_voice.push_back((Some(0.71), Some(0.15)));
    turn(&mut a, &mic, 30 * S, "can you check the build for me", Detect::Final, 0, Some(0.99), 2 * S, |_| {});
    turn(&mut a, &mic, 50 * S, "Heimdall what time is it", Detect::Final, 0, Some(0.), 2 * S, |_| {});
    let all = outcomes(&a);
    assert_eq!(all.len(), 3, "{all:?}");
    assert!(all.iter().all(|(o, _)| o == "send"), "{all:?}");
    assert_eq!(row(&a, 10 * S).0, "User");
    let voices: Vec<_> = records(&a, "mic_voice").iter().map(|r| r["voice"].as_str().unwrap().to_owned()).collect();
    assert_eq!(voices, ["owner", "owner", "unverified"]);
}

/// Another voice on his microphone (a guest through the speakers, someone in the room) saying his words: no early
/// send, no wake, no `listen_to` authority, labelled Unknown. His voice saying the same right after wakes.
#[test]
fn another_voice_on_his_microphone_never_wakes_or_grants_listening() {
    let (_t, mut a, mic) = rig(false);
    a.hooks.mic_voice.push_back((Some(0.24), Some(0.1)));
    turn(&mut a, &mic, 10 * S, "Heimdall, please listen to Alex from now on.", Detect::Final, 0, Some(0.99), 2 * S, |_| {});
    assert_eq!(outcomes(&a), [("held_not_user".to_string(), None)]);
    assert!(a.tools.speech_policy.listen_change(Some("Alex"), capture::host_now_ns()).is_err(), "an unknown voice grants nothing");
    assert!(records(&a, "agent_wake").is_empty());
    a.hooks.mic_voice.push_back((Some(0.8), Some(0.1)));
    turn(&mut a, &mic, 30 * S, "Heimdall, listen to Alex.", Detect::Final, 0, Some(0.), 2 * S, |_| {});
    assert_eq!(outcomes(&a).last().unwrap(), &("send".to_string(), Some("name".to_string())));
}

/// His words over Heimdall's echo: only the stretch the text guard kept is judged by voice. His voice there keeps
/// his words; a kept stretch not confidently his is "Unknown" (context, never a wake), never thrown away as Heimdall's
/// unless Heimdall's own score says so.
#[test]
fn over_the_echo_his_kept_words_are_judged_by_their_own_voice() {
    let (_t, mut a, mic) = rig(false);
    let base = capture::host_now_ns() - 60 * S;
    let ms = |t: u64| base + t * 1_000_000;
    a.tools.speech_policy.sentence_started("The build finished and every test passed.", ms(1000), true);
    a.tools.speech_policy.voice_stopped(ms(3500));
    a.hooks.mic_voice.push_back((Some(0.74), Some(0.3)));
    let first = mic_line(&mut a, &mic, base, "The@1100-1250 build@1250-1500 finished@1500-1900 wait@2600-2800 Heimdall@2800-3200 push@3300-3500 it@3500-3700 now@3700-3900", 0.9);
    let v = records(&a, "mic_voice");
    assert_eq!(v.len(), 1);
    assert_eq!((v[0]["stretch"].as_bool(), v[0]["start_ns"].as_u64()), (Some(true), Some(ms(2600))), "{}", v[0]);
    assert_eq!(row(&a, first).0, "User");
    assert_eq!(outcomes(&a).last().unwrap().0, "send");
    a.tools.speech_policy.sentence_started("The build finished and every test passed.", ms(20000), true);
    a.tools.speech_policy.voice_stopped(ms(22500));
    a.hooks.mic_voice.push_back((Some(0.35), Some(0.3)));
    let second = mic_line(&mut a, &mic, base, "The@20100-20250 build@20250-20500 finished@20500-20900 so@21500-21700 we@21700-21800 can@21800-22000 ship@22000-22300", 0.9);
    assert_eq!(row(&a, second).0, "Unknown");
    assert_eq!(outcomes(&a).last().unwrap(), &("held_not_user".to_string(), None), "an unsure voice over the echo never wakes");
    a.tools.speech_policy.sentence_started("The build finished and every test passed.", ms(30000), true);
    a.tools.speech_policy.voice_stopped(ms(32500));
    a.hooks.mic_voice.push_back((Some(0.3), Some(0.62)));
    let third = mic_line(&mut a, &mic, base, "The@30100-30250 build@30250-30500 finished@30500-30900 so@31500-31700 we@31700-31800 can@31800-32000 ship@32000-32300", 0.9);
    assert_eq!(row(&a, third), ("Heimdall".into(), "archive_only".into()), "Heimdall's own score: its voice");
    assert_eq!(outcomes(&a).len(), 2);
}

#[test]
fn whole_utterance_verdicts() {
    use heimdall_voice::{mic_verdict, MicVoice, Scores};
    let v = |user: Option<f32>, heimdall: Option<f32>, reliable: bool, audible: bool| {
        mic_verdict(&Scores { user, heimdall: heimdall.map(|c| ("drama3".to_string(), c)) }, reliable, audible)
    };
    let s = |user: Option<f32>, heimdall: Option<f32>| v(user, heimdall, true, false);
    assert_eq!(s(Some(0.78), Some(0.2)), MicVoice::Owner);
    assert_eq!(s(Some(0.56), Some(0.40)), MicVoice::Owner);
    assert_eq!(s(Some(0.48), Some(0.50)), MicVoice::Heimdall, "Heimdall's voice beats an unsure match of his");
    assert_eq!(s(Some(0.2), Some(0.81)), MicVoice::Heimdall, "03:08:43's echo");
    assert_eq!(s(Some(0.24), Some(0.61)), MicVoice::Heimdall, "03:08:35's echo");
    assert_eq!(s(Some(0.3), Some(0.1)), MicVoice::Unknown, "a guest heard on his microphone");
    assert_eq!(s(Some(0.3), None), MicVoice::Unknown, "no voice of Heimdall's enrolled: still not his");
    assert_eq!(s(Some(0.47), Some(0.1)), MicVoice::Unverified, "unclear: his microphone stays his");
    assert_eq!(v(Some(0.25), Some(0.1), false, false), MicVoice::Unverified, "03:08:24 'No, but': too short to call");
    assert_eq!(v(Some(0.47), Some(0.3), false, true), MicVoice::Unknown, "not his while Heimdall spoke: no benefit of the doubt");
    assert_eq!(v(Some(0.60), Some(0.55), true, false), MicVoice::Unverified, "near Heimdall's, but Heimdall was silent: not its echo");
    assert_eq!(v(Some(0.62), Some(0.56), false, true), MicVoice::Unknown, "too close to Heimdall's to be his");
    assert_eq!(s(None, Some(0.66)), MicVoice::Heimdall, "before his print exists, Heimdall's voice is still told");
    assert_eq!(s(None, Some(0.2)), MicVoice::Unverified);
    assert_eq!(s(Some(f32::NAN), Some(f32::NAN)), MicVoice::Unverified);
}

/// A microphone his print does not fit gets a set of its own from held lines that agree with each other (five on the
/// Shure or the MacBook, three on his AirPods or phone); held lines of different voices never do, a held line never
/// joins his print, and his print is never replaced (~08:25: "I should have much tighter tolerance").
#[test]
fn misfits_start_a_microphones_set_and_never_replace_his_print() {
    use heimdall_voice::UserVoice;
    let unit = |i: usize| { let mut v = vec![0.; speakers::embedding::DIMENSION]; v[i] = 1.; speakers::embedding::Embedding::new(v).unwrap() };
    // His voice through another microphone: 0.35 against his first samples (above `USER_HELD_FLOOR`, below
    // `USER_JOIN`); two different `near` voices are 0.12 apart.
    let near = |i: usize| { let mut v = vec![0.; speakers::embedding::DIMENSION]; v[0] = 0.35; v[i] = (1f32 - 0.35 * 0.35).sqrt(); speakers::embedding::Embedding::new(v).unwrap() };
    let mut u = UserVoice::default();
    for _ in 0..3 {
        assert!(u.add(unit(0), 1));
    }
    assert!(!u.misfit(unit(0), 2, "Shure MV7i") && u.embeddings.len() == 3, "a held line never joins");
    for i in 1..5 {
        assert!(!u.misfit(near(i), 3, "MacBook Pro Microphone"), "different voices never start a set");
    }
    for _ in 0..4 {
        assert!(!u.misfit(near(9), 4, "MacBook Pro Microphone"));
    }
    assert!(u.misfit(near(9), 5, "MacBook Pro Microphone"), "the fifth that agrees starts the MacBook's set");
    assert!(u.embeddings.iter().all(|e| e.cosine(&unit(0)) > 0.99), "his print is untouched");
    assert!(u.similarity_on(&near(9), "MacBook Pro Microphone").unwrap() > 0.99 && u.similarity_on(&unit(0), "MacBook Pro Microphone").unwrap() > 0.99, "on the MacBook, both are his");
    assert!(u.similarity_on(&near(9), "Shure MV7i").unwrap() < heimdall_voice::USER_JOIN, "the MacBook's set never speaks for him on the Shure");
    assert!(u.similarity(&near(9)).unwrap() < heimdall_voice::USER_JOIN, "nor for the barge-in");
    for _ in 0..2 {
        assert!(!u.misfit(near(20), 6, "User's AirPods Pro"));
    }
    assert!(u.misfit(near(20), 7, "User's AirPods Pro"), "his own headset needs three");
    assert!(!UserVoice::default().misfit(unit(1), 1, "Shure MV7i"), "no print yet: nothing to start");
    // Held lines a week apart are not one change of microphone.
    let week = heimdall_voice::USER_HELD_FOR_MS + 1;
    for n in 0..heimdall_voice::USER_RESTART as u64 {
        assert!(!u.misfit(near(40), 100 + n * week, "Studio Display Microphone"));
    }
    // A set keeps its first samples: sixteen more of a drifting voice never push them out.
    let first: Vec<_> = u.devices["MacBook Pro Microphone"].embeddings[..3].to_vec();
    for n in 0..20 {
        u.add_on(near(9), 1000 + n, "MacBook Pro Microphone", heimdall_voice::Support::Voice);
    }
    assert!(u.devices["MacBook Pro Microphone"].embeddings[..3].iter().zip(&first).all(|(a, b)| a.cosine(b) > 0.99));
    assert!(!u.misfit(near(30), 8, ""), "no microphone named: nothing counted");
}

/// Review of the night pass (13 Nov), non-blocker 3: a guest who says "Heimdall" five times on the Shure never becomes
/// The user there (each held line must reach `USER_HELD_FLOOR` against his first samples), and each held line ages on its
/// own, so five lines spread over weeks never start a set.
#[test]
fn held_lines_need_his_floor_and_age_one_by_one() {
    use heimdall_voice::UserVoice;
    let unit = |i: usize| { let mut v = vec![0.; speakers::embedding::DIMENSION]; v[i] = 1.; speakers::embedding::Embedding::new(v).unwrap() };
    let toward = |c: f32, i: usize| { let mut v = vec![0.; speakers::embedding::DIMENSION]; v[0] = c; v[i] = (1. - c * c).sqrt(); speakers::embedding::Embedding::new(v).unwrap() };
    let mut u = UserVoice::default();
    for _ in 0..3 {
        assert!(u.add(unit(0), 1));
    }
    // A guest: the same voice five times, unlike his (0.20, inside lane I's measured playback and stranger scores).
    for n in 0..2 * heimdall_voice::USER_RESTART as u64 {
        assert!(!u.misfit(toward(0.20, 7), 10 + n, "Shure MV7i"), "a voice below the floor never starts a set");
    }
    assert!(u.devices.get("Shure MV7i").is_none_or(|d| d.embeddings.is_empty() && d.rejected.is_empty()));
    // His voice on that microphone (0.40 against his first samples) still starts one.
    for n in 0..heimdall_voice::USER_RESTART as u64 - 1 {
        assert!(!u.misfit(toward(0.40, 8), 100 + n, "Shure MV7i"));
    }
    assert!(u.misfit(toward(0.40, 8), 200, "Shure MV7i"));
    // Held two days apart: by the fifth, the first is eight days old and no longer counts; the next one makes five
    // within the week.
    let day = 24 * 3600_000u64;
    let mut u2 = UserVoice::default();
    for _ in 0..3 {
        assert!(u2.add(unit(0), 1));
    }
    for k in 0..heimdall_voice::USER_RESTART as u64 {
        assert!(!u2.misfit(toward(0.40, 9), 1 + 2 * k * day, "Studio Display Microphone"), "held on day {}", 2 * k);
    }
    assert_eq!(u2.devices["Studio Display Microphone"].rejected.len(), heimdall_voice::USER_RESTART - 1);
    assert!(u2.misfit(toward(0.40, 9), 1 + 9 * day, "Studio Display Microphone"), "days 2, 4, 6, 8 and 9");
    // Saved and loaded, the held lines keep their times; a file without them drops its held lines.
    let mut u3 = UserVoice::default();
    for _ in 0..3 {
        assert!(u3.add(unit(0), 1));
    }
    assert!(!u3.misfit(toward(0.40, 9), 5, "Studio Display Microphone"));
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(heimdall_voice::USER_FILE);
    u3.save(&path).unwrap();
    assert_eq!(UserVoice::load(&path).devices["Studio Display Microphone"].rejected_ms, vec![5]);
    let mut old: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    old["devices"]["Studio Display Microphone"].as_object_mut().unwrap().remove("rejected_ms");
    std::fs::write(&path, serde_json::to_vec(&old).unwrap()).unwrap();
    assert!(UserVoice::load(&path).devices["Studio Display Microphone"].rejected.is_empty());
}

/// Drift guards for his print (~08:25): samples that each fit the newest ones cannot walk it away from his
/// first ones, lines with no support besides the voice never start anything, and an old file without anchors takes its
/// oldest samples as them.
#[test]
fn his_print_does_not_walk_and_is_never_widened_on_voice_alone() {
    use heimdall_voice::{Support, UserVoice};
    // Unit vectors on a circle in two dimensions: step `k` is `k` × 0.25 rad from his voice.
    let at = |k: usize| { let a = k as f32 * 0.25; let mut v = vec![0.; speakers::embedding::DIMENSION]; v[0] = a.cos(); v[1] = a.sin(); speakers::embedding::Embedding::new(v).unwrap() };
    let mut u = UserVoice::default();
    for _ in 0..3 {
        assert!(u.add(at(0), 1));
    }
    // Each step is 0.97 from the one before; ten rounds of each step, nearest first.
    let mut joined = 0;
    for k in 1..12 {
        for n in 0..10 {
            if u.add(at(k), 10 * k as u64 + n) {
                joined = joined.max(k);
            }
        }
    }
    assert!(at(joined).cosine(&at(0)) >= heimdall_voice::USER_JOIN, "walked to step {joined}");
    assert!(u.embeddings.iter().all(|e| e.cosine(&at(0)) >= heimdall_voice::USER_JOIN), "every kept sample is near his first");
    assert!(u.devices.is_empty(), "voice alone starts no set");
    // The same far voice with support on a microphone: a set of its own, his print as it was.
    let print = u.embeddings.len();
    // (0.35 against his first samples in a third dimension: above `USER_HELD_FLOOR`, unlike every kept sample.)
    let far = { let mut v = vec![0.; speakers::embedding::DIMENSION]; v[0] = 0.35; v[2] = (1f32 - 0.35 * 0.35).sqrt(); speakers::embedding::Embedding::new(v).unwrap() };
    for n in 0..heimdall_voice::USER_RESTART {
        u.add_on(far.clone(), 500 + n as u64, "MacBook Pro Microphone", Support::Addressed);
    }
    assert_eq!(u.embeddings.len(), print);
    assert_eq!(u.devices["MacBook Pro Microphone"].embeddings.len(), heimdall_voice::USER_RESTART);
    // An older file: anchors from its oldest samples.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(heimdall_voice::USER_FILE);
    let old = serde_json::json!({"embeddings": [at(0), at(1), at(0), at(1)], "recipe": heimdall_voice::USER_RECIPE, "updated_ms": 1, "rejected": [at(9)]});
    std::fs::write(&path, serde_json::to_vec(&old).unwrap()).unwrap();
    let back = UserVoice::load(&path);
    assert_eq!(back.anchors.len(), heimdall_voice::USER_MIN_SAMPLES);
    assert!(back.rejected.is_empty(), "old unsupported misfits are dropped");
}

/// "if I say 'listen to Alex', Alex should be treated like me". Alex's line that mentions
/// Heimdall without addressing it wakes as the user's would in a call (d1 not against it), only while listened to; their
/// "send it" too. Alex's lines never authorise `listen_to`.
#[test]
fn a_listened_guest_counts_like_owner() {
    let (_t, mut a, _mic) = rig(false);
    let mut store = EnrollmentStore::default();
    store.enroll("Alex", speakers::embedding::Embedding::new(vec![1.; speakers::embedding::DIMENSION]).unwrap()).unwrap();
    store.save(&a.data.join("speakers.json")).unwrap();
    a.send_speakers();
    let call = a.sources.iter().find(|(_, s)| crate::context_policy::is_call(s)).map(|(id, _)| id.clone()).unwrap();
    let line = |a: &mut Pipeline, text: &str, at: u64| {
        let words = timed(text, at);
        let end = words.last().unwrap().end_ns;
        a.audio.insert(call.clone(), AudioRing { start: at - S, samples: vec![0.01; ((end + S - at + S) / 62500) as usize].into() });
        a.listen(ListenEvent::SpeechStart { source: call.clone(), host_time_ns: at }).unwrap();
        a.listen(ListenEvent::FinalUtterance { source: call.clone(), text: text.into(), words, start_ns: at, end_ns: end, forced: false, end_of_turn_probability: 0.9 }).unwrap();
        let (session, origin) = (a.discord[&call].session, a.discord[&call].origin);
        a.speaker_names.insert(TrackId { source: call.clone(), session, speaker: 1 }, "Alex".into());
        let (from, to) = ((at - origin) as f64 / 1e9 - 0.5, (end - origin) as f64 / 1e9 + 0.5);
        a.discord.get_mut(&call).unwrap().segments = vec![speakers::diar::SpeakerSegment { start_time: from.max(0.), end_time: to, speaker: 1 }];
        a.hooks.send_choices.push_back(decide::send::Choice::Send);
        a.relabel_discord(&call).unwrap();
    };
    line(&mut a, "I think Heimdall should check the server logs.", 5 * S);
    a.set_listening("Alex", true, "menu").unwrap();
    line(&mut a, "I think Heimdall should check the server logs.", 20 * S);
    line(&mut a, "Write down the server name and send it.", 40 * S);
    line(&mut a, "The server is fine now, I restarted it.", 60 * S);
    a.set_listening("Alex", false, "menu").unwrap();
    line(&mut a, "I think Heimdall should check the server logs.", 80 * S);
    assert_eq!(outcomes(&a), [
        ("held_not_user".to_string(), None),
        ("send".to_string(), Some("name".to_string())),
        ("send".to_string(), Some("send_it".to_string())),
        ("held_in_call".to_string(), None),
        ("held_not_user".to_string(), None),
    ]);
    assert!(a.tools.speech_policy.listen_change(Some("Vim"), capture::host_now_ns()).is_err(), "Alex's words grant nothing");
}

const NARRATOR: &str = "Hey internets, one thing that's commonly done these days is equating democracy with freedom. That the more democracy we have, the more freedom we have, and the more people who are engaged in democracy, the more people voting, the better democracy will function, and thus the freedom will increase even more.";

/// The microphone's row from `from` (an app's own lines around it are skipped).
fn mic_row(a: &Pipeline, from: u64) -> String {
    let rows = a.index.utterances_between(from, from + 30 * S).unwrap();
    rows.iter().map(|r| r["speaker"].as_str().unwrap_or("").to_string()).find(|s| s != "audio").unwrap_or_else(|| panic!("no microphone line at {from}: {rows:?}"))
}
/// A browser on the Mac (not a call, not Heimdall's own output), as a recorded app source. Returns its id.
fn browser(a: &mut Pipeline) -> String {
    let info = capture::SourceInfo::new(
        capture::SourceKind::Process { name: "Ego Lite".into(), bundle_id: Some("com.example.browser".into()), pid: 5151, birth_us: 1, owner: None, output_devices: vec![] },
        capture::NativeFormat { sample_rate: 16000, channels: 1 },
        1,
    );
    let id = info.id.0.clone();
    a.source(info);
    id
}
/// The app says `text` from `at` (host clock), as one final.
fn app_says(a: &mut Pipeline, app: &str, text: &str, at: u64) {
    let words = timed(text, at);
    let end = words.last().unwrap().end_ns;
    a.listen(ListenEvent::SpeechStart { source: app.into(), host_time_ns: at }).unwrap();
    a.listen(ListenEvent::SpeechEnd { source: app.into(), host_time_ns: end }).unwrap();
    a.listen(ListenEvent::FinalUtterance { source: app.into(), text: text.into(), start_ns: at, end_ns: end, words, forced: false, end_of_turn_probability: 0.9 }).unwrap();
}

/// Bug note 2026-11-12T12-58-51: a YouTube narrator playing from the Mac's speakers came back through the Shure as
/// "Mic · the user" and woke the agent (no voice verdict, d1 send 0.92). The browser's own audio had the same words: the
/// line is playback, "Unknown", and never wakes. His own line right after, about the video, is his and wakes.
#[test]
fn playback_from_the_macs_own_output_is_not_owner_and_never_wakes() {
    let (_t, mut a, mic) = rig(false);
    let app = browser(&mut a);
    app_says(&mut a, &app, NARRATOR, 10 * S);
    let heard: Vec<&str> = NARRATOR.split_whitespace().collect();
    // As live: no verdict from the voice check, the decider says send.
    turn(&mut a, &mic, 13 * S, &heard[10..34].join(" "), Detect::Final, 0, Some(0.92), S / 2, |_| {});
    assert_eq!(mic_row(&a, 13 * S), "Unknown");
    assert!(outcomes(&a).iter().all(|(o, _)| o != "send"), "{:?}", outcomes(&a));
    assert!(records(&a, "agent_wake").is_empty());
    assert_eq!(records(&a, "mic_playback").len(), 1);
    a.hooks.mic_voice.push_back((Some(0.83), Some(0.27)));
    turn(&mut a, &mic, 60 * S, "Heimdall, that wasn't me by the way, that was the YouTube thing.", Detect::Final, 0, Some(0.95), S / 2, |_| {});
    assert_eq!(mic_row(&a, 60 * S), "User");
    assert_eq!(outcomes(&a).last().unwrap().0, "send");
}

/// While an app talks, a quiet line clearly unlike his print gets no benefit of the doubt (measured: playback through
/// the speakers scored 0.15-0.29 and was quiet); with no app talking the same line stays his, as before.
#[test]
fn a_quiet_voice_unlike_his_while_an_app_talks_is_not_his() {
    let quiet = |a: &mut Pipeline, mic: &str| {
        let ring = a.audio.get(mic).unwrap().start;
        a.audio.insert(mic.into(), AudioRing { start: ring, samples: vec![0.004; 20 * 16_000].into() });
    };
    let (_t, mut a, mic) = rig(false);
    let app = browser(&mut a);
    app_says(&mut a, &app, "and the space shuttle will carry the crew to the station tonight", 10 * S);
    a.hooks.mic_voice.push_back((Some(0.2), Some(0.1)));
    turn(&mut a, &mic, 11 * S, "what your watch was", Detect::Final, 0, Some(0.9), S / 2, |a| quiet(a, &mic));
    assert_eq!(mic_row(&a, 11 * S), "Unknown");
    assert_eq!(records(&a, "mic_voice")[0]["reason"], "app_audible");
    let (_t, mut a, mic) = rig(false);
    a.hooks.mic_voice.push_back((Some(0.2), Some(0.1)));
    turn(&mut a, &mic, 11 * S, "what your watch was", Detect::Final, 0, Some(0.9), S / 2, |a| quiet(a, &mic));
    assert_eq!(mic_row(&a, 11 * S), "User", "no app talking: a short quiet line stays his");
}

#[test]
fn app_audible_verdicts() {
    use heimdall_voice::{app_audible_verdict as v, MicVoice::*, Scores};
    let s = |u: f32| Scores { user: Some(u), heimdall: None };
    let long = heimdall_voice::MIC_RELIABLE_NS;
    assert_eq!(v(Unverified, Some(&s(0.2)), true, 0, 0.004), Unknown, "quiet and unlike him while an app talks");
    assert_eq!(v(Unverified, Some(&s(0.2)), true, long, 0.05), Unknown, "long enough to tell");
    assert_eq!(v(Unverified, Some(&s(0.37)), true, 800_000_000, 0.07), Unverified, "a short line at speaking level keeps the benefit of the doubt");
    assert_eq!(v(Unverified, Some(&s(0.45)), true, long, 0.004), Unverified, "unclear, not unlike him");
    assert_eq!(v(Unverified, Some(&s(0.2)), false, long, 0.004), Unverified, "no app talking: as before");
    assert_eq!(v(Unverified, None, true, long, 0.004), Unverified, "no score: no call");
    assert_eq!(v(Owner, Some(&s(0.8)), true, long, 0.05), Owner);
}

/// Review 12 Nov: his own voice sent back by a friend's open speakers in a call repeats his words in the app's audio.
/// A line his print verified as his stays his and wakes.
#[test]
fn his_verified_voice_echoed_by_an_app_stays_his() {
    let (_t, mut a, mic) = rig(false);
    let app = browser(&mut a);
    let line = "Heimdall, open the comparison page for the build please";
    app_says(&mut a, &app, line, 10 * S + 300_000_000);
    a.hooks.mic_voice.push_back((Some(0.82), Some(0.12)));
    turn(&mut a, &mic, 10 * S, line, Detect::Final, 0, Some(0.97), S / 2, |_| {});
    assert_eq!(mic_row(&a, 10 * S), "User");
    assert!(records(&a, "mic_playback").is_empty());
}
