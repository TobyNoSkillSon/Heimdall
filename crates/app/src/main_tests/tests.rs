    use super::*;
    #[test]
    fn default_is_dry() {
        let s = Settings::default();
        assert!(s.dry_run);
        assert!(!s.setup_requested);
    }
    #[test]
    fn replay_refuses_real_profile() {
        assert!(
            isolated_data(Some(settings::default_data().unwrap().join("test")), "test").is_err()
        );
    }
