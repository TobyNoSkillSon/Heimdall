//! Proposals (`docs/one-system.md`): nothing enters Goals or Scheduled without his yes, by voice or one
//! click. Whatever Heimdall, the Goals job or the plan refresher wants there is drafted as a proposal, already placed
//! where it would live; the user says yes (`plan_edit accept`) or clicks Yes, and it fills in where it sits (it keeps its
//! id). At most one shows per list (the front); the rest queue as "+N". One nobody answers is archived 48 h after the user
//! first saw it ("unanswered should be archived after two days and no they dont come back"): `front` is when it became
//! the one on screen, `shown` when the islands app first drew it to him, open and visible (its `seen` response, review
//! 2 Nov: a list left closed for two days must not lose a proposal he never saw); out of the list, into
//! the record (`<root>/record/proposals.jsonl`), never proposed again, no badge or count.

use super::files::Root;
use crate::goals::ops::{self, current, write_list};
use anyhow::{bail, ensure, Result};
use serde_json::{json, Map, Value};

/// An unanswered proposal is archived this long after it first showed.
pub const EXPIRE_S: i64 = 48 * 3600;
/// The lists that take proposals.
pub const LISTS: [&str; 2] = ["goals", "scheduled"];
/// Proposals queued per list at most (older ones beyond are archived at once: nobody reads a long queue).
pub const QUEUE: usize = 12;
pub const TITLE_CHARS: usize = 64;
pub const WHY_CHARS: usize = 300;

fn s<'a>(o: &'a Map<String, Value>, k: &str) -> &'a str { o.get(k).and_then(Value::as_str).unwrap_or("") }

/// A drafted item, before it is placed.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Draft {
    /// `goals` or `scheduled`.
    pub list: String,
    /// Goals: `goal` (a new goal) or `step` (a next step under `goal`). Scheduled: "".
    pub kind: String,
    pub title: String,
    /// The goal a step or a promise serves (a goal id), or "".
    pub goal: String,
    /// Scheduled: when it should happen (epoch s).
    pub due: Option<i64>,
    pub icon: String,
    pub about: String,
    /// The words behind it, shown when its title is clicked.
    pub why: String,
    /// Who drafted it: heimdall, goals, refresher, slate.
    pub by: String,
}

/// Is this proposal past its two days? Only one the user has seen can be (`shown` absent: queued, or never drawn to him).
pub fn expired(p: &Map<String, Value>, now: i64) -> bool {
    p.get("shown").and_then(Value::as_i64).is_some_and(|t| now - t >= EXPIRE_S)
}

/// The list's queue as objects.
pub fn queue(o: &Map<String, Value>) -> Vec<Map<String, Value>> {
    o.get("proposals").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_object).cloned().collect()
}

/// The proposal on screen: the first one not past its two days. And how many wait behind it (the "+N").
pub fn front(o: &Map<String, Value>, now: i64) -> (Option<Map<String, Value>>, usize) {
    let live: Vec<Map<String, Value>> = queue(o).into_iter().filter(|p| !expired(p, now)).collect();
    let n = live.len().saturating_sub(1);
    (live.into_iter().next(), n)
}

pub fn record_dir(root: &Root) -> std::path::PathBuf { root.0.join("record") }
fn record_file(root: &Root) -> std::path::PathBuf { record_dir(root).join("proposals.jsonl") }

/// One line in the record: an archived or declined proposal.
pub fn record(root: &Root, list: &str, p: &Map<String, Value>, outcome: &str, now: i64) -> Result<()> {
    use std::io::Write;
    std::fs::create_dir_all(record_dir(root))?;
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(record_file(root))?;
    writeln!(f, "{}", json!({"list": list, "proposal": p, "outcome": outcome, "at": now}))?;
    Ok(())
}

/// Titles in the record for `list` (archived or declined): never proposed again.
pub fn recorded(root: &Root, list: &str) -> Vec<String> {
    let Ok(t) = std::fs::read_to_string(record_file(root)) else { return Vec::new() };
    t.lines().filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["list"] == list)
        .filter_map(|v| v["proposal"]["title"].as_str().map(str::to_owned)).collect()
}

/// The same thing in other words: equal once normalised, or (both naming at least three content words) the Settled
/// rule's 75 % shared words. A short title never swallows a longer, distinct one ("Vireo: icon" vs "Vireo: icon review").
fn same(a: &str, b: &str) -> bool {
    use crate::goals::settled::words;
    ops::normalise(a) == ops::normalise(b) || (words(a).len() >= 3 && words(b).len() >= 3 && crate::goals::settled::same(a, b))
}

/// Bug note 2026-11-01T12-48-26 ("clear these orange goals and remember I didn't want them"): what
/// he removed from a list (`plan_edit` remove writes its `removed`) is never proposed there again, in the same words or
/// in other words (the Settled rule; on Goals and Settled without its two-week end, a removal holds).
pub fn removed_here(o: &Map<String, Value>, list: &str, title: &str, now: i64) -> bool {
    // Scheduled's memory lapses after `REMOVED_DAYS`: a promise removed once may be made again (fresh review, 2 Nov);
    // Goals' and Settled's never do ("remember I didn't want them").
    let now = if list == "scheduled" { now } else { 0 };
    let days = crate::goals::settled::REMOVED_DAYS * 86_400;
    let titles = o.get("removed").and_then(Value::as_array).into_iter().flatten()
        .filter(|r| now == 0 || r.get("at").and_then(Value::as_i64).is_none_or(|at| at == 0 || now - at <= days))
        .filter_map(|r| r.get("title").and_then(Value::as_str).or_else(|| r.as_str()));
    titles.into_iter().any(|t| same(t, title)) || crate::goals::settled::removed_like(o, title, "", now)
}

fn short_id(prefix: &str) -> String { format!("{prefix}-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]) }

/// Why this draft is refused against the list as it is, or None: it is already there (an item, a goal, a task or a
/// queued proposal of the same words), it was archived or declined before, or it is malformed.
fn refuse(o: &Map<String, Value>, d: &Draft, recorded: &[String], now: i64) -> Option<String> {
    if d.title.trim().is_empty() { return Some("no title".into()) }
    if recorded.iter().any(|r| same(r, &d.title)) { return Some("proposed before and not taken (it stays in the record)".into()) }
    if removed_here(o, &d.list, &d.title, now) { return Some("User removed it from this list; it does not come back".into()) }
    if queue(o).iter().any(|p| same(s(p, "title"), &d.title)) { return Some("already proposed".into()) }
    match d.list.as_str() {
        "goals" => {
            let goals: Vec<&Map<String, Value>> = o.get("goals").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_object).collect();
            match d.kind.as_str() {
                "goal" => if goals.iter().any(|g| same(s(g, "title"), &d.title)) { return Some("a goal of that title exists".into()) },
                "step" => {
                    let Some(g) = goals.iter().find(|g| s(g, "id") == d.goal) else { return Some(format!("no goal {:?} (show list goals for the ids)", d.goal)) };
                    if ops::goal_state(g) == "done" { return Some("that goal is finished".into()) }
                    if g.get("tasks").and_then(Value::as_array).into_iter().flatten().any(|t| t["title"].as_str().is_some_and(|x| same(x, &d.title))) {
                        return Some("that step is already under the goal".into());
                    }
                }
                k => return Some(format!("kind {k:?} is not goal or step")),
            }
        }
        "scheduled" => {
            let Some(due) = d.due else { return Some("no time, no promise: a Scheduled proposal needs due".into()) };
            if due <= now { return Some("its time is already past".into()) }
            if crate::islands::plan::items(o).iter().any(|i| same(s(i, "title"), &d.title)) { return Some("already in Scheduled".into()) }
        }
        l => return Some(format!("{l} takes no proposals (Settled fills only from proof)")),
    }
    None
}

/// Drafts one proposal into its list's queue (under the folder lock). The front of an empty queue shows at once
/// (`shown` stamped). Returns the proposal's id and whether it is the one on screen now.
pub fn propose(root: &Root, d: &Draft, now: i64) -> Result<(String, bool)> {
    root.locked(|| {
        let mut o = current(root, &d.list).unwrap_or_else(|| crate::islands::plan::fresh(&d.list));
        let r = push(root, &mut o, d, now)?;
        o.insert("updated".into(), json!(now));
        write_list(root, &d.list, &o)?;
        Ok(r)
    })
}

/// `propose` on a list already in hand (the caller holds the folder lock and writes it).
pub fn push(root: &Root, o: &mut Map<String, Value>, d: &Draft, now: i64) -> Result<(String, bool)> {
    let r = push_in(o, d, &recorded(root, &d.list), now)?;
    trim(root, o, &d.list, now);
    Ok(r)
}

/// `push` without the record at hand: `recorded` is the record's titles for this list (the Goals job passes them in
/// its evidence). The queue is trimmed by the next sweep.
pub fn push_in(o: &mut Map<String, Value>, d: &Draft, recorded: &[String], now: i64) -> Result<(String, bool)> {
    if let Some(why) = refuse(o, d, recorded, now) { bail!("{why}") }
    let id = match (d.list.as_str(), d.kind.as_str()) {
        ("goals", "goal") => unique_goal_id(o, &d.title),
        ("goals", _) => short_id("t"),
        _ => short_id("s"),
    };
    let mut p = json!({"id": id, "title": ops::cap(&d.title, TITLE_CHARS), "by": if d.by.is_empty() { "heimdall" } else { d.by.as_str() }, "proposed": now});
    if !d.why.trim().is_empty() { p["why"] = json!(ops::cap(&d.why, WHY_CHARS)) }
    if d.list == "goals" { p["kind"] = json!(d.kind) }
    if !d.goal.is_empty() { p["goal"] = json!(d.goal) }
    if let Some(due) = d.due { p["due"] = json!(due) }
    if let Some(i) = ops::symbol(&d.icon) { p["icon"] = json!(i) }
    if !d.about.trim().is_empty() { p["about"] = json!(ops::cap(&d.about, ops::ABOUT_CHARS)) }
    let shown_now = front(o, now).0.is_none();
    if shown_now { p["front"] = json!(now) }
    arr(o, "proposals").push(p);
    Ok((id, shown_now))
}

/// A long queue: the oldest unseen ones beyond `QUEUE` go to the record (never the one on screen).
fn trim(root: &Root, o: &mut Map<String, Value>, list: &str, now: i64) -> bool {
    let q = arr(o, "proposals");
    let mut any = false;
    while q.len() > QUEUE {
        let k = q.iter().position(|p| p.get("front").is_none()).unwrap_or(1).max(1);
        let gone = q.remove(k);
        if let Some(g) = gone.as_object() { let _ = record(root, list, g, "archived", now); }
        any = true;
    }
    any
}

fn unique_goal_id(o: &Map<String, Value>, title: &str) -> String {
    let slug: String = title.to_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    let mut slug = slug.split('-').filter(|p| !p.is_empty()).collect::<Vec<_>>().join("-");
    slug.truncate(32);
    let base = format!("g-{}", slug.trim_end_matches('-'));
    let taken = |id: &str| o.get("goals").and_then(Value::as_array).into_iter().flatten().any(|g| g["id"] == id)
        || queue(o).iter().any(|p| s(p, "id") == id);
    std::iter::once(base.clone()).chain((2..).map(|n| format!("{base}-{n}"))).find(|c| !taken(c)).unwrap()
}

/// How the user said yes.
#[derive(Clone, Debug, PartialEq)]
pub enum Yes { Click, Voice(String) }

impl Yes {
    fn to_value(&self, now: i64) -> Value {
        match self {
            Yes::Click => json!({"by": "click", "at": now}),
            Yes::Voice(q) if q.trim().is_empty() => json!({"by": "voice", "at": now}),
            Yes::Voice(q) => json!({"by": "voice", "at": now, "quote": ops::cap(q, 200)}),
        }
    }
}

/// Finds a queued proposal by id, exact title or a unique title fragment, in `list` or (None) either list.
pub fn find(root: &Root, list: Option<&str>, reference: &str) -> Option<(&'static str, Map<String, Value>)> {
    let r = reference.trim().to_lowercase();
    let mut hits = Vec::new();
    for l in LISTS.into_iter().filter(|l| list.is_none_or(|x| x == *l)) {
        let Some(o) = root.read(l) else { continue };
        for p in queue(&o) {
            if s(&p, "id") == reference || s(&p, "title").to_lowercase() == r { return Some((l, p)) }
            if !r.is_empty() && s(&p, "title").to_lowercase().contains(&r) { hits.push((l, p)) }
        }
    }
    if hits.len() == 1 { hits.pop() } else { None }
}

/// The user's yes: the proposal fills in where it sits, same id. A goal joins the goals (active, no tasks), a step joins its
/// goal's tasks as next, a promise joins Scheduled at its time (`due` replaces it when he named another: "move it to
/// Thursday"). Returns the title and the list.
pub fn accept(root: &Root, list: Option<&str>, reference: &str, yes: Yes, due: Option<i64>, now: i64) -> Result<(String, &'static str, Option<Map<String, Value>>)> {
    let (l, _) = find(root, list, reference).ok_or_else(|| anyhow::anyhow!("no proposal {reference:?} (plan_edit show lists them)"))?;
    root.locked(|| {
        let mut o = current(root, l).unwrap_or_else(|| crate::islands::plan::fresh(l));
        let q = o.get_mut("proposals").and_then(Value::as_array_mut).ok_or_else(|| anyhow::anyhow!("no proposals in {l}"))?;
        let r = reference.trim().to_lowercase();
        let k = q.iter().position(|p| p["id"] == reference || p["title"].as_str().is_some_and(|t| t.to_lowercase() == r))
            .or_else(|| q.iter().position(|p| p["title"].as_str().is_some_and(|t| t.to_lowercase().contains(&r))))
            .ok_or_else(|| anyhow::anyhow!("no proposal {reference:?}"))?;
        let p = q.remove(k).as_object().cloned().unwrap_or_default();
        let title = s(&p, "title").to_owned();
        let yes = yes.to_value(now);
        match (l, s(&p, "kind")) {
            ("goals", "goal") => {
                let active = o.get("goals").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_object).filter(|g| ops::goal_state(g) == "active").count();
                ensure!(active < ops::MAX_ACTIVE, "{} active goals already: finish or pause one first", ops::MAX_ACTIVE);
                let icon = ops::symbol(s(&p, "icon")).unwrap_or_else(|| ops::ICON.into());
                let g = json!({"id": s(&p, "id"), "title": title, "icon": icon, "about": s(&p, "about"), "state": "active", "now": "", "moved": now,
                               "tasks": [], "expanded": false, "yes": yes});
                arr(&mut o, "goals").push(g);
            }
            ("goals", _) => {
                let gid = s(&p, "goal").to_owned();
                let g = arr(&mut o, "goals").iter_mut().filter_map(Value::as_object_mut).find(|g| s(g, "id") == gid)
                    .ok_or_else(|| anyhow::anyhow!("its goal {gid} is gone"))?;
                arr(g, "tasks").push(json!({"id": s(&p, "id"), "title": title, "state": "next", "at": now, "yes": yes}));
                g.insert("moved".into(), json!(now));
            }
            _ => {
                let due = due.or_else(|| p.get("due").and_then(Value::as_i64)).ok_or_else(|| anyhow::anyhow!("no time"))?;
                let mut item = json!({"id": s(&p, "id"), "title": title, "done": false, "due": due, "yes": yes});
                for k in ["goal", "icon", "why"] { if let Some(v) = p.get(k) { item[k] = v.clone() } }
                arr(&mut o, "items").push(item);
            }
        }
        let next = stamp_front(&mut o, now).and_then(|_| front(&o, now).0);
        o.insert("updated".into(), json!(now));
        write_list(root, l, &o)?;
        Ok((title, l, next))
    })
}

/// The user's no: the proposal goes to the record and is never proposed again. Returns its title.
pub fn decline(root: &Root, list: Option<&str>, reference: &str, now: i64) -> Result<(String, &'static str, Option<Map<String, Value>>)> {
    let (l, p) = find(root, list, reference).ok_or_else(|| anyhow::anyhow!("no proposal {reference:?} (plan_edit show lists them)"))?;
    let id = s(&p, "id").to_owned();
    let next = root.locked(|| {
        let mut o = current(root, l).unwrap_or_else(|| crate::islands::plan::fresh(l));
        arr(&mut o, "proposals").retain(|x| x["id"] != id.as_str());
        let next = stamp_front(&mut o, now).and_then(|_| front(&o, now).0);
        record(root, l, &p, "declined", now)?;
        write_list(root, l, &o)?;
        Ok(next)
    })?;
    Ok((s(&p, "title").to_owned(), l, next))
}

fn arr<'a>(o: &'a mut Map<String, Value>, k: &str) -> &'a mut Vec<Value> {
    if !o.get(k).is_some_and(Value::is_array) { o.insert(k.into(), json!([])); }
    o.get_mut(k).and_then(Value::as_array_mut).unwrap()
}

/// The new front, if it was not the front before, is from now (Heimdall hears of it). Returns its title when stamped.
fn stamp_front(o: &mut Map<String, Value>, now: i64) -> Option<String> {
    let q = o.get_mut("proposals").and_then(Value::as_array_mut)?;
    let p = q.iter_mut().filter_map(Value::as_object_mut).find(|p| !expired(p, now))?;
    if p.contains_key("front") { return None }
    p.insert("front".into(), json!(now));
    Some(s(p, "title").to_owned())
}

/// The islands app drew this proposal to the user (its list open and on screen): its two days start now. Once.
pub fn seen(root: &Root, list: &str, id: &str, now: i64) -> Result<bool> {
    root.locked(|| {
        let Some(mut o) = root.read(list) else { return Ok(false) };
        let Some(p) = o.get_mut("proposals").and_then(Value::as_array_mut).and_then(|q| q.iter_mut().filter_map(Value::as_object_mut).find(|p| s(p, "id") == id)) else { return Ok(false) };
        if p.contains_key("shown") { return Ok(false) }
        p.insert("shown".into(), json!(now));
        write_list(root, list, &o)?;
        Ok(true)
    })
}

/// What one sweep did on a list.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Swept { pub archived: Vec<String>, pub shown: Option<Map<String, Value>> }

/// The two-day rule, on both lists (under the folder lock): every proposal past 48 h since it showed goes to the
/// record, and the next one shows from now. Writes only what changed. Returns per list what moved.
pub fn sweep(root: &Root, now: i64) -> Result<Vec<(&'static str, Swept)>> {
    let mut out = Vec::new();
    for l in LISTS {
        let r = root.locked(|| {
            let Some(mut o) = root.read(l) else { return Ok(Swept::default()) };
            let mut w = Swept::default();
            let before = o.clone();
            let gone: Vec<Map<String, Value>> = queue(&o).into_iter().filter(|p| expired(p, now)).collect();
            if !gone.is_empty() {
                arr(&mut o, "proposals").retain(|p| !p.as_object().is_some_and(|p| expired(p, now)));
                for g in &gone { record(root, l, g, "archived", now)?; w.archived.push(s(g, "title").to_owned()) }
            }
            trim(root, &mut o, l, now);
            if stamp_front(&mut o, now).is_some() { w.shown = front(&o, now).0 }
            if o != before { write_list(root, l, &o)?; }
            Ok(w)
        })?;
        if r != Swept::default() { out.push((l, r)) }
    }
    Ok(out)
}

/// The line Heimdall hears when a proposal comes on screen that it did not draft this turn: say it in one short line
/// and take the user's answer.
pub fn notice(list: &str, p: &Map<String, Value>) -> String {
    let what = match (list, s(p, "kind")) {
        ("goals", "goal") => "a new goal".to_owned(),
        ("goals", _) => format!("a next step for {}", s(p, "goal")),
        _ => p.get("due").and_then(Value::as_i64).and_then(|d| chrono::DateTime::from_timestamp(d, 0))
            .map_or("a promise".into(), |d| format!("a promise for {}", d.with_timezone(&chrono::Local).format("%a %d %b %H:%M"))),
    };
    let why = s(p, "why");
    format!("Proposed in {} ({what}): '{}' [{}]{}. It waits for User's yes: say it in one short line when it fits; his yes is plan_edit accept, his no plan_edit decline. Unanswered, it is archived in two days.",
        crate::islands::plan::list_title(list), s(p, "title"), s(p, "id"), if why.is_empty() { String::new() } else { format!(", from: {why}") })
}

/// `plan_edit show`'s lines for a list's proposals: the one on screen, then the queue.
pub fn text(o: &Map<String, Value>, now: i64) -> String {
    let (f, _) = front(o, now);
    let mut out = String::new();
    for p in queue(o).iter().filter(|p| !expired(p, now)) {
        let on = f.as_ref().is_some_and(|f| f["id"] == p["id"]);
        let mut l = format!("- proposal {} · {}", s(p, "id"), s(p, "title"));
        if !s(p, "kind").is_empty() { l.push_str(&format!(" · {}", s(p, "kind"))) }
        if !s(p, "goal").is_empty() { l.push_str(&format!(" · goal {}", s(p, "goal"))) }
        if let Some(d) = p.get("due").and_then(Value::as_i64).and_then(|d| chrono::DateTime::from_timestamp(d, 0)) {
            l.push_str(&format!(" · due {}", d.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M")));
        }
        l.push_str(if on { " · on screen" } else { " · queued" });
        out.push_str(&l);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> (tempfile::TempDir, Root) {
        let d = tempfile::tempdir().unwrap();
        let r = Root(d.path().to_path_buf());
        r.ensure().unwrap();
        (d, r)
    }
    fn goal(title: &str) -> Draft { Draft { list: "goals".into(), kind: "goal".into(), title: title.into(), icon: "briefcase".into(), by: "heimdall".into(), ..Draft::default() } }
    fn promise(title: &str, due: i64) -> Draft { Draft { list: "scheduled".into(), title: title.into(), due: Some(due), by: "heimdall".into(), ..Draft::default() } }

    #[test]
    fn one_shows_the_rest_queue_and_a_yes_fills_it_in_with_the_same_id() {
        let (_d, r) = root();
        let (a, shown_a) = propose(&r, &goal("Get a job"), 100).unwrap();
        let (b, shown_b) = propose(&r, &goal("Ship Arsenal"), 101).unwrap();
        assert!(shown_a && !shown_b);
        let o = r.read("goals").unwrap();
        let (f, more) = front(&o, 200);
        assert_eq!(f.unwrap()["id"], a.as_str());
        assert_eq!(more, 1);
        // Nothing entered Goals.
        assert!(o.get("goals").and_then(Value::as_array).is_none_or(|g| g.is_empty()));
        let (t, l, next) = accept(&r, None, "Get a job", Yes::Voice("yes".into()), None, 300).unwrap();
        assert_eq!((t.as_str(), l, next.map(|n| n["id"].as_str().unwrap().to_owned())), ("Get a job", "goals", Some(b.clone())));
        let o = r.read("goals").unwrap();
        assert_eq!(o["goals"][0]["id"], a.as_str());
        assert_eq!(o["goals"][0]["yes"]["by"], "voice");
        // The next one shows from the yes.
        let (f, more) = front(&o, 300);
        let f = f.unwrap();
        assert_eq!((f["id"].as_str(), f["front"].as_i64(), f.get("shown").is_none(), more), (Some(b.as_str()), Some(300), true, 0));
    }

    #[test]
    fn unanswered_for_two_days_it_is_archived_never_comes_back_and_the_next_shows() {
        let (_d, r) = root();
        let (first, _) = propose(&r, &promise("Release: check model licences", 10_000_000), 0).unwrap();
        let (second, _) = propose(&r, &promise("Arsenal: post-release setup", 10_000_000), 1).unwrap();
        // Never drawn to the user (the list closed for days): it does not expire.
        assert!(sweep(&r, 10 * EXPIRE_S).unwrap().is_empty());
        // Drawn to him at 100: two days from then, a day and 23 hours is still there.
        assert!(seen(&r, "scheduled", &first, 100).unwrap());
        assert!(!seen(&r, "scheduled", &first, 200).unwrap(), "once");
        assert!(sweep(&r, 100 + EXPIRE_S - 1).unwrap().is_empty());
        let swept = sweep(&r, 100 + EXPIRE_S).unwrap();
        assert_eq!(swept[0].0, "scheduled");
        assert_eq!(swept[0].1.archived, ["Release: check model licences"]);
        assert_eq!(swept[0].1.shown.as_ref().unwrap()["id"], second.as_str());
        let o = r.read("scheduled").unwrap();
        assert_eq!(queue(&o).len(), 1);
        assert!(plan_items_empty(&o), "an archived proposal never enters Scheduled");
        // In the record, and refused if drafted again.
        assert_eq!(recorded(&r, "scheduled"), ["Release: check model licences"]);
        assert!(propose(&r, &promise("Release: check model licences", 10_000_000), EXPIRE_S + 1).is_err());
        // The second one has its own two days from when he sees it.
        assert!(sweep(&r, 5 * EXPIRE_S).unwrap().is_empty());
    }

    /// Bug note 2026-11-01T12-48-26: eight asks the user removed ("remember I didn't want them") must never return. A
    /// removal by plan_edit is remembered on its list, and no one (Heimdall, the Goals job, the refresher) proposes it
    /// there again, in the same or other words; a different thing still goes through.
    #[test]
    fn what_owner_removed_is_never_proposed_again() {
        let d = tempfile::tempdir().unwrap();
        let h = crate::islands::Host::new(Root(d.path().join("fi")), None);
        h.root.ensure().unwrap();
        let mut g = ops::fresh();
        g.insert("goals".into(), json!([{"id": "g-arsenal", "title": "Ship Arsenal", "state": "active", "tasks": []}]));
        g.insert("items".into(), json!([{"id": "arsenal-pick-next-ideas", "title": "Arsenal: pick next ideas", "kind": "pick"}]));
        write_list(&h.root, "goals", &g).unwrap();
        let mut sc = crate::islands::plan::fresh("scheduled");
        sc.insert("items".into(), json!([{"id": "s-1", "title": "Review the JobRadar challenge", "due": 9_000_000_000_i64}]));
        write_list(&h.root, "scheduled", &sc).unwrap();
        crate::islands::tools::plan_edit(&h, &json!({"action": "remove", "item": "Arsenal: pick next ideas"})).unwrap();
        crate::islands::tools::plan_edit(&h, &json!({"action": "remove", "item": "Review the JobRadar challenge"})).unwrap();
        let step = |t: &str| Draft { list: "goals".into(), kind: "step".into(), goal: "g-arsenal".into(), title: t.into(), by: "goals".into(), ..Draft::default() };
        for t in ["Arsenal: pick next ideas", "Pick the next Arsenal ideas"] {
            assert!(propose(&h.root, &step(t), 100).unwrap_err().to_string().contains("removed"), "{t}");
        }
        assert!(propose(&h.root, &goal("Pick next ideas for Arsenal"), 100).is_err());
        assert!(propose(&h.root, &promise("Review the JobRadar challenge", 9_000_000_000), 100).unwrap_err().to_string().contains("removed"));
        // Something else is still proposed.
        propose(&h.root, &step("Write the Arsenal release notes"), 100).unwrap();
        assert_eq!(queue(&h.root.read("goals").unwrap()).len(), 1);
        // Scheduled's memory lapses after two weeks (a promise may be made again; fresh review, 2 Nov); Goals' holds.
        let later = crate::islands::tools::now_s() as i64 + (crate::goals::settled::REMOVED_DAYS + 1) * 86_400;
        propose(&h.root, &promise("Review the JobRadar challenge", 9_000_000_000), later).unwrap();
        assert!(propose(&h.root, &step("Pick the next Arsenal ideas"), later).unwrap_err().to_string().contains("removed"));
    }

    fn plan_items_empty(o: &Map<String, Value>) -> bool { crate::islands::plan::items(o).is_empty() }

    #[test]
    fn a_no_is_recorded_and_a_promise_needs_a_future_time_and_settled_takes_none() {
        let (_d, r) = root();
        assert!(propose(&r, &Draft { list: "scheduled".into(), title: "Later thing".into(), ..Draft::default() }, 0).unwrap_err().to_string().contains("no time"));
        assert!(propose(&r, &promise("Past", 5), 10).is_err());
        assert!(propose(&r, &Draft { list: "settled".into(), title: "Done".into(), ..Draft::default() }, 0).is_err());
        propose(&r, &promise("Dentist", 1000), 0).unwrap();
        decline(&r, Some("scheduled"), "Dentist", 1).unwrap();
        assert!(queue(&r.read("scheduled").unwrap()).is_empty());
        assert!(propose(&r, &promise("Dentist", 2000), 2).is_err());
    }

    #[test]
    fn a_promise_moved_by_voice_takes_the_new_time_and_a_step_joins_its_goal_as_next() {
        let (_d, r) = root();
        propose(&r, &promise("Release: check model licences", 1000), 0).unwrap();
        accept(&r, Some("scheduled"), "licences", Yes::Voice("move it to Thursday".into()), Some(5000), 1).unwrap();
        let o = r.read("scheduled").unwrap();
        assert_eq!(o["items"][0]["due"], 5000);
        propose(&r, &goal("Get a job"), 0).unwrap();
        accept(&r, None, "Get a job", Yes::Click, None, 1).unwrap();
        let gid = r.read("goals").unwrap()["goals"][0]["id"].as_str().unwrap().to_owned();
        let step = Draft { list: "goals".into(), kind: "step".into(), title: "Prepare CV drafts".into(), goal: gid.clone(), ..Draft::default() };
        assert!(propose(&r, &Draft { goal: "g-nope".into(), ..step.clone() }, 2).is_err());
        let (id, _) = propose(&r, &step, 2).unwrap();
        accept(&r, None, &id, Yes::Click, None, 3).unwrap();
        let o = r.read("goals").unwrap();
        assert_eq!(o["goals"][0]["tasks"][0]["id"], id.as_str());
        assert_eq!(o["goals"][0]["tasks"][0]["state"], "next");
        assert_eq!(o["goals"][0]["tasks"][0]["yes"]["by"], "click");
        // The same step again is refused.
        assert!(propose(&r, &step, 4).is_err());
    }
}
