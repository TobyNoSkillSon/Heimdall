//! Placement maths: the docks (right and left edge columns, the row under the notch), snapping, and where a released
//! island goes. Display-local rects are y down (origin at the display's top-left); `snap` and `edge_drop` take y-up
//! screen rects.

use super::geometry::{Rect, Size, Vector};

/// Automatic slots: a column at the display's right edge that starts below the band where a browser's tabs and
/// toolbar live (`top`), stacking downward; a full column continues to its left. Placed islands in the way are
/// stepped past.
pub fn auto_column(sizes: &[Size], width: f64, top: f64, bottom: f64, margin: f64, gap: f64, obstacles: &[Rect]) -> Vec<Rect> {
    let (mut right, mut y, mut col_width) = (width - margin, top, 0.0f64);
    let mut out = vec![];
    for s in sizes {
        let mut r = Rect::new(right - s.w, y, s.w, s.h);
        for _ in 0..64 {
            r = Rect::new(right - s.w, y, s.w, s.h);
            if r.max_y() > bottom && y > top && right - col_width - gap - s.w >= margin {
                right -= col_width + gap;
                col_width = 0.0;
                y = top;
                continue;
            }
            if let Some(o) = obstacles.iter().find(|o| o.inset(-gap / 2.0, -gap / 2.0).intersects(&r)) {
                y = o.max_y() + gap;
                continue;
            }
            break;
        }
        out.push(r);
        y = r.max_y() + gap;
        col_width = col_width.max(s.w);
    }
    out
}

fn nearest(v: f64, cands: &[f64], reach: f64) -> Option<f64> {
    cands.iter().copied().filter(|c| (c - v).abs() <= reach).fold(None, |best: Option<f64>, c| match best {
        Some(b) if (b - v).abs() <= (c - v).abs() => Some(b),
        _ => Some(c),
    })
}

/// Where a dropped island settles: free placement, pulled within `reach` to the row under the menu bar, a screen
/// edge or a neighbour; never into the menu bar strip or onto a neighbour. Rects are y-up screen rects; `row_top` is
/// the highest an island's top may be.
pub fn snap(r: Rect, screen: Rect, visible: Rect, row_top: f64, neighbours: &[Rect], m: f64, reach: f64) -> Rect {
    let (w, h) = (r.w, r.h);
    let mut yc = vec![row_top, visible.min_y() + m + h];
    for o in neighbours.iter().filter(|o| o.max_x() > r.min_x() - w && o.min_x() < r.max_x() + w) {
        yc.extend([o.max_y(), o.min_y() - m, o.max_y() + m + h]);
    }
    let mut top = nearest(r.max_y(), &yc, reach).unwrap_or(r.max_y());
    top = top.min(row_top);
    top = top.max(visible.min_y() + m + h);
    let y = top - h;
    let mut xc = vec![screen.min_x() + m, screen.max_x() - m - w, screen.mid_x() - w / 2.0];
    for o in neighbours {
        let same_line = o.min_y() < top && o.max_y() > y;
        let stacked = (o.min_y() - m - top).abs() < 1.0 || (o.max_y() + m - y).abs() < 1.0;
        if same_line {
            xc.extend([o.max_x() + m, o.min_x() - m - w]);
        }
        if stacked {
            xc.extend([o.min_x(), o.max_x() - w, o.mid_x() - w / 2.0]);
        }
    }
    let mut x = nearest(r.min_x(), &xc, reach).unwrap_or(r.min_x());
    x = x.max(screen.min_x() + m).min(screen.max_x() - m - w);
    for _ in 0..4 {
        let probe = Rect::new(x, y, w, h).inset(1.0, 1.0);
        let Some(o) = neighbours.iter().find(|o| o.intersects(&probe)) else { break };
        let sides: Vec<f64> =
            [o.max_x() + m, o.min_x() - m - w].into_iter().filter(|s| *s >= screen.min_x() + m && *s <= screen.max_x() - m - w).collect();
        let Some(s) = sides.iter().copied().fold(None, |best: Option<f64>, s| match best {
            Some(b) if (b - x).abs() <= (s - x).abs() => Some(b),
            _ => Some(s),
        }) else {
            break;
        };
        x = s;
    }
    Rect::new(x.round(), y.round(), w, h)
}

fn mirror(width: f64) -> impl Fn(&Rect) -> Rect {
    move |r: &Rect| Rect::new(width - r.max_x(), r.min_y(), r.w, r.h)
}

/// The left dock: the right column mirrored.
pub fn left_column(sizes: &[Size], width: f64, top: f64, bottom: f64, margin: f64, gap: f64, obstacles: &[Rect]) -> Vec<Rect> {
    let m = mirror(width);
    let obs: Vec<Rect> = obstacles.iter().map(&m).collect();
    auto_column(sizes, width, top, bottom, margin, gap, &obs).iter().map(&m).collect()
}

/// The top dock: a row under the menu bar centred on the notch (`centre`), side by side in order; a row wider than
/// the display continues below.
pub fn top_row(sizes: &[Size], width: f64, top: f64, centre: Option<f64>, margin: f64, gap: f64) -> Vec<Rect> {
    let mid = centre.unwrap_or(width / 2.0);
    let mut rows: Vec<Vec<usize>> = vec![vec![]];
    let mut used = 0.0;
    for (k, s) in sizes.iter().enumerate() {
        let need = (if rows.last().unwrap().is_empty() { 0.0 } else { gap }) + s.w;
        if !rows.last().unwrap().is_empty() && used + need > width - 2.0 * margin {
            rows.push(vec![]);
            used = 0.0;
        }
        used += (if rows.last().unwrap().is_empty() { 0.0 } else { gap }) + s.w;
        rows.last_mut().unwrap().push(k);
    }
    let mut out = vec![Rect::ZERO; sizes.len()];
    let mut y = top;
    for row in rows.iter().filter(|r| !r.is_empty()) {
        let total: f64 = row.iter().map(|k| sizes[*k].w).sum::<f64>() + gap * (row.len() as f64 - 1.0);
        let mut x = margin.max((mid - total / 2.0).round()).min(width - margin - total);
        let mut h = 0.0f64;
        for &k in row {
            out[k] = Rect::new(x, y, sizes[k].w, sizes[k].h);
            x += sizes[k].w + gap;
            h = h.max(sizes[k].h);
        }
        y += h + gap;
    }
    out
}

/// One edge column (the right edge). Islands pinned at a height keep it; the rest stack in order from `top`, stepping
/// past pins level with or above their slot; then swept top to bottom so nothing overlaps (an open island pushes the
/// ones under it down), and back up from `bottom` on overflow. Pins stay between `highest` and `bottom`.
#[allow(clippy::too_many_arguments)]
pub fn edge_column(
    sizes: &[Size],
    pins: &[Option<f64>],
    width: f64,
    top: f64,
    highest: f64,
    bottom: f64,
    margin: f64,
    gap: f64,
    obstacles: &[Rect],
) -> Vec<Rect> {
    let n = sizes.len();
    let mut want = vec![0.0f64; n];
    for k in 0..n {
        if let Some(p) = pins[k] {
            want[k] = p.max(highest).min(bottom - sizes[k].h);
        }
    }
    let pill = sizes.iter().map(|s| s.h).fold(f64::INFINITY, f64::min);
    let pill = if pill.is_finite() { pill } else { 0.0 };
    let pinned: Vec<Rect> = (0..n).filter(|k| pins[*k].is_some()).map(|k| Rect::new(0.0, want[k], 1.0, sizes[k].h)).collect();
    let mut y = top;
    for k in (0..n).filter(|k| pins[*k].is_none()) {
        for _ in 0..32 {
            let slot = Rect::new(0.0, y, 1.0, pill);
            let Some(o) = pinned.iter().find(|o| o.inset(0.0, -gap / 2.0).intersects(&slot) && o.mid_y() <= slot.mid_y()) else { break };
            y = o.max_y() + gap;
        }
        want[k] = y;
        y += sizes[k].h + gap;
    }
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by(|&a, &b| {
        let ka = (want[a], if pins[a].is_none() { 1 } else { 0 }, a);
        let kb = (want[b], if pins[b].is_none() { 1 } else { 0 }, b);
        ka.partial_cmp(&kb).unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut out = vec![Rect::ZERO; n];
    let mut floor = f64::NEG_INFINITY;
    for &k in &order {
        let s = sizes[k];
        let mut r = Rect::new(width - margin - s.w, want[k].max(floor), s.w, s.h);
        for _ in 0..32 {
            let Some(o) = obstacles.iter().find(|o| o.inset(-gap / 2.0, -gap / 2.0).intersects(&r)) else { break };
            r.y = o.max_y() + gap;
        }
        out[k] = r;
        floor = r.max_y() + gap;
    }
    let mut ceiling = bottom;
    for &k in order.iter().rev() {
        if out[k].max_y() > ceiling {
            out[k].y = highest.max(ceiling - out[k].h);
        }
        ceiling = out[k].min_y() - gap;
    }
    out
}

/// The left edge column: `edge_column` mirrored, 8 pt in from the left edge.
#[allow(clippy::too_many_arguments)]
pub fn left_edge_column(
    sizes: &[Size],
    pins: &[Option<f64>],
    width: f64,
    top: f64,
    highest: f64,
    bottom: f64,
    margin: f64,
    gap: f64,
    obstacles: &[Rect],
) -> Vec<Rect> {
    let m = mirror(width);
    let obs: Vec<Rect> = obstacles.iter().map(&m).collect();
    edge_column(sizes, pins, width, top, highest, bottom, margin, gap, &obs).iter().map(&m).collect()
}

/// Where a released island goes, PiP style: always to the left or right edge, at the height it lands. A flick
/// (`flick_speed` or faster) is projected along the throw as a scroll view decelerates (0.998 per ms). Rects y up;
/// `top` is the island's top clamped between `highest` and the bottom of the visible area.
pub fn edge_drop(r: Rect, v: Vector, screen: Rect, highest: f64, lowest: f64, flick_speed: f64) -> (String, f64) {
    let k = 0.998 / (1.0 - 0.998) / 1000.0;
    let p = if v.length() >= flick_speed { r.offset(v.dx * k, v.dy * k) } else { r };
    let side = if p.mid_x() < screen.mid_x() { "left" } else { "right" };
    let top = p.max_y().max(lowest + r.h).min(highest);
    (side.to_owned(), top.round())
}
