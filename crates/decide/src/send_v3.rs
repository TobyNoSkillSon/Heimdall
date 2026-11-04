//! Decider v3 (night 23 Oct): the dispatch question with instant-acknowledgement lines. v3 is trained
//! (lab/decider-data/run) on the live dispatch question plus one option per ack line of the current sentence's
//! language; a line label means "send now and play that line at once", plain `send` means send with no line. The
//! questions are the dataset's own export (`questions/dispatch-v3.json`, `export_metadata` of its manifest), byte for
//! byte, so production asks exactly what the model learned. v2 models keep the plain question (`send::question`).
//!
//! Decider v5 (d1-2B, 29 Oct) learned one English option set (`questions/dispatch-v5.json`, extracted byte for byte from
//! its training rows, Projects/Heimdall-decider-v5-data/data/v5): the same text as v3's English question, asked for
//! every sentence (no Polish lines), with the names header always on and the question's own words never renamed.
use crate::send::{Choice, Fitted, Prediction, Snapshot};
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeMap, path::Path, sync::OnceLock};

/// Which dispatch question a decider model was trained on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Generation {
    /// send / wait / not_for_heimdall.
    V2,
    /// v2's options plus one ack line per option (`question_ack`).
    V3,
    /// v3's English question for every sentence (`question_v5`), the names header always, the question never renamed.
    V5,
}
impl Generation {
    /// From the model's file or folder name: `…-heimdall-v<N>…` with N ≥ 5 is v5, N 3 or 4 is v3; anything else (v1,
    /// v2, the original) v2.
    pub fn of(model: &Path) -> Self {
        match version(model) {
            Some(n) if n >= 5 => Self::V5,
            Some(n) if n >= 3 => Self::V3,
            _ => Self::V2,
        }
    }
    /// Whether the decider picks the ack line with its send (v3 and later).
    pub fn picks_acks(self) -> bool {
        matches!(self, Self::V3 | Self::V5)
    }
    /// The generation of the decider the app loads (`d1::DEFAULT_MODEL`).
    pub fn shipped() -> Self {
        Self::of(Path::new(crate::d1::DEFAULT_MODEL))
    }
}

/// The decider's version from its file name (`…-heimdall-v4-…` → 4); None for the original and unnumbered models.
pub fn version(model: &Path) -> Option<u32> {
    let name = model.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let version = name.split("heimdall-v").nth(1).map(|rest| rest.chars().take_while(char::is_ascii_digit).collect::<String>());
    version.and_then(|v| v.parse::<u32>().ok())
}

const QUESTIONS: &str = include_str!("../questions/dispatch-v3.json");
fn questions() -> &'static Value {
    static Q: OnceLock<Value> = OnceLock::new();
    Q.get_or_init(|| serde_json::from_str(QUESTIONS).expect("bundled dispatch-v3.json is valid"))
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
/// Parses a v5 answer: v3's rule over v5's options, every line English (`en_<key>`).
pub fn parse_v5(value: Value) -> Result<Prediction> {
    parse_with(value, question_v5(), "en")
}

/// The plain options; every other option of `question_ack` is an ack line.
pub const PLAIN: [&str; 3] = ["send", "wait", "not_for_heimdall"];

/// The v3 dispatch question for `lang` (`en` or `pl`; anything else asks in English).
pub fn question_ack(lang: &str) -> Value {
    questions()["ack"][if lang == "pl" { "pl" } else { "en" }].clone()
}
/// The ack line options of `question_ack(lang)`: option key → spoken text.
pub fn lines(lang: &str) -> BTreeMap<String, String> {
    let q = question_ack(lang);
    q["criteria"].as_object().into_iter().flatten()
        .filter(|(k, _)| !PLAIN.contains(&k.as_str()))
        .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_owned()))
        .collect()
}
/// The voicebank ack id of a line option (`on_it` in English → `en_on_it`).
pub fn ack_id(lang: &str, key: &str) -> String {
    format!("{}_{key}", if lang == "pl" { "pl" } else { "en" })
}

/// The language whose lines the question offers: the dataset builder's rule (`scripts/build.py` `lang_of`) on the
/// current sentence, so production picks the question the model was trained with for the same words.
pub fn language(text: &str) -> &'static str {
    const PL_WORDS: &[&str] = &[
        "nie", "jest", "to", "że", "się", "jak", "czy", "co", "mam", "kurde", "no", "dobra", "tak", "zrób", "sprawdź",
        "możesz", "ten", "ta", "jeszcze", "już", "teraz", "bo", "ale", "i", "w", "na", "z", "do", "mi", "mnie", "jakby",
        "wiesz", "chyba", "tylko", "tam", "tutaj", "więc", "żeby", "być", "było", "będzie", "zobacz", "otwórz", "daj",
    ];
    const EN_WORDS: &[&str] = &["the", "is", "it", "and", "you", "that", "this", "what", "can", "to", "of", "i", "do", "a", "like", "so", "just"];
    const PL_LETTERS: &str = "ąćęłńóśźż";
    let lower = text.to_lowercase();
    // Python's [\w...']+: letters, digits, underscore and the apostrophe.
    let words: Vec<&str> = lower.split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '\'')).filter(|w| !w.is_empty()).collect();
    if words.is_empty() {
        return "en";
    }
    let pl = words.iter().filter(|w| PL_WORDS.contains(w)).count() + 2 * words.iter().filter(|w| w.chars().any(|c| PL_LETTERS.contains(c))).count();
    let en = words.iter().filter(|w| EN_WORDS.contains(w)).count();
    if pl > en { "pl" } else { "en" }
}
/// The current sentence of a snapshot: the pending turn at `through` (the last pending turn otherwise).
pub fn current_text(snapshot: &Snapshot) -> &str {
    snapshot.pending.iter().find(|t| t.seq == snapshot.through).or(snapshot.pending.last()).map_or("", |t| t.text.as_str())
}
/// The fitted v3 request for a snapshot and the language of its lines.
pub fn fit(snapshot: &Snapshot, tokens: impl FnMut(&str) -> Result<usize>) -> Result<(Fitted, &'static str)> {
    let lang = language(current_text(snapshot));
    Ok((snapshot.fit_question(&question_ack(lang), tokens)?, lang))
}

/// Parses a v3 answer. `choice` is the model's own argmax mapped to the plain options (any line is `Send`), `ack`
/// the voicebank id of the line it picked (None for plain send, wait or not-for-Heimdall). `probabilities` keep the
/// three plain keys the gates read, `send` being plain send plus every line (the whole "send now" mass); the full
/// per-line distribution stays in `output`.
pub fn parse(value: Value, lang: &str) -> Result<Prediction> {
    parse_with(value, &question_ack(lang), lang)
}
fn parse_with(value: Value, q: &Value, lang: &str) -> Result<Prediction> {
    ensure!(value["usage"]["output_tokens"].as_u64() == Some(0), "send generated tokens");
    let a = &value["answers"]["dispatch"];
    let keys: Vec<&str> = q["criteria"].as_object().context("v3 question without options")?.keys().map(String::as_str).collect();
    let raw = crate::d1::distribution(a, &keys)?;
    let chosen = a["choice"].as_str().context("missing send choice")?;
    crate::d1::ensure_max(chosen, &raw)?;
    let (choice, ack) = match chosen {
        "send" => (Choice::Send, None),
        "wait" => (Choice::Wait, None),
        "not_for_heimdall" => (Choice::NotForHeimdall, None),
        line if raw.contains_key(line) => (Choice::Send, Some(ack_id(lang, line))),
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
    fn the_generation_comes_from_the_model_file_and_the_shipped_one_is_v5() {
        assert_eq!(Generation::of(Path::new("d1/d1-omni-600M-heimdall-v2-F16.gguf")), Generation::V2);
        assert_eq!(Generation::of(Path::new("d1/d1-omni-600M-heimdall-F16.gguf")), Generation::V2, "v1");
        assert_eq!(Generation::of(Path::new("d1/d1-omni-600M-F16.gguf")), Generation::V2, "the original");
        assert_eq!(Generation::of(Path::new("/m/d1/d1-omni-600M-heimdall-v3-F16.gguf")), Generation::V3);
        assert_eq!(Generation::of(Path::new("d1-omni-600M-heimdall-v12-F16.gguf")), Generation::V5);
        assert_eq!(Generation::of(Path::new("/m/d1/d1-omni-600M-heimdall-v4-F16.gguf")), Generation::V3, "v4 keeps v3's question");
        assert_eq!(Generation::of(Path::new("/m/d1/d1-2B-heimdall-v5")), Generation::V5, "v5 is a checkpoint folder");
        assert_eq!(Generation::of(Path::new("d1-2B-heimdall-v12")), Generation::V5);
        assert_eq!(Generation::shipped(), Generation::V5, "v6 ships (4 Nov) and asks v5's questions: d1::DEFAULT_MODEL names it");
        assert!(Generation::V5.picks_acks() && Generation::V3.picks_acks() && !Generation::V2.picks_acks());
    }
    /// The v5 question file is the training rows' question byte for byte (`json.dumps(question, ensure_ascii=False)`
    /// of every dispatch row in data/v5), and its text is v3's English question: one option set, no Polish lines.
    #[test]
    fn the_v5_question_is_its_training_rows_and_v3s_english_question() {
        assert!(QUESTION_V5.starts_with("{\"criteria\": {\"checking\": \"Checking.\"") && QUESTION_V5.ends_with("\"type\": \"choice\"}\n"));
        assert_eq!(*question_v5(), question_ack("en"));
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
        // A v3 Polish answer is not a v5 answer.
        assert!(parse_v5(answer("sprawdzam", "pl", 0.5)).is_err());
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
    /// The plain question of the v3 export is today's live question, byte for byte (serde_json sorts keys as the
    /// dataset's `qjson` does), and the ack questions extend it exactly as `fmt.ack_question` does.
    #[test]
    fn the_questions_are_the_datasets_export_byte_for_byte() {
        assert_eq!(questions()["plain"].to_string(), crate::send::question().to_string());
        for lang in ["en", "pl"] {
            let q = question_ack(lang);
            let plain = crate::send::question();
            assert_eq!(q["type"], "choice");
            assert_eq!(q["instructions"].as_str().unwrap(), format!("{} Any other option means: send now and play that line at once. Pick a line only when User expects Heimdall to act or look into something and the line fits; otherwise choose plain send.", plain["instructions"].as_str().unwrap()));
            assert_eq!(q["criteria"]["send"], "The pending speech is meant for Heimdall and complete enough to respond or act now; play no line.");
            assert_eq!(q["criteria"]["wait"], plain["criteria"]["wait"]);
            assert_eq!(q["criteria"]["not_for_heimdall"], plain["criteria"]["not_for_heimdall"]);
            assert_eq!(lines(lang).len(), 27, "{lang}");
            let s = q.to_string();
            let keys: Vec<_> = q["criteria"].as_object().unwrap().keys().cloned().collect();
            let mut sorted = keys.clone();
            sorted.sort();
            assert_eq!(keys, sorted, "serialized in sorted key order");
            assert!(s.starts_with("{\"criteria\":{\"") && !s.contains("\": ") && !s.contains('\n'), "compact, as serde_json renders the request");
        }
        assert_eq!(lines("en")["on_it"], "On it.");
        assert_eq!(lines("pl")["sprawdzam"], "Sprawdzam.");
        assert_eq!(question_ack("de"), question_ack("en"));
    }
    /// Same answers as `build.py lang_of` (checked against it on these sentences).
    #[test]
    fn the_language_rule_is_the_dataset_builders() {
        for (text, want) in [
            ("Open Safari and start the timer.", "en"),
            ("Czy build już przeszedł?", "pl"),
            ("Dobra, odpal build", "pl"),
            ("I'm in a no-go zone", "en"),
            ("Do it now", "en"),
            ("snake_case_word", "en"),
            ("dobra to zrób", "pl"),
            ("no i co", "pl"),
            ("I do it", "en"),
            ("", "en"),
            ("...", "en"),
            ("Sprawdź to", "pl"),
            ("Heimdall, can you check the logs?", "en"),
            ("w tym pliku", "pl"),
            ("Zażółć", "pl"),
        ] {
            assert_eq!(language(text), want, "{text:?}");
        }
    }
    fn answer(choice: &str, lang: &str, top: f32) -> Value {
        let keys: Vec<String> = question_ack(lang)["criteria"].as_object().unwrap().keys().cloned().collect();
        let rest = (1. - top as f64) / (keys.len() - 1) as f64;
        let probabilities: serde_json::Map<String, Value> = keys.iter().map(|k| (k.clone(), json!(if k == choice { top as f64 } else { rest }))).collect();
        json!({"usage":{"input_tokens":700,"output_tokens":0},"answers":{"dispatch":{"choice":choice,"probabilities":probabilities}}})
    }
    #[test]
    fn a_line_label_is_a_send_with_that_line_and_plain_send_has_none() {
        let p = parse(answer("on_it", "en", 0.6), "en").unwrap();
        assert_eq!((p.choice, p.ack.as_deref()), (Choice::Send, Some("en_on_it")));
        assert!((p.probabilities["send"] - (0.6 + 0.4 * 27. / 29.)).abs() < 1e-4, "send = plain send + every line: {:?}", p.probabilities);
        assert_eq!(p.probabilities.len(), 3, "the gates' three keys");
        let p = parse(answer("sprawdzam", "pl", 0.5), "pl").unwrap();
        assert_eq!((p.choice, p.ack.as_deref()), (Choice::Send, Some("pl_sprawdzam")));
        let p = parse(answer("send", "en", 0.5), "en").unwrap();
        assert_eq!((p.choice, p.ack), (Choice::Send, None));
        let p = parse(answer("wait", "en", 0.5), "en").unwrap();
        assert_eq!((p.choice, p.ack), (Choice::Wait, None));
        let p = parse(answer("not_for_heimdall", "pl", 0.9), "pl").unwrap();
        assert_eq!((p.choice, p.ack), (Choice::NotForHeimdall, None));
        let sum: f32 = p.probabilities.values().sum();
        assert!((sum - 1.).abs() < 1e-4);
    }
    #[test]
    fn a_wrong_language_line_unknown_option_or_disagreeing_choice_is_an_error() {
        // A Polish line in the English question: the option set differs (an error, not a panic in the owner thread).
        assert!(parse(answer("sprawdzam", "pl", 0.5), "en").is_err());
        let mut v = answer("on_it", "en", 0.6);
        v["answers"]["dispatch"]["choice"] = json!("will_do");
        assert!(parse(v, "en").is_err(), "choice is not the argmax");
        let mut v = answer("on_it", "en", 0.6);
        v["answers"]["dispatch"]["choice"] = json!("dance");
        assert!(parse(v, "en").is_err());
        let mut v = answer("on_it", "en", 0.6);
        v["usage"]["output_tokens"] = json!(3);
        assert!(parse(v, "en").is_err());
        // A v2 answer (three options) is not a v3 answer.
        let v2 = json!({"usage":{"input_tokens":100,"output_tokens":0},"answers":{"dispatch":{"choice":"send","probabilities":{"send":0.5,"wait":0.25,"not_for_heimdall":0.25}}}});
        assert!(parse(v2, "en").is_err());
    }
    #[test]
    fn the_fitted_request_asks_the_current_sentences_language_within_the_window() {
        let mut c = crate::send::Conversation::default();
        c.append("User", "Open the build log.", false);
        c.sent(1);
        c.append("User", "Sprawdź czy build już przeszedł", true);
        let (fitted, lang) = fit(&c.snapshot(), |s| Ok(s.len() / 3)).unwrap();
        assert_eq!(lang, "pl");
        assert_eq!(fitted.request["questions"]["dispatch"], question_ack("pl"));
        assert!(fitted.request["state"].as_str().unwrap().ends_with("User: Sprawdź czy build już przeszedł"));
        // A long conversation still fits the native window with the longer question.
        let mut c = crate::send::Conversation::default();
        for i in 0..400 {
            c.append("User", &format!("line {i} about the build and the tests"), false);
        }
        c.sent(400);
        c.append("User", "Check the logs.", true);
        let snap = c.snapshot();
        let (fitted, lang) = fit(&snap, |s| Ok(s.len() / 3)).unwrap();
        assert_eq!(lang, "en");
        assert!(fitted.dropped_turns > 0);
        let question = question_ack("en").to_string().len() / 3 + 128;
        assert!(fitted.context_tokens + question <= crate::send::WINDOW);
        let (plain, _) = (snap.fit(|s| Ok(s.len() / 3)).unwrap(), ());
        assert!(fitted.dropped_turns > plain.dropped_turns, "the longer question leaves less room for history");
    }
}
