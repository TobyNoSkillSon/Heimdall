//! The plan islands: Goals (31 Oct, in To do's place: its `items` are the asks, its `goals` the goals; the shadow
//! keeps them, `goals::ops`), Scheduled and Settled, docked left in that order. Items carry a stable `id` (the app
//! diffs by it to animate). Shared by `plan_edit`, approvals and ticks (responses.rs). A plan island the user closes is
//! hidden, never deleted (bug note 2026-10-31T15-57-16: closing To do deleted the whole list): the app writes
//! `hidden: true`, and a copy is kept beside it however the close arrived (`keep_closed`), so `reopen_closed` brings it
//! back. Contract: `docs/archive/briefs/night-1022-islands.md`, `docs/goals.md`.

use super::files::Root;
use anyhow::{bail, Context, Result};
use serde_json::{json, Map, Value};

/// The three lists: island id (also the `list` key), title, dock order.
pub const LISTS: [(&str, &str, u32); 3] = [("goals", "Goals", 0), ("scheduled", "Scheduled", 1), ("settled", "Settled", 2)];
pub const GROUPS: [&str; 3] = ["running", "waiting", "discussed"];

/// The list a name means: "todo" (old callers, the tools' contract) is Goals now.
pub fn canonical(list: &str) -> &str { if list == "todo" { "goals" } else { list } }

pub fn list_title(list: &str) -> &'static str {
    LISTS.iter().find(|l| l.0 == canonical(list)).map_or("Goals", |l| l.1)
}

/// What a new plan island starts as. Settled is a pill (collapsed) by default; Goals is `goals::ops::fresh`.
pub(crate) fn fresh(list: &str) -> Map<String, Value> {
    if canonical(list) == "goals" { return crate::goals::ops::fresh() }
    let (id, title, order) = LISTS.iter().copied().find(|l| l.0 == list).unwrap_or(LISTS[1]);
    let mut o = json!({"id": id, "preset": "todo", "list": id, "title": title, "side": "left", "order": order, "items": []});
    if id == "settled" { o["expanded"] = json!(false) }
    o.as_object().unwrap().clone()
}

/// Items as objects (a plain string item becomes `{title}`), each with an id (title-derived when missing).
pub fn items(o: &Map<String, Value>) -> Vec<Map<String, Value>> {
    let mut out: Vec<Map<String, Value>> = Vec::new();
    for v in o.get("items").and_then(Value::as_array).into_iter().flatten() {
        let mut item = match v {
            Value::Object(m) => m.clone(),
            Value::String(s) => json!({"title": s}).as_object().unwrap().clone(),
            _ => continue,
        };
        if item.get("id").and_then(Value::as_str).is_none_or(str::is_empty) {
            let id = unique_id(item.get("title").and_then(Value::as_str).unwrap_or("item"), &out);
            item.insert("id".into(), json!(id));
        }
        out.push(item);
    }
    out
}

/// A stable id from the title: lower-case words joined by `-`, at most 40 characters, made unique among `taken`.
pub fn unique_id(title: &str, taken: &[Map<String, Value>]) -> String {
    let mut base: String = title.to_lowercase().chars().map(|c| if c.is_alphanumeric() { c } else { '-' }).collect();
    while base.contains("--") { base = base.replace("--", "-") }
    let mut base = base.trim_matches('-').chars().take(40).collect::<String>().trim_end_matches('-').to_string();
    if base.is_empty() { base = "item".into() }
    let used = |id: &str| taken.iter().any(|i| i.get("id").and_then(Value::as_str) == Some(id));
    if !used(&base) { return base }
    (2..).map(|n| format!("{base}-{n}")).find(|c| !used(c)).unwrap()
}

/// Where an item is: list id and index.
#[derive(Debug)]
pub struct Found { pub list: &'static str, pub index: usize, pub item: Map<String, Value> }

/// Find an item by id, then exact title, then a unique case-insensitive title match, across the three lists (in
/// Goals: its asks).
pub fn find(root: &Root, reference: &str) -> Result<Found> {
    let all: Vec<(&'static str, Vec<Map<String, Value>>)> =
        LISTS.iter().map(|l| (l.0, root.read(l.0).map(|o| items(&o)).unwrap_or_default())).collect();
    let title = |i: &Map<String, Value>| i.get("title").and_then(Value::as_str).unwrap_or("").to_string();
    for pass in 0..3 {
        let mut hits = Vec::new();
        for (list, its) in &all {
            for (k, i) in its.iter().enumerate() {
                let hit = match pass {
                    0 => i.get("id").and_then(Value::as_str) == Some(reference),
                    1 => title(i) == reference,
                    _ => title(i).to_lowercase().contains(&reference.to_lowercase()),
                };
                if hit { hits.push(Found { list, index: k, item: i.clone() }) }
            }
        }
        match hits.len() {
            0 => continue,
            1 => return Ok(hits.pop().unwrap()),
            _ if pass == 2 => bail!("several plan items match '{reference}'; use its exact title"),
            _ => return Ok(hits.remove(0)),
        }
    }
    bail!("no item '{reference}' in Goals' asks, Scheduled or Settled")
}

/// True when a plan island on disk predates the night-1022 shape: no `preset: "todo"` or `list` key, or items
/// that are plain strings or lack an id (the user's Settled list before night 1022 was nine plain strings).
pub fn needs_migration(list: &str, o: &Map<String, Value>) -> bool {
    o.get("preset").and_then(Value::as_str) != Some("todo")
        || o.get("list").and_then(Value::as_str) != Some(list)
        || o.get("items").and_then(Value::as_array).into_iter().flatten()
            .any(|v| v.get("id").and_then(Value::as_str).is_none_or(str::is_empty))
}

/// The plan shape, in place: `preset: "todo"`, `list`, and every item an object with an id (strings keep their
/// text as the title). Every other key, and every item's text and keys, are kept.
pub(crate) fn migrate_object(list: &str, o: &mut Map<String, Value>) {
    o.insert("preset".into(), json!("todo"));
    o.insert("list".into(), json!(list));
    if o.contains_key("items") {
        let its = items(o);
        o.insert("items".into(), Value::Array(its.into_iter().map(Value::Object).collect()));
    }
}

/// Before the first migration ever writes: the whole `islands/` folder copied once to
/// `<root>/backup-<YYYYMMDD-HHMMSS>/` (beside `islands/`, so the helper never shows it). Nothing when any
/// `backup-*` folder exists already. Call holding the folder lock.
pub fn backup_once(root: &Root) -> Result<Option<std::path::PathBuf>> {
    let taken = std::fs::read_dir(&root.0).into_iter().flatten().flatten()
        .any(|e| { let n = e.file_name().to_string_lossy().into_owned(); n.starts_with("backup-") && !n.starts_with("backup-goals-") && e.path().is_dir() });
    if taken { return Ok(None) }
    let dir = root.0.join(format!("backup-{}", chrono::Local::now().format("%Y%m%d-%H%M%S")));
    std::fs::create_dir_all(&dir)?;
    for e in std::fs::read_dir(root.islands())?.flatten() {
        let name = e.file_name();
        if name.to_string_lossy().starts_with('.') || !e.path().is_file() { continue }
        std::fs::copy(e.path(), dir.join(&name))?;
    }
    eprintln!("islands: plan lists migrated to the night-1022 shape; the islands folder was backed up to {}", dir.display());
    Ok(Some(dir))
}

/// Migrate the existing plan islands (`todo`, `scheduled`, `settled`, recognised by id) in place on first contact,
/// after the one-time backup. Returns the lists rewritten.
pub fn migrate_all(root: &Root) -> Result<Vec<&'static str>> {
    // Cheap check first: nothing to do needs no lock.
    if !LISTS.iter().any(|l| root.read(l.0).is_some_and(|o| needs_migration(l.0, &o))) { return Ok(Vec::new()) }
    root.locked(|| {
        let mut done = Vec::new();
        for (list, _, _) in LISTS {
            let Some(mut o) = root.read(list) else { continue };
            if !needs_migration(list, &o) { continue }
            backup_once(root)?;
            migrate_object(list, &mut o);
            write_list(root, list, &o)?;
            done.push(list);
        }
        Ok(done)
    })
}

/// Read-modify-write one list's items; the caller holds the folder lock. An old-shape island is migrated on the way
/// (after the one-time backup).
fn edit_locked<T>(root: &Root, list: &str, change: impl FnOnce(&mut Vec<Map<String, Value>>) -> Result<T>) -> Result<T> {
    // A closed or vanished list is written on from its kept copy, never from nothing (`goals::ops::current`).
    let mut o = match crate::goals::ops::current(root, list) {
        Some(mut o) => {
            if needs_migration(list, &o) { backup_once(root)?; migrate_object(list, &mut o); }
            o
        }
        None => fresh(list),
    };
    let mut its = items(&o);
    let out = change(&mut its)?;
    o.insert("items".into(), Value::Array(its.into_iter().map(Value::Object).collect()));
    o.insert("updated".into(), json!(super::tools::now_s()));
    write_list(root, list, &o)?;
    Ok(out)
}

/// A plan list written, with its kept copy beside it (`goals::ops::write_list`): a close that arrives without the
/// island still leaves its content.
pub fn write_list(root: &Root, list: &str, o: &Map<String, Value>) -> Result<()> { crate::goals::ops::write_list(root, list, o) }

/// A Scheduled item's due time from what the agent or the refresher writes: a local date-time
/// (`2026-10-24T10:00`, `2026-10-24 10:00`, seconds optional) or an RFC 3339 time with an offset. Returns epoch
/// seconds, the `due` key the helper counts down to (islands/README.md, docs/archive/briefs/night-1023-islands.md).
fn now_s() -> f64 { std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs_f64() }

pub fn due_seconds(text: &str) -> Result<i64> {
    use chrono::{DateTime, Local, NaiveDateTime, TimeZone};
    let t = text.trim();
    if let Ok(d) = DateTime::parse_from_rfc3339(t) { return Ok(d.timestamp()) }
    for fmt in ["%Y-%m-%dT%H:%M", "%Y-%m-%d %H:%M", "%Y-%m-%dT%H:%M:%S", "%Y-%m-%d %H:%M:%S"] {
        if let Ok(n) = NaiveDateTime::parse_from_str(t, fmt) {
            // A local time that does not exist (the spring-forward hour) is an error; an ambiguous one takes the first.
            return Local.from_local_datetime(&n).earliest().map(|d| d.timestamp())
                .with_context(|| format!("due: {t} does not exist in the local time zone"));
        }
    }
    bail!("due: a local date-time like 2026-10-24T10:00, got \"{t}\"")
}

/// Read-modify-write one list's items under the folder lock, creating the island when needed.
pub fn edit_list<T>(root: &Root, list: &str, change: impl FnOnce(&mut Vec<Map<String, Value>>) -> Result<T>) -> Result<T> {
    let list = canonical(list);
    root.locked(|| edit_locked(root, list, change))
}

/// Move an item (by id) out of `from` and into `to` with the given changes merged in (`null` removes a key). Both
/// lists change under one hold of the folder lock, so no other writer interleaves (the item is never duplicated or
/// lost). The destination is written first: a failed write leaves the item where it was.
pub fn move_item(root: &Root, from: &str, id: &str, to: &str, changes: &Map<String, Value>) -> Result<Option<Map<String, Value>>> {
    let (from, to) = (canonical(from), canonical(to));
    let same = from == to;
    let place = move |mut item: Map<String, Value>| {
        super::files::merge(&mut item, changes);
        // Groups were To do's; an ask has none.
        item.remove("group");
        if to == "goals" { item.entry("goal").or_insert(json!("")); }
        // Back into Scheduled with a due time already past (one the helper moved out): it would bounce straight back.
        if to == "scheduled" && from != "scheduled" && item.get("due").and_then(Value::as_f64).is_some_and(|d| d <= now_s()) { item.remove("due"); }
        item
    };
    root.locked(|| {
        edit_locked(root, from, |its| {
            let Some(k) = its.iter().position(|i| i.get("id").and_then(Value::as_str) == Some(id)) else { return Ok(None) };
            let item = place(its.remove(k));
            if same {
                its.push(item.clone());
            } else {
                let placed = item.clone();
                edit_locked(root, to, |dest| { dest.push(placed); Ok(()) })?;
            }
            Ok(Some(item))
        })
    })
}

/// Change an item in place (by id); returns it after the change.
pub fn change_item(root: &Root, list: &str, id: &str, changes: &Map<String, Value>) -> Result<Option<Map<String, Value>>> {
    edit_list(root, list, |its| {
        Ok(its.iter_mut().find(|i| i.get("id").and_then(Value::as_str) == Some(id)).map(|i| {
            super::files::merge(i, changes);
            i.clone()
        }))
    })
}

/// Where a closed plan island's last content waits (bug 2026-10-23T23-46-23: a stray click closed To do and nothing
/// brought it back): hidden beside the lists, so the helper never shows it.
fn closed_file(root: &Root, list: &str) -> std::path::PathBuf { crate::goals::ops::closed_file(root, list) }

/// Hidden by the user's close (the app writes `hidden: true` in place of deleting the file).
pub fn hidden(o: &Map<String, Value>) -> bool { o.get("hidden").and_then(Value::as_bool) == Some(true) }

/// The user closed a plan island: keep its last content for `reopen_closed`, from the file (hidden, as the app leaves it
/// now), else what the watcher saw (`seen`), else the copy kept at its last write (an app that still deletes the
/// file, a close before the watcher ever saw it: bug note 2026-10-31T15-57-16). Never deleted.
pub fn keep_closed(root: &Root, list: &str, seen: Option<&Map<String, Value>>) -> Result<()> {
    let list = canonical(list);
    root.locked(|| {
        let o = root.read(list).or_else(|| seen.cloned()).or_else(|| crate::goals::ops::kept(root, list));
        match o {
            Some(o) => super::files::write_atomic(&closed_file(root, list), &Value::Object(o)),
            None => { eprintln!("islands: {list} closed, and no copy of it was ever written"); Ok(()) }
        }
    })
}

/// A plan list taken off screen by Heimdall (`islands_remove`, any delete path): hidden in place with its copy kept,
/// never deleted (review 31 Oct: removing Goals, then a fresh file, lost every goal). Returns its title.
pub fn hide(root: &Root, list: &str) -> Result<Option<String>> {
    let list = canonical(list);
    root.locked(|| {
        let Some(mut o) = crate::goals::ops::current(root, list) else { return Ok(None) };
        o.insert("hidden".into(), json!(true));
        write_list(root, list, &o)?;
        super::files::write_atomic(&closed_file(root, list), &Value::Object(o.clone()))?;
        Ok(Some(o.get("title").and_then(Value::as_str).unwrap_or(list_title(list)).to_owned()))
    })
}

/// Show every closed plan island again as it was (`refresh_plan`, each plan refresher pass, `plan_edit` show): a
/// hidden one is un-hidden, a missing one written back from the copy kept at its close (else at its last write).
/// A list rewritten meanwhile keeps what it has now. Returns the lists shown again.
pub fn reopen_closed(root: &Root) -> Result<Vec<&'static str>> {
    let waiting = |l: &str| closed_file(root, l).exists() || {
        let disk = root.read(l);
        disk.as_ref().map_or(crate::goals::ops::kept_file(root, l).exists(), hidden) || (l == "goals" && crate::goals::ops::current(root, l) != disk)
    };
    if !LISTS.iter().any(|l| waiting(l.0)) { return Ok(Vec::new()) }
    root.locked(|| {
        let mut back = Vec::new();
        for (list, _, _) in LISTS {
            if show_locked(root, list)?.is_some() { back.push(list) }
        }
        Ok(back)
    })
}

/// One plan list shown again, holding the lock: `Some(list as now on screen)` when it was hidden, missing, or (Goals)
/// had lost its goals to a fresh file while a copy kept them: the copy's goals with every ask (`goals::ops::current`).
fn show_locked(root: &Root, list: &str) -> Result<Option<Map<String, Value>>> {
    let closed = closed_file(root, list);
    let disk = root.read(list);
    let out = match crate::goals::ops::current(root, list) {
        Some(mut o) if disk.as_ref() != Some(&o) || hidden(&o) => {
            o.remove("hidden");
            write_list(root, list, &o)?;
            Some(o)
        }
        _ => None,
    };
    if closed.exists() { std::fs::remove_file(&closed)?; }
    Ok(out)
}

/// `plan_edit` show: one list on screen again (un-hidden, or written back), as it is now.
pub fn show(root: &Root, list: &str) -> Result<Map<String, Value>> {
    let list = canonical(list);
    root.locked(|| Ok(match show_locked(root, list)? { Some(o) => o, None => root.read(list).unwrap_or_else(|| fresh(list)) }))
}

/// What `return_due` did.
#[derive(Debug, PartialEq)]
pub enum Returned { Back, AlreadyTold, NotThere }

/// A Scheduled item an older islands app moved into `todo.json` when it fell due: back into Scheduled, due (the user,
/// 31 Oct 17:58: no To do list; a due Scheduled item stays in Scheduled), stamped `told` with its due time, and a To do
/// left empty goes. One already told for that due time stays where it is (review 31 Oct: that helper moves a past due
/// item out of Scheduled at every change, so moving it back looped forever, a notice each round).
pub fn return_due(root: &Root, id: &str) -> Result<Returned> {
    root.locked(|| {
        let Some(mut todo) = root.read("todo") else { return Ok(Returned::NotThere) };
        let Some(arr) = todo.get_mut("items").and_then(Value::as_array_mut) else { return Ok(Returned::NotThere) };
        let Some(k) = arr.iter().position(|i| i["id"] == id) else { return Ok(Returned::NotThere) };
        if told(arr[k].as_object()) { return Ok(Returned::AlreadyTold) }
        let Value::Object(mut item) = arr.remove(k) else { return Ok(Returned::NotThere) };
        let empty = arr.is_empty();
        item.remove("group");
        if let Some(d) = item.get("due").cloned() { item.insert("told".into(), d); }
        let mut sched = crate::goals::ops::current(root, "scheduled").unwrap_or_else(|| fresh("scheduled"));
        let its = sched.entry("items").or_insert(json!([]));
        if let Some(a) = its.as_array_mut() { if !a.iter().any(|i| i["id"] == id) { a.push(Value::Object(item)) } }
        write_list(root, "scheduled", &sched)?;
        if empty { std::fs::remove_file(root.file("todo"))? } else { super::files::write_atomic(&root.file("todo"), &Value::Object(todo))? }
        Ok(Returned::Back)
    })
}

/// The item was already told for its due time (`told` equals `due`).
fn told(o: Option<&Map<String, Value>>) -> bool {
    o.is_some_and(|o| o.get("due").is_some_and(|d| !d.is_null()) && o.get("told") == o.get("due"))
}

/// A Scheduled item that fell due, told once: `Some((title, first))`, `first` false when it was already told for that
/// due time (then no notice); it is stamped `told: <due>`. `None` when Scheduled has no such item.
pub fn tell_due(root: &Root, id: &str) -> Result<Option<(String, bool)>> {
    root.locked(|| {
        let Some(mut sched) = crate::goals::ops::current(root, "scheduled") else { return Ok(None) };
        let Some(its) = sched.get_mut("items").and_then(Value::as_array_mut) else { return Ok(None) };
        let Some(item) = its.iter_mut().filter_map(Value::as_object_mut).find(|i| i.get("id").and_then(Value::as_str) == Some(id)) else { return Ok(None) };
        let title = item.get("title").and_then(Value::as_str).unwrap_or(id).to_owned();
        if told(Some(item)) { return Ok(Some((title, false))) }
        if let Some(d) = item.get("due").cloned().filter(|d| !d.is_null()) { item.insert("told".into(), d); }
        write_list(root, "scheduled", &sched)?;
        Ok(Some((title, true)))
    })
}

/// The list an island id belongs to, if it is a plan island.
pub fn list_of(island: &str, o: Option<&Map<String, Value>>) -> Option<&'static str> {
    if let Some(l) = o.and_then(|o| o.get("list")).and_then(Value::as_str) {
        if let Some(found) = LISTS.iter().find(|x| x.0 == canonical(l)) { return Some(found.0) }
    }
    LISTS.iter().find(|x| x.0 == canonical(island)).map(|x| x.0)
}

/// The user's choice on an approval: Yes moves the item to Running (the work goes ahead), Done ticks it off (he looked, he
/// did it), Later to Discussed (no decision yet), No takes it off the plan. The proposal goes with the decision.
/// Returns the item's title. On Goals an ask he answered leaves at once (Later keeps it, chosen "later").
pub fn approve(root: &Root, list: &str, id: &str, action: &str) -> Result<Option<String>> {
    let goals = canonical(list) == "goals";
    edit_list(root, list, |its| {
        let Some(k) = its.iter().position(|i| i.get("id").and_then(Value::as_str) == Some(id)) else { return Ok(None) };
        let title = its[k].get("title").and_then(Value::as_str).unwrap_or(id).to_string();
        if goals {
            if action == "later" {
                its[k].remove("proposal");
                its[k].insert("chosen".into(), json!("later"));
            } else {
                its.remove(k);
            }
            return Ok(Some(title));
        }
        match action {
            "no" => { its.remove(k); }
            "done" => {
                let i = &mut its[k];
                i.remove("proposal");
                i.insert("done".into(), json!(true));
                i.insert("chosen".into(), json!("done"));
            }
            other => {
                let i = &mut its[k];
                i.remove("proposal");
                i.insert("group".into(), json!(if other == "later" { "discussed" } else { "running" }));
                i.insert("chosen".into(), json!(other));
            }
        }
        Ok(Some(title))
    })
}

/// The user ticked (`done`) or unticked (`undo`) an item. Returns its title. On Goals: a ticked ask is answered and
/// leaves; a ticked task is done and marked his (`ticked`: the shadow never moves it back), unticked it is in hand
/// again.
pub fn tick(root: &Root, list: &str, id: &str, done: bool) -> Result<Option<String>> {
    if canonical(list) == "goals" { return tick_goals(root, id, done) }
    let changed = change_item(root, list, id, json!({"done": done}).as_object().unwrap())?;
    Ok(changed.map(|i| i.get("title").and_then(Value::as_str).unwrap_or(id).to_string()))
}

fn tick_goals(root: &Root, id: &str, done: bool) -> Result<Option<String>> {
    root.locked(|| {
        let Some(mut o) = root.read("goals") else { return Ok(None) };
        let mut title = None;
        if let Some(items) = o.get_mut("items").and_then(Value::as_array_mut) {
            if let Some(k) = items.iter().position(|i| i["id"] == id) {
                title = items[k]["title"].as_str().map(str::to_owned);
                if done { items.remove(k); } else { items[k]["done"] = json!(false); }
            }
        }
        if title.is_none() {
            for g in o.get_mut("goals").and_then(Value::as_array_mut).into_iter().flatten() {
                let Some(t) = g.get_mut("tasks").and_then(Value::as_array_mut).and_then(|ts| ts.iter_mut().find(|t| t["id"] == id)) else { continue };
                title = t["title"].as_str().map(str::to_owned);
                let t = t.as_object_mut().unwrap();
                if done {
                    t.insert("state".into(), json!("done"));
                    t.insert("ticked".into(), json!(true));
                    t.insert("at".into(), json!(super::tools::now_s() as i64));
                } else {
                    t.insert("state".into(), json!("now"));
                    t.remove("ticked");
                }
                break;
            }
        }
        if title.is_some() { write_list(root, "goals", &o)?; }
        Ok(title)
    })
}

/// The shape of the user's real `settled.json` on 22 Oct (copied, read-only): no `preset`/`list`, nine plain strings.
#[cfg(test)]
pub(crate) const REAL_SETTLED: &str = r#"{
  "edgeY" : 157, "expanded" : false, "icon" : "checkmark.seal", "id" : "settled",
  "items" : [
    "Islands into Heimdall, bound to threads", "Drop replies you talked over", "Island closes when message is sent",
    "To do: animations, approvals, Settled group", "Arsenal note in my prompt", "Arsenal polish list, models, peak memory",
    "Repair OCR and translation models", "Simple add-a-model, model folders", "Homebrew tap, prepared not published"
  ],
  "order" : 9, "side" : "left", "stage" : "Running", "started" : 1792623966.3, "state" : "running",
  "summary" : "Night pass: Heimdall chain one stage at a time; Arsenal in parallel", "title" : "Scheduled", "updated" : 1792630210.3
}"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn real_settled(r: &Root) -> Map<String, Value> {
        let o: Map<String, Value> = serde_json::from_str(REAL_SETTLED).unwrap();
        r.ensure().unwrap();
        std::fs::write(r.file("settled"), REAL_SETTLED).unwrap();
        o
    }

    #[test]
    fn owner_s_old_settled_list_is_migrated_in_place_once_after_one_backup() {
        let (_d, r) = root();
        let before = real_settled(&r);
        assert_eq!(migrate_all(&r).unwrap(), ["settled"]);
        let o = r.read("settled").unwrap();
        assert_eq!((o["preset"].as_str(), o["list"].as_str()), (Some("todo"), Some("settled")));
        let its = items(&o);
        let titles: Vec<&str> = its.iter().map(|i| i["title"].as_str().unwrap()).collect();
        let was: Vec<&str> = before["items"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
        assert_eq!(titles, was, "every item's text kept, in order");
        assert!(o["items"].as_array().unwrap().iter().all(|v| v["id"].as_str().is_some_and(|id| !id.is_empty())));
        assert_eq!(its[0]["id"], "islands-into-heimdall-bound-to-threads");
        for k in ["edgeY", "expanded", "icon", "order", "side", "stage", "started", "state", "summary", "title", "updated"] {
            assert_eq!(o[k], before[k], "{k} kept");
        }
        // One backup beside islands/, holding the original bytes.
        let backups: Vec<_> = std::fs::read_dir(&r.0).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().starts_with("backup-")).collect();
        assert_eq!(backups.len(), 1);
        assert_eq!(std::fs::read_to_string(backups[0].path().join("settled.json")).unwrap(), REAL_SETTLED);
        // Second contact: nothing to migrate, no second backup; a later old-shape write migrates without one.
        assert!(migrate_all(&r).unwrap().is_empty());
        r.put("goals", json!({"items": ["Plain"]}).as_object().unwrap()).unwrap();
        assert_eq!(migrate_all(&r).unwrap(), ["goals"]);
        assert_eq!(std::fs::read_dir(&r.0).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().starts_with("backup-")).count(), 1);
    }

    #[test]
    fn a_plan_edit_on_an_old_shape_list_migrates_it_and_keeps_the_strings() {
        let (_d, r) = root();
        real_settled(&r);
        let f = find(&r, "Arsenal note in my prompt").unwrap();
        assert_eq!(f.list, "settled");
        tick(&r, "settled", f.item["id"].as_str().unwrap(), true).unwrap();
        let o = r.read("settled").unwrap();
        assert_eq!((o["preset"].as_str(), o["list"].as_str(), o["title"].as_str()), (Some("todo"), Some("settled"), Some("Scheduled")));
        assert_eq!(items(&o).len(), 9);
        assert_eq!(items(&o)[4]["done"], true);
        assert!(std::fs::read_dir(&r.0).unwrap().flatten().any(|e| e.file_name().to_string_lossy().starts_with("backup-")));
    }

    #[test]
    fn a_move_is_never_seen_half_done_by_a_locked_reader() {
        let (_d, r) = root();
        r.put("goals", json!({"items": [{"id": "x", "title": "X"}, {"id": "y", "title": "Y"}]}).as_object().unwrap()).unwrap();
        r.put("scheduled", json!({"items": []}).as_object().unwrap()).unwrap();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (r2, stop2) = (r.clone(), stop.clone());
        let reader = std::thread::spawn(move || {
            let mut reads = 0;
            while !stop2.load(std::sync::atomic::Ordering::Acquire) {
                let n = r2.locked(|| Ok(["goals", "scheduled"].iter()
                    .map(|l| items(&r2.read(l).unwrap()).iter().filter(|i| i["id"] == "x").count()).sum::<usize>())).unwrap();
                assert_eq!(n, 1, "x is in exactly one list");
                reads += 1;
            }
            reads
        });
        for k in 0..60 {
            let (from, to) = if k % 2 == 0 { ("goals", "scheduled") } else { ("scheduled", "goals") };
            assert!(move_item(&r, from, "x", to, &Map::new()).unwrap().is_some());
        }
        stop.store(true, std::sync::atomic::Ordering::Release);
        assert!(reader.join().unwrap() > 0);
        assert_eq!(items(&r.read("goals").unwrap()).len(), 2);
        // A move within one list keeps one copy.
        move_item(&r, "goals", "x", "goals", json!({"group": "running"}).as_object().unwrap()).unwrap();
        assert_eq!(items(&r.read("goals").unwrap()).iter().filter(|i| i["id"] == "x").count(), 1);
    }

    fn root() -> (tempfile::TempDir, Root) {
        let d = tempfile::tempdir().unwrap();
        let r = Root(d.path().to_path_buf());
        (d, r)
    }

    #[test]
    fn plain_items_get_stable_unique_ids() {
        let o = json!({"items": ["Arsenal: push branch", {"title": "Arsenal: push branch"}, {"id": "x", "title": "Y"}, 3]});
        let its = items(o.as_object().unwrap());
        let ids: Vec<_> = its.iter().map(|i| i["id"].as_str().unwrap().to_string()).collect();
        assert_eq!(ids, ["arsenal-push-branch", "arsenal-push-branch-2", "x"]);
    }

    #[test]
    fn a_new_list_is_created_docked_left_and_settled_starts_as_a_pill() {
        let (_d, r) = root();
        edit_list(&r, "settled", |its| { its.push(json!({"id": "a", "title": "A"}).as_object().unwrap().clone()); Ok(()) }).unwrap();
        let o = r.read("settled").unwrap();
        assert_eq!((o["preset"].as_str(), o["list"].as_str(), o["title"].as_str(), o["side"].as_str()), (Some("todo"), Some("settled"), Some("Settled"), Some("left")));
        assert_eq!(o["order"], 2);
        assert_eq!(o["expanded"], false);
    }

    #[test]
    fn an_answered_ask_leaves_goals_later_keeps_it_and_unknown_keys_survive() {
        let (_d, r) = root();
        r.put("goals", json!({"preset": "todo", "list": "goals", "title": "Goals", "mine": 1, "goals": [], "items": [
            {"id": "a", "title": "Delete caches", "proposal": {"text": "rm ~/x"}},
            {"id": "b", "title": "Rename", "proposal": {"text": "mv"}, "extra": true},
            {"id": "c", "title": "Push", "proposal": {"text": "git push"}},
            {"id": "d", "title": "Check the page"}]}).as_object().unwrap()).unwrap();
        assert_eq!(approve(&r, "goals", "a", "yes").unwrap().as_deref(), Some("Delete caches"));
        assert_eq!(approve(&r, "todo", "b", "later").unwrap().as_deref(), Some("Rename"), "todo still means Goals");
        assert_eq!(approve(&r, "goals", "c", "no").unwrap().as_deref(), Some("Push"));
        assert_eq!(approve(&r, "goals", "d", "done").unwrap().as_deref(), Some("Check the page"));
        assert_eq!(approve(&r, "goals", "gone", "yes").unwrap(), None);
        let o = r.read("goals").unwrap();
        assert_eq!((o["mine"].as_i64(), o["goals"].as_array().map(Vec::len)), (Some(1), Some(0)));
        let its = items(&o);
        assert_eq!(its.len(), 1);
        assert_eq!((its[0]["id"].as_str(), its[0]["chosen"].as_str(), its[0].contains_key("proposal"), its[0]["extra"].as_bool()), (Some("b"), Some("later"), false, Some(true)));
    }

    #[test]
    fn ticks_answer_asks_finish_tasks_as_owners_and_mark_other_lists() {
        let (_d, r) = root();
        r.put("goals", json!({"preset": "todo", "list": "goals", "items": [{"id": "a", "title": "Pick a cut"}],
            "goals": [{"id": "g", "title": "Ship", "tasks": [{"id": "t", "title": "Scan", "state": "now"}]}]}).as_object().unwrap()).unwrap();
        assert_eq!(tick(&r, "goals", "a", true).unwrap().as_deref(), Some("Pick a cut"));
        assert!(items(&r.read("goals").unwrap()).is_empty(), "an answered ask leaves");
        assert_eq!(tick(&r, "goals", "t", true).unwrap().as_deref(), Some("Scan"));
        let t = r.read("goals").unwrap()["goals"][0]["tasks"][0].clone();
        assert_eq!((t["state"].as_str(), t["ticked"].as_bool(), t["at"].is_i64()), (Some("done"), Some(true), true));
        tick(&r, "goals", "t", false).unwrap();
        let t = r.read("goals").unwrap()["goals"][0]["tasks"][0].clone();
        assert_eq!((t["state"].as_str(), t.get("ticked")), (Some("now"), None));
        assert_eq!(tick(&r, "goals", "none", true).unwrap(), None);
        r.put("scheduled", json!({"items": [{"id": "s", "title": "S"}]}).as_object().unwrap()).unwrap();
        assert_eq!(tick(&r, "scheduled", "s", true).unwrap().as_deref(), Some("S"));
        assert_eq!(items(&r.read("scheduled").unwrap())[0]["done"], true);
        tick(&r, "scheduled", "s", false).unwrap();
        assert_eq!(items(&r.read("scheduled").unwrap())[0]["done"], false);
    }

    /// Bug note 2026-10-31T15-57-16: closing To do deleted the whole list. A closed plan list is hidden or kept, never
    /// lost, and comes back as it was.
    #[test]
    fn a_closed_plan_list_is_hidden_and_kept_and_reopen_shows_it_again() {
        let (_d, r) = root();
        edit_list(&r, "goals", |its| { its.push(json!({"id": "a", "title": "Approve the voice"}).as_object().unwrap().clone()); Ok(()) }).unwrap();
        // The app now hides it in place: reopen un-hides, keeping everything.
        r.patch("goals", |o| { o.insert("hidden".into(), json!(true)); }).unwrap();
        keep_closed(&r, "goals", None).unwrap();
        assert_eq!(reopen_closed(&r).unwrap(), ["goals"]);
        let o = r.read("goals").unwrap();
        assert!(!o.contains_key("hidden") && items(&o)[0]["title"] == "Approve the voice");
        assert!(reopen_closed(&r).unwrap().is_empty());
        // An app that still deletes the file, before the watcher ever saw the list (no `seen` copy): the copy kept at
        // its last write brings it back.
        r.remove("goals").unwrap();
        keep_closed(&r, "todo", None).unwrap();
        assert_eq!(reopen_closed(&r).unwrap(), ["goals"]);
        assert_eq!(items(&r.read("goals").unwrap())[0]["id"], "a");
        // Even with no close reported at all, a plan list is never just gone.
        r.remove("goals").unwrap();
        assert_eq!(reopen_closed(&r).unwrap(), ["goals"]);
        // Shown on request: a hidden one comes back, a shown one stays as it is.
        r.patch("goals", |o| { o.insert("hidden".into(), json!(true)); }).unwrap();
        assert!(!show(&r, "goals").unwrap().contains_key("hidden"));
        assert!(!r.read("goals").unwrap().contains_key("hidden"));
        assert_eq!(show(&r, "scheduled").unwrap()["list"], "scheduled", "a list never written shows fresh");
        // A list rewritten after the close keeps what it has now.
        r.remove("goals").unwrap();
        keep_closed(&r, "goals", Some(json!({"items": [{"id": "old", "title": "Old"}]}).as_object().unwrap())).unwrap();
        crate::islands::files::write_atomic(&r.file("goals"), &json!({"list": "goals", "items": []})).unwrap();
        assert!(reopen_closed(&r).unwrap().is_empty());
        assert!(items(&r.read("goals").unwrap()).is_empty());
    }

    #[test]
    fn find_by_id_title_or_unique_fragment_and_move_between_lists() {
        let (_d, r) = root();
        r.put("goals", json!({"items": [{"id": "a", "title": "Arsenal: push branch", "group": "waiting"}, {"id": "b", "title": "Arsenal: tests"}]}).as_object().unwrap()).unwrap();
        assert_eq!(find(&r, "b").unwrap().item["title"], "Arsenal: tests");
        assert_eq!(find(&r, "push").unwrap().item["id"], "a");
        assert!(find(&r, "arsenal").unwrap_err().to_string().contains("several"));
        assert!(find(&r, "nothing").unwrap_err().to_string().contains("no item"));
        let moved = move_item(&r, "goals", "a", "scheduled", json!({"when": "Tonight"}).as_object().unwrap()).unwrap().unwrap();
        assert!(!moved.contains_key("group"));
        assert_eq!(items(&r.read("scheduled").unwrap())[0]["when"], "Tonight");
        assert_eq!(items(&r.read("goals").unwrap()).len(), 1);
    }

    #[test]
    fn due_times_read_local_and_offset_forms() {
        use chrono::{Local, TimeZone};
        let local = Local.with_ymd_and_hms(2026, 10, 24, 10, 0, 0).unwrap().timestamp();
        assert_eq!(due_seconds("2026-10-24T10:00").unwrap(), local);
        assert_eq!(due_seconds(" 2026-10-24 10:00 ").unwrap(), local);
        assert_eq!(due_seconds("2026-10-24T10:00:00").unwrap(), local);
        assert_eq!(due_seconds("2026-10-24T08:00:00Z").unwrap(), 1792828800);
        assert_eq!(due_seconds("2026-10-24T10:00:00+02:00").unwrap(), 1792828800);
        assert!(due_seconds("Tonight").is_err());
        assert!(due_seconds("2026-10-24").is_err());
    }
}
