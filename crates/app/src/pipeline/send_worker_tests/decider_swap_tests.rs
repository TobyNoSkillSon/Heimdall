    //! Decider ▸ precision hot-swap on the real d1 owner, with a fake decider (no model): ordering against decisions in
    //! flight and queued, the rollback, and the live loop's setting, status and menu.
    use super::*;
    use crate::pipeline::plumbing_tests::rig;
    use decide::precision::{DeciderModel, Pick, Precision};
    use std::sync::Mutex;
    use std::time::Duration;

    type Log = Arc<Mutex<Vec<String>>>;

    /// A fake decider: its version and precision, and its drop logged.
    struct Fake(Pick, Log);
    impl Drop for Fake {
        fn drop(&mut self) { self.1.lock().unwrap().push(format!("drop {}", self.0.label())); }
    }
    fn v6(p: Precision) -> Pick { Pick::new(DeciderModel::V6, p) }
    const V6_BF16: Pick = Pick::new(DeciderModel::V6, Precision::Bf16);
    const V6_INT4: Pick = Pick::new(DeciderModel::V6, Precision::Int4);

    fn pump_until(app: &mut Pipeline, mut done: impl FnMut(&Pipeline) -> bool) {
        let until = Instant::now() + Duration::from_secs(10);
        while !done(app) {
            let event = app.internal.1.recv_timeout(until.saturating_duration_since(Instant::now())).expect("owner stalled");
            app.internal(event).unwrap();
        }
    }

    fn owner(app: &mut Pipeline) {
        app.send_worker = Some(InferenceOwner::supervised(app.internal.0.clone(), Arc::new(|| anyhow::bail!("fake owner: no model")), request_deadline, owner_pause).unwrap());
    }

    struct Rig {
        slot: Arc<Mutex<Option<Fake>>>,
        log: Log,
        answers: Arc<Mutex<Vec<(usize, Option<Pick>)>>>,
        outcomes: Arc<Mutex<Vec<crate::pipeline::decider_swap::Outcome>>>,
    }

    impl Rig {
        fn new(start: Pick) -> Self {
            let log: Log = Default::default();
            Rig { slot: Arc::new(Mutex::new(Some(Fake(start, log.clone())))), log, answers: Default::default(), outcomes: Default::default() }
        }
        /// A sentence's send decision, answered by whatever decider holds the slot (after `hold`).
        fn ask(&self, app: &Pipeline, n: usize, hold: Duration) {
            let (slot, log, answers) = (self.slot.clone(), self.log.clone(), self.answers.clone());
            app.send_worker.as_ref().unwrap().request(1, 20, move |_| {
                std::thread::sleep(hold);
                let p = slot.lock().unwrap().as_ref().map(|d| d.0);
                log.lock().unwrap().push(format!("decide {n} on {}", p.map_or("nothing".into(), |p| p.label())));
                p
            }, move |p, got| { answers.lock().unwrap().push((n, got)); p.decisions += 1; Ok(()) }).unwrap();
        }
        /// The swap as `InferenceOwner::swap` queues it (the top lane), on the fake: `fails` never loads.
        fn swap(&self, app: &Pipeline, to: Pick, fails: &'static [Pick]) {
            let (slot, log, outcomes) = (self.slot.clone(), self.log.clone(), self.outcomes.clone());
            app.send_worker.as_ref().unwrap().request(3, 0, move |state| {
                let choice = state.choice.clone();
                let log2 = log.clone();
                let mut held = slot.lock().unwrap();
                let from = state.choice.chosen();
                let out = swap_on_owner(&state.choice, &state.loading, &state.loads, &state.stop, &mut held, from, to, || {
                    let p = choice.chosen();
                    log2.lock().unwrap().push(format!("load {}", p.label()));
                    std::thread::sleep(Duration::from_millis(150));
                    if fails.contains(&p) { anyhow::bail!("the engine cannot read this checkpoint") }
                    Ok(Fake(p, log2.clone()))
                }, |_| Ok(()));
                state.after_swap(&out);
                out
            }, move |_, out| { outcomes.lock().unwrap().push(out); Ok(()) }).unwrap();
        }
    }

    /// hot-swap without restarting Heimdall. A swap asked while a decision runs waits for it; sentences
    /// arriving during the swap stay queued (none dropped) and are answered by the new decider; the old one is gone
    /// before the new one loads.
    #[test]
    fn a_swap_while_busy_finishes_the_decision_then_answers_the_queue_on_the_new_decider() {
        let (_t, mut app, _) = rig(false);
        owner(&mut app);
        app.send_worker.as_ref().unwrap().choice.set_chosen(v6(Precision::Int4));
        let r = Rig::new(v6(Precision::Int4));
        r.ask(&app, 0, Duration::from_millis(300));
        std::thread::sleep(Duration::from_millis(60));
        r.swap(&app, v6(Precision::Int8), &[]);
        for n in 1..=3 { r.ask(&app, n, Duration::ZERO); }
        pump_until(&mut app, |p| p.decisions == 4);
        assert_eq!(*r.outcomes.lock().unwrap(), [crate::pipeline::decider_swap::Outcome::Swapped { used: v6(Precision::Int8) }]);
        assert_eq!(*r.answers.lock().unwrap(), [(0, Some(v6(Precision::Int4))), (1, Some(v6(Precision::Int8))), (2, Some(v6(Precision::Int8))), (3, Some(v6(Precision::Int8)))],
            "the in-flight one on the old decider, every queued one on the new, none dropped");
        assert_eq!(*r.log.lock().unwrap(), ["decide 0 on V6 4-bit", "drop V6 4-bit", "load V6 8-bit", "decide 1 on V6 8-bit", "decide 2 on V6 8-bit", "decide 3 on V6 8-bit"]);
        assert_eq!(app.send_worker.as_ref().unwrap().choice.chosen(), v6(Precision::Int8), "later reloads load the 8-bit");
    }

    /// A precision that does not load goes back to the previous one; the queue is answered by it.
    #[test]
    fn a_failed_swap_rolls_back_and_the_queue_is_answered_by_the_previous_precision() {
        let (_t, mut app, _) = rig(false);
        owner(&mut app);
        let r = Rig::new(v6(Precision::Int4));
        r.swap(&app, V6_BF16, &[V6_BF16]);
        r.ask(&app, 1, Duration::ZERO);
        pump_until(&mut app, |p| p.decisions == 1);
        let out = r.outcomes.lock().unwrap().clone();
        assert!(matches!(&out[..], [crate::pipeline::decider_swap::Outcome::RolledBack { reason, used }] if *used == v6(Precision::Int4) && reason.contains("V6 16-bit didn\u{2019}t load")), "{out:?}");
        assert_eq!(*r.answers.lock().unwrap(), [(1, Some(v6(Precision::Int4)))]);
        assert_eq!(*r.log.lock().unwrap(), ["drop V6 4-bit", "load V6 16-bit", "load V6 4-bit", "decide 1 on V6 4-bit"]);
        assert_eq!(app.send_worker.as_ref().unwrap().choice.chosen(), v6(Precision::Int4));
    }

    /// The rollback to V5 (4 Nov): a version change is a swap like a precision change, on the same owner, at the same
    /// precision; one that fails goes back to V6.
    #[test]
    fn switching_the_version_swaps_the_decider_and_a_failed_one_goes_back() {
        let (_t, mut app, _) = rig(false);
        owner(&mut app);
        let v5 = Pick::new(DeciderModel::V5, Precision::Int4);
        let r = Rig::new(v6(Precision::Int4));
        r.swap(&app, v5, &[]);
        r.ask(&app, 1, Duration::ZERO);
        pump_until(&mut app, |p| p.decisions == 1);
        assert_eq!(*r.outcomes.lock().unwrap(), [crate::pipeline::decider_swap::Outcome::Swapped { used: v5 }]);
        assert_eq!(*r.log.lock().unwrap(), ["drop V6 4-bit", "load V5 4-bit", "decide 1 on V5 4-bit"]);
        assert_eq!(app.send_worker.as_ref().unwrap().choice.chosen(), v5);
        r.swap(&app, V6_INT4, &[V6_INT4]);
        r.ask(&app, 2, Duration::ZERO);
        pump_until(&mut app, |p| p.decisions == 2);
        let out = r.outcomes.lock().unwrap().clone();
        assert!(matches!(&out[1], crate::pipeline::decider_swap::Outcome::RolledBack { reason, used } if *used == v5 && reason.starts_with("V6 4-bit didn\u{2019}t load")), "{out:?}");
        assert_eq!(app.send_worker.as_ref().unwrap().choice.chosen(), v5);
    }

    fn statuses(app: &Pipeline) -> Vec<ui::DeciderStatus> {
        app.hooks.ui.lock().unwrap().iter().filter_map(|u| match u { ui::UiUpdate::Decider(d) => Some(d.clone()), _ => None }).collect()
    }

    /// The live loop end to end: the menu's Confirm starts a swap (the menu spins on it), the owner's loads fail (no
    /// model here), and the setting goes back with the reason shown and saved.
    #[test]
    fn the_menu_pick_swaps_and_a_failed_one_goes_back_with_its_reason_in_the_menu() {
        let (_t, mut app, _) = rig(false);
        app.test_persist_settings();
        owner(&mut app);
        assert_eq!(app.decider_pick(), v6(Precision::Int4), "V6 in 4-bit by default");
        app.action(ui::UiAction::SetDeciderPrecision("8bit".into())).unwrap();
        assert_eq!(app.settings.decider_precision, Precision::Int8);
        let swapping = statuses(&app).last().cloned().unwrap();
        assert_eq!(swapping.state, ui::DeciderState::Swapping { to: "8bit".into() });
        assert_eq!(swapping.version, "Decider V6");
        assert_eq!(swapping.options.iter().map(|o| o.label.as_str()).collect::<Vec<_>>(), ["16-bit", "8-bit", "4-bit"]);
        pump_until(&mut app, |p| p.decider_swapping.is_none());
        assert_eq!(app.settings.decider_precision, Precision::Int4, "back to the previous precision");
        let failed = statuses(&app).last().cloned().unwrap();
        assert!(matches!(&failed.state, ui::DeciderState::Failed(_)), "{failed:?}");
        assert!(failed.reason.as_deref().is_some_and(|r| r.starts_with("V6 8-bit didn\u{2019}t load") && r.contains("V6 4-bit didn\u{2019}t load either")), "{failed:?}");
        let saved = crate::settings::Settings::load(&app.data.join("settings.json")).unwrap();
        assert_eq!(saved.decider_precision, Precision::Int4, "the rollback is saved");
        assert!(app.health.holds("decisions"), "nothing runs: commands show as paused, never OK");
        assert_eq!(app.send_worker.as_ref().unwrap().choice.chosen(), v6(Precision::Int4), "the owner's next load is the previous precision");
        let records = app.index.evidence.records().unwrap();
        assert!(records.iter().any(|r| r.kind == "decider_swap" && r.payload["to"] == "8bit"));
        assert!(records.iter().any(|r| r.kind == "decider_swapped" && r.payload["outcome"] == "failed"));
    }

    /// Listening off while a swap runs: the owner goes with it before the new precision answered, so the setting goes
    /// back to the precision that ran and nothing stays marked as swapping.
    #[test]
    fn listening_off_mid_swap_keeps_the_precision_that_ran() {
        let (_t, mut app, _) = rig(false);
        app.test_persist_settings();
        owner(&mut app);
        app.action(ui::UiAction::SetDeciderPrecision("16bit".into())).unwrap();
        assert!(app.decider_swapping.is_some());
        app.set_running(false, "test").unwrap();
        assert!(app.decider_swapping.is_none());
        assert_eq!(app.settings.decider_precision, Precision::Int4);
        assert_eq!(crate::settings::Settings::load(&app.data.join("settings.json")).unwrap().decider_precision, Precision::Int4);
        assert_eq!(statuses(&app).last().unwrap().state, ui::DeciderState::Off);
    }

    /// No decider loaded yet (Listening off): a pick is only the setting, which the next load reads; an unknown or
    /// unchanged key changes nothing.
    #[test]
    fn a_pick_with_no_decider_loaded_is_kept_for_the_next_load() {
        let (_t, mut app, _) = rig(false);
        app.test_persist_settings();
        app.action(ui::UiAction::SetDeciderPrecision("16bit".into())).unwrap();
        assert_eq!(app.settings.decider_precision, Precision::Bf16);
        assert!(app.decider_swapping.is_none());
        let last = statuses(&app).last().cloned().unwrap();
        assert_eq!((last.chosen.as_str(), &last.state), ("16bit", &ui::DeciderState::Off));
        assert_eq!(crate::settings::Settings::load(&app.data.join("settings.json")).unwrap().decider_precision, Precision::Bf16);
        app.action(ui::UiAction::SetDeciderPrecision("2bit".into())).unwrap();
        app.action(ui::UiAction::SetDeciderPrecision("16bit".into())).unwrap();
        assert_eq!(app.settings.decider_precision, Precision::Bf16);
        // The owner started now (Listening on) loads the 16-bit.
        assert_eq!(app.inference().unwrap().choice.chosen(), v6(Precision::Bf16));
    }

    /// The setting's rollback path (4 Nov): `decider_model: "v5"` through the settings apply (the file, the settings tool)
    /// swaps the running decider; the owner here cannot load (no model), so the setting goes back to V6, saved, with the
    /// reason naming the version. With no decider loaded the next load reads it.
    #[test]
    fn decider_model_v5_in_settings_swaps_and_a_failed_one_goes_back_to_v6() {
        let (_t, mut app, _) = rig(false);
        app.test_persist_settings();
        owner(&mut app);
        let mut next = app.settings.clone();
        next.decider_model = DeciderModel::V5;
        app.apply_menu_settings(next).unwrap();
        assert_eq!(app.decider_swapping, Some((v6(Precision::Int4), Pick::new(DeciderModel::V5, Precision::Int4))));
        assert_eq!(statuses(&app).last().unwrap().state, ui::DeciderState::Swapping { to: "4bit".into() });
        pump_until(&mut app, |p| p.decider_swapping.is_none());
        assert_eq!(app.settings.decider_model, DeciderModel::V6, "back to the version that ran");
        let failed = statuses(&app).last().cloned().unwrap();
        assert!(failed.reason.as_deref().is_some_and(|r| r.starts_with("V5 4-bit didn\u{2019}t load")), "{failed:?}");
        assert_eq!(crate::settings::Settings::load(&app.data.join("settings.json")).unwrap().decider_model, DeciderModel::V6);
        let records = app.index.evidence.records().unwrap();
        assert!(records.iter().any(|r| r.kind == "decider_swap" && r.payload["to_model"] == "v5" && r.payload["from_model"] == "v6"));
        // Listening off (no owner): the pick is only the setting, and the owner started next loads V5.
        app.send_worker = None;
        let mut next = app.settings.clone();
        next.decider_model = DeciderModel::V5;
        app.apply_menu_settings(next).unwrap();
        assert!(app.decider_swapping.is_none());
        assert_eq!(app.inference().unwrap().choice.chosen(), Pick::new(DeciderModel::V5, Precision::Int4));
    }
