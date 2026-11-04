//! The menu's rows under synthetic pointer events, offscreen (`heimdall-ui-demo --menu-rows`; night 1027 audit items
//! 12-14): a row dragged out of drops its highlight, Support the Developer is lit only under the pointer, and a model
//! added while the menu is open is in its Cloud section at once.

#[cfg(target_os = "macos")]
fn run(prefix: &str, at_least: usize) {
    let dir = std::env::temp_dir().join(format!("heimdall-menu-rows-{prefix}-{}", std::process::id()));
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_heimdall-ui-demo")).arg("--menu-rows").arg(&dir).output().expect("run heimdall-ui-demo");
    let log = String::from_utf8_lossy(&out.stderr);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(!log.lines().any(|l| l.starts_with(&format!("check FAILED {prefix}"))), "{log}");
    let passed = log.lines().filter(|l| l.starts_with(&format!("check ok {prefix}"))).count();
    assert!(passed >= at_least, "{passed} of {at_least} checks passed:\n{log}");
}

#[cfg(target_os = "macos")]
#[test]
fn a_row_left_mid_drag_drops_its_highlight() {
    run("a_row_left_mid_drag_drops_its_highlight", 7);
}

#[cfg(target_os = "macos")]
#[test]
fn prominent_row_highlight_needs_the_pointer() {
    run("prominent_row_highlight_needs_the_pointer", 6);
}

#[cfg(target_os = "macos")]
#[test]
fn cloud_submenu_gains_a_model_added_while_open() {
    run("cloud_submenu_gains_a_model_added_while_open", 9);
}

/// The public edition's menu has no Bug Notes row; the personal one keeps it (`--features public` for the public run).
#[cfg(target_os = "macos")]
#[test]
fn bug_notes_row_only_in_the_personal_menu() {
    run("bug_notes_row_only_in_the_personal_menu", 1);
}
