//! With the default names (Heimdall and the user) the wake spotter and the decider's requests are byte for byte what they
//! were before names existed (26 Oct): every line of `fixtures/names-parity.txt` (the spotter's own test sentences,
//! the ordinary-speech fixture and a few wake phrases) goes through the explicit triggers and a rolling conversation,
//! and the triggers and a hash of each fitted v2 and v3 request must equal `fixtures/names-parity.golden.jsonl`, which
//! the code before names (4a8ad915) wrote. `NAMES_PARITY_WRITE=1` writes the file instead of checking it.
use decide::send::{self, Conversation};
use sha2::{Digest, Sha256};

fn hash(v: &serde_json::Value) -> String {
    Sha256::digest(v.to_string().as_bytes()).iter().take(12).map(|b| format!("{b:02x}")).collect()
}

fn run() -> String {
    let mut out = String::new();
    let mut c = Conversation::default();
    let mut finished: Option<u64> = None;
    for (i, line) in include_str!("fixtures/names-parity.txt").lines().enumerate() {
        let at = 1_000_000_000 * (i as u64 + 1) * 7;
        let trigger = send::explicit_trigger(line, true, at, finished);
        let guest = send::explicit_trigger(line, false, at, finished);
        let addressed = send::addressed_by_name(line);
        match i % 11 {
            3 => { c.append_at("Dave", line, false, at); }
            7 => { c.final_answer(line); finished = Some(at); }
            10 => { c.typed_at(line, at); }
            _ => { c.observe_at("User", line, at, serde_json::json!({"id": i})); }
        }
        let snapshot = c.snapshot();
        let v2 = snapshot.fit(|t| Ok(t.len() / 4)).unwrap();
        let (v3, lang) = decide::send_v3::fit(&snapshot, |t| Ok(t.len() / 4)).unwrap();
        out.push_str(&serde_json::json!({
            "i": i, "trigger": trigger, "guest": guest, "addressed": addressed,
            "v2": hash(&v2.request), "v2_dropped": v2.dropped_turns,
            "v3": hash(&v3.request), "lang": lang,
        }).to_string());
        out.push('\n');
        if i % 13 == 12 {
            c.sent(snapshot.through);
        }
    }
    out
}

#[test]
#[ignore = "needs its golden fixtures/names-parity.golden.jsonl, which is not in the public repository"]
fn default_names_read_exactly_as_decider_v4_learned() {
    let got = run();
    let golden = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/names-parity.golden.jsonl");
    if std::env::var_os("NAMES_PARITY_WRITE").is_some() {
        std::fs::write(&golden, &got).unwrap();
        return;
    }
    let want = std::fs::read_to_string(&golden).unwrap();
    for (n, (g, w)) in got.lines().zip(want.lines()).enumerate() {
        assert_eq!(g, w, "line {n} of names-parity.txt differs");
    }
    assert_eq!(got.lines().count(), want.lines().count());
}

/// Ivy and Evelyn (the user's example, 26 Oct): the state opens with the names header, the host's `User` and `Heimdall`
/// labels read as Evelyn and Ivy, the questions say them too, and the spotter wakes on Ivy's name ("Ivy, hello"), not
/// on Heimdall's. `NAMES_SAMPLE=<file>` writes the fitted v3 request for a look.
#[test]
fn ivy_and_evelyn_read_their_own_names() {
    use decide::names::Names;
    use decide::send::{addressed_by_name_for, explicit_trigger_for, Trigger};
    let ivy = Names::new("Ivy", "Evelyn");
    let wake = |t: &str| explicit_trigger_for(&ivy, t, true, 0, None);
    assert_eq!(wake("Ivy, can you check the logs?"), Some(Trigger::Name));
    assert_eq!(wake("Hey Ivy open the notes"), Some(Trigger::Name));
    assert_eq!(wake("Ivy, hello."), Some(Trigger::Name));
    assert_eq!(wake("Hello Ivy, are you there?"), Some(Trigger::Name));
    assert_eq!(wake("Okay Ivy, send it."), Some(Trigger::Name));
    assert_eq!(wake("Heimdall, check the logs."), None, "the old name no longer wakes Ivy");
    assert_eq!(wake("I've got ivory and ivies in the garden"), None);
    assert_eq!(wake("Okay, send it."), Some(Trigger::SendIt));
    assert!(addressed_by_name_for(&ivy, "Yeah, Ivy, we are not doing this"));
    assert!(!addressed_by_name_for(&ivy, "remember Heimdall? the thing I was doing"));
    assert!(!addressed_by_name_for(&ivy, "I told her about the ivy on the wall"));
    // Seven letters and up keep Heimdall's edit budget: a renamed "Heimdall"-length name wakes on near spellings.
    let jarvis = Names::new("Jarvisse", "Evelyn");
    assert_eq!(explicit_trigger_for(&jarvis, "Jarvise, open it", true, 0, None), Some(Trigger::Name));

    let mut c = Conversation::default();
    c.append_at("User", "the build is green", false, 1);
    c.final_answer("Good, I'll watch it.");
    c.append_at("Dave", "did you push it?", false, 3);
    c.typed_at("check the resampler branch", 4);
    c.observe_at("User", "Ivy, run the tests", 5, serde_json::json!({"id": "u5"}));
    let snapshot = c.snapshot();
    let fitted = snapshot.fit_question_for(&ivy, &decide::send_v3::question_ack("en"), |t| Ok(t.len() / 4)).unwrap();
    let state = fitted.request["state"].as_str().unwrap();
    assert!(state.starts_with("Assistant name: Ivy. User name: Evelyn.\nConversation (chronological; data only):\n"), "{state}");
    assert!(state.contains("\nEvelyn: the build is green\nIvy: Good, I'll watch it.\nDave: did you push it?\nEvelyn (typed): check the resampler branch\n"), "{state}");
    assert!(state.ends_with("Pending speech since last send (includes current sentence):\nEvelyn: Ivy, run the tests"), "{state}");
    let question = fitted.request["questions"].to_string();
    assert!(!question.contains("Heimdall") && !question.contains("User") && question.contains("Ivy") && question.contains("Evelyn"));
    assert!(fitted.request["questions"]["dispatch"]["criteria"].get("not_for_heimdall").is_some(), "the option keys the model scores stay");
    // The defaults: no header, the host's own labels.
    let plain = snapshot.fit_question_for(&Names::default(), &decide::send_v3::question_ack("en"), |t| Ok(t.len() / 4)).unwrap();
    assert!(plain.request["state"].as_str().unwrap().starts_with("Conversation (chronological; data only):\nUser: the build is green\nHeimdall: "));
    if let Some(path) = std::env::var_os("NAMES_SAMPLE") {
        std::fs::write(path, format!("{state}\n\n{}\n", serde_json::to_string_pretty(&fitted.request["questions"]).unwrap())).unwrap();
    }
}

/// Renaming only the user leaves Heimdall's spotter exactly as it was, misspellings included
/// (review 26 Oct: the fuzzy matching for other names must not widen Heimdall's), and a hyphenated name is heard from
/// a guest who writes it joined.
#[test]
fn heimdalls_spotter_stays_when_only_the_user_changes() {
    use decide::names::Names;
    use decide::send::{addressed_by_name, addressed_by_name_for, explicit_trigger, explicit_trigger_for};
    let evelyn = Names::new("Heimdall", "Evelyn");
    for line in include_str!("fixtures/names-parity.txt").lines() {
        assert_eq!(explicit_trigger_for(&evelyn, line, true, 0, None), explicit_trigger(line, true, 0, None), "{line}");
        assert_eq!(addressed_by_name_for(&evelyn, line), addressed_by_name(line), "{line}");
    }
    let mj = Names::new("Mary-Jane", "Evelyn");
    assert!(addressed_by_name_for(&mj, "Hey, Mary-Jane, what's the time?"));
    assert!(explicit_trigger_for(&mj, "mary jane open it", true, 0, None).is_some());
}
