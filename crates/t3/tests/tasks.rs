//! Night 20 Oct (bug notes 18-17-04, 18-24-56, 18-29-07, 18-29-34, 18-32-31; day review T1–T3): Heimdall's T3 tools
//! under T3's own names, one call per action, compact answers, and no false wakes. Against a local fake T3 whose
//! threads have runs and timelines like the real one (positions, activity vs messages, UTF-16 text pages, sub-agent
//! items, background commands and their notifications). Never touches the real T3 Code.
use agent::{
    actions::{ActionSink, Actions, Notice, Question},
    delivery::{DeliveryGate, DeliveryPermit},
};
use anyhow::Result;
use axum::{extract::State, http::HeaderMap, response::IntoResponse, response::Response, routing::{get, post}, Json, Router};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use t3::client::Credential;
use t3::{Client, CredentialStore, Workers};

#[derive(Default)]
struct Store(Mutex<Option<Credential>>);
impl CredentialStore for Store {
    fn load(&self) -> Result<Option<Credential>> {
        Ok(self.0.lock().unwrap().clone())
    }
    fn save(&self, c: &Credential) -> Result<()> {
        *self.0.lock().unwrap() = Some(c.clone());
        Ok(())
    }
}
#[derive(Default)]
struct Sink {
    deliveries: Mutex<Vec<(String, String)>>,
    /// The next delivery is taken by the agent but its acknowledgement is lost (Heimdall quits in between).
    lose_ack: std::sync::atomic::AtomicBool,
    /// The next restart notice's delivery times out before the agent takes it (Pi still starting).
    fail_restart: std::sync::atomic::AtomicBool,
    /// A notice the agent cannot take (every attempt fails, nothing delivered).
    refuse: Mutex<Option<String>>,
}
impl ActionSink for Sink {
    fn notice(&self, _: &Notice, p: &DeliveryPermit) -> Result<()> {
        p.check()
    }
    fn question(&self, _: &Question, p: &DeliveryPermit) -> Result<()> {
        p.check()
    }
    fn question_closed(&self, _: &str, _: &str, p: &DeliveryPermit) -> Result<()> {
        p.check()
    }
    fn deliver(&self, id: &str, text: &str, p: &DeliveryPermit) -> Result<()> {
        p.check()?;
        if id.starts_with("t3-restarted:") && self.fail_restart.swap(false, std::sync::atomic::Ordering::SeqCst) {
            anyhow::bail!("timed out waiting for the agent to take the notice");
        }
        if self.refuse.lock().unwrap().as_deref() == Some(id) {
            anyhow::bail!("the agent cannot take {id}");
        }
        self.deliveries.lock().unwrap().push((id.into(), text.into()));
        if self.lose_ack.swap(false, std::sync::atomic::Ordering::SeqCst) {
            anyhow::bail!("Heimdall quit before the delivery was acknowledged");
        }
        Ok(())
    }
    fn reload(&self, _: &str, p: &DeliveryPermit) -> Result<()> {
        p.check()
    }
}

const ACTIVE: [&str; 5] = ["preparing", "queued", "starting", "running", "waiting"];
/// Now plus `offset_s`. The fake's "now" never repeats: two items written in the same microsecond would tie, and
/// the order of times is what the scan reads (a test flaked on such a tie).
fn iso(offset_s: i64) -> String {
    static LAST: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
    let now = chrono::Utc::now().timestamp_micros();
    let prev = LAST.fetch_max(now, std::sync::atomic::Ordering::SeqCst);
    let now = if now <= prev { LAST.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1 } else { now };
    (chrono::DateTime::from_timestamp_micros(now).unwrap() + chrono::Duration::seconds(offset_s)).to_rfc3339()
}
#[derive(Default)]
struct Fake {
    threads: Mutex<Vec<Value>>,
    calls: Mutex<Vec<(String, Value)>>,
    root: Mutex<String>,
    changed: tokio::sync::Notify,
    /// A T3 build that marks t3_thread_send idempotent and not destructive (the 19 Oct build marks it destructive).
    idempotent_send: std::sync::atomic::AtomicBool,
    /// T3's pending-request listing fails (as the renamed tool did from 1 Nov until 0.1.6's name was used).
    fail_requests: std::sync::atomic::AtomicBool,
    /// T3 lists the user's second Claude account (`claudeAgent_2`) beside the main one.
    second_account: std::sync::atomic::AtomicBool,
    /// Every send and thread read fails as if T3 were busy or restarting (crewmates: the inbox keeps the message).
    fail_sends: std::sync::atomic::AtomicBool,
}
fn catalog(s: &Fake) -> Value {
    let open = json!({"type":"object","additionalProperties":true});
    let tool = |name: &str, ro: bool, destructive: bool| json!({"name":name,"description":format!("{name}."),"inputSchema":open,"annotations":{"readOnlyHint":ro,"idempotentHint":ro,"destructiveHint":destructive}});
    let mut send = tool("t3_thread_send",false,true);
    if s.idempotent_send.load(std::sync::atomic::Ordering::Relaxed) {
        send["annotations"] = json!({"readOnlyHint":false,"idempotentHint":true,"destructiveHint":false});
    }
    json!({"tools":[
        tool("orchestrator_capabilities",true,false), tool("t3_project_list",true,false), tool("t3_thread_list",true,false),
        tool("t3_thread_read",false,false), tool("t3_thread_wait",true,false), tool("t3_thread_launch",false,true),
        send, tool("t3_thread_interrupt",false,true), tool("t3_thread_organize",false,true),
        // T3 0.1.6's action tools (bug note 2026-11-02T14-56-51-3): one tool, an `action`.
        tool("t3_pending_request",false,true), tool("t3_queue",false,true), tool("t3_thread_configure",false,true),
    ]})
}
/// Opus as the installed T3 lists it since its 23 Oct update (`values` and `default`; bug note 2026-10-23T01-29-13).
fn live_options() -> Value {
    json!([{"id":"effort","values":["low","medium","high","xhigh","max","ultracode","ultrathink"],"default":"medium"},
        {"id":"fastMode","values":[false,true]}])
}
fn capabilities(s: &Fake) -> Value {
    let mut c = capabilities_one();
    if s.second_account.load(std::sync::atomic::Ordering::Acquire) {
        let mut two = c["providers"][0].clone();
        two["providerInstanceId"] = json!("claudeAgent_2");
        c["providers"].as_array_mut().unwrap().insert(1, two);
    }
    c
}
fn capabilities_one() -> Value {
    let effort = |default: &str| {
        let choices: Vec<Value> = ["low", "medium", "high", "max"].iter().map(|o| json!({"id":o,"isDefault":*o == default})).collect();
        json!([{"id":"effort","type":"select","options":choices}])
    };
    json!({"runtimeMode":"full-access","providers":[
        {"providerInstanceId":"claudeAgent","driverKind":"claudeAgent","runtimeModes":null,"constraints":[],"models":[
            {"id":"claude-haiku-5-5","options":effort("medium")},{"id":"claude-opus-5-5","options":live_options()}]},
        {"providerInstanceId":"codex","driverKind":"codex","runtimeModes":null,"constraints":[],"models":[{"id":"gpt-6-luna","options":[]}]},
        {"providerInstanceId":"cursor","driverKind":"cursor","constraints":["Provider instance is disabled."],"models":[{"id":"claude-opus-5-5"}]}]})
}
/// The installed T3's thread shell (threadShellFromProjection): the status is the latest run's (a queued run held in
/// the queue does not count), the active run the latest preparing, starting or running one.
fn detail(t: &Value) -> Value {
    let runs = t["runs"].as_array().unwrap();
    let latest = runs.iter().rev().find(|r| !(r["status"] == "queued" && r["queueHeld"] == true));
    let active = runs.iter().rev().find(|r| ["preparing", "starting", "running"].contains(&r["status"].as_str().unwrap()));
    let status = latest.map(|r| r["status"].clone()).unwrap_or(json!("idle"));
    let latest_id = latest.map(|r| r["runId"].clone());
    let active_id = active.map(|r| r["runId"].clone());
    // `approvals`: approval waits T3 counts without listing a request id (the count beyond `pending`).
    let pending = t["pending"].as_array().unwrap().len() + t["approvals"].as_u64().unwrap_or(0) as usize;
    let count = t["items"].as_array().unwrap().len();
    json!({"threadId":t["threadId"],"projectId":t["projectId"],"title":t["title"],"model":t["model"],"providerInstanceId":t["provider"].as_str().unwrap_or("claudeAgent"),
        "status":status,"latestRunId":latest_id,"activeRunId":active_id,"pendingRequestCount":pending,"itemCount":count,
        "archived":t["archived"],"runtimeMode":"full-access","relationshipToParent":t["relationshipToParent"],"parentThreadId":t["parentThreadId"],"creationSource":t["creationSource"],"updatedAt":t["updatedAt"],"createdAt":t["updatedAt"],"link":t["link"]})
}
/// A `t3_thread_list` row as the installed T3 lists it (live key list, 6 Nov; bug note 2026-11-06T11-57-54): no
/// `activeRunId` and no `pendingRequestCount`, which only the read shell (`detail`) carries.
fn row(t: &Value) -> Value {
    let mut d = detail(t);
    d.as_object_mut().unwrap().remove("activeRunId");
    d.as_object_mut().unwrap().remove("pendingRequestCount");
    d
}
async fn mcp(State(s): State<Arc<Fake>>, _h: HeaderMap, Json(v): Json<Value>) -> Response {
    let id = v["id"].clone();
    match v["method"].as_str().unwrap_or("") {
        "initialize" => return ([("Mcp-Session-Id", "s1")], Json(json!({"jsonrpc":"2.0","id":id,"result":{"protocolVersion":"2025-06-18","capabilities":{},"serverInfo":{"name":"Fake T3","version":"1"}}}))).into_response(),
        "notifications/initialized" => return axum::http::StatusCode::ACCEPTED.into_response(),
        "tools/list" => return Json(json!({"jsonrpc":"2.0","id":id,"result":catalog(&s)})).into_response(),
        _ => {}
    }
    let name = v["params"]["name"].as_str().unwrap().to_owned();
    let a = v["params"]["arguments"].clone();
    s.calls.lock().unwrap().push((name.clone(), a.clone()));
    if s.fail_sends.load(std::sync::atomic::Ordering::Acquire) && matches!(name.as_str(), "t3_thread_send" | "t3_thread_read" | "t3_thread_launch") {
        return Json(json!({"jsonrpc":"2.0","id":id,"error":{"code":-32000,"message":"T3 is restarting"}})).into_response();
    }
    let fail = |text: &str| Json(json!({"jsonrpc":"2.0","id":id,"result":{"isError":true,"content":[{"type":"text","text":text}]}})).into_response();
    let result = match name.as_str() {
        "orchestrator_capabilities" => capabilities(&s),
        "t3_project_list" => json!({"projects":[{"id":"p-vault","title":"Vault","workspaceRoot":*s.root.lock().unwrap()},{"id":"p-scratch","title":"No project","workspaceRoot":"/nonexistent-scratch"}],"nextCursor":null}),
        "t3_thread_list" => {
            let statuses: Vec<Value> = a["statuses"].as_array().cloned().unwrap_or_default();
            let mut rows: Vec<Value> = s.threads.lock().unwrap().iter()
                .filter(|t| a["allProjects"] == true || t["projectId"] == a["projectId"])
                .filter(|t| a["includeSubagents"] == true || t["relationshipToParent"] != "subagent")
                .filter(|t| a["titleContains"].as_str().is_none_or(|q| t["title"].as_str().unwrap().contains(q)))
                .map(row)
                .filter(|r| statuses.is_empty() || statuses.contains(&r["status"]))
                .collect();
            rows.sort_by(|x, y| y["updatedAt"].as_str().cmp(&x["updatedAt"].as_str()));
            let total = rows.len();
            rows.truncate(a["limit"].as_u64().unwrap_or(50) as usize);
            json!({"projectId":a["projectId"],"threads":rows,"nextCursor":null,"total":total})
        }
        "t3_thread_read" => {
            let threads = s.threads.lock().unwrap();
            let Some(t) = threads.iter().find(|t| t["threadId"] == a["threadId"]) else { return fail("Thread not found") };
            let messages = a["view"].as_str().unwrap_or("messages") == "messages";
            let all: Vec<&Value> = t["items"].as_array().unwrap().iter()
                .filter(|i| !messages || a["itemId"].is_string() || ["user_message","assistant_message","proposed_plan"].contains(&i["type"].as_str().unwrap()))
                .collect();
            let chosen: Vec<&Value> = if let Some(item) = a["itemId"].as_str() {
                all.into_iter().filter(|i| i["itemId"] == item).collect()
            } else {
                all.into_iter().filter(|i| a["afterPosition"].as_u64().is_none_or(|p| i["position"].as_u64().unwrap() > p)).collect()
            };
            let limit = a["limit"].as_u64().unwrap_or(20) as usize;
            let more = chosen.len() > limit;
            let max = a["maxCharsPerItem"].as_u64().unwrap_or(4000) as usize;
            let offset = a["textOffset"].as_u64().unwrap_or(0) as usize;
            let items: Vec<Value> = chosen.into_iter().take(limit).map(|i| {
                let units: Vec<u16> = i["text"].as_str().unwrap_or("").encode_utf16().collect();
                let start = offset.min(units.len());
                let mut end = (start + max).min(units.len());
                if end < units.len() && (0xDC00..0xE000).contains(&units[end]) { end -= 1; }
                let mut out = i.clone();
                out["text"] = json!(String::from_utf16(&units[start..end]).unwrap());
                out["textTruncated"] = json!(end < units.len());
                out["nextTextOffset"] = if end < units.len() { json!(end) } else { Value::Null };
                out
            }).collect();
            let runs: Vec<Value> = t["runs"].as_array().unwrap().iter().rev().take(a["runLimit"].as_u64().unwrap_or(10) as usize).cloned().collect();
            json!({"thread":detail(t),"recentRuns":runs,"items":items,"nextPosition":items.last().map(|i| i["position"].clone()).unwrap_or(a["afterPosition"].clone()),"hasMore":more})
        }
        "t3_thread_launch" => {
            let mut threads = s.threads.lock().unwrap();
            let n = threads.len();
            let tid = format!("mcp:task-{n}");
            let run = format!("run:{tid}:1");
            threads.push(json!({"threadId":tid,"projectId":a["projectId"],"title":a["title"],"model":a["modelSelection"]["model"],"provider":a["modelSelection"]["instanceId"],"archived":false,"pending":[],"relationshipToParent":null,"updatedAt":iso(0),
                "runs":[{"runId":run,"ordinal":1,"status":"running","model":a["modelSelection"]["model"],"requestedAt":iso(0),"startedAt":iso(0),"completedAt":null}],
                "items":[{"position":0,"itemId":format!("{tid}:brief"),"runId":run,"type":"user_message","status":"completed","text":a["message"],"createdBy":"agent","creationSource":"mcp","visibility":"local","updatedAt":iso(0)}]}));
            json!({"threadId":tid,"projectId":a["projectId"],"runId":run,"status":"running","modelSelection":a["modelSelection"]})
        }
        "t3_thread_send" => {
            let mut threads = s.threads.lock().unwrap();
            let Some(t) = threads.iter_mut().find(|t| t["threadId"] == a["threadId"]) else { return fail("Thread not found") };
            // T3 0.1.x refuses a message into an archived thread (log sweep 8 Nov, finding 1).
            if t["archived"] == true {
                return fail(&format!(r#"{{"_tag":"OrchestratorMcpFailure","code":"thread_not_sendable","message":"Thread {} is archived and cannot receive messages."}}"#, t["threadId"].as_str().unwrap()));
            }
            let active = t["runs"].as_array().unwrap().iter().rev().find(|r| ACTIVE.contains(&r["status"].as_str().unwrap())).map(|r| r["runId"].clone());
            // T3 0.1.x refuses a steer when no turn runs (bug note 2026-11-10T09-03-14; Heimdall's log, 5 Nov 07:44).
            if a["mode"] == "steer" && active.is_none() {
                return fail(&format!(r#"{{"_tag":"OrchestratorMcpFailure","code":"thread_not_sendable","message":"Thread {} has no running turn that can be steered."}}"#, t["threadId"].as_str().unwrap()));
            }
            let (run, delivery) = match active {
                // T3's queue_after_active: a run of its own that starts when the current turn ends.
                Some(_) if a["mode"] == "queue" => {
                    let n = t["runs"].as_array().unwrap().len() + 1;
                    let r = json!(format!("run:{}:{n}", t["threadId"].as_str().unwrap()));
                    let model = t["model"].clone();
                    t["runs"].as_array_mut().unwrap().push(json!({"runId":r,"ordinal":n,"status":"queued","model":model,"requestedAt":iso(0),"startedAt":null,"completedAt":null}));
                    (r, "queued")
                }
                Some(r) => (r, "steered"),
                None => {
                    let n = t["runs"].as_array().unwrap().len() + 1;
                    let r = json!(format!("run:{}:{n}", t["threadId"].as_str().unwrap()));
                    let model = t["model"].clone();
                    t["runs"].as_array_mut().unwrap().push(json!({"runId":r,"ordinal":n,"status":"running","model":model,"requestedAt":iso(0),"startedAt":iso(0),"completedAt":null}));
                    (r, "started")
                }
            };
            let pos = t["items"].as_array().unwrap().len();
            t["items"].as_array_mut().unwrap().push(json!({"position":pos,"itemId":format!("sent-{pos}"),"runId":run,"type":"user_message","status":"completed","text":a["message"],"createdBy":"agent","creationSource":"mcp","visibility":"local","updatedAt":iso(0)}));
            t["updatedAt"] = json!(iso(0));
            json!({"threadId":a["threadId"],"messageId":"m","runId":run,"status":if delivery == "queued" { "queued" } else { "running" },"delivery":delivery})
        }
        // The queue: a queued run's message is its user_message item.
        "t3_queue" if a["action"] == "list" => {
            let threads = s.threads.lock().unwrap();
            let t = threads.iter().find(|t| t["threadId"] == a["threadId"]).unwrap();
            let items: Vec<Value> = t["runs"].as_array().unwrap().iter().filter(|r| r["status"] == "queued").map(|r| {
                let text = t["items"].as_array().unwrap().iter().find(|i| i["runId"] == r["runId"] && i["type"] == "user_message").map(|i| i["text"].clone()).unwrap_or(json!(""));
                json!({"queuedRunId":r["runId"],"text":text,"truncated":false})
            }).collect();
            json!({"items":items,"nextCursor":null})
        }
        "t3_queue" if a["action"] == "edit" => {
            let mut threads = s.threads.lock().unwrap();
            let t = threads.iter_mut().find(|t| t["threadId"] == a["threadId"]).unwrap();
            if !t["runs"].as_array().unwrap().iter().any(|r| r["runId"] == a["queuedRunId"] && r["status"] == "queued") { return fail("Run is no longer queued") }
            for i in t["items"].as_array_mut().unwrap().iter_mut().filter(|i| i["runId"] == a["queuedRunId"] && i["type"] == "user_message") {
                i["text"] = a["text"].clone();
            }
            json!({"sequence":1})
        }
        // The account and model a thread runs on next (bug note 2026-11-06T12-26-36: the relay's move at 12:26).
        "t3_thread_configure" => {
            let mut threads = s.threads.lock().unwrap();
            let Some(t) = threads.iter_mut().find(|t| t["threadId"] == a["threadId"]) else { return fail("Thread not found") };
            t["provider"] = a["modelSelection"]["instanceId"].clone();
            t["model"] = a["modelSelection"]["model"].clone();
            json!({"sequence":1})
        }
        "t3_thread_interrupt" => {
            let mut threads = s.threads.lock().unwrap();
            let t = threads.iter_mut().find(|t| t["threadId"] == a["threadId"]).unwrap();
            for r in t["runs"].as_array_mut().unwrap().iter_mut().filter(|r| ACTIVE.contains(&r["status"].as_str().unwrap())) {
                r["status"] = json!("interrupted");
                r["completedAt"] = json!(iso(0));
            }
            s.changed.notify_waiters();
            json!({"threadId":a["threadId"],"runId":null,"status":"interrupt_requested"})
        }
        "t3_thread_organize" => {
            let mut threads = s.threads.lock().unwrap();
            if a["action"] == "archive" || a["action"] == "unarchive" {
                let t = threads.iter_mut().find(|t| t["threadId"] == a["threadId"]).unwrap();
                // T3 refuses to unarchive a thread that is not archived (3 and 4 Nov).
                if a["action"] == "unarchive" && t["archived"] != true {
                    return fail(&format!(r#"{{"_tag":"OrchestratorMcpFailure","code":"orchestration_error","message":"Thread {} is not archived."}}"#, t["threadId"].as_str().unwrap()));
                }
                // And to archive one that is (review 1111 S5).
                if a["action"] == "archive" && t["archived"] == true {
                    return fail(&format!(r#"{{"_tag":"OrchestratorMcpFailure","code":"orchestration_error","message":"Thread {} is already archived."}}"#, t["threadId"].as_str().unwrap()));
                }
                t["archived"] = json!(a["action"] == "archive");
            }
            json!({"sequence":1})
        }
        "t3_thread_wait" => {
            let deadline = tokio::time::Instant::now() + Duration::from_millis(a["timeoutMs"].as_u64().unwrap_or(1000).min(3000));
            loop {
                let notified = s.changed.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                let status = {
                    let threads = s.threads.lock().unwrap();
                    let t = threads.iter().find(|t| t["threadId"] == a["threadId"]).unwrap();
                    t["runs"].as_array().unwrap().iter().find(|r| a["runId"].is_null() || r["runId"] == a["runId"]).map(|r| r["status"].clone()).unwrap_or(json!("idle"))
                };
                if !ACTIVE.contains(&status.as_str().unwrap()) {
                    break json!({"threadId":a["threadId"],"runId":a["runId"],"status":status,"timedOut":false});
                }
                if tokio::time::timeout_at(deadline, notified).await.is_err() {
                    break json!({"threadId":a["threadId"],"runId":a["runId"],"status":status,"timedOut":true});
                }
            }
        }
        "t3_pending_request" if a["action"] == "list" => {
            if s.fail_requests.load(std::sync::atomic::Ordering::Acquire) { return fail("T3_CAPABILITY_MISSING") }
            json!({"requestIds":s.threads.lock().unwrap().iter().find(|t| t["threadId"] == a["threadId"]).unwrap()["pending"]})
        }
        "t3_pending_request" if a["action"] == "read" => json!({"requestId":a["requestId"],"questions":[{"id":"q","question":"Which engine first?","options":[]}]}),
        other => panic!("unexpected T3 call {other}"),
    };
    Json(json!({"jsonrpc":"2.0","id":id,"result":{"structuredContent":result,"content":[{"type":"text","text":result.to_string()}]}})).into_response()
}

struct Lab {
    dir: tempfile::TempDir,
    s: Arc<Fake>,
    server: tokio::task::JoinHandle<()>,
    sink: Arc<Sink>,
    w: Arc<Workers>,
    gate: Arc<DeliveryGate>,
}
impl Drop for Lab {
    fn drop(&mut self) {
        self.server.abort();
    }
}
impl Lab {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let s = Arc::new(Fake::default());
        *s.root.lock().unwrap() = std::fs::canonicalize(dir.path()).unwrap().to_string_lossy().into_owned();
        let app = Router::new()
            .route("/mcp", post(mcp))
            .route("/.well-known/t3/environment", get(|| async { Json(json!({"environmentId":"env"})) }))
            .with_state(s.clone());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let runtime = dir.path().join("runtime.json");
        std::fs::write(&runtime, json!({"port":origin.rsplit(':').next().unwrap().parse::<u16>().unwrap()}).to_string()).unwrap();
        let store = Arc::new(Store::default());
        store.save(&Credential { token: "t".into(), expires_ms: agent::now_ms() + 30 * 24 * 60 * 60 * 1000, environment: "env".into(), revoked: false }).unwrap();
        let client = Arc::new(Client::new(runtime, store).unwrap());
        let actions = Arc::new(Actions::open(&dir.path().join("actions.db")).unwrap());
        let sink = Arc::new(Sink::default());
        actions.attach(sink.clone());
        let w = Arc::new(Workers::open(&dir.path().join("workers.db"), client, actions).unwrap());
        // No waiting in tests: children closed or background left running are judged on the next pass.
        w.set_hold_timing(0, 60 * 60 * 1000);
        Self { dir, s, server, sink, w, gate: Arc::new(DeliveryGate::new(true)) }
    }
    fn p(&self) -> DeliveryPermit {
        self.gate.permit().unwrap()
    }
    async fn call(&self, tool: &str, args: Value) -> Result<String> {
        self.w.execute(tool, args, &uuid::Uuid::new_v4().to_string(), &self.p()).await
    }
    async fn pass(&self) {
        self.w.scan_all(&self.p()).await.unwrap();
    }
    fn calls(&self, name: &str) -> Vec<Value> {
        self.s.calls.lock().unwrap().iter().filter(|(n, _)| n == name).map(|(_, a)| a.clone()).collect()
    }
    fn deliveries(&self) -> Vec<(String, String)> {
        self.sink.deliveries.lock().unwrap().clone()
    }
    fn with<R>(&self, thread: &str, f: impl FnOnce(&mut Value) -> R) -> R {
        let mut threads = self.s.threads.lock().unwrap();
        let r = f(threads.iter_mut().find(|t| t["threadId"] == thread).unwrap());
        self.s.changed.notify_waiters();
        r
    }
    /// Append a timeline item to the thread's latest run; returns its id.
    fn item(&self, thread: &str, kind: &str, status: &str, text: &str, extra: Value) -> String {
        let run = self.with(thread, |t| t["runs"].as_array().unwrap().last().unwrap()["runId"].as_str().unwrap().to_owned());
        self.item_in(thread, &run, kind, status, text, extra)
    }
    /// Append a timeline item to one run of the thread; returns its id.
    fn item_in(&self, thread: &str, run: &str, kind: &str, status: &str, text: &str, extra: Value) -> String {
        self.with(thread, |t| {
            let pos = t["items"].as_array().unwrap().len();
            let run = json!(run);
            let id = format!("{}:item-{pos}", t["threadId"].as_str().unwrap());
            let mut i = json!({"position":pos,"itemId":id,"runId":run,"type":kind,"status":status,"text":text,"visibility":"local","createdBy":null,"updatedAt":iso(0)});
            for (k, v) in extra.as_object().into_iter().flatten() {
                i[k] = v.clone();
            }
            t["items"].as_array_mut().unwrap().push(i);
            t["updatedAt"] = json!(iso(0));
            id
        })
    }
    /// An item changes in place (a sub-agent closing): its status and its update time.
    fn set_item(&self, thread: &str, item: &str, status: &str) {
        self.with(thread, |t| {
            for i in t["items"].as_array_mut().unwrap().iter_mut().filter(|i| i["itemId"] == item) {
                i["status"] = json!(status);
                i["updatedAt"] = json!(iso(0));
            }
        })
    }
    /// A SendMessage to a finished Claude sub-agent resumes it: the same item, at its old position, moves to the
    /// thread's latest run and runs again (installed T3, 20 Oct 19:20, "Resuming agent a3eb6be").
    fn reopen(&self, thread: &str, item: &str) {
        self.with(thread, |t| {
            let run = t["runs"].as_array().unwrap().last().unwrap()["runId"].clone();
            for i in t["items"].as_array_mut().unwrap().iter_mut().filter(|i| i["itemId"] == item) {
                i["status"] = json!("running");
                i["runId"] = run.clone();
                i["updatedAt"] = json!(iso(0));
            }
        })
    }
    /// The turn T3 queues to hand finished sub-agents to the parent when they finished during its running turn:
    /// requested before that turn ends, no item until it starts (installed T3, 20 Oct: requested 19:18:59.539, the
    /// turn before completed .541, its first item, a notification, .573, started .602). Returns its run id.
    fn queue_run(&self, thread: &str) -> String {
        self.with(thread, |t| {
            let n = t["runs"].as_array().unwrap().len() + 1;
            let run = format!("run:{}:{n}", t["threadId"].as_str().unwrap());
            let model = t["model"].clone();
            t["runs"].as_array_mut().unwrap().push(json!({"runId":run,"ordinal":n,"status":"queued","model":model,"requestedAt":iso(0),"startedAt":null,"completedAt":null}));
            run
        })
    }
    /// Heimdall's app restarting: the same stores, a new process.
    fn restart(&mut self) {
        let (client, actions) = (self.w.client.clone(), self.w.actions.clone());
        self.w = Arc::new(Workers::open(&self.dir.path().join("workers.db"), client, actions).unwrap());
        self.w.set_hold_timing(0, 60 * 60 * 1000);
    }
    /// Heimdall's app quitting and starting again as a new process: the same data directory (workers and actions
    /// stores), new objects; the agent side (the sink) keeps what it was given, as Pi's session does.
    fn restart_process(&mut self) {
        t3::workers::new_process_for_tests(&self.dir.path().join("workers.db"));
        let actions = Arc::new(Actions::open(&self.dir.path().join("actions.db")).unwrap());
        actions.attach(self.sink.clone());
        self.w = Arc::new(Workers::open(&self.dir.path().join("workers.db"), self.w.client.clone(), actions).unwrap());
        self.w.set_hold_timing(0, 60 * 60 * 1000);
    }
    /// The app's watcher (event-backed waits and the 30-second pass), until `stop`.
    fn watcher(&self) -> (tokio::sync::watch::Sender<bool>, tokio::task::JoinHandle<()>) {
        let (stop, rx) = tokio::sync::watch::channel(false);
        (stop, tokio::spawn(self.w.clone().watch(self.gate.clone(), rx)))
    }
    async fn until(&self, what: &str, f: impl Fn(&[(String, String)]) -> bool) {
        for _ in 0..200 {
            if f(&self.deliveries()) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("timed out waiting for {what}: {:?}", self.deliveries());
    }
    fn end_run(&self, thread: &str, status: &str) {
        self.with(thread, |t| {
            let r = t["runs"].as_array_mut().unwrap().last_mut().unwrap();
            r["status"] = json!(status);
            r["completedAt"] = json!(iso(0));
            t["updatedAt"] = json!(iso(0));
        })
    }
    /// Set one run's status (a terminal one also gets its completion time).
    fn set_run(&self, thread: &str, run: &str, status: &str) {
        self.with(thread, |t| {
            let r = t["runs"].as_array_mut().unwrap().iter_mut().find(|r| r["runId"] == run).unwrap();
            r["status"] = json!(status);
            if !ACTIVE.contains(&status) {
                r["completedAt"] = json!(iso(0));
            } else if r["startedAt"].is_null() && status != "queued" {
                r["startedAt"] = json!(iso(0));
            }
            t["updatedAt"] = json!(iso(0));
        })
    }
    fn new_run(&self, thread: &str) {
        self.with(thread, |t| {
            let n = t["runs"].as_array().unwrap().len() + 1;
            let run = format!("run:{}:{n}", t["threadId"].as_str().unwrap());
            let model = t["model"].clone();
            t["runs"].as_array_mut().unwrap().push(json!({"runId":run,"ordinal":n,"status":"running","model":model,"requestedAt":iso(0),"startedAt":iso(0),"completedAt":null}));
        })
    }
    /// One of the user's own threads with `runs` finished runs and a long history: per run a user message, ten
    /// activity items and an assistant reply.
    fn owner_thread(&self, id: &str, title: &str, runs: usize) {
        let mut items = vec![];
        let mut run_rows = vec![];
        for r in 1..=runs {
            let run = format!("run:{id}:{r}");
            run_rows.push(json!({"runId":run,"ordinal":r,"status":"completed","model":"claude-opus-5-5","requestedAt":iso(-600),"startedAt":iso(-600),"completedAt":iso(-300)}));
            let mut push = |kind: &str, text: String, by: Value| {
                let pos = items.len();
                items.push(json!({"position":pos,"itemId":format!("{id}:i{pos}"),"runId":run,"type":kind,"status":"completed","text":text,"visibility":"local","createdBy":by,"updatedAt":iso(-300)}));
            };
            push("user_message", format!("Question {r} from User."), json!("user"));
            for k in 0..10 {
                push("command_execution", format!("$ step {k} of run {r}\nok"), Value::Null);
            }
            push("assistant_message", format!("Answer {r}: done."), json!("agent"));
        }
        // An old background command without its notification: history, never this run's work.
        items.push(json!({"position":items.len(),"itemId":format!("{id}:old-bg"),"runId":format!("run:{id}:1"),"type":"command_execution","status":"completed","text":format!("$ npm run dev\n{}x1. Output …", "Command running in background with ID: "),"visibility":"local","updatedAt":iso(-300)}));
        self.s.threads.lock().unwrap().push(json!({"threadId":id,"projectId":"p-vault","title":title,"model":"claude-opus-5-5","archived":false,"pending":[],"relationshipToParent":null,"updatedAt":iso(-300),"runs":run_rows,"items":items}));
    }
    /// A sub-agent thread (T3 `delegate_task` child) of `parent` with one run in `status` and `pending` request ids.
    fn sub_thread(&self, id: &str, title: &str, parent: &str, status: &str, pending: Value) {
        let run = format!("run:{id}:1");
        self.s.threads.lock().unwrap().push(json!({"threadId":id,"projectId":"p-vault","title":title,"model":"claude-haiku-5-5","archived":false,"pending":pending,
            "relationshipToParent":"subagent","parentThreadId":parent,"updatedAt":iso(0),
            "runs":[{"runId":run,"ordinal":1,"status":status,"model":"claude-haiku-5-5","requestedAt":iso(0),"startedAt":iso(0),"completedAt":null}],"items":[]}));
    }
    async fn delegate(&self, title: &str) -> String {
        let out = self.call("delegate_task", json!({"task":"Benchmark the engines.","title":title,"target":{"model":"claude-opus-5-5","options":{"effort":"high"}},"path":self.dir.path()})).await.unwrap();
        out.split(" as task ").nth(1).unwrap().split(' ').next().unwrap().to_owned()
    }
}

/// Run git in `dir` (tests make their own repositories in the lab's temporary folder).
fn git(dir: &std::path::Path, args: &[&str]) {
    let out = std::process::Command::new("git").arg("-C").arg(dir).args(["-c", "user.name=lab", "-c", "user.email=lab@example.invalid"]).args(args).output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

/// "spawning … it should take you like maximum one tool call". The model's provider is found
/// from the catalog; a wrong id answers with the usable models so the next call is right.
#[tokio::test]
async fn delegate_task_starts_a_thread_in_one_call() {
    let l = Lab::new().await;
    let out = l.call("delegate_task", json!({"task":"Find the largest file in ~/Downloads.","title":"Largest download","target":{"model":"claude-haiku-5-5","options":{"effort":"high"}}, "path":l.dir.path()})).await.unwrap();
    assert_eq!(out, "Started 'Largest download' as task mcp:task-0 (claude-haiku-5-5, effort high). Its result arrives as a notice when it is done; end your turn.");
    let launch = &l.calls("t3_thread_launch")[0];
    assert_eq!(launch["modelSelection"], json!({"instanceId":"claudeAgent","model":"claude-haiku-5-5","options":{"effort":"high"}}));
    assert_eq!(launch["runtimeMode"], "full-access");
    assert_eq!(launch["workspaceStrategy"], json!({"type":"root"}));
    assert_eq!(launch["message"], "Find the largest file in ~/Downloads.");
    assert!(launch["title"].as_str().unwrap().starts_with("Largest download [heimdall:"));
    // A replay of the same action answers from the record and never launches twice. (Worktree isolation needs the
    // project root to be a git repository: bug note 02-50-34.)
    git(l.dir.path(), &["init", "-q"]);
    let args = json!({"task":"x","title":"Once","target":{"model":"claude-opus-5-5"},"path":l.dir.path(),"isolation":"worktree","branch":"night-x"});
    let first = l.w.execute("delegate_task", args.clone(), "op-once", &l.p()).await.unwrap();
    assert_eq!(l.w.execute("delegate_task", args, "op-once", &l.p()).await.unwrap(), first);
    assert_eq!(l.calls("t3_thread_launch").len(), 2);
    assert_eq!(l.calls("t3_thread_launch")[1]["workspaceStrategy"], json!({"type":"worktree","baseRef":"HEAD","startFromOrigin":false,"branch":"night-x"}));
    // Wrong ids name the right ones; a disabled provider's copy of a model is no choice.
    let e = l.call("delegate_task", json!({"task":"x","title":"x","target":{"model":"claude-haiku-9"},"path":l.dir.path()})).await.unwrap_err().to_string();
    assert!(e.starts_with("MODEL_UNAVAILABLE: claude-haiku-9") && e.contains("claudeAgent claude-haiku-5-5 — effort low|medium*|high|max") && !e.contains("cursor"), "{e}");
    let e = l.call("delegate_task", json!({"task":"x","title":"x","target":{"model":"claude-opus-5-5","options":{"effort":"extreme"}},"path":l.dir.path()})).await.unwrap_err().to_string();
    assert_eq!(e, "OPTION_INVALID: effort takes one of low|medium|high|xhigh|max|ultracode|ultrathink");
    // The prompt's default target, on T3's current catalog shape: accepted (bug note 2026-10-23T01-29-13).
    l.call("delegate_task", json!({"task":"x","title":"Medium","target":{"model":"claude-opus-5-5","options":{"effort":"medium","fastMode":true}},"path":l.dir.path()})).await.unwrap();
    assert_eq!(l.calls("t3_thread_launch")[2]["modelSelection"], json!({"instanceId":"claudeAgent","model":"claude-opus-5-5","options":{"effort":"medium","fastMode":true}}));
    let e = l.call("delegate_task", json!({"task":"x","title":"x","target":{"model":"claude-opus-5-5","options":{"fastMode":"yes"}},"path":l.dir.path()})).await.unwrap_err().to_string();
    assert_eq!(e, "OPTION_INVALID: fastMode takes true or false");
    let e = l.call("delegate_task", json!({"task":"x","title":"x","target":{"model":"gpt-6-luna","providerInstanceId":"claudeAgent"},"path":l.dir.path()})).await.unwrap_err().to_string();
    assert!(e.starts_with("MODEL_UNAVAILABLE"), "{e}");
    assert_eq!(l.calls("t3_thread_launch").len(), 3, "rejected targets never launch");
    // The default task prompt (docs/skills.md) goes above every brief once set, also in a worktree.
    l.w.set_brief_preamble("Rules every task follows.\n");
    l.call("delegate_task", json!({"task":"Fix it.","title":"Ruled","target":{"model":"claude-opus-5-5"},"path":l.dir.path()})).await.unwrap();
    assert_eq!(l.calls("t3_thread_launch")[3]["message"], format!("Rules every task follows.\n\n{}\n\n---\n\nFix it.", t3::workers::AVOID_APPROVALS));
    l.call("delegate_task", json!({"task":"Fix it.","title":"Ruled tree","target":{"model":"claude-opus-5-5"},"path":l.dir.path(),"isolation":"worktree"})).await.unwrap();
    assert_eq!(l.calls("t3_thread_launch")[4]["message"], format!("Rules every task follows.\n\n{}\n\n---\n\nFix it.", t3::workers::AVOID_APPROVALS));
    // Review 1111: outside the Vault (no Vault CLAUDE.md loads there) the preamble keeps the two rules the Vault's
    // contract carries; inside it, it stays trimmed.
    let vault = l.dir.path().join("vault");
    std::fs::create_dir_all(&vault).unwrap();
    std::fs::write(vault.join("CLAUDE.md"), "# Vault Agent Contract\n").unwrap();
    l.w.set_outside_vault_rules("- Done means checked the way User will meet it.\n", &vault);
    l.call("delegate_task", json!({"task":"Fix it.","title":"Outside","target":{"model":"claude-opus-5-5"},"path":l.dir.path()})).await.unwrap();
    assert_eq!(l.calls("t3_thread_launch")[5]["message"], format!("Rules every task follows.\n- Done means checked the way User will meet it.\n\n{}\n\n---\n\nFix it.", t3::workers::AVOID_APPROVALS));
    l.call("delegate_task", json!({"task":"Fix it.","title":"Inside","target":{"model":"claude-opus-5-5"},"path":vault})).await.unwrap();
    let inside = l.calls("t3_thread_launch")[6]["message"].as_str().unwrap().to_owned();
    assert!(inside.starts_with("Rules every task follows.\n\n") && !inside.contains("Done means") && inside.ends_with("Fix it."), "{inside}");
    // A folder no project contains says so.
    let e = l.call("delegate_task", json!({"task":"x","title":"x","target":{"model":"claude-opus-5-5"},"path":"/"})).await.unwrap_err().to_string();
    assert!(e.starts_with("PROJECT_NOT_FOUND: no registered T3 project contains /"), "{e}");
}

/// Bug note 18-32-31, replayed from the benchmark worker of 19 Oct: run 1 ends with three research sub-agents
/// still working ("I'll pick this up when they report"), run 2 ends waiting on a background install, run 3 has the
/// result. Before: two "completed" wakes and five "stuck" wakes. Now: one notice, with run 3's reply.
#[tokio::test]
async fn a_turn_that_waits_on_its_sub_agents_or_background_work_wakes_nothing() {
    let l = Lab::new().await;
    l.w.set_self_close(false); // lane S: this rule is about a finished task that stays open (task_status, a later turn, a cleared island)
    let task = l.delegate("Qwen 27B engine benchmark").await;
    let agents: Vec<String> = ["Research Splash", "Research vllm-metal", "Research oMLX"]
        .iter()
        .map(|t| l.item(&task, "subagent", "running", "", json!({"title":t,"subagentId":format!("node:{t}")})))
        .collect();
    l.item(&task, "command_execution", "failed", "$ ps -o pid -p 38738\nNo such process", json!({}));
    l.item(&task, "assistant_message", "completed", "Atlas is cleaned and archived. Three research workers are now checking. I'll pick this up when they report.", json!({}));
    l.end_run(&task, "completed");
    l.pass().await;
    l.pass().await;
    assert!(l.deliveries().is_empty(), "{:?}", l.deliveries());
    let status = l.call("task_status", json!({})).await.unwrap();
    assert_eq!(status, format!("taskId · title · state · model:\n{task} · 'Qwen 27B engine benchmark' · waiting on 3 sub-agents · claude-opus-5-5"));
    // Two research agents finish; T3 starts run 2 to read them. It starts an install in the background and ends.
    l.set_item(&task, &agents[0], "completed");
    l.set_item(&task, &agents[1], "completed");
    l.new_run(&task);
    l.item(&task, "notification", "completed", "Subagent \"Research vllm-metal\" finished", json!({}));
    l.item(&task, "command_execution", "failed", "$ sleep 60\nError: Blocked", json!({}));
    l.item(&task, "command_execution", "completed", "$ until grep -q \"^exit\" ~/.cache/engine-compare-vllm/stable/install.log; do sleep 15; done; tail -6 install.log\nCommand running in background with ID: bver2oyer. Output is being written to: /private/tmp/x/tasks/bver2oyer.output. You will be notified when it completes.", json!({}));
    l.item(&task, "assistant_message", "completed", "Waiting on the vllm-metal install and the oMLX research.", json!({}));
    l.end_run(&task, "completed");
    l.pass().await;
    assert!(l.deliveries().is_empty(), "{:?}", l.deliveries());
    let one = l.call("task_status", json!({"taskId":task})).await.unwrap();
    assert!(one.contains("· waiting on 1 sub-agent and 1 background command") && one.contains("Background: until grep -q \"^exit\""), "{one}");
    assert!(one.ends_with("Latest reply:\nWaiting on the vllm-metal install and the oMLX research."), "{one}");
    // The last agent and the install finish; run 3 has the result.
    l.set_item(&task, &agents[2], "completed");
    l.new_run(&task);
    l.item(&task, "notification", "completed", "Subagent \"Research oMLX\" finished", json!({}));
    l.pass().await;
    l.item(&task, "notification", "completed", "Command \"until grep -q \"^exit\" ~/.cache/engine-compare-vllm/stable/install.log; do sleep 15; do…\" finished", json!({}));
    l.item(&task, "assistant_message", "completed", "Splash 94.5 tok/s at 1 stream, 174 at 8. DFlash2 wins on all three.", json!({}));
    l.end_run(&task, "completed");
    l.pass().await;
    l.pass().await;
    let got = l.deliveries();
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0].0, format!("terminal:{task}:run:{task}:3:completed"));
    // The held turns' replies ride before the result, in order (review 21 Oct: no reply is lost).
    assert!(
        got[0].1.starts_with(&format!(
            "Task 'Qwen 27B engine benchmark' ({task}) completed. Final reply:\n\nEarlier turn:\nAtlas is cleaned and archived. Three research workers are now checking. I'll pick this up when they report.\n\nEarlier turn:\nWaiting on the vllm-metal install and the oMLX research.\n\nLast turn:\nSplash 94.5 tok/s"
        )),
        "{}",
        got[0].1
    );
    assert!(!got.iter().any(|(id, _)| id.starts_with("stuck:")));
    assert!(l.call("task_status", json!({})).await.unwrap().contains("· done ·"));
}

/// Bug note 18-24-56: one failed tool call inside a running task is not a stall; nothing wakes until it ends.
#[tokio::test]
async fn failed_tool_calls_in_a_running_task_wake_nothing() {
    let l = Lab::new().await;
    let task = l.delegate("Engines").await;
    for k in 0..5 {
        l.item(&task, "command_execution", "failed", &format!("$ ls /opt/homebrew/Cellar/splash/1.3.{k}\nNo such file"), json!({}));
        l.pass().await;
    }
    assert!(l.deliveries().is_empty(), "{:?}", l.deliveries());
    l.item(&task, "assistant_message", "completed", "Done despite five failed commands.", json!({}));
    l.end_run(&task, "completed");
    l.pass().await;
    assert_eq!(l.deliveries().len(), 1);
    // A failed run is news at once, with its last reply.
    let failing = l.delegate("Fails").await;
    l.item(&failing, "assistant_message", "completed", "Provider error: overloaded.", json!({}));
    l.end_run(&failing, "failed");
    l.pass().await;
    assert!(l.deliveries()[1].1.starts_with("Task 'Fails'") && l.deliveries()[1].1.contains(" failed. Last reply:\n\nProvider error: overloaded."), "{:?}", l.deliveries());
}

/// Children that close without T3 starting another turn still release the held result (after the grace), and
/// background work left running for good (a preview server) is reported after the hold, named.
#[tokio::test]
async fn held_results_are_released_when_children_close_or_the_hold_runs_out() {
    let l = Lab::new().await;
    let task = l.delegate("Research").await;
    let agent = l.item(&task, "subagent", "running", "", json!({"title":"Reader"}));
    l.item(&task, "assistant_message", "completed", "Report written to Assistant/Reports/x.md.", json!({}));
    l.end_run(&task, "completed");
    l.pass().await;
    assert!(l.deliveries().is_empty());
    l.set_item(&task, &agent, "completed");
    l.pass().await; // children closed: the grace starts
    l.pass().await; // grace 0 in tests: delivered
    assert_eq!(l.deliveries().len(), 1, "{:?}", l.deliveries());
    assert!(l.deliveries()[0].1.contains("Final reply:\n\nReport written"));

    let preview = l.delegate("Preview").await;
    l.item(&preview, "command_execution", "completed", "$ npm run dev -- --port 4321\nCommand running in background with ID: b7. Output is being written to: /tmp/b7.output.", json!({}));
    l.item(&preview, "assistant_message", "completed", "Preview at http://localhost:4321.", json!({}));
    l.end_run(&preview, "completed");
    l.pass().await;
    assert_eq!(l.deliveries().len(), 1, "held while the background command is fresh");
    l.w.set_hold_timing(0, 0);
    l.pass().await;
    l.pass().await;
    let got = l.deliveries();
    assert_eq!(got.len(), 2, "{got:?}");
    assert!(got[1].1.contains("completed. It still has background work running: npm run dev -- --port 4321. Final reply:\n\nPreview at http://localhost:4321."), "{}", got[1].1);
    l.w.set_hold_timing(0, 60 * 60 * 1000);

    // A background command the agent killed is closed; an inherited (forked) one is not this thread's work.
    let killed = l.delegate("Killed").await;
    l.item(&killed, "command_execution", "completed", "$ tail -f log\nCommand running in background with ID: bk1. Output …", json!({}));
    l.item(&killed, "command_execution", "completed", "$ old server\nCommand running in background with ID: inh9. Output …", json!({"visibility":"inherited"}));
    l.item(&killed, "dynamic_tool", "completed", "{\n  \"toolName\": \"KillShell\",\n  \"input\": {\"shell_id\": \"bk1\"}\n}", json!({}));
    l.item(&killed, "assistant_message", "completed", "Stopped the log tail.", json!({}));
    l.end_run(&killed, "completed");
    l.pass().await;
    assert_eq!(l.deliveries().len(), 3, "{:?}", l.deliveries());
}

/// Day review T3: tasks Heimdall stopped itself sent "interrupted" notices with stale progress lines. task_cancel
/// stops and archives in one call, and the ending wakes nothing; so does an interrupt through t3_call.
#[tokio::test]
async fn own_stops_wake_nothing() {
    let l = Lab::new().await;
    let a = l.delegate("Widget prototype").await;
    l.item(&a, "assistant_message", "completed", "Rewriting the data model, glass panel and widget now…", json!({}));
    let out = l.call("task_cancel", json!({"taskId":a,"reason":"User scrapped the widget project."})).await.unwrap();
    assert_eq!(out, "Stopped and archived 'Widget prototype'.");
    assert_eq!(l.calls("t3_thread_interrupt")[0]["reason"], "User scrapped the widget project.");
    assert_eq!(l.calls("t3_thread_organize").last().unwrap(), &json!({"threadId":a,"action":"archive"}));
    let b = l.delegate("Widget research").await;
    l.call("t3_call", json!({"tool":"t3_thread_interrupt","args":{"threadId":b}})).await.unwrap();
    l.pass().await;
    l.pass().await;
    assert!(l.deliveries().is_empty(), "{:?}", l.deliveries());
    assert_eq!(l.call("task_status", json!({})).await.unwrap(), format!("taskId · title · state · model:\n{b} · 'Widget research' · interrupted · claude-opus-5-5"));
    // A finished task is only archived; another thread is not Heimdall's to cancel.
    l.end_run(&b, "interrupted");
    assert_eq!(l.call("task_cancel", json!({"taskId":b})).await.unwrap(), "Archived 'Widget research'.");
    assert_eq!(l.call("task_status", json!({})).await.unwrap(), "No open tasks.");
    l.owner_thread("owner", "User's research", 1);
    assert!(l.call("task_cancel", json!({"taskId":"owner"})).await.unwrap_err().to_string().starts_with("TASK_NOT_OWNED"));
}

/// Night 8 Nov log sweep, finding 1: the relay archived a finished task with task_cancel and the user's click for it
/// ("Try int8 once", 02:08:50) went to the archived thread, which T3 refuses (`thread_not_sendable`). A message for
/// Heimdall's own archived task unarchives it and is delivered; the user's own archived thread keeps T3's refusal.
#[tokio::test]
async fn a_message_for_an_archived_task_unarchives_it_and_is_delivered() {
    let l = Lab::new().await;
    let task = l.delegate("Rerun Nemotron on the Turkish clip").await;
    l.item(&task, "assistant_message", "completed", "Done: WER 0.31 forced Turkish.", json!({}));
    l.end_run(&task, "completed");
    assert_eq!(l.call("task_cancel", json!({"taskId":task,"reason":"Relayed and shown."})).await.unwrap(), "Archived 'Rerun Nemotron on the Turkish clip'.");
    let sent = l.call("t3_thread_send", json!({"threadId":task,"message":"The user clicked \"Try int8 once\"."})).await.unwrap();
    assert!(sent.starts_with(&format!("Sent to thread {task} (started")), "{sent}");
    assert_eq!(l.calls("t3_thread_organize").last().unwrap(), &json!({"threadId":task,"action":"unarchive"}));
    let sends = l.calls("t3_thread_send");
    assert_eq!(sends.len(), 2, "refused once, then delivered");
    let status = l.call("task_status", json!({})).await.unwrap();
    assert!(status.contains(&task) && status.contains("working"), "the task is followed again: {status}");
    // Its finish wakes the relay as any task's does.
    l.item(&task, "assistant_message", "completed", "int8: WER 0.33.", json!({}));
    l.end_run(&task, "completed");
    l.pass().await;
    assert!(l.deliveries().iter().any(|d| d.1.contains("int8: WER 0.33.")), "{:?}", l.deliveries());
    // The user's own archived thread is not Heimdall's to unarchive.
    l.owner_thread("owner", "User's research", 1);
    l.with("owner", |t| t["archived"] = json!(true));
    let e = l.call("t3_thread_send", json!({"threadId":"owner","message":"Carry on."})).await.unwrap_err().to_string();
    assert!(e.contains("thread_not_sendable"), "{e}");
    assert_eq!(l.calls("t3_thread_organize").iter().filter(|c| c["action"] == "unarchive").count(), 1);
}

/// Day review T1 ("if I have to wake you up and then tell you to read it and then tell you to
/// answer it like that's just stupid"): a message Heimdall sends into one of the user's threads wakes it when that run
/// ends, with the reply; the thread stays the user's (his later runs wake nothing, destructive calls still ask).
#[tokio::test]
async fn a_message_into_owners_thread_wakes_when_that_run_ends() {
    let l = Lab::new().await;
    l.owner_thread("t-qwen", "Compare Qwen 27B Inference Performance", 14);
    let sent = l.call("t3_thread_send", json!({"threadId":"t-qwen","message":"From User: check DFlash2 on every engine. Keep it short."})).await.unwrap();
    assert_eq!(sent, "Sent to thread t-qwen (started, running). Its reply arrives as a notice when this run ends.");
    // The old background command in the history is not this run's work; a sub-agent of this run is.
    let agent = l.item("t-qwen", "subagent", "running", "", json!({"title":"Check oMLX"}));
    l.item("t-qwen", "assistant_message", "completed", "Checking oMLX with a helper; back when it reports.", json!({}));
    l.end_run("t-qwen", "completed");
    l.pass().await;
    assert!(l.deliveries().is_empty(), "waits for its sub-agent: {:?}", l.deliveries());
    l.set_item("t-qwen", &agent, "completed");
    l.new_run("t-qwen");
    l.item("t-qwen", "assistant_message", "completed", "DFlash2 is fastest on Splash and oMLX; vllm-metal has no drafting for Qwen3.8.", json!({}));
    l.end_run("t-qwen", "completed");
    l.pass().await;
    let got = l.deliveries();
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(got[0].1.starts_with("Thread 'Compare Qwen 27B Inference Performance' (t-qwen), which you sent a message, completed. Final reply:\n\nEarlier turn:\nChecking oMLX with a helper; back when it reports.\n\nLast turn:\nDFlash2 is fastest"), "{}", got[0].1);
    // The user's next run there is his own business.
    l.new_run("t-qwen");
    l.item("t-qwen", "assistant_message", "completed", "Answer to User himself.", json!({}));
    l.end_run("t-qwen", "completed");
    l.pass().await;
    l.pass().await;
    assert_eq!(l.deliveries().len(), 1, "{:?}", l.deliveries());
    assert_eq!(l.call("task_status", json!({})).await.unwrap(), "No open tasks.", "a watched thread is not a task");
    let e = l.call("t3_call", json!({"tool":"t3_thread_interrupt","args":{"threadId":"t-qwen"}})).await.unwrap_err().to_string();
    assert!(e.starts_with("CONFIRM_REQUIRED"), "{e}");
    // A steer into his running turn follows that turn.
    l.new_run("t-qwen");
    let steered = l.call("t3_thread_send", json!({"threadId":"t-qwen","message":"Also note the cache.","mode":"steer"})).await.unwrap();
    assert!(steered.contains("(steered, running)"), "{steered}");
    l.item("t-qwen", "assistant_message", "completed", "Cache noted.", json!({}));
    l.end_run("t-qwen", "completed");
    l.pass().await;
    assert_eq!(l.deliveries().len(), 2);
    assert!(l.deliveries()[1].1.contains("Final reply:\n\nCache noted."));
}

/// Bug notes 18-17-04 and 18-29-07: checking three of the user's threads took seven calls (project list, thread list,
/// tool detail, three reads guessing afterPosition from itemCount), each read carrying ten runs and eighteen thread
/// fields. Now one call returns each thread's state line and its latest messages, whole.
#[tokio::test]
async fn reading_three_threads_is_one_compact_call() {
    let l = Lab::new().await;
    for (id, title, runs) in [("t-heim", "Resume Heimdall Project Work", 40), ("t-mob", "Mobius", 25), ("t-qwen", "Compare Qwen 27B", 14)] {
        l.owner_thread(id, title, runs);
    }
    let out = l.call("t3_thread_read", json!({"threadIds":["t-heim","t-mob","t-qwen"]})).await.unwrap();
    let sections: Vec<&str> = out.split("\n\n'").collect();
    assert_eq!(sections.len(), 3, "{out}");
    assert!(out.starts_with("'Resume Heimdall Project Work' · completed · updated 5 min ago · claude-opus-5-5 [thread t-heim]\nAssistant, "), "{out}");
    // The last three messages, oldest first: the 40th question, the 39th and 40th answers.
    let heim = sections[0];
    let a39 = heim.find("Answer 39: done.").unwrap();
    let q40 = heim.find("Question 40 from User.").unwrap();
    let a40 = heim.find("Answer 40: done.").unwrap();
    assert!(a39 < q40 && q40 < a40, "{heim}");
    assert!(heim.contains("\nThe user, "), "{heim}");
    for noise in ["recentRuns", "runId", "providerInstanceId", "creationSource", "sourceThreadId", "visibility", "itemCount", "\"", "{"] {
        assert!(!out.contains(noise), "{noise} reached the agent: {out}");
    }
    assert!(out.len() < 1200, "{} bytes", out.len());
    // last picks how many; details adds runs and settings only on request.
    let one = l.call("t3_thread_read", json!({"threadId":"t-mob","last":1})).await.unwrap();
    assert!(one.ends_with("\nAnswer 25: done.") && !one.contains("Runs"), "{one}");
    let detailed = l.call("t3_thread_read", json!({"threadId":"t-mob","last":1,"details":true})).await.unwrap();
    assert!(detailed.contains("\nRuns, newest first:\n- #25 completed") && detailed.contains("Settings: project p-vault"), "{detailed}");
    // The activity view: the newest items, one line each, and the whole text of one by its id.
    let activity = l.call("t3_thread_read", json!({"threadId":"t-qwen","view":"activity","last":3})).await.unwrap();
    let lines: Vec<&str> = activity.lines().skip(2).collect();
    assert_eq!(lines.len(), 3, "{activity}");
    assert_eq!(lines[2], "#168 command execution: $ npm run dev [item t-qwen:old-bg]", "{activity}");
    let whole = l.call("t3_thread_read", json!({"threadId":"t-qwen","itemId":"t-qwen:old-bg"})).await.unwrap();
    assert!(whole.ends_with("[thread t-qwen]\ncommand execution:\n$ npm run dev\nCommand running in background with ID: x1. Output …"), "{whole}");
    assert!(l.call("t3_thread_read", json!({"threadId":"t-qwen","threadIds":["t-mob"]})).await.is_err(), "one selector");
    // Through t3_call the raw read is compact too: no runs, no thread metadata.
    let raw = l.call("t3_call", json!({"tool":"t3_thread_read","args":{"threadId":"t-qwen","view":"activity","afterPosition":160,"limit":3,"runLimit":10}})).await.unwrap();
    assert!(raw.starts_with("'Compare Qwen 27B' · completed [thread t-qwen]\n#161 ") && !raw.contains("run:") && !raw.contains("model"), "{raw}");
}

/// A newest message larger than the read budget comes back as far as it fits, with the call that continues it; the
/// pages join exactly (Polish letters and emoji intact).
#[tokio::test]
async fn a_huge_latest_message_continues_exactly() {
    let l = Lab::new().await;
    l.owner_thread("t-big", "Big report", 2);
    let report = format!("Raport: zażółć gęślą jaźń 😀.\n\n{}\n\nKoniec.", "Akapit z emoji 🧪 i łódź. ".repeat(4000));
    l.with("t-big", |t| {
        let pos = t["items"].as_array().unwrap().len();
        t["items"].as_array_mut().unwrap().push(json!({"position":pos,"itemId":"t-big:report","runId":"run:t-big:2","type":"assistant_message","status":"completed","text":report,"visibility":"local","updatedAt":iso(-60)}));
    });
    let first = l.call("t3_thread_read", json!({"threadId":"t-big","last":2})).await.unwrap();
    let (body, more) = first.rsplit_once("\nMore: t3_thread_read ").expect("a continuation");
    assert!(first.contains("(1 earlier message did not fit; read fewer with last.)"), "{}", &first[first.len() - 300..]);
    let body = body.split_once("Assistant, ").unwrap().1.split_once(":\n").unwrap().1;
    let offset: u64 = more.rsplit("textOffset: ").next().unwrap().split('}').next().unwrap().parse().unwrap();
    let mut joined = body.to_owned();
    let mut next = Some(offset);
    while let Some(o) = next {
        let page = l.call("t3_thread_read", json!({"threadId":"t-big","itemId":"t-big:report","view":"messages","textOffset":o})).await.unwrap();
        let (text, more) = match page.rsplit_once("\nMore: t3_thread_read ") { Some((t, m)) => (t.to_owned(), Some(m.to_owned())), None => (page.clone(), None) };
        joined.push_str(text.split_once("assistant message:\n").unwrap().1);
        next = more.map(|m| m.rsplit("textOffset: ").next().unwrap().split('}').next().unwrap().parse().unwrap());
    }
    assert_eq!(joined, report.trim());
}

/// "List active threads with one-line status" (18-29-07): every project in one call, active first, sub-agents out.
#[tokio::test]
async fn thread_list_is_one_line_per_thread_active_first() {
    let l = Lab::new().await;
    l.owner_thread("t-old", "Old finished thread", 1);
    l.owner_thread("t-run", "Mobius", 2);
    l.new_run("t-run");
    l.owner_thread("t-sub", "Port Vella ASR into Mobius", 1);
    l.with("t-sub", |t| t["relationshipToParent"] = json!("subagent"));
    let mine = l.delegate("Live dashboard").await;
    let out = l.call("t3_thread_list", json!({})).await.unwrap();
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines[0], "3 threads, active first:", "{out}");
    assert!(lines[1].starts_with("'") && lines[1].contains(" · running · ") && lines[2].contains(" · running · "), "{out}");
    assert!(out.contains(&format!("'Live dashboard' · running · just now · your task [thread {mine}]")), "{out}");
    assert!(lines[3].starts_with("'Old finished thread' · completed · 5 min ago [thread t-old]"), "{out}");
    assert!(!out.contains("Vella"), "sub-agents are their parent's business: {out}");
    assert_eq!(l.call("t3_thread_list", json!({"query":"Mobius"})).await.unwrap(), "1 thread, active first:\n'Mobius' · running · 5 min ago [thread t-run]");
    assert_eq!(l.call("t3_thread_list", json!({"query":"zzz"})).await.unwrap(), "No threads with \"zzz\" in the title.");
}

/// Review 20 Oct (item 5): a result held for a sub-agent that never finishes, or that waits on a question nobody
/// answers, was held forever. At the hold limit it is delivered naming what it still waits on; the task stays
/// followed, so the turn that later reads the sub-agent brings the real result; a watched thread stays watched until
/// then, and following ends quietly when the sub-agent closes without such a turn.
#[tokio::test]
async fn a_stuck_sub_agent_holds_a_result_only_until_the_hold_limit() {
    let l = Lab::new().await;
    let task = l.delegate("Research").await;
    let agent = l.item(&task, "subagent", "running", "", json!({"title":"Reader"}));
    l.item(&task, "assistant_message", "completed", "Two findings so far; the reader is still going.", json!({}));
    l.end_run(&task, "completed");
    for _ in 0..3 {
        l.pass().await;
    }
    assert!(l.deliveries().is_empty(), "within the hold: {:?}", l.deliveries());
    l.w.set_subagent_hold(0);
    l.pass().await;
    let got = l.deliveries();
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0].0, format!("terminal:{task}:run:{task}:1:completed"));
    assert!(
        got[0].1.starts_with(&format!("Task 'Research' ({task}) completed. It is still waiting on sub-agent 'Reader'; a later result may follow. Final reply:\n\nTwo findings so far")),
        "{}",
        got[0].1
    );
    l.pass().await;
    l.pass().await;
    assert_eq!(l.deliveries().len(), 1, "one notice per held turn: {:?}", l.deliveries());
    assert!(l.call("task_status", json!({})).await.unwrap().contains("· waiting on 1 sub-agent ·"));
    // The reader finishes; T3's turn that reads it is the real result.
    l.set_item(&task, &agent, "completed");
    l.new_run(&task);
    l.item(&task, "assistant_message", "completed", "All five findings, final.", json!({}));
    l.end_run(&task, "completed");
    l.pass().await;
    assert_eq!(l.deliveries().len(), 2, "{:?}", l.deliveries());
    assert!(l.deliveries()[1].1.contains(" completed. Final reply:\n\nAll five findings, final."), "{}", l.deliveries()[1].1);

    // A watched thread of the user's: delivered at the limit, still followed for the turn that reads the sub-agent.
    l.owner_thread("t-w", "User's research", 2);
    l.call("t3_thread_send", json!({"threadId":"t-w","message":"Check the cache too."})).await.unwrap();
    let helper = l.item("t-w", "subagent", "running", "", json!({"title":"Cache check"}));
    l.item("t-w", "assistant_message", "completed", "Started a cache check.", json!({}));
    l.end_run("t-w", "completed");
    l.pass().await;
    l.pass().await;
    assert_eq!(l.deliveries().len(), 3, "{:?}", l.deliveries());
    assert!(l.deliveries()[2].1.contains("which you sent a message, completed. It is still waiting on sub-agent 'Cache check'"), "{}", l.deliveries()[2].1);
    l.set_item("t-w", &helper, "completed");
    l.new_run("t-w");
    l.item("t-w", "assistant_message", "completed", "Cache is cold on first run.", json!({}));
    l.end_run("t-w", "completed");
    l.pass().await;
    assert_eq!(l.deliveries().len(), 4, "{:?}", l.deliveries());
    assert!(l.deliveries()[3].1.contains("Final reply:\n\nCache is cold on first run."));
    l.new_run("t-w");
    l.item("t-w", "assistant_message", "completed", "the user's own next answer.", json!({}));
    l.end_run("t-w", "completed");
    l.pass().await;
    l.pass().await;
    assert_eq!(l.deliveries().len(), 4, "User's later runs are his own: {:?}", l.deliveries());

    // Worst case: the sub-agent closes after the limit and T3 starts no turn: no second notice, and the watch ends.
    l.owner_thread("t-q", "Quiet one", 1);
    l.call("t3_thread_send", json!({"threadId":"t-q","message":"One more check."})).await.unwrap();
    let quiet = l.item("t-q", "subagent", "running", "", json!({"title":"Check"}));
    l.item("t-q", "assistant_message", "completed", "Checking.", json!({}));
    l.end_run("t-q", "completed");
    l.pass().await;
    l.pass().await;
    assert_eq!(l.deliveries().len(), 5);
    l.set_item("t-q", &quiet, "completed");
    for _ in 0..3 {
        l.pass().await;
    }
    assert_eq!(l.deliveries().len(), 5, "{:?}", l.deliveries());
    l.new_run("t-q");
    l.item("t-q", "assistant_message", "completed", "The user asked something else.", json!({}));
    l.end_run("t-q", "completed");
    l.pass().await;
    assert_eq!(l.deliveries().len(), 5, "no longer watched: {:?}", l.deliveries());
}

/// Bug note 2026-10-28T02-20-08: T3 still counted a pending request on a task whose run had completed (no request id
/// listed), and an approval notice for that finished run came out beside its result. A running turn's approval is
/// reported; a finished one's leftover count is not.
#[tokio::test]
async fn a_finished_runs_leftover_pending_count_is_no_approval() {
    let l = Lab::new().await;
    let task = l.delegate("Technics manual").await;
    l.with(&task, |t| t["approvals"] = json!(1));
    l.pass().await;
    let approvals = |l: &Lab| l.deliveries().into_iter().filter(|(id, _)| id.starts_with("approval:")).count();
    assert_eq!(approvals(&l), 1, "a running turn's approval: {:?}", l.deliveries());
    l.item(&task, "assistant_message", "completed", "Manual delivered.", json!({}));
    l.end_run(&task, "completed");
    l.new_run(&task);
    l.item(&task, "assistant_message", "completed", "Moved it to Downloads.", json!({}));
    l.end_run(&task, "completed");
    l.pass().await;
    l.pass().await;
    assert_eq!(approvals(&l), 1, "no approval for a finished run: {:?}", l.deliveries());
    assert!(l.deliveries().iter().any(|(id, _)| id.starts_with("terminal:")), "{:?}", l.deliveries());
}

/// Bug notes 2026-11-02T16-05-08 and 16-19-09 ('Report: how IDOL works', 'Audit T3'): the task's last command tripped
/// Claude's rm check, Heimdall's hook answered, and T3 kept the approval "pending" after the run completed. Reading
/// that thread's pending requests failed (the renamed T3 tool), so the scan stopped before the terminal notice and
/// task_status said "done · waits on a question or approval". A completed run always wakes with its reply, whatever
/// leftover request T3 still counts, and nothing calls it waiting.
#[tokio::test]
async fn a_completed_run_with_a_leftover_approval_still_wakes_with_its_reply() {
    let l = Lab::new().await;
    let task = l.delegate("Report: how IDOL works").await;
    l.item(&task, "command_execution", "completed", "$ du -sh /tmp/idol && rm -rf /tmp/idol", json!({}));
    l.item(&task, "approval_request", "waiting", "du -sh /tmp/idol && rm -rf /tmp/idol", json!({}));
    l.with(&task, |t| t["approvals"] = json!(1));
    l.s.fail_requests.store(true, std::sync::atomic::Ordering::Release);
    l.item(&task, "assistant_message", "completed", "The IDOL report is ready.", json!({}));
    l.end_run(&task, "completed");
    l.pass().await;
    let got = l.deliveries();
    assert!(got.iter().any(|(id, text)| id.starts_with("terminal:") && text.contains("The IDOL report is ready.")), "{got:?}");
    assert!(!got.iter().any(|(id, _)| id.starts_with("approval:") || id.starts_with("question:")), "{got:?}");
    // Its leftover approval is not listed as a question and is no wait: the cleared island archives it.
    l.s.fail_requests.store(false, std::sync::atomic::Ordering::Release);
    assert!(l.w.clear_task(&task).unwrap());
    assert_eq!(l.w.archive_cleared().await.unwrap(), vec!["Report: how IDOL works".to_string()]);
    let status = l.call("task_status", json!({"taskId":task})).await.unwrap();
    assert!(status.contains("· done ·") && !status.contains("waits on"), "{status}");
}

/// Review 20 Oct (item 5): a sub-agent thread's question or approval was never heard (T3 counts it only on the
/// child, and sub-agent threads are left to their parent). It is reported as its task's, once, with the child thread
/// an answer needs; a sub-agent's own sub-agent reports to the same task; sub-agents of the user's threads are his.
#[tokio::test]
async fn a_sub_agents_question_or_approval_is_reported_as_its_tasks() {
    let l = Lab::new().await;
    let task = l.delegate("Engine research").await;
    let check = l.item(&task, "subagent", "running", "", json!({"title":"Check oMLX"}));
    l.item(&task, "assistant_message", "completed", "Waiting on the oMLX check.", json!({}));
    l.end_run(&task, "completed");
    l.pass().await;
    assert!(l.deliveries().is_empty());
    l.sub_thread("c-omlx", "Check oMLX", &task, "waiting", json!(["req-1"]));
    // A running intermediate sub-agent (not in the waiting list) whose own sub-agent waits on an approval.
    l.sub_thread("c-mid", "Survey", &task, "running", json!([]));
    l.sub_thread("c-deep", "Install probe", "c-mid", "waiting", json!([]));
    // A sub-agent of one of the user's own threads is his business.
    l.owner_thread("t-own", "User's thread", 1);
    l.sub_thread("c-owner", "His helper", "t-own", "waiting", json!(["req-9"]));
    l.pass().await;
    l.pass().await;
    let got = l.deliveries();
    assert_eq!(got.len(), 2, "{got:?}");
    let question = got.iter().find(|(id, _)| id == "question:c-omlx:req-1").expect("the sub-agent's question");
    assert_eq!(
        question.1,
        format!("Task 'Engine research' ({task}) asks through its sub-agent 'Check oMLX' [thread c-omlx, request req-1]:\nWhich engine first? [question q]")
    );
    let approval = got.iter().find(|(id, _)| id == "approval:c-deep:run:c-deep:1").expect("the nested sub-agent's approval");
    assert_eq!(approval.1, format!("Task 'Engine research' ({task}): its sub-agent 'Install probe' waits for an approval in T3; only the user can give it there [thread c-deep]."));
    assert!(!got.iter().any(|(id, _)| id.contains("c-owner")), "{got:?}");
    // The question waits for the user after a start (the harness classifies notices by id).
    assert_eq!(agent::notice::NoticeKind::classify(&question.0, &question.1), agent::notice::NoticeKind::Question);
    // Nothing in flight, nothing listed: a settled task costs no sweep.
    let lists = l.calls("t3_thread_list").len();
    l.set_item(&task, &check, "completed");
    l.item(&task, "notification", "completed", "Subagent \"Check oMLX\" finished", json!({}));
    l.new_run(&task);
    l.item(&task, "assistant_message", "completed", "oMLX wins.", json!({}));
    l.end_run(&task, "completed");
    l.pass().await;
    l.pass().await;
    assert!(l.deliveries().iter().any(|(_, t)| t.contains("Final reply:\n\nEarlier turn:\nWaiting on the oMLX check.\n\nLast turn:\noMLX wins.")), "{:?}", l.deliveries());
    assert!(lists > 0, "swept while the task was in flight");
    let before = l.calls("t3_thread_list").len();
    l.pass().await;
    assert_eq!(l.calls("t3_thread_list").len(), before, "no sweep once the task is settled");
}

/// Review 20 Oct (t3_thread_send): a queued message runs after the thread's current turn, whose ending is not its
/// reply. Installed T3: the send returns the queued run; a run held in the queue does not count for the thread's
/// status, so the current turn's end showed as "completed". The reply is that run's own; a message the user queues
/// behind it does not delay or replace it. The promise says what happens.
#[tokio::test]
async fn a_queued_message_is_answered_by_its_own_turn() {
    let l = Lab::new().await;
    l.owner_thread("t-busy", "Busy thread", 2);
    l.new_run("t-busy");
    l.item("t-busy", "assistant_message", "completed", "User's turn, half way.", json!({}));
    let sent = l.call("t3_thread_send", json!({"threadId":"t-busy","message":"After this, summarize.","mode":"queue"})).await.unwrap();
    assert_eq!(sent, "Sent to thread t-busy (queued, queued). It waits behind the thread's current turn; its reply arrives as a notice when that queued turn ends. Queued run run:t-busy:4: t3_call with t3_queue (action edit or cancel) and this queuedRunId changes or takes it back.");
    let mine = "run:t-busy:4";
    // The queue is held: the user's turn ends, and the thread shows "completed" from it.
    l.with("t-busy", |t| t["runs"][3]["queueHeld"] = json!(true));
    l.item_in("t-busy", "run:t-busy:3", "assistant_message", "completed", "User's turn, done.", json!({}));
    l.set_run("t-busy", "run:t-busy:3", "completed");
    l.pass().await;
    l.pass().await;
    assert!(l.deliveries().is_empty(), "User's turn is not the reply: {:?}", l.deliveries());
    // The queue resumes; the user queues another message behind Heimdall's while it runs.
    l.with("t-busy", |t| t["runs"][3]["queueHeld"] = json!(false));
    l.set_run("t-busy", mine, "running");
    l.pass().await;
    l.with("t-busy", |t| t["runs"].as_array_mut().unwrap().push(json!({"runId":"run:t-busy:5","ordinal":5,"status":"queued","model":"claude-opus-5-5","requestedAt":iso(-1),"startedAt":null,"completedAt":null})));
    l.item_in("t-busy", mine, "assistant_message", "completed", "Summary: three engines, DFlash2 fastest.", json!({}));
    l.set_run("t-busy", mine, "completed");
    l.set_run("t-busy", "run:t-busy:5", "running");
    l.pass().await;
    let got = l.deliveries();
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0].0, format!("terminal:t-busy:{mine}:completed"));
    assert!(got[0].1.contains("which you sent a message, completed. Final reply:\n\nSummary: three engines, DFlash2 fastest."), "{}", got[0].1);
    l.item_in("t-busy", "run:t-busy:5", "assistant_message", "completed", "User's queued answer.", json!({}));
    l.set_run("t-busy", "run:t-busy:5", "completed");
    l.pass().await;
    l.pass().await;
    assert_eq!(l.deliveries().len(), 1, "{:?}", l.deliveries());
}

/// The prompt sends a later request to a running task with mode queue. T3 starts the queued turn the moment the current
/// one ends, so the thread never showed that turn finished and its result was lost. Both results arrive, in order.
#[tokio::test]
async fn a_message_queued_into_a_task_keeps_the_current_result() {
    let l = Lab::new().await;
    let task = l.delegate("Engines").await;
    l.pass().await;
    let sent = l.call("t3_thread_send", json!({"threadId":task,"message":"Then compare memory use.","mode":"queue"})).await.unwrap();
    assert!(sent.contains("(queued, queued)") && sent.contains("when that queued turn ends"), "{sent}");
    l.pass().await;
    let first = format!("run:{task}:1");
    let queued = format!("run:{task}:2");
    l.item_in(&task, &first, "assistant_message", "completed", "Speeds: Splash 94 tok/s.", json!({}));
    l.set_run(&task, &first, "completed");
    l.set_run(&task, &queued, "running");
    l.pass().await;
    let got = l.deliveries();
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0].0, format!("terminal:{task}:{first}:completed"));
    assert!(got[0].1.contains("completed. Final reply:\n\nSpeeds: Splash 94 tok/s."), "{}", got[0].1);
    l.pass().await;
    assert_eq!(l.deliveries().len(), 1);
    l.item_in(&task, &queued, "assistant_message", "completed", "Memory: Splash 18 GB.", json!({}));
    l.set_run(&task, &queued, "completed");
    l.pass().await;
    l.pass().await;
    let got = l.deliveries();
    assert_eq!(got.len(), 2, "{got:?}");
    assert!(got[1].1.contains("Final reply:\n\nMemory: Splash 18 GB."), "{}", got[1].1);
}

/// Review 20 Oct (t3_thread_send): the direct path (reads and idempotent calls) skipped the watch and the adoption.
/// The installed T3 marks send, launch and fork destructive; a build marking send idempotent must still be followed.
#[tokio::test]
async fn a_send_is_followed_whatever_its_annotations_say() {
    let l = Lab::new().await;
    l.s.idempotent_send.store(true, std::sync::atomic::Ordering::Relaxed);
    l.owner_thread("t-x", "User's thread", 1);
    let sent = l.call("t3_call", json!({"tool":"t3_thread_send","args":{"threadId":"t-x","message":"Status?"}})).await.unwrap();
    assert!(sent.ends_with("Its reply arrives as a notice when this run ends."), "{sent}");
    l.item("t-x", "assistant_message", "completed", "All green.", json!({}));
    l.end_run("t-x", "completed");
    l.pass().await;
    assert_eq!(l.deliveries().len(), 1, "{:?}", l.deliveries());
    assert!(l.deliveries()[0].1.contains("Final reply:\n\nAll green."));
}

/// Bug note 19-19-06, replayed from the installed T3's thread "Research: best tiny decider model to train" (20 Oct
/// 19:15–19:20). Three Claude background sub-agents; the first one's notification starts run 2; the other two close
/// during run 2, which ends "still waiting on the vision/omni model sweep" 3 s after that sweep closed. T3 queues
/// run 3 ("Background activity updated") 2 ms before run 2 ends, so run 2 looked like a turn with a message queued
/// behind it, and its progress line woke Heimdall as "completed". Now: one notice, run 3's result. The same when T3
/// requests run 3 just after run 2 ends, or the scan sees run 3's notification already.
#[tokio::test]
async fn a_turn_whose_sub_agents_finished_meanwhile_is_not_the_result() {
    for case in ["queued-before-end", "requested-after-end", "notification-seen"] {
        let l = Lab::new().await;
        // The real grace (45 s): T3's turn arrives within it.
        l.w.set_hold_timing(45_000, 60 * 60 * 1000);
        let task = l.delegate("Research: best tiny decider model to train").await;
        let [text, vision, tune] = ["Research tiny text LLMs on HF", "Research tiny vision/omni models", "Research fine-tuning approach"]
            .map(|t| l.item(&task, "subagent", "running", "", json!({"title":t,"subagentId":format!("node:{t}")})));
        l.item(&task, "assistant_message", "completed", "Local reading is done. I'm waiting on the three research workers before writing the file.", json!({}));
        l.end_run(&task, "completed");
        l.pass().await;
        assert!(l.deliveries().is_empty(), "{case}: {:?}", l.deliveries());
        // The text agent finishes; its notification starts run 2 (followed while it runs).
        l.set_item(&task, &text, "completed");
        l.new_run(&task);
        l.item(&task, "notification", "completed", "Subagent \"Research tiny text LLMs on HF\" finished", json!({}));
        l.pass().await;
        // The other two close while run 2 runs; run 2 ends without the vision result.
        l.set_item(&task, &tune, "completed");
        l.item(&task, "reasoning", "completed", "While waiting on the vision agent, I'm noting licensing.", json!({}));
        l.set_item(&task, &vision, "completed");
        l.item(&task, "assistant_message", "completed", "Two of the three research workers are back. Still waiting on the vision/omni model sweep before writing the file.", json!({}));
        let r2 = format!("run:{task}:2");
        let r3 = match case {
            "requested-after-end" => {
                l.set_run(&task, &r2, "completed");
                l.pass().await;
                l.pass().await;
                assert!(l.deliveries().is_empty(), "{case}: {:?}", l.deliveries());
                l.new_run(&task);
                format!("run:{task}:3")
            }
            _ => {
                let r3 = l.queue_run(&task);
                l.set_run(&task, &r2, "completed");
                if case == "notification-seen" {
                    l.item_in(&task, &r3, "notification", "completed", "Background activity updated", json!({}));
                }
                l.pass().await;
                l.pass().await;
                assert!(l.deliveries().is_empty(), "{case}: {:?}", l.deliveries());
                r3
            }
        };
        if case != "notification-seen" {
            l.item_in(&task, &r3, "notification", "completed", "Background activity updated", json!({}));
        }
        l.set_run(&task, &r3, "running");
        l.pass().await;
        assert!(l.deliveries().is_empty(), "{case}: {:?}", l.deliveries());
        assert!(l.call("task_status", json!({})).await.unwrap().contains("· working ·"), "{case}");
        l.item_in(&task, &r3, "assistant_message", "completed", "I recommend fine-tuning the current model again rather than switching.", json!({}));
        l.set_run(&task, &r3, "completed");
        l.pass().await;
        l.pass().await;
        let got = l.deliveries();
        assert_eq!(got.len(), 1, "{case}: {got:?}");
        assert_eq!(got[0].0, format!("terminal:{task}:{r3}:completed"), "{case}");
        // Both progress turns ride before the result, in order: nothing they said is lost.
        assert!(
            got[0].1.contains("completed. Final reply:\n\nEarlier turn:\nLocal reading is done. I'm waiting on the three research workers before writing the file.\n\nEarlier turn:\nTwo of the three research workers are back. Still waiting on the vision/omni model sweep before writing the file.\n\nLast turn:\nI recommend fine-tuning"),
            "{case}: {}",
            got[0].1
        );
    }
}

/// The other side of the same rule: a sub-agent the turn waited for (launched and finished inside it, the parent
/// idle meanwhile) is the turn's own business, and its result goes out at once. A background sub-agent the turn
/// launched and worked beside, finishing just before the turn ends with no turn from T3 after it (the parent read it
/// in the turn), is delivered after the short grace.
#[tokio::test]
async fn sub_agents_a_turn_waited_for_or_read_itself_delay_nothing_for_long() {
    let l = Lab::new().await;
    let task = l.delegate("Quick check").await;
    let fg = l.item(&task, "subagent", "running", "", json!({"title":"Look up the licence"}));
    l.set_item(&task, &fg, "completed");
    l.item(&task, "assistant_message", "completed", "MIT.", json!({}));
    l.end_run(&task, "completed");
    l.pass().await;
    assert_eq!(l.deliveries().len(), 1, "{:?}", l.deliveries());

    let bg = l.delegate("Beside").await;
    let helper = l.item(&bg, "subagent", "running", "", json!({"title":"Helper"}));
    l.item(&bg, "command_execution", "completed", "$ ls\nok", json!({}));
    l.set_item(&bg, &helper, "completed");
    l.item(&bg, "assistant_message", "completed", "Helper's answer folded in: done.", json!({}));
    l.end_run(&bg, "completed");
    l.pass().await;
    assert_eq!(l.deliveries().len(), 1, "within the grace: {:?}", l.deliveries());
    l.pass().await;
    assert_eq!(l.deliveries().len(), 2, "{:?}", l.deliveries());
    assert!(l.deliveries()[1].1.contains("Final reply:\n\nHelper's answer folded in: done."));
}

/// Replayed from "Heimdall launch: competitors, pricing, name ideas" (20 Oct 19:13–19:34): Heimdall sends the user's new
/// direction into its task, and the task resumes its finished pricing worker with SendMessage. Claude re-opens the
/// same sub-agent item at its old position, so following the activity from the cursor never sees it open again; the
/// turn that ends "it's revising pricing.md now" is not the result. The turn after the worker finishes is.
#[tokio::test]
async fn a_resumed_sub_agent_holds_its_task_again() {
    let l = Lab::new().await;
    let task = l.delegate("Heimdall launch: competitors, pricing, name ideas").await;
    let pricing = l.item(&task, "subagent", "running", "", json!({"title":"Pricing research"}));
    let names = l.item(&task, "subagent", "running", "", json!({"title":"Name candidates research"}));
    l.item(&task, "assistant_message", "completed", "Two research workers are running.", json!({}));
    l.end_run(&task, "completed");
    l.pass().await;
    l.set_item(&task, &pricing, "completed");
    l.new_run(&task);
    l.item(&task, "notification", "completed", "Subagent \"Pricing research\" finished", json!({}));
    l.item(&task, "assistant_message", "completed", "Pricing file is in. One worker is still running.", json!({}));
    l.end_run(&task, "completed");
    l.pass().await;
    assert!(l.deliveries().is_empty(), "{:?}", l.deliveries());
    l.set_item(&task, &names, "completed");
    l.new_run(&task);
    l.item(&task, "notification", "completed", "Subagent \"Pricing research\" and subagent \"Name candidates research\" finished", json!({}));
    l.item(&task, "assistant_message", "completed", "Pricing and names are in.", json!({}));
    l.end_run(&task, "completed");
    l.pass().await;
    assert_eq!(l.deliveries().len(), 1, "{:?}", l.deliveries());
    // The user's new direction; the task resumes the pricing worker.
    l.call("t3_thread_send", json!({"threadId":task,"message":"More from User (19:19), use it in pricing.md."})).await.unwrap();
    l.item(&task, "dynamic_tool", "completed", "{\n  \"toolName\": \"SendMessage\",\n  \"input\": {\"to\": \"a3eb6be0adc11a3b7\"},\n  \"output\": {\"success\": true, \"message\": \"Resuming agent a3eb6be\", \"resumedAgentId\": \"a3eb6be0adc11a3b7\"}\n}", json!({}));
    l.reopen(&task, &pricing);
    l.item(&task, "assistant_message", "completed", "I've sent User's new direction to the pricing worker, and it's revising pricing.md now.", json!({}));
    l.end_run(&task, "completed");
    for _ in 0..3 {
        l.pass().await;
    }
    assert_eq!(l.deliveries().len(), 1, "{:?}", l.deliveries());
    assert!(l.call("task_status", json!({})).await.unwrap().contains("waiting on 1 sub-agent"));
    l.set_item(&task, &pricing, "completed");
    l.new_run(&task);
    l.item(&task, "notification", "completed", "Subagent \"Pricing research\" finished", json!({}));
    l.item(&task, "assistant_message", "completed", "Pricing is done with User's direction.", json!({}));
    l.end_run(&task, "completed");
    l.pass().await;
    l.pass().await;
    let got = l.deliveries();
    assert_eq!(got.len(), 2, "{got:?}");
    assert!(
        got[0].1.contains("Final reply:\n\nEarlier turn:\nTwo research workers are running.\n\nEarlier turn:\nPricing file is in. One worker is still running.\n\nLast turn:\nPricing and names are in."),
        "{}",
        got[0].1
    );
    // Delivered turns never ride again: only the turn held for the resumed worker comes before this one.
    assert!(
        got[1].1.contains("Final reply:\n\nEarlier turn:\nI've sent User's new direction to the pricing worker, and it's revising pricing.md now.\n\nLast turn:\nPricing is done with User's direction.\n\n"),
        "{}",
        got[1].1
    );
}

/// The pending case of 20 Oct 19:24: Heimdall stopped with "Heimdall launch" held for its sub-agent "Names second
/// pass (Opus)", and "Heimdall landing page prototype" still running. Both finished while Heimdall was off. After
/// the restart each result arrives once, the final one, and nothing more follows.
#[tokio::test]
async fn results_held_or_running_at_a_stop_arrive_once_after_the_restart() {
    let mut l = Lab::new().await;
    let launch = l.delegate("Heimdall launch: competitors, pricing, name ideas").await;
    let second = l.item(&launch, "subagent", "running", "", json!({"title":"Names second pass (Opus)"}));
    l.item(&launch, "assistant_message", "completed", "Merged the corrected Fish licence. Waiting on the pricing revision and the names second pass.", json!({}));
    l.end_run(&launch, "completed");
    let landing = l.delegate("Heimdall landing page prototype").await;
    l.item(&landing, "command_execution", "completed", "$ npm run build\nok", json!({}));
    l.pass().await;
    l.pass().await;
    assert!(l.deliveries().is_empty(), "{:?}", l.deliveries());
    // Heimdall is off from here.
    l.new_run(&launch);
    l.item(&launch, "notification", "completed", "Subagent \"Pricing research\" finished", json!({}));
    l.item(&launch, "assistant_message", "completed", "Pricing is done. The names second pass is still running.", json!({}));
    l.end_run(&launch, "completed");
    l.set_item(&launch, &second, "completed");
    l.new_run(&launch);
    l.item(&launch, "notification", "completed", "Subagent \"Names second pass (Opus)\" finished", json!({}));
    l.item(&launch, "assistant_message", "completed", "I wrote the four files: competitors, pricing, voice and names.", json!({}));
    l.end_run(&launch, "completed");
    let review = l.item(&landing, "subagent", "running", "", json!({"title":"Independent design review of Heimdall page"}));
    l.item(&landing, "assistant_message", "completed", "Spawned the reviewer; waiting on it.", json!({}));
    l.end_run(&landing, "completed");
    l.set_item(&landing, &review, "completed");
    l.new_run(&landing);
    l.item(&landing, "notification", "completed", "Subagent \"Independent design review of Heimdall page\" finished", json!({}));
    l.item(&landing, "assistant_message", "completed", "Fixed the reviewer's findings; the page is at lab/landing/index.html.", json!({}));
    l.end_run(&landing, "completed");
    l.restart();
    for _ in 0..3 {
        l.pass().await;
    }
    let got = l.deliveries();
    assert_eq!(got.len(), 2, "{got:?}");
    let text = |thread: &str| got.iter().find(|(id, _)| id.starts_with(&format!("terminal:{thread}:"))).map(|(_, t)| t.clone()).unwrap_or_default();
    // The turns that ended while Heimdall was off ride before each result, in order.
    assert!(
        text(&launch).contains("Final reply:\n\nEarlier turn:\nMerged the corrected Fish licence. Waiting on the pricing revision and the names second pass.\n\nEarlier turn:\nPricing is done. The names second pass is still running.\n\nLast turn:\nI wrote the four files"),
        "{}",
        text(&launch)
    );
    assert!(text(&landing).contains("Final reply:\n\nEarlier turn:\nSpawned the reviewer; waiting on it.\n\nLast turn:\nFixed the reviewer's findings"), "{}", text(&landing));
    assert!(got.iter().any(|(id, _)| *id == format!("terminal:{launch}:run:{launch}:3:completed")), "{got:?}");
    assert!(got.iter().any(|(id, _)| *id == format!("terminal:{landing}:run:{landing}:2:completed")), "{got:?}");
    l.pass().await;
    assert_eq!(l.deliveries().len(), 2);
}

/// Worst case of the wait for a queued run to show what started it: it never starts and shows nothing. After the
/// grace the earlier turn counts as answered before a message (the rule before 21 Oct), so nothing is held for good.
#[tokio::test]
async fn a_queued_run_that_never_shows_its_start_holds_nothing_past_the_grace() {
    let l = Lab::new().await;
    let task = l.delegate("Engines").await;
    l.pass().await;
    l.item(&task, "assistant_message", "completed", "Speeds: Splash 94 tok/s.", json!({}));
    l.w.set_hold_timing(45_000, 60 * 60 * 1000);
    let first = format!("run:{task}:1");
    l.queue_run(&task);
    l.set_run(&task, &first, "completed");
    l.pass().await;
    assert!(l.deliveries().is_empty(), "within the grace: {:?}", l.deliveries());
    l.w.set_hold_timing(0, 60 * 60 * 1000);
    l.pass().await;
    let got = l.deliveries();
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0].0, format!("terminal:{task}:{first}:completed"));
}

/// Review 21 Oct (night 2, item 3): holding back a progress turn must never lose what it said. The turn that read two
/// of three sub-agents carried their results ("Two of three back: A…, B…; waiting on C"); T3's turn after the third
/// says only "All done". One notice brings every withheld turn's reply, in order, then the last one.
#[tokio::test]
async fn a_withheld_turns_results_ride_in_the_next_notice_in_order() {
    let l = Lab::new().await;
    let task = l.delegate("Engine sweep").await;
    let [a, b, c] = ["Splash", "oMLX", "vllm-metal"].map(|t| l.item(&task, "subagent", "running", "", json!({"title":t})));
    l.item(&task, "assistant_message", "completed", "Three engine workers launched.", json!({}));
    l.end_run(&task, "completed");
    l.pass().await;
    // A and B finish; T3's turn reads them and waits on C.
    l.set_item(&task, &a, "completed");
    l.set_item(&task, &b, "completed");
    l.new_run(&task);
    l.item(&task, "notification", "completed", "Subagent \"Splash\" and subagent \"oMLX\" finished", json!({}));
    l.item(&task, "assistant_message", "completed", "Two of three back: Splash 94 tok/s, oMLX 71 tok/s; waiting on vllm-metal.", json!({}));
    l.end_run(&task, "completed");
    l.pass().await;
    l.pass().await;
    assert!(l.deliveries().is_empty(), "{:?}", l.deliveries());
    // C finishes; T3's turn says only that it is done.
    l.set_item(&task, &c, "completed");
    l.new_run(&task);
    l.item(&task, "notification", "completed", "Subagent \"vllm-metal\" finished", json!({}));
    l.item(&task, "assistant_message", "completed", "All done: vllm-metal 40 tok/s.", json!({}));
    l.end_run(&task, "completed");
    l.pass().await;
    l.pass().await;
    let got = l.deliveries();
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0].0, format!("terminal:{task}:run:{task}:3:completed"));
    assert!(
        got[0].1.contains(
            "completed. Final reply:\n\nEarlier turn:\nThree engine workers launched.\n\nEarlier turn:\nTwo of three back: Splash 94 tok/s, oMLX 71 tok/s; waiting on vllm-metal.\n\nLast turn:\nAll done: vllm-metal 40 tok/s.\n\n"
        ),
        "{}",
        got[0].1
    );
    // Carried once: the task's next turn brings only its own reply.
    l.call("t3_thread_send", json!({"threadId":task,"message":"Memory too?"})).await.unwrap();
    l.item(&task, "assistant_message", "completed", "Splash 18 GB.", json!({}));
    l.end_run(&task, "completed");
    l.pass().await;
    let got = l.deliveries();
    assert_eq!(got.len(), 2, "{got:?}");
    assert!(got[1].1.contains("Final reply:\n\nSplash 18 GB.\n\n"), "{}", got[1].1);
}

/// Review 21 Oct: a run queued behind a finished turn is T3's own only when its first input is T3's hand-over of
/// child work, by the installed T3's own strings. A model switch puts a handoff item before the queued message; pull
/// request watches and commands wake with notifications of their own. Those are messages: the earlier turn is the
/// result at once. A delegated task's report is a hand-over: the earlier turn rides in the next notice.
#[tokio::test]
async fn only_t3s_hand_over_of_child_work_supersedes_a_turn() {
    // (case, the queued run's first items as (type, text), whether T3 hands over child work)
    type Starts = &'static [(&'static str, &'static str)];
    let cases: [(&str, Starts, bool); 5] = [
        ("model switch", &[("handoff", "Provider handoff"), ("user_message", "Then compare memory use.")], false),
        ("pull request", &[("notification", "#42: checks passed")], false),
        ("command", &[("notification", "Command \"cargo test\" (exit 0) finished")], false),
        ("delegated task", &[("notification", "Delegated task \"Benchmark\" finished")], true),
        ("sub-agents counted", &[("notification", "4 subagents finished")], true),
    ];
    for (case, starts, own) in cases {
        let l = Lab::new().await;
        l.w.set_hold_timing(45_000, 60 * 60 * 1000);
        let task = l.delegate("Engines").await;
        l.pass().await;
        let first = format!("run:{task}:1");
        l.item(&task, "assistant_message", "completed", "Speeds: Splash 94 tok/s.", json!({}));
        let next = l.queue_run(&task);
        l.set_run(&task, &first, "completed");
        for (kind, text) in starts {
            l.item_in(&task, &next, kind, "completed", text, json!({}));
        }
        l.set_run(&task, &next, "running");
        l.pass().await;
        let got = l.deliveries();
        if own {
            assert!(got.is_empty(), "{case}: {got:?}");
        } else {
            assert_eq!(got.len(), 1, "{case}: {got:?}");
            assert_eq!(got[0].0, format!("terminal:{task}:{first}:completed"), "{case}");
            assert!(got[0].1.contains("Final reply:\n\nSpeeds: Splash 94 tok/s.\n\n"), "{case}: {}", got[0].1);
        }
        l.item_in(&task, &next, "assistant_message", "completed", "Memory: Splash 18 GB.", json!({}));
        l.set_run(&task, &next, "completed");
        l.pass().await;
        l.pass().await;
        let got = l.deliveries();
        let last = got.last().map(|(_, t)| t.as_str()).unwrap_or_default();
        if own {
            assert_eq!(got.len(), 1, "{case}: {got:?}");
            assert!(last.contains("Final reply:\n\nEarlier turn:\nSpeeds: Splash 94 tok/s.\n\nLast turn:\nMemory: Splash 18 GB.\n\n"), "{case}: {last}");
        } else {
            assert_eq!(got.len(), 2, "{case}: {got:?}");
            assert!(last.contains("Final reply:\n\nMemory: Splash 18 GB.\n\n"), "{case}: {last}");
        }
    }
}

/// Bug note 2026-11-03T23-16-29: the Arsenal site task (mcp:1fc57965) was found archived with its re-colour dead.
/// Archiving a thread ends its Claude session and every process it started, so Heimdall never archives a cleared task
/// while its work still runs, even after its result went out: a background command whose output is still written, a
/// command nothing can check (no output file) within `UNCHECKED_HOLD_MS`, or a Monitor within its timeout. Once that
/// work ends, the cleared task is archived as before.
#[tokio::test]
async fn a_cleared_task_with_live_background_work_or_a_monitor_is_never_archived() {
    let l = Lab::new().await;
    l.w.set_hold_timing(0, 0);
    let monitor = |id: &str| format!(
        "{{\n  \"toolName\": \"Monitor\",\n  \"input\": {},\n  \"output\": {}\n}}",
        json!({"description":"loco chain: failures, milestones","timeout_ms":1800000,"command":"tail -f out/loco7.log"}),
        json!({"taskId":id,"timeoutMs":1800000,"persistent":false})
    );
    let mut colour = Background::start(l.dir.path(), "bmc32z24v");
    let live = l.delegate("Arsenal site").await;
    // As it was started: a 1,900-character heredoc before the command, so T3's page of 1,500 characters per item cuts
    // the "Command running in background" line off.
    let heredoc = format!("cd lab/showcases && python3 - <<'EOF'\n{}EOF\nnice -n 10 python3 -u loco.py > out/loco7.log 2>&1", "s=s.replace(\"\"\"def step_colour(smooth):\"\"\", \"\"\"def step_colour(smooth, size=512):\"\"\")\n".repeat(25));
    assert!(heredoc.len() > 1600);
    l.item(&live, "command_execution", "completed", &colour.item(&heredoc, "bmc32z24v"), json!({}));
    let unchecked = l.delegate("Voice render").await;
    l.item(&unchecked, "command_execution", "completed", "$ python3 render.py\nCommand running in background with ID: bun1. Output …", json!({}));
    let watched = l.delegate("Model download").await;
    l.item(&watched, "dynamic_tool", "completed", &monitor("bmon1"), json!({}));
    for t in [&live, &unchecked, &watched] {
        l.item(t, "assistant_message", "completed", "Started; it runs in the background.", json!({}));
        l.end_run(t, "completed");
    }
    l.pass().await;
    l.pass().await;
    // Each result went out (the hold is over), and the user cleared each island.
    assert_eq!(l.deliveries().len(), 3, "{:?}", l.deliveries());
    for t in [&live, &unchecked, &watched] {
        assert!(l.w.clear_task(t).unwrap());
    }
    assert!(l.w.archive_cleared().await.unwrap().is_empty());
    l.pass().await;
    assert!(l.calls("t3_thread_organize").is_empty(), "live work: never archived: {:?}", l.calls("t3_thread_organize"));
    assert!(l.calls("t3_thread_interrupt").is_empty());
    assert_eq!(l.w.background_work(&live).unwrap(), 1, "the island reads it as background work");
    // The re-colour ends (nothing writes its output), the command reports, the monitor stops: each is archived.
    colour.finish();
    for (t, text) in [(&unchecked, "Command \"python3 render.py\" finished"), (&watched, "Monitor \"loco chain: failures, milestones\" was stopped")] {
        l.new_run(t);
        l.item(t, "notification", "completed", text, json!({}));
        l.item(t, "assistant_message", "completed", "Done.", json!({}));
        l.end_run(t, "completed");
    }
    l.pass().await;
    l.pass().await;
    let _ = l.w.archive_cleared().await.unwrap();
    let archived: Vec<Value> = l.calls("t3_thread_organize").into_iter().map(|a| a["threadId"].clone()).collect();
    for t in [&live, &unchecked, &watched] {
        assert!(archived.contains(&json!(t)), "{t} archived once its work ended: {archived:?}");
    }
}

/// Fresh review, 4 Nov, of the read-on of cut command items (bug note 2026-11-03T23-16-29): a long foreground output
/// that quotes a background command's line (a grep of an old transcript) is no background command, so its cleared task
/// is archived as before and its island shows nothing in the background.
#[tokio::test]
async fn a_long_output_quoting_the_background_line_is_no_background_command() {
    let l = Lab::new().await;
    l.w.set_self_close(false); // lane S: this rule is about a finished task that stays open (task_status, a later turn, a cleared island)
    l.w.set_hold_timing(0, 0);
    let task = l.delegate("Transcript search").await;
    let text = format!(
        "$ grep -rn 'running in background' transcripts/\n{}transcripts/old.jsonl:12: Command running in background with ID: bfake1. Output is being written to: /tmp/old/bfake1.output.\n{}",
        "transcripts/a.jsonl:1: nothing here\n".repeat(60),
        "transcripts/b.jsonl:3: more lines\n".repeat(20)
    );
    l.item(&task, "command_execution", "completed", &text, json!({}));
    l.item(&task, "assistant_message", "completed", "Found one old background line.", json!({}));
    l.end_run(&task, "completed");
    l.pass().await;
    l.pass().await;
    assert_eq!(l.w.background_work(&task).unwrap(), 0);
    assert!(l.w.clear_task(&task).unwrap());
    let _ = l.w.archive_cleared().await.unwrap();
    assert_eq!(l.calls("t3_thread_organize"), vec![json!({"threadId":task,"action":"archive"})]);
}

/// A background command as Claude runs it: its output file, held open by the command's processes until it ends.
struct Background {
    child: std::process::Child,
    path: String,
}
impl Background {
    fn start(dir: &std::path::Path, id: &str) -> Self {
        let path = dir.join(format!("{id}.output")).to_string_lossy().into_owned();
        let child = std::process::Command::new("/bin/sh").arg("-c").arg(format!("exec sleep 120 > '{path}' 2>&1")).spawn().unwrap();
        while !std::path::Path::new(&path).exists() {
            std::thread::sleep(Duration::from_millis(5));
        }
        Self { child, path }
    }
    /// The item T3 shows for it (installed T3 0.1.1, 21 Oct 00:57, thread mcp:3b0e9161, position 277).
    fn item(&self, command: &str, id: &str) -> String {
        format!("$ {command}\nCommand running in background with ID: {id}. Output is being written to: {}. You will be notified when it completes. To check interim output, use Read on that file path.", self.path)
    }
    fn finish(&mut self) {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
    }
}
impl Drop for Background {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Bug note 2026-10-21T01-28-46, replayed from the TTS research task (mcp:3b0e9161): its final reply came at 01:07
/// and woke nothing until 01:29. Four background downloads were still counted open: two finished while a later turn
/// ran (T3 writes no item for that), and the two "finished" notifications T3 did write quote the command's
/// description ("Wait for Maya1 download"), not its command line, so nothing matched and the reply waited out the
/// 30-minute hold. Now a finished turn's background commands count only while something writes their output file.
#[tokio::test]
async fn a_final_reply_is_not_held_by_background_commands_that_finished_unseen() {
    let l = Lab::new().await;
    l.w.set_self_close(false); // lane S: this rule is about a finished task that stays open (task_status, a later turn, a cleared island)
    let task = l.delegate("Research: small high-quality TTS for Heimdall").await;
    let mut chatterbox = Background::start(l.dir.path(), "b0a5nzni8");
    let mut voxcpm = Background::start(l.dir.path(), "bvt6app9s");
    let cmd = |m: &str| format!("until grep -qE \"^(OK|FAIL)\" fetch-{m}.log; do sleep 15; done; cat fetch-{m}.log | grep -E \"^(OK|FAIL)\" | cut -c1-80");
    l.item(&task, "command_execution", "completed", &chatterbox.item(&cmd("mlx-community_chatterbox-turbo-fp16"), "b0a5nzni8"), json!({}));
    l.item(&task, "command_execution", "completed", &voxcpm.item(&cmd("mlx-community_VoxCPM2-bf16"), "bvt6app9s"), json!({}));
    l.item(&task, "assistant_message", "completed", "Downloads are still running; I'll render each model as it lands and then build the page.", json!({}));
    l.end_run(&task, "completed");
    l.pass().await;
    l.pass().await;
    assert!(l.deliveries().is_empty(), "both downloads still run: {:?}", l.deliveries());
    assert!(l.call("task_status", json!({})).await.unwrap().contains("waiting on 2 background commands"));
    // Another command's notification starts turn 2 (its description, matching nothing). Both downloads finish while
    // it runs: no item. It starts the Maya1 wait in the background and ends.
    l.new_run(&task);
    l.item(&task, "notification", "completed", "Command \"Install UTMOS naturalness predictor in separate venv\" finished", json!({}));
    chatterbox.finish();
    voxcpm.finish();
    let mut maya = Background::start(l.dir.path(), "byy7tyfva");
    l.item(&task, "command_execution", "completed", &maya.item(&cmd("maya-research_maya1"), "byy7tyfva"), json!({}));
    l.item(&task, "assistant_message", "completed", "Waiting for the last three downloads (Maya1, VibeVoice, Kyutai); I'll render them and finish the page when they land.", json!({}));
    l.end_run(&task, "completed");
    l.pass().await;
    assert!(l.deliveries().is_empty(), "the Maya1 wait still runs: {:?}", l.deliveries());
    assert!(l.call("task_status", json!({})).await.unwrap().contains("waiting on 1 background command"), "the finished downloads no longer count");
    // The Maya1 wait finishes while idle: T3 starts turn 3 with its description; turn 3 has the result.
    maya.finish();
    l.new_run(&task);
    l.item(&task, "notification", "completed", "Command \"Wait for Maya1 download\" finished", json!({}));
    l.item(&task, "assistant_message", "completed", "1. I can't listen to audio, so these are my best guesses. Page with all 11 models: compare.html", json!({}));
    l.end_run(&task, "completed");
    l.pass().await;
    l.pass().await;
    let got = l.deliveries();
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0].0, format!("terminal:{task}:run:{task}:3:completed"));
    assert!(got[0].1.contains("Last turn:\n1. I can't listen to audio") && !got[0].1.contains("still has background work"), "{}", got[0].1);
    assert!(l.call("task_status", json!({})).await.unwrap().contains("· done ·"));
}

/// Bug note 2026-10-22T22-19-38, replayed from task mcp:6b581c50 ("Install planned voice pipeline"): the worker ended a
/// turn after each benchmark shard ("Shard 3 of 8 is running"), and every turn woke Heimdall with a "completed" notice
/// naming the same background commands, including a check run and a commit that had ended long before (only commands
/// within `BACKGROUND_HOLD_MS` were probed). Now finished commands of any age are settled, one notice names the work
/// still running, the shard turns after it wake nothing, and the final result carries their replies. The withholding
/// ends `SUBAGENT_HOLD_MS` after that notice.
#[tokio::test]
async fn shard_turns_waiting_on_background_work_already_reported_wake_nothing() {
    let l = Lab::new().await;
    // Every background command is past its hold at once.
    l.w.set_hold_timing(0, 0);
    let task = l.delegate("Install planned voice pipeline").await;
    let mut check = Background::start(l.dir.path(), "bijwaztup");
    let mut bench = Background::start(l.dir.path(), "bl7nqie41");
    l.item(&task, "command_execution", "completed", &check.item("CARGO_BUILD_JOBS=2 CC=/usr/bin/clang nice -n 19 scripts/check.sh > /tmp/heimdall-voice-install-check.log 2>&1", "bijwaztup"), json!({}));
    check.finish();
    l.item(&task, "command_execution", "completed", &bench.item("OUT=lab/eval/data/screen-bench && python3 lab/eval/bench.py --shards 8", "bl7nqie41"), json!({}));
    l.item(&task, "assistant_message", "completed", "Installed. The benchmark runs in 8 shards; shard 1 of 8 is running.", json!({}));
    l.end_run(&task, "completed");
    l.pass().await;
    l.pass().await;
    let got = l.deliveries();
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(got[0].1.contains("It still has background work running: OUT=lab/eval/data/screen-bench") && !got[0].1.contains("check.sh"), "{}", got[0].1);
    // Shards 2-5: each turn ends with only the reported benchmark running.
    for shard in 2..=5 {
        l.new_run(&task);
        l.item(&task, "notification", "completed", &format!("Command \"Wait for shard {}\" finished", shard - 1), json!({}));
        l.item(&task, "assistant_message", "completed", &format!("Shard {shard} of 8 is running."), json!({}));
        l.end_run(&task, "completed");
        l.pass().await;
        l.pass().await;
        assert_eq!(l.deliveries().len(), 1, "shard {shard}: {:?}", l.deliveries());
    }
    // The benchmark ends; the next turn has the result, with the shard turns' replies before it.
    bench.finish();
    l.new_run(&task);
    l.item(&task, "notification", "completed", "Command \"Run the voice benchmark\" finished", json!({}));
    l.item(&task, "assistant_message", "completed", "All 8 shards done: the planned pipeline wins on every screen.", json!({}));
    l.end_run(&task, "completed");
    l.pass().await;
    l.pass().await;
    let got = l.deliveries();
    assert_eq!(got.len(), 2, "{got:?}");
    assert_eq!(got[1].0, format!("terminal:{task}:run:{task}:6:completed"));
    let shards = (2..=5).map(|k| format!("Earlier turn:\nShard {k} of 8 is running.\n\n")).collect::<String>();
    assert!(got[1].1.contains(&format!("Final reply:\n\n{shards}Last turn:\nAll 8 shards done")) && !got[1].1.contains("background work"), "{}", got[1].1);

    // A turn a message started is an answer and goes out; past `SUBAGENT_HOLD_MS` after the notice, a turn waiting
    // on the same work goes out too.
    let task = l.delegate("Long soak").await;
    let soak = Background::start(l.dir.path(), "bsoak1");
    l.item(&task, "command_execution", "completed", &soak.item("scripts/soak.sh", "bsoak1"), json!({}));
    l.item(&task, "assistant_message", "completed", "Soak started.", json!({}));
    l.end_run(&task, "completed");
    l.pass().await;
    assert_eq!(l.deliveries().len(), 3, "{:?}", l.deliveries());
    l.new_run(&task);
    l.item(&task, "user_message", "completed", "How is it going?", json!({}));
    l.item(&task, "assistant_message", "completed", "Soak at hour 2, no errors.", json!({}));
    l.end_run(&task, "completed");
    l.pass().await;
    let got = l.deliveries();
    assert_eq!(got.len(), 4, "{got:?}");
    assert!(got[3].1.contains("Final reply:\n\nSoak at hour 2"), "{}", got[3].1);
    l.new_run(&task);
    l.item(&task, "notification", "completed", "Command \"Check the soak log\" finished", json!({}));
    l.item(&task, "assistant_message", "completed", "Soak at hour 3, no errors.", json!({}));
    l.end_run(&task, "completed");
    l.pass().await;
    assert_eq!(l.deliveries().len(), 4, "{:?}", l.deliveries());
    l.w.set_subagent_hold(0);
    l.pass().await;
    let got = l.deliveries();
    assert_eq!(got.len(), 5, "{got:?}");
    assert!(got[4].1.contains("It still has background work running: scripts/soak.sh") && got[4].1.contains("Final reply:\n\nSoak at hour 3"), "{}", got[4].1);

    // Review 23 Oct: a turn T3 started to hand over finished sub-agents is news, even with the reported work running.
    l.w.set_subagent_hold(30 * 60 * 1000);
    l.new_run(&task);
    l.item(&task, "notification", "completed", "4 subagents finished", json!({}));
    l.item(&task, "assistant_message", "completed", "All four reviews are in: ship it.", json!({}));
    l.end_run(&task, "completed");
    l.pass().await;
    let got = l.deliveries();
    assert_eq!(got.len(), 6, "{got:?}");
    assert!(got[5].1.contains("All four reviews are in"), "{}", got[5].1);
}

/// Bug notes 2026-10-24T20-06-04 and 2026-10-25T21-22-04, replayed from tasks mcp:dffaf13e ("Decider v4: master
/// dataset") and mcp:b2d9d14a ("Drama 3 BF16 download"): each worker armed a Claude Monitor on its own background job,
/// and every event the monitor printed started a turn with T3's "Background activity updated" ("Labelling is at 121
/// of 318", "Shard 4 of 16 is done"). That start counted as T3 handing over finished sub-agents, which is news, and
/// the Monitor was not counted as background work at all, so each tick woke Heimdall. Now a Monitor is background work
/// until its stop, a TaskStop or its own timeout; once a notice has named the work, the event turns ride in the next
/// notice; a re-armed monitor over the same work is not news; and the turn after the job's "finished" is the result
/// even while the monitor still runs.
#[tokio::test]
async fn monitor_event_turns_over_reported_background_work_wake_nothing() {
    let l = Lab::new().await;
    l.w.set_hold_timing(0, 0);
    let task = l.delegate("Drama 3 BF16 download").await;
    let mut download = Background::start(l.dir.path(), "bxuaynr0t");
    l.item(&task, "command_execution", "completed", &download.item("nice -n 19 python3 -I fetch.py >> fetch.log 2>&1", "bxuaynr0t"), json!({}));
    let monitor = |id: &str, what: &str| {
        // A long command line: the task id comes after the page's 1,500 characters per item.
        let command = format!("LOG=fetch.log; n=0; while true; do {} sleep 20; done", "t=$(wc -l < \"$LOG\"); ".repeat(60));
        // T3 writes the tool name first (installed T3, mcp:b2d9d14a position 27).
        format!(
            "{{\n  \"toolName\": \"Monitor\",\n  \"input\": {},\n  \"output\": {}\n}}",
            json!({"description":what,"timeout_ms":1800000,"command":command}),
            json!({"taskId":id,"timeoutMs":1800000,"persistent":false})
        )
    };
    l.item(&task, "dynamic_tool", "completed", &monitor("bi2qmljy0", "Drama BF16 download progress, retries, failures and completion"), json!({}));
    l.item(&task, "assistant_message", "completed", "The download is running; I'm waiting on the progress monitor.", json!({}));
    l.end_run(&task, "completed");
    l.pass().await;
    l.pass().await;
    let got = l.deliveries();
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(got[0].1.contains("monitor \"Drama BF16 download progress, retries, failures and completion\"") && got[0].1.contains("fetch.py"), "{}", got[0].1);
    // Shard events: each a turn T3 starts with "Background activity updated".
    for shard in 1..=3 {
        l.new_run(&task);
        l.item(&task, "notification", "completed", "Background activity updated", json!({}));
        l.item(&task, "assistant_message", "completed", &format!("Shard {shard} of 16 is done; still downloading."), json!({}));
        l.end_run(&task, "completed");
        l.pass().await;
        l.pass().await;
        assert_eq!(l.deliveries().len(), 1, "shard {shard}: {:?}", l.deliveries());
    }
    // The monitor times out and is re-armed with the same description: still progress.
    l.new_run(&task);
    l.item(&task, "notification", "completed", "Monitor \"Drama BF16 download progress, retries, failures and completion\" was stopped", json!({}));
    l.item(&task, "dynamic_tool", "completed", &monitor("b7a7rlvzd", "Drama BF16 download progress, retries, failures and completion"), json!({}));
    l.item(&task, "assistant_message", "completed", "The monitor expired; re-armed for the final exit only.", json!({}));
    l.end_run(&task, "completed");
    l.pass().await;
    l.pass().await;
    assert_eq!(l.deliveries().len(), 1, "re-arm: {:?}", l.deliveries());
    l.new_run(&task);
    l.item(&task, "notification", "completed", "Background activity updated", json!({}));
    l.item(&task, "assistant_message", "completed", "Shard 12 of 16 is done.", json!({}));
    l.end_run(&task, "completed");
    l.pass().await;
    l.pass().await;
    assert_eq!(l.deliveries().len(), 1, "after the re-arm: {:?}", l.deliveries());
    // The download finishes; the monitor still runs. That turn is the result, with the progress before it.
    download.finish();
    l.new_run(&task);
    l.item(&task, "notification", "completed", "Command \"Resume BF16 download from partial blobs\" finished", json!({}));
    l.item(&task, "assistant_message", "completed", "The full-precision Drama 3 is downloaded and verified.", json!({}));
    l.end_run(&task, "completed");
    l.pass().await;
    l.pass().await;
    let got = l.deliveries();
    assert_eq!(got.len(), 2, "{got:?}");
    assert!(got[1].1.contains("Earlier turn:\nShard 1 of 16 is done") && got[1].1.contains("Last turn:\nThe full-precision Drama 3 is downloaded"), "{}", got[1].1);

    // A Monitor alone (mcp:dffaf13e: its runners' commands had ended): named once, then its ticks wake nothing, until
    // a TaskStop ends it; a sub-agent's result handed over by "Background activity updated" is news.
    let task = l.delegate("Decider v4: master dataset").await;
    l.item(&task, "dynamic_tool", "completed", &monitor("bmon1", "labelling stages complete or error"), json!({}));
    l.item(&task, "assistant_message", "completed", "Labelling is at 95 of 318; the monitor reports each stage.", json!({}));
    l.end_run(&task, "completed");
    l.pass().await;
    l.pass().await;
    assert_eq!(l.deliveries().len(), 3, "{:?}", l.deliveries());
    for n in [108, 121, 135] {
        l.new_run(&task);
        l.item(&task, "notification", "completed", "Background activity updated", json!({}));
        l.item(&task, "assistant_message", "completed", &format!("Labelling is at {n} of 318. Still waiting."), json!({}));
        l.end_run(&task, "completed");
        l.pass().await;
        l.pass().await;
        assert_eq!(l.deliveries().len(), 3, "{n}: {:?}", l.deliveries());
    }
    let reviewer = l.item(&task, "subagent", "running", "", json!({"title":"Review the rubric"}));
    l.pass().await;
    l.set_item(&task, &reviewer, "completed");
    l.pass().await;
    l.new_run(&task);
    l.item(&task, "notification", "completed", "Background activity updated", json!({}));
    l.item(&task, "assistant_message", "completed", "The rubric review is back: two fixes.", json!({}));
    l.end_run(&task, "completed");
    l.pass().await;
    l.pass().await;
    let got = l.deliveries();
    assert_eq!(got.len(), 4, "{got:?}");
    assert!(got[3].1.contains("The rubric review is back"), "{}", got[3].1);
    l.new_run(&task);
    l.item(&task, "notification", "completed", "Background activity updated", json!({}));
    l.item(&task, "dynamic_tool", "completed", "{\n  \"toolName\": \"TaskStop\",\n  \"input\": {\"task_id\": \"bmon1\"}\n}", json!({}));
    l.item(&task, "assistant_message", "completed", "All 318 labelled; dataset written.", json!({}));
    l.end_run(&task, "completed");
    l.pass().await;
    l.pass().await;
    let got = l.deliveries();
    assert_eq!(got.len(), 5, "{got:?}");
    assert!(got[4].1.contains("All 318 labelled") && !got[4].1.contains("background work"), "{}", got[4].1);
}

/// Review 27 Oct of the Monitor rule: a different watch is news once; a background command that finishes and reports
/// through "Background activity updated" while a monitor runs is the result; a mixed "Monitor … and command …"
/// notification closes both; a Monitor item still starting (no task id yet) is tracked once it has one.
#[tokio::test]
async fn monitor_rule_edges() {
    let l = Lab::new().await;
    l.w.set_self_close(false); // lane S: this rule is about a finished task that stays open (task_status, a later turn, a cleared island)
    l.w.set_hold_timing(0, 0);
    let monitor = |id: Option<&str>, what: &str| {
        let output = id.map_or(String::new(), |id| format!(",\n  \"output\": {}", json!({"taskId":id,"timeoutMs":1800000,"persistent":false})));
        format!("{{\n  \"toolName\": \"Monitor\",\n  \"input\": {}{output}\n}}", json!({"description":what,"timeout_ms":1800000,"command":"tail -f run.log"}))
    };
    let task = l.delegate("Train the decider").await;
    let mut train = Background::start(l.dir.path(), "btrain1");
    l.item(&task, "command_execution", "completed", &train.item("python3 train.py", "btrain1"), json!({}));
    // The monitor item while it starts: no task id yet.
    let m = l.item(&task, "dynamic_tool", "running", &monitor(None, "training loss and errors"), json!({}));
    l.pass().await;
    l.with(&task, |t| {
        for i in t["items"].as_array_mut().unwrap().iter_mut().filter(|i| i["itemId"] == m) {
            i["status"] = json!("completed");
            i["text"] = json!(monitor(Some("bmonA"), "training loss and errors"));
        }
    });
    l.item(&task, "assistant_message", "completed", "Training runs; the monitor reports the loss.", json!({}));
    l.end_run(&task, "completed");
    l.pass().await;
    l.pass().await;
    let got = l.deliveries();
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(got[0].1.contains("monitor \"training loss and errors\""), "a monitor read before its id is still tracked: {}", got[0].1);
    // A different watch is news once.
    l.new_run(&task);
    l.item(&task, "notification", "completed", "Background activity updated", json!({}));
    l.item(&task, "dynamic_tool", "completed", &monitor(Some("bmonB"), "GPU memory"), json!({}));
    l.item(&task, "assistant_message", "completed", "Step 200: loss 0.52. Watching GPU memory too.", json!({}));
    l.end_run(&task, "completed");
    l.pass().await;
    l.pass().await;
    assert_eq!(l.deliveries().len(), 2, "{:?}", l.deliveries());
    // Ticks after that wake nothing.
    l.new_run(&task);
    l.item(&task, "notification", "completed", "Background activity updated", json!({}));
    l.item(&task, "assistant_message", "completed", "Step 400: loss 0.47.", json!({}));
    l.end_run(&task, "completed");
    l.pass().await;
    l.pass().await;
    assert_eq!(l.deliveries().len(), 2, "{:?}", l.deliveries());
    // The training command ends unseen; its end reaches the next turn as "Background activity updated": the result.
    train.finish();
    l.new_run(&task);
    l.item(&task, "notification", "completed", "Background activity updated", json!({}));
    l.item(&task, "assistant_message", "completed", "Training finished: loss 0.31, checkpoint saved.", json!({}));
    l.end_run(&task, "completed");
    l.pass().await;
    l.pass().await;
    let got = l.deliveries();
    assert_eq!(got.len(), 3, "{got:?}");
    assert!(got[2].1.contains("Training finished"), "{}", got[2].1);
    // A mixed notification closes the monitor and the command it names.
    let task = l.delegate("Fetch the weights").await;
    let fetch = Background::start(l.dir.path(), "bfetch1");
    l.item(&task, "command_execution", "completed", &fetch.item("python3 fetch.py", "bfetch1"), json!({}));
    l.item(&task, "dynamic_tool", "completed", &monitor(Some("bmonC"), "fetch progress"), json!({}));
    l.item(&task, "assistant_message", "completed", "Fetching.", json!({}));
    l.end_run(&task, "completed");
    l.pass().await;
    assert_eq!(l.deliveries().len(), 4, "{:?}", l.deliveries());
    l.new_run(&task);
    l.item(&task, "notification", "completed", "Monitor \"fetch progress\" and command \"python3 fetch.py\" finished", json!({}));
    l.item(&task, "assistant_message", "completed", "All weights fetched.", json!({}));
    l.end_run(&task, "completed");
    l.pass().await;
    let got = l.deliveries();
    assert_eq!(got.len(), 5, "{got:?}");
    assert!(got[4].1.contains("All weights fetched") && !got[4].1.contains("background work"), "{}", got[4].1);
    drop(fetch);
    // A Monitor item T3 left open without its task id (its turn over) never stops the reading of what follows.
    let task = l.delegate("Stuck monitor").await;
    l.item(&task, "dynamic_tool", "running", &monitor(None, "never started"), json!({}));
    l.item(&task, "assistant_message", "completed", "The turn was interrupted.", json!({}));
    l.end_run(&task, "completed");
    l.pass().await;
    l.new_run(&task);
    l.item(&task, "user_message", "completed", "Start the soak.", json!({}));
    let soak = Background::start(l.dir.path(), "bsoak9");
    l.item(&task, "command_execution", "completed", &soak.item("scripts/soak.sh", "bsoak9"), json!({}));
    l.item(&task, "assistant_message", "completed", "Soak started.", json!({}));
    l.end_run(&task, "completed");
    l.pass().await;
    l.pass().await;
    let got = l.deliveries();
    assert!(got.last().is_some_and(|(_, t)| t.contains("Soak started") && t.contains("background work running: scripts/soak.sh")), "what follows the stuck item is tracked: {got:?}");
}

/// Bug note 2026-10-24T23-34-21, replayed from task mcp:14c20ff4 ("Decider v4: max-RAM training launcher"): from 23:34
/// its turns failed at once on "Failed to authenticate: OAuth session expired and could not be refreshed" and the task
/// was reported failed while its background work ran; a turn started at 23:40 worked. Now such a failure is retried
/// once by itself after `AUTH_RETRY_MS` and wakes nothing; a retry that works brings the result, one that fails again
/// is reported with what it means. Bug note 2026-10-24T23-42-04: meanwhile Heimdall's own sends to it are refused,
/// instead of stacking behind the same failure.
#[tokio::test]
async fn a_task_turn_failing_on_its_sign_in_is_retried_once_and_not_re_steered() {
    const AUTH: &str = "Failed to authenticate: OAuth session expired and could not be refreshed";
    let l = Lab::new().await;
    l.w.set_self_close(false); // lane S: this rule is about a finished task that stays open (task_status, a later turn, a cleared island)
    l.w.set_auth_retry(60 * 60 * 1000);
    let task = l.delegate("Decider v4: max-RAM training launcher").await;
    l.item(&task, "assistant_message", "completed", "Training launched; I'll check the first epoch.", json!({}));
    l.end_run(&task, "completed");
    l.pass().await;
    assert_eq!(l.deliveries().len(), 1, "{:?}", l.deliveries());
    // T3's next turn fails on the sign-in at once, twice.
    for _ in 0..2 {
        l.new_run(&task);
        l.item(&task, "notification", "completed", "Background activity updated", json!({}));
        l.item(&task, "assistant_message", "completed", AUTH, json!({}));
        l.end_run(&task, "failed");
        l.pass().await;
    }
    assert_eq!(l.deliveries().len(), 1, "a sign-in failure wakes nothing: {:?}", l.deliveries());
    assert!(l.calls("t3_thread_send").is_empty());
    // Once it is due, the next pass sends the one retry; its turn works.
    l.w.set_auth_retry(0);
    l.pass().await;
    let sends = l.calls("t3_thread_send");
    assert_eq!(sends.len(), 1, "{sends:?}");
    assert_eq!(sends[0]["message"], t3::workers::AUTH_RETRY_MESSAGE);
    l.pass().await;
    assert_eq!(l.calls("t3_thread_send").len(), 1, "one retry only");
    // While the retry turn works, Heimdall's own message goes through (review 27 Oct: it was refused).
    let steer = l.call("t3_thread_send", json!({"threadId":task,"message":"Also log the loss."})).await.unwrap();
    assert!(!steer.contains("NOT_SENT"), "{steer}");
    l.item(&task, "assistant_message", "completed", "Epoch 1 done: loss 0.41.", json!({}));
    l.end_run(&task, "completed");
    l.pass().await;
    let got = l.deliveries();
    assert_eq!(got.len(), 2, "{got:?}");
    assert!(got[1].1.contains("completed") && got[1].1.contains("Last turn:\nEpoch 1 done") && !got[1].1.contains("sign-in"), "{}", got[1].1);
    // The failed turns' own words ride before it: nothing is lost.
    assert!(got[1].1.contains(&format!("Earlier turn:\n{AUTH}")), "{}", got[1].1);

    // The retry fails the same way: reported, and Heimdall's own message is refused for the cooldown.
    l.new_run(&task);
    l.item(&task, "assistant_message", "completed", AUTH, json!({}));
    l.end_run(&task, "failed");
    l.pass().await;
    l.pass().await;
    assert_eq!(l.deliveries().len(), 2, "{:?}", l.deliveries());
    l.item(&task, "assistant_message", "completed", AUTH, json!({}));
    l.end_run(&task, "failed");
    l.pass().await;
    let got = l.deliveries();
    assert_eq!(got.len(), 3, "{got:?}");
    assert!(got[2].1.contains("failed") && got[2].1.contains("after Heimdall's one automatic retry") && got[2].1.contains("send it nothing until then"), "{}", got[2].1);
    let refused = l.call("t3_thread_send", json!({"threadId":task,"message":"Resume the training."})).await.unwrap_err().to_string();
    assert!(refused.starts_with("NOT_SENT:") && refused.contains("Send after"), "{refused}");
    assert_eq!(l.calls("t3_thread_send").len(), 3, "nothing more went out");

    // A failure whose own first line is no sign-in error is reported at once, whatever else it quotes.
    let crash = l.delegate("Upload the dataset").await;
    l.item(&crash, "assistant_message", "completed", "The upload script crashed.\nIts log says: 401 Unauthorized from the HF API.", json!({}));
    l.end_run(&crash, "failed");
    l.pass().await;
    assert!(l.deliveries().last().is_some_and(|(id, text)| id.ends_with(":failed") && text.contains("The upload script crashed")), "{:?}", l.deliveries());
    let delivered = l.deliveries().len();

    // A task that works again by itself before the retry is due gets no retry (review 27 Oct).
    l.w.set_auth_retry(60 * 60 * 1000);
    let back = l.delegate("Night labelling").await;
    l.item(&back, "assistant_message", "completed", AUTH, json!({}));
    l.end_run(&back, "failed");
    l.pass().await;
    assert_eq!(l.deliveries().len(), delivered);
    let sends = l.calls("t3_thread_send").len();
    l.new_run(&back);
    l.item(&back, "notification", "completed", "Command \"label.py\" finished", json!({}));
    l.item(&back, "assistant_message", "completed", "Labelling finished: 318 of 318.", json!({}));
    l.end_run(&back, "completed");
    l.w.set_auth_retry(0);
    l.pass().await;
    l.pass().await;
    assert_eq!(l.calls("t3_thread_send").len(), sends, "no retry to a task that recovered");
    assert!(l.deliveries().last().is_some_and(|(_, text)| text.contains("Labelling finished")), "{:?}", l.deliveries());
    let delivered = l.deliveries().len();

    // night1114 relay (bug note 2026-11-13T10-54-06): while the retry is still due, a send (the user's restart, 10:48)
    // goes through and is that retry: no second, automatic one follows, and the answer says so.
    l.w.set_auth_retry(60 * 60 * 1000);
    let other = l.delegate("Drama 3 BF16 download").await;
    l.item(&other, "assistant_message", "completed", AUTH, json!({}));
    l.end_run(&other, "failed");
    l.pass().await;
    assert_eq!(l.deliveries().len(), delivered, "{:?}", l.deliveries());
    let sent = l.call("t3_thread_send", json!({"threadId":other,"message":"Carry on."})).await.unwrap();
    assert!(!sent.contains("NOT_SENT") && sent.contains("this message is its retry"), "{sent}");
    let sends = l.calls("t3_thread_send").len();
    l.w.set_auth_retry(0);
    l.pass().await;
    l.pass().await;
    assert_eq!(l.calls("t3_thread_send").len(), sends, "the send was the retry: no automatic one after it");
    // Its turn (the run the send started) fails on its sign-in again: that is the retry failing, reported once; a send
    // right after is refused with when.
    l.item(&other, "assistant_message", "completed", AUTH, json!({}));
    l.end_run(&other, "failed");
    l.pass().await;
    assert!(l.deliveries().last().is_some_and(|(_, text)| text.contains("after Heimdall's one automatic retry")), "{:?}", l.deliveries());
    let refused = l.call("t3_thread_send", json!({"threadId":other,"message":"Carry on."})).await.unwrap_err().to_string();
    assert!(refused.contains("NOT_SENT") && refused.contains("Send after"), "{refused}");
    // Bounded (review 27 Oct): past the cooldown, a send goes through.
    l.w.set_auth_cooldown(0);
    let sent = l.call("t3_thread_send", json!({"threadId":other,"message":"Carry on."})).await.unwrap();
    assert!(!sent.contains("NOT_SENT"), "{sent}");
}

/// Bug note 2026-10-24T23-42-04 (the user stopped a worker with about seven of Heimdall's messages piled on it): a send to
/// a thread where Heimdall's own earlier message still waits in T3's queue joins that message; one turn reads both.
#[tokio::test]
async fn heimdalls_sends_join_its_own_message_still_queued() {
    let l = Lab::new().await;
    let task = l.delegate("Decider v4: master dataset").await;
    let first = l.call("t3_thread_send", json!({"threadId":task,"message":"Switch the runners to effort high.","mode":"queue"})).await.unwrap();
    assert!(first.contains("Queued run"), "{first}");
    let joined = l.call("t3_thread_send", json!({"threadId":task,"message":"And stop reporting progress."})).await.unwrap();
    assert!(joined.starts_with("Added to your message still queued"), "{joined}");
    assert_eq!(l.calls("t3_thread_send").len(), 1, "no second turn");
    let queue = l.with(&task, |t| t["runs"].as_array().unwrap().iter().filter(|r| r["status"] == "queued").count());
    assert_eq!(queue, 1);
    let text = l.with(&task, |t| t["items"].as_array().unwrap().iter().rev().find(|i| i["type"] == "user_message").unwrap()["text"].clone());
    // One mark from Heimdall at the top (bug note 2026-11-08T06-13-58), not one per joined message.
    assert_eq!(text, format!("{} Switch the runners to effort high.\n\nAnd stop reporting progress.", t3::workers::steer_mark()));
    // The same operation replayed (a lost answer) joins nothing twice.
    let args = json!({"threadId":task,"message":"And keep the logs."});
    let once = l.w.execute("t3_thread_send", args.clone(), "op-join", &l.p()).await.unwrap();
    let again = l.w.execute("t3_thread_send", args, "op-join", &l.p()).await.unwrap();
    assert_eq!(once, again);
    let text = l.with(&task, |t| t["items"].as_array().unwrap().iter().rev().find(|i| i["type"] == "user_message").unwrap()["text"].clone());
    assert_eq!(text, format!("{} Switch the runners to effort high.\n\nAnd stop reporting progress.\n\nAnd keep the logs.", t3::workers::steer_mark()));
    // Heimdall stopped between T3's edit and recording it: the same message again finds itself joined already.
    let crashed = "Switch the runners to effort high.\n\nAnd stop reporting progress.\n\nAnd keep the logs.\n\nAnd tidy up.";
    l.with(&task, |t| {
        let i = t["items"].as_array_mut().unwrap().iter_mut().rev().find(|i| i["type"] == "user_message").unwrap();
        i["text"] = json!(crashed);
    });
    let again = l.w.execute("t3_thread_send", json!({"threadId":task,"message":"And tidy up."}), "op-crash", &l.p()).await.unwrap();
    assert!(again.starts_with("Added"), "{again}");
    let text = l.with(&task, |t| t["items"].as_array().unwrap().iter().rev().find(|i| i["type"] == "user_message").unwrap()["text"].clone());
    assert_eq!(text, crashed);
    // Once that turn has started, a send is a send again (here a steer into it).
    let queued = l.with(&task, |t| t["runs"].as_array().unwrap().last().unwrap()["runId"].as_str().unwrap().to_owned());
    l.with(&task, |t| {
        for r in t["runs"].as_array_mut().unwrap().iter_mut() {
            if r["runId"] == queued { r["status"] = json!("running"); r["startedAt"] = json!(iso(0)); } else if r["status"] == "running" { r["status"] = json!("completed"); }
        }
    });
    let steer = l.call("t3_thread_send", json!({"threadId":task,"message":"Also write the README."})).await.unwrap();
    assert!(!steer.starts_with("Added"), "{steer}");
    assert_eq!(l.calls("t3_thread_send").len(), 2);
}

/// After a rebuild and restart the agent still follows every task and thread it followed, and
/// After a rebuild and restart the agent still follows every task and thread it followed, and
/// gets each completion once: one that came while Heimdall was down (at startup), one after the restart, and one
/// whose delivery the quit cut short (offered again under the same id, which the agent's deliveries deduplicate).
/// The agent also gets one short restart notice listing what it follows; a later agent start in the same process
/// (a model switch) sends none.
#[tokio::test]
async fn followed_tasks_survive_a_restart_and_each_result_arrives_once() {
    let mut l = Lab::new().await;
    l.w.set_self_close(false); // lane S: this rule is about a finished task that stays open (task_status, a later turn, a cleared island)
    let islands = l.delegate("Floating Islands prototype (standalone)").await;
    let remote = l.delegate("Heimdall iPhone remote prototype").await;
    let tts = l.delegate("Research: what Reddit recommends for local TTS voices").await;
    let arsenal = l.delegate("Arsenal website (ex-Mobius) with cannon hero").await;
    l.owner_thread("mcp:mobius", "Mobius", 3);
    l.call("t3_thread_send", json!({"threadId":"mcp:mobius","message":"Finish the MCP work overnight, then the rename."})).await.unwrap();
    l.pass().await;
    // Before the quit: one result delivered, one taken by the agent with the acknowledgement lost at the quit.
    l.item(&arsenal, "assistant_message", "completed", "Arsenal site deployed to the preview.", json!({}));
    l.end_run(&arsenal, "completed");
    l.pass().await;
    l.item(&tts, "assistant_message", "completed", "Reddit favours Kokoro and Chatterbox.", json!({}));
    l.end_run(&tts, "completed");
    l.sink.lose_ack.store(true, std::sync::atomic::Ordering::SeqCst);
    let _ = l.w.scan_all(&l.p()).await;
    assert_eq!(l.deliveries().len(), 2, "{:?}", l.deliveries());
    // Heimdall is down: the islands task finishes.
    l.item(&islands, "assistant_message", "completed", "Islands demo shows realistic tasks now.", json!({}));
    l.end_run(&islands, "completed");
    l.restart_process();
    l.w.set_self_close(false);
    let status = l.call("task_status", json!({})).await.unwrap();
    for t in [&islands, &remote, &tts, &arsenal] {
        assert!(status.contains(t.as_str()), "{t} still a task after the restart: {status}");
    }
    let (stop, watcher) = l.watcher();
    l.until("the downtime result and the restart notice", |d| {
        d.iter().any(|(id, _)| id.starts_with(&format!("terminal:{islands}:"))) && d.iter().any(|(id, _)| id.starts_with("t3-restarted:"))
    })
    .await;
    // A completion after the restart, through the event-backed wait.
    l.item(&remote, "assistant_message", "completed", "The iPhone remote builds and pairs.", json!({}));
    l.end_run(&remote, "completed");
    l.until("the result after the restart", |d| d.iter().any(|(id, _)| id.starts_with(&format!("terminal:{remote}:")))).await;
    l.item("mcp:mobius", "assistant_message", "completed", "MCP work finished; rename planned.", json!({}));
    l.end_run("mcp:mobius", "completed");
    l.until("the watched thread's reply", |d| d.iter().any(|(id, _)| id.starts_with("terminal:mcp:mobius:"))).await;
    stop.send(true).unwrap();
    watcher.await.unwrap();
    let got = l.deliveries();
    let ids: Vec<&str> = got.iter().map(|(id, _)| id.as_str()).collect();
    for t in [islands.as_str(), remote.as_str(), arsenal.as_str(), "mcp:mobius"] {
        assert_eq!(ids.iter().filter(|id| id.starts_with(&format!("terminal:{t}:"))).count(), 1, "{t}: {ids:?}");
    }
    // The one in flight at the quit: offered again with its own id (so the agent takes it once), never a new id.
    let tts_ids: Vec<&&str> = ids.iter().filter(|id| id.starts_with(&format!("terminal:{tts}:"))).collect();
    assert!(tts_ids.len() <= 2 && tts_ids.iter().all(|id| **id == format!("terminal:{tts}:run:{tts}:1:completed")), "{ids:?}");
    let restarts: Vec<&(String, String)> = got.iter().filter(|(id, _)| id.starts_with("t3-restarted:")).collect();
    assert_eq!(restarts.len(), 1, "{ids:?}");
    let lines: Vec<&str> = restarts[0].1.lines().collect();
    assert_eq!(lines[0], "Heimdall restarted. T3 work you still follow (its results still arrive as notices):");
    assert_eq!(lines.len(), 6, "{}", restarts[0].1);
    assert!(lines.contains(&format!("- task {remote} · 'Heimdall iPhone remote prototype' · working").as_str()), "{}", restarts[0].1);
    assert!(lines.contains(&format!("- task {islands} · 'Floating Islands prototype (standalone)' · done").as_str()), "{}", restarts[0].1);
    assert!(lines.contains(&"- thread mcp:mobius · 'Mobius' · working"), "{}", restarts[0].1);
    // A new agent start in the same app process (a model switch) is not a restart.
    l.restart();
    let (stop, watcher) = l.watcher();
    tokio::time::sleep(Duration::from_millis(500)).await;
    stop.send(true).unwrap();
    watcher.await.unwrap();
    assert_eq!(l.deliveries().iter().filter(|(id, _)| id.starts_with("t3-restarted:")).count(), 1);
    assert_eq!(l.deliveries().len(), got.len(), "nothing again: {:?}", l.deliveries());
}

/// Night 3 review: the restart notice's first drain times out (Pi still starting). It stays in the outbox under its id
/// and the next drain delivers it; a later pass never adds a second restart notice with a fresh id and text. If an
/// Agent Off/On drops it from the outbox before it reached the agent, it is re-armed under the same id.
#[tokio::test]
async fn a_restart_notice_whose_first_drain_fails_goes_out_once() {
    use std::sync::atomic::Ordering::SeqCst;
    for off_on in [false, true] {
        let mut l = Lab::new().await;
        let _task = l.delegate("Followed across a restart").await;
        l.pass().await;
        l.restart_process();
        l.sink.fail_restart.store(true, SeqCst);
        let (stop, watcher) = l.watcher();
        for _ in 0..200 {
            if !l.sink.fail_restart.load(SeqCst) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(!l.sink.fail_restart.load(SeqCst), "the watcher never offered the restart notice");
        stop.send(true).unwrap();
        watcher.await.unwrap();
        assert!(!l.deliveries().iter().any(|(id, _)| id.starts_with("t3-restarted:")), "{:?}", l.deliveries());
        let waiting: Vec<String> = l.w.actions.outbox_undelivered().unwrap().into_iter().filter(|id| id.starts_with("t3-restarted:")).collect();
        assert_eq!(waiting.len(), 1, "{waiting:?}");
        if off_on {
            l.gate.intent(agent::delivery::StopReason::Agent, false);
            l.gate.intent(agent::delivery::StopReason::Agent, true);
        }
        // The next pass (here a new watcher in the same process: an agent start after a model switch) delivers it.
        let (stop, watcher) = l.watcher();
        l.until("the waiting restart notice", |d| d.iter().any(|(id, _)| id.starts_with("t3-restarted:"))).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        stop.send(true).unwrap();
        watcher.await.unwrap();
        let ids: Vec<String> = l.deliveries().into_iter().map(|(id, _)| id).filter(|id| id.starts_with("t3-restarted:")).collect();
        assert_eq!(ids, waiting, "off_on {off_on}");
    }
}

/// Night 3 review: one outbox row the agent cannot take failed the watcher's whole pass at the audit's drain, before
/// the per-run waits were armed, so a followed run's end was seen only at the next 30-second tick. The pass goes on.
#[tokio::test]
async fn an_undeliverable_outbox_row_does_not_keep_the_pass_from_arming_its_waits() {
    let l = Lab::new().await;
    let task = l.delegate("Still running").await;
    *l.sink.refuse.lock().unwrap() = Some("stuck".into());
    l.w.actions.enqueue("stuck", "A notice the agent cannot take now.", &l.p()).unwrap();
    let waited = || l.calls("t3_thread_wait").iter().any(|a| a["threadId"] == task.as_str());
    let (stop, watcher) = l.watcher();
    for _ in 0..100 {
        if waited() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    stop.send(true).unwrap();
    watcher.await.unwrap();
    assert!(waited(), "the pass ended at the stuck row before it armed the run's wait");
    assert!(l.w.actions.outbox_undelivered().unwrap().contains(&"stuck".to_owned()), "the row waits for a later drain");
}

/// The heartbeat (bug note 01-28-46): a finished run whose notice never reached the agent gets one notice, never a
/// duplicate. A notice dropped from the outbox (a newer delivery epoch: Agent off and on before it went out) goes out
/// once more as `:again`; a finished run nothing reported (the wait saw it end, the scan after it failed) is scanned
/// and reported; delivered ones are left alone, tick after tick.
#[tokio::test]
async fn the_heartbeat_delivers_a_missed_result_once() {
    let l = Lab::new().await;
    let a = l.delegate("Dropped from the outbox").await;
    l.item(&a, "assistant_message", "completed", "Result A.", json!({}));
    l.end_run(&a, "completed");
    // Written while the agent could not take it, then dropped for a newer epoch.
    l.w.actions.detach();
    assert!(l.w.scan_all(&l.p()).await.is_err());
    l.w.actions.attach(l.sink.clone());
    l.gate.intent(agent::delivery::StopReason::Agent, false);
    l.gate.intent(agent::delivery::StopReason::Agent, true);
    l.w.actions.drain(&l.p()).unwrap();
    assert!(l.deliveries().is_empty(), "dropped, never delivered");
    assert_eq!(l.w.actions.outbox_state(&format!("terminal:{a}:run:{a}:1:completed")).unwrap().map(|(d, _)| d), Some(-1));
    // B: its wait saw the run end (the status is recorded), the scan after it never ran.
    let b = l.delegate("Never reported").await;
    l.item(&b, "assistant_message", "completed", "Result B.", json!({}));
    l.end_run(&b, "completed");
    rusqlite::Connection::open(l.dir.path().join("workers.db")).unwrap().execute("UPDATE workers SET status='completed' WHERE thread=?", [&b]).unwrap();
    let c = l.delegate("Delivered normally").await;
    l.item(&c, "assistant_message", "completed", "Result C.", json!({}));
    l.end_run(&c, "completed");
    rusqlite::Connection::open(l.dir.path().join("workers.db")).unwrap().execute("UPDATE workers SET status='completed' WHERE thread=?", [&c]).unwrap();
    assert_eq!(l.w.heartbeat(&l.p()).await.unwrap(), 3);
    let got = l.deliveries();
    assert_eq!(got.len(), 3, "{got:?}");
    assert!(got.iter().any(|(id, t)| *id == format!("terminal:{a}:run:{a}:1:completed:again") && t.contains("Final reply:\n\nResult A.")), "{got:?}");
    assert!(got.iter().any(|(id, t)| *id == format!("terminal:{b}:run:{b}:1:completed") && t.contains("Result B.")), "{got:?}");
    // Ticks and passes after that find nothing to do.
    for _ in 0..3 {
        assert_eq!(l.w.heartbeat(&l.p()).await.unwrap(), 0);
        l.pass().await;
    }
    assert_eq!(l.deliveries().len(), 3, "{:?}", l.deliveries());
}

/// Night pass 3 (21 Oct 04:00:52): the night-pass task's notice said "completed with no reply text" although its last
/// turn had replied. Its brief was longer than one read's 4000 characters; T3 answers a read of the rest of one item
/// with `hasMore: false`, and the reply collection took that for the end of the thread after the first item.
#[tokio::test]
async fn a_brief_longer_than_one_read_does_not_hide_the_final_reply() {
    let l = Lab::new().await;
    let task = l.delegate("Night pass").await;
    l.with(&task, |t| t["items"][0]["text"] = json!(format!("Night pass brief. {}", "Rule. ".repeat(1500))));
    l.item(&task, "assistant_message", "completed", "Working on it.", json!({}));
    l.item(&task, "assistant_message", "completed", "Rebuilt, installed and verified.", json!({}));
    l.end_run(&task, "completed");
    l.pass().await;
    let got = l.deliveries();
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(got[0].1.contains("completed. Final reply:\n\nRebuilt, installed and verified."), "{}", got[0].1);
}

/// Night pass 3 (21 Oct 04:00:52): four background commands recorded before Heimdall restarted had no output path
/// (`children.output`), so nothing could check them; at the hold limit the notice said the task "still has background
/// work running" although all four had finished 30 minutes earlier. A command nothing can check is named as such,
/// never as running; one whose output file is still being written stays "still running".
#[tokio::test]
async fn background_work_nothing_can_check_is_not_reported_as_running() {
    let l = Lab::new().await;
    let task = l.delegate("Unchecked").await;
    l.item(&task, "command_execution", "completed", "$ scripts/check.sh --tests\nCommand running in background with ID: bx1. Output …", json!({}));
    l.item(&task, "assistant_message", "completed", "Checks are running; I'll report when they finish.", json!({}));
    l.end_run(&task, "completed");
    l.pass().await;
    assert!(l.deliveries().is_empty(), "held while the background command is fresh");
    l.w.set_hold_timing(0, 0);
    l.pass().await;
    l.pass().await;
    let got = l.deliveries();
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(!got[0].1.contains("still has background work running"), "{}", got[0].1);
    assert!(got[0].1.contains("Heimdall cannot check whether its background work still runs: scripts/check.sh --tests."), "{}", got[0].1);
    assert!(got[0].1.contains("Final reply:\n\nChecks are running"), "{}", got[0].1);
}

/// Night pass 3 (21 Oct 04:00:52): the night-pass result's delivery was not acknowledged in time (the agent runtime
/// was starved), so its notice went out from the outbox later, but the turns it carried stayed in `withheld` for good
/// (runs 1-24 of that task): the bookkeeping after the notice ran only when the delivery was acknowledged at once.
#[tokio::test]
async fn a_result_whose_delivery_failed_once_still_retires_the_turns_it_carried() {
    let l = Lab::new().await;
    let task = l.delegate("Engine research").await;
    let check = l.item(&task, "subagent", "running", "", json!({"title":"Check oMLX"}));
    l.item(&task, "assistant_message", "completed", "Waiting on the oMLX check.", json!({}));
    l.end_run(&task, "completed");
    l.pass().await;
    l.set_item(&task, &check, "completed");
    l.item(&task, "notification", "completed", "Subagent \"Check oMLX\" finished", json!({}));
    l.new_run(&task);
    l.item(&task, "assistant_message", "completed", "oMLX wins.", json!({}));
    l.end_run(&task, "completed");
    // The agent cannot take it now: written to the outbox, not delivered.
    l.w.actions.detach();
    assert!(l.w.scan_all(&l.p()).await.is_err());
    l.w.actions.attach(l.sink.clone());
    l.w.actions.drain(&l.p()).unwrap();
    l.pass().await;
    l.pass().await;
    let got = l.deliveries();
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(got[0].1.contains("Final reply:\n\nEarlier turn:\nWaiting on the oMLX check.\n\nLast turn:\noMLX wins."), "{}", got[0].1);
    let db = rusqlite::Connection::open(l.dir.path().join("workers.db")).unwrap();
    let left: i64 = db.query_row("SELECT count(*) FROM withheld WHERE thread=?", [&task], |r| r.get(0)).unwrap();
    assert_eq!(left, 0, "the carried turns are retired with the notice");
    let pending: i64 = db.query_row("SELECT count(*) FROM pending_finals WHERE thread=?", [&task], |r| r.get(0)).unwrap();
    assert_eq!(pending, 0);
}

/// Review of the withheld fix (21 Oct): a result whose delivery failed, then Agent Off and On before the outbox drained
/// (its row is dropped for the new epoch): the heartbeat's `:again` notice still carries the earlier turn's reply,
/// and the carried turn is retired once that notice is delivered.
#[tokio::test]
async fn a_failed_result_resent_after_off_and_on_still_carries_the_earlier_turns() {
    let l = Lab::new().await;
    let task = l.delegate("Engine research").await;
    let check = l.item(&task, "subagent", "running", "", json!({"title":"Check oMLX"}));
    l.item(&task, "assistant_message", "completed", "Waiting on the oMLX check.", json!({}));
    l.end_run(&task, "completed");
    l.pass().await;
    l.set_item(&task, &check, "completed");
    l.item(&task, "notification", "completed", "Subagent \"Check oMLX\" finished", json!({}));
    l.new_run(&task);
    l.item(&task, "assistant_message", "completed", "oMLX wins.", json!({}));
    l.end_run(&task, "completed");
    l.w.actions.detach();
    assert!(l.w.scan_all(&l.p()).await.is_err());
    l.w.actions.attach(l.sink.clone());
    l.gate.intent(agent::delivery::StopReason::Agent, false);
    l.gate.intent(agent::delivery::StopReason::Agent, true);
    l.w.actions.drain(&l.p()).unwrap();
    assert!(l.deliveries().is_empty(), "dropped for the new epoch");
    assert_eq!(l.w.heartbeat(&l.p()).await.unwrap(), 1);
    let got = l.deliveries();
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(got[0].0.ends_with(":again"), "{got:?}");
    assert!(got[0].1.contains("Final reply:\n\nEarlier turn:\nWaiting on the oMLX check.\n\nLast turn:\noMLX wins."), "{}", got[0].1);
    l.pass().await;
    let db = rusqlite::Connection::open(l.dir.path().join("workers.db")).unwrap();
    let left: i64 = db.query_row("SELECT count(*) FROM withheld WHERE thread=?", [&task], |r| r.get(0)).unwrap();
    assert_eq!(left, 0, "retired once the resend is delivered");
}

/// Lab check against a copy of the live store (never the live file): which tasks and threads a restart reloads and
/// the restart notice it would send. `HEIMDALL_T3_DB_COPY=/tmp/copy/t3-workers.sqlite cargo test -p t3 --test tasks
/// live_store_copy -- --ignored --nocapture`. Opening migrates the copy.
#[tokio::test]
#[ignore]
async fn live_store_copy_reloads_its_followed_work() {
    let Ok(path) = std::env::var("HEIMDALL_T3_DB_COPY") else { return };
    let dir = tempfile::tempdir().unwrap();
    let client = Arc::new(Client::new(dir.path().join("runtime.json"), Arc::new(Store::default())).unwrap());
    let actions = Arc::new(Actions::open(&dir.path().join("actions.db")).unwrap());
    let w = Workers::open(std::path::Path::new(&path), client, actions).unwrap();
    println!("{}", w.restart_notice(true).unwrap().unwrap_or_else(|| "(nothing followed)".into()));
}

/// Bug note 20-22-34, replayed from the Mobius thread of 21 Oct (T3 runs 825–829). Heimdall's message (19:34) became
/// run 825, which resumed a sub-agent an earlier run had started (above the watch cursor) and ended 30 s later. Runs
/// 826–828 were started by T3 itself (`Subagent … finished`, `Command … finished`); 828 had the 3 images (19:53).
/// Before: the watch ended with 825's notice and 828 woke nothing. Now each of T3's own follow-up runs is reported
/// (a held one rides in the next), the user's own next run ends the following, and nothing repeats after a restart.
#[tokio::test]
async fn a_watched_threads_own_follow_up_runs_still_wake_it() {
    let mut l = Lab::new().await;
    l.owner_thread("mobius", "Mobius", 3);
    let worker = l.item("mobius", "subagent", "completed", "", json!({"title":"Fix Qwen int8 orphan-affine load"}));
    l.call("t3_thread_send", json!({"threadId":"mobius","message":"User (19:34): ship 16-bit only; stop the 8-bit fix."})).await.unwrap();
    l.reopen("mobius", &worker);
    l.item("mobius", "dynamic_tool", "completed", "{\"toolName\": \"SendMessage\", \"input\": {\"to\": \"abd6a31b\"}}", json!({}));
    l.item("mobius", "assistant_message", "completed", "I've told the worker to stop the 8-bit fix. Next: 3.7.1.", json!({}));
    l.end_run("mobius", "completed");
    l.pass().await;
    assert_eq!(l.deliveries().len(), 1, "{:?}", l.deliveries());
    assert!(l.deliveries()[0].1.contains("Final reply:\n\nI've told the worker"), "{}", l.deliveries()[0].1);
    // 826: T3 hands over the sub-agent; the turn starts the build and ends.
    l.set_item("mobius", &worker, "completed");
    l.new_run("mobius");
    l.item("mobius", "notification", "completed", "Subagent \"Fix Qwen int8 orphan-affine load\" finished", json!({}));
    l.item("mobius", "assistant_message", "completed", "The Opus worker's 3.7.1 strict build is running; I'll check its result when it finishes.", json!({}));
    l.end_run("mobius", "completed");
    l.pass().await;
    assert_eq!(l.deliveries().len(), 2, "{:?}", l.deliveries());
    assert!(l.deliveries()[1].1.contains("Final reply:\n\nThe Opus worker's 3.7.1 strict build is running"), "{}", l.deliveries()[1].1);
    // 827: the build's notification; 3.7.1 installed, the image test runs in the background (held meanwhile).
    l.new_run("mobius");
    l.item("mobius", "notification", "completed", "Command \"Run strict 3.7.1 release build\" and subagent \"Fix Qwen int8 orphan-affine load\" finished", json!({}));
    l.item("mobius", "command_execution", "completed", "$ peer run haiku --wait - < BRIEF-QWEN-MCP-TEST.md\nCommand running in background with ID: bq1. Output is being written to: /nonexistent/bq1.output.", json!({}));
    l.item("mobius", "assistant_message", "completed", "3.7.1 is installed. The fresh agent is generating the 3 images over MCP now.", json!({}));
    l.end_run("mobius", "completed");
    l.pass().await;
    assert_eq!(l.deliveries().len(), 2, "held for its background test: {:?}", l.deliveries());
    // 828 (19:53): the test's notification; the images exist.
    l.new_run("mobius");
    l.item("mobius", "notification", "completed", "Command \"peer run haiku --wait - < BRIEF-QWEN-MCP-TEST.md\" finished", json!({}));
    l.item("mobius", "assistant_message", "completed", "The 3 images exist: a fresh agent made them over Arsenal 3.7.1's MCP.", json!({}));
    l.end_run("mobius", "completed");
    l.pass().await;
    let got = l.deliveries();
    assert_eq!(got.len(), 3, "{got:?}");
    assert!(got[2].0.ends_with(":completed") && got[2].0.contains("run:mobius:7"), "{}", got[2].0);
    assert!(got[2].1.contains("Earlier turn:\n3.7.1 is installed.") && got[2].1.contains("Last turn:\nThe 3 images exist"), "{}", got[2].1);
    // A restart reports nothing again.
    l.restart_process();
    l.pass().await;
    l.pass().await;
    assert_eq!(l.deliveries().iter().filter(|(id, _)| id.starts_with("terminal:")).count(), 3, "{:?}", l.deliveries());
    // The user's own next run there: his business, and the thread is no longer followed.
    l.new_run("mobius");
    l.item("mobius", "user_message", "completed", "User: what about the hero image?", json!({"createdBy":"user","creationSource":"web"}));
    l.item("mobius", "assistant_message", "completed", "Answer to User himself.", json!({}));
    l.end_run("mobius", "completed");
    l.pass().await;
    l.pass().await;
    assert_eq!(l.deliveries().iter().filter(|(id, _)| id.starts_with("terminal:")).count(), 3, "{:?}", l.deliveries());
    assert!(l.w.restart_notice(true).unwrap().is_none(), "nothing followed after the user's own turn");
}

/// The follow-up following has an end: quiet for `TRAIL_MS` after its last notice, the thread is the user's again; and
/// a continuation that ended just before the user's own turn started is still reported, once.
#[tokio::test]
async fn following_a_watched_thread_ends_after_quiet_or_at_owners_turn() {
    let l = Lab::new().await;
    l.owner_thread("quiet", "Quiet thread", 2);
    l.call("t3_thread_send", json!({"threadId":"quiet","message":"Check the cache."})).await.unwrap();
    l.item("quiet", "assistant_message", "completed", "Cache checked.", json!({}));
    l.end_run("quiet", "completed");
    l.w.set_trail(0);
    l.pass().await;
    assert_eq!(l.deliveries().len(), 1);
    tokio::time::sleep(Duration::from_millis(5)).await;
    l.pass().await;
    assert!(l.w.restart_notice(true).unwrap().is_none(), "quiet past the trail: no longer followed");
    l.new_run("quiet");
    l.item("quiet", "notification", "completed", "Command \"npm test\" (exit 0) finished", json!({}));
    l.item("quiet", "assistant_message", "completed", "Tests pass.", json!({}));
    l.end_run("quiet", "completed");
    l.pass().await;
    assert_eq!(l.deliveries().len(), 1, "{:?}", l.deliveries());

    l.w.set_trail(60 * 60 * 1000);
    l.owner_thread("busy", "Busy thread", 2);
    l.call("t3_thread_send", json!({"threadId":"busy","message":"Run the benchmark."})).await.unwrap();
    l.item("busy", "assistant_message", "completed", "Benchmark started in the background.", json!({}));
    l.end_run("busy", "completed");
    l.pass().await;
    assert_eq!(l.deliveries().len(), 2);
    // T3's follow-up run ends, then the user's own turn starts before the next pass.
    l.new_run("busy");
    l.item("busy", "notification", "completed", "Command \"bench\" finished", json!({}));
    l.item("busy", "assistant_message", "completed", "Benchmark: 94 tok/s.", json!({}));
    l.end_run("busy", "completed");
    l.new_run("busy");
    l.item("busy", "user_message", "completed", "User: thanks, now the docs.", json!({"createdBy":"user"}));
    l.pass().await;
    l.item("busy", "assistant_message", "completed", "Docs updated.", json!({}));
    l.end_run("busy", "completed");
    l.pass().await;
    let got = l.deliveries();
    assert_eq!(got.len(), 3, "{got:?}");
    assert!(got[2].1.contains("Final reply:\n\nBenchmark: 94 tok/s.") && !got[2].1.contains("Docs updated"), "{}", got[2].1);
}

/// Bug note 16-38-55, replayed from the iPhone remote task of 21 Oct (runs 15–17): Heimdall's message (16:07) became
/// run 15, which ended "switch Developer Mode on; I'm building" with a waiter in the background; 16 and 17 were
/// started by those waiters' notifications and ended asking the user again, each with a new waiter. The 30-minute
/// background hold kept every one of those replies until run 19 (16:45). Now a reply held only for background work
/// goes out once `BACKGROUND_HOLD_MS` (a few minutes) has passed, naming the work, and each turn's reply arrives once.
#[tokio::test]
async fn a_reply_held_only_for_a_background_waiter_arrives_within_minutes() {
    assert!(t3::workers::BACKGROUND_HOLD_MS <= 5 * 60 * 1000, "a background waiter must not hold a reply for long");
    let mut l = Lab::new().await;
    let task = l.delegate("Heimdall iPhone remote prototype").await;
    l.item(&task, "assistant_message", "completed", "Unlock the iPhone and keep it on the cable.", json!({}));
    l.end_run(&task, "completed");
    l.pass().await;
    assert_eq!(l.deliveries().len(), 1);
    l.w.set_hold_timing(0, 400);
    l.call("t3_thread_send", json!({"threadId":task,"message":"The user is ready now (16:07): the iPhone is unlocked."})).await.unwrap();
    l.item(&task, "command_execution", "completed", "$ /tmp/hr_wait.sh\nCommand running in background with ID: bi03w9ndy. Output is being written to: /nonexistent/bi03w9ndy.output.", json!({}));
    l.item(&task, "assistant_message", "completed", "The phone reports Developer Mode as off. Switch it on, let it restart and tap Turn On.", json!({}));
    l.end_run(&task, "completed");
    l.pass().await;
    assert_eq!(l.deliveries().len(), 1, "held while the waiter is fresh: {:?}", l.deliveries());
    tokio::time::sleep(Duration::from_millis(500)).await;
    l.pass().await; // past the hold: the children-closed grace starts (0 in tests)
    l.pass().await;
    let got = l.deliveries();
    assert_eq!(got.len(), 2, "{got:?}");
    assert!(got[1].1.contains("It still has background work running: /tmp/hr_wait.sh") && got[1].1.contains("Switch it on"), "{}", got[1].1);
    // Run 17: the waiter's notification starts a turn that asks again, with a new waiter.
    l.new_run(&task);
    l.item(&task, "notification", "completed", "Command \"Wait for install or the next iOS step\" finished", json!({}));
    l.item(&task, "command_execution", "completed", "$ for i in {1..250}; do grep -q x go.log && break; sleep 15; done\nCommand running in background with ID: bls0f4oba. Output is being written to: /nonexistent/bls0f4oba.output.", json!({}));
    l.item(&task, "assistant_message", "completed", "As of 16:32 the iPhone still says Developer Mode is off. Plug the cable back in.", json!({}));
    l.end_run(&task, "completed");
    l.pass().await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    l.pass().await; // past the hold: the children-closed grace starts (0 in tests)
    l.pass().await;
    let got = l.deliveries();
    assert_eq!(got.len(), 3, "{got:?}");
    assert!(got[2].1.contains("Final reply:\n\nAs of 16:32") && !got[2].1.contains("Earlier turn"), "{}", got[2].1);
    l.restart_process();
    l.w.set_hold_timing(0, 400);
    l.pass().await;
    l.pass().await;
    assert_eq!(l.deliveries().iter().filter(|(id, _)| id.starts_with("terminal:")).count(), 3, "{:?}", l.deliveries());
}

/// Bug note 02-50-34 (tools.log 22 Oct 02:49:45: `path` Projects/Heimdall with isolation worktree; T3 failed in
/// "GitWorkflowService.hasCommit (<the Vault>)", the Vault being no repository): `path` in a project's
/// own repository gets T3's worktree isolation; another repository inside a project that is no repository
/// (Projects/Heimdall inside the Vault) never reaches T3 as a worktree, so nothing runs git at the project root:
/// Heimdall makes the worktree from the repository holding `path`, beside it, and the thread is told to work there.
/// A shared task there runs at the root told which folder to work in. An existing worktree of the project's
/// repository binds as one.
#[tokio::test]
async fn delegate_task_binds_only_a_projects_own_repository() {
    let l = Lab::new().await;
    let app = l.dir.path().join("Projects/App");
    std::fs::create_dir_all(app.join("src")).unwrap();
    git(&app, &["init", "-q", "-b", "main"]);
    git(&app, &["commit", "-q", "--allow-empty", "-m", "first"]);
    let app_real = std::fs::canonicalize(&app).unwrap();
    let root_real = std::fs::canonicalize(l.dir.path()).unwrap();
    let iso = json!({"task":"Fix it.","title":"Fix the islands","target":{"model":"claude-opus-5-5"},"path":app.join("src"),"isolation":"worktree"});
    let out = l.w.execute("delegate_task", iso.clone(), "toolu_iso", &l.p()).await.unwrap();
    let launch = &l.calls("t3_thread_launch")[0];
    assert_eq!(launch["workspaceStrategy"], json!({"type":"root"}), "never a T3 worktree of a project that is no repository");
    let brief = launch["message"].as_str().unwrap();
    let branch = brief.split(" on branch ").nth(1).unwrap().split(',').next().unwrap().to_owned();
    assert!(branch.starts_with("heimdall/fix-the-islands-") && branch.len() == "heimdall/fix-the-islands-".len() + 8, "{branch}");
    let folder = root_real.join("Projects").join(format!("App-{}", branch.replace('/', "-")));
    assert!(
        brief.starts_with(&format!("Working folder: {} (a new git worktree of {} on branch {branch}, made for this task; this thread starts in {}; ", folder.join("src").display(), app_real.display(), root_real.display())),
        "{brief}"
    );
    assert!(brief.ends_with("\n\nFix it."), "{brief}");
    assert!(out.contains(&format!(" in its own git worktree {} (branch {branch}).", folder.join("src").display())), "{out}");
    let head = std::process::Command::new("git").arg("-C").arg(&folder).args(["rev-parse", "--abbrev-ref", "HEAD"]).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&head.stdout).trim(), branch, "the worktree is on its branch");
    assert!(!root_real.join(".git").exists(), "nothing ran git init at the project root");
    // A replay answers from the record: no second launch, no second worktree.
    assert_eq!(l.w.execute("delegate_task", iso, "toolu_iso", &l.p()).await.unwrap(), out);
    assert_eq!(l.calls("t3_thread_launch").len(), 1);
    // A named branch: the worktree is named after it; another task given the same branch does not share it (review of
    // the 22 Oct lane: isolation would be lost).
    let named = json!({"task":"More.","title":"More","target":{"model":"claude-opus-5-5"},"path":app,"isolation":"worktree","branch":"islands/fix"});
    l.call("delegate_task", named.clone()).await.unwrap();
    let launches = l.calls("t3_thread_launch");
    assert!(launches[1]["message"].as_str().unwrap().starts_with(&format!("Working folder: {} (", root_real.join("Projects/App-islands-fix").display())), "{}", launches[1]["message"]);
    let e = l.call("delegate_task", named).await.unwrap_err().to_string();
    assert!(e.starts_with("WORKTREE_BRANCH_IN_USE: branch islands/fix is already checked out at "), "{e}");
    assert_eq!(l.calls("t3_thread_launch").len(), 2);
    // The branch checked out elsewhere is not taken over.
    git(&app, &["branch", "taken"]);
    git(&app, &["worktree", "add", "-q", root_real.join("Elsewhere").to_str().unwrap(), "taken"]);
    let e = l.call("delegate_task", json!({"task":"x","title":"x","target":{"model":"claude-opus-5-5"},"path":app,"isolation":"worktree","branch":"taken"})).await.unwrap_err().to_string();
    assert!(e.starts_with("WORKTREE_BRANCH_IN_USE: branch taken is already checked out at "), "{e}");
    assert_eq!(l.calls("t3_thread_launch").len(), 2, "never launched");
    l.call("delegate_task", json!({"task":"Fix it.","title":"Fix","target":{"model":"claude-opus-5-5"},"path":app})).await.unwrap();
    let launch = &l.calls("t3_thread_launch")[2];
    assert_eq!(launch["workspaceStrategy"], json!({"type":"root"}), "never another repository as worktreePath");
    assert!(launch["message"].as_str().unwrap().starts_with(&format!("Working folder: {} (this thread starts in ", app_real.display())), "{}", launch["message"]);
    assert!(launch["message"].as_str().unwrap().ends_with("\n\nFix it."));
    // A plain folder (no repository) cannot be isolated either.
    let plain = l.dir.path().join("Notes");
    std::fs::create_dir_all(&plain).unwrap();
    let e = l.call("delegate_task", json!({"task":"x","title":"x","target":{"model":"claude-opus-5-5"},"path":plain,"isolation":"worktree"})).await.unwrap_err().to_string();
    assert!(e.starts_with("ISOLATION_UNAVAILABLE: ") && e.contains("is not inside a git repository"), "{e}");

    // The project root is a repository: isolation from anywhere in it; an existing worktree of it binds as one; a
    // folder of its own checkout is the root.
    let root = l.dir.path();
    git(root, &["init", "-q"]);
    git(root, &["commit", "-q", "--allow-empty", "-m", "first"]);
    let wt = root.join("wt-feature");
    git(root, &["worktree", "add", "-q", "-b", "feature", wt.to_str().unwrap()]);
    let sub = root.join("docs");
    std::fs::create_dir_all(&sub).unwrap();
    l.call("delegate_task", json!({"task":"y","title":"Iso","target":{"model":"claude-opus-5-5"},"path":sub,"isolation":"worktree"})).await.unwrap();
    l.call("delegate_task", json!({"task":"y","title":"Wt","target":{"model":"claude-opus-5-5"},"path":wt})).await.unwrap();
    l.call("delegate_task", json!({"task":"y","title":"Sub","target":{"model":"claude-opus-5-5"},"path":sub})).await.unwrap();
    let launches = l.calls("t3_thread_launch");
    assert_eq!(launches[3]["workspaceStrategy"], json!({"type":"worktree","baseRef":"HEAD","startFromOrigin":false}));
    assert_eq!(launches[4]["workspaceStrategy"], json!({"type":"existing_worktree","worktreePath":std::fs::canonicalize(&wt).unwrap()}));
    assert_eq!(launches[5]["workspaceStrategy"], json!({"type":"root"}));
    assert_eq!(launches[5]["message"], "y", "a folder T3 binds needs no note");
    // With the default task prompt (docs/skills.md) the folder line belongs to the task, below the prompt's rule.
    l.w.set_brief_preamble("Rules.");
    l.call("delegate_task", json!({"task":"Fix it.","title":"Fix","target":{"model":"claude-opus-5-5"},"path":app})).await.unwrap();
    let ruled = l.calls("t3_thread_launch").last().unwrap()["message"].as_str().unwrap().to_owned();
    assert!(ruled.starts_with(&format!("Rules.\n\n{}\n\n---\n\nWorking folder: {} (", t3::workers::AVOID_APPROVALS, app_real.display())) && ruled.ends_with("\n\nFix it."), "{ruled}");
}

/// Bug note 22 Oct 15:35, replayed from the Voxtral research task (mcp:3bf45699, run 3): the turn launched an Explore
/// sub-agent and worked beside it; the sub-agent closed at 15:30:01, the parent ran more commands and model calls
/// after that (it read the sub-agent's result inside the turn), and the turn ended at 15:34:03. T3 started no turn
/// to hand the sub-agent over, yet the result was held for the 45 s grace and went out on the next 30 s pass after
/// it, at 15:35:14, after the user had already asked. A sub-agent whose result the turn took in delays nothing.
#[tokio::test]
async fn a_background_sub_agent_the_turn_read_itself_delays_nothing() {
    let l = Lab::new().await;
    l.w.set_hold_timing(45_000, 60 * 60 * 1000);
    let task = l.delegate("Research: Mistral's Voxtral voice agent pipeline").await;
    l.item(&task, "reasoning", "completed", "I'll do the direct search myself while an Explore agent looks in parallel.", json!({}));
    let explore = l.item(&task, "subagent", "running", "", json!({"title":"Find shelved streaming pipeline"}));
    l.item(&task, "command_execution", "completed", "$ sed -n 1,200p lab/latency/MAP.md\n# Where a voice request's time goes", json!({}));
    l.item(&task, "reasoning", "completed", "That 7.5 s figure came from a fit, not a measurement.", json!({}));
    l.pass().await;
    l.set_item(&task, &explore, "completed");
    // After it closed: a command ends, then a new model call (the sub-agent's result rides on it).
    l.item(&task, "command_execution", "completed", "$ python3 -I /tmp/latan.py\nmedian 4.9 s", json!({}));
    l.item(&task, "reasoning", "completed", "The shelved sliding-window pipeline re-decodes the last 8 seconds.", json!({}));
    l.item(&task, "assistant_message", "completed", "The user was right: the cache is warm on 98% of calls.", json!({}));
    l.end_run(&task, "completed");
    l.pass().await;
    let got = l.deliveries();
    assert_eq!(got.len(), 1, "held for the grace though the turn read its sub-agent: {got:?}");
    assert!(got[0].1.contains("Final reply:\n\nThe user was right"), "{}", got[0].1);
}

/// The same bug note's timing, through the app's watcher: a result is never left for the next 30 s pass. A run that
/// ends after its event-backed wait timed out is seen at once (the wait is re-armed, not left to the next pass), and
/// a result held for the short grace (a background sub-agent closed just before the turn ended, the case T3 hands
/// over in a turn of its own) goes out when the grace ends.
#[tokio::test]
async fn results_go_out_within_seconds_not_at_the_next_pass() {
    let l = Lab::new().await;
    let terminal = |d: &[(String, String)]| d.iter().filter(|(id, _)| id.starts_with("terminal:")).count();
    let plain = l.delegate("Plain").await;
    let (stop, watcher) = l.watcher();
    // The fake's wait times out after 3 s; the run ends after that.
    tokio::time::sleep(Duration::from_millis(3600)).await;
    l.item(&plain, "assistant_message", "completed", "Done.", json!({}));
    let ended = std::time::Instant::now();
    l.end_run(&plain, "completed");
    l.until("the plain result", |d| terminal(d) == 1).await;
    assert!(ended.elapsed() < Duration::from_secs(2), "seen {:?} after the run ended", ended.elapsed());

    l.w.set_hold_timing(1500, 60 * 60 * 1000);
    let task = l.delegate("Beside").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let helper = l.item(&task, "subagent", "running", "", json!({"title":"Helper"}));
    l.item(&task, "command_execution", "completed", "$ ls\nok", json!({}));
    l.set_item(&task, &helper, "completed");
    l.item(&task, "assistant_message", "completed", "Helper's answer folded in: done.", json!({}));
    let ended = std::time::Instant::now();
    l.end_run(&task, "completed");
    l.until("the held result", |d| terminal(d) == 2).await;
    let took = ended.elapsed();
    stop.send(true).unwrap();
    watcher.await.unwrap();
    assert!(took >= Duration::from_millis(1400) && took < Duration::from_secs(4), "held result went out {took:?} after the run ended (grace 1.5 s)");
}

/// Bug note 2026-10-30T21-56-20: three climbs ran with no job island. The relay had started them with T3's own
/// `delegate_task` (children `thread:delegated-task:…` of the relay thread) and later woke them with sends, so
/// Heimdall's tools never saw them start. The observer's pass gives every run of a thread working for Heimdall its
/// island once: the relay's T3 children (a new run in an old one too) and Heimdall's own tasks restarted from inside
/// T3. Native sub-agents, a task's own sub-agents and the user's threads get none, and no run gets a second call.
#[tokio::test]
async fn every_run_working_for_heimdall_gets_its_island_once() {
    let l = Lab::new().await;
    let started: Arc<Mutex<Vec<(String, String)>>> = Arc::default();
    let seen = started.clone();
    l.w.on_task_started(Arc::new(move |t, title, _| {
        seen.lock().unwrap().push((t.into(), title.into()));
        None
    }));
    l.owner_thread("relay", "Heimdall relay", 1);
    l.owner_thread("owner", "User's research", 1);
    let climb = "thread:delegated-task:command%3Amcp%3Arelay%3Adelegate-task%3A1";
    l.sub_thread(climb, "Heimdall website: climb", "relay", "running", json!([]));
    l.with(climb, |t| t["creationSource"] = json!("mcp"));
    // The relay's native sub-agent (Claude's Agent tool) and a T3 child of the user's own thread.
    l.sub_thread("native", "Explore the menu", "relay", "running", json!([]));
    l.with("native", |t| t["creationSource"] = json!("provider"));
    l.sub_thread("thread:delegated-task:owner-child", "User's child", "owner", "running", json!([]));
    l.with("thread:delegated-task:owner-child", |t| t["creationSource"] = json!("mcp"));
    let task = l.delegate("Arsenal site").await;
    assert_eq!(started.lock().unwrap().len(), 1, "delegate_task's own island");

    l.w.observe_for_tests(&l.p()).await.unwrap();
    l.w.observe_for_tests(&l.p()).await.unwrap();
    assert_eq!(
        *started.lock().unwrap(),
        [(task.clone(), "Arsenal site".to_string()), (climb.to_string(), "Heimdall website: climb".to_string())],
        "the relay's climb once, over two passes; the task's run already had its island"
    );
    // The climb's run ends and the relay wakes the old thread (T3's own t3_thread_send); a Heimdall task restarted
    // from inside T3 likewise.
    l.end_run(climb, "completed");
    l.new_run(climb);
    l.end_run(&task, "completed");
    l.new_run(&task);
    l.w.observe_for_tests(&l.p()).await.unwrap();
    l.w.observe_for_tests(&l.p()).await.unwrap();
    let got = started.lock().unwrap().clone();
    assert_eq!(got.len(), 4, "{got:?}");
    assert!(got[2..].contains(&(climb.to_string(), "Heimdall website: climb".to_string())), "{got:?}");
    assert!(got[2..].contains(&(task.clone(), "Arsenal site".to_string())), "{got:?}");
    // The relay thread's title was read from its listing, never by a read of its own.
    assert!(l.calls("t3_thread_read").iter().all(|a| a["threadId"] != "relay"));
}

/// ("figure out why they are not closing themself after the islands responsible for a job have been
/// cleared"): 36 finished task threads piled up. A task whose job island the user closed is archived by Heimdall itself
/// once it is finished and its result went out; a task still at work, waiting on its sub-agents or on a question is
/// never stopped, and is archived only when it has finished and been reported. The user's own threads are never touched.
#[tokio::test]
async fn a_cleared_task_is_archived_once_finished_and_reported_and_never_stopped() {
    let l = Lab::new().await;
    l.w.set_self_close(false); // lane S: this rule is about a finished task that stays open (task_status, a later turn, a cleared island)
    // Finished and reported: archived at once.
    let done = l.delegate("Disk cleanup").await;
    l.item(&done, "assistant_message", "completed", "Freed 12 GB.", json!({}));
    l.end_run(&done, "completed");
    l.pass().await;
    assert_eq!(l.deliveries().len(), 1);
    assert!(l.w.clear_task(&done).unwrap());
    assert_eq!(l.w.archive_cleared().await.unwrap(), vec!["Disk cleanup".to_string()]);
    assert_eq!(l.calls("t3_thread_organize"), vec![json!({"threadId":done,"action":"archive"})]);
    assert_eq!(l.call("task_status", json!({})).await.unwrap(), "No open tasks.");

    // Still running when the user clears its island: nothing is interrupted or archived while it works.
    let running = l.delegate("Voice benchmark").await;
    assert!(l.w.clear_task(&running).unwrap());
    l.pass().await;
    assert!(l.w.archive_cleared().await.unwrap().is_empty());
    assert!(l.calls("t3_thread_interrupt").is_empty(), "a cleared island never stops its task");
    assert_eq!(l.calls("t3_thread_organize").len(), 1);
    // Its turn ends while its sub-agent still works: held, not reported, not archived.
    let agent = l.item(&running, "subagent", "running", "", json!({"title":"Measure Kokoro","subagentId":"node:k"}));
    l.item(&running, "assistant_message", "completed", "Waiting on the Kokoro numbers.", json!({}));
    l.end_run(&running, "completed");
    l.pass().await;
    assert_eq!(l.deliveries().len(), 1, "{:?}", l.deliveries());
    assert_eq!(l.calls("t3_thread_organize").len(), 1, "waiting on a sub-agent: still open");
    // The sub-agent finishes and T3's hand-over turn has the result: reported first, then archived on that pass.
    l.set_item(&running, &agent, "completed");
    l.new_run(&running);
    l.item(&running, "notification", "completed", "Subagent \"Measure Kokoro\" finished", json!({}));
    l.item(&running, "assistant_message", "completed", "Kokoro: 210x real time.", json!({}));
    l.end_run(&running, "completed");
    l.pass().await;
    l.pass().await;
    let got = l.deliveries();
    assert_eq!(got.len(), 2, "{got:?}");
    assert!(got[1].1.contains("Kokoro: 210x real time."), "{got:?}");
    assert_eq!(l.calls("t3_thread_organize").last().unwrap(), &json!({"threadId":running,"action":"archive"}));
    assert_eq!(l.call("task_status", json!({})).await.unwrap(), "No open tasks.");
    assert!(l.calls("t3_thread_interrupt").is_empty());

    // Finished but asking the user a question: kept open.
    let asks = l.delegate("Pick a voice").await;
    l.item(&asks, "assistant_message", "completed", "Warm or bright?", json!({}));
    l.end_run(&asks, "completed");
    l.pass().await;
    l.with(&asks, |t| t["pending"] = json!(["req-1"]));
    assert!(l.w.clear_task(&asks).unwrap());
    assert!(l.w.archive_cleared().await.unwrap().is_empty(), "an open question keeps the task");
    l.with(&asks, |t| t["pending"] = json!([]));
    assert_eq!(l.w.archive_cleared().await.unwrap(), vec!["Pick a voice".to_string()]);

    // Cleared, then given more work before it was archived: the new run shows on an island again, so it stays open.
    let more = l.delegate("Arsenal page").await;
    l.with(&more, |t| t["pending"] = json!(["req-2"]));
    l.item(&more, "assistant_message", "completed", "Which hero image?", json!({}));
    l.end_run(&more, "completed");
    l.pass().await;
    assert!(l.w.clear_task(&more).unwrap());
    l.with(&more, |t| t["pending"] = json!([]));
    l.call("t3_thread_send", json!({"threadId":more,"message":"Use the dark one, then add the pricing row."})).await.unwrap();
    l.end_run(&more, "completed");
    l.item(&more, "assistant_message", "completed", "Pricing row added.", json!({}));
    l.pass().await;
    l.pass().await;
    assert!(l.deliveries().iter().any(|(_, t)| t.contains("Pricing row added.")), "{:?}", l.deliveries());
    assert!(l.w.archive_cleared().await.unwrap().is_empty(), "new work after the clear: kept open");
    assert!(l.call("task_status", json!({})).await.unwrap().contains("'Arsenal page'"));

    // The user's own thread, and a task already archived: not marked.
    l.owner_thread("owner", "User's research", 1);
    assert!(!l.w.clear_task("owner").unwrap());
    assert!(!l.w.clear_task(&done).unwrap());
}

/// Bug note 2026-11-04T18-30-26 ('Beauty climb', 4 Nov 17:55): pending approvals in Heimdall's task threads never
/// woke the agent. The hook answered the rm prompt and the command ran, so that card is no wait; but a real prompt
/// beside it (here one whose card is not in the thread's activity, as a sub-agent's) was hidden by it. Every real wait
/// wakes once per request with the command, Claude's reason and the link that opens the thread, and the task's island
/// can read it (`approval_wait`) until the prompt is gone.
#[tokio::test]
async fn a_task_waiting_on_an_approval_wakes_once_per_request_with_its_command() {
    let l = Lab::new().await;
    let task = l.delegate("Beauty climb").await;
    l.with(&task, |t| t["link"] = json!("<t3-thread://v2/env/beauty>"));
    let card = |call: &str| format!("turn-item:runtime-request:runtime-request%3Aprovider%3AclaudeAgent%3Anative-request%3A{call}%3Ad555");
    let command = |call: &str| format!("turn-item:provider:claudeAgent:native-item:{call}");
    // The hook's answered card: its command ran, T3 still counts it.
    l.item(&task, "command_execution", "completed", "$ sed -i '' 's|rm -f $S/x|y|' tools/render.sh", json!({"itemId":command("toolu_A")}));
    l.item(&task, "approval_request", "waiting", "sed -i … Dangerous rm operation on possibly-empty variable path", json!({"itemId":card("toolu_A")}));
    l.with(&task, |t| t["approvals"] = json!(1));
    l.pass().await;
    let approvals = |l: &Lab| l.deliveries().into_iter().filter(|(id, _)| id.starts_with("approval:")).collect::<Vec<_>>();
    assert!(approvals(&l).is_empty(), "the hook's answered card is no wait: {:?}", l.deliveries());
    assert_eq!(l.w.approval_wait(&task).unwrap(), None);
    // A second prompt T3 counts, its card not in this thread's activity: a wait, though the answered card is in view.
    l.with(&task, |t| t["approvals"] = json!(2));
    l.pass().await;
    let got = approvals(&l);
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0].0, format!("approval:{task}:run:{task}:1"));
    assert!(got[0].1.contains("'Beauty climb'") && got[0].1.contains("waits for an approval") && got[0].1.contains("Open it: t3-thread://v2/env/beauty"), "{}", got[0].1);
    // A real prompt with its card: a wake of its own, with the command and the reason.
    l.item(&task, "command_execution", "running", "$ rm -f $S/$w-t9*.png", json!({"itemId":command("toolu_B")}));
    l.item(&task, "approval_request", "waiting", "rm -f $S/$w-t9*.png\nDangerous rm operation on possibly-empty variable path: $S/$w-t9*.png", json!({"itemId":card("toolu_B")}));
    l.with(&task, |t| t["approvals"] = json!(3));
    l.pass().await;
    l.pass().await;
    let got = approvals(&l);
    assert_eq!(got.len(), 2, "one wake per request, never repeated: {got:?}");
    let b = got.iter().find(|(id, _)| id == &format!("approval:{task}:run:{task}:1:toolu_B")).expect("the card's own wake");
    assert!(b.1.contains("It asks: rm -f $S/$w-t9*.png Dangerous rm operation on possibly-empty variable path"), "{}", b.1);
    let wait = l.w.approval_wait(&task).unwrap().expect("the island's record");
    assert_eq!(wait.link.as_deref(), Some("t3-thread://v2/env/beauty"));
    assert!(wait.text.as_deref().is_some_and(|t| t.starts_with("rm -f $S/$w-t9*.png Dangerous rm")), "{wait:?}");
    // The run ends: no wait any more, and its result wakes as usual.
    l.item(&task, "assistant_message", "completed", "Climb done.", json!({}));
    l.end_run(&task, "completed");
    l.pass().await;
    assert_eq!(l.w.approval_wait(&task).unwrap(), None);
    assert!(l.deliveries().iter().any(|(id, text)| id.starts_with("terminal:") && text.contains("Climb done.")), "{:?}", l.deliveries());
}

/// Bug note 2026-11-04T22-25-15 (setup-wizard task, 4 Nov 22:22): T3 queued its own turn to hand over a finished
/// sub-agent while the task's turn still ran, then cancelled that queued turn before it started. T3's thread status
/// is its latest run's, so the task read "cancelled" while its turn (and its work) went on, and once that turn ended
/// the dropped run would have been reported as the task's ending. A run that never started is no state: the task
/// stays working, nothing terminal goes out, and the turn that ran brings the result.
#[tokio::test]
async fn a_queued_run_t3_cancelled_before_it_started_is_not_the_tasks_ending() {
    let l = Lab::new().await;
    let task = l.delegate("Rebuild setup wizard").await;
    let first = format!("run:{task}:1");
    let helper = l.item(&task, "subagent", "running", "", json!({"title":"Rebuild native wizard"}));
    l.pass().await;
    l.set_item(&task, &helper, "completed");
    let dropped = l.queue_run(&task);
    l.set_run(&task, &dropped, "cancelled");
    l.pass().await;
    l.pass().await;
    assert!(l.deliveries().is_empty(), "nothing ended: {:?}", l.deliveries());
    let status = l.call("task_status", json!({"taskId":task})).await.unwrap();
    assert!(status.contains("· working") && !status.contains("cancelled"), "{status}");
    l.item_in(&task, &first, "assistant_message", "completed", "The wizard is rebuilt.", json!({}));
    l.set_run(&task, &first, "completed");
    l.pass().await;
    l.pass().await;
    let got = l.deliveries();
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0].0, format!("terminal:{task}:{first}:completed"));
    assert!(got[0].1.contains("completed") && got[0].1.contains("The wizard is rebuilt."), "{}", got[0].1);
    assert!(!got.iter().any(|(id, _)| id.contains("cancelled")), "{got:?}");
}

/// Review 5 Nov (b): Heimdall's own follow-up into a finished task, cancelled by T3 before it started. The task no
/// longer reads "completed" with the earlier turn's result: it reads cancelled, and one notice says nothing of the
/// message ran, so it can be sent again. Also when the message was queued behind the turn that was still running:
/// that turn's own result still goes out first, once.
#[tokio::test]
async fn heimdalls_own_message_t3_dropped_before_it_started_reads_cancelled() {
    let l = Lab::new().await;
    let task = l.delegate("Disk cleanup").await;
    l.item(&task, "assistant_message", "completed", "Freed 12 GB.", json!({}));
    l.end_run(&task, "completed");
    l.pass().await;
    assert_eq!(l.deliveries().len(), 1);
    l.call("t3_call", json!({"tool":"t3_thread_send","args":{"threadId":task,"message":"Also clear the Xcode caches."}})).await.unwrap();
    // T3 drops it before the provider starts it.
    l.with(&task, |t| {
        let r = t["runs"].as_array_mut().unwrap().last_mut().unwrap();
        r["status"] = json!("cancelled");
        r["startedAt"] = json!(null);
        r["completedAt"] = json!(iso(0));
    });
    l.pass().await;
    l.pass().await;
    let got = l.deliveries();
    assert_eq!(got.len(), 2, "{got:?}");
    assert_eq!(got[1].0, format!("terminal:{task}:run:{task}:2:cancelled"));
    assert!(got[1].1.contains("cancelled Heimdall's message before it started") && !got[1].1.contains("Freed 12 GB"), "{}", got[1].1);
    let status = l.call("task_status", json!({"taskId":task})).await.unwrap();
    assert!(status.contains("cancelled") && !status.contains("completed"), "{status}");

    // Queued behind a turn that is still running, then dropped when that turn ends.
    let busy = l.delegate("Voice benchmark").await;
    let first = format!("run:{busy}:1");
    l.call("t3_call", json!({"tool":"t3_thread_send","args":{"threadId":busy,"message":"Then try Kokoro too.","mode":"queue"}})).await.unwrap();
    let queued = format!("run:{busy}:2");
    l.item_in(&busy, &first, "assistant_message", "completed", "Supertonic: 120x real time.", json!({}));
    l.set_run(&busy, &first, "completed");
    l.set_run(&busy, &queued, "cancelled");
    l.pass().await;
    l.pass().await;
    let mine: Vec<_> = l.deliveries().into_iter().filter(|(id, _)| id.contains(&busy)).collect();
    assert_eq!(mine.len(), 2, "{mine:?}");
    assert!(mine[0].0 == format!("terminal:{busy}:{first}:completed") && mine[0].1.contains("Supertonic: 120x"), "{mine:?}");
    assert_eq!(mine[1].0, format!("terminal:{busy}:{queued}:cancelled"));
}

/// Review 5 Nov (a): a result recorded against a run T3 dropped before it started (as before the never-started rule)
/// must not make the earlier run look newer: the cleared, reported task is archived. A run T3 itself dropped (not
/// Heimdall's message) is still no ending.
#[tokio::test]
async fn a_result_recorded_against_a_dropped_run_still_lets_the_task_be_archived() {
    let l = Lab::new().await;
    let task = l.delegate("Rebuild setup wizard").await;
    let first = format!("run:{task}:1");
    l.item_in(&task, &first, "assistant_message", "completed", "The wizard is rebuilt.", json!({}));
    l.set_run(&task, &first, "completed");
    // T3 queues and drops its own hand-over turn, and an older build's pass recorded the task's ending against it.
    let dropped = l.queue_run(&task);
    l.set_run(&task, &dropped, "cancelled");
    {
        // What it wrote: the latest run (the dropped one), T3's status, and the notice that went out for it.
        let db = rusqlite::Connection::open(l.dir.path().join("workers.db")).unwrap();
        db.execute("UPDATE workers SET run=?,status='cancelled' WHERE thread=?", [&dropped, &task]).unwrap();
        db.execute("INSERT INTO notices VALUES(?,?,?,'terminal')", [format!("terminal:{task}:{dropped}:cancelled"), task.clone(), dropped.clone()]).unwrap();
    }
    l.pass().await;
    assert!(l.deliveries().is_empty(), "the ending is not reported again under the earlier run: {:?}", l.deliveries());
    assert!(l.w.clear_task(&task).unwrap());
    assert_eq!(l.w.archive_cleared().await.unwrap(), vec!["Rebuild setup wizard".to_string()]);
}

/// Bug note 2026-11-06T11-57-54 (the user, 11:57): task threads stopped on approval prompts and Heimdall never said so.
/// T3's list rows carry no request count and a run waiting on an approval still reads "running", so the sweep of
/// sub-agents never looked at a running child's approval; a task T3 showed "waiting" without counting the request
/// was skipped too; and the relay's own listing said "approvals are not listed". Each now reaches Heimdall once.
#[tokio::test]
async fn an_approval_in_a_running_sub_agent_or_an_uncounted_wait_reaches_heimdall() {
    let l = Lab::new().await;
    let task = l.delegate("Arsenal VM test").await;
    l.with(&task, |t| t["link"] = json!("<t3-thread://v2/env/vm>"));
    l.item(&task, "subagent", "running", "", json!({"title":"Install probe"}));
    // Two running sub-agents, as T3 lists them: one waits on an approval, one only works.
    l.sub_thread("c-ask", "Install probe", &task, "running", json!([]));
    l.with("c-ask", |t| t["approvals"] = json!(1));
    let card = "turn-item:runtime-request:runtime-request%3Aprovider%3AclaudeAgent%3Anative-request%3Atoolu_R%3Ae1";
    l.item("c-ask", "command_execution", "running", "$ rm -rf $VM/disk.img", json!({"itemId":"turn-item:provider:claudeAgent:native-item:toolu_R"}));
    l.item("c-ask", "approval_request", "waiting", "rm -rf $VM/disk.img\nDangerous rm operation on possibly-empty variable path", json!({"itemId":card}));
    l.sub_thread("c-busy", "Survey", &task, "running", json!([]));
    l.pass().await;
    l.pass().await;
    let approvals = |l: &Lab| l.deliveries().into_iter().filter(|(id, _)| id.starts_with("approval:")).collect::<Vec<_>>();
    let got = approvals(&l);
    assert_eq!(got.len(), 1, "one notice for the running child's approval, none for the busy one: {got:?}");
    assert_eq!(got[0].0, "approval:c-ask:run:c-ask:1:toolu_R");
    assert!(got[0].1.contains("its sub-agent 'Install probe' waits for an approval") && got[0].1.contains("It asks: rm -rf $VM/disk.img Dangerous rm"), "{}", got[0].1);
    // The relay's own listing names it, though T3's lists no approvals.
    let listed = l.call("t3_call", json!({"tool":"t3_pending_request","args":{"action":"list","threadId":"c-ask"}})).await.unwrap();
    assert!(listed.contains("Approval waiting (T3 lists no approvals; only the user can give it, in T3): rm -rf $VM/disk.img"), "{listed}");
    let quiet = l.call("t3_call", json!({"tool":"t3_pending_request","args":{"action":"list","threadId":"c-busy"}})).await.unwrap();
    assert!(!quiet.contains("Approval waiting"), "{quiet}");
    // The task itself: T3 shows it "waiting" but counts nothing. A wait, said once, and named by task_status.
    let run = l.with(&task, |t| t["runs"].as_array().unwrap().last().unwrap()["runId"].as_str().unwrap().to_owned());
    l.set_run(&task, &run, "waiting");
    l.pass().await;
    l.pass().await;
    let got = approvals(&l);
    assert_eq!(got.len(), 2, "{got:?}");
    assert!(got.iter().any(|(id, text)| id == &format!("approval:{task}:{run}") && text.contains("Open it: t3-thread://v2/env/vm")), "{got:?}");
    let status = l.call("task_status", json!({"taskId":task})).await.unwrap();
    assert!(status.contains("waits for the user's approval in T3 (open it: t3-thread://v2/env/vm)"), "{status}");
    // Each new delegate_task brief carries the steering away from the asks Claude Code keeps (part 2).
    l.w.set_brief_preamble("Rules.");
    l.call("delegate_task", json!({"task":"Fix it.","title":"Fix","target":{"model":"claude-opus-5-5"},"path":l.dir.path()})).await.unwrap();
    let brief = l.calls("t3_thread_launch").last().unwrap()["message"].as_str().unwrap().to_owned();
    assert!(brief.contains("`rm -rf` on literal absolute paths") && brief.contains("`.claude/` settings") && brief.contains("full-access mode"), "{brief}");
}

/// Bug note 2026-11-06T12-26-36 (12:08): five tasks failed one after another on the main Claude account's weekly limit
/// while the second account was at 0%, and nothing moved them; at 12:27 the relay moved them by hand. A task that
/// fails on a usage limit now moves to the account with quota and resumes there ("check what's on disk"), with one
/// quiet notice; a new task goes to that account too; it never loops back onto a spent account, and with the failover
/// off it is reported as before.
#[tokio::test]
async fn a_task_that_hits_a_usage_limit_moves_to_the_account_with_quota_once() {
    use t3::workers::{AccountsHook, ACCOUNTS_OFF};
    use agent::accounts::{AccountQuota, Kind, Window};
    let l = Lab::new().await;
    let now = agent::now_ms();
    let acct = |instance: &str, used: f64| AccountQuota { instance: instance.into(), kind: Kind::Claude, windows: vec![Window { name: "weekly".into(), used_percent: Some(used), resets_at_ms: Some(now + 3 * 86_400_000) }], reached: None, error: None };
    // 12:26's reading: default weekly 100%, ~/.claude-account2 weekly 0%.
    let reading = Arc::new(Mutex::new(Ok(vec![acct("claudeAgent", 100.0), acct("claudeAgent_2", 0.0)])));
    let r = reading.clone();
    let hook: AccountsHook = Arc::new(move |_wait| r.lock().unwrap().clone());
    l.w.set_accounts(hook);
    let task = l.delegate("Arsenal VM test").await;
    l.s.second_account.store(true, std::sync::atomic::Ordering::Release);
    l.item(&task, "assistant_message", "completed", "You've hit your weekly limit · resets Nov 9 at 1pm (Europe/Warsaw)", json!({}));
    l.end_run(&task, "failed");
    l.pass().await;
    l.pass().await;
    let configured = l.calls("t3_thread_configure");
    assert_eq!(configured.len(), 1, "{configured:?}");
    // The task keeps its effort on the other account (night review, 6 Nov).
    assert_eq!(configured[0], json!({"threadId":task,"modelSelection":{"instanceId":"claudeAgent_2","model":"claude-opus-5-5","options":{"effort":"high"}}}));
    let sends = l.calls("t3_thread_send");
    assert_eq!(sends.len(), 1);
    assert!(sends[0]["message"].as_str().unwrap().starts_with("Your run stopped when the main Claude account hit its weekly limit. Heimdall moved this thread to Claude code 2."), "{}", sends[0]);
    assert!(sends[0]["clientRequestId"].as_str().unwrap().starts_with("heimdall-failover-"));
    let got = l.deliveries();
    assert!(!got.iter().any(|(id, _)| id.starts_with("terminal:")), "the limit is not news, the move is: {got:?}");
    let moved: Vec<_> = got.iter().filter(|(id, _)| id.starts_with("t3-failover:")).collect();
    assert_eq!(moved.len(), 1, "{got:?}");
    assert_eq!(moved[0].1, format!("Task 'Arsenal VM test' ({task}) stopped on the main Claude account's weekly limit; Heimdall moved it to Claude code 2 and resumed it there. Nothing to do."));
    assert_eq!(agent::notice::NoticeKind::classify(&moved[0].0, &moved[0].1), agent::notice::NoticeKind::Context, "quiet: no wake");
    // A new task asked for on the spent account starts on the one with quota.
    let started = l.call("delegate_task", json!({"task":"Next.","title":"Promo","target":{"model":"claude-opus-5-5","providerInstanceId":"claudeAgent"},"path":l.dir.path()})).await.unwrap();
    assert_eq!(l.calls("t3_thread_launch").last().unwrap()["modelSelection"]["instanceId"], "claudeAgent_2");
    assert!(started.contains("It runs on Claude code 2: the main Claude account has no quota left."), "{started}");
    // The resumed run hits the second account's limit too, while the main one is still spent (the reading is stale):
    // no move back, no loop; the failure is reported with the earlier turn and why it stayed.
    l.item(&task, "assistant_message", "completed", "You've hit your weekly limit · resets Nov 13 at 9am", json!({}));
    l.end_run(&task, "failed");
    l.pass().await;
    l.pass().await;
    assert_eq!(l.calls("t3_thread_configure").len(), 1, "never moved twice for one limit");
    let terminal: Vec<_> = l.deliveries().into_iter().filter(|(id, _)| id.starts_with(&format!("terminal:{task}"))).collect();
    assert_eq!(terminal.len(), 1, "{terminal:?}");
    assert!(terminal[0].1.contains("it was not moved again (one move per limit reset)"), "{}", terminal[0].1);
    // Off (Settings::task_failover_off): a limit-failed task is reported as before, nothing moves.
    *reading.lock().unwrap() = Err(ACCOUNTS_OFF.into());
    l.s.second_account.store(false, std::sync::atomic::Ordering::Release);
    let other = l.delegate("Feedback inbox").await;
    l.item(&other, "assistant_message", "completed", "You've hit your weekly limit · resets Nov 9 at 1pm (Europe/Warsaw)", json!({}));
    l.end_run(&other, "failed");
    l.pass().await;
    assert_eq!(l.calls("t3_thread_configure").len(), 1);
    let done: Vec<_> = l.deliveries().into_iter().filter(|(id, _)| id.starts_with(&format!("terminal:{other}"))).collect();
    assert_eq!(done.len(), 1, "{:?}", l.deliveries());
    assert!(!done[0].1.contains("moved"), "{}", done[0].1);
}

/// Bug note 2026-11-08T02-03-49 (01:58:55, 01:59:00): delegate_task named no account, both Claude accounts offered
/// Opus, and it answered MODEL_AMBIGUOUS although the main account was spent until Mon 13:00. Now the account with
/// quota runs it; with both usable the main account (T3's first) is the stable default, still no error.
#[tokio::test]
async fn a_task_with_no_account_named_runs_on_the_one_with_quota() {
    use t3::workers::AccountsHook;
    use agent::accounts::{AccountQuota, Kind, Window};
    let l = Lab::new().await;
    let now = agent::now_ms();
    let acct = |instance: &str, used: f64| AccountQuota { instance: instance.into(), kind: Kind::Claude, windows: vec![Window { name: "weekly".into(), used_percent: Some(used), resets_at_ms: Some(now + 86_400_000) }], reached: None, error: None };
    let reading = Arc::new(Mutex::new(Ok(vec![acct("claudeAgent", 100.0), acct("claudeAgent_2", 3.0)])));
    let r = reading.clone();
    let hook: AccountsHook = Arc::new(move |_wait| r.lock().unwrap().clone());
    l.w.set_accounts(hook);
    l.s.second_account.store(true, std::sync::atomic::Ordering::Release);
    let start = |title: &str| json!({"task":"Go.","title":title,"target":{"model":"claude-opus-5-5"},"path":l.dir.path()});
    let started = l.call("delegate_task", start("Clip")).await.unwrap();
    assert_eq!(l.calls("t3_thread_launch").last().unwrap()["modelSelection"]["instanceId"], "claudeAgent_2", "{started}");
    // Both with quota: the main account, no MODEL_AMBIGUOUS.
    *reading.lock().unwrap() = Ok(vec![acct("claudeAgent", 40.0), acct("claudeAgent_2", 3.0)]);
    l.call("delegate_task", start("Next")).await.unwrap();
    assert_eq!(l.calls("t3_thread_launch").last().unwrap()["modelSelection"]["instanceId"], "claudeAgent");
    // No reading at all (the failover off): T3's first, still no error.
    *reading.lock().unwrap() = Err(t3::workers::ACCOUNTS_OFF.into());
    l.call("delegate_task", start("Third")).await.unwrap();
    assert_eq!(l.calls("t3_thread_launch").last().unwrap()["modelSelection"]["instanceId"], "claudeAgent");
}

/// Night review H1 and H2 (6 Nov): a plan that was dropped without moving (T3 started a queued turn on the spent
/// account first) no longer blocks the real move when that turn fails on the limit too; and a Codex thread's "You've
/// hit your usage limit" is no Claude limit, so nothing tries to move it to a Claude account.
#[tokio::test]
async fn a_dropped_failover_plan_does_not_block_the_move_and_codex_limits_never_move() {
    use t3::workers::AccountsHook;
    use agent::accounts::{AccountQuota, Kind, Window};
    let l = Lab::new().await;
    let now = agent::now_ms();
    let acct = |instance: &str, used: f64| AccountQuota { instance: instance.into(), kind: Kind::Claude, windows: vec![Window { name: "weekly".into(), used_percent: Some(used), resets_at_ms: Some(now + 3 * 86_400_000) }], reached: None, error: None };
    let codex = AccountQuota { instance: "codex".into(), kind: Kind::Codex, windows: vec![], reached: None, error: None };
    let reading = vec![acct("claudeAgent", 100.0), acct("claudeAgent_2", 0.0), codex];
    let hook: AccountsHook = Arc::new(move |_wait| Ok(reading.clone()));
    l.w.set_accounts(hook);
    let task = l.delegate("Arsenal VM test").await;
    let other = l.delegate("Feedback inbox").await;
    l.s.second_account.store(true, std::sync::atomic::Ordering::Release);
    let first = l.with(&task, |t| t["runs"][0]["runId"].as_str().unwrap().to_owned());
    // The plan for the first failed run, and then T3 starting the queued steer on the same account before the move.
    rusqlite::Connection::open(l.dir.path().join("workers.db")).unwrap().execute(
        "INSERT INTO task_failovers(thread,run,from_instance,to_instance,model,limit_window,at_ms,until_ms,sent_ms) VALUES(?1,?2,'claudeAgent','claudeAgent_2','claude-opus-5-5','weekly',?3,?4,NULL)",
        rusqlite::params![task, first, now as i64, (now + 3 * 86_400_000) as i64],
    ).unwrap();
    l.set_run(&task, &first, "failed");
    l.new_run(&task);
    l.pass().await;
    assert!(l.calls("t3_thread_configure").is_empty(), "the plan was dropped: its run is not the task's any more");
    // That turn fails on the limit too: now it moves.
    l.item(&task, "assistant_message", "completed", "You've hit your weekly limit · resets Nov 9 at 1pm (Europe/Warsaw)", json!({}));
    l.end_run(&task, "failed");
    l.pass().await;
    l.pass().await;
    assert_eq!(l.calls("t3_thread_configure").len(), 1, "{:?}", l.deliveries());
    assert!(!l.deliveries().iter().any(|(_, text)| text.contains("one move per limit reset")), "{:?}", l.deliveries());
    // A Codex task with Codex's own limit wording: an ordinary failure, no move, no Claude account marked spent.
    l.with(&other, |t| t["provider"] = json!("codex"));
    l.item(&other, "assistant_message", "completed", "You've hit your usage limit. Upgrade to Pro or try again in 3 days.", json!({}));
    l.end_run(&other, "failed");
    l.pass().await;
    l.pass().await;
    assert_eq!(l.calls("t3_thread_configure").len(), 1, "the Codex task is not moved");
    let done: Vec<_> = l.deliveries().into_iter().filter(|(id, _)| id.starts_with(&format!("terminal:{other}"))).collect();
    assert_eq!(done.len(), 1, "{:?}", l.deliveries());
    assert!(!done[0].1.contains("moved") && !done[0].1.contains("quota"), "{}", done[0].1);
}

/// Bug note 2026-11-08T06-13-58: two tasks ignored Heimdall's steers (the user's newer words, 06:02 and 06:07) as text
/// "attached to" their instructions. Every message Heimdall sends now starts with a mark saying who speaks: into its
/// own task, its orchestrator speaking for the user; into the user's other threads, his voice assistant. One mark each, by
/// `t3_thread_send` or through `t3_call`, steer or queue, and a message already marked keeps one.
#[tokio::test]
async fn heimdalls_messages_say_they_come_from_heimdall() {
    let l = Lab::new().await;
    let task = l.delegate("Re-rank the job offers").await;
    l.call("t3_thread_send", json!({"threadId":task,"message":"User (06:02): a fresh search is allowed.","mode":"steer"})).await.unwrap();
    l.call("t3_call", json!({"tool":"t3_thread_send","args":{"threadId":task,"message":"Only the newest generation."}})).await.unwrap();
    let marked = format!("{} Only the newest generation.", t3::workers::steer_mark());
    l.call("t3_thread_send", json!({"threadId":task,"message":marked})).await.unwrap();
    l.owner_thread("t-own", "User's research", 1);
    l.call("t3_thread_send", json!({"threadId":"t-own","message":"Status?"})).await.unwrap();
    let sent: Vec<String> = l.calls("t3_thread_send").iter().map(|c| c["message"].as_str().unwrap().to_owned()).collect();
    assert_eq!(sent, [
        format!("{} User (06:02): a fresh search is allowed.", t3::workers::steer_mark()),
        marked.clone(),
        marked,
        format!("{} Status?", t3::workers::message_mark()),
    ]);
    assert!(t3::workers::steer_mark().contains("From Heimdall") && t3::workers::steer_mark().contains("orchestrator"));
    assert_eq!(t3::workers::from_heimdall("  ", true), "  ");
    assert_eq!(t3::workers::unmarked(&t3::workers::from_heimdall("Go on.", true)), "Go on.");
    assert_eq!(t3::workers::unmarked("Go on."), "Go on.");
    // The task rules every task gets say what the mark means, in both editions.
    let rules = include_str!("../../agent/prompt/worker.md");
    let rules = agent::identity::template(rules, &agent::identity::current()).split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(rules.contains(&format!("starts with `{}` comes from Heimdall, the orchestrator that started this task", t3::workers::steer_mark())), "{rules}");
}

// ---- Lane S (night 10–11 Nov): sends that always land, tasks that close themselves, a default target. ----

/// Bug note 2026-11-10T09-03-14: a steer into a thread with no running turn failed, three times on 10 Nov with
/// "unarchiving it failed too … is not archived", and the relay retried each with `auto`. The three shapes the relay
/// met (a finished task, an idle one, an archived one) all land as a new turn in one call; the user's archived thread
/// keeps T3's refusal.
#[tokio::test]
async fn a_send_always_lands_on_a_finished_idle_or_archived_task() {
    let l = Lab::new().await;
    let finished = l.delegate("Finished task").await;
    l.item(&finished, "assistant_message", "completed", "Done.", json!({}));
    l.end_run(&finished, "completed");
    let idle = l.delegate("Idle task").await;
    l.end_run(&idle, "interrupted");
    let archived = l.delegate("Archived task").await;
    l.end_run(&archived, "completed");
    l.with(&archived, |t| t["archived"] = json!(true));
    for (n, id) in [&finished, &idle, &archived].into_iter().enumerate() {
        let sent = l.call("t3_thread_send", json!({"threadId":id,"message":format!("Steer {n}"),"mode":"steer"})).await.unwrap_or_else(|e| panic!("{id}: {e}"));
        assert!(sent.starts_with(&format!("Sent to thread {id} (started")), "{id}: {sent}");
        assert!(!sent.contains("unarchiv"), "{sent}");
    }
    assert_eq!(l.calls("t3_thread_organize").iter().filter(|c| c["action"] == "unarchive").map(|c| c["threadId"].clone()).collect::<Vec<_>>(), vec![json!(archived)]);
    // Each refused steer went again once, as a new turn under its own retry key.
    let sends = l.calls("t3_thread_send");
    assert_eq!(sends.len(), 6, "{sends:?}");
    assert!(sends.iter().skip(1).step_by(2).all(|s| s["mode"] == "auto"), "{sends:?}");
    // The same through t3_call, the relay's other way in.
    l.end_run(&idle, "completed");
    let sent = l.call("t3_call", json!({"tool":"t3_thread_send","args":{"threadId":idle,"message":"Again","mode":"steer"}})).await.unwrap();
    assert!(sent.starts_with(&format!("Sent to thread {idle} (started")), "{sent}");
    // A running turn is still steered, not restarted.
    let sent = l.call("t3_thread_send", json!({"threadId":idle,"message":"More","mode":"steer"})).await.unwrap();
    assert!(sent.starts_with(&format!("Sent to thread {idle} (steered")), "{sent}");
    // The user's own archived thread is not Heimdall's to unarchive.
    l.owner_thread("owner", "User's research", 1);
    l.with("owner", |t| t["archived"] = json!(true));
    let e = l.call("t3_thread_send", json!({"threadId":"owner","message":"Carry on.","mode":"steer"})).await.unwrap_err().to_string();
    assert!(e.contains("is archived") && !e.contains("unarchiving"), "{e}");
    // Review 1111 S4: nor is his idle thread Heimdall's to start a turn in; T3's refusal says how to do it on purpose.
    l.owner_thread("owner-idle", "User's notes", 1);
    let before = l.calls("t3_thread_send").len();
    let e = l.call("t3_thread_send", json!({"threadId":"owner-idle","message":"Carry on.","mode":"steer"})).await.unwrap_err().to_string();
    assert!(e.starts_with("T3_TOOL_FAILED") && e.contains("no running turn") && e.contains("ask the user first"), "{e}");
    assert_eq!(l.calls("t3_thread_send").len(), before + 1, "no retry as a new turn");
    let sent = l.call("t3_thread_send", json!({"threadId":"owner-idle","message":"Carry on.","mode":"auto"})).await.unwrap();
    assert!(sent.starts_with("Sent to thread owner-idle (started"), "{sent}");
}

/// Review 1111 S5: the user's click approves one call, also when that call finds the thread already archived.
#[tokio::test]
async fn an_already_archived_answer_spends_owners_confirmation() {
    let l = Lab::new().await;
    l.owner_thread("owner-old", "User's old notes", 1);
    l.with("owner-old", |t| t["archived"] = json!(true));
    let call = json!({"tool":"t3_thread_organize","args":{"threadId":"owner-old","action":"archive"}});
    let e = l.call("t3_call", call.clone()).await.unwrap_err().to_string();
    assert!(e.starts_with("CONFIRM_REQUIRED"), "{e}");
    let question = e.split('"').nth(1).unwrap().to_owned();
    let yes = l.w.actions.ask("ask-archive", &[agent::actions::AskItem::one(&question, &["Yes", "No"])], &l.p()).unwrap();
    l.w.actions.answer(&yes, "Yes", &l.p()).unwrap();
    let mut with = call.clone();
    with["confirm"] = json!(yes);
    assert_eq!(l.call("t3_call", with.clone()).await.unwrap(), "Thread owner-old is already archived; nothing to do.");
    let e = l.call("t3_call", with).await.unwrap_err().to_string();
    assert!(e.starts_with("CONFIRM_USED"), "{e}");
}

/// Proposal 1 (tool calls, 10 Nov): the relay closed 254 of 363 delivered tasks with task_cancel. A task whose run
/// completed with nothing open is archived as its final notice goes out, and the notice says so; a failed run or an
/// open question keeps it open; a message to it reopens it.
#[tokio::test]
async fn a_finished_task_closes_itself_with_its_notice_unless_something_is_open() {
    let l = Lab::new().await;
    let done = l.delegate("Report").await;
    l.item(&done, "assistant_message", "completed", "Report written.", json!({}));
    l.end_run(&done, "completed");
    let failed = l.delegate("Broken build").await;
    l.item(&failed, "assistant_message", "completed", "The build failed.", json!({}));
    l.end_run(&failed, "failed");
    l.pass().await;
    let got = l.deliveries();
    let notice = got.iter().find(|d| d.1.contains("Report written.")).unwrap_or_else(|| panic!("{got:?}")).1.clone();
    assert!(notice.contains(&format!("Report written.\n\n{}", t3::workers::CLOSED)), "{notice}");
    let failed_notice = got.iter().find(|d| d.1.contains("The build failed.")).unwrap().1.clone();
    assert!(!failed_notice.contains(t3::workers::CLOSED), "{failed_notice}");
    assert_eq!(l.calls("t3_thread_organize"), vec![json!({"threadId":done,"action":"archive"})]);
    let status = l.call("task_status", json!({})).await.unwrap();
    assert!(!status.contains(&done) && status.contains(&failed), "{status}");
    // Reopened by a message, followed again, and closed again at its next result.
    let sent = l.call("t3_thread_send", json!({"threadId":done,"message":"Add the chart."})).await.unwrap();
    assert!(sent.starts_with(&format!("Sent to thread {done} (started")), "{sent}");
    l.item(&done, "assistant_message", "completed", "Chart added.", json!({}));
    l.end_run(&done, "completed");
    l.pass().await;
    assert!(l.deliveries().iter().any(|d| d.1.contains("Chart added.") && d.1.contains(t3::workers::CLOSED)), "{:?}", l.deliveries());
    // task_cancel on a closed task still answers plainly.
    assert_eq!(l.call("task_cancel", json!({"taskId":done})).await.unwrap(), "Archived 'Report'.");
    // An open question in T3 keeps it open.
    let asks = l.delegate("Asks first").await;
    l.item(&asks, "assistant_message", "completed", "Which engine first?", json!({}));
    l.with(&asks, |t| t["pending"] = json!(["req-1"]));
    l.end_run(&asks, "completed");
    l.pass().await;
    assert!(!l.calls("t3_thread_organize").iter().any(|c| c["threadId"] == json!(asks) && c["action"] == "archive"));
}

/// Review 1111 S1: Vault workers put their question for the user in the final reply, not as a T3 question, so T3 shows
/// nothing open. A reply ending in a question, one putting a decision to the user, and one reporting a staged build keep
/// the task open (archiving it would remove its build before he answers); a plain done reply still closes it.
#[tokio::test]
async fn a_finished_task_whose_reply_waits_on_owner_stays_open() {
    let l = Lab::new().await;
    let waits = [
        ("Asks", "Built and checked the menu.\n\nInstall it?"),
        ("Decision", "Fixed the parser.\n\nShould I also rewrite the importer. Default: no."),
        ("Staged", "Staged the build at ~/Applications/.Heimdall.app.stage; the check passed.\n\nThat is all."),
    ];
    let mut open = vec![];
    for (title, reply) in waits {
        let t = l.delegate(title).await;
        l.item(&t, "assistant_message", "completed", reply, json!({}));
        l.end_run(&t, "completed");
        open.push(t);
    }
    let done = l.delegate("Plain").await;
    l.item(&done, "assistant_message", "completed", "Report written. Nothing needs User.", json!({}));
    l.end_run(&done, "completed");
    l.pass().await;
    let got = l.deliveries();
    for (title, _) in waits {
        let n = got.iter().find(|d| d.1.contains(&format!("'{title}'"))).unwrap_or_else(|| panic!("{title}: {got:?}"));
        assert!(!n.1.contains(t3::workers::CLOSED), "{}", n.1);
    }
    assert!(got.iter().any(|d| d.1.contains("Report written.") && d.1.contains(t3::workers::CLOSED)), "{got:?}");
    assert_eq!(l.calls("t3_thread_organize"), vec![json!({"threadId":done,"action":"archive"})]);
    let status = l.call("task_status", json!({})).await.unwrap();
    assert!(open.iter().all(|t| status.contains(t.as_str())), "{status}");
}

/// Bug note 2026-11-12T00-19-55: the night pass integrator ended a turn at 23:04 waiting on a lane it had started
/// with T3's own delegate_task; the self-close archived it, T3 disposed of the lane's completion at 00:01 because
/// its parent was archived, and the pass stood still for 75 minutes. A task with a live delegated child stays open
/// (its notice says why), a cleared island does not archive it either, and once T3 wakes it after the child finished,
/// that turn's result closes it as usual.
#[tokio::test]
async fn a_finished_task_with_a_live_delegated_child_is_not_closed_or_archived() {
    let l = Lab::new().await;
    let parent = l.delegate("Night pass").await;
    let lane = "thread:delegated-task:command%3Amcp%3Aint%3Adelegate-task%3Alane-k";
    l.sub_thread(lane, "Lane K: secret audit", &parent, "running", json!([]));
    // A finished delegated child and one of another parent hold nothing.
    l.sub_thread("thread:delegated-task:done", "Lane J", &parent, "completed", json!([]));
    l.sub_thread("thread:delegated-task:other", "Other lane", "someone-else", "running", json!([]));
    l.item(&parent, "assistant_message", "completed", "Check passed. Waiting on lane K.", json!({}));
    l.end_run(&parent, "completed");
    l.pass().await;
    let got = l.deliveries();
    let notice = got.iter().find(|d| d.1.contains("Waiting on lane K.")).unwrap_or_else(|| panic!("{got:?}")).1.clone();
    assert!(!notice.contains(t3::workers::CLOSED), "{notice}");
    assert!(notice.contains("delegated task 'Lane K: secret audit' still runs"), "{notice}");
    assert!(!notice.contains("Lane J") && !notice.contains("Other lane"), "{notice}");
    assert!(l.calls("t3_thread_organize").is_empty(), "{:?}", l.calls("t3_thread_organize"));
    // The user clearing its island archives it only once the child is done.
    assert!(l.w.clear_task(&parent).unwrap());
    assert!(l.w.archive_cleared().await.unwrap().is_empty());
    assert!(l.calls("t3_thread_organize").is_empty());
    assert!(l.call("task_status", json!({})).await.unwrap().contains(&parent));
    // The lane finishes; T3 wakes the parent with its result (a new turn); that turn's result closes it.
    l.set_run(lane, &format!("run:{lane}:1"), "completed");
    l.new_run(&parent);
    l.item(&parent, "assistant_message", "completed", "Lane K merged. The pass is done.", json!({}));
    l.end_run(&parent, "completed");
    l.pass().await;
    let got = l.deliveries();
    let last = got.iter().find(|d| d.1.contains("Lane K merged.")).unwrap_or_else(|| panic!("{got:?}")).1.clone();
    assert!(last.contains(t3::workers::CLOSED), "{last}");
    assert_eq!(l.calls("t3_thread_organize"), vec![json!({"threadId":parent,"action":"archive"})]);
    // A child in another project counts too; one that ends without waking its parent (cancelled) leaves no task open
    // for good: once it is done and the thread has been quiet a while, a pass archives the kept task.
    let other = l.delegate("Second pass").await;
    let lane = "thread:delegated-task:command%3Amcp%3Aint%3Adelegate-task%3Alane-m";
    l.sub_thread(lane, "Lane M", &other, "running", json!([]));
    l.with(lane, |t| t["projectId"] = json!("p-scratch"));
    l.item(&other, "assistant_message", "completed", "Waiting on lane M.", json!({}));
    l.end_run(&other, "completed");
    l.pass().await;
    let got = l.deliveries();
    let notice = got.iter().find(|d| d.1.contains("Waiting on lane M.")).unwrap_or_else(|| panic!("{got:?}")).1.clone();
    assert!(notice.contains("delegated task 'Lane M' still runs") && !notice.contains(t3::workers::CLOSED), "{notice}");
    l.set_run(lane, &format!("run:{lane}:1"), "cancelled");
    assert!(l.w.archive_cleared().await.unwrap().is_empty(), "not while its thread changed just now");
    l.with(&other, |t| t["updatedAt"] = json!(iso(-300)));
    assert_eq!(l.w.archive_cleared().await.unwrap(), vec!["Second pass".to_string()]);
    assert_eq!(l.calls("t3_thread_organize").last().unwrap(), &json!({"threadId":other,"action":"archive"}));
}

/// Proposal 5: delegate_task with no target starts Claude Opus 5.5 at medium effort on the account with quota left.
/// Personal edition only: the public edition takes the user's own providers (below).
#[cfg(not(feature = "public"))]
#[tokio::test]
async fn delegate_task_without_a_target_starts_opus_medium_on_the_account_with_quota() {
    use t3::workers::AccountsHook;
    use agent::accounts::{AccountQuota, Kind, Window};
    let l = Lab::new().await;
    let now = agent::now_ms();
    let acct = move |instance: &str, used: f64| AccountQuota { instance: instance.into(), kind: Kind::Claude, windows: vec![Window { name: "weekly".into(), used_percent: Some(used), resets_at_ms: Some(now + 86_400_000) }], reached: None, error: None };
    let hook: AccountsHook = Arc::new(move |_wait| Ok(vec![acct("claudeAgent", 100.0), acct("claudeAgent_2", 3.0)]));
    l.w.set_accounts(hook);
    l.s.second_account.store(true, std::sync::atomic::Ordering::Release);
    let started = l.call("delegate_task", json!({"task":"Go.","title":"Plain","path":l.dir.path()})).await.unwrap();
    let launch = l.calls("t3_thread_launch").last().unwrap().clone();
    assert_eq!(launch["modelSelection"], json!({"instanceId":"claudeAgent_2","model":"claude-opus-5-5","options":{"effort":"medium"}}), "{started}");
    assert_eq!(launch["runtimeMode"], "full-access");
    // An effort alone keeps the default model; a named model keeps its own options.
    l.call("delegate_task", json!({"task":"Go.","title":"Deep","target":{"options":{"effort":"high"}},"path":l.dir.path()})).await.unwrap();
    assert_eq!(l.calls("t3_thread_launch").last().unwrap()["modelSelection"]["options"], json!({"effort":"high"}));
    assert_eq!(t3::workers::default_target(&json!({"model":"claude-haiku-5-5"})), json!({"model":"claude-haiku-5-5"}));
}

/// The public edition's task default names no vendor: without a target model, the user's own T3 providers decide
/// (`workers::catalog_default`): the model T3 says the caller runs, else the first usable provider's default or first
/// model; disabled providers and those without full access never; target.providerInstanceId narrows it.
#[test]
fn the_task_default_comes_from_the_providers_t3_lists() {
    use t3::workers::catalog_default;
    let caps = capabilities_one();
    assert_eq!(catalog_default(&caps, None), Some(("claudeAgent".into(), "claude-haiku-5-5".into())), "the first usable provider's first model");
    assert_eq!(catalog_default(&caps, Some("codex")), Some(("codex".into(), "gpt-6-luna".into())));
    assert_eq!(catalog_default(&caps, Some("cursor")), None, "a disabled provider is never the default");
    let mut marked = caps.clone();
    marked["providers"][0]["models"][1]["isDefault"] = json!(true);
    assert_eq!(catalog_default(&marked, None), Some(("claudeAgent".into(), "claude-opus-5-5".into())), "T3's default model wins");
    let mut inherited = caps.clone();
    inherited["inheritedProviderInstanceId"] = json!("codex");
    inherited["inheritedModel"] = json!("gpt-6-luna");
    assert_eq!(catalog_default(&inherited, None), Some(("codex".into(), "gpt-6-luna".into())), "the model T3 says the caller runs");
    inherited["inheritedModel"] = json!("gone-model");
    assert_eq!(catalog_default(&inherited, None), Some(("claudeAgent".into(), "claude-haiku-5-5".into())), "an unlisted inherited model is skipped");
    let only_grok = json!({"providers":[
        {"providerInstanceId":"grok","driverKind":"grok","runtimeModes":["approval-required"],"constraints":[],"models":[{"id":"grok-4.7"}]},
        {"providerInstanceId":"local","driverKind":"pi","runtimeModes":["full-access"],"constraints":[],"models":[{"id":"default"}]}]});
    assert_eq!(catalog_default(&only_grok, None), Some(("local".into(), "default".into())), "no full access, no default");
    assert_eq!(catalog_default(&json!({"providers":[]}), None), None);
}

/// The public edition: delegate_task with no target starts the user's own provider's model with that model's own
/// default options (no effort forced), and says so; it never falls back to a named vendor.
#[cfg(feature = "public")]
#[tokio::test]
async fn the_public_edition_s_task_default_is_the_user_s_provider() {
    let l = Lab::new().await;
    let started = l.call("delegate_task", json!({"task":"Go.","title":"Plain","path":l.dir.path()})).await.unwrap();
    let launch = l.calls("t3_thread_launch").last().unwrap().clone();
    assert_eq!(launch["modelSelection"], json!({"instanceId":"claudeAgent","model":"claude-haiku-5-5"}), "{started}");
    assert!(started.contains("(claude-haiku-5-5)"), "{started}");
    // An effort alone is kept on the picked model; a provider alone narrows the pick.
    l.call("delegate_task", json!({"task":"Go.","title":"Deep","target":{"options":{"effort":"high"}},"path":l.dir.path()})).await.unwrap();
    assert_eq!(l.calls("t3_thread_launch").last().unwrap()["modelSelection"], json!({"instanceId":"claudeAgent","model":"claude-haiku-5-5","options":{"effort":"high"}}));
    l.call("delegate_task", json!({"task":"Go.","title":"Other","target":{"providerInstanceId":"codex"},"path":l.dir.path()})).await.unwrap();
    assert_eq!(l.calls("t3_thread_launch").last().unwrap()["modelSelection"], json!({"instanceId":"codex","model":"gpt-6-luna"}));
    assert_eq!(t3::workers::default_target(&json!(null)), json!({}), "no vendor is filled in");
}

/// T3_call's archive failures (3 and 4 Nov): unarchiving a thread that is not archived is not a failure.
#[tokio::test]
async fn unarchiving_a_thread_that_is_not_archived_is_not_a_failure() {
    let l = Lab::new().await;
    let t = l.delegate("Live task").await;
    let out = l.call("t3_call", json!({"tool":"t3_thread_organize","args":{"threadId":t,"action":"unarchive"}})).await.unwrap();
    assert_eq!(out, format!("Thread {t} is already unarchived; nothing to do."));
}
// ---- end lane S ----

// ---- crewmates (17:09–17:13; crates/t3/src/crew.rs) ----
impl Lab {
    /// The crew layer on this lab's workers, its store where the app keeps it (`<data>/agent/crew.sqlite`), with no
    /// waiting: a sub-job wake goes on the pass after the work ended, and every pass looks at each thread.
    fn crew(&self) -> Arc<t3::crew::Crew> {
        let crew = t3::crew::Crew::open(&self.dir.path().join("agent/crew.sqlite"), &self.w).unwrap();
        crew.set_timing(0, 0);
        // As the app does: a crewmate without a folder works in Heimdall's home folder.
        self.w.set_default_folder(self.dir.path());
        crew
    }
    async fn crew_pass(&self, crew: &t3::crew::Crew) {
        crew.pass(&self.p()).await.unwrap();
    }
    /// The thread a crew_add or crew_tell started, from its answer.
    fn crew_thread(&self, name: &str) -> String {
        self.w.crew().unwrap().store.get(name).unwrap().unwrap().thread.unwrap()
    }
    fn reply(&self, thread: &str, text: &str, status: &str) {
        self.item(thread, "assistant_message", "completed", text, json!({}));
        self.end_run(thread, status);
    }
    fn sent_to(&self, thread: &str) -> Vec<String> {
        self.calls("t3_thread_send").into_iter().filter(|c| c["threadId"] == thread).map(|c| c["message"].as_str().unwrap().to_owned()).collect()
    }
}

/// A crewmate is created by the agent in one call, its first message starts its thread with its standing intro, its
/// result comes back as a notice like a task's, and its thread never closes itself.
#[tokio::test]
async fn a_crewmate_is_created_told_and_never_closed() {
    let l = Lab::new().await;
    let crew = l.crew();
    let added = l.call("crew_add", json!({"name":"Hub","role":"runs the social hub","folder":l.dir.path(),"brief":"/nonexistent/WORKER.md","message":"Check the approval queue."})).await.unwrap();
    assert!(added.starts_with("Crewmate Hub added: runs the social hub · claude-opus-5-5 medium"), "{added}");
    assert!(added.contains("Told Hub: started its thread"), "{added}");
    let launch = l.calls("t3_thread_launch").pop().unwrap();
    assert!(launch["title"].as_str().unwrap().starts_with("Crew · Hub [heimdall:"), "{launch}");
    let brief = launch["message"].as_str().unwrap();
    assert!(brief.contains("You are Hub, one of") && brief.contains("Your role: runs the social hub"), "{brief}");
    assert!(brief.contains("/nonexistent/WORKER.md; it does not exist yet. Write it"), "{brief}");
    assert!(brief.contains("Never address") && brief.ends_with("The message:\nCheck the approval queue."), "{brief}");
    assert_eq!(launch["modelSelection"]["options"]["effort"], "medium");
    let thread = l.crew_thread("Hub");
    l.reply(&thread, "Queue checked: three drafts wait. Nothing needs User.", "completed");
    l.pass().await;
    l.crew_pass(&crew).await;
    let got = l.deliveries();
    let notice = got.iter().find(|d| d.0.starts_with(&format!("terminal:{thread}"))).unwrap_or_else(|| panic!("{got:?}"));
    assert!(notice.1.contains("Queue checked") && !notice.1.contains(t3::workers::CLOSED), "{}", notice.1);
    assert!(l.calls("t3_thread_organize").is_empty(), "a crewmate's thread is never archived by itself");
    // The thread's island closing does not archive it either.
    assert!(l.w.clear_task(&thread).unwrap());
    assert!(l.w.archive_cleared().await.unwrap().is_empty());
    let status = l.call("crew_status", json!({"name":"Hub"})).await.unwrap();
    assert!(status.starts_with("Hub · idle · runs the social hub"), "{status}");
    assert!(status.contains("from Heimdall: Check the approval queue.") && status.contains("result: completed: Queue checked"), "{status}");
    // The same name twice is refused; a second message goes into the same thread.
    assert!(l.call("crew_add", json!({"name":"hub","role":"x"})).await.unwrap_err().to_string().starts_with("CREW_EXISTS"));
    let told = l.call("crew_tell", json!({"name":"hub","message":"Post the first draft."})).await.unwrap();
    assert_eq!(told, "Told Hub: started a turn in its thread. Its result arrives as a notice.");
    assert_eq!(l.calls("t3_thread_launch").len(), 1);
}

/// The user, 17:13: nothing reaches a crewmate except through Heimdall's crew layer. The agent's own T3 tools refuse to
/// send into, stop, archive or reconfigure a crew thread; reading it stays free.
#[tokio::test]
async fn a_crew_thread_is_reached_only_through_the_crew() {
    let l = Lab::new().await;
    let _crew = l.crew();
    l.call("crew_add", json!({"name":"Trainer","role":"runs the training queue","message":"Start."})).await.unwrap();
    let thread = l.crew_thread("Trainer");
    let sends = l.calls("t3_thread_send").len();
    for (tool, args) in [
        ("t3_thread_send", json!({"threadId":thread,"message":"psst"})),
        ("task_cancel", json!({"taskId":thread})),
        ("t3_call", json!({"tool":"t3_thread_send","args":{"threadId":thread,"message":"psst"}})),
        ("t3_call", json!({"tool":"t3_thread_organize","args":{"threadId":thread,"action":"archive"}})),
        ("t3_call", json!({"tool":"t3_thread_interrupt","args":{"threadId":thread}})),
        ("t3_call", json!({"tool":"t3_thread_configure","args":{"threadId":thread,"modelSelection":{}}})),
        ("t3_call", json!({"tool":"t3_thread_organize","args":{"action":"archive"},"threads":["other",thread]})),
    ] {
        let e = l.call(tool, args.clone()).await.unwrap_err().to_string();
        assert!(e.starts_with(&format!("CREW_THREAD: {thread} is crewmate Trainer's thread")), "{tool} {args}: {e}");
    }
    assert_eq!(l.calls("t3_thread_send").len(), sends, "nothing was sent");
    assert!(l.calls("t3_thread_organize").is_empty() && l.calls("t3_thread_interrupt").is_empty());
    assert!(l.call("t3_thread_read", json!({"threadId":thread})).await.is_ok());
    assert!(l.call("t3_call", json!({"tool":"t3_queue","args":{"threadId":thread,"action":"list"}})).await.is_ok());
}

/// The user's 17:09 problem: a worker's thread archived itself and a local process could not message it. A crewmate's
/// archived thread is reopened by the message; a thread gone from T3 is replaced by a new one started from the
/// crewmate's brief and history, and the record follows. Each answer says which happened.
#[tokio::test]
async fn a_message_reaches_a_crewmate_whose_thread_is_archived_or_gone() {
    let l = Lab::new().await;
    let crew = l.crew();
    l.call("crew_add", json!({"name":"Library","role":"runs the Library's nightly import","message":"Import tonight's batch."})).await.unwrap();
    let first = l.crew_thread("Library");
    l.reply(&first, "Imported 41 books; two covers missing.", "completed");
    l.pass().await;
    l.with(&first, |t| t["archived"] = json!(true));
    let told = l.call("crew_tell", json!({"name":"Library","message":"Fetch the two missing covers."})).await.unwrap();
    assert_eq!(told, "Told Library: reopened its archived thread and started a turn. Its result arrives as a notice.");
    assert!(l.calls("t3_thread_organize").contains(&json!({"threadId":first,"action":"unarchive"})));
    assert!(l.sent_to(&first).last().unwrap().ends_with("Fetch the two missing covers."));
    l.reply(&first, "Both covers fetched.", "completed");
    l.pass().await;
    // Gone: deleted from T3.
    // Deleted from T3 (the fake numbers new threads by count, so the row stays under another id).
    l.with(&first, |t| t["threadId"] = json!("deleted"));
    let told = l.call("crew_tell", json!({"name":"Library","message":"Check the import log."})).await.unwrap();
    let second = l.crew_thread("Library");
    assert_ne!(second, first, "{told}");
    assert_eq!(told, format!("Told Library: its thread {first} was gone, so it started a new one ({second}) from its brief and history. Its result arrives as a notice."));
    let brief = l.calls("t3_thread_launch").pop().unwrap()["message"].as_str().unwrap().to_owned();
    assert!(brief.contains(&format!("your previous one ({first}) was lost")), "{brief}");
    assert!(brief.contains("Your last result") && brief.contains("Both covers fetched."), "{brief}");
    assert!(brief.contains("from Heimdall: Import tonight's batch.") && brief.ends_with("The message:\nCheck the import log."), "{brief}");
    // Gone between passes while idle: the record drops the thread, and the next message starts a new one.
    l.reply(&second, "Log clean.", "completed");
    l.pass().await;
    // Deleted from T3 (the fake numbers new threads by count, so the row stays under another id).
    l.with(&second, |t| t["threadId"] = json!("deleted"));
    l.crew_pass(&crew).await;
    assert_eq!(crew.store.get("Library").unwrap().unwrap().thread, None);
    let status = l.call("crew_status", json!({"name":"Library"})).await.unwrap();
    assert!(status.starts_with("Library · lost its thread") && status.contains(&format!("lost thread {second}: gone from T3")), "{status}");
}

/// T3 unreachable: the message waits in the crewmate's inbox and goes on a later pass, never lost, never doubled.
#[tokio::test]
async fn a_message_that_cannot_go_now_waits_in_the_inbox() {
    let l = Lab::new().await;
    let crew = l.crew();
    l.call("crew_add", json!({"name":"Feedback","role":"reads the feedback inbox","message":"Read today's feedback."})).await.unwrap();
    let thread = l.crew_thread("Feedback");
    l.reply(&thread, "Two reports.", "completed");
    l.pass().await;
    // T3 is restarting: every send and read fails.
    l.s.fail_sends.store(true, std::sync::atomic::Ordering::Release);
    let told = l.call("crew_tell", json!({"name":"Feedback","message":"Fix the first report."})).await.unwrap();
    assert!(told.starts_with("Queued for Feedback: it could not go now ("), "{told}");
    // Many failing passes: never given up, one notice after MAX_ATTEMPTS.
    for _ in 0..(t3::crew::MAX_ATTEMPTS + 3) {
        l.crew_pass(&crew).await;
    }
    assert_eq!(crew.store.pending().unwrap().len(), 1);
    let warned: Vec<_> = l.deliveries().into_iter().filter(|(id, _)| id.starts_with("crew:undelivered:")).collect();
    assert_eq!(warned.len(), 1, "{warned:?}");
    assert!(warned[0].1.contains("it stays in its inbox"), "{}", warned[0].1);
    // T3 is back: delivered once, then never again.
    l.s.fail_sends.store(false, std::sync::atomic::Ordering::Release);
    l.crew_pass(&crew).await;
    l.crew_pass(&crew).await;
    let sent = l.sent_to(&thread);
    assert_eq!(sent.iter().filter(|m| m.ends_with("Fix the first report.")).count(), 1, "{sent:?}");
    assert!(crew.store.pending().unwrap().is_empty());
}

/// A first start that failed is retried with the very same launch, even when the crewmate's history changed in
/// between (review: a rebuilt brief made every retry a REPLAY_CONFLICT).
#[tokio::test]
async fn a_failed_first_start_is_retried_with_the_same_launch() {
    let l = Lab::new().await;
    let crew = l.crew();
    l.s.fail_sends.store(true, std::sync::atomic::Ordering::Release);
    let added = l.call("crew_add", json!({"name":"Late","role":"starts while T3 restarts","message":"Begin."})).await.unwrap();
    assert!(added.contains("Queued for Late: it could not go now"), "{added}");
    l.call("crew_schedule", json!({"action":"add","name":"Late","when":"daily 07:00","message":"Morning run."})).await.unwrap();
    l.s.fail_sends.store(false, std::sync::atomic::Ordering::Release);
    l.crew_pass(&crew).await;
    assert!(crew.store.get("Late").unwrap().unwrap().thread.is_some(), "{:?}", l.calls("t3_thread_launch"));
    assert!(crew.store.pending().unwrap().is_empty());
}

/// A turn that waits on its own sub-agent inside the turn and ends once it is back is not woken again (review).
#[tokio::test]
async fn a_crewmate_whose_sub_agent_ended_within_its_turn_is_not_woken() {
    let l = Lab::new().await;
    let crew = l.crew();
    l.call("crew_add", json!({"name":"Quick","role":"x","message":"Go."})).await.unwrap();
    let thread = l.crew_thread("Quick");
    let agent = l.item(&thread, "subagent", "running", "", json!({"title":"Look it up","subagentId":"node:q"}));
    l.pass().await;
    l.crew_pass(&crew).await;
    l.set_item(&thread, &agent, "completed");
    l.reply(&thread, "Looked it up: done.", "completed");
    l.pass().await;
    for _ in 0..3 {
        l.crew_pass(&crew).await;
    }
    assert!(l.sent_to(&thread).is_empty(), "{:?}", l.sent_to(&thread));
}

/// Crew tools replayed under the same operation id answer as the first time: no second crewmate, schedule or retire.
#[tokio::test]
async fn crew_tools_replayed_with_the_same_operation_change_nothing_twice() {
    let l = Lab::new().await;
    let crew = l.crew();
    let p = l.p();
    let add = json!({"name":"Once","role":"x","message":"Hi."});
    let first = l.w.execute("crew_add", add.clone(), "op-add", &p).await.unwrap();
    assert_eq!(l.w.execute("crew_add", add, "op-add", &p).await.unwrap(), first);
    assert_eq!(l.calls("t3_thread_launch").len(), 1);
    let sched = json!({"action":"add","name":"Once","when":"daily 07:00","message":"m"});
    let one = l.w.execute("crew_schedule", sched.clone(), "op-s", &p).await.unwrap();
    assert_eq!(l.w.execute("crew_schedule", sched, "op-s", &p).await.unwrap(), one);
    assert_eq!(crew.store.all_schedules().unwrap().len(), 1);
    let r = l.w.execute("crew_retire", json!({"name":"Once"}), "op-r", &p).await.unwrap();
    assert_eq!(l.w.execute("crew_retire", json!({"name":"Once"}), "op-r", &p).await.unwrap(), r);
}

/// After a forced restart a crewmate's turn ends interrupted: the new Heimdall process resumes it in the same thread
/// by itself with one notice, the interruption itself wakes nothing, and a second failure is reported as usual.
#[tokio::test]
async fn a_crewmate_interrupted_by_a_restart_is_resumed_once_by_itself() {
    let mut l = Lab::new().await;
    let _crew = l.crew();
    l.call("crew_add", json!({"name":"Decider","role":"trains Decider v7","message":"Run the night training."})).await.unwrap();
    let thread = l.crew_thread("Decider");
    l.restart_process();
    let crew = l.crew();
    l.reply(&thread, "Epoch 3 of 8…", "interrupted");
    l.pass().await;
    l.crew_pass(&crew).await;
    let got = l.deliveries();
    assert!(!got.iter().any(|(id, _)| id.starts_with(&format!("terminal:{thread}"))), "the interruption is not news: {got:?}");
    let resumed: Vec<_> = got.iter().filter(|(id, _)| id.starts_with("crew:resumed:")).collect();
    assert_eq!(resumed.len(), 1, "{got:?}");
    assert!(resumed[0].1.starts_with("Crew: Decider's turn ended interrupted; Heimdall resumed it by itself (Told Decider: started a turn in its thread."), "{}", resumed[0].1);
    assert!(l.sent_to(&thread).last().unwrap().contains("your last turn ended interrupted before it finished"));
    l.crew_pass(&crew).await;
    assert_eq!(l.sent_to(&thread).len(), 1, "resumed once");
    // The resumed turn fails too: reported.
    l.reply(&thread, "CUDA? No: the MLX kernel crashed.", "failed");
    l.pass().await;
    l.crew_pass(&crew).await;
    assert!(l.deliveries().iter().any(|(id, t)| id.starts_with(&format!("terminal:{thread}")) && t.contains("the MLX kernel crashed")), "{:?}", l.deliveries());
    assert_eq!(l.sent_to(&thread).len(), 1);
}

/// "Tasks that said I'll report when the worker finishes and then never woke": a crewmate whose turn ended waiting on
/// its sub-agent is woken when that sub-agent has ended and no turn of its own started meanwhile.
#[tokio::test]
async fn a_crewmate_waiting_on_its_sub_job_is_woken_when_it_ends() {
    let l = Lab::new().await;
    let crew = l.crew();
    l.call("crew_add", json!({"name":"Taste","role":"improves the taste model","message":"Label the new batch."})).await.unwrap();
    let thread = l.crew_thread("Taste");
    let agent = l.item(&thread, "subagent", "running", "", json!({"title":"Label batch 12","subagentId":"node:label"}));
    l.reply(&thread, "Labelling runs in a sub-agent; I'll report when it finishes.", "completed");
    l.pass().await;
    l.crew_pass(&crew).await;
    assert_eq!(crew.state(&crew.store.get("Taste").unwrap().unwrap()), "waiting on its sub-jobs");
    assert!(l.sent_to(&thread).is_empty());
    // The sub-agent ends (a crash), and T3 starts no turn for it.
    l.set_item(&thread, &agent, "failed");
    l.pass().await;
    l.crew_pass(&crew).await; // the work ended: the grace starts
    l.crew_pass(&crew).await; // grace over: woken
    let sent = l.sent_to(&thread);
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert!(sent[0].contains("the work you were waiting on has ended (Label batch 12)"), "{}", sent[0]);
    l.crew_pass(&crew).await;
    assert_eq!(l.sent_to(&thread).len(), 1, "woken once");
}

/// A crewmate's turn that hits a usage limit moves to the account with quota as a task's does; the crew's record
/// follows the move, and the crew does not resume it a second time.
#[tokio::test]
async fn a_crewmate_moves_account_on_a_usage_limit_like_a_task() {
    use agent::accounts::{AccountQuota, Kind, Window};
    use t3::workers::AccountsHook;
    let l = Lab::new().await;
    let crew = l.crew();
    let now = agent::now_ms();
    let acct = |instance: &str, used: f64| AccountQuota { instance: instance.into(), kind: Kind::Claude, windows: vec![Window { name: "weekly".into(), used_percent: Some(used), resets_at_ms: Some(now + 3 * 86_400_000) }], reached: None, error: None };
    let reading = vec![acct("claudeAgent", 100.0), acct("claudeAgent_2", 0.0)];
    let hook: AccountsHook = Arc::new(move |_wait| Ok(reading.clone()));
    l.w.set_accounts(hook);
    l.call("crew_add", json!({"name":"Site","role":"keeps the Heimdall website","message":"Fix the hero."})).await.unwrap();
    let thread = l.crew_thread("Site");
    l.s.second_account.store(true, std::sync::atomic::Ordering::Release);
    l.reply(&thread, "You've hit your weekly limit · resets Nov 19 at 1pm (Europe/Warsaw)", "failed");
    l.pass().await;
    l.pass().await;
    l.crew_pass(&crew).await;
    assert_eq!(l.calls("t3_thread_configure").len(), 1);
    let sends = l.sent_to(&thread);
    assert_eq!(sends.len(), 1, "the account move's own resume only: {sends:?}");
    assert!(!l.deliveries().iter().any(|(id, _)| id.starts_with("crew:resumed:")));
    let status = l.call("crew_status", json!({"name":"Site"})).await.unwrap();
    assert!(status.contains("on claudeAgent_2") && status.contains("runs on claudeAgent_2 now"), "{status}");
}

/// The way in for the user's project tools (`heimdall --crew tell`): queued in the crew's store without waking the agent,
/// delivered on the next pass marked as a project tool's, recorded, and its result kept in the history for the digest
/// rather than sent to the agent, unless it asks something of the user. It can only reach an existing crewmate.
#[tokio::test]
async fn the_local_command_queues_for_a_crewmate_and_its_result_goes_to_the_digest() {
    let l = Lab::new().await;
    let crew = l.crew();
    l.call("crew_add", json!({"name":"Notes","role":"acts on User's notes on the hub","message":"Say hello."})).await.unwrap();
    let thread = l.crew_thread("Notes");
    l.reply(&thread, "Ready.", "completed");
    l.pass().await;
    let _ = l.call("crew_status", json!({})).await.unwrap(); // the agent looked
    let data = l.dir.path().to_owned();
    let note = data.join("note.txt");
    std::fs::write(&note, "User's note on draft 4: shorter, no emoji.").unwrap();
    let mut stdin = std::io::empty();
    let line = t3::crew::cli(&["tell".into(), "notes".into(), note.to_string_lossy().into(), "--id".into(), "n4".into()], || Ok(data.clone()), &mut stdin).unwrap();
    assert_eq!(line, "Queued for Notes as message 2; Heimdall delivers it on its next pass (within a minute while it runs) and keeps it in Notes's history.");
    // The same id again is the same message.
    t3::crew::cli(&["tell".into(), "Notes".into(), note.to_string_lossy().into(), "--id".into(), "n4".into()], || Ok(data.clone()), &mut stdin).unwrap();
    let before = l.deliveries().len();
    l.crew_pass(&crew).await;
    let sent = l.sent_to(&thread);
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert!(sent[0].contains("From one of the project's tools on the Mac (passed on by Heimdall):\nUser's note on draft 4"), "{}", sent[0]);
    l.reply(&thread, "Draft 4 shortened and saved. Nothing needs User.", "completed");
    l.pass().await;
    l.crew_pass(&crew).await;
    assert_eq!(l.deliveries().len(), before, "no wake for a routine result: {:?}", l.deliveries());
    let digest = l.call("crew_status", json!({})).await.unwrap();
    assert!(digest.contains("Notes · idle · acts on User's notes on the hub · claude-opus-5-5 medium · 1 in (1 routine), 1 results; last: Draft 4 shortened"), "{digest}");
    let again = l.call("crew_status", json!({})).await.unwrap();
    assert!(again.contains("· nothing"), "{again}");
    // A routine result that asks the user something reaches the agent.
    std::fs::write(&note, "User's note: post draft 5?").unwrap();
    t3::crew::cli(&["tell".into(), "Notes".into(), note.to_string_lossy().into()], || Ok(data.clone()), &mut stdin).unwrap();
    l.crew_pass(&crew).await;
    l.reply(&thread, "Draft 5 is ready.\n\nShould I post it? Default: hold.", "completed");
    l.pass().await;
    assert!(l.deliveries().iter().any(|(_, t)| t.contains("Should I post it?")), "{:?}", l.deliveries());
    // Only an existing crewmate; nothing but tell, list and the migration's import.
    let e = t3::crew::cli(&["tell".into(), "Nobody".into(), note.to_string_lossy().into()], || Ok(data.clone()), &mut stdin).unwrap_err().to_string();
    assert!(e.starts_with("NO_CREWMATE: no crewmate is called 'Nobody'"), "{e}");
    assert!(t3::crew::cli(&["add".into(), "X".into()], || Ok(data.clone()), &mut stdin).is_err());
    assert!(t3::crew::cli(&["list".into()], || Ok(data.clone()), &mut stdin).unwrap().starts_with("Notes · acts on"));
}

/// A crewmate's schedule fires straight into its thread without a turn of the agent; the firing and its result are
/// in the history. Retiring it ends its schedules and archives its thread; crew_add brings it back with its history.
#[tokio::test]
async fn a_crew_schedule_fires_into_its_thread_and_retiring_ends_it() {
    let l = Lab::new().await;
    let crew = l.crew();
    l.call("crew_add", json!({"name":"Weekly","role":"runs the weekly project cycle","message":"Introduce yourself."})).await.unwrap();
    let thread = l.crew_thread("Weekly");
    l.reply(&thread, "Hello.", "completed");
    l.pass().await;
    let added = l.call("crew_schedule", json!({"action":"add","name":"Weekly","when":"weekly sun 21:00","message":"Run the weekly project cycle."})).await.unwrap();
    assert!(added.starts_with("Schedule 1 for Weekly: weekly sun 21:00, first at Sun"), "{added}");
    assert!(l.call("crew_schedule", json!({})).await.unwrap().starts_with("1 · Weekly · weekly sun 21:00 · next Sun"));
    l.crew_pass(&crew).await;
    assert!(l.sent_to(&thread).is_empty(), "not due yet");
    crew.shift_schedules_for_tests(8 * 86_400_000);
    let before = l.deliveries().len();
    l.crew_pass(&crew).await;
    l.crew_pass(&crew).await;
    let sent = l.sent_to(&thread);
    assert_eq!(sent.len(), 1, "fired once: {sent:?}");
    assert!(sent[0].contains("Your schedule (passed on by Heimdall):\nRun the weekly project cycle."), "{}", sent[0]);
    assert_eq!(l.deliveries().len(), before);
    let status = l.call("crew_status", json!({"name":"Weekly"})).await.unwrap();
    assert!(status.contains("Schedule 1: weekly sun 21:00, next") && status.contains("schedule 1 (weekly sun 21:00) fired"), "{status}");
    let retired = l.call("crew_retire", json!({"name":"Weekly","reason":"User dropped it"})).await.unwrap();
    assert!(retired.starts_with("Weekly retired; its history stays"), "{retired}");
    assert!(l.calls("t3_thread_organize").contains(&json!({"threadId":thread,"action":"archive"})));
    assert!(l.call("crew_tell", json!({"name":"Weekly","message":"x"})).await.unwrap_err().to_string().starts_with("CREW_RETIRED"));
    assert_eq!(l.call("crew_schedule", json!({})).await.unwrap(), "No crew schedules.");
    let back = l.call("crew_add", json!({"name":"Weekly","role":"runs the weekly project cycle"})).await.unwrap();
    assert!(back.starts_with("Crewmate Weekly re-added with its history"), "{back}");
}

/// Re-adding (the user, 17:11): an existing T3 thread becomes a crewmate's body in one call, and a crewmate can be
/// pointed at another thread. The migration's import names threads that the next pass adopts.
#[tokio::test]
async fn an_existing_thread_is_adopted_as_a_crewmates_body() {
    let l = Lab::new().await;
    let crew = l.crew();
    l.owner_thread("t-old-worker", "Newsletter worker", 2);
    let added = l.call("crew_add", json!({"name":"Builder","role":"builds the hub","thread":"t-old-worker"})).await.unwrap();
    assert!(added.ends_with("Its body is thread t-old-worker."), "{added}");
    assert!(l.w.owns_task("t-old-worker").unwrap());
    l.pass().await;
    assert!(l.deliveries().is_empty(), "its old runs are not news: {:?}", l.deliveries());
    let told = l.call("crew_tell", json!({"name":"Builder","message":"Add the Sponsor button."})).await.unwrap();
    assert_eq!(told, "Told Builder: started a turn in its thread. Its result arrives as a notice.");
    l.owner_thread("t-other", "Hub v2", 1);
    let moved = l.call("crew_change", json!({"name":"Builder","thread":"t-other"})).await.unwrap();
    assert_eq!(moved, "Builder: changed thread.");
    assert_eq!(l.crew_thread("Builder"), "t-other");
    // The registry import: threads adopted on the next pass.
    let reg = l.dir.path().join("Assistant");
    std::fs::create_dir_all(&reg).unwrap();
    // A thread Heimdall started (as the registry's were), and one of the user's own, which the import may not take.
    let dec = l.delegate("Decider v7").await;
    l.owner_thread("t-owner", "User's own", 1);
    std::fs::write(reg.join("workers.md"), format!("## Active\n- Decider v7 · plan, data, trainer · {dec} · Opus 5.5 medium (claudeAgent) · Projects/Heimdall/lab/decider-v7/WORKER.md (to write) · 14 Nov\n- Grab · x · t-owner · Opus 5.5 · a/WORKER.md · 14 Nov\n")).unwrap();
    let data = l.dir.path().to_owned();
    let line = t3::crew::cli(&["import".into(), reg.join("workers.md").to_string_lossy().into()], || Ok(data.clone()), &mut std::io::empty()).unwrap();
    assert_eq!(line, "Imported 2 crewmates: Decider v7, Grab; Heimdall adopts their threads on its next pass.");
    l.crew_pass(&crew).await;
    assert!(crew.store.get("Decider v7").unwrap().unwrap().adopted);
    assert_eq!(crew.store.crewmate_of(&dec).unwrap().as_deref(), Some("Decider v7"));
    assert_eq!(crew.store.get("Grab").unwrap().unwrap().thread, None);
    assert!(!l.w.owns_task("t-owner").unwrap());
}
// ---- end crewmates ----

/// The app's watcher runs the crew's pass: a project tool's message queued while Heimdall runs is delivered by the
/// next control-plane pass, with nobody calling the crew.
#[tokio::test]
async fn the_watcher_delivers_a_queued_crew_message() {
    let l = Lab::new().await;
    let _crew = l.crew();
    l.call("crew_add", json!({"name":"Hub","role":"runs the social hub","message":"Start."})).await.unwrap();
    let thread = l.crew_thread("Hub");
    l.reply(&thread, "Started.", "completed");
    let data = l.dir.path().to_owned();
    let note = data.join("n.txt");
    std::fs::write(&note, "A note from the hub.").unwrap();
    t3::crew::cli(&["tell".into(), "Hub".into(), note.to_string_lossy().into()], || Ok(data.clone()), &mut std::io::empty()).unwrap();
    let (stop, task) = l.watcher();
    for _ in 0..100 {
        if l.sent_to(&thread).len() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    stop.send(true).unwrap();
    task.await.unwrap();
    assert!(l.sent_to(&thread)[0].ends_with("A note from the hub."), "{:?}", l.sent_to(&thread));
}
