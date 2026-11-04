//! Plan refresher tests: a fake Messages API on localhost (never a real call), lab islands and session folders.
use super::*;
use crate::pi_session::without_own_notices;
use std::io::Write;
use std::sync::atomic::AtomicUsize;

#[derive(Clone)]
enum Mode {
    /// Answer with this text block.
    Answer(String),
    /// Wait, then answer.
    Slow(Duration, String),
    Status(u16),
    /// Answer with this whole response.
    Raw(Value),
}

struct Fake {
    mode: Mutex<Mode>,
    hits: AtomicUsize,
    body: Mutex<Value>,
    headers: Mutex<Vec<(String, String)>>,
}

async fn serve() -> (String, Arc<Fake>) {
    use axum::{extract::State, http::HeaderMap, routing::post, Json, Router};
    let fake = Arc::new(Fake { mode: Mutex::new(Mode::Status(500)), hits: AtomicUsize::new(0), body: Mutex::new(Value::Null), headers: Mutex::new(vec![]) });
    async fn messages(State(f): State<Arc<Fake>>, headers: HeaderMap, Json(body): Json<Value>) -> axum::response::Response {
        use axum::response::IntoResponse;
        f.hits.fetch_add(1, Ordering::SeqCst);
        *f.body.lock().unwrap() = body;
        *f.headers.lock().unwrap() = headers.iter().map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_owned())).collect();
        let mode = f.mode.lock().unwrap().clone();
        let answer = |text: String| Json(json!({"type":"message","role":"assistant","model":MODEL,
            "content":[{"type":"thinking","thinking":"","signature":"sig"},{"type":"text","text":text}],
            "stop_reason":"end_turn"})).into_response();
        match mode {
            Mode::Answer(t) => answer(t),
            Mode::Slow(d, t) => { tokio::time::sleep(d).await; answer(t) }
            Mode::Raw(v) => Json(v).into_response(),
            Mode::Status(s) => (axum::http::StatusCode::from_u16(s).unwrap(), Json(json!({"type":"error","error":{"type":"api_error","message":"boom"}}))).into_response(),
        }
    }
    let app = Router::new().route("/v1/messages", post(messages)).with_state(fake.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, fake)
}

/// The user message's text blocks, joined (the prompt is sent as blocks for the prompt cache).
fn user_text(body: &Value) -> String {
    body["messages"][0]["content"].as_array().unwrap().iter().map(|b| b["text"].as_str().unwrap()).collect()
}

struct Lab {
    _dir: tempfile::TempDir,
    cfg: Config,
    root: Root,
    session: PathBuf,
}

fn lab(url: &str, key: Option<&str>, timeout: Duration) -> Lab {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let notes = dir.path().join("notes");
    std::fs::create_dir_all(data.join("agent")).unwrap();
    std::fs::create_dir_all(&notes).unwrap();
    std::fs::write(notes.join("night-pass-2026-10-21.md"), "old note").unwrap();
    std::fs::write(notes.join("night-pass-2026-10-22.md"), "- Arsenal: push the branch tonight").unwrap();
    let session = data.join("2026-10-14T20-24-54-925Z_x.jsonl");
    std::fs::write(data.join("agent/active-session.json"), serde_json::to_vec(&session).unwrap()).unwrap();
    let mut f = std::fs::File::create(&session).unwrap();
    for line in [
        json!({"type":"message","id":"e0","parentId":null,"message":{"role":"user","content":"User: summarised long ago"}}),
        json!({"type":"compaction","id":"c0","parentId":"e0","firstKeptEntryId":"e0","summary":"OLD summary"}),
        json!({"type":"message","id":"e1","parentId":"c0","message":{"role":"user","content":[{"type":"text","text":"User: push the Arsenal branch tonight"}]}}),
        // An abandoned branch (Pi sessions are trees): never part of the context.
        json!({"type":"message","id":"x1","parentId":"e1","message":{"role":"user","content":"User: abandoned branch"}}),
        json!({"type":"message","id":"e2","parentId":"e1","message":{"role":"user","content":format!("User: a long ask {}", "word ".repeat(1200))}}),
        // Kept from e1 on: Pi still sends e1 and e2 to the model after this compaction.
        json!({"type":"compaction","id":"c1","parentId":"e2","firstKeptEntryId":"e1","summary":"User works on Arsenal and Heimdall."}),
        json!({"type":"message","id":"e3","parentId":"c1","message":{"role":"assistant","content":[{"type":"thinking","thinking":"secret"},{"type":"text","text":"I'll do it tonight."},{"type":"toolCall","id":"t","name":"mcp__heimdall__delegate_task","arguments":{"title":"Arsenal push"}}]}}),
        json!({"type":"message","id":"e4","parentId":"e3","message":{"role":"toolResult","toolName":"mcp__heimdall__delegate_task","content":[{"type":"text","text":"Task started: abc."},{"type":"image","data":"AAAA","mimeType":"image/png"}]}}),
        json!({"type":"custom","id":"e5","parentId":"e4","customType":"heimdall_receipt","data":{}}),
    ] { writeln!(f, "{line}").unwrap(); }
    let root = Root(dir.path().join("islands-root"));
    let (url, key) = (url.to_owned(), key.map(str::to_owned));
    let cfg = Config {
        data,
        islands: root.clone(),
        notes,
        endpoint: Arc::new(move || Endpoint { base: url.clone(), key: key.clone(), workspace: None, timeout, model: None, ..Default::default() }),
        enabled: Arc::new(|| true),
    };
    Lab { _dir: dir, cfg, root, session }
}

fn seed(root: &Root) {
    let put = |id: &str, title: &str, items: Value| {
        let mut o = Map::new();
        o.insert("preset".into(), json!("todo"));
        o.insert("list".into(), json!(id));
        o.insert("title".into(), json!(title));
        o.insert("items".into(), items);
        o.insert("unknown_key".into(), json!("kept"));
        root.put(id, &o).unwrap();
    };
    put("scheduled", "Scheduled", json!([
        {"id":"a1","title":"Arsenal: push branch","done":false},
        {"id":"w1","title":"Delete old caches","done":false,"note":{"text":"rm caches"}},
        {"id":"r1","title":"Heimdall: fix underrun","done":false},
    ]));
    put("settled", "Settled", json!([{"id":"s1","title":"Vella: ship 1.2","done":true}]));
}

/// A local date-time `days` ahead, the way the model writes a time ("2026-11-05T10:00").
fn in_days(days: i64) -> String { (chrono::Local::now() + chrono::Duration::days(days)).format("%Y-%m-%dT%H:%M").to_string() }

/// The title `answer_proposing` proposes.
const PROPOSED: &str = "Vireo: icon review";

/// The notice for one proposal that came on screen, as `diff_text` words it.
fn proposed_notice(title: &str) -> String {
    format!("Plan: proposed \u{201c}{title}\u{201d} for Scheduled, waiting for User's yes (one is on screen: say it in one short line; his yes is plan_edit accept, his no decline).")
}

/// An answer that does what the model used to do to the lists (word for a time, tick, drop, move to Settled) and
/// proposes one item with a real time. Only the proposal is the refresher's to write (2 Nov).
fn answer_proposing() -> String {
    json!({
        "scheduled": [
            {"id":"a1","title":"Arsenal: push branch","when":"Tonight","done":false},
            {"id":"r1","title":"Heimdall: fix underrun","done":true},
            {"id":"","title":PROPOSED,"when":in_days(2),"detail":"Needs: User said the icon is next.","done":false},
            {"id":"","title":"Tidy the notes","when":"Later","done":false},
        ],
        "jobs": []
    }).to_string()
}

/// An answer that returns the lists as they are now.
fn echo(root: &Root) -> String {
    let list = |id: &str| -> Vec<Value> { items(root, id).iter().map(|v| {
        let mut o = json!({"id":v["id"],"title":v["title"],"done":v["done"]});
        if let Some(x) = v.get("when") { o["when"] = x.clone(); }
        o
    }).collect() };
    json!({"scheduled":list("scheduled"),"settled":list("settled"),"jobs":[]}).to_string()
}

/// `echo` plus new items (title, when) the model suggests without an id.
fn echo_with(root: &Root, new: &[(&str, &str)]) -> String {
    let mut a: Value = serde_json::from_str(&echo(root)).unwrap();
    for (title, when) in new { a["scheduled"].as_array_mut().unwrap().push(json!({"id":"","title":title,"when":when,"done":false})); }
    a.to_string()
}

/// scheduled.json's proposal queue.
fn proposals_of(root: &Root) -> Vec<Value> {
    root.read("scheduled").and_then(|o| o.get("proposals").and_then(Value::as_array).cloned()).unwrap_or_default()
}

fn now_s() -> i64 { chrono::Utc::now().timestamp() }

fn items(root: &Root, id: &str) -> Vec<Value> {
    root.read(id).unwrap()["items"].as_array().unwrap().clone()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pass_applies_the_answer_keeps_ids_and_unknown_keys_and_reports_the_diff() {
    let (url, fake) = serve().await;
    *fake.mode.lock().unwrap() = Mode::Answer(answer_proposing());
    let l = lab(&url, Some("sk-test"), Duration::from_secs(10));
    seed(&l.root);
    let before = items(&l.root, "scheduled");
    let shared = Shared::default();
    let notice = pass(&l.cfg, &shared).await.unwrap().unwrap();
    // One system (2 Nov): the pass proposes the one item with a real time and touches nothing else. The answer's
    // word for a time, tick and dropped item on existing promises, and its item without a time, change nothing.
    assert_eq!(notice, proposed_notice(PROPOSED));
    let sched = items(&l.root, "scheduled");
    assert_eq!(sched, before, "existing promises exactly as they were");
    assert_eq!(sched[0]["id"], "a1", "ids kept");
    assert!(sched[0].get("when").is_none() && sched.iter().all(|i| i["done"] == false), "no word for a time, no tick");
    assert_eq!(l.root.read("scheduled").unwrap()["unknown_key"], "kept", "island keys survive");
    let queue = proposals_of(&l.root);
    assert_eq!(queue.len(), 1, "the item without a time is dropped: {queue:?}");
    let p = &queue[0];
    assert_eq!((p["title"].as_str(), p["by"].as_str(), p["why"].as_str()), (Some(PROPOSED), Some("refresher"), Some("User said the icon is next.")));
    assert_eq!(p["due"].as_i64(), crate::islands::plan::due_seconds(&in_days(2)).ok());
    assert!(p["front"].as_i64().is_some() && p.get("shown").is_none() && p["id"].as_str().unwrap().starts_with("s-"), "on screen, not yet seen: {p}");
    let settled = items(&l.root, "settled");
    assert_eq!(settled, [json!({"id":"s1","title":"Vella: ship 1.2","done":true})], "Settled untouched");
    // The request: Haiku 5.5, adaptive thinking at effort high, structured output, the key header, the input.
    let body = fake.body.lock().unwrap().clone();
    assert_eq!(body["model"], "claude-haiku-5-5");
    assert_eq!(body["thinking"], json!({"type":"adaptive"}));
    assert_eq!(body["output_config"]["effort"], "high");
    assert_eq!(body["output_config"]["format"]["type"], "json_schema");
    assert!(body.get("temperature").is_none() && body.get("top_p").is_none());
    let prompt = &user_text(&body);
    assert!(prompt.contains("User works on Arsenal and Heimdall.") && !prompt.contains("OLD summary"), "latest compaction summary only");
    assert!(prompt.contains("Incoming: User: push the Arsenal branch tonight"), "the compaction's kept range");
    assert!(prompt.contains(&format!("User: a long ask {}", "word ".repeat(1200).trim_end())), "the whole session, not a trimmed tail");
    assert!(!prompt.contains("summarised long ago") && !prompt.contains("abandoned branch"), "only what Pi still sends, on the active branch");
    assert!(prompt.contains("Heimdall called delegate_task"));
    assert!(prompt.contains("delegate_task answered: Task started: abc."));
    assert!(!prompt.contains("secret") && !prompt.contains("AAAA"), "no thinking, no images");
    assert!(prompt.contains("night-pass-2026-10-22.md") && !prompt.contains("old note"), "newest night note");
    let headers = fake.headers.lock().unwrap().clone();
    assert!(headers.contains(&("x-api-key".into(), "sk-test".into())));
    assert!(headers.contains(&("anthropic-version".into(), "2023-06-01".into())));
}

/// Replaces `a_scheduled_date_time_becomes_the_due_time_the_island_counts_down_to` (the refresher no longer writes
/// `due` onto items): a date-time becomes the proposal's due time, a word or a time already past is no promise, and
/// the model still sees the due time of an existing promise the way it writes one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_date_time_becomes_the_proposal_s_due_time_and_a_word_or_past_time_is_dropped() {
    let (url, fake) = serve().await;
    let rfc = (chrono::Utc::now() + chrono::Duration::days(3)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let answer = json!({
        "scheduled": [{"id":"a1","title":"Arsenal: push branch","when":"2030-10-24T10:00","done":false},
                      {"id":"","title":"Vella: notes","when":in_days(1),"done":false},
                      {"id":"","title":"Vella: release video","when":rfc,"done":false},
                      {"id":"","title":"Later: something","when":"Later","done":false},
                      {"id":"","title":"Missed: tonight","when":"2020-01-01T23:00","done":false},
                      {"id":"w1","title":"Delete old caches","done":false},{"id":"r1","title":"Heimdall: fix underrun","done":false}],
        "jobs": []
    }).to_string();
    *fake.mode.lock().unwrap() = Mode::Answer(answer);
    let l = lab(&url, Some("sk-test"), Duration::from_secs(10));
    seed(&l.root);
    let before = items(&l.root, "scheduled");
    let shared = Shared::default();
    pass(&l.cfg, &shared).await.unwrap();
    assert_eq!(items(&l.root, "scheduled"), before, "an existing promise gets no time from a pass");
    let queue = proposals_of(&l.root);
    let got: Vec<(&str, Option<i64>)> = queue.iter().map(|p| (p["title"].as_str().unwrap(), p["due"].as_i64())).collect();
    let due = |t: &str| Some(crate::islands::plan::due_seconds(t).unwrap());
    assert_eq!(got, [("Vella: notes", due(&in_days(1))), ("Vella: release video", due(&rfc))], "{queue:?}");
    assert!(queue[0].get("front").is_some() && queue[1].get("front").is_none(), "one shows, the next queues behind it");
    // An existing promise's due time reaches the model the way it writes one; a Settled item's never does.
    l.root.patch("scheduled", |o| { o["items"][0]["due"] = json!(crate::islands::plan::due_seconds("2030-10-24T10:00").unwrap()); }).unwrap();
    l.root.patch("settled", |o| { o["items"][0]["due"] = json!(1_000_000); }).unwrap();
    *fake.mode.lock().unwrap() = Mode::Answer(echo(&l.root));
    writeln!(std::fs::OpenOptions::new().append(true).open(&l.session).unwrap(), "{}", json!({"type":"message","message":{"role":"user","content":"more"}})).unwrap();
    pass(&l.cfg, &shared).await.unwrap();
    let prompt = user_text(&fake.body.lock().unwrap());
    assert!(prompt.contains("\"when\": \"2030-10-24T10:00\""), "{prompt}");
    assert!(!prompt.contains("1970-01-1"), "a Settled item's due time stays out of the model's view");
    assert_eq!(items(&l.root, "scheduled")[0]["due"].as_i64(), due("2030-10-24T10:00"));
    assert_eq!(proposals_of(&l.root), queue, "an echo proposes nothing and withdraws nothing");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nothing_new_skips_the_model_and_new_conversation_runs_it_again() {
    let (url, fake) = serve().await;
    *fake.mode.lock().unwrap() = Mode::Answer(answer_proposing());
    let l = lab(&url, Some("k"), Duration::from_secs(10));
    seed(&l.root);
    let shared = Shared::default();
    assert!(pass(&l.cfg, &shared).await.unwrap().is_some());
    // Its own write does not count as new.
    assert_eq!(pass(&l.cfg, &shared).await.unwrap(), None);
    assert_eq!(fake.hits.load(Ordering::SeqCst), 1);
    // Same lists again: the model runs, nothing changed, no notice.
    *fake.mode.lock().unwrap() = Mode::Answer(echo(&l.root));
    writeln!(std::fs::OpenOptions::new().append(true).open(&l.session).unwrap(), "{}", json!({"type":"message","message":{"role":"user","content":"more"}})).unwrap();
    assert_eq!(pass(&l.cfg, &shared).await.unwrap(), None);
    assert_eq!(fake.hits.load(Ordering::SeqCst), 2);
    // The user ticks an item: new.
    l.root.patch("scheduled", |o| { o["items"][0]["done"] = json!(true); }).unwrap();
    let _ = pass(&l.cfg, &shared).await;
    assert_eq!(fake.hits.load(Ordering::SeqCst), 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_malformed_answer_changes_nothing_and_waits_for_something_new() {
    let (url, fake) = serve().await;
    let l = lab(&url, Some("k"), Duration::from_secs(10));
    seed(&l.root);
    let before: Vec<_> = LISTS.iter().map(|(id, _)| items(&l.root, id)).collect();
    let shared = Shared::default();
    for bad in ["not json {", "{\"scheduled\": []}", "{\"scheduled\":[{\"id\":\"a1\"}],\"settled\":[],\"jobs\":[]}",
                "{\"scheduled\":[],\"settled\":[],\"jobs\":[]}"] {
        *fake.mode.lock().unwrap() = Mode::Answer(bad.into());
        *shared.last.lock().unwrap() = None;
        assert!(pass(&l.cfg, &shared).await.is_err(), "{bad}");
        let after: Vec<_> = LISTS.iter().map(|(id, _)| items(&l.root, id)).collect();
        assert_eq!(before, after, "never wipes or edits the lists on a bad answer: {bad}");
    }
    let hits = fake.hits.load(Ordering::SeqCst);
    assert_eq!(pass(&l.cfg, &shared).await.unwrap(), None, "the same input is not retried");
    assert_eq!(fake.hits.load(Ordering::SeqCst), hits);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
/// A server error costs nothing: the next tick retries the same input. A timeout may have been a paid runaway answer
/// (128K of output takes minutes): counted at its worst case and sent again only with something new (item 28).
async fn a_timeout_or_server_error_changes_nothing_and_the_next_tick_retries() {
    let (url, fake) = serve().await;
    let l = lab(&url, Some("k"), Duration::from_millis(300));
    seed(&l.root);
    let before = items(&l.root, "scheduled");
    let shared = Shared::default();
    *fake.mode.lock().unwrap() = Mode::Slow(Duration::from_secs(3), answer_proposing());
    let started = std::time::Instant::now();
    let e = pass(&l.cfg, &shared).await.unwrap_err();
    assert!(format!("{e:#}").contains("timed out"), "{e:#}");
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(items(&l.root, "scheduled"), before);
    assert!(proposals_of(&l.root).is_empty(), "a timed-out pass proposes nothing");
    assert_eq!(pass(&l.cfg, &shared).await.unwrap(), None, "a timed-out input is not sent again");
    assert_eq!(fake.hits.load(Ordering::SeqCst), 1);
    append(&l.session, json!({"type":"message","message":{"role":"user","content":"User: and then"}}));
    *fake.mode.lock().unwrap() = Mode::Status(529);
    let e = pass(&l.cfg, &shared).await.unwrap_err();
    assert!(format!("{e:#}").contains("529") && !format!("{e:#}").contains("k\""), "{e:#}");
    *fake.mode.lock().unwrap() = Mode::Answer(answer_proposing());
    assert!(pass(&l.cfg, &shared).await.unwrap().is_some(), "retried after failures");
    assert_eq!(fake.hits.load(Ordering::SeqCst), 3);
    assert_eq!(proposals_of(&l.root).len(), 1, "the retry proposed once");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_a_key_no_request_is_made() {
    let (url, fake) = serve().await;
    *fake.mode.lock().unwrap() = Mode::Answer(answer_proposing());
    let l = lab(&url, None, Duration::from_secs(10));
    seed(&l.root);
    let before = items(&l.root, "scheduled");
    assert_eq!(pass(&l.cfg, &Shared::default()).await.unwrap(), None);
    assert_eq!(fake.hits.load(Ordering::SeqCst), 0);
    assert_eq!(items(&l.root, "scheduled"), before);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn requests_during_a_pass_coalesce_into_one_follow_up() {
    let (url, fake) = serve().await;
    *fake.mode.lock().unwrap() = Mode::Slow(Duration::from_millis(600), answer_proposing());
    let l = lab(&url, Some("k"), Duration::from_secs(10));
    seed(&l.root);
    let r = Refresher::default();
    assert!(r.request().contains("not running"));
    let sent = Arc::new(Mutex::new(Vec::<String>::new()));
    let s2 = sent.clone();
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let task = r.spawn(l.cfg.clone(), Arc::new(move |t: &str| { s2.lock().unwrap().push(t.into()); Ok(()) }), Duration::from_secs(3600), Duration::ZERO, stop_rx);
    assert_eq!(r.request(), STARTED);
    tokio::time::sleep(Duration::from_millis(150)).await;
    // While the first pass waits on the model: new conversation and five more requests.
    writeln!(std::fs::OpenOptions::new().append(true).open(&l.session).unwrap(), "{}", json!({"type":"message","message":{"role":"user","content":"and tonight too"}})).unwrap();
    for _ in 0..5 { assert_eq!(r.request(), STARTED); }
    tokio::time::sleep(Duration::from_millis(2000)).await;
    assert_eq!(fake.hits.load(Ordering::SeqCst), 2, "one pass plus one coalesced follow-up");
    assert!(!sent.lock().unwrap().is_empty(), "the changed plan reached the agent");
    stop_tx.send_replace(true);
    tokio::time::timeout(Duration::from_secs(2), task).await.unwrap().unwrap();
    assert!(r.request().contains("not running"));
}

#[test]
fn the_diff_reads_as_one_short_line() {
    let proposed = Diff { added: vec!["Arsenal: push branch".into()], ..Default::default() };
    assert_eq!(diff_text(&proposed).unwrap(), "Plan: proposed \u{201c}Arsenal: push branch\u{201d} for Scheduled, waiting for User's yes.");
    // The proposal that came on screen: Heimdall says it in one short line.
    let shown = Diff { shown: Some(json!({"id":"s-1","title":"Arsenal: push branch"}).as_object().unwrap().clone()), ..proposed.clone() };
    assert_eq!(diff_text(&shown).unwrap(), proposed_notice("Arsenal: push branch"));
    let two = Diff { added: vec!["A".into(), "B".into()], ..Default::default() };
    assert_eq!(diff_text(&two).unwrap(), "Plan: proposed \u{201c}A\u{201d}, \u{201c}B\u{201d} for Scheduled, waiting for User's yes.");
    assert_eq!(diff_text(&Diff::default()), None);
    let many = Diff { added: (0..200).map(|i| format!("Item number {i}")).collect(), ..Default::default() };
    assert!(diff_text(&many).unwrap().chars().count() <= 700);
}

#[test]
fn validation_caps_lists_and_strings_and_mints_ids_only_here() {
    let current: Lists = [vec![json!({"id":"a1","title":"x"}).as_object().unwrap().clone()], vec![]];
    let item = |id: &str, title: &str, done: bool| json!({"id":id,"title":title,"done":done});
    let mut sched: Vec<Value> = (0..50).map(|i| item("", &format!("T{i}"), i % 2 == 0)).collect();
    sched.insert(0, json!({"id":"a1","title":"y".repeat(300),"done":false,"when":"w".repeat(100)}));
    sched.insert(1, item("a1", "duplicate id", false));
    sched.insert(2, item("made-up", "unknown id", false));
    let settled: Vec<Value> = (0..50).map(|i| item("", &format!("S{i}"), true)).collect();
    // Settled in an answer is ignored (the Goals shadow writes it).
    // A `todo` key (To do is Goals' now) is ignored.
    let answer = json!({"todo":[item("", "Stray To do", false)],"scheduled":sched,"settled":settled,"jobs":[]});
    let p = validate(&answer.to_string(), &current, &[], &[], "").unwrap();
    assert_eq!((p.lists[0].len(), p.lists[1].len()), (LIST_CAP[0], 0));
    assert_eq!(p.lists[0][0].id, "a1");
    assert_eq!(p.lists[0][0].title.chars().count(), TITLE_CHARS);
    assert_eq!(p.lists[0][0].when.as_ref().unwrap().chars().count(), WHEN_CHARS);
    assert_eq!(p.lists[0][1].id, "", "a repeated id is a new item");
    assert_eq!(p.lists[0][2].id, "", "an unknown id is a new item");
    let first_done = p.lists[0].iter().position(|i| i.done).unwrap();
    assert!(p.lists[0][first_done..].iter().all(|i| i.done), "done items sink below open ones");
    assert!(!p.lists.iter().flatten().any(|i| i.title == "Stray To do"));
    // Bad values are rejected whole.
    for bad in [
        json!({"scheduled":[{"id":"","title":"  ","done":false}],"settled":[],"jobs":[]}),
        json!({"scheduled":[{"id":"","title":"t","done":"yes"}],"settled":[],"jobs":[]}),
        json!({"todo":[],"settled":[],"jobs":[]}),
        json!([1,2]),
    ] {
        assert!(validate(&bad.to_string(), &current, &[], &[], "").is_err(), "{bad}");
    }
}

/// A job entry in the answer's full shape.
fn job_answer(island: &str, over: Value) -> Value {
    let mut j = json!({"island": island, "title": "", "state": "running", "stage": "Working", "summary": "Working on it",
        "steps": [], "progress": {"done": 0, "total": 0, "unit": ""}, "parts": [], "facts": [], "image": "", "open": "", "icon": "", "clear": []});
    for (k, v) in over.as_object().unwrap() { j[k] = v.clone(); }
    j
}

fn job(id: &str, thread: Option<&str>, island: Value) -> Job {
    Job { id: id.into(), thread: thread.map(str::to_owned), island: island.as_object().unwrap().clone() }
}

fn png(dir: &Path, name: &str) -> String {
    let p = dir.join(name);
    std::fs::write(&p, b"\x89PNG\r\n\x1a\n0000000000").unwrap();
    p.to_string_lossy().into_owned()
}

#[test]
fn job_islands_are_checked_against_the_style_doc_and_what_the_pass_knows() {
    let d = tempfile::tempdir().unwrap();
    let (shot, old, gone) = (png(d.path(), "compare.png"), png(d.path(), "old.png"), d.path().join("gone.png").to_string_lossy().into_owned());
    let jobs = vec![
        job("site", None, json!({"title": "Arsenal site", "state": "running", "image": old})),
        job("bound", Some("t-bound"), json!({"title": "Bound job"})),
        job("phone", None, json!({"title": "iPhone", "state": "waiting", "attention": "question"})),
        job("ipad", None, json!({"title": "iPad sale", "state": "done"})),
    ];
    let conversation = "Heimdall called run {\"cmd\": \"python3 serve.py # http://localhost:8765/arsenal/\"}";
    let steps: Vec<Value> = (0..9).map(|i| json!({"title": format!("Step {i}"), "state": if i < 3 { "done" } else if i == 3 { "now" } else { "todo" }})).collect();
    let a = json!({"scheduled":[],"settled":[],"jobs":[
        job_answer("site", json!({"title": "Arsenal website rebuild project", "state": "running", "stage": "Running the model tests",
            "summary": format!("Three tests running\nthen {}", "and more ".repeat(30)), "steps": steps, "image": shot,
            "open": "http://localhost:8765/arsenal/", "progress": {"done": 3, "total": 10, "unit": "tests"},
            "parts": [
                {"type": "bars", "label": "", "value": 0, "unit": "score", "items": [{"label": "DDColor", "value": 0.82}, {"label": "", "value": 1}], "points": []},
                {"type": "stat", "label": "SNR", "value": 41.2, "unit": "dB", "items": [], "points": []},
                {"type": "line", "label": "", "value": 0, "unit": "%", "items": [], "points": [1]},
                {"type": "stat", "label": "", "value": 3, "unit": "", "items": [], "points": []},
                {"type": "stat", "label": "", "value": 4, "unit": "", "items": [], "points": []}],
            "icon": "<svg viewBox=\"0 0 24 24\"><rect x=\"2\" y=\"6\" width=\"20\" height=\"12\" rx=\"2\"/></svg>",
            "facts": [{"label": "Port", "value": "8765"}, {"label": "", "value": "x"}]})),
        job_answer("site", json!({"stage": "Duplicate"})),
        job_answer("bound", json!({"stage": "Invented", "state": "done"})),
        job_answer("phone", json!({"state": "done", "stage": "Your test"})),
        job_answer("ipad", json!({"title": "iPad sale", "state": "done", "stage": "Live", "image": gone, "open": "localhost"})),
        job_answer("nobody", json!({}))]});
    let p = validate(&a.to_string(), &[vec![], vec![]], &jobs, &[shot.clone()], conversation).unwrap();
    assert_eq!(p.jobs.iter().map(|u| u.island.as_str()).collect::<Vec<_>>(), ["site", "phone", "ipad"], "unbound, known, once each: a bound island is never rebuilt");
    let site = &p.jobs[0];
    assert_eq!(site.title, None, "a good current title stays");
    assert_eq!(site.state.as_deref(), Some("running"));
    assert_eq!(site.stage.as_deref(), Some("Running the"));
    let summary = site.summary.as_ref().unwrap();
    assert!(summary.chars().count() <= SUMMARY_CHARS && !summary.contains('\n') && summary.starts_with("Three tests running then"));
    let steps = site.steps.as_ref().unwrap();
    assert_eq!(steps.len(), STEPS_CAP);
    assert_eq!(steps[0], ("Step 2".to_string(), "done".to_string()), "the oldest done steps go first");
    assert_eq!(steps[1].1, "now");
    assert_eq!(site.image.as_deref(), Some(shot.as_str()));
    assert_eq!(site.open.as_deref(), Some("http://localhost:8765/arsenal/"));
    assert_eq!(site.progress, Some((3.0, 10.0, "tests".into())));
    assert_eq!(site.parts, vec![
        json!({"type": "bars", "items": [{"label": "DDColor", "value": 0.82}], "unit": "score", OWN_PART: true}),
        json!({"type": "stat", "value": "41.2", "unit": "dB SNR", OWN_PART: true}),
        json!({"type": "stat", "value": "3", "unit": "", OWN_PART: true}),
    ], "a one-point line shows nothing; three parts at most");
    assert!(site.icon.as_deref().is_some_and(|i| i.starts_with("<svg")));
    assert_eq!(site.facts, Some(vec![("Port".to_string(), "8765".to_string())]));
    assert_eq!(p.jobs[1].state, None, "an open question's state belongs to the binding");
    let ipad = &p.jobs[2];
    assert_eq!((ipad.state.as_deref(), ipad.image.as_ref(), ipad.open.as_ref()), (Some("done"), None, None), "no picture that is not there, no bare host");
    // A picture the conversation did not name is refused even when it exists; the island's own one may stay.
    let other = png(d.path(), "other.png");
    let a = json!({"scheduled":[],"settled":[],"jobs":[job_answer("site", json!({"image": other})), job_answer("ipad", json!({"image": other}))]});
    let p = validate(&a.to_string(), &[vec![], vec![]], &jobs, &[], "").unwrap();
    assert_eq!((p.jobs[0].image.as_ref(), p.jobs[1].image.as_ref()), (None, None));
    let p = validate(&a.to_string(), &[vec![], vec![]], &jobs, &[other.clone()], "").unwrap();
    assert_eq!(p.jobs[1].image.as_deref(), Some(other.as_str()), "the conversation's pictures");
    let a = json!({"scheduled":[],"settled":[],"jobs":[job_answer("site", json!({"image": old}))]});
    assert_eq!(validate(&a.to_string(), &[vec![], vec![]], &jobs, &[], "").unwrap().jobs[0].image.as_deref(), Some(old.as_str()));
    // A malformed entry rejects the answer.
    let bad = json!({"scheduled":[],"settled":[],"jobs":[{"stage": "x"}]});
    assert!(validate(&bad.to_string(), &[vec![], vec![]], &jobs, &[], "").is_err());
}

#[test]
fn open_targets_must_be_named_and_safe() {
    let d = tempfile::tempdir().unwrap();
    let app = d.path().join("Tool.app");
    std::fs::create_dir_all(&app).unwrap();
    let script = d.path().join("run.sh");
    std::fs::write(&script, "#!/bin/sh").unwrap();
    let page = d.path().join("compare.html");
    std::fs::write(&page, "<html>").unwrap();
    let (app, script, page) = (app.to_string_lossy().into_owned(), script.to_string_lossy().into_owned(), page.to_string_lossy().into_owned());
    let jobs = vec![job("site", None, json!({"title": "site", "state": "running"})), job("ipad", None, json!({"title": "iPad sale"}))];
    let work = format!("Heimdall: wrote {page}\n  run answered: open {app}; {script} /tmp/clipped…");
    let answer = |island: &str, over: Value| json!({"scheduled":[],"settled":[],"jobs":[job_answer(island, over)]}).to_string();
    let none: Lists = [vec![], vec![]];
    let site = |over: Value, conv: &str| validate(&answer("site", over), &none, &jobs, &[], conv).unwrap().jobs.remove(0);
    let u = site(json!({"state": "waiting", "title": "Arsenal site", "open": page}), &work);
    assert_eq!(u.state.as_deref(), Some("waiting"), "no thread: its state is the refresher's");
    assert_eq!(u.title.as_deref(), Some("Arsenal site"), "an id-like title is replaced");
    assert_eq!(u.open.as_deref(), Some(page.as_str()), "a page the conversation named");
    assert_eq!(site(json!({"open": app}), &work).open, None, "an app would launch");
    assert_eq!(site(json!({"open": script}), &work).open, None, "a script would run");
    assert_eq!(site(json!({"open": "https://example.com/elsewhere"}), &work).open, None, "not named by the work");
    assert_eq!(site(json!({"open": "https://example.com/elsewhere"}), "User: see https://example.com/elsewhere").open.as_deref(), Some("https://example.com/elsewhere"));
    assert_eq!(site(json!({"open": "https://example.com/else"}), "User: see https://example.com/elsewhere").open, None, "a prefix is not a name");
    assert_eq!(site(json!({"open": "/tmp/clipped…"}), &work).open, None, "a clipped path");
    let link = format!("{}/Link", d.path().display());
    std::os::unix::fs::symlink(&app, &link).unwrap();
    assert!(!openable(&link), "a link to an app");
    assert!(openable(d.path().to_str().unwrap()) && openable(&page) && !openable(&format!("{page}x")));
    let u = site(json!({"state": "done", "clear": ["steps", "facts", "image"], "facts": [{"label": "Took", "value": "2 h"}]}), "");
    assert_eq!((u.state.as_deref(), u.clear.clone()), (Some("done"), vec!["steps", "image"]), "facts were replaced, not cleared");
    assert_eq!(site(json!({"open": format!("**{page}**")}), &format!("see **{page}**")).open, None, "the stars are not part of it");
    assert_eq!(site(json!({"open": "http://localhost:8765/arsenal/"}), "Open **http://localhost:8765/arsenal/**: done").open.as_deref(), Some("http://localhost:8765/arsenal/"));
    let dotted = d.path().join("shots.v1.2");
    std::fs::create_dir_all(&dotted).unwrap();
    assert!(openable(dotted.to_str().unwrap()) && !openable(&app));
    let u = validate(&answer("ipad", json!({"state": "done", "steps": [], "facts": []})), &none, &jobs, &[], "").unwrap().jobs.remove(0);
    assert_eq!((u.state.as_deref(), u.steps.as_ref(), u.facts.as_ref()), (Some("done"), None, None), "[] keeps");
}

#[test]
fn a_slow_pass_never_overwrites_what_changed_meanwhile() {
    let d = tempfile::tempdir().unwrap();
    let root = Root(d.path().to_path_buf());
    let seen = json!({"title": "iPad sale", "state": "running", "stage": "Listing", "summary": "Listing it", "progress": 0.4});
    root.put("ipad", seen.as_object().unwrap()).unwrap();
    let jobs = vec![job("ipad", None, seen.clone())];
    // During the pass the agent marks it done with a new summary, and a question arrives.
    root.put("ipad", json!({"state": "done", "summary": "Sold for 5 500 zł", "attention": "question"}).as_object().unwrap()).unwrap();
    let u = JobUpdate { island: "ipad".into(), state: Some("running".into()), stage: Some("Live".into()), summary: Some("Listed".into()),
        steps: Some(vec![("List it".into(), "now".into())]), ..Default::default() };
    let plan = Plan { lists: Default::default(), jobs: vec![u], keep: vec![] };
    apply(&root, &Default::default(), &plan, &jobs).unwrap();
    let o = root.read("ipad").unwrap();
    assert_eq!((o["state"].as_str(), o["summary"].as_str()), (Some("done"), Some("Sold for 5 500 zł")), "theirs");
    assert_eq!(o["stage"], "Live", "an untouched key takes the model's change");
    assert_eq!(o["steps"], json!([{"title": "List it", "state": "now"}]));
    assert_eq!(o["progress"], 0.4, "progress the agent set is not the refresher's to clear");
}

#[test]
fn a_rebuilt_island_keeps_other_writers_parts_and_drops_a_broken_picture() {
    let o = json!({"title": "Voices", "state": "running", "image": "/nowhere/none", "steps": [{"title": "Old phase", "state": "now"}],
        "thread": {"id": "t", "title": "Voices"},
        "parts": [{"type": "text", "value": "Which voice?", "t3q": true}, {"type": "button", "label": "Yes", "action": "t3q:r:0"},
                  {"type": "bars", "items": [], OWN_PART: true}]});
    let o = o.as_object().unwrap();
    let u = JobUpdate {
        island: "voices".into(), title: Some("Male voices".into()), state: Some("running".into()), stage: Some("Rebuilding".into()),
        summary: Some("Rebuilding the film".into()), steps: Some(vec![("Colour all frames".into(), "now".into()), ("Your look".into(), "todo".into())]),
        progress: Some((1.0, 4.0, "steps".into())), parts: vec![json!({"type": "bars", "items": [{"label": "A", "value": 2.0}], "unit": "s", OWN_PART: true})],
        icon: Some("film.stack".into()), ..Default::default()
    };
    let p = job_patch(o, &u);
    assert_eq!(p["steps"], json!([{"title": "Colour all frames", "state": "now"}, {"title": "Your look", "state": "todo"}]), "the new phase replaces the old one");
    assert_eq!((p["progress"].clone(), p[OWN_PROGRESS].clone()), (json!(0.25), json!(0.25)));
    assert_eq!(p["thread"]["title"], "Male voices");
    assert!(p["image"].is_null() && p.contains_key("image"), "a picture that is gone is removed");
    let parts = p["parts"].as_array().unwrap();
    assert_eq!(parts.iter().map(|x| x["type"].as_str().unwrap()).collect::<Vec<_>>(), ["progress", "bars", "text", "button"]);
    assert_eq!(p["icon"], "film.stack");
    assert!(parts[2]["t3q"] == true && parts[3]["action"] == "t3q:r:0", "the binding's question stays");
    // Done: no progress; nothing to say twice.
    let done = JobUpdate { state: Some("done".into()), progress: Some((4.0, 4.0, "".into())), ..u.clone() };
    assert!(job_patch(o, &done).get("progress").is_none_or(Value::is_null), "no progress once done");
    let mut ours = o.clone();
    ours.insert("progress".into(), json!(0.5));
    ours.insert(OWN_PROGRESS.into(), json!(0.5));
    let p = job_patch(&ours, &JobUpdate { progress: None, ..u.clone() });
    assert!(p["progress"].is_null() && p.contains_key("progress"), "the refresher's own progress goes when the count stops");
    ours.insert("progress".into(), json!(0.7));
    let p = job_patch(&ours, &JobUpdate { progress: None, ..u.clone() });
    assert!(!p.contains_key("progress") && p[OWN_PROGRESS].is_null(), "the agent's newer progress stays; the stale mark goes");
    let p = job_patch(o, &JobUpdate { clear: vec!["steps"], steps: None, ..u.clone() });
    assert!(p["steps"].is_null() && p.contains_key("steps"));
    let mut after = o.clone();
    crate::islands::files::merge(&mut after, &job_patch(o, &u));
    assert!(job_patch(&after, &u).is_empty(), "an unchanged island is not rewritten");
}

#[test]
fn a_tick_owner_made_during_the_pass_wins_and_missing_lists_are_created() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root(dir.path().into());
    let mut o = Map::new();
    o.insert("items".into(), json!([{"id":"a1","title":"A","done":false}]));
    root.put("scheduled", &o).unwrap();
    let before: Lists = [vec![json!({"id":"a1","title":"A","done":false}).as_object().unwrap().clone()], vec![]];
    // The user ticks after the pass read the lists, and the agent adds an item with `plan_edit`.
    root.patch("scheduled", |o| {
        o["items"][0]["done"] = json!(true);
        o["items"].as_array_mut().unwrap().insert(0, json!({"id":"e1","title":"Added meanwhile","done":false}));
    }).unwrap();
    let plan = Plan { lists: [vec![Item { id: "a1".into(), title: "A".into(), done: false, when: None, detail: None }], vec![]], jobs: vec![], keep: vec![] };
    apply(&root, &before, &plan, &[]).unwrap();
    let sched = items(&root, "scheduled");
    assert_eq!(sched.len(), 2);
    assert_eq!(sched[0]["id"], "e1", "an item added during the pass survives");
    assert_eq!(sched[1]["done"], true, "User's tick wins");
    assert!(root.read("settled").is_none(), "Settled is the Goals shadow's to create");
    assert!(root.read("todo").is_none(), "To do is never created");
}

#[test]
fn the_request_follows_the_haiku_5_5_shape() {
    let b = request_body("hi");
    assert_eq!(b["model"], "claude-haiku-5-5");
    assert_eq!(b["max_tokens"], 128_000, "the model's maximum: no cap of our own (item 28)");
    assert_eq!(b["thinking"]["type"], "adaptive");
    assert_eq!(b["output_config"]["effort"], "high");
    let s = &b["output_config"]["format"]["schema"];
    assert_eq!(s["required"], json!(["scheduled","jobs"]), "Settled is the Goals shadow's");
    // Structured outputs take no length or count constraints; every object closes its properties.
    let text = s.to_string();
    for banned in ["minLength", "maxLength", "minItems", "maxItems", "minimum", "maximum"] { assert!(!text.contains(banned), "{banned}"); }
    assert_eq!(s["$defs"]["item"]["additionalProperties"], false);
    assert_eq!(b["messages"].as_array().unwrap().len(), 1, "ends with the user turn: no prefill");
}

#[test]
fn the_session_pointer_is_followed_only_inside_the_data_folder() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path();
    assert_eq!(session_path(data), data.join("pi-session.jsonl"));
    std::fs::create_dir_all(data.join("agent")).unwrap();
    std::fs::write(data.join("agent/active-session.json"), serde_json::to_vec(&data.join("s.jsonl")).unwrap()).unwrap();
    assert_eq!(session_path(data), data.join("s.jsonl"));
    std::fs::write(data.join("agent/active-session.json"), serde_json::to_vec("/etc/passwd").unwrap()).unwrap();
    assert_eq!(session_path(data), data.join("pi-session.jsonl"));
}

/// Bound islands are code's and the island agent's (30 Oct): the pass reads no thread, shows each bound island as one
/// read-only line, never rebuilds it, and a bound island's churn (stage, history, counters) is not news.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bound_islands_are_one_read_only_line_never_rebuilt_and_their_churn_is_not_news() {
    let (url, fake) = serve().await;
    let l = lab(&url, Some("k"), Duration::from_secs(10));
    seed(&l.root);
    for (id, thread) in [("job-a", Some("t-a")), ("ipad", None)] {
        let mut o = Map::new();
        o.insert("title".into(), json!(if id == "job-a" { "Release" } else { "iPad sale" }));
        if let Some(t) = thread { o.insert("thread".into(), json!({"id": t, "title": "x"})); }
        o.insert("stage".into(), json!("Building"));
        o.insert("steps".into(), json!([{"title": "Preparing  3 min", "state": "now"}]));
        l.root.put(id, &o).unwrap();
    }
    let mut answer: Value = serde_json::from_str(&echo(&l.root)).unwrap();
    answer["jobs"] = json!([
        job_answer("job-a", json!({"stage": "Invented", "summary": "Invented", "state": "done"})),
        job_answer("ipad", json!({"stage": "Listed", "summary": "Listed on the site"}))]);
    *fake.mode.lock().unwrap() = Mode::Answer(answer.to_string());
    let shared = Shared::default();
    pass(&l.cfg, &shared).await.unwrap();
    let a = l.root.read("job-a").unwrap();
    assert_eq!((a["stage"].as_str(), a.get("summary"), a.get("state")), (Some("Building"), None, None), "a bound island is never rebuilt");
    assert_eq!(l.root.read("ipad").unwrap()["stage"], "Listed", "an unbound one is");
    let prompt = user_text(&fake.body.lock().unwrap());
    assert!(prompt.contains("<thread_islands") && prompt.contains("- job-a · Release · running · Building"), "{prompt}");
    assert!(!prompt.contains("\"island\": \"job-a\"") && !prompt.contains("<threads"), "no thread read, not among the jobs");
    // Code rewrote the bound island's history and stage: nothing new for the plan. Its state changed: news.
    let calls = shared.calls.load(Ordering::Acquire);
    l.root.patch("job-a", |o| { o.insert("steps".into(), json!([{"title": "Testing  1 min", "state": "now"}])); o.insert("stage".into(), json!("Testing")); }).unwrap();
    assert_eq!(pass(&l.cfg, &shared).await.unwrap(), None);
    assert_eq!(shared.calls.load(Ordering::Acquire), calls, "a bound island's churn is not news");
    l.root.patch("job-a", |o| { o.insert("state".into(), json!("done")); }).unwrap();
    let fp_before = shared.last.lock().unwrap().clone().unwrap().fp;
    let g = gather(&l.cfg, None).unwrap().unwrap();
    assert_ne!(g.fingerprint.islands, fp_before.islands, "its state is");
}

/// Opt-in, read-only, no model call: the prompt a pass would send from a copy of a real data folder.
/// `HEIMDALL_PLAN_LAB_DATA=<copy of the data folder> cargo nextest run -p app --bin heimdall -- --ignored real_input`
#[test]
#[ignore]
fn real_input_stays_within_budget() {
    let Some(data) = std::env::var_os("HEIMDALL_PLAN_LAB_DATA") else { return };
    let dir = tempfile::tempdir().unwrap();
    let cfg = Config {
        data: PathBuf::from(data),
        islands: Root(dir.path().into()),
        notes: heimdall_home::home_dir().join("Assistant"),
        endpoint: Arc::new(Endpoint::default),
        enabled: Arc::new(|| true),
    };
    let started = std::time::Instant::now();
    let g = gather(&cfg, None).unwrap().unwrap();
    let prompt = user_prompt(&g).text();
    eprintln!("gather {:?}; summary {} chars; {} conversation lines; prompt {} chars (~{} tokens)",
        started.elapsed(), g.summary.as_ref().map_or(0, |s| s.len()), g.conversation.len(), prompt.len(), prompt.len() / 4);
    std::fs::write(dir.path().join("prompt.txt"), &prompt).unwrap();
    assert!(prompt.len() + SYSTEM.len() < 800_000);
}

fn append(session: &Path, line: Value) {
    writeln!(std::fs::OpenOptions::new().append(true).open(session).unwrap(), "{line}").unwrap();
}

/// Review finding 1: the user's real Settled list (plain strings, no `preset`/`list`) must survive the first pass.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn owner_s_old_settled_list_survives_the_first_pass_and_is_migrated() {
    // Since 2 Nov Settled is the Goals shadow's: a pass never shows it to the model and never writes it.
    let (url, fake) = serve().await;
    let l = lab(&url, Some("k"), Duration::from_secs(10));
    l.root.ensure().unwrap();
    std::fs::write(l.root.file("settled"), crate::islands::plan::REAL_SETTLED).unwrap();
    let answer = json!({"scheduled": [{"id": "", "title": "Heimdall: night pass review", "when": in_days(1), "done": false}],
                        "settled": [], "jobs": []});
    *fake.mode.lock().unwrap() = Mode::Answer(answer.to_string());
    let notice = pass(&l.cfg, &Shared::default()).await.unwrap().unwrap();
    assert_eq!(notice, proposed_notice("Heimdall: night pass review"));
    assert_eq!(lists::items(&l.root.read("settled").unwrap()).len(), 9, "Settled untouched");
    assert_eq!(proposals_of(&l.root).len(), 1, "the proposal went to Scheduled, which had no file yet");
    let prompt = user_text(&fake.body.lock().unwrap());
    assert!(!prompt.contains("Repair OCR and translation models"), "Settled is not shown");
}

/// Review finding 2: the refresher's own notice (and bookkeeping entries) are not "something new".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_refresher_s_own_notice_does_not_trigger_the_next_pass() {
    let (url, fake) = serve().await;
    *fake.mode.lock().unwrap() = Mode::Answer(answer_proposing());
    let l = lab(&url, Some("k"), Duration::from_secs(10));
    seed(&l.root);
    let shared = Shared::default();
    let notice = pass(&l.cfg, &shared).await.unwrap().unwrap();
    assert_eq!(notice, proposed_notice(PROPOSED));
    // Delivered as context: appended to the session with its header, plus a receipt; no turn.
    append(&l.session, json!({"type":"message","message":{"role":"user","content":[{"type":"text","text":format!("[Heimdall notice: {NOTICE_PREFIX}1792630210300]\n{notice}")}]}}));
    append(&l.session, json!({"type":"custom","customType":"heimdall_receipt","data":{}}));
    assert_eq!(pass(&l.cfg, &shared).await.unwrap(), None);
    assert_eq!(fake.hits.load(Ordering::SeqCst), 1, "no model call for its own notice");
    // A real notice coalesced beside it is new.
    *fake.mode.lock().unwrap() = Mode::Answer(echo(&l.root));
    append(&l.session, json!({"type":"message","message":{"role":"user","content":[{"type":"text","text":format!("[Heimdall notice: {NOTICE_PREFIX}2]\nPlan: x.\n\n[Heimdall notice: terminal:w:r:completed]\nArsenal build finished")}]}}));
    let _ = pass(&l.cfg, &shared).await.unwrap();
    assert_eq!(fake.hits.load(Ordering::SeqCst), 2);
    let prompt = user_text(&fake.body.lock().unwrap());
    assert!(prompt.contains("Arsenal build finished") && !prompt.contains(NOTICE_PREFIX), "its own notices are not model input either");
}

#[test]
fn own_notices_are_cut_and_other_notices_kept() {
    let t = "[Heimdall notice: plan-refresh-1]\nPlan: a.\n[Heimdall notice: ask-1]\nQuestion ask-1: User clicked Yes.\n[Heimdall notice: plan-refresh-2]\nPlan: b.";
    assert_eq!(without_own_notices(t), "[Heimdall notice: ask-1]\nQuestion ask-1: User clicked Yes.\n");
    assert_eq!(without_own_notices("User: hi"), "User: hi");
    assert_eq!(without_own_notices("[Heimdall notice: plan-refresh-9]\nPlan: c.").trim(), "");
}

/// Review finding 8: an unchanged session file is not read again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unchanged_session_file_is_not_read() {
    let (url, fake) = serve().await;
    *fake.mode.lock().unwrap() = Mode::Answer(answer_proposing());
    let l = lab(&url, Some("k"), Duration::from_secs(10));
    seed(&l.root);
    let shared = Shared::default();
    pass(&l.cfg, &shared).await.unwrap();
    assert_eq!(shared.reads.load(Ordering::SeqCst), 1);
    for _ in 0..3 { assert_eq!(pass(&l.cfg, &shared).await.unwrap(), None); }
    assert_eq!(shared.reads.load(Ordering::SeqCst), 1, "size and mtime unchanged: no read");
    append(&l.session, json!({"type":"custom","customType":"heimdall_receipt","data":{}}));
    assert_eq!(pass(&l.cfg, &shared).await.unwrap(), None);
    assert_eq!(shared.reads.load(Ordering::SeqCst), 2, "grown: read once, nothing said, no model call");
    assert_eq!(pass(&l.cfg, &shared).await.unwrap(), None);
    assert_eq!(shared.reads.load(Ordering::SeqCst), 2);
    assert_eq!(fake.hits.load(Ordering::SeqCst), 1);
}

/// Review finding 2: model passes stay at least the gap apart (5 min live: 12 an hour at most), however often
/// changes and requests arrive; requests inside the gap coalesce into one pass when it ends.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn model_passes_stay_a_gap_apart_and_requests_inside_it_coalesce() {
    let (url, fake) = serve().await;
    *fake.mode.lock().unwrap() = Mode::Answer(answer_proposing());
    let l = lab(&url, Some("k"), Duration::from_secs(10));
    seed(&l.root);
    let r = Refresher::default();
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let task = r.spawn(l.cfg.clone(), Arc::new(|_: &str| Ok(())), Duration::from_secs(3600), Duration::from_millis(1500), stop_rx);
    assert_eq!(r.request(), STARTED);
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(fake.hits.load(Ordering::SeqCst), 1);
    *fake.mode.lock().unwrap() = Mode::Answer(echo(&l.root));
    for n in 0..5 {
        append(&l.session, json!({"type":"message","message":{"role":"user","content":format!("change {n}")}}));
        assert!(r.request().contains("refreshes in about 1 min"), "{}", r.request());
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(fake.hits.load(Ordering::SeqCst), 1, "inside the gap: no pass");
    tokio::time::sleep(Duration::from_millis(1600)).await;
    assert_eq!(fake.hits.load(Ordering::SeqCst), 2, "one pass when the gap ended");
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(fake.hits.load(Ordering::SeqCst), 2, "the requests were served by that one pass");
    stop_tx.send_replace(true);
    tokio::time::timeout(Duration::from_secs(2), task).await.unwrap().unwrap();
}

/// Replaces `edits_made_during_a_pass_win_over_the_model_s_answer` (the model no longer edits items): edits, ticks,
/// `plan_edit` removes and moves made while the model thinks stay as they were made, whatever the answer says about
/// those items; the pass still proposes its new item.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_edit_made_during_a_pass_stays_and_the_pass_only_proposes() {
    let (url, fake) = serve().await;
    let l = lab(&url, Some("k"), Duration::from_secs(10));
    seed(&l.root);
    lists::edit_list(&l.root, "scheduled", |its| { its.push(json!({"id":"u1","title":"Vireo: icon","done":false}).as_object().unwrap().clone()); Ok(()) }).unwrap();
    // The answer, written from the snapshot: rewords w1 and a1, ticks r1, drops u1, proposes one.
    let answer = json!({
        "scheduled": [{"id":"w1","title":"Delete the old caches","done":false},
                      {"id":"a1","title":"Arsenal: push the branch","done":false},
                      {"id":"r1","title":"Heimdall: fix underrun","done":true},
                      {"id":"","title":"Islands: review fixes","when":in_days(1),"done":false}],
        "jobs": []
    });
    *fake.mode.lock().unwrap() = Mode::Slow(Duration::from_millis(700), answer.to_string());
    let root = l.root.clone();
    let meanwhile = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(250)).await;
        tokio::task::spawn_blocking(move || {
            // The agent gives w1 a time, the user ticks r1; the agent removes a1 and moves s1 back to Scheduled.
            assert!(lists::change_item(&root, "scheduled", "w1", json!({"when":"Tomorrow"}).as_object().unwrap()).unwrap().is_some());
            assert!(lists::tick(&root, "scheduled", "r1", true).unwrap().is_some());
            lists::edit_list(&root, "scheduled", |its| { its.retain(|i| i["id"] != "a1"); Ok(()) }).unwrap();
            assert!(lists::move_item(&root, "settled", "s1", "scheduled", json!({"when":"Later"}).as_object().unwrap()).unwrap().is_some());
        }).await.unwrap();
    });
    let notice = pass(&l.cfg, &Shared::default()).await.unwrap().unwrap();
    meanwhile.await.unwrap();
    let sched = items(&l.root, "scheduled");
    let by = |list: &[Value], id: &str| list.iter().find(|i| i["id"] == id).cloned();
    let w1 = by(&sched, "w1").expect("the edited item stays in Scheduled");
    assert_eq!((w1["when"].as_str(), w1["title"].as_str()), (Some("Tomorrow"), Some("Delete old caches")), "the agent's edit wins, the model's rewording is ignored");
    assert_eq!(by(&sched, "r1").expect("the ticked item stays")["done"].as_bool(), Some(true));
    assert!(by(&sched, "a1").is_none() && by(&items(&l.root, "settled"), "a1").is_none(), "a removed item is not brought back");
    assert_eq!(by(&sched, "s1").expect("the moved item stays where it was moved")["when"], "Later");
    assert!(by(&sched, "u1").is_some(), "an item the answer leaves out is not removed");
    assert!(items(&l.root, "settled").is_empty(), "Settled is the Goals shadow's");
    assert_eq!(l.root.read("scheduled").unwrap()["unknown_key"], "kept");
    assert_eq!(proposals_of(&l.root).iter().map(|p| p["title"].as_str().unwrap()).collect::<Vec<_>>(), ["Islands: review fixes"]);
    assert_eq!(notice, proposed_notice("Islands: review fixes"), "only what the refresher did");
}

/// Opt-in lab run over a recorded session and a copy of the islands folder: writes the prompt
/// (`prompt.txt`, the system prompt first) to `HEIMDALL_PLAN_LAB_OUT`; then takes the answer from
/// `HEIMDALL_PLAN_LAB_ANSWER` when set, else from ONE real Claude Haiku 5.5 call when HEIMDALL_ANTHROPIC_API_KEY is
/// set (else it stops after the prompt), and validates and applies it to the islands copy.
/// HEIMDALL_PLAN_LAB_DATA=<data copy> HEIMDALL_PLAN_LAB_ISLANDS=<islands copy>
/// HEIMDALL_PLAN_LAB_NOTES=<notes> HEIMDALL_PLAN_LAB_OUT=<dir> [HEIMDALL_PLAN_LAB_ANSWER=<answer.json>]
/// cargo test -p app --bin heimdall -- --ignored lab_rich_islands --nocapture
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn lab_rich_islands() {
    let var = |k: &str| std::env::var(k).map(PathBuf::from).unwrap_or_else(|_| panic!("{k} is not set"));
    let (data, islands, notes, out) = (var("HEIMDALL_PLAN_LAB_DATA"), var("HEIMDALL_PLAN_LAB_ISLANDS"), var("HEIMDALL_PLAN_LAB_NOTES"), var("HEIMDALL_PLAN_LAB_OUT"));
    std::fs::create_dir_all(&out).unwrap();
    let root = Root(islands);
    let cfg = Config { data, islands: root.clone(), notes, endpoint: Arc::new(Endpoint::anthropic), enabled: Arc::new(|| true) };
    let g = gather(&cfg, None).unwrap().unwrap();
    let prompt = user_prompt(&g);
    std::fs::write(out.join("prompt.txt"), format!("{SYSTEM}\n\n=====\n\n{}", prompt.text())).unwrap();
    std::fs::write(out.join("schema.json"), serde_json::to_string_pretty(&schema()).unwrap()).unwrap();
    eprintln!("prompt {} chars (~{} tokens with the system prompt)", prompt.len(), (prompt.len() + SYSTEM.len()) / 4);
    let answer = if let Ok(path) = std::env::var("HEIMDALL_PLAN_LAB_ANSWER") {
        std::fs::read_to_string(path).unwrap()
    } else {
        let e = Endpoint::anthropic();
        if e.key.is_none() { eprintln!("no answer file and no key: prompt only"); return };
        let started = std::time::Instant::now();
        let answer = call_model(&e, &prompt).await.unwrap().text;
        eprintln!("model answered in {:.1} s", started.elapsed().as_secs_f64());
        answer
    };
    std::fs::write(out.join("answer.json"), &answer).unwrap();
    let plan = validate(&answer, &g.lists, &g.jobs, &g.pictures, &g.conversation.join("\n")).unwrap();
    std::fs::write(out.join("validated.txt"), format!("{:#?}", plan.jobs)).unwrap();
    let diff = apply(&root, &g.lists, &plan, &g.jobs).unwrap();
    eprintln!("plan: {:?}", diff_text(&diff));
    for (id, o) in root.all() {
        if is_job(&o) { eprintln!("{id}: {}", serde_json::to_string(&o).unwrap()); }
    }
}

/// The worked examples in Haiku's prompt are what a pass may write: every JSON block parses, a job entry has exactly
/// the answer's keys and passes validation unchanged in substance, icons are drawable, an item has exactly the item's
/// keys, and nothing teaches To do (Goals' since 31 Oct).
#[test]
fn the_prompt_s_worked_examples_are_valid_answers() {
    // Both editions' examples (docs/editions.md); SYSTEM carries this build's.
    let personal = include_str!("../prompt/plan-refresher-examples.md");
    let public = include_str!("../prompt/public/plan-refresher-examples.md");
    assert!(SYSTEM.ends_with(if crate::edition::PUBLIC { public } else { personal }) && SYSTEM.contains("# Worked examples"));
    for text in [personal, public] {
        worked_examples_are_valid(text);
    }
}

/// The public edition's island prompt carries nothing of the user's: his name, quotes, paths, listings or projects.
#[test]
fn the_public_island_prompt_carries_nothing_of_owners() {
    for text in [include_str!("../prompt/public/plan-refresher.md"), include_str!("../prompt/public/plan-refresher-examples.md")] {
        for private in ["Vault", "Arsenal", "Allegro", "allegrolokalnie", "iPad", "zł", "night pass", "B-29", "DDColor"] {
            assert!(!text.contains(private), "public island prompt carries {private:?}");
        }
    }
}

fn worked_examples_are_valid(text: &str) {
    let keys: Vec<String> = schema()["properties"]["jobs"]["items"]["required"].as_array().unwrap().iter().map(|k| k.as_str().unwrap().to_owned()).collect();
    let item_keys: Vec<String> = schema()["$defs"]["item"]["required"].as_array().unwrap().iter().map(|k| k.as_str().unwrap().to_owned()).collect();
    let mut jobs = 0;
    assert!(!text.contains("To do") && !text.contains("\"todo\":") && !text.contains("\"group\""), "no example teaches To do");
    for block in text.split("```json").skip(1) {
        let v: Value = serde_json::from_str(block.split("```").next().unwrap()).unwrap_or_else(|e| panic!("bad JSON ({e}): {block}"));
        if v.get("state").is_some() {
            jobs += 1;
            let mut have: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
            have.sort();
            let mut want = keys.clone();
            want.sort();
            assert_eq!(have, want, "an example job has exactly the answer's keys");
            let icon = v["icon"].as_str().unwrap();
            assert!(icon.is_empty() || crate::islands::icon(icon).is_some(), "drawable icon: {icon}");
            let job = job(v["island"].as_str().unwrap(), None, json!({"title": "x"}));
            let u = job_update(&v, &job, &[], "");
            assert!(u.stage.is_some() && u.summary.as_deref().is_some_and(|s| s.chars().count() <= SUMMARY_CHARS), "{v}");
            assert_eq!(u.icon.is_some(), !icon.is_empty());
            assert!(v["steps"].as_array().unwrap().len() <= STEPS_CAP);
        } else {
            let mut have: Vec<&String> = v.as_object().unwrap().keys().collect();
            have.sort();
            let mut want: Vec<&String> = item_keys.iter().collect();
            want.sort();
            assert_eq!(have, want, "an example answer item has exactly the answer's keys: {v}");
        }
    }
    // Thread-bound islands are code's and the island agent's since 30 Oct: the examples rebuild only islands without a thread.
    assert!(jobs >= 2, "{jobs} job examples");
}

/// "A lot of those to-do tasks are so stupid, like plug in the charger, empty the bin... clean up the
/// weird ones." The lists hold real work and decisions; no worked example may teach a chore as an item.
#[test]
fn the_lists_hold_no_chores() {
    let prompt = include_str!("../prompt/plan-refresher.md");
    assert!(prompt.contains("- No chores: the lists hold real work and decisions only."));
    for chore in ["charger", "Trash", "sounds right", "toggle", "mic"] {
        assert!(prompt.contains(chore), "the rule names {chore}");
    }
    let text = include_str!("../prompt/plan-refresher-examples.md");
    for block in text.split("```json").skip(1) {
        let v: Value = serde_json::from_str(block.split("```").next().unwrap()).unwrap();
        let Some(title) = v.get("title").filter(|_| v.get("state").is_none()).and_then(Value::as_str) else { continue };
        let lower = title.to_lowercase();
        let words: Vec<&str> = lower.split(|c: char| !c.is_alphanumeric()).collect();
        let chore = ["charger", "trash", "bin", "toggle", "toggles", "mic"].iter().any(|w| words.contains(w))
            || ["sounds right", "plug in"].iter().any(|p| lower.contains(p));
        assert!(!chore, "a chore taught as an item: {title}");
    }
}

/// To do becomes Goals, kept by another job. The prompt says so in one line and teaches nothing of To
/// do: no To do list in the answer or the input, no groups, quick answers, kinds or folding.
#[test]
fn the_prompt_leaves_to_do_to_goals() {
    let personal = include_str!("../prompt/plan-refresher.md");
    let public = include_str!("../prompt/public/plan-refresher.md");
    // no To do list, Goals the asks. Since 2 Nov Scheduled takes a proposal with a real time, never an item.
    assert!(personal.contains("- Nothing enters Scheduled without the user's yes (2 Nov). Your `scheduled` answer proposes: code takes only new items (id \"\") with a real local date-time in `when`"));
    assert!(public.contains("- Nothing enters Scheduled without the user's yes (2 Nov). Your `scheduled` answer proposes: code takes only new items (id \"\") with a real local date-time in `when`"));
    for text in [personal, public] {
        assert_eq!(text.matches("To do").count(), 0, "no To do list named");
        assert_eq!(text.matches("to-do").count(), 0, "no to-do list named (the 2 Nov prompt dropped the 'never a to-do list' line)");
        assert!(!text.contains("moves the item to Goals"), "a due item stays in Scheduled");
        for gone in ["Keeping To do clean", "Quick answers", "- kind (", "- next (", "\"waiting\"", "\"discussed\"", "group \"running\"", "fold the item", "folded"] {
            assert!(!text.contains(gone), "the prompt still teaches {gone:?}");
        }
    }
    assert!(schema()["properties"].get("todo").is_none() && schema()["$defs"]["item"]["properties"].get("group").is_none());
}

fn plan_of_items(items: Vec<Item>) -> Plan { Plan { lists: [items, vec![]], jobs: vec![], keep: vec![] } }

fn new_item(title: &str, when: Option<&str>) -> Item { Item { id: String::new(), title: title.into(), done: false, when: when.map(str::to_owned), detail: None } }

/// Replaces `items_the_refresher_moves_or_adds_to_settled_arrive_ticked` (nothing moves to Settled any more): an
/// existing promise, the user's tick and the Settled list are exactly as they were after a plan that rewords, unticks,
/// drops and "moves to Settled" them; the file is not even rewritten.
#[test]
fn existing_promises_and_owners_ticks_are_never_changed_by_a_pass() {
    let d = tempfile::tempdir().unwrap();
    let root = Root(d.path().to_path_buf());
    let m1 = json!({"id": "m1", "title": "Merge night", "done": false, "due": 1_900_000_000, "yes": {"by": "click", "at": 1}, "note": {"text": "kept"}});
    let t1 = json!({"id": "t1", "title": "User ticked this", "done": true});
    let s0 = json!({"id": "s0", "title": "Old", "done": false});
    root.put("scheduled", json!({"preset": "todo", "list": "scheduled", "items": [m1, t1], "unknown_key": "kept"}).as_object().unwrap()).unwrap();
    root.put("settled", json!({"preset": "todo", "list": "settled", "items": [s0]}).as_object().unwrap()).unwrap();
    let (sched_file, settled_file) = (std::fs::read(root.file("scheduled")).unwrap(), std::fs::read(root.file("settled")).unwrap());
    let before: Lists = [vec![m1.as_object().unwrap().clone(), t1.as_object().unwrap().clone()], vec![s0.as_object().unwrap().clone()]];
    let it = |id: &str, t: &str, done: bool, when: Option<&str>| Item { id: id.into(), title: t.into(), done, when: when.map(str::to_owned), detail: Some("new words".into()) };
    // The plan rewords m1 and gives it a time, unticks t1, and lists a Settled item (Settled is not in the plan at all).
    let plan = Plan { lists: [vec![it("m1", "Merge the night branch", true, Some("2031-01-01T10:00")), it("t1", "User ticked this", false, None)],
                              vec![it("s0", "Old", true, None), it("m1", "Merge night", true, None), it("", "New thing", true, None)]], jobs: vec![], keep: vec![] };
    let diff = apply(&root, &before, &plan, &[]).unwrap();
    assert_eq!(diff, Diff::default());
    assert_eq!(std::fs::read(root.file("scheduled")).unwrap(), sched_file, "Scheduled is not rewritten");
    assert_eq!(std::fs::read(root.file("settled")).unwrap(), settled_file, "Settled is not rewritten");
    // An answer that drops everything old removes nothing either.
    let diff = apply(&root, &before, &plan_of_items(vec![new_item("Only new", Some(&in_days(1)))]), &[]).unwrap();
    assert_eq!(diff.added, ["Only new"]);
    let sched = items(&root, "scheduled");
    assert_eq!(sched, [m1, t1], "the promises stay, User's tick stays");
}

/// New: an item without a real time is no promise (docs/one-system.md: "no time, no promise") and is dropped.
#[test]
fn an_item_without_a_time_is_no_promise_and_is_dropped() {
    let d = tempfile::tempdir().unwrap();
    let root = Root(d.path().to_path_buf());
    root.put("scheduled", json!({"preset": "todo", "list": "scheduled", "items": [{"id": "a1", "title": "Arsenal: push branch", "done": false}]}).as_object().unwrap()).unwrap();
    let file = std::fs::read(root.file("scheduled")).unwrap();
    let before: Lists = [vec![json!({"id": "a1", "title": "Arsenal: push branch", "done": false}).as_object().unwrap().clone()], vec![]];
    let mut ticked = new_item("Ticked, with a time", Some(&in_days(1)));
    ticked.done = true;
    let mut existing = new_item("Arsenal: push branch", Some(&in_days(1)));
    existing.id = "a1".into();
    let plan = plan_of_items(vec![
        new_item("No time at all", None), new_item("A word", Some("Tonight")), new_item("Tomorrow-ish", Some("tomorrow 10:00")),
        new_item("A day, no hour", Some("2031-01-01")), new_item("Already past", Some("2020-01-01T23:00")), ticked, existing]);
    let diff = apply(&root, &before, &plan, &[]).unwrap();
    assert_eq!(diff, Diff::default());
    assert_eq!(diff_text(&diff), None, "nothing to tell the agent");
    assert_eq!(std::fs::read(root.file("scheduled")).unwrap(), file, "no file change, no queue");
    // One with a real time beside them is the only one that comes through.
    let mut plan = plan;
    plan.lists[0].push(new_item("Ship Vella 1.3", Some(&in_days(3))));
    let diff = apply(&root, &before, &plan, &[]).unwrap();
    assert_eq!(diff.added, ["Ship Vella 1.3"]);
    assert_eq!(proposals_of(&root).len(), 1);
    assert_eq!(items(&root, "scheduled").len(), 1);
}

/// New: a proposal the refresher already made, one queued, one declined (in the record) or a promise already in
/// Scheduled is not proposed twice; a second new one queues behind the one on screen.
#[test]
fn a_proposal_already_made_or_in_the_record_is_not_proposed_twice() {
    let d = tempfile::tempdir().unwrap();
    let root = Root(d.path().to_path_buf());
    root.put("scheduled", json!({"preset": "todo", "list": "scheduled", "items": [{"id": "a1", "title": "Arsenal: push branch", "done": false}]}).as_object().unwrap()).unwrap();
    let apply_new = |titles: &[&str]| apply(&root, &Default::default(), &plan_of_items(titles.iter().map(|t| new_item(t, Some(&in_days(2)))).collect()), &[]).unwrap();
    let first = apply_new(&["Drama 3: pick the better voice"]);
    assert_eq!(first.added, ["Drama 3: pick the better voice"]);
    assert!(first.shown.is_some(), "the first one comes on screen");
    assert_eq!(first.added_ids.len(), 1);
    // The same again (and in other words of case and spacing), and a promise already there.
    assert_eq!(apply_new(&["Drama 3: pick the better voice", "  drama 3:  PICK the better voice ", "Arsenal: push branch"]), Diff::default());
    assert_eq!(proposals_of(&root).len(), 1);
    // A different one queues behind: not on screen, so no line to say.
    let second = apply_new(&["Vella: ship 1.3"]);
    assert_eq!((second.added.len(), second.shown.is_none()), (1, true));
    assert!(!diff_text(&second).unwrap().contains("on screen"));
    let queue = proposals_of(&root);
    assert_eq!(queue.iter().map(|p| (p["title"].as_str().unwrap(), p.get("front").is_some())).collect::<Vec<_>>(),
               [("Drama 3: pick the better voice", true), ("Vella: ship 1.3", false)]);
    // The user says no to the first: it is in the record and never comes back.
    crate::islands::proposals::decline(&root, Some("scheduled"), "Drama 3", now_s()).unwrap();
    assert_eq!(crate::islands::proposals::recorded(&root, "scheduled"), ["Drama 3: pick the better voice"]);
    assert_eq!(apply_new(&["Drama 3: pick the better voice", "Vella: ship 1.3"]), Diff::default());
    assert_eq!(proposals_of(&root).len(), 1, "only the queued one is left");
}

/// Lane 5 review: Pi's context edits apply (history repair drops or replaces entries the agent no longer sees), and a
/// session past the ceiling loses its oldest lines, never its newest.
#[test]
fn the_session_is_read_as_the_agent_sees_it_within_the_ceiling() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.jsonl");
    let mut lines = vec![
        json!({"type":"message","id":"a","parentId":null,"message":{"role":"assistant","content":[{"type":"text","text":"refused reply"}]}}),
        json!({"type":"message","id":"b","parentId":"a","message":{"role":"user","content":"User: before the edit"}}),
        json!({"type":"context_edit","id":"ea","parentId":"b","targetId":"a","replacement":null}),
        json!({"type":"context_edit","id":"eb","parentId":"ea","targetId":"b","replacement":{"content":"User: after the edit"}}),
    ];
    let mut parent = "eb".to_owned();
    for n in 0..400 {
        let id = format!("m{n}");
        lines.push(json!({"type":"message","id":id,"parentId":parent,"message":{"role":"user","content":format!("line {n} {}", "x".repeat(1000))}}));
        parent = id;
    }
    let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
    std::fs::write(&path, &text).unwrap();
    let (_, out, _) = read_session(&path, text.len() as u64).unwrap();
    let all = out.join("\n");
    assert!(!all.contains("refused reply") && !all.contains("before the edit"), "edits applied");
    assert!(all.len() <= CONVERSATION_CEILING && out.last().unwrap().starts_with("Incoming: line 399 "), "newest kept");
    assert!(!all.contains("Incoming: line 0 "), "oldest dropped past the ceiling");
    // Under the ceiling the replaced entry reads as its replacement.
    let short: String = lines[..4].iter().map(|l| format!("{l}\n")).collect();
    std::fs::write(&path, &short).unwrap();
    let (_, out, _) = read_session(&path, short.len() as u64).unwrap();
    assert_eq!(out, vec!["Incoming: User: after the edit".to_owned()]);
}

/// Anthropic compiles the answer schema (and any strict tool schemas) into one grammar and refuses a request whose
/// grammar is too large: "The compiled grammar is too large… Simplify your tool schemas" failed every pass on 22 Oct
/// (20:20 to 23:08). Measured that night against the API with Haiku 5.5 (`lab/refresher-grammar/`): the schema with
/// the item written out in all three lists (3 983 bytes) failed; with the lists sharing it through `$ref` (about
/// 2 800 bytes) it compiles, and still does with one more item written out (3 458 bytes). The request carries no
/// tools; the lists share the item; no optional keys, no unions; the schema stays under 3 200 bytes.
#[test]
fn the_refresher_request_stays_far_under_the_grammar_limits() {
    fn walk(v: &Value, optional: &mut usize, unions: &mut usize) {
        match v {
            Value::Object(o) => {
                if o.contains_key("anyOf") || o.contains_key("oneOf") || o.get("type").is_some_and(Value::is_array) { *unions += 1; }
                if let Some(props) = o.get("properties").and_then(Value::as_object) {
                    let req: HashSet<&str> = o.get("required").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
                    *optional += props.keys().filter(|k| !req.contains(k.as_str())).count();
                    assert_eq!(o.get("additionalProperties"), Some(&json!(false)), "closed objects only: {v}");
                }
                for x in o.values() { walk(x, optional, unions); }
            }
            Value::Array(a) => for x in a { walk(x, optional, unions); },
            _ => {}
        }
    }
    let body = request_body("hello");
    assert!(body.get("tools").is_none() && body.get("tool_choice").is_none(), "the refresher sends no tools");
    let schema = &body["output_config"]["format"]["schema"];
    assert_eq!(schema["properties"]["scheduled"]["items"], json!({"$ref": "#/$defs/item"}));
    assert!(schema["properties"].get("settled").is_none(), "Settled is the Goals shadow's");
    let (mut optional, mut unions) = (0, 0);
    walk(schema, &mut optional, &mut unions);
    assert_eq!(optional, 0, "every key required (empty means none): each optional key roughly doubles part of the grammar");
    assert_eq!(unions, 0, "no union types");
    let bytes = serde_json::to_string(schema).unwrap().len();
    assert!(bytes <= 3_200, "answer schema is {bytes} bytes; 3 983 failed to compile");
}

/// Item 28: the refresher's prompt keeps a byte-identical cached prefix from pass to pass. The clock moved to the
/// end; the conversation goes in whole `CHUNK_LINES` chunks, each complete chunk unchanged when the session grows.
#[test]
fn the_prompt_cache_prefix_survives_a_new_clock_and_a_growing_conversation() {
    let l = lab("http://127.0.0.1:9", None, Duration::from_secs(1));
    seed(&l.root);
    for k in 0..45 { append(&l.session, json!({"type":"message","message":{"role":"user","content":format!("User: line {k}")}})); }
    let g = gather(&l.cfg, None).unwrap().unwrap();
    let at = |h: u32| chrono::TimeZone::with_ymd_and_hms(&chrono::Local, 2026, 10, 24, h, 0, 0).unwrap();
    let (a, b) = (user_prompt_at(&g, at(2)), user_prompt_at(&g, at(3)));
    let (ba, bb) = (request_body(a.clone()), request_body(b.clone()));
    assert_eq!(ba["system"][0]["cache_control"], json!({"type":"ephemeral","ttl":"1h"}));
    assert_eq!(ba["system"], bb["system"]);
    let (ca, cb) = (ba["messages"][0]["content"].as_array().unwrap(), bb["messages"][0]["content"].as_array().unwrap());
    assert_eq!(ca[..ca.len() - 1], cb[..cb.len() - 1], "everything but the last block is the same at another time");
    let last = ca.last().unwrap();
    assert!(last.get("cache_control").is_none() && last["text"].as_str().unwrap().contains("Now: Saturday 2026-10-24 02:00"));
    assert!(last["text"].as_str().unwrap().ends_with("Rewrite the lists and rebuild the job islands now."));
    assert!(!ca[0]["text"].as_str().unwrap().contains("Now:"), "the clock no longer opens the prompt");
    assert!(last["text"].as_str().unwrap().contains("<current_lists>") && last["text"].as_str().unwrap().contains("night-pass-2026-10-22.md"), "live state after the cached stretch");
    // The conversation in chunks: summary, the opening tag, then whole chunks of 40 lines and the newest one.
    let chunks: Vec<&Value> = ca.iter().filter(|c| c["text"].as_str().unwrap().starts_with("Incoming") || c["text"].as_str().unwrap().starts_with("Heimdall")).collect();
    assert!(chunks.len() >= 2 && chunks[0]["text"].as_str().unwrap().lines().count() == CHUNK_LINES, "{}", chunks.len());
    let marked = ca.iter().filter(|c| c.get("cache_control").is_some()).count();
    assert_eq!(marked, 3, "the newest complete chunks and the newest line end cached stretches");
    assert!(ca.iter().rev().nth(1).unwrap().get("cache_control").is_some(), "the cache reaches the newest line");
    // The user says more: every complete chunk is byte-identical, so the next pass reads them from the cache.
    for k in 0..3 { append(&l.session, json!({"type":"message","message":{"role":"user","content":format!("User: later {k}")}})); }
    let g2 = gather(&l.cfg, None).unwrap().unwrap();
    let b2 = request_body(user_prompt_at(&g2, at(3)));
    let c2 = b2["messages"][0]["content"].as_array().unwrap();
    let complete = ca.len() - 2; // all but the newest (partial) chunk and the live tail
    let text = |v: &[Value]| v.iter().map(|c| c["text"].clone()).collect::<Vec<_>>();
    assert_eq!(text(&c2[..complete]), text(&ca[..complete]));
    assert_ne!(c2[complete]["text"], ca[complete]["text"], "the newest chunk grew");
    assert_eq!(user_prompt_at(&g2, at(3)).text().matches("</recent_conversation>").count(), 1);
}

/// A conversation over the ceiling loses whole chunks from the front, so the chunks that stay are unchanged.
#[test]
fn trimming_the_oldest_lines_keeps_chunk_boundaries() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.jsonl");
    let mut f = std::fs::File::create(&path).unwrap();
    for k in 0..1000 { writeln!(f, "{}", json!({"type":"message","message":{"role":"user","content":format!("User: {k} {}", "w".repeat(600))}})).unwrap(); }
    drop(f);
    let len = std::fs::metadata(&path).unwrap().len();
    let (_, lines, _) = read_session(&path, len).unwrap();
    assert!(lines.iter().map(|l| l.len() + 1).sum::<usize>() <= CONVERSATION_CEILING);
    let first = |lines: &[String]| -> usize { lines[0].split_whitespace().nth(2).unwrap().parse().unwrap() };
    assert_eq!(first(&lines) % TRIM_LINES, 0, "trimmed at a boundary counted from the compaction: {}", first(&lines));
    // The user says more: the start (and with it the cached prefix) stays where it was.
    let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
    for k in 1000..1020 { writeln!(f, "{}", json!({"type":"message","message":{"role":"user","content":format!("User: {k} {}", "w".repeat(600))}})).unwrap(); }
    drop(f);
    let (_, more, _) = read_session(&path, std::fs::metadata(&path).unwrap().len()).unwrap();
    assert_eq!((first(&more), more.len()), (first(&lines), lines.len() + 20));
}

/// Over the $0.50 limit the pass is skipped without a call, and waits for something new.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pass_over_the_limit_is_skipped_and_not_retried_on_the_same_input() {
    let (url, fake) = serve().await;
    *fake.mode.lock().unwrap() = Mode::Answer(answer_proposing());
    let mut l = lab(&url, Some("sk-test"), Duration::from_secs(10));
    seed(&l.root);
    let u = url.clone();
    // Claude Opus 5.5 with a conversation of ~400 kB: its prompt alone passes $0.50 written cold.
    l.cfg.endpoint = Arc::new(move || Endpoint { base: u.clone(), key: Some("sk-test".into()), workspace: None, timeout: Duration::from_secs(10), model: Some("claude-opus-5-5".into()), ..Default::default() });
    for _ in 0..200 { append(&l.session, json!({"type":"message","message":{"role":"user","content":format!("User: {}", "w".repeat(1600))}})); }
    let shared = Shared::default();
    assert_eq!(pass(&l.cfg, &shared).await.unwrap(), None);
    assert_eq!(pass(&l.cfg, &shared).await.unwrap(), None);
    assert_eq!(fake.hits.load(Ordering::SeqCst), 0, "never sent");
    assert_eq!(shared.calls.load(Ordering::Acquire), 1, "the second tick saw nothing new");
}

/// Review finding (item 28): a paid run without an answer (cut off, at the limit, refused) is not resent on the
/// same input; the next pass waits for something new.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cut_off_run_is_paid_for_once() {
    let (url, fake) = serve().await;
    *fake.mode.lock().unwrap() = Mode::Raw(json!({"content": [{"type": "thinking", "thinking": "", "signature": "s"}], "stop_reason": "max_tokens",
        "usage": {"input_tokens": 100, "cache_creation_input_tokens": 9000, "cache_read_input_tokens": 0, "output_tokens": 128000}}));
    let l = lab(&url, Some("sk-test"), Duration::from_secs(10));
    seed(&l.root);
    let runs: Arc<Mutex<Vec<crate::background_call::Run>>> = Default::default();
    let seen = runs.clone();
    crate::background_call::set_ledger(Arc::new(move |r: &crate::background_call::Run| seen.lock().unwrap().push(r.clone())));
    let shared = Shared::default();
    assert!(pass(&l.cfg, &shared).await.unwrap_err().to_string().contains("cut off"));
    assert_eq!(pass(&l.cfg, &shared).await.unwrap(), None);
    assert_eq!(fake.hits.load(Ordering::SeqCst), 1, "the same input is not sent again");
    assert_eq!(runs.lock().unwrap().iter().map(|r| r.outcome).collect::<Vec<_>>(), ["cut off"], "and the paid run is in the ledger");
    // Something new: a pass again.
    *fake.mode.lock().unwrap() = Mode::Answer(echo(&l.root));
    append(&l.session, json!({"type":"message","message":{"role":"user","content":"User: one more"}}));
    pass(&l.cfg, &shared).await.unwrap();
    assert_eq!(fake.hits.load(Ordering::SeqCst), 2);
}

/// Bug 2026-10-23T23-46-23: a stray click closed a plan list; `refresh_plan` inside the gap shows it again at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refresh_plan_shows_a_closed_plan_island_again_at_once() {
    let (url, _fake) = serve().await;
    let l = lab(&url, None, Duration::from_secs(10));
    seed(&l.root);
    let sched = l.root.read("scheduled").unwrap();
    // The app hides a closed list now; an older one deleted the file. Both come back.
    l.root.patch("settled", |o| { o.insert("hidden".into(), json!(true)); }).unwrap();
    crate::islands::plan::keep_closed(&l.root, "settled", None).unwrap();
    crate::islands::plan::keep_closed(&l.root, "scheduled", Some(&sched)).unwrap();
    l.root.remove("scheduled").unwrap();
    let r = Refresher::default();
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let task = r.spawn(l.cfg.clone(), Arc::new(|_: &str| Ok(())), Duration::from_secs(3600), Duration::from_secs(3600), stop_rx);
    *r.shared.last_call.lock().unwrap() = Some(std::time::Instant::now());
    assert!(r.request().contains("refreshes in about"));
    assert_eq!(l.root.read("scheduled"), Some(sched));
    assert!(l.root.read("settled").unwrap().get("hidden").is_none(), "a hidden list is shown again");
    stop_tx.send_replace(true);
    tokio::time::timeout(Duration::from_secs(2), task).await.unwrap().unwrap();
}

fn notice(text: &str) -> Value {
    json!({"type":"message","message":{"role":"user","content":[{"type":"text","text":format!("[Heimdall delivery: notice-ab12]\n[Now: Tuesday 27 Oct 2026, 04:00]\n[Heimdall notice: terminal:w:r:completed]\n{text}")}]}})
}
fn owner(text: &str) -> Value {
    json!({"type":"message","message":{"role":"user","content":[{"type":"text","text":format!("[Heimdall delivery: 2dc5482c]\nTranscript:\n[04:00:00 Mic · User] {text}")}]}})
}

/// Night 1027: 337 passes in two days, one every ~5 minutes around the clock (25 Oct 04:00-07:00: 36 passes with
/// nobody talking, same-size prompts). The user's words still run a pass at once; notices, Heimdall's own lines and a
/// bound island whose state moved wait for the slow lane, longer while the user is quiet, and a waiting change is served once
/// the lane opens without anything new arriving.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn only_owners_words_run_a_pass_at_once_and_other_changes_wait_for_their_lane() {
    let (url, fake) = serve().await;
    let l = lab(&url, Some("k"), Duration::from_secs(10));
    seed(&l.root);
    let mut o = Map::new();
    o.insert("title".into(), json!("Release"));
    o.insert("thread".into(), json!({"id": "t-read", "title": "x"}));
    l.root.put("job-a", &o).unwrap();
    *fake.mode.lock().unwrap() = Mode::Answer(echo(&l.root));
    let shared = Shared::default();
    shared.slow_ms.store(700, Ordering::Release);
    shared.quiet_ms.store(60_000, Ordering::Release);
    let hits = || fake.hits.load(Ordering::SeqCst);
    pass(&l.cfg, &shared).await.unwrap();
    assert_eq!(hits(), 1, "the first look runs");
    append(&l.session, notice("Arsenal build finished"));
    pass(&l.cfg, &shared).await.unwrap();
    l.root.patch("job-a", |o| { o.insert("state".into(), json!("waiting")); }).unwrap();
    pass(&l.cfg, &shared).await.unwrap();
    // Only code's churn on the bound island (its stage and history): not news at all.
    l.root.patch("job-a", |o| { o.insert("stage".into(), json!("Testing")); o.insert("steps".into(), json!([{"title": "Testing  2 min", "state": "now"}])); }).unwrap();
    pass(&l.cfg, &shared).await.unwrap();
    assert_eq!(hits(), 1, "a notice and a bound island's new state wait while User is quiet");
    append(&l.session, owner("Add the voice pick to my list"));
    pass(&l.cfg, &shared).await.unwrap();
    assert_eq!(hits(), 2, "User's words run a pass at once");
    let prompt = user_text(&fake.body.lock().unwrap());
    assert!(prompt.contains("Arsenal build finished") && prompt.contains("Add the voice pick"), "the waiting change is in it");
    append(&l.session, notice("Tests passed"));
    pass(&l.cfg, &shared).await.unwrap();
    assert_eq!(hits(), 2, "inside the slow lane");
    tokio::time::sleep(Duration::from_millis(800)).await;
    pass(&l.cfg, &shared).await.unwrap();
    assert_eq!(hits(), 3, "the lane opened: the waiting notice is served with nothing new arriving");
    assert!(user_text(&fake.body.lock().unwrap()).contains("Tests passed"));
    pass(&l.cfg, &shared).await.unwrap();
    assert_eq!(hits(), 3, "then nothing new: no pass");
    // A `refresh_plan` request is the fast lane too.
    append(&l.session, notice("Night build installed"));
    shared.requested.store(true, Ordering::Release);
    pass(&l.cfg, &shared).await.unwrap();
    assert_eq!(hits(), 4);
}

/// Replaces `an_item_added_within_the_hour_leaves_only_when_two_passes_drop_it` (the refresher removes nothing):
/// passes that drop an old item or the item it proposed remove and withdraw nothing, and passes that suggest the
/// proposal again do not make it twice. Passes here are set off by notices.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_proposal_stays_when_later_passes_drop_it_and_is_not_made_twice_when_they_repeat_it() {
    let (url, fake) = serve().await;
    let l = lab(&url, Some("k"), Duration::from_secs(10));
    seed(&l.root);
    let titles = |root: &Root| -> Vec<String> { items(root, "scheduled").iter().map(|v| v["title"].as_str().unwrap().to_owned()).collect() };
    let before = titles(&l.root);
    let drama = "Drama 3: pick the better voice by ear";
    let when = in_days(1);
    *fake.mode.lock().unwrap() = Mode::Answer(echo_with(&l.root, &[(drama, &when)]));
    let shared = Shared::default();
    let n = pass(&l.cfg, &shared).await.unwrap().unwrap();
    assert!(n.contains("proposed \u{201c}Drama 3"), "{n}");
    assert_eq!(titles(&l.root), before, "a proposal is not an item");
    // The next pass drops an old item and does not mention the proposal: nothing leaves, nothing is withdrawn, no notice.
    let mut a: Value = serde_json::from_str(&echo(&l.root)).unwrap();
    a["scheduled"].as_array_mut().unwrap().retain(|it| it["title"] != "Heimdall: fix underrun");
    *fake.mode.lock().unwrap() = Mode::Answer(a.to_string());
    append(&l.session, notice("Tests passed"));
    assert_eq!(pass(&l.cfg, &shared).await.unwrap(), None);
    assert_eq!(titles(&l.root), before, "an old item the answer drops stays");
    assert_eq!(proposals_of(&l.root).len(), 1);
    // The model suggests it again, as many times as it likes: still one proposal, no second notice.
    for n in 0..2 {
        *fake.mode.lock().unwrap() = Mode::Answer(echo_with(&l.root, &[(drama, &when)]));
        append(&l.session, notice(&format!("Build installed {n}")));
        assert_eq!(pass(&l.cfg, &shared).await.unwrap(), None);
    }
    assert_eq!(proposals_of(&l.root).len(), 1);
    assert_eq!(fake.hits.load(Ordering::SeqCst), 4);
}

/// Agent off or Heimdall paused (`DeliveryGate`): no thread read, no session read, no model call.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn while_the_agent_is_off_nothing_is_read_or_called() {
    let (url, fake) = serve().await;
    *fake.mode.lock().unwrap() = Mode::Answer(answer_proposing());
    let mut l = lab(&url, Some("k"), Duration::from_secs(10));
    seed(&l.root);
    l.cfg.enabled = Arc::new(|| false);
    let shared = Shared::default();
    for _ in 0..3 {
        append(&l.session, owner("hello"));
        assert_eq!(pass(&l.cfg, &shared).await.unwrap(), None);
    }
    assert_eq!((fake.hits.load(Ordering::SeqCst), shared.reads.load(Ordering::SeqCst)), (0, 0));
}

#[test]
fn a_pass_log_names_what_moved() {
    let a = Fingerprint { session: 1, owner: 1, note: 1, islands: 1 };
    assert_eq!(changed_parts(None, &a, false), "the first look since start");
    assert_eq!(changed_parts(Some(&a), &Fingerprint { session: 2, islands: 2, ..a }, false), "notices or Heimdall's lines, islands");
    assert_eq!(changed_parts(Some(&a), &Fingerprint { session: 2, owner: 2, ..a }, true), "refresh_plan, User's words");
}

/// Replaces `a_young_item_owner_asks_to_remove_goes_at_once`: what the user says no to is gone at once and stays gone,
/// whether the later pass is set off by his words or by `refresh_plan`; a new suggestion still gets through.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_proposal_owner_declined_is_not_proposed_again_by_a_pass_he_or_refresh_plan_sets_off() {
    let (url, fake) = serve().await;
    let l = lab(&url, Some("k"), Duration::from_secs(10));
    seed(&l.root);
    let title = "Drama 3: pick the better voice by ear";
    let when = in_days(1);
    *fake.mode.lock().unwrap() = Mode::Answer(echo_with(&l.root, &[(title, &when)]));
    let shared = Shared::default();
    pass(&l.cfg, &shared).await.unwrap().unwrap();
    crate::islands::proposals::decline(&l.root, Some("scheduled"), "Drama 3", now_s()).unwrap();
    assert!(proposals_of(&l.root).is_empty());
    // The user's pass: the model re-suggests it without an id. Not proposed again, no removed-and-added flicker.
    *fake.mode.lock().unwrap() = Mode::Answer(echo_with(&l.root, &[(title, &when)]));
    append(&l.session, owner("drop the Drama 3 item, I picked already"));
    assert_eq!(pass(&l.cfg, &shared).await.unwrap(), None);
    assert!(proposals_of(&l.root).is_empty());
    // A new suggestion on the user's next pass comes through.
    *fake.mode.lock().unwrap() = Mode::Answer(echo_with(&l.root, &[(title, &when), ("Voice: try the new take", &when)]));
    append(&l.session, owner("and the voice take?"));
    let n = pass(&l.cfg, &shared).await.unwrap().unwrap();
    assert!(n.contains("proposed \u{201c}Voice: try the new take\u{201d}") && !n.contains("Drama 3"), "{n}");
    // Through `refresh_plan` too.
    *fake.mode.lock().unwrap() = Mode::Answer(echo_with(&l.root, &[(title, &when)]));
    append(&l.session, notice("Tests passed"));
    shared.requested.store(true, Ordering::Release);
    assert_eq!(pass(&l.cfg, &shared).await.unwrap(), None);
    assert_eq!(proposals_of(&l.root).iter().map(|p| p["title"].as_str().unwrap()).collect::<Vec<_>>(), ["Voice: try the new take"]);
    assert_eq!(crate::islands::proposals::recorded(&l.root, "scheduled"), [title]);
}

/// Review of night 1027: the user's words were the hash of all his lines in the window, so a compaction that summarised
/// older ones looked like the user speaking: a fast pass, and the quiet clock reset. Only his newest line counts.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_compaction_is_not_owner_speaking() {
    let (url, fake) = serve().await;
    let l = lab(&url, Some("k"), Duration::from_secs(10));
    seed(&l.root);
    *fake.mode.lock().unwrap() = Mode::Answer(echo(&l.root));
    let shared = Shared::default();
    shared.slow_ms.store(60_000, Ordering::Release);
    shared.quiet_ms.store(60_000, Ordering::Release);
    pass(&l.cfg, &shared).await.unwrap();
    assert_eq!(fake.hits.load(Ordering::SeqCst), 1);
    // Summarises e1 and e2 (both the user's) away: the window loses his lines, his newest line is still e2.
    append(&l.session, json!({"type":"compaction","id":"c2","parentId":"e5","firstKeptEntryId":"e3","summary":"User pushed Arsenal."}));
    pass(&l.cfg, &shared).await.unwrap();
    assert_eq!(fake.hits.load(Ordering::SeqCst), 1, "waits for the slow lane");
    assert!(shared.owner.lock().unwrap().1.is_none(), "the quiet clock is not reset");
    append(&l.session, json!({"type":"message","id":"e6","parentId":"c2","message":{"role":"user","content":"User: and the voice pick"}}));
    pass(&l.cfg, &shared).await.unwrap();
    assert_eq!(fake.hits.load(Ordering::SeqCst), 2, "his new line runs a pass at once");
    assert!(shared.owner.lock().unwrap().1.is_some());
}

/// Review of night 1027: a network error lost a `refresh_plan` request, so the retry waited for the slow lane.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_request_survives_a_network_error() {
    let (url, fake) = serve().await;
    let l = lab(&url, Some("k"), Duration::from_secs(10));
    seed(&l.root);
    *fake.mode.lock().unwrap() = Mode::Answer(echo(&l.root));
    let shared = Shared::default();
    shared.slow_ms.store(60_000, Ordering::Release);
    shared.quiet_ms.store(60_000, Ordering::Release);
    pass(&l.cfg, &shared).await.unwrap();
    append(&l.session, notice("Arsenal build finished"));
    shared.requested.store(true, Ordering::Release);
    *fake.mode.lock().unwrap() = Mode::Status(500);
    pass(&l.cfg, &shared).await.unwrap_err();
    assert!(shared.requested.load(Ordering::Acquire), "the request is still pending");
    *fake.mode.lock().unwrap() = Mode::Answer(echo(&l.root));
    pass(&l.cfg, &shared).await.unwrap();
    assert_eq!(fake.hits.load(Ordering::SeqCst), 3, "the retry is in the fast lane");
}

/// Review of night 1027: with a 5-minute tick, a 15-minute lane checked at 14:59 waited for the next tick (~20 min).
/// A lane opens half a tick early.
#[test]
fn a_lane_opens_on_the_tick_nearest_its_gap() {
    let shared = Shared::default();
    shared.slow_ms.store(15_000, Ordering::Release);
    shared.quiet_ms.store(30_000, Ordering::Release);
    shared.period_ms.store(5_000, Ordering::Release);
    *shared.owner.lock().unwrap() = (Some(1), Some(std::time::Instant::now()));
    let ago = |ms: u64| Some(std::time::Instant::now() - Duration::from_millis(ms));
    *shared.last_call.lock().unwrap() = ago(14_900);
    assert!(shared.lane_open(), "a tick just before 15 min serves the slow lane");
    *shared.last_call.lock().unwrap() = ago(12_000);
    assert!(!shared.lane_open(), "not a tick early");
    *shared.owner.lock().unwrap() = (Some(1), None);
    *shared.last_call.lock().unwrap() = ago(29_900);
    assert!(shared.lane_open(), "the quiet lane likewise");
    *shared.last_call.lock().unwrap() = ago(25_000);
    assert!(!shared.lane_open());
}


/// A fake ChatGPT-route Responses server: every request's headers and body, and a distinct streamed answer per request
/// (`{"pass": n}`), so a reused answer would show.
async fn serve_responses() -> (String, Arc<Mutex<Vec<(Vec<(String, String)>, Value)>>>) {
    use axum::{extract::State, http::HeaderMap, routing::post, Router};
    type Got = Arc<Mutex<Vec<(Vec<(String, String)>, Value)>>>;
    async fn responses(State(got): State<Got>, h: HeaderMap, body: String) -> axum::response::Response {
        let body: Value = serde_json::from_str(&body).unwrap();
        let heads = h.iter().map(|(k, v)| (k.as_str().to_owned(), v.to_str().unwrap_or("").to_owned())).collect();
        let n = { let mut g = got.lock().unwrap(); g.push((heads, body)); g.len() };
        let item = json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": json!({"pass": n}).to_string()}]});
        let usage = json!({"input_tokens": 9000, "input_tokens_details": {"cached_tokens": 8448}, "output_tokens": 700, "output_tokens_details": {"reasoning_tokens": 600}});
        let text = format!("event: response.output_item.done\ndata: {}\n\nevent: response.completed\ndata: {}\n\n",
            json!({"type": "response.output_item.done", "output_index": 0, "item": item}),
            json!({"type": "response.completed", "response": {"status": "completed", "output": [], "usage": usage}}));
        axum::response::Response::builder().header("content-type", "text/event-stream").body(axum::body::Body::from(text)).unwrap()
    }
    let got: Got = Default::default();
    let app = Router::new().route("/responses", post(responses)).with_state(got.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, got)
}

/// A captured request as a fixture: the credential cut out, the 30 KB system prompt named by size and hash.
fn fixture(heads: &[(String, String)], body: &Value) -> Value {
    let mut b = body.clone();
    let system = b["instructions"].as_str().unwrap_or("").to_owned();
    b["instructions"] = json!(format!("<SYSTEM: {} bytes, fnv {:016x}>", system.len(), crate::background_call::prompt_hash(&call("", &system, "".into()))));
    let heads: Vec<Value> = heads.iter().filter(|(k, _)| !["host", "content-length"].contains(&k.as_str()))
        .map(|(k, v)| json!([k, if k == "authorization" { "Bearer [scrubbed]" } else { v.as_str() }])).collect();
    json!({"headers": heads, "body": b})
}

/// "we have to cache Luna prompts". The plan refresher on the ChatGPT route (GPT-6 Luna), two
/// passes as the wire sees them: the same cache key in the body and in the affinity headers Pi sends on that route
/// (`session_id`, `x-client-request-id`), the same instructions, and the first pass's conversation a byte-identical
/// prefix of the second's, ending a message; only the new line and the clock differ, after it. Each pass is its own
/// request whose own answer is read (nothing cached on Heimdall's side), and a changed system prompt or model starts a
/// new key.
/// `LUNA_CACHE_FIXTURES=<dir>` writes both requests there (synthetic lab content, credential scrubbed).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn luna_passes_share_one_cache_identity_and_a_byte_identical_prefix_on_the_chatgpt_route() {
    use crate::background_call::Wire;
    let (url, got) = serve_responses().await;
    let l = lab(&url, Some("chatgpt-access-token-abcdef"), Duration::from_secs(5));
    seed(&l.root);
    for k in 0..45 { append(&l.session, json!({"type":"message","message":{"role":"user","content":format!("User: line {k}")}})); }
    let e = Endpoint { wire: Wire::Responses, provider: agent::provider::CHATGPT, model: Some("gpt-6-luna".into()), reasoning: true,
        efforts: ["low", "medium", "high", "xhigh", "max"].map(String::from).to_vec(), ..(l.cfg.endpoint)() };
    let at = |h: u32, m: u32| chrono::TimeZone::with_ymd_and_hms(&chrono::Local, 2026, 10, 27, h, m, 0).unwrap();
    let g1 = gather(&l.cfg, None).unwrap().unwrap();
    let p1 = user_prompt_at(&g1, at(22, 56));
    let a1 = call_model(&e, &p1).await.unwrap();
    append(&l.session, json!({"type":"message","message":{"role":"user","content":"User: and cache the Luna prompts"}}));
    let g2 = gather(&l.cfg, None).unwrap().unwrap();
    let p2 = user_prompt_at(&g2, at(23, 1));
    let a2 = call_model(&e, &p2).await.unwrap();
    let got = got.lock().unwrap().clone();
    if let Ok(dir) = std::env::var("LUNA_CACHE_FIXTURES") {
        for (k, (h, b)) in got.iter().enumerate() {
            std::fs::write(Path::new(&dir).join(format!("pass-{}.json", k + 1)), serde_json::to_string_pretty(&fixture(h, b)).unwrap()).unwrap();
        }
    }
    // Two requests, each answer its own: no answer reuse.
    assert_eq!(got.len(), 2);
    assert_eq!((a1.text.as_str(), a2.text.as_str()), (r#"{"pass":1}"#, r#"{"pass":2}"#));
    // Token accounting: cached reads apart, reasoning inside the output, never added again.
    assert_eq!((a1.usage.input, a1.usage.cache_read, a1.usage.output), (552, 8448, 700));
    let ((h1, b1), (h2, b2)) = (&got[0], &got[1]);
    let head = |h: &[(String, String)], k: &str| h.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
    let key = b1["prompt_cache_key"].as_str().unwrap().to_owned();
    assert!(key.starts_with("heimdall-plan-refresher-") && key.len() <= 64 && !key.contains("chatgpt-access-token"));
    assert_eq!(b2["prompt_cache_key"], key.as_str(), "a new clock and a new line keep the cache identity");
    for h in [h1, h2] {
        assert_eq!(head(h, "session_id").as_deref(), Some(key.as_str()), "Pi's affinity header on this route carries the key");
        assert_eq!(head(h, "x-client-request-id").as_deref(), Some(key.as_str()));
    }
    // What comes before the input is the same: instructions, schema, effort, model; no fields the route rejects.
    for k in ["instructions", "text", "reasoning", "model", "store"] { assert_eq!(b1[k], b2[k], "{k}"); }
    assert_eq!(b1["reasoning"]["effort"], "high");
    assert!(b1.get("prompt_cache_retention").is_none() && b1.get("max_output_tokens").is_none() && b1.get("temperature").is_none());
    // The input: one user message per stretch, joined exactly the prompt's text (same words, same order).
    let joined = |b: &Value| b["input"].as_array().unwrap().iter().map(|m| { assert_eq!(m["role"], "user"); m["content"].as_str().unwrap().to_owned() }).collect::<String>();
    let (t1, t2) = (&joined(b1), &joined(b2));
    assert_eq!((t1.as_str(), t2.as_str()), (p1.text().as_str(), p2.text().as_str()));
    // A message end where the passes still agree: every message but pass 1's newest (growing) chunk and its live tail
    // comes again unchanged in pass 2, so GPT-5.6+'s lookup at that message end finds pass 1's prefix.
    let (m1, m2) = (b1["input"].as_array().unwrap(), b2["input"].as_array().unwrap());
    let shared = m1.len() - 2;
    assert!(shared >= 3 && m1[..shared] == m2[..shared], "{} {}", m1.len(), m2.len());
    assert!(m1[shared - 1]["content"].as_str().unwrap().lines().count() == CHUNK_LINES, "the shared stretch ends on a whole chunk");
    assert_ne!(m1[shared], m2[shared], "the newest chunk grew");
    assert!(m1.last().unwrap()["content"].as_str().unwrap().starts_with("</recent_conversation>"), "the live tail is its own last message");
    // Pass 1's stable stretch (summary and the whole conversation so far) opens pass 2's input unchanged.
    let stable1 = t1.find("</recent_conversation>").unwrap();
    assert_eq!(t2[..stable1], t1[..stable1], "byte-identical reusable prefix");
    assert!(t2[stable1..].starts_with("Incoming: User: and cache the Luna prompts\n</recent_conversation>"), "the new line follows it");
    assert!(!t1.contains("cache the Luna prompts") && t2.contains("cache the Luna prompts"), "the new context reaches the model");
    assert!(t1.ends_with("Now: Tuesday 2026-10-27 22:56\n\nRewrite the lists and rebuild the job islands now."));
    assert!(t2.ends_with("Now: Tuesday 2026-10-27 23:01\n\nRewrite the lists and rebuild the job islands now."));
    assert!(!t1[..stable1].contains("Now:"), "no clock in the reusable prefix");
    // A changed system prompt, model or effort is a new cache identity.
    let l2 = crate::background_call::Loop::for_endpoint(&e);
    let base = call("gpt-6-luna", SYSTEM, p1.clone());
    let k = |c: &crate::background_call::Call| crate::background_call::wire::answer_body(&l2, c, 0)["prompt_cache_key"].as_str().unwrap().to_owned();
    assert_eq!(k(&base), key);
    assert_ne!(k(&call("gpt-6-luna", "another system prompt", p1.clone())), key);
    assert_ne!(k(&call("gpt-6.1-sol", SYSTEM, p1.clone())), key);
    assert_ne!(k(&crate::background_call::Call { effort: "low", ..base.clone() }), key);
}

/// Review, 30 Oct: the plan lists honour Background Tasks ▸ Plan Lists' own model, not only the shared one.
#[test]
fn the_plan_lists_run_their_own_task_model_when_one_is_set() {
    use crate::background_model::{publish, publish_tasks, with_task_model, default_task_models, BackgroundModel, Task};
    publish(&BackgroundModel { provider: "anthropic".into(), id: "claude-haiku-5-5".into() }, &[]);
    publish_tasks(&default_task_models());
    assert_eq!(live_endpoint().model.as_deref(), Some("claude-haiku-5-5"), "no own model: the shared one");
    publish_tasks(&with_task_model(&default_task_models(), Task::Plan, Some(BackgroundModel { provider: "anthropic".into(), id: "claude-opus-5-5".into() })));
    assert_eq!(live_endpoint().model.as_deref(), Some("claude-opus-5-5"));
}

#[test]
fn a_detail_loses_its_needs_prefix() {
    assert_eq!(quiet_detail("Needs: a yes on installing.\nIt passed."), "A yes on installing.\nIt passed.");
    assert_eq!(quiet_detail("Front and back."), "Front and back.");
    assert_eq!(quiet_detail("Needs:"), "");
}

/// To do is Goals', kept by another job. With `todo.json` and `goals.json` in the islands folder, a
/// pass (whose answer even carries a stray `todo` list) writes neither, byte for byte, and the prompt carries
/// neither file's items; Scheduled takes only a proposal and Settled nothing (2 Nov).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pass_never_writes_or_sends_to_do_or_goals() {
    let (url, fake) = serve().await;
    let l = lab(&url, Some("k"), Duration::from_secs(10));
    seed(&l.root);
    l.root.ensure().unwrap();
    let todo = r#"{"id": "todo", "preset": "todo", "list": "todo", "title": "To do",
 "items": [{"id": "t1", "title": "Old todo: pick the voice", "group": "waiting", "done": false, "kind": "pick"}]}"#;
    let goals = r#"{"id": "goals", "preset": "todo", "list": "goals", "title": "Goals", "icon": "flag",
 "items": [{"id": "g-a1", "title": "Ask: approve the release", "done": false, "goal": "g-ship"}],
 "goals": [{"id": "g-ship", "title": "Ship Heimdall publicly", "state": "active", "now": "Opening film nearly final",
            "tasks": [{"id": "t-1", "title": "Privacy scan", "state": "now"}]}]}"#;
    std::fs::write(l.root.file("todo"), todo).unwrap();
    std::fs::write(l.root.file("goals"), goals).unwrap();
    let mut answer: Value = serde_json::from_str(&answer_proposing()).unwrap();
    answer["todo"] = json!([{"id": "t1", "title": "Old todo: pick the voice", "done": true}, {"id": "", "title": "Stray", "done": false}]);
    *fake.mode.lock().unwrap() = Mode::Answer(answer.to_string());
    let shared = Shared::default();
    let notice = pass(&l.cfg, &shared).await.unwrap().unwrap();
    assert!(notice.contains(PROPOSED) && !notice.contains("Stray") && !notice.contains("Old todo"), "{notice}");
    assert_eq!(proposals_of(&l.root).iter().map(|p| p["title"].as_str().unwrap()).collect::<Vec<_>>(), [PROPOSED], "only the Scheduled proposal is written");
    assert_eq!(std::fs::read_to_string(l.root.file("todo")).unwrap(), todo, "todo.json untouched");
    assert_eq!(std::fs::read_to_string(l.root.file("goals")).unwrap(), goals, "goals.json untouched");
    assert!(!items(&l.root, "settled").iter().any(|i| i["id"] == "w1"), "Settled is not the refresher's");
    let prompt = user_text(&fake.body.lock().unwrap());
    for theirs in ["Old todo", "pick the voice", "approve the release", "Ship Heimdall", "Privacy scan", "g-ship", "\"todo\":", "\"goals\""] {
        assert!(!prompt.contains(theirs), "the prompt carries {theirs:?}");
    }
    assert!(prompt.contains("\"scheduled\":") && prompt.contains("Arsenal: push branch"));
    // A change in either file is no news for this job.
    std::fs::write(l.root.file("goals"), goals.replace("nearly final", "final")).unwrap();
    std::fs::write(l.root.file("todo"), todo.replace("false", "true")).unwrap();
    assert_eq!(pass(&l.cfg, &shared).await.unwrap(), None);
    assert_eq!(fake.hits.load(Ordering::SeqCst), 1, "no pass for Goals' or To do's churn");
}
