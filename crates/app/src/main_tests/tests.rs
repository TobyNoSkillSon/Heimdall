    use super::*;
    #[test]
    fn default_is_dry_and_native_pending() {
        let s = Settings::default();
        assert!(s.dry_run);
        assert!(s.archive_choice_pending);
        assert_eq!(s.archive_format, settings::ArchiveFormat::NativeLossless);
        assert!(!s.setup_requested);
    }
    #[test]
    fn replay_refuses_real_profile() {
        assert!(
            isolated_data(Some(settings::default_data().unwrap().join("test")), "test").is_err()
        );
    }
