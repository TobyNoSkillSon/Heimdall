use decide::*;
use serde_json::json;
fn input() -> DecisionInput {
    DecisionInput {
        source: DecisionSource::Microphone,
        is_final: true,
        current: Turn {
            id: "one".into(),
            role: Role::Owner,
            transcript: "Please open the first link.".into(),
            audio: AudioClip::new(vec![0.; 8000]).unwrap(),
        },
        context: vec![],
        pending_transcript: String::new(),
        agent_state: String::new(),
        media: DecisionMedia::Text,
        questions: vec![Question::CommandCheck(CommandSelection::LookAtThis)],
    }
}
#[test]
fn bounded_inputs_and_closed_questions() {
    let mut i = input();
    assert!(i.validate().is_ok());
    i.questions.push(Question::CommandCheck(CommandSelection::LookAtThis));
    assert!(i.validate().is_err(), "duplicate question");
    i.questions = vec![Question::Legacy];
    assert!(i.validate().is_err(), "a legacy question is never asked");
    i.questions = vec![Question::CommandCheck(CommandSelection::OtherCommand)];
    assert!(i.validate().is_err(), "a check names a listed command");
    i = input();
    i.current.audio.samples[0] = f32::NAN;
    assert!(i.current.audio.validate().is_err());
    assert!(i.validate().is_ok());
    i = input();
    i.current.audio.sample_rate = 48000;
    assert!(i.current.audio.validate().is_err());
    assert!(i.validate().is_ok());
    i = input();
    i.current.role = Role::Agent;
    assert!(i.validate().is_err());
    i = input();
    i.context = vec![i.current.clone(); 21];
    assert!(i.validate().is_err());
    i = input();
    i.current.audio.samples = vec![0.; 480001];
    assert!(i.current.audio.validate().is_err());
    assert!(i.validate().is_ok());
}
#[test]
fn media_exclusion_and_typed_question_serialization() {
    let mut i = input();
    let tokens = |s: &str| Ok(s.len() / 3);
    let text = i.fit_text(tokens).unwrap();
    i.media = DecisionMedia::Audio;
    let req = i.fit_text(tokens).unwrap();
    assert_eq!(req, text);
    assert!(req.get("files").is_none());
    assert!(req.get("images").is_none());
    assert_eq!(req["questions"]["command_check"]["type"], "choice");
    let q = D1::questions(&i);
    assert_eq!(q["command_check"]["criteria"].as_object().unwrap().len(), 3);
    assert_eq!(
        serde_json::to_value(&i.questions).unwrap(),
        json!([{"question":"command_check","candidates":"look_at_this"}])
    );
}
#[test]
fn archive_round_trip_preserves_exact_audio_and_failures() {
    let tmp = tempfile::tempdir().unwrap();
    let mut a = TrainingArchive::open(tmp.path()).unwrap();
    let i = input();
    a.append(&i, None, Some("model unavailable")).unwrap();
    a.append(&i, None, Some("model unavailable")).unwrap();
    let log = std::fs::read_to_string(tmp.path().join("decisions.jsonl")).unwrap();
    let lines: Vec<_> = log.lines().collect();
    assert_eq!(lines.len(), 2);
    let v: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(v["error"], "model unavailable");
    let path = v["input"]["current"]["audio"]["path"].as_str().unwrap();
    let actual: Vec<f32> = hound::WavReader::open(tmp.path().join(path)).unwrap().samples::<f32>().map(Result::unwrap).collect();
    assert_eq!(actual, i.current.audio.samples);
    assert_eq!(
        std::fs::read_dir(tmp.path().join("audio")).unwrap().count(),
        1
    );
    assert_eq!(v["d1_questions"]["command_check"]["type"], json!("choice"));
}
#[test]
fn handmade_labels_cover_requests_context_and_human_speech() {
    let v: Vec<serde_json::Value> =
        serde_json::from_str(include_str!("fixtures/smoke.json")).unwrap();
    assert_eq!(v.len(), 30);
    assert_eq!(v.iter().filter(|x| x["send_now"] == true).count(), 15);
    assert_eq!(v.iter().filter(|x| x["addressee"] == true).count(), 21);
    assert!(v.iter().all(|x| !x["text"].as_str().unwrap().is_empty()));
}

#[test]
fn long_history_keeps_current_first_and_context_newest_first() {
    let mut i = input();
    i.current.transcript = "CURRENT: take a screenshot now".into();
    i.context = (0..20)
        .map(|n| Turn {
            id: format!("history-{n}"),
            role: Role::Agent,
            transcript: format!("history-{n} {}", "old context ".repeat(2000)),
            audio: AudioClip::new(vec![0.; 160]).unwrap(),
        })
        .collect();
    // The JSON state (archived as `d1_state`): current first, context newest first.
    let state = i.state_text(0.9).unwrap();
    assert!(
        state.starts_with(r#"{"utterance":"CURRENT: take a screenshot now","pending_transcript":"#)
    );
    assert!(state.find("history-19").unwrap() < state.find("history-18").unwrap());
    assert!(state.find("history-18").unwrap() < state.find("history-0").unwrap());
    // The request the decider reads (V5's layout) keeps the current sentence last and drops the oldest history first.
    let req = i.fit_text(|s| Ok(s.len() / 3)).unwrap();
    let state = req["state"].as_str().unwrap();
    assert!(state.ends_with("User: CURRENT: take a screenshot now") && !state.contains("history-0 "), "{state}");
    i.context.iter_mut().for_each(|t| t.transcript.truncate(40));
    let state = i.fit_text(|s| Ok(s.len() / 3)).unwrap()["state"].as_str().unwrap().to_owned();
    assert!(state.contains("history-19") && state.contains("history-0 "), "short history fits whole: {state}");
    assert_eq!(req["questions"]["command_check"]["instructions"], "Did User, right now, ask Heimdall to look at his screen?");
}

#[test]
fn pending_thought_has_priority_over_history_and_tcl_is_not_model_input() {
    let mut i = input();
    i.pending_transcript = "Earlier line. Current line.".into();
    let state = i.state_text(0.0).unwrap();
    assert!(state.find("pending_transcript").unwrap() < state.find("recent_turns").unwrap());
    assert_eq!(state, i.state_text(1.0).unwrap());
    assert!(!state.contains("addressed_probability"));
}

#[test]
fn removed_send_assets_do_not_supply_a_runtime_or_policy() {
    let tmp=tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("send-head.onnx"),[]).unwrap();
    let error=LocalDecider::load(tmp.path(),tmp.path().join("archive")).err().unwrap().to_string();
    assert!(!error.contains("supplied together"));
    assert!(!tmp.path().join("archive").exists(),"missing d1 must fail before opening an archive");
}
