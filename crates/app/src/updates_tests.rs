use super::*;

struct FakeHost {
    busy: Mutex<Option<String>>,
    notices: Mutex<Vec<Notice>>,
}

impl BusyGate for FakeHost {
    fn busy(&self) -> Option<String> {
        self.busy.lock().unwrap().clone()
    }
}

impl Host for FakeHost {
    fn notify(&self, notice: Notice) {
        self.notices.lock().unwrap().push(notice);
    }
    fn quit(&self) {
        panic!("tests never quit");
    }
}

fn release(tag: &str) -> Release {
    let version = Version::parse(tag).unwrap();
    Release {
        tag: tag.into(),
        assets: vec![Product::ALPHA.zip_name(&version), "SHA256SUMS".into(), "SHA256SUMS.sig".into()],
        version,
        name: String::new(),
        body: "Faster start.".into(),
        draft: false,
        prerelease: true,
        published: true,
    }
}

fn updates(dir: &Path, busy: Option<&str>) -> (Updates, Arc<FakeHost>) {
    let host = Arc::new(FakeHost { busy: Mutex::new(busy.map(str::to_owned)), notices: Mutex::new(vec![]) });
    let app = dir.join("Applications/Heimdall Alpha.app");
    let u = Updates::new(host.clone(), app, dir.join("updates"), Version::parse("0.1.0-alpha.1").unwrap());
    (u, host)
}

#[test]
fn updates_are_off_in_the_personal_edition_and_outside_a_bundle() {
    // Test binaries run from target/, never from an app bundle, in either edition.
    assert!(!enabled());
    assert!(running_app().is_none());
    report(ReadyPhase::Ready, ""); // a no-op when off
}

#[test]
fn the_menu_row_follows_the_phase() {
    assert_eq!(menu_row(&Phase::Idle), MenuRow { title: "Check for Updates…".into(), enabled: true, command: UpdateCommand::CheckForUpdates });
    let r = release("v0.1.0-alpha.2");
    assert_eq!(menu_row(&Phase::Available(r.clone())).title, "Update to 0.1.0-alpha.2…");
    assert_eq!(menu_row(&Phase::Available(r.clone())).command, UpdateCommand::ShowOffer);
    assert!(!menu_row(&Phase::Downloading(r.clone())).enabled);
    let (title, message) = offer_text(&r);
    assert_eq!(title, "Update to Heimdall Alpha 0.1.0-alpha.2?");
    assert!(message.ends_with("Faster start."));
}

#[test]
fn a_saved_offer_is_restored_until_this_version_catches_up() {
    let dir = tempfile::tempdir().unwrap();
    let (u, _) = updates(dir.path(), None);
    let state = u.state.lock().unwrap().dir.clone();
    Updates::save(&state, &Saved { last_check: Some(now()), offer: Some(release_json(&release("v0.1.0-alpha.2"))) });
    u.restore_offer();
    assert_eq!(u.menu_row().title, "Update to 0.1.0-alpha.2…");
    Updates::save(&state, &Saved { last_check: Some(now()), offer: Some(release_json(&release("v0.1.0-alpha.1"))) });
    let (older, _) = updates(dir.path(), None);
    older.restore_offer();
    assert_eq!(older.menu_row().command, UpdateCommand::CheckForUpdates);
    assert!(Updates::saved(&state).offer.is_none(), "a caught-up offer is dropped");
}

#[test]
fn update_is_refused_while_busy_and_no_check_runs_when_not_due() {
    let dir = tempfile::tempdir().unwrap();
    let (u, host) = updates(dir.path(), Some("recording"));
    u.state.lock().unwrap().machine.handle(Event::Checked(Some(release("v0.1.0-alpha.2"))));
    u.send(UpdateCommand::Install);
    let notices = host.notices.lock().unwrap().clone();
    assert!(matches!(&notices[..], [Notice::Failed(m)] if m.contains("recording")), "{notices:?}");
    assert!(matches!(u.state.lock().unwrap().machine.phase, Phase::Available(_)), "the offer stays");
    // Busy: the periodic check waits even when due.
    u.check_if_due();
    assert!(!u.state.lock().unwrap().checking);
    // Idle but checked a minute ago: not due.
    *host.busy.lock().unwrap() = None;
    let dir_state = u.state.lock().unwrap().dir.clone();
    Updates::save(&dir_state, &Saved { last_check: Some(now() - 60), offer: None });
    u.check_if_due();
    assert!(!u.state.lock().unwrap().checking);
    u.send(UpdateCommand::ShowOffer);
    assert!(matches!(host.notices.lock().unwrap().last(), Some(Notice::Offer { title, .. }) if title.contains("0.1.0-alpha.2")));
}

#[test]
fn updates_gating_per_edition() {
    // Personal: never, whatever else holds (the bundle is not even looked at).
    assert!(!enabled_for(false, false, || panic!("the personal edition never looks for its bundle")));
    // Public: from its bundle, unless HEIMDALL_UPDATE=0.
    assert!(enabled_for(true, false, || true));
    assert!(!enabled_for(true, true, || true));
    assert!(!enabled_for(true, false, || false));
    // Off means no host is started, no menu row is ever sent, and the menu's clicks do nothing.
    assert!(RUNNING.get().is_none());
    row_clicked();
    install_clicked();
}

#[test]
fn updates_busy_is_the_agent_speech_or_a_loading_model() {
    assert_eq!(busy_reason(false, false, false), None);
    assert_eq!(busy_reason(true, true, true).as_deref(), Some("working on a task"));
    assert_eq!(busy_reason(false, true, false).as_deref(), Some("speaking"));
    assert_eq!(busy_reason(false, false, true).as_deref(), Some("loading a model"));
}

#[test]
fn updates_notices_become_the_menu_row_or_native_alerts() {
    let row = || Some(menu_row(&Phase::Available(release("v0.1.0-alpha.2"))));
    assert_eq!(ui_update(Notice::Changed, row), Some(ui::UiUpdate::UpdateRow(Some(ui::UpdateRow { title: "Update to 0.1.0-alpha.2…".into(), enabled: true }))));
    assert_eq!(ui_update(Notice::Changed, || None), None);
    let offer = ui_update(Notice::Offer { title: "Update to Heimdall Alpha 0.1.0-alpha.2?".into(), message: "m".into() }, || None);
    assert!(matches!(offer, Some(ui::UiUpdate::UpdateAlert(ui::UpdateAlert { offer: true, .. }))));
    for notice in [Notice::UpToDate("u".into()), Notice::Failed("f".into()), Notice::Result("r".into())] {
        assert!(matches!(ui_update(notice, || None), Some(ui::UiUpdate::UpdateAlert(ui::UpdateAlert { offer: false, .. }))));
    }
}
