    use super::*;
    #[test]
    fn island_cues_name_the_command_and_its_outcome() {
        use decide::CommandSelection as C;
        assert_eq!(screen::EXCLUDED_WINDOW_TITLE, ui::ISLAND_WINDOW_TITLE, "capture filters must find the island");
        assert_eq!(cue_kind("look at this"), ui::CommandKind::LookAtThis);
        assert_eq!(cue_kind("take a screenshot"), ui::CommandKind::Screenshot);
        // "bug note" is no longer a command; its words would only ever look like an unknown phrase.
        assert_eq!(cue_kind("bug note"), ui::CommandKind::LookAtThis);
        // Decider V5's two screen commands only ( and 20:57): cancel-task and the "stop completely"
        // family propose nothing; their words reach the agent as speech. "Stop speaking" was never a command.
        assert_eq!(COMMAND_PHRASES, ["look at this", "take a screenshot"]);
        assert!(C::ALL.iter().all(|c| c.key() != "cancel_task"));
        let words = |text: &str| text.split_whitespace().enumerate().map(|(n, w)| screen::TimedWord { text: w.into(), start_ns: n as u64 * 300_000_000, end_ns: n as u64 * 300_000_000 + 250_000_000 }).collect::<Vec<_>>();
        for said in ["Can you stop completely? I want to talk to you.", "cancel the task", "cancel task now", "cancel that", "stop everything", "stop what you're doing", "stop speaking right now", "take a look at the screen"] {
            assert!(screen::PhraseMatcher::new(COMMAND_PHRASES).observe_utterance("mic", "explicit", &words(said), true).is_empty(), "{said}");
        }
        let done = |label: &str, kind| ui::CommandCue::Done { id: Some(1), kind, label: label.into() };
        let failed = |reason: &str, kind| ui::CommandCue::Failed { id: Some(1), kind, reason: reason.into() };
        assert_eq!(action_cue(1, C::LookAtThis, Ok(()), false, true), done("Screenshot", ui::CommandKind::LookAtThis));
        assert_eq!(action_cue(1, C::LookAtThis, Ok(()), true, false), failed("Seeing is off", ui::CommandKind::LookAtThis));
        let floor = anyhow::anyhow!("below disk floor");
        assert_eq!(action_cue(1, C::TakeScreenshot, Err(&floor), false, false), failed("Disk almost full", ui::CommandKind::Screenshot));
        assert_eq!(action_cue(1, C::Unsure, Err(&floor), false, false), ui::CommandCue::Dropped { id: 1 });
    }
    #[test]
    fn approved_screen_command_keeps_words_and_attaches_archived_image() {
        let (_t, mut a) = app();
        let i = crate::fake_source("mic", 0, 1);
        let source = i.id.0.clone();
        a.source(i);
        a.audio.insert(
            source.clone(),
            AudioRing {
                start: 0,
                samples: vec![0.01; 16000 * 10].into(),
            },
        );
        a.screen_buffer
            .lock()
            .unwrap()
            .push(
                screen::Frame::new(
                    screen::DisplayInfo {
                        id: 1,
                        x: 0.,
                        y: 0.,
                        width: 1,
                        height: 1,
                    },
                    2_000_000_000,
                    1,
                    1,
                    vec![20, 40, 60],
                )
                .unwrap(),
            )
            .unwrap();
        a.command_test_answers.push_back((
            decide::CommandIntent::Yes,
            decide::CommandSelection::LookAtThis,
        ));
        say(&mut a, &source, "look at this", 2_000_000_000, false);
        assert_eq!(a.notes, 1);
        assert_eq!(a.attachment_batch.len(), 1);
        let rows = a.index.utterances_between(0, 10_000_000_000).unwrap();
        assert!(rows[0]["text"]
            .as_str()
            .unwrap()
            .starts_with("look at this [screenshot"));
        let records = a.index.evidence.records().unwrap();
        assert!(records.iter().any(|r| r.kind == "command_action"
            && r.payload["command"] == "look_at_this"
            && r.payload["error"].is_null()));
        assert!(records.iter().any(|r| r.kind == "screenshot"));
    }
    /// Seeing off: a frame left in the buffer is never attached; the command's words keep a plain note instead.
    #[test]
    fn screen_command_with_seeing_off_notes_screen_off_and_attaches_nothing() {
        let (_t, mut a) = app();
        let i = crate::fake_source("mic", 0, 1);
        let source = i.id.0.clone();
        a.source(i);
        a.audio.insert(source.clone(), AudioRing { start: 0, samples: vec![0.01; 16000 * 10].into() });
        let display = screen::DisplayInfo { id: 1, x: 0., y: 0., width: 1, height: 1 };
        a.screen_buffer.lock().unwrap().push(screen::Frame::new(display, 2_000_000_000, 1, 1, vec![20, 40, 60]).unwrap()).unwrap();
        let mut next = a.settings.clone();
        next.screen_off = true;
        a.apply_menu_settings(next).unwrap();
        assert!(a.tools.screen_off.load(Ordering::Acquire) && !a.runtime_settings_value().seeing);
        a.command_test_answers.push_back((decide::CommandIntent::Yes, decide::CommandSelection::LookAtThis));
        say(&mut a, &source, "look at this", 2_000_000_000, false);
        assert_eq!(a.notes, 1);
        assert!(a.attachment_batch.is_empty());
        let rows = a.index.utterances_between(0, 10_000_000_000).unwrap();
        assert_eq!(rows[0]["text"].as_str().unwrap(), format!("look at this {SCREEN_OFF_NOTE}"));
        let records = a.index.evidence.records().unwrap();
        assert!(records.iter().any(|r| r.kind == "screen_command" && r.payload["screen_off"] == true));
        assert!(!records.iter().any(|r| r.kind == "screenshot"));
        assert!(!a.component_status.contains_key("command-action") && !a.component_status.contains_key("screen-command"));
    }
    /// A window decode decides the command; the final's later detection of it is a merged row, not a second chain
    /// or action. The same command in the next utterance, even within a second, is a new one.
    #[test]
    fn window_and_final_merge_once_and_a_late_detection_is_merged() {
        let (_t, mut a) = app();
        let info = crate::fake_source("mic", 0, 1); let source = info.id.0.clone(); a.source(info);
        a.audio.insert(source.clone(), AudioRing { start: 0, samples: vec![0.01; 16000*10].into() });
        a.command_test_answers.push_back((decide::CommandIntent::Yes, decide::CommandSelection::TakeScreenshot));
        let words = timed("take a screenshot now", 2_000_000_000);
        let end = words.last().unwrap().end_ns;
        a.listen(ListenEvent::SpeechStart { source: source.clone(), host_time_ns: 2_000_000_000 }).unwrap();
        let segment = words[..3].to_vec();
        a.listen(ListenEvent::TranscriptSegment { source: source.clone(), text: "take a screenshot".into(), start_ns: 2_000_000_000, end_ns: segment[2].end_ns, words: segment }).unwrap();
        assert_eq!(a.index.evidence.records().unwrap().iter().filter(|r| r.kind=="command_action").count(), 1, "decided at the window");
        a.listen(ListenEvent::FinalUtterance { source: source.clone(), text: "now".into(), words: words[3..].to_vec(), start_ns: words[3].start_ns, end_ns: end, end_of_turn_probability: 0.9, forced: false }).unwrap();
        let records = a.index.evidence.records().unwrap();
        let chains: Vec<_> = records.iter().filter(|r| r.payload["phase"] == "chain").collect();
        assert_eq!(chains.len(), 1);
        // A screen phrase in a window segment is also matched as a partial: either way it is decided before the final.
        assert_ne!(chains[0].payload["proposals"][0]["origin"], "parakeet_final");
        assert_eq!(records.iter().filter(|r| r.kind=="command_action").count(), 1);
        let merged: Vec<_> = records.iter().filter(|r| r.payload["phase"]=="merged" && r.payload["reason"]=="already_decided").collect();
        assert!(!merged.is_empty());
        let origins: Vec<_> = merged.last().unwrap().payload["proposals"].as_array().unwrap().iter().map(|p| p["origin"].as_str().unwrap()).collect();
        assert_eq!(origins.last(), Some(&"parakeet_final"), "{origins:?}");
        assert!(a.command_candidates.is_empty());
        // A subsequent spoken command is a distinct utterance, even nearby (its trigger is 1 s after the first).
        a.command_test_answers.push_back((decide::CommandIntent::Yes, decide::CommandSelection::TakeScreenshot));
        say(&mut a, &source, "take a screenshot", 3_000_000_000, false);
        assert_eq!(a.index.evidence.records().unwrap().iter().filter(|r| r.kind=="command_action").count(), 2);
    }
    #[test]
    fn overlapping_aliases_merge_but_distinct_occurrences_and_sources_do_not() {
        let candidate = |source: &str, ns, phrase: &str| decide::CommandCandidate { source: source.into(), trigger_ns: ns, phrase_guess: phrase.into(), score: 1. };
        let proposals = [Proposal { origin: "parakeet_partial", candidate: candidate("mic:a", 2_000_000_000, "look at this") }];
        assert!(overlaps(&proposals, &candidate("mic:a", 2_400_000_000, "take a screenshot"), "parakeet_final"));
        assert!(!overlaps(&proposals, &candidate("mic:b", 2_400_000_000, "look at this"), "parakeet_final"));
        assert!(!overlaps(&proposals, &candidate("mic:a", 4_000_000_000, "look at this"), "parakeet_final"));
    }
    #[test]
    fn a_repeated_command_in_one_final_has_one_chain_and_action() {
        let (_t, mut a) = app();let info=crate::fake_source("mic",0,1);let source=info.id.0.clone();a.source(info);
        a.audio.insert(source.clone(),AudioRing {start:0,samples:vec![0.01;16000*10].into()});
        a.command_test_answers.extend([(decide::CommandIntent::Yes,decide::CommandSelection::TakeScreenshot);2]);
        say(&mut a,&source,"take a screenshot and take a screenshot",2_000_000_000,false);
        let records=a.index.evidence.records().unwrap();
        assert_eq!(records.iter().filter(|r|r.payload["phase"]=="chain").count(),1);
        assert_eq!(records.iter().filter(|r|r.kind=="command_action").count(),1);
    }
    /// A command that fails before its cue (its decision cannot be recorded) still settles the island, and nothing of
    /// that final stays held.
    #[test]
    fn a_failed_command_settles_its_cue_and_keeps_the_rest() {
        use decide::{CommandIntent as I, CommandSelection as C};
        let (t, mut a) = app();let info=crate::fake_source("mic",0,1);let source=info.id.0.clone();a.source(info);
        a.audio.insert(source.clone(),AudioRing {start:0,samples:vec![0.01;16000*20].into()});
        let last_cue = |a: &Pipeline| a.hooks.ui.lock().unwrap().iter().rev().find_map(|u| match u { ui::UiUpdate::Command(c) => Some(c.clone()), _ => None }).unwrap();
        std::fs::write(t.path().join("not-a-dir"), b"").unwrap();
        a.bugs.inbox = t.path().join("not-a-dir/inbox");
        a.command_test_answers.extend([(I::Unsure, C::TakeScreenshot); 2]);
        let words = timed("take a screenshot and take a screenshot", 8_000_000_000);
        let end = words.last().unwrap().end_ns;
        a.listen(ListenEvent::SpeechStart { source: source.clone(), host_time_ns: 8_000_000_000 }).unwrap();
        let r = a.listen(ListenEvent::FinalUtterance { source: source.clone(), text: "take a screenshot and take a screenshot".into(), words, start_ns: 8_000_000_000, end_ns: end, end_of_turn_probability: 0.9, forced: false });
        assert!(r.is_err());
        assert!(matches!(last_cue(&a), ui::CommandCue::Dropped { .. } | ui::CommandCue::Failed { .. }), "{:?}", last_cue(&a));
        assert!(a.command_candidates.len() <= 1 && a.command_inflight.values().all(|n| *n == 0));
    }
    #[test]
    fn primary_change_abandons_old_candidates_before_new_primary_can_act() {
        let (_t, mut a) = app();
        let first=crate::fake_source("mic",0,1); let old=first.id.0.clone(); a.source(first);
        a.source(crate::fake_source("mic",1,1));
        a.microphone_state(&capture::MicrophoneState {in_use:Some("replay-0".into()),..Default::default()});
        let words = timed("look at this", 2_000_000_000);
        a.listen(ListenEvent::PartialTranscript { source: old, text: "look at this".into(), words, stable_words: 0 }).unwrap();
        assert_eq!(a.command_candidates.len(), 1);
        assert_eq!(a.screen_holds.len(), 1, "the partial held the screen from the phrase");
        a.microphone_state(&capture::MicrophoneState {in_use:Some("replay-1".into()),..Default::default()});
        assert!(a.command_candidates.is_empty() && a.screen_holds.is_empty());
        assert!(a.index.evidence.records().unwrap().iter().any(|r| r.payload["phase"]=="abandoned"));
    }
    /// A partial's candidate whose phrase the turn's final no longer carries is abandoned, not decided: its tentative
    /// cue leaves and its held screen is released.
    #[test]
    fn partial_candidate_the_final_retracts_is_abandoned_and_released() {
        let (_t, mut a) = app();
        let i = crate::fake_source("mic", 0, 1);
        let source = i.id.0.clone();
        a.source(i);
        a.audio.insert(source.clone(), AudioRing { start: 0, samples: vec![0.01; 16000 * 10].into() });
        a.listen(ListenEvent::SpeechStart { source: source.clone(), host_time_ns: 2_000_000_000 }).unwrap();
        let heard = timed("look at this", 2_000_000_000);
        a.listen(ListenEvent::PartialTranscript { source: source.clone(), text: "look at this".into(), words: heard, stable_words: 0 }).unwrap();
        assert_eq!(a.command_candidates.len(), 1);
        let id = a.command_candidates[0].trigger_ns;
        let words = timed("luke had this chart", 2_000_000_000);
        let end = words.last().unwrap().end_ns;
        a.listen(ListenEvent::FinalUtterance { source: source.clone(), text: "luke had this chart".into(), words, start_ns: 2_000_000_000, end_ns: end, end_of_turn_probability: 0.9, forced: false }).unwrap();
        assert!(a.command_candidates.is_empty() && a.screen_holds.is_empty());
        let records = a.index.evidence.records().unwrap();
        assert!(records.iter().any(|r| r.payload["reason"] == "partial_phrase_retracted"));
        assert!(!records.iter().any(|r| r.payload["phase"] == "chain" || r.kind == "command_action"));
        let cues: Vec<_> = a.hooks.ui.lock().unwrap().iter().filter_map(|u| match u { ui::UiUpdate::Command(c) => Some(c.clone()), _ => None }).collect();
        assert_eq!(cues, [ui::CommandCue::Heard { id, kind: ui::CommandKind::LookAtThis }, ui::CommandCue::Dropped { id }]);
    }
    #[test]
    fn command_escapes_are_archived_and_auto_entries_are_distinct_from_spoken_notes() {
        let (_t, mut a) = app();
        let primary = crate::fake_source("mic", 0, 1);
        let source = primary.id.0.clone();
        a.source(primary);
        a.audio.insert(
            source.clone(),
            AudioRing {
                start: 0,
                samples: vec![0.01; 16000 * 20].into(),
            },
        );
        for (n, intent, selection) in [
            (
                0,
                decide::CommandIntent::FalseAlarm,
                decide::CommandSelection::LookAtThis,
            ),
            (
                1,
                decide::CommandIntent::Unsure,
                decide::CommandSelection::LookAtThis,
            ),
            (
                2,
                decide::CommandIntent::FalseAlarm,
                decide::CommandSelection::OtherCommand,
            ),
            (
                3,
                decide::CommandIntent::Unsure,
                decide::CommandSelection::Unsure,
            ),
        ] {
            a.command_test_answers.push_back((intent, selection));
            say(
                &mut a,
                &source,
                "look at this",
                (2 + n) * 1_000_000_000,
                false,
            );
        }
        let records = a.index.evidence.records().unwrap();
        let chains: Vec<_> = records
            .iter()
            .filter(|r| r.kind == "command_decision" && r.payload["phase"] == "chain")
            .collect();
        assert_eq!(chains.len(), 4);
        assert!(chains.iter().all(
            |r| r.payload["approved"].is_null() && r.payload["audio"].is_null()
        ));
        // One question per command.
        assert!(chains.iter().all(|r| r.payload["steps"].as_array().unwrap().len() == 1));
        assert!(!records
            .iter()
            .any(|r| r.kind == "command_action" || r.kind == "screen_command"));
        // Command reviews go beside the bug inbox, never into it.
        let entries: Vec<serde_json::Value> = std::fs::read_dir(crate::bugs::reviews_dir(&a.bugs.inbox))
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|e| e == "json"))
            .map(|p| serde_json::from_slice(&std::fs::read(p).unwrap()).unwrap())
            .collect();
        assert_eq!(entries.len(), 2, "the two unsure answers go to review");
        assert!(entries
            .iter()
            .all(|e| e["auto"] == true && e["kind"] == "command_decision_auto"));
        assert!(crate::bugs::BugNotes::new(a.bugs.inbox.clone())
            .listed
            .is_empty());
        assert!(!a.bugs.inbox.exists());
    }
