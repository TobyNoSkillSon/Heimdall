//! The nudge pill's landing jiggle (night 1022) is intended (the user's call, 27 Oct: keep it): the motion judge played
//! both nudge cards and failed them for "the drawn width reverses 5 times in one motion". It now leaves the jiggle's
//! own frames out of the reversal count; the spring's overshoot around it still counts.

#[cfg(target_os = "macos")]
#[test]
fn the_motion_judge_passes_the_nudge_jiggle() {
    let dir = std::env::temp_dir().join(format!("heimdall-nudge-judge-{}", std::process::id()));
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_heimdall-ui-demo"))
        .arg("--gallery")
        .arg(&dir)
        .args(["--only", "nudge_bell,nudge_question"])
        .output()
        .expect("run heimdall-ui-demo");
    let _ = std::fs::remove_dir_all(&dir);
    let log = format!("{}\n{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(!log.contains("FAILED"), "{log}");
    assert!(log.contains("nudge_bell") && log.contains("nudge_question"), "{log}");
    assert!(out.status.success(), "{log}");
}
