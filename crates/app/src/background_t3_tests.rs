use super::*;
use crate::background_call::{self as call, Call, Prompt, Tool};
use std::collections::VecDeque;
use std::sync::Mutex;

const T3: &str = "t3:claudeAgent_claude_code_2";

/// A scripted T3: every call logged; the thread's timeline (`items`), the waits' answers in turn (the last repeats).
struct Fake {
    log: Mutex<Vec<(String, Value)>>,
    items: Mutex<Vec<Value>>,
    waits: Mutex<VecDeque<Value>>,
    status: Mutex<&'static str>,
    projects: Mutex<Vec<Value>>,
    /// Each wait sleeps this long before it answers (a run that never ends).
    sleep: Duration,
    /// `t3_project_create` is refused (a client without full access).
    deny_create: bool,
    /// The launch makes the thread, then its answer is lost (`Some(true)`: an error; `Some(false)`: no thread id).
    lose_launch: Option<bool>,
    /// Every call's deadline, in order.
    deadlines: Mutex<Vec<(String, Duration)>>,
    /// Titles of the threads launched.
    titles: Mutex<Vec<String>>,
    waited: std::sync::atomic::AtomicUsize,
    /// `t3_thread_send` fails (the thread was archived in T3).
    deny_send: bool,
    /// `t3_thread_send` names no run (T3 queued the turn).
    send_no_run: bool,
    /// `t3_thread_list` also brings archived threads (T3's list has no archived filter), marked `archived`.
    list_archived: bool,
    /// Each `t3_thread_organize` takes this long.
    organize_sleep: Duration,
}

impl Fake {
    fn new(items: Vec<Value>, waits: Vec<Value>) -> Arc<Fake> {
        Arc::new(Fake { log: Mutex::new(vec![]), items: Mutex::new(items), waits: Mutex::new(waits.into()), status: Mutex::new("idle"), projects: Mutex::new(vec![]), sleep: Duration::ZERO, deny_create: false,
            lose_launch: None, deadlines: Mutex::new(vec![]), titles: Mutex::new(vec![]), waited: Default::default(), deny_send: false, send_no_run: false, list_archived: false, organize_sleep: Duration::ZERO })
    }
    fn names(&self) -> Vec<String> { self.log.lock().unwrap().iter().map(|(n, _)| n.clone()).collect() }
    fn args(&self, name: &str) -> Vec<Value> { self.log.lock().unwrap().iter().filter(|(n, _)| n == name).map(|(_, a)| a.clone()).collect() }
}

#[async_trait]
impl Calls for Fake {
    async fn call(&self, name: &str, args: Value, deadline: Duration) -> Result<Value> {
        self.log.lock().unwrap().push((name.to_owned(), args.clone()));
        self.deadlines.lock().unwrap().push((name.to_owned(), deadline));
        Ok(match name {
            "t3_project_list" => json!({"projects": self.projects.lock().unwrap().clone(), "nextCursor": null}),
            "t3_project_create" if self.deny_create => anyhow::bail!("T3_TOOL_FAILED: capability_denied: Changing projects or environment settings needs a full-access client."),
            "t3_project_create" => {
                self.projects.lock().unwrap().push(json!({"id": "p-bg", "workspaceRoot": args["workspaceRoot"]}));
                json!({"projectId": "p-bg"})
            }
            "t3_thread_launch" => {
                self.titles.lock().unwrap().push(args["title"].as_str().unwrap().to_owned());
                match self.lose_launch {
                    Some(true) => anyhow::bail!("{}: no answer within 60000 ms. A mutation may have been accepted", ::t3::client::TIMEOUT),
                    Some(false) => json!({"status": "running"}),
                    // Threads are numbered by launch: th-1, th-2, …
                    None => { let n = self.titles.lock().unwrap().len(); json!({"threadId": format!("th-{n}"), "runId": format!("run-{n}"), "status": "running"}) }
                }
            }
            "t3_thread_list" => {
                // Launched threads (th-1, th-2, … by launch; a lost launch's "th-lost"), archived ones left out.
                let tag = args["titleContains"].as_str().unwrap();
                let archived = self.args("t3_thread_organize");
                let threads: Vec<Value> = self.titles.lock().unwrap().iter().enumerate().filter(|(_, t)| t.contains(tag))
                    .map(|(i, t)| json!({"threadId": if self.lose_launch.is_some() { "th-lost".to_owned() } else { format!("th-{}", i + 1) }, "title": t, "status": "idle"}))
                    .map(|mut t| { if archived.iter().any(|a| a["threadId"] == t["threadId"]) { t["archived"] = json!(true) } t })
                    .filter(|t| self.list_archived || t["archived"] != true).collect();
                json!({"threads": threads})
            }
            "t3_thread_wait" => {
                self.waited.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if !self.sleep.is_zero() { tokio::time::sleep(self.sleep).await; }
                let mut w = self.waits.lock().unwrap();
                let v = if w.len() > 1 { w.pop_front().unwrap() } else { w.front().cloned().unwrap_or(json!({"timedOut": true})) };
                v
            }
            "t3_thread_read" if args.get("itemId").is_some() => {
                // The rest of a cut-off item.
                let offset = args["textOffset"].as_u64().unwrap() as usize;
                let items = self.items.lock().unwrap();
                let whole = items.iter().find(|i| i["itemId"] == args["itemId"]).unwrap()["whole"].as_str().unwrap().to_owned();
                json!({"items": [{"itemId": args["itemId"], "text": &whole[offset..], "textTruncated": false}]})
            }
            "t3_thread_read" => {
                let after = args["afterPosition"].as_u64();
                // An item with `after_wait` appears only once that many waits have answered.
                let waited = self.waited.load(std::sync::atomic::Ordering::SeqCst) as u64;
                let items: Vec<Value> = self.items.lock().unwrap().iter().filter(|i| after.is_none_or(|a| i["position"].as_u64().unwrap() > a))
                    .filter(|i| i["after_wait"].as_u64().is_none_or(|n| waited >= n)).cloned().collect();
                json!({"thread": {"status": *self.status.lock().unwrap(), "pendingRequestCount": 0, "latestRunId": "run-1"}, "items": items, "hasMore": false})
            }
            "t3_thread_send" if self.deny_send => anyhow::bail!("T3_TOOL_FAILED: Thread {} is archived; unarchive it first.", args["threadId"].as_str().unwrap()),
            "t3_thread_send" if self.send_no_run => json!({"threadId": args["threadId"], "messageId": "m", "status": "queued", "delivery": "queued"}),
            "t3_thread_send" => json!({"threadId": args["threadId"], "messageId": "m", "runId": "run-sent", "status": "running", "delivery": "started"}),
            "t3_thread_organize" if !self.organize_sleep.is_zero() => { tokio::time::sleep(self.organize_sleep).await; json!({}) }
            "t3_thread_interrupt" | "t3_thread_organize" => json!({}),
            other => panic!("unexpected T3 call {other}"),
        })
    }
}

fn item(position: u64, kind: &str, text: &str) -> Value {
    json!({"position": position, "itemId": format!("i{position}"), "type": kind, "text": text, "status": "completed"})
}
fn done() -> Value { json!({"status": "completed", "timedOut": false}) }
fn timed_out() -> Value { json!({"status": "running", "timedOut": true}) }

fn schema() -> Value { json!({"type": "object", "properties": {"ok": {"type": "boolean"}, "word": {"type": "string"}}, "required": ["ok", "word"]}) }
fn job() -> Call<'static> {
    let mut p = Prompt::default();
    p.stable("<history>long ago</history>\n").push("Now: 10:00. Set ok and word.");
    Call { job: "test job", model: "claude-haiku-5-5".into(), system: "You sort things.", schema: schema(), effort: "low", prompt: p, history: Vec::new(), session: None }
}
fn limits() -> Limits { Limits { total: Duration::from_secs(5), slice: Duration::from_millis(10), cleanup: Duration::from_millis(500) } }

/// The fake registered, a ledger that keeps the runs, and the endpoint a "t3:" choice resolves to.
fn lab(fake: &Arc<Fake>, dir: &Path) -> (crate::plan_refresher::Endpoint, Arc<Mutex<Vec<Run>>>) {
    set_calls(fake.clone(), dir.join(PROJECT_DIR), limits());
    let runs = Arc::new(Mutex::new(vec![]));
    let keep = runs.clone();
    call::set_ledger(Arc::new(move |r: &Run| keep.lock().unwrap().push(r.clone())));
    crate::background_model::publish(&crate::background_model::BackgroundModel { provider: T3.into(), id: "claude-haiku-5-5".into() }, &[]);
    crate::background_model::publish_efforts(&[]);
    (crate::background_model::endpoint(), runs)
}

fn rt() -> tokio::runtime::Runtime { tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap() }

/// One call is one thread: the project made once, a launch on the instance in the most restrictive mode with the
/// whole job in one message, the wait, the reply's JSON, the archive, one ledger line at $0.
#[test]
fn a_call_launches_one_thread_reads_its_json_and_archives_it() {
    let dir = tempfile::tempdir().unwrap();
    let fake = Fake::new(vec![item(0, "user_message", "…"), item(1, "reasoning", "thinking"),
        item(2, "assistant_message", "```json\n{\"ok\": true, \"word\": \"pong\"}\n```")], vec![done()]);
    let (e, runs) = lab(&fake, dir.path());
    assert_eq!((e.provider, e.key.is_some(), e.model.as_deref()), (T3, true, Some("claude-haiku-5-5")));
    assert_eq!(call::Loop::for_endpoint(&e).limit, None, "a subscription: no dollars");
    let a = rt().block_on(call::ask(&e, &job())).unwrap();
    assert_eq!(serde_json::from_str::<Value>(&a.text).unwrap(), json!({"ok": true, "word": "pong"}));
    assert_eq!((a.usd, a.usage), (0.0, Usage::default()));
    let launch = &fake.args("t3_thread_launch")[0];
    assert_eq!(launch["projectId"], "p-bg");
    assert_eq!(launch["modelSelection"], json!({"instanceId": "claudeAgent_claude_code_2", "model": "claude-haiku-5-5", "options": {"effort": "low"}}));
    assert_eq!((launch["runtimeMode"].as_str(), launch["workspaceStrategy"]["type"].as_str()), (Some("approval-required"), Some("root")));
    assert!(launch["title"].as_str().unwrap().starts_with("[heimdall-bg] test job"));
    let m = launch["message"].as_str().unwrap();
    assert!(m.contains("Use no tools") && m.contains("You sort things.") && m.contains(&schema().to_string()) && m.contains("<history>long ago</history>\nNow: 10:00"), "{m}");
    assert!(m.find("You sort things.").unwrap() < m.find("<history>").unwrap(), "stable parts first");
    assert_eq!(fake.args("t3_thread_organize"), [json!({"threadId": "th-1", "action": "archive"})]);
    assert!(fake.args("t3_thread_interrupt").is_empty(), "it ended by itself");
    assert_eq!(fake.args("t3_project_create")[0]["title"], PROJECT_TITLE);
    assert!(dir.path().join(PROJECT_DIR).is_dir());
    let r = runs.lock().unwrap().clone();
    assert_eq!(r.len(), 1);
    assert_eq!((r[0].outcome, r[0].usd, r[0].model.as_str()), ("answered", 0.0, "t3:claudeAgent_claude_code_2/claude-haiku-5-5"));
    // The next call finds the project already made.
    let before = fake.names().len();
    rt().block_on(call::ask(&e, &job())).unwrap();
    assert!(!fake.names()[before..].iter().any(|n| n.starts_with("t3_project")), "{:?}", &fake.names()[before..]);
    // `ask_blocking` (the voice namer, the day timeline) takes the same path.
    assert!(call::ask_blocking(&e, &job()).is_ok());
}

/// A client T3 does not let make projects puts the threads in the project holding Heimdall's home.
#[test]
fn without_project_rights_the_threads_go_to_the_home_project() {
    let dir = tempfile::tempdir().unwrap();
    let mut fake = Fake::new(vec![item(1, "assistant_message", "{\"ok\": true, \"word\": \"w\"}")], vec![done()]);
    Arc::get_mut(&mut fake).unwrap().deny_create = true;
    fake.projects.lock().unwrap().push(json!({"id": "p-root", "workspaceRoot": "/"}));
    let (e, _) = lab(&fake, dir.path());
    rt().block_on(call::ask(&e, &job())).unwrap();
    assert_eq!(fake.args("t3_thread_launch")[0]["projectId"], "p-root");
}

/// A reply longer than T3 sends at once is read whole.
#[test]
fn a_cut_off_reply_is_read_whole() {
    let dir = tempfile::tempdir().unwrap();
    let whole = format!("{{\"ok\": true, \"word\": \"{}\"}}", "x".repeat(300));
    let mut cut = item(1, "assistant_message", &whole[..100]);
    cut["textTruncated"] = json!(true);
    cut["nextTextOffset"] = json!(100);
    cut["whole"] = json!(whole);
    let fake = Fake::new(vec![cut], vec![done()]);
    let (e, _) = lab(&fake, dir.path());
    let a = rt().block_on(call::ask(&e, &job())).unwrap();
    assert_eq!(serde_json::from_str::<Value>(&a.text).unwrap()["word"].as_str().unwrap().len(), 300);
}

/// Claude Code ends a turn it could not run with the provider's error as its reply: a failed call, not settled, and
/// a usage limit starts the instance's quota episode until the reset it states.
#[test]
fn a_harness_error_reply_fails_the_call_and_a_usage_limit_waits_for_its_reset() {
    let dir = tempfile::tempdir().unwrap();
    let reset_s = agent::now_ms() / 1000 + 3600;
    let fake = Fake::new(vec![item(1, "assistant_message", &format!("Claude AI usage limit reached|{reset_s}"))], vec![done()]);
    let (e, runs) = lab(&fake, dir.path());
    let err = rt().block_on(call::ask(&e, &job())).unwrap_err();
    assert!(!call::settled(&err) && err.to_string().contains("could not run"), "{err:#}");
    assert_eq!(fake.args("t3_thread_organize").len(), 1, "archived");
    assert_eq!(runs.lock().unwrap()[0].outcome, "failed");
    let out = crate::background_gate::quota_check(T3, agent::now_ms()).unwrap_err();
    assert!(out.downcast_ref::<crate::background_gate::QuotaOut>().is_some_and(|q| q.until_ms == reset_s * 1000), "{out:#}");
    // While spent, no thread is launched; the reset ends it.
    let launches = fake.args("t3_thread_launch").len();
    assert!(rt().block_on(call::ask(&e, &job())).is_err());
    assert_eq!(fake.args("t3_thread_launch").len(), launches);
    assert!(crate::background_gate::quota_check(T3, reset_s * 1000).is_ok());
    assert!(crate::background_gate::quota_check("anthropic", agent::now_ms()).is_ok(), "only that instance");
}

/// Bug note 2026-11-02T14-47-41: a spent account's run in T3 0.1.6 is a `system_notice` ("Claude usage limit
/// reached …", its id carrying the reset), Claude Code's reply and an `error` item, then status failed. That is the
/// account spent until the notice's reset (so the task falls back), never "the model asked for a tool".
#[test]
fn a_usage_limit_notice_is_the_account_spent_not_a_tool() {
    let dir = tempfile::tempdir().unwrap();
    let reset_s = agent::now_ms() / 1000 + 4 * 3600;
    let mut notice = item(1, "system_notice", "Claude usage limit reached. This turn is paused until the 7-day limit resets in 4h.");
    notice["title"] = json!("Claude usage limit reached. This turn is paused until the 7-day limit resets in 4h.");
    notice["itemId"] = json!(format!("turn-item:provider:claudeAgent:native-item:usage-limit%3Aprovider-turn%3Aseven_day%3A{reset_s}"));
    let mut error = item(3, "error", "Claude API rate limit reached. Try again later.");
    error["title"] = json!("Usage limit reached");
    error["status"] = json!("failed");
    let fake = Fake::new(vec![item(0, "user_message", "…"), notice, item(2, "assistant_message", "You've hit your weekly limit · resets Nov 6 at 9am (Europe/Warsaw)"), error],
        vec![json!({"status": "failed", "timedOut": false})]);
    let (e, runs) = lab(&fake, dir.path());
    let err = rt().block_on(call::ask(&e, &job())).unwrap_err();
    assert!(!err.to_string().contains("asked for a tool"), "{err:#}");
    assert!(err.to_string().contains("ended failed"), "{err:#}");
    assert_eq!(runs.lock().unwrap()[0].outcome, "failed");
    assert!(fake.args("t3_thread_interrupt").is_empty(), "not stopped as a tool request");
    let out = crate::background_gate::quota_check(T3, agent::now_ms()).unwrap_err();
    assert!(out.downcast_ref::<crate::background_gate::QuotaOut>().is_some_and(|q| q.until_ms == reset_s * 1000), "spent until the notice's reset: {out:#}");
    assert!(crate::background_gate::quota_check(T3, reset_s * 1000).is_ok());
}

/// Review 2 Nov: a failed run whose answer only talks about usage limits marks nothing spent.
#[test]
fn prose_about_limits_in_a_failed_run_marks_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let fake = Fake::new(vec![item(1, "assistant_message", "Today's notes: the usage limit was hit at 09:45; you've hit your goal.")], vec![json!({"status": "failed", "timedOut": false})]);
    let (e, _) = lab(&fake, dir.path());
    assert!(rt().block_on(call::ask(&e, &job())).is_err());
    assert!(crate::background_gate::quota_check(T3, agent::now_ms()).is_ok(), "not spent");
}

/// A tool item (or a wait on an approval or question) stops the run at once: interrupted, archived, `Stopped` so the
/// job does not resend the same input.
#[test]
fn a_tool_request_interrupts_and_fails_the_call() {
    let dir = tempfile::tempdir().unwrap();
    let mut cmd = item(2, "command_execution", "$ cat ~/.ssh/id_rsa");
    cmd["status"] = json!("running");
    let fake = Fake::new(vec![item(0, "user_message", "…"), item(1, "command_execution", "$ Preparing workspace"), cmd], vec![timed_out()]);
    let (e, runs) = lab(&fake, dir.path());
    let err = rt().block_on(call::ask(&e, &job())).unwrap_err();
    assert!(call::settled(&err) && err.to_string().contains("asked for a tool (command execution)"), "{err:#}");
    assert_eq!(fake.args("t3_thread_interrupt").len(), 1);
    assert_eq!(fake.args("t3_thread_organize").len(), 1);
    assert_eq!(runs.lock().unwrap()[0].outcome, "refused");
    // An approval or question wait.
    let fake = Fake::new(vec![item(0, "user_message", "…")], vec![timed_out()]);
    *fake.status.lock().unwrap() = "waiting";
    let (e, _) = lab(&fake, dir.path());
    let err = rt().block_on(call::ask(&e, &job())).unwrap_err();
    assert!(err.to_string().contains("an approval or question"), "{err:#}");
    assert_eq!((fake.args("t3_thread_interrupt").len(), fake.args("t3_thread_organize").len()), (1, 1));
}

/// A run that never ends stops at the call's deadline: interrupted and archived within it, counted as timed out.
#[test]
fn a_run_past_the_deadline_is_interrupted_and_archived() {
    let dir = tempfile::tempdir().unwrap();
    let mut fake = Fake::new(vec![item(0, "user_message", "…")], vec![timed_out()]);
    Arc::get_mut(&mut fake).unwrap().sleep = Duration::from_millis(30);
    let (e, runs) = lab(&fake, dir.path());
    set_calls(fake.clone(), dir.path().join(PROJECT_DIR), Limits { total: Duration::from_millis(400), slice: Duration::from_millis(20), cleanup: Duration::from_millis(150) });
    let t = std::time::Instant::now();
    let err = rt().block_on(call::ask(&e, &job())).unwrap_err();
    assert!(t.elapsed() < Duration::from_millis(800), "{:?}", t.elapsed());
    assert!(call::settled(&err) && err.to_string().contains("did not answer within"), "{err:#}");
    assert_eq!((fake.args("t3_thread_interrupt").len(), fake.args("t3_thread_organize").len()), (1, 1));
    assert_eq!(runs.lock().unwrap()[0].outcome, "timed out");
}

/// Review 30 Oct: a launch whose answer is lost (a timeout after T3 made the thread, or an answer without its id)
/// still has its thread found by the title's tag, interrupted and archived.
#[test]
fn a_lost_launch_answer_still_stops_and_archives_the_thread() {
    for lost in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let mut fake = Fake::new(vec![], vec![done()]);
        Arc::get_mut(&mut fake).unwrap().lose_launch = Some(lost);
        let (e, _) = lab(&fake, dir.path());
        let err = rt().block_on(call::ask(&e, &job())).unwrap_err();
        assert!(format!("{err:#}").contains(if lost { "did not start" } else { "lacks a thread id" }), "{err:#}");
        let title = fake.titles.lock().unwrap()[0].clone();
        let tag = title.rsplit(' ').next().unwrap();
        assert!(title.starts_with("[heimdall-bg] test job ") && tag.len() == 12, "{title}");
        assert_eq!(fake.args("t3_thread_list")[0]["titleContains"], tag);
        assert_eq!(fake.args("t3_thread_launch")[0]["clientRequestId"].as_str().unwrap()[..12], *tag);
        assert_eq!(fake.args("t3_thread_interrupt"), [json!({"threadId": "th-lost"})]);
        assert_eq!(fake.args("t3_thread_organize"), [json!({"threadId": "th-lost", "action": "archive"})]);
    }
}

/// Review 30 Oct: the interrupt and archive get a deadline of their own, outside the call's spent budget.
#[test]
fn cleanup_calls_keep_a_sane_deadline_after_the_budget_is_spent() {
    let dir = tempfile::tempdir().unwrap();
    let mut fake = Fake::new(vec![item(0, "user_message", "…")], vec![timed_out()]);
    Arc::get_mut(&mut fake).unwrap().sleep = Duration::from_millis(30);
    let (e, _) = lab(&fake, dir.path());
    set_calls(fake.clone(), dir.path().join(PROJECT_DIR), Limits { total: Duration::from_millis(200), slice: Duration::from_millis(20), cleanup: Duration::from_secs(4) });
    assert!(rt().block_on(call::ask(&e, &job())).is_err());
    let d = fake.deadlines.lock().unwrap().clone();
    for name in ["t3_thread_interrupt", "t3_thread_organize"] {
        assert_eq!(d.iter().find(|(n, _)| n == name).unwrap().1, Duration::from_secs(4), "{name}");
    }
    assert!(Limits::default().cleanup >= Duration::from_secs(5) && Limits::default().total + 2 * Limits::default().cleanup <= TIMEOUT);
}

/// Review 30 Oct: a wait that answers "idle" before the launched run has started does not end the call with no text;
/// it waits for the run it launched.
#[test]
fn idle_before_the_run_starts_keeps_waiting() {
    let dir = tempfile::tempdir().unwrap();
    let mut reply = item(1, "assistant_message", "{\"ok\": true, \"word\": \"w\"}");
    reply["after_wait"] = json!(2);
    let fake = Fake::new(vec![item(0, "user_message", "…"), reply], vec![json!({"status": "idle", "timedOut": false}), done()]);
    let (e, _) = lab(&fake, dir.path());
    let a = rt().block_on(call::ask(&e, &job())).unwrap();
    assert_eq!(serde_json::from_str::<Value>(&a.text).unwrap()["word"], "w");
    assert_eq!(fake.args("t3_thread_wait").len(), 2);
}

/// Review 30 Oct: the background project's folder work runs off the async runtime: every file-system call in this file
/// sits inside a `blocking` closure.
#[test]
fn the_project_lookup_does_no_file_system_work_on_the_runtime() {
    let src = include_str!("background_t3.rs");
    let shipping = src.split("#[cfg(test)]").next().unwrap();
    for (at, _) in shipping.match_indices("std::fs::") {
        let before = &shipping[at.saturating_sub(400)..at];
        assert!(before.contains("blocking(") || before.contains("spawn_blocking"), "std::fs outside a blocking closure: {}", &shipping[at.saturating_sub(120)..at + 40]);
    }
}

/// Prose, a missing field or a wrong type is no answer: `Stopped`, the thread archived.
#[test]
fn a_malformed_answer_is_stopped() {
    let dir = tempfile::tempdir().unwrap();
    for (reply, why) in [("I think yes.", "not JSON"), ("{\"ok\": true}", "lacks \"word\""), ("[1, 2]", "not a JSON object"),
                         ("{\"ok\": \"yes\", \"word\": \"w\"}", "\"ok\" is not \"boolean\""), ("", "no text")] {
        let fake = Fake::new(vec![item(1, "assistant_message", reply)], vec![done()]);
        let (e, _) = lab(&fake, dir.path());
        let err = rt().block_on(call::ask(&e, &job())).unwrap_err();
        assert!(call::settled(&err) && err.to_string().contains(why), "{reply}: {err:#}");
        assert_eq!(fake.args("t3_thread_organize").len(), 1);
    }
    // Prose around the JSON is cut away.
    assert_eq!(extract("Here it is: {\"ok\": true, \"word\": \"w\"} done"), Some(json!({"ok": true, "word": "w"})));
}

/// Review 30 Oct: a job's own failure (a prompt too long, a refused launch) changes no backend: the instance stays usable
/// for every other task.
#[test]
fn a_jobs_own_failure_marks_no_backend() {
    let dir = tempfile::tempdir().unwrap();
    let fake = Fake::new(vec![item(1, "assistant_message", "API Error: 400 {\"error\":{\"message\":\"prompt is too long: 250000 tokens > 200000 maximum\"}}")], vec![done()]);
    let (e, _) = lab(&fake, dir.path());
    assert!(rt().block_on(call::ask(&e, &job())).is_err());
    assert!(crate::background_gate::usable(T3, agent::now_ms()), "the instance is not marked");
    let fake = Fake::new(vec![item(1, "assistant_message", "API Error: 529 {\"type\":\"overloaded_error\"}")], vec![done()]);
    let (e, _) = lab(&fake, dir.path());
    assert!(rt().block_on(call::ask(&e, &job())).is_err());
    assert!(!crate::background_gate::usable(T3, agent::now_ms()), "overloaded: the instance is down");
}

/// A failed or interrupted run is a plain failure, retried on the next pass.
#[test]
fn a_failed_run_is_an_error_not_settled() {
    let dir = tempfile::tempdir().unwrap();
    let fake = Fake::new(vec![item(0, "user_message", "…")], vec![json!({"status": "failed", "timedOut": false})]);
    let (e, _) = lab(&fake, dir.path());
    let err = rt().block_on(call::ask(&e, &job())).unwrap_err();
    assert!(!call::settled(&err) && err.to_string().contains("ended failed"), "{err:#}");
}

/// The user's level for the model replaces the job's effort; with T3's list known, a level it does not list falls back to
/// the model's default; the prompt hash follows the effort.
#[test]
fn the_models_level_reaches_t3_as_options_effort() {
    use crate::background_model::BackgroundEffort;
    let dir = tempfile::tempdir().unwrap();
    let fake = Fake::new(vec![item(1, "assistant_message", "{\"ok\": true, \"word\": \"w\"}")], vec![done()]);
    let (e, _) = lab(&fake, dir.path());
    crate::background_model::publish_efforts(&[BackgroundEffort { provider: T3.into(), id: "claude-haiku-5-5".into(), level: "ultrathink".into() }]);
    rt().block_on(call::ask(&e, &job())).unwrap();
    assert_eq!(fake.args("t3_thread_launch")[0]["modelSelection"]["options"], json!({"effort": "ultrathink"}));
    // "t3:" alone is the default instance, for the level as for the model.
    crate::background_model::publish_efforts(&[BackgroundEffort { provider: "t3:".into(), id: "claude-haiku-5-5".into(), level: "max".into() }]);
    rt().block_on(call::ask(&e, &job())).unwrap();
    assert_eq!(fake.args("t3_thread_launch")[1]["modelSelection"]["options"], json!({"effort": "max"}));
    // No level: the job's own; one the model does not list (with T3's list known): the model's default.
    crate::background_model::publish_efforts(&[]);
    publish_catalog(&serde_json::from_str(include_str!("background_t3_capabilities.json")).unwrap());
    let e = crate::background_model::endpoint();
    assert!(e.efforts.contains(&"ultrathink".to_owned()));
    rt().block_on(call::ask(&e, &Call { effort: "minimal", ..job() })).unwrap();
    assert_eq!(fake.args("t3_thread_launch")[2]["modelSelection"]["options"], json!({"effort": "medium"}));
    assert_ne!(call::prompt_hash(&Call { effort: "max", ..job() }), call::prompt_hash(&job()));
}

/// Deep search and the suggester run tools in Heimdall: not on T3, said plainly, nothing launched.
#[test]
fn a_tool_loop_is_not_offered_on_t3() {
    let dir = tempfile::tempdir().unwrap();
    let fake = Fake::new(vec![], vec![done()]);
    let (e, _) = lab(&fake, dir.path());
    let tool = Tool { name: "look", description: "d", schema: json!({"type": "object"}) };
    let err = call::ask_tools_blocking(&e, &job(), &[tool], "submit", 3, &mut |_, _| String::new()).unwrap_err();
    assert!(err.downcast_ref::<NoToolsOnT3>().is_some() && err.to_string().contains("not on T3"), "{err:#}");
    assert!(fake.names().is_empty());
}

/// Agent off and emergency mode's pause stop a T3 call before anything reaches T3.
#[test]
fn agent_off_and_emergency_mode_stop_t3_calls_too() {
    let dir = tempfile::tempdir().unwrap();
    let fake = Fake::new(vec![], vec![done()]);
    let (e, _) = lab(&fake, dir.path());
    crate::background_gate::emergency_pause(Some(None));
    let err = rt().block_on(call::ask(&e, &job())).unwrap_err();
    assert!(crate::background_gate::quiet(&err));
    crate::background_gate::emergency_pause(None);
    let gate = Arc::new(agent::delivery::DeliveryGate::new(true));
    crate::background_gate::set(gate.clone());
    gate.intent(agent::delivery::StopReason::Agent, false);
    assert!(crate::background_gate::quiet(&rt().block_on(call::ask(&e, &job())).unwrap_err()));
    gate.intent(agent::delivery::StopReason::Agent, true);
    assert!(fake.names().is_empty(), "{:?}", fake.names());
}

/// T3's live catalog (captured 30 Oct): every enabled instance that can run a thread and lists models; the personal
/// edition's Claude ones only. Each model's effort values and default come along.
#[test]
fn the_instances_come_from_t3s_capabilities() {
    let catalog: Value = serde_json::from_str(include_str!("background_t3_capabilities.json")).unwrap();
    let (personal, _) = parse(&catalog, false);
    assert_eq!(personal.iter().map(|i| i.instance.as_str()).collect::<Vec<_>>(), ["claudeAgent", "claudeAgent_claude_code_2"], "no Codex, nothing disabled");
    let two = &personal[1];
    assert_eq!((two.name.as_str(), two.driver.as_str(), two.models.len()), ("Claude code 2", "claudeAgent", 4));
    let haiku = two.models.iter().find(|m| m.id == "claude-haiku-5-5").unwrap();
    assert_eq!((haiku.name.as_str(), haiku.efforts.join(" "), haiku.default_effort.as_deref()), ("Claude Haiku 5.5", "low medium high xhigh max ultrathink".into(), Some("medium")));
    let (public, flat) = parse(&catalog, true);
    assert_eq!(public.iter().map(|i| i.instance.as_str()).collect::<Vec<_>>(), ["codex", "claudeAgent", "claudeAgent_claude_code_2", "grok"]);
    let sol = flat.iter().find(|k| k.model.id == "gpt-6.1-sol").unwrap();
    assert_eq!((sol.option.as_deref(), sol.model.name.as_str(), sol.model.default_effort.as_deref()), (Some("reasoningEffort"), "gpt-6.1-sol", Some("low")));
    // Published: the choices check against it.
    publish_catalog(&catalog);
    assert!(choice_ok("claudeAgent_claude_code_2") && !choice_ok("cursor") && !choice_ok("nowhere"));
    assert_eq!(choice_ok("codex"), crate::edition::PUBLIC);
}

/// The menu's T3 choices: "t3:<instance>" with a model, "t3:" as the default instance; never Codex in the personal
/// edition; no T3 client: no call.
#[test]
fn t3_choices_are_checked_and_need_a_client() {
    use crate::background_model::BackgroundModel;
    assert_eq!(BackgroundModel::from_ui(T3, "claude-haiku-5-5").unwrap().provider, T3);
    assert_eq!(BackgroundModel::from_ui("t3:", "claude-haiku-5-5").unwrap().provider, T3);
    assert!(BackgroundModel::from_ui(T3, " ").is_none() && BackgroundModel::from_ui("t3:bad id", "m").is_none());
    assert_eq!(BackgroundModel::from_ui("t3:codex", "gpt-6.1-sol").is_some(), crate::edition::PUBLIC);
    // Nothing registered on this thread: the endpoint has no key, so no job calls.
    let e = endpoint(T3, "claude-haiku-5-5");
    assert!(e.key.is_none() && e.provider == T3);
    assert!(stated_reset("Claude AI usage limit reached|1793066649") == Some(1_793_066_649_000) && stated_reset("API Error: 429").is_none());
}

/// The levels on the other formats: Messages' `output_config.effort`, Responses' `reasoning.effort`, chat completions'
/// `reasoning_effort`; the menu's "off" as each format's least; an unknown name keeps the job's own.
#[test]
fn the_models_level_replaces_the_jobs_effort_on_every_format() {
    use crate::background_model::{publish_efforts, BackgroundEffort};
    let level = |p: &str, id: &str, l: &str| publish_efforts(&[BackgroundEffort { provider: p.into(), id: id.into(), level: l.into() }]);
    let ep = |wire: call::Wire, provider: &'static str| crate::plan_refresher::Endpoint { wire, provider, reasoning: true, ..Default::default() };
    let c = |model: &str| Call { model: model.into(), effort: "high", ..job() };
    let anth = ep(call::Wire::Anthropic, "anthropic");
    level("anthropic", "claude-haiku-5-5", "max");
    let got = call::with_effort(&anth, &c("claude-haiku-5-5"));
    assert_eq!(call::body(&got, 100)["output_config"]["effort"], "max");
    assert_eq!(call::with_effort(&anth, &c("claude-sonnet-5-5")).effort, "high", "another model keeps the job's");
    level("anthropic", "claude-haiku-5-5", "off");
    assert_eq!(call::with_effort(&anth, &c("claude-haiku-5-5")).effort, "low");
    let resp = ep(call::Wire::Responses, "openai");
    level("openai", "gpt-6.1-sol", "low");
    let got = call::with_effort(&resp, &c("gpt-6.1-sol"));
    assert_eq!(call::wire::answer_body(&call::Loop::for_endpoint(&resp), &got, 0)["reasoning"]["effort"], "low");
    let chat = ep(call::Wire::ChatCompletions, "openai-compatible");
    level("openai-compatible", "kimi", "off");
    let got = call::with_effort(&chat, &c("kimi"));
    assert_eq!(call::wire::answer_body(&call::Loop::for_endpoint(&chat), &got, 0)["reasoning_effort"], "minimal");
    level("openai-compatible", "kimi", "bogus");
    assert_eq!(call::with_effort(&chat, &c("kimi")).effort, "high");
    // The built-in engine's empty id is its current model.
    let local = ep(call::Wire::ChatCompletions, ui::LOCAL_ENGINE);
    level(ui::LOCAL_ENGINE, "", "medium");
    assert_eq!(call::with_effort(&local, &c(agent::runtime::MODEL)).effort, "medium");
    publish_efforts(&[]);
}

/// Live, opt-in (`cargo test -p app background_t3::tests::live -- --ignored`): one tiny Claude Haiku 5.5 call on
/// claudeAgent_claude_code_2 through the real T3 client, on the app's own path. It never touches the running app's
/// T3 connection: the lab client pairs a connection of its own (the app's pairing, full access, in a memory store),
/// and that session is revoked at the end (`t3 auth session revoke`, the sessions that were not there before it).
/// The project folder is a temporary one, and its T3 project is deleted at the end. A session T3's CLI issues cannot
/// stand in: T3's MCP takes only an MCP client's, and making a project needs a full-access client (live, 30 Oct).
#[test]
#[ignore]
fn live_one_tiny_haiku_call_on_claude_code_2() {
    struct Memory(Mutex<Option<::t3::client::Credential>>);
    impl ::t3::CredentialStore for Memory {
        fn load(&self) -> Result<Option<::t3::client::Credential>> { Ok(self.0.lock().unwrap().clone()) }
        fn save(&self, c: &::t3::client::Credential) -> Result<()> { *self.0.lock().unwrap() = Some(c.clone()); Ok(()) }
    }
    let home = PathBuf::from(std::env::var_os("HOME").unwrap());
    let cli = home.join(".t3/bin/t3");
    fn sessions(cli: &Path, t3home: &Path) -> Vec<String> {
        let out = std::process::Command::new(cli).args(["auth", "session", "list", "--json"]).env("T3CODE_HOME", t3home).output().unwrap();
        let v: Value = serde_json::from_slice(&out.stdout).unwrap_or(Value::Null);
        v.as_array().into_iter().flatten().filter_map(|s| s["sessionId"].as_str().map(str::to_owned)).collect()
    }
    // The lab's own sessions are revoked however the test ends: every one that was not there before it.
    struct Revoke(PathBuf, PathBuf, Vec<String>);
    impl Drop for Revoke {
        fn drop(&mut self) {
            for s in sessions(&self.0, &self.1).into_iter().filter(|s| !self.2.contains(s)) {
                let r = std::process::Command::new(&self.0).args(["auth", "session", "revoke", &s]).env("T3CODE_HOME", &self.1).output();
                eprintln!("lab session {s} revoked: {}", r.is_ok_and(|r| r.status.success()));
            }
        }
    }
    let _revoke = Revoke(cli.clone(), home.join(".t3"), sessions(&cli, &home.join(".t3")));
    assert!(!_revoke.2.is_empty(), "T3's sessions must be listable before the lab pairs");
    let client = Arc::new(::t3::Client::local(Arc::new(Memory(Mutex::new(None)))).unwrap());
    let dir = tempfile::tempdir().unwrap();
    set_calls(Arc::new(Real(client.clone())), dir.path().join(PROJECT_DIR), Limits::default());
    crate::background_model::publish(&crate::background_model::BackgroundModel { provider: T3.into(), id: "claude-haiku-5-5".into() }, &[]);
    let e = crate::background_model::endpoint();
    let c = Call { job: "live probe", model: "claude-haiku-5-5".into(), system: "You answer with the JSON the schema asks for, nothing else.", schema: schema(), effort: "low", prompt: "Set ok to true and word to \"pong\".".into(), history: Vec::new(), session: None };
    let t = std::time::Instant::now();
    // One runtime for the whole test: a connection pooled on a dropped runtime fails the next request.
    let rt = rt();
    let result = rt.block_on(call::ask(&e, &c));
    eprintln!("live T3 call: {:?} in {:.1} s", result.as_ref().map(|a| a.text.clone()).map_err(|e| format!("{e:#}")), t.elapsed().as_secs_f64());
    // What is left in T3: the thread (archived?) and the project, deleted with its folder.
    let project = PROJECTS.lock().unwrap().iter().find(|(f, _)| f == &dir.path().join(PROJECT_DIR)).map(|(_, id)| id.clone());
    rt.block_on(async {
        if let Some(p) = &project {
            // Its one thread was archived (the call logs a failed archive), so the project is deleted with `force`.
            // Only the lab's own project, never a fallback to Heimdall's home project.
            let ours = std::fs::canonicalize(dir.path().join(PROJECT_DIR)).ok();
            let list = client.call("t3_project_list", json!({"limit": 100})).await.unwrap_or(Value::Null);
            let lab_root = list["projects"].as_array().into_iter().flatten().find(|x| x["id"].as_str() == Some(p.as_str()))
                .and_then(|x| x["workspaceRoot"].as_str()).and_then(|r| std::fs::canonicalize(r).ok());
            if ours.is_some() && lab_root == ours {
                let deleted = client.call("t3_project_delete", json!({"projectId": p, "force": true})).await;
                eprintln!("lab project deleted: {}", deleted.map(|v| v.to_string()).unwrap_or_else(|e| format!("{e:#}")));
            } else {
                eprintln!("the threads went to project {p} (not the lab's own): not deleted");
            }
        }
    });
    let a = result.unwrap();
    assert_eq!(serde_json::from_str::<Value>(&a.text).unwrap(), json!({"ok": true, "word": "pong"}));
}

/// Live, opt-in, like the one above (Goals, 1 Nov: "make sure the Goals also runs"): a rolling session of two turns on
/// claudeAgent_claude_code_2: the first launches the session's thread, the second is sent into the same thread (one
/// thread in the lab project), then the session ends. Same lab pairing, revoked; same temporary project, deleted.
#[test]
#[ignore]
fn live_a_rolling_session_keeps_one_thread_on_claude_code_2() {
    struct Memory(Mutex<Option<::t3::client::Credential>>);
    impl ::t3::CredentialStore for Memory {
        fn load(&self) -> Result<Option<::t3::client::Credential>> { Ok(self.0.lock().unwrap().clone()) }
        fn save(&self, c: &::t3::client::Credential) -> Result<()> { *self.0.lock().unwrap() = Some(c.clone()); Ok(()) }
    }
    let home = PathBuf::from(std::env::var_os("HOME").unwrap());
    let cli = home.join(".t3/bin/t3");
    fn sessions(cli: &Path, t3home: &Path) -> Vec<String> {
        let out = std::process::Command::new(cli).args(["auth", "session", "list", "--json"]).env("T3CODE_HOME", t3home).output().unwrap();
        let v: Value = serde_json::from_slice(&out.stdout).unwrap_or(Value::Null);
        v.as_array().into_iter().flatten().filter_map(|s| s["sessionId"].as_str().map(str::to_owned)).collect()
    }
    // The lab's own sessions are revoked however the test ends: every one that was not there before it.
    struct Revoke(PathBuf, PathBuf, Vec<String>);
    impl Drop for Revoke {
        fn drop(&mut self) {
            for s in sessions(&self.0, &self.1).into_iter().filter(|s| !self.2.contains(s)) {
                let r = std::process::Command::new(&self.0).args(["auth", "session", "revoke", &s]).env("T3CODE_HOME", &self.1).output();
                eprintln!("lab session {s} revoked: {}", r.is_ok_and(|r| r.status.success()));
            }
        }
    }
    let _revoke = Revoke(cli.clone(), home.join(".t3"), sessions(&cli, &home.join(".t3")));
    assert!(!_revoke.2.is_empty(), "T3's sessions must be listable before the lab pairs");
    let client = Arc::new(::t3::Client::local(Arc::new(Memory(Mutex::new(None)))).unwrap());
    let dir = tempfile::tempdir().unwrap();
    set_calls(Arc::new(Real(client.clone())), dir.path().join(PROJECT_DIR), Limits::default());
    crate::background_model::publish(&crate::background_model::BackgroundModel { provider: T3.into(), id: "claude-haiku-5-5".into() }, &[]);
    let e = crate::background_model::endpoint();
    let first = Call { job: "live probe", model: "claude-haiku-5-5".into(), system: "You answer with the JSON the schema asks for, nothing else.", schema: schema(), effort: "low", prompt: "Set ok to true and word to \"one\".".into(), history: Vec::new(), session: Some("goals-live".into()) };
    let t = std::time::Instant::now();
    let rt = rt();
    let one = rt.block_on(call::ask(&e, &first));
    eprintln!("live turn 1: {:?} in {:.1} s", one.as_ref().map(|a| a.text.clone()).map_err(|e| format!("{e:#}")), t.elapsed().as_secs_f64());
    let one_text = one.as_ref().map(|a| a.text.clone()).unwrap_or_default();
    let second = Call { history: vec![call::Turn { user: first.prompt.clone(), assistant: one_text.clone() }], prompt: "Now set word to \"two\".".into(), ..first.clone() };
    let t = std::time::Instant::now();
    let result = rt.block_on(call::ask(&e, &second));
    eprintln!("live turn 2: {:?} in {:.1} s", result.as_ref().map(|a| a.text.clone()).map_err(|e| format!("{e:#}")), t.elapsed().as_secs_f64());
    // One thread for the session: count the lab project's threads, then end the session (archives it).
    let project_id = PROJECTS.lock().unwrap().iter().find(|(f, _)| f == &dir.path().join(PROJECT_DIR)).map(|(_, id)| id.clone());
    let threads = project_id.as_ref().map(|p| rt.block_on(client.call("t3_thread_list", json!({"projectId": p, "limit": 20}))).map(|v| v["threads"].as_array().map_or(0, Vec::len)).unwrap_or(0));
    eprintln!("threads in the lab project: {threads:?}");
    rt.block_on(async { call::t3::end_session("goals-live") });
    // What is left in T3: the thread (archived?) and the project, deleted with its folder.
    let project = PROJECTS.lock().unwrap().iter().find(|(f, _)| f == &dir.path().join(PROJECT_DIR)).map(|(_, id)| id.clone());
    rt.block_on(async {
        if let Some(p) = &project {
            // Its one thread was archived (the call logs a failed archive), so the project is deleted with `force`.
            // Only the lab's own project, never a fallback to Heimdall's home project.
            let ours = std::fs::canonicalize(dir.path().join(PROJECT_DIR)).ok();
            let list = client.call("t3_project_list", json!({"limit": 100})).await.unwrap_or(Value::Null);
            let lab_root = list["projects"].as_array().into_iter().flatten().find(|x| x["id"].as_str() == Some(p.as_str()))
                .and_then(|x| x["workspaceRoot"].as_str()).and_then(|r| std::fs::canonicalize(r).ok());
            if ours.is_some() && lab_root == ours {
                let deleted = client.call("t3_project_delete", json!({"projectId": p, "force": true})).await;
                eprintln!("lab project deleted: {}", deleted.map(|v| v.to_string()).unwrap_or_else(|e| format!("{e:#}")));
            } else {
                eprintln!("the threads went to project {p} (not the lab's own): not deleted");
            }
        }
    });
    assert_eq!(serde_json::from_str::<Value>(&one_text).unwrap(), json!({"ok": true, "word": "one"}));
    assert_eq!(serde_json::from_str::<Value>(&result.unwrap().text).unwrap(), json!({"ok": true, "word": "two"}));
    assert_eq!(threads, Some(1), "the second turn went into the session's thread");
}

// ---------------------------------------------------------------------------------------------------------
// A rolling session (the Goals shadow): one thread per session key.

fn answer(word: &str) -> String { format!("{{\"ok\": true, \"word\": \"{word}\"}}") }

/// The session's call after `n` earlier turns (each answered `w<k>`), its new input `input`.
fn rolling(session: &str, n: usize, input: &str) -> Call<'static> {
    let history = (0..n).map(|k| call::Turn { user: format!("lines {k}").into(), assistant: answer(&format!("w{k}")) }).collect();
    Call { history, session: Some(session.into()), prompt: input.into(), ..job() }
}

/// The timeline of a thread's next turn: its user message and reply after `at`.
fn turn_items(fake: &Fake, at: u64, word: &str) {
    fake.items.lock().unwrap().extend([item(at, "user_message", "…"), item(at + 1, "assistant_message", &answer(word))]);
}

fn word(a: &call::Answer) -> String { serde_json::from_str::<Value>(&a.text).unwrap()["word"].as_str().unwrap().to_owned() }

/// The first call launches the session's thread with the whole history in its first message and keeps it; the next
/// sends only the new turn into the same thread and reads only that turn's reply.
#[test]
fn a_session_launches_once_with_its_history_then_sends_only_the_new_turn() {
    let dir = tempfile::tempdir().unwrap();
    let fake = Fake::new(vec![], vec![done()]);
    turn_items(&fake, 0, "first");
    let (e, runs) = lab(&fake, dir.path());
    let a = rt().block_on(call::ask(&e, &rolling("goals-s1", 2, "lines 2"))).unwrap();
    assert_eq!(word(&a), "first");
    let launch = &fake.args("t3_thread_launch")[0];
    let title = launch["title"].as_str().unwrap();
    assert!(title.starts_with("[heimdall-bg] test job goals-s1 ") && title.rsplit(' ').next().unwrap().len() == 12, "{title}");
    let m = launch["message"].as_str().unwrap();
    assert!(m.starts_with("[heimdall-bg] A rolling background session") && m.contains("Use no tools") && m.contains("You sort things.") && m.contains(&schema().to_string()), "{m}");
    let (t0, t1, new) = (m.find("<input>\nlines 0\n</input>\n<answer>\n{\"ok\": true, \"word\": \"w0\"}").unwrap(), m.find("<input>\nlines 1\n</input>").unwrap(), m.rfind("<input>\nlines 2\n</input>").unwrap());
    assert!(t0 < t1 && t1 < new, "{m}");
    assert!(fake.args("t3_thread_organize").is_empty() && fake.args("t3_thread_interrupt").is_empty(), "the thread stays open");
    // The second turn: no launch, the new input alone into th-1. Its reply appears only after an idle wait: the first
    // turn's reply is never taken for it.
    *fake.waits.lock().unwrap() = vec![json!({"status": "idle", "timedOut": false}), done()].into();
    let mut items = vec![item(2, "user_message", "…"), item(3, "assistant_message", &answer("second"))];
    items[1]["after_wait"] = json!(3);
    fake.items.lock().unwrap().extend(items);
    let a = rt().block_on(call::ask(&e, &rolling("goals-s1", 3, "lines 3"))).unwrap();
    assert_eq!(word(&a), "second");
    assert_eq!(fake.args("t3_thread_launch").len(), 1);
    let sent = fake.args("t3_thread_send");
    assert_eq!((sent.len(), sent[0]["threadId"].as_str(), sent[0]["mode"].as_str()), (1, Some("th-1"), Some("auto")));
    assert_eq!(sent[0]["message"], "<input>\nlines 3\n</input>\n\nAnswer now with only the JSON.");
    assert!(fake.args("t3_thread_wait").iter().skip(1).all(|w| w["runId"] == "run-sent"), "it follows the run the send started");
    assert!(fake.args("t3_thread_organize").is_empty());
    let r = runs.lock().unwrap().clone();
    assert_eq!(r.iter().map(|r| r.outcome).collect::<Vec<_>>(), ["answered", "answered"], "one ledger line a call");
    // A stateless job beside it is untouched: its own thread, archived.
    rt().block_on(call::ask(&e, &job())).unwrap();
    assert_eq!(fake.args("t3_thread_organize"), [json!({"threadId": "th-2", "action": "archive"})]);
}

/// A remembered thread whose turns do not match the history (a restart's lost map, a fallback that ran elsewhere, a
/// failed turn) is archived and a fresh launch carries the whole history.
#[test]
fn a_session_whose_turns_do_not_match_relaunches_with_the_whole_history() {
    let dir = tempfile::tempdir().unwrap();
    let fake = Fake::new(vec![], vec![done()]);
    turn_items(&fake, 0, "first");
    let (e, _) = lab(&fake, dir.path());
    rt().block_on(call::ask(&e, &rolling("goals-s1", 0, "lines 0"))).unwrap();
    // The job's history has two turns more than the thread answered (one went to the fallback).
    turn_items(&fake, 2, "again");
    let a = rt().block_on(call::ask(&e, &rolling("goals-s1", 3, "lines 3"))).unwrap();
    assert_eq!(word(&a), "again");
    assert!(fake.args("t3_thread_send").is_empty());
    assert_eq!(fake.args("t3_thread_organize"), [json!({"threadId": "th-1", "action": "archive"})]);
    let second = &fake.args("t3_thread_launch")[1];
    assert!(second["message"].as_str().unwrap().contains("<input>\nlines 2\n</input>\n<answer>"), "the whole history");
    // The model changed (another instance or model after a fallback): relaunched too.
    turn_items(&fake, 4, "third");
    rt().block_on(call::ask(&e, &Call { model: "claude-sonnet-5-5".into(), ..rolling("goals-s1", 4, "lines 4") })).unwrap();
    assert_eq!((fake.args("t3_thread_launch").len(), fake.args("t3_thread_send").len()), (3, 0));
}

/// A thread T3 no longer takes a turn in (archived by hand) is replaced in the same call.
#[test]
fn a_session_thread_that_refuses_the_turn_is_replaced_at_once() {
    let dir = tempfile::tempdir().unwrap();
    let mut fake = Fake::new(vec![], vec![done()]);
    Arc::get_mut(&mut fake).unwrap().deny_send = true;
    turn_items(&fake, 0, "first");
    let (e, _) = lab(&fake, dir.path());
    rt().block_on(call::ask(&e, &rolling("goals-s1", 0, "lines 0"))).unwrap();
    turn_items(&fake, 2, "fresh");
    assert_eq!(word(&rt().block_on(call::ask(&e, &rolling("goals-s1", 1, "lines 1"))).unwrap()), "fresh");
    assert_eq!(fake.args("t3_thread_send").len(), 1);
    assert_eq!(fake.args("t3_thread_interrupt"), [json!({"threadId": "th-1"})]);
    assert_eq!(fake.args("t3_thread_organize"), [json!({"threadId": "th-1", "action": "archive"})]);
    assert_eq!(fake.args("t3_thread_launch").len(), 2);
}

/// `end_session` archives the thread; a new session key (compaction) archives the job's old one.
#[test]
fn ending_or_replacing_a_session_archives_its_thread() {
    let dir = tempfile::tempdir().unwrap();
    let fake = Fake::new(vec![], vec![done()]);
    turn_items(&fake, 0, "first");
    let (e, _) = lab(&fake, dir.path());
    rt().block_on(call::ask(&e, &rolling("goals-s1", 0, "lines 0"))).unwrap();
    end_session("goals-s1");
    assert_eq!(fake.args("t3_thread_organize"), [json!({"threadId": "th-1", "action": "archive"})]);
    end_session("goals-s1");
    assert_eq!(fake.args("t3_thread_organize").len(), 1, "once");
    // The same key afterwards launches anew; then a new segment's key archives that thread.
    turn_items(&fake, 2, "second");
    rt().block_on(call::ask(&e, &rolling("goals-s1", 1, "lines 1"))).unwrap();
    assert_eq!(fake.args("t3_thread_launch").len(), 2);
    turn_items(&fake, 4, "segment two");
    rt().block_on(call::ask(&e, &rolling("goals-s2", 0, "header"))).unwrap();
    assert_eq!(fake.args("t3_thread_organize")[1], json!({"threadId": "th-2", "action": "archive"}));
    assert!(fake.titles.lock().unwrap()[2].contains(" goals-s2 "));
    // Another job's session is not this job's segment: left alone.
    turn_items(&fake, 6, "other");
    rt().block_on(call::ask(&e, &Call { job: "other job", ..rolling("other-s1", 0, "x") })).unwrap();
    assert_eq!(fake.args("t3_thread_organize").len(), 2);
}

/// A failed turn in a session's thread (a tool request) interrupts and archives it, as any call; the next call
/// launches a fresh thread with the whole history.
#[test]
fn a_failed_session_turn_interrupts_and_archives_and_the_next_relaunches() {
    let dir = tempfile::tempdir().unwrap();
    let fake = Fake::new(vec![], vec![done()]);
    turn_items(&fake, 0, "first");
    let (e, runs) = lab(&fake, dir.path());
    rt().block_on(call::ask(&e, &rolling("goals-s1", 0, "lines 0"))).unwrap();
    let mut cmd = item(3, "command_execution", "$ ls");
    cmd["status"] = json!("running");
    fake.items.lock().unwrap().extend([item(2, "user_message", "…"), cmd]);
    *fake.waits.lock().unwrap() = vec![timed_out()].into();
    let err = rt().block_on(call::ask(&e, &rolling("goals-s1", 1, "lines 1"))).unwrap_err();
    assert!(call::settled(&err) && err.to_string().contains("asked for a tool"), "{err:#}");
    assert_eq!(fake.args("t3_thread_interrupt"), [json!({"threadId": "th-1"})]);
    assert_eq!(fake.args("t3_thread_organize"), [json!({"threadId": "th-1", "action": "archive"})]);
    assert_eq!(runs.lock().unwrap().last().unwrap().outcome, "refused");
    // The job kept no answer for that turn: its history is still one turn, and nothing remembers th-1.
    fake.items.lock().unwrap().retain(|i| i["position"].as_u64().unwrap() < 2);
    *fake.waits.lock().unwrap() = vec![done()].into();
    turn_items(&fake, 4, "fresh");
    assert_eq!(word(&rt().block_on(call::ask(&e, &rolling("goals-s1", 1, "lines 1"))).unwrap()), "fresh");
    assert_eq!((fake.args("t3_thread_launch").len(), fake.args("t3_thread_send").len()), (2, 1));
}

/// A stateless call's message is the one it always was; a session's first message carries its turns.
#[test]
fn only_a_session_gets_the_session_message() {
    assert!(message(&job()).starts_with("[heimdall-bg] A one-time, stateless background call"));
    assert!(message(&rolling("goals-s1", 0, "x")).starts_with("[heimdall-bg] A rolling background session"));
    assert!(!message(&rolling("goals-s1", 0, "x")).contains("<earlier_turns>"));
}

/// A restart forgets the map: the session's old thread, still open in T3, is archived before the fresh launch; a
/// stateless call's thread of the same job is not a session's and is left alone.
#[test]
fn a_restart_archives_the_session_thread_it_forgot() {
    let dir = tempfile::tempdir().unwrap();
    let fake = Fake::new(vec![], vec![done()]);
    turn_items(&fake, 0, "first");
    let (e, _) = lab(&fake, dir.path());
    rt().block_on(call::ask(&e, &rolling("goals-s1", 0, "lines 0"))).unwrap();
    sessions(|all| all.clear());
    turn_items(&fake, 2, "after restart");
    assert_eq!(word(&rt().block_on(call::ask(&e, &rolling("goals-s1", 1, "lines 1"))).unwrap()), "after restart");
    assert_eq!(fake.args("t3_thread_list")[0]["titleContains"], "[heimdall-bg] test job ");
    assert_eq!(fake.args("t3_thread_organize"), [json!({"threadId": "th-1", "action": "archive"})]);
    assert!(fake.args("t3_thread_send").is_empty());
    // A stateless title is never swept.
    fake.titles.lock().unwrap().push("[heimdall-bg] test job 0123456789ab".into());
    sessions(|all| all.clear());
    turn_items(&fake, 4, "again");
    rt().block_on(call::ask(&e, &rolling("goals-s1", 2, "lines 2"))).unwrap();
    let archived: Vec<Value> = fake.args("t3_thread_organize").iter().map(|a| a["threadId"].clone()).collect();
    assert_eq!(archived, [json!("th-1"), json!("th-2")]);
}

/// A send T3 queued names no run: the previous turn's run, already over, is never taken for the new one.
#[test]
fn a_queued_turn_without_a_run_is_not_ended_by_the_previous_run() {
    let dir = tempfile::tempdir().unwrap();
    let mut fake = Fake::new(vec![], vec![done()]);
    Arc::get_mut(&mut fake).unwrap().send_no_run = true;
    turn_items(&fake, 0, "first");
    let (e, _) = lab(&fake, dir.path());
    rt().block_on(call::ask(&e, &rolling("goals-s1", 0, "lines 0"))).unwrap();
    *fake.waits.lock().unwrap() = vec![json!({"status": "completed", "runId": "run-1", "timedOut": false}), done()].into();
    let mut items = vec![item(2, "user_message", "…"), item(3, "assistant_message", &answer("queued"))];
    items[1]["after_wait"] = json!(3);
    fake.items.lock().unwrap().extend(items);
    assert_eq!(word(&rt().block_on(call::ask(&e, &rolling("goals-s1", 1, "lines 1"))).unwrap()), "queued");
    assert_eq!(fake.args("t3_thread_wait").len(), 3);
}

/// `end_session` from inside a runtime (a job's async pass) archives on that runtime.
#[test]
fn end_session_inside_a_runtime_archives_the_thread() {
    let dir = tempfile::tempdir().unwrap();
    let fake = Fake::new(vec![], vec![done()]);
    turn_items(&fake, 0, "first");
    let (e, _) = lab(&fake, dir.path());
    rt().block_on(async {
        call::ask(&e, &rolling("goals-s1", 0, "lines 0")).await.unwrap();
        end_session("goals-s1");
        for _ in 0..10 { tokio::task::yield_now().await; }
    });
    assert_eq!(fake.args("t3_thread_organize"), [json!({"threadId": "th-1", "action": "archive"})]);
}

/// Review 31 Oct: T3's list brings archived threads back. A sweep skips those it marks and those this process archived,
/// archives at most `SWEEP_MAX`, and stops at one cleanup deadline, so the launch keeps its budget.
#[test]
fn the_sweep_skips_archived_threads_and_keeps_to_its_budget() {
    let dir = tempfile::tempdir().unwrap();
    let mut fake = Fake::new(vec![], vec![done()]);
    Arc::get_mut(&mut fake).unwrap().list_archived = true;
    turn_items(&fake, 0, "first");
    let (e, _) = lab(&fake, dir.path());
    rt().block_on(call::ask(&e, &rolling("goals-s1", 0, "lines 0"))).unwrap();
    end_session("goals-s1");
    // Listed again with `archived` (and remembered here): never archived twice, launch after launch.
    for n in 1..4 {
        turn_items(&fake, 2 * n, "again");
        rt().block_on(call::ask(&e, &rolling(&format!("goals-s{}", n + 1), 0, "x"))).unwrap();
    }
    let ids: Vec<Value> = fake.args("t3_thread_organize").iter().map(|a| a["threadId"].clone()).collect();
    assert_eq!(ids, [json!("th-1"), json!("th-2"), json!("th-3")], "each once, by the session change");
    // Remembered alone (T3 marks nothing): still skipped.
    let unmarked = Fake::new(vec![], vec![done()]);
    let (e2, _) = lab(&unmarked, dir.path());
    unmarked.titles.lock().unwrap().push("[heimdall-bg] test job goals-old 0123456789ab".into());
    mark_archived("th-1");
    turn_items(&unmarked, 0, "fresh");
    sessions(|all| all.clear());
    rt().block_on(call::ask(&e2, &rolling("goals-s9", 0, "x"))).unwrap();
    assert!(unmarked.args("t3_thread_organize").is_empty(), "{:?}", unmarked.args("t3_thread_organize"));
    // Forty leftovers open (a long outage), each archive slow: at most SWEEP_MAX go, within one cleanup deadline, and the
    // launch still answers.
    let mut slow = Fake::new(vec![], vec![done()]);
    Arc::get_mut(&mut slow).unwrap().organize_sleep = Duration::from_millis(40);
    for k in 0..40 { slow.titles.lock().unwrap().push(format!("[heimdall-bg] test job goals-old{k} {:012x}", k)); }
    turn_items(&slow, 0, "on time");
    let (e3, _) = lab(&slow, dir.path());
    sessions(|all| all.clear());
    archived(|a| a.clear());
    let t = std::time::Instant::now();
    assert_eq!(word(&rt().block_on(call::ask(&e3, &rolling("goals-s1", 0, "x"))).unwrap()), "on time");
    assert_eq!(slow.args("t3_thread_organize").len(), SWEEP_MAX);
    assert!(t.elapsed() < limits().cleanup + Duration::from_millis(300), "{:?}", t.elapsed());
    // A sweep slower than its deadline stops there.
    let mut stuck = Fake::new(vec![], vec![done()]);
    Arc::get_mut(&mut stuck).unwrap().organize_sleep = Duration::from_secs(2);
    for k in 0..3 { stuck.titles.lock().unwrap().push(format!("[heimdall-bg] test job goals-old{k} {:012x}", k)); }
    turn_items(&stuck, 0, "still");
    let (e4, _) = lab(&stuck, dir.path());
    sessions(|all| all.clear());
    let t = std::time::Instant::now();
    assert_eq!(word(&rt().block_on(call::ask(&e4, &rolling("goals-s1", 0, "x"))).unwrap()), "still");
    assert!(t.elapsed() < limits().cleanup + Duration::from_millis(300), "{:?}", t.elapsed());
}
