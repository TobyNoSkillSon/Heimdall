//! The word check on every take it rejected live, 20-27 Oct 2026 (`lab/word-check-1027/takes.jsonl`: 602 logged
//! rejections, the intended sentence recovered from the agent session, each labelled by hand from its transcript as a
//! real voice error, a false rejection or unsure). Night 1027: ~148 of the 152 rejections of 25-27 Oct were the
//! recognizer's spelling, and they switched 11 sentences to the lighter voice. No model runs here.
//!
//! Every take labelled real must still be rejected. The counts are printed (`--nocapture`); set
//! `WORD_CHECK_TAKES_OUT=<file>` to write each row's verdict for a before/after diff.
use serde_json::Value;

fn takes() -> Option<Vec<Value>> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../lab/word-check-1027/takes.jsonl");
    let text = std::fs::read_to_string(path).ok()?;
    Some(text.lines().filter_map(|l| serde_json::from_str(l).ok()).collect())
}

/// A row the check can be replayed on: a transcript was logged whole (17 end in "…", cut by the log line) and the
/// sentence is known.
fn replayable(t: &Value) -> bool {
    let heard = t["heard"].as_str().unwrap_or("");
    !heard.is_empty() && !heard.ends_with('…') && !t["expected"].as_str().unwrap_or("").is_empty()
}

#[test]
fn logged_takes_real_errors_stay_rejected_and_false_ones_are_counted() {
    let Some(takes) = takes() else {
        eprintln!("lab/word-check-1027/takes.jsonl not found: skipped");
        return;
    };
    let mut out = String::new();
    // (label, window) -> (replayed, rejected)
    let mut counts: std::collections::BTreeMap<(String, &str), (usize, usize)> = Default::default();
    let mut real_passed = Vec::new();
    for t in takes.iter().filter(|t| replayable(t)) {
        let (expected, heard) = (t["expected"].as_str().unwrap(), t["heard"].as_str().unwrap());
        let v = speak::coverage::check(expected, heard);
        let rejected = v["accepted"] != true;
        let label = t["label"].as_str().unwrap_or("unsure").to_owned();
        let late = t["ts"].as_str().unwrap_or("") >= "2026-10-25";
        for window in ["all", "25-27 Oct"].into_iter().filter(|w| *w == "all" || late) {
            let c = counts.entry((label.clone(), window)).or_default();
            c.0 += 1;
            c.1 += usize::from(rejected);
        }
        if label == "real" && !rejected {
            real_passed.push(format!("{} {expected:?} heard {heard:?}", t["ts"]));
        }
        out.push_str(&format!("{}\t{}\t{}\t{}\t{}\n", t["ts"].as_str().unwrap_or(""), label, t["category"].as_str().unwrap_or(""), if rejected { "rejected" } else { "accepted" }, v["edits"]));
    }
    for ((label, window), (n, rejected)) in &counts {
        eprintln!("{window:>9} {label:>6}: {rejected} of {n} still rejected");
    }
    if let Some(path) = std::env::var_os("WORD_CHECK_TAKES_OUT") {
        std::fs::write(path, out).unwrap();
    }
    assert!(real_passed.is_empty(), "real voice errors accepted:\n{}", real_passed.join("\n"));
    // Agreement across the takes of one sentence (`accept_agreed_take`), in log order: a take rejected for the same
    // wrong words as an earlier take of the same text passes. Keyed by text across all nights, which is looser than one
    // reply: a real voice error must never be agreed.
    let mut earlier: std::collections::HashMap<String, Vec<Value>> = Default::default();
    let mut agreed: std::collections::BTreeMap<String, usize> = Default::default();
    let mut real_agreed = Vec::new();
    for t in takes.iter().filter(|t| replayable(t)) {
        let (expected, heard) = (t["expected"].as_str().unwrap(), t["heard"].as_str().unwrap());
        let mut v = speak::coverage::check(expected, heard);
        if v["accepted"] == true {
            continue;
        }
        let prior = earlier.entry(expected.to_owned()).or_default();
        let label = t["label"].as_str().unwrap_or("unsure").to_owned();
        if speak::coverage::accept_agreed_take(&mut v, prior) {
            *agreed.entry(label.clone()).or_default() += 1;
            if label == "real" {
                real_agreed.push(format!("{} {expected:?} heard {heard:?}", t["ts"]));
            }
        } else {
            prior.push(v);
        }
    }
    for (label, n) in &agreed {
        eprintln!("      all {label:>6}: {n} more accepted by agreement with an earlier take");
    }
    assert!(real_agreed.is_empty(), "real voice errors agreed:\n{}", real_agreed.join("\n"));
}
