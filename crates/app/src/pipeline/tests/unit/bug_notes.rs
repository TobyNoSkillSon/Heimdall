    use super::*;
    const S: u64 = 1_000_000_000;
    /// "completely remove this voice trigger for bugnotes". Whatever he says, on every detection path
    /// (partial, window decode, final), nothing starts a note: no command candidate or cue, nothing in the inbox, and
    /// listening never pauses; the words stay ordinary speech.
    #[test]
    fn saying_bug_note_starts_nothing_on_any_path() {
        for text in ["bug note the menu froze", "Bug note.", "bugnote he ignored me", "bug notes are piling up",
            "But note, it failed.", "Heimdall bug again", "zgłoś błąd menu się zawiesiło", "zgłoś bóląd", "okay bug note"] {
            let (_t, mut a, source) = note_rig();
            a.audio.insert(source.clone(), AudioRing { start: 0, samples: vec![0.01; 16000 * 30].into() });
            let words = timed(text, 10 * S);
            a.listen(ListenEvent::SpeechStart { source: source.clone(), host_time_ns: 10 * S }).unwrap();
            a.listen(ListenEvent::TranscriptSegment { source: source.clone(), text: text.into(), start_ns: 10 * S,
                end_ns: words.last().unwrap().end_ns, words: words.clone() }).unwrap();
            say(&mut a, &source, text, 10 * S, false);
            let records = a.index.evidence.records().unwrap();
            assert!(!records.iter().any(|r| r.kind == "command_decision" || r.kind == "bug_note"), "{text}");
            assert!(a.command_candidates.is_empty(), "{text}");
            assert!(!a.hooks.ui.lock().unwrap().iter().any(|u| matches!(u, ui::UiUpdate::Command(_))), "{text}");
            assert!(!a.bugs.inbox.exists(), "{text}: nothing was written");
            assert!(a.bugs.listed.is_empty());
            let rows = a.index.utterances_between(0, 60 * S).unwrap();
            assert_eq!(rows.len(), 1, "{text}");
            assert_ne!(rows[0]["decision"]["status"], "bug_note", "{text}");
            assert!(rows[0]["text"].as_str().unwrap().contains(text.trim_end_matches('.')), "{text}: the words stay in the transcript");
            assert_eq!(a.errors, 0, "{text}");
        }
    }
    /// The menu's count is the inbox's: notes filed by the tool show up, and notes the night round resolved stop
    /// counting the next time the menu opens.
    #[test]
    fn menu_count_follows_the_inbox_when_the_menu_opens() {
        let (_t, mut a, _source) = note_rig();
        let inbox = a.bugs.inbox.clone();
        assert!(inbox.starts_with(&a.data), "tests write inside their own data dir");
        a.action(ui::UiAction::RefreshApiCatalog).unwrap();
        assert!(!a.hooks.ui.lock().unwrap().iter().any(|u| matches!(u, ui::UiUpdate::BugNotes(_))), "no change, no update");
        let first = heimdall_night::bug_notes::write(&inbox, "t3_workers result was a wall of JSON", None, agent::now_ms()).unwrap();
        heimdall_night::bug_notes::write(&inbox, "island stuck on Working", None, agent::now_ms()).unwrap();
        a.action(ui::UiAction::RefreshApiCatalog).unwrap();
        let last = |a: &Pipeline| a.hooks.ui.lock().unwrap().iter().rev().find_map(|u| match u { ui::UiUpdate::BugNotes(n) => Some(n.clone()), _ => None });
        assert_eq!(last(&a).unwrap().len(), 2);
        assert_eq!(ui::bug_notes_title(a.bugs.open()), "Bug Notes (2)");
        heimdall_night::bug_notes::resolve(&inbox, first["id"].as_str().unwrap(), "Fixed.", agent::now_ms()).unwrap();
        a.action(ui::UiAction::RefreshApiCatalog).unwrap();
        // The resolved note stays as a fixed card for a few days; the count is the open ones.
        let listed = last(&a).unwrap();
        assert_eq!(listed.iter().filter(|n| n.is_open()).count(), 1);
        assert_eq!(a.bugs.open(), 1);
        assert_eq!(a.bugs.listed[0].text, "island stuck on Working");
        assert_eq!(listed[1].status, ui::BugStatus::Fixed(None));
    }
