//! [providers] Lab contracts for ChatGPT (Sign in with ChatGPT, the plan's usage) through Heimdall's real pinned Pi
//! and production extension. The fake cases put a local server where api.openai.com is (the lab extension rewrites
//! only the fetch destination), so Pi's built-in `openai` provider sees the production URL and a ChatGPT-style token
//! (not `sk-`), which is how it knows to send the plan-usage request shape.
//!
//! Run (offline): HEIMDALL_CHATGPT_LAB=<abs path containing /target/chatgpt-lab/> cargo nextest run -p agent \
//!      --test chatgpt_lab --run-ignored only -E 'not test(live)'
//! Live (one tiny request on the user's plan, after his browser consent; opens the browser only with
//! HEIMDALL_CHATGPT_OPEN=1, else prints the link): HEIMDALL_CHATGPT_LIVE=1 plus the line above with -E 'test(live)'.
use agent::{
    harness::{Harness, Pi, PiConfig},
    protocol::Event,
    provider::{self, ApiModel},
};
use anyhow::{ensure, Context, Result};
use axum::{body::Bytes, http::{HeaderMap, StatusCode}, response::{IntoResponse, Response}, routing::post, Router};
use serde_json::{json, Value};
use std::{path::{Path, PathBuf}, sync::{Arc, Mutex}, time::Duration};

/// A ChatGPT-style access token (a JWT, never `sk-`): what Pi's per-request command prints in the installed app.
const LAB_TOKEN: &str = "eyJhbGciOiJub25lIn0.eyJsYWIiOnRydWV9.lab";

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
        Ok(json!({"hits":[]}))
    }
}

fn lab_base() -> Result<PathBuf> {
    let base = PathBuf::from(std::env::var("HEIMDALL_CHATGPT_LAB").context("HEIMDALL_CHATGPT_LAB")?);
    ensure!(base.is_absolute() && base.to_string_lossy().contains("/target/chatgpt-lab"), "isolated lab path required");
    Ok(base)
}

struct Lab { config: PiConfig, server: agent::mcp::McpServer, gate: Arc<agent::delivery::DeliveryGate> }

/// One lab session on `model`: Pi's key is `api_key` (a command or an environment reference), and with `fake` every
/// request for api.openai.com goes to it instead.
async fn lab(base: &Path, case: &str, model: &ApiModel, api_key: String, fake: Option<std::net::SocketAddr>) -> Result<Lab> {
    lab_tools(base, case, model, api_key, fake, &[]).await
}

async fn lab_tools(base: &Path, case: &str, model: &ApiModel, api_key: String, fake: Option<std::net::SocketAddr>, tools: &[&str]) -> Result<Lab> {
    // The harness hands a lab build's credential to Pi in this variable (the installed app's Pi runs the command).
    if std::env::var_os(provider::provider(provider::CHATGPT).unwrap().key_env).is_none() {
        // SAFETY: lab tests run one per process (nextest); nothing else reads the environment concurrently.
        unsafe { std::env::set_var(provider::provider(provider::CHATGPT).unwrap().key_env, LAB_TOKEN) };
    }
    let runtime = tokio::task::spawn_blocking(agent::runtime::Runtime::verified).await??;
    let data = base.join(case);
    let _ = std::fs::remove_dir_all(&data);
    std::fs::create_dir_all(&data)?;
    let catalog = provider::pi_catalog(&runtime.root, model.provider_info().unwrap());
    ensure!(catalog.is_some(), "Pi's pinned OpenAI catalog is missing from the runtime");
    // A sign-in, as the installed app configures it (agent::runtime::PiAuth::Console, no endpoint or headers).
    let stand_in = api_key.contains(LAB_TOKEN);
    let auth = agent::runtime::PiAuth::Console { api_key, base_url: None, headers: Default::default(), models: vec![] };
    let dir = agent::runtime::configure_api(&data, model, catalog.as_ref(), &auth)?;
    let written = std::fs::read_to_string(dir.join("models.json"))?;
    // The command stand-in prints the lab token itself; otherwise no token may reach Pi's config.
    ensure!(stand_in || !written.contains(LAB_TOKEN), "the token must not be written to Pi's config");
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
    let rewrite = fake.map_or(String::new(), |fake| format!(r#"
const fetch = globalThis.fetch;
globalThis.fetch = (input, init) => {{
  const url = typeof input === 'string' || input instanceof URL ? String(input) : input.url;
  return url.startsWith('https://api.openai.com/') ? fetch(url.replace('https://api.openai.com', 'http://{fake}'), init) : fetch(input, init);
}};"#));
    std::fs::write(&ext, format!(
        "import heimdall from {production};\n{rewrite}\nexport default function(pi) {{ heimdall(pi); pi.on('session_start', () => pi.setActiveTools({tools})); }}\n",
        production = serde_json::to_string(&runtime.extension.to_string_lossy())?,
        tools = serde_json::to_string(tools)?,
    ))?;
    config.extension = Some(ext);
    Ok(Lab { config, server, gate })
}

async fn turn(lab: Lab, id: &str, text: &str, deadline: Duration) -> Result<(bool, String)> {
    let mut pi = Pi::launch_config_permitted(lab.config, Some(lab.gate.permit()?))?;
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
    }).await;
    let stopped = pi.shutdown().await;
    lab.server.shutdown().await;
    let out = result.context("lab turn deadline")??;
    stopped?;
    Ok(out)
}

/// OpenAI Responses streaming, as `/v1/responses` sends it: one message with `text`.
fn responses_sse(model: &str, text: &str) -> Response {
    let mut out = String::new();
    let mut event = |v: Value| out.push_str(&format!("event: {}\ndata: {v}\n\n", v["type"].as_str().unwrap()));
    let item = json!({"id":"msg_lab","type":"message","status":"in_progress","role":"assistant","content":[]});
    event(json!({"type":"response.created","sequence_number":0,"response":{"id":"resp_lab","object":"response","status":"in_progress","model":model,"output":[]}}));
    event(json!({"type":"response.output_item.added","sequence_number":1,"output_index":0,"item":item}));
    event(json!({"type":"response.content_part.added","sequence_number":2,"item_id":"msg_lab","output_index":0,"content_index":0,"part":{"type":"output_text","text":"","annotations":[]}}));
    let (a, z) = text.split_at(text.len() / 2);
    for (i, part) in [a, z].iter().enumerate() {
        event(json!({"type":"response.output_text.delta","sequence_number":3 + i,"item_id":"msg_lab","output_index":0,"content_index":0,"delta":part}));
    }
    event(json!({"type":"response.output_text.done","sequence_number":5,"item_id":"msg_lab","output_index":0,"content_index":0,"text":text}));
    let done = json!({"id":"msg_lab","type":"message","status":"completed","role":"assistant","content":[{"type":"output_text","text":text,"annotations":[]}]});
    event(json!({"type":"response.output_item.done","sequence_number":6,"output_index":0,"item":done}));
    event(json!({"type":"response.completed","sequence_number":7,"response":{"id":"resp_lab","object":"response","status":"completed","model":model,"output":[done],
        "usage":{"input_tokens":1200,"input_tokens_details":{"cached_tokens":0},"output_tokens":4,"output_tokens_details":{"reasoning_tokens":0},"total_tokens":1204}}}));
    ([("content-type", "text/event-stream")], out).into_response()
}

async fn serve(app: Router) -> Result<(std::net::SocketAddr, tokio::task::JoinHandle<()>)> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    Ok((addr, tokio::spawn(async move { let _ = axum::serve(listener, app).await; })))
}

/// The default model from Pi's pinned catalog, as Add Model keeps it.
fn default_model() -> Result<ApiModel> {
    let rt = agent::runtime::Runtime::verified()?;
    let p = provider::provider(provider::CHATGPT).unwrap();
    let catalog = provider::pi_catalog(&rt.root, p).context("Pi's OpenAI catalog")?;
    let m = provider::merge_catalog(p, &[json!({"id": agent::chatgpt::DEFAULT_MODEL})], Some(&catalog), None).remove(0);
    ensure!(m.api == provider::Api::OpenaiResponses && m.unsupported().is_none(), "{m:?}");
    Ok(m)
}

/// The plan-usage request shape: Pi's per-request command hands over the token (the installed app's path), Pi sends
/// it as `Authorization: Bearer` to `/v1/responses` with `store: false` and `stream: true` and without the fields this
/// route rejects, and the streamed answer completes the turn. Then a usage limit is named in the user's words.
#[tokio::test]
#[ignore = "explicit pinned-runtime lab: HEIMDALL_CHATGPT_LAB"]
async fn plan_requests_reach_the_responses_api_and_limits_are_named() -> Result<()> {
    let base = lab_base()?;
    let model = default_model()?;
    let seen: Arc<Mutex<Vec<Value>>> = Arc::default();
    let limited = Arc::new(Mutex::new(false));
    let (s, l) = (seen.clone(), limited.clone());
    let app = Router::new().route("/v1/responses", post(move |headers: HeaderMap, body: Bytes| {
        let (s, l) = (s.clone(), l.clone());
        async move {
            let body: Value = serde_json::from_slice(&body).unwrap();
            let auth = headers.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or_default().replace(LAB_TOKEN, "<token>");
            let rejected: Vec<&str> = ["max_output_tokens","temperature","prompt_cache_retention","top_p","metadata","user","safety_identifier","truncation","background"].into_iter().filter(|k| body.get(*k).is_some()).collect();
            s.lock().unwrap().push(json!({"authorization": auth, "model": body["model"], "store": body["store"], "stream": body["stream"],
                "rejected_fields": rejected,
                "tools": body["tools"].as_array().map(|t| t.iter().map(|x| json!({"type": x["type"], "name": x["name"], "namespace": x.get("namespace").is_some() || x["type"] == "namespace"})).collect::<Vec<_>>()),
                "additional_tools": body["input"].as_array().is_some_and(|a| a.iter().any(|i| i["type"] == "additional_tools" || i.get("additional_tools").is_some())),
                "system_items": body["input"].as_array().is_some_and(|a| a.iter().any(|i| i["role"] == "system")), "instructions": body["instructions"].is_string() || body["input"].as_array().is_some_and(|a| a.iter().any(|i| i["role"] == "developer"))}));
            if *l.lock().unwrap() {
                return (StatusCode::TOO_MANY_REQUESTS, axum::Json(json!({"error":{"type":"invalid_request_error","code":"subscription_sharing_usage_limit_exceeded","message":"You have reached your usage limit for this app."}}))).into_response();
            }
            responses_sse(body["model"].as_str().unwrap_or_default(), "CHATGPT_OK")
        }
    }));
    let (addr, task) = serve(app).await?;
    let lab_ = lab_tools(&base, "plan", &model, format!("!printf %s {LAB_TOKEN}"), Some(addr), &["mcp__heimdall__timeline_search"]).await?;
    let (error, text) = turn(lab_, "chatgpt-lab", "Reply CHATGPT_OK.", Duration::from_secs(90)).await?;
    let requests = seen.lock().unwrap().clone();
    ensure!(!error && text.contains("CHATGPT_OK"), "turn failed: {text} {requests:?}");
    ensure!(!requests.is_empty(), "no request reached /v1/responses");
    for r in &requests {
        ensure!(r["authorization"] == "Bearer <token>", "token not sent as Bearer: {r}");
        ensure!(r["model"] == agent::chatgpt::DEFAULT_MODEL && r["store"] == false && r["stream"] == true, "request shape: {r}");
        ensure!(r["rejected_fields"].as_array().is_some_and(|a| a.is_empty()), "fields the plan route rejects were sent: {r}");
        ensure!(r["system_items"] == false, "explicit system items are rejected on this route: {r}");
    }
    *limited.lock().unwrap() = true;
    let lab_ = lab(&base, "limit", &model, format!("!printf %s {LAB_TOKEN}"), Some(addr)).await?;
    let (error, text) = turn(lab_, "chatgpt-limit", "Reply CHATGPT_OK.", Duration::from_secs(120)).await?;
    task.abort();
    ensure!(error, "a usage limit must fail the turn: {text}");
    let said = provider::explain_error(provider::CHATGPT, &text);
    ensure!(said.as_deref() == Some("ChatGPT: usage limit reached · see ChatGPT ▸ Settings ▸ Usage"), "limit not named: {text} → {said:?}");
    std::fs::write(base.join("plan-receipt.json"), serde_json::to_vec_pretty(&json!({"requests": requests, "limit_error": text, "limit_said": said}))?)?;
    Ok(())
}

/// One real round trip on the user's plan: Heimdall's own sign-in (his consent in the browser), the eligibility probe,
/// then one Pi turn to api.openai.com. Nothing is saved: the session lives in this process only.
#[tokio::test]
#[ignore = "live: one tiny request on the ChatGPT plan after a browser consent; HEIMDALL_CHATGPT_LIVE=1"]
async fn live_round_trip_on_the_plan() -> Result<()> {
    ensure!(std::env::var("HEIMDALL_CHATGPT_LIVE").as_deref() == Ok("1"), "HEIMDALL_CHATGPT_LIVE=1 required");
    let base = lab_base()?;
    let host = agent::chatgpt::host_id(&base)?;
    let login = agent::chatgpt::begin(agent::chatgpt::AUTH_BASE, &host, None, agent::chatgpt::PORT).await?;
    eprintln!("Open to connect ChatGPT: {}", login.url);
    if std::env::var("HEIMDALL_CHATGPT_OPEN").as_deref() == Ok("1") {
        std::process::Command::new("/usr/bin/open").arg(&login.url).status()?;
    }
    let session = agent::chatgpt::finish(login, || false, Duration::from_secs(600)).await?.session;
    let listed = agent::chatgpt::models(agent::chatgpt::API_BASE, &session.access).await?;
    eprintln!("models: {}", listed.iter().filter_map(|m| m["id"].as_str()).collect::<Vec<_>>().join(", "));
    agent::chatgpt::probe(agent::chatgpt::API_BASE, &session.access, &agent::chatgpt::test_model(&listed)).await?;
    let p = provider::provider(provider::CHATGPT).unwrap();
    // SAFETY: one lab test per process; the token reaches Pi through its environment as lab builds hand it over.
    unsafe { std::env::set_var(p.key_env, &session.access) };
    // One tool call too: OpenAI's preview notes group tools in namespaces, while Pi sends plain function tools; this
    // settles whether the agent's tools work on the plan route.
    let lab_ = lab_tools(&base, "live", &default_model()?, format!("${}", p.key_env), None, &["mcp__heimdall__timeline_search"]).await?;
    let (error, text) = turn(lab_, "chatgpt-live", "Call mcp__heimdall__timeline_search once with query heimdall and limit 1, then reply with exactly: HEIMDALL_CHATGPT_LIVE_OK", Duration::from_secs(120)).await?;
    ensure!(!error && text.contains("HEIMDALL_CHATGPT_LIVE_OK"), "live turn: {text}");
    eprintln!("live reply: {text}");
    Ok(())
}
