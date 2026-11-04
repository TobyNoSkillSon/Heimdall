    use super::*;

    use CommandKind::*;

    fn heard(id: u64, kind: CommandKind) -> CommandCue {
        CommandCue::Heard { id, kind }
    }
    fn done(id: u64, kind: CommandKind) -> CommandCue {
        CommandCue::Done { id: Some(id), kind, label: kind.done_label().into() }
    }

    #[test]
    fn heard_grows_then_rests_as_a_glyph_until_done_then_retracts_to_nothing() {
        let mut i = Cues::default();
        assert_eq!(i.cue(heard(1, LookAtThis), 0.0), Look::Heard { kind: LookAtThis, count: 1 });
        assert_eq!(i.deadline(), Some(HEARD_SECONDS));
        assert_eq!(i.tick(HEARD_SECONDS), Look::Pending { kind: LookAtThis, count: 1 });
        // At rest with a pending command: no deadline, so no timer runs.
        assert_eq!(i.deadline(), None);
        // Decided minutes later.
        let look = i.cue(done(1, LookAtThis), 300.0);
        assert_eq!(look, Look::Result { kind: LookAtThis, label: "Screenshot".into(), tone: Tone::Done, count: 1 });
        assert_eq!(i.tick(300.0 + DONE_SECONDS), Look::Hidden);
        assert_eq!(i.deadline(), None);
    }

    #[test]
    fn rejected_command_leaves_silently_from_glyph_and_from_heard() {
        let mut i = Cues::default();
        i.cue(heard(1, Screenshot), 0.0);
        i.tick(5.0);
        assert_eq!(i.cue(CommandCue::Dropped { id: 1 }, 6.0), Look::Hidden);
        i.cue(heard(2, Cancel), 7.0);
        assert_eq!(i.cue(CommandCue::Dropped { id: 2 }, 7.2), Look::Hidden);
        assert_eq!(i.deadline(), None);
    }

    #[test]
    fn several_pending_commands_share_one_glyph_with_a_count() {
        let mut i = Cues::default();
        i.cue(heard(1, LookAtThis), 0.0);
        i.cue(heard(2, Screenshot), 0.5);
        i.cue(heard(3, Cancel), 0.8);
        // A repeated detection of the same command (partial, then window, then final) is not a new one.
        i.cue(heard(3, Cancel), 0.9);
        // The glyph keeps the first command's icon; the count says how many.
        assert_eq!(i.tick(10.0), Look::Pending { kind: LookAtThis, count: 3 });
        i.cue(done(3, Cancel), 11.0);
        assert_eq!(i.tick(11.0 + DONE_SECONDS), Look::Pending { kind: LookAtThis, count: 2 });
        i.cue(CommandCue::Dropped { id: 2 }, 12.0);
        assert_eq!(i.look(), Look::Pending { kind: LookAtThis, count: 1 });
    }

    #[test]
    fn heard_during_a_result_keeps_the_result_then_shows_the_glyph() {
        let mut i = Cues::default();
        i.cue(done(9, Cancel), 0.0);
        let look = i.cue(heard(10, LookAtThis), 0.5);
        assert_eq!(look, Look::Result { kind: Cancel, label: "Cancelled".into(), tone: Tone::Done, count: 1 });
        assert_eq!(i.tick(DONE_SECONDS), Look::Pending { kind: LookAtThis, count: 1 });
    }

    #[test]
    fn failure_is_orange_with_its_reason_and_stays_longer() {
        let mut i = Cues::default();
        i.cue(heard(1, LookAtThis), 0.0);
        let look = i.cue(CommandCue::Failed { id: Some(1), kind: LookAtThis, reason: "Seeing is off".into() }, 2.0);
        assert_eq!(look, Look::Result { kind: LookAtThis, label: "Seeing is off".into(), tone: Tone::Failed, count: 1 });
        assert_eq!(i.tick(2.0 + DONE_SECONDS), look);
        assert_eq!(i.tick(2.0 + FAILED_SECONDS), Look::Hidden);
    }

    #[test]
    fn outcome_without_detection_and_clear_on_listening_off() {
        let mut i = Cues::default();
        i.cue(heard(1, Screenshot), 0.0);
        let undetected = CommandCue::Done { id: None, kind: Cancel, label: "Cancelled".into() };
        i.cue(undetected, 1.0);
        assert_eq!(i.tick(1.0 + DONE_SECONDS), Look::Pending { kind: Screenshot, count: 1 });
        i.clear(100.0);
        assert_eq!(i.look(), Look::Hidden);
        assert_eq!(i.deadline(), None);
    }

    fn restarting(key: &str, label: &str) -> HealthCue {
        HealthCue::Restarting { key: key.into(), part: Part::Transcription, label: label.into() }
    }
    fn problem(key: &str, part: Part, label: &str, action: &str, control: HealthControl) -> HealthCue {
        HealthCue::Problem { key: key.into(), part, label: label.into(), action: action.into(), control }
    }

    /// Bug notes 26 Oct 00:52:29, 00:54:39: the island says what a failing wake is doing: when it retries, that it
    /// retries, and that Heimdall gave up; a later answer clears it.
    #[test]
    fn a_retrying_wake_says_when_it_retries_and_when_it_gave_up() {
        let mut i = Cues::default();
        let retry = |label: &str| HealthCue::Restarting { key: "agent-retry".into(), part: Part::Agent, label: label.into() };
        assert_eq!(i.health(retry("Model not answering · retrying in 15 s"), 0.0), status(RESTART_SYMBOL, "Model not answering · retrying in 15 s", None, HealthTone::Restarting, true));
        assert_eq!(i.tick(ARRIVAL_SECONDS), status(RESTART_SYMBOL, "Model not answering · retrying in 15 s", None, HealthTone::Restarting, false));
        assert_eq!(i.health(retry("Model not answering · retrying now"), 15.0), status(RESTART_SYMBOL, "Model not answering · retrying now", None, HealthTone::Restarting, true));
        let gave_up = i.health(problem("agent-retry", Part::Agent, "Model not answering · gave up", "Say it again", HealthControl::OpenAgent), 60.0);
        assert!(matches!(&gave_up, Look::Status { tone: HealthTone::Problem, label, detail: Some(d), .. } if label == "Model not answering · gave up" && d == "Say it again"), "{gave_up:?}");
        assert!(i.alerting());
        i.health(HealthCue::Clear { key: "agent-retry".into() }, 90.0);
        assert!(!i.alerting());
        assert_eq!(i.tick(90.0 + ARRIVAL_SECONDS), Look::Hidden);
    }

    #[test]
    fn restart_is_orange_until_ok_then_simply_leaves() {
        let mut i = Cues::default();
        let look = i.health(restarting("listen", "Restarting transcription…"), 0.0);
        assert_eq!(look, status(RESTART_SYMBOL, "Restarting transcription…", None, HealthTone::Restarting, true));
        // The arrival pulse ends; the orange pill stays, static, with no timer.
        assert_eq!(i.tick(ARRIVAL_SECONDS), status(RESTART_SYMBOL, "Restarting transcription…", None, HealthTone::Restarting, false));
        assert_eq!(i.deadline(), None);
        assert!(!i.alerting());
        // Back: the orange pill leaves, no "OK" after it (no success after everything that happened).
        let ok = HealthCue::Recovered { key: "listen".into(), part: Part::Transcription, label: "Transcription OK".into() };
        assert_eq!(i.health(ok, 9.0), Look::Hidden);
        assert_eq!(i.deadline(), None);
        assert!(i.take_reports().iter().all(|r| !matches!(r, IslandReport::Shown { label, .. } if label == "Transcription OK")));
    }

    #[test]
    fn restart_that_gives_up_turns_red_until_fixed_and_a_click_collapses_it() {
        let mut i = Cues::default();
        i.health(restarting("listen", "Restarting transcription…"), 0.0);
        let red = i.health(problem("listen", Part::Transcription, "Transcription stopped", "Restart Heimdall", HealthControl::Relaunch), 5.0);
        assert!(matches!(&red, Look::Status { tone: HealthTone::Problem, pulse: true, detail: Some(d), .. } if d == "Restart Heimdall"));
        assert!(i.alerting());
        assert_eq!(i.tick(5.0 + ARRIVAL_SECONDS), status("waveform.slash", "Transcription stopped", Some("Restart Heimdall"), HealthTone::Problem, false));
        // Persistent: an hour later it is still there, with no timer running.
        assert_eq!(i.deadline(), None);
        assert_eq!(i.control(), Some(HealthControl::Relaunch));
        assert_eq!(i.acknowledge(6.0), Some(("listen".into(), HealthControl::Relaunch)));
        assert_eq!(i.look(), Look::Alert { symbol: "waveform.slash", tone: HealthTone::Problem });
        assert_eq!(i.control(), None);
        assert!(i.alerting(), "the menu-bar helmet stays red until the problem ends");
        // The same report again does not reopen the pill.
        i.health(problem("listen", Part::Transcription, "Transcription stopped", "Restart Heimdall", HealthControl::Relaunch), 60.0);
        assert_eq!(i.look(), Look::Alert { symbol: "waveform.slash", tone: HealthTone::Problem });
        i.health(HealthCue::Clear { key: "listen".into() }, 61.0);
        assert_eq!(i.look(), Look::Hidden);
        assert!(!i.alerting());
    }

    #[test]
    fn a_drop_says_itself_once_then_rests_as_a_glyph_until_it_returns() {
        let mut i = Cues::default();
        let drop = HealthCue::Drop { key: "mic".into(), part: Part::Microphone, label: "AirPods Pro disconnected".into() };
        assert!(matches!(i.health(drop.clone(), 0.0), Look::Status { tone: HealthTone::Drop, symbol: "mic.slash", .. }));
        assert_eq!(i.tick(DROP_SECONDS), Look::Alert { symbol: "mic.slash", tone: HealthTone::Drop });
        // Repeated reports change nothing.
        assert_eq!(i.health(drop, 10.0), Look::Alert { symbol: "mic.slash", tone: HealthTone::Drop });
        assert_eq!(i.deadline(), None);
        assert!(!i.alerting());
        let back = HealthCue::Recovered { key: "mic".into(), part: Part::Microphone, label: "Microphone OK".into() };
        assert_eq!(i.health(back, 20.0), Look::Hidden, "back: the glyph simply leaves");
    }

    #[test]
    fn commands_keep_working_beside_health_and_listening_off_keeps_health() {
        let mut i = Cues::default();
        i.health(problem("screen", Part::Screen, "Screen permission off", "Open Settings", HealthControl::OpenSettings(crate::model::Permission::ScreenRecording)), 0.0);
        i.tick(1.0);
        // A spoken command still grows out of the notch, and its result shows; then the red pill returns, without a
        // second arrival pulse.
        assert_eq!(i.cue(heard(1, LookAtThis), 2.0), Look::Heard { kind: LookAtThis, count: 1 });
        assert!(matches!(i.tick(2.0 + HEARD_SECONDS), Look::Status { tone: HealthTone::Problem, pulse: false, .. }));
        assert!(matches!(i.cue(done(1, LookAtThis), 3.0), Look::Result { .. }));
        assert!(matches!(i.tick(3.0 + DONE_SECONDS), Look::Status { tone: HealthTone::Problem, pulse: false, .. }));
        i.clear(100.0);
        assert!(matches!(i.look(), Look::Status { tone: HealthTone::Problem, .. }));
        assert!(i.alerting());
    }

    #[test]
    fn notice_is_brief_and_a_restart_outranks_a_problem_at_rest() {
        let mut i = Cues::default();
        let switched = HealthCue::Notice { part: Part::Microphone, label: "AirPods Pro → MacBook Pro Microphone".into() };
        assert!(matches!(i.health(switched, 0.0), Look::Status { tone: HealthTone::Notice, symbol: "mic", .. }));
        assert_eq!(i.tick(NOTICE_SECONDS), Look::Hidden);
        i.health(problem("agent", Part::Agent, "Agent delivery needs review", "Click to open the Agent window", HealthControl::OpenAgent), 10.0);
        i.health(restarting("listen", "Restarting transcription…"), 11.0);
        assert!(matches!(i.tick(12.0), Look::Status { tone: HealthTone::Restarting, .. }));
        i.health(HealthCue::Clear { key: "listen".into() }, 13.0);
        assert!(matches!(i.look(), Look::Status { tone: HealthTone::Problem, .. }));
        assert_eq!(i.control(), Some(HealthControl::OpenAgent));
    }

    #[test]
    fn lost_outcomes_cannot_grow_the_pending_list_without_bound() {
        let mut i = Cues::default();
        for id in 0..1000 {
            i.cue(heard(id, Other), id as f64);
        }
        assert_eq!(i.tick(2000.0), Look::Pending { kind: Other, count: MAX_PENDING });
    }

    fn notice(label: &str) -> HealthCue {
        HealthCue::Notice { part: Part::Microphone, label: label.into() }
    }
    /// What the pill says (a result with its count); leaked: tests only.
    fn shown_label(look: &Look) -> Option<&'static str> {
        match look {
            Look::Status { label, .. } => Some(Box::leak(label.clone().into_boxed_str())),
            Look::Result { label, count, .. } => Some(Box::leak(result_text(label, *count).into_boxed_str())),
            _ => None,
        }
    }

    /// Before: a notice that arrived while a command result showed was dropped, never seen.
    #[test]
    fn a_notice_during_a_result_waits_its_turn() {
        let mut i = Cues::default();
        i.cue(CommandCue::Done { id: None, kind: Listening, label: "Listening".into() }, 0.0);
        let look = i.health(notice("AirPods Pro → MacBook Pro Microphone"), 0.4);
        assert_eq!(shown_label(&look), Some("Listening"), "the result keeps its time");
        let look = i.tick(DONE_SECONDS);
        assert_eq!(shown_label(&look), Some("AirPods Pro → MacBook Pro Microphone"));
        assert_eq!(i.deadline(), Some(DONE_SECONDS + NOTICE_SECONDS), "shown for its full time from when it showed");
        assert_eq!(i.tick(DONE_SECONDS + NOTICE_SECONDS), Look::Hidden);
    }

    #[test]
    fn a_second_moment_waits_until_the_first_is_readable_unless_it_is_about_the_same_condition() {
        let mut i = Cues::default();
        i.health(notice("Agent reloaded"), 0.0);
        let look = i.health(notice("AirPods Pro → MacBook Pro Microphone"), 0.3);
        assert_eq!(shown_label(&look), Some("Agent reloaded"));
        assert_eq!(shown_label(&i.tick(NOTICE_SECONDS)), Some("AirPods Pro → MacBook Pro Microphone"));
        // After the minimum read time a newer moment replaces it at once.
        let look = i.health(notice("Third"), NOTICE_SECONDS + MIN_READ_SECONDS + 0.1);
        assert_eq!(shown_label(&look), Some("Third"));
        // A restart that recovers at once: its own arrival leaves with it (cut, reported), nothing replaces it.
        let mut i = Cues::default();
        i.health(restarting("listen", "Restarting transcription…"), 0.0);
        let ok = HealthCue::Recovered { key: "listen".into(), part: Part::Transcription, label: "Transcription OK".into() };
        assert_eq!(shown_label(&i.health(ok, 0.3)), None);
        assert!(i.take_reports().iter().any(|r| matches!(r, IslandReport::Cut { why: "condition ended", .. })));
    }

    #[test]
    fn a_command_takes_the_island_from_a_fresh_notice_which_then_returns() {
        let mut i = Cues::default();
        i.health(notice("AirPods Pro → MacBook Pro Microphone"), 0.0);
        assert_eq!(i.cue(heard(1, LookAtThis), 0.4), Look::Heard { kind: LookAtThis, count: 1 });
        let look = i.tick(0.4 + HEARD_SECONDS);
        assert_eq!(shown_label(&look), Some("AirPods Pro → MacBook Pro Microphone"));
        // A notice already read for MIN_READ_SECONDS does not come back.
        let mut i = Cues::default();
        i.health(notice("Agent reloaded"), 0.0);
        i.cue(heard(1, LookAtThis), MIN_READ_SECONDS + 0.5);
        assert_eq!(i.tick(MIN_READ_SECONDS + 0.5 + HEARD_SECONDS), Look::Pending { kind: LookAtThis, count: 1 });
    }

    #[test]
    fn a_queued_arrival_about_a_condition_that_ended_is_dropped() {
        let mut i = Cues::default();
        i.cue(done(1, Screenshot), 0.0);
        i.health(restarting("listen", "Restarting transcription…"), 0.2);
        i.health(HealthCue::Clear { key: "listen".into() }, 0.5);
        assert_eq!(i.tick(DONE_SECONDS), Look::Hidden);
    }

    /// Release review 2: two problems 0.1 s apart; the second waits behind the first's arrival. A click on the
    /// pill on screen must act on that pill's problem, not the newest one.
    #[test]
    fn a_click_acts_on_the_problem_on_screen_not_the_newest() {
        use crate::model::Permission;
        let mut i = Cues::default();
        i.health(problem("screen", Part::Screen, "Screen Recording permission is off", "Click to open Settings", HealthControl::OpenSettings(Permission::ScreenRecording)), 0.0);
        let look = i.health(problem("archive", Part::Archive, "Archive database is damaged", "Click to show it in Finder", HealthControl::ShowArchive), 0.1);
        assert_eq!(shown_label(&look), Some("Screen Recording permission is off"));
        assert_eq!(i.control(), Some(HealthControl::OpenSettings(Permission::ScreenRecording)));
        assert_eq!(i.acknowledge(0.2), Some(("screen".into(), HealthControl::OpenSettings(Permission::ScreenRecording))));
        // The archive problem's own arrival follows, and its click is its own.
        assert_eq!(shown_label(&i.look()), Some("Archive database is damaged"));
        assert_eq!(i.control(), Some(HealthControl::ShowArchive));
        assert_eq!(i.acknowledge(0.3), Some(("archive".into(), HealthControl::ShowArchive)));
        assert_eq!(i.control(), None);
        // At rest (no moment showing) the selected problem is the one clicked.
        let mut j = Cues::default();
        j.health(problem("a", Part::Agent, "A", "do a", HealthControl::OpenAgent), 0.0);
        j.health(problem("b", Part::Archive, "B", "do b", HealthControl::ShowArchive), 0.1);
        j.tick(10.0);
        assert_eq!(shown_label(&j.look()), Some("B"));
        assert_eq!(j.acknowledge(10.0), Some(("b".into(), HealthControl::ShowArchive)));
        // While a command covers the pill there is nothing to click.
        j.cue(heard(1, LookAtThis), 11.0);
        assert_eq!(j.control(), None);
        assert_eq!(j.acknowledge(11.1), None);
        // A click landing after the moment is overdue but before the redraw acts on what is still on screen.
        let mut k = Cues::default();
        k.health(problem("screen", Part::Screen, "Screen Recording permission is off", "Click to open Settings", HealthControl::OpenSettings(Permission::ScreenRecording)), 0.0);
        k.health(problem("archive", Part::Archive, "Archive database is damaged", "Click to show it in Finder", HealthControl::ShowArchive), 0.1);
        assert_eq!(k.acknowledge(0.81), Some(("screen".into(), HealthControl::OpenSettings(Permission::ScreenRecording))));
    }

    /// The user's live test, 16 Oct (seconds after 10:30:00): "look at the screen" at :03, "take a screenshot" at :06 and
    /// :12, decided at the turn's end; the three results reached the island at :14.482, :15.094 and :15.763 and each
    /// replaced the last, so he saw one ("it only showed it like for the last one … kind of annoying").
    #[test]
    fn the_same_result_again_counts_on_as_it_arrives() {
        let mut i = Cues::default();
        i.cue(heard(1, LookAtThis), 3.0);
        i.cue(heard(2, Screenshot), 6.0);
        i.cue(heard(3, Screenshot), 12.0);
        assert_eq!(i.tick(13.5), Look::Pending { kind: LookAtThis, count: 3 });
        // The heard glyph turns into its result; each later one changes only the count, as it arrives.
        assert_eq!(shown_label(&i.cue(done(1, LookAtThis), 14.482)), Some("Screenshot"));
        assert_eq!(i.cue(done(2, Screenshot), 15.094), Look::Result { kind: LookAtThis, label: "Screenshot".into(), tone: Tone::Done, count: 2 });
        assert_eq!(shown_label(&i.cue(done(3, Screenshot), 15.763)), Some("Screenshot ×3"));
        // The last count keeps the full time, then the island rests: nothing pending is left behind.
        assert_eq!(i.deadline(), Some(15.763 + DONE_SECONDS));
        assert_eq!(i.tick(15.763 + DONE_SECONDS), Look::Hidden);
        assert_eq!(i.deadline(), None);
        // A count that arrives sooner than the label can be read waits for it.
        let mut i = Cues::default();
        i.cue(done(1, Screenshot), 0.0);
        assert_eq!(shown_label(&i.cue(done(2, Screenshot), 0.2)), Some("Screenshot"));
        assert_eq!(i.deadline(), Some(FIRST_COUNT_SECONDS));
        assert_eq!(shown_label(&i.tick(FIRST_COUNT_SECONDS)), Some("Screenshot ×2"));
        assert_eq!(shown_label(&i.cue(done(3, Screenshot), FIRST_COUNT_SECONDS + 0.1)), Some("Screenshot ×2"));
        assert_eq!(shown_label(&i.tick(FIRST_COUNT_SECONDS + COUNT_SECONDS)), Some("Screenshot ×3"));
    }

    #[test]
    fn a_burst_folds_into_the_next_count_which_says_the_total() {
        let mut i = Cues::default();
        let mut shown: Vec<&str> = vec![];
        let mut note = |l: Option<&'static str>| if let Some(l) = l { if shown.last() != Some(&l) { shown.push(l) } };
        for k in 0..12u64 {
            note(shown_label(&i.cue(done(k, Screenshot), k as f64 * 0.1)));
        }
        let mut t = 0.0;
        while let Some(d) = i.deadline() {
            t = d;
            note(shown_label(&i.tick(d)));
        }
        assert_eq!(shown, ["Screenshot", "Screenshot ×6", "Screenshot ×11", "Screenshot ×12"]);
        assert!((t - (1.1 + COUNT_SECONDS + DONE_SECONDS)).abs() < 1e-9, "gone at {t}");
        // Different results take turns; past three waiting, a new one joins the newest, which names each part.
        let mut j = Cues::default();
        j.cue(done(1, Screenshot), 0.0);
        j.cue(done(2, Cancel), 0.1);
        j.cue(CommandCue::Failed { id: Some(3), kind: LookAtThis, reason: "Seeing is off".into() }, 0.2);
        j.cue(done(4, Other), 0.3);
        j.cue(done(5, Listening), 0.4);
        j.cue(CommandCue::Failed { id: Some(6), kind: Cancel, reason: "Agent is off".into() }, 0.45);
        j.cue(done(7, Listening), 0.5);
        let mut shown = vec![];
        while let Some(d) = j.deadline() {
            if let Look::Result { label, tone, count, .. } = j.tick(d) {
                if shown.last() != Some(&(result_text(&label, count), tone)) {
                    shown.push((result_text(&label, count), tone));
                }
            }
        }
        assert_eq!(shown, [
            ("Cancelled".to_string(), Tone::Done),
            ("Seeing is off".into(), Tone::Failed),
            ("Done · Cancel: Agent is off · +2 more".into(), Tone::Failed),
        ]);
    }

    #[test]
    fn a_result_during_the_last_ones_retraction_counts_on_and_a_later_one_starts_again() {
        let mut i = Cues::default();
        i.cue(done(1, Screenshot), 0.0);
        assert_eq!(i.tick(DONE_SECONDS), Look::Hidden);
        assert_eq!(shown_label(&i.cue(done(2, Screenshot), DONE_SECONDS + 0.2)), Some("Screenshot ×2"));
        assert_eq!(i.tick(2.0 * DONE_SECONDS + 0.2), Look::Hidden);
        assert_eq!(shown_label(&i.cue(done(3, Screenshot), 30.0)), Some("Screenshot"));
        // A different result in between breaks the count.
        i.cue(done(4, Cancel), 30.5);
        assert_eq!(shown_label(&i.tick(30.0 + QUEUED_DONE_SECONDS)), Some("Cancelled"));
        assert_eq!(shown_label(&i.tick(30.0 + QUEUED_DONE_SECONDS + DONE_SECONDS)), None);
        assert_eq!(shown_label(&i.cue(done(5, Screenshot), 30.0 + QUEUED_DONE_SECONDS + DONE_SECONDS + 0.1)), Some("Screenshot"));
    }

    #[test]
    fn a_restart_goes_right_after_the_result_and_notices_after_every_result() {
        // Heard, then its own result: one pill, the glyph becomes the result, nothing waits.
        let mut i = Cues::default();
        i.cue(heard(1, Screenshot), 0.0);
        assert_eq!(shown_label(&i.cue(done(1, Screenshot), 0.3)), Some("Screenshot"));
        assert_eq!(i.deadline(), Some(0.3 + DONE_SECONDS));
        // A notice waits behind every result; a restart goes right after the result on screen.
        let mut i = Cues::default();
        i.cue(done(1, Screenshot), 0.0);
        i.health(notice("AirPods Pro → MacBook Pro Microphone"), 0.2);
        i.cue(CommandCue::Failed { id: Some(2), kind: Cancel, reason: "Agent is off".into() }, 0.4);
        i.health(restarting("listen", "Restarting transcription…"), 0.5);
        assert_eq!(shown_label(&i.tick(QUEUED_DONE_SECONDS)), Some("Restarting transcription…"));
        let after = QUEUED_DONE_SECONDS + ARRIVAL_SECONDS;
        assert_eq!(shown_label(&i.tick(after)), Some("Agent is off"));
        assert_eq!(i.deadline(), Some(after + FAILED_SECONDS), "the last result keeps its full time");
        assert_eq!(shown_label(&i.tick(after + FAILED_SECONDS)), Some("AirPods Pro → MacBook Pro Microphone"));
        // Listening off drops what waits.
        let mut i = Cues::default();
        i.cue(done(1, Screenshot), 0.0);
        i.cue(done(2, Cancel), 0.2);
        i.clear(0.5);
        assert_eq!(i.look(), Look::Hidden);
        assert_eq!(i.deadline(), None);
    }

    fn shown_ids(reports: &[IslandReport]) -> Vec<(Option<u64>, &'static str, u64)> {
        reports.iter().filter_map(|r| match r { IslandReport::Shown { id, phase, waited_ms, .. } => Some((*id, *phase, *waited_ms)), _ => None }).collect()
    }

    /// The 16 Oct burst as reports: each cue shown once, with how long it waited for the island.
    #[test]
    fn reports_say_when_each_cue_took_the_island_and_how_long_it_waited() {
        let mut i = Cues::default();
        i.cue(heard(1, LookAtThis), 3.0);
        i.cue(heard(2, Screenshot), 6.0);
        i.cue(done(1, LookAtThis), 14.482);
        i.cue(done(2, Screenshot), 15.094);
        i.cue(done(3, Screenshot), 15.763);
        while let Some(d) = i.deadline() {
            i.tick(d);
        }
        let reports = i.take_reports();
        assert!(reports.iter().all(|r| matches!(r, IslandReport::Shown { .. })), "{reports:#?}");
        assert_eq!(shown_ids(&reports), [(Some(1), "heard", 0), (Some(2), "heard", 0), (Some(1), "done", 0), (Some(2), "done", 0), (Some(3), "done", 0)]);
        assert!(i.take_reports().is_empty(), "transitions only: taken once");
        // A count that waits for the label to be read reports its wait; so does a heard cue under a result.
        let mut i = Cues::default();
        i.cue(done(1, Cancel), 0.0);
        i.cue(heard(2, LookAtThis), 0.5);
        i.cue(done(3, Cancel), 0.2);
        assert_eq!(shown_ids(&i.take_reports()), [(Some(1), "done", 0)]);
        i.tick(FIRST_COUNT_SECONDS);
        assert_eq!(shown_ids(&i.take_reports()), [(Some(3), "done", 400)]);
        i.tick(FIRST_COUNT_SECONDS + DONE_SECONDS);
        assert_eq!(shown_ids(&i.take_reports()), [(Some(2), "heard", 1800)]);
    }

    #[test]
    fn reports_say_which_cues_were_cut_before_they_were_readable() {
        // Listening off 0.4 s into a result, with its count not yet said and a different one waiting.
        let mut i = Cues::default();
        i.cue(done(1, Screenshot), 0.0);
        i.cue(done(2, Screenshot), 0.2);
        i.cue(done(3, Cancel), 0.3);
        i.take_reports();
        i.clear(0.4);
        let cuts: Vec<_> = i.take_reports().into_iter().filter_map(|r| match r { IslandReport::Cut { id, shown_ms, why, .. } => Some((id, shown_ms, why)), _ => None }).collect();
        assert_eq!(cuts, [(Some(1), 400, "listening off"), (Some(2), 0, "listening off before its count"), (Some(3), 0, "listening off before its turn")]);
        // Nine notices behind a result: the oldest falls out of the health queue, and says so.
        let mut i = Cues::default();
        i.cue(done(1, Screenshot), 0.0);
        for k in 0..=MAX_QUEUED {
            i.health(notice(&format!("notice {k}")), 0.1);
        }
        let cut: Vec<_> = i.take_reports().into_iter().filter(|r| matches!(r, IslandReport::Cut { .. })).collect();
        assert!(matches!(&cut[..], [IslandReport::Cut { label, why: "health queue full", .. }] if label == "notice 0"), "{cut:#?}");
        // A long burst keeps every count but a bounded list of cues; the rest say so at once.
        let mut i = Cues::default();
        for k in 0..40u64 {
            i.cue(done(k, Screenshot), k as f64 * 0.01);
        }
        let folded = i.take_reports().into_iter().filter(|r| matches!(r, IslandReport::Cut { why: "counted in a burst, not reported on its own", .. })).count();
        assert_eq!(folded, 40 - MAX_CUES);
        assert_eq!(shown_label(&i.tick(FIRST_COUNT_SECONDS)), Some("Screenshot ×40"));
        // A cue the UI never gave the island (Listening off) is reported; a drop is not a cut.
        assert!(matches!(IslandReport::not_taken(&done(4, Screenshot), "listening off"), Some(IslandReport::Cut { id: Some(4), phase: "done", .. })));
        assert_eq!(IslandReport::not_taken(&CommandCue::Dropped { id: 4 }, "listening off"), None);
    }

    /// Round-2 review: screenshots every 0.7 s kept counting on the result on screen, each count extending its time,
    /// and a restart queued behind it stayed hidden for 60 s.
    #[test]
    fn a_restart_is_not_starved_by_a_result_that_keeps_counting() {
        let mut i = Cues::default();
        i.cue(done(0, Screenshot), 0.0);
        i.health(restarting("listen", "Restarting transcription…"), 0.3);
        let mut restart_at = None;
        for k in 1..86u64 {
            let t = k as f64 * 0.7;
            while let Some(d) = i.deadline().filter(|d| *d <= t) {
                i.tick(d);
            }
            if restart_at.is_none() && shown_label(&i.look()) == Some("Restarting transcription…") {
                restart_at = Some(t);
            }
            i.cue(done(k, Screenshot), t);
        }
        let at = restart_at.expect("the restart never showed");
        assert!(at <= 0.3 + QUEUED_DONE_SECONDS + COUNT_SECONDS + 0.7, "restart shown at {at}");
        // Results after it still count among themselves.
        let mut later = vec![];
        while let Some(d) = i.deadline() {
            if let Some(l) = shown_label(&i.tick(d)) {
                later.push(l);
            }
        }
        assert!(later.iter().all(|l| l.starts_with("Screenshot") || l.starts_with("Restarting")), "{later:?}");
    }

    /// Round-2 review: heard, a notice behind it, then the heard command dropped: the notice was stranded with no
    /// deadline, so nothing ever showed it.
    #[test]
    fn a_notice_behind_a_dropped_heard_cue_still_shows() {
        let mut i = Cues::default();
        i.cue(heard(1, LookAtThis), 0.0);
        i.health(notice("AirPods Pro → MacBook Pro Microphone"), 0.2);
        assert_eq!(shown_label(&i.cue(CommandCue::Dropped { id: 1 }, 0.4)), Some("AirPods Pro → MacBook Pro Microphone"));
        assert_eq!(i.deadline(), Some(0.4 + NOTICE_SECONDS));
    }

    /// Round-2 review: 1,000 distinct failures joined into one waiting result made a 24,946-byte label.
    #[test]
    fn joined_results_keep_a_short_label() {
        let mut i = Cues::default();
        i.cue(done(0, Screenshot), 0.0);
        for k in 1..1000u64 {
            i.cue(CommandCue::Failed { id: Some(k), kind: Cancel, reason: format!("reason {k}") }, 0.0);
        }
        let mut labels = vec![];
        while let Some(d) = i.deadline() {
            if let Look::Result { label, .. } = i.tick(d) {
                labels.push(label);
            }
        }
        let joined = labels.last().unwrap();
        assert!(joined.len() <= 120, "{} bytes: {joined}", joined.len());
        assert!(joined.ends_with("+995 more"), "{joined}");
    }

    /// Random interleavings of every cue over simulated time: nothing waits without a deadline, and every result and
    /// notice is shown or reported cut within a bound.
    #[test]
    fn random_interleavings_never_strand_or_lose_a_cue() {
        let mut seed: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut rand = move |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        };
        for round in 0..300 {
            let mut i = Cues::default();
            let mut t = 0.0;
            let mut waiting: Vec<(String, f64)> = vec![];
            let mut next_id = 1u64;
            let mut heard_ids: Vec<u64> = vec![];
            let settle = |i: &mut Cues, upto: f64, waiting: &mut Vec<(String, f64)>| {
                while let Some(d) = i.deadline().filter(|d| *d <= upto) {
                    i.tick(d);
                }
                for r in i.take_reports() {
                    let key = match r {
                        IslandReport::Shown { id: Some(id), phase: "done" | "failed", .. } | IslandReport::Cut { id: Some(id), phase: "done" | "failed", .. } => format!("cue {id}"),
                        IslandReport::Shown { phase: "health", label, .. } | IslandReport::Cut { phase: "health", label, .. } => label,
                        _ => continue,
                    };
                    waiting.retain(|(k, _)| *k != key);
                }
            };
            for _ in 0..60 {
                t += [0.05, 0.2, 0.7, 1.5, 4.0][rand(5) as usize];
                settle(&mut i, t, &mut waiting);
                match rand(9) {
                    0 | 1 => {
                        i.cue(heard(next_id, [Screenshot, LookAtThis, Cancel][rand(3) as usize]), t);
                        heard_ids.push(next_id);
                        next_id += 1;
                    }
                    2 | 3 => {
                        let id = heard_ids.pop().unwrap_or_else(|| { next_id += 1; next_id });
                        i.cue(done(id, [Screenshot, LookAtThis, Cancel, Other][rand(4) as usize]), t);
                        waiting.push((format!("cue {id}"), t));
                    }
                    4 => {
                        let id = heard_ids.pop().unwrap_or_else(|| { next_id += 1; next_id });
                        i.cue(CommandCue::Failed { id: Some(id), kind: Screenshot, reason: "Seeing is off".into() }, t);
                        waiting.push((format!("cue {id}"), t));
                    }
                    5 => {
                        if let Some(id) = heard_ids.pop() {
                            i.cue(CommandCue::Dropped { id }, t);
                        }
                    }
                    6 => {
                        let label = format!("notice {round}.{t:.2}");
                        i.health(notice(&label), t);
                        waiting.push((label, t));
                    }
                    7 => {
                        match rand(3) {
                            0 => i.health(restarting("listen", "Restarting transcription…"), t),
                            1 => i.health(HealthCue::Recovered { key: "listen".into(), part: Part::Transcription, label: "Transcription OK".into() }, t),
                            _ => i.health(HealthCue::Clear { key: "listen".into() }, t),
                        };
                    }
                    _ => {
                        i.clear(t);
                        heard_ids.clear();
                    }
                }
                let queued = !i.queued.is_empty() || !i.results.is_empty();
                assert!(!queued || i.deadline().is_some(), "round {round} at {t}: something waits with no deadline");
                for (what, since) in &waiting {
                    assert!(t - since <= 30.0, "round {round}: {what} waited {:.1} s by {t:.2}", t - since);
                }
            }
            settle(&mut i, t + 120.0, &mut waiting);
            assert!(waiting.is_empty(), "round {round}: never shown or cut: {waiting:?}");
        }
    }


    // ---------------------------------------------------------------- catching up (31 Oct: the archive locked ~4 min)

    fn busy(key: &str, label: &str) -> HealthCue {
        HealthCue::Busy { key: key.into(), part: Part::Agent, label: label.into() }
    }

    /// It arrives once; the app's resends with a falling count change only the words, on screen or held, with no
    /// second arrival; it never turns the helmet red; back, it simply leaves.
    #[test]
    fn catching_up_arrives_once_and_its_words_update_in_place() {
        let mut i = Cues::default();
        let first = "Catching up · 6,489 notes to save";
        assert_eq!(i.health(busy("archive", first), 0.0), status(BUSY_SYMBOL, first, None, HealthTone::Busy, true));
        assert_eq!(i.deadline(), Some(ARRIVAL_SECONDS));
        assert!(!i.alerting());
        // A resend during the arrival: the same moment, its time kept, the new words.
        let second = "Catching up · 5,902 notes to save";
        assert_eq!(i.health(busy("archive", second), 0.4), status(BUSY_SYMBOL, second, None, HealthTone::Busy, true));
        assert_eq!(i.deadline(), Some(ARRIVAL_SECONDS), "no new arrival");
        assert_eq!(i.tick(ARRIVAL_SECONDS), status(BUSY_SYMBOL, second, None, HealthTone::Busy, false));
        assert_eq!(i.deadline(), None, "held with no timer");
        // Resends while held: the words change, nothing arrives again.
        let third = "Catching up · 2,310 notes to save";
        assert_eq!(i.health(busy("archive", third), 2.0), status(BUSY_SYMBOL, third, None, HealthTone::Busy, false));
        assert_eq!(i.on_screen(), None);
        assert_eq!(i.deadline(), None);
        // The same words again change nothing.
        assert_eq!(i.health(busy("archive", third), 4.0), status(BUSY_SYMBOL, third, None, HealthTone::Busy, false));
        assert!(!i.alerting());
        // Back: it leaves, no success mark.
        let back = HealthCue::Recovered { key: "archive".into(), part: Part::Agent, label: "Caught up".into() };
        assert_eq!(i.health(back, 9.0), Look::Hidden);
        assert_eq!(i.on_screen(), None);
        assert_eq!(i.deadline(), None);
    }

    /// `Clear` ends it too, even during its arrival.
    #[test]
    fn catching_up_ends_with_clear() {
        let mut i = Cues::default();
        i.health(busy("archive", "Catching up · 120 notes to save"), 0.0);
        assert_eq!(i.health(HealthCue::Clear { key: "archive".into() }, 0.3), Look::Hidden);
        assert_eq!(i.held(), Look::Hidden);
        assert_eq!(i.deadline(), None);
    }

    /// Below a restart and an unacknowledged problem, above the pending command glyph and a token at rest.
    #[test]
    fn catching_up_yields_to_a_restart_and_a_problem_and_holds_over_a_pending_command() {
        let mut i = Cues::default();
        let words = "Catching up · 6,489 notes to save";
        i.health(busy("archive", words), 0.0);
        i.tick(ARRIVAL_SECONDS);
        // A restart arrives (after its read) and holds over it; back, catching up holds again.
        i.health(restarting("listen", "Restarting transcription"), 1.0);
        i.tick(1.0 + ARRIVAL_SECONDS);
        assert!(matches!(i.held(), Look::Status { tone: HealthTone::Restarting, .. }), "{:?}", i.held());
        // A resend under the restart keeps it under.
        i.health(busy("archive", "Catching up · 4,000 notes to save"), 2.0);
        assert!(matches!(i.held(), Look::Status { tone: HealthTone::Restarting, .. }));
        i.health(HealthCue::Recovered { key: "listen".into(), part: Part::Transcription, label: "Transcription back".into() }, 3.0);
        assert_eq!(i.held(), status(BUSY_SYMBOL, "Catching up · 4,000 notes to save", None, HealthTone::Busy, false));
        // An unacknowledged problem holds over it; acknowledged, catching up shows again (the helmet stays red for
        // the problem alone).
        i.health(problem("screen", Part::Screen, "Screen Recording off", "Open Settings", HealthControl::None), 4.0);
        i.tick(4.0 + ARRIVAL_SECONDS);
        assert!(matches!(i.held(), Look::Status { tone: HealthTone::Problem, .. }));
        assert!(i.acknowledge(6.0).is_some());
        assert!(matches!(i.held(), Look::Status { tone: HealthTone::Busy, .. }), "{:?}", i.held());
        i.health(HealthCue::Clear { key: "screen".into() }, 7.0);
        assert!(!i.alerting());
        // A pending command waits under it.
        i.cue(heard(1, Screenshot), 8.0);
        i.tick(8.0 + HEARD_SECONDS);
        assert!(matches!(i.held(), Look::Status { tone: HealthTone::Busy, .. }), "{:?}", i.held());
        assert_eq!(i.look(), i.held());
        i.cue(CommandCue::Dropped { id: 1 }, 9.0);
    }

    /// Two at once: the newest shows, and a resend of the older one never brings it forward (no flip every resend).
    #[test]
    fn a_resend_keeps_its_place_among_conditions() {
        let mut i = Cues::default();
        i.health(busy("archive", "Catching up · 900 notes to save"), 0.0);
        i.health(busy("index", "Catching up · rebuilding the index"), 2.0);
        i.tick(10.0);
        assert_eq!(i.busy_key(), Some("index"));
        i.health(busy("archive", "Catching up · 800 notes to save"), 11.0);
        assert_eq!(i.busy_key(), Some("index"));
        assert!(matches!(i.held(), Look::Status { label, .. } if label == "Catching up · rebuilding the index"));
        i.health(HealthCue::Clear { key: "index".into() }, 12.0);
        assert!(matches!(i.held(), Look::Status { label, .. } if label == "Catching up · 800 notes to save"));
    }

    /// Its words in the configured names, like every cue.
    #[test]
    fn catching_up_is_named() {
        let id = crate::identity::Identity::default();
        let cue = busy("archive", "Catching up · 3 notes to save").named_with(&id);
        assert!(matches!(cue, HealthCue::Busy { ref key, .. } if key == "archive"));
    }
