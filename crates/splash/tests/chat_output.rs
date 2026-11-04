//! Output reading against Splash's Python server: model outputs decoded token by token (Qwen) or fed as text chunks
//! (Flint's GLM syntax) give the same deltas, reasoning, content and tool calls; value conversion and held partial
//! tags agree.

#[path = "chat_support/mod.rs"]
mod support;

use serde_json::{json, Value};
use splash::chat::format::{ChatFormat, FLINT, QWEN};
use splash::chat::output::{convert_value, hold_partial, Collected, Delta, OutputReader, ToolParameterTypes, ToolTypes, TypeSet};
use splash::chat::tokens::Streamer;
use std::collections::HashMap;

fn type_set(value: &Value) -> Option<TypeSet> {
    value.as_array().map(|a| a.iter().map(|t| t.as_str().unwrap().to_owned()).collect())
}

fn tool_types(table: &Value) -> Option<Box<dyn ToolTypes>> {
    let table = table.as_object()?;
    let map: HashMap<String, ToolParameterTypes> = table
        .iter()
        .map(|(name, types)| {
            let parameters = types["parameters"].as_object().unwrap().iter().map(|(p, t)| (p.clone(), type_set(t))).collect();
            (name.clone(), ToolParameterTypes { parameters, other: type_set(&types["other"]) })
        })
        .collect();
    Some(Box::new(map))
}

/// A delta as server._collect's publish records it.
fn python_event(delta: &Delta) -> Value {
    match delta {
        Delta::Reasoning(text) => json!(["reasoning_content", text]),
        Delta::Content(text) => json!(["content", text]),
        Delta::ToolCallStart { index, id, name } => {
            json!(["tool", {"index": index, "id": id, "type": "function", "function": {"name": name}}])
        }
        Delta::ToolCallArguments { index, arguments } => json!(["tool", {"index": index, "function": {"arguments": arguments}}]),
    }
}

fn reader(case: &Value, format: &ChatFormat) -> OutputReader {
    OutputReader::new(
        format,
        case["thinking"].as_bool().unwrap(),
        case["may_call_tools"].as_bool().unwrap(),
        tool_types(&case["tool_types"]),
        "req42",
        case.get("structured").and_then(Value::as_bool).unwrap_or(false),
    )
}

fn check(label: &str, case: &Value, deltas: &[Delta], collected: &Collected) {
    let events: Vec<Value> = deltas.iter().map(python_event).collect();
    assert_eq!(Value::Array(events), case["events"], "{label}: events");
    let calls: Vec<Value> = collected.tool_calls.iter().map(|c| c.to_openai()).collect();
    assert_eq!(Value::Array(calls), case["tool_calls"], "{label}: tool calls");
    assert_eq!(collected.reasoning, case["reasoning"].as_str().unwrap(), "{label}");
    assert_eq!(collected.content, case["content"].as_str().unwrap(), "{label}");
    assert_eq!(collected.reasoning_open, case["reasoning_open"], "{label}");
    assert_eq!(collected.cut_in_call, case["cut_in_call"], "{label}");
    assert_eq!(collected.cut_call.as_deref(), case["cut_call"].as_str(), "{label}");
    // The stream's concatenated deltas are the final answer.
    let streamed: String = deltas.iter().filter_map(|d| match d {
        Delta::Content(t) => Some(t.as_str()),
        _ => None,
    }).collect();
    assert_eq!(streamed, collected.content, "{label}: streamed content");
}

#[test]
fn qwen_outputs_match_python() {
    let Some(tokenizer) = support::tokenizer() else { return };
    for case in support::fixture("output.json")["qwen"].as_array().unwrap() {
        let label = case["name"].as_str().unwrap();
        let stops: Vec<String> =
            case.get("stop").and_then(Value::as_array).map_or(Vec::new(), |s| s.iter().map(|v| v.as_str().unwrap().to_owned()).collect());
        let mut streamer = Streamer::new(tokenizer.clone(), stops);
        let mut reader = reader(case, &QWEN);
        let mut deltas = Vec::new();
        let ids = support::ids(&case["token_ids"]);
        for frame in ids.chunks(case["frames"].as_u64().unwrap() as usize) {
            for text in streamer.put_tokens(frame).unwrap() {
                deltas.extend(reader.put(&text));
            }
        }
        for text in streamer.end().unwrap() {
            deltas.extend(reader.put(&text));
        }
        assert_eq!(streamer.stop_sequence(), case["stop_sequence"].as_str(), "{label}");
        let length = case.get("reason").and_then(Value::as_str) == Some("length");
        let (owed, collected) = reader.finish(length, streamer.stop_sequence().is_some());
        deltas.extend(owed);
        check(label, case, &deltas, &collected);
        let tool_calls = case["may_call_tools"].as_bool().unwrap();
        let counted = streamer.count_reasoning_tokens(Some(&QWEN), case["thinking"].as_bool().unwrap(), tool_calls);
        assert_eq!(counted as u64, case["reasoning_tokens"].as_u64().unwrap(), "{label}: reasoning tokens");
    }
}

#[test]
fn flint_outputs_match_python() {
    for case in support::fixture("output.json")["flint"].as_array().unwrap() {
        let label = case["name"].as_str().unwrap();
        let mut reader = reader(case, &FLINT);
        let mut deltas = Vec::new();
        for chunk in case["chunks"].as_array().unwrap() {
            deltas.extend(reader.put(chunk.as_str().unwrap()));
        }
        let (owed, collected) = reader.finish(false, false);
        deltas.extend(owed);
        check(label, case, &deltas, &collected);
    }
}

#[test]
fn value_conversion_matches_python() {
    for case in support::fixture("output.json")["convert"].as_array().unwrap() {
        let text = case["text"].as_str().unwrap();
        let types = type_set(&case["types"]).unwrap();
        let value = convert_value(text, &types);
        if case["is_text"].as_bool().unwrap() {
            assert_eq!(value, Value::String(text.to_owned()), "{text:?} {types:?}");
        } else {
            assert_eq!(value, case["value"], "{text:?} {types:?}");
            assert!(!value.is_string() || value != Value::String(text.to_owned()), "{text:?}");
        }
    }
}

#[test]
fn held_partial_tags_match_python() {
    for case in support::fixture("output.json")["hold"].as_array().unwrap() {
        let text = case["text"].as_str().unwrap();
        let markers: Vec<&str> = case["markers"].as_array().unwrap().iter().map(|m| m.as_str().unwrap()).collect();
        let (ready, held) = hold_partial(text, &markers);
        assert_eq!(json!([ready, held]), case["split"], "{text:?}");
    }
}

#[test]
fn finish_reason_counts_calls_unless_cut() {
    let call = splash::chat::output::ToolCall { id: "call_1".into(), name: "f".into(), arguments: "{}".into() };
    let collected = Collected {
        reasoning: String::new(),
        content: String::new(),
        tool_calls: vec![call],
        reasoning_open: false,
        cut_in_call: false,
        cut_call: None,
    };
    assert_eq!(collected.finish_reason("stop"), "tool_calls");
    assert_eq!(collected.finish_reason("length"), "length");
    // A stop sequence inside a call leaves it unfinished: the output did not call it.
    let cut = Collected { cut_in_call: true, cut_call: Some("call_1".into()), ..collected };
    assert_eq!(cut.finish_reason("stop"), "stop");
}

#[test]
fn chat_types_cross_threads() {
    fn shared<T: Send + Sync>() {}
    fn sent<T: Send>() {}
    shared::<splash::chat::template::ChatTemplates>();
    shared::<splash::chat::tokens::PromptTokenizer>();
    sent::<Streamer>();
    sent::<OutputReader>();
}

#[test]
fn structured_content_is_validated() {
    use splash::chat::output::validate_response_content;
    let object_only = |v: &Value| if v.is_object() { Ok(()) } else { Err("not an object".to_owned()) };
    assert!(validate_response_content("{\"a\": 1}", Some(&object_only)).is_ok());
    let error = validate_response_content("[1]", Some(&object_only)).unwrap_err();
    assert_eq!((error.status, error.code), (500, "invalid_model_output"));
    assert_eq!(error.message, "model returned invalid structured output: not an object");
    assert!(validate_response_content("not json", None).is_ok());
}
