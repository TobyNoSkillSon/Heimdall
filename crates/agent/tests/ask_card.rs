//! The adaptive `ask` card (24 Oct, lab/ask-card/CONTRACT.md §1–2): both call forms, their limits, and the answers that
//! come back to the agent.
use agent::{
    actions::{card_grants, ActionSink, Actions, AskAnswer, AskGrant, AskItem, AskOption, Clicked, Notice, Question, parse_ask},
    delivery::{DeliveryGate, DeliveryPermit},
    mcp_contract::{tools, validate_tool},
};
use anyhow::Result;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Sink {
    questions: Mutex<Vec<Question>>,
    delivered: Mutex<Vec<String>>,
}
impl ActionSink for Sink {
    fn notice(&self, _: &Notice, _: &DeliveryPermit) -> Result<()> {
        Ok(())
    }
    fn question(&self, q: &Question, p: &DeliveryPermit) -> Result<()> {
        p.with(|| {
            self.questions.lock().unwrap().push(q.clone());
            Ok(())
        })
    }
    fn question_closed(&self, _: &str, _: &str, _: &DeliveryPermit) -> Result<()> {
        Ok(())
    }
    fn deliver(&self, _id: &str, text: &str, p: &DeliveryPermit) -> Result<()> {
        p.with(|| {
            self.delivered.lock().unwrap().push(text.to_owned());
            Ok(())
        })
    }
    fn reload(&self, _: &str, _: &DeliveryPermit) -> Result<()> {
        Ok(())
    }
}

fn setup() -> (tempfile::TempDir, Arc<DeliveryGate>, Actions, Arc<Sink>) {
    let dir = tempfile::tempdir().unwrap();
    let gate = Arc::new(DeliveryGate::new(true));
    let a = Actions::open(&dir.path().join("receipts.db")).unwrap();
    let sink = Arc::new(Sink::default());
    a.attach(sink.clone());
    (dir, gate, a, sink)
}

fn answer(question_id: &str, labels: &[&str], other: Option<&str>) -> AskAnswer {
    AskAnswer {
        question_id: question_id.into(),
        labels: labels.iter().map(|l| (*l).to_owned()).collect(),
        other: other.map(str::to_owned),
    }
}

/// Stale branches (multi-select, with descriptions) and a yes-or-no push, in one card.
fn two_questions() -> Value {
    json!({"questions": [
        {
            "header": "Branches",
            "question": "Which stale branches should the worker delete?",
            "multiSelect": true,
            "options": [
                {"label": "resampler-old", "description": "merged in September"},
                {"label": "resampler-v2", "description": "merged in October"},
                "meter-fix",
                "stash-2024"
            ]
        },
        {"header": "Push", "question": "Push the fix to main?", "options": ["Yes", "No"]}
    ]})
}

#[test]
fn both_call_forms_pass_the_schema_and_the_handler() {
    let legacy = json!({"question": "Install the new build?", "options": ["Yes", "No"]});
    validate_tool("ask", &legacy).unwrap();
    validate_tool("ask", &two_questions()).unwrap();
    let items = parse_ask(&two_questions()).unwrap();
    assert_eq!(items.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(), ["q1", "q2"]);
    assert!(items[0].multi_select && items[0].allow_other, "multiSelect as asked, Other on by default");
    assert_eq!(items[0].header.as_deref(), Some("Branches"));
    assert_eq!(items[0].options[0], AskOption { label: "resampler-old".into(), description: Some("merged in September".into()), grant: None });
    assert_eq!(items[0].options[2], AskOption { label: "meter-fix".into(), description: None, grant: None });
    assert!(!items[1].multi_select && items[1].allow_other, "a question with no multiSelect is single select");
    assert_eq!(items[1].header.as_deref(), Some("Push"));
}

#[test]
fn the_legacy_call_is_one_question_with_the_same_card() {
    let items = parse_ask(&json!({"question": "Install the new build?", "options": ["Yes", "No"]})).unwrap();
    assert_eq!(items, vec![AskItem::one("Install the new build?", &["Yes", "No"])]);
    assert!(items[0].allow_other && !items[0].multi_select && items[0].header.is_none());
}

#[test]
fn the_legacy_answer_is_delivered_byte_for_byte_as_before() {
    let (_dir, gate, a, sink) = setup();
    let p = gate.permit().unwrap();
    let q = a.ask("legacy", &[AskItem::one("Install the new build?", &["Yes", "No"])], &p).unwrap();
    assert_eq!(a.answer(&q, "No", &p).unwrap(), Clicked::Delivered);
    assert_eq!(*sink.delivered.lock().unwrap(), vec![format!("Question {q}: User clicked No. Question: Install the new build?")]);
    // The same answer through the card's own call is the same line.
    let again = a.ask("legacy-2", &[AskItem::one("Ship it?", &["Yes", "No"])], &p).unwrap();
    assert_eq!(a.answer_card(&again, &[answer("q1", &["Yes"], None)], &p).unwrap(), Clicked::Delivered);
    assert_eq!(sink.delivered.lock().unwrap()[1], format!("Question {again}: User clicked Yes. Question: Ship it?"));
}

#[test]
fn a_receipt_written_before_the_card_still_answers_in_its_old_form() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("receipts.db");
    let gate = Arc::new(DeliveryGate::new(true));
    let p = gate.permit().unwrap();
    let sink = Arc::new(Sink::default());
    let a = Actions::open(&path).unwrap();
    a.attach(sink.clone());
    let q = a.ask("old", &[AskItem::one("Ship?", &["Yes", "No"])], &p).unwrap();
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute("UPDATE receipts SET body=? WHERE id=?", rusqlite::params![r#"{"options":["Yes","No"],"question":"Ship?"}"#, q])
        .unwrap();
    assert_eq!(a.answer(&q, "No", &p).unwrap(), Clicked::Delivered);
    assert_eq!(*sink.delivered.lock().unwrap(), vec![format!("Question {q}: User clicked No. Question: Ship?")]);
}

#[test]
fn the_limits_reject_what_the_card_cannot_show() {
    let long = |n: usize| "x".repeat(n);
    let cases = [
        ("both forms together", json!({"question": "Q?", "options": ["Yes", "No"], "questions": [{"question": "Q?", "options": ["Yes", "No"]}]})),
        ("five questions", json!({"questions": (0..5).map(|_| json!({"question": "Q?", "options": ["Yes", "No"]})).collect::<Vec<_>>()})),
        ("no questions", json!({"questions": []})),
        ("one option", json!({"questions": [{"question": "Q?", "options": ["Yes"]}]})),
        ("five options", json!({"questions": [{"question": "Q?", "options": ["A", "B", "C", "D", "E"]}]})),
        ("duplicate labels", json!({"questions": [{"question": "Q?", "options": ["Yes", "yes"]}]})),
        ("long header", json!({"questions": [{"header": "Stale branches", "question": "Q?", "options": ["Yes", "No"]}]})),
        ("description over 80", json!({"questions": [{"question": "Q?", "options": [{"label": "Yes", "description": long(81)}, "No"]}]})),
        ("label over 40", json!({"questions": [{"question": "Q?", "options": [long(41), "No"]}]})),
        ("question over 120", json!({"questions": [{"question": long(121), "options": ["Yes", "No"]}]})),
        ("blank question", json!({"questions": [{"question": "   ", "options": ["Yes", "No"]}]})),
        ("legacy option over 24", json!({"question": "Install?", "options": ["Yes", long(25)]})),
        ("legacy duplicate labels", json!({"question": "Install?", "options": ["Yes", "YES"]})),
        ("neither form", json!({})),
    ];
    for (why, args) in cases {
        assert!(validate_tool("ask", &args).is_err(), "{why} must be refused by the schema or the handler: {args}");
        assert!(parse_ask(&args).is_err(), "{why} must be refused by the handler: {args}");
    }
    // An unknown field is refused by the schema alone (the handler reads only the fields it knows).
    assert!(validate_tool("ask", &json!({"questions": [{"question": "Q?", "options": ["Yes", "No"], "colour": "red"}]})).is_err());
    // The limits themselves are exact: 12, 40, 80 and 120 characters pass.
    let edge = json!({"questions": [{"header": long(12), "question": long(120), "options": [{"label": long(40), "description": long(80)}, "No"]}]});
    validate_tool("ask", &edge).unwrap();
}

#[test]
fn a_multi_select_answer_comes_back_keyed_and_in_card_order() {
    let (_dir, gate, a, sink) = setup();
    let p = gate.permit().unwrap();
    let q = a.ask("multi", &parse_ask(&two_questions()).unwrap(), &p).unwrap();
    assert_eq!(sink.questions.lock().unwrap()[0].questions.len(), 2, "the card carries both questions");
    // Answers given out of order still read in card order.
    let answers = [answer("q2", &["No"], None), answer("q1", &["resampler-old", "meter-fix"], None)];
    assert_eq!(a.answer_card(&q, &answers, &p).unwrap(), Clicked::Delivered);
    assert_eq!(
        *sink.delivered.lock().unwrap(),
        vec![format!(
            "Question {q}: User answered q1 \"Which stale branches should the worker delete?\": resampler-old, meter-fix\n\
             Question {q}: User answered q2 \"Push the fix to main?\": No"
        )]
    );
    assert_eq!(a.ask_answer(&q).unwrap().unwrap().1, "q1: resampler-old, meter-fix\nq2: No");
    assert_eq!(a.state(&q).unwrap(), "answered");
}

#[test]
fn other_is_typed_words_and_only_where_the_question_allows_it() {
    let (_dir, gate, a, sink) = setup();
    let p = gate.permit().unwrap();
    let items = parse_ask(&json!({"questions": [
        {"question": "Which stash?", "multiSelect": true, "options": ["stash-2024", "stash-2023"]},
        {"question": "Keep it?", "allowOther": false, "options": ["Yes", "No"]}
    ]}))
    .unwrap();
    let q = a.ask("other", &items, &p).unwrap();
    // Multi-select: picks and Other together; an Other-only single answer is one answer.
    let answers = [answer("q1", &["stash-2024"], Some("and the one from March")), answer("q2", &["No"], None)];
    assert_eq!(a.answer_card(&q, &answers, &p).unwrap(), Clicked::Delivered);
    assert!(sink.delivered.lock().unwrap()[0].contains(r#"q1 "Which stash?": stash-2024; Other: "and the one from March""#));

    let refused = a.ask("other-no", &items, &p).unwrap();
    let err = a.answer_card(&refused, &[answer("q1", &["stash-2023"], None), answer("q2", &[], Some("maybe"))], &p).unwrap_err();
    assert!(err.to_string().contains("q2 has no Other"), "{err}");
    assert_eq!(a.state(&refused).unwrap(), "shown", "a refused answer leaves the card open");
    let blank = a.answer_card(&refused, &[answer("q1", &[], Some("   ")), answer("q2", &["Yes"], None)], &p).unwrap_err();
    assert!(blank.to_string().contains("Other is 1 to 500 characters"), "{blank}");
}

#[test]
fn an_answer_with_nothing_answered_or_naming_an_option_off_the_card_is_refused() {
    let (_dir, gate, a, _sink) = setup();
    let p = gate.permit().unwrap();
    let q = a.ask("refuse", &parse_ask(&two_questions()).unwrap(), &p).unwrap();
    let nothing = a.answer_card(&q, &[answer("q1", &[], None), answer("q2", &[], None)], &p).unwrap_err();
    assert!(nothing.to_string().contains("ASK_INCOMPLETE"), "{nothing}");
    assert!(a.answer_card(&q, &[], &p).unwrap_err().to_string().contains("ASK_INCOMPLETE"));
    let off_card = a.answer_card(&q, &[answer("q1", &["resampler-v3"], None), answer("q2", &["Yes"], None)], &p).unwrap_err();
    assert!(off_card.to_string().contains("ASK_UNKNOWN_CHOICE"), "{off_card}");
    let stray = a.answer_card(&q, &[answer("q9", &["Yes"], None), answer("q2", &["Yes"], None)], &p).unwrap_err();
    assert!(stray.to_string().contains("ASK_UNKNOWN_QUESTION"), "{stray}");
    let twice = a.answer_card(&q, &[answer("q2", &["Yes"], None), answer("q2", &["No"], None)], &p).unwrap_err();
    assert!(twice.to_string().contains("one answer per question"), "{twice}");
    // q1 is multi-select (any number, at least one); q2 is single select (exactly one).
    let single_picked_twice = a.answer_card(&q, &[answer("q1", &["meter-fix"], None), answer("q2", &["Yes", "No"], None)], &p).unwrap_err();
    assert!(single_picked_twice.to_string().contains("q2 takes one answer"), "{single_picked_twice}");
    // Still open: the whole answer lands once it is complete and right.
    assert_eq!(a.state(&q).unwrap(), "shown");
    assert_eq!(a.answer_card(&q, &[answer("q1", &["meter-fix"], None), answer("q2", &["Yes"], None)], &p).unwrap(), Clicked::Delivered);
}

/// Bug note 2026-11-02T16-27-21: the user ticked one question of two and could not send. Send now goes with what he
/// answered; the agent hears which question he left unanswered, by id, whether its answer is empty or absent.
#[test]
fn a_card_sent_with_some_questions_answered_tells_the_agent_which_were_left() {
    let (_dir, gate, a, sink) = setup();
    let p = gate.permit().unwrap();
    let q = a.ask("partial", &parse_ask(&two_questions()).unwrap(), &p).unwrap();
    assert_eq!(a.answer_card(&q, &[answer("q1", &["meter-fix"], None), answer("q2", &[], None)], &p).unwrap(), Clicked::Delivered);
    let delivered = sink.delivered.lock().unwrap()[0].clone();
    assert!(delivered.contains(r#"q1 "Which stale branches should the worker delete?": meter-fix"#), "{delivered}");
    assert!(delivered.contains(&format!("Question {q}: User left q2 ")) && delivered.ends_with("unanswered"), "{delivered}");
    assert_eq!(a.ask_answer(&q).unwrap().unwrap().1, "q1: meter-fix\nq2: (unanswered)");
    assert_eq!(a.state(&q).unwrap(), "answered");

    let absent = a.ask("partial-absent", &parse_ask(&two_questions()).unwrap(), &p).unwrap();
    assert_eq!(a.answer_card(&absent, &[answer("q2", &["Yes"], None)], &p).unwrap(), Clicked::Delivered);
    let delivered = sink.delivered.lock().unwrap()[1].clone();
    assert!(delivered.contains(&format!("Question {absent}: User left q1 ")) && delivered.contains(r#"q2 ""#), "{delivered}");
}

#[test]
fn a_spoken_close_still_ends_a_multi_question_card_and_a_later_send_does_nothing() {
    let (_dir, gate, a, _sink) = setup();
    let p = gate.permit().unwrap();
    let q = a.ask("voice", &parse_ask(&two_questions()).unwrap(), &p).unwrap();
    assert_eq!(a.close(&q, "not that one, leave them", &p).unwrap(), agent::actions::Closed::Closed);
    assert_eq!(a.answer_card(&q, &[answer("q1", &["meter-fix"], None), answer("q2", &["Yes"], None)], &p).unwrap(), Clicked::ClosedByVoice);
    assert_eq!(a.ask_answer(&q).unwrap().unwrap().1, "not that one, leave them");
    assert_eq!(a.ask_click(&q).unwrap(), None);
}

#[test]
fn a_single_choice_still_stores_its_bare_label_for_t3_confirmations() {
    let (_dir, gate, a, _sink) = setup();
    let p = gate.permit().unwrap();
    let q = a.ask("confirm", &[AskItem::one("Delete project 'Vella'?", &["Yes", "No"])], &p).unwrap();
    a.answer_card(&q, &[answer("q1", &["Yes"], None)], &p).unwrap();
    assert_eq!(a.ask_click(&q).unwrap(), Some(("Delete project 'Vella'?".to_owned(), "Yes".to_owned())));
}

#[test]
fn the_ask_tool_describes_the_one_adaptive_card() {
    let list = tools();
    let ask = list.as_array().unwrap().iter().find(|t| t["name"] == "ask").unwrap();
    let description = ask["description"].as_str().unwrap();
    for part in ["questions", "multiSelect", "allowOther", "numbered", "Option and a digit", "keyed by its id", "question with options still works"] {
        assert!(description.contains(part), "{part} missing from {description}");
    }
    assert_eq!(ask["inputSchema"]["additionalProperties"], false);
}

/// Bug notes 2026-11-03T23-04-19 and 2026-11-03T18-04-11: an option can carry a grant for the deny-list hook, naming
/// exact paths or login items, which the island draws in full under the option (4 Nov evening); only his own pointer's
/// pick of that option grants it, and such a card takes only a click.
#[test]
fn an_option_grants_only_what_the_card_names() {
    let card = |grant: Value, description: &str| json!({"questions": [{"question": "Delete the old T3 Code copies (1.6 GB)?", "options": [
        {"label": "Delete nightly", "description": description, "grant": grant}, "Keep them"]}]});
    let ok = card(json!({"delete": ["~/Backups/T3 Code (Nightly).app", "~/.t3-backup-before-nightly-20261005"]}), "~/Backups/T3 Code (Nightly).app, ~/.t3-backup-before-nightly-20261005");
    // The schema offers a grant in the personal edition (grants on), not the public one; the handler checks it either way.
    assert_eq!(heimdall_emergency::grants::ENABLED, !cfg!(feature = "public"));
    assert_eq!(validate_tool("ask", &ok).is_ok(), heimdall_emergency::grants::ENABLED);
    // The card draws each grant itself, so the text need not repeat a path (it used to have to, when the island cut it).
    let long = "~/Library/Application Support/t3code-nightly/sessions/2026-10-05 before the nightly migration/state.sqlite";
    let drawn = parse_ask(&card(json!({"delete": [long]}), "Frees 1.6 GB")).unwrap();
    assert_eq!(drawn[0].options[0].grant.as_ref().unwrap().delete, vec![long.to_string()]);
    // Review 4 Nov evening: a question's grants must fit the window (8 lines of 56 characters), else the card could
    // hide part of what a click approves.
    let eight: Vec<String> = (0..8).map(|k| format!("~/Backups/old-copy-{k}.app")).collect();
    assert!(parse_ask(&card(json!({"delete": eight}), "x")).is_ok());
    let tall: Vec<String> = (0..5).map(|k| format!("~/Backups/{}-{k}", "a".repeat(60))).collect();
    let e = parse_ask(&card(json!({"delete": tall}), "x")).unwrap_err().to_string();
    assert!(e.starts_with("ASK_BAD_GRANT") && e.contains("at most 8 lines"), "{e}");
    let items = parse_ask(&ok).unwrap();
    assert_eq!(items[0].options[0].grant.as_ref().unwrap().delete.len(), 2);
    assert_eq!(items[0].options[1].grant, None);
    for (grant, description, why) in [
        (json!({"delete": ["~/Documents"]}), "x", "a home main folder"),
        (json!({"delete": ["~/Backups/x", "~/Backups/x"]}), "x", "the same path twice"),
        (json!({"delete": ["~/Backups/*"]}), "Backups/*", "a wildcard"),
        (json!({"delete": ["~/Backups/../Documents"]}), "x", "a dot-dot"),
        (json!({"delete": ["~/$OLD"]}), "x", "a variable"),
        (json!({"loginItems": ["x\" & delete every login item & \""]}), "x", "a quote in a login item"),
        (json!({"delete": ["~/Library"]}), "Library", "too broad"),
        (json!({"delete": ["Backups/x"]}), "x", "relative"),
        (json!({}), "x", "empty"),
    ] {
        let e = parse_ask(&card(grant.clone(), description)).unwrap_err().to_string();
        assert!(e.starts_with("ASK_BAD_GRANT"), "{why}: {grant} -> {e}");
    }
    let login = parse_ask(&card(json!({"loginItems": ["swiftpm-testing-helper"]}), "removes swiftpm-testing-helper")).unwrap();
    assert_eq!(login[0].options[0].grant.as_ref().unwrap().login_items, vec!["swiftpm-testing-helper".to_string()]);

    let picked = [answer("q1", &["Delete nightly"], None)];
    let grants = card_grants(&items, &picked, &picked);
    assert_eq!(grants.len(), 1);
    assert_eq!((grants[0].0.as_str(), grants[0].1.as_str()), ("q1", "Delete nightly"));
    assert!(card_grants(&items, &[answer("q1", &["Keep them"], None)], &[answer("q1", &["Keep them"], None)]).is_empty());
    assert!(card_grants(&items, &[answer("q1", &[], Some("Delete nightly"))], &picked).is_empty(), "typed Other never grants");
    // Only his own pointer's pick (the island's `by_hand`): a key, his voice, an accessibility press or a posted event
    // answers the card but grants nothing; a hand pick not in the answer grants nothing either.
    assert!(card_grants(&items, &picked, &[]).is_empty(), "picked, but not by hand");
    assert!(card_grants(&items, &picked, &[answer("q2", &["Delete nightly"], None)]).is_empty(), "by hand on another question");
    assert!(card_grants(&items, &[answer("q1", &["Keep them"], None)], &picked).is_empty(), "by hand but not the answer");
    let _: AskGrant = grants[0].2.clone();

    // Only a click answers it: his voice leaves the card open, also after a restart (review 4 Nov).
    let (dir, gate, a, _sink) = setup();
    let p = gate.permit().unwrap();
    let id = a.ask("grant-card", &items, &p).unwrap();
    assert!(a.needs_click(&id));
    assert!(Actions::open(&dir.path().join("receipts.db")).unwrap().needs_click(&id), "decided by the card, not by memory");
    let plain = a.ask("plain-card", &parse_ask(&two_questions()).unwrap(), &p).unwrap();
    assert!(!a.needs_click(&plain));
}

/// Bug notes 2026-11-04T18-59-48 and 2026-11-04T23-37-22: the relay put the path in the question of a short card,
/// which grants nothing. The schema's `grant` says how to use it (personal edition), and the short form refuses a
/// grant with where it goes.
#[test]
fn the_grant_field_says_how_to_use_it_and_the_short_form_points_to_it() {
    let list = tools();
    let ask = list.as_array().unwrap().iter().find(|t| t["name"] == "ask").unwrap();
    let grant = &ask["inputSchema"]["properties"]["questions"]["items"]["properties"]["options"]["items"]["oneOf"][1]["properties"]["grant"];
    if heimdall_emergency::grants::ENABLED {
        let d = grant["description"].as_str().expect("grant describes its use");
        for part in ["only User's own click", "questions form", "plain rm of its absolute path", "30 minutes", "grants nothing", "delete login item"] {
            assert!(d.contains(part), "{part} missing from {d}");
        }
    } else {
        assert!(grant.is_null(), "no grant in the public edition");
    }
    let short = json!({"question": "Delete /Users/t/Applications/Arsenal.app.previous?", "options": [{"label": "Delete it", "grant": {"delete": ["/Users/t/Applications/Arsenal.app.previous"]}}, "Keep"]});
    let e = parse_ask(&short).unwrap_err().to_string();
    assert!(e.starts_with("ASK_BAD_GRANT") && e.contains("questions"), "{e}");
    let e = validate_tool("ask", &short).unwrap_err().to_string();
    assert!(e.starts_with("ASK_BAD_GRANT"), "the server says where a grant goes: {e}");
    let full = json!({"questions": [{"question": "Delete the old Arsenal copy (56 MB)?", "options": [{"label": "Delete it", "grant": {"delete": ["~/Applications/Arsenal.app.previous"]}}, "Keep"]}]});
    assert_eq!(parse_ask(&full).unwrap()[0].options[0].grant.as_ref().unwrap().delete, vec!["~/Applications/Arsenal.app.previous".to_string()]);
    assert_eq!(validate_tool("ask", &full).is_ok(), heimdall_emergency::grants::ENABLED);
}

/// Bug note 2026-11-04T23-01-22: an option can grant one new keychain file for signing (the hook's
/// `grants::grant_keychain`). Exactly one path, as text; never the login keychain, a file that exists, a relative
/// path, a hidden character or one too long to draw. It is drawn with the grant's other lines, so it counts toward them.
#[test]
fn an_option_grants_one_new_keychain_file_only() {
    let card = |grant: Value| json!({"questions": [{"question": "Set up the release signing keychain?", "options": [
        {"label": "Set it up", "grant": grant}, "Not now"]}]});
    let fresh = format!("~/Library/Keychains/heimdall-ask-test-{}.keychain-db", std::process::id());
    let ok = card(json!({"keychain": fresh}));
    assert_eq!(validate_tool("ask", &ok).is_ok(), heimdall_emergency::grants::ENABLED, "the schema offers it with the grant");
    let items = parse_ask(&ok).unwrap();
    assert_eq!(items[0].options[0].grant, Some(AskGrant { keychain: Some(fresh.clone()), ..Default::default() }));
    let picked = [answer("q1", &["Set it up"], None)];
    assert_eq!(card_grants(&items, &picked, &picked)[0].2.keychain.as_deref(), Some(fresh.as_str()), "the writer gets it");
    assert!(card_grants(&items, &picked, &[]).is_empty(), "never without his own click");
    // Beside deletes on the same option.
    let both = parse_ask(&card(json!({"keychain": fresh, "delete": ["~/Backups/old-signing"]}))).unwrap();
    assert_eq!(both[0].options[0].grant.as_ref().unwrap().delete, vec!["~/Backups/old-signing".to_string()]);

    // A file that exists, inside a project of Heimdall's home when the test's scratch folder is one (the worktree's target).
    let scratch = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("ask-keychain-{}", std::process::id()));
    std::fs::create_dir_all(&scratch).unwrap();
    let scratch = std::fs::canonicalize(&scratch).unwrap();
    let existing = scratch.join("existing.keychain-db");
    std::fs::write(&existing, b"x").unwrap();
    let in_project = scratch.strip_prefix(heimdall_home::home_dir().join("Projects")).is_ok();
    let e = parse_ask(&card(json!({"keychain": existing.display().to_string()}))).unwrap_err().to_string();
    assert!(e.starts_with("ASK_BAD_GRANT"), "{e}");
    if in_project {
        assert!(e.contains("already exists"), "{e}");
        assert!(parse_ask(&card(json!({"keychain": scratch.join("new.keychain-db").display().to_string()}))).is_ok(), "a new one in a project");
    }
    let _ = std::fs::remove_dir_all(&scratch);

    let long = format!("~/Library/Keychains/{}.keychain-db", "a".repeat(1100));
    let tall = format!("~/Library/Keychains/{}.keychain-db", "a".repeat(460));
    for (grant, why) in [
        (json!({"keychain": "~/Library/Keychains/login.keychain-db"}), "the login keychain"),
        (json!({"keychain": "/Users/t/Library/Keychains/LOGIN.keychain-db"}), "the login keychain in other case"),
        (json!({"keychain": "~/Library/Keychains/System.keychain-db"}), "macOS's own"),
        (json!({"keychain": "Library/Keychains/x.keychain-db"}), "relative"),
        (json!({"keychain": "x.keychain-db"}), "a bare name"),
        (json!({"keychain": "~/Library/Keychains/x\u{200B}.keychain-db"}), "a zero-width space"),
        (json!({"keychain": "~/Library/Keychains/x\u{202E}bd-niahcyek.keychain-db"}), "a direction mark"),
        (json!({"keychain": "~/Library/Keychains/x\t.keychain-db"}), "a tab"),
        (json!({"keychain": long}), "too long for the hook"),
        (json!({"keychain": tall}), "too long to draw in 8 lines"),
        (json!({"keychain": ["~/Library/Keychains/a.keychain-db"]}), "a list"),
        (json!({"keychain": ""}), "empty"),
        (json!({"keychain": "~/Library/Keychains/$NAME.keychain-db"}), "a variable"),
        (json!({"keychain": "~/Library/Keychains/../x.keychain-db"}), "a dot-dot"),
        (json!({"keychain": "~/Library/Keychains/sub/x.keychain-db"}), "a folder below"),
        (json!({"keychain": "~/Library/Keychains/x.keychain"}), "not a .keychain-db"),
        (json!({"keychain": "/tmp/x.keychain-db"}), "outside the Keychains and the Vault's projects"),
        (json!({"keychains": "~/Library/Keychains/x.keychain-db"}), "an unknown key"),
    ] {
        let e = parse_ask(&card(grant.clone())).unwrap_err().to_string();
        assert!(e.starts_with("ASK_BAD_GRANT"), "{why}: {grant} -> {e}");
    }
}
