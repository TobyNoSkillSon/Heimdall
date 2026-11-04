use super::*;
use tempfile::TempDir;

fn env(user: &TempDir, id: &str) -> Env {
    Env { user_home: user.path().to_path_buf(), bundle_id: id.into(), data_override: None, home_override: None }
}

#[test]
fn a_blank_public_profile_lives_in_the_users_home_until_a_folder_is_picked() {
    let user = TempDir::new().unwrap();
    // Even with a personal profile on the same Mac, the public edition never reads it.
    fs::create_dir_all(user.path().join("Library/Application Support/Heimdall")).unwrap();
    let loc = resolve_in(&env(&user, PUBLIC_ID));
    assert_eq!((loc.home.as_path(), loc.data.clone(), loc.source), (user.path(), user.path().join(".heimdall"), Source::Fresh));
    assert!(loc.needs_choice());
}

#[test]
fn the_personal_legacy_profile_is_used_in_place_and_never_moved_by_resolving() {
    let user = TempDir::new().unwrap();
    let legacy = user.path().join("Library/Application Support/Heimdall");
    fs::create_dir_all(&legacy).unwrap();
    fs::create_dir(user.path().join("Vault")).unwrap();
    let loc = resolve_in(&env(&user, PERSONAL_ID));
    assert_eq!((loc.data.clone(), loc.home.clone(), loc.source), (legacy.clone(), user.path().join("Vault"), Source::Legacy));
    assert!(legacy.is_dir());
}

#[test]
fn the_environment_wins_so_children_and_tests_share_one_folder() {
    let user = TempDir::new().unwrap();
    let mut e = env(&user, PUBLIC_ID);
    e.data_override = Some("/tmp/d".into());
    e.home_override = Some("/tmp/h".into());
    let loc = resolve_in(&e);
    assert_eq!((loc.home, loc.data, loc.source), ("/tmp/h".into(), "/tmp/d".into(), Source::Env));
}

#[test]
fn editions_keep_separate_pointers() {
    let user = TempDir::new().unwrap();
    assert_ne!(env(&user, PUBLIC_ID).pointer_path(), env(&user, PERSONAL_ID).pointer_path());
    set_home(&env(&user, PUBLIC_ID), Path::new("/Users/someone/Notes")).unwrap();
    assert_eq!(resolve_in(&env(&user, PUBLIC_ID)).data, PathBuf::from("/Users/someone/Notes/.heimdall"));
    assert_eq!(resolve_in(&env(&user, PERSONAL_ID)).source, Source::Fresh);
}

#[test]
fn a_move_waits_for_the_next_start_and_is_one_rename_on_the_same_volume() {
    let user = TempDir::new().unwrap();
    let e = env(&user, PUBLIC_ID);
    let old = resolve_in(&e);
    fs::create_dir_all(old.data.join("logs")).unwrap();
    fs::write(old.data.join("settings.json"), b"{}").unwrap();
    let home = user.path().join("Notes");
    fs::create_dir(&home).unwrap();
    let plan = plan_move(&e, &old, &home).unwrap();
    assert!(plan.same_volume);
    assert_eq!(plan.bytes, 2);
    assert!(!plan.leave_link);
    request_move(&e, &old, &plan).unwrap();
    // Before the restart the files have not moved and resolving still finds them.
    let waiting = resolve_in(&e);
    assert_eq!((waiting.data.clone(), waiting.home.clone()), (plan.from.clone(), old.home.clone()));
    assert!(old.data.join("settings.json").is_file());
    let applied = apply_pending(&e).unwrap().unwrap();
    let to = fs::canonicalize(&home).unwrap().join(".heimdall");
    assert_eq!(applied, Applied::Renamed { from: plan.from.clone(), to: to.clone() });
    assert!(to.join("settings.json").is_file() && !old.data.exists());
    let now = resolve_in(&e);
    assert_eq!((now.data, now.source), (to, Source::Pointer));
    assert_eq!(apply_pending(&e).unwrap(), None, "applied once");
}

#[test]
fn the_legacy_profile_leaves_a_link_behind_for_tools_that_name_the_old_folder() {
    let user = TempDir::new().unwrap();
    let e = env(&user, PERSONAL_ID);
    let legacy = e.legacy_data();
    fs::create_dir_all(legacy.join("archive")).unwrap();
    fs::write(legacy.join("archive/a"), b"abc").unwrap();
    let vault = user.path().join("Vault");
    fs::create_dir(&vault).unwrap();
    let old = resolve_in(&e);
    let plan = plan_move(&e, &old, &vault).unwrap();
    assert!(plan.leave_link);
    request_move(&e, &old, &plan).unwrap();
    apply_pending(&e).unwrap();
    let to = fs::canonicalize(&vault).unwrap().join(".heimdall");
    assert_eq!(fs::read(legacy.join("archive/a")).unwrap(), b"abc", "the old path still reads through the link");
    assert!(fs::symlink_metadata(&legacy).unwrap().file_type().is_symlink());
    assert_eq!(resolve_in(&e).data, to);
}

#[test]
fn a_move_into_an_existing_heimdall_folder_is_refused_and_nothing_moves() {
    let user = TempDir::new().unwrap();
    let e = env(&user, PUBLIC_ID);
    let old = resolve_in(&e);
    fs::create_dir_all(&old.data).unwrap();
    fs::write(old.data.join("x"), b"1").unwrap();
    let home = user.path().join("Other");
    fs::create_dir_all(home.join(".heimdall")).unwrap();
    fs::write(home.join(".heimdall/y"), b"2").unwrap();
    assert!(plan_move(&e, &old, &home).unwrap_err().contains("already has"));
    // A folder that appears between the request and the start is refused at the start too.
    fs::remove_file(home.join(".heimdall/y")).unwrap();
    let plan = plan_move(&e, &old, &home).unwrap();
    request_move(&e, &old, &plan).unwrap();
    fs::write(home.join(".heimdall/y"), b"2").unwrap();
    assert!(apply_pending(&e).is_err());
    assert!(old.data.join("x").is_file());
    let after = read_pointer(&e).unwrap();
    assert_eq!((after.pending, after.data), (None, Some(plan.from.clone())), "the pending move is dropped, the files stay found");
    assert_eq!(resolve_in(&e).data, plan.from);
}

#[test]
fn a_move_interrupted_after_the_rename_finishes_at_the_next_start() {
    let user = TempDir::new().unwrap();
    let e = env(&user, PUBLIC_ID);
    let old = resolve_in(&e);
    fs::create_dir_all(&old.data).unwrap();
    let home = user.path().join("Notes");
    fs::create_dir(&home).unwrap();
    let plan = plan_move(&e, &old, &home).unwrap();
    request_move(&e, &old, &plan).unwrap();
    fs::rename(&old.data, &plan.to).unwrap(); // the crash: files moved, pointer not yet written
    assert_eq!(apply_pending(&e).unwrap(), Some(Applied::Finished { to: plan.to.clone() }));
    assert_eq!(resolve_in(&e).data, plan.to);
}

#[test]
fn home_inside_heimdalls_own_folder_or_a_missing_folder_is_refused() {
    let user = TempDir::new().unwrap();
    let e = env(&user, PUBLIC_ID);
    let old = resolve_in(&e);
    fs::create_dir_all(old.data.join("inner")).unwrap();
    assert!(plan_move(&e, &old, &old.data.join("inner")).is_err());
    assert!(plan_move(&e, &old, &user.path().join("missing")).is_err());
    assert!(plan_move(&e, &old, Path::new("relative")).is_err());
}

#[test]
fn copy_tree_keeps_bytes_links_and_counts() {
    let a = TempDir::new().unwrap();
    let src = a.path().join("src");
    fs::create_dir_all(src.join("d")).unwrap();
    fs::write(src.join("d/f"), vec![7u8; 1000]).unwrap();
    std::os::unix::fs::symlink("d/f", src.join("l")).unwrap();
    let dst = a.path().join("dst");
    assert_eq!(copy_tree(&src, &dst).unwrap(), 1000);
    assert_eq!(tree_stats(&src).unwrap(), tree_stats(&dst).unwrap());
    assert!(fs::symlink_metadata(dst.join("l")).unwrap().file_type().is_symlink());
}

#[test]
fn the_bundle_id_is_read_from_info_plist() {
    let plist = format!("<dict>\n\t<key>CFBundleIdentifier</key>\n\t<string>{PUBLIC_ID}</string>\n</dict>");
    assert_eq!(plist_string(&plist, "CFBundleIdentifier").as_deref(), Some(PUBLIC_ID));
}

/// Review, 27 Oct: a data folder that is a link moves the folder it points at; nothing is left behind the link.
#[test]
fn a_linked_data_folder_moves_the_real_folder() {
    let user = TempDir::new().unwrap();
    let e = env(&user, PUBLIC_ID);
    let real = user.path().join("Elsewhere/heimdall-files");
    fs::create_dir_all(&real).unwrap();
    fs::write(real.join("settings.json"), b"{}").unwrap();
    std::os::unix::fs::symlink(&real, user.path().join(".heimdall")).unwrap();
    let old = resolve_in(&e);
    let home = user.path().join("Notes");
    fs::create_dir(&home).unwrap();
    let plan = plan_move(&e, &old, &home).unwrap();
    request_move(&e, &old, &plan).unwrap();
    assert!(matches!(apply_pending(&e).unwrap(), Some(Applied::Renamed { .. })));
    assert!(plan.to.join("settings.json").is_file());
}

/// Review, 27 Oct: a second move after a legacy move keeps the old path's link pointing at the files.
#[test]
fn a_second_move_repoints_the_legacy_link() {
    let user = TempDir::new().unwrap();
    let e = env(&user, PERSONAL_ID);
    fs::create_dir_all(e.legacy_data()).unwrap();
    fs::write(e.legacy_data().join("a"), b"1").unwrap();
    for name in ["Vault", "Notes"] {
        fs::create_dir(user.path().join(name)).unwrap();
        let old = resolve_in(&e);
        let plan = plan_move(&e, &old, &user.path().join(name)).unwrap();
        request_move(&e, &old, &plan).unwrap();
        apply_pending(&e).unwrap();
    }
    assert_eq!(fs::read(e.legacy_data().join("a")).unwrap(), b"1");
    assert!(fs::read_link(e.legacy_data()).unwrap().ends_with("Notes/.heimdall"));
}

/// Review, 27 Oct: when the pointer could not be rewritten after the files moved, resolving finds them at the new place.
#[test]
fn files_that_moved_before_the_pointer_was_rewritten_are_found() {
    let user = TempDir::new().unwrap();
    let e = env(&user, PUBLIC_ID);
    let old = resolve_in(&e);
    fs::create_dir_all(&old.data).unwrap();
    let home = user.path().join("Notes");
    fs::create_dir(&home).unwrap();
    let plan = plan_move(&e, &old, &home).unwrap();
    request_move(&e, &old, &plan).unwrap();
    fs::rename(&plan.from, &plan.to).unwrap();
    assert_eq!(resolve_in(&e).data, plan.to);
}

/// A cargo test binary that names no folder gets its own, never the user's profile (31 Oct, the archive lock stall).
#[test]
fn a_test_binary_never_resolves_to_the_users_profile() {
    assert!(test_folder(Path::new("/x/target/debug/deps/heimdall-0123abcd")).is_some());
    assert!(test_folder(Path::new("/x/target/test/debug/deps/app-0123abcd")).is_some());
    assert!(test_folder(Path::new("/Users/t/Applications/Heimdall.app/Contents/MacOS/Heimdall")).is_none());
    assert!(test_folder(Path::new("/x/target/debug/heimdall-ui-demo")).is_none(), "lab binaries keep their own rules");
    if std::env::var_os("HEIMDALL_DATA").is_none() {
        let env = Env::current();
        let data = env.data_override.clone().expect("this test binary has a folder of its own");
        assert!(data.starts_with(std::env::temp_dir()), "{}", data.display());
        assert!(!data.starts_with(env.legacy_data()) && !data.starts_with(env.user_home.join(DIR)));
    }
}
