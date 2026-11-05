//! The first-run greeting: Setup's Done wakes the agent with one greeting, once ever per profile;
//! a later launch and an upgraded profile that was set up before this build get none. Through the real `Pipeline`
//! and its saved settings.json, with the fake Pi (no model).
use super::*;

/// The greeting notices Pi was sent (queued or delivered; the spool keeps both). A finished delivery's body moves to
/// the archive while the agent runs (`Bridge::retire`), under whichever launch's session was live then, so a retired
/// row is read back from any session.
fn greetings(data: &std::path::Path) -> usize {
    let Ok(db) = rusqlite::Connection::open(data.join("pi-spool.sqlite")) else { return 0 };
    let Ok(mut q) = db.prepare("SELECT wire FROM deliveries") else { return 0 };
    let wires: Vec<String> = q.query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect();
    let root = data.join("archive");
    let unarchived = |wire: &String| -> String {
        let value: serde_json::Value = serde_json::from_str(wire).unwrap();
        let Some(r) = value["archive_ref"].as_str() else { return wire.clone() };
        heimdall_archive::store::sessions(&root).unwrap().iter()
            .find_map(|name| heimdall_archive::store::read_log(&heimdall_archive::store::session_path(&root, name), r).ok())
            .map(|bytes| String::from_utf8(bytes).unwrap())
            .unwrap_or_else(|| panic!("archived delivery {r} not found"))
    };
    wires.iter().map(unarchived).filter(|w| w.contains(agent::notice::FIRST_RUN)).count()
}

/// One launch on this data folder: its settings as the live app loads them, Agent on or off (the menu's switch, as
/// saved), the agent started when on, then Setup's Done. Returns the settings it saved.
fn launch_and_finish_setup(data: &std::path::Path, agent_on: bool) -> Settings {
    launch(data, agent_on, true)
}
/// `launch_and_finish_setup`, with or without pressing Done.
fn launch(data: &std::path::Path, agent_on: bool, done: bool) -> Settings {
    let mut settings = Settings::load_and_migrate(&data.join("settings.json")).unwrap();
    settings.dry_run = !agent_on;
    let mut a = Pipeline::new(data.to_path_buf(), data.join("missing-models"), settings, None, false).unwrap();
    if agent_on {
        a.ensure_agent().unwrap();
    }
    if done {
        a.action(ui::UiAction::SetupDone).unwrap();
    }
    if let Some(agent) = a.agent.as_ref() {
        agent.barrier().unwrap();
    }
    Settings::load(&data.join("settings.json")).unwrap()
}

fn profile() -> (tempfile::TempDir, std::path::PathBuf) {
    let t = tempfile::tempdir().unwrap();
    let data = t.path().join("data");
    settings::private_dir(&data).unwrap();
    (t, data)
}

#[test]
fn setup_done_greets_once_per_profile_and_never_an_upgraded_one() {
    crate::bridge::test_harness(Some(super::plumbing_tests::mock_harness()));

    // A new profile (no settings.json yet): Setup's Done sends exactly one greeting, and the profile records it.
    let (_fresh, data) = profile();
    let saved = launch_and_finish_setup(&data, true);
    assert_eq!(greetings(&data), 1, "Setup's Done sends one greeting");
    assert_eq!((saved.first_run_greeted, saved.first_run_greeting_due), (Some(true), false), "the greeting is recorded in the profile");

    // A second launch, and Done again (Setup opened from the menu): nothing more.
    launch_and_finish_setup(&data, true);
    assert_eq!(greetings(&data), 1, "a second launch sends no greeting");

    // A profile set up by an older build (no greeting field, as the user's): upgraded to this one, Done sends nothing.
    let (_old, data) = profile();
    std::fs::write(data.join("settings.json"), r#"{"dry_run":false,"setup_requested":true}"#).unwrap();
    let saved = launch_and_finish_setup(&data, true);
    assert_eq!(saved.first_run_greeted, Some(true), "an upgraded, set-up profile counts as greeted");
    assert_eq!(greetings(&data), 0, "an upgraded profile is not greeted");

    // Done with Agent off: nothing can go yet; the greeting waits in the profile and goes, once, at the next start.
    let (_off, data) = profile();
    let saved = launch_and_finish_setup(&data, false);
    assert_eq!((greetings(&data), saved.first_run_greeted, saved.first_run_greeting_due), (0, Some(false), true));
    let saved = launch(&data, true, false);
    assert_eq!(greetings(&data), 1, "the waiting greeting goes at the agent's start, without another Done");
    assert_eq!((saved.first_run_greeted, saved.first_run_greeting_due), (Some(true), false));

    // Done with Agent off, then Agent switched on in the same run (its pre-flight passed): the greeting goes, once.
    let (_on, data) = profile();
    let mut a = Pipeline::new(data.clone(), data.join("missing-models"), Settings::load_and_migrate(&data.join("settings.json")).unwrap(), None, false).unwrap();
    a.action(ui::UiAction::SetupDone).unwrap();
    assert_eq!((greetings(&data), a.settings.first_run_greeting_due), (0, true));
    a.tools.menu_intent(agent::delivery::StopReason::Agent, true);
    a.tools.menu_intent_processed(agent::delivery::StopReason::Agent);
    let mut next = a.settings.clone();
    next.dry_run = false;
    a.apply_menu_settings(next).unwrap();
    a.agent.as_ref().unwrap().barrier().unwrap();
    assert_eq!(greetings(&data), 1, "Agent on sends the waiting greeting");
    assert_eq!(Settings::load(&data.join("settings.json")).unwrap().first_run_greeted, Some(true));
}

#[test]
fn the_greeting_wakes_the_agent_before_anyone_has_spoken() {
    // 5 Nov: the message says the intent plainly, and asks for a short spoken welcome, not a tour.
    for public in [false, true] {
        let text = crate::setup_wizard::greeting(public, None);
        assert!(text.starts_with("The user just finished setting you up. Greet them and talk to them."), "{text}");
        assert!(text.contains("`speak`") && text.contains("a sentence or two") && !text.contains("explain what you can do"));
        assert_eq!(text.contains("`onboarding`"), public, "only the public edition ships the skill");
    }
    let kind = agent::notice::NoticeKind::classify(agent::notice::FIRST_RUN, &crate::setup_wizard::greeting(crate::edition::PUBLIC, None));
    assert_eq!(kind, agent::notice::NoticeKind::Scheduled);
    let checked = crate::setup_wizard::greeting(true, Some((std::path::Path::new("/Applications/Heimdall Alpha.app/Contents/MacOS/heimdall-emergency"), std::path::Path::new("/Users/a/.heimdall"))));
    assert_eq!(agent::notice::NoticeKind::classify(agent::notice::FIRST_RUN, &checked), agent::notice::NoticeKind::Scheduled);
    // A file from before the field reads as missing, not as the struct's default (serde's struct-level default).
    let (_t, data) = profile();
    for (file, greeted) in [(r#"{"setup_requested":true}"#, Some(true)), (r#"{"setup_requested":false}"#, Some(false))] {
        std::fs::write(data.join("settings.json"), file).unwrap();
        assert_eq!(Settings::load(&data.join("settings.json")).unwrap().first_run_greeted, greeted, "{file}");
    }
    assert_eq!(Settings::default().first_run_greeted, Some(false));
}

/// Setup ▸ Coding Agents through the real `Pipeline`: each choice is saved and survives the next launch. (A test
/// build has no installed CLI, so no harness file is touched; harness_autonomy_tests.rs covers the files.)
#[test]
fn the_coding_agents_choice_is_saved() {
    for choice in [false, true] {
        let (_t, data) = profile();
        let mut a = Pipeline::new(data.clone(), data.join("missing-models"), Settings::load_and_migrate(&data.join("settings.json")).unwrap(), None, false).unwrap();
        assert_eq!(a.settings.harness_autonomy, None);
        a.action(ui::UiAction::SetHarnessAutonomy(choice)).unwrap();
        assert_eq!(Settings::load(&data.join("settings.json")).unwrap().harness_autonomy, Some(choice));
    }
}
