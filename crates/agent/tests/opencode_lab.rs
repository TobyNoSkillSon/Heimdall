//! Explicit lab contracts for OpenCode Go through Heimdall's real Pi harness, production extension and MCP server.
//! A local fake stands in for opencode.ai: the lab extension rewrites only the native fetch destination, so Pi's
//! built-in OpenCode Go provider and Heimdall's provider adapter still see the production URLs. No cloud request,
//! installed app, real profile or Keychain is involved; the key is a lab placeholder in the environment.
//! Pi 1.4.0 serves its catalog models from `opencode.ai/inference/go`; a Console sign-in keeps the base URL it reports.
//!
//! Run: HEIMDALL_OPENCODE_LAB=<abs path containing /target/opencode-lab/> cargo nextest run -p agent \
//!      --test opencode_lab --run-ignored only
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

const LAB_KEY: &str = "sk-lab-0123456789abcdefghijklmnop";

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
    let base = PathBuf::from(std::env::var("HEIMDALL_OPENCODE_LAB").context("HEIMDALL_OPENCODE_LAB")?);
    ensure!(base.is_absolute() && base.to_string_lossy().contains("/target/opencode-lab"), "isolated lab path required");
    Ok(base)
}

/// One pinned runtime for every lab case (the build cache's, or `HEIMDALL_AGENT_RUNTIME`, e.g. a candidate
/// bundle's); each case has its own config, session and fake server.
async fn runtime(_base: &Path) -> Result<agent::runtime::Runtime> {
    tokio::task::spawn_blocking(agent::runtime::Runtime::verified).await?
}

struct Lab {
    config: PiConfig,
    server: agent::mcp::McpServer,
    gate: Arc<agent::delivery::DeliveryGate>,
}

/// How the lab session authenticates: a pasted key, or a Console sign-in (its token handed over in the
/// environment as lab builds do, or fetched by Pi's per-request command as the installed app does).
#[derive(Clone, Copy, PartialEq)]
enum Auth { Key, Console, ConsoleCommand }

const CONSOLE_TOKEN: &str = "console-access-lab-token";

async fn lab(base: &Path, case: &str, model: &ApiModel, fake: std::net::SocketAddr, tools: &[&str]) -> Result<Lab> {
    lab_auth(base, case, model, fake, tools, Auth::Key).await
}

async fn lab_auth(base: &Path, case: &str, model: &ApiModel, fake: std::net::SocketAddr, tools: &[&str], auth: Auth) -> Result<Lab> {
    let p = provider::provider(provider::OPENCODE_GO).unwrap();
    // SAFETY: lab tests run one per process (nextest); nothing else reads the environment concurrently.
    if auth == Auth::Key {
        unsafe { std::env::set_var(p.key_env, LAB_KEY) };
    } else {
        unsafe { std::env::remove_var(p.key_env) };
        provider::save_console_session(p, &agent::console::Session {
            access: CONSOLE_TOKEN.into(), refresh: "lab-refresh".into(), expires_ms: agent::now_ms() + 3_600_000,
            org_id: Some("org-lab".into()), server: "https://opencode.ai/console".into(),
            base_url: Some("https://opencode.ai/zen/go/v1".into()), headers: Default::default(), models: vec![provider::HAIKU.into()], client_id: None,
        })?;
    }
    let runtime = runtime(base).await?;
    let data = base.join(case);
    let _ = std::fs::remove_dir_all(&data);
    std::fs::create_dir_all(&data)?;
    let catalog = provider::pi_catalog(&runtime.root, model.provider_info().unwrap());
    ensure!(catalog.is_some(), "Pi's pinned OpenCode Go catalog is missing from the runtime");
    let pi_auth = match (auth, provider::console_session(p)) {
        (Auth::Key, _) | (_, None) => agent::runtime::PiAuth::Key { workspace: None, api_key: format!("${}", p.key_env) },
        (_, Some(session)) => agent::runtime::PiAuth::Console {
            // The installed app's command prints the current token; this stand-in prints the lab token.
            api_key: if auth == Auth::ConsoleCommand { format!("!printf %s {CONSOLE_TOKEN}") } else { format!("${}", agent::console::TOKEN_ENV) },
            base_url: session.base_url.clone(),
            headers: session.request_headers(),
            models: session.models.clone(),
        },
    };
    let dir = agent::runtime::configure_api(&data, model, catalog.as_ref(), &pi_auth)?;
    let gate = Arc::new(agent::delivery::DeliveryGate::new(true));
    let server = agent::mcp::McpServer::start(Arc::new(Host), Arc::new(Search), gate.clone()).await?;
    let mut config = PiConfig::owned(&runtime, &data, dir, server.url.clone(), server.token.clone());
    config.api_model = Some(model.clone());
    config.engine = None;
    config.working_dir = std::env::temp_dir();
    let prompt = data.join("lab-prompt.md");
    std::fs::write(&prompt, "You are Heimdall. Follow the lab test instruction exactly.")?;
    config.prompt = Some(prompt);
    let ext = data.join("lab-extension.ts");
    std::fs::write(
        &ext,
        format!(
            r#"import heimdall from {production};
{drop_token}
const fetch = globalThis.fetch;
globalThis.fetch = (input, init) => {{
  const url = typeof input === 'string' || input instanceof URL ? String(input) : input.url;
  return fetch(url.startsWith('https://opencode.ai/') ? url.replace('https://opencode.ai', 'http://{fake}') : input, init);
}};
export default function(pi) {{ heimdall(pi); pi.on('session_start', () => pi.setActiveTools({tools})); }}
"#,
            production = serde_json::to_string(&runtime.extension.to_string_lossy())?,
            // The command case: no token in the environment, so Pi must run the configured command per request.
            drop_token = if auth == Auth::ConsoleCommand { "delete process.env.HEIMDALL_OPENCODE_TOKEN;" } else { "" },
            tools = serde_json::to_string(tools)?,
        ),
    )?;
    config.extension = Some(ext);
    Ok(Lab { config, server, gate })
}

type Frames = Arc<Mutex<Vec<(f64, Value)>>>;

/// Sends one prompt and waits for its completion (the turn's error flag and final text).
async fn turn(lab: Lab, id: &str, text: &str, deadline: Duration) -> Result<(bool, String, Frames)> {
    let start = Instant::now();
    let frames: Frames = Arc::default();
    let saved = frames.clone();
    let mut pi = Pi::launch_config_permitted(lab.config, Some(lab.gate.permit()?))?;
    pi.set_transport_recorder(Arc::new(move |direction, v| {
        if direction == "reply" { saved.lock().unwrap().push((start.elapsed().as_secs_f64(), v.clone())); }
        Ok(())
    }));
    pi.state().await?;
    pi.send(agent::protocol::message(id, text, &[], true, false)?).await?;
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
    let stopped = pi.shutdown().await;
    lab.server.shutdown().await;
    let (error, text) = result.context("lab turn deadline")??;
    stopped?;
    Ok((error, text, frames))
}

async fn serve(app: Router) -> Result<(std::net::SocketAddr, tokio::task::JoinHandle<()>)> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    Ok((addr, tokio::spawn(async move { let _ = axum::serve(listener, app).await; })))
}

/// Anthropic Messages streaming, as `/v1/messages` sends it.
fn anthropic_sse(blocks: &[Value], stop: &str, input_tokens: u64) -> Response {
    let mut out = String::new();
    let mut event = |v: Value| out.push_str(&format!("event: {}\ndata: {v}\n\n", v["type"].as_str().unwrap()));
    event(json!({"type":"message_start","message":{"id":"msg_lab","type":"message","role":"assistant","model":provider::HAIKU,"content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":input_tokens,"output_tokens":1}}}));
    for (i, b) in blocks.iter().enumerate() {
        if b["type"] == "tool_use" {
            event(json!({"type":"content_block_start","index":i,"content_block":{"type":"tool_use","id":b["id"],"name":b["name"],"input":{}}}));
            let input = b["input"].to_string();
            let (a, z) = input.split_at(input.len() / 2);
            for part in [a, z] { event(json!({"type":"content_block_delta","index":i,"delta":{"type":"input_json_delta","partial_json":part}})); }
        } else {
            event(json!({"type":"content_block_start","index":i,"content_block":{"type":"text","text":""}}));
            let text = b["text"].as_str().unwrap();
            let (a, z) = text.split_at(text.len() / 2);
            for part in [a, z] { event(json!({"type":"content_block_delta","index":i,"delta":{"type":"text_delta","text":part}})); }
        }
        event(json!({"type":"content_block_stop","index":i}));
    }
    event(json!({"type":"message_delta","delta":{"stop_reason":stop,"stop_sequence":null},"usage":{"output_tokens":24}}));
    event(json!({"type":"message_stop"}));
    ([("content-type", "text/event-stream")], out).into_response()
}

#[derive(Clone, Default)]
struct Seen(Arc<Mutex<Vec<Value>>>);
impl Seen {
    fn push(&self, path: &str, headers: &HeaderMap, body: &Value) -> usize {
        let h = |n: &str| headers.get(n).and_then(|v| v.to_str().ok()).map(str::to_owned);
        let mut v = self.0.lock().unwrap();
        v.push(json!({"path":path,"x-api-key":h("x-api-key").map(|k| k.replace(LAB_KEY, "<lab key>")),"authorization":h("authorization").map(|a| a.replace(LAB_KEY, "<lab key>")),
            "session":h("x-opencode-session"),"model":body["model"],"max_tokens":body["max_tokens"].as_u64().or(body["max_completion_tokens"].as_u64()),
            "stream":body["stream"],"tools":body["tools"].as_array().map(|t| t.len()),"chars":body.to_string().len(),
            "summary":body.to_string().contains("<conversation>"),"tool_result":body.to_string().contains("TIMELINE_LAB_RESULT")}));
        v.len()
    }
    fn all(&self) -> Vec<Value> {
        self.0.lock().unwrap().clone()
    }
}

/// Claude Haiku 5.5 (Pi's catalog model, Anthropic Messages): the key goes to `x-api-key`, OpenCode's session
/// header is present, the request carries Heimdall's 16,000-token output limit, and a streamed tool call runs
/// Heimdall's MCP tool before the streamed final answer.
#[tokio::test]
#[ignore = "explicit pinned-runtime lab: HEIMDALL_OPENCODE_LAB"]
async fn haiku_streams_a_tool_call_through_the_production_harness() -> Result<()> {
    let base = lab_base()?;
    let seen = Seen::default();
    let s = seen.clone();
    let app = Router::new().route("/inference/go/v1/messages", post(move |headers: HeaderMap, body: Bytes| {
        let s = s.clone();
        async move {
            let body: Value = serde_json::from_slice(&body).unwrap();
            s.push("/inference/go/v1/messages", &headers, &body);
            if body.to_string().contains("TIMELINE_LAB_RESULT") {
                anthropic_sse(&[json!({"type":"text","text":"HAIKU_TOOL_OK"})], "end_turn", 2200)
            } else {
                anthropic_sse(&[json!({"type":"tool_use","id":"toolu_lab","name":"mcp__heimdall__timeline_search","input":{"query":"opencode-lab","limit":1}})], "tool_use", 2000)
            }
        }
    }));
    let (addr, task) = serve(app).await?;
    let lab = lab(&base, "haiku-tool", &ApiModel::haiku(), addr, &["mcp__heimdall__timeline_search"]).await?;
    let (error, text, frames) = turn(lab, "haiku-lab", "Call mcp__heimdall__timeline_search once with query opencode-lab and limit 1, then reply HAIKU_TOOL_OK.", Duration::from_secs(120)).await?;
    task.abort();
    let requests = seen.all();
    ensure!(!error && text.contains("HAIKU_TOOL_OK"), "turn failed: {text} {requests:?}");
    ensure!(requests.len() == 2, "expected tool call then answer: {requests:?}");
    for r in &requests {
        ensure!(r["x-api-key"] == "<lab key>", "key not sent as x-api-key: {r}");
        ensure!(r["session"].as_str().is_some_and(|s| !s.is_empty()), "x-opencode-session missing: {r}");
        // The model's own output limit (audit 27 Oct, F7), not the reserve.
        ensure!(r["model"] == provider::HAIKU && r["stream"] == true && r["max_tokens"] == ApiModel::haiku().limits().response, "request shape: {r}");
    }
    ensure!(requests[0]["session"] == requests[1]["session"], "session header must be stable within a conversation");
    ensure!(requests[1]["tool_result"] == true, "tool result not returned to the model");
    let frames = frames.lock().unwrap();
    ensure!(frames.iter().filter(|(_, v)| v["type"] == "tool_execution_start").count() == 1, "exactly one MCP tool call");
    ensure!(frames.iter().any(|(_, v)| v["type"] == "message_update" && v["assistantMessageEvent"]["type"] == "text_delta"), "answer not streamed");
    // The tool log (19 Oct): real Pi's events give one plain line per call, with the arguments and the result.
    let log = std::fs::read_to_string(agent::tool_log::path(&base.join("haiku-tool"))).context("tool log")?;
    ensure!(log.lines().count() == 1 && log.contains("  timeline_search  ok  ") && log.contains("\"query\":\"opencode-lab\"") && log.contains("TIMELINE_LAB_RESULT"), "tool log: {log}");
    receipt(&base, "haiku-tool", json!({"requests":requests,"tool_calls":1,"streamed":true,"tool_log":log}))
}

/// Claude Haiku 5.5 on a Console sign-in: the access token goes as both `x-api-key` and `Authorization: Bearer`,
/// with the account's `x-org-id`, to the Console-provided endpoint; first handed over in the environment (lab
/// builds), then fetched by Pi's per-request key command (the installed app's path).
#[tokio::test]
#[ignore = "explicit pinned-runtime lab: HEIMDALL_OPENCODE_LAB"]
async fn console_sign_in_reaches_haiku_with_bearer_and_org() -> Result<()> {
    let base = lab_base()?;
    let mut receipts = serde_json::Map::new();
    for (case, auth) in [("console-env", Auth::Console), ("console-command", Auth::ConsoleCommand)] {
        let seen = Seen::default();
        let s = seen.clone();
        let orgs = Arc::new(Mutex::new(vec![]));
        let o = orgs.clone();
        let app = Router::new().route("/zen/go/v1/messages", post(move |headers: HeaderMap, body: Bytes| {
            let (s, o) = (s.clone(), o.clone());
            async move {
                let body: Value = serde_json::from_slice(&body).unwrap();
                o.lock().unwrap().push(headers.get("x-org-id").and_then(|v| v.to_str().ok()).map(str::to_owned));
                let mut h = headers.clone();
                for name in ["x-api-key", "authorization"] {
                    if let Some(v) = headers.get(name).and_then(|v| v.to_str().ok()) {
                        h.insert(name, v.replace(CONSOLE_TOKEN, "<console token>").parse().unwrap());
                    }
                }
                s.push("/zen/go/v1/messages", &h, &body);
                anthropic_sse(&[json!({"type":"text","text":"CONSOLE_OK"})], "end_turn", 1500)
            }
        }));
        let (addr, task) = serve(app).await?;
        let lab = lab_auth(&base, case, &ApiModel::haiku(), addr, &[], auth).await?;
        let (error, text, _) = turn(lab, case, "Reply CONSOLE_OK.", Duration::from_secs(60)).await?;
        task.abort();
        let requests = seen.all();
        ensure!(!error && text.contains("CONSOLE_OK"), "{case}: {text} {requests:?}");
        ensure!(requests.iter().all(|r| r["x-api-key"] == "<console token>" && r["authorization"] == "Bearer <console token>"), "{case}: token headers {requests:?}");
        ensure!(orgs.lock().unwrap().iter().all(|o| o.as_deref() == Some("org-lab")), "{case}: x-org-id missing");
        receipts.insert(case.into(), json!({"requests": requests}));
    }
    receipt(&base, "console", Value::Object(receipts))
}

/// The chat-completions family (Kimi K3 from Pi's catalog): streamed tool call and answer, Bearer key, session header.
#[tokio::test]
#[ignore = "explicit pinned-runtime lab: HEIMDALL_OPENCODE_LAB"]
async fn chat_completions_model_streams_a_tool_call() -> Result<()> {
    let base = lab_base()?;
    let rt = runtime(&base).await?;
    let p = provider::provider(provider::OPENCODE_GO).unwrap();
    let catalog = provider::pi_catalog(&rt.root, p).context("catalog")?;
    let kimi = provider::merge_catalog(p, &[serde_json::json!({"id":"kimi-k3"})], Some(&catalog), None).remove(0);
    ensure!(kimi.api == provider::Api::OpenaiCompletions, "Kimi K3 is a chat-completions model in Pi 1.2.0");
    let seen = Seen::default();
    let s = seen.clone();
    let chunk = |delta: Value, finish: Option<&str>| format!("data: {}\n\n", json!({"id":"lab","object":"chat.completion.chunk","created":1,"model":"kimi-k3","choices":[{"index":0,"delta":delta,"finish_reason":finish}]}));
    let app = Router::new().route("/inference/go/v1/chat/completions", post(move |headers: HeaderMap, body: Bytes| {
        let s = s.clone();
        async move {
            let body: Value = serde_json::from_slice(&body).unwrap();
            s.push("/inference/go/v1/chat/completions", &headers, &body);
            let text = if body.to_string().contains("TIMELINE_LAB_RESULT") {
                chunk(json!({"role":"assistant","content":"KIMI_TOOL_OK"}), None) + &chunk(json!({}), Some("stop"))
            } else {
                chunk(json!({"role":"assistant","tool_calls":[{"index":0,"id":"call_lab","type":"function","function":{"name":"mcp__heimdall__timeline_search","arguments":"{\"query\":\"opencode-lab\",\"limit\":1}"}}]}), None)
                    + &chunk(json!({}), Some("tool_calls"))
            };
            ([("content-type", "text/event-stream")], text + "data: [DONE]\n\n").into_response()
        }
    }));
    let (addr, task) = serve(app).await?;
    let lab = lab(&base, "kimi-tool", &kimi, addr, &["mcp__heimdall__timeline_search"]).await?;
    let (error, text, _) = turn(lab, "kimi-lab", "Call mcp__heimdall__timeline_search once, then reply KIMI_TOOL_OK.", Duration::from_secs(120)).await?;
    task.abort();
    let requests = seen.all();
    ensure!(!error && text.contains("KIMI_TOOL_OK"), "turn failed: {text} {requests:?}");
    ensure!(requests.len() == 2 && requests[1]["tool_result"] == true, "tool loop: {requests:?}");
    ensure!(requests.iter().all(|r| r["authorization"] == "Bearer <lab key>" && r["session"].as_str().is_some_and(|s| !s.is_empty())), "auth/session: {requests:?}");
    ensure!(requests[0]["max_tokens"] == kimi.limits().response, "output limit: {requests:?}");
    receipt(&base, "kimi-tool", json!({"requests":requests,"limits":{"context":kimi.limits().context,"response":kimi.limits().response}}))
}

/// Switching a long local conversation (121,298 tokens of usage on the local model) to Haiku compacts before the
/// first answer: Pi applies Haiku's 96,000-token threshold, sends the summary request, then the answer request,
/// and keeps a durable checkpoint. The first turn does not fail.
#[tokio::test]
#[ignore = "explicit pinned-runtime lab: HEIMDALL_OPENCODE_LAB"]
async fn switching_a_long_history_to_haiku_compacts_first() -> Result<()> {
    let base = lab_base()?;
    let seen = Seen::default();
    let s = seen.clone();
    let app = Router::new().route("/inference/go/v1/messages", post(move |headers: HeaderMap, body: Bytes| {
        let s = s.clone();
        async move {
            let body: Value = serde_json::from_slice(&body).unwrap();
            let summary = body.to_string().contains("<conversation>");
            s.push("/inference/go/v1/messages", &headers, &body);
            let text = if summary { "# Summary\nSynthetic observations reviewed. Reply COMPACTION_OK next." } else { "COMPACTION_OK" };
            anthropic_sse(&[json!({"type":"text","text":text})], "end_turn", if summary { 98_000 } else { 21_000 })
        }
    }).layer(axum::extract::DefaultBodyLimit::max(4 * 1024 * 1024)));
    let (addr, task) = serve(app).await?;
    let lab = lab(&base, "compaction", &ApiModel::haiku(), addr, &[]).await?;
    let mut lines = vec![json!({"type":"session","version":3,"id":"opencode-compaction-lab","timestamp":"2026-10-18T00:00:00.000Z","cwd":std::env::temp_dir()})];
    let mut parent: Option<String> = None;
    for i in 0..100 {
        let id = format!("u{i}");
        lines.push(json!({"type":"message","id":id,"parentId":parent,"timestamp":"2026-10-18T00:00:00.000Z","message":{"role":"user","content":[{"type":"text","text":"Synthetic microphone observation: discussed the build and tool results. ".repeat(74)}],"timestamp":1}}));
        parent = Some(id);
        let id = format!("a{i}");
        lines.push(json!({"type":"message","id":id,"parentId":parent,"timestamp":"2026-10-18T00:00:01.000Z","message":{"role":"assistant","content":[{"type":"text","text":"Observation retained."}],"api":"openai-completions","provider":"splash","model":"qwen3.8-27b","stopReason":"stop","usage":{"input":121280,"output":18,"cacheRead":0,"cacheWrite":0,"totalTokens":121298,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}},"timestamp":2}}));
        parent = Some(id);
    }
    std::fs::write(&lab.config.session, lines.iter().map(Value::to_string).collect::<Vec<_>>().join("\n") + "\n")?;
    let session = lab.config.session.clone();
    let (error, text, frames) = turn(lab, "compact-first", "Reply COMPACTION_OK.", Duration::from_secs(120)).await?;
    task.abort();
    let requests = seen.all();
    ensure!(!error && text.contains("COMPACTION_OK"), "first turn after the switch failed: {text}");
    ensure!(requests.len() >= 2 && requests[0]["summary"] == true && requests.last().unwrap()["summary"] == false, "summary must precede the answer: {requests:?}");
    // The summary may carry the whole old history (more than Haiku's 128K budget); it must still get a real output
    // budget, not one clamped to the budget's window.
    ensure!(requests[0]["max_tokens"].as_u64().is_some_and(|m| m >= 16_000), "summary output clamped: {requests:?}");
    let answer_chars = requests.last().unwrap()["chars"].as_u64().unwrap();
    // Pi's own estimate is about 4 characters per token: the answer request is far below 100,000 tokens.
    ensure!(answer_chars < 4 * 96_000, "answer request still carries the long history: {answer_chars} chars");
    ensure!(std::fs::read_to_string(&session)?.lines().filter_map(|l| serde_json::from_str::<Value>(l).ok()).any(|v| v["type"] == "compaction"), "durable checkpoint absent");
    ensure!(frames.lock().unwrap().iter().any(|(_, v)| v["type"] == "compaction_start"), "compaction row absent");
    let limits = ApiModel::haiku().limits();
    receipt(&base, "compaction", json!({"old_history_usage_tokens":121298,"haiku_context":limits.context,"threshold":limits.threshold(),"keep_recent":limits.recent,"requests":requests}))
}

/// A rejected key fails the turn once, in words; a throttle with a reset waits and resends the same request; a
/// throttle without one is reported and only Pi's bounded retry (two attempts) follows, never a hot loop.
#[tokio::test]
#[ignore = "explicit pinned-runtime lab: HEIMDALL_OPENCODE_LAB"]
async fn rejected_keys_and_throttles_are_named_and_bounded() -> Result<()> {
    let base = lab_base()?;
    let mut receipts = serde_json::Map::new();
    for (case, statuses) in [("rejected", vec![401u16]), ("reset", vec![429, 429, 200]), ("no-reset", vec![429, 429, 429, 429, 429])] {
        let seen = Seen::default();
        let s = seen.clone();
        let plan = Arc::new(statuses.clone());
        let app = Router::new().route("/inference/go/v1/messages", post(move |headers: HeaderMap, body: Bytes| {
            let (s, plan) = (s.clone(), plan.clone());
            async move {
                let body: Value = serde_json::from_slice(&body).unwrap();
                let n = s.push("/inference/go/v1/messages", &headers, &body);
                match plan.get(n - 1).copied().unwrap_or(429) {
                    401 => (StatusCode::UNAUTHORIZED, [("content-type", "application/json")], r#"{"type":"error","error":{"type":"api_error","message":"Upstream request failed: Invalid credential"}}"#).into_response(),
                    429 if case == "reset" => (StatusCode::TOO_MANY_REQUESTS, [("retry-after", "1.5")], "slow down").into_response(),
                    429 => (StatusCode::TOO_MANY_REQUESTS, [("content-type", "application/json")], r#"{"type":"error","error":{"type":"rate_limit_error","message":"usage limit reached"}}"#).into_response(),
                    _ => anthropic_sse(&[json!({"type":"text","text":"RESET_OK"})], "end_turn", 2000),
                }
            }
        }));
        let (addr, task) = serve(app).await?;
        let mut lab = lab(&base, &format!("errors-{case}"), &ApiModel::haiku(), addr, &[]).await?;
        // The reset case proves the watchdog is suspended through stated waits; Pi's own backoff (2 s, 4 s) in
        // the no-reset case runs under the ordinary watchdog.
        lab.config.inactivity = Duration::from_secs(if case == "reset" { 3 } else { 60 });
        let started = Instant::now();
        let (error, text, frames) = turn(lab, case, "Reply RESET_OK.", Duration::from_secs(90)).await?;
        task.abort();
        let requests = seen.all();
        let frames = frames.lock().unwrap();
        let problems: Vec<String> = frames.iter().filter(|(_, v)| v["type"] == "heimdall_provider_problem").map(|(_, v)| v["message"].as_str().unwrap_or_default().to_owned()).collect();
        let failure = frames.iter().rev().find(|(_, v)| v["type"] == "message_end" && v["message"]["stopReason"] == "error").map(|(_, v)| v["message"]["errorMessage"].as_str().unwrap_or_default().to_owned());
        match case {
            "rejected" => {
                ensure!(error && requests.len() == 1, "a rejected key is not retried: {requests:?}");
                ensure!(problems == ["OpenCode Go: access rejected / no subscription"], "{problems:?}");
                let shown = agent::provider::explain_error("opencode-go", failure.as_deref().unwrap_or_default());
                ensure!(shown.as_deref() == Some("OpenCode Go: access rejected / no subscription"), "agent window text: {failure:?}");
            }
            "reset" => {
                ensure!(!error && text.contains("RESET_OK") && requests.len() == 3, "reset wait: {text} {requests:?}");
                ensure!(started.elapsed() >= Duration::from_millis(3000), "retry-after not honoured");
                ensure!(frames.iter().filter(|(_, v)| v["type"] == "heimdall_provider_wait" && v["delay_ms"] == 1500).count() == 2, "two waits shown");
            }
            _ => {
                ensure!(error, "an exhausted allowance fails the turn");
                ensure!((1..=4).contains(&requests.len()), "bounded attempts only: {}", requests.len());
                ensure!(problems.iter().all(|p| p == "OpenCode Go: usage limit reached"), "{problems:?}");
            }
        }
        receipts.insert(case.into(), json!({"requests":requests.len(),"error":error,"problems":problems,"failure":failure,"elapsed_s":started.elapsed().as_secs_f64()}));
    }
    receipt(&base, "errors", Value::Object(receipts))
}

fn receipt(base: &Path, case: &str, mut v: Value) -> Result<()> {
    v["controlled_endpoint"] = json!(true);
    v["cloud_requests"] = json!(0);
    v["date_unix_ms"] = json!(agent::now_ms());
    let text = serde_json::to_vec_pretty(&v)?;
    ensure!(!String::from_utf8_lossy(&text).contains(LAB_KEY), "receipt must not contain the key");
    agent::runtime::atomic_write(&base.join(format!("{case}-receipt.json")), &text)?;
    println!("{case}: {v}");
    Ok(())
}

/// Heimdall's production request shape ("Heimdall should have his own system prompt … Vault is
/// something separate"): Pi runs in the Vault as in production, yet the request carries Heimdall's prompt and no
/// Vault AGENTS.md/CLAUDE.md. Writes each request body (system, tools, thinking) per thinking level to the lab dir
/// so token counts can be taken from the exact bytes Pi sends.
#[tokio::test]
#[ignore = "explicit pinned-runtime lab: HEIMDALL_OPENCODE_LAB"]
async fn production_prompt_reaches_haiku_without_vault_context_files() -> Result<()> {
    let base = lab_base()?;
    let bodies: Arc<Mutex<Vec<Value>>> = Arc::default();
    let b = bodies.clone();
    let app = Router::new().route("/inference/go/v1/messages", post(move |body: Bytes| {
        let b = b.clone();
        async move {
            b.lock().unwrap().push(serde_json::from_slice::<Value>(&body).unwrap());
            anthropic_sse(&[json!({"type":"text","text":"No request."})], "end_turn", 2000)
        }
    }));
    let (addr, task) = serve(app).await?;
    let mut lab = lab(&base, "production-prompt", &ApiModel::haiku(), addr, &[]).await?;
    // Production cwd, prompt and tool set: only the fetch destination is redirected.
    lab.config.working_dir = heimdall_home::home_dir();
    lab.config.prompt = Some(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("prompt/heimdall.md"));
    let runtime = runtime(&base).await?;
    let ext = base.join("production-prompt/lab-extension.ts");
    std::fs::write(&ext, format!(
        r#"import heimdall from {production};
const fetch = globalThis.fetch;
globalThis.fetch = (input, init) => {{
  const url = typeof input === 'string' || input instanceof URL ? String(input) : input.url;
  return fetch(url.startsWith('https://opencode.ai/') ? url.replace('https://opencode.ai', 'http://{addr}') : input, init);
}};
export default function(pi) {{ heimdall(pi); }}
"#,
        production = serde_json::to_string(&runtime.extension.to_string_lossy())?,
    ))?;
    lab.config.extension = Some(ext);
    let mut pi = Pi::launch_config_permitted(lab.config, Some(lab.gate.permit()?))?;
    pi.state().await?;
    for (i, level) in ["medium", "low"].iter().enumerate() {
        pi.query(json!({"type":"set_thinking_level","level":level})).await?;
        let id = format!("prompt-lab-{level}");
        pi.send(agent::protocol::message(&id, "Transcript:\n[Discord · Dave] did you push the audio fix?", &[], true, false)?).await?;
        tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                match pi.next().await? {
                    Some(Event::Complete { .. }) => return Ok::<_, anyhow::Error>(()),
                    None => anyhow::bail!("Pi exited"),
                    _ => {}
                }
            }
        }).await.context("lab turn deadline")??;
        ensure!(bodies.lock().unwrap().len() == i + 1, "one request per turn");
    }
    pi.shutdown().await?;
    lab.server.shutdown().await;
    task.abort();
    let bodies = bodies.lock().unwrap().clone();
    let mut summary = vec![];
    for (level, body) in ["medium", "low"].iter().zip(&bodies) {
        std::fs::write(base.join(format!("production-prompt-{level}-request.json")), serde_json::to_vec_pretty(body)?)?;
        let system = body["system"].to_string();
        let flat = body["system"].as_array().into_iter().flatten().filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n").split_whitespace().collect::<Vec<_>>().join(" ");
        ensure!(flat.contains("# Heimdall") && flat.contains("Never edit `AGENTS.md`"), "Heimdall prompt missing: {level}");
        let log = agent::tool_log::path(&base.join("production-prompt")).display().to_string();
        ensure!(flat.contains(&format!("`{log}`")) && !flat.contains(agent::tool_log::PROMPT_PLACEHOLDER), "the tool log's path is not in the prompt: {level}");
        ensure!(!system.contains("<project_instructions") && !system.contains("Vault Agent Contract"), "Vault context file reached Heimdall: {level}");
        summary.push(json!({"level":level,"system_chars":system.len(),"tools":body["tools"].as_array().map(|t| t.len()),
            "body_chars":body.to_string().len(),"thinking":body["thinking"],"output_config":body["output_config"],"max_tokens":body["max_tokens"]}));
    }
    receipt(&base, "production-prompt", json!({"requests":summary}))
}

/// The provider's rule since 18 Oct: a thinking block is bound to the history before it. After a compaction
/// (which rewrites that history) every kept Haiku thinking block is rejected with a 400, on every later turn.
const SIGNATURE_REJECTED: &str = "Upstream request failed: [invalid_request_error] messages.3.content.0: Invalid `signature` in `thinking` block. The block is bound to a different conversation. Remove the block, or set `thinking.block_binding.prefix_mismatch_behavior` to \"drop_block\".";

fn signed_thinking(body: &Value) -> bool {
    body["messages"].as_array().into_iter().flatten().filter(|m| m["role"] == "assistant")
        .flat_map(|m| m["content"].as_array().cloned().unwrap_or_default())
        .any(|b| b["type"] == "thinking" && b["signature"].as_str().is_some_and(|s| !s.is_empty()))
}

/// A Haiku conversation with signed thinking, then a compaction whose kept tail holds one of those turns.
fn compacted_haiku_session(path: &Path) -> Result<()> {
    let ts = "2026-10-18T11:49:00.000Z";
    let assistant = |text: &str| json!({"role":"assistant","content":[{"type":"thinking","thinking":"Plan the reply.","thinkingSignature":"sig-bound-to-the-original-prefix"},{"type":"text","text":text}],
        "api":"anthropic-messages","provider":"opencode-go","model":provider::HAIKU,"stopReason":"stop",
        "usage":{"input":40000,"output":20,"cacheRead":0,"cacheWrite":0,"totalTokens":40020,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}},"timestamp":2});
    let user = |text: &str| json!({"role":"user","content":[{"type":"text","text":text}],"timestamp":1});
    let lines = [
        json!({"type":"session","version":3,"id":"thinking-binding-lab","timestamp":ts,"cwd":std::env::temp_dir()}),
        json!({"type":"message","id":"u1","parentId":null,"timestamp":ts,"message":user("First request.")}),
        json!({"type":"message","id":"a1","parentId":"u1","timestamp":ts,"message":assistant("First answer.")}),
        json!({"type":"message","id":"u2","parentId":"a1","timestamp":ts,"message":user("Second request.")}),
        json!({"type":"message","id":"a2","parentId":"u2","timestamp":ts,"message":assistant("Second answer.")}),
        json!({"type":"compaction","id":"c1","parentId":"a2","timestamp":ts,"summary":"## Goal\n- User is testing the agent.","firstKeptEntryId":"u2","tokensBefore":75944}),
    ];
    std::fs::write(path, lines.iter().map(Value::to_string).collect::<Vec<_>>().join("\n") + "\n")?;
    Ok(())
}

/// 18 Oct 13:50 and 13:51: after a compaction, every turn failed with the thinking-signature 400 and each failed
/// turn went to review. The production extension asks the provider to drop invalidated blocks; when a provider
/// does not honour that, the rejected request is resent once without thinking blocks. Neither case fails the turn.
#[tokio::test]
#[ignore = "explicit pinned-runtime lab: HEIMDALL_OPENCODE_LAB"]
async fn compacted_thinking_history_is_dropped_not_rejected() -> Result<()> {
    let base = lab_base()?;
    let mut receipts = serde_json::Map::new();
    // honours: the provider accepts `drop_block` with the beta header; strict: it rejects any signed block.
    for (case, honours) in [("thinking-binding", true), ("thinking-strict", false)] {
        let seen: Arc<Mutex<Vec<Value>>> = Arc::default();
        let s = seen.clone();
        let app = Router::new().route("/inference/go/v1/messages", post(move |headers: HeaderMap, body: Bytes| {
            let s = s.clone();
            async move {
                let body: Value = serde_json::from_slice(&body).unwrap();
                let beta = headers.get("anthropic-beta").and_then(|v| v.to_str().ok()).unwrap_or("").to_owned();
                let dropping = body["thinking"]["block_binding"]["prefix_mismatch_behavior"] == "drop_block" && beta.split(',').any(|b| b == "thinking-binding-controls-2026-08-01");
                let rejected = signed_thinking(&body) && !(honours && dropping);
                s.lock().unwrap().push(json!({"thinking":body["thinking"],"beta":beta,"signed_thinking":signed_thinking(&body),"rejected":rejected}));
                if rejected {
                    return (StatusCode::BAD_REQUEST, axum::Json(json!({"type":"error","error":{"type":"invalid_request_error","message":SIGNATURE_REJECTED}}))).into_response();
                }
                anthropic_sse(&[json!({"type":"text","text":"BINDING_OK"})], "end_turn", 1500)
            }
        }));
        let (addr, task) = serve(app).await?;
        let lab = lab(&base, case, &ApiModel::haiku(), addr, &[]).await?;
        compacted_haiku_session(&lab.config.session)?;
        let (error, text, _) = turn(lab, case, "Reply BINDING_OK.", Duration::from_secs(60)).await?;
        task.abort();
        let requests = seen.lock().unwrap().clone();
        ensure!(!error && text.contains("BINDING_OK"), "{case}: turn failed: {text} {requests:?}");
        ensure!(requests[0]["signed_thinking"] == true, "{case}: the kept thinking block must reach the provider first: {requests:?}");
        ensure!(requests.iter().all(|r| r["thinking"]["block_binding"]["prefix_mismatch_behavior"] == "drop_block"), "{case}: binding control missing: {requests:?}");
        if honours {
            ensure!(requests.len() == 1, "{case}: one request when the provider drops blocks: {requests:?}");
        } else {
            ensure!(requests.len() == 2 && requests[1]["signed_thinking"] == false, "{case}: one resend without thinking: {requests:?}");
        }
        receipts.insert(case.into(), json!({"requests":requests}));
    }
    receipt(&base, "thinking-binding", Value::Object(receipts))
}

/// The same failure against the real OpenCode Go endpoint: two Haiku turns with real signed thinking, then a
/// compaction checkpoint that summarizes the first turn and keeps the second (the 13:50 shape), then a third turn
/// from a fresh Pi. Costs well under $0.01. Needs HEIMDALL_OPENCODE_REAL_TOKEN (a Console access token; never
/// printed) and HEIMDALL_OPENCODE_REAL_ORG.
#[tokio::test]
#[ignore = "explicit real-endpoint lab: HEIMDALL_OPENCODE_LAB + HEIMDALL_OPENCODE_REAL_TOKEN"]
async fn real_endpoint_answers_after_compacting_thinking_turns() -> Result<()> {
    let base = lab_base()?;
    let token = std::env::var("HEIMDALL_OPENCODE_REAL_TOKEN").context("HEIMDALL_OPENCODE_REAL_TOKEN")?;
    let org = std::env::var("HEIMDALL_OPENCODE_REAL_ORG").context("HEIMDALL_OPENCODE_REAL_ORG")?;
    let p = provider::provider(provider::OPENCODE_GO).unwrap();
    unsafe { std::env::remove_var(p.key_env) };
    provider::save_console_session(p, &agent::console::Session {
        access: token, refresh: "lab-refresh".into(), expires_ms: agent::now_ms() + 3_600_000,
        org_id: Some(org), server: "https://opencode.ai/console".into(),
        base_url: Some("https://opencode.ai/zen/go/v1".into()), headers: Default::default(), models: vec![provider::HAIKU.into()], client_id: None,
    })?;
    let session = provider::console_session(p).context("lab console session")?;
    let model = ApiModel::haiku();
    let runtime = runtime(&base).await?;
    let data = base.join("real-thinking-binding");
    let _ = std::fs::remove_dir_all(&data);
    std::fs::create_dir_all(&data)?;
    let catalog = provider::pi_catalog(&runtime.root, p);
    let pi_auth = agent::runtime::PiAuth::Console { api_key: format!("${}", agent::console::TOKEN_ENV), base_url: session.base_url.clone(), headers: session.request_headers(), models: session.models.clone() };
    let dir = agent::runtime::configure_api(&data, &model, catalog.as_ref(), &pi_auth)?;
    let gate = Arc::new(agent::delivery::DeliveryGate::new(true));
    let server = agent::mcp::McpServer::start(Arc::new(Host), Arc::new(Search), gate.clone()).await?;
    let mut config = PiConfig::owned(&runtime, &data, dir, server.url.clone(), server.token.clone());
    config.api_model = Some(model.clone());
    config.engine = None;
    config.working_dir = std::env::temp_dir();
    let prompt = data.join("lab-prompt.md");
    std::fs::write(&prompt, "You are a test assistant. Think privately before every answer. Never use tools.")?;
    config.prompt = Some(prompt);
    let ext = data.join("lab-extension.ts");
    std::fs::write(&ext, format!("import heimdall from {};\nexport default function(pi) {{ heimdall(pi); pi.on('session_start', () => pi.setActiveTools([])); }}\n", serde_json::to_string(&runtime.extension.to_string_lossy())?))?;
    config.extension = Some(ext);
    let session_path = config.session.clone();
    async fn complete(pi: &mut Pi) -> Result<(bool, String)> {
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
    let mut answers = vec![];
    let mut pi = Pi::launch_config_permitted(config.clone(), Some(gate.permit()?))?;
    pi.state().await?;
    for (i, text) in ["What is 37 times 43? Work it out step by step privately, then give only the number.", "Now work out 41 times 59 step by step privately, then give only the number."].iter().enumerate() {
        pi.send(agent::protocol::message(&format!("real-{i}"), text, &[], true, false)?).await?;
        answers.push(complete(&mut pi).await?);
    }
    pi.shutdown().await?;
    ensure!(answers.iter().all(|(error, _)| !error), "a turn before the compaction failed: {answers:?}");
    // Pi's checkpoint: the first turn becomes a summary, the second turn (with its signed thinking) is kept.
    let entries: Vec<Value> = std::fs::read_to_string(&session_path)?.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
    // Adaptive thinking may skip a turn: keep from the newest request whose answer carries a signed block.
    let signed = |v: &Value| v["message"]["role"] == "assistant"
        && v["message"]["content"].as_array().into_iter().flatten().any(|b| b["type"] == "thinking" && b["thinkingSignature"].as_str().is_some_and(|s| !s.is_empty()));
    let thought = entries.iter().rposition(signed);
    let kept_thinking = thought.is_some();
    let kept = thought.and_then(|t| entries[..t].iter().rev().find(|v| v["message"]["role"] == "user"))
        .and_then(|v| v["id"].as_str()).context("no request with a signed thinking answer")?.to_owned();
    let last = entries.last().and_then(|v| v["id"].as_str()).context("last entry")?;
    let mut file = std::fs::OpenOptions::new().append(true).open(&session_path)?;
    use std::io::Write;
    writeln!(file, "{}", json!({"type":"compaction","id":"labcompact","parentId":last,"timestamp":"2026-10-18T11:50:06.850Z",
        "summary":"## Goal\n- User asked for two products; the first was 37 times 43 = 1591.","firstKeptEntryId":kept,"tokensBefore":75944}))?;
    drop(file);
    let mut pi = Pi::launch_config_permitted(config, Some(gate.permit()?))?;
    pi.state().await?;
    pi.send(agent::protocol::message("real-2", "And 23 times 29? Only the number.", &[], true, false)?).await?;
    answers.push(complete(&mut pi).await?);
    pi.shutdown().await?;
    server.shutdown().await;
    ensure!(kept_thinking, "the kept turn has no signed thinking block; the lab did not reproduce the failure shape: {answers:?}");
    ensure!(answers.iter().all(|(error, _)| !error), "a turn failed: {answers:?}");
    ensure!(answers[2].1.contains("667"), "third answer wrong: {answers:?}");
    let v = json!({"answers":answers,"kept_signed_thinking":kept_thinking,"controlled_endpoint":false,"cloud_requests":3,"date_unix_ms":agent::now_ms()});
    agent::runtime::atomic_write(&base.join("real-thinking-binding-receipt.json"), &serde_json::to_vec_pretty(&v)?)?;
    Ok(())
}

/// 18 Oct 13:38: the agent edited the Vault AGENTS.md. The production extension refuses `edit`/`write` on
/// AGENTS.md or CLAUDE.md before the tool runs; the file is untouched and the model is told why.
#[tokio::test]
#[ignore = "explicit pinned-runtime lab: HEIMDALL_OPENCODE_LAB"]
async fn instruction_files_are_never_edited_by_the_agent() -> Result<()> {
    let base = lab_base()?;
    let data = base.join("instruction-guard");
    let target = data.join("AGENTS.md");
    let results: Arc<Mutex<Vec<Value>>> = Arc::default();
    let r = results.clone();
    let path = target.to_string_lossy().to_string();
    let app = Router::new().route("/inference/go/v1/messages", post(move |body: Bytes| {
        let (r, path) = (r.clone(), path.clone());
        async move {
            let body: Value = serde_json::from_slice(&body).unwrap();
            let result = body["messages"].as_array().into_iter().flatten().flat_map(|m| m["content"].as_array().cloned().unwrap_or_default()).find(|b| b["type"] == "tool_result");
            if let Some(result) = result {
                r.lock().unwrap().push(result);
                anthropic_sse(&[json!({"type":"text","text":"GUARD_OK"})], "end_turn", 2200)
            } else {
                anthropic_sse(&[json!({"type":"tool_use","id":"toolu_guard","name":"edit","input":{"path":path,"edits":[{"oldText":"original","newText":"changed by the agent"}]}})], "tool_use", 2000)
            }
        }
    }));
    let (addr, task) = serve(app).await?;
    let lab = lab(&base, "instruction-guard", &ApiModel::haiku(), addr, &["edit"]).await?;
    std::fs::write(&target, "original\n")?;
    let (error, text, _) = turn(lab, "guard", "Record my correction.", Duration::from_secs(60)).await?;
    task.abort();
    let results = results.lock().unwrap().clone();
    ensure!(!error && text.contains("GUARD_OK"), "turn failed: {text}");
    ensure!(std::fs::read_to_string(&target)? == "original\n", "AGENTS.md was edited");
    ensure!(results.len() == 1 && results[0]["is_error"] == true && results[0].to_string().contains("instruction file"), "refusal not returned: {results:?}");
    receipt(&base, "instruction-guard", json!({"tool_result":results[0]}))
}

/// The model refuses an image whose label does not match its bytes, as Anthropic does (20 Oct 19:20:55), naming the
/// block by its `messages.N.content.M…` path; and, for the `poison` text, any tool result that carries it.
fn refused_block(body: &Value) -> Option<String> {
    let sniff = |data: &str| {
        use base64::Engine;
        let head = &data[..data.len().min(64) / 4 * 4];
        let b = base64::engine::general_purpose::STANDARD.decode(head).ok()?;
        agent::history::sniff(&b)
    };
    for (n, message) in body["messages"].as_array()?.iter().enumerate() {
        for (m, block) in message["content"].as_array().into_iter().flatten().enumerate() {
            // A text-only tool result is one string on the wire.
            let inner = match (block["type"].as_str(), &block["content"]) {
                (Some("tool_result"), Value::String(t)) => vec![json!({"type":"text","text":t})],
                (Some("tool_result"), c) => c.as_array().cloned().unwrap_or_default(),
                _ => vec![block.clone()],
            };
            for (k, b) in inner.iter().enumerate() {
                let path = if block["type"] == "tool_result" { format!("messages.{n}.content.{m}.tool_result.content.{k}") } else { format!("messages.{n}.content.{m}") };
                if b["type"] == "image" {
                    let declared = b["source"]["media_type"].as_str().unwrap_or_default();
                    let real = sniff(b["source"]["data"].as_str().unwrap_or_default()).unwrap_or("unknown");
                    if real != declared {
                        return Some(format!("{path}.image.source.base64: The image was specified using the {declared} media type, but the image appears to be a {real} image"));
                    }
                }
                if block["type"] == "tool_result" && b["text"].as_str().is_some_and(|t| t.contains("POISON_LAB")) {
                    return Some(format!("{path}.text: lab refusal of this block"));
                }
            }
        }
    }
    None
}

/// The lab extension plus a tool `lab_shot` that returns what a buggy tool would: `mode` "image" a JPEG labelled
/// PNG (the 20 Oct `look`), "poison" a text the fake provider refuses.
fn lab_shot_extension(lab: &Lab, mode: &str) -> Result<()> {
    use base64::Engine;
    let ext = lab.config.extension.clone().context("lab extension")?;
    let jpeg = base64::engine::general_purpose::STANDARD.encode([0xffu8, 0xd8, 0xff, 0xe0, 0x00, 0x10, b'J', b'F', b'I', b'F', 0, 1, 1, 0, 0, 1, 0, 1, 0, 0, 0xff, 0xd9]);
    let content = if mode == "image" {
        json!([{"type":"image","data":jpeg,"mimeType":"image/png"},{"type":"text","text":"Display 1: size 1728 1117"}])
    } else {
        json!([{"type":"text","text":"POISON_LAB result"}])
    };
    let text = std::fs::read_to_string(&ext)?.replace(
        "export default function(pi) { heimdall(pi);",
        &format!("export default function(pi) {{ heimdall(pi); pi.registerTool({{ name: 'lab_shot', label: 'lab_shot', description: 'Lab screenshot', parameters: {{ type: 'object', properties: {{}} }}, async execute() {{ return {{ content: {content}, details: undefined }}; }} }});"),
    );
    ensure!(text.contains("lab_shot"), "lab extension shape changed");
    std::fs::write(ext, text)?;
    Ok(())
}

/// What one lab run reports: the turn's error and final text, the history-repair lines, the session afterwards.
async fn turn_events(lab: Lab, id: &str, text: &str) -> Result<(bool, String, Vec<Value>, String)> {
    let session = lab.config.session.clone();
    let mut pi = Pi::launch_config_permitted(lab.config, Some(lab.gate.permit()?))?;
    let repairs: Arc<Mutex<Vec<Value>>> = Arc::default();
    pi.state().await?;
    pi.send(agent::protocol::message(id, text, &[], true, false)?).await?;
    let result = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            match pi.next().await? {
                Some(Event::Complete { error, text, .. }) => return Ok::<_, anyhow::Error>((error, text)),
                Some(Event::Other(v)) if v["type"] == "heimdall_history_repair" => repairs.lock().unwrap().push(v),
                None => anyhow::bail!("Pi exited"),
                _ => {}
            }
        }
    }).await;
    let stopped = pi.shutdown().await;
    lab.server.shutdown().await;
    let (error, text) = result.context("lab turn deadline")??;
    stopped?;
    let lines = repairs.lock().unwrap().clone();
    Ok((error, text, lines, std::fs::read_to_string(session)?))
}

/// 20 Oct 19:20:55 onwards: a tool result's JPEG labelled PNG made the provider refuse every request, through a
/// compaction, two reloads and Agent off/on. Now, through the real pinned Pi and the production extension:
/// - `image`: the refused request is repaired in the same run (the image relabelled by Pi's own append-only edit,
///   the failed attempt omitted) and the turn answers;
/// - `poison`: a refusal with nothing to repair restarts the history from its summary, and the turn answers;
/// - `refuse_all`: a provider that refuses every history settles the turn with its error after the summary, with
///   one `gave_up` line, never a loop;
/// - `start`: the same broken history written before a start is repaired by Heimdall before Pi reads it, so the
///   first request already carries the right label.
#[tokio::test]
#[ignore = "explicit pinned-runtime lab: HEIMDALL_OPENCODE_LAB"]
async fn a_refused_history_is_repaired_and_the_turn_answers() -> Result<()> {
    let base = lab_base()?;
    let mut receipts = serde_json::Map::new();
    for case in ["image", "poison", "refuse_all", "start"] {
        let seen: Arc<Mutex<Vec<Value>>> = Arc::default();
        let s = seen.clone();
        let app = Router::new().route("/inference/go/v1/messages", post(move |body: Bytes| {
            let s = s.clone();
            async move {
                let body: Value = serde_json::from_slice(&body).unwrap();
                let tools_answered = body.to_string().contains("tool_result");
                let refused = if case == "refuse_all" { Some("messages.1.content.0.text: lab refuses every history".to_owned()) } else { refused_block(&body) };
                // The last message the model reads: after a rebuild it must be the user's request (night review 21 Oct).
                let last = body["messages"].as_array().and_then(|m| m.last()).cloned().unwrap_or_default();
                let last_text = match &last["content"] { Value::String(t) => t.clone(), c => c.as_array().into_iter().flatten().filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n") };
                let n = { let mut v = s.lock().unwrap(); v.push(json!({"refused":refused,"summary":body.to_string().contains("Since that summary"),"tool_result":tools_answered,
                    "images":body.to_string().matches("\"media_type\"").count(),"jpeg_label":body.to_string().contains("image/jpeg"),"last_role":last["role"],"last_text":last_text})); v.len() };
                if let Some(message) = refused {
                    return (StatusCode::BAD_REQUEST, axum::Json(json!({"type":"error","error":{"type":"invalid_request_error","message":message}}))).into_response();
                }
                if n == 1 && case != "start" && !tools_answered {
                    return anthropic_sse(&[json!({"type":"tool_use","id":"toolu_lab_shot","name":"lab_shot","input":{}})], "tool_use", 1500);
                }
                anthropic_sse(&[json!({"type":"text","text":"REPAIRED_OK"})], "end_turn", 1500)
            }
        }));
        let (addr, task) = serve(app).await?;
        let lab = lab(&base, &format!("history-{case}"), &ApiModel::haiku(), addr, &["lab_shot"]).await?;
        lab_shot_extension(&lab, if case == "poison" || case == "refuse_all" { "poison" } else { "image" })?;
        if case == "start" { broken_history(&lab.config.session)?; }
        let (error, text, lines, session) = turn_events(lab, &format!("history-{case}"), "Take a lab_shot, then reply REPAIRED_OK.").await?;
        task.abort();
        let requests = seen.lock().unwrap().clone();
        let states: Vec<&str> = lines.iter().filter_map(|l| l["state"].as_str()).collect();
        match case {
            "image" => {
                ensure!(!error && text.contains("REPAIRED_OK"), "image: {text} {requests:?}");
                ensure!(requests.len() == 3 && requests[1]["refused"].is_string() && requests[2]["refused"].is_null() && requests[2]["jpeg_label"] == true, "image: {requests:?}");
                ensure!(states == ["repaired"], "image: {lines:?}");
                ensure!(session.contains("\"context_edit\"") && session.contains("\"completed\":true"), "image: the edit and a completed receipt are in the session");
            }
            "poison" => {
                ensure!(!error && text.contains("REPAIRED_OK"), "poison: {text} {requests:?}");
                ensure!(requests.len() == 3 && requests[2]["summary"] == true && requests[2]["tool_result"] == false, "poison: {requests:?}");
                ensure!(states == ["restarted_from_summary"], "poison: {lines:?}");
                // After the rebuild the model's last message is the request it answers, not the summary (MUST 2).
                let last = requests[2]["last_text"].as_str().unwrap_or_default();
                ensure!(requests[2]["last_role"] == "user" && last.contains("The request you are answering now.") && last.contains("Take a lab_shot, then reply REPAIRED_OK.") && !last.contains("[Heimdall delivery:"),
                    "poison: the last message is not the request: {}", requests[2]);
                ensure!(session.contains("\"customType\":\"heimdall_request\""), "poison: the request is in the session");
            }
            "refuse_all" => {
                ensure!(error, "refuse_all: the turn fails once the summary is refused too: {text}");
                ensure!(requests.len() == 2, "refuse_all: no loop: {requests:?}");
                ensure!(states == ["restarted_from_summary", "gave_up"], "refuse_all: {lines:?}");
            }
            _ => {
                ensure!(!error && text.contains("REPAIRED_OK"), "start: {text} {requests:?}");
                ensure!(requests.len() == 1 && requests[0]["jpeg_label"] == true, "start: one request, already repaired: {requests:?}");
                ensure!(states == ["repaired_at_start"], "start: {lines:?}");
            }
        }
        receipts.insert(case.into(), json!({"requests":requests,"lines":lines}));
    }
    receipt(&base, "history-repair", Value::Object(receipts))
}

/// The 20 Oct shape before a start: a compaction whose kept tail holds the tool result with a JPEG labelled PNG.
fn broken_history(path: &Path) -> Result<()> {
    use base64::Engine;
    let ts = "2026-10-20T17:20:52.588Z";
    let jpeg = base64::engine::general_purpose::STANDARD.encode([0xffu8, 0xd8, 0xff, 0xe0, 0x00, 0x10, b'J', b'F', b'I', b'F', 0, 1, 1, 0, 0, 1, 0, 1, 0, 0, 0xff, 0xd9]);
    let lines = [
        json!({"type":"session","version":3,"id":"history-repair-lab","timestamp":ts,"cwd":std::env::temp_dir()}),
        json!({"type":"message","id":"u1","parentId":null,"timestamp":ts,"message":{"role":"user","content":[{"type":"text","text":"Show me YouTube."}],"timestamp":1}}),
        json!({"type":"message","id":"a1","parentId":"u1","timestamp":ts,"message":{"role":"assistant","content":[{"type":"toolCall","id":"toolu_look","name":"lab_shot","arguments":{}}],
            "api":"anthropic-messages","provider":"opencode-go","model":provider::HAIKU,"stopReason":"toolUse","usage":{"input":10,"output":5,"cacheRead":0,"cacheWrite":0,"totalTokens":15,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}},"timestamp":2}}),
        json!({"type":"message","id":"r1","parentId":"a1","timestamp":ts,"message":{"role":"toolResult","toolCallId":"toolu_look","toolName":"lab_shot","isError":false,"content":[{"type":"image","data":jpeg,"mimeType":"image/png"},{"type":"text","text":"Display 1: size 1728 1117"}],"timestamp":3}}),
        json!({"type":"message","id":"a2","parentId":"r1","timestamp":ts,"message":{"role":"assistant","content":[],"api":"anthropic-messages","provider":"opencode-go","model":provider::HAIKU,"stopReason":"error",
            "errorMessage":"400 messages.2.content.0.tool_result.content.0.image.source.base64: The image was specified using the image/png media type, but the image appears to be a image/jpeg image","usage":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":0,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}},"timestamp":4}}),
        json!({"type":"compaction","id":"c1","parentId":"a2","timestamp":ts,"summary":"## Goal\n- User is testing the agent.","firstKeptEntryId":"u1","tokensBefore":112652}),
    ];
    std::fs::write(path, lines.iter().map(Value::to_string).collect::<Vec<_>>().join("\n") + "\n")?;
    Ok(())
}
