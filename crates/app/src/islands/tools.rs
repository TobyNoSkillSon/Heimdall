//! The agent's island tools on Heimdall's MCP server (night 1022): `islands_put`, `islands_view`, `islands_remove`,
//! `plan_edit` (Goals' asks, Scheduled, Settled). Shapes are checked by `agent::mcp_contract`; the words here, by `islands/docs/island-style.md` (the
//! same checks as `islands put`). Every result is one short line.

use super::files::{safe_id, write_atomic};
use super::plan;
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Map, Value};

pub fn now_s() -> f64 {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs_f64();
    (t * 10.0).round() / 10.0
}

fn words(s: &str) -> usize { s.split_whitespace().count() }

/// The checkable half of island-style.md for what this call sets.
pub fn style_problem(id: &str, over: &Map<String, Value>, new: bool) -> Option<String> {
    let title = over.get("title").and_then(Value::as_str);
    if new && title.is_none_or(|t| t.trim().is_empty()) {
        return Some("a new island needs a title: 1-3 words naming the thing (\"Qwen-Image\", \"iPad sale\")".into());
    }
    if let Some(t) = title {
        let n = words(t);
        if !(1..=3).contains(&n) { return Some(format!("title is 1-3 words naming the thing; \"{t}\" has {n}")) }
        let norm = |s: &str| s.to_lowercase().replace(['_', ' '], "-");
        if t == t.to_lowercase() && norm(t) == norm(id) {
            return Some(format!("title names the thing for User, not an id: \"{t}\""));
        }
    }
    if let Some(s) = over.get("stage").and_then(Value::as_str) {
        if words(s) > 2 { return Some(format!("stage is one or two present-tense words (\"Downloading\", \"Final tests\"); \"{s}\" has {}", words(s))) }
    }
    if let Some(s) = over.get("summary").and_then(Value::as_str) {
        if s.contains('\n') { return Some("summary is one line".into()) }
        let n = s.chars().count();
        if n > 110 { return Some(format!("summary is one plain line (110 characters at most); this one has {n}")) }
    }
    None
}

const SHOWN: [&str; 15] = ["title", "state", "stage", "progress", "value", "summary", "steps", "items", "kv", "image", "open", "parts", "path", "preset", "icon"];

/// The CLI's `stamp`: `started` on creation, `updated` when what it shows changes, `ended` when it ends.
pub fn stamp(o: &mut Map<String, Value>, old: Option<&Map<String, Value>>) {
    let now = now_s();
    if !o.contains_key("started") && old.is_none() { o.insert("started".into(), json!(now)); }
    let changed = old.is_none_or(|old| SHOWN.iter().any(|k| old.get(*k) != o.get(*k)));
    if changed { o.insert("updated".into(), json!(now)); }
    let state = |m: &Map<String, Value>| m.get("state").and_then(Value::as_str).unwrap_or("running").to_string();
    let (now_state, was) = (state(o), old.map(state));
    if now_state == "done" || now_state == "failed" {
        if was.as_deref() != Some(now_state.as_str()) || !o.contains_key("ended") { o.insert("ended".into(), json!(now)); }
    } else {
        o.remove("ended");
    }
}

/// `stamp` after Heimdall changed an island in place by `patch` (thread sync).
pub fn stamp_now(o: &mut Map<String, Value>, patch: &Map<String, Value>) {
    let now = now_s();
    if patch.keys().any(|k| SHOWN.contains(&k.as_str())) { o.insert("updated".into(), json!(now)); }
    match patch.get("state").and_then(Value::as_str) {
        Some("done" | "failed") => { o.insert("ended".into(), json!(now)); }
        Some(_) => { o.remove("ended"); }
        None => {}
    }
}

fn title_of(o: &Map<String, Value>, id: &str) -> String {
    o.get("title").and_then(Value::as_str).unwrap_or(id).to_string()
}

/// `islands_put`: one island, created or merged; `thread` binds it to a T3 thread.
pub fn put(h: &super::Host, a: &Value) -> Result<String> {
    let id = safe_id(a["id"].as_str().context("id: required")?);
    ensure!(plan::list_of(&id, None).is_none(), "{id} is a plan list; change it with plan_edit");
    let mut over = Map::new();
    for k in ["title", "state", "stage", "summary", "image", "open", "side", "expanded"] {
        if let Some(v) = a.get(k) { over.insert(k.into(), v.clone()); }
    }
    for k in ["steps", "items", "kv", "parts"] {
        if let Some(v) = a.get(k) {
            over.insert(k.into(), if v.as_array().is_some_and(|l| l.is_empty()) { Value::Null } else { v.clone() });
        }
    }
    if over.contains_key("side") { over.insert("position".into(), Value::Null); over.insert("edgeY".into(), Value::Null); }
    // A picture that is not there would draw as a broken image: it is left out and the result says so.
    let mut dropped = None;
    if let Some(Value::String(img)) = over.get("image") {
        if img.trim().is_empty() {
            over.insert("image".into(), Value::Null);
        } else if !super::picture(img) {
            dropped = Some(img.clone());
            over.remove("image");
        }
    }
    // An icon the app can draw (an SF Symbol name or a tiny plain SVG); anything else is refused plainly.
    if let Some(v) = a.get("icon") {
        match v.as_str().map(str::trim) {
            Some("") => { over.insert("icon".into(), Value::Null); }
            Some(i) => match super::icon(i) {
                Some(i) => { over.insert("icon".into(), json!(i)); }
                None => bail!("icon: an SF Symbol name (\"ipad\", \"film.stack\") or a tiny SVG of plain shapes (path, circle, rect, line, polygon; under {} bytes; no text, styles, links or scripts)", super::ICON_SVG_MAX),
            },
            None => bail!("icon: a string"),
        }
    }
    // The goal it serves (a Goals id; "" clears): a chip on the island, and the goal's segment lit blue while it runs.
    if let Some(g) = a.get("goal").and_then(Value::as_str) {
        let g = g.trim();
        if g.is_empty() { over.insert("goal".into(), Value::Null); } else {
            ensure!(goal_known(&h.root, g), "goal: no goal {g} (plan_edit show list goals gives the ids)");
            over.insert("goal".into(), json!(g));
        }
    }
    let sent = a.get("sent").and_then(Value::as_str).map(|s| s.split_whitespace().collect::<Vec<_>>().join(" ")).filter(|s| !s.is_empty());
    if let Some(s) = &sent { ensure!(s.chars().count() <= 110, "sent is one line: what it was sent to do (110 characters at most)"); }
    let thread = a["thread"].as_str().map(str::to_owned);
    let (out, new, newly_bound) = h.root.locked(|| {
        let old = h.root.read(&id);
        if let Some(p) = style_problem(&id, &over, old.is_none()) { bail!("{p}. See islands/docs/island-style.md.") }
        let mut o = old.clone().unwrap_or_default();
        // What it was sent to do is written once and never replaced (a status line ages; the errand does not).
        if let Some(s) = &sent { if o.get("sent").and_then(Value::as_str).is_none_or(str::is_empty) { o.insert("sent".into(), json!(s)); } }
        let was_bound = o.get("thread").and_then(|t| t.get("id")).and_then(Value::as_str).map(str::to_owned);
        super::files::merge(&mut o, &over);
        o.insert("id".into(), json!(id));
        let newly_bound = thread.as_ref().is_some_and(|t| was_bound.as_deref() != Some(t));
        if let Some(t) = &thread {
            o.insert("thread".into(), json!({"id": t, "title": title_of(&o, &id)}));
            // A new thread-bound island drops out of the notch (the app removes the key after the flight).
            if newly_bound { o.insert("origin".into(), json!({"from": "notch"})); }
        }
        stamp(&mut o, old.as_ref());
        write_atomic(&h.root.file(&id), &Value::Object(o.clone()))?;
        Ok((o, old.is_none(), newly_bound))
    })?;
    if let Some(t) = &thread {
        h.bindings.bind(&id, t);
        h.t3_job(super::binding::Job::Sync(id.clone()));
    }
    let title = title_of(&out, &id);
    let line = match (new, newly_bound) {
        (_, true) => format!("Island '{title}' shown, following T3 thread {}.", thread.unwrap_or_default()),
        (true, _) => format!("Island '{title}' shown."),
        _ => format!("Island '{title}' updated."),
    };
    Ok(match dropped {
        Some(img) => format!("{line} Image left out: no picture at {img}."),
        None => line,
    })
}

/// A goal id in Goals (any state).
pub fn goal_known(root: &super::files::Root, g: &str) -> bool {
    root.read("goals").is_some_and(|o| o["goals"].as_array().into_iter().flatten().any(|x| x["id"] == g))
}

/// The goal a job serves, from facts in the file only: the job's title names one of a goal's open steps (`now` or
/// `next`, the Settled rule's same words). None otherwise: Heimdall names it with `islands_put` goal (review 2 Nov: a
/// project word alone lit "Ship Heimdall publicly" for every Heimdall bug fix).
pub fn goal_for(root: &super::files::Root, title: &str) -> Option<String> {
    let o = root.read("goals")?;
    let job = title.split_once(':').map_or(title, |(_, rest)| rest).trim();
    let hits: Vec<String> = o["goals"].as_array().into_iter().flatten().filter_map(Value::as_object)
        .filter(|g| crate::goals::ops::goal_state(g) == "active")
        .filter(|g| g.get("tasks").and_then(Value::as_array).into_iter().flatten()
            .any(|t| matches!(t["state"].as_str(), Some("now" | "next")) && t["title"].as_str().is_some_and(|x| crate::goals::settled::same(x, job) || crate::goals::settled::same(x, title))))
        .filter_map(|g| g.get("id").and_then(Value::as_str).map(str::to_owned)).collect();
    if hits.len() == 1 { hits.into_iter().next() } else { None }
}

/// The line that tells Heimdall the next proposal is on screen now.
fn next_line(list: &str, next: Option<Map<String, Value>>) -> String {
    next.map(|p| format!(" Next on screen: {}", super::proposals::notice(list, &p))).unwrap_or_default()
}

/// `islands_view`: any file in a viewer island, with optional proposed-action buttons.
pub fn view(h: &super::Host, a: &Value) -> Result<String> {
    let path = std::path::Path::new(a["path"].as_str().context("path: required")?);
    ensure!(path.is_absolute(), "path: use an absolute path");
    ensure!(path.exists(), "no file at {}", path.display());
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let id = format!("view-{}", safe_id(name));
    let title = a["title"].as_str().map(str::to_owned)
        .unwrap_or_else(|| path.file_stem().and_then(|s| s.to_str()).unwrap_or(name).to_string());
    if a.get("title").is_some() {
        if let Some(p) = style_problem(&id, json!({"title": title}).as_object().unwrap(), false) { bail!("{p}. See islands/docs/island-style.md.") }
    }
    let mut over = json!({"preset": "viewer", "path": path.to_string_lossy(), "title": title, "expanded": true, "hidden": false});
    over["actions"] = a.get("actions").filter(|v| v.as_array().is_some_and(|l| !l.is_empty())).cloned().unwrap_or(Value::Null);
    h.root.locked(|| {
        let old = h.root.read(&id);
        let mut o = old.clone().unwrap_or_default();
        super::files::merge(&mut o, over.as_object().unwrap());
        o.insert("id".into(), json!(id));
        stamp(&mut o, old.as_ref());
        // Shown again: a new `updated` makes the helper look at the file again, so an edited file is read anew
        // (bug 22 Oct 20:46: the viewer kept the first version when the same path was viewed again).
        o.insert("updated".into(), json!(now_s()));
        write_atomic(&h.root.file(&id), &Value::Object(o))
    })?;
    Ok(format!("Showing '{title}' in a viewer."))
}

/// `islands_remove`: islands by id (thread bindings go with them); a plan list is hidden instead.
pub fn remove(h: &super::Host, a: &Value) -> Result<String> {
    let mut gone = Vec::new();
    let mut missing = Vec::new();
    let mut hid = Vec::new();
    for id in a["ids"].as_array().into_iter().flatten().filter_map(Value::as_str) {
        // There is no To do list: "todo" names nothing here (it would hide Goals; review 31 Oct).
        if id == "todo" {
            missing.push("todo (there is no such list; Goals, Scheduled and Settled are the plan lists)".into());
            continue;
        }
        // A plan list is hidden, never deleted: it comes back with show (review 31 Oct).
        if let Some(list) = plan::list_of(id, None) {
            match plan::hide(&h.root, list)? { Some(t) => hid.push(t), None => missing.push(id.to_string()) }
            continue;
        }
        let title = h.root.read(id).map(|o| title_of(&o, id));
        h.bindings.unbind(&safe_id(id));
        match (h.root.remove(id)?, title) {
            (true, t) => gone.push(t.unwrap_or_else(|| id.to_string())),
            (false, _) => missing.push(id.to_string()),
        }
    }
    let mut line = match gone.len() {
        0 => String::new(),
        1 => format!("Removed '{}'.", gone[0]),
        n => format!("Removed {n} islands."),
    };
    for t in hid {
        if !line.is_empty() { line.push(' ') }
        line.push_str(&format!("Hid '{t}' (a plan list is hidden, never removed; plan_edit show brings it back)."));
    }
    if !missing.is_empty() {
        if !line.is_empty() { line.push(' ') }
        line.push_str(&format!("No island {}.", missing.join(", ")));
    }
    Ok(line)
}

/// The one answer to a promise refused at Scheduled's door: both ways in, by field name, so an agent with an older tool
/// list still finds them (bug note 2026-11-02T23-28-44).
const SCHEDULED_DOOR: &str = "a promise enters Scheduled on User's yes. If he asked for it, add it with his own words in the quote field (action add, list scheduled, item, due, quote: \"<his exact words>\"), never words he did not say. If he did not, use action propose (list scheduled, item, due, why) and he says yes or clicks it";

/// The same for a move into Scheduled: the move itself takes his words (an add would leave the original behind).
const SCHEDULED_MOVE: &str = "a promise enters Scheduled on User's yes. If he asked for it, move it with his own words in the quote field (action move, list scheduled, item, due, quote: \"<his exact words>\"), never words he did not say. If he did not, use action propose (list scheduled, item, due, why) and he says yes or clicks it";

/// A quoted span that opens `s` (straight or curly double quotes), trimmed: the user's words written at the head of a detail.
fn leading_quote(s: &str) -> Option<&str> {
    let s = s.trim_start();
    let (open, close) = [('"', '"'), ('\u{201c}', '\u{201d}')].into_iter().find(|(o, _)| s.starts_with(*o))?;
    let rest = &s[open.len_utf8()..];
    let end = rest.find(close)?;
    Some(rest[..end].trim()).filter(|q| !q.is_empty())
}

/// `plan_edit` (one system, `docs/one-system.md`): nothing enters Goals or Scheduled without his yes.
/// `propose` drafts a goal, a next step under a goal, or a promise with a time; `accept` is his yes by voice (`quote`
/// his words; `due` when he named another time), `decline` his no. `add` puts an ask in Goals (something only he can do
/// now; it shows on Scheduled's guarantee line), a promise in Scheduled only with a time and his own words asking for it
/// (`quote`), and something in Settled only with its proof. Move, tick, undo, remove change what is there; `show` puts
/// a list on screen again and returns what it holds, proposals included. "todo" is Goals (old callers).
pub fn plan_edit(h: &super::Host, a: &Value) -> Result<String> {
    let root = &h.root;
    let action = a["action"].as_str().context("action: required")?;
    let list = a["list"].as_str().map(plan::canonical);
    if action == "show" {
        let list = list.unwrap_or("goals");
        let o = plan::show(root, list)?;
        return Ok(list_text(list, &o));
    }
    let item = a["item"].as_str().context("item: required")?.trim();
    let now = now_s() as i64;
    // His words: `quote`, or a quoted span that opens `detail` (bug note 2026-11-02T23-28-44: a relay thread whose tool
    // list predated `quote` put "his words" in detail and was refused, with no field it knew of to put them in).
    // Only Scheduled's door takes the detail form: accept and Settled's proof take the quote field alone (fresh review,
    // 3 Nov: a detail opening with a quoted title would have settled an item or counted as his yes).
    let quote = a.get("quote").and_then(Value::as_str).unwrap_or("").trim();
    let promise_quote = Some(quote).filter(|q| !q.is_empty())
        .or_else(|| a.get("detail").and_then(Value::as_str).and_then(leading_quote)).unwrap_or("");
    let due_at = a.get("due").and_then(Value::as_str).map(plan::due_seconds).transpose()?;
    match action {
        "propose" => {
            let list = list.unwrap_or(if due_at.is_some() { "scheduled" } else { "goals" });
            ensure!(!item.contains('\n') && item.chars().count() <= 64, "item: one short line naming the thing (\"Release: check model licences\")");
            let goal = a.get("goal").and_then(Value::as_str).unwrap_or("").to_owned();
            let d = super::proposals::Draft {
                list: list.into(),
                kind: if list == "goals" { if goal.is_empty() { "goal".into() } else { "step".into() } } else { String::new() },
                title: item.into(), goal, due: due_at,
                icon: a.get("icon").and_then(Value::as_str).unwrap_or("").into(),
                about: a.get("about").and_then(Value::as_str).unwrap_or("").into(),
                why: a.get("why").and_then(Value::as_str).or_else(|| a.get("detail").and_then(Value::as_str)).unwrap_or(quote).into(),
                by: "heimdall".into(),
            };
            let (id, shown) = super::proposals::propose(root, &d, now)?;
            return Ok(format!("Proposed '{item}' in {} [{id}]{}; say it to User in one short line. His yes: accept; his no: decline; unanswered, it is archived in two days.",
                plan::list_title(list), if shown { ", on screen" } else { ", queued behind the one on screen" }));
        }
        "accept" => {
            ensure!(!quote.is_empty(), "accept is User's yes: quote his words");
            let (title, l, next) = super::proposals::accept(root, list, item, super::proposals::Yes::Voice(quote.into()), due_at, now)?;
            return Ok(format!("'{title}' is in {} on User's yes.{}", plan::list_title(l), next_line(l, next)));
        }
        "decline" => {
            let (title, l, next) = super::proposals::decline(root, list, item, now)?;
            return Ok(format!("Declined '{title}': it left {} and is not proposed again.{}", plan::list_title(l), next_line(l, next)));
        }
        _ => {}
    }
    let mut changes = Map::new();
    for k in ["group", "when", "detail", "proposal", "kind"] {
        if let Some(v) = a.get(k) { changes.insert(k.into(), v.clone()); }
    }
    // Running work is not a plan item: it shows on the right as a job island, its items as steps.
    if changes.get("group").and_then(Value::as_str) == Some("running") {
        bail!("running work goes on the right, not in Goals: show it with islands_put (thread bound, its items as steps)");
    }
    // Groups were To do's: everything in Goals' items is an ask.
    changes.remove("group");
    if let Some(g) = a.get("goal").and_then(Value::as_str) {
        let known = g.is_empty() || root.read("goals").is_some_and(|o| o["goals"].as_array().into_iter().flatten().any(|x| x["id"] == g));
        ensure!(known, "goal: no goal {g} (show list goals for the ids)");
        changes.insert("goal".into(), json!(g));
    }
    if let Some(d) = a.get("due").and_then(Value::as_str) {
        // An absolute due time: the helper counts down and moves the item into Goals as an ask when due.
        let due = plan::due_seconds(d)?;
        ensure!(due > now_s() as i64, "due: {d} is already past; give the time it should happen");
        ensure!(list.is_none_or(|l| l == "scheduled"), "due belongs to Scheduled items (list scheduled)");
        changes.insert("due".into(), json!(due));
        // The date replaces a written "when".
        if !changes.contains_key("when") { changes.insert("when".into(), Value::Null); }
    } else if changes.contains_key("when") {
        // A new word without a date: an old due time would win on screen (or move it straight back into Goals).
        changes.insert("due".into(), Value::Null);
    }
    if changes.contains_key("proposal") || changes.contains_key("kind") || changes.contains_key("goal") {
        // A promise is proposed with the action, not a proposal object (bug note 2026-11-02T23-28-44): say how.
        ensure!(list != Some("scheduled") || !changes.contains_key("proposal"), "{SCHEDULED_DOOR}");
        ensure!(list.unwrap_or("goals") == "goals", "proposals, kinds and goals belong to Goals' asks (list goals)");
    }
    match action {
        "add" => {
            ensure!(!item.contains('\n') && item.chars().count() <= 100, "item: one short line naming the project first (\"Arsenal: push branch\")");
            // A due time is a Scheduled item's.
            let list = list.unwrap_or(if changes.get("due").is_some_and(|d| !d.is_null()) { "scheduled" } else { "goals" });
            if list == "scheduled" {
                // No time, no promise; and only his own words put one there (else: propose it).
                ensure!(changes.get("due").is_some_and(|d| !d.is_null()), "Scheduled holds promises with a time: give due (or propose it)");
                ensure!(!promise_quote.is_empty(), "{SCHEDULED_DOOR}");
                changes.insert("yes".into(), json!({"by": "voice", "at": now, "quote": crate::goals::ops::cap(promise_quote, 200)}));
                changes.remove("when");
            }
            if list == "settled" {
                // Nothing settles without proof.
                // What code can check, never the agent's word: a commit that exists in a repo, an output file that
                // exists; or the user's own words (quote) for a decision. "Tests pass" said by the agent is a claim.
                let p = a.get("proof").and_then(Value::as_str).unwrap_or("").trim();
                let checked = crate::goals::proof::find(p).filter(|v| match v["kind"].as_str() {
                    Some("commit") => crate::goals::proof::commit_exists(v["value"].as_str().unwrap_or("")),
                    Some("file") => true,
                    _ => false,
                });
                let proof = checked.or_else(|| (!quote.is_empty()).then(|| crate::goals::proof::quote(quote, now)))
                    .context("Settled takes proof code can check: a commit hash in a repo or an output file's path (proof), or User's own words (quote)")?;
                changes.insert("proof".into(), proof);
                changes.insert("done".into(), json!(true));
                changes.insert("at".into(), json!(now));
            }
            if list == "goals" {
                changes.entry("goal").or_insert(json!(""));
                changes.insert("asked".into(), json!(now_s() as i64));
            }
            plan::edit_list(root, list, |its| {
                ensure!(!its.iter().any(|i| i.get("title").and_then(Value::as_str) == Some(item)), "'{item}' is already in {}", plan::list_title(list));
                let mut new = json!({"id": plan::unique_id(item, its), "title": item, "done": false}).as_object().unwrap().clone();
                super::files::merge(&mut new, &changes);
                its.push(new);
                Ok(())
            })?;
            Ok(format!("Added '{item}' to {}{}.", plan::list_title(list), if list == "goals" { " (an ask)" } else { "" }))
        }
        "move" => {
            let f = plan::find(root, item)?;
            let id = f.item["id"].as_str().unwrap_or_default().to_string();
            let to = list.unwrap_or(f.list);
            ensure!(to == "scheduled" || !changes.contains_key("due"), "due belongs to Scheduled items: move it with list scheduled");
            // Moving between lists is entering one: Settled only from proof (add with proof), Scheduled only on his
            // words (review 2 Nov: an ask moved into Scheduled became a promise with no yes).
            ensure!(to != "settled" || f.list == "settled", "Settled takes proof: add it there with proof (a commit or an output file) or User's own words");
            if to == "scheduled" {
                let has_due = changes.get("due").is_some_and(|d| !d.is_null()) || (f.list == "scheduled" && f.item.get("due").is_some_and(|d| !d.is_null()) && !changes.contains_key("when"));
                ensure!(has_due, "Scheduled holds promises with a time: give due");
                changes.remove("when");
                if f.list != "scheduled" {
                    ensure!(!promise_quote.is_empty(), "{SCHEDULED_MOVE}");
                    changes.insert("yes".into(), json!({"by": "voice", "at": now, "quote": crate::goals::ops::cap(promise_quote, 200)}));
                }
            }
            if to == f.list {
                plan::change_item(root, to, &id, &changes)?;
            } else {
                plan::move_item(root, f.list, &id, to, &changes)?;
            }
            let title = f.item["title"].as_str().unwrap_or(item);
            Ok(format!("Moved '{title}' to {}.", plan::list_title(to)))
        }
        "tick" | "undo" => {
            let f = plan::find(root, item)?;
            let title = plan::tick(root, f.list, f.item["id"].as_str().unwrap_or_default(), action == "tick")?.unwrap_or_else(|| item.into());
            Ok(match (action, f.list) {
                ("tick", "goals") => format!("Answered '{title}'; it left Goals."),
                ("tick", _) => format!("Ticked '{title}'."),
                _ => format!("Unticked '{title}'."),
            })
        }
        "remove" => {
            let f = plan::find(root, item)?;
            let id = f.item["id"].as_str().unwrap_or_default().to_string();
            plan::edit_list(root, f.list, |its| { its.retain(|i| i.get("id").and_then(Value::as_str) != Some(id.as_str())); Ok(()) })?;
            // Removed stays removed ("remember I didn't want them"): the Goals shadow never writes it again,
            // and nobody proposes it there again (`proposals::removed_here`; bug note 2026-11-01T12-48-26).
            // Written with the kept copy (`write_list`), so a closed or lost file keeps the memory (review 2 Nov).
            if let (Some(t), true) = (f.item.get("title").and_then(Value::as_str), matches!(f.list, "goals" | "scheduled" | "settled")) {
                let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
                let quote = f.item.get("quote").and_then(Value::as_str).unwrap_or("");
                root.locked(|| {
                    let Some(mut o) = crate::goals::ops::current(root, f.list) else { return Ok(()) };
                    crate::goals::settled::remember_removed(&mut o, t, quote, now);
                    crate::goals::ops::write_list(root, f.list, &o)
                })?;
            }
            Ok(format!("Removed '{}' from {}.", f.item["title"].as_str().unwrap_or(item), plan::list_title(f.list)))
        }
        other => bail!("action: {other} is not propose, accept, decline, add, move, tick, undo, remove or show"),
    }
}

/// What a list holds, for `plan_edit` show: Goals as the shadow reads them (goals, their tasks, the asks), the other
/// lists one item a line.
fn list_text(list: &str, o: &Map<String, Value>) -> String {
    let props = super::proposals::text(o, now_s() as i64);
    let props = if props.is_empty() { String::new() } else { format!("\nProposals (waiting for User's yes):\n{}", props.trim_end()) };
    if list == "goals" { return format!("Goals (on screen):\n{}{props}", crate::goals::ops::compact(o, true).trim_end()) }
    let lines: Vec<String> = plan::items(o).iter().map(|i| {
        let mut l = format!("- {} · {}", i.get("id").and_then(Value::as_str).unwrap_or(""), i.get("title").and_then(Value::as_str).unwrap_or(""));
        if i.get("done").and_then(Value::as_bool) == Some(true) { l.push_str(" · done") }
        if let Some(w) = i.get("when").and_then(Value::as_str) { l.push_str(&format!(" · {w}")) }
        if let Some(d) = i.get("due").and_then(Value::as_i64).and_then(|d| chrono::DateTime::from_timestamp(d, 0)) {
            l.push_str(&format!(" · due {}", d.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M")));
        }
        l
    }).collect();
    let mut out = format!("{} (on screen):\n{}{props}", plan::list_title(list), if lines.is_empty() { "nothing".to_owned() } else { lines.join("\n") });
    if let Some(g) = o.get("guarantee") {
        let w: Vec<&str> = g["waiting"].as_array().into_iter().flatten().filter_map(|w| w["title"].as_str()).collect();
        out.push_str(&if w.is_empty() { "\nWaiting on User: nothing".to_owned() } else { format!("\nWaiting on User: {}", w.join("; ")) });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::islands::{files::Root, Host};

    fn host() -> (tempfile::TempDir, Host) {
        let d = tempfile::tempdir().unwrap();
        let h = Host::new(Root(d.path().join("fi")), None);
        (d, h)
    }

    #[test]
    fn style_rules_match_the_cli() {
        let o = |v: Value| v.as_object().unwrap().clone();
        assert!(style_problem("x", &o(json!({})), true).unwrap().contains("needs a title"));
        assert!(style_problem("x", &o(json!({"title": "One two three four"})), true).unwrap().contains("has 4"));
        assert!(style_problem("qwen-image", &o(json!({"title": "qwen image"})), true).unwrap().contains("not an id"));
        assert!(style_problem("qwen-image", &o(json!({"title": "Qwen-Image"})), true).is_none());
        assert!(style_problem("x", &o(json!({"stage": "Downloading the weights"})), false).unwrap().contains("one or two"));
        assert!(style_problem("x", &o(json!({"summary": "a\nb"})), false).unwrap().contains("one line"));
        assert!(style_problem("x", &o(json!({"summary": "a".repeat(111)})), false).unwrap().contains("110"));
        assert!(style_problem("x", &o(json!({"summary": "a".repeat(110)})), false).is_none());
    }

    #[test]
    fn plan_edit_sends_running_work_to_a_job_island() {
        let (_d, h) = host();
        let e = plan_edit(&h, &json!({"action": "add", "item": "Voice: ack hold", "list": "todo", "group": "running"})).unwrap_err();
        assert!(e.to_string().contains("islands_put"), "{e}");
    }

    /// Bug note 2026-11-02T23-28-44 (2 Nov 23:28): the user typed "cool, lets schedule a website work tomorrow" and the relay,
    /// whose tool list predated `quote`, put his words in detail, then tried a proposal object: both were refused, and
    /// neither refusal named a way in. His words opening detail count as the quote; every refusal names both doors.
    #[test]
    fn a_scheduled_promise_owner_asked_for_goes_in_and_every_refusal_names_the_way_in() {
        let (_d, h) = host();
        h.root.ensure().unwrap();
        let due = "2099-11-03T15:00";
        // No words: refused, and the answer names the quote field and the propose action.
        let e = plan_edit(&h, &json!({"action": "add", "list": "scheduled", "item": "Arsenal: website work", "due": due})).unwrap_err().to_string();
        assert!(e.contains("quote field") && e.contains("action propose"), "{e}");
        // A proposal object on Scheduled: the same guidance, not "belong to Goals".
        let e = plan_edit(&h, &json!({"action": "add", "list": "scheduled", "item": "Arsenal: website work", "due": due,
            "proposal": {"text": "Arsenal website work, tomorrow at 15:00?"}})).unwrap_err().to_string();
        assert!(e.contains("action propose") && !e.contains("belong to Goals"), "{e}");
        // His words at the head of detail, as the relay wrote them at 23:28: in, with his yes recorded.
        let line = plan_edit(&h, &json!({"action": "add", "list": "scheduled", "item": "Arsenal: website work", "due": due,
            "detail": "\"cool, lets scheadule a website work tomorrow\" (User, typed 2 Nov; he picked Arsenal's site and 15:00 on the card)"})).unwrap();
        assert!(line.contains("Scheduled"), "{line}");
        let item = h.root.read("scheduled").unwrap()["items"].as_array().unwrap().iter()
            .find(|i| i["title"] == "Arsenal: website work").cloned().unwrap();
        assert_eq!(item["yes"]["quote"], "cool, lets scheadule a website work tomorrow");
        // The quote field itself, and curly quotes in detail, work too; detail without a leading quote does not.
        plan_edit(&h, &json!({"action": "add", "list": "scheduled", "item": "Arsenal: icon pick", "due": due, "quote": "pick the icon at three"})).unwrap();
        plan_edit(&h, &json!({"action": "add", "list": "scheduled", "item": "Arsenal: README", "due": due, "detail": "\u{201c}do the readme then\u{201d}"})).unwrap();
        assert!(plan_edit(&h, &json!({"action": "add", "list": "scheduled", "item": "Arsenal: docs", "due": due, "detail": "User wants docs"})).is_err());
        // The detail form is Scheduled's only: Settled's proof and accept take the quote field alone (fresh review, 3 Nov).
        let e = plan_edit(&h, &json!({"action": "add", "list": "settled", "item": "Release notes", "detail": "\"Release notes\" draft"})).unwrap_err().to_string();
        assert!(e.contains("Settled takes proof"), "{e}");
        assert!(plan_edit(&h, &json!({"action": "accept", "item": "Arsenal: release notes", "detail": "\"yes\""})).unwrap_err().to_string().contains("quote his words"));
        // A refused move into Scheduled names move, not add; the example is a placeholder, never words to copy.
        plan_edit(&h, &json!({"action": "add", "list": "goals", "item": "Arsenal: pick a date", "kind": "decide"})).unwrap();
        let e = plan_edit(&h, &json!({"action": "move", "list": "scheduled", "item": "Arsenal: pick a date", "due": due})).unwrap_err().to_string();
        assert!(e.contains("action move") && e.contains("<his exact words>"), "{e}");
        // And the propose door the refusal names exists for Scheduled.
        let line = plan_edit(&h, &json!({"action": "propose", "list": "scheduled", "item": "Arsenal: release notes", "due": due, "why": "talked about it"})).unwrap();
        assert!(line.starts_with("Proposed 'Arsenal: release notes' in Scheduled"), "{line}");
    }

    #[test]
    fn put_leaves_out_a_picture_that_is_not_there() {
        let (d, h) = host();
        let shot = d.path().join("shot.png");
        std::fs::write(&shot, b"\x89PNG\r\n\x1a\n0000000000").unwrap();
        put(&h, &json!({"id": "phone", "title": "iPhone", "image": shot.to_str().unwrap()})).unwrap();
        let line = put(&h, &json!({"id": "phone", "image": "/Users/x/FloatingIslands/none"})).unwrap();
        assert!(line.ends_with("Image left out: no picture at /Users/x/FloatingIslands/none."), "{line}");
        assert_eq!(h.root.read("phone").unwrap()["image"], shot.to_str().unwrap(), "the good picture stays");
        put(&h, &json!({"id": "phone", "image": ""})).unwrap();
        assert!(!h.root.read("phone").unwrap().contains_key("image"), "an empty path clears it");
    }

    #[test]
    fn put_sets_a_drawable_icon_and_refuses_others() {
        let (_d, h) = host();
        let svg = r#"<svg viewBox="0 0 24 24"><rect x="4" y="2" width="16" height="20" rx="3"/></svg>"#;
        put(&h, &json!({"id": "ipad", "title": "iPad sale", "icon": svg})).unwrap();
        assert_eq!(h.root.read("ipad").unwrap()["icon"], svg);
        assert!(put(&h, &json!({"id": "ipad", "icon": "<svg><script/></svg>"})).unwrap_err().to_string().contains("SF Symbol"));
        put(&h, &json!({"id": "ipad", "icon": "ipad"})).unwrap();
        assert_eq!(h.root.read("ipad").unwrap()["icon"], "ipad");
        put(&h, &json!({"id": "ipad", "icon": ""})).unwrap();
        assert!(!h.root.read("ipad").unwrap().contains_key("icon"));
    }

    #[test]
    fn put_creates_merges_clears_lists_and_stamps() {
        let (_d, h) = host();
        assert_eq!(put(&h, &json!({"id": "tests", "title": "Tests", "stage": "Building", "steps": [{"title": "Build", "state": "now"}]})).unwrap(), "Island 'Tests' shown.");
        let o = h.root.read("tests").unwrap();
        assert!(o.contains_key("started") && o.contains_key("updated"));
        assert_eq!(put(&h, &json!({"id": "tests", "state": "done", "steps": []})).unwrap(), "Island 'Tests' updated.");
        let o = h.root.read("tests").unwrap();
        assert!(!o.contains_key("steps") && o.contains_key("ended"));
        assert_eq!(o["stage"], "Building");
        let e = put(&h, &json!({"id": "new", "stage": "Downloading"})).unwrap_err().to_string();
        assert!(e.contains("needs a title") && e.contains("island-style.md"), "{e}");
        assert!(put(&h, &json!({"id": "todo", "title": "To do"})).unwrap_err().to_string().contains("plan_edit"));
    }

    #[test]
    fn put_with_a_thread_binds_once_and_drops_out_of_the_notch() {
        let (_d, h) = host();
        let line = put(&h, &json!({"id": "voices", "title": "Voices", "thread": "th-1"})).unwrap();
        assert_eq!(line, "Island 'Voices' shown, following T3 thread th-1.");
        let o = h.root.read("voices").unwrap();
        assert_eq!(o["thread"], json!({"id": "th-1", "title": "Voices"}));
        assert_eq!(o["origin"], json!({"from": "notch"}));
        assert_eq!(h.bindings.get("voices").unwrap().thread, "th-1");
        // The app drops `origin` after the flight; a later put on the same thread does not bring it back.
        h.root.patch("voices", |o| { o.remove("origin"); }).unwrap();
        put(&h, &json!({"id": "voices", "thread": "th-1", "summary": "Two voices left"})).unwrap();
        assert!(!h.root.read("voices").unwrap().contains_key("origin"));
    }

    #[test]
    fn view_and_remove() {
        let (d, h) = host();
        let f = d.path().join("notes.md");
        std::fs::write(&f, "# Hi").unwrap();
        let line = view(&h, &json!({"path": f.to_str().unwrap(), "actions": [{"label": "Do all of it", "action": "do-all"}]})).unwrap();
        assert_eq!(line, "Showing 'notes' in a viewer.");
        let o = h.root.read("view-notes.md").unwrap();
        assert_eq!((o["preset"].as_str(), o["expanded"].as_bool()), (Some("viewer"), Some(true)));
        assert_eq!(o["actions"][0]["action"], "do-all");
        assert!(view(&h, &json!({"path": "relative.md"})).unwrap_err().to_string().contains("absolute"));
        assert!(view(&h, &json!({"path": "/nonexistent/x.md"})).unwrap_err().to_string().contains("no file"));
        put(&h, &json!({"id": "a", "title": "Alpha", "thread": "t"})).unwrap();
        assert_eq!(remove(&h, &json!({"ids": ["a", "zzz"]})).unwrap(), "Removed 'Alpha'. No island zzz.");
        assert!(h.bindings.get("a").is_none());
        assert_eq!(remove(&h, &json!({"ids": ["view-notes.md", "zzz"]})).unwrap(), "Removed 'notes'. No island zzz.");
    }

    #[test]
    fn viewing_the_same_file_again_stamps_it_so_the_helper_reads_it_anew() {
        let (d, h) = host();
        let f = d.path().join("notes.md");
        std::fs::write(&f, "# One").unwrap();
        view(&h, &json!({"path": f.to_str().unwrap()})).unwrap();
        let first = h.root.read("view-notes.md").unwrap()["updated"].as_f64().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(120));
        std::fs::write(&f, "# Two").unwrap();
        view(&h, &json!({"path": f.to_str().unwrap()})).unwrap();
        assert!(h.root.read("view-notes.md").unwrap()["updated"].as_f64().unwrap() > first, "a second view rewrites the file");
    }

    #[test]
    fn plan_edit_schedules_only_with_a_time_and_owners_words() {
        let (_d, h) = host();
        let later = chrono::Local::now() + chrono::Duration::days(2);
        let text = later.format("%Y-%m-%dT%H:%M").to_string();
        // No yes, no promise: his words asking for it, or a proposal.
        assert!(plan_edit(&h, &json!({"action": "add", "item": "Heimdall: menu revamp", "list": "scheduled", "due": text})).unwrap_err().to_string().contains("yes"));
        assert!(plan_edit(&h, &json!({"action": "add", "item": "Arsenal: tap", "list": "scheduled", "when": "Fri", "quote": "do the tap friday"})).unwrap_err().to_string().contains("time"));
        assert_eq!(plan_edit(&h, &json!({"action": "add", "item": "Heimdall: menu revamp", "list": "scheduled", "due": text, "quote": "menu revamp on Saturday"})).unwrap(), "Added 'Heimdall: menu revamp' to Scheduled.");
        let it = &h.root.read("scheduled").unwrap()["items"][0];
        assert_eq!((it["due"].as_i64().unwrap(), it["yes"]["by"].as_str()), (plan::due_seconds(&text).unwrap(), Some("voice")));
        assert!(plan_edit(&h, &json!({"action": "add", "item": "Old", "list": "scheduled", "due": "2020-01-01T10:00", "quote": "x y z"})).unwrap_err().to_string().contains("past"));
        assert!(plan_edit(&h, &json!({"action": "add", "item": "X", "list": "todo", "due": text})).unwrap_err().to_string().contains("Scheduled"));
        // A promise moves to another time; a word without a time is refused (no time, no promise).
        let sooner = (later - chrono::Duration::days(1)).format("%Y-%m-%dT%H:%M").to_string();
        plan_edit(&h, &json!({"action": "move", "item": "menu revamp", "due": sooner})).unwrap();
        assert_eq!(h.root.read("scheduled").unwrap()["items"][0]["due"].as_i64(), Some(plan::due_seconds(&sooner).unwrap()));
        assert!(plan_edit(&h, &json!({"action": "move", "item": "menu revamp", "when": "Sat"})).is_err());
    }

    #[test]
    fn proposals_wait_for_his_yes_and_settled_takes_only_proof() {
        let (_d, h) = host();
        let later = (chrono::Local::now() + chrono::Duration::days(1)).format("%Y-%m-%dT%H:%M").to_string();
        let line = plan_edit(&h, &json!({"action": "propose", "item": "Release: check model licences", "due": later, "why": "Licences before release"})).unwrap();
        assert!(line.starts_with("Proposed 'Release: check model licences' in Scheduled") && line.contains("on screen"), "{line}");
        assert!(plan::items(&h.root.read("scheduled").unwrap()).is_empty(), "a proposal is not a promise");
        let shown = plan_edit(&h, &json!({"action": "show", "list": "scheduled"})).unwrap();
        assert!(shown.contains("proposal s-") && shown.contains("on screen"), "{shown}");
        assert!(plan_edit(&h, &json!({"action": "accept", "item": "licences"})).is_err(), "a yes quotes him");
        assert_eq!(plan_edit(&h, &json!({"action": "accept", "item": "licences", "quote": "yes"})).unwrap(), "'Release: check model licences' is in Scheduled on User's yes.");
        assert_eq!(plan::items(&h.root.read("scheduled").unwrap()).len(), 1);
        // A goal, then a step under it.
        plan_edit(&h, &json!({"action": "propose", "item": "Get a job", "list": "goals", "icon": "briefcase"})).unwrap();
        plan_edit(&h, &json!({"action": "accept", "item": "Get a job", "quote": "yes"})).unwrap();
        let gid = h.root.read("goals").unwrap()["goals"][0]["id"].as_str().unwrap().to_owned();
        plan_edit(&h, &json!({"action": "propose", "item": "Prepare CV drafts", "list": "goals", "goal": gid})).unwrap();
        assert_eq!(plan_edit(&h, &json!({"action": "decline", "item": "CV drafts"})).unwrap(), "Declined 'Prepare CV drafts': it left Goals and is not proposed again.");
        assert!(plan_edit(&h, &json!({"action": "propose", "item": "Prepare CV drafts", "list": "goals", "goal": gid})).is_err());
        // Settled: proof or nothing.
        assert!(plan_edit(&h, &json!({"action": "add", "item": "Menu motion", "list": "settled"})).unwrap_err().to_string().contains("proof"));
        assert!(plan_edit(&h, &json!({"action": "add", "item": "Menu motion", "list": "settled", "proof": "merged as 5eadbeef1"})).is_err(), "a commit no repo has");
        assert!(plan_edit(&h, &json!({"action": "add", "item": "Menu motion", "list": "settled", "proof": "all 40 tests passed"})).is_err(), "the agent's word");
        let f = _d.path().join("menu-report.html");
        std::fs::write(&f, "x").unwrap();
        assert_eq!(plan_edit(&h, &json!({"action": "add", "item": "Menu motion", "list": "settled", "proof": format!("report at {}", f.display())})).unwrap(), "Added 'Menu motion' to Settled.");
        assert_eq!(h.root.read("settled").unwrap()["items"][0]["proof"]["kind"], "file");
        // Moving into Settled is refused; into Scheduled it needs his words.
        plan_edit(&h, &json!({"action": "add", "item": "Arsenal: pick the icon", "kind": "pick", "goal": gid})).unwrap();
        assert!(plan_edit(&h, &json!({"action": "move", "item": "pick the icon", "list": "settled"})).is_err());
        assert!(plan_edit(&h, &json!({"action": "move", "item": "pick the icon", "list": "scheduled", "due": later})).unwrap_err().to_string().contains("yes"));
    }

    #[test]
    fn plan_edit_adds_asks_to_goals_moves_ticks_and_removes() {
        let (_d, h) = host();
        h.root.put("goals", json!({"preset": "todo", "list": "goals", "title": "Goals", "items": [],
            "goals": [{"id": "g-arsenal", "title": "Ship Arsenal", "tasks": [{"id": "t-1", "title": "Homebrew tap", "state": "now"}]}]}).as_object().unwrap()).unwrap();
        assert_eq!(plan_edit(&h, &json!({"action": "add", "item": "Arsenal: pick the icon", "kind": "pick", "goal": "g-arsenal"})).unwrap(), "Added 'Arsenal: pick the icon' to Goals (an ask).");
        // Old callers: list todo, a waiting group and a proposal are an ask in Goals.
        assert_eq!(plan_edit(&h, &json!({"action": "add", "item": "Delete caches", "list": "todo", "group": "waiting", "proposal": {"text": "rm -rf ~/x"}})).unwrap(), "Added 'Delete caches' to Goals (an ask).");
        assert!(plan_edit(&h, &json!({"action": "add", "item": "Delete caches"})).unwrap_err().to_string().contains("already"));
        assert!(plan_edit(&h, &json!({"action": "add", "item": "X", "goal": "g-nope"})).unwrap_err().to_string().contains("no goal g-nope"));
        let asks = plan::items(&h.root.read("goals").unwrap());
        assert_eq!((asks[0]["kind"].as_str(), asks[0]["goal"].as_str(), asks[0]["asked"].is_i64()), (Some("pick"), Some("g-arsenal"), true));
        assert_eq!((asks[1].get("group"), asks[1]["goal"].as_str()), (None, Some("")), "asks have no group");
        let later = (chrono::Local::now() + chrono::Duration::days(1)).format("%Y-%m-%dT%H:%M").to_string();
        assert!(plan_edit(&h, &json!({"action": "move", "item": "pick the icon", "list": "scheduled", "when": "Tonight"})).is_err(), "no time, no promise");
        assert_eq!(plan_edit(&h, &json!({"action": "move", "item": "pick the icon", "list": "scheduled", "due": later, "quote": "do the icon tomorrow"})).unwrap(), "Moved 'Arsenal: pick the icon' to Scheduled.");
        assert_eq!(plan_edit(&h, &json!({"action": "tick", "item": "Arsenal: pick the icon"})).unwrap(), "Ticked 'Arsenal: pick the icon'.");
        assert_eq!(plan_edit(&h, &json!({"action": "tick", "item": "delete-caches"})).unwrap(), "Answered 'Delete caches'; it left Goals.");
        assert!(plan::items(&h.root.read("goals").unwrap()).is_empty());
        assert_eq!(plan_edit(&h, &json!({"action": "remove", "item": "Arsenal: pick the icon"})).unwrap(), "Removed 'Arsenal: pick the icon' from Scheduled.");
        assert!(plan_edit(&h, &json!({"action": "tick", "item": "nothing"})).unwrap_err().to_string().contains("no item"));
        assert!(plan_edit(&h, &json!({"action": "add", "item": "X", "list": "settled", "kind": "pick"})).unwrap_err().to_string().contains("Goals"));
        // show: the agent reads Goals (ids included), and a list the user closed comes back on screen.
        h.root.patch("goals", |o| { o.insert("hidden".into(), json!(true)); }).unwrap();
        let shown = plan_edit(&h, &json!({"action": "show", "list": "goals"})).unwrap();
        assert!(shown.starts_with("Goals (on screen):") && shown.contains("g-arsenal · Ship Arsenal") && shown.contains("now t-1 Homebrew tap"), "{shown}");
        assert!(!h.root.read("goals").unwrap().contains_key("hidden"));
        assert_eq!(plan_edit(&h, &json!({"action": "show", "list": "settled"})).unwrap(), "Settled (on screen):\nnothing");
    }

    #[test]
    fn a_job_island_carries_what_it_was_sent_to_do_once_and_the_goal_it_serves() {
        let (_d, h) = host();
        h.root.put("goals", json!({"preset": "todo", "list": "goals", "items": [], "goals": [
            {"id": "g-arsenal", "title": "Ship Arsenal", "state": "active", "tasks": [{"id": "t-1", "title": "Promote the allowed models into 1.0", "state": "next"}]},
            {"id": "g-pub", "title": "Ship Heimdall publicly", "state": "active", "tasks": []}]}).as_object().unwrap()).unwrap();
        crate::islands::binding::task_island(&h, "th-1", "Arsenal 1.0: Promote the allowed models into the 1.0 release [heimdall:abc]", "").unwrap();
        let o = h.root.read("task-arsenal-1-0").or_else(|| h.root.all().into_iter().find(|(i, _)| i.starts_with("task-arsenal")).map(|x| x.1)).unwrap();
        assert_eq!((o["sent"].as_str(), o["goal"].as_str()), (Some("Promote the allowed models into the 1.0 release"), Some("g-arsenal")));
        let id = o["id"].as_str().unwrap().to_owned();
        put(&h, &json!({"id": id, "sent": "Something else"})).unwrap();
        assert_eq!(h.root.read(&id).unwrap()["sent"], "Promote the allowed models into the 1.0 release", "written once");
        // A project word alone is no fact: no guess.
        assert_eq!(goal_for(&h.root, "Heimdall: fix the menu"), None);
        assert!(put(&h, &json!({"id": id, "goal": "g-nope"})).is_err());
        put(&h, &json!({"id": id, "goal": "g-pub"})).unwrap();
        assert_eq!(h.root.read(&id).unwrap()["goal"], "g-pub");
    }

    /// Review 31 Oct: "todo" names no list; removing it must not hide Goals.
    #[test]
    fn removing_todo_is_refused_and_goals_stay_shown() {
        let (_d, h) = host();
        crate::goals::ops::ensure(&h.root, 1, true).unwrap();
        let line = remove(&h, &json!({"ids": ["todo"]})).unwrap();
        assert!(line.contains("no such list"), "{line}");
        assert!(!h.root.read("goals").unwrap().contains_key("hidden"));
    }
}
