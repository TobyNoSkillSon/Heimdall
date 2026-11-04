//! The voice namer's rules on a synthetic archive: who is looked at, when the model is asked, and what its answer
//! must show before a card. No model runs: `ask` is a stand-in that counts its calls.
use super::*;
use rusqlite::{params, Connection};
use serde_json::json;
use speakers::{embedding::Embedding, identity::EnrollmentStore};

const MIN: i64 = 60_000;
const DAY: i64 = 24 * 60 * MIN;
/// Fri 23 Oct 2026 20:00 UTC.
const NOW: i64 = 1_792_785_600_000;

fn db() -> Connection {
    let c = Connection::open_in_memory().unwrap();
    c.execute_batch("CREATE TABLE utterances(session INTEGER NOT NULL, id TEXT NOT NULL, wall_ms INTEGER, source_id TEXT, source TEXT, speaker TEXT, start_ns INTEGER, end_ns INTEGER, text TEXT, words TEXT, notes TEXT, decision TEXT, PRIMARY KEY(session, id));").unwrap();
    c
}
fn say(c: &Connection, session: i64, wall: i64, speaker: &str, text: &str) {
    let id: i64 = c.query_row("SELECT count(*) FROM utterances", [], |r| r.get(0)).unwrap();
    c.execute("INSERT INTO utterances(session,id,wall_ms,source_id,source,speaker,start_ns,end_ns,text) VALUES(?1,?2,?3,'discord','Discord',?4,?5,?6,?7)",
        params![session, format!("s:{id}"), wall, speaker, wall * 1_000_000, wall * 1_000_000 + 1, text]).unwrap();
}
fn store(codes: &[&str]) -> EnrollmentStore {
    let mut s = EnrollmentStore::default();
    s.enroll("Angel", Embedding::new(vec![1.; speakers::embedding::DIMENSION]).unwrap()).unwrap();
    for c in codes {
        s.voices.insert((*c).into(), speakers::identity::CodedVoice {
            embeddings: vec![Embedding::new(vec![1.; speakers::embedding::DIMENSION]).unwrap()],
            heard_in: "Discord".into(), quote: String::new(), first_ms: 0, last_ms: 0, calls: 1, days: Default::default(), irrelevant: false,
        });
    }
    s
}
const DAVE: &str = "Speaker Wed 21 Oct 20h #2";
const ONE_OFF: &str = "Speaker Fri 23 Oct 19h #5";

/// A regular: two earlier evenings of an hour each with the user, then talking now.
fn regular(c: &Connection) {
    for (session, start) in [(1, NOW - 2 * DAY), (2, NOW - DAY)] {
        for m in 0..14 {
            let t = start + m * 5 * MIN;
            if m % 5 == 0 {
                say(c, session, t - 2_000, "User", "Dave, what do you think?");
            }
            say(c, session, t, DAVE, "yeah I think we should push it tonight");
        }
    }
    say(c, 3, NOW - MIN, DAVE, "okay I'm back");
}
fn counting(answer: serde_json::Value) -> (std::rc::Rc<std::cell::Cell<usize>>, impl FnMut(&str) -> anyhow::Result<String>) {
    let calls = std::rc::Rc::new(std::cell::Cell::new(0));
    let text = answer.to_string();
    let counter = calls.clone();
    (calls, move |_: &str| { counter.set(counter.get() + 1); Ok(text.clone()) })
}

#[test]
fn a_one_off_voice_costs_nothing_and_never_gets_a_card() {
    let c = db();
    // A random Discord call tonight: a voice talking for 40 minutes, never heard before.
    for m in 0..40 {
        say(&c, 3, NOW - 40 * MIN + m * MIN, ONE_OFF, "anyone want to queue");
    }
    let mut state = State::default();
    let calls = std::cell::Cell::new(0);
    let mut ask = |_: &str| { calls.set(calls.get() + 1); Ok(String::new()) };
    for minute in 0..30 {
        let out = pass(&c, &store(&[ONE_OFF]), &mut state, (NOW + minute * MIN) as u64, &mut ask).unwrap();
        assert!(!matches!(out, Outcome::Propose(_)));
    }
    assert_eq!(calls.get(), 0, "no model call for a voice heard on one day");
    let s = stats(&c, ONE_OFF).unwrap();
    assert_eq!((s.days.len(), s.lines), (1, 40));
    assert!(!s.eligible());
}

#[test]
fn a_regular_is_looked_at_while_talking_and_then_not_again_for_a_while() {
    let c = db();
    regular(&c);
    let s = stats(&c, DAVE).unwrap();
    assert!(s.eligible(), "{s:?}");
    assert!(s.days.len() >= 2 && s.minutes >= MIN_MINUTES);
    let (calls, mut ask) = counting(json!({"name": "", "confidence": "low", "hints": [], "quote_line": 0, "reason": "no name"}));
    let mut state = State::default();
    assert!(matches!(pass(&c, &store(&[DAVE]), &mut state, NOW as u64, &mut ask).unwrap(), Outcome::Unsure(_)));
    assert_eq!(calls.get(), 1);
    // A minute later: checked recently, no call.
    assert!(matches!(pass(&c, &store(&[DAVE]), &mut state, (NOW + MIN) as u64, &mut ask).unwrap(), Outcome::Waiting(_)));
    assert_eq!(calls.get(), 1);
    // Twenty minutes later but nothing new said: still no call.
    say(&c, 3, NOW + 20 * MIN, DAVE, "one more");
    assert!(matches!(pass(&c, &store(&[DAVE]), &mut state, (NOW + 21 * MIN) as u64, &mut ask).unwrap(), Outcome::Waiting(_)));
    assert_eq!(calls.get(), 1);
}

#[test]
fn one_long_call_past_midnight_is_not_a_return() {
    let c = db();
    // 23:00 to 01:00 local in one call: two days by the clock, one visit.
    let start = NOW - (NOW % DAY) + DAY - 60 * MIN - chrono_offset_ms();
    for m in 0..24 {
        say(&c, 1, start + m * 5 * MIN, ONE_OFF, "still here");
    }
    let s = stats(&c, ONE_OFF).unwrap();
    assert_eq!(s.days.len(), 2, "{s:?}");
    assert_eq!(s.visits, 1);
    assert!(!s.eligible());
}

/// The local zone's offset from UTC at `NOW`, in ms (the archive's days are local).
fn chrono_offset_ms() -> i64 {
    let secs = (NOW / 1000) as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&secs, &mut tm) };
    tm.tm_gmtoff * 1000
}

#[test]
fn close_spellings_of_a_longer_name_count_as_the_name() {
    assert!(same_name("fosta", "foster") && same_name("dave", "dave"));
    assert!(!same_name("ola", "ala"), "short names must match exactly");
    assert!(!same_name("foster", "poster") && !same_name("angel", "angela-marie"));
    assert!(says("Yeah, by the way, Morgen, some new model", "Morgan"));
}

#[test]
fn a_name_another_voice_answers_is_not_a_hint() {
    let c = db();
    // The user says "Erin, ..." three evenings, but someone else answers each time; the voice only speaks later.
    for (session, t) in [(1, NOW - 3 * DAY), (2, NOW - 2 * DAY), (3, NOW - DAY)] {
        say(&c, session, t, "User", "Erin, can you hear me?");
        say(&c, session, t + 2_000, "Angel", "she's not here");
        say(&c, session, t + 30_000, DAVE, "hi all");
    }
    let lines = context(&c, DAVE).unwrap();
    let hints: Vec<usize> = lines.iter().filter(|l| l.text.contains("Erin")).map(|l| l.n).collect();
    assert_eq!(hints.len(), 3);
    let answer = json!({"name": "Erin", "confidence": "high", "hints": hints, "quote_line": hints[0], "reason": ""}).to_string();
    assert!(verify(DAVE, &answer, &lines, &[], &[]).unwrap().is_none());
}

#[test]
fn a_regular_who_is_not_talking_now_is_left_alone() {
    let c = db();
    regular(&c);
    let (calls, mut ask) = counting(json!({}));
    let later = NOW + 30 * MIN;
    assert_eq!(pass(&c, &store(&[DAVE]), &mut State::default(), later as u64, &mut ask).unwrap(), Outcome::Idle);
    // A forgotten voice is never looked at.
    let mut s = store(&[DAVE]);
    s.set_irrelevant(DAVE, true);
    assert_eq!(pass(&c, &s, &mut State::default(), NOW as u64, &mut ask).unwrap(), Outcome::Idle);
    assert_eq!(calls.get(), 0);
}

#[test]
fn the_context_carries_the_address_before_the_voice_answers() {
    let c = db();
    regular(&c);
    let lines = context(&c, DAVE).unwrap();
    let address = lines.iter().position(|l| l.speaker == "User" && l.text.contains("Dave")).unwrap();
    assert!(lines[address + 1].own, "the voice answers next");
    let p = prompt(DAVE, &lines, &["Angel".into()], &[]);
    assert!(p.contains("User: Dave, what do you think?") && p.contains("VOICE: yeah") && p.contains("Angel"));
}

fn hint_lines(c: &Connection) -> (Vec<Line>, Vec<usize>) {
    let lines = context(c, DAVE).unwrap();
    let hints = lines.iter().filter(|l| l.text.contains("Dave")).map(|l| l.n).collect();
    (lines, hints)
}

#[test]
fn a_name_needs_several_independent_hints_checked_against_the_transcript() {
    let c = db();
    regular(&c);
    let (lines, hints) = hint_lines(&c);
    assert!(hints.len() >= 3);
    let ok = |answer: serde_json::Value, declined: &[String]| verify(DAVE, &answer.to_string(), &lines, &[], declined).unwrap();
    let good = json!({"name": "Dave", "confidence": "high", "hints": hints, "quote_line": hints[1], "reason": "addressed, answers"});
    let p = ok(good.clone(), &[]).expect("three addresses over two evenings");
    assert_eq!(p.name, "Dave");
    assert!(p.quote.starts_with("User: Dave,"), "{}", p.quote);
    assert_eq!(card_text(&p), "Is this Dave? \u{201c}User: Dave, what do you think?\u{201d}");
    // A name Heimdall already knows is another voice, also spelled as the recogniser drifts.
    assert!(verify(DAVE, &good.to_string(), &lines, &["dave".into()], &[]).unwrap().is_none());
    assert!(same_names("Morgen", "Morgan") && !same_names("Morgen", "Morgan Smith"));
    // Not sure enough, turned down before, the user himself, or a made-up line number.
    let mut medium = good.clone();
    medium["confidence"] = json!("medium");
    assert!(ok(medium, &[]).is_none());
    assert!(ok(good.clone(), &["dave".into()]).is_none());
    assert!(ok(json!({"name": "User", "confidence": "high", "hints": hints, "quote_line": hints[0], "reason": ""}), &[]).is_none());
    assert!(ok(json!({"name": "Dave", "confidence": "high", "hints": [hints[0], 9999, 9998], "quote_line": hints[0], "reason": ""}), &[]).is_none());
    // Lines that do not say the name are not hints.
    let others: Vec<usize> = lines.iter().filter(|l| l.own).take(3).map(|l| l.n).collect();
    assert!(ok(json!({"name": "Dave", "confidence": "high", "hints": others, "quote_line": others[0], "reason": ""}), &[]).is_none());
}

#[test]
fn one_moment_repeated_or_the_voice_saying_the_name_is_not_enough() {
    // Three addresses within one minute of one call: one moment.
    let c = db();
    for i in 0..3 {
        say(&c, 1, NOW - 10 * MIN + i * 10_000, "User", "Erin, can you hear me?");
        say(&c, 1, NOW - 10 * MIN + i * 10_000 + 2_000, DAVE, "yes");
    }
    let lines = context(&c, DAVE).unwrap();
    let hints: Vec<usize> = lines.iter().filter(|l| l.text.contains("Erin")).map(|l| l.n).collect();
    let answer = json!({"name": "Erin", "confidence": "high", "hints": hints, "quote_line": hints[0], "reason": ""}).to_string();
    assert!(verify(DAVE, &answer, &lines, &[], &[]).unwrap().is_none());
    // The voice itself says "John, ..." to someone: John is someone else.
    let c = db();
    for (session, t) in [(1, NOW - DAY), (2, NOW - 2 * DAY), (3, NOW - 3 * DAY)] {
        say(&c, session, t, "User", "John, are you there?");
        say(&c, session, t + 2_000, DAVE, "yep");
    }
    say(&c, 3, NOW - 3 * DAY + 60_000, DAVE, "John, give me the link");
    let lines = context(&c, DAVE).unwrap();
    let hints: Vec<usize> = lines.iter().filter(|l| !l.own && l.text.contains("John")).map(|l| l.n).collect();
    let answer = json!({"name": "John", "confidence": "high", "hints": hints, "quote_line": hints[0], "reason": ""}).to_string();
    assert!(verify(DAVE, &answer, &lines, &[], &[]).unwrap().is_none());
    // Its own introduction is fine.
    let c = db();
    for (session, t) in [(1, NOW - DAY), (2, NOW - 2 * DAY)] {
        say(&c, session, t, DAVE, "hey it's John here");
        say(&c, session, t + 60_000, "User", "John, you good?");
        say(&c, session, t + 62_000, DAVE, "yeah");
    }
    let lines = context(&c, DAVE).unwrap();
    let hints: Vec<usize> = lines.iter().filter(|l| l.text.contains("John")).map(|l| l.n).collect();
    let answer = json!({"name": "John", "confidence": "high", "hints": hints, "quote_line": hints[0], "reason": ""}).to_string();
    assert_eq!(verify(DAVE, &answer, &lines, &[], &[]).unwrap().unwrap().quote, "They said: hey it's John here");
}

#[test]
fn a_proposal_is_remembered_so_the_card_is_not_shown_again_soon() {
    let c = db();
    regular(&c);
    let (lines, hints) = hint_lines(&c);
    drop(lines);
    let (calls, mut ask) = counting(json!({"name": "Dave", "confidence": "high", "hints": hints, "quote_line": hints[0], "reason": ""}));
    let mut state = State::default();
    let out = pass(&c, &store(&[DAVE]), &mut state, NOW as u64, &mut ask).unwrap();
    assert!(matches!(&out, Outcome::Propose(p) if p.name == "Dave" && p.code == DAVE));
    assert_eq!(state.voices[DAVE].asked.as_ref().unwrap().0, "Dave");
    for _ in 0..40 {
        say(&c, 3, NOW + 25 * MIN, DAVE, "still here");
    }
    assert!(matches!(pass(&c, &store(&[DAVE]), &mut state, (NOW + 26 * MIN) as u64, &mut ask).unwrap(), Outcome::Waiting(_)));
    assert_eq!(calls.get(), 1);
}

#[test]
fn the_request_is_haiku_with_its_own_system_prompt_and_a_strict_schema() {
    let body = request_body("x");
    assert_eq!(body["model"], "claude-haiku-5-5");
    assert_eq!(body["system"][0]["text"], SYSTEM);
    assert_eq!(body["thinking"]["type"], "adaptive");
    assert_eq!(body["output_config"]["format"]["schema"]["additionalProperties"], false);
    assert!(body.get("temperature").is_none());
    assert!(SYSTEM.contains("answers next") && SYSTEM.contains("Never answer User"));
}

/// Lab run on a real archive, read-only: every call speaker label treated as if it were a coded voice. Prints who
/// would be looked at (came back, an hour of conversation) and writes the model's user message for the labels in
/// `HEIMDALL_NAMER_PROMPTS` (comma-separated) to `HEIMDALL_NAMER_OUT`, with known names excluding that label.
/// `HEIMDALL_NAMER_DB=<archive>/heimdall.sqlite cargo test -p app --bin heimdall real_archive -- --ignored --nocapture`
#[test]
#[ignore]
fn real_archive_eligibility() {
    let path = std::env::var("HEIMDALL_NAMER_DB").expect("HEIMDALL_NAMER_DB");
    let c = open(std::path::Path::new(&path)).unwrap();
    let labels: Vec<String> = c
        .prepare("SELECT DISTINCT speaker FROM utterances WHERE source='Discord' AND speaker NOT IN ('User','Heimdall','audio') AND speaker NOT LIKE 'Speaker %'")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let mut rows = vec![];
    for l in &labels {
        let s = stats(&c, l).unwrap();
        println!("{:<26} lines {:>4}  days {}  visits {}  minutes {:>4}  eligible {}", l, s.lines, s.days.len(), s.visits, s.minutes, s.eligible());
        rows.push(json!({"label": l, "lines": s.lines, "days": s.days, "visits": s.visits, "minutes": s.minutes, "eligible": s.eligible()}));
    }
    if let (Ok(want), Ok(out)) = (std::env::var("HEIMDALL_NAMER_PROMPTS"), std::env::var("HEIMDALL_NAMER_OUT")) {
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(format!("{out}/eligibility.json"), serde_json::to_vec_pretty(&rows).unwrap()).unwrap();
        for l in want.split(',') {
            let lines = context(&c, l).unwrap();
            let known: Vec<String> = labels.iter().filter(|k| *k != l && !k.starts_with("Speaker")).cloned().collect();
            std::fs::write(format!("{out}/prompt-{l}.txt"), prompt("Speaker Thu 22 Oct 20h #1", &lines, &known, &[])).unwrap();
            let lines_json: Vec<_> = lines.iter().map(|x| json!({"n": x.n, "session": x.session, "wall_ms": x.wall_ms, "speaker": x.speaker, "own": x.own, "text": x.text})).collect();
            std::fs::write(format!("{out}/lines-{l}.json"), serde_json::to_vec(&lines_json).unwrap()).unwrap();
            // A model answer saved beside it: what the code-side checks make of it, and the card it would show.
            if let Ok(answer) = std::fs::read_to_string(format!("{out}/answer-{l}.json")) {
                match verify("Speaker Thu 22 Oct 20h #1", &answer, &lines, &[], &[]).unwrap() {
                    Some(p) => println!("{l}: card \"{}\" hints {:?}", card_text(&p), p.hints),
                    None => println!("{l}: no card"),
                }
            }
        }
    }
}
