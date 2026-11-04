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
    assert_eq!(outcomes(&a), [("held_not_owner".to_string(), None)], "the name from another voice is context only");
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
    turn(&mut a, &mic, 10 * S, "Heimdall, please listen to Kanan from now on.", Detect::Final, 0, Some(0.99), 2 * S, |_| {});
    assert_eq!(outcomes(&a), [("held_not_owner".to_string(), None)]);
    assert!(a.tools.speech_policy.listen_change(Some("Kanan"), capture::host_now_ns()).is_err(), "an unknown voice grants nothing");
    assert!(records(&a, "agent_wake").is_empty());
    a.hooks.mic_voice.push_back((Some(0.8), Some(0.1)));
    turn(&mut a, &mic, 30 * S, "Heimdall, listen to Kanan.", Detect::Final, 0, Some(0.), 2 * S, |_| {});
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
    assert_eq!(outcomes(&a).last().unwrap(), &("held_not_owner".to_string(), None), "an unsure voice over the echo never wakes");
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

/// His print can start again when his microphone changes: five held lines that agree with each other replace it;
/// held lines of different voices never do, and a held line never joins his print.
#[test]
fn misfits_only_restart_his_print() {
    use heimdall_voice::UserVoice;
    let unit = |i: usize| { let mut v = vec![0.; speakers::embedding::DIMENSION]; v[i] = 1.; speakers::embedding::Embedding::new(v).unwrap() };
    let mut u = UserVoice::default();
    for _ in 0..3 {
        assert!(u.add(unit(0), 1));
    }
    assert!(!u.misfit(unit(0), 2) && u.embeddings.len() == 3, "a held line never joins");
    for i in 1..5 {
        assert!(!u.misfit(unit(i), 3), "different voices never restart it");
    }
    for _ in 0..4 {
        assert!(!u.misfit(unit(9), 4));
    }
    assert!(u.misfit(unit(9), 5), "the fifth that agrees starts it again");
    assert_eq!(u.embeddings.len(), 5);
    assert!(u.similarity(&unit(9)).unwrap() > 0.99);
    assert!(!UserVoice::default().misfit(unit(1), 1), "no print yet: nothing to restart");
}

/// "if I say 'listen to Kanan', Kanan should be treated like me". Kanan's line that mentions
/// Heimdall without addressing it wakes as the user's would in a call (d1 not against it), only while listened to; their
/// "send it" too. Kanan's lines never authorise `listen_to`.
#[test]
fn a_listened_guest_counts_like_owner() {
    let (_t, mut a, _mic) = rig(false);
    let mut store = EnrollmentStore::default();
    store.enroll("Kanan", speakers::embedding::Embedding::new(vec![1.; speakers::embedding::DIMENSION]).unwrap()).unwrap();
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
        a.speaker_names.insert(TrackId { source: call.clone(), session, speaker: 1 }, "Kanan".into());
        let (from, to) = ((at - origin) as f64 / 1e9 - 0.5, (end - origin) as f64 / 1e9 + 0.5);
        a.discord.get_mut(&call).unwrap().segments = vec![speakers::diar::SpeakerSegment { start_time: from.max(0.), end_time: to, speaker: 1 }];
        a.hooks.send_choices.push_back(decide::send::Choice::Send);
        a.relabel_discord(&call).unwrap();
    };
    line(&mut a, "I think Heimdall should check the server logs.", 5 * S);
    a.set_listening("Kanan", true, "menu").unwrap();
    line(&mut a, "I think Heimdall should check the server logs.", 20 * S);
    line(&mut a, "Write down the server name and send it.", 40 * S);
    line(&mut a, "The server is fine now, I restarted it.", 60 * S);
    a.set_listening("Kanan", false, "menu").unwrap();
    line(&mut a, "I think Heimdall should check the server logs.", 80 * S);
    assert_eq!(outcomes(&a), [
        ("held_not_owner".to_string(), None),
        ("send".to_string(), Some("name".to_string())),
        ("send".to_string(), Some("send_it".to_string())),
        ("held_in_call".to_string(), None),
        ("held_not_owner".to_string(), None),
    ]);
    assert!(a.tools.speech_policy.listen_change(Some("Vim"), capture::host_now_ns()).is_err(), "Kanan's words grant nothing");
}
