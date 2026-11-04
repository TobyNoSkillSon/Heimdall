//! `heimdall-ui-demo --island-loops <dir>`: the island's looping content (the thinking dots while the agent works,
//! the spinner while Listening loads) checked offscreen. Stills at eight points of the 0.9 s cycle drawn from the
//! frame model (`<case>-model-<phase>.png`) and from the layers' own Core Animation loop, paused at the same points
//! (`<case>-anim-<phase>.png`), which must match pixel for pixel; and the main thread's cost while the loop shows,
//! measured on a live clock over 5 s: island ticks and layer-tree commits a second (GPU trace 28 Oct 20:05: the app
//! redrew at 30 fps while the agent worked).

use std::fmt::Write as _;
use std::path::Path;
use std::time::Instant;

use objc2_foundation::MainThreadMarker;

use super::checks::{advance, snapshot, spin, Checks};
use super::host::{with_host, Host, Place, HOST};
use crate::island::object::{Content, Event};
use crate::island::{Activity, CommandCue, CommandKind};

const PERIOD: f64 = 0.9;

struct Case {
    name: &'static str,
    start: fn(),
    looped: fn(&Content) -> bool,
    /// Steps a cycle (the dots were drawn by a 30 fps timer: 27 a cycle; the spinner has eight); eight points of the
    /// cycle on its steps are drawn.
    steps: usize,
}

impl Case {
    fn phases(&self) -> Vec<f64> {
        (0..8).map(|k| (k * self.steps / 8) as f64 / self.steps as f64).collect()
    }
}

fn thinking() {
    super::apply(Event::Sent);
    super::apply(Event::Activity(Some(Activity::Thinking)));
}

fn loading() {
    super::apply(Event::Command(CommandCue::Heard { id: 1, kind: CommandKind::Listening }));
}

const CASES: [Case; 2] = [
    Case { name: "dots", start: thinking, looped: |c| matches!(c, Content::Dots), steps: 27 },
    Case { name: "spinner", start: loading, looped: |c| matches!(c, Content::Spinner), steps: 8 },
];

/// When the looping slot's cycle began (island clock), while it shows.
fn since(looped: fn(&Content) -> bool) -> Option<f64> {
    with_host(|h| {
        let f = h.island.frame(h.clock.unwrap_or(0.0));
        f.slots.iter().find(|s| looped(&s.content) && s.opacity > 0.99).map(|s| s.since)
    })
    .flatten()
}

fn set_clock(t: f64) {
    with_host(|h| {
        h.clock = Some(t);
        h.apply(Event::Tick);
    });
    spin(0.0);
}

fn stills(c: &mut Checks, dir: &Path, mtm: MainThreadMarker, case: &Case) {
    super::install_offscreen(mtm);
    (case.start)();
    advance(1.2);
    let Some(since) = since(case.looped) else {
        c.check(false, &format!("{}: the looping slot shows", case.name));
        return;
    };
    let now = with_host(|h| h.clock.unwrap_or(0.0)).unwrap_or(0.0);
    let first = ((now - since) / PERIOD).ceil();
    for p in case.phases() {
        // A hair past the point, so the phase's floor is the point itself.
        set_clock(since + PERIOD * (first + p) + 1e-4);
        let shows = self::since(case.looped).is_some();
        c.check(shows, &format!("{}: still looping at phase {p:.3}", case.name));
        let model = dir.join(format!("{}-model-{p:.3}.png", case.name));
        c.check(snapshot(&model), &format!("{}: model still at {p:.3}", case.name));
        // The layers' own loop, paused at the same point of its cycle (none before the fix: nothing is written).
        let anim = dir.join(format!("{}-anim-{p:.3}.png", case.name));
        let drawn = with_host(|h| h.tree.loop_still(p * PERIOD + 1e-4, &anim)).unwrap_or(false);
        if drawn {
            // The model's still is a hair past the step (1e-4 s): the dots' fade there may round 1/255 apart.
            let diff = pixels(&model).zip(pixels(&anim)).and_then(|(a, b)| (a.len() == b.len()).then(|| a.iter().zip(&b).map(|(x, y)| x.abs_diff(*y)).max().unwrap_or(0)));
            c.check(diff.is_some_and(|d| d <= 1), &format!("{}: the layer animation at {p:.3} draws the model's frame (largest channel difference {diff:?} of 255)", case.name));
        } else {
            c.note(&format!("{}: no layer animation to pause at {p:.3}", case.name));
        }
    }
}

/// A PNG's samples.
fn pixels(path: &Path) -> Option<Vec<u8>> {
    let mut reader = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(path).ok()?)).read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buf).ok()?;
    buf.truncate(info.buffer_size());
    Some(buf)
}

/// CPU seconds this process has used (user and system).
fn cpu() -> f64 {
    let mut u: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut u) };
    u.ru_utime.tv_sec as f64 + u.ru_utime.tv_usec as f64 / 1e6 + u.ru_stime.tv_sec as f64 + u.ru_stime.tv_usec as f64 / 1e6
}

/// The live clock (real timers, real run loop) while the loop shows: ticks and commits a second over 5 s.
fn live_cost(c: &mut Checks, mtm: MainThreadMarker, case: &Case) -> (f64, f64) {
    let saved = HOST.with(|h| h.borrow_mut().take());
    HOST.with(|h| *h.borrow_mut() = Some(Host::new(mtm, Place::offscreen(), true)));
    (case.start)();
    // The arrival's motion settles (offscreen hosts step it on a 120 Hz timer).
    spin(2.0);
    let looping = with_host(|h| {
        let f = h.island.frame(h.now());
        f.slots.iter().any(|s| (case.looped)(&s.content))
    })
    .unwrap_or(false);
    c.check(looping, &format!("{}: live, the loop shows after its arrival", case.name));
    // The layers' loop is phase-locked to the model's: its beginTime mapped through Core Animation's clock gives the
    // phase the model draws now (within 20 ms of the cycle).
    let phases = with_host(|h| {
        let now = h.now();
        let model = h.island.frame(now).slots.iter().find(|s| (case.looped)(&s.content)).map(|s| s.phase);
        (model, h.tree.loop_phase())
    });
    if let Some((Some(model), Some(anim))) = phases {
        let d = (model - anim).abs();
        let off = d.min(1.0 - d) * PERIOD * 1000.0;
        c.check(off < 20.0, &format!("{}: the layers' loop runs at the model's phase (model {model:.3}, layers {anim:.3}, {off:.1} ms apart)", case.name));
    } else {
        c.note(&format!("{}: no layer loop to compare with the model's phase", case.name));
    }
    let (ticks0, frames0) = with_host(|h| (h.stats.ticks, h.stats.frames)).unwrap_or((0, 0));
    let (t0, c0) = (Instant::now(), cpu());
    spin(5.0);
    let (secs, used) = (t0.elapsed().as_secs_f64(), cpu() - c0);
    let (ticks1, frames1, clock) = with_host(|h| (h.stats.ticks, h.stats.frames, h.clock_report())).unwrap_or((0, 0, (false, false, None)));
    let (tps, fps) = ((ticks1 - ticks0) as f64 / secs, (frames1 - frames0) as f64 / secs);
    c.note(&format!(
        "{}: live over {secs:.1} s: {tps:.1} island ticks/s, {fps:.1} layer-tree commits/s, {:.2} % CPU (process), display link {}, loop timer {}",
        case.name,
        used / secs * 100.0,
        clock.0,
        clock.1
    ));
    HOST.with(|h| *h.borrow_mut() = saved);
    (tps, fps)
}

/// Returns the failed checks; the report goes to `dir/island-loops.txt`.
pub(crate) fn run(dir: &Path) -> Vec<String> {
    let mtm = MainThreadMarker::new().expect("the island loops check runs on the main thread");
    objc2_app_kit::NSApplication::sharedApplication(mtm).setActivationPolicy(objc2_app_kit::NSApplicationActivationPolicy::Prohibited);
    std::fs::create_dir_all(dir).ok();
    let mut c = Checks::new();
    super::set_sink(Box::new(|_| {}));
    for case in &CASES {
        stills(&mut c, dir, mtm, case);
    }
    let mut costs = String::new();
    for case in &CASES {
        let (tps, fps) = live_cost(&mut c, mtm, case);
        let _ = writeln!(costs, "{} ticks_per_s={tps:.2} commits_per_s={fps:.2}", case.name);
        c.check(fps < 1.0, &format!("{}: while only the loop moves the app commits nothing ({fps:.1}/s)", case.name));
    }
    std::fs::write(dir.join("island-loops.txt"), format!("{}\n{costs}", c.report)).ok();
    c.failed
}
