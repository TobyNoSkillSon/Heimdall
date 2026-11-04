//! The `islands` binary on what the Swift parity script cannot check exactly: `--due` values relative to the clock,
//! `view` leaving `updated` alone (as Swift does), and the data-folder path. Each test runs in its own temporary data folder.

use std::path::{Path, PathBuf};
use std::process::Command;

use islands::model::due::Cal;
use islands::model::{time, Due, IslandFile, Json};
use serde_json::Value;

struct Lab {
    dir: PathBuf,
    /// Run as Heimdall's agents do, without the lab's plan writes.
    strict: bool,
}

struct Out {
    code: i32,
    stdout: String,
    stderr: String,
}

impl Lab {
    fn new(tag: &str) -> Lab {
        let dir = std::env::temp_dir().join(format!("islands-cli-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Lab { dir, strict: false }
    }
    fn run_in(&self, cwd: &Path, data: &str, args: &[&str]) -> Out {
        // The fixtures below write Scheduled and Settled straight (lab only; Heimdall's plan_edit is the door).
        let o = Command::new(env!("CARGO_BIN_EXE_islands")).args(args).current_dir(cwd).env("FLOATING_ISLANDS_DIR", data)
            .env("ISLANDS_LAB_PLAN_WRITES", if self.strict { "0" } else { "1" }).output().unwrap();
        Out {
            code: o.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
        }
    }
    fn run(&self, args: &[&str]) -> Out {
        self.run_in(&self.dir, &self.dir.join("data").to_string_lossy(), args)
    }
    fn island(&self, id: &str) -> Json {
        IslandFile::read(&self.dir.join("data/islands").join(format!("{id}.json"))).expect("island file")
    }
    fn raw(&self, id: &str) -> String {
        std::fs::read_to_string(self.dir.join("data/islands").join(format!("{id}.json"))).unwrap()
    }
}

impl Drop for Lab {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn items(o: &Json) -> Vec<Json> {
    o["items"].as_array().unwrap().iter().map(|v| v.as_object().unwrap().clone()).collect()
}

fn due_of(o: &Json, k: usize) -> Option<f64> {
    items(o)[k].get("due").and_then(Value::as_f64)
}

#[test]
fn scheduled_add_stores_due_as_epoch_seconds() {
    let lab = Lab::new("due-add");
    let cal = Cal::current();
    let out = lab.run(&["plan", "scheduled", "add", "Push branch", "--due", "2026-10-24 10:00"]);
    assert_eq!((out.code, out.stdout.as_str()), (0, "plan scheduled · added 1 · 1 open\n"), "{}", out.stderr);
    let o = lab.island("scheduled");
    assert_eq!(due_of(&o, 0), cal.local(2026, 10, 24, 10, 0, 0));
    assert!(items(&o)[0]["due"].is_i64(), "a whole number, no fraction");
    assert!(lab.raw("scheduled").contains(&format!("\"due\" : {}", cal.local(2026, 10, 24, 10, 0, 0).unwrap() as i64)));

    let before = time::now();
    lab.run(&["plan", "scheduled", "add", "Soon", "--due", "+2h"]);
    let d = due_of(&lab.island("scheduled"), 1).unwrap();
    assert!((d - (before + 7200.0)).abs() <= 2.0, "{d} vs {before}");

    lab.run(&["plan", "scheduled", "add", "Tomorrow", "--due", "tomorrow 10:00"]);
    lab.run(&["plan", "scheduled", "add", "Tonight", "--due", "tonight"]);
    lab.run(&["plan", "scheduled", "add", "Offset", "--due", "2026-10-24T10:00:00+02:00"]);
    let o = lab.island("scheduled");
    let now = time::now();
    assert_eq!(due_of(&o, 2), cal.at_time(10, 0, 0, cal.add_days(1, now).unwrap()));
    assert_eq!(due_of(&o, 3), cal.at_time(23, 0, 0, now));
    assert_eq!(due_of(&o, 4), Some(time::parse_iso8601("2026-10-24T08:00:00Z").unwrap()));
    assert_eq!(items(&o)[4]["id"], "offset", "ids as before");
}

#[test]
fn nothing_enters_scheduled_or_settled_from_the_cli() {
    let mut lab = Lab::new("one-system");
    lab.strict = true;
    for args in [&["plan", "scheduled", "add", "Push", "--due", "+2h"][..], &["plan", "settled", "add", "Done thing"][..]] {
        let out = lab.run(args);
        assert_eq!(out.code, 2, "{args:?}: {}", out.stdout);
        assert!(out.stderr.contains("plan_edit"), "{}", out.stderr);
    }
    // An ask in Goals is still the CLI's to add, and it cannot be carried into Scheduled from here.
    assert_eq!(lab.run(&["plan", "goals", "add", "Pick the icon"]).code, 0);
    assert_eq!(lab.run(&["plan", "goals", "move", "1", "--to", "scheduled", "--due", "+2h"]).code, 2);
    assert!(!lab.dir.join("data/islands/scheduled.json").exists() && !lab.dir.join("data/islands/settled.json").exists());
}

#[test]
fn unreadable_due_fails_with_one_line() {
    let lab = Lab::new("due-bad");
    let out = lab.run(&["plan", "scheduled", "add", "X", "--due", "Friday-ish"]);
    assert_eq!(out.code, 2);
    assert_eq!(out.stdout, "");
    assert_eq!(out.stderr.lines().count(), 1, "{}", out.stderr);
    assert_eq!(out.stderr, "islands plan: --due \"Friday-ish\" is not a time (\"2026-10-24 10:00\", \"tomorrow 10:00\", +2h)\n");
    assert!(!lab.dir.join("data/islands/scheduled.json").exists(), "nothing written");
    let out = lab.run(&["plan", "scheduled", "add", "X", "--due"]);
    assert_eq!((out.code, out.stderr.as_str()), (2, "islands plan: --due needs a value\n"));
}

#[test]
fn list_shows_due_over_when() {
    let lab = Lab::new("due-list");
    lab.run(&["plan", "scheduled", "add", "Both", "--when", "Fri", "--due", "+3h"]);
    lab.run(&["plan", "scheduled", "add", "Only when", "--when", "Sat"]);
    let d = due_of(&lab.island("scheduled"), 0).unwrap();
    let label = Due::label(d, time::now(), &Cal::current()).0;
    let out = lab.run(&["plan", "scheduled"]);
    assert_eq!(out.stdout, format!("Scheduled (scheduled)\n  1. [ ] Both · {label}  (both)\n  2. [ ] Only when · Sat  (only-when)\n"));
    assert!(!items(&lab.island("scheduled"))[0].contains_key("when"), "a due replaces when");
    // An item written with both (by hand or an older writer) still shows the due.
    std::fs::write(
        lab.dir.join("data/islands/scheduled.json"),
        format!(r#"{{"id":"scheduled","items":[{{"id":"both","title":"Both","when":"Fri","due":{d}}}]}}"#),
    )
    .unwrap();
    assert!(lab.run(&["plan", "scheduled"]).stdout.contains(&format!("Both · {label}  (both)")));
    // `islands plan` (all three lists) shows it the same way.
    assert!(lab.run(&["plan"]).stdout.contains(&format!("Both · {label}  (both)")));
}

#[test]
fn move_sets_due_and_drops_when() {
    let lab = Lab::new("due-move");
    lab.run(&["plan", "scheduled", "add", "Push", "--when", "Fri"]);
    let out = lab.run(&["plan", "scheduled", "move", "1", "--due", "+30m"]);
    assert_eq!((out.code, out.stdout.as_str()), (0, "plan scheduled · moved 1 · 1 open\n"), "--due alone is enough: {}", out.stderr);
    let it = &items(&lab.island("scheduled"))[0];
    let d = it["due"].as_f64().unwrap();
    assert!(!it.contains_key("when"));
    // Carried to another list with the item; an old caller's `--to todo` means Goals' asks (there is no To do).
    lab.run(&["plan", "scheduled", "move", "push", "--to", "todo"]);
    assert_eq!(due_of(&lab.island("goals"), 0), Some(d));
    assert!(!lab.dir.join("data/islands/todo.json").exists(), "no todo.json is written");
    // `islands plan todo …` and `islands todo …` too.
    lab.run(&["plan", "todo", "add", "Old caller"]);
    lab.run(&["todo", "add", "Older caller"]);
    assert_eq!(items(&lab.island("goals")).len(), 3);
    assert!(!lab.dir.join("data/islands/todo.json").exists());
    assert!(!lab.run(&["plan", "--help"]).stdout.contains("To do"), "no To do in the help");
    assert_eq!(lab.run(&["plan", "move", "push", "--due", "whenever"]).code, 2);
}

#[test]
fn view_leaves_updated_alone_like_swift() {
    let lab = Lab::new("view");
    std::fs::write(lab.dir.join("notes.md"), "# A\n").unwrap();
    let out = lab.run(&["view", "notes.md"]);
    assert_eq!(out.stdout, "view view-notes.md · notes · viewer done dock right open\n");
    assert!(!lab.island("view-notes.md").contains_key("updated"));
}

#[test]
fn data_folder_resolves_like_the_swift_build() {
    let lab = Lab::new("dir");
    let rel = lab.run_in(&lab.dir, "rel/../q/", &["dir"]);
    // Relative to the working directory as getcwd reports it (symlinks resolved), dot segments removed.
    assert_eq!(rel.stdout.trim(), lab.dir.canonicalize().unwrap().join("q").to_string_lossy());
    let abs = format!("{}//b/./", lab.dir.display());
    let out = lab.run_in(&lab.dir, &abs, &["dir"]);
    assert_eq!(out.stdout.trim(), format!("{}/b/.", lab.dir.display()), "doubled and trailing slashes go; dot segments stay");
}

/// Every island off screen at once and back. Only `visibility.json` changes; no island file does.
#[test]
fn hide_all_and_show_all_write_only_the_visibility_flag() {
    let lab = Lab::new("visibility");
    assert_eq!(lab.run(&["put", "tests", "--title", "Tests", "--state", "running"]).code, 0);
    let before = lab.raw("tests");
    let flag = lab.dir.join("data/visibility.json");
    let out = lab.run(&["hide-all"]);
    assert_eq!((out.code, out.stdout.trim(), out.stderr.as_str()), (0, "islands hidden", ""));
    assert_eq!(IslandFile::read(&flag).unwrap()["hidden"], Value::Bool(true));
    assert_eq!(lab.raw("tests"), before, "the island's file is untouched");
    let out = lab.run(&["show-all"]);
    assert_eq!((out.code, out.stdout.trim()), (0, "islands shown"));
    assert_eq!(IslandFile::read(&flag).unwrap()["hidden"], Value::Bool(false));
    assert_eq!(lab.raw("tests"), before);
    assert!(lab.run(&["--help"]).stdout.contains("islands hide-all|show-all"));
}

/// Goals (31 Oct): `islands plan` works on Goals by default; its verbs work on the asks; the goals are listed compactly.
#[test]
fn plan_goals_lists_asks_and_goals_compactly() {
    let lab = Lab::new("goals");
    let out = lab.run(&["plan", "add", "Approve caches", "--proposal", "Delete 12 GB", "--goal", "g-ship"]);
    assert_eq!((out.code, out.stdout.as_str()), (0, "plan goals · added 1 · 1 open\n"), "{}", out.stderr);
    let o = lab.island("goals");
    assert_eq!((o["list"].as_str(), o["title"].as_str(), o["icon"].as_str()), (Some("goals"), Some("Goals"), Some("flag")));
    let ask = &items(&o)[0];
    assert!(ask.get("group").is_none(), "an ask has no group: {ask:?}");
    assert_eq!(ask["goal"], "g-ship");
    let now = time::now();
    let mut o = lab.island("goals");
    o.insert("goals".into(), serde_json::json!([
        {"id": "g-done", "title": "Get a job", "state": "done"},
        {"id": "g-ship", "title": "Ship Heimdall publicly", "now": "Opening film nearly final", "moved": now - 7200.0,
         "tasks": [{"title": "Opening film", "state": "done"}, {"title": "Privacy scan", "state": "now"}, {"title": "Signing", "state": "next"}]}]));
    IslandFile::write_atomic(&o, &lab.dir.join("data/islands/goals.json")).unwrap();
    let out = lab.run(&["plan", "goals"]);
    assert_eq!(
        out.stdout,
        "Goals (goals)\n  1. [ ] Approve caches · proposal  (approve-caches)\n  Goals\n  - Ship Heimdall publicly · 1/3 tasks · moved 2 h ago  (g-ship)\n      Opening film nearly final · now: Privacy scan\n  - Get a job · done  (g-done)\n"
    );
    // `islands plan` lists Goals first and no empty To do once To do is gone.
    let all = lab.run(&["plan"]).stdout;
    assert!(all.starts_with("Goals (goals)") && !all.contains("To do"), "{all}");
    // Ticking an ask keeps the goals as they were.
    lab.run(&["plan", "done", "1"]);
    let o = lab.island("goals");
    assert_eq!(items(&o)[0]["done"], true);
    assert_eq!(o["goals"].as_array().unwrap().len(), 2);
    // The kept copy follows the CLI's write.
    assert_eq!(IslandFile::read(&lab.dir.join("data/islands/.kept-goals.json")), Some(o));
}

/// Review 31 Oct: `islands plan add` with goals.json gone (an agent removed it, an old app closed it) made a fresh list
/// and the goals were lost. It starts from the closed copy (still hidden) or the kept copy; an unparseable file stays.
#[test]
fn plan_add_starts_a_missing_goals_from_its_kept_copy() {
    let lab = Lab::new("goals-kept");
    lab.run(&["dir"]);
    let isl = lab.dir.join("data/islands");
    std::fs::create_dir_all(&isl).unwrap();
    let goals = serde_json::json!([{"id": "g-ship", "title": "Ship Heimdall publicly", "about": "not drawn", "tasks": [{"title": "Film", "state": "done"}]}]);
    let kept: Json = serde_json::from_value(serde_json::json!({"id": "goals", "preset": "todo", "list": "goals", "items": [], "goals": goals})).unwrap();
    IslandFile::write_atomic(&kept, &isl.join(".kept-goals.json")).unwrap();
    assert_eq!(lab.run(&["plan", "add", "Approve caches"]).code, 0);
    let o = lab.island("goals");
    assert_eq!(o["goals"][0]["about"], "not drawn", "goals and their unknown keys kept: {o:?}");
    assert_eq!(items(&o).len(), 1);
    assert!(o.get("hidden").is_none());
    // Closed (file gone, closed copy beside it): written on from the copy, and the new item brings the list back
    // (main's review 1 Nov: closing never swallows what comes later).
    std::fs::remove_file(isl.join("goals.json")).unwrap();
    let mut closed = kept.clone();
    closed.insert("hidden".into(), Value::Bool(true));
    IslandFile::write_atomic(&closed, &isl.join(".closed-goals.json")).unwrap();
    lab.run(&["plan", "add", "Second"]);
    let o = lab.island("goals");
    assert_eq!((o.get("hidden").and_then(Value::as_bool), o["goals"][0]["id"].as_str()), (None, Some("g-ship")));
    // Moved to Goals from Scheduled with Goals gone: the destination starts from the copy too.
    std::fs::remove_file(isl.join("goals.json")).unwrap();
    lab.run(&["plan", "scheduled", "add", "Later thing"]);
    assert_eq!(lab.run(&["plan", "scheduled", "move", "1", "--to", "goals"]).code, 0);
    assert_eq!(lab.island("goals")["goals"][0]["id"], "g-ship");
    // A Goals file that does not parse is never replaced.
    std::fs::write(isl.join("goals.json"), "{ not json").unwrap();
    let out = lab.run(&["plan", "add", "Third"]);
    assert_eq!(out.code, 2, "{}", out.stdout);
    assert_eq!(std::fs::read_to_string(isl.join("goals.json")).unwrap(), "{ not json");
}

/// Re-review 31 Oct: `islands rm goals` deleted the file and `islands put goals …` recreated it empty. Plan lists are
/// edited with `islands plan` only; `rm --all` and `clear` leave them.
#[test]
fn rm_and_put_refuse_plan_lists_and_clear_keeps_them() {
    let lab = Lab::new("plan-guard");
    lab.run(&["plan", "add", "Approve caches"]);
    lab.run(&["plan", "scheduled", "add", "Later"]);
    lab.run(&["put", "job", "--title", "Build", "--state", "running"]);
    for id in ["goals", "scheduled", "settled"] {
        let out = lab.run(&["rm", id]);
        assert_eq!(out.code, 2, "rm {id}");
        assert!(out.stderr.contains("islands plan"), "{}", out.stderr);
        let out = lab.run(&["put", id, "--title", "Goals"]);
        assert_eq!(out.code, 2, "put {id}");
        assert!(out.stderr.contains("islands plan"), "{}", out.stderr);
    }
    assert_eq!(items(&lab.island("goals")).len(), 1);
    lab.run(&["rm", "--all"]);
    assert!(lab.dir.join("data/islands/goals.json").exists() && !lab.dir.join("data/islands/job.json").exists());
    lab.run(&["put", "job2", "--title", "Build", "--state", "running"]);
    lab.run(&["clear"]);
    assert!(lab.dir.join("data/islands/goals.json").exists() && lab.dir.join("data/islands/scheduled.json").exists());
    assert!(!lab.dir.join("data/islands/job2.json").exists());
}

/// Review 1 Nov of bug 2026-10-31T15-57-16 (from main, 16ac4aea, kept through the Goals rebase): a closed (hidden)
/// plan list keeps its items, and a new item brings it back rather than being swallowed.
#[test]
fn adding_to_a_closed_plan_list_brings_it_back_with_its_items() {
    let lab = Lab::new("closed-list");
    assert_eq!(lab.run(&["plan", "goals", "add", "Keep me"]).code, 0);
    let o = islands::model::PlanList::closed("goals", &lab.island("goals")).expect("a plan list is kept");
    IslandFile::write_atomic(&o, &lab.dir.join("data/islands/goals.json")).unwrap();
    assert_eq!(lab.island("goals")["hidden"], true);
    assert_eq!(lab.run(&["plan", "goals", "add", "New one"]).code, 0);
    let o = lab.island("goals");
    assert!(o.get("hidden").is_none(), "the new item brought the list back: {o:?}");
    let titles: Vec<String> = items(&o).iter().map(|i| i["title"].as_str().unwrap().to_owned()).collect();
    assert_eq!(titles, ["Keep me", "New one"]);
}
