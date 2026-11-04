//! Stateless Streamable HTTP MCP, loopback only and bearer authenticated.
use crate::{
    delivery::{DeliveryGate, DeliveryPermit},
    protocol::Attachment,
};
use anyhow::Result;
use async_trait::async_trait;
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use serde_json::{json, Value};
use std::sync::Arc;
#[async_trait]
pub trait TranscriptSearch: Send + Sync {
    async fn timeline(&self, _query: Value, _permit: &DeliveryPermit) -> Result<Value> {
        anyhow::bail!("INDEX_UNAVAILABLE: timeline index is not available; retry after indexing")
    }
    /// A `timeline_search` hit handle (`h12`) as its chunk id, while the handle is kept.
    fn resolve_handle(&self, _handle: &str) -> Option<String> {
        None
    }
    async fn chunks(&self, _query: Value, _permit: &DeliveryPermit) -> Result<Value> {
        anyhow::bail!("chunk index is not available yet")
    }
    async fn search(&self, _query: &str, _limit: usize, _permit: &DeliveryPermit) -> Result<Value> {
        anyhow::bail!("archive index is not available")
    }
}
pub struct UnavailableArchive;
#[async_trait]
impl TranscriptSearch for UnavailableArchive {
    async fn search(&self, _: &str, _: usize, _: &DeliveryPermit) -> Result<Value> {
        anyhow::bail!("archive index is not available yet")
    }
}
pub struct ScreenshotResult {
    pub images: Vec<Attachment>,
    /// Supplemental failures accompany usable ring images, rather than isError.
    pub warnings: Vec<String>,
    /// Plain answers in place of images, such as that the user switched the screen off.
    pub notes: Vec<String>,
}
impl ScreenshotResult {
    pub fn into_value(self) -> Result<Value> {
        let mut content = vec![];
        for image in self.images {
            let block = image.block()?;
            content.push(json!({"type":"image","data":block["source"]["data"],"mimeType":block["source"]["media_type"]}));
        }
        for warning in self.warnings {
            content.push(json!({"type":"text","text":warning}));
        }
        for note in self.notes {
            content.push(json!({"type":"text","text":note}));
        }
        Ok(json!({"content":content}))
    }
}
#[async_trait]
pub trait ToolHost: Send + Sync {
    fn speak_replay(
        &self,
        text: &str,
        language: &str,
        _action: &str,
        permit: &DeliveryPermit,
    ) -> Result<String> {
        Ok(format!(
            "Speech queued: {}.",
            self.speak(text, language, permit)?
        ))
    }
    async fn screen_frames(&self, _query: Value, _permit: &DeliveryPermit) -> Result<Value> {
        anyhow::bail!("CAPTURE_UNAVAILABLE: screen store is not attached")
    }
    async fn action(
        &self,
        _name: &str,
        _args: Value,
        _action_id: &str,
        _permit: &DeliveryPermit,
    ) -> Result<String> {
        anyhow::bail!("HOST_UNAVAILABLE: action channel is not attached; check the agent")
    }

    /// Enqueue speech; never wait for inference/playback. Return the speech job ID.
    fn speak(&self, text: &str, language: &str, permit: &DeliveryPermit) -> Result<u64>;
    async fn screenshot_now(&self, _permit: &DeliveryPermit) -> Result<ScreenshotResult> {
        anyhow::bail!("capture unavailable")
    }
    /// `look`: bring `app` forward (if given) and return the display the user is on as image content.
    async fn look(&self, _app: Option<&str>, _permit: &DeliveryPermit) -> Result<Value> {
        anyhow::bail!("CAPTURE_UNAVAILABLE: screen capture is not attached")
    }
}
struct Service {
    host: Arc<dyn ToolHost>,
    archive: Arc<dyn TranscriptSearch>,
    token: String,
    authority: String,
    generation: String,
    gate: Arc<DeliveryGate>,
    lifetime: Arc<DeliveryGate>,
    scope: Option<Scope>,
}
/// A second server for one outside caller: the relay thread (docs/claude-relay.md, docs/emergency.md). It has its own
/// port and token, serves `allow` of the host's tools plus `own`'s tools, and tells `own` about every call so it
/// enters Heimdall's record. It lives only as long as the grant: shut it down and the token is dead.
pub struct Scope {
    pub token: String,
    pub allow: Allow,
    pub own: Arc<dyn ScopedTools>,
}
/// Which of the host's tools a scoped server serves.
#[derive(Clone, Copy, Debug)]
pub enum Allow {
    /// Every tool the main server has (`mcp_contract::tools()`): the relay acts as Heimdall's agent.
    All,
    /// Only these (none: only `own`'s, the end-only grant after a stand-down).
    Only(&'static [&'static str]),
}
impl Allow {
    pub fn allows(&self, name: &str) -> bool {
        match self { Self::All => true, Self::Only(names) => names.contains(&name) }
    }
}
#[async_trait]
pub trait ScopedTools: Send + Sync {
    /// Tool definitions of its own (`tools/list`), beside the allowed host tools.
    fn definitions(&self) -> Vec<Value>;
    /// One of its own tools; `None` when `name` is not one.
    async fn call(&self, name: &str, args: &Value, permit: &DeliveryPermit) -> Option<Result<String>>;
    /// Every call that ran, with its plain-text answer or error.
    fn record(&self, name: &str, args: &Value, result: &std::result::Result<String, String>);
}
pub struct McpServer {
    pub url: String,
    pub token: String,
    lifetime: Arc<DeliveryGate>,
    task: Option<tokio::task::JoinHandle<()>>,
}
impl McpServer {
    pub async fn start(
        host: Arc<dyn ToolHost>,
        archive: Arc<dyn TranscriptSearch>,
        gate: Arc<DeliveryGate>,
    ) -> Result<Self> {
        Self::serve(host, archive, gate, None).await
    }
    /// The scoped server (`Scope`): `url` and `token` are the scope's.
    pub async fn start_scoped(host: Arc<dyn ToolHost>, archive: Arc<dyn TranscriptSearch>, gate: Arc<DeliveryGate>, scope: Scope) -> Result<Self> {
        Self::serve(host, archive, gate, Some(scope)).await
    }
    async fn serve(host: Arc<dyn ToolHost>, archive: Arc<dyn TranscriptSearch>, gate: Arc<DeliveryGate>, scope: Option<Scope>) -> Result<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}/mcp", listener.local_addr()?);
        let token = scope.as_ref().map_or_else(|| uuid::Uuid::new_v4().to_string(), |s| s.token.clone());
        let lifetime = Arc::new(DeliveryGate::new(true));
        let serving = lifetime.permit()?;
        let service = Arc::new(Service {
            host,
            archive,
            token: token.clone(),
            authority: listener.local_addr()?.to_string(),
            generation: uuid::Uuid::new_v4().to_string(),
            gate,
            lifetime: lifetime.clone(),
            scope,
        });
        let router = Router::new()
            .route("/mcp", post(request))
            .layer(axum::extract::DefaultBodyLimit::max(
                crate::mcp_contract::REQUEST_BYTES,
            ))
            .with_state(service);
        let task = tokio::spawn(async move {
            // Own every HTTP/1 connection instead of axum::serve's detached tasks.
            // HTTP/1 is the existing stateless MCP transport (no upgrades).
            let mut connections = tokio::task::JoinSet::new();
            let cancelled = serving.cancelled();
            tokio::pin!(cancelled);
            loop {
                tokio::select! {
                    biased;
                    _ = &mut cancelled => break,
                    _ = connections.join_next(), if !connections.is_empty() => {},
                    accepted = listener.accept() => {
                        let Ok((socket, _)) = accepted else { break; };
                        if connections.len() >= 16 { drop(socket); continue; }
                        let service = hyper_util::service::TowerToHyperService::new(router.clone());
                        connections.spawn(async move {
                            let _ = tokio::time::timeout(std::time::Duration::from_secs(75), hyper::server::conn::http1::Builder::new()
                                .serve_connection(hyper_util::rt::TokioIo::new(socket), service)).await;
                        });
                    }
                }
            }
            drop(listener);
            connections.abort_all();
            while connections.join_next().await.is_some() {}
        });
        Ok(Self {
            url,
            token,
            lifetime,
            task: Some(task),
        })
    }
    pub async fn shutdown(mut self) {
        // Revoke tool commits and response polls synchronously, then join sockets.
        self.lifetime
            .store(false, std::sync::atomic::Ordering::Release);
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}
impl Drop for McpServer {
    fn drop(&mut self) {
        self.lifetime
            .store(false, std::sync::atomic::Ordering::Release);
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}
pub use crate::mcp_contract::{tools, PROTOCOL};
/// `output: "frames"`: one archived screen frame per hit (the first four), each after its metadata line, within the
/// image budget. A hit without a frame says so in one line.
async fn timeline_frames(s: &Service, page: &Value, result: &mut Value, permit: &DeliveryPermit) {
    let mut images = 0;
    for hit in page["hits"].as_array().into_iter().flatten().take(4) {
        let handle = hit["h"].as_str().unwrap_or("?");
        let mut frame = match s.host.screen_frames(json!({"at": hit["start"], "quality": "preview"}), permit).await {
            Ok(f) => f,
            Err(e) => { push_text(result, &format!("[{handle}] no frame: {}", e.to_string().chars().take(160).collect::<String>())); continue; }
        };
        crate::plain::result_blocks(&mut frame, "screen_frames", &chrono::Local);
        let blocks = frame["content"].as_array().cloned().unwrap_or_default();
        let bytes: usize = blocks.iter().filter_map(|b| b["data"].as_str()).map(str::len).sum();
        if images + bytes > crate::mcp_contract::IMAGE_BYTES { push_text(result, &format!("[{handle}] frame left out: image budget")); break; }
        images += bytes;
        let content = result["content"].as_array_mut().unwrap();
        for mut b in blocks {
            if let Some(t) = b["text"].as_str() { b["text"] = json!(format!("[{handle}] {t}")); }
            content.push(b);
        }
    }
}
/// `output: "clip"`: one archive clip around the best hit, widened to the other hits within ten minutes of it.
async fn timeline_clip(s: &Service, page: &Value, result: &mut Value, op: &str, permit: &DeliveryPermit) {
    let time = |v: &Value| v.as_str().and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok());
    let hits = page["hits"].as_array().cloned().unwrap_or_default();
    let Some(top) = hits.first().and_then(|h| time(&h["start"])) else { return push_text(result, "No hits to clip.") };
    let near: Vec<&Value> = hits.iter().filter(|h| time(&h["start"]).is_some_and(|t| (t - top).num_seconds().abs() <= 600)).collect();
    let start = near.iter().filter_map(|h| time(&h["start"])).min().unwrap() - chrono::Duration::seconds(5);
    let end = near.iter().filter_map(|h| time(&h["end"]).or(time(&h["start"]))).max().unwrap() + chrono::Duration::seconds(5);
    let window = json!({"start": start.to_rfc3339(), "end": end.to_rfc3339()});
    let text = match crate::mcp_contract::validate_tool("archive_clip", &window) {
        Ok(()) => s.host.action("archive_clip", window, op, permit).await.unwrap_or_else(|e| format!("Clip failed: {e}")),
        Err(e) => format!("Clip failed: {e}"),
    };
    push_text(result, &text);
}
/// Tools that take a timeline id take a `timeline_search` handle too (token diet, 26 Oct): `screen_frames.chunk_id`,
/// `taste_note.lines` and an island notice's image `archive_id` get the full id before validation. Unknown handles
/// pass unchanged (and fail as an unknown id would).
fn resolve_handles(archive: &dyn TranscriptSearch, name: &str, args: &Value) -> Value {
    let mut args = args.clone();
    let is_handle = |v: &Value| v.as_str().is_some_and(|s| s.len() > 1 && s.starts_with('h') && s[1..].bytes().all(|b| b.is_ascii_digit()));
    let mut fix = |v: &mut Value| if is_handle(v) { if let Some(id) = archive.resolve_handle(v.as_str().unwrap()) { *v = json!(id); } };
    // `get_mut`, never indexing: indexing a missing key would add it as null and fail validation.
    match name {
        "screen_frames" => if let Some(v) = args.get_mut("chunk_id") { fix(v) },
        "taste_note" => if let Some(lines) = args.get_mut("lines").and_then(Value::as_array_mut) { lines.iter_mut().for_each(&mut fix) },
        "island_notice" => if let Some(v) = args.get_mut("detail").and_then(|d| d.get_mut("archive_id")) { fix(v) },
        _ => {}
    }
    args
}
/// One tool call, validated and gated: the main server's and the scoped one's shared path.
async fn dispatch(s: &Service, name: &str, args: &Value, action: &str, permit: &DeliveryPermit) -> Result<Value> {
    crate::mcp_contract::validate_tool(name, args)
        .map_err(|e| anyhow::anyhow!("INVALID_ARGUMENT: {e}"))?;
    // A state write admitted now runs to its end even if Agent turns off during it; its answer is still withheld after Off, like every other result.
    let write = crate::mcp_contract::writes_state(name).then(|| s.gate.crucial_write(&permit)).transpose()?;
    let tool_permit = write.as_ref().map_or(permit, |w| w.permit());
    permit
        .run(async {
            let result = match name {
                "speak" => text_result(s.host.speak_replay(
                    args["text"].as_str().unwrap(),
                    args["language"].as_str().unwrap_or("en"),
                    &action,
                    &permit,
                )?),
                "screen_frames" => {
                    s.host.screen_frames(args.clone(), &permit).await?
                }
                "timeline_search" => {
                    let mut query = args.clone();
                    let output = query.as_object_mut().unwrap().remove("output");
                    if output.as_ref().is_some_and(|o| o == "full") { query["output"] = json!("full"); }
                    let page = s.archive.timeline(query, &permit).await?;
                    let mut result = text_result(&page);
                    match output.as_ref().and_then(Value::as_str) {
                        Some("frames") => timeline_frames(&s, &page, &mut result, &permit).await,
                        Some("clip") => timeline_clip(&s, &page, &mut result, &action, tool_permit).await,
                        _ => {}
                    }
                    result
                }
                "look" => s.host.look(args["app"].as_str(), &permit).await?,
                _ => text_result(
                    s.host.action(name, args.clone(), &action, tool_permit).await?,
                ),
            };
            // tool-output: every tool answers in plain lines, never raw JSON (plain.rs).
            let mut result = result;
            crate::plain::result_blocks(&mut result, name, &chrono::Local);
            // Every image leaves labelled from its own bytes (20 Oct: a JPEG labelled PNG
            // poisoned the conversation; `history`).
            crate::history::normalize_tool_images(&mut result);
            let bytes = serde_json::to_vec(&result)?.len();
            anyhow::ensure!(
                bytes
                    <= crate::mcp_contract::IMAGE_BYTES
                        + crate::mcp_contract::TEXT_BYTES,
                "OUTPUT_TOO_LARGE: narrow the request or use a continuation"
            );
            Ok(result)
        })
        .await
}
/// A call on the scoped server: its own tools, or an allowed host tool on the shared path; anything else is not there.
async fn scoped_call(s: &Service, p: &Value, id: &Value, permit: &DeliveryPermit) -> std::result::Result<Value, (i64, String)> {
    let scope = s.scope.as_ref().unwrap();
    let name = p["name"].as_str().unwrap_or("");
    let args = if p["arguments"].is_null() { json!({}) } else { p["arguments"].clone() };
    let action = p["_meta"]["heimdallActionId"].as_str().filter(|a| !a.is_empty() && a.len() <= 128).map(str::to_owned).unwrap_or_else(|| format!("{}:{id}", s.generation));
    let answer: Result<String> = match scope.own.call(name, &args, permit).await {
        Some(r) => r,
        // A host tool answers in full, its images included (the CLI writes them to files).
        None if scope.allow.allows(name) && tools().as_array().unwrap().iter().any(|t| t["name"] == name) => {
            let resolved = resolve_handles(&*s.archive, name, &args);
            let result = dispatch(s, name, &resolved, &action, permit).await.map_err(|e| e.to_string().chars().take(1024).collect::<String>());
            let text = result.as_ref().map(|v| v["content"].as_array().into_iter().flatten().filter_map(|c| c["text"].as_str()).collect::<Vec<_>>().join("\n")).map_err(Clone::clone);
            scope.own.record(name, &args, &text);
            return Ok(match result { Ok(v) => v, Err(e) => json!({"isError":true,"content":[{"type":"text","text":e}]}) });
        }
        None => return Err((-32602, format!("{name} is not available to this agent"))),
    };
    let answer = answer.map_err(|e| e.to_string().chars().take(1024).collect::<String>());
    scope.own.record(name, &args, &answer);
    Ok(match answer {
        Ok(text) => text_result(text),
        Err(e) => json!({"isError":true,"content":[{"type":"text","text":e}]}),
    })
}
fn push_text(result: &mut Value, text: &str) {
    result["content"].as_array_mut().unwrap().push(json!({"type":"text","text":text}));
}
fn text_result(value: impl ToString) -> Value {
    json!({"content":[{"type":"text","text":value.to_string()}]})
}
fn protocol_error(id: &Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})
}
async fn request(
    State(s): State<Arc<Service>>,
    headers: HeaderMap,
    bytes: axum::body::Bytes,
) -> Response {
    let Ok(server_permit) = s.lifetime.permit() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    if headers.get("authorization").and_then(|h| h.to_str().ok())
        != Some(&format!("Bearer {}", s.token))
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    // Non-browser bridge only. No browser Origin is authorized.
    if headers.contains_key("origin") {
        return StatusCode::FORBIDDEN.into_response();
    }
    if headers.get("host").and_then(|h| h.to_str().ok()) != Some(s.authority.as_str()) {
        return StatusCode::MISDIRECTED_REQUEST.into_response();
    }
    let Ok(agent_permit) = s.gate.permit() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let permit = agent_permit.scoped(server_permit);
    if headers
        .get("content-type")
        .and_then(|h| h.to_str().ok())
        .is_none_or(|h| !h.starts_with("application/json"))
    {
        return StatusCode::UNSUPPORTED_MEDIA_TYPE.into_response();
    }
    if headers
        .get("accept")
        .and_then(|h| h.to_str().ok())
        .is_none_or(|h| !h.contains("application/json") || !h.contains("text/event-stream"))
    {
        return StatusCode::NOT_ACCEPTABLE.into_response();
    }
    let v: Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(_) => {
            return gated_response(
                protocol_error(&Value::Null, -32700, "Parse error: send valid JSON"),
                permit,
            )
        }
    };
    let id = v.get("id").cloned().unwrap_or(Value::Null);
    let method = v["method"].as_str().unwrap_or("");
    let valid = v.is_object()
        && v["jsonrpc"] == "2.0"
        && !method.is_empty()
        && v.as_object()
            .unwrap()
            .keys()
            .all(|k| matches!(k.as_str(), "jsonrpc" | "id" | "method" | "params"))
        && v.get("id")
            .is_none_or(|i| i.as_str().is_some_and(|s| s.len() <= 512) || i.is_i64() || i.is_u64())
        && v.get("params").is_none_or(Value::is_object);
    let body = if !valid {
        protocol_error(&id, -32600, "Invalid Request: use a JSON-RPC 2.0 object")
    } else if method != "initialize"
        && headers
            .get("mcp-protocol-version")
            .and_then(|h| h.to_str().ok())
            != Some(PROTOCOL)
    {
        return StatusCode::BAD_REQUEST.into_response();
    } else if v.get("id").is_none() {
        if method == "notifications/initialized"
            && v.get("params").is_none_or(|p| p.as_object().is_some_and(|p| p.is_empty()))
        {
            return StatusCode::ACCEPTED.into_response();
        }
        return StatusCode::BAD_REQUEST.into_response();
    } else {
        let result: Result<Value, (i64, String)> = match method {
            "initialize" => {
                if v["params"]["protocolVersion"] == PROTOCOL
                    && v["params"]["capabilities"].is_object()
                    && v["params"]["clientInfo"]["name"].is_string()
                    && v["params"]["clientInfo"]["version"].is_string()
                {
                    Ok(
                        json!({"protocolVersion":PROTOCOL,"capabilities":{"tools":{}},"serverInfo":{"name":"heimdall","version":env!("CARGO_PKG_VERSION")}}),
                    )
                } else {
                    Err((
                        -32602,
                        format!(
                            "Unsupported initialization; use protocol {PROTOCOL} and clientInfo"
                        ),
                    ))
                }
            }
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({"tools":match &s.scope {
                None => tools(),
                Some(scope) => {
                    let mut list: Vec<Value> = tools().as_array().unwrap().iter().filter(|t| t["name"].as_str().is_some_and(|n| scope.allow.allows(n))).cloned().collect();
                    list.extend(scope.own.definitions());
                    Value::Array(list)
                }
            }})),
            "tools/call" if s.scope.is_some() => scoped_call(&s, &v["params"], &id, &permit).await,
            "tools/call" => {
                let p = &v["params"];
                let name = p["name"].as_str().unwrap_or("");
                if !tools()
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|t| t["name"] == name)
                {
                    Err((-32602, "Unknown tool; refresh tools/list".into()))
                } else if !p.is_object()
                    || p.as_object()
                        .unwrap()
                        .keys()
                        .any(|k| !matches!(k.as_str(), "name" | "arguments" | "_meta"))
                    || p.get("_meta").is_some_and(|m| !m.is_object())
                    || p["_meta"]
                        .get("heimdallActionId")
                        .is_some_and(|id| id.as_str().is_none_or(|s| s.is_empty() || s.len() > 128))
                {
                    Err((
                        -32602,
                        "tools/call requires name, arguments and optional _meta".into(),
                    ))
                } else {
                    let resolved = resolve_handles(&*s.archive, name, &p["arguments"]);
                    let args = &resolved;
                    let action = p["_meta"]["heimdallActionId"]
                        .as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| format!("{}:{id}", s.generation));
                    let r = dispatch(&s, name, args, &action, &permit).await;
                    Ok(r.unwrap_or_else(|e|json!({"isError":true,"content":[{"type":"text","text":e.to_string().chars().take(1024).collect::<String>()}]})))
                }
            }
            _ => Err((-32601, "Method not found".into())),
        };
        match result {
            Ok(result) => json!({"jsonrpc":"2.0","id":id,"result":result}),
            Err((code, message)) => protocol_error(&id, code, &message),
        }
    };
    gated_response(body, permit)
}
fn gated_response(body: Value, permit: DeliveryPermit) -> Response {
    let mut response = Json(body).into_response();
    let body = std::mem::replace(response.body_mut(), axum::body::Body::empty());
    *response.body_mut() = axum::body::Body::new(GatedBody { body, permit });
    response
}

/// Check at the actual HTTP body delivery poll, including responses that were
/// ready before Off. Bytes already yielded to HTTP before Off cannot be recalled.
struct GatedBody {
    body: axum::body::Body,
    permit: DeliveryPermit,
}
impl http_body::Body for GatedBody {
    type Data = axum::body::Bytes;
    type Error = axum::Error;
    fn size_hint(&self) -> http_body::SizeHint {
        self.body.size_hint()
    }
    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        let permit = self.permit.clone();
        match permit.with(|| Ok(std::pin::Pin::new(&mut self.body).poll_frame(cx))) {
            Ok(frame) => frame,
            Err(_) => std::task::Poll::Ready(None),
        }
    }
}

#[cfg(test)]
mod delivery_tests {
    use super::*;
    use http_body::Body;
    #[tokio::test]
    async fn ready_response_is_suppressed_at_body_poll_after_server_retirement() {
        let agent = Arc::new(DeliveryGate::new(true));
        let server = Arc::new(DeliveryGate::new(true));
        let permit = agent.permit().unwrap().scoped(server.permit().unwrap());
        let mut body = GatedBody {
            body: axum::body::Body::from("private transcript or pixels"),
            permit,
        };
        server.store(false, std::sync::atomic::Ordering::Release);
        assert!(agent.load(std::sync::atomic::Ordering::Acquire));
        let frame = std::future::poll_fn(|cx| std::pin::Pin::new(&mut body).poll_frame(cx)).await;
        assert!(frame.is_none());
    }
    #[tokio::test]
    async fn ready_response_is_suppressed_at_body_poll_after_off_on() {
        let gate = Arc::new(DeliveryGate::new(true));
        let permit = gate.permit().unwrap();
        let mut body = GatedBody {
            body: axum::body::Body::from("private transcript or pixels"),
            permit,
        };
        gate.store(false, std::sync::atomic::Ordering::Release);
        gate.store(true, std::sync::atomic::Ordering::Release);
        let frame = std::future::poll_fn(|cx| std::pin::Pin::new(&mut body).poll_frame(cx)).await;
        assert!(frame.is_none());
    }
}

#[cfg(test)]
mod handle_tests {
    use super::*;
    struct Handles;
    #[async_trait::async_trait]
    impl TranscriptSearch for Handles {
        fn resolve_handle(&self, h: &str) -> Option<String> {
            (h == "h3").then(|| "56:text:utterance:session-1:241".to_owned())
        }
    }
    /// Token diet (26 Oct): a handle the search returned works wherever a timeline id is taken.
    #[test]
    fn handles_become_ids_for_tools_that_take_timeline_ids() {
        let id = "56:text:utterance:session-1:241";
        assert_eq!(resolve_handles(&Handles, "screen_frames", &json!({"chunk_id":"h3"}))["chunk_id"], id);
        assert_eq!(resolve_handles(&Handles, "taste_note", &json!({"lines":["h3","session-2:9","h99"]}))["lines"], json!([id, "session-2:9", "h99"]));
        assert_eq!(resolve_handles(&Handles, "island_notice", &json!({"text":"x","detail":{"type":"image","archive_id":"h3"}}))["detail"]["archive_id"], id);
        assert_eq!(resolve_handles(&Handles, "speak", &json!({"text":"h3"}))["text"], "h3", "other tools are untouched");
        for (name, args) in [("screen_frames", json!({})), ("island_notice", json!({"text":"x"})), ("taste_note", json!({"subject":"s"}))] {
            assert_eq!(resolve_handles(&Handles, name, &args), args, "nothing is added to {name}");
        }
    }
}

