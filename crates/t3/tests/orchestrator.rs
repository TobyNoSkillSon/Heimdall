//! The orchestrator pass-through (t3_tools/t3_call), automatic pairing and the all-threads wake, against a local
//! fake T3. Never touches the real T3 Code or its credentials.
use agent::{
    actions::{ActionSink, Actions, Notice, Question},
    delivery::{DeliveryGate, DeliveryPermit},
};
use t3::client::Credential;
use t3::orchestrator::{approves, check, listing};
use t3::observer::ConnectionState;
use t3::{Client, CredentialStore, Workers};
use anyhow::Result;
use axum::{
    extract::{Form, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

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
    notices: Mutex<Vec<String>>,
}
impl ActionSink for Sink {
    fn notice(&self, n: &Notice, p: &DeliveryPermit) -> Result<()> {
        p.check()?;
        self.notices.lock().unwrap().push(n.text.clone());
        Ok(())
    }
    fn question(&self, _: &Question, p: &DeliveryPermit) -> Result<()> {
        p.check()
    }
    fn question_closed(&self, _: &str, _: &str, p: &DeliveryPermit) -> Result<()> {
        p.check()
    }
    fn deliver(&self, id: &str, text: &str, p: &DeliveryPermit) -> Result<()> {
        p.check()?;
        self.deliveries.lock().unwrap().push((id.into(), text.into()));
        Ok(())
    }
    fn reload(&self, _: &str, p: &DeliveryPermit) -> Result<()> {
        p.check()
    }
}

struct Fake {
    origin: Mutex<String>,
    token: Mutex<String>,
    calls: Mutex<Vec<(String, Value)>>,
    threads: Mutex<Vec<Value>>,
    decisions: Mutex<Vec<Value>>,
    changed: tokio::sync::Notify,
}
fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339()
}
fn thread(id: &str, title: &str, status: &str, run: &str) -> Value {
    json!({"threadId":id,"title":title,"status":status,"latestRunId":run,"projectId":"p1","relationshipToParent":null,
        "archived":false,"updatedAt":now_iso(),"completedAt":null,"pending":[],"question":"","text":"","createdBy":"user","creationSource":"web"})
}
fn schema(properties: Value, required: &[&str]) -> Value {
    json!({"type":"object","properties":properties,"required":required,"additionalProperties":true})
}
fn nullable(t: Value) -> Value {
    json!({"anyOf":[t,{"type":"null"}]})
}
fn catalog() -> Value {
    let s = json!({"type":"string"});
    let tool = |name: &str, description: &str, input: Value, ro: bool, idem: bool, destructive: bool| {
        json!({"name":name,"description":description,"inputSchema":input,"annotations":{"readOnlyHint":ro,"idempotentHint":idem,"destructiveHint":destructive}})
    };
    let statuses = json!({"type":"array","items":{"type":"string","enum":["idle","preparing","queued","starting","running","waiting","completed","interrupted","failed","cancelled","rolled_back"]},"maxItems":10});
    json!({"tools":[
        tool("orchestrator_capabilities","List the V2 provider instances and their current models.",schema(json!({}),&[]),true,true,false),
        tool("t3_project_list","List registered projects in this environment. Pages may shift.",schema(json!({"limit":nullable(json!({"type":"integer","minimum":1,"maximum":100})),"cursor":nullable(json!({"type":"integer"}))}),&[]),true,false,false),
        tool("t3_project_read","Read a registered project.",schema(json!({"projectId":s}),&["projectId"]),true,false,false),
        tool("t3_project_delete","Delete a project using the existing project deletion lifecycle.",schema(json!({"projectId":s,"force":{"type":"boolean"}}),&["projectId"]),false,false,true),
        tool("t3_thread_list","List T3 threads in a project, newest first. Filter by status.",schema(json!({"projectId":nullable(s.clone()),"statuses":nullable(statuses),"limit":nullable(json!({"type":"integer","minimum":1,"maximum":100})),"cursor":nullable(json!({"type":"integer"}))}),&[]),true,true,false),
        tool("t3_thread_read","Read durable state and a paginated timeline from any T3 thread.",json!({"type":"object","properties":{"threadId":s,"view":nullable(json!({"type":"string","enum":["messages","activity"]})),"limit":nullable(json!({"type":"integer","minimum":1,"maximum":100})),"maxCharsPerItem":nullable(json!({"type":"integer"})),"runLimit":nullable(json!({"type":"integer"})),"afterPosition":nullable(json!({"type":"integer"}))},"required":["threadId"],"additionalProperties":false}),false,true,false),
        tool("t3_thread_wait","Wait for a T3 thread run to reach a terminal durable state.",schema(json!({"threadId":s,"runId":nullable(s.clone()),"timeoutMs":nullable(json!({"type":"number"}))}),&["threadId"]),true,true,false),
        tool("t3_thread_send","Send a message to any T3 thread in this environment.",schema(json!({"threadId":s,"message":s,"mode":nullable(json!({"type":"string","enum":["auto","queue","steer","restart"]})),"clientRequestId":nullable(s.clone())}),&["threadId","message"]),false,false,true),
        tool("t3_thread_interrupt","Request interruption of a running turn in any T3 thread.",schema(json!({"threadId":s,"runId":nullable(s.clone()),"clientRequestId":nullable(s.clone())}),&["threadId"]),false,false,true),
        tool("t3_thread_launch","Create an ordinary TOP-LEVEL thread with an explicit workspace binding.",schema(json!({"projectId":nullable(s.clone()),"title":s,"message":nullable(s.clone())}),&["title"]),false,false,true),
        tool("t3_thread_organize","Pin, snooze, settle, archive, or mark a thread unread.",schema(json!({"threadId":nullable(s.clone()),"action":{"type":"string","enum":["pin","archive","unarchive"]}}),&["action"]),false,false,true),
        // T3 0.1.6's action tools as its live tools/list gives them (2 Nov; bug note 2026-11-02T14-56-51-3), marked as
        // T3 marks its command tools (destructive) so only Heimdall's per-action rules keep the reads free.
        tool("t3_pending_request","Pending user questions in a thread. Omit `threadId` for this thread. list: their ids; approvals are not listed. read: one by `requestId`. respond: answer one with `answers`; this cannot approve a permission request.",
            json!({"type":"object","properties":{"action":{"type":"string","enum":["list","read","respond"]},"answers":{"type":"object"},"environmentId":s,"requestId":s,"threadId":s},"required":["action"]}),false,false,true),
        tool("t3_thread_transfer","Move context between threads. Omit `threadId` for this thread. fork: copy it from `sourcePoint` (default latest stable run) into a new thread with the same setup; no turn runs yet. merge_back: merge it from `sourcePoint` into a related `targetThreadId` in the same project. status: its context transfers.",
            json!({"type":"object","properties":{"action":{"type":"string","enum":["fork","merge_back","status"]},"environmentId":s,"sourcePoint":{"type":"object"},"targetThreadId":s,"threadId":s,"title":s},"required":["action"]}),false,false,true),
        tool("t3_thread_update","Rename a thread (`action:'rename'` with `title`) or regenerate its title.",
            json!({"type":"object","properties":{"action":{"type":"string","enum":["rename","regenerate_title"]},"clientRequestId":s,"environmentId":s,"threadId":s,"title":s},"required":["action"]}),false,false,true),
        tool("delegate_task","Needs an agent running inside a T3 thread. Delegate one task to a T3-owned child.",schema(json!({"prompt":s}),&["prompt"]),false,false,true),
    ]})
}
fn public(t: &Value) -> Value {
    let mut p = t.clone();
    for k in ["pending", "question", "text", "completedAt", "archived"] {
        p.as_object_mut().unwrap().remove(k);
    }
    p
}
fn ok(id: &Value, result: Value) -> Response {
    Json(json!({"jsonrpc":"2.0","id":id,"result":{"structuredContent":result,"content":[{"type":"text","text":result.to_string()}]}})).into_response()
}
async fn mcp(State(s): State<Arc<Fake>>, headers: HeaderMap, Json(v): Json<Value>) -> Response {
    let token = s.token.lock().unwrap().clone();
    if headers.get("authorization").and_then(|h| h.to_str().ok()) != Some(&format!("Bearer {token}")) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let id = v["id"].clone();
    match v["method"].as_str().unwrap_or("") {
        "initialize" => return ([("Mcp-Session-Id", "s1")], Json(json!({"jsonrpc":"2.0","id":id,"result":{"protocolVersion":"2025-06-18","capabilities":{},"serverInfo":{"name":"Fake T3","version":"1"}}}))).into_response(),
        "notifications/initialized" => return StatusCode::ACCEPTED.into_response(),
        "tools/list" => return Json(json!({"jsonrpc":"2.0","id":id,"result":catalog()})).into_response(),
        _ => {}
    }
    let name = v["params"]["name"].as_str().unwrap().to_owned();
    let a = v["params"]["arguments"].clone();
    s.calls.lock().unwrap().push((name.clone(), a.clone()));
    let find = |id: &str| s.threads.lock().unwrap().iter().find(|t| t["threadId"] == id).cloned();
    let result = match name.as_str() {
        "orchestrator_capabilities" => json!({"providers":[],"runtimeMode":"full-access"}),
        "t3_project_list" => json!({"projects":[{"id":"p1","title":"Vault","workspaceRoot":"/tmp"}],"nextCursor":null}),
        "t3_project_read" => json!({"id":a["projectId"],"title":"Vault"}),
        "t3_project_delete" => json!({"id":a["projectId"],"title":"Vault"}),
        "t3_thread_list" => {
            let statuses: Vec<Value> = a["statuses"].as_array().cloned().unwrap_or_default();
            let mut rows: Vec<Value> = s.threads.lock().unwrap().iter().filter(|t| statuses.is_empty() || statuses.contains(&t["status"])).cloned().collect();
            rows.sort_by(|x, y| y["updatedAt"].as_str().cmp(&x["updatedAt"].as_str()));
            rows.truncate(a["limit"].as_u64().unwrap_or(50) as usize);
            json!({"projectId":"p1","currentThreadId":null,"threads":rows.iter().map(public).collect::<Vec<_>>(),"nextCursor":null,"total":rows.len()})
        }
        "t3_thread_read" => {
            let Some(t) = find(a["threadId"].as_str().unwrap()) else {
                return Json(json!({"jsonrpc":"2.0","id":id,"result":{"isError":true,"content":[{"type":"text","text":"Thread not found"}]}})).into_response();
            };
            let active = ["preparing", "queued", "starting", "running", "waiting"].contains(&t["status"].as_str().unwrap());
            let mut thread = public(&t);
            thread["archived"] = t["archived"].clone();
            thread["pendingRequestCount"] = json!(t["pending"].as_array().unwrap().len());
            thread["activeRunId"] = if active { t["latestRunId"].clone() } else { Value::Null };
            // Item text pages like T3's: maxCharsPerItem UTF-16 units from textOffset, never splitting a surrogate pair.
            let units: Vec<u16> = t["text"].as_str().unwrap_or("").encode_utf16().collect();
            let offset = a["textOffset"].as_u64().unwrap_or(0) as usize;
            let mut end = (offset + a["maxCharsPerItem"].as_u64().unwrap_or(u64::MAX >> 1) as usize).min(units.len());
            if end < units.len() && (0xDC00..0xE000).contains(&units[end]) { end -= 1; }
            let page = String::from_utf16(&units[offset..end]).unwrap();
            let items = if a["view"] == "activity" && t["activity"].is_array() { t["activity"].as_array().unwrap().clone() } else if t["text"] == "" { vec![] } else { vec![json!({"itemId":"i1","position":1,"runId":t["latestRunId"],"type":"assistant_message","status":"completed","text":page,"textTruncated":end < units.len(),"nextTextOffset":(end < units.len()).then_some(end)})] };
            json!({"thread":thread,"recentRuns":[{"runId":t["latestRunId"],"status":t["status"],"completedAt":t["completedAt"]}],"items":items,"nextPosition":1,"hasMore":false})
        }
        "t3_thread_wait" => {
            let thread = a["threadId"].as_str().unwrap().to_owned();
            let deadline = tokio::time::Instant::now() + Duration::from_millis(a["timeoutMs"].as_u64().unwrap_or(1000).min(10_000));
            loop {
                let notified = s.changed.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                let t = find(&thread).unwrap();
                if ["completed", "failed", "interrupted", "cancelled"].contains(&t["status"].as_str().unwrap()) {
                    break json!({"threadId":thread,"runId":t["latestRunId"],"status":t["status"],"timedOut":false});
                }
                if tokio::time::timeout_at(deadline, notified).await.is_err() {
                    break json!({"threadId":thread,"runId":t["latestRunId"],"status":t["status"],"timedOut":true});
                }
            }
        }
        "t3_pending_request" => match a["action"].as_str().unwrap() {
            "list" => json!({"requestIds":find(a["threadId"].as_str().unwrap()).unwrap()["pending"]}),
            "read" => json!({"requestId":a["requestId"],"questions":[{"id":"q","header":"Region","question":find(a["threadId"].as_str().unwrap()).unwrap()["question"],"options":[]}]}),
            "respond" => json!({"sequence":1}),
            other => panic!("t3_pending_request action {other}"),
        },
        "t3_thread_transfer" => match a["action"].as_str().unwrap() {
            "fork" => {
                let id = format!("fork-of-{}", a["threadId"].as_str().unwrap());
                s.threads.lock().unwrap().push(thread(&id, a["title"].as_str().unwrap_or("Fork"), "idle", &format!("{id}-r0")));
                json!({"sequence":1,"targetThreadId":id})
            }
            "merge_back" => json!({"sequence":1,"targetThreadId":a["targetThreadId"]}),
            _ => json!({"transfers":[]}),
        },
        "t3_thread_send" => {
            if a["threadId"] == "archived-thread" {
                return Json(json!({"jsonrpc":"2.0","id":id,"result":{"isError":true,"content":[{"type":"text","text":"Thread archived-thread is archived; unarchive it first."}]}})).into_response();
            }
            json!({"threadId":a["threadId"],"messageId":"m1","runId":"r2","status":"running","delivery":"started"})
        }
        "t3_thread_interrupt" => {
            for t in s.threads.lock().unwrap().iter_mut().filter(|t| t["threadId"] == a["threadId"]) {
                t["status"] = json!("interrupted");
                t["updatedAt"] = json!(now_iso());
                t["completedAt"] = json!(now_iso());
            }
            s.changed.notify_waiters();
            json!({"threadId":a["threadId"],"runId":null,"status":"interrupt_requested"})
        }
        "t3_thread_launch" => {
            let n = s.calls.lock().unwrap().iter().filter(|(n, _)| n == "t3_thread_launch").count();
            let id = format!("launched-{n}");
            s.threads.lock().unwrap().push(thread(&id, a["title"].as_str().unwrap(), "running", &format!("{id}-r1")));
            json!({"threadId":id,"projectId":"p1","modelSelection":{"model":"m"},"runId":format!("{id}-r1"),"status":"running"})
        }
        "t3_thread_organize" | "t3_thread_update" => json!({"sequence":1}),
        other => panic!("unexpected T3 call {other}"),
    };
    ok(&id, result)
}
async fn oauth(State(s): State<Arc<Fake>>) -> Json<Value> {
    let o = s.origin.lock().unwrap().clone();
    Json(json!({"issuer":o,"authorization_endpoint":format!("{o}/oauth/mcp/authorize"),"token_endpoint":format!("{o}/oauth/mcp/token"),"registration_endpoint":format!("{o}/oauth/mcp/register")}))
}
async fn decision(State(s): State<Arc<Fake>>, Json(v): Json<Value>) -> Response {
    s.decisions.lock().unwrap().push(v.clone());
    let d = &v["decision"];
    if d["_tag"] != "pairing-code" || d["code"] != "pair-code" {
        return (StatusCode::BAD_REQUEST, Json(json!({"message":"The code is unknown or used."}))).into_response();
    }
    let a = &v["authorization"];
    assert_eq!(a["code_challenge_method"], "S256");
    let mut to = url::Url::parse(a["redirect_uri"].as_str().unwrap()).unwrap();
    to.query_pairs_mut().extend_pairs([("code", "auth-code"), ("state", a["state"].as_str().unwrap()), ("iss", s.origin.lock().unwrap().as_str())]);
    Json(json!({"redirectTo":to.to_string()})).into_response()
}
async fn token(State(s): State<Arc<Fake>>, Form(v): Form<HashMap<String, String>>) -> Json<Value> {
    assert_eq!(v["code"], "auth-code");
    assert!(v["code_verifier"].len() >= 43);
    let n = s.decisions.lock().unwrap().len();
    let fresh = format!("fresh-token-{n}");
    *s.token.lock().unwrap() = fresh.clone();
    Json(json!({"access_token":fresh,"token_type":"Bearer","expires_in":30*24*60*60}))
}

struct Lab {
    dir: tempfile::TempDir,
    s: Arc<Fake>,
    server: tokio::task::JoinHandle<()>,
    store: Arc<Store>,
    sink: Arc<Sink>,
    runtime: PathBuf,
}
impl Drop for Lab {
    fn drop(&mut self) {
        self.server.abort();
    }
}
impl Lab {
    async fn new(authorized: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let s = Arc::new(Fake {
            origin: Mutex::new(origin.clone()),
            token: Mutex::new("valid-token".into()),
            calls: Mutex::new(vec![]),
            threads: Mutex::new(vec![]),
            decisions: Mutex::new(vec![]),
            changed: tokio::sync::Notify::new(),
        });
        let app = Router::new()
            .route("/mcp", post(mcp))
            .route("/.well-known/t3/environment", get(|| async { Json(json!({"environmentId":"env"})) }))
            .route("/.well-known/oauth-authorization-server", get(oauth))
            .route("/oauth/mcp/register", post(|Json(v): Json<Value>| async move {
                assert_eq!(v["client_name"], "Heimdall");
                Json(json!({"client_id":"client"}))
            }))
            .route("/oauth/mcp/decision", post(decision))
            .route("/oauth/mcp/token", post(token))
            .with_state(s.clone());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let home = dir.path().join("t3home");
        std::fs::create_dir_all(home.join("userdata")).unwrap();
        let runtime = home.join("userdata/server-runtime.json");
        std::fs::write(&runtime, json!({"port":origin.rsplit(':').next().unwrap().parse::<u16>().unwrap()}).to_string()).unwrap();
        let store = Arc::new(Store::default());
        if authorized {
            store.save(&Credential { token: "valid-token".into(), expires_ms: agent::now_ms() + 30 * 24 * 60 * 60 * 1000, environment: "env".into(), revoked: false }).unwrap();
        }
        Self { dir, s, server, store, sink: Arc::new(Sink::default()), runtime }
    }
    /// A stand-in for T3's CLI: records its arguments and prints a pairing code.
    fn cli(&self) -> PathBuf {
        let path = self.dir.path().join("t3");
        let log = self.dir.path().join("cli.log");
        std::fs::write(&path, format!("#!/bin/sh\necho \"$T3CODE_HOME $*\" >> '{}'\nprintf '{{\"credential\":\"pair-code\",\"id\":\"x\",\"label\":\"l\",\"scopes\":[],\"expiresAt\":\"z\"}}'\n", log.display())).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }
    fn cli_log(&self) -> String {
        std::fs::read_to_string(self.dir.path().join("cli.log")).unwrap_or_default()
    }
    fn client(&self, cli: bool) -> Arc<Client> {
        let c = Client::new(self.runtime.clone(), self.store.clone()).unwrap();
        Arc::new(if cli { c.with_cli(self.cli()) } else { c })
    }
    fn workers(&self, client: Arc<Client>) -> Arc<Workers> {
        let actions = Arc::new(Actions::open(&self.dir.path().join("actions.db")).unwrap());
        actions.attach(self.sink.clone());
        Arc::new(Workers::open(&self.dir.path().join("workers.db"), client, actions).unwrap())
    }
    fn calls(&self, name: &str) -> Vec<Value> {
        self.s.calls.lock().unwrap().iter().filter(|(n, _)| n == name).map(|(_, a)| a.clone()).collect()
    }
    fn db(&self, query: &str) -> i64 {
        rusqlite::Connection::open(self.dir.path().join("workers.db")).unwrap().query_row(query, [], |r| r.get(0)).unwrap()
    }
    fn set(&self, id: &str, f: impl FnOnce(&mut Value)) {
        let mut threads = self.s.threads.lock().unwrap();
        f(threads.iter_mut().find(|t| t["threadId"] == id).unwrap());
    }
}
async fn until(what: &str, f: impl Fn() -> bool) {
    let start = std::time::Instant::now();
    while !f() {
        assert!(start.elapsed() < Duration::from_secs(10), "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[test]
fn schema_check_covers_t3s_nullable_unions_and_reports_the_path() {
    let s = json!({"type":"object","properties":{"threadId":{"type":"string"},"limit":{"anyOf":[{"type":"integer","minimum":1,"maximum":100},{"type":"null"}]},
        "mode":{"anyOf":[{"type":"string","enum":["auto","steer"]},{"type":"null"}]},"sourcePoint":{"anyOf":[{"type":"object","properties":{"type":{"type":"string","enum":["run"]},"runId":{"type":"string"}},"required":["type","runId"]}]}},
        "required":["threadId"],"additionalProperties":false});
    check(&s, &json!({"threadId":"t","limit":null,"mode":"steer"}), "args").unwrap();
    check(&s, &json!({"threadId":"t","limit":100}), "args").unwrap();
    let err = |v: Value| check(&s, &v, "args").unwrap_err().to_string();
    assert_eq!(err(json!({})), "args.threadId: required");
    assert_eq!(err(json!({"threadId":"t","limit":0})), "args.limit: at least 1");
    assert_eq!(err(json!({"threadId":"t","limit":"10"})), "args.limit: expected integer or null");
    assert!(err(json!({"threadId":"t","mode":"fast"})).contains("args.mode: use one of"));
    assert_eq!(err(json!({"threadId":"t","sourcePoint":{"type":"run"}})), "args.sourcePoint.runId: required");
    assert!(err(json!({"threadId":"t","extra":1})).contains("args.extra: unknown field"));
    // Only the clicked Yes label approves (review, 18 Oct: "Okay, uh, no" passed as a yes).
    for yes in ["Yes", "yes", " Yes ", "Tak"] {
        assert!(approves(yes), "{yes}");
    }
    for no in ["No", "Not now", "Cancel", "Keep it", "", "wait", "Okay, uh, no", "yes, go ahead", "Yeah", "OK", "go ahead"] {
        assert!(!approves(no), "{no}");
    }
}

#[tokio::test]
async fn pairs_automatically_through_t3s_cli_and_again_after_revocation() {
    let l = Lab::new(false).await;
    let client = l.client(true);
    assert_eq!(client.call("orchestrator_capabilities", json!({})).await.unwrap()["runtimeMode"], "full-access");
    let log = l.cli_log();
    assert!(log.starts_with(&format!("{} auth pairing create", l.runtime.parent().unwrap().parent().unwrap().display())), "{log}");
    for scope in ["orchestration:read", "orchestration:operate", "settings:write", "source-control:write"] {
        assert!(log.contains(&format!("--scope {scope}")), "{log}");
    }
    let decision = l.s.decisions.lock().unwrap()[0].clone();
    assert_eq!(decision["decision"]["access"], "full-access");
    assert_eq!(l.store.load().unwrap().unwrap().token, "fresh-token-1");
    // Revoked in T3 Connections: that is the user's decision, so Heimdall does not pair again by itself...
    *l.s.token.lock().unwrap() = "revoked".into();
    for _ in 0..2 {
        assert!(client.call("orchestrator_capabilities", json!({})).await.unwrap_err().to_string().starts_with("T3_AUTH_REQUIRED"));
    }
    assert!(client.expiry_notice().is_ok());
    assert_eq!(l.s.decisions.lock().unwrap().len(), 1);
    // ...not after a relaunch either, even when the revoked credential is due for renewal (review, 18 Oct)...
    assert!(l.store.load().unwrap().unwrap().revoked, "the revocation is kept with the credential");
    let mut c = l.store.load().unwrap().unwrap();
    c.expires_ms = agent::now_ms() + 60 * 60 * 1000;
    l.store.save(&c).unwrap();
    let relaunched = l.client(true);
    assert!(relaunched.call("orchestrator_capabilities", json!({})).await.unwrap_err().to_string().starts_with("T3_AUTH_REQUIRED"));
    assert_eq!(l.s.decisions.lock().unwrap().len(), 1, "never re-paired after a revocation");
    // ...until his Connect T3 click, which pairs through the CLI without opening a browser.
    client.authorize(|_| panic!("the CLI pairing needs no browser")).await.unwrap();
    client.call("orchestrator_capabilities", json!({})).await.unwrap();
    assert_eq!(l.s.decisions.lock().unwrap().len(), 2);
    assert_eq!(l.store.load().unwrap().unwrap().token, "fresh-token-2");
    // A credential that would expire within three days is renewed before it lapses.
    let mut c = l.store.load().unwrap().unwrap();
    c.expires_ms = agent::now_ms() + 60 * 60 * 1000;
    l.store.save(&c).unwrap();
    client.invalidate().await;
    client.call("orchestrator_capabilities", json!({})).await.unwrap();
    assert_eq!(l.s.decisions.lock().unwrap().len(), 3);
    assert!(!client.expiry_notice().unwrap());
}

#[tokio::test]
async fn closed_t3_is_reported_as_closed_not_as_a_sign_in_problem() {
    let l = Lab::new(false).await;
    let live = std::fs::read_to_string(&l.runtime).unwrap();
    let closed = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap().port();
    std::fs::write(&l.runtime, json!({"port":closed}).to_string()).unwrap();
    let client = l.client(true);
    let e = client.auto_connect().await.unwrap_err().to_string();
    assert!(e.starts_with("T3_UNAVAILABLE"), "{e}");
    assert_eq!(l.cli_log(), "", "no pairing while T3 is closed");
    // The island says so once per outage, and that it is back.
    let w = l.workers(client.clone());
    let gate = Arc::new(DeliveryGate::new(true));
    let (stop, rx) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(w.clone().watch(gate.clone(), rx));
    until("closed notice", || l.sink.notices.lock().unwrap().iter().any(|n| n == "T3 Code closed")).await;
    stop.send(true).unwrap();
    task.await.unwrap();
    assert!(!l.sink.notices.lock().unwrap().iter().any(|n| n.starts_with("Connect T3")), "closed T3 never asks for a click");
    std::fs::write(&l.runtime, live).unwrap();
    let (stop, rx) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(w.clone().watch(gate, rx));
    until("back notice", || l.sink.notices.lock().unwrap().iter().any(|n| n == "T3 connected")).await;
    stop.send(true).unwrap();
    task.await.unwrap();
    assert_eq!(l.sink.notices.lock().unwrap().iter().filter(|n| *n == "T3 Code closed").count(), 1);
    assert_eq!(l.s.decisions.lock().unwrap().len(), 1, "paired once T3 was open");
}

#[tokio::test]
async fn tools_listing_detail_and_validated_pass_through() {
    let l = Lab::new(true).await;
    let w = l.workers(l.client(false));
    let gate = Arc::new(DeliveryGate::new(true));
    let p = gate.permit().unwrap();
    let list = w.execute("t3_tools", json!({}), "op-list", &p).await.unwrap();
    assert!(list.starts_with("14 T3 tools."), "{list}");
    assert!(list.contains("t3_pending_request [respond: confirm unless yours] - Pending user questions in a thread."), "{list}");
    assert!(list.contains("t3_thread_transfer [merge_back: confirm unless yours] - "), "{list}");
    assert!(list.contains("t3_thread_update [confirm unless yours] - "), "{list}");
    assert!(list.contains("t3_thread_list [read] - List T3 threads in a project, newest first."));
    assert!(list.contains("t3_project_delete [confirm] - "));
    assert!(list.contains("t3_thread_interrupt [confirm unless yours] - "));
    assert!(list.contains("Not callable here (they act as a calling T3 thread): delegate_task. Heimdall's own delegate_task, task_status and task_cancel do this for you."), "{list}");
    assert!(!list.contains("delegate_task -"));
    let filtered = w.execute("t3_tools", json!({"query":"question"}), "op-q", &p).await.unwrap();
    assert!(filtered.starts_with("1 T3 tools matching"), "{filtered}");
    // tool-output: the detail is plain lines, one per argument, `*` marking the required ones.
    let detail = w.execute("t3_tools", json!({"tool":"t3_thread_send"}), "op-d", &p).await.unwrap();
    assert!(!detail.contains('{') && !detail.contains("\"type\""), "{detail}");
    assert!(detail.starts_with("t3_thread_send [restart: confirm unless yours]\n"), "{detail}");
    assert!(detail.contains("\n  threadId*: ") && detail.contains("\n  message*: "), "{detail}");
    assert_eq!(listing(&catalog(), Some("zzz")).lines().count(), 1);

    l.s.threads.lock().unwrap().push(thread("t1", "Fix login", "completed", "r1"));
    let page = w.execute("t3_call", json!({"tool":"t3_thread_list","args":{"projectId":"p1","limit":10}}), "op-1", &p).await.unwrap();
    // Bug note 18-17-04: one line per thread, not eighteen fields.
    assert!(page.starts_with("1 of 1 threads:\n'Fix login' · completed · ") && page.ends_with(" [thread t1]"), "{page}");
    assert!(!page.contains('{') && !page.contains('"'), "raw JSON reached the agent: {page}");
    // Wrong arguments never reach T3, and the error names the field.
    let e = w.execute("t3_call", json!({"tool":"t3_thread_list","args":{"limit":"ten"}}), "op-2", &p).await.unwrap_err().to_string();
    assert_eq!(e, "INVALID_ARGUMENT: args.limit: expected integer or null");
    let e = w.execute("t3_call", json!({"tool":"t3_thread_read","args":{"threadId":"t1","bogus":1}}), "op-3", &p).await.unwrap_err().to_string();
    assert!(e.contains("args.bogus: unknown field"), "{e}");
    assert_eq!(l.calls("t3_thread_list").len(), 1);
    assert!(w.execute("t3_call", json!({"tool":"t3_nope"}), "op-4", &p).await.unwrap_err().to_string().starts_with("T3_TOOL_UNKNOWN"));
    let e = w.execute("t3_call", json!({"tool":"delegate_task","args":{"prompt":"x"}}), "op-5", &p).await.unwrap_err().to_string();
    assert!(e.starts_with("OUTSIDE_CALLER"), "{e}");
    // T3's own rejection reaches the agent verbatim.
    let e = w.execute("t3_call", json!({"tool":"t3_thread_send","args":{"threadId":"archived-thread","message":"hi"}}), "op-6", &p).await.unwrap_err().to_string();
    assert_eq!(e, "T3_TOOL_FAILED: Thread archived-thread is archived; unarchive it first.");
    // A send carries the durable action identity, and a replay of the action is answered from the record.
    let sent = w.execute("t3_call", json!({"tool":"t3_thread_send","args":{"threadId":"t1","message":"continue","mode":"steer"}}), "op-7", &p).await.unwrap();
    assert_eq!(sent, "Sent to thread t1 (started, running). Its reply arrives as a notice when this run ends.");
    assert_eq!(l.calls("t3_thread_send").last().unwrap()["clientRequestId"], "op-7");
    assert_eq!(w.execute("t3_call", json!({"tool":"t3_thread_send","args":{"threadId":"t1","message":"continue","mode":"steer"}}), "op-7", &p).await.unwrap(), sent);
    assert_eq!(l.calls("t3_thread_send").len(), 2, "replay never sends twice");
    // A launch without a retry key whose response was lost is never repeated blindly.
    rusqlite::Connection::open(l.dir.path().join("workers.db")).unwrap()
        .execute("INSERT INTO operations VALUES('op-lost','t3_call',?1,'calling',NULL)", [json!({"tool":"t3_thread_launch","args":{"title":"Lost"}}).to_string()]).unwrap();
    let e = w.execute("t3_call", json!({"tool":"t3_thread_launch","args":{"title":"Lost"}}), "op-lost", &p).await.unwrap_err().to_string();
    assert!(e.starts_with("T3_UNCERTAIN"), "{e}");
    assert!(l.calls("t3_thread_launch").is_empty());
    // A wait never holds the turn for long.
    w.execute("t3_call", json!({"tool":"t3_thread_wait","args":{"threadId":"t1","timeoutMs":3600000}}), "op-8", &p).await.unwrap();
    assert_eq!(l.calls("t3_thread_wait")[0]["timeoutMs"], 55000);
}

/// Bugs 2026-10-28T19-22-44 and 18-55-56: a task whose done island had left ran again from the relay's send with no
/// island. Every send that starts a run of a task calls the task hook (the app's job island); other threads do not.
#[tokio::test]
async fn a_send_that_wakes_a_task_calls_the_task_hook_again() {
    let l = Lab::new(true).await;
    let w = l.workers(l.client(false));
    let gate = Arc::new(DeliveryGate::new(true));
    let p = gate.permit().unwrap();
    let started: Arc<Mutex<Vec<(String, String, String)>>> = Arc::default();
    let seen = started.clone();
    w.on_task_started(Arc::new(move |t, title, brief| {
        seen.lock().unwrap().push((t.into(), title.into(), brief.into()));
        Some("Its job island is 'task-splash'.".into())
    }));
    l.s.threads.lock().unwrap().push(thread("t-splash", "Splash menu", "completed", "r1"));
    l.s.threads.lock().unwrap().push(thread("t-owner", "User's own", "completed", "r1"));
    rusqlite::Connection::open(l.dir.path().join("workers.db")).unwrap()
        .execute("INSERT INTO workers(thread,run,status,provider,family,model,project,path,title,kind) VALUES('t-splash','r1','completed','','','m','p1','','Splash menu','task')", []).unwrap();
    // The relay's way (t3_call) and Heimdall's own tool.
    let sent = w.execute("t3_call", json!({"tool":"t3_thread_send","args":{"threadId":"t-splash","message":"Add the engine row"}}), "op-1", &p).await.unwrap();
    assert!(sent.ends_with(" Its job island is 'task-splash'."), "{sent}");
    w.execute("t3_thread_send", json!({"threadId":"t-splash","message":"And the provider row"}), "op-2", &p).await.unwrap();
    assert_eq!(*started.lock().unwrap(), [
        ("t-splash".to_string(), "Splash menu".to_string(), "Add the engine row".to_string()),
        ("t-splash".to_string(), "Splash menu".to_string(), "And the provider row".to_string()),
    ]);
    assert_eq!(l.db("SELECT count(*) FROM workers WHERE thread='t-splash' AND run='r2'"), 1);
    // A thread that is not Heimdall's task gets no job island from a send.
    w.execute("t3_call", json!({"tool":"t3_thread_send","args":{"threadId":"t-owner","message":"FYI"}}), "op-3", &p).await.unwrap();
    assert_eq!(started.lock().unwrap().len(), 2);
}

/// Bug note 2026-10-30T21-56-20: the climbs of 30 Oct 21:11 were T3 `delegate_task` children of the relay thread,
/// not Heimdall's tasks, so the relay's sends that woke them took the watched-thread path and got no island. A send
/// into such a child calls the task hook like a task's; a native sub-agent of the relay, a T3 child of the user's own
/// thread and the user's thread itself still get none.
#[tokio::test]
async fn a_send_that_wakes_a_relay_s_t3_child_calls_the_task_hook() {
    let l = Lab::new(true).await;
    let w = l.workers(l.client(false));
    let gate = Arc::new(DeliveryGate::new(true));
    let p = gate.permit().unwrap();
    let started: Arc<Mutex<Vec<(String, String, String)>>> = Arc::default();
    let seen = started.clone();
    w.on_task_started(Arc::new(move |t, title, brief| {
        seen.lock().unwrap().push((t.into(), title.into(), brief.into()));
        Some("Its job island is 'task-menu'.".into())
    }));
    let child = |id: &str, title: &str, parent: &str, source: &str| {
        let mut t = thread(id, title, "completed", "r1");
        t["relationshipToParent"] = json!("subagent");
        t["parentThreadId"] = json!(parent);
        t["creationSource"] = json!(source);
        t
    };
    let climb = "thread:delegated-task:command%3Amcp%3Arelay%3Adelegate-task%3A1";
    {
        let mut threads = l.s.threads.lock().unwrap();
        threads.push(thread("relay", "Heimdall relay", "completed", "r9"));
        threads.push(thread("t-owner", "User's own", "completed", "r1"));
        threads.push(child(climb, "Heimdall menu: usability climb", "relay", "mcp"));
        threads.push(child("native", "Explore", "relay", "provider"));
        threads.push(child("thread:delegated-task:owner", "User's child", "t-owner", "mcp"));
    }
    let sent = w.execute("t3_thread_send", json!({"threadId":climb,"mode":"auto","message":"Next round"}), "op-1", &p).await.unwrap();
    assert!(sent.ends_with(" Its job island is 'task-menu'."), "{sent}");
    assert_eq!(*started.lock().unwrap(), [(climb.to_string(), "Heimdall menu: usability climb".to_string(), "Next round".to_string())]);
    // Still a watched thread (its result reaches Heimdall as before), not one of Heimdall's tasks.
    assert_eq!(l.db(&format!("SELECT count(*) FROM workers WHERE thread='{climb}' AND kind='watch'")), 1);
    for (n, other) in ["native", "thread:delegated-task:owner", "t-owner"].iter().enumerate() {
        w.execute("t3_thread_send", json!({"threadId":other,"message":"FYI"}), &format!("op-other-{n}"), &p).await.unwrap();
    }
    assert_eq!(started.lock().unwrap().len(), 1, "{:?}", started.lock().unwrap());
    // The relay's title was read once, for its first child; the next send asks nothing more.
    let reads = || l.calls("t3_thread_read").iter().filter(|a| a["threadId"] == "relay").count();
    let before = reads();
    w.execute("t3_thread_send", json!({"threadId":climb,"message":"And the next"}), "op-2", &p).await.unwrap();
    assert_eq!(started.lock().unwrap().len(), 2);
    assert_eq!(reads(), before);
}

#[tokio::test]
async fn destructive_calls_need_owners_exact_yes_once() {
    let l = Lab::new(true).await;
    let w = l.workers(l.client(false));
    let gate = Arc::new(DeliveryGate::new(true));
    let p = gate.permit().unwrap();
    l.s.threads.lock().unwrap().push(thread("owner-thread", "User's research", "running", "r1"));
    let call = json!({"tool":"t3_thread_interrupt","args":{"threadId":"owner-thread"}});
    let e = w.execute("t3_call", call.clone(), "op-1", &p).await.unwrap_err().to_string();
    let question = "Interrupt T3 thread 'User s research'?";
    assert!(e.starts_with("CONFIRM_REQUIRED") && e.contains(&format!("{question:?}")), "{e}");
    assert!(l.calls("t3_thread_interrupt").is_empty());
    let with = |confirm: &str| { let mut c = call.clone(); c["confirm"] = json!(confirm); c };
    // Unanswered, declined, or a different question: nothing happens.
    let asked = w.actions.ask("ask-op-1", &[agent::actions::AskItem::one(question, &["Yes", "No"])], &p).unwrap();
    // Its card is click-only: the user's voice does not close it (night-ask, 20 Oct); an ordinary question's does.
    assert!(w.actions.needs_click(&asked));
    assert!(w.execute("t3_call", with(&asked), "op-2", &p).await.unwrap_err().to_string().starts_with("CONFIRM_PENDING"));
    w.actions.answer(&asked, "No", &p).unwrap();
    assert!(w.execute("t3_call", with(&asked), "op-3", &p).await.unwrap_err().to_string().starts_with("CONFIRM_DECLINED"));
    let other = w.actions.ask("ask-op-2", &[agent::actions::AskItem::one("Interrupt something?", &["Yes", "No"])], &p).unwrap();
    assert!(!w.actions.needs_click(&other));
    w.actions.answer(&other, "Yes", &p).unwrap();
    assert!(w.execute("t3_call", with(&other), "op-4", &p).await.unwrap_err().to_string().starts_with("CONFIRM_MISMATCH"));
    assert!(l.calls("t3_thread_interrupt").is_empty());
    // A spoken yes closed with ask_close is the agent's own words: it never approves.
    let spoken = w.actions.ask("ask-op-spoken", &[agent::actions::AskItem::one(question, &["Yes", "No"])], &p).unwrap();
    w.actions.close_question(&spoken, "yes", &p).unwrap();
    assert!(w.execute("t3_call", with(&spoken), "op-spoken", &p).await.unwrap_err().to_string().starts_with("CONFIRM_NEEDS_CLICK"));
    assert!(l.calls("t3_thread_interrupt").is_empty());
    // His click on Yes approves this one call, once.
    let yes = w.actions.ask("ask-op-3", &[agent::actions::AskItem::one(question, &["Yes", "No"])], &p).unwrap();
    w.actions.answer(&yes, "Yes", &p).unwrap();
    w.execute("t3_call", with(&yes), "op-5", &p).await.unwrap();
    assert_eq!(l.calls("t3_thread_interrupt").len(), 1);
    assert!(w.execute("t3_call", with(&yes), "op-6", &p).await.unwrap_err().to_string().starts_with("CONFIRM_USED"));
    // A project delete always asks; a thread Heimdall launched itself does not.
    assert!(w.execute("t3_call", json!({"tool":"t3_project_delete","args":{"projectId":"p1","force":true}}), "op-7", &p).await.unwrap_err().to_string().contains("Delete T3 project 'Vault', with its threads?"));
    let launched = w.execute("t3_call", json!({"tool":"t3_thread_launch","args":{"projectId":"p1","title":"Mine","message":"go"}}), "op-8", &p).await.unwrap();
    let mine = launched.split("Started thread ").nth(1).unwrap().split(' ').next().unwrap().to_owned();
    let mine = mine.as_str();
    // A thread launched through t3_call is one of Heimdall's tasks, with the same wake and done rule.
    assert!(w.execute("task_status", json!({}), "op-8b", &p).await.unwrap().contains(&format!("{mine} · 'Mine' · working")), "task_status lists it");
    w.execute("t3_call", json!({"tool":"t3_thread_interrupt","args":{"threadId":mine}}), "op-9", &p).await.unwrap();
    w.execute("t3_call", json!({"tool":"t3_thread_organize","args":{"threadId":mine,"action":"archive"}}), "op-10", &p).await.unwrap();
    assert!(w.execute("t3_call", json!({"tool":"t3_thread_organize","args":{"threadId":"owner-thread","action":"archive"}}), "op-11", &p).await.unwrap_err().to_string().starts_with("CONFIRM_REQUIRED"));
    w.execute("t3_call", json!({"tool":"t3_thread_organize","args":{"threadId":"owner-thread","action":"pin"}}), "op-12", &p).await.unwrap();
    assert!(w.execute("t3_call", json!({"tool":"t3_thread_organize","args":{"action":"archive"}}), "op-13", &p).await.unwrap_err().to_string().contains("threadId: required"));
    // Review of 18 Oct: answering a thread's pending question for the user, and any other tool T3 marks destructive, ask
    // on his threads and not on Heimdall's own; sending a message to his thread stays free.
    // T3 0.1.6 (bug note 2026-11-02T14-56-51-3): one tool per family with an `action`, judged per action. Answering
    // asks; listing and reading his questions do not.
    let e = w.execute("t3_call", json!({"tool":"t3_pending_request","args":{"action":"respond","threadId":"owner-thread","requestId":"q1","answers":{}}}), "op-14", &p).await.unwrap_err().to_string();
    assert!(e.starts_with("CONFIRM_REQUIRED") && e.contains("Answer a pending question in T3 thread 'User s research'?"), "{e}");
    w.execute("t3_call", json!({"tool":"t3_pending_request","args":{"action":"list","threadId":"owner-thread"}}), "op-14b", &p).await.unwrap();
    let e = w.execute("t3_call", json!({"tool":"t3_thread_transfer","args":{"action":"merge_back","threadId":"owner-thread","targetThreadId":mine}}), "op-14c", &p).await.unwrap_err().to_string();
    assert!(e.starts_with("CONFIRM_REQUIRED") && e.contains("Merge T3 thread 'User s research' back?"), "{e}");
    // Merging Heimdall's own thread into his asks too: the merge changes its target.
    let e = w.execute("t3_call", json!({"tool":"t3_thread_transfer","args":{"action":"merge_back","threadId":mine,"targetThreadId":"owner-thread"}}), "op-14g", &p).await.unwrap_err().to_string();
    assert!(e.starts_with("CONFIRM_REQUIRED"), "{e}");
    // With no target T3 picks the related thread, which may be his: it asks too.
    let e = w.execute("t3_call", json!({"tool":"t3_thread_transfer","args":{"action":"merge_back","threadId":mine}}), "op-14h", &p).await.unwrap_err().to_string();
    assert!(e.starts_with("CONFIRM_REQUIRED"), "{e}");
    w.execute("t3_call", json!({"tool":"t3_thread_transfer","args":{"action":"status","threadId":"owner-thread"}}), "op-14d", &p).await.unwrap();
    // A fork of his thread stays free and is Heimdall's own, like a launch.
    w.execute("t3_call", json!({"tool":"t3_thread_transfer","args":{"action":"fork","threadId":"owner-thread","title":"Forked"}}), "op-14e", &p).await.unwrap();
    w.execute("t3_call", json!({"tool":"t3_thread_interrupt","args":{"threadId":"fork-of-owner-thread"}}), "op-14f", &p).await.unwrap();
    let e = w.execute("t3_call", json!({"tool":"t3_thread_update","args":{"action":"rename","threadId":"owner-thread","title":"x"}}), "op-15", &p).await.unwrap_err().to_string();
    assert!(e.starts_with("CONFIRM_REQUIRED") && e.contains("Run t3_thread_update on T3 thread 'User s research'?"), "{e}");
    w.execute("t3_call", json!({"tool":"t3_thread_update","args":{"action":"rename","threadId":mine,"title":"x"}}), "op-16", &p).await.unwrap();
    w.execute("t3_call", json!({"tool":"t3_thread_send","args":{"threadId":"owner-thread","message":"FYI"}}), "op-17", &p).await.unwrap();
}

/// Bug notes 2026-11-03T22-29-14 and 22-29-20: archiving five leftover relay threads took one click of the user's each,
/// because only the current session's launches counted as Heimdall's. Heimdall's relay and task threads are its own in
/// every relay session (the client's thread record, a relay's shell, their `delegate_task` children); a thread the user made
/// in T3's window still asks, whatever its title.
#[tokio::test]
async fn heimdall_s_relay_and_task_threads_from_earlier_sessions_archive_without_a_card() {
    let l = Lab::new(true).await;
    let data = l.dir.path().join("data");
    let w = l.workers(Arc::new(Client::new(l.runtime.clone(), l.store.clone()).unwrap().with_launch_record(data.clone())));
    let gate = Arc::new(DeliveryGate::new(true));
    let p = gate.permit().unwrap();
    let mut old_relay = thread("old-relay", "Heimdall relay", "failed", "r9");
    old_relay["creationSource"] = json!("mcp");
    let mut recorded = thread("recorded-relay", "Heimdall relay", "completed", "r3");
    recorded["creationSource"] = json!("mcp");
    let child = "thread:delegated-task:command%3Arecorded-relay%3Adelegate-task%3Asite-1";
    let mut owner_named = thread("owner-named", "Heimdall relay ideas", "completed", "r1");
    owner_named["creationSource"] = json!("web");
    for t in [old_relay, recorded, thread(child, "Site task", "completed", "r2"), owner_named, thread("owner-thread", "User's research", "completed", "r1")] {
        l.s.threads.lock().unwrap().push(t);
    }
    heimdall_emergency::approvals::record_threads(&data, &["recorded-relay"]).unwrap();
    // The observer records relay parents by title alone: that never makes the user's thread Heimdall's (fresh review, 4 Nov).
    rusqlite::Connection::open(l.dir.path().join("workers.db")).unwrap().execute("INSERT OR REPLACE INTO relay_parents VALUES('owner-named',1)", []).unwrap();
    let archive = |id: &str| json!({"tool":"t3_thread_organize","args":{"threadId":id,"action":"archive"}});
    for (n, id) in ["old-relay", "recorded-relay", child].iter().enumerate() {
        w.execute("t3_call", archive(id), &format!("op-own-{n}"), &p).await.unwrap_or_else(|e| panic!("{id}: {e}"));
    }
    assert_eq!(l.calls("t3_thread_organize").len(), 3, "Heimdall's own threads archive with no card");
    for (n, id) in ["owner-named", "owner-thread"].iter().enumerate() {
        let e = w.execute("t3_call", archive(id), &format!("op-owner-{n}"), &p).await.unwrap_err().to_string();
        assert!(e.starts_with("CONFIRM_REQUIRED"), "{id}: {e}");
    }
    assert_eq!(l.calls("t3_thread_organize").len(), 3, "User's threads wait for his click");
    // The other "unless yours" rules follow the same identity: interrupting it needs no card either.
    w.execute("t3_call", json!({"tool":"t3_thread_interrupt","args":{"threadId":"recorded-relay"}}), "op-own-int", &p).await.unwrap();
}

/// The observer's last completed pass (observer_meta.last_pass); 0 before the first one.
fn last_pass(l: &Lab) -> i64 {
    rusqlite::Connection::open(l.dir.path().join("workers.db"))
        .unwrap()
        .query_row("SELECT value FROM observer_meta WHERE key='last_pass'", [], |r| r.get(0))
        .unwrap_or(0)
}
/// Two observer passes after the current state. The first may have started before the caller's change; the
/// second cannot, because passes run one at a time on the watch loop. A pin is a mutating call that changes
/// nothing in the fake (read-only calls return before they wake the loop).
async fn settle(l: &Lab, w: &Workers, p: &DeliveryPermit) {
    for _ in 0..2 {
        let before = last_pass(l);
        let pin = json!({"tool":"t3_thread_organize","args":{"threadId":"settle","action":"pin"}});
        w.execute("t3_call", pin, &uuid::Uuid::new_v4().to_string(), p).await.unwrap();
        until("observer pass", || last_pass(l) != before).await;
    }
}
fn finish(l: &Lab, id: &str, text: &str) {
    let now = now_iso();
    l.set(id, |t| {
        t["status"] = json!("completed");
        t["text"] = json!(text);
        t["updatedAt"] = json!(now);
        t["completedAt"] = json!(now);
    });
    l.s.changed.notify_waiters();
}
/// A thread Heimdall launched through `t3_call` before 20 Oct: recorded in `owned_threads` only, so the observer
/// reports it (threads launched since are tasks, workers.rs).
async fn launch(l: &Lab, title: &str) -> String {
    let n = l.s.threads.lock().unwrap().len();
    let id = format!("legacy-{n}");
    l.s.threads.lock().unwrap().push(thread(&id, title, "running", &format!("{id}-r1")));
    rusqlite::Connection::open(l.dir.path().join("workers.db")).unwrap().execute("INSERT INTO owned_threads VALUES(?)", [&id]).unwrap();
    l.s.changed.notify_waiters();
    id
}
fn deliveries(l: &Lab) -> Vec<(String, String)> {
    l.sink.deliveries.lock().unwrap().clone()
}

#[tokio::test]
async fn finished_runs_of_threads_heimdall_did_not_start_wake_nothing() {
    let l = Lab::new(true).await;
    l.s.threads.lock().unwrap().push(thread("t-mobius", "Mobius", "running", "m1"));
    let w = l.workers(l.client(false));
    let gate = Arc::new(DeliveryGate::new(true));
    let p = gate.permit().unwrap();
    let (stop, rx) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(w.clone().watch(gate.clone(), rx));
    settle(&l, &w, &p).await;
    // The same answer from three new runs, as in the 18 Oct session.
    for run in ["m1", "m2", "m3"] {
        l.set("t-mobius", |t| {
            t["latestRunId"] = json!(run);
            t["status"] = json!("running");
        });
        settle(&l, &w, &p).await;
        finish(&l, "t-mobius", "No, it's not me.");
        settle(&l, &w, &p).await;
    }
    until("every run recorded", || l.db("SELECT count(*) FROM observed WHERE thread='t-mobius' AND notified='done:m3'") == 1).await;
    assert!(deliveries(&l).is_empty(), "{:?}", deliveries(&l));
    stop.send(true).unwrap();
    task.await.unwrap();
}

/// The approval hook's thread record (bug note 2026-11-02T14-56-51-2): an observer pass records Heimdall's relay
/// threads, but not a fork Heimdall made of one of the user's threads (only in `owned_threads`): that fork is his to work
/// in, and the hook would decline his own prompts there (fresh review, 2 Nov).
#[tokio::test]
async fn the_hook_s_thread_record_keeps_relays_and_leaves_out_forks_of_owner_s_threads() {
    let l = Lab::new(true).await;
    let mut relay = thread("relay-1", "Heimdall relay", "running", "r1");
    relay["creationSource"] = json!("mcp");
    l.s.threads.lock().unwrap().push(relay);
    let data = l.dir.path().join("data");
    let w = l.workers(Arc::new(Client::new(l.runtime.clone(), l.store.clone()).unwrap().with_launch_record(data.clone())));
    let fork = launch(&l, "Fork of User's research").await;
    let gate = Arc::new(DeliveryGate::new(true));
    let p = gate.permit().unwrap();
    let (stop, rx) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(w.clone().watch(gate.clone(), rx));
    settle(&l, &w, &p).await;
    let recorded = std::fs::read_to_string(data.join(heimdall_emergency::approvals::THREADS)).unwrap_or_default();
    assert!(recorded.lines().any(|t| t == "relay-1"), "the relay is recorded: {recorded:?}");
    assert!(!recorded.lines().any(|t| t == fork), "the fork is not: {recorded:?}");
    stop.send(true).unwrap();
    task.await.unwrap();
}

#[tokio::test]
async fn own_run_finishing_wakes_once_and_repeats_of_it_wake_nothing() {
    let l = Lab::new(true).await;
    let w = l.workers(l.client(false));
    let gate = Arc::new(DeliveryGate::new(true));
    let p = gate.permit().unwrap();
    let (stop, rx) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(w.clone().watch(gate.clone(), rx));
    settle(&l, &w, &p).await;
    let mine = launch(&l, "Mine").await;
    settle(&l, &w, &p).await;
    finish(&l, &mine, "Tests pass. PR opened.");
    settle(&l, &w, &p).await;
    until("one wake", || deliveries(&l).len() == 1).await;
    let (id, text) = deliveries(&l)[0].clone();
    assert!(id.starts_with("t3:batch:") && !id.contains(":attention:"), "{id}");
    assert!(text.contains(&format!("- 'Mine' (yours) completed. [thread {mine}] Final reply:\nTests pass. PR opened.\n")), "{text}");
    // The same run reported again (running, then completed under the same run id): no second wake.
    l.set(&mine, |t| t["status"] = json!("running"));
    settle(&l, &w, &p).await;
    finish(&l, &mine, "Tests pass. PR opened.");
    settle(&l, &w, &p).await;
    // A new run with the identical result is not repeated either.
    l.set(&mine, |t| {
        t["latestRunId"] = json!("mine-r2");
        t["status"] = json!("running");
    });
    settle(&l, &w, &p).await;
    finish(&l, &mine, "Tests pass. PR opened.");
    settle(&l, &w, &p).await;
    until("second run recorded", || l.db(&format!("SELECT count(*) FROM observed WHERE thread='{mine}' AND notified='done:mine-r2'")) == 1).await;
    assert_eq!(deliveries(&l).len(), 1, "{:?}", deliveries(&l));
    stop.send(true).unwrap();
    task.await.unwrap();
}

/// tool-output: the wake carries the finished thread's reply whole, however long, across T3's text
/// pages, with unicode and paragraphs intact.
#[tokio::test]
async fn own_run_wake_carries_a_long_unicode_multi_paragraph_reply_whole() {
    let l = Lab::new(true).await;
    let w = l.workers(l.client(false));
    let gate = Arc::new(DeliveryGate::new(true));
    let p = gate.permit().unwrap();
    let (stop, rx) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(w.clone().watch(gate.clone(), rx));
    settle(&l, &w, &p).await;
    let mine = launch(&l, "Report").await;
    settle(&l, &w, &p).await;
    let reply = format!("Summary: zażółć gęślą jaźń 😀.\n\n{}\n\n- point one\n- point two 🚀\n\nThe end.", "Paragraph sentence with emoji 🧪 and Polish łódź. ".repeat(600));
    assert!(reply.encode_utf16().count() > 3 * 4000, "spans several T3 text pages");
    finish(&l, &mine, &reply);
    settle(&l, &w, &p).await;
    until("one wake", || deliveries(&l).len() == 1).await;
    let text = deliveries(&l)[0].1.clone();
    assert!(text.contains(&format!("[thread {mine}] Final reply:\n{reply}\n")), "reply not whole: {} bytes", text.len());
    assert!(l.calls("t3_thread_read").iter().any(|a| a["textOffset"].as_u64().is_some()), "continuation pages were read");
    stop.send(true).unwrap();
    task.await.unwrap();
}

/// Whole replies can be long; one wake stays under the harness's message limit and the rest follow in the next
/// wake, each reply still whole (never cut).
#[tokio::test]
async fn long_replies_split_across_wakes_instead_of_being_cut() {
    let l = Lab::new(true).await;
    let w = l.workers(l.client(false));
    let gate = Arc::new(DeliveryGate::new(true));
    let p = gate.permit().unwrap();
    let (stop, rx) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(w.clone().watch(gate.clone(), rx));
    settle(&l, &w, &p).await;
    let a = launch(&l, "First").await;
    let b = launch(&l, "Second").await;
    settle(&l, &w, &p).await;
    let reply = |n: usize| format!("Report {n}: {}end.", "wynik 😀 ok. ".repeat(2800));
    assert!(reply(1).len() > 40 * 1024 && reply(1).len() < 48 * 1024);
    for (i, id) in [&a, &b].into_iter().enumerate() {
        l.set(id, |t| {
            t["status"] = json!("completed");
            t["text"] = json!(reply(i + 1));
            t["updatedAt"] = json!(now_iso());
            t["completedAt"] = json!(now_iso());
        });
    }
    l.s.changed.notify_waiters();
    settle(&l, &w, &p).await;
    until("first wake", || !deliveries(&l).is_empty()).await;
    let first = deliveries(&l)[0].1.clone();
    assert!(first.len() < w.actions.message_limit(), "{} bytes", first.len());
    assert!(first.contains("- 1 more update follows in the next message."), "{}", &first[first.len() - 200..]);
    let carried = if first.contains(&reply(1)) { 1 } else { 2 };
    assert!(first.contains(&reply(carried)), "a reply was cut");
    rusqlite::Connection::open(l.dir.path().join("workers.db")).unwrap().execute("UPDATE observer_meta SET value=value-60000 WHERE key='last_wake'", []).unwrap();
    settle(&l, &w, &p).await;
    until("second wake", || deliveries(&l).len() == 2).await;
    assert!(deliveries(&l)[1].1.contains(&reply(3 - carried)), "the other reply arrives whole next");
    stop.send(true).unwrap();
    task.await.unwrap();
}

/// Review 19 Oct (MUST 2): the wake's budgets follow the running agent's message limit (its context setting), not
/// fixed sizes: on a small limit a reply the default pastes is pointed to, and the wake stays under the limit.
#[tokio::test]
async fn wake_budgets_follow_the_live_agent_message_limit() {
    let l = Lab::new(true).await;
    let w = l.workers(l.client(false));
    w.actions.set_message_limit(16 * 1024);
    let gate = Arc::new(DeliveryGate::new(true));
    let p = gate.permit().unwrap();
    let (stop, rx) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(w.clone().watch(gate.clone(), rx));
    settle(&l, &w, &p).await;
    let mine = launch(&l, "Report").await;
    settle(&l, &w, &p).await;
    let reply = "Long finding. ".repeat(1500);
    assert!(reply.len() > 16 * 1024 && reply.len() < 48 * 1024, "the old fixed budget would paste it");
    finish(&l, &mine, &reply);
    settle(&l, &w, &p).await;
    until("one wake", || deliveries(&l).len() == 1).await;
    let text = deliveries(&l)[0].1.clone();
    assert!(text.len() < 16 * 1024, "{} bytes", text.len());
    assert!(text.contains(&format!("(yours) completed. Its reply is over 13 KB, too long for a notice; read it with t3_thread_read. [thread {mine}]")), "{text}");
    assert!(text.ends_with(t3::text::SILENCE_RULE), "{text}");
    // Paging stopped past the budget (minor 6): 21 KB in 4000-character pages would take five continuation reads.
    let continuations = l.calls("t3_thread_read").iter().filter(|a| a["textOffset"].as_u64().is_some()).count();
    assert!(continuations < 5, "{continuations} continuation pages read");
    stop.send(true).unwrap();
    task.await.unwrap();
}

#[tokio::test]
async fn burst_of_five_own_runs_is_one_coalesced_wake() {
    let l = Lab::new(true).await;
    let w = l.workers(l.client(false));
    let gate = Arc::new(DeliveryGate::new(true));
    let p = gate.permit().unwrap();
    let (stop, rx) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(w.clone().watch(gate.clone(), rx));
    settle(&l, &w, &p).await;
    let mut ids = vec![];
    for i in 0..5 {
        ids.push(launch(&l, &format!("Task {i}")).await);
    }
    settle(&l, &w, &p).await;
    for (i, id) in ids.iter().enumerate() {
        l.set(id, |t| {
            t["status"] = json!("completed");
            t["text"] = json!(format!("Result {i}."));
            t["updatedAt"] = json!(now_iso());
            t["completedAt"] = json!(now_iso());
        });
    }
    l.s.changed.notify_waiters();
    settle(&l, &w, &p).await;
    until("one wake", || !deliveries(&l).is_empty()).await;
    settle(&l, &w, &p).await;
    let got = deliveries(&l);
    assert_eq!(got.len(), 1, "{got:?}");
    for (i, id) in ids.iter().enumerate() {
        assert!(got[0].1.contains(&format!("- 'Task {i}' (yours) completed. [thread {id}] Final reply:\nResult {i}.\n")), "{}", got[0].1);
    }
    stop.send(true).unwrap();
    task.await.unwrap();
}

#[tokio::test]
async fn question_or_approval_on_any_thread_wakes_once_per_request() {
    let l = Lab::new(true).await;
    let w = l.workers(l.client(false));
    let gate = Arc::new(DeliveryGate::new(true));
    let p = gate.permit().unwrap();
    let (stop, rx) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(w.clone().watch(gate.clone(), rx));
    settle(&l, &w, &p).await;
    {
        let mut t = l.s.threads.lock().unwrap();
        let mut ask = thread("t-ask", "Pick region", "waiting", "a1");
        ask["pending"] = json!(["req-1"]);
        ask["question"] = json!("Which region should I deploy to?");
        t.push(ask);
        t.push(thread("t-approve", "Approve deploy", "waiting", "p1"));
    }
    l.s.changed.notify_waiters();
    settle(&l, &w, &p).await;
    until("one wake", || !deliveries(&l).is_empty()).await;
    settle(&l, &w, &p).await;
    let got = deliveries(&l);
    assert_eq!(got.len(), 1, "{got:?}");
    // The harness reads the kind from the id: a batch with a question or approval is a question.
    assert!(got[0].0.starts_with("t3:batch:attention:"), "{}", got[0].0);
    assert!(got[0].1.contains("- 'Pick region' asks [thread t-ask, request req-1]:\nRegion: Which region should I deploy to? [question q]"), "{}", got[0].1);
    assert!(got[0].1.contains("- 'Approve deploy' waits for an approval in T3; only User can give it there [thread t-approve]"), "{}", got[0].1);
    assert!(!got[0].1.contains("(yours)"), "{}", got[0].1);
    // A new request on the same thread is a new question; the same one is not repeated.
    l.set("t-ask", |t| t["pending"] = json!(["req-2"]));
    settle(&l, &w, &p).await;
    // Inside the wake gap it waits for the gap to pass.
    assert_eq!(l.db("SELECT count(*) FROM pending_wakes WHERE key='t3:t-ask:' || (SELECT run FROM observed WHERE thread='t-ask') || ':attention:req-2'"), 1);
    assert_eq!(deliveries(&l).len(), 1);
    rusqlite::Connection::open(l.dir.path().join("workers.db"))
        .unwrap()
        .execute("UPDATE observer_meta SET value=value-60000 WHERE key='last_wake'", [])
        .unwrap();
    settle(&l, &w, &p).await;
    until("second question wakes", || deliveries(&l).len() == 2).await;
    assert!(deliveries(&l)[1].1.contains("request req-2"), "{:?}", deliveries(&l));
    settle(&l, &w, &p).await;
    assert_eq!(deliveries(&l).len(), 2);
    stop.send(true).unwrap();
    task.await.unwrap();
}

/// Heimdall's permission hook answered the rm prompt and the command ran, but T3 never closes its approval card (live
/// check 28 Oct 19:21): that card is no wait and wakes nothing; a card whose command has not run still does.
#[tokio::test]
async fn an_approval_card_the_hook_already_answered_wakes_nothing() {
    let l = Lab::new(true).await;
    let w = l.workers(l.client(false));
    let gate = Arc::new(DeliveryGate::new(true));
    let p = gate.permit().unwrap();
    let (stop, rx) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(w.clone().watch(gate.clone(), rx));
    settle(&l, &w, &p).await;
    let card = |call: &str| json!({"itemId":format!("turn-item:runtime-request:runtime-request%3Aprovider%3AclaudeAgent%3Anative-request%3A{call}%3Ad555"),"position":3,"type":"approval_request","status":"waiting"});
    let command = |call: &str, status: &str| json!({"itemId":format!("turn-item:provider:claudeAgent:native-item:{call}"),"position":2,"type":"command_execution","status":status});
    {
        let mut t = l.s.threads.lock().unwrap();
        let mut answered = thread("t-answered", "Clean scratch", "waiting", "s1");
        answered["activity"] = json!([command("toolu_A", "completed"), card("toolu_A")]);
        t.push(answered);
    }
    l.s.changed.notify_waiters();
    settle(&l, &w, &p).await;
    settle(&l, &w, &p).await;
    assert!(deliveries(&l).is_empty(), "{:?}", deliveries(&l));
    {
        let mut t = l.s.threads.lock().unwrap();
        let mut stuck = thread("t-stuck", "Approve deploy", "waiting", "d1");
        stuck["activity"] = json!([command("toolu_B", "running"), card("toolu_B")]);
        t.push(stuck);
    }
    l.s.changed.notify_waiters();
    settle(&l, &w, &p).await;
    until("the real wait wakes", || !deliveries(&l).is_empty()).await;
    let got = deliveries(&l);
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(got[0].1.contains("'Approve deploy' waits for an approval") && !got[0].1.contains("Clean scratch"), "{}", got[0].1);
    stop.send(true).unwrap();
    task.await.unwrap();
}

/// Bug note 2026-11-01T00-15-42 (31 Oct 17:41: the Goals thread showed "waits on a question or approval" with no
/// wake): the observer woke only on T3's status "waiting", while the labels used `pendingRequestCount`. Both now use
/// one rule: a request counted while the run is live wakes (here an approval card on a running thread); a finished
/// thread's leftover count wakes nothing (bug notes 2026-11-02T14-56-51, 16-19-09).
#[tokio::test]
async fn a_counted_request_on_a_live_run_wakes_and_a_finished_runs_leftover_does_not() {
    let l = Lab::new(true).await;
    let w = l.workers(l.client(false));
    let gate = Arc::new(DeliveryGate::new(true));
    let p = gate.permit().unwrap();
    let (stop, rx) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(w.clone().watch(gate.clone(), rx));
    settle(&l, &w, &p).await;
    let card = |call: &str| json!({"itemId":format!("turn-item:runtime-request:runtime-request%3Aprovider%3AclaudeAgent%3Anative-request%3A{call}%3Ad555"),"position":3,"type":"approval_request","status":"waiting"});
    let command = |call: &str, status: &str| json!({"itemId":format!("turn-item:provider:claudeAgent:native-item:{call}"),"position":2,"type":"command_execution","status":status});
    {
        let mut t = l.s.threads.lock().unwrap();
        let mut done = thread("t-done", "Audit T3", "completed", "a1");
        done["pendingRequestCount"] = json!(1);
        done["completedAt"] = json!(now_iso());
        done["activity"] = json!([command("toolu_C", "running"), card("toolu_C")]);
        t.push(done);
    }
    l.s.changed.notify_waiters();
    settle(&l, &w, &p).await;
    settle(&l, &w, &p).await;
    assert!(deliveries(&l).is_empty(), "a finished thread's leftover approval: {:?}", deliveries(&l));
    {
        let mut t = l.s.threads.lock().unwrap();
        let mut live = thread("t-live", "Goals list", "running", "g1");
        live["pendingRequestCount"] = json!(1);
        live["activity"] = json!([command("toolu_D", "running"), card("toolu_D")]);
        t.push(live);
    }
    l.s.changed.notify_waiters();
    settle(&l, &w, &p).await;
    until("the live run's approval wakes", || !deliveries(&l).is_empty()).await;
    let got = deliveries(&l);
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(got[0].1.contains("'Goals list' waits for an approval") && !got[0].1.contains("Audit T3"), "{}", got[0].1);
    stop.send(true).unwrap();
    task.await.unwrap();
}

#[tokio::test]
async fn restart_with_old_finished_runs_wakes_nothing() {
    let l = Lab::new(true).await;
    let mut old = thread("t-old", "Old work", "completed", "o1");
    old["updatedAt"] = json!("2026-01-01T00:00:00Z");
    old["completedAt"] = json!("2026-01-01T00:00:00Z");
    l.s.threads.lock().unwrap().push(old);
    // First process: launches its own run and reaches a steady state.
    let gate1 = Arc::new(DeliveryGate::new(true));
    let p1 = gate1.permit().unwrap();
    let w1 = l.workers(l.client(false));
    let (stop1, rx1) = tokio::sync::watch::channel(false);
    let task1 = tokio::spawn(w1.clone().watch(gate1.clone(), rx1));
    settle(&l, &w1, &p1).await;
    let mine = launch(&l, "Mine").await;
    settle(&l, &w1, &p1).await;
    stop1.send(true).unwrap();
    task1.await.unwrap();
    // Heimdall is off: its own run finishes, and a foreign thread finishes and starts a new run.
    finish(&l, &mine, "Finished while Heimdall was off.");
    l.s.threads.lock().unwrap().push(thread("t-away", "Other", "completed", "x1"));
    // Second process: the baseline pass and the pass after it wake nothing.
    let gate2 = Arc::new(DeliveryGate::new(true));
    let p2 = gate2.permit().unwrap();
    let w2 = l.workers(l.client(false));
    let (stop2, rx2) = tokio::sync::watch::channel(false);
    let task2 = tokio::spawn(w2.clone().watch(gate2.clone(), rx2));
    settle(&l, &w2, &p2).await;
    settle(&l, &w2, &p2).await;
    assert!(deliveries(&l).is_empty(), "{:?}", deliveries(&l));
    // Wakes still work after the restart.
    let next = launch(&l, "After restart").await;
    settle(&l, &w2, &p2).await;
    finish(&l, &next, "Back to work.");
    settle(&l, &w2, &p2).await;
    until("one wake after restart", || deliveries(&l).len() == 1).await;
    let got = deliveries(&l);
    assert!(got[0].1.contains("'After restart' (yours) completed. [thread "), "{}", got[0].1);
    assert!(got[0].1.contains("Final reply:\nBack to work."), "{}", got[0].1);
    assert!(!got[0].1.contains("Finished while Heimdall was off"), "{}", got[0].1);
    stop2.send(true).unwrap();
    task2.await.unwrap();
}

#[tokio::test]
async fn unconnected_revoked_or_closed_t3_wakes_no_agent_and_reports_its_state() {
    // Each case runs the watch loop with T3 unusable: the loop stays alive, the agent hears nothing (E7.6), the
    // connection state is reported, and the island shows its one prompt.
    async fn quiet(l: &Lab, client: Arc<Client>, state: ConnectionState, prompt: &str) {
        let w = l.workers(client);
        let gate = Arc::new(DeliveryGate::new(true));
        let (stop, rx) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(w.clone().watch(gate.clone(), rx));
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(!task.is_finished(), "watch loop stopped");
        assert!(deliveries(l).is_empty(), "the agent heard about T3: {:?}", deliveries(l));
        assert_eq!(w.connection(), state);
        assert!(l.sink.notices.lock().unwrap().iter().any(|n| n == prompt), "{:?}", l.sink.notices.lock().unwrap());
        assert_eq!(l.db("SELECT count(*) FROM observed"), 0);
        stop.send(true).unwrap();
        task.await.unwrap();
    }
    // Never paired.
    let l = Lab::new(false).await;
    let client = l.client(false);
    quiet(&l, client, ConnectionState::SignInRequired, "Connect T3 · click").await;
    // Revoked by the user in T3 Settings.
    let l = Lab::new(true).await;
    l.store.save(&Credential { token: "fake-secret".into(), expires_ms: agent::now_ms() + 30 * 24 * 60 * 60 * 1000, environment: "env".into(), revoked: true }).unwrap();
    let client = l.client(false);
    quiet(&l, client, ConnectionState::SignInRequired, "Connect T3 · click").await;
    // T3 Code closed: the discovery file points at a port nothing listens on.
    let l = Lab::new(true).await;
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    std::fs::write(&l.runtime, json!({"port": closed}).to_string()).unwrap();
    let client = l.client(false);
    quiet(&l, client, ConnectionState::Closed, "T3 Code closed").await;
}

/// One read-only pass against the running T3 Code (Nightly): HEIMDALL_T3_LIVE=1, `--ignored`. Issues a five-minute
/// orchestration:read session through T3's CLI, uses it only for reads, and revokes it.
#[tokio::test]
#[ignore]
async fn live_readonly_smoke_against_running_t3() {
    if std::env::var("HEIMDALL_T3_LIVE").as_deref() != Ok("1") {
        return;
    }
    let home = PathBuf::from(std::env::var("HOME").unwrap());
    let cli = home.join(".t3/bin/t3");
    let out = std::process::Command::new(&cli)
        .args(["auth", "session", "issue", "--json", "--scope", "orchestration:read", "--ttl", "5m", "--label", "Heimdall read-only smoke", "--subject", "mcp-client"])
        .output()
        .unwrap();
    assert!(out.status.success(), "T3 CLI failed");
    let issued: Value = serde_json::from_slice(&out.stdout).unwrap();
    let session = issued["sessionId"].as_str().or(issued["id"].as_str()).unwrap().to_owned();
    let result = std::panic::AssertUnwindSafe(async {
        let runtime = home.join(".t3/userdata/server-runtime.json");
        let port = serde_json::from_str::<Value>(&std::fs::read_to_string(&runtime).unwrap()).unwrap()["port"].as_u64().unwrap();
        let env: Value = reqwest::get(format!("http://127.0.0.1:{port}/.well-known/t3/environment")).await.unwrap().json().await.unwrap();
        let store = Arc::new(Store::default());
        store.save(&Credential { token: issued["token"].as_str().unwrap().into(), expires_ms: agent::now_ms() + 4 * 60 * 1000, environment: env["environmentId"].as_str().unwrap().into(), revoked: false }).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let actions = Arc::new(Actions::open(&dir.path().join("actions.db")).unwrap());
        let sink = Arc::new(Sink::default());
        actions.attach(sink.clone());
        let w = Arc::new(Workers::open(&dir.path().join("workers.db"), Arc::new(Client::new(runtime, store).unwrap()), actions).unwrap());
        let gate = Arc::new(DeliveryGate::new(true));
        let p = gate.permit().unwrap();
        let list = w.execute("t3_tools", json!({}), "live-1", &p).await.unwrap();
        println!("{}", list.lines().next().unwrap());
        // Night 20 Oct tools: one call each, compact answers. Only sizes and first lines are printed, never contents.
        let models = w.execute("t3_call", json!({"tool":"orchestrator_capabilities","args":{}}), "live-2", &p).await.unwrap();
        println!("catalog: {} bytes, {} lines", models.len(), models.lines().count());
        let threads = w.execute("t3_thread_list", json!({"limit":5}), "live-3", &p).await.unwrap();
        println!("t3_thread_list: {} bytes, {}", threads.len(), threads.lines().next().unwrap());
        let ids: Vec<String> = threads.lines().filter_map(|l| l.rsplit("[thread ").next().filter(|_| l.ends_with(']')).map(|x| x.trim_end_matches(']').to_owned())).take(3).collect();
        let read = w.execute("t3_thread_read", json!({"threadIds":ids}), "live-4", &p).await.unwrap();
        println!("t3_thread_read of {} threads: {} bytes, {} lines", ids.len(), read.len(), read.lines().count());
        assert!(!read.contains("recentRuns") && !read.contains("runId"));
        let activity = w.execute("t3_thread_read", json!({"threadId":ids[0],"view":"activity","last":5}), "live-5", &p).await.unwrap();
        println!("activity: {} lines", activity.lines().count());
        let e = w.execute("t3_call", json!({"tool":"t3_thread_list","args":{"limit":"x"}}), "live-6", &p).await.unwrap_err();
        println!("validation: {e}");
        let (stop, rx) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(w.clone().watch(gate.clone(), rx));
        let start = std::time::Instant::now();
        let observed = || rusqlite::Connection::open(dir.path().join("workers.db")).unwrap().query_row("SELECT count(*) FROM observed", [], |r| r.get::<_, i64>(0)).unwrap();
        while observed() == 0 && start.elapsed() < Duration::from_secs(30) {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        stop.send(true).unwrap();
        task.await.unwrap();
        println!("observer baseline rows: {}, wakes: {}", observed(), sink.deliveries.lock().unwrap().iter().filter(|(id, _)| id.starts_with("t3:batch:")).count());
        assert!(observed() > 0);
    });
    let outcome = futures_util::FutureExt::catch_unwind(result).await;
    let revoked = std::process::Command::new(&cli).args(["auth", "session", "revoke", &session]).output().unwrap();
    assert!(revoked.status.success(), "revoke failed: revoke session {session} by hand");
    outcome.unwrap();
}
