//! heimdall-install-tool, shipped in `Heimdall Alpha.app/Contents/Helpers/`.
//!
//!   update   --plan <plan.json>                    the app's hand-off: wait for it to quit, swap, relaunch, check
//!                                                  readiness, roll back on failure (heimdall_update::install)
//!   recover  --destination <app> --state <dir>     repair an interrupted swap
//!   rollback --destination <app> --state <dir>     put the one rollback back (the replaced version becomes the rollback)
//!
//! Progress goes to `<state>/update.log`, a failure to `<state>/update-result.json`, which the next app launch reports.

use heimdall_update::install::{self, Installer, Plan};
use heimdall_update::Product;
use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "Usage: heimdall-install-tool update --plan <plan.json>
       heimdall-install-tool recover --destination <Heimdall Alpha.app> --state <state dir>
       heimdall-install-tool rollback --destination <Heimdall Alpha.app> --state <state dir>
";

fn value(args: &[String], key: &str) -> Option<PathBuf> {
    let i = args.iter().position(|a| a == key)?;
    args.get(i + 1).map(PathBuf::from)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let product = Product::ALPHA;
    let usage = || {
        eprint!("{USAGE}");
        ExitCode::from(2)
    };
    match args.first().map(String::as_str) {
        Some("update") => {
            let Some(path) = value(&args, "--plan") else { return usage() };
            let Some(plan) = std::fs::read(&path).ok().and_then(|d| serde_json::from_slice::<Plan>(&d).ok()) else {
                return usage();
            };
            install::append_log(&plan.state_dir, &format!("update {} -> {} at {}", plan.from, plan.staged.version, plan.destination.display()));
            match Installer::new(plan, product).run() {
                Ok(outcome) => {
                    println!("{outcome:?}");
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    eprintln!("Heimdall Alpha update stopped: {error}");
                    ExitCode::from(1)
                }
            }
        }
        Some("recover") => {
            let (Some(destination), Some(state)) = (value(&args, "--destination"), value(&args, "--state")) else {
                return usage();
            };
            match install::recover(&destination, &state, &product) {
                Ok(Some(note)) => {
                    println!("{note}");
                    ExitCode::SUCCESS
                }
                Ok(None) => ExitCode::SUCCESS,
                Err(error) => {
                    eprintln!("recovery failed: {error}");
                    ExitCode::from(1)
                }
            }
        }
        Some("rollback") => {
            let (Some(destination), Some(state)) = (value(&args, "--destination"), value(&args, "--state")) else {
                return usage();
            };
            match install::roll_back_installed(&destination, &state, &product) {
                Ok(version) => {
                    println!("rolled back to {version}");
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    eprintln!("rollback not performed: {error}");
                    ExitCode::from(1)
                }
            }
        }
        _ => usage(),
    }
}
