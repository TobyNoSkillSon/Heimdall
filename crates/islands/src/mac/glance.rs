//! A job island's bottom row (`model::glance`): the open target's chip on the left; on the right the stage, a small
//! live graphic with its figure, and how long. And the graphics themselves, shared with the parts: the sparkline
//! (the `line` part's look, the decider's training-loss line the user liked, 23 Oct 17:32), small for a glance, and the
//! share bar. Nothing here moves at rest: a sparkline draws itself when it arrives and draws only its newest step
//! when a rewrite arrives; a bar grows from its last value.

use objc2::rc::Retained;
use objc2_app_kit::NSColor;
use objc2_quartz_core::{CALayer, CAShapeLayer, CATextLayer};

use super::draw;
use super::look::*;
use super::parts::{Body, Control, HitRegion};
use crate::model::geometry::Rect;
use crate::model::glance::{clock_text, glance, spark_points, Clock, Mark, Series, Tone};
use crate::model::{j, time, Island, Json, OpenTarget};

/// A value's graphic in a key-value row and in the glance.
pub const MARK_W: f64 = 38.0;
pub const MARK_H: f64 = 14.0;
/// The bottom row's height: the open chip's.
pub const FOOT_H: f64 = 26.0;

pub struct SparkStyle {
    pub line: f64,
    pub dot: f64,
    pub inset: f64,
}

impl SparkStyle {
    /// The `line` part: a 2 pt line and a 7 pt end dot.
    pub const LINE: SparkStyle = SparkStyle { line: 2.0, dot: 7.0, inset: 3.0 };
    /// The glance and key values: the same look at a glance's size.
    pub const MINI: SparkStyle = SparkStyle { line: 1.6, dot: 4.5, inset: 2.5 };
}

pub struct Spark {
    pub layer: Retained<CALayer>,
    shape: Retained<CAShapeLayer>,
    dot: Retained<CALayer>,
    /// The end point in the parent's coordinates.
    pub end: (f64, f64),
    /// Where the line was before its newest point (in the spark's own coordinates), and the share of its length up
    /// to there.
    before: Option<((f64, f64), f64)>,
}

impl Spark {
    /// A line through `values` filling `r` (parent coordinates, y down), its last point marked with a dot.
    pub fn draw(values: &[f64], r: Rect, style: &SparkStyle) -> Spark {
        let c = layer();
        c.setAnchorPoint(pt(0.0, 0.0));
        c.setFrame(cgr(r));
        let pts = spark_points(values, r.w, r.h, style.inset);
        let path = draw::CgPath::new();
        for (k, (x, y)) in pts.iter().enumerate() {
            if k == 0 {
                path.move_to(*x, *y);
            } else {
                path.line_to(*x, *y);
            }
        }
        let s = CAShapeLayer::new();
        s.setPath(Some(path.cg()));
        s.setStrokeColor(Some(&cg(&white(1.0))));
        s.setFillColor(None);
        s.setLineWidth(style.line);
        // SAFETY: framework constants.
        unsafe {
            s.setLineJoin(objc2_quartz_core::kCALineJoinRound);
            s.setLineCap(objc2_quartz_core::kCALineCapRound);
        }
        no_implicit(&s);
        c.addSublayer(&s);
        let last = pts.last().copied().unwrap_or((r.w, r.h / 2.0));
        let d = style.dot;
        let dot = boxed(last.0 - d / 2.0, last.1 - d / 2.0, d, d, &white(1.0), d / 2.0);
        c.addSublayer(&dot);
        let seg = |a: &(f64, f64), b: &(f64, f64)| ((b.0 - a.0).powi(2) + (b.1 - a.1).powi(2)).sqrt();
        let total: f64 = pts.windows(2).map(|w| seg(&w[0], &w[1])).sum();
        let before = (pts.len() >= 3 && total > 0.0).then(|| {
            let upto: f64 = pts[..pts.len() - 1].windows(2).map(|w| seg(&w[0], &w[1])).sum();
            (pts[pts.len() - 2], upto / total)
        });
        Spark { layer: c, shape: s, dot, end: (r.x + last.0, r.y + last.1), before }
    }

    /// The arrival: the line draws itself from its start.
    pub fn draw_in(&self) -> Box<dyn Fn()> {
        let s = self.shape.clone();
        Box::new(move || {
            let a = Spring::CALM.animation("strokeEnd", any(&*num(0.0)), any(&*num(1.0)), 0.0);
            add(&s, &a, "draw");
        })
    }

    /// A rewrite's arrival: only the newest step draws, and the dot moves along with it.
    pub fn step_in(&self) -> Option<Box<dyn Fn()>> {
        let ((bx, by), share) = self.before?;
        let (s, dot) = (self.shape.clone(), self.dot.clone());
        Some(Box::new(move || {
            let a = Spring::CALM.animation("strokeEnd", any(&*num(share)), any(&*num(1.0)), 0.0);
            add(&s, &a, "step");
            let to = dot.position();
            let from = pt(bx - dot.bounds().size.width / 2.0, by - dot.bounds().size.height / 2.0);
            let m = Spring::CALM.animation("position", any(&*point_val(from)), any(&*point_val(to)), 0.0);
            add(&dot, &m, "step");
        }))
    }
}

/// A share as a short capsule: a dim track and a white fill.
fn bar(r: Rect, share: f64, root: &CALayer) -> Retained<CALayer> {
    let h = 4.0;
    let y = r.y + (r.h - h) / 2.0;
    root.addSublayer(&boxed(r.x, y, r.w, h, &white(0.2), h / 2.0));
    let fill = boxed(r.x, y, (r.w * share).max(h), h, &white(1.0), h / 2.0);
    root.addSublayer(&fill);
    fill
}

/// Draws `mark` in `r` and registers how it arrives: a sparkline draws itself, a bar grows. On a rewrite, `moved`
/// says whether its value changed (only then does it move: a sparkline's newest step, a bar from `before`).
pub fn place_mark(root: &CALayer, mark: &Mark, r: Rect, moved: Option<f64>, body: &mut Body) {
    match mark {
        Mark::None => {}
        Mark::Bar(share) => {
            let fill = bar(r, *share, root);
            let grow = |from: f64, fill: Retained<CALayer>| -> Box<dyn Fn()> {
                Box::new(move || {
                    let w = fill.bounds().size.width;
                    let a = Spring::GROW.animation("bounds.size.width", any(&*num(from.min(w))), any(&*num(w)), 0.0);
                    add(&fill, &a, "grow");
                })
            };
            body.arrivals.push(grow(4.0, fill.clone()));
            if let Some(before) = moved {
                body.updates.push(grow((r.w * before.clamp(0.0, 1.0)).max(4.0), fill));
            }
        }
        Mark::Spark(values) => {
            let sp = Spark::draw(values, r, &SparkStyle::MINI);
            root.addSublayer(&sp.layer);
            body.arrivals.push(sp.draw_in());
            if moved.is_some() {
                if let Some(step) = sp.step_in() {
                    body.updates.push(step);
                }
            }
        }
    }
}

/// The open target as a chip: its label and an arrow, or a dim "Not found" when the file is gone.
pub struct OpenChip {
    cap: Retained<CALayer>,
    text: Retained<CATextLayer>,
    arrow: Option<Retained<CALayer>>,
    pub width: f64,
    target: String,
    live: bool,
}

const CHIP_PAD: f64 = 11.0;

pub fn open_chip(target: &str, island: &Island, max_w: f64, scale: f64) -> OpenChip {
    let b = OpenTarget::button(target, matches!(island.state(), "running" | "waiting"));
    let gone = !b.ready;
    let arrow_room = if gone { 0.0 } else { 14.0 };
    let t = text1(&b.label, &small_font(), &*if gone { ink2() } else { ink() }, (max_w - 2.0 * CHIP_PAD - arrow_room).max(24.0), scale);
    let arrow = (!gone).then(|| symbol_layer("arrow.up.right", 10.0, &[ink()], Weight::Bold, scale));
    let width = t.bounds().size.width + 2.0 * CHIP_PAD + arrow_room;
    let cap = boxed(0.0, 0.0, width, FOOT_H, &white(if gone { 0.08 } else { 0.14 }), FOOT_H / 2.0);
    OpenChip { cap, text: t, arrow, width, target: target.to_owned(), live: b.live }
}

impl OpenChip {
    /// Puts the chip at (`x`, `y`) and registers its click; returns its height.
    pub fn place(self, root: &CALayer, x: f64, y: f64, body: &mut Body) -> f64 {
        self.cap.setPosition(pt(x, y));
        root.addSublayer(&self.cap);
        let tw = self.text.bounds().size.width;
        self.text.setPosition(pt(x + CHIP_PAD, y + (FOOT_H - self.text.bounds().size.height) / 2.0));
        root.addSublayer(&self.text);
        if let Some(a) = &self.arrow {
            a.setPosition(pt(x + CHIP_PAD + tw + 9.0, y + FOOT_H / 2.0));
            root.addSublayer(a);
        }
        if self.live {
            let r = Rect::new(x, y, self.width, FOOT_H);
            body.regions.push(HitRegion { rect: r, control: Control::Open { target: self.target, from: Some(r), image: None } });
        }
        FOOT_H
    }
}

const GAP: f64 = 10.0;
const MARK_GAP: f64 = 5.0;
/// The running clock's widest next text: every digit an 8 and one digit more ("7 min" → "88 min", "1 h 9" →
/// "8 h 88").
pub fn clock_room(c: &str) -> String {
    let mut out = String::new();
    let mut grown = false;
    let chars: Vec<char> = c.chars().collect();
    for (k, ch) in chars.iter().enumerate() {
        if ch.is_ascii_digit() {
            out.push('8');
            let last_digit = chars.get(k + 1).is_none_or(|n| !n.is_ascii_digit());
            if last_digit && !grown && chars[k + 1..].iter().all(|n| !n.is_ascii_digit()) {
                out.push('8');
                grown = true;
            }
        } else {
            out.push(*ch);
        }
    }
    out
}

/// The open chip keeps at least this share of the row (or all it needs); the stage words give way first.
const CHIP_SHARE: f64 = 0.45;
/// Words narrower than this are left out rather than cut to a stub.
const WORDS_MIN: f64 = 44.0;

/// The bottom row of a job island at `y`; returns the body's height after it (none at all when it has nothing to
/// show).
#[allow(clippy::too_many_arguments)]
pub fn build_footer(
    island: &Island,
    p: &Json,
    root: &CALayer,
    y: f64,
    w: f64,
    scale: f64,
    born: Option<f64>,
    series: &Series,
    header_ring: bool,
    body: &mut Body,
) -> f64 {
    let mut g = glance(island, series, header_ring, born);
    let target = j::str(p.get("target")).filter(|t| !t.trim().is_empty());
    // A finished job with nothing but its duration to say gets no row of its own.
    if target.is_none() && g.words.is_none() && g.figure.is_none() && g.mark == Mark::None && matches!(g.clock, Clock::Took(_)) {
        g.clock = Clock::None;
    }
    if g.is_empty() && target.is_none() {
        return if y > 0.0 { y - PART_GAP } else { y };
    }
    // The right side, measured first: the figure, the clock and its glyph keep their size; the words give way.
    let figure = g.figure.as_deref().map(|f| text1(f, &small_mono(), &white(1.0), w, scale));
    let clock = clock_text(g.clock, time::now());
    let font = small_mono();
    let clock_l = clock.as_deref().map(|c| {
        // Right-aligned in room for one more digit: a minute later it grows leftwards into it, nothing moves or clips.
        let l = text_layer(c, &font, &ink2(), w, 1, Align::Right, false, scale);
        if let Clock::Since(_) = g.clock {
            let b = l.bounds().size;
            let room = text_width(&clock_room(c), &font).ceil() + 1.0;
            l.setBounds(cgr(Rect::new(0.0, 0.0, b.width.max(room), b.height)));
        }
        l
    });
    let glyph = clock_l.as_ref().map(|_| symbol_layer("timer", 11.0, &[ink2()], Weight::Semibold, scale));
    let words_color = tone_color(g.tone);
    let words_font = if g.tone == Tone::Value { small_mono() } else { small_font() };
    let mut mark_w = if g.mark == Mark::None { 0.0 } else { MARK_W };
    let clock_w = clock_l.as_ref().map(|c| c.bounds().size.width);
    let figure_w = figure.as_ref().map(|f| f.bounds().size.width);
    let fixed_with = |clock: bool, mark_w: f64| {
        let mut parts = vec![];
        if mark_w > 0.0 || figure.is_some() {
            parts.push(mark_w + figure_w.map(|f| f + if mark_w > 0.0 { MARK_GAP } else { 0.0 }).unwrap_or(0.0));
        }
        if let (true, Some(c)) = (clock, clock_w) {
            parts.push(13.0 + 3.0 + c);
        }
        parts.iter().sum::<f64>() + GAP * parts.len().saturating_sub(1) as f64
    };
    let chip_keep = target.map(|t| open_chip(t, island, w, scale).width.min(w * CHIP_SHARE));
    let room_for_words = |fixed: f64| w - fixed - if fixed > 0.0 { GAP } else { 0.0 } - chip_keep.map(|c| c + 14.0).unwrap_or(0.0);
    // Crowded: the duration goes before the stage does (what it is doing matters more than for how long); in a row
    // too narrow for the chip and the right side at all, the duration and then the graphic go (its figure stays).
    let chip_room = chip_keep.map(|c| c + 14.0).unwrap_or(0.0);
    let mut keep_clock = g.words.is_none() || room_for_words(fixed_with(true, mark_w)) >= WORDS_MIN;
    if keep_clock && chip_room + fixed_with(true, mark_w) > w {
        keep_clock = false;
    }
    if chip_room + fixed_with(keep_clock, mark_w) > w {
        mark_w = 0.0;
    }
    let clock_l = clock_l.filter(|_| keep_clock);
    let glyph = glyph.filter(|_| keep_clock);
    let fixed = fixed_with(keep_clock, mark_w);
    let words_room = room_for_words(fixed);
    let words = g.words.as_deref().filter(|_| words_room >= WORDS_MIN).map(|s| text1(s, &words_font, &words_color, words_room, scale));
    let right_w = fixed + words.as_ref().map(|l| l.bounds().size.width + if fixed > 0.0 { GAP } else { 0.0 }).unwrap_or(0.0);
    // The chip takes what is left, cut short when it must be.
    if let Some(t) = target {
        let room = if right_w > 0.0 { w - right_w - 14.0 } else { w };
        open_chip(t, island, room.max(chip_keep.unwrap_or(90.0)), scale).place(root, 0.0, y, body);
    }
    // Right to left from the edge: clock, then the graphic and its figure, then the words.
    let cy = y + FOOT_H / 2.0;
    let mut x = w;
    let mut first = true;
    let mut gap = |x: &mut f64| {
        if !std::mem::take(&mut first) {
            *x -= GAP;
        }
    };
    let centre = |l: &CATextLayer, x: f64| l.setPosition(pt(x, cy - l.bounds().size.height / 2.0));
    if let (Some(c), Some(gl)) = (&clock_l, &glyph) {
        gap(&mut x);
        x -= c.bounds().size.width;
        centre(c, x);
        root.addSublayer(c);
        x -= 3.0 + 6.5;
        gl.setPosition(pt(x, cy));
        root.addSublayer(gl);
        x -= 6.5;
        if let Clock::Since(_) = g.clock {
            let since = g.clock;
            body.clock = Some((c.clone(), Box::new(move || clock_text(since, time::now()).unwrap_or_default())));
        }
    }
    if figure.is_some() || mark_w > 0.0 {
        gap(&mut x);
    }
    if let Some(f) = &figure {
        x -= f.bounds().size.width;
        centre(f, x);
        root.addSublayer(f);
        if mark_w > 0.0 {
            x -= MARK_GAP;
        }
    }
    if mark_w > 0.0 {
        x -= mark_w;
        // (`mark_w` is 0 when the row had no room for the graphic: only its figure shows.)
        place_mark(root, &g.mark, Rect::new(x, cy - MARK_H / 2.0, MARK_W, MARK_H), series.before("progress"), body);
    }
    if let Some(l) = &words {
        gap(&mut x);
        x -= l.bounds().size.width;
        centre(l, x);
        root.addSublayer(l);
    }
    y + FOOT_H
}

/// The glance's words: quiet grey, orange while waiting on the user, white for a result's value.
pub fn tone_color(t: Tone) -> Retained<NSColor> {
    match t {
        Tone::Waiting => orange(),
        Tone::Value => white(1.0),
        Tone::Plain => ink2(),
    }
}

#[cfg(test)]
mod tests {
    use super::clock_room;

    #[test]
    fn the_clock_keeps_room_for_one_more_digit() {
        assert_eq!(clock_room("7 min"), "88 min");
        assert_eq!(clock_room("59 min"), "888 min");
        assert_eq!(clock_room("1 h 9"), "8 h 88");
        assert_eq!(clock_room("<1 min"), "<88 min");
    }
}
