//! OpenAI chat-completion shapes, errors and per-request metrics (api_shapes.py, errors.py, metrics.py).
use serde_json::{json, Map, Value};

/// An error the API answers with (errors.APIError); OpenAI's dialect.
#[derive(Clone, Debug, PartialEq)]
pub struct ApiError {
    pub status: u16,
    pub message: String,
    pub code: String,
    /// Seconds a client should wait before retrying, for 503s that say so.
    pub retry_after: Option<u32>,
}

impl ApiError {
    pub fn new(status: u16, message: impl Into<String>, code: impl Into<String>) -> Self {
        Self { status, message: message.into(), code: code.into(), retry_after: None }
    }
    /// A 400 with Splash's default code.
    pub fn bad(message: impl Into<String>) -> Self {
        Self::new(400, message, "invalid_request_error")
    }
    pub fn timeout() -> Self {
        Self::new(504, "request timed out", "request_timeout")
    }
    pub fn context_length() -> Self {
        Self::new(400, "prompt exceeds the context window", "context_length_exceeded")
    }
    pub fn payload(&self) -> Value {
        let kind = if self.status >= 500 { "server_error" } else { "invalid_request_error" };
        json!({"error": {"message": self.message, "type": kind, "code": self.code}})
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}: {}", self.status, self.code, self.message)
    }
}
impl std::error::Error for ApiError {}

/// The prefix cache's part in a request (backend.CacheInfo).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CacheInfo {
    pub status: &'static str,
    pub matched_tokens: u32,
    pub lane: Option<u32>,
    pub deduplicated_tokens: u64,
}

/// A finished generation (backend.NativeResult).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NativeResult {
    /// stop, length or cancelled.
    pub reason: &'static str,
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub start_to_first_token_ms: f64,
    pub first_token_to_done_ms: f64,
    pub request_wall_ms: f64,
    pub prefill_tokens: u32,
    pub cache: CacheInfo,
    pub stop_sequence: Option<String>,
    pub first_token_batch_tokens: u32,
    pub drafted_tokens: u32,
    pub accepted_draft_tokens: u32,
    pub draft_cycles: u32,
    pub bounded: bool,
}

impl NativeResult {
    pub fn metrics(&self) -> Value {
        let decode_tokens = self.completion_tokens.saturating_sub(self.first_token_batch_tokens);
        let mut latency = Map::new();
        for (name, ms) in [
            ("start_to_first_token_ms", self.start_to_first_token_ms),
            ("first_token_to_done_ms", self.first_token_to_done_ms),
            ("wall_ms", self.request_wall_ms),
        ] {
            if ms.is_finite() && ms >= 0.0 {
                latency.insert(name.into(), json!(ms));
            }
        }
        let get = |m: &Map<String, Value>, k: &str| m.get(k).and_then(Value::as_f64);
        if let (true, Some(wall), Some(decode)) =
            (self.completion_tokens > 0, get(&latency, "wall_ms"), get(&latency, "first_token_to_done_ms"))
        {
            if wall >= decode {
                let ttft = wall - decode;
                latency.insert("ttft_ms".into(), json!(ttft));
                if let Some(start) = get(&latency, "start_to_first_token_ms").filter(|s| ttft >= *s) {
                    latency.insert("queue_to_start_ms".into(), json!(ttft - start));
                }
            }
        }
        if let Some(decode_ms) = get(&latency, "first_token_to_done_ms").filter(|ms| *ms > 0.0) {
            if self.first_token_batch_tokens > 0 && decode_tokens > 0 {
                let rate = decode_tokens as f64 * 1000.0 / decode_ms;
                if rate.is_finite() {
                    latency.insert("stream_tokens_per_second".into(), json!(rate));
                }
            }
        }
        let mut metrics = json!({
            "prefill": {"tokens": self.prefill_tokens},
            "decode": {"tokens": decode_tokens},
            "request_latency": latency,
            "cache": {
                "status": if self.cache.status.is_empty() { "unknown" } else { self.cache.status },
                "matched_tokens": self.cache.matched_tokens,
                "deduplicated_tokens": self.cache.deduplicated_tokens,
                "lane": self.cache.lane,
            },
        });
        if self.drafted_tokens > 0 {
            metrics["speculation"] = json!({
                "drafted_tokens": self.drafted_tokens,
                "accepted_draft_tokens": self.accepted_draft_tokens,
                "cycles": self.draft_cycles,
            });
        }
        metrics
    }

    /// llama-server-compatible counts and request lifecycle timings.
    pub fn timings(&self) -> Value {
        let metrics = self.metrics();
        let latency = &metrics["request_latency"];
        let prompt_ms = latency["start_to_first_token_ms"].as_f64().unwrap_or(0.0);
        let rate = if prompt_ms != 0.0 { self.prefill_tokens as f64 * 1000.0 / prompt_ms } else { 0.0 };
        let mut t = json!({
            "prompt_n": self.prompt_tokens,
            "prompt_ms": prompt_ms,
            "prompt_per_second": if rate.is_finite() { rate } else { 0.0 },
            "predicted_n": self.completion_tokens,
            "predicted_ms": latency["first_token_to_done_ms"].as_f64().unwrap_or(0.0),
            "predicted_per_second": latency["stream_tokens_per_second"].as_f64().unwrap_or(0.0),
            "cache_n": self.cache.matched_tokens,
        });
        if self.drafted_tokens > 0 {
            t["draft_n"] = json!(self.drafted_tokens);
            t["draft_n_accepted"] = json!(self.accepted_draft_tokens);
        }
        t
    }

    pub fn usage(&self, reasoning_tokens: u32) -> Value {
        json!({
            "prompt_tokens": self.prompt_tokens,
            "completion_tokens": self.completion_tokens,
            "total_tokens": self.prompt_tokens + self.completion_tokens,
            "prompt_tokens_details": {"cached_tokens": self.cache.matched_tokens},
            "completion_tokens_details": {"reasoning_tokens": reasoning_tokens},
        })
    }

    /// OpenAI finish_reason: whole tool calls count only when the token limit did not end the output.
    pub fn finish_reason(&self, called: bool) -> &'static str {
        if called && self.reason != "length" { "tool_calls" } else { self.reason }
    }
}

/// One streamed chunk (api_shapes.stream_chunk).
pub fn stream_chunk(model: &str, id: &str, created: u64, delta: Value, finish: Option<&str>) -> Value {
    json!({
        "id": format!("chatcmpl-{id}"),
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
    })
}

pub fn completion_response(model: &str, id: &str, created: u64, result: &NativeResult, message: Value, called: bool, reasoning_tokens: u32) -> Value {
    json!({
        "id": format!("chatcmpl-{id}"),
        "object": "chat.completion",
        "created": created,
        "model": model,
        "choices": [{"index": 0, "message": message, "finish_reason": result.finish_reason(called)}],
        "usage": result.usage(reasoning_tokens),
        "metrics": result.metrics(),
        "timings": result.timings(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn metrics_match_splash() {
        let r = NativeResult {
            reason: "stop",
            prompt_tokens: 100,
            completion_tokens: 11,
            start_to_first_token_ms: 50.0,
            first_token_to_done_ms: 200.0,
            request_wall_ms: 300.0,
            prefill_tokens: 60,
            first_token_batch_tokens: 1,
            cache: CacheInfo { status: "hit", matched_tokens: 40, lane: Some(0), deduplicated_tokens: 0 },
            ..Default::default()
        };
        let m = r.metrics();
        assert_eq!(m["decode"]["tokens"], 10);
        assert_eq!(m["request_latency"]["ttft_ms"], 100.0);
        assert_eq!(m["request_latency"]["queue_to_start_ms"], 50.0);
        assert_eq!(m["request_latency"]["stream_tokens_per_second"], 50.0);
        assert_eq!(r.timings()["prompt_per_second"], 1200.0);
        assert_eq!(r.finish_reason(true), "tool_calls");
        assert_eq!(NativeResult { reason: "length", ..r }.finish_reason(true), "length");
    }
}
