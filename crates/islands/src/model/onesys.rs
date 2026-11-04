//! One system (`docs/one-system.md`): the facts Goals, Scheduled, Settled and the job islands draw,
//! read from their files. Nothing enters Goals or Scheduled without the user's yes, so what Heimdall wants there is a
//! proposal: one shown per list (the front), the rest "+N", each archived 48 h after it first showed. Running comes
//! from job islands naming a goal; Scheduled's guarantee line says nothing waits, what waits, or that it is not sure;
//! Settled counts only what has proof. Every label that reads the clock says when it next changes, so one one-shot
//! timer redraws it and nothing runs at rest.

use std::collections::HashMap;

use serde_json::Value;

use super::due::{Cal, Due};
use super::goals::{Goal, GoalState, TaskState};
use super::plan::item_id;
use super::{j, Island, Json};

/// A proposal nobody answered leaves the list this long after it first showed.
pub const PROPOSAL_TTL: f64 = 48.0 * 3600.0;
/// Heimdall's sweep older than this reads "Not sure".
pub const SWEEP_STALE: f64 = 30.0 * 60.0;
/// A running job island not heard from for this long dims and says so.
pub const SILENT: f64 = 15.0 * 60.0;
/// Scheduled's week strip spans this many hours.
pub const WEEK_H: f64 = 168.0;

// MARK: Proposals

/// One entry of a list's `proposals` queue.
#[derive(Clone, Debug, PartialEq)]
pub struct Proposal {
    pub id: String,
    pub title: String,
    pub why: Option<String>,
    /// Goals: "goal" (a new goal) or "step" (a next step under `goal`).
    pub kind: Option<String>,
    pub goal: Option<String>,
    pub icon: Option<String>,
    /// Scheduled: when it would happen.
    pub due: Option<f64>,
    /// When the user first saw it (the app's `seen`, stamped by Heimdall); absent, it cannot expire.
    pub shown: Option<f64>,
}

impl Proposal {
    pub fn live(&self, now: f64) -> bool {
        self.shown.is_none_or(|s| now - s < PROPOSAL_TTL)
    }
    pub fn is_goal(&self) -> bool {
        self.kind.as_deref() == Some("goal")
    }
}

/// The queue in file order (entries without an id or a title are skipped).
pub fn proposals(raw: &Json) -> Vec<Proposal> {
    j::objs(raw.get("proposals"))
        .unwrap_or_default()
        .into_iter()
        .filter_map(|o| {
            let title = j::nonempty(o.get("title"))?.trim().to_owned();
            let id = item_id(o)?;
            (!title.is_empty()).then(|| Proposal {
                id,
                title,
                why: j::nonempty(o.get("why")).map(str::to_owned),
                kind: j::nonempty(o.get("kind")).map(str::to_lowercase),
                goal: j::nonempty(o.get("goal")).map(str::to_owned),
                icon: j::nonempty(o.get("icon")).map(str::to_owned),
                due: Due::seconds(o.get("due")),
                shown: Due::seconds(o.get("shown")),
            })
        })
        .collect()
}

/// The front proposal (the first not past 48 h since it showed) and how many live ones queue behind it ("+N").
pub fn front(raw: &Json, now: f64) -> Option<(Proposal, usize)> {
    let live: Vec<Proposal> = proposals(raw).into_iter().filter(|p| p.live(now)).collect();
    let mut it = live.into_iter();
    let f = it.next()?;
    Some((f, it.count()))
}

/// The next moment a shown proposal expires (the front changes or "+N" drops).
pub fn next_expiry(raw: &Json, now: f64) -> Option<f64> {
    proposals(raw).iter().filter(|p| p.live(now)).filter_map(|p| p.shown.map(|s| s + PROPOSAL_TTL)).fold(None, min_opt)
}

fn min_opt(a: Option<f64>, b: f64) -> Option<f64> {
    Some(a.map_or(b, |a| a.min(b)))
}

// MARK: Goals and the job islands that run their work

/// A goal as the other lists name it: its id, title and symbol.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct GoalRef {
    pub id: String,
    pub title: String,
    pub symbol: String,
}

/// Every goal of a Goals file by id (any state: a finished goal still names Settled's rows).
pub fn goal_refs(goals_raw: &Json) -> Vec<GoalRef> {
    super::Goals::parse(goals_raw).into_iter().map(|g| GoalRef { id: g.id.clone().unwrap_or_else(|| g.key.clone()), title: g.title.clone(), symbol: g.symbol() }).collect()
}

/// A job island's goal, when it names one.
pub fn job_goal(i: &Island) -> Option<String> {
    j::nonempty(i.raw.get("goal")).map(str::to_owned)
}

/// Running job islands per goal id: a goal's work is lit blue only while an island of it runs.
pub fn running_by_goal<'a>(islands: impl IntoIterator<Item = &'a Island>) -> HashMap<String, usize> {
    let mut m = HashMap::new();
    for i in islands {
        // A job gone silent is not sure, so it lights nothing (review 2 Nov).
        if i.list().is_some() || i.state() != "running" || !matches!(i.look(), "task" | "waiting") || silent_since(i, crate::model::time::now()).is_some() {
            continue;
        }
        if let Some(g) = job_goal(i) {
            *m.entry(g).or_insert(0) += 1;
        }
    }
    m
}

/// A segment of a goal's bar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Seg {
    Done,
    Running,
    Queued,
    Proposed,
}

/// What one active goal draws: its bar, its ring's shares and its queued lines.
#[derive(Clone, Debug, PartialEq)]
pub struct GoalView {
    pub segs: Vec<Seg>,
    pub running: usize,
    /// The ring: done share and running share (of done + running + queued + the front step).
    pub done_share: f64,
    pub run_share: f64,
    /// Queued task titles: `next`, and `now` with no running island behind it.
    pub queued: Vec<String>,
}

pub fn goal_view(g: &Goal, running: usize, front_step: bool) -> GoalView {
    let done = g.tasks.iter().filter(|t| t.state == TaskState::Done).count();
    // `now` tasks backed by a running island are the blue segments; the rest wait like `next` ones.
    let mut backed = running;
    let mut queued = vec![];
    for t in &g.tasks {
        match t.state {
            TaskState::Now if backed > 0 => backed -= 1,
            TaskState::Now | TaskState::Next => queued.push(t.title.clone()),
            TaskState::Done => {}
        }
    }
    let paused = g.state == GoalState::Paused;
    let running = if paused { 0 } else { running };
    let mut segs = vec![Seg::Done; done];
    segs.extend(std::iter::repeat_n(Seg::Running, running));
    segs.extend(std::iter::repeat_n(Seg::Queued, queued.len()));
    if front_step {
        segs.push(Seg::Proposed);
    }
    if segs.is_empty() {
        segs.push(Seg::Queued);
    }
    let total = (done + running + queued.len() + front_step as usize).max(1) as f64;
    GoalView { segs, running, done_share: done as f64 / total, run_share: running as f64 / total, queued }
}

// MARK: Scheduled's guarantee line

#[derive(Clone, Debug, PartialEq)]
pub struct Choice {
    pub label: String,
    pub action: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Waiting {
    pub id: String,
    pub title: String,
    pub line: Option<String>,
    pub goal: Option<String>,
    pub choices: Vec<Choice>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Unsure {
    pub id: String,
    pub title: String,
    pub line: Option<String>,
    pub since: Option<f64>,
}

impl Unsure {
    /// "Arsenal picks not heard from in 40 min" (with `since`), else its own line.
    pub fn text(&self, now: f64) -> String {
        match self.since {
            Some(s) => format!("{} not heard from in {}", self.title, minutes(now - s)),
            None => self.line.clone().unwrap_or_else(|| self.title.clone()),
        }
    }
}

/// What the guarantee line says.
#[derive(Clone, Debug, PartialEq)]
pub enum Guarantee {
    /// "Nothing waiting on you · checked 2 min ago".
    Quiet { checked: f64 },
    /// The first waiting item, its choices, and how many more wait.
    Waiting { first: Waiting, more: usize },
    /// "Not sure": a source is silent.
    Unsure { first: Unsure, more: usize },
    /// "Not sure": no sweep, or the last one is older than 30 min (`checked` then says how old).
    Stale { checked: Option<f64> },
}

pub fn guarantee(sched_raw: &Json, now: f64) -> Guarantee {
    let Some(g) = j::obj(sched_raw.get("guarantee")) else { return Guarantee::Stale { checked: None } };
    let checked = Due::seconds(g.get("checked"));
    let Some(checked) = checked.filter(|c| now - c.min(now) <= SWEEP_STALE) else { return Guarantee::Stale { checked } };
    let s = |o: &Json, k: &str| j::nonempty(o.get(k)).map(|s| s.trim().to_owned()).filter(|s| !s.is_empty());
    let waiting: Vec<Waiting> = j::objs(g.get("waiting"))
        .unwrap_or_default()
        .into_iter()
        .filter_map(|o| {
            Some(Waiting {
                title: s(o, "title")?,
                id: item_id(o).unwrap_or_default(),
                line: s(o, "line"),
                goal: s(o, "goal"),
                choices: j::objs(o.get("choices"))
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|c| {
                        let label = s(c, "label")?;
                        Some(Choice { action: s(c, "action").unwrap_or_else(|| label.to_lowercase()), label })
                    })
                    .take(3)
                    .collect(),
            })
        })
        .collect();
    if let Some(first) = waiting.first() {
        return Guarantee::Waiting { first: first.clone(), more: waiting.len() - 1 };
    }
    let unsure: Vec<Unsure> = j::objs(g.get("unsure"))
        .unwrap_or_default()
        .into_iter()
        .filter_map(|o| Some(Unsure { title: s(o, "title")?, id: item_id(o).unwrap_or_default(), line: s(o, "line"), since: Due::seconds(o.get("since")) }))
        .collect();
    if let Some(first) = unsure.first() {
        return Guarantee::Unsure { first: first.clone(), more: unsure.len() - 1 };
    }
    Guarantee::Quiet { checked }
}

/// "2 min", "1 h", "3 d": how long, for the guarantee line and the job islands.
pub fn minutes(secs: f64) -> String {
    let m = (secs.max(0.0) / 60.0) as i64;
    if m < 60 {
        format!("{} min", m.max(1))
    } else if m < 48 * 60 {
        format!("{} h", m / 60)
    } else {
        format!("{} d", m / (24 * 60))
    }
}

/// "checked just now", "checked 2 min ago".
pub fn checked_label(checked: f64, now: f64) -> String {
    if now - checked < 60.0 {
        "checked just now".into()
    } else {
        format!("checked {} ago", minutes(now - checked))
    }
}

/// The next minute boundary of an age counted from `since` (labels in whole minutes change then).
pub fn next_minute(since: f64, now: f64) -> f64 {
    let age = (now - since).max(0.0);
    since + ((age / 60.0).floor() + 1.0) * 60.0
}

/// When the guarantee line next reads differently.
pub fn guarantee_next(sched_raw: &Json, now: f64) -> Option<f64> {
    match guarantee(sched_raw, now) {
        Guarantee::Quiet { checked } => Some(next_minute(checked, now).min(checked + SWEEP_STALE + 1.0)),
        Guarantee::Stale { checked: Some(c) } => Some(next_minute(c, now)),
        Guarantee::Stale { checked: None } => None,
        Guarantee::Unsure { first, .. } => {
            let checked = j::obj(sched_raw.get("guarantee")).and_then(|g| Due::seconds(g.get("checked"))).map(|c| c + SWEEP_STALE + 1.0);
            let label = first.since.map(|s| next_minute(s, now));
            [checked, label].into_iter().flatten().fold(None, min_opt)
        }
        Guarantee::Waiting { .. } => j::obj(sched_raw.get("guarantee")).and_then(|g| Due::seconds(g.get("checked"))).map(|c| c + SWEEP_STALE + 1.0),
    }
}

// MARK: Scheduled's rows and week strip

/// A promise's lead: "Now" once due, "10:00" today, "Tue 10:00" within the week, then "12 Nov".
pub fn sched_lead(due: f64, now: f64, cal: &Cal) -> String {
    if due <= now {
        return "Now".into();
    }
    let (h, m) = cal.hour_minute(due);
    match cal.days(now, due) {
        n if n <= 0 => format!("{h:02}:{m:02}"),
        1..=6 => format!("{} {h:02}:{m:02}", cal.weekday(due)),
        _ => cal.day_month(due),
    }
}

/// When a lead next reads differently: when it falls due, or at the next local midnight.
pub fn sched_lead_next(due: f64, now: f64, cal: &Cal) -> Option<f64> {
    (due > now).then(|| due.min(cal.add_days(1, cal.start_of_day(now)).unwrap_or(now + 86400.0)))
}

/// Where a moment sits on the week strip, `w` wide: hours from now, linear, 6 pt in from each end.
pub fn strip_x(t: f64, now: f64, w: f64) -> f64 {
    let h = ((t - now) / 3600.0).clamp(0.0, WEEK_H);
    6.0 + (w - 12.0) * h / WEEK_H
}

/// The strip's labels: Now, then the weekdays two, four and seven days on, at their hours.
pub fn strip_labels(now: f64, cal: &Cal) -> Vec<(String, f64)> {
    vec![("Now".into(), 0.0), (cal.weekday(now + 48.0 * 3600.0), 48.0), (cal.weekday(now + 96.0 * 3600.0), 96.0), (cal.weekday(now + WEEK_H * 3600.0), WEEK_H)]
}

/// The promises Scheduled draws: items with a `due`, not done, by time.
pub fn promises(sched_raw: &Json) -> Vec<super::PlanItem> {
    let mut v: Vec<_> = super::Plan::items(sched_raw).into_iter().filter(|i| i.due.is_some() && !i.done).collect();
    v.sort_by(|a, b| a.due.partial_cmp(&b.due).unwrap_or(std::cmp::Ordering::Equal));
    v
}

// MARK: Settled: proof, claims, the slate

#[derive(Clone, Debug, PartialEq)]
pub struct Proof {
    pub kind: String,
    pub value: String,
    pub label: Option<String>,
}

impl Proof {
    /// The chip's symbol for a kind of proof.
    pub fn symbol(kind: &str) -> &'static str {
        match kind {
            "commit" => "arrow.triangle.branch",
            "test" => "testtube.2",
            "file" => "doc.richtext",
            "install" => "arrow.down.app",
            "quote" => "quote.opening",
            _ => "checkmark.seal",
        }
    }
}

/// A settled item: proved (`proof`) or only claimed.
#[derive(Clone, Debug, PartialEq)]
pub struct SettledItem {
    pub key: String,
    pub title: String,
    pub at: f64,
    pub goal: Option<String>,
    pub proof: Option<Proof>,
}

pub fn settled_items(raw: &Json, now: f64) -> Vec<SettledItem> {
    let mut out = vec![];
    let mut seen = std::collections::HashSet::new();
    for o in j::objs(raw.get("items")).unwrap_or_default() {
        let Some(title) = j::nonempty(o.get("title")).map(str::trim).filter(|t| !t.is_empty()) else { continue };
        let Some(at) = Due::seconds(o.get("at")).map(|a| if a > 1e11 { a / 1000.0 } else { a }) else { continue };
        let mut key = item_id(o).unwrap_or_else(|| title.to_owned());
        let mut n = 2;
        while !seen.insert(key.clone()) {
            key = format!("{key}#{n}");
            n += 1;
        }
        let proof = j::obj(o.get("proof")).and_then(|p| {
            let kind = j::nonempty(p.get("kind"))?.to_lowercase();
            Some(Proof { kind, value: j::str(p.get("value")).unwrap_or_default().to_owned(), label: j::nonempty(p.get("label")).map(str::to_owned) })
        });
        out.push(SettledItem { key, title: title.to_owned(), at: at.min(now), goal: j::nonempty(o.get("goal")).map(str::to_owned), proof });
    }
    out.sort_by(|a, b| a.at.partial_cmp(&b.at).unwrap_or(std::cmp::Ordering::Equal));
    out
}

/// Settled's count: proved items since the slate while it is under a week old, else this week's.
#[derive(Clone, Debug, PartialEq)]
pub struct SettledCount {
    pub proved: usize,
    pub claimed: usize,
    /// "since the clean slate" (true) or "this week".
    pub since_slate: bool,
}

pub fn slate(raw: &Json) -> Option<f64> {
    Due::seconds(raw.get("slate"))
}

/// The seven columns' days (local midnights, oldest first; the last is today).
pub fn week_starts(now: f64, cal: &Cal) -> [f64; 7] {
    let today = cal.start_of_day(now);
    let mut s = [today; 7];
    for (k, d) in s.iter_mut().enumerate() {
        let back = (6 - k) as i64;
        *d = cal.add_days(-back, today).map(|t| cal.start_of_day(t + 3600.0)).unwrap_or(today - back as f64 * 86400.0);
    }
    s
}

pub fn settled_count(items: &[SettledItem], slate: Option<f64>, now: f64, cal: &Cal) -> SettledCount {
    let since_slate = slate.is_some_and(|s| now - s < 7.0 * 86400.0);
    let from = if since_slate { slate.unwrap_or(0.0) } else { week_starts(now, cal)[0].max(slate.unwrap_or(0.0)) };
    let shown = items.iter().filter(|i| i.at >= from);
    let (p, c): (Vec<&SettledItem>, Vec<&SettledItem>) = shown.partition(|i| i.proof.is_some());
    SettledCount { proved: p.len(), claimed: c.len(), since_slate }
}

/// One day column: its proved and claimed items (oldest first), and whether it is before the slate.
#[derive(Clone, Debug, PartialEq)]
pub struct DayColumn {
    pub start: f64,
    pub proved: usize,
    pub claimed: usize,
    pub before_slate: bool,
}

pub fn day_columns(items: &[SettledItem], slate: Option<f64>, now: f64, cal: &Cal) -> Vec<DayColumn> {
    let starts = week_starts(now, cal);
    (0..7)
        .map(|d| {
            let end = if d == 6 { f64::INFINITY } else { starts[d + 1] };
            let day: Vec<&SettledItem> = items.iter().filter(|i| i.at >= starts[d] && i.at < end && slate.is_none_or(|s| i.at >= s)).collect();
            DayColumn {
                start: starts[d],
                proved: day.iter().filter(|i| i.proof.is_some()).count(),
                claimed: day.iter().filter(|i| i.proof.is_none()).count(),
                before_slate: slate.is_some_and(|s| end <= s),
            }
        })
        .collect()
}

// MARK: Job islands

/// What a job island was sent to do (written once).
pub fn sent(i: &Island) -> Option<String> {
    j::nonempty(i.raw.get("sent")).map(|s| s.split_whitespace().collect::<Vec<_>>().join(" ")).filter(|s| !s.is_empty())
}

/// Helpers working now, from the `band` part.
pub fn helpers_working(i: &Island) -> usize {
    i.parts()
        .iter()
        .filter(|p| j::str(p.get("type")) == Some("band"))
        .flat_map(|p| j::objs(p.get("helpers")).unwrap_or_default())
        .filter(|h| j::str(h.get("state")) == Some("now"))
        .count()
}

/// "open turn, 3 helpers".
pub fn fact_line(helpers: usize) -> String {
    match helpers {
        0 => "open turn".into(),
        1 => "open turn, 1 helper".into(),
        n => format!("open turn, {n} helpers"),
    }
}

/// A running island not heard from for 15 min: since when (its `updated`).
pub fn silent_since(i: &Island, now: f64) -> Option<f64> {
    if i.state() != "running" || i.list().is_some() {
        return None;
    }
    i.updated().filter(|u| now - u >= SILENT)
}

/// When a job island's look next changes by the clock: it goes silent, or its "N min" moves on.
pub fn job_next(i: &Island, now: f64) -> Option<f64> {
    if i.state() != "running" || i.list().is_some() || sent(i).is_none() {
        return None;
    }
    let u = i.updated()?;
    Some(if now - u < SILENT { u + SILENT } else { next_minute(u, now) })
}

// MARK: Viewer reports

/// A report page's `save` writes only a bare `*.json` name beside the page: no folder, no hidden file, no "..".
pub fn save_name_ok(name: &str) -> bool {
    let n = name.trim();
    n == name
        && n.len() > 5
        && n.len() <= 128
        && n.ends_with(".json")
        && !n.starts_with('.')
        && !n.contains(['/', '\\', ':', '\0'])
        && !n.contains("..")
        && n.chars().all(|c| !c.is_control())
}

/// A message from a report page: `{action, value, label}` (a JSON object, or a string holding one).
pub fn report_message(body: &Value) -> Option<(String, Value, Option<String>)> {
    let o = match body {
        Value::String(s) => serde_json::from_str::<Value>(s).ok()?,
        v => v.clone(),
    };
    let o = o.as_object()?;
    let action = j::nonempty(o.get("action"))?.to_owned();
    Some((action, o.get("value").cloned().unwrap_or(Value::Null), j::nonempty(o.get("label")).map(str::to_owned)))
}

/// A `save` value's file and data, when the name is allowed.
pub fn save_request(value: &Value) -> Option<(String, Value)> {
    let o = value.as_object()?;
    let file = j::str(o.get("file"))?;
    save_name_ok(file).then(|| (file.to_owned(), o.get("data").cloned().unwrap_or(Value::Null)))
}

/// Write a report's saved data beside its page: pretty JSON, temp file and rename.
pub fn save_beside(page: &std::path::Path, file: &str, data: &Value) -> std::io::Result<std::path::PathBuf> {
    if !save_name_ok(file) {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "not a bare .json name"));
    }
    let dir = page.parent().ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no folder"))?;
    let dest = dir.join(file);
    let tmp = dir.join(format!(".{file}.{}.tmp", std::process::id()));
    let mut text = serde_json::to_string_pretty(data).map_err(std::io::Error::other)?;
    text.push('\n');
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, &dest).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })?;
    Ok(dest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobj;

    const H: f64 = 3600.0;

    #[test]
    fn the_front_is_the_first_live_proposal_and_plus_counts_the_live_ones_behind_it() {
        let now = 1_000_000.0;
        let raw = jobj! {"proposals": [
            {"id": "old", "title": "Shown three days ago", "shown": now - 72.0 * H},
            {"id": "a", "title": "Prepare CV drafts", "shown": now - 47.0 * H, "kind": "step", "goal": "g-job"},
            {"id": "b", "title": "Not shown yet"},
            {"id": "c", "title": "Gone too", "shown": now - 49.0 * H},
            {"id": "d", "title": "Also waiting"},
            {"title": "No id"}]};
        let (f, plus) = front(&raw, now).unwrap();
        assert_eq!((f.id.as_str(), plus), ("a", 2));
        assert_eq!(f.goal.as_deref(), Some("g-job"));
        // An hour on, the front passes 48 h: it is archived and the next one shows.
        let (f, plus) = front(&raw, now + 1.5 * H).unwrap();
        assert_eq!((f.id.as_str(), plus), ("b", 1));
        assert_eq!(next_expiry(&raw, now), Some(now + H));
        assert!(front(&jobj! {"proposals": []}, now).is_none());
        assert!(front(&jobj! {}, now).is_none());
    }

    #[test]
    fn running_counts_only_running_job_islands_naming_a_goal() {
        let isl = |id: &str, o: serde_json::Value| Island::new(id, o.as_object().unwrap().clone());
        let all = [
            isl("a", serde_json::json!({"title": "A", "state": "running", "goal": "g-arsenal"})),
            isl("b", serde_json::json!({"title": "B", "state": "running", "goal": "g-arsenal"})),
            isl("c", serde_json::json!({"title": "C", "state": "done", "goal": "g-arsenal"})),
            isl("d", serde_json::json!({"title": "D", "state": "running"})),
            isl("e", serde_json::json!({"title": "E", "state": "waiting", "goal": "g-job"})),
            isl("goals", serde_json::json!({"preset": "todo", "list": "goals", "goal": "g-arsenal", "state": "running"})),
        ];
        let m = running_by_goal(all.iter());
        assert_eq!(m.get("g-arsenal"), Some(&2));
        assert_eq!(m.get("g-job"), None, "waiting is not running");
        assert_eq!(m.len(), 1);
    }

    #[test]
    fn the_bar_is_done_then_running_then_queued_then_the_proposal() {
        let raw = jobj! {"goals": [{"id": "g", "title": "Ship Arsenal", "tasks": [
            {"title": "A", "state": "done"}, {"title": "B", "state": "done"},
            {"title": "C", "state": "now"}, {"title": "D", "state": "now"}, {"title": "E", "state": "next"}]}]};
        let g = &crate::model::Goals::parse(&raw)[0];
        let v = goal_view(g, 1, true);
        assert_eq!(v.segs, [Seg::Done, Seg::Done, Seg::Running, Seg::Queued, Seg::Queued, Seg::Proposed]);
        assert_eq!(v.queued, ["D", "E"], "a now task with no island behind it waits like a next one");
        assert!((v.done_share - 2.0 / 6.0).abs() < 1e-9 && (v.run_share - 1.0 / 6.0).abs() < 1e-9);
        // No island runs: every now task is queued, nothing is blue.
        let v = goal_view(g, 0, false);
        assert_eq!(v.segs.iter().filter(|s| **s == Seg::Running).count(), 0);
        assert_eq!(v.queued, ["C", "D", "E"]);
        // A goal with nothing yet: one dim segment; with only its proposal, the dashed one alone.
        let empty = &crate::model::Goals::parse(&jobj! {"goals": [{"id": "g", "title": "Get a job"}]})[0];
        assert_eq!(goal_view(empty, 0, false).segs, [Seg::Queued]);
        assert_eq!(goal_view(empty, 0, true).segs, [Seg::Proposed]);
        assert_eq!(goal_view(empty, 0, true).done_share, 0.0);
    }

    #[test]
    fn the_guarantee_says_quiet_waiting_unsure_or_stale() {
        let now = 1_000_000.0;
        assert_eq!(guarantee(&jobj! {}, now), Guarantee::Stale { checked: None });
        let q = jobj! {"guarantee": {"checked": now - 120.0, "waiting": [], "unsure": []}};
        assert_eq!(guarantee(&q, now), Guarantee::Quiet { checked: now - 120.0 });
        assert_eq!(checked_label(now - 120.0, now), "checked 2 min ago");
        assert_eq!(guarantee_next(&q, now), Some(now + 60.0));
        let stale = jobj! {"guarantee": {"checked": now - 40.0 * 60.0}};
        assert_eq!(guarantee(&stale, now), Guarantee::Stale { checked: Some(now - 2400.0) });
        let w = jobj! {"guarantee": {"checked": now - 60.0, "waiting": [
            {"id": "w1", "title": "Approve the Metal toolchain download", "line": "blocks Arsenal 1.0", "goal": "g-arsenal",
             "choices": [{"label": "Allow", "action": "allow"}, {"label": "Not now"}]},
            {"id": "w2", "title": "Two"}, {"id": "w3", "title": "Three"}],
            "unsure": [{"id": "u", "title": "Arsenal picks"}]}};
        match guarantee(&w, now) {
            Guarantee::Waiting { first, more } => {
                assert_eq!((first.id.as_str(), more, first.goal.as_deref()), ("w1", 2, Some("g-arsenal")));
                assert_eq!(first.choices[1], Choice { label: "Not now".into(), action: "not now".into() });
            }
            g => panic!("{g:?}"),
        }
        let u = jobj! {"guarantee": {"checked": now - 60.0, "unsure": [{"id": "u", "title": "Arsenal picks", "since": now - 40.0 * 60.0}, {"id": "v", "title": "Two", "line": "Echo log silent"}]}};
        match guarantee(&u, now) {
            Guarantee::Unsure { first, more } => {
                assert_eq!(first.text(now), "Arsenal picks not heard from in 40 min");
                assert_eq!(more, 1);
            }
            g => panic!("{g:?}"),
        }
        let u2 = Unsure { id: "v".into(), title: "Two".into(), line: Some("Echo log silent".into()), since: None };
        assert_eq!(u2.text(now), "Echo log silent");
    }

    #[test]
    fn scheduled_leads_read_now_time_weekday_time_then_date() {
        let cal = Cal::fixed("Europe/Warsaw", "en_GB");
        let now = cal.local(2026, 11, 2, 9, 0, 0).unwrap();
        assert_eq!(sched_lead(now - 60.0, now, &cal), "Now");
        assert_eq!(sched_lead(cal.local(2026, 11, 2, 15, 30, 0).unwrap(), now, &cal), "15:30");
        assert_eq!(sched_lead(cal.local(2026, 11, 3, 10, 0, 0).unwrap(), now, &cal), "Tue 10:00");
        assert_eq!(sched_lead(cal.local(2026, 11, 8, 23, 0, 0).unwrap(), now, &cal), "Sun 23:00");
        assert_eq!(sched_lead(cal.local(2026, 11, 12, 10, 0, 0).unwrap(), now, &cal), "12 Nov");
        // Tomorrow's lead turns into a time at midnight.
        let tue = cal.local(2026, 11, 3, 10, 0, 0).unwrap();
        assert_eq!(sched_lead_next(tue, now, &cal), cal.local(2026, 11, 3, 0, 0, 0));
        assert_eq!(sched_lead_next(now - 1.0, now, &cal), None);
        assert_eq!(strip_x(now, now, 328.0), 6.0);
        assert_eq!(strip_x(now + 400.0 * H, now, 328.0), 322.0);
        let l = strip_labels(now, &cal);
        assert_eq!(l.iter().map(|x| x.0.as_str()).collect::<Vec<_>>(), ["Now", "Wed", "Fri", "Mon"]);
        let raw = jobj! {"items": [{"id": "b", "title": "B", "due": now + 2.0 * H}, {"id": "x", "title": "No time"}, {"id": "a", "title": "A", "due": now + H}]};
        assert_eq!(promises(&raw).iter().map(|i| i.key.as_str()).collect::<Vec<_>>(), ["a", "b"]);
    }

    #[test]
    fn settled_counts_proof_since_the_slate_and_never_a_claim() {
        let cal = Cal::fixed("Europe/Warsaw", "en_GB");
        let now = cal.local(2026, 11, 4, 15, 0, 0).unwrap();
        let s0 = cal.local(2026, 11, 2, 0, 0, 0).unwrap();
        let at = |d: i64, h: i64| cal.local(2026, 11, d, h, 0, 0).unwrap();
        let raw = jobj! {"slate": s0, "items": [
            {"id": "a", "title": "Before the slate", "at": at(1, 12), "proof": {"kind": "commit", "value": "abc"}},
            {"id": "b", "title": "Merged", "at": at(2, 10), "proof": {"kind": "commit", "value": "abc"}},
            {"id": "c", "title": "Tested", "at": at(4, 9), "proof": {"kind": "test", "value": "42 passed"}},
            {"id": "d", "title": "Claimed only", "at": at(4, 10)},
            {"id": "e", "title": "No time", "proof": {"kind": "file", "value": "/x"}}]};
        let items = settled_items(&raw, now);
        assert_eq!(items.len(), 4, "an item without `at` is not drawn");
        let c = settled_count(&items, slate(&raw), now, &cal);
        assert_eq!(c, SettledCount { proved: 2, claimed: 1, since_slate: true });
        let cols = day_columns(&items, slate(&raw), now, &cal);
        assert_eq!(cols.iter().map(|d| (d.proved, d.claimed)).collect::<Vec<_>>(), [(0, 0), (0, 0), (0, 0), (0, 0), (1, 0), (0, 0), (1, 1)]);
        assert_eq!(cols.iter().filter(|d| d.before_slate).count(), 4, "29 Oct to 1 Nov");
        // A week past the slate: the count is this week's.
        let later = now + 9.0 * 86400.0;
        let c = settled_count(&items, slate(&raw), later, &cal);
        assert!(!c.since_slate && c.proved == 0 && c.claimed == 0);
        // No slate: this week's, and no day is before it.
        let c = settled_count(&items, None, now, &cal);
        assert_eq!((c.proved, c.since_slate), (3, false));
    }

    #[test]
    fn job_islands_say_what_they_were_sent_to_do_and_when_they_went_quiet() {
        let now = 1_000_000.0;
        let i = Island::new("j", jobj! {"title": "Arsenal 1.0", "state": "running", "sent": "Promote  the models", "goal": "g-arsenal", "updated": now - 22.0 * 60.0,
            "parts": [{"type": "band", "code": true, "helpers": [{"title": "a", "state": "now"}, {"title": "b", "state": "done"}, {"title": "c", "state": "now"}]}]});
        assert_eq!(sent(&i).as_deref(), Some("Promote the models"));
        assert_eq!(helpers_working(&i), 2);
        assert_eq!(fact_line(2), "open turn, 2 helpers");
        assert_eq!(silent_since(&i, now), Some(now - 1320.0));
        assert_eq!(minutes(now - (now - 1320.0)), "22 min");
        assert_eq!(job_next(&i, now), Some(now + 60.0));
        let fresh = Island::new("j", jobj! {"title": "A", "state": "running", "sent": "x", "updated": now - 60.0});
        assert_eq!((silent_since(&fresh, now), job_next(&fresh, now)), (None, Some(now - 60.0 + SILENT)));
        let done = Island::new("j", jobj! {"title": "A", "state": "done", "sent": "x", "updated": now - 3600.0});
        assert_eq!((silent_since(&done, now), job_next(&done, now)), (None, None));
    }

    #[test]
    fn a_report_saves_only_a_bare_json_name_beside_its_page() {
        for ok in ["picks.json", "compare-2.json", "a b.json"] {
            assert!(save_name_ok(ok), "{ok}");
        }
        for bad in ["", ".json", ".picks.json", "../picks.json", "dir/picks.json", "/tmp/picks.json", "picks.txt", "picks.json/", "a\\b.json", "..json", " picks.json", "p..q.json", "x:y.json"] {
            assert!(!save_name_ok(bad), "{bad:?}");
        }
        let v = serde_json::json!({"action": "save", "value": {"file": "picks.json", "data": {"a": 1}}, "label": "Save"});
        let (a, val, l) = report_message(&v).unwrap();
        assert_eq!((a.as_str(), l.as_deref()), ("save", Some("Save")));
        assert_eq!(save_request(&val), Some(("picks.json".into(), serde_json::json!({"a": 1}))));
        assert!(save_request(&serde_json::json!({"file": "../x.json", "data": 1})).is_none());
        assert_eq!(report_message(&serde_json::json!(r#"{"action":"pick","value":"b"}"#)).unwrap().1, "b");
        assert!(report_message(&serde_json::json!({"value": 1})).is_none());
        let dir = std::env::temp_dir().join(format!("fi-save-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let page = dir.join("page.html");
        let out = save_beside(&page, "picks.json", &serde_json::json!({"k": [1, 2]})).unwrap();
        assert_eq!(out, dir.join("picks.json"));
        let back: Value = serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
        assert_eq!(back, serde_json::json!({"k": [1, 2]}));
        assert!(save_beside(&page, "../evil.json", &Value::Null).is_err());
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1, "no temp file left");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
