//! Lane R, bug note 2026-11-11T12-05-15: a job's report outlives its island.

use super::*;
use crate::islands::binding::{sync_one, task_island, Question, Pick, Retire, ThreadSource, ThreadView, DONE_SHOWN_FOR};
use crate::islands::files::Root;
use std::sync::Mutex;

fn obj(v: Value) -> Map<String, Value> { v.as_object().unwrap().clone() }

fn host() -> (tempfile::TempDir, crate::islands::Host) {
    let d = tempfile::tempdir().unwrap();
    let h = crate::islands::Host::new(Root(d.path().join("fi")), None);
    (d, h)
}

/// A file that exists, like a job's report.
fn report(d: &tempfile::TempDir, name: &str) -> String {
    let p = d.path().join(name);
    std::fs::write(&p, "<h1>r</h1>").unwrap();
    p.to_string_lossy().into_owned()
}

fn island_of(h: &crate::islands::Host, thread: &str) -> String {
    h.bindings.all().into_iter().find(|(_, b)| b.thread == thread).expect("a job island").0
}

struct Fake(Mutex<ThreadView>);
#[async_trait::async_trait]
impl ThreadSource for Fake {
    async fn view(&self, _: &str) -> anyhow::Result<ThreadView> { Ok(self.0.lock().unwrap().clone()) }
    async fn answer(&self, _: &str, _: &Question, _: &[Pick]) -> anyhow::Result<()> { Ok(()) }
}

fn settled_rows(root: &Root) -> Vec<Value> {
    crate::goals::ops::current(root, crate::goals::settled::ID).and_then(|o| o.get("items").and_then(Value::as_array).cloned()).unwrap_or_default()
}

#[test]
fn a_final_reply_names_its_page_first_then_any_deliverable() {
    let home = std::env::var("HOME").unwrap();
    assert_eq!(named_in("Done. Notes in /tmp/a/notes.md; the report is /tmp/a/report.html.").as_deref(), Some("/tmp/a/report.html"));
    assert_eq!(named_in("Rendered `/tmp/shot.png` and wrote /tmp/x.rs").as_deref(), Some("/tmp/shot.png"));
    assert_eq!(named_in("Page: file:///tmp/My%20Page.html").as_deref(), Some("/tmp/My Page.html"));
    assert_eq!(named_in("Written to ~/Work/Assistant/Research/jarvis.html.").as_deref(), Some(format!("{home}/Work/Assistant/Research/jarvis.html").as_str()));
    // Code, logs and folders are not deliverables; a URL is not a file here.
    assert_eq!(named_in("Fixed crates/app/src/x.rs and /tmp/x.rs; log /tmp/run.log; see https://a.b/c.html; folder /tmp/out/"), None);
    // Night 1112 fresh review: a bug note, a skill or a repo doc the reply cites is not what the job made.
    assert_eq!(named_in("Closed /Users/t/Work/Projects/Heimdall/bugs/notes/2026-11-11T12-05-15.md; see /Users/t/Work/System/skills/job-search/SKILL.md and /Users/t/Work/Projects/JobBoard/docs/links.md"), None);
    assert_eq!(named_in("Read /Users/t/Work/Projects/Heimdall/AGENTS.md, wrote /Users/t/Work/Assistant/Research/x.md").as_deref(), Some("/Users/t/Work/Assistant/Research/x.md"));
}

#[test]
fn only_a_file_that_exists_is_a_deliverable() {
    let d = tempfile::tempdir().unwrap();
    let r = report(&d, "r.html");
    assert_eq!(file_of(&r).as_deref(), Some(r.as_str()));
    assert_eq!(file_of(&format!("file://{r}")).as_deref(), Some(r.as_str()));
    assert_eq!(file_of(&d.path().to_string_lossy()), None, "a folder");
    assert_eq!(file_of("/tmp/never-written-lane-r.html"), None);
    assert_eq!(file_of("https://example.com/r.html"), None);
    let pic = report(&d, "p.png");
    assert_eq!(deliverable(&obj(json!({"open": "/tmp/never-written-lane-r.html", "image": pic}))).as_deref(), Some(pic.as_str()), "the picture when the open target is missing");
}

/// The self-close path (lane S): the task's thread is archived once its result is delivered, and the next binding pass
/// ends its island. Before: the island and the way to its report vanished. Now: a done island with a file turns into
/// its report island in place, unbound, and Settled gets the file.
#[tokio::test]
async fn a_self_closed_job_leaves_its_report_on_screen_and_in_settled() {
    let (d, h) = host();
    let page = report(&d, "jarvis.html");
    task_island(&h, "th", "What people want from a Jarvis", &format!("Write the report to {page}")).unwrap();
    let id = &island_of(&h, "th");
    h.root.put(id, &obj(json!({"icon": "person.3", "steps": [{"title": "Read", "state": "done"}], "kv": [{"label": "Wishes", "value": "514"}]}))).unwrap();
    let fake = Fake(Mutex::new(ThreadView { status: "completed".into(), archived: true, ..Default::default() }));
    assert!(sync_one(&h.root, &h.bindings, &fake, id).await.unwrap());
    let o = h.root.read(id).expect("the island stays as the report's");
    assert_eq!(o["preset"], "result");
    assert_eq!(o["open"], json!(page));
    assert_eq!(o["icon"], "person.3");
    assert_eq!(o["expanded"], false);
    assert_eq!(o["report"]["thread"], "th");
    for k in ["thread", "state", "stage", "steps", "kv", "parts"] { assert!(o.get(k).is_none(), "{k} goes with the job") }
    assert!(h.bindings.get(id).is_none(), "unbound: no thread pass touches it again");
    crate::islands::binding::adopt(&h.root, &h.bindings);
    assert!(h.bindings.get(id).is_none(), "never adopted back");
    let rows = settled_rows(&h.root);
    assert_eq!(rows.len(), 1);
    assert_eq!((rows[0]["proof"]["kind"].as_str(), rows[0]["proof"]["value"].as_str()), (Some("file"), Some(page.as_str())));
    // Another pass changes nothing (the report island is nobody's job).
    assert!(!sync_one(&h.root, &h.bindings, &fake, id).await.unwrap());
    assert_eq!(h.root.read(id).unwrap()["preset"], "result");
}

/// The island agent may never get to set the open target before the self-close archives the thread: the file the
/// final reply named is the one kept.
#[tokio::test]
async fn the_file_the_final_reply_names_is_kept_when_nothing_set_one() {
    let (d, h) = host();
    let page = report(&d, "life-story.html");
    task_island(&h, "th", "Life story page", "").unwrap();
    let id = &island_of(&h, "th");
    let mut r = crate::islands::stages::Reducer::new("run-1", 1000.0);
    r.item(&json!({"position": 1, "runId": "run-1", "type": "assistant_message", "text": format!("Built the page with music. It is at {page}."), "updatedAt": "2026-11-11T14:40:45Z"}));
    let fake = Fake(Mutex::new(ThreadView { status: "completed".into(), archived: true, run: "run-1".into(), signals: Some(r.signals(2000.0)), ..Default::default() }));
    sync_one(&h.root, &h.bindings, &fake, id).await.unwrap();
    assert_eq!(h.root.read(id).unwrap()["open"], json!(page));
}

#[tokio::test]
async fn a_job_with_no_deliverable_goes_as_before() {
    let (_d, h) = host();
    task_island(&h, "th", "Empik job pay", "").unwrap();
    let id = &island_of(&h, "th");
    let fake = Fake(Mutex::new(ThreadView { status: "completed".into(), archived: true, ..Default::default() }));
    assert!(sync_one(&h.root, &h.bindings, &fake, id).await.unwrap());
    assert!(h.root.read(id).is_none());
    assert!(settled_rows(&h.root).is_empty());
}

/// The other way a done island leaves (two minutes seen, `Retire`) keeps its report the same way; one whose file
/// The user already opened from it goes (he read it; the viewer is its own island and stays).
#[test]
fn a_retired_done_island_keeps_its_report_unless_owner_opened_it() {
    let (d, h) = host();
    let (a, b) = (report(&d, "a.html"), report(&d, "b.pdf"));
    h.root.put("task-a", &obj(json!({"title": "Bench", "state": "done", "open": a, "thread": "th-a"}))).unwrap();
    h.root.put("task-b", &obj(json!({"title": "Costs", "state": "done", "open": b, "thread": "th-b"}))).unwrap();
    h.bindings.bind("task-a", "th-a");
    h.bindings.bind("task-b", "th-b");
    // The user opened task-b's file from it after it finished.
    opened(&h.root, "task-b").unwrap();
    assert_eq!(h.root.read("task-b").unwrap()["opened"], true);
    let state = json!({"islands": {"task-a": {"open": false}, "task-b": {"open": false}}});
    crate::islands::files::write_atomic(&h.root.state(), &state).unwrap();
    let gone = Retire::default().tick(&h.root, &h.bindings, || false, DONE_SHOWN_FOR);
    assert_eq!(gone.len(), 2, "both leave as job islands: {gone:?}");
    assert_eq!(h.root.read("task-a").unwrap()["preset"], "result");
    assert!(h.root.read("task-b").is_none(), "opened already: no report island");
    assert_eq!(settled_rows(&h.root).len(), 2, "both files are in Settled");
}

#[test]
fn a_report_island_goes_once_opened_and_a_running_island_does_not_remember_an_open() {
    let (d, h) = host();
    let a = report(&d, "a.html");
    h.root.put("task-a", &obj(json!({"preset": "result", "title": "Bench", "open": a, "report": {"thread": "th", "at": 1}}))).unwrap();
    opened(&h.root, "task-a").unwrap();
    assert!(h.root.read("task-a").is_none());
    // A draft opened while the job still runs is not the final report.
    h.root.put("task-r", &obj(json!({"title": "R", "state": "running", "open": a}))).unwrap();
    opened(&h.root, "task-r").unwrap();
    assert!(h.root.read("task-r").unwrap().get("opened").is_none());
}

#[test]
fn at_most_three_report_islands_stay_and_a_job_coming_back_takes_its_own() {
    let (d, h) = host();
    let f = report(&d, "r.html");
    for (k, id) in ["r1", "r2", "r3", "r4"].iter().enumerate() {
        let o = obj(json!({"title": format!("Job {k}"), "state": "done", "open": f}));
        h.root.locked(|| leave_locked(&h.root, id, &o, &format!("th-{id}"), 100 + k as i64)).unwrap();
        settled(&h.root, &o, &f, 100 + k as i64);
    }
    let left: Vec<String> = h.root.all().into_iter().filter(|(_, o)| is_report(o)).map(|(id, _)| id).collect();
    assert_eq!(left, ["r2", "r3", "r4"], "the oldest went");
    job_back(&h.root, "th-r3");
    assert!(h.root.read("r3").is_none());
    assert!(h.root.read("r2").is_some());
}

/// Settled holds today's closed jobs' reports: as the proof of the Goals job's row for the same work (written first
/// or after), never twice.
#[test]
fn settled_gets_the_file_once_on_the_goals_row_for_the_same_work() {
    let (d, h) = host();
    let f = report(&d, "jarvis.html");
    let now = 1_794_409_116;
    // The Goals job wrote its row first, without proof (11 Nov 16:38: "Jarvis wishes report: 514 wishes, 17 use cases").
    let mut list = crate::goals::settled::fresh();
    list["items"] = json!([{"id": "s-1", "title": "Jarvis wishes report: 514 wishes, 17 use cases", "done": true, "at": now + 9, "kind": "shipped", "goal": "g-1", "by": "goals"}]);
    crate::goals::ops::write_list(&h.root, "settled", &list).unwrap();
    settle(&h.root, "Jarvis wishes", "", &f, now).unwrap();
    settle(&h.root, "Jarvis wishes", "", &f, now + 60).unwrap();
    let rows = settled_rows(&h.root);
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["proof"]["value"], json!(f));
    assert_eq!(rows[0]["goal"], "g-1");
    // Unrelated work an hour away is its own row.
    let g = report(&d, "costs.html");
    settle(&h.root, "Cloud costs", "", &g, now + 3600).unwrap();
    assert_eq!(settled_rows(&h.root).len(), 2);
}

#[test]
fn the_goals_row_written_after_takes_over_the_jobs_row() {
    let (d, h) = host();
    let f = report(&d, "life.html");
    let now = 1_794_405_645;
    settle(&h.root, "Life story page", "", &f, now).unwrap();
    let mut file = crate::goals::ops::current(&h.root, "settled").unwrap();
    let e = crate::goals::settled::Entry { title: "Life story animated page built, with music".into(), kind: "shipped", goal: "g-heimdall".into(),
        task: "t-9".into(), at: now + 30, quote: String::new(), proof: None };
    assert_eq!(crate::goals::settled::add(&mut file, &[e]), [""]);
    let rows = file["items"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["title"], "Life story animated page built, with music");
    assert_eq!((rows[0]["goal"].as_str(), rows[0]["by"].as_str()), (Some("g-heimdall"), Some("goals")));
    assert_eq!(rows[0]["proof"]["value"], json!(f));
}

#[test]
fn a_settled_row_owner_removed_is_not_written_back() {
    let (d, h) = host();
    let f = report(&d, "x.html");
    let mut list = crate::goals::settled::fresh();
    crate::goals::settled::remember_removed(&mut list, "Jeżyce ad poster", "", 1_794_400_000);
    crate::goals::ops::write_list(&h.root, "settled", &list).unwrap();
    settle(&h.root, "Jeżyce ad poster", "", &f, 1_794_400_100).unwrap();
    assert!(settled_rows(&h.root).is_empty());
}

/// Review: a Goals row that already carries a proof (a commit) still gets the report, its proof kept beside it; an
/// unrelated row sharing only a project name does not.
#[test]
fn a_proved_goals_row_gets_the_report_and_unrelated_work_is_not_joined() {
    let (d, h) = host();
    let f = report(&d, "latency.html");
    let now = 1_794_390_852;
    let mut list = crate::goals::settled::fresh();
    list["items"] = json!([
        {"id": "s-1", "title": "Heimdall menu revamp merged", "at": now - 60, "kind": "shipped", "goal": "g", "by": "goals", "proof": {"kind": "commit", "value": "abc"}},
        {"id": "s-2", "title": "Heimdall latency report: first word in 1.2 s", "at": now + 20, "kind": "shipped", "goal": "g", "by": "goals", "proof": {"kind": "commit", "value": "def"}}]);
    crate::goals::ops::write_list(&h.root, "settled", &list).unwrap();
    settle(&h.root, "Heimdall latency", "", &f, now).unwrap();
    let rows = settled_rows(&h.root);
    assert_eq!(rows.len(), 2, "{rows:?}");
    let latency = rows.iter().find(|r| r["id"] == "s-2").unwrap();
    assert_eq!((latency["proof"]["value"].as_str(), latency["alsoProof"]["value"].as_str()), (Some(f.as_str()), Some("def")));
    assert_eq!(rows.iter().find(|r| r["id"] == "s-1").unwrap()["proof"]["kind"], "commit");
}

#[test]
fn a_web_address_or_an_existing_open_target_is_never_replaced() {
    let d = tempfile::tempdir().unwrap();
    let r = report(&d, "r.html");
    assert!(open_free(&obj(json!({}))));
    assert!(open_free(&obj(json!({"open": "/tmp/never-written-lane-r.html"}))));
    assert!(!open_free(&obj(json!({"open": "http://localhost:5173/"}))));
    assert!(!open_free(&obj(json!({"open": r}))));
}
