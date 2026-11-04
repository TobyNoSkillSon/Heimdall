//! Bug note 2026-11-02T14-56-51-3: Heimdall called `t3_pending_request_list`, `_read` and `_respond` (and
//! `t3_queue_list`, `t3_queue_edit`) for days after T3 had folded them into `t3_pending_request` and `t3_queue` with an
//! `action`; every call failed with T3_CAPABILITY_MISSING while the tests mocked the old names. This checks every T3
//! tool Heimdall calls by name, its argument keys and its `action` against the tool list of the installed public
//! release (T3 Code 0.1.6, recorded from the live server on 2 Nov in `fixtures/t3-0.1.6-tools.json`).
use serde_json::Value;

const SOURCES: &[(&str, &str)] = &[
    ("t3/src/lib.rs", include_str!("../src/lib.rs")),
    ("t3/src/observer.rs", include_str!("../src/observer.rs")),
    ("t3/src/orchestrator.rs", include_str!("../src/orchestrator.rs")),
    ("t3/src/threads.rs", include_str!("../src/threads.rs")),
    ("t3/src/workers.rs", include_str!("../src/workers.rs")),
    ("app/src/islands/binding.rs", include_str!("../../app/src/islands/binding.rs")),
    ("app/src/background_t3.rs", include_str!("../../app/src/background_t3.rs")),
    ("app/src/emergency.rs", include_str!("../../app/src/emergency.rs")),
    ("app/src/relay_catalog.rs", include_str!("../../app/src/relay_catalog.rs")),
];

fn t3_tool(name: &str) -> bool {
    name.starts_with("t3_") || matches!(name, "orchestrator_capabilities" | "delegate_task" | "task_status" | "task_cancel")
}

/// Every `call("tool", json!({...}))` (and `call_deadline`, `call_raw`): the tool and its literal argument object.
fn calls(source: &str) -> Vec<(String, Option<String>)> {
    let mut out = vec![];
    for opener in ["call(\"", "call_deadline(\"", "call_raw(\""] {
        let mut rest = source;
        while let Some(at) = rest.find(opener) {
            rest = &rest[at + opener.len()..];
            let Some(end) = rest.find('"') else { break };
            let name = rest[..end].to_owned();
            let after = rest[end + 1..].trim_start_matches([',', ' ']);
            let args = after.strip_prefix("json!(").filter(|a| a.starts_with('{')).map(|a| {
                let mut depth = 0;
                let mut stop = a.len();
                for (i, c) in a.char_indices() {
                    match c {
                        '{' => depth += 1,
                        '}' => {
                            depth -= 1;
                            if depth == 0 {
                                stop = i + 1;
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                a[..stop].to_owned()
            });
            out.push((name, args));
        }
    }
    out
}

/// The top-level `"key":` names of a `json!` object literal, and the literal `"action"` value if there is one.
fn keys(object: &str) -> (Vec<String>, Option<String>) {
    let mut keys = vec![];
    let mut action = None;
    let mut depth = 0;
    let bytes: Vec<char> = object.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            '{' | '[' => depth += 1,
            '}' | ']' => depth -= 1,
            '"' if depth == 1 => {
                let start = i + 1;
                let mut j = start;
                while j < bytes.len() && bytes[j] != '"' {
                    j += 1;
                }
                let word: String = bytes[start..j].iter().collect();
                let tail: String = bytes[j + 1..].iter().collect();
                let tail = tail.trim_start();
                if let Some(value) = tail.strip_prefix(':') {
                    if word == "action" {
                        let value = value.trim_start();
                        if let Some(v) = value.strip_prefix('"') {
                            action = v.split('"').next().map(str::to_owned);
                        }
                    }
                    keys.push(word);
                }
                i = j;
            }
            _ => {}
        }
        i += 1;
    }
    (keys, action)
}

#[test]
fn every_t3_tool_heimdall_calls_is_in_the_installed_release() {
    let live: Value = serde_json::from_str(include_str!("fixtures/t3-0.1.6-tools.json")).unwrap();
    let names: Vec<&str> = live["toolNames"].as_array().unwrap().iter().map(|n| n.as_str().unwrap()).collect();
    let mut checked = 0;
    let mut failures = vec![];
    for (file, source) in SOURCES {
        for (name, args) in calls(source).into_iter().filter(|(n, _)| t3_tool(n)) {
            checked += 1;
            if !names.contains(&name.as_str()) {
                failures.push(format!("{file}: {name} is not a T3 0.1.6 tool"));
                continue;
            }
            let schema = &live["inputSchemas"][&name];
            let (Some(args), false) = (args, schema.is_null()) else { continue };
            let (keys, action) = keys(&args);
            for key in keys {
                if schema["properties"].get(&key).is_none() {
                    failures.push(format!("{file}: {name} has no argument {key}"));
                }
            }
            if let Some(action) = action {
                if !schema["properties"]["action"]["enum"].as_array().unwrap().iter().any(|a| a == action.as_str()) {
                    failures.push(format!("{file}: {name} has no action {action}"));
                }
            }
        }
    }
    // The pending-request helpers name the tool through a constant.
    assert_eq!(t3::PENDING_REQUEST, "t3_pending_request");
    assert!(names.contains(&t3::PENDING_REQUEST));
    assert!(SOURCES[0].1.contains(r#""action":"list""#) && SOURCES[0].1.contains(r#""action":"read""#) && SOURCES[0].1.contains(r#""action":"respond""#));
    assert!(checked > 40, "only {checked} calls found; the scan lost its pattern");
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// Not in a call, and not in text that tells the agent which tool to use (review 2 Nov: the queued-send reply still
/// named t3_queue_edit). Only the orchestrator keeps the old names, as the operation names it judges calls by.
#[test]
fn the_retired_names_appear_nowhere_but_the_orchestrators_operation_names() {
    let retired = ["t3_pending_request_list", "t3_pending_request_read", "t3_pending_request_respond", "t3_queue_list", "t3_queue_read",
        "t3_queue_edit", "t3_queue_cancel", "t3_queue_reorder", "t3_queue_promote_to_steer", "t3_thread_fork", "t3_thread_merge_back",
        "t3_thread_transfers", "t3_thread_configuration", "t3_thread_rollback"];
    for (file, source) in SOURCES.iter().filter(|(f, _)| !f.ends_with("orchestrator.rs")) {
        for name in retired {
            assert!(!source.contains(name), "{file} names {name}");
        }
    }
}
