//! Day pass 26 Oct (the user, 00:40 and 00:53): keep the provider's one-hour prompt cache warm. After 55 minutes without
//! a request Heimdall sends one short cache-refresh message; the agent checks for anything stalled and otherwise
//! answers `NO_REPLY`, which nobody hears or reads. Pings stop 3 hours after the last real request, and none goes out
//! with the Agent off or on a provider without the hour cache.
use super::*;
use crate::maintenance::{CompactionPolicy, PING_TEXT, PING_TEXT_AFTER_START};
use crate::provider::Limits;

fn opus(sim: &mut Sim, hour_cache: bool) {
    sim.bridge.set_compaction_policy(Some(CompactionPolicy::for_limits(Limits::for_context(252_571, Some(128_000)), hour_cache)));
    let mut w = sim.world.lock().unwrap();
    // Below the cold-cache break-even (92,000): the cache going cold starts no compaction here.
    w.context_tokens = Some(60_000);
    w.window = 1_000_000;
    w.default_reply = vec![Step::Think(2_000), Step::Final("NO_REPLY")];
}
fn pings(sim: &Sim) -> Vec<u64> {
    sim.wakes().iter().filter(|i| i.text.contains("[Heimdall cache refresh]")).map(|i| i.at).collect()
}
/// Advances minute by minute, as the supervisor's 10 s tick would find the ping due within the minute.
fn minutes(sim: &mut Sim, n: u64) {
    for _ in 0..n { sim.advance(MIN); }
}

/// P1: the user asks something, then nothing for an hour. At 55 minutes one ping goes out with its instructions; the
/// agent answers NO_REPLY and the user hears and sees nothing.
#[test]
fn p1_a_ping_after_55_idle_minutes_answers_silently() {
    let mut sim = Sim::new(EVENING_MS);
    opus(&mut sim, true);
    sim.world.lock().unwrap().replies.push_back(vec![Step::Think(2_000), Step::Final("Done.")]);
    sim.say("check the build");
    sim.settle();
    let request = sim.world.lock().unwrap().last_request_at.unwrap();
    let heard = sim.heard_texts();
    assert_eq!(heard, ["Done."]);
    let reply = sim.last_reply.clone();
    minutes(&mut sim, 54);
    assert!(sim.now() < request + 55 * MIN && pings(&sim).is_empty(), "no ping before 55 minutes");
    minutes(&mut sim, 2);
    let sent = pings(&sim);
    assert_eq!(sent.len(), 1);
    assert!(sent[0] >= request + 55 * MIN && sent[0] < request + 57 * MIN, "pinged at 55 minutes: {}", sent[0] - request);
    let ping = sim.wakes().into_iter().find(|i| i.text.contains("[Heimdall cache refresh]")).unwrap();
    assert!(ping.text.contains(PING_TEXT), "the instructions ride the message: {}", ping.text);
    assert!(ping.id.as_deref().is_some_and(|id| id.starts_with("cache-ping-")), "{:?}", ping.id);
    sim.settle();
    assert_eq!(sim.heard_texts(), heard, "NO_REPLY is never spoken");
    assert_eq!(sim.last_reply, reply, "nor shown");
    // The ping's request refreshed the hour.
    assert!(sim.world.lock().unwrap().last_request_at.unwrap() >= sent[0]);
    sim.check_global();
}

/// P2: Pings at 55, 110 and 165 minutes after the last real request, then none (3 hours); the cache goes cold.
/// The user's next words reset the window.
#[test]
fn p2_pings_stop_three_hours_after_the_last_real_request() {
    let mut sim = Sim::new(EVENING_MS);
    opus(&mut sim, true);
    sim.say("anything new?");
    sim.settle();
    let request = sim.world.lock().unwrap().last_request_at.unwrap();
    minutes(&mut sim, 5 * 60);
    let after: Vec<u64> = pings(&sim).iter().map(|t| (t - request) / MIN).collect();
    assert_eq!(after.len(), 3, "{after:?}");
    for (got, want) in after.iter().zip([55, 110, 165]) {
        assert!((want..want + 3).contains(got), "pings at {after:?} minutes");
    }
    assert!(sim.heard_texts().iter().all(|h| !h.contains("NO_REPLY")));
    sim.say("back again");
    sim.settle();
    let back = sim.now();
    minutes(&mut sim, 60);
    assert_eq!(pings(&sim).iter().filter(|t| **t > back).count(), 1, "a real request starts the pings again");
    sim.check_global();
}

/// P3: With the Agent off no ping goes out, however long the cache has been idle.
#[test]
fn p3_no_ping_with_the_agent_off() {
    let mut sim = Sim::new(EVENING_MS);
    opus(&mut sim, true);
    sim.say("thanks");
    sim.settle();
    sim.agent_on = false;
    minutes(&mut sim, 3 * 60);
    assert!(pings(&sim).is_empty());
    sim.check_global();
}

/// P4: A provider without the hour cache (OpenCode Go, ChatGPT, the local engine) gets no ping.
#[test]
fn p4_no_ping_without_the_hour_cache() {
    let mut sim = Sim::new(EVENING_MS);
    opus(&mut sim, false);
    sim.say("thanks");
    sim.settle();
    minutes(&mut sim, 2 * 60);
    assert!(pings(&sim).is_empty());
    sim.check_global();
}

/// P5: A ping never lands in the middle of a turn: a 58-minute tool run keeps the agent busy past the ping time, and
/// once it settles the turn's own requests have refreshed the cache.
#[test]
fn p5_no_ping_while_a_turn_runs() {
    let mut sim = Sim::new(EVENING_MS);
    opus(&mut sim, true);
    sim.world.lock().unwrap().replies.push_back(vec![Step::Think(1_000), Step::Tool("t3_worker_wait", 58 * MIN), Step::Think(1_000), Step::Final("NO_REPLY")]);
    sim.say("wait for the worker");
    minutes(&mut sim, 59);
    assert!(pings(&sim).is_empty());
    sim.check_global();
}

/// P6 (review 26 Oct): after a start the 3-hour window still counts from the last real request; the pings' own
/// requests never move it, and until the user talks the ping asks only for NO_REPLY.
#[test]
fn p6_after_a_start_pings_stop_three_hours_after_the_last_real_request() {
    let mut sim = Sim::new(EVENING_MS);
    opus(&mut sim, true);
    sim.say("goodnight");
    sim.settle();
    let request = sim.world.lock().unwrap().last_request_at.unwrap();
    sim.advance(MIN);
    sim.restart(Some("Reloaded: lab"));
    opus(&mut sim, true);
    minutes(&mut sim, 6 * 60);
    let sent: Vec<u64> = pings(&sim).iter().map(|t| (t - request) / MIN).collect();
    assert_eq!(sent.len(), 3, "pings at {sent:?} minutes");
    assert!(sim.wakes().iter().filter(|i| i.text.contains("[Heimdall cache refresh]")).all(|i| i.text.contains(PING_TEXT_AFTER_START)), "NO_REPLY only until User talks");
    assert!(sim.heard_texts().is_empty() || sim.heard_texts().iter().all(|h| !h.contains("NO_REPLY")));
    sim.check_global();
}

/// P7 (review 26 Oct): a ping the provider fails is dropped: never resent, never reported to the user, and the agent stays
/// idle for his next words.
#[test]
fn p7_a_failed_ping_is_dropped_silently() {
    let mut sim = Sim::new(EVENING_MS);
    opus(&mut sim, true);
    sim.say("thanks");
    sim.settle();
    let before = sim.resolutions.len();
    sim.world.lock().unwrap().replies.push_back(vec![Step::Think(1_000), Step::Fail("503 Service Unavailable".into())]);
    minutes(&mut sim, 57);
    sim.settle();
    assert_eq!(pings(&sim).len(), 1, "the failed ping is not resent");
    let state: String = sim.bridge.db.query_row("SELECT state FROM deliveries WHERE id LIKE 'cache-ping-%'", [], |r| r.get(0)).unwrap();
    assert_eq!(state, "failed");
    assert_eq!(sim.resolutions.len(), before, "nothing reported: {:?}", sim.resolutions);
    assert_eq!(sim.bridge.status, crate::Status::Idle);
    let spoke = sim.now();
    sim.say("are you there?");
    assert_eq!(sim.wakes().last().unwrap().at, spoke, "his words go out at once");
    sim.settle();
    sim.check_global();
}
