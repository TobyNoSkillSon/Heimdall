//! The FloatingIslands app: AppKit + Core Animation through objc2.
//!   FloatingIslands [--max-hours N (0: no limit)] [--parent-pid PID] [--selftest] [--flicktest] [--render DIR]
//!                   [--render-lane DIR] [--viewer-shots OUT FIXTURES] [--streams-check] [--render-glance DIR]
//!                   [--share-check OUT PAGE FILE] [--html-bench OUT FRAGMENTS [ROUNDS]] [--offscreen]
//!                   [--web-media-check OUT [VIDEO-URL] [IMAGE-URL]]
//! Data folder: ~/Library/Application Support/FloatingIslands (FLOATING_ISLANDS_DIR overrides it).

pub mod app;
pub mod confirm;
pub mod draw;
pub mod filewatch;
pub mod flicktest;
pub mod glance;
pub mod glance_render;
pub mod goals;
pub mod html;
pub mod html_bench;
pub mod island;
pub mod lane_render;
pub mod lists;
pub mod look;
pub mod marks;
pub mod overlay;
pub mod panel;
pub mod parts;
pub mod render;
pub mod scheduled;
pub mod toggle_render;
pub mod selftest;
pub mod settled;
pub mod share;
pub mod share_check;
pub mod store;
pub mod streams;
pub mod sys;
pub mod viewer;
pub mod viewer_audio;
pub mod web_check;
pub mod viewer_shots;
pub mod viewer_text;
pub mod world;

use std::os::unix::io::AsRawFd;

use objc2::MainThreadMarker;
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};

use crate::model::{Lifetime, Paths};

thread_local! {
    static KEEP: std::cell::RefCell<Vec<sys::Source>> = const { std::cell::RefCell::new(Vec::new()) };
}

fn keep(s: Option<sys::Source>) {
    if let Some(s) = s {
        KEEP.with(|k| k.borrow_mut().push(s));
    }
}

/// Quit cleanly: windows close, state and pid files go.
pub fn terminate() -> ! {
    let _ = std::fs::remove_file(Paths::pid());
    let _ = std::fs::remove_file(Paths::state());
    std::process::exit(0)
}

/// Lab modes rewrite or clear the data folder (`clear_folder`, removed island files). They run only on a lab folder
/// named by FLOATING_ISLANDS_DIR, never on the user's real one (review 23 Oct, finding 15: `--render-lane` had no guard).
const LAB_MODES: [&str; 11] = ["--offscreen", "--html-bench", "--share-check", "--selftest", "--flicktest", "--render", "--render-lane", "--render-toggle", "--record-toggle", "--streams-check", "--render-glance"];

/// The lab mode `args` ask for when no lab folder is set (`lab_dir`): it must refuse before the folder is touched.
fn refused_lab_mode(args: &[String], lab_dir: bool) -> Option<&str> {
    if lab_dir {
        return None;
    }
    args.iter().map(String::as_str).find(|a| LAB_MODES.contains(a))
}

/// Whether FLOATING_ISLANDS_DIR names a lab folder: an empty value does not (`Paths::root` falls back to the real one).
fn lab_dir_set(v: Option<&std::ffi::OsStr>) -> bool {
    v.is_some_and(|d| !d.is_empty())
}

pub fn main() {
    let args: Vec<String> = std::env::args().collect();
    if let Some(mode) = refused_lab_mode(&args, lab_dir_set(std::env::var_os("FLOATING_ISLANDS_DIR").as_deref())) {
        println!("{}: refusing to run without FLOATING_ISLANDS_DIR (it rewrites the data folder)", mode.trim_start_matches('-'));
        std::process::exit(2);
    }
    let mtm = MainThreadMarker::new().expect("FloatingIslands runs on the main thread");
    if let Some(i) = args.iter().position(|a| a == "--web-media-check") {
        // Bug note 2026-11-04T00-46-09: offscreen, before the data folder's lock (it reads no island).
        if let Some(out) = args.get(i + 1) {
            web_check::run(out, args.get(i + 2).map(String::as_str), args.get(i + 3).map(String::as_str));
        }
    }
    if let Some(i) = args.iter().position(|a| a == "--viewer-shots") {
        if i + 2 < args.len() {
            let app = NSApplication::sharedApplication(mtm);
            app.setActivationPolicy(NSApplicationActivationPolicy::Prohibited);
            viewer_shots::run_viewer_shots(&args[i + 1], &args[i + 2]);
        }
    }
    Paths::ensure();
    // One instance per data folder.
    let lock = std::fs::OpenOptions::new().create(true).read(true).write(true).truncate(false).open(Paths::root().join(".app.lock"));
    let locked = lock.as_ref().map(|f| unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0).unwrap_or(false);
    if !locked {
        eprintln!("FloatingIslands is already running for {}", Paths::root().display());
        std::process::exit(1);
    }
    std::mem::forget(lock);
    let _ = std::fs::write(Paths::pid(), std::process::id().to_string());
    // SIGTERM (`islands quit`, test traps) quits cleanly through the main queue.
    unsafe { libc::signal(libc::SIGTERM, libc::SIG_IGN) };
    keep(sys::Source::signal(libc::SIGTERM, || terminate()));

    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    // Shared copies of viewer pages (`model/share.rs`) older than a day go.
    crate::model::share::sweep(&Paths::shares(), crate::model::share::KEEP);
    // `--offscreen` (lab cost runs, 3 Nov: test islands must never cover the user's screen): the app runs whole, every
    // island built and drawn into its layers, but no window is ever ordered in.
    if args.iter().any(|a| a == "--offscreen" || a == "--html-bench" || a == "--render" || a == "--render-lane" || a == "--render-toggle" || a == "--streams-check" || a == "--render-glance" || a == "--share-check") {
        island::OFFSCREEN.with(|o| o.set(true));
    }
    let controller = app::App::install();
    controller.start();

    // A hard lifetime, so a forgotten test instance never lingers (default 24 h; 0: Heimdall supervises it).
    if let Some(h) = Lifetime::max_hours(&args) {
        sys::after(h * 3600.0, || terminate());
    }
    // `--parent-pid N`: quit when Heimdall exits, however it ends. A kernel process-exit event, no polling.
    if let Some(parent) = Lifetime::parent_pid(&args) {
        keep(sys::Source::process_exit(parent, || terminate()));
        let gone = unsafe { libc::kill(parent, 0) } != 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
        if gone {
            sys::soon(|| terminate());
        }
    }

    if args.iter().any(|a| a == "--selftest") {
        selftest::run();
    }
    if args.iter().any(|a| a == "--streams-check") {
        streams::check();
    }
    if args.iter().any(|a| a == "--flicktest") {
        flicktest::run();
    }
    if let Some(i) = args.iter().position(|a| a == "--render") {
        if let Some(out) = args.get(i + 1) {
            render::run(out);
        }
    }
    if let Some(i) = args.iter().position(|a| a == "--render-toggle") {
        if let Some(out) = args.get(i + 1) {
            toggle_render::run(out);
        }
    }
    if let Some(i) = args.iter().position(|a| a == "--record-toggle") {
        if let Some(out) = args.get(i + 1) {
            toggle_render::live(out);
        }
    }
    if let Some(i) = args.iter().position(|a| a == "--render-lane") {
        if let Some(out) = args.get(i + 1) {
            lane_render::run(out);
        }
    }
    if let Some(i) = args.iter().position(|a| a == "--share-check") {
        if let (Some(out), Some(page), Some(plain)) = (args.get(i + 1), args.get(i + 2), args.get(i + 3)) {
            let (out, page, plain) = (out.clone(), page.clone(), plain.clone());
            sys::soon(move || share_check::run(&out, &page, &plain));
        }
    }
    if let Some(i) = args.iter().position(|a| a == "--html-bench") {
        if let (Some(out), Some(dir)) = (args.get(i + 1), args.get(i + 2)) {
            html_bench::run(out, dir, args.get(i + 3).and_then(|r| r.parse().ok()).unwrap_or(1));
        }
    }
    if let Some(i) = args.iter().position(|a| a == "--render-glance") {
        if let Some(out) = args.get(i + 1) {
            glance_render::run(out);
        }
    }
    app.run();
}

#[cfg(test)]
mod tests {
    use super::{lab_dir_set, refused_lab_mode};

    #[test]
    fn an_empty_lab_folder_is_no_lab_folder() {
        assert!(!lab_dir_set(None));
        assert!(!lab_dir_set(Some(std::ffi::OsStr::new(""))));
        assert!(lab_dir_set(Some(std::ffi::OsStr::new("/tmp/lab"))));
    }

    #[test]
    fn lab_modes_refuse_without_a_lab_folder() {
        let args = |a: &[&str]| -> Vec<String> { a.iter().map(|s| s.to_string()).collect() };
        for mode in ["--offscreen", "--html-bench", "--render-lane", "--render-toggle", "--record-toggle", "--render", "--flicktest", "--selftest", "--streams-check", "--render-glance"] {
            assert_eq!(refused_lab_mode(&args(&["FloatingIslands", mode, "/tmp/out"]), false), Some(mode));
            assert_eq!(refused_lab_mode(&args(&["FloatingIslands", mode, "/tmp/out"]), true), None, "{mode} with a lab folder");
        }
        assert_eq!(refused_lab_mode(&args(&["FloatingIslands", "--parent-pid", "42", "--max-hours", "0"]), false), None,
            "Heimdall's own launch runs on the real folder");
    }
}
