//! Swift `IslandModelTests/PlanTests.swift`: plan lists, item keys, sinking, proposals, diffs, attention, responses,
//! old-shape files, the helper's lifetime flags and slugs.

use std::collections::HashSet;

use islands::jobj;
use islands::model::*;
use serde_json::{json, Value};

fn item(id: Option<&str>, title: &str, group: Option<&str>, done: bool, extra: Json) -> Value {
    let mut o = jobj! {"title": title};
    if let Some(id) = id {
        o.insert("id".into(), Value::from(id));
    }
    if let Some(g) = group {
        o.insert("group".into(), Value::from(g));
    }
    if done {
        o.insert("done".into(), Value::Bool(true));
    }
    for (k, v) in extra {
        o.entry(k).or_insert(v);
    }
    Value::Object(o)
}
fn it(id: &str, title: &str, group: Option<&str>) -> Value {
    item(Some(id), title, group, false, Json::new())
}
fn ticked(id: &str, title: &str, group: Option<&str>) -> Value {
    item(Some(id), title, group, true, Json::new())
}
fn items(v: Vec<Value>) -> Json {
    jobj! {"items": v}
}
fn keys(i: &[PlanItem]) -> Vec<String> {
    i.iter().map(|x| x.key.clone()).collect()
}
fn strs(v: &[String]) -> Vec<&str> {
    v.iter().map(String::as_str).collect()
}

#[test]
fn plan_islands_dock_left_in_order_with_titles_and_icons() {
    let isl: Vec<Island> = PlanList::ALL.iter().map(|l| Island::new(l, PlanList::new_island(l))).collect();
    // No To do list: Goals, Scheduled, Settled.
    assert_eq!(isl.iter().map(|i| i.list().unwrap()).collect::<Vec<_>>(), ["goals", "scheduled", "settled"]);
    assert_eq!(isl.iter().map(|i| i.side().unwrap()).collect::<Vec<_>>(), ["left", "left", "left"]);
    assert_eq!(isl.iter().map(|i| i.order().unwrap()).collect::<Vec<_>>(), [0.0, 1.0, 2.0]);
    assert_eq!(isl.iter().map(|i| i.title()).collect::<Vec<_>>(), ["Goals", "Scheduled", "Settled"]);
    assert_eq!(isl.iter().map(|i| i.icon().unwrap()).collect::<Vec<_>>(), ["flag", "calendar", "checkmark.seal"]);
    assert!(!isl[2].expanded(), "Settled is a pill by default");
    // The retired To do is known (never drawn, its file left alone); an agent's own checklist is not it.
    assert!(PlanList::legacy_todo("todo", &Json::new()));
    assert!(PlanList::legacy_todo("x", &jobj! {"preset": "todo", "list": "todo"}));
    assert!(!PlanList::legacy_todo("st-approvals", &jobj! {"preset": "todo", "title": "Approvals"}));
    assert_eq!(Island::new("x", jobj! {"preset": "todo"}).title(), "List", "an untitled checklist is never called To do");
    assert_eq!(PlanList::new_island("goals")["goals"], Value::Array(vec![]), "a fresh Goals file has its goals");
    assert!(PlanList::new_island("scheduled").get("goals").is_none());
    // An explicit order or side still wins; a plain To do island without `list` is the To do list.
    assert_eq!(Island::new("x", jobj! {"preset": "todo", "list": "settled", "order": 7}).order(), Some(7.0));
    assert_eq!(Island::new("x", jobj! {"preset": "todo"}).list().as_deref(), Some("todo"));
    assert_eq!(Island::new("x", jobj! {"preset": "task"}).list(), None);
    assert_eq!(Island::new("x", jobj! {"preset": "task"}).order(), None);
}

#[test]
fn item_keys_use_ids_and_fall_back_to_unique_titles() {
    let raw = items(vec![
        it("a", "Arsenal: push branch", None),
        item(None, "Echo bug", None, false, Json::new()),
        item(None, "Echo bug", None, false, Json::new()),
        json!("Plain"),
        it("", "Empty id", None),
    ]);
    assert_eq!(keys(&Plan::items(&raw)), ["a", "Echo bug", "Echo bug#2", "Plain", "Empty id"]);
    assert_eq!(Plan::items(&raw).iter().map(|i| i.id.clone()).collect::<Vec<_>>(), [Some("a".into()), None, None, None, None]);
}

#[test]
fn ticked_items_sink_to_the_bottom_of_their_group() {
    let raw = items(vec![
        ticked("1", "One", Some("running")),
        it("2", "Two", Some("running")),
        it("3", "Three", Some("waiting")),
        ticked("4", "Four", Some("running")),
        it("5", "Five", Some("running")),
        it("6", "Six", None),
    ]);
    let g = Plan::grouped(&Plan::items(&raw), "todo");
    assert_eq!(g.iter().map(|x| x.0.as_str()).collect::<Vec<_>>(), ["running", "waiting", "discussed"]);
    assert_eq!(keys(&g[0].1), ["2", "5", "1", "4"]);
    assert_eq!(keys(&Plan::display_order(&raw, "todo")), ["2", "5", "1", "4", "3", "6"]);
    // Scheduled and Settled: one group without a header, ticked items last.
    let s = Plan::grouped(&Plan::items(&raw), "scheduled");
    assert_eq!(s.iter().map(|x| x.0.as_str()).collect::<Vec<_>>(), [""]);
    assert_eq!(keys(&s[0].1), ["2", "3", "5", "6", "1", "4"]);
}

#[test]
fn scheduled_when_detail_and_proposal_parse() {
    let raw = items(vec![
        item(
            Some("w"),
            "Delete old caches",
            Some("waiting"),
            false,
            jobj! {"proposal": {"text": "~/.cache 12 GB\n~/Library/Caches 4 GB"}, "when": "Tonight", "detail": "Freed 16 GB"},
        ),
        item(Some("c"), "Custom", Some("waiting"), false, jobj! {"proposal": {"text": "Pick", "actions": [{"label": "Do it", "action": "do"}]}}),
    ]);
    let i = Plan::items(&raw);
    assert_eq!(i[0].when.as_deref(), Some("Tonight"));
    assert_eq!(i[0].detail.as_deref(), Some("Freed 16 GB"));
    let p = i[0].proposal.as_ref().unwrap();
    assert_eq!(p.text, "~/.cache 12 GB\n~/Library/Caches 4 GB");
    assert_eq!(p.actions.iter().map(|a| a.1.as_str()).collect::<Vec<_>>(), ["yes", "no", "later"], "actions default to Yes / No / Later");
    assert_eq!(p.actions.iter().map(|a| a.0.as_str()).collect::<Vec<_>>(), ["Yes", "No", "Later"]);
    assert_eq!(i[1].proposal.as_ref().unwrap().actions.iter().map(|a| a.1.as_str()).collect::<Vec<_>>(), ["do"]);
    assert_eq!(Proposal::parse(Some(&json!({"nothing": 1}))), None);
    assert_eq!(Proposal::parse(Some(&json!("Plain text"))).unwrap().actions.len(), 3);
}

#[test]
fn diff_by_id_finds_adds_removes_moves_and_ticks() {
    let old = items(vec![it("a", "A", Some("running")), it("b", "B", Some("running")), it("c", "C", Some("discussed")), it("d", "D", Some("discussed"))]);
    // b renamed (same id: kept, not re-added), a ticked (sinks), c moved to waiting, d removed, e added.
    let new = items(vec![ticked("a", "A", Some("running")), it("b", "B renamed", Some("running")), it("c", "C", Some("waiting")), it("e", "E", Some("discussed"))]);
    let d = ItemDiff::new(&Plan::display_order(&old, "todo"), &Plan::display_order(&new, "todo"));
    assert_eq!(strs(&d.added), ["e"]);
    assert_eq!(strs(&d.removed), ["d"]);
    assert_eq!(strs(&d.ticked), ["a"]);
    assert_eq!(strs(&d.moved), ["c"]);
    assert_eq!(strs(&d.untouched), ["b"]);
    // Without ids the title is the key: a rename is a remove plus an add.
    let (o2, n2) = (Plan::items(&jobj! {"items": ["X", "Y"]}), Plan::items(&jobj! {"items": ["X", "Z"]}));
    let d2 = ItemDiff::new(&o2, &n2);
    assert_eq!(strs(&d2.added), ["Z"]);
    assert_eq!(strs(&d2.removed), ["Y"]);
    assert_eq!(strs(&d2.untouched), ["X"]);
    // Same content, nothing to animate.
    assert_eq!(strs(&ItemDiff::new(&o2, &o2).untouched), ["X", "Y"]);
}

#[test]
fn attention_key_and_derived_waiting() {
    let at = |id: &str, o: Json| Attention::of(&Island::new(id, o));
    assert_eq!(at("j", jobj! {"attention": "question"}).as_deref(), Some("question"));
    assert_eq!(at("j", jobj! {"attention": "waiting"}).as_deref(), Some("waiting"));
    assert_eq!(at("j", jobj! {"attention": "loud"}), None);
    assert_eq!(at("j", Json::new()), None);
    let plan = jobj! {"preset": "todo", "items": [it("w", "Approve", Some("waiting"))]};
    assert_eq!(at("todo", plan.clone()).as_deref(), Some("waiting"), "an open waiting item asks for attention");
    assert_eq!(at("todo", jobj! {"preset": "todo", "items": [ticked("w", "Approve", Some("waiting"))]}), None);
    // A new waiting item changes the signature (pulses again after the user opened it); the same one does not.
    let sig = |o: &Json| Attention::signature(&Island::new("todo", o.clone()));
    let more = jobj! {"preset": "todo", "items": [it("w", "Approve", Some("waiting")), it("x", "Another", Some("waiting"))]};
    assert_ne!(sig(&plan), sig(&more));
    let mut renamed = plan.clone();
    renamed.insert("title".into(), Value::from("Renamed"));
    assert_eq!(sig(&plan), sig(&renamed));
    // A new question (new buttons) does too.
    let q1 = jobj! {"attention": "question", "parts": [{"type": "button", "label": "A", "action": "t3q:1:0"}]};
    let q2 = jobj! {"attention": "question", "parts": [{"type": "button", "label": "A", "action": "t3q:2:0"}]};
    assert_ne!(Attention::signature(&Island::new("j", q1)), Attention::signature(&Island::new("j", q2)));
}

#[test]
fn response_formats() {
    assert_eq!(Responses::tick("todo", "a", true), jobj! {"island": "todo", "kind": "tick", "action": "done", "value": "a"});
    assert_eq!(Responses::tick("todo", "a", false)["action"], "undo");
    assert_eq!(Responses::approval("todo", "caches", "yes"), jobj! {"island": "todo", "kind": "approval", "action": "yes", "value": "caches"});
    // Written as a response file the Rust reader consumes. This is the only test in this binary that sets the variable.
    let dir = std::env::temp_dir().join(format!("fi-plan-{}", std::process::id()));
    std::env::set_var("FLOATING_ISLANDS_DIR", &dir);
    let url = IslandFile::respond_object(&Responses::approval("todo", "caches", "later")).expect("written");
    let o = IslandFile::read(&url);
    std::env::remove_var("FLOATING_ISLANDS_DIR");
    let _ = std::fs::remove_dir_all(&dir);
    assert!(url.file_name().unwrap().to_string_lossy().starts_with("todo-"));
    let o = o.expect("readable");
    assert_eq!(o["kind"], "approval");
    assert_eq!(o["action"], "later");
    assert_eq!(o["value"], "caches");
    assert!(o.get("at").is_some());
}

/// The user's real settled.json (22 Oct): no `preset`, no `list`, nine plain strings. It is the Settled list.
#[test]
fn old_shape_plan_islands_are_known_by_id() {
    let text = r#"{"edgeY":157,"expanded":false,"icon":"checkmark.seal","id":"settled","items":["Islands into Heimdall, bound to threads","Drop replies you talked over","Island closes when message is sent","To do: animations, approvals, Settled group","Arsenal note in my prompt","Arsenal polish list, models, peak memory","Repair OCR and translation models","Simple add-a-model, model folders","Homebrew tap, prepared not published"],"order":9,"side":"left","stage":"Running","state":"running","title":"Scheduled"}"#;
    let raw: Json = serde_json::from_str(text).unwrap();
    let settled = Island::new("settled", raw);
    assert_eq!(settled.preset(), "todo");
    assert_eq!(settled.list().as_deref(), Some("settled"));
    assert_eq!(settled.plan_items().len(), 9);
    assert_eq!(settled.plan_items()[0].title, "Islands into Heimdall, bound to threads");
    assert_eq!(settled.title(), "Scheduled", "its own title is kept");
    assert_eq!(Island::new("todo", Json::new()).list(), None, "todo is no longer a plan list by its id");
    assert_eq!(Island::new("scheduled", jobj! {"preset": "todo"}).list().as_deref(), Some("scheduled"));
    assert_eq!(Island::new("scheduled", jobj! {"items": ["A"]}).title(), "Scheduled");
    assert_eq!(Island::new("x", jobj! {"preset": "todo", "list": "settled"}).list().as_deref(), Some("settled"), "an explicit list still wins");
}

fn args(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| s.to_string()).collect()
}

#[test]
fn parent_pid() {
    assert_eq!(Lifetime::parent_pid(&args(&["--max-hours", "0", "--parent-pid", "4242"])), Some(4242));
    assert_eq!(Lifetime::parent_pid(&[]), None);
    assert_eq!(Lifetime::parent_pid(&args(&["--parent-pid"])), None);
    assert_eq!(Lifetime::parent_pid(&args(&["--parent-pid", "soon"])), None);
    assert_eq!(Lifetime::parent_pid(&args(&["--parent-pid", "1"])), None, "launchd is never a parent to follow");
    assert_eq!(Lifetime::parent_pid(&args(&["--parent-pid", "-5"])), None);
}

#[test]
fn max_hours() {
    assert_eq!(Lifetime::max_hours(&[]), Some(24.0));
    assert_eq!(Lifetime::max_hours(&args(&["--max-hours", "2.5"])), Some(2.5));
    assert_eq!(Lifetime::max_hours(&args(&["--max-hours", "0"])), None, "0 means no limit");
    assert_eq!(Lifetime::max_hours(&args(&["--selftest", "--max-hours", "-1"])), None);
    assert_eq!(Lifetime::max_hours(&args(&["--max-hours", "soon"])), Some(24.0));
    assert_eq!(Lifetime::max_hours(&args(&["--max-hours"])), Some(24.0));
}

#[test]
fn slug_ids_are_stable_and_unique() {
    let none = HashSet::new();
    assert_eq!(Plan::slug("Arsenal: push branch", &none), "arsenal-push-branch");
    assert_eq!(Plan::slug("Arsenal: push branch", &HashSet::from(["arsenal-push-branch".to_owned()])), "arsenal-push-branch-2");
    assert_eq!(Plan::slug("!!!", &none), "item");
    assert!(Plan::slug(&"word ".repeat(30), &none).chars().count() <= 40);
}

#[test]
fn a_row_symbol_comes_from_its_own_icon_then_its_project_then_its_kind() {
    let raw = items(vec![
        it("a", "Echo guard: keep raw audio in archive", Some("waiting")),
        it("b", "Voices: pick one by ear", Some("waiting")),
        it("c", "Stream the Arsenal builds", Some("discussed")),
        item(Some("d"), "Mac: empty the Trash", Some("waiting"), false, jobj! {"icon": "trash.circle"}),
        it("e", "Pick a release date", Some("waiting")),
        it("f", "Something new", None),
    ]);
    let i = Plan::items(&raw);
    let s: Vec<String> = i.iter().map(|p| p.symbol("todo")).collect();
    // The project prefix wins over words later in the title; plurals match; the item's own icon wins over all.
    assert_eq!(s, ["speaker.wave.2", "waveform", "shippingbox", "trash.circle", "flag", "bubble.left.and.bubble.right"]);
    assert_eq!(i[5].symbol("scheduled"), "clock");
    assert_eq!(i[5].symbol("settled"), "checkmark");
    assert_eq!(Plan::items(&items(vec![it("g", "Nothing known", Some("waiting"))]))[0].symbol("todo"), "hand.raised");
    assert!(PLAN_SYMBOLS.iter().all(|(w, s)| !w.is_empty() && *w == w.to_lowercase() && !s.is_empty()));
}

#[test]
fn a_row_opens_only_when_it_has_more_than_its_title() {
    let raw = items(vec![
        it("a", "Plain", None),
        item(Some("b"), "Explained", None, false, jobj! {"detail": "Why and what next"}),
        item(Some("c"), "Asks", Some("waiting"), false, jobj! {"proposal": {"text": "Delete?"}}),
        item(Some("d"), "Answered and ticked", Some("waiting"), true, jobj! {"proposal": {"text": "Delete?"}}),
    ]);
    assert_eq!(Plan::items(&raw).iter().map(|p| p.discloses()).collect::<Vec<_>>(), [false, true, true, false]);
}

#[test]
fn a_closed_row_shows_the_first_line_of_its_detail_with_spaces_collapsed() {
    let raw = items(vec![
        it("a", "Plain", None),
        item(Some("b"), "Explained", None, false, jobj! {"detail": "\n   Spaced   out \t first  \nSecond line"}),
        item(Some("c"), "Blank", None, false, jobj! {"detail": " \n \n"}),
    ]);
    assert_eq!(Plan::items(&raw).iter().map(|p| p.detail_line()).collect::<Vec<_>>(), [None, Some("Spaced out first".to_owned()), None]);
}

/// Bug 2026-10-31T15-57-16: the user closed To do by accident and the whole list was deleted. Closing a plan list now hides
/// it (file and items kept, minimised); clearing `hidden` brings it back. Any other island is still removed.
#[test]
fn closing_a_plan_list_hides_it_and_keeps_its_items() {
    for id in ["goals", "scheduled", "settled"] {
        let o = jobj! {"preset": "todo", "list": id, "expanded": true, "items": [it("a", "Keep me", None)], "edgeY": 300};
        let c = PlanList::closed(id, &o).unwrap_or_else(|| panic!("{id} is kept"));
        assert_eq!(c["hidden"], true, "{id}");
        assert_eq!(c["expanded"], false, "{id}");
        assert_eq!(c["items"], o["items"], "{id}: every item kept");
        assert_eq!(c["edgeY"], 300, "{id}: its place kept");
        let island = Island::new(id, c.clone());
        assert!(island.hidden());
        let mut back = c;
        back.remove("hidden");
        assert!(!Island::new(id, back).hidden(), "{id} comes back when hidden is cleared");
    }
    // An old-shape plan list known only by its id, and one named by its `list` key under another id.
    assert!(PlanList::closed("settled", &Json::new()).is_some());
    assert!(PlanList::closed("my-goals", &jobj! {"preset": "todo", "list": "goals"}).is_some());
    // A job island, or an ad-hoc todo-preset island, still goes.
    assert!(PlanList::closed("build", &jobj! {"title": "Build", "state": "running"}).is_none());
    assert!(PlanList::closed("st-approvals", &jobj! {"preset": "todo", "items": []}).is_none());
}

#[test]
fn goals_attention_comes_only_from_asks_waiting_on_owner() {
    let at = |o: Json| Attention::of(&Island::new("goals", o));
    let goals = serde_json::json!([{"id": "g", "title": "Ship Arsenal", "tasks": [{"title": "Tap", "state": "now"}]}]);
    assert_eq!(at(jobj! {"preset": "todo", "list": "goals", "items": [], "goals": goals.clone()}), None, "a goal never rings the bell");
    assert_eq!(at(jobj! {"preset": "todo", "list": "goals", "items": [{"id": "a", "title": "Approve"}], "goals": goals.clone()}).as_deref(), Some("waiting"));
    assert_eq!(at(jobj! {"preset": "todo", "list": "goals", "items": [{"id": "a", "title": "Approve", "chosen": "yes"}]}), None, "answered");
    assert_eq!(at(jobj! {"preset": "todo", "list": "goals", "items": [{"id": "a", "title": "Approve", "done": true}]}), None, "ticked");
    let sig = |o: Json| Attention::signature(&Island::new("goals", o));
    assert_ne!(
        sig(jobj! {"list": "goals", "items": [{"id": "a", "title": "A"}]}),
        sig(jobj! {"list": "goals", "items": [{"id": "a", "title": "A"}, {"id": "b", "title": "B"}]}),
        "a new ask pulses again"
    );
}

/// Review 31 Oct: a missing goals.json was made fresh by a writer (the due move, `islands plan add`) and the goals and
/// their finished tasks were lost. A writer starts from the file, else the closed copy (still hidden), else the kept
/// copy; a Goals list never loses its goals while a copy holds some.
#[test]
fn a_plan_list_is_written_on_from_its_kept_copy_never_from_nothing() {
    let goals = json!([{"id": "g", "title": "Ship Arsenal", "about": "kept", "tasks": [{"title": "Tap", "state": "done"}]}]);
    let closed = jobj! {"id": "goals", "list": "goals", "items": [], "goals": goals.clone(), "hidden": true};
    let kept = jobj! {"id": "goals", "list": "goals", "items": [{"id": "k", "title": "Kept ask"}], "goals": goals.clone()};
    // The file wins.
    let file = jobj! {"id": "goals", "list": "goals", "items": [{"id": "f", "title": "File ask"}], "goals": goals.clone()};
    assert_eq!(PlanList::start_from("goals", Some(file.clone()), Some(closed.clone()), Some(kept.clone())), file);
    // Closed and its file gone: the closed copy, still hidden (it stays closed).
    let mut c = closed.clone();
    c.remove("hidden");
    let o = PlanList::start_from("goals", None, Some(c), Some(kept.clone()));
    assert_eq!((o["hidden"].clone(), o["goals"].clone()), (Value::Bool(true), goals.clone()));
    // Gone without a close: the copy kept at its last write, as it was (not hidden).
    let o = PlanList::start_from("goals", None, None, Some(kept.clone()));
    assert_eq!(o, kept);
    // A file without goals (an older writer) takes them from a copy; unknown keys on a goal ride along.
    let o = PlanList::start_from("goals", Some(jobj! {"id": "goals", "list": "goals", "items": []}), None, Some(kept.clone()));
    assert_eq!(o["goals"][0]["about"], "kept");
    // Nothing anywhere: fresh, with its empty goals.
    assert_eq!(PlanList::start_from("goals", None, None, None), PlanList::new_island("goals"));
    assert_eq!(PlanList::closed_copy("goals"), ".closed-goals.json");
    assert_eq!(PlanList::kept_copy("scheduled"), ".kept-scheduled.json");
}
