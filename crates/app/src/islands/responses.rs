//! The user's clicks on islands, after the watcher consumed them from `responses/` (night 1022): each one is acted on
//! (approvals and ticks rewrite the plan, a `t3q:` button answers its thread, a close unbinds) and becomes one plain
//! line for the agent. Approvals, buttons and controls wake the agent like an `ask` click; ticks, opens, closes and
//! thread answers arrive as context, and so does a Scheduled item falling due (it stays in Scheduled). A plan list the user closes is hidden and kept, never lost (bug note 2026-10-31T15-57-16).

use super::{plan, Class, Host};
use serde_json::{Map, Value};

fn s<'a>(r: &'a Map<String, Value>, k: &str) -> &'a str { r.get(k).and_then(Value::as_str).unwrap_or("") }

/// One line of at most `n` characters.
fn one_line(t: &str, n: usize) -> String {
    let t = t.split_whitespace().collect::<Vec<_>>().join(" ");
    if t.chars().count() <= n { t } else { t.chars().take(n - 1).collect::<String>() + "\u{2026}" }
}

fn shown(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    }
}

/// The item still offers `action`: its proposal's own buttons, or Yes / No / Later when it names none.
fn offered(item: Option<&Map<String, Value>>, action: &str) -> bool {
    let Some(p) = item.and_then(|i| i.get("proposal")) else { return false };
    match p.get("actions").and_then(Value::as_array) {
        // A button without an action id answers with its label in lower case (the app's `Proposal`).
        Some(a) => a.iter().any(|x| match x.get("action").and_then(Value::as_str) {
            Some(id) => id == action,
            None => x.get("label").and_then(Value::as_str).is_some_and(|l| l.to_lowercase() == action),
        }),
        None => matches!(action, "yes" | "no" | "later"),
    }
}

/// The label the user saw on an approval button: the proposal's own, else Yes / No / Later.
fn approval_label(item: Option<&Map<String, Value>>, action: &str) -> String {
    item.and_then(|i| i.get("proposal")).and_then(|p| p.get("actions")).and_then(Value::as_array).into_iter().flatten()
        .find(|a| a.get("action").and_then(Value::as_str) == Some(action))
        .and_then(|a| a.get("label")).and_then(Value::as_str).map(str::to_owned)
        .unwrap_or_else(|| { let mut c = action.chars(); c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default() })
}

/// A part's label by its control id (toggle, slider).
fn part_label(island: Option<&Map<String, Value>>, id: &str) -> String {
    island.and_then(|o| o.get("parts")).and_then(Value::as_array).into_iter().flatten()
        .find(|p| p.get("id").and_then(Value::as_str) == Some(id))
        .and_then(|p| p.get("label")).and_then(Value::as_str).unwrap_or(id).to_string()
}

/// A button's label by its action, from the island's proposed `actions` or its `parts`; else the action id.
fn button_label(island: Option<&Map<String, Value>>, action: &str) -> String {
    ["actions", "parts"].iter()
        .flat_map(|k| island.and_then(|o| o.get(*k)).and_then(Value::as_array).into_iter().flatten())
        .find(|b| b.get("action").and_then(Value::as_str) == Some(action))
        .and_then(|b| b.get("label")).and_then(Value::as_str).unwrap_or(action).to_string()
}

/// Act on one response; returns the line for the agent and how it arrives (none: handled elsewhere, or ignored).
pub fn handle(h: &Host, r: &Map<String, Value>) -> Option<(Class, String)> {
    let island_id = s(r, "island");
    let island = h.seen(island_id);
    let title = island.as_ref().and_then(|o| o.get("title")).and_then(Value::as_str).unwrap_or(island_id).to_string();
    let action = s(r, "action");
    let log = |e: anyhow::Error| eprintln!("islands: {} response on {island_id} not applied: {}", s(r, "kind"), super::short_error(&e));
    match s(r, "kind") {
        "approval" => {
            let item_id = shown(r.get("value"));
            let list = plan::list_of(island_id, island.as_ref()).unwrap_or("goals");
            // Yes on a proposal: his one click; it fills in where it sits (docs/one-system.md).
            if let Some((l, p)) = super::proposals::find(&h.root, Some(list), &item_id).filter(|(_, p)| p.get("id").and_then(Value::as_str) == Some(item_id.as_str())) {
                let name = p.get("title").and_then(Value::as_str).unwrap_or(&item_id).to_string();
                if action != "yes" {
                    return Some((Class::Answer, format!("User chose {action} on the proposal '{name}'; nothing was done (only Yes takes it).")));
                }
                return Some(match super::proposals::accept(&h.root, Some(l), &item_id, super::proposals::Yes::Click, None, super::tools::now_s() as i64) {
                    Ok((t, l, next)) => (Class::Answer, format!("User clicked Yes on the proposal '{t}': it is in {} now.{}", plan::list_title(l),
                        next.map(|p| format!(" Next on screen: {}", super::proposals::notice(l, &p))).unwrap_or_default())),
                    Err(e) => { log(e); (Class::Answer, format!("User clicked Yes on the proposal '{name}', but it could not be added.")) }
                });
            }
            let item = h.root.read(list).map(|o| plan::items(&o)).unwrap_or_default().into_iter()
                .find(|i| i.get("id").and_then(Value::as_str) == Some(item_id.as_str()));
            let label = r.get("label").and_then(Value::as_str).filter(|l| !l.is_empty()).map(str::to_owned)
                .unwrap_or_else(|| approval_label(item.as_ref(), action));
            let name = item.as_ref().and_then(|i| i.get("title")).and_then(Value::as_str).unwrap_or(&item_id).to_string();
            // A click on buttons that changed or went away meanwhile (the plan refresher rewrites its own answers)
            // does nothing to the item; Heimdall still hears what he chose.
            if !offered(item.as_ref(), action) {
                return Some((Class::Answer, format!("User chose {label} on '{name}', but that question had changed; nothing was done to the item.")));
            }
            if let Err(e) = plan::approve(&h.root, list, &item_id, action) { log(e) }
            Some((Class::Answer, format!("User chose {label} on the approval '{name}'.")))
        }
        "tick" => {
            let item_id = shown(r.get("value"));
            let list = plan::list_of(island_id, island.as_ref()).unwrap_or("goals");
            let done = action != "undo";
            let name = match plan::tick(&h.root, list, &item_id, done) {
                Ok(t) => t.unwrap_or(item_id),
                Err(e) => { log(e); item_id }
            };
            Some((Class::Context, if done { format!("User ticked '{name}' in {}.", plan::list_title(list)) } else { format!("User unticked '{name}' in {}.", plan::list_title(list)) }))
        }
        // The helper before night 1022 ticks in the file itself and reports the title.
        "todo" => {
            let done = r.get("value").and_then(Value::as_bool).unwrap_or(true);
            Some((Class::Context, format!("User {} '{action}' in {title}.", if done { "ticked" } else { "unticked" })))
        }
        "button" => {
            let label = shown(r.get("value"));
            // A viewer's proposed-action buttons report only their action id: the label comes from the island.
            let label = if label.is_empty() { button_label(island.as_ref(), action) } else { label };
            let how = if label == action { String::new() } else { format!(" (action {action})") };
            Some((Class::Answer, format!("User clicked '{label}' on the island '{title}'{how}.")))
        }
        // The islands app drew a proposal to the user, open and on screen: its two days start now. Nobody is told.
        "seen" => {
            let id = shown(r.get("value"));
            if let Some(list) = plan::list_of(island_id, island.as_ref()) {
                if let Err(e) = super::proposals::seen(&h.root, list, &id, super::tools::now_s() as i64) { log(e) }
            }
            None
        }
        // A choice on Scheduled's guarantee line: applied to what waits (an ask, a missed promise).
        "waiting" => Some((Class::Answer, super::guarantee::choose(&h.root, &shown(r.get("value")), action).unwrap_or_else(|e| {
            let line = format!("User chose {action} on '{}', but it could not be applied.", shown(r.get("value")));
            log(e);
            line
        }))),
        // A report page in the viewer spoke through its line back to Heimdall (`window.heimdall.send`).
        "report" => {
            let label = r.get("label").and_then(Value::as_str).filter(|l| !l.trim().is_empty()).map(str::to_owned).unwrap_or_else(|| action.to_string());
            let value = match r.get("value") {
                None | Some(Value::Null) => String::new(),
                Some(Value::String(s)) => format!(": {}", one_line(s, 300)),
                // The app writes what it did: `saved` (the path) or `refused` (review 2 Nov: a refused name read as saved).
                Some(v) if action == "save" => match (v.get("saved").and_then(Value::as_str), v.get("file").and_then(Value::as_str)) {
                    (Some(p), _) => format!(": saved {p}"),
                    (None, f) => format!(": not saved ({} is not a plain .json name beside the page, or the write failed)", f.unwrap_or("the file")),
                },
                Some(v) => format!(": {}", one_line(&v.to_string(), 300)),
            };
            let how = if label == action { String::new() } else { format!(" (action {action})") };
            Some((Class::Answer, format!("User chose '{label}' in the report '{title}'{how}{value}.")))
        }
        "list" => Some((Class::Answer, format!("User picked '{}' in the list on '{title}'.", shown(r.get("value"))))),
        "toggle" => {
            let on = r.get("value").and_then(Value::as_bool).unwrap_or(false);
            Some((Class::Answer, format!("User switched '{}' {} on '{title}'.", part_label(island.as_ref(), action), if on { "on" } else { "off" })))
        }
        "slider" => Some((Class::Answer, format!("User set '{}' to {} on '{title}'.", part_label(island.as_ref(), action), shown(r.get("value"))))),
        "closed" => {
            // A job island: its task is archived once finished, by Heimdall itself (never stopped while it works).
            let bound = h.bindings.get(island_id);
            h.bindings.unbind(island_id);
            if let Some(b) = bound { h.t3_job(super::binding::Job::Cleared(b.thread)) }
            // A plan island is hidden, and comes back on `refresh_plan`, the next pass or `plan_edit` show, as it was:
            // its copy is kept even when the watcher never saw it (the kept copy of its last write).
            if let Some(list) = plan::list_of(island_id, island.as_ref()) {
                if let Err(e) = plan::keep_closed(&h.root, list, island.as_ref()) { log(e) }
            }
            Some((Class::Context, format!("User closed the island '{title}'.")))
        }
        "open" => Some((Class::Context, format!("User opened {} from the island '{title}'.", shown(r.get("value"))))),
        // A Scheduled item fell due: it stays in Scheduled, drawn as due. An older islands app
        // moved it into a To do list: it goes back.
        "due" => {
            let item_id = shown(r.get("value"));
            // Each due time is told once: an item already told (`told` equals `due`) moves and notifies no more.
            let back = match plan::return_due(&h.root, &item_id) {
                Ok(plan::Returned::AlreadyTold) => return None,
                Ok(r) => r == plan::Returned::Back,
                Err(e) => { log(e); false }
            };
            let title = || h.root.read("scheduled").map(|o| plan::items(&o)).unwrap_or_default().into_iter()
                .find(|i| i.get("id").and_then(Value::as_str) == Some(item_id.as_str()))
                .and_then(|i| i.get("title").and_then(Value::as_str).map(str::to_owned));
            // Moved back just now (and stamped told by that move): this is its one notice.
            if back { return Some((Class::Context, format!("Scheduled item now due: '{}'.", title().unwrap_or(item_id.clone())))) }
            let name = match plan::tell_due(&h.root, &item_id) {
                Ok(Some((_, false))) => return None,
                Ok(Some((title, true))) => title,
                Ok(None) => item_id,
                Err(e) => { log(e); item_id }
            };
            Some((Class::Context, format!("Scheduled item now due: '{name}'.")))
        }
        other => {
            eprintln!("islands: unknown response kind {other:?} on {island_id}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::islands::{binding::Job, files::Root};
    use serde_json::json;

    fn host() -> (tempfile::TempDir, Host) {
        let d = tempfile::tempdir().unwrap();
        let h = Host::new(Root(d.path().to_path_buf()), None);
        (d, h)
    }
    fn r(v: Value) -> Map<String, Value> { v.as_object().unwrap().clone() }

    #[test]
    fn a_click_on_buttons_that_changed_does_nothing_and_says_so_with_the_label_he_saw() {
        let (_d, h) = host();
        h.root.put("goals", &r(json!({"preset": "todo", "list": "goals", "title": "Goals", "goals": [], "items": [
            {"id": "trash", "title": "Empty Trash"},
            {"id": "page", "title": "Check the page",
             "proposal": {"text": "", "actions": [{"label": "Looks right", "action": "done"}], "anticipated": true}}]}))).unwrap();
        // "Keep" (no) was on screen, but the buttons went away meanwhile: nothing is done to the ask.
        let (c, line) = handle(&h, &r(json!({"island": "goals", "kind": "approval", "action": "no", "value": "trash", "label": "Keep"}))).unwrap();
        assert_eq!((c, line.as_str()), (Class::Answer, "User chose Keep on 'Empty Trash', but that question had changed; nothing was done to the item."));
        assert_eq!(h.root.read("goals").unwrap()["items"].as_array().unwrap().len(), 2);
        // The label he saw wins over a rewording that landed meanwhile; the answered ask leaves.
        let (_, line) = handle(&h, &r(json!({"island": "goals", "kind": "approval", "action": "done", "value": "page", "label": "Looks good"}))).unwrap();
        assert_eq!(line, "User chose Looks good on the approval 'Check the page'.");
        assert_eq!(plan::items(&h.root.read("goals").unwrap()).iter().map(|i| i["id"].as_str().unwrap()).collect::<Vec<_>>(), ["trash"]);
    }

    #[test]
    fn approvals_wake_the_agent_and_answered_asks_leave_goals() {
        let (_d, h) = host();
        h.root.put("goals", &r(json!({"preset": "todo", "list": "goals", "title": "Goals", "goals": [], "items": [
            {"id": "caches", "title": "Delete old caches", "proposal": {"text": "rm", "actions": [{"label": "Do it", "action": "yes"}, {"label": "No", "action": "no"}]}},
            {"id": "push", "title": "Push branch", "proposal": {"text": "git push"}}]}))).unwrap();
        h.remember(&h.root.all());
        let (c, line) = handle(&h, &r(json!({"island": "goals", "kind": "approval", "action": "yes", "value": "caches"}))).unwrap();
        assert_eq!((c, line.as_str()), (Class::Answer, "User chose Do it on the approval 'Delete old caches'."));
        // An old helper still reporting on the To do island: the same list.
        let (_, line) = handle(&h, &r(json!({"island": "todo", "kind": "approval", "action": "later", "value": "push"}))).unwrap();
        assert_eq!(line, "User chose Later on the approval 'Push branch'.");
        let items = plan::items(&h.root.read("goals").unwrap());
        assert_eq!(items.len(), 1);
        assert_eq!((items[0]["id"].as_str(), items[0]["chosen"].as_str(), items[0].contains_key("proposal")), (Some("push"), Some("later"), false));
    }

    #[test]
    fn ticks_update_the_plan_and_arrive_as_context() {
        let (_d, h) = host();
        h.root.put("scheduled", &r(json!({"preset": "todo", "list": "scheduled", "title": "Scheduled", "items": [{"id": "a", "title": "Arsenal: push", "when": "Tonight"}]}))).unwrap();
        let (c, line) = handle(&h, &r(json!({"island": "scheduled", "kind": "tick", "action": "done", "value": "a"}))).unwrap();
        assert_eq!((c, line.as_str()), (Class::Context, "User ticked 'Arsenal: push' in Scheduled."));
        assert_eq!(plan::items(&h.root.read("scheduled").unwrap())[0]["done"], true);
        let (_, line) = handle(&h, &r(json!({"island": "scheduled", "kind": "tick", "action": "undo", "value": "a"}))).unwrap();
        assert_eq!(line, "User unticked 'Arsenal: push' in Scheduled.");
        let (_, line) = handle(&h, &r(json!({"island": "todo", "kind": "todo", "action": "Old item", "value": true}))).unwrap();
        assert_eq!(line, "User ticked 'Old item' in todo.");
    }

    #[test]
    fn buttons_controls_closes_and_opens_read_as_plain_lines() {
        let (_d, h) = host();
        h.root.put("notes", &r(json!({"title": "Notes", "parts": [{"type": "toggle", "id": "hq", "label": "High quality"}, {"type": "slider", "id": "spd", "label": "Speed"}]}))).unwrap();
        h.remember(&h.root.all());
        let line = |v: Value| handle(&h, &r(v)).unwrap();
        assert_eq!(line(json!({"island": "notes", "kind": "button", "action": "do-all", "value": "Do all of it"})), (Class::Answer, "User clicked 'Do all of it' on the island 'Notes' (action do-all).".into()));
        assert_eq!(line(json!({"island": "notes", "kind": "button", "action": "go"})).1, "User clicked 'go' on the island 'Notes'.");
        h.root.put("view-plan.md", &r(json!({"title": "Plan", "preset": "viewer", "actions": [{"label": "Do all of it", "action": "do-all"}]}))).unwrap();
        assert_eq!(line(json!({"island": "view-plan.md", "kind": "button", "action": "do-all"})).1, "User clicked 'Do all of it' on the island 'Plan' (action do-all).");
        assert_eq!(line(json!({"island": "notes", "kind": "list", "action": "i2", "value": "Second"})).1, "User picked 'Second' in the list on 'Notes'.");
        assert_eq!(line(json!({"island": "notes", "kind": "toggle", "action": "hq", "value": false})).1, "User switched 'High quality' off on 'Notes'.");
        assert_eq!(line(json!({"island": "notes", "kind": "slider", "action": "spd", "value": 1.5})).1, "User set 'Speed' to 1.5 on 'Notes'.");
        assert_eq!(line(json!({"island": "notes", "kind": "open", "action": "open", "value": "/tmp/a.html"})), (Class::Context, "User opened /tmp/a.html from the island 'Notes'.".into()));
        // Closed: the helper already deleted the file; the title comes from what the watcher saw last.
        h.bindings.bind("notes", "th");
        h.root.remove("notes").unwrap();
        assert_eq!(line(json!({"island": "notes", "kind": "closed", "action": "close"})).1, "User closed the island 'Notes'.");
        assert!(h.bindings.get("notes").is_none());
        assert!(handle(&h, &r(json!({"island": "x", "kind": "weird"}))).is_none());
    }

    /// closing a job island hands its thread to the binding loop, which archives the task once it
    /// is finished; an island bound to no thread hands nothing over.
    #[test]
    fn closing_a_job_island_hands_its_thread_over_for_archiving() {
        let (_d, h) = host();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        h.set_t3(Some(tx));
        h.root.put("task-disk", &r(json!({"title": "Disk cleanup", "state": "done", "thread": "mcp:d"}))).unwrap();
        h.bindings.bind("task-disk", "mcp:d");
        h.remember(&h.root.all());
        let (_, line) = handle(&h, &r(json!({"island": "task-disk", "kind": "closed", "action": "close"}))).unwrap();
        assert_eq!(line, "User closed the island 'Disk cleanup'.");
        assert!(matches!(rx.try_recv(), Ok(Job::Cleared(t)) if t == "mcp:d"));
        assert!(h.bindings.get("task-disk").is_none());
        h.root.put("notes", &r(json!({"title": "Notes"}))).unwrap();
        handle(&h, &r(json!({"island": "notes", "kind": "closed", "action": "close"}))).unwrap();
        assert!(rx.try_recv().is_err());
    }

    /// Bug note 2026-10-31T15-57-16: closing To do deleted the whole list.
    #[test]
    fn a_closed_plan_island_is_hidden_never_lost_and_comes_back_as_it_was() {
        let (_d, h) = host();
        plan::edit_list(&h.root, "goals", |its| { its.push(r(json!({"id": "a", "title": "Call the bank"}))); Ok(()) }).unwrap();
        h.root.put("notes", &r(json!({"title": "Notes"}))).unwrap();
        // The app hides a plan island in place now: the list stays, hidden, and comes back.
        h.root.patch("goals", |o| { o.insert("hidden".into(), json!(true)); }).unwrap();
        let (c, line) = handle(&h, &r(json!({"island": "goals", "kind": "closed", "action": "close"}))).unwrap();
        assert_eq!((c, line.as_str()), (Class::Context, "User closed the island 'Goals'."));
        assert_eq!(h.root.read("goals").unwrap()["items"][0]["title"], "Call the bank");
        assert_eq!(plan::reopen_closed(&h.root).unwrap(), ["goals"]);
        assert!(!h.root.read("goals").unwrap().contains_key("hidden"));
        // An app that still deletes the file, and a watcher that never saw the list: its title and content survive.
        h.root.remove("goals").unwrap();
        h.root.remove("notes").unwrap();
        let (_, line) = handle(&h, &r(json!({"island": "goals", "kind": "closed", "action": "close"}))).unwrap();
        assert_eq!(line, "User closed the island 'Goals'.");
        handle(&h, &r(json!({"island": "notes", "kind": "closed", "action": "close"}))).unwrap();
        assert_eq!(plan::reopen_closed(&h.root).unwrap(), ["goals"]);
        assert_eq!(h.root.read("goals").unwrap()["items"][0]["title"], "Call the bank");
        assert!(h.root.read("notes").is_none(), "only plan islands come back");
    }

    #[test]
    fn a_scheduled_item_that_fell_due_stays_in_scheduled_and_reaches_the_agent_by_its_title() {
        let (_d, h) = host();
        h.root.put("scheduled", &r(json!({"preset": "todo", "list": "scheduled", "items": [{"id": "menu", "title": "Heimdall: menu revamp", "due": 1}]}))).unwrap();
        let (c, line) = handle(&h, &r(json!({"island": "scheduled", "kind": "due", "action": "moved", "value": "menu"}))).unwrap();
        assert_eq!((c, line.as_str()), (Class::Context, "Scheduled item now due: 'Heimdall: menu revamp'."));
        assert_eq!(plan::items(&h.root.read("scheduled").unwrap()).len(), 1, "it stays");
        assert!(h.root.read("goals").is_none(), "never moved into Goals");
        // Gone again before Heimdall read it: the id stands in.
        let (_, line) = handle(&h, &r(json!({"island": "scheduled", "kind": "due", "action": "moved", "value": "gone"}))).unwrap();
        assert_eq!(line, "Scheduled item now due: 'gone'.");
        // An older islands app still moves it into a To do list: it goes back into Scheduled, due, and the To do goes.
        h.root.put("todo", &r(json!({"preset": "todo", "list": "todo", "items": [{"id": "tap", "title": "Arsenal: tap", "group": "waiting", "due": 2}]}))).unwrap();
        let (_, line) = handle(&h, &r(json!({"island": "scheduled", "kind": "due", "action": "moved", "value": "tap"}))).unwrap();
        assert_eq!(line, "Scheduled item now due: 'Arsenal: tap'.");
        assert!(h.root.read("todo").is_none());
        let it = plan::items(&h.root.read("scheduled").unwrap()).into_iter().find(|i| i["id"] == "tap").unwrap();
        assert!(it.get("group").is_none() && it["due"] == 2);
    }

    /// Review 31 Oct: the Swift rollback helper moves a past-due item out of Scheduled at every change. Each due time is
    /// told once and moved back once; then nothing moves or notifies again.
    #[test]
    fn a_due_item_moves_back_and_is_told_once_however_often_a_helper_moves_it() {
        let (_d, h) = host();
        let item = json!({"id": "tap", "title": "Arsenal: tap", "due": 5});
        // Round 1: the helper moved it into To do and reports it.
        h.root.put("scheduled", &r(json!({"preset": "todo", "list": "scheduled", "items": []}))).unwrap();
        h.root.put("todo", &r(json!({"items": [item.clone()]}))).unwrap();
        let due = json!({"island": "scheduled", "kind": "due", "action": "moved", "value": "tap"});
        assert_eq!(handle(&h, &r(due.clone())).unwrap().1, "Scheduled item now due: 'Arsenal: tap'.");
        let back = plan::items(&h.root.read("scheduled").unwrap());
        assert_eq!((back[0]["due"].as_i64(), back[0]["told"].as_i64()), (Some(5), Some(5)));
        // Round 2: the helper moves it again (with its stamp): no notice, and it is not moved back again.
        let mut again = back[0].clone();
        again.insert("group".into(), json!("waiting"));
        h.root.put("scheduled", &r(json!({"items": []}))).unwrap();
        h.root.put("todo", &r(json!({"items": [again]}))).unwrap();
        assert!(handle(&h, &r(due.clone())).is_none());
        assert!(plan::items(&h.root.read("scheduled").unwrap()).is_empty() && h.root.read("todo").is_some(), "no loop");
        // The new app keeps it in Scheduled and reports it: told once for that due time, again for a new one.
        h.root.put("scheduled", &r(json!({"items": [{"id": "menu", "title": "Menu", "due": 7}]}))).unwrap();
        let menu = json!({"island": "scheduled", "kind": "due", "action": "moved", "value": "menu"});
        assert!(handle(&h, &r(menu.clone())).is_some());
        assert!(handle(&h, &r(menu.clone())).is_none(), "told once");
        h.root.put("scheduled", &r(json!({"items": [{"id": "menu", "title": "Menu", "due": 9, "told": 7}]}))).unwrap();
        assert!(handle(&h, &r(menu)).is_some(), "rescheduled: told again");
    }

    #[test]
    fn a_click_on_yes_fills_a_proposal_in_and_report_buttons_reach_heimdall() {
        let (_d, h) = host();
        let (id, _) = crate::islands::proposals::propose(&h.root, &crate::islands::proposals::Draft { list: "goals".into(), kind: "goal".into(),
            title: "Get a job".into(), ..Default::default() }, 1).unwrap();
        let (_, line) = handle(&h, &r(json!({"island": "goals", "kind": "approval", "action": "yes", "value": id}))).unwrap();
        assert_eq!(line, "User clicked Yes on the proposal 'Get a job': it is in Goals now.");
        assert_eq!(h.root.read("goals").unwrap()["goals"][0]["yes"]["by"], "click");
        h.root.put("view-page", json!({"preset": "viewer", "title": "Latency", "path": "/tmp/x.html"}).as_object().unwrap()).unwrap();
        let (c, line) = handle(&h, &r(json!({"island": "view-page", "kind": "report", "action": "keep", "label": "Keep in record"}))).unwrap();
        assert_eq!((c, line.as_str()), (Class::Answer, "User chose 'Keep in record' in the report 'Latency' (action keep)."));
        let (_, line) = handle(&h, &r(json!({"island": "view-page", "kind": "report", "action": "save", "value": {"file": "picks.json", "saved": "/tmp/picks.json"}}))).unwrap();
        assert!(line.ends_with(": saved /tmp/picks.json."), "{line}");
        let (_, line) = handle(&h, &r(json!({"island": "view-page", "kind": "report", "action": "save", "value": {"file": "../x.json", "refused": true}}))).unwrap();
        assert!(line.contains("not saved (../x.json"), "{line}");
        // Seen: the two days start, silently, once.
        let (pid, _) = crate::islands::proposals::propose(&h.root, &crate::islands::proposals::Draft { list: "goals".into(), kind: "goal".into(),
            title: "Ship Arsenal".into(), ..Default::default() }, 1).unwrap();
        assert!(handle(&h, &r(json!({"island": "goals", "kind": "seen", "value": pid}))).is_none());
        let q = crate::islands::proposals::queue(&h.root.read("goals").unwrap());
        assert!(q.iter().any(|p| p["id"] == pid.as_str() && p["shown"].is_i64()));
    }
}
