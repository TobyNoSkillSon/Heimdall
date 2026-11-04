use super::*;

fn call(model: &str, prompt: Prompt) -> Call<'static> {
    Call { job: "test job", model: model.into(), system: "You sort things.", schema: json!({"type": "object"}), effort: "high", prompt, history: Vec::new(), session: None }
}

fn two_stretches(now: &str) -> Prompt {
    let mut p = Prompt::default();
    p.stable("<summary>long ago</summary>\n").stable("lines 1-40\n");
    p.push(format!("live state\nNow: {now}\n\nDo it now."));
    p
}

#[test]
fn the_body_marks_the_system_and_each_stable_stretch_for_the_hour_cache_and_ends_with_the_volatile_part() {
    let b = body(&call("claude-haiku-5-5", two_stretches("10:00")), 1234);
    let hour = json!({"type": "ephemeral", "ttl": "1h"});
    assert_eq!(b["system"], json!([{"type": "text", "text": "You sort things.", "cache_control": hour}]));
    let content = b["messages"][0]["content"].as_array().unwrap();
    assert_eq!(content.len(), 3);
    assert_eq!((content[0]["cache_control"].clone(), content[1]["cache_control"].clone()), (hour.clone(), hour));
    assert!(content[2].get("cache_control").is_none() && content[2]["text"].as_str().unwrap().contains("Now: 10:00"), "the clock is last, after every breakpoint");
    assert_eq!(b["messages"].as_array().unwrap().len(), 1, "one user turn: no prefill");
    assert_eq!((b["max_tokens"].as_u64(), b["thinking"].clone(), b["output_config"]["effort"].clone()), (Some(1234), json!({"type": "adaptive"}), json!("high")));
    assert_eq!(b["output_config"]["format"], json!({"type": "json_schema", "schema": {"type": "object"}}));
    assert!(b.get("temperature").is_none() && b.get("top_p").is_none() && b.get("tools").is_none());
}

#[test]
fn at_most_three_user_breakpoints_are_kept_the_newest_and_empty_blocks_are_dropped() {
    let mut p = Prompt::default();
    for k in 0..6 { p.stable(format!("chunk {k}\n")); }
    p.stable("").push("tail");
    let b = body(&call("claude-haiku-5-5", p.clone()), 100);
    let marked: Vec<usize> = b["messages"][0]["content"].as_array().unwrap().iter().enumerate().filter(|(_, v)| v.get("cache_control").is_some()).map(|(i, _)| i).collect();
    assert_eq!(marked, [3, 4, 5], "with the system's, four: the API's maximum");
    assert_eq!(b["messages"][0]["content"].as_array().unwrap().len(), 7);
    assert_eq!(p.text(), "chunk 0\nchunk 1\nchunk 2\nchunk 3\nchunk 4\nchunk 5\ntail");
}

#[test]
fn the_prefix_is_byte_identical_between_runs_while_the_prompt_is_unchanged() {
    let a = body(&call("claude-haiku-5-5", two_stretches("10:00")), 100);
    let b = body(&call("claude-haiku-5-5", two_stretches("10:05")), 100);
    let prefix = |v: &Value| {
        let c = v["messages"][0]["content"].as_array().unwrap();
        (v["system"].to_string(), v["output_config"].to_string(), v["model"].clone(), c[..c.len() - 1].iter().map(Value::to_string).collect::<Vec<_>>())
    };
    assert_eq!(prefix(&a), prefix(&b));
    assert_ne!(a, b, "only the volatile tail moved");
    assert_eq!(prompt_hash(&call("claude-haiku-5-5", two_stretches("10:00"))), prompt_hash(&call("claude-haiku-5-5", "other".into())), "the hash is the stable prefix's");
    let mut changed = call("claude-haiku-5-5", "x".into());
    changed.system = "You sort things differently.";
    assert_ne!(prompt_hash(&changed), prompt_hash(&call("claude-haiku-5-5", "x".into())));
}

#[test]
fn max_tokens_is_the_model_s_maximum_unless_the_limit_needs_less() {
    let small: Prompt = "short".into();
    let b = budget(&call("claude-haiku-5-5", small.clone())).unwrap();
    assert_eq!((b.max_tokens, b.limited), (128_000, false), "Haiku 5.5: 128K, the limit never binds");
    assert_eq!(budget(&call("claude-haiku-4-5", small.clone())).unwrap().max_tokens, 64_000);
    // Sonnet 5.5 at $10/M out: 128K would be $1.28, so as many as $0.50 buys.
    let s = budget(&call("claude-sonnet-5-5", small.clone())).unwrap();
    assert!(s.limited && (49_000..50_000).contains(&s.max_tokens), "{s:?}");
    let o = budget(&call("claude-opus-5-5", small.clone())).unwrap();
    assert!(o.limited && (24_000..25_000).contains(&o.max_tokens), "{o:?}");
    for m in MODELS {
        let b = budget(&call(m.id, small.clone())).unwrap();
        assert!(b.worst_usd <= LIMIT_USD + 1e-9 && b.max_tokens <= m.max_output, "{} {b:?}", m.id);
    }
    // A dated snapshot id is its model's; an unknown model is priced as the dearest one.
    assert_eq!(model("claude-haiku-4-5-20251001"), model("claude-haiku-4-5"));
    assert_eq!(budget(&call("claude-someday-9", small.clone())).unwrap().max_tokens, budget(&call("claude-fable-5-1", small)).unwrap().max_tokens);
}

#[test]
fn the_worst_case_prices_a_cold_cache_and_the_prompt_s_tier_and_skips_only_past_fifty_cents() {
    // Haiku 5.5, 400 kB of conversation (~160K tokens by the 2.5-byte estimate): the dearer tier, every stable token a
    // one-hour write ($1/M), 128K out at $2.50/M: about $0.48, under the limit, nothing lowered.
    let mut p = Prompt::default();
    p.stable("x".repeat(400_000)).push("Now: 10:00");
    let b = budget(&call("claude-haiku-5-5", p.clone())).unwrap();
    assert!(b.prompt_tokens > 100_000 && !b.limited && (0.47..0.49).contains(&b.worst_usd), "{b:?}");
    // The same prompt on Opus 5.5: its input alone ($8/M written) passes $0.50: skipped, never sent.
    let err = budget(&call("claude-opus-5-5", p.clone())).unwrap_err();
    assert!(err.input_usd > LIMIT_USD && err.to_string().contains("skipped"), "{err}");
    let prepared = prepare(&call("claude-opus-5-5", p));
    assert!(prepared.unwrap_err().downcast_ref::<OverLimit>().is_some(), "the jobs tell a skip from a failure");
    // Uncached text is priced at the input price, not the write price.
    let mut cold = Prompt::default();
    cold.stable("x".repeat(30_000));
    let mut warm = Prompt::default();
    warm.push("x".repeat(30_000));
    assert!(budget(&call("claude-sonnet-5-5", cold)).unwrap().max_tokens < budget(&call("claude-sonnet-5-5", warm)).unwrap().max_tokens);
}

#[test]
fn usage_parses_both_cache_fields_and_prices_each_at_its_rate() {
    let u = Usage::from_api(&json!({"input_tokens": 2048, "cache_read_input_tokens": 1800, "cache_creation_input_tokens": 248, "output_tokens": 503,
                                    "cache_creation": {"ephemeral_5m_input_tokens": 148, "ephemeral_1h_input_tokens": 100}}));
    assert_eq!(u, Usage { input: 2048, output: 503, cache_read: 1800, cache_write_5m: 148, cache_write_1h: 100 });
    assert_eq!((u.prompt(), u.cache_write()), (4096, 248));
    // Without the split every write is a one-hour write (the only TTL these requests ask for).
    let u = Usage::from_api(&json!({"input_tokens": 1000, "cache_read_input_tokens": 50_000, "cache_creation_input_tokens": 9000, "output_tokens": 12_000}));
    assert_eq!((u.cache_write_1h, u.cache_write_5m), (9000, 0));
    // Haiku 5.5 under 100K: 1000×0.10 + 50k×0.01 + 9000×0.20 + 12k×0.50 per M.
    let usd = u.dollars(&model("claude-haiku-5-5"));
    assert!((usd - (100.0 + 500.0 + 1800.0 + 6000.0) / 1e6).abs() < 1e-12, "{usd}");
    // Over 100K the whole request pays the dearer prices, cache reads included.
    let big = Usage { input: 1000, cache_read: 120_000, output: 1000, ..Default::default() };
    assert!((big.dollars(&model("claude-haiku-5-5")) - (500.0 + 6000.0 + 2500.0) / 1e6).abs() < 1e-12);
    // Sonnet 5.5 reads at $0.10/M (pricing page, 24 Oct 2026).
    assert!((Usage { cache_read: 1_000_000, ..Default::default() }.dollars(&model("claude-sonnet-5-5")) - 0.10).abs() < 1e-12);
}

#[test]
fn every_answered_cut_or_refused_run_is_counted_in_the_ledger_with_its_cost() {
    let runs: Arc<Mutex<Vec<Run>>> = Default::default();
    let seen = runs.clone();
    set_ledger(Arc::new(move |r: &Run| seen.lock().unwrap().push(r.clone())));
    let c = call("claude-haiku-5-5", two_stretches("10:00"));
    let prepared = prepare(&c).unwrap();
    let usage = json!({"input_tokens": 300, "cache_read_input_tokens": 9000, "cache_creation_input_tokens": 700, "output_tokens": 2000});
    let ok = json!({"content": [{"type": "thinking", "thinking": "", "signature": "s"}, {"type": "text", "text": "{\"a\":1}"}], "stop_reason": "end_turn", "usage": usage});
    let a = finish(&c, &prepared, &ok).unwrap();
    assert_eq!(a.text, "{\"a\":1}");
    assert_eq!((a.usage.cache_read, a.usage.cache_write_1h), (9000, 700));
    let cut = json!({"content": [], "stop_reason": "max_tokens", "usage": usage});
    let e = finish(&c, &prepared, &cut).unwrap_err();
    assert!(e.to_string().contains("cut off") && settled(&e), "a paid run without an answer is not resent");
    assert_eq!(e.downcast_ref::<Stopped>().unwrap().usage.cache_read, 9000, "its usage travels with it");
    let refused = json!({"content": [], "stop_reason": "refusal", "usage": usage});
    assert!(finish(&c, &prepared, &refused).is_err());
    // A run whose max_tokens the limit lowered stops "at the limit".
    let sonnet = call("claude-sonnet-5-5", "x".into());
    let lowered = prepare(&sonnet).unwrap();
    assert!(finish(&sonnet, &lowered, &cut).unwrap_err().to_string().contains("$0.50 limit"));
    let runs = runs.lock().unwrap().clone();
    assert_eq!(runs.iter().map(|r| r.outcome).collect::<Vec<_>>(), ["answered", "cut off", "refused", "limit"]);
    let r = &runs[0];
    assert_eq!((r.job, r.model.as_str(), r.hash), ("test job", "claude-haiku-5-5", prompt_hash(&c)));
    assert!((r.usd - (300.0 * 0.10 + 9000.0 * 0.01 + 700.0 * 0.20 + 2000.0 * 0.50) / 1e6).abs() < 1e-12, "{}", r.usd);
    let line = r.line();
    assert_eq!((line["cache_read"].as_u64(), line["cache_write"].as_u64(), line["outcome"].as_str()), (Some(9000), Some(700), Some("answered")));
    assert!(line.get("text").is_none() && !line.to_string().contains("Now:"), "no request content in the ledger");
}

/// The whole path against a fake api.anthropic.com: the body the client sends, the key header, the answer, the ledger.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_run_sends_the_cached_layout_and_counts_the_response_s_usage() {
    use axum::{extract::State, http::HeaderMap, routing::post, Json, Router};
    let got: Arc<Mutex<Vec<Value>>> = Default::default();
    async fn messages(State(got): State<Arc<Mutex<Vec<Value>>>>, h: HeaderMap, Json(body): Json<Value>) -> Json<Value> {
        assert_eq!((h.get("x-api-key").unwrap().to_str().unwrap(), h.get("anthropic-version").unwrap().to_str().unwrap()), ("sk-test", "2023-06-01"));
        let n = { let mut g = got.lock().unwrap(); g.push(body); g.len() };
        // The second identical prefix reads what the first wrote.
        let usage = if n == 1 { json!({"input_tokens": 12, "cache_creation_input_tokens": 900, "cache_read_input_tokens": 0, "output_tokens": 40}) }
                    else { json!({"input_tokens": 12, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 900, "output_tokens": 40}) };
        Json(json!({"content": [{"type": "text", "text": "{}"}], "stop_reason": "end_turn", "usage": usage}))
    }
    let app = Router::new().route("/v1/messages", post(messages)).with_state(got.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let e = Endpoint { base, key: Some("sk-test".into()), workspace: None, timeout: Duration::from_secs(10), model: None, ..Default::default() };
    let runs: Arc<Mutex<Vec<Run>>> = Default::default();
    let seen = runs.clone();
    set_ledger(Arc::new(move |r: &Run| seen.lock().unwrap().push(r.clone())));
    let first = ask(&e, &call("claude-haiku-5-5", two_stretches("10:00"))).await.unwrap();
    let second = ask(&e, &call("claude-haiku-5-5", two_stretches("10:05"))).await.unwrap();
    assert_eq!((first.usage.cache_write_1h, second.usage.cache_read), (900, 900));
    assert!(second.usd < first.usd, "a cache read costs a twentieth of the write");
    let runs = runs.lock().unwrap().clone();
    assert_eq!(runs.iter().map(|r| (r.usage.cache_write(), r.usage.cache_read, r.usd)).collect::<Vec<_>>(), [(900, 0, first.usd), (0, 900, second.usd)], "both runs in the ledger");
    let bodies = got.lock().unwrap().clone();
    assert_eq!(bodies[0]["system"], bodies[1]["system"]);
    let content = |k: usize| bodies[k]["messages"][0]["content"].as_array().unwrap()[..2].to_vec();
    assert_eq!(content(0), content(1));
    assert_eq!(bodies[0]["max_tokens"], 128_000);
    // No key: no call.
    let none = Endpoint { key: None, ..e };
    assert!(ask(&none, &call("claude-haiku-5-5", "x".into())).await.is_err());
    assert_eq!(got.lock().unwrap().len(), 2);
}

/// Night review 24 Oct, finding 2: a connect that times out never reached Anthropic. Nothing was billed, so nothing is
/// counted, and the job retries it like any failed request. TEST-NET-1 (192.0.2.1) is never assigned: its SYN goes
/// unanswered (or the route is refused at once); the test build waits `CONNECT_TIMEOUT` (1 s) for it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_connect_that_times_out_is_free_and_retried() {
    let e = Endpoint { base: "http://192.0.2.1:9".into(), key: Some("sk-test".into()), workspace: None, timeout: Duration::from_secs(30), model: None, ..Default::default() };
    let runs: Arc<Mutex<Vec<Run>>> = Default::default();
    let seen = runs.clone();
    set_ledger(Arc::new(move |r: &Run| seen.lock().unwrap().push(r.clone())));
    let began = std::time::Instant::now();
    let err = ask(&e, &call("claude-haiku-5-5", "x".into())).await.unwrap_err();
    assert!(began.elapsed() < Duration::from_secs(10), "the connect deadline, not the run's");
    assert!(!settled(&err), "retried: {err:#}");
    assert!(err.to_string().starts_with("request failed"), "{err:#}");
    assert!(runs.lock().unwrap().is_empty(), "no paid run in the ledger");
}

// ---------------------------------------------------------------------------------------------------------
// The tool loop (`run_tools`), scripted: no network. A response is what the Messages API would return.

fn tool_use(id: &str, name: &str, input: Value) -> Value {
    json!({"type": "tool_use", "id": id, "name": name, "input": input})
}

fn reply(stop: &str, content: Vec<Value>, input: u64, output: u64) -> Value {
    json!({"stop_reason": stop, "content": content, "usage": {"input_tokens": input, "output_tokens": output}})
}

fn tools() -> Vec<Tool> {
    vec![
        Tool { name: "search", description: "look", schema: json!({"type": "object"}) },
        Tool { name: "submit", description: "answer", schema: json!({"type": "object"}) },
    ]
}

/// Runs `run_tools` against scripted responses, recording every body sent and every tool call made.
fn scripted(c: &Call, script: Vec<Value>, limit: f64, rounds: usize) -> (Result<Answer>, Vec<Value>, Vec<String>) {
    let mut bodies: Vec<Value> = vec![];
    let mut calls: Vec<String> = vec![];
    let mut next = script.into_iter();
    let mut send = |body: &Value, _worst: f64| -> Result<Value> {
        bodies.push(body.clone());
        next.next().ok_or_else(|| anyhow::anyhow!("no scripted reply"))
    };
    let mut run = |name: &str, input: &Value| -> String {
        calls.push(format!("{name} {}", input["words"].as_str().unwrap_or("")));
        format!("result for {name}")
    };
    let answer = run_tools(c, &tools(), "submit", rounds, limit, &mut run, &mut send);
    drop(send);
    (answer, bodies, calls)
}

#[test]
fn a_search_then_the_submitted_answer_is_one_run_with_its_results_fed_back() {
    let c = call("claude-haiku-5-5", Prompt::from("the talk"));
    let script = vec![
        reply("tool_use", vec![tool_use("t1", "search", json!({"words": "cad"}))], 900, 40),
        reply("tool_use", vec![tool_use("t2", "submit", json!({"suggest": false}))], 1200, 30),
    ];
    let (answer, bodies, calls) = scripted(&c, script, LIMIT_USD, 5);
    let answer = answer.unwrap();
    assert_eq!(answer.text, json!({"suggest": false}).to_string());
    assert_eq!(calls, vec!["search cad".to_owned()]);
    // Round two carries the first round's tool use and a tool_result for it.
    let second = &bodies[1]["messages"];
    assert_eq!(second.as_array().unwrap().len(), 3);
    assert_eq!(second[2]["content"][0]["type"], "tool_result");
    assert_eq!(second[2]["content"][0]["tool_use_id"], "t1");
    assert_eq!(second[2]["content"][0]["content"], "result for search");
    // The tools and the system prompt are the cached prefix; the answer is counted in full.
    assert_eq!(bodies[0]["tools"][0]["name"], "search");
    assert_eq!(bodies[0]["system"][0]["cache_control"]["ttl"], "1h");
    assert_eq!(answer.usage.input, 2100);
    assert_eq!(answer.usage.output, 70);
    assert!(answer.usd > 0.0 && answer.usd < LIMIT_USD);
}

#[test]
fn a_turn_that_ends_without_submitting_or_is_refused_stops_with_a_reason() {
    let c = call("claude-haiku-5-5", Prompt::from("the talk"));
    let (ended, _, _) = scripted(&c, vec![reply("end_turn", vec![json!({"type": "text", "text": "hm"})], 100, 5)], LIMIT_USD, 5);
    let err = ended.unwrap_err();
    assert!(settled(&err), "paid for, not resent: {err:#}");
    assert!(err.to_string().contains("without submitting"), "{err:#}");
    let (refused, _, _) = scripted(&c, vec![reply("refusal", vec![], 100, 5)], LIMIT_USD, 5);
    assert!(refused.unwrap_err().to_string().contains("declined"));
}

#[test]
fn no_answer_within_the_round_limit_stops_and_is_settled() {
    let c = call("claude-haiku-5-5", Prompt::from("the talk"));
    let looping = || reply("tool_use", vec![tool_use("t", "search", json!({"words": "x"}))], 100, 10);
    let (out, bodies, calls) = scripted(&c, vec![looping(), looping(), looping()], LIMIT_USD, 2);
    let err = out.unwrap_err();
    assert!(settled(&err) && err.to_string().contains("no answer within 2 rounds"), "{err:#}");
    assert_eq!(bodies.len(), 2, "two rounds, then stop");
    assert_eq!(calls.len(), 2);
}

#[test]
fn a_round_that_no_longer_fits_the_limit_is_skipped_before_it_is_sent() {
    let c = call("claude-haiku-5-5", Prompt::from("the talk"));
    // A limit this small leaves no output allowance: the first round is refused before any body goes out.
    let (out, bodies, _) = scripted(&c, vec![], 0.000_001, 5);
    assert!(out.is_err() && settled(&out.unwrap_err()));
    assert!(bodies.is_empty(), "nothing sent");
}

#[test]
fn the_tool_round_budget_is_the_model_maximum_unless_the_limit_needs_less() {
    let c = call("claude-haiku-5-5", Prompt::from("the talk"));
    let (_, bodies, _) = scripted(&c, vec![reply("tool_use", vec![tool_use("t", "submit", json!({"suggest": false}))], 10, 10)], LIMIT_USD, 5);
    assert_eq!(bodies[0]["max_tokens"], model("claude-haiku-5-5").max_output);
    let (_, lowered, _) = scripted(&c, vec![reply("tool_use", vec![tool_use("t", "submit", json!({}))], 10, 10)], 0.02, 5);
    let max = lowered[0]["max_tokens"].as_u64().unwrap();
    assert!(max < model("claude-haiku-5-5").max_output && max >= MIN_OUTPUT, "{max}");
}

// ---------------------------------------------------------------------------------------------------------
// Every request format against a local fake (deep search and the jobs on any provider).
// The fakes answer as OpenAI's and Anthropic's APIs document; no network, no model.

/// One fake server: `answer(n, body)` gives the n-th response (content type, body); every request's `authorization` /
/// `x-api-key` header and body are kept. Served on its own thread, so the blocking client can be driven from the test.
type Answerer = Arc<dyn Fn(usize, &Value) -> (&'static str, String) + Send + Sync>;
#[derive(Clone)]
struct Fake { got: Arc<Mutex<Vec<(String, Value)>>>, answer: Answerer }

fn fake(path: &'static str, answer: Answerer) -> (String, Arc<Mutex<Vec<(String, Value)>>>) {
    use axum::{extract::State, http::HeaderMap, routing::post, Router};
    async fn handle(State(f): State<Fake>, h: HeaderMap, body: String) -> axum::response::Response {
        let body: Value = serde_json::from_str(&body).unwrap();
        let auth = h.get("authorization").or(h.get("x-api-key")).map(|v| v.to_str().unwrap().to_owned()).unwrap_or_default();
        let n = { let mut g = f.got.lock().unwrap(); g.push((auth, body.clone())); g.len() };
        let (kind, text) = (f.answer)(n, &body);
        let status = if kind == "401" { axum::http::StatusCode::UNAUTHORIZED } else { axum::http::StatusCode::OK };
        let kind = if kind == "401" { "application/json" } else { kind };
        axum::response::Response::builder().status(status).header("content-type", kind).body(axum::body::Body::from(text)).unwrap()
    }
    let f = Fake { got: Default::default(), answer };
    let got = f.got.clone();
    let app = Router::new().route(path, post(handle)).with_state(f);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async move {
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            tx.send(format!("http://{}", l.local_addr().unwrap())).unwrap();
            axum::serve(l, app).await.unwrap();
        })
    });
    (rx.recv().unwrap(), got)
}

fn endpoint_for(wire: Wire, provider: &'static str, base: String) -> Endpoint {
    Endpoint { base, key: Some("sk-test-123456".into()), timeout: Duration::from_secs(10), model: Some("m".into()), wire, provider, reasoning: true, max_output: Some(64_000), ..Default::default() }
}

/// A Responses stream as OpenAI sends it: a few deltas, then `response.completed` with the whole response.
fn sse(response: Value) -> String {
    format!("event: response.created\ndata: {}\n\nevent: response.output_text.delta\ndata: {}\n\nevent: response.completed\ndata: {}\n\n",
        json!({"type": "response.created", "response": {"status": "in_progress"}}), json!({"type": "response.output_text.delta", "delta": "x"}),
        json!({"type": "response.completed", "response": response}))
}

/// Round one asks for a search, round two submits; per format.
fn tool_round(wire: Wire, n: usize) -> (&'static str, String) {
    let (name, args, id) = if n == 1 { ("search", json!({"words": "cad"}), "c1") } else { ("submit", json!({"found": "the CAD talk"}), "c2") };
    match wire {
        Wire::Anthropic => ("application/json", json!({"stop_reason": "tool_use", "content": [{"type": "tool_use", "id": id, "name": name, "input": args}],
            "usage": {"input_tokens": 100, "output_tokens": 10}}).to_string()),
        Wire::ChatCompletions => ("application/json", json!({"choices": [{"finish_reason": "tool_calls", "message": {"role": "assistant", "content": null,
            "tool_calls": [{"id": id, "type": "function", "function": {"name": name, "arguments": args.to_string()}}]}}],
            "usage": {"prompt_tokens": 100, "completion_tokens": 10, "prompt_tokens_details": {"cached_tokens": 60}}}).to_string()),
        Wire::Responses => ("text/event-stream", sse(json!({"status": "completed", "output": [
            {"type": "reasoning", "id": "rs_1", "summary": []},
            {"type": "function_call", "id": format!("fc_{n}"), "call_id": id, "name": name, "arguments": args.to_string()}],
            "usage": {"input_tokens": 100, "output_tokens": 10, "input_tokens_details": {"cached_tokens": 60}}}))),
    }
}

#[test]
fn every_format_runs_a_two_round_tool_loop_that_ends_in_submit() {
    for (wire, provider, path) in [(Wire::Anthropic, "opencode-go", "/v1/messages"), (Wire::ChatCompletions, "openai-compatible", "/chat/completions"), (Wire::Responses, "openai", "/responses")] {
        let (base, got) = fake(path, Arc::new(move |n, _| tool_round(wire, n)));
        let runs: Arc<Mutex<Vec<Run>>> = Default::default();
        let seen = runs.clone();
        set_ledger(Arc::new(move |r: &Run| seen.lock().unwrap().push(r.clone())));
        let mut ran = vec![];
        let answer = ask_tools_blocking(&endpoint_for(wire, provider, base), &call("m", "find the talk".into()), &tools(), "submit", 5,
            &mut |name, input| { ran.push(format!("{name} {}", input["words"].as_str().unwrap_or(""))); "13:02 User: the CAD model".into() }).unwrap();
        assert_eq!(answer.text, json!({"found": "the CAD talk"}).to_string(), "{wire:?}");
        assert_eq!((ran, answer.usd), (vec!["search cad".to_owned()], 0.0), "{wire:?}: off Anthropic, no dollars");
        let got = got.lock().unwrap().clone();
        assert_eq!(got.len(), 2, "{wire:?}");
        let (auth, first) = &got[0];
        let second = &got[1].1;
        match wire {
            Wire::Anthropic => {
                assert_eq!(auth, "sk-test-123456");
                assert_eq!(first["max_tokens"], 64_000, "unpriced Messages: the model's own maximum, not the $0.50 cut");
                assert_eq!(second["messages"][2]["content"][0]["tool_use_id"], "c1");
            }
            Wire::ChatCompletions => {
                assert_eq!(auth, "Bearer sk-test-123456");
                assert_eq!((first["messages"][0]["role"].as_str(), first["tools"][0]["function"]["name"].as_str(), first["reasoning_effort"].as_str()), (Some("system"), Some("search"), Some("high")));
                assert!(first.get("max_tokens").is_none());
                let m = second["messages"].as_array().unwrap();
                assert_eq!((m[2]["tool_calls"][0]["id"].as_str(), m[3]["role"].as_str(), m[3]["tool_call_id"].as_str(), m[3]["content"].as_str()),
                    (Some("c1"), Some("tool"), Some("c1"), Some("13:02 User: the CAD model")));
            }
            Wire::Responses => {
                assert_eq!(auth, "Bearer sk-test-123456");
                assert_eq!((first["instructions"].as_str(), first["store"].as_bool(), first["stream"].as_bool(), first["reasoning"]["effort"].as_str(), first["tools"][0]["name"].as_str()),
                    (Some("You sort things."), Some(false), Some(true), Some("high"), Some("search")));
                let input = second["input"].as_array().unwrap();
                assert_eq!(input.len(), 3, "the prompt, the call, its output; no reasoning item: {input:?}");
                assert!(input[1].get("id").is_none() && input[1]["call_id"] == "c1", "store: false: no item ids");
                assert_eq!((input[2]["type"].as_str(), input[2]["call_id"].as_str()), (Some("function_call_output"), Some("c1")));
            }
        }
        let runs = runs.lock().unwrap().clone();
        assert_eq!(runs.iter().map(|r| (r.outcome, r.usd)).collect::<Vec<_>>(), [("tool round", 0.0), ("answered", 0.0)], "{wire:?}: both rounds in the ledger");
        if wire != Wire::Anthropic { assert_eq!((runs[0].usage.input, runs[0].usage.cache_read), (40, 60), "cached prompt tokens are cache reads") }
    }
}

#[test]
fn every_format_gives_a_job_its_structured_answer() {
    let ok = json!({"names": ["Anna"]});
    for (wire, provider, path) in [(Wire::Anthropic, "opencode-go", "/v1/messages"), (Wire::ChatCompletions, "openai-compatible", "/chat/completions"), (Wire::Responses, "openai", "/responses")] {
        let text = ok.to_string();
        let (base, got) = fake(path, Arc::new(move |_, _| match wire {
            Wire::Anthropic => ("application/json", json!({"stop_reason": "end_turn", "content": [{"type": "text", "text": text}], "usage": {"input_tokens": 5, "output_tokens": 5}}).to_string()),
            // A local reasoning model: its thinking and a fence around the JSON.
            Wire::ChatCompletions => ("application/json", json!({"choices": [{"finish_reason": "stop", "message": {"content": format!("<think>hm</think>\n```json\n{text}\n```")}}],
                "usage": {"prompt_tokens": 5, "completion_tokens": 5}}).to_string()),
            Wire::Responses => ("text/event-stream", sse(json!({"status": "completed", "output": [{"type": "message", "content": [{"type": "output_text", "text": text}]}],
                "usage": {"input_tokens": 5, "output_tokens": 5}}))),
        }));
        let e = endpoint_for(wire, provider, base);
        let a = ask_blocking(&e, &call("m", "who spoke".into())).unwrap();
        assert_eq!((serde_json::from_str::<Value>(&a.text).unwrap(), a.usd), (ok.clone(), 0.0), "{wire:?}");
        let body = got.lock().unwrap()[0].1.clone();
        match wire {
            Wire::Anthropic => assert_eq!(body["output_config"]["format"]["schema"], json!({"type": "object"})),
            Wire::ChatCompletions => assert_eq!((body["response_format"]["type"].as_str(), body["response_format"]["json_schema"]["schema"].clone()), (Some("json_schema"), json!({"type": "object"}))),
            Wire::Responses => assert_eq!((body["text"]["format"]["type"].as_str(), body["text"]["format"]["schema"].clone()), (Some("json_schema"), json!({"type": "object"}))),
        }
    }
}

/// Agent off mid-run (the user's Agent switch): the next round is never sent and nothing is answered. The mechanism is the
/// gate every POST passes (`background_gate::check` in `post`), so a tool loop stops at its next round with `AgentOff`.
#[test]
fn agent_off_mid_run_stops_the_loop_before_the_next_round() {
    use agent::delivery::{DeliveryGate, StopReason};
    let (base, got) = fake("/chat/completions", Arc::new(|n, _| tool_round(Wire::ChatCompletions, n)));
    let gate = Arc::new(DeliveryGate::new(true));
    crate::background_gate::set(gate.clone());
    let g = gate.clone();
    let err = ask_tools_blocking(&endpoint_for(Wire::ChatCompletions, "openai-compatible", base), &call("m", "x".into()), &tools(), "submit", 5,
        &mut |_, _| { g.intent(StopReason::Agent, false); "result".into() }).unwrap_err();
    assert!(err.downcast_ref::<crate::background_gate::AgentOff>().is_some(), "{err:#}");
    assert_eq!(got.lock().unwrap().len(), 1, "round two never went out");
}

#[test]
fn errors_never_carry_the_key_and_a_job_without_a_credential_makes_no_call() {
    let (base, got) = fake("/chat/completions", Arc::new(|_, _| ("401", json!({"error": {"message": "Incorrect API key provided: sk-test-123456"}}).to_string())));
    let e = endpoint_for(Wire::ChatCompletions, "openai-compatible", base);
    let err = ask_blocking(&e, &call("m", "x".into())).unwrap_err().to_string();
    assert!(err.starts_with("OpenAI-Compatible answered 401") && !err.contains("sk-test-123456"), "{err}");
    let none = Endpoint { key: None, ..e };
    assert!(ask_blocking(&none, &call("m", "x".into())).is_err() && ask_tools_blocking(&none, &call("m", "x".into()), &tools(), "submit", 2, &mut |_, _| String::new()).is_err());
    assert_eq!(got.lock().unwrap().len(), 1, "no credential: no request");
}

#[test]
fn a_call_with_broken_json_arguments_is_answered_with_the_error_and_tried_again() {
    let (base, _) = fake("/chat/completions", Arc::new(|n, _| if n == 1 {
        ("application/json", json!({"choices": [{"finish_reason": "stop", "message": {"tool_calls": [{"function": {"name": "submit", "arguments": "{\"found\": "}}]}}]}).to_string())
    } else { tool_round(Wire::ChatCompletions, 2) }));
    let a = ask_tools_blocking(&endpoint_for(Wire::ChatCompletions, "openai-compatible", base), &call("m", "x".into()), &tools(), "submit", 3, &mut |_, _| unreachable!()).unwrap();
    assert_eq!(a.text, json!({"found": "the CAD talk"}).to_string());
}

/// Opt-in, real: Sign in with ChatGPT's Responses API with a token from the environment (never the Keychain):
/// `HEIMDALL_CHATGPT_ACCESS_TOKEN=… cargo test -p app --bin heimdall chatgpt_answers_for_real -- --ignored`.
/// `HEIMDALL_CHATGPT_MODEL` picks the model (default `agent::chatgpt::TEST_MODEL`). Uses the plan's usage.
#[test]
#[ignore]
fn chatgpt_answers_for_real_and_runs_a_tool_loop() {
    let Ok(token) = std::env::var("HEIMDALL_CHATGPT_ACCESS_TOKEN") else { panic!("set HEIMDALL_CHATGPT_ACCESS_TOKEN") };
    let model = std::env::var("HEIMDALL_CHATGPT_MODEL").unwrap_or_else(|_| agent::chatgpt::TEST_MODEL.into());
    let e = Endpoint { base: agent::chatgpt::API_BASE.into(), key: Some(token), timeout: Duration::from_secs(120), model: Some(model.clone()), wire: Wire::Responses, provider: agent::provider::CHATGPT, ..Default::default() };
    real_smoke(&e, &model);
}

/// Opt-in, real: any OpenAI-compatible server already running on this Mac (no key):
/// `HEIMDALL_LOCAL_OPENAI_BASE=http://127.0.0.1:8100/v1 HEIMDALL_LOCAL_OPENAI_MODEL=… cargo test … local_engine_answers_for_real -- --ignored`.
#[test]
#[ignore]
fn local_engine_answers_for_real_and_runs_a_tool_loop() {
    let base = std::env::var("HEIMDALL_LOCAL_OPENAI_BASE").expect("set HEIMDALL_LOCAL_OPENAI_BASE");
    let model = std::env::var("HEIMDALL_LOCAL_OPENAI_MODEL").expect("set HEIMDALL_LOCAL_OPENAI_MODEL");
    let e = Endpoint { base, key: Some(agent::provider::NO_KEY.into()), timeout: Duration::from_secs(300), model: Some(model.clone()), wire: Wire::ChatCompletions, provider: agent::provider::CUSTOM, ..Default::default() };
    real_smoke(&e, &model);
}

fn real_smoke(e: &Endpoint, model: &str) {
    let c = Call { job: "smoke", model: model.into(), system: "Answer in JSON.", effort: "low", prompt: "Give {\"ok\": true}.".into(),
                   schema: json!({"type": "object", "properties": {"ok": {"type": "boolean"}}, "required": ["ok"], "additionalProperties": false}), history: Vec::new(), session: None };
    let a = ask_blocking(e, &c).unwrap();
    assert_eq!(serde_json::from_str::<Value>(&a.text).unwrap()["ok"], true, "{}", a.text);
    let tools = vec![
        Tool { name: "lookup", description: "Returns the secret word.", schema: json!({"type": "object", "properties": {}}) },
        Tool { name: "submit", description: "Submit the final answer.", schema: json!({"type": "object", "properties": {"word": {"type": "string"}}, "required": ["word"]}) },
    ];
    let c = Call { system: "Call lookup once, then submit the word it returns.", prompt: "What is the secret word?".into(), ..c };
    let a = ask_tools_blocking(e, &c, &tools, "submit", 4, &mut |name, _| if name == "lookup" { "heimdall".into() } else { "unknown tool".into() }).unwrap();
    assert!(a.text.to_lowercase().contains("heimdall"), "{}", a.text);
}

/// 27 Oct 03:08 and 03:12: on the ChatGPT route the background jobs spent output tokens and read "no text": with
/// `store: false` the answer came as `response.output_item.done` and `response.completed` carried an empty `output`.
#[test]
fn a_responses_stream_whose_completed_output_is_empty_is_read_from_its_items() {
    let message = json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "{\"ok\":true}"}]});
    let call = json!({"type": "function_call", "call_id": "c1", "name": "lookup", "arguments": "{}"});
    let stream = format!("event: response.output_item.done\ndata: {}\n\nevent: response.output_item.done\ndata: {}\n\nevent: response.completed\ndata: {}\n\n",
        json!({"type": "response.output_item.done", "item": message}), json!({"type": "response.output_item.done", "item": call}),
        json!({"type": "response.completed", "response": {"status": "completed", "output": [], "usage": {"input_tokens": 10, "output_tokens": 571}}}));
    let r = crate::background_call::wire::responses_final(&stream).unwrap();
    assert_eq!(r["output"], json!([message, call]));
    // A completed response that carries its output keeps it.
    let full = sse(json!({"status": "completed", "output": [message], "usage": {}}));
    assert_eq!(crate::background_call::wire::responses_final(&full).unwrap()["output"], json!([message]));
}

/// The ChatGPT sign-in's stream as the live probe saw it (27 Oct, gpt-6-luna): each item only in its
/// `response.output_item.done`, here out of order, and `response.completed` with `"output": []` beside the usage.
fn sse_items(items: Vec<Value>, usage: Value) -> String {
    let mut s = String::new();
    for (i, item) in items.iter().enumerate().rev() {
        s.push_str(&format!("event: response.output_item.done\ndata: {}\n\n", json!({"type": "response.output_item.done", "output_index": i, "item": item})));
    }
    s.push_str(&format!("event: response.completed\ndata: {}\n\n", json!({"type": "response.completed", "response": {"status": "completed", "output": [], "usage": usage}})));
    s
}

/// Audit 27 Oct, F3 (the part 87ddc739 left open): a background tool job on the ChatGPT route, whose calls arrive only
/// as items and out of order, runs both rounds, and the reasoning item never goes back.
#[test]
fn a_tool_job_on_the_chatgpt_route_reads_its_calls_from_the_items_in_order() {
    let usage = json!({"input_tokens": 100, "output_tokens": 10, "input_tokens_details": {"cached_tokens": 60}});
    let (base, got) = fake("/responses", Arc::new(move |n, _| {
        let (name, args, id) = if n == 1 { ("search", json!({"words": "cad"}), "c1") } else { ("submit", json!({"found": "the CAD talk"}), "c2") };
        ("text/event-stream", sse_items(vec![json!({"type": "reasoning", "id": "rs_1", "summary": []}),
            json!({"type": "function_call", "id": format!("fc_{n}"), "call_id": id, "name": name, "arguments": args.to_string()})], usage.clone()))
    }));
    let a = ask_tools_blocking(&endpoint_for(Wire::Responses, "openai", base), &call("gpt-6-luna", "find the talk".into()), &tools(), "submit", 5,
        &mut |_, _| "13:02 User: the CAD model".into()).unwrap();
    assert_eq!(a.text, json!({"found": "the CAD talk"}).to_string());
    let got = got.lock().unwrap();
    let input = got[1].1["input"].as_array().unwrap();
    assert_eq!((input.len(), input[1]["call_id"].as_str(), input[2]["type"].as_str()), (3, Some("c1"), Some("function_call_output")));
    // Two calls in one round go back in the response's order, whatever order their events came in.
    let two = sse_items(vec![json!({"type": "function_call", "call_id": "a"}), json!({"type": "function_call", "call_id": "b"})], json!({}));
    let r = wire::responses_final(&two).unwrap();
    assert_eq!((r["output"][0]["call_id"].as_str(), r["output"][1]["call_id"].as_str()), (Some("a"), Some("b")));
}

/// Audit 27 Oct, F9: background Responses requests carry a prompt-cache key (Pi sends its session id; the jobs sent none
/// and read 0 to 8,448 of 88-92K prompt tokens from cache). One key per job and stable prefix; the other formats send none.
#[test]
fn responses_requests_carry_one_cache_key_per_job_and_prefix() {
    let l = Loop::for_endpoint(&endpoint_for(Wire::Responses, "openai", String::new()));
    let job = call("gpt-6-luna", two_stretches("12:00"));
    let key = wire::answer_body(&l, &job, 0)["prompt_cache_key"].as_str().unwrap().to_owned();
    assert_eq!(key, format!("heimdall-test-job-{:016x}", prompt_hash(&job)));
    assert!(key.len() <= 64);
    assert_eq!(wire::answer_body(&l, &call("gpt-6-luna", two_stretches("12:05")), 0)["prompt_cache_key"], key.as_str(), "the volatile rest does not change it");
    assert_eq!(wire::body(&l, &job, &tools(), &[], 0)["prompt_cache_key"], key.as_str(), "tool rounds too");
    let other = Call { system: "Another system prompt.", ..job.clone() };
    assert_ne!(wire::answer_body(&l, &other, 0)["prompt_cache_key"], key.as_str());
    for w in [Wire::Anthropic, Wire::ChatCompletions] {
        let l = Loop::for_endpoint(&endpoint_for(w, "openai-compatible", String::new()));
        assert!(wire::answer_body(&l, &job, 1000).get("prompt_cache_key").is_none(), "{w:?}");
    }
}

/// Luna cache (27 Oct): OpenAI's prompt counts include the cached part and its output counts include reasoning; the
/// ledger keeps cached reads apart from uncached input and never adds reasoning to the output again.
#[test]
fn openai_usage_splits_cached_reads_and_counts_reasoning_once() {
    let responses = json!({"status": "completed", "output": [], "usage": {"input_tokens": 90_155, "input_tokens_details": {"cached_tokens": 8448},
        "output_tokens": 6539, "output_tokens_details": {"reasoning_tokens": 5900}}});
    let (_, _, u) = wire::read_answer(Wire::Responses, &responses);
    assert_eq!((u.input, u.cache_read, u.output, u.cache_write(), u.prompt()), (81_707, 8448, 6539, 0, 90_155));
    let chat = json!({"choices": [{"finish_reason": "stop", "message": {"content": "{}"}}], "usage": {"prompt_tokens": 3000,
        "prompt_tokens_details": {"cached_tokens": 2048}, "completion_tokens": 500, "completion_tokens_details": {"reasoning_tokens": 400}}});
    let (_, _, u) = wire::read_answer(Wire::ChatCompletions, &chat);
    assert_eq!((u.input, u.cache_read, u.output), (952, 2048, 500));
}

/// Audit 27 Oct, F11: a job's effort follows the model's own list (ChatGPT's Sol takes up to "ultra", Luna "max"), not
/// a cap at "high"; without a list the old mapping stays.
#[test]
fn a_jobs_effort_follows_the_models_listed_levels() {
    let effort = |efforts: &[&str], job: &'static str| {
        let e = Endpoint { efforts: efforts.iter().map(|s| s.to_string()).collect(), ..endpoint_for(Wire::Responses, "openai", String::new()) };
        wire::answer_body(&Loop::for_endpoint(&e), &Call { effort: job, ..call("m", "x".into()) }, 0)["reasoning"]["effort"].as_str().unwrap().to_owned()
    };
    let luna = ["low", "medium", "high", "xhigh", "max"];
    assert_eq!(effort(&luna, "max"), "max");
    assert_eq!(effort(&luna, "high"), "high");
    assert_eq!(effort(&luna, "minimal"), "low", "up to the least the model takes");
    assert_eq!(effort(&["low", "medium", "high"], "max"), "high", "down when nothing is above");
    assert_eq!(effort(&[], "max"), "high", "no list: the old mapping");
    assert_eq!(effort(&[], "low"), "low");
    // The catalog entry's list reaches the endpoint (`background_model::resolve`).
    assert_eq!(wire::effort_mask(&["max".into(), "ultra".into(), "bogus".into()]), 0b1100_0000);
}

/// Audit 27 Oct, F8: the ChatGPT route answers a spent plan with HTTP 200 and the limit inside the stream. The job's
/// next pass makes no request until the limit is back (a success on the provider, or half an hour for one try).
#[test]
fn a_spent_plan_stops_the_next_background_request_until_it_is_back() {
    let limit = format!("data: {}\n\n", json!({"type": "error", "message": "The ChatGPT user has reached their Subscription Sharing usage limit. Ask the user to try again after their usage limit resets."}));
    let (base, got) = fake("/responses", Arc::new(move |_, _| ("text/event-stream", limit.clone())));
    let e = endpoint_for(Wire::Responses, "openai", base);
    assert!(ask_blocking(&e, &call("gpt-6-luna", "x".into())).unwrap_err().to_string().contains("usage limit"));
    let skipped = ask_blocking(&e, &call("gpt-6-luna", "x".into())).unwrap_err();
    assert!(skipped.downcast_ref::<crate::background_gate::QuotaOut>().is_some(), "{skipped:#}");
    assert!(ask_tools_blocking(&e, &call("gpt-6-luna", "x".into()), &tools(), "submit", 3, &mut |_, _| String::new()).is_err());
    assert_eq!(got.lock().unwrap().len(), 1, "one request: the passes after it never went out");
    // The agent answered on the provider: the jobs run again.
    crate::background_gate::quota_back("openai");
    let _ = ask_blocking(&e, &call("gpt-6-luna", "x".into()));
    assert_eq!(got.lock().unwrap().len(), 2);
    crate::background_gate::quota_back("openai");
}

// ---------------------------------------------------------------------------------------------------------
// Rolling sessions (Goals only): every other job's requests stay byte-identical.

/// FNV-1a 64 of a request's text.
fn fnv(s: &str) -> u64 { s.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| (h ^ b as u64).wrapping_mul(0x0000_0100_0000_01b3)) }

/// The day timeline's run as that job builds it: two stable stretches, then the clock.
fn day_timeline_call() -> Call<'static> {
    let mut p = Prompt::default();
    p.stable("<yesterday>User trained the decider</yesterday>\n").stable("<lines>\n09:00 User: morning\n09:05 Heimdall: hi\n</lines>\n").push("Now: 10:00\n\nUpdate the timeline.");
    crate::day_timeline::call("claude-haiku-5-5", p)
}

/// Every format's request for the fixture, as fingerprints (Messages priced and unpriced, chat completions, Responses,
/// the T3 message).
fn fingerprints(c: &Call) -> [u64; 5] {
    let l = |wire| Loop { wire, limit: None, max_output: None, reasoning: true, efforts: 0 };
    [fnv(&prepare(c).unwrap().body.to_string()), fnv(&wire::answer_body(&l(Wire::Anthropic), c, 32_000).to_string()),
     fnv(&wire::answer_body(&l(Wire::ChatCompletions), c, 0).to_string()), fnv(&wire::answer_body(&l(Wire::Responses), c, 0).to_string()),
     fnv(&t3::message(c))]
}

/// The fingerprints were taken from the code before rolling sessions existed (31 Oct): a job without history or session
/// sends the very same bytes on every format. (The day timeline's own prompt changing moves its row: re-take it then;
/// re-taken 4 Nov for the public edition's rewritten prompt.)
#[test]
fn a_job_without_history_sends_the_same_bytes_as_before_rolling_sessions() {
    let dt = day_timeline_call();
    assert!(dt.history.is_empty() && dt.session.is_none());
    assert_eq!(fingerprints(&dt), [0x90c65402e578a6a9, 0x663eb387fcd7860b, 0x70c50e60f85040ab, 0x7012364b4c4d2b55, 0x0e350c1749348af3]);
    assert_eq!(fingerprints(&call("claude-haiku-5-5", two_stretches("10:00"))), [0x0db81918bb49497e, 0x23545aa84a70536c, 0x6d035a50b5c5a2fa, 0x85c808c3e7e16307, 0x02a29b28fea48cf1]);
}

/// A Goals-like rolling call: two earlier turns, then the new one (its stable new lines, then the clock).
fn rolling(session: Option<&str>) -> Call<'static> {
    let turn = |lines: &str, answer: &str| {
        let mut user = Prompt::default();
        user.stable(lines.to_owned()).push("Goals now: g1\nNow: 09:00");
        Turn { user, assistant: answer.into() }
    };
    let mut p = Prompt::default();
    p.stable("<lines>\n10:01 User: the film is done\n</lines>\n").push("Goals now: g1\nNow: 10:02");
    Call { history: vec![turn("<lines>\n09:00 User: morning\n</lines>\n", "{\"ops\":[]}"), turn("<lines>\n09:30 User: cutting the film\n</lines>\n", "{\"ops\":[1]}")],
           session: session.map(str::to_owned), ..call("claude-haiku-5-5", p) }
}

fn marked(content: &Value) -> Vec<usize> {
    content.as_array().unwrap().iter().enumerate().filter(|(_, b)| b.get("cache_control").is_some()).map(|(i, _)| i).collect()
}

#[test]
fn a_rolling_call_sends_its_turns_in_order_with_three_breakpoints_where_the_cache_is() {
    let c = rolling(Some("goals-s1"));
    let b = prepare(&c).unwrap().body;
    let m = b["messages"].as_array().unwrap();
    let roles: Vec<&str> = m.iter().map(|x| x["role"].as_str().unwrap()).collect();
    assert_eq!(roles, ["user", "assistant", "user", "assistant", "user"]);
    // The earlier turns as they were sent and answered, unmarked but for the last answer's end.
    assert_eq!(m[0]["content"], json!([{"type": "text", "text": "<lines>\n09:00 User: morning\n</lines>\n"}, {"type": "text", "text": "Goals now: g1\nNow: 09:00"}]));
    assert_eq!(m[1]["content"], json!([{"type": "text", "text": "{\"ops\":[]}"}]));
    assert_eq!((marked(&m[0]["content"]), marked(&m[1]["content"]), marked(&m[2]["content"])), (vec![], vec![], vec![]));
    assert_eq!(m[3]["content"][0]["cache_control"], json!({"type": "ephemeral", "ttl": "1h"}));
    assert_eq!(marked(&m[4]["content"]), [0], "the new turn's last stable stretch; the clock after it");
    assert!(m[4]["content"][1]["text"].as_str().unwrap().contains("Now: 10:02"));
    let total = 1 + m.iter().map(|x| marked(&x["content"]).len()).sum::<usize>();
    assert!(total <= 4 && total == 3, "{total}");
    // The new turn with many stable stretches still keeps one: never past the API's four.
    let mut p = Prompt::default();
    for k in 0..6 { p.stable(format!("chunk {k}\n")); }
    let many = Call { prompt: p, ..rolling(None) };
    let mb = body(&many, 100);
    assert_eq!(marked(&mb["messages"][4]["content"]), [5]);
    // Next call: this turn joins the history, append-only; everything before the new turn is the same bytes.
    let mut next = rolling(Some("goals-s1"));
    next.history.push(Turn { user: c.prompt.clone(), assistant: "{\"ops\":[2]}".into() });
    next.prompt = "Now: 10:10".into();
    let nb = prepare(&next).unwrap().body;
    let strip = |v: &Value| { let mut v = v.clone(); for b in v["content"].as_array_mut().unwrap() { b.as_object_mut().unwrap().remove("cache_control"); } v };
    for i in 0..5 { assert_eq!(strip(&nb["messages"][i]), strip(&m[i]), "message {i} moved"); }
    assert_eq!((nb["system"].clone(), nb["output_config"].clone()), (b["system"].clone(), b["output_config"].clone()));
}

#[test]
fn a_rolling_call_prices_its_history_as_cached_prefix_and_keeps_the_limit() {
    let c = rolling(None);
    let hist = c.history.iter().map(|t| t.user.len() + t.assistant.len()).sum::<usize>();
    let alone = Call { history: vec![], ..c.clone() };
    let (b, a) = (budget(&c).unwrap(), budget(&alone).unwrap());
    assert_eq!(b.prompt_tokens, estimate_tokens(c.system.len() + c.schema.to_string().len() + hist + c.prompt.cached_len()) + estimate_tokens(c.prompt.len() - c.prompt.cached_len()));
    assert!(b.prompt_tokens > a.prompt_tokens && b.worst_usd > a.worst_usd);
    // A history too long for $0.50 is skipped, never sent (Haiku 5.5's dearer tier: $1/M one-hour writes).
    let mut big = rolling(None);
    big.history[0].user = "x".repeat(1_400_000).into();
    let err = budget(&big).unwrap_err();
    assert!(err.input_usd > 0.5 - MIN_OUTPUT as f64 * 2.5 / 1e6, "{err}");
}

#[test]
fn a_rolling_call_on_responses_and_chat_sends_the_turns_in_order_with_the_session_as_cache_key() {
    let c = rolling(Some("goals-S1/a"));
    let l = |wire| Loop { wire, limit: None, max_output: None, reasoning: true, efforts: 0 };
    let r = wire::answer_body(&l(Wire::Responses), &c, 0);
    let items: Vec<(String, String)> = r["input"].as_array().unwrap().iter().map(|i| (i["role"].as_str().unwrap().to_owned(), i["content"].as_str().unwrap().to_owned())).collect();
    let want: Vec<(String, String)> = [("user", "<lines>\n09:00 User: morning\n</lines>\n"), ("user", "Goals now: g1\nNow: 09:00"), ("assistant", "{\"ops\":[]}"),
        ("user", "<lines>\n09:30 User: cutting the film\n</lines>\n"), ("user", "Goals now: g1\nNow: 09:00"), ("assistant", "{\"ops\":[1]}"),
        ("user", "<lines>\n10:01 User: the film is done\n</lines>\n"), ("user", "Goals now: g1\nNow: 10:02")].iter().map(|(a, b)| (a.to_string(), b.to_string())).collect();
    assert_eq!(items, want);
    assert_eq!(r["prompt_cache_key"], "heimdall-goals-s1-a", "the session's, so routing finds the segment's cache");
    assert_eq!(r["instructions"], "You sort things.");
    // Without a session the key stays the job's and prompt's.
    assert_eq!(wire::cache_key(&rolling(None)), wire::cache_key(&call("claude-haiku-5-5", "x".into())));
    assert!(wire::cache_key(&Call { session: Some("s".repeat(200)), ..rolling(None) }).len() <= 64);
    let ch = wire::answer_body(&l(Wire::ChatCompletions), &c, 0);
    let msgs: Vec<(String, String)> = ch["messages"].as_array().unwrap().iter().map(|i| (i["role"].as_str().unwrap().to_owned(), i["content"].as_str().unwrap().to_owned())).collect();
    assert_eq!(msgs, [("system", "You sort things."), ("user", "<lines>\n09:00 User: morning\n</lines>\nGoals now: g1\nNow: 09:00"), ("assistant", "{\"ops\":[]}"),
        ("user", "<lines>\n09:30 User: cutting the film\n</lines>\nGoals now: g1\nNow: 09:00"), ("assistant", "{\"ops\":[1]}"),
        ("user", "<lines>\n10:01 User: the film is done\n</lines>\nGoals now: g1\nNow: 10:02")].map(|(a, b)| (a.to_owned(), b.to_owned())));
    // An unpriced Messages provider (OpenCode Go's Claude) gets the same turns as Anthropic.
    assert_eq!(wire::answer_body(&l(Wire::Anthropic), &c, 100)["messages"], body(&c, 100)["messages"]);
}

/// Over the wire (`post`, whose session headers are the body's cache key): a scripted Responses server gets every turn
/// and the session's key.
#[test]
fn a_rolling_call_over_the_wire_carries_every_turn_and_the_session_key() {
    let (base, seen) = fake("/responses", Arc::new(|_, _| ("text/event-stream", sse(json!({"status": "completed", "output": [{"type": "message", "content": [{"type": "output_text", "text": "{}"}]}], "usage": {"input_tokens": 10, "output_tokens": 2}})))));
    let e = endpoint_for(Wire::Responses, agent::provider::CHATGPT, base);
    ask_blocking(&e, &rolling(Some("goals-s1"))).unwrap();
    let (_, body) = seen.lock().unwrap()[0].clone();
    assert_eq!(body["prompt_cache_key"], "heimdall-goals-s1");
    assert_eq!(body["input"].as_array().unwrap().len(), 8);
}
