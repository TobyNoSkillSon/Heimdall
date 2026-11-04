//! The menu stays open on a tile, Launch at Login or a submenu radio click, through AppKit's own event path: the
//! demo opens the real status menu, posts mouse events into its window's queue (the cursor never moves), and
//! reads the state back while the menu tracks; arrow keys then highlight Launch at Login like a native row.
//! Each sequence gets up to three attempts, since outside input on a Mac in use can close a popped-up menu.

#[test]
#[ignore = "opens the real menu on screen for a few seconds (needs a logged-in GUI session); agent-run"]
fn menu_stays_open_on_tile_row_and_submenu_clicks() {
    let dir = std::env::temp_dir().join(format!("heimdall-menu-checks-{}", std::process::id()));
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_heimdall-ui-demo"))
        .arg("--checks")
        .arg(&dir)
        .output()
        .expect("run heimdall-ui-demo");
    let log = String::from_utf8_lossy(&out.stderr);
    let _ = std::fs::remove_dir_all(&dir);
    for sequence in 1..=3 {
        assert!(
            log.lines().any(|l| l.starts_with(&format!("checks {sequence}: passed"))),
            "sequence {sequence} did not pass:\n{log}"
        );
    }
    assert!(out.status.success(), "{log}");
}
