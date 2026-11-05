    use super::*;
    use ui::agent_notice::{AgentNoticeAction as A, QuestionAnswer};

    /// Bug note 2026-11-05T02-53-27: every island card's answer went to the agent's question path and failed with
    /// ASK_NOT_FOUND. Through the real callback, the app's own cards (answer, pick and dismissal) reach the live loop;
    /// an agent question still goes to the agent and never to the live loop.
    #[test]
    fn the_apps_own_cards_reach_the_live_loop_and_agent_questions_do_not() {
        let t = tempfile::tempdir().unwrap();
        let tools = Arc::new(bridge::Tools::new(t.path(), t.path()));
        let (tx, rx) = unbounded();
        let callback = ui_action_callback(tools, tx, ui::Ui::handle());
        let answer = || vec![QuestionAnswer { question_id: "q1".into(), labels: vec!["Yes".into()], other: None }];
        let owned = [
            crate::suggester::CARD_PREFIX,
            crate::emergency_recovery::PREFIX,
            crate::emergency_recovery::RETURN_PREFIX,
            crate::emergency_local::CARD_PREFIX,
            crate::emergency_local::BACK_PREFIX,
            crate::voice_namer::CARD_PREFIX,
            crate::voice_suggest::CARD_PREFIX,
        ];
        for prefix in owned {
            let id = format!("{prefix}1");
            for action in [
                A::Answer { request_id: id.clone(), answers: answer(), by_hand: vec![] },
                A::Pick { request_id: id.clone(), answer: answer().remove(0) },
                A::Dismiss { request_id: id.clone() },
            ] {
                callback(ui::UiAction::AgentNotice(action.clone()));
                match rx.try_recv() {
                    Ok(ui::UiAction::AgentNotice(got)) => assert_eq!(got, action, "{prefix}"),
                    other => panic!("{prefix}: {action:?} did not reach the live loop: {other:?}"),
                }
            }
        }
        for action in [
            A::Answer { request_id: "ask-7f3a".into(), answers: answer(), by_hand: vec![] },
            A::Dismiss { request_id: "ask-7f3a".into() },
        ] {
            callback(ui::UiAction::AgentNotice(action));
            assert!(rx.try_recv().is_err(), "an agent question is the agent's");
        }
    }
