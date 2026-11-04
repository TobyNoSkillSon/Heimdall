//! Golden tests against Splash 1.4.1's Python: the same requests give the same refusals, policies, grammars (byte for
//! byte) and parameter types. Fixtures come from crates/splash/lab/constrain_golden.py.
use serde_json::Value;
use splash::chat::format::{FLINT, QWEN};
use splash::constrain::{
    is_constrained, normalize_response_format, normalize_tools, output_grammar, ApiError, JsonTypes, ParameterTypes,
};
use std::sync::Arc;

fn fixtures() -> Vec<Value> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/constrain/grammars.json");
    serde_json::from_str(&std::fs::read_to_string(path).expect("grammars.json")).expect("fixture JSON")
}

fn type_list(types: Option<JsonTypes>) -> Value {
    types.map_or(Value::Null, |t| {
        let mut names = t.names();
        names.sort();
        names.into()
    })
}

/// What the Rust port makes of one fixture request, in the generator's shape.
fn run(case: &Value) -> Result<Value, ApiError> {
    let request = &case["request"];
    let chat_format = if case["format"] == "flint" { FLINT } else { QWEN };
    let thinking = case["thinking"].as_bool().unwrap();
    let parallel = request.get("parallel_tool_calls").cloned().unwrap_or(Value::Bool(true));
    let tools = normalize_tools(request.get("tools"), request.get("tool_choice"), Some(&parallel), Default::default())?;
    let response = normalize_response_format(request.get("response_format"))?;
    let policy = tools.as_ref().map(|t| &t.policy);
    let grammar = output_grammar(policy, response.as_ref(), thinking, &chat_format)?;
    let types = tools.as_ref().map(|tools| {
        let mut reader = ParameterTypes::new(Arc::new(tools.policy.clone()));
        let mut names: Vec<String> =
            tools.tools.iter().map(|t| t["function"]["name"].as_str().unwrap().to_string()).collect();
        names.push("not_offered".into());
        let mut out = serde_json::Map::new();
        for name in names {
            let read = reader.of(&name);
            // Python's dict keeps the shape's order; compare as maps.
            let parameters: serde_json::Map<String, Value> =
                read.parameters.iter().map(|(k, v)| (k.clone(), type_list(*v))).collect();
            out.insert(name, serde_json::json!({"parameters": parameters, "other": type_list(read.other)}));
        }
        Value::Object(out)
    });
    Ok(serde_json::json!({
        "tools": tools.as_ref().map(|t| t.tools.clone()),
        "policy": tools.as_ref().map(|t| {
            let mut strict: Vec<&String> = t.policy.strict.iter().collect();
            strict.sort();
            serde_json::json!({
                "names": t.policy.schemas.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>(),
                "required": t.policy.required,
                "parallel": t.policy.parallel,
                "strict": strict,
                "constrained": t.policy.constrained,
            })
        }),
        "constrained": is_constrained(policy, response.as_ref()),
        "grammar": grammar.map(|(g, _)| g),
        "types": types,
    }))
}

fn sorted_keys(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<_> = map.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            Value::Object(entries.into_iter().map(|(k, v)| (k.clone(), sorted_keys(v))).collect())
        }
        Value::Array(items) => Value::Array(items.iter().map(sorted_keys).collect()),
        other => other.clone(),
    }
}

#[test]
fn constrain_grammars_match_python() {
    let mut failures = Vec::new();
    let cases = fixtures();
    assert!(cases.len() > 50, "fixture cases");
    for case in &cases {
        let name = case["name"].as_str().unwrap();
        let expected = &case["expected"];
        match (run(case), expected.get("error")) {
            (Err(error), Some(want)) => {
                let message = want["message"].as_str().unwrap();
                let ok = error.status as u64 == want["status"].as_u64().unwrap()
                    && match case.get("message_prefix").and_then(Value::as_str) {
                        // The JSON Schema meta-check's own wording differs between validators.
                        Some(prefix) => error.message.starts_with(prefix) && error.message.len() > prefix.len(),
                        None => error.message == message,
                    };
                if !ok {
                    failures.push(format!("{name}: {} {:?}, want {want}", error.status, error.message));
                }
            }
            (Err(error), None) => failures.push(format!("{name}: refused with {:?}", error.message)),
            (Ok(_), Some(want)) => failures.push(format!("{name}: accepted, want {want}")),
            (Ok(got), None) => {
                if got["grammar"] != expected["grammar"] {
                    let (g, w) = (got["grammar"].as_str().unwrap_or(""), expected["grammar"].as_str().unwrap_or(""));
                    let at = g.bytes().zip(w.bytes()).position(|(a, b)| a != b).unwrap_or(g.len().min(w.len()));
                    let lo = at.saturating_sub(80);
                    failures.push(format!(
                        "{name}: grammar differs at byte {at}\n got: {:?}\nwant: {:?}",
                        g.get(lo..(at + 80).min(g.len())).unwrap_or(""),
                        w.get(lo..(at + 80).min(w.len())).unwrap_or("")
                    ));
                }
                for field in ["tools", "policy", "constrained"] {
                    if got[field] != expected[field] {
                        failures.push(format!("{name}: {field} {} want {}", got[field], expected[field]));
                    }
                }
                if sorted_keys(&got["types"]) != sorted_keys(&expected["types"]) {
                    failures.push(format!("{name}: types {} want {}", got["types"], expected["types"]));
                }
            }
        }
    }
    assert!(failures.is_empty(), "{} of {} cases differ:\n{}", failures.len(), cases.len(), failures.join("\n"));
}

#[test]
fn constrain_grammars_compile() {
    // Every grammar the port writes is one llguidance takes (without a tokenizer, as Splash checks patterns).
    for case in fixtures() {
        if let Ok(result) = run(&case) {
            if let Some(grammar) = result["grammar"].as_str() {
                let parsed = llguidance::api::TopLevelGrammar::from_lark_or_grammar_list(grammar).expect("parses");
                let error = llguidance::api::GrammarInit::Serialized(parsed)
                    .validate(None, llguidance::api::ParserLimits::default())
                    .into_error();
                assert!(error.is_none(), "{}: {error:?}", case["name"]);
            }
        }
    }
}

/// A reference chain d0 -> d1 -> ... -> d9999 with `end` at its end.
fn chain(end: Value) -> serde_json::Map<String, Value> {
    let mut defs = serde_json::Map::new();
    for i in 0..9999 {
        defs.insert(format!("d{i}"), serde_json::json!({"$ref": format!("#/$defs/d{}", i + 1)}));
    }
    defs.insert("d9999".into(), end);
    defs
}

#[test]
fn constrain_reference_chains_are_refused_not_overflowed() {
    let too_complex = "tool parameter schemas are too complex";
    let strict = |parameters: Value| {
        let tools = serde_json::json!([{"type": "function", "function": {"name": "x", "strict": true, "parameters": parameters}}]);
        normalize_tools(Some(&tools), Some(&"required".into()), None, Default::default()).unwrap().unwrap()
    };
    // Through a parameter's value (literal texts and types read the chain) and through the arguments' own root.
    let in_value = strict(serde_json::json!({"type": "object", "properties": {"a": {"$ref": "#/$defs/d0"}}, "$defs": chain(serde_json::json!({"enum": ["x"]}))}));
    let at_root = strict(serde_json::json!({"$ref": "#/$defs/d0", "$defs": chain(serde_json::json!({"type": "object"}))}));
    for tools in [&in_value, &at_root] {
        let error = output_grammar(Some(&tools.policy), None, false, &QWEN).unwrap_err();
        assert_eq!((error.status, error.message.as_str()), (400, too_complex));
        // The output projector reads them as untyped instead of failing.
        let types = ParameterTypes::new(Arc::new(tools.policy.clone())).of("x");
        assert_eq!(types.other, None);
    }
    // A non-strict tool's chain only costs its types.
    let tools = serde_json::json!([{"type": "function", "function": {"name": "y", "parameters":
        {"type": "object", "properties": {"a": {"$ref": "#/$defs/d0"}}, "$defs": chain(serde_json::json!({"type": "integer"}))}}}]);
    let free = normalize_tools(Some(&tools), None, None, Default::default()).unwrap().unwrap();
    assert!(ParameterTypes::new(Arc::new(free.policy)).of("y").parameters.is_empty());
    // A short chain still reads through.
    let mut defs = serde_json::Map::new();
    for i in 0..50 {
        defs.insert(format!("d{i}"), serde_json::json!({"$ref": format!("#/$defs/d{}", i + 1)}));
    }
    defs.insert("d50".into(), serde_json::json!({"type": "integer"}));
    let short = strict(serde_json::json!({"type": "object", "properties": {"a": {"$ref": "#/$defs/d0"}}, "$defs": defs}));
    assert!(output_grammar(Some(&short.policy), None, false, &QWEN).is_ok());
    assert_eq!(ParameterTypes::new(Arc::new(short.policy.clone())).of("x").of("a"), Some(JsonTypes::of(&["integer"])));
}
