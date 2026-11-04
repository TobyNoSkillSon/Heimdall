//! Setup's app-side rules, against temp folders and fixture T3 Code bundles.
use super::*;
use heimdall_home::Source;
use std::fs;

fn env(user_home: &Path, bundle_id: &str) -> Env {
    Env { user_home: user_home.to_path_buf(), bundle_id: bundle_id.into(), data_override: None, home_override: None }
}

#[test]
fn wizard_first_launch_rule_per_edition() {
    // Before setup was requested: the whole tour from Welcome, in both editions.
    assert_eq!(first_launch(false, false, || true), Some(ui::SetupStep::Welcome));
    assert_eq!(first_launch(true, false, || false), Some(ui::SetupStep::Welcome));
    // Public: the home step while no home folder was chosen.
    assert_eq!(first_launch(true, true, || true), Some(ui::SetupStep::Home));
    assert_eq!(first_launch(true, true, || false), None);
    // Personal: never by itself for the legacy profile, and the home question is not even asked.
    assert_eq!(first_launch(false, true, || panic!("the personal edition never checks the home choice")), None);
}

#[test]
fn wizard_personal_legacy_profile_needs_a_choice_but_does_not_open_setup() {
    let user = tempfile::tempdir().unwrap();
    fs::create_dir_all(user.path().join("Library/Application Support/Heimdall")).unwrap();
    let loc = heimdall_home::resolve_in(&env(user.path(), heimdall_home::PERSONAL_ID));
    assert_eq!(loc.source, Source::Legacy);
    assert!(loc.needs_choice());
    assert_eq!(first_launch(false, true, || loc.needs_choice()), None);
    // Kept as it is: home and data recorded, nothing moved, no question next time.
    home_change::keep_in(&env(user.path(), heimdall_home::PERSONAL_ID), &loc).unwrap();
    let after = heimdall_home::resolve_in(&env(user.path(), heimdall_home::PERSONAL_ID));
    assert_eq!((after.source, &after.data), (Source::Pointer, &loc.data));
    assert!(!after.needs_choice());
}

#[test]
fn wizard_public_fresh_profile_keeps_or_plans_a_move() {
    let user = tempfile::tempdir().unwrap();
    let e = env(user.path(), heimdall_home::PUBLIC_ID);
    let loc = heimdall_home::resolve_in(&e);
    assert_eq!(loc.source, Source::Fresh);
    fs::create_dir_all(&loc.data).unwrap();
    fs::write(loc.data.join("settings.json"), vec![b'x'; 1234]).unwrap();
    // A new folder: the move with its size, waiting for the confirm; nothing written yet.
    let docs = user.path().join("Documents");
    fs::create_dir_all(&docs).unwrap();
    let (plan, state) = choose(&loc, home_change::plan_in(&e, &loc, &docs));
    let shown = state.pending.unwrap();
    assert_eq!((shown.bytes, shown.to_home.file_name().unwrap().to_str().unwrap()), (1234, "Documents"));
    assert!(plan.is_some() && heimdall_home::read_pointer(&e).is_none());
    // A folder that cannot be home: the reason, no plan.
    let (plan, state) = choose(&loc, home_change::plan_in(&e, &loc, &user.path().join("missing")));
    assert!(plan.is_none() && state.error.unwrap().contains("does not exist"));
    // Continue instead: the user's home is the choice.
    home_change::keep_in(&e, &loc).unwrap();
    assert!(!heimdall_home::resolve_in(&e).needs_choice());
}

#[test]
fn wizard_confirmed_move_is_recorded_restarts_and_quits() {
    let user = tempfile::tempdir().unwrap();
    let e = env(user.path(), heimdall_home::PUBLIC_ID);
    let loc = heimdall_home::resolve_in(&e);
    fs::create_dir_all(&loc.data).unwrap();
    fs::write(loc.data.join("settings.json"), b"{}").unwrap();
    let docs = user.path().join("Documents");
    fs::create_dir_all(&docs).unwrap();
    let (plan, _) = choose(&loc, home_change::plan_in(&e, &loc, &docs));
    let to_home = plan.as_ref().unwrap().to_home.clone();
    // Nothing waiting for a confirm: nothing happens.
    assert!(confirm(&loc, None, |_| panic!("nothing to request"), || panic!("nothing to restart")).is_empty());
    let mut restarted = false;
    let updates = confirm(&loc, plan, |home| home_change::request_in(&e, &loc, home), || { restarted = true; Ok(()) });
    // The move is in the pointer for the next start, the fresh copy was started, then the normal Quit.
    assert_eq!(updates, [ui::UiUpdate::Quit]);
    assert!(restarted);
    let pending = heimdall_home::read_pointer(&e).unwrap().pending.unwrap();
    assert_eq!(pending.to_home, to_home);
}

#[test]
fn wizard_failed_restart_shows_in_setup_and_does_not_quit() {
    let user = tempfile::tempdir().unwrap();
    let e = env(user.path(), heimdall_home::PUBLIC_ID);
    let loc = heimdall_home::resolve_in(&e);
    fs::create_dir_all(&loc.data).unwrap();
    let docs = user.path().join("Documents");
    fs::create_dir_all(&docs).unwrap();
    let (plan, _) = choose(&loc, home_change::plan_in(&e, &loc, &docs));
    let updates = confirm(&loc, plan, |home| home_change::request_in(&e, &loc, home), || Err("/tmp/heimdall is not inside an app; start it again by hand.".into()));
    let [ui::UiUpdate::Home(state)] = updates.as_slice() else { panic!("one Home state, no Quit: {updates:?}") };
    assert_eq!(state.error.as_deref(), Some(RESTART_FAILED));
    assert!(state.pending.is_none() && !state.needs_choice);
    // The move stays recorded: it happens at the next start.
    assert!(heimdall_home::read_pointer(&e).unwrap().pending.is_some());
}

#[test]
fn wizard_move_of_a_folder_named_at_launch_is_refused_without_a_restart() {
    let user = tempfile::tempdir().unwrap();
    let lab = user.path().join("lab");
    fs::create_dir_all(&lab).unwrap();
    let e = Env { data_override: Some(lab), ..env(user.path(), heimdall_home::PUBLIC_ID) };
    let loc = heimdall_home::resolve_in(&e);
    assert_eq!(loc.source, Source::Env);
    let docs = user.path().join("Documents");
    fs::create_dir_all(&docs).unwrap();
    let plan = home_change::plan_in(&e, &loc, &docs).ok();
    let updates = confirm(&loc, plan, |home| home_change::request_in(&e, &loc, home), || panic!("a refused move never restarts"));
    let [ui::UiUpdate::Home(state)] = updates.as_slice() else { panic!("{updates:?}") };
    assert!(state.error.as_deref().unwrap().contains("named data folder"));
    assert!(!e.pointer_path().exists());
    // Continue records nothing for it either.
    home_change::keep_in(&e, &loc).unwrap();
    assert!(!e.pointer_path().exists());
}

#[test]
fn wizard_t3_detection_states_from_fixture_bundles() {
    let user = tempfile::tempdir().unwrap();
    let apps = user.path().join("Applications");
    let dirs = vec![apps.clone()];
    let missing = t3_state(&t3::install::detect(user.path(), &dirs), false, ui::T3Install::Idle);
    assert!(missing.found.is_none() && !missing.ready());
    let bundle = |name: &str, version: &str| {
        let app = apps.join(format!("{name}.app"));
        fs::create_dir_all(app.join("Contents")).unwrap();
        fs::write(app.join("Contents/Info.plist"), format!("<plist><dict><key>CFBundleIdentifier</key><string>com.t3tools.t3code</string><key>CFBundleName</key><string>{name}</string><key>CFBundleShortVersionString</key><string>{version}</string></dict></plist>")).unwrap();
    };
    bundle("T3 Code (Alpha)", "0.0.45");
    let old = t3_state(&t3::install::detect(user.path(), &dirs), false, ui::T3Install::Idle);
    assert!(!old.ready() && old.found.as_ref().is_some_and(|a| a.version == "0.0.45"));
    bundle("T3 Code", "0.1.3");
    let found = t3_state(&t3::install::detect(user.path(), &dirs), true, ui::T3Install::Idle);
    assert!(found.ready() && found.connect_allowed);
    assert_eq!(found.found.unwrap().name, "T3 Code");
}

#[test]
fn wizard_t3_consent_gate_per_edition() {
    // Personal: connects as before. Public: only after Setup's Connect.
    assert!(t3_allowed(false, false));
    assert!(!t3_allowed(true, false));
    assert!(t3_allowed(true, true));
    let saved = crate::settings::Settings { t3_connect_allowed: true, ..Default::default() };
    load_consent(&saved);
    assert!(t3_consent().load(Ordering::Acquire));
    load_consent(&crate::settings::Settings::default());
    assert_eq!(t3_consent().load(Ordering::Acquire), !crate::edition::PUBLIC);
    // The choice is saved only when made: a default settings file never carries it.
    let json = serde_json::to_string(&crate::settings::Settings::default()).unwrap();
    assert!(!json.contains("t3_connect_allowed"));
}

#[test]
fn wizard_vella_detection_and_install_command() {
    let user = tempfile::tempdir().unwrap();
    let apps = user.path().join("Applications");
    let dirs = vec![apps.clone()];
    assert_eq!(find_vella(&dirs), None);
    let bundle = |name: &str, id: &str| {
        let app = apps.join(format!("{name}.app"));
        fs::create_dir_all(app.join("Contents")).unwrap();
        fs::write(app.join("Contents/Info.plist"), format!("<plist><dict><key>CFBundleIdentifier</key><string>{id}</string><key>CFBundleShortVersionString</key><string>2.1.2</string></dict></plist>")).unwrap();
        app
    };
    // Another app named Vella is not Vella; the bundle ID decides.
    bundle("Vella", "com.example.vella");
    assert_eq!(find_vella(&dirs), None);
    let real = bundle("Vella Dictation", VELLA_ID);
    assert_eq!(find_vella(&dirs), Some(ui::VellaApp { path: real, version: "2.1.2".into() }));
    // Homebrew: the first that exists; none means the page opens instead.
    let brew = user.path().join("brew");
    assert_eq!(find_brew(&[&user.path().join("none"), &brew]), None);
    fs::write(&brew, "").unwrap();
    assert_eq!(find_brew(&[&user.path().join("none"), &brew]), Some(brew.clone()));
    // The command the button runs, built but never run here.
    let command = vella_install_command(&brew);
    assert_eq!(command.get_program(), brew.as_os_str());
    assert_eq!(command.get_args().collect::<Vec<_>>(), ["install", "--cask", "tobynoskillson/tap/vella"]);
}
