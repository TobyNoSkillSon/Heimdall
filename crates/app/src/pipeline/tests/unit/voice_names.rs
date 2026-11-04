    //! Quiet speaker naming: the voice card's answers and the renames it shares with the transcript,
    //! the Speakers window and the agent's `name_voice`.
    use super::*;
    use crate::voice_namer as namer;
    use crate::voice_suggest as sg;
    fn voice() -> speakers::embedding::Embedding {
        speakers::embedding::Embedding::new(vec![1.; speakers::embedding::DIMENSION]).unwrap()
    }
    fn coded(a: &Pipeline) -> String {
        let path = a.data.join("speakers.json");
        let mut store = EnrollmentStore::default();
        let code = store.code_voice(vec![voice(), voice()], "Discord", "yo", 1_792_600_000_000, "Fri 23 Oct 21h", "2026-10-23").unwrap();
        store.save(&path).unwrap();
        code
    }
    /// One archived line of `speaker`, in this session or (`other`) an earlier one.
    fn archived(a: &Pipeline, id: u64, speaker: &str, other: bool) {
        let o = agent::protocol::Observation {
            id: format!("{}:{id}", a.session.file_name().unwrap().to_string_lossy()),
            timestamp_ms: 1,
            source: "Discord".into(),
            speaker: speaker.into(),
            language: "en".into(),
            text: "hello there".into(),
        };
        a.index.record(&o, "discord", id, id + 1, &[], &[], &serde_json::json!({})).unwrap();
        if other {
            let c = heimdall_archive::store::open(&crate::archive::store_root(&a.session)).unwrap();
            c.execute("UPDATE utterances SET session=session-1 WHERE id LIKE ?1", [format!("%:{id}")]).unwrap();
        }
    }
    fn speakers_in_archive(a: &Pipeline) -> Vec<String> {
        let c = heimdall_archive::store::open(&crate::archive::store_root(&a.session)).unwrap();
        let mut q = c.prepare("SELECT speaker FROM utterances ORDER BY session, id").unwrap();
        q.query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect()
    }
    fn proposal(code: &str) -> namer::Proposal {
        namer::Proposal { code: code.into(), name: "John".into(), quote: "User: hey John, you there?".into(), hints: vec![1, 4, 9] }
    }
    fn notices(a: &Pipeline) -> Vec<ui::agent_notice::AgentNotice> {
        a.hooks.ui.lock().unwrap().iter().filter_map(|u| match u { ui::UiUpdate::AgentNotice(n) => Some(n.clone()), _ => None }).collect()
    }
    fn answer(a: &mut Pipeline, code: &str, choice: &str) -> Result<()> {
        answer_card(a, &format!("{}{code}", namer::CARD_PREFIX), choice)
    }
    fn answer_card(a: &mut Pipeline, request_id: &str, choice: &str) -> Result<()> {
        let answers = vec![ui::agent_notice::QuestionAnswer { question_id: "q1".into(), labels: vec![choice.into()], other: None }];
        a.action(ui::UiAction::AgentNotice(ui::agent_notice::AgentNoticeAction::Answer { request_id: request_id.into(), answers, by_hand: vec![] }))
    }
    /// Return in the card's name field with `typed` (the island sends it as the question's Other text, trimmed).
    fn type_name(a: &mut Pipeline, code: &str, typed: &str) -> Result<()> {
        let answers = vec![ui::agent_notice::QuestionAnswer { question_id: "q1".into(), labels: vec![], other: Some(typed.trim().into()) }];
        a.action(ui::UiAction::AgentNotice(ui::agent_notice::AgentNoticeAction::Answer { request_id: format!("{}{code}", namer::CARD_PREFIX), answers, by_hand: vec![] }))
    }
    /// The card's name field: its placeholder, whether it takes the keyboard, and how many answers follow it.
    fn field(n: &ui::agent_notice::AgentNotice) -> Option<(String, bool, usize)> {
        match &n.action {
            Some(ui::agent_notice::NoticeAction::Ask { questions, .. }) if questions[0].allow_other => questions[0].field.as_ref().map(|f| (f.placeholder.clone(), f.focus_until_ms > agent::now_ms(), f.after)),
            _ => None,
        }
    }
    fn options(n: &ui::agent_notice::AgentNotice) -> Vec<String> {
        match &n.action {
            Some(ui::agent_notice::NoticeAction::Ask { questions, .. }) => questions[0].options.iter().map(|o| o.label.clone()).collect(),
            _ => vec![],
        }
    }
    /// The new-voice card for `code` with these guesses, as a pass brings it.
    fn new_card(a: &mut Pipeline, code: &str, names: &[&str]) {
        let candidates = names.iter().map(|n| sg::Candidate { name: (*n).into(), sure: false }).collect();
        a.show_new_voice(sg::Suggestion { code: code.into(), quote: "I need to visit the barber.".into(), candidates, error: None }).unwrap();
    }
    fn new_answer(a: &mut Pipeline, code: &str, choice: &str) -> Result<()> {
        answer_card(a, &format!("{}{code}", sg::CARD_PREFIX), choice)
    }
    fn type_new(a: &mut Pipeline, code: &str, typed: &str) -> Result<()> {
        let answers = vec![ui::agent_notice::QuestionAnswer { question_id: "q1".into(), labels: vec![], other: Some(typed.trim().into()) }];
        a.action(ui::UiAction::AgentNotice(ui::agent_notice::AgentNoticeAction::Answer { request_id: format!("{}{code}", sg::CARD_PREFIX), answers, by_hand: vec![] }))
    }
    fn state(a: &Pipeline) -> namer::State {
        namer::State::load(&a.data.join("voice-namer.json"))
    }
    /// (voice card v3): one card per new voice. The quote, the guessed names as one-click chips, the
    /// name field, Not important last. No Figure it out, Later, Ask me after, Only if sure, and no second card.
    #[test]
    fn a_new_voice_gets_one_card_quote_names_field_not_important() {
        let (_t, mut a) = app();
        let code = coded(&a);
        archived(&a, 80, &code, false);
        new_card(&mut a, &code, &["Mari", "Ellie"]);
        let shown = notices(&a);
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].text, "New voice: \u{201c}I need to visit the barber.\u{201d}");
        assert_eq!(options(&shown[0]), ["Mari", "Ellie", "Not important"]);
        assert_eq!(field(&shown[0]), Some(("Type the name".into(), false, 1)), "the field before Not important; it waits for his click");
        assert!(shown[0].id.starts_with(sg::CARD_PREFIX));
        let st = state(&a);
        assert_eq!(st.shown.len(), 1, "it counts against the rate limit");
        assert!(st.voices[&code].snoozed_until > agent::now_ms() + sg::SNOOZE_MS - 60_000, "left unanswered, the voice rests");
        // One click names the voice and opens nothing else.
        let before = a.hooks.ui.lock().unwrap().len();
        new_answer(&mut a, &code, "Ellie").unwrap();
        assert_eq!(speakers_in_archive(&a), vec!["Ellie"]);
        let after: Vec<_> = a.hooks.ui.lock().unwrap()[before..].to_vec();
        assert!(!after.iter().any(|u| matches!(u, ui::UiUpdate::AgentNotice(_) | ui::UiUpdate::VoiceNameEntry { .. })), "{after:?}");
        assert!(new_answer(&mut a, &code, "Ellie").is_err(), "answered once");
    }
    #[test]
    fn the_old_answers_are_gone_and_not_important_forgets() {
        let (_t, mut a) = app();
        let code = coded(&a);
        for old in ["Figure it out", "Later", "Ask me after", "Only if sure"] {
            new_card(&mut a, &code, &[]);
            assert!(new_answer(&mut a, &code, old).is_err(), "{old}");
        }
        assert!(!EnrollmentStore::load(&a.data.join("speakers.json")).unwrap().voices[&code].irrelevant);
        new_card(&mut a, &code, &[]);
        assert_eq!(options(notices(&a).last().unwrap()), ["Not important"], "no real guess: no chip");
        new_answer(&mut a, &code, "Not important").unwrap();
        assert!(EnrollmentStore::load(&a.data.join("speakers.json")).unwrap().voices[&code].irrelevant, "keeps its code, never asked again");
    }
    #[test]
    fn a_typed_name_saves_exactly_as_typed() {
        let (_t, mut a) = app();
        let code = coded(&a);
        archived(&a, 81, &code, false);
        new_card(&mut a, &code, &["Mari"]);
        type_new(&mut a, &code, "Marii-Anne O'Neil").unwrap();
        assert_eq!(speakers_in_archive(&a), vec!["Marii-Anne O'Neil"]);
        assert!(EnrollmentStore::load(&a.data.join("speakers.json")).unwrap().names.contains_key("Marii-Anne O'Neil"));
        let code = coded(&a);
        archived(&a, 82, &code, false);
        new_card(&mut a, &code, &[]);
        type_new(&mut a, &code, "dave").unwrap();
        assert_eq!(speakers_in_archive(&a)[1], "dave", "his spelling, not a corrected one");
    }
    /// A state file from before voice card v3 (Ask me after, Only if sure) still loads: the old choice is dropped and
    /// the voice is open for a card again; its snooze and turned-down names are kept.
    #[test]
    fn an_older_state_file_loads_without_the_old_choices() {
        let (_t, a) = app();
        let path = a.data.join("voice-namer.json");
        std::fs::write(&path, r#"{"voices":{"Speaker Tue 3 Nov 17h #1":{"declined":["Erin"],"suggested_ms":5,"snoozed_until":9,"mode":"After","figured_ms":7}},"shown":[5]}"#).unwrap();
        let st = state(&a);
        let v = &st.voices["Speaker Tue 3 Nov 17h #1"];
        assert_eq!((v.declined.clone(), v.suggested_ms, v.snoozed_until, st.shown.clone()), (vec!["Erin".to_string()], 5, 9, vec![5]));
    }
    #[test]
    fn esc_in_the_name_field_lets_the_next_card_come() {
        let (_t, mut a) = app();
        let code = coded(&a);
        new_card(&mut a, &code, &["Mari"]);
        let id = notices(&a).last().unwrap().id.clone();
        // Typing is a pick the island keeps: nothing for the app, never an error.
        let pick = ui::agent_notice::QuestionAnswer { question_id: "q1".into(), labels: vec![], other: Some("Ma".into()) };
        a.action(ui::UiAction::AgentNotice(ui::agent_notice::AgentNoticeAction::Pick { request_id: id.clone(), answer: pick })).unwrap();
        a.action(ui::UiAction::AgentNotice(ui::agent_notice::AgentNoticeAction::Dismiss { request_id: id.clone() })).unwrap();
        let other = coded(&a);
        new_card(&mut a, &other, &[]);
        assert!(notices(&a).last().unwrap().id.ends_with(&other), "the namer holds no card any more");
    }
    /// A pass with a voice talking in the call: the guess runs before the card, and the card carries it; a failing
    /// model still brings the card, without a guess.
    #[test]
    fn a_pass_guesses_before_the_card_shows() {
        let (_t, a) = app();
        let code = coded(&a);
        let now = agent::now_ms();
        let lines = [("User", "Mari, can you hear me okay?"), (code.as_str(), "Yeah I can hear you fine now."), (code.as_str(), "It was way faster than last week, honestly."), (code.as_str(), "We should ship it on Friday then.")];
        for (k, (who, text)) in lines.iter().enumerate() {
            archived(&a, 200 + k as u64, who, false);
            let c = heimdall_archive::store::open(&crate::archive::store_root(&a.session)).unwrap();
            c.execute("UPDATE utterances SET wall_ms=?1, text=?2 WHERE id LIKE ?3", rusqlite::params![now as i64 - 60_000 + k as i64 * 5_000, text, format!("%:{}", 200 + k)]).unwrap();
        }
        let db = crate::archive::store_root(&a.session).join(heimdall_archive::store::INDEX);
        let pass = |look: &mut dyn FnMut(&str) -> Result<String>| crate::pipeline::voice_names::background_pass(&db, &a.data.join("speakers.json"), &a.data.join("voice-namer.json"), std::slice::from_ref(&code), now, None, Some(look)).unwrap();
        let mut model = |_: &str| -> Result<String> { Ok(serde_json::json!({"candidates": [{"name": "Mari", "confidence": "medium", "hints": []}], "reason": ""}).to_string()) };
        let namer::Outcome::Suggest(s) = pass(&mut model) else { panic!("a card") };
        assert_eq!((s.code.as_str(), s.quote.as_str()), (code.as_str(), "We should ship it on Friday then."));
        assert_eq!(sg::card_options(&s), ["Mari", "Not important"]);
        let mut broken = |_: &str| -> Result<String> { anyhow::bail!("quota") };
        let namer::Outcome::Suggest(s) = pass(&mut broken) else { panic!("a card") };
        assert_eq!((sg::card_options(&s), s.error.as_deref()), (vec!["Not important".to_string()], Some("quota")));
    }
    #[test]
    fn latest_never_picks_a_voice_marked_not_important_and_forget_takes_the_card_down() {
        let (_t, mut a) = app();
        let code = coded(&a);
        archived(&a, 96, &code, false);
        let c = heimdall_archive::store::open(&crate::archive::store_root(&a.session)).unwrap();
        c.execute("UPDATE utterances SET wall_ms=?1", [agent::now_ms() as i64 - 60_000]).unwrap();
        new_card(&mut a, &code, &[]);
        let id = notices(&a)[0].id.clone();
        a.forget_voice(&code).unwrap();
        assert!(a.hooks.ui.lock().unwrap().iter().any(|u| matches!(u, ui::UiUpdate::DismissAgentNotice(d) if *d == id)));
        assert!(a.latest_unnamed_voice().is_err());
    }
    #[test]
    fn naming_the_voice_elsewhere_takes_its_card_down() {
        let (_t, mut a) = app();
        let code = coded(&a);
        archived(&a, 90, &code, false);
        new_card(&mut a, &code, &["Elizabeth"]);
        let id = notices(&a)[0].id.clone();
        a.rename_voice(&code, "Elizabeth").unwrap();
        assert!(a.hooks.ui.lock().unwrap().iter().any(|u| matches!(u, ui::UiUpdate::DismissAgentNotice(d) if *d == id)));
        assert!(answer_card(&mut a, &id, "Elizabeth").is_err());
    }
    #[test]
    fn name_voice_latest_names_the_voice_owner_was_just_talking_to() {
        let (_t, mut a) = app();
        let code = coded(&a);
        archived(&a, 95, &code, false);
        // The archived line's time is now: it is the last conversation's.
        let c = heimdall_archive::store::open(&crate::archive::store_root(&a.session)).unwrap();
        c.execute("UPDATE utterances SET wall_ms=?1", [agent::now_ms() as i64 - 60_000]).unwrap();
        let (latest, quote, others) = a.latest_unnamed_voice().unwrap();
        assert_eq!((latest.as_str(), quote.as_str(), others.len()), (code.as_str(), "hello there", 0));
        a.rename_voice(&latest, "Elizabeth").unwrap();
        assert_eq!(speakers_in_archive(&a), vec!["Elizabeth"]);
        assert!(a.latest_unnamed_voice().is_err(), "nobody unnamed is left");
    }
    #[test]
    fn naming_a_coded_voice_relabels_every_session_and_live_line() {
        let (_t, mut a) = app();
        let code = coded(&a);
        archived(&a, 31, &code, true);
        archived(&a, 32, &code, false);
        archived(&a, 33, "Angel", false);
        let track = TrackId { source: "discord".into(), session: 1, speaker: 2 };
        a.speaker_events(vec![SpeakerEvent::VoiceCoded { track: track.clone(), code: code.clone(), new: false }]).unwrap();
        let reply = a.rename_voice(&code, "John").unwrap();
        assert!(reply.contains("2 earlier line(s)"), "{reply}");
        assert_eq!(speakers_in_archive(&a), vec!["John", "John", "Angel"]);
        assert_eq!(a.speaker_names[&track], "John");
        let store = EnrollmentStore::load(&a.data.join("speakers.json")).unwrap();
        assert!(store.voices.is_empty() && store.names["John"].embeddings.len() == 2, "the samples became John's");
        // A name can be corrected the same way; an unknown voice is refused. A rename changes no moment's members.
        let members = |a: &Pipeline| -> Vec<(String, String)> {
            let c = heimdall_archive::store::open(&crate::archive::store_root(&a.session)).unwrap();
            let mut q = c.prepare("SELECT chunk_id, target FROM chunk_links WHERE relation='member' ORDER BY 1, 2").unwrap();
            q.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().map(Result::unwrap).collect()
        };
        let before = members(&a);
        assert!(!before.is_empty(), "the test archive has moments");
        a.rename_voice("John", "Jon").unwrap();
        assert_eq!(members(&a), before);
        assert_eq!(speakers_in_archive(&a)[0], "Jon");
        // Every chunk tag follows; no moment is linked to itself (review of 23 Oct) and none keeps an old label.
        let c = heimdall_archive::store::open(&crate::archive::store_root(&a.session)).unwrap();
        let tag = |v: &str| c.query_row("SELECT count(*) FROM chunk_tags WHERE key='speaker' AND value=?1", [v], |r| r.get::<_, i64>(0)).unwrap();
        assert!(tag("Jon") > 0);
        assert_eq!((tag("John"), tag(&code)), (0, 0));
        let self_links: i64 = c.query_row("SELECT count(*) FROM chunk_links WHERE relation='member' AND chunk_id=target", [], |r| r.get(0)).unwrap();
        assert_eq!(self_links, 0);
        // Anonymous labels are never renamed everywhere.
        assert!(a.rename_voice("Speaker 2", "X").is_err());
        assert!(a.rename_voice("Unknown", "X").is_err());
        assert!(a.rename_voice("Speaker nobody", "X").is_err());
        assert!(a.rename_voice("Jon", " ").is_err());
    }
    #[test]
    fn the_card_asks_once_and_yes_names_the_voice() {
        let (_t, mut a) = app();
        let code = coded(&a);
        archived(&a, 40, &code, false);
        a.show_voice_card(proposal(&code));
        a.show_voice_card(proposal(&code));
        let shown = notices(&a);
        assert_eq!(shown.len(), 1, "one card at a time");
        assert_eq!(shown[0].text, "Is this John? \u{201c}User: hey John, you there?\u{201d}");
        assert!(matches!(&shown[0].action, Some(ui::agent_notice::NoticeAction::Ask { questions, .. })
            if questions.len() == 1 && questions[0].options.iter().map(|o| o.label.as_str()).collect::<Vec<_>>() == ["Yes", "Not important"]));
        assert_eq!(field(&shown[0]), Some(("Type the name".into(), false, 1)), "Yes, the name field, Not important; never focused: he did not ask");
        assert_eq!(a.speaker_prompts, 1);
        answer(&mut a, &code, "Yes").unwrap();
        assert_eq!(speakers_in_archive(&a), vec!["John"]);
        assert!(a.hooks.ui.lock().unwrap().iter().any(|u| matches!(u, ui::UiUpdate::DismissAgentNotice(id) if id.ends_with(&code))));
        assert!(answer(&mut a, &code, "Yes").is_err(), "answered once");
    }
    #[test]
    fn not_important_marks_the_voice_irrelevant_and_the_field_takes_another_name() {
        let (_t, mut a) = app();
        let code = coded(&a);
        a.show_voice_card(proposal(&code));
        answer(&mut a, &code, "Not important").unwrap();
        assert!(EnrollmentStore::load(&a.data.join("speakers.json")).unwrap().voices[&code].irrelevant);

        let (_t2, mut b) = app();
        let code = coded(&b);
        archived(&b, 50, &code, false);
        b.show_voice_card(proposal(&code));
        type_name(&mut b, &code, "Dave").unwrap();
        assert_eq!(speakers_in_archive(&b), vec!["Dave"]);
        assert!(!b.hooks.ui.lock().unwrap().iter().any(|u| matches!(u, ui::UiUpdate::VoiceNameEntry { .. })), "no second step");
    }
    #[test]
    fn the_transcript_names_a_coded_label_directly() {
        let (_t, mut a) = app();
        let code = coded(&a);
        archived(&a, 60, &code, false);
        a.action(ui::UiAction::NameTranscriptSpeaker { line_id: 60, source: "Discord".into(), speaker: code.clone(), name: "Erin".into() }).unwrap();
        assert_eq!(speakers_in_archive(&a), vec!["Erin"]);
    }
    #[test]
    fn a_speaker_n_label_never_renames_a_known_person() {
        let (_t, mut a) = app();
        let path = a.data.join("speakers.json");
        let mut store = EnrollmentStore::default();
        store.enroll("Dave", voice()).unwrap();
        store.save(&path).unwrap();
        let track = TrackId { source: "discord".into(), session: 1, speaker: 2 };
        archived(&a, 70, "Speaker 2", false);
        a.labels.insert(70, (ui::TranscriptLine { id: 70, label: Some(ui::Label { source: "Discord".into(), speaker: "Speaker 2".into() }), segments: vec![], is_final: true }, Some(track.clone())));
        a.speaker_events(vec![SpeakerEvent::LabelChanged { track, name: "Dave".into() }]).unwrap();
        assert!(a.name_transcript_speaker(70, "Discord", "Speaker 2", "Erin").is_err());
        assert!(EnrollmentStore::load(&path).unwrap().names.contains_key("Dave"));
    }
    #[test]
    fn replay_and_quiet_calls_spend_nothing() {
        let (_t, mut a) = app();
        // The test pipeline is a replay one: no pass, ever.
        a.voice_namer_tick();
        assert!(!a.data.join("voice-namer.json").exists());
    }
