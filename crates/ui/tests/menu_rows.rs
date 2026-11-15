//! The menu's rows under synthetic pointer events, offscreen (`heimdall-ui-demo --menu-rows`; night 1027 audit items
//! 12-14): a row dragged out of drops its highlight, Support the Developer is lit only under the pointer, and a model
//! added while the menu is open is in its Cloud section at once.

/// Every check of one `--menu-rows` run (the demo runs them all, about 2 s, so once), with the least each must pass:
/// a row left mid-drag drops its highlight; the prominent row's highlight needs the pointer; the Cloud submenu gains a
/// model added while open; the Bug Notes row is only in the personal menu (`--features public` for the public run).
const CHECKS: [(&str, usize); 4] = [
    ("a_row_left_mid_drag_drops_its_highlight", 7),
    ("prominent_row_highlight_needs_the_pointer", 6),
    ("cloud_submenu_gains_a_model_added_while_open", 9),
    ("bug_notes_row_only_in_the_personal_menu", 1),
];

#[cfg(target_os = "macos")]
#[test]
fn menu_rows_under_synthetic_pointer_events() {
    let dir = std::env::temp_dir().join(format!("heimdall-menu-rows-{}", std::process::id()));
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_heimdall-ui-demo")).arg("--menu-rows").arg(&dir).output().expect("run heimdall-ui-demo");
    let log = String::from_utf8_lossy(&out.stderr);
    let _ = std::fs::remove_dir_all(&dir);
    let mut failures = vec![];
    for (prefix, at_least) in CHECKS {
        if log.lines().any(|l| l.starts_with(&format!("check FAILED {prefix}"))) {
            failures.push(format!("{prefix}: a check FAILED"));
        }
        let passed = log.lines().filter(|l| l.starts_with(&format!("check ok {prefix}"))).count();
        if passed < at_least {
            failures.push(format!("{prefix}: {passed} of {at_least} checks passed"));
        }
    }
    assert!(failures.is_empty(), "{}\n{log}", failures.join("\n"));
}
