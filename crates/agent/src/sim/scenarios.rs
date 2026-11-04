//! lab/harness/SCENARIOS.md on the virtual clock. Each test names its scenario id and the EXPECTED.md rules it
//! checks, and ends with the global properties G1 and G2.
use super::*;
use crate::notice::restart_message;

fn png(n: u8) -> Attachment {
    Attachment { media_type: "image/png".into(), bytes: vec![n; 32] }
}
fn texts(inputs: &[Input]) -> Vec<String> {
    inputs.iter().map(|i| i.text.clone()).collect()
}
fn hms(ms: u64) -> String {
    crate::clock::local_hms(ms)
}

/// S01 (E1.1, E11.1): addressed speech wakes once, at once, and the agent is working.
#[test]
fn s01_addressed_speech_wakes_once() {
    let mut sim = Sim::new(EVENING_MS);
    sim.advance(10 * S);
    let t0 = sim.now();
    sim.say("open the build log");
    let wakes = sim.wakes();
    assert_eq!(wakes.len(), 1);
    assert_eq!(wakes[0].kind, InputKind::Prompt);
    assert!(wakes[0].at - t0 <= 150, "wake delivered {} ms after the decision", wakes[0].at - t0);
    assert!(wakes[0].text.contains("open the build log"));
    assert_eq!(sim.bridge.status, crate::Status::Working);
    sim.settle();
    assert_eq!(sim.turns().len(), 1);
    assert_eq!(sim.bridge.status, crate::Status::Idle);
    sim.check_global();
}

/// S02 (root cause 4): not-for-Heimdall speech rides the next wake with its clock time; a line older than 30 minutes
/// stays in the archive instead.
#[test]
fn s02_asides_carry_times_and_age_out_after_30_minutes() {
    let mut sim = Sim::new(EVENING_MS);
    let t0 = sim.now();
    let a = "the build is green on my side";
    sim.aside("User", a);
    sim.advance(5 * MIN);
    sim.say("so what do you think");
    let wake1 = sim.wakes()[0].text.clone();
    assert!(wake1.contains(&format!("[{} Mic · User] {a}", hms(t0))), "{wake1}");
    sim.settle();
    let b_at = t0 + 6 * MIN;
    sim.clock.set(b_at);
    let b = sim.aside("User", "this line is going to be old");
    sim.clock.set(t0 + 40 * MIN);
    let c = "and this one is fresh";
    sim.aside("User", c);
    sim.advance(MIN);
    sim.say("now answer");
    let wake2 = sim.wakes().last().unwrap().text.clone();
    assert!(wake2.contains(c), "{wake2}");
    assert!(!wake2.contains("going to be old"), "{wake2}");
    assert_eq!(sim.dropped(&b).as_deref(), Some("older than 30 minutes when a wake went out"));
    sim.settle();
    sim.check_global();
}

/// S05 (E1.4, E1.15): the restart message is context, never agent speech: a start plays nothing and runs no turn.
#[test]
fn s05_a_start_never_speaks_or_wakes() {
    let mut sim = Sim::new(EVENING_MS);
    sim.advance(10 * MIN);
    assert!(sim.effects().is_empty());
    assert!(sim.turns().is_empty());
    assert_eq!(sim.appends().len(), 1);
    assert_eq!(sim.appends()[0].text, restart_message(EVENING_MS));
}

/// S06 (E1.9, E7.2, incident 17:41–17:54 "Mobius"): a long-lived thread Heimdall did not start finishes a new run
/// every 30 s with the same last message; nothing reaches the agent. If an old observer still produced the lines
/// with random ids, the harness's 24-hour repeat check lets one through at most.
#[test]
fn s06_repeating_non_owned_thread_never_wakes() {
    let mut sim = Sim::new(EVENING_MS);
    sim.say("hello");
    sim.settle();
    let before = sim.inputs().len();
    for n in 0..30 {
        let text = if n % 3 == 2 { String::new() } else { "No, it's not me, it's the Mobius bridge".into() };
        assert!(!sim.t3(T3Event::Finished { thread: "mcp:9209239e".into(), run: format!("run-{}", 628 + n), owned: false, text }));
        sim.advance(30 * S);
    }
    assert_eq!(sim.inputs().len(), before, "zero wakes, zero appends");
    // Defence in depth: the legacy observer's random ids with identical words.
    for n in 0..7 {
        sim.notice(&format!("t3-activity:{n}-random"), "T3 update: 'Mobius' completed: No, it's not me");
        sim.advance(2 * MIN);
    }
    let mobius: Vec<_> = sim.inputs().into_iter().filter(|i| i.text.contains("'Mobius' completed")).collect();
    assert_eq!(mobius.len(), 1, "{mobius:#?}");
    sim.check_global();
}

/// S07 (E1.8, E6.3, E7.2): another thread's question wakes once per new request set, with deterministic ids.
#[test]
fn s07_question_wakes_once_per_request_set() {
    let mut sim = Sim::new(EVENING_MS);
    sim.say("start");
    sim.settle();
    let waiting = |requests: &[&str]| T3Event::Waiting { thread: "thread-a".into(), run: "r1".into(), owned: false,
        requests: requests.iter().map(|s| s.to_string()).collect(), text: format!("Thread A asks: {}", requests.join(" and ")) };
    assert!(sim.t3(waiting(&["q1"])));
    sim.settle();
    assert!(!sim.t3(waiting(&["q1"])), "same request set, same id");
    sim.settle();
    assert!(sim.t3(waiting(&["q1", "q2"])));
    sim.settle();
    // Answered: no pending requests and no approval either, so the policy reports nothing new.
    let notice_turns: Vec<_> = sim.wakes().into_iter().filter(|i| i.text.contains("Thread A asks")).collect();
    assert_eq!(notice_turns.len(), 2, "{:#?}", texts(&notice_turns));
    assert!(notice_turns[0].id.as_deref() == Some("t3:thread-a:r1:attention:q1"));
    assert!(notice_turns[1].id.as_deref() == Some("t3:thread-a:r1:attention:q1,q2"));
    sim.check_global();
}

/// S08 (E7.2): an approval wait reported on ten passes wakes once.
#[test]
fn s08_approval_wait_wakes_once() {
    let mut sim = Sim::new(EVENING_MS);
    sim.say("start");
    sim.settle();
    for _ in 0..10 {
        sim.t3(T3Event::Waiting { thread: "thread-b".into(), run: "r7".into(), owned: false, requests: vec![], text: "Thread B waits for an approval only User can give in T3".into() });
        sim.advance(30 * S);
    }
    sim.settle();
    assert_eq!(sim.wakes().iter().filter(|i| i.text.contains("approval only User")).count(), 1);
    sim.check_global();
}

/// E1.8: an approval wait's words carry no run or request id, so the same words on another run are a new request
/// and wake; the same run reported again is one wake.
#[test]
fn approval_waits_on_different_runs_each_wake_once() {
    let mut sim = Sim::new(EVENING_MS);
    sim.say("start");
    sim.settle();
    let approval = |run: &str| T3Event::Waiting { thread: "thread-c".into(), run: run.into(), owned: false, requests: vec![],
        text: "Thread C waits for an approval only User can give in T3".into() };
    assert!(sim.t3(approval("run-1")));
    assert!(!sim.t3(approval("run-1")), "the same approval reported again is one");
    sim.settle();
    sim.advance(10 * MIN);
    assert!(sim.t3(approval("run-2")), "ten minutes later, another run's approval wakes");
    sim.settle();
    assert_eq!(sim.wakes().iter().filter(|i| i.text.contains("Thread C waits for an approval")).count(), 2);
    sim.check_global();
}

/// E11.3 (18 Oct 17:43:07 +5.1 s, 17:43:38 +9.2 s): the user's click on a question Heimdall asked is not held behind the
/// 30-second notice gap. The notice turn here runs 20 s; the answer steers it (19 Oct 18:48) instead of waiting.
#[test]
fn an_answer_never_waits_behind_the_notice_gap() {
    let mut sim = Sim::new(EVENING_MS);
    sim.say("start");
    sim.settle();
    sim.world.lock().unwrap().replies.push_back(vec![Step::Think(20_000)]);
    let event_at = sim.now();
    assert!(sim.t3(T3Event::Finished { thread: "worker-2".into(), run: "r1".into(), owned: true, text: "Worker finished: tests pass".into() }));
    sim.advance(5 * S);
    assert!(sim.notice("ask-1b2c", "Question ask-1b2c: User clicked Yes. Question: Install?"));
    sim.settle();
    let answer = sim.wakes().into_iter().find(|i| i.text.contains("User clicked Yes")).expect("the answer woke the agent");
    assert!(answer.at < event_at + crate::notice::NOTICE_GAP_MS, "the answer waited for the gap: delivered at +{} ms", answer.at - event_at);
    sim.check_global();
}

/// E4.4 with a start context that failed twice: the speech it carried was not settled up front, so the start that
/// finally adds the context carries it. (Before, the first failed start settled it and the speech was lost.)
#[test]
fn a_start_context_that_fails_twice_keeps_its_speech_for_the_next_start() {
    let mut sim = Sim::new(EVENING_MS);
    sim.say("start");
    sim.settle();
    let aside = "the build is green on my side";
    sim.aside("User", aside);
    for _ in 0..2 {
        sim.advance(MIN);
        sim.world.lock().unwrap().failing_appends = 1;
        sim.restart(None);
    }
    sim.advance(MIN);
    sim.restart(None);
    let last = sim.appends().last().unwrap().clone();
    assert!(last.text.contains(aside), "the catch-up lost the speech: {}", last.text);
    assert!(last.text.ends_with(&restart_message(sim.now())), "{}", last.text);
    sim.check_global();
}

/// E2.1 / E4.5 (18 Oct): a legacy queued notice with no time reads `(earlier)` in the catch-up, not the clock time of
/// the start that found it.
#[test]
fn a_legacy_queued_notice_without_a_time_is_marked_earlier() {
    let mut sim = Sim::new(EVENING_MS);
    let wire = serde_json::json!({"message": {"role": "user", "content": [{"type": "text", "text": "[Heimdall notice: old]\nWorker finished long ago"}]}, "notice": "event", "shouldQuery": true});
    sim.bridge.db.execute("INSERT INTO deliveries VALUES('legacy-1',?1,1,0,'queued')", [wire.to_string()]).unwrap();
    sim.restart(None);
    let start = sim.appends().last().unwrap().clone();
    assert!(start.text.contains("[(earlier) Notice] [Heimdall notice: old]"), "{}", start.text);
    assert!(sim.wakes().iter().all(|w| !w.text.contains("Worker finished long ago")), "a legacy notice never wakes the agent");
}

/// S09 (E7.1): an owned worker's result wakes once; the next pass seeing the same terminal run adds nothing.
#[test]
fn s09_owned_worker_result_wakes_once() {
    let mut sim = Sim::new(EVENING_MS);
    sim.say("launch a worker");
    sim.settle();
    let done = T3Event::Finished { thread: "worker-1".into(), run: "r1".into(), owned: true, text: "Worker finished: tests pass".into() };
    assert!(sim.t3(done.clone()));
    sim.settle();
    assert!(!sim.t3(done));
    sim.settle();
    assert_eq!(sim.wakes().iter().filter(|i| i.text.contains("tests pass")).count(), 1);
    sim.check_global();
}

/// S10 (E2.1–E2.3, E11.3, incident 17:43:07/17:43:37; 19 Oct 18:46:53): during the user's six-tool turn a worker notice
/// and a question answer arrive, then the user speaks. The answer click is the user's own input and steers the turn the moment
/// it arrives (card answers "should not be queued, they should be steering messages"); his speech
/// steers at the next tool boundary; the worker notice waits for settlement and is never a steer.
#[test]
fn s10_worker_notices_queue_behind_owners_turn_and_answer_clicks_steer() {
    let mut sim = Sim::new(EVENING_MS);
    sim.world.lock().unwrap().replies.push_back(vec![Step::Think(1_000), Step::Tool("island_notice", 3_000), Step::Tool("island_notice", 3_000),
        Step::Tool("island_notice", 3_000), Step::Tool("island_notice", 3_000), Step::Tool("island_notice", 3_000), Step::Tool("island_notice", 3_000)]);
    let t0 = sim.now();
    sim.say("test the island icons one by one");
    sim.advance(4 * S);
    sim.notice("terminal:worker-1:r1:completed", "Worker worker-1 finished: build green");
    sim.advance(4 * S);
    let clicked_at = sim.now();
    sim.notice("ask-7", "Question ask-7: User clicked Yes. Question: Keep the new icon?");
    let click = sim.wakes().into_iter().find(|i| i.text.contains("Keep the new icon")).expect("the click reached the turn");
    assert_eq!(click.kind, InputKind::Steer, "the click steers the running turn");
    assert_eq!(click.at, clicked_at, "the click waited {} ms", click.at - clicked_at);
    sim.advance(S);
    sim.say("and make the last one bigger");
    let steer = sim.wakes().into_iter().find(|i| i.text.contains("make the last one bigger")).unwrap();
    assert_eq!(steer.kind, InputKind::Steer);
    assert!(click.at < steer.at, "delivered in the order User gave them");
    assert!(sim.wakes().iter().all(|i| !(i.kind == InputKind::Steer && i.text.contains("build green"))), "a worker notice steered");
    let settled_at = sim.turns()[0].0;
    sim.settle();
    let notices: Vec<_> = sim.wakes().into_iter().filter(|i| i.text.contains("build green")).collect();
    assert_eq!(notices.len(), 1, "{:#?}", texts(&notices));
    assert_eq!(notices[0].kind, InputKind::Prompt);
    assert!(!notices[0].text.contains("Keep the new icon"), "the click is not delivered twice: {}", notices[0].text);
    assert!(notices[0].at >= t0 + 19 * S, "after User's turn settled (started {settled_at})");
    assert_eq!(sim.turns().len(), 2, "User's turn (with both steers) and one notice turn");
    sim.check_global();
}

/// 19 Oct 18:44–18:47: two answer clicks during one long turn steer it in the order the user clicked; a click while idle
/// starts a turn at once, even inside the notice gap.
#[test]
fn answer_clicks_steer_in_click_order_and_wake_an_idle_agent_at_once() {
    let mut sim = Sim::new(EVENING_MS);
    sim.world.lock().unwrap().replies.push_back(vec![Step::Think(1_000), Step::Tool("bash", 20_000)]);
    sim.say("start the widget thread and ask me two things");
    sim.advance(3 * S);
    sim.notice("ask-a", "Question ask-a: User clicked Later. Question: Start a thread?");
    sim.advance(S);
    sim.notice("ask-b", "Question ask-b: User clicked Keep going. Question: Stop it?");
    let steers: Vec<_> = sim.wakes().into_iter().filter(|i| i.kind == InputKind::Steer).collect();
    assert_eq!(steers.len(), 2, "{:#?}", texts(&steers));
    assert!(steers[0].text.contains("clicked Later") && steers[1].text.contains("clicked Keep going"), "{:#?}", texts(&steers));
    sim.settle();
    assert_eq!(sim.turns().len(), 1, "no notice turn repeats a steered click");
    // Idle, right after a notice turn: the click does not wait for the 30-second gap.
    assert!(sim.t3(T3Event::Finished { thread: "worker-2".into(), run: "r1".into(), owned: true, text: "Worker finished: tests pass".into() }));
    sim.settle();
    let at = sim.now();
    sim.notice("ask-c", "Question ask-c: User clicked Yes. Question: Install?");
    let wake = sim.wakes().into_iter().find(|i| i.text.contains("clicked Yes")).expect("the click woke the agent");
    assert_eq!(wake.kind, InputKind::Prompt);
    assert!(wake.at - at < crate::notice::NOTICE_GAP_MS, "the click waited {} ms", wake.at - at);
    sim.settle();
    sim.check_global();
}

/// S11 (E1.6, E1.10; rewrite of `joined_fast_read_result_reaches_the_agent_once_and_remembers_the_speed`): context-
/// only notices are appended with no turn; a question answer still wakes.
#[test]
fn s11_context_only_notices_are_appends() {
    let mut sim = Sim::new(EVENING_MS);
    sim.say("read this to me fast");
    sim.settle();
    let turns = sim.turns().len();
    sim.notice("fastread-1", "Fast read: read at 390 wpm.");
    sim.notice("w-progress", "Worker monitoring: worker-1 is still running tests");
    sim.notice("unheard-speech:1", "Heimdall said without sound: Done.");
    sim.settle();
    assert_eq!(sim.turns().len(), turns, "no turn for context");
    let appends = texts(&sim.appends());
    for words in ["390 wpm", "still running tests", "without sound"] {
        assert_eq!(appends.iter().filter(|a| a.contains(words)).count(), 1, "{words}: {appends:#?}");
    }
    sim.notice("ask-1", "Question ask-1: User clicked No. Question: Delete the branch?");
    sim.settle();
    assert_eq!(sim.turns().len(), turns + 1, "an answer wakes");
    sim.check_global();
}

/// S13 (E1.18): five worker notices within 20 s while idle: the first wakes, the other four go out together at least
/// 30 s after it.
#[test]
fn s13_notices_coalesce_and_pace_at_one_turn_per_30_seconds() {
    let mut sim = Sim::new(EVENING_MS);
    sim.say("launch five workers");
    sim.settle();
    let t0 = sim.now();
    for n in 0..5 {
        sim.notice(&format!("terminal:w{n}:r1:completed"), &format!("Worker w{n} finished"));
        sim.advance(4 * S);
    }
    sim.settle();
    let notices: Vec<_> = sim.wakes().into_iter().filter(|i| i.text.contains("[Heimdall notice")).collect();
    assert_eq!(notices.len(), 2, "{:#?}", texts(&notices));
    assert!(notices[0].text.contains("Worker w0 finished") && !notices[0].text.contains("w1"));
    for n in 1..5 { assert!(notices[1].text.contains(&format!("Worker w{n} finished"))); }
    assert!(notices[1].at >= notices[0].at + 30 * S && notices[0].at >= t0);
    sim.check_global();
}

/// S14 (E4.1, E4.3, E4.5): the agent turns on after five hours off with 40 minutes of speech in the archive. The
/// catch-up holds the last 30 minutes with clock times, the restart message comes last, no turn runs, and the user's next
/// utterance wakes with only post-start speech.
#[test]
fn s14_turn_on_after_hours_off() {
    let on = EVENING_MS + 5 * 60 * MIN;
    let mut sim = Sim::new(EVENING_MS);
    for n in 0..40u64 {
        let speaker = if n % 3 == 0 { "Dave" } else { "User" };
        sim.heard_while_off(on - (40 - n) * MIN, speaker, &format!("off-period line {n:02}"));
    }
    sim.clock.set(on);
    sim.restart(None);
    let append = sim.appends().last().unwrap().text.clone();
    assert!(append.ends_with(&restart_message(on)), "{append}");
    assert!(!append.contains("off-period line 09"), "older than 30 minutes");
    for n in 10..40u64 {
        let speaker = if n % 3 == 0 { "Dave" } else { "User" };
        assert!(append.contains(&format!("[{} Mic · {speaker}] off-period line {n:02}", hms(on - (40 - n) * MIN))), "line {n}: {append}");
    }
    sim.advance(10 * MIN);
    assert_eq!(sim.turns().len(), 0, "zero turns until User speaks");
    sim.say("what did Dave say about the build");
    let wake = sim.wakes()[0].text.clone();
    assert!(!wake.contains("off-period"), "the first wake carries only post-start speech: {wake}");
    sim.settle();
    sim.check_global();
}

/// S15 (E1.16, E4.3, E7.4, incident 17:41:59 "Done. I circled the failed."): the conversation ends with the user's redo
/// request whose turn failed; four hours later the app launches with the provider key missing; a T3 line for a
/// thread Heimdall did not start, a T3 update and an owned worker result arrive before the key is connected 52 s
/// later. Until the user speaks, the only turn is the worker's result (pi-exit, 21 Oct: a task's result wakes the agent,
/// once, after the start context); the stale redo is never a request, nothing else wakes it, and nothing looks,
/// points or speaks.
#[test]
fn s15_launch_with_a_stale_request_and_notices_does_nothing() {
    let mut sim = Sim::new(EVENING_MS - 4 * 60 * MIN);
    let signature = "400 invalid_request_error: Invalid `signature` in `thinking` block";
    sim.world.lock().unwrap().replies.extend([vec![Step::Think(500), Step::Fail(signature.into())], vec![Step::Think(500), Step::Fail(signature.into())]]);
    let redo = sim.say("can you do it again, I'm back on the T3 screen");
    sim.settle();
    assert!(sim.resolutions.iter().any(|r| r.outcome == crate::Resolved::Failed));
    // App launch: the agent is on, the key is missing.
    sim.clock.set(EVENING_MS + 2 * MIN);
    sim.bridge.status = crate::Status::WaitingProvider("Connect Anthropic".into());
    sim.t3(T3Event::Finished { thread: "mcp:mobius".into(), run: "run-640".into(), owned: false, text: "No, it's not me".into() });
    sim.notice("t3:mcp:mobius:run-641:done", "T3 update:\n- 'Mobius' completed: the bridge is fine");
    sim.notice("terminal:worker-9:r3:completed", "Worker worker-9 finished: the screenshot diff is ready");
    sim.advance(52 * S);
    sim.restart(None);
    let start = sim.appends().iter().rfind(|a| a.text.ends_with(&restart_message(sim.started_at))).unwrap().text.clone();
    assert!(!start.contains("screenshot diff is ready"), "the worker result is a wake, not history: {start}");
    assert!(start.contains("the bridge is fine"), "the T3 update is history: {start}");
    assert!(!start.contains("not me"));
    assert!(!start.contains("can you do it again"), "four hours old: archive only: {start}");
    sim.advance(15 * MIN);
    let after: Vec<_> = sim.turns().into_iter().filter(|t| t.0 >= EVENING_MS).collect();
    assert_eq!(after.len(), 1, "only the worker's result: {after:?}");
    assert_eq!(after[0].1, "terminal:worker-9:r3:completed");
    let woke: Vec<_> = sim.wakes().into_iter().filter(|w| w.at >= EVENING_MS).collect();
    assert!(woke.len() == 1 && woke[0].text.contains("screenshot diff is ready") && !woke[0].text.contains("can you do it again"), "{woke:#?}");
    // Whatever that turn does is the result's own business; nothing looks or points for the stale redo.
    assert!(sim.effects().iter().filter(|e| e.at >= EVENING_MS).all(|e| e.delivery == "terminal:worker-9:r3:completed" && e.tool != "look" && e.tool != "point"),
        "{:#?}", sim.effects());
    assert!(sim.dropped(&redo).is_some());
    sim.check_global();
}

/// S16 (E4.4, E4.5, incident 17:42:33): six unsettled asides from two hours before the start and two from ten
/// minutes before. The two recent ones are history in the catch-up with their times, the six old ones are never
/// delivered, the first wake carries only the new utterance, and all eight are settled.
#[test]
fn s16_spool_leftovers_from_before_the_start_are_history() {
    let mut sim = Sim::new(EVENING_MS - 3 * 60 * MIN);
    let start = EVENING_MS;
    let mut old = vec![];
    for n in 0..6 {
        sim.clock.set(start - 2 * 60 * MIN + n * MIN);
        old.push(sim.aside("User", &format!("old aside {n}")));
    }
    let mut recent = vec![];
    for n in 0..2 {
        sim.clock.set(start - 10 * MIN + n * MIN);
        recent.push((sim.now(), sim.aside("User", &format!("recent aside {n}"))));
    }
    sim.clock.set(start);
    sim.restart(None);
    let catchup = sim.appends().last().unwrap().text.clone();
    for (n, (at, _)) in recent.iter().enumerate() {
        assert!(catchup.contains(&format!("[{} Mic · User] recent aside {n}", hms(*at))), "{catchup}");
    }
    assert!(!catchup.contains("old aside"));
    assert_eq!(sim.bridge.pending_observations().unwrap(), 0, "all eight settled");
    for id in &old { assert!(sim.dropped(id).is_some(), "{id} kept a reason"); }
    sim.advance(MIN);
    sim.say("what were we doing");
    let wake = sim.wakes()[0].text.clone();
    assert!(!wake.contains("aside"), "{wake}");
    sim.settle();
    sim.check_global();
}

/// S17 (E1.14, E4.4; rewrite of the cross-start half of `failed_turn_without_effects_retries_once_then_hands_speech_to_the_next_wake`):
/// inside the process a failure before any tool ran is resent once; across a start it never is. Its words are
/// already in the conversation (Pi keeps a failed request), so the catch-up does not repeat them (night pass 20→21
/// Oct: on 20 Oct "Hi Heimdall, you there?" came back in each restart's catch-up after its own failed request).
#[test]
fn s17_a_failed_delivery_is_resent_in_process_and_never_across_a_start() {
    let mut sim = Sim::new(EVENING_MS);
    let overloaded = "529 overloaded_error: Overloaded";
    sim.world.lock().unwrap().replies.push_back(vec![Step::Think(800), Step::Fail(overloaded.into())]);
    sim.say("check the deploy");
    sim.settle();
    let deploy: Vec<_> = sim.wakes().into_iter().filter(|i| i.text.contains("check the deploy")).collect();
    assert_eq!(deploy.len(), 2, "one in-process resend");
    assert!(deploy[1].text.contains("resent after a delivery interruption"));
    // Fails on every try (an overload is a passing failure: three resends, `notice::RETRY_DELAYS_MS`); then a
    // restart 10 minutes later: history once, never resent.
    let tries = 1 + crate::notice::RETRY_DELAYS_MS.len();
    sim.world.lock().unwrap().replies.extend((0..tries).map(|_| vec![Step::Think(800), Step::Fail(overloaded.into())]));
    sim.say("check the logs");
    sim.settle();
    assert_eq!(sim.wakes().iter().filter(|i| i.text.contains("check the logs")).count(), tries);
    sim.advance(10 * MIN);
    sim.restart(None);
    let start = sim.appends().last().unwrap().text.clone();
    assert_eq!(start.matches("check the logs").count(), 0, "already in the conversation: {start}");
    assert!(start.ends_with(&crate::notice::restart_message(sim.now())), "{start}");
    sim.advance(5 * MIN);
    assert_eq!(sim.wakes().iter().filter(|i| i.text.contains("check the logs")).count(), tries, "not resent across the start");
    sim.check_global();
}

/// S17b (E1.14): a resend that could not go out within two minutes (the agent was blocked) is not sent at all.
#[test]
fn s17b_a_resend_older_than_two_minutes_is_dropped() {
    let mut sim = Sim::new(EVENING_MS);
    sim.world.lock().unwrap().replies.push_back(vec![Step::Think(800), Step::Fail("503 Service Unavailable".into())]);
    sim.world.lock().unwrap().compaction_ms = 3 * MIN;
    sim.say("check the deploy");
    sim.advance(500);
    sim.compaction();
    sim.settle();
    assert_eq!(sim.wakes().iter().filter(|i| i.text.contains("check the deploy")).count(), 1, "the late resend was dropped");
    assert!(sim.resolutions.iter().any(|r| r.reason.contains("longer than two minutes")));
    sim.say("anything?");
    assert!(sim.wakes().last().unwrap().text.contains("check the deploy"), "its words ride the next wake");
    sim.settle();
    sim.check_global();
}

/// S18 (E6.2): a crash after the turn's `speak` started: on restart the delivery settles from the effects record and
/// nothing is replayed.
#[test]
fn s18_interrupted_tool_turn_is_never_replayed() {
    let mut sim = Sim::new(EVENING_MS);
    sim.world.lock().unwrap().replies.push_back(vec![Step::Think(500), Step::Tool("speak", 5_000), Step::Tool("point", 1_000)]);
    sim.say("say hello then point at the dock");
    sim.advance(2 * S);
    assert_eq!(sim.effects().len(), 1);
    sim.world.lock().unwrap().crash();
    sim.advance(10 * S);
    sim.restart(None);
    sim.settle();
    assert_eq!(sim.effects().len(), 1, "no speech replayed");
    assert_eq!(sim.wakes().len(), 1);
    assert!(sim.bridge.inflight().unwrap().is_empty());
    sim.check_global();
}

/// S19 (E4.1–E4.4, E5.2, E2.2): restart with everything pending at once: the history at 98 % of the threshold, a
/// the user wake queued 2 s before (Pi compacting), a T3 question on another thread, an owned worker result, 25 minutes
/// of off-period speech. In order: compaction, then the catch-up with the restart message last, then his request from
/// 2 s before as a wake (night pass 3, bug note 20 Oct 22:12:57: it never reached the model, so it is still a
/// request, not history); the question and the worker's result (pi-exit, 21 Oct: a task's result is never history)
/// wake together after that turn settles; nothing else by itself.
#[test]
fn s19_restart_with_everything_pending() {
    let mut sim = Sim::new(EVENING_MS - 60 * MIN);
    sim.say("first request of the session");
    sim.settle();
    let restart_at = EVENING_MS;
    for n in 0..25u64 { sim.heard_while_off(restart_at - (25 - n) * MIN, "User", &format!("background {n:02}")); }
    sim.clock.set(restart_at - 10 * S);
    sim.compaction();
    sim.clock.set(restart_at - 2 * S);
    let queued = "please also check the release notes";
    sim.say(queued);
    sim.t3(T3Event::Waiting { thread: "thread-q".into(), run: "r2".into(), owned: false, requests: vec!["q9".into()], text: "Thread Q asks which branch to use".into() });
    sim.notice("terminal:worker-2:r1:completed", "Worker worker-2 finished: lint clean");
    assert!(sim.wakes().iter().all(|i| !i.text.contains(queued)), "queued behind the compaction");
    sim.clock.set(restart_at);
    sim.world.lock().unwrap().context_tokens = Some(74_800);
    sim.restart(None);
    sim.settle();
    let compactions = sim.world.lock().unwrap().compactions.clone();
    let start_append = sim.appends().into_iter().last().unwrap();
    assert!(compactions.iter().any(|c| c.0 >= restart_at && c.1 <= start_append.at), "compaction first: {compactions:?} then {}", start_append.at);
    let text = &start_append.text;
    assert!(!text.contains(queued), "his request is not history: {text}");
    assert!(!text.contains("lint clean"), "the worker's result is a wake, not history: {text}");
    assert!(text.contains("background 24") && text.ends_with(&restart_message(restart_at)), "{text}");
    assert!(!text.contains("which branch"), "the open question waits for User");
    let after: Vec<_> = sim.wakes().into_iter().filter(|i| i.at >= restart_at).collect();
    assert_eq!(after.len(), 2, "{:#?}", texts(&after));
    assert!(after[0].text.contains(queued) && after[0].kind == InputKind::Prompt && after[0].at >= start_append.at, "{:#?}", texts(&after));
    assert!(after[1].text.contains("which branch") && after[1].kind == InputKind::Prompt, "his request counts as him speaking");
    assert!(after[1].text.contains("lint clean"), "the result goes out once, with the question: {:#?}", texts(&after));
    sim.advance(5 * MIN);
    assert_eq!(sim.turns().iter().filter(|t| t.0 >= restart_at).count(), 2, "nothing else by itself");
    sim.say("ok, I'm back");
    sim.settle();
    let after: Vec<_> = sim.wakes().into_iter().filter(|i| i.at >= restart_at).collect();
    assert_eq!(after.len(), 3, "{:#?}", texts(&after));
    assert!(after[2].text.contains("I'm back") && !after[2].text.contains(queued));
    assert!(sim.inputs().iter().filter(|i| i.text.contains(queued)).count() == 1);
    sim.check_global();
}

/// S20 / S21 (E4.1, E4.3): a model switch and a reload are starts: the reload's reason sits on the line before the
/// restart message, nothing continues by itself.
#[test]
fn s20_s21_model_switch_and_reload_are_starts() {
    let mut sim = Sim::new(EVENING_MS);
    sim.say("switch to the other model after this");
    sim.settle();
    sim.advance(MIN);
    sim.restart(None);
    assert!(sim.appends().last().unwrap().text.ends_with(&restart_message(sim.now())));
    sim.advance(MIN);
    sim.restart(Some("Reloaded to pick up new tools"));
    let text = sim.appends().last().unwrap().text.clone();
    assert!(text.ends_with(&format!("Reloaded to pick up new tools\n{}", restart_message(sim.now()))), "{text}");
    sim.advance(10 * MIN);
    assert_eq!(sim.turns().len(), 1, "no automatic continuation");
    sim.check_global();
}

/// S22 (E4.6): two starts fail before their context is added; the third succeeds: the restart message is in the
/// conversation once.
#[test]
fn s22_start_retries_append_once() {
    let mut sim = Sim::new(EVENING_MS);
    sim.say("hi");
    sim.settle();
    for _ in 0..2 {
        sim.advance(MIN);
        sim.world.lock().unwrap().failing_appends = 1;
        sim.restart(None);
    }
    sim.advance(MIN);
    sim.restart(None);
    let appends = sim.appends();
    assert_eq!(appends.iter().filter(|a| a.at > EVENING_MS).count(), 1, "{:#?}", texts(&appends));
    sim.check_global();
}

/// S24 (E2.1): the user speaks during a tool turn: one steer, taken at the next tool boundary, never a second turn.
#[test]
fn s24_owner_speech_steers_the_running_turn() {
    let mut sim = Sim::new(EVENING_MS);
    sim.world.lock().unwrap().replies.push_back(vec![Step::Think(500), Step::Tool("bash", 4_000), Step::Tool("speak", 1_000)]);
    sim.say("run the tests");
    sim.advance(S);
    sim.say("only the fast ones");
    let steers: Vec<_> = sim.wakes().into_iter().filter(|i| i.kind == InputKind::Steer).collect();
    assert_eq!(steers.len(), 1);
    sim.settle();
    assert_eq!(sim.turns().len(), 1);
    sim.check_global();
}

/// S25, S26 (E3.1–E3.3): Stop while thinking and during a tool: the turn aborts, its effect stays recorded once,
/// the delivery settles and is never resent, the agent is idle.
#[test]
fn s25_s26_stop_while_thinking_and_during_a_tool() {
    let mut sim = Sim::new(EVENING_MS);
    sim.world.lock().unwrap().replies.push_back(vec![Step::Think(10_000)]);
    sim.say("think hard about this");
    sim.advance(2 * S);
    sim.stop();
    assert_eq!(sim.bridge.status, crate::Status::Idle);
    assert!(sim.resolutions.iter().any(|r| r.outcome == crate::Resolved::Stopped));
    sim.world.lock().unwrap().replies.push_back(vec![Step::Think(500), Step::Tool("bash", 30_000)]);
    sim.say("sleep 30 in the shell");
    sim.advance(2 * S);
    sim.stop();
    sim.settle();
    assert_eq!(sim.effects().len(), 1, "the started command is recorded once, never replayed");
    assert_eq!(sim.wakes().len(), 2, "no resend after Stop");
    assert_eq!(sim.bridge.status, crate::Status::Idle);
    sim.check_global();
}

/// S27 (E3.4, E3.6): Stop during a compaction with a the user wake and a T3 question queued behind it. No turn when the
/// compaction ends; the queued wake is not delivered but its words stay context for the user's next wake; the question
/// becomes context; the user's next words start a fresh turn.
#[test]
fn s27_stop_during_compaction() {
    let mut sim = Sim::new(EVENING_MS);
    sim.say("hello");
    sim.settle();
    sim.compaction();
    sim.advance(2 * S);
    let early = "summarise the meeting";
    sim.say(early);
    sim.t3(T3Event::Waiting { thread: "thread-c".into(), run: "r1".into(), owned: false, requests: vec!["q1".into()], text: "Thread C asks for the API key name".into() });
    sim.advance(2 * S);
    sim.stop();
    let stop_at = sim.now();
    sim.advance(20 * S);
    assert_eq!(sim.turns().iter().filter(|t| t.0 >= stop_at).count(), 0, "no turn when the compaction ends");
    assert!(sim.appends().iter().any(|a| a.text.contains("API key name")), "the question is context");
    sim.say("actually, just the action items");
    let wake = sim.wakes().last().unwrap().clone();
    assert_eq!(wake.kind, InputKind::Prompt);
    assert!(wake.text.contains("action items") && wake.text.contains(early), "the stopped words are context: {}", wake.text);
    sim.settle();
    sim.check_global();
}

/// S29 (E3.5): Stop, then the user speaks 150 ms later: a fresh turn with only the new words.
#[test]
fn s29_stop_then_immediate_speech_starts_fresh() {
    let mut sim = Sim::new(EVENING_MS);
    sim.world.lock().unwrap().replies.push_back(vec![Step::Think(500), Step::Tool("bash", 20_000)]);
    sim.say("build everything");
    sim.advance(2 * S);
    sim.stop();
    sim.advance(150);
    sim.say("no, only the agent crate");
    let last = sim.wakes().last().unwrap().clone();
    assert_eq!(last.kind, InputKind::Prompt);
    assert!(!last.text.contains("build everything"), "{}", last.text);
    sim.settle();
    sim.check_global();
}

/// S31 / S59 (E5.5, E10.1): a thinking-signature 400 that still reaches the harness is resent once; each of the user's
/// sentences reaches the model once besides a marked resend. Before the turn acted, his second sentence restarts it
/// (night pass H2), so the first is already in the conversation and only the restarted request's words are resent;
/// after a tool ran, the second sentence steers, the tool-bearing first is settled (never replayed) and the
/// steered one is resent once.
#[test]
fn s31_s59_signature_400_mid_steer_is_resent_once() {
    let signature = "400 invalid_request_error: messages.5.content.0: Invalid `signature` in `thinking` block";
    // Before any tool: restarted, then the restarted request fails.
    let mut sim = Sim::new(EVENING_MS);
    sim.world.lock().unwrap().replies.extend([vec![Step::Think(3_000), Step::Fail(signature.into())], vec![Step::Think(3_000), Step::Fail(signature.into())]]);
    sim.say("draw a circle around the failed turn");
    sim.advance(S);
    sim.say("the red one");
    sim.settle();
    let circle = sim.wakes().into_iter().filter(|i| i.text.contains("draw a circle")).count();
    let red = sim.wakes().into_iter().filter(|i| i.text.contains("the red one")).count();
    assert_eq!((circle, red), (1, 2), "the first is in the conversation already; the failed request's words are resent once");
    assert_eq!(sim.resolutions.iter().filter(|r| r.outcome == crate::Resolved::Retrying).count(), 1);
    sim.check_global();
    // After a tool: the second sentence steers into the request that fails.
    let mut sim = Sim::new(EVENING_MS);
    sim.world.lock().unwrap().replies.push_back(vec![Step::Think(500), Step::Tool("look", 500), Step::Think(3_000), Step::Fail(signature.into())]);
    sim.say("draw a circle around the failed turn");
    sim.advance(1_500);
    sim.say("the red one");
    assert_eq!(sim.wakes().last().unwrap().kind, InputKind::Steer);
    sim.settle();
    let circle = sim.wakes().into_iter().filter(|i| i.text.contains("draw a circle")).count();
    let red = sim.wakes().into_iter().filter(|i| i.text.contains("the red one")).count();
    assert_eq!((circle, red), (1, 2), "the tool-bearing request is never replayed; the steered words are resent once");
    assert_eq!(sim.resolutions.iter().filter(|r| r.outcome == crate::Resolved::Retrying).count(), 1);
    assert_eq!(sim.resolutions.iter().filter(|r| r.outcome == crate::Resolved::SettledAfterError).count(), 1);
    sim.check_global();
}

/// S34 / S35 (E5.6, E9.1): a wake during an 18 s compaction is queued and goes out once at its end, words and image
/// together.
#[test]
fn s34_s35_wake_during_compaction_goes_out_once_with_its_image() {
    let mut sim = Sim::new(EVENING_MS);
    sim.compaction();
    sim.advance(3 * S);
    sim.say_with("look at the screen right now", &[png(1)]);
    assert!(sim.wakes().is_empty());
    sim.advance(16 * S);
    let wakes = sim.wakes();
    assert_eq!(wakes.len(), 1);
    assert_eq!(wakes[0].images, 1);
    assert!(wakes[0].text.contains("look at the screen right now"));
    assert!(wakes[0].at >= EVENING_MS + 18 * S);
    sim.settle();
    sim.check_global();
}

/// S44 (E9.1, incident 17:45:04–17:45:15): the follow-up trigger sends the user's words at once and the screenshot is
/// ready 1.4 s later, never a message with an empty transcript; a screenshot arriving with no turn running starts
/// nothing, and a notice during the turn still waits for it to settle. While the turn has not acted, the screenshot
/// restarts it, so the one answer sees words and image together (night pass H2); once a tool ran, it steers the
/// running turn as one prompt with its caption and image.
#[test]
fn s44_late_screenshot_steers_the_running_turn() {
    let words = "look at the screen right now. Island test complete and it's still there.";
    let caption = format!("Screenshot for: {words}");
    // Still thinking: the turn restarts with the screenshot.
    let mut sim = Sim::new(EVENING_MS);
    sim.world.lock().unwrap().replies.push_back(vec![Step::Think(2_000), Step::Tool("look", 1_000), Step::Tool("speak", 1_000)]);
    sim.say(words);
    sim.advance(1_400);
    assert!(sim.late_screenshot(&caption, &[png(7)]), "a turn is running: the screenshot joins it");
    // A notice in the same turn never steers (E2.2).
    sim.notice("terminal:t:r:completed", "Worker t run r completed.");
    let shot = sim.wakes().into_iter().find(|i| i.images == 1).expect("the screenshot reached Pi");
    assert_eq!(shot.kind, InputKind::Prompt, "{shot:#?}");
    assert!(shot.text.contains(&caption), "{}", shot.text);
    assert_eq!(sim.world.lock().unwrap().rejoined.len(), 1, "the words' turn restarted, unanswered");
    sim.settle();
    assert_eq!(sim.heard.iter().filter(|h| h.final_reply).count(), 2, "one answer for words and screenshot, one for the notice");
    assert!(sim.wakes().iter().all(|i| !i.text.trim().is_empty()));
    assert!(sim.wakes().iter().all(|i| !(i.kind == InputKind::Steer && i.text.contains("[Heimdall notice"))), "a notice steered");
    sim.check_global();
    // After a tool: the screenshot steers.
    let mut sim = Sim::new(EVENING_MS);
    sim.world.lock().unwrap().replies.push_back(vec![Step::Think(1_000), Step::Tool("look", 1_000), Step::Think(1_000), Step::Tool("speak", 1_000)]);
    sim.say(words);
    sim.advance(1_400);
    assert!(sim.late_screenshot(&caption, &[png(7)]), "a turn is running: the screenshot steers it");
    sim.notice("terminal:t:r:completed", "Worker t run r completed.");
    let steers: Vec<_> = sim.wakes().into_iter().filter(|i| i.kind == InputKind::Steer).collect();
    assert_eq!(steers.len(), 1, "{steers:#?}");
    assert_eq!(steers[0].images, 1);
    assert!(steers[0].text.contains(&caption), "{}", steers[0].text);
    let steer_id = steers[0].id.clone().expect("a delivery id");
    sim.settle();
    // The notice runs its own turn once the user's settles; the screenshot never does.
    assert!(sim.turns().iter().all(|t| t.1 != steer_id), "the screenshot is no turn of its own");
    assert_eq!(sim.turns().len(), 2, "User's turn, then the notice's");
    assert!(sim.wakes().iter().all(|i| !i.text.trim().is_empty()));
    // Idle: held for the next wake, nothing starts.
    let before = sim.inputs().len();
    assert!(!sim.late_screenshot("Screenshot for: and this", &[png(8)]));
    assert_eq!(sim.inputs().len(), before, "a held screenshot is no wake");
    sim.check_global();
}

/// S39 (E6.3, E6.4): the same T3 question from two passes is one delivery; the same words under random ids are too.
#[test]
fn s39_content_derived_ids_and_repeat_words() {
    let mut sim = Sim::new(EVENING_MS);
    sim.say("go");
    sim.settle();
    let q = T3Event::Waiting { thread: "t".into(), run: "r".into(), owned: false, requests: vec!["a".into()], text: "Thread T asks: merge now?".into() };
    assert!(sim.t3(q.clone()));
    assert!(!sim.t3(q));
    assert!(!sim.notice("t3-activity:random-2", "Thread T asks: merge now?"));
    sim.settle();
    assert_eq!(sim.wakes().iter().filter(|i| i.text.contains("merge now?")).count(), 1);
    sim.check_global();
}

/// S58 (E10.2, E6.6): a 400 "image exceeds limit" on the request that carried the user's steered screenshot: the turn
/// fails once, the island gets words, the steered words ride the next wake once without that image, no resend.
#[test]
fn s58_provider_400_mid_steer() {
    let mut sim = Sim::new(EVENING_MS);
    let rejected = r#"400 {"type":"error","error":{"type":"invalid_request_error","message":"messages.7.content.1.image.source.base64: image exceeds 5 MB maximum"}}"#;
    sim.world.lock().unwrap().replies.push_back(vec![Step::Think(500), Step::Tool("bash", 2_000), Step::Think(1_000), Step::Fail(rejected.into())]);
    sim.say("check the CI page");
    sim.advance(S);
    let steered = "and this screenshot too";
    sim.say_with(steered, &[png(7)]);
    sim.settle();
    let rejections: Vec<_> = sim.resolutions.iter().filter(|r| r.outcome == crate::Resolved::Rejected).collect();
    assert_eq!(rejections.len(), 1, "{:?}", sim.resolutions);
    assert!(rejections[0].reason.contains("screenshot was too large"));
    assert_eq!(sim.wakes().iter().filter(|i| i.text.contains(steered)).count(), 1, "not resent");
    sim.say("try again without it");
    let next = sim.wakes().last().unwrap().clone();
    assert!(next.text.contains(steered) && next.images == 0, "{next:?}");
    sim.settle();
    sim.check_global();
}

/// S60 (E10.3, E4): the key is rejected at the start; the user speaks twice; the key is connected two minutes later: no
/// restart loop, the restart message goes in, then both utterances go out (they came within `START_RESUME_MS` of the
/// start and never reached the model: night pass 3, bug note 20 Oct 22:12:57), and his next words wake.
#[test]
fn s60_key_rejected_then_connected() {
    let mut sim = Sim::new(EVENING_MS);
    sim.bridge.status = crate::Status::WaitingProvider("Anthropic: key rejected".into());
    sim.say("are you there");
    sim.advance(30 * S);
    sim.say("hello?");
    sim.advance(90 * S);
    assert!(sim.wakes().is_empty(), "waiting, nothing delivered");
    sim.restart(None);
    let start = sim.appends().last().unwrap().text.clone();
    assert!(!start.contains("are you there") && !start.contains("hello?") && start.ends_with(&restart_message(sim.now())), "{start}");
    let wakes = sim.wakes();
    assert!(wakes.iter().any(|w| w.text.contains("are you there")) && wakes.iter().any(|w| w.text.contains("hello?")), "{:#?}", texts(&wakes));
    assert!(wakes.iter().all(|w| w.at >= sim.appends().last().unwrap().at));
    sim.settle();
    sim.say("ok now?");
    let last = sim.wakes().last().unwrap().clone();
    assert!(last.text.contains("ok now?") && !last.text.contains("are you there"), "{last:?}");
    sim.settle();
    sim.check_global();
}

/// S63 (E10.7): no response headers in 120 s: the turn fails, nothing is resent at once, the words ride the next wake.
#[test]
fn s63_no_headers_in_120_seconds() {
    let mut sim = Sim::new(EVENING_MS);
    sim.world.lock().unwrap().replies.push_back(vec![Step::Think(120_000), Step::Fail("Anthropic did not answer within 120 seconds".into())]);
    sim.say("what's the weather");
    sim.settle();
    assert_eq!(sim.wakes().len(), 1);
    sim.say("hello?");
    assert!(sim.wakes()[1].text.contains("what's the weather"));
    sim.settle();
    sim.check_global();
}

/// E2.4: a notice that waited more than 30 minutes (the agent was blocked) is history when it can go out: appended
/// as context, never a turn of its own. A task's result is the exception (pi-exit, 21 Oct): it is still the answer
/// the agent waits for, so it wakes it once, like a scheduled wake.
#[test]
fn e2_4_a_notice_older_than_30_minutes_becomes_context() {
    let mut sim = Sim::new(EVENING_MS);
    sim.say("start the workers");
    sim.settle();
    sim.bridge.status = crate::Status::WaitingEngine;
    sim.notice("t3:mcp:w1:r1:done", "T3 update:\n- 'w1' (yours) finished an hour ago");
    sim.notice("terminal:w2:r1:completed", "Task w2 completed long ago. Final reply:\n\nDone.");
    sim.advance(31 * MIN);
    assert!(sim.inputs().iter().all(|i| !i.text.contains("an hour ago") && !i.text.contains("completed long ago")));
    sim.bridge.engine_ready();
    sim.pump();
    let late: Vec<_> = sim.inputs().into_iter().filter(|i| i.text.contains("an hour ago")).collect();
    assert_eq!(late.len(), 1);
    assert_eq!(late[0].kind, InputKind::Append);
    let result: Vec<_> = sim.inputs().into_iter().filter(|i| i.text.contains("completed long ago")).collect();
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].kind, InputKind::Prompt);
    assert!(!result[0].text.contains("an hour ago"), "the old update is not part of the result's turn");
    assert_eq!(sim.turns().len(), 2);
    sim.check_global();
}

/// TOOLING §2.5 random interleavings: seeded mixes of the user's speech, asides, notices of every kind, provider
/// failures, compactions, Stop, restarts and crashes, checked only against the global properties: G1, G2, no notice
/// ever steers a turn, nothing but a task's result starts a turn between a start and the user's next addressed words
/// (pi-exit, 21 Oct: a result wakes the agent like a scheduled wake), no result reaches the model twice, and nothing
/// delivered before a start is delivered again after it. `HEIMDALL_SIM_CASES` raises the number of seeds.
#[test]
fn random_interleavings_keep_the_global_properties() {
    let cases: u64 = std::env::var("HEIMDALL_SIM_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(1000);
    for seed in 0..cases {
        let mut x = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let mut next = |n: u64| { x ^= x << 13; x ^= x >> 7; x ^= x << 17; x % n };
        let mut sim = Sim::new(EVENING_MS);
        let mut starts = vec![EVENING_MS];
        let mut counter = 0;
        for _ in 0..40 {
            counter += 1;
            match next(12) {
                0 | 1 | 2 => { sim.say(&format!("request {seed}-{counter}")); }
                3 => { sim.aside("User", &format!("aside {seed}-{counter}")); }
                4 => { sim.notice(&format!("terminal:w{counter}:r1:completed"), &format!("worker {seed}-{counter} finished")); }
                5 => { sim.t3(T3Event::Waiting { thread: format!("t{counter}"), run: "r".into(), owned: false, requests: vec![format!("q{counter}")], text: format!("thread {seed}-{counter} asks") }); }
                6 => { sim.notice(&format!("fastread-{counter}"), &format!("Fast read: read at {} wpm.", 200 + counter)); }
                7 => {
                    let fail = ["400 invalid_request_error: Invalid `signature` in `thinking` block", "529 overloaded_error", r#"400 {"error":{"type":"invalid_request_error","message":"image exceeds 5 MB maximum"}}"#][next(3) as usize];
                    sim.world.lock().unwrap().replies.push_back(vec![Step::Think(500 + next(3_000)), Step::Fail(fail.into())]);
                }
                8 => sim.stop(),
                9 => sim.compaction(),
                10 => {
                    if next(2) == 0 { sim.world.lock().unwrap().crash(); }
                    sim.advance(next(5_000));
                    sim.restart(None);
                    starts.push(sim.now());
                }
                _ => sim.world.lock().unwrap().replies.push_back(vec![Step::Think(1_000), Step::Tool("bash", next(20_000)), Step::Tool("speak", 1_000)]),
            }
            sim.advance(next(45_000));
        }
        sim.settle();
        sim.check_global();
        let inputs = sim.inputs();
        assert!(inputs.iter().all(|i| !(i.kind == InputKind::Steer && i.text.contains("[Heimdall notice"))), "seed {seed}: a notice steered");
        for n in 1..=counter {
            // Once, plus one resend per provider failure in the same process (E1.14), never across a start.
            let header = format!("[Heimdall notice: terminal:w{n}:r1:completed]");
            let copies = inputs.iter().filter(|i| i.text.contains(&header)).count();
            // A result coalesced into another notice's delivery is resent with that delivery.
            let carriers: std::collections::HashSet<_> = inputs.iter().filter(|i| i.text.contains(&header)).filter_map(|i| i.id.clone()).collect();
            let resends = sim.resolutions.iter().filter(|r| carriers.contains(&r.id) && r.outcome == crate::Resolved::Retrying).count();
            assert!(copies <= 1 + resends, "seed {seed}: result w{n} reached the model {copies} times: {:#?}\nstarts {starts:?}\nresolutions {:#?}",
                inputs.iter().filter(|i| i.text.contains(&header)).map(|i| (i.at, i.kind.clone(), i.id.clone(), i.generation)).collect::<Vec<_>>(), sim.resolutions);
        }
        for (n, start) in starts.iter().enumerate() {
            let end = starts.get(n + 1).copied().unwrap_or(u64::MAX);
            // His own request from just before the start that never reached the model goes out after the start
            // context (night pass 3): it is his addressed words, like anything he says after the start.
            let recent: Vec<String> = sim.said.iter().filter(|s| s.3 && s.1 < *start && start - s.1 <= crate::notice::START_RESUME_MS)
                .map(|s| format!("User] {}", s.2)).collect();
            let resumed = inputs.iter().filter(|i| i.at >= *start && i.at < end && i.kind != InputKind::Append && recent.iter().any(|l| i.text.contains(l.as_str())))
                .map(|i| i.at).min();
            let first_owner = sim.said.iter().filter(|s| s.3 && s.1 >= *start && s.1 < end).map(|s| s.1).chain(resumed).min().unwrap_or(end);
            let early: Vec<_> = sim.turns().into_iter().filter(|t| t.0 >= *start && t.0 < first_owner && !t.1.starts_with("terminal:")).collect();
            assert!(early.is_empty(), "seed {seed}: a turn started after the start at {start} before User spoke: {early:?}");
            let before: std::collections::HashSet<_> = inputs.iter().filter(|i| i.at < *start).filter_map(|i| i.id.clone()).collect();
            let crossed: Vec<_> = inputs.iter().filter(|i| i.at >= *start && i.id.as_ref().is_some_and(|id| before.contains(id))).collect();
            assert!(crossed.is_empty(), "seed {seed}: a delivery crossed the start at {start}: {crossed:#?}\nresolutions {:#?}", sim.resolutions);
        }
    }
}

/// 17:mm:ss on 18 Oct 2026 (CEST).
fn at(min: u64, sec: u64) -> u64 {
    EVENING_MS + (min - 39) * MIN + sec * S
}
fn mobius(sim: &mut Sim, run: u64) {
    let text = "T3 update: 'Mobius' completed: No, it's not me, it's the Mobius bridge".to_string();
    // The T3 policy drops it (the thread is not Heimdall's) …
    assert!(!sim.t3(T3Event::Finished { thread: "mcp:9209239e".into(), run: format!("run-{run}"), owned: false, text: text.clone() }));
    // … and if an old observer still sent its random-id line, the harness never lets it steer or repeat.
    sim.notice(&format!("t3-activity:{run}-{}", sim.now()), &text);
}

/// S64 — the regression gate: 18 Oct 17:39–17:54 replayed on the virtual clock from the evening's evidence
/// (EXPECTED.md Appendix A: archive timelines, Pi session entries, spool rows), the harness's part. Against the new
/// rules: zero turns before the user's 17:42:33 request; the stale 13:50 redo and the 13:52 spool row never act as
/// requests; no "Done. I circled" (no tool before 17:42:33); zero Mobius deliveries as turns; no notice steers the
/// icon test and no the user request waits behind a notice turn; the spoken stop at 17:44:06 is a harness Stop, not a
/// turn; the 17:45:04 words and their screenshot go out in one delivery and no input is image-only; the 17:46:14 and
/// 17:46:22 asides ride the 17:46:28 wake with their times; the 17:54:46 crash replays nothing.
/// Island, speed-sample, speaking-animation and Agent-window checks of S64 belong to their own domains' replays.
#[test]
fn s64_replay_of_18_oct_17_39_to_17_54() {
    // The afternoon (13:35–13:52, Haiku): the redo request whose turn failed twice with the thinking 400, and an
    // aside whose decision was pending when Listening went off.
    let mut sim = Sim::new(EVENING_MS - 4 * 60 * MIN);
    sim.clock.set(EVENING_MS - (3 * 60 + 49) * MIN + 15 * S); // 13:50:15
    let signature = "400 invalid_request_error: messages.41.content.0: Invalid `signature` in `thinking` block";
    sim.world.lock().unwrap().replies.extend([vec![Step::Think(800), Step::Fail(signature.into())], vec![Step::Think(800), Step::Fail(signature.into())]]);
    let redo = sim.say("can you do it again, I'm back on the T3 screen");
    sim.settle();
    sim.clock.set(EVENING_MS - (3 * 60 + 47) * MIN + 4 * S); // 13:52:04
    let stale_aside = sim.aside("User", "okay that's enough for now");
    let afternoon_inputs = sim.inputs().len();

    // 17:41:07 Heimdall turned on (Opus via Anthropic).
    sim.clock.set(at(41, 7));
    sim.restart(None);
    let start = sim.appends().last().unwrap().clone();
    assert_eq!(start.text, restart_message(at(41, 7)), "nothing from 13:50 is recent history");
    mobius(&mut sim, 628); // 17:41:59 is reported at its own time below
    sim.clock.set(at(41, 59));
    mobius(&mut sim, 629);

    // 17:42:33 the user's first request.
    sim.clock.set(at(42, 33));
    assert_eq!(sim.turns().iter().filter(|t| t.0 >= at(41, 7)).count(), 0, "zero turns before 17:42:33");
    assert!(sim.effects().iter().all(|e| e.at < at(41, 7)), "no look/point/speak: no 'Done. I circled the failed turn'");
    sim.say("Heimdall, are you there? Test the island for me in a second");
    let first = sim.wakes().into_iter().find(|i| i.at == at(42, 33)).expect("first wake");
    assert!(!first.text.contains("can you do it again") && !first.text.contains("enough for now"), "{}", first.text);
    sim.settle();

    // 17:43:01 the icon test: six island notices 3-6 s apart while Mobius lines arrive (17:43:07, 17:43:37).
    sim.clock.set(at(43, 1));
    sim.world.lock().unwrap().replies.push_back(vec![Step::Think(2_000), Step::Tool("island_notice", 4_000), Step::Tool("island_notice", 5_000),
        Step::Tool("island_notice", 3_000), Step::Tool("island_notice", 6_000), Step::Tool("island_notice", 4_000), Step::Tool("island_notice", 3_000)]);
    sim.say("show me every island notice type one after another");
    sim.advance(6 * S);
    mobius(&mut sim, 630);
    sim.advance(2 * S);
    let next_request = sim.now();
    sim.say("make the warning one orange");
    sim.advance(22 * S);
    mobius(&mut sim, 631);
    // 17:44:06 the spoken stop: the harness stops; the phrase is not a turn.
    sim.clock.set(at(44, 6));
    sim.world.lock().unwrap().replies.push_back(vec![Step::Think(1_000), Step::Tool("island_notice", 20_000)]);
    sim.say("now list them again slowly");
    sim.advance(2 * S);
    let stop_at = at(44, 8);
    sim.clock.set(stop_at);
    sim.aside("User", "Can you stop completely? I want to talk to you.");
    sim.stop();
    sim.advance(10 * S);
    assert_eq!(sim.turns().iter().filter(|t| t.0 >= stop_at && t.0 < at(44, 50)).count(), 0, "no turn after the spoken stop");

    // 17:44:56 "…look at the screen right now. Island test complete and it's still there." with its screenshot.
    sim.clock.set(at(45, 4));
    sim.say_with("look at the screen right now. Island test complete and it's still there.", &[png(5)]);
    sim.advance(31 * S);
    mobius(&mut sim, 632); // 17:45:35
    // 17:46:14 and 17:46:22: not-for-Heimdall decisions (the name trigger lacked "Hiimdoll"), then "Heimdall."
    sim.clock.set(at(46, 14));
    let missed = "I think it didn't hear me at all";
    sim.aside("User", missed);
    sim.clock.set(at(46, 22));
    let hiimdoll = "Oh, and now you couldn't hear me. Hiimdoll, come on.";
    sim.aside("User", hiimdoll);
    sim.clock.set(at(46, 28));
    sim.say("Heimdall.");
    let wake = sim.wakes().into_iter().find(|i| i.at == at(46, 28)).expect("17:46:28 wake");
    assert!(wake.text.contains(&format!("[{} Mic · User] {missed}", hms(at(46, 14)))) && wake.text.contains(&format!("[{} Mic · User] {hiimdoll}", hms(at(46, 22)))), "{}", wake.text);
    sim.settle();
    for (m, s, run) in [(47, 59, 633), (52, 39, 640)] {
        sim.clock.set(at(m, s));
        mobius(&mut sim, run);
    }
    // 17:50:25 a vague addressed remark; 17:52–17:54 asides while marking the screen.
    sim.clock.set(at(50, 25));
    sim.say("Yeah, I can, but that's strange");
    sim.settle();
    for n in 0..5 { sim.clock.set(at(52, 50) + n * 20 * S); sim.aside("User", &format!("this one is wrong too, number {n}")); }
    sim.clock.set(at(54, 30));
    sim.world.lock().unwrap().replies.push_back(vec![Step::Think(1_000), Step::Tool("speak", 30_000)]);
    sim.say("so tell me what's wrong with all of them");
    sim.clock.set(at(54, 45));
    mobius(&mut sim, 658);
    sim.advance(S);
    // 17:54:46 the crash; the supervisor restarts Pi.
    sim.world.lock().unwrap().crash();
    let effects_before = sim.effects().len();
    sim.advance(3 * S);
    sim.restart(None);
    sim.advance(5 * MIN);

    let evening: Vec<Input> = sim.inputs().into_iter().skip(afternoon_inputs).collect();
    let mobius_turns = evening.iter().filter(|i| i.kind != InputKind::Append && i.text.contains("'Mobius' completed")).count();
    assert!(mobius_turns <= 1, "Mobius woke the agent {mobius_turns} times");
    assert!(evening.iter().all(|i| !(i.kind == InputKind::Steer && i.text.contains("[Heimdall notice"))), "a notice steered a turn");
    // Every the user request reached Pi at the moment he finished it (as a prompt or a steer): none waited behind a notice.
    for (_, said_at, words, addressed) in sim.said.iter().filter(|s| s.3 && s.1 >= at(41, 7)) {
        let _ = addressed;
        let got = evening.iter().find(|i| i.kind != InputKind::Append && i.text.contains(&format!("User] {words}"))).unwrap_or_else(|| panic!("{words} never reached Pi"));
        assert_eq!(got.at, *said_at, "{words:?} waited {} ms", got.at - said_at);
    }
    assert!(sim.inputs().iter().filter(|i| i.at == next_request).all(|i| i.kind == InputKind::Steer), "17:43 request steers the icon test");
    assert!(evening.iter().all(|i| !i.text.contains("Can you stop completely") || i.at > stop_at + 10 * S), "the stop phrase is not a turn of its own");
    let image_inputs: Vec<_> = evening.iter().filter(|i| i.images > 0).collect();
    assert_eq!(image_inputs.len(), 1);
    assert!(image_inputs[0].text.contains("look at the screen right now") && image_inputs[0].text.contains("Island test complete"), "words and screenshot together");
    assert_eq!(sim.effects().len(), effects_before, "the crash replays nothing");
    let restart = sim.appends().last().unwrap().text.clone();
    assert!(restart.ends_with(&restart_message(sim.started_at)));
    assert!(sim.dropped(&redo).is_some() && sim.dropped(&stale_aside).is_some(), "the afternoon is archive-only");
    sim.check_global();
}

/// Where each lab/harness/SCENARIOS.md id is checked. `sim:` names a test in this file (run on the virtual clock);
/// `test:` an existing test elsewhere; `domain:` a scenario owned by another domain's simulator (island, images,
/// speech, decider, T3 observer); `missing:` not covered yet, with the reason.
const COVERAGE: &[(&str, &str)] = &[
    ("S01", "sim:s01_addressed_speech_wakes_once"),
    ("S02", "sim:s02_asides_carry_times_and_age_out_after_30_minutes"),
    ("S03", "domain:decide name variants (observed_name_variants_require_a_sentence_start)"),
    ("S04", "test:interrupted_synthesized_playback_arms_follow_up_at_actual_output_end (app pipeline)"),
    ("S05", "sim:s05_a_start_never_speaks_or_wakes"),
    ("S06", "sim:s06_repeating_non_owned_thread_never_wakes"),
    ("S07", "sim:s07_question_wakes_once_per_request_set"),
    ("S08", "sim:s08_approval_wait_wakes_once"),
    ("S09", "sim:s09_owned_worker_result_wakes_once"),
    ("S10", "sim:s10_worker_notices_queue_behind_owners_turn_and_answer_clicks_steer"),
    ("S11", "sim:s11_context_only_notices_are_appends"),
    ("S12", "domain:speech own-voice filter (own_voice_never_interrupts)"),
    ("S13", "sim:s13_notices_coalesce_and_pace_at_one_turn_per_30_seconds"),
    ("S14", "sim:s14_turn_on_after_hours_off"),
    ("S15", "sim:s15_launch_with_a_stale_request_and_notices_does_nothing"),
    ("S16", "sim:s16_spool_leftovers_from_before_the_start_are_history"),
    ("S17", "sim:s17_a_failed_delivery_is_resent_in_process_and_never_across_a_start"),
    ("S18", "sim:s18_interrupted_tool_turn_is_never_replayed"),
    ("S19", "sim:s19_restart_with_everything_pending"),
    ("S20", "sim:s20_s21_model_switch_and_reload_are_starts"),
    ("S21", "sim:s20_s21_model_switch_and_reload_are_starts"),
    ("S22", "sim:s22_start_retries_append_once"),
    ("S23", "test:every_restart_path_resumes_the_same_pi_session (app bridge)"),
    ("S24", "sim:s24_owner_speech_steers_the_running_turn"),
    ("S25", "sim:s25_s26_stop_while_thinking_and_during_a_tool"),
    ("S26", "sim:s25_s26_stop_while_thinking_and_during_a_tool"),
    ("S27", "sim:s27_stop_during_compaction"),
    ("S28", "sim:s64_replay_of_18_oct_17_39_to_17_54"),
    ("S29", "sim:s29_stop_then_immediate_speech_starts_fresh"),
    ("S30", "test:history_near_the_threshold_compacts_after_an_answer_not_inside_the_next (app bridge)"),
    ("S31", "sim:s31_s59_signature_400_mid_steer_is_resent_once"),
    ("S32", "test:sim::night_scenarios::n10_first_words_after_agent_on_never_wait_for_a_cold_cache_compaction, n11_…, n13_an_hour_idle_compacts_while_away (night pass H1: compacted while idle, never before the wake)"),
    ("S33", "test:failed_idle_compaction_at_launch_keeps_the_agent_up_with_one_attempt (app bridge)"),
    ("S34", "sim:s34_s35_wake_during_compaction_goes_out_once_with_its_image"),
    ("S35", "sim:s34_s35_wake_during_compaction_goes_out_once_with_its_image"),
    ("S36", "missing:compaction instructions live in Pi's extension; needs a fake-provider capture of the summary request"),
    ("S37", "test:cold_cache_compaction_before_a_wake_shows_as_compacting (agent harness); island state: domain island"),
    ("S38", "test:event_wait_resolves_without_polling_and_watcher_deduplicates_terminal (t3)"),
    ("S39", "sim:s39_content_derived_ids_and_repeat_words"),
    ("S40", "test:ambiguous_tool_success_does_not_erase_effect_guard (agent)"),
    ("S41", "test:voice_wakes_queued_behind_busy_turn_or_engine_down_cannot_cross_off_on (agent)"),
    ("S42", "test:ten_wakes_coalesce_ordered_without_repeating_completed_observations (agent)"),
    ("S43", "test:agent_status_becomes_island_health (app)"),
    ("S44", "sim:s44_late_screenshot_steers_the_running_turn"),
    ("S45", "domain:images (attachment bound to its utterance)"),
    ("S46", "domain:images (attachment expiry)"),
    ("S47", "domain:images (downscale before delivery)"),
    ("S48", "domain:images (Agent window history)"),
    ("S49", "domain:images (Agent window settlement)"),
    ("S50", "domain:speech (island line timing)"),
    ("S51", "domain:speech (speaking state follows audio)"),
    ("S52", "domain:speech (two players)"),
    ("S53", "domain:speech (speed sample)"),
    ("S54", "domain:speech (speech-end race)"),
    ("S55", "domain:island (notice flood clears itself)"),
    ("S56", "domain:island (notice update and clear by id)"),
    ("S57", "test:unheard_speech_falls_back_to_one_island_line_and_a_heimdall_turn (app); context kind: sim:s11_context_only_notices_are_appends"),
    ("S58", "sim:s58_provider_400_mid_steer"),
    ("S59", "sim:s31_s59_signature_400_mid_steer_is_resent_once"),
    ("S60", "sim:s60_key_rejected_then_connected"),
    ("S61", "test:provider-retry.test.mjs (429 with long reset)"),
    ("S62", "test:provider-retry.test.mjs (529 backoff)"),
    ("S63", "sim:s63_no_headers_in_120_seconds"),
    ("S64", "sim:s64_replay_of_18_oct_17_39_to_17_54"),
];

#[test]
fn every_scenario_id_is_mapped_and_every_sim_test_exists() {
    let source = include_str!("scenarios.rs");
    assert_eq!(COVERAGE.len(), 64);
    for (n, (id, place)) in COVERAGE.iter().enumerate() {
        assert_eq!(*id, format!("S{:02}", n + 1));
        if let Some(name) = place.strip_prefix("sim:") {
            assert!(source.contains(&format!("fn {name}()")), "{id}: no sim test {name}");
        } else {
            assert!(["test:", "domain:", "missing:"].iter().any(|p| place.starts_with(p)), "{id}: {place}");
        }
    }
}
