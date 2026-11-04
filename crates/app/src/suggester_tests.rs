//! The suggester's rules: when the stub speaks up, the pacing, the answer's checks, the card, the handoff, the feedback
//! phrases, the file-search filter and the label export. No model call here (`ask` is exercised with a scripted run in
//! `background_call_tests.rs`, and by the replay).
use super::*;
use serde_json::json;

fn line(at_ms: u64, speaker: &str, text: &str, owner: bool) -> Line {
    Line { at_ms, speaker: speaker.into(), text: text.into(), owner }
}

fn window(lines: &[Line]) -> Window {
    let mut w = Window::default();
    for l in lines {
        w.push(l.clone());
    }
    w
}

const NOW: u64 = 1_800_000_000_000;

fn card_answer(extra: Value) -> String {
    let mut v = json!({
        "suggest": true,
        "topic": "CAD file from the old laptop",
        "questions": [{"text": "Should I look for it?", "options": ["Find the file", "Investigate it", "Not now"], "multi_select": false}],
        "found": [],
        "handoff": "User half-remembers a CAD file from his old laptop; search it by name."
    });
    if let (Value::Object(base), Value::Object(more)) = (&mut v, extra) {
        base.extend(more);
    }
    v.to_string()
}

#[test]
fn the_stub_speaks_up_only_on_owners_own_line_with_someone_else_talking() {
    let talk = [line(NOW - 60_000, "Sam", "so what did we build last week", false), line(NOW, "User", "I remember there was a CAD file like this", true)];
    assert!(stub_suggest_now(&window(&talk)));
    // Someone else's line, a line with no cue, a line to Heimdall, and talk with nobody else in it: all quiet.
    assert!(!stub_suggest_now(&window(&[line(NOW, "Sam", "I remember there was a CAD file", false)])));
    assert!(!stub_suggest_now(&window(&[line(NOW - 60_000, "Sam", "hi", false), line(NOW, "User", "nice weather today", true)])));
    assert!(!stub_suggest_now(&window(&[line(NOW - 60_000, "Sam", "hi", false), line(NOW, "User", "Heimdall, do you remember the CAD file?", true)])));
    assert!(!stub_suggest_now(&window(&[line(NOW, "User", "I remember there was a CAD file", true)])));
    // The other voice spoke more than five minutes ago: not a conversation now.
    assert!(!stub_suggest_now(&window(&[line(NOW - 6 * 60_000, "Sam", "hi", false), line(NOW, "User", "I remember a file", true)])));
    // A half-remembered name is a cue too.
    assert!(stub_suggest_now(&window(&[line(NOW - 60_000, "Sam", "which one", false), line(NOW, "User", "what was it called, the drawing program", true)])));
    // Everyday talk with "something like", "remember" or "look into" in it does not start a pass (the replay, 24 Oct).
    for everyday in ["something like that was funny", "I don't remember anything about it, honestly, that was a long day", "look into the screen for me", "Remember when we went to the lake?"] {
        assert!(!stub_suggest_now(&window(&[line(NOW - 60_000, "Sam", "hi", false), line(NOW, "User", everyday, true)])), "{everyday}");
    }
}

#[test]
fn the_window_keeps_ten_minutes_and_sixty_lines() {
    let mut w = Window::default();
    w.push(line(0, "Sam", "old", false));
    w.push(line(WINDOW_MS + 1, "User", "new", true));
    assert_eq!(w.transcript().lines().count(), 1, "the line older than ten minutes is gone");
    for i in 0..(WINDOW_LINES as u64 + 5) {
        w.push(line(WINDOW_MS + 2 + i, "Sam", "x", false));
    }
    assert_eq!(w.lines().count(), WINDOW_LINES);
    assert_eq!(w.people(), vec!["Sam".to_owned()], "User is not one of the people");
}

#[test]
fn one_call_a_minute_one_card_every_three_minutes_and_no_repeat_topic() {
    let mut p = Pacer::default();
    assert!(p.may_call(NOW) && p.may_show(NOW, "CAD file"));
    p.called(NOW);
    assert!(!p.may_call(NOW + 59_000) && p.may_call(NOW + CALL_GAP_MS));
    p.shown(NOW, "CAD file");
    assert!(!p.may_call(NOW + 10 * 60_000), "no call while a card is up");
    assert!(!p.may_show(NOW + GAP_MS, "Sam's photos"), "a card is still up");
    p.closed();
    assert!(!p.may_show(NOW + GAP_MS - 1, "Sam's photos"), "one card every three minutes");
    assert!(!p.may_show(NOW + GAP_MS, "CAD file"), "the same topic is not offered again within two hours");
    assert!(p.may_show(NOW + GAP_MS, "Sam's photos"));
    assert!(p.may_show(NOW + TOPIC_MS, "CAD file"), "after two hours the topic may come back");
}

#[test]
fn two_dismissals_in_a_row_pause_for_fifteen_minutes_and_a_pick_resets_the_count() {
    let mut p = Pacer::default();
    p.resolved(NOW, Outcome::Dismissed);
    p.resolved(NOW + 1, Outcome::Picked);
    p.resolved(NOW + 2, Outcome::Dismissed);
    assert!(!p.paused(NOW + 3), "a pick in between resets the count");
    p.resolved(NOW + 4, Outcome::Dismissed);
    assert!(p.paused(NOW + 5) && p.paused(NOW + PAUSE_MS + 3));
    assert!(!p.may_call(NOW + 5) && !p.may_show(NOW + 5, "anything"));
    assert!(!p.paused(NOW + 4 + PAUSE_MS));
    p.resolved(NOW + 6, Outcome::Ignored);
    assert!(!p.paused(NOW + 6 + PAUSE_MS), "an ignored card neither counts nor resets");
}

#[test]
fn topics_match_on_their_words() {
    assert!(same_topic("CAD file from the old laptop", "the CAD file on my old laptop"));
    assert!(!same_topic("CAD file", "Sam's holiday photos"));
    assert!(!same_topic("", "anything"));
}

#[test]
fn a_silent_answer_is_silent_and_a_card_is_checked_in_code() {
    assert!(matches!(verify(&json!({"suggest": false, "topic": "nothing", "questions": [], "found": [], "handoff": ""}).to_string(), &[]).unwrap(), Verdict::Silent(_)));
    let Verdict::Card(s) = verify(&card_answer(json!({})), &[]).unwrap() else { panic!("a valid card") };
    assert_eq!(s.topic, "CAD file from the old laptop");
    assert_eq!(s.questions[0].id, "q1");
    assert_eq!(s.questions[0].options, vec!["Find the file", "Investigate it", "Not now"]);
    assert!(!s.questions[0].multi_select);
}

#[test]
fn a_card_refuses_contacting_anyone_malformed_questions_and_too_many() {
    let contact = card_answer(json!({"questions": [{"text": "Should I message Sam?", "options": ["Yes", "Not now"], "multi_select": false}]}));
    assert_eq!(verify(&contact, &[]).unwrap(), Verdict::Silent("proposes contacting someone".into()));
    let option = card_answer(json!({"questions": [{"text": "What next?", "options": ["Email Priya", "Not now"], "multi_select": false}]}));
    assert!(matches!(verify(&option, &[]).unwrap(), Verdict::Silent(r) if r.contains("contacting")));
    let one = card_answer(json!({"questions": [{"text": "Next?", "options": ["Only one"], "multi_select": false}]}));
    assert!(matches!(verify(&one, &[]).unwrap(), Verdict::Silent(r) if r.contains("question 1 is malformed")));
    let dup = card_answer(json!({"questions": [{"text": "Next?", "options": ["Same", "Same"], "multi_select": false}]}));
    assert!(matches!(verify(&dup, &[]).unwrap(), Verdict::Silent(_)));
    // A label past forty characters is cut to forty, not refused: the card still answers.
    let long = card_answer(json!({"questions": [{"text": "Next?", "options": ["A very long outcome that goes on and on and on past forty characters", "Not now"], "multi_select": false}]}));
    let Verdict::Card(s) = verify(&long, &[]).unwrap() else { panic!("a card") };
    assert_eq!(s.questions[0].options[0].chars().count(), 40);
    let four = json!({"text": "Q", "options": ["a", "b"], "multi_select": false});
    let many = card_answer(json!({"questions": [four.clone(), four.clone(), four.clone(), four]}));
    assert!(matches!(verify(&many, &[]).unwrap(), Verdict::Silent(r) if r.contains("not one to three")));
    let none = card_answer(json!({"questions": []}));
    assert!(matches!(verify(&none, &[]).unwrap(), Verdict::Silent(_)));
    assert!(verify("not json", &[]).is_err());
}

#[test]
fn found_paths_must_come_from_a_search_in_this_run() {
    let answer = card_answer(json!({"found": [
        {"path": "/Users/owner/Documents/cad/old.dwg", "note": "the drawing"},
        {"path": "/Users/owner/invented.dwg", "note": "not searched"}
    ]}));
    let seen = vec!["/Users/owner/Documents/cad/old.dwg".to_owned()];
    let Verdict::Card(s) = verify(&answer, &seen).unwrap() else { panic!() };
    assert_eq!(s.found, vec![Found { path: "/Users/owner/Documents/cad/old.dwg".into(), note: "the drawing".into() }]);
}

#[test]
fn only_not_now_dismisses_and_anything_else_is_a_pick() {
    let a = |labels: &[&str], other: Option<&str>| ui::agent_notice::QuestionAnswer { question_id: "q1".into(), labels: labels.iter().map(|l| l.to_string()).collect(), other: other.map(str::to_owned) };
    assert!(dismissed(&[a(&["Not now"], None)]));
    assert!(dismissed(&[a(&["not now"], None)]), "case does not matter");
    assert!(!dismissed(&[a(&["Find the file"], None)]));
    assert!(!dismissed(&[a(&["Not now"], Some("actually yes"))]), "typed text is a pick");
    assert!(!dismissed(&[]));
    // Sent with a question left (bug note 2026-11-02T16-27-21): only what he answered counts.
    let left = |id: &str| ui::agent_notice::QuestionAnswer { question_id: id.into(), labels: vec![], other: None };
    assert!(dismissed(&[a(&["Not now"], None), left("q2")]), "Not now on the one he answered dismisses");
    assert!(!dismissed(&[left("q1")]), "nothing answered is no dismissal");
    assert_eq!(choice(&[a(&["Find the file"], None), left("q2")]), "Find the file", "a question left adds nothing");
}

#[test]
fn the_card_asks_the_questions_with_other_allowed() {
    let Verdict::Card(s) = verify(&card_answer(json!({})), &[]).unwrap() else { panic!() };
    let n = card("suggest:abc", &s, NOW + CARD_MS);
    assert_eq!(n.id, "suggest:abc");
    assert_eq!(n.text, "CAD file from the old laptop");
    let Some(ui::agent_notice::NoticeAction::Ask { request_id, questions, expires_ms, .. }) = n.action else { panic!("an ask card") };
    assert_eq!(request_id, "suggest:abc");
    assert_eq!(expires_ms, NOW + CARD_MS);
    assert_eq!(questions.len(), 1);
    assert!(questions[0].allow_other && !questions[0].multi_select);
    assert_eq!(questions[0].options.iter().map(|o| o.label.as_str()).collect::<Vec<_>>(), vec!["Find the file", "Investigate it", "Not now"]);
}

#[test]
fn the_handoff_carries_the_suggestion_the_pick_and_the_talk() {
    let Verdict::Card(s) = verify(&card_answer(json!({"found": []})), &[]).unwrap() else { panic!() };
    let pick = vec![ui::agent_notice::QuestionAnswer { question_id: "q1".into(), labels: vec!["Find the file".into()], other: None }];
    let text = handoff(&s, &pick, NOW, false, "[10:02 Sam] the CAD file from the laptop");
    assert!(text.starts_with("[Heimdall suggestion, picked by User]"));
    assert!(text.contains("This was a suggestion from the island"));
    assert!(text.contains("Question \"Should I look for it?\": User picked Find the file."), "{text}");
    assert!(text.contains("Notes: User half-remembers"));
    assert!(text.contains("[10:02 Sam] the CAD file"));
    assert!(!text.contains("do not speak"), "no call, no call line");
    let during = handoff(&s, &pick, NOW, true, "");
    assert!(during.contains("User is on a call: answer on screen only, and do not speak."));
    let typed = vec![ui::agent_notice::QuestionAnswer { question_id: "q1".into(), labels: vec![], other: Some("the Dropbox one".into()) }];
    assert!(handoff(&s, &typed, NOW, false, "").contains("(typed: the Dropbox one)"));
}

#[test]
fn a_typed_bad_suggestion_or_irrelevant_is_feedback_and_long_talk_is_not() {
    assert_eq!(feedback("that was a bad suggestion"), Some("bad suggestion"));
    assert_eq!(feedback("Bad suggestion."), Some("bad suggestion"));
    assert_eq!(feedback("irrelevant"), Some("irrelevant"));
    assert_eq!(feedback("Not relevant, thanks"), Some("irrelevant"));
    assert_eq!(feedback("I think the suggestion about the bad suggestion stuff was irrelevant to what we were discussing today"), None);
    assert_eq!(feedback("find the CAD file"), None);
}

#[test]
fn spotlight_words_are_plain_and_file_results_are_filtered() {
    assert_eq!(query("  old-laptop CAD!! file  ").as_deref(), Some("oldlaptop CAD file"));
    assert_eq!(query("a b c d e").as_deref(), Some("a b c d"), "at most four words");
    assert_eq!(query("!!! ..."), None);
    assert!(search_files("", Path::new("/Users/owner"), Path::new("/Users/owner/Library/Application Support/Heimdall")).is_err());
    let home = Path::new("/Users/owner");
    let data = Path::new("/Users/owner/Library/Application Support/Heimdall");
    assert!(keep_path("/Users/owner/Documents/cad/old.dwg", home, data));
    assert!(!keep_path("/Users/owner/Library/Caches/com.apple.x/file", home, data));
    assert!(!keep_path("/Users/owner/Library/Containers/app/Data/Library/Caches/x", home, data));
    assert!(!keep_path("/Users/owner/.Trash/old.dwg", home, data));
    assert!(!keep_path("/Users/owner/Library/Application Support/Heimdall/archive/x", home, data), "Heimdall's own data is never a result");
    assert!(!keep_path("/Users/owner/code/project/.git/config", home, data));
    assert!(!keep_path("/Users/owner/code/project/node_modules/x.js", home, data));
    assert!(!keep_path("/Volumes/Other/old.dwg", home, data));
}

#[test]
fn the_archive_search_fails_cleanly_without_an_archive() {
    assert!(search_timeline(Path::new("/nonexistent/archive/index.sqlite"), "cad").is_err());
    assert!(search_timeline(Path::new("/nonexistent/archive/index.sqlite"), "!!!").is_err(), "no words is an error too");
}

#[test]
fn the_prompt_names_the_talk_the_people_and_the_time() {
    let w = window(&[line(NOW - 60_000, "Sam", "remember the CAD file", false), line(NOW, "User", "yes, that one", true)]);
    let p = prompt(&w, NOW, &["Sam".to_owned(), "Priya".to_owned()]);
    let text = p.text();
    assert!(text.starts_with("People Heimdall knows by name: Sam, Priya."), "{text}");
    assert!(text.contains("[") && text.contains("Sam] remember the CAD file"));
    assert!(text.contains("User] yes, that one"));
    assert!(text.contains(&format!("call {SUBMIT} once")));
    // The people line is stable (cached between passes); the talk is volatile.
    assert!(p.blocks[0].cache && !p.blocks.last().unwrap().cache);
    assert!(!prompt(&w, NOW, &[]).text().contains("People Heimdall knows"));
}

#[test]
fn the_tools_and_the_answer_schema_are_the_three_the_run_uses() {
    let names: Vec<&str> = tools().iter().map(|t| t.name).collect();
    assert_eq!(names, vec![SEARCH_FILES, SEARCH_TIMELINE, SUBMIT]);
    let schema = submit_schema();
    assert_eq!(schema["required"], json!(["suggest", "topic", "questions", "found", "handoff"]));
    assert_eq!(schema["properties"]["questions"]["maxItems"], 3);
}

#[test]
fn the_export_gives_v4_its_labels_from_the_log() {
    let log = [
        json!({"id": "a", "event": "shown", "at": 1, "topic": "CAD", "card": "c", "transcript": "t1"}),
        json!({"id": "a", "event": "picked", "at": 2, "choice": "Find the file"}),
        json!({"id": "b", "event": "shown", "at": 3, "topic": "photos", "card": "c", "transcript": "t2"}),
        json!({"id": "b", "event": "dismissed", "at": 4, "choice": "Not now"}),
        json!({"id": "c", "event": "shown", "at": 5, "topic": "x", "card": "c", "transcript": "t3"}),
        json!({"id": "c", "event": "ignored", "at": 6}),
        json!({"id": "d", "event": "shown", "at": 7, "topic": "y", "card": "c", "transcript": "t4"}),
        json!({"id": "d", "event": "feedback", "at": 8, "kind": "irrelevant", "text": "irrelevant"}),
        json!({"event": "silent", "at": 9, "reason": "quiet: nothing"}),
    ]
    .iter()
    .map(Value::to_string)
    .collect::<Vec<_>>()
    .join("\n");
    let rows = export(&log);
    assert_eq!(rows.len(), 4, "silent decisions are not cards");
    assert_eq!(rows[0]["suggest_now"], true);
    assert_eq!(rows[0]["choice"], "Find the file");
    assert_eq!(rows[1]["suggest_now"], false);
    assert!(rows[2]["suggest_now"].is_null(), "ignored carries no signal");
    assert_eq!(rows[3]["suggest_now"], false);
    assert_eq!(rows[3]["feedback"], "irrelevant");
    assert_eq!(rows[0]["transcript"], "t1");
}

#[test]
fn the_log_appends_one_line_per_event() {
    let dir = tempfile::tempdir().unwrap();
    let path = log_path(dir.path());
    append(&path, &json!({"id": "a", "event": "shown"})).unwrap();
    append(&path, &json!({"id": "a", "event": "picked"})).unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    assert_eq!(text.lines().count(), 2);
    assert_eq!(export(&text).len(), 1);
}

#[test]
fn the_pacer_survives_a_round_trip_through_json() {
    let mut p = Pacer::default();
    p.shown(NOW, "CAD file");
    let back: Pacer = serde_json::from_str(&serde_json::to_string(&p).unwrap()).unwrap();
    assert_eq!(back, p);
}

#[test]
fn the_card_line_and_choice_read_plainly() {
    let Verdict::Card(s) = verify(&card_answer(json!({})), &[]).unwrap() else { panic!() };
    assert_eq!(card_line(&s), "Should I look for it? [Find the file | Investigate it | Not now]");
    let answers = vec![ui::agent_notice::QuestionAnswer { question_id: "q1".into(), labels: vec!["Find the file".into()], other: Some("the Dropbox one".into()) }];
    assert_eq!(choice(&answers), "Find the file (typed: the Dropbox one)");
}

#[test]
fn the_public_suggester_prompt_carries_nothing_of_owners() {
    // The public edition ships this text: no names of his, no city, no languages, no people (docs/editions.md).
    let text = include_str!("../prompt/public/suggester.md");
    for word in ["User", "Poznań", "Polish", "Sam", "Priya", "Heimdall's owner"] {
        assert!(!text.contains(word), "the public prompt names {word}");
    }
    assert!(text.contains(SUBMIT) && text.contains("Not now"), "the public prompt still names the tool and the dismissal");
}

#[test]
fn a_reaching_verb_with_a_person_after_it_is_contact_and_a_noun_use_is_not() {
    for wording in ["Call Priya", "Text Sam about it", "Message them", "Email Priya the drawing", "Send it to Priya", "Ring them up"] {
        assert!(proposes_contact(wording), "{wording}");
    }
    for wording in ["Look up the messages from Priya", "Find the text file", "Call recording from Tuesday", "Send the CAD file", "The email from Priya", "Look up what Priya said"] {
        assert!(!proposes_contact(wording), "{wording}");
    }
}

#[test]
fn contact_in_the_topic_or_the_handoff_refuses_the_card_too() {
    let in_handoff = card_answer(json!({"handoff": "Message Sam the drawing once it is found."}));
    assert_eq!(verify(&in_handoff, &[]).unwrap(), Verdict::Silent("proposes contacting someone".into()));
    let in_topic = card_answer(json!({"topic": "Email Priya about the drawing"}));
    assert!(matches!(verify(&in_topic, &[]).unwrap(), Verdict::Silent(r) if r.contains("contacting")));
}

#[test]
fn a_card_offers_to_look_things_up_and_never_to_act() {
    assert!(acts_outward("Delete the old drafts"));
    assert!(acts_outward("Buy the drawings?"));
    assert!(!acts_outward("Find the drawings"));
    let answer = card_answer(json!({"questions": [{"text": "Trash the old backups?", "options": ["Yes", "Not now"], "multi_select": false}]}));
    assert!(matches!(verify(&answer, &[]).unwrap(), Verdict::Silent(r) if r.contains("outside a look-up")));
}

#[test]
fn every_question_carries_not_now_and_a_full_list_gives_way_to_it() {
    let bare = card_answer(json!({"questions": [{"text": "Which?", "options": ["CAD files", "Photos"], "multi_select": false}]}));
    let Verdict::Card(s) = verify(&bare, &[]).unwrap() else { panic!("a card") };
    assert_eq!(s.questions[0].options, vec!["CAD files", "Photos", "Not now"]);
    let full = card_answer(json!({"questions": [{"text": "Which?", "options": ["A1", "B2", "C3", "D4"], "multi_select": false}]}));
    let Verdict::Card(s) = verify(&full, &[]).unwrap() else { panic!("a card") };
    assert_eq!(s.questions[0].options, vec!["A1", "B2", "C3", "Not now"]);
}

#[test]
fn a_line_with_an_instruction_after_the_phrase_is_a_message_not_feedback() {
    assert_eq!(feedback("Not relevant, check Downloads instead"), None);
    assert_eq!(feedback("irrelevant, what about the photos?"), None);
    assert_eq!(feedback("that's irrelevant, thanks"), Some("irrelevant"));
    assert_eq!(feedback("that was a bad suggestion"), Some("bad suggestion"));
}

#[test]
fn a_long_line_is_cut_before_it_reaches_the_prompt() {
    let mut w = Window::default();
    w.push(line(NOW, "Sam", &"x".repeat(1000), false));
    assert_eq!(w.last().unwrap().text.chars().count(), LINE_CHARS);
}

#[test]
fn without_a_home_folder_there_is_no_file_search_at_all() {
    assert!(search_files("cad", Path::new(""), Path::new("/Users/owner/Library/Application Support/Heimdall")).is_err());
}
