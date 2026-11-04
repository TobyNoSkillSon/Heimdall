//! A plan list's kept copy on a real data folder: every islands-side write refreshes `.kept-<list>.json`, and a writer
//! starts a missing list from its closed or kept copy, never from nothing (review 31 Oct: a fresh goals.json lost every
//! goal). The only test in this binary: it sets FLOATING_ISLANDS_DIR.

use islands::model::{Goals, IslandFile, Json, Paths};
use serde_json::{json, Value};

fn obj(v: Value) -> Json {
    serde_json::from_value(v).unwrap()
}

#[test]
fn every_write_keeps_a_copy_and_a_missing_list_comes_back_from_it() {
    let dir = std::env::temp_dir().join(format!("fi-kept-copy-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::env::set_var("FLOATING_ISLANDS_DIR", &dir);
    Paths::ensure();
    let isl = Paths::islands();
    let goals = json!([{"id": "g-ship", "title": "Ship Heimdall publicly", "about": "not drawn", "tasks": [{"title": "Film", "state": "done"}]}]);
    IslandFile::locked(|| IslandFile::write_island("goals", &obj(json!({"id": "goals", "list": "goals", "items": [{"id": "a", "title": "Ask"}], "goals": goals})))).unwrap();
    let kept_first = IslandFile::read(&isl.join(".kept-goals.json"));
    // A patch (the goal toggle) refreshes it.
    IslandFile::patch("goals", |o| {
        Goals::toggle_expanded(o, "g-ship");
    });
    let kept_toggle = IslandFile::read(&isl.join(".kept-goals.json")).unwrap();
    // Deleted later: a writer starts from the copy, with the newest ask and the toggle.
    std::fs::remove_file(Paths::file("goals")).unwrap();
    let back = IslandFile::plan_start("goals").unwrap();
    // Closed and deleted by an old app: the closed copy wins, still hidden.
    IslandFile::write_atomic(&obj(json!({"id": "goals", "list": "goals", "items": [], "goals": [], "hidden": true})), &isl.join(".closed-goals.json")).unwrap();
    let closed = IslandFile::plan_start("goals").unwrap();
    // Unreadable: never replaced.
    std::fs::write(Paths::file("goals"), "{ not json").unwrap();
    let none = IslandFile::plan_start("goals");
    // A Goals file without goals never replaces a kept copy that has some (re-review 31 Oct).
    std::fs::remove_file(Paths::file("goals")).unwrap();
    IslandFile::locked(|| IslandFile::write_island("goals", &obj(json!({"id": "goals", "list": "goals", "items": [{"id": "z", "title": "Z"}], "goals": goals})))).unwrap();
    IslandFile::locked(|| IslandFile::write_island("goals", &obj(json!({"id": "goals", "list": "goals", "items": []})))).unwrap();
    let kept_guarded = IslandFile::read(&isl.join(".kept-goals.json")).unwrap();
    // A job island keeps no copy.
    IslandFile::locked(|| IslandFile::write_island("build", &obj(json!({"title": "Build"})))).unwrap();
    let job_copy = isl.join(".kept-build.json").exists();
    std::env::remove_var("FLOATING_ISLANDS_DIR");
    let _ = std::fs::remove_dir_all(&dir);

    assert!(kept_first.is_some_and(|k| k["items"][0]["id"] == "a"));
    assert_eq!(kept_toggle["goals"][0]["expanded"], true);
    assert_eq!((back["items"][0]["id"].clone(), back["goals"][0]["expanded"].clone(), back["goals"][0]["about"].clone()), (json!("a"), json!(true), json!("not drawn")));
    assert_eq!(closed["hidden"], true);
    assert_eq!(closed["goals"][0]["id"], "g-ship", "a copy's goals are never dropped");
    assert!(none.is_none());
    assert!(!job_copy);
    assert_eq!(kept_guarded["goals"][0]["id"], "g-ship", "a goals-less write left the kept goals alone");
}
