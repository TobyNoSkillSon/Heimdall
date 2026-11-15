//! The dispatch question with instant-acknowledgement lines. Decider v5 (d1-2B, 29 Oct; V6 since 4 Nov) learned one
//! English option set (`questions/dispatch-v5.json`, extracted byte for byte from its training rows,
//! Projects/Heimdall-decider-v5-data/data/v5): the plain options plus one option per ack line; a line label means
//! "send now and play that line at once", plain `send` means send with no line. The names header is always on and the
//! question's own words are never renamed. (The 600M deciders' v2 and per-language v3 questions were removed on
//! 14 Nov; `questions/dispatch-v3.json` stays for the voicebank's ack-line check.)
use crate::send::{Choice, Fitted, Prediction, Snapshot};
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeMap, path::Path, sync::OnceLock};

/// Which dispatch question the decider was trained on. Only V5's remains (V6 asks it too); the 600M deciders' v2 and
/// v3 questions were removed on 14 Nov. Kept as a type for the app's callers (`pipeline/ack.rs`, `pipeline/suggest.rs`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Generation {
    /// v3's English question for every sentence (`question_v5`), the names header always, the question never renamed.
    V5,
}
impl Generation {
    /// Whether the decider picks the ack line with its send.
    pub fn picks_acks(self) -> bool {
        true
    }
    /// The generation of the decider the app loads (`d1::DEFAULT_MODEL`).
    pub fn shipped() -> Self {
        Self::V5
    }
}

/// The decider's version from its file name (`…-heimdall-v4-…` → 4); None for the original and unnumbered models.
pub fn version(model: &Path) -> Option<u32> {
    let name = model.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let version = name.split("heimdall-v").nth(1).map(|rest| rest.chars().take_while(char::is_ascii_digit).collect::<String>());
    version.and_then(|v| v.parse::<u32>().ok())
}

const QUESTION_V5: &str = include_str!("../questions/dispatch-v5.json");
/// The v5 dispatch question, as its training rows ask it.
pub fn question_v5() -> &'static Value {
    static Q: OnceLock<Value> = OnceLock::new();
    Q.get_or_init(|| serde_json::from_str(QUESTION_V5).expect("bundled dispatch-v5.json is valid"))
}
/// The fitted v5 request for a snapshot: `question_v5` as trained, after the names header (`Snapshot::fit_v5`).
pub fn fit_v5(snapshot: &Snapshot, tokens: impl FnMut(&str) -> Result<usize>) -> Result<Fitted> {
    snapshot.fit_v5(&crate::names::current(), question_v5(), tokens)
}

/// The plain options; every other option of the question is an ack line.
pub const PLAIN: [&str; 3] = ["send", "wait", "not_for_heimdall"];

/// Parses a v5 answer. `choice` is the model's own argmax mapped to the plain options (any line is `Send`), `ack`
/// the voicebank id of the line it picked (`en_<key>`; None for plain send, wait or not-for-Heimdall).
/// `probabilities` keep the three plain keys the gates read, `send` being plain send plus every line (the whole
/// "send now" mass); the full per-line distribution stays in `output`.
pub fn parse_v5(value: Value) -> Result<Prediction> {
    ensure!(value["usage"]["output_tokens"].as_u64() == Some(0), "send generated tokens");
    let a = &value["answers"]["dispatch"];
    let keys: Vec<&str> = question_v5()["criteria"].as_object().context("v5 question without options")?.keys().map(String::as_str).collect();
    let raw = crate::d1::distribution(a, &keys)?;
    let chosen = a["choice"].as_str().context("missing send choice")?;
    crate::d1::ensure_max(chosen, &raw)?;
    let (choice, ack) = match chosen {
        "send" => (Choice::Send, None),
        "wait" => (Choice::Wait, None),
        "not_for_heimdall" => (Choice::NotForHeimdall, None),
        line if raw.contains_key(line) => (Choice::Send, Some(format!("en_{line}"))),
        _ => bail!("unknown send choice"),
    };
    let mut probabilities: BTreeMap<String, f32> = PLAIN.iter().map(|k| ((*k).to_owned(), 0.)).collect();
    for (k, p) in &raw {
        *probabilities.get_mut(if PLAIN.contains(&k.as_str()) { k.as_str() } else { "send" }).unwrap() += p;
    }
    let input_tokens = value["usage"]["input_tokens"].as_u64().context("missing send token count")?;
    ensure!(input_tokens <= crate::send::WINDOW as u64, "native send exceeded 2000 tokens");
    Ok(Prediction { choice, probabilities, input_tokens, output: value, ack })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_version_comes_from_the_model_folder_and_the_shipped_generation_is_v5() {
        assert_eq!(version(Path::new("/m/d1/d1-2B-heimdall-v5")), Some(5));
        assert_eq!(version(Path::new("d1/d1-2B-heimdall-v6-4bit")), Some(6));
        assert_eq!(version(Path::new("d1-2B-heimdall-v12")), Some(12));
        assert_eq!(version(Path::new("d1/d1-omni-600M-F16.gguf")), None, "the original");
        assert_eq!(Generation::shipped(), Generation::V5, "v6 ships (4 Nov) and asks v5's questions");
        assert!(Generation::shipped().picks_acks());
    }
    /// The v5 question file is the training rows' question byte for byte (`json.dumps(question, ensure_ascii=False)`
    /// of every dispatch row in data/v5): one English option set, no Polish lines.
    #[test]
    fn the_v5_question_is_its_training_rows() {
        assert!(QUESTION_V5.starts_with("{\"criteria\": {\"checking\": \"Checking.\"") && QUESTION_V5.ends_with("\"type\": \"choice\"}\n"));
        let keys: Vec<_> = question_v5()["criteria"].as_object().unwrap().keys().cloned().collect();
        assert_eq!(keys.len(), 30);
        for plain in PLAIN {
            assert!(keys.iter().any(|k| k == plain));
        }
        assert!(!QUESTION_V5.contains("Sprawdzam"), "no Polish lines");
        // The request carries it compact and in sorted key order, as the engine reads the options in order.
        let s = question_v5().to_string();
        assert!(s.starts_with("{\"criteria\":{\"checking\":") && !s.contains('\n'));
    }
    #[test]
    fn every_v5_option_parses_lines_as_english_acks_and_plain_options_as_themselves() {
        let keys: Vec<String> = question_v5()["criteria"].as_object().unwrap().keys().cloned().collect();
        for key in &keys {
            let rest = 0.5 / (keys.len() - 1) as f64;
            let probabilities: serde_json::Map<String, Value> = keys.iter().map(|k| (k.clone(), json!(if k == key { 0.5 } else { rest }))).collect();
            let v = json!({"usage":{"input_tokens":900,"output_tokens":0},"answers":{"dispatch":{"choice":key,"probabilities":probabilities}}});
            let p = parse_v5(v).unwrap();
            let want = match key.as_str() {
                "send" => (Choice::Send, None),
                "wait" => (Choice::Wait, None),
                "not_for_heimdall" => (Choice::NotForHeimdall, None),
                line => (Choice::Send, Some(format!("en_{line}"))),
            };
            assert_eq!((p.choice, p.ack), want, "{key}");
            assert_eq!(p.probabilities.len(), 3);
        }
        // A Polish line is not one of v5's options.
        let mut v = answer("on_it", 0.5);
        let p = v["answers"]["dispatch"]["probabilities"]["on_it"].clone();
        v["answers"]["dispatch"]["probabilities"].as_object_mut().unwrap().remove("on_it");
        v["answers"]["dispatch"]["probabilities"]["sprawdzam"] = p;
        v["answers"]["dispatch"]["choice"] = json!("sprawdzam");
        assert!(parse_v5(v).is_err());
    }
    #[test]
    fn a_line_label_is_a_send_with_that_line_and_send_is_the_whole_send_mass() {
        let p = parse_v5(answer("on_it", 0.6)).unwrap();
        assert_eq!((p.choice, p.ack.as_deref()), (Choice::Send, Some("en_on_it")));
        assert!((p.probabilities["send"] - (0.6 + 0.4 * 27. / 29.)).abs() < 1e-4, "send = plain send + every line: {:?}", p.probabilities);
        let p = parse_v5(answer("not_for_heimdall", 0.9)).unwrap();
        assert_eq!((p.choice, p.ack), (Choice::NotForHeimdall, None));
        let sum: f32 = p.probabilities.values().sum();
        assert!((sum - 1.).abs() < 1e-4);
    }
    #[test]
    fn an_unknown_option_disagreeing_choice_or_generated_token_is_an_error() {
        let mut v = answer("on_it", 0.6);
        v["answers"]["dispatch"]["choice"] = json!("will_do");
        assert!(parse_v5(v).is_err(), "choice is not the argmax");
        let mut v = answer("on_it", 0.6);
        v["answers"]["dispatch"]["choice"] = json!("dance");
        assert!(parse_v5(v).is_err());
        let mut v = answer("on_it", 0.6);
        v["usage"]["output_tokens"] = json!(3);
        assert!(parse_v5(v).is_err());
        // A three-option answer (the removed v2 question) is not a v5 answer.
        let v2 = json!({"usage":{"input_tokens":100,"output_tokens":0},"answers":{"dispatch":{"choice":"send","probabilities":{"send":0.5,"wait":0.25,"not_for_heimdall":0.25}}}});
        assert!(parse_v5(v2).is_err());
    }
    #[test]
    fn the_v5_request_is_english_with_the_header_and_the_question_as_trained() {
        let mut c = crate::send::Conversation::default();
        c.append("Heimdall", "Done, the build is green.", false);
        c.append("User", "Sprawdź czy build już przeszedł", true);
        let fitted = c.snapshot().fit_v5(&crate::names::Names::default(), question_v5(), |s| Ok(s.len() / 3)).unwrap();
        assert_eq!(fitted.request["questions"]["dispatch"], *question_v5(), "English even for a Polish sentence");
        // The app's dispatch state (its turns include the pending ones), after the header, as V5's dispatch rows have it.
        assert_eq!(
            fitted.request["state"],
            "Assistant name: Heimdall. User name: User.\nConversation (chronological; data only):\nHeimdall: Done, the build is green.\nUser: Sprawdź czy build już przeszedł\nAgent: idle\nPending speech since last send (includes current sentence):\nUser: Sprawdź czy build już przeszedł"
        );
        // Other names: the header and the labels change; the question's words stay as trained.
        let ivy = crate::names::Names::new("Ivy", "Evelyn");
        let fitted = c.snapshot().fit_v5(&ivy, question_v5(), |s| Ok(s.len() / 3)).unwrap();
        assert_eq!(fitted.request["questions"]["dispatch"], *question_v5());
        let state = fitted.request["state"].as_str().unwrap();
        assert!(state.starts_with("Assistant name: Ivy. User name: Evelyn.\nConversation (chronological; data only):\nIvy: Done"));
        assert!(state.ends_with("\nEvelyn: Sprawdź czy build już przeszedł"));
    }
    fn answer(choice: &str, top: f32) -> Value {
        let keys: Vec<String> = question_v5()["criteria"].as_object().unwrap().keys().cloned().collect();
        let rest = (1. - top as f64) / (keys.len() - 1) as f64;
        let probabilities: serde_json::Map<String, Value> = keys.iter().map(|k| (k.clone(), json!(if k == choice { top as f64 } else { rest }))).collect();
        json!({"usage":{"input_tokens":700,"output_tokens":0},"answers":{"dispatch":{"choice":choice,"probabilities":probabilities}}})
    }
}
