    //! [chrome] Naming "Speaker N" from the transcript window: the label resolves to the call's voice track.
    use super::*;
    fn line(id: u64, speaker: &str) -> ui::TranscriptLine {
        ui::TranscriptLine { id, label: Some(ui::Label { source: "Discord".into(), speaker: speaker.into() }), segments: vec![], is_final: true }
    }
    #[test]
    fn transcript_labels_resolve_to_the_lines_call_session_and_refuse_named_or_unknown_speakers() {
        let (_t, mut a) = app();
        let track = TrackId { source: "discord".into(), session: 3, speaker: 1 };
        // A run line (one id, several speakers): Speaker 2 is the same call session's speaker 2.
        a.labels.insert(40, (line(40, "Speaker 1"), Some(track.clone())));
        assert_eq!(a.transcript_track(40, "Discord", "Speaker 2").unwrap(), TrackId { speaker: 2, ..track.clone() });
        assert_eq!(a.transcript_track(40, "Discord", "Speaker 1").unwrap(), track);
        // A named voice, "Speaker ?" and out-of-range numbers are not nameable here.
        for speaker in ["Dave", "Speaker ?", "Speaker 0", "Speaker 9", "Speakers 2"] {
            assert!(a.transcript_track(40, "Discord", speaker).is_err(), "{speaker}");
        }
        // A line without a track and no open call: nothing to name, and the action says so without sending.
        a.labels.insert(41, (line(41, "Speaker 2"), None));
        assert!(a.transcript_track(41, "Discord", "Speaker 2").is_err());
        assert!(a.action(ui::UiAction::NameTranscriptSpeaker { line_id: 41, source: "Discord".into(), speaker: "Speaker 2".into(), name: "Ada".into() }).is_err());
        // The line's call closed (no actor): the name is not lost silently, the action fails.
        assert!(a.action(ui::UiAction::NameTranscriptSpeaker { line_id: 40, source: "Discord".into(), speaker: "Speaker 2".into(), name: "Ada".into() }).is_err());
        assert!(a.action(ui::UiAction::NameTranscriptSpeaker { line_id: 40, source: "Discord".into(), speaker: "Speaker 2".into(), name: "  ".into() }).is_err());
    }
