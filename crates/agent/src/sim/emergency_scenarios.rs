//! Emergency mode (docs/emergency.md; ~00:05): while Heimdall's agent is down, everything that would
//! have woken it goes into its conversation as context and to the cover agent, and starts no turn of its own.
use super::*;
use crate::emergency::{Relay, Relayed, HELD};

const QUOTA: &str = "The ChatGPT user has reached their Subscription Sharing usage limit. Ask the user to try again after their usage limit resets or use an API key instead.";

fn relay() -> (Relay, Arc<Mutex<Vec<Relayed>>>) {
    let got = Arc::new(Mutex::new(Vec::new()));
    let sink = got.clone();
    (Arc::new(move |r: Relayed| sink.lock().unwrap().push(r)), got)
}
fn prompts(sim: &Sim) -> usize {
    sim.inputs().iter().filter(|i| i.kind == InputKind::Prompt).count()
}

/// E1: the 26 Oct 23:33 outage replayed. A wake fails on the quota; emergency mode holds turns. The user's words, the
/// failed wake's words, a typed message and a task's result each reach the conversation once as held context and the
/// cover agent once, and none starts a turn. Off again, his next words get a turn of their own and nothing held is
/// sent twice.
#[test]
fn e1_held_input_is_context_for_the_agent_and_a_message_for_the_cover_agent_never_a_turn() {
    let mut sim = Sim::new(EVENING_MS);
    sim.advance(MIN);
    // A spent quota is never resent (notice::quota_exhausted): one failed request.
    sim.world.lock().unwrap().replies.push_back(vec![Step::Think(500), Step::Fail(QUOTA.into())]);
    sim.say("Heimdall, what's on the plan tonight?");
    sim.settle();
    let turns = prompts(&sim);
    let (relay, got) = relay();
    sim.rt.block_on(sim.bridge.hold_turns(Some(relay))).unwrap();
    sim.pump();
    assert!(sim.bridge.holding_turns());
    // The words the failed request left unanswered went to the cover agent at once.
    {
        let got = got.lock().unwrap();
        assert_eq!(got.len(), 1, "{got:#?}");
        assert_eq!(got[0].kind, "wake");
        assert!(got[0].text.contains("what's on the plan tonight?"), "{}", got[0].text);
    }
    sim.advance(20 * S);
    sim.say("Are you there?");
    sim.typed("Typed while he was down");
    assert!(sim.t3(T3Event::Finished { thread: "t1".into(), run: "r1".into(), owned: true, text: "Tests pass.".into() }));
    sim.settle();
    sim.advance(2 * MIN);
    sim.settle();
    assert_eq!(prompts(&sim), turns, "no turn while held: {:#?}", sim.inputs());
    let got = got.lock().unwrap().clone();
    let kinds: Vec<&str> = got.iter().map(|r| r.kind.as_str()).collect();
    assert_eq!(kinds, ["wake", "wake", "typed", "result"], "{got:#?}");
    assert!(got[1].text.contains("Are you there?") && !got[1].text.contains("what's on the plan"), "each line once: {}", got[1].text);
    assert!(got[3].text.contains("Tests pass."));
    // Every one of them is in the conversation, marked, in order.
    let held: Vec<String> = sim.appends().into_iter().filter(|a| a.text.starts_with(HELD)).map(|a| a.text).collect();
    assert_eq!(held.len(), 4, "{held:#?}");
    assert!(held[1].contains("Are you there?") && held[2].contains("Typed while he was down") && held[3].contains("Tests pass."));
    // Back: his next words start a turn, carrying only themselves.
    sim.rt.block_on(sim.bridge.hold_turns(None)).unwrap();
    sim.advance(10 * S);
    sim.say("Are you back?");
    sim.settle();
    let wakes = sim.wakes();
    let last = wakes.last().unwrap();
    assert!(last.text.contains("Are you back?") && !last.text.contains("Are you there?") && !last.text.contains("what's on the plan"), "{}", last.text);
    assert_eq!(prompts(&sim), turns + 1);
    assert_eq!(got.len(), 4, "nothing relayed after the hold ends");
    sim.check_global();
}

/// E2: a held input is context even while the provider side blocks turns, and work waiting for an answer is what the
/// no-answer check counts: a queued wake is, a question waiting for the user is not.
#[test]
fn e2_waiting_for_an_answer_counts_owner_s_words_not_notices() {
    let mut sim = Sim::new(EVENING_MS);
    sim.advance(MIN);
    sim.settle();
    assert!(!sim.bridge.awaiting_answer());
    sim.world.lock().unwrap().replies.push_back(vec![Step::Think(60 * S), Step::Final("ok")]);
    sim.say("Long one");
    assert!(sim.bridge.awaiting_answer(), "a running turn waits for its answer");
    sim.settle();
    assert!(!sim.bridge.awaiting_answer());
    let (relay, got) = relay();
    sim.rt.block_on(sim.bridge.hold_turns(Some(relay))).unwrap();
    sim.bridge.status = crate::Status::AuthBlocked("ChatGPT: sign in again".into());
    sim.say("Still there?");
    sim.settle();
    assert_eq!(got.lock().unwrap().len(), 1);
    assert!(sim.appends().iter().any(|a| a.text.starts_with(HELD) && a.text.contains("Still there?")), "appended while turns are blocked");
    assert!(!sim.bridge.awaiting_answer(), "held words wait for nobody");
}

/// E3 (review 27 Oct): a wake in flight when the hold begins (the agent hangs, then its request fails and is queued for
/// a resend) is held when it comes back: no turn while the agent is down, and the cover agent gets its words.
#[test]
fn e3_a_wake_in_flight_when_the_hold_begins_is_held_when_it_fails() {
    let mut sim = Sim::new(EVENING_MS);
    sim.advance(MIN);
    sim.settle();
    sim.world.lock().unwrap().replies.push_back(vec![Step::Think(30 * S), Step::Fail("503 Service Unavailable".into())]);
    sim.say("Check the build, please");
    sim.advance(S);
    let turns = prompts(&sim);
    let (relay, got) = relay();
    sim.rt.block_on(sim.bridge.hold_turns(Some(relay))).unwrap();
    assert!(got.lock().unwrap().is_empty(), "its words belong to the running request");
    sim.settle();
    sim.advance(2 * MIN);
    sim.settle();
    assert_eq!(prompts(&sim), turns, "no resend while held: {:#?}", sim.inputs());
    let got = got.lock().unwrap().clone();
    assert_eq!(got.len(), 1, "{got:#?}");
    assert!(got[0].kind == "wake" && got[0].text.contains("Check the build, please"), "{got:#?}");
}

/// E4 (review 27 Oct): a hand-back's "while you were down" left queued when the hold comes back is Heimdall's own and
/// never reaches the cover agent; input that arrived as emergency mode ended (`emergency-late-`) wakes the agent even
/// while held.
#[test]
fn e4_a_stale_hand_back_is_dropped_and_late_input_still_wakes() {
    let mut sim = Sim::new(EVENING_MS);
    sim.advance(MIN);
    sim.settle();
    sim.world.lock().unwrap().replies.push_back(vec![Step::Think(20 * S), Step::Final("ok")]);
    sim.say("Busy turn");
    sim.advance(S);
    sim.rt.block_on(sim.bridge.notice("emergency-back-1", "while you were down", "While you were down: …", crate::notice::NoticeKind::Scheduled, None)).unwrap();
    let (relay, got) = relay();
    sim.rt.block_on(sim.bridge.hold_turns(Some(relay))).unwrap();
    sim.settle();
    assert!(got.lock().unwrap().iter().all(|r| !r.text.contains("While you were down")), "{:#?}", got.lock().unwrap());
    assert!(sim.inputs().iter().all(|i| !i.text.contains("While you were down")));
    let turns = prompts(&sim);
    sim.advance(MIN);
    assert!(sim.notice("emergency-late-w9", "[Arrived while emergency mode was ending; nobody answered it] User said: hello?"));
    sim.settle();
    assert_eq!(prompts(&sim), turns + 1, "late input wakes the agent");
    assert!(got.lock().unwrap().iter().all(|r| !r.text.contains("hello?")));
}

/// The user, 20:44: in relay mode Pi's session mirrors the relay thread for as long as it acts, so a long relay grows it.
/// It is compacted as usual, never cut: idle compaction runs under a relay-mode hold, and still waits in emergency mode
/// (its provider is down).
#[test]
fn e5_a_long_relay_mirror_is_compacted_in_relay_mode_and_waits_in_emergency_mode() {
    for relay_mode in [false, true] {
        let mut sim = Sim::new(EVENING_MS);
        sim.bridge.set_compaction_policy(Some(crate::maintenance::CompactionPolicy::for_limits(crate::provider::Limits::for_context(252_571, Some(128_000)), true)));
        sim.world.lock().unwrap().window = 1_000_000;
        let (relay, _) = relay();
        sim.rt.block_on(sim.bridge.hold_turns(Some(relay))).unwrap();
        sim.bridge.set_hold_compacts(relay_mode);
        sim.world.lock().unwrap().context_tokens = Some(220_000);
        sim.advance(MIN);
        sim.settle();
        let started = sim.world.lock().unwrap().compactions_started;
        assert_eq!(started, usize::from(relay_mode), "relay mode {relay_mode}");
        assert_eq!(prompts(&sim), 0, "and no turn either way");
    }
}
