//! The left-dock lists drawn as one family: Goals (its own module, `goals.rs`), To do, Scheduled, Settled (plan
//! islands) and Today (the day timeline). Each opens with one graphic: To do its share of waiting, running and
//! discussed work on the Today ribbon's 10 pt track; Scheduled the week ahead with a mark per due item; Settled the
//! week's finished work as day columns (its own module, `settled.rs`); Today the day itself. Rows share one anatomy: a lead column (tick ring, due time, day or start time), a mark (the
//! project's symbol or the block's rail) and the title with one quiet line under it. State is colour and symbol, never
//! a header: orange waits on the user, blue runs, green is decided, white is soon. An empty list shows its graphic empty
//! and its own symbol, large and dim. Nothing moves at rest.

use std::collections::HashSet;

use objc2::rc::Retained;
use objc2_app_kit::NSColor;
use objc2_quartz_core::CALayer;

use super::look::*;
use super::parts::{capitalized, Body, Control, HitRegion};
use crate::model::due::Cal;
use crate::model::geometry::Rect;
use crate::model::{time, Island, Plan, PlanItem};

/// The shared grid: the lead column's width, the mark's centre and where titles start (Scheduled, Settled, Today).
pub const LEAD_W: f64 = 50.0;
pub const MARK_CX: f64 = 58.0;
pub const TEXT_X: f64 = 72.0;
/// To do's grid: the tick ring, the project symbol and the title.
pub const TODO_RING_CX: f64 = 9.0;
pub const TODO_MARK_CX: f64 = 31.0;
pub const TODO_TEXT_X: f64 = 44.0;
/// The graphic's track, as Today's ribbon.
pub const TRACK_H: f64 = 10.0;
/// Between rows, between groups, and from the graphic to the first row.
const ROW_GAP: f64 = 10.0;
const GROUP_GAP: f64 = 16.0;
const GRAPHIC_GAP: f64 = 16.0;

pub fn waiting_color() -> Retained<NSColor> {
    orange()
}
pub fn running_color() -> Retained<NSColor> {
    rgb(0x0A84FF, 1.0)
}
pub fn discussed_color() -> Retained<NSColor> {
    rgb(0x8E8E93, 1.0)
}
pub fn settled_color() -> Retained<NSColor> {
    rgb(0x30D158, 1.0)
}

fn lead_font() -> Retained<objc2_app_kit::NSFont> {
    digits(12.0, Weight::Medium)
}
fn legend_font() -> Retained<objc2_app_kit::NSFont> {
    system(11.0, Weight::Medium)
}
fn legend_count_font() -> Retained<objc2_app_kit::NSFont> {
    digits(12.0, Weight::Semibold)
}

/// A layer holding one keyed piece of a list, placed at `y`; opaque on the island's black, so a row crossing others
/// mid-slide covers them cleanly.
pub fn keyed(y: f64, width: f64, h: f64) -> Retained<CALayer> {
    let l = layer();
    l.setAnchorPoint(pt(0.0, 0.0));
    l.setFrame(cgr(Rect::new(0.0, y, width, h)));
    set_bg(&l, &black());
    l
}

/// The dim 10 pt track every list's graphic sits on.
fn track(w: f64) -> Retained<CALayer> {
    let t = boxed(0.0, 0.0, w, TRACK_H, &white(0.09), TRACK_H / 2.0);
    t.setMasksToBounds(true);
    t
}

/// The line an item's explanation starts with, without the "Needs:" every plan item opens with: the row already
/// says it waits by its colour.
pub fn quiet_line(it: &PlanItem) -> Option<String> {
    let l = it.detail_line()?;
    let l = l.strip_prefix("Needs:").map(str::trim_start).unwrap_or(&l);
    let mut c = l.chars();
    let first = c.next()?;
    Some(first.to_uppercase().chain(c).collect())
}

/// The whole explanation an open row shows, its "Needs:" dropped the same way.
pub fn quiet_detail(d: &str) -> String {
    let d = d.trim_start();
    match d.strip_prefix("Needs:") {
        Some(rest) => {
            let rest = rest.trim_start();
            let mut c = rest.chars();
            c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
        }
        None => d.to_owned(),
    }
}

/// Wrapped text cut to at most `lines` whole lines, the last ending with an ellipsis: measured as Core Text breaks
/// it (`text_block`), so the last line is never sliced by a layer shorter than what it draws.
pub fn lines_layer(s: &str, font: &objc2_app_kit::NSFont, color: &NSColor, w: f64, lines: usize, scale: f64) -> Retained<objc2_quartz_core::CATextLayer> {
    let one = text_block("Ag", font, color, w, scale).bounds().size.height;
    let fits = |l: &objc2_quartz_core::CATextLayer| l.bounds().size.height <= one * lines as f64 + 1.0;
    let full = text_block(s, font, color, w, scale);
    if fits(&full) {
        return full;
    }
    let chars: Vec<char> = s.chars().collect();
    let cut = |n: usize| format!("{}…", chars[..n].iter().collect::<String>().trim_end());
    let (mut lo, mut hi) = (0usize, chars.len());
    while lo < hi {
        let mid = (lo + hi + 1) / 2;
        if fits(&text_block(&cut(mid), font, color, w, scale)) {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    text_block(&cut(lo), font, color, w, scale)
}

/// Whether an explanation's line only says the title again: half or more of its words (four letters or longer)
/// share a five-letter start, or the whole word, with one of the title's. "The real iPad photos." echoes "iPad sale:
/// send the photos"; "Labelling the wakes heard since Tuesday." adds to "Decider V6: label 40 wakes".
pub fn echoes(title: &str, line: &str) -> bool {
    let words = |s: &str| -> Vec<String> {
        s.split(|c: char| !c.is_alphanumeric()).filter(|w| w.chars().count() >= 4).map(|w| w.to_lowercase()).collect()
    };
    let (t, l) = (words(title), words(line));
    if l.is_empty() {
        return false;
    }
    let same = |a: &str, b: &str| {
        a == b || {
            let n = a.chars().zip(b.chars()).take_while(|(x, y)| x == y).count();
            n >= 5 || (n >= 4 && (a.len().min(b.len()) == n))
        }
    };
    let hits = l.iter().filter(|w| t.iter().any(|x| same(w, x))).count();
    hits * 2 >= l.len()
}

/// What a waiting item asks of the user: its `kind`, else read from its answers, explanation and title.
pub fn ask_kind(it: &PlanItem) -> &'static str {
    const KINDS: [&str; 9] = ["approve", "test", "pick", "send", "decide", "review", "write", "fix", "answer"];
    if let Some(k) = it.live.kind.as_deref().and_then(|k| KINDS.iter().find(|x| **x == k)) {
        return k;
    }
    let mut text = it.title.to_lowercase();
    text.push(' ');
    text.push_str(&it.detail_line().unwrap_or_default().to_lowercase());
    for (l, _) in it.proposal.iter().flat_map(|p| p.actions.iter()) {
        text.push(' ');
        text.push_str(&l.to_lowercase());
    }
    let has = |ws: &[&str]| ws.iter().any(|w| text.split(|c: char| !c.is_alphanumeric()).any(|t| t.starts_with(w)));
    if has(&["test"]) {
        "test"
    } else if has(&["send", "sent", "photo"]) {
        "send"
    } else if has(&["pick", "choose", "opinion"]) {
        "pick"
    } else if has(&["confirm", "check", "review", "look"]) {
        "review"
    } else if has(&["name", "write", "draft"]) {
        "write"
    } else if has(&["clear", "fix", "unblock"]) {
        "fix"
    } else if has(&["decide", "decision", "resume", "whether"]) {
        "decide"
    } else if has(&["yes", "update", "approve", "go"]) {
        "approve"
    } else {
        "answer"
    }
}

/// The symbol for a kind of ask.
pub fn ask_symbol(kind: &str) -> &'static str {
    match kind {
        "approve" => "checkmark",
        "test" => "play.fill",
        "pick" => "hand.point.up.left.fill",
        "send" => "paperplane.fill",
        "decide" => "arrow.triangle.branch",
        "review" => "eye.fill",
        "write" => "pencil",
        "fix" => "wrench.fill",
        _ => "questionmark",
    }
}

/// How long ago, short: "12m", "5h", "3d".
pub fn age_short(since: f64, now: f64) -> String {
    let m = ((now - since).max(0.0) / 60.0) as i64;
    if m < 60 {
        format!("{}m", m.max(1))
    } else if m < 24 * 60 {
        format!("{}h", m / 60)
    } else {
        format!("{}d", m / (24 * 60))
    }
}

/// How a due time reads in the lead column.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Tone {
    /// Due now (it stays in Scheduled, at the top): white and bold, never red or "overdue".
    Now,
    /// Within the hour: white and bold.
    Soon,
    Later,
}

/// A Scheduled item's due time, short enough for the lead column: "Now", "25 min", "3 h", a weekday within the week,
/// then "8 Nov".
pub fn due_short(due: f64, now: f64, cal: &Cal) -> (String, Tone) {
    let d = due - now;
    if d <= 0.0 {
        return ("Now".into(), Tone::Now);
    }
    let mins = (d / 60.0).ceil() as i64;
    if mins < 60 {
        return (format!("{} min", mins.max(1)), Tone::Soon);
    }
    match cal.days(now, due) {
        n if n <= 0 => (format!("{} h", ((d / 3600.0).round() as i64).max(1)), Tone::Later),
        1..=6 => (cal.weekday(due), Tone::Later),
        _ => (cal.day_month(due), Tone::Later),
    }
}

/// A legend entry: a dot, a count and a name, as Today's legend. Returns its width.
fn legend_item(root: &CALayer, x: f64, y: f64, color: &NSColor, hollow: bool, count: Option<usize>, name: &str, w: f64, scale: f64) -> f64 {
    let dot = 7.0;
    let n = text1(name, &legend_font(), &ink3(), w, scale);
    let lh = n.bounds().size.height;
    let d = if hollow { boxed(x, y + (lh - dot) / 2.0, dot, dot, &clear(), dot / 2.0) } else { boxed(x, y + (lh - dot) / 2.0, dot, dot, color, dot / 2.0) };
    if hollow {
        d.setBorderColor(Some(&cg(color)));
        d.setBorderWidth(1.2);
    }
    root.addSublayer(&d);
    let mut cx = x + dot + 5.0;
    if let Some(c) = count {
        let t = text1(&c.to_string(), &legend_count_font(), &ink(), w, scale);
        t.setPosition(pt(cx, y + (lh - t.bounds().size.height) / 2.0));
        root.addSublayer(&t);
        cx += t.bounds().size.width + 4.0;
    }
    if name.is_empty() {
        return cx - 4.0 - x;
    }
    n.setPosition(pt(cx, y));
    root.addSublayer(&n);
    cx + n.bounds().size.width - x
}

/// A small ring in the waiting colour over a dim track, its arc the share of the hour left (`left`, 0 to 1).
fn countdown_ring(left: f64, d: f64) -> Retained<CALayer> {
    use objc2_quartz_core::CAShapeLayer;
    let c = layer();
    c.setBounds(cgr(Rect::new(0.0, 0.0, d, d)));
    let r = d / 2.0 - 1.5;
    let path = super::draw::CgPath::new();
    path.move_to(d / 2.0, d / 2.0 - r);
    path.arc(d / 2.0, d / 2.0, r, -std::f64::consts::FRAC_PI_2, 1.5 * std::f64::consts::PI, false);
    for (color, end) in [(white(0.18), 1.0), (waiting_color(), left.max(0.04))] {
        let a = CAShapeLayer::new();
        no_implicit(&a);
        a.setPath(Some(path.cg()));
        a.setLineWidth(2.4);
        // SAFETY: a framework constant.
        a.setLineCap(unsafe { objc2_quartz_core::kCALineCapRound });
        a.setStrokeColor(Some(&cg(&color)));
        a.setFillColor(None);
        a.setStrokeEnd(end);
        a.setFrame(c.bounds());
        c.addSublayer(&a);
    }
    c
}

/// Who acts next on a waiting item: its `next`, else Heimdall once the user answered, else the user.
pub fn actor(it: &PlanItem) -> String {
    it.live.next.clone().unwrap_or_else(|| if it.chosen.is_some() { "heimdall".into() } else { "owner".into() })
}

pub fn actor_color(a: &str) -> Retained<NSColor> {
    match a {
        "owner" => waiting_color(),
        "heimdall" => running_color(),
        _ => discussed_color(),
    }
}

/// The first line of an explanation that has words.
fn first_line(d: &str) -> String {
    d.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("").to_owned()
}

/// A ring in `color` over a dim track, its arc `p` of the way round (0 to 1), as a job island's progress ring.
fn progress_ring(p: f64, d: f64, color: Retained<NSColor>) -> Retained<CALayer> {
    use objc2_quartz_core::CAShapeLayer;
    let c = layer();
    c.setBounds(cgr(Rect::new(0.0, 0.0, d, d)));
    let r = d / 2.0 - 1.5;
    let path = super::draw::CgPath::new();
    path.move_to(d / 2.0, d / 2.0 - r);
    path.arc(d / 2.0, d / 2.0, r, -std::f64::consts::FRAC_PI_2, 1.5 * std::f64::consts::PI, false);
    for (col, end) in [(white(0.18), 1.0), (color, p.clamp(0.03, 1.0))] {
        let a = CAShapeLayer::new();
        no_implicit(&a);
        a.setPath(Some(path.cg()));
        a.setLineWidth(2.4);
        // SAFETY: a framework constant.
        a.setLineCap(unsafe { objc2_quartz_core::kCALineCapRound });
        a.setStrokeColor(Some(&cg(&col)));
        a.setFillColor(None);
        a.setStrokeEnd(end);
        a.setFrame(c.bounds());
        c.addSublayer(&a);
    }
    c
}

pub const STRIP_H: f64 = 4.0;

/// A running item's stages as a thin ribbon (the job island band's, small): widths by seconds with a floor, the past
/// ones dim, the current (last) one lit blue.
fn stage_strip(segs: &[(String, f64)], w: f64) -> Retained<CALayer> {
    let l = layer();
    l.setAnchorPoint(pt(0.0, 0.0));
    l.setBounds(cgr(Rect::new(0.0, 0.0, w, STRIP_H)));
    let gap = 2.0;
    let n = segs.len();
    let avail = (w - gap * (n.saturating_sub(1)) as f64).max(1.0);
    let floor = (avail / n as f64 * 0.25).min(18.0);
    let total: f64 = segs.iter().map(|s| s.1).sum::<f64>().max(1.0);
    let raw: Vec<f64> = segs.iter().map(|s| (s.1 / total * avail).max(floor)).collect();
    let k = avail / raw.iter().sum::<f64>().max(1.0);
    let mut x = 0.0;
    for (i, rw) in raw.iter().enumerate() {
        let sw = rw * k;
        let c = if i + 1 == n { running_color() } else { running_color().colorWithAlphaComponent(0.35) };
        let b = boxed(x, 0.0, sw, STRIP_H, &c, STRIP_H / 2.0);
        if i + 1 == n {
            b.setShadowColor(Some(&cg(&c)));
            b.setShadowOpacity(0.8);
            b.setShadowRadius(3.0);
            b.setShadowOffset(objc2_core_foundation::CGSize { width: 0.0, height: 0.0 });
        }
        l.addSublayer(&b);
        x += sw + gap;
    }
    l
}

/// An empty list's sign: its own symbol, large and dim, centred under the empty graphic.
pub fn empty_sign(symbol: &str, root: &CALayer, y: f64, w: f64, scale: f64, body: &mut Body) -> f64 {
    let h = 64.0;
    let l = keyed(y, w, h);
    let s = symbol_layer(symbol, 30.0, &[white(0.2)], Weight::Regular, scale);
    s.setPosition(pt(w / 2.0, h / 2.0));
    l.addSublayer(&s);
    root.addSublayer(&l);
    body.rows.insert("empty".into(), (l, format!("empty\u{1}{symbol}")));
    y + h
}

// MARK: Graphics

/// To do: one track split into running (blue), waiting (orange) and discussed (grey) by count, in the rows' order,
/// then a dot and count for each.
fn todo_graphic(items: &[PlanItem], root: &CALayer, y0: f64, w: f64, scale: f64, body: &mut Body) -> f64 {
    let open: Vec<&PlanItem> = items.iter().filter(|i| !i.done).collect();
    let count = |g: &str| open.iter().filter(|i| i.group.as_deref().unwrap_or("discussed") == g).count();
    let groups = [("running", "Running", running_color()), ("waiting", "Waiting on you", waiting_color()), ("discussed", "Discussed", discussed_color())];
    let counts: Vec<usize> = groups.iter().map(|g| count(g.0)).collect();
    let total: usize = counts.iter().sum();
    let g = keyed(y0, w, 0.0);
    let t = track(w);
    let gap = 2.0;
    let shown = counts.iter().filter(|c| **c > 0).count();
    let avail = w - gap * (shown.max(1) - 1) as f64;
    let mut x = 0.0;
    for (k, c) in counts.iter().enumerate() {
        if *c == 0 {
            continue;
        }
        let sw = avail * *c as f64 / total as f64;
        t.addSublayer(&boxed(x, 0.0, sw, TRACK_H, &groups[k].2, 0.0));
        x += sw + gap;
    }
    g.addSublayer(&t);
    let mut h = TRACK_H;
    if total > 0 {
        let ly = TRACK_H + 8.0;
        let mut lx = 0.0;
        let mut lh: f64 = 0.0;
        for (k, c) in counts.iter().enumerate() {
            if *c == 0 {
                continue;
            }
            lx += legend_item(&g, lx, ly, &groups[k].2, false, Some(*c), "", w, scale) + 12.0;
            lh = lh.max(line_height(&legend_count_font()));
        }
        h = ly + lh;
    }
    g.setBounds(cgr(Rect::new(0.0, 0.0, w, h)));
    root.addSublayer(&g);
    body.rows.insert("graphic".into(), (g, format!("todo\u{1}{counts:?}")));
    y0 + h
}

/// Where a moment falls on Scheduled's week track: the rest of today takes the first third, tomorrow the next fifth,
/// the five days after the rest. Later than that sits at the end.
pub fn week_x(t: f64, now: f64, cal: &Cal, w: f64) -> f64 {
    let today_end = cal.add_days(1, cal.start_of_day(now)).unwrap_or(now + 86400.0);
    let tomorrow_end = cal.add_days(1, today_end).unwrap_or(today_end + 86400.0);
    let week_end = cal.add_days(5, tomorrow_end).unwrap_or(tomorrow_end + 5.0 * 86400.0);
    let (a, b) = (0.34 * w, 0.54 * w);
    let f = |t: f64, s: f64, e: f64| ((t - s) / (e - s).max(1.0)).clamp(0.0, 1.0);
    if t <= today_end {
        a * f(t, now, today_end)
    } else if t <= tomorrow_end {
        a + (b - a) * f(t, today_end, tomorrow_end)
    } else {
        b + (w - b) * f(t, tomorrow_end, week_end)
    }
}

// MARK: Rows

/// Proposal buttons sized to their labels: equal widths when the longest fits that way, else each to its label,
/// shrunk together to the room there is.
pub fn button_widths(natural: &[f64], avail: f64, gap: f64) -> Vec<f64> {
    if natural.is_empty() {
        return vec![];
    }
    let n = natural.len() as f64;
    let room = avail - gap * (n - 1.0);
    let equal = room / n;
    if natural.iter().all(|w| *w <= equal) {
        return vec![equal; natural.len()];
    }
    let sum: f64 = natural.iter().sum();
    if sum <= room {
        // The spare room goes to every button alike.
        let extra = (room - sum) / n;
        return natural.iter().map(|w| w + extra).collect();
    }
    natural.iter().map(|w| w * room / sum).collect()
}

/// The plan lists. To do: the share graphic, then running, waiting and discussed rows (`Plan::display_order`), the
/// groups set apart by a hairline, each row's state in its ring: dashed blue runs, lit orange waits on the user, grey was
/// discussed. Scheduled: the week graphic, then rows with their due time in the lead column (said once for a run of the
/// same; due within the hour, orange with a draining ring). Settled draws itself (`settled.rs`). A row is its lead,
/// the project's symbol and the title (two lines at most) with the first line of its explanation under it when that adds to the title; an answered To do row shows the answer on its right. Only
/// To do's ring ticks; a click on the rest of a row with more to read opens it: the title wraps, the explanation
/// follows in full, then the proposal's buttons.
#[allow(clippy::too_many_arguments)]
pub fn build_plan(island: &Island, root: &CALayer, y0: f64, width: f64, scale: f64, open_items: &HashSet<String>, body: &mut Body, cal: &Cal) -> f64 {
    let list = island.list().unwrap_or_else(|| "todo".into());
    if list == "goals" {
        return super::goals::build(island, root, y0, width, scale, open_items, body);
    }
    let all = island.plan_items();
    if list == "settled" {
        return super::settled::build(&island.raw, time::now(), cal, root, y0, width, scale, open_items, body);
    }
    if list == "scheduled" {
        return super::scheduled::build(island, root, y0, width, scale, open_items, body, cal);
    }
    // Drawn in `Plan::display_order`: the row diff (`ItemDiff`) and the CLI's numbers follow it.
    let groups = Plan::grouped(&all, &list);
    let now = time::now();
    let mut y = todo_graphic(&all, root, y0, width, scale, body);
    if groups.is_empty() {
        let symbol = crate::model::PlanList::icon(&list);
        return empty_sign(symbol, root, y + 8.0, width, scale, body);
    }
    y += GRAPHIC_GAP;
    let todo = list == "todo";
    let (mark_cx, text_x) = if todo { (TODO_MARK_CX, TODO_TEXT_X) } else { (MARK_CX, TEXT_X) };
    let mut last_lead: Option<String> = None;
    let mut first = true;
    for (gk, (group, items)) in groups.iter().enumerate() {
        if gk > 0 {
            // A hairline between bands, across the text column.
            let hl = keyed(y + (GROUP_GAP - 1.0) / 2.0 + ROW_GAP / 2.0, width, 1.0);
            hl.addSublayer(&boxed(text_x, 0.0, width - text_x, 1.0, &white(0.1), 0.0));
            root.addSublayer(&hl);
            body.rows.insert(format!("hdr:{group}"), (hl, group.clone()));
            y += GROUP_GAP;
        }
        for it in items.iter().take(500) {
            if !first {
                y += ROW_GAP;
            }
            first = false;
            let open = it.discloses() && open_items.contains(&it.key);
            let asks = it.proposal.is_some() && !it.done;
            let state = if it.done && todo { "done" } else if todo { group.as_str() } else { list.as_str() };
            let state_color = match state {
                "waiting" => waiting_color(),
                "running" => running_color(),
                "settled" => settled_color(),
                _ => discussed_color(),
            };

            // The lead: To do's tick ring; Scheduled's due time; Settled's day. Said once for a run of the same.
            let (lead_text, tone) = match list.as_str() {
                "scheduled" => match it.due.filter(|_| !it.done) {
                    Some(d) => {
                        let (s, t) = due_short(d, now, cal);
                        (Some(s), t)
                    }
                    None => (Some(it.when.clone().map(|w| capitalized(&w)).unwrap_or_else(|| "Some day".into())), Tone::Later),
                },
                _ => (None, Tone::Later),
            };
            let lead_shown = lead_text.clone().filter(|l| last_lead.as_ref() != Some(l) || matches!(tone, Tone::Now | Tone::Soon));
            if lead_text.is_some() {
                last_lead = lead_text.clone();
            }

            // Answered and still here: the chosen answer, quiet, on the right (To do; the other lists take no answers).
            let mut trailing: Option<(Retained<CALayer>, f64)> = None;
            if todo && !open && !it.done && it.chosen.is_none() && state == "waiting" {
                // How long it has waited: quiet, orange once it is two days old.
                if let Some(a) = it.live.asked {
                    let old = now - a >= 2.0 * 86400.0;
                    let t = text1(&age_short(a, now), &digits(12.0, Weight::Medium), &*if old { waiting_color() } else { ink3() }, 60.0, scale);
                    let tw = t.bounds().size.width;
                    trailing = Some((Retained::into_super(t), tw));
                }
            }
            if todo && !open && !it.done {
                if let Some(c) = &it.chosen {
                    let label = it.proposal.as_ref().and_then(|p| p.actions.iter().find(|a| &a.1 == c).map(|a| a.0.clone())).unwrap_or_else(|| capitalized(c));
                    let t = text1(&label, &small_font(), &ink3(), 90.0, scale);
                    let tw = t.bounds().size.width;
                    trailing = Some((Retained::into_super(t), tw));
                }
            }
            let trailing_w = trailing.as_ref().map(|t| t.1 + 10.0).unwrap_or(0.0);

            let title_c = if it.done && todo {
                ink_done()
            } else if state == "discussed" {
                ink2()
            } else {
                ink()
            };
            let tw = width - text_x - trailing_w;
            let t = if it.done && todo {
                text_layer(&it.title, &body_strong(), &title_c, tw, 1, Align::Left, true, scale)
            } else {
                lines_layer(&it.title, &body_strong(), &title_c, tw, if open { 8 } else { 2 }, scale)
            };
            let th = t.bounds().size.height;
            // Open: the explanation's first line; the rest waits behind a "more" mark (`more:<key>`).
            let more_key = format!("more:{}", it.key);
            let full = it.detail.as_deref().map(quiet_detail);
            let has_more = full.as_ref().is_some_and(|d| d.lines().filter(|l| !l.trim().is_empty()).count() > 1 || d.chars().count() > 90);
            let more_open = open_items.contains(&more_key);
            let shown_detail = full.as_ref().filter(|_| open).map(|d| if more_open || !has_more { d.clone() } else { first_line(d) });
            let more_w = if open && has_more { 26.0 } else { 0.0 };
            let detail = shown_detail.as_ref().map(|d| text_block(d, &note_font(), &ink2(), width - text_x - more_w, scale));
            // Settled names the decision in its title; elsewhere the line says what the title does not.
            let running_live = state == "running" && (it.live.stage.is_some() || !it.live.segments.is_empty());
            let subtitle_text = if open || (it.done && todo) || list == "settled" {
                None
            } else if running_live {
                // Running: its stage and how long it has run, in the running colour.
                let stage = it.live.stage.clone().or_else(|| it.live.segments.last().map(|s| s.0.clone())).unwrap_or_default();
                Some(match it.live.started {
                    Some(st) => format!("{stage}  {}", super::parts::band_len_short(now - st)),
                    None => stage,
                })
            } else {
                quiet_line(it).filter(|l| !echoes(&it.title, l))
            };
            let sub_c = if running_live { running_color() } else { ink2() };
            let subtitle = subtitle_text.as_ref().map(|s| text_layer(s, &note_font(), &sub_c, width - text_x, 1, Align::Left, false, scale));
            // Running with stages: a thin ribbon of them under the line, the current one lit.
            let strip = (state == "running" && !open && it.live.segments.len() > 1).then(|| stage_strip(&it.live.segments, width - text_x));
            let mut row_h = th;
            if let Some(s) = &subtitle {
                row_h += 2.0 + s.bounds().size.height;
            }
            if let Some(d) = &detail {
                row_h += 4.0 + d.bounds().size.height;
            }
            if strip.is_some() {
                row_h += 6.0 + STRIP_H;
            }
            let line_mid = th.min(line_height(&body_strong())) / 2.0;

            let symbol = Some(it.symbol(&list)).filter(|s| symbol_exists(s)).unwrap_or_else(|| PlanItem { icon: None, ..it.clone() }.symbol(&list));
            let row = keyed(y, width, row_h);
            if todo {
                let (name, colors): (&str, Vec<Retained<NSColor>>) = match state {
                    "done" => ("checkmark.circle.fill", vec![white(1.0), settled_color()]),
                    "running" => ("circle.dashed", vec![state_color.clone()]),
                    // A decision to make: the ring holds a dot.
                    "waiting" => ("circle.inset.filled", vec![state_color.clone()]),
                    _ => ("circle", vec![state_color.clone()]),
                };
                let ring: Retained<CALayer> = match (state, it.live.progress) {
                    // Running with a known share done: a blue arc over a dim track, as job islands draw it.
                    ("running", Some(p)) => progress_ring(p, 16.0, running_color()),
                    _ => sym(name, 17.0, &colors, scale),
                };
                ring.setPosition(pt(TODO_RING_CX, line_mid));
                row.addSublayer(&ring);
            } else if let Some(l) = &lead_shown {
                let (c, f) = match tone {
                    Tone::Now => (ink(), digits(12.0, Weight::Bold)),
                    Tone::Soon => (waiting_color(), digits(12.0, Weight::Semibold)),
                    Tone::Later => (if l == "Today" { ink() } else { ink2() }, lead_font()),
                };
                let lt = text1(l, &f, &c, LEAD_W - 2.0, scale);
                lt.setPosition(pt(0.0, line_mid - lt.bounds().size.height / 2.0));
                row.addSublayer(&lt);
            }
            let mark_c = match state {
                "settled" => settled_color(),
                _ if tone == Tone::Now => ink(),
                _ if tone == Tone::Soon => waiting_color(),
                _ => ink3(),
            };
            // Due within the hour: the mark is an orange ring that drains as the minutes run out. A waiting To do row's
            // mark is what it asks (approve, test, pick, send…) in the colour of who acts next: orange the user, blue
            // Heimdall (once he answered), grey anyone else.
            let next = actor(it);
            let mark = match (list.as_str(), tone, it.due) {
                ("todo", _, _) if state == "waiting" => symbol_layer(ask_symbol(ask_kind(it)), 12.0, &[actor_color(&next)], Weight::Semibold, scale),
                ("scheduled", Tone::Soon, Some(d)) if !it.done => countdown_ring(((d - now) / 3600.0).clamp(0.0, 1.0), 14.0),
                _ => symbol_layer(&symbol, 13.0, &[mark_c], Weight::Medium, scale),
            };
            mark.setPosition(pt(mark_cx, line_mid));
            row.addSublayer(&mark);
            t.setPosition(pt(text_x, 0.0));
            row.addSublayer(&t);
            let mut ty = th;
            if let Some(s) = &subtitle {
                s.setPosition(pt(text_x, ty + 2.0));
                row.addSublayer(s);
                ty += 2.0 + s.bounds().size.height;
            }
            if let Some(st) = &strip {
                st.setPosition(pt(text_x, ty + 6.0));
                row.addSublayer(st);
            }
            let mut more_rect = None;
            if let Some(d) = &detail {
                d.setPosition(pt(text_x, ty + 4.0));
                row.addSublayer(d);
                if more_w > 0.0 {
                    let m = symbol_layer(if more_open { "chevron.up.circle" } else { "ellipsis.circle" }, 15.0, &[ink2()], Weight::Regular, scale);
                    let lh = line_height(&note_font());
                    m.setPosition(pt(width - 9.0, ty + 4.0 + lh / 2.0));
                    row.addSublayer(&m);
                    more_rect = Some(Rect::new(width - more_w - 4.0, y + ty, more_w + 4.0, lh + 8.0));
                }
            }
            if let Some((tr, tw)) = &trailing {
                let h = tr.bounds().size.height;
                tr.setPosition(pt(width - tw, line_mid - h / 2.0));
                row.addSublayer(tr);
            }
            root.addSublayer(&row);
            let sig = [
                it.title.clone(),
                state.to_owned(),
                lead_shown.clone().unwrap_or_default(),
                format!("{tone:?}"),
                if open { it.detail.clone().unwrap_or_default() } else { subtitle_text.clone().unwrap_or_default() },
                it.chosen.clone().unwrap_or_default(),
                if open { "o" } else { "" }.into(),
                if asks { "p" } else { "" }.into(),
                symbol.clone(),
                if more_open { "m" } else { "" }.into(),
                format!("{:?}{:?}{}", it.live.progress, it.live.segments, next),
            ]
            .join("\u{1}");
            body.rows.insert(format!("row:{}", it.key), (row, sig));
            // Only the ring ticks, so a click on a title never ticks by surprise; the rest of the row opens and closes
            // it (first region wins).
            if todo {
                body.regions.push(HitRegion { rect: Rect::new(0.0, y - 4.0, 22.0, th + 8.0), control: Control::Tick { key: it.key.clone(), title: it.title.clone() } });
            }
            if let Some(r) = more_rect {
                body.regions.push(HitRegion { rect: r, control: Control::Disclose { key: more_key.clone() } });
            }
            if it.discloses() {
                body.regions.push(HitRegion { rect: Rect::new(0.0, y, width, row_h), control: Control::Disclose { key: it.key.clone() } });
            }
            y += row_h;
            if let (true, Some(p)) = (open && asks, &it.proposal) {
                y += 8.0;
                y = proposal_block(it, p, root, y, width, text_x, scale, body);
            }
        }
    }
    y
}

/// An open proposal under its row: its text, then its buttons sized to their labels, the first one white.
#[allow(clippy::too_many_arguments)]
fn proposal_block(it: &PlanItem, p: &crate::model::Proposal, root: &CALayer, y: f64, width: f64, text_x: f64, scale: f64, body: &mut Body) -> f64 {
    let (inset, x0) = (10.0, text_x - 10.0);
    let text = (!p.text.is_empty()).then(|| text_block(&p.text, &small_font(), &ink(), width - x0 - 2.0 * inset, scale));
    let (bh, gap) = (32.0, 8.0);
    let block_h = inset + text.as_ref().map(|t| t.bounds().size.height + 10.0).unwrap_or(0.0) + bh + inset;
    let block = keyed(y, width, block_h);
    block.addSublayer(&boxed(x0, 0.0, width - x0, block_h, &white(0.08), 14.0));
    let mut by = inset;
    if let Some(t) = &text {
        t.setPosition(pt(x0 + inset, by));
        block.addSublayer(t);
        by += t.bounds().size.height + 10.0;
    }
    let acts: Vec<&(String, String)> = p.actions.iter().take(3).collect();
    let natural: Vec<f64> = acts.iter().map(|a| text_width(&a.0, &button_font()) + 28.0 + if it.chosen.as_deref() == Some(a.1.as_str()) { 18.0 } else { 0.0 }).collect();
    let widths = button_widths(&natural, width - x0 - 2.0 * inset, gap);
    let mut x = x0 + inset;
    for (k, (label, action)) in acts.iter().enumerate() {
        let bw = widths[k];
        let picked = it.chosen.as_deref() == Some(action.as_str());
        let primary = if it.chosen.is_none() { k == 0 } else { picked };
        let fill = if primary { white(1.0) } else { white(if it.chosen.is_none() { 0.16 } else { 0.1 }) };
        block.addSublayer(&boxed(x, by, bw, bh, &fill, bh / 2.0));
        let lc = if primary {
            black()
        } else if it.chosen.is_none() {
            white(1.0)
        } else {
            ink2()
        };
        let lt = text1(label, &button_font(), &lc, bw - 12.0, scale);
        let mut lx = x + (bw - lt.bounds().size.width) / 2.0;
        if picked {
            let ck = symbol_layer("checkmark", 12.0, &[black()], Weight::Bold, scale);
            lx += 9.0;
            ck.setPosition(pt(lx - 11.0, by + bh / 2.0));
            block.addSublayer(&ck);
        }
        lt.setPosition(pt(lx, by + (bh - lt.bounds().size.height) / 2.0));
        block.addSublayer(&lt);
        if it.chosen.is_none() {
            body.regions.push(HitRegion { rect: Rect::new(x, y + by, bw, bh), control: Control::Approve { key: it.key.clone(), action: action.clone() } });
        }
        x += bw + gap;
    }
    root.addSublayer(&block);
    body.rows.insert(format!("prop:{}", it.key), (block, [p.text.clone(), it.chosen.clone().unwrap_or_default()].join("\u{1}")));
    y + block_h + 4.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::due::Cal;

    fn item(title: &str, detail: Option<&str>) -> PlanItem {
        PlanItem { index: 0, key: title.into(), id: None, title: title.into(), group: None, done: false, when: None, due: None, detail: detail.map(str::to_owned), proposal: None, chosen: None, icon: None, live: Default::default(), at: None, goal: None }
    }

    #[test]
    fn the_needs_prefix_is_dropped_from_the_quiet_line_and_the_open_detail() {
        assert_eq!(quiet_line(&item("a", Some("Needs: the real iPad photos.\nFront and back."))).as_deref(), Some("The real iPad photos."));
        assert_eq!(quiet_line(&item("a", Some("Labelling the wakes."))).as_deref(), Some("Labelling the wakes."));
        assert_eq!(quiet_detail("Needs: a yes.\nNothing updates."), "A yes.\nNothing updates.");
        assert_eq!(quiet_detail("Plain."), "Plain.");
        assert_eq!(quiet_line(&item("a", Some("Needs:"))), None);
    }

    #[test]
    fn an_empty_plan_list_shows_no_status_lines() {
        // Bug (30 Oct): an empty To do, Scheduled or Settled showed "Note / Added just now" over its empty state.
        for l in ["todo", "scheduled", "settled"] {
            let i = Island::new(l, crate::jobj! {"preset": "todo", "list": l, "items": []});
            let types: Vec<String> = super::super::parts::body_parts(&i).iter().map(|p| p["type"].as_str().unwrap().to_owned()).collect();
            assert_eq!(types, ["todo"], "{l}");
        }
    }

    #[test]
    fn a_title_cut_at_two_lines_is_as_tall_as_the_lines_it_draws() {
        // Bug (30 Oct): a two-line title's layer was shorter than what it drew, slicing the second line in half.
        let f = body_strong();
        let one = text_block("Ag", &f, &ink(), 250.0, 2.0).bounds().size.height;
        let long = "Heimdall: the public alpha build needs your review before it can be signed and shared with anyone";
        let t = lines_layer(long, &f, &ink(), 250.0, 2, 2.0);
        let shown = text_of(&t).unwrap();
        assert!(shown.ends_with('…') && shown.len() < long.len(), "{shown}");
        assert!((t.bounds().size.height - 2.0 * one).abs() <= 1.0, "{} vs {one}", t.bounds().size.height);
        let short = lines_layer("Ego Lite: update", &f, &ink(), 250.0, 2, 2.0);
        assert_eq!(text_of(&short).as_deref(), Some("Ego Lite: update"));
        assert!((short.bounds().size.height - one).abs() <= 1.0);
    }

    #[test]
    fn a_waiting_item_shows_its_kind_of_ask_its_age_and_who_acts_next() {
        let mut it = item("Vella: test the iPhone build", Some("Needs: press start and stop to test."));
        assert_eq!(ask_kind(&it), "test");
        it.live.kind = Some("pick".into());
        assert_eq!(ask_kind(&it), "pick");
        it.live.kind = Some("nonsense".into());
        assert_eq!(ask_kind(&it), "test");
        assert_eq!(ask_kind(&item("iPad sale: send the photos", None)), "send");
        assert_eq!(ask_kind(&item("Something odd", None)), "answer");
        assert_eq!(actor(&it), "owner");
        it.chosen = Some("later".into());
        assert_eq!(actor(&it), "heimdall");
        it.live.next = Some("ania".into());
        assert_eq!(actor(&it), "ania");
        assert_eq!((age_short(1000.0, 1000.0 + 90.0), age_short(0.0, 5.0 * 3600.0), age_short(0.0, 3.0 * 86400.0)), ("1m".into(), "5h".into(), "3d".into()));
    }

    #[test]
    fn the_live_fields_parse_when_present_and_stay_empty_otherwise() {
        let o = crate::jobj! {"title": "a", "kind": "Approve", "asked": 100, "next": "Owner", "stage": "Building", "progress": 1.7, "started": "50",
            "segments": [{"title": "Testing", "secs": 30}, {"secs": 4}, {"title": "Building", "secs": -3}]};
        let l = crate::model::ItemLive::parse(&o);
        assert_eq!(l.kind.as_deref(), Some("approve"));
        assert_eq!((l.asked, l.started, l.progress), (Some(100.0), Some(50.0), Some(1.0)));
        assert_eq!(l.next.as_deref(), Some("owner"));
        assert_eq!(l.segments, vec![("Testing".to_owned(), 30.0), ("Building".to_owned(), 0.0)]);
        assert_eq!(crate::model::ItemLive::parse(&crate::jobj! {"title": "a"}), crate::model::ItemLive::default());
    }

    #[test]
    fn a_line_that_only_repeats_the_title_is_dropped() {
        assert!(echoes("iPad sale: send the photos", "The real iPad photos."));
        assert!(echoes("Ego Lite: update to 0.6.1.9", "A yes on updating Ego Lite."));
        assert!(echoes("Arsenal: pick niche models", "Your picks on the niche-models page."));
        assert!(!echoes("Decider V6: label 40 wakes", "Labelling the wakes heard since Tuesday."));
        assert!(!echoes("Arsenal: Homebrew tap", "Nothing until the repo is public."));
        assert!(!echoes("Anything", "A b."));
    }

    #[test]
    fn due_times_are_short_enough_for_the_lead_column() {
        let cal = Cal::fixed("Europe/Warsaw", "en_GB");
        let now = cal.local(2026, 10, 30, 8, 0, 0).unwrap();
        assert_eq!(due_short(now - 5.0, now, &cal), ("Now".into(), Tone::Now));
        assert_eq!(due_short(now + 25.0 * 60.0, now, &cal), ("25 min".into(), Tone::Soon));
        assert_eq!(due_short(now + 3.0 * 3600.0, now, &cal).0, "3 h");
        assert_eq!(due_short(cal.local(2026, 10, 31, 10, 0, 0).unwrap(), now, &cal).0, "Sat");
        assert_eq!(due_short(cal.local(2026, 11, 8, 10, 0, 0).unwrap(), now, &cal).0, "8 Nov");
    }

    #[test]
    fn the_week_track_puts_today_first_and_clamps_the_far_future() {
        let cal = Cal::fixed("Europe/Warsaw", "en_GB");
        let now = cal.local(2026, 10, 30, 8, 0, 0).unwrap();
        let w = 300.0;
        assert_eq!(week_x(now, now, &cal, w), 0.0);
        let later_today = week_x(now + 6.0 * 3600.0, now, &cal, w);
        assert!(later_today > 0.0 && later_today < 0.34 * w);
        let tomorrow = week_x(cal.local(2026, 10, 31, 12, 0, 0).unwrap(), now, &cal, w);
        assert!(tomorrow > 0.34 * w && tomorrow < 0.54 * w);
        assert_eq!(week_x(now + 60.0 * 86400.0, now, &cal, w), w);
    }

    #[test]
    fn buttons_fit_their_labels_and_the_room() {
        assert_eq!(button_widths(&[60.0, 60.0], 300.0, 8.0), vec![146.0, 146.0]);
        let w = button_widths(&[140.0, 90.0, 70.0], 270.0, 8.0);
        assert!((w.iter().sum::<f64>() + 16.0 - 270.0).abs() < 1e-9);
        assert!(w[0] > w[1] && w[1] > w[2], "{w:?}");
        let w = button_widths(&[120.0, 60.0, 60.0], 300.0, 8.0);
        assert!(w[0] >= 120.0 && (w.iter().sum::<f64>() + 16.0 - 300.0).abs() < 1e-9, "{w:?}");
        assert!(button_widths(&[], 100.0, 8.0).is_empty());
    }
}
