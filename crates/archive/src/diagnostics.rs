//! Optional application context and sparse behavioural checks at the common evidence boundary.
//! No archive reader installs these hooks. Callbacks must never wait for another application lock.
use crate::Record;
use serde_json::Value;
use std::sync::OnceLock;

pub struct Hooks {
    pub context: fn(&Record) -> Option<Value>,
    pub observe: fn(&Record, i64) -> Vec<Record>,
    /// Every record as it is handed to the archive, on the caller's thread (the app's log line). Must not block.
    pub enqueued: fn(&Record),
}
static HOOKS: OnceLock<Hooks> = OnceLock::new();
pub fn install(hooks: Hooks) {
    let _ = HOOKS.set(hooks);
}
pub(crate) fn enrich(r: &Record) -> Option<Record> {
    let context = (HOOKS.get()?.context)(r)?;
    let mut r = r.clone();
    if r.payload.is_object() {
        r.payload["system_context"] = context;
    } else {
        r.payload = serde_json::json!({"value":r.payload,"system_context":context});
    }
    Some(r)
}
pub(crate) fn enqueued(r: &Record) {
    if let Some(hooks) = HOOKS.get() {
        (hooks.enqueued)(r);
    }
}
pub(crate) fn observe(r: &Record, id: i64) -> Vec<Record> {
    if r.kind == "invariant_violation" {
        return vec![];
    }
    HOOKS.get().map(|h| (h.observe)(r, id)).unwrap_or_default()
}
/// Classification shared by live enrichment and the retrospective `--problems` view.
/// Recovery and intentionally disabled components remain in the full timeline.
pub fn problem(kind: &str, p: &Value) -> bool {
    if kind == "send_decision" && (p["latency_ms"].as_f64().is_some_and(|v|v>500.) || p["pending_truncated"]==true) { return true; }
    if kind == "invariant_violation" || kind == "diagnostic_drops" {
        return true;
    }
    if kind == "archive_stopped"
        || kind == "listen_worker_ended"
        || kind.ends_with("_failure")
        || kind.ends_with("_failed")
        || kind.ends_with("_error")
        || kind.ends_with("_hung")
        || kind.ends_with("_stalled")
    {
        return true;
    }
    if ["memory_refusal", "memory_unload", "load_failed"]
        .contains(&p["event"].as_str().unwrap_or(""))
    {
        return true;
    }
    if [
        "problem",
        "failed",
        "unavailable",
        "restarting",
        "error",
        "stopped",
        "denied",
        "stalled",
        "blocked",
        "Failed",
        "NeedsReview",
        "Restarting",
    ]
    .contains(&p["state"].as_str().unwrap_or(""))
    {
        return true;
    }
    if p["queue_depth"].as_u64().is_some_and(|n| n > 1)
        || structured_error(&p["problem"])
        || p["transport_accepted"] == false
        || p["event"]["error"] == true
    {
        return true;
    }
    // Inspect structured error fields, never transcript/tool text containing the word 'error'.
    match p {
        Value::Object(m) => m.iter().any(|(k, v)| {
            k != "system_context"
                && ((k == "error" || k.ends_with("_error")) && structured_error(v)
                    || ["result", "output", "event", "wire", "decision"].contains(&k.as_str())
                        && problem("", v))
        }),
        _ => false,
    }
}
fn structured_error(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(v) => *v,
        Value::String(s) => !s.trim().is_empty(),
        Value::Number(n) => n.as_f64() != Some(0.),
        Value::Array(v) => !v.is_empty(),
        Value::Object(v) => !v.is_empty(),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn classifies_real_voice_and_drama_failures_without_flagging_recovery() {
        assert!(problem(
            "voice_reply",
            &json!({"outcome":"nothing","reason":"decider_error","error":"reply question requires audio"})
        ));
        assert!(problem(
            "drama3",
            &json!({"event":"memory_refusal","available_bytes":71300000000u64,"required_available_bytes":72400000000u64})
        ));
        assert!(!problem(
            "health",
            &json!({"state":"recovered","reason":null})
        ));
        assert!(!problem("utterance", &json!({"text":"there is an error"})));
        assert!(!problem("command_action", &json!({"error":null})));
        assert!(!problem("health", &json!({"error":"","state":"recovered"})));
        assert!(problem(
            "agent_state",
            &json!({"state":"NeedsReview","queue_depth":7})
        ));
    }
}
