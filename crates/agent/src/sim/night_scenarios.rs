//! Night pass 19→20 Oct (docs/briefs/night-1020-issues.md): the user's words that arrive while a turn still thinks join
//! that turn (H2), and Heimdall's own compactions never sit in front of his request (H1). Worst cases: talk-over at
//! every phase of a turn, chunked speech, compaction and wake races, and seeded sweeps of both.
use super::*;

fn reply(sim: &Sim, steps: Vec<Step>) {
    sim.world.lock().unwrap().replies.push_back(steps);
}
fn finals(sim: &Sim) -> Vec<String> {
    sim.heard.iter().filter(|h| h.final_reply).map(|h| h.text.clone()).collect()
}
fn copies(sim: &Sim, words: &str) -> usize {
    sim.wakes().iter().filter(|i| i.text.contains(words)).count()
}
/// Opus 5.5 with the 19 Oct budget: idle mark 214,685 tokens (85%), cold-cache break-even 92,000.
fn opus_policy() -> crate::maintenance::CompactionPolicy {
    crate::maintenance::CompactionPolicy::for_limits(crate::provider::Limits::for_context(252_571, Some(128_000)), true)
}
fn with_policy(sim: &mut Sim, tokens: u64) {
    sim.bridge.set_compaction_policy(Some(opus_policy()));
    let mut w = sim.world.lock().unwrap();
    w.context_tokens = Some(tokens);
    w.window = 1_000_000;
    w.compaction_ms = 45_000;
}

// ---- H2: speech during a running turn ----

/// N1, the 18:39 replay: the user asks how such a website is called; three seconds later, while the first request still
/// thinks, he asks again. One answer for both, nothing said twice, each sentence reaches the model once, nothing is
/// reported as stopped.
#[test]
fn n1_a_question_repeated_while_the_turn_thinks_gets_one_answer() {
    let mut sim = Sim::new(EVENING_MS);
    reply(&sim, vec![Step::Think(25_000), Step::Final("Usually a dashboard or mission control.")]);
    reply(&sim, vec![Step::Think(25_000), Step::Final("That is a mission control or command center.")]);
    let first = "how could we call this kind of website?";
    let second = "What kind of websites do we call them?";
    sim.say(first);
    sim.advance(3 * S);
    let asked = sim.now();
    sim.say(second);
    let restart = sim.wakes().last().unwrap().clone();
    assert_eq!(restart.kind, InputKind::Prompt, "the turn restarts with the new words");
    assert_eq!(restart.at, asked, "at once, not after the request in flight");
    assert!(restart.text.contains(second) && !restart.text.contains(first), "the first words are in the conversation already: {}", restart.text);
    sim.settle();
    assert_eq!(finals(&sim), ["That is a mission control or command center."], "one answer");
    assert_eq!((copies(&sim, first), copies(&sim, second)), (1, 1));
    assert!(sim.resolutions.is_empty(), "nothing stopped or resent: {:?}", sim.resolutions);
    assert_eq!(sim.bridge.status, crate::Status::Idle);
    sim.check_global();
}

/// N2: once the turn has acted (a tool ran) or written its reply, new words steer it as designed (E2.1): taken at
/// the next tool boundary, or answered on their own after the written reply, which is still said.
#[test]
fn n2_after_a_tool_or_a_written_reply_words_steer() {
    let mut sim = Sim::new(EVENING_MS);
    reply(&sim, vec![Step::Think(500), Step::Tool("t3_worker_start", 1_000), Step::Think(3_000), Step::Final("The widget prototype is underway.")]);
    sim.say("yes, but keep it as a separate project");
    sim.advance(2 * S);
    sim.say("and the island gets too wide");
    assert_eq!(sim.wakes().last().unwrap().kind, InputKind::Steer);
    sim.settle();
    assert!(sim.world.lock().unwrap().rejoined.is_empty());
    assert_eq!(sim.turns().len(), 1);
    assert_eq!(finals(&sim).len(), 2, "the written reply, then the steer's: {:?}", finals(&sim));
    sim.check_global();
}

/// N3: chunks of one long thought, two seconds apart, while the model thinks five seconds per request. Three chunks:
/// the turn restarts with each and answers once, after the last. Four chunks: restarts stop at `MAX_RESTARTS`
/// (night review 4), so the fourth steers the thinking turn and gets its own answer after the first. Every chunk
/// reaches the model once either way.
#[test]
fn n3_chunked_speech_restarts_until_the_last_chunk_then_answers_once() {
    let chunks = ["In general we should rely more on workers", "and the orchestration rules for heavy work", "Opus 5.5 at medium or high reasoning", "and Haiku for the small ones"];
    for count in [3, 4] {
        let mut sim = Sim::new(EVENING_MS);
        sim.world.lock().unwrap().default_reply = vec![Step::Think(5_000), Step::Final("Noted: Haiku for small jobs, Opus for hard ones.")];
        sim.world.lock().unwrap().steer_reply = vec![Step::Think(800), Step::Final("And Haiku takes the small ones.")];
        for (i, chunk) in chunks[..count].iter().enumerate() {
            sim.say(chunk);
            if i + 1 < count { sim.advance(2 * S); }
        }
        let last = sim.now();
        sim.settle();
        assert_eq!(sim.world.lock().unwrap().rejoined.len(), (count - 1).min(crate::MAX_RESTARTS as usize));
        if count == 3 {
            assert_eq!(finals(&sim).len(), 1, "{:?}", finals(&sim));
            assert!(sim.heard[0].at >= last + 5 * S, "answered after the last chunk");
        } else {
            assert_eq!(sim.wakes().last().unwrap().kind, InputKind::Steer);
            assert_eq!(finals(&sim).len(), 2, "{:?}", finals(&sim));
        }
        for chunk in &chunks[..count] { assert_eq!(copies(&sim, chunk), 1, "{chunk}"); }
        sim.check_global();
    }
}

/// N4: talk-over at every phase of a turn (thinking, a `speak` line, a tool, the reply being written, the run
/// settling), every 250 ms: never two turns at once, nothing reaches the model twice, a tool never runs twice, and
/// words that arrive before the turn acted get the same single answer as the first words.
#[test]
fn n4_talk_over_at_every_phase() {
    for offset in (0..9_000).step_by(250) {
        let mut sim = Sim::new(EVENING_MS);
        reply(&sim, vec![Step::Think(1_500), Step::Speak("Looking.", 500), Step::Think(1_000), Step::Tool("bash", 2_000), Step::Think(2_000), Step::Final("The build is green.")]);
        sim.world.lock().unwrap().steer_reply = vec![Step::Think(800), Step::Final("Only the fast ones ran.")];
        sim.say("is the build green?");
        sim.advance(offset);
        let before_tool = offset < 1_500;
        sim.say("only the fast tests");
        sim.settle();
        let rejoined = sim.world.lock().unwrap().rejoined.len();
        assert_eq!(rejoined, usize::from(before_tool), "offset {offset}");
        let speaks = sim.effects().iter().filter(|e| e.tool == "speak").count();
        assert!(speaks <= 2, "offset {offset}: {speaks} speak calls");
        if before_tool {
            // The restarted turn (default reply) answers once.
            assert_eq!(finals(&sim).len(), 1, "offset {offset}: {:?}", finals(&sim));
        }
        assert!(finals(&sim).len() <= 2, "offset {offset}: {:?}", finals(&sim));
        sim.check_global();
    }
}

/// N5: Stop right after a restart behaves like any Stop (E3): the restarted turn aborts, nothing is resent.
#[test]
fn n5_stop_after_a_restart() {
    let mut sim = Sim::new(EVENING_MS);
    sim.world.lock().unwrap().default_reply = vec![Step::Think(10_000)];
    sim.say("think about the dashboard");
    sim.advance(S);
    sim.say("the live one");
    sim.advance(S);
    sim.stop();
    sim.settle();
    assert_eq!(sim.world.lock().unwrap().rejoined.len(), 1);
    assert!(sim.resolutions.iter().any(|r| r.outcome == crate::Resolved::Stopped));
    assert_eq!(sim.wakes().len(), 2, "no resend after Stop");
    assert!(finals(&sim).is_empty());
    assert_eq!(sim.bridge.status, crate::Status::Idle);
    sim.check_global();
}

/// N6: a notice that arrives while the turn thinks still waits for the restarted turn to settle (E2.2), and the user's
/// typed text restarts a thinking turn like speech does.
#[test]
fn n6_notices_wait_for_the_restarted_turn_and_typed_text_restarts() {
    let mut sim = Sim::new(EVENING_MS);
    sim.world.lock().unwrap().default_reply = vec![Step::Think(4_000), Step::Final("Done.")];
    sim.say("check the benchmark thread");
    sim.advance(S);
    sim.notice("terminal:w:r:completed", "Worker w run r completed.");
    sim.advance(S);
    sim.typed("and tell me its last line");
    let typed = sim.wakes().last().unwrap().clone();
    assert_eq!(typed.kind, InputKind::Prompt);
    assert!(sim.wakes().iter().all(|i| !i.text.contains("[Heimdall notice")), "the notice waits");
    sim.settle();
    let notice = sim.wakes().into_iter().find(|i| i.text.contains("[Heimdall notice")).expect("delivered after settling");
    assert_eq!(notice.kind, InputKind::Prompt);
    assert!(notice.at >= typed.at + 4 * S);
    sim.check_global();
}

/// N7: seeded sweep. Two to four utterances at random moments during scripted turns with random tools: G1/G2 hold,
/// and every restart happened while the turn had neither acted nor replied.
#[test]
fn n7_seeded_talk_over_sweep() {
    let cases: u64 = std::env::var("HEIMDALL_SIM_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(300);
    for seed in 0..cases {
        let mut rng = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let mut next = |n: u64| { rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407); (rng >> 33) % n };
        let mut sim = Sim::new(EVENING_MS);
        for _ in 0..4 {
            let mut steps = vec![Step::Think(500 + next(4_000))];
            for _ in 0..next(3) { steps.push(Step::Tool("bash", 200 + next(2_000))); steps.push(Step::Think(200 + next(2_000))); }
            steps.push(Step::Final("ok"));
            reply(&sim, steps);
        }
        for i in 0..2 + next(3) {
            sim.say(&format!("utterance {seed}-{i}"));
            sim.advance(next(6_000));
        }
        sim.settle();
        let rejoined = sim.world.lock().unwrap().rejoined.clone();
        for joined in rejoined {
            for id in joined {
                assert!(sim.effects().iter().all(|e| e.delivery != id), "seed {seed}: a delivery with a tool effect was restarted");
            }
        }
        sim.check_global();
    }
}

// ---- H1: compaction off the user's path ----

/// N10, the 18:12 replay: Agent on after 1 h 47 min off, 98K-token history on a cold cache; the user speaks 7 s later.
/// His words go out at once (no compaction in front of them); afterwards the cache is warm again, so no
/// compaction follows.
#[test]
fn n10_first_words_after_agent_on_never_wait_for_a_cold_cache_compaction() {
    let off_since = EVENING_MS - (107 * MIN);
    let mut sim = Sim::new(EVENING_MS);
    with_policy(&mut sim, 98_000);
    sim.world.lock().unwrap().last_request_at = Some(off_since);
    sim.advance(7 * S);
    let spoke = sim.now();
    sim.say("Heimdall, you done?");
    let wake = sim.wakes().last().unwrap().clone();
    assert_eq!(wake.at, spoke, "delivered at once");
    sim.settle();
    sim.advance(5 * MIN);
    let w = sim.world.lock().unwrap();
    assert!(w.compactions.is_empty() && w.compactions_started == 0, "the wake warmed the cache");
    drop(w);
    sim.check_global();
}

/// N11: Agent on with a cold cache and nobody talking: the cold-cache compaction starts after 30 s of quiet and
/// finishes; the next words go out at once.
#[test]
fn n11_cold_cache_at_agent_on_compacts_once_quiet() {
    let mut sim = Sim::new(EVENING_MS);
    with_policy(&mut sim, 98_000);
    sim.world.lock().unwrap().last_request_at = Some(EVENING_MS - 2 * 60 * MIN);
    sim.advance(29 * S);
    assert!(!sim.world.lock().unwrap().compacting(), "not before 30 s of quiet");
    sim.advance(2 * S);
    assert!(sim.world.lock().unwrap().compacting(), "cold cache, 98K > 92K break-even");
    sim.advance(50 * S);
    assert_eq!(sim.world.lock().unwrap().compactions.len(), 1);
    let spoke = sim.now();
    sim.say("hello");
    assert_eq!(sim.wakes().last().unwrap().at, spoke);
    sim.settle();
    sim.advance(10 * MIN);
    assert_eq!(sim.world.lock().unwrap().compactions.len(), 1, "no second compaction");
    sim.check_global();
}

/// N12, ("When the agent is compacting I should not be able to interrupt you … I've interrupted
/// your compaction twice, which is just wasting money"): history past the idle mark. After the turn and 30 s of quiet
/// the compaction starts; the user speaks 10 s into it and presses Stop: the compaction runs to its end, and his words go
/// out the moment it ends, after the summary. Pi's own threshold is never reached.
#[test]
fn n12_owner_never_cuts_an_idle_compaction_and_is_answered_as_it_ends() {
    let mut sim = Sim::new(EVENING_MS);
    with_policy(&mut sim, 220_000);
    sim.say("open the dashboard");
    sim.settle();
    let settled = sim.heard.last().unwrap().at;
    sim.advance(settled + 29 * S - sim.now());
    assert!(!sim.world.lock().unwrap().compacting(), "not within 30 s of the turn");
    sim.advance(2 * S);
    assert!(sim.world.lock().unwrap().idle_compacting(), "idle, quiet, past the 215K mark");
    sim.advance(10 * S);
    let before = sim.wakes().len();
    sim.say("make it darker");
    sim.stop();
    sim.typed("and bigger");
    assert_eq!(sim.wakes().len(), before, "queued behind the compaction");
    assert!(sim.world.lock().unwrap().idle_compacting(), "neither his words nor his Stop cut it");
    sim.advance(40 * S);
    let w = sim.world.lock().unwrap();
    assert_eq!((w.compactions_started, w.compactions.len()), (1, 1), "it finished");
    let (_, ended) = w.compactions[0];
    drop(w);
    let wake = sim.wakes().last().unwrap().clone();
    assert_eq!(wake.at, ended, "his words go out as it ends");
    assert!(wake.text.contains("and bigger"), "{}", wake.text);
    sim.settle();
    sim.check_global();
}

/// N12b: the user's Stop while Pi compacts inside a turn (its own threshold) does not cut the summary either: the turn
/// stops as the compaction ends.
#[test]
fn n12b_stop_during_a_turns_compaction_stops_the_turn_once_it_ends() {
    let mut sim = Sim::new(EVENING_MS);
    sim.world.lock().unwrap().compaction_ms = 45_000;
    reply(&sim, vec![Step::Think(120_000), Step::Final("done")]);
    sim.say("summarise the week");
    assert!(sim.world.lock().unwrap().turn_running());
    sim.compaction();
    sim.advance(5 * S);
    sim.stop();
    assert!(sim.world.lock().unwrap().compacting(), "the Stop waits for the compaction");
    assert!(sim.world.lock().unwrap().turn_running());
    sim.advance(41 * S);
    let w = sim.world.lock().unwrap();
    assert_eq!((w.compactions_started, w.compactions.len()), (1, 1), "it finished");
    assert!(!w.turn_running(), "the turn stopped as it ended");
    drop(w);
    sim.settle();
    sim.check_global();
}

/// N13: the user away with the agent on. The cache-refresh pings keep the cache warm for 3 hours (day pass 26 Oct); the
/// last one goes out at 165 minutes, so the cache goes cold an hour later and the history is compacted while idle.
/// His next words neither wait for a summary nor re-send the whole history cold.
#[test]
fn n13_hours_away_compacts_once_the_pings_stop() {
    let mut sim = Sim::new(EVENING_MS);
    with_policy(&mut sim, 120_000);
    sim.world.lock().unwrap().default_reply = vec![Step::Think(2_000), Step::Final("NO_REPLY")];
    sim.say("thanks");
    sim.settle();
    let real = sim.world.lock().unwrap().last_request_at.unwrap();
    while sim.now() < real + 3 * 60 * MIN { sim.advance(MIN); }
    assert!(!sim.world.lock().unwrap().compacting() && sim.world.lock().unwrap().compactions.is_empty(), "the pings kept it warm");
    let last = sim.world.lock().unwrap().last_request_at.unwrap();
    assert!(last >= real + 165 * MIN, "the last ping went out at 165 minutes");
    sim.advance(last + 59 * MIN - sim.now());
    assert!(!sim.world.lock().unwrap().compacting(), "the cache is still warm");
    sim.advance(2 * MIN);
    assert!(sim.world.lock().unwrap().compacting());
    sim.advance(MIN);
    assert_eq!(sim.world.lock().unwrap().compactions.len(), 1);
    assert!(sim.world.lock().unwrap().compactions[0].0 >= last + 60 * MIN);
    let spoke = sim.now();
    sim.say("I'm back");
    assert_eq!(sim.wakes().last().unwrap().at, spoke);
    sim.settle();
    sim.check_global();
}

/// N14: a start whose catch-up must be compacted first (E4.2) and the user's words during that compaction: the
/// compaction finishes, the restart message goes in before his words, and they go out as soon as it is in.
#[test]
fn n14_wake_during_a_start_compaction_follows_the_restart_message() {
    let mut sim = Sim::new(EVENING_MS);
    sim.say("hello");
    sim.settle();
    sim.world.lock().unwrap().context_tokens = Some(74_000);
    sim.aside("Dave", &"we were talking about the dashboard layout ".repeat(120));
    sim.advance(MIN);
    sim.restart(None);
    assert!(sim.world.lock().unwrap().idle_compacting(), "the start compacts first (catch-up past the threshold)");
    sim.advance(3 * S);
    let spoke = sim.now();
    sim.say("what did Dave say?");
    assert!(sim.world.lock().unwrap().idle_compacting(), "his words do not cut it");
    sim.advance(2 * MIN);
    let inputs = sim.inputs();
    let restart = inputs.iter().rposition(|i| i.kind == InputKind::Append && i.text.contains("you have just been restarted")).expect("restart message");
    let wake = inputs.iter().rposition(|i| i.kind != InputKind::Append).unwrap();
    assert!(restart < wake, "restart message first");
    assert!(inputs[wake].at > spoke, "waited for the compaction");
    let w = sim.world.lock().unwrap();
    assert_eq!(w.compactions.len(), w.compactions_started, "nothing cut it");
    assert_eq!(inputs[wake].at, w.compactions.last().unwrap().1, "out as it ends");
    drop(w);
    sim.settle();
    sim.check_global();
}

/// N15: Pi's own threshold compaction (inside a turn) is not Heimdall's to cancel: a wake still waits for it and goes
/// out once at its end (E5.6). Typed text during Heimdall's idle compaction waits for it the same way.
#[test]
fn n15_pis_own_compaction_is_waited_for_and_heimdalls_is_not() {
    let mut sim = Sim::new(EVENING_MS);
    sim.compaction();
    sim.advance(3 * S);
    sim.say("look at this");
    assert!(sim.wakes().is_empty(), "Pi's own compaction: queued");
    sim.settle();
    assert_eq!(sim.wakes().len(), 1);
    let mut sim = Sim::new(EVENING_MS);
    with_policy(&mut sim, 220_000);
    sim.typed("hi");
    sim.settle();
    sim.advance(31 * S);
    assert!(sim.world.lock().unwrap().idle_compacting());
    let before = sim.wakes().len();
    sim.typed("and this one");
    assert_eq!(sim.wakes().len(), before, "typed text waits too");
    sim.advance(45 * S);
    assert_eq!(sim.world.lock().unwrap().compactions.len(), 1);
    assert!(sim.wakes().last().unwrap().text.contains("and this one"));
    sim.check_global();
}

/// N16: seeded sweep of compaction and wake races: Heimdall's compactions start at random idle moments, the user speaks
/// at random times. Words outside a compaction or turn go out at once, every compaction finishes, and G1/G2 hold.
#[test]
fn n16_seeded_compaction_wake_races() {
    let cases: u64 = std::env::var("HEIMDALL_SIM_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(200);
    for seed in 0..cases {
        let mut rng = seed.wrapping_mul(2862933555777941757).wrapping_add(3037000493);
        let mut next = |n: u64| { rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407); (rng >> 33) % n };
        let mut sim = Sim::new(EVENING_MS);
        with_policy(&mut sim, 180_000 + next(60_000));
        if next(2) == 0 { sim.world.lock().unwrap().last_request_at = Some(EVENING_MS - 2 * 60 * MIN); }
        for i in 0..3 + next(4) {
            sim.advance(next(120_000));
            let spoke = sim.now();
            let (compacting, busy) = { let w = sim.world.lock().unwrap(); (w.compacting(), w.turn_running()) };
            let before = sim.wakes().len();
            sim.say(&format!("words {seed}-{i}"));
            if !compacting && !busy {
                let wakes = sim.wakes();
                assert!(wakes.len() > before && wakes.last().unwrap().at == spoke, "seed {seed}: words {i} waited");
            }
            // Pi grows the history as the user talks.
            let mut w = sim.world.lock().unwrap();
            w.context_tokens = w.context_tokens.map(|t| t + next(8_000));
        }
        sim.settle();
        let w = sim.world.lock().unwrap();
        assert_eq!(w.compactions.len(), w.compactions_started, "seed {seed}: a compaction was cut");
        drop(w);
        sim.check_global();
    }
}

// ---- Night review 20 Oct (docs/briefs/night-1020-review.md, harness items 4 and 9) ----

/// R4a: a restart never cuts a reply or a tool call being written. The user's second sentence arrives every 250 ms
/// across a turn that thinks 2 s, then writes its reply for 3 s; a second turn that thinks 1.5 s, writes a tool call
/// for 0.5 s, then runs it. The turn restarts only while it thinks; from the first written token his words steer, the
/// written reply is still said, and the tool runs exactly once.
#[test]
fn r4a_restart_never_cuts_a_reply_or_tool_call_being_written() {
    for (script, writing_from) in [
        (vec![Step::Think(2_000), Step::Stream(3_000), Step::Final("The build is green.")], 2_000),
        (vec![Step::Think(1_500), Step::Stream(500), Step::Tool("bash", 2_000), Step::Think(1_000), Step::Final("Ran the fast tests.")], 1_500),
    ] {
        for offset in (0..5_000).step_by(250) {
            let mut sim = Sim::new(EVENING_MS);
            reply(&sim, script.clone());
            sim.world.lock().unwrap().steer_reply = vec![Step::Think(800), Step::Final("Only the fast ones.")];
            sim.say("is the build green?");
            sim.advance(offset);
            sim.say("only the fast tests");
            sim.settle();
            let restarted = !sim.world.lock().unwrap().rejoined.is_empty();
            assert_eq!(restarted, offset < writing_from, "offset {offset}");
            if !restarted {
                assert_eq!(sim.wakes().last().unwrap().kind, InputKind::Steer, "offset {offset}");
                let first = script.iter().find_map(|s| match s { Step::Final(t) => Some(*t), _ => None }).unwrap();
                assert!(finals(&sim).iter().any(|f| f == first), "offset {offset}: the written reply is said: {:?}", finals(&sim));
            }
            assert!(sim.effects().iter().filter(|e| e.tool == "bash").count() <= 1, "offset {offset}");
            sim.check_global();
        }
    }
}

/// R4b: restarts are bounded per turn (`MAX_RESTARTS`): six sentences one second apart into a turn that thinks 10 s
/// restart it twice; the rest steer, every sentence reaches the model once, and the turn answers. The next turn may
/// restart again.
#[test]
fn r4b_restarts_are_bounded_per_turn_and_count_again_for_the_next() {
    let mut sim = Sim::new(EVENING_MS);
    sim.world.lock().unwrap().default_reply = vec![Step::Think(10_000), Step::Final("Got all of it.")];
    sim.world.lock().unwrap().steer_reply = vec![Step::Think(500), Step::Final("And the rest.")];
    let words: Vec<String> = (0..6).map(|i| format!("sentence number {i}")).collect();
    for w in &words {
        sim.say(w);
        sim.advance(S);
    }
    sim.settle();
    assert_eq!(sim.world.lock().unwrap().rejoined.len(), crate::MAX_RESTARTS as usize);
    for w in &words { assert_eq!(copies(&sim, w), 1, "{w}"); }
    assert!(!finals(&sim).is_empty());
    sim.advance(MIN);
    sim.say("a new question");
    sim.advance(S);
    sim.say("said again");
    sim.settle();
    assert_eq!(sim.world.lock().unwrap().rejoined.len(), crate::MAX_RESTARTS as usize + 1, "a new turn restarts again");
    assert_eq!(sim.world.lock().unwrap().most_restarts_in_row, crate::MAX_RESTARTS as usize);
    sim.check_global();
}

/// R4c: a wake the decider sent for someone else's line (a call participant) never restarts the user's thinking turn: it
/// steers. The user's own words right after still restart it.
#[test]
fn r4c_someone_elses_line_steers_and_never_restarts() {
    let mut sim = Sim::new(EVENING_MS);
    sim.world.lock().unwrap().default_reply = vec![Step::Think(8_000), Step::Final("Done.")];
    sim.say("check the benchmark thread");
    sim.advance(S);
    sim.false_wake("Angel", "Doesn't have everything.");
    assert_eq!(sim.wakes().last().unwrap().kind, InputKind::Steer);
    assert!(sim.world.lock().unwrap().rejoined.is_empty());
    sim.settle();
    sim.check_global();
    let mut sim = Sim::new(EVENING_MS);
    sim.world.lock().unwrap().default_reply = vec![Step::Think(8_000), Step::Final("Done.")];
    sim.say("check the benchmark thread");
    sim.advance(S);
    sim.say("and its last line");
    assert_eq!(sim.world.lock().unwrap().rejoined.len(), 1);
    sim.settle();
    sim.check_global();
}

/// R4d: seeded sweep with written replies, written tool calls, the user's words and other speakers' lines at random
/// moments: G1/G2 hold, no restarted delivery had a tool effect, and no turn restarts more than `MAX_RESTARTS` times
/// in a row.
#[test]
fn r4d_seeded_sweep_with_writing_phases_and_other_speakers() {
    let cases: u64 = std::env::var("HEIMDALL_SIM_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(300);
    for seed in 0..cases {
        let mut rng = seed.wrapping_mul(0x2545F4914F6CDD1D).wrapping_add(7);
        let mut next = |n: u64| { rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407); (rng >> 33) % n };
        let mut sim = Sim::new(EVENING_MS);
        for _ in 0..5 {
            let mut steps = vec![Step::Think(500 + next(4_000))];
            for _ in 0..next(3) {
                if next(2) == 0 { steps.push(Step::Stream(100 + next(800))); }
                steps.push(Step::Tool("bash", 200 + next(2_000)));
                steps.push(Step::Think(200 + next(2_000)));
            }
            if next(2) == 0 { steps.push(Step::Stream(200 + next(3_000))); }
            steps.push(Step::Final("ok"));
            reply(&sim, steps);
        }
        for i in 0..2 + next(5) {
            if next(4) == 0 { sim.false_wake("Angel", &format!("call line {seed}-{i}")); } else { sim.say(&format!("utterance {seed}-{i}")); }
            sim.advance(next(4_000));
        }
        sim.settle();
        let w = sim.world.lock().unwrap();
        assert!(w.most_restarts_in_row <= crate::MAX_RESTARTS as usize, "seed {seed}");
        for id in w.rejoined.iter().flatten() {
            assert!(w.effects.iter().all(|e| &e.delivery != id), "seed {seed}: a delivery with a tool effect was restarted");
        }
        drop(w);
        sim.check_global();
    }
}

/// R9a: lines the decider sent for other people (19 Oct: 12 in 85 min of a Discord call) never cancel Heimdall's
/// idle compaction: the compaction finishes and the wake waits for it and then goes out.
#[test]
fn r9a_someone_elses_line_waits_for_an_idle_compaction() {
    let mut sim = Sim::new(EVENING_MS);
    with_policy(&mut sim, 220_000);
    sim.typed("hi");
    sim.settle();
    sim.advance(31 * S);
    assert!(sim.world.lock().unwrap().idle_compacting());
    sim.advance(10 * S);
    let before = sim.wakes().len();
    sim.false_wake("Angel", "Now I downloaded the EPUB.");
    assert!(sim.world.lock().unwrap().compacting(), "not cancelled");
    assert_eq!(sim.wakes().len(), before, "waits behind the compaction");
    sim.advance(40 * S);
    let w = sim.world.lock().unwrap();
    assert_eq!((w.compactions_started, w.compactions.len()), (1, 1));
    let (_, ended) = w.compactions[0];
    drop(w);
    let wake = sim.wakes().last().unwrap().clone();
    assert_eq!(wake.at, ended, "goes out as the compaction ends");
    assert!(wake.text.contains("Now I downloaded the EPUB."));
    sim.settle();
    sim.check_global();
}

/// R9b: a Discord call with the agent on: another speaker's line is sent every 40 s for 20 minutes while the history
/// is past the idle mark. The compaction is never cancelled and finishes; nothing starves it.
#[test]
fn r9b_a_call_full_of_false_wakes_does_not_starve_compaction() {
    let mut sim = Sim::new(EVENING_MS);
    with_policy(&mut sim, 220_000);
    sim.typed("I'm on a call");
    sim.settle();
    for i in 0..30 {
        sim.advance(40 * S);
        sim.false_wake("Angel", &format!("call line {i}"));
        // Pi's history grows back after a compaction only through turns; keep it past the mark.
        let mut w = sim.world.lock().unwrap();
        if w.context_tokens.is_none() { w.context_tokens = Some(220_000); }
    }
    sim.settle();
    let w = sim.world.lock().unwrap();
    assert_eq!(w.compactions.len(), w.compactions_started, "a compaction was cut");
    assert!(!w.compactions.is_empty(), "never compacted");
    drop(w);
    sim.check_global();
}

/// R9c: a T3 question that waits for the user (held after a start until he addresses Heimdall) does not hold the idle
/// compaction back; the user's first words wait for it and the question goes out after his turn.
#[test]
fn r9c_a_question_waiting_for_owner_does_not_block_compaction() {
    let mut sim = Sim::new(EVENING_MS);
    with_policy(&mut sim, 98_000);
    sim.world.lock().unwrap().last_request_at = Some(EVENING_MS - 2 * 60 * MIN);
    assert!(sim.t3(T3Event::Waiting { thread: "w".into(), run: "r".into(), owned: true, requests: vec!["q1".into()], text: "Worker w asks: which branch?".into() }));
    assert!(sim.bridge.awaiting_owner());
    sim.advance(31 * S);
    assert!(sim.world.lock().unwrap().idle_compacting(), "the held question does not block it");
    sim.say("which thread asked?");
    sim.advance(45 * S);
    let ended = sim.world.lock().unwrap().compactions[0].1;
    let wake = sim.wakes().iter().find(|i| i.text.contains("which thread asked?")).unwrap().clone();
    assert_eq!(wake.at, ended, "out as the compaction ends");
    sim.settle();
    assert!(sim.wakes().iter().any(|i| i.text.contains("which branch?")), "the question goes out after his turn");
    sim.check_global();
}

/// R9d: someone else's line after a start is not the user addressing Heimdall: T3 updates stay context (E4.3) until he
/// speaks. (A task's own result wakes the agent even then, pi-exit 21 Oct: `result_scenarios`.)
#[test]
fn r9d_someone_elses_line_does_not_end_the_wait_for_owner_after_a_start() {
    let mut sim = Sim::new(EVENING_MS);
    sim.false_wake("Dave", "Or less.");
    sim.settle();
    assert!(sim.bridge.awaiting_owner());
    sim.advance(MIN);
    sim.notice("t3:mcp:w:r:done", "T3 update:\n- 'w' (yours) completed: ok");
    assert!(sim.wakes().iter().all(|i| !i.text.contains("[Heimdall notice")), "a T3 update is context until User speaks");
    assert!(sim.bridge.awaiting_owner(), "an update is not User either");
    sim.say("anything new?");
    assert!(!sim.bridge.awaiting_owner());
    sim.settle();
    sim.check_global();
}

/// R9e: the user's click on Heimdall's question card during an idle compaction waits for it and goes out as it ends; a
/// repeated wake (same key) during a compaction is no new input.
#[test]
fn r9e_a_click_during_an_idle_compaction_goes_out_as_it_ends() {
    let mut sim = Sim::new(EVENING_MS);
    with_policy(&mut sim, 220_000);
    sim.say("delete the heavy files?");
    sim.settle();
    sim.advance(31 * S);
    assert!(sim.world.lock().unwrap().idle_compacting());
    sim.notice("ask-1", "User clicked Yes on your question \"Delete AllForOne's heavy files?\"");
    assert!(sim.world.lock().unwrap().idle_compacting(), "the click does not cut it");
    sim.advance(45 * S);
    let ended = sim.world.lock().unwrap().compactions[0].1;
    assert!(sim.wakes().last().unwrap().text.contains("User clicked Yes"));
    assert_eq!(sim.wakes().last().unwrap().at, ended);
    sim.settle();
    sim.check_global();

    let mut sim = Sim::new(EVENING_MS);
    with_policy(&mut sim, 220_000);
    sim.rt.block_on(sim.bridge.wake_keyed(Some("outbox-1"), "respond", &[], false, None)).unwrap();
    sim.pump();
    sim.settle();
    sim.advance(31 * S);
    assert!(sim.world.lock().unwrap().idle_compacting());
    sim.rt.block_on(sim.bridge.wake_keyed(Some("outbox-1"), "respond", &[], false, None)).unwrap();
    sim.pump();
    assert!(sim.world.lock().unwrap().compacting());
    sim.settle();
}

/// R9f: seeded sweep of compaction races with the user's words, other speakers' lines, clicks and Stops: no compaction is
/// ever cut, and G1/G2 hold.
#[test]
fn r9f_seeded_nothing_cuts_a_compaction() {
    let cases: u64 = std::env::var("HEIMDALL_SIM_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(200);
    for seed in 0..cases {
        let mut rng = seed.wrapping_mul(0x9E3779B97F4A7C15).wrapping_add(11);
        let mut next = |n: u64| { rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407); (rng >> 33) % n };
        let mut sim = Sim::new(EVENING_MS);
        with_policy(&mut sim, 180_000 + next(60_000));
        if next(2) == 0 { sim.world.lock().unwrap().last_request_at = Some(EVENING_MS - 2 * 60 * MIN); }
        for i in 0..4 + next(6) {
            sim.advance(next(120_000));
            match next(5) {
                0 => { sim.false_wake("Angel", &format!("line {seed}-{i}")); }
                1 => { sim.notice(&format!("ask-q{seed}-{i}"), &format!("User clicked No on question {i}")); }
                2 => { sim.stop(); }
                _ => { sim.say(&format!("words {seed}-{i}")); }
            }
            let mut w = sim.world.lock().unwrap();
            w.context_tokens = w.context_tokens.map(|t| t + next(8_000)).or(Some(180_000));
        }
        sim.settle();
        let w = sim.world.lock().unwrap();
        assert_eq!(w.compactions.len(), w.compactions_started, "seed {seed}: a compaction was cut");
        drop(w);
        sim.check_global();
    }
}
