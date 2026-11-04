//! The home folder from the app's side (docs/home.md): fixing it at start, and changing it from the setup wizard or
//! settings. UI-free. A change is planned (`plan`, shown to the user), requested (`request`, written into the pointer),
//! then the app starts a fresh copy of itself (`relaunch`) and quits; the new copy moves the folder before it opens it.

use heimdall_home::{Applied, Env, Location, MovePlan, Source};
use std::path::{Path, PathBuf};

/// Where home and Heimdall's own files are for this run. `source` says whether a home was picked
/// (`needs_choice()`): `Fresh` and `Legacy` show the wizard's home step.
pub fn current() -> Location {
    heimdall_home::current().clone()
}

/// What moving home to `new_home` would do (size, same volume, the link left behind). Writes nothing.
pub fn plan(new_home: &Path) -> Result<MovePlan, String> {
    plan_in(&Env::current(), heimdall_home::current(), new_home)
}

/// Records the move for the next start (or just the new home when nothing exists yet). The caller then calls
/// `relaunch` and quits.
pub fn request(new_home: &Path) -> Result<MovePlan, String> {
    request_in(&Env::current(), heimdall_home::current(), new_home)
}

/// Keeps the home shown as the choice, with nothing moved (the setup wizard's Continue).
pub fn keep() -> Result<(), String> {
    keep_in(&Env::current(), heimdall_home::current())
}

pub fn plan_in(env: &Env, loc: &Location, new_home: &Path) -> Result<MovePlan, String> {
    heimdall_home::plan_move(env, loc, new_home)
}

pub fn request_in(env: &Env, loc: &Location, new_home: &Path) -> Result<MovePlan, String> {
    // A profile named at launch (`--data-dir`, `HEIMDALL_DATA`: lab runs, tests) is not the pointer's to move.
    if loc.source == Source::Env {
        return Err("This Heimdall was started on a named data folder; its home can't be changed here.".into());
    }
    let plan = plan_in(env, loc, new_home)?;
    if std::fs::symlink_metadata(&plan.from).is_ok() {
        heimdall_home::request_move(env, loc, &plan)?;
    } else {
        heimdall_home::set_home(env, &plan.to_home)?;
    }
    Ok(plan)
}

/// A fresh profile records its home; the personal edition's legacy profile records its home and its current data
/// folder, so nothing moves. A home already chosen, or a folder named at launch, records nothing.
pub fn keep_in(env: &Env, loc: &Location) -> Result<(), String> {
    match loc.source {
        Source::Fresh => heimdall_home::set_home(env, &loc.home),
        Source::Legacy => heimdall_home::write_pointer(env, &heimdall_home::Pointer { home: loc.home.clone(), data: Some(loc.data.clone()), pending: None }),
        Source::Env | Source::Pointer => Ok(()),
    }
}

/// Starts a fresh copy of the running app once this process has exited (`open -n`, through LaunchServices, so the
/// new copy reads the pointer rather than this process's environment). The caller quits right after. Errors when
/// this binary is not inside an app bundle.
pub fn relaunch() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let app = bundle_of(&exe).ok_or_else(|| format!("{} is not inside an app; start it again by hand.", exe.display()))?;
    let launch = ["/usr/bin/open".to_string(), "-n".into(), app.to_string_lossy().into_owned()];
    crate::watchdog::spawn_relaunch(std::process::id(), &launch).map_err(|e| format!("{e:#}"))
}

/// `X.app` for `X.app/Contents/MacOS/<exe>`.
fn bundle_of(exe: &Path) -> Option<PathBuf> {
    exe.ancestors().nth(3).filter(|p| p.extension().is_some_and(|e| e == "app")).map(Path::to_path_buf)
}

/// App start, single-threaded and before anything opens the data folder: performs a move asked for last run (only
/// the app on its own profile; never with `--data-dir` or an inherited `HEIMDALL_DATA`), fixes this process's
/// location (`explicit` wins) and exports it, so every child (agent, workers, CLIs, islands) uses the same folders;
/// in the public edition that includes the islands folder (`FLOATING_ISLANDS_DIR`).
/// Returns the move's outcome for the log, which is not open yet.
pub fn start(explicit: Option<&Path>) -> Option<Result<Option<Applied>, String>> {
    forget_inherited();
    let env = Env::current();
    let moved = (explicit.is_none() && env.data_override.is_none()).then(|| heimdall_home::apply_pending(&env));
    let mut loc = heimdall_home::resolve_in(&env);
    if let Some(data) = explicit {
        loc = Location { data: data.to_path_buf(), source: Source::Env, ..loc };
    }
    std::env::set_var("HEIMDALL_DATA", &loc.data);
    std::env::set_var("HEIMDALL_HOME", &loc.home);
    std::env::set_var(EXPORTED, "1");
    // The public edition's islands live in its own data folder; an inherited folder (a lab run) still wins.
    if let Some(dir) = crate::islands::live::edition_dir(crate::edition::PUBLIC, &loc.data) {
        if std::env::var_os("FLOATING_ISLANDS_DIR").is_none_or(|d| d.is_empty()) {
            std::env::set_var("FLOATING_ISLANDS_DIR", dir);
            std::env::set_var(EXPORTED_ISLANDS, "1");
        }
    }
    heimdall_home::init(loc);
    moved
}

/// Marks the folder variables this app exported for its children. A copy it relaunched (`open -n` keeps the
/// caller's environment, as do the updater's and the watchdog's restarts), or another Heimdall its agent opened,
/// inherits them; they name that run's folders, so the live app drops them before it reads anything. A
/// `HEIMDALL_DATA` set by hand or by a test carries no mark and still wins.
const EXPORTED: &str = "HEIMDALL_FOLDERS_EXPORTED";
const EXPORTED_ISLANDS: &str = "HEIMDALL_ISLANDS_EXPORTED";

/// Drops folder variables a Heimdall app exported (see `EXPORTED`). Live app start only; child modes keep them.
pub fn forget_inherited() {
    if std::env::var_os(EXPORTED).is_some() {
        for key in ["HEIMDALL_DATA", "HEIMDALL_HOME", EXPORTED] {
            std::env::remove_var(key);
        }
    }
    if std::env::var_os(EXPORTED_ISLANDS).is_some() {
        std::env::remove_var("FLOATING_ISLANDS_DIR");
        std::env::remove_var(EXPORTED_ISLANDS);
    }
}

/// The start's log lines: where Heimdall lives, and what a pending move did.
pub fn log_start(moved: Option<Result<Option<Applied>, String>>) {
    let loc = heimdall_home::current();
    eprintln!("Heimdall home: {} (data {}, {:?})", loc.home.display(), loc.data.display(), loc.source);
    match moved {
        Some(Ok(Some(applied))) => eprintln!("Heimdall home: moved: {applied:?}"),
        Some(Err(e)) => eprintln!("Heimdall home: the move did not happen; the files stay where they were: {e}"),
        _ => {}
    }
}

#[cfg(test)]
#[path = "home_change_tests.rs"]
mod tests;
