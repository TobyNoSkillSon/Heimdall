//! The reply path on the virtual clock (`crate::reply`; "your final response will be actually the
//! speaking response … you can use it while you're thinking, while you're doing the task and then the final response
//! will be also read"). The final reply is what the user hears or reads, `speak` is for updates during the work, a turn
//! with nothing for the user stays silent, and no sentence is said twice. Each test ends with G1 and G2.
use super::*;

fn finals(sim: &Sim) -> Vec<String> {
    sim.heard.iter().filter(|h| h.final_reply).map(|h| h.text.clone()).collect()
}
fn reply(sim: &Sim, steps: Vec<Step>) {
    sim.world.lock().unwrap().replies.push_back(steps);
}

/// R1: an answer without tools is one model call, and its text is what the user gets when the turn settles; with work
/// in between, `speak` lines arrive during it and the final reply ends it. The pull-down keeps the final reply.
#[test]
fn r1_the_final_reply_is_what_owner_hears() {
    let mut sim = Sim::new(EVENING_MS);
    reply(&sim, vec![Step::Think(2_000), Step::Final("Yes, I can hear you.")]);
    let t0 = sim.now();
    sim.say("Hi Heimdall, can you hear me?");
    sim.settle();
    assert_eq!(sim.heard, vec![Heard { at: t0 + 2_000, text: "Yes, I can hear you.".into(), final_reply: true }]);
    assert!(sim.effects().is_empty(), "no speak call, no second model call");
    assert_eq!(sim.turns().len(), 1);
    assert_eq!(sim.last_reply.as_deref(), Some("Yes, I can hear you."));

    // Longer work: updates as often as the model wants, then the answer.
    reply(&sim, vec![
        Step::Think(1_000), Step::Speak("On it. Checking the build.", 1_000), Step::Tool("bash", 6_000),
        Step::Speak("Halfway: 120 of 222 tests passed.", 500), Step::Tool("bash", 5_000),
        Step::Final("The build is green.\nAll 222 tests passed."),
    ]);
    sim.say("check the build");
    sim.settle();
    let texts = sim.heard_texts();
    assert_eq!(&texts[1..], ["On it. Checking the build.", "Halfway: 120 of 222 tests passed.", "The build is green.\nAll 222 tests passed."]);
    assert_eq!(finals(&sim).len(), 2, "one final reply per turn");
    assert!(sim.heard[3].at - sim.heard[1].at >= 11_000, "the answer comes when the work is done");
    assert_eq!(sim.last_reply.as_deref(), Some("The build is green.\nAll 222 tests passed."));
    sim.check_global();
}

/// R2: a turn with nothing for the user says and shows nothing: the silent token, an empty reply, the earlier
/// "No request." habit, a worker notice that needs nothing, and context that never makes a turn. The next real
/// reply is heard whole.
#[test]
fn r2_silent_turns_stay_silent() {
    let mut sim = Sim::new(EVENING_MS);
    // The G1 check tells utterances apart by their words, so each false wake says something else.
    for (n, text) in ["NO_REPLY", "", "No request; waiting for the rest of the sentence.", "NO_REPLY — that was my own voice on the mic."].into_iter().enumerate() {
        reply(&sim, vec![Step::Think(1_500), Step::Final(text)]);
        sim.say(&format!("Yes, I can hear you ({n})."));
        sim.settle();
        sim.advance(40 * S);
    }
    reply(&sim, vec![Step::Think(1_000), Step::Final("NO_REPLY")]);
    sim.notice("terminal:w1:r1:completed", "Worker w1 finished: the tests pass.");
    sim.settle();
    sim.aside("Dave", "did you push the fix");
    sim.aside("User", "yeah an hour ago");
    sim.settle();
    assert_eq!(sim.turns().len(), 5);
    assert!(sim.heard.is_empty(), "{:?}", sim.heard);
    assert!(sim.effects().is_empty());
    assert_eq!(sim.last_reply, None, "the pull-down keeps nothing new");
    reply(&sim, vec![Step::Think(1_000), Step::Final("Dave pushed the fix an hour ago.")]);
    sim.say("did Dave push it?");
    sim.settle();
    assert_eq!(sim.heard_texts(), ["Dave pushed the fix an hour ago."]);
    sim.check_global();
}

/// R3: `speak` then a final reply that repeats it: each sentence is said once. A final reply that only repeats says
/// nothing more but still fills the pull-down; a silent end after speaking shows the spoken lines; the next turn may
/// say the same words again.
#[test]
fn r3_speak_then_final_never_says_a_sentence_twice() {
    let mut sim = Sim::new(EVENING_MS);
    let line = "Started a Haiku thread. I'll tell you when it replies.";
    reply(&sim, vec![Step::Think(1_000), Step::Tool("delegate_task", 2_000), Step::Speak(line, 500), Step::Final(line)]);
    sim.say("spawn a Haiku thread");
    sim.settle();
    assert_eq!(sim.heard_texts(), [line]);
    assert_eq!(sim.last_reply.as_deref(), Some(line));
    sim.advance(40 * S);

    reply(&sim, vec![Step::Speak("On it.", 500), Step::Tool("bash", 3_000), Step::Final("On it! The tests are green.\n- All 40 passed.")]);
    sim.say("run the fast tests");
    sim.settle();
    assert_eq!(&sim.heard_texts()[1..], ["On it.", "The tests are green.\n- All 40 passed."]);
    assert_eq!(sim.last_reply.as_deref(), Some("On it! The tests are green.\n- All 40 passed."), "the pull-down has the reply as written");
    sim.advance(40 * S);

    reply(&sim, vec![Step::Speak("Noted. The animations are slow.", 500), Step::Final("NO_REPLY")]);
    sim.say("the animations are slow");
    sim.settle();
    assert_eq!(sim.heard_texts().last().map(String::as_str), Some("Noted. The animations are slow."));
    assert_eq!(sim.last_reply.as_deref(), Some("Noted. The animations are slow."));
    sim.advance(40 * S);

    reply(&sim, vec![Step::Think(500), Step::Final("On it.")]);
    sim.say("again");
    sim.settle();
    assert_eq!(sim.heard_texts().last().map(String::as_str), Some("On it."), "another turn says it again");
    sim.check_global();
}

/// R4: the user steers a running turn. During a tool, the steer joins before the model writes its reply: one final reply.
/// After the last tool, while the reply is already written, Pi keeps the same run but the written reply ended its own
/// model turn: the user hears it, then the reply to the steer (review 19 Oct: "the user asks A; while A's final text streams
/// he adds B → A is never said"). Nothing is said twice.
#[test]
fn r4_a_steered_turn_ends_with_one_final_reply() {
    let mut sim = Sim::new(EVENING_MS);
    reply(&sim, vec![Step::Think(500), Step::Speak("Running the tests.", 500), Step::Tool("bash", 4_000), Step::Tool("bash", 2_000), Step::Final("Only the fast tests ran. All 40 passed.")]);
    sim.say("run the tests");
    sim.advance(1_500);
    sim.say("only the fast ones");
    sim.settle();
    assert_eq!(sim.wakes().iter().filter(|i| i.kind == InputKind::Steer).count(), 1);
    assert_eq!(sim.turns().len(), 1);
    assert_eq!(sim.heard_texts(), ["Running the tests.", "Only the fast tests ran. All 40 passed."]);
    sim.advance(40 * S);

    // A steer after the last tool, while the first reply is being written: that reply is said, then the steer's.
    sim.world.lock().unwrap().steer_reply = vec![Step::Think(800), Step::Final("Only the fast ones: all 40 passed.")];
    reply(&sim, vec![Step::Speak("Running the tests.", 500), Step::Think(3_000), Step::Final("All 222 tests passed.")]);
    sim.say("run all the tests");
    sim.advance(1_500);
    sim.say("and what time is it");
    sim.settle();
    assert_eq!(sim.turns().len(), 2);
    assert_eq!(&sim.heard_texts()[2..], ["Running the tests.", "All 222 tests passed.", "Only the fast ones: all 40 passed."]);
    assert_eq!(finals(&sim).len(), 3, "each written reply once");
    assert_eq!(sim.last_reply.as_deref(), Some("Only the fast ones: all 40 passed."));

    // The steer's reply repeats the first: said once. (The turn acted first, so the words steer it; before any tool
    // they would restart it instead, night pass H2, `n1_…`.)
    sim.advance(40 * S);
    sim.world.lock().unwrap().steer_reply = vec![Step::Think(800), Step::Final("All 222 tests passed. It is 14:05.")];
    reply(&sim, vec![Step::Tool("bash", 500), Step::Think(3_000), Step::Final("All 222 tests passed.")]);
    sim.say("run them again");
    sim.advance(1_500);
    sim.say("also the time please");
    sim.settle();
    assert_eq!(&sim.heard_texts()[5..], ["All 222 tests passed.", "It is 14:05."]);
    sim.check_global();
}

/// R5: the user's Stop and a provider error end a turn without a final reply: only what was already spoken reached
/// him, and the next turn starts with nothing said.
#[test]
fn r5_stopped_or_failed_turns_say_nothing_more() {
    let mut sim = Sim::new(EVENING_MS);
    reply(&sim, vec![Step::Speak("On it.", 500), Step::Final("Half an"), Step::Think(10_000)]);
    sim.say("think hard about this");
    sim.advance(2 * S);
    sim.stop();
    sim.settle();
    assert_eq!(sim.heard_texts(), ["On it."]);
    reply(&sim, vec![Step::Speak("Checking.", 500), Step::Fail("529 overloaded_error".into())]);
    sim.say("try again");
    sim.settle();
    assert_eq!(sim.heard_texts(), ["On it.", "Checking."]);
    reply(&sim, vec![Step::Think(500), Step::Final("On it. Checking. The answer is 42.")]);
    sim.say("once more");
    sim.settle();
    assert_eq!(sim.heard_texts().last().map(String::as_str), Some("On it. Checking. The answer is 42."), "nothing carried over from the stopped or failed turn");
    sim.check_global();
}

/// R6: restarts and catch-up are unaffected: a start's context makes no turn and says nothing, a crashed turn never
/// settles a reply, and the turn after the restart is said whole.
#[test]
fn r6_restart_and_catch_up_say_nothing() {
    let mut sim = Sim::new(EVENING_MS);
    sim.heard_while_off(EVENING_MS - 5 * MIN, "User", "remember to check the build");
    reply(&sim, vec![Step::Speak("Checking.", 500), Step::Tool("bash", 20_000)]);
    sim.say("check the build");
    sim.advance(3 * S);
    sim.world.lock().unwrap().crash();
    sim.advance(2 * S);
    sim.restart(Some("crash recovery"));
    sim.settle();
    assert_eq!(sim.heard_texts(), ["Checking."], "the crashed turn has no reply and the start context says nothing");
    let appends = sim.appends();
    assert!(appends.last().unwrap().text.contains(&crate::notice::restart_message(sim.started_at)), "{appends:#?}");
    assert_eq!(sim.turns().len(), 1, "the start context made no turn");
    sim.advance(10 * MIN);
    sim.restart(None);
    sim.settle();
    assert_eq!(sim.turns().len(), 1);
    reply(&sim, vec![Step::Think(500), Step::Final("Checking. The build finished green.")]);
    sim.say("did it finish?");
    sim.settle();
    assert_eq!(sim.heard_texts(), ["Checking.", "Checking. The build finished green."]);
    sim.check_global();
}
