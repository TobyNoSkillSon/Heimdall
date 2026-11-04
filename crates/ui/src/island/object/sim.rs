//! The island scenario simulator (lab/testing/TOOLING.md §1). It drives the real `Island` — the code the app runs —
//! on virtual time: frame by frame at the display rate while something moves, a few frames of each loop, and
//! straight to the next event or deadline at rest. Every frame is checked against the invariants below; failures are
//! findings, never panics (the runner itself is wrapped in `catch_unwind` by the tests).
//!
//! | # | Invariant |
//! |---|---|
//! | I1 | no panic, no non-finite geometry |
//! | I2 | symmetric about the notch: ears mirrored, column content centred |
//! | I3 | one clock: every element's start is an event or deadline time, loops are phase-locked to it |
//! | I4 | no jumps: per-frame bounds on width, height, radius and opacity; nothing pops in or out at full strength |
//! | I5 | content inside the shape, nothing under the camera housing |
//! | I6 | nothing draws at rest: two frames are identical, no frames are requested, deadlines fire once |
//! | I7 | every state exitable: withheld events end at rest or a declared persistent state, whose exit works |
//! | I8 | the voice's look ends when the audio ends |
//! | I9 | lines clear themselves |
//! | I10 | the shape fits its window and the display |
//! | I11 | hit targets at rest are inside the shape, big enough for a trackpad tap, and hit-testing finds them |
//! | I12 | the track is non-empty and starts at the motion's start |
//! | I13 | at most three motion starts a second |
//! | I14 | deterministic (the tests run sequences twice and compare trace hashes) |
//! | I15 | no empty box: while arriving at a new size, the shape never stands at full size with nothing of its content drawn for more than 50 ms |
//! | I16 | the notch never shows below the shape: at least as deep as the notch while at least as wide |
//! | I17 | no text over text: two texts above 20 % opacity never overlap by more than a fifth of the smaller |
//! | I18 | a reopened element keeps its kind: a note that left and comes back without a new notice keeps its icon |
//! | I19 | no success after speech: once the audio ends with nothing new arriving, no check mark settles in the lip (it retracts) |
//! | I20 | a reply's words only grow while it is spoken: each sentence is added, none is replaced or dropped |
//! | I21 | sized to its content: no state is wider than its content + 2 × 28 pt or deeper than its content + 16 pt, never less than the notch (the reader keeps its widest unit's width) |
//! | I22 | an agent's note never stays open unattended: the pull-down it opened, on screen with the pointer away for `NOTE_UNATTENDED`, pulls itself up (one the user pulled down from the notch stays) |

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::hash::{Hash, Hasher};

use super::geom::Display;
use super::layout::Class;
use super::measure::{Approx, Fuzz, FuzzWidth, Measure};
use super::scene::{GRACE, NOTE_UNATTENDED};
use super::{Event, FrameModel, FrameNeed, Island, Output, Switch, Target};
use crate::agent_notice::{AgentNotice, Detail, NoticeAction, NoticeKind};
use crate::island::{Activity, CommandCue, CommandKind, HealthControl, HealthCue, Part};
use crate::reader::{FastRead, FastReadUpdate};

/// One step of a scenario.
#[derive(Clone, Debug, PartialEq)]
pub enum Step {
    Ev(Event),
    /// A click on the `n`-th target the island offers at that moment (none: nothing happens).
    ClickAny(usize),
    /// The process crashed: the state is lost and a new island starts.
    Crash,
}

/// A sequence of steps on a display.
#[derive(Clone, Debug)]
pub struct Scenario {
    pub id: String,
    pub what: String,
    pub display: Display,
    pub steps: Vec<(f64, Step)>,
}

impl Scenario {
    pub fn new(id: &str, what: &str, steps: Vec<(f64, Step)>) -> Scenario {
        Scenario { id: id.into(), what: what.into(), display: Display::MBP16, steps }
    }
    pub fn on(mut self, d: Display) -> Scenario {
        self.display = d;
        self
    }
}

/// How the runner measures text.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Text {
    Approx,
    Fuzz(FuzzWidth),
}

/// Runner settings.
#[derive(Clone, Copy, Debug)]
pub struct Config {
    pub fps: f64,
    /// Seed for frame jitter (±4 ms) and dropped frames (100 ms – 2 s gaps); none: an even clock.
    pub jitter: Option<u64>,
    pub text: Text,
    /// Withhold events after the last one and check every state ends (I7).
    pub exit: bool,
}

impl Default for Config {
    fn default() -> Config {
        Config { fps: 120.0, jitter: None, text: Text::Approx, exit: true }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Finding {
    pub at: f64,
    pub invariant: &'static str,
    pub detail: String,
}

#[derive(Clone, Debug, Default)]
pub struct Report {
    pub findings: Vec<Finding>,
    pub frames: usize,
    pub hash: u64,
    pub classes: BTreeSet<Class>,
    pub motions: usize,
    pub outputs: Vec<Output>,
    pub end: Option<Class>,
}

impl Report {
    pub fn ok(&self) -> bool {
        self.findings.is_empty()
    }
}

/// Opacity above which a slot counts as visible.
const VISIBLE: f64 = 0.05;
/// The island window's width (`mac/island`): the widest layout (a wide table's pull-down, `layout::WIDE_W` 700) with
/// room for overshoot, flares and the glow.
pub const WINDOW_W: f64 = 780.0;
/// A declared persistent state's exit is checked for this long.
const EXIT_WAIT: f64 = 3600.0;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545F4914F6CDD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next() % n as u64) as usize
        }
    }
    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
}

fn measure(text: Text) -> Box<dyn Measure> {
    match text {
        Text::Approx => Box::new(Approx),
        Text::Fuzz(w) => Box::new(Fuzz(w)),
    }
}

struct Runner<'a> {
    cfg: &'a Config,
    island: Island,
    display: Display,
    t: f64,
    prev: Option<FrameModel>,
    report: Report,
    hasher: std::collections::hash_map::DefaultHasher,
    /// Times the island was told something (events and deadlines): every element's start must be one.
    times: HashSet<u64>,
    rng: Rng,
    voice_on: bool,
    voice_off_at: Option<f64>,
    seen_ids: BTreeSet<u64>,
    seen_keys: BTreeSet<String>,
    seen_notices: BTreeSet<String>,
    /// Per motion (by id): its first and last shape and first-step speed, for the jump bounds.
    motion_cache: Option<(u64, super::Shape, super::Shape, f64, f64)>,
    /// The motion before it (a frame gap can span the end of one motion and the start of the next).
    motion_before: Option<(u64, super::Shape, super::Shape, f64, f64)>,
    /// The loop already checked (motion id, class): later deadlines that change nothing skip its frames.
    loop_checked: Option<(u64, Class)>,
    /// Deadline frames since the scene last changed (periodic ticks).
    quiet_ticks: usize,
    /// I15: since when the shape has stood at full size without its content.
    empty_since: Option<f64>,
    /// I15: the shape when the current motion started.
    motion_from: Option<(u64, (f64, f64))>,
    last_target: Option<(f64, f64)>,
    /// I18: each note's icon when it first showed.
    note_icons: std::collections::HashMap<String, String>,
    note_last: BTreeSet<String>,
    /// I19: the audio ended naturally and nothing has arrived since: the check marks visible then.
    after_voice: Option<HashSet<String>>,
    /// I20: the reply's sentences at the last frame while the voice played.
    spoken_prev: Option<Vec<String>>,
    /// I22: the pointer is over the island, or over the pull-down's band; the user pulled it down from the notch; since
    /// when one an agent's note opened has been on screen unattended.
    pointer: bool,
    band_hot: bool,
    note_owner: bool,
    note_unattended: Option<f64>,
}

impl<'a> Runner<'a> {
    fn new(cfg: &'a Config, display: Display) -> Runner<'a> {
        Runner {
            cfg,
            island: Island::new(display, measure(cfg.text)),
            display,
            t: 0.0,
            prev: None,
            report: Report::default(),
            hasher: Default::default(),
            times: HashSet::new(),
            rng: Rng(cfg.jitter.unwrap_or(1).max(1)),
            voice_on: false,
            voice_off_at: None,
            seen_ids: BTreeSet::new(),
            seen_keys: BTreeSet::new(),
            seen_notices: BTreeSet::new(),
            motion_cache: None,
            motion_before: None,
            loop_checked: None,
            quiet_ticks: 0,
            empty_since: None,
            motion_from: None,
            last_target: None,
            note_icons: Default::default(),
            note_last: BTreeSet::new(),
            after_voice: None,
            spoken_prev: None,
            pointer: false,
            band_hot: false,
            note_owner: false,
            note_unattended: None,
        }
    }

    /// The current motion's first and last shape and its first-step speed (width, height), computed once.
    fn motion_bounds(&mut self) -> (super::Shape, super::Shape, f64, f64) {
        let id = self.island.motion().0;
        if let Some((mid, a, b, vw, vh)) = self.motion_cache {
            if mid == id {
                return (a, b, vw, vh);
            }
        }
        let tr = self.island.track(self.t);
        let (a, b) = (tr.first, tr.last());
        let vw = (tr.get(1).w - a.w).abs() / tr.step.max(1e-6) / 60.0;
        let vh = (tr.get(1).h - a.h).abs() / tr.step.max(1e-6) / 60.0;
        // I12, once per motion: non-empty (by type), starting at the motion's start.
        if tr.begin != self.island.motion().1 && self.island.motion().1.is_finite() {
            let d = format!("track begins {} but the motion began {}", tr.begin, self.island.motion().1);
            self.find("I12 track", d);
        }
        self.motion_before = self.motion_cache.take();
        self.motion_cache = Some((id, a, b, vw, vh));
        (a, b, vw, vh)
    }

    fn find(&mut self, invariant: &'static str, detail: String) {
        if self.report.findings.len() < 50 {
            self.report.findings.push(Finding { at: self.t, invariant, detail });
        }
    }

    fn apply(&mut self, ev: Event, t: f64) {
        self.note(&ev, t);
        let before = self.island.motion_starts().len();
        self.times.insert(t.to_bits());
        self.island.apply(ev, t);
        let outs = self.island.take_outputs();
        // The window host's side of drawing mode: Send closes and captures, Close closes.
        for o in &outs {
            if matches!(o, Output::DrawUndo | Output::DrawClear) {
                self.times.insert(t.to_bits());
                self.island.apply(Event::Ink(0), t);
            }
        }
        self.report.outputs.extend(outs);
        if self.island.motion_starts().len() != before || self.island.motion_starts().len() == 64 {
            self.check_rate();
        }
    }

    /// Bookkeeping for I7–I9, I19 and I20.
    fn note(&mut self, ev: &Event, t: f64) {
        match ev {
            Event::Speaking(false) if self.voice_on => {
                let f = self.island.frame(t);
                self.after_voice = Some(f.slots.iter().filter(|s| s.opacity > VISIBLE && is_check(s)).map(|s| s.id.to_string()).collect());
            }
            Event::Tick | Event::SpeechLevel(_) | Event::Speaking(false) | Event::SpokenText(None) | Event::Hover(_) | Event::Pointer(_) | Event::Press(_) => {}
            _ => self.after_voice = None,
        }
        // A cut, a stop or a crash starts the words over legitimately.
        if matches!(ev, Event::Stop | Event::SpeechCut | Event::Speaking(false)) {
            self.spoken_prev = None;
        }
        match ev {
            Event::Speaking(true) => {
                self.voice_on = true;
                self.voice_off_at = None;
            }
            Event::Speaking(false) | Event::Stop | Event::SpeechCut => {
                if self.voice_on || !matches!(ev, Event::Speaking(false)) {
                    // A cut (Stop or the audio cut) ends the look at once; only a natural end holds it for GRACE.
                    self.voice_off_at = Some(if matches!(ev, Event::Speaking(false)) { t } else { t - GRACE });
                }
                self.voice_on = false;
            }
            Event::Command(CommandCue::Heard { id, .. }) => {
                self.seen_ids.insert(*id);
            }
            Event::Health(HealthCue::Restarting { key, .. } | HealthCue::Problem { key, .. } | HealthCue::Drop { key, .. } | HealthCue::Busy { key, .. }) => {
                self.seen_keys.insert(key.clone());
            }
            Event::Notice { notice, .. } => {
                self.seen_notices.insert(notice.id.clone());
                self.note_icons.remove(&notice.id);
            }
            _ => {}
        }
        // I22: who opened the note, and whether anyone is looking at it.
        match ev {
            Event::Pointer(over) => {
                self.pointer = *over;
                self.note_unattended = None;
            }
            Event::Hover(target) => self.band_hot = *target == Some(Target::ReplyBand),
            // Dragging it (a pull, which a turn of notes alone takes too since 23 Oct) is attending to it.
            Event::Scroll(_) | Event::Click(Target::NoteClose(_)) | Event::Pull(Some(_)) => self.note_unattended = None,
            Event::Notice { notice, .. } if notice.detail.is_some() && notice.action.is_none() => {
                // A note opens the pull-down as the agent's unless the user has it open.
                if !self.island.reply_open() {
                    self.note_owner = false;
                }
                self.note_unattended = None;
            }
            Event::Click(Target::Token(super::Token::Reply)) | Event::OpenChat => {
                self.note_owner = true;
                self.band_hot = false;
            }
            Event::Click(Target::ReplyBand) => {
                self.note_owner = false;
                self.band_hot = false;
            }
            _ => {}
        }
    }

    fn check_rate(&mut self) {
        let s = self.island.motion_starts();
        if s.len() >= 4 {
            let n = s.len();
            if s[n - 1] - s[n - 4] < 1.0 - 1e-6 {
                let d = format!("4 motions in {:.3} s", s[n - 1] - s[n - 4]);
                self.find("I13 at most three motions a second", d);
            }
        }
    }

    fn step_frame(&mut self) -> f64 {
        let base = 1.0 / self.cfg.fps;
        match self.cfg.jitter {
            Some(_) => {
                let r = self.rng.unit();
                if r < 0.01 {
                    0.1 + self.rng.unit() * 1.9
                } else {
                    (base + (self.rng.unit() - 0.5) * 0.008).max(0.001)
                }
            }
            None => base,
        }
    }

    /// Runs time forward to `t1`, frame by frame while something moves, firing deadlines.
    fn advance(&mut self, t1: f64) {
        let mut loop_frames = 0usize;
        let mut guard = 0usize;
        while self.t < t1 {
            guard += 1;
            if guard > 2_000_000 {
                self.find("I6 nothing draws at rest", "the runner did not reach the next event (endless frames or deadlines)".into());
                return;
            }
            let need = self.island.needs_frames(self.t);
            let dl = self.island.deadline().filter(|d| d.is_finite());
            if let Some(d) = dl {
                if d <= self.t {
                    // Overdue (a late tick): fire it now; it must move on.
                    let t = self.t;
                    self.apply(Event::Tick, t);
                    if let Some(d2) = self.island.deadline() {
                        if d2 <= t {
                            self.find("I6 deadlines fire once", format!("deadline {d2:.4} still due after firing at {t:.4}"));
                            return;
                        }
                    }
                    self.check_frame();
                    continue;
                }
            }
            let next = match need {
                FrameNeed::Moving => {
                    loop_frames = 0;
                    self.t + self.step_frame()
                }
                FrameNeed::Loop { fps, .. } => {
                    loop_frames += 1;
                    let key = (self.island.motion().0, self.island.class());
                    if self.loop_checked == Some(key) {
                        loop_frames = usize::MAX / 2;
                    }
                    if loop_frames > (fps.max(1.0) * 1.0) as usize {
                        self.loop_checked = Some(key);
                        // One period of loop frames checked; the rest is the same loop.
                        loop_frames = 0;
                        dl.unwrap_or(t1).min(t1)
                    } else {
                        self.t + 1.0 / fps.max(1.0)
                    }
                }
                FrameNeed::None => {
                    loop_frames = 0;
                    self.check_rest();
                    dl.unwrap_or(t1).min(t1)
                }
            };
            let next = match dl {
                Some(d) if d < next => d,
                _ => next,
            };
            let next = next.min(t1).max(self.t + 1e-9);
            self.t = next;
            let mut quiet = false;
            if dl.is_some_and(|d| d <= next) {
                let t = self.t;
                let before = (self.island.motion().0, self.island.class(), self.island.targets().len());
                self.apply(Event::Tick, t);
                if let Some(d2) = self.island.deadline() {
                    if d2 <= t {
                        self.find("I6 deadlines fire once", format!("deadline {d2:.4} still due after firing at {t:.4}"));
                        return;
                    }
                }
                // A periodic tick that started no motion (a clock's next second): every 25th is checked in full.
                let after = (self.island.motion().0, self.island.class(), self.island.targets().len());
                if before == after && self.island.needs_frames(t) != FrameNeed::Moving {
                    self.quiet_ticks += 1;
                    quiet = self.quiet_ticks % 25 != 0;
                } else {
                    self.quiet_ticks = 0;
                }
            }
            if !quiet {
                self.check_frame();
            }
        }
    }

    fn check_rest(&mut self) {
        let step = 1.0 / self.cfg.fps;
        if self.island.deadline().is_some_and(|d| d <= self.t + step) {
            // A one-shot change lands in between: that is not drawing at rest.
            return;
        }
        let a = self.island.frame(self.t);
        let b = self.island.frame(self.t + step);
        if (FrameModel { t: 0.0, ..a.clone() }) != (FrameModel { t: 0.0, ..b.clone() }) {
            let what = if a.shape != b.shape {
                format!("shape {:?} → {:?}", a.shape, b.shape)
            } else if a.light != b.light || a.glow != b.glow {
                format!("light {:?} → {:?}", a.light, b.light)
            } else if a.pointing != b.pointing {
                format!("pointing {} → {}", a.pointing, b.pointing)
            } else {
                a.slots.iter().zip(&b.slots).find(|(x, y)| x != y).map_or_else(|| "slot count".to_string(), |(x, y)| format!("{x:?} → {y:?}"))
            };
            self.find("I6 nothing draws at rest", format!("two frames differ while nothing moves ({:?}): {what}", self.island.class()));
        }
        if self.island.class() == Class::Rest && (a.visible || !a.slots.is_empty()) {
            self.find("I6 nothing draws at rest", "rest draws something".into());
        }
        // I11: every target at rest.
        let targets: Vec<(Target, super::Rect)> = self.island.targets().to_vec();
        for (target, r) in &targets {
            if r.w < 28.0 || r.h < 24.0 {
                self.find("I11 hit targets", format!("{target:?} is {:.0} × {:.0} pt", r.w, r.h));
            }
            if !a.shape.contains_rect(r, 1.0) {
                self.find("I11 hit targets", format!("{target:?} {r:?} outside the shape {:?}", a.shape));
            }
            let hit = self.island.hit(self.t, r.center());
            if hit.as_ref() != Some(target) {
                self.find("I11 hit targets", format!("hit at {target:?}'s centre gives {hit:?}"));
            }
        }
    }

    fn check_frame(&mut self) {
        let bounds = self.motion_bounds();
        let f = self.island.frame(self.t);
        let mut found: Vec<(&'static str, String)> = vec![];
        self.report.frames += 1;
        self.report.classes.insert(self.island.class());
        // Trace hash (I14).
        self.t.to_bits().hash(&mut self.hasher);
        for v in [f.shape.w, f.shape.h, f.shape.flare, f.glow, f.light.run, f.pointing] {
            v.to_bits().hash(&mut self.hasher);
        }
        for s in &f.slots {
            s.id.hash(&mut self.hasher);
            for v in [s.rect.x, s.rect.y, s.rect.w, s.rect.h, s.opacity] {
                v.to_bits().hash(&mut self.hasher);
            }
        }
        // I1.
        let finite = f.shape.is_finite()
            && f.glow.is_finite()
            && f.light.run.is_finite()
            && f.light.opacity.is_finite()
            && f.pointing.is_finite()
            && f.slots.iter().all(|s| s.rect.is_finite() && s.opacity.is_finite() && s.blur.is_finite() && s.phase.is_finite());
        if !finite {
            self.find("I1 finite", format!("non-finite frame {f:?}"));
            self.prev = Some(f);
            return;
        }
        // I10.
        // The window holds the widest shape plus the spring's overshoot (about 1 %) and the flares.
        if f.shape.w > WINDOW_W - 2.0 * 30.0 || f.shape.w < 0.0 || f.shape.h < 0.0 || f.shape.h > self.display.height / 2.0 + 120.0 {
            found.push(("I10 bounds", format!("shape {:?}", f.shape)));
        }
        let (nw, nh) = self.display.notch_size();
        // I2.
        let visible: Vec<&super::Slot> = f.slots.iter().filter(|s| s.opacity > VISIBLE).collect();
        let free: HashSet<String> = self.island.layout_items().iter().filter(|i| i.free).map(|i| i.id.to_string()).collect();
        let column: Vec<&&super::Slot> = visible.iter().filter(|s| !s.ear && !s.id.contains('~') && !free.contains(&*s.id.to_string())).collect();
        if !column.is_empty() {
            let l = column.iter().map(|s| s.rect.x).fold(f64::INFINITY, f64::min);
            let r = column.iter().map(|s| s.rect.max_x()).fold(f64::NEG_INFINITY, f64::max);
            if (l + r).abs() > 1.0 {
                found.push(("I2 symmetric", format!("column content spans {l:.1}..{r:.1} ({:?})", self.island.class())));
            }
        }
        // Ears as two groups (a clock is a dot and a time centred together): both or neither, mirrored.
        // Every drawn ear counts (a dimmed partner fades in on the same frames as a full one).
        let drawn: Vec<&super::Slot> = f.slots.iter().collect();
        // Leaving ears count too: a pair changing one side cross-fades in place.
        let ears: Vec<&&super::Slot> = drawn.iter().filter(|s| s.ear).collect();
        let side = |left: bool| ears.iter().filter(|e| (e.rect.mid_x() < 0.0) == left).map(|e| e.rect).reduce(|a, b| a.union(&b));
        match (side(true), side(false)) {
            (Some(l), Some(r)) => {
                if (l.mid_x() + r.mid_x()).abs() > 1.0 {
                    found.push(("I2 symmetric", format!("ears centred at {:.1} and {:.1}", l.mid_x(), r.mid_x())));
                }
            }
            (Some(e), None) | (None, Some(e)) => {
                // A dimmed partner reaches the draw threshold a frame sooner while both fade: only a visible lone ear
                // counts.
                let strong = ears.iter().any(|x| (x.rect.mid_x() < 0.0) == (e.mid_x() < 0.0) && x.opacity > VISIBLE);
                if strong {
                    found.push(("I2 symmetric", format!("one ear alone at x {:.1} ({:?})", e.mid_x(), self.island.class())));
                }
            }
            (None, None) => {}
        }
        // I3.
        for s in &f.slots {
            if !self.times.contains(&s.since.to_bits()) {
                found.push(("I3 one clock", format!("{} started at {:.4}, not an event or deadline time", s.id, s.since)));
            }
        }
        // I5.
        for s in &visible {
            if s.clip {
                continue;
            }
            let r = s.rect;
            if !f.shape.contains_rect(&r, 1.5) {
                found.push(("I5 content inside the shape", format!("{} {:?} op {:.2} outside {:?} ({:?})", s.id, r, s.opacity, f.shape, self.island.class())));
            }
            if self.display.has_notch() && r.max_x() > -nw / 2.0 + 0.5 && r.x < nw / 2.0 - 0.5 && r.y < nh - 0.5 && r.h > 0.0 {
                found.push(("I5 nothing under the camera", format!("{} {:?} under the notch", s.id, r)));
            }
        }
        // I4.
        if let Some(p) = &self.prev {
            let dt = (f.t - p.t).max(1e-6);
            let k = (dt * 60.0).max(1.0);
            // A display change is a declared cut (the island moved to another display).
            let both_visible = p.visible && f.visible && p.display == f.display;
            if both_visible {
                // No faster than the spring itself: the old fixed bounds (wing 24, drop 16 pt a 60 Hz frame), or the
                // grow spring's peak speed for this motion's size (about 10 % of the change a frame), whichever is
                // larger. A snap moves the whole change in one frame and is caught either way.
                // The calm (critically damped) spring peaks at ω/e ≈ 11 % of the change a frame; a retarget may
                // carry its velocity in (the track's first step).
                let (a, b, vw, vh) = bounds;
                let mut wing = (24.0f64).max(0.13 * (b.w - a.w).abs() / 2.0).max(1.2 * vw / 2.0);
                let mut drop = (16.0f64).max(0.13 * (b.h - a.h).abs()).max(1.2 * vh);
                // A gap that spans two motions moves as fast as either.
                if p.motion != f.motion {
                    if let Some((id, a, b, vw, vh)) = self.motion_before {
                        if id == p.motion.0 {
                            wing = wing.max(0.13 * (b.w - a.w).abs() / 2.0).max(1.2 * vw / 2.0);
                            drop = drop.max(0.13 * (b.h - a.h).abs()).max(1.2 * vh);
                        }
                    }
                }
                if (f.shape.w - p.shape.w).abs() / 2.0 > wing * k + 1e-6 {
                    found.push(("I4 no jumps", format!("width {:.1} → {:.1} in {:.4} s", p.shape.w, f.shape.w, dt)));
                }
                if (f.shape.h - p.shape.h).abs() > drop * k + 1e-6 {
                    found.push(("I4 no jumps", format!("height {:.1} → {:.1} in {:.4} s (motion {:?} → {:?}, {:?})", p.shape.h, f.shape.h, dt, p.motion, f.motion, self.island.scene())));
                }
                // The radius follows the height (0.4 pt per point until it caps), so it moves at most that much faster.
                if (f.shape.radius() - p.shape.radius()).abs() > (6.0f64).max(0.4 * drop) * k + 1e-6 {
                    found.push(("I4 no jumps", format!("radius {:.1} → {:.1}", p.shape.radius(), f.shape.radius())));
                }
            }
            let live: HashSet<String> = self.island.layout_items().iter().filter(|i| i.live).map(|i| i.id.to_string()).collect();
            let bound = 0.35 * k + 1e-6;
            for s in &f.slots {
                if live.contains(&*s.id.to_string()) {
                    continue;
                }
                // A ghost continues the element it took over (same id before the '~').
                let orig = s.id.split('~').next().unwrap_or("");
                let before = p.slots.iter().find(|q| q.id == s.id).map(|q| q.opacity).unwrap_or_else(|| {
                    p.slots.iter().filter(|q| q.id.split('~').next() == Some(orig) && !f.slots.iter().any(|x| x.id == q.id)).map(|q| q.opacity).fold(0.0, f64::max)
                });
                if (s.opacity - before).abs() > bound && p.display == f.display {
                    found.push(("I4 no jumps", format!("{} opacity {before:.2} → {:.2} in {:.4} s", s.id, s.opacity, dt)));
                }
            }
            for q in &p.slots {
                if !f.slots.iter().any(|s| s.id == q.id) && q.opacity > bound && !live.contains(&*q.id.to_string()) && p.display == f.display {
                    // A ghost takes over a vanishing element under a new id: that is a continuation, not a cut.
                    let orig = q.id.split('~').next().unwrap_or("");
                    let continued = f.slots.iter().any(|s| s.id.split('~').next() == Some(orig));
                    if !continued {
                        found.push(("I4 no jumps", format!("{} vanished at opacity {:.2}", q.id, q.opacity)));
                    }
                }
            }
        }
        // I15: an empty shape at full size while arriving (the speaking box that opened empty).
        let column_items: Vec<String> = self.island.layout_items().iter().filter(|i| !i.ear && !matches!(&*i.content, super::Content::Text { lines, .. } if lines.is_empty())).map(|i| i.id.to_string()).collect();
        let at_size = (f.shape.w - f.target.0).abs() < 2.0 && (f.shape.h - f.target.1).abs() < 2.0;
        // An arrival: this motion changed the size (a swap at one size is a cross-fade, not an empty box).
        if self.motion_from.is_none_or(|(id, _)| id != f.motion.0) {
            // The size the motion before this one was heading for.
            let before = self.last_target.unwrap_or(f.target);
            self.motion_from = Some((f.motion.0, before));
        }
        self.last_target = Some(f.target);
        let grew = self.motion_from.is_some_and(|(_, (w, h))| (w - f.target.0).abs() > 2.0 || (h - f.target.1).abs() > 2.0);
        let at_size = at_size && grew;
        // Empty: nothing of it drawn (a sequential cross-fade between two contents passes through low opacities by
        // design; the bug is a box with no content at all).
        let content_shows = f.slots.iter().any(|s| (column_items.iter().any(|c| **c == *s.id) || s.id.contains('~')) && !s.ear && s.opacity >= 0.05);
        if f.visible && f.moving && at_size && !column_items.is_empty() && !content_shows {
            let since = *self.empty_since.get_or_insert(f.t);
            if f.t - since > 0.05 + 1e-9 {
                found.push(("I15 no empty shape at full size", format!("{:.0}×{:.0} without its content for {:.3} s ({:?})", f.shape.w, f.shape.h, f.t - since, self.island.class())));
            }
        } else {
            self.empty_since = None;
        }
        // I16: never shorter than the notch while at least as wide (the notch would hang below the shape).
        if let Some((nw, nh)) = self.display.notch {
            if f.visible && f.shape.w >= nw && f.shape.h < nh - 0.01 {
                found.push(("I16 the notch never shows below the shape", format!("{:.1}×{:.1} under a {nw:.0}×{nh:.0} notch", f.shape.w, f.shape.h)));
            }
        }
        // I17: two texts never over each other (both above 20 %).
        let texty = |s: &super::Slot| match &*s.content {
            super::Content::Text { lines, .. } => !lines.is_empty(),
            super::Content::Unit { .. } | super::Content::Split { .. } => true,
            super::Content::Capsule { label, .. } => !label.is_empty(),
            _ => false,
        };
        let texts: Vec<&super::Slot> = f.slots.iter().filter(|s| texty(s) && s.opacity > 0.2).collect();
        'pairs: for (i, a) in texts.iter().enumerate() {
            for b in &texts[i + 1..] {
                let (x0, y0) = (a.rect.x.max(b.rect.x), a.rect.y.max(b.rect.y));
                let (x1, y1) = (a.rect.max_x().min(b.rect.max_x()), a.rect.max_y().min(b.rect.max_y()));
                let inter = (x1 - x0).max(0.0) * (y1 - y0).max(0.0);
                let smaller = (a.rect.w * a.rect.h).min(b.rect.w * b.rect.h).max(1e-6);
                if inter / smaller > 0.2 {
                    found.push(("I17 no text over text", format!("{} ({:.2}) over {} ({:.2})", a.id, a.opacity, b.id, b.opacity)));
                    break 'pairs;
                }
            }
        }
        // I18: a reopened note keeps its icon (a note that left and comes back with no new notice for it).
        let showing: Vec<(String, String)> = match self.island.scene() {
            super::Scene::Reply { notes, .. } => notes.iter().map(|n| (n.id.clone(), n.icon.name.clone())).collect(),
            _ => vec![],
        };
        for (id, name) in &showing {
            let reopened = !self.note_last.contains(id);
            match self.note_icons.get(id) {
                Some(first) if reopened && first != name => {
                    found.push(("I18 a reopened element keeps its kind", format!("note {id} was {first}, reopened as {name}")));
                }
                _ => {
                    self.note_icons.insert(id.clone(), name.clone());
                }
            }
        }
        self.note_last = showing.into_iter().map(|(id, _)| id).collect();
        // I8. The pulled-down reply keeps the voice's ears by the user's choice; its bars must be flat once the audio
        // has ended.
        let reply_open = matches!(self.island.scene(), super::Scene::Reply { .. });
        let voice_visible = !reply_open && f.slots.iter().any(|s| (&*s.id == "voice.helmet" || &*s.id == "voice.wave") && s.opacity > VISIBLE);
        let bars_moving = f.slots.iter().any(|s| &*s.id == "voice.wave" && s.opacity > VISIBLE && matches!(&*s.content, super::Content::Waveform { flat: false, .. }));
        if reply_open && bars_moving && !self.voice_on && self.voice_off_at.is_none_or(|off| self.t > off + GRACE + 0.7) {
            found.push(("I8 speaking ends with the audio", "the pulled-down reply's bars move after the audio ended".into()));
        }
        if voice_visible && !self.voice_on {
            let late = self.voice_off_at.map_or(true, |off| self.t > off + GRACE + 0.7);
            if late {
                found.push(("I8 speaking ends with the audio", format!("the voice's look shows {:.2} s after the audio ended", self.voice_off_at.map_or(f64::NAN, |o| self.t - o))));
            }
        }
        // I19: after the audio ended, with nothing new since, no check mark arrives in the turn's lip (the removed
        // "done" stage; a command's or the drawing keys' own moment that waited is not the voice's ending).
        let lip = self.island.class() == Class::Lip;
        if let Some(before) = self.after_voice.as_ref().filter(|_| !self.island.drawing()) {
            if let Some(s) = f.slots.iter().find(|s| s.opacity > VISIBLE && is_check(s) && (lip || s.id.starts_with("lip.")) && !before.contains(&*s.id.to_string())) {
                found.push(("I19 no success after speech", format!("{} appeared {:.2} s after the audio ended", s.id, self.voice_off_at.map_or(f64::NAN, |o| self.t - o))));
            }
        }
        // I20: while the voice plays (by the island's own account: a stalled voice has ended) its sentences only grow.
        if self.voice_on && self.island.voice_on() {
            let now = self.island.spoken().to_vec();
            if let Some(prev) = &self.spoken_prev {
                let capped = prev.len() >= super::scene::MAX_SENTENCES;
                if !capped && !now.starts_with(prev) {
                    found.push(("I20 a reply's words only grow", format!("{prev:?} became {now:?}")));
                }
            }
            self.spoken_prev = Some(now);
        } else {
            self.spoken_prev = None;
        }
        // I22: a pull-down an agent's note opened, on screen with nobody at it, pulls itself up (bug note
        // 2026-10-20T18-41-17: a note left open read as a stuck island).
        let note_open = self.island.class() == Class::Reply && matches!(self.island.scene(), super::layout::Scene::Reply { notes, .. } if !notes.is_empty());
        if note_open && !self.note_owner && !self.pointer && !self.band_hot {
            let since = *self.note_unattended.get_or_insert(self.t);
            if self.t > since + NOTE_UNATTENDED + 0.7 {
                found.push(("I22 an agent's note pulls itself up", format!("open {:.2} s with the pointer away", self.t - since)));
            }
        } else {
            self.note_unattended = None;
        }
        // I21: the target shape is no bigger than its content needs ("the animation for you thinking is
        // literally three dots. It doesn't need to make the dynamic island thicker").
        if let Some(d) = super::layout::oversize(self.island.layout(), &self.island.display()) {
            found.push(("I21 sized to its content", d));
        }
        for (inv, d) in found {
            self.find(inv, d);
        }
        self.prev = Some(f);
    }

    fn run_steps(&mut self, steps: &[(f64, Step)]) {
        let mut steps: Vec<&(f64, Step)> = steps.iter().collect();
        steps.sort_by(|a, b| a.0.total_cmp(&b.0));
        for (te, step) in steps {
            let te = if te.is_finite() { te.max(self.t) } else { self.t };
            self.advance(te);
            self.t = te;
            match step {
                Step::Ev(ev) => self.apply(ev.clone(), te),
                Step::ClickAny(n) => {
                    let targets = self.island.targets().to_vec();
                    if !targets.is_empty() {
                        let target = targets[n % targets.len()].0.clone();
                        self.apply(Event::Click(target), te);
                    }
                }
                Step::Crash => {
                    self.island = Island::new(self.display, measure(self.cfg.text));
                    self.pointer = false;
                    self.band_hot = false;
                    self.note_owner = false;
                    self.note_unattended = None;
                    self.prev = None;
                    self.after_voice = None;
                    self.spoken_prev = None;
                    self.voice_on = false;
                    self.voice_off_at = None;
                }
            }
            if let Step::Ev(Event::Display(d)) = step {
                self.display = d.sanitized();
            }
            self.check_frame();
        }
    }

    /// I7 and I9: withhold events; the island ends at rest or in a declared persistent state, whose exit works.
    fn exit(&mut self) {
        let start = self.t;
        self.advance(start + EXIT_WAIT);
        if self.island.class() == Class::Rest {
            return self.check_lines();
        }
        let class = self.island.class();
        let persistent = matches!(class, Class::Question | Class::Reply | Class::Reader | Class::Drawing | Class::Ears | Class::Stack | Class::StackButton | Class::Lip);
        if !persistent {
            self.find("I7 every state exitable", format!("{class:?} held for an hour without events"));
        }
        // The exits the user and the app have: answer or dismiss, pull up, close, clear, decide, end.
        let t = self.t;
        let mut exits: Vec<Event> = vec![Event::DrawClose, Event::ClearNotices, Event::Activity(None), Event::Speaking(false), Event::Point(0)];
        // Listening and Seeing off hold their token until they are on again (24 Oct).
        exits.extend([Switch::Listening, Switch::Seeing].map(|switch| Event::Switch { switch, on: true, moment: false }));
        // Dictating to Vella holds its token until Vella says it stopped.
        exits.push(Event::Vella { on: false, keys: None });
        // Emergency mode's token holds until emergency mode ends (docs/claude-relay.md).
        exits.push(Event::Emergency(false));
        for id in &self.seen_ids {
            exits.push(Event::Command(CommandCue::Dropped { id: *id }));
        }
        for key in &self.seen_keys {
            exits.push(Event::Health(HealthCue::Clear { key: key.clone() }));
        }
        for ev in exits {
            self.apply(ev, t);
        }
        // Click whatever the island offers (tokens, dismiss, the reader's band) until it is at rest.
        for round in 0..12 {
            let t = self.t + 0.5;
            self.advance(t);
            if self.island.class() == Class::Rest {
                break;
            }
            // Clicks that close (a note's or the reader's token would open it again: they expire on their own).
            let closing = |t: &Target| matches!(t, Target::ReaderBand | Target::ReplyBand | Target::Dismiss(_) | Target::NoteClose(_) | Target::AskPick { .. } | Target::AskSend(_) | Target::Token(super::Token::Problem) | Target::HealthButton);
            if let Some((target, _)) = self.island.targets().iter().find(|(t, _)| closing(t)).cloned() {
                let t = self.t;
                self.apply(Event::Click(target), t);
            }
            let _ = round;
        }
        let t = self.t;
        self.advance(t + EXIT_WAIT);
        if self.island.class() != Class::Rest {
            self.find("I7 every state exitable", format!("{:?} remains after its exits ({:?})", self.island.class(), self.island.scene()));
        }
        self.check_lines();
    }

    fn check_lines(&mut self) {
        if !self.island.line_ids().is_empty() {
            let ids = self.island.line_ids();
            self.find("I9 lines clear", format!("lines {ids:?} still waiting after an hour"));
        }
    }
}

/// A success mark: a check symbol.
fn is_check(s: &super::Slot) -> bool {
    matches!(&*s.content, super::Content::Symbol { name, .. } if name.contains("checkmark"))
}

/// Plays `s` and checks every frame.
pub fn run(s: &Scenario, cfg: &Config) -> Report {
    let mut r = Runner::new(cfg, s.display.sanitized());
    r.island.set_wall_origin(0.0);
    r.run_steps(&s.steps);
    if cfg.exit {
        r.exit();
    }
    r.report.end = Some(r.island.class());
    r.report.motions = r.island.motion_starts().len();
    r.report.hash = r.hasher.finish();
    r.report
}

/// Plays `s` and returns every frame at `fps` from 0 to `end` (for renders): the frame and the event that landed
/// at it, if any.
pub fn frames(s: &Scenario, fps: f64, end: f64, text: Box<dyn Measure>) -> Vec<FrameModel> {
    frames_timed(s, fps, end, text).0
}

/// As `frames`, with the seconds each `Island::frame` call took (the model's share of the main thread per frame).
pub fn frames_timed(s: &Scenario, fps: f64, end: f64, text: Box<dyn Measure>) -> (Vec<FrameModel>, Vec<f64>) {
    let mut island = Island::new(s.display.sanitized(), text);
    let mut steps: Vec<&(f64, Step)> = s.steps.iter().collect();
    steps.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut out = vec![];
    let mut cost = vec![];
    let mut k = 0usize;
    let n = (end * fps).ceil() as usize;
    let mut next_step = 0;
    for i in 0..=n {
        let t = i as f64 / fps;
        // Everything due by this frame, in order: deadlines and steps.
        loop {
            let dl = island.deadline().filter(|d| *d <= t);
            let st = steps.get(next_step).filter(|(ts, _)| *ts <= t).map(|(ts, _)| *ts);
            match (dl, st) {
                (Some(d), Some(s)) if d < s => island.apply(Event::Tick, d),
                (Some(d), None) => island.apply(Event::Tick, d),
                (_, Some(_)) => {
                    let (ts, step) = steps[next_step];
                    next_step += 1;
                    match step {
                        Step::Ev(ev) => island.apply(ev.clone(), *ts),
                        Step::ClickAny(n) => {
                            let targets = island.targets().to_vec();
                            if !targets.is_empty() {
                                island.apply(Event::Click(targets[n % targets.len()].0.clone()), *ts);
                            }
                        }
                        Step::Crash => island = Island::new(s.display.sanitized(), Box::new(Approx)),
                    }
                    for o in island.take_outputs() {
                        if matches!(o, Output::DrawUndo | Output::DrawClear) {
                            island.apply(Event::Ink(0), *ts);
                        }
                    }
                }
                (None, None) => break,
            }
            k += 1;
            if k > 1_000_000 {
                break;
            }
        }
        let started = std::time::Instant::now();
        let f = island.frame(t);
        cost.push(started.elapsed().as_secs_f64());
        out.push(f);
    }
    (out, cost)
}

// ---------------------------------------------------------------------- generators

/// The closed event vocabulary with degenerate parameters (TOOLING.md §1.3); `k` picks one.
pub fn vocabulary(k: usize, rng_bits: u64) -> Step {
    let r = rng_bits;
    let label = |i: u64| -> String {
        match i % 9 {
            0 => String::new(),
            1 => "x".into(),
            2 => "Dave's fix landed".into(),
            3 => "word ".repeat(120),
            4 => "\n\n".into(),
            5 => "🚀✨ done".into(),
            6 => "שלום עולם".into(),
            7 => "a".repeat(80),
            _ => "Build failed".into(),
        }
    };
    let kinds = [NoticeKind::Info, NoticeKind::Success, NoticeKind::Warning, NoticeKind::Error, NoticeKind::Looking, NoticeKind::Working];
    let parts = Part::ALL;
    let id = (r >> 8) % 4;
    let ev = match k % VOCABULARY {
        0 => Event::Speaking(true),
        1 => Event::Speaking(false),
        2 => Event::SpokenText(Some(label(r))),
        3 => Event::SpokenText(None),
        4 => Event::SpeechLevel([f32::NAN, -1.0, 2.0, 0.5, 0.0][(r % 5) as usize]),
        5 => Event::Sent,
        6 => Event::Activity(Some(Activity::Thinking)),
        7 => Event::Activity(Some(Activity::Compacting)),
        8 => Event::Activity(None),
        9 => if r % 2 == 0 { Event::Stop } else { Event::SpeechCut },
        10 | 11 | 12 => Event::Notice { notice: AgentNotice::line(format!("n{id}"), label(r >> 3), kinds[(r % 6) as usize]), body: None },
        13 => {
            let detail = [Detail::Text("log line\nerror".into()), Detail::Table { headers: vec!["a".into()], rows: vec![vec!["1".into()]] }, Detail::Svg("<svg/>".into()), Detail::Image { archive_id: "x".into() }][(r % 4) as usize].clone();
            let body = [None, Some((0.0, 0.0)), Some((344.0, 120.0)), Some((344.0, 5000.0)), Some((f64::NAN, 3.0))][((r >> 4) % 5) as usize];
            let mut n = AgentNotice::line(format!("d{id}"), label(r >> 5), kinds[(r % 4) as usize]);
            n.detail = Some(detail);
            Event::Notice { notice: n, body }
        }
        14 | 15 => {
            // Two to four options, as the `ask` contract allows.
            let n_opts = 2 + (r % 3) as usize;
            let options: Vec<String> = ["Yes", "No", "Main", "Island", "A much longer answer label", "None"].iter().cycle().take(n_opts).map(|s| s.to_string()).collect();
            let expires_ms = [0u64, 2_000, 600_000, u64::MAX / 4][((r >> 3) % 4) as usize];
            let mut n = AgentNotice::line(format!("q{id}"), if r % 3 == 0 { label(r >> 7) } else { "Install the new build?".into() }, NoticeKind::Info);
            let labels: Vec<&str> = options.iter().map(String::as_str).collect();
            n.action = Some(NoticeAction::single(&format!("q{id}"), "Install the new build?", &labels, expires_ms));
            Event::Notice { notice: n, body: None }
        }
        16 => {
            let mut n = AgentNotice::line(format!("t3-connect-{id}"), "", NoticeKind::Warning);
            n.action = Some(NoticeAction::ConnectT3);
            Event::Notice { notice: n, body: None }
        }
        17 => Event::NoticeClear([format!("n{id}"), format!("q{id}"), "unknown".into(), format!("d{id}")][(r % 4) as usize].clone()),
        18 => Event::ClearNotices,
        19 => Event::CloseQuestion { id: format!("q{id}"), answer: ["yes", "No.", "maybe", ""][(r % 4) as usize].into() },
        20 => Event::Command(CommandCue::Heard { id, kind: [CommandKind::Screenshot, CommandKind::LookAtThis, CommandKind::Cancel, CommandKind::Listening, CommandKind::Other][(r % 5) as usize] }),
        21 => Event::Command(CommandCue::Done { id: if r % 3 == 0 { None } else { Some(id) }, kind: [CommandKind::Screenshot, CommandKind::Listening, CommandKind::Other][(r % 3) as usize], label: label(r >> 4) }),
        22 => Event::Command(CommandCue::Failed { id: Some(id), kind: CommandKind::Screenshot, reason: "Seeing is off".into() }),
        23 => Event::Command(CommandCue::Dropped { id }),
        24 => Event::Health(HealthCue::Restarting { key: format!("k{}", id % 2), part: parts[(r % 10) as usize], label: "Restarting transcription".into() }),
        25 => Event::Health(HealthCue::Recovered { key: format!("k{}", id % 2), part: parts[(r % 10) as usize], label: "Transcription back".into() }),
        26 => Event::Health(HealthCue::Problem { key: format!("k{}", id % 2), part: Part::Screen, label: "Screen Recording off".into(), action: if r % 4 == 0 { String::new() } else { "Open Settings".into() }, control: HealthControl::OpenSettings(crate::model::Permission::ScreenRecording) }),
        27 => Event::Health(HealthCue::Drop { key: "mic".into(), part: Part::Microphone, label: "AirPods disconnected".into() }),
        // A notice, or catching up (on the restart's keys, with a count that changes as the app resends it).
        28 if r % 2 == 0 => Event::Health(HealthCue::Notice { part: Part::Microphone, label: label(r) }),
        28 => Event::Health(HealthCue::Busy { key: format!("k{}", id % 2), part: Part::Agent, label: format!("Catching up · {} notes to save", (r >> 3) % 20_000) }),
        29 => Event::Health(HealthCue::Clear { key: format!("k{}", id % 2) }),
        30 | 31 => Event::ListeningOff,
        32 => Event::Reader(FastReadUpdate::Show(FastRead {
            id: format!("r{id}"),
            text: ["", "One.", "The build failed because tests timed out. Three things matter. First, the limit is two minutes, and the tests need longer.", &"word ".repeat(2000)][(r % 4) as usize].to_string(),
            title: if r % 5 == 0 { None } else { Some("Build report".into()) },
            wpm: [0u32, 240, 450, 9999][((r >> 3) % 4) as usize],
        })),
        33 => Event::Reader(FastReadUpdate::Withdraw(format!("r{id}"))),
        34 => Event::DrawOpen,
        35 => Event::DrawClose,
        36 => Event::Ink((r % 4) as usize),
        37 => Event::CaptureFailed(label(r)),
        38 => Event::Point([0usize, 1, 40][(r % 3) as usize]),
        39 => Event::Display([Display::MBP16, Display::PLAIN, Display { width: 1280.0, height: 800.0, scale: 1.0, notch: None }, Display { width: 0.0, height: f64::NAN, scale: 0.0, notch: Some((0.0, 0.0)) }][(r % 4) as usize]),
        40 => Event::Scroll([40.0, -1e9, f64::NAN, 1e9][(r % 4) as usize]),
        41 if (r >> 20) % 3 == 0 => Event::Pointer((r >> 22) % 2 == 0),
        41 => Event::Hover([None, Some(Target::DrawSend), Some(Target::DrawClose), Some(Target::ReplyBand)][(r % 4) as usize].clone()),
        42 => Event::Press([None, Some(Target::DrawSend)][(r % 2) as usize].clone()),
        43 => Event::LastReply(match r % 5 {
            0 => String::new(),
            1 => "The build is green.\n\nKuba pushed the audio fix an hour ago.\n- one\n- two".into(),
            2 => "Sentence after sentence. ".repeat(80),
            3 => "\n\n  \n".into(),
            _ => label(r >> 2),
        }),
        // Night 1022: a floating island's nudge; a drag on the pulled-down reply (released, or held at odd values).
        44 => Event::Nudge { question: r % 2 == 0 },
        45 => Event::Pull([None, Some(4.0), Some(30.0), Some(80.0), Some(f64::NAN), Some(-20.0), None][(r % 7) as usize]),
        // 24 Oct: a switch turned (a shortcut or a tile), or its state told without a moment. It shares the last
        // entry, so the generators' draws (and every seeded sequence without it) stay as they were.
        46 if (r >> 20) % 2 == 0 => Event::Switch { switch: [Switch::Listening, Switch::Seeing, Switch::Islands][(r % 3) as usize], on: (r >> 2) % 2 == 0, moment: (r >> 3) % 4 != 0 },
        // Dictating to Vella started or stopped, with its shortcut (one that fits an ear, one that does not, none).
        46 if (r >> 20) % 4 == 1 => Event::Vella { on: (r >> 2) % 3 != 0, keys: [None, Some("⌃⌘N".into()), Some("⌃⌥Space".into())][(r % 3) as usize].clone() },
        _ => return if r % 7 == 0 { Step::Crash } else { Step::ClickAny((r % 8) as usize) },
    };
    Step::Ev(ev)
}

/// How many vocabulary entries there are.
pub const VOCABULARY: usize = 47;

/// A seeded random sequence: 5–60 steps, weighted towards bursts and interruptions (TOOLING.md §1.4, generator 3).
pub fn random(seed: u64) -> Scenario {
    let mut rng = Rng(seed.wrapping_mul(0x9E3779B97F4A7C15) | 1);
    let n = 5 + rng.below(56);
    let mut t = 0.0;
    let mut steps = vec![];
    for _ in 0..n {
        let gap = match rng.below(10) {
            0 => 0.0,
            1..=4 => rng.unit() * 0.3,
            5..=7 => rng.unit() * 2.0,
            8 => rng.unit() * 15.0,
            _ => 8.0 * 3600.0 * (rng.below(20) == 0) as u8 as f64 + rng.unit() * 0.05,
        };
        t += gap;
        let k = rng.below(VOCABULARY);
        let bits = rng.next();
        steps.push((t, vocabulary(k, bits)));
    }
    let display = match rng.below(4) {
        0 => Display::PLAIN,
        _ => Display::MBP16,
    };
    Scenario { id: format!("random-{seed}"), what: "seeded random sequence".into(), display, steps }
}

/// The phases at which the next event lands in a running motion (TOOLING.md §1.4, generator 2).
pub const PHASES: [f64; 8] = [0.0, 1.0 / 120.0, 0.11, 0.22, 0.40, 1.0, 20.0, 2.9];

/// The combinatorial grid's cell: event `a` at 0.1 s, then `b` at 0.1 + `phase`.
pub fn pair(a: usize, b: usize, phase: usize, bits: u64) -> Scenario {
    let pa = vocabulary(a, bits);
    let pb = vocabulary(b, bits.rotate_left(17));
    let dt = PHASES[phase % PHASES.len()];
    Scenario { id: format!("pair-{a}-{b}-{phase}"), what: "grid pair".into(), display: Display::MBP16, steps: vec![(0.1, pa), (0.1 + dt, pb)] }
}

/// A triple: three events, the later two landing at grid phases.
pub fn triple(a: usize, b: usize, c: usize, pb: usize, pc: usize, bits: u64) -> Scenario {
    let t1 = 0.1 + PHASES[pb % PHASES.len()];
    let t2 = t1 + PHASES[pc % PHASES.len()];
    Scenario {
        id: format!("triple-{a}-{b}-{c}-{pb}-{pc}"),
        what: "grid triple".into(),
        display: Display::MBP16,
        steps: vec![(0.1, vocabulary(a, bits)), (t1, vocabulary(b, bits.rotate_left(13))), (t2, vocabulary(c, bits.rotate_left(29)))],
    }
}

/// Coverage of size classes across many reports.
pub fn coverage<'a>(reports: impl IntoIterator<Item = &'a Report>) -> BTreeMap<Class, usize> {
    let mut m = BTreeMap::new();
    for r in reports {
        for c in &r.classes {
            *m.entry(*c).or_insert(0) += 1;
        }
    }
    m
}

/// A compact frame-by-frame listing of `s` between `from` and `to` (for diagnosing a finding).
pub fn trace(s: &Scenario, fps: f64, from: f64, to: f64) -> String {
    let frames = frames(s, fps, to, Box::new(Approx));
    let mut out = String::new();
    for f in frames.iter().filter(|f| f.t >= from - 1e-9) {
        out += &format!("t {:.4} {}×{} m{}:", f.t, (f.shape.w * 10.0).round() / 10.0, (f.shape.h * 10.0).round() / 10.0, f.motion.0);
        for sl in &f.slots {
            out += &format!(" {}[{:.2} x{:.0}..{:.0}]", sl.id, sl.opacity, sl.rect.x, sl.rect.max_x());
        }
        out += "\n";
    }
    out
}
