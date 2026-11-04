    use super::*;
    #[test]
    fn limited_ownership_is_journalled_each_generation_and_noticed_once() -> Result<()> {
        let d=tempfile::tempdir()?;let journal=heimdall_archive::Journal::open(&d.path().join("session-ownership"))?;
        let shown=AtomicBool::new(false);let mut notices=vec![];
        for limited in [false,true,true,false,true] {
            record_process_ownership(&journal,limited,&shown,|cue|notices.push(cue))?;
        }
        assert_eq!(notices,vec![ui::HealthCue::Notice{part:ui::Part::Agent,label:"Agent cleanup is limited".into()}]);
        let rows=journal.records()?;assert_eq!(rows.len(),5);
        assert_eq!(rows.iter().filter(|r|r.payload["cleanup_limited"]==true).count(),3);
        assert!(rows.iter().all(|r|r.kind=="agent_process_ownership"));
        Ok(())
    }
    /// Bug notes 26 Oct 00:52:29, 00:54:39: the island's words for a wake that failed: when it retries, that it is
    /// retrying, and that Heimdall gave up. An immediate resend stays silent; a restart's history says nothing.
    #[test]
    fn a_failing_wake_tells_the_island_when_it_retries_and_when_it_gave_up() {
        let r = |outcome, reason: &str| agent::Resolution { id: "w".into(), outcome, reason: reason.into() };
        let retrying = |label: &str| ui::HealthCue::Restarting { key: RETRY_KEY.into(), part: ui::Part::Agent, label: label.into() };
        assert_eq!(resolution_cues(&r(agent::Resolved::Retrying, "Request timed out."), Some(15_000)), [retrying("Model not answering · retrying in 15 s")]);
        assert_eq!(resolution_cues(&r(agent::Resolved::Retrying, "Request timed out."), Some(4_200)), [retrying("Model not answering · retrying in 5 s")]);
        assert_eq!(resolution_cues(&r(agent::Resolved::Retrying, "something odd"), None), []);
        assert_eq!(resolution_cues(&r(agent::Resolved::Resent, ""), None), [retrying("Model not answering · retrying now")]);
        assert_eq!(resolution_cues(&r(agent::Resolved::Failed, "Request timed out."), None), [ui::HealthCue::Problem {
            key: RETRY_KEY.into(), part: ui::Part::Agent, label: "Model not answering · gave up".into(), action: "Say it again".into(), control: ui::HealthControl::OpenAgent,
        }]);
        assert!(matches!(&resolution_cues(&r(agent::Resolved::Failed, "400 something"), None)[..], [ui::HealthCue::Problem { label, .. }] if label == "Agent couldn't answer · gave up"));
        assert_eq!(resolution_cues(&r(agent::Resolved::Failed, "interrupted by a restart before any tool ran; its words are history"), None), []);
    }
    /// Review 26 Oct: Agent off left "retrying in 45 s" on the island, since the waiting wake is only cancelled when
    /// Agent comes back on. A retry cue (orange or red) leaves with Agent off; orange also leaves once nothing waits.
    #[test]
    fn a_retry_cue_leaves_when_agent_is_off_or_nothing_waits() -> Result<()> {
        let d = tempfile::tempdir()?;
        let session = d.path().join("session-retry");
        std::fs::create_dir_all(&session)?;
        let diagnostics = heimdall_archive::records::Writer::open(&session)?;
        let mut b = agent::Bridge::dry_run(d.path().join("spool.sqlite"))?;
        for (cue, on, after) in [(RetryCue::Retrying, true, RetryCue::None), (RetryCue::GaveUp, true, RetryCue::GaveUp), (RetryCue::GaveUp, false, RetryCue::None), (RetryCue::Retrying, false, RetryCue::None)] {
            let mut shown = cue;
            report_resolutions(&mut b, &diagnostics, None, &mut shown, on);
            assert_eq!(shown, after, "{cue:?}, Agent on {on}");
        }
        Ok(())
    }
