//! The first ⌃⌘H of a long day, through the real offscreen host (`heimdall-ui-demo --bench-chat`, audit 27 Oct): the
//! 400-row conversation is laid out on per-word widths (a few thousand CoreText calls, not 52k), a new row lays out
//! only itself, the width cache never empties mid-layout, and on the live clock a stalled apply delays the opening
//! motion instead of skipping it (its first painted frame shows the island still closed).

#[cfg(target_os = "macos")]
fn bench(env: &[(&str, &str)]) -> String {
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_heimdall-ui-demo"));
    cmd.args(["--bench-chat", "400"]);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("run heimdall-ui-demo");
    let report = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(out.status.success(), "{report}\n{}", String::from_utf8_lossy(&out.stderr));
    report
}

#[cfg(target_os = "macos")]
fn line(report: &str, start: &str) -> String {
    report.lines().find(|l| l.trim_start().starts_with(start)).unwrap_or_else(|| panic!("no {start:?} in\n{report}")).to_string()
}

#[cfg(target_os = "macos")]
fn num(l: &str, key: &str) -> f64 {
    let at = l.find(key).unwrap_or_else(|| panic!("no {key:?} in {l}")) + key.len();
    l[at..].chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect::<String>().parse().unwrap()
}

/// Before 27 Oct: 52k CoreText calls and 12 cache wipes on the first open, as many again for one new row.
#[cfg(target_os = "macos")]
#[test]
fn a_long_day_opens_on_word_widths_and_a_new_row_lays_out_alone() {
    let report = bench(&[]);
    let first = line(&report, "FIRST ⌃⌘H");
    assert!(num(&first, "misses=") <= 3000.0, "{first}");
    assert_eq!(num(&first, "clears="), 0.0, "{first}");
    let new_row = line(&report, "a new row while open");
    assert!(num(&new_row, "misses=") <= 40.0, "one new row shapes only its own words: {new_row}");
    let third = line(&report, "THIRD ⌃⌘H");
    assert!(num(&third, "misses=") <= 40.0, "{third}");
    assert_eq!(num(&third, "clears="), 0.0, "{third}");
}

/// Before 27 Oct: the first frame painted after a slow open showed 100% of the opening (726 ms into a 0.45 s spring).
#[cfg(target_os = "macos")]
#[test]
fn a_stalled_open_still_plays_its_opening_motion() {
    let report = bench(&[("BENCH_LIVE", "1"), ("BENCH_STALL_MS", "200")]);
    let live = line(&report, "LIVE CLOCK");
    assert!(num(&live, "first frame shows ") < 15.0, "the first painted frame is the motion's start, not its end: {live}");
}
