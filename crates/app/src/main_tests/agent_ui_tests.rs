    /// Compatibility health labels remain while the card migrates to typed outcomes.
    #[test]
    fn island_card_labels_match_the_backend() {
        let bridge = [include_str!("../bridge/mod.rs"), include_str!("../bridge/adapters.rs"), include_str!("../bridge/speech.rs"),
            include_str!("../bridge/intent.rs"), include_str!("../bridge/tools.rs"), include_str!("../bridge/supervisor.rs"),
            include_str!("../bridge/delivery.rs"), include_str!("../bridge/health.rs"), include_str!("../bridge/startup.rs"),
            include_str!("../bridge/window.rs")].concat();
        for label in [ui::agent_notice::T3_CONNECTED, ui::agent_notice::T3_CONNECT_FAILED, ui::agent_notice::QUESTION_UNAVAILABLE] {
            assert!(bridge.contains(&format!("\"{label}\"")), "bridge/ no longer sends {label:?}");
        }
        assert_eq!(super::AGENT_OFF, "Agent is off");
    }

    #[test]
    fn refusals_are_calm_words() {
        let off: anyhow::Error = agent::delivery::DeliveryBlocked { written: 0, total: 0 }.into();
        assert_eq!(super::refusal(&off), "Agent is off");
        assert_eq!(super::refusal(&anyhow::anyhow!("Agent supervisor is not running")), "The agent isn't running");
    }
