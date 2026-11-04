//! Night pass 3 follow-up (21 Oct, pi-exit): the result of a task Heimdall started wakes the agent, also right after
//! a start while the user has not spoken, and survives a restart once (after a rebuild and restart
//! the agent keeps receiving its tasks' completion notices, none lost or duplicated). At 04:00:52 the night-pass
//! task's result arrived 29 minutes after the install's start and became context: nobody was woken. Every other
//! notice keeps E4.3: T3 updates for threads Heimdall did not start stay context until the user speaks.
use super::*;

const RESULT: &str = "terminal:mcp:night:run-25:completed";
const RESULT_TEXT: &str = "Task 'Night pass' (mcp:night) completed. Final reply:\n\nRebuilt, installed and verified.";

fn result_wakes(sim: &Sim) -> Vec<Input> {
    sim.wakes().into_iter().filter(|i| i.text.contains(&format!("[Heimdall notice: {RESULT}]"))).collect()
}

/// R1: a task's result after a start, the user silent: exactly one turn, and it does not count as the user addressing
/// Heimdall (other notices still wait for him). The same id again (the heartbeat's repeat) adds nothing.
#[test]
fn r1_a_task_result_after_a_start_wakes_once_while_owner_is_silent() {
    let mut sim = Sim::new(EVENING_MS);
    let start = sim.now();
    sim.advance(29 * MIN);
    assert!(sim.bridge.awaiting_owner());
    assert!(sim.notice(RESULT, RESULT_TEXT));
    sim.settle();
    assert_eq!(result_wakes(&sim).len(), 1, "{:#?}", sim.inputs());
    assert_eq!(sim.turns_between(start, sim.now() + 1), 1);
    assert!(sim.bridge.awaiting_owner(), "a task's result is not User addressing Heimdall");
    assert!(!sim.notice(RESULT, RESULT_TEXT), "the same result again is a duplicate");
    sim.advance(MIN);
    sim.settle();
    assert_eq!(result_wakes(&sim).len(), 1);
    assert!(sim.appends().iter().all(|a| !a.text.contains("Rebuilt, installed")), "never context as well");
    sim.check_global();
}

/// R2: a task's result still queued when the agent quit (it could not take it then) goes out once after the start
/// context, and never again after a second restart or a repeated callback.
#[test]
fn r2_a_task_result_pending_at_quit_goes_out_once_after_the_restart() {
    let mut sim = Sim::new(EVENING_MS);
    sim.advance(MIN);
    sim.bridge.status = crate::Status::WaitingProvider("Connect Anthropic".into());
    assert!(sim.notice(RESULT, RESULT_TEXT));
    assert!(result_wakes(&sim).is_empty(), "the agent could not take it");
    sim.advance(10 * MIN);
    sim.restart(None);
    sim.settle();
    let wakes = result_wakes(&sim);
    assert_eq!(wakes.len(), 1, "{:#?}", sim.inputs());
    let start_context = sim.inputs().iter().rposition(|i| i.kind == InputKind::Append).unwrap();
    let delivered = sim.inputs().iter().position(|i| i.text.contains(&format!("[Heimdall notice: {RESULT}]"))).unwrap();
    assert!(delivered > start_context, "after the start context");
    assert!(!catch_up_has_result(&sim), "a wake, not history");
    sim.advance(MIN);
    sim.restart(None);
    assert!(!sim.notice(RESULT, RESULT_TEXT));
    sim.settle();
    assert_eq!(result_wakes(&sim).len(), 1, "never twice");
    sim.check_global();
}
fn catch_up_has_result(sim: &Sim) -> bool {
    sim.appends().iter().any(|a| a.text.contains("Rebuilt, installed"))
}

/// R3: a result queued by a build from before this rule (stored as an `event` notice) is a task's result too.
#[test]
fn r3_a_task_result_queued_by_an_older_build_still_wakes_after_the_restart() {
    let mut sim = Sim::new(EVENING_MS);
    // The old build could not deliver it before it quit.
    sim.bridge.status = crate::Status::WaitingProvider("Connect Anthropic".into());
    let wire = serde_json::json!({"type":"user","uuid":"notice-old-result","message":{"role":"user","content":[{"type":"text","text":format!("[Heimdall notice: {RESULT}]\n{RESULT_TEXT}")}]},
        "notice":"event","shouldQuery":true,"timestamp_ms":sim.now()});
    sim.bridge.db.execute("INSERT INTO deliveries VALUES('notice-old-result',?1,1,0,'queued')", [wire.to_string()]).unwrap();
    sim.advance(MIN);
    assert!(result_wakes(&sim).is_empty());
    sim.restart(None);
    sim.settle();
    assert_eq!(result_wakes(&sim).len(), 1, "{:#?}", sim.inputs());
    sim.check_global();
}

/// R4 (E4.3 kept): after a start, T3 updates for threads Heimdall did not start, the legacy observer's lines and a
/// question stay out of turns until the user speaks; updates are context.
#[test]
fn r4_other_t3_updates_after_a_start_stay_context() {
    let mut sim = Sim::new(EVENING_MS);
    let start = sim.now();
    sim.advance(MIN);
    sim.notice("t3:batch:5f2a9c", "T3 update:\n- 'Mobius' completed: No, it's not me");
    sim.notice("t3-activity:1234", "T3 update: 'Mobius' completed: the bridge works");
    sim.t3(T3Event::Waiting { thread: "w".into(), run: "r".into(), owned: true, requests: vec!["q1".into()], text: "Worker w asks: which branch?".into() });
    sim.settle();
    sim.advance(15 * MIN);
    assert_eq!(sim.turns_between(start, sim.now() + 1), 0, "{:#?}", sim.turns());
    assert!(sim.appends().iter().any(|a| a.text.contains("No, it's not me")), "context");
    assert!(sim.appends().iter().any(|a| a.text.contains("the bridge works")), "context");
    sim.check_global();
}

/// R5: a result and a scheduled wake arriving together after a start each reach the model once: the one waiting
/// behind the other's turn goes out when the notice gap ends (`notice_deadline` names scheduled wakes and results).
#[test]
fn r5_a_result_and_a_scheduled_wake_together_both_go_out_once() {
    let mut sim = Sim::new(EVENING_MS);
    sim.advance(MIN);
    assert!(sim.notice(RESULT, RESULT_TEXT));
    assert!(sim.notice("selfwake:wake-1a2b3c", "Scheduled wake wake-1a2b3c: you set it at 01:52 for 07:00.\nYour note, your own instruction to act on now: check the build"));
    sim.settle();
    assert_eq!(result_wakes(&sim).len(), 1, "{:#?}", sim.inputs());
    assert_eq!(sim.wakes().iter().filter(|i| i.text.contains("check the build")).count(), 1, "{:#?}", sim.inputs());
    sim.check_global();
}

/// R6 (review 21 Oct): a result coalesced into an older worker update's delivery, which Pi could not take at that
/// moment (nothing written), and then a restart: the result still wakes the agent once after the start context, the
/// update riding with it as it would have.
#[test]
fn r6_a_result_coalesced_into_an_update_survives_a_restart_once() {
    let mut sim = Sim::new(EVENING_MS);
    sim.say("start the night pass");
    sim.settle();
    // Both wait behind a turn, then go out together in the update's delivery, which Pi refuses for now.
    sim.world.lock().unwrap().replies.push_back(vec![fake_pi::Step::Think(10 * S)]);
    sim.say("and check the build");
    sim.notice("t3:mcp:w:r:done", "T3 update:\n- 'w' (yours) completed: the update");
    assert!(sim.notice(RESULT, RESULT_TEXT));
    sim.world.lock().unwrap().unavailable_prompts = u32::MAX;
    sim.advance(11 * S);
    sim.pump();
    assert!(result_wakes(&sim).is_empty(), "{:#?}", sim.inputs());
    sim.world.lock().unwrap().unavailable_prompts = 0;
    sim.restart(None);
    sim.settle();
    assert_eq!(result_wakes(&sim).len(), 1, "{:#?}", sim.inputs());
    assert_eq!(sim.inputs().iter().filter(|i| i.text.contains("the update")).count(), 1, "the update rides with it once");
    sim.advance(MIN);
    sim.restart(None);
    sim.settle();
    assert_eq!(result_wakes(&sim).len(), 1, "never twice");
    sim.check_global();
}

/// R7 (bug note 2026-10-28T02-20-08): after a start an approval waits for the user (E4.3); its run's result arriving
/// first withdraws it, so his next words never bring an approval for a run that is over. Another run's approval
/// still waits for him and goes out.
#[test]
fn r7_a_runs_result_withdraws_its_approval_still_waiting_after_a_start() {
    let mut sim = Sim::new(EVENING_MS);
    sim.advance(MIN);
    let stale = "approval:mcp:manual:run:thread:mcp%3Amanual:ordinal:4";
    let live = "approval:mcp:other:run:thread:mcp%3Aother:ordinal:2";
    assert!(sim.notice(stale, "Task 'Manual' (mcp:manual) waits for an approval in T3; only User can give it there."));
    assert!(sim.notice(live, "Task 'Other' (mcp:other) waits for an approval in T3; only User can give it there."));
    sim.settle();
    assert!(sim.bridge.awaiting_owner());
    assert!(sim.notice("terminal:mcp:manual:run:thread:mcp%3Amanual:ordinal:4:completed", "Task 'Manual' (mcp:manual) completed. Final reply:\n\nDone."));
    sim.settle();
    sim.advance(MIN);
    sim.say("what's new?");
    sim.settle();
    sim.advance(MIN);
    sim.settle();
    let sent = |id: &str| sim.inputs().iter().any(|i| i.text.contains(&format!("[Heimdall notice: {id}]")));
    assert!(!sent(stale), "{:#?}", sim.inputs());
    assert!(sent(live), "{:#?}", sim.inputs());
    sim.check_global();

    // A run that ended on its provider's sign-in (`…:failed:auth`, t3 workers) withdraws its approval too.
    let mut sim = Sim::new(EVENING_MS);
    sim.advance(MIN);
    let stale = "approval:mcp:wizard:run:thread:mcp%3Awizard:ordinal:3";
    assert!(sim.notice(stale, "Task 'Wizard' (mcp:wizard) waits for an approval in T3; only User can give it there."));
    sim.settle();
    assert!(sim.notice("terminal:mcp:wizard:run:thread:mcp%3Awizard:ordinal:3:failed:auth", "Task 'Wizard' (mcp:wizard) failed: It failed on its provider's sign-in."));
    sim.settle();
    sim.say("what's new?");
    sim.settle();
    sim.advance(MIN);
    sim.settle();
    assert!(!sim.inputs().iter().any(|i| i.text.contains(&format!("[Heimdall notice: {stale}]"))), "{:#?}", sim.inputs());
    sim.check_global();
}
