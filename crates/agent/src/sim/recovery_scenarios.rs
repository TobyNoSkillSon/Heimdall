//! Night pass 20→21 Oct (docs/briefs/night-1021.md, lab/harness/EXPECTED.md E4.1, E4.4): what a start hands the
//! agent. The user (20 Oct): "after the compaction, we don't need to send it the last 30 minutes of transcript … it's
//! already compacted. What we need to send is any audio that happened after the compaction started … we need to be
//! smart about when do we actually recover". So a start's catch-up begins where the conversation's own record ends —
//! its newest message, or the start of its last compaction, whichever is later — never more than 30 minutes back,
//! and never repeats what the history holds. The framing stays: history only, then the restart message, last.
use super::*;

fn catch_up(sim: &Sim) -> String {
    sim.appends().last().expect("a start context").text.clone()
}
fn reply(sim: &Sim, steps: Vec<Step>) {
    sim.world.lock().unwrap().replies.push_back(steps);
}

/// C1: a plain restart carries only what was said after the conversation's last message: not the speech from before
/// it (heard while the agent was busy, or never handed over), not the 30 minutes before the start.
#[test]
fn c1_a_plain_restart_sends_only_what_came_after_the_last_message() {
    let mut sim = Sim::new(EVENING_MS);
    sim.advance(20 * MIN);
    let before = sim.now();
    sim.heard_while_off(before - 5 * MIN, "Dave", "this was before the last message");
    sim.say("what's on my calendar?");
    sim.settle();
    let history_end = sim.world.lock().unwrap().history_end.unwrap();
    sim.advance(2 * MIN);
    sim.heard_while_off(sim.now(), "Dave", "said after the answer, never handed over");
    sim.advance(MIN);
    sim.aside("Dave", "a pending line after the answer");
    sim.advance(7 * MIN);
    sim.restart(Some("Reloaded: User reloaded the agent"));
    let text = catch_up(&sim);
    assert!(!text.contains("this was before the last message"), "{text}");
    assert!(!text.contains("what's on my calendar?"), "already in the conversation: {text}");
    assert!(text.contains("said after the answer, never handed over") && text.contains("a pending line after the answer"), "{text}");
    assert!(text.starts_with(&format!("Heimdall catch-up: what was said since {} ", crate::clock::local_hms(history_end))), "{text}");
    assert!(text.ends_with(&crate::notice::restart_message(sim.now())), "{text}");
    assert_eq!(sim.turns_between(sim.started_at, sim.now() + 1), 0, "history only: no turn");
    sim.check_global();
}

/// C2, the 20 Oct 19:20–19:22 shape: a compaction runs, then the agent restarts. Of the archived speech, only what was
/// said after the compaction began comes back (the summary holds the conversation before it). A spool line the model
/// never received comes back whenever it was said within 30 minutes: no summary holds it (night review 21 Oct, MUST
/// 1), and the header names the earliest line.
#[test]
fn c2_after_a_compaction_the_archive_from_when_it_began_and_every_line_never_received() {
    let mut sim = Sim::new(EVENING_MS);
    sim.say("show me YouTube");
    sim.settle();
    sim.advance(2 * MIN);
    let waiting = sim.aside("User", "talking to Dave before the compaction");
    let waiting_at = sim.now();
    sim.heard_while_off(sim.now() + MIN, "Dave", "archived before the compaction");
    sim.advance(3 * MIN);
    sim.compaction();
    sim.advance(5 * S);
    sim.aside("User", "said while it compacted");
    sim.settle();
    assert!(!sim.world.lock().unwrap().compacting());
    sim.advance(MIN);
    sim.aside("Dave", "said after the compaction");
    sim.advance(MIN);
    sim.restart(Some("Reloaded: Session storage maintenance"));
    let text = catch_up(&sim);
    assert!(text.contains("said while it compacted") && text.contains("said after the compaction"), "{text}");
    assert!(text.contains("talking to Dave before the compaction"), "never received, so no summary holds it: {text}");
    assert!(!text.contains("archived before the compaction"), "{text}");
    assert!(!text.contains("show me YouTube"), "{text}");
    assert!(text.starts_with(&format!("Heimdall catch-up: what was said since {} ", crate::clock::local_hms(waiting_at))), "{text}");
    assert_eq!(sim.dropped(&waiting), None);
    assert!(text.ends_with(&crate::notice::restart_message(sim.now())));
    sim.check_global();
}

/// C3: the user talks during an idle compaction: it finishes, his words follow, and a later start's catch-up still begins
/// at the conversation's last message.
#[test]
fn c3_a_compaction_owner_talks_over_finishes_and_the_catch_up_follows_it() {
    let mut sim = Sim::new(EVENING_MS);
    sim.bridge.set_compaction_policy(Some(crate::maintenance::CompactionPolicy::for_limits(crate::provider::Limits::for_context(252_571, Some(128_000)), true)));
    sim.world.lock().unwrap().context_tokens = Some(220_000);
    sim.world.lock().unwrap().window = 1_000_000;
    sim.world.lock().unwrap().compaction_ms = 45_000;
    sim.say("first");
    sim.settle();
    sim.advance(31 * S);
    assert!(sim.world.lock().unwrap().idle_compacting(), "an idle compaction runs");
    sim.aside("Dave", "before User talks over it");
    sim.say("second, which waits for it");
    sim.settle();
    let w = sim.world.lock().unwrap();
    assert_eq!((w.compactions_started, w.compactions.len()), (1, 1), "his words did not cut it");
    drop(w);
    let history_end = sim.world.lock().unwrap().history_end.unwrap();
    sim.advance(MIN);
    sim.aside("Dave", "after the answer");
    sim.restart(None);
    // A history this long compacts before its start context goes in (E4.2); that compaction holds nothing said
    // after the start, so the context it then adds is the same.
    sim.settle();
    let text = catch_up(&sim);
    assert!(text.contains("after the answer") && !text.contains("before User talks over it"), "{text}");
    assert!(text.starts_with(&format!("Heimdall catch-up: what was said since {} ", crate::clock::local_hms(history_end))), "{text}");
    sim.check_global();
}

/// C4: hours off: the conversation ended long ago, so the 30 minutes before the start bound the catch-up, as before.
#[test]
fn c4_after_hours_off_the_catch_up_still_reaches_back_only_30_minutes() {
    let mut sim = Sim::new(EVENING_MS);
    sim.say("good night");
    sim.settle();
    let off = sim.now();
    sim.heard_while_off(off + 60 * MIN, "User", "an hour later");
    sim.heard_while_off(off + 160 * MIN, "User", "twenty minutes before the start");
    sim.heard_while_off(off + 175 * MIN, "Dave", "five minutes before the start");
    sim.advance(180 * MIN);
    sim.restart(None);
    let text = catch_up(&sim);
    assert!(!text.contains("an hour later"), "{text}");
    assert!(text.contains("twenty minutes before the start") && text.contains("five minutes before the start"), "{text}");
    assert!(text.starts_with(&format!("Heimdall catch-up: what was said since {} ", crate::clock::local_hms(sim.now() - 30 * MIN))), "{text}");
    sim.check_global();
}

/// C5, the 20 Oct 19:22–19:24 replay: every request failed; the user asked twice and reloaded twice. Each failed request
/// left his words in the conversation, so no restart's catch-up repeats them (they came back in every catch-up that
/// evening), each restart message is last, and his next words go out at once as a wake.
#[test]
fn c5_words_of_a_failed_request_never_come_back_in_a_catch_up() {
    let mut sim = Sim::new(EVENING_MS);
    let refused = "400 {\"type\":\"error\",\"error\":{\"type\":\"invalid_request_error\",\"message\":\"messages.118.content.0.tool_result.content.0.image.source.base64: The image was specified using the image/png media type, but the image appears to be a image/jpeg image\"}}";
    for _ in 0..2 { reply(&sim, vec![Step::Think(2_000), Step::Fail(refused.into())]); }
    sim.say("Hi Heimdall, you there?");
    sim.settle();
    sim.advance(10 * S);
    sim.restart(Some("Reloaded: User reloaded the agent from the Agent window at 19:23"));
    assert!(!catch_up(&sim).contains("you there"), "{}", catch_up(&sim));
    sim.say("Hi Heimdall, are you there?");
    sim.settle();
    sim.advance(10 * S);
    sim.restart(None);
    let text = catch_up(&sim);
    assert!(!text.contains("you there"), "{text}");
    assert!(text.ends_with(&crate::notice::restart_message(sim.now())), "{text}");
    sim.say("Heimdall.");
    assert!(sim.wakes().last().unwrap().text.contains("Heimdall."), "his next words wake the agent at once");
    sim.settle();
    sim.check_global();
}

/// C6: seeded sweep of starts after random talk, compactions, failures and restarts: a catch-up never repeats a line
/// the conversation already took (a wake, a steer or an earlier catch-up), never reaches past 30 minutes, and G1/G2
/// hold.
#[test]
fn c6_seeded_starts_never_repeat_what_the_conversation_holds() {
    let cases: u64 = std::env::var("HEIMDALL_SIM_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(200);
    for seed in 0..cases {
        let mut rng = seed.wrapping_mul(0x9E3779B97F4A7C15).wrapping_add(21);
        let mut next = |n: u64| { rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407); (rng >> 33) % n };
        let mut sim = Sim::new(EVENING_MS);
        for i in 0..6 + next(10) {
            sim.advance(next(8 * MIN));
            match next(7) {
                0 | 1 => { sim.say(&format!("request {seed}-{i}")); }
                2 => { sim.aside(if next(2) == 0 { "User" } else { "Dave" }, &format!("aside {seed}-{i}")); }
                3 => { let at = sim.now().saturating_sub(next(40 * MIN)); sim.heard_while_off(at, "Dave", &format!("archived {seed}-{i}")); }
                4 => { if !sim.world.lock().unwrap().turn_running() { sim.compaction(); } }
                5 => { reply(&sim, vec![Step::Think(1_000), Step::Fail("400 invalid_request_error: messages.3.content.0.image.source.base64: refused".into())]); sim.say(&format!("failing {seed}-{i}")); }
                _ => {
                    sim.settle();
                    // What the conversation took: every start context and every delivery Pi read (a steer the run
                    // never reached before it failed was not read, and must come back).
                    let read = sim.world.lock().unwrap().evidence().consumed;
                    let held: Vec<String> = sim.inputs().iter().filter(|i| i.kind == InputKind::Append || i.id.as_ref().is_some_and(|id| read.contains(id)))
                        .map(|i| i.text.clone()).collect();
                    sim.restart(None);
                    let text = catch_up(&sim);
                    for line in text.lines().filter(|l| l.starts_with('[')) {
                        let words = line.split_once("] ").map(|(_, w)| w).unwrap_or(line);
                        assert!(!held.iter().any(|h| h.contains(&format!("] {words}"))), "seed {seed}: {words:?} was already in the conversation:\n{text}");
                    }
                    assert!(text.ends_with(&crate::notice::restart_message(sim.now())), "seed {seed}");
                }
            }
        }
        sim.settle();
        sim.check_global();
    }
}

/// C7, night review 21 Oct (MUST 1): Heimdall's own idle compaction runs while a line waits for the next wake, then
/// storage maintenance reloads the agent. The line never reached the model, so the summary does not hold it: it comes
/// back.
#[test]
fn c7_a_reload_after_an_idle_compaction_brings_back_what_was_waiting() {
    let mut sim = Sim::new(EVENING_MS);
    sim.bridge.set_compaction_policy(Some(crate::maintenance::CompactionPolicy::for_limits(crate::provider::Limits::for_context(252_571, Some(128_000)), true)));
    sim.world.lock().unwrap().context_tokens = Some(220_000);
    sim.world.lock().unwrap().window = 1_000_000;
    sim.say("what's the weather?");
    sim.settle();
    let waiting = sim.aside("User", "remind me to call Dave later");
    sim.advance(31 * S);
    assert!(sim.world.lock().unwrap().idle_compacting(), "the idle compaction runs");
    sim.settle();
    assert_eq!(sim.world.lock().unwrap().compactions.len(), 1, "it finished");
    sim.advance(MIN);
    sim.restart(Some("Reloaded: Session storage maintenance"));
    let text = catch_up(&sim);
    assert!(text.contains("remind me to call Dave later"), "{text}");
    assert!(!text.contains("what's the weather?"), "{text}");
    assert_eq!(sim.dropped(&waiting), None);
    sim.check_global();
}

/// C8, night review 21 Oct (MUST 1): the provider is slow, then refuses the request; the user turns the agent off and on.
/// The request's words are in the conversation (Pi read them before it failed) and stay out; a line heard while the
/// provider was slow, and a steer the failed run never read, never reached the model and come back.
#[test]
fn c8_after_a_failed_request_and_a_toggle_only_what_the_model_never_read_comes_back() {
    let mut sim = Sim::new(EVENING_MS);
    let refused = "400 {\"type\":\"error\",\"error\":{\"type\":\"invalid_request_error\",\"message\":\"messages.3.content.0.image.source.base64: refused\"}}";
    reply(&sim, vec![Step::Stream(1_000), Step::Think(30_000), Step::Fail(refused.into())]);
    let asked = sim.say("open the quarterly report");
    sim.advance(10 * S);
    let during = sim.aside("User", "while the provider was slow");
    sim.advance(5 * S);
    let steer = sim.say("and show it full screen");
    sim.settle();
    assert!(sim.resolutions.iter().any(|r| matches!(r.outcome, crate::Resolved::Rejected)), "{:?}", sim.resolutions);
    sim.advance(MIN);
    sim.restart(None);
    let text = catch_up(&sim);
    assert!(text.contains("while the provider was slow"), "{text}");
    assert!(text.contains("and show it full screen"), "a steer the failed run never read: {text}");
    assert!(!text.contains("open the quarterly report"), "the failed request holds its words: {text}");
    assert!(sim.dropped(&asked).is_some_and(|r| r.contains("already in the conversation")), "{:?}", sim.dropped(&asked));
    assert_eq!((sim.dropped(&during), sim.dropped(&steer)), (None, None));
    sim.check_global();
}

/// C9: the app went down right after Pi wrote a request into its session, before this bridge heard Pi take it in.
/// Pi's session decides: that request stays out of the catch-up; a steer Pi had not read yet comes back.
#[test]
fn c9_after_a_crash_pis_session_decides_which_words_the_conversation_holds() {
    let mut sim = Sim::new(EVENING_MS);
    reply(&sim, vec![Step::Stream(1_000), Step::Think(60_000)]);
    sim.say("summarise my inbox");
    sim.advance(2 * S);
    sim.say("and flag the urgent ones");
    // The bridge never heard Pi read the first one (the app died first).
    sim.bridge.db.execute("UPDATE observations SET in_history=0", []).unwrap();
    sim.world.lock().unwrap().crash();
    sim.advance(5 * S);
    sim.restart(None);
    let text = catch_up(&sim);
    assert!(!text.contains("summarise my inbox"), "Pi's session holds it: {text}");
    assert!(text.contains("and flag the urgent ones"), "{text}");
    sim.check_global();
}

/// Night review 21 Oct (minor): the island says "still refuses after repair" only when the run's repair acted.
#[test]
fn c10_a_refusal_says_after_repair_only_when_a_repair_ran() {
    let mut sim = Sim::new(EVENING_MS);
    let refused = "400 {\"type\":\"error\",\"error\":{\"type\":\"invalid_request_error\",\"message\":\"messages.118.content.0.tool_result.content.0.image.source.base64: The image was specified using the image/png media type, but the image appears to be a image/jpeg image\"}}";
    let rejection = |sim: &Sim| sim.resolutions.iter().rev().find(|r| matches!(r.outcome, crate::Resolved::Rejected)).map(|r| r.reason.clone()).unwrap_or_default();
    reply(&sim, vec![Step::Think(1_000), Step::Repaired("restarted_from_summary"), Step::Think(1_000), Step::Repaired("gave_up"), Step::Fail(refused.into())]);
    sim.say("hello, are you there?");
    sim.settle();
    assert_eq!(rejection(&sim), "Agent couldn't answer: the model still refuses this conversation after repair (the model could not read the screenshot)");
    reply(&sim, vec![Step::Think(1_000), Step::Fail(refused.into())]);
    sim.say("and now?");
    sim.settle();
    assert_eq!(rejection(&sim), "Agent couldn't answer: the model refuses this conversation (the model could not read the screenshot)", "no repair ran in this run");
    sim.check_global();
}

/// Bug notes 26 Oct 00:52:29 and 00:54:39: the model request for the user's wake timed out six times in a row; Heimdall
/// resent it once, then nothing went out until he spoke again. A passing failure is resent after 5 s, 15 s and 45 s
/// (`notice::RETRY_DELAYS_MS`), each resend marked, and the wake gets through on the fourth attempt.
#[test]
fn r1_a_timed_out_wake_retries_with_growing_delays_until_it_gets_through() {
    let mut sim = Sim::new(EVENING_MS);
    for _ in 0..3 { reply(&sim, vec![Step::Think(38_000), Step::Fail("Request timed out.".into())]); }
    reply(&sim, reply_speak());
    let words = "forget about it, for shelvish job";
    sim.say(words);
    sim.settle();
    let tries: Vec<Input> = sim.wakes().into_iter().filter(|i| i.text.contains(words)).collect();
    assert_eq!(tries.len(), 4, "{tries:#?}");
    for (n, pair) in tries.windows(2).enumerate() {
        let gap = pair[1].at - pair[0].at;
        assert_eq!(gap, 38_000 + crate::notice::RETRY_DELAYS_MS[n], "resend {n} waits its delay after the failure");
        assert!(pair[1].text.contains("resent after a delivery interruption"));
    }
    let outcomes: Vec<_> = sim.resolutions.iter().map(|r| r.outcome).collect();
    use crate::Resolved::{Resent, Retrying};
    assert_eq!(outcomes, [Retrying, Resent, Retrying, Resent, Retrying, Resent]);
    assert!(!sim.bridge.retry_pending());
    sim.check_global();
}

/// The same timeouts with no end: after the third resend fails the wake is given up (one `Failed`, for the island's
/// "gave up, say it again"), nothing more goes out by itself, and his words stay pending for his next wake.
#[test]
fn r2_a_wake_that_keeps_timing_out_is_given_up_after_three_resends() {
    let mut sim = Sim::new(EVENING_MS);
    for _ in 0..4 { reply(&sim, vec![Step::Think(38_000), Step::Fail("Request timed out.".into())]); }
    sim.say("what's the weather");
    sim.settle();
    sim.advance(5 * MIN);
    assert_eq!(sim.wakes().iter().filter(|i| i.text.contains("what's the weather")).count(), 4);
    let failed: Vec<_> = sim.resolutions.iter().filter(|r| r.outcome == crate::Resolved::Failed).collect();
    assert_eq!(failed.len(), 1);
    assert!(crate::notice::transient_failure(&failed[0].reason));
    assert!(!sim.bridge.retry_pending() && sim.bridge.retry_deadline().is_none());
    sim.say("Heimdall, again");
    assert!(sim.wakes().last().unwrap().text.contains("Heimdall, again"), "his next words go out at once");
    sim.settle();
    sim.check_global();
}

/// The user speaks while a resend waits out its delay: it goes at once with his new words, not 45 s later.
#[test]
fn r3_new_words_during_a_retry_delay_send_it_now() {
    let mut sim = Sim::new(EVENING_MS);
    reply(&sim, vec![Step::Think(2_000), Step::Fail("Request timed out.".into())]);
    sim.say("open the plan");
    sim.advance(3 * S);
    let due = sim.bridge.retry_deadline().expect("a resend waits");
    assert_eq!(due - sim.now(), crate::notice::RETRY_DELAYS_MS[0] - S);
    sim.say("the day plan");
    let last = sim.wakes().last().unwrap().clone();
    assert!(last.at < due, "it went before its delay ended");
    assert!(last.text.contains("the day plan") && last.text.contains("open the plan"), "{}", last.text);
    sim.settle();
    sim.check_global();
}

/// A refusal or an unrecognised error is not a passing failure: one resend at once, as before (E1.14).
#[test]
fn r4_other_failures_keep_their_single_immediate_resend() {
    let mut sim = Sim::new(EVENING_MS);
    for _ in 0..2 { reply(&sim, vec![Step::Think(1_000), Step::Fail("something odd".into())]); }
    sim.say("check the build");
    sim.settle();
    let tries: Vec<Input> = sim.wakes().into_iter().filter(|i| i.text.contains("check the build")).collect();
    assert_eq!(tries.len(), 2);
    assert_eq!(tries[1].at - tries[0].at, 1_000);
    sim.check_global();
}

/// 26 Oct 23:33–23:35: ChatGPT's usage limit was spent and every wake went out twice ("resent after a delivery
/// interruption"), each failing in about 11 s. A spent limit lasts until its window resets: the wake is not resent,
/// the island names the limit, and the user's words ride his next wake.
#[test]
fn r5_a_spent_usage_limit_is_never_resent() {
    let mut sim = Sim::new(EVENING_MS);
    let limit = "The ChatGPT user has reached their Subscription Sharing usage limit. Ask the user to try again after their usage limit resets or use an API key instead.";
    for _ in 0..2 { reply(&sim, vec![Step::Think(11_000), Step::Fail(limit.into())]); }
    sim.say("looking for");
    sim.settle();
    sim.advance(5 * MIN);
    assert_eq!(sim.wakes().iter().filter(|i| i.text.contains("looking for")).count(), 1, "sent once, never resent");
    assert!(!sim.wakes().iter().any(|i| i.text.contains("resent after a delivery interruption")));
    let outcomes: Vec<_> = sim.resolutions.iter().map(|r| (r.outcome, r.reason.clone())).collect();
    assert_eq!(outcomes, [(crate::Resolved::Rejected, "Agent couldn't answer: the plan's usage limit is used up; your words go with the next request".to_owned())]);
    assert!(!sim.bridge.retry_pending() && sim.bridge.retry_deadline().is_none());
    sim.say("Heimdall, hello");
    let next = sim.wakes().last().unwrap().text.clone();
    assert!(next.contains("Heimdall, hello") && next.contains("looking for"), "his words go with the next wake: {next}");
    sim.settle();
    sim.check_global();
}
