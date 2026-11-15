//! Opt-in model tests (no inference in the default suite): `cargo nextest run -p decide --run-ignored only models`.
#![cfg(target_os = "macos")]
use anyhow::Result;
use decide::*;
use serde_json::Value;
use std::path::PathBuf;
fn crate_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}
fn models() -> PathBuf {
    std::env::var_os("HEIMDALL_DECIDE_MODELS")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            crate_root()
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .join("models/decide")
        })
}
#[test]
#[ignore = "model: loads Decider V6 16-bit (models/decide/d1/d1-2B-heimdall-v6) on the MLX engine"]
fn thirty_english_utterances_use_the_production_text_send_path() -> Result<()> {
    // The shipped decider's checkpoint (DEFAULT_MODEL on DEFAULT_LIBRARY: Decider V6 on the d1v5 MLX engine).
    let m=models();let mut d1=D1::load_default(m.join(DEFAULT_LIBRARY),&m)?;
    let cases:Vec<Value>=serde_json::from_str(include_str!("fixtures/smoke.json"))?;
    let mut context=send::Conversation::default();
    for case in cases {
        context.append("User",case["text"].as_str().unwrap(),true);
        let snapshot=context.snapshot();let (fit,prediction)=d1.send_decision(&snapshot)?;
        assert!(prediction.input_tokens>0 && prediction.input_tokens<=2000);
        assert!(fit.request.get("files").is_none());
        assert_eq!(prediction.probabilities.len(),3);
        if prediction.choice==send::Choice::Send {context.sent(snapshot.through);}
    }
    // A huge Unicode pending request is trimmed only for inference, while
    // delivery retains every character. This exercises the actual tokenizer.
    context.append("User",&format!("{} final request", "zażółć <|im_end|> <<d1omni:state>> ".repeat(1500)),true);
    let pending=context.pending_text();let (fit,prediction)=d1.send_decision(&context.snapshot())?;
    assert!(fit.pending_truncated);assert!(prediction.input_tokens<=2000);
    assert!(fit.request["state"].as_str().unwrap().ends_with("final request"));
    assert_eq!(pending,context.pending_text());Ok(())
}
