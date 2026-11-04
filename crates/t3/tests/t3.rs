//! Local fake only: never creates real T3 threads or invokes providers.
use agent::{
    actions::{ActionSink, Actions, Notice, Question},
    delivery::{DeliveryGate, DeliveryPermit},
};
use t3::client::Credential;
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
    collections::{HashMap, HashSet},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
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
struct Fake {
    transport_failed: AtomicBool,
    missing_capability: AtomicBool,
    catalog: Mutex<Value>,
    fail_read: AtomicBool,
    fail_wait: AtomicBool,
    reject_launch: AtomicBool,
    activity: Mutex<Option<Vec<Value>>>,
    origin: Mutex<String>,
    calls: Mutex<Vec<(String, Value)>>,
    unauthorized: AtomicBool,
    restarted: AtomicBool,
    terminal: AtomicBool,
    pending: AtomicBool,
    lose_launch: AtomicBool,
    wait: tokio::sync::Notify,
    text: String,
}
impl Fake {
    fn capabilities() -> Value {
        json!({"runtimeMode":"full-access","providers":[{"providerInstanceId":"codex","driverKind":"codex","runtimeModes":["full-access"],"models":[{"id":"test-model","options":[{"id":"reasoningEffort","type":"select","options":[{"id":"high"}]}]},{"id":"test-next","options":[]}]},{"providerInstanceId":"claudeAgent","driverKind":"claude-code","runtimeModes":["full-access"],"models":[{"id":"test-other","options":[]}]}]})
    }
}
async fn mcp(State(s): State<Arc<Fake>>, headers: HeaderMap, Json(v): Json<Value>) -> Response {
    if s.transport_failed.load(Ordering::Acquire) {return StatusCode::SERVICE_UNAVAILABLE.into_response();}
    if s.unauthorized.load(Ordering::Acquire) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    assert_eq!(headers.get("authorization").unwrap(), "Bearer fake-secret");
    if v["method"] == "initialize" {
        return ([("Mcp-Session-Id",if s.restarted.load(Ordering::Acquire){"session-2"}else{"session-1"})],Json(json!({"jsonrpc":"2.0","id":v["id"],"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"Fake T3","version":"1"}}}))).into_response();
    }
    assert_eq!(headers.get("mcp-protocol-version").unwrap(), "2025-06-18");
    if headers.get("mcp-session-id").unwrap()
        != if s.restarted.load(Ordering::Acquire) {
            "session-2"
        } else {
            "session-1"
        }
    {
        return StatusCode::NOT_FOUND.into_response();
    }
    if v["method"] == "notifications/initialized" {
        return StatusCode::ACCEPTED.into_response();
    }
    if v["method"] == "tools/list" {
        let names = [
            "orchestrator_capabilities",
            "t3_project_list",
            "t3_thread_list",
            "t3_thread_launch",
            "t3_thread_send",
            "t3_thread_configure",
            "t3_thread_organize",
            "t3_thread_interrupt",
            "t3_thread_read",
            "t3_thread_wait",
            "t3_pending_request",
        ];
        return Json(json!({"jsonrpc":"2.0","id":v["id"],"result":{"tools":names.iter().filter(|n|!s.missing_capability.load(Ordering::Acquire)|| **n!="t3_thread_read").map(|n|json!({"name":n,"inputSchema":{"type":"object"}})).collect::<Vec<_>>()}})).into_response();
    }
    let name = v["params"]["name"].as_str().unwrap().to_owned();
    let a = &v["params"]["arguments"];
    s.calls.lock().unwrap().push((name.clone(), a.clone()));
    if (name=="t3_thread_read" && s.fail_read.load(Ordering::Acquire)) || (name=="t3_thread_wait" && s.fail_wait.load(Ordering::Acquire)) || (name=="t3_thread_launch" && s.reject_launch.load(Ordering::Acquire)) {
        return Json(json!({"jsonrpc":"2.0","id":v["id"],"result":{"isError":true,"content":[{"type":"text","text":"runtimeMode full-access rejected by server"}]}})).into_response();
    }
    let terminal = s.terminal.load(Ordering::Acquire);
    let status = if terminal { "completed" } else { "running" };
    let result = match name.as_str() {
        "orchestrator_capabilities" => s.catalog.lock().unwrap().clone(),
        "t3_project_list" => {
            json!({"projects":[{"id":"project","workspaceRoot":a["testRoot"]}],"nextCursor":null})
        }
        "t3_thread_list" => {
            let launched = s
                .calls
                .lock()
                .unwrap()
                .iter()
                .rev()
                .find(|(n, _)| n == "t3_thread_launch")
                .map(|(_, a)| a.clone());
            json!({"threads":launched.map(|a|vec![json!({"threadId":"owned-thread","latestRunId":"run-1","status":status,"title":a["title"],"providerInstanceId":"codex","model":"test-model","runtimeMode":"full-access"})]).unwrap_or_default()})
        }
        "t3_thread_launch" => {
            if s.lose_launch.swap(false, Ordering::AcqRel) {
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
            assert_eq!(a["runtimeMode"], "full-access");
            assert_eq!(a["modelSelection"]["instanceId"], "codex");
            json!({"threadId":"owned-thread","runId":"run-1","status":"running"})
        }
        "t3_thread_send" => {
            json!({"threadId":"owned-thread","runId":"run-2","status":"running","delivery":"steered"})
        }
        "t3_thread_interrupt" => json!({"threadId":"owned-thread","status":"interrupt_requested"}),
        "t3_thread_configure" | "t3_thread_organize" => json!({"sequence":1}),
        "t3_pending_request" if a["action"] == "list" => {
            json!({"requestIds":if s.pending.load(Ordering::Acquire){vec!["q-1"]}else{vec![]}})
        }
        "t3_pending_request" if a["action"] == "read" => {
            json!({"requestId":"q-1","questions":[{"id":"q","question":"What should I do?","options":[]}]})
        }
        "t3_thread_wait" => {
            let notified = s.wait.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if !s.terminal.load(Ordering::Acquire) {
                notified.await;
            }
            json!({"threadId":"owned-thread","runId":"run-1","status":"completed","timedOut":false})
        }
        "t3_thread_read" => {
            let offset = a["textOffset"].as_u64().unwrap_or(0) as usize;
            let units = s.text.encode_utf16().collect::<Vec<_>>();
            let n = a["maxCharsPerItem"].as_u64().unwrap_or(100) as usize;
            let end = (offset + n - (n % 3)).min(units.len());
            let after = a["afterPosition"].as_i64();
            let items = if a["view"]=="activity" && s.activity.lock().unwrap().is_some() {
                let all=s.activity.lock().unwrap().clone().unwrap();
                all.into_iter().filter(|i|if let Some(id)=a["itemId"].as_str(){i["itemId"]==id}else{i["position"].as_i64().is_some_and(|n|after.is_none_or(|p|n>p))}).take(a["limit"].as_u64().unwrap_or(5) as usize).collect()
            } else if after.is_some() {
                vec![]
            } else {
                vec![
                    json!({"itemId":"item-1","runId":"run-1","position":0,"type":"assistant_message","status":status,"text":String::from_utf16(&units[offset..end]).unwrap(),"textTruncated":end<units.len(),"nextTextOffset":(end<units.len()).then_some(end)}),
                ]
            };
            let next=items.last().and_then(|i|i["position"].as_i64()).or(after).unwrap_or(0);
            let more=a["view"]=="activity" && a["itemId"].is_null() && s.activity.lock().unwrap().as_ref().is_some_and(|all|all.iter().any(|i|i["position"].as_i64().is_some_and(|p|p>next)));
            json!({"thread":{"threadId":"owned-thread","status":status,"activeRunId":if terminal{Value::Null}else{json!("run-1")},"latestRunId":"run-1","pendingRequestCount":if s.pending.load(Ordering::Acquire){2}else{0},"archived":false},"items":items,"recentRuns":[],"nextPosition":next,"hasMore":more})
        }
        _ => panic!("unexpected call"),
    };
    let result = if name == "t3_project_list" {
        let root = s
            .calls
            .lock()
            .unwrap()
            .iter()
            .find_map(|(n, a)| {
                if n == "test_root" {
                    a.as_str().map(str::to_owned)
                } else {
                    None
                }
            })
            .unwrap();
        json!({"projects":[{"id":"project","workspaceRoot":root}],"nextCursor":null})
    } else {
        result
    };
    let wire =
        json!({"jsonrpc":"2.0","id":v["id"],"result":{"structuredContent":result,"content":[]}});
    (
        [("Content-Type", "text/event-stream")],
        format!(": heartbeat\n\ndata: {wire}\n\n"),
    )
        .into_response()
}
async fn oauth(State(s): State<Arc<Fake>>) -> Json<Value> {
    let o = s.origin.lock().unwrap().clone();
    Json(
        json!({"issuer":o,"authorization_endpoint":format!("{o}/approve"),"token_endpoint":format!("{o}/token"),"registration_endpoint":format!("{o}/register")}),
    )
}
async fn resource(State(s): State<Arc<Fake>>) -> Json<Value> {
    Json(json!({"resource":format!("{}/mcp",s.origin.lock().unwrap())}))
}
async fn register(Json(v): Json<Value>) -> Json<Value> {
    assert_eq!(v["client_name"], "Heimdall");
    Json(json!({"client_id":"client"}))
}
async fn token(Form(v): Form<HashMap<String, String>>) -> Json<Value> {
    assert_eq!(v["grant_type"], "authorization_code");
    assert_eq!(v["code"], "auth-code");
    assert_eq!(v["client_id"], "client");
    assert!(v["code_verifier"].len() >= 43);
    Json(json!({"access_token":"fake-secret","token_type":"Bearer","expires_in":30*24*60*60}))
}
struct Lab {
    dir: tempfile::TempDir,
    s: Arc<Fake>,
    server: tokio::task::JoinHandle<()>,
    client: Arc<Client>,
    store: Arc<Store>,
}
impl Lab {
    async fn new(auth: bool) -> Self {
        Self::with_text(auth, "😀ż".repeat(3000)).await
    }
    /// A lab whose worker reply is `text`.
    async fn with_text(auth: bool, text: String) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let s = Arc::new(Fake {
            transport_failed: AtomicBool::new(false),
            missing_capability: AtomicBool::new(false),
            catalog: Mutex::new(Fake::capabilities()),
            fail_read: AtomicBool::new(false),
            fail_wait: AtomicBool::new(false),
            reject_launch: AtomicBool::new(false),
            activity: Mutex::new(None),
            origin: Mutex::new(origin.clone()),
            calls: Mutex::new(vec![("test_root".into(), json!(dir.path()))]),
            unauthorized: AtomicBool::new(false),
            restarted: AtomicBool::new(false),
            terminal: AtomicBool::new(false),
            pending: AtomicBool::new(false),
            lose_launch: AtomicBool::new(false),
            wait: tokio::sync::Notify::new(),
            text,
        });
        let app = Router::new()
            .route("/mcp", post(mcp))
            .route(
                "/.well-known/t3/environment",
                get(|| async { Json(json!({"environmentId":"env"})) }),
            )
            .route("/.well-known/oauth-authorization-server", get(oauth))
            .route("/.well-known/oauth-protected-resource/mcp", get(resource))
            .route("/register", post(register))
            .route("/token", post(token))
            .with_state(s.clone());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let runtime = dir.path().join("runtime.json");
        std::fs::write(
            &runtime,
            json!({"port":origin.rsplit(':').next().unwrap().parse::<u16>().unwrap()}).to_string(),
        )
        .unwrap();
        let store = Arc::new(Store::default());
        if auth {
            store
                .save(&Credential {
                    token: "fake-secret".into(),
                    expires_ms: agent::now_ms() + 30 * 24 * 60 * 60 * 1000,
                    environment: "env".into(),
                    revoked: false,
                })
                .unwrap();
        }
        let client = Arc::new(Client::new(runtime, store.clone()).unwrap());
        Self {
            dir,
            s,
            server,
            client,
            store,
        }
    }
    fn workers(&self) -> Arc<Workers> {
        let actions = Arc::new(Actions::open(&self.dir.path().join("actions.db")).unwrap());
        actions.attach(Arc::new(Sink::default()));
        Arc::new(
            Workers::open(
                &self.dir.path().join("workers.db"),
                self.client.clone(),
                actions,
            )
            .unwrap(),
        )
    }
    async fn start(&self, w: &Workers, p: &DeliveryPermit) {
        w.execute("delegate_task",json!({"path":self.dir.path(),"target":{"providerInstanceId":"codex","model":"test-model"},"title":"Test","task":"Never billed"}),"launch-1",p).await.unwrap();
    }
}
impl Drop for Lab {
    fn drop(&mut self) {
        self.server.abort();
    }
}
#[derive(Default)]
struct Sink {
    ids: Mutex<HashSet<String>>,
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
    fn deliver(&self, id: &str, _: &str, p: &DeliveryPermit) -> Result<()> {
        p.check()?;
        self.ids.lock().unwrap().insert(id.into());
        Ok(())
    }
    fn reload(&self, _: &str, p: &DeliveryPermit) -> Result<()> {
        p.check()
    }
}
#[tokio::test]
async fn retains_session_and_protocol_parses_sse_and_reinitializes_after_restart() {
    let l = Lab::new(true).await;
    assert_eq!(
        l.client
            .call("orchestrator_capabilities", json!({}))
            .await
            .unwrap(),
        Fake::capabilities()
    );
    l.s.restarted.store(true, Ordering::Release);
    assert!(l
        .client
        .call("orchestrator_capabilities", json!({}))
        .await
        .is_err());
    assert_eq!(
        l.client
            .call("orchestrator_capabilities", json!({}))
            .await
            .unwrap(),
        Fake::capabilities()
    );
}
#[tokio::test]
async fn expiry_and_401_require_click_never_issue_a_token() {
    let l = Lab::new(true).await;
    assert!(!l.client.expiry_notice().unwrap());
    l.s.unauthorized.store(true, Ordering::Release);
    let e = l
        .client
        .call("orchestrator_capabilities", json!({}))
        .await
        .unwrap_err();
    assert!(e.to_string().contains("Connect T3"));
    let mut c = l.store.load().unwrap().unwrap();
    let before = l.client.connection_notice_key();
    c.expires_ms = 1;
    l.store.save(&c).unwrap();
    assert!(l.client.expiry_notice().unwrap());
    assert_ne!(before, l.client.connection_notice_key());
    assert!(l
        .client
        .call("orchestrator_capabilities", json!({}))
        .await
        .unwrap_err()
        .to_string()
        .contains("Connect T3"));
}
#[tokio::test]
async fn oauth_pkce_loopback_callback_and_memory_credential() {
    let l = Lab::new(false).await;
    assert!(l
        .client
        .call("orchestrator_capabilities", json!({}))
        .await
        .is_err());
    l.client
        .authorize(|auth| {
            let u = url::Url::parse(auth)?;
            let q = u.query_pairs().collect::<HashMap<_, _>>();
            assert_eq!(q["code_challenge_method"], "S256");
            assert_eq!(q["scope"], "orchestration:read orchestration:operate");
            let mut callback = url::Url::parse(&q["redirect_uri"])?;
            callback.query_pairs_mut().extend_pairs([
                ("state", q["state"].as_ref()),
                ("iss", u.origin().ascii_serialization().as_str()),
                ("code", "auth-code"),
            ]);
            tokio::spawn(async move {
                reqwest::get(callback).await.unwrap();
            });
            Ok(())
        })
        .await
        .unwrap();
    assert!(l.store.load().unwrap().is_some());
    assert!(l.client.catalog().await.unwrap()["tools"].is_array());
    assert!(!std::fs::read_to_string(l.dir.path().join("runtime.json"))
        .unwrap()
        .contains("fake-secret"));
}
#[tokio::test]
async fn owned_boundary_full_access_replay_and_utf16_reply() {
    let l = Lab::new(true).await;
    let w = l.workers();
    let gate = Arc::new(DeliveryGate::new(true));
    let p = gate.permit().unwrap();
    assert!(w.execute("task_cancel", json!({"taskId":"not-owned"}), "bad", &p).await.unwrap_err().to_string().starts_with("TASK_NOT_OWNED"));
    l.start(&w, &p).await;
    l.start(&w, &p).await;
    assert_eq!(l.s.calls.lock().unwrap().iter().filter(|(n, _)| n == "t3_thread_launch").count(), 1);
    // task_status with an id is the latest reply, whole, in one call (tool-output, 19 Oct).
    let reply = w.execute("task_status", json!({"taskId":"owned-thread"}), "read", &p).await.unwrap();
    assert!(reply.starts_with("'Test' · working") && reply.contains("[task owned-thread]"), "{reply}");
    assert!(reply.ends_with(&format!("\nLatest reply:\n{}", l.s.text)), "reply not whole");
    gate.store(false, Ordering::Release);
    gate.store(true, Ordering::Release);
    assert!(w.execute("t3_thread_send", json!({"threadId":"owned-thread","message":"private"}), "off", &p).await.is_err());
    assert!(!l.s.calls.lock().unwrap().iter().any(|(n, _)| n == "t3_thread_send"));
}
#[tokio::test]
async fn event_wait_resolves_without_polling_and_watcher_deduplicates_terminal() {
    let l = Lab::new(true).await;
    let w = l.workers();
    let gate = Arc::new(DeliveryGate::new(true));
    l.start(&w, &gate.permit().unwrap()).await;
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    l.s.pending.store(true, Ordering::Release);
    let watch = tokio::spawn(w.clone().watch(gate.clone(), stop_rx));
    let start = std::time::Instant::now();
    loop {
        if l.s
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|(n, _)| n == "t3_thread_wait")
        {
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(3));
        tokio::task::yield_now().await;
    }
    l.s.terminal.store(true, Ordering::Release);
    l.s.wait.notify_waiters();
    let dbpath = l.dir.path().join("workers.db");
    let began = std::time::Instant::now();
    loop {
        let db = rusqlite::Connection::open(&dbpath).unwrap();
        let n: i64 = db
            .query_row(
                "SELECT count(*) FROM notices WHERE kind='terminal'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        if n == 1 {
            break;
        }
        assert!(began.elapsed() < Duration::from_secs(3));
        tokio::task::yield_now().await;
    }
    stop_tx.send(true).unwrap();
    watch.await.unwrap();
    let db = rusqlite::Connection::open(dbpath).unwrap();
    let body: String = db
        .query_row("SELECT body FROM final_items LIMIT 1", [], |r| r.get(0))
        .unwrap();
    assert_eq!(body, l.s.text);
    // The finished notice carries the reply whole (tool-output, 19 Oct): no t3_worker_read needed to see it.
    let outbox = rusqlite::Connection::open(l.dir.path().join("actions.db")).unwrap();
    let notice: String = outbox.query_row("SELECT text FROM outbox WHERE id LIKE 'terminal:%'", [], |r| r.get(0)).unwrap();
    assert_eq!(notice, format!("Task 'Test' (owned-thread) completed. Final reply:\n\n{}\n\n{}", l.s.text, t3::text::SILENCE_RULE));
    let question: String = outbox.query_row("SELECT text FROM outbox WHERE id LIKE 'question:%'", [], |r| r.get(0)).unwrap();
    assert_eq!(question, "Task 'Test' (owned-thread) asks [request q-1]:\nWhat should I do? [question q]");
    let questions: i64 = db
        .query_row(
            "SELECT count(*) FROM notices WHERE kind='question'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let approvals: i64 = db
        .query_row(
            "SELECT count(*) FROM notices WHERE kind='approval'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!((questions, approvals), (1, 1));
    assert!(w.active().unwrap().is_empty());
}
#[tokio::test]
async fn rediscovery_follows_port_change_with_same_environment() {
    let l = Lab::new(true).await;
    let next = Lab::new(true).await;
    l.client
        .call("orchestrator_capabilities", json!({}))
        .await
        .unwrap();
    l.server.abort();
    std::fs::copy(
        next.dir.path().join("runtime.json"),
        l.dir.path().join("runtime.json"),
    )
    .unwrap();
    assert!(l
        .client
        .call("orchestrator_capabilities", json!({}))
        .await
        .is_err());
    assert_eq!(
        l.client
            .call("orchestrator_capabilities", json!({}))
            .await
            .unwrap(),
        Fake::capabilities()
    );
}

#[tokio::test]
async fn uncertain_launch_reconciles_without_creating_another_worker() {
    let l = Lab::new(true).await;
    let w = l.workers();
    let p = Arc::new(DeliveryGate::new(true)).permit().unwrap();
    l.s.lose_launch.store(true, Ordering::Release);
    let args = json!({"path":l.dir.path(),"target":{"providerInstanceId":"codex","model":"test-model"},"title":"Test","task":"Never billed"});
    assert!(w
        .execute("delegate_task", args.clone(), "lost-launch", &p)
        .await
        .is_err());
    let recovered = w.execute("delegate_task", args, "lost-launch", &p).await.unwrap();
    assert_eq!(recovered, "Recovered the earlier start of 'Test' as task owned-thread (test-model). Its result arrives as a notice when it is done; end your turn.");
    assert_eq!(
        l.s.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(n, _)| n == "t3_thread_launch")
            .count(),
        1
    );
}

async fn wait_db(l:&Lab,query:&str,expected:i64) {
    let start=std::time::Instant::now();
    loop {let db=rusqlite::Connection::open(l.dir.path().join("workers.db")).unwrap();let n:i64=db.query_row(query,[],|r|r.get(0)).unwrap();if n==expected{return;}assert!(start.elapsed()<Duration::from_secs(3),"{query}: {n} != {expected}");tokio::task::yield_now().await;}
}
#[tokio::test]
async fn recorded_live_null_modes_use_provider_defaults_and_server_rejection_is_clear() {
    let l=Lab::new(true).await;let evidence:Value=serde_json::from_str(include_str!("fixtures/t3-live-v2-evidence.json")).unwrap();
    let real=evidence.as_object().unwrap().values().find(|v|v["providers"].is_array()).unwrap();
    for provider in l.s.catalog.lock().unwrap()["providers"].as_array_mut().unwrap() {
        let recorded=real["providers"].as_array().unwrap().iter().find(|p|p["providerInstanceId"]==provider["providerInstanceId"]).unwrap();
        provider["runtimeModes"]=recorded["runtimeModes"].clone();assert!(provider["runtimeModes"].is_null());
    }
    let w=l.workers();let gate=Arc::new(DeliveryGate::new(true));let p=gate.permit().unwrap();l.start(&w,&p).await;
    assert_eq!(l.s.calls.lock().unwrap().iter().find(|(n,_)|n=="t3_thread_launch").unwrap().1["runtimeMode"],"full-access");
    l.s.reject_launch.store(true,Ordering::Release);
    let error=w.execute("delegate_task",json!({"path":l.dir.path(),"target":{"model":"test-other"},"title":"Rejected","task":"Never billed"}),"rejected",&p).await.unwrap_err();
    // Day review T2: T3's own reason, not a generic line.
    assert_eq!(error.to_string(),"T3_TOOL_FAILED: runtimeMode full-access rejected by server");
    l.s.catalog.lock().unwrap()["providers"][0]["runtimeModes"]=json!([]);
    assert!(w.execute("delegate_task",json!({"path":l.dir.path(),"target":{"model":"test-model"},"title":"No modes","task":"Never billed"}),"no-modes",&p).await.unwrap_err().to_string().contains("PERMISSION_UNAVAILABLE"));
}
#[tokio::test]
async fn monitoring_failure_is_durable_deduplicated_and_recovers_with_owned_work_retained() {
    let l=Lab::new(true).await;let w=l.workers();let gate=Arc::new(DeliveryGate::new(true));l.start(&w,&gate.permit().unwrap()).await;
    l.s.fail_read.store(true,Ordering::Release);
    for round in 0..2 {
        let (stop,rx)=tokio::sync::watch::channel(false);let task=tokio::spawn(w.clone().watch(gate.clone(),rx));
        wait_db(&l,"SELECT count(*) FROM notices WHERE kind='monitor'",1).await;
        if round==1 {tokio::time::sleep(Duration::from_millis(30)).await;}
        stop.send(true).unwrap();task.await.unwrap();
    }
    assert_eq!(w.active().unwrap().len(),1);
    let adb=rusqlite::Connection::open(l.dir.path().join("actions.db")).unwrap();
    assert_eq!(adb.query_row("SELECT count(*) FROM receipts WHERE body LIKE '%Worker monitoring unavailable%'",[],|r|r.get::<_,i64>(0)).unwrap(),1);
    l.s.fail_read.store(false,Ordering::Release);let reopened=l.workers();
    let (stop,rx)=tokio::sync::watch::channel(false);let task=tokio::spawn(reopened.clone().watch(gate,rx));
    wait_db(&l,"SELECT count(*) FROM notices WHERE kind='monitor'",2).await;
    stop.send(true).unwrap();task.await.unwrap();assert_eq!(reopened.active().unwrap().len(),1);
    assert_eq!(adb.query_row("SELECT count(*) FROM receipts WHERE body LIKE '%Worker monitoring recovered%'",[],|r|r.get::<_,i64>(0)).unwrap(),1);
}
#[tokio::test]
async fn failed_wait_and_final_collection_are_visible_and_retry_retains_the_final() {
    let l=Lab::new(true).await;let w=l.workers();let gate=Arc::new(DeliveryGate::new(true));l.start(&w,&gate.permit().unwrap()).await;
    l.s.fail_wait.store(true,Ordering::Release);
    let (stop,rx)=tokio::sync::watch::channel(false);let task=tokio::spawn(w.clone().watch(gate.clone(),rx));
    wait_db(&l,"SELECT count(*) FROM notices WHERE kind='monitor'",1).await;stop.send(true).unwrap();task.await.unwrap();
    l.s.fail_wait.store(false,Ordering::Release);l.s.fail_read.store(true,Ordering::Release);l.s.terminal.store(true,Ordering::Release);
    let (stop,rx)=tokio::sync::watch::channel(false);let task=tokio::spawn(w.clone().watch(gate.clone(),rx));tokio::time::sleep(Duration::from_millis(40)).await;stop.send(true).unwrap();task.await.unwrap();
    let db=rusqlite::Connection::open(l.dir.path().join("workers.db")).unwrap();assert_eq!(db.query_row("SELECT count(*) FROM final_items",[],|r|r.get::<_,i64>(0)).unwrap(),0);
    l.s.fail_read.store(false,Ordering::Release);
    let (stop,rx)=tokio::sync::watch::channel(false);let task=tokio::spawn(w.clone().watch(gate,rx));wait_db(&l,"SELECT count(*) FROM notices WHERE kind='terminal'",1).await;wait_db(&l,"SELECT count(*) FROM notices WHERE kind='monitor'",2).await;
    stop.send(true).unwrap();task.await.unwrap();assert_eq!(db.query_row("SELECT body FROM final_items",[],|r|r.get::<_,String>(0)).unwrap(),l.s.text);
}
/// Bug note 18-24-56: a failed item in a running task wakes nothing; the cursor still passes the first page and an
/// open sub-agent item is re-read by id until it closes.
#[tokio::test]
async fn activity_cursor_passes_pages_rereads_open_sub_agents_and_failures_wake_nothing() {
    let l=Lab::new(true).await;let w=l.workers();let gate=Arc::new(DeliveryGate::new(true));l.start(&w,&gate.permit().unwrap()).await;
    *l.s.activity.lock().unwrap()=Some((0..60).map(|i|json!({"itemId":format!("item-{i}"),"runId":"run-1","position":i,"type":if i==1{"subagent"}else{"command_execution"},"title":"Helper","status":if i==12{"failed"}else if i==1{"running"}else{"completed"},"text":"activity"})).collect());
    let (stop,rx)=tokio::sync::watch::channel(false);let task=tokio::spawn(w.clone().watch(gate.clone(),rx));wait_db(&l,"SELECT coalesce(activity_cursor,-1) FROM workers WHERE thread='owned-thread'",59).await;stop.send(true).unwrap();task.await.unwrap();
    wait_db(&l,"SELECT count(*) FROM children WHERE id='item-1'",1).await;
    l.s.activity.lock().unwrap().as_mut().unwrap()[1]["status"]=json!("completed");
    let (stop,rx)=tokio::sync::watch::channel(false);let task=tokio::spawn(w.clone().watch(gate,rx));wait_db(&l,"SELECT count(*) FROM children WHERE id='item-1'",0).await;
    stop.send(true).unwrap();task.await.unwrap();
    let calls=l.s.calls.lock().unwrap();assert!(calls.iter().any(|(n,a)|n=="t3_thread_read"&&a["afterPosition"]==49));assert!(calls.iter().any(|(n,a)|n=="t3_thread_read"&&a["itemId"]=="item-1"));
    let db=rusqlite::Connection::open(l.dir.path().join("workers.db")).unwrap();
    assert_eq!(db.query_row("SELECT count(*) FROM notices WHERE id LIKE 'stuck:%'",[],|r|r.get::<_,i64>(0)).unwrap(),0,"a failed tool call is no notice");
}
#[tokio::test]
async fn completed_worker_operation_payloads_retire_after_archival_and_replay_still_deduplicates() {
    let l=Lab::new(true).await;let w=l.workers();let gate=Arc::new(DeliveryGate::new(true));let p=gate.permit().unwrap();l.start(&w,&p).await;
    let saved=Arc::new(Mutex::new(vec![]));let storage=saved.clone();w.actions.attach_archive(Arc::new(move|v|{storage.lock().unwrap().push(v.clone());Ok("saved".into())}));
    let args=json!({"threadId":"owned-thread","message":"Large prepared worker message"});let first=w.execute("t3_thread_send",args.clone(),"send-retire",&p).await.unwrap();w.retire().unwrap();
    assert_eq!(w.execute("t3_thread_send",args,"send-retire",&p).await.unwrap(),first);
    assert_eq!(l.s.calls.lock().unwrap().iter().filter(|(n,_)|n=="t3_thread_send").count(),1);assert!(saved.lock().unwrap().iter().any(|v|v["args"].as_str().is_some_and(|s|s.contains("Large prepared"))));
    let db=rusqlite::Connection::open(l.dir.path().join("workers.db")).unwrap();assert!(db.query_row("SELECT args FROM operations WHERE id='send-retire'",[],|r|r.get::<_,String>(0)).unwrap().starts_with("sha256:"));
}

#[tokio::test]
async fn transport_capability_and_registry_failures_reach_the_island_once_and_never_the_agent() {
    for failure in ["transport","capability","registry"] {
        let l=Lab::new(true).await;let w=l.workers();let gate=Arc::new(DeliveryGate::new(true));l.start(&w,&gate.permit().unwrap()).await;
        let db=rusqlite::Connection::open(l.dir.path().join("workers.db")).unwrap();
        match failure {"transport"=>l.s.transport_failed.store(true,Ordering::Release),"capability"=>{l.s.missing_capability.store(true,Ordering::Release);l.client.invalidate().await;},_=>{db.execute_batch("ALTER TABLE workers RENAME TO unavailable_workers").unwrap();}}
        let (stop,rx)=tokio::sync::watch::channel(false);let task=tokio::spawn(w.clone().watch(gate.clone(),rx));
        let adb=rusqlite::Connection::open(l.dir.path().join("actions.db")).unwrap();let start=std::time::Instant::now();
        loop {let n:i64=adb.query_row("SELECT count(*) FROM receipts WHERE body LIKE '%Worker monitoring unavailable%'",[],|r|r.get(0)).unwrap();if n==1{break;}assert!(start.elapsed()<Duration::from_secs(3),"{failure} notice missing");tokio::task::yield_now().await;}
        assert_eq!(adb.query_row("SELECT count(*) FROM outbox WHERE id LIKE 'monitor:%'",[],|r|r.get::<_,i64>(0)).unwrap(),0,"{failure}: monitoring reached the agent outbox");
        stop.send(true).unwrap();task.await.unwrap();
        match failure {"transport"=>l.s.transport_failed.store(false,Ordering::Release),"capability"=>{l.s.missing_capability.store(false,Ordering::Release);l.client.invalidate().await;},_=>{db.execute_batch("ALTER TABLE unavailable_workers RENAME TO workers").unwrap();}}
        let (stop,rx)=tokio::sync::watch::channel(false);let task=tokio::spawn(w.clone().watch(gate,rx));wait_db(&l,"SELECT count(*) FROM notices WHERE id LIKE 'monitor:%:recovered'",1).await;stop.send(true).unwrap();task.await.unwrap();assert_eq!(w.active().unwrap().len(),1);
        assert_eq!(adb.query_row("SELECT count(*) FROM receipts WHERE body LIKE '%Worker monitoring unavailable%'",[],|r|r.get::<_,i64>(0)).unwrap(),1);
    }
}

#[tokio::test]
async fn archived_and_superseded_failure_keys_recover_but_pending_finals_stay() {
    let l=Lab::new(true).await;let w=l.workers();let gate=Arc::new(DeliveryGate::new(true));l.start(&w,&gate.permit().unwrap()).await;
    w.actions.monitor_failure("owned-thread:old-run",true).unwrap();
    w.actions.monitor_failure("archived-thread:run-archived",true).unwrap();
    w.actions.monitor_failure("owned-thread:pending-run",true).unwrap();
    w.actions.set_monitor(true,1).unwrap();
    let db=rusqlite::Connection::open(l.dir.path().join("workers.db")).unwrap();
    db.execute("INSERT INTO pending_finals VALUES('owned-thread','pending-run','completed')",[]).unwrap();
    // Preserve a genuinely pending collection; failing collection must not clear it.
    l.s.fail_read.store(true,Ordering::Release);
    let (stop,rx)=tokio::sync::watch::channel(false);let task=tokio::spawn(w.clone().watch(gate.clone(),rx));
    wait_db(&l,"SELECT count(*) FROM notices WHERE kind='monitor'",1).await;stop.send(true).unwrap();task.await.unwrap();
    let adb=rusqlite::Connection::open(l.dir.path().join("actions.db")).unwrap();
    let keys:Vec<String>=adb.prepare("SELECT id FROM monitor_failures").unwrap().query_map([],|r|r.get(0)).unwrap().collect::<rusqlite::Result<_>>().unwrap();
    assert!(!keys.contains(&"owned-thread:old-run".into()),"superseded failure never retired");
    assert!(!keys.contains(&"archived-thread:run-archived".into()),"archived failure never retired");
    assert!(keys.contains(&"owned-thread:pending-run".into()));
    l.s.fail_read.store(false,Ordering::Release);
    let (stop,rx)=tokio::sync::watch::channel(false);let task=tokio::spawn(w.clone().watch(gate,rx));
    wait_db(&l,"SELECT count(*) FROM notices WHERE id LIKE 'monitor:%:recovered'",1).await;stop.send(true).unwrap();task.await.unwrap();assert!(!w.actions.monitor_failed().unwrap());
}
#[tokio::test]
async fn settled_finals_retire_for_live_threads_but_pending_collection_keeps_payload() {
    let l=Lab::new(true).await;let w=l.workers();let gate=Arc::new(DeliveryGate::new(true));l.start(&w,&gate.permit().unwrap()).await;
    let db=rusqlite::Connection::open(l.dir.path().join("workers.db")).unwrap();
    db.execute_batch("INSERT INTO final_items VALUES('owned-thread','settled','item','large final body'); INSERT INTO notices VALUES('terminal:owned-thread:settled:completed','owned-thread','settled','terminal'); INSERT INTO final_items VALUES('owned-thread','pending','partial','keep pending body'); INSERT INTO pending_finals VALUES('owned-thread','pending','completed');").unwrap();
    let saved=Arc::new(Mutex::new(vec![]));let storage=saved.clone();w.actions.attach_archive(Arc::new(move|v|{storage.lock().unwrap().push(v.clone());Ok("saved".into())}));
    w.retire().unwrap();
    let body:String=db.query_row("SELECT body FROM final_items WHERE run='settled'",[],|r|r.get(0)).unwrap();
    assert!(body.starts_with("archive:"),"settled run retained payload on live thread");
    assert_eq!(db.query_row("SELECT body FROM final_items WHERE run='pending'",[],|r|r.get::<_,String>(0)).unwrap(),"keep pending body");
    w.retire().unwrap();assert_eq!(saved.lock().unwrap().iter().filter(|v|v["store"]=="worker_final").count(),1);
}

/// tool-output ("the first request that you did to like T3 workers like results what the fuck is that
/// it's just a wall of useless text"): every task tool answers in plain lines; the catalog is one line per model.
#[tokio::test]
async fn task_tools_answer_in_plain_lines_and_the_real_catalog_is_compact() {
    let l = Lab::new(true).await;
    let fixture: Value = serde_json::from_str(include_str!("fixtures/t3-catalog-19oct.json")).unwrap();
    let mut caps = fixture["capabilities"].clone();
    // A provider without full-access is named once, not listed as a choice.
    caps["providers"].as_array_mut().unwrap().push(json!({"providerInstanceId":"cursor","driverKind":"cursor","runtimeModes":["approval-required"],"models":[{"id":"cursor-x","options":[]}]}));
    *l.s.catalog.lock().unwrap() = caps;
    let w = l.workers();
    let p = Arc::new(DeliveryGate::new(true)).permit().unwrap();
    let catalog = w.execute("t3_call", json!({"tool":"orchestrator_capabilities","args":{}}), "cat", &p).await.unwrap();
    let before = fixture["before"].as_str().unwrap();
    println!("catalog before: {} bytes for 17 models (first page of two); after: {} bytes for all 17", before.len(), catalog.len());
    assert_eq!(catalog, "provider model — options (* default):
codex gpt-6-luna — reasoningEffort low|medium*|high|xhigh|max
codex gpt-5.6-terra — reasoningEffort low|medium*|high|xhigh|max|ultra
codex gpt-5.6-luna — reasoningEffort low|medium*|high|xhigh|max
claudeAgent claude-opus-5-5 — effort low|medium*|high|xhigh|max|ultracode|ultrathink, fastMode true|false
claudeAgent claude-sonnet-5-5 — effort low|medium|high*|xhigh|max|ultrathink
claudeAgent claude-fable-5-1 — effort low|medium*|high|xhigh|max|ultracode|ultrathink
claudeAgent claude-fable-5 — effort low|medium*|high|xhigh|max|ultracode|ultrathink
claudeAgent claude-opus-5 — effort low|medium|high*|xhigh|max|ultracode|ultrathink, fastMode true|false
claudeAgent claude-opus-4-8 — effort low|medium|high*|xhigh|max|ultracode|ultrathink, fastMode true|false
claudeAgent claude-opus-4-7 — effort low|medium|high|xhigh*|max|ultrathink, fastMode true|false
claudeAgent claude-opus-4-6 — effort low|medium|high*|max|ultrathink, fastMode true|false, contextWindow 200k|1m*
claudeAgent claude-opus-4-5 — effort low|medium|high*|max, fastMode true|false
claudeAgent claude-sonnet-5 — effort low|medium|high*|xhigh|max|ultrathink
claudeAgent claude-sonnet-4-6 — effort low|medium|high*|max|ultrathink, contextWindow 200k*|1m
claudeAgent claude-haiku-5-5 — effort low|medium*|high|xhigh|max|ultrathink
claudeAgent claude-haiku-4-5 — thinking true|false
grok grok-build
Not usable (no full-access mode): cursor.");
    assert!(catalog.len() * 4 < before.len(), "catalog is not compact: {} vs {} bytes", catalog.len(), before.len());

    // Tasks: none yet, then one line per task, newest first.
    *l.s.catalog.lock().unwrap() = Fake::capabilities();
    assert_eq!(w.execute("task_status", json!({}), "list-0", &p).await.unwrap(), "No open tasks.");
    let started = w.execute("delegate_task", json!({"path":l.dir.path(),"target":{"model":"test-model","options":{"reasoningEffort":"high"}},"title":"Haiku reply test","task":"Never billed"}), "launch", &p).await.unwrap();
    assert_eq!(started, "Started 'Haiku reply test' as task owned-thread (test-model, reasoningEffort high). Its result arrives as a notice when it is done; end your turn.");
    assert_eq!(w.execute("task_status", json!({}), "list-1", &p).await.unwrap(), "taskId · title · state · model:\nowned-thread · 'Haiku reply test' · working · test-model");
    let sent = w.execute("t3_thread_send", json!({"threadId":"owned-thread","message":"go on"}), "send", &p).await.unwrap();
    assert_eq!(sent, "Sent to thread owned-thread (steered, running). Its reply arrives as a notice when this run ends.");
    // A replay answers from the durable record in the same words.
    assert_eq!(w.execute("t3_thread_send", json!({"threadId":"owned-thread","message":"go on"}), "send", &p).await.unwrap(), sent);
    rusqlite::Connection::open(l.dir.path().join("workers.db")).unwrap().execute("UPDATE workers SET archived=1", []).unwrap();
    assert_eq!(w.execute("task_status", json!({}), "list-2", &p).await.unwrap(), "No open tasks.");
    for out in [&catalog, &started, &sent] {
        assert!(!out.contains('{'), "raw JSON: {out}");
    }
}

/// `task_status {taskId}` is the latest reply in full even across T3's text pages; a reply above the read budget
/// continues with `t3_thread_read {itemId, textOffset}` instead of being cut, and the pages join exactly.
#[tokio::test]
async fn task_status_returns_the_latest_reply_whole_and_continues_a_huge_one() {
    // 540 KB of paragraphs with Polish letters and emoji, above the 64 KB read budget (12 UTF-16 units per
    // repeat, so the fake's 3-unit pages never split the emoji).
    let l = Lab::with_text(true, "😀ż Łódź!\n\n\n".repeat(30_000)).await;
    let w = l.workers();
    let p = Arc::new(DeliveryGate::new(true)).permit().unwrap();
    l.start(&w, &p).await;
    let first = w.execute("task_status", json!({"taskId":"owned-thread"}), "r1", &p).await.unwrap();
    let (head, rest) = first.split_once("\nLatest reply:\n").unwrap();
    assert!(head.starts_with("'Test' · working"), "{head}");
    let (body, more) = rest.rsplit_once("\nMore: t3_thread_read ").expect("a continuation");
    assert!(more.contains("itemId: \"item-1\", view: \"messages\", textOffset: "), "{more}");
    let offset: u64 = more.rsplit("textOffset: ").next().unwrap().trim_end_matches('}').parse().unwrap();
    let second = w.execute("t3_thread_read", json!({"threadId":"owned-thread","itemId":"item-1","view":"messages","textOffset":offset}), "r2", &p).await.unwrap();
    let tail = second.split_once("assistant message (running):\n").unwrap().1;
    let tail = tail.split("\nMore: t3_thread_read ").next().unwrap();
    assert!(l.s.text.starts_with(&format!("{body}{tail}")), "pages join exactly");
    assert!(body.len() >= 64 * 1024);
}

/// Heimdall's Claude permission hook answers only threads Heimdall launched (`heimdall_emergency::approvals`): every
/// launch records its first message's digest, exactly the text T3 starts the thread with, before the launch call.
#[tokio::test]
async fn every_launch_is_recorded_for_the_approval_hook_first() {
    let l = Lab::new(true).await;
    let data = l.dir.path().join("heimdall-data");
    let client = Arc::new(Client::new(l.dir.path().join("runtime.json"), l.store.clone()).unwrap().with_launch_record(data.clone()));
    let actions = Arc::new(Actions::open(&l.dir.path().join("actions-record.db")).unwrap());
    actions.attach(Arc::new(Sink::default()));
    let w = Arc::new(Workers::open(&l.dir.path().join("workers-record.db"), client.clone(), actions).unwrap());
    w.set_brief_preamble("# Heimdall task\n\nRules.");
    let p = Arc::new(DeliveryGate::new(true)).permit().unwrap();
    l.start(&w, &p).await;
    let sent = l.s.calls.lock().unwrap().iter().find(|(n, _)| n == "t3_thread_launch").unwrap().1["message"].as_str().unwrap().to_owned();
    assert!(sent.starts_with("# Heimdall task") && sent.contains("Never billed"), "{sent}");
    assert!(heimdall_emergency::approvals::launched(&data, &heimdall_emergency::approvals::digest(&sent)));
    // As Claude receives it: T3 trims the message.
    assert!(heimdall_emergency::approvals::launched(&data, &heimdall_emergency::approvals::digest(sent.trim())));
    assert!(!heimdall_emergency::approvals::launched(&data, &heimdall_emergency::approvals::digest("User's own thread")));
    // The new thread's id too, once T3 answers (bug note 2026-11-02T14-56-51-2): its `delegate_task` children are
    // Heimdall's for the hook.
    use heimdall_emergency::approvals::heimdall_thread;
    assert!(heimdall_thread(&data, "owned-thread"));
    assert!(heimdall_thread(&data, "thread:delegated-task:command%3Aowned-thread%3Adelegate-task%3Ab1"));
    assert!(!heimdall_thread(&data, "thread:delegated-task:command%3Aowner-thread%3Adelegate-task%3Ab1"));
    // Without a record folder (tests, lab clients) nothing is written.
    assert!(!l.dir.path().join(heimdall_emergency::approvals::LAUNCHES).exists());
}
