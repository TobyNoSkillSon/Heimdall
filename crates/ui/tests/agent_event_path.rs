//! The Agent window and the island's question card through their real controls, offscreen: the demo builds the
//! window and the card without ordering either in (nothing reaches the screen, no permission), presses every
//! button through accessibility (its target/action), hit-tests their centres, sends with Return, clicks the Edit
//! link, and checks each emitted action: Send and Interrupt, Stop, Reload, Edit and resend, Completed / Abandon
//! for an interrupted wake, each question's own Yes and No, Connect T3, expiry, dismissal and clearing.

#[cfg(target_os = "macos")]
#[test]
fn agent_window_and_question_card_act_through_their_controls() {
    let dir = std::env::temp_dir().join(format!("heimdall-agent-checks-{}", std::process::id()));
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_heimdall-ui-demo"))
        .arg("--agent")
        .arg(&dir)
        .output()
        .expect("run heimdall-ui-demo");
    let log = String::from_utf8_lossy(&out.stderr);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(!log.contains("FAILED"), "{log}");
    assert!(log.lines().filter(|l| l.starts_with("check ")).count() >= 50, "{log}");
    assert!(out.status.success(), "{log}");
}
