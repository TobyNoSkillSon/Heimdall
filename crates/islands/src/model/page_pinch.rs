//! A page in the viewer may take the trackpad pinch itself (on his CV in the applications
//! review page: "when I am hovering over the CV, I should be able to use my fingers on the touchpad and zoom in and
//! zoom out"; 17:03: "it should be the same as the browser if its the same its perfect").
//!
//! The convention (docs/viewer.md, "Let a page take the pinch"): the page says `pinch=page` in its
//! `<meta name="heimdall-viewer" content="…">` and defines `window.heimdallPinch(e)`. For each trackpad magnify event
//! the viewer calls it with `{phase, scale, x, y}`: `phase` is `"begin"`, `"change"`, `"end"` or `"double-tap"` (the
//! two-finger double-tap); `scale` is this event's factor, `1 + magnification` (multiply the zoom by it: the factor
//! Chromium hands a page as `exp(-deltaY / 100)` of its ctrl+wheel); `x`, `y` are the pointer in CSS px of the
//! page's viewport (`clientX`, `clientY`). What the hook returns for `"begin"` routes the whole gesture: `true` and
//! the page has the gesture (nothing else magnifies); anything else, a throw or no hook, and the viewer magnifies the
//! whole page as it does for every other page. A double-tap the page does not take gets the viewer's own smart zoom.
//! A page that says nothing is never asked.
//!
//! The JavaScript call is asynchronous: until the page answers the begin, the gesture's events are held (`Router`),
//! then flushed to the side that has it (to the page as one change at the last point; to the viewer as the events
//! themselves, in order), so nothing is lost or doubled and nothing jumps.

use std::path::Path;

use super::viewer_size::{page_head_of, viewer_meta};

/// Whether `html` takes its own pinch: `pinch=page` in its `<meta name="heimdall-viewer">` (case-blind).
pub fn page_pinch(html: &str) -> bool {
    viewer_meta(html).is_some_and(|pairs| pairs.iter().any(|(k, v)| k == "pinch" && v.eq_ignore_ascii_case("page")))
}

/// Whether `file` takes its own pinch (`page_pinch` on its first 64 KB).
pub fn page_pinch_of(file: &Path) -> bool {
    page_head_of(file).is_some_and(|h| page_pinch(&h))
}

/// The smallest factor one event hands the page (a very fast pinch out can report a magnification under −1).
pub const MIN_FACTOR: f64 = 0.05;

/// The most events held for a page's answer (about a second of a pinch) before the gesture goes to WebKit.
pub const MAX_HELD: usize = 120;

/// One event's factor: `1 + magnification`, never 0 or below.
pub fn factor(magnification: f64) -> f64 {
    if magnification.is_finite() {
        (1.0 + magnification).max(MIN_FACTOR)
    } else {
        1.0
    }
}

/// A magnify event's phase (AppKit's `NSEvent.phase`; an event with no phase counts as a change).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Begin,
    Change,
    End,
}

impl Phase {
    /// From `NSEventPhase` bits (began 1, stationary 2, changed 4, ended 8, cancelled 16, may begin 32).
    pub fn from_bits(bits: usize) -> Phase {
        if bits & 1 != 0 {
            Phase::Begin
        } else if bits & (8 | 16) != 0 {
            Phase::End
        } else {
            Phase::Change
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Phase::Begin => "begin",
            Phase::Change => "change",
            Phase::End => "end",
        }
    }
}

/// One call of the page's hook.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Call {
    pub phase: &'static str,
    pub scale: f64,
    pub x: f64,
    pub y: f64,
}

impl Call {
    /// The JavaScript that calls the hook: true only when it is there, does not throw and returns exactly `true`.
    /// `x`, `y` here are the pointer over the page zoom (`Router::event`); the script maps them through the visual
    /// viewport, so a page the viewer has magnified whole still gets `clientX`, `clientY`.
    pub fn script(&self) -> String {
        let num = |v: f64| if v.is_finite() { format!("{v}") } else { "0".to_owned() };
        format!(
            "(()=>{{try{{const f=window.heimdallPinch;if(typeof f!=='function')return false;const v=window.visualViewport,s=v&&v.scale>0?v.scale:1;\
             return f({{phase:'{}',scale:{},x:(v?v.offsetLeft:0)+{}/s,y:(v?v.offsetTop:0)+{}/s}})===true}}catch(e){{return false}}}})()",
            self.phase,
            num(self.scale),
            num(self.x),
            num(self.y)
        )
    }
}

/// What the viewer does with one event (or with the events it held, once the page has answered).
#[derive(Clone, Debug, PartialEq)]
pub enum Step {
    /// To the web view's own magnify (`super`): the page did not take the gesture, or does not take pinches at all.
    View,
    /// Ask the page with this begin; its answer goes to `Router::answer` with this gesture's number.
    Ask(Call, u64),
    /// Hold the event until the page answers.
    Hold,
    /// To the page's hook (its answer does not matter now).
    Page(Call),
    /// Replay the held events to the web view's own magnify, in order.
    ReplayHeld,
    /// Drop the held events (the page has them, folded into the `Page` step that comes with this).
    DropHeld,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Route {
    Idle,
    /// Waiting for the page's answer to gesture `n`; the held events' product and last point; whether the end is held.
    Asking { n: u64, product: f64, x: f64, y: f64, ended: bool, held: usize },
    Page,
    View,
}

/// Routes one viewer's magnify gestures (`Step`s); one per page.
#[derive(Clone, Debug)]
pub struct Router {
    takes: bool,
    route: Route,
    serial: u64,
}

impl Router {
    /// `takes`: the page says `pinch=page` (`page_pinch`). A router for a page that does not sends everything on.
    pub fn new(takes: bool) -> Router {
        Router { takes, route: Route::Idle, serial: 0 }
    }

    /// A router for a rebuilt page that goes on numbering gestures where this one stopped, so an answer the old page
    /// gives late can never be taken for the new page's.
    pub fn renewed(&self, takes: bool) -> Router {
        Router { takes, route: Route::Idle, serial: self.serial }
    }

    pub fn takes(&self) -> bool {
        self.takes
    }

    /// Whether the page has the gesture in progress (lab checks).
    pub fn page_has_it(&self) -> bool {
        matches!(self.route, Route::Page)
    }

    /// A magnify event: its phase, magnification and pointer (web view points from its top-left, over the page zoom;
    /// `Call::script` maps them to the page's viewport).
    pub fn event(&mut self, phase: Phase, magnification: f64, x: f64, y: f64) -> Vec<Step> {
        if !self.takes {
            return vec![Step::View];
        }
        let f = factor(magnification);
        // A change or end with no begin before it (a gesture begun before the page loaded) starts one.
        let phase = if self.route == Route::Idle && phase != Phase::Begin { Phase::Begin } else { phase };
        match (phase, self.route) {
            (Phase::Begin, route) => {
                // A gesture begun before the page answered the last one: that one's held rest is dropped (the page
                // has its begin at most; replaying it whole too could zoom twice), and its late answer is ignored.
                let mut steps = if matches!(route, Route::Asking { .. }) { vec![Step::DropHeld] } else { vec![] };
                self.serial += 1;
                let n = self.serial;
                self.route = Route::Asking { n, product: 1.0, x, y, ended: false, held: 0 };
                steps.push(Step::Ask(Call { phase: "begin", scale: f, x, y }, n));
                steps
            }
            (_, Route::Asking { held, .. }) if held >= MAX_HELD => {
                // The page has not answered in about a second (busy or hung): the gesture is WebKit's.
                self.route = if phase == Phase::End { Route::Idle } else { Route::View };
                vec![Step::ReplayHeld, Step::View]
            }
            (Phase::Change, Route::Asking { n, product, ended, held, .. }) => {
                self.route = Route::Asking { n, product: product * f, x, y, ended, held: held + 1 };
                vec![Step::Hold]
            }
            (Phase::End, Route::Asking { n, product, held, .. }) => {
                self.route = Route::Asking { n, product: product * f, x, y, ended: true, held: held + 1 };
                vec![Step::Hold]
            }
            (Phase::Change, Route::Page) => vec![Step::Page(Call { phase: "change", scale: f, x, y })],
            (Phase::End, Route::Page) => {
                // The end's own factor (usually none) as a last change, as when the end was held: then a plain end.
                self.route = Route::Idle;
                let mut steps = vec![];
                if (f - 1.0).abs() > 1e-12 {
                    steps.push(Step::Page(Call { phase: "change", scale: f, x, y }));
                }
                steps.push(Step::Page(Call { phase: "end", scale: 1.0, x, y }));
                steps
            }
            (Phase::Change, Route::View) => vec![Step::View],
            (Phase::End, Route::View) => {
                self.route = Route::Idle;
                vec![Step::View]
            }
            (_, Route::Idle) => unreachable!("an idle router starts a gesture"),
        }
    }

    /// The page's answer to the begin of gesture `n` (`true`: it took it). An answer to an older gesture is ignored.
    pub fn answer(&mut self, n: u64, taken: bool) -> Vec<Step> {
        let Route::Asking { n: asked, product, x, y, ended, .. } = self.route else { return vec![] };
        if asked != n {
            return vec![];
        }
        if taken {
            self.route = if ended { Route::Idle } else { Route::Page };
            let mut steps = vec![Step::DropHeld];
            if (product - 1.0).abs() > 1e-12 {
                steps.push(Step::Page(Call { phase: "change", scale: product, x, y }));
            }
            if ended {
                steps.push(Step::Page(Call { phase: "end", scale: 1.0, x, y }));
            }
            steps
        } else {
            self.route = if ended { Route::Idle } else { Route::View };
            vec![Step::ReplayHeld]
        }
    }

    /// The two-finger double-tap at the pointer: the call to ask the page with when it takes pinches (it answers
    /// `true` when it took the tap; anything else and the viewer's own smart zoom runs), `None` for a page that does
    /// not (the viewer's own smart zoom, as before).
    pub fn double_tap(&self, x: f64, y: f64) -> Option<Call> {
        self.takes.then_some(Call { phase: "double-tap", scale: 1.0, x, y })
    }
}
