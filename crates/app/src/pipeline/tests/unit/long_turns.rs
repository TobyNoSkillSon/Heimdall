    use super::*;
    /// "whenever I say look at this, then the screenshot has been taken ... I may
    /// talk for like another ... five minutes ... it doesn't get broken".
    #[test]
    fn look_at_this_then_five_minutes_of_talk_attaches_the_phrase_time_frames() {
        let (_t, mut a, source) = long_turn_app(20);
        let (actions, errors) = long_turn(&mut a, &source, 300, &[look(0, true, true)]);
        assert!(errors.iter().all(|e| e.is_null()), "command failed: {errors:?}");
        assert_eq!(actions.len(), 1, "{actions:?}");
        assert!(actions[0]["full_resolution_errors"].as_array().unwrap().is_empty(), "{}", actions[0]);
        // Ring: both displays as they were when "look" began. Full resolution: grabbed at the
        // first replay step after the phrase's last word (step 1, the screen of second 1), where the partial
        // carried it — never the screen five minutes on.
        assert_eq!(archived_with_preferred_attachments(&a, &actions[0]), [
            ("ring".into(), 1, 0, 0, 1), ("ring".into(), 2, 0, 0, 2),
            ("full_resolution".into(), 1, 1, 1, 101), ("full_resolution".into(), 2, 1, 1, 102),
        ]);
        assert!(a.screen_holds.is_empty() && a.command_candidates.is_empty());
        let chain = a.index.evidence.records().unwrap().into_iter().find(|r| r.payload["phase"] == "chain").unwrap();
        let origins: Vec<_> = chain.payload["proposals"].as_array().unwrap().iter().map(|p| p["origin"].as_str().unwrap().to_string()).collect();
        // Held at the partial, decided from the first window decode (whose text also re-proposes it as a partial);
        // the final's later detection merges into the decided command.
        assert_eq!(origins, ["parakeet_partial", "parakeet_partial", "parakeet_window"]);
        assert_eq!(chain.payload["stage"], "early");
        assert!(chain.payload["audio"].is_null(), "no audio reaches the command decider");
        // The decider reads at most the 15 s up to the phrase, never the five-minute turn.
        let text = chain.payload["transcript"].as_str().unwrap();
        assert!(text.starts_with("Look at this") && text.split_whitespace().count() <= 4, "{text}");
    }
    #[test]
    fn two_commands_in_a_three_minute_turn_each_attach_their_own_frames() {
        let (_t, mut a, source) = long_turn_app(20);
        // The first is carried by a partial; the second only by the 15-s window decode.
        let (actions, errors) = long_turn(&mut a, &source, 180, &[look(10, true, true), Spoken { at_s:100,phrase:"take a screenshot",partial:false,answer:Some((decide::CommandIntent::Yes,decide::CommandSelection::TakeScreenshot)) }]);
        assert!(errors.iter().all(|e| e.is_null()), "command failed: {errors:?}");
        assert_eq!(actions.len(), 2);
        let shots: Vec<_> = actions.iter().flat_map(|a| a["shots"].as_array().unwrap().clone()).collect();
        let at = |kind: &str| shots.iter().filter(|s| s["kind"] == kind).map(|s| (s["frame_ns"].as_u64().unwrap() - S) / S).collect::<Vec<_>>();
        assert_eq!(at("ring"), [10, 10, 100, 100]);
        // The window decode that found the second phrase ran at 104 s: 4 s after, not 80 s after.
        assert_eq!(at("full_resolution"), [11, 11, 104, 104]);
        assert!(a.screen_holds.is_empty());
    }
    #[test]
    fn rejected_command_leaves_nothing_on_disk_and_releases_its_hold() {
        let (_t, mut a, source) = long_turn_app(20);
        let (actions, errors) = long_turn(&mut a, &source, 60, &[look(5, true, false)]);
        assert!(errors.iter().all(|e| e.is_null()), "{errors:?}");
        assert!(actions.is_empty());
        assert!(a.screen_holds.is_empty() && a.attachment_batch.is_empty());
        assert!(!a.index.evidence.records().unwrap().iter().any(|r| r.kind == "screenshot"));
        let shots = crate::archive::run_dir(&a.data, &a.session).join("shots");
        assert!(!shots.exists() || std::fs::read_dir(&shots).unwrap().next().is_none());
    }
    #[test]
    fn five_second_history_still_attaches_the_phrase_frames_after_five_minutes() {
        let (_t, mut a, source) = long_turn_app(5);
        let (actions, errors) = long_turn(&mut a, &source, 300, &[look(0, true, true)]);
        assert!(errors.iter().all(|e| e.is_null()), "command failed: {errors:?}");
        let got = archived_with_preferred_attachments(&a, &actions[0]);
        assert_eq!(got.iter().map(|s| (s.0.as_str(), s.2, s.3)).collect::<Vec<_>>(),
            [("ring", 0, 0), ("ring", 0, 0), ("full_resolution", 1, 1), ("full_resolution", 1, 1)]);
    }
    #[test]
    fn closed_source_abandons_the_command_and_releases_its_hold() {
        let (_t, mut a, source) = long_turn_app(20);
        for t in 0..4 {
            a.screen_buffer.lock().unwrap().push(screen::Frame::new(screen::DisplayInfo { id: 1, x: 0., y: 0., width: 1, height: 1 }, t * S, 1, 1, vec![t as u8; 3]).unwrap()).unwrap();
        }
        a.listen(ListenEvent::SpeechStart { source: source.clone(), host_time_ns: 2 * S }).unwrap();
        let mut heard = timed("look at this.", 2 * S);
        a.listen(ListenEvent::PartialTranscript { source: source.clone(), text: "look at this.".into(), words: heard.clone(), stable_words: 0 }).unwrap();
        assert_eq!(a.screen_holds.len(), 1);
        heard.extend(timed("take a screenshot", 8 * S));
        a.listen(ListenEvent::PartialTranscript { source: source.clone(), text: "look at this. take a screenshot".into(), words: heard, stable_words: 0 }).unwrap();
        assert_eq!(a.screen_holds.len(), 2);
        a.close_source(&source);
        assert!(a.screen_holds.is_empty() && a.command_candidates.is_empty());
        assert!(a.index.evidence.records().unwrap().iter().any(|r| r.payload["phase"] == "abandoned"));
    }
    #[test]
    fn a_screenshot_at_the_start_of_a_five_minute_turn_still_decides() {
        let (_t, mut a, source) = long_turn_app(20);
        // Not carried by a partial here: the window decode is its first detection.
        let (_, errors) = long_turn(&mut a, &source, 300, &[Spoken { at_s: 0, phrase: "Take a screenshot", partial: false, answer: Some((decide::CommandIntent::Yes, decide::CommandSelection::TakeScreenshot)) }]);
        assert!(errors.iter().all(|e| e.is_null()), "command failed: {errors:?}");
        let records = a.index.evidence.records().unwrap();
        let chain = records.iter().find(|r| r.payload["phase"] == "chain").unwrap();
        assert_eq!(chain.payload["decider_text"]["origin"], "window_decode");
        assert!(records.iter().any(|r| r.kind == "command_action" && r.payload["command"] == "take_screenshot"));
    }
