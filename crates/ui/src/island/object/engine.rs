//! The island object: state machine + layout + one motion clock (DESIGN.md §4).
//!
//! There is one transition. When the scene changes, the shape springs from its current geometry and velocity to the
//! new layout's size, and every content element follows the shape's progress `p` (the furthest the spring has
//! reached, so an overshoot never flickers content):
//! - an element in both states (same id, same content) stays and glides on the spring;
//! - an incoming element fades in with smoothstep(0.1, 0.7, p), blur 8 → 0, scaled to fit the growing shape about
//!   the top centre, and never shows a line the bottom edge has not passed; ears never scale and wait until the
//!   shape is wide enough for them;
//! - an outgoing element leaves with the shape: a short row fades early (smoothstep(0.1, 0.6, p)), a large body
//!   retracting is the last thing to leave (smoothstep(0.5, 1.0, p), scaled by the square root of the shrink), so the
//!   shape is never an empty slab.
//! A new target mid-flight retargets the same spring from its current position and velocity and snapshots every
//! visible element, so nothing restarts from zero and nothing jumps. Content-only changes of live elements (reader
//! units, numerals, the waveform, the clock) are cuts and start no motion. At most three motions start per second:
//! a change arriving sooner waits for the next slot (a deadline).

use std::collections::HashMap;
use std::sync::Arc;

use super::geom::{clamp01, smooth, Display, Point, Rect, Shape};
use super::layout::{layout, Class, Item, Layout, Scene};
use super::measure::Measure;
use super::scene::Model;
use super::spring::{Spring, CALM, GROW};
use super::{Content, Event, FrameModel, FrameNeed, Fx, Output, Slot, Target, Track};

/// Motions start at most this often (three a second).
pub const MOTION_GAP: f64 = 1.0 / 3.0;
/// How close is settled, in points (a fifth of a pixel at 2×: the last frame's snap to the target is invisible).
const SETTLE_EPS: f64 = 0.1;
/// What leaves a place clears in this long (on the clock).
pub const YIELD_OUT: f64 = 0.10;
/// A held carried text (`held_glide`) keeps its place until the yielding text under it is below a fifth of its opacity
/// (80 % of `YIELD_OUT`, the yielding text also drifts up), then glides over the next 40 ms.
const HOLD_FROM: f64 = 0.8 * YIELD_OUT;
const HOLD_TO: f64 = 1.2 * YIELD_OUT;
/// What arrives in a place another element leaves starts this late, and fades in over `FADE_IN`: the two overlap
/// only below a fifth of their opacity (never text over text) and the place is never empty for a visible frame.
pub const WAIT_IN: f64 = 0.06;
pub const FADE_IN: f64 = 0.14;
/// What arrives where a gliding text passes waits for the glide to leave.
pub const GLIDE_WAIT: f64 = 0.20;
/// Arrival effects (the system's bounce, wiggle) start once the shape has covered this much of its motion and the
/// symbol is nearly opaque: the symbol bounces as it lands, not after the shape has come to rest.
const FX_PROGRESS: f64 = 0.92;
/// A change arriving this soon after a motion began, while the shape is still growing, redirects that motion
/// instead of waiting for the next slot (the words arriving 0.12 s after the voice are one arrival, not two).
const EARLY_RETARGET: f64 = 0.25;
/// The track's sampling step.
pub const TRACK_STEP: f64 = 1.0 / 120.0;

#[derive(Clone, Copy, Debug)]
struct Motion {
    id: u64,
    start: f64,
    spring: Spring,
    /// w, h, flare: position and velocity at the start.
    from: [(f64, f64); 3],
    to: Shape,
    /// When the shape and the content are exactly at the target.
    settle: f64,
    /// When the shape is within a point of its target and the content has arrived: targets take clicks from here.
    visual: f64,
    /// When arrival effects may start (`FX_PROGRESS` of the spring).
    fx_at: f64,
    /// The size it started from (for the shrinking test).
    from_area: f64,
}

impl Motion {
    fn at_rest(shape: Shape, t: f64) -> Motion {
        Motion { id: 0, start: t, spring: CALM, from: [(shape.w, 0.0), (shape.h, 0.0), (shape.flare, 0.0)], to: shape, settle: t, visual: t, fx_at: t, from_area: shape.w * shape.h }
    }
    fn dims(&self, t: f64) -> [(f64, f64); 3] {
        if t >= self.settle {
            return [(self.to.w, 0.0), (self.to.h, 0.0), (self.to.flare, 0.0)];
        }
        let tau = t - self.start;
        let to = [self.to.w, self.to.h, self.to.flare];
        let mut out = [(0.0, 0.0); 3];
        for i in 0..3 {
            out[i] = self.spring.eval(self.from[i].0, self.from[i].1, to[i], tau);
        }
        out
    }
    fn shape(&self, t: f64) -> Shape {
        let d = self.dims(t);
        Shape { w: d[0].0.max(0.0), h: d[1].0.max(0.0), flare: d[2].0.max(0.0) }
    }
    /// The content's progress: the furthest reached (0–1) and the glide value (overshoot included).
    fn progress(&self, t: f64) -> (f64, f64) {
        if t >= self.settle {
            return (1.0, 1.0);
        }
        let tau = t - self.start;
        (self.spring.reached(tau), self.spring.unit(tau))
    }
}

/// How an element looked when a transition began.
#[derive(Clone, Debug)]
struct Snap {
    rect: Rect,
    opacity: f64,
    blur: f64,
    /// It was leaving (clipped by the shape) when it came back: it stays clipped until it settles.
    clip: bool,
    /// Its content key then.
    key: u64,
}

/// An element leaving.
#[derive(Clone, Debug)]
struct Ghost {
    id: Arc<str>,
    orig: Arc<str>,
    content: Arc<Content>,
    key: u64,
    size: (f64, f64),
    /// Its layout rect (unscaled), pivot the top centre.
    base: Rect,
    scale0: f64,
    rule0: f64,
    own: (f64, f64),
    body: bool,
    ear: bool,
    op0: f64,
    blur0: f64,
    since: f64,
    /// Something moves into its place: it clears first (sequential, never two texts over each other).
    yields: bool,
    /// Where it drew when it became a ghost (scaled with the shape).
    drawn: Rect,
    /// Points it travels with the column while it leaves (the speaking row scrolling up: the oldest sentence leaves
    /// with the others' glide, never under the one moving into its place).
    drift: f64,
}

/// An element as evaluated at one time, with what a transition needs to continue it.
#[derive(Clone, Debug)]
struct Inner {
    slot: Slot,
    orig: Arc<str>,
    base: Rect,
    scale: f64,
    own: (f64, f64),
    /// Opacity before the breathing effect.
    raw_opacity: f64,
}

/// The island. See the module docs and `object/mod.rs`.
pub struct Island {
    display: Display,
    measure: Box<dyn Measure>,
    model: Model,
    scene: Scene,
    layout: Layout,
    motion: Motion,
    carried: HashMap<Arc<str>, Snap>,
    ghosts: Vec<Ghost>,
    /// When each element's current content arrived: (content key, time).
    since: HashMap<Arc<str>, (u64, f64)>,
    /// The previous layout's targets (a target that has not moved keeps working during a motion).
    prev_targets: Vec<(Target, Rect)>,
    /// A change waiting for the next motion slot.
    pending: bool,
    last_start: f64,
    seq: u64,
    /// Every motion start, newest last (bounded), for the one-clock check.
    starts: Vec<f64>,
    /// The display changed: lay out again at the next resolve.
    dirty: bool,
    /// The running motion already took one early redirect (at most one per slot: no thrash).
    early_used: bool,
    /// Incoming elements that wait for an outgoing one in their place to clear.
    delayed: std::collections::HashSet<Arc<str>>,
    /// Incoming elements that wait for a glide to leave their place.
    after_glide: std::collections::HashSet<Arc<str>>,
    /// Carried texts that move while a yielding text clears from the place they cross: they hold their place for
    /// `YIELD_OUT` on the clock, then glide (an option label losing its description glided into the fading one).
    held_glide: std::collections::HashSet<Arc<str>>,
    /// The new layout keeps column content of the old one (a sentence added below the ones shown): incoming column
    /// content appears in place as the edge passes it, scaled only to the width it needs.
    joins: bool,
    /// The column scrolls (the speaking row full, its oldest sentence leaving at the top): what arrives comes up
    /// from below the edge with the column, this many points below its place at the motion's start.
    enter: HashMap<Arc<str>, f64>,
    /// Night 1022: the nudge's jiggle starts here (the shape landing), once per nudge motion (its id).
    wobble: Option<(u64, f64)>,
}

impl Island {
    pub fn new(display: Display, measure: Box<dyn Measure>) -> Island {
        let display = display.sanitized();
        let layout = Layout::rest(&display);
        Island {
            display,
            measure,
            model: Model::new(),
            scene: Scene::Rest,
            motion: Motion::at_rest(display.rest(), f64::NEG_INFINITY),
            layout,
            carried: HashMap::new(),
            ghosts: vec![],
            since: HashMap::new(),
            prev_targets: vec![],
            pending: false,
            last_start: f64::NEG_INFINITY,
            seq: 0,
            starts: vec![],
            dirty: false,
            early_used: false,
            delayed: Default::default(),
            after_glide: Default::default(),
            held_glide: Default::default(),
            joins: false,
            enter: HashMap::new(),
            wobble: None,
        }
    }

    /// Wall-clock milliseconds at island time 0 (questions expire on the wall clock).
    pub fn set_wall_origin(&mut self, wall_ms_at_zero: f64) {
        if wall_ms_at_zero.is_finite() {
            self.model.wall0 = wall_ms_at_zero;
        }
    }

    /// The only way state changes.
    pub fn apply(&mut self, ev: Event, t: f64) {
        let t = if t.is_finite() { t } else { self.last_time() };
        if let Event::Display(d) = &ev {
            // The same display again (a Space change, a wake) changes nothing.
            if d.sanitized() != self.display {
                self.display = d.sanitized();
                self.layout_dirty();
                // On a display without a notch a tucked question shows again: nothing there could bring it back.
                if !self.display.has_notch() {
                    self.model.untuck();
                }
            }
        }
        // Without a notch the menu bar's middle is the apps': no peek lip there, whatever hover arrives.
        let ev = match ev {
            Event::Hover(Some(Target::Token(super::Token::Reply))) if !self.display.has_notch() => Event::Hover(None),
            // Nor is a question tucked there: with no notch to bring it back, its card has no lip (bug note
            // 2026-10-28T19-45-55, review).
            Event::Hover(Some(Target::AskTuck(_))) if !self.display.has_notch() => Event::Hover(None),
            Event::Click(Target::AskTuck(_)) if !self.display.has_notch() => Event::Tick,
            ev => ev,
        };
        // Night 1022: a drag stretches only the pulled-down reply on screen (not one hidden under drawing mode).
        let ev = match ev {
            Event::Pull(Some(_)) if self.layout.class != Class::Reply => Event::Tick,
            ev => ev,
        };
        self.model.apply(ev, t);
        self.resolve(t);
        if self.layout.class != Class::Reply {
            self.model.let_go(t);
        }
        // Night 1022: a nudge's motion jiggles the pill as it lands (only while the nudge is the scene: on anything
        // else it stops, a step of at most a few points).
        if matches!(self.scene, Scene::Lip(super::Lip::Nudge { .. })) && self.wobble.is_none_or(|(id, _)| id != self.motion.id) && self.motion.id != 0 {
            self.wobble = Some((self.motion.id, self.motion.fx_at));
        }
        // The pulled-down turn scrolls within its bodies.
        if self.layout.class == super::layout::Class::Reply {
            self.model.clamp_reply_scroll(self.layout.scroll_max, self.layout.scroll_at);
        }
    }

    pub fn set_display(&mut self, d: Display, t: f64) {
        self.apply(Event::Display(d), t);
    }

    fn last_time(&self) -> f64 {
        if self.last_start.is_finite() {
            self.last_start
        } else {
            0.0
        }
    }

    fn layout_dirty(&mut self) {
        // The next resolve lays out again (sizes depend on the display); what shows now stays until then.
        self.dirty = true;
    }

    /// What happened that the app must hear (answers, results, drawing mode's actions, reports).
    pub fn take_outputs(&mut self) -> Vec<Output> {
        std::mem::take(&mut self.model.outputs)
    }

    /// The scene at the last event or deadline.
    pub fn scene(&self) -> &Scene {
        &self.scene
    }

    pub fn class(&self) -> Class {
        self.layout.class
    }

    /// The digit keys (Option plus a digit) the question card on screen answers now (at most four; zero for none).
    pub fn card_keys(&self) -> usize {
        self.model.card_keys()
    }

    /// The request ids of the question cards the island holds.
    pub fn card_ids(&self) -> Vec<String> {
        self.model.card_ids()
    }

    pub fn display(&self) -> Display {
        self.display
    }

    /// The target layout's size.
    pub fn target_size(&self) -> (f64, f64) {
        (self.layout.w, self.layout.h)
    }

    /// The current layout's items (for the renderer's warm-up and tests).
    pub fn items(&self) -> impl Iterator<Item = (&Arc<str>, &Arc<Content>, Rect)> {
        self.layout.items.iter().map(|i| (&i.id, &i.content, i.rect))
    }

    /// Every target of the current layout, with its rect.
    pub fn targets(&self) -> &[(Target, Rect)] {
        &self.layout.targets
    }

    /// Motion starts so far (bounded to the last 64).
    pub fn motion_starts(&self) -> &[f64] {
        &self.starts
    }

    pub fn motion(&self) -> (u64, f64) {
        (self.motion.id, self.motion.start)
    }

    /// The motion `id` (started by the apply just made) starts `dt` seconds later: the apply took that long, and
    /// its first frame can paint only now. Without this, a slow apply (laying out a long conversation) ate the
    /// opening spring, and the first frame showed it already open (audit 27 Oct: 660 ms into a 0.45 s spring).
    /// Everything timed from the motion's start moves with it; nothing else changes.
    pub fn delay_motion(&mut self, id: u64, dt: f64) {
        if self.motion.id != id || id == 0 || !(dt > 0.0) || !dt.is_finite() {
            return;
        }
        let t0 = self.motion.start;
        let m = &mut self.motion;
        m.start += dt;
        m.settle += dt;
        m.visual += dt;
        m.fx_at += dt;
        if self.last_start == t0 {
            self.last_start += dt;
        }
        if let Some(s) = self.starts.last_mut().filter(|s| **s == t0) {
            *s += dt;
        }
        for (_, s) in self.since.values_mut() {
            if *s == t0 {
                *s += dt;
            }
        }
        if let Some((wid, s)) = self.wobble.as_mut() {
            if *wid == id {
                *s += dt;
            }
        }
    }

    /// Whether Heimdall's voice look is up at `t`, and whether the voice says it plays.
    pub fn speaking(&self, t: f64) -> bool {
        self.model.speaking(t)
    }
    pub fn voice_on(&self) -> bool {
        self.model.voice_on()
    }
    pub fn drawing(&self) -> bool {
        self.model.drawing()
    }
    pub fn line_ids(&self) -> Vec<String> {
        self.model.line_ids()
    }
    /// Something needs the user (a problem is held): the menu-bar helmet turns red.
    pub fn alerting(&self) -> bool {
        self.model.cues.alerting()
    }
    /// Whether a last reply exists (pulling the notch down shows it).
    pub fn reply_available(&self) -> bool {
        self.model.reply_available()
    }
    /// The last reply as the pull-down shows it (paragraphs).
    pub fn reply(&self) -> Vec<String> {
        self.model.reply_paragraphs()
    }
    /// The sentences the speaking row holds now.
    pub fn spoken(&self) -> &[String] {
        self.model.spoken()
    }
    pub fn reply_open(&self) -> bool {
        self.model.reply_open()
    }

    /// Whether the open pull-down is the user's (⌃⌘H or a click), not an agent's note's.
    pub fn owners_reply_open(&self) -> bool {
        self.model.owners_reply_open()
    }

    /// Where a drop goes now: into the open pull-down's composer while he writes there, else to the agent
    /// (`drop::route`; `focused`: the composer has the keyboard).
    pub fn drop_route(&self, focused: bool) -> crate::island::drop::Route {
        // Only while the pull-down is what shows: a question, the reader or drawing mode over it hides its composer.
        if self.layout.class != Class::Reply {
            return crate::island::drop::Route::Agent;
        }
        self.model.drop_route(focused)
    }

    /// What was dropped into the composer, waiting for its send.
    pub fn composer_attached(&self) -> &[crate::island::drop::DropItem] {
        self.model.composer_attached()
    }
    /// The last turn's notes by id, oldest first.
    pub fn note_ids(&self) -> Vec<String> {
        self.model.note_ids()
    }

    /// At rest, the notch itself is a target while there is a last reply to pull down or a question tucked away (only
    /// under a real notch: on a display without one the menu bar's middle belongs to the apps).
    fn rest_targets(&self) -> Vec<(Target, Rect)> {
        match self.display.notch {
            Some((nw, nh)) if self.model.notch_opens() => {
                // Inside the notch's rounded bottom corners.
                vec![(Target::Token(super::Token::Reply), Rect::new(-nw / 2.0 + 8.0, 0.0, nw - 16.0, nh - 4.0))]
            }
            _ => vec![],
        }
    }

    /// Re-resolves the scene at `t` and starts (or defers) the transition to it.
    fn resolve(&mut self, t: f64) {
        let scene = self.model.scene(t);
        let mut next = if scene == self.scene && !self.dirty { self.layout.clone() } else { layout(&scene, &self.display, self.measure.as_ref()) };
        if next.is_rest() {
            next.targets = self.rest_targets();
        }
        // On another display mid-motion, the same layout is still a cut (the shape it was growing from belongs to the
        // old display).
        let moving_display = self.dirty && t < self.motion.settle;
        if next == self.layout && !moving_display {
            self.scene = scene;
            self.pending = false;
            self.dirty = false;
            return;
        }
        if !moving_display && self.in_place(&next) {
            self.stamp(&mut next, t);
            self.layout = next;
            self.scene = scene;
            self.pending = false;
            self.dirty = false;
            return;
        }
        // A display change is a cut: the island is on another display now, laid out there at once, no morph.
        if self.dirty {
            self.stamp(&mut next, t);
            self.seq += 1;
            let to = Shape { w: next.w, h: next.h, flare: next.flare };
            self.motion = Motion { id: self.seq, start: t, spring: GROW, from: [(to.w, 0.0), (to.h, 0.0), (to.flare, 0.0)], to, settle: t, visual: t, fx_at: t, from_area: to.w * to.h };
            self.carried.clear();
            self.ghosts.clear();
            self.prev_targets = next.targets.clone();
            self.layout = next;
            self.scene = scene;
            self.pending = false;
            self.dirty = false;
            return;
        }
        if t - self.last_start < MOTION_GAP - 1e-9 {
            // Still growing from a motion that just began: redirect it now (one arrival, one motion). Otherwise the
            // scene is laid out when its slot comes (`self.scene` stays the one on screen).
            let growing = !self.early_used
                && !self.layout.is_rest()
                && !next.is_rest()
                && t - self.motion.start < EARLY_RETARGET
                && self.motion.to.w * self.motion.to.h > self.motion.from_area + 1e-6;
            if !growing {
                self.pending = true;
                return;
            }
            self.stamp(&mut next, t);
            self.transition(next, t, true);
            self.scene = scene;
            self.dirty = false;
            return;
        }
        self.stamp(&mut next, t);
        self.transition(next, t, false);
        self.scene = scene;
        self.dirty = false;
    }

    /// Records when each element's content arrived.
    fn stamp(&mut self, l: &mut Layout, t: f64) {
        let mut since = HashMap::new();
        for it in &l.items {
            let s = match self.since.get(&it.id) {
                Some((key, s)) if *key == it.key => *s,
                _ => t,
            };
            since.insert(it.id.clone(), (it.key, s));
        }
        self.since = since;
    }

    /// Only live elements changed: a cut, no motion. At rest only the notch's target can change.
    fn in_place(&self, next: &Layout) -> bool {
        let a = &self.layout;
        if a.is_rest() && next.is_rest() {
            return true;
        }
        // The pulled-down turn's note X's scroll with its body (and leave or enter the window): scrolling is still a
        // change in place. Every other target is the same, in the same place.
        let fixed = |l: &Layout| -> Vec<(Target, super::Rect)> {
            l.targets.iter().filter(|(t, _)| !(l.class == Class::Reply && matches!(t, Target::NoteClose(_) | Target::NotePin(_) | Target::ChatNote(_)))).cloned().collect()
        };
        let same_targets = fixed(a) == fixed(next);
        if a.w != next.w || a.h != next.h || a.flare != next.flare || a.class != next.class || a.items.len() != next.items.len() || !same_targets {
            return false;
        }
        a.items.iter().zip(&next.items).all(|(x, y)| x.id == y.id && x.ear == y.ear && x.alpha == y.alpha && (x.live || (x.key == y.key && x.rect == y.rect)))
    }

    fn transition(&mut self, next: Layout, t: f64, early: bool) {
        let now = self.eval(t);
        let dims = self.motion.dims(t);
        let to = Shape { w: next.w, h: next.h, flare: next.flare };
        let spring = if next.is_rest() { CALM } else { GROW };
        let from_shape = self.shape_at(t);
        let mut settle = t;
        for (i, target) in [to.w, to.h, to.flare].into_iter().enumerate() {
            settle = settle.max(t + spring.settle_exact(dims[i].0, dims[i].1, target, SETTLE_EPS));
        }
        // The content's progress (the furthest reached) is complete at the spring's first peak; glides settle with
        // the carried elements below.
        settle = settle.max(t + spring.reach_time());
        let mut visual = t + spring.reach_time();
        for (i, target) in [to.w, to.h].into_iter().enumerate() {
            visual = visual.max(t + spring.settle_exact(dims[i].0, dims[i].1, target, 1.0));
        }
        let visual = visual.min(settle);
        let fx_at = t + spring.time_to(FX_PROGRESS);
        self.seq += 1;
        let shrinking = to.w * to.h < from_shape.w * from_shape.h - 1e-6;
        self.motion = Motion { id: self.seq, start: t, spring, from: dims, to, settle, visual, fx_at, from_area: from_shape.w * from_shape.h };
        self.pending = false;
        if early {
            // The same arrival, redirected: it keeps its slot (the rate limit counts arrivals, not redirects).
            self.early_used = true;
        } else {
            self.last_start = t;
            self.early_used = false;
            self.starts.push(t);
            if self.starts.len() > 64 {
                self.starts.remove(0);
            }
        }
        // Snapshot every visible element: the same element continues; anything else leaves with the shape.
        let mut carried = HashMap::new();
        let mut ghosts = vec![];
        for inner in now {
            if inner.slot.opacity <= 0.002 {
                continue;
            }
            // The same element continues: same content, live content, or a symbol that changes in place (the system's
            // replace transition swaps it on the same image view).
            let symbol = |c: &Content| matches!(c, Content::Symbol { .. });
            // A body is live for its scroll only: new words or notes in it (the turn's reply arriving under its
            // table) are new content, which leaves and arrives with the shape rather than gliding at its new size.
            let same_body = |a: &Content, b: &Content| match (a, b) {
                (Content::Body { lines: l1, notes: n1, font: f1, color: c1, line_h: h1, marks: m1, .. }, Content::Body { lines: l2, notes: n2, font: f2, color: c2, line_h: h2, marks: m2, .. }) => {
                    l1 == l2 && n1 == n2 && f1 == f2 && c1 == c2 && h1 == h2 && m1 == m2
                }
                _ => true,
            };
            let same = next.items.iter().find(|it| {
                it.id == inner.orig && same_body(&it.content, &inner.slot.content) && (it.key == inner.slot.key || it.live || (symbol(&it.content) && symbol(&inner.slot.content)))
            });
            if let Some(it) = same {
                if !carried.contains_key(&it.id) {
                    carried.insert(it.id.clone(), Snap { rect: inner.slot.rect, opacity: inner.raw_opacity, blur: inner.slot.blur, clip: inner.slot.clip, key: inner.slot.key });
                    continue;
                }
            }
            let body = inner.own.1 >= 120.0 && shrinking;
            let rule0 = rule(from_shape, inner.own, body && !pull_down(&inner.orig), self.column_top());
            ghosts.push(Ghost {
                id: format!("{}~{}", inner.orig, self.seq).into(),
                orig: inner.orig.clone(),
                content: inner.slot.content.clone(),
                key: inner.slot.key,
                size: inner.slot.size,
                base: inner.base,
                scale0: inner.scale,
                rule0,
                own: inner.own,
                body,
                ear: inner.slot.ear,
                // It leaves from what was on screen.
                op0: inner.slot.opacity,
                blur0: inner.slot.blur,
                since: inner.slot.since,
                yields: false,
                drift: 0.0,
                // Everywhere it can draw while it leaves: from where it is to its size at the target shape (a ghost
                // scales toward the column's top as the shape shrinks).
                drawn: if inner.slot.ear || rule0 <= 1e-6 {
                    inner.slot.rect
                } else {
                    let k = (inner.scale * rule(to, inner.own, body && !pull_down(&inner.orig), self.column_top()) / rule0).clamp(0.0, 1.0);
                    inner.slot.rect.union(&inner.base.scaled_about(Point::new(0.0, self.column_top()), k))
                },
            });
        }
        // Where an incoming element can draw during the motion: its place, scaled toward the column's top while the
        // shape is still smaller than the layout.
        let top = self.column_top();
        let fit0 = if next.w > 0.0 && next.h > top { (from_shape.w / next.w).min((from_shape.h - top).max(0.0) / (next.h - top)).clamp(0.0, 1.0) } else { 1.0 };
        let incoming_zone = |it: &Item| if it.ear { it.rect } else { it.rect.union(&it.rect.scaled_about(Point::new(0.0, top), fit0)) };
        let texty = |c: &Content| match c {
            Content::Text { lines, .. } => !lines.is_empty(),
            Content::Unit { .. } | Content::Split { .. } => true,
            Content::Capsule { label, .. } => !label.is_empty(),
            _ => false,
        };
        // A text gliding out of the place where a new element fades in keeps gliding; what arrives there waits
        // until the glide has left (one content layer per place, never text over text), all of it together.
        let crossing = carried.iter().any(|(id, snap)| {
            let Some(it) = next.items.iter().find(|i| &i.id == id) else { return false };
            if it.ear || it.live || !texty(&it.content) {
                return false;
            }
            let (a, b) = (snap.rect.center(), it.rect.center());
            if ((a.x - b.x).powi(2) + (a.y - b.y).powi(2)).sqrt() < 2.0 {
                return false;
            }
            // Inset by a point, never past half of a degenerate (zero-width) text.
            let path = snap.rect.union(&it.rect);
            let path = path.inset(1.0f64.min(path.w / 4.0).min(path.h / 4.0));
            next.items.iter().any(|o| &o.id != id && !o.ear && !carried.contains_key(&o.id) && intersects(&path, &incoming_zone(o)))
        });
        // The column scrolls: every staying column element glides by the same vertical step (the speaking row's oldest
        // sentence leaving at the top). What leaves moves with them, so nothing slides under it.
        // Only the speaking row's sentence lines scroll; anything else in the column (a status row) means no scroll.
        let sentence = |id: &str| id.starts_with("voice.s");
        let column_other = carried.keys().chain(next.items.iter().filter(|i| !carried.contains_key(&i.id)).map(|i| &i.id)).any(|id| {
            next.items.iter().any(|i| &i.id == id && !i.ear && !sentence(id))
        });
        let steps: Vec<f64> = if column_other {
            vec![]
        } else {
            carried
                .iter()
                .filter_map(|(id, snap)| next.items.iter().find(|i| &i.id == id && !i.ear && !i.live).map(|i| i.rect.mid_y() - snap.rect.mid_y()))
                .collect()
        };
        // Independent of the map's order (determinism): the steps' range decides.
        let (lo, hi) = steps.iter().fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), d| (lo.min(*d), hi.max(*d)));
        let scroll = if !steps.is_empty() && hi - lo < 0.5 && lo.abs().min(hi.abs()) > 1.0 && lo.signum() == hi.signum() { 0.5 * (lo + hi) } else { 0.0 };
        if scroll != 0.0 {
            for gh in ghosts.iter_mut().filter(|g| !g.ear) {
                gh.drift = scroll;
            }
        }
        let mut after_glide = std::collections::HashSet::new();
        if crossing {
            for it in next.items.iter().filter(|i| !i.ear && !carried.contains_key(&i.id)) {
                after_glide.insert(it.id.clone());
            }
        }
        let mut delayed = std::collections::HashSet::new();
        // Text over text never shows: an outgoing element that an incoming or gliding one moves into clears first
        // (out over the first quarter of the motion), and an incoming one in its place waits for it.
        // The column: an outgoing element that an incoming or gliding one moves into clears first, and whatever
        // is new waits for it, all together (never one answer alone on one side).
        let mut column_waits = false;
        let mut held_glide = std::collections::HashSet::new();
        for gh in ghosts.iter_mut().filter(|g| !g.ear) {
            let zone = gh.base.union(&gh.drawn).inset(-2.0);
            for it in next.items.iter().filter(|i| !i.ear) {
                let path = match carried.get(&it.id) {
                    Some(snap) => snap.rect.union(&it.rect),
                    None => incoming_zone(it),
                };
                if intersects(&zone, &path) {
                    gh.yields = true;
                    column_waits |= !carried.contains_key(&it.id);
                    // A carried text that moves into the yielding one holds its place until that clears.
                    if let Some(snap) = carried.get(&it.id) {
                        let moves = (snap.rect.x - it.rect.x).abs() + (snap.rect.y - it.rect.y).abs() > 0.5;
                        // Only the question card's own rows (`q.`, `q.under.`): a speech column's lines glide with it.
                        if moves && it.id.starts_with("q.") && matches!(*it.content, Content::Text { .. }) {
                            held_glide.insert(it.id.clone());
                        }
                    }
                }
            }
        }
        // A row of buttons swaps as a row: when the new column waits, buttons that would stay go out and come back
        // with it (a lone surviving answer would stand off-centre while its row is empty).
        if column_waits {
            let staying: Vec<Arc<str>> = carried
                .keys()
                .filter(|id| next.items.iter().any(|i| &i.id == *id && !i.ear && !i.live && matches!(*i.content, Content::Capsule { .. })))
                .cloned()
                .collect();
            let mut staying = staying;
            staying.sort();
            for id in staying {
                let (Some(snap), Some(it)) = (carried.remove(&id), next.items.iter().find(|i| i.id == id)) else { continue };
                ghosts.push(Ghost {
                    id: format!("{}~{}", id, self.seq).into(),
                    orig: id.clone(),
                    content: it.content.clone(),
                    key: it.key,
                    size: (snap.rect.w, snap.rect.h),
                    base: snap.rect,
                    scale0: 1.0,
                    rule0: 0.0,
                    own: (next.w, next.h),
                    body: false,
                    ear: false,
                    op0: snap.opacity,
                    blur0: snap.blur,
                    since: t,
                    yields: true,
                    drawn: snap.rect,
                    drift: 0.0,
                });
            }
        }
        // The ears: a pair swaps as a pair. When a leaving ear and an arriving one would draw over each other, both
        // sides clear (a staying ear included) and the new pair comes in together.
        let ears_swap = ghosts.iter().filter(|g| g.ear).any(|gh| {
            let zone = gh.base.inset(-1.0);
            next.items.iter().any(|it| it.ear && !carried.contains_key(&it.id) && intersects(&zone, &it.rect))
        });
        if ears_swap {
            let mut staying: Vec<Arc<str>> = carried.keys().filter(|id| next.items.iter().any(|i| &i.id == *id && i.ear)).cloned().collect();
            staying.sort();
            for id in staying {
                let (Some(snap), Some(it)) = (carried.remove(&id), next.items.iter().find(|i| i.id == id)) else { continue };
                ghosts.push(Ghost {
                    id: format!("{}~{}", id, self.seq).into(),
                    orig: id.clone(),
                    content: it.content.clone(),
                    key: it.key,
                    size: (snap.rect.w, snap.rect.h),
                    base: snap.rect,
                    scale0: 1.0,
                    rule0: 0.0,
                    own: (next.w, next.h),
                    body: false,
                    ear: true,
                    op0: snap.opacity,
                    blur0: snap.blur,
                    since: t,
                    yields: true,
                    drawn: snap.rect,
                    drift: 0.0,
                });
            }
            for gh in ghosts.iter_mut().filter(|g| g.ear) {
                gh.yields = true;
            }
        }
        for it in &next.items {
            if carried.contains_key(&it.id) {
                continue;
            }
            if (it.ear && ears_swap) || (!it.ear && column_waits) {
                delayed.insert(it.id.clone());
            }
        }
        // The motion settles once every glide is within `SETTLE_EPS` of its place and every clock-timed fade is done.
        let glide = carried
            .iter()
            .filter_map(|(id, snap)| next.items.iter().find(|i| &i.id == id).map(|i| (snap.rect.center(), i.rect.center(), snap.rect.w - i.rect.w, snap.rect.h - i.rect.h)))
            .map(|(a, b, dw, dh)| (a.x - b.x).abs().max((a.y - b.y).abs()).max(dw.abs()).max(dh.abs()))
            .fold(0.0, f64::max);
        if glide > SETTLE_EPS {
            self.motion.settle = self.motion.settle.max(t + spring.settle_exact(0.0, 0.0, 1.0, SETTLE_EPS / glide));
        }
        if !delayed.is_empty() || !after_glide.is_empty() {
            let wait = if after_glide.is_empty() { WAIT_IN } else { GLIDE_WAIT };
            self.motion.settle = self.motion.settle.max(t + wait + FADE_IN);
        }
        if !held_glide.is_empty() {
            self.motion.settle = self.motion.settle.max(t + HOLD_TO);
        }
        self.motion.visual = self.motion.visual.min(self.motion.settle);
        self.joins = next.items.iter().any(|i| !i.ear && carried.contains_key(&i.id));
        // A scrolling column: what arrives rides in from below with it (clipped by the edge), nothing waits.
        let mut enter = HashMap::new();
        if scroll != 0.0 {
            for it in next.items.iter().filter(|i| !i.ear && !carried.contains_key(&i.id)) {
                enter.insert(it.id.clone(), -scroll);
                delayed.remove(&it.id);
                after_glide.remove(&it.id);
            }
        }
        self.enter = enter;
        self.delayed = delayed;
        self.after_glide = after_glide;
        self.held_glide = held_glide;
        self.prev_targets = std::mem::replace(&mut self.layout.targets, vec![]);
        self.carried = carried;
        self.ghosts = ghosts;
        self.layout = next;
    }

    /// Every element at `t`, with transition state.
    fn eval(&self, t: f64) -> Vec<Inner> {
        let m = &self.motion;
        let shape = self.shape_at(t);
        let (p, g) = m.progress(t);
        let settled = t >= m.settle;
        let l = &self.layout;
        let shrinking = l.w * l.h < m.from_area - 1e-6;
        // Column content scales about the column's top centre: the notch's bottom edge, so nothing is pulled up
        // under the camera housing while it grows or shrinks.
        let top = self.column_top();
        let pivot = Point::new(0.0, top);
        let mut out = Vec::with_capacity(l.items.len() + self.ghosts.len());
        if !settled {
            let k_short = smooth(0.1, 0.6, p);
            let k_body = smooth(0.5, 1.0, p);
            // A yielding text clears in `YIELD_OUT` on the clock (not the spring's progress, which can cover a
            // quarter in two frames), overlapping its successor's start only below a fifth of their opacity.
            let k_yield = smooth(0.0, YIELD_OUT, t - m.start);
            let k_capsule_out = smooth(0.0, 0.08, t - m.start);
            for gh in &self.ghosts {
                // A white capsule fading slowly over black reads grey: it clears on the clock like a yielding text.
                let capsule = matches!(*gh.content, Content::Capsule { .. });
                // Ears clear on the clock with the yielding texts, all of them together (a pair never leaves half
                // there): the shrinking shape would cut their numerals and clock at the notch's sides mid-fade.
                let k = if capsule { k_capsule_out } else if gh.yields || gh.ear { k_yield } else if gh.body { k_body } else { k_short };
                let op = gh.op0 * (1.0 - k);
                // Below 1 % it is gone (a lingering invisible ghost would still count as drawn).
                if op <= 0.01 {
                    continue;
                }
                let scale = if gh.ear {
                    1.0
                } else {
                    // A pull-down's body shrinks with the shape (never sliced flat by its edge at full strength,
                    // review of 063424a); other bodies keep the square root of the shrink.
                    let r = rule(shape, gh.own, gh.body && !pull_down(&gh.orig), top);
                    let s = if gh.rule0 > 1e-6 { (gh.scale0 * r / gh.rule0).clamp(0.0, 1.0) } else { gh.scale0 };
                    // A wide body (a table) leaving never reaches the shrinking shape's rounded corners: its text
                    // would be cut by the curve (the jobs table's bottom row at 1.49 s).
                    if gh.body && pull_down(&gh.orig) && gh.base.w > 0.0 { s.min(((shape.w - 2.0 * super::layout::PAD_MAX) / gh.base.w).max(0.0)) } else { s }
                };
                let target_blur = if shrinking { 4.0 } else { 6.0 };
                let blur = gh.blur0 + (target_blur - gh.blur0) * k;
                let rect = if gh.ear { gh.base } else { gh.base.scaled_about(pivot, scale) };
                // A yielding text drifts up as it blurs out (the system's replace motion, done by the island); in a
                // scrolling column it travels with the column instead.
                let rect = if gh.drift != 0.0 {
                    rect.offset(0.0, gh.drift * g)
                } else if gh.yields {
                    rect.offset(0.0, -rise_room(gh.base, &self.ghosts, &l.items) * k)
                } else {
                    rect
                };
                // A pull-down's body leaving fades ahead of the shrinking edge: what the edge reaches has gone, never a
                // table row sliced flat at full strength (review of 063424a).
                out.push(Inner {
                    slot: Slot {
                        id: gh.id.clone(),
                        content: gh.content.clone(),
                        key: gh.key,
                        rect,
                        size: gh.size,
                        opacity: op,
                        blur,
                        fx: Fx::None,
                        arrived: false,
                        phase: 0.0,
                        ear: gh.ear,
                        // Leaving, everything is clipped by the shrinking shape (swallowed by the notch).
                        clip: true,
                        since: gh.since,
                    },
                    orig: gh.orig.clone(),
                    base: gh.base,
                    scale,
                    own: gh.own,
                    raw_opacity: op,
                });
            }
        }
        let k_in = smooth(0.1, 0.7, p);
        // White capsules come in quickly (a capsule fading slowly over black reads grey).
        let k_capsule = smooth(0.06, 0.15, t - m.start) * smooth(0.1, 0.35, p);
        let fit = if l.w > 0.0 && l.h > top { (shape.w / l.w).min((shape.h - top).max(0.0) / (l.h - top)).clamp(0.0, 1.0) } else { 1.0 };
        for it in &l.items {
            let since = self.since.get(&it.id).map_or(m.start, |(_, s)| *s);
            let carried_clip = !settled && self.carried.get(&it.id).is_some_and(|s| s.clip);
            let (rect, mut opacity, blur, scale, edge_clip) = if settled {
                (it.rect, it.alpha, 0.0, 1.0, false)
            } else if let Some(s) = self.carried.get(&it.id) {
                // Live content (reader units, numerals, the waveform, the grabber) keeps its own size: its centre
                // glides with the shape, it never stretches between two contents' rects.
                // (The same live content caught mid-arrival, still scaled small, grows on with the shape.)
                let growing = s.key == it.key && ((s.rect.w - it.rect.w).abs() >= 1.0 || (s.rect.h - it.rect.h).abs() >= 1.0);
                let rect = if it.live && it.free && (it.ear || (s.key != it.key && !matches!(*it.content, Content::Capsule { .. }))) {
                    // Live parts placed against each other (a bug note's dot beside its time in an ear, a reader unit
                    // on its focus letter) cut together: one gliding while the other cuts would split the group
                    // off-centre for the length of the motion.
                    it.rect
                } else if it.live && !growing {
                    let c = s.rect.center();
                    let (x, y) = (c.x + (it.rect.mid_x() - c.x) * g, c.y + (it.rect.mid_y() - c.y) * g);
                    Rect::centered(x, y, it.rect.w, it.rect.h)
                } else {
                    // A held text keeps its place until the yielding text it would cross has cleared.
                    let hold = if self.held_glide.contains(&it.id) { smooth(HOLD_FROM, HOLD_TO, t - m.start) } else { 1.0 };
                    s.rect.lerp(&it.rect, g * hold)
                };
                let op = s.opacity + (it.alpha - s.opacity) * k_in;
                (rect, op, s.blur * (1.0 - k_in), 1.0, false)
            } else {
                // Joining content that stays (a sentence added below the ones shown) appears in place as the edge
                // passes it, scaled only if the shape is not yet wide enough for it; anything else grows with the
                // shape from the column's top.
                let fit_w = if l.w > 0.0 { (shape.w / l.w).clamp(0.0, 1.0) } else { 1.0 };
                let entering = self.enter.get(&it.id).copied();
                let scale = if it.ear || entering.is_some() { 1.0 } else if self.joins { fit_w } else { fit };
                let mut edge_clip = entering.is_some();
                let rect = if let Some(off) = entering {
                    it.rect.offset(0.0, off * (1.0 - g))
                } else if it.ear {
                    it.rect
                } else if self.joins {
                    it.rect.scaled_about(Point::new(0.0, it.rect.y), scale)
                } else {
                    it.rect.scaled_about(pivot, scale)
                };
                let mut gate = 1.0;
                if it.ear {
                    // Both ears of a pair (all their parts) fade in together once the shape is wide enough to hold
                    // them whole.
                    // The fade starts a little before that (clipped by the shape meanwhile, at low opacity), so it is
                    // never a pop at 60 Hz.
                    let need = (2.0 * (l.ear_reach + 1.5) + 4.0).min(l.w - 1.0);
                    if l.w > shape.w {
                        gate = smooth(need - 12.0, l.w, shape.w);
                    }
                    // Without a notch the shape also grows down from the top edge: the same for its height.
                    if l.h > shape.h {
                        gate *= smooth(it.rect.max_y() - 8.0, l.h, shape.h);
                    }
                } else if entering.is_some() {
                    // Riding in with a scrolling column: the shape's outline clips it, so it needs no edge gate.
                    gate = 1.0;
                } else {
                    // Never a half-revealed line: nothing shows until the bottom edge has passed it (fully at the
                    // target height, however close to the bottom the element sits).
                    // Measured against the room the element has below it at the target, the gate rises in step with
                    // the shape's own growth (a smooth fade, never a pop).
                    let room = (l.h - it.rect.max_y()).max(1.0);
                    gate = clamp01((shape.h - rect.max_y()) / room);
                    // A question card's elements are clipped by the outline and fade in with the motion instead: a foot
                    // row has only a few points of room below it and would otherwise appear within one frame.
                    if it.gate_top {
                        gate = 1.0;
                    }
                    // Revealed by the edge, it is clipped by the outline until it is fully in: a shape growing
                    // sideways from the notch's width at the same time can still have its bottom corners round
                    // past a wide line's ends (at low opacity).
                    edge_clip = true;
                    // A white capsule is revealed by the growing edge (clipped by the outline) and fades in over the
                    // first two thirds of that: it never sits half-faded (grey) and never pops in within a frame.
                    if matches!(*it.content, Content::Capsule { .. }) {
                        let room = (l.h - it.rect.y).max(1.0);
                        gate = clamp01((shape.h - rect.y) / room * 1.5);
                        edge_clip = true;
                    }
                }
                // A white capsule comes in fast (fading slowly over black it reads grey), after whatever it waits for.
                let capsule = matches!(*it.content, Content::Capsule { .. });
                let wait = if self.after_glide.contains(&it.id) {
                    Some(GLIDE_WAIT)
                } else if self.delayed.contains(&it.id) {
                    Some(WAIT_IN)
                } else {
                    None
                };
                let k = match (wait, capsule) {
                    (Some(w), true) => smooth(w, w + 0.12, t - m.start),
                    (Some(w), false) => smooth(w, w + FADE_IN, t - m.start),
                    (None, true) => k_capsule,
                    (None, false) => k_in,
                };
                (rect, it.alpha * k * gate, 8.0 * (1.0 - k), scale, edge_clip && (gate < 1.0 || entering.is_some()))
            };
            let raw = opacity;
            let drawn_base = !settled && (self.carried.contains_key(&it.id) || (self.joins && !it.ear && !self.enter.contains_key(&it.id)));
            let held = settled || !it.ear || (shape.w >= 2.0 * (l.ear_reach + 1.5) + 4.0 && shape.h >= it.rect.max_y() + 4.0);
            let arrived = opacity >= 0.9 * it.alpha.max(0.01) && t >= m.fx_at;
            let _ = &mut opacity;
            let phase = if it.content.animated() { (((t - since) / 0.9).fract() + 1.0).fract() } else { 0.0 };
            out.push(Inner {
                slot: Slot {
                    id: it.id.clone(),
                    content: it.content.clone(),
                    key: it.key,
                    rect,
                    size: (it.rect.w, it.rect.h),
                    opacity: opacity.clamp(0.0, 1.0),
                    blur,
                    fx: it.fx,
                    arrived,
                    phase: if phase.is_finite() { phase } else { 0.0 },
                    ear: it.ear,
                    clip: it.clip || !held || carried_clip || edge_clip,
                    since,
                },
                orig: it.id.clone(),
                // Mid-glide, or joining in place (scaled about its own top), it is where it draws: a ghost made of it
                // now leaves from there at scale 1, with the rest of the column (scaling its target rect about the
                // column's top instead would pull it up over the element above).
                base: if drawn_base { rect } else { it.rect },
                scale: if drawn_base { 1.0 } else { scale },
                own: (l.w, l.h),
                raw_opacity: raw.clamp(0.0, 1.0),
            });
        }
        out
    }

    /// Everything at the notch at `t`. Pure.
    pub fn frame(&self, t: f64) -> FrameModel {
        let m = &self.motion;
        let moving = self.moving(t);
        let shape = self.shape_at(t);
        let light = self.model.light_now(t);
        let pointing = self.model.pointing_now(t);
        let glow = (light.run * light.opacity).clamp(0.0, 1.0);
        let target = (self.layout.w, self.layout.h);
        // Without a notch a retracting band under a point tall is gone (no hairline at the top edge).
        let gone = !self.display.has_notch() && self.layout.is_rest() && shape.h < 1.0;
        let rest = self.layout.is_rest() && (t >= m.settle || gone) && !self.extra_moving(t);
        if rest {
            return FrameModel { t, visible: false, shape: self.display.rest(), glow, slots: vec![], light, pointing, motion: (m.id, m.start), moving, display: self.display, target, jiggle: false };
        }
        let mut slots: Vec<Slot> = self.eval(t).into_iter().map(|i| i.slot).filter(|s| s.opacity > 0.002).collect();
        // Night 1022: a dragged pull-down's band (grabber, pin) rides its stretching bottom edge.
        let stretch = self.model.stretch(t);
        if stretch > 0.0 {
            for s in slots.iter_mut().filter(|s| matches!(s.id.split('~').next(), Some("reply.grabber" | "reply.pin"))) {
                // Never past the shape's bottom (it may be retracting under a stretch that is still springing back).
                s.rect.y += stretch.min((shape.h - 4.0 - (s.rect.y + s.rect.h)).max(0.0));
            }
        }
        let jiggle = self.jiggling(t);
        FrameModel { t, visible: true, shape, glow, slots, light, pointing, motion: (m.id, m.start), moving, display: self.display, target, jiggle }
    }

    fn moving(&self, t: f64) -> bool {
        if t < self.motion.settle || self.extra_moving(t) {
            return true;
        }
        // Symbol effects run in the render server: they need no frames.
        self.model.light_moving(t) || self.model.pointing_moving(t)
    }

    /// Whether the caller must keep drawing at `t`.
    pub fn needs_frames(&self, t: f64) -> FrameNeed {
        if self.moving(t) {
            return FrameNeed::Moving;
        }
        if self.layout.loop_fps > 0.0 && !self.layout.is_rest() {
            return FrameNeed::Loop { epoch: self.motion.start, period: 0.9, fps: self.layout.loop_fps };
        }
        if let Some(epoch) = self.model.light_breathing(t) {
            return FrameNeed::Loop { epoch, period: super::scene::BREATH_PERIOD, fps: 20.0 };
        }
        FrameNeed::None
    }

    /// The clock a live window runs at `t`: `needs_frames`, except that a loop of animated content alone (the
    /// thinking dots, the spinner: `Content::animated`) needs no ticks. The painter gives those a Core Animation loop
    /// phase-locked to the slot's `since`, played by the render server, so while only they move the app commits
    /// nothing (GPU trace 28 Oct 20:05: Heimdall redrew its whole island tree at 30 fps while the agent worked).
    /// Offscreen stills keep drawing each phase from `frame(t)`.
    pub fn needs_ticks(&self, t: f64) -> FrameNeed {
        match self.needs_frames(t) {
            FrameNeed::Loop { .. } => match self.model.light_breathing(t) {
                Some(epoch) => FrameNeed::Loop { epoch, period: super::scene::BREATH_PERIOD, fps: 20.0 },
                None => FrameNeed::None,
            },
            other => other,
        }
    }

    /// The next one-shot change (an expiry, a grace end, a reader unit, a deferred motion).
    pub fn deadline(&self) -> Option<f64> {
        let mut d = self.model.deadline();
        if self.pending {
            let slot = self.last_start + MOTION_GAP;
            d = Some(d.map_or(slot, |x| x.min(slot)));
        }
        d
    }

    /// What a click at `p` lands on. Only inside the shape; until the motion has visibly arrived (within a point of
    /// its target) only a target that has not moved (Send stays tappable while the ears change).
    pub fn hit(&self, t: f64, p: Point) -> Option<Target> {
        if self.layout.is_rest() {
            // The notch's own target (the last reply, pulled down).
            return self.layout.targets.iter().find(|(_, r)| r.contains(p)).map(|(target, _)| target.clone());
        }
        let shape = self.shape_at(t);
        if !shape.contains(p, 0.5) {
            return None;
        }
        let moving = t < self.motion.visual;
        self.layout
            .targets
            .iter()
            .find(|(target, r)| {
                // The notch's own target (at rest and the peek lip) takes the pointer while the lip grows, so moving
                // onto the lip never reads as leaving it.
                // The voice's words resize with each sentence: putting them away takes the click mid-motion too (review 1 Nov).
                // Stop keeps its place in the ear while the words grow: it takes the click mid-motion too.
                let notch = matches!(target, Target::Token(super::Token::Reply) | Target::Hush | Target::StopSpeech);
                r.contains(p) && (!moving || notch || self.prev_targets.iter().any(|(pt, pr)| pt == target && (pr.x - r.x).abs() < 0.5 && (pr.y - r.y).abs() < 0.5 && (pr.w - r.w).abs() < 0.5))
            })
            .map(|(target, _)| target.clone())
    }

    /// The words a Copy at `p` takes (bug note 2026-10-30T22-16-36): the message of the pulled-down conversation
    /// under it, or the turn's reply. `None` off the shape, mid-motion, or over no words.
    pub fn copy_at(&self, t: f64, p: Point) -> Option<String> {
        if self.layout.is_rest() || t < self.motion.visual || !self.shape_at(t).contains(p, 0.5) {
            return None;
        }
        self.layout.copies.iter().find(|(r, _)| r.contains(p)).map(|(_, text)| text.clone())
    }

    /// What ⌘C copies with nothing selected in a field: the pulled-down turn's reply, else the newest message showing.
    pub fn copy_shown(&self) -> Option<String> {
        if self.layout.class != Class::Reply {
            return None;
        }
        let reply = self.model.turn_paragraphs().join("\n").trim().to_string();
        if !reply.is_empty() {
            return Some(reply);
        }
        self.layout.copies.iter().max_by(|a, b| a.0.y.total_cmp(&b.0.y)).map(|(_, text)| text.clone())
    }

    /// Whether `p` is inside the island's shape at `t` (never at rest): the pointer over it (`Event::Pointer`).
    pub fn covers(&self, t: f64, p: Point) -> bool {
        !self.layout.is_rest() && self.shape_at(t).contains(p, 0.5)
    }

    /// The shape's motion from its start, sampled every `TRACK_STEP` until it settles. Never empty.
    pub fn track(&self, t: f64) -> Track {
        let m = &self.motion;
        let begin = if m.start.is_finite() { m.start } else { t };
        let end = if m.settle.is_finite() { m.settle.max(begin) } else { begin };
        let n = (((end - begin) / TRACK_STEP).ceil() as usize).min(480);
        let first = self.shape_at(begin);
        let rest = (1..=n).map(|i| self.shape_at(begin + i as f64 * TRACK_STEP)).collect();
        Track { begin, step: TRACK_STEP, first, rest }
    }

    /// The shape at `t`. At the notch's width or wider it is never shorter than the notch: a spring settling into
    /// the ear pair would otherwise dip a point or two and leave the notch hanging below it.
    fn shape_at(&self, t: f64) -> Shape {
        let mut s = self.motion.shape(t);
        // Night 1022: the nudge's jiggle (a swell about the top centre) and the pulled-down reply's stretch.
        let k = self.wobble_at(t);
        if k > 0.0 {
            s.w *= 1.0 + k;
            s.h *= 1.0 + 0.5 * k;
        }
        s.h += self.model.stretch(t);
        if let Some((nw, nh)) = self.display.notch {
            if s.w >= nw - 0.01 && s.h < nh {
                s.h = nh;
            }
        }
        s
    }

    /// Night 1022: the nudge's jiggle at `t`, a fraction of the shape's size: three swells, each smaller
    /// (A·sin²·decay, smooth at both ends, never smaller than the shape it rides on), zero outside `NUDGE_WOBBLE`.
    pub fn wobble_at(&self, t: f64) -> f64 {
        const AMP: f64 = 0.08;
        const DECAY: f64 = 0.17;
        let Some((_, start)) = self.wobble else { return 0.0 };
        if !matches!(self.scene, Scene::Lip(super::Lip::Nudge { .. })) {
            return 0.0;
        }
        let tau = t - start;
        if !(0.0..super::NUDGE_WOBBLE).contains(&tau) {
            return 0.0;
        }
        let x = (std::f64::consts::PI * 3.0 * tau / super::NUDGE_WOBBLE).sin();
        AMP * x * x * (-tau / DECAY).exp()
    }

    /// Night 1022: the nudge's jiggle window is open at `t` (including the instants between its swells).
    fn jiggling(&self, t: f64) -> bool {
        matches!(self.scene, Scene::Lip(super::Lip::Nudge { .. })) && self.wobble.is_some_and(|(_, s)| t >= s - 1e-9 && t < s + super::NUDGE_WOBBLE)
    }

    /// Night 1022: motion outside the shape's spring (the jiggle, the stretch).
    fn extra_moving(&self, t: f64) -> bool {
        let nudge = matches!(self.scene, Scene::Lip(super::Lip::Nudge { .. }));
        (nudge && self.wobble.is_some_and(|(_, s)| t >= s - 1e-9 && t < s + super::NUDGE_WOBBLE)) || self.model.stretch_moving(t)
    }

    /// Night 1022: how far the pulled-down reply is stretched by a drag at `t` (tests).
    pub fn stretch(&self, t: f64) -> f64 {
        self.model.stretch(t)
    }
    /// Night 1022: a nudge waiting or showing (tests).
    pub fn nudge_pending(&self) -> bool {
        self.model.nudge_pending()
    }

    /// Where column content starts: the notch's bottom edge (the band's on a display without a notch).
    fn column_top(&self) -> f64 {
        self.display.notch_size().1
    }

    /// The items of the target layout (for invariants).
    pub(crate) fn layout_items(&self) -> &[Item] {
        &self.layout.items
    }
    pub(crate) fn layout(&self) -> &Layout {
        &self.layout
    }
}

/// How much an outgoing element shrinks with the shape: fit to the shape below the notch (a square root for a
/// retracting body).
/// The pulled-down turn's bodies (its reply and its notes).
fn pull_down(id: &str) -> bool {
    matches!(id, "reply.body" | "reply.notes")
}

/// The most a yielding text drifts up as it blurs out (the island's replace motion).
const YIELD_DRIFT: f64 = 6.0;

/// How far a yielding text may drift up: at most `YIELD_DRIFT`, and never into the nearest text above it that overlaps
/// it sideways (a leaving or an incoming one). A speaking column's line that yields under a line that stays would
/// otherwise drift into that line while it is still at full opacity (question_while_speaking, 24 Oct).
fn rise_room(base: Rect, ghosts: &[Ghost], items: &[super::layout::Item]) -> f64 {
    let sideways = |r: &Rect| r.max_y() <= base.y + 0.5 && r.x < base.max_x() && base.x < r.max_x();
    let above = ghosts
        .iter()
        .filter(|o| !o.ear && sideways(&o.base))
        .map(|o| o.base.max_y())
        .chain(items.iter().filter(|i| !i.ear && sideways(&i.rect)).map(|i| i.rect.max_y()))
        .fold(f64::NEG_INFINITY, f64::max);
    if above.is_finite() { (base.y - above).clamp(0.0, YIELD_DRIFT) } else { YIELD_DRIFT }
}

fn rule(shape: Shape, own: (f64, f64), body: bool, top: f64) -> f64 {
    if own.0 <= 0.0 || own.1 <= top {
        return 1.0;
    }
    let fit = (shape.w / own.0).min((shape.h - top).max(0.0) / (own.1 - top)).clamp(0.0, 1.0);
    if body {
        fit.sqrt()
    } else {
        fit
    }
}

fn intersects(a: &Rect, b: &Rect) -> bool {
    a.x < b.max_x() && b.x < a.max_x() && a.y < b.max_y() && b.y < a.max_y()
}
