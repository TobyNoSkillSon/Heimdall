//! Mask parity with Splash 1.4.1's TokenConstraint over the real Qwen3.8 tokenizer (read only; no model). Each trace
//! replays the same commits and mask requests the Python ran and compares per-row digests (sha256 prefix of the row's
//! little-endian bytes and its allowed-token count). Fixtures come from crates/splash/lab/constrain_golden.py.
use serde_json::Value;
use sha2::{Digest, Sha256};
use splash::chat::format::QWEN;
use splash::constrain::{ConstraintFactory, TokenConstraint};
use splash::engine::MaskProvider;
use std::path::PathBuf;

/// Splash's own prepared tokenizer under the user's home, unless SPLASH_TOKENIZER_DIR names one.
fn qwen_tokenizer() -> String {
    format!("{}/Library/Application Support/Splash/models/.resolved/469b4497c8e3e5cff64591ad71a0aa828d59cdf241c84772599878d51566b454/tokenizer", std::env::var("HOME").unwrap_or_default())
}

fn factory() -> Option<ConstraintFactory> {
    let dir = PathBuf::from(std::env::var("SPLASH_TOKENIZER_DIR").unwrap_or_else(|_| qwen_tokenizer()));
    let path = dir.join("tokenizer.json");
    if !path.exists() {
        println!("note: no Qwen tokenizer at {}; mask parity skipped", path.display());
        return None;
    }
    let tokenizer = tokenizers::Tokenizer::from_file(&path).expect("tokenizer");
    Some(ConstraintFactory::new(&tokenizer, QWEN).expect("factory"))
}

fn traces() -> Vec<Value> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/constrain/masks.json");
    let fixture: Value = serde_json::from_str(&std::fs::read_to_string(path).expect("masks.json")).expect("JSON");
    fixture["traces"].as_array().unwrap().clone()
}

fn tokens(value: &Value) -> Vec<u32> {
    value.as_array().unwrap().iter().map(|t| t.as_u64().unwrap() as u32).collect()
}

fn digest(row: &[u32]) -> Value {
    let bytes: Vec<u8> = row.iter().flat_map(|w| w.to_le_bytes()).collect();
    let hash = hex::encode(Sha256::digest(&bytes));
    let allowed: u32 = row.iter().map(|w| w.count_ones()).sum();
    serde_json::json!([&hash[..16], allowed])
}

fn replay(constraint: &TokenConstraint, op: &Value) -> Value {
    let error = |e: anyhow::Error| serde_json::json!({"error": e.to_string()});
    match op["op"].as_str().unwrap() {
        "commit" => constraint.commit(&tokens(&op["tokens"])).map_or_else(error, |_| serde_json::json!({"ok": true})),
        "masks" => {
            let words = constraint.words_per_mask();
            match constraint.masks(&tokens(&op["simulation"]), words as u32) {
                Ok(rows) => serde_json::json!({"rows": rows.chunks(words).map(digest).collect::<Vec<_>>()}),
                Err(e) => error(e),
            }
        }
        _ => constraint.finish().map_or_else(error, |_| serde_json::json!({"ok": true})),
    }
}

#[test]
fn constrain_masks_match_python() {
    let Some(factory) = factory() else { return };
    let mut failures = Vec::new();
    for trace in traces() {
        let name = trace["name"].as_str().unwrap();
        let openings: Vec<(Vec<u32>, String)> = trace["openings"]
            .as_array()
            .unwrap()
            .iter()
            .map(|o| (tokens(&o[0]), o[1].as_str().unwrap().to_string()))
            .collect();
        let prefixes = || openings.clone();
        let constraint = factory.create(trace["grammar"].as_str().unwrap(), None, Some(&prefixes)).expect("compiles");
        for (index, (op, want)) in trace["ops"].as_array().unwrap().iter().zip(trace["results"].as_array().unwrap()).enumerate() {
            let got = replay(&constraint, op);
            // Parser errors carry llguidance's wording, which may change between releases; compare their kind.
            let same = match (got.get("error"), want.get("error")) {
                (Some(g), Some(w)) => {
                    let (g, w) = (g.as_str().unwrap(), w.as_str().unwrap());
                    g == w || (g.starts_with("Parser Error") && w.starts_with("Parser Error"))
                }
                _ => got == *want,
            };
            if !same {
                failures.push(format!("{name} op {index} {op}: got {got}, want {want}"));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn constrain_cache_shares_compiled_grammars() {
    let Some(factory) = factory() else { return };
    let trace = traces().into_iter().find(|t| t["name"] == "json_answer").unwrap();
    let grammar = trace["grammar"].as_str().unwrap().to_string();
    let opening = tokens(&trace["ops"][1]["tokens"]);
    let first = factory.create(&grammar, None, None).expect("compiles");
    let second = factory.create(&grammar, None, None).expect("cached");
    let stats = factory.stats();
    assert_eq!((stats.entries, stats.hits, stats.misses), (1, 1, 1));
    assert_eq!(stats.source_bytes, grammar.len());
    // Each request gets its own matcher state.
    let start = first.masks(&[], first.words_per_mask() as u32).unwrap();
    first.commit(&opening).unwrap();
    let advanced = first.masks(&[], first.words_per_mask() as u32).unwrap();
    assert_ne!(start, advanced);
    assert_eq!(second.masks(&[], second.words_per_mask() as u32).unwrap(), start);
    // A refused prefix is the request's 400.
    let refuse = || vec![(vec![9u32, 9, 9], "tool x has too many parameters to constrain".to_string())];
    let error = factory.create("%llguidance {}\nstart: \"a\"\n", None, Some(&refuse)).err().expect("refused");
    assert_eq!((error.status, error.message.as_str()), (400, "tool x has too many parameters to constrain"));
    let error = factory.create("%llguidance {}\nstart: %json {\"type\": \"nope\"}\n", None, None).err().expect("bad schema");
    assert!(error.message.starts_with("unsupported output schema: "), "{}", error.message);
}

#[test]
fn constrain_request_constraint_builds_the_python_grammar() {
    let Some(factory) = factory() else { return };
    let dir = PathBuf::from(std::env::var("SPLASH_TOKENIZER_DIR").unwrap_or_else(|_| qwen_tokenizer()));
    let tokenizer = tokenizers::Tokenizer::from_file(dir.join("tokenizer.json")).unwrap();
    let encode = |text: &str| tokenizer.encode(text, false).unwrap().get_ids().to_vec();
    let cases: Value = serde_json::from_str(
        &std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/constrain/grammars.json")).unwrap(),
    )
    .unwrap();
    let trace = traces().into_iter().find(|t| t["name"] == "tool_call").unwrap();
    let case = cases.as_array().unwrap().iter().find(|c| c["name"] == trace["case"]).unwrap();
    let tools = splash::constrain::normalize_tools(case["request"].get("tools"), None, None, Default::default()).unwrap().unwrap();
    // The call openings tokenize as Python's tokenizer did.
    let openings = splash::constrain::call_openings(&tools.policy, true, &QWEN, &encode);
    let want: Vec<(Vec<u32>, String)> =
        trace["openings"].as_array().unwrap().iter().map(|o| (tokens(&o[0]), o[1].as_str().unwrap().into())).collect();
    assert_eq!(openings, want);
    let constraint = factory.request_constraint(Some(&tools.policy), None, true, None, &encode).unwrap().expect("strict tools constrain");
    let first = constraint.masks(&[], constraint.words_per_mask() as u32).unwrap();
    assert_eq!(digest(&first), trace["results"][0]["rows"][0]);
    // Free tools under auto, parallel: no grammar.
    let free = splash::constrain::normalize_tools(Some(&serde_json::json!([{"function": {"name": "now"}}])), None, None, Default::default())
        .unwrap()
        .unwrap();
    assert!(factory.request_constraint(Some(&free.policy), None, true, None, &encode).unwrap().is_none());
}

#[test]
fn constrain_identical_misses_compile_once() {
    let Some(factory) = factory() else { return };
    let grammar = "%llguidance {}\nstart: \"concurrent\" /[a-z]{0,8}/\n";
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..4).map(|_| scope.spawn(|| factory.create(grammar, Some(std::time::Duration::from_secs(30)), None).is_ok())).collect();
        assert!(handles.into_iter().all(|h| h.join().unwrap()));
    });
    let stats = factory.stats();
    assert_eq!((stats.misses, stats.hits, stats.entries), (1, 3, 1));
}
