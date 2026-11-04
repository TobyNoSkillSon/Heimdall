//! Explicit lab contracts for Anthropic through Heimdall's real Pi harness, production extension and MCP server.
//! A local fake stands in for api.anthropic.com: the lab extension rewrites only the native fetch destination, so Pi's
//! built-in Anthropic provider and Heimdall's provider adapter still see the production URL. No cloud request,
//! installed app, real profile or Keychain is involved; the key is a lab placeholder in the environment.
//!
//! Run: HEIMDALL_ANTHROPIC_LAB=<abs path containing /target/anthropic-lab/> cargo nextest run -p agent \
//!      --test anthropic_lab --run-ignored only -E 'not test(real_)'
//!
//! The `real_` cases call the real API (paid, a few cents) and need ANTHROPIC_API_KEY in the environment; the key is
//! handed to Pi exactly as the app hands over a Keychain key and never printed or written.
use agent::{
    harness::{Harness, Pi, PiConfig},
    protocol::Event,
    provider::{self, ApiModel},
};
use anyhow::{ensure, Context, Result};
use axum::{
    body::Bytes,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
    Router,
};
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

const LAB_KEY: &str = "sk-ant-api03-lab-0123456789abcdefghijklmnop";
/// Production's tool set and system prompt (every Heimdall MCP tool, `prompt/heimdall.md`): the real cache measurement.
const PRODUCTION: &[&str] = &["*"];

struct Host;
#[async_trait::async_trait]
impl agent::mcp::ToolHost for Host {
    fn speak(&self, _: &str, _: &str, _: &agent::delivery::DeliveryPermit) -> Result<u64> {
        anyhow::bail!("speech unavailable in lab")
    }
}
struct Search;
#[async_trait::async_trait]
impl agent::mcp::TranscriptSearch for Search {
    async fn timeline(&self, _: Value, _: &agent::delivery::DeliveryPermit) -> Result<Value> {
        // Tool results reach the model as plain lines (`plain::timeline`): the marker is a hit's text.
        Ok(json!({"hits":[{"id":"lab-1","text":"TIMELINE_LAB_RESULT"}]}))
    }
}

fn lab_base() -> Result<PathBuf> {
    let base = PathBuf::from(std::env::var("HEIMDALL_ANTHROPIC_LAB").context("HEIMDALL_ANTHROPIC_LAB")?);
    ensure!(base.is_absolute() && base.to_string_lossy().contains("/target/anthropic-lab"), "isolated lab path required");
    Ok(base)
}

struct Lab {
    config: PiConfig,
    server: agent::mcp::McpServer,
    gate: Arc<agent::delivery::DeliveryGate>,
}

/// One case's config, session and MCP server. `fake`: the local stand-in for api.anthropic.com (`None`: the real API).
async fn lab(base: &Path, case: &str, model: &ApiModel, fake: Option<std::net::SocketAddr>, tools: &[&str], key: &str) -> Result<Lab> {
    let p = provider::provider(provider::ANTHROPIC).unwrap();
    // SAFETY: lab tests run one per process (nextest); nothing else reads the environment concurrently.
    unsafe { std::env::set_var(p.key_env, key) };
    let runtime = tokio::task::spawn_blocking(agent::runtime::Runtime::verified).await??;
    let data = base.join(case);
    let _ = std::fs::remove_dir_all(&data);
    std::fs::create_dir_all(&data)?;
    let catalog = provider::pi_catalog(&runtime.root, p);
    ensure!(catalog.as_ref().and_then(|c| provider::pi_entry(c, &model.id)).is_some(), "Pi's pinned catalog lacks {}", model.id);
    // A key that is not scoped to a workspace names it (the app's check found it; a lab case sets it).
    let workspace = std::env::var(p.workspaces.unwrap().env).ok();
    let dir = agent::runtime::configure_api(&data, model, catalog.as_ref(), &agent::runtime::PiAuth::Key { workspace, api_key: format!("${}", p.key_env) })?;
    let gate = Arc::new(agent::delivery::DeliveryGate::new(true));
    let server = agent::mcp::McpServer::start(Arc::new(Host), Arc::new(Search), gate.clone()).await?;
    let mut config = PiConfig::owned(&runtime, &data, dir, server.url.clone(), server.token.clone());
    config.api_model = Some(model.clone());
    config.engine = None;
    config.working_dir = std::env::temp_dir();
    if tools == PRODUCTION {
        config.prompt = Some(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("prompt/heimdall.md"));
    } else {
        let prompt = data.join("lab-prompt.md");
        std::fs::write(&prompt, "You are Heimdall. Follow the lab test instruction exactly. Keep answers to one short line.")?;
        config.prompt = Some(prompt);
    }
    let restrict = if tools == PRODUCTION { String::new() } else { format!(" pi.on('session_start', () => pi.setActiveTools({}));", serde_json::to_string(tools)?) };
    let ext = data.join("lab-extension.ts");
    let redirect = match fake {
        Some(fake) => format!(
            r#"const fetch = globalThis.fetch;
globalThis.fetch = (input, init) => {{
  const url = typeof input === 'string' || input instanceof URL ? String(input) : input.url;
  return fetch(url.startsWith('https://api.anthropic.com/') ? url.replace('https://api.anthropic.com', 'http://{fake}') : input, init);
}};"#
        ),
        None => String::new(),
    };
    std::fs::write(
        &ext,
        format!(
            "import heimdall from {production};\n{redirect}\nexport default function(pi) {{ heimdall(pi);{restrict} }}\n",
            production = serde_json::to_string(&runtime.extension.to_string_lossy())?,
        ),
    )?;
    config.extension = Some(ext);
    Ok(Lab { config, server, gate })
}

type Frames = Arc<Mutex<Vec<(f64, Value)>>>;

/// Sends prompts one after another on one Pi and waits for each completion: (error, text) per turn, and every frame.
async fn turns(lab: Lab, texts: &[&str], deadline: Duration) -> Result<(Vec<(bool, String)>, Frames)> {
    let start = Instant::now();
    let frames: Frames = Arc::default();
    let saved = frames.clone();
    let mut pi = Pi::launch_config_permitted(lab.config, Some(lab.gate.permit()?))?;
    pi.set_transport_recorder(Arc::new(move |direction, v| {
        if direction == "reply" { saved.lock().unwrap().push((start.elapsed().as_secs_f64(), v.clone())); }
        Ok(())
    }));
    pi.state().await?;
    let mut out = vec![];
    for (i, text) in texts.iter().enumerate() {
        pi.send(agent::protocol::message(&format!("lab-{i}"), text, &[], true, false)?).await?;
        let result = tokio::time::timeout(deadline, async {
            loop {
                match pi.next().await? {
                    Some(Event::Complete { error, text, .. }) => return Ok::<_, anyhow::Error>((error, text)),
                    None => anyhow::bail!("Pi exited"),
                    _ => {}
                }
            }
        })
        .await;
        match result {
            Ok(Ok(r)) => out.push(r),
            Ok(Err(e)) => { let _ = pi.shutdown().await; lab.server.shutdown().await; return Err(e); }
            Err(_) => { let _ = pi.shutdown().await; lab.server.shutdown().await; anyhow::bail!("lab turn deadline"); }
        }
    }
    pi.shutdown().await?;
    lab.server.shutdown().await;
    Ok((out, frames))
}

async fn serve(app: Router) -> Result<(std::net::SocketAddr, tokio::task::JoinHandle<()>)> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    Ok((addr, tokio::spawn(async move { let _ = axum::serve(listener, app).await; })))
}

/// Anthropic Messages streaming, as `/v1/messages` sends it, with the cache fields of `usage`.
fn anthropic_sse(model: &str, blocks: &[Value], stop: &str, usage: Value) -> Response {
    let mut out = String::new();
    let mut event = |v: Value| out.push_str(&format!("event: {}\ndata: {v}\n\n", v["type"].as_str().unwrap()));
    event(json!({"type":"message_start","message":{"id":"msg_lab","type":"message","role":"assistant","model":model,"content":[],"stop_reason":null,"stop_sequence":null,"usage":usage}}));
    for (i, b) in blocks.iter().enumerate() {
        if b["type"] == "tool_use" {
            event(json!({"type":"content_block_start","index":i,"content_block":{"type":"tool_use","id":b["id"],"name":b["name"],"input":{}}}));
            event(json!({"type":"content_block_delta","index":i,"delta":{"type":"input_json_delta","partial_json":b["input"].to_string()}}));
        } else {
            event(json!({"type":"content_block_start","index":i,"content_block":{"type":"text","text":""}}));
            event(json!({"type":"content_block_delta","index":i,"delta":{"type":"text_delta","text":b["text"]}}));
        }
        event(json!({"type":"content_block_stop","index":i}));
    }
    event(json!({"type":"message_delta","delta":{"stop_reason":stop,"stop_sequence":null},"usage":{"output_tokens":24}}));
    event(json!({"type":"message_stop"}));
    ([("content-type", "text/event-stream")], out).into_response()
}

#[derive(Clone, Default)]
struct Seen(Arc<Mutex<Vec<(HeaderMap, Value)>>>);
impl Seen {
    fn push(&self, headers: &HeaderMap, body: &Value) -> usize {
        let mut v = self.0.lock().unwrap();
        v.push((headers.clone(), body.clone()));
        v.len()
    }
    fn all(&self) -> Vec<(HeaderMap, Value)> {
        self.0.lock().unwrap().clone()
    }
}
fn header(h: &HeaderMap, name: &str) -> Option<String> {
    h.get(name).and_then(|v| v.to_str().ok()).map(str::to_owned)
}
/// Every `cache_control` marker in a request body, by place.
fn cache_markers(body: &Value) -> Vec<(String, Value)> {
    let mut out = vec![];
    for (i, b) in body["system"].as_array().into_iter().flatten().enumerate() {
        if !b["cache_control"].is_null() { out.push((format!("system[{i}]"), b["cache_control"].clone())); }
    }
    for (i, t) in body["tools"].as_array().into_iter().flatten().enumerate() {
        if !t["cache_control"].is_null() { out.push((format!("tools[{i}]"), t["cache_control"].clone())); }
    }
    for (i, m) in body["messages"].as_array().into_iter().flatten().enumerate() {
        for b in m["content"].as_array().into_iter().flatten() {
            if !b["cache_control"].is_null() { out.push((format!("messages[{i}].{}", m["role"].as_str().unwrap_or("?")), b["cache_control"].clone())); }
        }
    }
    out
}

fn receipt(base: &Path, case: &str, mut v: Value) -> Result<()> {
    v["date_unix_ms"] = json!(agent::now_ms());
    let text = serde_json::to_vec_pretty(&v)?;
    ensure!(!String::from_utf8_lossy(&text).contains(LAB_KEY), "receipt must not contain the key");
    if let Ok(real) = std::env::var("ANTHROPIC_API_KEY") { ensure!(real.len() < 8 || !String::from_utf8_lossy(&text).contains(&real), "receipt must not contain the key"); }
    agent::runtime::atomic_write(&base.join(format!("{case}-receipt.json")), &text)?;
    println!("{case}: {v}");
    Ok(())
}

/// Claude Opus 5.5 on Pi's built-in Anthropic provider: the key goes in `x-api-key` (no Authorization), the request
/// carries Heimdall's 16,000-token output limit, adaptive thinking with no budget, effort low (Heimdall's thinking level) as the conversation's
/// effort message, and one-hour cache markers on the system prompt, the last tool and the last user turn; a streamed
/// tool call runs Heimdall's MCP tool before the streamed answer, and Pi reports the cache counts the API returned.
#[tokio::test]
#[ignore = "explicit pinned-runtime lab: HEIMDALL_ANTHROPIC_LAB"]
async fn opus_streams_a_tool_call_with_hour_cache_markers_and_low_effort() -> Result<()> {
    let base = lab_base()?;
    let seen = Seen::default();
    let s = seen.clone();
    let app = Router::new().route("/v1/messages", post(move |headers: HeaderMap, body: Bytes| {
        let s = s.clone();
        async move {
            let body: Value = serde_json::from_slice(&body).unwrap();
            let n = s.push(&headers, &body);
            if body.to_string().contains("TIMELINE_LAB_RESULT") {
                anthropic_sse(provider::OPUS, &[json!({"type":"text","text":"OPUS_TOOL_OK"})], "end_turn", json!({"input_tokens":40,"cache_read_input_tokens":2600,"cache_creation_input_tokens":120,"cache_creation":{"ephemeral_5m_input_tokens":0,"ephemeral_1h_input_tokens":120},"output_tokens":1}))
            } else {
                let _ = n;
                anthropic_sse(provider::OPUS, &[json!({"type":"tool_use","id":"toolu_lab","name":"mcp__heimdall__timeline_search","input":{"query":"anthropic-lab","limit":1}})], "tool_use", json!({"input_tokens":12,"cache_read_input_tokens":0,"cache_creation_input_tokens":2600,"cache_creation":{"ephemeral_5m_input_tokens":0,"ephemeral_1h_input_tokens":2600},"output_tokens":1}))
            }
        }
    }));
    let (addr, task) = serve(app).await?;
    let opus = ApiModel::anthropic(provider::OPUS).unwrap();
    let lab = lab(&base, "opus-tool", &opus, Some(addr), &["mcp__heimdall__timeline_search"], LAB_KEY).await?;
    let (results, frames) = turns(lab, &["Call mcp__heimdall__timeline_search once with query anthropic-lab and limit 1, then reply OPUS_TOOL_OK."], Duration::from_secs(120)).await?;
    task.abort();
    let requests = seen.all();
    let (error, text) = results[0].clone();
    ensure!(!error && text.contains("OPUS_TOOL_OK"), "turn failed: {text}");
    ensure!(requests.len() == 2, "expected tool call then answer: {}", requests.len());
    let mut shapes = vec![];
    for (h, body) in &requests {
        ensure!(header(h, "x-api-key").as_deref() == Some(LAB_KEY), "key not sent as x-api-key");
        ensure!(header(h, "authorization").is_none(), "no Authorization header with an API key");
        ensure!(header(h, "anthropic-version").is_some(), "anthropic-version missing");
        ensure!(body["model"] == provider::OPUS && body["stream"] == true && body["max_tokens"] == 128_000, "request shape: model/stream/max_tokens {}", json!([body["model"], body["stream"], body["max_tokens"]]));
        ensure!(body["thinking"]["type"] == "adaptive" && body["thinking"]["budget_tokens"].is_null(), "adaptive thinking, no budget: {}", body["thinking"]);
        ensure!(body["temperature"].is_null(), "no sampling parameters on Opus 5.5");
        ensure!(body["tool_choice"].is_null() || body["tool_choice"]["type"] == "auto", "no forced tool choice (400 on Opus 5.5)");
        let messages = body["messages"].as_array().context("messages")?;
        let last = messages.last().context("no messages")?;
        ensure!(last["role"] == "system" && last["output_config"]["effort"] == agent::runtime::THINKING, "the conversation's effort is Heimdall's thinking level: {last}");
        let markers = cache_markers(body);
        ensure!(markers.iter().all(|(_, c)| *c == json!({"type":"ephemeral","ttl":"1h"})), "every marker is the one-hour cache: {markers:?}");
        ensure!(markers.iter().any(|(at, _)| at.starts_with("system")), "system prompt not cached: {markers:?}");
        ensure!(markers.iter().any(|(at, _)| at.starts_with("tools")), "tools not cached: {markers:?}");
        ensure!(markers.iter().any(|(at, _)| at.starts_with("messages") && at.ends_with(".user")), "history not cached: {markers:?}");
        ensure!(markers.len() <= 4, "at most four breakpoints: {markers:?}");
        shapes.push(json!({"betas":header(h, "anthropic-beta"),"thinking":body["thinking"],"output_config":body["output_config"],"last_message":last,"cache_markers":markers.iter().map(|(at, c)| json!([at, c])).collect::<Vec<_>>(),"max_tokens":body["max_tokens"],"tools":body["tools"].as_array().map(|t| t.len())}));
    }
    ensure!(requests[1].1.to_string().contains("TIMELINE_LAB_RESULT"), "tool result not returned to the model");
    let frames = frames.lock().unwrap();
    ensure!(frames.iter().filter(|(_, v)| v["type"] == "tool_execution_start").count() == 1, "exactly one MCP tool call");
    let usages: Vec<Value> = frames.iter().filter(|(_, v)| v["type"] == "message_end" && v["message"]["role"] == "assistant").map(|(_, v)| v["message"]["usage"].clone()).collect();
    ensure!(usages.len() == 2 && usages[0]["cacheWrite"] == 2600 && usages[0]["cacheWrite1h"] == 2600 && usages[1]["cacheRead"] == 2600, "Pi reports the cache counts: {usages:?}");
    // What Heimdall's spend estimate makes of them (one-hour writes at 2× input).
    let cost: f64 = usages.iter().filter_map(provider::Usage::from_pi).filter_map(|u| opus.request_cost(&u, true)).sum();
    receipt(&base, "opus-tool", json!({"requests":shapes,"usage":usages,"estimated_usd":cost}))
}

/// The cache-refresh ping (day pass 26 Oct, `maintenance::KeepWarm`) through the real pinned Pi: its request carries
/// the one-hour cache markers like every request, so reading the cached history refreshes the hour; the ping's words
/// are the last user turn; the agent's `NO_REPLY` is a silent reply (nothing said, nothing shown).
#[tokio::test]
#[ignore = "explicit pinned-runtime lab: HEIMDALL_ANTHROPIC_LAB"]
async fn cache_ping_carries_the_hour_cache_and_its_no_reply_is_silent() -> Result<()> {
    let base = lab_base()?;
    let seen = Seen::default();
    let s = seen.clone();
    let app = Router::new().route("/v1/messages", post(move |headers: HeaderMap, body: Bytes| {
        let s = s.clone();
        async move {
            let body: Value = serde_json::from_slice(&body).unwrap();
            let n = s.push(&headers, &body);
            let (text, usage) = if n == 1 {
                ("FIRST_OK", json!({"input_tokens":12,"cache_read_input_tokens":0,"cache_creation_input_tokens":2600,"cache_creation":{"ephemeral_5m_input_tokens":0,"ephemeral_1h_input_tokens":2600},"output_tokens":1}))
            } else {
                (agent::reply::SILENT, json!({"input_tokens":4,"cache_read_input_tokens":2600,"cache_creation_input_tokens":90,"cache_creation":{"ephemeral_5m_input_tokens":0,"ephemeral_1h_input_tokens":90},"output_tokens":3}))
            };
            anthropic_sse(provider::OPUS, &[json!({"type":"text","text":text})], "end_turn", usage)
        }
    }));
    let (addr, task) = serve(app).await?;
    let opus = ApiModel::anthropic(provider::OPUS).unwrap();
    let lab = lab(&base, "cache-ping", &opus, Some(addr), &[], LAB_KEY).await?;
    let ping = agent::harness::marked_prompt("cache-ping-lab", agent::maintenance::PING_TEXT, agent::now_ms());
    let (results, frames) = turns(lab, &["Reply FIRST_OK.", &ping], Duration::from_secs(120)).await?;
    task.abort();
    let requests = seen.all();
    ensure!(requests.len() == 2, "one request per turn: {}", requests.len());
    let (_, body) = &requests[1];
    let markers = cache_markers(body);
    ensure!(!markers.is_empty() && markers.iter().all(|(_, c)| *c == json!({"type":"ephemeral","ttl":"1h"})), "the ping asks for the one-hour cache: {markers:?}");
    ensure!(markers.iter().any(|(at, _)| at.starts_with("messages") && at.ends_with(".user")), "the history is a cache breakpoint: {markers:?}");
    let last_user = body["messages"].as_array().context("messages")?.iter().rev().find(|m| m["role"] == "user").context("no user turn")?;
    ensure!(last_user.to_string().contains("[Heimdall cache refresh]"), "the ping is the last user turn: {last_user}");
    let (error, text) = results[1].clone();
    ensure!(!error && text.trim() == agent::reply::SILENT, "the ping's answer: {text}");
    let mut replies = agent::reply::Replies::default();
    ensure!(replies.finish(&text, "stop") == agent::reply::Final::default(), "NO_REPLY says and shows nothing");
    let frames = frames.lock().unwrap();
    let usages: Vec<Value> = frames.iter().filter(|(_, v)| v["type"] == "message_end" && v["message"]["role"] == "assistant").map(|(_, v)| v["message"]["usage"].clone()).collect();
    ensure!(usages.len() == 2 && usages[1]["cacheRead"] == 2600, "the ping read the cached history: {usages:?}");
    receipt(&base, "cache-ping", json!({"cache_markers":markers.iter().map(|(at, c)| json!([at, c])).collect::<Vec<_>>(),"betas":header(&requests[1].0, "anthropic-beta"),"usage":usages}))
}

/// Anthropic's failures through the production adapter: a rejected key fails the turn once in words; spent credits
/// fail it once as "Anthropic credits used up" (never waited on); 529 Overloaded is resent with backoff and answers.
#[tokio::test]
#[ignore = "explicit pinned-runtime lab: HEIMDALL_ANTHROPIC_LAB"]
async fn rejected_keys_spent_credits_and_overload_are_named_and_bounded() -> Result<()> {
    let base = lab_base()?;
    let mut receipts = serde_json::Map::new();
    for (case, plan) in [("rejected", vec![401u16]), ("credits", vec![400]), ("overloaded", vec![529, 529, 200])] {
        let seen = Seen::default();
        let s = seen.clone();
        let plan = Arc::new(plan.clone());
        let app = Router::new().route("/v1/messages", post(move |headers: HeaderMap, body: Bytes| {
            let (s, plan) = (s.clone(), plan.clone());
            async move {
                let body: Value = serde_json::from_slice(&body).unwrap_or_default();
                let n = s.push(&headers, &body);
                let json = [("content-type", "application/json")];
                match plan.get(n - 1).copied().unwrap_or(200) {
                    401 => (StatusCode::UNAUTHORIZED, json, r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#).into_response(),
                    400 => (StatusCode::BAD_REQUEST, json, r#"{"type":"error","error":{"type":"invalid_request_error","message":"Your credit balance is too low to access the Anthropic API. Please go to Plans & Billing to upgrade or purchase credits."}}"#).into_response(),
                    529 => (StatusCode::from_u16(529).unwrap(), json, r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#).into_response(),
                    _ => anthropic_sse(provider::OPUS, &[json!({"type":"text","text":"OVERLOAD_OK"})], "end_turn", json!({"input_tokens":20,"output_tokens":1})),
                }
            }
        }));
        let (addr, task) = serve(app).await?;
        let lab = lab(&base, &format!("errors-{case}"), &ApiModel::anthropic(provider::OPUS).unwrap(), Some(addr), &[], LAB_KEY).await?;
        let started = Instant::now();
        let (results, frames) = turns(lab, &["Reply OVERLOAD_OK."], Duration::from_secs(90)).await?;
        task.abort();
        let (error, text) = results[0].clone();
        let n = seen.all().len();
        let frames = frames.lock().unwrap();
        let problems: Vec<String> = frames.iter().filter(|(_, v)| v["type"] == "heimdall_provider_problem").map(|(_, v)| v["message"].as_str().unwrap_or_default().to_owned()).collect();
        let failure = frames.iter().rev().find(|(_, v)| v["type"] == "message_end" && v["message"]["stopReason"] == "error").map(|(_, v)| v["message"]["errorMessage"].as_str().unwrap_or_default().to_owned());
        let shown = failure.as_deref().and_then(|f| provider::explain_error(provider::ANTHROPIC, f));
        match case {
            "rejected" => {
                ensure!(error && n == 1, "a rejected key is not retried: {n}");
                ensure!(problems == ["Anthropic: key rejected"] && shown.as_deref() == Some("Anthropic: key rejected"), "{problems:?} {failure:?}");
            }
            "credits" => {
                ensure!(error && n == 1, "spent credits are not retried: {n}");
                ensure!(problems == ["Anthropic credits used up"] && shown.as_deref() == Some("Anthropic credits used up"), "{problems:?} {failure:?}");
            }
            _ => {
                ensure!(!error && text.contains("OVERLOAD_OK") && n == 3, "overload backoff: {text} {n}");
                ensure!(started.elapsed() >= Duration::from_secs(3), "1 s then 2 s backoff");
                ensure!(frames.iter().filter(|(_, v)| v["type"] == "heimdall_provider_wait" && v["delay_ms"].as_u64().is_some_and(|d| d > 0)).count() == 2, "two waits shown");
            }
        }
        receipts.insert(case.into(), json!({"requests":n,"error":error,"problems":problems,"failure":failure,"shown":shown,"elapsed_s":started.elapsed().as_secs_f64()}));
    }
    receipt(&base, "errors", Value::Object(receipts))
}

/// A synthetic session whose last Opus request started `idle` ago with `prompt` tokens of history.
fn idle_session(path: &Path, idle: Duration, prompt: u64) -> Result<()> {
    // Pi reads the request start from each message's `timestamp` (ms); the entry stamps are informational.
    let at = agent::now_ms() - idle.as_millis() as u64;
    let iso = |_: u64| "2026-10-18T00:00:00.000Z";
    let mut lines = vec![json!({"type":"session","version":3,"id":"anthropic-idle-lab","timestamp":iso(at),"cwd":std::env::temp_dir()})];
    let mut parent: Option<String> = None;
    for i in 0..40 {
        let id = format!("u{i}");
        lines.push(json!({"type":"message","id":id,"parentId":parent,"timestamp":iso(at),"message":{"role":"user","content":[{"type":"text","text":"Synthetic microphone observation: discussed the build and tool results. ".repeat(60)}],"timestamp":at - 1000}}));
        parent = Some(id);
        let id = format!("a{i}");
        lines.push(json!({"type":"message","id":id,"parentId":parent,"timestamp":iso(at),"message":{"role":"assistant","content":[{"type":"text","text":"Observation retained."}],"api":"anthropic-messages","provider":"anthropic","model":provider::OPUS,"stopReason":"stop","usage":{"input":200,"output":18,"cacheRead":prompt - 200,"cacheWrite":0,"totalTokens":prompt + 18,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}},"timestamp":at}}));
        parent = Some(id);
    }
    std::fs::write(path, lines.iter().map(Value::to_string).collect::<Vec<_>>().join("\n") + "\n")?;
    Ok(())
}

/// Night pass H1 (19 Oct 18:12: the user's first words after Agent on waited 45 s for a cold-cache compaction inside their
/// prompt): the extension never compacts in front of a message. A message after an hour idle on a 120K-token history
/// goes out as is, like one on a warm cache; Heimdall compacts a cold cache while the agent is idle instead
/// (`maintenance`, `Bridge::maintain`, sim `n1_…`).
#[tokio::test]
#[ignore = "explicit pinned-runtime lab: HEIMDALL_ANTHROPIC_LAB"]
async fn a_message_never_waits_for_a_compaction_whatever_the_cache() -> Result<()> {
    let base = lab_base()?;
    let mut receipts = serde_json::Map::new();
    for (case, idle) in [("cold", Duration::from_secs(61 * 60)), ("warm", Duration::from_secs(5 * 60))] {
        let seen = Seen::default();
        let s = seen.clone();
        let app = Router::new().route("/v1/messages", post(move |headers: HeaderMap, body: Bytes| {
            let s = s.clone();
            async move {
                let body: Value = serde_json::from_slice(&body).unwrap();
                let summary = body.to_string().contains("<conversation>");
                s.push(&headers, &body);
                let text = if summary { "# Summary\nSynthetic observations reviewed. Reply CACHE_OK next." } else { "CACHE_OK" };
                anthropic_sse(provider::OPUS, &[json!({"type":"text","text":text})], "end_turn", json!({"input_tokens":3000,"output_tokens":1}))
            }
        }).layer(axum::extract::DefaultBodyLimit::max(8 * 1024 * 1024)));
        let (addr, task) = serve(app).await?;
        let mut opus = ApiModel::anthropic(provider::OPUS).unwrap();
        opus.context_budget = Some(501_714);
        let lab = lab(&base, &format!("idle-{case}"), &opus, Some(addr), &[], LAB_KEY).await?;
        idle_session(&lab.config.session, idle, 120_000)?;
        let session = lab.config.session.clone();
        let (results, frames) = turns(lab, &["Reply CACHE_OK."], Duration::from_secs(120)).await?;
        task.abort();
        let requests = seen.all();
        let (error, text) = results[0].clone();
        ensure!(!error && text.contains("CACHE_OK"), "{case}: the turn failed: {text}");
        let summaries: Vec<bool> = requests.iter().map(|(_, b)| b.to_string().contains("<conversation>")).collect();
        let frames = frames.lock().unwrap();
        let compacting = frames.iter().any(|(_, v)| v["type"] == "compaction_start");
        let compacted = std::fs::read_to_string(&session)?.lines().filter_map(|l| serde_json::from_str::<Value>(l).ok()).any(|v| v["type"] == "compaction");
        ensure!(summaries == [false] && !compacting && !compacted, "{case}: the message goes out as is: {summaries:?}");
        receipts.insert(case.into(), json!({"idle_s":idle.as_secs(),"requests":summaries,"chars":requests.iter().map(|(_, b)| b.to_string().len()).collect::<Vec<_>>()}));
    }
    receipt(&base, "idle-compaction", Value::Object(receipts))
}

/// The user's key (18 Oct) is not scoped to a workspace: Anthropic refuses every request without
/// `anthropic-workspace-id`. With the workspace the app found, every request Pi makes carries it, the compaction
/// summary included; the key still travels only as `x-api-key`.
#[tokio::test]
#[ignore = "explicit pinned-runtime lab: HEIMDALL_ANTHROPIC_LAB"]
async fn an_unscoped_key_names_its_workspace_on_replies_and_compaction() -> Result<()> {
    let base = lab_base()?;
    let p = provider::provider(provider::ANTHROPIC).unwrap();
    // SAFETY: lab tests run one per process.
    unsafe { std::env::set_var(p.workspaces.unwrap().env, "wrkspc_lab01") };
    let seen = Seen::default();
    let s = seen.clone();
    let app = Router::new().route("/v1/messages", post(move |headers: HeaderMap, body: Bytes| {
        let s = s.clone();
        async move {
            let body: Value = serde_json::from_slice(&body).unwrap();
            s.push(&headers, &body);
            if header(&headers, "anthropic-workspace-id").as_deref() != Some("wrkspc_lab01") {
                return (StatusCode::BAD_REQUEST, [("content-type", "application/json")], r#"{"type":"error","error":{"type":"invalid_request_error","message":"This API key is not scoped to a workspace, so this request must include the anthropic-workspace-id header"}}"#).into_response();
            }
            let summary = body.to_string().contains("<conversation>");
            let text = if summary { "# Summary\nSynthetic observations reviewed. Reply WORKSPACE_OK next." } else { "WORKSPACE_OK" };
            anthropic_sse(provider::OPUS, &[json!({"type":"text","text":text})], "end_turn", json!({"input_tokens":3000,"output_tokens":1}))
        }
    }).layer(axum::extract::DefaultBodyLimit::max(8 * 1024 * 1024)));
    let (addr, task) = serve(app).await?;
    let mut opus = ApiModel::anthropic(provider::OPUS).unwrap();
    opus.context_budget = Some(501_714);
    let lab = lab(&base, "workspace", &opus, Some(addr), &[], "sk-ant-usr-lab-0123456789abcdefghijklmnop").await?;
    let raw = std::fs::read_to_string(lab.config.agent_dir.join("models.json"))?;
    ensure!(!raw.contains("sk-ant-usr-lab"), "the key never reaches Pi's files");
    idle_session(&lab.config.session, Duration::from_secs(61 * 60), 120_000)?;
    let (results, _) = turns(lab, &["Reply WORKSPACE_OK."], Duration::from_secs(120)).await?;
    task.abort();
    let requests = seen.all();
    let (error, text) = results[0].clone();
    ensure!(!error && text.contains("WORKSPACE_OK"), "the turn failed: {text}");
    let summaries: Vec<bool> = requests.iter().map(|(_, b)| b.to_string().contains("<conversation>")).collect();
    ensure!(summaries == [true, false], "compaction summary, then the answer: {summaries:?}");
    for (h, _) in &requests {
        ensure!(header(h, "anthropic-workspace-id").as_deref() == Some("wrkspc_lab01"), "workspace header missing");
        ensure!(header(h, "x-api-key").as_deref() == Some("sk-ant-usr-lab-0123456789abcdefghijklmnop") && header(h, "authorization").is_none(), "key header");
    }
    receipt(&base, "workspace", json!({"requests":summaries,"workspace_headers":requests.iter().map(|(h, _)| header(h, "anthropic-workspace-id")).collect::<Vec<_>>()}))
}

/// "how much thinking do we allow for the models". One Pi, three turns on Claude Sonnet 5.5: at Low
/// Pi asks for adaptive thinking at effort low; switched to Off without a restart, the next request is Anthropic's
/// `between_tools` with one top-level effort (low) and no per-message effort (which `between_tools` rejects); at
/// Max, adaptive thinking at effort max. On Opus, Off is not a level: Pi stays at its least thinking, Low.
#[tokio::test]
#[ignore = "explicit pinned-runtime lab: HEIMDALL_ANTHROPIC_LAB"]
async fn thinking_levels_change_without_a_restart_and_off_follows_anthropics_rules() -> Result<()> {
    let base = lab_base()?;
    let seen = Seen::default();
    let s = seen.clone();
    let app = Router::new().route("/v1/messages", post(move |headers: HeaderMap, body: Bytes| {
        let s = s.clone();
        async move {
            let body: Value = serde_json::from_slice(&body).unwrap();
            s.push(&headers, &body);
            // Anthropic's rules for Claude Sonnet 5.5.
            let thinking = &body["thinking"];
            let effort_messages = body["messages"].as_array().map_or(0, |m| m.iter().filter(|m| m["role"] == "system" && !m["output_config"].is_null()).count());
            let invalid = thinking["type"] == "disabled"
                || (thinking["type"] == "between_tools" && (thinking.as_object().map_or(0, |o| o.len()) != 1 || effort_messages > 0
                    || matches!(body["output_config"]["effort"].as_str(), Some("xhigh" | "max"))));
            if invalid {
                return (StatusCode::BAD_REQUEST, [("content-type", "application/json")], r#"{"type":"error","error":{"type":"invalid_request_error","message":"thinking: invalid for this model"}}"#).into_response();
            }
            anthropic_sse(provider::SONNET, &[json!({"type":"text","text":"THINK_OK"})], "end_turn", json!({"input_tokens":30,"output_tokens":1}))
        }
    }));
    let (addr, task) = serve(app).await?;
    let sonnet = ApiModel::anthropic(provider::SONNET).unwrap();
    let sonnet_lab = lab(&base, "thinking", &sonnet, Some(addr), &[], LAB_KEY).await?;
    let mut pi = Pi::launch_config_permitted(sonnet_lab.config, Some(sonnet_lab.gate.permit()?))?;
    pi.state().await?;
    let mut levels = vec![];
    for (i, level) in [None, Some("off"), Some("max")].into_iter().enumerate() {
        if let Some(level) = level { levels.push(pi.set_thinking(level).await?); }
        pi.send(agent::protocol::message(&format!("lab-{i}"), "Reply THINK_OK.", &[], true, false)?).await?;
        let (error, text) = tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                match pi.next().await? {
                    Some(Event::Complete { error, text, .. }) => return Ok::<_, anyhow::Error>((error, text)),
                    None => anyhow::bail!("Pi exited"),
                    _ => {}
                }
            }
        }).await??;
        ensure!(!error && text.contains("THINK_OK"), "turn {i} failed: {text}");
    }
    let pid = pi.pid();
    pi.shutdown().await?;
    sonnet_lab.server.shutdown().await;
    task.abort();
    ensure!(levels == ["off", "max"], "Pi runs the chosen levels: {levels:?}");
    let requests = seen.all();
    ensure!(requests.len() == 3, "one request per turn, one Pi ({pid:?}): {}", requests.len());
    let shape = |b: &Value| (b["thinking"]["type"].as_str().unwrap_or_default().to_owned(), b["output_config"]["effort"].as_str().map(str::to_owned),
        b["messages"].as_array().unwrap().iter().rev().find(|m| m["role"] == "system" && !m["output_config"].is_null()).map(|m| m["output_config"]["effort"].as_str().unwrap_or_default().to_owned()));
    let shapes: Vec<_> = requests.iter().map(|(_, b)| shape(b)).collect();
    ensure!(shapes[0] == ("adaptive".into(), Some("high".into()), Some("low".into())), "Low: adaptive, per-message effort low: {:?}", shapes[0]);
    ensure!(shapes[1] == ("between_tools".into(), Some("low".into()), None), "Off: between_tools, effort low, no per-message effort: {:?}", shapes[1]);
    ensure!(requests[1].1["thinking"] == json!({"type":"between_tools"}), "between_tools takes no other field: {}", requests[1].1["thinking"]);
    ensure!(shapes[2] == ("adaptive".into(), Some("high".into()), Some("max".into())), "Max: adaptive, per-message effort max: {:?}", shapes[2]);
    // Opus: Off is clamped to Low by Pi itself (its catalog has no Off).
    let opus = ApiModel::anthropic(provider::OPUS).unwrap();
    let lab2 = lab(&base, "thinking-opus", &opus, None, &[], LAB_KEY).await?;
    let mut pi = Pi::launch_config_permitted(lab2.config, Some(lab2.gate.permit()?))?;
    pi.state().await?;
    let opus_off = pi.set_thinking("off").await?;
    pi.shutdown().await?;
    lab2.server.shutdown().await;
    ensure!(opus_off == "low", "Opus cannot turn thinking off: {opus_off}");
    receipt(&base, "thinking", json!({"sonnet":shapes.iter().map(|s| json!(s)).collect::<Vec<_>>(),"levels":levels,"opus_off":opus_off}))
}

fn real_key() -> Result<String> {
    std::env::var("ANTHROPIC_API_KEY").ok().filter(|k| k.starts_with("sk-ant-")).context("ANTHROPIC_API_KEY (a real key) is required for real_ cases")
}

/// The real API, paid (two short turns each on Haiku and Opus with production's prompt and tools, about 15 cents):
/// the second request reads the first's prefix from the cache. Writes the measured cached and uncached input per
/// request to `real-cache-receipt.json`.
#[tokio::test]
#[ignore = "explicit paid acceptance: HEIMDALL_ANTHROPIC_LAB and ANTHROPIC_API_KEY"]
async fn real_api_second_request_reads_the_cached_prefix() -> Result<()> {
    let base = lab_base()?;
    let key = real_key()?;
    // SAFETY: lab tests run one per process.
    unsafe { std::env::remove_var("ANTHROPIC_API_KEY") };
    let mut results = serde_json::Map::new();
    for id in [provider::HAIKU, provider::OPUS] {
        let model = ApiModel::anthropic(id).unwrap();
        let lab = lab(&base, &format!("real-cache-{id}"), &model, None, PRODUCTION, &key).await?;
        let (out, frames) = turns(lab, &["Reply with the single word ONE.", "Reply with the single word TWO."], Duration::from_secs(120)).await?;
        ensure!(out.iter().all(|(e, _)| !e), "{id}: a real turn failed: {out:?}");
        let usages: Vec<Value> = frames.lock().unwrap().iter().filter(|(_, v)| v["type"] == "message_end" && v["message"]["role"] == "assistant").map(|(_, v)| v["message"]["usage"].clone()).collect();
        let cost: f64 = usages.iter().filter_map(provider::Usage::from_pi).filter_map(|u| model.request_cost(&u, true)).sum();
        results.insert(id.into(), json!({"usage":usages,"answers":out.iter().map(|(_, t)| t.clone()).collect::<Vec<_>>(),"estimated_usd":cost}));
        ensure!(usages.len() == 2 && usages[1]["cacheRead"].as_u64().unwrap_or(0) > 0, "{id}: the second request did not read the cache: {usages:?}");
    }
    receipt(&base, "real-cache", Value::Object(results))
}

/// The real API, paid: a tiny Opus conversation, compacted through Pi as Heimdall's compaction does it, then one more
/// turn. Preserved thinking binds thinking blocks to the conversation; the turn after the rewrite must not fail with
/// "Invalid signature in thinking block".
#[tokio::test]
#[ignore = "explicit paid acceptance: HEIMDALL_ANTHROPIC_LAB and ANTHROPIC_API_KEY"]
async fn real_api_turn_after_compaction_keeps_working() -> Result<()> {
    let base = lab_base()?;
    let key = real_key()?;
    unsafe { std::env::remove_var("ANTHROPIC_API_KEY") };
    let model = ApiModel::anthropic(provider::OPUS).unwrap();
    let lab = lab(&base, "real-compaction", &model, None, &["mcp__heimdall__timeline_search"], &key).await?;
    let start = Instant::now();
    let frames: Frames = Arc::default();
    let saved = frames.clone();
    let mut pi = Pi::launch_config_permitted(lab.config, Some(lab.gate.permit()?))?;
    pi.set_transport_recorder(Arc::new(move |direction, v| {
        if direction == "reply" { saved.lock().unwrap().push((start.elapsed().as_secs_f64(), v.clone())); }
        Ok(())
    }));
    pi.state().await?;
    let mut answers = vec![];
    answers.push(ask(&mut pi, "c1", "Remember the codeword HEIMDALL-ORANGE. Think briefly about why it might matter, then call mcp__heimdall__timeline_search once with query codeword and limit 1, and reply DONE.").await?);
    answers.push(ask(&mut pi, "c2", "Reply with the single word AGAIN.").await?);
    pi.compact().await?;
    answers.push(ask(&mut pi, "c3", "What was the codeword? Reply with it only.").await?);
    let _ = pi.shutdown().await;
    lab.server.shutdown().await;
    let frames = frames.lock().unwrap();
    let errors: Vec<String> = frames.iter().filter(|(_, v)| v["type"] == "message_end" && v["message"]["stopReason"] == "error").map(|(_, v)| v["message"]["errorMessage"].as_str().unwrap_or_default().to_owned()).collect();
    ensure!(errors.iter().all(|e| !e.contains("signature")), "a thinking block was rejected after compaction: {errors:?}");
    ensure!(answers.iter().all(|(e, _)| !e) && answers[2].1.contains("HEIMDALL-ORANGE"), "turns after compaction: {answers:?}");
    let usages: Vec<Value> = frames.iter().filter(|(_, v)| v["type"] == "message_end" && v["message"]["role"] == "assistant").map(|(_, v)| v["message"]["usage"].clone()).collect();
    receipt(&base, "real-compaction", json!({"answers":answers,"errors":errors,"usage":usages}))
}

async fn ask(pi: &mut Pi, id: &str, text: &str) -> Result<(bool, String)> {
    pi.send(agent::protocol::message(id, text, &[], true, false)?).await?;
    tokio::time::timeout(Duration::from_secs(120), async {
        loop {
            match pi.next().await? {
                Some(Event::Complete { error, text, .. }) => return Ok::<_, anyhow::Error>((error, text)),
                None => anyhow::bail!("Pi exited"),
                _ => {}
            }
        }
    }).await.context("turn deadline")?
}
