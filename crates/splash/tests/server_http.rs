//! The HTTP server end to end over the fake engine: chat completions whole and streamed, reasoning, tool calls,
//! structured output, stop sequences, limits, errors, the engine's death, a client that leaves, and the endpoints
//! Heimdall's engine manager probes. No model runs. The real Qwen3.8 tokenizer and template come from
//! SPLASH_TOKENIZER_DIR (default: Splash's prepared model on this Mac); the tests skip with a note without them.
use serde_json::{json, Value};
use splash::chat::format::QWEN;
use splash::chat::template::ChatTemplates;
use splash::chat::tokens::PromptTokenizer;
use splash::constrain::ConstraintFactory;
use splash::engine::fake::{FakeEngine, FakeHandle};
use splash::server::app::App;
use splash::server::http::{router, Server};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

const IM_END: u32 = 248046;

fn tokenizer_dir() -> Option<PathBuf> {
    let dir = std::env::var_os("SPLASH_TOKENIZER_DIR").map(PathBuf::from).unwrap_or_else(|| {
        PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(
            "Library/Application Support/Splash/models/.resolved/469b4497c8e3e5cff64591ad71a0aa828d59cdf241c84772599878d51566b454/tokenizer",
        )
    });
    if dir.join("tokenizer.json").is_file() {
        Some(dir)
    } else {
        eprintln!("skipped: no Qwen3.8 tokenizer at {} (set SPLASH_TOKENIZER_DIR)", dir.display());
        None
    }
}

struct Harness {
    base: String,
    fake: FakeHandle,
    _runtime: tokio::runtime::Runtime,
}

/// A server whose fake engine answers every prompt with `reply`'s tokens, then `<|im_end|>`.
fn harness(reply: &str) -> Option<Harness> {
    harness_with(reply, |b| b)
}

fn harness_with(reply: &str, configure: impl FnOnce(splash::engine::fake::FakeEngineBuilder) -> splash::engine::fake::FakeEngineBuilder) -> Option<Harness> {
    let dir = tokenizer_dir()?;
    let tokenizer = Arc::new(PromptTokenizer::from_dir(&dir).unwrap());
    let mut tokens = tokenizer.encode(reply);
    tokens.push(IM_END);
    let builder = FakeEngine::builder().context(32768).vocab(QWEN.vocabulary).batch(3).stop_token(IM_END).script(move |_| tokens.clone());
    let (engine, fake) = configure(builder).build();
    let ready = engine.wait_ready(Some(Duration::from_secs(5))).unwrap();
    let app = Arc::new(App {
        engine: splash::server::app::EngineSlot::new(engine),
        templates: ChatTemplates::load(&dir, tokenizer.encoder()).unwrap(),
        constraints: ConstraintFactory::new(tokenizer.tokenizer(), QWEN).unwrap(),
        tokenizer,
        format: QWEN,
        images: splash::images::ImageCache::new(),
        ready,
        model: "unsloth/Qwen3.8-27B-GGUF:UD-Q4_K_M".into(),
        response_model: "unsloth/Qwen3.8-27B-GGUF:UD-Q4_K_M".into(),
        model_names: vec!["unsloth/Qwen3.8-27B-GGUF:UD-Q4_K_M".into(), "qwen3.8-27b".into()],
        default_reasoning_effort: None,
        default_thinking_budget: None,
        request_timeout: None,
        max_image_pixels: splash::images::MAX_PIXELS,
        started_at: 0.0,
    });
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    let listener = runtime.block_on(tokio::net::TcpListener::bind("127.0.0.1:0")).unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = Server::new(app, 8, "127.0.0.1", port);
    runtime.spawn(async move { axum::serve(listener, router(server, 1 << 20)).await });
    Some(Harness { base: format!("http://127.0.0.1:{port}"), fake, _runtime: runtime })
}

fn client() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder().timeout(Duration::from_secs(20)).no_proxy().build().unwrap()
}

fn post(h: &Harness, body: Value) -> (u16, Value) {
    let r = client().post(format!("{}/v1/chat/completions", h.base)).json(&body).send().unwrap();
    (r.status().as_u16(), r.json().unwrap())
}

fn chunks(h: &Harness, body: Value) -> Vec<Value> {
    let text = client().post(format!("{}/v1/chat/completions", h.base)).json(&body).send().unwrap().text().unwrap();
    assert!(text.trim_end().ends_with("data: [DONE]"), "{text}");
    text.split("\n\n").filter_map(|e| e.strip_prefix("data: ")).filter(|d| *d != "[DONE]").map(|d| serde_json::from_str(d).unwrap()).collect()
}

fn hello() -> Value {
    json!({"model": "qwen3.8-27b", "messages": [{"role": "user", "content": "Say hello."}], "temperature": 0})
}

#[test]
fn a_whole_answer_splits_reasoning_from_content() {
    let Some(h) = harness("The user wants a greeting.\n</think>\n\nHello there.") else { return };
    let (status, body) = post(&h, hello());
    assert_eq!(status, 200, "{body}");
    let message = &body["choices"][0]["message"];
    assert_eq!(message["reasoning_content"], "The user wants a greeting.\n");
    assert_eq!(message["content"], "Hello there.");
    assert_eq!(body["choices"][0]["finish_reason"], "stop");
    assert_eq!(body["object"], "chat.completion");
    assert!(body["usage"]["prompt_tokens"].as_u64().unwrap() > 5);
    assert!(body["usage"]["completion_tokens_details"]["reasoning_tokens"].as_u64().unwrap() > 0);
    // The prompt the engine got is the template's, ending in the generation prompt that opens thinking.
    let request = &h.fake.log().requests[0];
    assert!(request.generation_prompt_tokens > 0);
    assert_eq!(request.sampling.temperature, 0.0);
}

#[test]
fn a_stream_carries_the_same_answer_in_chunks() {
    let Some(h) = harness("Thinking it over.\n</think>\n\nHello there, friend.") else { return };
    let mut body = hello();
    body["stream"] = true.into();
    body["stream_options"] = json!({"include_usage": true});
    let chunks = chunks(&h, body);
    assert_eq!(chunks[0]["choices"][0]["delta"], json!({"role": "assistant", "content": ""}));
    let text = |field: &str| chunks.iter().filter_map(|c| c["choices"][0]["delta"][field].as_str()).collect::<String>();
    assert_eq!(text("reasoning_content"), "Thinking it over.\n");
    assert_eq!(text("content"), "Hello there, friend.");
    let finish: Vec<_> = chunks.iter().filter_map(|c| c["choices"][0]["finish_reason"].as_str()).collect();
    assert_eq!(finish, ["stop"]);
    assert!(chunks.last().unwrap()["usage"]["total_tokens"].as_u64().unwrap() > 0);
}

fn weather_tool() -> Value {
    json!([{"type": "function", "function": {"name": "get_weather", "description": "Weather for a city",
        "parameters": {"type": "object", "properties": {"city": {"type": "string"}, "days": {"type": "integer"}}, "required": ["city"]}}}])
}

#[test]
fn a_tool_call_comes_back_as_openai_tool_calls() {
    let reply = "I should look it up.\n</think>\n\n<tool_call>\n<function=get_weather>\n<parameter=city>\nParis\n</parameter>\n<parameter=days>\n3\n</parameter>\n</function>\n</tool_call>";
    let Some(h) = harness(reply) else { return };
    let mut body = hello();
    body["tools"] = weather_tool();
    let (status, answer) = post(&h, body.clone());
    assert_eq!(status, 200, "{answer}");
    let call = &answer["choices"][0]["message"]["tool_calls"][0];
    assert_eq!(call["function"]["name"], "get_weather");
    let arguments: Value = serde_json::from_str(call["function"]["arguments"].as_str().unwrap()).unwrap();
    assert_eq!(arguments, json!({"city": "Paris", "days": 3}));
    assert_eq!(answer["choices"][0]["finish_reason"], "tool_calls");
    // Streamed, the call arrives as a header and argument fragments that add up to the same JSON.
    body["stream"] = true.into();
    let chunks = chunks(&h, body);
    let calls: Vec<&Value> = chunks.iter().filter_map(|c| c["choices"][0]["delta"]["tool_calls"].get(0)).collect();
    assert_eq!(calls[0]["function"]["name"], "get_weather");
    let streamed: String = calls.iter().filter_map(|c| c["function"]["arguments"].as_str()).collect();
    assert_eq!(serde_json::from_str::<Value>(&streamed).unwrap(), arguments);
}

#[test]
fn a_required_tool_call_generates_under_a_grammar() {
    let reply = "</think>\n\n<tool_call>\n<function=get_weather>\n<parameter=city>\nOslo\n</parameter>\n</function>\n</tool_call>";
    let Some(h) = harness(reply) else { return };
    let mut body = hello();
    body["tools"] = weather_tool();
    body["tool_choice"] = "required".into();
    let (status, answer) = post(&h, body);
    assert_eq!(status, 200, "{answer}");
    assert_eq!(answer["choices"][0]["message"]["tool_calls"][0]["function"]["name"], "get_weather");
    let log = h.fake.log();
    assert_eq!(log.requests[0].constraint, splash::protocol::ConstraintMode::TokenMask);
    assert!(!log.mask_responses.is_empty(), "the engine asked for masks and got them");
}

#[test]
fn structured_output_is_constrained_and_checked() {
    let Some(h) = harness("</think>\n\n{\"answer\": 42}") else { return };
    let mut body = hello();
    body["response_format"] = json!({"type": "json_schema", "json_schema": {"name": "a", "schema": {"type": "object", "properties": {"answer": {"type": "integer"}}, "required": ["answer"]}}});
    let (status, answer) = post(&h, body);
    assert_eq!(status, 200, "{answer}");
    let content: Value = serde_json::from_str(answer["choices"][0]["message"]["content"].as_str().unwrap()).unwrap();
    assert_eq!(content, json!({"answer": 42}));
}

#[test]
fn a_stop_sequence_cuts_the_answer_and_stops_the_engine() {
    let Some(h) = harness_with("</think>\n\nOne, two, STOP, three, four, five, six.", |b| b.token_delay(Duration::from_millis(5))) else { return };
    let mut body = hello();
    body["stop"] = "STOP".into();
    let (status, answer) = post(&h, body);
    assert_eq!(status, 200, "{answer}");
    assert_eq!(answer["choices"][0]["message"]["content"], "One, two, ");
    assert_eq!(answer["choices"][0]["finish_reason"], "stop");
    assert!(h.fake.wait_for(Duration::from_secs(2), |l| !l.cancels.is_empty()));
}

#[test]
fn the_output_limit_ends_with_length() {
    let Some(h) = harness("</think>\n\nA long answer that goes on and on and on.") else { return };
    let mut body = hello();
    body["max_tokens"] = 4.into();
    let (status, answer) = post(&h, body);
    assert_eq!(status, 200, "{answer}");
    assert_eq!(answer["choices"][0]["finish_reason"], "length");
    assert_eq!(answer["usage"]["completion_tokens"], 4);
    assert_eq!(h.fake.log().requests[0].logical_max_output_tokens, 4);
}

#[test]
fn reasoning_effort_none_renders_without_thinking() {
    let Some(h) = harness("Plain hello.") else { return };
    let mut body = hello();
    body["reasoning_effort"] = "none".into();
    let (status, answer) = post(&h, body);
    assert_eq!(status, 200, "{answer}");
    assert_eq!(answer["choices"][0]["message"]["content"], "Plain hello.");
    assert!(answer["choices"][0]["message"].get("reasoning_content").is_none());
}

#[test]
fn bad_requests_get_splashs_errors() {
    let Some(h) = harness("</think>\n\nx") else { return };
    for (patch, status, needle) in [
        (json!({"temperature": 3}), 400, "temperature must be a number in [0, 2]"),
        (json!({"model": "gpt-9"}), 404, "model gpt-9 not found"),
        (json!({"messages": "hi"}), 400, ""),
        (json!({"reasoning_effort": "huge"}), 400, "invalid reasoning_effort"),
        (json!({"n": 2}), 400, "n and logprobs are not currently supported"),
        (json!({"max_tokens": 0}), 400, "max_tokens must be a positive integer"),
    ] {
        let mut body = hello();
        for (k, v) in patch.as_object().unwrap() {
            body[k] = v.clone();
        }
        let (got, answer) = post(&h, body);
        assert_eq!(got, status, "{patch}: {answer}");
        assert!(answer["error"]["message"].as_str().unwrap().contains(needle), "{patch}: {answer}");
    }
    assert!(h.fake.log().requests.is_empty(), "no refused request reached the engine");
}

#[test]
fn the_probes_heimdall_reads_answer() {
    let Some(h) = harness_with("x", |b| b.disk(4096, 107374182400)) else { return };
    let get = |path: &str| {
        let r = client().get(format!("{}{path}", h.base)).send().unwrap();
        (r.status().as_u16(), r.json::<Value>().unwrap())
    };
    assert_eq!(get("/health"), (200, json!({"status": "ok"})));
    assert_eq!(get("/ready").0, 200);
    let (_, models) = get("/v1/models");
    let served = models["data"].as_array().unwrap().iter().find(|m| m["id"] == "qwen3.8-27b").unwrap();
    assert_eq!(served["max_model_len"], 32768);
    let (_, status) = get("/status");
    assert_eq!(status["disk"]["used_bytes"], 4096);
    assert_eq!(status["disk"]["capacity_bytes"], 107374182400u64);
    assert_eq!(status["weights"]["released"], false);
    // Memory release takes no fields: a bare POST is answered, never refused for its missing Content-Type.
    let r = client().post(format!("{}/v1/memory/release", h.base)).send().unwrap();
    assert_ne!(r.status().as_u16(), 415);
    assert!(h.fake.wait_for(Duration::from_secs(2), |l| !l.release_requests.is_empty()));
    // A page on another site cannot reach the server through a rebound name.
    let r = client().get(format!("{}/health", h.base)).header("Host", "evil.example").send().unwrap();
    assert_eq!(r.status().as_u16(), 403);
}

#[test]
fn a_dead_engine_refuses_new_requests() {
    let Some(h) = harness("</think>\n\nx") else { return };
    h.fake.crash();
    std::thread::sleep(Duration::from_millis(200));
    let (status, answer) = post(&h, hello());
    assert_eq!(status, 503, "{answer}");
    assert_eq!(client().get(format!("{}/ready", h.base)).send().unwrap().status().as_u16(), 503);
}

#[test]
fn a_client_that_leaves_cancels_its_request() {
    let Some(h) = harness_with(&"word ".repeat(400), |b| b.token_delay(Duration::from_millis(20))) else { return };
    let mut body = hello();
    body["stream"] = true.into();
    body["reasoning_effort"] = "none".into();
    let mut response = client().post(format!("{}/v1/chat/completions", h.base)).json(&body).send().unwrap();
    let mut first = [0u8; 64];
    use std::io::Read;
    let _ = response.read(&mut first).unwrap();
    drop(response);
    assert!(h.fake.wait_for(Duration::from_secs(5), |l| !l.cancels.is_empty()), "the engine heard the cancel");
}

#[test]
fn pages_of_other_sites_and_non_json_bodies_are_refused() {
    let Some(h) = harness("</think>\n\nx") else { return };
    let url = format!("{}/v1/chat/completions", h.base);
    // A form or text/plain post needs no preflight, so the server itself must refuse it.
    let r = client().post(&url).header("Content-Type", "text/plain").body(hello().to_string()).send().unwrap();
    assert_eq!(r.status().as_u16(), 415);
    let r = client().post(&url).header("Origin", "https://evil.example").json(&hello()).send().unwrap();
    assert_eq!(r.status().as_u16(), 403);
    // The server's own origin is fine.
    let own = h.base.clone();
    let r = client().post(&url).header("Origin", own).json(&hello()).send().unwrap();
    assert_eq!(r.status().as_u16(), 200);
    assert_eq!(h.fake.log().requests.len(), 1, "only the allowed request reached the engine");
    // Overload answers carry Retry-After, as Python's 503s do.
    h.fake.crash();
    std::thread::sleep(Duration::from_millis(200));
    let r = client().post(&url).json(&hello()).send().unwrap();
    assert_eq!((r.status().as_u16(), r.headers()["retry-after"].to_str().unwrap()), (503, "1"));
}

#[test]
fn a_stream_that_fails_before_it_starts_answers_with_an_http_status() {
    // One lane: the second request waits for it without a START, then the engine dies.
    let Some(h) = harness_with("</think>\n\nx", |b| b.capacity(1)) else { return };
    h.fake.pause();
    let url = format!("{}/v1/chat/completions", h.base);
    let first = { let url = url.clone(); std::thread::spawn(move || client().post(&url).json(&{ let mut b = hello(); b["stream"] = true.into(); b }).send().map(|r| r.status().as_u16())) };
    assert!(h.fake.wait_for(Duration::from_secs(5), |l| l.requests.len() == 1));
    let second = { let url = url.clone(); std::thread::spawn(move || client().post(&url).json(&{ let mut b = hello(); b["stream"] = true.into(); b }).send().map(|r| r.status().as_u16())) };
    assert!(h.fake.wait_for(Duration::from_secs(5), |l| l.requests.len() == 2));
    std::thread::sleep(Duration::from_millis(300));
    h.fake.crash();
    // The started stream answered 200 and ends with an error event; the waiting one gets a real 503.
    assert_eq!(first.join().unwrap().unwrap(), 200);
    assert_eq!(second.join().unwrap().unwrap(), 503);
}

#[test]
fn a_long_held_tool_argument_still_sends_keepalives() {
    let items: Vec<String> = (0..40).map(|i| format!("\"item number {i}\"")).collect();
    let reply = format!("</think>\n\n<tool_call>\n<function=save>\n<parameter=items>\n[{}]\n</parameter>\n</function>\n</tool_call>", items.join(", "));
    let Some(h) = harness_with(&reply, |b| b.token_delay(Duration::from_millis(40))) else { return };
    let mut body = hello();
    body["stream"] = true.into();
    body["tools"] = json!([{"type": "function", "function": {"name": "save", "parameters": {"type": "object", "properties": {"items": {"type": "array", "items": {"type": "string"}}}}}}]);
    let chunks = chunks(&h, body);
    let arguments: Vec<usize> = chunks.iter().enumerate().filter(|(_, c)| c["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"].is_string()).map(|(i, _)| i).collect();
    // The arguments open with "{" at once; the array is held until it ends, seconds later, and empty chunks keep the
    // stream alive meanwhile.
    let (open, array) = (arguments[0], arguments[1]);
    let keepalives = chunks[open..array].iter().filter(|c| c["choices"][0]["delta"] == json!({})).count();
    assert!(keepalives >= 1, "{keepalives} keepalives between the arguments' opening and the held array");
}
