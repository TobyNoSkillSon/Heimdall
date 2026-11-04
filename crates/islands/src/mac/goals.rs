//! Goals, open ("goals are self-motivating"; 2 Nov, the one system: `docs/one-system.md`, the approved
//! design `Assistant/Plans/goals/one-system.html`). Per active goal: a progress ring round its symbol (green the share
//! done, blue the share running, dim the rest, a soft green glow), its title, a segmented bar (green done, blue one per
//! running job island of this goal, dim queued, a thin outlined segment for its front step proposal), the latest move in
//! grey, "N running on the right" in blue while job islands of it run, the queued tasks as grey-dot lines (three at
//! most), and the front step proposal as a ghost row ("Prepare CV drafts  [Yes] +2"). A front goal proposal is a ghost
//! goal after the goals. A ghost's title opens its `why`; Yes answers in one click. Asks are not drawn here (a real
//! block shows on Scheduled's guarantee line) and finished tasks are Settled's. Nothing moves at rest.

use std::collections::HashSet;

use objc2::rc::Retained;
use objc2_app_kit::NSFont;
use objc2_quartz_core::CALayer;

use super::lists::{empty_sign, keyed};
use super::look::*;
use super::marks::*;
use super::parts::{Body, Control, HitRegion};
use super::world::Ctx;
use crate::model::geometry::Rect;
use crate::model::onesys::{self, Proposal, Seg};
use crate::model::{time, Goal, GoalState, Goals, Island};

/// The ring's diameter, where it sits (2 pt out to the left, 4 pt above the title), and where text starts.
pub const RING: f64 = 44.0;
const RING_X: f64 = -2.0;
const RING_Y: f64 = -4.0;
pub const GOAL_TEXT_X: f64 = 54.0;
/// A goal's padding above and below.
const PAD_TOP: f64 = 5.0;
const PAD_BOTTOM: f64 = 8.0;
pub const BAR_H: f64 = 6.0;
const BAR_GAP: f64 = 3.0;
/// Past this many segments the bar is drawn as spans, one per kind.
const SEGMENTS: usize = 24;
/// Queued lines shown under a goal.
const QUEUED: usize = 3;

fn title_font() -> Retained<NSFont> {
    system(16.0, Weight::Semibold)
}
fn now_font() -> Retained<NSFont> {
    system(13.5, Weight::Regular)
}
fn line_font() -> Retained<NSFont> {
    system(14.5, Weight::Medium)
}

/// Text cut to `lines`, placed so each line sits in a `line_h` box (CSS line-height).
fn boxed_lines(s: &str, f: &NSFont, c: &objc2_app_kit::NSColor, w: f64, lines: usize, line_h: f64, scale: f64) -> (Retained<CALayer>, f64, f64) {
    let (t, h) = pitched(s, f, c, w, lines, line_h, scale);
    (t, 0.0, h)
}

/// The open Goals list.
pub fn build(island: &Island, root: &CALayer, y0: f64, width: f64, scale: f64, open_items: &HashSet<String>, body: &mut Body) -> f64 {
    let ctx = Ctx::current();
    let now = time::now();
    let goals: Vec<Goal> = Goals::parse(&island.raw).into_iter().filter(|g| g.state == GoalState::Active).collect();
    let front = onesys::front(&island.raw, now);
    if goals.is_empty() && front.is_none() {
        return empty_sign("flag", root, y0 + 8.0, width, scale, body);
    }
    let id_of = |g: &Goal| g.id.clone().unwrap_or_else(|| g.key.clone());
    let mut placed = false;
    let mut y = y0;
    for g in &goals {
        let gid = id_of(g);
        let step = front.as_ref().filter(|(p, _)| !p.is_goal() && p.goal.as_deref() == Some(gid.as_str()));
        placed |= step.is_some();
        let running = ctx.running.get(&gid).copied().unwrap_or(0);
        y = goal_block(g, running, step, root, y, width, scale, open_items, body);
    }
    // A goal proposal (or a step whose goal is not among the active ones): a ghost goal after the goals.
    if let Some(f) = front.as_ref().filter(|_| !placed) {
        y = ghost_goal(f, root, y, width, scale, open_items, body);
    }
    y
}

/// The ring: the shares done and running round the goal's symbol on a dark disc, a soft green glow under it.
fn ring(symbol: &str, done: f64, run: f64, scale: f64) -> Retained<CALayer> {
    let r = boxed(RING_X, RING_Y, RING, RING, &track_fill(), RING / 2.0);
    glow(&r, &proof_green(), 6.0, 0.45);
    let rr = (RING - 7.0) / 2.0;
    let arc = |from: f64, to: f64, c: &objc2_app_kit::NSColor| {
        if to - from <= 1e-6 {
            return;
        }
        let p = super::draw::CgPath::new();
        let a0 = -std::f64::consts::FRAC_PI_2 + from * std::f64::consts::TAU;
        let a1 = -std::f64::consts::FRAC_PI_2 + to * std::f64::consts::TAU;
        p.move_to(RING / 2.0 + rr * a0.cos(), RING / 2.0 + rr * a0.sin());
        p.arc(RING / 2.0, RING / 2.0, rr, a0, a1, false);
        r.addSublayer(&stroked(&p, 0.0, 0.0, RING, RING, c, 7.0, None));
    };
    arc(0.0, done.min(1.0), &proof_green());
    arc(done, (done + run).min(1.0), &run_blue());
    let inner = boxed(7.0, 7.0, RING - 14.0, RING - 14.0, &rgb(0x141416, 1.0), (RING - 14.0) / 2.0);
    r.addSublayer(&inner);
    let s = symbol_layer(symbol, 15.0, &[white(1.0)], Weight::Medium, scale);
    s.setPosition(pt(RING / 2.0, RING / 2.0));
    r.addSublayer(&s);
    r
}

/// The bar: one segment per piece of work, equal widths, 3 pt apart; spans per kind past `SEGMENTS`.
fn bar(segs: &[Seg], x0: f64, y: f64, w: f64, into: &CALayer) {
    let color = |s: Seg| match s {
        Seg::Done => rgb(0x34E065, 1.0),
        Seg::Running => rgb(0x2E95FF, 1.0),
        _ => track_fill(),
    };
    let draw = |x: f64, sw: f64, s: Seg| {
        if s == Seg::Proposed {
            // The design's proposed segment: a thin outline, no fill (`.bar .p`).
            let l = boxed(x, y, sw, BAR_H, &clear(), BAR_H / 2.0);
            circular(&l);
            l.setBorderColor(Some(&cg(&white(0.45))));
            l.setBorderWidth(1.0);
            into.addSublayer(&l);
            return;
        }
        let l = boxed(x, y, sw, BAR_H, &color(s), BAR_H / 2.0);
        match s {
            Seg::Done => glow(&l, &proof_green(), 3.0, 0.7),
            Seg::Running => glow(&l, &run_blue(), 4.0, 0.9),
            _ => {}
        }
        into.addSublayer(&l);
    };
    if segs.len() <= SEGMENTS {
        let n = segs.len() as f64;
        let sw = (w - BAR_GAP * (n - 1.0)) / n;
        for (k, s) in segs.iter().enumerate() {
            draw(x0 + k as f64 * (sw + BAR_GAP), sw, *s);
        }
        return;
    }
    // Many pieces: one span per kind, by count.
    let kinds = [Seg::Done, Seg::Running, Seg::Queued, Seg::Proposed];
    let counts: Vec<usize> = kinds.iter().map(|k| segs.iter().filter(|s| *s == k).count()).collect();
    let shown = counts.iter().filter(|c| **c > 0).count() as f64;
    let avail = w - BAR_GAP * (shown - 1.0);
    let mut x = x0;
    for (k, c) in counts.iter().enumerate() {
        if *c == 0 {
            continue;
        }
        let sw = (avail * *c as f64 / segs.len() as f64).max(BAR_H);
        draw(x, sw, kinds[k]);
        x += sw + BAR_GAP;
    }
}

/// A proposal's ghost row at (x, y), `w` wide: open dot, title at 75 % white, Yes, "+N"; its `why` under it once its
/// title was clicked. Keyed `key` so a yes fills it in where it sits. Returns its bottom.
#[allow(clippy::too_many_arguments)]
fn ghost_row(p: &Proposal, plus_n: usize, key: &str, root: &CALayer, x: f64, y: f64, w: f64, scale: f64, open_items: &HashSet<String>, body: &mut Body) -> (Retained<CALayer>, f64) {
    let answered = open_items.contains(&format!("yes:{}", p.id));
    let why_open = p.why.is_some() && open_items.contains(&format!("why:{}", p.id));
    let row = keyed(y, x + w, 0.0);
    // Clear: a ghost goal's ring shows beside it.
    set_bg(&row, &clear());
    let (yes, yw) = yes_chip(answered, scale);
    let pl = plus(plus_n, scale);
    let pw = pl.as_ref().map(|l| l.bounds().size.width + 9.0).unwrap_or(0.0);
    let right = x + w - 6.0;
    let tx = x + 8.0 + 7.0 + 9.0;
    let tw = right - pw - yw - 9.0 - tx;
    let (t, off, th) = boxed_lines(&p.title, &line_font(), &white(0.75), tw, 2, 20.0, scale);
    let inner = th.max(YES_H);
    let h = inner + 8.0;
    row.addSublayer(&ghost_box(x, 0.0, w, h));
    row.addSublayer(&open_dot(x + 8.0 + 3.5, h / 2.0, 7.0, &white(0.6)));
    t.setPosition(pt(tx, 4.0 + (inner - th) / 2.0 + off));
    row.addSublayer(&t);
    let yx = right - pw - yw;
    yes.setPosition(pt(yx, (h - YES_H) / 2.0));
    row.addSublayer(&yes);
    if let Some(l) = &pl {
        l.setPosition(pt(right - l.bounds().size.width - 2.0, (h - l.bounds().size.height) / 2.0));
        row.addSublayer(l);
    }
    let mut total = h;
    if let (true, Some(why)) = (why_open, &p.why) {
        let wl = text_block(why, &note_font(), &ink2(), w - 16.0, scale);
        wl.setPosition(pt(x + 8.0, h + 6.0));
        row.addSublayer(&wl);
        total += 6.0 + wl.bounds().size.height;
    }
    row.setBounds(cgr(Rect::new(0.0, 0.0, x + w, total)));
    root.addSublayer(&row);
    if !answered {
        body.regions.push(HitRegion { rect: Rect::new(yx - 2.0, y, yw + 4.0, h), control: Control::Yes { list: "goals".into(), id: p.id.clone() } });
    }
    if p.why.is_some() {
        body.regions.push(HitRegion { rect: Rect::new(x, y, (yx - x - 4.0).max(10.0), h), control: Control::Disclose { key: format!("why:{}", p.id) } });
    }
    let sig = [p.title.clone(), plus_n.to_string(), answered.to_string(), why_open.to_string(), format!("{w:.0}")].join("\u{1}");
    body.rows.insert(key.to_owned(), (row.clone(), sig));
    (row, y + total)
}

/// One active goal: its head (ring, title, bar, latest move, running line), its queued lines and its ghost step.
#[allow(clippy::too_many_arguments)]
fn goal_block(g: &Goal, running: usize, step: Option<&(Proposal, usize)>, root: &CALayer, y0: f64, width: f64, scale: f64, open_items: &HashSet<String>, body: &mut Body) -> f64 {
    let v = onesys::goal_view(g, running, step.is_some());
    let tw = width - GOAL_TEXT_X;
    let top = y0 + PAD_TOP;
    let row = keyed(top, width, 0.0);
    let symbol = Some(g.symbol()).filter(|s| symbol_exists(s)).unwrap_or_else(|| "flag".into());
    row.addSublayer(&ring(&symbol, v.done_share, v.run_share, scale));
    let (t, off, th) = boxed_lines(&g.title, &title_font(), &ink(), tw, 2, 22.0, scale);
    t.setPosition(pt(GOAL_TEXT_X, off));
    row.addSublayer(&t);
    let mut h = th + 7.0;
    bar(&v.segs, GOAL_TEXT_X, h, tw, &row);
    h += BAR_H + 8.0;
    if let Some(n) = &g.now {
        let (l, off, lh) = boxed_lines(n, &now_font(), &white(0.62), tw, 2, 18.0, scale);
        l.setPosition(pt(GOAL_TEXT_X, h + off));
        row.addSublayer(&l);
        h += lh;
    }
    // CSS margins collapse: what follows the bar sits 8 pt under it, whatever its own top margin.
    let mut after_bar = g.now.is_none();
    if v.running > 0 {
        h += if after_bar { 0.0 } else { 6.0 };
        after_bar = false;
        let d = dot(GOAL_TEXT_X + 3.5, h + 10.0, 7.0, &run_blue());
        glow(&d, &run_blue(), 3.0, 1.0);
        row.addSublayer(&d);
        let f = system(13.5, Weight::Medium);
        let l = text1(&format!("{} running on the right", v.running), &f, &run_blue().colorWithAlphaComponent(0.95), tw - 16.0, scale);
        l.setPosition(pt(GOAL_TEXT_X + 16.0, h + (20.0 - l.bounds().size.height) / 2.0));
        row.addSublayer(&l);
        h += 20.0;
    }
    let head_h = h.max(RING + RING_Y);
    row.setBounds(cgr(Rect::new(0.0, 0.0, width, head_h)));
    root.addSublayer(&row);
    let sig = [g.title.clone(), format!("{:?}", v.segs), format!("{:.3}/{:.3}", v.done_share, v.run_share), g.now.clone().unwrap_or_default(), v.running.to_string(), symbol, format!("{width:.0}")].join("\u{1}");
    body.rows.insert(format!("goal:{}", g.key), (row, sig));
    let mut y = top + head_h;
    // The queued tasks, a grey dot each (their keys are the tasks', so a yes on a step fills its ghost in place).
    let queued_keys: Vec<(String, String)> = {
        let mut backed = v.running;
        g.tasks
            .iter()
            .filter(|t| match t.state {
                crate::model::TaskState::Now if backed > 0 => {
                    backed -= 1;
                    false
                }
                crate::model::TaskState::Done => false,
                _ => true,
            })
            .map(|t| (t.key.clone(), t.title.clone()))
            .take(QUEUED)
            .collect()
    };
    for (key, title) in &queued_keys {
        y += if after_bar { 0.0 } else { 6.0 };
        after_bar = false;
        let l = keyed(y, width, 0.0);
        let (t, off, th) = boxed_lines(title, &line_font(), &ink(), tw - 16.0, 2, 20.0, scale);
        l.addSublayer(&dot(GOAL_TEXT_X + 3.5, th / 2.0, 7.0, &white(0.55)));
        t.setPosition(pt(GOAL_TEXT_X + 16.0, off));
        l.addSublayer(&t);
        l.setBounds(cgr(Rect::new(0.0, 0.0, width, th)));
        root.addSublayer(&l);
        body.rows.insert(format!("row:{key}"), (l, format!("{title}\u{1}{width:.0}")));
        y += th;
    }
    if let Some((p, n)) = step {
        y += if after_bar { 0.0 } else { 7.0 };
        y = ghost_row(p, *n, &format!("row:{}", p.id), root, GOAL_TEXT_X, y, tw, scale, open_items, body).1;
    }
    y + PAD_BOTTOM
}

/// A goal proposal after the goals: a dashed ring with its symbol, then the ghost box (title, Yes, +N). Keyed as the
/// goal it becomes.
fn ghost_goal(f: &(Proposal, usize), root: &CALayer, y0: f64, width: f64, scale: f64, open_items: &HashSet<String>, body: &mut Body) -> f64 {
    let (p, n) = f;
    let top = y0 + PAD_TOP;
    let disc = keyed(top, GOAL_TEXT_X - 4.0, RING + RING_Y);
    disc.addSublayer(&stroked(&ring_path(RING, RING / 2.0 - 1.0), RING_X, RING_Y, RING, RING, &white(0.45), 1.3, Some((3.0, 2.5))));
    let symbol = p.icon.clone().filter(|s| symbol_exists(s)).unwrap_or_else(|| "flag".into());
    let s = symbol_layer(&symbol, 15.0, &[white(0.75)], Weight::Medium, scale);
    s.setPosition(pt(RING_X + RING / 2.0, RING_Y + RING / 2.0));
    disc.addSublayer(&s);
    root.addSublayer(&disc);
    body.rows.insert(format!("gdisc:{}", p.id), (disc, symbol));
    // The box centred on the ring.
    let (_, bottom) = ghost_row(p, *n, &format!("goal:{}", p.id), root, GOAL_TEXT_X, top + RING_Y + RING / 2.0 - 16.0, width - GOAL_TEXT_X, scale, open_items, body);
    bottom.max(top + RING + RING_Y) + PAD_BOTTOM
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobj;

    #[test]
    fn goals_draw_the_active_goals_their_queue_and_one_ghost() {
        let now = time::now();
        let island = Island::new("goals", jobj! {"preset": "todo", "list": "goals", "goals": [
            {"id": "g-ship", "title": "Ship Heimdall publicly", "now": "Designing the lists", "tasks": [
                {"id": "t1", "title": "Done one", "state": "done"}, {"id": "t2", "title": "Build day columns", "state": "now"},
                {"id": "t3", "title": "Shadow job", "state": "next"}, {"id": "t4", "title": "Four", "state": "next"}, {"id": "t5", "title": "Five", "state": "next"}]},
            {"id": "g-old", "title": "Paused", "state": "paused"}],
            "proposals": [{"id": "p1", "title": "Prepare CV drafts", "kind": "step", "goal": "g-ship", "why": "User: CV first", "shown": now - 3600.0},
                          {"id": "p2", "title": "Next"}, {"id": "p3", "title": "Gone", "shown": now - 50.0 * 3600.0}]});
        let root = layer();
        let mut body = Body::empty();
        build(&island, &root, 0.0, 328.0, 2.0, &HashSet::new(), &mut body);
        let mut keys: Vec<&str> = body.rows.keys().map(|s| s.as_str()).collect();
        keys.sort();
        // No island runs in a unit test: "now" is queued too; three lines at most; the ghost keyed by its id.
        assert_eq!(keys, ["goal:g-ship", "row:p1", "row:t2", "row:t3", "row:t4"]);
        assert!(body.rows["row:p1"].1.starts_with("Prepare CV drafts\u{1}1\u{1}false\u{1}false"), "{}", body.rows["row:p1"].1);
        assert!(body.regions.iter().any(|r| matches!(&r.control, Control::Yes { list, id } if list == "goals" && id == "p1")));
        // Its title opens the why.
        let open: HashSet<String> = ["why:p1".to_owned()].into();
        let mut body = Body::empty();
        build(&island, &root, 0.0, 328.0, 2.0, &open, &mut body);
        assert!(body.rows["row:p1"].1.contains("\u{1}true\u{1}"));
    }

    #[test]
    fn a_goal_proposal_is_a_ghost_goal_and_nothing_at_all_is_the_empty_sign() {
        let island = Island::new("goals", jobj! {"preset": "todo", "list": "goals", "goals": [], "proposals": [{"id": "g-job", "title": "Get a job", "kind": "goal", "icon": "briefcase"}, {"id": "x", "title": "x"}]});
        let mut body = Body::empty();
        build(&island, &layer(), 0.0, 328.0, 2.0, &HashSet::new(), &mut body);
        assert!(body.rows.contains_key("goal:g-job") && body.rows.contains_key("gdisc:g-job"));
        let empty = Island::new("goals", jobj! {"preset": "todo", "list": "goals", "goals": []});
        let mut body = Body::empty();
        build(&empty, &layer(), 0.0, 328.0, 2.0, &HashSet::new(), &mut body);
        assert!(body.rows.contains_key("empty"));
    }
}
