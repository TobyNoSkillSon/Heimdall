//! The three request formats a tool loop speaks (deep search runs on Background Tasks ▸
//! Model whatever provider it is, "no provider-specific code paths in deep search"). `background_call::run_tools` is
//! one loop; everything that differs between the formats is here, so no caller ever sees which one ran:
//! - Anthropic Messages: Anthropic, and OpenCode Go's Claude models. The body is `background_call::tool_body`.
//! - OpenAI chat completions: OpenRouter, OpenCode Go's other models, Custom (any OpenAI-compatible URL), Own Engine
//!   and the built-in engine. Only the fields every compatible server takes: no `max_tokens` (a local server's
//!   default is the rest of its context), no effort, no cache markers (their prefix caching is automatic, and the
//!   stable part comes first as `Prompt::text` keeps it).
//! - OpenAI Responses: Sign in with ChatGPT (and any model whose catalog names this format). Streamed, as the ChatGPT
//!   plan's own check sends it (`agent::chatgpt::probe`), with `store: false`; the earlier rounds' calls are sent back
//!   without their item ids (nothing is stored to refer to) and reasoning items are dropped.
//!
//! A tool call's id, name and input come out the same from all three (`ToolCall`), and so do the stop (`Stop`) and the
//! token counts (`Usage`: cached prompt tokens as cache reads, the rest as input).

use super::{tool_body, Call, Loop, Tool, Usage};
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// A request format.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Wire {
    /// `POST {base}/v1/messages`, `x-api-key`.
    #[default]
    Anthropic,
    /// `POST {base}/chat/completions`, `Authorization: Bearer`.
    ChatCompletions,
    /// `POST {base}/responses`, `Authorization: Bearer`, streamed.
    Responses,
}

/// One tool call, whatever the format. `bad`: the arguments were not JSON (an OpenAI format sends them as a string).
#[derive(Clone, Debug, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub input: Value,
    pub bad: Option<String>,
}

/// Why a turn ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stop {
    ToolUse,
    Refusal,
    MaxTokens,
    End,
}

/// One response, read.
#[derive(Clone, Debug, PartialEq)]
pub struct Round {
    pub calls: Vec<ToolCall>,
    pub stop: Stop,
    pub usage: Usage,
}

/// The tools as the format lists them.
pub fn tools_json(w: Wire, tools: &[Tool]) -> Value {
    Value::Array(tools.iter().map(|t| match w {
        Wire::Anthropic => json!({"name": t.name, "description": t.description, "input_schema": t.schema}),
        Wire::ChatCompletions => json!({"type": "function", "function": {"name": t.name, "description": t.description, "parameters": t.schema}}),
        Wire::Responses => json!({"type": "function", "name": t.name, "description": t.description, "parameters": t.schema, "strict": false}),
    }).collect())
}

/// The conversation's first turn: the user message (Anthropic: its blocks with their cache marks; the system prompt
/// rides in the body for Anthropic and Responses, as the first message for chat completions). A rolling session's
/// earlier turns (`Call::history`) come first, oldest first, each its user message then the answer's text, in the
/// same order every call, so the provider's automatic prefix cache reuses them; without history, the one message.
pub fn first_turn(w: Wire, c: &Call) -> Vec<Value> {
    match w {
        Wire::Anthropic => super::messages(c),
        Wire::ChatCompletions => {
            let mut m = vec![json!({"role": "system", "content": c.system})];
            for t in &c.history {
                m.push(json!({"role": "user", "content": t.user.text()}));
                m.push(json!({"role": "assistant", "content": t.assistant}));
            }
            m.push(json!({"role": "user", "content": c.prompt.text()}));
            m
        }
        Wire::Responses => {
            let mut m = Vec::new();
            for t in &c.history {
                m.extend(responses_turns(&t.user));
                m.push(json!({"role": "assistant", "content": t.assistant}));
            }
            m.extend(responses_turns(&c.prompt));
            m
        }
    }
}

/// The Responses user turn: one message per stretch of the prompt, the same text in the same order. GPT-5.6 and later
/// look a cached prefix up only at message ends (the latest and up to 20 earlier; OpenAI's prompt-caching guide), so
/// one message holding the conversation and the clock had one lookup point, and it moved every pass: Luna's plan
/// passes read 0 or 8,448 tokens (the instructions) of byte-identical prefixes (27 Oct, the user: "we have to cache Luna
/// prompts"). With each stable stretch its own message, the newest chunk a pass shares with the last one ends a
/// message. No breakpoint fields: Sign in with ChatGPT rejects the cache options (Pi 1.4.0).
fn responses_turns(p: &super::Prompt) -> Vec<Value> {
    if p.blocks.is_empty() { return vec![json!({"role": "user", "content": ""})] }
    p.blocks.iter().map(|b| json!({"role": "user", "content": b.text})).collect()
}

/// Reasoning efforts, least first, in OpenAI's names (the ChatGPT sign-in's list goes on to "max" and "ultra").
const EFFORT_ORDER: [&str; 8] = ["none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra"];

/// The efforts a model's catalog entry lists (`agent::provider::ApiModel::efforts`) as `Loop::efforts`; unknown names
/// are left out.
pub fn effort_mask(efforts: &[String]) -> u16 {
    efforts.iter().filter_map(|e| EFFORT_ORDER.iter().position(|o| o == e)).fold(0, |m, i| m | 1 << i)
}

/// The effort a request asks for. With the model's own list (audit 27 Oct, F11: ChatGPT's Sol, Terra and Luna take
/// "max", which every job's "max" was clipped to "high" before): the job's effort, else the nearest the model lists
/// above it, else below. Without one: "minimal" to "high" as they are, anything more as "high", the most every
/// Responses or chat-completions model takes.
fn responses_effort(l: &Loop, effort: &str) -> &'static str {
    let at = EFFORT_ORDER.iter().position(|e| *e == effort);
    if l.efforts == 0 {
        return match at { Some(i @ 1..=4) => EFFORT_ORDER[i], _ => "high" };
    }
    let at = at.unwrap_or(4);
    (at..EFFORT_ORDER.len()).chain((0..at).rev()).find(|i| l.efforts & (1 << i) != 0).map_or("high", |i| EFFORT_ORDER[i])
}

/// The job's effort on a chat-completions request, when the model reasons (`Loop::reasoning`).
fn chat_effort(l: &Loop, c: &Call, mut b: Value) -> Value {
    if l.reasoning { b["reasoning_effort"] = json!(responses_effort(l, c.effort)) }
    b
}

/// `reasoning.effort` on a Responses body, only for a model that reasons (a non-reasoning model rejects it), and the
/// prompt-cache key (audit 27 Oct, F9: the jobs' 88-92K-token prompts read 0 to 8,448 tokens from cache, against 97.9%
/// for the agent on the same route, which sends Pi's session id). The key names the job and its stable prefix
/// (`prompt_hash`: model, effort, schema, system), so each job's runs share one cache and a changed prompt starts anew.
fn responses_effort_field(l: &Loop, c: &Call, mut body: Value) -> Value {
    if l.reasoning { body["reasoning"] = json!({"effort": responses_effort(l, c.effort)}); }
    body["prompt_cache_key"] = json!(cache_key(c));
    body
}

/// `heimdall-<job>-<prompt hash>`, at most 64 characters of [a-z0-9-]. A rolling session's is `heimdall-<session>`
/// (`Call::session`), so every turn of a segment routes to the cache that holds its prefix.
pub fn cache_key(c: &Call) -> String {
    let clean = |s: &str, n: usize| -> String { s.chars().map(|ch| if ch.is_ascii_alphanumeric() { ch.to_ascii_lowercase() } else { '-' }).take(n).collect() };
    if let Some(s) = c.session.as_deref().filter(|s| !s.is_empty()) { return format!("heimdall-{}", clean(s, 55)) }
    format!("heimdall-{}-{:016x}", clean(c.job, 36), super::prompt_hash(c))
}

/// One round's body. `max_tokens` is sent on the Messages format only (it requires one).
pub fn body(l: &Loop, c: &Call, tools: &[Tool], history: &[Value], max_tokens: u64) -> Value {
    match l.wire {
        Wire::Anthropic => tool_body(c, tools, history, max_tokens),
        Wire::ChatCompletions => chat_effort(l, c, json!({"model": c.model, "messages": history, "tools": tools_json(l.wire, tools), "stream": false})),
        Wire::Responses => responses_effort_field(l, c, json!({
            "model": c.model, "instructions": c.system, "input": history, "tools": tools_json(l.wire, tools),
            "store": false, "stream": true,
        })),
    }
}

/// A job's structured answer (`background_call::ask`) off Anthropic's prices: the job's schema as the format's JSON
/// schema output (chat completions: `response_format`; Responses: `text.format`), not strict (the jobs' schemas were
/// written for Messages and need not meet OpenAI's strict subset).
pub fn answer_body(l: &Loop, c: &Call, max_tokens: u64) -> Value {
    match l.wire {
        Wire::Anthropic => super::body(c, max_tokens),
        Wire::ChatCompletions => chat_effort(l, c, json!({
            "model": c.model, "messages": first_turn(l.wire, c), "stream": false,
            "response_format": {"type": "json_schema", "json_schema": {"name": "answer", "schema": c.schema}},
        })),
        Wire::Responses => responses_effort_field(l, c, json!({
            "model": c.model, "instructions": c.system, "input": first_turn(l.wire, c),
            "text": {"format": {"type": "json_schema", "name": "answer", "schema": c.schema, "strict": false}},
            "store": false, "stream": true,
        })),
    }
}

/// A structured answer's text, its stop and its usage. A local model's thinking (`…</think>`) and a Markdown fence
/// around the JSON are cut off.
pub fn read_answer(w: Wire, v: &Value) -> (String, Stop, Usage) {
    let (text, stop, usage) = match w {
        Wire::Anthropic => {
            let text: String = v["content"].as_array().into_iter().flatten().filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str()).collect();
            let stop = match v["stop_reason"].as_str() { Some("refusal") => Stop::Refusal, Some("max_tokens") => Stop::MaxTokens, _ => Stop::End };
            (text, stop, Usage::from_api(&v["usage"]))
        }
        Wire::ChatCompletions => {
            let choice = &v["choices"][0];
            let stop = match choice["finish_reason"].as_str() {
                Some("length") => Stop::MaxTokens,
                Some("content_filter") => Stop::Refusal,
                _ if choice["message"]["refusal"].as_str().is_some_and(|r| !r.is_empty()) => Stop::Refusal,
                _ => Stop::End,
            };
            (choice["message"]["content"].as_str().unwrap_or("").to_owned(), stop, openai_usage(&v["usage"], "prompt_tokens", "completion_tokens", "prompt_tokens_details"))
        }
        Wire::Responses => {
            let parts: Vec<Value> = v["output"].as_array().into_iter().flatten().filter(|i| i["type"] == "message").flat_map(|i| i["content"].as_array().cloned().unwrap_or_default()).collect();
            let text: String = parts.iter().filter(|c| c["type"] == "output_text").filter_map(|c| c["text"].as_str()).collect();
            let stop = match (v["status"].as_str(), v["incomplete_details"]["reason"].as_str()) {
                (Some("incomplete"), Some("max_output_tokens")) => Stop::MaxTokens,
                (Some("incomplete"), Some("content_filter")) => Stop::Refusal,
                _ if parts.iter().any(|c| c["type"] == "refusal") => Stop::Refusal,
                _ => Stop::End,
            };
            (text, stop, openai_usage(&v["usage"], "input_tokens", "output_tokens", "input_tokens_details"))
        }
    };
    (json_text(&text), stop, usage)
}

/// The JSON in a model's text: after a closing `</think>`, inside a ```json fence if there is one.
pub fn json_text(t: &str) -> String {
    let t = t.rsplit_once("</think>").map_or(t, |(_, rest)| rest).trim();
    let t = t.strip_prefix("```json").or_else(|| t.strip_prefix("```")).map_or(t, |r| r.trim_end().strip_suffix("```").unwrap_or(r));
    t.trim().to_owned()
}

/// An OpenAI-format call's arguments: a JSON string (some servers send the object itself).
fn arguments(raw: &Value) -> (Value, Option<String>) {
    match raw {
        Value::String(s) if s.trim().is_empty() => (json!({}), None),
        Value::String(s) => match serde_json::from_str::<Value>(s) {
            Ok(v) => (v, None),
            Err(e) => (Value::Null, Some(format!("the arguments are not valid JSON ({e})"))),
        },
        Value::Null => (json!({}), None),
        v => (v.clone(), None),
    }
}

/// OpenAI's usage (`prompt_tokens`/`input_tokens` include the cached part).
fn openai_usage(u: &Value, prompt: &str, completion: &str, details: &str) -> Usage {
    let n = |v: &Value| v.as_u64().unwrap_or(0);
    let cached = n(&u[details]["cached_tokens"]);
    Usage { input: n(&u[prompt]).saturating_sub(cached), output: n(&u[completion]), cache_read: cached, ..Usage::default() }
}

/// A response, read. `round` numbers ids a server left out, so each result still finds its call.
pub fn read(w: Wire, v: &Value, round: usize) -> Round {
    match w {
        Wire::Anthropic => {
            let calls = v["content"].as_array().into_iter().flatten().filter(|b| b["type"] == "tool_use")
                .map(|b| ToolCall { id: b["id"].as_str().unwrap_or("").into(), name: b["name"].as_str().unwrap_or("").into(), input: b["input"].clone(), bad: None })
                .collect();
            let stop = match v["stop_reason"].as_str() {
                Some("tool_use") => Stop::ToolUse,
                Some("refusal") => Stop::Refusal,
                Some("max_tokens") => Stop::MaxTokens,
                _ => Stop::End,
            };
            Round { calls, stop, usage: Usage::from_api(&v["usage"]) }
        }
        Wire::ChatCompletions => {
            let choice = &v["choices"][0];
            let calls: Vec<ToolCall> = choice["message"]["tool_calls"].as_array().into_iter().flatten().enumerate().map(|(i, t)| {
                let (input, bad) = arguments(&t["function"]["arguments"]);
                let id = t["id"].as_str().filter(|s| !s.is_empty()).map_or_else(|| format!("call_{round}_{i}"), str::to_owned);
                ToolCall { id, name: t["function"]["name"].as_str().unwrap_or("").into(), input, bad }
            }).collect();
            // Local servers often say "stop" beside their tool calls: the calls decide.
            let stop = match choice["finish_reason"].as_str() {
                Some("length") => Stop::MaxTokens,
                Some("content_filter") => Stop::Refusal,
                _ if !calls.is_empty() => Stop::ToolUse,
                _ if choice["message"]["refusal"].as_str().is_some_and(|r| !r.is_empty()) => Stop::Refusal,
                _ => Stop::End,
            };
            Round { calls, stop, usage: openai_usage(&v["usage"], "prompt_tokens", "completion_tokens", "prompt_tokens_details") }
        }
        Wire::Responses => {
            let items = v["output"].as_array().cloned().unwrap_or_default();
            let calls: Vec<ToolCall> = items.iter().filter(|i| i["type"] == "function_call").enumerate().map(|(k, i)| {
                let (input, bad) = arguments(&i["arguments"]);
                let id = i["call_id"].as_str().filter(|s| !s.is_empty()).map_or_else(|| format!("call_{round}_{k}"), str::to_owned);
                ToolCall { id, name: i["name"].as_str().unwrap_or("").into(), input, bad }
            }).collect();
            let refused = items.iter().filter(|i| i["type"] == "message").flat_map(|i| i["content"].as_array().cloned().unwrap_or_default()).any(|c| c["type"] == "refusal");
            let stop = match (v["status"].as_str(), v["incomplete_details"]["reason"].as_str()) {
                (Some("incomplete"), Some("max_output_tokens")) => Stop::MaxTokens,
                (Some("incomplete"), Some("content_filter")) => Stop::Refusal,
                _ if !calls.is_empty() => Stop::ToolUse,
                _ if refused => Stop::Refusal,
                _ => Stop::End,
            };
            Round { calls, stop, usage: openai_usage(&v["usage"], "input_tokens", "output_tokens", "input_tokens_details") }
        }
    }
}

/// The turn after a tool round: the model's turn as sent, then one result per call (`results` in `r.calls` order).
pub fn answer_turn(w: Wire, history: &mut Vec<Value>, v: &Value, r: &Round, results: Vec<String>) {
    match w {
        Wire::Anthropic => {
            history.push(json!({"role": "assistant", "content": v["content"]}));
            let blocks: Vec<Value> = r.calls.iter().zip(results).map(|(c, out)| json!({"type": "tool_result", "tool_use_id": c.id, "content": out})).collect();
            history.push(json!({"role": "user", "content": blocks}));
        }
        Wire::ChatCompletions => {
            // Rebuilt rather than echoed, so a call whose id the server left out carries the one its result names.
            let message = &v["choices"][0]["message"];
            let calls: Vec<Value> = r.calls.iter().zip(message["tool_calls"].as_array().cloned().unwrap_or_default()).map(|(c, t)| {
                let args = match &t["function"]["arguments"] { Value::String(s) => s.clone(), a => a.to_string() };
                json!({"id": c.id, "type": "function", "function": {"name": c.name, "arguments": args}})
            }).collect();
            let content = message["content"].as_str().filter(|s| !s.is_empty()).map_or(Value::Null, |s| json!(s));
            history.push(json!({"role": "assistant", "content": content, "tool_calls": calls}));
            for (c, out) in r.calls.iter().zip(results) { history.push(json!({"role": "tool", "tool_call_id": c.id, "content": out})); }
        }
        Wire::Responses => {
            // `store: false`: an item id would refer to nothing, so the calls go back by `call_id` only, and the
            // model's text as an assistant message; reasoning items are left out.
            let mut k = 0;
            for i in v["output"].as_array().into_iter().flatten() {
                match i["type"].as_str() {
                    Some("function_call") => {
                        let Some(c) = r.calls.get(k) else { continue };
                        k += 1;
                        let args = match &i["arguments"] { Value::String(s) => s.clone(), a => a.to_string() };
                        history.push(json!({"type": "function_call", "call_id": c.id, "name": c.name, "arguments": args}));
                    }
                    Some("message") => {
                        let text: String = i["content"].as_array().into_iter().flatten().filter(|c| c["type"] == "output_text").filter_map(|c| c["text"].as_str()).collect();
                        if !text.is_empty() { history.push(json!({"role": "assistant", "content": text})); }
                    }
                    _ => {}
                }
            }
            for (c, out) in r.calls.iter().zip(results) { history.push(json!({"type": "function_call_output", "call_id": c.id, "output": out})); }
        }
    }
}

/// A streamed Responses answer: the final response object (`response.completed`, or `response.incomplete` with its
/// reason). A server that ignored `stream` and answered in JSON is read as it is.
pub fn responses_final(text: &str) -> Result<Value> {
    if let Ok(v) = serde_json::from_str::<Value>(text.trim()) {
        if v.get("output").is_some() { return Ok(v) }
    }
    // The items as they finished: with `store: false` the ChatGPT route may close with `response.completed` whose
    // `output` is empty, the answer having come only as `response.output_item.done` events (27 Oct 03:08: Luna spent
    // 8,944 output tokens on the plan refresher and the job read "no text"). Pi's own client reads the events too.
    let mut items: Vec<(u64, Value)> = vec![];
    for line in text.lines() {
        let Some(data) = line.strip_prefix("data:").map(str::trim) else { continue };
        let Ok(ev) = serde_json::from_str::<Value>(data) else { continue };
        match ev["type"].as_str() {
            // In `output_index` order, which is the response's order, not necessarily the events'.
            Some("response.output_item.done") => items.push((ev["output_index"].as_u64().unwrap_or(items.len() as u64), ev["item"].clone())),
            Some("response.completed") | Some("response.incomplete") => {
                let mut r = ev["response"].clone();
                if r["output"].as_array().is_none_or(|o| o.is_empty()) && !items.is_empty() {
                    items.sort_by_key(|(i, _)| *i);
                    r["output"] = Value::Array(items.into_iter().map(|(_, item)| item).collect());
                }
                return Ok(r);
            }
            Some("response.failed") => bail!("the model failed: {}", message(&ev["response"]["error"])),
            Some("error") => bail!("the model failed: {}", message(&ev)),
            _ => {}
        }
    }
    bail!("the stream ended without a finished response")
}

/// An error object's message, at most 200 characters.
pub fn message(e: &Value) -> String {
    let m = e["message"].as_str().or(e["error"]["message"].as_str()).or(e["error"].as_str()).or(e["detail"].as_str()).or(e["code"].as_str()).unwrap_or("");
    m.chars().take(200).collect()
}
