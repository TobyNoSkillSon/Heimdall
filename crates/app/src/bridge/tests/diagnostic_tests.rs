    use super::*;
    #[test]
    fn agent_health_records_raw_causes_even_when_the_visible_label_is_unchanged() {
        let temp = tempfile::tempdir().unwrap();
        let journal = heimdall_archive::Journal::open(&temp.path().join("session-health")).unwrap();
        let writer = heimdall_archive::records::Writer::open(&temp.path().join("session-health")).unwrap();
        let settings = Settings { dry_run: false, ..Default::default() };
        let mut health = AgentHealth::default();
        assert!(!health.record_cues(&agent::Status::AuthBlocked("RPC truncated".into()), 4, &settings, &writer).is_empty());
        writer.flush().unwrap();
        let before = journal.records().unwrap().len();
        for _ in 0..100 { health.record_cues(&agent::Status::AuthBlocked("RPC truncated".into()), 4, &settings, &writer); }
        writer.flush().unwrap();
        assert_eq!(journal.records().unwrap().len(), before);
        health.record_cues(&agent::Status::AuthBlocked("Ledger disk full".into()), 4, &settings, &writer);
        // The held fault clears when Pi idles, but Recovered is claimed only after a successful turn (E6.6).
        let cleared = health.record_cues(&agent::Status::Idle, 0, &settings, &writer);
        assert!(matches!(cleared.as_slice(), [ui::HealthCue::Clear { .. }]), "{cleared:?}");
        health.turn_succeeded();
        health.record_cues(&agent::Status::Idle, 0, &settings, &writer);
        writer.flush().unwrap();
        let rows = journal.records().unwrap();
        assert!(rows.iter().any(|r| r.payload["reason"].as_str().is_some_and(|s| s.contains("RPC truncated"))));
        assert!(rows.iter().any(|r| r.payload["reason"].as_str().is_some_and(|s| s.contains("Ledger disk full"))));
        assert!(rows.iter().any(|r| r.payload["state"] == "cleared"));
        assert!(!rows.iter().any(|r| r.payload["state"] == "recovered"), "no turn had succeeded");
    }
