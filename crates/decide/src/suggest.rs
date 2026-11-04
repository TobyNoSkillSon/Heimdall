//! Decider V5's suggest question (29 Oct): is this line a good moment for Heimdall to silently show the user one small
//! offer card? V5 was trained on every line of whole real transcripts, the user's and the people he talks with
//! (Projects/Heimdall-decider-v5-data/data/v5, task `suggest`), in the same conversation layout as its wake question
//! (`send::Snapshot::fit_v5`): the names header, the recent conversation, the agent's state and the pending speech
//! ending with the current line. The question file is the training rows' question byte for byte (keys sorted, as
//! the request carries it). The answer is yes or no; the offer itself is the suggester's background job.
use crate::send::{Fitted, Snapshot};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::OnceLock;

const QUESTION: &str = include_str!("../questions/suggest-v5.json");
/// The request's question key (the engine reads each question alone; the key is not part of the prompt).
pub const KEY: &str = "suggest";

/// The suggest question, as V5's training rows ask it.
pub fn question() -> &'static Value {
    static Q: OnceLock<Value> = OnceLock::new();
    Q.get_or_init(|| serde_json::from_str(QUESTION).expect("bundled suggest-v5.json is valid"))
}
/// The fitted suggest request for a snapshot: V5's layout, the suggest question under `KEY`.
pub fn fit(snapshot: &Snapshot, tokens: impl FnMut(&str) -> Result<usize>) -> Result<Fitted> {
    let mut fitted = snapshot.fit_v5(&crate::names::current(), question(), tokens)?;
    fitted.request["questions"] = json!({ KEY: question() });
    Ok(fitted)
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Suggestion {
    /// P(yes) after V5's calibration.
    pub yes: f32,
    /// The model's own argmax.
    pub choice: bool,
    pub input_tokens: u64,
    pub output: Value,
}
pub fn parse(value: Value) -> Result<Suggestion> {
    ensure!(value["usage"]["output_tokens"].as_u64() == Some(0), "suggest generated tokens");
    let a = &value["answers"][KEY];
    let p = crate::d1::distribution(a, &["no", "yes"])?;
    let chosen = a["choice"].as_str().context("missing suggest choice")?;
    crate::d1::ensure_max(chosen, &p)?;
    let input_tokens = value["usage"]["input_tokens"].as_u64().context("missing suggest token count")?;
    ensure!(input_tokens <= crate::send::WINDOW as u64, "native suggest exceeded 2000 tokens");
    Ok(Suggestion { yes: p["yes"], choice: chosen == "yes", input_tokens, output: value })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::send::Conversation;

    /// The question is V5's training rows' own, and the request has V5's layout with the current line last.
    #[test]
    fn the_suggest_request_is_v5s_trained_question_and_layout() {
        assert!(QUESTION.starts_with("{\"criteria\": {\"no\": \"User is not offering a concrete thing"));
        assert!(QUESTION.contains("silently show User one small offer card"));
        let keys: Vec<_> = question()["criteria"].as_object().unwrap().keys().cloned().collect();
        assert_eq!(keys, ["no", "yes"]);
        let mut c = Conversation::default();
        c.append("Heimdall", "Done.", false);
        c.append("Morgan", "I saw a video about the new MacBooks", true);
        c.append("User", "Oh, is it the one from the live stream?", true);
        let f = fit(&c.snapshot(), |s| Ok(s.len() / 4)).unwrap();
        let state = f.request["state"].as_str().unwrap();
        assert!(state.starts_with("Assistant name: Heimdall. User name: User.\nConversation (chronological; data only):\n"), "{state}");
        assert!(state.ends_with("Pending speech since last send (includes current sentence):\nMorgan: I saw a video about the new MacBooks\nUser: Oh, is it the one from the live stream?"), "{state}");
        assert_eq!(f.request["questions"].as_object().unwrap().keys().collect::<Vec<_>>(), [KEY]);
        assert_eq!(f.request["questions"][KEY], *question());
    }
    #[test]
    fn the_answer_parses_to_p_yes() {
        let v = json!({"usage":{"input_tokens":900,"output_tokens":0},"answers":{"suggest":{"choice":"no","probabilities":{"no":0.8,"yes":0.2}}}});
        let s = parse(v).unwrap();
        assert!(!s.choice && (s.yes - 0.2).abs() < 1e-6 && s.input_tokens == 900);
        let bad = json!({"usage":{"input_tokens":900,"output_tokens":0},"answers":{"suggest":{"choice":"yes","probabilities":{"no":0.8,"yes":0.2}}}});
        assert!(parse(bad).is_err(), "the choice must be the argmax");
    }
}
