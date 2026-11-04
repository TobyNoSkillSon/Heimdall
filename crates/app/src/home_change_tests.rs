//! Home changes (docs/home.md) against temp folders: a stand-in user home, never the real pointer or profile.
use super::*;
use heimdall_home::{resolve_in, Env, Source, PERSONAL_ID, PUBLIC_ID};
use std::fs;

fn env(user: &Path, id: &str) -> Env {
    Env { user_home: user.to_path_buf(), bundle_id: id.into(), data_override: None, home_override: None }
}

#[test]
fn a_change_with_files_is_requested_for_the_next_start_and_moves_there() {
    let user = tempfile::tempdir().unwrap();
    let e = env(user.path(), PUBLIC_ID);
    let loc = resolve_in(&e);
    fs::create_dir_all(loc.data.join("logs")).unwrap();
    fs::write(loc.data.join("settings.json"), b"{}").unwrap();
    let notes = user.path().join("Notes");
    fs::create_dir(&notes).unwrap();
    let plan = plan_in(&e, &loc, &notes).unwrap();
    // The plan names the real folder (temp folders resolve through /private).
    assert_eq!((plan.from.clone(), plan.bytes), (fs::canonicalize(&loc.data).unwrap(), 2));
    assert!(!e.pointer_path().exists(), "planning writes nothing");
    let requested = request_in(&e, &loc, &notes).unwrap();
    assert_eq!(requested, plan);
    let pointer = heimdall_home::read_pointer(&e).unwrap();
    assert_eq!(pointer.pending.as_ref().map(|m| m.to_home.clone()), Some(plan.to_home.clone()));
    // Until the next start nothing has moved and this run keeps its folder.
    assert!(loc.data.join("settings.json").is_file());
    assert_eq!(resolve_in(&e).data, plan.from);
    heimdall_home::apply_pending(&e).unwrap();
    let after = resolve_in(&e);
    assert_eq!((after.home, after.data.clone(), after.source), (plan.to_home.clone(), plan.to_home.join(".heimdall"), Source::Pointer));
    assert!(after.data.join("settings.json").is_file() && !loc.data.exists());
}

#[test]
fn with_nothing_to_move_the_new_home_is_set_at_once() {
    let user = tempfile::tempdir().unwrap();
    let e = env(user.path(), PERSONAL_ID);
    let loc = resolve_in(&e);
    assert!(loc.needs_choice() && !loc.data.exists());
    let home = user.path().join("Home");
    fs::create_dir(&home).unwrap();
    let plan = request_in(&e, &loc, &home).unwrap();
    let pointer = heimdall_home::read_pointer(&e).unwrap();
    assert_eq!((pointer.home, pointer.pending), (plan.to_home.clone(), None));
    let after = resolve_in(&e);
    assert!(!after.needs_choice());
    assert_eq!((after.data, after.source), (plan.to_home.join(".heimdall"), Source::Pointer));
}

#[test]
fn a_profile_named_at_launch_is_never_moved_and_bad_folders_are_refused() {
    let user = tempfile::tempdir().unwrap();
    let mut e = env(user.path(), PUBLIC_ID);
    let lab = user.path().join("lab");
    fs::create_dir(&lab).unwrap();
    e.data_override = Some(lab.clone());
    let loc = resolve_in(&e);
    let home = user.path().join("Home");
    fs::create_dir(&home).unwrap();
    assert!(request_in(&e, &loc, &home).is_err());
    assert!(!e.pointer_path().exists(), "a lab run never writes the pointer");
    let e = env(user.path(), PUBLIC_ID);
    let loc = resolve_in(&e);
    assert!(request_in(&e, &loc, Path::new("relative")).is_err());
    assert!(request_in(&e, &loc, &user.path().join("missing")).is_err());
    assert!(!e.pointer_path().exists());
}

#[test]
fn relaunch_needs_an_app_bundle() {
    assert_eq!(bundle_of(Path::new("/A/Heimdall Alpha.app/Contents/MacOS/heimdall")), Some(PathBuf::from("/A/Heimdall Alpha.app")));
    assert_eq!(bundle_of(Path::new("/target/debug/heimdall")), None);
    // A test binary is not in a bundle: nothing is started.
    assert!(relaunch().is_err());
}

/// Review, 27 Oct: a relaunched copy (or another Heimdall the agent opened) inherits the folders this app exported;
/// they are dropped so the new copy applies its pending move and reads its own pointer. A hand-set folder stays.
#[test]
fn folders_this_app_exported_are_dropped_by_the_next_app_but_a_hand_set_folder_stays() {
    std::env::set_var("HEIMDALL_DATA", "/tmp/exported-data");
    std::env::set_var("HEIMDALL_HOME", "/tmp/exported-home");
    std::env::set_var("FLOATING_ISLANDS_DIR", "/tmp/exported-islands");
    std::env::set_var(super::EXPORTED, "1");
    std::env::set_var(super::EXPORTED_ISLANDS, "1");
    super::forget_inherited();
    for key in ["HEIMDALL_DATA", "HEIMDALL_HOME", "FLOATING_ISLANDS_DIR", super::EXPORTED, super::EXPORTED_ISLANDS] {
        assert!(std::env::var_os(key).is_none(), "{key} survived");
    }
    std::env::set_var("HEIMDALL_DATA", "/tmp/lab-data");
    super::forget_inherited();
    assert_eq!(std::env::var_os("HEIMDALL_DATA").as_deref(), Some(std::ffi::OsStr::new("/tmp/lab-data")));
    std::env::remove_var("HEIMDALL_DATA");
}
