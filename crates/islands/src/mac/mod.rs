//! The FloatingIslands app: AppKit + Core Animation through objc2.
//!   FloatingIslands [--max-hours N (0: no limit)] [--parent-pid PID] [--offscreen]
//! Lab harnesses (`--features lab`, never in the bundled helper; each needs FLOATING_ISLANDS_DIR, see `LAB_MODES`):
//!   [--selftest] [--flicktest] [--render DIR] [--render-lane DIR] [--render-toggle DIR] [--record-toggle DIR]
//!   [--streams-check] [--share-check OUT PAGE FILE] [--viewer-pinch-shots OUT PAGE SEQUENCES]
//!   and [--viewer-shots OUT FIXTURES], which needs no lab folder.
//! Data folder: ~/Library/Application Support/FloatingIslands (FLOATING_ISLANDS_DIR overrides it).

pub mod app;
pub mod confirm;
pub mod draw;
pub mod filewatch;
pub mod glance;
pub mod goals;
pub mod html;
pub mod island;
pub mod lists;
pub mod look;
pub mod marks;
pub mod outside_click;
pub mod overlay;
pub mod panel;
pub mod parts;
pub mod scheduled;
pub mod settled;
pub mod share;
pub mod store;
pub mod streams;
pub mod sys;
pub mod viewer;
pub mod viewer_audio;
pub mod viewer_edit;
pub mod viewer_text;
pub mod world;

// Lab harnesses (audit ISL-2, 14 Nov): built only with `--features lab`.
#[cfg(feature = "lab")]
pub mod flicktest;
#[cfg(feature = "lab")]
pub mod lane_render;
#[cfg(feature = "lab")]
pub mod render;
#[cfg(feature = "lab")]
pub mod selftest;
#[cfg(feature = "lab")]
pub mod share_check;
#[cfg(feature = "lab")]
pub mod toggle_render;
#[cfg(feature = "lab")]
pub mod viewer_pinch_shots;
#[cfg(feature = "lab")]
pub mod viewer_shots;

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

/// One table for every lab mode (audit ISL-19): the flag, and whether the app runs offscreen for it. Lab modes rewrite
/// or clear the data folder (`clear_folder`, removed island files, the zoom memory). They run only on a lab folder named
/// by FLOATING_ISLANDS_DIR, never on the user's real one (review 23 Oct, finding 15: `--render-lane` had no guard; ISL-8, 14
/// Nov: the viewer harnesses checked only `is_err`, and an empty value is the real folder).
const LAB_MODES: [(&str, bool); 10] = [
    ("--offscreen", true),
    ("--share-check", true),
    ("--selftest", false),
    ("--flicktest", false),
    ("--render", true),
    ("--render-lane", true),
    ("--render-toggle", true),
    ("--record-toggle", false),
    ("--streams-check", true),
    ("--viewer-pinch-shots", false),
];

/// Harness flags deleted on 14 Nov (audit ISL-2): refused rather than run as the plain app.
const RETIRED: [&str; 5] = ["--html-bench", "--render-glance", "--web-media-check", "--viewer-zoom-shots", "--viewer-picture-shots"];

/// The harness flag `args` ask for that this build lacks: lab harnesses are compiled only with `--features lab`
/// (`--offscreen` is the whole app drawn offscreen, in every build). Refused rather than run as the plain app.
fn missing_harness(args: &[String], lab_build: bool) -> Option<&str> {
    if lab_build {
        return None;
    }
    args.iter().map(String::as_str).find(|a| *a == "--viewer-shots" || LAB_MODES.iter().any(|(m, _)| m == a && *m != "--offscreen"))
}

/// The lab mode `args` ask for when no lab folder is set (`lab_dir`): it must refuse before the folder is touched.
fn refused_lab_mode(args: &[String], lab_dir: bool) -> Option<&str> {
    if lab_dir {
        return None;
    }
    args.iter().map(String::as_str).find(|a| LAB_MODES.iter().any(|(m, _)| m == a))
}

/// Whether FLOATING_ISLANDS_DIR names a lab folder: an empty value does not (`Paths::root` falls back to the real one).
fn lab_dir_set(v: Option<&std::ffi::OsStr>) -> bool {
    v.is_some_and(|d| !d.is_empty())
}

pub fn main() {
    let args: Vec<String> = std::env::args().collect();
    if let Some(mode) = args.iter().find(|a| RETIRED.contains(&a.as_str())) {
        println!("{}: retired on 14 Nov (no lab ran it)", mode.trim_start_matches('-'));
        std::process::exit(2);
    }
    if let Some(mode) = missing_harness(&args, cfg!(feature = "lab")) {
        println!("{}: a lab harness; build with `cargo build -p islands --features lab`", mode.trim_start_matches('-'));
        std::process::exit(2);
    }
    if let Some(mode) = refused_lab_mode(&args, lab_dir_set(std::env::var_os("FLOATING_ISLANDS_DIR").as_deref())) {
        println!("{}: refusing to run without FLOATING_ISLANDS_DIR (it rewrites the data folder)", mode.trim_start_matches('-'));
        std::process::exit(2);
    }
    let mtm = MainThreadMarker::new().expect("FloatingIslands runs on the main thread");
    #[cfg(feature = "lab")]
    lab::before_app(&args, mtm);
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
    // `--offscreen` and the offscreen lab modes (lab cost runs, 3 Nov: test islands must never cover the user's screen): the
    // app runs whole, every island built and drawn into its layers, but no window is ever ordered in.
    if args.iter().any(|a| LAB_MODES.iter().any(|(m, offscreen)| *offscreen && m == a)) {
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

    #[cfg(feature = "lab")]
    lab::after_start(&args);
    app.run();
}

/// The lab harnesses' dispatch (`--features lab`).
#[cfg(feature = "lab")]
mod lab {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};

    use super::*;

    /// Harnesses that run instead of the app, before the data folder's lock.
    pub fn before_app(args: &[String], mtm: MainThreadMarker) {
        let at = |flag: &str, n: usize| args.iter().position(|a| a == flag).filter(|i| i + n < args.len());
        if let Some(i) = at("--viewer-shots", 2) {
            NSApplication::sharedApplication(mtm).setActivationPolicy(NSApplicationActivationPolicy::Prohibited);
            viewer_shots::run_viewer_shots(&args[i + 1], &args[i + 2]);
        }
        if let Some(i) = at("--viewer-pinch-shots", 3) {
            // A page's own pinch (13 Nov): synthesized magnify gestures reach its hook; parity data for lab/pinch_parity.py.
            NSApplication::sharedApplication(mtm).setActivationPolicy(NSApplicationActivationPolicy::Prohibited);
            viewer_pinch_shots::run(&args[i + 1], &args[i + 2], &args[i + 3]);
        }
    }

    /// Harnesses that drive the running app.
    pub fn after_start(args: &[String]) {
        let arg = |flag: &str, n: usize| args.iter().position(|a| a == flag).and_then(|i| args.get(i + n));
        if args.iter().any(|a| a == "--selftest") {
            selftest::run();
        }
        if args.iter().any(|a| a == "--streams-check") {
            streams::check();
        }
        if args.iter().any(|a| a == "--flicktest") {
            flicktest::run();
        }
        if let Some(out) = arg("--render", 1) {
            render::run(out);
        }
        if let Some(out) = arg("--render-toggle", 1) {
            toggle_render::run(out);
        }
        if let Some(out) = arg("--record-toggle", 1) {
            toggle_render::live(out);
        }
        if let Some(out) = arg("--render-lane", 1) {
            lane_render::run(out);
        }
        if let (Some(out), Some(page), Some(plain)) = (arg("--share-check", 1), arg("--share-check", 2), arg("--share-check", 3)) {
            let (out, page, plain) = (out.clone(), page.clone(), plain.clone());
            sys::soon(move || share_check::run(&out, &page, &plain));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{lab_dir_set, missing_harness, refused_lab_mode, LAB_MODES};

    #[test]
    fn an_empty_lab_folder_is_no_lab_folder() {
        assert!(!lab_dir_set(None));
        assert!(!lab_dir_set(Some(std::ffi::OsStr::new(""))));
        assert!(lab_dir_set(Some(std::ffi::OsStr::new("/tmp/lab"))));
    }

    #[test]
    fn lab_modes_refuse_without_a_lab_folder() {
        let args = |a: &[&str]| -> Vec<String> { a.iter().map(|s| s.to_string()).collect() };
        for mode in ["--offscreen", "--share-check", "--render-lane", "--render-toggle", "--record-toggle", "--render", "--flicktest", "--selftest", "--streams-check", "--viewer-pinch-shots"] {
            assert_eq!(refused_lab_mode(&args(&["FloatingIslands", mode, "/tmp/out"]), false), Some(mode));
            assert_eq!(refused_lab_mode(&args(&["FloatingIslands", mode, "/tmp/out"]), true), None, "{mode} with a lab folder");
        }
        assert_eq!(refused_lab_mode(&args(&["FloatingIslands", "--parent-pid", "42", "--max-hours", "0"]), false), None,
            "Heimdall's own launch runs on the real folder");
    }

    #[test]
    fn a_build_without_the_lab_feature_refuses_every_harness() {
        let args = |a: &[&str]| -> Vec<String> { a.iter().map(|s| s.to_string()).collect() };
        for (mode, _) in LAB_MODES.iter().filter(|(m, _)| *m != "--offscreen") {
            assert_eq!(missing_harness(&args(&["FloatingIslands", mode, "/tmp/out"]), false), Some(*mode));
            assert_eq!(missing_harness(&args(&["FloatingIslands", mode, "/tmp/out"]), true), None);
        }
        assert_eq!(missing_harness(&args(&["FloatingIslands", "--viewer-shots", "/tmp/o", "f"]), false), Some("--viewer-shots"));
        assert_eq!(missing_harness(&args(&["FloatingIslands", "--offscreen"]), false), None, "the whole app offscreen is no harness");
        assert_eq!(missing_harness(&args(&["FloatingIslands", "--parent-pid", "42", "--max-hours", "0"]), false), None);
    }

    /// Audit TST-11 (14 Nov): a process's first SF Symbol image, CoreText layout or ImageIO thumbnail connects to
    /// WindowServer and checks in with LaunchServices (sampled: the symbol image waits on both). Nextest runs each test
    /// in its own process, so each of these paid it, ten at once: 12 to 17 s each against milliseconds for their
    /// neighbours. They share one process here; every case still runs, and a failure names its case.
    #[test]
    fn appkit_renders() {
        let cases: [(&str, fn()); 10] = [
            ("mac::goals::tests::a_proposal_ghost_has_yes_and_no_and_a_no_answers_it_once", super::goals::tests::a_proposal_ghost_has_yes_and_no_and_a_no_answers_it_once),
            ("mac::goals::tests::goals_draw_the_active_goals_their_queue_and_one_ghost", super::goals::tests::goals_draw_the_active_goals_their_queue_and_one_ghost),
            ("mac::goals::tests::a_goal_proposal_is_a_ghost_goal_and_nothing_at_all_is_the_empty_sign", super::goals::tests::a_goal_proposal_is_a_ghost_goal_and_nothing_at_all_is_the_empty_sign),
            ("mac::goals::tests::an_ongoing_goal_draws_a_loop_and_a_done_card_adds_no_segment", super::goals::tests::an_ongoing_goal_draws_a_loop_and_a_done_card_adds_no_segment),
            ("mac::settled::tests::today_shows_three_rows_then_earlier_and_claims_are_not_counted", super::settled::tests::today_shows_three_rows_then_earlier_and_claims_are_not_counted),
            ("mac::scheduled::tests::scheduled_draws_the_guarantee_the_strip_and_a_ghost_in_its_place", super::scheduled::tests::scheduled_draws_the_guarantee_the_strip_and_a_ghost_in_its_place),
            ("mac::scheduled::tests::a_waiting_item_offers_its_choices_and_nothing_promised_says_so", super::scheduled::tests::a_waiting_item_offers_its_choices_and_nothing_promised_says_so),
            ("mac::draw::tests::thumbnail_of_bad_files_is_none", super::draw::tests::thumbnail_of_bad_files_is_none),
            ("mac::parts::tests::a_closed_rows_detail_line_is_one_line_cut_with_an_ellipsis_at_the_row_width", super::parts::tests::a_closed_rows_detail_line_is_one_line_cut_with_an_ellipsis_at_the_row_width),
            ("mac::lists::tests::a_title_cut_at_two_lines_is_as_tall_as_the_lines_it_draws", super::lists::tests::a_title_cut_at_two_lines_is_as_tall_as_the_lines_it_draws),
        ];
        let failed: Vec<&str> = cases.iter().filter(|(_, case)| std::panic::catch_unwind(case).is_err()).map(|(name, _)| *name).collect();
        assert!(failed.is_empty(), "failed: {failed:?}");
    }
}
