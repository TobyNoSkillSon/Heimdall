//! Every agent harness stays in its no-prompt mode (docs/harness-autonomy.md). At each start, and
//! when setup finishes or T3 is connected, `heimdall_emergency::harness::ensure` checks Claude Code (every config
//! folder), T3 Code's Claude launch variable, Codex, OpenCode and Grok, and writes back whatever an update or a
//! settings screen changed. Anything set back is one island line naming the harnesses; nothing drifted, nothing shown.
//!
//! The personal edition always does it (the user decided, 2 Nov 23:20: "make sure it's set up that way for users"). The
//! public edition asks once, in Setup's Coding Agents step (`Settings::harness_autonomy`): Set Up sets them and keeps
//! them so at each start; Leave as Is never touches them. Only the installed app writes (an app bundle with its
//! `heimdall-emergency` beside it): a lab or test build never touches the harnesses' real settings, and the hooks
//! always point at the installed CLI.
use heimdall_emergency::harness::{self, Report, Setup};
use std::path::{Path, PathBuf};

/// The island line's id: one line however many harnesses drifted.
const NOTICE: &str = "harness-autonomy";

/// The bundled CLI beside this app's binary, when this is an installed bundle.
pub fn bundled_cli() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let home = PathBuf::from(std::env::var_os("HOME")?);
    if !installed_bundle(&exe, &home) { return None; }
    exe.parent().map(|p| p.join("heimdall-emergency")).filter(|p| p.exists())
}

/// An app binary in an Applications folder (`~/Applications` or `/Applications`), the only places Heimdall installs.
/// A candidate in `dist/` or a lab bundle started with the real home never points the hooks at itself (fresh review,
/// 3 Nov: deleted, it would leave every Claude session's hooks failing until the installed app started again).
pub fn installed_bundle(exe: &Path, home: &Path) -> bool {
    let s = exe.to_string_lossy();
    let parent_ok = [home.join("Applications"), PathBuf::from("/Applications")].iter()
        .any(|apps| exe.parent().and_then(Path::parent).and_then(Path::parent).and_then(Path::parent) == Some(apps.as_path()));
    s.contains(".app/Contents/MacOS/") && parent_ok
}

/// Whether this install sets the harnesses: the personal edition always; the public edition after Set Up only.
pub fn wanted(public: bool, choice: Option<bool>) -> bool {
    !public || choice == Some(true)
}

fn setup<'a>(user: &'a Path, data: &Path, cli: &Path, backups: &'a Path) -> Setup<'a> {
    Setup { user, hook_command: Some(heimdall_emergency::approvals::hook_command(cli, data)), backups: Some(backups) }
}

/// One pass over `user`'s harnesses, the same at first run and at every start: None when nothing may be touched (not
/// wanted, or no installed CLI for the hooks); else what was found and set back. Each file is backed up under
/// `<data>/harness-backups` before its first rewrite.
pub fn pass(public: bool, choice: Option<bool>, user: &Path, data: &Path, cli: Option<&Path>) -> Option<Report> {
    if !wanted(public, choice) { return None; }
    let backups = data.join("harness-backups");
    Some(harness::ensure(&setup(user, data, cli?, &backups), true))
}

/// Setup's Coding Agents step: each harness found, whether it already works without asking and whether the deny list
/// guards it, the deny list, and the saved choice. Reads only. Without the installed CLI (a lab build) the hooks are not
/// judged.
pub fn setup_state(user: &Path, data: &Path, cli: Option<&Path>, chosen: Option<bool>) -> ui::HarnessSetup {
    let backups = data.join("harness-backups");
    let s = match cli {
        Some(cli) => setup(user, data, cli, &backups),
        None => Setup { user, hook_command: None, backups: None },
    };
    ui::HarnessSetup {
        rows: harness::status(&s).into_iter().map(|(h, ready)| ui::HarnessRow { name: h.to_string(), ready, guarded: harness::GUARDED.contains(&h) }).collect(),
        deny: heimdall_emergency::deny::SUMMARY.iter().map(|s| s.to_string()).collect(),
        chosen,
    }
}

/// The island line for what was set back, or None when nothing was.
pub fn notice_text(report: &Report) -> Option<String> {
    let fixed: Vec<&str> = report.harnesses().into_iter().filter(|h| report.drift.iter().any(|d| d.harness == *h && d.fixed)).collect();
    (!fixed.is_empty()).then(|| format!("No-prompt mode restored · {}", fixed.join(", ")))
}

fn user_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Setup's Coding Agents step as this Mac is now.
pub fn current(data: &Path, chosen: Option<bool>) -> ui::HarnessSetup {
    user_home().map(|u| setup_state(&u, data, bundled_cli().as_deref(), chosen)).unwrap_or_default()
}

/// Checks and re-applies on its own thread; nothing waits for it. `choice` is `Settings::harness_autonomy`. Setup's
/// step is refreshed afterwards. `announce`: the island line for what was set back (not for Set Up itself, where
/// nothing was "restored").
pub fn start(data: &Path, choice: Option<bool>, announce: bool) {
    let public = crate::edition::PUBLIC;
    if !wanted(public, choice) { return; }
    let Some(cli) = bundled_cli() else { return };
    let data = data.to_path_buf();
    let _ = std::thread::Builder::new().name("harness-autonomy".into()).spawn(move || {
        let Some(user) = user_home() else { return };
        let Some(report) = pass(public, choice, &user, &data, Some(&cli)) else { return };
        if !report.drift.is_empty() || !report.errors.is_empty() { eprintln!("harness autonomy:\n{}", report.lines()); }
        let ui = ui::Ui::handle();
        if let Some(text) = notice_text(&report).filter(|_| announce) {
            ui.send(ui::UiUpdate::AgentNotice(ui::agent_notice::AgentNotice::line(NOTICE, text, ui::agent_notice::NoticeKind::Info)));
        }
        ui.send(ui::UiUpdate::HarnessSetup(setup_state(&user, &data, Some(&cli), choice)));
    });
}

#[cfg(test)]
#[path = "harness_autonomy_tests.rs"]
mod tests;
