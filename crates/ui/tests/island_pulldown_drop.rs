//! A drop on the open pull-down (30 Oct 03:00, the user: "With the pull-down open, I can't drop an image/screenshot onto
//! it"), offscreen through the real island, its click target and composer field, and the drop target's catcher: the
//! click target lets an armed drag through; an empty composer sends the drop like a notch drop; while he types it
//! attaches, shows in the composer, goes with Return, and its badge removes it.

#[cfg(target_os = "macos")]
#[test]
fn a_drop_on_the_open_pull_down_attaches_to_the_message_or_goes_like_a_notch_drop() {
    let dir = std::env::temp_dir().join(format!("heimdall-pulldown-drop-{}", std::process::id()));
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_heimdall-ui-demo")).arg("--pulldown-drop").arg(&dir).output().expect("run heimdall-ui-demo");
    let report = std::fs::read_to_string(dir.join("pulldown-drop-checks.txt")).unwrap_or_default();
    let _ = std::fs::remove_dir_all(&dir);
    let log = format!("{report}\n{}", String::from_utf8_lossy(&out.stderr));
    assert!(!log.contains("FAILED") && !log.contains("check FAIL"), "{log}");
    assert!(report.lines().filter(|l| l.starts_with("check ok")).count() >= 24, "{log}");
    assert!(out.status.success(), "{log}");
}
