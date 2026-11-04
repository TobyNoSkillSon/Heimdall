//! The scoped MCP server of emergency mode (docs/emergency.md): the cover agent's token reaches only the cover tools,
//! never the main agent's server, every call is recorded, and the token dies with the server.
use agent::{
    delivery::DeliveryPermit,
    mcp::{McpServer, Scope, ScopedTools, ToolHost, UnavailableArchive},
};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Default)]
struct Host(Mutex<Vec<String>>);
#[async_trait]
impl ToolHost for Host {
    fn speak(&self, text: &str, _: &str, _: &DeliveryPermit) -> Result<u64> {
        self.0.lock().unwrap().push(format!("speak:{text}"));
        Ok(7)
    }
    async fn action(&self, name: &str, _: Value, _: &str, _: &DeliveryPermit) -> Result<String> {
        self.0.lock().unwrap().push(name.into());
        Ok(format!("{name} done."))
    }
}
#[derive(Default)]
struct Own(Mutex<Vec<(String, std::result::Result<String, String>)>>);
#[async_trait]
impl ScopedTools for Own {
    fn definitions(&self) -> Vec<Value> {
        vec![json!({"name":"emergency_status","description":"Emergency state.","inputSchema":{"type":"object","properties":{},"additionalProperties":false}})]
    }
    async fn call(&self, name: &str, _: &Value, _: &DeliveryPermit) -> Option<Result<String>> {
        (name == "emergency_status").then(|| Ok("Emergency mode on: quota.".into()))
    }
    fn record(&self, name: &str, _: &Value, result: &std::result::Result<String, String>) {
        self.0.lock().unwrap().push((name.into(), result.clone()));
    }
}
async fn rpc(url: &str, token: &str, value: Value) -> String {
    let address = url.trim_start_matches("http://").trim_end_matches("/mcp");
    let Ok(mut socket) = tokio::net::TcpStream::connect(address).await else { return "refused".into() };
    let body = value.to_string();
    let request = format!("POST /mcp HTTP/1.1\r\nHost: {address}\r\nAccept: application/json, text/event-stream\r\nMCP-Protocol-Version: 2025-06-18\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\nAuthorization: Bearer {token}\r\n\r\n{body}", body.len());
    socket.write_all(request.as_bytes()).await.unwrap();
    let mut reply = String::new();
    let _ = socket.read_to_string(&mut reply).await;
    reply
}
fn call(name: &str, args: Value) -> Value {
    json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":args,"_meta":{"heimdallActionId":"cli-1"}}})
}

#[tokio::test]
async fn the_cover_token_reaches_only_the_cover_tools_and_dies_with_its_server() {
    let host = Arc::new(Host::default());
    let own = Arc::new(Own::default());
    let gate = Arc::new(agent::delivery::DeliveryGate::new(true));
    let main = McpServer::start(host.clone(), Arc::new(UnavailableArchive), gate.clone()).await.unwrap();
    let token = agent::emergency::new_token();
    let scoped = McpServer::start_scoped(host.clone(), Arc::new(UnavailableArchive), gate.clone(),
        Scope { token: token.clone(), allow: agent::mcp::Allow::Only(&["speak", "plan_edit"]), own: own.clone() }).await.unwrap();
    assert_eq!(scoped.token, token);
    assert_ne!(scoped.url, main.url);
    // The cover token is not the main agent's.
    assert!(rpc(&main.url, &token, json!({"jsonrpc":"2.0","id":1,"method":"tools/list"})).await.starts_with("HTTP/1.1 401"));
    assert!(rpc(&scoped.url, &main.token, json!({"jsonrpc":"2.0","id":1,"method":"tools/list"})).await.starts_with("HTTP/1.1 401"));
    let list = rpc(&scoped.url, &token, json!({"jsonrpc":"2.0","id":1,"method":"tools/list"})).await;
    assert!(list.contains("\"speak\"") && list.contains("plan_edit") && list.contains("emergency_status"), "{list}");
    assert!(!list.contains("timeline_search") && !list.contains("reload_me") && !list.contains("screen_frames"), "{list}");
    let spoke = rpc(&scoped.url, &token, call("speak", json!({"text":"Heimdall's agent is out of quota."}))).await;
    assert!(spoke.contains("Speech queued: 7."), "{spoke}");
    assert!(rpc(&scoped.url, &token, call("emergency_status", json!({}))).await.contains("Emergency mode on: quota."));
    let refused = rpc(&scoped.url, &token, call("reload_me", json!({}))).await;
    assert!(refused.contains("not available to this agent"), "{refused}");
    let invalid = rpc(&scoped.url, &token, call("speak", json!({"text":""}))).await;
    assert!(invalid.contains("isError") && invalid.contains("INVALID_ARGUMENT"), "{invalid}");
    assert_eq!(*host.0.lock().unwrap(), ["speak:Heimdall's agent is out of quota."]);
    let recorded: Vec<String> = own.0.lock().unwrap().iter().map(|(n, r)| format!("{n}:{}", r.is_ok())).collect();
    assert_eq!(recorded, ["speak:true", "emergency_status:true", "speak:false"]);
    // Agent off refuses the cover agent too.
    gate.store(false, std::sync::atomic::Ordering::Release);
    assert!(rpc(&scoped.url, &token, call("speak", json!({"text":"x"}))).await.starts_with("HTTP/1.1 503"));
    gate.store(true, std::sync::atomic::Ordering::Release);
    let url = scoped.url.clone();
    scoped.shutdown().await;
    assert!(!rpc(&url, &token, call("speak", json!({"text":"x"}))).await.contains("Speech queued"));
    assert_eq!(host.0.lock().unwrap().len(), 1);
    main.shutdown().await;
}
