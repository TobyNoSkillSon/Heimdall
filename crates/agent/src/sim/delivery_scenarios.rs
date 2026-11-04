//! Night pass 3 (21 Oct), delivery part: the user's words reach the agent once and get an answer across compactions,
//! reloads and restarts.
//! - Bug note 21 Oct 00:10:14: one line heard on two microphones woke the agent twice (two deliveries in the same
//!   second, the second restarting the turn with the line twice). One wake per utterance.
//! - Bug note 20 Oct 22:12:57: "Heimdall, you there?" said while a reload or restart was pending never got an answer:
//!   every start turned everything queued into history ("Don't do anything unless the user talks to you"). His request
//!   from moments before the start is still a request.
use super::*;

fn catch_up(sim: &Sim) -> String {
    sim.appends().last().expect("a start context").text.clone()
}
fn carries(input: &Input, words: &str) -> bool {
    input.text.contains(&format!("User] {words}"))
}
/// Index of the last input carrying `words` and of the last start context, in the order Pi received them.
fn order(sim: &Sim, words: &str) -> (Option<usize>, Option<usize>) {
    let inputs = sim.inputs();
    (inputs.iter().rposition(|i| i.kind != InputKind::Append && carries(i, words)), inputs.iter().rposition(|i| i.kind == InputKind::Append))
}

/// D1, 21 Oct 00:10:05: the same line from the Shure and the AirPods, 173 ms apart, each with its own wake. One
/// delivery, no restart of the turn; the copy keeps its reason; G1 holds (the line reached the model once).
#[test]
fn d1_one_line_on_two_microphones_is_one_wake() {
    let mut sim = Sim::new(EVENING_MS);
    sim.advance(MIN);
    let line = "Okay, I have both enabled right now.";
    let before = sim.wakes().len();
    let first = sim.say(line);
    sim.advance(173);
    let copy = sim.say(line);
    let wakes: Vec<_> = sim.wakes().into_iter().skip(before).collect();
    assert_eq!(wakes.len(), 1, "one delivery: {wakes:#?}");
    assert!(carries(&wakes[0], line));
    assert_eq!(sim.dropped(&first), None);
    assert!(sim.dropped(&copy).is_some_and(|r| r.contains("another microphone")), "{:?}", sim.dropped(&copy));
    sim.settle();
    assert_eq!(sim.turns_between(0, sim.now() + 1), 1, "one turn, one answer");
    sim.check_global();
    // The same words seconds later are the user speaking again: they go out (G1 counts by words, so it is not rechecked).
    sim.advance(10 * S);
    sim.say(line);
    assert_eq!(sim.wakes().len(), before + 2);
}

/// D2: idempotent per utterance. A wake repeated for an utterance whose words are already out (the same observation
/// observed and woken twice, while its turn runs or after it) starts no second delivery; a wake that brings an image
/// still goes out.
#[test]
fn d2_a_repeated_wake_for_the_same_utterance_starts_nothing() {
    let mut sim = Sim::new(EVENING_MS);
    sim.world.lock().unwrap().replies.push_back(vec![Step::Think(5_000), Step::Final("Here.")]);
    let id = sim.say("Heimdall, are you there?");
    let before = sim.wakes().len();
    let o = crate::protocol::Observation { id: id.clone(), timestamp_ms: sim.now(), source: "mic:primary".into(), speaker: "User".into(), language: "en".into(), text: "Heimdall, are you there?".into() };
    sim.rt.block_on(sim.bridge.observe(o.clone())).unwrap();
    let again = sim.rt.block_on(sim.bridge.wake_keyed_through(None, "Act on User's latest request.", &[], true, None, Some(&id), crate::Speaker::Owner)).unwrap();
    sim.pump();
    assert_eq!(again, None);
    assert_eq!(sim.wakes().len(), before, "no second delivery while the turn runs");
    sim.settle();
    let again = sim.rt.block_on(sim.bridge.wake_keyed_through(None, "Act on User's latest request.", &[], true, None, Some(&id), crate::Speaker::Owner)).unwrap();
    sim.pump();
    assert_eq!(again, None);
    assert_eq!(sim.wakes().len(), before, "nor after it answered");
    let image = crate::protocol::Attachment { media_type: "image/png".into(), bytes: vec![0x89, b'P', b'N', b'G'] };
    let with_image = sim.rt.block_on(sim.bridge.wake_keyed_through(None, "Act on User's latest request.", &[image], true, None, Some(&id), crate::Speaker::Owner)).unwrap();
    sim.pump();
    assert!(with_image.is_some(), "an image is something new");
    sim.settle();
    sim.check_global();
}

/// D8, night 3 review: a start begins between the user's line being observed and its wake (the bridge handles them as two
/// commands; a reload or crash recovery can run between). The start's catch-up claims the line, so the wake finds no
/// line of its own; it must still go out after the start context and start a turn that answers him. Both orders: the
/// wake while the start context is still waiting, and after it went in.
#[test]
fn d8_a_start_between_observe_and_wake_still_answers_owner() {
    for context_first in [false, true] {
        let mut sim = Sim::new(EVENING_MS);
        sim.say("hello");
        sim.settle();
        sim.advance(MIN);
        sim.world.lock().unwrap().replies.push_back(vec![Step::Think(1_000), Step::Final("Yes, I'm here.")]);
        let words = "Heimdall, you there?";
        let o = sim.observation("User", words);
        let id = o.id.clone();
        sim.said.push((id.clone(), sim.now(), words.into(), true));
        sim.rt.block_on(sim.bridge.observe(o)).unwrap();
        // The restart, as `Sim::restart`, with the start context delivered before the wake or after it.
        sim.world.lock().unwrap().restart();
        sim.replies.reset();
        sim.bridge = Sim::open_bridge(&sim.world, &sim.spool.clone(), &sim.clock);
        let evidence = sim.world.lock().unwrap().evidence();
        sim.bridge.recover_from(evidence).unwrap();
        sim.begin_start(Some("Reloaded: Session storage maintenance".into()));
        if context_first { sim.pump(); }
        sim.rt.block_on(sim.bridge.wake_keyed_through(None, "Act on User's latest request.", &[], true, None, Some(&id), crate::Speaker::Owner)).unwrap();
        sim.pump();
        sim.settle();
        let inputs = sim.inputs();
        let start = inputs.iter().rposition(|i| i.kind == InputKind::Append).expect("a start context");
        assert!(inputs[start].text.contains(words), "context_first {context_first}: his line is in the catch-up");
        assert!(inputs[start + 1..].iter().any(|i| i.kind != InputKind::Append), "context_first {context_first}: no wake after the start:\n{inputs:#?}");
        assert!(sim.heard_texts().iter().any(|t| t == "Yes, I'm here."), "context_first {context_first}: he gets no answer: {:?}", sim.heard_texts());
        assert_eq!(sim.turns_between(sim.started_at, sim.now() + 1), 1, "context_first {context_first}");
        sim.check_global();
    }
}

/// D3, the 20 Oct 22:12:57 note: a reload is pending (storage maintenance or the user's Reload, waiting for the turn to
/// settle); the user asks "Heimdall, you there?" meanwhile. His words wait, the agent restarts, the start context goes in
/// first, then his words go out as a wake and he gets an answer. They are not in the catch-up.
#[test]
fn d3_a_wake_during_a_pending_reload_is_answered_after_the_start() {
    let mut sim = Sim::new(EVENING_MS);
    sim.say("go through the YouTube home page and tell me what's recommended");
    sim.settle();
    sim.world.lock().unwrap().replies.push_back(vec![Step::Think(1_000), Step::Final("Yes, I'm here.")]);
    sim.advance(MIN);
    sim.bridge.status = crate::Status::ReloadPending;
    let wakes = sim.wakes().len();
    sim.say("Heimdall, you there?");
    assert_eq!(sim.wakes().len(), wakes, "nothing goes to a process about to be replaced");
    sim.advance(40 * S);
    sim.restart(Some("Reloaded: Session storage maintenance"));
    let text = catch_up(&sim);
    assert!(!text.contains("you there"), "a request, not history: {text}");
    assert!(text.ends_with(&crate::notice::restart_message(sim.now())), "{text}");
    let (wake, start) = order(&sim, "Heimdall, you there?");
    assert!(wake.zip(start).is_some_and(|(w, s)| w > s), "his words go out after the start context: {:#?}", sim.inputs());
    sim.settle();
    assert!(sim.heard_texts().iter().any(|t| t == "Yes, I'm here."), "he gets an answer: {:?}", sim.heard_texts());
    assert_eq!(sim.turns_between(sim.started_at, sim.now() + 1), 1);
    sim.check_global();
}

/// D4: the same through Pi's own compaction and a crash: a wake that waited for Pi's compaction, then the process
/// died and was replaced, still goes out after the start context. Typed text queued meanwhile goes out too, in order.
#[test]
fn d4_a_wake_waiting_for_a_compaction_survives_a_crash_and_is_answered() {
    let mut sim = Sim::new(EVENING_MS);
    sim.say("what's on my calendar?");
    sim.settle();
    sim.compaction();
    sim.say("Heimdall, also check the weather");
    sim.typed("and the traffic");
    assert!(sim.world.lock().unwrap().compacting());
    sim.world.lock().unwrap().crash();
    sim.advance(5 * S);
    sim.restart(None);
    let (wake, start) = order(&sim, "Heimdall, also check the weather");
    assert!(wake.zip(start).is_some_and(|(w, s)| w > s), "{:#?}", sim.inputs());
    let typed = sim.inputs().iter().rposition(|i| i.text.contains("and the traffic"));
    assert!(typed.zip(wake).is_some_and(|(t, w)| t > w), "typed text keeps its order: {:#?}", sim.inputs());
    assert!(!catch_up(&sim).contains("check the weather") && !catch_up(&sim).contains("and the traffic"));
    sim.settle();
    sim.check_global();
}

/// D5, worst cases that stay history (E4.4): a request queued more than `START_RESUME_MS` before the start, someone
/// else's line the decider sent, and a notice. They come back as context; no turn runs until the user speaks.
#[test]
fn d5_old_requests_other_speakers_and_notices_queued_before_a_start_stay_history() {
    let mut sim = Sim::new(EVENING_MS);
    sim.say("hello");
    sim.settle();
    sim.bridge.status = crate::Status::ReloadPending;
    sim.say("Heimdall, read me the news");
    sim.advance(crate::notice::START_RESUME_MS + MIN);
    sim.false_wake("Dave", "Heimdall is a funny name");
    sim.advance(10 * S);
    sim.restart(Some("Reloaded: User reloaded the agent"));
    let text = catch_up(&sim);
    assert!(text.contains("read me the news") && text.contains("funny name"), "{text}");
    assert_eq!(sim.turns_between(sim.started_at, sim.now() + 1), 0, "history only");
    assert!(sim.bridge.awaiting_owner(), "nothing of his was waiting");
    sim.settle();
    assert_eq!(sim.turns_between(sim.started_at, sim.now() + 1), 0);
    sim.check_global();
}

/// D6: a turn in progress when a reload is asked for finishes first (the supervisor reloads only once Pi is idle;
/// app `reload_waits_for_settlement_and_notice_uses_original_permit`), and what the user says meanwhile is answered after
/// the restart instead of steering a process about to be replaced.
#[test]
fn d6_the_turn_in_progress_finishes_and_words_said_meanwhile_are_answered_after_the_reload() {
    let mut sim = Sim::new(EVENING_MS);
    sim.world.lock().unwrap().replies.push_back(vec![Step::Think(20_000), Step::Final("The home page has three music videos.")]);
    sim.say("go through the YouTube home page");
    sim.advance(5 * S);
    sim.bridge.status = crate::Status::ReloadPending;
    sim.say("and open the first one");
    sim.settle();
    assert!(sim.heard_texts().iter().any(|t| t.contains("three music videos")), "{:?}", sim.heard_texts());
    sim.restart(Some("Reloaded: User reloaded the agent from the Agent window"));
    let (wake, start) = order(&sim, "and open the first one");
    assert!(wake.zip(start).is_some_and(|(w, s)| w > s), "{:#?}", sim.inputs());
    sim.settle();
    assert_eq!(sim.turns_between(sim.started_at, sim.now() + 1), 1);
    sim.check_global();
}

/// D7: seeded sweep of talk, two-microphone copies, pending reloads, compactions and restarts: every utterance
/// reaches the model at most once (G1/G2), and every request the user made within `START_RESUME_MS` of a start gets a
/// turn after it.
#[test]
fn d7_seeded_restarts_with_waiting_requests_and_microphone_copies() {
    let cases: u64 = std::env::var("HEIMDALL_SIM_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(200);
    for seed in 0..cases {
        let mut rng = seed.wrapping_mul(0x9E3779B97F4A7C15).wrapping_add(33);
        let mut next = |n: u64| { rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407); (rng >> 33) % n };
        let mut sim = Sim::new(EVENING_MS);
        let mut waiting: Vec<String> = vec![];
        for i in 0..6 + next(10) {
            sim.advance(next(3 * MIN));
            match next(6) {
                0 => { sim.say(&format!("request {seed}-{i}")); }
                1 => { let words = format!("copied {seed}-{i}"); sim.say(&words); sim.advance(next(1_000)); sim.say(&words); }
                2 => { if !sim.world.lock().unwrap().turn_running() { sim.compaction(); } }
                3 => {
                    if !sim.world.lock().unwrap().turn_running() {
                        sim.bridge.status = crate::Status::ReloadPending;
                        let words = format!("waiting {seed}-{i}");
                        sim.say(&words);
                        waiting.push(words);
                        sim.advance(next(60) * S);
                        sim.restart(Some("Reloaded: Session storage maintenance"));
                        for words in waiting.drain(..) {
                            let (wake, start) = order(&sim, &words);
                            assert!(wake.zip(start).is_some_and(|(w, s)| w > s), "seed {seed}: {words:?} was not answered after the start:\n{:#?}", sim.inputs());
                        }
                    }
                }
                4 => { sim.aside("Dave", &format!("aside {seed}-{i}")); }
                _ => { sim.settle(); sim.restart(None); }
            }
        }
        sim.settle();
        sim.check_global();
    }
}
