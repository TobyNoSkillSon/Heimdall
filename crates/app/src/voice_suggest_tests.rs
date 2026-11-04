//! New-voice suggestions on transcript fixtures (an online call with one new voice, a meeting with three): when a card
//! shows, what it says, how often, and which names it guesses. No model runs: `ask` is a stand-in.
use super::*;
use crate::voice_namer::VoiceState;
use rusqlite::{params, Connection};
use speakers::embedding::{Embedding, DIMENSION};

const MIN: i64 = 60_000;
/// Tue 3 Nov 2026 17:00 UTC.
const NOW: i64 = 1_793_725_200_000;
const MARI: &str = "Speaker Tue 3 Nov 17h #1";
const B: &str = "Speaker Tue 3 Nov 17h #2";
const C: &str = "Speaker Tue 3 Nov 17h #3";

fn db() -> Connection {
    let c = Connection::open_in_memory().unwrap();
    c.execute_batch("CREATE TABLE utterances(session INTEGER NOT NULL, id TEXT NOT NULL, wall_ms INTEGER, source_id TEXT, source TEXT, speaker TEXT, start_ns INTEGER, end_ns INTEGER, text TEXT, words TEXT, notes TEXT, decision TEXT, PRIMARY KEY(session, id));").unwrap();
    c
}
fn say(c: &Connection, wall: i64, speaker: &str, text: &str) {
    let id: i64 = c.query_row("SELECT count(*) FROM utterances", [], |r| r.get(0)).unwrap();
    c.execute("INSERT INTO utterances(session,id,wall_ms,source_id,source,speaker,start_ns,end_ns,text) VALUES(1,?1,?2,'discord','Discord',?3,?4,?5,?6)",
        params![format!("s:{id}"), wall, speaker, wall * 1_000_000, wall * 1_000_000 + 1, text]).unwrap();
}
fn unit(i: usize) -> Embedding {
    let mut v = vec![0.; DIMENSION];
    v[i] = 1.;
    Embedding::new(v).unwrap()
}
/// A voice whose samples score `cos` against `unit(0)`.
fn near(cos: f32) -> Embedding {
    let mut v = vec![0.; DIMENSION];
    v[0] = cos;
    v[1] = (1. - cos * cos).sqrt();
    Embedding::new(v).unwrap()
}
fn store(codes: &[(&str, Embedding)]) -> EnrollmentStore {
    let mut s = EnrollmentStore::default();
    s.enroll("Kanan", unit(0)).unwrap();
    for (c, e) in codes {
        s.voices.insert((*c).into(), speakers::identity::CodedVoice {
            embeddings: vec![e.clone()],
            heard_in: "Discord".into(), quote: String::new(), first_ms: 0, last_ms: 0, calls: 1, days: Default::default(), irrelevant: false,
        });
    }
    s
}
fn in_call(codes: &[&str]) -> Vec<String> {
    codes.iter().map(|c| c.to_string()).collect()
}
/// The user's online call: a new voice says a few things in the last five minutes; the user calls her Mari twice.
fn online_call(c: &Connection) {
    say(c, NOW - 5 * MIN, "User", "Mari, can you hear me okay?");
    say(c, NOW - 5 * MIN + 3_000, MARI, "Yeah I can hear you fine now.");
    say(c, NOW - 4 * MIN, "User", "So what did you think of the build?");
    say(c, NOW - 4 * MIN + 4_000, MARI, "It was way faster than last week, honestly.");
    say(c, NOW - 3 * MIN, "User", "Mari, did Ellie send you the link?");
    say(c, NOW - 3 * MIN + 2_000, MARI, "No, not yet.");
    say(c, NOW - 2 * MIN, MARI, "Okay.");
}

#[test]
fn a_new_voice_in_the_call_gets_one_card_with_a_sentence_it_just_said() {
    let c = db();
    online_call(&c);
    let s = store(&[(MARI, unit(5))]);
    let card = next(&c, &s, &State::default(), &in_call(&[MARI]), NOW as u64).unwrap().expect("a card");
    // "No, not yet." and "Okay." are shorter: the newest line of four words or more is quoted.
    assert_eq!(card, Suggestion { code: MARI.into(), quote: "It was way faster than last week, honestly.".into(), candidates: vec![], error: None });
    assert_eq!(card_text(&card), "New voice: \u{201c}It was way faster than last week…\u{201d}");
    // No guess yet: the name field (the island's) and Not important, nothing else.
    assert_eq!(card_options(&card), [NOT_IMPORTANT]);
}

#[test]
fn every_card_stays_on_the_islands_one_line() {
    let long = "So basically what I was trying to say before the connection dropped is that the whole plan changes";
    let s = Suggestion { code: MARI.into(), quote: long.into(), candidates: vec![Candidate { name: "Alexandra".into(), sure: false }], error: None };
    assert!(ui::island::object::question_fits_one_line(&card_text(&s)), "{}", card_text(&s));
}

#[test]
fn what_the_voice_said_is_counted_in_lines_and_words() {
    let c = db();
    online_call(&c);
    let h = heard(&c, MARI, NOW - RECENT_MS).unwrap();
    assert_eq!((h.lines, h.words, h.last_ms), (4, 19, NOW - 2 * MIN));
}

#[test]
fn no_card_for_a_voice_that_said_little_left_the_call_or_is_not_important() {
    let c = db();
    say(&c, NOW - MIN, MARI, "Yeah.");
    say(&c, NOW - MIN + 5_000, MARI, "Mm-hmm.");
    let s = store(&[(MARI, unit(5))]);
    assert_eq!(next(&c, &s, &State::default(), &in_call(&[MARI]), NOW as u64).unwrap(), None, "two short lines");
    online_call(&c);
    assert_eq!(next(&c, &s, &State::default(), &[], NOW as u64).unwrap(), None, "not in an open call");
    let mut gone = s.clone();
    gone.voices.get_mut(MARI).unwrap().irrelevant = true;
    assert_eq!(next(&c, &gone, &State::default(), &in_call(&[MARI]), NOW as u64).unwrap(), None, "Not important");
    // Talking, but its newest line is older than three minutes: the moment passed.
    assert_eq!(next(&c, &s, &State::default(), &in_call(&[MARI]), (NOW + 4 * MIN) as u64).unwrap(), None);
}

#[test]
fn cards_are_rate_limited_and_an_unanswered_card_snoozes() {
    let c = db();
    online_call(&c);
    let s = store(&[(MARI, unit(5))]);
    let mut state = State { shown: vec![(NOW - 4 * MIN) as u64], ..Default::default() };
    assert_eq!(next(&c, &s, &state, &in_call(&[MARI]), NOW as u64).unwrap(), None, "another card four minutes ago");
    state.shown = (1..=PER_DAY as i64).map(|h| (NOW - h * 60 * MIN) as u64).collect();
    assert_eq!(next(&c, &s, &state, &in_call(&[MARI]), NOW as u64).unwrap(), None, "six cards today");
    state.shown = vec![(NOW - 11 * MIN) as u64];
    assert!(next(&c, &s, &state, &in_call(&[MARI]), NOW as u64).unwrap().is_some());
    // A card left unanswered: not again until the snooze is over; then once more.
    state.voices.insert(MARI.into(), VoiceState { suggested_ms: NOW as u64 - 1, snoozed_until: NOW as u64 + SNOOZE_MS, ..Default::default() });
    assert_eq!(next(&c, &s, &state, &in_call(&[MARI]), NOW as u64).unwrap(), None);
    for (k, line) in ["I'm back, sorry about that.", "The dog needed to go out again.", "So where were we with it?"].iter().enumerate() {
        say(&c, NOW + SNOOZE_MS as i64 - 3 * MIN + k as i64 * MIN, MARI, line);
    }
    assert!(next(&c, &s, &state, &in_call(&[MARI]), NOW as u64 + SNOOZE_MS).unwrap().is_some());
}

/// A meeting: three new voices talking at once.
fn meeting(c: &Connection) {
    for (i, code) in [B, C, MARI].iter().enumerate() {
        let t = NOW - 4 * MIN + i as i64 * 20_000;
        say(c, t, code, "I think the deadline is too tight for this.");
        say(c, t + 60_000, code, "We could split the work between the two teams.");
        say(c, t + 120_000, code, "Let's decide that on Friday then.");
    }
}

#[test]
fn several_new_voices_get_the_same_simple_card_one_at_a_time() {
    let c = db();
    meeting(&c);
    let s = store(&[(MARI, unit(5)), (B, unit(6)), (C, unit(7))]);
    let mut state = State::default();
    let card = next(&c, &s, &state, &in_call(&[B, C, MARI]), NOW as u64).unwrap().expect("a card");
    assert_eq!(card.code, MARI, "the one that spoke last");
    assert_eq!(card_text(&card), "New voice: \u{201c}Let's decide that on Friday then.\u{201d}", "no count, no question about the others");
    assert_eq!(card_options(&card), [NOT_IMPORTANT]);
    // Shown (and left unanswered): the next voice waits for the ten-minute gap, then gets its own card.
    state.shown.push(NOW as u64);
    state.voices.insert(MARI.into(), VoiceState { suggested_ms: NOW as u64, snoozed_until: NOW as u64 + SNOOZE_MS, ..Default::default() });
    assert_eq!(next(&c, &s, &state, &in_call(&[B, C, MARI]), NOW as u64 + 60_000).unwrap(), None, "one card per ten minutes");
    let later = NOW + 10 * MIN;
    for (k, code) in [B, C, MARI].iter().enumerate() {
        let t = later - 3 * MIN + k as i64 * 20_000;
        say(&c, t, code, "Back to the deadline, it really is too tight.");
        say(&c, t + 60_000, code, "We should ask for one more week at least.");
        say(&c, t + 120_000, code, "Fine, Friday it is.");
    }
    let card = next(&c, &s, &state, &in_call(&[B, C, MARI]), later as u64).unwrap().expect("the next voice's card");
    assert_eq!(card.code, C, "the one that spoke last, not the voice already asked about");
    assert_eq!(card_text(&card), "New voice: \u{201c}Fine, Friday it is.\u{201d}");
}

fn answer(candidates: serde_json::Value) -> impl FnMut(&str) -> Result<String> {
    let text = json!({"candidates": candidates, "reason": ""}).to_string();
    move |prompt: &str| {
        assert!(prompt.starts_with("Heard in: Discord.") && prompt.contains("VOICE: Yeah I can hear you fine now."), "{prompt}");
        Ok(text.clone())
    }
}

fn guessed(c: &Connection, s: &EnrollmentStore, state: &State, ask: Option<&mut dyn FnMut(&str) -> Result<String>>) -> Suggestion {
    let mut sug = Suggestion { code: MARI.into(), quote: "It was way faster than last week, honestly.".into(), candidates: vec![], error: None };
    guess(c, s, state, &mut sug, ask).unwrap();
    sug
}

#[test]
fn the_card_offers_at_most_two_real_guesses_said_around_the_voice() {
    let c = db();
    online_call(&c);
    let lines = crate::voice_namer::context(&c, MARI).unwrap();
    let n = |t: &str| lines.iter().find(|l| l.text == t).unwrap().n;
    let mut ask = answer(json!([
        {"name": "Ellie", "confidence": "low", "hints": [n("Mari, did Ellie send you the link?")]},
        {"name": "Mari", "confidence": "high", "hints": [n("Mari, can you hear me okay?"), n("Mari, did Ellie send you the link?")]},
        {"name": "Bob", "confidence": "medium", "hints": []},
        {"name": "User", "confidence": "high", "hints": []},
    ]));
    // The voice sounds a little like Kanan (0.42): a guess after the model's, never sure.
    let s = store(&[(MARI, near(0.42))]);
    let card = guessed(&c, &s, &State::default(), Some(&mut ask));
    assert_eq!(card.candidates, vec![
        Candidate { name: "Mari".into(), sure: true },
        Candidate { name: "Kanan".into(), sure: false },
    ], "a low hunch (Ellie) is no real guess; Bob was never said; User is never a guess");
    // The quote, the names, (the island's name field), Not important: one card, no question, no second step.
    assert_eq!(card_text(&card), "New voice: \u{201c}It was way faster than last week…\u{201d}");
    assert_eq!(card_options(&card), ["Mari", "Kanan", NOT_IMPORTANT]);
}

#[test]
fn a_name_turned_down_or_said_once_is_never_sure() {
    let c = db();
    online_call(&c);
    let lines = crate::voice_namer::context(&c, MARI).unwrap();
    let hint = lines.iter().find(|l| l.text == "Mari, can you hear me okay?").unwrap().n;
    let mut ask = answer(json!([{"name": "Mari", "confidence": "high", "hints": [hint]}]));
    let s = store(&[(MARI, unit(5))]);
    let card = guessed(&c, &s, &State::default(), Some(&mut ask));
    assert_eq!(card.candidates, vec![Candidate { name: "Mari".into(), sure: false }], "one hint");
    let mut declined = State::default();
    declined.voices.insert(MARI.into(), VoiceState { declined: vec!["Mari".into()], ..Default::default() });
    let card = guessed(&c, &s, &declined, Some(&mut ask));
    assert!(card.candidates.is_empty());
}

#[test]
fn with_no_real_guess_the_card_is_the_quote_the_field_and_not_important() {
    let c = db();
    online_call(&c);
    // No model (none set up) and no voice it sounds like.
    let s = store(&[(MARI, unit(5))]);
    let card = guessed(&c, &s, &State::default(), None);
    assert!(card.candidates.is_empty() && card.error.is_none());
    assert_eq!(card_options(&card), [NOT_IMPORTANT]);
    // Only low hunches: no chip either.
    let mut ask = answer(json!([{"name": "Ellie", "confidence": "low", "hints": []}]));
    assert_eq!(card_options(&guessed(&c, &s, &State::default(), Some(&mut ask))), [NOT_IMPORTANT]);
    // A model that fails leaves the voice-similarity guess and says why.
    let s = store(&[(MARI, near(0.43))]);
    let mut broken = |_: &str| -> Result<String> { anyhow::bail!("quota") };
    let card = guessed(&c, &s, &State::default(), Some(&mut broken));
    assert_eq!(card.candidates, vec![Candidate { name: "Kanan".into(), sure: false }]);
    assert_eq!(card.error.as_deref(), Some("quota"));
    assert_eq!(card_options(&card), ["Kanan", NOT_IMPORTANT]);
}

#[test]
fn a_voice_far_from_every_known_one_sounds_like_nobody() {
    let s = store(&[(MARI, near(0.39))]);
    assert_eq!(sounds_like(&s, MARI), None);
    let s = store(&[(MARI, near(0.41))]);
    assert_eq!(sounds_like(&s, MARI).map(|(n, _)| n), Some("Kanan".into()));
}

#[test]
fn the_look_has_its_own_prompt_and_a_strict_schema() {
    let body = crate::background_call::prepare(&call("claude-haiku-5-5", "x")).unwrap().body;
    assert_eq!(body["system"][0]["text"], SYSTEM);
    assert_eq!(body["output_config"]["format"]["schema"]["additionalProperties"], false);
    assert!(SYSTEM.contains("answers next") && SYSTEM.contains("Never answer User"));
}

#[test]
fn the_most_recent_unnamed_voice_is_the_one_named_latest() {
    let c = db();
    meeting(&c);
    say(&c, NOW - 90 * MIN, "Speaker Mon 2 Nov 20h #1", "an older call");
    let s = store(&[(MARI, unit(5)), (B, unit(6)), (C, unit(7)), ("Speaker Mon 2 Nov 20h #1", unit(8))]);
    let (code, quote, others) = crate::pipeline::latest_unnamed_for_tests(&c, &s, &[], NOW).unwrap();
    assert_eq!((code.as_str(), quote.as_str()), (MARI, "Let's decide that on Friday then."));
    assert_eq!(others, vec![C.to_string(), B.to_string()], "the older call's voice is another conversation");
    // In an open call, its voices come first.
    let (code, _, _) = crate::pipeline::latest_unnamed_for_tests(&c, &s, &in_call(&[B]), NOW).unwrap();
    assert_eq!(code, B);
    // Nobody unnamed in the last 12 hours.
    assert!(crate::pipeline::latest_unnamed_for_tests(&c, &s, &[], NOW + 13 * 60 * MIN).is_err());
}

#[test]
fn a_long_sentence_in_wide_letters_is_cut_to_the_line() {
    let s = Suggestion { code: MARI.into(), quote: "Whoever wins Monday's WWDC Workshop owes me two weeks of homework, I swear".into(), candidates: vec![], error: None };
    // The gallery's `question_new_voice_long` renders this line (lab/speakers/voice-naming/render/suggest/).
    assert_eq!(card_text(&s), "New voice: \u{201c}Whoever wins Monday's WWDC…\u{201d}");
}

/// Lab run on a real archive, read-only: the new-voice rule replayed minute by minute over a stretch of days, a voice
/// counted "in the call" while it spoke in the last three minutes, each card left unanswered. Prints the cards (no
/// guesses: no model runs).
/// `HEIMDALL_NAMER_DB=<archive>/heimdall.sqlite HEIMDALL_NAMER_STORE=<data>/speakers.json HEIMDALL_FROM_MS=… HEIMDALL_TO_MS=…
/// cargo test -p app --bin heimdall real_archive_suggestions -- --ignored --nocapture`
#[test]
#[ignore]
fn real_archive_suggestions() {
    let db = crate::voice_namer::open(std::path::Path::new(&std::env::var("HEIMDALL_NAMER_DB").unwrap())).unwrap();
    let store = EnrollmentStore::load(std::path::Path::new(&std::env::var("HEIMDALL_NAMER_STORE").unwrap())).unwrap();
    let from: i64 = std::env::var("HEIMDALL_FROM_MS").unwrap().parse().unwrap();
    let to: i64 = std::env::var("HEIMDALL_TO_MS").unwrap().parse().unwrap();
    let mut state = State::default();
    let mut q = db.prepare("SELECT DISTINCT speaker FROM utterances WHERE wall_ms>=?1 AND wall_ms<?2 AND speaker LIKE 'Speaker %'").unwrap();
    let mut t = from;
    let mut cards = 0;
    while t < to {
        let talking: Vec<String> = q.query_map(params![t - crate::voice_namer::ACTIVE_MS, t], |r| r.get(0)).unwrap().map(Result::unwrap).collect();
        if !talking.is_empty() {
            if let Some(s) = next(&db, &store, &state, &talking, t as u64).unwrap() {
                cards += 1;
                println!("{} {} | {} | in the call {:?}", crate::voice_namer::clock_for_tests(t), speakers::naming::voice_time(t as u64).day, card_text(&s), talking);
                state.shown.push(t as u64);
                let v = state.voices.entry(s.code).or_default();
                v.suggested_ms = t as u64;
                v.snoozed_until = t as u64 + SNOOZE_MS;
            }
        }
        t += MIN;
    }
    println!("{cards} card(s)");
}
