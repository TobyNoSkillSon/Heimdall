//! The clean slate ("we will start from a clean slate and you will fill in the most important goals and
//! scheduled, settled can be gone"; `docs/one-system.md`). Once, on the first start of the one-system build (Goals
//! carries no `slate`): the three lists, their kept and closed copies and the old To do are copied into
//! `<root>/record/clean-slate-<stamp>/`, then Goals, Scheduled and Settled are written empty with `slate`. Nothing is
//! deleted: the old items stay in the record and are not shown. The old Scheduled and Settled items never come back
//! as proposals (his decision, 2 Nov); the old active goals do, one at a time, for his yes. Every list keeps its
//! `removed` memory (what he did not want stays unwanted; bug note 2026-11-01T12-48-26).

use super::files::{read_object, write_atomic, Root};
use super::proposals::{self, Draft};
use crate::goals::ops;
use anyhow::Result;
use serde_json::{json, Map, Value};

const LISTS: [&str; 3] = ["goals", "scheduled", "settled"];

fn s<'a>(o: &'a Map<String, Value>, k: &str) -> &'a str { o.get(k).and_then(Value::as_str).unwrap_or("") }

/// The slate's time, when this folder has had it.
pub fn slate(root: &Root) -> Option<i64> {
    root.read("goals").or_else(|| ops::kept(root, "goals")).and_then(|o| o.get("slate").and_then(Value::as_i64))
}

/// Runs the clean slate once (call before anything else writes the lists: `islands::live::start`, ahead of
/// `goals::ops::ensure`). `seed`: the personal edition, whose first run proposes the four seed goals. Returns what it did.
pub fn ensure(root: &Root, now: i64, seed: bool) -> Result<Vec<String>> {
    if slate(root).is_some() { return Ok(Vec::new()) }
    root.ensure()?;
    let mut did = Vec::new();
    root.locked(|| {
        if slate(root).is_some() { return Ok(()) }
        let isl = root.islands();
        // What existed: each list, its copies, and the retired To do.
        let mut names: Vec<String> = Vec::new();
        for l in LISTS {
            for n in [format!("{l}.json"), format!(".kept-{l}.json"), format!(".closed-{l}.json")] { names.push(n) }
        }
        names.extend(["todo.json".to_owned(), ".todo-migrated.json".to_owned()]);
        let present: Vec<&String> = names.iter().filter(|n| isl.join(n).is_file()).collect();
        let goals_before = ops::current(root, "goals");
        // What the user removed from each list stays removed across the slate (bug note 2026-11-01T12-48-26).
        let removed_before: Vec<(&str, Value)> = LISTS.iter()
            .filter_map(|l| ops::current(root, l).and_then(|o| o.get("removed").filter(|r| r.is_array()).cloned()).map(|r| (*l, r))).collect();
        let mut proposed = 0;
        if !present.is_empty() {
            let dir = proposals::record_dir(root).join(format!("clean-slate-{}", chrono::Local::now().format("%Y%m%d-%H%M%S")));
            std::fs::create_dir_all(&dir)?;
            for n in &present { std::fs::copy(isl.join(n), dir.join(n))?; }
            did.push(format!("the old lists ({} files) are kept in {}", present.len(), dir.display()));
        }
        // The lists anew, each with the slate (positions the user gave them kept). Goals last: it carries the mark, so a
        // failure before it leaves the slate to run again at the next start (review 2 Nov).
        for l in ["scheduled", "settled", "goals"] {
            let old = root.read(l);
            let mut o = super::plan::fresh(l);
            if let Some(old) = &old {
                for k in ["side", "order", "edgeY", "position", "hidden"] { if let Some(v) = old.get(k) { o.insert(k.into(), v.clone()); } }
            }
            o.insert("slate".into(), json!(now));
            o.insert("items".into(), json!([]));
            if l == "goals" {
                o.insert("goals".into(), json!([]));
                o.insert("seeded".into(), json!(true));
                // Goals refill from the user's yes, drafted in the same hold of the lock (never a slate without them).
                for d in drafts(goals_before.as_ref(), seed) {
                    match proposals::push(root, &mut o, &d, now) {
                        Ok(_) => proposed += 1,
                        Err(e) => eprintln!("Clean slate: goal {:?} not proposed: {e:#}", d.title),
                    }
                }
            }
            // After the old goals are proposed: the memory keeps out what comes later, never his own goals.
            if let Some((_, r)) = removed_before.iter().find(|(k, _)| *k == l) { o.insert("removed".into(), r.clone()); }
            ops::write_list(root, l, &o)?;
            let closed = ops::closed_file(root, l);
            if closed.exists() { std::fs::remove_file(&closed)?; }
        }
        // The retired To do is in the record; the Goals job must not migrate it again.
        let todo = isl.join("todo.json");
        if todo.exists() {
            let migrated = isl.join(".todo-migrated.json");
            if !migrated.exists() { write_atomic(&migrated, &Value::Object(read_object(&todo).unwrap_or_default()))?; }
            std::fs::remove_file(&todo)?;
        }
        did.push("Goals, Scheduled and Settled start empty".into());
        if proposed > 0 { did.push(format!("{proposed} goal(s) proposed for User's yes")) }
        Ok(())
    })?;
    Ok(did)
}

/// The goals the slate proposes: the old active goals (a truly first run: the seeds, personal edition only).
fn drafts(goals_before: Option<&Map<String, Value>>, seed: bool) -> Vec<Draft> {
    match goals_before.filter(|o| ops::has_goals(o)) {
        Some(o) => o["goals"].as_array().into_iter().flatten().filter_map(Value::as_object).filter(|g| ops::goal_state(g) == "active")
            .map(|g| Draft { list: "goals".into(), kind: "goal".into(), title: s(g, "title").into(), icon: s(g, "icon").into(), about: s(g, "about").into(),
                             why: "One of your goals before the clean slate.".into(), by: "slate".into(), ..Draft::default() }).collect(),
        None if seed => ops::SEED.iter().map(|(_, title, icon, about)| Draft { list: "goals".into(), kind: "goal".into(), title: (*title).into(),
                             icon: (*icon).into(), about: (*about).into(), by: "slate".into(), ..Draft::default() }).collect(),
        None => Vec::new(),
    }
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

    #[test]
    fn the_old_lists_go_to_the_record_the_lists_start_empty_and_it_runs_once() {
        let (_d, r) = root();
        let goals = json!({"id": "goals", "preset": "todo", "list": "goals", "edgeY": 287, "items": [{"id": "a-1", "title": "Old ask"}],
            "removed": [{"title": "Use cases: pick ideas", "at": 900}],
            "goals": [{"id": "g-job", "title": "Get a job", "icon": "briefcase", "state": "active", "tasks": [{"id": "t-1", "title": "CV", "state": "done"}]},
                      {"id": "g-old", "title": "Old finished", "state": "done", "tasks": []}]});
        let sched = json!({"id": "scheduled", "preset": "todo", "list": "scheduled", "items": [{"id": "p-1", "title": "Editions: review", "when": "Later"}]});
        let settled = json!({"id": "settled", "preset": "todo", "list": "settled", "items": [{"id": "s-1", "title": "Menu installed", "when": "Today", "done": true}],
            "removed": [{"title": "Never this", "at": 0}]});
        for (l, v) in [("goals", &goals), ("scheduled", &sched), ("settled", &settled)] { ops::write_list(&r, l, v.as_object().unwrap()).unwrap(); }
        std::fs::write(ops::closed_file(&r, "scheduled"), sched.to_string()).unwrap();

        let did = ensure(&r, 1000, true).unwrap();
        assert!(did.iter().any(|d| d.contains("start empty")), "{did:?}");
        // The record holds every old file, whole.
        let rec = std::fs::read_dir(proposals::record_dir(&r)).unwrap().flatten().find(|e| e.file_name().to_string_lossy().starts_with("clean-slate-")).unwrap().path();
        let kept: Value = serde_json::from_str(&std::fs::read_to_string(rec.join("scheduled.json")).unwrap()).unwrap();
        assert_eq!(kept["items"][0]["title"], "Editions: review");
        assert!(rec.join(".closed-scheduled.json").exists() && rec.join("settled.json").exists() && rec.join(".kept-goals.json").exists());
        // Empty lists with the slate; the user's place kept; Settled keeps what he removed.
        let g = r.read("goals").unwrap();
        assert_eq!((g["slate"].as_i64(), g["goals"].as_array().unwrap().len(), g["items"].as_array().unwrap().len(), g["edgeY"].as_i64()), (Some(1000), 0, 0, Some(287)));
        assert!(r.read("scheduled").unwrap()["items"].as_array().unwrap().is_empty());
        assert!(!ops::closed_file(&r, "scheduled").exists());
        let st = r.read("settled").unwrap();
        assert!(st["items"].as_array().unwrap().is_empty());
        assert_eq!(st["removed"][0]["title"], "Never this");
        // So does Goals (bug note 2026-11-01T12-48-26): an ask he removed is not proposed after the slate either.
        assert_eq!(g["removed"][0]["title"], "Use cases: pick ideas");
        let again = Draft { list: "goals".into(), kind: "goal".into(), title: "Pick use cases ideas".into(), by: "goals".into(), ..Draft::default() };
        assert!(proposals::propose(&r, &again, 1001).is_err());
        // The old active goal comes back as a proposal, not as a goal; the finished one does not.
        let q = proposals::queue(&g);
        assert_eq!(q.iter().map(|p| p["title"].as_str().unwrap()).collect::<Vec<_>>(), ["Get a job"]);
        assert_eq!((q[0]["front"].as_i64(), q[0].get("shown").is_none()), (Some(1000), true), "on screen; its two days start when User sees it");
        // Scheduled and Settled items never come back as proposals.
        assert!(proposals::queue(&r.read("scheduled").unwrap()).is_empty());
        // Nothing brings the old goals back from a copy, and the Goals job does not reseed.
        assert!(ops::ensure(&r, 1001, true).unwrap().is_empty());
        assert!(ops::current(&r, "goals").unwrap()["goals"].as_array().unwrap().is_empty());
        // Once.
        assert!(ensure(&r, 2000, true).unwrap().is_empty());
        assert_eq!(slate(&r), Some(1000));
    }

    #[test]
    fn a_first_run_proposes_the_seed_goals_and_the_public_edition_nothing() {
        let (_d, r) = root();
        ensure(&r, 5, true).unwrap();
        let g = r.read("goals").unwrap();
        assert_eq!(proposals::queue(&g).len(), 4);
        assert!(g["goals"].as_array().unwrap().is_empty());
        assert!(ops::ensure(&r, 6, true).unwrap().is_empty(), "no seeding over the slate");
        let (_d2, p) = root();
        ensure(&p, 5, false).unwrap();
        assert!(proposals::queue(&p.read("goals").unwrap()).is_empty());
    }

    #[test]
    fn an_old_to_do_is_kept_in_the_record_and_never_migrated_after_the_slate() {
        let (_d, r) = root();
        std::fs::write(r.file("todo"), json!({"id": "todo", "items": [{"title": "Old waiting", "group": "waiting"}]}).to_string()).unwrap();
        ensure(&r, 5, true).unwrap();
        assert!(!r.file("todo").exists());
        ops::ensure(&r, 6, true).unwrap();
        assert!(r.read("goals").unwrap()["items"].as_array().unwrap().is_empty());
    }

    /// Opt-in: the clean slate on a copy of a real islands folder (`SLATE_FROM=<FloatingIslands dir>`; the copy is in a
    /// temp dir, the source is only read). Prints what it kept and what it proposes.
    #[test]
    #[ignore]
    fn on_a_copy_of_a_real_folder() {
        let Some(src) = std::env::var_os("SLATE_FROM") else { return };
        let (_d, r) = root();
        for e in std::fs::read_dir(std::path::Path::new(&src).join("islands")).unwrap().flatten() {
            if e.path().is_file() { std::fs::copy(e.path(), r.islands().join(e.file_name())).unwrap(); }
        }
        let before: Vec<(String, usize)> = LISTS.iter().map(|l| (l.to_string(), r.read(l).map(|o| super::super::plan::items(&o).len() + o.get("goals").and_then(Value::as_array).map_or(0, Vec::len)).unwrap_or(0))).collect();
        let did = ensure(&r, 1_793_600_000, true).unwrap();
        println!("before: {before:?}\ndid: {did:#?}");
        for l in LISTS {
            let o = r.read(l).unwrap();
            println!("{l}: items {} goals {} proposals {:?}", super::super::plan::items(&o).len(), o.get("goals").and_then(Value::as_array).map_or(0, Vec::len),
                proposals::queue(&o).iter().map(|p| p["title"].as_str().unwrap_or("").to_owned()).collect::<Vec<_>>());
        }
        let rec = std::fs::read_dir(proposals::record_dir(&r)).unwrap().flatten().next().unwrap().path();
        for l in LISTS {
            let kept: Value = serde_json::from_str(&std::fs::read_to_string(rec.join(format!("{l}.json"))).unwrap()).unwrap();
            let orig: Value = serde_json::from_str(&std::fs::read_to_string(std::path::Path::new(&src).join("islands").join(format!("{l}.json"))).unwrap()).unwrap();
            assert_eq!(kept, orig, "{l} kept whole in the record");
        }
        assert!(ops::ensure(&r, 1_793_600_100, true).unwrap().is_empty());
    }
}

