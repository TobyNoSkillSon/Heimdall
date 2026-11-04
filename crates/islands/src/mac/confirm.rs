//! A second step before an answer goes to the agent: the clicked button's row turns into
//! "Sure?  [Cancel] [Confirm]" in place. Confirm, or a second click on the same button within `WINDOW` seconds, sends
//! it; Cancel, or nothing for `WINDOW` seconds, puts the row back.

use objc2::rc::Retained;
use objc2_app_kit::NSColor;
use objc2_quartz_core::CALayer;

use super::look::*;
use super::sys;
use crate::model::geometry::{Point, Rect};

pub const WINDOW: f64 = 4.0;
/// A double-click or a bounced tap is not a decision: Confirm takes clicks only this long after "Sure?" shows.
pub const SETTLE: f64 = 0.35;

/// "Later" only puts a question off (it comes back): one click. Every other answer asks first.
pub fn needed(action: &str) -> bool {
    !["later", "not-now", "notnow"].contains(&action.to_lowercase().as_str())
}

#[derive(Clone)]
pub struct Row {
    pub layer: Retained<CALayer>,
    pub cancel: Rect,
    pub confirm: Rect,
    /// The button that was clicked: a second click there confirms (unless it lands on Cancel).
    pub clicked: Rect,
    /// `sys::uptime()` when it showed.
    pub started: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hit {
    Confirm,
    Cancel,
    Outside,
    Ignored,
}

/// The row over `row` (the clicked button's row, in the parent's coordinates, y down), painted `background` so it
/// covers the buttons under it. Arrives with a quick fade and a small settle.
pub fn row(row: Rect, clicked: Rect, background: &NSColor, scale: f64) -> Row {
    let h = row.h.min(34.0);
    let y0 = row.min_y() + (row.h - h) / 2.0;
    let l = layer();
    l.setAnchorPoint(pt(0.0, 0.0));
    let frame = row.inset(-2.0, -2.0);
    l.setFrame(cgr(frame));
    set_bg(&l, background);
    let (pad, gap) = (2.0, 8.0);
    let ask = text1("Sure?", &button_font(), &ink2(), 10_000.0, scale);
    let ask_w = ask.bounds().size.width.ceil() + 6.0;
    let cw0 = clicked.w.max(104.0);
    let mut confirm = Rect::new(clicked.mid_x() - cw0 / 2.0, y0, cw0, h);
    confirm.x = confirm.min_x().max(row.min_x()).min(row.max_x() - confirm.w);
    let left_w = confirm.min_x() - row.min_x() - gap;
    let right_w = row.max_x() - confirm.max_x() - gap;
    let cancel;
    let mut ask_x: Option<f64> = None;
    if left_w.max(right_w) < 80.0 {
        let cw = 92.0;
        ask_x = Some(row.min_x());
        cancel = Rect::new(row.min_x() + ask_w + gap, y0, cw, h);
        confirm = Rect::new(cancel.max_x() + gap, y0, row.max_x() - cancel.max_x() - gap, h);
    } else if right_w < 80.0 || (left_w > right_w && left_w >= ask_w + gap + 96.0) {
        let cw = if left_w >= ask_w + gap + 96.0 { left_w - ask_w - gap } else { left_w };
        cancel = Rect::new(confirm.min_x() - gap - cw, y0, cw, h);
        if cw < left_w {
            ask_x = Some(confirm.min_x() - gap - cw - gap - ask_w);
        }
    } else {
        cancel = Rect::new(confirm.max_x() + gap, y0, right_w, h);
        if left_w >= ask_w {
            ask_x = Some(confirm.min_x() - gap - ask_w);
        }
    }
    if let Some(ax) = ask_x {
        ask.setPosition(pt(ax - row.min_x() + pad, pad + (row.h - ask.bounds().size.height) / 2.0));
        l.addSublayer(&ask);
    }
    let cap = |r: Rect, fill: &NSColor, text: &str, ink: &NSColor| {
        let local = r.offset(-frame.min_x(), -frame.min_y());
        let c = boxed(local.x, local.y, local.w, local.h, fill, h / 2.0);
        l.addSublayer(&c);
        let t = text1(text, &button_font(), ink, r.w - 16.0, scale);
        let tb = t.bounds().size;
        t.setPosition(pt(local.min_x() + (local.w - tb.width) / 2.0, local.min_y() + (local.h - tb.height) / 2.0));
        l.addSublayer(&t);
    };
    cap(cancel, &white(0.16), "Cancel", &white(1.0));
    cap(confirm, &white(1.0), "Confirm", &black());
    let f = basic("opacity", Some(any(&*num(0.0))), Some(any(&*num(1.0))), 0.18);
    let s = basic("transform.scale", Some(any(&*num(0.97))), Some(any(&*num(1.0))), 0.18);
    let g = group(&[&f, &s], 0.18);
    g.setTimingFunction(Some(&ease_out()));
    timed(&g, &l);
    add(&l, &g, "confirmIn");
    Row { layer: l, cancel, confirm, clicked, started: sys::uptime() }
}

/// What a click at `p` (the row's parent coordinates) means while the row is up.
pub fn hit(r: &Row, p: Point, now: f64) -> Hit {
    if r.cancel.inset(-2.0, -4.0).contains(p) {
        return Hit::Cancel;
    }
    let age = now - r.started;
    if r.confirm.inset(-2.0, -4.0).contains(p) || (r.clicked.contains(p) && age <= WINDOW) {
        return if age >= SETTLE { Hit::Confirm } else { Hit::Ignored };
    }
    Hit::Outside
}

/// Out: a quick fade, then the layer goes.
pub fn dismiss(r: &Row) {
    let l = r.layer.clone();
    transaction(false, Some(Box::new(move || l.removeFromSuperlayer())), || fade(&r.layer, 0.0, 0.12, 0.0));
}
