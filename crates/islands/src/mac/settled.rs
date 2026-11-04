//! Settled, open (2 Nov, the one system: `docs/one-system.md`, the approved design
//! `Assistant/Plans/goals/one-system.html`): proof of progress. Nothing settles without proof (a commit, a test run, a
//! file, an install, the user's own quote); a finished piece without one is claimed, drawn dashed and never counted. The
//! big count is the proved items since the clean slate while it is under a week old ("since the clean slate"), else
//! this week's; "N claimed" beside a dashed pill. Seven day columns: a green pill per proved item, a dashed one per
//! claim on top, a dim baseline on an empty day, Today lit in a faint blue column. Then today's newest three rows (time,
//! the goal's symbol, the title, the proof chip; a claim's chip dashed) and "› N earlier today", which unfolds. A chip
//! shows its proof under the row; a file proof also opens. Nothing moves at rest.

use std::collections::HashSet;

use objc2_quartz_core::CALayer;

use super::lists::keyed;
use super::look::*;
use super::marks::*;
use super::parts::{Body, Control, HitRegion};
use super::world::Ctx;
use crate::model::due::Cal;
use crate::model::geometry::Rect;
use crate::model::onesys::{self, Proof, SettledItem};

/// Chart geometry: its height (labels under it), the column gap, a pill and its gap.
const CHART_H: f64 = 74.0;
const LABELS_H: f64 = 20.0;
const COL_GAP: f64 = 6.0;
const PILL_H: f64 = 7.0;
const PILL_GAP: f64 = 3.0;
/// The rows' grid: lead, symbol centre, title start.
const LEAD_W: f64 = 42.0;
const MARK_CX: f64 = 57.0;
const TEXT_X: f64 = 72.0;
/// Today's rows before "N earlier today".
const OPEN_CAP: usize = 3;
/// The open-items key that unfolds today's every row.
pub const ALL_KEY: &str = "settled:all";

/// The big count and its caption. Returns the layer, its width and height.
fn count_block(n: usize, claimed: usize, since_slate: bool, scale: f64) -> (objc2::rc::Retained<CALayer>, f64, f64) {
    let g = layer();
    g.setAnchorPoint(pt(0.0, 0.0));
    let num = text1(&n.to_string(), &digits(28.0, Weight::Bold), &ink(), 200.0, scale);
    let nh = num.bounds().size.height;
    let cap_h = 28.0;
    num.setPosition(pt(-1.0, (cap_h - nh) / 2.0));
    g.addSublayer(&num);
    let f = system(12.0, Weight::Medium);
    let y = cap_h + 4.0;
    let t = text1(if since_slate { "since the clean slate" } else { "this week" }, &f, &white(0.55), 240.0, scale);
    let lh = t.bounds().size.height;
    t.setPosition(pt(0.0, y));
    g.addSublayer(&t);
    let mut x = t.bounds().size.width;
    if claimed > 0 {
        x += 12.0;
        g.addSublayer(&dashed_box(x, y + (lh - 7.0) / 2.0, 14.0, 7.0, 3.5, &white(0.5), 1.0, (2.0, 1.5)));
        x += 14.0 + 5.0;
        let c = text1(&format!("{claimed} claimed"), &f, &white(0.55), 120.0, scale);
        c.setPosition(pt(x, y));
        g.addSublayer(&c);
        x += c.bounds().size.width;
    }
    let h = y + lh;
    g.setBounds(cgr(Rect::new(0.0, 0.0, x.max(num.bounds().size.width), h)));
    (g, x.max(num.bounds().size.width), h)
}

/// The seven columns and their day names.
fn columns(cols: &[onesys::DayColumn], cal: &Cal, w: f64, scale: f64) -> (objc2::rc::Retained<CALayer>, Vec<f64>, String) {
    let g = layer();
    g.setAnchorPoint(pt(0.0, 0.0));
    let cw = (w - COL_GAP * 6.0) / 7.0;
    let most = cols.iter().map(|c| c.proved + c.claimed).max().unwrap_or(0) as f64;
    let pitch = (PILL_H + PILL_GAP).min((CHART_H - 2.0) / most.max(1.0)).max(1.5);
    let ph = (pitch * PILL_H / (PILL_H + PILL_GAP)).max(1.0);
    let mut tops = vec![];
    let mut sig = vec![];
    for (d, c) in cols.iter().enumerate() {
        let x = d as f64 * (cw + COL_GAP);
        let today = d == 6;
        if today {
            let lit = boxed(x - 3.0, -2.0, cw + 6.0, CHART_H + 4.0, &run_blue().colorWithAlphaComponent(0.07), 8.0);
            lit.setBorderColor(Some(&cg(&run_blue().colorWithAlphaComponent(0.35))));
            lit.setBorderWidth(1.0);
            g.addSublayer(&lit);
        }
        if c.proved + c.claimed == 0 {
            g.addSublayer(&boxed(x, CHART_H - 3.0, cw, 3.0, &white(0.12), 1.5));
        }
        for k in 0..c.proved {
            let y = CHART_H - (k as f64 + 1.0) * pitch + (pitch - ph);
            let p = boxed(x, y, cw, ph, &proof_green(), ph / 2.0);
            p.setOpacity(0.6);
            g.addSublayer(&p);
        }
        for k in 0..c.claimed {
            let y = CHART_H - ((c.proved + k) as f64 + 1.0) * pitch + (pitch - ph);
            g.addSublayer(&dashed_box(x + 1.0, y + 1.0, cw - 2.0, (ph - 2.0).max(1.0), (ph / 2.0 - 1.0).max(0.5), &white(0.5), 1.3, (3.0, 2.0)));
        }
        tops.push(CHART_H - (c.proved + c.claimed) as f64 * pitch);
        let name = if today { "Today".to_owned() } else { cal.weekday(c.start + 12.0 * 3600.0) };
        let f = system(11.0, if today { Weight::Semibold } else { Weight::Medium });
        let l = text1(&name, &f, &white(if today { 0.96 } else { 0.4 }), cw + COL_GAP, scale);
        l.setPosition(pt(x + (cw - l.bounds().size.width) / 2.0, CHART_H + 16.0 - f.ascender()));
        g.addSublayer(&l);
        sig.push(format!("{name}{}/{}{}", c.proved, c.claimed, if c.before_slate { "b" } else { "" }));
    }
    g.setOpacity(0.85);
    g.setBounds(cgr(Rect::new(0.0, 0.0, w, CHART_H + LABELS_H)));
    (g, tops, sig.join(","))
}

/// The proof chip: a symbol and its kind on #1c1c1e; a claim's "claimed" in a dashed outline. Its width.
fn proof_chip(proof: Option<&Proof>, scale: f64) -> (objc2::rc::Retained<CALayer>, f64) {
    let f = system(11.5, Weight::Medium);
    match proof {
        Some(p) => chip(Proof::symbol(&p.kind), &p.kind, &f, &white(0.75), &white(0.75), &chip_fill(), 20.0, 8.0, 120.0, scale),
        None => {
            let t = text1("claimed", &f, &white(0.6), 100.0, scale);
            let w = t.bounds().size.width + 16.0;
            let c = layer();
            c.setAnchorPoint(pt(0.0, 0.0));
            c.setBounds(cgr(Rect::new(0.0, 0.0, w, 20.0)));
            c.addSublayer(&dashed_box(0.0, 0.0, w, 20.0, 10.0, &white(0.4), 1.0, (3.0, 2.0)));
            t.setPosition(pt(8.0, (20.0 - t.bounds().size.height) / 2.0));
            c.addSublayer(&t);
            (c, w)
        }
    }
}

/// Settled's whole body.
#[allow(clippy::too_many_arguments)]
pub fn build(raw: &crate::model::Json, now: f64, cal: &Cal, root: &CALayer, y0: f64, w: f64, scale: f64, open_items: &HashSet<String>, body: &mut Body) -> f64 {
    let ctx = Ctx::current();
    let slate = onesys::slate(raw);
    let items: Vec<SettledItem> = onesys::settled_items(raw, now).into_iter().filter(|i| slate.is_none_or(|s| i.at >= s)).collect();
    let count = onesys::settled_count(&items, slate, now, cal);
    let cols = onesys::day_columns(&items, slate, now, cal);
    let (cb, cbw, cbh) = count_block(count.proved, count.claimed, count.since_slate, scale);
    let (chart, tops, csig) = columns(&cols, cal, w, scale);
    // The count sits over the columns' empty top left, unless a stack there would run under it.
    let cw = (w - COL_GAP * 6.0) / 7.0;
    let overlay = tops.iter().enumerate().all(|(d, top)| d as f64 * (cw + COL_GAP) > cbw + 8.0 || *top >= cbh + 6.0);
    let top = y0 + 4.0;
    let head = keyed(top, w, 0.0);
    head.addSublayer(&chart);
    chart.setPosition(pt(0.0, if overlay { 0.0 } else { cbh + 12.0 }));
    head.addSublayer(&cb);
    cb.setPosition(pt(0.0, 0.0));
    let hh = CHART_H + LABELS_H + if overlay { 0.0 } else { cbh + 12.0 };
    head.setBounds(cgr(Rect::new(0.0, 0.0, w, hh)));
    root.addSublayer(&head);
    body.rows.insert("graphic".into(), (head, format!("{}|{}|{}|{csig}|{w:.0}", count.proved, count.claimed, count.since_slate)));
    let mut y = top + hh;

    // Today's rows, newest first.
    let today = cols[6].start;
    let mut day: Vec<&SettledItem> = items.iter().filter(|i| i.at >= today).collect();
    day.reverse();
    let all = open_items.contains(ALL_KEY);
    let shown = if all || day.len() <= OPEN_CAP { day.len() } else { OPEN_CAP };
    let tf = system(14.5, Weight::Medium);
    for it in day.iter().take(shown) {
        let (chip_l, chip_w) = proof_chip(it.proof.as_ref(), scale);
        let (t, th) = pitched(&it.title, &tf, &*if it.proof.is_some() { ink() } else { white(0.6) }, w - TEXT_X - chip_w - 7.0, 2, 19.0, scale);
        let inner = th.max(20.0);
        let h = inner + 8.0;
        let mid = 4.0 + inner / 2.0;
        let row = keyed(y, w, 0.0);
        let (hr, mi) = cal.hour_minute(it.at);
        let lead = text1(&format!("{hr:02}:{mi:02}"), &digits(12.5, Weight::Medium), &white(0.55), LEAD_W, scale);
        lead.setPosition(pt(0.0, mid - lead.bounds().size.height / 2.0));
        row.addSublayer(&lead);
        let symbol = ctx.symbol(None, it.goal.as_deref(), &it.title);
        let s = symbol_layer(&symbol, 13.0, &[white(0.62)], Weight::Medium, scale);
        s.setPosition(pt(MARK_CX, mid));
        row.addSublayer(&s);
        t.setPosition(pt(TEXT_X, mid - th / 2.0));
        row.addSublayer(&t);
        chip_l.setPosition(pt(w - chip_w, mid - 10.0));
        row.addSublayer(&chip_l);
        let pkey = format!("proof:{}", it.key);
        let open = open_items.contains(&pkey);
        let mut total = h;
        if let (true, Some(p)) = (open, &it.proof) {
            let label = p.label.clone().unwrap_or_else(|| super::parts::capitalized(&p.kind));
            let lb = text1(&label, &system(12.0, Weight::Semibold), &ink(), w - TEXT_X - 24.0, scale);
            let vl = text_block(&p.value, &system(12.0, Weight::Regular), &ink2(), w - TEXT_X - 24.0, scale);
            let lbh = lb.bounds().size.height;
            let bh = lbh + 2.0 + vl.bounds().size.height + 16.0;
            row.addSublayer(&boxed(TEXT_X, h, w - TEXT_X, bh, &white(0.08), 10.0));
            lb.setPosition(pt(TEXT_X + 12.0, h + 8.0));
            row.addSublayer(&lb);
            vl.setPosition(pt(TEXT_X + 12.0, h + 10.0 + lbh));
            row.addSublayer(&vl);
            total += bh + 4.0;
        }
        row.setBounds(cgr(Rect::new(0.0, 0.0, w, total)));
        root.addSublayer(&row);
        if let Some(p) = &it.proof {
            let file = (p.kind == "file" && !p.value.is_empty()).then(|| p.value.clone());
            body.regions.push(HitRegion { rect: Rect::new(w - chip_w - 4.0, y, chip_w + 4.0, h), control: Control::Proof { key: it.key.clone(), open: file } });
        }
        let sig = [it.title.clone(), format!("{hr:02}:{mi:02}"), symbol, format!("{:?}", it.proof), open.to_string(), format!("{w:.0}")].join("\u{1}");
        body.rows.insert(format!("row:{}", it.key), (row, sig));
        y += total;
    }
    if day.len() > OPEN_CAP {
        y += 4.0;
        let row = keyed(y, w, 0.0);
        let ch = symbol_layer(if all { "chevron.up" } else { "chevron.right" }, 9.0, &[white(0.5)], Weight::Semibold, scale);
        ch.setPosition(pt(TEXT_X + 4.5, 12.0));
        row.addSublayer(&ch);
        let label = if all { String::new() } else { format!("{} earlier today", day.len() - OPEN_CAP) };
        let t = text1(&label, &system(13.0, Weight::Medium), &white(0.55), w - TEXT_X, scale);
        t.setPosition(pt(TEXT_X + 15.0, 12.0 - t.bounds().size.height / 2.0));
        row.addSublayer(&t);
        row.setBounds(cgr(Rect::new(0.0, 0.0, w, 24.0)));
        root.addSublayer(&row);
        body.rows.insert("settled:more".into(), (row, format!("{label}\u{1}{all}")));
        body.regions.push(HitRegion { rect: Rect::new(TEXT_X - 4.0, y, w - TEXT_X + 4.0, 24.0), control: Control::Disclose { key: ALL_KEY.into() } });
        y += 24.0;
    }
    y
}

/// What Settled shows by the clock (its days move on at midnight; the slate turns a week old).
pub fn label_sig(raw: &crate::model::Json, now: f64, cal: &Cal) -> String {
    let c = onesys::settled_count(&onesys::settled_items(raw, now), onesys::slate(raw), now, cal);
    format!("{:.0}|{}", cal.start_of_day(now), c.since_slate)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobj;

    #[test]
    fn today_shows_three_rows_then_earlier_and_claims_are_not_counted() {
        let cal = Cal::current();
        let now = crate::model::time::now();
        let day0 = cal.start_of_day(now);
        let at = |k: f64| day0 + (now - day0) * k;
        let mut items = vec![];
        for k in 0..7 {
            items.push(serde_json::json!({"id": format!("p{k}"), "title": format!("Proved {k}"), "at": at(0.1 + k as f64 * 0.1), "proof": {"kind": "file", "value": "/tmp/x.html"}}));
        }
        items.push(serde_json::json!({"id": "c", "title": "Claimed", "at": at(0.95)}));
        let raw = jobj! {"slate": day0 - 3600.0, "items": items};
        let mut body = Body::empty();
        build(&raw, now, &cal, &layer(), 0.0, 328.0, 2.0, &HashSet::new(), &mut body);
        assert!(body.rows["graphic"].1.starts_with("7|1|true|"), "{}", body.rows["graphic"].1);
        assert_eq!(body.rows.keys().filter(|k| k.starts_with("row:")).count(), 3);
        assert!(body.rows.contains_key("row:c"), "the newest first, a claim among them");
        assert!(body.rows["settled:more"].1.starts_with("5 earlier today"));
        assert!(body.regions.iter().any(|r| matches!(&r.control, Control::Proof { key, open: Some(f) } if key == "p6" && f == "/tmp/x.html")));
        let mut body = Body::empty();
        build(&raw, now, &cal, &layer(), 0.0, 328.0, 2.0, &[ALL_KEY.to_owned()].into(), &mut body);
        assert_eq!(body.rows.keys().filter(|k| k.starts_with("row:")).count(), 8);
        // Nothing yet: the count and the columns, nothing else.
        let mut body = Body::empty();
        build(&jobj! {"slate": day0, "items": []}, now, &cal, &layer(), 0.0, 328.0, 2.0, &HashSet::new(), &mut body);
        assert_eq!(body.rows.keys().collect::<Vec<_>>(), ["graphic"]);
        assert!(body.rows["graphic"].1.starts_with("0|0|true|"));
    }
}
