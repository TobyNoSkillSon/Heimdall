//! The nine parts (Ideas/generative-ui.md §3): bars, line, stat, list, image, slider, toggle, button, text; the status,
//! steps, items, key values and open button an island shows; and the plan lists. Each renders into a layer of a given
//! width and registers its click targets (Swift `Parts.swift`).

use std::collections::{HashMap, HashSet};
use std::path::Path;

use objc2::rc::Retained;
use objc2_app_kit::NSColor;
use objc2_quartz_core::{CALayer, CATextLayer};
use serde_json::Value;

use super::draw;
use super::glance::{build_footer, open_chip, place_mark, Spark, SparkStyle, MARK_H, MARK_W};
use super::look::*;
use crate::model::due::{Cal, Due};
use crate::model::geometry::Rect;
use crate::model::glance::{kv_mark, Mark, Series};
use crate::model::island::viewer_path;
use crate::model::{format_number, j, time, Island, Json};

#[derive(Clone, Debug, PartialEq)]
pub enum Control {
    Button { id: String, label: String },
    ListRow { id: String, title: String },
    Toggle { id: String, on: bool },
    Slider { id: String, min: f64, max: f64, step: f64 },
    /// Open `target` (`OpenTarget::route`); `from` is the clicked element's frame in body coordinates and `image` the
    /// picture it shows, so a viewer grows out of it (Quick Look style) instead of the notch.
    Open { target: String, from: Option<Rect>, image: Option<String> },
    /// Tick a plan item off or back on (`key`: its stable id, see `PlanItem::key`).
    Tick { key: String, title: String },
    /// Open or close a waiting item's proposal.
    Disclose { key: String },
    /// Answer a proposal: Yes / No / Later (or the file's own actions).
    Approve { key: String, action: String },
    /// Show or hide a goal's finished tasks (`key`: the goal's, `Goal::key`).
    GoalToggle { key: String },
    /// Yes on a list's front proposal: one click, an `approval` response (`docs/one-system.md`).
    Yes { list: String, id: String },
    /// A choice on Scheduled's guarantee line: a `waiting` response for the waiting item `id`.
    Waiting { id: String, action: String, label: String },
    /// A Settled row's proof chip: shows the proof under the row; a file proof also opens.
    Proof { key: String, open: Option<String> },
    /// A job island's bottom row: show or hide its stage band (`details`).
    Details,
}

impl Control {
    /// The same control (what a press and its release must agree on).
    pub fn same(&self, b: &Control) -> bool {
        use Control::*;
        match (self, b) {
            (Open { target: x, .. }, Open { target: y, .. }) => x == y,
            (Button { id: x, .. }, Button { id: y, .. }) | (ListRow { id: x, .. }, ListRow { id: y, .. }) | (Toggle { id: x, .. }, Toggle { id: y, .. }) => x == y,
            (Tick { key: x, .. }, Tick { key: y, .. }) | (Disclose { key: x }, Disclose { key: y }) | (GoalToggle { key: x }, GoalToggle { key: y }) => x == y,
            (Approve { key: x, action: a }, Approve { key: y, action: b }) | (Waiting { id: x, action: a, .. }, Waiting { id: y, action: b, .. }) => x == y && a == b,
            (Yes { list: a, id: x }, Yes { list: b, id: y }) => a == b && x == y,
            (Proof { key: x, .. }, Proof { key: y, .. }) => x == y,
            (Details, Details) => true,
            _ => false,
        }
    }
}

#[derive(Clone, Debug)]
pub struct HitRegion {
    /// Body coordinates.
    pub rect: Rect,
    pub control: Control,
}

pub struct SliderParts {
    pub track: Rect,
    pub fill: Retained<CALayer>,
    pub knob: Retained<CALayer>,
    pub label: Option<Retained<CATextLayer>>,
    pub unit: String,
}

pub type Clock = (Retained<CATextLayer>, Box<dyn Fn() -> String>);

pub struct Body {
    pub layer: Retained<CALayer>,
    pub height: f64,
    pub regions: Vec<HitRegion>,
    pub sliders: HashMap<String, SliderParts>,
    /// Small arrival animations, played when the body is shown.
    pub arrivals: Vec<Box<dyn Fn()>>,
    /// What moves when a rewrite arrives while the body is shown: a sparkline draws its newest step, a bar grows
    /// from its last value. Nothing else moves; nothing at rest.
    pub updates: Vec<Box<dyn Fn()>>,
    /// The "started 12 min ago" line, refreshed once a minute while the island is open.
    pub clock: Option<Clock>,
    /// Plan rows, group headers and open proposals, each in its own layer keyed `row:<key>`, `hdr:<group>`,
    /// `prop:<key>`, with the signature of what it shows: an update diffs them by key and animates each.
    pub rows: HashMap<String, (Retained<CALayer>, String)>,
    /// Cleaned `html` fragments drawn with their fallback this time because their bitmap is not ready yet: the
    /// controller asks the renderer for them and rebuilds the body when they arrive.
    pub pending_html: Vec<String>,
}

impl Body {
    pub fn empty() -> Body {
        let l = layer();
        Body { layer: l, height: 0.0, regions: vec![], sliders: HashMap::new(), arrivals: vec![], updates: vec![], clock: None, rows: HashMap::new(), pending_html: vec![] }
    }
}

fn num_of(v: Option<&Value>) -> Option<f64> {
    match v? {
        Value::String(s) => s.parse().ok(),
        v => j::num(Some(v)),
    }
}

fn str_of(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => n.as_f64().map(format_number),
        Value::Bool(b) => Some(if *b { "1" } else { "0" }.into()),
        _ => None,
    }
}

fn part(t: &str) -> Json {
    let mut o = Json::new();
    o.insert("type".into(), Value::from(t));
    o
}

/// A job island (running, waiting, done, failed): its status is one row at the bottom, the glance.
pub fn is_job(i: &Island) -> bool {
    ["task", "waiting", "result", "failed"].contains(&i.look())
}

/// A job island has something of its own to show besides the stage band: a part of the agent's or the binding's
/// (HTML, a picture, a chart, a question), key values or an image.
pub fn job_has_graphic(i: &Island) -> bool {
    i.parts().iter().any(|q| j::str(q.get("type")) != Some("band")) || !i.facts().is_empty() || i.image().is_some()
}

/// The stage band shows on a job island while it has nothing else to show, or when the user opened its details (`details`,
/// a click on the bottom row).
pub fn job_shows_band(i: &Island) -> bool {
    // A done island has no disclosure, so an opened band does not outlive the run (review, 4 Nov).
    !job_has_graphic(i) || (i.state() != "done" && i.raw.get("details").and_then(Value::as_bool) == Some(true))
}

/// What an open island shows, in order. A job island: the agent's summary, steps, key values and image, its own
/// parts, then one bottom row with the open target's chip on the left and the glance (stage, a live graphic, how
/// long) on the right (the content gets the room). Any other island: its status first when it
/// has nothing else to say, the same content, the open target as a button.
pub fn body_parts(i: &Island) -> Vec<Json> {
    let mut p = vec![];
    let job = is_job(i);
    // One system (`docs/one-system.md`): a job island sent to do something says so once, names its goal, keeps its
    // stage band (its helpers as a count on the fact line) and ends with that line. No status line older than its stage.
    // No line under the title (the user's rules; 3 Nov): the errand stays in the file, the island shows the work.
    if job && crate::model::onesys::sent(i).is_some() {
        if crate::model::onesys::job_goal(i).is_some() {
            p.push(part("goalchip"));
        }
        // The one telling graphic leads (4 Nov: job islands should look better than a bare stage bar); the stage band goes behind the bottom
        // row's disclosure once there is something to show, and stays as the body while there is nothing else.
        let shown = job_shows_band(i);
        for mut q in i.parts() {
            if j::str(q.get("type")) == Some("band") {
                if !shown {
                    continue;
                }
                q.insert("nohelpers".into(), Value::Bool(true));
            }
            p.push(q);
        }
        if !i.facts().is_empty() {
            p.push(part("kv"));
        }
        if let Some(img) = i.image() {
            let mut o = part("image");
            o.insert("path".into(), Value::from(img.clone()));
            o.insert("open".into(), Value::from(img));
            p.push(o);
        }
        let mut f = part("jobfact");
        if let Some(o) = i.open() {
            f.insert("target".into(), Value::from(o));
        }
        p.push(f);
        return p;
    }
    // A plan list draws its own empty state, so it never falls back to the status lines.
    let custom = i.list().is_some()
        || !i.parts().is_empty() || i.summary().is_some() || !i.steps().is_empty() || !i.items().is_empty() || !i.facts().is_empty() || i.image().is_some();
    if !job && !custom {
        p.push(part("status"));
    }
    if let Some(s) = i.summary() {
        let mut o = part("text");
        o.insert("value".into(), Value::from(s));
        p.push(o);
    }
    if !i.steps().is_empty() {
        p.push(part("steps"));
    }
    if i.list().is_some() || i.grouped_items().is_some() {
        p.push(part("todo"));
    } else if !i.items().is_empty() {
        p.push(part("items"));
    }
    if !i.facts().is_empty() {
        p.push(part("kv"));
    }
    if let Some(img) = i.image() {
        let mut o = part("image");
        o.insert("path".into(), Value::from(img.clone()));
        o.insert("open".into(), Value::from(img));
        p.push(o);
    }
    p.extend(i.parts());
    if job {
        let mut f = part("footer");
        if let Some(o) = i.open() {
            f.insert("target".into(), Value::from(o));
        }
        p.push(f);
    } else if let Some(o) = i.open() {
        let mut b = part("openButton");
        b.insert("target".into(), Value::from(o));
        p.push(b);
    }
    p
}

/// "under a minute", "12 min", "1 h 5 min", "3 d".
pub fn spoken(t: f64) -> String {
    let m = (t / 60.0) as i64;
    if m < 1 {
        return "under a minute".into();
    }
    if m < 60 {
        return format!("{m} min");
    }
    if m < 24 * 60 {
        return if m % 60 == 0 { format!("{} h", m / 60) } else { format!("{} h {} min", m / 60, m % 60) };
    }
    format!("{} d", m / (24 * 60))
}

pub fn ago(d: f64, now: f64) -> String {
    if now - d < 60.0 {
        "just now".into()
    } else {
        format!("{} ago", spoken(now - d))
    }
}

/// The status line pair: state and stage, and a clock line (how long since it started or last changed).
pub fn status_lines(i: &Island, born: Option<f64>, written: Option<f64>) -> (String, Box<dyn Fn() -> String>) {
    let look = i.look().to_owned();
    let word = match look.as_str() {
        "failed" => "Failed",
        "waiting" => "Waiting on you",
        "result" => "Done",
        "task" => "Running",
        _ => {
            if i.preset() == "list" {
                "List"
            } else {
                "Note"
            }
        }
    };
    let mut first = vec![word.to_owned()];
    if look == "task" {
        if let Some(p) = i.progress() {
            first.push(format!("{}%", (p * 100.0).round() as i64));
        }
    }
    if look != "result" {
        if let Some(s) = i.stage() {
            first.push(s);
        }
    }
    if look == "result" {
        if let Some(v) = i.value() {
            first.push(v);
        }
    }
    let started = i.started().or(born);
    let updated = i.updated();
    let ended = i.ended().or(if ["result", "failed"].contains(&look.as_str()) { updated.or(written) } else { None });
    let times = move || {
        let now = time::now();
        if ["result", "failed"].contains(&look.as_str()) {
            if let Some(e) = ended {
                let mut t = format!("{}{}", if look == "failed" { "Failed " } else { "Finished " }, ago(e, now));
                if let Some(s) = started {
                    if e - s >= 1.0 {
                        t += &format!(" · took {}", spoken((e - s).max(60.0)));
                    }
                }
                return t;
            }
        }
        let mut bits: Vec<String> = vec![];
        if let Some(s) = started {
            let w = if look == "waiting" { "Asked " } else if look == "task" { "Started " } else { "Added " };
            bits.push(format!("{w}{}", ago(s, now)));
        }
        if let Some(u) = updated {
            if started.map(|s| u - s >= 60.0).unwrap_or(true) {
                bits.push(format!("{}{}", if bits.is_empty() { "Updated " } else { "updated " }, ago(u, now)));
            }
        }
        bits.join(" · ")
    };
    (first.join(" · "), Box::new(times))
}

/// File dates (creation, modification) of an island file, the fallback for writers that leave out the stamps.
pub fn file_dates(path: &Path) -> (Option<f64>, Option<f64>) {
    let Ok(m) = std::fs::metadata(path) else { return (None, None) };
    let secs = |t: std::io::Result<std::time::SystemTime>| t.ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs_f64());
    (secs(m.created()), secs(m.modified()))
}

fn grow_bar(bar: &Retained<CALayer>) -> Box<dyn Fn()> {
    let bar = bar.clone();
    Box::new(move || {
        let w = bar.bounds().size.width;
        let a = Spring::GROW.animation("bounds.size.width", any(&*num(w.min(4.0))), any(&*num(w)), 0.0);
        add(&bar, &a, "grow");
    })
}

/// A slider's bounds from its file: finite (a missing, infinite or NaN bound takes the default, 0 to 1) and low to high.
pub fn slider_bounds(min: Option<f64>, max: Option<f64>) -> (f64, f64) {
    let lo = min.filter(|v| v.is_finite()).unwrap_or(0.0);
    let hi = max.filter(|v| v.is_finite()).unwrap_or(1.0).max(lo);
    (lo, hi)
}

/// Where `v` sits between `lo` and `hi`, 0 to 1. Halved first, so a span as wide as -1e308 to 1e308 (which overflows
/// to infinity) still gives a number; an empty span is 0.
pub fn slider_fraction(lo: f64, hi: f64, v: f64) -> f64 {
    let f = (v / 2.0 - lo / 2.0) / (hi / 2.0 - lo / 2.0);
    if f.is_finite() {
        f.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

/// The value at `t` (0 to 1) between `lo` and `hi`, finite for any finite bounds.
pub fn slider_value(lo: f64, hi: f64, t: f64) -> f64 {
    let t = if t.is_finite() { t.clamp(0.0, 1.0) } else { 0.0 };
    (lo * (1.0 - t) + hi * t).clamp(lo, hi)
}

fn path_of_part(p: &Json) -> Option<String> { str_of(p.get("path")).or_else(|| str_of(p.get("src"))) }

pub fn load_thumbnail(path: &str, max_pixel: usize) -> Option<draw::Image> {
    let p = if path.starts_with("file://") { viewer_path(path) } else { crate::model::file::expand_tilde(path) };
    draw::thumbnail(Path::new(&p), max_pixel, true)
}

/// The most `html` parts one island draws (review, 3 Nov).
pub const HTML_PARTS: usize = 2;

/// The island's `html` parts against the renderer: a fragment whose bitmap is drawn stays (with its cleaned HTML under
/// `clean`); one still drawing is noted in `pending` and, like one the gate refuses or that failed to draw, gives way
/// to its `fallback` parts (the usual slots), so an island never waits on WebKit and a bad payload shows what it
/// always did.
pub fn resolve_html(parts: Vec<Json>, width: f64, scale: f64, pending: &mut Vec<String>) -> Vec<Json> {
    let mut out = Vec::with_capacity(parts.len());
    let mut n = 0;
    for p in parts {
        if j::str(p.get("type")) != Some("html") {
            out.push(p);
            continue;
        }
        // At most `HTML_PARTS` drawn per island: more could evict each other from the renderer's cache forever.
        n += 1;
        let clean = j::str(p.get("html")).filter(|_| n <= HTML_PARTS).and_then(|h| crate::model::html::sanitize(h, &|f| f.is_file()).ok()).map(|c| c.html);
        match clean {
            Some(c) if super::html::cached(&c, width, scale).is_some() => {
                let mut q = p.clone();
                q.insert("clean".into(), Value::from(c));
                q.remove("fallback");
                out.push(q);
                continue;
            }
            Some(c) if super::html::failed(&c, width, scale).is_none() => pending.push(c),
            _ => {}
        }
        out.extend(j::objs(p.get("fallback")).unwrap_or_default().into_iter().filter(|f| j::str(f.get("type")) != Some("html")).cloned());
    }
    out
}

/// The cleaned fragments of an island's `html` parts that still need drawing (the controller draws them ahead, while
/// the island is closed, so opening it never waits).
pub fn html_to_draw(island: &Island, width: f64, scale: f64) -> Vec<String> {
    let mut pending = vec![];
    resolve_html(island.parts(), width, scale, &mut pending);
    pending
}

/// Build the expanded body of an island from its parts. Consecutive buttons form one row.
/// `series`: the numbers the island showed over its rewrites (`model::glance::Series`); `header_ring`: its header's
/// icon is the progress ring (the glance does not repeat it).
#[allow(clippy::too_many_arguments)]
pub fn build_body(
    island: &Island,
    width: f64,
    scale: f64,
    born: Option<f64>,
    written: Option<f64>,
    open_items: &HashSet<String>,
    cal: &Cal,
    series: &Series,
    header_ring: bool,
) -> Body {
    let root = layer();
    root.setAnchorPoint(pt(0.0, 0.0));
    let mut y = 0.0f64;
    let mut body = Body::empty();
    body.layer = root.clone();
    let parts = resolve_html(body_parts(island), width, scale, &mut body.pending_html);
    let w = width;
    let quiet = crate::model::onesys::silent_since(island, time::now()).filter(|_| crate::model::onesys::sent(island).is_some());
    let mut i = 0;
    while i < parts.len() {
        if y > 0.0 {
            y += PART_GAP;
        }
        let p = &parts[i];
        let ty = j::str(p.get("type")).unwrap_or("text");
        match ty {
            "button" => {
                let mut row: Vec<&Json> = vec![];
                while i < parts.len() && j::str(parts[i].get("type")) == Some("button") && row.len() < 3 {
                    row.push(&parts[i]);
                    i += 1;
                }
                i -= 1;
                let (h, gap) = (34.0, 8.0);
                let bw = (w - gap * (row.len() as f64 - 1.0)) / row.len() as f64;
                for (k, b) in row.iter().enumerate() {
                    let label = str_of(b.get("label")).unwrap_or_else(|| "OK".into());
                    let id = j::str(b.get("action")).or(j::str(b.get("id"))).map(str::to_owned).unwrap_or_else(|| label.clone());
                    let chosen = island.chosen();
                    let dim = chosen.as_deref().is_some_and(|c| c != id);
                    let x = k as f64 * (bw + gap);
                    let cap = boxed(x, y, bw, h, &white(if dim { 0.16 } else { 1.0 }), h / 2.0);
                    root.addSublayer(&cap);
                    let t = text1(&label, &button_font(), &*if dim { ink2() } else { black() }, bw - 16.0, scale);
                    let tb = t.bounds().size;
                    t.setPosition(pt(x + (bw - tb.width) / 2.0, y + (h - tb.height) / 2.0));
                    root.addSublayer(&t);
                    if chosen.is_none() {
                        body.regions.push(HitRegion { rect: Rect::new(x, y, bw, h), control: Control::Button { id, label } });
                    }
                }
                y += h;
            }
            "text" => {
                let s = str_of(p.get("value")).or_else(|| str_of(p.get("text"))).unwrap_or_default();
                let lines = j::int(p.get("lines")).map(|n| n.max(0) as usize).unwrap_or(if island.preset() == "note" { 6 } else { 3 });
                let t = text_layer(&s, &body_font(), &ink(), w, lines.max(1), Align::Left, false, scale);
                t.setPosition(pt(0.0, y));
                root.addSublayer(&t);
                y += t.bounds().size.height;
            }
            "stat" => {
                let v = str_of(p.get("value")).unwrap_or_else(|| "–".into());
                let t = text1(&v, &stat_font(), &white(1.0), w * 0.7, scale);
                t.setPosition(pt(0.0, y));
                root.addSublayer(&t);
                let tb = t.bounds().size;
                if let Some(unit) = str_of(p.get("unit")) {
                    let u = text1(&unit, &small_font(), &ink2(), w - tb.width - 6.0, scale);
                    u.setPosition(pt(tb.width + 6.0, y + tb.height - u.bounds().size.height - 4.0));
                    root.addSublayer(&u);
                }
                y += tb.height;
                if let (Some(b), Some(vv)) = (j::obj(p.get("baseline")), num_of(p.get("value"))) {
                    if let Some(bv) = num_of(b.get("value")) {
                        if bv.max(vv) > 0.0 {
                            let label = str_of(b.get("label")).unwrap_or_else(|| "before".into());
                            let bunit = str_of(b.get("unit")).or_else(|| str_of(p.get("unit")).map(|u| format!(" {u}"))).unwrap_or_default();
                            let lt = text1(&format!("{label} {}{bunit}", str_of(b.get("value")).unwrap_or_default()), &small_font(), &ink2(), w * 0.4, scale);
                            let bar_w = w - lt.bounds().size.width - 10.0;
                            let m = bv.max(vv);
                            y += 4.0;
                            let a = boxed(0.0, y, (bar_w * vv / m).max(4.0), 5.0, &white(1.0), 2.5);
                            root.addSublayer(&a);
                            body.arrivals.push(grow_bar(&a));
                            y += 9.0;
                            let bb = boxed(0.0, y, (bar_w * bv / m).max(4.0), 5.0, &white(0.25), 2.5);
                            root.addSublayer(&bb);
                            body.arrivals.push(grow_bar(&bb));
                            lt.setPosition(pt(bar_w + 10.0, y + 2.5 - lt.bounds().size.height / 2.0));
                            root.addSublayer(&lt);
                            y += 5.0;
                        }
                    }
                }
            }
            "bars" => {
                let items: Vec<&Json> = j::objs(p.get("items")).unwrap_or_default().into_iter().take(6).collect();
                let vals: Vec<f64> = items.iter().map(|o| num_of(o.get("value")).unwrap_or(0.0)).collect();
                let vmax = vals.iter().copied().fold(f64::NEG_INFINITY, f64::max);
                let m = num_of(p.get("max")).unwrap_or(0.0).max(if vals.is_empty() { 1.0 } else { vmax }).max(1e-9);
                let unit = str_of(p.get("unit")).map(|u| format!(" {u}")).unwrap_or_default();
                let labels: Vec<_> = items.iter().map(|o| text1(&str_of(o.get("label")).unwrap_or_default(), &small_font(), &ink(), w * 0.36, scale)).collect();
                let values: Vec<_> =
                    items.iter().map(|o| text1(&(str_of(o.get("value")).unwrap_or_default() + &unit), &small_mono(), &white(1.0), w * 0.3, scale)).collect();
                let lw = labels.iter().map(|l| l.bounds().size.width).fold(0.0, f64::max);
                let vw = values.iter().map(|l| l.bounds().size.width).fold(0.0, f64::max);
                let bar_x = lw + 10.0;
                let bar_max = (w - bar_x - vw - 10.0).max(20.0);
                let any_hl = items.iter().any(|o| j::boolean(o.get("highlight")) == Some(true));
                let row_h = 22.0;
                for (k, it) in items.iter().enumerate() {
                    let cy = y + row_h / 2.0;
                    labels[k].setPosition(pt(0.0, cy - labels[k].bounds().size.height / 2.0));
                    root.addSublayer(&labels[k]);
                    let lit = !any_hl || j::boolean(it.get("highlight")) == Some(true);
                    let bar = boxed(bar_x, cy - 4.0, (bar_max * vals[k] / m).max(4.0), 8.0, &white(if lit { 1.0 } else { 0.32 }), 4.0);
                    root.addSublayer(&bar);
                    body.arrivals.push(grow_bar(&bar));
                    values[k].setPosition(pt(w - values[k].bounds().size.width, cy - values[k].bounds().size.height / 2.0));
                    root.addSublayer(&values[k]);
                    y += row_h;
                }
            }
            "line" => {
                let pts: Vec<f64> = j::arr(p.get("points")).map(|a| a.iter().filter_map(|v| num_of(Some(v))).collect()).unwrap_or_default();
                let h = 44.0;
                let last_text = pts.last().map(|v| format_number(*v) + &str_of(p.get("unit")).map(|u| format!(" {u}")).unwrap_or_default());
                let lt = last_text.map(|t| text1(&t, &small_mono(), &white(1.0), w * 0.3, scale));
                let plot_w = w - lt.as_ref().map(|l| l.bounds().size.width + 10.0).unwrap_or(0.0);
                if pts.len() >= 2 {
                    let sp = Spark::draw(&pts, Rect::new(0.0, y, plot_w, h), &SparkStyle::LINE);
                    root.addSublayer(&sp.layer);
                    body.arrivals.push(sp.draw_in());
                    if let Some(lt) = &lt {
                        lt.setPosition(pt(w - lt.bounds().size.width, sp.end.1 - lt.bounds().size.height / 2.0));
                        root.addSublayer(lt);
                    }
                }
                y += h;
            }
            "progress" => {
                let pv = num_of(p.get("value")).unwrap_or(0.0);
                root.addSublayer(&boxed(0.0, y, w, 6.0, &white(0.18), 3.0));
                let f = boxed(0.0, y, (w * pv).max(6.0), 6.0, &white(1.0), 3.0);
                root.addSublayer(&f);
                body.arrivals.push(grow_bar(&f));
                y += 6.0;
            }
            "list" => {
                let items: Vec<&Json> = j::objs(p.get("items")).unwrap_or_default().into_iter().take(6).collect();
                let selected = j::str(p.get("selected")).map(str::to_owned).or_else(|| island.chosen());
                let row_h = 34.0;
                for (k, it) in items.iter().enumerate() {
                    let id = str_of(it.get("id")).or_else(|| str_of(it.get("title"))).unwrap_or_else(|| k.to_string());
                    let title = str_of(it.get("title")).unwrap_or_else(|| id.clone());
                    if k > 0 {
                        root.addSublayer(&boxed(0.0, y, w, 0.5, &white(0.1), 0.0));
                    }
                    let is_sel = selected.as_deref() == Some(id.as_str());
                    let t = text1(&title, &body_font(), &*if selected.is_none() || is_sel { ink() } else { ink2() }, w * 0.62, scale);
                    t.setPosition(pt(0.0, y + (row_h - t.bounds().size.height) / 2.0));
                    root.addSublayer(&t);
                    let mut right = w;
                    if is_sel {
                        let c = sym("checkmark.circle.fill", 16.0, &[white(1.0), green()], scale);
                        c.setPosition(pt(w - 8.0, y + row_h / 2.0));
                        root.addSublayer(&c);
                        right -= 24.0;
                    }
                    if let Some(v) = str_of(it.get("value")) {
                        let vt = text1(&v, &small_mono(), &ink2(), w * 0.34, scale);
                        vt.setPosition(pt(right - vt.bounds().size.width, y + (row_h - vt.bounds().size.height) / 2.0));
                        root.addSublayer(&vt);
                    }
                    body.regions.push(HitRegion { rect: Rect::new(0.0, y, w, row_h), control: Control::ListRow { id, title } });
                    y += row_h;
                }
            }
            "image" => {
                let path = str_of(p.get("path")).or_else(|| str_of(p.get("src")));
                if let Some(img) = path.and_then(|path| load_thumbnail(&path, (w * scale) as usize)) {
                    let aspect = img.height() as f64 / img.width().max(1) as f64;
                    let h = (w * aspect).min(160.0);
                    let iw = h / aspect.max(0.01);
                    let l = boxed((w - iw) / 2.0, y, iw, h, &clear(), 10.0);
                    // SAFETY: a CGImage as contents.
                    unsafe { l.setContents(Some(img.object())) };
                    // SAFETY: a framework constant.
                    l.setContentsGravity(unsafe { objc2_quartz_core::kCAGravityResizeAspectFill });
                    l.setMasksToBounds(true);
                    root.addSublayer(&l);
                    // The picture is clickable with or without `open`: it opens itself, full size, in the viewer.
                    if let Some(o) = crate::model::island::image_part_target(p) {
                        let r = Rect::new((w - iw) / 2.0, y, iw, h);
                        body.regions.push(HitRegion { rect: r, control: Control::Open { target: o, from: Some(r), image: path_of_part(p) } });
                    }
                    y += h;
                } else if y > 0.0 {
                    y -= PART_GAP;
                }
            }
            "toggle" => {
                let id = str_of(p.get("id")).unwrap_or_else(|| "toggle".into());
                let on = j::boolean(p.get("on")).unwrap_or(false);
                let row_h = 26.0;
                let t = text1(&str_of(p.get("label")).unwrap_or_else(|| id.clone()), &body_font(), &white(1.0), w - 50.0, scale);
                t.setPosition(pt(0.0, y + (row_h - t.bounds().size.height) / 2.0));
                root.addSublayer(&t);
                root.addSublayer(&boxed(w - 38.0, y + 3.0, 38.0, 20.0, &*if on { green() } else { white(0.22) }, 10.0));
                root.addSublayer(&boxed(if on { w - 19.0 } else { w - 37.0 }, y + 4.0, 18.0, 18.0, &white(1.0), 9.0));
                body.regions.push(HitRegion { rect: Rect::new(0.0, y, w, row_h), control: Control::Toggle { id, on } });
                y += row_h;
            }
            "slider" => {
                let id = str_of(p.get("id")).unwrap_or_else(|| "slider".into());
                let (lo, hi) = slider_bounds(num_of(p.get("min")), num_of(p.get("max")));
                let v = num_of(p.get("value")).filter(|v| v.is_finite()).unwrap_or(lo).clamp(lo, hi);
                let unit = str_of(p.get("unit")).unwrap_or_default();
                let row_h = 26.0;
                let suffix = if unit.is_empty() { String::new() } else { format!(" {unit}") };
                let vt = text1(&(format_number(hi) + &suffix), &small_mono(), &white(1.0), w * 0.3, scale);
                let track_w = w - vt.bounds().size.width - 14.0;
                let frac = slider_fraction(lo, hi, v);
                root.addSublayer(&boxed(0.0, y + row_h / 2.0 - 2.0, track_w, 4.0, &white(0.2), 2.0));
                let fill = boxed(0.0, y + row_h / 2.0 - 2.0, (track_w * frac).max(4.0), 4.0, &white(1.0), 2.0);
                root.addSublayer(&fill);
                let knob = boxed(track_w * frac - 9.0, y + row_h / 2.0 - 9.0, 18.0, 18.0, &white(1.0), 9.0);
                root.addSublayer(&knob);
                retext_plain(&vt, &(format_number(v) + &suffix), &small_mono(), &white(1.0));
                // SAFETY: a framework constant.
                vt.setAlignmentMode(unsafe { objc2_quartz_core::kCAAlignmentRight });
                vt.setPosition(pt(w - vt.bounds().size.width, y + (row_h - vt.bounds().size.height) / 2.0));
                root.addSublayer(&vt);
                let track = Rect::new(0.0, y, track_w, row_h);
                body.regions.push(HitRegion {
                    rect: track.inset(-9.0, 0.0),
                    control: Control::Slider { id: id.clone(), min: lo, max: hi, step: num_of(p.get("step")).unwrap_or(0.0) },
                });
                body.sliders.insert(id, SliderParts { track, fill, knob, label: Some(vt), unit });
                y += row_h;
            }
            "status" => {
                let (first, times) = status_lines(island, born, written);
                let t = text1(&first, &status_font(), &ink(), w, scale);
                t.setPosition(pt(0.0, y));
                root.addSublayer(&t);
                y += t.bounds().size.height;
                let when = times();
                if !when.is_empty() {
                    y += 2.0;
                    let c = text1(&when, &small_font(), &ink2(), w, scale);
                    let b = c.bounds();
                    c.setBounds(cgr(Rect::new(0.0, 0.0, w, b.size.height)));
                    c.setPosition(pt(0.0, y));
                    root.addSublayer(&c);
                    y += b.size.height;
                    body.clock = Some((c, times));
                }
            }
            "html" => {
                if let Some(d) = j::str(p.get("clean")).and_then(|f| super::html::cached(f, w, scale)) {
                    let l = layer();
                    l.setAnchorPoint(pt(0.0, 0.0));
                    l.setFrame(cgr(Rect::new(0.0, y, d.width, d.height)));
                    // SAFETY: a live CGImage.
                    unsafe { l.setContents(Some(d.image.object())) };
                    l.setContentsScale(scale);
                    l.setContentsGravity(unsafe { objc2_quartz_core::kCAGravityResize });
                    no_implicit(&l);
                    root.addSublayer(&l);
                    y += d.height;
                }
            }
            "band" => {
                if quiet.is_some() {
                    // Not heard from: the band steps back with the rest; the fact line says so.
                    let holder = layer();
                    holder.setAnchorPoint(pt(0.0, 0.0));
                    holder.setFrame(cgr(Rect::new(0.0, 0.0, w, 1.0)));
                    holder.setOpacity(0.55);
                    root.addSublayer(&holder);
                    y = build_band(p, &holder, y - 2.0, w, scale);
                } else {
                    y = build_band(p, &root, y, w, scale);
                }
            }
            "sent" => {
                let t = text_layer(&crate::model::onesys::sent(island).unwrap_or_default(), &system(14.0, Weight::Regular), &white(0.88), w, 2, Align::Left, false, scale);
                t.setPosition(pt(0.0, y + 2.0));
                if quiet.is_some() {
                    t.setOpacity(0.55);
                }
                root.addSublayer(&t);
                y += 2.0 + t.bounds().size.height;
            }
            "goalchip" => {
                if y > 0.0 {
                    y -= PART_GAP - 6.0;
                }
                let ctx = super::world::Ctx::current();
                let gid = crate::model::onesys::job_goal(island).unwrap_or_default();
                match ctx.goal(&gid) {
                    Some(g) => {
                        let sym_name = Some(g.symbol.clone()).filter(|s| symbol_exists(s)).unwrap_or_else(|| "flag".into());
                        let (c, _) = super::marks::chip(&sym_name, &g.title, &system(11.5, Weight::Medium), &white(0.62), &white(0.62), &super::marks::chip_fill(), 22.0, 9.0, w, scale);
                        c.setPosition(pt(0.0, y));
                        if quiet.is_some() {
                            c.setOpacity(0.55);
                        }
                        root.addSublayer(&c);
                        y += 22.0;
                    }
                    None => y -= 6.0,
                }
            }
            "jobfact" => {
                y -= PART_GAP - 6.0;
                y = job_fact(island, p, &root, y, w, scale, born, quiet, &mut body);
            }
            "steps" => {
                let row_h = 26.0;
                for st in island.steps().iter().take(10) {
                    let (s, colors, ink_c) = match st.mark {
                        "done" => ("checkmark.circle.fill", vec![white(1.0), green()], ink_done()),
                        "now" => ("circle.inset.filled", vec![white(1.0)], white(1.0)),
                        "failed" => ("xmark.circle.fill", vec![white(1.0), red()], ink()),
                        _ => ("circle", vec![ink2()], ink()),
                    };
                    let ic = sym(s, 16.0, &colors, scale);
                    ic.setPosition(pt(8.0, y + row_h / 2.0));
                    root.addSublayer(&ic);
                    let t = text1(&st.title, &*if st.mark == "now" { body_strong() } else { body_font() }, &ink_c, w - 26.0, scale);
                    t.setPosition(pt(26.0, y + (row_h - t.bounds().size.height) / 2.0));
                    root.addSublayer(&t);
                    y += row_h;
                }
            }
            "items" => {
                let bf = body_font();
                for (k, s) in island.items().iter().take(12).enumerate() {
                    if k > 0 {
                        y += 4.0;
                    }
                    let t = text_layer(s, &bf, &ink(), w - 18.0, 2, Align::Left, false, scale);
                    let dot = boxed(3.0, y + (bf.ascender() - bf.capHeight() / 2.0).ceil() - 2.5, 5.0, 5.0, &ink2(), 2.5);
                    root.addSublayer(&dot);
                    t.setPosition(pt(18.0, y));
                    root.addSublayer(&t);
                    y += t.bounds().size.height;
                }
            }
            "todo" => {
                y = super::lists::build_plan(island, &root, y, w, scale, open_items, &mut body, cal);
            }
            "timeline" => match crate::model::timeline::Timeline::parse(p) {
                Some(t) => y = build_timeline(&t, &root, y, w, scale, cal, open_items, &mut body),
                None if y > 0.0 => y -= PART_GAP,
                None => {}
            },
            "kv" => {
                // A value that is a share gets a small bar, a number that moved a sparkline, both just left of it
                // (a live graphic wherever the data allows).
                // The graphics stand in one column, left of the widest value that has one.
                let row_h = 22.0;
                let facts: Vec<_> = island.facts().into_iter().take(6).collect();
                let rows: Vec<_> = facts
                    .iter()
                    .map(|(label, value)| (text1(value, &small_mono(), &white(1.0), w * 0.6, scale), kv_mark(value, series.get(&format!("kv:{label}")))))
                    .collect();
                let col = rows.iter().filter(|(_, m)| *m != Mark::None).map(|(v, _)| v.bounds().size.width).fold(0.0, f64::max);
                for ((label, _), (v, mark)) in facts.iter().zip(rows) {
                    let vw = v.bounds().size.width;
                    let cy = y + row_h / 2.0;
                    let mark_w = if mark == Mark::None { 0.0 } else { MARK_W + 8.0 + col - vw };
                    let l = text1(label, &small_font(), &ink2(), w - vw - mark_w - 12.0, scale);
                    l.setPosition(pt(0.0, cy - l.bounds().size.height / 2.0));
                    v.setPosition(pt(w - vw, cy - v.bounds().size.height / 2.0));
                    root.addSublayer(&l);
                    root.addSublayer(&v);
                    if mark != Mark::None {
                        let r = Rect::new(w - col - 8.0 - MARK_W, cy - MARK_H / 2.0, MARK_W, MARK_H);
                        place_mark(&root, &mark, r, series.before(&format!("kv:{label}")), &mut body);
                    }
                    y += row_h;
                }
            }
            "openButton" => {
                let target = str_of(p.get("target")).unwrap_or_default();
                let chip = open_chip(&target, island, w, scale);
                y += chip.place(&root, 0.0, y, &mut body);
            }
            "footer" => {
                y = build_footer(island, p, &root, y, w, scale, born, series, header_ring, &mut body);
            }
            _ => {
                if y > 0.0 {
                    y -= PART_GAP;
                }
            }
        }
        i += 1;
    }
    root.setBounds(cgr(Rect::new(0.0, 0.0, w, y)));
    body.height = y;
    body
}

/// A sent job island's last line: "open turn, 3 helpers" with a clock while it runs (or "not heard from in 22 min"
/// once quiet), how long it has run on the right; done, its report as "This report" (opens in the viewer) and "lands
/// in Settled"; waiting or failed, said in its colour.
#[allow(clippy::too_many_arguments)]
fn job_fact(island: &Island, p: &Json, root: &CALayer, y: f64, w: f64, scale: f64, born: Option<f64>, quiet: Option<f64>, body: &mut Body) -> f64 {
    use crate::model::onesys;
    let h = 22.0;
    let f = system(12.5, Weight::Medium);
    let state = island.state().to_owned();
    let started = island.started().or(born);
    let ended = island.ended().or(island.updated());
    let mid = y + h / 2.0;
    let mut left_x = 0.0;
    let left = |s: &str, c: Retained<NSColor>, symbol: Option<&str>, x: &mut f64| {
        if let Some(sy) = symbol {
            let l = symbol_layer(sy, 11.0, std::slice::from_ref(&c), Weight::Medium, scale);
            l.setPosition(pt(*x + 5.5, mid));
            root.addSublayer(&l);
            *x += 17.0;
        }
        let t = text1(s, &f, &c, w - *x - 90.0, scale);
        t.setPosition(pt(*x, mid - t.bounds().size.height / 2.0));
        root.addSublayer(&t);
    };
    let elapsed = move || started.map(|s| band_len(time::now() - s)).unwrap_or_default();
    match state.as_str() {
        "done" => {
            if let Some(t) = j::str(p.get("target")).map(str::to_owned) {
                let (c, cw) = super::marks::chip("doc.richtext", "This report", &f, &white(1.0), &white(1.0), &super::marks::chip_fill(), h, 10.0, w * 0.6, scale);
                c.setBorderColor(Some(&cg(&rgb(0x0A84FF, 0.6))));
                c.setBorderWidth(1.0);
                c.setPosition(pt(0.0, y));
                root.addSublayer(&c);
                body.regions.push(HitRegion { rect: Rect::new(0.0, y, cw, h), control: Control::Open { target: t, from: Some(Rect::new(0.0, y, cw, h)), image: None } });
                let r = text1("lands in Settled", &f, &rgb(0x30D158, 1.0), w - cw - 10.0, scale);
                r.setPosition(pt(w - r.bounds().size.width, mid - r.bounds().size.height / 2.0));
                root.addSublayer(&r);
                return y + h;
            }
            if let (Some(s), Some(e)) = (started, ended) {
                left(&format!("took {}", band_len(e - s)), white(0.5), Some("checkmark"), &mut left_x);
            }
            return y + h;
        }
        "waiting" => left("waits on you", orange(), Some("hand.raised"), &mut left_x),
        "failed" => left("failed", red(), Some("xmark"), &mut left_x),
        _ => match (quiet, island.summary()) {
            (Some(u), _) => left(&format!("not heard from in {}", onesys::minutes(time::now() - u)), white(0.62), Some("clock"), &mut left_x),
            // The one-line state (4 Nov): the agent's now line, white, beside the clock; a long one steps down a size.
            (None, Some(s)) => {
                let clock = if started.is_some() { text_width(&elapsed(), &f).max(CLOCK_MIN) } else { 0.0 };
                let room = w - clock - if job_has_graphic(island) && state != "done" { CHEVRON_W } else { 0.0 } - 12.0;
                let font = [13.0, 12.0].into_iter().map(|z| system(z, Weight::Regular)).find(|ft| text_width(&s, ft) <= room).unwrap_or_else(|| system(12.0, Weight::Regular));
                let t = text1(&s, &font, &white(0.86), room, scale);
                t.setPosition(pt(0.0, mid - t.bounds().size.height / 2.0));
                root.addSublayer(&t);
            }
            (None, None) => left(&onesys::fact_line(onesys::helpers_working(island)), white(0.5), Some("clock"), &mut left_x),
        },
    }
    // With a graphic, the stage band sits behind this row: a click shows or hides it, the small chevron says which.
    let disclose = job_has_graphic(island) && state != "done";
    let right = if disclose { CHEVRON_W } else { 0.0 };
    if disclose {
        let open = island.raw.get("details").and_then(Value::as_bool) == Some(true);
        let c = symbol_layer(if open { "chevron.up" } else { "chevron.down" }, 9.0, &[white(0.38)], Weight::Semibold, scale);
        c.setPosition(pt(w - CHEVRON_W / 2.0 + 2.0, mid));
        root.addSublayer(&c);
        body.regions.push(HitRegion { rect: Rect::new(0.0, y, w, h), control: Control::Details });
    }
    if started.is_some() {
        let t = text_layer(&elapsed(), &f, &white(0.7), CLOCK_W, 1, Align::Right, false, scale);
        let th = t.bounds().size.height;
        t.setBounds(cgr(Rect::new(0.0, 0.0, CLOCK_W, th)));
        t.setPosition(pt(w - CLOCK_W - right, mid - th / 2.0));
        root.addSublayer(&t);
        body.clock = Some((t, Box::new(elapsed)));
    }
    y + h
}

/// The bottom row's clock box, the least room the now line leaves the clock, and the disclosure's width.
const CLOCK_W: f64 = 64.0;
const CLOCK_MIN: f64 = 40.0;
const CHEVRON_W: f64 = 16.0;

/// A day timeline's colour per kind: Apple's dark-mode system colours.
pub fn timeline_color(kind: &str) -> Retained<NSColor> {
    match kind {
        "build" => rgb(0x0A84FF, 1.0),
        // Neutral greys apart by lightness (3 Nov: no purple or light blue): only build and people keep an accent.
        "talk" => rgb(0xAEAEB2, 1.0),
        "people" => rgb(0x30D158, 1.0),
        "web" => rgb(0x8E8E93, 1.0),
        "play" => rgb(0x636366, 1.0),
        _ => rgb(0x8E8E93, 1.0),
    }
}

/// The `timeline` part: the day ribbon (blocks in their colours on a dim track, a white tick at now, hours under
/// it), a tiny legend naming the colours on it, then a row per block: its start time, a rail in its colour that grows
/// with its length, the title with the length on the right and the line under it. A click on a row with details
/// opens it: the explanation, its tags as capsules, then people, threads, tasks, files and decisions, each behind its
/// symbol. Rows are keyed layers (`row:tl:<start>`), so opening one slides the rest. Nothing moves at rest.
#[allow(clippy::too_many_arguments)]
pub fn build_timeline(t: &crate::model::timeline::Timeline, root: &CALayer, y0: f64, w: f64, scale: f64, cal: &Cal, open_items: &HashSet<String>, body: &mut Body) -> f64 {
    use crate::model::timeline::{kind_name, length, rail_extra};
    let hhmm = |s: f64| {
        let (h, m) = cal.hour_minute(s);
        format!("{h:02}:{m:02}")
    };
    let mut y = y0;
    let (track_h, gap) = (10.0, 2.0);
    let track = boxed(0.0, y, w, track_h, &white(0.09), track_h / 2.0);
    track.setMasksToBounds(true);
    let spans = t.spans(w, 3.0);
    for (k, &(x, bw)) in spans.iter().enumerate() {
        // A hair of track between neighbours keeps them apart.
        let next_gap = spans.get(k + 1).is_some_and(|n| n.0 - (x + bw) < gap);
        let seg = boxed(x, 0.0, (bw - if next_gap { gap } else { 0.0 }).max(2.0), track_h, &timeline_color(&t.blocks[k].kind), 0.0);
        track.addSublayer(&seg);
    }
    root.addSublayer(&track);
    let nx = t.x(t.now, w).clamp(1.0, w - 1.0);
    root.addSublayer(&boxed(nx - 1.0, y - 3.0, 2.0, track_h + 6.0, &white(1.0), 1.0));
    y += track_h + 5.0;
    let hour_font = digits(11.0, Weight::Medium);
    let mut label_h: f64 = 0.0;
    let ticks = t.ticks();
    for (k, s) in ticks.iter().enumerate() {
        let l = text1(&format!("{:02}", cal.hour_minute(*s).0), &hour_font, &ink3(), 40.0, scale);
        let lw = l.bounds().size.width;
        let x = t.x(*s, w);
        let x = if k == 0 { 0.0 } else if k + 1 == ticks.len() { w - lw } else { x - lw / 2.0 };
        l.setPosition(pt(x.clamp(0.0, w - lw), y));
        root.addSublayer(&l);
        label_h = label_h.max(l.bounds().size.height);
    }
    y += label_h;

    // The legend: a dot and a name per colour on the ribbon, quieter than the hours, wrapping on a narrow island.
    let kinds = t.kinds();
    if !kinds.is_empty() {
        y += 7.0;
        let legend_font = system(10.0, Weight::Medium);
        let (dot, dot_gap, item_gap) = (6.0, 5.0, 12.0);
        let mut x = 0.0;
        let mut line_h: f64 = 0.0;
        for k in kinds {
            let l = text1(kind_name(k), &legend_font, &ink3(), w, scale);
            let (lw, lh) = (l.bounds().size.width, l.bounds().size.height);
            if x > 0.0 && x + dot + dot_gap + lw > w {
                x = 0.0;
                y += line_h + 3.0;
            }
            root.addSublayer(&boxed(x, y + (lh - dot) / 2.0, dot, dot, &timeline_color(k), dot / 2.0));
            l.setPosition(pt(x + dot + dot_gap, y));
            root.addSublayer(&l);
            x += dot + dot_gap + lw + item_gap;
            line_h = line_h.max(lh);
        }
        y += line_h;
    }
    if t.blocks.is_empty() {
        // Empty as the other lists are: the graphic, then the list's own symbol, large and dim.
        return super::lists::empty_sign(crate::model::timeline::EMPTY_SYMBOL, root, y + 8.0, w, scale, body);
    }
    y += 14.0;

    // The lists' shared grid (`lists::LEAD_W`, `MARK_CX`, `TEXT_X`): the rail centred on the mark column.
    let (time_w, rail_x, text_x) = (super::lists::LEAD_W, super::lists::MARK_CX - 2.0, super::lists::TEXT_X);
    let text_w = w - text_x;
    let current = t.current();
    let rows = t.rows();
    let first = t.blocks.len() - rows.len();
    for (k, b) in rows.iter().enumerate() {
        if k > 0 {
            y += 10.0;
        }
        let key = b.key();
        let open = b.discloses() && open_items.contains(&key);
        let row = keyed(y, w, 0.0);
        let mut ry = 0.0;
        let len_s = length(b.end - b.start);
        let len = text1(&len_s, &small_mono(), &ink3(), 80.0, scale);
        let lw = len.bounds().size.width;
        let title = text1(&b.title, &body_strong(), &ink(), text_w - lw - 10.0, scale);
        title.setPosition(pt(text_x, ry));
        row.addSublayer(&title);
        let th = title.bounds().size.height;
        len.setPosition(pt(w - lw, ry + (th - len.bounds().size.height) / 2.0 + 0.5));
        row.addSublayer(&len);
        let start = text1(&hhmm(b.start), &small_mono(), &ink2(), time_w, scale);
        start.setPosition(pt(0.0, ry + (th - start.bounds().size.height) / 2.0 + 0.5));
        row.addSublayer(&start);
        ry += th;
        if let Some(line) = &b.line {
            ry += 1.0;
            let l = text1(line, &note_font(), &ink2(), text_w, scale);
            l.setPosition(pt(text_x, ry));
            row.addSublayer(&l);
            ry += l.bounds().size.height;
        }
        let rail_h = (ry - 4.0 + rail_extra((b.end - b.start) / 60.0)).max(14.0);
        let color = timeline_color(&b.kind);
        row.addSublayer(&boxed(rail_x, 2.0, 4.0, rail_h, &color, 2.0));
        let live = current == Some(first + k);
        if live {
            // Still going: the rail ends in a white dot.
            row.addSublayer(&boxed(rail_x - 1.0, 2.0 + rail_h - 6.0, 6.0, 6.0, &white(1.0), 3.0));
        }
        if open {
            ry = timeline_details(&b.meta, &row, ry, text_x, text_w, scale);
        }
        let h = ry.max(2.0 + rail_h);
        row.setBounds(cgr(Rect::new(0.0, 0.0, w, h)));
        root.addSublayer(&row);
        let sig = [b.title.as_str(), b.line.as_deref().unwrap_or(""), &len_s, if open { "o" } else { "" }, if live { "l" } else { "" }, &b.kind].join("\u{1}");
        body.rows.insert(format!("row:{key}"), (row, sig));
        if b.discloses() {
            body.regions.push(HitRegion { rect: Rect::new(0.0, y, w, h), control: Control::Disclose { key } });
        }
        y += h;
    }
    y
}

/// An open timeline row's details from `y` in `row`: the explanation, the tags as small capsules, then each list
/// behind its symbol. Returns the new bottom.
fn timeline_details(m: &crate::model::timeline::Meta, row: &CALayer, y0: f64, x0: f64, w: f64, scale: f64) -> f64 {
    let mut y = y0;
    if let Some(a) = &m.about {
        y += 6.0;
        let t = text_block(a, &note_font(), &ink(), w, scale);
        t.setPosition(pt(x0, y));
        row.addSublayer(&t);
        y += t.bounds().size.height;
    }
    if !m.tags.is_empty() {
        y += 8.0;
        let (h, pad, gap) = (20.0, 8.0, 6.0);
        let f = system(11.0, Weight::Medium);
        let mut x = 0.0;
        for tag in &m.tags {
            let l = text1(tag, &f, &ink2(), w - 2.0 * pad, scale);
            let cw = l.bounds().size.width + 2.0 * pad;
            if x > 0.0 && x + cw > w {
                x = 0.0;
                y += h + gap;
            }
            row.addSublayer(&boxed(x0 + x, y, cw, h, &white(0.08), h / 2.0));
            l.setPosition(pt(x0 + x + pad, y + (h - l.bounds().size.height) / 2.0));
            row.addSublayer(&l);
            x += cw + gap;
        }
        y += h;
    }
    let (icon_w, f) = (20.0, note_font());
    for (symbol, items) in m.lists() {
        y += 7.0;
        let ic = symbol_layer(symbol, 11.0, &[ink3()], Weight::Medium, scale);
        ic.setPosition(pt(x0 + 7.0, y + line_height(&f) / 2.0));
        row.addSublayer(&ic);
        // People and files read as one run; threads and decisions one each, a hair apart, so a wrapped one stays one.
        let runs = if matches!(symbol, "person.2" | "doc") { vec![items.join(", ")] } else { items.to_vec() };
        for (k, text) in runs.iter().enumerate() {
            if k > 0 {
                y += 3.0;
            }
            let t = text_block(text, &f, &ink2(), w - icon_w, scale);
            t.setPosition(pt(x0 + icon_w, y));
            row.addSublayer(&t);
            y += t.bounds().size.height;
        }
    }
    y + 4.0
}

/// A text layer's string replaced with plain attributes (no paragraph style), as Swift's slider label does.
pub fn retext_plain(l: &CATextLayer, s: &str, font: &objc2_app_kit::NSFont, color: &NSColor) {
    let a = attributed(s, &attrs(font, color, None, false));
    // SAFETY: an attributed string for a text layer.
    unsafe { l.setString(Some(any(&*a))) };
}

/// A layer holding one keyed piece of a plan list, placed at `y`; opaque on the island's black, so a row crossing
/// others mid-slide covers them cleanly.
fn keyed(y: f64, width: f64, h: f64) -> Retained<CALayer> {
    let l = layer();
    l.setAnchorPoint(pt(0.0, 0.0));
    l.setFrame(cgr(Rect::new(0.0, y, width, h)));
    set_bg(&l, &black());
    l
}

/// How long something has run, short: "<1 min", "12 min", "1 h 03".
pub fn band_len_short(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    if s >= 3600 {
        format!("{} h {:02}", s / 3600, s % 3600 / 60)
    } else if s >= 60 {
        format!("{} min", s / 60)
    } else {
        "<1 min".into()
    }
}

/// What a plan row shows on its right: a Scheduled item's countdown to its `due` time, else its written `when`.
pub fn when_label(it: &crate::model::PlanItem, list: &str, now: f64, cal: &Cal) -> Option<(String, bool)> {
    if list == "scheduled" {
        if let Some(d) = it.due {
            return Some(Due::label(d, now, cal));
        }
    }
    it.when.clone().map(|w| (w, false))
}

/// Swift's `String.capitalized`: each word's first letter upper case, the rest lower.
pub fn capitalized(s: &str) -> String {
    let mut out = String::new();
    let mut start = true;
    for c in s.chars() {
        if c.is_alphanumeric() {
            if start {
                out.extend(c.to_uppercase());
            } else {
                out.extend(c.to_lowercase());
            }
            start = false;
        } else {
            out.push(c);
            start = true;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 4 Nov: a job island with a graphic folds its stage band behind the bottom row; without one, the band is the body.
    #[test]
    fn a_job_islands_band_folds_behind_its_graphic() {
        let band = crate::jobj! {"type": "band", "code": true, "segments": [{"title": "Reading", "secs": 60.0}]};
        let html = crate::jobj! {"type": "html", "html": "<span class=\"big\">8.5</span>", "islandAgent": true};
        let types = |o: Json| body_parts(&Island::new("j", o)).iter().map(|p| j::str(p.get("type")).unwrap_or("").to_owned()).collect::<Vec<_>>();
        let base = |parts: Vec<Json>, details: bool| crate::jobj! {"title": "Night pass", "state": "running", "sent": "Merge the night's branches", "summary": "Merging the call-state fix",
            "parts": parts.into_iter().map(Value::Object).collect::<Vec<_>>(), "details": details};
        assert_eq!(types(base(vec![band.clone()], false)), ["band", "jobfact"]);
        assert_eq!(types(base(vec![html.clone(), band.clone()], false)), ["html", "jobfact"]);
        assert_eq!(types(base(vec![html.clone(), band.clone()], true)), ["html", "band", "jobfact"]);
        let mut done = base(vec![html, band], true);
        done.insert("state".into(), Value::from("done"));
        assert_eq!(types(done), ["html", "jobfact"]);
    }

    /// Bounds of -1e308 and 1e308 overflow `hi - lo` to infinity, which used to put the knob at NaN.
    #[test]
    fn slider_fraction_is_finite_for_any_bounds() {
        let (lo, hi) = slider_bounds(Some(-1e308), Some(1e308));
        assert!(((hi - lo) / (hi - lo)).is_nan(), "the old formula's span");
        for v in [lo, -1e300, 0.0, 1e300, hi] {
            let f = slider_fraction(lo, hi, v);
            assert!(f.is_finite() && (0.0..=1.0).contains(&f), "{v}: {f}");
        }
        assert_eq!(slider_fraction(lo, hi, hi), 1.0);
        assert_eq!(slider_fraction(lo, hi, 0.0), 0.5);
        for t in [0.0, 0.25, 1.0] {
            let v = slider_value(lo, hi, t);
            assert!(v.is_finite() && (slider_fraction(lo, hi, v) - t).abs() < 1e-12, "{t}: {v}");
        }
        assert_eq!(slider_fraction(0.0, 100.0, 20.0), 0.2);
        assert_eq!(slider_value(0.0, 100.0, 0.2), 20.0);
        // Equal, reversed and not-finite bounds.
        assert_eq!(slider_fraction(5.0, 5.0, 5.0), 0.0);
        assert_eq!(slider_value(0.0, 1.0, f64::NAN), 0.0);
        assert_eq!(slider_bounds(Some(3.0), Some(1.0)), (3.0, 3.0));
        assert_eq!(slider_bounds(Some(f64::NEG_INFINITY), Some(f64::NAN)), (0.0, 1.0));
    }

    #[test]
    fn spoken_and_ago_match_swift() {
        assert_eq!(spoken(30.0), "under a minute");
        assert_eq!(spoken(725.0), "12 min");
        assert_eq!(spoken(3900.0), "1 h 5 min");
        assert_eq!(spoken(7200.0), "2 h");
        assert_eq!(spoken(3.0 * 86400.0), "3 d");
        assert_eq!(ago(100.0, 130.0), "just now");
        assert_eq!(ago(0.0, 725.0), "12 min ago");
    }

    #[test]
    fn a_bare_island_still_shows_its_status() {
        let n = Island::new("n", crate::jobj! {"preset": "note"});
        assert_eq!(body_parts(&n).iter().map(|p| p["type"].as_str().unwrap()).collect::<Vec<_>>(), ["status"]);
        let (first, _) = status_lines(&n, None, None);
        assert_eq!(first, "Note");
        let n = Island::new("n", crate::jobj! {"preset": "note", "parts": [{"type": "text", "value": "a"}], "open": "/x"});
        assert_eq!(body_parts(&n).iter().map(|p| p["type"].as_str().unwrap()).collect::<Vec<_>>(), ["text", "openButton"]);
    }

    #[test]
    fn a_job_islands_status_is_one_bottom_row_after_its_content() {
        // The status block leaves the top; the content gets the room.
        let types = |i: &Island| body_parts(i).iter().map(|p| p["type"].as_str().unwrap().to_owned()).collect::<Vec<_>>();
        let bare = Island::new("x", crate::jobj! {"title": "Male voices", "stage": "Searching"});
        assert_eq!(types(&bare), ["footer"]);
        let full = Island::new("j", crate::jobj! {"title": "Jarvis voice", "state": "running", "stage": "Building page", "progress": 0.4,
            "summary": "Cleaning the takes", "steps": [{"title": "Cut", "state": "done"}], "kv": [{"label": "Takes", "value": "7/10"}],
            "parts": [{"type": "line", "points": [1, 2]}], "open": "/x.html"});
        assert_eq!(types(&full), ["text", "steps", "kv", "line", "footer"], "no status on top, no separate progress bar or open button");
        assert_eq!(body_parts(&full).last().unwrap()["target"], "/x.html", "the open chip sits in the bottom row");
        for state in ["waiting", "done", "failed"] {
            let i = Island::new("s", crate::jobj! {"title": "T", "state": state, "summary": "s"});
            assert_eq!(types(&i), ["text", "footer"], "{state}");
        }
    }

    #[test]
    fn each_stage_is_named_once_under_the_band() {
        let segs = band_segments(vec![("Reading".into(), 5.0), ("Reading".into(), 5.0), ("Researching".into(), 9.0), ("Reading".into(), 3.0)]);
        assert_eq!(segs, [("Reading".to_string(), 10.0), ("Researching".to_string(), 9.0), ("Reading".to_string(), 3.0)]);
        assert_eq!(band_named(&segs), [false, true, true], "the first Reading stretch is not named again");
    }

    #[test]
    fn a_sent_job_island_draws_no_line_under_its_title() {
        let i = Island::new("j", crate::jobj! {"title": "Climb", "state": "running", "sent": "Climb the README to 8.5", "parts": [{"type": "band", "code": true, "segments": []}]});
        let types: Vec<String> = body_parts(&i).iter().map(|p| p["type"].as_str().unwrap().to_owned()).collect();
        assert!(!types.contains(&"sent".to_string()), "{types:?}");
    }

    #[test]
    fn an_html_part_not_drawn_yet_or_refused_shows_its_fallback() {
        let fb = serde_json::json!([{"type": "stat", "value": "6.09"}]);
        let parts = vec![
            crate::jobj! {"type": "html", "html": "<p>6.09</p>", "fallback": fb.clone()},
            crate::jobj! {"type": "html", "html": "<script>x</script>", "fallback": fb},
            crate::jobj! {"type": "band"},
        ];
        let mut pending = vec![];
        let out = resolve_html(parts, 328.0, 2.0, &mut pending);
        assert_eq!(out.iter().map(|p| p["type"].as_str().unwrap()).collect::<Vec<_>>(), ["stat", "stat", "band"]);
        assert_eq!(pending, ["<p>6.09</p>"], "only the clean one is drawn");
        let many: Vec<Json> = (0..5).map(|k| crate::jobj! {"type": "html", "html": format!("<p>{k}</p>")}).collect();
        let mut pending = vec![];
        resolve_html(many, 328.0, 2.0, &mut pending);
        assert_eq!(pending.len(), HTML_PARTS, "at most two drawn per island");
    }

    #[test]
    fn capitalized_like_foundation() {
        assert_eq!(capitalized("yes"), "Yes");
        assert_eq!(capitalized("not-now"), "Not-Now");
    }

    #[test]
    fn a_closed_rows_detail_line_is_one_line_cut_with_an_ellipsis_at_the_row_width() {
        let short = text_layer("Converting weights", &note_font(), &ink2(), 120.0, 1, Align::Left, false, 2.0);
        assert_eq!(text_of(&short).as_deref(), Some("Converting weights"));
        let long = "The first line of a detail that runs well past the width of a closed row, so it has to be cut short";
        let cut = text_layer(long, &note_font(), &ink2(), 120.0, 1, Align::Left, false, 2.0);
        let shown = text_of(&cut).unwrap();
        assert!(shown.ends_with('…') && shown.len() < long.len(), "{shown}");
        assert!(cut.bounds().size.width <= 120.0 && cut.bounds().size.height <= line_height(&note_font()), "{shown}");
    }
}

/// The band (round 3 of the island design, Heimdall's code writes it as a `band` part on a thread's job island): helper
/// rows (a lit white dot for one working now, a dim tick for a finished one, names only), then the stage ribbon, one
/// segment per stage, widths by time with a floor wide enough for each name. Neutral: past segments a dim
/// grey, named under them; the current one the label white with a soft glow, named with its length; orange only while
/// the user must answer (`state: waiting`), red when failed, a pause a hollow outline; a done ribbon dims and ends in a
/// green check. Native layers, so it stays crisp; nothing moves at rest.
pub fn build_band(p: &Json, root: &CALayer, y0: f64, w: f64, scale: f64) -> f64 {
    let state = j::str(p.get("state")).unwrap_or("running");
    let done = state == "done";
    let mut y = y0;
    // A job island sent to do something counts its helpers on its fact line instead.
    let helpers: Vec<&Json> = if j::boolean(p.get("nohelpers")) == Some(true) { vec![] } else { j::objs(p.get("helpers")).unwrap_or_default().into_iter().take(4).collect() };
    let lit = ink();
    for h in &helpers {
        let title = str_of(h.get("title")).unwrap_or_default();
        let cy = y + 13.0;
        if j::str(h.get("state")) == Some("now") {
            let dot = boxed(4.0, cy - 4.0, 8.0, 8.0, &lit, 4.0);
            glow_layer(&dot, &lit);
            root.addSublayer(&dot);
            let t = text1(&title, &system(14.0, Weight::Semibold), &ink(), w - 24.0, scale);
            t.setPosition(pt(22.0, cy - t.bounds().size.height / 2.0));
            root.addSublayer(&t);
        } else {
            let tick = sym("checkmark", 10.0, &[white(0.4)], scale);
            tick.setPosition(pt(8.0, cy));
            root.addSublayer(&tick);
            let t = text1(&title, &system(13.0, Weight::Regular), &white(0.4), w - 24.0, scale);
            t.setPosition(pt(22.0, cy - t.bounds().size.height / 2.0));
            root.addSublayer(&t);
        }
        y += 26.0;
    }
    if !helpers.is_empty() {
        y += 8.0;
    }
    let segs = band_segments(j::objs(p.get("segments")).unwrap_or_default().into_iter()
        .map(|o| (str_of(o.get("title")).unwrap_or_default(), num_of(o.get("secs")).unwrap_or(0.0).max(0.0))).collect());
    let named = band_named(&segs);
    if segs.is_empty() {
        return if helpers.is_empty() { y0 } else { y - 8.0 };
    }
    let (rib_h, gap) = (6.0, 3.0);
    let (f_cur, f_len, f_past) = (system(12.0, Weight::Semibold), system(11.0, Weight::Regular), system(11.0, Weight::Regular));
    let last = segs.len() - 1;
    let cur_len = band_len(segs[last].1);
    let cur_name = text1(&segs[last].0, &f_cur, &ink(), w, scale);
    // A failure has no length worth saying: its name alone.
    let cur_time = text1(if segs[last].0 == "Failed" { String::new() } else { format!("  {cur_len}") }.as_str(), &f_len, &ink2(), w, scale);
    let labels: Vec<Retained<CATextLayer>> = segs.iter().enumerate()
        .map(|(i, (n, _))| if i == last && !done { cur_name.clone() } else { text1(if named[i] { n } else { "" }, &f_past, &*if done { ink2() } else { white(0.4) }, w, scale) }).collect();
    let need: Vec<f64> = labels.iter().enumerate()
        .map(|(i, l)| if i == last && !done { l.bounds().size.width + cur_time.bounds().size.width + 4.0 } else if named[i] { l.bounds().size.width + 8.0 } else { 8.0 }).collect();
    let cap = if done { 20.0 } else { 0.0 };
    let avail = (w - cap - gap * last as f64).max(10.0);
    let total = segs.iter().map(|s| s.1).sum::<f64>().max(1.0);
    let mut ws: Vec<f64> = segs.iter().zip(&need).map(|(s, q)| q.max(s.1 / total * avail)).collect();
    for _ in 0..8 {
        let over = ws.iter().sum::<f64>() - avail;
        if over <= 0.01 { break }
        let flex: Vec<f64> = ws.iter().zip(&need).map(|(a, q)| a - q).collect();
        let fs: f64 = flex.iter().filter(|f| **f > 0.0).sum();
        if fs <= 0.0 { let k = avail / ws.iter().sum::<f64>(); ws.iter_mut().for_each(|a| *a *= k); break }
        for (a, f) in ws.iter_mut().zip(&flex) { if *f > 0.0 { *a -= over * f / fs } }
    }
    let ly = y + rib_h + 7.0;
    let mut x = 0.0;
    for (i, ((name, _), sw)) in segs.iter().zip(&ws).enumerate() {
        let cur = i == last && !done;
        // The band in neutrals ("stick to the default, with just some accent colours when something is
        // more important"): the stage in hand bright, the stages behind it dim; colour only for a wait or a failure.
        let hue = if cur && state == "waiting" { orange() } else if cur && (state == "failed" || name == "Failed") { red() } else { stage_colour(name) };
        if cur && name == "Paused" {
            let seg = boxed(x, y, *sw, rib_h, &clear(), rib_h / 2.0);
            seg.setBorderColor(Some(&ink2().CGColor()));
            seg.setBorderWidth(1.4);
            root.addSublayer(&seg);
        } else if cur {
            let seg = boxed(x, y, *sw, rib_h, &hue, rib_h / 2.0);
            glow_layer(&seg, &hue);
            root.addSublayer(&seg);
        } else {
            let past = if name == "Failed" { red() } else { hue };
            // Behind the stage in hand: a dim neutral, never a second bright colour.
            let past = if name == "Failed" { past } else { white(if done { 0.34 } else { 0.24 }) };
            root.addSublayer(&boxed(x, y, *sw, rib_h, &past, rib_h / 2.0));
        }
        let l = &labels[i];
        l.setPosition(pt(x, ly));
        root.addSublayer(l);
        if cur {
            cur_time.setPosition(pt(x + l.bounds().size.width, ly + l.bounds().size.height - cur_time.bounds().size.height));
            root.addSublayer(&cur_time);
        }
        x += sw + gap;
    }
    if done {
        let check = sym("checkmark.circle.fill", 15.0, &[black(), green()], scale);
        check.setPosition(pt(w - 8.0, y + rib_h / 2.0));
        root.addSublayer(&check);
    }
    ly + cur_name.bounds().size.height
}

/// The band's segments with a stage that runs on in the next one merged (Reading, Reading is one Reading).
pub fn band_segments(segs: Vec<(String, f64)>) -> Vec<(String, f64)> {
    let mut out: Vec<(String, f64)> = Vec::with_capacity(segs.len());
    for (n, t) in segs {
        match out.last_mut() {
            Some(l) if l.0 == n => l.1 += t,
            _ => out.push((n, t)),
        }
    }
    out
}

/// Which segments are named under the band: each stage once, where it was last (the current one always), so a job
/// that went back to Reading shows "Reading" once, not under every Reading stretch (bug, 3 Nov).
pub fn band_named(segs: &[(String, f64)]) -> Vec<bool> {
    (0..segs.len()).map(|i| i + 1 == segs.len() || !segs[i + 1..].iter().any(|s| s.0 == segs[i].0)).collect()
}

/// A stage's colour on the band: every working stage the label white, drawn bright while in hand and dim
/// behind it; a wait orange, a pause grey, a failure red.
pub fn stage_colour(name: &str) -> Retained<NSColor> {
    match name {
        "Your answer" => orange(),
        "Paused" => rgb(0x78787E, 1.0),
        "Failed" => red(),
        _ => ink(),
    }
}

/// "<1 min", "12 min", "1 h 03".
pub fn band_len(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    if s >= 3600 { format!("{} h {:02}", s / 3600, s % 3600 / 60) } else if s >= 60 { format!("{} min", s / 60) } else { "<1 min".into() }
}

/// A soft glow of `c` around a band layer (its shadow; static, nothing animates).
fn glow_layer(l: &CALayer, c: &NSColor) {
    l.setShadowColor(Some(&c.CGColor()));
    l.setShadowOpacity(0.85);
    l.setShadowRadius(4.0);
    l.setShadowOffset(objc2_core_foundation::CGSize { width: 0.0, height: 0.0 });
}
