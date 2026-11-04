//! The one-system design's small shared marks (`Assistant/Plans/goals/one-system.html`): the ghost box a proposal
//! sits in, its Yes chip with a small waveform (one word or one click), "+N", dashed outlines, glows and dots. Static
//! layers; nothing moves at rest.

use objc2::rc::Retained;
use objc2::Message;
use objc2_app_kit::NSColor;
use objc2_core_foundation::CGSize;
use objc2_foundation::{NSArray, NSNumber};
use objc2_quartz_core::{CALayer, CAShapeLayer, CATextLayer};

use super::draw::CgPath;
use super::look::*;
use crate::model::geometry::Rect;

pub const YES_H: f64 = 24.0;

pub fn proof_green() -> Retained<NSColor> {
    rgb(0x30D158, 1.0)
}
pub fn run_blue() -> Retained<NSColor> {
    rgb(0x0A84FF, 1.0)
}
pub fn amber() -> Retained<NSColor> {
    rgb(0xFFD60A, 1.0)
}
/// The dim fill of quiet chips and tracks (#1c1c1e, #2c2c2e).
pub fn chip_fill() -> Retained<NSColor> {
    rgb(0x1C1C1E, 1.0)
}
pub fn track_fill() -> Retained<NSColor> {
    rgb(0x2C2C2E, 1.0)
}

pub fn glow(l: &CALayer, c: &NSColor, radius: f64, opacity: f32) {
    l.setShadowColor(Some(&cg(c)));
    l.setShadowOpacity(opacity);
    l.setShadowRadius(radius);
    l.setShadowOffset(CGSize { width: 0.0, height: 0.0 });
}

/// A shape layer in a `w` × `h` frame at (x, y), stroked, optionally dashed.
#[allow(clippy::too_many_arguments)]
pub fn stroked(path: &CgPath, x: f64, y: f64, w: f64, h: f64, color: &NSColor, width: f64, dash: Option<(f64, f64)>) -> Retained<CAShapeLayer> {
    let s = CAShapeLayer::new();
    no_implicit(&s);
    s.setAnchorPoint(pt(0.0, 0.0));
    s.setFrame(cgr(Rect::new(x, y, w, h)));
    s.setPath(Some(path.cg()));
    s.setFillColor(None);
    s.setStrokeColor(Some(&cg(color)));
    s.setLineWidth(width);
    if let Some((on, off)) = dash {
        s.setLineDashPattern(Some(&NSArray::from_retained_slice(&[NSNumber::new_f64(on), NSNumber::new_f64(off)])));
    }
    s
}

/// A rounded rect's outline path inside (0, 0, w, h), inset by half the line.
pub fn round_rect_path(w: f64, h: f64, r: f64, inset: f64) -> CgPath {
    let p = CgPath::new();
    let (x0, y0, x1, y1) = (inset, inset, w - inset, h - inset);
    let r = r.min((x1 - x0) / 2.0).min((y1 - y0) / 2.0).max(0.0);
    use std::f64::consts::{FRAC_PI_2, PI};
    p.move_to(x0 + r, y0);
    p.line_to(x1 - r, y0);
    p.arc(x1 - r, y0 + r, r, -FRAC_PI_2, 0.0, false);
    p.line_to(x1, y1 - r);
    p.arc(x1 - r, y1 - r, r, 0.0, FRAC_PI_2, false);
    p.line_to(x0 + r, y1);
    p.arc(x0 + r, y1 - r, r, FRAC_PI_2, PI, false);
    p.line_to(x0, y0 + r);
    p.arc(x0 + r, y0 + r, r, PI, 1.5 * PI, false);
    p
}

/// A dashed rounded outline (a proposal's bar segment, a claim's pill, the "claimed" chip).
#[allow(clippy::too_many_arguments)]
pub fn dashed_box(x: f64, y: f64, w: f64, h: f64, r: f64, color: &NSColor, width: f64, dash: (f64, f64)) -> Retained<CAShapeLayer> {
    stroked(&round_rect_path(w, h, r, width / 2.0), x, y, w, h, color, width, Some(dash))
}

/// A circle's outline centred at (cx, cy).
pub fn ring_path(d: f64, r: f64) -> CgPath {
    let p = CgPath::new();
    p.move_to(d / 2.0 + r, d / 2.0);
    p.arc(d / 2.0, d / 2.0, r, 0.0, 2.0 * std::f64::consts::PI, false);
    p
}

/// A proposal's box: an inset hairline at 28 % white on a faint fill, radius 10.
pub fn ghost_box(x: f64, y: f64, w: f64, h: f64) -> Retained<CALayer> {
    let b = boxed(x, y, w, h, &white(0.03), 10.0);
    b.setBorderColor(Some(&cg(&white(0.28))));
    b.setBorderWidth(1.0);
    b
}

/// An open circle: a proposal's dot.
pub fn open_dot(cx: f64, cy: f64, d: f64, color: &NSColor) -> Retained<CALayer> {
    let b = boxed(cx - d / 2.0, cy - d / 2.0, d, d, &clear(), d / 2.0);
    b.setBorderColor(Some(&cg(color)));
    b.setBorderWidth(1.5);
    b
}

pub fn dot(cx: f64, cy: f64, d: f64, color: &NSColor) -> Retained<CALayer> {
    boxed(cx - d / 2.0, cy - d / 2.0, d, d, color, d / 2.0)
}

/// The Yes chip (24 pt, a small waveform then "Yes"); answered, a check stands where the waveform was. Returns the
/// chip and its width; its top-left is at (0, 0).
pub fn yes_chip(answered: bool, scale: f64) -> (Retained<CALayer>, f64) {
    let f = system(12.5, Weight::Semibold);
    let t = text1("Yes", &f, &*if answered { ink2() } else { white(1.0) }, 60.0, scale);
    let tw = t.bounds().size.width;
    let w = 10.0 + 10.0 + 5.0 + tw + 10.0;
    let c = boxed(0.0, 0.0, w, YES_H, &white(if answered { 0.08 } else { 0.16 }), YES_H / 2.0);
    let s = symbol_layer(if answered { "checkmark" } else { "waveform" }, 10.0, &[if answered { proof_green() } else { white(1.0) }], Weight::Semibold, scale);
    s.setPosition(pt(15.0, YES_H / 2.0));
    c.addSublayer(&s);
    t.setPosition(pt(25.0, (YES_H - t.bounds().size.height) / 2.0));
    c.addSublayer(&t);
    (c, w)
}

/// "+2": how many proposals queue behind the shown one.
pub fn plus(n: usize, scale: f64) -> Option<Retained<CATextLayer>> {
    (n > 0).then(|| text1(&format!("+{n}"), &digits(11.5, Weight::Semibold), &white(0.5), 40.0, scale))
}

/// A quiet capsule with a symbol and a word (a goal chip, a proof chip), `h` tall; returns it and its width.
#[allow(clippy::too_many_arguments)]
pub fn chip(symbol: &str, label: &str, font: &objc2_app_kit::NSFont, ink_c: &NSColor, sym_c: &NSColor, fill: &NSColor, h: f64, pad: f64, max_w: f64, scale: f64) -> (Retained<CALayer>, f64) {
    let t = text1(label, font, ink_c, (max_w - 2.0 * pad - 16.0).max(20.0), scale);
    let tw = t.bounds().size.width;
    let w = pad + 11.0 + 5.0 + tw + pad;
    let c = boxed(0.0, 0.0, w, h, fill, h / 2.0);
    circular(&c);
    let s = symbol_layer(symbol, 11.0, &[sym_c.retain()], Weight::Medium, scale);
    s.setPosition(pt(pad + 5.5, h / 2.0));
    c.addSublayer(&s);
    t.setPosition(pt(pad + 16.0, (h - t.bounds().size.height) / 2.0));
    c.addSublayer(&t);
    (c, w)
}

/// Words wrapped to `w`, at most `max_lines` lines, each line in a `line_h` box (CSS line-height), the last cut with an
/// ellipsis when more is left. Returns the layer (top-left anchored) and its height (`line_h` × lines).
pub fn pitched(s: &str, font: &objc2_app_kit::NSFont, color: &NSColor, w: f64, max_lines: usize, line_h: f64, scale: f64) -> (Retained<CALayer>, f64) {
    let words: Vec<&str> = s.split_whitespace().collect();
    let mut lines: Vec<String> = vec![];
    let mut cur = String::new();
    let mut k = 0;
    while k < words.len() {
        let try_line = if cur.is_empty() { words[k].to_owned() } else { format!("{cur} {}", words[k]) };
        if text_width(&try_line, font) <= w || cur.is_empty() {
            cur = try_line;
            k += 1;
        } else {
            lines.push(std::mem::take(&mut cur));
            if lines.len() + 1 == max_lines.max(1) {
                break;
            }
        }
    }
    // The last line takes the rest (cut with an ellipsis by the one-line layer when too wide).
    let rest: Vec<&str> = words[k..].to_vec();
    let last = [cur.as_str()].into_iter().chain(rest.iter().copied()).filter(|t| !t.is_empty()).collect::<Vec<_>>().join(" ");
    if !last.is_empty() || lines.is_empty() {
        lines.push(last);
    }
    let l = layer();
    l.setAnchorPoint(pt(0.0, 0.0));
    let lh = line_height(font);
    for (n, t) in lines.iter().enumerate() {
        let tl = text1(t, font, color, w, scale);
        tl.setPosition(pt(0.0, n as f64 * line_h + ((line_h - lh) / 2.0).max(0.0)));
        l.addSublayer(&tl);
    }
    let h = line_h * lines.len() as f64;
    l.setBounds(cgr(Rect::new(0.0, 0.0, w, h)));
    (l, h)
}
