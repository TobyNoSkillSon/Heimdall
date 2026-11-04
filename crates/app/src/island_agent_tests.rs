//! The island agent: when it is woken (a fake clock), what its answer may write, and one call end to end against a
//! fake model (no real call).
use super::*;
use crate::islands::train::{Status, Train};
use std::sync::Mutex;

fn snap(island: &str, stage: &str, state: &str) -> Snapshot {
    Snapshot {
        island: island.into(),
        thread: format!("t-{island}"),
        state: state.into(),
        signals: stages::Signals { run: "r1".into(), started: 0.0, now: 600.0, stage: stage.into(), stage_since: 300.0,
            changes: vec![(0.0, "Starting".into()), (300.0, stage.into())], ..Default::default() },
        raw: Default::default(),
        image: None,
        question: None,
        drawn: false,
    }
}

fn training(loss_n: usize, evals: usize) -> Train {
    Train { run: "decider-v5".into(), status: Some(Status::Live), step: Some(400), total: Some(2126), loss: (0..loss_n.min(48)).map(|i| 0.5 - i as f64 * 0.005).collect(),
        loss_n, last_loss: Some(0.31), tok_s: vec![3100.0, 3080.0, 3120.0], evals: (0..evals).map(|i| (400 * (i as u64 + 1), vec![("suggest_auc".into(), 0.877483)])).collect(), ..Default::default() }
}

#[test]
fn a_stage_change_calls_once_after_the_coalescing_window_and_not_again_within_two_minutes() {
    let mut t = Triggers::default();
    t.observe(snap("a", "Starting", "running"), 0.0);
    assert_eq!(t.next(1000.0), None, "a new island at Starting has nothing to draw");
    t.observe(snap("a", "Reading", "running"), 10.0);
    assert_eq!(t.next(1000.0), None, "Reading is still Preparing: no call");
    t.observe(snap("a", "Testing", "running"), 20.0);
    assert_eq!(t.next(20.0 + COALESCE - 1.0), None, "changes coalesce for 90 s");
    t.observe(snap("a", "Sub-agents", "running"), 60.0);
    assert_eq!(t.next(20.0 + COALESCE), Some(("a".into(), Reason::Stage)), "the first reason's clock, not the latest");
    let s = t.begin("a", 110.0).unwrap();
    assert_eq!(s.signals.stage, "Sub-agents", "the call takes the newest snapshot");
    t.observe(snap("a", "Reviewing", "running"), 120.0);
    assert_eq!(t.next(120.0 + COALESCE), None, "at least two minutes between calls");
    assert_eq!(t.next(110.0 + MIN_GAP), Some(("a".into(), Reason::Stage)));
    assert_eq!(t.wake_at(), Some(110.0 + MIN_GAP));
}

#[test]
fn a_run_end_a_picture_and_an_eval_wake_it_and_loss_is_throttled() {
    let mut t = Triggers::default();
    let mut s = snap("d", "Training", "running");
    s.signals.train = Some(training(10, 0));
    t.observe(s.clone(), 0.0);
    t.begin("d", 100.0).unwrap();
    // Three new loss points: not yet; five: yes, once per ten minutes.
    s.signals.train = Some(training(13, 0));
    t.observe(s.clone(), 200.0);
    assert_eq!(t.next(1000.0), None);
    s.signals.train = Some(training(15, 0));
    t.observe(s.clone(), 300.0);
    assert_eq!(t.next(1000.0), None, "the first call counted as the loss call: ten minutes");
    s.signals.train = Some(training(16, 0));
    t.observe(s.clone(), 100.0 + LOSS_EVERY);
    assert_eq!(t.next(100.0 + LOSS_EVERY + COALESCE), Some(("d".into(), Reason::Loss)));
    t.begin("d", 100.0 + LOSS_EVERY + COALESCE).unwrap();
    // A new eval wakes it at once (after the window).
    let at = 2000.0;
    s.signals.train = Some(training(17, 1));
    t.observe(s.clone(), at);
    assert_eq!(t.next(at + COALESCE), Some(("d".into(), Reason::Eval)));
    t.begin("d", at + COALESCE).unwrap();
    // A new picture.
    s.image = Some("/x/pictures/d-1.png".into());
    t.observe(s.clone(), 3000.0);
    assert_eq!(t.next(3000.0 + COALESCE), Some(("d".into(), Reason::Picture)));
    t.begin("d", 3000.0 + COALESCE).unwrap();
    // A helper finished.
    s.signals.subs_done = 1;
    t.observe(s.clone(), 3500.0);
    assert_eq!(t.next(3500.0 + COALESCE), Some(("d".into(), Reason::Helper)));
    t.begin("d", 3500.0 + COALESCE).unwrap();
    // The run ends while another reason waits: the end wins.
    s.signals.stage = "Testing".into();
    t.observe(s.clone(), 4000.0);
    s.state = "done".into();
    t.observe(s.clone(), 4010.0);
    assert_eq!(t.next(4000.0 + COALESCE), Some(("d".into(), Reason::End)));
}

#[test]
fn the_heartbeat_comes_after_thirty_minutes_only_when_something_changed() {
    let mut t = Triggers::default();
    let mut s = snap("h", "Testing", "running");
    t.observe(s.clone(), 0.0);
    t.begin("h", 100.0).unwrap();
    t.observe(s.clone(), 100.0 + HEARTBEAT + 10.0);
    assert_eq!(t.next(100.0 + HEARTBEAT + 200.0), None, "nothing changed: no heartbeat");
    s.signals.passed = Some(12);
    t.observe(s.clone(), 100.0 + HEARTBEAT - 60.0);
    assert_eq!(t.next(100.0 + HEARTBEAT), None, "changed, but not thirty minutes yet");
    t.observe(s.clone(), 100.0 + HEARTBEAT + 20.0);
    assert_eq!(t.next(100.0 + HEARTBEAT + 20.0 + COALESCE), Some(("h".into(), Reason::Heartbeat)));
    // Deferred (no key, Islands off): looked at again after DEFER.
    t.defer("h", 5000.0);
    assert_eq!(t.next(5000.0 + DEFER - 1.0), None);
    assert_eq!(t.next(5000.0 + DEFER), Some(("h".into(), Reason::Heartbeat)));
}

#[test]
fn the_longest_waiting_island_goes_first() {
    let mut t = Triggers::default();
    t.observe(snap("b", "Testing", "running"), 50.0);
    t.observe(snap("a", "Testing", "running"), 10.0);
    assert_eq!(t.next(500.0).unwrap().0, "a");
}

fn png(dir: &std::path::Path, name: &str) -> String {
    let p = dir.join(name);
    std::fs::write(&p, b"\x89PNG\r\n\x1a\n0000000000").unwrap();
    p.to_string_lossy().into_owned()
}

fn answer(over: Value) -> String {
    let mut a = json!({"title": "Decider V5", "icon": "brain", "state": "running", "stage": "Training", "summary": "Training; step 400 is the new best"});
    for (k, v) in over.as_object().unwrap() { a[k] = v.clone(); }
    a.to_string()
}

#[test]
fn the_signals_carry_the_contracts_fields_and_the_pictures_age() {
    let mut s = snap("d", "Training", "running");
    s.signals.train = Some(training(30, 1));
    s.signals.brief = Some("Train Decider V5 on the new data".into());
    s.signals.recent = vec![(500.0, "said: Step 400 eval is the best so far.".into())];
    s.signals.picture_at = Some(300.0);
    s.image = Some("/x/pictures/d-1.png".into());
    let island = json!({"title": "Decider V5", "sent": "Train Decider V5 on the new data"}).as_object().unwrap().clone();
    let v = signals(&s, &island, Some(&json!({"title": "Decider V5", "icon": "brain"})));
    for k in ["task", "brief", "now", "elapsed_min", "state_hint", "stage", "history", "recent", "helpers", "counters", "metrics", "previous_island", "sent"] {
        assert!(v.get(k).is_some(), "{k} in {v}");
    }
    assert_eq!(v["counters"]["latest_picture"], "/x/pictures/d-1.png");
    assert_eq!(v["counters"]["picture_min_ago"], json!(5.0), "named at 300 s, now 600 s");
    assert_eq!(v["metrics"]["evals_percent"]["suggest_auc"], json!(87.7));
    assert_eq!(v["elapsed_min"], json!(10.0));
    assert!(message(&v).starts_with("<signals>\n{") && message(&v).ends_with("</signals>\n\nDraw the island for this moment."));
    // A failed job's history ends in a red Failed stage; a question is Your answer.
    let mut f = snap("d", "Testing", "failed");
    f.signals.changes.push((500.0, "Failed".into()));
    let h = signals(&f, &island, None)["history"].clone();
    assert_eq!(h.as_array().unwrap().last().unwrap()[0], "Failed");
    let mut q = snap("d", "Testing", "waiting");
    q.question = Some(binding::Question { request: "r".into(), items: vec![binding::Item { id: "q1".into(), header: Some("Dangerous rm".into()), text: "rm -rf /tmp/x".into(),
        multi_select: false, allow_other: false, options: vec![binding::Opt { label: "Allow".into(), description: None }, binding::Opt { label: "Deny".into(), description: None }] }] });
    let v = signals(&q, &island, None);
    assert_eq!((v["stage"].as_str(), v["question"]["prompt"].as_str(), v["question"]["options"][1].as_str()), (Some("Your answer"), Some("rm -rf /tmp/x"), Some("Deny")));
}

#[test]
fn checks_catch_long_lines_banned_counters_invented_numbers_and_unreal_paths() {
    let d = tempfile::tempdir().unwrap();
    let (shot, stray) = (png(d.path(), "shot.png"), png(d.path(), "stray.png"));
    let page = d.path().join("report.html");
    std::fs::write(&page, "<h1>r</h1>").unwrap();
    let mut s = snap("d", "Training", "running");
    s.image = Some(shot.clone());
    s.signals.train = Some(training(30, 1));
    let sig = signals(&s, &Map::new(), None);
    let loss: Vec<f64> = s.signals.train.as_ref().unwrap().loss.clone();
    let c = check(&answer(json!({"parts": [{"type": "line", "points": loss, "unit": "loss"}, {"type": "bars", "unit": "%", "max": 100,
        "items": [{"label": "Suggest AUC", "value": 87.7}, {"label": "Made up", "value": 55}, {"label": "Steps", "value": 2126}]}]})), &sig, &s);
    assert!(c.failures.is_empty(), "{:?}", c.failures);
    assert_eq!(c.parts.len(), 2);
    assert_eq!(c.parts[1]["items"].as_array().unwrap().len(), 2, "an invented bar is left out");
    assert_eq!(c.summary.as_deref(), Some("Training; step 400 is the new best"));
    // Over-long lines and banned counters are failures (the repair turn), and are never written.
    let c = check(&answer(json!({"title": "The Decider V5 training run", "stage": "Running the e2e check", "summary": "Training the decider and checking each eval as it lands",
        "parts": [{"type": "stat", "value": "7", "unit": "sources read"}, {"type": "stat", "value": "0", "unit": "tests passed"}, {"type": "stat", "value": "12", "unit": "files changed"}]})), &sig, &s);
    let f = c.failures.join(" | ");
    for want in ["title", "stage", "summary 55 characters", "sources read", "a counter of 0", "files changed"] { assert!(f.contains(want), "{want} in {f}") }
    assert!(c.parts.is_empty() && c.summary.is_none() && c.title.is_none());
    // Pictures: only the island's stored latest picture; open targets must exist.
    let c = check(&answer(json!({"parts": [{"type": "image", "path": stray}, {"type": "image", "path": shot, "open": shot}], "open": page.to_string_lossy()})), &sig, &s);
    assert_eq!(c.parts, vec![json!({"type": "image", "path": shot, "open": shot, OWN_PART: true})]);
    assert!(c.failures.iter().any(|f| f.contains("not the latest picture")));
    assert_eq!(c.open.as_deref(), Some(page.to_str().unwrap()));
    assert!(check(&answer(json!({"open": "/nowhere/x.html"})), &sig, &s).failures.iter().any(|f| f.contains("does not exist")));
    // Text and buttons are the binding's; more than two graphics is a failure; a fenced answer still parses.
    let c = check(&format!("```json\n{}\n```", answer(json!({"parts": [{"type": "text", "value": "rm -rf x"}, {"type": "button", "label": "Allow", "action": "allow"}]}))), &sig, &s);
    assert!(c.parts.is_empty() && c.failures.is_empty());
    assert_eq!(check("not json", &sig, &s).failures, vec!["not one JSON object".to_string()]);
    // State: only a stop or an ask the code cannot see, over a run it reads as done.
    assert_eq!(check(&answer(json!({"state": "done"})), &sig, &s).state, None);
    let mut done = s.clone();
    done.state = "done".into();
    assert_eq!(check(&answer(json!({"state": "failed", "summary": "Stopped at the usage limit"})), &sig, &done).state.as_deref(), Some("failed"));
    // Helpers: names only, four at most.
    let c = check(&answer(json!({"helpers": [{"title": "Fresh review", "state": "now"}, {"title": "Curate models", "state": "done"}, {"title": "a b c d e", "state": "now"}]})), &sig, &s);
    assert_eq!(c.helpers.unwrap().len(), 2);
}

#[test]
fn applying_writes_the_checked_island_and_hands_the_now_line_to_the_agent() {
    let d = tempfile::tempdir().unwrap();
    let root = Root(d.path().to_path_buf());
    root.put("d", json!({"title": "Task", "state": "running", "summary": "Code's first sentence", "thread": {"id": "t", "title": "Task"}, "parts": [
        {"type": "text", "value": "Which?", "t3q": true},
        {"type": "bars", "items": [], "refresher": true},
        {"type": "stat", "value": "1", OWN_PART: true},
        {"type": "band", "code": true, "segments": []}]}).as_object().unwrap()).unwrap();
    let c = Checked { title: Some("Decider V5".into()), icon: Some("brain".into()), summary: Some("Training; step 400 is the new best".into()),
        parts: vec![json!({"type": "line", "points": [1.0, 0.5], "unit": "loss", OWN_PART: true})], helpers: Some(vec![json!({"title": "Fresh review", "state": "now"})]), ..Default::default() };
    assert!(apply(&root, "d", "run-1", "Training", &c).unwrap());
    let o = root.read("d").unwrap();
    let types: Vec<&str> = o["parts"].as_array().unwrap().iter().map(|p| p["type"].as_str().unwrap()).collect();
    assert_eq!(types, ["line", "text", "band"], "its graphic first, the question, the band last; the refresher's and its old part go");
    assert_eq!((o["title"].as_str(), o["thread"]["title"].as_str(), o["icon"].as_str()), (Some("Decider V5"), Some("Decider V5"), Some("brain")));
    assert_eq!((o["summary"].as_str(), o["summaryRun"].as_str(), o["summaryStage"].as_str()), (Some("Training; step 400 is the new best"), Some("run-1"), Some("Training")));
    assert_eq!(o["helpers"][0]["title"], "Fresh review");
    assert_eq!(o["state"], "running", "state is code's");
    assert!(!apply(&root, "d", "run-1", "Training", &c).unwrap(), "unchanged: not rewritten");
    // A failed run code reads as done: the agent's reading stands.
    root.patch("d", |o| { o.insert("state".into(), json!("done")); }).unwrap();
    apply(&root, "d", "run-1", "end", &Checked { state: Some("failed".into()), ..c.clone() }).unwrap();
    assert_eq!(root.read("d").unwrap()["state"], "failed");
    assert!(!apply(&root, "gone", "run-1", "Training", &c).unwrap(), "a removed island stays removed");
}

struct FakeModel { answers: Mutex<Vec<String>>, messages: Mutex<Vec<String>> }
#[async_trait::async_trait]
impl Model for FakeModel {
    async fn ask(&self, _: &crate::plan_refresher::Endpoint, message: String) -> Result<String> {
        self.messages.lock().unwrap().push(message);
        let mut a = self.answers.lock().unwrap();
        Ok(if a.len() > 1 { a.remove(0) } else { a[0].clone() })
    }
}

fn lab_cfg(root: &Root, model: Arc<FakeModel>, on: Arc<std::sync::atomic::AtomicBool>, key: Arc<Mutex<Option<String>>>) -> Config {
    Config {
        root: root.clone(),
        endpoint: Arc::new(move || crate::plan_refresher::Endpoint { key: key.lock().unwrap().clone(), ..Default::default() }),
        enabled: Arc::new(move || on.load(std::sync::atomic::Ordering::Acquire)),
        model,
    }
}

#[tokio::test]
async fn one_call_carries_the_signals_a_failed_check_gets_one_repair_turn_and_nothing_runs_without_a_key_or_while_off() {
    let d = tempfile::tempdir().unwrap();
    let root = Root(d.path().to_path_buf());
    root.put("d", json!({"title": "Task", "state": "running", "stage": "Training", "thread": {"id": "t-d"}}).as_object().unwrap()).unwrap();
    let mut s = snap("d", "Training", "running");
    s.signals.train = Some(training(20, 1));
    let loss = s.signals.train.as_ref().unwrap().loss.clone();
    let long = answer(json!({"summary": "Training the decider and checking each eval as it lands", "parts": [{"type": "stat", "value": "7", "unit": "sources read"}]}));
    let good = answer(json!({"parts": [{"type": "line", "points": loss, "unit": "loss"}]}));
    let model = Arc::new(FakeModel { answers: Mutex::new(vec![long.clone(), good.clone()]), messages: Mutex::new(vec![]) });
    let (on, key) = (Arc::new(std::sync::atomic::AtomicBool::new(false)), Arc::new(Mutex::new(None::<String>)));
    let cfg = lab_cfg(&root, model.clone(), on.clone(), key.clone());
    let mut t = Triggers::default();
    t.observe(s, 0.0);
    // Islands off, then no credential: no call, looked at later.
    assert_eq!(run_one(&cfg, &mut t, "d", Reason::Stage, 100.0).await.unwrap(), None);
    on.store(true, std::sync::atomic::Ordering::Release);
    assert_eq!(run_one(&cfg, &mut t, "d", Reason::Stage, 100.0).await.unwrap(), None);
    assert!(model.messages.lock().unwrap().is_empty());
    *key.lock().unwrap() = Some("k".into());
    assert_eq!(run_one(&cfg, &mut t, "d", Reason::Eval, 1000.0).await.unwrap(), Some(true));
    let msgs = model.messages.lock().unwrap().clone();
    assert_eq!(msgs.len(), 2, "one call and one repair turn, never more");
    assert!(msgs[0].starts_with("<signals>") && msgs[0].contains("\"stage\": \"Training\""));
    assert!(msgs[1].starts_with(&msgs[0]) && msgs[1].contains(&long) && msgs[1].contains("summary 55 characters") && msgs[1].contains("sources read"),
        "the repair carries the whole input, the answer and the failures");
    let o = root.read("d").unwrap();
    assert_eq!((o["parts"][0]["type"].as_str(), o["title"].as_str(), o["icon"].as_str()), (Some("line"), Some("Decider V5"), Some("brain")));
    // Still failing after the repair: what fails is not written, and no third call.
    model.messages.lock().unwrap().clear();
    *model.answers.lock().unwrap() = vec![long.clone()];
    let mut s2 = snap("d", "Reviewing", "running");
    s2.signals.train = Some(training(20, 1));
    t.observe(s2, 2000.0);
    run_one(&cfg, &mut t, "d", Reason::Stage, 2200.0).await.unwrap();
    assert_eq!(model.messages.lock().unwrap().len(), 2);
    let o = root.read("d").unwrap();
    assert_ne!(o["summary"].as_str(), Some("Training the decider and checking each eval as it lands"));
    assert!(o["parts"].as_array().is_none_or(|p| p.iter().all(|x| x["unit"] != "sources read")), "a banned counter is never written");
    assert!(model.messages.lock().unwrap()[0].contains("\"previous_island\""), "the next call sees what it drew");
}

#[test]
fn the_schema_is_the_prompts_output_contract() {
    let s = schema();
    assert_eq!(s["required"], json!(["title", "icon", "state", "stage", "summary"]));
    for k in ["parts", "helpers", "open", "progress", "sent"] { assert!(s["properties"].get(k).is_some(), "{k}") }
    let text = s.to_string();
    for banned in ["minLength", "maxLength", "minItems", "maxItems", "minimum", "maximum"] { assert!(!text.contains(banned), "{banned}") }
}

/// The final instructions are copied as they are; the public copy carries nothing of his.
#[test]
fn the_prompt_is_the_final_agent_prompt_and_its_reference_islands_parse() {
    let public = include_str!("../prompt/public/island-agent.md");
    for private in ["Vault", "Arsenal", "Allegro", "iPad", "zł", "night pass", "B-29", "DDColor", "TrainingLab"] {
        assert!(!public.contains(private), "public island agent prompt carries {private:?}");
    }
    for text in [SYSTEM, public] {
        let islands: Vec<Map<String, Value>> = text.split("```json").skip(1).filter_map(|b| parse(b.split("```").next().unwrap()).ok()).filter(|o| o.contains_key("summary")).collect();
        assert!(islands.len() >= 6, "{}", islands.len());
        for o in islands {
            for k in ["title", "icon", "state", "stage", "summary"] { assert!(o.contains_key(k), "{k} in {o:?}") }
            assert!(o["summary"].as_str().unwrap().chars().count() <= SUMMARY_CHARS, "{o:?}");
        }
    }
}

#[test]
fn numbers_read_thousands_and_json_arrays_alike() {
    assert_eq!(numbers("[0.3269,0.3294] 3,100 tok/s, 83% and 1.5."), vec![0.3269, 0.3294, 3100.0, 83.0, 1.5]);
}

/// Review, 30 Oct: the now line's numbers must be in the signals too; the banned list is the prompt's counter phrases
/// and applies only to parts that are written (a "Commit the fix" row, or a question's text, is no counter).
#[test]
fn the_now_lines_numbers_are_checked_and_only_written_counters_are_banned() {
    let mut s = snap("d", "Training", "running");
    s.signals.train = Some(training(30, 1));
    let sig = signals(&s, &Map::new(), None);
    let c = check(&answer(json!({"summary": "Training; step 900 is the new best"})), &sig, &s);
    assert!(c.summary.is_none() && c.failures.iter().any(|f| f.contains("900")), "{:?}", c.failures);
    assert!(check(&answer(json!({"summary": "Training; step 400 is the new best"})), &sig, &s).failures.is_empty());
    let c = check(&answer(json!({"parts": [{"type": "list", "items": [{"title": "Commit the fix", "value": "next"}]},
        {"type": "text", "value": "3 files changed, 2 commits"}]})), &sig, &s);
    assert!(c.failures.is_empty(), "{:?}", c.failures);
    assert_eq!(c.parts.len(), 1);
    assert!(!check(&answer(json!({"parts": [{"type": "stat", "value": "2", "unit": "commits"}]})), &sig, &s).failures.is_empty());
}

/// Review, 30 Oct: after a restart every island was called once on first sight. A settled one is not; a running one
/// only when it does not already show this moment's line.
#[test]
fn first_sight_after_a_restart_calls_only_running_islands_that_need_it() {
    let mut t = Triggers::default();
    t.observe(snap("done", "Testing", "done"), 0.0);
    t.observe(snap("failed", "Testing", "failed"), 0.0);
    let mut drawn = snap("drawn", "Testing", "running");
    drawn.drawn = true;
    t.observe(drawn, 0.0);
    t.observe(snap("fresh", "Testing", "running"), 0.0);
    assert_eq!(t.next(1000.0), Some(("fresh".into(), Reason::Stage)));
    t.begin("fresh", 1000.0);
    assert_eq!(t.next(5000.0), None, "the settled and the already-drawn islands are not called");
}

/// Review, 30 Oct: the stage word is code's and given to the agent as the island shows it; the checks accept every
/// code stage word (the final prompt is not rewritten).
#[test]
fn every_code_stage_word_is_given_and_accepted() {
    for stage in ["Downloading", "Labelling", "Drafting", "Background", "Needs approval", "Sub-agents", "Your answer"] {
        let mut s = snap("d", "Reading", if stage == "Needs approval" || stage == "Your answer" { "waiting" } else { "running" });
        s.signals.train = None;
        let island = json!({"title": "Task", "stage": stage}).as_object().unwrap().clone();
        let sig = signals(&s, &island, None);
        assert_eq!(sig["stage"], stage, "the island's stage word goes to the agent as it is");
        let c = check(&json!({"title": "Task", "icon": "gearshape.2", "state": "running", "stage": stage, "summary": "Getting the weights ready"}).to_string(), &sig, &s);
        assert!(c.failures.is_empty(), "{stage}: {:?}", c.failures);
    }
}

/// a job island's open target goes to the island viewer on click; the agent may name it with
/// `~/` (expanded) and any kind the viewer shows.
#[test]
fn an_open_target_under_home_is_accepted() {
    let home = std::env::var("HOME").unwrap();
    let d = std::path::PathBuf::from(&home).join(format!(".island-agent-open-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(d.join("report.pdf"), b"%PDF-1.4").unwrap();
    let s = snap("d", "Writing", "running");
    let sig = signals(&s, &Map::new(), None);
    let rel = format!("~/{}/report.pdf", d.file_name().unwrap().to_string_lossy());
    let c = check(&answer(json!({"open": rel})), &sig, &s);
    assert_eq!(c.open.as_deref(), Some(rel.as_str()), "{:?}", c.failures);
    let _ = std::fs::remove_dir_all(&d);
}

/// Review, 30 Oct: a prompt in a job thread must not make one click launch anything. The agent's open target is a page
/// the work served, or an absolute or `~/` path to a folder or a file the viewer shows, never an app, an installer, a
/// script, an executable, an unknown type or a relative path.
#[test]
fn open_targets_that_could_launch_something_are_refused() {
    use std::os::unix::fs::PermissionsExt;
    let d = tempfile::tempdir().unwrap();
    let at = |n: &str| d.path().join(n).to_string_lossy().into_owned();
    std::fs::write(at("report.html"), "<h1>r</h1>").unwrap();
    std::fs::create_dir_all(at("Tool.app")).unwrap();
    std::fs::create_dir_all(at("results")).unwrap();
    for n in ["setup.pkg", "run.command", "do.sh", "noext", "go.scpt", "page.webloc"] { std::fs::write(at(n), "x").unwrap() }
    std::fs::write(at("tool.png"), "x").unwrap();
    std::fs::set_permissions(at("tool.png"), std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(safe_open_path(&at("report.html")) && safe_open_path(&at("results")));
    for bad in ["Tool.app", "setup.pkg", "run.command", "do.sh", "noext", "go.scpt", "page.webloc", "tool.png"] {
        assert!(!safe_open_path(&at(bad)), "{bad}");
    }
    assert!(!safe_open_path("report.html"), "relative");
    let s = snap("d", "Writing", "running");
    let sig = signals(&s, &Map::new(), None);
    let c = check(&answer(json!({"open": at("Tool.app")})), &sig, &s);
    assert!(c.open.is_none() && !c.failures.is_empty());
    assert!(check(&answer(json!({"open": "x-apple.systempreferences:com.apple.preference.security"})), &sig, &s).open.is_none());
}

/// Bug note 2026-11-01T05-50-57: a job island says in one line what the agent was sent to do, set once, apart from the
/// changing now line. An island without it is asked for it (from the task and brief) and gets it once; one that has it
/// is never asked and never rewritten; the line is held to one plain errand.
#[test]
fn an_island_without_its_errand_gets_one_sent_line_once_and_keeps_it() {
    let d = tempfile::tempdir().unwrap();
    let root = Root(d.path().to_path_buf());
    root.put("d", json!({"title": "Decider V5", "state": "running", "thread": {"id": "t", "title": "Decider V5"}}).as_object().unwrap()).unwrap();
    let mut s = snap("d", "Training", "running");
    s.signals.brief = Some("Train Decider V5 on the new data and report its test score".into());
    let v = signals(&s, &root.read("d").unwrap(), None);
    assert_eq!(v["sent"], "");
    assert!(message(&v).ends_with(SENT_ASK), "asked while there is none");
    for (bad, why) in [("Training the decider on the new data", "status"), ("Train", "one word"), ("Train it from /Users/x/data", "path"),
                       (&"Train the decider on the new data ".repeat(5)[..], "characters"), ("Train Decider V5 on 900 new rows", "not in the signals")] {
        let c = check(&answer(json!({"sent": bad, "summary": "Training the decider"})), &v, &s);
        assert!(c.sent.is_none() && c.failures.iter().any(|f| f.contains(why)), "{bad}: {:?}", c.failures);
    }
    let c = check(&answer(json!({"sent": "Train Decider V5 on the new data", "summary": "Training the decider"})), &v, &s);
    assert!(c.failures.is_empty(), "{:?}", c.failures);
    assert_eq!(c.sent.as_deref(), Some("Train Decider V5 on the new data"));
    assert!(apply(&root, "d", "run-1", "Training", &c).unwrap());
    let o = root.read("d").unwrap();
    assert_eq!((o["sent"].as_str(), o["summary"].as_str()), (Some("Train Decider V5 on the new data"), Some("Training the decider")), "apart from the now line");
    // Once it has one: not asked, an answer's other line is ignored, and nothing rewrites it.
    let v = signals(&s, &o, None);
    assert_eq!(v["sent"], "Train Decider V5 on the new data");
    assert!(!message(&v).contains("`sent`"));
    let c = check(&answer(json!({"sent": "Report the test score", "summary": "Training the decider"})), &v, &s);
    assert!(c.sent.is_none() && c.failures.is_empty());
    apply(&root, "d", "run-1", "Testing", &Checked { sent: Some("Report the test score".into()), ..c }).unwrap();
    assert_eq!(root.read("d").unwrap()["sent"], "Train Decider V5 on the new data", "written once");
}

/// The HTML body (3 Nov): a climb's status file reaches the signals, its numbers carry the HTML, and the app's own
/// gate refuses what it would not draw.
#[test]
fn a_status_file_reaches_the_signals_and_wakes_the_island() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("island-status.json");
    std::fs::write(&path, json!({"kind": "climb", "progress": {"done": 11, "total": 16, "unit": "rounds"}, "target": {"value": 8.5},
        "series": [{"name": "Opus", "points": [5.79, 6.38]}], "values": [{"label": "mean, round 11", "value": 6.09}]}).to_string()).unwrap();
    let text = format!("$ mv /tmp/s.json {}", path.display());
    assert_eq!(stages::status_in(&text).as_deref(), Some(path.to_str().unwrap()));
    assert_eq!(stages::status_in("cat notes/island-status.json"), None, "only an absolute path");
    let mut s = snap("c", "Reviewing", "running");
    s.signals.status = Some(path.to_string_lossy().into_owned());
    let sig = signals(&s, &Map::new(), None);
    assert_eq!(sig["status"]["values"][0]["value"], 6.09);
    let mut t = Triggers::default();
    t.observe(s.clone(), 0.0);
    t.begin("c", 100.0).unwrap();
    std::fs::write(&path, json!({"kind": "climb", "values": [{"label": "mean, round 12", "value": 6.4}]}).to_string()).unwrap();
    t.observe(s.clone(), 300.0);
    assert_eq!(t.next(300.0 + COALESCE), Some(("c".into(), Reason::Status)), "a rewritten status file is news");
    std::fs::write(&path, "x".repeat(STATUS_BYTES as usize + 1)).unwrap();
    assert!(signals(&s, &Map::new(), None).get("status").is_none(), "an oversized status file is left out");
}

#[test]
fn html_is_checked_by_the_apps_gate_and_its_numbers_by_the_signals() {
    let mut s = snap("c", "Reviewing", "running");
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("island-status.json");
    std::fs::write(&path, json!({"progress": {"done": 5.2, "total": 17.96, "unit": "GB"}, "series": [{"name": "Opus", "points": [5.79, 6.38]}],
        "target": {"value": 8.5}, "values": [{"label": "mean, round 11", "value": 6.09}, {"label": "rate", "value": 4.8, "unit": "MB/s"}]}).to_string()).unwrap();
    s.signals.status = Some(path.to_string_lossy().into_owned());
    let sig = signals(&s, &Map::new(), None);
    let ok = r#"<div class="row"><span class="big">6.09</span><span class="unit">mean, round 11</span></div><x-line series="5.79 6.38" target="8.5" ends="1"></x-line><x-ring done="5.2" total="17.96" label="29%"></x-ring>"#;
    let c = check(&answer(json!({"summary": "Judging the next README round", "html": ok, "parts": [{"type": "stat", "value": "6.09", "unit": "mean"}]})), &sig, &s);
    assert!(c.failures.is_empty(), "{:?}", c.failures);
    assert_eq!(c.html.as_deref(), Some(ok));
    for (bad, why) in [
        (r#"<span class="big">743.2</span>"#, "not in the signals"),
        (r#"<x-line series="5.79 777.7"></x-line>"#, "not in the signals"),
        (r#"<p>6.09</p><script>alert(1)</script>"#, "do not draw"),
        (r#"<img src="https://example.com/a.png">"#, "do not draw"),
        (&"<br>".repeat(700), "refused"),
        ("<x-ring\n value=\"0.93\" label=\"93%\"></x-ring>", "not in the signals"),
        (r#"<div class="bar"><i style="width:62%"></i></div>"#, "not in the signals"),
        (r#"<span>6.09</span> <span>117 jobs left</span>"#, "bookkeeping"),
        (r#"<img src="/etc/hosts.png">"#, "do not draw"),
    ] {
        let c = check(&answer(json!({"summary": "Judging the next README round", "html": bad})), &sig, &s);
        assert!(c.html.is_none() && c.failures.iter().any(|f| f.contains(why)), "{bad}: {:?}", c.failures);
    }
}

#[test]
fn html_is_written_first_with_the_parts_as_its_fallback() {
    let d = tempfile::tempdir().unwrap();
    let root = Root(d.path().to_path_buf());
    root.put("c", json!({"title": "Climb", "state": "running", "parts": [{"type": "band", "code": true, "segments": []}]}).as_object().unwrap()).unwrap();
    let c = Checked { html: Some("<p>6.09</p>".into()), parts: vec![json!({"type": "stat", "value": "6.09", OWN_PART: true})], ..Default::default() };
    assert!(apply(&root, "c", "run-1", "Reviewing", &c).unwrap());
    let o = root.read("c").unwrap();
    assert_eq!(o["parts"][0]["type"], "html");
    assert_eq!(o["parts"][0]["fallback"][0]["value"], "6.09");
    assert_eq!(o["parts"][1]["type"], "band");
    // The next answer without HTML replaces it (it is this job's own part).
    let plain = Checked { parts: vec![json!({"type": "stat", "value": "7", OWN_PART: true})], ..Default::default() };
    apply(&root, "c", "run-1", "Reviewing", &plain).unwrap();
    let types: Vec<String> = root.read("c").unwrap()["parts"].as_array().unwrap().iter().map(|p| p["type"].as_str().unwrap().to_owned()).collect();
    assert_eq!(types, ["stat", "band"]);
}

#[test]
fn the_html_addendum_follows_the_frozen_prompt_and_its_examples_pass_the_gate() {
    assert!(SYSTEM.starts_with(include_str!("../prompt/island-agent.md")));
    for text in [include_str!("../prompt/island-agent-html.md"), include_str!("../prompt/public/island-agent-html.md")] {
        let bodies: Vec<&str> = text.split("```html").skip(1).map(|b| b.split("```").next().unwrap()).collect();
        assert_eq!(bodies.len(), 4);
        for b in bodies {
            let c = island_html::sanitize(b, &|_| true).unwrap();
            assert!(c.stripped.is_empty(), "{:?}", c.stripped);
        }
    }
    let public = include_str!("../prompt/public/island-agent-html.md");
    for private in ["Vault", "Arsenal"] { assert!(!public.contains(private), "{private}") }
}
