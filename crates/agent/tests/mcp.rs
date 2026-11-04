use agent::{
    mcp::{McpServer, ScreenshotResult, ToolHost, UnavailableArchive},
    protocol::Attachment,
};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
struct Host(AtomicUsize);
#[async_trait]
impl ToolHost for Host {
    fn speak(&self, _: &str, _: &str, _: &agent::delivery::DeliveryPermit) -> Result<u64> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(42)
    }
    async fn screen_frames(&self, _: Value, _: &agent::delivery::DeliveryPermit) -> Result<Value> {
        Ok(ScreenshotResult {
            images: (1..=4)
                .map(|id| Attachment {
                    media_type: "image/png".into(),
                    bytes: vec![id; 3],
                })
                .collect(),
            warnings: vec!["test supplemental failure".into()],
            notes: vec![],
        }
        .into_value()
        .unwrap())
    }
}
async fn rpc(server: &McpServer, value: Value, auth: bool) -> String {
    rpc_at(&server.url, &server.token, value, auth).await
}
async fn rpc_at(url: &str, token: &str, value: Value, auth: bool) -> String {
    let address = url.trim_start_matches("http://").trim_end_matches("/mcp");
    let mut socket = tokio::net::TcpStream::connect(address).await.unwrap();
    let body = value.to_string();
    let authorization = if auth {
        format!("Authorization: Bearer {token}\r\n")
    } else {
        String::new()
    };
    let request=format!("POST /mcp HTTP/1.1\r\nHost: {address}\r\nAccept: application/json, text/event-stream\r\nMCP-Protocol-Version: 2025-06-18\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{authorization}\r\n{body}",body.len());
    socket.write_all(request.as_bytes()).await.unwrap();
    let mut reply = String::new();
    socket.read_to_string(&mut reply).await.unwrap();
    reply
}
#[tokio::test]
async fn real_loopback_mcp_tools_and_auth() {
    let host = Arc::new(Host(AtomicUsize::new(0)));
    let server = McpServer::start(
        host.clone(),
        Arc::new(UnavailableArchive),
        Arc::new(agent::delivery::DeliveryGate::new(true)),
    )
    .await
    .unwrap();
    let unauthorized = rpc(
        &server,
        json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
        false,
    )
    .await;
    assert!(unauthorized.starts_with("HTTP/1.1 401"));
    let list = rpc(
        &server,
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
        true,
    )
    .await;
    assert!(
        list.contains("timeline_search")
            && list.contains("speak")
            && list.contains("screen_frames")
    );
    let speech=rpc(&server,json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"speak","arguments":{"text":"hello","language":"pl"}}}),true).await;
    assert!(speech.contains("Speech queued: 42"));
    assert_eq!(host.0.load(Ordering::SeqCst), 1);
    let archive=rpc(&server,json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"timeline_search","arguments":{"query":"hello"}}}),true).await;
    assert!(archive.contains("not available") && archive.contains("isError"));
    let screen=rpc(&server,json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"screen_frames","arguments":{}}}),true).await;
    let response: Value = serde_json::from_str(screen.split("\r\n\r\n").nth(1).unwrap()).unwrap();
    let content = response["result"]["content"].as_array().unwrap();
    assert_eq!(content.iter().filter(|b| b["type"] == "image").count(), 4);
    assert_eq!(content[0]["data"], "AQEB");
    assert_eq!(content[3]["data"], "BAQE");
    assert!(content[4]["text"]
        .as_str()
        .unwrap()
        .contains("test supplemental failure"));
    assert!(response["result"].get("isError").is_none());
    server.shutdown().await;
}

struct PendingHost {
    started: Arc<tokio::sync::Notify>,
    dropped: Arc<AtomicUsize>,
}
struct DropSignal(Arc<AtomicUsize>);
impl Drop for DropSignal {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
#[async_trait]
impl ToolHost for PendingHost {
    fn speak(&self, _: &str, _: &str, _: &agent::delivery::DeliveryPermit) -> Result<u64> {
        anyhow::bail!("unexpected speak")
    }
    async fn screen_frames(&self, _: Value, _: &agent::delivery::DeliveryPermit) -> Result<Value> {
        let _drop = DropSignal(self.dropped.clone());
        self.started.notify_one();
        std::future::pending().await
    }
}
#[async_trait]
impl agent::mcp::TranscriptSearch for PendingHost {
    async fn timeline(&self, _: Value, _: &agent::delivery::DeliveryPermit) -> Result<Value> {
        let _drop = DropSignal(self.dropped.clone());
        self.started.notify_one();
        std::future::pending().await
    }
}
#[tokio::test]
async fn off_blocks_every_tool_and_off_on_cancels_pending_operations() {
    let gate = Arc::new(agent::delivery::DeliveryGate::new(false));
    let host = Arc::new(Host(AtomicUsize::new(0)));
    let server = McpServer::start(host.clone(), Arc::new(UnavailableArchive), gate.clone())
        .await
        .unwrap();
    for (name, args) in [
        ("speak", json!({"text":"private speech"})),
        ("screen_frames", json!({})),
        ("timeline_search", json!({"query":"private"})),
    ] {
        let reply = rpc(&server, json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":args}}), true).await;
        assert!(
            reply.starts_with("HTTP/1.1 503"),
            "Off returns no MCP result: {reply}"
        );
        assert!(!reply.contains("Speech queued") && !reply.contains("image/png"));
    }
    assert_eq!(host.0.load(Ordering::SeqCst), 0);
    server.shutdown().await;
    for name in ["screen_frames", "timeline_search"] {
        gate.store(true, Ordering::Release);
        let host = Arc::new(PendingHost {
            started: Arc::new(tokio::sync::Notify::new()),
            dropped: Arc::new(AtomicUsize::new(0)),
        });
        let server = Arc::new(
            McpServer::start(host.clone(), host.clone(), gate.clone())
                .await
                .unwrap(),
        );
        let s = server.clone();
        let request = tokio::spawn(async move {
            rpc(&s, json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":name,"arguments":if name=="screen_frames"{json!({})}else{json!({"query":"private"})}}}), true).await
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), host.started.notified())
            .await
            .unwrap();
        gate.store(false, Ordering::Release);
        gate.store(true, Ordering::Release);
        let reply = tokio::time::timeout(std::time::Duration::from_secs(2), request)
            .await
            .unwrap()
            .unwrap();
        assert!(!reply.contains("private") && !reply.contains("image/png"));
        assert_eq!(host.dropped.load(Ordering::SeqCst), 1);
        drop(server);
    }
}

struct RetiringArchive {
    started: tokio::sync::Notify,
    release: tokio::sync::Notify,
    dropped: Arc<AtomicUsize>,
    completed: AtomicUsize,
}
#[async_trait]
impl agent::mcp::TranscriptSearch for RetiringArchive {
    async fn timeline(&self, _: Value, permit: &agent::delivery::DeliveryPermit) -> Result<Value> {
        let _drop = DropSignal(self.dropped.clone());
        self.started.notify_one();
        self.release.notified().await;
        permit.with(|| {
            self.completed.fetch_add(1, Ordering::SeqCst);
            Ok(json!({"marker":"retired-server-private-result"}))
        })
    }
}
#[async_trait]
impl ToolHost for RetiringArchive {
    fn speak(&self, _: &str, _: &str, _: &agent::delivery::DeliveryPermit) -> Result<u64> {
        Ok(42)
    }
    async fn screen_frames(
        &self,
        _: Value,
        permit: &agent::delivery::DeliveryPermit,
    ) -> Result<Value> {
        let _drop = DropSignal(self.dropped.clone());
        self.started.notify_one();
        self.release.notified().await;
        permit.with(|| {
            self.completed.fetch_add(1, Ordering::SeqCst);
            Ok(ScreenshotResult {
                images: vec![Attachment {
                    media_type: "image/png".into(),
                    bytes: b"retired-server-private-result".to_vec(),
                }],
                warnings: vec![],
                notes: vec![],
            }
            .into_value()?)
        })
    }
}
#[tokio::test]
async fn retirement_joins_accepted_request_and_never_delivers_private_result_while_agent_on() {
    for name in ["timeline_search", "screen_frames"] {
        let gate = Arc::new(agent::delivery::DeliveryGate::new(true));
        let archive = Arc::new(RetiringArchive {
            started: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
            dropped: Arc::new(AtomicUsize::new(0)),
            completed: AtomicUsize::new(0),
        });
        let server = McpServer::start(archive.clone(), archive.clone(), gate.clone())
            .await
            .unwrap();
        let url = server.url.clone();
        let token = server.token.clone();
        let request = tokio::spawn(async move {
            rpc_at(&url,&token,json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":name,"arguments":if name=="screen_frames"{json!({})}else{json!({"query":"private"})}}}),true).await
        });
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            archive.started.notified(),
        )
        .await
        .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), server.shutdown())
            .await
            .unwrap();
        // Shutdown must have dropped the accepted operation before we release it.
        let dropped_at_shutdown = archive.dropped.load(Ordering::SeqCst);
        assert!(gate.load(Ordering::Acquire));
        archive.release.notify_one();
        let reply = tokio::time::timeout(std::time::Duration::from_secs(2), request)
            .await
            .unwrap()
            .unwrap();
        let delivered =
            reply.contains("retired-server-private-result") || reply.contains("image/png");
        println!("{name}: retired_server_delivered_private_result={delivered} accepted_operation_joined={} agent_still_on=true", dropped_at_shutdown == 1);
        assert!(!delivered);
        assert_eq!(dropped_at_shutdown, 1);
        assert_eq!(archive.completed.load(Ordering::SeqCst), 0);
        // A new server on the same Agent epoch remains usable.
        let next = McpServer::start(
            Arc::new(Host(AtomicUsize::new(0))),
            Arc::new(UnavailableArchive),
            gate,
        )
        .await
        .unwrap();
        assert!(rpc(&next,json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"speak","arguments":{"text":"new server"}}}),true).await.contains("Speech queued: 42"));
        next.shutdown().await;
    }
}

#[tokio::test]
async fn retirement_joins_idle_and_incomplete_http_connections() {
    let server = McpServer::start(
        Arc::new(Host(AtomicUsize::new(0))),
        Arc::new(UnavailableArchive),
        Arc::new(agent::delivery::DeliveryGate::new(true)),
    )
    .await
    .unwrap();
    let address = server
        .url
        .trim_start_matches("http://")
        .trim_end_matches("/mcp");
    let mut socket = tokio::net::TcpStream::connect(address).await.unwrap();
    socket
        .write_all(b"POST /mcp HTTP/1.1\r\nHost: localhost\r\nContent-Length: 99999\r\n")
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), server.shutdown())
        .await
        .unwrap();
    let mut reply = Vec::new();
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        socket.read_to_end(&mut reply),
    )
    .await
    .unwrap();
    assert!(reply.is_empty());
}

async fn raw(server: &McpServer, body: &str, extra: &str) -> String {
    let address = server
        .url
        .trim_start_matches("http://")
        .trim_end_matches("/mcp");
    let mut socket = tokio::net::TcpStream::connect(address).await.unwrap();
    let req=format!("POST /mcp HTTP/1.1\r\nHost: {address}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\nMCP-Protocol-Version: 2025-06-18\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n{body}",server.token,body.len());
    socket.write_all(req.as_bytes()).await.unwrap();
    let mut reply = String::new();
    socket.read_to_string(&mut reply).await.unwrap();
    reply
}
#[tokio::test]
async fn initialization_origin_protocol_envelopes_notifications_and_unknown_tools() {
    let server = McpServer::start(
        Arc::new(Host(AtomicUsize::new(0))),
        Arc::new(UnavailableArchive),
        Arc::new(agent::delivery::DeliveryGate::new(true)),
    )
    .await
    .unwrap();
    let init = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"1"}}});
    assert!(rpc(&server, init.clone(), true)
        .await
        .contains("2025-06-18"));
    let mut wrong = init;
    wrong["params"]["protocolVersion"] = json!("2026-07-28");
    assert!(rpc(&server, wrong, true).await.contains("-32602"));
    assert!(raw(&server, "{", "").await.contains("-32700"));
    assert!(raw(&server, "{}", "").await.contains("-32600"));
    assert!(raw(&server, "{}", "Origin: https://evil.test\r\n")
        .await
        .starts_with("HTTP/1.1 403"));
    assert!(rpc(
        &server,
        json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}),
        true
    )
    .await
    .starts_with("HTTP/1.1 202"));
    assert!(
        rpc(&server, json!({"jsonrpc":"2.0","method":"arbitrary"}), true)
            .await
            .starts_with("HTTP/1.1 400")
    );
    let unknown=rpc(&server,json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"bogus","arguments":{}}}),true).await;
    assert!(unknown.contains("-32602") && !unknown.contains("isError"));
    let wrong=rpc(&server,json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"speak","arguments":{"text":"hi","language":42}}}),true).await;
    assert!(wrong.contains("INVALID_ARGUMENT") && wrong.contains("isError"));
    server.shutdown().await;
}

#[tokio::test]
async fn pinned_pi_initialized_notification_may_omit_params() {
    let server=McpServer::start(Arc::new(Host(AtomicUsize::new(0))),Arc::new(UnavailableArchive),Arc::new(agent::delivery::DeliveryGate::new(true))).await.unwrap();
    let reply=rpc(&server,json!({"jsonrpc":"2.0","method":"notifications/initialized"}),true).await;
    assert!(reply.starts_with("HTTP/1.1 202"),"{reply}");
    let malformed=rpc(&server,json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{"unexpected":1}}),true).await;
    assert!(malformed.starts_with("HTTP/1.1 400"),"{malformed}");
    server.shutdown().await;
}

/// tool-output ("The amount of just useless fucking JSON that is just pasted into you is absolutely
/// ridiculous"): whatever a tool's owner returns, the model reads plain lines; text content is never shortened.
struct JsonHost;
#[async_trait]
impl ToolHost for JsonHost {
    fn speak(&self, _: &str, _: &str, _: &agent::delivery::DeliveryPermit) -> Result<u64> {
        Ok(1)
    }
    async fn action(&self, _: &str, _: Value, _: &str, _: &agent::delivery::DeliveryPermit) -> Result<String> {
        Ok(json!({"task_id":"w-1","status":"archived","run_id":null,"text":"Zażółć 😀\n\nsecond paragraph"}).to_string())
    }
    async fn screen_frames(&self, _: Value, _: &agent::delivery::DeliveryPermit) -> Result<Value> {
        let meta = json!({"frame_id":"screen:1:2:3","capture":"2026-10-19T10:30:07Z","display_id":"1","width":2,"height":1,"quality":"preview","archived":false});
        Ok(json!({"content":[{"type":"text","text":meta.to_string()},{"type":"image","data":"AQEB","mimeType":"image/jpeg"}]}))
    }
}
struct JsonArchive;
#[async_trait]
impl agent::mcp::TranscriptSearch for JsonArchive {
    async fn timeline(&self, _: Value, _: &agent::delivery::DeliveryPermit) -> Result<Value> {
        Ok(json!({"search_mode":"literal","anchor":"9","hits":[{"h":"h1","start":"2026-10-19T10:30:07Z","label":"Mic · User","text":"the chart is wrong"}],"next_cursor":"c1"}))
    }
}
#[tokio::test]
async fn every_tool_result_reaches_the_model_as_plain_lines() {
    let server = McpServer::start(Arc::new(JsonHost), Arc::new(JsonArchive), Arc::new(agent::delivery::DeliveryGate::new(true))).await.unwrap();
    let call = |id: u64, name: &str, args: Value| json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":name,"arguments":args}});
    let content = |reply: String| -> Value {
        let v: Value = serde_json::from_str(reply.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        v["result"]["content"].clone()
    };
    let archived = content(rpc(&server, call(1, "task_cancel", json!({"taskId":"w-1"})), true).await);
    assert_eq!(archived[0]["text"], "status: archived\ntask_id: w-1\ntext:\n  Zażółć 😀\n  \n  second paragraph");
    let found = content(rpc(&server, call(2, "timeline_search", json!({"query":"chart"})), true).await);
    let found = found[0]["text"].as_str().unwrap();
    assert!(found.starts_with("19 Oct "), "{found}");
    assert!(found.ends_with(" · Mic · User · [h1] the chart is wrong\nMore: cursor c1"), "{found}");
    let frames = content(rpc(&server, call(3, "screen_frames", json!({})), true).await);
    assert!(frames[0]["text"].as_str().unwrap().starts_with("Frame screen:1:2:3 · captured 2026-10-19T"), "{frames}");
    assert_eq!(frames[1]["type"], "image");
    for block in [archived[0]["text"].as_str().unwrap(), found, frames[0]["text"].as_str().unwrap()] {
        assert!(!block.contains("\":") && !block.starts_with('{'), "raw JSON reached the model: {block}");
    }
    server.shutdown().await;
}
/// Writes its state in two permit-checked steps (like `plan_edit` or `bug_note`: receipt, then the write) and turns Agent off between
/// them, as the user's click can land mid-call.
struct OffMidWrite {
    gate: Arc<agent::delivery::DeliveryGate>,
    steps: std::sync::Mutex<Vec<String>>,
}
#[async_trait]
impl ToolHost for OffMidWrite {
    fn speak(&self, _: &str, _: &str, _: &agent::delivery::DeliveryPermit) -> Result<u64> {
        anyhow::bail!("unexpected speak")
    }
    async fn action(&self, name: &str, _: Value, _: &str, p: &agent::delivery::DeliveryPermit) -> Result<String> {
        p.with(|| { self.steps.lock().unwrap().push(format!("{name}:receipt")); Ok(()) })?;
        self.gate.intent(agent::delivery::StopReason::Agent, false);
        p.with(|| { self.steps.lock().unwrap().push(format!("{name}:saved")); Ok(()) })?;
        Ok("saved".into())
    }
}
/// Agent off lets a tool call that writes state finish; anything else stops at once. The write's
/// answer is still withheld after Off, like every result; a call that is not a state write is cut between its steps.
#[tokio::test]
async fn agent_off_mid_call_lets_a_state_write_finish_and_cuts_anything_else() {
    for (name, args, finishes) in [("plan_edit", json!({"action":"add","item":"Menu: froze"}), true), ("task_cancel", json!({"taskId":"w-1"}), false)] {
        let gate = Arc::new(agent::delivery::DeliveryGate::new(true));
        let host = Arc::new(OffMidWrite { gate: gate.clone(), steps: Default::default() });
        let server = McpServer::start(host.clone(), Arc::new(UnavailableArchive), gate.clone()).await.unwrap();
        assert_eq!(agent::mcp_contract::writes_state(name), finishes);
        let _ = rpc(&server, json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":args}}), true).await;
        let steps = host.steps.lock().unwrap().clone();
        assert_eq!(steps.len(), if finishes { 2 } else { 1 }, "{name}: {steps:?}");
        assert_eq!(gate.writes_in_flight(), 0, "{name}: the write ended");
        assert!(gate.stopped_for(agent::delivery::StopReason::Agent));
        // After Off nothing starts, a state write included.
        let refused = rpc(&server, json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":name,"arguments":args}}), true).await;
        assert!(refused.starts_with("HTTP/1.1 503"), "{refused}");
        assert_eq!(host.steps.lock().unwrap().len(), steps.len());
        server.shutdown().await;
    }
}
