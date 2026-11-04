//! First run on a fresh profile, against an isolated home with each harness as a fresh install leaves it: what Setup's
//! Coding Agents step shows, then each choice (Leave as Is, Set Up) and the next start, in both editions.
use super::*;
use heimdall_emergency::harness::Drift;
use std::fs;

/// A Mac where every harness was just installed and asks before acting: a Claude Code folder with an `ask` rule, T3
/// Code with its default Claude instance, Codex on `on-request`, OpenCode at its defaults, Grok with no config yet.
fn fresh_mac(user: &Path) {
    fs::create_dir_all(user.join(".claude")).unwrap();
    fs::write(user.join(".claude/settings.json"), r#"{"theme":"dark","permissions":{"ask":["Bash(git push *)"]}}"#).unwrap();
    fs::create_dir_all(user.join(".t3/userdata")).unwrap();
    fs::write(user.join(".t3/userdata/settings.json"), r#"{"providerInstances":{"claudeAgent":{"driver":"claudeAgent"}}}"#).unwrap();
    fs::create_dir_all(user.join(".codex")).unwrap();
    fs::write(user.join(".codex/config.toml"), "model = \"x\"\napproval_policy = \"on-request\"\n").unwrap();
    fs::create_dir_all(user.join(".config/opencode")).unwrap();
    fs::write(user.join(".config/opencode/opencode.json"), r#"{"model":"m"}"#).unwrap();
    fs::create_dir_all(user.join(".grok/bin")).unwrap();
}

/// Every file under `dir` with its bytes, to prove nothing was touched.
fn snapshot(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut out = vec![];
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() { stack.push(p) } else { out.push((p.clone(), fs::read(&p).unwrap())) }
        }
    }
    out.sort();
    out
}

const HARNESSES: [&str; 5] = ["Claude Code", "T3 Code", "Codex", "OpenCode", "Grok"];

struct Mac { _t: tempfile::TempDir, user: PathBuf, data: PathBuf, cli: PathBuf }

fn mac() -> Mac {
    let t = tempfile::tempdir().unwrap();
    let user = t.path().join("home");
    let data = user.join(".heimdall");
    fs::create_dir_all(&data).unwrap();
    fresh_mac(&user);
    let cli = t.path().join("Heimdall Alpha.app/Contents/MacOS/heimdall-emergency");
    fs::create_dir_all(cli.parent().unwrap()).unwrap();
    fs::write(&cli, "").unwrap();
    Mac { _t: t, user, data, cli }
}

#[test]
fn first_run_shows_each_harness_that_asks_and_the_deny_list() {
    let m = mac();
    assert_eq!(crate::settings::Settings::default().harness_autonomy, None, "a fresh profile has not chosen");
    let step = setup_state(&m.user, &m.data, Some(&m.cli), None);
    let names: Vec<&str> = step.rows.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(names, HARNESSES);
    assert!(step.rows.iter().all(|r| !r.ready) && step.pending());
    // The deny list holds for Claude agents only: the shield marks Claude Code and T3 Code, not the others.
    let guarded: Vec<&str> = step.rows.iter().filter(|r| r.guarded).map(|r| r.name.as_str()).collect();
    assert_eq!(guarded, ["Claude Code", "T3 Code"]);
    assert_eq!(step.chosen, None);
    assert_eq!(step.deny, ["No deletes outside your home folder", "No disk erasing", "No Keychain access", "No system settings"]);
    // Nothing installed: no rows, nothing to set.
    let empty = tempfile::tempdir().unwrap();
    let none = setup_state(empty.path(), &m.data, Some(&m.cli), None);
    assert!(none.rows.is_empty() && !none.pending());
    // A config the check cannot read (comments in OpenCode's .jsonc) is not shown as set.
    fs::remove_file(m.user.join(".config/opencode/opencode.json")).unwrap();
    fs::write(m.user.join(".config/opencode/opencode.jsonc"), "// mine\n{\"permission\": {\"*\": \"allow\", \"question\": \"deny\"}}").unwrap();
    pass(true, Some(true), &m.user, &m.data, Some(&m.cli)).unwrap();
    let after = setup_state(&m.user, &m.data, Some(&m.cli), Some(true));
    assert!(after.rows.iter().any(|r| r.name == "OpenCode" && !r.ready) && after.pending(), "{:?}", after.rows);
    assert!(after.rows.iter().filter(|r| r.name != "OpenCode").all(|r| r.ready));
}

#[test]
fn leave_as_is_and_no_choice_yet_touch_nothing_in_the_public_edition() {
    let m = mac();
    let before = snapshot(&m.user);
    // T3's Connect comes before the step: no choice yet, nothing set.
    assert!(pass(true, None, &m.user, &m.data, Some(&m.cli)).is_none());
    // Leave as Is, then every later start.
    for _start in 0..2 { assert!(pass(true, Some(false), &m.user, &m.data, Some(&m.cli)).is_none()); }
    assert_eq!(snapshot(&m.user), before, "no harness file changed and no backup was made");
    assert!(!m.data.join("harness-backups").exists());
    let step = setup_state(&m.user, &m.data, Some(&m.cli), Some(false));
    assert!(step.rows.iter().all(|r| !r.ready) && step.chosen == Some(false), "Setup reopened shows the choice kept");
    // The first-run prompt asks for no check.
    let text = crate::setup_wizard::greeting(true, wanted(true, Some(false)).then_some((m.cli.as_path(), m.data.as_path())));
    assert!(!text.contains("harness check") && text.contains("`onboarding`"), "{text}");
}

#[test]
fn set_up_sets_every_harness_backs_each_file_up_and_the_next_start_finds_nothing() {
    let m = mac();
    let report = pass(true, Some(true), &m.user, &m.data, Some(&m.cli)).expect("Set Up applies");
    // The only error allowed is a Mac's own managed Claude settings, which no user setting can override.
    assert!(report.errors.iter().all(|e| e.contains("managed-settings")), "{}", report.lines());
    assert_eq!(report.harnesses(), HARNESSES.to_vec(), "{}", report.lines());
    assert!(report.drift.iter().all(|d: &Drift| d.fixed));
    assert_eq!(notice_text(&report).as_deref(), Some("No-prompt mode restored · Claude Code, T3 Code, Codex, OpenCode, Grok"));
    // The same settings the startup check keeps (harness.rs), on the real files.
    let claude: serde_json::Value = serde_json::from_str(&fs::read_to_string(m.user.join(".claude/settings.json")).unwrap()).unwrap();
    assert_eq!((claude["permissions"]["defaultMode"].as_str(), claude["theme"].as_str()), (Some("bypassPermissions"), Some("dark")));
    let hook = claude["hooks"]["PermissionRequest"][0]["hooks"][0]["command"].as_str().unwrap();
    assert!(hook.contains("heimdall-emergency' hook permission") && hook.contains(&m.data.display().to_string()), "{hook}");
    assert!(fs::read_to_string(m.user.join(".codex/config.toml")).unwrap().contains("approval_policy = \"never\""));
    assert!(fs::read_to_string(m.user.join(".grok/config.toml")).unwrap().contains("always-approve"));
    // Each file that existed was copied before its rewrite (Grok had none to copy).
    let backups: Vec<String> = fs::read_dir(m.data.join("harness-backups")).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    for prefix in ["claude-code-settings.json.", "t3-code-settings.json.", "codex-config.toml.", "opencode-opencode.json."] {
        assert!(backups.iter().any(|b| b.starts_with(prefix)), "{prefix} in {backups:?}");
    }
    let saved = fs::read_dir(m.data.join("harness-backups")).unwrap().flatten().find(|e| e.file_name().to_string_lossy().starts_with("codex-config")).unwrap();
    assert_eq!(fs::read_to_string(saved.path()).unwrap(), "model = \"x\"\napproval_policy = \"on-request\"\n", "the backup is the original");
    // The step now shows every one as set; Continue sends nothing more.
    let step = setup_state(&m.user, &m.data, Some(&m.cli), Some(true));
    assert!(step.rows.iter().all(|r| r.ready) && !step.pending(), "{:?}", step.rows);
    // The next start: nothing drifted, no line, no new backup.
    let count = backups.len();
    let again = pass(true, Some(true), &m.user, &m.data, Some(&m.cli)).unwrap();
    assert!(again.drift.is_empty() && again.errors.iter().all(|e| e.contains("managed-settings")), "{}", again.lines());
    assert_eq!(notice_text(&again), None);
    assert_eq!(fs::read_dir(m.data.join("harness-backups")).unwrap().count(), count);
    // An update sets Codex back: the start after it restores only that.
    fs::write(m.user.join(".codex/config.toml"), "model = \"x\"\napproval_policy = \"untrusted\"\nsandbox_mode = \"danger-full-access\"\n").unwrap();
    let restored = pass(true, Some(true), &m.user, &m.data, Some(&m.cli)).unwrap();
    assert_eq!(notice_text(&restored).as_deref(), Some("No-prompt mode restored · Codex"));
    // The first-run prompt asks the agent to verify it once, with the installed CLI.
    let text = crate::setup_wizard::greeting(true, wanted(true, Some(true)).then_some((m.cli.as_path(), m.data.as_path())));
    assert!(text.contains("harness check") && text.contains("heimdall-emergency") && text.contains("one sentence"), "{text}");
    assert!(text.contains(&format!("--data-dir '{}' harness check", m.data.display())), "the check reads this profile's hooks: {text}");
    assert!(text.contains("Codex, OpenCode and Grok are not guarded"), "{text}");
}

#[test]
fn the_personal_edition_sets_them_without_asking_and_a_lab_build_never_writes() {
    let m = mac();
    // A lab or test build has no installed CLI: nothing is written in either edition.
    let before = snapshot(&m.user);
    assert!(pass(false, None, &m.user, &m.data, None).is_none());
    assert!(pass(true, Some(true), &m.user, &m.data, None).is_none());
    assert_eq!(snapshot(&m.user), before);
    // The user's install: no step, no choice saved, set at once.
    assert!(wanted(false, None) && wanted(false, Some(false)));
    let report = pass(false, None, &m.user, &m.data, Some(&m.cli)).unwrap();
    assert_eq!(report.harnesses(), HARNESSES.to_vec());
    assert!(!crate::setup_wizard::greeting(false, Some((&m.cli, &m.data))).contains("`onboarding`"));
}

#[test]
fn one_line_names_what_was_set_back() {
    let d = |harness, fixed| Drift { harness, file: PathBuf::from("/x"), what: "w".into(), fixed };
    let r = Report { drift: vec![d("Claude Code", true), d("Claude Code", true), d("Codex", true), d("Grok", false)], ..Default::default() };
    assert_eq!(notice_text(&r).as_deref(), Some("No-prompt mode restored · Claude Code, Codex"));
    assert_eq!(notice_text(&Report::default()), None);
}

/// Fresh review, 3 Nov: only an app in an Applications folder points the hooks at itself; a candidate in dist/ or a
/// lab bundle never does, even started with the real home.
#[test]
fn only_an_installed_bundle_owns_the_hooks() {
    let home = Path::new("/Users/t");
    assert!(installed_bundle(Path::new("/Users/t/Applications/Heimdall.app/Contents/MacOS/Heimdall"), home));
    assert!(installed_bundle(Path::new("/Applications/Heimdall.app/Contents/MacOS/Heimdall"), home));
    assert!(!installed_bundle(Path::new("/Users/t/Work/Projects/Heimdall/dist/Heimdall.app/Contents/MacOS/Heimdall"), home));
    assert!(!installed_bundle(Path::new("/Users/t/Applications/lab/Heimdall.app/Contents/MacOS/Heimdall"), home));
    assert!(!installed_bundle(Path::new("/Users/t/Work/Projects/Heimdall/target/debug/heimdall"), home));
}
