    //! Don't ask again (bug note 2026-10-21T23-35-54): the prompt's third answer and its undo in the Speakers window.
    use super::*;
    /// The list after the user's own row (always first, listened to: `listen_to_tests`).
    fn speakers(a: &Pipeline) -> Vec<ui::KnownSpeaker> {
        let list = a.hooks.ui.lock().unwrap().iter().rev().find_map(|u| match u { ui::UiUpdate::Speakers(l) => Some(l.clone()), _ => None }).unwrap_or_default();
        list.into_iter().filter(|k| k.listen != ui::Listen::Always).collect()
    }
    fn voice() -> speakers::embedding::Embedding {
        speakers::embedding::Embedding::new(vec![1.; speakers::embedding::DIMENSION]).unwrap()
    }
    #[test]
    fn ignored_voices_are_listed_and_ask_again_undoes_them() {
        let (_t, mut a) = app();
        let path = a.data.join("speakers.json");
        let mut store = EnrollmentStore::default();
        store.enroll("Dave", voice()).unwrap();
        let id = store.ignore(vec![voice()], "Discord", "Subscribe for more videos like this one every single week", 1_792_600_000_000).unwrap();
        store.save(&path).unwrap();
        a.send_speakers();
        let list = speakers(&a);
        assert_eq!(list.len(), 2, "{list:?}");
        assert!(!list[0].ignored && list[0].name == "Dave");
        let ignored = &list[1];
        assert!(ignored.ignored && ignored.id == id && ignored.heard_in == "Discord");
        assert!(ignored.name.starts_with('\u{201c}') && ignored.name.ends_with("\u{2026}\u{201d}"), "short and quoted: {}", ignored.name);
        assert!(!ignored.last_heard.is_empty());
        // Ask Again: gone from the store and the list; the named voice stays.
        a.action(ui::UiAction::UnignoreSpeaker { id: id.clone() }).unwrap();
        let back = EnrollmentStore::load(&path).unwrap();
        assert!(back.ignored.is_empty() && back.names.contains_key("Dave"));
        assert_eq!(speakers(&a).len(), 1);
        // Again for an id already gone: nothing changes, no error.
        a.action(ui::UiAction::UnignoreSpeaker { id }).unwrap();
    }
    /// Quiet speaker naming: Forget on the voice card lists the coded voice under its code; Ask Again undoes it.
    #[test]
    fn a_forgotten_coded_voice_is_listed_and_ask_again_undoes_it() {
        let (_t, mut a) = app();
        let path = a.data.join("speakers.json");
        let mut store = EnrollmentStore::default();
        let code = store.code_voice(vec![voice()], "Discord", "gg", 1_792_600_000_000, "Fri 23 Oct 21h", "2026-10-23").unwrap();
        store.save(&path).unwrap();
        a.forget_voice(&code).unwrap();
        assert!(EnrollmentStore::load(&path).unwrap().voices[&code].irrelevant);
        let list = speakers(&a);
        assert!(list.iter().any(|s| s.ignored && s.id == code && s.name == code), "{list:?}");
        a.action(ui::UiAction::UnignoreSpeaker { id: code.clone() }).unwrap();
        let back = EnrollmentStore::load(&path).unwrap();
        assert!(!back.voices[&code].irrelevant, "kept, and proposable again");
        assert!(speakers(&a).is_empty());
        assert!(a.forget_voice("Speaker nobody").is_err());
    }
    /// Bug note 2026-11-04T02-17-22: a diarization frame with no speaker events must not re-read `speakers.json` and
    /// resend the list while the file is unchanged; a save (by anyone) is picked up on the next frame.
    #[test]
    fn frames_without_events_resend_the_list_only_after_the_store_changed() {
        let (_t, mut a) = app();
        let path = a.data.join("speakers.json");
        let mut store = EnrollmentStore::default();
        store.enroll("Dave", voice()).unwrap();
        store.save(&path).unwrap();
        let sent = |a: &Pipeline| a.hooks.ui.lock().unwrap().iter().filter(|u| matches!(u, ui::UiUpdate::Speakers(_))).count();
        a.speaker_events(vec![]).unwrap();
        assert_eq!(sent(&a), 1, "the first frame sends the list");
        for _ in 0..5 {
            a.speaker_events(vec![]).unwrap();
        }
        assert_eq!(sent(&a), 1, "unchanged store: no reparse, no resend");
        store.enroll("Vim", voice()).unwrap();
        store.save(&path).unwrap();
        a.speaker_events(vec![]).unwrap();
        assert_eq!(sent(&a), 2, "a new save is sent on the next frame");
        assert_eq!(speakers(&a).len(), 2);
        a.speaker_events(vec![]).unwrap();
        assert_eq!(sent(&a), 2);
        // Measured: a ~800 KB store (the user's is 785 KB) parsed per frame before, one stat per frame now.
        for i in 0..185 {
            store.enroll(&format!("Guest {i}"), voice()).unwrap();
        }
        store.save(&path).unwrap();
        let size = std::fs::metadata(&path).unwrap().len();
        let t = std::time::Instant::now();
        for _ in 0..20 {
            a.load_store().unwrap();
        }
        let parse = t.elapsed() / 20;
        a.speaker_events(vec![]).unwrap();
        let t = std::time::Instant::now();
        for _ in 0..20 {
            a.speaker_events(vec![]).unwrap();
        }
        eprintln!("speakers.json {} KB: parse {parse:?} per frame before, {:?} per frame now", size >> 10, t.elapsed() / 20);
    }
