//! "Nothing ignored": a guarantee, not a list (`docs/one-system.md`). One line on top of Scheduled,
//! written by Heimdall's sweep into `scheduled.json` as `guarantee: {checked, waiting, unsure}` and drawn by the
//! islands app, which says "Not sure" by itself when `checked` is older than 30 min (Heimdall stopped sweeping).
//! Only real blocks wait on the user, each read from a fact on disk: an open ask in Goals, a job island waiting on him or
//! failed, a promise in Scheduled whose time passed 30 min ago unticked. Proposals never wait. Unsure: a running job
//! island not heard from for 15 min. The sweep runs every minute (`run`) with the two-day archive of proposals.

use super::files::Root;
use super::plan;
use anyhow::Result;
use serde_json::{json, Map, Value};

/// A running job island silent this long is not sure.
pub const SILENT_S: i64 = 15 * 60;
/// A promise this long past its time, unticked, is missed.
pub const MISSED_S: i64 = 30 * 60;
/// The guarantee is rewritten at least this often while it is unchanged (so "checked N min ago" stays near).
pub const REFRESH_S: i64 = 5 * 60;
/// The sweep's period.
pub const PERIOD: std::time::Duration = std::time::Duration::from_secs(60);

fn s<'a>(o: &'a Map<String, Value>, k: &str) -> &'a str { o.get(k).and_then(Value::as_str).unwrap_or("") }

/// A plan list (by id or `list`), never a job.
fn is_plan(id: &str, o: &Map<String, Value>) -> bool { plan::list_of(id, Some(o)).is_some() || s(o, "list") == "todo" || id == "todo" }

/// A job island: on the right, a state, not a plan list, viewer or note.
fn is_job(id: &str, o: &Map<String, Value>) -> bool {
    !is_plan(id, o) && !matches!(s(o, "preset"), "viewer" | "note" | "todo" | "list") && !s(o, "state").is_empty()
}

fn epoch(o: &Map<String, Value>, k: &str) -> Option<i64> { o.get(k).and_then(Value::as_f64).map(|f| f as i64) }

/// What waits on the user and what is unsure, from the islands on disk at `now`.
pub fn compute(all: &[(String, Map<String, Value>)], now: i64) -> (Vec<Value>, Vec<Value>) {
    let mut waiting = Vec::new();
    let mut unsure = Vec::new();
    let get = |id: &str| all.iter().find(|(i, _)| i == id).map(|(_, o)| o);
    // Goals' open asks: something only he can do now.
    if let Some(g) = get("goals") {
        for a in plan::items(g) {
            let open = a.get("done").and_then(Value::as_bool) != Some(true) && s(&a, "chosen").is_empty() && matches!(s(&a, "next"), "" | "owner");
            if !open { continue }
            let choices: Vec<Value> = a.get("proposal").and_then(|p| p.get("actions")).and_then(Value::as_array).cloned()
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| if a.get("proposal").is_some() { vec![json!({"label": "Yes", "action": "yes"}), json!({"label": "No", "action": "no"})] } else { vec![] });
            let line = s(&a, "detail").lines().next().unwrap_or("").trim().to_owned();
            waiting.push(json!({"id": format!("ask:{}", s(&a, "id")), "title": s(&a, "title"), "line": line, "goal": s(&a, "goal"),
                                "choices": choices, "source": {"list": "goals", "id": s(&a, "id")}}));
        }
    }
    // Job islands blocked on him or failed; running ones gone silent.
    for (id, o) in all.iter().filter(|(id, o)| is_job(id, o)) {
        let title = s(o, "title");
        match s(o, "state") {
            "waiting" => waiting.push(json!({"id": format!("job:{id}"), "title": format!("{title} waits on you"),
                "line": s(o, "stage"), "goal": s(o, "goal"), "choices": [], "source": {"island": id}})),
            "failed" => waiting.push(json!({"id": format!("job:{id}"), "title": format!("{title} failed"),
                "line": s(o, "stage"), "goal": s(o, "goal"), "choices": [], "source": {"island": id}})),
            "running" => {
                let heard = epoch(o, "updated").or_else(|| epoch(o, "started"));
                if let Some(h) = heard.filter(|h| now - h >= SILENT_S) {
                    unsure.push(json!({"id": format!("silent:{id}"), "title": title, "since": h, "source": {"island": id}}));
                }
            }
            _ => {}
        }
    }
    // Promises whose time passed unticked.
    if let Some(sc) = get("scheduled") {
        for i in plan::items(sc) {
            let Some(due) = i.get("due").and_then(Value::as_f64).map(|d| d as i64) else { continue };
            if i.get("done").and_then(Value::as_bool) == Some(true) || now - due < MISSED_S { continue }
            let at = chrono::DateTime::from_timestamp(due, 0).map(|d| d.with_timezone(&chrono::Local).format("%a %H:%M").to_string()).unwrap_or_default();
            waiting.push(json!({"id": format!("missed:{}", s(&i, "id")), "title": format!("Missed: {}", s(&i, "title")), "line": format!("was due {at}"),
                "goal": s(&i, "goal"), "choices": [{"label": "Done", "action": "done"}, {"label": "Drop", "action": "drop"}],
                "source": {"list": "scheduled", "id": s(&i, "id")}}));
        }
    }
    (waiting, unsure)
}

/// One sweep's write: the guarantee into Scheduled when what it says changed, or every `REFRESH_S` (under the lock).
/// Returns true when it wrote.
pub fn write(root: &Root, now: i64) -> Result<bool> {
    root.locked(|| {
        let (waiting, unsure) = compute(&root.all(), now);
        let mut o = crate::goals::ops::current(root, "scheduled").unwrap_or_else(|| plan::fresh("scheduled"));
        let old = o.get("guarantee").cloned().unwrap_or(Value::Null);
        let same = old["waiting"] == json!(waiting) && old["unsure"] == json!(unsure);
        let fresh = old["checked"].as_i64().is_some_and(|c| now - c < REFRESH_S && now >= c);
        if same && fresh { return Ok(false) }
        o.insert("guarantee".into(), json!({"checked": now, "waiting": waiting, "unsure": unsure}));
        crate::goals::ops::write_list(root, "scheduled", &o)?;
        Ok(true)
    })
}

/// The user's click on a choice of the guarantee line (`kind: "waiting"`): applied to its source. Returns the line the
/// agent hears.
pub fn choose(root: &Root, waiting_id: &str, action: &str) -> Result<String> {
    let sched = root.read("scheduled").unwrap_or_default();
    let entry = sched.get("guarantee").and_then(|g| g.get("waiting")).and_then(Value::as_array).into_iter().flatten()
        .find(|w| w["id"] == waiting_id).cloned();
    let Some(w) = entry else { return Ok(format!("User chose {action} on '{waiting_id}', which no longer waits; nothing was done.")) };
    let title = w["title"].as_str().unwrap_or(waiting_id).to_owned();
    let label = w["choices"].as_array().into_iter().flatten().find(|c| c["action"] == action)
        .and_then(|c| c["label"].as_str()).unwrap_or(action).to_owned();
    let src_id = w["source"]["id"].as_str().unwrap_or("").to_owned();
    let line = match w["source"]["list"].as_str() {
        Some("goals") => {
            plan::approve(root, "goals", &src_id, action)?;
            format!("User chose {label} on '{title}' (it waited on him).")
        }
        Some("scheduled") => {
            let item = plan::items(&sched).into_iter().find(|i| i.get("id").and_then(Value::as_str) == Some(src_id.as_str()));
            plan::edit_list(root, "scheduled", |its| { its.retain(|i| i.get("id").and_then(Value::as_str) != Some(src_id.as_str())); Ok(()) })?;
            // His click is his word: a kept promise that serves a goal settles with it as proof.
            if let (true, Some(i)) = (action == "done", item) {
                let goal = i.get("goal").and_then(Value::as_str).unwrap_or("").to_owned();
                let now = crate::islands::tools::now_s() as i64;
                let e = crate::goals::settled::Entry { title: i.get("title").and_then(Value::as_str).unwrap_or("").into(), kind: "shipped", goal, task: String::new(),
                    at: now, quote: "Done, clicked by User".into(), proof: Some(crate::goals::proof::quote("Done, clicked by User", now)) };
                root.locked(|| crate::goals::settled::record_locked(root, &[e], now))?;
            }
            if action == "done" { format!("User says the missed promise '{title}' is done; it left Scheduled.") }
            else { format!("User dropped the missed promise '{title}'; it left Scheduled.") }
        }
        _ => format!("User chose {label} on '{title}'."),
    };
    write(root, crate::islands::tools::now_s() as i64)?;
    Ok(line)
}

/// The sweep, every `PERIOD` until `stop`: proposals past two days go to the record (the next one shows, and
/// Heimdall hears of it), then the guarantee. Runs on its own thread in the live app (`islands::live::start`).
pub fn run(host: std::sync::Arc<super::Host>, stop: std::sync::Arc<std::sync::atomic::AtomicBool>) {
    use std::sync::atomic::Ordering;
    while !stop.load(Ordering::Acquire) {
        let now = crate::islands::tools::now_s() as i64;
        match super::proposals::sweep(&host.root, now) {
            Ok(moved) => for (list, w) in moved {
                for t in &w.archived { eprintln!("islands: proposal '{t}' in {list} archived after two days unanswered") }
                if let Some(p) = &w.shown { host.deliver(super::Class::Context, &super::proposals::notice(list, p)) }
            },
            Err(e) => eprintln!("islands: proposal sweep: {}", super::short_error(&e)),
        }
        if let Err(e) = write(&host.root, now) { eprintln!("islands: guarantee not written: {}", super::short_error(&e)) }
        // Sleep in short steps so a quit is prompt.
        for _ in 0..PERIOD.as_secs() { if stop.load(Ordering::Acquire) { return } std::thread::sleep(std::time::Duration::from_secs(1)) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn o(v: Value) -> Map<String, Value> { v.as_object().unwrap().clone() }

    #[test]
    fn quiet_when_nothing_waits_and_proposals_never_wait() {
        let all = vec![
            ("goals".to_owned(), o(json!({"list": "goals", "items": [], "goals": [], "proposals": [{"id": "g-job", "title": "Get a job", "kind": "goal", "shown": 0}]}))),
            ("scheduled".to_owned(), o(json!({"list": "scheduled", "items": [{"id": "s-1", "title": "Later", "due": 10_000}], "proposals": [{"id": "s-2", "title": "P", "due": 5}]}))),
            ("task-done".to_owned(), o(json!({"title": "Done job", "state": "done"}))),
        ];
        assert_eq!(compute(&all, 1000), (vec![], vec![]));
    }

    #[test]
    fn real_blocks_wait_an_answered_ask_does_not_and_silence_is_unsure() {
        let all = vec![
            ("goals".to_owned(), o(json!({"list": "goals", "goals": [], "items": [
                {"id": "a-1", "title": "Approve the Metal toolchain download", "detail": "blocks Arsenal 1.0", "goal": "g-arsenal",
                 "proposal": {"text": "", "actions": [{"label": "Allow", "action": "yes"}, {"label": "Not now", "action": "later"}]}},
                {"id": "a-2", "title": "Answered", "chosen": "yes"},
                {"id": "a-3", "title": "Ania's", "next": "ania"}]}))),
            ("scheduled".to_owned(), o(json!({"list": "scheduled", "items": [{"id": "s-1", "title": "Dentist", "due": 0}, {"id": "s-2", "title": "Ticked", "due": 0, "done": true}]}))),
            ("task-a".to_owned(), o(json!({"title": "Arsenal 1.0", "state": "failed", "stage": "Build broke", "goal": "g-arsenal"}))),
            ("task-b".to_owned(), o(json!({"title": "Picks", "state": "running", "updated": 100.0}))),
            ("task-c".to_owned(), o(json!({"title": "Fresh", "state": "running", "updated": 3000.0}))),
            ("v".to_owned(), o(json!({"preset": "viewer", "title": "Report", "state": "failed"}))),
        ];
        let (w, u) = compute(&all, 3000);
        let ids: Vec<&str> = w.iter().map(|x| x["id"].as_str().unwrap()).collect();
        assert_eq!(ids, ["ask:a-1", "job:task-a", "missed:s-1"]);
        assert_eq!(w[0]["choices"][0]["label"], "Allow");
        assert_eq!(w[0]["line"], "blocks Arsenal 1.0");
        assert_eq!(u.len(), 1);
        assert_eq!((u[0]["title"].as_str(), u[0]["since"].as_i64()), (Some("Picks"), Some(100)));
    }

    #[test]
    fn the_line_is_rewritten_only_when_it_changes_or_grows_old_and_a_choice_reaches_its_source() {
        let d = tempfile::tempdir().unwrap();
        let r = Root(d.path().to_path_buf());
        r.ensure().unwrap();
        crate::goals::ops::write_list(&r, "goals", &o(json!({"id": "goals", "list": "goals", "goals": [], "items": [
            {"id": "a-1", "title": "Pick the cut", "proposal": {"text": "", "actions": [{"label": "Yes", "action": "yes"}]}}]}))).unwrap();
        assert!(write(&r, 100).unwrap());
        assert!(!write(&r, 160).unwrap());
        assert!(write(&r, 100 + REFRESH_S).unwrap());
        let line = choose(&r, "ask:a-1", "yes").unwrap();
        assert!(line.contains("User chose Yes on 'Pick the cut'"), "{line}");
        assert!(plan::items(&r.read("goals").unwrap()).is_empty(), "the answered ask leaves");
        assert!(r.read("scheduled").unwrap()["guarantee"]["waiting"].as_array().unwrap().is_empty());
        assert!(choose(&r, "ask:gone", "yes").unwrap().contains("no longer waits"));
    }
}
