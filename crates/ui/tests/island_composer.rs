//! The island chat's composer through its real views, offscreen (bug notes 2026-10-25T23-12-07,
//! "the typing doesn't work"): one placeholder, drawn by the island, none by the native field; typed words take
//! its place exactly; real key presses show in the field and hide it, backspace brings it back; ⌃⌘H opens the chat
//! with the field focused and pulls it up again; a long message scrolls on its one line.

#[cfg(target_os = "macos")]
#[test]
fn the_composer_has_one_placeholder_shows_typing_and_takes_the_keyboard_on_control_command_h() {
    let dir = std::env::temp_dir().join(format!("heimdall-composer-{}", std::process::id()));
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_heimdall-ui-demo")).arg("--composer").arg(&dir).output().expect("run heimdall-ui-demo");
    let report = std::fs::read_to_string(dir.join("composer-checks.txt")).unwrap_or_default();
    let _ = std::fs::remove_dir_all(&dir);
    let log = format!("{report}\n{}", String::from_utf8_lossy(&out.stderr));
    assert!(!log.contains("FAILED"), "{log}");
    assert!(report.lines().filter(|l| l.starts_with("check ok")).count() >= 25, "{log}");
    assert!(out.status.success(), "{log}");
}
