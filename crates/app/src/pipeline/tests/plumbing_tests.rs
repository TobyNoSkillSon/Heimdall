//! Spoken-command plumbing, end to end through the real `Pipeline`: detection (partial transcript, 15-s
//! window decode, final) → candidate and held screen → stubbed d1 → execution → evidence → transcript
//! note → agent delivery to a fake harness (Agent on) or the local dry log (Agent off) → spoken replies
//! and the agent's MCP tools. Models are stubbed and capture is fake; nothing needs a permission.
use super::*;
use agent::mcp::ToolHost;
use std::{sync::atomic::AtomicUsize, time::Duration};

/// Test-only stand-ins for d1 answers the pipeline would otherwise load models for, and a record of
/// every UI update the pipeline sends.
#[derive(Default)]
pub(crate) struct Hooks {
    /// d1's SendNow probability for the next finals that reach the decision.
    pub(crate) send: VecDeque<f32>,
    pub(crate) send_choices: VecDeque<decide::send::Choice>,
    pub(crate) ui: Mutex<Vec<ui::UiUpdate>>,
    /// Heimdall's voice comes out of speakers (pipeline/output_route.rs; night pass 1022).
    pub(crate) speakers: bool,
    /// d1's exact (send, wait, not_for_heimdall) for the next finals; the choice is their maximum (night pass 1022).
    pub(crate) send_probs: VecDeque<(f32, f32, f32)>,
    /// The host clock the instant acknowledgement reads (pipeline/ack.rs); None: the real clock.
    pub(crate) ack_now: Option<u64>,
    /// The ack's estimate of the next reply, ms after his last word (pipeline/ack.rs); None: no estimate.
    pub(crate) expected_reply_ms: Option<u64>,
    /// Decider v3 (pipeline/ack.rs `AckPlan`): Some(true) acts as if v3 is loaded; None: the shipped model.
    pub(crate) decider_v3: Option<bool>,
    /// v3 sends for the next finals: the line d1 picked with its send (None: plain send).
    pub(crate) send_v3: VecDeque<Option<String>>,
    /// The user starts talking again between a wake and d1's verdict on its words (pipeline/ack.rs `late_ack`).
    pub(crate) talk_before_verdict: bool,
    /// The own-voice check's best cosine against Heimdall's voices for the next barge-in windows; None fails the
    /// check (pipeline/heimdall_voice.rs).
    pub(crate) voice: VecDeque<Option<f32>>,
    /// The barge-in check's best cosine against the user's voice for the next windows, beside `voice` (Some(None) fails
    /// the check; Some(Some(c)) a score; an item of None in the outer queue's place: no profile yet).
    pub(crate) user_voice: VecDeque<Option<f32>>,
    /// The replay tests' real encoder against the user's profile (used when `user_voice` is empty).
    #[allow(clippy::type_complexity)]
    pub(crate) user_fn: Option<Box<dyn FnMut(&[f32]) -> Option<f32> + Send>>,
    /// Embeddings the next samples of the user's voice turn into (pipeline/heimdall_voice.rs `user_voice_heard`).
    pub(crate) user_samples: VecDeque<speakers::embedding::Embedding>,
    /// The speaking voice has a profile even with no cosine queued (the check is ready).
    pub(crate) voice_enrolled: bool,
    /// The replay tests' real encoder: a window's best cosine against Heimdall's voices (used when `voice` is empty).
    #[allow(clippy::type_complexity)]
    pub(crate) voice_fn: Option<Box<dyn FnMut(&[f32]) -> Option<f32> + Send>>,
    /// Whole microphone utterances' (user, Heimdall) cosines for the next verdicts (mic_voice.rs); empty: unverified.
    pub(crate) mic_voice: VecDeque<(Option<f32>, Option<f32>)>,
    /// The next this many checks are asked but never answered (the encoder's thread gone).
    pub(crate) lose_voice_answers: usize,
    /// Checks answered, waiting for the live loop (`barge_voice`).
    pub(crate) voice_answers: VecDeque<(u64, std::result::Result<heimdall_voice::Scores, String>)>,
    /// The host clock Vella's link reads (pipeline/vella.rs); None: the real clock.
    pub(crate) vella_now: Option<u64>,
    /// Decider V5's P(yes) for the next suggest questions (pipeline/suggest.rs `suggest_ask`).
    pub(crate) suggest: VecDeque<f32>,
    /// A card could follow (an agent running, background calls allowed) without one.
    pub(crate) suggest_allowed: bool,
    /// The profile's real user voice decides whether d1 may wake (`Pipeline::user_voice_known`); false: assumed known.
    pub(crate) no_user_voice: bool,
    /// Setup waits for its last step's phrase (`Pipeline::waits_for_phrase`).
    pub(crate) phrase_waits: bool,
}
const S: u64 = 1_000_000_000;
use decide::{CommandIntent as I, CommandSelection as C};

pub(crate) fn mock_harness() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../agent/tests/fixtures/mock-adapter.py")
}
/// A replay pipeline with the user's primary mic, a Discord app source, and two displays showing
/// second `t` from t = 0 to 60 s (pixel: [t, display, 7]). Agent on runs the fake Pi harness.
pub(crate) fn rig(agent_on: bool) -> (tempfile::TempDir, Pipeline, String) {
    let t = tempfile::tempdir().unwrap();
    let settings = Settings { dry_run: !agent_on, ..Default::default() };
    let mut a = Pipeline::new(t.path().join("data"), t.path().join("missing-models"), settings, None, true).unwrap();
    if agent_on {
        crate::bridge::test_harness(Some(mock_harness()));
    }
    let mic = crate::fake_source("mic", 0, 1);
    let id = mic.id.0.clone();
    a.source(mic);
    a.source(capture::SourceInfo::new(
        capture::SourceKind::Process { name: "Discord".into(), bundle_id: Some("com.hnc.Discord".into()), pid: 4242, birth_us: 1, owner: None, output_devices: vec![] },
        capture::NativeFormat { sample_rate: 16000, channels: 1 },
        1,
    ));
    let mut buffer = a.screen_buffer.lock().unwrap();
    buffer.reconfigure(screen::ScreenConfig { fps: 1, ..Default::default() }, 0).unwrap();
    for t in 0..=60u64 {
        for d in [1u32, 2] {
            let display = screen::DisplayInfo { id: d, x: 0., y: 0., width: 2, height: 1 };
            buffer.push(screen::Frame::new(display, t * S, 2, 1, [t as u8, d as u8, 7].repeat(2)).unwrap()).unwrap();
        }
    }
    drop(buffer);
    (t, a, id)
}
pub(crate) fn timed(text: &str, start: u64) -> Vec<Word> {
    text.split_whitespace()
        .enumerate()
        .map(|(i, w)| Word { text: w.into(), start_ns: start + i as u64 * 300_000_000, end_ns: start + i as u64 * 300_000_000 + 250_000_000, confidence: 1. })
        .collect()
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Detect {
    /// Only Smart Turn's final carries the phrase.
    Final,
    /// A 15-s window decode emits every word but the last as a segment before the final.
    Window,
    /// A partial transcript ending with the phrase arrives before the final (screen phrases only: partials match
    /// nothing else).
    Partial,
}
/// One turn on the user's mic starting at `at`; the command phrase is its first `phrase_words` words.
/// `send` is d1's SendNow answer when the final reaches the decision. The capture clock stands `quiet` past the last word; `during` runs after
/// his speech starts.
#[allow(clippy::too_many_arguments)]
/// The island notice receipts' bodies, oldest first. The agent supervisor retires a shown receipt after any of its
/// loop's passes (`agent::Actions::retire`): the payload moves to the session archive and the row keeps only its
/// digest. A retired body is read back from the archive, so a check never depends on polling before that pass.
pub(crate) fn island_notices(a: &Pipeline) -> Vec<String> {
    let db = rusqlite::Connection::open(a.data.join("agent/mcp-actions.sqlite")).unwrap();
    let mut q = db.prepare("SELECT r.body,p.reference FROM receipts r LEFT JOIN retired_payloads p ON p.kind='receipt' AND p.id=r.id WHERE r.kind='notice' ORDER BY r.rowid").unwrap();
    let rows: Vec<(String, Option<String>)> = q.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().map(Result::unwrap).collect();
    rows.into_iter()
        .map(|(body, reference)| match reference {
            Some(r) => {
                let payload: serde_json::Value = serde_json::from_slice(&heimdall_archive::store::read_log(&a.session, &r).unwrap()).unwrap();
                payload["body"].as_str().unwrap().to_owned()
            }
            None => body,
        })
        .collect()
}
pub(crate) fn turn(a: &mut Pipeline, mic: &str, at: u64, text: &str, detect: Detect, phrase_words: usize, send: Option<f32>, quiet: u64, during: impl FnOnce(&mut Pipeline)) {
    let words = timed(text, at);
    let end = words.last().unwrap().end_ns;
    let ring = at.saturating_sub(5 * S);
    a.audio.insert(mic.into(), AudioRing { start: ring, samples: vec![0.01; ((end + quiet - ring) / 62500) as usize].into() });
    a.listen(ListenEvent::SpeechStart { source: mic.into(), host_time_ns: at }).unwrap();
    during(a);
    let mut tail = words.clone();
    match detect {
        Detect::Final => {}
        Detect::Window => {
            let segment: Vec<_> = tail.drain(..words.len() - 1).collect();
            a.listen(ListenEvent::TranscriptSegment { source: mic.into(), text: listen::Transcript::from_words(segment.clone()).text, start_ns: segment[0].start_ns, end_ns: segment.last().unwrap().end_ns, words: segment }).unwrap();
        }
        Detect::Partial => {
            let heard = words[..phrase_words].to_vec();
            a.listen(ListenEvent::PartialTranscript { source: mic.into(), text: listen::Transcript::from_words(heard.clone()).text, words: heard, stable_words: 0 }).unwrap();
        }
    }
    a.listen(ListenEvent::SpeechEnd { source: mic.into(), host_time_ns: end }).unwrap();
    a.hooks.send.extend(send);
    a.listen(ListenEvent::FinalUtterance { source: mic.into(), text: listen::Transcript::from_words(tail.clone()).text, start_ns: tail[0].start_ns, end_ns: end, words: tail, forced: false, end_of_turn_probability: 0.9 }).unwrap();
}
fn records(a: &Pipeline) -> Vec<heimdall_archive::Record> {
    a.index.evidence.records().unwrap()
}
fn count(records: &[heimdall_archive::Record], kind: &str) -> usize {
    records.iter().filter(|r| r.kind == kind).count()
}
/// Polls the archive (the agent thread records harness traffic as it arrives).
fn wait_for(a: &Pipeline, what: &str, done: impl Fn(&[heimdall_archive::Record]) -> bool) -> Vec<heimdall_archive::Record> {
    let began = Instant::now();
    loop {
        let r = records(a);
        if done(&r) {
            return r;
        }
        assert!(began.elapsed() < Duration::from_secs(10), "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}
fn completions(r: &[heimdall_archive::Record]) -> usize {
    r.iter().filter(|r| r.kind == "agent_reply" && r.payload["event"]["kind"] == "complete" && r.payload["event"]["error"] == false).count()
}
/// The notes on the last final transcript line the UI received.
fn final_line_notes(a: &Pipeline) -> Vec<String> {
    let log = a.hooks.ui.lock().unwrap();
    let line = log.iter().rev().find_map(|u| match u { ui::UiUpdate::Transcript(l) if l.is_final => Some(l.clone()), _ => None }).expect("no final transcript line");
    line.segments.iter().filter_map(|s| match s { ui::Segment::Note(n) => Some(n.clone()), _ => None }).collect()
}
/// (pixel second, display) of a PNG's first pixel.
fn png_tag(bytes: &[u8]) -> (u8, u8) {
    let mut reader = png::Decoder::new(std::io::Cursor::new(bytes)).read_info().unwrap();
    let mut pixels = vec![0; reader.output_buffer_size().unwrap()];
    reader.next_frame(&mut pixels).unwrap();
    (pixels[0], pixels[1])
}

struct Case {
    text: &'static str,
    phrase_words: usize,
    command: C,
}
const CASES: &[Case] = &[
    Case { text: "look at this the chart is wrong", phrase_words: 3, command: C::LookAtThis },
    Case { text: "take a screenshot of the error", phrase_words: 3, command: C::TakeScreenshot },
];
impl Case {
    fn detections(&self) -> Vec<Detect> {
        let mut d = vec![Detect::Final, Detect::Window];
        if self.screen() {
            d.push(Detect::Partial);
        }
        d
    }
    fn screen(&self) -> bool {
        matches!(self.command, C::LookAtThis | C::TakeScreenshot)
    }
    fn phrase(&self) -> String {
        self.text.split_whitespace().take(self.phrase_words).collect::<Vec<_>>().join(" ")
    }
}

/// "from the plumbing perspective ... whatever we can test from the code perspective
/// everything works". Every fixed command × every detection path, Agent off: one decision, one action,
/// its evidence, its transcript note, and nothing left held. One test per path so they run in parallel.
#[test]
fn every_fixed_command_runs_once_from_the_final() {
    assert_eq!(every_fixed_command_runs_once_via(Detect::Final), 2);
}
#[test]
fn every_fixed_command_runs_once_from_a_window_decode() {
    assert_eq!(every_fixed_command_runs_once_via(Detect::Window), 2);
}
#[test]
fn every_screen_command_runs_once_from_a_partial() {
    assert_eq!(every_fixed_command_runs_once_via(Detect::Partial), 2);
}
fn every_fixed_command_runs_once_via(path: Detect) -> usize {
    let mut cases = 0;
    for case in CASES {
        for detect in case.detections().into_iter().filter(|d| *d == path) {
            let label = format!("{:?} via {detect:?}", case.text);
            let (_t, mut a, mic) = rig(false);
            a.command_test_answers.push_back((I::Yes, case.command));
            turn(&mut a, &mic, 4 * S, case.text, detect, case.phrase_words, Some(0.), S / 2, |_| {});
            let r = records(&a);
            // Detection → candidate: one chain, every detection merged into it.
            let chains: Vec<_> = r.iter().filter(|r| r.kind == "command_decision" && r.payload["phase"] == "chain").collect();
            assert_eq!(chains.len(), 1, "{label}");
            let origins: Vec<_> = chains[0].payload["proposals"].as_array().unwrap().iter().map(|p| p["origin"].as_str().unwrap()).collect();
            let expected: &[&str] = match detect {
                Detect::Final => &["parakeet_final"],
                // The window's words decide it at detection; the final's detection then merges.
                Detect::Window if matches!(case.command,C::LookAtThis|C::TakeScreenshot) => &["parakeet_partial","parakeet_window"],
                Detect::Window => &["parakeet_window"],
                Detect::Partial => &["parakeet_partial", "parakeet_final"],
            };
            assert_eq!(origins, expected, "{label}");
            assert_eq!(chains[0].payload["candidate"]["phrase_guess"], case.phrase(), "{label}");
            assert!(chains[0].payload["audio"].is_null() && !chains[0].payload["decider_text"]["words"].as_array().unwrap().is_empty(), "{label}: decided from text only");
            // Decision → execution: authorised once, executed once, without error.
            assert_eq!(chains[0].payload["approved"], case.command.key(), "{label}");
            assert!(chains[0].payload["error"].is_null(), "{label}: {}", chains[0].payload["error"]);
            let actions: Vec<_> = r.iter().filter(|r| r.kind == "command_action").collect();
            assert_eq!(actions.len(), 1, "{label}");
            assert!(actions[0].payload["error"].is_null() && actions[0].payload["command"] == case.command.key(), "{label}: {}", actions[0].payload);
            // Execution → evidence and the transcript note the UI shows.
            let notes = final_line_notes(&a);
            let text = a.index.utterances_between(0, 60 * S).unwrap().last().unwrap()["text"].as_str().unwrap().to_string();
            match case.command {
                C::LookAtThis | C::TakeScreenshot => {
                    let shots: Vec<_> = r.iter().filter(|r| r.kind == "screenshot").collect();
                    assert_eq!(shots.len(), 2, "{label}: one ring frame per display");
                    let mut tags: Vec<_> = shots.iter().map(|s| png_tag(&a.index.evidence.image_bytes(s.payload["image_id"].as_str().unwrap()).unwrap().1)).collect();
                    tags.sort();
                    assert_eq!(tags, [(4, 1), (4, 2)], "{label}: the screen when the phrase began");
                    assert_eq!(a.attachment_batch.len(), 2, "{label}: waits for the next wake");
                    assert!(std::fs::read_dir(crate::archive::run_dir(&a.data, &a.session).join("shots")).unwrap().next().is_none(), "{label}: temporary PNGs left");
                    assert!(notes.len() == 1 && notes[0].starts_with("screenshot ") && notes[0].ends_with(" attached"), "{label}: {notes:?}");
                    assert!(text.contains(&format!("[{}]", notes[0])), "{label}: {text}");
                }
                _ => unreachable!(),
            }
            // Nothing stays held or pending; no harness ran with the Agent off.
            assert!(a.screen_holds.is_empty() && a.command_candidates.is_empty(), "{label}: hold or candidate left");
            assert_eq!(a.errors, 0, "{label}: {:?}", a.component_status);
            assert!(!r.iter().any(|r| r.kind.starts_with("agent_transport")), "{label}");
            cases += 1;
        }
    }
    cases
}

/// The user's live test, 13 Oct (session-1791882699718-6cdddd31), reworded to Decider V5's two commands. A phrase a
/// partial transcript carries shows its tentative cue at once and is decided at the turn's final; a rejection clears
/// it. A Polish screen phrase is no longer a command (V5 learned none; Polish dropped, 28 Oct 20:53).
#[test]
fn live_session_commands_show_tentative_detection_then_approved_result_or_rejection() {
    let (_t, mut a, mic) = rig(false);
    // A partial's detection time: the start of the phrase's last word (word `i` of a turn at `at`).
    let word = |at: u64, i: u64| at + i * 300_000_000;
    a.command_test_answers.push_back((I::Yes, C::LookAtThis));
    turn(&mut a, &mic, 4 * S, "Okay, look at this.", Detect::Partial, 4, Some(0.), S, |_| {});
    turn(&mut a, &mic, 12 * S, "Nie. Okej, spójrz na ekran.", Detect::Final, 3, Some(0.), S / 2, |_| {});
    a.command_test_answers.push_back((I::FalseAlarm, C::Unsure));
    turn(&mut a, &mic, 20 * S, "So let's take a screenshot.", Detect::Partial, 5, Some(0.), S, |_| {});
    let r = records(&a);
    let chains: Vec<_> = r.iter().filter(|r| r.payload["phase"] == "chain").collect();
    let seen: Vec<_> = chains.iter().map(|c| (c.payload["candidate"]["phrase_guess"].as_str().unwrap(), c.payload["stage"].as_str().unwrap(), c.payload["approved"].as_str())).collect();
    assert_eq!(seen, [("look at this", "final", Some("look_at_this")), ("take a screenshot", "final", None)]);
    let origins = |c: &heimdall_archive::Record| c.payload["proposals"].as_array().unwrap().iter().map(|p| p["origin"].as_str().unwrap().to_string()).collect::<Vec<_>>();
    assert_eq!(origins(chains[0]), ["parakeet_partial", "parakeet_final"]);
    assert_eq!(count(&r, "command_action"), 1);
    // Each chain asked exactly V5's trained question for its command.
    let asked: Vec<_> = chains.iter().map(|c| c.payload["steps"][0]["instructions"].to_string()).collect();
    assert!(asked[0].contains("Did User, right now, ask Heimdall to look at his screen?"), "{}", asked[0]);
    assert!(asked[1].contains("Did User, right now, ask Heimdall to take a screenshot?"), "{}", asked[1]);
    let cues: Vec<_> = a.hooks.ui.lock().unwrap().iter().filter_map(|u| match u { ui::UiUpdate::Command(c) => Some(c.clone()), _ => None }).collect();
    let look = ui::CommandKind::LookAtThis;
    assert_eq!(cues, [
        ui::CommandCue::Heard { id: word(4 * S, 3), kind: look },
        ui::CommandCue::Done { id: Some(word(4 * S, 3)), kind: look, label: "Screenshot".into() },
        ui::CommandCue::Heard { id: word(20 * S, 4), kind: ui::CommandKind::Screenshot },
        ui::CommandCue::Dropped { id: word(20 * S, 4) },
    ]);
    assert!(a.screen_holds.is_empty() && a.command_candidates.is_empty());
}

/// Every screen-phrase path (partial, window decode, final) attaches the frame shown when the phrase's first word
/// began: not the turn's start and not the moment of detection or decision. Frames are one per second and encode
/// their second in the first pixel byte.
#[test]
fn every_screen_path_attaches_the_frame_at_the_phrase_first_word() {
    // "look" is word 11 of a turn at 4 s: it begins at 7.3 s, so the frame is second 7; the turn began at second 4
    // and its last word ends after 9 s.
    let text = "the build broke again on main and nobody noticed it today. look at this chart it is broken";
    let look = 4 * S + 11 * 300_000_000;
    for detect in [Detect::Partial, Detect::Window, Detect::Final] {
        let (_t, mut a, mic) = rig(false);
        a.command_test_answers.push_back((I::Yes, C::LookAtThis));
        turn(&mut a, &mic, 4 * S, text, detect, 14, Some(0.), S / 2, |_| {});
        let r = records(&a);
        let chain = r.iter().find(|r| r.payload["phase"] == "chain").unwrap_or_else(|| panic!("{detect:?}: no decision"));
        assert_eq!(chain.payload["approved"], "look_at_this", "{detect:?}");
        assert_eq!(chain.payload["phrase_start_ns"], look, "{detect:?}");
        let first = chain.payload["proposals"][0]["origin"].as_str().unwrap().to_string();
        assert_eq!(first, match detect { Detect::Final => "parakeet_final", _ => "parakeet_partial" }, "{detect:?}");
        let mut tags: Vec<_> = r.iter().filter(|r| r.kind == "screenshot").map(|s| png_tag(&a.index.evidence.image_bytes(s.payload["image_id"].as_str().unwrap()).unwrap().1)).collect();
        tags.sort();
        assert_eq!(tags, [(7, 1), (7, 2)], "{detect:?}: the screen when \"look\" began");
        assert!(a.screen_holds.is_empty() && a.command_candidates.is_empty(), "{detect:?}");
    }
}

/// d1 answers other than "yes" + a listed command: no action, no note, nothing written, holds released.
#[test]
fn every_escape_answer_leaves_no_action_for_every_command() {
    let escapes = [(I::FalseAlarm, C::Unsure), (I::Unsure, C::Unsure)];
    for case in CASES {
        for answer in escapes {
            let label = format!("{:?} answered {answer:?}", case.text);
            let (_t, mut a, mic) = rig(false);
            a.command_test_answers.push_back(answer);
            let detect = if case.screen() { Detect::Partial } else { Detect::Window };
            turn(&mut a, &mic, 4 * S, case.text, detect, case.phrase_words, Some(0.), S / 2, |_| {});
            let r = records(&a);
            let chain = r.iter().find(|r| r.payload["phase"] == "chain").unwrap();
            assert!(chain.payload["approved"].is_null() && chain.payload["error"].is_null(), "{label}: {}", chain.payload);
            for kind in ["command_action", "screen_command", "screenshot", "bug_note"] {
                assert_eq!(count(&r, kind), 0, "{label}: {kind}");
            }
            assert!(final_line_notes(&a).is_empty(), "{label}");
            assert!(a.bugs.listed.is_empty(), "{label}");
            assert!(a.attachment_batch.is_empty() && a.screen_holds.is_empty() && a.command_candidates.is_empty(), "{label}");
            // The words still reach the agent's transcript as ordinary speech.
            assert!(a.pending_text.iter().any(|t| t.contains(case.text)), "{label}");
        }
    }
}

/// The wake path, Agent on (fake Pi harness) and Agent off (local dry log): the turns arrive labelled in
/// spoken order, the screenshot rides on the wake after it as images, once, and is never sent again.
#[test]
fn wake_carries_turns_in_order_and_the_screenshot_once() {
    for agent_on in [true, false] {
        let label = if agent_on { "Agent on" } else { "Agent off" };
        let (_t, mut a, mic) = rig(agent_on);
        turn(&mut a, &mic, 2 * S, "the build failed on main", Detect::Final, 0, Some(0.), S / 2, |_| {});
        a.command_test_answers.push_back((I::Yes, C::LookAtThis));
        turn(&mut a, &mic, 6 * S, "look at this the chart is wrong", Detect::Partial, 3, Some(1.), 3 * S / 2, |_| {});
        assert!(a.attachment_batch.is_empty() && a.pending_text.is_empty(), "{label}: the wake took the batch");
        a.test_agent().barrier().unwrap();
        wait_for(&a, "first completion", |r| completions(r) >= 1);
        turn(&mut a, &mic, 12 * S, "thanks that is all", Detect::Final, 0, Some(1.), 3 * S / 2, |_| {});
        a.test_agent().barrier().unwrap();
        let r = wait_for(&a, "second completion", |r| completions(r) >= 2);
        // What the agent bridge sent (Agent off: what it would have sent), archived before transport.
        let wakes: Vec<_> = r.iter().filter(|r| r.kind == "agent_send" && r.payload["query"] == true).collect();
        assert_eq!(wakes.len(), 2, "{label}");
        let content = |w: &heimdall_archive::Record| w.payload["payload"]["message"]["content"].as_array().unwrap().clone();
        let first = content(wakes[0]);
        let wake_text = first[0]["text"].as_str().unwrap();
        let delivered = r.iter().filter(|row|row.kind=="agent_send").take_while(|row|row.payload["message_id"]!=wakes[0].payload["message_id"]).filter_map(|row|row.payload["payload"]["message"]["content"][0]["text"].as_str()).collect::<Vec<_>>().join("\n");
        let text = if agent_on {wake_text}else{&delivered};
        let all = r.iter().filter(|row|row.kind=="agent_send").filter_map(|row|row.payload["payload"]["message"]["content"][0]["text"].as_str()).collect::<Vec<_>>().join("\n");
        for words in ["the build failed on main","look at this the chart is wrong","thanks that is all"] {assert_eq!(all.matches(words).count(),1,"{label}: duplicated transcript");}
        if !agent_on {assert!(wake_text.contains("observations already in context"));}
        let (build, look) = (text.find("the build failed on main").unwrap(), text.find("look at this the chart is wrong [screenshot").unwrap());
        assert!(build < look, "{label}: out of order:\n{text}");
        assert!(text.contains(" Mic · User]") && !text.contains("[Mic · User]"), "{label}: unlabelled or without its time:\n{text}");
        let images: Vec<_> = first[1..].iter().map(|b| png_tag(&a.index.evidence.image_bytes(b["source"]["archive_image_id"].as_str().unwrap()).unwrap().1)).collect();
        assert_eq!(images, [(6, 1), (6, 2)], "{label}: the screenshot follows the text");
        let second = content(wakes[1]);
        assert_eq!(second.len(), 1, "{label}: images sent twice");
        let text = second[0]["text"].as_str().unwrap();
        assert!(!text.contains("the build failed"), "{label}: resent text:\n{text}");
        if agent_on {assert!(text.contains("thanks that is all"));}else{assert!(text.contains("observations already in context"));}
        if agent_on {
            // The harness itself received exactly these two prompts, the first with both images.
            let got: Vec<_> = r.iter().filter(|r| r.kind == "agent_reply" && r.payload["event"]["type"] == "mock_pi").map(|r| r.payload["event"]["request"].clone()).collect();
            assert_eq!(got.len(), 2, "{label}");
            assert_eq!(got[0]["type"], "prompt");
            assert!(got[0]["message"].as_str().unwrap().contains("look at this the chart is wrong [screenshot"));
            use base64::Engine;
            let sent: Vec<_> = got[0]["images"].as_array().unwrap().iter().map(|i| {
                assert_eq!(i["mimeType"], "image/png");
                png_tag(&base64::engine::general_purpose::STANDARD.decode(i["data"].as_str().unwrap()).unwrap())
            }).collect();
            assert_eq!(sent, [(6, 1), (6, 2)]);
            assert!(got[1]["images"].as_array().unwrap().is_empty());
            // Control requests are not wakes: after each settled reply the supervisor asks Pi for the history size
            // (`get_session_stats`, idle compaction), which may land before or after this count.
            let wake_writes = r.iter().filter(|row| row.kind == "agent_transport_sent" && !row.payload["wire"]["id"].as_str().is_some_and(|id| id.starts_with("control-"))).count();
            assert_eq!(wake_writes, 2, "{label}: one transport write per wake");
        } else {
            assert!(!r.iter().any(|r| r.kind.starts_with("agent_transport")), "{label}: transport used");
            assert!(wakes.iter().all(|w| w.source == "agent:dry"));
            assert!(!a.data.join("pi-spool.sqlite").exists() && !a.data.join("pi-session.jsonl").exists(), "{label}: harness state created");
            let log = std::fs::read_to_string(crate::archive::run_dir(&a.data, &a.session).join("dry-run.jsonl")).unwrap();
            assert_eq!(log.lines().filter(|l| l.contains("\"shouldQuery\":true")).count(), 2);
            assert!(!log.contains("iVBOR"), "pixels written to the dry log");
        }
        drop(a);
    }
}

/// Synthesis that holds a text starting with "hold" until it is cancelled (at most 5 s; "holdlong", for the real-model
/// replays, 10 minutes), and speaks anything else.
struct Synth {
    started: crossbeam_channel::Sender<String>,
    cancelled: crossbeam_channel::Sender<String>,
}
impl speak::Synthesizer for Synth {
    fn synthesize(&mut self, text: &str, _: &str, cancel: &dyn Fn() -> bool) -> Result<speak::Audio> {
        self.started.send(text.into())?;
        if text.starts_with("hold") {
            let began = Instant::now();
            let limit = Duration::from_secs(if text.starts_with("holdlong") { 600 } else { 5 });
            while !cancel() {
                ensure!(began.elapsed() < limit, "never cancelled");
                std::thread::sleep(Duration::from_millis(2));
            }
            self.cancelled.send(text.into())?;
            anyhow::bail!("speech interrupted");
        }
        Ok(speak::Audio { samples: vec![0.1; 160], sample_rate: 16000, first_audio_ms: 0., synthesis_ms: 1. })
    }
}
struct Played(Arc<AtomicUsize>);
impl speak::AudioOutput for Played {
    fn output(&mut self, _: &speak::Audio, cancel: &dyn Fn() -> bool) -> Result<()> {
        if !cancel() {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
        Ok(())
    }
}
pub(crate) struct Speech {
    started: crossbeam_channel::Receiver<String>,
    cancelled: crossbeam_channel::Receiver<String>,
    played: Arc<AtomicUsize>,
}
impl Speech {
    pub(crate) fn install(a: &Pipeline) -> Self {
        let (started_tx, started) = crossbeam_channel::unbounded();
        let (cancelled_tx, cancelled) = crossbeam_channel::unbounded();
        let played = Arc::new(AtomicUsize::new(0));
        a.tools.test_guarded_speech_queue(Synth { started: started_tx, cancelled: cancelled_tx }, Played(played.clone()));
        Self { started, cancelled, played }
    }
    /// The agent's `speak` tool, as the MCP server calls it; returns once synthesis has begun.
    pub(crate) fn say(&self, a: &Pipeline, text: &str) {
        a.tools.speak(text, "en", &a.tools.agent_enabled.permit().unwrap()).unwrap();
        assert_eq!(self.started.recv_timeout(Duration::from_secs(5)).unwrap(), text);
    }
    pub(crate) fn cancelled(&self, text: &str) {
        assert_eq!(self.cancelled.recv_timeout(Duration::from_secs(5)).expect("speech kept playing"), text);
    }
    /// Nothing was cancelled (checked for 100 ms).
    pub(crate) fn still_speaking(&self) {
        assert!(self.cancelled.recv_timeout(Duration::from_millis(100)).is_err(), "speech stopped");
    }
    fn played(&self, n: usize) {
        let began = Instant::now();
        while self.played.load(Ordering::SeqCst) < n {
            assert!(began.elapsed() < Duration::from_secs(5), "speech never played");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(self.played.load(Ordering::SeqCst), n);
    }
}

/// Bug note 2026-10-27T20-20-25 (the user, 20:20: "barge-in doesn't cut speech that comes through the cover's speak"):
/// the relay's `speak` arrives through its own scoped server and the CLI, and is the same agent speech Pi's is:
/// The user talking over it is judged by the barge-in and stops it, and Stop cuts it.
#[test]
fn relay_speech_is_cut_by_barge_in_and_stop_like_pi_s() {
    struct NoOwn;
    #[async_trait::async_trait]
    impl agent::mcp::ScopedTools for NoOwn {
        fn definitions(&self) -> Vec<serde_json::Value> { vec![] }
        async fn call(&self, _: &str, _: &serde_json::Value, _: &agent::delivery::DeliveryPermit) -> Option<Result<String>> { None }
        fn record(&self, _: &str, _: &serde_json::Value, _: &std::result::Result<String, String>) {}
    }
    let (t, mut a, mic) = rig(true);
    let speech = Speech::install(&a);
    turn(&mut a, &mic, 2 * S, "hello there", Detect::Final, 0, Some(0.), S / 2, |_| {});
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
    let token = agent::emergency::new_token();
    let scope = agent::mcp::Scope { token: token.clone(), allow: agent::mcp::Allow::All, own: Arc::new(NoOwn) };
    let host: Arc<dyn ToolHost> = a.tools.clone();
    let server = rt.block_on(agent::mcp::McpServer::start_scoped(host, Arc::new(agent::mcp::UnavailableArchive), a.tools.agent_enabled.clone(), scope)).unwrap();
    let data = t.path().join("relay-cli");
    agent::emergency::write_connection(&data, &server.url, &token, "thread-1").unwrap();
    let relay_says = |text: &str| {
        let (code, out) = heimdall_emergency::run_as(&["call".to_string(), "speak".to_string(), serde_json::json!({"text": text}).to_string()], Some(data.clone()), Some("thread-1"));
        assert_eq!(code, 0, "{out}");
        assert_eq!(speech.started.recv_timeout(Duration::from_secs(5)).unwrap(), text);
    };
    relay_says("hold relay speech one");
    assert!(a.tools.heimdall_speaking(), "relay speech is Heimdall speaking: a barge-in arms on it");
    a.hooks.user_voice.extend([Some(0.8), Some(0.8)]);
    let m = mic.clone();
    turn(&mut a, &mic, 6 * S, "wait, stop", Detect::Final, 0, Some(0.), S, |a| hear(a, &m, 7 * S + 800_000_000));
    speech.cancelled("hold relay speech one");
    assert_eq!(barges(&a).last().unwrap()["outcome"], "stopped");
    relay_says("hold relay speech two");
    a.test_agent().send(crate::bridge::Command::Cancel).unwrap();
    speech.cancelled("hold relay speech two");
    rt.block_on(server.shutdown());
}

/// Agent on: the user's voice over a reply stops it (decision 31: by voice, no transcript, no decider). "Cancel task",
/// "stop completely", "stop speaking" and "open ..." are no longer commands (cancel removed 29 Oct;
/// 20:55): ordinary speech that reaches the agent with everything said since the last wake, exactly once, and nothing
/// cancels its run.
#[test]
fn barge_in_by_voice_and_no_cancel_command_with_the_agent_on() {
    let (_t, mut a, mic) = rig(true);
    let speech = Speech::install(&a);
    turn(&mut a, &mic, 2 * S, "hello there", Detect::Final, 0, Some(0.), S / 2, |_| {});
    assert!(a.agent.is_some());
    speech.say(&a, "hold barge");
    a.hooks.user_voice.extend([Some(0.8), Some(0.8)]);
    let m = mic.clone();
    turn(&mut a, &mic, 6 * S, "and another thing", Detect::Final, 0, Some(0.), S / 2, |a| hear(a, &m, 7 * S + 800_000_000));
    speech.cancelled("hold barge");
    let reasons: Vec<_> = barges(&a).iter().map(|b| b["reason"].as_str().unwrap_or("").to_owned()).collect();
    assert_eq!(reasons, ["user_voice_once", "user_voice"], "two windows of his voice in a row");
    for (n, text) in ["stop speaking right now", "open that in the browser", "cancel task right now", "can you stop completely I want to talk to you", "cancel the task"].iter().enumerate() {
        turn(&mut a, &mic, (10 + 2 * n as u64) * S, text, Detect::Window, 2, Some(0.), S / 2, |_| {});
    }
    assert_eq!(count(&records(&a), "command_decision"), 0, "a removed command was proposed");
    // A reply after all that plays normally: nothing cancelled the agent.
    speech.say(&a, "Done.");
    speech.played(1);
    turn(&mut a, &mic, 22 * S, "what is next", Detect::Final, 0, Some(1.), 3 * S / 2, |_| {});
    a.test_agent().barrier().unwrap();
    let r = wait_for(&a, "the wake", |r| completions(r) >= 1);
    let got: Vec<_> = r.iter().filter(|r| r.kind == "agent_reply" && r.payload["event"]["type"] == "mock_pi").map(|r| r.payload["event"]["request"]["message"].as_str().unwrap().to_string()).collect();
    assert_eq!(got.len(), 1, "{got:?}");
    let order: Vec<_> = ["hello there", "stop speaking right now", "open that in the browser", "cancel task right now", "can you stop completely", "cancel the task", "what is next"].iter().map(|t| got[0].find(t).unwrap_or_else(|| panic!("{t:?} missing:\n{}", got[0]))).collect();
    assert!(order.windows(2).all(|w| w[0] < w[1]), "{}", got[0]);
    assert!(!got[0].contains("agent task cancelled"), "{}", got[0]);
    assert!(!r.iter().any(|r| r.payload["outcome"] == "stopped_by_command"));
    assert_eq!(a.errors, 0, "{:?}", a.component_status);
}

/// E3.1 / E8.5: Stop ends the Speed sample as it ends agent speech, including a sample whose words are still waiting
/// for the voice to start.
#[test]
fn stop_cancels_the_speed_sample_too() {
    let (_t, a, _) = rig(true);
    let (started_tx, started) = crossbeam_channel::unbounded();
    let (cancelled_tx, cancelled) = crossbeam_channel::unbounded();
    let played = Arc::new(AtomicUsize::new(0));
    a.tools.test_sample_speech(Synth { started: started_tx, cancelled: cancelled_tx }, Played(played.clone()));
    a.tools.test_sample_says("hold the speed sample").unwrap();
    assert_eq!(started.recv_timeout(Duration::from_secs(5)).unwrap(), "hold the speed sample");
    a.tools.interrupt();
    assert_eq!(cancelled.recv_timeout(Duration::from_secs(5)).expect("the sample kept playing"), "hold the speed sample");
    assert_eq!(played.load(Ordering::SeqCst), 0);
}

/// "Heimdall, stop everything" is no longer a harness stop (the cancel family left the command list, 29 Oct): the user's
/// name wakes Heimdall with it, as any request; the agent decides what stopping means.
#[test]
fn addressed_stop_phrase_is_an_ordinary_wake_now() {
    let (_t, mut a, mic) = rig(true);
    turn(&mut a, &mic, 2 * S, "hello there", Detect::Final, 0, Some(1.), S / 2, |_| {});
    a.test_agent().barrier().unwrap();
    wait_for(&a, "first completion", |r| completions(r) >= 1);
    turn(&mut a, &mic, 6 * S, "Heimdall, stop everything", Detect::Final, 0, Some(1.), S / 2, |_| {});
    a.test_agent().barrier().unwrap();
    let r = wait_for(&a, "the stop phrase's wake", |r| completions(r) >= 2);
    let got: Vec<_> = r.iter().filter(|r| r.kind == "agent_reply" && r.payload["event"]["type"] == "mock_pi").map(|r| r.payload["event"]["request"]["message"].as_str().unwrap().to_string()).collect();
    assert!(got[1].contains("Heimdall, stop everything") && !got[1].contains("agent task cancelled"), "{got:#?}");
    assert!(!r.iter().any(|r| r.payload["outcome"] == "stopped_by_command"));
    assert_eq!(a.errors, 0, "{:?}", a.component_status);
}

async fn rpc(server: &agent::mcp::McpServer, call: serde_json::Value) -> serde_json::Value {
    rpc_as(server, call, &uuid::Uuid::new_v4().to_string()).await
}
/// One call under a given operation id (a replay sends the same one).
async fn rpc_as(server: &agent::mcp::McpServer, mut call: serde_json::Value, op: &str) -> serde_json::Value {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let address = server.url.trim_start_matches("http://").trim_end_matches("/mcp");
    let mut socket = tokio::net::TcpStream::connect(address).await.unwrap();
    call["_meta"]=serde_json::json!({"heimdallActionId":op});
    let body = serde_json::json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":call}).to_string();
    let request = format!("POST /mcp HTTP/1.1\r\nHost: {address}\r\nAccept: application/json, text/event-stream\r\nMCP-Protocol-Version: 2025-06-18\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\nAuthorization: Bearer {}\r\n\r\n{body}", body.len(), server.token);
    socket.write_all(request.as_bytes()).await.unwrap();
    let mut reply = String::new();
    socket.read_to_string(&mut reply).await.unwrap();
    assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
    serde_json::from_str::<serde_json::Value>(reply.split("\r\n\r\n").nth(1).unwrap()).unwrap()["result"].clone()
}

/// Taste ledger (24 Oct): `taste_note` writes one entry into this profile's ledger, timed from the archived line he
/// said it in; a replayed call (same operation id) answers the same line and adds nothing; `taste_search` finds it.
#[test]
fn taste_note_records_once_per_call_and_taste_search_finds_it() {
    let (_t, mut a, mic) = rig(true);
    turn(&mut a, &mic, 2 * S, "that icon looks cheap", Detect::Final, 3, Some(0.), S / 2, |_| {});
    let db = heimdall_archive::store::open(&a.data.join("archive")).unwrap();
    let (line, wall): (String, i64) = db.query_row("SELECT id, wall_ms FROM utterances WHERE text LIKE '%icon looks cheap%'", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    let archive = Arc::new(crate::index::ArchiveSearch { root: a.data.join("archive"), embeddings: a.tools.embeddings.clone() });
    let server = rt.block_on(agent::mcp::McpServer::start(a.tools.clone(), archive, a.tools.agent_enabled.clone())).unwrap();
    let call = serde_json::json!({"name":"taste_note","arguments":{"subject":"app icon","kind":"icon","verdict":"dislike","words":"that icon looks cheap","why":"marks must look crafted","lines":[line]}});
    let first = rt.block_on(rpc_as(&server, call.clone(), "op-taste-1"));
    let text = first["content"][0]["text"].as_str().unwrap_or_default().to_owned();
    assert!(first.get("isError").is_none() && text.starts_with("Noted: taste-"), "{first}");
    let again = rt.block_on(rpc_as(&server, call.clone(), "op-taste-1"));
    assert_eq!(again["content"][0]["text"], text.as_str(), "a replay answers the first call's line, with its id");
    let ledger = agent::taste::load(&agent::taste::path(&a.data)).unwrap();
    assert_eq!(ledger.len(), 1);
    assert_eq!(ledger[0].at_ms(), wall / 1000 * 1000, "timed from the line, to the second");
    assert_eq!(ledger[0].lines, [line]);
    // A new call with the same words: the ledger's own repeat check.
    let repeat = rt.block_on(rpc(&server, call));
    assert!(repeat["content"][0]["text"].as_str().unwrap().starts_with("Already in the ledger as taste-"), "{repeat}");
    let found = rt.block_on(rpc(&server, serde_json::json!({"name":"taste_search","arguments":{"query":"icon","verdict":"dislike"}})));
    assert!(found["content"][0]["text"].as_str().unwrap().contains("dislike · icon · app icon: \"that icon looks cheap\""), "{found}");
}

/// MCP v2 on the real host: literal timeline attribution, recorded screen evidence,
/// durable speech receipts, barge-in interruption and Speaking-off refusal.
#[test]
fn mcp_tools_search_the_archive_take_screenshots_and_speak() {
    let (_t, mut a, mic) = rig(true);
    a.command_test_answers.push_back((I::Yes, C::LookAtThis));
    turn(&mut a, &mic, 2 * S, "look at this the chart is wrong", Detect::Final, 3, Some(0.), S / 2, |_| {});
    let speech = Speech::install(&a);
    let now = capture::host_now_ns();
    for d in [1u32, 2] {
        let display = screen::DisplayInfo { id: d, x: 0., y: 0., width: 2, height: 1 };
        a.screen_buffer.lock().unwrap().push(screen::Frame::new(display, now, 2, 1, [100, d as u8, 7].repeat(2)).unwrap()).unwrap();
    }
    *a.tools.buffer.lock().unwrap() = Some(a.screen_buffer.clone());
    *a.tools.test_grabber.lock().unwrap() = Some(Arc::new(|id| {
        let display = screen::DisplayInfo { id, x: 0., y: 0., width: 4, height: 2 };
        screen::Frame::new(display, capture::host_now_ns(), 4, 2, [200, id as u8, 9].repeat(8))
    }));
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    let archive = Arc::new(crate::index::ArchiveSearch { root: a.data.join("archive"), embeddings: a.tools.embeddings.clone() });
    let server = rt.block_on(agent::mcp::McpServer::start(a.tools.clone(), archive, a.tools.agent_enabled.clone())).unwrap();

    let found = rt.block_on(rpc(&server, serde_json::json!({"name":"timeline_search","arguments":{"query":"chart is wrong"}})));
    assert!(found.get("isError").is_none(), "{found}");
    // tool-output (19 Oct): one plain line per hit, never raw JSON.
    let rows = found["content"][0]["text"].as_str().unwrap();
    // Token diet (26 Oct 21:43): a short source label and a handle, never the device string or the full id.
    assert!(rows.lines().any(|l|l.contains(" · Mic · User · [h")&&l.ends_with("] look at this the chart is wrong")),"{rows}");
    assert!(!rows.contains("mic:replay"),"{rows}");

    let chunks = rt.block_on(rpc(&server, serde_json::json!({"name":"timeline_search","arguments":{"query":"chart is wrong","kinds":["text"],"tags":[{"key":"speaker","value":"User"}]}})));
    assert!(chunks.get("isError").is_none(), "{chunks}");
    let chunks = chunks["content"][0]["text"].as_str().unwrap().to_owned();
    assert!(chunks.lines().any(|l| l.ends_with("] look at this the chart is wrong")), "{chunks}");
    // output full names the stored id; the handle on the line opens the same text.
    let full = rt.block_on(rpc(&server, serde_json::json!({"name":"timeline_search","arguments":{"query":"chart is wrong","kinds":["text"],"output":"full"}})));
    let full = full["content"][0]["text"].as_str().unwrap().to_owned();
    let head = full.lines().next().unwrap_or_else(|| panic!("{full}"));
    let handle = head.split("[").nth(1).unwrap().split("]").next().unwrap().to_owned();
    // Preserve main's semantic archive search through the single timeline tool.
    let chunk_id = head.split("] ").nth(1).unwrap().to_owned();
    let opened = rt.block_on(rpc(&server, serde_json::json!({"name":"timeline_search","arguments":{"id":handle}})));
    assert!(opened["content"][0]["text"].as_str().unwrap().ends_with("look at this the chart is wrong"), "{opened}");
    let db=heimdall_archive::store::open(&a.data.join("archive")).unwrap();
    let hash:String=db.query_row("SELECT content_hash FROM timeline_chunks WHERE id=?",[&chunk_id],|r|r.get(0)).unwrap();
    let vector=a.index.evidence.vector(&[1.,0.]).unwrap();
    db.execute("INSERT OR REPLACE INTO chunk_embeddings VALUES(?1,'plumbing-test','1',2,?2,?3,0)",rusqlite::params![chunk_id,vector,hash]).unwrap();
    let ranked=rt.block_on(rpc(&server,serde_json::json!({"name":"timeline_search","arguments":{"vector":{"model_id":"plumbing-test","model_version":"1","values":[1.,0.]},"tags":[{"key":"speaker","value":"User"}],"limit":1}})));
    assert!(ranked.get("isError").is_none(),"{ranked}");
    let ranked=ranked["content"][0]["text"].as_str().unwrap();
    assert!(ranked.contains(" · 1.00 · [h")&&ranked.ends_with("] look at this the chart is wrong"),"{ranked}");
    let invalid = rt.block_on(rpc(&server, serde_json::json!({"name":"timeline_search","arguments":{"limit":0}})));
    assert_eq!(invalid["isError"], true);

    // Fixture recording lives in the same archive the MCP reads; no native grab.
    let wall=heimdall_archive::wall_ns();
    let writer=heimdall_archive::video::Writer::open(&a.session).unwrap();
    let segment=writer.begin(&heimdall_archive::video::Geometry{display:1,x:0.,y:0.,width:2,height:1},&heimdall_archive::video::Format{width:2,height:1,hvcc:vec![],parameters:vec![]},now,wall,5).unwrap();
    let mut fixture=std::io::Cursor::new(vec![]);
    image::RgbImage::from_pixel(2,1,image::Rgb([100,10,7])).write_to(&mut fixture,image::ImageFormat::Png).unwrap();
    writer.append(segment,&heimdall_archive::video::Sample{host_ns:now,end_ns:now+S,key:true,data:fixture.into_inner()}).unwrap();
    let shot=rt.block_on(rpc(&server,serde_json::json!({"name":"screen_frames","arguments":{"at":agent::mcp_contract::utc(wall),"display_id":"1"}})));
    assert!(shot.get("isError").is_none(),"{shot}");
    let metadata=shot["content"][0]["text"].as_str().unwrap();
    let captured=agent::plain::local_time(&agent::mcp_contract::utc(wall));
    assert!(metadata.starts_with("Frame screen:")&&metadata.contains(&format!(" · captured {captured} · display 1 · ")),"{metadata}");
    assert_eq!(shot["content"][1]["type"],"image");assert_eq!(shot["content"][1]["mimeType"],"image/jpeg");

    let spoken = rt.block_on(rpc(&server, serde_json::json!({"name":"speak","arguments":{"text":"Done."}})));
    assert!(spoken["content"][0]["text"].as_str().unwrap().starts_with("Speech queued"), "{spoken}");
    assert_eq!(speech.started.recv_timeout(Duration::from_secs(5)).unwrap(), "Done.");
    speech.played(1);
    // The user starts talking: the reply stops.
    rt.block_on(rpc(&server, serde_json::json!({"name":"speak","arguments":{"text":"hold reply"}})));
    assert_eq!(speech.started.recv_timeout(Duration::from_secs(5)).unwrap(), "hold reply");
    a.audio.insert(mic.clone(), AudioRing { start: 29 * S, samples: vec![0.01; 16000].into() });
    a.listen(ListenEvent::SpeechStart { source: mic.clone(), host_time_ns: 30 * S }).unwrap();
    speech.still_speaking();
    a.hooks.user_voice.extend([Some(0.8), Some(0.8)]);
    hear(&mut a, &mic, 31 * S + 720_000_000);
    speech.cancelled("hold reply");
    a.listen(ListenEvent::SpeechEnd { source: mic.clone(), host_time_ns: 31 * S + 800_000_000 }).unwrap();
    // Speaking off: the reply stops, and the next line is shown as text instead of failing.
    rt.block_on(rpc(&server, serde_json::json!({"name":"speak","arguments":{"text":"hold again"}})));
    assert_eq!(speech.started.recv_timeout(Duration::from_secs(5)).unwrap(), "hold again");
    a.action(ui::UiAction::SetSpeakReplies(false)).unwrap();
    speech.cancelled("hold again");
    a.tools.mcp_actions.attach(ui::Ui::handle(), Arc::new(|_, _, _| Ok(())), Arc::new(|_, _| Ok(())), Arc::new(|_, _| Ok(()))).unwrap();
    let shown = rt.block_on(rpc(&server, serde_json::json!({"name":"speak","arguments":{"text":"Shown."}})));
    assert!(shown.get("isError").is_none(), "{shown}");
    assert_eq!(shown["content"][0]["text"], "Shown on the island.");
    a.action(ui::UiAction::SetSpeakReplies(true)).unwrap();
    rt.block_on(rpc(&server, serde_json::json!({"name":"speak","arguments":{"text":"Back."}})));
    speech.played(2);
    rt.block_on(server.shutdown());
}

// Barge-in: talking over Heimdall stops it after about a second, unless the decider is
// confident it should keep talking. There is no "stop speaking" command.

/// A pipeline with Speaking on and a held agent reply ("hold ...") in progress; the capture ring starts
/// at 0 and runs to `from`.
pub(crate) fn speaking(saying: &str, from: u64) -> (tempfile::TempDir, Pipeline, String, Speech) {
    let (t, mut a, mic) = rig(false);
    a.tools.set_agent_enabled(true);
    let speech = Speech::install(&a);
    speech.say(&a, saying);
    a.audio.insert(mic.clone(), AudioRing { start: 0, samples: vec![0.01; (from / 62500) as usize].into() });
    (t, a, mic, speech)
}
/// The capture path: 20-ms chunks into the ring up to `until`, each followed by the barge-in check.
pub(crate) fn hear(a: &mut Pipeline, mic: &str, until: u64) {
    while a.audio[mic].end() < until {
        let at = a.audio[mic].end();
        a.audio.get_mut(mic).unwrap().push(at, &[0.01; 320]);
        a.barge_audio(mic).unwrap();
    }
}
pub(crate) fn barges(a: &Pipeline) -> Vec<serde_json::Value> {
    records(a).into_iter().filter(|r| r.kind == "barge_in").map(|r| r.payload).collect()
}

/// Heimdall's own app audio never starts a barge-in; its voice on the microphone is judged by voice (heimdall_voice_tests.rs).
#[test]
fn heimdalls_own_app_audio_never_arms_a_barge_in() {
    let (_t, mut a, _mic, speech) = speaking("hold the linker could not find the metal library", 2 * S);
    let own = capture::SourceInfo::new(
        capture::SourceKind::Process { name: "Heimdall".into(), bundle_id: Some("dev.heimdall.app".into()), pid: std::process::id(), birth_us: 1, owner: None, output_devices: vec![] },
        capture::NativeFormat { sample_rate: 16000, channels: 1 },
        1,
    );
    let own_id = own.id.0.clone();
    a.source(own);
    a.audio.insert(own_id.clone(), AudioRing { start: 0, samples: vec![0.01; 32000].into() });
    a.hooks.user_voice.push_back(Some(0.9));
    a.listen(ListenEvent::SpeechStart { source: own_id.clone(), host_time_ns: 2 * S }).unwrap();
    hear(&mut a, &own_id, 4 * S);
    assert!(a.barge.is_none() && barges(&a).is_empty(), "Heimdall's own audio armed a barge-in");
    speech.still_speaking();
    a.tools.interrupt();
    speech.cancelled("hold the linker could not find the metal library");
}

/// Speaking off and Listening off stop Heimdall at once, even while a barge-in waits for its second.
#[test]
/// Speaking off stops Heimdall at once. Listening off ends the barge-in (no mic to judge) but never the agent's speech:
/// each switch turns off only itself.
fn speaking_off_stops_immediately_and_listening_off_leaves_speech_playing() {
    let (_t, mut a, mic, speech) = speaking("hold first", 2 * S);
    a.listen(ListenEvent::SpeechStart { source: mic.clone(), host_time_ns: 2 * S }).unwrap();
    hear(&mut a, &mic, 2 * S + 300_000_000);
    a.action(ui::UiAction::SetSpeakReplies(false)).unwrap();
    speech.cancelled("hold first");
    hear(&mut a, &mic, 4 * S);
    assert!(a.barge.is_none());
    assert_eq!(barges(&a)[0]["outcome"], "heimdall_finished", "nothing left to stop");
    a.action(ui::UiAction::SetSpeakReplies(true)).unwrap();
    a.listen(ListenEvent::SpeechEnd { source: mic.clone(), host_time_ns: 4 * S }).unwrap();
    speech.say(&a, "hold second");
    a.listen(ListenEvent::SpeechStart { source: mic.clone(), host_time_ns: 5 * S }).unwrap();
    hear(&mut a, &mic, 5 * S + 300_000_000);
    assert!(a.barge.is_some());
    a.set_running(false, "test").unwrap();
    speech.still_speaking();
    assert!(a.barge.is_none());
}

/// Heimdall starts a reply while the user is already talking: that is a barge-in too, and his voice stops it.
#[test]
fn heimdall_starting_over_ongoing_speech_is_judged() {
    let (_t, mut a, mic) = rig(false);
    a.tools.set_agent_enabled(true);
    let speech = Speech::install(&a);
    a.audio.insert(mic.clone(), AudioRing { start: 0, samples: vec![0.01; 32000].into() });
    a.listen(ListenEvent::SpeechStart { source: mic.clone(), host_time_ns: 2 * S }).unwrap();
    hear(&mut a, &mic, 3 * S);
    assert!(a.barge.is_none(), "Heimdall was quiet");
    speech.say(&a, "hold late reply");
    hear(&mut a, &mic, 3 * S + 20_000_000);
    assert!(a.barge.is_some());
    a.hooks.user_voice.extend([Some(0.8), Some(0.8)]);
    hear(&mut a, &mic, 4 * S + 740_000_000);
    speech.cancelled("hold late reply");
    let b = barges(&a);
    let last = b.last().unwrap();
    assert_eq!((last["armed_by"].as_str(), last["onset_ns"].as_u64(), last["outcome"].as_str()), (Some("heimdall_started_over_speech"), Some(3 * S + 20_000_000), Some("stopped")));
}

/// "Stop speaking" is no longer a command: no candidate, no decision, the words reach the agent.
#[test]
fn removed_commands_are_not_recognised() {
    for text in ["stop speaking right now", "stop talking please", "open that in the browser", "open discord now"] {
        for detect in [Detect::Final, Detect::Window] {
            let (_t, mut a, mic) = rig(false);
            turn(&mut a, &mic, 4 * S, text, detect, 2, Some(0.), S / 2, |_| {});
            let r = records(&a);
            assert_eq!(count(&r, "command_decision"), 0, "{text:?} via {detect:?}");
            assert!(a.pending_text.iter().any(|t| t.contains(text)), "{text:?} via {detect:?} did not reach the agent");
            assert!(final_line_notes(&a).is_empty());
        }
    }
}

/// 16 Oct: a long turn repeated the screenshot request before any feedback. The
/// partial cue is immediate; only an approved command executes, once per kind.
#[test]
fn partial_screen_cue_precedes_final_and_repeated_requests_execute_once() {
    let (_t,mut a,mic)=rig(false);
    let text="look at this I just want you to take a screenshot you did not take it. take a screenshot";
    a.command_test_answers.extend([(I::Yes,C::LookAtThis),(I::Yes,C::TakeScreenshot)]);
    turn(&mut a,&mic,4*S,text,Detect::Final,3,Some(0.),S/2,|a| {
        let words=timed("look at this",4*S);
        a.listen(ListenEvent::PartialTranscript {source:mic.clone(),text:"look at this".into(),words,stable_words:0}).unwrap();
        // A re-decode can move word timestamps slightly; it is still one cue.
        let mut words=timed("look at this",4*S);
        for word in &mut words {word.start_ns+=10_000_000;word.end_ns+=10_000_000;}
        a.listen(ListenEvent::PartialTranscript {source:mic.clone(),text:"look at this".into(),words,stable_words:0}).unwrap();
        assert_eq!(a.command_candidates.len(),1);
        assert_eq!(a.hooks.ui.lock().unwrap().iter().filter(|u|matches!(u,ui::UiUpdate::Command(ui::CommandCue::Heard{..}))).count(),1);
        assert_eq!(count(&records(a),"command_action"),0,"tentative words cannot execute");
        assert!(a.hooks.ui.lock().unwrap().iter().any(|u| matches!(u,ui::UiUpdate::Command(ui::CommandCue::Heard{kind:ui::CommandKind::LookAtThis,..}))));
    });
    let r=records(&a);
    let actions:Vec<_>=r.iter().filter(|r|r.kind=="command_action").map(|r|r.payload["command"].as_str().unwrap()).collect();
    assert_eq!(actions,["look_at_this","take_screenshot"]);
    assert!(r.iter().any(|r|r.payload["reason"]=="same_command_in_utterance"));
    let done:Vec<_>=a.hooks.ui.lock().unwrap().iter().filter_map(|u|match u {
        ui::UiUpdate::Command(ui::CommandCue::Done{kind,..})=>Some(*kind),_=>None}).collect();
    assert_eq!(done,[ui::CommandKind::LookAtThis,ui::CommandKind::Screenshot],"each distinct result must reach the island");
    assert!(a.screen_holds.is_empty() && a.command_candidates.is_empty());
}
#[test]
fn retracted_partial_or_rejected_screen_command_never_executes() {
    for rejected in [false,true] {
        let (_t,mut a,mic)=rig(false);
        if rejected {a.command_test_answers.push_back((I::FalseAlarm,C::Unsure));}
        let text=if rejected {"take a screenshot is what he said"}else{"ordinary conversation without a command"};
        turn(&mut a,&mic,4*S,text,Detect::Final,3,Some(0.),S/2,|a| {
            let words=timed("take a screenshot",4*S);
            a.listen(ListenEvent::PartialTranscript {source:mic.clone(),text:"take a screenshot".into(),words,stable_words:0}).unwrap();
            assert_eq!(count(&records(a),"command_action"),0);
        });
        let r=records(&a);
        for kind in ["command_action","screen_command","screenshot"] {assert_eq!(count(&r,kind),0);}
        let cues:Vec<_>=a.hooks.ui.lock().unwrap().iter().filter_map(|u|match u {ui::UiUpdate::Command(c)=>Some(c.clone()),_=>None}).collect();
        assert!(matches!(cues.first(),Some(ui::CommandCue::Heard{kind:ui::CommandKind::Screenshot,..})));
        assert!(matches!(cues.last(),Some(ui::CommandCue::Dropped{..})));
        assert!(!cues.iter().any(|c|matches!(c,ui::CommandCue::Done{..})));
        assert!(a.attachment_batch.is_empty() && a.screen_holds.is_empty() && a.command_candidates.is_empty());
    }
}

#[test]
fn rolling_send_keeps_wait_and_asides_until_a_trigger_and_journals_every_final() {
    let (_t, mut a, mic)=rig(false);
    turn(&mut a,&mic,10*S,"I was thinking we could",Detect::Final,0,Some(0.),3*S,|_|{});
    assert!(a.pending_text.iter().cloned().collect::<Vec<_>>().join("\n").contains("thinking"));
    a.hooks.send_choices.push_back(decide::send::Choice::NotForHeimdall);
    turn(&mut a,&mic,20*S,"Alex are you coming",Detect::Final,0,None,3*S,|_|{});
    turn(&mut a,&mic,30*S,"Heimdal move that file",Detect::Final,0,Some(0.),0,|_|{});
    a.agent.as_ref().unwrap().barrier().unwrap();
    let r=records(&a);let decisions:Vec<_>=r.iter().filter(|r|r.kind=="send_decision").collect();
    assert_eq!(decisions.len(),3);
    assert_eq!(decisions[0].payload["outcome"],"wait");
    assert_eq!(decisions[1].payload["outcome"],"not_for_heimdall");
    assert_eq!(decisions[2].payload["trigger"],"name");
    let prompt=decisions[2].payload["prompt"]["state"].as_str().unwrap();
    assert!(prompt.contains("thinking") && prompt.contains("Alex are you coming"));
    assert!(decisions[2].payload["audio"]["pcm_sha256"].is_string());
    assert!(a.pending_text.is_empty());
    let log=std::fs::read_to_string(crate::archive::run_dir(&a.data,&a.session).join("dry-run.jsonl")).unwrap();
    let queries:Vec<serde_json::Value>=log.lines().map(|l|serde_json::from_str(l).unwrap()).filter(|r:&serde_json::Value|r["would_send"]["shouldQuery"]==true).collect();
    assert_eq!(queries.len(),1);
    let query=queries[0].to_string();assert!(query.contains("observations already in context"));
    assert_eq!(queries[0]["would_send"]["observation_ids"].as_array().unwrap().len(),3);
    for words in ["thinking","Alex are you coming","Heimdal move"] {assert_eq!(log.matches(words).count(),1,"each observation is supplied once");}
}

/// D2 (19 Oct 18:45:00): with "Start a thread on one test desktop widget?" up, the user's "Yes, but let's keep it as a
/// separate project" scored send 0.41 / not_for_heimdall 0.51 and reached the agent only because it named Heimdall.
/// Review 20 Oct: d1 stays in the loop. Inside the card's minute his talk is sent at the card's lower bar; a filler
/// d1 would send, an aside below the bar, talk before the card and talk after the window do not wake the agent; and
/// the card closes with the words that were for Heimdall only.
#[test]
fn talk_while_a_question_card_is_up_goes_through_d1_at_the_cards_lower_bar() {
    let (_t, mut a, mic) = rig(true);
    let card = a.tools.test_open_card("Start a thread on one test desktop widget?", 20 * S);
    // (start, text, d1 send probability, d1 top choice is send)
    for (start, text, p) in [
        (10 * S, "that would be really cool actually", 0.41),
        (22 * S, "Um.", 0.72),
        (25 * S, "Alex are you coming", 0.20),
        (30 * S, "Yes, but let's keep it as a separate project", 0.41),
        (200 * S, "Alex are you coming", 0.41),
    ] {
        turn(&mut a, &mic, start, text, Detect::Final, 0, Some(p), 3 * S, |_| {});
    }
    a.agent.as_ref().unwrap().barrier().unwrap();
    let r = records(&a);
    let decisions: Vec<_> = r.iter().filter(|r| r.kind == "send_decision").collect();
    assert_eq!(decisions.len(), 5);
    let outcome = |i: usize| (decisions[i].payload["outcome"].as_str().unwrap().to_owned(), decisions[i].payload["trigger"].as_str().map(str::to_owned));
    assert_eq!(outcome(0), ("wait".into(), None), "ended before the card appeared: d1's top choice");
    assert_eq!(outcome(1), ("filler_held".into(), None), "a filler never wakes the agent while a card is up");
    assert_eq!(outcome(2), ("wait".into(), None), "an aside below the card's bar");
    assert_eq!(outcome(3), ("send".into(), Some("open_question".into())));
    assert_eq!(decisions[3].payload["question_card"], "answering");
    assert_eq!(outcome(4), ("wait".into(), None), "began long after the card appeared, and it stayed open");
    assert_eq!(r.iter().filter(|r| r.kind == "agent_wake").count(), 1);
    // Talk never closes the card (25 Oct): it stays open and answerable, and his words are no answer to it.
    let actions = a.tools.mcp_actions.actions.as_ref().unwrap();
    assert_eq!(actions.state(&card).unwrap(), "shown");
    assert!(actions.ask_answer(&card).unwrap().is_none());
}

/// No follow-up window (decision 33): an aside right after Heimdall's spoken question stays pending on d1's verdict,
/// card or no card; his name still wakes at once; and after the card closes, a reply just after Heimdall spoke is d1's
/// to send, never a bypass.
#[test]
fn no_follow_up_window_with_or_without_a_card() {
    let (_t, mut a, mic) = rig(true);
    let card = a.tools.test_open_card("Delete AllForOne's heavy files and archive the rest?", 20 * S);
    a.tools.playback_ended(20 * S);
    turn(&mut a, &mic, 22 * S, "Oh shit, that's my mom calling", Detect::Final, 0, Some(0.2), 3 * S, |_| {});
    a.agent.as_ref().unwrap().barrier().unwrap();
    let r = records(&a);
    let d: Vec<_> = r.iter().filter(|r| r.kind == "send_decision").collect();
    assert_eq!((d[0].payload["outcome"].as_str(), d[0].payload["trigger"].as_str()), (Some("wait"), None));
    assert_eq!(r.iter().filter(|r| r.kind == "agent_wake").count(), 0);
    assert_eq!(a.tools.mcp_actions.actions.as_ref().unwrap().state(&card).unwrap(), "shown");
    // His name is still explicit: it wakes the agent at once; the card stays open.
    turn(&mut a, &mic, 30 * S, "Heimdall, no, keep the archive", Detect::Final, 0, Some(0.2), 3 * S, |_| {});
    a.agent.as_ref().unwrap().barrier().unwrap();
    let r = records(&a);
    assert_eq!(r.iter().filter(|r| r.kind == "send_decision").last().unwrap().payload["trigger"], "name");
    assert_eq!(r.iter().filter(|r| r.kind == "agent_wake").count(), 1);
    // Talk never closes the card (25 Oct): the name wakes the agent, and the card stays open until a click or ask_close.
    let actions = a.tools.mcp_actions.actions.as_ref().unwrap();
    assert_eq!(actions.state(&card).unwrap(), "shown");
    assert!(actions.ask_answer(&card).unwrap().is_none());
    // The agent closes it (ask_close); a reply right after Heimdall spoke wakes only by d1's verdict.
    let p = a.tools.agent_enabled.permit().unwrap();
    assert_eq!(actions.close_question(&card, "Heimdall, no, keep the archive", &p).unwrap(), "closed");
    a.tools.playback_ended(40 * S);
    turn(&mut a, &mic, 42 * S, "and the build folder too", Detect::Final, 0, Some(0.2), 3 * S, |_| {});
    a.agent.as_ref().unwrap().barrier().unwrap();
    let last = records(&a).into_iter().filter(|r| r.kind == "send_decision").last().unwrap().payload;
    assert_eq!((last["trigger"].as_str(), last["outcome"].as_str()), (None, Some("wait")), "{last}");
    turn(&mut a, &mic, 46 * S, "and the build folder too please", Detect::Final, 0, Some(1.), 3 * S, |_| {});
    a.agent.as_ref().unwrap().barrier().unwrap();
    assert_eq!(records(&a).iter().filter(|r| r.kind == "send_decision").last().unwrap().payload["trigger"], "d1");
}

#[test]
fn typed_send_is_a_barrier_in_the_same_conversation() {
    let (_t,mut a,mic)=rig(true);
    turn(&mut a,&mic,10*S,"An unfinished thought",Detect::Final,0,Some(0.),3*S,|_|{});
    a.agent.as_ref().unwrap().window_action(ui::AgentWindowAction::Send {text:"Use the current folder".into(),steer:false}).unwrap();
    a.agent.as_ref().unwrap().barrier().unwrap();
    let snapshot=a.tools.send_context.lock().unwrap().snapshot();
    assert!(snapshot.pending.is_empty());
    assert!(snapshot.turns.iter().any(|t|t.speaker=="User (typed)" && t.text=="Use the current folder"));
    turn(&mut a,&mic,20*S,"Another new thought",Detect::Final,0,Some(0.),3*S,|_|{});
    let r=records(&a);let last=r.iter().filter(|r|r.kind=="send_decision" && r.source!="typed").last().unwrap();
    let state=last.payload["prompt"]["state"].as_str().unwrap();
    assert!(state.contains("User (typed): Use the current folder"));
    let pending=state.split("Pending speech since last send (includes current sentence):\n").nth(1).unwrap();
    assert!(pending.contains("Another new thought") && !pending.contains("unfinished"));
    assert!(r.iter().any(|r|r.kind=="send_decision" && r.payload["trigger"]=="typed"));
}

#[test]
fn typed_barrier_suppresses_an_inference_result_queued_after_it() {
    let (_t,mut a,mic)=rig(true);
    turn(&mut a,&mic,10*S,"An unfinished thought",Detect::Final,0,Some(0.),3*S,|_|{});
    let through=a.tools.send_context.lock().unwrap().snapshot().through;
    let runtime=a.agent.as_ref().unwrap();
    runtime.window_action(ui::AgentWindowAction::Send {text:"Use the current folder".into(),steer:false}).unwrap();
    let permit=runtime.wake_permit(false).unwrap();
    runtime.wake_pending_with_permit(vec![],false,through,permit).unwrap();
    runtime.barrier().unwrap();
    let r=records(&a);
    assert!(!r.iter().any(|r|r.kind=="agent_wake"));
    assert!(r.iter().any(|r|r.kind=="send_decision_delivery" && r.payload["outcome"]=="superseded_by_barrier"));
}

#[test]
fn completed_speech_enters_context_and_arms_follow_up_from_playback() {
    struct Synth;
    impl speak::Synthesizer for Synth {
        fn synthesize(&mut self,_:&str,_:&str,_:&dyn Fn()->bool)->Result<speak::Audio> {
            Ok(speak::Audio {samples:vec![0.;160],sample_rate:16000,first_audio_ms:0.,synthesis_ms:1.})
        }
    }
    struct Output;
    impl speak::AudioOutput for Output {fn output(&mut self,_:&speak::Audio,_:&dyn Fn()->bool)->Result<()> {Ok(())}}
    let (_t,a,_)=rig(true);
    a.tools.test_guarded_speech_queue(Synth,Output);
    a.tools.note_spoken("tool narration is not a final answer");
    let permit=a.tools.agent_enabled.permit().unwrap();
    agent::mcp::ToolHost::speak(a.tools.as_ref(),"The file is ready.","en",&permit).unwrap();
    a.tools.test_finish_speech().unwrap();
    a.tools.speech_status();
    let context=a.tools.send_context.lock().unwrap();let snapshot=context.snapshot();
    assert_eq!(snapshot.turns.len(),1);
    assert_eq!((&*snapshot.turns[0].speaker,&*snapshot.turns[0].text),("Heimdall","The file is ready."));
    let finished=context.finished_ns.unwrap();
    // No follow-up window (decision 33): "Okay" right after the reply goes to the decider.
    assert_eq!(decide::send::explicit_trigger("Okay",true,finished+S,context.finished_ns),None);
    assert_eq!(decide::send::explicit_trigger("Okay",true,finished+30*S+1,context.finished_ns),None);
}

/// `speak` is one tool in both modes ("completely agnostic to whether the sound is on or off"), and
/// every line, mid-run included, joins the decider's conversation when it is said ("that last message should also
/// be included into the decider's conversation flow"). Only real playback opens the 30-second reply window
/// (docs/briefs/decider-phase2-text.md); a silent island line or question card never does.
#[test]
fn speak_and_ask_enter_decider_context_in_both_modes_and_only_playback_opens_the_window() {
    struct Synth;
    impl speak::Synthesizer for Synth {
        fn synthesize(&mut self,_:&str,_:&str,_:&dyn Fn()->bool)->Result<speak::Audio> {
            Ok(speak::Audio {samples:vec![0.;160],sample_rate:16000,first_audio_ms:0.,synthesis_ms:1.})
        }
    }
    struct Output;
    impl speak::AudioOutput for Output {fn output(&mut self,_:&speak::Audio,_:&dyn Fn()->bool)->Result<()> {Ok(())}}
    let (_t,mut a,_)=rig(true);
    a.tools.test_guarded_speech_queue(Synth,Output);
    a.tools.mcp_actions.attach(ui::Ui::handle(),Arc::new(|_,_,_|Ok(())),Arc::new(|_,_|Ok(())),Arc::new(|_,_|Ok(()))).unwrap();
    let rt=tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    let archive=Arc::new(crate::index::ArchiveSearch{root:a.data.join("archive"),embeddings:a.tools.embeddings.clone()});
    let server=rt.block_on(agent::mcp::McpServer::start(a.tools.clone(),archive,a.tools.agent_enabled.clone())).unwrap();
    // The agent is mid-run: a request was sent and no final answer has settled.
    {let mut c=a.tools.send_context.lock().unwrap();let through=c.snapshot().through;c.sent(through);assert!(c.working);}
    let heimdall=|a:&Pipeline|a.tools.send_context.lock().unwrap().snapshot().turns.iter().filter(|t|t.speaker=="Heimdall").map(|t|t.text.clone()).collect::<Vec<_>>();
    let window=|a:&Pipeline|a.tools.send_context.lock().unwrap().finished_ns;
    // Speaking on: heard, then a Heimdall turn at its audible end, and the reply window opens.
    let said=rt.block_on(rpc(&server,serde_json::json!({"name":"speak","arguments":{"text":"Which branch?"}})));
    assert!(said["content"][0]["text"].as_str().unwrap().starts_with("Speech queued"),"{said}");
    a.tools.test_finish_speech().unwrap();a.tools.speech_status();
    assert_eq!(heimdall(&a),["Which branch?"]);
    let heard=window(&a).unwrap();
    assert_eq!(decide::send::explicit_trigger("the main one",true,heard+S,Some(heard)),None,"no follow-up window (decision 33)");
    a.tools.send_context.lock().unwrap().finished_ns=None;
    // Speaking off: the same call shows the text, never an error; it is a Heimdall turn, but text never opens the window.
    a.action(ui::UiAction::SetSpeakReplies(false)).unwrap();
    let shown=rt.block_on(rpc(&server,serde_json::json!({"name":"speak","arguments":{"text":"Build is green."}})));
    assert!(shown.get("isError").is_none(),"{shown}");
    assert_eq!(shown["content"][0]["text"],"Shown on the island.");
    assert_eq!(heimdall(&a),["Which branch?","Build is green."]);
    assert_eq!(window(&a),None,"a silent island line must not open the reply window");
    assert!(a.tools.send_context.lock().unwrap().working,"a mid-run line does not end the run");
    let db=rusqlite::Connection::open(a.data.join("agent/mcp-actions.sqlite")).unwrap();
    let body:String=db.query_row("SELECT body FROM receipts WHERE kind='notice' ORDER BY rowid DESC LIMIT 1",[],|r|r.get(0)).unwrap();
    assert!(body.contains("Build is green."),"{body}");
    // A silent question card: the decider sees the question (once, even when the call is replayed), no window.
    let permit=a.tools.agent_enabled.permit().unwrap();
    let ask=serde_json::json!({"question":"Install the new build?","options":["Yes","No"]});
    for _ in 0..2 { rt.block_on(a.tools.mcp_actions.action(&a.tools,"ask",ask.clone(),"ask-silent",&permit)).unwrap(); }
    assert_eq!(heimdall(&a),["Which branch?","Build is green.","Install the new build?"]);
    assert_eq!(window(&a),None,"a silent question must not open the reply window");
    // Speaking on: the card is still silent (speech-policy, night 3: cards are never read aloud), one turn, no window.
    a.action(ui::UiAction::SetSpeakReplies(true)).unwrap();
    let permit=a.tools.agent_enabled.permit().unwrap();
    rt.block_on(a.tools.mcp_actions.action(&a.tools,"ask",serde_json::json!({"question":"Push it too?","options":["Yes","No"]}),"ask-with-speaking-on",&permit)).unwrap();
    a.tools.speech_status();
    assert!(!a.tools.agent_speech_busy(),"a question card is never spoken");
    assert_eq!(heimdall(&a),["Which branch?","Build is green.","Install the new build?","Push it too?"]);
    assert_eq!(window(&a),None,"a silent question must not open the reply window");
    // A final reply that repeats what this turn already said says nothing more (agent::reply).
    a.tools.agent_final_answer("Build is green.","stop").unwrap();
    assert_eq!(heimdall(&a),["Which branch?","Build is green.","Install the new build?","Push it too?"]);
    // A silent reply is not something Heimdall said.
    for silent in ["NO_REPLY","No request."] { a.tools.agent_final_answer(silent,"stop").unwrap(); }
    assert_eq!(heimdall(&a).len(),4);
    rt.block_on(server.shutdown());
}

/// Queued speech that is never heard (Speaking switched off mid-line, or synthesis failed) is shown once as the
/// island line and enters the decider's conversation; the user talking over a line stops it and shows nothing.
#[test]
fn unheard_speech_falls_back_to_one_island_line_and_a_heimdall_turn() {
    let (_t,a,_)=rig(true);
    let speech=Speech::install(&a);
    a.tools.mcp_actions.attach(ui::Ui::handle(),Arc::new(|_,_,_|Ok(())),Arc::new(|_,_|Ok(())),Arc::new(|_,_|Ok(()))).unwrap();
    let notices=island_notices;
    let heimdall=|a:&Pipeline|a.tools.send_context.lock().unwrap().snapshot().turns.iter().filter(|t|t.speaker=="Heimdall").map(|t|t.text.clone()).collect::<Vec<_>>();
    let settle=|a:&Pipeline,done:&dyn Fn(&Pipeline)->bool| {
        let began=Instant::now();
        while !done(a) && began.elapsed()<Duration::from_secs(3) {a.tools.speech_status();std::thread::sleep(Duration::from_millis(20));}
    };
    // The user talks over it (Speaking still on): stopped on purpose; even if Speaking goes off before the
    // cancellation is drained, nothing is shown for it.
    speech.say(&a,"hold barged");
    a.tools.interrupt();speech.cancelled("hold barged");
    // Speaking switched off mid-line: the agent was told it was queued, so it is shown, once.
    speech.say(&a,"hold until off");
    a.tools.enabled.store(false,Ordering::Release);a.tools.interrupt();speech.cancelled("hold until off");
    settle(&a,&|a|!notices(a).is_empty());
    std::thread::sleep(Duration::from_millis(200));a.tools.speech_status();
    let n=notices(&a);
    assert_eq!(n.len(),1,"{n:?}");assert!(n[0].contains("hold until off"),"{n:?}");
    assert_eq!(heimdall(&a),["hold until off"]);
    assert_eq!(a.tools.send_context.lock().unwrap().finished_ns,None,"unheard text never opens the reply window");
}

#[test]
fn restart_restores_pending_speech_and_recent_turns_before_the_next_decision() {
    let (_temp,mut a,mic)=rig(true);
    turn(&mut a,&mic,10*S,"Move the screenshots",Detect::Final,0,Some(0.),0,|_|{});
    a.agent.as_ref().unwrap().barrier().unwrap();
    let original=a.tools.send_context.lock().unwrap().snapshot().pending[0].observation.clone().unwrap();
    // A prior final answer belongs to the same restored conversation.
    {
        let mut context=a.tools.send_context.lock().unwrap();
        context.append_at("Heimdall","Which destination?",false,11*S);
        a.tools.persist_send_context(&context).unwrap();
    }
    let data=a.data.clone();let models=a.models.clone();let mut settings=a.settings.clone();settings.dry_run=true;
    a.finish().unwrap();drop(a);
    let mut restarted=Pipeline::new(data,models,settings,None,true).unwrap();
    let snapshot=restarted.tools.send_context.lock().unwrap().snapshot();
    assert_eq!(snapshot.pending.len(),1);
    assert_eq!(snapshot.pending[0].text,"Move the screenshots");
    assert!(snapshot.turns.iter().any(|t|t.speaker=="Heimdall" && t.text=="Which destination?"));
    assert_eq!(snapshot.pending[0].observation.as_ref(),Some(&original));
    restarted.source(crate::fake_source("mic",0,1));
    turn(&mut restarted,&mic,20*S,"into yesterday's folder",Detect::Final,0,Some(1.),0,|_|{});
    restarted.agent.as_ref().unwrap().barrier().unwrap();
    let r=records(&restarted);
    let decision=r.iter().find(|r|r.kind=="send_decision").unwrap();
    let prompt=decision.payload["prompt"]["state"].as_str().unwrap();
    assert!(prompt.contains("Move the screenshots") && prompt.contains("into yesterday's folder") && prompt.contains("Which destination?"));
    // The decider's context is restored (above); the speech pending before the restart is history, never part of
    // the first wake after a start (E4.4, E4.5, root cause 3): the archive keeps it and a live start's catch-up
    // carries it.
    let log=std::fs::read_to_string(crate::archive::run_dir(&restarted.data,&restarted.session).join("dry-run.jsonl")).unwrap();
    assert!(log.contains("yesterday's folder") && !log.contains("Move the screenshots"),"{log}");
}

#[test]
fn failed_synthesis_does_not_arm_the_follow_up_trigger() {
    struct Fails;
    impl speak::Synthesizer for Fails {fn synthesize(&mut self,_:&str,_:&str,_:&dyn Fn()->bool)->Result<speak::Audio> {anyhow::bail!("injected synthesis failure")}}
    struct Output;
    impl speak::AudioOutput for Output {fn output(&mut self,_:&speak::Audio,_:&dyn Fn()->bool)->Result<()> {panic!("no audio should reach output")}}
    let (_t,a,_)=rig(true);a.tools.test_guarded_speech_queue(Fails,Output);
    let permit=a.tools.agent_enabled.permit().unwrap();
    a.tools.mcp_actions.attach(ui::Ui::handle(),Arc::new(|_,_,_|Ok(())),Arc::new(|_,_|Ok(())),Arc::new(|_,_|Ok(()))).unwrap();
    agent::mcp::ToolHost::speak(a.tools.as_ref(),"Never spoken.","en",&permit).unwrap();
    a.tools.test_finish_speech().unwrap();a.tools.speech_status();
    assert!(a.tools.send_context.lock().unwrap().finished_ns.is_none());
    // The user still gets the line: shown on the island, and the decider has it as a Heimdall turn.
    let db=rusqlite::Connection::open(a.data.join("agent/mcp-actions.sqlite")).unwrap();
    let shown:Vec<String>=db.prepare("SELECT body FROM receipts WHERE kind='notice'").unwrap().query_map([],|r|r.get(0)).unwrap().map(Result::unwrap).collect();
    assert!(shown.len()==1 && shown[0].contains("Never spoken."),"{shown:?}");
    assert!(a.tools.send_context.lock().unwrap().snapshot().turns.iter().any(|t|t.speaker=="Heimdall"&&t.text=="Never spoken."));
}

#[test]
fn restore_reads_only_the_latest_checkpoint_without_materializing_history() {
    let (_t,a,_)=rig(true);
    let root=a.data.join("archive");
    // An unrelated older chunk is unreadable: a bounded latest lookup has no
    // reason to open it. The original session scan does, even with a kind filter.
    let old=root.join("session-old-history");
    let j=heimdall_archive::Journal::open(&old).unwrap();
    j.event("unrelated","test",&[],1,serde_json::json!({"old":"history"})).unwrap();
    let db=heimdall_archive::store::open(&root).unwrap();
    while heimdall_archive::chunks::pack_evidence(&db,&heimdall_archive::live::Queue::shared(&root).unwrap(),&root,heimdall_archive::wall_ns()+1).unwrap()>0 {}
    db.execute("UPDATE cold_chunks SET sha256=?1",["0".repeat(64)]).unwrap();
    let mut context=decide::send::Conversation::default();context.append("User","restore this pending request",true);
    a.tools.persist_send_context(&context).unwrap();
    a.tools.restore_send_context().unwrap();
    assert!(a.tools.send_context.lock().unwrap().pending_text().contains("restore this pending request"));
    // Compaction must keep precisely the latest send checkpoint available.
    while heimdall_archive::chunks::pack_evidence(&db,&heimdall_archive::live::Queue::shared(&root).unwrap(),&root,heimdall_archive::wall_ns()+1).unwrap()>0 {}
    assert_eq!(db.query_row("SELECT count(*) FROM evidence WHERE kind='send_context'",[],|r|r.get::<_,i64>(0)).unwrap(),1);
    a.tools.restore_send_context().unwrap();
}

#[test]
fn restored_conversation_keeps_old_turns_before_new_speech_after_reboot() {
    let (_t,a,_)=rig(true);
    {let mut c=a.tools.send_context.lock().unwrap();c.append_at("User","old boot request",false,900_000_000_000);c.append_at("Heimdall","old boot answer",false,901_000_000_000);a.tools.persist_send_context(&c).unwrap();}
    a.tools.restore_send_context().unwrap();
    let mut c=a.tools.send_context.lock().unwrap();c.append_at("User","new boot request",true,1_000_000_000);
    let snapshot=c.snapshot();assert_eq!(snapshot.turns.iter().map(|t|t.text.as_str()).collect::<Vec<_>>(),["old boot request","old boot answer","new boot request"]);
    c.trim_history(&snapshot,2);assert_eq!(c.snapshot().turns[0].text,"new boot request");
}

#[test]
fn explicit_name_sends_from_text_when_the_waveform_is_unavailable() {
    let (_t,mut a,mic)=rig(true);
    let source=mic.clone();
    turn(&mut a,&mic,10*S,"Heimdall check the build",Detect::Final,0,Some(0.),0,move|a|{a.audio.remove(&source);});
    a.agent.as_ref().unwrap().barrier().unwrap();
    let records=records(&a);
    assert!(records.iter().any(|r|r.kind=="agent_wake"));
    let send=records.iter().find(|r|r.kind=="send_decision").unwrap();
    assert_eq!(send.payload["trigger"],"name");assert!(send.payload["audio"]["unavailable"].is_string());
    let rows=a.index.utterances_between(10*S,20*S).unwrap();
    assert_eq!(rows[0]["decision"]["trigger"],"name","late transcript indexing erased a completed send answer");
    assert!(a.early_sends.is_empty());
}

/// Drawing mode's Send ("decider doesn't need to know … when he decides to send it to the agent, then
/// only then the images are actually attached"): the marked screenshot joins the pending utterance without a wake
/// and without entering the decider's conversation; the wake the decider sends carries it as an image, once.
#[test]
fn marked_screenshot_waits_for_the_decider_send_and_rides_once() {
    for agent_on in [true, false] {
        let label = if agent_on { "Agent on" } else { "Agent off" };
        let (_t, mut a, mic) = rig(agent_on);
        // A marked screenshot whose first pixel tags it: (9, 9).
        let png = ui::drawing::encode_png(2, 1, &[9, 9, 7, 9, 9, 7]).unwrap();
        a.action(ui::UiAction::DrawingSent(ui::drawing::MarkedShot { png, width: 2, height: 1, display: 1, strokes: 3 })).unwrap();
        let r = records(&a);
        assert_eq!(a.attachment_batch.len(), 1, "{label}: attached to the pending utterance");
        assert_eq!(count(&r, "marked_screenshot"), 1, "{label}: archived");
        assert_eq!(count(&r, "agent_send"), 0, "{label}: Send never wakes the agent");
        assert!(a.tools.send_context.lock().unwrap().snapshot().pending.is_empty(), "{label}: the decider is told nothing");
        // The user talks; the decider holds the first sentence, then sends.
        turn(&mut a, &mic, 2 * S, "hmm hold on", Detect::Final, 0, Some(0.), S / 2, |_| {});
        assert_eq!(a.attachment_batch.len(), 1, "{label}: still waiting while the decider waits");
        let context = serde_json::to_string(&a.tools.send_context.lock().unwrap().snapshot().pending.iter().map(|t| t.text.clone()).collect::<Vec<_>>()).unwrap();
        assert!(!context.contains("screenshot") && !context.contains("image"), "{label}: the decider saw the image: {context}");
        turn(&mut a, &mic, 6 * S, "what is wrong with this chart", Detect::Final, 0, Some(1.), 3 * S / 2, |_| {});
        assert!(a.attachment_batch.is_empty(), "{label}: the decider's send took it");
        a.test_agent().barrier().unwrap();
        wait_for(&a, "first completion", |r| completions(r) >= 1);
        turn(&mut a, &mic, 12 * S, "thanks that is all", Detect::Final, 0, Some(1.), 3 * S / 2, |_| {});
        a.test_agent().barrier().unwrap();
        let r = wait_for(&a, "second completion", |r| completions(r) >= 2);
        let wakes: Vec<_> = r.iter().filter(|r| r.kind == "agent_send" && r.payload["query"] == true).collect();
        assert_eq!(wakes.len(), 2, "{label}");
        let content = |w: &heimdall_archive::Record| w.payload["payload"]["message"]["content"].as_array().unwrap().clone();
        let first = content(wakes[0]);
        let images: Vec<_> = first[1..].iter().map(|b| png_tag(&a.index.evidence.image_bytes(b["source"]["archive_image_id"].as_str().unwrap()).unwrap().1)).collect();
        assert_eq!(images, [(9, 9)], "{label}: the marked screenshot rides on the decider's send as an image");
        assert!(!first[0]["text"].as_str().unwrap().contains("screenshot"), "{label}: no note about it in the text");
        assert_eq!(content(wakes[1]).len(), 1, "{label}: sent once");
    }
}

/// The capture filters find every window Heimdall leaves out of screenshots, and the agent is told the pointing
/// commands exactly as the UI parses them.
#[test]
fn drawing_windows_are_excluded_and_the_point_guide_matches() {
    assert_eq!(screen::EXCLUDED_WINDOW_TITLES, &[ui::ISLAND_WINDOW_TITLE, ui::drawing::OVERLAY_WINDOW_TITLE, ui::drawing::POINTER_WINDOW_TITLE, ui::drop::WINDOW_TITLE]);
    assert_eq!(agent::mcp_contract::POINT_GUIDE, ui::drawing::AGENT_GUIDE);
}

/// Show Islands on Streams: off by default and out of the file; the menu's switch is saved and shown, and
/// Heimdall's own capture filters keep the island out of its screenshots either way.
#[test]
fn islands_on_streams_is_off_by_default_saved_and_shown() {
    let (_t, mut a, _mic) = rig(false);
    a.test_persist_settings();
    assert!(!a.settings.islands_on_streams && !a.ui_settings_value().islands_on_streams);
    a.action(ui::UiAction::SetIslandsOnStreams(true)).unwrap();
    assert!(a.settings.islands_on_streams && a.ui_settings_value().islands_on_streams);
    assert!(Settings::load(&a.data.join("settings.json")).unwrap().islands_on_streams, "saved");
    a.action(ui::UiAction::SetIslandsOnStreams(false)).unwrap();
    assert!(!a.ui_settings_value().islands_on_streams);
    assert!(!std::fs::read_to_string(a.data.join("settings.json")).unwrap().contains("islands_on_streams"), "off stays out of the file");
    let old: Settings = serde_json::from_str(r#"{"speak_replies":true}"#).unwrap();
    assert!(!old.islands_on_streams, "a file from before the switch: off");
    assert!(screen::EXCLUDED_WINDOW_TITLES.contains(&ui::ISLAND_WINDOW_TITLE));
}

/// `look` with Seeing off answers in words and captures nothing; `point` refuses a mistake with its line.
#[tokio::test]
async fn look_respects_seeing_and_point_mistakes_come_back() {
    let (_t, a, _mic) = rig(false);
    let gate = std::sync::Arc::new(agent::delivery::DeliveryGate::new(true));
    let permit = gate.permit().unwrap();
    a.tools.screen_off.store(true, std::sync::atomic::Ordering::Release);
    let seen = a.tools.look(None, &permit).await.unwrap();
    assert_eq!(seen["content"][0]["text"], crate::bridge::SCREEN_OFF);
    assert!(seen["content"].as_array().unwrap().iter().all(|c| c["type"] == "text"));
    // Pointing needs the UI's main thread; without one it says so rather than hanging.
    let err = crate::drawing::point(&a.tools, &serde_json::json!({"spec":"size 10 10\nbox 1 1 2 2"}), &permit).unwrap_err();
    assert!(err.to_string().contains("UI_UNAVAILABLE"), "{err}");
}
