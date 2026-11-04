//! The Bug Notes window's cards offscreen: every state renders, the cards lay out without overlap, and a click on a
//! card's evidence row (through the window's own event path) opens it; a second closes it; a refresh keeps it open.

#[cfg(target_os = "macos")]
#[test]
fn bug_notes_cards_render_and_open_their_evidence_on_a_click() {
    let dir = std::env::temp_dir().join(format!("heimdall-bug-notes-{}", std::process::id()));
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_heimdall-ui-demo")).arg("--bug-notes").arg(&dir).output().expect("run heimdall-ui-demo");
    let log = String::from_utf8_lossy(&out.stderr);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(!log.contains("FAILED"), "{log}");
    assert!(log.lines().filter(|l| l.starts_with("check ")).count() >= 11, "{log}");
    assert!(out.status.success(), "{log}");
}
