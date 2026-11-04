//! The tokenizer against Splash's Python server: encoding, decoding, the streaming detokenizer with stop sequences,
//! thinking-end tokens, startup validation and call openings.

#[path = "chat_support/mod.rs"]
mod support;

use splash::chat::format::{FLINT, QWEN};
use splash::chat::tokens::Streamer;

#[test]
fn encoding_and_decoding_match_python() {
    let Some(tokenizer) = support::tokenizer() else { return };
    let fixture = support::fixture("tokens.json");
    for case in fixture["encode"].as_array().unwrap() {
        let text = case["text"].as_str().unwrap();
        let ids = tokenizer.encode(text);
        assert_eq!(ids, support::ids(&case["tokens"]), "{text:?}");
        assert_eq!(tokenizer.decode(&ids, false), case["decoded"].as_str().unwrap(), "{text:?}");
        assert_eq!(tokenizer.decode(&ids, true), case["decoded_skip"].as_str().unwrap(), "{text:?}");
    }
}

#[test]
fn streamer_matches_python() {
    let Some(tokenizer) = support::tokenizer() else { return };
    let fixture = support::fixture("tokens.json");
    for case in fixture["decode_streams"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let stops: Vec<String> = case["stop"].as_array().unwrap().iter().map(|s| s.as_str().unwrap().to_owned()).collect();
        let mut streamer = Streamer::new(tokenizer.clone(), stops);
        let ids = support::ids(&case["tokens"]);
        let mut emitted = Vec::new();
        for frame in ids.chunks(case["frames"].as_u64().unwrap() as usize) {
            emitted.extend(streamer.put_tokens(frame).unwrap());
        }
        emitted.extend(streamer.end().unwrap());
        let expected: Vec<&str> = case["emitted"].as_array().unwrap().iter().map(|s| s.as_str().unwrap()).collect();
        assert_eq!(emitted, expected, "{name}");
        assert_eq!(streamer.stop_sequence(), case["stop_sequence"].as_str(), "{name}");
    }
}

#[test]
fn thinking_end_tokens_match_python() {
    let Some(tokenizer) = support::tokenizer() else { return };
    let fixture = support::fixture("tokens.json");
    for case in fixture["thinking_end"].as_array().unwrap() {
        let text = case["text"].as_str().unwrap();
        match tokenizer.thinking_end_tokens(Some(text), &QWEN) {
            Ok(tokens) => assert_eq!(tokens.unwrap(), support::ids(&case["tokens"]), "{text:?}"),
            Err(error) => assert_eq!(error, case["error"].as_str().unwrap_or_else(|| panic!("{text:?}: {error}"))),
        }
    }
    assert_eq!(tokenizer.thinking_end_tokens(None, &QWEN), Ok(None));
}

#[test]
fn validation_matches_python() {
    let Some(tokenizer) = support::tokenizer() else { return };
    let fixture = support::fixture("tokens.json");
    let outcome = |r: Result<(), String>| r.err().map(serde_json::Value::from).unwrap_or(serde_json::Value::Null);
    assert_eq!(outcome(tokenizer.validate(&QWEN)), fixture["validate"]["qwen"]);
    assert_eq!(outcome(tokenizer.validate(&FLINT)), fixture["validate"]["flint"]);
    assert_eq!(tokenizer.eos_token_id(), Some(248046));
}

#[test]
fn call_openings_match_python() {
    let Some(tokenizer) = support::tokenizer() else { return };
    let fixture = support::fixture("tokens.json");
    for case in fixture["call_openings"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let expected = support::ids(&case["tokens"]);
        assert_eq!(tokenizer.call_opening(&QWEN, name, false), expected, "{name}");
        let mut thinking = vec![QWEN.think_end_id];
        thinking.extend(&expected);
        assert_eq!(tokenizer.call_opening(&QWEN, name, true), thinking, "{name}");
    }
}
