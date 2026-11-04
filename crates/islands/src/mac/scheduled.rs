//! Scheduled, open (2 Nov, the one system: `docs/one-system.md`, the approved design
//! `Assistant/Plans/goals/one-system.html`): what is promised and when, and the one guarantee that nothing waits on
//! The user unseen. On top the guarantee line: a green ring, "Nothing waiting on you" and how long ago Heimdall's sweep
//! checked; an amber dot with the first thing that waits, its goal and line, its choices and "+N"; or "Not sure" with
//! the quartered ring (a silent source, or a sweep older than 30 min). Under a hairline the week strip: now's tick, a
//! white mark per promise due within seven days, a dashed ring where the front proposal would go. Then the rows by
//! time ("Tue 10:00", the goal's symbol, the title), the front proposal as a ghost row at its time's place with Yes.
//! An item without `due` is no promise and is not drawn. Nothing at all: "Nothing promised". Nothing moves at rest.

use std::collections::HashSet;

use objc2::rc::Retained;
use objc2_quartz_core::CALayer;

use super::draw::CgPath;
use super::lists::keyed;
use super::look::*;
use super::marks::*;
use super::parts::{Body, Control, HitRegion};
use super::world::Ctx;
use crate::model::due::Cal;
use crate::model::geometry::Rect;
use crate::model::onesys::{self, Guarantee, Proposal};
use crate::model::{time, Island};

/// The rows' grid: the lead column, the symbol's centre and where titles start.
pub const LEAD_W: f64 = 66.0;
const MARK_CX: f64 = 81.0;
const TEXT_X: f64 = 96.0;
const STRIP_H: f64 = 32.0;
const RING_D: f64 = 14.0;
const GU_TEXT_X: f64 = 24.0;
const CHIP_H: f64 = 28.0;

fn title_f() -> Retained<objc2_app_kit::NSFont> {
    system(14.5, Weight::Medium)
}
fn line_f() -> Retained<objc2_app_kit::NSFont> {
    system(12.5, Weight::Regular)
}

/// The quartered ring of "Not sure".
fn unsure_ring() -> Retained<CALayer> {
    let r = boxed(0.0, 0.0, RING_D, RING_D, &clear(), RING_D / 2.0);
    r.setBorderColor(Some(&cg(&white(0.55))));
    r.setBorderWidth(1.5);
    for start in [0.0, std::f64::consts::PI] {
        let p = CgPath::new();
        let c = RING_D / 2.0;
        p.move_to(c, c);
        p.arc(c, c, c, start - std::f64::consts::FRAC_PI_2, start, false);
        p.line_to(c, c);
        let s = objc2_quartz_core::CAShapeLayer::new();
        no_implicit(&s);
        s.setFrame(cgr(Rect::new(0.0, 0.0, RING_D, RING_D)));
        s.setPath(Some(p.cg()));
        s.setFillColor(Some(&cg(&white(0.55))));
        r.addSublayer(&s);
    }
    r.setOpacity(0.8);
    r
}

fn ok_ring() -> Retained<CALayer> {
    let r = boxed(0.0, 0.0, RING_D, RING_D, &clear(), RING_D / 2.0);
    r.setBorderColor(Some(&cg(&proof_green().colorWithAlphaComponent(0.8))));
    r.setBorderWidth(1.5);
    r.addSublayer(&boxed(4.0, 4.0, 6.0, 6.0, &proof_green(), 3.0));
    r
}

fn amber_dot() -> Retained<CALayer> {
    let d = boxed(0.0, 0.0, RING_D, RING_D, &amber(), RING_D / 2.0);
    glow(&d, &amber(), 4.0, 0.55);
    d
}

/// The guarantee line and the hairline under it. Returns where the strip starts.
#[allow(clippy::too_many_arguments)]
fn guarantee(raw: &crate::model::Json, ctx: &Ctx, now: f64, root: &CALayer, y0: f64, w: f64, scale: f64, open_items: &HashSet<String>, body: &mut Body) -> f64 {
    let g = onesys::guarantee(raw, now);
    let row = keyed(y0, w, 0.0);
    let top = 2.0;
    let tw = w - GU_TEXT_X;
    let place_ring = |r: Retained<CALayer>| {
        r.setPosition(pt(0.0, top + 3.0));
        row.addSublayer(&r);
    };
    let mut h = top;
    let mut sig = vec![];
    // A heading and a quiet line under it (with "+N" on its right), for waiting and not sure.
    let heading = |title: &str, title_c: Retained<objc2_app_kit::NSColor>, symbol: Option<String>, line: String, more: usize, h: &mut f64| {
        let (t, th) = pitched(title, &title_f(), &title_c, tw, 2, 20.0, scale);
        t.setPosition(pt(GU_TEXT_X, *h));
        row.addSublayer(&t);
        *h += th;
        let pl = plus(more, scale);
        let pw = pl.as_ref().map(|p| p.bounds().size.width + 8.0).unwrap_or(0.0);
        let mut x = GU_TEXT_X;
        if let Some(s) = symbol {
            let sl = symbol_layer(&s, 11.0, &[white(0.55)], Weight::Medium, scale);
            sl.setPosition(pt(x + 5.5, *h + 9.0));
            row.addSublayer(&sl);
            x += 17.0;
        }
        if !line.is_empty() {
            let l = text1(&line, &line_f(), &white(0.55), w - x - pw, scale);
            l.setPosition(pt(x, *h + (18.0 - l.bounds().size.height) / 2.0));
            row.addSublayer(&l);
        }
        if let Some(p) = &pl {
            p.setPosition(pt(w - p.bounds().size.width, *h + (18.0 - p.bounds().size.height) / 2.0));
            row.addSublayer(p);
        }
        *h += 18.0;
    };
    match &g {
        Guarantee::Quiet { checked } => {
            place_ring(ok_ring());
            let age = text1(&onesys::checked_label(*checked, now), &system(11.5, Weight::Medium), &white(0.4), 140.0, scale);
            let aw = age.bounds().size.width;
            age.setPosition(pt(w - aw, h + (20.0 - age.bounds().size.height) / 2.0));
            row.addSublayer(&age);
            let t = text1("Nothing waiting on you", &title_f(), &white(0.8), tw - aw - 10.0, scale);
            t.setPosition(pt(GU_TEXT_X, h + (20.0 - t.bounds().size.height) / 2.0));
            row.addSublayer(&t);
            h += 20.0;
            sig.push(format!("q{}", onesys::checked_label(*checked, now)));
        }
        Guarantee::Waiting { first, more } => {
            place_ring(amber_dot());
            let goal = first.goal.as_deref().and_then(|g| ctx.goal(g));
            let line = [goal.map(|g| g.title.clone()), first.line.clone()].into_iter().flatten().collect::<Vec<_>>().join(" · ");
            heading(&first.title, ink(), goal.map(|g| g.symbol.clone()).filter(|s| symbol_exists(s)), line.clone(), *more, &mut h);
            // The choices, the first one primary (white); one shows what was picked until the sweep moves on.
            let chosen = first.choices.iter().find(|c| open_items.contains(&format!("chose:{}:{}", first.id, c.action)));
            if !first.choices.is_empty() {
                h += 9.0;
                let mut x = GU_TEXT_X;
                let f = system(13.0, Weight::Semibold);
                for (k, c) in first.choices.iter().enumerate() {
                    let picked = chosen.is_some_and(|p| p.action == c.action);
                    let primary = if chosen.is_some() { picked } else { k == 0 };
                    let t = text1(&c.label, &f, &*if primary { black() } else if chosen.is_some() { ink3() } else { white(1.0) }, w - x - 26.0, scale);
                    let cw = t.bounds().size.width + 26.0 + if picked { 16.0 } else { 0.0 };
                    if x + cw > w + 0.5 {
                        break;
                    }
                    row.addSublayer(&boxed(x, h, cw, CHIP_H, &*if primary { rgb(0xF5F5F5, 1.0) } else { track_fill() }, CHIP_H / 2.0));
                    let mut tx = x + 13.0;
                    if picked {
                        let ck = symbol_layer("checkmark", 11.0, &[black()], Weight::Bold, scale);
                        ck.setPosition(pt(tx + 5.0, h + CHIP_H / 2.0));
                        row.addSublayer(&ck);
                        tx += 16.0;
                    }
                    t.setPosition(pt(tx, h + (CHIP_H - t.bounds().size.height) / 2.0));
                    row.addSublayer(&t);
                    if chosen.is_none() {
                        body.regions.push(HitRegion { rect: Rect::new(x, y0 + h, cw, CHIP_H), control: Control::Waiting { id: first.id.clone(), action: c.action.clone(), label: c.label.clone() } });
                    }
                    x += cw + 7.0;
                }
                h += CHIP_H;
            }
            sig.push(format!("w{}|{}|{line}|{more}|{:?}|{:?}", first.id, first.title, first.choices, chosen.map(|c| &c.action)));
        }
        Guarantee::Unsure { first, more } => {
            place_ring(unsure_ring());
            let text = first.text(now);
            heading("Not sure", ink(), None, text.clone(), *more, &mut h);
            sig.push(format!("u{text}|{more}"));
        }
        Guarantee::Stale { checked } => {
            place_ring(unsure_ring());
            let text = match checked {
                Some(c) => format!("Not checked for {}", onesys::minutes(now - c)),
                None => "Not checked yet".into(),
            };
            heading("Not sure", ink(), None, text.clone(), 0, &mut h);
            sig.push(format!("s{text}"));
        }
    }
    h += 12.0;
    row.addSublayer(&boxed(0.0, h, w, 1.0, &white(0.08), 0.0));
    h += 1.0;
    row.setBounds(cgr(Rect::new(0.0, 0.0, w, h)));
    root.addSublayer(&row);
    sig.push(format!("{w:.0}"));
    body.rows.insert("guarantee".into(), (row, sig.join("\u{1}")));
    y0 + h + 10.0
}

/// The week strip: track, now's tick, a white mark per promise within seven days, a dashed ring for the proposal.
#[allow(clippy::too_many_arguments)]
fn strip(dues: &[f64], ghost: Option<f64>, now: f64, cal: &Cal, root: &CALayer, y0: f64, w: f64, scale: f64, body: &mut Body) -> f64 {
    let g = keyed(y0 + 2.0, w, STRIP_H);
    g.addSublayer(&boxed(0.0, 6.0, w, 6.0, &white(0.08), 3.0));
    g.addSublayer(&boxed(0.0, 2.0, 2.5, 14.0, &white(1.0), 1.25));
    let mut sig = vec![];
    for d in dues.iter().filter(|d| **d - now <= onesys::WEEK_H * 3600.0) {
        let x = onesys::strip_x(*d, now, w);
        g.addSublayer(&boxed(x - 3.0, 4.0, 6.0, 10.0, &white(1.0), 3.0));
        sig.push(format!("{x:.0}"));
    }
    if let Some(d) = ghost.filter(|d| *d - now <= onesys::WEEK_H * 3600.0) {
        let x = onesys::strip_x(d, now, w);
        g.addSublayer(&boxed(x - 4.5, 4.5, 9.0, 9.0, &black(), 4.5));
        g.addSublayer(&stroked(&ring_path(9.0, 4.5), x - 4.5, 4.5, 9.0, 9.0, &white(0.8), 1.3, Some((2.2, 1.6))));
        sig.push(format!("g{x:.0}"));
    }
    let f = system(11.0, Weight::Regular);
    for (k, (name, h)) in onesys::strip_labels(now, cal).into_iter().enumerate() {
        let l = text1(&name, &f, &white(0.5), 80.0, scale);
        let lw = l.bounds().size.width;
        let x = match k {
            0 => 0.0,
            3 => w - lw,
            _ => onesys::strip_x(now + h * 3600.0, now, w) - lw / 2.0,
        };
        l.setPosition(pt(x, 29.0 - f.ascender()));
        g.addSublayer(&l);
        sig.push(name);
    }
    root.addSublayer(&g);
    body.rows.insert("strip".into(), (g, sig.join("\u{1}")));
    y0 + 2.0 + STRIP_H + 4.0
}

enum Row<'a> {
    Promise(&'a crate::model::PlanItem),
    Ghost(&'a Proposal, usize),
}

/// The open Scheduled list.
#[allow(clippy::too_many_arguments)]
pub fn build(island: &Island, root: &CALayer, y0: f64, w: f64, scale: f64, open_items: &HashSet<String>, body: &mut Body, cal: &Cal) -> f64 {
    let ctx = Ctx::current();
    let now = time::now();
    let raw = &island.raw;
    let mut y = guarantee(raw, &ctx, now, root, y0, w, scale, open_items, body);
    let promises = onesys::promises(raw);
    let front = onesys::front(raw, now);
    let dues: Vec<f64> = promises.iter().filter_map(|p| p.due).collect();
    y = strip(&dues, front.as_ref().and_then(|f| f.0.due), now, cal, root, y, w, scale, body);
    if promises.is_empty() && front.is_none() {
        let l = keyed(y, w, 26.0);
        let t = text1("Nothing promised", &system(13.0, Weight::Regular), &white(0.4), w, scale);
        t.setPosition(pt(0.0, 4.0));
        l.addSublayer(&t);
        root.addSublayer(&l);
        body.rows.insert("empty".into(), (l, "Nothing promised".into()));
        return y + 26.0;
    }
    let mut rows: Vec<(f64, Row)> = promises.iter().map(|p| (p.due.unwrap_or(f64::MAX), Row::Promise(p))).collect();
    if let Some((p, n)) = &front {
        rows.push((p.due.unwrap_or(f64::MAX), Row::Ghost(p, *n)));
    }
    rows.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    for (due, r) in rows {
        y += 2.0;
        let (key, title, symbol, ghost) = match &r {
            Row::Promise(p) => (p.key.clone(), p.title.clone(), ctx.symbol(p.icon.as_deref(), p.goal.as_deref(), &p.title), None),
            Row::Ghost(p, n) => (p.id.clone(), p.title.clone(), ctx.symbol(p.icon.as_deref(), p.goal.as_deref(), &p.title), Some((*p, *n))),
        };
        let lead = if due < f64::MAX { onesys::sched_lead(due, now, cal) } else { String::new() };
        let answered = ghost.is_some_and(|(p, _)| open_items.contains(&format!("yes:{}", p.id)));
        let why_open = ghost.is_some_and(|(p, _)| p.why.is_some() && open_items.contains(&format!("why:{}", p.id)));
        let yes = ghost.map(|_| yes_chip(answered, scale));
        let pl = ghost.and_then(|(_, n)| plus(n, scale));
        let pw = pl.as_ref().map(|p| p.bounds().size.width + 7.0).unwrap_or(0.0);
        let right = w - pw;
        let yw = yes.as_ref().map(|y| y.1 + 7.0).unwrap_or(0.0);
        let tf = title_f();
        let (t, th) = pitched(&title, &tf, &*if ghost.is_some() { white(0.78) } else { ink() }, right - yw - TEXT_X, 2, 19.0, scale);
        let inner = th.max(20.0).max(if ghost.is_some() { YES_H } else { 0.0 });
        let h = inner + 10.0;
        let row = keyed(y, w, 0.0);
        set_bg(&row, &clear());
        if ghost.is_some() {
            row.addSublayer(&ghost_box(-4.0, 0.0, w + 10.0, h));
        }
        let mid = 5.0 + inner / 2.0;
        let now_lead = lead == "Now";
        let lf = if now_lead { digits(12.5, Weight::Semibold) } else { digits(12.5, Weight::Medium) };
        let l = text1(&lead, &lf, &*if now_lead { ink() } else { white(0.55) }, MARK_CX - 8.0, scale);
        l.setPosition(pt(0.0, mid - l.bounds().size.height / 2.0));
        row.addSublayer(&l);
        let s = symbol_layer(&symbol, 13.0, &[white(0.62)], Weight::Medium, scale);
        s.setPosition(pt(MARK_CX, mid));
        row.addSublayer(&s);
        t.setPosition(pt(TEXT_X, mid - th / 2.0));
        row.addSublayer(&t);
        let mut total = h;
        if let Some((c, cw)) = &yes {
            c.setPosition(pt(right - cw, mid - YES_H / 2.0));
            row.addSublayer(c);
            if !answered {
                body.regions.push(HitRegion { rect: Rect::new(right - cw - 2.0, y, cw + 4.0, h), control: Control::Yes { list: "scheduled".into(), id: key.clone() } });
            }
        }
        if let Some(p) = &pl {
            p.setPosition(pt(w - p.bounds().size.width, mid - p.bounds().size.height / 2.0));
            row.addSublayer(p);
        }
        if let Some((p, _)) = ghost {
            if p.why.is_some() {
                body.regions.push(HitRegion { rect: Rect::new(0.0, y, right - yw - 4.0, h), control: Control::Disclose { key: format!("why:{}", p.id) } });
            }
            if let (true, Some(why)) = (why_open, &p.why) {
                let wl = text_block(why, &note_font(), &ink2(), w - TEXT_X, scale);
                wl.setPosition(pt(TEXT_X, h + 2.0));
                row.addSublayer(&wl);
                total += 2.0 + wl.bounds().size.height + 6.0;
            }
        }
        row.setBounds(cgr(Rect::new(0.0, 0.0, w, total)));
        root.addSublayer(&row);
        let sig = [title, lead, symbol, ghost.is_some().to_string(), answered.to_string(), why_open.to_string(), ghost.map(|g| g.1).unwrap_or(0).to_string(), format!("{w:.0}")].join("\u{1}");
        body.rows.insert(format!("row:{key}"), (row, sig));
        y += total;
    }
    y
}

/// What Scheduled's open body shows by the clock now (its redraw signature) and when that next changes.
pub fn label_sig(raw: &crate::model::Json, now: f64, cal: &Cal) -> String {
    let g = format!("{:?}", onesys::guarantee(raw, now)).replace(|c: char| c.is_ascii_digit() || c == '.', "");
    let g = match onesys::guarantee(raw, now) {
        Guarantee::Quiet { checked } => onesys::checked_label(checked, now),
        Guarantee::Stale { checked: Some(c) } => onesys::minutes(now - c),
        Guarantee::Unsure { first, .. } => first.text(now),
        _ => g,
    };
    let leads: Vec<String> = onesys::promises(raw).iter().filter_map(|p| p.due).map(|d| onesys::sched_lead(d, now, cal)).collect();
    let front = onesys::front(raw, now).map(|(p, n)| format!("{}+{n}", p.id)).unwrap_or_default();
    format!("{g}|{}|{front}|{:.0}", leads.join(","), cal.start_of_day(now))
}

pub fn next_change(raw: &crate::model::Json, now: f64, cal: &Cal) -> Option<f64> {
    let leads = onesys::promises(raw).iter().filter_map(|p| p.due).filter_map(|d| onesys::sched_lead_next(d, now, cal)).fold(None, |a: Option<f64>, b| Some(a.map_or(b, |a| a.min(b))));
    let midnight = cal.add_days(1, cal.start_of_day(now));
    [onesys::guarantee_next(raw, now), onesys::next_expiry(raw, now), leads, midnight].into_iter().flatten().fold(None, |a: Option<f64>, b| Some(a.map_or(b, |a| a.min(b))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobj;

    #[test]
    fn scheduled_draws_the_guarantee_the_strip_and_a_ghost_in_its_place() {
        let now = time::now();
        let cal = Cal::current();
        let island = Island::new("scheduled", jobj! {"preset": "todo", "list": "scheduled",
            "guarantee": {"checked": now - 120.0, "waiting": [], "unsure": []},
            "items": [{"id": "a", "title": "Later one", "due": now + 50.0 * 3600.0}, {"id": "b", "title": "No time"}, {"id": "c", "title": "Soon", "due": now + 3600.0}],
            "proposals": [{"id": "p", "title": "Release: check model licences", "due": now + 20.0 * 3600.0, "why": "User said", "shown": now}]});
        let mut body = Body::empty();
        build(&island, &layer(), 0.0, 328.0, 2.0, &HashSet::new(), &mut body, &cal);
        let mut rows: Vec<(f64, String)> = body.rows.iter().filter(|(k, _)| k.starts_with("row:")).map(|(k, r)| (r.0.frame().origin.y, k.clone())).collect();
        rows.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        assert_eq!(rows.iter().map(|r| r.1.as_str()).collect::<Vec<_>>(), ["row:c", "row:p", "row:a"], "by time, the ghost at its place, no row without due");
        assert!(body.rows["guarantee"].1.starts_with("qchecked 2 min ago"), "{}", body.rows["guarantee"].1);
        assert!(body.rows["strip"].1.contains("\u{1}g"), "the proposal's dashed ring");
        assert!(body.regions.iter().any(|r| matches!(&r.control, Control::Yes { list, id } if list == "scheduled" && id == "p")));
        assert!(next_change(&island.raw, now, &cal).is_some_and(|t| t > now && t <= now + 60.0));
    }

    #[test]
    fn a_waiting_item_offers_its_choices_and_nothing_promised_says_so() {
        let now = time::now();
        let cal = Cal::current();
        let island = Island::new("scheduled", jobj! {"preset": "todo", "list": "scheduled",
            "guarantee": {"checked": now - 60.0, "waiting": [{"id": "w", "title": "Approve the Metal toolchain download", "line": "blocks Arsenal 1.0",
                "choices": [{"label": "Allow", "action": "allow"}, {"label": "Not now", "action": "not-now"}]}, {"id": "w2", "title": "x"}, {"id": "w3", "title": "y"}]}});
        let mut body = Body::empty();
        build(&island, &layer(), 0.0, 328.0, 2.0, &HashSet::new(), &mut body, &cal);
        let choices: Vec<&str> = body.regions.iter().filter_map(|r| match &r.control { Control::Waiting { id, action, .. } if id == "w" => Some(action.as_str()), _ => None }).collect();
        assert_eq!(choices, ["allow", "not-now"]);
        assert!(body.rows.contains_key("empty"));
        let stale = Island::new("scheduled", jobj! {"preset": "todo", "list": "scheduled", "guarantee": {"checked": now - 2400.0}});
        let mut body = Body::empty();
        build(&stale, &layer(), 0.0, 328.0, 2.0, &HashSet::new(), &mut body, &cal);
        assert!(body.rows["guarantee"].1.starts_with("sNot checked for 40 min"), "{}", body.rows["guarantee"].1);
    }
}
