//! Night pass 3, delivery part, end to end through the real `Pipeline` with stubbed models: one utterance heard on
//! two microphones reaches the decider and the agent once (bug note 21 Oct 00:10:14, AirPods Pro and Shure MV7i both
//! on: "Okay, I have both enabled right now…" woke the agent twice in the same second).
use super::plumbing_tests::{rig, turn, Detect};
use super::*;

const S: u64 = 1_000_000_000;
const LINE: &str = "Okay, I have both enabled right now. We have the um AirPods and we have the Shure microphone.";

fn records(a: &Pipeline) -> Vec<heimdall_archive::Record> {
    a.index.evidence.records().unwrap()
}

#[test]
fn one_utterance_on_two_microphones_reaches_the_decider_and_the_agent_once() {
    let (_t, mut a, shure) = rig(false);
    let second = crate::fake_source("mic", 1, 1);
    let airpods = second.id.0.clone();
    a.source(second);
    // The Shure's final first, the AirPods' 577 ms of start later (00:10:05: 10.88 s and 12.24 s spans).
    turn(&mut a, &shure, 10 * S, LINE, Detect::Final, 0, Some(0.), 2 * S, |_| {});
    turn(&mut a, &airpods, 10 * S + 577_000_000, LINE, Detect::Final, 0, Some(0.), 2 * S, |_| {});
    a.agent.as_ref().unwrap().barrier().unwrap();
    let r = records(&a);
    let decided: Vec<_> = r.iter().filter(|r| r.kind == "send_decision").collect();
    assert_eq!(decided.len(), 1, "one decision for one utterance");
    assert_eq!(decided[0].source, shure);
    let given = r.iter().filter(|r| r.kind == "agent_context_routing" && r.payload["given_to_agent"] == true).count();
    assert_eq!(given, 1, "the agent gets the line once");
    assert_eq!(a.pending_text.iter().filter(|t| t.contains("both enabled right now")).count(), 1, "{:?}", a.pending_text);
    // Both lines stay in the archive; the copy names the line it repeats.
    let rows = a.index.utterances_between(0, 100 * S).unwrap();
    assert_eq!(rows.len(), 2, "{rows:#?}");
    let first = rows.iter().find(|r| r["source_id"] == shure.as_str()).unwrap();
    let copy = rows.iter().find(|r| r["source_id"] == airpods.as_str()).unwrap();
    assert_eq!(copy["decision"]["status"], "heard_on_another_microphone", "{copy:#}");
    assert_eq!(copy["decision"]["same_as"], first["id"]);
    assert_eq!(copy["decision"]["given_to_agent"], false);

    // Worst cases after it: the second microphone alone is heard as before, and the user repeating himself on one
    // microphone is two utterances.
    turn(&mut a, &airpods, 60 * S, "And now it's only the AirPods.", Detect::Final, 0, Some(0.), 2 * S, |_| {});
    turn(&mut a, &shure, 90 * S, "Heimdall, are you there?", Detect::Final, 0, Some(0.), 2 * S, |_| {});
    turn(&mut a, &shure, 95 * S, "Heimdall, are you there?", Detect::Final, 0, Some(0.), 2 * S, |_| {});
    a.agent.as_ref().unwrap().barrier().unwrap();
    assert_eq!(records(&a).iter().filter(|r| r.kind == "send_decision").count(), 4);
}
