//! The HTTP API Heimdall uses (server.py FrontendHandler): chat completions, streamed or whole, the model list,
//! health, readiness, status and weight release. Generation runs on blocking threads; streams reach the socket
//! through a channel, and a client that leaves cancels its request.
use super::app::{Answer, App, Event};
use super::shapes::{completion_response, stream_chunk, ApiError};
use axum::{
    body::{Body, Bytes},
    extract::{DefaultBodyLimit, Path, Request, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use serde_json::{json, Map, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, Semaphore};

/// SSE silence after which a stream sends something (server.SSE_KEEPALIVE_SECONDS).
const SSE_KEEPALIVE: Duration = Duration::from_secs(2);
/// How long a status probe waits for a busy engine before the last answer stands in.
const STATUS_TIMEOUT: Duration = Duration::from_millis(250);
/// How long a stream may wait on a client that reads nothing before it is taken to have left (Python's socket write
/// timeout).
const STALLED_CLIENT: Duration = Duration::from_secs(30);

pub struct Server {
    pub app: Arc<App>,
    pub requests: Arc<Semaphore>,
    pub queue_size: usize,
    pub allowed_hosts: Vec<String>,
    pub host: String,
    pub port: u16,
    pub instance_id: String,
    /// The engine's last status answer and when it came.
    last_status: Mutex<Option<(Value, Instant)>>,
}

impl Server {
    pub fn new(app: Arc<App>, queue_size: usize, host: &str, port: u16) -> Arc<Self> {
        let mut allowed_hosts: Vec<String> = ["localhost", "127.0.0.1", "::1"].map(String::from).to_vec();
        if host != "0.0.0.0" && host != "::" {
            allowed_hosts.push(host.to_ascii_lowercase());
        }
        Arc::new(Server {
            app,
            requests: Arc::new(Semaphore::new(queue_size)),
            queue_size,
            allowed_hosts,
            host: host.into(),
            port,
            instance_id: format!("{:024x}", rand::random::<u128>() >> 32),
            last_status: Mutex::new(None),
        })
    }

    /// The engine's status with the server's own facts (backend.status, Frontend.status, FrontendServer.status). A
    /// busy engine that misses the probe keeps its last readiness.
    pub fn status(&self) -> Value {
        let app = &self.app;
        let engine = app.engine.get();
        let mut snapshot = match engine.status(STATUS_TIMEOUT) {
            Ok(fresh) => {
                *self.last_status.lock().unwrap() = Some((fresh.clone(), Instant::now()));
                fresh
            }
            Err(error) => {
                let last = self.last_status.lock().unwrap().clone();
                let busy = matches!(error, crate::engine::EngineError::Timeout(_));
                match last {
                    Some((mut v, at)) => {
                        let ready = busy && v["ready"] == true && at.elapsed() < Duration::from_secs(120);
                        v["ready"] = ready.into();
                        v["status_stale"] = true.into();
                        v["status_age_ms"] = json!(at.elapsed().as_secs_f64() * 1000.0);
                        v
                    }
                    None => json!({"ready": false, "status_stale": true}),
                }
            }
        };
        if engine.failure().is_some() {
            snapshot["ready"] = false.into();
        }
        snapshot["vision"] = app.ready.vision.into();
        snapshot["input_modalities"] = json!(app.input_modalities());
        snapshot["reasoning_switch"] = app.format.reasoning_switch().into();
        snapshot["chat_template"] = app.templates.status();
        let grammar = app.constraints.stats();
        snapshot["grammar_cache"] = serde_json::to_value(&grammar).unwrap_or(Value::Null);
        snapshot["transport"] = json!({
            "ready": engine.failure().is_none(),
            "pending": engine.pending_count(),
            "pending_limit": engine.pending_limit(),
            "error": engine.failure().map(|e| e.to_string()),
        });
        snapshot["instance"] = json!({
            "id": self.instance_id,
            "pid": std::process::id(),
            "model": app.model,
            "host": self.host,
            "port": self.port,
            "started_at": app.started_at,
            "server": "heimdall-splash",
        });
        snapshot["http"] = json!({"requests": {"capacity": self.queue_size, "available": self.requests.available_permits()}});
        snapshot
    }
}

fn json_response(status: u16, value: &Value) -> Response {
    let mut r = Response::new(Body::from(serde_json::to_vec(value).unwrap_or_default()));
    *r.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    r
}

fn error_response(e: &ApiError) -> Response {
    let mut r = json_response(e.status, &e.payload());
    if e.status == 503 {
        r.headers_mut().insert(header::RETRY_AFTER, HeaderValue::from(e.retry_after.unwrap_or(1)));
    }
    r
}

/// The host name of a Host or Origin authority, lowercase, without its port or a trailing dot; and the port.
fn authority(value: &str) -> Option<(String, Option<u16>)> {
    let (name, port) = if let Some(rest) = value.strip_prefix('[') {
        let (name, after) = rest.split_once(']')?;
        (name, after.strip_prefix(':'))
    } else {
        match value.rsplit_once(':') {
            Some((n, p)) => (n, Some(p)),
            None => (value, None),
        }
    };
    let port = match port {
        Some(p) => Some(p.parse::<u16>().ok()?),
        None => None,
    };
    let name = name.trim_end_matches('.').to_ascii_lowercase();
    (!name.is_empty()).then_some((name, port))
}

/// Only names this server answers to may reach it, which keeps DNS-rebound pages out; and a browser page may call it
/// only from its own origin, which keeps the pages of other sites from making the engine work
/// (http_security.validate_headers, without --allowed-origin).
async fn host_check(State(server): State<Arc<Server>>, request: Request, next: Next) -> Response {
    let forbidden = |m: &str| error_response(&ApiError::new(403, m, "forbidden"));
    let hosts: Vec<_> = request.headers().get_all(header::HOST).iter().collect();
    let origins: Vec<_> = request.headers().get_all(header::ORIGIN).iter().collect();
    if hosts.len() != 1 || origins.len() > 1 {
        return forbidden("expected one Host header and at most one Origin header");
    }
    let Some((host, port)) = hosts[0].to_str().ok().and_then(authority) else { return forbidden("invalid Host header") };
    if !server.allowed_hosts.contains(&host) {
        return forbidden(&format!("Host {host} is not allowed"));
    }
    if let Some(origin) = origins.first() {
        let own = origin.to_str().ok().and_then(|o| o.strip_prefix("http://")).and_then(authority).is_some_and(|(h, p)| h == host && p.unwrap_or(80) == port.unwrap_or(80));
        if !own {
            return forbidden("pages of other origins may not use this server");
        }
    }
    next.run(request).await
}

/// A POST body must be JSON, as Python's server requires: a page elsewhere cannot send one without a preflight.
async fn json_only(request: Request, next: Next) -> Response {
    // Memory release takes no fields: a bare POST needs no Content-Type (server._serve, fieldless routes).
    let bare = request.uri().path() == "/v1/memory/release"
        && request.headers().get(header::CONTENT_LENGTH).is_none_or(|l| l.to_str().is_ok_and(|l| l.trim() == "0"));
    if request.method() == axum::http::Method::POST && !bare {
        let types: Vec<_> = request.headers().get_all(header::CONTENT_TYPE).iter().collect();
        let kind = types.first().and_then(|t| t.to_str().ok()).map(|t| t.split(';').next().unwrap_or("").trim().to_ascii_lowercase());
        let json = kind.as_deref().is_some_and(|k| k == "application/json" || (k.starts_with("application/") && k.ends_with("+json")));
        let encoded = request.headers().get_all(header::CONTENT_ENCODING).iter().any(|e| !e.to_str().is_ok_and(|e| e.trim().eq_ignore_ascii_case("identity")));
        if encoded {
            return error_response(&ApiError::new(415, "content encoding is not supported", "unsupported_media_type"));
        }
        if types.len() != 1 || !json {
            return error_response(&ApiError::new(415, "Content-Type must be application/json", "unsupported_media_type"));
        }
    }
    next.run(request).await
}

pub fn router(server: Arc<Server>, max_request_bytes: usize) -> Router {
    Router::new()
        .route("/health", get(|| async { json_response(200, &json!({"status": "ok"})) }))
        .route("/ready", get(ready))
        .route("/status", get(status))
        .route("/v1/models", get(models))
        .route("/v1/models/{*name}", get(model))
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/memory/release", post(release))
        .fallback(|| async { error_response(&ApiError::new(404, "not found", "not_found")) })
        .layer(DefaultBodyLimit::max(max_request_bytes))
        .layer(middleware::from_fn(json_only))
        .layer(middleware::from_fn_with_state(server.clone(), host_check))
        .with_state(server)
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    tokio::task::spawn_blocking(f).await.expect("blocking task")
}

async fn ready(State(server): State<Arc<Server>>) -> Response {
    let ready = blocking(move || server.status()["ready"] == true).await;
    json_response(if ready { 200 } else { 503 }, &json!({"status": if ready { "ready" } else { "unavailable" }}))
}

async fn status(State(server): State<Arc<Server>>) -> Response {
    let status = blocking(move || server.status()).await;
    json_response(200, &status)
}

fn model_entries(app: &App) -> Vec<Value> {
    app.model_names
        .iter()
        .map(|name| {
            let mut m = json!({
                "id": name,
                "object": "model",
                "created": 0,
                "owned_by": "splash",
                "max_model_len": app.max_context(),
                "context_length": app.max_context(),
                "vision": app.ready.vision,
                "input_modalities": app.input_modalities(),
            });
            if name != &app.response_model {
                m["root"] = json!(app.response_model);
            }
            m
        })
        .collect()
}

async fn models(State(server): State<Arc<Server>>) -> Response {
    let data = model_entries(&server.app);
    let typed: Vec<Value> = data.iter().map(|m| json!({"name": m["id"], "description": "Splash resident model", "release_date": ""})).collect();
    json_response(200, &json!({"object": "list", "data": data, "models": typed}))
}

async fn model(State(server): State<Arc<Server>>, Path(name): Path<String>) -> Response {
    match model_entries(&server.app).into_iter().find(|m| m["id"] == name.as_str()) {
        Some(m) => json_response(200, &m),
        None => error_response(&ApiError::new(404, "model not found", "model_not_found")),
    }
}

async fn release(State(server): State<Arc<Server>>) -> Response {
    let answer = blocking(move || server.app.engine.get().release_weights(Duration::from_secs(5))).await;
    match answer {
        Ok(status) => {
            let weights = status.get("weights").cloned().unwrap_or(json!({}));
            if weights["released"] == true && weights["restoring"] == false {
                json_response(200, &json!({"released": true, "weights": weights}))
            } else {
                error_response(&ApiError::new(409, "the engine is serving requests or restoring its weights; it releases them only while idle", "engine_busy"))
            }
        }
        Err(e) => error_response(&ApiError::new(503, e.to_string(), "runtime_unavailable")),
    }
}

/// The request's JSON object, with null fields read as absent (server._serve).
fn body_object(bytes: &[u8]) -> Result<Map<String, Value>, ApiError> {
    let value: Value = serde_json::from_slice(bytes).map_err(|_| ApiError::bad("request body must be valid JSON"))?;
    match value {
        Value::Object(map) => Ok(map.into_iter().filter(|(_, v)| !v.is_null()).collect()),
        _ => Err(ApiError::bad("request body must be a JSON object")),
    }
}

fn stream_options(body: &Map<String, Value>) -> Result<(bool, bool, bool), ApiError> {
    let invalid = || ApiError::bad("invalid streaming options");
    let stream = match body.get("stream") {
        None => false,
        Some(Value::Bool(b)) => *b,
        _ => return Err(invalid()),
    };
    let include_usage = match body.get("stream_options") {
        None => false,
        Some(Value::Object(o)) => match o.get("include_usage") {
            None | Some(Value::Null) => false,
            Some(Value::Bool(b)) => *b,
            _ => return Err(invalid()),
        },
        _ => return Err(invalid()),
    };
    let progress = match body.get("return_progress") {
        None | Some(Value::Bool(false)) => false,
        Some(Value::Bool(true)) if stream => true,
        _ => return Err(ApiError::bad("return_progress requires stream: true and must be a boolean")),
    };
    Ok((stream, include_usage, progress))
}

/// Set false when the handler's future is dropped: the client left.
struct Alive(Arc<AtomicBool>);
impl Drop for Alive {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

fn message(answer: &Answer) -> Value {
    let mut m = json!({"role": "assistant", "content": if answer.content.is_empty() { Value::Null } else { answer.content.clone().into() }});
    if !answer.reasoning.is_empty() {
        m["reasoning_content"] = answer.reasoning.clone().into();
    }
    if !answer.tool_calls.is_empty() {
        m["tool_calls"] = answer.tool_calls.iter().map(|c| c.to_openai()).collect();
    }
    m
}

fn sse(value: &Value) -> Bytes {
    Bytes::from(format!("data: {}\n\n", serde_json::to_string(value).unwrap_or_default()))
}

async fn chat_completions(State(server): State<Arc<Server>>, _headers: HeaderMap, bytes: Bytes) -> Response {
    if let Some(refusal) = server.app.refusal() {
        return error_response(&refusal);
    }
    // One admission slot through preparation and the whole response.
    let Ok(permit) = server.requests.clone().try_acquire_owned() else {
        return error_response(&ApiError::new(503, "frontend request capacity is exhausted", "frontend_overloaded"));
    };
    let body = match body_object(&bytes) {
        Ok(b) => b,
        Err(e) => return error_response(&e),
    };
    let (stream, include_usage, progress) = match stream_options(&body) {
        Ok(o) => o,
        Err(e) => return error_response(&e),
    };
    let app = server.app.clone();
    let prepared = blocking(move || -> Result<_, ApiError> {
        let mut job = app.prepare(&body)?;
        job.submission.return_progress = progress;
        let request = app.submit(&mut job)?;
        Ok((job, request))
    })
    .await;
    let (job, request) = match prepared {
        Ok(p) => p,
        Err(e) => return error_response(&e),
    };
    let app = server.app.clone();
    let model = app.response_model.clone();
    if !stream {
        let alive = Arc::new(AtomicBool::new(true));
        let _guard = Alive(alive.clone());
        let answer = blocking(move || {
            let _permit = permit;
            let result = app.collect(&job, request, &mut |_| alive.load(Ordering::Acquire));
            (result, job)
        })
        .await;
        return match answer {
            (Ok(answer), job) => json_response(200, &completion_response(&model, &job.public_id, job.created, &answer.result, message(&answer), answer.called, answer.reasoning_tokens)),
            (Err(e), _) => error_response(&e),
        };
    }
    let (tx, rx) = mpsc::channel::<Bytes>(64);
    // The response opens at the start or the first keepalive, as Python's does; an error before then is answered
    // with its own HTTP status, which clients retry by.
    let (head_tx, head_rx) = tokio::sync::oneshot::channel::<Option<ApiError>>();
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let (id, created) = (job.public_id.clone(), job.created);
        let chunk = |delta: Value, finish: Option<&str>| stream_chunk(&model, &id, created, delta, finish);
        let head = std::cell::RefCell::new(Some(head_tx));
        let mut started = false;
        let mut last_write = Instant::now();
        // A client that reads nothing for STALLED_CLIENT has left.
        let send = |bytes: Bytes, last_write: &mut Instant| -> bool {
            if let Some(head) = head.borrow_mut().take() {
                if head.send(None).is_err() {
                    return false;
                }
            }
            *last_write = Instant::now();
            let deadline = Instant::now() + STALLED_CLIENT;
            let mut bytes = bytes;
            loop {
                match tx.try_send(bytes) {
                    Ok(()) => return true,
                    Err(mpsc::error::TrySendError::Closed(_)) => return false,
                    Err(mpsc::error::TrySendError::Full(back)) => {
                        if Instant::now() >= deadline {
                            return false;
                        }
                        bytes = back;
                        std::thread::sleep(Duration::from_millis(10));
                    }
                }
            }
        };
        let result = app.collect(&job, request, &mut |event| match event {
            Event::Started => {
                started = true;
                send(sse(&chunk(json!({"role": "assistant", "content": ""}), None)), &mut last_write)
            }
            Event::Delta(delta) => send(sse(&chunk(delta.to_openai(), None)), &mut last_write),
            Event::Progress(Value::Null) => {
                if last_write.elapsed() < SSE_KEEPALIVE {
                    return !tx.is_closed();
                }
                // Before the start an SSE comment; after it a chunk that adds nothing, as clients that time out on
                // missing data events ignore comments.
                let bytes = if started { sse(&chunk(json!({}), None)) } else { Bytes::from_static(b": splash-keepalive\n\n") };
                send(bytes, &mut last_write)
            }
            Event::Progress(p) => {
                let mut c = chunk(json!({}), None);
                c["prompt_progress"] = p;
                send(sse(&c), &mut last_write)
            }
        });
        match result {
            Ok(answer) => {
                if !started {
                    send(sse(&chunk(json!({"role": "assistant", "content": ""}), None)), &mut last_write);
                }
                let mut last = chunk(json!({}), Some(answer.result.finish_reason(answer.called)));
                last["timings"] = answer.result.timings();
                send(sse(&last), &mut last_write);
                if include_usage {
                    let mut usage = chunk(json!({}), None);
                    usage["choices"] = json!([]);
                    usage["usage"] = answer.result.usage(answer.reasoning_tokens);
                    usage["metrics"] = answer.result.metrics();
                    send(sse(&usage), &mut last_write);
                }
            }
            Err(e) if e.status == 499 => return,
            Err(e) => {
                let early = head.borrow_mut().take();
                if let Some(head) = early {
                    let _ = head.send(Some(e));
                    return;
                }
                send(sse(&e.payload()), &mut last_write);
            }
        }
        send(Bytes::from_static(b"data: [DONE]\n\n"), &mut last_write);
    });
    match head_rx.await {
        Ok(Some(error)) => return error_response(&error),
        Ok(None) => {}
        Err(_) => return error_response(&ApiError::new(500, "internal server error", "internal_server_error")),
    }
    let body = Body::from_stream(tokio_stream::wrappers::ReceiverStream::new(rx).map(Ok::<_, std::convert::Infallible>));
    let mut r = Response::new(body);
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    r.into_response()
}

use futures_util::StreamExt;
